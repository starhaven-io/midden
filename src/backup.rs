use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::macros::format_description;

/// Copy `path` to `<path>.bak-YYYYMMDD-HHMMSS` and return the backup path.
///
/// Uses local time if available, falling back to UTC if the local offset can't
/// be determined (some Linux distros, sandboxed environments).
pub fn timestamped_copy(path: &Path) -> Result<PathBuf> {
    let stamp = stamp_now();
    // The stamp has one-second resolution, so two mutations in the same second
    // (e.g. `prune --apply` then `doctor --fix`) would compute the same name and
    // the second copy would clobber the first backup. Bump a suffix until free.
    let mut backup = backup_path(path, &stamp);
    let mut n = 1;
    while backup.exists() {
        backup = backup_path(path, &format!("{stamp}-{n}"));
        n += 1;
    }
    std::fs::copy(path, &backup)
        .with_context(|| format!("backup {} -> {}", path.display(), backup.display()))?;
    // `fs::copy` mirrors the source's permissions — including an erroneously
    // broad mode. Backups of credential-bearing config exist only for the
    // owner to restore, so clamp to owner-only regardless of the source.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set mode on backup {}", backup.display()))?;
    }
    Ok(backup)
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
}
