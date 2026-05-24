// This module is included once per integration test binary; each binary only
// uses a subset of the helpers, so clippy will (correctly) flag the rest as
// unused per-binary. Silence that here rather than per-item.
#![allow(dead_code)]

use assert_cmd::Command;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A scratch directory holding a fixture `.claude.json` and a fake home.
///
/// Tests construct one with `Fixture::new()`, optionally add real directories
/// for project entries that should *not* be classified as orphans, and seed a
/// `projects` map plus arbitrary sibling keys.
pub struct Fixture {
    /// Project root (passed as the target directory to commands).
    pub root: TempDir,
    /// Separate fake `$HOME/.claude` used as the user scope. Must NOT live
    /// inside `root`, otherwise `<root>/.claude/settings.json` ends up
    /// shared between user-scope and project-scope, breaking resolution.
    home: TempDir,
    pub claude_home: PathBuf,
    pub config: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir-root");
        let home = tempfile::tempdir().expect("tempdir-home");
        let claude_home = home.path().join(".claude");
        std::fs::create_dir_all(&claude_home).unwrap();
        let config = home.path().join(".claude.json");
        Self {
            root,
            home,
            claude_home,
            config,
        }
    }

    /// Create a real subdirectory and return its absolute path string. Use for
    /// project entries that should be considered live.
    pub fn touch_dir(&self, rel: &str) -> String {
        let p = self.root.path().join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Write a fixture .claude.json from a `projects` map and extra top-level keys.
    pub fn write_config(&self, projects: Value, extras: Value) {
        let mut obj = serde_json::Map::new();
        // Order matters for the "unrelated keys preserved" assertion.
        if let Value::Object(map) = extras {
            for (k, v) in map {
                obj.insert(k, v);
            }
        }
        obj.insert("projects".into(), projects);
        let data = Value::Object(obj);
        std::fs::write(&self.config, serde_json::to_string_pretty(&data).unwrap()).unwrap();
    }

    pub fn read_config(&self) -> Value {
        let raw = std::fs::read_to_string(&self.config).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("midden").expect("binary");
        cmd.env_remove("NO_COLOR")
            .env("CLICOLOR", "0")
            .arg("--color")
            .arg("never")
            .arg("--config")
            .arg(&self.config)
            .arg("--claude-home")
            .arg(&self.claude_home);
        cmd
    }

    pub fn backup_paths(&self) -> Vec<PathBuf> {
        list_backups(&self.config)
    }
}

pub fn list_backups(config: &Path) -> Vec<PathBuf> {
    let dir = config.parent().unwrap();
    let prefix = format!("{}.bak-", config.file_name().unwrap().to_string_lossy());
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect()
}

/// Standard `projects` map: one live dir, two missing, one missing worktree.
pub fn standard_projects(live: &str) -> Value {
    json!({
        live: { "lastVisited": "2026-05-01T00:00:00Z" },
        "/no/such/dir": { "lastVisited": "2026-04-01T00:00:00Z" },
        "/also/gone": { "lastVisited": "2026-03-01T00:00:00Z" },
        "/Users/x/proj/.claude/worktrees/witty-curie/checkout": { "lastVisited": "2026-02-01T00:00:00Z" }
    })
}

pub fn standard_extras() -> Value {
    json!({
        "mcpServers": { "example": { "command": "node", "args": ["x.js"] } },
        "oauthAccount": { "email": "patrick@example.com" },
        "numStartups": 42
    })
}
