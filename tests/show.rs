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
fn resolved_settings_show_provenance_and_shadowing() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));

    let user_settings = fx.claude_home.join("settings.json");
    write_json(
        &user_settings,
        &json!({ "permissions": { "defaultMode": "ask" } }),
    );

    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({ "permissions": { "defaultMode": "bypass" } }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("permissions.defaultMode = \"bypass\""),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("user shadowed"), "stdout:\n{stdout}");
    assert!(stdout.contains("[project]"), "stdout:\n{stdout}");
}

#[test]
fn array_keys_concat_and_dedupe_across_scopes() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));

    let user_settings = fx.claude_home.join("settings.json");
    write_json(
        &user_settings,
        &json!({ "permissions": { "deny": ["Read(./.env)", "Bash(rm:*)"] } }),
    );

    let project_settings = fx.root.path().join(".claude/settings.json");
    write_json(
        &project_settings,
        &json!({ "permissions": { "deny": ["Read(./.env)", "Read(./secrets/**)"] } }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let entry = v["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "permissions.deny")
        .expect("permissions.deny");
    let eff = entry["effective"].as_array().unwrap();
    assert_eq!(eff.len(), 3, "deduped union");
    assert!(eff.contains(&json!("Read(./.env)")));
    assert!(eff.contains(&json!("Bash(rm:*)")));
    assert!(eff.contains(&json!("Read(./secrets/**)")));
    for c in entry["contributions"].as_array().unwrap() {
        assert_eq!(c["shadowed"], json!(false));
    }
}

#[test]
fn show_masks_secret_settings_by_default() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let user_settings = fx.claude_home.join("settings.json");
    write_json(
        &user_settings,
        &json!({ "env": { "ANTHROPIC_API_KEY": "sk-very-real-token-abc123" } }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("env.ANTHROPIC_API_KEY"));
    assert!(stdout.contains("sk-v***"), "stdout:\n{stdout}");
    assert!(!stdout.contains("sk-very-real-token-abc123"));
}

#[test]
fn show_lists_claude_md_files_and_flags_contradictions() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    std::fs::write(
        fx.claude_home.join("CLAUDE.md"),
        "- Always commit signed.\n",
    )
    .unwrap();
    std::fs::write(fx.root.path().join("CLAUDE.md"), "- Never commit signed.\n").unwrap();

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CLAUDE.md"));
    assert!(stdout.contains("contradictions"), "stdout:\n{stdout}");
}

#[test]
fn show_lists_mcp_servers_with_scope_and_target() {
    let fx = Fixture::new();
    fx.write_config(
        json!({}),
        json!({
            "mcpServers": {
                "user-one": { "command": "node", "args": ["a.js"] }
            }
        }),
    );
    let project_mcp = fx.root.path().join(".mcp.json");
    write_json(
        &project_mcp,
        &json!({ "mcpServers": { "proj-one": { "url": "http://localhost:9999" } } }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("user-one"), "stdout:\n{stdout}");
    assert!(stdout.contains("proj-one"), "stdout:\n{stdout}");
    assert!(stdout.contains("[project]"));
    assert!(stdout.contains("[user]"));
}

#[test]
fn show_lists_skills_with_skill_md() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let skill_dir = fx.root.path().join(".claude/skills/my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "---\nname: my-skill\n---\n").unwrap();

    fx.cmd()
        .arg("show")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("my-skill"));
}

#[test]
fn json_output_emits_root_and_all_sections() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));

    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["root"].is_string());
    for key in [
        "settings",
        "claude_md",
        "contradictions",
        "skills",
        "commands",
        "agents",
        "hooks",
        "mcp_servers",
        "worktrees",
    ] {
        assert!(v.get(key).is_some(), "missing key {key} in JSON output");
    }
}

#[test]
fn hooks_get_their_own_section_with_provenance() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));

    // Hooks at user scope: a Bash PreToolUse hook that blocks --no-verify.
    let user_settings = fx.claude_home.join("settings.json");
    write_json(
        &user_settings,
        &json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {
                                "type": "command",
                                "command": "bash -c 'echo no-verify-checker'"
                            }
                        ]
                    }
                ]
            }
        }),
    );

    // Hooks at local scope: a Stop hook.
    let local_settings = fx.root.path().join(".claude/settings.local.json");
    write_json(
        &local_settings,
        &json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": "say done" }
                        ]
                    }
                ]
            }
        }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("hooks\n"),
        "expected hooks section header\n{stdout}"
    );
    assert!(stdout.contains("PreToolUse"), "stdout:\n{stdout}");
    assert!(stdout.contains("Stop"), "stdout:\n{stdout}");
    assert!(stdout.contains("[user]"), "user-scope tag\n{stdout}");
    assert!(stdout.contains("[local]"), "local-scope tag\n{stdout}");
    assert!(stdout.contains("no-verify-checker"));
    assert!(stdout.contains("say done"));

    // Critically: hooks must NOT also appear inside the settings section as a
    // raw JSON dump — only in the hooks section.
    assert!(
        !stdout.contains("hooks.PreToolUse ="),
        "hooks should not be in settings dump:\n{stdout}"
    );
}

#[test]
fn json_output_includes_hooks_array() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let user_settings = fx.claude_home.join("settings.json");
    write_json(
        &user_settings,
        &json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{ "type": "command", "command": "x" }]
                    }
                ]
            }
        }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let hooks = v["hooks"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["event"], "PreToolUse");
    assert_eq!(hooks[0]["scope"], "user");
    assert_eq!(hooks[0]["matcher"], "Bash");
    assert_eq!(hooks[0]["command"], "x");
}
