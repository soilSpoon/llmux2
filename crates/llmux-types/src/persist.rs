//! Atomic writes for secret files: tempfile in the target directory, 0o600,
//! fsync, then rename and fsync the directory.

use std::{
    fs,
    io::{self, Write},
    path::Path,
};

/// A private (0600 on Unix) tempfile in `path`'s directory, synced to disk but
/// not yet persisted. The caller chooses `persist` or `persist_noclobber`.
pub fn private_tempfile(path: &Path, bytes: &[u8]) -> io::Result<tempfile::NamedTempFile> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    Ok(file)
}

/// fsync a directory after persisting files into it.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Replace `path` atomically with `bytes`, leaving no partial file on failure.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file = private_tempfile(path, bytes)?;
    let parent = file
        .path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_owned();
    file.persist(path).map_err(|error| error.error)?;
    sync_dir(&parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        fs::write(&path, "old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_failure_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing").join("secret");
        assert!(atomic_write(&missing, b"x").is_err());
        assert!(!missing.exists());
    }
}
