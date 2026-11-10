#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Zippity supports Linux and macOS only");

mod cache;
mod engine;
mod error;
mod finder;
mod input;
mod listing;
mod process;
mod reader;
mod source;
mod write;

use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::Deserialize;

use error::{Error, Result};

static CANCELLED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn cancelled() -> bool {
    CANCELLED
        .get()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Writers,
    Stage {
        sources: Vec<PathBuf>,
        #[serde(default)]
        prefix: String,
    },
    Plan {
        archive: PathBuf,
        revision: String,
        password: Option<String>,
        #[serde(default)]
        changes: Vec<write::Change>,
    },
    Apply {
        archive: PathBuf,
        revision: String,
        password: Option<String>,
        #[serde(default)]
        changes: Vec<write::Change>,
    },
    Create {
        sources: Vec<PathBuf>,
        destination: PathBuf,
        format: source::Format,
        options: write::Options,
        password: Option<String>,
    },
    Convert {
        archive: PathBuf,
        revision: String,
        password: Option<String>,
        destination: PathBuf,
        format: source::Format,
        options: write::Options,
        new_password: Option<String>,
    },
    ReadService {
        socket: PathBuf,
        member: String,
        revision: String,
        offset: u64,
        length: usize,
        password: Option<String>,
    },
    ReadInput {
        change: write::Change,
        offset: u64,
        length: usize,
    },
    CopyInput {
        archive: PathBuf,
        revision: String,
        change: write::Change,
        destination: PathBuf,
        #[serde(default)]
        overwrite: bool,
        #[serde(default)]
        preserve_metadata: bool,
    },
    Cache,
    CleanCache {
        directory: PathBuf,
    },
    /// Whether `path` is an archive zippity opens, judged by content without running 7zz.
    Probe {
        path: PathBuf,
    },
    List {
        archive: PathBuf,
        password: Option<String>,
        /// Reply in the line format of `reply_lines`.
        #[serde(default)]
        lines: bool,
    },
    Check {
        archive: PathBuf,
        revision: String,
        password: Option<String>,
    },
    Read {
        archive: PathBuf,
        member: String,
        revision: String,
        password: Option<String>,
        offset: u64,
        length: usize,
    },
    Copy {
        archive: PathBuf,
        member: String,
        revision: String,
        password: Option<String>,
        destination: PathBuf,
        #[serde(default)]
        overwrite: bool,
        #[serde(default)]
        preserve_metadata: bool,
        /// A private member cache that keeps the archive's listing between requests.
        cache: Option<PathBuf>,
    },
    CopyMany {
        archive: PathBuf,
        revision: String,
        password: Option<String>,
        #[serde(default)]
        files: Vec<engine::Target>,
        #[serde(default)]
        directories: Vec<engine::Target>,
        #[serde(default)]
        overwrite: bool,
        #[serde(default)]
        preserve_metadata: bool,
        #[serde(default)]
        keep_damaged: bool,
        /// The folder that link targets must stay inside; without it no link is made.
        root: Option<PathBuf>,
        cache: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    if let Some(argument) = std::env::args_os().nth(1) {
        if argument == "--serve" {
            return report(reader::serve());
        }
        if argument == "--clean-cache" {
            let path = std::env::args_os().nth(2).map(PathBuf::from);
            return report(match path {
                Some(path) => cache::clean(&path),
                None => Err(Error::new("request", "Cache directory is required")),
            });
        }
        if argument == "--version" {
            println!("zippity {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        if argument == "--help" || argument == "-h" {
            println!(
                "zippity: archive browsing and transactional management for Yazi\n\nSend one JSON request on stdin. Operations include list, check, read, copy, stage, plan, apply, create, and cache management.\n--serve starts a private session reader.\nPasswords belong in stdin, never command-line arguments.\nRequires 7zz; RAR writing also requires a separately installed rar."
            );
            return ExitCode::SUCCESS;
        }
        eprintln!("Use --help, or send a JSON request on stdin.");
        return ExitCode::FAILURE;
    }
    report(execute())
}

fn report(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = std::io::stderr().lock();
            let _ = serde_json::to_writer(&mut stderr, &error);
            let _ = writeln!(stderr);
            ExitCode::FAILURE
        }
    }
}

/// Writes `value` as JSON through a 64 KiB buffer: stdout flushes a line at a time, and an
/// index has no newlines, so writing through it directly costs a system call per kilobyte.
fn reply(value: &impl serde::Serialize, failure: &'static str) -> Result<()> {
    let mut stdout = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
    serde_json::to_writer(&mut stdout, value).map_err(|_| Error::new("io", failure))?;
    stdout.flush()?;
    Ok(())
}

/// Writes `index` for the plugin: a JSON line with everything but the members, then a
/// line per member with its path, flags, size, modification time, mode, and link target,
/// separated by tabs. Flags: 1 folder, 2 unknown size, 4 encrypted, 8 Finder metadata.
/// Listings hide names and link targets with control characters, so neither holds a tab or
/// a newline. Lua's string functions read this on Yazi's main thread, where its JSON
/// decoder cannot run, so the members never cross into that thread one value at a time.
fn reply_lines(index: &engine::Index) -> Result<()> {
    let mut out = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
    let head = serde_json::json!({
        "source": &index.source,
        "hidden": index.hidden,
        "preview_limit": index.preview_limit,
    });
    serde_json::to_writer(&mut out, &head)
        .map_err(|_| Error::new("io", "Cannot write archive index"))?;
    out.write_all(b"\n")?;
    for entry in &index.entries {
        let flags = u8::from(entry.directory)
            | u8::from(entry.unknown_size) << 1
            | u8::from(entry.encrypted) << 2
            | u8::from(entry.finder) << 3;
        write!(out, "{}\t{flags}\t{}\t", entry.path, entry.size)?;
        if let Some(modified) = entry.modified {
            write!(out, "{modified}")?;
        }
        out.write_all(b"\t")?;
        if let Some(mode) = entry.mode {
            write!(out, "{mode}")?;
        }
        writeln!(out, "\t{}", entry.link.as_deref().unwrap_or_default())?;
    }
    out.flush()?;
    Ok(())
}

fn install_signals() -> Result<()> {
    let flag = CANCELLED.get_or_init(|| Arc::new(AtomicBool::new(false)));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(flag))?;
    }
    Ok(())
}

fn execute() -> Result<()> {
    install_signals()?;
    let mut input = Vec::new();
    // Room for a copy of every member of the largest index, each with its destination.
    const MAX_REQUEST_BYTES: u64 = 128 * 1024 * 1024;
    std::io::stdin()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() > MAX_REQUEST_BYTES as usize {
        return Err(Error::new("limit", "Request exceeds 128 MiB"));
    }
    let mut value: serde_json::Value = serde_json::from_slice(&input)
        .map_err(|_| Error::new("request", "Invalid JSON request"))?;
    if let Some(protected) = value.as_object_mut().and_then(|o| o.remove("protected")) {
        let paths: Vec<PathBuf> = serde_json::from_value(protected)
            .map_err(|_| Error::new("request", "Invalid protected archive list"))?;
        if paths.len() > 8 {
            return Err(Error::new("limit", "Nested archive depth exceeds eight"));
        }
        let targets = ["files", "directories"]
            .iter()
            .filter_map(|key| value.get(key).and_then(|v| v.as_array()))
            .flatten()
            .filter_map(|target| target.get("destination"));
        let destinations: Vec<&str> = value
            .get("destination")
            .into_iter()
            .chain(targets)
            .filter_map(|v| v.as_str())
            .collect();
        for path in paths {
            let source = source::Source::open(&path)?;
            for destination in &destinations {
                source.check_destination(std::path::Path::new(destination))?;
            }
        }
    }
    let request: Request =
        serde_json::from_value(value).map_err(|_| Error::new("request", "Invalid JSON request"))?;
    match request {
        Request::Writers => {
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::json!({ "zip":true, "7z":true, "rar":write::rar_available() })
            )?;
        }
        Request::ReadService {
            socket,
            member,
            revision,
            offset,
            length,
            password,
        } => {
            let bytes = reader::read(
                &socket,
                &serde_json::json!({"member":member,"revision":revision,"offset":offset,"length":length,"password":password}),
            )?;
            std::io::stdout().lock().write_all(&bytes)?;
        }
        Request::Stage { sources, prefix } => {
            let changes = write::stage(&sources, &prefix, true)?;
            reply(&changes, "Cannot write staged inputs")?;
        }
        Request::Plan {
            archive,
            revision,
            password,
            changes,
        } => {
            let index = engine::list(&archive, password.as_deref())?;
            if index.source.revision != revision {
                return Err(Error::new(
                    "changed",
                    "Archive changed; discard the pending edits",
                ));
            }
            let entries = write::plan(&index, &changes)?;
            reply(&entries, "Cannot write edit plan")?;
        }
        Request::Apply {
            archive,
            revision,
            password,
            changes,
        } => {
            let index = write::apply(&archive, &revision, password.as_deref(), &changes)?;
            reply(&index, "Cannot write saved index")?;
        }
        Request::Create {
            sources,
            destination,
            format,
            options,
            password,
        } => {
            let path = write::create(
                &sources,
                &destination,
                format,
                &options,
                password.as_deref(),
                true,
            )?;
            serde_json::to_writer(
                std::io::stdout().lock(),
                &serde_json::json!({ "path":path }),
            )
            .map_err(|_| Error::new("io", "Cannot write archive path"))?;
        }
        Request::Convert {
            archive,
            revision,
            password,
            destination,
            format,
            options,
            new_password,
        } => {
            let path = write::convert(
                &archive,
                &revision,
                password.as_deref(),
                &destination,
                format,
                &options,
                new_password.as_deref(),
            )?;
            serde_json::to_writer(
                std::io::stdout().lock(),
                &serde_json::json!({ "path":path }),
            )
            .map_err(|_| Error::new("io", "Cannot write archive path"))?;
        }
        Request::ReadInput {
            change,
            offset,
            length,
        } => {
            std::io::stdout()
                .lock()
                .write_all(&input::read(&change, offset, length)?)?;
        }
        Request::CopyInput {
            archive,
            revision,
            change,
            destination,
            overwrite,
            preserve_metadata,
        } => {
            let size = input::copy(
                &archive,
                &revision,
                &change,
                &destination,
                overwrite,
                preserve_metadata,
            )?;
            writeln!(std::io::stdout().lock(), "{{\"done\":{size}}}")?;
        }
        Request::Cache => {
            let path = cache::create()?;
            if serde_json::to_writer(
                std::io::stdout().lock(),
                &serde_json::json!({ "path": path }),
            )
            .is_err()
            {
                let _ = cache::clean(&path);
                return Err(Error::new("io", "Cannot return cache directory"));
            }
        }
        Request::CleanCache { directory } => {
            cache::clean(&directory)?;
            writeln!(std::io::stdout().lock(), "{{\"cleaned\":true}}")?;
        }
        Request::Probe { path } => {
            let archive = match source::Source::open(&path) {
                Ok(_) => true,
                Err(err) if err.kind == "not_archive" => false,
                Err(err) => return Err(err),
            };
            writeln!(std::io::stdout().lock(), "{{\"archive\":{archive}}}")?;
        }
        Request::List {
            archive,
            password,
            lines,
        } => {
            let index = engine::list(&archive, password.as_deref())?;
            if lines {
                reply_lines(&index)?;
            } else {
                reply(&index, "Cannot write archive index")?;
            }
        }
        Request::Check {
            archive,
            revision,
            password,
        } => {
            engine::verify(&archive, &revision, password.as_deref())?;
            writeln!(std::io::stdout().lock(), "{{\"verified\":true}}")?;
        }
        Request::Read {
            archive,
            member,
            revision,
            password,
            offset,
            length,
        } => {
            let bytes = engine::read(
                &archive,
                &member,
                &revision,
                password.as_deref(),
                offset,
                length,
            )?;
            std::io::stdout().lock().write_all(&bytes)?;
        }
        Request::Copy {
            archive,
            member,
            revision,
            password,
            destination,
            overwrite,
            preserve_metadata,
            cache,
        } => {
            let report = engine::copy_many(
                &archive,
                &revision,
                password.as_deref(),
                &[engine::Target {
                    member,
                    destination,
                }],
                &[],
                engine::Flags {
                    overwrite,
                    preserve_metadata,
                    keep_damaged: false,
                    finder: true,
                    cache: cache.as_deref(),
                },
                None,
            )?;
            if !report.skipped.is_empty() {
                return Err(engine::link_unreadable());
            }
            if !report.damaged.is_empty() || !report.lost.is_empty() {
                return Err(Error::new(
                    "damaged",
                    "Archive member failed decoding or verification",
                ));
            }
            writeln!(std::io::stdout().lock(), "{{\"done\":{}}}", report.done)?;
        }
        Request::CopyMany {
            archive,
            revision,
            password,
            files,
            directories,
            overwrite,
            preserve_metadata,
            keep_damaged,
            root,
            cache,
        } => {
            let report = engine::copy_many(
                &archive,
                &revision,
                password.as_deref(),
                &files,
                &directories,
                engine::Flags {
                    overwrite,
                    preserve_metadata,
                    keep_damaged,
                    finder: true,
                    cache: cache.as_deref(),
                },
                root.as_deref(),
            )?;
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string(&report)
                    .map_err(|_| Error::new("io", "Cannot write copy report"))?
            )?;
        }
    }
    Ok(())
}
