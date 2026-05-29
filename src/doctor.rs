use anyhow::{Context, Result, bail};
use colored::Colorize;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

use crate::backup;
use crate::claude_json::{self, ClaudeJson};
use crate::git;
use crate::orphans;
use crate::output;
use crate::paths::{Env, ProjectPaths};
use crate::process;
use crate::secrets;

const WORKTREE_STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const CLAUDE_JSON_BLOAT_THRESHOLD_KB: usize = 512;
const CREDENTIAL_DENY_HINTS: &[&str] = &[".env", "secrets"];

pub struct Options {
    pub path: PathBuf,
    pub fix: bool,
    pub force: bool,
    pub show_secrets: bool,
    pub json: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Serialize)]
pub struct Location {
    pub file: PathBuf,
    /// Dotted JSON path inside `file`, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub location: Location,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_fix: Option<String>,
    pub auto_fixable: bool,
}

pub fn run(env: &Env, opts: Options) -> Result<ExitCode> {
    let project = ProjectPaths::new(opts.path.canonicalize().unwrap_or(opts.path.clone()));
    let mut findings = Vec::new();

    // Read the user-scope ~/.claude.json once; three checks consult it. A parse
    // error aborts the run (exit 2), as it did when the first check loaded it.
    let claude_json = if env.claude_json.exists() {
        Some(ClaudeJson::load(&env.claude_json)?)
    } else {
        None
    };

    check_orphaned_projects(env, claude_json.as_ref(), &mut findings);
    check_claude_json_size(env, claude_json.as_ref(), &mut findings);
    check_stale_worktrees(&project, &mut findings)?;
    check_secrets_in_committed_settings(&project, &mut findings, opts.show_secrets)?;
    check_local_settings_in_git(&project, &mut findings)?;
    check_credential_deny_rules(&project, env, &mut findings)?;
    check_dead_skill_command_agent_refs(&project, env, &mut findings)?;
    check_disabled_mcp_servers(&project, env, claude_json.as_ref(), &mut findings)?;

    // Sort by severity, then id, then location so output is stable regardless
    // of filesystem read order.
    findings.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then(a.id.cmp(b.id))
            .then(a.location.file.cmp(&b.location.file))
            .then(a.location.key_path.cmp(&b.location.key_path))
    });

    if !opts.json {
        emit_human(&findings);
    }

    // Apply before emitting JSON so `fix_applied` reflects whether a mutation
    // actually happened — not merely that --fix was passed. In human mode
    // apply_fixes prints its own progress after the findings, so order is kept.
    let fix_applied = if opts.fix {
        apply_fixes(env, &findings, &opts)?
    } else {
        false
    };

    if opts.json {
        emit_json(&findings, fix_applied);
    }

    let exit = if findings.iter().any(|f| f.severity == Severity::Error) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    };
    Ok(exit)
}

fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Error => 0,
        Severity::Warn => 1,
        Severity::Info => 2,
    }
}

fn emit_human(findings: &[Finding]) {
    if findings.is_empty() {
        println!("{} no findings", "clean.".green().bold());
        return;
    }

    for f in findings {
        let tag = match f.severity {
            Severity::Error => "error".red().bold(),
            Severity::Warn => "warn".yellow().bold(),
            Severity::Info => "info".cyan().bold(),
        };
        let auto = if f.auto_fixable {
            " (auto-fixable)".dimmed().to_string()
        } else {
            String::new()
        };
        println!("{tag} [{}] {}{auto}", f.id, f.message);
        let loc = match &f.location.key_path {
            Some(k) => format!("  at {}:{}", f.location.file.display(), k),
            None => format!("  at {}", f.location.file.display()),
        };
        println!("{}", loc.dimmed());
        if let Some(fix) = &f.suggested_fix {
            println!("  {} {fix}", "fix:".green());
        }
        println!();
    }

    let errs = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let warns = findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .count();
    let infos = findings
        .iter()
        .filter(|f| f.severity == Severity::Info)
        .count();
    let fixable = findings.iter().filter(|f| f.auto_fixable).count();
    println!(
        "{} error, {} warn, {} info — {} auto-fixable",
        errs, warns, infos, fixable
    );
}

fn emit_json(findings: &[Finding], fix: bool) {
    let v = json!({
        "findings": findings,
        "fix_applied": fix,
    });
    println!("{}", serde_json::to_string_pretty(&v).expect("serialize"));
}

/// Apply the auto-fixable findings. Returns whether the config was actually
/// mutated (false when nothing is auto-fixable or the targets no longer exist).
fn apply_fixes(env: &Env, findings: &[Finding], opts: &Options) -> Result<bool> {
    let auto: Vec<&Finding> = findings.iter().filter(|f| f.auto_fixable).collect();
    if auto.is_empty() {
        if !opts.json {
            println!("nothing auto-fixable.");
        }
        return Ok(false);
    }
    if !opts.force && process::claude_is_running() {
        bail!(
            "a `claude` process is running — quit it first, or pass --force \
             (Claude Code rewrites {} live and may overwrite our changes)",
            env.claude_json.display()
        );
    }

    let prune_targets: HashSet<&str> = auto
        .iter()
        .filter(|f| f.id == "orphaned-project")
        .filter_map(|f| {
            f.location
                .key_path
                .as_deref()
                .and_then(|k| k.strip_prefix("projects."))
        })
        .collect();
    if prune_targets.is_empty() {
        return Ok(false);
    }

    let mut config = ClaudeJson::load(&env.claude_json)?;
    let total = config.projects().map(|p| p.len()).unwrap_or(0);
    if !opts.force && orphans::looks_like_wrong_host(prune_targets.len(), total) {
        bail!(
            "{} of {} project entries resolve missing — this usually means you are on a \
             different machine or an unmounted volume, not that they are all dead. \
             Re-run with --force to prune them anyway.",
            prune_targets.len(),
            total
        );
    }
    let Some(map) = config.projects_mut() else {
        return Ok(false);
    };
    let before = map.len();
    map.retain(|k, _| !prune_targets.contains(k.as_str()));
    let removed = before - map.len();
    if removed == 0 {
        return Ok(false);
    }
    let new_raw = claude_json::render(&config.data)?;
    let backup_path = backup::timestamped_copy(&env.claude_json)?;
    claude_json::write_atomic(&env.claude_json, &new_raw)?;
    if !opts.json {
        println!("pruned {removed} orphaned project entries.");
        println!("backed up to {}", backup_path.display());
    }
    Ok(true)
}

// -- checks ------------------------------------------------------------------

fn check_orphaned_projects(env: &Env, config: Option<&ClaudeJson>, out: &mut Vec<Finding>) {
    let Some(config) = config else {
        return;
    };
    let Some(projects) = config.projects() else {
        return;
    };
    for o in orphans::find(projects, false) {
        out.push(Finding {
            id: "orphaned-project",
            severity: Severity::Warn,
            location: Location {
                file: env.claude_json.clone(),
                key_path: Some(format!("projects.{}", o.path)),
            },
            message: if o.is_worktree {
                format!("worktree directory no longer exists: {}", o.path)
            } else {
                format!("project directory no longer exists: {}", o.path)
            },
            suggested_fix: Some("remove this entry with `midden prune --apply`".into()),
            auto_fixable: true,
        });
    }
}

fn check_claude_json_size(env: &Env, config: Option<&ClaudeJson>, out: &mut Vec<Finding>) {
    let Some(config) = config else {
        return;
    };
    // `raw` is the file's text as read, so its byte length is the on-disk size.
    let kb = config.raw.len() / 1024;
    if kb < CLAUDE_JSON_BLOAT_THRESHOLD_KB {
        return;
    }
    // Point at the actual culprits. prune only reclaims orphaned `projects`
    // entries; the bulk is usually regenerable `cached*` blobs and per-project
    // metrics Claude Code rewrites — neither is safe to hand-prune.
    let breakdown = match largest_top_level_keys(&config.data, 3) {
        keys if keys.is_empty() => String::new(),
        keys => {
            let parts: Vec<String> = keys
                .iter()
                .map(|(k, bytes)| format!("{k} ({})", output::kb(*bytes)))
                .collect();
            format!("; biggest: {}", parts.join(", "))
        }
    };
    out.push(Finding {
        id: "claude-json-bloat",
        severity: Severity::Info,
        location: Location {
            file: env.claude_json.clone(),
            key_path: None,
        },
        message: format!("{} is {} KB{}", env.claude_json.display(), kb, breakdown),
        suggested_fix: Some(
            "`midden prune --apply` reclaims orphaned project entries; the rest is \
             cache and metrics Claude Code rewrites, not safe to hand-prune"
                .into(),
        ),
        auto_fixable: false,
    });
}

/// The `n` largest top-level keys by compact-serialized byte size, so the bloat
/// finding can name what is actually big. Empty if `data` is not a JSON object.
fn largest_top_level_keys(data: &Value, n: usize) -> Vec<(String, usize)> {
    let Value::Object(map) = data else {
        return Vec::new();
    };
    let mut sizes: Vec<(String, usize)> = map
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::to_string(v).map_or(0, |s| s.len())))
        .collect();
    sizes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    sizes.truncate(n);
    sizes
}

fn check_stale_worktrees(project: &ProjectPaths, out: &mut Vec<Finding>) -> Result<()> {
    let dir = project.worktrees_dir();
    if !dir.is_dir() {
        return Ok(());
    }
    let now = SystemTime::now();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_ephemeral_slug(name) {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = now.duration_since(mtime).unwrap_or_default();
        if age < WORKTREE_STALE_AFTER {
            continue;
        }
        let days = age.as_secs() / 86_400;
        out.push(Finding {
            id: "stale-worktree",
            severity: Severity::Info,
            location: Location {
                file: path.clone(),
                key_path: None,
            },
            message: format!("worktree {name} has been idle for {days} days"),
            suggested_fix: Some(format!(
                "inspect for uncommitted work, then `rm -rf {}`",
                path.display()
            )),
            auto_fixable: false,
        });
    }
    Ok(())
}

fn is_ephemeral_slug(s: &str) -> bool {
    // adjective-scientist pattern: two lowercase tokens separated by a single
    // hyphen, each at least 3 chars. Deliberately conservative.
    let mut parts = s.split('-');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    a.len() >= 3
        && b.len() >= 3
        && a.chars().all(|c| c.is_ascii_lowercase())
        && b.chars().all(|c| c.is_ascii_lowercase())
}

fn check_secrets_in_committed_settings(
    project: &ProjectPaths,
    out: &mut Vec<Finding>,
    show_secrets: bool,
) -> Result<()> {
    let settings = project.settings();
    if !settings.exists() {
        return Ok(());
    }
    // A settings.json git explicitly ignores is not "committed" — flagging it as
    // an Error (and exiting 1) is a false positive for the common pattern of
    // gitignoring .claude/ wholesale. When git can't tell (not a repo, no git),
    // fall through and flag it, as before. settings.local.json is covered
    // separately by check_local_settings_in_git.
    let rel = settings
        .strip_prefix(&project.root)
        .unwrap_or(settings.as_path());
    if git::is_ignored(&project.root, rel) == Some(true) {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&settings)
        .with_context(|| format!("read {}", settings.display()))?;
    let Ok(value): Result<Value, _> = serde_json::from_str(&raw) else {
        return Ok(());
    };

    let mut hits = Vec::new();
    walk_for_secrets(&value, "", &mut hits);

    for (key_path, raw_value) in hits {
        let displayed = if show_secrets {
            raw_value.clone()
        } else {
            secrets::mask(&raw_value)
        };
        out.push(Finding {
            id: "secret-in-committed-settings",
            severity: Severity::Error,
            location: Location {
                file: settings.clone(),
                key_path: Some(key_path.clone()),
            },
            message: format!("possible secret in committed settings: {key_path} = {displayed}"),
            suggested_fix: Some(format!(
                "move `{key_path}` to .claude/settings.local.json (gitignored)"
            )),
            auto_fixable: false,
        });
    }
    Ok(())
}

fn walk_for_secrets(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let new_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if secrets::key_looks_sensitive(k) {
                    // The whole subtree under a sensitive key is suspect —
                    // including bare strings in arrays, which carry no key.
                    collect_secret_strings(v, &new_path, out);
                } else {
                    walk_for_secrets(v, &new_path, out);
                }
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                walk_for_secrets(v, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// Collect every non-empty string under an already-sensitive key, descending
/// through nested objects and arrays so a secret stored as `apiKeys: ["sk-…"]`
/// is caught, not just one stored as a direct string value.
fn collect_secret_strings(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::String(s) if !s.is_empty() => out.push((path.to_string(), s.clone())),
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                collect_secret_strings(v, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                collect_secret_strings(v, &format!("{path}.{k}"), out);
            }
        }
        _ => {}
    }
}

/// `settings.local.json` is the machine-local override file — Claude Code, and
/// our own secret-in-committed-settings fix, steer credentials into it on the
/// assumption it stays out of git. Flag it when git tracks it (already exposed)
/// or merely fails to ignore it (one `git add` away from exposure).
fn check_local_settings_in_git(project: &ProjectPaths, out: &mut Vec<Finding>) -> Result<()> {
    let local = project.local_settings();
    if !local.is_file() {
        return Ok(());
    }
    // Pass a repo-relative path so git resolves it against the worktree it
    // discovers, regardless of symlinked roots.
    let rel = local.strip_prefix(&project.root).unwrap_or(local.as_path());
    if git::is_tracked(&project.root, rel) == Some(true) {
        out.push(Finding {
            id: "local-settings-tracked",
            severity: Severity::Warn,
            location: Location {
                file: local.clone(),
                key_path: None,
            },
            message: "settings.local.json is tracked by git — it is meant to stay machine-local \
                      and often holds secrets"
                .into(),
            suggested_fix: Some(
                "`git rm --cached .claude/settings.local.json`, then add it to .gitignore".into(),
            ),
            auto_fixable: false,
        });
    } else if git::is_ignored(&project.root, rel) == Some(false) {
        out.push(Finding {
            id: "local-settings-not-ignored",
            severity: Severity::Warn,
            location: Location {
                file: local,
                key_path: None,
            },
            message: "settings.local.json is not gitignored — `git add` would commit your \
                      machine-local overrides"
                .into(),
            suggested_fix: Some("add `.claude/settings.local.json` to .gitignore".into()),
            auto_fixable: false,
        });
    }
    Ok(())
}

fn check_credential_deny_rules(
    project: &ProjectPaths,
    env: &Env,
    out: &mut Vec<Finding>,
) -> Result<()> {
    let merged_deny = merged_deny_rules(project, env)?;
    let lower: Vec<String> = merged_deny.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut missing = Vec::new();
    for hint in CREDENTIAL_DENY_HINTS {
        if !lower.iter().any(|s| s.contains(hint)) {
            missing.push(*hint);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // Cite an existing settings file where the rule could be added. Deny
    // arrays concat across scopes, so a user-scope rule covers every project.
    // Prefer user → project → local → user-as-fallback (suggest creating it).
    let (location_file, suggested_fix) = recommend_deny_location(project, env);
    out.push(Finding {
        id: "missing-credential-deny",
        severity: Severity::Warn,
        location: Location {
            file: location_file,
            key_path: Some("permissions.deny".into()),
        },
        message: format!(
            "no deny rule covers {} — Claude could read credentials here",
            missing.join(", ")
        ),
        suggested_fix: Some(suggested_fix),
        auto_fixable: false,
    });
    Ok(())
}

fn recommend_deny_location(project: &ProjectPaths, env: &Env) -> (PathBuf, String) {
    let user = env.user_settings();
    let proj = project.settings();
    let local = project.local_settings();
    let example =
        r#"add e.g. "Read(./.env)", "Read(./.env.*)", "Read(./secrets/**)" to permissions.deny"#;
    if user.is_file() {
        (
            user,
            format!("{example} in ~/.claude/settings.json to cover every project"),
        )
    } else if proj.is_file() {
        (proj.clone(), format!("{example} in {}", proj.display()))
    } else if local.is_file() {
        (local.clone(), format!("{example} in {}", local.display()))
    } else {
        (
            user.clone(),
            format!(
                "{example} (create {} to apply across all projects)",
                user.display()
            ),
        )
    }
}

fn merged_deny_rules(project: &ProjectPaths, env: &Env) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for path in [
        env.user_settings(),
        project.settings(),
        project.local_settings(),
    ] {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && let Some(deny) = v.pointer("/permissions/deny").and_then(Value::as_array)
        {
            for d in deny {
                if let Value::String(s) = d {
                    out.push(s.clone());
                }
            }
        }
    }
    Ok(out)
}

fn check_dead_skill_command_agent_refs(
    project: &ProjectPaths,
    env: &Env,
    out: &mut Vec<Finding>,
) -> Result<()> {
    // Skills are directories with SKILL.md. Flag a skill dir missing SKILL.md.
    for (label, dir) in [
        ("user skill", env.user_skills_dir()),
        ("project skill", project.skills_dir()),
    ] {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if !path.join("SKILL.md").is_file() {
                out.push(Finding {
                    id: "skill-missing-skill-md",
                    severity: Severity::Warn,
                    location: Location {
                        file: path.clone(),
                        key_path: None,
                    },
                    message: format!(
                        "{label} {:?} is missing SKILL.md",
                        path.file_name().unwrap_or_default()
                    ),
                    suggested_fix: Some(
                        "add a SKILL.md with frontmatter, or remove the directory".into(),
                    ),
                    auto_fixable: false,
                });
            }
        }
    }

    // Commands and agents are bare markdown files. Flag empty files or files
    // missing the frontmatter that the loader expects.
    for (label, dir) in [
        ("user command", env.user_commands_dir()),
        ("project command", project.commands_dir()),
        ("user agent", env.user_agents_dir()),
        ("project agent", project.agents_dir()),
    ] {
        if !dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&dir)
            .max_depth(3)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(meta) = path.metadata() else { continue };
            if meta.len() == 0 {
                out.push(Finding {
                    id: "empty-config-file",
                    severity: Severity::Warn,
                    location: Location {
                        file: path.to_path_buf(),
                        key_path: None,
                    },
                    message: format!("{label} file is empty: {}", path.display()),
                    suggested_fix: Some("remove the file or add content".into()),
                    auto_fixable: false,
                });
            }
        }
    }
    Ok(())
}

fn check_disabled_mcp_servers(
    project: &ProjectPaths,
    env: &Env,
    config: Option<&ClaudeJson>,
    out: &mut Vec<Finding>,
) -> Result<()> {
    // User scope: reuse the already-parsed ~/.claude.json.
    if let Some(config) = config {
        scan_mcp_servers(&config.data, &env.claude_json, out);
    }
    // Project and managed scopes live in their own files.
    for path in [project.mcp_json(), project.managed_mcp_json()] {
        if !path.exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        scan_mcp_servers(&v, &path, out);
    }
    Ok(())
}

fn scan_mcp_servers(v: &Value, file: &Path, out: &mut Vec<Finding>) {
    let Some(servers) = v.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    for (name, def) in servers {
        let cmd = def.get("command").and_then(Value::as_str).unwrap_or("");
        let url = def.get("url").and_then(Value::as_str).unwrap_or("");
        if cmd.is_empty() && url.is_empty() {
            out.push(Finding {
                id: "mcp-server-unreachable",
                severity: Severity::Warn,
                location: Location {
                    file: file.to_path_buf(),
                    key_path: Some(format!("mcpServers.{name}")),
                },
                message: format!("MCP server `{name}` has no command or url — it will never start"),
                suggested_fix: Some(
                    "set `command` (stdio) or `url` (http/sse), or remove the entry".into(),
                ),
                auto_fixable: false,
            });
        }
        if def.get("disabled").and_then(Value::as_bool) == Some(true) {
            out.push(Finding {
                id: "mcp-server-disabled",
                severity: Severity::Info,
                location: Location {
                    file: file.to_path_buf(),
                    key_path: Some(format!("mcpServers.{name}")),
                },
                message: format!("MCP server `{name}` is defined but disabled"),
                suggested_fix: Some("remove the entry if you no longer need it".into()),
                auto_fixable: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_slug_matches_adjective_scientist() {
        assert!(is_ephemeral_slug("witty-curie"));
        assert!(is_ephemeral_slug("clever-darwin"));
        assert!(!is_ephemeral_slug("not-ephemeral-name"));
        assert!(!is_ephemeral_slug("Some-Name"));
        assert!(!is_ephemeral_slug("singleword"));
        assert!(!is_ephemeral_slug("a-b"));
    }

    #[test]
    fn walk_for_secrets_captures_dotted_paths() {
        let v: Value = serde_json::from_str(
            r#"{
                "env": { "ANTHROPIC_API_KEY": "sk-secret-123" },
                "permissions": { "deny": ["Read(./.env)"] }
            }"#,
        )
        .unwrap();
        let mut hits = Vec::new();
        walk_for_secrets(&v, "", &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "env.ANTHROPIC_API_KEY");
    }

    #[test]
    fn walk_for_secrets_captures_array_elements_under_sensitive_key() {
        // A secret stored as an array of bare strings under a sensitive key
        // used to slip through (the elements have no key of their own).
        let v: Value = serde_json::from_str(
            r#"{ "apiKeys": ["sk-aaaa-1111", "sk-bbbb-2222"], "name": "fine" }"#,
        )
        .unwrap();
        let mut hits = Vec::new();
        walk_for_secrets(&v, "", &mut hits);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "apiKeys[0]");
        assert_eq!(hits[1].0, "apiKeys[1]");
    }

    #[test]
    fn recommend_deny_location_prefers_existing_user_settings() {
        let home = tempfile::tempdir().unwrap();
        let proj_root = tempfile::tempdir().unwrap();
        let env = Env::new(
            Some(home.path().join(".claude.json")),
            Some(home.path().join(".claude")),
        );
        let project = ProjectPaths::new(proj_root.path());
        std::fs::create_dir_all(env.user_settings().parent().unwrap()).unwrap();
        std::fs::write(env.user_settings(), "{}").unwrap();

        let (file, fix) = recommend_deny_location(&project, &env);
        assert_eq!(
            file,
            env.user_settings(),
            "should cite the user settings file"
        );
        assert!(fix.contains("~/.claude/settings.json"), "fix: {fix}");
    }

    #[test]
    fn recommend_deny_location_falls_back_to_user_path_when_nothing_exists() {
        let home = tempfile::tempdir().unwrap();
        let proj_root = tempfile::tempdir().unwrap();
        let env = Env::new(
            Some(home.path().join(".claude.json")),
            Some(home.path().join(".claude")),
        );
        let project = ProjectPaths::new(proj_root.path());
        // No settings files created anywhere.

        let (file, fix) = recommend_deny_location(&project, &env);
        assert_eq!(file, env.user_settings());
        assert!(fix.contains("create"), "fix should hint creation: {fix}");
    }

    #[test]
    fn recommend_deny_location_uses_project_when_user_missing() {
        let home = tempfile::tempdir().unwrap();
        let proj_root = tempfile::tempdir().unwrap();
        let env = Env::new(
            Some(home.path().join(".claude.json")),
            Some(home.path().join(".claude")),
        );
        let project = ProjectPaths::new(proj_root.path());
        // Only project-scope settings exists.
        std::fs::create_dir_all(project.settings().parent().unwrap()).unwrap();
        std::fs::write(project.settings(), "{}").unwrap();

        let (file, _) = recommend_deny_location(&project, &env);
        assert_eq!(file, project.settings());
    }

    #[test]
    fn largest_top_level_keys_ranks_by_size() {
        let v: Value =
            serde_json::from_str(r#"{"small":1,"big":"xxxxxxxxxxxxxxxxxxxx","mid":[1,2,3]}"#)
                .unwrap();
        let top = largest_top_level_keys(&v, 2);
        assert_eq!(top.len(), 2, "truncated to n");
        assert_eq!(top[0].0, "big");
        assert_eq!(top[1].0, "mid");
    }

    #[test]
    fn largest_top_level_keys_empty_on_non_object() {
        let v: Value = serde_json::from_str(r#""just a string""#).unwrap();
        assert!(largest_top_level_keys(&v, 3).is_empty());
    }
}
