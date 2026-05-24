use crate::paths::WORKTREE_MARKER;
use serde_json::Map;
use serde_json::Value;
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
        if !Path::new(path).is_dir() {
            out.push(Orphan {
                path: path.clone(),
                is_worktree,
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
