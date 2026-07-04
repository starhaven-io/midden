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

#[cfg(unix)]
#[test]
fn apply_never_broadens_the_config_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let live = fx.touch_dir("live-project");
    fx.write_config(standard_projects(&live), standard_extras());
    // Claude Code keeps ~/.claude.json owner-only; a prune --apply must not
    // republish it under the umask default (the 0600 -> 0644 regression).
    std::fs::set_permissions(&fx.config, std::fs::Permissions::from_mode(0o600)).unwrap();

    fx.cmd()
        .arg("prune")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&fx.config), 0o600, "config must stay owner-only");
    let backups = fx.backup_paths();
    assert_eq!(backups.len(), 1);
    assert_eq!(mode(&backups[0]), 0o600, "backup holds the same secrets");
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

#[test]
fn transcripts_dry_run_reports_dead_artifacts_and_deletes_nothing() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let dead_cwd = fx.root.path().join("missing-project");
    let dead_cwd = dead_cwd.to_string_lossy().into_owned();
    let jsonl = fx.write_transcript("dead-project", &dead_cwd);
    let artifact_dir = fx.session_artifact_dir("dead-project");

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .assert()
        .success()
        .stdout(contains("transcripts"))
        .stdout(contains("dead-project"))
        .stdout(contains("delete"))
        .stdout(contains("dry run"));

    assert!(jsonl.exists(), "dry run must not remove transcript jsonl");
    assert!(
        artifact_dir.exists(),
        "dry run must not remove session artifact dirs"
    );
    assert!(
        fx.claude_home.join("projects/dead-project").exists(),
        "dry run must keep the transcript project dir"
    );
}

#[test]
fn transcripts_apply_removes_dead_artifacts_and_keeps_live_dirs() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let dead_cwd = fx.root.path().join("missing-project");
    let dead_cwd = dead_cwd.to_string_lossy().into_owned();
    let live_cwd = fx.touch_dir("live-project");

    let dead_jsonl = fx.write_transcript("dead-project", &dead_cwd);
    let dead_artifact = fx.session_artifact_dir("dead-project");
    let live_jsonl = fx.write_transcript("live-project", &live_cwd);

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("directory removed"));

    assert!(!dead_jsonl.exists(), "dead transcript jsonl removed");
    assert!(!dead_artifact.exists(), "dead session artifact dir removed");
    assert!(
        !fx.claude_home.join("projects/dead-project").exists(),
        "empty dead transcript project dir removed"
    );
    assert!(live_jsonl.exists(), "live transcript jsonl kept");
    assert!(
        fx.claude_home.join("projects/live-project").exists(),
        "live transcript project dir kept"
    );
    assert!(
        fx.backup_paths().is_empty(),
        "transcript deletion does not create .claude.json backups"
    );
}

#[test]
fn transcripts_apply_preserves_memory_dir() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let dead_cwd = fx.root.path().join("missing-project");
    let dead_cwd = dead_cwd.to_string_lossy().into_owned();

    let jsonl = fx.write_transcript("dead-with-memory", &dead_cwd);
    let project_dir = fx.claude_home.join("projects/dead-with-memory");
    let memory = project_dir.join("memory");
    std::fs::create_dir_all(&memory).unwrap();
    std::fs::write(memory.join("MEMORY.md"), "durable user data").unwrap();

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("memory preserved"));

    assert!(!jsonl.exists(), "transcript jsonl removed");
    assert!(project_dir.exists(), "project dir kept for memory/");
    assert!(memory.join("MEMORY.md").exists(), "memory files preserved");
}

#[test]
fn transcripts_apply_reports_partial_clean_when_unknown_entries_remain() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let dead_cwd = fx.root.path().join("missing-project");
    let dead_cwd = dead_cwd.to_string_lossy().into_owned();

    let jsonl = fx.write_transcript("dead-with-extra", &dead_cwd);
    let project_dir = fx.claude_home.join("projects/dead-with-extra");
    std::fs::write(project_dir.join("notes.txt"), "not ours").unwrap();

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("partially cleaned"));

    assert!(!jsonl.exists(), "transcript jsonl removed");
    assert!(project_dir.join("notes.txt").exists(), "unknown file kept");
    assert!(project_dir.exists(), "project dir kept for unknown entry");
}

#[test]
fn transcripts_disagreement_is_skipped() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let cwd_a = fx
        .root
        .path()
        .join("missing-a")
        .to_string_lossy()
        .into_owned();
    let cwd_b = fx
        .root
        .path()
        .join("missing-b")
        .to_string_lossy()
        .into_owned();
    let first = fx.write_transcript_line(
        "disagree",
        "00000000-0000-4000-8000-000000000001.jsonl",
        &format!("{{\"cwd\":{}}}\n", json!(cwd_a)),
    );
    let second = fx.write_transcript_line(
        "disagree",
        "00000000-0000-4000-8000-000000000002.jsonl",
        &format!("{{\"cwd\":{}}}\n", json!(cwd_b)),
    );

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("cwd-disagreement"));

    assert!(first.exists(), "disagreed transcript kept");
    assert!(second.exists(), "disagreed transcript kept");
}

#[test]
fn transcripts_no_jsonl_dir_is_skipped() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let dir = fx.transcript_project_dir("no-jsonl");
    std::fs::write(dir.join("README.txt"), "not a transcript").unwrap();

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("no-jsonl"));

    assert!(dir.join("README.txt").exists(), "skipped dir left intact");
}

#[cfg(unix)]
#[test]
fn transcripts_stat_failure_is_kept() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let locked = fx.root.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let uncertain = locked.join("missing");
    let jsonl = fx.write_transcript("uncertain", &uncertain.to_string_lossy());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let result = fx
        .cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    result.stdout(contains("0 dead"));
    assert!(
        jsonl.exists(),
        "permission-denied cwd stat is not provably absent"
    );
}

#[test]
fn transcripts_mass_deletion_gate_trips_and_force_overrides() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let live = fx.touch_dir("live-project");
    fx.write_transcript("live-project", &live);
    for i in 0..9 {
        let dead = fx.root.path().join(format!("missing-{i}"));
        fx.write_transcript(&format!("dead-{i}"), &dead.to_string_lossy());
    }

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .assert()
        .failure()
        .stderr(contains("different machine"));
    assert!(
        fx.claude_home.join("projects/dead-0").exists(),
        "gate must refuse before deleting"
    );

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();
    assert!(
        !fx.claude_home.join("projects/dead-0").exists(),
        "--force overrides transcript mass-deletion gate"
    );
    assert!(fx.claude_home.join("projects/live-project").exists());
}

#[test]
fn transcript_gate_refuses_before_config_prune_writes() {
    let fx = Fixture::new();
    let mut projects = serde_json::Map::new();
    for i in 0..4 {
        projects.insert(fx.touch_dir(&format!("live-{i}")), json!({}));
    }
    projects.insert("/config/orphan".to_string(), json!({}));
    fx.write_config(Value::Object(projects), standard_extras());
    let before = std::fs::read_to_string(&fx.config).unwrap();

    for i in 0..4 {
        let dead = fx.root.path().join(format!("missing-transcript-{i}"));
        fx.write_transcript(&format!("dead-{i}"), &dead.to_string_lossy());
    }
    fx.transcript_project_dir("skipped-no-jsonl");

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .assert()
        .code(2)
        .stderr(contains("resolvable transcript dirs"));

    assert_eq!(
        before,
        std::fs::read_to_string(&fx.config).unwrap(),
        "transcript gate must fire before .claude.json is rewritten"
    );
    assert!(
        fx.backup_paths().is_empty(),
        "no backup should be created when upfront gates refuse"
    );
}

#[test]
fn transcripts_apply_prunes_config_and_transcripts_together_without_force() {
    let fx = Fixture::new();
    let mut projects = serde_json::Map::new();
    let mut live_projects = Vec::new();
    for i in 0..4 {
        let live = fx.touch_dir(&format!("live-{i}"));
        projects.insert(
            live.clone(),
            json!({ "lastVisited": "2026-07-04T00:00:00Z" }),
        );
        live_projects.push(live);
    }
    let orphan = fx
        .root
        .path()
        .join("missing-config-project")
        .to_string_lossy()
        .into_owned();
    projects.insert(
        orphan.clone(),
        json!({ "lastVisited": "2026-07-03T00:00:00Z" }),
    );
    fx.write_config(Value::Object(projects), standard_extras());

    let dead_cwd = fx
        .root
        .path()
        .join("missing-transcript-project")
        .to_string_lossy()
        .into_owned();
    let live_cwd = fx.touch_dir("live-transcript-project");
    let dead_jsonl = fx.write_transcript("dead-transcript-project", &dead_cwd);
    let dead_artifact = fx.session_artifact_dir("dead-transcript-project");
    let live_jsonl = fx.write_transcript("live-transcript-project", &live_cwd);

    let mut cmd = fx.cmd();
    cmd.env("MIDDEN_TEST_ASSUME_NO_CLAUDE_PROCESS", &fx.config)
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .assert()
        .success();

    let after = fx.read_config();
    let after_projects = after["projects"].as_object().unwrap();
    assert!(
        !after_projects.contains_key(&orphan),
        "config orphan should be pruned"
    );
    for live in live_projects {
        assert!(
            after_projects.contains_key(&live),
            "live project entry should remain: {live}"
        );
    }

    assert_eq!(
        fx.backup_paths().len(),
        1,
        "combined apply should create exactly the config backup"
    );
    assert!(!dead_jsonl.exists(), "dead transcript jsonl removed");
    assert!(!dead_artifact.exists(), "dead session artifact dir removed");
    assert!(
        !fx.claude_home
            .join("projects/dead-transcript-project")
            .exists(),
        "empty dead transcript project dir removed"
    );
    assert!(live_jsonl.exists(), "live transcript jsonl kept");
    assert!(
        fx.claude_home
            .join("projects/live-transcript-project")
            .exists(),
        "live transcript project dir kept"
    );
}

#[cfg(unix)]
#[test]
fn inaccessible_transcript_dir_is_skipped_without_aborting() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let live = fx.touch_dir("live-project");
    let dead = fx.root.path().join("missing-project");
    fx.write_transcript("live-project", &live);
    fx.write_transcript("dead-project", &dead.to_string_lossy());

    let locked = fx.transcript_project_dir("locked");
    std::fs::write(locked.join("note.txt"), "hidden").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let result = fx
        .cmd()
        .arg("prune")
        .arg("--transcripts")
        .assert()
        .success();

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    result
        .stdout(contains("locked"))
        .stdout(contains("skipped: inaccessible"))
        .stdout(contains("dead-project"))
        .stdout(contains("1 dead"))
        .stdout(contains("1 kept"));
}

#[test]
fn transcripts_worktrees_only_filters_by_derived_cwd() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let normal = fx.root.path().join("missing-normal");
    let worktree = fx
        .root
        .path()
        .join("repo/.claude/worktrees/witty-curie/checkout");
    fx.write_transcript("normal-dead", &normal.to_string_lossy());
    fx.write_transcript("worktree-dead", &worktree.to_string_lossy());

    fx.cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--worktrees-only")
        .arg("--apply")
        .arg("--force")
        .assert()
        .success();

    assert!(
        fx.claude_home.join("projects/normal-dead").exists(),
        "non-worktree derived cwd is filtered out"
    );
    assert!(
        !fx.claude_home.join("projects/worktree-dead").exists(),
        "worktree derived cwd is pruned"
    );
}

#[test]
fn transcripts_worktrees_only_reports_unclassified_skips() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let normal = fx.root.path().join("missing-normal");
    let worktree = fx
        .root
        .path()
        .join("repo/.claude/worktrees/witty-curie/checkout");
    fx.write_transcript("normal-dead", &normal.to_string_lossy());
    fx.write_transcript("worktree-dead", &worktree.to_string_lossy());
    fx.transcript_project_dir("no-jsonl");

    let out = fx
        .cmd()
        .arg("prune")
        .arg("--transcripts")
        .arg("--worktrees-only")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.contains("worktree-dead"), "{stdout}");
    assert!(stdout.contains("no-jsonl"), "{stdout}");
    assert!(stdout.contains("skipped: no-jsonl"), "{stdout}");
    assert!(
        !stdout.contains("normal-dead"),
        "resolvable non-worktree dirs stay filtered: {stdout}"
    );
}

#[test]
fn transcripts_json_output_includes_dir_statuses() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let live = fx.touch_dir("live-project");
    let dead = fx.root.path().join("missing-project");
    fx.write_transcript("live-project", &live);
    fx.write_transcript("dead-project", &dead.to_string_lossy());
    fx.transcript_project_dir("no-jsonl");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("prune")
        .arg("--transcripts")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["transcripts"]["total"], json!(3));
    assert_eq!(v["transcripts"]["kept"], json!(1));
    assert_eq!(v["transcripts"]["dead"], json!(1));
    assert_eq!(v["transcripts"]["skipped"], json!(1));
    assert_eq!(v["transcripts"]["applied"], json!(false));

    let dirs = v["transcripts"]["dirs"].as_array().unwrap();
    assert!(
        dirs.iter()
            .any(|d| d["status"] == json!("dead") && d["delete"].as_array().unwrap().len() == 1)
    );
    assert!(
        dirs.iter()
            .any(|d| d["status"] == json!("skipped") && d["reason"] == json!("no-jsonl"))
    );
}

#[test]
fn transcripts_apply_deletes_exactly_reported_set() {
    let fx = Fixture::new();
    fx.write_config(json!({}), standard_extras());
    let dead = fx.root.path().join("missing-project");
    fx.write_transcript("dead-project", &dead.to_string_lossy());
    fx.session_artifact_dir("dead-project");

    let dry = fx
        .cmd()
        .arg("--json")
        .arg("prune")
        .arg("--transcripts")
        .output()
        .expect("dry run");
    assert!(
        dry.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_json: Value = serde_json::from_slice(&dry.stdout).expect("dry json");
    let mut reported = dry_json["transcripts"]["dirs"][0]["delete"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    reported.sort();

    let applied = fx
        .cmd()
        .arg("--json")
        .arg("prune")
        .arg("--transcripts")
        .arg("--apply")
        .arg("--force")
        .output()
        .expect("apply");
    assert!(
        applied.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied_json: Value = serde_json::from_slice(&applied.stdout).expect("apply json");
    let mut deleted = applied_json["transcripts"]["dirs"][0]["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    deleted.sort();

    assert_eq!(deleted, reported, "--apply deletes the dry-run set");
    assert_eq!(applied_json["transcripts"]["applied"], json!(true));
    assert_eq!(applied_json["backup"], json!(null));
    assert!(fx.backup_paths().is_empty());
}
