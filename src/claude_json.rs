use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::io::Write;
use std::path::Path;

/// Loaded `~/.claude.json` plus the raw text it was parsed from. The raw text
/// is kept so we can report size deltas without re-serializing twice.
pub struct ClaudeJson {
    pub raw: String,
    pub data: Value,
}

impl ClaudeJson {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let data: Value =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        Ok(Self { raw, data })
    }

    pub fn projects(&self) -> Option<&Map<String, Value>> {
        self.data.get("projects").and_then(Value::as_object)
    }

    pub fn projects_mut(&mut self) -> Option<&mut Map<String, Value>> {
        self.data.get_mut("projects").and_then(Value::as_object_mut)
    }
}

/// Render the JSON the same way Claude Code would: indent=2, preserving key
/// order. We rely on `serde_json`'s `preserve_order` feature so unrelated keys
/// keep their original positions.
pub fn render(data: &Value) -> Result<String> {
    serde_json::to_string_pretty(data).map_err(|e| anyhow!("serialize: {e}"))
}

/// The `projects.<dir>` entry whose key refers to `root`, if any. Claude Code
/// records the working directory as the shell reported it, which can differ
/// from the canonical form (macOS `/var` -> `/private/var`), so fall back to
/// comparing canonicalized keys when no exact match exists.
pub fn project_entry<'a>(data: &'a Value, root: &Path) -> Option<&'a Value> {
    let projects = data.get("projects")?.as_object()?;
    if let Some(v) = projects.get(root.to_string_lossy().as_ref()) {
        return Some(v);
    }
    projects
        .iter()
        .find_map(|(k, v)| (std::fs::canonicalize(k).ok()? == root).then_some(v))
}

/// Write `contents` to `path` atomically: stream to a temp sibling on the same
/// filesystem, flush it to disk, then rename over the target. A crash or
/// `ENOSPC` mid-write leaves either the old file or the new one intact — never
/// the truncated half-write a plain `fs::write` would produce on the central
/// `~/.claude.json`.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("claude.json");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    // Close the handle (end of scope) before renaming.
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create temp {}", tmp.display()))?;
        // The rename publishes the temp's permissions as the target's, and
        // `File::create` applies the umask (typically 0644) — which would
        // silently broaden a 0600 target holding OAuth tokens. Mirror the
        // target's current mode, owner-only for new files, before any content
        // is written.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = match std::fs::metadata(path) {
                Ok(m) => m.permissions().mode() & 0o7777,
                Err(_) => 0o600,
            };
            f.set_permissions(std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("set mode on temp {}", tmp.display()))?;
        }
        f.write_all(contents.as_bytes())
            .with_context(|| format!("write temp {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("sync temp {}", tmp.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ));
    }
    // The rename is durable only once the directory entry is synced; until
    // then a crash can roll back to the old file. Best-effort — not every
    // filesystem supports fsync on a directory handle.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_entry_matches_an_exact_key() {
        let data = json!({ "projects": { "/x/y": { "k": 1 } } });
        assert!(project_entry(&data, Path::new("/x/y")).is_some());
        assert!(project_entry(&data, Path::new("/x/z")).is_none());
    }

    #[test]
    fn project_entry_falls_back_to_canonical_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir(&a).unwrap();
        let root = a.canonicalize().unwrap();
        // A key recorded in non-canonical form (here via a `.` component) must
        // still resolve to the same entry.
        let noncanon = dir.path().join(".").join("a");
        let mut projects = Map::new();
        projects.insert(noncanon.to_string_lossy().into_owned(), json!({ "k": 1 }));
        let mut top = Map::new();
        top.insert("projects".into(), Value::Object(projects));
        let data = Value::Object(top);
        assert!(project_entry(&data, &root).is_some());
    }

    #[test]
    fn write_atomic_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_a_restrictive_target_mode() {
        // Regression: File::create + umask used to broaden a 0600 target to
        // 0644 on every rewrite.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{}").unwrap();
        chmod(&path, 0o600);
        write_atomic(&path, "{\"k\":1}").unwrap();
        assert_eq!(mode(&path), 0o600, "0600 target must stay 0600");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_a_broad_target_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{}").unwrap();
        chmod(&path, 0o644);
        write_atomic(&path, "{\"k\":1}").unwrap();
        assert_eq!(mode(&path), 0o644, "deliberate modes are kept, not clamped");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_creates_missing_targets_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.json");
        write_atomic(&path, "{}").unwrap();
        assert_eq!(mode(&path), 0o600, "this file class holds credentials");
    }
}
