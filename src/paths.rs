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
    // std::env::home_dir was undeprecated in Rust 1.86. Fall back to $HOME on
    // older toolchains and Linux where it consults /etc/passwd.
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    #[cfg(target_os = "windows")]
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(home);
    }
    PathBuf::from(".")
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

    pub fn claude_md(&self) -> PathBuf {
        self.root.join("CLAUDE.md")
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

/// The marker substring that identifies ephemeral worktree directories.
pub const WORKTREE_MARKER: &str = "/.claude/worktrees/";
