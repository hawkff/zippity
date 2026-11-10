use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::error::{Error, Result};

pub struct Process(pub Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 7zz diagnostics also print archive paths and member names, so only its exact
/// wrong-password messages count, where it prints them: alone, before " : member",
/// or after "/archive : " in list output.
pub fn wrong_password(diagnostic: &str) -> bool {
    const OPEN: &str = "Cannot open encrypted archive. Wrong password?";
    const MESSAGES: [&str; 5] = [
        OPEN,
        "Wrong password",
        "Data Error in encrypted file. Wrong password?",
        "CRC Failed in encrypted file. Wrong password?",
        "Headers Error in encrypted archive. Wrong password?",
    ];
    diagnostic.lines().any(|line| {
        let line = line.strip_prefix("ERROR: ").unwrap_or(line);
        let message = line.split(" : ").next().unwrap_or(line);
        let listed = line.starts_with('/') && line.ends_with(&format!(" : {OPEN}"));
        listed || MESSAGES.contains(&message)
    })
}

/// Whether 7zz reported a member it could not decode, on a line that names it, such as
/// "ERROR: CRC Failed : member". Failures of the whole run, such as running out of
/// memory or failing to read the archive, print other messages.
fn member_failure(diagnostic: &str) -> bool {
    const RESULTS: [&str; 8] = [
        "Unsupported Method",
        "CRC Failed",
        "Data Error",
        "Unavailable data",
        "Unexpected end of data",
        "There are some data after the end of the payload data",
        "Is not archive",
        "Headers Error",
    ];
    diagnostic.lines().any(|line| {
        line.strip_prefix("ERROR: ")
            .and_then(|line| line.split_once(" : "))
            .is_some_and(|(result, _)| RESULTS.contains(&result))
    })
}

/// Splits arguments, given their byte lengths, into consecutive groups that stay well
/// under the 1 MiB command-line limit on macOS.
pub fn argument_groups(lengths: impl IntoIterator<Item = usize>) -> Vec<std::ops::Range<usize>> {
    let (mut groups, mut start, mut end, mut bytes) = (Vec::new(), 0, 0, 0);
    for length in lengths {
        // Count a "-i!" prefix and the terminating NUL with every argument.
        let length = length + 4;
        if end > start && bytes + length > 256 * 1024 {
            groups.push(start..end);
            (start, bytes) = (end, 0);
        }
        bytes += length;
        end += 1;
    }
    if end > start {
        groups.push(start..end);
    }
    groups
}

pub fn password_input(password: Option<&str>) -> Result<String> {
    let password = password.unwrap_or_default();
    if password.len() > 4096 || password.contains(['\n', '\r', '\0']) {
        return Err(Error::new(
            "unsupported",
            "Password contains unsupported characters or is too long",
        ));
    }
    Ok(format!("{password}\n{password}\n"))
}

pub fn start(command: &mut Command) -> Result<Process> {
    let program = command.get_program().to_string_lossy().into_owned();
    command.spawn().map(Process).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            Error::new(
                "dependency",
                format!("{program} is required for this operation"),
            )
        } else {
            err.into()
        }
    })
}

pub fn run<W: Write + Send>(
    commands: Vec<Command>,
    input: &str,
    output: &mut W,
    limit: u64,
    timeout: Duration,
    progress: bool,
) -> Result<u64> {
    if commands.is_empty() {
        return Err(Error::new("request", "Archive command is missing"));
    }
    let mut children: Vec<Process> = Vec::new();
    for (index, mut command) in commands.into_iter().enumerate() {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(previous) = children.last_mut() {
            command.stdin(previous.0.stdout.take().expect("piped stdout"));
        } else {
            command.stdin(Stdio::piped());
        }
        let mut child = start(&mut command)?;
        if index == 0 {
            let mut stdin = child.0.stdin.take().expect("piped stdin");
            if let Err(err) = stdin.write_all(input.as_bytes())
                && err.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(err.into());
            }
        }
        children.push(child);
    }
    let mut stdout = children
        .last_mut()
        .expect("command")
        .0
        .stdout
        .take()
        .expect("piped stdout");
    let errors: Vec<_> = children
        .iter_mut()
        .map(|c| c.0.stderr.take().expect("piped stderr"))
        .collect();
    let stopped = AtomicBool::new(false);
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let result = (|| -> Result<u64> {
                let mut buffer = vec![0; 1024 * 1024];
                let (mut total, mut pending) = (0u64, 0u64);
                loop {
                    let count = stdout.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    total = total
                        .checked_add(count as u64)
                        .ok_or_else(|| Error::new("limit", "Archive output size overflow"))?;
                    if total > limit {
                        return Err(Error::new(
                            "limit",
                            "Archive output exceeds its allowed size",
                        ));
                    }
                    output.write_all(&buffer[..count])?;
                    pending += count as u64;
                    if progress && pending >= 1024 * 1024 {
                        writeln!(std::io::stdout().lock(), "{{\"bytes\":{pending}}}")?;
                        pending = 0;
                    }
                }
                if progress && pending > 0 {
                    writeln!(std::io::stdout().lock(), "{{\"bytes\":{pending}}}")?;
                }
                Ok(total)
            })();
            if result.is_err() {
                stopped.store(true, Ordering::Relaxed);
            }
            result
        });
        let error_readers: Vec<_> = errors
            .into_iter()
            .map(|mut stderr| {
                let stopped = &stopped;
                scope.spawn(move || {
                    // The first 64 KiB tell a wrong password from damaged members. The rest
                    // is read and dropped, so a long report cannot block its writer. One byte
                    // more than that shows whether there was a rest.
                    let mut bytes = Vec::new();
                    let kept = stderr.by_ref().take(65_537).read_to_end(&mut bytes);
                    if kept.is_err() || std::io::copy(&mut stderr, &mut std::io::sink()).is_err() {
                        stopped.store(true, Ordering::Relaxed);
                    }
                    bytes
                })
            })
            .collect();
        let started = Instant::now();
        // A listing or a small member takes 7zz a few milliseconds, so the first looks come
        // a millisecond apart and each pause is one longer, up to the 20 ms of a long run.
        let mut pause = Duration::from_millis(1);
        let status = loop {
            let failure = if crate::cancelled() {
                Some(Error::new("cancelled", "Archive operation cancelled"))
            } else if stopped.load(Ordering::Relaxed) || started.elapsed() > timeout {
                Some(Error::new(
                    "limit",
                    "Archive operation exceeded its time or output limit",
                ))
            } else {
                None
            };
            if let Some(error) = failure {
                for child in &mut children {
                    let _ = child.0.kill();
                }
                break Err(error);
            }
            let mut complete = true;
            let mut success = true;
            let mut error = None;
            for child in &mut children {
                match child.0.try_wait() {
                    Ok(Some(status)) => success &= status.success(),
                    Ok(None) => complete = false,
                    Err(err) => {
                        error = Some(Error::from(err));
                        break;
                    }
                }
            }
            if let Some(error) = error {
                for child in &mut children {
                    let _ = child.0.kill();
                }
                break Err(error);
            }
            if complete {
                break Ok(success);
            }
            thread::sleep(pause);
            pause = (pause + Duration::from_millis(1)).min(Duration::from_millis(20));
        };
        let count = reader
            .join()
            .map_err(|_| Error::new("io", "Archive output reader failed"))??;
        let mut diagnostic = String::new();
        for reader in error_readers {
            let mut bytes = reader
                .join()
                .map_err(|_| Error::new("io", "Archive error reader failed"))?;
            // A report cut at the limit ends inside a line, which says nothing reliable.
            if bytes.len() > 65_536 {
                let whole = bytes[..65_536].iter().rposition(|&byte| byte == b'\n');
                bytes.truncate(whole.map_or(0, |end| end + 1));
            }
            diagnostic.push_str(&String::from_utf8_lossy(&bytes));
        }
        if !status? {
            if wrong_password(&diagnostic) {
                return Err(Error::new(
                    "password",
                    "Archive needs a password, or the password is incorrect",
                ));
            }
            if member_failure(&diagnostic) {
                return Err(Error::new(
                    "damaged",
                    "Archive members failed to decode; check the input and available volumes",
                ));
            }
            return Err(Error::new(
                "archive",
                "Archive operation failed; check the input, format, and available volumes",
            ));
        }
        Ok(count)
    })
}

#[cfg(test)]
mod tests {
    use super::{argument_groups, member_failure, run, wrong_password};
    use std::{process::Command, time::Duration};

    #[test]
    fn long_reports_still_name_their_cause() {
        // 4000 lines come to twice the 64 KiB that are kept.
        let kind = |line: &str| {
            let script = format!(
                "i=0; while [ $i -lt 4000 ]; do echo '{line}' >&2; i=$((i+1)); done; exit 2"
            );
            let mut command = Command::new("sh");
            command.args(["-c", &script]);
            let limit = Duration::from_secs(60);
            run(vec![command], "", &mut std::io::sink(), 0, limit, false)
                .unwrap_err()
                .kind
        };
        assert_eq!(kind("ERROR: Wrong password : notes.txt"), "password");
        assert_eq!(kind("ERROR: Data Error : notes.txt"), "damaged");
        assert_eq!(kind("A report of some other failure"), "archive");
    }

    #[test]
    fn argument_groups_stay_under_the_budget() {
        assert!(argument_groups([]).is_empty());
        let groups = argument_groups([10, 20]);
        assert!(groups.len() == 1 && groups[0] == (0..2));
        let long = 100 * 1024;
        assert_eq!(argument_groups([long, long, long, 1]), [0..2, 2..4]);
        assert_eq!(argument_groups([300 * 1024, 5]), [0..1, 1..2]);
    }

    #[test]
    fn member_failures_come_from_member_results() {
        for diagnostic in [
            "ERROR: CRC Failed : notes.txt",
            "ERROR: Headers Error : b.txt",
            "\nERROR: Unexpected end of data : disk.iso\nERRORS:\nUnexpected end of archive",
        ] {
            assert!(member_failure(diagnostic), "{diagnostic}");
        }
        for diagnostic in [
            "\nERROR: Can't allocate required memory!",
            "ERRORS:\nData Error",
            "ERROR: Headers Error",
            "ERROR: /tmp/CRC Failed : Can not open the file as archive",
            "ERROR: Input/output error : Data Error",
        ] {
            assert!(!member_failure(diagnostic), "{diagnostic}");
        }
    }

    #[test]
    fn wrong_passwords_come_from_messages_not_names() {
        for diagnostic in [
            "ERROR: /tmp/a.rar : Cannot open encrypted archive. Wrong password?",
            "ERROR: /tmp/a.7z\nCannot open encrypted archive. Wrong password?",
            "ERROR: Wrong password : notes.txt",
            "ERROR: CRC Failed in encrypted file. Wrong password? : notes.txt",
        ] {
            assert!(wrong_password(diagnostic), "{diagnostic}");
        }
        for diagnostic in [
            "ERROR: CRC Failed : Wrong password.txt",
            "ERROR: CRC Failed : report. Wrong password?",
            "ERROR: CRC Failed : x : Cannot open encrypted archive. Wrong password?",
            "ERROR: /tmp/Wrong password? : Can not open the file as archive",
        ] {
            assert!(!wrong_password(diagnostic), "{diagnostic}");
        }
    }
}
