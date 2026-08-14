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

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
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
fn show_masks_secret_arrays_by_default() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let user_settings = fx.claude_home.join("settings.json");
    // A sensitive-named key holding an array of secrets must be masked, not
    // printed verbatim (regression guard for the array-masking leak).
    write_json(
        &user_settings,
        &json!({ "apiKeys": ["sk-real-secret-aaaa", "sk-real-secret-bbbb"] }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("apiKeys"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("sk-real-secret-aaaa") && !stdout.contains("sk-real-secret-bbbb"),
        "array secret leaked unmasked:\n{stdout}"
    );
    assert!(stdout.contains("sk-r***"), "stdout:\n{stdout}");
}

#[test]
fn show_masks_token_shaped_values_under_innocent_keys() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let user_settings = fx.claude_home.join("settings.json");
    // Neither `env.EXTRA` nor `model` looks sensitive by name; the first
    // value is masked purely by its token shape, the second left alone. The
    // token is assembled at runtime so the source never holds a
    // scanner-matching literal.
    let token = format!("ghp_{}", "AAAAbbbb1111cccc2222dddd3333eeee");
    write_json(
        &user_settings,
        &json!({
            "env": { "EXTRA": token },
            "model": "opus"
        }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ghp_***"), "stdout:\n{stdout}");
    assert!(!stdout.contains(&token), "token leaked:\n{stdout}");
    assert!(stdout.contains("\"opus\""), "stdout:\n{stdout}");
}

#[test]
fn show_masks_hook_commands_and_mcp_urls() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let user_settings = fx.claude_home.join("settings.json");
    write_json(
        &user_settings,
        &json!({
            "hooks": {
                "Stop": [{ "hooks": [{
                    "type": "command",
                    "command": "curl -H 'Authorization: Bearer SECRETtoken1234567890' https://api.example.com/done"
                }] }]
            }
        }),
    );
    write_json(
        &fx.root.path().join(".mcp.json"),
        &json!({
            "mcpServers": {
                "sse": { "url": "https://mcp.example.com/sse?api_key=verysecret1234&v=2" }
            }
        }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("SECRETtoken1234567890"),
        "bearer token leaked:\n{stdout}"
    );
    assert!(stdout.contains("SECR***"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("verysecret1234"),
        "url query secret leaked:\n{stdout}"
    );
    assert!(stdout.contains("api_key=very***"), "stdout:\n{stdout}");
    assert!(stdout.contains("v=2"), "innocent param survives:\n{stdout}");

    // --show-secrets restores the raw values.
    let out = fx
        .cmd()
        .arg("show")
        .arg(fx.root.path())
        .arg("--show-secrets")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("SECRETtoken1234567890"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("verysecret1234"), "stdout:\n{stdout}");
}

#[test]
fn show_secrets_flag_unmasks() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let user_settings = fx.claude_home.join("settings.json");
    write_json(
        &user_settings,
        &json!({ "apiKeys": ["sk-real-secret-aaaa"] }),
    );

    let out = fx
        .cmd()
        .arg("show")
        .arg(fx.root.path())
        .arg("--show-secrets")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("sk-real-secret-aaaa"),
        "--show-secrets should unmask:\n{stdout}"
    );
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
fn show_lists_local_scope_mcp_servers_from_the_projects_map() {
    let fx = Fixture::new();
    // `claude mcp add` defaults to local scope: the server lands in the
    // project's entry inside ~/.claude.json, not in any settings file. The key
    // is written as the shell reported the cwd, which on macOS may be a
    // non-canonical alias (/var vs /private/var) of the path show resolves.
    let root_key = fx.root.path().to_string_lossy().into_owned();
    fx.write_config(
        json!({
            &root_key: {
                "mcpServers": { "local-one": { "command": "uvx", "args": ["serve"] } },
                "hasTrustDialogAccepted": true
            }
        }),
        json!({ "mcpServers": { "user-one": { "command": "node" } } }),
    );

    let out = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("local-one"), "stdout:\n{stdout}");
    assert!(stdout.contains("[local]"), "stdout:\n{stdout}");
    assert!(stdout.contains("user-one"), "stdout:\n{stdout}");
    assert!(stdout.contains("[user]"), "stdout:\n{stdout}");
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
fn nonexistent_target_path_is_an_error() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.cmd()
        .arg("show")
        .arg("/no/such/target")
        .assert()
        .code(2)
        .stderr(contains("target directory not found"));
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

#[test]
fn show_inventories_commands_agents_and_worktrees() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write(
        &fx.claude_home.join("commands/deploy.md"),
        "Deploy the project.\n",
    );
    write(
        &fx.root.path().join(".claude/commands/nested/review.md"),
        "Review the change.\n",
    );
    write(
        &fx.root
            .path()
            .join(".claude/commands/node_modules/hidden.md"),
        "This vendored command must be ignored.\n",
    );
    write(
        &fx.claude_home.join("agents/researcher.md"),
        "---\nname: researcher\n---\n",
    );
    write(
        &fx.root.path().join(".claude/agents/nested/reviewer.md"),
        "---\nname: reviewer\n---\n",
    );
    write(
        &fx.root.path().join(".claude/agents/ignored.txt"),
        "not markdown\n",
    );
    std::fs::create_dir_all(fx.root.path().join(".claude/worktrees/zesty-newton")).unwrap();
    std::fs::create_dir_all(fx.root.path().join(".claude/worktrees/amber-curie")).unwrap();
    write(
        &fx.root.path().join(".claude/worktrees/not-a-directory"),
        "ignored\n",
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
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let command_names = report["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| {
            (
                command["name"].as_str().unwrap(),
                command["scope"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        command_names,
        vec![("deploy", "user"), ("review", "project")]
    );
    let agent_names = report["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| {
            (
                agent["name"].as_str().unwrap(),
                agent["scope"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        agent_names,
        vec![("researcher", "user"), ("reviewer", "project")]
    );
    let worktree_names = report["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|worktree| worktree["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(worktree_names, vec!["amber-curie", "zesty-newton"]);

    let human = fx.cmd().arg("show").arg(fx.root.path()).output().unwrap();
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    for expected in [
        "deploy",
        "review",
        "researcher",
        "reviewer",
        "amber-curie",
        "zesty-newton",
        "[user]",
        "[project]",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}:\n{stdout}");
    }
    assert!(!stdout.contains("hidden"), "stdout:\n{stdout}");
}
