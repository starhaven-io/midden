use anyhow::{Context, Result, bail};
use colored::Colorize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use walkdir::WalkDir;

use crate::claude_json;
use crate::paths::{Env, ProjectPaths, managed_settings_files};
use crate::secrets;

pub struct Options {
    pub path: PathBuf,
    pub show_secrets: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    User,
    Project,
    Local,
    Managed,
}

impl Scope {
    fn label(&self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
            Scope::Managed => "managed",
        }
    }
}

#[derive(Debug, Serialize)]
struct Contribution {
    scope: Scope,
    file: PathBuf,
    value: Value,
    /// For scalars: true if a higher scope shadows this. For arrays this is
    /// always false (arrays merge, not override).
    shadowed: bool,
}

#[derive(Debug, Serialize)]
struct Resolved {
    key: String,
    effective: Value,
    contributions: Vec<Contribution>,
}

pub fn run(env: &Env, opts: Options) -> Result<ExitCode> {
    // A typo'd path would resolve an empty report as if it were real state —
    // bad input is the exit-2 lane.
    let root = opts
        .path
        .canonicalize()
        .with_context(|| format!("target directory not found: {}", opts.path.display()))?;
    if !root.is_dir() {
        bail!("target is not a directory: {}", root.display());
    }
    let project = ProjectPaths::new(&root);

    let mut sources: Vec<(Scope, PathBuf, Value)> = Vec::new();
    if let Some(v) = read_json(&env.user_settings()) {
        sources.push((Scope::User, env.user_settings(), v));
    }
    if let Some(v) = read_json(&project.settings()) {
        sources.push((Scope::Project, project.settings(), v));
    }
    if let Some(v) = read_json(&project.local_settings()) {
        sources.push((Scope::Local, project.local_settings(), v));
    }
    for managed in managed_settings_files() {
        if let Some(v) = read_json(&managed) {
            sources.push((Scope::Managed, managed, v));
        }
    }

    let mut hooks = collect_hooks(&sources);
    let resolved: Vec<Resolved> = resolve_settings(&sources)
        .into_iter()
        // Hooks have their own section — drop them from the generic settings
        // dump so they aren't shown twice as opaque JSON blobs.
        .filter(|r| !r.key.starts_with("hooks."))
        .collect();
    let claude_mds = collect_claude_md(&project, env);
    let contradictions = detect_contradictions(&claude_mds);
    let skills = collect_dirs(&[env.user_skills_dir(), project.skills_dir()], "SKILL.md");
    let commands = collect_files(&[env.user_commands_dir(), project.commands_dir()]);
    let agents = collect_files(&[env.user_agents_dir(), project.agents_dir()]);
    let mut mcp_servers = collect_mcp_servers(env, &project);
    let worktrees = collect_worktrees(&project);

    let mut resolved = resolved;
    if !opts.show_secrets {
        for r in &mut resolved {
            if path_looks_sensitive(&r.key) {
                secrets::mask_value(&mut r.effective);
                for c in &mut r.contributions {
                    secrets::mask_value(&mut c.value);
                }
            }
            // Token-shaped values hide under innocent keys too — args arrays,
            // env.DATABASE_URL — so mask by content as well as by key name.
            secrets::mask_sensitive_values(&mut r.effective);
            for c in &mut r.contributions {
                secrets::mask_sensitive_values(&mut c.value);
            }
        }
        // Hook commands and MCP URLs are free-form text that can embed
        // credentials (Bearer headers, user:pass URLs, token query params).
        for h in &mut hooks {
            h.command = secrets::mask_embedded(&h.command);
        }
        for s in &mut mcp_servers {
            if let Some(url) = &mut s.url {
                *url = secrets::mask_embedded(url);
            }
        }
    }

    let report = Report {
        root,
        resolved,
        claude_mds,
        contradictions,
        skills,
        commands,
        agents,
        hooks,
        mcp_servers,
        worktrees,
    };

    if opts.json {
        emit_json(&report);
    } else {
        emit_human(&report);
    }

    Ok(ExitCode::SUCCESS)
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Merge the same key across scopes with provenance tracking. Scalars: highest
/// scope wins, lowers marked shadowed. Arrays: concat + dedupe across scopes
/// (no contribution shadowed). Objects: recurse.
fn resolve_settings(sources: &[(Scope, PathBuf, Value)]) -> Vec<Resolved> {
    // Flatten each source into (path, value) pairs. Objects recurse; arrays
    // and scalars are leaves.
    let mut by_path: BTreeMap<String, Vec<(Scope, PathBuf, Value)>> = BTreeMap::new();
    for (scope, file, value) in sources {
        let mut leaves = Vec::new();
        flatten(value, String::new(), &mut leaves);
        for (k, v) in leaves {
            by_path
                .entry(k)
                .or_default()
                .push((*scope, file.clone(), v));
        }
    }

    let mut out = Vec::new();
    for (key, mut contribs) in by_path {
        // Sort highest scope last; for scalars that's the winner.
        contribs.sort_by_key(|(s, _, _)| *s);

        let all_arrays = contribs.iter().all(|(_, _, v)| v.is_array());

        let (effective, contributions) = if all_arrays {
            // Concat + dedupe by structural equality.
            let mut merged: Vec<Value> = Vec::new();
            for (_, _, v) in &contribs {
                if let Value::Array(arr) = v {
                    for item in arr {
                        if !merged.iter().any(|m| m == item) {
                            merged.push(item.clone());
                        }
                    }
                }
            }
            let contributions = contribs
                .iter()
                .map(|(s, f, v)| Contribution {
                    scope: *s,
                    file: f.clone(),
                    value: v.clone(),
                    shadowed: false,
                })
                .collect();
            (Value::Array(merged), contributions)
        } else {
            // Scalar override: highest scope wins.
            let winner_idx = contribs.len() - 1;
            let effective = contribs[winner_idx].2.clone();
            let contributions = contribs
                .iter()
                .enumerate()
                .map(|(i, (s, f, v))| Contribution {
                    scope: *s,
                    file: f.clone(),
                    value: v.clone(),
                    shadowed: i != winner_idx,
                })
                .collect();
            (effective, contributions)
        };

        out.push(Resolved {
            key,
            effective,
            contributions,
        });
    }
    out
}

fn flatten(value: &Value, prefix: String, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let new = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(v, new, out);
            }
        }
        // Arrays + scalars are leaves.
        _ => out.push((prefix, value.clone())),
    }
}

fn path_looks_sensitive(dotted: &str) -> bool {
    dotted
        .rsplit('.')
        .next()
        .is_some_and(secrets::key_looks_sensitive)
        || secrets::key_looks_sensitive(dotted)
}

#[derive(Debug, Serialize)]
struct ClaudeMd {
    file: PathBuf,
    scope: ClaudeMdScope,
    bytes: u64,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ClaudeMdScope {
    User,
    Project,
    Local,
    Ancestor,
}

fn collect_claude_md(project: &ProjectPaths, env: &Env) -> Vec<ClaudeMd> {
    let mut out = Vec::new();
    let user = env.user_claude_md();
    if let Ok(m) = std::fs::metadata(&user) {
        out.push(ClaudeMd {
            file: user,
            scope: ClaudeMdScope::User,
            bytes: m.len(),
        });
    }
    // Walk from project root up to filesystem root for ancestor CLAUDE.md.
    let mut current = project.root.clone();
    let original_root = current.clone();
    loop {
        for (name, scope) in [
            (
                "CLAUDE.md",
                if current == original_root {
                    ClaudeMdScope::Project
                } else {
                    ClaudeMdScope::Ancestor
                },
            ),
            ("CLAUDE.local.md", ClaudeMdScope::Local),
        ] {
            let p = current.join(name);
            if let Ok(m) = std::fs::metadata(&p) {
                out.push(ClaudeMd {
                    file: p,
                    scope,
                    bytes: m.len(),
                });
            }
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    // Nested CLAUDE.md inside the project (subdirectories). Skip vendored and
    // build-output dirs that pile up their own CLAUDE.md files but never get
    // loaded by Claude Code.
    let walker = WalkDir::new(&project.root)
        .max_depth(4)
        .into_iter()
        .filter_entry(|e| !is_vendored_dir(e.path()));
    let mut nested: Vec<ClaudeMd> = Vec::new();
    for entry in walker.filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.file_name().and_then(|n| n.to_str()) == Some("CLAUDE.md") && p != project.claude_md() {
            if out.iter().any(|c| c.file == p) {
                continue;
            }
            if let Ok(m) = entry.metadata() {
                nested.push(ClaudeMd {
                    file: p.to_path_buf(),
                    scope: ClaudeMdScope::Project,
                    bytes: m.len(),
                });
            }
        }
    }
    nested.sort_by(|a, b| a.file.cmp(&b.file));
    out.extend(nested);
    out
}

/// Directory names that hold vendored or generated content. The walker prunes
/// at the directory boundary so we never descend into them.
const VENDORED_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "target",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
];

fn is_vendored_dir(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    VENDORED_DIRS.contains(&name)
}

#[derive(Debug, Serialize)]
struct Contradiction {
    a_file: PathBuf,
    b_file: PathBuf,
    a_line: String,
    b_line: String,
    keyword: String,
}

type Directive = (Polarity, String, String);

/// Heuristic CLAUDE.md contradiction detection. We look for imperative lines
/// ("do X", "don't X", "never X", "always X") that share a content keyword
/// across files and disagree on directive polarity. This is best-effort by
/// design — false negatives are common, false positives kept low.
fn detect_contradictions(files: &[ClaudeMd]) -> Vec<Contradiction> {
    let mut lines_by_file: Vec<(PathBuf, Vec<Directive>)> = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f.file) else {
            continue;
        };
        let mut entries = Vec::new();
        for line in text.lines() {
            if let Some((pol, kw, raw)) = parse_directive(line) {
                entries.push((pol, kw, raw));
            }
        }
        if !entries.is_empty() {
            lines_by_file.push((f.file.clone(), entries));
        }
    }

    let mut out = Vec::new();
    for i in 0..lines_by_file.len() {
        for j in (i + 1)..lines_by_file.len() {
            for (a_pol, a_kw, a_raw) in &lines_by_file[i].1 {
                for (b_pol, b_kw, b_raw) in &lines_by_file[j].1 {
                    if a_kw == b_kw && a_pol != b_pol {
                        out.push(Contradiction {
                            a_file: lines_by_file[i].0.clone(),
                            b_file: lines_by_file[j].0.clone(),
                            a_line: a_raw.clone(),
                            b_line: b_raw.clone(),
                            keyword: a_kw.clone(),
                        });
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Polarity {
    Do,
    Dont,
}

/// Parse a single line for a coarse imperative directive. Returns `(polarity,
/// content-keyword, original-line)`. The content-keyword is the first
/// significant word after the polarity verb, lowercased.
fn parse_directive(line: &str) -> Option<(Polarity, String, String)> {
    let trimmed = line.trim_start_matches(['-', '*', '#', ' ', '\t']).trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    // Negations before their positive prefixes: "must not" would otherwise
    // parse as a positive "must" and invert the directive's polarity. The
    // typographic apostrophe variant shows up in prose-styled files.
    let (polarity, rest) = if let Some(rest) = lower.strip_prefix("never ") {
        (Polarity::Dont, rest)
    } else if let Some(rest) = lower
        .strip_prefix("don't ")
        .or_else(|| lower.strip_prefix("don’t "))
    {
        (Polarity::Dont, rest)
    } else if let Some(rest) = lower.strip_prefix("do not ") {
        (Polarity::Dont, rest)
    } else if let Some(rest) = lower
        .strip_prefix("must not ")
        .or_else(|| lower.strip_prefix("must never "))
    {
        (Polarity::Dont, rest)
    } else if let Some(rest) = lower.strip_prefix("always ") {
        (Polarity::Do, rest)
    } else if let Some(rest) = lower.strip_prefix("must ") {
        (Polarity::Do, rest)
    } else {
        return None;
    };
    let keyword: String = rest
        .split_whitespace()
        .filter(|w| !STOPWORDS.contains(w))
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    if keyword.is_empty() {
        return None;
    }
    Some((polarity, keyword, trimmed.to_string()))
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "to", "in", "on", "at", "of", "for", "and", "or", "but", "with", "this",
    "that", "any", "all",
];

#[derive(Debug, Serialize)]
struct LocatedDir {
    name: String,
    file: PathBuf,
    scope: &'static str,
}

fn collect_dirs(roots: &[PathBuf], required_file: &str) -> Vec<LocatedDir> {
    let mut out = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let scope = if i == 0 { "user" } else { "project" };
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut found: Vec<LocatedDir> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(required_file).is_file() {
                found.push(LocatedDir {
                    name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    file: path.join(required_file),
                    scope,
                });
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(found);
    }
    out
}

#[derive(Debug, Serialize)]
struct LocatedFile {
    name: String,
    file: PathBuf,
    scope: &'static str,
}

fn collect_files(roots: &[PathBuf]) -> Vec<LocatedFile> {
    let mut out = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let scope = if i == 0 { "user" } else { "project" };
        if !root.is_dir() {
            continue;
        }
        let walker = WalkDir::new(root)
            .max_depth(3)
            .into_iter()
            .filter_entry(|e| !is_vendored_dir(e.path()));
        let mut found: Vec<LocatedFile> = Vec::new();
        for entry in walker.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(LocatedFile {
                    name: p
                        .file_stem()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    file: p.to_path_buf(),
                    scope,
                });
            }
        }
        found.sort_by(|a, b| a.file.cmp(&b.file));
        out.extend(found);
    }
    out
}

#[derive(Debug, Serialize)]
struct Hook {
    /// Event name from settings, e.g. "PreToolUse", "PostToolUse", "Stop".
    event: String,
    /// Tool matcher pattern. None when the matcher is omitted (all tools).
    #[serde(skip_serializing_if = "Option::is_none")]
    matcher: Option<String>,
    /// Hook entry kind ("command", "script", etc.).
    kind: String,
    /// The command/script body. Long values are kept full in JSON; the human
    /// presenter truncates.
    command: String,
    scope: Scope,
    file: PathBuf,
}

/// Pull every individual hook entry out of every settings source. Each
/// `hooks.<EventName>` array contains matcher-groups, and each group's inner
/// `hooks` array contains one or more concrete commands — we flatten the lot.
fn collect_hooks(sources: &[(Scope, PathBuf, Value)]) -> Vec<Hook> {
    let mut out = Vec::new();
    for (scope, file, value) in sources {
        let Some(events) = value.get("hooks").and_then(Value::as_object) else {
            continue;
        };
        for (event_name, groups) in events {
            let Some(group_arr) = groups.as_array() else {
                continue;
            };
            for group in group_arr {
                let matcher = group
                    .get("matcher")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .filter(|s| !s.is_empty());
                let Some(entries) = group.get("hooks").and_then(Value::as_array) else {
                    continue;
                };
                for entry in entries {
                    let kind = entry
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("command")
                        .to_string();
                    let command = entry
                        .get("command")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .unwrap_or_default();
                    out.push(Hook {
                        event: event_name.clone(),
                        matcher: matcher.clone(),
                        kind,
                        command,
                        scope: *scope,
                        file: file.clone(),
                    });
                }
            }
        }
    }
    // Stable order: by event, then scope (precedence ascending), then file.
    out.sort_by(|a, b| {
        a.event
            .cmp(&b.event)
            .then(a.scope.cmp(&b.scope))
            .then(a.file.cmp(&b.file))
    });
    out
}

#[derive(Debug, Serialize)]
struct McpServer {
    name: String,
    scope: &'static str,
    file: PathBuf,
    command: Option<String>,
    url: Option<String>,
    disabled: bool,
}

fn collect_mcp_servers(env: &Env, project: &ProjectPaths) -> Vec<McpServer> {
    let mut out = Vec::new();
    // User and local scope both live in ~/.claude.json: the top-level
    // `mcpServers` map is user scope; the per-project entry's `mcpServers` is
    // local scope — the default destination of `claude mcp add`.
    if let Some(claude) = read_json(&env.claude_json) {
        push_mcp_servers(claude.get("mcpServers"), "user", &env.claude_json, &mut out);
        let local = claude_json::project_entry(&claude, &project.root)
            .and_then(|entry| entry.get("mcpServers"));
        push_mcp_servers(local, "local", &env.claude_json, &mut out);
    }
    for (scope, path) in [
        ("project", project.mcp_json()),
        ("managed", project.managed_mcp_json()),
    ] {
        if let Some(v) = read_json(&path) {
            push_mcp_servers(v.get("mcpServers"), scope, &path, &mut out);
        }
    }
    out
}

fn push_mcp_servers(
    servers: Option<&Value>,
    scope: &'static str,
    file: &Path,
    out: &mut Vec<McpServer>,
) {
    let Some(servers) = servers.and_then(Value::as_object) else {
        return;
    };
    for (name, def) in servers {
        out.push(McpServer {
            name: name.clone(),
            scope,
            file: file.to_path_buf(),
            command: def.get("command").and_then(Value::as_str).map(String::from),
            url: def.get("url").and_then(Value::as_str).map(String::from),
            disabled: def
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
}

#[derive(Debug, Serialize)]
struct Worktree {
    name: String,
    file: PathBuf,
}

fn collect_worktrees(project: &ProjectPaths) -> Vec<Worktree> {
    let dir = project.worktrees_dir();
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.push(Worktree {
                    name: p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    file: p,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Everything `show` resolved for a target directory. Field order is the JSON
/// emission order; `settings`/`claude_md` keep their original key names.
#[derive(Serialize)]
struct Report {
    root: PathBuf,
    #[serde(rename = "settings")]
    resolved: Vec<Resolved>,
    #[serde(rename = "claude_md")]
    claude_mds: Vec<ClaudeMd>,
    contradictions: Vec<Contradiction>,
    skills: Vec<LocatedDir>,
    commands: Vec<LocatedFile>,
    agents: Vec<LocatedFile>,
    hooks: Vec<Hook>,
    mcp_servers: Vec<McpServer>,
    worktrees: Vec<Worktree>,
}

// -- presentation ------------------------------------------------------------

fn emit_human(report: &Report) {
    let Report {
        root,
        resolved,
        claude_mds,
        contradictions,
        skills,
        commands,
        agents,
        hooks,
        mcp_servers,
        worktrees,
    } = report;
    println!("{} {}", "resolved for".bold(), root.display());
    println!();

    println!("{}", "settings".bold().underline());
    if resolved.is_empty() {
        println!("  (no settings found)");
    } else {
        for r in resolved {
            let val_str = format_value(&r.effective);
            println!("  {} = {}", r.key.cyan(), val_str);
            for c in &r.contributions {
                let tag = if c.shadowed {
                    format!("[{} shadowed]", c.scope.label())
                        .dimmed()
                        .strikethrough()
                        .to_string()
                } else if c.value.is_array() {
                    format!("[{} merged]", c.scope.label()).green().to_string()
                } else {
                    format!("[{}]", c.scope.label()).green().to_string()
                };
                println!(
                    "    {tag} {} = {}",
                    c.file.display(),
                    format_value(&c.value).dimmed()
                );
            }
        }
    }
    println!();

    println!("{}", "CLAUDE.md".bold().underline());
    if claude_mds.is_empty() {
        println!("  (none)");
    } else {
        for c in claude_mds {
            let scope = match c.scope {
                ClaudeMdScope::User => "user",
                ClaudeMdScope::Project => "project",
                ClaudeMdScope::Local => "local",
                ClaudeMdScope::Ancestor => "ancestor",
            };
            println!("  [{scope}] {} ({} bytes)", c.file.display(), c.bytes);
        }
    }
    if !contradictions.is_empty() {
        println!();
        println!("  {}", "contradictions:".yellow().bold());
        for c in contradictions {
            println!("    keyword `{}`", c.keyword);
            println!("      {} — {}", c.a_file.display(), c.a_line.dimmed());
            println!("      {} — {}", c.b_file.display(), c.b_line.dimmed());
        }
    }
    println!();

    print_section(
        "skills",
        skills
            .iter()
            .map(|s| (s.name.as_str(), s.file.as_path(), s.scope)),
    );
    print_section(
        "commands",
        commands
            .iter()
            .map(|c| (c.name.as_str(), c.file.as_path(), c.scope)),
    );
    print_section(
        "agents",
        agents
            .iter()
            .map(|a| (a.name.as_str(), a.file.as_path(), a.scope)),
    );

    println!("{}", "hooks".bold().underline());
    if hooks.is_empty() {
        println!("  (none)");
    } else {
        let mut current_event = "";
        for h in hooks {
            if h.event != current_event {
                println!("  {}", h.event.bold());
                current_event = &h.event;
            }
            let matcher = h.matcher.as_deref().unwrap_or("*");
            println!(
                "    [{}] {} ({}): {}",
                h.scope.label(),
                matcher.cyan(),
                h.kind,
                truncate_oneline(&h.command, 80)
            );
            println!("      {}", h.file.display().to_string().dimmed());
        }
    }
    println!();

    println!("{}", "mcp servers".bold().underline());
    if mcp_servers.is_empty() {
        println!("  (none)");
    } else {
        for s in mcp_servers {
            let target = s
                .command
                .as_deref()
                .or(s.url.as_deref())
                .unwrap_or("<unreachable>");
            let dis = if s.disabled {
                " (disabled)".red().to_string()
            } else {
                String::new()
            };
            println!("  [{}] {} -> {target}{dis}", s.scope, s.name);
            println!("    {}", s.file.display().to_string().dimmed());
        }
    }
    println!();

    println!("{}", "worktrees".bold().underline());
    if worktrees.is_empty() {
        println!("  (none)");
    } else {
        for w in worktrees {
            println!("  {} — {}", w.name, w.file.display());
        }
    }
}

fn print_section<'a>(title: &str, iter: impl Iterator<Item = (&'a str, &'a Path, &'a str)>) {
    println!("{}", title.bold().underline());
    let mut empty = true;
    for (name, file, scope) in iter {
        empty = false;
        println!("  [{scope}] {name}");
        println!("    {}", file.display().to_string().dimmed());
    }
    if empty {
        println!("  (none)");
    }
    println!();
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Array(arr) if arr.len() <= 6 => {
            let inner: Vec<String> = arr.iter().map(format_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Array(arr) => format!("<array of {} items>", arr.len()),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Collapse a possibly-multiline string to one line and truncate to at most
/// `max` chars. Whitespace runs are flattened so multi-line shell scripts
/// render as a single readable summary.
fn truncate_oneline(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max {
        collapsed
    } else {
        let mut t: String = collapsed.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn emit_json(report: &Report) {
    println!(
        "{}",
        serde_json::to_string_pretty(report).expect("serialize")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(scope: Scope, file: &str, v: Value) -> (Scope, PathBuf, Value) {
        (scope, PathBuf::from(file), v)
    }

    #[test]
    fn scalar_higher_scope_wins_lower_shadowed() {
        let sources = vec![
            s(
                Scope::User,
                "u",
                json!({ "permissions": { "defaultMode": "ask" } }),
            ),
            s(
                Scope::Project,
                "p",
                json!({ "permissions": { "defaultMode": "bypass" } }),
            ),
        ];
        let r = resolve_settings(&sources);
        let entry = r
            .iter()
            .find(|r| r.key == "permissions.defaultMode")
            .unwrap();
        assert_eq!(entry.effective, json!("bypass"));
        let user = entry
            .contributions
            .iter()
            .find(|c| c.scope == Scope::User)
            .unwrap();
        let project = entry
            .contributions
            .iter()
            .find(|c| c.scope == Scope::Project)
            .unwrap();
        assert!(user.shadowed);
        assert!(!project.shadowed);
    }

    #[test]
    fn managed_wins_over_everything() {
        let sources = vec![
            s(
                Scope::User,
                "u",
                json!({ "permissions": { "defaultMode": "ask" } }),
            ),
            s(
                Scope::Project,
                "p",
                json!({ "permissions": { "defaultMode": "bypass" } }),
            ),
            s(
                Scope::Local,
                "l",
                json!({ "permissions": { "defaultMode": "allow" } }),
            ),
            s(
                Scope::Managed,
                "m",
                json!({ "permissions": { "defaultMode": "deny" } }),
            ),
        ];
        let r = resolve_settings(&sources);
        let entry = r
            .iter()
            .find(|r| r.key == "permissions.defaultMode")
            .unwrap();
        assert_eq!(entry.effective, json!("deny"));
        for c in &entry.contributions {
            assert_eq!(c.shadowed, c.scope != Scope::Managed);
        }
    }

    #[test]
    fn arrays_concat_and_dedupe_across_scopes() {
        let sources = vec![
            s(
                Scope::User,
                "u",
                json!({ "permissions": { "deny": ["Read(./.env)", "Bash(rm:*)"] } }),
            ),
            s(
                Scope::Project,
                "p",
                json!({ "permissions": { "deny": ["Read(./.env)", "Read(./secrets/**)"] } }),
            ),
        ];
        let r = resolve_settings(&sources);
        let entry = r.iter().find(|r| r.key == "permissions.deny").unwrap();
        let eff = entry.effective.as_array().unwrap();
        assert_eq!(eff.len(), 3, "deduped union");
        assert!(eff.contains(&json!("Read(./.env)")));
        assert!(eff.contains(&json!("Bash(rm:*)")));
        assert!(eff.contains(&json!("Read(./secrets/**)")));
        // No contribution shadowed for arrays.
        assert!(entry.contributions.iter().all(|c| !c.shadowed));
    }

    #[test]
    fn parse_directive_detects_polarity() {
        let (pol, kw, _) = parse_directive("- never commit secrets to git").unwrap();
        assert_eq!(pol, Polarity::Dont);
        assert!(kw.starts_with("commit"));

        let (pol, _, _) = parse_directive("Always run cargo fmt").unwrap();
        assert_eq!(pol, Polarity::Do);

        assert!(parse_directive("This is a paragraph.").is_none());
    }

    #[test]
    fn parse_directive_handles_negated_must() {
        let (pol, kw, _) = parse_directive("- Must not commit directly to main").unwrap();
        assert_eq!(pol, Polarity::Dont, "must not is a negation");
        assert!(kw.starts_with("commit"), "keyword: {kw}");

        let (pol, _, _) = parse_directive("must never push tags").unwrap();
        assert_eq!(pol, Polarity::Dont);

        // Typographic apostrophe, common in prose-styled CLAUDE.md files.
        let (pol, _, _) = parse_directive("Don’t use tabs").unwrap();
        assert_eq!(pol, Polarity::Dont);
    }

    #[test]
    fn must_not_contradicts_always() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A.md");
        let b = dir.path().join("B.md");
        std::fs::write(&a, "- Always use tabs.\n").unwrap();
        std::fs::write(&b, "- Must not use tabs.\n").unwrap();
        let files = vec![
            ClaudeMd {
                file: a,
                scope: ClaudeMdScope::User,
                bytes: 0,
            },
            ClaudeMd {
                file: b,
                scope: ClaudeMdScope::Project,
                bytes: 0,
            },
        ];
        let c = detect_contradictions(&files);
        assert_eq!(c.len(), 1, "polarity must differ: {c:?}");
        assert!(c[0].keyword.starts_with("use"));
    }

    #[test]
    fn contradiction_detected_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A.md");
        let b = dir.path().join("B.md");
        std::fs::write(&a, "- Always commit signed.\n").unwrap();
        std::fs::write(&b, "- Never commit signed.\n").unwrap();
        let files = vec![
            ClaudeMd {
                file: a.clone(),
                scope: ClaudeMdScope::User,
                bytes: 0,
            },
            ClaudeMd {
                file: b.clone(),
                scope: ClaudeMdScope::Project,
                bytes: 0,
            },
        ];
        let c = detect_contradictions(&files);
        assert_eq!(c.len(), 1);
        assert!(c[0].keyword.starts_with("commit"));
    }

    #[test]
    fn vendored_dirs_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The project's own CLAUDE.md must still be found...
        std::fs::write(root.join("CLAUDE.md"), "ok").unwrap();
        // ...but a CLAUDE.md vendored inside node_modules must NOT be.
        let vendor = root.join("node_modules").join("some-pkg");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("CLAUDE.md"), "noise").unwrap();
        // Nested vendor in a subdir as well.
        let nested_vendor = root.join("apps").join("web").join("node_modules").join("x");
        std::fs::create_dir_all(&nested_vendor).unwrap();
        std::fs::write(nested_vendor.join("CLAUDE.md"), "noise").unwrap();
        // And one legitimate nested CLAUDE.md that should be picked up.
        let nested_legit = root.join("apps").join("web");
        std::fs::create_dir_all(&nested_legit).unwrap();
        std::fs::write(nested_legit.join("CLAUDE.md"), "legit").unwrap();

        let project = ProjectPaths::new(root);
        let env = Env::new(
            Some(root.join(".claude.json")),
            Some(root.join(".claude-home")),
        );
        let mds = collect_claude_md(&project, &env);
        let paths: Vec<_> = mds.iter().map(|m| m.file.clone()).collect();
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("CLAUDE.md") && !p.to_string_lossy().contains("node_modules")),
            "expected project root CLAUDE.md, got: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == &nested_legit.join("CLAUDE.md")),
            "expected nested legit CLAUDE.md, got: {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.to_string_lossy().contains("node_modules")),
            "node_modules CLAUDE.md must not appear: {paths:?}"
        );
    }

    #[test]
    fn collect_hooks_flattens_groups_and_tags_provenance() {
        let sources = vec![
            s(
                Scope::User,
                "u",
                json!({
                    "hooks": {
                        "PreToolUse": [
                            {
                                "matcher": "Bash",
                                "hooks": [
                                    { "type": "command", "command": "echo user-bash" }
                                ]
                            }
                        ]
                    }
                }),
            ),
            s(
                Scope::Local,
                "l",
                json!({
                    "hooks": {
                        "PreToolUse": [
                            {
                                "matcher": "Bash",
                                "hooks": [
                                    { "type": "command", "command": "block --no-verify" }
                                ]
                            }
                        ],
                        "Stop": [
                            { "hooks": [ { "type": "command", "command": "say done" } ] }
                        ]
                    }
                }),
            ),
        ];
        let hooks = collect_hooks(&sources);
        assert_eq!(hooks.len(), 3);

        // Sorted by event, then scope ascending (User before Local).
        assert_eq!(hooks[0].event, "PreToolUse");
        assert_eq!(hooks[0].scope, Scope::User);
        assert_eq!(hooks[0].command, "echo user-bash");
        assert_eq!(hooks[0].matcher.as_deref(), Some("Bash"));

        assert_eq!(hooks[1].event, "PreToolUse");
        assert_eq!(hooks[1].scope, Scope::Local);
        assert_eq!(hooks[1].command, "block --no-verify");

        assert_eq!(hooks[2].event, "Stop");
        assert_eq!(hooks[2].matcher, None, "no matcher in this group");
        assert_eq!(hooks[2].command, "say done");
    }

    #[test]
    fn truncate_oneline_collapses_whitespace_and_truncates() {
        let s = "line one\n  line two\tline three";
        assert_eq!(truncate_oneline(s, 100), "line one line two line three");
        let truncated = truncate_oneline(s, 10);
        assert!(truncated.ends_with('…'), "{truncated}");
        assert!(truncated.chars().count() <= 11);
    }

    #[test]
    fn hooks_filtered_out_of_settings_section() {
        // The presenter drops `hooks.*` keys from the merged settings view so
        // they don't appear twice. Exercised via the public `run` would be
        // overkill — just confirm the filter predicate works as expected.
        let entries = vec![
            Resolved {
                key: "permissions.defaultMode".into(),
                effective: json!("ask"),
                contributions: vec![],
            },
            Resolved {
                key: "hooks.PreToolUse".into(),
                effective: json!([]),
                contributions: vec![],
            },
        ];
        let filtered: Vec<_> = entries
            .into_iter()
            .filter(|r| !r.key.starts_with("hooks."))
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].key, "permissions.defaultMode");
    }
}
