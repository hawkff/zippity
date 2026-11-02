use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tempfile::TempPath;

use crate::{
    error::{Error, Result},
    listing::{self, Entry},
    source::{Format, Source},
};

const MAX_LISTING_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Serialize)]
pub struct Index {
    pub source: Source,
    pub entries: Vec<Entry>,
    /// macOS Finder metadata, which listings leave out; see `listing::split_mac_metadata`.
    #[serde(skip)]
    pub metadata: Vec<Entry>,
    /// Members the listing hides; an archive with any cannot be edited.
    pub hidden: usize,
    pub preview_limit: u64,
}

pub fn list(archive: &Path, password: Option<&str>) -> Result<Index> {
    let source = Source::open(archive)?;
    let mut output = Vec::new();
    // An lz4 frame describes its one member in its header, without a listing command.
    if source.format != Format::Lz4 {
        run(
            &source,
            &[],
            password,
            &mut output,
            MAX_LISTING_BYTES,
            false,
        )?;
    }
    let text = std::str::from_utf8(&output)
        .map_err(|_| Error::new("unsupported", "7zz listing is not valid UTF-8"))?;
    let text = without_password_prompt(text);
    // A single-stream format names its one member after the archive, as 7-Zip does.
    let name = source
        .archive
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("archive");
    let unnamed = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
        _ => format!("{name}~"),
    };
    let mut listing = match source.format {
        Format::Lz4 => lz4_listing(unnamed),
        Format::Unar => xad_listing(text, &unnamed)?,
        _ => listing::parse(text, &unnamed)?,
    };
    // 7z and ZIP keep a link's target as its data.
    if matches!(source.format, Format::Zip | Format::SevenZip) {
        link_targets(&source, &mut listing.entries, password);
    }
    listing.hidden += listing::contain_links(&mut listing.entries);
    let metadata = listing::split_mac_metadata(&mut listing.entries);
    source.unchanged()?;
    Ok(Index {
        source,
        entries: listing.entries,
        metadata,
        hidden: listing.hidden,
        preview_limit: MAX_READ_BYTES,
    })
}

/// 7zz prints a password prompt before listings with encrypted headers, even with piped
/// input. Builds that turn off terminal echo, such as Debian's `7zip`, word it differently.
fn without_password_prompt(text: &str) -> &str {
    [
        "\nEnter password:\n",
        "\nEnter password (will not be echoed):\n",
    ]
    .into_iter()
    .find_map(|prompt| text.strip_prefix(prompt))
    .unwrap_or(text)
}

/// Reads the targets of links whose listing does not show them. Links it cannot read, as
/// without the password for their data, keep no target, so `listing::contain_links` hides
/// them.
// shortcut: every listing reads these targets again; keep them with the revision if archives
// with many links list slowly.
fn link_targets(source: &Source, entries: &mut [Entry], password: Option<&str>) {
    let unread: Vec<usize> = (0..entries.len())
        .filter(|&i| {
            entries[i].link.as_deref() == Some("") && (1..=4096).contains(&entries[i].size)
        })
        .collect();
    let members: Vec<&Entry> = unread.iter().map(|&i| &entries[i]).collect();
    let targets = small_data(source, &members, password, 64 * 1024 * 1024);
    for (i, target) in unread.into_iter().zip(targets) {
        if let Some(target) = target.and_then(|target| String::from_utf8(target).ok()) {
            entries[i].link = Some(target);
        }
    }
}

/// The data of each of `members`, in order, or None for one that cannot be read. Each
/// argument group takes one 7zz pass; a group that fails is read again member by member,
/// so a locked or damaged member keeps only its own data out. Listed sizes can be made up,
/// so members past `budget` bytes, in archive order, are not read.
fn small_data(
    source: &Source,
    members: &[&Entry],
    password: Option<&str>,
    budget: u64,
) -> Vec<Option<Vec<u8>>> {
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by_key(|&i| members[i].position);
    let mut spent = 0u64;
    order.retain(|&i| {
        spent = spent.saturating_add(members[i].size);
        spent <= budget
    });
    let mut data = vec![None; members.len()];
    let mut read = |group: &[usize]| {
        let names: Vec<&str> = group
            .iter()
            .map(|&i| members[i].original_path.as_str())
            .collect();
        let total: u64 = group.iter().map(|&i| members[i].size).sum();
        let mut output = Vec::new();
        if run(source, &names, password, &mut output, total, false).is_err()
            || output.len() as u64 != total
        {
            return false;
        }
        let mut rest = output.as_slice();
        for &i in group {
            let (bytes, tail) = rest.split_at(members[i].size as usize);
            rest = tail;
            data[i] = Some(bytes.to_vec());
        }
        true
    };
    let lengths = order.iter().map(|&i| members[i].original_path.len());
    for range in crate::process::argument_groups(lengths) {
        let group = &order[range];
        if !read(group) && group.len() > 1 {
            for &i in group {
                read(&[i]);
            }
        }
    }
    data
}

/// Every field of an entry, for listings kept in a private member cache; the plugin sees
/// fewer.
#[derive(Deserialize, Serialize)]
#[serde(remote = "Entry")]
struct EntryDef {
    path: String,
    original_path: String,
    directory: bool,
    implicit_directory: bool,
    size: u64,
    unknown_size: bool,
    encrypted: bool,
    modified: Option<f64>,
    mode: Option<u32>,
    link: Option<String>,
    finder: bool,
    position: usize,
    crc: Option<u32>,
    group: Option<u64>,
}

#[derive(Deserialize)]
struct KeptEntry(#[serde(with = "EntryDef")] Entry);

/// A listing kept in a private member cache, for the archive revision it describes.
#[derive(Deserialize)]
struct Kept {
    version: u32,
    archive: PathBuf,
    revision: String,
    hidden: usize,
    entries: Vec<KeptEntry>,
    metadata: Vec<KeptEntry>,
}

/// `Kept` as it is written, borrowing the index rather than copying it.
#[derive(Serialize)]
struct KeptOut<'a> {
    version: u32,
    archive: &'a Path,
    revision: &'a str,
    hidden: usize,
    entries: Vec<KeptRef<'a>>,
    metadata: Vec<KeptRef<'a>>,
}

#[derive(Serialize)]
struct KeptRef<'a>(#[serde(serialize_with = "kept_entry")] &'a Entry);

fn kept_entry<S: serde::Serializer>(
    entry: &&Entry,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    EntryDef::serialize(entry, serializer)
}

const KEPT_VERSION: u32 = 1;

/// `list`, reusing the listing kept in the private member cache `cache` while the archive's
/// revision stays the same. The revision covers each volume's inode, size, and change time,
/// which any rewrite updates. A listing that needs a password is never kept.
pub fn list_cached(archive: &Path, password: Option<&str>, cache: Option<&Path>) -> Result<Index> {
    let Some(cache) =
        cache.filter(|cache| password.is_none() && crate::cache::owned(cache).is_ok())
    else {
        return list(archive, password);
    };
    let path = cache.join(".listing.json");
    let source = Source::open(archive)?;
    let kept = fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Kept>(&data).ok())
        .filter(|kept| {
            kept.version == KEPT_VERSION
                && kept.archive == source.archive
                && kept.revision == source.revision
        });
    if let Some(kept) = kept {
        let unwrap = |entries: Vec<KeptEntry>| entries.into_iter().map(|entry| entry.0).collect();
        return Ok(Index {
            source,
            entries: unwrap(kept.entries),
            metadata: unwrap(kept.metadata),
            hidden: kept.hidden,
            preview_limit: MAX_READ_BYTES,
        });
    }
    let index = list(archive, None)?;
    // A listing that cannot be kept costs only time on the next request.
    let _ = keep(&index, cache, &path);
    Ok(index)
}

fn keep(index: &Index, cache: &Path, path: &Path) -> Result<()> {
    let kept = KeptOut {
        version: KEPT_VERSION,
        archive: &index.source.archive,
        revision: &index.source.revision,
        hidden: index.hidden,
        entries: index.entries.iter().map(KeptRef).collect(),
        metadata: index.metadata.iter().map(KeptRef).collect(),
    };
    let mut file = tempfile::NamedTempFile::new_in(cache)?;
    let mut writer = std::io::BufWriter::new(file.as_file_mut());
    serde_json::to_writer(&mut writer, &kept)
        .map_err(|_| Error::new("io", "Cannot keep the listing"))?;
    writer.flush()?;
    drop(writer);
    file.persist(path).map_err(|err| err.error)?;
    Ok(())
}

/// An lsar JSON listing, rewritten into the records `listing::parse` reads so that XAD
/// archives follow the same rules for links, special files, and names. Each member keeps
/// its XAD index, which is how unar selects it.
fn xad_listing(text: &str, unnamed: &str) -> Result<listing::Listing> {
    use std::fmt::Write as _;
    let unreadable = || Error::new("unsupported", "Cannot read the lsar listing");
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| unreadable())?;
    let contents = value["lsarContents"].as_array().ok_or_else(unreadable)?;
    let (mut records, mut indexes) = (String::new(), std::collections::HashMap::new());
    let mut sizeless = 0;
    for item in contents {
        let flag = |key: &str| item[key].as_u64() == Some(1) || item[key].as_bool() == Some(true);
        if flag("XADIsEncrypted") {
            return Err(Error::new(
                "unsupported",
                "unar takes passwords only as arguments, so zippity does not open encrypted XAD archives",
            ));
        }
        let (Some(name), Some(index)) = (item["XADFileName"].as_str(), item["XADIndex"].as_u64())
        else {
            return Err(unreadable());
        };
        let directory = flag("XADIsDirectory");
        let size = item["XADFileSize"].as_u64().map(|size| size.to_string());
        // unar unpacks whole files before zippity sees them, so a file needs its size.
        if size.is_none() && !directory {
            sizeless += 1;
            continue;
        }
        // A newline would end the record early; a carriage return hides the member instead.
        let _ = writeln!(records, "Path = {}", name.replace('\n', "\r"));
        let folder = if directory { "+" } else { "-" };
        let _ = writeln!(
            records,
            "Folder = {folder}\nSize = {}",
            size.unwrap_or_default()
        );
        if let Some(date) = item["XADLastModificationDate"]
            .as_str()
            .and_then(|date| chrono::DateTime::parse_from_str(date, "%Y-%m-%d %H:%M:%S %z").ok())
        {
            let _ = writeln!(
                records,
                "Modified = {}",
                date.naive_utc().format("%Y-%m-%d %H:%M:%S")
            );
        }
        if let Some(mode) = item["XADPosixPermissions"].as_u64() {
            let _ = writeln!(records, "Mode = {}", mode_string(mode));
        }
        // A resource fork shares its file's name, so only the data fork's index may stand
        // for the name; forks and links are hidden anyway.
        let (link, fork) = (flag("XADIsLink"), flag("XADIsResourceFork"));
        if link {
            records.push_str("Link = +\n");
        }
        if fork {
            records.push_str("Alternate Stream = +\n");
        }
        records.push('\n');
        if !link && !fork {
            indexes.entry(name.to_owned()).or_insert(index);
        }
    }
    let mut listing = listing::parse(&records, unnamed)?;
    listing.hidden += sizeless;
    for entry in &mut listing.entries {
        if let Some(index) = indexes.get(&entry.original_path) {
            entry.original_path = index.to_string();
        }
    }
    Ok(listing)
}

/// A POSIX mode as the ten characters 7zz prints, file type first.
fn mode_string(mode: u64) -> String {
    let kind = match mode & 0o170_000 {
        0o040_000 => 'd',
        0o120_000 => 'l',
        0o060_000 => 'b',
        0o020_000 => 'c',
        0o010_000 => 'p',
        0o140_000 => 's',
        _ => '-',
    };
    let bits = (0..9).rev().map(|bit| {
        if mode & (1 << bit) == 0 {
            '-'
        } else {
            ['x', 'w', 'r'][bit % 3]
        }
    });
    std::iter::once(kind).chain(bits).collect()
}

/// The one member of an lz4 file. Its size stays unknown: a frame header may record the
/// size of its own frame, but a file can hold several frames.
fn lz4_listing(path: String) -> listing::Listing {
    listing::Listing {
        entries: vec![Entry {
            path,
            original_path: String::new(),
            directory: false,
            implicit_directory: false,
            size: 0,
            unknown_size: true,
            encrypted: false,
            modified: None,
            mode: None,
            link: None,
            finder: false,
            position: 0,
            crc: None,
            group: None,
        }],
        hidden: 0,
    }
}

/// The entry at `path`; `listing::parse` returns entries sorted by path.
pub(crate) fn lookup<'a>(index: &'a Index, path: &str) -> Option<&'a Entry> {
    let found = index
        .entries
        .binary_search_by(|entry| entry.path.as_str().cmp(path))
        .ok()?;
    Some(&index.entries[found])
}

/// Reads by offset need a decoder that writes a stream; unar only writes files.
pub(crate) fn unstreamed() -> Error {
    Error::new(
        "unsupported",
        "This archive's decoder cannot read by offset; copy the member instead",
    )
}

/// A link holds no data of its own; copies recreate it.
pub(crate) fn link_unreadable() -> Error {
    Error::new(
        "unsupported",
        "A link has no contents; copy it to recreate the link",
    )
}

/// Reads by offset need the member's size, which some single-file formats never store.
pub(crate) fn size_unknown() -> Error {
    Error::new(
        "unsupported",
        "This member's size is unknown until it is decompressed; copy it instead",
    )
}

fn member<'a>(index: &'a Index, path: &str, revision: &str) -> Result<&'a Entry> {
    listing::validate_path(path)?;
    if index.source.revision != revision {
        return Err(Error::new(
            "changed",
            "Archive volumes changed; reopen the archive",
        ));
    }
    // Finder metadata is not listed, but Save As copies it.
    lookup(index, path)
        .or_else(|| {
            let found = index
                .metadata
                .binary_search_by(|entry| entry.path.as_str().cmp(path))
                .ok()?;
            Some(&index.metadata[found])
        })
        .filter(|entry| !entry.directory)
        .ok_or_else(|| Error::new("missing", "Archive member is missing or is a directory"))
}

pub fn read(
    archive: &Path,
    path: &str,
    revision: &str,
    password: Option<&str>,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    if length > 1024 * 1024 {
        return Err(Error::new("limit", "Read requests are limited to 1 MiB"));
    }
    let index = list(archive, password)?;
    let entry = member(&index, path, revision)?;
    if entry.link.is_some() {
        return Err(link_unreadable());
    }
    if index.source.format == Format::Unar {
        return Err(unstreamed());
    }
    if entry.unknown_size {
        return Err(size_unknown());
    }
    if entry.size > MAX_READ_BYTES {
        return Err(Error::new(
            "limit",
            "Large members require an explicit preview or copy",
        ));
    }
    if offset >= entry.size || length == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    let count = run(
        &index.source,
        &[entry.original_path.as_str()],
        password,
        &mut bytes,
        entry.size,
        false,
    )?;
    if count != entry.size {
        return Err(Error::new(
            "archive",
            "Extracted size does not match the archive listing",
        ));
    }
    index.source.unchanged()?;
    let start = offset as usize;
    let end = (start + length).min(bytes.len());
    Ok(bytes[start..end].to_vec())
}

/// An archive member and the local path it is copied to.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub member: String,
    pub destination: PathBuf,
}

struct Pending<'a> {
    entry: &'a Entry,
    destination: &'a Path,
    before: Option<DestinationStamp>,
    // Closed between uses, so a large selection does not run out of file descriptors.
    temporary: TempPath,
    identity: (u64, u64),
    /// Whether a pass wrote to the temporary file, which the next pass then empties.
    touched: AtomicBool,
}

impl Pending<'_> {
    /// Reopens the temporary file, refusing a link or a file that replaced it.
    fn open(&self) -> std::io::Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&self.temporary)?;
        let metadata = file.metadata()?;
        if (metadata.dev(), metadata.ino()) != self.identity {
            return Err(std::io::Error::other("A temporary file was replaced"));
        }
        Ok(file)
    }

    /// Bytes the last pass wrote for this member.
    fn written(&self) -> std::io::Result<u64> {
        Ok(self.open()?.metadata()?.len())
    }

    /// Whether the temporary file holds the listed size and, unless empty, the listed CRC.
    fn verified(&self) -> std::io::Result<bool> {
        // Only a clean pass vouches for a member of unknown size.
        if self.entry.unknown_size {
            return Ok(false);
        }
        let mut file = self.open()?;
        if file.metadata()?.len() != self.entry.size {
            return Ok(false);
        }
        let Some(expected) = self.entry.crc.filter(|_| self.entry.size > 0) else {
            return Ok(self.entry.size == 0);
        };
        let (mut crc, mut buffer) = (0, vec![0; 1024 * 1024]);
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                return Ok(crc == expected);
            }
            crc = crc32(crc, &buffer[..count]);
        }
    }
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
};

/// Continues the CRC-32 (IEEE) that ZIP, 7z, and RAR listings report.
fn crc32(crc: u32, data: &[u8]) -> u32 {
    !data.iter().fold(!crc, |crc, &byte| {
        CRC_TABLE[usize::from(crc as u8 ^ byte)] ^ (crc >> 8)
    })
}

/// How copies treat existing destinations, archived metadata, and damaged members.
#[derive(Clone, Copy)]
pub struct Flags<'a> {
    pub overwrite: bool,
    pub preserve_metadata: bool,
    /// Keep the bytes 7zz produced for a damaged member as `<destination>.damaged`.
    pub keep_damaged: bool,
    /// Give copies and folders their macOS Finder metadata from the archive, on macOS.
    pub finder: bool,
    /// A private member cache that keeps the archive's listing between requests.
    pub cache: Option<&'a Path>,
}

/// What a copy request produced. A damaged member failed its own checks. A lost member
/// could not be produced or verified because of damage elsewhere in its stream.
#[derive(Default, Serialize)]
pub struct Report {
    pub done: u64,
    pub damaged: Vec<String>,
    pub lost: Vec<String>,
    pub partial: Vec<PathBuf>,
    /// Links left out because their targets would leave the destination root.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Open,
    Copied,
    Damaged,
    Lost,
}

/// Copies archive members to local files with one listing and as few 7zz passes as the
/// command line allows, then gives `directories` their archived metadata. A failed pass
/// keeps every member that matches its listed size and CRC, and `salvage` retries the rest
/// a bounded number of times per solid group. Only failures 7zz reports for members start
/// this; a failure of the whole run fails the request. Copies are published under the
/// destination checks taken when the request started.
pub fn copy_many(
    archive: &Path,
    revision: &str,
    password: Option<&str>,
    files: &[Target],
    directories: &[Target],
    flags: Flags<'_>,
    root: Option<&Path>,
) -> Result<Report> {
    let index = list_cached(archive, password, flags.cache)?;
    if index.source.revision != revision {
        return Err(Error::new("changed", "Archive changed; reopen it"));
    }
    let mut folders = Vec::new();
    for target in directories {
        listing::validate_path(&target.member)?;
        let folder = lookup(&index, &target.member)
            .filter(|entry| entry.directory)
            .ok_or_else(|| Error::new("missing", "Archive folder is missing"))?;
        folders.push((target, folder));
    }
    let (mut pending, mut links) = (Vec::new(), Vec::new());
    for target in files {
        let entry = member(&index, &target.member, revision)?;
        index.source.check_destination(&target.destination)?;
        let before = destination_stamp(&target.destination)?;
        if before.is_some() && !flags.overwrite {
            return Err(Error::new("exists", "Destination exists"));
        }
        if let Some(link) = &entry.link {
            links.push((entry, link, target, before));
            continue;
        }
        let parent = target
            .destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // Keep decrypted temporary files and new destinations private.
        let temporary = tempfile::Builder::new()
            .prefix(".zippity-")
            .tempfile_in(parent)?;
        let metadata = temporary.as_file().metadata()?;
        pending.push(Pending {
            entry,
            destination: &target.destination,
            before,
            temporary: temporary.into_temp_path(),
            identity: (metadata.dev(), metadata.ino()),
            touched: AtomicBool::new(false),
        });
    }
    // Sizes share one output stream out among the copies, so an unsized member goes alone.
    if pending.len() > 1 && pending.iter().any(|copy| copy.entry.unknown_size) {
        return Err(Error::new(
            "request",
            "A member of unknown size must be copied on its own",
        ));
    }
    // 7zz writes the selected members to stdout one after another, in listing order.
    pending.sort_by_key(|copy| copy.entry.position);
    let source = &index.source;
    // Compressed tar decodes as one stream, whatever the listing says.
    let group = |copy: &Pending| {
        if source.format.compressed_tar() {
            Some(0)
        } else {
            copy.entry.group
        }
    };
    let mut states = vec![State::Open; pending.len()];
    let mut open = Vec::new();
    let lengths = pending.iter().map(|copy| copy.entry.original_path.len());
    for range in crate::process::argument_groups(lengths) {
        let members: Vec<usize> = range.collect();
        let mut rest = pass(source, &pending, &members, password, &mut states)?;
        if let &[only] = rest.as_slice()
            && members.len() == 1
        {
            states[only] = alone(&pending[only], group(&pending[only]).is_some())?;
            continue;
        }
        if let Some(&first) = rest.first()
            && blame(&pending[first], group(&pending[first]).is_some())?
        {
            states[first] = State::Damaged;
            rest.remove(0);
        }
        open.extend(rest);
    }
    // A 7z decoder that fails inside a block writes nothing more for that block
    // (7zExtract.cpp, CFolderOutStream::FlushCorrupted), so whatever follows a short damaged
    // member there is lost without another pass.
    if source.format == Format::SevenZip {
        let mut cuts = BTreeMap::new();
        for (copy, state) in pending.iter().zip(&states) {
            if *state == State::Damaged
                && let Some(block) = copy.entry.group
                && copy.written()? < copy.entry.size
            {
                cuts.entry(block).or_insert(copy.entry.position);
            }
        }
        open.retain(|&i| {
            let cut = group(&pending[i]).and_then(|block| cuts.get(&block));
            if cut.is_some_and(|&cut| pending[i].entry.position > cut) {
                states[i] = State::Lost;
                return false;
            }
            true
        });
    }
    // Each solid group gets its own retries; the independent members share theirs.
    let mut units: BTreeMap<Option<u64>, Vec<usize>> = BTreeMap::new();
    for i in open {
        units.entry(group(&pending[i])).or_default().push(i);
    }
    for (key, members) in units {
        salvage(
            source,
            &pending,
            members,
            key.is_some(),
            password,
            &mut states,
        )?;
    }
    // A partial copy comes from a pass of its own, so no other member's bytes follow it.
    // Each solid group gets one such pass, since it decodes the group from its start.
    let mut partial = vec![false; pending.len()];
    if flags.keep_damaged {
        let mut decoded = BTreeSet::new();
        let damaged: Vec<usize> = (0..pending.len())
            .filter(|&i| states[i] == State::Damaged)
            .collect();
        for i in damaged {
            if group(&pending[i]).is_some_and(|block| !decoded.insert(block)) {
                continue;
            }
            if !pass(source, &pending, &[i], password, &mut states)?.is_empty() {
                partial[i] = pending[i].written()? > 0;
            }
        }
    }
    // Finder metadata goes onto the copies before they are finished, and onto the folders.
    let finder = if cfg!(target_os = "macos") && flags.finder && source.format != Format::Unar {
        let described: Vec<&Entry> = pending
            .iter()
            .zip(&states)
            .filter(|(_, state)| **state == State::Copied)
            .map(|(copy, _)| copy.entry)
            .chain(folders.iter().map(|(_, folder)| *folder))
            .filter(|entry| entry.finder)
            .collect();
        finder_data(&index, &described, password)
    } else {
        BTreeMap::new()
    };
    for (i, copy) in pending.iter().enumerate() {
        if states[i] == State::Copied
            && let Some(data) = finder.get(copy.entry.path.as_str())
        {
            crate::finder::apply(data, &copy.open()?);
        }
    }
    for (i, copy) in pending.iter().enumerate() {
        if states[i] == State::Copied {
            finish(
                &copy.open()?,
                copy.before.as_ref(),
                copy.entry,
                flags.preserve_metadata,
            )?;
        }
    }
    if crate::cancelled() {
        return Err(Error::new("cancelled", "Archive operation cancelled"));
    }
    index.source.unchanged()?;
    let (mut report, mut sidecars) = (Report::default(), Vec::new());
    for (i, copy) in pending.into_iter().enumerate() {
        match states[i] {
            State::Copied => {
                index.source.check_destination(copy.destination)?;
                if destination_stamp(copy.destination)? != copy.before {
                    return Err(Error::new(
                        "changed",
                        "Destination changed during extraction",
                    ));
                }
                // A member of unknown size counts what its clean pass wrote.
                let size = if copy.entry.unknown_size {
                    copy.written()?
                } else {
                    copy.entry.size
                };
                publish(copy.temporary, copy.destination, copy.before.is_some())?;
                report.done += size;
            }
            State::Damaged => {
                report.damaged.push(copy.entry.path.clone());
                if partial[i] {
                    sidecars.push(copy);
                }
            }
            State::Open | State::Lost => report.lost.push(copy.entry.path.clone()),
        }
    }
    // Partial copies go last and never replace anything, not even a copy published above.
    for copy in sidecars {
        let mut path = copy.destination.as_os_str().to_owned();
        path.push(".damaged");
        match copy.temporary.persist_noclobber(&path) {
            Ok(()) => report.partial.push(path.into()),
            Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.error.into()),
        }
    }
    // Links come after the copies, and only where their targets stay inside `root`.
    let mut created = BTreeSet::new();
    for (entry, link, target, before) in links {
        if !root.is_some_and(|root| link_inside(root, &target.destination, link, &created)) {
            report.skipped.push(entry.path.clone());
            continue;
        }
        index.source.check_destination(&target.destination)?;
        if destination_stamp(&target.destination)? != before {
            return Err(Error::new(
                "changed",
                "Destination changed during extraction",
            ));
        }
        let parent = target
            .destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let temporary = tempfile::Builder::new()
            .prefix(".zippity-")
            .make_in(parent, |path| std::os::unix::fs::symlink(link, path))?
            .into_temp_path();
        publish(temporary, &target.destination, before.is_some())?;
        created.insert(target.destination.clone());
    }
    // Restore children before their parents: writing into a folder changes its time.
    folders.sort_by(|a, b| b.0.member.cmp(&a.0.member));
    for (target, folder) in folders {
        let file = open_folder(&index.source, &target.destination)?;
        if let Some(data) = finder.get(folder.path.as_str()) {
            crate::finder::apply(data, &file);
        }
        if flags.preserve_metadata {
            apply_metadata(&file, folder)?;
        }
    }
    Ok(report)
}

/// AppleDouble entries larger than this are not applied, nor any past the total.
const MAX_FINDER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FINDER_TOTAL: u64 = 128 * 1024 * 1024;

/// The AppleDouble data holding the Finder metadata of each of `members`, by member path.
/// An entry under `__MACOSX` wins over one beside the member.
fn finder_data(
    index: &Index,
    members: &[&Entry],
    password: Option<&str>,
) -> BTreeMap<String, Vec<u8>> {
    let find = |member: &str| {
        [listing::Style::Mirrored, listing::Style::Beside]
            .into_iter()
            .find_map(|style| {
                let path = listing::companion(member, style);
                let found = index
                    .metadata
                    .binary_search_by(|entry| entry.path.as_str().cmp(&path))
                    .ok()?;
                Some(&index.metadata[found])
                    .filter(|entry| !entry.directory && entry.size <= MAX_FINDER_BYTES)
            })
    };
    let (described, entries): (Vec<&str>, Vec<&Entry>) = members
        .iter()
        .filter_map(|member| Some((member.path.as_str(), find(&member.path)?)))
        .unzip();
    described
        .into_iter()
        .zip(small_data(
            &index.source,
            &entries,
            password,
            MAX_FINDER_TOTAL,
        ))
        .filter_map(|(member, data)| Some((member.to_owned(), data?)))
        .collect()
}

/// Whether a link at `destination` to `target`, which climbs first and then descends as
/// `listing::contain_links` checked, stays inside `root`: no step may be a link already on
/// disk, except a last step that this extraction created and checked itself.
fn link_inside(root: &Path, destination: &Path, target: &str, created: &BTreeSet<PathBuf>) -> bool {
    let Some(mut at) = destination.parent().map(Path::to_path_buf) else {
        return false;
    };
    let Ok(depth) = at
        .strip_prefix(root)
        .map(|below| below.components().count())
    else {
        return false;
    };
    let parts: Vec<&str> = target
        .split('/')
        .filter(|part| !matches!(*part, "" | "."))
        .collect();
    let climb = parts.iter().take_while(|part| **part == "..").count();
    if climb > depth {
        return false;
    }
    for _ in 0..climb {
        at.pop();
    }
    let last = parts.len() - climb;
    parts[climb..].iter().enumerate().all(|(step, name)| {
        at.push(name);
        !fs::symlink_metadata(&at).is_ok_and(|meta| meta.file_type().is_symlink())
            || (step + 1 == last && created.contains(&at))
    })
}

/// One 7zz pass over `members`, in listing order, into their temporary files, split into
/// commands that fit the argument budget. Marks the members it verifies as copied and
/// returns the others, still in order.
fn pass(
    source: &Source,
    pending: &[Pending],
    members: &[usize],
    password: Option<&str>,
    states: &mut [State],
) -> Result<Vec<usize>> {
    let mut open = Vec::new();
    let lengths = members
        .iter()
        .map(|&i| pending[i].entry.original_path.len());
    for range in crate::process::argument_groups(lengths) {
        let members = &members[range];
        let copies: Vec<&Pending> = members.iter().map(|&i| &pending[i]).collect();
        for copy in &copies {
            // A fresh temporary file is empty already.
            if copy.touched.load(Ordering::Relaxed) {
                copy.open()?.set_len(0)?;
            }
        }
        match extract(source, &copies, password) {
            Ok(()) => {
                for &i in members {
                    states[i] = State::Copied;
                }
            }
            Err(err) if err.kind == "damaged" => {
                for &i in members {
                    if pending[i].verified()? {
                        states[i] = State::Copied;
                    } else {
                        open.push(i);
                    }
                }
            }
            Err(err) => return Err(err),
        }
    }
    Ok(open)
}

/// Whether a failed pass can blame its first unverified member. Every member before it
/// matched its listed size, so its bytes started in the right place. In a solid stream, a
/// member that got no bytes may instead sit behind damage the decoder could not get past.
fn blame(copy: &Pending, solid: bool) -> std::io::Result<bool> {
    Ok(copy.entry.crc.is_some() && (!solid || copy.written()? > 0))
}

/// The state of a member that failed a pass of its own: damaged, unless it sits in a solid
/// stream and got no bytes, which may mean the decoder broke before reaching it.
fn alone(copy: &Pending, solid: bool) -> std::io::Result<State> {
    Ok(if !solid || copy.written()? > 0 {
        State::Damaged
    } else {
        State::Lost
    })
}

/// Retries one unit left open by the first passes: a solid group gets at most two more
/// passes; the independent members get four, and then one pass each. Each pass blames its
/// first unverified member when it can and leaves it out of the next one.
fn salvage(
    source: &Source,
    pending: &[Pending],
    mut open: Vec<usize>,
    solid: bool,
    password: Option<&str>,
    states: &mut [State],
) -> Result<()> {
    // Without a CRC, a shared pass can neither verify nor blame anyone.
    let attempts = if !open.iter().any(|&i| pending[i].entry.crc.is_some()) {
        0
    } else if solid {
        2
    } else {
        4
    };
    for _ in 0..attempts {
        if open.is_empty() {
            return Ok(());
        }
        open = pass(source, pending, &open, password, states)?;
        let Some(&first) = open.first() else {
            return Ok(());
        };
        if blame(&pending[first], solid)? {
            states[first] = State::Damaged;
        } else if solid {
            // The stream broke before this member could be checked; nothing after it is trusted.
            break;
        } else {
            // Only a pass of its own can vouch for a member without a CRC.
            single(source, pending, first, password, states)?;
        }
        open.remove(0);
    }
    for i in open {
        if solid {
            states[i] = State::Lost;
        } else {
            single(source, pending, i, password, states)?;
        }
    }
    Ok(())
}

/// One pass for one independent member, so a failure is its own.
fn single(
    source: &Source,
    pending: &[Pending],
    member: usize,
    password: Option<&str>,
    states: &mut [State],
) -> Result<()> {
    if !pass(source, pending, &[member], password, states)?.is_empty() {
        states[member] = State::Damaged;
    }
    Ok(())
}

/// Streams `copies` out of one 7zz pass, each into its temporary file.
fn extract(source: &Source, copies: &[&Pending], password: Option<&str>) -> Result<()> {
    let members: Vec<&str> = copies
        .iter()
        .map(|copy| copy.entry.original_path.as_str())
        .collect();
    let size: u64 = copies.iter().map(|copy| copy.entry.size).sum();
    // A member of unknown size is alone in its archive and takes all the output.
    let unknown = copies.iter().any(|copy| copy.entry.unknown_size);
    let mut split = Split {
        copies,
        current: 0,
        written: 0,
        file: None,
    };
    // Nothing else bounds an unsized stream, so it may not outgrow the destination's free space.
    if source.format == Format::Unar {
        if unar_extract(source, copies, &mut split)? != size {
            return Err(Error::new(
                "damaged",
                "Extracted size does not match the archive listing",
            ));
        }
        return Ok(());
    }
    let limit = if unknown {
        let directory = copies[0].temporary.parent().unwrap_or(Path::new("."));
        let space = rustix::fs::statvfs(directory).map_err(std::io::Error::from)?;
        space.f_bavail.saturating_mul(space.f_frsize)
    } else {
        size
    };
    if run(source, &members, password, &mut split, limit, true)? != size && !unknown {
        return Err(Error::new(
            "damaged",
            "Extracted size does not match the archive listing",
        ));
    }
    Ok(())
}

/// Unpacks `copies` with unar into a private directory beside their destinations, then
/// streams the files into `split` in order. A failed run is member damage: listing worked.
fn unar_extract(source: &Source, copies: &[&Pending], split: &mut Split) -> Result<u64> {
    let directory = copies[0].temporary.parent().unwrap_or(Path::new("."));
    let unpacked = tempfile::Builder::new()
        .prefix(".zippity-")
        .tempdir_in(directory)?;
    let size: u64 = copies.iter().map(|copy| copy.entry.size).sum();
    // The unpacked files and then the copies of them sit side by side until publication.
    let space = rustix::fs::statvfs(directory).map_err(std::io::Error::from)?;
    if space.f_bavail.saturating_mul(space.f_frsize) < size.saturating_mul(2) {
        return Err(Error::new(
            "limit",
            "Not enough free space to unpack these members beside their destination",
        ));
    }
    let mut command = Command::new("unar");
    command
        .env("TZ", "UTC")
        .args(["-q", "-nr", "-D", "-f", "-k", "skip", "-i", "-o"])
        .arg(unpacked.path())
        .arg(&source.archive)
        .args(copies.iter().map(|copy| copy.entry.original_path.as_str()));
    // A missing file, as from a name unar wrote differently, or a link is that member's
    // damage, which a pass of its own can then confirm.
    let open = |copy: &Pending| {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(unpacked.path().join(&copy.entry.path))
            .map_err(|_| Error::new("damaged", "unar did not write a member the listing names"))
    };
    let run = crate::process::run(
        vec![command],
        "",
        &mut std::io::sink(),
        1024 * 1024,
        Duration::from_secs(3600 + size / (1024 * 1024)),
        false,
    );
    match run {
        Ok(_) => {}
        Err(err) if err.kind == "archive" => {
            // A pass of one member keeps what unar wrote, up to its listed size, for an
            // unverified partial copy. Several members' bytes could not be told apart.
            if let [copy] = copies
                && let Ok(file) = open(copy)
                && file.metadata().is_ok_and(|metadata| metadata.is_file())
            {
                let _ = std::io::copy(&mut file.take(copy.entry.size), split);
            }
            return Err(Error::new(
                "damaged",
                "unar could not decode every selected member",
            ));
        }
        Err(err) => return Err(err),
    }
    let mut written = 0;
    for copy in copies {
        let mut file = open(copy)?;
        // Each file must hold its own listed size, or bytes would shift between copies.
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != copy.entry.size {
            return Err(Error::new(
                "damaged",
                "unar wrote a member that does not match the listing",
            ));
        }
        written += std::io::copy(&mut file, split)?;
    }
    Ok(written)
}

/// Writes one 7zz output stream into consecutive files, giving each its listed size.
struct Split<'a, 'b> {
    copies: &'a [&'a Pending<'b>],
    current: usize,
    written: u64,
    file: Option<fs::File>,
}

impl Write for Split<'_, '_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut rest = data;
        while !rest.is_empty() {
            let copy = self.copies.get(self.current).ok_or_else(|| {
                std::io::Error::other("7zz wrote more data than the listing declares")
            })?;
            let size = if copy.entry.unknown_size {
                u64::MAX
            } else {
                copy.entry.size
            };
            let room = size - self.written;
            if room == 0 {
                (self.current, self.written, self.file) = (self.current + 1, 0, None);
                continue;
            }
            let file = match &mut self.file {
                Some(file) => file,
                file => {
                    copy.touched.store(true, Ordering::Relaxed);
                    file.insert(copy.open()?)
                }
            };
            let take = rest.len().min(usize::try_from(room).unwrap_or(usize::MAX));
            file.write_all(&rest[..take])?;
            self.written += take as u64;
            rest = &rest[take..];
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Give a finished copy its final permissions and times. A copy that replaces a file is
/// flushed to disk first, so a crash after the move cannot leave that file emptied; a new
/// file has nothing to lose. Flushing before the destination checks keeps the checks right
/// before the move.
pub(crate) fn finish(
    file: &fs::File,
    before: Option<&DestinationStamp>,
    entry: &Entry,
    preserve_metadata: bool,
) -> Result<()> {
    if preserve_metadata {
        apply_metadata(file, entry)?;
    }
    // Archive contents must not inherit setuid or setgid privileges.
    if let Some(before) = before
        && (!preserve_metadata || entry.mode.is_none())
    {
        file.set_permissions(fs::Permissions::from_mode(before.mode & 0o777))?;
    }
    if before.is_some() {
        file.sync_all()?;
    }
    Ok(())
}

/// Move a finished copy into place, replacing only a destination that existed before.
pub(crate) fn publish(temporary: TempPath, destination: &Path, replace: bool) -> Result<()> {
    let persisted = if replace {
        temporary.persist(destination)
    } else {
        temporary.persist_noclobber(destination)
    };
    persisted.map_err(|e| e.error.into())
}

#[derive(PartialEq)]
pub(crate) struct DestinationStamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    pub mode: u32,
}

pub(crate) fn destination_stamp(path: &Path) -> Result<Option<DestinationStamp>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(DestinationStamp {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
            mode: meta.mode(),
        })),
        Ok(_) => Err(Error::new(
            "unsupported",
            "Destination must be a regular file or a new path",
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub enum Operation<'a> {
    List,
    Extract(&'a [&'a str]),
    Test,
}

pub fn commands(source: &Source, operation: Operation<'_>) -> Vec<Command> {
    let format = source.format;
    if format == Format::Unar {
        let mut command = Command::new("lsar");
        command.env("TZ", "UTC");
        match operation {
            Operation::List => command.args(["-j", "-nr"]),
            Operation::Test => command.args(["-t", "-nr"]),
            // unar writes files, not a stream; `unar_extract` runs it.
            Operation::Extract(_) => return Vec::new(),
        };
        command.arg(&source.archive);
        return vec![command];
    }
    if format.single_file() {
        // The one member is the decompressed stream.
        if let Operation::Extract(_) = operation {
            return vec![
                format
                    .decompressor(&source.archive, source.multipart)
                    .expect("compressed"),
            ];
        }
        let Some(kind) = format.type_switch(source.multipart) else {
            // Only lz4 lacks a 7zz type, and its listing comes from the frame header.
            let mut command = Command::new("lz4");
            command.args(["-t", "-q"]).arg(&source.archive);
            return vec![command];
        };
        let mut command = Command::new("7zz");
        command.env("TZ", "UTC");
        match operation {
            Operation::List => command.args(["l", "-slt", "-ba"]),
            _ => command.args(["t", "-bso0", "-bse2"]),
        };
        command
            .args(["-bd", "-bb0", "-bsp0", "-sccUTF-8"])
            .arg(kind)
            .arg("--")
            .arg(&source.archive);
        return vec![command];
    }
    let compressed = format.compressed_tar();
    let mut commands = Vec::new();
    if compressed {
        commands.push(
            format
                .decompressor(&source.archive, source.multipart)
                .expect("compressed"),
        );
    }
    let mut command = Command::new("7zz");
    command.env("TZ", "UTC");
    match operation {
        Operation::List => {
            command.args(["l", "-slt", "-ba"]);
        }
        Operation::Extract(members) => {
            // A member without a name is alone in its archive and needs no filter.
            command
                .args(["x", "-so", "-bso0", "-bse2", "-spd", "-y"])
                .args(
                    members
                        .iter()
                        .filter(|member| !member.is_empty())
                        .map(|member| format!("-i!{member}")),
                );
        }
        Operation::Test => {
            command.args(["t", "-bso0", "-bse2"]);
        }
    }
    command.args(["-bd", "-bb0", "-bsp0", "-sccUTF-8"]);
    if compressed {
        command.args(["-si", "-ttar"]);
    } else {
        match source.format {
            Format::Tar => {
                command.arg("-ttar");
            }
            // Without a type, 7zz follows a .deb into its data stream and hides the rest.
            Format::Ar => {
                command.arg("-tAr");
            }
            _ => {}
        }
        command.arg("--").arg(&source.archive);
    }
    commands.push(command);
    commands
}

pub fn run<W: Write + Send>(
    source: &Source,
    members: &[&str],
    password: Option<&str>,
    output: &mut W,
    limit: u64,
    progress: bool,
) -> Result<u64> {
    let operation = if members.is_empty() {
        Operation::List
    } else {
        Operation::Extract(members)
    };
    // Plain listings are quick. Other runs get an hour plus a second for every MiB they write,
    // counting at most a hundred times the archive's size, so an unsized stream also ends.
    let timeout = if members.is_empty() && !source.format.compressed_tar() {
        60
    } else {
        let packed: u64 = source.volumes.iter().map(|volume| volume.size).sum();
        3600 + limit.min(packed.saturating_mul(100)) / (1024 * 1024)
    };
    crate::process::run(
        commands(source, operation),
        &crate::process::password_input(password)?,
        output,
        limit,
        Duration::from_secs(timeout),
        progress,
    )
}

pub fn verify(archive: &Path, revision: &str, password: Option<&str>) -> Result<()> {
    let source = Source::open(archive)?;
    if source.revision != revision {
        return Err(Error::new("changed", "Archive changed; reopen it"));
    }
    crate::process::run(
        commands(&source, Operation::Test),
        &crate::process::password_input(password)?,
        &mut std::io::sink(),
        MAX_LISTING_BYTES,
        Duration::from_secs(3600),
        false,
    )?;
    source.unchanged()
}

pub(crate) fn apply_metadata(file: &fs::File, entry: &Entry) -> Result<()> {
    if let Some(mode) = entry.mode {
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    }
    if let Some(time) = entry.modified {
        file.set_times(
            fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs_f64(time)),
        )?;
    }
    Ok(())
}

/// Opens an extracted folder to give it its archived metadata, checking that it is the
/// folder it should be.
fn open_folder(source: &Source, destination: &Path) -> Result<fs::File> {
    source.check_destination(destination)?;
    let before = fs::symlink_metadata(destination)?;
    if before.is_symlink() || !before.is_dir() {
        return Err(Error::new(
            "unsupported",
            "Destination metadata does not match the archive member",
        ));
    }
    let file = fs::File::open(destination)?;
    let opened = file.metadata()?;
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        return Err(Error::new(
            "changed",
            "Destination changed while opening it",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::{crc32, without_password_prompt, xad_listing};

    #[test]
    fn listings_drop_either_password_prompt() {
        for prompt in [
            "\nEnter password:\n",
            "\nEnter password (will not be echoed):\n",
        ] {
            let text = format!("{prompt}--\nPath = a.rar\n");
            assert_eq!(without_password_prompt(&text), "--\nPath = a.rar\n");
        }
        assert_eq!(
            without_password_prompt("--\nPath = a.rar\n"),
            "--\nPath = a.rar\n"
        );
    }

    #[test]
    fn crc32_matches_the_standard_check_value_in_pieces() {
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn xad_listings_follow_the_listing_rules() {
        let text = r#"{"lsarContents": [
            {"XADFileName": "root.txt", "XADFileSize": 5, "XADIndex": 0, "XADIsResourceFork": 1},
            {"XADFileName": "root.txt", "XADFileSize": 20, "XADIndex": 1, "XADPosixPermissions": 33188,
             "XADLastModificationDate": "2026-10-25 01:52:19 +0000"},
            {"XADFileName": "sub", "XADIsDirectory": 1, "XADIndex": 2},
            {"XADFileName": "sub/deep.txt", "XADFileSize": 10, "XADIndex": 3},
            {"XADFileName": "link", "XADFileSize": 8, "XADIndex": 4, "XADIsLink": 1},
            {"XADFileName": "../escape", "XADFileSize": 1, "XADIndex": 5},
            {"XADFileName": "sizeless", "XADIndex": 6}
        ]}"#;
        let listing = xad_listing(text, "unnamed").unwrap();
        let entries: Vec<_> = listing
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.original_path.as_str(), e.size))
            .collect();
        // The data fork, not the resource fork that shares its name, stands for root.txt.
        assert_eq!(
            entries,
            [
                ("root.txt", "1", 20),
                ("sub", "2", 0),
                ("sub/deep.txt", "3", 10)
            ]
        );
        assert_eq!(listing.hidden, 4);
        assert_eq!(listing.entries[0].mode, Some(0o644));
        assert_eq!(listing.entries[0].modified, Some(1_792_893_139.0));
        let encrypted = r#"{"lsarContents": [
            {"XADFileName": "a", "XADFileSize": 1, "XADIndex": 0, "XADIsEncrypted": 1}
        ]}"#;
        assert_eq!(
            xad_listing(encrypted, "unnamed").unwrap_err().kind,
            "unsupported"
        );
    }
}
