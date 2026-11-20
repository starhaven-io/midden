use anyhow::{Result, anyhow};
use serde::Serializer;
use std::path::{Path, PathBuf};

/// Resolved user-scope paths for Claude Code and Codex.
///
/// CLI flags override defaults; `CODEX_HOME` overrides the default Codex path.
/// Tests construct this directly to point at isolated fixture directories.
pub struct Env {
    pub claude_json: PathBuf,
    pub claude_home: PathBuf,
    pub codex_home: PathBuf,
}

impl Env {
    #[cfg(test)]
    pub fn new(config: Option<PathBuf>, claude_home: Option<PathBuf>) -> Self {
        Self::try_new(config, claude_home, None).expect("home directory must be discoverable")
    }

    pub fn try_new(
        config: Option<PathBuf>,
        claude_home: Option<PathBuf>,
        codex_home: Option<PathBuf>,
    ) -> Result<Self> {
        let env_codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
        let needs_home = config.is_none()
            || claude_home.is_none()
            || (codex_home.is_none() && env_codex_home.is_none());
        let home = if needs_home {
            Some(
                std::env::home_dir()
                    .ok_or_else(|| anyhow!("could not determine the user's home directory"))?,
            )
        } else {
            None
        };
        Ok(Self {
            claude_json: config
                .or_else(|| home.as_ref().map(|home| home.join(".claude.json")))
                .expect("home was required for the default config path"),
            claude_home: claude_home
                .or_else(|| home.as_ref().map(|home| home.join(".claude")))
                .expect("home was required for the default Claude path"),
            codex_home: codex_home
                .or(env_codex_home)
                .or_else(|| home.as_ref().map(|home| home.join(".codex")))
                .expect("home was required for the default Codex path"),
        })
    }

    #[cfg(test)]
    pub fn with_codex_home(mut self, codex_home: Option<PathBuf>) -> Self {
        if let Some(codex_home) = codex_home {
            self.codex_home = codex_home;
        }
        self
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

    pub fn codex_config(&self) -> PathBuf {
        self.codex_home.join("config.toml")
    }

    pub fn codex_memory_dirs(&self) -> [PathBuf; 2] {
        ["memories", "memories_v2"].map(|name| self.codex_home.join(name))
    }
}

pub(crate) fn home_dir() -> PathBuf {
    // CLI construction rejects a missing home before resolving default paths.
    std::env::home_dir().expect("home directory was validated during CLI construction")
}

/// Project-scope paths rooted at a target directory.
pub struct ProjectPaths {
    pub root: PathBuf,
    pub repository_root: PathBuf,
    shared_local_root: Option<PathBuf>,
}

impl ProjectPaths {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        let repository_root = crate::git::repository_root(root);
        #[cfg(unix)]
        let shared_local_root = repository_root
            .as_deref()
            .and_then(|worktree| {
                shared_local_settings_root(worktree, rustix::process::getuid().as_raw())
            })
            .filter(|candidate| root.canonicalize().ok().as_ref() != Some(candidate));
        #[cfg(not(unix))]
        let shared_local_root = None;
        Self {
            root: root.to_path_buf(),
            repository_root: repository_root.unwrap_or_else(|| root.to_path_buf()),
            shared_local_root,
        }
    }

    pub fn settings(&self) -> PathBuf {
        self.root.join(".claude").join("settings.json")
    }

    pub fn local_settings_roots(&self) -> impl DoubleEndedIterator<Item = &Path> {
        std::iter::once(self.root.as_path()).chain(self.shared_local_root.as_deref())
    }

    pub fn local_settings(&self) -> impl DoubleEndedIterator<Item = PathBuf> + '_ {
        self.local_settings_roots()
            .map(|root| root.join(LOCAL_SETTINGS))
    }

    pub fn mcp_json(&self) -> PathBuf {
        self.root.join(".mcp.json")
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

pub const LOCAL_SETTINGS: &str = ".claude/settings.local.json";

#[cfg(unix)]
fn shared_local_settings_root(worktree: &Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let main = crate::git::main_worktree(worktree)?;
    if std::env::home_dir()
        .and_then(|home| home.canonicalize().ok())
        .as_deref()
        == Some(&main)
    {
        return None;
    }
    for path in [&main, &main.join(".git"), &main.join(".claude")] {
        match path.symlink_metadata() {
            Ok(metadata) if metadata.uid() == uid => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && path.ends_with(".claude") => {}
            _ => return None,
        }
    }
    Some(main)
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

/// Managed MCP configuration exists only at this system path; a repository's
/// `.claude/managed-mcp.json` is not read by Claude Code.
pub fn managed_mcp_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(
            "/Library/Application Support/ClaudeCode/managed-mcp.json",
        ))
    }
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/etc/claude-code/managed-mcp.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

pub struct PathDiscovery {
    pub paths: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, std::io::Error)>,
}

/// Expand managed files and drop-ins without hiding inaccessible policy layers.
pub fn managed_settings_files() -> PathDiscovery {
    expand_managed(managed_settings_paths())
}

/// Equal-scope precedence depends on sorted drop-ins, not filesystem order.
fn expand_managed(candidates: Vec<PathBuf>) -> PathDiscovery {
    let mut discovery = PathDiscovery {
        paths: Vec::new(),
        errors: Vec::new(),
    };
    for candidate in candidates {
        let metadata = match std::fs::metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                discovery.errors.push((candidate, error));
                continue;
            }
        };
        if metadata.is_file() {
            discovery.paths.push(candidate);
        } else if metadata.is_dir() {
            match std::fs::read_dir(&candidate) {
                Ok(entries) => {
                    let mut files = Vec::new();
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                let path = entry.path();
                                if path.extension().and_then(|extension| extension.to_str())
                                    == Some("json")
                                    && !entry.file_name().as_encoded_bytes().starts_with(b".")
                                {
                                    files.push(path);
                                }
                            }
                            Err(error) => discovery.errors.push((candidate.clone(), error)),
                        }
                    }
                    files.sort();
                    discovery.paths.extend(files);
                }
                Err(error) => discovery.errors.push((candidate, error)),
            }
        }
    }
    discovery
}

/// The marker substring that identifies ephemeral worktree directories.
pub const WORKTREE_MARKER: &str = "/.claude/worktrees/";

// JSON strings require Unicode while Unix paths do not. Match the CLI's human
// rendering instead of letting one byte-oriented path abort the whole report.
pub(crate) fn serialize_path<S>(path: &Path, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&path.display().to_string())
}

pub(crate) fn serialize_optional_path<S>(
    path: &Option<PathBuf>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match path {
        Some(path) => serializer.serialize_some(&path.display().to_string()),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn shared_local_settings_require_the_current_owner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join(".claude")).unwrap();
        let uid = rustix::process::getuid().as_raw();
        assert_eq!(shared_local_settings_root(&root, uid).as_ref(), Some(&root));
        assert_eq!(shared_local_settings_root(&root, uid.wrapping_add(1)), None);
    }

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
        std::fs::write(dropin.join(".hidden.json"), "not JSON").unwrap();
        std::fs::write(dropin.join("._a.json"), "AppleDouble metadata").unwrap();
        let missing = dir.path().join("nope.json");

        let files = expand_managed(vec![plain.clone(), dropin.clone(), missing]);
        assert_eq!(
            files.paths,
            vec![plain, dropin.join("a.json"), dropin.join("b.json")],
            "files kept, dirs expanded sorted, non-json and missing dropped"
        );
    }
    #[test]
    fn managed_discovery_distinguishes_absence_from_uninspectable_paths() {
        let dir = tempfile::tempdir().unwrap();
        let parent_file = dir.path().join("not-a-directory");
        std::fs::write(&parent_file, "{}").unwrap();
        let invalid = parent_file.join("managed-settings.json");
        let discovery = expand_managed(vec![dir.path().join("absent.json"), invalid.clone()]);
        assert!(discovery.paths.is_empty());
        assert_eq!(discovery.errors.len(), 1);
        assert_eq!(discovery.errors[0].0, invalid);
        assert_eq!(
            discovery.errors[0].1.kind(),
            std::io::ErrorKind::NotADirectory
        );
    }
}
