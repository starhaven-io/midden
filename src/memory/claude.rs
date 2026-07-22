use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use super::{
    Adapter, Association, DiscoveryRequest, HumanDetail, LoadState, MemoryState, Provider,
    ProviderInventory, Scope, SourceKind, SourceRole, SourceSpec, Warning,
};
use crate::git;
use crate::paths::{self, Env, ProjectPaths};
use crate::show;
use crate::transcripts;

const MAX_IMPORT_DEPTH: usize = 5;
const MAX_MEMORY_FILES: usize = 1024;
const MAX_PROJECT_DIRS: usize = 4096;
const MAX_PROJECT_TRANSCRIPTS: usize = 16;
const MAX_RULE_ENTRIES: usize = 4096;

pub(super) struct ClaudeAdapter<'a> {
    env: &'a Env,
}

#[derive(Clone, Copy)]
struct InstructionContext<'a> {
    scope: Scope,
    association: Association,
    exclusions: Option<&'a GlobSet>,
    trust_root: Option<&'a Path>,
}

impl<'a> ClaudeAdapter<'a> {
    pub(super) fn new(env: &'a Env) -> Self {
        Self { env }
    }
}

impl Adapter for ClaudeAdapter<'_> {
    fn discover(&self, request: &DiscoveryRequest<'_>) -> ProviderInventory {
        let repository_root =
            git::repository_root(request.target).unwrap_or_else(|| request.target.to_path_buf());
        let project = ProjectPaths::new(&repository_root);
        let settings = show::settings_sources(self.env, &project);
        let malformed_settings = malformed_settings(self.env, &project);
        let memory_enabled = show::effective_setting(&settings, "autoMemoryEnabled");
        let invalid_memory_enabled = memory_enabled
            .as_ref()
            .is_some_and(|value| !value.is_boolean());
        let memory_state = if malformed_settings || invalid_memory_enabled {
            MemoryState::Unknown
        } else if memory_enabled
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            MemoryState::Enabled
        } else {
            MemoryState::Disabled
        };
        let configured = self.env.claude_home.is_dir()
            || settings
                .iter()
                .any(|(_, _, value)| has_relevant_setting(value));
        let mut inventory = ProviderInventory::new(Provider::Claude, configured, memory_state);

        if malformed_settings {
            inventory.warnings.push(Warning::new(
                "invalid-claude-settings",
                "could not parse one or more Claude settings files; auto-memory state is unknown",
            ));
        }
        if invalid_memory_enabled {
            inventory.warnings.push(Warning::new(
                "invalid-auto-memory-enabled",
                "autoMemoryEnabled must be a boolean; auto-memory state is unknown",
            ));
        }
        collect_operational_sources(&mut inventory, &settings);
        let exclusions = build_exclusions(&mut inventory, &settings);
        warn_unsupported_auto_memory_directories(&mut inventory, &settings);

        let mut imported = BTreeSet::new();
        for path in managed_claude_md_paths() {
            push_instruction_with_imports(
                &mut inventory,
                path,
                InstructionContext {
                    scope: Scope::Managed,
                    association: Association::Global,
                    exclusions: None,
                    trust_root: None,
                },
                &mut imported,
            );
        }
        push_instruction_with_imports(
            &mut inventory,
            self.env.user_claude_md(),
            InstructionContext {
                scope: Scope::Global,
                association: Association::Global,
                exclusions: Some(&exclusions),
                trust_root: None,
            },
            &mut imported,
        );
        collect_rules(
            &mut inventory,
            &self.env.claude_home.join("rules"),
            InstructionContext {
                scope: Scope::Global,
                association: Association::Global,
                exclusions: Some(&exclusions),
                trust_root: None,
            },
            &mut imported,
        );

        for directory in ancestor_chain(request.target) {
            let scope = instruction_scope(&directory, &repository_root);
            push_instruction_with_imports(
                &mut inventory,
                directory.join("CLAUDE.md"),
                InstructionContext {
                    scope,
                    association: Association::Target,
                    exclusions: Some(&exclusions),
                    trust_root: Some(&repository_root),
                },
                &mut imported,
            );
            if directory == repository_root {
                push_instruction_with_imports(
                    &mut inventory,
                    directory.join(".claude").join("CLAUDE.md"),
                    InstructionContext {
                        scope: Scope::Repository,
                        association: Association::Target,
                        exclusions: Some(&exclusions),
                        trust_root: Some(&repository_root),
                    },
                    &mut imported,
                );
            }
            push_instruction_with_imports(
                &mut inventory,
                directory.join("CLAUDE.local.md"),
                InstructionContext {
                    scope,
                    association: Association::Target,
                    exclusions: Some(&exclusions),
                    trust_root: Some(&repository_root),
                },
                &mut imported,
            );
        }
        collect_rules(
            &mut inventory,
            &repository_root.join(".claude").join("rules"),
            InstructionContext {
                scope: Scope::Repository,
                association: Association::Target,
                exclusions: Some(&exclusions),
                trust_root: Some(&repository_root),
            },
            &mut imported,
        );

        let custom_memory_dir = effective_auto_memory_directory(&settings);
        if let Some(value) = custom_memory_dir.as_ref().and_then(Value::as_str) {
            match expand_memory_dir(value) {
                Some(path) => collect_memory_dir(
                    &mut inventory,
                    &path,
                    Association::Target,
                    memory_state,
                    request.include_unassociated,
                    Some("configured by autoMemoryDirectory".to_string()),
                    Some("configured by autoMemoryDirectory".to_string()),
                ),
                None => inventory.warnings.push(Warning::new(
                    "invalid-auto-memory-directory",
                    format!("autoMemoryDirectory must be absolute or start with ~/: {value:?}"),
                )),
            }
        } else if custom_memory_dir.is_some() {
            inventory.warnings.push(Warning::new(
                "invalid-auto-memory-directory",
                "autoMemoryDirectory must be a string; the memory location is unknown",
            ));
        } else {
            collect_default_memory_dirs(
                &mut inventory,
                self.env,
                request,
                &repository_root,
                memory_state,
            );
        }

        inventory.configured = inventory.configured || !inventory.sources.is_empty();
        inventory
    }
}

fn collect_operational_sources(
    inventory: &mut ProviderInventory,
    settings: &[(show::Scope, PathBuf, Value)],
) {
    for (scope, path, value) in settings {
        if !has_relevant_setting(value) {
            continue;
        }
        let (scope, association) = match scope {
            show::Scope::User => (Scope::Global, Association::Global),
            show::Scope::Project | show::Scope::Local => (Scope::Repository, Association::Target),
            show::Scope::Managed => (Scope::Managed, Association::Global),
        };
        inventory.push_path(
            path.clone(),
            SourceSpec::new(
                SourceRole::OperationalState,
                SourceKind::Configuration,
                scope,
                LoadState::Loaded,
                association,
            ),
        );
    }
}

fn has_relevant_setting(value: &Value) -> bool {
    value.get("autoMemoryEnabled").is_some()
        || value.get("autoMemoryDirectory").is_some()
        || value.get("claudeMdExcludes").is_some()
}

fn effective_auto_memory_directory(settings: &[(show::Scope, PathBuf, Value)]) -> Option<Value> {
    let supported = settings
        .iter()
        .filter(|(scope, _, _)| matches!(scope, show::Scope::User | show::Scope::Managed))
        .cloned()
        .collect::<Vec<_>>();
    show::effective_setting(&supported, "autoMemoryDirectory")
}

fn warn_unsupported_auto_memory_directories(
    inventory: &mut ProviderInventory,
    settings: &[(show::Scope, PathBuf, Value)],
) {
    for (scope, path, value) in settings {
        if matches!(scope, show::Scope::Project | show::Scope::Local)
            && value.get("autoMemoryDirectory").is_some()
        {
            inventory.warnings.push(Warning::at(
                "unsupported-auto-memory-directory-scope",
                "Claude ignores autoMemoryDirectory outside managed and user settings",
                path.clone(),
            ));
        }
    }
}

fn build_exclusions(
    inventory: &mut ProviderInventory,
    settings: &[(show::Scope, PathBuf, Value)],
) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    let Some(value) = show::effective_setting(settings, "claudeMdExcludes") else {
        return builder.build().expect("empty glob set");
    };
    let Some(patterns) = value.as_array() else {
        inventory.warnings.push(Warning::new(
            "invalid-claude-md-excludes",
            "claudeMdExcludes must be an array of glob strings",
        ));
        return builder.build().expect("empty glob set");
    };
    for pattern in patterns {
        let Some(pattern) = pattern.as_str() else {
            inventory.warnings.push(Warning::new(
                "invalid-claude-md-exclude",
                "ignored non-string claudeMdExcludes entry",
            ));
            continue;
        };
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(error) => inventory.warnings.push(Warning::new(
                "invalid-claude-md-exclude",
                format!("ignored {pattern:?}: {error}"),
            )),
        }
    }
    builder.build().unwrap_or_else(|error| {
        inventory.warnings.push(Warning::new(
            "invalid-claude-md-excludes",
            error.to_string(),
        ));
        GlobSetBuilder::new().build().expect("empty glob set")
    })
}

fn malformed_settings(env: &Env, project: &ProjectPaths) -> bool {
    let mut settings = vec![
        env.user_settings(),
        project.settings(),
        project.local_settings(),
    ];
    settings.extend(paths::managed_settings_files());
    settings.iter().any(|path| {
        fs::read_to_string(path)
            .ok()
            .is_some_and(|raw| serde_json::from_str::<Value>(&raw).is_err())
    })
}

fn push_instruction_with_imports(
    inventory: &mut ProviderInventory,
    path: PathBuf,
    context: InstructionContext<'_>,
    imported: &mut BTreeSet<PathBuf>,
) {
    if !path.is_file() {
        return;
    }
    if is_excluded(context, &path) {
        inventory.push_path(
            path,
            SourceSpec::new(
                SourceRole::Authority,
                SourceKind::Instruction,
                context.scope,
                LoadState::Disabled,
                context.association,
            )
            .with_detail("excluded by claudeMdExcludes"),
        );
        return;
    }
    if !promote_imported_instruction(inventory, &path, context) {
        inventory.push_path(
            path.clone(),
            SourceSpec::new(
                SourceRole::Authority,
                SourceKind::Instruction,
                context.scope,
                LoadState::Loaded,
                context.association,
            ),
        );
    }
    collect_imports(inventory, &path, context, 0, imported);
}

fn promote_imported_instruction(
    inventory: &mut ProviderInventory,
    path: &Path,
    context: InstructionContext<'_>,
) -> bool {
    let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Some(source) = inventory
        .sources
        .iter_mut()
        .find(|source| source.kind == SourceKind::ImportedInstruction && source.path == identity)
    else {
        return false;
    };
    source.kind = SourceKind::Instruction;
    source.scope = context.scope;
    source.load_state = LoadState::Loaded;
    source.association = context.association;
    source.detail = source.detail.take().map(|detail| format!("also {detail}"));
    true
}

fn collect_imports(
    inventory: &mut ProviderInventory,
    source: &Path,
    context: InstructionContext<'_>,
    depth: usize,
    imported: &mut BTreeSet<PathBuf>,
) {
    imported.insert(
        source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf()),
    );
    let Ok(raw) = fs::read_to_string(source) else {
        inventory.warnings.push(Warning::at(
            "source-inaccessible",
            "could not read instruction imports",
            source.to_path_buf(),
        ));
        return;
    };
    let imports = imports_from_text(&raw);
    if depth >= MAX_IMPORT_DEPTH && !imports.is_empty() {
        inventory.warnings.push(Warning::at(
            "claude-import-depth-exceeded",
            format!("imports beyond {MAX_IMPORT_DEPTH} hops were not followed"),
            source.to_path_buf(),
        ));
        return;
    }

    for value in imports {
        let path = resolve_import(source, &value);
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !imported.insert(identity.clone()) {
            continue;
        }
        if !path.is_file() {
            inventory.warnings.push(Warning::at(
                "claude-import-missing",
                format!("imported by {}", source.display()),
                path,
            ));
            continue;
        }
        let external = is_external(context, &path);
        let detail = if external {
            format!(
                "imported by {}; external import approval is unresolved",
                source.display()
            )
        } else {
            format!("imported by {}", source.display())
        };
        inventory.push_path(
            identity,
            SourceSpec::new(
                SourceRole::Authority,
                SourceKind::ImportedInstruction,
                context.scope,
                if external {
                    LoadState::Unknown
                } else {
                    LoadState::Loaded
                },
                context.association,
            )
            .with_detail(detail),
        );
        collect_imports(inventory, &path, context, depth + 1, imported);
    }
}

fn is_excluded(context: InstructionContext<'_>, path: &Path) -> bool {
    context
        .exclusions
        .is_some_and(|exclusions| exclusions.is_match(path))
}

fn is_external(context: InstructionContext<'_>, path: &Path) -> bool {
    let Some(root) = context.trust_root else {
        return false;
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    !path.starts_with(root)
}

fn imports_from_text(raw: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut fence: Option<&str> = None;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }

        let visible = line
            .split('`')
            .enumerate()
            .filter_map(|(index, segment)| (index % 2 == 0).then_some(segment))
            .collect::<Vec<_>>()
            .join(" ");
        for token in visible.split_whitespace() {
            let token = token.trim_matches(|character: char| {
                matches!(
                    character,
                    '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '"' | '\''
                )
            });
            let token = token.trim_end_matches(['.', ':', '!', '?']);
            if let Some(path) = token.strip_prefix('@')
                && !path.is_empty()
            {
                imports.push(path.to_string());
            }
        }
    }
    imports
}

fn resolve_import(source: &Path, value: &str) -> PathBuf {
    if value == "~" {
        return paths::home_dir();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return paths::home_dir().join(rest);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        source.parent().unwrap_or_else(|| Path::new(".")).join(path)
    }
}

fn collect_rules(
    inventory: &mut ProviderInventory,
    directory: &Path,
    context: InstructionContext<'_>,
    imported: &mut BTreeSet<PathBuf>,
) {
    collect_rules_with_limit(inventory, directory, context, imported, MAX_RULE_ENTRIES);
}

fn collect_rules_with_limit(
    inventory: &mut ProviderInventory,
    directory: &Path,
    context: InstructionContext<'_>,
    imported: &mut BTreeSet<PathBuf>,
    max_entries: usize,
) {
    if !directory.is_dir() {
        return;
    }
    let walker = WalkDir::new(directory).follow_links(true).max_depth(16);
    let mut entries = Vec::new();
    for (inspected, entry) in walker.into_iter().enumerate() {
        if inspected == max_entries {
            inventory.warnings.push(Warning::at(
                "claude-rule-entry-limit",
                format!(
                    "only the first {max_entries} filesystem-enumerated rule entries were inspected; selection may vary when truncated"
                ),
                directory.to_path_buf(),
            ));
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                inventory
                    .warnings
                    .push(Warning::new("claude-rule-inaccessible", error.to_string()));
                continue;
            }
        };
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    for entry in entries {
        let path = entry.path();
        if !entry.file_type().is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("md")
        {
            continue;
        }
        let excluded = is_excluded(context, path);
        let load_state = if excluded {
            LoadState::Disabled
        } else if rule_is_path_scoped(path) {
            LoadState::OnDemand
        } else {
            LoadState::Loaded
        };
        let spec = SourceSpec::new(
            SourceRole::Authority,
            SourceKind::Rule,
            context.scope,
            load_state,
            context.association,
        );
        inventory.push_path(
            path.to_path_buf(),
            if excluded {
                spec.with_detail("excluded by claudeMdExcludes")
            } else {
                spec
            },
        );
        if !excluded {
            collect_imports(inventory, path, context, 0, imported);
        }
    }
}

fn rule_is_path_scoped(path: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if line.starts_with("paths:") {
            return true;
        }
    }
    false
}

fn collect_default_memory_dirs(
    inventory: &mut ProviderInventory,
    env: &Env,
    request: &DiscoveryRequest<'_>,
    repository_root: &Path,
    memory_state: MemoryState,
) {
    let projects = env.claude_home.join("projects");
    let Ok(entries) = fs::read_dir(&projects) else {
        return;
    };
    let target_identity = git::repository_identity(repository_root);
    let mut identity_cache = BTreeMap::new();
    let mut directories = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("memory").is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    if directories.len() > MAX_PROJECT_DIRS {
        directories.truncate(MAX_PROJECT_DIRS);
        inventory.warnings.push(Warning::at(
            "claude-project-directory-limit",
            format!("only the first {MAX_PROJECT_DIRS} project memory directories were inspected"),
            projects,
        ));
    }

    for directory in directories {
        let (cwds, sampled) = match transcripts::project_cwds(&directory, MAX_PROJECT_TRANSCRIPTS) {
            Ok(result) => result,
            Err(error) => {
                inventory.warnings.push(Warning::at(
                    "claude-project-association-unavailable",
                    error.to_string(),
                    directory,
                ));
                continue;
            }
        };
        let association = classify_cwds(
            repository_root,
            target_identity.as_deref(),
            &cwds,
            &mut identity_cache,
        );
        if association != Association::Target && !request.include_unassociated {
            if cwds.is_empty() {
                inventory.warnings.push(Warning::at(
                    "claude-memory-unassociated",
                    "memory is present but no transcript cwd evidence can associate it with the target; rerun with --all",
                    directory.join("memory"),
                ));
            }
            continue;
        }
        let detail = cwd_detail(&cwds, sampled);
        let human_detail = human_cwd_detail(repository_root, association, &cwds, sampled);
        collect_memory_dir(
            inventory,
            &directory.join("memory"),
            association,
            memory_state,
            request.include_unassociated,
            detail,
            human_detail,
        );
    }
}

fn classify_cwds(
    target_root: &Path,
    target_identity: Option<&Path>,
    cwds: &[PathBuf],
    identity_cache: &mut BTreeMap<PathBuf, Option<PathBuf>>,
) -> Association {
    if cwds.is_empty() {
        return Association::Unknown;
    }
    let matches = cwds
        .iter()
        .map(|cwd| same_repository(target_root, target_identity, cwd, identity_cache))
        .collect::<Vec<_>>();
    if matches.iter().all(|result| *result == Some(true)) {
        Association::Target
    } else if matches.iter().all(|result| *result == Some(false)) {
        Association::Other
    } else {
        Association::Unknown
    }
}

fn same_repository(
    target_root: &Path,
    target_identity: Option<&Path>,
    cwd: &Path,
    identity_cache: &mut BTreeMap<PathBuf, Option<PathBuf>>,
) -> Option<bool> {
    let candidate_root = cwd.canonicalize().ok();
    if candidate_root.as_deref() == Some(target_root) {
        return Some(true);
    }
    let target_identity = target_identity?;
    let candidate_root = candidate_root.unwrap_or_else(|| cwd.to_path_buf());
    let candidate_identity = identity_cache
        .entry(candidate_root.clone())
        .or_insert_with(|| git::repository_identity(&candidate_root));
    Some(candidate_identity.as_deref()? == target_identity)
}

fn cwd_detail(cwds: &[PathBuf], sampled: bool) -> Option<String> {
    labeled_cwd_detail(cwds, sampled, "associated")
}

fn labeled_cwd_detail(cwds: &[PathBuf], sampled: bool, label: &str) -> Option<String> {
    let first = cwds.first()?;
    let mut detail = if cwds.len() == 1 {
        format!("{label} cwd: {}", first.display())
    } else {
        format!("{} {label} cwds; first: {}", cwds.len(), first.display())
    };
    if sampled {
        detail.push_str(&format!(
            "; sampled first {MAX_PROJECT_TRANSCRIPTS} transcripts"
        ));
    }
    Some(detail)
}

fn human_cwd_detail(
    target_root: &Path,
    association: Association,
    cwds: &[PathBuf],
    sampled: bool,
) -> Option<String> {
    // "associated" would overclaim for a dir whose cwd evidence could not be
    // matched to the target; label the unresolved case as evidence instead.
    let label = if association == Association::Unknown {
        "evidence"
    } else {
        "associated"
    };
    let detail = labeled_cwd_detail(cwds, sampled, label).or_else(|| {
        (association == Association::Unknown)
            .then(|| "association unknown: no transcript cwd evidence".to_string())
    })?;
    let repeats_target = association == Association::Target
        && cwds.len() == 1
        && !sampled
        && same_path(&cwds[0], target_root);
    (!repeats_target).then_some(detail)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn collect_memory_dir(
    inventory: &mut ProviderInventory,
    directory: &Path,
    association: Association,
    memory_state: MemoryState,
    include_unknown: bool,
    association_detail: Option<String>,
    human_directory_detail: Option<String>,
) {
    if !directory.is_dir() {
        return;
    }
    let walker = WalkDir::new(directory)
        .follow_links(false)
        .max_depth(8)
        .sort_by_file_name();
    let mut files = Vec::new();
    for entry in walker {
        match entry {
            Ok(entry) if entry.file_type().is_file() => files.push(entry.into_path()),
            Ok(_) => {}
            Err(error) => inventory.warnings.push(Warning::at(
                "claude-memory-source-inaccessible",
                error.to_string(),
                directory.to_path_buf(),
            )),
        }
    }
    if files.len() > MAX_MEMORY_FILES {
        files.truncate(MAX_MEMORY_FILES);
        inventory.warnings.push(Warning::at(
            "claude-memory-file-limit",
            format!("only the first {MAX_MEMORY_FILES} files were inventoried"),
            directory.to_path_buf(),
        ));
    }

    let human_detail_anchor = human_directory_detail.as_ref().and_then(|_| {
        files
            .iter()
            .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("MEMORY.md"))
            .or_else(|| {
                files.iter().find(|path| {
                    path.extension().and_then(|extension| extension.to_str()) == Some("md")
                        || include_unknown
                })
            })
            .cloned()
    });

    for path in files {
        let is_markdown = path.extension().and_then(|extension| extension.to_str()) == Some("md");
        if !is_markdown && !include_unknown {
            continue;
        }
        let is_index = path.file_name().and_then(|name| name.to_str()) == Some("MEMORY.md");
        let is_human_detail_anchor = human_detail_anchor.as_ref() == Some(&path);
        let (role, kind, load_state, detail, human_detail) = if is_index {
            let load_detail = "startup index: first 200 lines or 25 KiB";
            (
                SourceRole::RetainedMemory,
                SourceKind::MemoryIndex,
                memory_load_state(memory_state, false),
                Some(match &association_detail {
                    Some(association) => {
                        format!("{load_detail}; {association}")
                    }
                    None => load_detail.to_string(),
                }),
                HumanDetail::Replacement(match (is_human_detail_anchor, &human_directory_detail) {
                    (true, Some(directory_detail)) => {
                        format!("{load_detail}; {directory_detail}")
                    }
                    _ => load_detail.to_string(),
                }),
            )
        } else if is_markdown {
            (
                SourceRole::RetainedMemory,
                SourceKind::MemoryTopic,
                memory_load_state(memory_state, true),
                association_detail.clone(),
                match (is_human_detail_anchor, &human_directory_detail) {
                    (true, Some(detail)) => HumanDetail::Replacement(detail.clone()),
                    _ => HumanDetail::Hidden,
                },
            )
        } else {
            (
                SourceRole::Unknown,
                SourceKind::Unknown,
                LoadState::Unknown,
                association_detail.clone(),
                match (is_human_detail_anchor, &human_directory_detail) {
                    (true, Some(detail)) => HumanDetail::Replacement(detail.clone()),
                    _ => HumanDetail::Hidden,
                },
            )
        };
        inventory.push_path(
            path,
            SourceSpec {
                role,
                kind,
                scope: Scope::Repository,
                load_state,
                association,
                detail,
                human_detail,
            },
        );
    }
}

fn memory_load_state(memory_state: MemoryState, on_demand: bool) -> LoadState {
    match memory_state {
        MemoryState::Enabled if on_demand => LoadState::OnDemand,
        MemoryState::Enabled => LoadState::Loaded,
        MemoryState::Disabled => LoadState::Disabled,
        MemoryState::Unknown => LoadState::Unknown,
    }
}

fn expand_memory_dir(value: &str) -> Option<PathBuf> {
    if value == "~" {
        return Some(paths::home_dir());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Some(paths::home_dir().join(rest));
    }
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

fn ancestor_chain(target: &Path) -> Vec<PathBuf> {
    let mut paths = target
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    paths.reverse();
    paths
}

fn instruction_scope(directory: &Path, repository_root: &Path) -> Scope {
    if directory == repository_root {
        Scope::Repository
    } else {
        Scope::Path
    }
}

fn managed_claude_md_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![PathBuf::from(
            "/Library/Application Support/ClaudeCode/CLAUDE.md",
        )]
    }
    #[cfg(target_os = "linux")]
    {
        vec![PathBuf::from("/etc/claude-code/CLAUDE.md")]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_ignore_code() {
        let raw = "@README.md\n`@inline.md`\n```md\n@fenced.md\n```\nSee @docs/rules.md, now.\n";
        assert_eq!(imports_from_text(raw), vec!["README.md", "docs/rules.md"]);
    }

    #[test]
    fn path_scoped_rule_reads_frontmatter_only() {
        let dir = tempfile::tempdir().unwrap();
        let scoped = dir.path().join("scoped.md");
        let plain = dir.path().join("plain.md");
        fs::write(&scoped, "---\npaths:\n  - src/**\n---\nrule\n").unwrap();
        fs::write(&plain, "# paths:\nnot frontmatter\n").unwrap();
        assert!(rule_is_path_scoped(&scoped));
        assert!(!rule_is_path_scoped(&plain));
    }

    #[test]
    fn repository_identity_accepts_multiple_cwds_in_one_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "--quiet"])
            .output()
            .unwrap();
        let nested = dir.path().join("crates/api");
        fs::create_dir_all(&nested).unwrap();
        let root = dir.path().canonicalize().unwrap();
        let identity = git::repository_identity(&root);
        let mut identity_cache = BTreeMap::new();
        assert_eq!(
            classify_cwds(
                &root,
                identity.as_deref(),
                &[root.clone(), nested.clone(), nested],
                &mut identity_cache,
            ),
            Association::Target
        );
        assert_eq!(identity_cache.len(), 1);
    }

    #[test]
    fn rule_inventory_stops_at_the_entry_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "a\n").unwrap();
        fs::write(dir.path().join("b.md"), "b\n").unwrap();
        let mut inventory = ProviderInventory::new(Provider::Claude, true, MemoryState::Enabled);
        let mut imported = BTreeSet::new();
        collect_rules_with_limit(
            &mut inventory,
            dir.path(),
            InstructionContext {
                scope: Scope::Repository,
                association: Association::Target,
                exclusions: None,
                trust_root: Some(dir.path()),
            },
            &mut imported,
            2,
        );

        assert_eq!(inventory.sources.len(), 1);
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.code == "claude-rule-entry-limit")
        );
    }

    #[test]
    fn unavailable_cwds_have_unknown_repository_association() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "--quiet"])
            .output()
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let identity = git::repository_identity(&root);
        let mut identity_cache = BTreeMap::new();

        assert_eq!(
            classify_cwds(
                &root,
                identity.as_deref(),
                &[root.join("moved-or-unmounted")],
                &mut identity_cache,
            ),
            Association::Unknown
        );
    }

    #[test]
    fn human_cwd_evidence_only_hides_an_exact_single_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nested = root.join("crates/api");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            human_cwd_detail(
                &root,
                Association::Target,
                std::slice::from_ref(&root),
                false
            ),
            None
        );
        assert!(
            human_cwd_detail(
                &root,
                Association::Target,
                std::slice::from_ref(&nested),
                false,
            )
            .unwrap()
            .contains(&nested.display().to_string())
        );
        assert!(
            human_cwd_detail(
                &root,
                Association::Other,
                std::slice::from_ref(&root),
                false
            )
            .is_some()
        );
        assert_eq!(
            human_cwd_detail(&root, Association::Unknown, &[], false).as_deref(),
            Some("association unknown: no transcript cwd evidence")
        );
        assert!(
            human_cwd_detail(
                &root,
                Association::Unknown,
                std::slice::from_ref(&root),
                false,
            )
            .unwrap()
            .starts_with("evidence cwd:")
        );
        assert!(
            human_cwd_detail(&root, Association::Target, &[root.clone(), nested], false)
                .unwrap()
                .starts_with("2 associated cwds")
        );
        assert!(
            human_cwd_detail(
                &root,
                Association::Target,
                std::slice::from_ref(&root),
                true,
            )
            .unwrap()
            .contains("sampled first 16 transcripts")
        );
    }
}
