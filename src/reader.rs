use crate::{
    cache, engine,
    error::{Error, Result},
    listing::Entry,
    process::{self, Process},
};
use rustix::event::{PollFd, PollFlags, Timespec};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub member: String,
    pub revision: String,
    pub offset: u64,
    pub length: usize,
    pub password: Option<String>,
}

#[derive(Deserialize)]
struct Start {
    archive: PathBuf,
    revision: String,
    password: Option<String>,
}

struct Stream {
    children: Vec<Process>,
    receive: mpsc::Receiver<Result<Vec<u8>>>,
    output: Option<thread::JoinHandle<()>>,
    errors: Vec<thread::JoinHandle<Vec<u8>>>,
    pending: VecDeque<u8>,
    offset: u64,
    member: String,
    password: Option<String>,
    started: Instant,
    stopped: Arc<AtomicBool>,
    root: PathBuf,
}

impl Drop for Stream {
    fn drop(&mut self) {
        for process in &mut self.children {
            let _ = process.0.kill();
        }
        let (_, empty) = mpsc::sync_channel(0);
        self.receive = empty;
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
        for errors in self.errors.drain(..) {
            let _ = errors.join();
        }
    }
}

impl Stream {
    fn new(
        index: &engine::Index,
        entry: &Entry,
        password: Option<&str>,
        stopped: Arc<AtomicBool>,
        root: PathBuf,
    ) -> Result<Self> {
        let mut children: Vec<Process> = Vec::new();
        for mut command in engine::commands(
            &index.source,
            engine::Operation::Extract(&[entry.original_path.as_str()]),
        ) {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            if let Some(previous) = children.last_mut() {
                command.stdin(previous.0.stdout.take().expect("stdout"));
            } else {
                command.stdin(Stdio::piped());
            }
            let mut child = process::start(&mut command)?;
            if children.is_empty() {
                let mut input = child.0.stdin.take().expect("stdin");
                if let Err(err) = input.write_all(process::password_input(password)?.as_bytes())
                    && err.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(err.into());
                }
            }
            children.push(child);
        }
        let mut stdout = children
            .last_mut()
            .expect("decoder")
            .0
            .stdout
            .take()
            .expect("stdout");
        let errors = children
            .iter_mut()
            .map(|child| {
                let stderr = child.0.stderr.take().expect("stderr");
                thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let _ = stderr.take(65_537).read_to_end(&mut bytes);
                    bytes
                })
            })
            .collect();
        let (send, receive) = mpsc::sync_channel(2);
        let size = entry.size;
        let output = thread::spawn(move || {
            let mut total = 0u64;
            let mut buffer = vec![0; 1024 * 1024];
            loop {
                let count = match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(err) => {
                        let _ = send.send(Err(err.into()));
                        break;
                    }
                };
                total += count as u64;
                if total > size {
                    let _ = send.send(Err(Error::new(
                        "limit",
                        "Decoded member exceeds its declared size",
                    )));
                    break;
                }
                if send.send(Ok(buffer[..count].to_vec())).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            children,
            receive,
            output: Some(output),
            errors,
            pending: VecDeque::new(),
            offset: 0,
            member: entry.path.clone(),
            password: password.map(str::to_owned),
            started: Instant::now(),
            stopped,
            root,
        })
    }

    fn next(&mut self) -> Result<bool> {
        loop {
            if crate::cancelled()
                || self.stopped.load(Ordering::Relaxed)
                || !self.root.exists()
                || self.started.elapsed() > Duration::from_secs(3600)
            {
                return Err(Error::new(
                    "cancelled",
                    "Native archive read cancelled or timed out",
                ));
            }
            match self.receive.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(bytes)) => {
                    self.pending.extend(bytes);
                    return Ok(true);
                }
                Ok(Err(err)) => return Err(err),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        while self.next()? {
            if !self.pending.is_empty() {
                return Err(Error::new(
                    "archive",
                    "Unexpected bytes after archive member",
                ));
            }
        }
        let mut success = true;
        for child in &mut self.children {
            loop {
                if let Some(status) = child.0.try_wait()? {
                    success &= status.success();
                    break;
                }
                if crate::cancelled()
                    || self.stopped.load(Ordering::Relaxed)
                    || !self.root.exists()
                    || self.started.elapsed() > Duration::from_secs(3600)
                {
                    return Err(Error::new("cancelled", "Archive read timed out"));
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        let mut password_error = false;
        for reader in self.errors.drain(..) {
            let bytes = reader
                .join()
                .map_err(|_| Error::new("io", "Archive diagnostic reader failed"))?;
            if bytes.len() > 65_536 {
                return Err(Error::new(
                    "limit",
                    "Archive diagnostics exceed their limit",
                ));
            }
            password_error |= process::wrong_password(&String::from_utf8_lossy(&bytes));
        }
        if !success {
            return Err(Error::new(
                if password_error {
                    "password"
                } else {
                    "archive"
                },
                "Archive member failed decoding or verification",
            ));
        }
        Ok(())
    }

    fn read(&mut self, offset: u64, length: usize, size: u64) -> Result<Vec<u8>> {
        let end = (offset + length as u64).min(size);
        let mut result = Vec::new();
        while self.offset < end {
            if self.pending.is_empty() && !self.next()? {
                self.finish()?;
                return Err(Error::new("archive", "Archive member ended early"));
            }
            let count = self.pending.len().min((end - self.offset) as usize);
            let skip = offset.saturating_sub(self.offset).min(count as u64) as usize;
            result.extend_from_slice(&self.pending.make_contiguous()[skip..count]);
            self.pending.drain(..count);
            self.offset += count as u64;
        }
        if self.offset == size {
            self.finish()?;
        }
        Ok(result)
    }
}

pub fn serve() -> Result<()> {
    crate::install_signals()?;
    let mut input = BufReader::new(std::io::stdin());
    let mut line = String::new();
    input.by_ref().take(65_537).read_line(&mut line)?;
    if line.len() > 65_536 {
        return Err(Error::new("limit", "Reader initialization is too large"));
    }
    let start: Start = serde_json::from_str(&line)
        .map_err(|_| Error::new("request", "Invalid reader initialization"))?;
    let index = engine::list(&start.archive, start.password.as_deref())?;
    if index.source.revision != start.revision {
        return Err(Error::new("changed", "Archive changed"));
    }
    let root = cache::create()?;
    let socket = root.join("reader.sock");
    let result = (|| {
        // Relative socket names avoid the short sockaddr_un pathname limit.
        std::env::set_current_dir(&root)?;
        let listener = UnixListener::bind("reader.sock")?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        writeln!(
            std::io::stdout().lock(),
            "{}",
            serde_json::json!({ "socket":socket, "root":root })
        )?;
        std::io::stdout().flush()?;
        let closed = Arc::new(AtomicBool::new(false));
        let parent_closed = Arc::clone(&closed);
        thread::spawn(move || {
            let _ = std::io::copy(&mut input, &mut std::io::sink());
            parent_closed.store(true, Ordering::Relaxed);
        });
        let mut stream: Option<Stream> = None;
        let mut last_used = Instant::now();
        while !closed.load(Ordering::Relaxed)
            && !crate::cancelled()
            && last_used.elapsed() < Duration::from_secs(600)
            && root.exists()
        {
            let (mut client, _) = match listener.accept() {
                Ok(client) => client,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    // Sleep until a client connects, waking every 100 ms for the checks above.
                    let mut listening = [PollFd::new(&listener, PollFlags::IN)];
                    let wake = Timespec {
                        tv_sec: 0,
                        tv_nsec: 100_000_000,
                    };
                    match rustix::event::poll(&mut listening, Some(&wake)) {
                        Ok(_) | Err(rustix::io::Errno::INTR) => continue,
                        Err(err) => return Err(Error::from(std::io::Error::from(err))),
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(err) => return Err(Error::from(err)),
            };
            // macOS can report EAGAIN on an accepted socket even after making it blocking,
            // so the socket stays nonblocking and every wait goes through poll().
            let received = client.set_nonblocking(true).and_then(|()| {
                receive(
                    &mut client,
                    65_537,
                    Instant::now() + Duration::from_secs(10),
                )
            });
            let bytes = match received {
                Ok(bytes) => bytes,
                Err(err) => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "zippity reader: dropped a request it could not read: {err}"
                    );
                    continue;
                }
            };
            let result = (|| -> Result<Vec<u8>> {
                if bytes.len() > 65_536 {
                    return Err(Error::new("limit", "Reader request is too large"));
                }
                let request: Request = serde_json::from_slice(&bytes)
                    .map_err(|_| Error::new("request", "Invalid reader request"))?;
                if request.length > 1024 * 1024
                    || request.offset.checked_add(request.length as u64).is_none()
                {
                    return Err(Error::new("limit", "Native read range exceeds its limit"));
                }
                if request.revision != index.source.revision {
                    return Err(Error::new(
                        "changed",
                        "Reader belongs to another archive revision",
                    ));
                }
                index.source.unchanged()?;
                let entry = engine::lookup(&index, &request.member)
                    .filter(|entry| !entry.directory)
                    .ok_or_else(|| Error::new("missing", "Archive member is missing"))?;
                if index.source.format == crate::source::Format::Unar {
                    return Err(engine::unstreamed());
                }
                if entry.unknown_size {
                    return Err(engine::size_unknown());
                }
                if entry.link.is_some() {
                    return Err(engine::link_unreadable());
                }
                if request.offset >= entry.size || request.length == 0 {
                    return Ok(Vec::new());
                }
                let password = request.password.as_deref().or(start.password.as_deref());
                if stream.as_ref().is_none_or(|s| {
                    s.member != request.member
                        || s.offset > request.offset
                        || s.password.as_deref() != password
                }) {
                    drop(stream.take());
                    stream = Some(Stream::new(
                        &index,
                        entry,
                        password,
                        Arc::clone(&closed),
                        root.clone(),
                    )?);
                }
                let bytes = stream.as_mut().expect("reader").read(
                    request.offset,
                    request.length,
                    entry.size,
                )?;
                index.source.unchanged()?;
                Ok(bytes)
            })();
            let (status, bytes) = match result {
                Ok(bytes) => (0u8, bytes),
                Err(err) => {
                    stream = None;
                    (1, serde_json::to_vec(&err).unwrap_or_default())
                }
            };
            let mut response = Vec::with_capacity(9 + bytes.len());
            response.push(status);
            response.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            response.extend_from_slice(&bytes);
            let deadline = Instant::now() + Duration::from_secs(30);
            if let Err(err) = send(&mut client, &response, deadline) {
                let _ = writeln!(
                    std::io::stderr(),
                    "zippity reader: could not send a response: {err}"
                );
            }
            last_used = Instant::now();
        }
        // One line on why the service stopped, for whoever reads its stderr; Yazi does not.
        let reason = if closed.load(Ordering::Relaxed) {
            "its parent closed stdin"
        } else if crate::cancelled() {
            "a signal"
        } else if !root.exists() {
            "its directory is gone"
        } else {
            "inactivity"
        };
        let _ = writeln!(std::io::stderr(), "zippity reader: stopping after {reason}");
        Ok(())
    })();
    let _ = cache::clean(&root);
    result
}

/// The time left before `deadline`. One client is served at a time, so a client that
/// trickles bytes must not hold the reader past it.
fn left(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| std::io::ErrorKind::TimedOut.into())
}

/// Waits until `client` is ready for `flags` or `deadline` passes.
fn wait(client: &UnixStream, flags: PollFlags, deadline: Instant) -> std::io::Result<()> {
    let left = left(deadline)?;
    let timeout = Timespec {
        tv_sec: left.as_secs() as i64,
        tv_nsec: i64::from(left.subsec_nanos()),
    };
    match rustix::event::poll(&mut [PollFd::new(client, flags)], Some(&timeout)) {
        Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Reads a nonblocking `client` to its end, or to `limit` bytes, by `deadline`.
fn receive(client: &mut UnixStream, limit: usize, deadline: Instant) -> std::io::Result<Vec<u8>> {
    let (mut bytes, mut buffer) = (Vec::new(), [0; 8192]);
    while bytes.len() < limit {
        left(deadline)?;
        match client.read(&mut buffer[..(limit - bytes.len()).min(8192)]) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                wait(client, PollFlags::IN, deadline)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(bytes)
}

/// Writes all of `data` to a nonblocking `client` by `deadline`.
fn send(client: &mut UnixStream, mut data: &[u8], deadline: Instant) -> std::io::Result<()> {
    while !data.is_empty() {
        left(deadline)?;
        match client.write(data) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(count) => data = &data[count..],
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                wait(client, PollFlags::OUT, deadline)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub fn read(socket: &Path, request: &serde_json::Value) -> Result<Vec<u8>> {
    let parent = socket
        .parent()
        .ok_or_else(|| Error::new("request", "Invalid reader socket"))?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(Error::new(
            "privacy",
            "Reader socket is not in a private owned directory",
        ));
    }
    std::env::set_current_dir(parent)?;
    let mut stream = UnixStream::connect(
        socket
            .file_name()
            .ok_or_else(|| Error::new("request", "Invalid reader socket name"))?,
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(3600)))?;
    serde_json::to_writer(&mut stream, request)
        .map_err(|_| Error::new("io", "Cannot send reader request"))?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut header = [0u8; 9];
    stream.read_exact(&mut header)?;
    let size = u64::from_le_bytes(header[1..].try_into().expect("length"));
    if size > 1024 * 1024 {
        return Err(Error::new("limit", "Reader response exceeds 1 MiB"));
    }
    let mut bytes = vec![0; size as usize];
    stream.read_exact(&mut bytes)?;
    if header[0] != 0 {
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| Error::new("io", "Invalid reader error"))?;
        let kind = match value["kind"].as_str() {
            Some("password") => "password",
            Some("changed") => "changed",
            _ => "archive",
        };
        return Err(Error::new(
            kind,
            value["message"]
                .as_str()
                .unwrap_or("Native archive read failed"),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{receive, send};
    use std::{
        io::{ErrorKind, Read, Write},
        os::unix::net::UnixStream,
        thread,
        time::{Duration, Instant},
    };

    fn pair() -> (UnixStream, UnixStream) {
        let (server, client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        (server, client)
    }

    #[test]
    fn a_trickling_client_cannot_outlast_the_deadline() {
        let (mut server, mut client) = pair();
        let trickle = thread::spawn(move || {
            for _ in 0..100 {
                if client.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        let deadline = started + Duration::from_millis(300);
        let err = receive(&mut server, 65_537, deadline).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(server);
        trickle.join().unwrap();
    }

    #[test]
    fn sends_end_at_the_deadline_and_arrive_whole_otherwise() {
        let (mut server, client) = pair();
        // Nobody drains the client, so the socket buffer fills.
        let data = vec![7u8; 8 * 1024 * 1024];
        let deadline = Instant::now() + Duration::from_millis(300);
        let err = send(&mut server, &data, deadline).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        drop(client);

        let (mut server, mut client) = pair();
        let reader = thread::spawn(move || {
            let mut received = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                match client.read(&mut buffer).unwrap() {
                    0 => return received,
                    count => received.extend_from_slice(&buffer[..count]),
                }
                thread::sleep(Duration::from_micros(200));
            }
        });
        let data: Vec<u8> = (0..1_000_000u32).map(|i| i as u8).collect();
        send(&mut server, &data, Instant::now() + Duration::from_secs(30)).unwrap();
        drop(server);
        assert_eq!(reader.join().unwrap(), data);
    }
}
