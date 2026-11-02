//! macOS Finder tags, label colors, and custom icons. They live in extended attributes,
//! which 7-Zip does not read, so archives carry them the way Finder writes them: as
//! AppleDouble `._name` files under a top-level `__MACOSX` folder.

#[cfg(target_os = "macos")]
pub use mac::{apply, attributes, pack};

#[cfg(target_os = "macos")]
mod mac {
    use std::{
        ffi::{CString, c_char, c_int, c_void},
        fs,
        io::{self, Write},
        os::unix::ffi::OsStrExt,
        path::Path,
    };

    use rustix::fs::XattrFlags;

    /// The attributes archives keep. Others, such as quarantine flags and download
    /// addresses, stay out.
    const KEPT: [&str; 3] = [
        "com.apple.metadata:_kMDItemUserTags",
        "com.apple.FinderInfo",
        "com.apple.ResourceFork",
    ];
    /// Larger attributes, such as a huge resource fork, stay out.
    const MAX_VALUE: usize = 16 * 1024 * 1024;
    const COPYFILE_XATTR: u32 = 1 << 2;
    const COPYFILE_PACK: u32 = 1 << 22;
    const COPYFILE_UNPACK: u32 = 1 << 23;

    unsafe extern "C" {
        fn copyfile(
            from: *const c_char,
            to: *const c_char,
            state: *mut c_void,
            flags: u32,
        ) -> c_int;
    }

    /// Converts between a file's attributes and an AppleDouble file, as Finder does.
    fn convert(from: &Path, to: &Path, direction: u32) -> io::Result<()> {
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: both paths are NUL-terminated and outlive the call, and a null state asks
        // copyfile for its defaults.
        let status = unsafe {
            copyfile(
                from.as_ptr(),
                to.as_ptr(),
                std::ptr::null_mut(),
                direction | COPYFILE_XATTR,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// One attribute of `path`, unless it is missing or too large.
    fn read(path: &Path, name: &str) -> Option<Vec<u8>> {
        let size = rustix::fs::getxattr(path, name, &mut [0u8; 0][..]).ok()?;
        if size > MAX_VALUE {
            return None;
        }
        let mut value = vec![0; size];
        let read = rustix::fs::getxattr(path, name, &mut value[..]).ok()?;
        value.truncate(read);
        Some(value)
    }

    /// The kept attributes of `path`, which must not be a link.
    pub fn attributes(path: &Path) -> Vec<(&'static str, Vec<u8>)> {
        KEPT.into_iter()
            .filter_map(|name| Some((name, read(path, name)?)))
            .collect()
    }

    /// Writes a new AppleDouble file at `output` holding `attributes`.
    pub fn pack(attributes: &[(&'static str, Vec<u8>)], output: &Path) -> io::Result<()> {
        let scratch = tempfile::NamedTempFile::new()?;
        for (name, value) in attributes {
            rustix::fs::fsetxattr(scratch.as_file(), *name, value, XattrFlags::empty())?;
        }
        convert(scratch.path(), output, COPYFILE_PACK)
    }

    /// Gives the open `target` the kept attributes in AppleDouble `data`, as far as its file
    /// system allows: Finder metadata never stops an extraction.
    pub fn apply(data: &[u8], target: &fs::File) {
        let _ = unpack(data, target);
    }

    fn unpack(data: &[u8], target: &fs::File) -> io::Result<()> {
        let mut packed = tempfile::NamedTempFile::new()?;
        packed.write_all(data)?;
        packed.flush()?;
        let unpacked = tempfile::NamedTempFile::new()?;
        convert(packed.path(), unpacked.path(), COPYFILE_UNPACK)?;
        for (name, value) in attributes(unpacked.path()) {
            // A file system that refuses one attribute, as a folder refuses a resource
            // fork, still takes the others.
            let _ = rustix::fs::fsetxattr(target, name, &value, XattrFlags::empty());
        }
        Ok(())
    }
}

/// Only macOS has Finder metadata.
#[cfg(not(target_os = "macos"))]
pub fn attributes(_path: &std::path::Path) -> Vec<(&'static str, Vec<u8>)> {
    Vec::new()
}

#[cfg(not(target_os = "macos"))]
pub fn pack(
    _attributes: &[(&'static str, Vec<u8>)],
    _output: &std::path::Path,
) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn apply(_data: &[u8], _target: &std::fs::File) {}
