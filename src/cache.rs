use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use crate::error::{Error, Result};

const MARKER: &str = ".zippity-cache";
const CONTENTS: &[u8] = b"zippity member cache v1\n";

pub fn create() -> Result<PathBuf> {
    let directory = tempfile::Builder::new().prefix("zippity-").tempdir()?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
    if fs::metadata(directory.path())?.permissions().mode() & 0o077 != 0 {
        return Err(Error::new(
            "cache",
            "Temporary filesystem cannot provide a private cache directory",
        ));
    }
    fs::write(directory.path().join(MARKER), CONTENTS)?;
    let path = directory.path().canonicalize()?;
    let _ = directory.keep();
    Ok(path)
}

pub fn clean(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    }
    owned(path)?;
    fs::remove_dir_all(path)?;
    Ok(())
}

/// Whether `path` is a private member cache that `create` made.
pub fn owned(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let temporary = std::env::temp_dir().canonicalize()?;
    let parent = path.parent().map(Path::canonicalize).transpose()?;
    if !metadata.is_dir()
        || metadata.permissions().mode() & 0o077 != 0
        || parent.as_deref() != Some(temporary.as_path())
        || !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("zippity-"))
        || !fs::symlink_metadata(path.join(MARKER)).is_ok_and(|m| m.is_file())
        || fs::read(path.join(MARKER))? != CONTENTS
    {
        return Err(Error::new("cache", "Not a private Zippity cache directory"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_private_and_cleanup_is_scoped() {
        let path = create().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(path.join("preview"), "content").unwrap();
        clean(&path).unwrap();
        assert!(!path.exists());
        clean(&path).unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        assert!(clean(unrelated.path()).is_err());
        assert!(unrelated.path().exists());
    }

    #[test]
    fn cleanup_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("zippity-link");
        let target = create().unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(clean(&link).is_err());
        assert!(target.exists());
        clean(&target).unwrap();
    }
}
