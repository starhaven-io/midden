use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
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
