use crate::paths::WORKTREE_MARKER;
use serde_json::Map;
use serde_json::Value;
use std::io::ErrorKind;
use std::path::Path;

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
/// means "can't tell", and an entry we can't check must never be pruned.
pub(crate) fn provably_absent(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => !meta.is_dir(),
        Err(e) => matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory),
    }
}

/// Split a list of orphans into (worktree, other) counts.
pub fn counts(orphans: &[Orphan]) -> (usize, usize) {
    let wt = orphans.iter().filter(|o| o.is_worktree).count();
    (wt, orphans.len() - wt)
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
