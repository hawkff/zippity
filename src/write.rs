use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    hash::{Hash, Hasher},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    engine,
    error::{Error, Result},
    finder,
    listing::{self, Entry},
    process,
    source::Format,
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Delete {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Put {
        path: String,
        source: PathBuf,
        stamp: String,
    },
}

#[derive(Clone, Serialize)]
pub struct Planned {
    #[serde(flatten)]
    pub entry: Entry,
    pub origin: Option<String>,
    pub input: Option<PathBuf>,
    pub stamp: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    #[serde(default = "normal_compression")]
    pub level: u8,
    #[serde(default)]
    pub encrypt_headers: bool,
    #[serde(default)]
    pub zip_crypto: bool,
    pub volume_bytes: Option<u64>,
    /// Compression method, such as LZMA2, PPMd, or Deflate.
    pub method: Option<String>,
    /// Dictionary size, or memory for PPMd, such as 64m.
    pub dictionary: Option<String>,
    /// Fast bytes for LZMA and Deflate, model order for PPMd.
    pub word_size: Option<u32>,
    /// Solid blocks: on, off, or a block size such as 4g for 7z; on or off for RAR.
    pub solid: Option<String>,
}

/// A size such as 64m: digits and a k, m, or g unit, read the same way by 7zz and rar.
fn size_value(size: &str) -> Result<&str> {
    let valid = size.strip_suffix(['k', 'm', 'g']).is_some_and(|digits| {
        (1..=5).contains(&digits.len())
            && !digits.starts_with('0')
            && digits.bytes().all(|b| b.is_ascii_digit())
    });
    if valid {
        Ok(size)
    } else {
        Err(Error::new(
            "request",
            "Sizes are a number with k, m, or g, such as 64m",
        ))
    }
}

/// 7-Zip's method string for `options`, such as PPMd:mem=256m:o=16, with the property
/// names each method reads and the word sizes its encoder accepts.
fn method_value(format: Format, options: &Options) -> Result<String> {
    let requested = options
        .method
        .as_deref()
        .unwrap_or(if format == Format::Zip {
            "Deflate"
        } else {
            "LZMA2"
        });
    let known = [
        "LZMA2",
        "LZMA",
        "PPMd",
        "BZip2",
        "Deflate",
        "Deflate64",
        "Copy",
    ];
    let method = known
        .into_iter()
        .find(|name| name.eq_ignore_ascii_case(requested))
        .filter(|name| format == Format::SevenZip || *name != "LZMA2")
        .ok_or_else(|| Error::new("request", "This format has no such compression method"))?;
    let (dictionary, word) = match method {
        "LZMA2" | "LZMA" => (Some("d"), Some(("fb", 5, 273))),
        // ZIP's PPMd variant stops at model order 16.
        "PPMd" => (
            Some("mem"),
            Some(("o", 2, if format == Format::Zip { 16 } else { 32 })),
        ),
        "BZip2" => (Some("d"), None),
        "Deflate" => (None, Some(("fb", 3, 258))),
        "Deflate64" => (None, Some(("fb", 3, 257))),
        _ => (None, None),
    };
    let mut value = method.to_owned();
    if let Some(size) = &options.dictionary {
        let name = dictionary
            .ok_or_else(|| Error::new("request", "This method has no dictionary size"))?;
        value += &format!(":{name}={}", size_value(size)?);
    }
    if let Some(size) = options.word_size {
        let (name, low, high) =
            word.ok_or_else(|| Error::new("request", "This method has no word size"))?;
        if !(low..=high).contains(&size) {
            return Err(Error::new(
                "request",
                format!("{method} word size must be {low} to {high}"),
            ));
        }
        value += &format!(":{name}={size}");
    }
    Ok(value)
}

pub fn rar_available() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            fs::metadata(path.join("rar")).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        })
    })
}

fn normal_compression() -> u8 {
    5
}

fn input_stamp(path: &Path) -> Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() && !meta.is_dir() && !meta.file_type().is_symlink() {
        return Err(Error::new(
            "unsupported",
            "Archive inputs must be files, folders, or symbolic links, not special files",
        ));
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
        meta.mode(),
    )
        .hash(&mut hash);
    Ok(format!("{:016x}", hash.finish()))
}

pub fn validate_input(change: &Change) -> Result<Entry> {
    let Change::Put {
        path,
        source,
        stamp,
    } = change
    else {
        return Err(Error::new("request", "Expected an added file"));
    };
    input_entry(path, source, stamp)
}

fn input_entry(path: &str, source: &Path, stamp: &str) -> Result<Entry> {
    listing::validate_path(path)?;
    if input_stamp(source)? != stamp {
        return Err(Error::new(
            "changed",
            "An added file changed; add it again before saving",
        ));
    }
    let meta = fs::symlink_metadata(source)?;
    let link = if meta.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        let target = target
            .to_str()
            .ok_or_else(|| Error::new("unsupported", "Link targets must be valid UTF-8"))?;
        Some(target.to_owned())
    } else {
        None
    };
    Ok(Entry {
        path: path.into(),
        original_path: path.into(),
        directory: meta.is_dir(),
        implicit_directory: false,
        // A link is stored with its target, and has no data of its own.
        size: if meta.is_dir() || link.is_some() {
            0
        } else {
            meta.len()
        },
        encrypted: false,
        modified: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64()),
        mode: Some(meta.mode() & 0o777),
        link,
        finder: false,
        position: 0,
        unknown_size: false,
        crc: None,
        group: None,
    })
}

/// Lists local inputs for an archive. `skip_finder_files` leaves out Finder metadata files
/// found inside folders; archive members copied out for Save As keep theirs.
pub fn stage(sources: &[PathBuf], prefix: &str, skip_finder_files: bool) -> Result<Vec<Change>> {
    if !prefix.is_empty() {
        listing::validate_path(prefix)?;
    }
    let mut changes = Vec::new();
    fn visit(source: &Path, path: String, skip: bool, changes: &mut Vec<Change>) -> Result<()> {
        listing::validate_path(&path)?;
        let stamp = input_stamp(source)?;
        // A link is kept as a link, so only its folder is resolved.
        let source = if fs::symlink_metadata(source)?.file_type().is_symlink() {
            let name = source
                .file_name()
                .ok_or_else(|| Error::new("unsupported", "Input needs a filename"))?;
            let parent = source
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            parent.canonicalize()?.join(name)
        } else {
            source.canonicalize()?
        };
        changes.push(Change::Put {
            path: path.clone(),
            source: source.clone(),
            stamp,
        });
        if changes.len() > listing::MAX_ENTRIES {
            return Err(Error::new("limit", "Too many archive inputs"));
        }
        if fs::symlink_metadata(&source)?.is_dir() {
            for child in fs::read_dir(&source)? {
                let child = child?;
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| Error::new("unsupported", "Input names must be valid UTF-8"))?;
                // Finder's folder settings, folder icons, and AppleDouble files; a selected
                // one is still added.
                if skip
                    && (name == ".DS_Store" || name == "Icon\r" || name.starts_with("._"))
                    && child.file_type()?.is_file()
                {
                    continue;
                }
                visit(&child.path(), format!("{path}/{name}"), skip, changes)?;
            }
        }
        Ok(())
    }
    for source in sources {
        let name = source
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| Error::new("unsupported", "Input needs a filename"))?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        visit(source, path, skip_finder_files, &mut changes)?;
    }
    Ok(changes)
}

fn under(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|s| s.starts_with('/'))
}

/// `root`, which callers check exists, and every key below it. Those keys sort from
/// `root/` up to `root0`, because '0' follows '/'.
fn subtree(entries: &BTreeMap<String, Planned>, root: &str) -> Vec<String> {
    let mut keys: Vec<_> = entries
        .range(format!("{root}/")..format!("{root}0"))
        .map(|(key, _)| key.clone())
        .collect();
    keys.push(root.to_owned());
    keys
}

pub fn plan(index: &engine::Index, changes: &[Change]) -> Result<Vec<Planned>> {
    if changes.len() > 10_000 {
        return Err(Error::new("limit", "Too many pending edits"));
    }
    let mut entries: BTreeMap<String, Planned> = index
        .entries
        .iter()
        .cloned()
        .map(|entry| {
            (
                entry.path.clone(),
                Planned {
                    origin: Some(entry.path.clone()),
                    entry,
                    input: None,
                    stamp: None,
                },
            )
        })
        .collect();
    for change in changes {
        let path = match change {
            Change::Delete { path } => {
                listing::validate_path(path)?;
                if !entries.contains_key(path) {
                    return Err(Error::new("missing", "Deleted member does not exist"));
                }
                for key in subtree(&entries, path) {
                    entries.remove(&key);
                }
                continue;
            }
            Change::Rename { from, to } => {
                listing::validate_path(from)?;
                listing::validate_path(to)?;
                if from == to {
                    continue;
                }
                if under(to, from) || under(from, to) {
                    return Err(Error::new(
                        "conflict",
                        "Rename source and destination overlap",
                    ));
                }
                if !entries.contains_key(from) {
                    return Err(Error::new("missing", "Renamed member does not exist"));
                }
                let moved = subtree(&entries, from);
                let mut replacements = Vec::new();
                for old in &moved {
                    let next = format!("{to}{}", &old[from.len()..]);
                    listing::validate_path(&next)?;
                    if entries.contains_key(&next) {
                        return Err(Error::new("conflict", "Rename destination already exists"));
                    }
                    replacements.push((old.clone(), next));
                }
                for (old, next) in replacements {
                    let mut value = entries.remove(&old).expect("selected entry");
                    value.entry.path = next.clone();
                    entries.insert(next, value);
                }
                to.as_str()
            }
            Change::Put {
                path,
                source,
                stamp,
            } => {
                let entry = input_entry(path, source, stamp)?;
                if entries
                    .get(path)
                    .is_some_and(|old| old.entry.directory != entry.directory)
                {
                    return Err(Error::new(
                        "conflict",
                        "A file and directory cannot replace each other",
                    ));
                }
                entries.insert(
                    path.clone(),
                    Planned {
                        entry,
                        origin: None,
                        input: Some(source.clone()),
                        stamp: Some(stamp.clone()),
                    },
                );
                path.as_str()
            }
        };
        let mut parent = path;
        while let Some((next, _)) = parent.rsplit_once('/') {
            parent = next;
            if entries.get(parent).is_some_and(|e| !e.entry.directory) {
                return Err(Error::new(
                    "conflict",
                    "A file blocks a member's parent directory",
                ));
            }
            entries.entry(parent.into()).or_insert_with(|| Planned {
                entry: Entry {
                    path: parent.into(),
                    original_path: parent.into(),
                    directory: true,
                    implicit_directory: true,
                    size: 0,
                    encrypted: false,
                    modified: None,
                    mode: None,
                    link: None,
                    finder: false,
                    position: 0,
                    unknown_size: false,
                    crc: None,
                    group: None,
                },
                origin: None,
                input: None,
                stamp: None,
            });
        }
        if entries.len() > listing::MAX_ENTRIES {
            return Err(Error::new("limit", "Too many archive entries"));
        }
    }
    // A hidden member would keep the archive from being edited again.
    let mut planned: Vec<Entry> = entries.values().map(|p| p.entry.clone()).collect();
    if listing::contain_links(&mut planned) > 0 {
        return Err(Error::new(
            "unsupported",
            "Links that point outside the archive cannot be added",
        ));
    }
    // A `._name` file beside a member of that name is Finder metadata, which listings leave
    // out, and an edit can put a member beside one, as by renaming `a` to `b` next to `._b`.
    listing::split_mac_metadata(&mut planned);
    let shown: BTreeMap<String, bool> = planned
        .into_iter()
        .map(|entry| (entry.path, entry.finder))
        .collect();
    entries.retain(|path, planned| {
        let finder = shown.get(path);
        planned.entry.finder |= finder == Some(&true);
        finder.is_some()
    });
    Ok(entries.into_values().collect())
}

fn run(command: Command, password: Option<&str>) -> Result<()> {
    process::run(
        vec![command],
        &process::password_input(password)?,
        &mut std::io::sink(),
        16 * 1024 * 1024,
        Duration::from_secs(3600),
        false,
    )?;
    Ok(())
}

fn private_directory(parent: &Path) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix(".zippity-write-")
        .tempdir_in(parent)?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))?;
    if fs::metadata(dir.path())?.mode() & 0o077 != 0 {
        return Err(Error::new(
            "privacy",
            "This filesystem cannot provide private archive staging",
        ));
    }
    Ok(dir)
}

struct StagedDirectories(Vec<PathBuf>);

impl Drop for StagedDirectories {
    fn drop(&mut self) {
        // Restore parent access first so private staging can be removed on every exit path.
        for directory in &self.0 {
            let _ = fs::set_permissions(directory, fs::Permissions::from_mode(0o700));
        }
    }
}

fn publish_new(source: &Path, destination: &Path) -> Result<()> {
    use rustix::{fs::CWD, io::Errno};
    // Another writer took the name after the early check.
    let exists = || Error::new("exists", "Archive destination already exists");
    let flags = rustix::fs::RenameFlags::NOREPLACE;
    match rustix::fs::renameat_with(CWD, source, CWD, destination, flags) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(exists()),
        // A volume that cannot rename without replacing; see `engine::claim`.
        Err(Errno::NOTSUP) => {
            let folder = source.is_dir();
            engine::claim(destination, folder).map_err(|err| {
                if err.kind() == std::io::ErrorKind::AlreadyExists {
                    exists()
                } else {
                    err.into()
                }
            })?;
            fs::rename(source, destination)
                .inspect_err(|_| engine::release(destination, folder))?;
            Ok(())
        }
        Err(err) => Err(std::io::Error::from(err).into()),
    }
}

fn options(
    command: &mut Command,
    format: Format,
    options: &Options,
    password: Option<&str>,
) -> Result<()> {
    if ![0, 1, 3, 5, 7, 9].contains(&options.level) {
        return Err(Error::new(
            "request",
            "Compression level must be 0, 1, 3, 5, 7, or 9",
        ));
    }
    if options.zip_crypto && format != Format::Zip {
        return Err(Error::new("request", "ZipCrypto is only available for ZIP"));
    }
    if options.encrypt_headers && format == Format::Zip {
        return Err(Error::new(
            "request",
            "ZIP does not support encrypted filenames",
        ));
    }
    if (options.encrypt_headers || options.zip_crypto) && password.is_none() {
        return Err(Error::new(
            "request",
            "Encryption options require a password",
        ));
    }
    if let Some(size) = options.volume_bytes {
        if size < 1024 * 1024 {
            return Err(Error::new(
                "request",
                "Split volumes must be at least 1 MiB",
            ));
        }
        command.arg(format!("-v{size}b"));
    }
    if format == Format::Rar {
        let level = match options.level {
            0 => 0,
            1 => 1,
            3 => 2,
            5 => 3,
            7 => 4,
            _ => 5,
        };
        command.arg(format!("-m{level}"));
        if password.is_some() {
            command.arg(if options.encrypt_headers { "-hp" } else { "-p" });
        }
        if options.method.is_some() || options.word_size.is_some() {
            return Err(Error::new(
                "request",
                "RAR takes only a dictionary size and solid on or off",
            ));
        }
        if let Some(size) = &options.dictionary {
            command.arg(format!("-md{}", size_value(size)?));
        }
        match options.solid.as_deref() {
            None => {}
            Some("on") => {
                command.arg("-s");
            }
            Some("off") => {
                command.arg("-s-");
            }
            Some(_) => return Err(Error::new("request", "RAR solid is on or off")),
        }
    } else {
        command.arg(format!("-mx={}", options.level));
        if password.is_some() {
            command.arg("-p");
            if format == Format::Zip {
                command.arg(if options.zip_crypto {
                    "-mem=ZipCrypto"
                } else {
                    "-mem=AES256"
                });
            }
            if format == Format::SevenZip && options.encrypt_headers {
                command.arg("-mhe=on");
            }
        }
        if options.method.is_some() || options.dictionary.is_some() || options.word_size.is_some() {
            let switch = if format == Format::Zip { "-mm" } else { "-m0" };
            command.arg(format!("{switch}={}", method_value(format, options)?));
        }
        match (format, options.solid.as_deref()) {
            (_, None) => {}
            (Format::SevenZip, Some(solid @ ("on" | "off"))) => {
                command.arg(format!("-ms={solid}"));
            }
            (Format::SevenZip, Some(size)) => {
                command.arg(format!("-ms={}", size_value(size)?));
            }
            _ => return Err(Error::new("request", "ZIP has no solid blocks")),
        }
    }
    Ok(())
}

/// Whether a file added with a password is stored encrypted. 7z writes no stream, and so
/// nothing to encrypt, for an empty file.
fn encrypts(format: Format, size: u64) -> bool {
    format != Format::SevenZip || size > 0
}

/// What verification compares for a member: kind, size, encryption, and link target. Links
/// leave encryption out, since some formats encrypt a link's data and RAR keeps none.
fn shape(entry: &Entry, encrypted: bool) -> (bool, u64, bool, Option<String>) {
    let encrypted = encrypted && !entry.directory && entry.link.is_none();
    (entry.directory, entry.size, encrypted, entry.link.clone())
}

fn base_command(format: Format, action: &str) -> Command {
    let mut command = Command::new(if format == Format::Rar { "rar" } else { "7zz" });
    command.arg(action).env("TZ", "UTC");
    if format == Format::Rar {
        command.args(["-idq", "-y", "-cfg-"]);
    } else {
        command.args(["-bd", "-bb0", "-bso0", "-bse2", "-bsp0", "-spd", "-y"]);
    }
    command
}

fn prepare_file(change: &Change, staging: &Path) -> Result<PathBuf> {
    let Change::Put {
        path,
        source,
        stamp,
    } = change
    else {
        return Err(Error::new("request", "Expected an added file"));
    };
    let entry = input_entry(path, source, stamp)?;
    let target = staging.join(path);
    fs::create_dir_all(target.parent().expect("staged member parent"))?;
    if let Some(link) = &entry.link {
        std::os::unix::fs::symlink(link, &target)?;
    } else if entry.directory {
        fs::create_dir(&target)?;
    } else {
        fs::copy(source, &target)?;
    }
    if input_stamp(source)? != *stamp {
        return Err(Error::new("changed", "Input changed while staging"));
    }
    // Opening a link would reach its target, whose metadata is not the archive's business.
    if entry.link.is_none() {
        engine::apply_metadata(&fs::File::open(&target)?, &entry)?;
    }
    Ok(target)
}

fn prepare_inputs<'a>(
    changes: impl Iterator<Item = &'a Change>,
    inputs: &Path,
) -> Result<(StagedDirectories, Vec<String>)> {
    let changes: Vec<_> = changes.collect();
    let mut directories = StagedDirectories(Vec::new());
    let mut names = Vec::new();
    for change in &changes {
        let Change::Put { path, .. } = change else {
            return Err(Error::new("request", "Expected an added file"));
        };
        let entry = validate_input(change)?;
        let target = inputs.join(path);
        if fs::symlink_metadata(&target).is_ok() {
            return Err(Error::new(
                "conflict",
                "Input names collide on the staging filesystem",
            ));
        }
        if entry.directory {
            fs::create_dir_all(&target)?;
            directories.0.push(target);
        } else {
            prepare_file(change, inputs)?;
        }
        names.push(path.clone());
        // Finder metadata goes where Finder puts it. Only macOS has it, and a link's would
        // belong to its target.
        let attributes = match change {
            Change::Put { source, .. } if entry.link.is_none() => finder::attributes(source),
            _ => Vec::new(),
        };
        if !attributes.is_empty() {
            let companion = listing::companion(path, listing::Style::Mirrored);
            let output = inputs.join(&companion);
            // An input with that name, as from a copied archive, wins.
            if fs::symlink_metadata(&output).is_err() {
                fs::create_dir_all(output.parent().expect("companion folder"))?;
                finder::pack(&attributes, &output)?;
                names.push(companion);
            }
        }
    }
    for change in changes.into_iter().rev() {
        let entry = validate_input(change)?;
        if entry.directory {
            engine::apply_metadata(&fs::File::open(inputs.join(&entry.path))?, &entry)?;
        }
    }
    Ok((directories, names))
}

fn add_staged(
    archive: &Path,
    format: Format,
    inputs: &Path,
    names: &[String],
    settings: &Options,
    password: Option<&str>,
) -> Result<()> {
    let list = tempfile::NamedTempFile::new_in(inputs.parent().expect("staging parent"))?;
    // 7zz and rar trim each line of a list and drop one pair of quotes around it, so every
    // name goes in quotes and keeps its own leading and trailing spaces and quotes.
    let lines: String = names.iter().map(|name| format!("\"{name}\"\n")).collect();
    fs::write(list.path(), lines)?;
    let mut command = base_command(format, "a");
    options(&mut command, format, settings, password)?;
    // Staged links are stored as links.
    command.arg(if format == Format::Rar { "-ol" } else { "-snl" });
    if format != Format::Rar {
        command.arg(if format == Format::Zip {
            "-tzip"
        } else {
            "-t7z"
        });
        command.arg("-scsUTF-8");
    }
    command
        .current_dir(inputs)
        .arg(archive)
        .arg(format!("@{}", list.path().display()));
    run(command, password)
}

/// Whether `path` is a `._name` file beside a `name`. On exFAT and FAT volumes macOS keeps
/// the attributes of a file, which every file it writes has, in such a file.
fn attribute_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|name| name.to_str());
    name.and_then(|name| name.strip_prefix("._"))
        .filter(|name| !name.is_empty())
        .is_some_and(|name| fs::symlink_metadata(path.with_file_name(name)).is_ok())
}

/// A private folder for staged inputs, and the `inputs` folder made in it. It sits under
/// `beside`, unless macOS keeps each file's attributes there in a `._name` file beside the
/// file, as on exFAT and FAT volumes. An archiver that is given a folder would add those
/// files, and among the copies for Save As they could not be told from the `._name` members
/// an archive holds, and could take their place. Inputs then go to the temporary folder.
fn input_staging(beside: &Path) -> Result<(tempfile::TempDir, PathBuf)> {
    let temporary = std::env::temp_dir();
    for parent in [beside, temporary.as_path()] {
        let staging = private_directory(parent)?;
        let probe = staging.path().join("probe");
        fs::write(&probe, "probe")?;
        // An attribute of its own shows where the volume keeps them, whether or not macOS
        // adds one. A volume that takes none has no such files.
        let flags = rustix::fs::XattrFlags::empty();
        let _ = rustix::fs::setxattr(&probe, "user.zippity.probe", b"1", flags);
        let shadowed = match fs::symlink_metadata(staging.path().join("._probe")) {
            Ok(_) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(err.into()),
        };
        if !shadowed {
            fs::remove_file(&probe)?;
            let inputs = staging.path().join("inputs");
            fs::create_dir(&inputs)?;
            return Ok((staging, inputs));
        }
    }
    Err(Error::new(
        "unsupported",
        "This volume and the temporary folder both keep file attributes in separate files, so nothing can be staged",
    ))
}

fn store_implicit_directories(
    index: &engine::Index,
    archive: &Path,
    temporary: &Path,
    settings: &Options,
    password: Option<&str>,
) -> Result<()> {
    if !index.entries.iter().any(|e| e.implicit_directory) {
        return Ok(());
    }
    let (_staging, inputs) = input_staging(temporary)?;
    let mut names = Vec::new();
    for entry in index.entries.iter().filter(|e| e.implicit_directory) {
        fs::create_dir_all(inputs.join(&entry.path))?;
        names.push(entry.path.clone());
    }
    add_staged(
        archive,
        index.source.format,
        &inputs,
        &names,
        settings,
        password,
    )
}

/// Checks a new archive's format, settings, and destination before anything is staged,
/// and returns the destination's folder.
fn preflight(
    destination: &Path,
    format: Format,
    settings: &Options,
    password: Option<&str>,
) -> Result<PathBuf> {
    if !matches!(format, Format::Zip | Format::SevenZip | Format::Rar) {
        return Err(Error::new(
            "unsupported",
            "Creation supports ZIP, 7z, and optional RAR",
        ));
    }
    process::password_input(password)?;
    // options() validates as it builds; this command is discarded.
    options(&mut Command::new("7zz"), format, settings, password)?;
    if fs::symlink_metadata(destination).is_ok() {
        return Err(Error::new("exists", "Archive destination already exists"));
    }
    Ok(destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?)
}

pub fn create(
    sources: &[PathBuf],
    destination: &Path,
    format: Format,
    settings: &Options,
    password: Option<&str>,
    skip_finder_files: bool,
) -> Result<PathBuf> {
    let parent = preflight(destination, format, settings, password)?;
    for input in sources {
        // A link is stored as a link, so it is no input folder.
        if fs::symlink_metadata(input)?.file_type().is_symlink() {
            continue;
        }
        let input = input.canonicalize()?;
        if input.is_dir() && parent.starts_with(&input) {
            return Err(Error::new(
                "conflict",
                "Archive output cannot be inside an input directory",
            ));
        }
    }
    let changes = stage(sources, "", skip_finder_files)?;
    if changes.is_empty() {
        return Err(Error::new("request", "Select at least one archive input"));
    }
    if let Some(size) = settings.volume_bytes {
        let total: u128 = changes
            .iter()
            .map(|c| match c {
                // A link is stored as itself, not as its target.
                Change::Put { source, .. } => fs::symlink_metadata(source)
                    .map(|m| u128::from(m.len()) + 8192)
                    .unwrap_or(u128::MAX),
                _ => 0,
            })
            .sum();
        if total / u128::from(size.max(1)) > 9000 {
            return Err(Error::new(
                "limit",
                "The requested volume size would produce too many parts",
            ));
        }
    }
    let temporary = private_directory(&parent)?;
    let (_staging, inputs) = input_staging(temporary.path())?;
    let (_directories, names) = prepare_inputs(changes.iter(), &inputs)?;
    let output = temporary.path().join("output");
    fs::create_dir(&output)?;
    let extension = match format {
        Format::Zip => "zip",
        Format::SevenZip => "7z",
        _ => "rar",
    };
    let mut filename = format!("archive.{extension}");
    // Volumes take their directory's name, so backup.7z.parts holds backup.7z.001.
    if settings.volume_bytes.is_some()
        && let Some(name) = destination.file_name().and_then(|name| name.to_str())
    {
        let name = name.strip_suffix(".parts").unwrap_or(name);
        filename = if name.ends_with(&format!(".{extension}")) {
            name.to_owned()
        } else {
            format!("{name}.{extension}")
        };
    }
    let archive = output.join(filename);
    add_staged(&archive, format, &inputs, &names, settings, password)?;
    let first = if settings.volume_bytes.is_some() {
        let mut paths: Vec<_> = fs::read_dir(&output)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        paths.retain(|path| !attribute_file(path));
        paths.sort();
        paths
            .into_iter()
            .next()
            .ok_or_else(|| Error::new("archive", "No archive volumes were produced"))?
    } else {
        archive.clone()
    };
    let index = engine::list(&first, password)?;
    engine::verify(&first, &index.source.revision, password)?;
    let mut inputs = changes
        .iter()
        .map(validate_input)
        .collect::<Result<Vec<_>>>()?;
    // A link that leaves the selection is stored, and listed as hidden like any such link.
    let hidden = listing::contain_links(&mut inputs);
    // Finder metadata among the inputs, as Save As stages it, is left out of listings too.
    listing::split_mac_metadata(&mut inputs);
    let expected: BTreeMap<_, _> = inputs
        .iter()
        .map(|entry| {
            let encrypted = password.is_some() && encrypts(format, entry.size);
            (entry.path.clone(), shape(entry, encrypted))
        })
        .collect();
    let actual: BTreeMap<_, _> = index
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), shape(entry, entry.encrypted)))
        .collect();
    if actual != expected || index.hidden != hidden {
        return Err(Error::new(
            "archive",
            "Created archive does not match the selected inputs",
        ));
    }
    for volume in &index.source.volumes {
        fs::File::open(&volume.path)?.sync_all()?;
    }
    for change in &changes {
        if let Change::Put { source, stamp, .. } = change
            && input_stamp(source)? != *stamp
        {
            return Err(Error::new(
                "changed",
                "Input changed before archive publication",
            ));
        }
    }
    if crate::cancelled() {
        return Err(Error::new("cancelled", "Archive creation cancelled"));
    }
    if settings.volume_bytes.is_some() {
        let first_name = first.file_name().expect("volume name");
        publish_new(&output, destination)?;
        Ok(destination.join(first_name))
    } else {
        fs::File::open(&archive)?.sync_all()?;
        publish_new(&archive, destination)?;
        Ok(destination.to_owned())
    }
}

/// Writes every member of `archive` into a new archive at `destination`, as `create` does
/// for local inputs. Members are first copied out, verified and with their metadata, into
/// private staging, so the new format, compression, and password apply to all of them.
// shortcut: create stages the copies a second time; feed staging to add_staged if large
// conversions get slow.
pub fn convert(
    archive: &Path,
    revision: &str,
    password: Option<&str>,
    destination: &Path,
    format: Format,
    settings: &Options,
    new_password: Option<&str>,
) -> Result<PathBuf> {
    let parent = preflight(destination, format, settings, new_password)?;
    let index = engine::list(archive, password)?;
    if index.source.revision != revision {
        return Err(Error::new("changed", "Archive changed; reopen it"));
    }
    // The copy would lack the members the listing hides.
    if index.hidden > 0 {
        return Err(Error::new(
            "unsupported",
            "Archives with hidden members, such as links, cannot be saved as a new archive",
        ));
    }
    if index.entries.is_empty() {
        return Err(Error::new("empty", "The archive has no members to save"));
    }
    let (_staging, copies) = input_staging(&parent)?;
    // Dropped before `staging`, so read-only folders cannot keep it from being removed.
    let mut staged = StagedDirectories(Vec::new());
    let (mut files, mut directories) = (Vec::new(), Vec::new());
    for entry in &index.entries {
        let target = engine::Target {
            member: entry.path.clone(),
            destination: copies.join(&entry.path),
        };
        if entry.directory {
            fs::create_dir_all(&target.destination)?;
            staged.0.push(target.destination.clone());
            directories.push(target);
        } else {
            fs::create_dir_all(target.destination.parent().expect("staged member parent"))?;
            files.push(target);
        }
    }
    // Finder metadata is copied as it is stored, and stays out of listings again.
    for entry in index.metadata.iter().filter(|entry| !entry.directory) {
        let destination = copies.join(&entry.path);
        fs::create_dir_all(destination.parent().expect("staged member parent"))?;
        files.push(engine::Target {
            member: entry.path.clone(),
            destination,
        });
    }
    let flags = engine::Flags {
        overwrite: false,
        preserve_metadata: true,
        keep_damaged: false,
        finder: false,
        cache: None,
    };
    let report = engine::copy_many(
        archive,
        revision,
        password,
        &files,
        &directories,
        flags,
        Some(copies.as_path()),
    )?;
    if !report.damaged.is_empty() || !report.lost.is_empty() {
        return Err(Error::new(
            "damaged",
            "Archive members failed decoding or verification",
        ));
    }
    // The copies are read again below, so members stored without owner read access, as with
    // mode 000, get it here and in the new archive.
    // Permissions on a link would apply to its target.
    let copied = index
        .entries
        .iter()
        .chain(index.metadata.iter().filter(|e| !e.directory));
    for entry in copied.filter(|entry| entry.link.is_none()) {
        let needed = if entry.directory { 0o500 } else { 0o400 };
        if let Some(mode) = entry.mode.filter(|mode| mode & needed != needed) {
            let path = copies.join(&entry.path);
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777 | needed))?;
        }
    }
    let sources = fs::read_dir(&copies)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    create(&sources, destination, format, settings, new_password, false)
}

/// The stored Finder metadata entries in `index` that describe `path`, or with `below`
/// also the members inside it. Folders that only paths imply are left out, since a backend
/// can neither rename nor delete them by name.
fn companions<'a>(index: &'a engine::Index, path: &str, below: bool) -> Vec<&'a Entry> {
    index
        .metadata
        .iter()
        .filter(|entry| !entry.implicit_directory)
        .filter(|entry| {
            listing::described(&entry.path, entry.directory).is_some_and(|(member, _)| {
                if below {
                    under(&member, path)
                } else {
                    member == path
                }
            })
        })
        .collect()
}

/// rar reads `*`, `?`, and brackets in names as wildcards, which could reach other members.
/// 7zz runs with `-spd`, which keeps names literal.
fn rar_literal<'a>(format: Format, mut names: impl Iterator<Item = &'a str>) -> Result<()> {
    if format == Format::Rar && names.any(|name| name.contains(['*', '?', '[', ']'])) {
        return Err(Error::new(
            "unsupported",
            "RAR edits with wildcard-like names are unsupported",
        ));
    }
    Ok(())
}

/// Deletes the members that `working` stores as `names`.
fn delete(working: &Path, format: Format, names: &[&str], password: Option<&str>) -> Result<()> {
    rar_literal(format, names.iter().copied())?;
    for range in process::argument_groups(names.iter().map(|name| name.len())) {
        let names = &names[range];
        let mut command = base_command(format, "d");
        if format == Format::Rar {
            command.arg("--").arg(working).args(names);
        } else {
            command
                .args(names.iter().map(|name| format!("-i!{name}")))
                .arg("--")
                .arg(working);
        }
        run(command, password)?;
    }
    Ok(())
}

pub fn apply(
    archive: &Path,
    revision: &str,
    password: Option<&str>,
    changes: &[Change],
) -> Result<engine::Index> {
    let lock_source = crate::source::Source::open(archive)?;
    let lock = fs::File::open(&lock_source.archive)?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| Error::new("busy", "Another archive edit is in progress"))?;
    let index = engine::list(archive, password)?;
    if index.source.revision != revision {
        return Err(Error::new(
            "changed",
            "Archive changed; discard or rebase the pending edits",
        ));
    }
    if index.source.volumes.len() != 1
        || !matches!(
            index.source.format,
            Format::Zip | Format::SevenZip | Format::Rar
        )
    {
        return Err(Error::new(
            "unsupported",
            "Only single-volume ZIP, 7z, and RAR archives can be edited",
        ));
    }
    // A save would carry members the plan cannot see.
    if index.hidden > 0 {
        return Err(Error::new(
            "unsupported",
            "Archives with hidden members, such as links, cannot be edited",
        ));
    }
    if changes.is_empty() {
        return Ok(index);
    }
    let expected = plan(&index, changes)?;
    let encrypted_headers = match engine::list(&index.source.archive, None) {
        Ok(_) => false,
        Err(err) if err.kind == "password" => true,
        Err(err) => return Err(err),
    };
    if index.source.format == Format::Rar {
        for change in changes {
            let paths: Vec<&str> = match change {
                Change::Delete { path } => vec![path],
                Change::Rename { from, to } => vec![from, to],
                Change::Put { path, .. } => vec![path],
            };
            if paths.iter().any(|p| p.contains(['*', '?', '[', ']'])) {
                return Err(Error::new(
                    "unsupported",
                    "RAR edits with wildcard-like names are unsupported",
                ));
            }
        }
    }
    engine::verify(&index.source.archive, revision, password)?;
    let parent = index
        .source
        .archive
        .parent()
        .expect("absolute archive parent");
    let temporary = private_directory(parent)?;
    let working = temporary.path().join(match index.source.format {
        Format::Zip => "archive.zip",
        Format::SevenZip => "archive.7z",
        _ => "archive.rar",
    });
    fs::copy(&index.source.archive, &working)?;
    let encrypted = encrypted_headers || index.entries.iter().any(|e| e.encrypted);
    if encrypted && password.is_none() {
        return Err(Error::new(
            "password",
            "Saving this archive requires its password",
        ));
    }
    let settings = Options {
        level: 5,
        encrypt_headers: encrypted_headers,
        zip_crypto: false,
        volume_bytes: None,
        method: None,
        dictionary: None,
        word_size: None,
        solid: None,
    };
    let write_password = if encrypted { password } else { None };
    let mut finder_seen = !index.metadata.is_empty();
    // Consecutive additions share one backend pass, and so do consecutive deletions.
    for batch in changes.chunk_by(|a, b| {
        std::mem::discriminant(a) == std::mem::discriminant(b)
            && !matches!(a, Change::Rename { .. })
    }) {
        if matches!(batch[0], Change::Put { .. }) {
            let inputs: BTreeMap<_, _> = batch
                .iter()
                .map(|change| {
                    let Change::Put { path, .. } = change else {
                        unreachable!()
                    };
                    (path, change)
                })
                .collect();
            // A replaced member's Finder metadata goes with its old data.
            if finder_seen {
                let current = engine::list(&working, password)?;
                let stale: Vec<&str> = inputs
                    .keys()
                    .flat_map(|path| companions(&current, path, false))
                    .filter(|entry| !entry.directory)
                    .map(|entry| entry.original_path.as_str())
                    .collect();
                delete(&working, index.source.format, &stale, password)?;
            }
            let (_staging, staged) = input_staging(temporary.path())?;
            let (_directories, names) = prepare_inputs(inputs.into_values(), &staged)?;
            finder_seen |= names.iter().any(|name| name.starts_with("__MACOSX/"));
            add_staged(
                &working,
                index.source.format,
                &staged,
                &names,
                &settings,
                write_password,
            )?;
            for change in batch {
                validate_input(change)?;
            }
            continue;
        }
        let current = engine::list(&working, password)?;
        store_implicit_directories(
            &current,
            &working,
            temporary.path(),
            &settings,
            write_password,
        )?;
        let original = |path: &String| {
            engine::lookup(&current, path)
                .map(|entry| entry.original_path.as_str())
                .ok_or_else(|| Error::new("missing", "Edited archive member is missing"))
        };
        if let Change::Rename { from, to } = &batch[0] {
            let mut pairs = vec![(original(from)?.to_owned(), to.clone())];
            // Finder metadata moves with its member. Entries inside a folder that moves,
            // such as `._name` files beside the members of a renamed folder, move with it.
            let moving: Vec<&Entry> = companions(&current, from, true)
                .into_iter()
                .filter(|entry| !under(&entry.path, from))
                .collect();
            let folders: BTreeSet<&str> = moving
                .iter()
                .filter(|entry| entry.directory)
                .map(|entry| entry.path.as_str())
                .collect();
            for entry in &moving {
                let mut at = entry.path.as_str();
                let mut carried = false;
                while let Some((parent, _)) = at.rsplit_once('/') {
                    carried |= folders.contains(parent);
                    at = parent;
                }
                if carried {
                    continue;
                }
                let (member, style) =
                    listing::described(&entry.path, entry.directory).expect("described entry");
                let moved = format!("{to}{}", &member[from.len()..]);
                pairs.push((
                    entry.original_path.clone(),
                    listing::companion(&moved, style),
                ));
            }
            let names = pairs
                .iter()
                .flat_map(|(old, new)| [old.as_str(), new.as_str()]);
            rar_literal(index.source.format, names)?;
            let lengths = pairs.iter().map(|(old, new)| old.len() + new.len());
            for range in process::argument_groups(lengths) {
                let mut command = base_command(index.source.format, "rn");
                command.arg("--").arg(&working);
                for (old, new) in &pairs[range] {
                    command.arg(old).arg(new);
                }
                run(command, password)?;
            }
            continue;
        }
        let mut names = Vec::new();
        for change in batch {
            let Change::Delete { path } = change else {
                unreachable!()
            };
            names.push(original(path)?);
            // A member's Finder metadata, and that of the members inside it, goes with it.
            let metadata = companions(&current, path, true);
            names.extend(
                metadata
                    .into_iter()
                    .map(|entry| entry.original_path.as_str()),
            );
        }
        delete(&working, index.source.format, &names, password)?;
    }
    let checked = engine::list(&working, password)?;
    engine::verify(&working, &checked.source.revision, password)?;
    let actual: BTreeMap<_, _> = checked
        .entries
        .iter()
        .map(|e| (e.path.clone(), shape(e, e.encrypted)))
        .collect();
    let expected: BTreeMap<_, _> = expected
        .iter()
        .map(|e| {
            let added =
                e.input.is_some() && encrypted && encrypts(index.source.format, e.entry.size);
            (
                e.entry.path.clone(),
                shape(&e.entry, e.entry.encrypted || added),
            )
        })
        .collect();
    if actual != expected || checked.hidden > 0 {
        return Err(Error::new(
            "archive",
            "Saved archive does not match the pending changes",
        ));
    }
    if encrypted_headers && engine::list(&working, None).is_ok() {
        return Err(Error::new(
            "archive",
            "Refusing to remove archive header encryption",
        ));
    }
    index.source.unchanged()?;
    if crate::cancelled() {
        return Err(Error::new("cancelled", "Archive save cancelled"));
    }
    let mode = fs::metadata(&index.source.archive)?.mode() & 0o777;
    fs::set_permissions(&working, fs::Permissions::from_mode(mode))?;
    fs::File::open(&working)?.sync_all()?;
    fs::rename(&working, &index.source.archive)?;
    engine::list(&index.source.archive, password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_and_renames_reach_only_their_subtree() {
        let entry = |path: &str| Entry {
            path: path.into(),
            original_path: path.into(),
            directory: path == "a",
            implicit_directory: false,
            size: 0,
            encrypted: false,
            modified: None,
            mode: None,
            link: None,
            finder: false,
            position: 0,
            unknown_size: false,
            crc: None,
            group: None,
        };
        let index = engine::Index {
            source: crate::source::Source {
                archive: PathBuf::new(),
                revision: String::new(),
                volumes: Vec::new(),
                format: Format::Zip,
                multipart: false,
            },
            entries: ["a", "a-b", "a.b", "a/b", "a0"].map(entry).into(),
            metadata: Vec::new(),
            hidden: 0,
            preview_limit: 0,
        };
        let paths = |change| {
            plan(&index, &[change])
                .unwrap()
                .into_iter()
                .map(|planned| planned.entry.path)
                .collect::<Vec<_>>()
        };
        let delete = Change::Delete { path: "a".into() };
        assert_eq!(paths(delete), ["a-b", "a.b", "a0"]);
        let rename = Change::Rename {
            from: "a".into(),
            to: "c".into(),
        };
        assert_eq!(paths(rename), ["a-b", "a.b", "a0", "c", "c/b"]);
    }

    #[test]
    fn rar_names_stay_literal() {
        let names = ["__MACOSX/dir/._a*b.txt", "dir/._[x]"];
        for name in names {
            assert!(rar_literal(Format::Rar, [name].into_iter()).is_err());
            assert!(rar_literal(Format::Zip, [name].into_iter()).is_ok());
        }
        assert!(rar_literal(Format::Rar, ["__MACOSX/dir/._a.txt"].into_iter()).is_ok());
    }

    #[test]
    fn folders_leave_out_finder_metadata() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        for directory in ["sub", "._assets"] {
            fs::create_dir_all(folder.join(directory)).unwrap();
        }
        for name in [
            "kept.txt",
            ".DS_Store",
            "._kept.txt",
            "sub/._x",
            "sub/.hidden",
            "._assets/kept.txt",
        ] {
            fs::write(folder.join(name), "x").unwrap();
        }
        fs::write(root.path().join(".DS_Store"), "x").unwrap();
        let sources = [folder, root.path().join(".DS_Store")];
        let mut paths: Vec<_> = stage(&sources, "", true)
            .unwrap()
            .into_iter()
            .map(|change| match change {
                Change::Put { path, .. } => path,
                _ => unreachable!("staging only puts"),
            })
            .collect();
        paths.sort();
        let kept = [
            ".DS_Store",
            "folder",
            "folder/._assets",
            "folder/._assets/kept.txt",
            "folder/kept.txt",
            "folder/sub",
            "folder/sub/.hidden",
        ];
        assert_eq!(paths, kept);
    }
}
