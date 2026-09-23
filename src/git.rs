use std::path::{Path, PathBuf};
use std::process::Command;

/// Targets are often untrusted checkouts whose repository config can name a
/// `core.fsmonitor` program that `ls-files` and `check-ignore` execute, or
/// whose committed subdirectory is laid out as a bare repository that Git
/// discovers implicitly. Command-line config overrides the repository's, and
/// `safe.bareRepository` is honored only from protected config such as this.
fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "safe.bareRepository=explicit"])
        .arg("-C")
        .arg(root);
    command
}

/// Exit code of `git -C root <args> -- <path>`, or None if git can't be spawned.
/// A 128 (fatal: not a repository, etc.) also maps to None — callers treat that
/// as "can't tell".
fn git_code(root: &Path, args: &[&str], path: &Path) -> Option<i32> {
    git_command(root)
        .args(args)
        .arg("--")
        .arg(path)
        .output()
        .ok()?
        .status
        .code()
}

/// Whether `path` (relative to `root`) is tracked by git — present in the
/// index. None when git is unavailable or `root` is not inside a repository.
/// Pass a repo-relative path: an absolute path can fall outside the worktree
/// git discovers once symlinks (e.g. macOS `/var` -> `/private/var`) differ.
pub fn is_tracked(root: &Path, path: &Path) -> Option<bool> {
    match git_code(root, &["ls-files", "--error-unmatch"], path)? {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

/// Whether `path` (relative to `root`) is ignored by git. None when git is
/// unavailable or `root` is not inside a repository.
pub fn is_ignored(root: &Path, path: &Path) -> Option<bool> {
    match git_code(root, &["check-ignore", "--quiet"], path)? {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

fn git_path(root: &Path, args: &[&str]) -> Option<PathBuf> {
    let output = git_command(root).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut bytes = output.stdout;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(String::from_utf8(bytes).ok()?);
    let absolute = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    absolute.canonicalize().ok()
}

pub fn repository_root(path: &Path) -> Option<PathBuf> {
    git_path(path, &["rev-parse", "--show-toplevel"])
}

/// The common Git directory is shared by every worktree, unlike the worktree
/// root, so it is the repository identity Claude's shared memory needs.
pub fn repository_identity(path: &Path) -> Option<PathBuf> {
    git_path(path, &["rev-parse", "--git-common-dir"])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn untracked_then_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "--quiet"]);
        std::fs::write(root.join("local.json"), "{}").unwrap();
        let rel = Path::new("local.json");

        assert_eq!(is_tracked(root, rel), Some(false));
        git(root, &["add", "local.json"]);
        assert_eq!(is_tracked(root, rel), Some(true));
    }

    #[test]
    fn ignored_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "--quiet"]);
        std::fs::write(root.join(".gitignore"), "secret.json\n").unwrap();
        std::fs::write(root.join("secret.json"), "{}").unwrap();
        let rel = Path::new("secret.json");

        assert_eq!(is_ignored(root, rel), Some(true));
        assert_eq!(is_tracked(root, rel), Some(false));
    }

    #[test]
    fn outside_a_repo_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let rel = Path::new("x.json");
        assert_eq!(is_tracked(dir.path(), rel), None);
        assert_eq!(is_ignored(dir.path(), rel), None);
    }

    #[test]
    fn root_and_identity_are_canonical() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);

        assert_eq!(repository_root(dir.path()), dir.path().canonicalize().ok());
        assert_eq!(
            repository_identity(dir.path()),
            dir.path().join(".git").canonicalize().ok()
        );
    }
    #[test]
    fn repository_paths_preserve_trailing_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project ");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        assert_eq!(repository_root(&root), root.canonicalize().ok());
        assert_eq!(
            repository_identity(&root),
            root.join(".git").canonicalize().ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn implicit_bare_repositories_are_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("marker");
        let hook = outside.path().join("fsmonitor.sh");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho ran >> '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let embedded = dir.path().join("embedded");
        std::fs::create_dir_all(embedded.join("objects")).unwrap();
        std::fs::create_dir_all(embedded.join("refs/heads")).unwrap();
        std::fs::write(embedded.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            embedded.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tworktree = .\n\tfsmonitor = {}\n",
                hook.display()
            ),
        )
        .unwrap();
        std::fs::write(embedded.join("x.json"), "{}").unwrap();
        let rel = Path::new("x.json");

        assert_eq!(is_tracked(&embedded, rel), None);
        assert_eq!(is_ignored(&embedded, rel), None);
        assert_eq!(repository_root(&embedded), None);
        assert_eq!(repository_identity(&embedded), None);
        assert!(!marker.exists(), "the embedded fsmonitor hook ran");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn repository_paths_preserve_non_unicode_names() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        assert_eq!(repository_root(&root), root.canonicalize().ok());
        assert_eq!(
            repository_identity(&root),
            root.join(".git").canonicalize().ok()
        );
    }
}
