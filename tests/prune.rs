mod common;

use common::{Fixture, standard_extras, standard_projects};
use predicates::str::contains;
use serde_json::{Value, json};

#[test]
fn dry_run_lists_orphans_and_writes_nothing() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live-project");
    fx.write_config(standard_projects(&live), standard_extras());
    let before = std::fs::read_to_string(&fx.config).unwrap();

    fx.cmd()
        .arg("prune")
        .assert()
        .success()
        .stdout(contains("/no/such/dir"))
        .stdout(contains("/also/gone"))
        .stdout(contains("[worktree]"))
        .stdout(contains("dry run"));

    let after = std::fs::read_to_string(&fx.config).unwrap();
    assert_eq!(before, after, "dry run must not modify the config");
    assert!(fx.backup_paths().is_empty(), "no backup on dry run");
}

#[test]
fn apply_removes_orphans_keeps_live_and_extras_intact() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live-project");
    fx.write_config(standard_projects(&live), standard_extras());

    fx.cmd()
        .arg("prune")
        .arg("--apply")
        // We can't easily simulate "no running claude" in CI when Claude IS
        // running. The test process itself is named differently, but if a real
        // `claude` is up, --force is the only safe option in tests.
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("backed up to"));

    let after = fx.read_config();
    let projects = after["projects"].as_object().unwrap();

    assert_eq!(projects.len(), 1, "exactly one entry should survive");
    assert!(projects.contains_key(&live));
    assert!(!projects.contains_key("/no/such/dir"));
    assert!(!projects.contains_key("/also/gone"));
    assert!(
        !projects.contains_key("/Users/x/proj/.claude/worktrees/witty-curie/checkout"),
        "worktree orphan should be pruned"
    );

    // Unrelated top-level keys preserved in meaning.
    assert_eq!(
        after["mcpServers"]["example"]["command"],
        json!("node"),
        "mcpServers must survive"
    );
    assert_eq!(
        after["oauthAccount"]["email"],
        json!("patrick@example.com"),
        "oauthAccount must survive"
    );
    assert_eq!(after["numStartups"], json!(42));

    let backups = fx.backup_paths();
    assert_eq!(backups.len(), 1, "exactly one backup should be written");
}

#[test]
fn backup_is_a_faithful_copy_of_the_pre_apply_config() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live-project");
    fx.write_config(standard_projects(&live), standard_extras());
    let before = std::fs::read_to_string(&fx.config).unwrap();

    fx.cmd()
        .arg("prune")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();

    let backups = fx.backup_paths();
    assert_eq!(backups.len(), 1, "exactly one backup");
    let backed_up = std::fs::read_to_string(&backups[0]).unwrap();
    assert_eq!(
        backed_up, before,
        "backup must byte-match the pre-apply config (not a post-write copy)"
    );
    let after = std::fs::read_to_string(&fx.config).unwrap();
    assert_ne!(after, before, "live config should have changed");
}

#[test]
fn worktrees_only_skips_non_worktree_orphans() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live-project");
    fx.write_config(standard_projects(&live), standard_extras());

    fx.cmd()
        .arg("prune")
        .arg("--worktrees-only")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();

    let after = fx.read_config();
    let projects = after["projects"].as_object().unwrap();
    assert!(projects.contains_key(&live));
    // Non-worktree orphans must remain when --worktrees-only is set.
    assert!(projects.contains_key("/no/such/dir"));
    assert!(projects.contains_key("/also/gone"));
    assert!(
        !projects.contains_key("/Users/x/proj/.claude/worktrees/witty-curie/checkout"),
        "worktree orphan should be pruned"
    );
}

#[test]
fn refuses_mass_deletion_without_force() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live");
    let mut projects = serde_json::Map::new();
    projects.insert(live, json!({}));
    for i in 0..9 {
        projects.insert(format!("/gone/{i}"), json!({}));
    }
    fx.write_config(Value::Object(projects), standard_extras());

    // 9 of 10 entries resolve missing (>=90%). The mass-deletion guard is
    // checked before the running-claude gate, so this is deterministic in CI
    // regardless of whether a real claude process happens to be running.
    fx.cmd()
        .arg("prune")
        .arg("--apply")
        .assert()
        .failure()
        .stderr(contains("different machine"));
    assert!(
        fx.backup_paths().is_empty(),
        "must not write when the mass-deletion guard refuses"
    );

    // --force overrides the guard.
    fx.cmd()
        .arg("prune")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();
    let after = fx.read_config();
    assert_eq!(after["projects"].as_object().unwrap().len(), 1);
}

#[test]
fn clean_config_reports_no_action() {
    let fx = Fixture::new();
    let live = fx.touch_dir("only-live");
    fx.write_config(json!({ &live: {} }), standard_extras());

    fx.cmd()
        .arg("prune")
        .assert()
        .success()
        .stdout(contains("clean."));
    assert!(fx.backup_paths().is_empty());
}

#[test]
fn apply_preserves_top_level_key_order() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live-project");
    fx.write_config(standard_projects(&live), standard_extras());

    fx.cmd()
        .arg("prune")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();

    // Assert on the RAW written text — re-parsing with serde_json (which also
    // has preserve_order) would mask a regression that dropped the feature.
    let raw = std::fs::read_to_string(&fx.config).unwrap();
    let pos = |needle: &str| {
        raw.find(needle)
            .unwrap_or_else(|| panic!("missing {needle} in:\n{raw}"))
    };
    let (mcp, oauth, starts, projects) = (
        pos("\"mcpServers\""),
        pos("\"oauthAccount\""),
        pos("\"numStartups\""),
        pos("\"projects\""),
    );
    assert!(
        mcp < oauth && oauth < starts && starts < projects,
        "top-level key order not preserved (mcp={mcp} oauth={oauth} starts={starts} projects={projects}):\n{raw}"
    );
}

#[test]
fn errors_on_malformed_config() {
    let fx = Fixture::new();
    std::fs::write(&fx.config, "{ not valid json").unwrap();
    fx.cmd()
        .arg("prune")
        .assert()
        .failure()
        .stderr(contains("parse"));
}

#[test]
fn handles_config_without_projects_map() {
    let fx = Fixture::new();
    // Parseable, but no "projects" key (write_config always adds one, so write
    // directly).
    std::fs::write(
        &fx.config,
        serde_json::to_string_pretty(&json!({ "numStartups": 1 })).unwrap(),
    )
    .unwrap();
    fx.cmd()
        .arg("prune")
        .assert()
        .success()
        .stdout(contains("no 'projects' map found"));
}

#[test]
fn missing_config_is_an_error() {
    let fx = Fixture::new();
    // Don't write a config.
    fx.cmd()
        .arg("prune")
        .assert()
        .failure()
        .stderr(contains("not found"));
}

#[test]
fn json_output_dry_run_emits_orphans_array() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live");
    fx.write_config(standard_projects(&live), standard_extras());

    let out = fx.cmd().arg("--json").arg("prune").output().expect("run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["total"], json!(4));
    assert_eq!(v["orphans"].as_array().unwrap().len(), 3);
    assert_eq!(v["removed"], json!(false));
    assert_eq!(v["backup"], json!(null), "no backup on dry run");
}

#[test]
fn json_output_apply_emits_backup_path() {
    let fx = Fixture::new();
    let live = fx.touch_dir("live");
    fx.write_config(standard_projects(&live), standard_extras());

    let out = fx
        .cmd()
        .arg("--json")
        .arg("prune")
        .arg("--apply")
        .arg("--force")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["removed"], json!(true));
    assert!(v["bytes_after"].as_u64().unwrap() < v["bytes_before"].as_u64().unwrap());

    let backup = v["backup"].as_str().expect("backup path string");
    assert!(backup.contains(".bak-"), "backup path: {backup}");
    assert!(
        std::path::Path::new(backup).is_file(),
        "backup file should exist: {backup}"
    );
}
