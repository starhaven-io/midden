use crate::paths::WORKTREE_MARKER;
use anyhow::{Result, bail};
use serde_json::Map;
use serde_json::Value;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// One orphaned project entry: a key in `projects` whose directory no longer
/// exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub path: String,
    pub is_worktree: bool,
}

/// Inspect a `projects` map and report which keys point at directories that
/// are provably absent. Existence is the only signal — we never guess from
/// the value's contents.
pub fn find(projects: &Map<String, Value>, worktrees_only: bool) -> Vec<Orphan> {
    let mut out = Vec::new();
    for path in projects.keys() {
        let is_worktree = path.contains(WORKTREE_MARKER);
        if worktrees_only && !is_worktree {
            continue;
        }
        if provably_absent(Path::new(path)) {
            out.push(Orphan {
                path: path.clone(),
                is_worktree,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Whether `path` is provably not a project directory. Only a definitive
/// answer counts: the path resolves to nothing (`NotFound`), routes through a
/// non-directory (`NotADirectory`), or exists but is not a directory. Any
/// other stat failure — permission denied, I/O error, an unresponsive mount —
/// means "can't tell", and an entry we can't check must never be pruned. A
/// path whose volume is not mounted also stats as `NotFound`, so that case is
/// excluded here rather than left to the mass-deletion gate, which only
/// notices when nearly everything is missing.
pub(crate) fn provably_absent(path: &Path) -> bool {
    let absent = match std::fs::metadata(path) {
        Ok(meta) => !meta.is_dir(),
        Err(e) => matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory),
    };
    absent && unmounted_volume(path).is_none()
}

/// Directories whose children are volumes that come and go: external disks,
/// network shares, automounts. The count is how many components below the
/// root name the volume (`/media/<user>/<label>` needs two). A volume
/// directory that is missing means the volume is not mounted right now, which
/// says nothing about whether the project it held still exists.
const VOLUME_ROOTS: &[(&str, usize)] = &[
    ("/Volumes", 1),
    ("/Network", 1),
    ("/mnt", 1),
    ("/media", 2),
    ("/run/media", 2),
    ("/net", 1),
    ("/afs", 1),
];

/// Symlink hops followed before resolution gives up, matching the kernel's
/// own limit so a link loop ends in "can't tell" rather than a hang.
const MAX_SYMLINK_HOPS: usize = 40;

/// The absent volume directory that `path` would live on, if it sits under a
/// volume root whose volume is not mounted.
pub(crate) fn unmounted_volume(path: &Path) -> Option<PathBuf> {
    unmounted_volume_within(
        path,
        VOLUME_ROOTS
            .iter()
            .map(|(root, depth)| (Path::new(root), *depth)),
    )
}

/// Walk `path` the way the kernel resolves it, one component at a time and
/// through every symlink, and stop at the first volume directory that is
/// missing. `fs::metadata` follows the same links, so a project reached
/// through `~/disk -> /Volumes/Disk` stats as `NotFound` once the disk is
/// unplugged, and only the walk shows which volume it was on. The walk ends
/// where the kernel's would: nothing after a missing component can be
/// reached, a `..` included, so a volume named only beyond that point is no
/// evidence. The one exception is a missing volume root, as `/Volumes` is on
/// Linux: its children are volumes by position whether or not it exists.
fn unmounted_volume_within<'a>(
    path: &Path,
    roots: impl IntoIterator<Item = (&'a Path, usize)>,
) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let roots: Vec<(&Path, usize)> = roots.into_iter().collect();
    let mut pending: Vec<OsString> = path
        .components()
        .rev()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    let mut resolved = PathBuf::new();
    let mut exists = true;
    let mut hops = 0;
    while let Some(component) = pending.pop() {
        match Path::new(&component).components().next() {
            Some(Component::RootDir) => resolved.push(&component),
            Some(Component::ParentDir) if exists => {
                resolved.pop();
            }
            Some(Component::ParentDir) => return None,
            Some(Component::Normal(_)) => {
                resolved.push(&component);
                let volume = names_volume(&resolved, &roots);
                if !exists {
                    if volume {
                        return Some(resolved);
                    }
                    continue;
                }
                if volume {
                    match std::fs::metadata(&resolved) {
                        Ok(_) => {}
                        Err(e) if e.kind() == ErrorKind::NotFound => return Some(resolved),
                        Err(_) => return None,
                    }
                }
                match std::fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        hops += 1;
                        if hops > MAX_SYMLINK_HOPS {
                            return None;
                        }
                        let target = std::fs::read_link(&resolved).ok()?;
                        resolved.pop();
                        pending.extend(
                            target
                                .components()
                                .rev()
                                .map(|component| component.as_os_str().to_owned()),
                        );
                    }
                    Ok(_) => {}
                    Err(e)
                        if e.kind() == ErrorKind::NotFound
                            && roots.iter().any(|(root, _)| root.starts_with(&resolved)) =>
                    {
                        exists = false;
                    }
                    Err(_) => return None,
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether `path` sits at volume depth under one of `roots`: the first
/// component below `/Volumes`, either of the first two below `/media`.
fn names_volume(path: &Path, roots: &[(&Path, usize)]) -> bool {
    roots.iter().any(|(root, depth)| {
        path.strip_prefix(root)
            .is_ok_and(|rest| (1..=*depth).contains(&rest.components().count()))
    })
}

/// A `projects` entry kept because its volume is not mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmounted {
    pub path: String,
    pub volume: PathBuf,
}

/// Entries whose directory is missing only because its volume is not mounted.
/// They are not orphans, but a dry run that silently kept them would look
/// like it had missed them.
pub fn on_unmounted_volumes(projects: &Map<String, Value>, worktrees_only: bool) -> Vec<Unmounted> {
    let mut out = Vec::new();
    for path in projects.keys() {
        if worktrees_only && !path.contains(WORKTREE_MARKER) {
            continue;
        }
        if let Some(volume) = unmounted_volume(Path::new(path)) {
            out.push(Unmounted {
                path: path.clone(),
                volume,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Split a list of orphans into (worktree, other) counts.
pub fn counts(orphans: &[Orphan]) -> (usize, usize) {
    let wt = orphans.iter().filter(|o| o.is_worktree).count();
    (wt, orphans.len() - wt)
}

/// Refuse a removal set that looks like the wrong host rather than dead
/// entries. Entries kept for an unmounted volume still resolve missing on
/// this host; leaving them out of the ratio would let a config copied from
/// another machine look healthy enough to prune whatever is left, so they
/// count as missing while never being removed.
pub fn ensure_not_wrong_host(
    force: bool,
    removing: usize,
    unmounted: usize,
    total: usize,
) -> Result<()> {
    let missing = removing + unmounted;
    if !force && removing > 0 && looks_like_wrong_host(missing, total) {
        bail!(
            "{missing} of {total} project entries resolve missing{} — this usually means you are \
             on a different machine or an unmounted volume, not that they are all dead. \
             Re-run with --force to prune them anyway.",
            unmounted_note(unmounted)
        );
    }
    Ok(())
}

/// The gates' note on how many of the missing entries are kept for a volume
/// that is not mounted.
pub fn unmounted_note(unmounted: usize) -> String {
    if unmounted == 0 {
        String::new()
    } else {
        format!(" ({unmounted} on volumes that are not mounted)")
    }
}

/// Whether a removal set this large relative to the total looks less like
/// genuine orphans and more like running on the wrong machine or against an
/// unmounted volume (where every recorded absolute path resolves missing).
/// Existence alone can't distinguish the two, so mass deletions are gated
/// behind `--force`.
pub fn looks_like_wrong_host(removing: usize, total: usize) -> bool {
    looks_like_wrong_host_with_scope(removing, total, total)
}

/// Variant for surfaces where the wrong-host ratio and the minimum engagement
/// count need different denominators. For transcript dirs, skipped dirs should
/// help engage the safety gate, but not dilute the dead/resolvable ratio.
pub fn looks_like_wrong_host_with_scope(
    removing: usize,
    ratio_total: usize,
    engagement_total: usize,
) -> bool {
    removing > 0 && engagement_total >= 5 && removing * 100 >= ratio_total * 90
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(entries: &[(&str, Value)]) -> Map<String, Value> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn extant_dirs_are_not_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let projects = map(&[(&path, json!({}))]);
        assert!(find(&projects, false).is_empty());
    }

    #[test]
    fn missing_dirs_are_orphans() {
        let projects = map(&[("/no/such/path", json!({}))]);
        let orphans = find(&projects, false);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].path, "/no/such/path");
        assert!(!orphans[0].is_worktree);
    }

    #[test]
    fn a_file_at_the_path_is_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let key = file.to_string_lossy().into_owned();
        let projects = map(&[(&key, json!({}))]);
        assert_eq!(
            find(&projects, false).len(),
            1,
            "a file is provably not a project dir"
        );
    }

    #[test]
    fn a_path_through_a_file_is_an_orphan() {
        // stat() on <file>/child fails with ENOTDIR — still a definitive answer.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let key = file.join("child").to_string_lossy().into_owned();
        let projects = map(&[(&key, json!({}))]);
        assert_eq!(find(&projects, false).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn an_unstatable_path_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let project = locked.join("project");
        std::fs::create_dir(&project).unwrap();
        let key = project.to_string_lossy().into_owned();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let projects = map(&[(&key, json!({}))]);
        let orphans = find(&projects, false);

        // Restore so the TempDir can clean up after itself.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Unprivileged: the stat fails with EACCES -> can't tell -> kept. Root
        // bypasses the mode and sees a live directory -> also kept.
        assert!(
            orphans.is_empty(),
            "unstatable entries must never be pruned: {orphans:?}"
        );
    }

    #[test]
    fn worktree_marker_classifies() {
        let projects = map(&[(
            "/Users/x/proj/.claude/worktrees/witty-curie/checkout",
            json!({}),
        )]);
        let orphans = find(&projects, false);
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].is_worktree);
    }

    #[test]
    fn worktrees_only_filters_non_worktree() {
        let projects = map(&[
            ("/no/such/path", json!({})),
            (
                "/Users/x/proj/.claude/worktrees/witty-curie/checkout",
                json!({}),
            ),
        ]);
        let orphans = find(&projects, true);
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].is_worktree);
    }

    #[test]
    fn wrong_host_heuristic() {
        assert!(!looks_like_wrong_host(3, 4), "small total never trips");
        assert!(!looks_like_wrong_host(4, 10), "40% is normal");
        assert!(!looks_like_wrong_host(0, 10), "nothing missing");
        assert!(looks_like_wrong_host(9, 10), "90% missing");
        assert!(looks_like_wrong_host(10, 10), "all missing");
    }

    #[test]
    fn wrong_host_heuristic_can_engage_on_a_larger_scope() {
        assert!(
            looks_like_wrong_host_with_scope(4, 4, 5),
            "skipped transcript dirs should count toward the engagement minimum"
        );
        assert!(
            !looks_like_wrong_host_with_scope(3, 4, 5),
            "ratio still uses the resolvable denominator"
        );
    }

    #[test]
    fn a_project_on_an_absent_volume_is_not_an_orphan() {
        // Lexically under a volume root on every platform, with a volume that
        // does not exist: indistinguishable from a project on an unplugged disk.
        let volume = format!("/Volumes/midden-missing-{}", std::process::id());
        let key = format!("{volume}/project");
        let projects = map(&[(&key, json!({}))]);
        assert!(find(&projects, false).is_empty(), "not dead, not mounted");
        let kept = on_unmounted_volumes(&projects, false);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, key);
        assert_eq!(kept[0].volume, Path::new(&volume));
        assert!(
            on_unmounted_volumes(&projects, true).is_empty(),
            "worktrees-only filter applies"
        );
    }

    #[test]
    fn a_missing_project_on_a_mounted_volume_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        // Custom roots must be spelled the way resolution spells them.
        let base = dir.path().canonicalize().unwrap();
        let volume = base.join("disk");
        std::fs::create_dir(&volume).unwrap();
        let roots = [(base.as_path(), 1)];
        assert_eq!(
            unmounted_volume_within(&volume.join("gone"), roots),
            None,
            "volume mounted, project gone"
        );
        assert_eq!(
            unmounted_volume_within(&base.join("other").join("gone"), roots),
            Some(base.join("other"))
        );
        assert_eq!(unmounted_volume_within(&volume, roots), None);
    }

    #[test]
    fn per_user_media_volumes_need_two_components() {
        let dir = tempfile::tempdir().unwrap();
        // Custom roots must be spelled the way resolution spells them.
        let base = dir.path().canonicalize().unwrap();
        let user = base.join("alice");
        let label = user.join("disk");
        std::fs::create_dir_all(&label).unwrap();
        let roots = [(base.as_path(), 2)];
        assert_eq!(unmounted_volume_within(&label.join("gone"), roots), None);
        assert_eq!(
            unmounted_volume_within(&user.join("other").join("gone"), roots),
            Some(user.join("other"))
        );
        assert_eq!(
            unmounted_volume_within(&base.join("bob").join("disk"), roots),
            Some(base.join("bob"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn aliases_of_an_absent_volume_name_the_volume() {
        let dir = tempfile::tempdir().unwrap();
        // Custom roots must be spelled the way resolution spells them.
        let base = dir.path().canonicalize().unwrap();
        let vols = base.join("vols");
        let other = base.join("other");
        std::fs::create_dir_all(&vols).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let disk = vols.join("disk");
        std::os::unix::fs::symlink(&disk, base.join("alias")).unwrap();
        std::os::unix::fs::symlink("../vols/disk", other.join("rel")).unwrap();
        let roots = [(vols.as_path(), 1)];
        for alias in [
            base.join("alias/project"),
            other.join("rel/project"),
            other.join("../vols/disk/project"),
        ] {
            assert_eq!(
                unmounted_volume_within(&alias, roots),
                Some(disk.clone()),
                "{}",
                alias.display()
            );
        }
        std::fs::create_dir(&disk).unwrap();
        assert_eq!(
            unmounted_volume_within(&base.join("alias/project"), roots),
            None,
            "volume mounted, project gone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("loop", dir.path().join("loop")).unwrap();
        let key = dir.path().join("loop/project");
        assert_eq!(unmounted_volume(&key), None);
        let projects = map(&[(&key.to_string_lossy(), json!({}))]);
        assert!(find(&projects, false).is_empty(), "ELOOP is not NotFound");
    }

    #[test]
    fn the_walk_stops_where_the_kernel_does() {
        let dir = tempfile::tempdir().unwrap();
        // Custom roots must be spelled the way resolution spells them.
        let base = dir.path().canonicalize().unwrap();
        let vols = base.join("vols");
        let live = base.join("live");
        std::fs::create_dir_all(&vols).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        let roots = [(vols.as_path(), 1)];
        // Lexically this is the live directory, but the kernel rejects it at
        // `gone`, so it is unreachable until the volume is mounted.
        let gone = vols.join("gone");
        assert_eq!(
            unmounted_volume_within(&gone.join("../../live"), roots),
            Some(gone.clone())
        );
        assert_eq!(
            unmounted_volume_within(&base.join("missing/../vols/gone/project"), roots),
            None,
            "dead before any volume root is reached"
        );
    }

    #[test]
    fn a_volume_path_is_judged_where_the_kernel_stops() {
        let live = tempfile::tempdir().unwrap();
        let live = live.path().canonicalize().unwrap();
        let volume = format!("/Volumes/midden-missing-{}", std::process::id());
        let key = format!("{volume}/../..{}", live.display());
        let projects = map(&[(&key, json!({}))]);
        assert!(find(&projects, false).is_empty(), "not dead, not mounted");
        let kept = on_unmounted_volumes(&projects, false);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].volume, Path::new(&volume));
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_at_the_volume_directory_names_the_volume() {
        let dir = tempfile::tempdir().unwrap();
        // Custom roots must be spelled the way resolution spells them.
        let base = dir.path().canonicalize().unwrap();
        let vols = base.join("vols");
        std::fs::create_dir_all(&vols).unwrap();
        let disk = vols.join("disk");
        std::os::unix::fs::symlink(base.join("no/such/target"), &disk).unwrap();
        std::os::unix::fs::symlink(disk.join("project"), base.join("alias")).unwrap();
        let roots = [(vols.as_path(), 1)];
        for path in [disk.join("project"), base.join("alias/src")] {
            assert_eq!(
                unmounted_volume_within(&path, roots),
                Some(disk.clone()),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn the_gate_counts_unmounted_entries_as_missing() {
        assert!(ensure_not_wrong_host(false, 1, 0, 10).is_ok());
        let error = ensure_not_wrong_host(false, 1, 9, 10)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "10 of 10 project entries resolve missing (9 on volumes that are not mounted)"
            ),
            "{error}"
        );
        assert!(
            ensure_not_wrong_host(false, 0, 10, 10).is_ok(),
            "nothing to remove, nothing to gate"
        );
        assert!(ensure_not_wrong_host(true, 1, 9, 10).is_ok());
    }

    #[test]
    fn parent_components_do_not_name_a_volume() {
        assert_eq!(unmounted_volume(Path::new("/Volumes/../nope")), None);
        assert_eq!(unmounted_volume(Path::new("/Volumes")), None);
        assert_eq!(unmounted_volume(Path::new("relative/Volumes/x")), None);
    }

    #[test]
    fn counts_split_worktree_and_other() {
        let orphans = vec![
            Orphan {
                path: "/a".into(),
                is_worktree: false,
            },
            Orphan {
                path: "/b".into(),
                is_worktree: true,
            },
            Orphan {
                path: "/c".into(),
                is_worktree: true,
            },
        ];
        assert_eq!(counts(&orphans), (2, 1));
    }
}
