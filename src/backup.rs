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
    let backup = backup_path(path, &stamp);
    std::fs::copy(path, &backup)
        .with_context(|| format!("backup {} -> {}", path.display(), backup.display()))?;
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
