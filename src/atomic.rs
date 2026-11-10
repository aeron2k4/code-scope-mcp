//! Replacing a file so that an interrupted write never leaves it half written.

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace `path` with `contents` through a temporary file in the same
/// directory and a rename, creating it if it does not exist. A symlink stays a
/// symlink, since its target is what gets replaced. The original permissions
/// are kept, and a read-only file is refused since a rename would replace it
/// regardless.
///
/// A file with other hard links is written in place instead: a rename would
/// leave those names with the old contents. That write is not atomic.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(err) if err.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => return Err(err),
    };
    let meta = match fs::metadata(&target) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    let permissions = meta.as_ref().map(fs::Metadata::permissions);
    if permissions.as_ref().is_some_and(|p| p.readonly()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the file is read-only",
        ));
    }
    if meta.as_ref().is_some_and(has_other_links) {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&target)?;
        file.write_all(contents.as_bytes())?;
        return file.sync_all();
    }
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.astlens-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::File::create_new(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&tmp, permissions)?;
        }
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    } else {
        sync_dir(dir);
    }
    result
}

/// Whether the file is known under more than one name.
fn has_other_links(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        meta.nlink() > 1
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Make the rename itself last through a power loss, where the platform lets
/// a directory be synced. The file is already written, so a failure here is
/// not one of the write.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn creates_a_missing_file_and_replaces_an_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_atomic(&path, "one").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one");
        write_atomic(&path, "two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(entries(dir.path()), 1, "no temporary file left");
    }

    #[test]
    fn refuses_a_read_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked");
        fs::write(&path, "old").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();

        let err = write_atomic(&path, "new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_other_names_of_a_hard_linked_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let other = dir.path().join("b.txt");
        fs::write(&path, "old and longer").unwrap();
        fs::hard_link(&path, &other).unwrap();

        write_atomic(&path, "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_to_string(&other).unwrap(), "new");
        assert_eq!(entries(dir.path()), 2, "no temporary file left");
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_a_symlink_and_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-settings.json");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, "new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
