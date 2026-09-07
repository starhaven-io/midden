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
fn malformed_settings_are_an_explicit_error() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let settings = fx.root.path().join(".claude/settings.json");
    write(&settings, "{ not json");

    fx.cmd()
        .arg("show")
        .arg(fx.root.path())
        .assert()
        .code(2)
        .stderr(contains(settings.display().to_string()))
        .stderr(contains("parse"));
}

#[test]
fn malformed_mcp_configuration_is_an_explicit_error() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let mcp = fx.root.path().join(".mcp.json");
    write(&mcp, "[");

    fx.cmd()
        .arg("show")
        .arg(fx.root.path())
        .assert()
        .code(2)
        .stderr(contains(mcp.display().to_string()))
        .stderr(contains("parse"));
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
fn show_masks_sensitive_object_keys_nested_in_arrays() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write_json(
        &fx.claude_home.join("settings.json"),
        &json!({"plugins": [{"password": "hunter2", "name": "safe"}]}),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !stdout.contains("hunter2"),
        "nested secret leaked:\n{stdout}"
    );
    assert!(stdout.contains("hunt***"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("safe"),
        "innocent sibling missing:\n{stdout}"
    );
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
fn show_secrets_does_not_disable_terminal_escaping() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write_json(
        &fx.claude_home.join("settings.json"),
        &json!({
            "hooks": {
                "Stop": [{ "hooks": [{
                    "type": "command",
                    "command": "echo \u{1b}]8;;https://example.test\u{7}link\u{202e}"
                }] }]
            }
        }),
    );

    let out = fx
        .cmd()
        .arg("show")
        .arg(fx.root.path())
        .arg("--show-secrets")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\\u{1b}"), "stdout:\n{stdout}");
    assert!(stdout.contains("\\u{7}"), "stdout:\n{stdout}");
    assert!(stdout.contains("\\u{202e}"), "stdout:\n{stdout}");
    assert!(!stdout.contains('\u{1b}'), "raw escape leaked:\n{stdout}");
    assert!(
        !stdout.contains('\u{202e}'),
        "raw bidi control leaked:\n{stdout}"
    );
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
fn claude_md_size_limit_matches_provider_boundary() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let instructions = fx.root.path().join("CLAUDE.md");
    std::fs::write(&instructions, vec![b'x'; 4 * 1024 * 1024]).unwrap();
    // show canonicalizes its target, and a macOS temp root is reached through
    // a symlink (/var -> /private/var), so compare against the resolved path.
    let instructions = instructions.canonicalize().unwrap();

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
    let source = report["claude_md"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["file"] == instructions.display().to_string())
        .unwrap();
    assert_eq!(source["load_state"], "loaded");

    std::fs::write(&instructions, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let source = report["claude_md"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["file"] == instructions.display().to_string())
        .unwrap();
    assert_eq!(source["load_state"], "disabled");
    assert!(
        source["detail"]
            .as_str()
            .unwrap()
            .contains("skipped by Claude")
    );
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
fn identical_hook_groups_are_deduplicated_across_scopes() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let group = json!([{
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "echo once" }]
    }]);
    write_json(
        &fx.claude_home.join("settings.json"),
        &json!({ "hooks": { "PreToolUse": group.clone() } }),
    );
    write_json(
        &fx.root.path().join(".claude/settings.json"),
        &json!({ "hooks": { "PreToolUse": group } }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(report["hooks"][0]["scope"], "user");
}

#[test]
fn hook_deduplication_preserves_unique_handlers_in_overlapping_groups() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write_json(
        &fx.claude_home.join("settings.json"),
        &json!({
            "hooks": { "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [
                    { "type": "command", "command": "echo shared" },
                    { "type": "command", "command": "echo user" }
                ]
            }] }
        }),
    );
    write_json(
        &fx.root.path().join(".claude/settings.json"),
        &json!({
            "hooks": { "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [
                    { "type": "command", "command": "echo shared" },
                    { "type": "command", "command": "echo project" }
                ]
            }] }
        }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let commands = report["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["command"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(commands.len(), 3);
    assert_eq!(
        commands
            .iter()
            .filter(|command| **command == "echo shared")
            .count(),
        1
    );
    assert!(commands.contains(&"echo user"));
    assert!(commands.contains(&"echo project"));
}

#[test]
fn identical_handlers_are_deduplicated_across_overlapping_matchers_and_metadata() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write_json(
        &fx.claude_home.join("settings.json"),
        &json!({
            "hooks": { "PreToolUse": [{
                "matcher": "Bash|Read",
                "hooks": [{
                    "type": "command",
                    "command": "echo shared",
                    "timeout": 10
                }]
            }] }
        }),
    );
    write_json(
        &fx.root.path().join(".claude/settings.json"),
        &json!({
            "hooks": { "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "echo shared",
                    "timeout": 30,
                    "statusMessage": "Running"
                }]
            }] }
        }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(report["hooks"][0]["matcher"], "Bash|Read");
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

#[test]
fn all_hook_kinds_retain_masked_definitions_and_useful_human_summaries() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let definitions = json!([
        {"type": "command", "command": "echo check", "timeout": 30},
        {"type": "http", "url": "https://example.com/check", "headers": {"Authorization": "Bearer example-private-value"}},
        {"type": "prompt", "prompt": "Check the release summary"},
        {"type": "agent", "prompt": "Review changed files"},
        {"type": "mcp_tool", "server": "reviewer", "tool": "check", "input": {"token": "example-private-value"}}
    ]);
    write_json(
        &fx.claude_home.join("settings.json"),
        &json!({
            "hooks": {"Stop": [{"hooks": definitions}]}
        }),
    );
    let output = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let hooks = report["hooks"].as_array().unwrap();
    assert_eq!(hooks.len(), 5);
    assert_eq!(hooks[0]["command"], "echo check");
    assert_eq!(hooks[0]["definition"]["timeout"], 30);
    assert_eq!(hooks[1]["definition"]["url"], "https://example.com/check");
    assert_eq!(
        hooks[2]["definition"]["prompt"],
        "Check the release summary"
    );
    assert_eq!(hooks[3]["definition"]["prompt"], "Review changed files");
    assert_eq!(hooks[4]["definition"]["tool"], "check");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("example-private-value"));
    assert!(
        !report["settings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"].as_str().unwrap().starts_with("hooks."))
    );
    fx.cmd()
        .arg("show")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("https://example.com/check"))
        .stdout(contains("Check the release summary"))
        .stdout(contains("Review changed files"))
        .stdout(contains("reviewer"));
    fx.cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .arg("--show-secrets")
        .assert()
        .success()
        .stdout(contains("example-private-value"));
}

#[test]
fn central_mcp_state_uses_the_same_read_budget_as_other_commands() {
    let fx = Fixture::new();
    fx.write_config(
        json!({}),
        json!({
            "history": "x".repeat(8 * 1024 * 1024),
            "mcpServers": {"example": {"command": "example-server"}}
        }),
    );
    fx.cmd()
        .arg("--json")
        .arg("show")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("example-server"));
}

#[cfg(target_os = "linux")]
#[test]
fn json_show_accepts_non_unicode_project_paths() {
    use std::os::unix::ffi::OsStringExt;
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let root = fx
        .root
        .path()
        .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
    std::fs::create_dir(&root).unwrap();
    write(&root.join("CLAUDE.md"), "# Instructions\n");
    write_json(
        &root.join(".claude/settings.json"),
        &json!({"model": "example"}),
    );
    let output = fx
        .cmd()
        .arg("--json")
        .arg("show")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["root"],
        root.canonicalize().unwrap().display().to_string()
    );
    assert!(!report["settings"].as_array().unwrap().is_empty());
}

#[test]
fn contradiction_inventory_deduplicates_and_reports_its_limit() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write(
        &fx.claude_home.join("CLAUDE.md"),
        "Always use tabs in all examples.\nAlways use tabs in all examples.\n",
    );
    write(
        &fx.root.path().join("CLAUDE.md"),
        "Never use tabs in all examples.\nNever use tabs in all examples.\n",
    );
    let output = fx
        .cmd()
        .args(["--json", "show"])
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["contradictions"].as_array().unwrap().len(), 1);
    assert_eq!(report["contradictions_truncated"], false);

    let lines = (0..140)
        .map(|index| format!("Never use tabs in example {index}.\n"))
        .collect::<String>();
    write(&fx.root.path().join("CLAUDE.md"), &lines);
    let output = fx
        .cmd()
        .args(["--json", "show"])
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["contradictions"].as_array().unwrap().len(), 128);
    assert_eq!(report["contradictions_truncated"], true);
    fx.cmd()
        .arg("show")
        .arg(fx.root.path())
        .assert()
        .success()
        .stdout(contains("additional contradictions omitted"));
}

#[cfg(unix)]
#[test]
fn inaccessible_optional_inventory_warns_without_corrupting_json() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.cmd()
        .args(["--json", "show"])
        .arg(fx.root.path())
        .assert()
        .success()
        .stderr("");
    let blocked = fx.claude_home.join("commands/nested\nfolder");
    std::fs::create_dir_all(&blocked).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let output = fx
        .cmd()
        .args(["--json", "show"])
        .arg(fx.root.path())
        .output()
        .unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _: Value = serde_json::from_slice(&output.stdout).unwrap();
    let warnings = String::from_utf8_lossy(&output.stderr);
    assert!(
        warnings.contains("warning: could not inspect"),
        "{warnings}"
    );
    assert!(warnings.contains("nested\\nfolder"), "{warnings}");
    assert!(!warnings.contains("nested\nfolder"));
}

#[test]
fn hook_and_mcp_definition_commands_share_argument_masking() {
    let fx = Fixture::new();
    fx.write_config(
        json!({}),
        json!({
            "mcpServers": {"example": {"command": "server --token example-private-value"}}
        }),
    );
    write_json(
        &fx.claude_home.join("settings.json"),
        &json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "check --token example-private-value"}]}]}
        }),
    );
    let output = fx
        .cmd()
        .args(["--json", "show"])
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["hooks"][0]["definition"]["command"],
        report["hooks"][0]["command"]
    );
    assert_eq!(
        report["mcp_servers"][0]["definition"]["command"],
        report["mcp_servers"][0]["command"]
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("example-private-value"));
}
