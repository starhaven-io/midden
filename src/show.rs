use anyhow::{Context, Result, bail};
use colored::Colorize;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use walkdir::WalkDir;

use crate::claude_json;
use crate::paths::{Env, ProjectPaths, managed_settings_files, serialize_path};
use crate::safe_io;
use crate::secrets;
use crate::terminal;

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
    #[serde(serialize_with = "serialize_path")]
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

    let (sources, source_errors) = settings_sources(env, &project);
    if let Some(error) = source_errors.first() {
        bail!("could not load {}: {}", error.path.display(), error.message);
    }

    let mut hooks = collect_hooks(&sources);
    let resolved: Vec<Resolved> = resolve_settings(&sources)
        .into_iter()
        // Hooks have their own section — drop them from the generic settings
        // dump so they aren't shown twice as opaque JSON blobs.
        .filter(|r| !r.key.starts_with("hooks."))
        .collect();
    let mut claude_mds = collect_claude_md(&project, env, opts.show_secrets);
    let (contradictions, contradictions_truncated) = detect_contradictions(&mut claude_mds);
    let skills = collect_dirs(
        &[env.user_skills_dir(), project.skills_dir()],
        "SKILL.md",
        opts.show_secrets,
    );
    let commands = collect_files(
        &[env.user_commands_dir(), project.commands_dir()],
        opts.show_secrets,
    );
    let agents = collect_files(
        &[env.user_agents_dir(), project.agents_dir()],
        opts.show_secrets,
    );
    let mut mcp_servers = collect_mcp_servers(env, &project)?;
    let worktrees = collect_worktrees(&project, opts.show_secrets);

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
            secrets::mask_tree(&mut r.effective);
            for c in &mut r.contributions {
                secrets::mask_tree(&mut c.value);
            }
        }
        // Hook commands and MCP URLs are free-form text that can embed
        // credentials (Bearer headers, user:pass URLs, token query params).
        for h in &mut hooks {
            h.command = secrets::mask_embedded(&h.command);
            mask_definition(&mut h.definition);
        }
        for s in &mut mcp_servers {
            if let Some(command) = &mut s.command {
                *command = secrets::mask_embedded(command);
            }
            if let Some(url) = &mut s.url {
                *url = secrets::mask_embedded(url);
            }
            mask_definition(&mut s.definition);
        }
    }

    let report = Report {
        root,
        resolved,
        claude_mds,
        contradictions,
        contradictions_truncated,
        skills,
        commands,
        agents,
        hooks,
        mcp_servers,
        worktrees,
    };

    if opts.json {
        emit_json(&report, opts.show_secrets);
    } else {
        emit_human(&report, opts.show_secrets);
    }

    Ok(ExitCode::SUCCESS)
}

fn mask_definition(definition: &mut Value) {
    if let Some(Value::String(command)) = definition.get_mut("command") {
        *command = secrets::mask_embedded(command);
    }
    secrets::mask_tree(definition);
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    read_json_with_limit(path, safe_io::MAX_CONFIG_BYTES)
}

fn read_json_with_limit(path: &Path, limit: usize) -> Result<Option<Value>> {
    let text = match safe_io::read_to_string(path, limit) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let value = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(value))
}

#[derive(Debug)]
pub(crate) struct SettingsReadError {
    pub path: PathBuf,
    pub message: String,
}

pub(crate) fn settings_sources(
    env: &Env,
    project: &ProjectPaths,
) -> (Vec<(Scope, PathBuf, Value)>, Vec<SettingsReadError>) {
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    let mut candidates = vec![
        (Scope::User, env.user_settings()),
        (Scope::Project, project.settings()),
        (Scope::Local, project.local_settings()),
    ];
    let managed = managed_settings_files();
    errors.extend(
        managed
            .errors
            .into_iter()
            .map(|(path, error)| SettingsReadError {
                path,
                message: error.to_string(),
            }),
    );
    candidates.extend(managed.paths.into_iter().map(|path| (Scope::Managed, path)));
    for (scope, path) in candidates {
        match read_json(&path) {
            Ok(Some(value)) => sources.push((scope, path, value)),
            Ok(None) => {}
            Err(error) => errors.push(SettingsReadError {
                path,
                message: format!("{error:#}"),
            }),
        }
    }
    (sources, errors)
}

pub(crate) fn effective_setting(sources: &[(Scope, PathBuf, Value)], key: &str) -> Option<Value> {
    resolve_settings(sources)
        .into_iter()
        .find(|resolved| resolved.key == key)
        .map(|resolved| resolved.effective)
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
    #[serde(serialize_with = "serialize_path")]
    file: PathBuf,
    scope: ClaudeMdScope,
    bytes: u64,
    load_state: ClaudeMdLoadState,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ClaudeMdLoadState {
    Loaded,
    Disabled,
    Unknown,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ClaudeMdScope {
    User,
    Project,
    Local,
    Ancestor,
}

fn collect_claude_md(project: &ProjectPaths, env: &Env, show_secrets: bool) -> Vec<ClaudeMd> {
    let mut out = Vec::new();
    let user = env.user_claude_md();
    if let Some(m) = source_metadata(&user, show_secrets)
        && m.is_file()
    {
        let (load_state, detail) = claude_md_initial_state(m.len());
        out.push(ClaudeMd {
            file: user,
            scope: ClaudeMdScope::User,
            bytes: m.len(),
            load_state,
            detail,
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
            if let Some(m) = source_metadata(&p, show_secrets)
                && m.is_file()
            {
                let (load_state, detail) = claude_md_initial_state(m.len());
                out.push(ClaudeMd {
                    file: p,
                    scope,
                    bytes: m.len(),
                    load_state,
                    detail,
                });
            }
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    out
}

fn claude_md_initial_state(bytes: u64) -> (ClaudeMdLoadState, Option<String>) {
    if bytes > safe_io::MAX_INSTRUCTION_BYTES as u64 {
        (
            ClaudeMdLoadState::Disabled,
            Some(format!(
                "skipped by Claude because the file exceeds the {} byte CLAUDE.md limit",
                safe_io::MAX_INSTRUCTION_BYTES
            )),
        )
    } else {
        (ClaudeMdLoadState::Loaded, None)
    }
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
    #[serde(serialize_with = "serialize_path")]
    a_file: PathBuf,
    #[serde(serialize_with = "serialize_path")]
    b_file: PathBuf,
    a_line: String,
    b_line: String,
    keyword: String,
}

type DirectivesByKeyword = BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>;
const MAX_CONTRADICTIONS: usize = 128;

/// Heuristic CLAUDE.md contradiction detection. We look for imperative lines
/// ("do X", "don't X", "never X", "always X") that share a content keyword
/// across files and disagree on directive polarity. This is best-effort by
/// design — false negatives are common, false positives kept low.
fn detect_contradictions(files: &mut [ClaudeMd]) -> (Vec<Contradiction>, bool) {
    let mut lines_by_file: Vec<(PathBuf, DirectivesByKeyword)> = Vec::new();
    for f in files {
        let text = match safe_io::read_to_string(&f.file, safe_io::MAX_INSTRUCTION_BYTES) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
                f.load_state = ClaudeMdLoadState::Disabled;
                f.detail = Some(format!(
                    "skipped by Claude because the file exceeds the {} byte CLAUDE.md limit; midden did not scan it: {error}",
                    safe_io::MAX_INSTRUCTION_BYTES
                ));
                continue;
            }
            Err(error) => {
                f.load_state = ClaudeMdLoadState::Unknown;
                f.detail = Some(format!("could not inspect for contradictions: {error}"));
                continue;
            }
        };
        let mut entries = DirectivesByKeyword::new();
        for line in text.lines() {
            if let Some((pol, kw, raw)) = parse_directive(line) {
                let (positive, negative) = entries.entry(kw).or_default();
                match pol {
                    Polarity::Do => positive.insert(raw),
                    Polarity::Dont => negative.insert(raw),
                };
            }
        }
        if !entries.is_empty() {
            lines_by_file.push((f.file.clone(), entries));
        }
    }

    let mut out = Vec::new();
    for i in 0..lines_by_file.len() {
        for j in (i + 1)..lines_by_file.len() {
            for (keyword, (a_positive, a_negative)) in &lines_by_file[i].1 {
                let Some((b_positive, b_negative)) = lines_by_file[j].1.get(keyword) else {
                    continue;
                };
                for (a_lines, b_lines) in [(a_positive, b_negative), (a_negative, b_positive)] {
                    for a_line in a_lines {
                        for b_line in b_lines {
                            if out.len() == MAX_CONTRADICTIONS {
                                return (out, true);
                            }
                            out.push(Contradiction {
                                a_file: lines_by_file[i].0.clone(),
                                b_file: lines_by_file[j].0.clone(),
                                a_line: a_line.clone(),
                                b_line: b_line.clone(),
                                keyword: keyword.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
    (out, false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Polarity {
    Do,
    Dont,
}

/// Parse a single line for a coarse imperative directive. Returns `(polarity,
/// content-keyword, original-line)`. The content-keyword combines the first
/// two significant words after the polarity verb, lowercased.
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
    } else {
        let rest = lower.strip_prefix("must ")?;
        (Polarity::Do, rest)
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
    #[serde(serialize_with = "serialize_path")]
    file: PathBuf,
    scope: &'static str,
}

fn collect_dirs(roots: &[PathBuf], required_file: &str, show_secrets: bool) -> Vec<LocatedDir> {
    let mut out = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let scope = if i == 0 { "user" } else { "project" };
        let Some(entries) = source_entries(root, show_secrets) else {
            continue;
        };
        let mut found: Vec<LocatedDir> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warn_source(root, &error, show_secrets);
                    continue;
                }
            };
            let path = entry.path();
            if source_metadata(&path, show_secrets).is_some_and(|metadata| metadata.is_dir())
                && source_metadata(&path.join(required_file), show_secrets)
                    .is_some_and(|metadata| metadata.is_file())
            {
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
    #[serde(serialize_with = "serialize_path")]
    file: PathBuf,
    scope: &'static str,
}

fn collect_files(roots: &[PathBuf], show_secrets: bool) -> Vec<LocatedFile> {
    let mut out = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let scope = if i == 0 { "user" } else { "project" };
        let Some(metadata) = source_metadata(root, show_secrets) else {
            continue;
        };
        if !metadata.is_dir() {
            warn_source(root, "expected a directory", show_secrets);
            continue;
        }
        let walker = WalkDir::new(root)
            .max_depth(3)
            .into_iter()
            .filter_entry(|e| !is_vendored_dir(e.path()));
        let mut found: Vec<LocatedFile> = Vec::new();
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warn_source(error.path().unwrap_or(root), &error, show_secrets);
                    continue;
                }
            };
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("md")
                && source_metadata(p, show_secrets).is_some_and(|metadata| metadata.is_file())
            {
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
    /// Handler type, including command, HTTP, MCP, prompt, and agent hooks.
    kind: String,
    /// The command/script body. Long values are kept full in JSON; the human
    /// presenter truncates.
    command: String,
    /// Complete handler configuration, including non-command hook fields.
    definition: Value,
    scope: Scope,
    #[serde(serialize_with = "serialize_path")]
    file: PathBuf,
}

/// Pull every individual hook entry out of every settings source. Each
/// `hooks.<EventName>` array contains matcher-groups, and each group's inner
/// `hooks` array contains one or more handlers.
fn collect_hooks(sources: &[(Scope, PathBuf, Value)]) -> Vec<Hook> {
    let mut out = Vec::new();
    let mut seen_handlers: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
                    // Claude runs a handler only once when the same handler is
                    // contributed by multiple settings files or matching
                    // groups. Matcher provenance does not form part of the
                    // handler identity.
                    let identity = hook_handler_identity(entry);
                    let seen = seen_handlers.entry(event_name.clone()).or_default();
                    if seen.contains(&identity) {
                        continue;
                    }
                    seen.push(identity);
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
                        definition: entry.clone(),
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

fn hook_summary(hook: &Hook) -> String {
    let field = match hook.kind.as_str() {
        "command" => return hook.command.clone(),
        "http" => "url",
        "prompt" | "agent" => "prompt",
        _ => return format_value(&hook.definition),
    };
    hook.definition
        .get(field)
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format_value(&hook.definition))
}

fn hook_handler_identity(entry: &Value) -> String {
    let kind = entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("command");
    let fields: &[&str] = match kind {
        // Presentation-only metadata such as timeout and statusMessage does
        // not turn the same operation into a second handler.
        "command" => &["command", "args", "shell", "async", "asyncRewake", "if"],
        "http" => &["url", "headers", "allowedEnvVars", "if"],
        "mcp_tool" => &["server", "tool", "input", "if"],
        "prompt" | "agent" => &["prompt", "model", "if"],
        _ => return serde_json::to_string(entry).unwrap_or_default(),
    };
    let mut identity = serde_json::Map::new();
    identity.insert("type".into(), Value::String(kind.to_string()));
    for field in fields {
        if let Some(value) = entry.get(*field) {
            identity.insert((*field).to_string(), value.clone());
        }
    }
    serde_json::to_string(&identity).unwrap_or_default()
}

#[derive(Debug, Serialize)]
struct McpServer {
    name: String,
    scope: &'static str,
    #[serde(serialize_with = "serialize_path")]
    file: PathBuf,
    command: Option<String>,
    url: Option<String>,
    disabled: bool,
    definition: Value,
}

fn collect_mcp_servers(env: &Env, project: &ProjectPaths) -> Result<Vec<McpServer>> {
    let mut out = Vec::new();
    // User and local scope both live in ~/.claude.json: the top-level
    // `mcpServers` map is user scope; the per-project entry's `mcpServers` is
    // local scope — the default destination of `claude mcp add`.
    if let Some(claude) = read_json_with_limit(&env.claude_json, safe_io::MAX_CLAUDE_JSON_BYTES)? {
        push_mcp_servers(claude.get("mcpServers"), "user", &env.claude_json, &mut out);
        let local = claude_json::project_entry(&claude, &project.root)
            .and_then(|entry| entry.get("mcpServers"));
        push_mcp_servers(local, "local", &env.claude_json, &mut out);
    }
    for (scope, path) in [
        ("project", project.mcp_json()),
        ("managed", project.managed_mcp_json()),
    ] {
        if let Some(v) = read_json(&path)? {
            push_mcp_servers(v.get("mcpServers"), scope, &path, &mut out);
        }
    }
    Ok(out)
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
            definition: def.clone(),
        });
    }
}

#[derive(Debug, Serialize)]
struct Worktree {
    name: String,
    #[serde(serialize_with = "serialize_path")]
    file: PathBuf,
}

fn collect_worktrees(project: &ProjectPaths, show_secrets: bool) -> Vec<Worktree> {
    let dir = project.worktrees_dir();
    let Some(entries) = source_entries(&dir, show_secrets) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn_source(&dir, &error, show_secrets);
                continue;
            }
        };
        let path = entry.path();
        if source_metadata(&path, show_secrets).is_some_and(|metadata| metadata.is_dir()) {
            out.push(Worktree {
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                file: path,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn warn_source(path: &Path, error: impl std::fmt::Display, show_secrets: bool) {
    eprintln!(
        "warning: could not inspect {}: {}",
        display_path(path, show_secrets),
        display_text(&error.to_string(), show_secrets)
    );
}

fn source_metadata(path: &Path, show_secrets: bool) -> Option<std::fs::Metadata> {
    match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            warn_source(path, error, show_secrets);
            None
        }
    }
}

fn source_entries(path: &Path, show_secrets: bool) -> Option<std::fs::ReadDir> {
    match std::fs::read_dir(path) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            warn_source(path, error, show_secrets);
            None
        }
    }
}

/// Everything `show` resolved for a target directory. Field order is the JSON
/// emission order; `settings`/`claude_md` keep their original key names.
#[derive(Serialize)]
struct Report {
    #[serde(serialize_with = "serialize_path")]
    root: PathBuf,
    #[serde(rename = "settings")]
    resolved: Vec<Resolved>,
    #[serde(rename = "claude_md")]
    claude_mds: Vec<ClaudeMd>,
    contradictions: Vec<Contradiction>,
    contradictions_truncated: bool,
    skills: Vec<LocatedDir>,
    commands: Vec<LocatedFile>,
    agents: Vec<LocatedFile>,
    hooks: Vec<Hook>,
    mcp_servers: Vec<McpServer>,
    worktrees: Vec<Worktree>,
}

// -- presentation ------------------------------------------------------------

fn emit_human(report: &Report, show_secrets: bool) {
    let Report {
        root,
        resolved,
        claude_mds,
        contradictions,
        contradictions_truncated,
        skills,
        commands,
        agents,
        hooks,
        mcp_servers,
        worktrees,
    } = report;
    println!(
        "{} {}",
        "resolved for".bold(),
        display_path(root, show_secrets)
    );
    println!();

    println!("{}", "settings".bold().underline());
    if resolved.is_empty() {
        println!("  (no settings found)");
    } else {
        for r in resolved {
            let val_str = format_value(&r.effective);
            println!(
                "  {} = {}",
                display_text(&r.key, show_secrets).cyan(),
                terminal::escape(&val_str)
            );
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
                    display_path(&c.file, show_secrets),
                    terminal::escape(&format_value(&c.value)).dimmed()
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
            println!(
                "  [{scope}; {}] {} ({} bytes)",
                match c.load_state {
                    ClaudeMdLoadState::Loaded => "loaded",
                    ClaudeMdLoadState::Disabled => "disabled",
                    ClaudeMdLoadState::Unknown => "load unknown",
                },
                display_path(&c.file, show_secrets),
                c.bytes
            );
            if let Some(detail) = &c.detail {
                println!("    {}", display_text(detail, show_secrets).dimmed());
            }
        }
    }
    if !contradictions.is_empty() {
        println!();
        println!("  {}", "contradictions:".yellow().bold());
        for c in contradictions {
            println!("    keyword `{}`", display_text(&c.keyword, show_secrets));
            println!(
                "      {} — {}",
                display_path(&c.a_file, show_secrets),
                display_text(&c.a_line, show_secrets).dimmed()
            );
            println!(
                "      {} — {}",
                display_path(&c.b_file, show_secrets),
                display_text(&c.b_line, show_secrets).dimmed()
            );
        }
    }
    if *contradictions_truncated {
        println!("  additional contradictions omitted (limit: {MAX_CONTRADICTIONS})");
    }
    println!();

    print_section(
        "skills",
        skills
            .iter()
            .map(|s| (s.name.as_str(), s.file.as_path(), s.scope)),
        show_secrets,
    );
    print_section(
        "commands",
        commands
            .iter()
            .map(|c| (c.name.as_str(), c.file.as_path(), c.scope)),
        show_secrets,
    );
    print_section(
        "agents",
        agents
            .iter()
            .map(|a| (a.name.as_str(), a.file.as_path(), a.scope)),
        show_secrets,
    );

    println!("{}", "hooks".bold().underline());
    if hooks.is_empty() {
        println!("  (none)");
    } else {
        let mut current_event = "";
        for h in hooks {
            if h.event != current_event {
                println!("  {}", display_text(&h.event, show_secrets).bold());
                current_event = &h.event;
            }
            let matcher = h.matcher.as_deref().unwrap_or("*");
            println!(
                "    [{}] {} ({}): {}",
                h.scope.label(),
                display_text(matcher, show_secrets).cyan(),
                display_text(&h.kind, show_secrets),
                display_text(&truncate_oneline(&hook_summary(h), 80), show_secrets)
            );
            println!("      {}", display_path(&h.file, show_secrets).dimmed());
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
            println!(
                "  [{}] {} -> {}{dis}",
                s.scope,
                display_text(&s.name, show_secrets),
                display_text(target, show_secrets)
            );
            println!("    {}", display_path(&s.file, show_secrets).dimmed());
        }
    }
    println!();

    println!("{}", "worktrees".bold().underline());
    if worktrees.is_empty() {
        println!("  (none)");
    } else {
        for w in worktrees {
            println!(
                "  {} — {}",
                display_text(&w.name, show_secrets),
                display_path(&w.file, show_secrets)
            );
        }
    }
}

fn print_section<'a>(
    title: &str,
    iter: impl Iterator<Item = (&'a str, &'a Path, &'a str)>,
    show_secrets: bool,
) {
    println!("{}", title.bold().underline());
    let mut empty = true;
    for (name, file, scope) in iter {
        empty = false;
        println!("  [{scope}] {}", display_text(name, show_secrets));
        println!("    {}", display_path(file, show_secrets).dimmed());
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

fn display_text(value: &str, show_secrets: bool) -> String {
    let value = if show_secrets {
        value.to_string()
    } else {
        secrets::mask_embedded(value)
    };
    terminal::escape(&value)
}

fn display_path(path: &Path, show_secrets: bool) -> String {
    display_text(&path.display().to_string(), show_secrets)
}

fn emit_json(report: &Report, show_secrets: bool) {
    let mut value = serde_json::to_value(report).expect("serialize");
    if !show_secrets {
        secrets::mask_sensitive_values(&mut value);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&value).expect("serialize")
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

        let (pol, kw, _) = parse_directive("- Must sign every commit").unwrap();
        assert_eq!(pol, Polarity::Do, "bare must is positive");
        assert!(kw.starts_with("sign"));

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
        let mut files = vec![
            ClaudeMd {
                file: a,
                scope: ClaudeMdScope::User,
                bytes: 0,
                load_state: ClaudeMdLoadState::Loaded,
                detail: None,
            },
            ClaudeMd {
                file: b,
                scope: ClaudeMdScope::Project,
                bytes: 0,
                load_state: ClaudeMdLoadState::Loaded,
                detail: None,
            },
        ];
        let (c, truncated) = detect_contradictions(&mut files);
        assert!(!truncated);
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
        let mut files = vec![
            ClaudeMd {
                file: a.clone(),
                scope: ClaudeMdScope::User,
                bytes: 0,
                load_state: ClaudeMdLoadState::Loaded,
                detail: None,
            },
            ClaudeMd {
                file: b.clone(),
                scope: ClaudeMdScope::Project,
                bytes: 0,
                load_state: ClaudeMdLoadState::Loaded,
                detail: None,
            },
        ];
        let (c, truncated) = detect_contradictions(&mut files);
        assert!(!truncated);
        assert_eq!(c.len(), 1);
        assert!(c[0].keyword.starts_with("commit"));
    }

    #[test]
    fn descendant_claude_md_files_are_not_active_for_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("CLAUDE.md"), "ok").unwrap();
        let vendor = root.join("node_modules").join("some-pkg");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("CLAUDE.md"), "noise").unwrap();
        let nested_legit = root.join("apps").join("web");
        std::fs::create_dir_all(&nested_legit).unwrap();
        std::fs::write(nested_legit.join("CLAUDE.md"), "legit").unwrap();

        let project = ProjectPaths::new(root);
        let env = Env::new(
            Some(root.join(".claude.json")),
            Some(root.join(".claude-home")),
        );
        let mds = collect_claude_md(&project, &env, false);
        let paths: Vec<_> = mds.iter().map(|m| m.file.clone()).collect();
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("CLAUDE.md") && !p.to_string_lossy().contains("node_modules")),
            "expected project root CLAUDE.md, got: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p == &nested_legit.join("CLAUDE.md")),
            "descendant CLAUDE.md should apply only when that descendant is targeted: {paths:?}"
        );
    }

    #[test]
    fn ancestor_claude_md_files_are_active_for_nested_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("CLAUDE.md"), "root").unwrap();
        let nested = root.join("apps").join("web");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("CLAUDE.md"), "nested").unwrap();

        let project = ProjectPaths::new(&nested);
        let env = Env::new(
            Some(root.join(".claude.json")),
            Some(root.join(".claude-home")),
        );
        let mds = collect_claude_md(&project, &env, false);
        let paths: Vec<_> = mds.iter().map(|m| m.file.clone()).collect();

        assert!(paths.iter().any(|p| p == &root.join("CLAUDE.md")));
        assert!(paths.iter().any(|p| p == &nested.join("CLAUDE.md")));
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
    #[cfg(unix)]
    #[test]
    fn report_serialization_accepts_non_unicode_paths() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
        let report = Report {
            root: path.clone(),
            resolved: vec![Resolved {
                key: "model".into(),
                effective: json!("example"),
                contributions: vec![Contribution {
                    scope: Scope::Project,
                    file: path.clone(),
                    value: json!("example"),
                    shadowed: false,
                }],
            }],
            claude_mds: vec![],
            contradictions: vec![],
            contradictions_truncated: false,
            skills: vec![],
            commands: vec![],
            agents: vec![],
            hooks: vec![],
            mcp_servers: vec![],
            worktrees: vec![],
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["root"], path.display().to_string());
        assert_eq!(
            value["settings"][0]["contributions"][0]["file"],
            path.display().to_string()
        );
    }
}
