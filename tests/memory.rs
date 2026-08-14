mod common;

use common::Fixture;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn write(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn write_json(path: impl AsRef<Path>, value: Value) {
    write(path, &serde_json::to_string_pretty(&value).unwrap());
}

fn provider<'a>(inventory: &'a Value, name: &str) -> &'a Value {
    inventory["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["provider"] == name)
        .unwrap()
}

fn source<'a>(provider: &'a Value, path: &Path) -> &'a Value {
    let original = path.display().to_string();
    let canonical = path
        .canonicalize()
        .ok()
        .map(|path| path.display().to_string());
    provider["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| {
            source["path"] == original
                || canonical
                    .as_ref()
                    .is_some_and(|canonical| source["path"] == *canonical)
                || canonical.as_ref().is_some_and(|canonical| {
                    source["path"]
                        .as_str()
                        .and_then(|stored| Path::new(stored).canonicalize().ok())
                        .is_some_and(|stored| stored.display().to_string() == *canonical)
                })
        })
        .unwrap_or_else(|| panic!("source not found: {original}"))
}

fn paired_fixture() -> (Fixture, PathBuf, PathBuf) {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    let root = fx.root.path().canonicalize().unwrap();

    write(root.join("AGENTS.md"), "# Shared instructions\n");
    write(root.join("CLAUDE.md"), "@AGENTS.md\n# Claude\n");
    write(
        fx.codex_home.join("config.toml"),
        "[features]\nmemories = true\n\n[memories]\nuse_memories = true\n",
    );
    let codex_memory = fx.codex_home.join("memories");
    write(
        codex_memory.join("memory_summary.md"),
        "# Current summary\n",
    );
    write(codex_memory.join("MEMORY.md"), "# Durable entries\n");
    write(codex_memory.join("raw_memories.md"), "# Evidence\n");

    write_json(
        fx.claude_home.join("settings.json"),
        json!({ "autoMemoryEnabled": true }),
    );
    let slug = "paired-project";
    fx.write_transcript(slug, &root.display().to_string());
    let claude_memory = fx.transcript_project_dir(slug).join("memory");
    write(claude_memory.join("MEMORY.md"), "# Project index\n");
    write(claude_memory.join("debugging.md"), "# Debugging\n");

    (fx, codex_memory, claude_memory)
}

#[test]
fn json_inventory_has_dual_provider_parity() {
    let (fx, codex_memory, claude_memory) = paired_fixture();
    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(inventory["providers"].as_array().unwrap().len(), 2);

    let codex = provider(&inventory, "codex");
    let claude = provider(&inventory, "claude");
    for provider in [codex, claude] {
        assert_eq!(provider["memory_state"], "enabled");
        assert_eq!(
            provider["capabilities"]["instruction_inventory"],
            "supported"
        );
        assert_eq!(provider["capabilities"]["memory_inventory"], "supported");
        assert_eq!(provider["capabilities"]["management"], "read-only");
    }

    let codex_instruction = source(codex, &fx.root.path().join("AGENTS.md"));
    assert_eq!(codex_instruction["role"], "authority");
    assert_eq!(codex_instruction["association"], "target");
    assert_eq!(codex_instruction["load_state"], "loaded");
    let codex_summary = source(codex, &codex_memory.join("memory_summary.md"));
    assert_eq!(codex_summary["kind"], "memory-summary");
    assert_eq!(codex_summary["load_state"], "loaded");

    let claude_instruction = source(claude, &fx.root.path().join("CLAUDE.md"));
    assert_eq!(claude_instruction["role"], "authority");
    let shared_import = source(claude, &fx.root.path().join("AGENTS.md"));
    assert_eq!(shared_import["kind"], "imported-instruction");
    let claude_index = source(claude, &claude_memory.join("MEMORY.md"));
    assert_eq!(claude_index["kind"], "memory-index");
    assert_eq!(claude_index["load_state"], "loaded");
    let claude_topic = source(claude, &claude_memory.join("debugging.md"));
    assert_eq!(claude_topic["kind"], "memory-topic");
    assert_eq!(claude_topic["load_state"], "on-demand");
    assert!(
        claude_topic["detail"]
            .as_str()
            .unwrap()
            .contains("associated cwd:")
    );
}

#[test]
fn provider_filter_keeps_the_same_schema() {
    let (fx, _, _) = paired_fixture();
    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let providers = inventory["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["provider"], "claude");
    assert!(providers[0]["capabilities"].is_object());
    assert!(providers[0]["sources"].is_array());
    assert!(providers[0]["warnings"].is_array());
}

#[test]
fn inventories_provider_native_instruction_loading() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    let nested = fx.root.path().join("crates/api");
    std::fs::create_dir_all(&nested).unwrap();

    write(fx.codex_home.join("AGENTS.md"), "ignored global\n");
    write(fx.codex_home.join("AGENTS.override.md"), "active global\n");
    write(
        fx.codex_home.join("config.toml"),
        "project_doc_fallback_filenames = [\"TEAM.md\"]\n",
    );
    write(fx.root.path().join("TEAM.md"), "repository fallback\n");
    write(nested.join("AGENTS.override.md"), "path override\n");

    write(
        fx.root.path().join("CLAUDE.md"),
        "repository instructions\n",
    );
    write(nested.join("CLAUDE.local.md"), "path instructions\n");
    write(
        fx.root.path().join(".claude/rules/api.md"),
        "---\npaths:\n  - crates/api/**\n---\nAPI rule\n",
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(&nested)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let codex = provider(&inventory, "codex");
    let claude = provider(&inventory, "claude");

    assert!(
        codex["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != fx.codex_home.join("AGENTS.md").display().to_string())
    );
    assert_eq!(
        source(codex, &fx.codex_home.join("AGENTS.override.md"))["scope"],
        "global"
    );
    assert_eq!(
        source(codex, &fx.root.path().join("TEAM.md"))["scope"],
        "repository"
    );
    assert_eq!(
        source(codex, &nested.join("AGENTS.override.md"))["scope"],
        "path"
    );

    assert_eq!(
        source(claude, &fx.root.path().join("CLAUDE.md"))["scope"],
        "repository"
    );
    assert_eq!(
        source(claude, &nested.join("CLAUDE.local.md"))["scope"],
        "path"
    );
    assert_eq!(
        source(claude, &fx.root.path().join(".claude/rules/api.md"))["load_state"],
        "on-demand"
    );
}

#[test]
fn codex_instruction_budget_reports_truncated_and_disabled_sources() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    let nested = fx.root.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    write(
        fx.codex_home.join("config.toml"),
        "project_doc_max_bytes = 5\n",
    );
    write(fx.codex_home.join("AGENTS.md"), "abc");
    let repository = fx.root.path().join("AGENTS.md");
    write(&repository, "12345");
    let path = nested.join("AGENTS.md");
    write(&path, "x");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(&nested)
        .arg("--provider")
        .arg("codex")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let codex = provider(&inventory, "codex");
    assert_eq!(source(codex, &repository)["load_state"], "truncated");
    assert!(
        source(codex, &repository)["detail"]
            .as_str()
            .unwrap()
            .contains("2 of 5 bytes")
    );
    assert_eq!(source(codex, &path)["load_state"], "disabled");
}

#[test]
fn disabled_memory_is_visible_for_both_providers() {
    let (fx, codex_memory, claude_memory) = paired_fixture();
    write(
        fx.codex_home.join("config.toml"),
        "[features]\nmemories = false\n",
    );
    write_json(
        fx.claude_home.join("settings.json"),
        json!({ "autoMemoryEnabled": false }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let codex = provider(&inventory, "codex");
    let claude = provider(&inventory, "claude");
    assert_eq!(codex["memory_state"], "disabled");
    assert_eq!(claude["memory_state"], "disabled");
    assert_eq!(
        source(codex, &codex_memory.join("memory_summary.md"))["load_state"],
        "disabled"
    );
    assert_eq!(
        source(claude, &claude_memory.join("MEMORY.md"))["load_state"],
        "disabled"
    );
}

#[test]
fn claude_memory_settings_use_provider_scope_rules() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let user_memory = fx.root.path().join("user-memory");
    let project_memory = fx.root.path().join("project-memory");
    let local_memory = fx.root.path().join("local-memory");
    write_json(
        fx.claude_home.join("settings.json"),
        json!({
            "autoMemoryEnabled": true,
            "autoMemoryDirectory": user_memory
        }),
    );
    write_json(
        fx.root.path().join(".claude/settings.json"),
        json!({ "autoMemoryDirectory": project_memory }),
    );
    write_json(
        fx.root.path().join(".claude/settings.local.json"),
        json!({
            "autoMemoryEnabled": false,
            "autoMemoryDirectory": local_memory
        }),
    );
    let local_index = fx.root.path().join("local-memory/MEMORY.md");
    let user_index = fx.root.path().join("user-memory/MEMORY.md");
    let project_index = fx.root.path().join("project-memory/MEMORY.md");
    write(&local_index, "local memory\n");
    write(&user_index, "user memory\n");
    write(&project_index, "project memory\n");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let claude = provider(&inventory, "claude");
    assert_eq!(claude["memory_state"], "disabled");
    assert_eq!(source(claude, &user_index)["load_state"], "disabled");
    assert!(claude["sources"].as_array().unwrap().iter().all(|source| {
        source["path"] != local_index.display().to_string()
            && source["path"] != project_index.display().to_string()
    }));
    assert!(
        claude["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "unsupported-auto-memory-directory-scope")
    );
}

#[test]
fn malformed_provider_configuration_is_visible() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write(fx.codex_home.join("config.toml"), "[features\n");
    write(fx.claude_home.join("settings.json"), "{");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let codex = provider(&inventory, "codex");
    let claude = provider(&inventory, "claude");
    assert_eq!(codex["memory_state"], "unknown");
    assert_eq!(claude["memory_state"], "unknown");
    assert!(
        codex["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "invalid-codex-config")
    );
    assert!(
        claude["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "invalid-claude-settings")
    );
}

#[test]
fn invalid_claude_memory_setting_types_and_exclusions_are_explicit() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let claude_md = fx.root.path().join("CLAUDE.md");
    write(&claude_md, "Project instructions.\n");
    write_json(
        fx.claude_home.join("settings.json"),
        json!({
            "autoMemoryEnabled": "yes",
            "autoMemoryDirectory": 42,
            "claudeMdExcludes": [42, "[", "**/CLAUDE.md"]
        }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let claude = provider(&inventory, "claude");
    assert_eq!(claude["memory_state"], "unknown");
    let warning_codes = claude["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|warning| warning["code"].as_str())
        .collect::<Vec<_>>();
    assert!(warning_codes.contains(&"invalid-auto-memory-enabled"));
    assert!(warning_codes.contains(&"invalid-auto-memory-directory"));
    assert_eq!(
        warning_codes
            .iter()
            .filter(|code| **code == "invalid-claude-md-exclude")
            .count(),
        2
    );
    let instruction = source(claude, &claude_md);
    assert_eq!(instruction["load_state"], "disabled");
    assert_eq!(instruction["detail"], "excluded by claudeMdExcludes");
}

#[test]
fn relative_claude_memory_directory_and_non_array_exclusions_are_rejected() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write_json(
        fx.claude_home.join("settings.json"),
        json!({
            "autoMemoryDirectory": "relative/memory",
            "claudeMdExcludes": "**/CLAUDE.md"
        }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let warning_codes = provider(&inventory, "claude")["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|warning| warning["code"].as_str())
        .collect::<Vec<_>>();
    assert!(warning_codes.contains(&"invalid-auto-memory-directory"));
    assert!(warning_codes.contains(&"invalid-claude-md-excludes"));
}

#[test]
fn claude_exclusions_and_external_imports_preserve_unknown_state() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let external = fx.codex_home.join("shared-instructions.md");
    write(&external, "external\n");
    write(
        fx.root.path().join("CLAUDE.md"),
        &format!("@{}\n", external.display()),
    );
    let excluded = fx.root.path().join(".claude/rules/ignored.md");
    write(&excluded, "ignored\n");
    write_json(
        fx.root.path().join(".claude/settings.json"),
        json!({ "claudeMdExcludes": ["**/ignored.md"] }),
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let claude = provider(&inventory, "claude");
    let external = source(claude, &external);
    assert_eq!(external["kind"], "imported-instruction");
    assert_eq!(external["load_state"], "unknown");
    assert!(
        external["detail"]
            .as_str()
            .unwrap()
            .contains("approval is unresolved")
    );
    let excluded = source(claude, &excluded);
    assert_eq!(excluded["load_state"], "disabled");
    assert_eq!(excluded["detail"], "excluded by claudeMdExcludes");
}

#[test]
fn intra_repository_imports_use_the_repository_as_the_trust_root() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    let nested = fx.root.path().join("subdirectory");
    write(nested.join("CLAUDE.md"), "@../shared.md\n");
    let shared = fx.root.path().join("shared.md");
    write(&shared, "shared repository instructions\n");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(&nested)
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let imported = source(provider(&inventory, "claude"), &shared);
    assert_eq!(imported["load_state"], "loaded");
    assert!(
        !imported["detail"]
            .as_str()
            .unwrap()
            .contains("approval is unresolved")
    );
}

#[test]
fn claude_imports_stop_after_five_hops() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    write(fx.root.path().join("CLAUDE.md"), "@imports/1.md\n");
    for index in 1..=6 {
        let contents = if index == 6 {
            "end\n".to_string()
        } else {
            format!("@{}.md\n", index + 1)
        };
        write(
            fx.root.path().join(format!("imports/{index}.md")),
            &contents,
        );
    }

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let claude = provider(&inventory, "claude");
    for index in 1..=5 {
        assert_eq!(
            source(claude, &fx.root.path().join(format!("imports/{index}.md")))["load_state"],
            "loaded"
        );
    }
    let sixth = fx.root.path().join("imports/6.md").display().to_string();
    assert!(
        claude["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != sixth)
    );
    assert!(
        claude["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "claude-import-depth-exceeded")
    );
}

#[test]
fn claude_import_cycles_do_not_duplicate_sources() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let root = fx.root.path().join("CLAUDE.md");
    let imported = fx.root.path().join("imports/cycle.md");
    write(&root, "@imports/cycle.md\n");
    write(&imported, "@../CLAUDE.md\n");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let sources = provider(&inventory, "claude")["sources"]
        .as_array()
        .unwrap();
    let root = root.canonicalize().unwrap().display().to_string();
    let imported = imported.canonicalize().unwrap().display().to_string();
    assert_eq!(
        sources
            .iter()
            .filter(|source| source["path"] == root)
            .count(),
        1
    );
    assert_eq!(
        sources
            .iter()
            .filter(|source| source["path"] == imported)
            .count(),
        1
    );
}

#[test]
fn directly_loaded_claude_md_is_not_duplicated_when_ancestor_imports_it() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    let nested = fx.root.path().join("nested");
    let nested_instructions = nested.join("CLAUDE.md");
    write(fx.root.path().join("CLAUDE.md"), "@nested/CLAUDE.md\n");
    write(&nested_instructions, "nested instructions\n");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(&nested)
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let sources = provider(&inventory, "claude")["sources"]
        .as_array()
        .unwrap();
    let nested_instructions = nested_instructions
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let matches = sources
        .iter()
        .filter(|source| source["path"] == nested_instructions)
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["kind"], "instruction");
    assert!(
        matches[0]["detail"]
            .as_str()
            .unwrap()
            .contains("also imported by")
    );
}

#[cfg(unix)]
#[test]
fn claude_rules_follow_provider_supported_symlinks() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let shared_rules = fx.codex_home.join("shared-rules");
    write(shared_rules.join("shared.md"), "shared rule\n");
    let rules = fx.root.path().join(".claude/rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::os::unix::fs::symlink(&shared_rules, rules.join("shared")).unwrap();

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let linked_rule = rules.join("shared/shared.md");
    assert_eq!(
        source(provider(&inventory, "claude"), &linked_rule)["load_state"],
        "loaded"
    );
}

#[test]
fn all_includes_unassociated_claude_memory_without_decoding_slugs() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let target_slug = "not-a-reversible-path";
    fx.write_transcript(target_slug, &fx.root.path().display().to_string());
    let target_memory = fx
        .transcript_project_dir(target_slug)
        .join("memory/MEMORY.md");
    write(&target_memory, "target\n");
    let unknown_memory = fx
        .transcript_project_dir("another-lossy-slug")
        .join("memory/MEMORY.md");
    write(&unknown_memory, "unknown\n");

    let base = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    let base: Value = serde_json::from_slice(&base.stdout).unwrap();
    let claude = provider(&base, "claude");
    assert!(
        claude["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["path"] == target_memory.display().to_string())
    );
    assert!(
        claude["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != unknown_memory.display().to_string())
    );

    let all = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .arg("--all")
        .output()
        .unwrap();
    let all: Value = serde_json::from_slice(&all.stdout).unwrap();
    let claude = provider(&all, "claude");
    assert_eq!(source(claude, &unknown_memory)["association"], "unknown");

    let human = fx
        .cmd()
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .arg("--all")
        .output()
        .unwrap();
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert_eq!(
        stdout
            .matches("association unknown: no transcript cwd evidence")
            .count(),
        1,
        "{stdout}"
    );
}

#[test]
fn claude_memory_association_unifies_real_git_worktrees() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    write(fx.root.path().join("README.md"), "fixture\n");
    fx.git(&["add", "README.md"]);
    fx.git(&[
        "-c",
        "user.name=Midden Tests",
        "-c",
        "user.email=midden@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
    ]);
    let worktree_parent = tempfile::tempdir().unwrap();
    let worktree = worktree_parent.path().join("checkout");
    fx.git(&["worktree", "add", "--quiet", worktree.to_str().unwrap()]);
    assert!(worktree.join(".git").is_file());

    fx.write_transcript("linked-worktree", &worktree.display().to_string());
    let memory = fx
        .transcript_project_dir("linked-worktree")
        .join("memory/MEMORY.md");
    write(&memory, "shared worktree memory\n");
    write(
        memory.parent().unwrap().join("topic.md"),
        "worktree topic\n",
    );

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        source(provider(&inventory, "claude"), &memory)["association"],
        "target"
    );

    let human = fx
        .cmd()
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert_eq!(stdout.matches("associated cwd:").count(), 1, "{stdout}");
    assert!(stdout.contains(&worktree.display().to_string()), "{stdout}");
}

#[test]
fn human_memory_association_uses_first_source_without_an_index() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    let nested = fx.root.path().join("crates/api");
    std::fs::create_dir_all(&nested).unwrap();
    fx.write_transcript("nested-cwd", &nested.display().to_string());
    let topic = fx
        .transcript_project_dir("nested-cwd")
        .join("memory/topic.md");
    write(&topic, "nested topic\n");

    let out = fx
        .cmd()
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&topic.display().to_string()), "{stdout}");
    assert_eq!(stdout.matches("associated cwd:").count(), 1, "{stdout}");
    assert!(stdout.contains(&nested.display().to_string()), "{stdout}");
}

#[test]
fn claude_memory_association_identifies_a_different_repository() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    fx.git(&["init", "--quiet"]);
    let other = tempfile::tempdir().unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(other.path())
        .args(["init", "--quiet"])
        .output()
        .unwrap();
    fx.write_transcript("other-repository", &other.path().display().to_string());
    let memory = fx
        .transcript_project_dir("other-repository")
        .join("memory/MEMORY.md");
    write(&memory, "other repository memory\n");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .arg("--all")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        source(provider(&inventory, "claude"), &memory)["association"],
        "other"
    );
}

#[test]
fn default_view_warns_when_memory_has_no_transcript_association() {
    let fx = Fixture::new();
    fx.write_config(json!({}), json!({}));
    let memory = fx
        .transcript_project_dir("transcripts-were-pruned")
        .join("memory/MEMORY.md");
    write(&memory, "preserved memory\n");

    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("claude")
        .output()
        .unwrap();
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let claude = provider(&inventory, "claude");
    assert!(
        claude["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != memory.display().to_string())
    );
    let warning = claude["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["code"] == "claude-memory-unassociated")
        .unwrap();
    assert_eq!(
        warning["path"],
        memory.parent().unwrap().display().to_string()
    );
    assert!(warning["message"].as_str().unwrap().contains("--all"));
}

#[test]
fn human_output_compares_provider_coverage() {
    let (fx, _, _) = paired_fixture();
    let out = fx
        .cmd()
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("memory for"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("codex  memory enabled"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("claude  memory enabled"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("provider coverage"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("startup index: first 200 lines or 25 KiB"),
        "stdout:\n{stdout}"
    );
    assert!(!stdout.contains("associated cwd:"), "stdout:\n{stdout}");
}

#[test]
fn human_output_honors_color_controls_without_coloring_json() {
    let (fx, _, _) = paired_fixture();
    let colored = fx
        .cmd_with_color("always")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(colored.status.success());
    assert!(
        colored.stdout.windows(2).any(|window| window == b"\x1b["),
        "stdout:\n{}",
        String::from_utf8_lossy(&colored.stdout)
    );

    let uncolored = fx
        .cmd()
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(uncolored.status.success());
    assert!(!uncolored.stdout.windows(2).any(|window| window == b"\x1b["));

    let json = fx
        .cmd_with_color("always")
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .output()
        .unwrap();
    assert!(json.status.success());
    assert!(!json.stdout.windows(2).any(|window| window == b"\x1b["));
    serde_json::from_slice::<Value>(&json.stdout).unwrap();
}

#[test]
fn bad_target_uses_error_exit_lane() {
    let fx = Fixture::new();
    let out = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path().join("missing"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("target directory not found")
    );
}

#[test]
fn all_includes_unrecognized_codex_memory_sources() {
    let (fx, codex_memory, _) = paired_fixture();
    let unknown_file = codex_memory.join("notes.md");
    let unknown_directory = codex_memory.join("archive");
    write(&unknown_file, "unrecognized memory\n");
    write(unknown_directory.join("entry.md"), "archived memory\n");
    for known_directory in [".git", "rollout_summaries", "skills"] {
        std::fs::create_dir_all(codex_memory.join(known_directory)).unwrap();
    }

    let base = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("codex")
        .output()
        .unwrap();
    assert!(base.status.success());
    let base: Value = serde_json::from_slice(&base.stdout).unwrap();
    let base_sources = provider(&base, "codex")["sources"].as_array().unwrap();
    assert!(base_sources.iter().all(|source| {
        source["path"] != unknown_file.display().to_string()
            && source["path"] != unknown_directory.display().to_string()
    }));

    let all = fx
        .cmd()
        .arg("--json")
        .arg("memory")
        .arg("show")
        .arg(fx.root.path())
        .arg("--provider")
        .arg("codex")
        .arg("--all")
        .output()
        .unwrap();
    assert!(
        all.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&all.stderr)
    );
    let all: Value = serde_json::from_slice(&all.stdout).unwrap();
    let codex = provider(&all, "codex");
    for path in [&unknown_file, &unknown_directory] {
        let source = source(codex, path);
        assert_eq!(source["role"], "unknown");
        assert_eq!(source["kind"], "unknown");
        assert_eq!(source["scope"], "unknown");
        assert_eq!(source["load_state"], "unknown");
        assert_eq!(source["association"], "unknown");
        assert_eq!(source["detail"], "unrecognized Codex memory source");
    }
    let source_paths = codex["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|source| source["path"].as_str())
        .collect::<Vec<_>>();
    for known_directory in [".git", "rollout_summaries", "skills"] {
        let known = codex_memory.join(known_directory).display().to_string();
        assert_eq!(
            source_paths.iter().filter(|path| **path == known).count(),
            usize::from(known_directory != ".git")
        );
    }
}
