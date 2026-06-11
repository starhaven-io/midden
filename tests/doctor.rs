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
fn flags_secret_in_array_under_sensitive_key() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let project_settings = fx.root.path().join(".claude/settings.json");
    // A secret stored as a string array under a sensitive-named key.
    write_json(
        &project_settings,
        &json!({ "apiKeys": ["sk-real-committed-secret-aaaa"] }),
    );

    let out = fx.cmd().arg("doctor").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("secret-in-committed-settings"),
        "stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-real-committed-secret-aaaa"),
        "array secret leaked unmasked:\n{stdout}"
    );
    assert!(stdout.contains("sk-r***"), "stdout:\n{stdout}");
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
fn gitignored_settings_json_is_not_flagged_as_committed_secret() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    // A repo that gitignores .claude/ wholesale (midden's own convention).
    std::fs::write(fx.root.path().join(".gitignore"), ".claude/\n").unwrap();
    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({ "env": { "ANTHROPIC_API_KEY": "sk-very-real-token-abc123" } }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&"secret-in-committed-settings"),
        "gitignored settings.json must not be flagged as a committed secret; ids: {ids:?}"
    );
}

#[test]
fn flags_secret_in_committed_mcp_json() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write_json(
        &fx.root.path().join(".mcp.json"),
        &json!({
            "mcpServers": {
                "svc": {
                    "command": "node",
                    "env": { "SERVICE_API_KEY": "sk-mcp-committed-aaaa" }
                }
            }
        }),
    );

    let out = fx.cmd().arg("doctor").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("secret-in-committed-mcp"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("mcpServers.svc.env.SERVICE_API_KEY"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("sk-m***"), "stdout:\n{stdout}");
    assert!(!stdout.contains("sk-mcp-committed-aaaa"), "leak:\n{stdout}");
}

#[test]
fn env_expansion_in_mcp_json_is_not_flagged() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    // The recommended pattern: the committed file references the environment
    // instead of carrying the credential.
    write_json(
        &fx.root.path().join(".mcp.json"),
        &json!({
            "mcpServers": {
                "svc": { "command": "node", "env": { "SERVICE_API_KEY": "${SERVICE_API_KEY}" } }
            }
        }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&"secret-in-committed-mcp"),
        "a ${{VAR}} reference is not a committed secret; ids: {ids:?}"
    );
}

#[test]
fn gitignored_mcp_json_is_not_flagged() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    std::fs::write(fx.root.path().join(".gitignore"), ".mcp.json\n").unwrap();
    write_json(
        &fx.root.path().join(".mcp.json"),
        &json!({
            "mcpServers": {
                "svc": { "command": "node", "env": { "API_KEY": "sk-mcp-ignored-aaaa" } }
            }
        }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&"secret-in-committed-mcp"),
        "gitignored .mcp.json is not committed; ids: {ids:?}"
    );
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
fn flags_unreachable_local_scope_mcp_server() {
    let fx = Fixture::new();
    // Local scope: the server definition lives in the project's entry inside
    // ~/.claude.json, the default destination of `claude mcp add`.
    let root_key = fx.root.path().to_string_lossy().into_owned();
    fx.write_config(
        json!({ &root_key: { "mcpServers": { "broken": {} } } }),
        json!({}),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let finding = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "mcp-server-unreachable")
        .expect("local-scope server should be linted");
    let key_path = finding["location"]["key_path"].as_str().unwrap();
    assert!(
        key_path.starts_with("projects.") && key_path.ends_with(".mcpServers.broken"),
        "key_path should locate the projects entry: {key_path}"
    );
}

#[test]
fn flags_stale_mcpjson_approvals_and_real_disables() {
    let fx = Fixture::new();
    let root_key = fx.root.path().to_string_lossy().into_owned();
    // .mcp.json defines `real`; the project entry approves a server that no
    // longer exists and disables the one that does.
    write_json(
        &fx.root.path().join(".mcp.json"),
        &json!({ "mcpServers": { "real": { "command": "node" } } }),
    );
    fx.write_config(
        json!({
            &root_key: {
                "enabledMcpjsonServers": ["ghost"],
                "disabledMcpjsonServers": ["real"]
            }
        }),
        json!({}),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let findings = v["findings"].as_array().unwrap();
    let stale = findings
        .iter()
        .find(|f| f["id"] == "stale-mcp-approval")
        .expect("ghost approval should be flagged");
    assert!(
        stale["message"].as_str().unwrap().contains("ghost"),
        "message: {}",
        stale["message"]
    );
    let disabled = findings
        .iter()
        .find(|f| f["id"] == "mcp-server-disabled")
        .expect("disabledMcpjsonServers entry should surface");
    assert!(
        disabled["message"].as_str().unwrap().contains("real"),
        "message: {}",
        disabled["message"]
    );
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
fn json_fix_applied_reflects_whether_a_mutation_happened() {
    // Nothing auto-fixable -> fix_applied is false even though --fix was passed.
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(json!({ &live: {} }), json!({}));
    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({ "permissions": { "deny": ["Read(./.env)", "Read(./.env.*)", "Read(./secrets/**)"] } }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .arg("--fix")
        .arg("--force")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["fix_applied"], json!(false), "no auto-fixable findings");

    // An orphan present -> fix_applied is true.
    let fx2 = Fixture::new();
    let live2 = fx2.touch_dir("alive");
    fx2.write_config(json!({ &live2: {}, "/missing": {} }), json!({}));
    let out2 = fx2
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx2.root.path())
        .arg("--fix")
        .arg("--force")
        .output()
        .unwrap();
    let v2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(v2["fix_applied"], json!(true), "orphan pruned");
}

#[test]
fn errors_on_malformed_claude_json() {
    let fx = Fixture::new();
    std::fs::write(&fx.config, "{ not valid json").unwrap();
    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .failure()
        .stderr(contains("parse"));
}

#[test]
fn skips_malformed_settings_json_but_still_runs_other_checks() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(json!({ &live: {}, "/missing": {} }), json!({}));
    // A malformed project settings.json must not abort doctor; the orphan
    // finding still surfaces.
    let project_settings = fx.root.path().join(".claude/settings.json");
    std::fs::create_dir_all(project_settings.parent().unwrap()).unwrap();
    std::fs::write(&project_settings, "{ not valid").unwrap();

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("orphaned-project"));
}

#[test]
fn fix_with_nothing_auto_fixable_writes_nothing() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(json!({ &live: {} }), json!({}));
    // The only issue is a secret (Error, not auto-fixable).
    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({ "env": { "API_KEY": "sk-realsecret-aaaa" } }),
    );

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .arg("--fix")
        .arg("--force")
        .assert()
        .failure() // the secret finding is an Error -> exit 1
        .stdout(contains("nothing auto-fixable"));
    assert!(
        fx.backup_paths().is_empty(),
        "no backup when nothing is auto-fixable"
    );
}

#[test]
fn fix_is_idempotent() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    fx.write_config(json!({ &live: {}, "/missing": {} }), json!({}));

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .arg("--fix")
        .arg("--force")
        .assert()
        .success();
    assert_eq!(fx.backup_paths().len(), 1);

    // Second run: the orphan is gone, so nothing is auto-fixable and no new
    // backup is written.
    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .arg("--fix")
        .arg("--force")
        .assert()
        .success()
        .stdout(contains("nothing auto-fixable"));
    assert_eq!(fx.backup_paths().len(), 1, "no second backup");
}

#[test]
fn flags_empty_command_file() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let cmd_dir = fx.root.path().join(".claude/commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();
    std::fs::write(cmd_dir.join("empty.md"), "").unwrap();

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("empty-config-file"));
}

#[test]
fn flags_disabled_mcp_server_distinctly_from_unreachable() {
    let fx = Fixture::new();
    fx.write_config(
        json!({}),
        json!({ "mcpServers": { "off": { "command": "node", "disabled": true } } }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(ids.contains(&"mcp-server-disabled"), "ids: {ids:?}");
    assert!(
        !ids.contains(&"mcp-server-unreachable"),
        "has a command, so not unreachable; ids: {ids:?}"
    );
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
fn bloat_finding_names_the_largest_key() {
    let fx = Fixture::new();
    let live = fx.touch_dir("alive");
    // A dominant cache blob pushes the file past the bloat threshold; the
    // finding should name the culprit, not just report a size.
    let blob = "x".repeat(600 * 1024);
    fx.write_config(
        json!({ &live: {} }),
        json!({ "cachedGrowthBookFeatures": blob }),
    );

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
    let bloat = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "claude-json-bloat")
        .expect("expected a claude-json-bloat finding");
    let msg = bloat["message"].as_str().unwrap();
    assert!(msg.contains("cachedGrowthBookFeatures"), "message: {msg}");
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

#[test]
fn flags_tracked_local_settings() {
    let fx = Fixture::new();
    fx.git(&["init", "--quiet"]);
    write_json(
        &fx.root.path().join(".claude/settings.local.json"),
        &json!({ "env": { "GITHUB_TOKEN": "ghp_x" } }),
    );
    fx.git(&["add", ".claude/settings.local.json"]);

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("local-settings-tracked"));
}

#[test]
fn flags_local_settings_not_gitignored() {
    let fx = Fixture::new();
    fx.git(&["init", "--quiet"]);
    write_json(
        &fx.root.path().join(".claude/settings.local.json"),
        &json!({ "permissions": { "defaultMode": "bypassPermissions" } }),
    );

    fx.cmd()
        .arg("doctor")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("local-settings-not-ignored"));
}

#[test]
fn gitignored_local_settings_is_clean() {
    let fx = Fixture::new();
    fx.git(&["init", "--quiet"]);
    std::fs::write(
        fx.root.path().join(".gitignore"),
        ".claude/settings.local.json\n",
    )
    .unwrap();
    write_json(
        &fx.root.path().join(".claude/settings.local.json"),
        &json!({ "permissions": { "defaultMode": "bypassPermissions" } }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("doctor")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(
        !ids.iter().any(|id| id.starts_with("local-settings")),
        "gitignored local settings should not be flagged; ids: {ids:?}"
    );
}
