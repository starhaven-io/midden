use std::path::{Path, PathBuf};
use std::process::Command;

/// Targets are often untrusted checkouts whose repository config can name a
/// `core.fsmonitor` program that `ls-files` and `check-ignore` execute, or
/// whose committed subdirectory is laid out as a bare repository that Git
/// discovers implicitly. Command-line config overrides the repository's, and
/// `safe.bareRepository` is honored only from protected config such as this.
fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    // These would point every query at a repository other than the one that
    // holds `root`.
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(variable);
    }
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

/// The worktree root of the repository holding `path`. The result bounds
/// which instruction imports count as internal, so it must not be movable by
/// the checkout itself: `core.worktree` in the repository's own config (or in
/// the config of a gitdir its `.git` file points at) makes Git report any
/// directory as the top level. Only a top level that holds the nearest `.git`
/// entry at or above `path` is accepted. That entry stays in the worktree
/// root for linked worktrees and submodules, so those still resolve.
pub fn repository_root(path: &Path) -> Option<PathBuf> {
    let reported = git_path(path, &["rev-parse", "--show-toplevel"])?;
    let path = path.canonicalize().ok()?;
    let discovered = path
        .ancestors()
        .find(|directory| directory.join(".git").symlink_metadata().is_ok())?;
    (discovered == reported).then_some(reported)
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

    #[test]
    fn configured_worktree_cannot_move_the_repository_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("outer").join("repo");
        std::fs::create_dir_all(repo.join("nested")).unwrap();
        git(&repo, &["init", "--quiet"]);
        assert_eq!(
            repository_root(&repo.join("nested")),
            repo.canonicalize().ok()
        );

        // A relative value is resolved against the Git directory.
        for worktree in ["../..", "../../..", "/", "../nested"] {
            git(&repo, &["config", "core.worktree", worktree]);
            assert_eq!(repository_root(&repo), None, "core.worktree = {worktree}");
            assert_eq!(
                repository_root(&repo.join("nested")),
                None,
                "core.worktree = {worktree}"
            );
        }
    }

    #[test]
    fn gitdir_file_cannot_borrow_a_worktree_from_another_repository() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        git(&other, &["init", "--quiet"]);
        git(
            &other,
            &["config", "core.worktree", dir.path().to_str().unwrap()],
        );
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", other.join(".git").display()),
        )
        .unwrap();

        assert_eq!(repository_root(&checkout), None);
    }

    #[test]
    fn linked_worktrees_and_submodules_keep_their_own_root() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        git(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let linked = dir.path().join("linked");
        git(
            &main,
            &["worktree", "add", "--quiet", linked.to_str().unwrap()],
        );
        std::fs::create_dir_all(linked.join("nested")).unwrap();
        assert_eq!(
            repository_root(&linked.join("nested")),
            linked.canonicalize().ok()
        );

        // A submodule's gitdir lives under the superproject and names the
        // submodule directory through core.worktree.
        let submodule = main.join("sub");
        let gitdir = main.join(".git").join("modules").join("sub");
        std::fs::create_dir_all(&submodule).unwrap();
        std::fs::create_dir_all(gitdir.parent().unwrap()).unwrap();
        git(
            &submodule,
            &[
                "init",
                "--quiet",
                "--separate-git-dir",
                gitdir.to_str().unwrap(),
            ],
        );
        git(&submodule, &["config", "core.worktree", "../../../sub"]);
        assert_eq!(repository_root(&submodule), submodule.canonicalize().ok());
    }

    #[test]
    fn environment_cannot_redirect_repository_queries() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        let output = git_command(&repo)
            .env("GIT_WORK_TREE", "/")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "/",
            "fixture: an inherited GIT_WORK_TREE would move the top level"
        );
        let scrubbed: Vec<_> = git_command(&repo)
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_owned())
            .collect();
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
        ] {
            assert!(scrubbed.iter().any(|name| name == variable), "{variable}");
        }
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
