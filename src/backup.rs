use anyhow::{Context, Result, bail};
use std::fs::OpenOptions;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::safe_io;

/// Copy `path` to `<path>.bak-YYYYMMDD-HHMMSS` and return the backup path.
///
/// Uses local time if available, falling back to UTC if the local offset can't
/// be determined (some Linux distros, sandboxed environments).
pub fn timestamped_copy(path: &Path) -> Result<PathBuf> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "refusing to back up through symlinked config {}; pass --config with the resolved target path",
            path.display()
        );
    }
    let stamp = stamp_now();
    let mut source = safe_io::open_regular(path, false)
        .with_context(|| format!("open backup source {}", path.display()))?;
    for n in 0..1000 {
        let suffix = if n == 0 {
            stamp.clone()
        } else {
            format!("{stamp}-{n}")
        };
        let backup = backup_path(path, &suffix);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut destination = match options.open(&backup) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create backup {}", backup.display()));
            }
        };

        let copy_result = (|| -> Result<()> {
            source
                .rewind()
                .with_context(|| format!("rewind backup source {}", path.display()))?;
            std::io::copy(&mut source, &mut destination)
                .with_context(|| format!("backup {} -> {}", path.display(), backup.display()))?;
            destination
                .flush()
                .with_context(|| format!("flush backup {}", backup.display()))?;
            destination
                .sync_all()
                .with_context(|| format!("sync backup {}", backup.display()))?;
            Ok(())
        })();
        if let Err(error) = copy_result {
            drop(destination);
            let _ = std::fs::remove_file(&backup);
            return Err(error);
        }
        drop(destination);
        #[cfg(unix)]
        if let Some(parent) = backup.parent()
            && let Ok(directory) = std::fs::File::open(parent)
        {
            let _ = directory.sync_all();
        }
        return Ok(backup);
    }
    anyhow::bail!(
        "create backup beside {}: exhausted collision retries",
        path.display()
    )
}

fn stamp_now() -> String {
    let fmt = format_description!("[year][month][day]-[hour][minute][second]");
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    now.format(&fmt).expect("format")
}

fn backup_path(path: &Path, stamp: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let new_name = format!("{file_name}.bak-{stamp}");
    path.with_file_name(new_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_appends_stamp() {
        let p = Path::new("/tmp/.claude.json");
        let b = backup_path(p, "20260101-120000");
        assert_eq!(b, Path::new("/tmp/.claude.json.bak-20260101-120000"));
    }

    #[test]
    fn timestamped_copy_does_not_clobber_an_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("config.json");
        std::fs::write(&src, "{}").unwrap();
        // Two back-to-back copies land in the same second; the second must get a
        // distinct, suffix-bumped name rather than overwriting the first.
        let first = timestamped_copy(&src).unwrap();
        let second = timestamped_copy(&src).unwrap();
        assert_ne!(first, second, "second backup reused the first name");
        assert!(first.exists() && second.exists(), "both backups survive");
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_owner_only_even_when_the_source_is_broad() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("config.json");
        std::fs::write(&src, "{\"token\":1}").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        let backup = timestamped_copy(&src).unwrap();

        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "backup must not inherit a broad source mode");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "{\"token\":1}");
    }

    #[test]
    fn timestamped_copy_creates_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("config.json");
        std::fs::write(&src, "{}").unwrap();
        let backup = timestamped_copy(&src).unwrap();
        assert!(backup.exists());
        assert_eq!(backup.parent(), src.parent());
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("config.json.bak-")
        );
    }

    #[cfg(unix)]
    #[test]
    fn timestamped_copy_never_follows_a_dangling_destination_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("config.json");
        let outside = dir.path().join("outside");
        std::fs::write(&src, "sensitive").unwrap();
        let planted = backup_path(&src, &stamp_now());
        std::os::unix::fs::symlink(&outside, &planted).unwrap();

        let backup = timestamped_copy(&src).unwrap();

        assert_ne!(backup, planted);
        assert!(
            !outside.exists(),
            "the symlink target must remain untouched"
        );
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "sensitive");
    }

    #[cfg(unix)]
    #[test]
    fn timestamped_copy_refuses_a_symlink_source() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let source = dir.path().join(".claude.json");
        std::fs::write(&target, "sensitive").unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();

        let error = timestamped_copy(&source).unwrap_err().to_string();
        assert!(error.contains("refusing to back up through symlinked config"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "sensitive");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
