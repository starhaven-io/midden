use std::path::{Path, PathBuf};
use std::process::Command;

/// Exit code of `git -C root <args> -- <path>`, or None if git can't be spawned.
/// A 128 (fatal: not a repository, etc.) also maps to None — callers treat that
/// as "can't tell".
fn git_code(root: &Path, args: &[&str], path: &Path) -> Option<i32> {
    Command::new("git")
        .arg("-C")
        .arg(root)
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
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(value.trim());
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
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
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
}
