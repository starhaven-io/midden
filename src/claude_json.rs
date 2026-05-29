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
    Ok(())
}
