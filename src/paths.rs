use std::path::{Path, PathBuf};

/// Resolved paths for the user-scope state Claude Code writes.
///
/// Constructed once from CLI flags (`--config`, `--claude-home`) with `$HOME`
/// as the fallback. Tests construct this directly to point at a fixture dir.
pub struct Env {
    pub claude_json: PathBuf,
    pub claude_home: PathBuf,
}

impl Env {
    pub fn new(config: Option<PathBuf>, claude_home: Option<PathBuf>) -> Self {
        let home = home_dir();
        Self {
            claude_json: config.unwrap_or_else(|| home.join(".claude.json")),
            claude_home: claude_home.unwrap_or_else(|| home.join(".claude")),
        }
    }

    pub fn user_settings(&self) -> PathBuf {
        self.claude_home.join("settings.json")
    }

    pub fn user_claude_md(&self) -> PathBuf {
        self.claude_home.join("CLAUDE.md")
    }

    pub fn user_skills_dir(&self) -> PathBuf {
        self.claude_home.join("skills")
    }

    pub fn user_commands_dir(&self) -> PathBuf {
        self.claude_home.join("commands")
    }

    pub fn user_agents_dir(&self) -> PathBuf {
        self.claude_home.join("agents")
    }
}

fn home_dir() -> PathBuf {
    // std::env::home_dir was un-deprecated in Rust 1.87 (< this crate's 1.88
    // MSRV) and resolves $HOME, then /etc/passwd, on the Unix platforms this
    // tool targets. Fall back to the current directory only if no home exists.
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Project-scope paths rooted at a target directory.
pub struct ProjectPaths {
    pub root: PathBuf,
}

impl ProjectPaths {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn settings(&self) -> PathBuf {
        self.root.join(".claude").join("settings.json")
    }

    pub fn local_settings(&self) -> PathBuf {
        self.root.join(".claude").join("settings.local.json")
    }

    pub fn mcp_json(&self) -> PathBuf {
        self.root.join(".mcp.json")
    }

    pub fn managed_mcp_json(&self) -> PathBuf {
        self.root.join(".claude").join("managed-mcp.json")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join(".claude").join("skills")
    }

    pub fn commands_dir(&self) -> PathBuf {
        self.root.join(".claude").join("commands")
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join(".claude").join("agents")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join(".claude").join("worktrees")
    }
}

/// Managed (MDM-delivered) settings paths. Returns paths in the order they
/// should be checked; existence is the caller's job.
pub fn managed_settings_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"),
            PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.d"),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            PathBuf::from("/etc/claude-code/managed-settings.json"),
            PathBuf::from("/etc/claude-code/managed-settings.d"),
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        vec![]
    }
}

/// Managed settings expanded to concrete files: plain files kept as-is,
/// drop-in directories expanded to their `*.json` entries.
pub fn managed_settings_files() -> Vec<PathBuf> {
    expand_managed(managed_settings_paths())
}

/// Sorted expansion — read_dir order is unspecified, and the last equal-scope
/// source wins a scalar, so an unsorted read would make the resolved winner
/// nondeterministic across runs and machines.
fn expand_managed(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for candidate in candidates {
        if candidate.is_file() {
            out.push(candidate);
        } else if candidate.is_dir()
            && let Ok(entries) = std::fs::read_dir(&candidate)
        {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                .collect();
            files.sort();
            out.extend(files);
        }
    }
    out
}

/// The marker substring that identifies ephemeral worktree directories.
pub const WORKTREE_MARKER: &str = "/.claude/worktrees/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_managed_keeps_files_and_expands_dirs_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("managed-settings.json");
        std::fs::write(&plain, "{}").unwrap();
        let dropin = dir.path().join("managed-settings.d");
        std::fs::create_dir(&dropin).unwrap();
        std::fs::write(dropin.join("b.json"), "{}").unwrap();
        std::fs::write(dropin.join("a.json"), "{}").unwrap();
        std::fs::write(dropin.join("ignore.txt"), "").unwrap();
        let missing = dir.path().join("nope.json");

        let files = expand_managed(vec![plain.clone(), dropin.clone(), missing]);
        assert_eq!(
            files,
            vec![plain, dropin.join("a.json"), dropin.join("b.json")],
            "files kept, dirs expanded sorted, non-json and missing dropped"
        );
    }
}
