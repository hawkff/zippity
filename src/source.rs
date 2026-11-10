use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Hash, Serialize)]
pub struct Volume {
    pub path: PathBuf,
    pub size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Format {
    #[serde(rename = "zip")]
    Zip,
    #[serde(rename = "rar")]
    Rar,
    #[serde(rename = "7z")]
    SevenZip,
    #[serde(rename = "iso")]
    Iso,
    #[serde(rename = "tar")]
    Tar,
    #[serde(rename = "tar.gz")]
    TarGzip,
    #[serde(rename = "tar.bz2")]
    TarBzip2,
    #[serde(rename = "tar.xz")]
    TarXz,
    #[serde(rename = "tar.zst")]
    TarZstd,
    #[serde(rename = "tar.lzma")]
    TarLzma,
    #[serde(rename = "tar.Z")]
    TarZ,
    #[serde(rename = "tar.lz4")]
    TarLz4,
    /// One compressed file, shown as its only member.
    #[serde(rename = "gz")]
    Gzip,
    #[serde(rename = "bz2")]
    Bzip2,
    #[serde(rename = "xz")]
    Xz,
    #[serde(rename = "zst")]
    Zstd,
    #[serde(rename = "lzma")]
    Lzma,
    #[serde(rename = "Z")]
    Z,
    #[serde(rename = "lz4")]
    Lz4,
    /// ar archives and Debian packages, opened at their own level.
    #[serde(rename = "ar")]
    Ar,
    /// Read-only formats 7zz detects by itself: packages, installers, disk and file
    /// system images. 7zz follows disk images down to the file system inside.
    #[serde(rename = "other")]
    Other,
    /// StuffIt and Amiga LZX archives, which only a separately installed unar reads.
    #[serde(rename = "unar")]
    Unar,
}

impl Format {
    pub fn compressed_tar(self) -> bool {
        matches!(
            self,
            Self::TarGzip
                | Self::TarBzip2
                | Self::TarXz
                | Self::TarZstd
                | Self::TarLzma
                | Self::TarZ
                | Self::TarLz4
        )
    }

    pub fn single_file(self) -> bool {
        matches!(
            self,
            Self::Gzip | Self::Bzip2 | Self::Xz | Self::Zstd | Self::Lzma | Self::Z | Self::Lz4
        )
    }

    /// The 7zz type of the compression around a tar or a single file; lz4 has none.
    pub fn compression(self) -> Option<&'static str> {
        Some(match self {
            Self::TarGzip | Self::Gzip => "gzip",
            Self::TarBzip2 | Self::Bzip2 => "bzip2",
            Self::TarXz | Self::Xz => "xz",
            Self::TarZstd | Self::Zstd => "zstd",
            Self::TarLzma | Self::Lzma => "lzma",
            Self::TarZ | Self::Z => "Z",
            _ => return None,
        })
    }

    /// The 7zz type switch for the compression. Volumes are joined first: in a type chain,
    /// the outer type comes last.
    pub fn type_switch(self, volumes: bool) -> Option<String> {
        let kind = self.compression()?;
        Some(if volumes {
            format!("-t{kind}.Split")
        } else {
            format!("-t{kind}")
        })
    }

    /// The command that writes the decompressed stream of a compressed tar or file.
    pub fn decompressor(self, path: &Path, volumes: bool) -> Option<Command> {
        if matches!(self, Self::TarLz4 | Self::Lz4) {
            let mut command = Command::new("lz4");
            command.args(["-d", "-c", "-q"]).arg(path);
            return Some(command);
        }
        let mut command = Command::new("7zz");
        command
            .args(["x", "-so", "-bso0", "-bse2", "-bd"])
            .arg(self.type_switch(volumes)?)
            .arg("--")
            .arg(path);
        Some(command)
    }
}

#[derive(Debug, Serialize)]
pub struct Source {
    pub archive: PathBuf,
    pub revision: String,
    pub volumes: Vec<Volume>,
    pub format: Format,
    /// The name follows a volume scheme, so sibling files can change the volume set.
    pub multipart: bool,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let (volumes, multipart) = volumes(path)?;
        let archive = volumes[0].path.clone();
        // A spanned ZIP opens from its last segment, which starts mid-stream.
        let spanned = multipart
            && archive
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));
        let format = if spanned {
            Format::Zip
        } else {
            detect(&archive, multipart)?
        };
        Ok(Self {
            archive,
            revision: revision(&volumes),
            volumes,
            format,
            multipart,
        })
    }

    /// Checks the volumes again. Their format cannot change while they stay the same.
    pub fn unchanged(&self) -> Result<()> {
        if revision(&volumes(&self.archive)?.0) != self.revision {
            return Err(Error::new(
                "changed",
                "Archive volumes changed; reopen the archive",
            ));
        }
        Ok(())
    }

    pub fn check_destination(&self, destination: &Path) -> Result<()> {
        match fs::metadata(destination) {
            Ok(meta)
                if self
                    .volumes
                    .iter()
                    .any(|v| v.device == meta.dev() && v.inode == meta.ino()) =>
            {
                Err(Error::new(
                    "read_only",
                    "Cannot replace a source archive volume",
                ))
            }
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

fn revision(volumes: &[Volume]) -> String {
    let mut hasher = DefaultHasher::new();
    volumes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The volumes of the set `path` belongs to, first volume first, and whether its name
/// follows a volume scheme.
fn volumes(path: &Path) -> Result<(Vec<Volume>, bool)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize()?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::new("unsupported", "Archive filename must be valid UTF-8"))?;
    let family = match (Family::new(name), spanned(&parent, name)) {
        (Family::Single(_) | Family::Old(_), Some(stem)) => Family::Spanned(stem),
        (family, _) => family,
    };
    let (mut parts, mut beyond) = (BTreeMap::new(), false);
    if matches!(family, Family::Single(_)) {
        parts.insert(0, parent.join(name));
    } else {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            let Some(index) = family.index(filename) else {
                continue;
            };
            // The numbered part after the last a set may have: 7zz would read it after a
            // full set, and beside a smaller set it is an unrelated file.
            if index == MAX_VOLUMES && matches!(family, Family::Split(_)) {
                beyond = true;
                continue;
            }
            if parts.insert(index, entry.path()).is_some() {
                return Err(Error::new("volume", "Archive volume names are ambiguous"));
            }
            if parts.len() > MAX_VOLUMES {
                return Err(Error::new("limit", "Archive has too many volumes"));
            }
        }
    }
    if beyond && parts.len() == MAX_VOLUMES {
        return Err(Error::new("limit", "Archive has too many volumes"));
    }
    if parts.is_empty() {
        return Err(Error::new(
            "volume",
            "Archive or its first volume is missing",
        ));
    }
    // A numbered name without its set's first volume, such as notes.2024 or game.x86, is
    // not taken for a volume. Asking for a first volume that is missing still is.
    if !parts.contains_key(&0)
        && family.index(name) != Some(0)
        && matches!(family, Family::Split(_) | Family::Old(_))
    {
        return Err(not_archive());
    }
    let mut volumes = Vec::new();
    for (expected, (actual, path)) in parts.into_iter().enumerate() {
        if expected != actual {
            return Err(Error::new(
                "volume",
                "An archive volume is missing; keep all parts beside the first volume",
            ));
        }
        let path = path.canonicalize()?;
        let meta = fs::metadata(&path)?;
        if !meta.is_file() {
            return Err(Error::new(
                "volume",
                "An archive volume is not a regular file",
            ));
        }
        volumes.push(Volume {
            path,
            size: meta.len(),
            device: meta.dev(),
            inode: meta.ino(),
            modified_seconds: meta.mtime(),
            modified_nanoseconds: meta.mtime_nsec(),
            changed_seconds: meta.ctime(),
            changed_nanoseconds: meta.ctime_nsec(),
        });
    }
    Ok((volumes, !matches!(family, Family::Single(_))))
}

fn not_archive() -> Error {
    Error::new("not_archive", "Not an archive zippity can open")
}

fn detect(archive: &Path, volumes: bool) -> Result<Format> {
    let mut file = fs::File::open(archive)?;
    let mut magic = Vec::new();
    Read::by_ref(&mut file).take(4096).read_to_end(&mut magic)?;
    let magic = magic.as_slice();
    let mut name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    // A volume number says nothing about what the volumes hold.
    if volumes
        && let Some((stem, number)) = name.rsplit_once('.')
        && number.bytes().all(|b| b.is_ascii_digit())
    {
        name = stem.to_owned();
    }
    // Each compression holds either a tar or one file. lzma has no signature.
    let compressed = if magic.starts_with(b"\x1f\x8b") {
        Some((Format::TarGzip, Format::Gzip))
    } else if magic.starts_with(b"BZh") {
        Some((Format::TarBzip2, Format::Bzip2))
    } else if magic.starts_with(b"\xfd7zXZ\0") {
        Some((Format::TarXz, Format::Xz))
    } else if magic.starts_with(b"\x28\xb5\x2f\xfd") {
        Some((Format::TarZstd, Format::Zstd))
    } else if magic.starts_with(b"\x1f\x9d") {
        Some((Format::TarZ, Format::Z))
    } else if magic.starts_with(b"\x04\x22\x4d\x18") {
        Some((Format::TarLz4, Format::Lz4))
    } else if name.ends_with(".lzma") {
        Some((Format::TarLzma, Format::Lzma))
    } else {
        None
    };
    let format = if magic.starts_with(b"PK\x03\x04")
        || magic.starts_with(b"PK\x05\x06")
        || magic.starts_with(b"PK\x07\x08")
    {
        Format::Zip
    } else if magic.starts_with(b"Rar!\x1a\x07\x00") || magic.starts_with(b"Rar!\x1a\x07\x01\x00") {
        Format::Rar
    } else if magic.starts_with(b"7z\xbc\xaf\x27\x1c") {
        Format::SevenZip
    } else if let Some((tar, single)) = compressed {
        if volumes && single == Format::Lz4 {
            return Err(Error::new("unsupported", "Split lz4 files are unsupported"));
        }
        // Nor does a number after the name, as on a dated copy, change what a compressed
        // file holds. Other formats go by content before any such name is asked.
        let undated = match name.rsplit_once('.') {
            Some((stem, number))
                if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
            {
                stem
            }
            _ => name.as_str(),
        };
        if holds_tar(undated, archive, single, volumes)? {
            tar
        } else {
            single
        }
    } else if tar_header(magic) || name.ends_with(".tar") {
        Format::Tar
    } else if is_iso(&mut file)? {
        Format::Iso
    } else if let Some(format) = recognized(magic, &tail(&mut file)?) {
        format
    } else if name.ends_with(".sea") {
        // A self-extracting StuffIt archive has no signature of its own; unar decides.
        Format::Unar
    } else {
        return Err(not_archive());
    };
    Ok(format)
}

/// Whether a compressed stream holds a tar. The name decides when it says either way;
/// otherwise the first decompressed block does.
fn holds_tar(name: &str, path: &Path, single: Format, volumes: bool) -> Result<bool> {
    if [".tgz", ".tbz", ".tbz2", ".txz", ".tzst", ".taz"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return Ok(true);
    }
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    if let Some((_, inner)) = stem.rsplit_once('.') {
        return Ok(inner == "tar");
    }
    let mut command = single
        .decompressor(path, volumes)
        .expect("compressed format");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::process::start(&mut command)?;
    let mut stdout = child.0.stdout.take().expect("piped stdout");
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut head = Vec::new();
        let _ = Read::by_ref(&mut stdout).take(512).read_to_end(&mut head);
        let _ = send.send(head);
    });
    // A stalled decompressor must not hold the open. Dropping the child stops it, which
    // also ends the read; the rest of the stream is not needed.
    let head = receive
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap_or_default();
    drop(child);
    Ok(tar_header(&head))
}

/// Whether `block` starts with a tar header: the ustar mark, or a matching checksum, which
/// is all that headers older than POSIX carry.
fn tar_header(block: &[u8]) -> bool {
    let Some(header) = block.get(..512) else {
        return false;
    };
    if &header[257..262] == b"ustar" {
        return true;
    }
    let field = std::str::from_utf8(&header[148..156]).unwrap_or_default();
    let Ok(expected) = u32::from_str_radix(field.trim_matches([' ', '\0']), 8) else {
        return false;
    };
    // The checksum counts its own field as spaces; older writers summed signed bytes.
    let (mut unsigned, mut signed) = (0u32, 0i32);
    for (i, &byte) in header.iter().enumerate() {
        let byte = if (148..156).contains(&i) { b' ' } else { byte };
        unsigned += u32::from(byte);
        signed += i32::from(byte as i8);
    }
    header[0] != 0 && (expected == unsigned || i64::from(expected) == i64::from(signed))
}

fn is_iso(file: &mut fs::File) -> Result<bool> {
    // Bound recognition to the sectors before the usual UDF anchor at sector 256.
    for sector in 16..256 {
        file.seek(SeekFrom::Start(sector * 2048))?;
        let mut signature = [0; 8];
        match file.read_exact(&mut signature) {
            Ok(()) => {
                if (signature[1..6] == *b"CD001" && signature[6] == 1)
                    || signature == *b"\0BEA01\x01\0"
                {
                    return Ok(true);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(err) => return Err(err.into()),
        }
    }
    Ok(false)
}

/// The last 512 bytes, where disk image trailers sit.
fn tail(file: &mut fs::File) -> Result<Vec<u8>> {
    let length = file.metadata()?.len();
    let mut bytes = Vec::new();
    if length >= 512 {
        file.seek(SeekFrom::Start(length - 512))?;
        Read::by_ref(file).take(512).read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

/// Formats recognized by the signatures 7zz checks for them.
fn recognized(head: &[u8], tail: &[u8]) -> Option<Format> {
    let at = |offset: usize, signature: &[u8]| {
        head.get(offset..offset + signature.len()) == Some(signature)
    };
    if at(0, b"!<arch>\n") {
        return Some(Format::Ar);
    }
    // The signatures XADMaster's parsers check.
    if (at(0, b"S") && at(10, b"rLau")) // classic StuffIt
        || at(0, b"StuffIt (c)1997-") // StuffIt 5
        || at(0, b"StuffIt!")
        || at(0, b"StuffIt?") // StuffIt X
        || at(0, b"LZX")
    {
        return Some(Format::Unar);
    }
    let found = (at(2, b"-l") && head.get(6) == Some(&b'-')) // lzh, lha
        || at(0, b"\x60\xea") // arj
        || at(0, b"MSCF\0\0\0\0") // cab
        || at(0, b"ITSF\x03\0\0\0\x60\0\0\0") // chm
        || at(0, b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") // msi and other compound files
        || at(0, b"07070") // cpio with portable headers
        || at(0, b"\xc7\x71")
        || at(0, b"\x71\xc7") // cpio with binary headers
        || at(16, b"Compressed ROMFS") // cramfs
        || at(0, b"xar!") // xar, pkg
        || at(0, b"\xed\xab\xee\xdb") // rpm
        || at(0, b"MSWIM\0\0\0") // wim
        || [b"hsqs", b"sqsh", b"shsq", b"qshs"].iter().any(|s| at(0, *s)) // squashfs
        || at(0, b"QFI\xfb") // qcow
        || at(64, b"\x7f\x10\xda\xbe") // vdi
        || at(0, b"conectix")
        || tail.starts_with(b"conectix") // vhd, dynamic or fixed
        || at(0, b"vhdxfile")
        || at(0, b"KDMV") // vmdk
        || tail.starts_with(b"koly") // dmg
        || at(0, b":\xff\x26\xed") // Android sparse image
        || at(512, b"EFI PART") // GPT disk image
        || at(32, b"NXSB") // apfs
        || at(1024, b"H+\0\x04")
        || at(1024, b"HX\0\x05") // hfs+
        || at(1080, b"\x53\xef") // ext2, ext3, ext4
        || at(3, b"NTFS    ")
        || fat(head)
        || at(0, b"MZ"); // exe: PE files, installers, self-extracting archives
    found.then_some(Format::Other)
}

/// A FAT boot sector: the 55 AA mark alone also ends every partition table, so the BIOS
/// parameter block must be plausible too.
fn fat(head: &[u8]) -> bool {
    let Some(sector) = head.get(..512) else {
        return false;
    };
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
    let reserved = u16::from_le_bytes([sector[14], sector[15]]);
    sector[510..] == [0x55, 0xaa]
        && matches!(sector[0], 0xeb | 0xe9)
        && matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
        && sector[13].is_power_of_two()
        && reserved > 0
        && matches!(sector[16], 1 | 2)
}

/// The stem of the spanned ZIP set that `name` belongs to: `stem.zip` and `stem.zNN`
/// beside a `stem.z01` that starts with the spanning signature Info-ZIP writes.
fn spanned<'a>(parent: &Path, name: &'a str) -> Option<&'a str> {
    let (stem, extension) = name.rsplit_once('.')?;
    // Bytes, not characters: the extension may start with any character.
    let segment = match extension.as_bytes() {
        [b'z' | b'Z', digits @ ..] => digits.len() >= 2 && digits.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if !segment && !extension.eq_ignore_ascii_case("zip") {
        return None;
    }
    // Segments may differ in case, as the member scan allows.
    let sibling = |extensions: [&str; 2]| {
        extensions
            .into_iter()
            .map(|extension| parent.join(format!("{stem}.{extension}")))
            .find(|path| path.is_file())
    };
    let mut magic = [0; 4];
    fs::File::open(sibling(["z01", "Z01"])?)
        .ok()?
        .read_exact(&mut magic)
        .ok()?;
    (magic == *b"PK\x07\x08" && sibling(["zip", "ZIP"]).is_some()).then_some(stem)
}

/// The most volumes a set may have.
const MAX_VOLUMES: usize = 10_000;

/// The number that a numbered volume's extension spells.
fn volume_number(digits: &str) -> Option<usize> {
    let number = digits.parse().ok()?;
    digits.bytes().all(|b| b.is_ascii_digit()).then_some(number)
}

enum Family<'a> {
    Single(&'a str),
    Part(&'a str),
    Old(&'a str),
    Split(&'a str),
    /// Info-ZIP segments: `stem.zip` is the last and opens the set, `stem.z01` is the first.
    Spanned(&'a str),
}

impl<'a> Family<'a> {
    fn new(name: &'a str) -> Self {
        let lower = name.to_ascii_lowercase();
        if let Some(stem) = lower.strip_suffix(".rar") {
            if let Some((prefix, digits)) = stem.rsplit_once(".part")
                && !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
            {
                return Self::Part(&name[..prefix.len()]);
            }
            return Self::Old(&name[..stem.len()]);
        }
        if let Some((stem, extension)) = name.rsplit_once('.') {
            // A set needs every volume from the first, so a number above the most a set may
            // have, as in backup.zip.20241110, is part of the name.
            if extension.len() >= 3 && volume_number(extension).is_some_and(|n| n <= MAX_VOLUMES) {
                return Self::Split(stem);
            }
            let bytes = extension.as_bytes();
            if bytes.len() == 3
                && matches!(bytes[0].to_ascii_lowercase(), b'r'..=b'z')
                && bytes[1..].iter().all(u8::is_ascii_digit)
            {
                return Self::Old(stem);
            }
        }
        Self::Single(name)
    }

    fn index(&self, name: &str) -> Option<usize> {
        match self {
            Self::Single(expected) => (name == *expected).then_some(0),
            Self::Spanned(stem) => {
                let extension = name.strip_prefix(stem)?.strip_prefix('.')?;
                if extension.eq_ignore_ascii_case("zip") {
                    return Some(0);
                }
                let digits = extension
                    .strip_prefix('z')
                    .or_else(|| extension.strip_prefix('Z'))?;
                if digits.len() < 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                digits.parse::<usize>().ok().filter(|&number| number > 0)
            }
            Self::Split(prefix) => {
                let digits = name.strip_prefix(prefix)?.strip_prefix('.')?;
                if digits.len() < 3 {
                    return None;
                }
                // One more than a set may have still counts: 7zz would read that part.
                volume_number(digits)
                    .filter(|&number| number <= MAX_VOLUMES + 1)?
                    .checked_sub(1)
            }
            Self::Part(prefix) => {
                let rest = name.strip_prefix(prefix)?.to_ascii_lowercase();
                let digits = rest.strip_prefix(".part")?.strip_suffix(".rar")?;
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                digits.parse::<usize>().ok()?.checked_sub(1)
            }
            Self::Old(stem) => {
                let ext = name
                    .strip_prefix(stem)?
                    .strip_prefix('.')?
                    .to_ascii_lowercase();
                if ext == "rar" {
                    return Some(0);
                }
                let bytes = ext.as_bytes();
                if bytes.len() != 3
                    || !matches!(bytes[0], b'r'..=b'z')
                    || !bytes[1..].iter().all(u8::is_ascii_digit)
                {
                    return None;
                }
                Some(1 + usize::from(bytes[0] - b'r') * 100 + ext[1..].parse::<usize>().ok()?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_disc_images_without_filename_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("cached-member");
        for signature in [*b"\x01CD001\x01\0", *b"\0BEA01\x01\0"] {
            let mut bytes = vec![0; 16 * 2048];
            bytes.extend(signature);
            fs::write(&archive, bytes).unwrap();
            assert_eq!(Source::open(&archive).unwrap().format, Format::Iso);
        }
        for sector in [17, 255, 256] {
            let mut bytes = vec![0; sector * 2048];
            bytes[16 * 2048..16 * 2048 + 8].copy_from_slice(b"\0CDW02\x01\0");
            bytes.extend(b"\0BEA01\x01\0");
            fs::write(&archive, bytes).unwrap();
            assert_eq!(Source::open(&archive).is_ok(), sector < 256);
        }
        fs::write(&archive, b"not a disc image").unwrap();
        assert!(Source::open(&archive).is_err());
        let mut bytes = vec![0; 16 * 2048];
        bytes.extend(b"\x01CD001\x02\0");
        fs::write(&archive, bytes).unwrap();
        assert!(Source::open(&archive).is_err());
    }

    #[test]
    fn recognizes_formats_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("no-extension");
        let detect = |bytes: Vec<u8>| {
            fs::write(&archive, bytes).unwrap();
            Source::open(&archive).map(|source| source.format)
        };
        let at = |offset: usize, signature: &[u8], length: usize| {
            let mut bytes = vec![0; length.max(offset + signature.len())];
            bytes[offset..offset + signature.len()].copy_from_slice(signature);
            bytes
        };
        assert_eq!(detect(at(0, b"!<arch>\n", 64)).unwrap(), Format::Ar);
        let mut stuffit = at(0, b"SIT!", 64);
        stuffit[10..14].copy_from_slice(b"rLau");
        assert_eq!(detect(stuffit).unwrap(), Format::Unar);
        for signature in [&b"StuffIt (c)1997-"[..], b"StuffIt!", b"StuffIt?", b"LZX"] {
            assert_eq!(detect(at(0, signature, 64)).unwrap(), Format::Unar);
        }
        for (offset, signature) in [
            (0, &b"MSCF\0\0\0\0"[..]),
            (2, b"-lh0-"),
            (0, b"0707010000"),
            (0, b"xar!\0\x1c"),
            (0, b"\xed\xab\xee\xdb"),
            (0, b"MSWIM\0\0\0"),
            (0, b"hsqs"),
            (0, b"QFI\xfb"),
            (64, b"\x7f\x10\xda\xbe"),
            (512, b"EFI PART"),
            (32, b"NXSB"),
            (1024, b"H+\0\x04"),
            (1080, b"\x53\xef"),
            (3, b"NTFS    "),
            (0, b"MZ"),
        ] {
            assert_eq!(
                detect(at(offset, signature, 2048)).unwrap(),
                Format::Other,
                "{signature:?}"
            );
        }
        // Disk image trailers sit in the last 512 bytes.
        let mut dmg = vec![0; 4096];
        dmg[4096 - 512..4096 - 508].copy_from_slice(b"koly");
        assert_eq!(detect(dmg).unwrap(), Format::Other);
        // A FAT boot sector needs a plausible parameter block, not just the 55 AA mark.
        let mut fat = vec![0; 2048];
        fat[0] = 0xeb;
        fat[11..13].copy_from_slice(&512u16.to_le_bytes());
        fat[13] = 4;
        fat[14] = 1;
        fat[16] = 2;
        fat[510..512].copy_from_slice(&[0x55, 0xaa]);
        assert_eq!(detect(fat.clone()).unwrap(), Format::Other);
        fat[0] = 0xfa;
        assert_eq!(detect(fat).unwrap_err().kind, "not_archive");
        assert_eq!(
            detect(b"plain text that is not an archive".to_vec())
                .unwrap_err()
                .kind,
            "not_archive"
        );
    }

    #[test]
    fn compressed_names_decide_between_tar_and_one_file() {
        // These names decide without reading the file.
        let nowhere = Path::new("/nonexistent/zippity");
        for name in [
            "backup.tar.gz",
            "backup.tgz",
            "x.tar.z",
            "x.taz",
            "x.tzst",
            "x.tar.lzma",
        ] {
            assert!(
                holds_tar(name, nowhere, Format::Gzip, false).unwrap(),
                "{name}"
            );
        }
        for name in ["notes.txt.gz", "v1.2.gz", ".bashrc.gz", "data.cpio.xz"] {
            assert!(
                !holds_tar(name, nowhere, Format::Gzip, false).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn tar_headers_count_with_or_without_the_ustar_mark() {
        let mut header = vec![0; 512];
        header[..8].copy_from_slice(b"file.txt");
        header[100..107].copy_from_slice(b"0000644");
        let sum: u32 = header.iter().map(|&b| u32::from(b)).sum::<u32>() + 8 * u32::from(b' ');
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        assert!(
            tar_header(&header),
            "a pre-POSIX header with a valid checksum"
        );
        header[0] = b'g';
        assert!(!tar_header(&header), "a changed header fails its checksum");
        header[257..262].copy_from_slice(b"ustar");
        assert!(tar_header(&header));
        assert!(!tar_header(&header[..511]));
    }

    #[test]
    fn single_file_lookup_does_not_need_directory_read_permission() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("sample.zip");
        fs::write(&archive, b"PK\x03\x04fixture").unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o300)).unwrap();
        let opened = Source::open(&archive);
        let checked = opened.as_ref().map(|source| source.unchanged());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        checked.unwrap().unwrap();
        let source = opened.unwrap();
        assert!(!source.multipart);
        fs::write(dir.path().join("unrelated"), b"other").unwrap();
        source.unchanged().unwrap();
        let replacement = dir.path().join("replacement.zip");
        fs::write(&replacement, b"PK\x03\x04fixture").unwrap();
        fs::rename(replacement, &archive).unwrap();
        assert!(source.unchanged().is_err());
        fs::remove_file(&archive).unwrap();
        assert!(Source::open(&archive).is_err());
    }

    #[test]
    fn numbered_names_without_a_first_volume_are_not_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let kind = |name: &str| Source::open(&dir.path().join(name)).unwrap_err().kind;
        for name in ["notes.2024", "game.x86", "snapshot.z80", "backup.7z.002"] {
            fs::write(dir.path().join(name), b"plain text").unwrap();
            assert_eq!(kind(name), "not_archive", "{name}");
        }
        // Asking for the first volume of a set that lacks it still says so.
        assert_eq!(kind("backup.7z.001"), "volume");
        // A number no set can reach is part of the name: content decides, siblings do not.
        for name in ["dated.zip.20241110", "set.zip.001", "set.zip.20241110"] {
            fs::write(dir.path().join(name), b"PK\x03\x04fixture").unwrap();
        }
        let dated = Source::open(&dir.path().join("dated.zip.20241110")).unwrap();
        assert!(dated.format == Format::Zip && !dated.multipart);
        // The name before the number still says that a compressed file holds a tar.
        fs::write(dir.path().join("dated.tar.gz.20241110"), b"\x1f\x8b\x08").unwrap();
        let tar = Source::open(&dir.path().join("dated.tar.gz.20241110")).unwrap();
        assert_eq!(tar.format, Format::TarGzip);
        // Content still comes before a name that only a number hides.
        let mut disc = vec![0; 16 * 2048];
        disc.extend(b"\x01CD001\x01\0");
        fs::write(dir.path().join("disc.tar.1"), disc).unwrap();
        let disc = Source::open(&dir.path().join("disc.tar.1")).unwrap();
        assert_eq!(disc.format, Format::Iso);
        for stray in ["set.zip.10001", "set.zip.010001"] {
            fs::write(dir.path().join(stray), b"unrelated").unwrap();
        }
        // A spanned ZIP still counts every segment it finds.
        fs::write(dir.path().join("span.z01"), b"PK\x07\x08first").unwrap();
        fs::write(dir.path().join("span.zip"), b"tail").unwrap();
        fs::write(dir.path().join("span.z10000"), b"far").unwrap();
        assert_eq!(kind("span.zip"), "volume");
        let parts = |name: &str| Source::open(&dir.path().join(name)).unwrap().volumes.len();
        assert_eq!(parts("set.zip.001"), 1);
        // One part more than a set may have is refused, since 7zz would read them all.
        for number in 2..=MAX_VOLUMES {
            fs::write(dir.path().join(format!("set.zip.{number:03}")), b"").unwrap();
        }
        assert_eq!(kind("set.zip.001"), "limit");
    }

    #[test]
    fn numeric_volumes_detect_new_and_removed_parts() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("sample.zip.001");
        let second = dir.path().join("sample.zip.002");
        fs::write(&first, b"PK\x03\x04fixture").unwrap();
        let source = Source::open(&first).unwrap();
        fs::write(&second, b"second").unwrap();
        assert!(source.unchanged().is_err());
        let source = Source::open(&second).unwrap();
        assert_eq!(source.volumes.len(), 2);
        source.unchanged().unwrap();
        fs::remove_file(&second).unwrap();
        assert!(source.unchanged().is_err());
    }

    #[test]
    fn detects_both_volume_naming_schemes() {
        let family = Family::new("example.part003.rar");
        assert_eq!(family.index("example.part001.rar"), Some(0));
        assert_eq!(family.index("example.part003.rar"), Some(2));
        assert_eq!(family.index("unrelated.part001.rar"), None);
        let family = Family::new("example.r01");
        assert_eq!(family.index("example.rar"), Some(0));
        assert_eq!(family.index("example.r00"), Some(1));
        assert_eq!(family.index("example.s00"), Some(101));
        assert_eq!(family.index("example.r+1"), None);
    }

    #[test]
    fn spanned_zip_segments_form_one_set_led_by_the_zip() {
        let dir = tempfile::tempdir().unwrap();
        let last = dir.path().join("set.zip");
        fs::write(dir.path().join("set.z01"), b"PK\x07\x08first").unwrap();
        fs::write(dir.path().join("set.z02"), b"middle").unwrap();
        fs::write(&last, b"\x66\x7c tail of the set").unwrap();
        fs::write(dir.path().join("set.rar"), b"unrelated").unwrap();
        for opened in ["set.zip", "set.z01", "set.z02"] {
            let source = Source::open(&dir.path().join(opened)).unwrap();
            assert_eq!(source.archive, last.canonicalize().unwrap(), "{opened}");
            assert!(source.format == Format::Zip && source.multipart && source.volumes.len() == 3);
        }
        let source = Source::open(&last).unwrap();
        fs::write(dir.path().join("set.z03"), b"added").unwrap();
        assert!(source.unchanged().is_err());
        // A segment named in another case still finds the rest of its set.
        fs::write(dir.path().join("set.Z04"), b"upper case").unwrap();
        let mixed = Source::open(&dir.path().join("set.Z04")).unwrap();
        assert!(mixed.archive == last.canonicalize().unwrap() && mixed.volumes.len() == 5);
        // Without a first segment that carries the spanning signature, a .zip stands alone.
        let alone = tempfile::tempdir().unwrap();
        let only = alone.path().join("only.zip");
        fs::write(&only, b"PK\x03\x04fixture").unwrap();
        assert!(!Source::open(&only).unwrap().multipart);
        fs::write(alone.path().join("only.z01"), b"unrelated data").unwrap();
        assert!(!Source::open(&only).unwrap().multipart);
        // Any extension is safe to inspect, whatever its first character.
        let unicode = alone.path().join("photo.\u{e9}t\u{e9}");
        fs::write(&unicode, b"plain").unwrap();
        assert_eq!(Source::open(&unicode).unwrap_err().kind, "not_archive");
    }

    #[test]
    fn rejects_missing_parts_and_tracks_every_volume() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("example.part01.rar");
        let second = dir.path().join("example.part02.rar");
        let third = dir.path().join("example.part03.rar");
        fs::write(&first, b"Rar!\x1a\x07\x00\x00").unwrap();
        fs::write(&third, b"third").unwrap();
        assert!(Source::open(&first).is_err());
        fs::write(&second, b"second").unwrap();
        let source = Source::open(&third).unwrap();
        assert_eq!(source.archive, first.canonicalize().unwrap());
        assert_eq!(source.volumes.len(), 3);
        assert!(source.multipart);
        source.unchanged().unwrap();
        fs::write(&second, b"changed second").unwrap();
        assert!(source.unchanged().is_err());
        assert!(source.check_destination(&first).is_err());
    }
}
