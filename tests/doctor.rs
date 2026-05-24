mod common;

use common::Fixture;
use predicates::str::contains;
use serde_json::{Value, json};
use std::path::Path;

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

#[test]
fn detects_orphaned_projects() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(
        json!({
            &live: {},
            "/missing/x": {},
        }),
        json!({}),
    );

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("orphaned-project"))
        .stdout(contains("/missing/x"))
        .stdout(contains("auto-fixable"));
}

#[test]
fn fix_prunes_orphaned_projects() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(
        json!({
            &live: {},
            "/missing/x": {},
        }),
        json!({}),
    );

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .arg("--fix")
        .arg("--force")
        .assert()
        .success();

    let after = fx.read_config();
    let projects = after["projects"].as_object().unwrap();
    assert!(projects.contains_key(&live));
    assert!(!projects.contains_key("/missing/x"));
    assert_eq!(fx.backup_paths().len(), 1);
}

#[test]
fn flags_secrets_in_committed_settings_but_masks_by_default() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({
            "env": { "ANTHROPIC_API_KEY": "sk-very-real-token-abc123" }
        }),
    );

    let out = fx.cmd().arg("doctor").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("secret-in-committed-settings"));
    assert!(stdout.contains("env.ANTHROPIC_API_KEY"));
    assert!(stdout.contains("sk-v***"), "stdout:\n{stdout}");
    assert!(!stdout.contains("sk-very-real-token-abc123"));
}

#[test]
fn show_secrets_unmasks() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({
            "env": { "ANTHROPIC_API_KEY": "sk-very-real-token-abc123" }
        }),
    );

    let out = fx
        .cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .arg("--show-secrets")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sk-very-real-token-abc123"));
}

#[test]
fn flags_missing_credential_deny_rules() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let project_settings = fx.root.path().join(".claude/settings.json");
    // permissions with no credential deny coverage
    write_json(
        &project_settings,
        &json!({ "permissions": { "deny": ["Bash(rm:*)"] } }),
    );

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("missing-credential-deny"));
}

#[test]
fn covered_credential_deny_rules_pass() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({
            "permissions": {
                "deny": ["Read(./.env)", "Read(./.env.*)", "Read(./secrets/**)"]
            }
        }),
    );

    let out = fx.cmd().arg("doctor").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("missing-credential-deny"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn flags_unreachable_mcp_server() {
    let fx = Fixture::new();
    fx.write_config(
        json!({}),
        json!({
            "mcpServers": {
                "broken": {}, // neither command nor url
                "ok": { "command": "node", "args": ["x.js"] }
            }
        }),
    );

    let out = fx.cmd().arg("doctor").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("mcp-server-unreachable"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("broken"));
}

#[test]
fn flags_skill_directory_missing_skill_md() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let skill_dir = fx.root.path().join(".claude/skills/broken-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("README.md"), "hi").unwrap();

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("skill-missing-skill-md"));
}

#[test]
fn json_output_emits_findings_array() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(json!({ &live: {}, "/missing": {} }), json!({}));

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let findings = v["findings"].as_array().unwrap();
    assert!(findings.iter().any(|f| f["id"] == "orphaned-project"));
}

#[test]
fn clean_config_reports_no_findings() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(json!({ &live: {} }), json!({}));
    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({
            "permissions": {
                "deny": ["Read(./.env)", "Read(./.env.*)", "Read(./secrets/**)"]
            }
        }),
    );

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("clean."));
}
