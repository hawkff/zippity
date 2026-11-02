use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::error::{Error, Result};

pub const MAX_ENTRIES: usize = 200_000;
const MAX_PATH_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub path: String,
    pub original_path: String,
    pub directory: bool,
    #[serde(skip)]
    pub implicit_directory: bool,
    pub size: u64,
    /// The format does not store the size; only a lone member can lack one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unknown_size: bool,
    pub encrypted: bool,
    pub modified: Option<f64>,
    pub mode: Option<u32>,
    /// A symbolic link's target. Only links whose targets stay inside the archive are kept,
    /// and copies recreate them as links.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The archive holds macOS Finder metadata for this member.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub finder: bool,
    /// Order in the 7zz listing, which is also the order 7zz extracts members in.
    #[serde(skip)]
    pub position: usize,
    /// CRC-32 of the member's data, when the format stores one.
    #[serde(skip)]
    pub crc: Option<u32>,
    /// Members of one group decode as one solid stream: a 7z block or a RAR solid run.
    #[serde(skip)]
    pub group: Option<u64>,
}

pub fn validate_path(path: &str) -> Result<()> {
    let bytes = path.as_bytes();
    let drive_prefix =
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
    if path.is_empty()
        || path.len() > 4096
        || path.starts_with('/')
        || path.contains('\\')
        || drive_prefix
        || path.chars().any(char::is_control)
        || path.split('/').count() > 128
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(Error::new(
            "unsupported",
            "Archive contains an unsafe or unsupported member path",
        ));
    }
    Ok(())
}

/// What a listing holds: the members zippity can show, and how many others it hides.
#[derive(Debug)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Links, special files, alternate streams, and names zippity cannot use safely.
    /// They are never extracted.
    pub hidden: usize,
}

enum Parsed {
    Entry(Entry),
    Skip,
    /// Hidden; true for a directory.
    Hidden(bool),
}

/// Parses `7zz l -slt -ba` output. A lone member without a name, as in single-stream
/// formats, is shown as `unnamed`.
pub fn parse(text: &str, unnamed: &str) -> Result<Listing> {
    let mut entries = BTreeMap::new();
    let mut fields = Fields::default();
    let (mut path_bytes, mut run, mut hidden, mut nameless) = (0, 0, 0, Vec::new());
    // Split on newlines only: a name ending in a carriage return must keep it.
    for line in text.split('\n').chain(std::iter::once("")) {
        if line.is_empty() {
            if !fields.is_empty() {
                let mut entry = match entry(&fields, unnamed)? {
                    Parsed::Entry(entry) => entry,
                    Parsed::Skip => {
                        fields.clear();
                        continue;
                    }
                    Parsed::Hidden(directory) => {
                        // A hidden member can still start the solid run that later ones continue.
                        if !directory && let Some(solid) = fields.get("Solid") {
                            run += u64::from(*solid != "+");
                        }
                        hidden += 1;
                        fields.clear();
                        continue;
                    }
                };
                if entry.original_path.is_empty() {
                    nameless.push(entry);
                    fields.clear();
                    continue;
                }
                entry.position = entries.len();
                // RAR flags each member that continues the solid stream of the one before it.
                if !entry.directory
                    && let Some(solid) = fields.get("Solid")
                {
                    run += u64::from(*solid != "+");
                    entry.group = Some(run);
                }
                path_bytes += entry.path.len();
                if path_bytes > MAX_PATH_BYTES {
                    return Err(Error::new(
                        "limit",
                        "Archive paths exceed the index size limit",
                    ));
                }
                if entries.insert(entry.path.clone(), entry).is_some() {
                    return Err(Error::new(
                        "unsupported",
                        "Duplicate archive member paths are unsupported",
                    ));
                }
                if entries.len() > MAX_ENTRIES {
                    return Err(Error::new("limit", "Archive has too many entries"));
                }
                fields.clear();
            }
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            return Err(Error::new(
                "unsupported",
                "Cannot read the 7zz listing; multiline names or comments are unsupported",
            ));
        };
        if fields.get(key).is_some() {
            return Err(Error::new("unsupported", "Ambiguous 7zz listing"));
        }
        fields.0.push((key, value));
    }

    // Only a member that is alone in its archive can be extracted without its name.
    if nameless.len() == 1 && entries.is_empty() {
        let entry = nameless.pop().expect("one member");
        entries.insert(entry.path.clone(), entry);
    }
    hidden += nameless.len();
    // Extraction needs sizes to split one output stream among several members.
    if entries.len() > 1 {
        let before = entries.len();
        entries.retain(|_, entry| !entry.unknown_size);
        hidden += before - entries.len();
    }

    // A group of one member does not depend on any other member.
    let mut sizes = BTreeMap::new();
    for group in entries.values().filter_map(|entry| entry.group) {
        *sizes.entry(group).or_insert(0) += 1;
    }
    for entry in entries.values_mut() {
        entry.group = entry.group.filter(|group| sizes[group] > 1);
    }

    // Folders that only member paths imply. A walk up from a member stops at a folder that
    // exists or that an earlier walk found, so each folder is visited once.
    let (mut implied, mut through) = (BTreeSet::new(), BTreeSet::new());
    for path in entries.keys() {
        let mut parent = path.as_str();
        while let Some((next, _)) = parent.rsplit_once('/') {
            parent = next;
            match entries.get(parent) {
                // A link with members below it would be extracted through, so it is hidden
                // and stands as a folder.
                Some(entry) if entry.link.is_some() => {
                    through.insert(parent);
                }
                Some(entry) if !entry.directory => {
                    return Err(Error::new(
                        "unsupported",
                        "Archive contains a file/directory name conflict",
                    ));
                }
                Some(_) => break,
                None => {}
            }
            if !implied.insert(parent) {
                break;
            }
            path_bytes += parent.len();
            if path_bytes > MAX_PATH_BYTES {
                return Err(Error::new(
                    "limit",
                    "Archive paths exceed the index size limit",
                ));
            }
        }
    }
    if entries.len() + implied.len() - through.len() > MAX_ENTRIES {
        return Err(Error::new("limit", "Archive has too many entries"));
    }
    let implied: Vec<String> = implied.into_iter().map(str::to_owned).collect();
    let through: Vec<String> = through.into_iter().map(str::to_owned).collect();
    hidden += through.len();
    for link in &through {
        entries.remove(link);
    }
    for parent in implied {
        let entry = Entry {
            path: parent.clone(),
            original_path: parent.clone(),
            directory: true,
            implicit_directory: true,
            size: 0,
            unknown_size: false,
            encrypted: false,
            modified: None,
            mode: None,
            link: None,
            finder: false,
            position: 0,
            crc: None,
            group: None,
        };
        entries.insert(parent, entry);
    }
    Ok(Listing {
        entries: entries.into_values().collect(),
        hidden,
    })
}

/// The fields of one listing record. A record has about twenty, so a search through them
/// costs less than building a map for every member.
#[derive(Default)]
struct Fields<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Fields<'a> {
    fn get(&self, key: &str) -> Option<&&'a str> {
        self.0
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

fn entry(fields: &Fields, unnamed: &str) -> Result<Parsed> {
    let attributes = fields.get("Attributes").copied().unwrap_or_default();
    let directory = fields.get("Folder") == Some(&"+")
        // Windows attribute letters come first, as RD for a read-only folder.
        || attributes
            .split_whitespace()
            .next()
            .is_some_and(|letters| letters.contains('D'))
        || attributes.trim_start().starts_with('d')
        || fields.get("Mode").is_some_and(|mode| mode.starts_with('d'));
    let (path, original_path) = match fields.get("Path") {
        Some(path) => {
            let path = if directory {
                path.trim_end_matches('/')
            } else {
                path
            };
            if directory
                && path == "."
                && fields.get("Size").is_none_or(|v| v.is_empty() || *v == "0")
            {
                return Ok(Parsed::Skip);
            }
            (path.trim_start_matches("./"), path)
        }
        None if !directory => (unnamed, ""),
        None => return Ok(Parsed::Hidden(true)),
    };
    let unix_mode = fields
        .get("Mode")
        .copied()
        .filter(|mode| !mode.is_empty())
        .or_else(|| attributes.split_whitespace().find(|s| s.len() == 10));
    // Windows attribute letters come first; L marks a reparse point, such as a link.
    let reparse = attributes.split_whitespace().next().is_some_and(|letters| {
        letters.bytes().all(|b| b.is_ascii_uppercase()) && letters.contains('L')
    });
    let size = fields.get("Size").and_then(|value| {
        if directory && value.is_empty() {
            Some(0)
        } else {
            value.parse().ok()
        }
    });
    let linked = |name| fields.get(name).is_some_and(|value| !value.is_empty());
    let posix_link = unix_mode.is_some_and(|mode| mode.starts_with('l'))
        || attributes.split_whitespace().any(|v| v.starts_with('l'));
    // tar and RAR listings show a symbolic link's target; 7z and ZIP keep it as the
    // member's data, which `engine::list` reads. Other kinds of links stay hidden.
    let symlink = !directory
        && (posix_link || linked("Symbolic Link"))
        && !linked("Hard Link")
        && !linked("Link")
        && !reparse;
    if path.len() > 4096
        || validate_path(path).is_err()
        || unix_mode.is_some_and(|mode| mode.starts_with(['b', 'c', 'p', 's']))
        || (!symlink && (posix_link || linked("Symbolic Link")))
        || linked("Hard Link")
        || linked("Link")
        || reparse
        || fields.get("Alternate Stream") == Some(&"+")
    {
        return Ok(Parsed::Hidden(directory));
    }
    let (size, unknown_size) = match size {
        Some(size) if !directory || size == 0 => (size, false),
        None if !directory => (0, true),
        _ => return Ok(Parsed::Hidden(directory)),
    };
    Ok(Parsed::Entry(Entry {
        path: path.to_owned(),
        original_path: original_path.to_owned(),
        directory,
        implicit_directory: false,
        size,
        unknown_size,
        encrypted: fields.get("Encrypted") == Some(&"+"),
        position: 0,
        crc: fields
            .get("CRC")
            .and_then(|value| u32::from_str_radix(value, 16).ok()),
        group: fields.get("Block").and_then(|value| value.parse().ok()),
        mode: unix_mode.and_then(parse_mode),
        link: symlink.then(|| {
            fields
                .get("Symbolic Link")
                .copied()
                .unwrap_or_default()
                .to_owned()
        }),
        finder: false,
        modified: fields.get("Modified").and_then(|value| {
            let date = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()?
                .and_utc();
            (date.timestamp() >= 0)
                .then(|| date.timestamp() as f64 + f64::from(date.timestamp_subsec_nanos()) / 1e9)
        }),
    }))
}

/// Hides the symbolic links whose targets leave the archive, pass through another link,
/// or are unknown, and returns how many. A kept target climbs with `..` first, then names
/// members below. A link holds no data of its own, so its size becomes zero.
pub fn contain_links(entries: &mut Vec<Entry>) -> usize {
    let links: BTreeSet<String> = entries
        .iter()
        .filter(|entry| entry.link.is_some())
        .map(|entry| entry.path.clone())
        .collect();
    let before = entries.len();
    entries.retain(|entry| {
        entry
            .link
            .as_deref()
            .is_none_or(|target| contained(&entry.path, target, &links))
    });
    for entry in entries.iter_mut().filter(|entry| entry.link.is_some()) {
        entry.size = 0;
        entry.crc = None;
    }
    before - entries.len()
}

fn contained(path: &str, target: &str, links: &BTreeSet<String>) -> bool {
    if target.is_empty()
        || target.len() > 4096
        || target.starts_with('/')
        || target.contains('\\')
        || target.chars().any(char::is_control)
    {
        return false;
    }
    let mut at: Vec<&str> = path.split('/').collect();
    at.pop();
    let mut descending = false;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." if !descending => {
                if at.pop().is_none() {
                    return false;
                }
            }
            ".." => return false,
            name => {
                // Passing through another link could lead anywhere.
                if descending && links.contains(&at.join("/")) {
                    return false;
                }
                descending = true;
                at.push(name);
            }
        }
    }
    true
}

/// How an AppleDouble entry, which holds a member's macOS Finder metadata, sits in an
/// archive relative to that member.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    /// `__MACOSX/<folder>/._<name>`, as Finder and Archive Utility write it.
    Mirrored,
    /// A folder under `__MACOSX` that mirrors the folder with its path.
    MirrorFolder,
    /// `<folder>/._<name>` beside the member, as macOS tar writes it.
    Beside,
}

/// The member that the Finder metadata entry at `path` describes, and how it sits.
pub fn described(path: &str, directory: bool) -> Option<(String, Style)> {
    let (rest, mirrored) = match path.strip_prefix("__MACOSX/") {
        Some(rest) => (rest, true),
        None => (path, false),
    };
    if directory {
        return mirrored.then(|| (rest.to_owned(), Style::MirrorFolder));
    }
    let (folder, name) = match rest.rsplit_once('/') {
        Some((folder, name)) => (Some(folder), name),
        None => (None, rest),
    };
    let name = name.strip_prefix("._").filter(|name| !name.is_empty())?;
    let member = folder.map_or_else(|| name.to_owned(), |folder| format!("{folder}/{name}"));
    let style = if mirrored {
        Style::Mirrored
    } else {
        Style::Beside
    };
    Some((member, style))
}

/// The path of the entry in `style` that holds `member`'s Finder metadata.
pub fn companion(member: &str, style: Style) -> String {
    let (folder, name) = match member.rsplit_once('/') {
        Some((folder, name)) => (format!("{folder}/"), name),
        None => (String::new(), member),
    };
    match style {
        Style::Mirrored => format!("__MACOSX/{folder}._{name}"),
        Style::MirrorFolder => format!("__MACOSX/{member}"),
        Style::Beside => format!("{folder}._{name}"),
    }
}

/// Moves macOS Finder metadata out of `entries` and returns it: everything under a
/// top-level `__MACOSX` folder, and `._name` files beside a `name` member. Listings do not
/// show it and extraction never writes it as files; extraction on macOS applies it, and
/// edits keep it with its members. The members it describes get `finder` set.
pub fn split_mac_metadata(entries: &mut Vec<Entry>) -> Vec<Entry> {
    let mirrored = |path: &str| path == "__MACOSX" || path.starts_with("__MACOSX/");
    let targets: Vec<_> = entries
        .iter()
        .map(|entry| described(&entry.path, entry.directory))
        .collect();
    if targets.iter().all(Option::is_none) && !entries.iter().any(|e| mirrored(&e.path)) {
        return Vec::new();
    }
    // A `._name` file is metadata only beside a member called `name`.
    let wanted: BTreeSet<&str> = targets
        .iter()
        .flatten()
        .filter(|(_, style)| *style == Style::Beside)
        .map(|(member, _)| member.as_str())
        .collect();
    let present: BTreeSet<&str> = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .filter(|path| wanted.contains(path))
        .collect();
    let (mut members, mut keep) = (BTreeSet::new(), Vec::with_capacity(entries.len()));
    for (entry, target) in entries.iter().zip(&targets) {
        let kept = match target {
            Some((member, Style::Beside)) => !present.contains(member.as_str()),
            _ => !mirrored(&entry.path),
        };
        if !kept
            && let Some((member, style)) = target
            && *style != Style::MirrorFolder
        {
            members.insert(member.clone());
        }
        keep.push(kept);
    }
    let (kept, metadata): (Vec<_>, Vec<_>) = std::mem::take(entries)
        .into_iter()
        .zip(keep)
        .partition(|(_, kept)| *kept);
    *entries = kept.into_iter().map(|(entry, _)| entry).collect();
    for entry in entries.iter_mut() {
        entry.finder = members.contains(&entry.path);
    }
    metadata.into_iter().map(|(entry, _)| entry).collect()
}

fn parse_mode(mode: &str) -> Option<u32> {
    if mode.len() != 10 {
        return None;
    }
    let mut result = 0;
    for (index, value) in mode.bytes().skip(1).enumerate() {
        let set = match index % 3 {
            0 => value == b'r',
            1 => value == b'w',
            _ => matches!(value, b'x' | b's' | b't'),
        };
        if set {
            result |= 1 << (8 - index);
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(text: &str) -> Vec<Entry> {
        parse(text, "unnamed").unwrap().entries
    }

    #[test]
    fn builds_implicit_directories() {
        let entries = entries("Path = a/b/notes.txt\nFolder = -\nSize = 5\nEncrypted = +\n\n");
        assert_eq!(
            entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["a", "a/b", "a/b/notes.txt"]
        );
        assert!(entries[0].directory && entries[0].implicit_directory);
        assert!(entries[1].implicit_directory);
        assert!(entries[2].encrypted && !entries[2].implicit_directory);
        let explicit = self::entries("Path = a\nFolder = +\nSize = 0\n");
        assert!(!explicit[0].implicit_directory);
    }

    #[test]
    fn keeps_links_that_stay_inside_the_archive() {
        let listing = parse(
            "Path = dir/file\nSize = 1\nMode = -rw-r--r--\n\n\
             Path = dir/rel\nSize = 4\nMode = lrwxrwxrwx\nSymbolic Link = file\n\n\
             Path = dir/up\nSize = 7\nMode = lrwxrwxrwx\nSymbolic Link = ../file\n\n\
             Path = dir/out\nSize = 10\nMode = lrwxrwxrwx\nSymbolic Link = ../../file\n\n\
             Path = dir/abs\nSize = 4\nMode = lrwxrwxrwx\nSymbolic Link = /etc\n\n\
             Path = dir/via\nSize = 5\nMode = lrwxrwxrwx\nSymbolic Link = rel/x\n\n\
             Path = dir/back\nSize = 9\nMode = lrwxrwxrwx\nSymbolic Link = file/../x\n\n\
             Path = dir/hard\nSize = 1\nMode = -rw-r--r--\nHard Link = dir/file\n\n\
             Path = holder\nSize = 3\nMode = lrwxrwxrwx\nSymbolic Link = dir\n\n\
             Path = holder/inside\nSize = 1\nMode = -rw-r--r--\n",
            "unnamed",
        )
        .unwrap();
        let mut entries = listing.entries;
        let hidden = listing.hidden + contain_links(&mut entries);
        let links: Vec<_> = entries
            .iter()
            .filter_map(|e| Some((e.path.as_str(), e.link.as_deref()?, e.size)))
            .collect();
        assert_eq!(links, [("dir/rel", "file", 0), ("dir/up", "../file", 0)]);
        // out, abs, via, back, the hard link, and holder, which has a member below it.
        assert_eq!(hidden, 6);
        assert!(
            entries
                .iter()
                .any(|e| e.path == "holder" && e.implicit_directory)
        );
    }

    #[test]
    fn moves_finder_metadata_out_of_the_listing() {
        let mut entries = entries(
            "Path = __MACOSX\nFolder = +\nSize = 0\n\n\
             Path = __MACOSX/dir\nFolder = +\nSize = 0\n\n\
             Path = __MACOSX/dir/._a.txt\nSize = 4\n\n\
             Path = __MACOSX/._dir\nSize = 4\n\n\
             Path = dir/a.txt\nSize = 1\n\n\
             Path = dir/b.txt\nSize = 1\n\n\
             Path = dir/._b.txt\nSize = 4\n\n\
             Path = ._lonely\nSize = 4\n",
        );
        let metadata = split_mac_metadata(&mut entries);
        let shown: Vec<_> = entries
            .iter()
            .map(|e| (e.path.as_str(), e.finder))
            .collect();
        assert_eq!(
            shown,
            [
                ("._lonely", false),
                ("dir", true),
                ("dir/a.txt", true),
                ("dir/b.txt", true)
            ]
        );
        let moved: Vec<_> = metadata.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            moved,
            [
                "__MACOSX",
                "__MACOSX/._dir",
                "__MACOSX/dir",
                "__MACOSX/dir/._a.txt",
                "dir/._b.txt"
            ]
        );
        for entry in &metadata[1..] {
            let (member, style) = described(&entry.path, entry.directory).unwrap();
            assert_eq!(companion(&member, style), entry.path);
        }
        assert!(described("__MACOSX/dir/notes.txt", false).is_none());
    }

    #[test]
    fn reads_folders_from_windows_attribute_letters() {
        // 7z listings carry no Folder field, and a read-only or hidden folder prints RD or HD.
        let entries = entries(
            "Path = locked\nSize = 0\nAttributes = RD dr-xr-xr-x\n\n\
             Path = locked/a.txt\nSize = 1\nAttributes = A -rw-r--r--\n\n\
             Path = hidden\nSize = 0\nAttributes = HD\n",
        );
        let folders = entries
            .iter()
            .filter(|e| e.directory)
            .map(|e| e.path.as_str());
        assert!(folders.eq(["hidden", "locked"]));
    }

    #[test]
    fn accepts_blank_disc_directory_sizes_and_unsized_lone_files() {
        let listing = parse("Path = folder\nFolder = +\nSize = \n", "unnamed").unwrap();
        assert!(listing.entries[0].directory && listing.entries[0].size == 0);
        // A lone member may lack a size, as in a bzip2 file; among others it is hidden.
        let listing = parse("Path = file\nFolder = -\nSize = \n", "unnamed").unwrap();
        assert!(listing.entries[0].unknown_size && listing.hidden == 0);
        for text in [
            "Path = file\nFolder = -\nSize = \n\nPath = other\nSize = 1\n",
            "Path = folder\nFolder = +\nSize = 2048\n",
        ] {
            let listing = parse(text, "unnamed").unwrap();
            assert!(
                listing.hidden == 1 && listing.entries.iter().all(|e| !e.unknown_size),
                "{text:?}"
            );
        }
    }

    #[test]
    fn parses_fractional_utc_timestamps() {
        let entries = entries("Path = notes.txt\nSize = 1\nModified = 1970-01-02 00:00:01.125\n");
        assert_eq!(entries[0].modified, Some(86401.125));
    }

    #[test]
    fn rejects_traversal_and_ambiguous_paths() {
        for path in [
            "../file",
            "/file",
            "a/../b",
            "a//b",
            "a\\b",
            "C:/file",
            "C:relative",
            "a\nb",
            "./a",
            "",
        ] {
            assert!(validate_path(path).is_err(), "{path:?}");
        }
        for path in [
            "file with spaces",
            "資料/é.txt",
            "-name",
            "@name",
            "a*?.txt",
            "2024-01-01 10:30.log",
            "folder/name:part",
        ] {
            validate_path(path).unwrap();
        }
    }

    #[test]
    fn reads_crcs_and_solid_groups() {
        let entries = entries(concat!(
            "Path = a\nSize = 1\nCRC = 0000000A\nSolid = -\n\n",
            "Path = b\nSize = 1\nSolid = +\n\n",
            "Path = c\nSize = 1\nSolid = -\n\n",
            "Path = d\nSize = 1\nBlock = 4\n\n",
            "Path = e\nSize = 1\nBlock = 4\n\n",
            "Path = f\nSize = 1\nBlock = 5\n\n",
        ));
        let groups: Vec<_> = entries.iter().map(|entry| entry.group).collect();
        // A RAR run or a 7z block of one member stands alone.
        assert_eq!(groups, [Some(1), Some(1), None, Some(4), Some(4), None]);
        assert_eq!(entries[0].crc, Some(10));
        assert_eq!(entries[1].crc, None);
        // A hidden member that starts a run still separates the runs around it.
        let runs = self::entries(concat!(
            "Path = a\nSize = 1\nSolid = -\n\n",
            "Path = b\nSize = 1\nSolid = +\n\n",
            "Path = ../hidden\nSize = 1\nSolid = -\n\n",
            "Path = c\nSize = 1\nSolid = +\n\n",
            "Path = d\nSize = 1\nSolid = +\n\n",
        ));
        let groups: Vec<_> = runs.iter().map(|entry| entry.group).collect();
        assert_eq!(groups, [Some(1), Some(1), Some(2), Some(2)]);
    }

    #[test]
    fn hides_links_special_files_streams_and_unusable_names() {
        let listing = parse(
            concat!(
                "Path = kept.txt\nSize = 1\n\n",
                "Path = cpio-link\nSize = 8\nMode = lrwxrwxrwx\nSymbolic Link = kept.txt\n\n",
                "Path = wim-link\nSize = 48\nAttributes = L\nLink = kept.txt\n\n",
                "Path = hard\nSize = 1\nHard Link = kept.txt\n\n",
                "Path = device\nSize = 0\nMode = crw-r--r--\n\n",
                "Path = kept.txt:stream\nSize = 1\nAlternate Stream = +\n\n",
                "Path = ../outside\nSize = 1\n\n",
                "Path = private\rdata\nFolder = +\nSize = \n\n",
                "Path = folder\nSize = \nMode = drwxr-xr-x\n\n",
            ),
            "unnamed",
        )
        .unwrap();
        let paths: Vec<_> = listing
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.directory, e.link.as_deref()))
            .collect();
        // A symbolic link stays for `contain_links` to judge; other links stay hidden.
        assert_eq!(
            paths,
            [
                ("cpio-link", false, Some("kept.txt")),
                ("folder", true, None),
                ("kept.txt", false, None)
            ]
        );
        assert_eq!(listing.hidden, 6);
    }

    #[test]
    fn names_a_lone_member_without_a_path() {
        let listing = parse("Size = 10240\nPacked Size = 292\n\n", "sample").unwrap();
        assert_eq!(listing.entries.len(), 1);
        let entry = &listing.entries[0];
        assert!(entry.path == "sample" && entry.original_path.is_empty() && entry.size == 10240);
        let listing = parse("Size = 1\n\nPath = other\nSize = 2\n\n", "sample").unwrap();
        assert!(
            listing.entries.len() == 1 && listing.entries[0].path == "other" && listing.hidden == 1
        );
        let listing = parse("Size = 1\n\nSize = 2\n\n", "sample").unwrap();
        assert!(listing.entries.is_empty() && listing.hidden == 2);
    }

    #[test]
    fn rejects_duplicate_paths_fields_and_conflicts() {
        for text in [
            "Path = a\nSize = 1\n\nPath = a\nSize = 1\n",
            "Path = a\nSize = 1\nSize = 2\n",
            "Path = a\nSize = 1\n\nPath = a/b\nSize = 1\n",
            "Path = a\nwith newline\nSize = 1\n",
        ] {
            assert!(parse(text, "unnamed").is_err(), "{text:?}");
        }
    }
}
