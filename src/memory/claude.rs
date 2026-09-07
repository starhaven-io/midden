use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

use super::{
    Adapter, Association, DiscoveryRequest, HumanDetail, LoadState, MemoryState, Provider,
    ProviderInventory, Scope, SourceKind, SourceRole, SourceSpec, Warning,
};
use crate::claude_json::{self, ClaudeJson};
use crate::git;
use crate::paths::{self, Env, ProjectPaths};
use crate::safe_io;
use crate::show;
use crate::transcripts;

const MAX_IMPORT_DEPTH: usize = 4;
const MEMORY_INDEX_MAX_LINES: usize = 200;
const MEMORY_INDEX_MAX_BYTES: usize = 25 * 1024;
const MAX_MEMORY_FILES: usize = 1024;
const MAX_MEMORY_ENTRIES: usize = 8192;
const MAX_PROJECT_DIRS: usize = 4096;
const MAX_PROJECT_ENTRIES: usize = 16384;
const MAX_PROJECT_TRANSCRIPTS: usize = 16;
const MAX_RULE_ENTRIES: usize = 4096;
const MAX_IMPORTED_SOURCES: usize = 1024;

pub(super) struct ClaudeAdapter<'a> {
    env: &'a Env,
}

#[derive(Clone, Copy)]
struct InstructionContext<'a> {
    scope: Scope,
    association: Association,
    exclusions: Option<&'a GlobSet>,
    trust_root: Option<&'a Path>,
    external_import_approval: ExternalImportApproval,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExternalImportApproval {
    Approved,
    Denied,
    Unknown,
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
        let (settings, settings_errors) = show::settings_sources(self.env, &project);
        let malformed_settings = !settings_errors.is_empty();
        let memory_enabled = show::effective_setting(&settings, "autoMemoryEnabled");
        let memory_enabled_scope = effective_setting_scope(&settings, "autoMemoryEnabled");
        let memory_directory_scope = effective_setting_scope(&settings, "autoMemoryDirectory");
        let trust_conditional_enabled = memory_enabled_scope
            .is_some_and(|scope| matches!(scope, show::Scope::Project | show::Scope::Local));
        let trust_conditional_directory = memory_directory_scope
            .is_some_and(|scope| matches!(scope, show::Scope::Project | show::Scope::Local));
        let invalid_memory_enabled = memory_enabled
            .as_ref()
            .is_some_and(|value| !value.is_boolean());
        let observed_memory_state =
            if malformed_settings || invalid_memory_enabled || trust_conditional_enabled {
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
        // A filesystem inventory cannot observe --settings, the environment
        // disable, an in-session /memory toggle, or higher managed tiers.
        let memory_state = MemoryState::Unknown;
        let configured = self.env.claude_home.is_dir()
            || settings
                .iter()
                .any(|(_, _, value)| has_relevant_setting(value));
        let mut inventory = ProviderInventory::new(Provider::Claude, configured, memory_state);

        if malformed_settings {
            for error in settings_errors {
                inventory.warnings.push(Warning::at(
                    "invalid-claude-settings",
                    format!(
                        "could not read or parse Claude settings; auto-memory state is unknown: {}",
                        error.message
                    ),
                    error.path,
                ));
            }
        }
        if invalid_memory_enabled {
            inventory.warnings.push(Warning::new(
                "invalid-auto-memory-enabled",
                "autoMemoryEnabled must be a boolean; auto-memory state is unknown",
            ));
        }
        if trust_conditional_enabled || trust_conditional_directory {
            inventory.warnings.push(Warning::new(
                "claude-project-trust-unresolved",
                "project or local settings control auto-memory state or location, but midden cannot observe whether Claude trusts this workspace",
            ));
        }
        inventory.warnings.push(Warning::new(
            "claude-session-memory-state-unresolved",
            format!(
                "visible filesystem layers suggest auto-memory is {}, but --settings, the environment disable, in-session toggles, and server/endpoint-managed settings are not observable; effective state and location remain unknown",
                observed_memory_state.label()
            ),
        ));
        collect_operational_sources(&mut inventory, &settings);
        let exclusions = build_exclusions(&mut inventory, &settings);
        let project_import_approval =
            external_import_approval(&mut inventory, self.env, &repository_root);

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
                    external_import_approval: ExternalImportApproval::Approved,
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
                external_import_approval: ExternalImportApproval::Approved,
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
                external_import_approval: ExternalImportApproval::Approved,
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
                    external_import_approval: project_import_approval,
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
                        external_import_approval: project_import_approval,
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
                    external_import_approval: project_import_approval,
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
                external_import_approval: project_import_approval,
            },
            &mut imported,
        );

        let custom_memory_dir = effective_auto_memory_directory(&settings, true);
        let custom_path = custom_memory_dir.as_ref().and_then(|(value, scope)| {
            let path = configured_memory_path(&mut inventory, value)?;
            let detail = if matches!(scope, show::Scope::Project | show::Scope::Local) {
                "configured by the highest visible autoMemoryDirectory layer; effective only when Claude trusts the workspace; a session override may select a different location"
            } else {
                "configured by an observable autoMemoryDirectory layer; a session override may select a different location"
            };
            collect_configured_memory_dir(
                &mut inventory,
                &path,
                *scope,
                memory_state,
                request.include_unassociated,
                detail,
            );
            Some(path)
        });
        if custom_memory_dir.is_none() {
            collect_default_memory_dirs(
                &mut inventory,
                self.env,
                request,
                &repository_root,
                memory_state,
            );
        }
        if trust_conditional_directory {
            match effective_auto_memory_directory(&settings, false) {
                Some((value, scope)) => {
                    if let Some(path) = configured_memory_path(&mut inventory, &value)
                        && custom_path.as_ref() != Some(&path)
                    {
                        collect_configured_memory_dir(
                            &mut inventory,
                            &path,
                            scope,
                            memory_state,
                            request.include_unassociated,
                            "possible fallback when Claude does not trust the workspace; a session override may select a different location",
                        );
                    }
                }
                None => collect_default_memory_dirs(
                    &mut inventory,
                    self.env,
                    request,
                    &repository_root,
                    memory_state,
                ),
            }
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
        let trust_conditional = matches!(scope, show::Scope::Project | show::Scope::Local);
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
                if trust_conditional {
                    LoadState::Unknown
                } else {
                    LoadState::Loaded
                },
                association,
            )
            .with_detail(if trust_conditional {
                "configuration layer is effective only when Claude trusts the workspace"
            } else {
                "configuration layer"
            }),
        );
    }
}

fn has_relevant_setting(value: &Value) -> bool {
    value.get("autoMemoryEnabled").is_some()
        || value.get("autoMemoryDirectory").is_some()
        || value.get("claudeMdExcludes").is_some()
}

fn effective_auto_memory_directory(
    settings: &[(show::Scope, PathBuf, Value)],
    include_trust_conditional: bool,
) -> Option<(Value, show::Scope)> {
    settings
        .iter()
        .filter(|(scope, _, _)| {
            include_trust_conditional || matches!(scope, show::Scope::Managed | show::Scope::User)
        })
        .filter_map(|(scope, _, value)| {
            value
                .get("autoMemoryDirectory")
                .cloned()
                .map(|value| (value, *scope))
        })
        .max_by_key(|(_, scope)| *scope)
}

fn configured_memory_path(inventory: &mut ProviderInventory, value: &Value) -> Option<PathBuf> {
    let Some(value) = value.as_str() else {
        inventory.warnings.push(Warning::new(
            "invalid-auto-memory-directory",
            "autoMemoryDirectory must be a string; the memory location is unknown",
        ));
        return None;
    };
    let Some(path) = expand_memory_dir(value) else {
        inventory.warnings.push(Warning::new(
            "invalid-auto-memory-directory",
            format!("autoMemoryDirectory must be absolute or start with ~/: {value:?}"),
        ));
        return None;
    };
    Some(path)
}

fn collect_configured_memory_dir(
    inventory: &mut ProviderInventory,
    path: &Path,
    setting_scope: show::Scope,
    memory_state: MemoryState,
    include_unknown: bool,
    detail: &str,
) {
    let (scope, association) = match setting_scope {
        show::Scope::Managed => (Scope::Managed, Association::Global),
        show::Scope::User => (Scope::Global, Association::Global),
        show::Scope::Project | show::Scope::Local => (Scope::Repository, Association::Target),
    };
    collect_memory_dir(
        inventory,
        path,
        MemoryDirContext {
            scope,
            association,
            memory_state,
            include_unknown,
            association_detail: Some(detail.to_string()),
            human_directory_detail: Some(detail.to_string()),
        },
    );
}

fn effective_setting_scope(
    settings: &[(show::Scope, PathBuf, Value)],
    key: &str,
) -> Option<show::Scope> {
    settings
        .iter()
        .filter_map(|(scope, _, value)| value.get(key).map(|_| *scope))
        .max()
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

fn external_import_approval(
    inventory: &mut ProviderInventory,
    env: &Env,
    repository_root: &Path,
) -> ExternalImportApproval {
    if !env.claude_json.exists() {
        return ExternalImportApproval::Unknown;
    }
    let config = match ClaudeJson::load(&env.claude_json) {
        Ok(config) => config,
        Err(error) => {
            inventory.warnings.push(Warning::at(
                "claude-external-import-approval-unavailable",
                format!("could not resolve external import approval: {error}"),
                env.claude_json.clone(),
            ));
            return ExternalImportApproval::Unknown;
        }
    };
    match claude_json::project_entry(&config.data, repository_root)
        .and_then(|entry| entry.get("hasClaudeMdExternalIncludesApproved"))
        .and_then(Value::as_bool)
    {
        Some(true) => ExternalImportApproval::Approved,
        Some(false) => ExternalImportApproval::Denied,
        None => ExternalImportApproval::Unknown,
    }
}

enum InstructionRead {
    Loaded(String),
    TooLarge(String),
    Unknown(String),
}

fn read_instruction_source(path: &Path) -> InstructionRead {
    match safe_io::read_to_string(path, safe_io::MAX_INSTRUCTION_BYTES) {
        Ok(raw) => InstructionRead::Loaded(raw),
        Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
            InstructionRead::TooLarge(format!(
                "midden did not scan the contents because they exceed the {} byte inspection limit",
                safe_io::MAX_INSTRUCTION_BYTES
            ))
        }
        Err(error) => {
            InstructionRead::Unknown(format!("could not read instruction source: {error}"))
        }
    }
}

fn push_instruction_with_imports(
    inventory: &mut ProviderInventory,
    path: PathBuf,
    context: InstructionContext<'_>,
    imported: &mut BTreeSet<PathBuf>,
) {
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return;
    };
    if !metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
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
    let read = read_instruction_source(&path);
    let (load_state, detail, raw) = match read {
        InstructionRead::Loaded(raw) => (LoadState::Loaded, None, Some(raw)),
        InstructionRead::TooLarge(detail) => (
            LoadState::Disabled,
            Some(format!(
                "skipped by Claude because the file exceeds the {} byte CLAUDE.md limit; {detail}",
                safe_io::MAX_INSTRUCTION_BYTES
            )),
            None,
        ),
        InstructionRead::Unknown(detail) => {
            inventory.warnings.push(Warning::at(
                "source-inaccessible",
                detail.clone(),
                path.clone(),
            ));
            (LoadState::Unknown, Some(detail), None)
        }
    };
    if !promote_imported_instruction(inventory, &path, context, load_state, detail.clone()) {
        let mut spec = SourceSpec::new(
            SourceRole::Authority,
            SourceKind::Instruction,
            context.scope,
            load_state,
            context.association,
        );
        spec.detail = detail;
        inventory.push_path(path.clone(), spec);
    }
    if let Some(raw) = raw {
        collect_imports_from_raw(inventory, &path, context, 0, imported, &raw);
    }
}

fn promote_imported_instruction(
    inventory: &mut ProviderInventory,
    path: &Path,
    context: InstructionContext<'_>,
    load_state: LoadState,
    detail: Option<String>,
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
    source.load_state = load_state;
    source.association = context.association;
    source.detail = match (detail, source.detail.take()) {
        (Some(detail), Some(import_detail)) => Some(format!("{detail}; also {import_detail}")),
        (Some(detail), None) => Some(detail),
        (None, Some(import_detail)) => Some(format!("also {import_detail}")),
        (None, None) => None,
    };
    true
}

fn collect_imports_from_raw(
    inventory: &mut ProviderInventory,
    source: &Path,
    context: InstructionContext<'_>,
    depth: usize,
    imported: &mut BTreeSet<PathBuf>,
    raw: &str,
) {
    if import_limit_reached(inventory, source, imported.len()) {
        return;
    }
    imported.insert(
        source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf()),
    );
    let imports = imports_from_text(raw);
    if depth >= MAX_IMPORT_DEPTH && !imports.is_empty() {
        inventory.warnings.push(Warning::at(
            "claude-import-depth-exceeded",
            format!("imports beyond {MAX_IMPORT_DEPTH} hops were not followed"),
            source.to_path_buf(),
        ));
        return;
    }

    for value in imports {
        if import_limit_reached(inventory, source, imported.len()) {
            break;
        }
        let path = lexical_normalize(&resolve_import(source, &value));
        let may_be_external = import_may_be_external(context, &path);
        let approval = if may_be_external {
            context.external_import_approval
        } else {
            ExternalImportApproval::Approved
        };

        // A denied or unresolved external import is an authority boundary, not
        // permission to probe the target. Preserve the lexical source without
        // canonicalizing, statting, or opening it.
        if approval != ExternalImportApproval::Approved {
            if !imported.insert(path.clone()) {
                continue;
            }
            let detail = match approval {
                ExternalImportApproval::Denied => format!(
                    "imported by {}; external import was not approved",
                    source.display()
                ),
                ExternalImportApproval::Unknown => format!(
                    "imported by {}; external import approval is unresolved",
                    source.display()
                ),
                ExternalImportApproval::Approved => unreachable!(),
            };
            inventory.push_uninspected(
                path,
                SourceSpec::new(
                    SourceRole::Authority,
                    SourceKind::ImportedInstruction,
                    context.scope,
                    match approval {
                        ExternalImportApproval::Denied => LoadState::Disabled,
                        ExternalImportApproval::Unknown => LoadState::Unknown,
                        ExternalImportApproval::Approved => unreachable!(),
                    },
                    context.association,
                )
                .with_detail(detail),
            );
            continue;
        }

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
        let raw = safe_io::read_to_string(&path, safe_io::MAX_INSTRUCTION_BYTES);
        let load_state = if raw.is_ok() {
            LoadState::Loaded
        } else {
            LoadState::Unknown
        };
        let detail = format!("imported by {}", source.display());
        let spec = SourceSpec::new(
            SourceRole::Authority,
            SourceKind::ImportedInstruction,
            context.scope,
            load_state,
            context.association,
        )
        .with_detail(detail);
        inventory.push_path(identity, spec);
        match raw {
            Ok(raw) => {
                collect_imports_from_raw(inventory, &path, context, depth + 1, imported, &raw);
            }
            Err(error) => inventory.warnings.push(Warning::at(
                "source-inaccessible",
                format!("could not read imported instruction: {error}"),
                path,
            )),
        }
    }
}

fn import_limit_reached(
    inventory: &mut ProviderInventory,
    source: &Path,
    imported_count: usize,
) -> bool {
    if imported_count < MAX_IMPORTED_SOURCES {
        return false;
    }
    if !inventory
        .warnings
        .iter()
        .any(|warning| warning.code == "claude-import-source-limit")
    {
        inventory.warnings.push(Warning::at(
            "claude-import-source-limit",
            format!("only the first {MAX_IMPORTED_SOURCES} instruction sources were inspected"),
            source.to_path_buf(),
        ));
    }
    true
}

fn is_excluded(context: InstructionContext<'_>, path: &Path) -> bool {
    context.exclusions.is_some_and(|exclusions| {
        exclusions.is_match(path)
            || path
                .canonicalize()
                .is_ok_and(|identity| exclusions.is_match(identity))
    })
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn import_may_be_external(context: InstructionContext<'_>, path: &Path) -> bool {
    let Some(root) = context.trust_root else {
        return false;
    };
    let root = lexical_normalize(root);
    let path = lexical_normalize(path);
    if !path.starts_with(&root) {
        return true;
    }

    let mut current = root;
    let Ok(relative) = path.strip_prefix(&current) else {
        return true;
    };
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        }
    }
    false
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
    let walker = WalkDir::new(directory)
        .follow_links(true)
        .max_depth(16)
        .into_iter();
    let mut entries = Vec::new();
    for (inspected, entry) in walker.enumerate() {
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
        let read = (!excluded).then(|| read_instruction_source(path));
        let (load_state, detail, raw) = if excluded {
            (
                LoadState::Disabled,
                Some("excluded by claudeMdExcludes".to_string()),
                None,
            )
        } else {
            match read.expect("non-excluded rule was read") {
                InstructionRead::Loaded(raw) => {
                    let state = if rule_is_path_scoped(&raw) {
                        LoadState::OnDemand
                    } else {
                        LoadState::Loaded
                    };
                    (state, None, Some(raw))
                }
                InstructionRead::TooLarge(detail) => {
                    inventory.warnings.push(Warning::at(
                        "claude-rule-unscanned",
                        detail.clone(),
                        path.to_path_buf(),
                    ));
                    (LoadState::Unknown, Some(detail), None)
                }
                InstructionRead::Unknown(detail) => {
                    inventory.warnings.push(Warning::at(
                        "claude-rule-inaccessible",
                        detail.clone(),
                        path.to_path_buf(),
                    ));
                    (LoadState::Unknown, Some(detail), None)
                }
            }
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
            match detail {
                Some(detail) => spec.with_detail(detail),
                None => spec,
            },
        );
        if let Some(raw) = raw {
            collect_imports_from_raw(inventory, path, context, 0, imported, &raw);
        }
    }
}

fn rule_is_path_scoped(raw: &str) -> bool {
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
    let mut directories = Vec::new();
    let mut entry_limit_reached = false;
    for (index, entry) in entries.flatten().enumerate() {
        if index == MAX_PROJECT_ENTRIES {
            entry_limit_reached = true;
            break;
        }
        let path = entry.path();
        if path.is_dir() && path.join("memory").is_dir() {
            directories.push(path);
        }
    }
    directories.sort();
    if entry_limit_reached || directories.len() > MAX_PROJECT_DIRS {
        directories.truncate(MAX_PROJECT_DIRS);
        inventory.warnings.push(Warning::at(
            "claude-project-directory-limit",
            format!(
                "at most {MAX_PROJECT_DIRS} project memory directories from the first {MAX_PROJECT_ENTRIES} entries were inspected"
            ),
            projects,
        ));
    }

    for directory in directories {
        let (cwds, incomplete) =
            match transcripts::project_cwds(&directory, MAX_PROJECT_TRANSCRIPTS) {
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
        let complete_association = classify_cwds(
            repository_root,
            target_identity.as_deref(),
            &cwds,
            &mut identity_cache,
        );
        let association = if incomplete {
            Association::Unknown
        } else {
            complete_association
        };
        if association != Association::Target && !request.include_unassociated {
            let (code, message) = if incomplete {
                (
                    "claude-memory-association-incomplete",
                    "transcript cwd evidence is incomplete (missing metadata or scan limit), so target association is unknown; rerun with --all".to_string(),
                )
            } else {
                (
                    "claude-memory-unassociated",
                    "memory is present but transcript cwd evidence cannot associate it with the target; rerun with --all".to_string(),
                )
            };
            inventory
                .warnings
                .push(Warning::at(code, message, directory.join("memory")));
            continue;
        }
        let detail = cwd_detail(&cwds, incomplete);
        let human_detail = human_cwd_detail(repository_root, association, &cwds, incomplete);
        collect_memory_dir(
            inventory,
            &directory.join("memory"),
            MemoryDirContext {
                scope: Scope::Repository,
                association,
                memory_state,
                include_unknown: request.include_unassociated,
                association_detail: detail,
                human_directory_detail: human_detail,
            },
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

fn cwd_detail(cwds: &[PathBuf], incomplete: bool) -> Option<String> {
    labeled_cwd_detail(cwds, incomplete, "associated")
}

fn labeled_cwd_detail(cwds: &[PathBuf], incomplete: bool, label: &str) -> Option<String> {
    let first = cwds.first()?;
    let mut detail = if cwds.len() == 1 {
        format!("{label} cwd: {}", first.display())
    } else {
        format!("{} {label} cwds; first: {}", cwds.len(), first.display())
    };
    if incomplete {
        detail.push_str("; incomplete transcript evidence");
    }
    Some(detail)
}

fn human_cwd_detail(
    target_root: &Path,
    association: Association,
    cwds: &[PathBuf],
    incomplete: bool,
) -> Option<String> {
    // "associated" would overclaim for a dir whose cwd evidence could not be
    // matched to the target; label the unresolved case as evidence instead.
    let label = if association == Association::Unknown {
        "evidence"
    } else {
        "associated"
    };
    let detail = labeled_cwd_detail(cwds, incomplete, label).or_else(|| {
        (association == Association::Unknown)
            .then(|| "association unknown: no transcript cwd evidence".to_string())
    })?;
    let repeats_target = association == Association::Target
        && cwds.len() == 1
        && !incomplete
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

struct MemoryDirContext {
    scope: Scope,
    association: Association,
    memory_state: MemoryState,
    include_unknown: bool,
    association_detail: Option<String>,
    human_directory_detail: Option<String>,
}

fn collect_memory_dir(
    inventory: &mut ProviderInventory,
    directory: &Path,
    context: MemoryDirContext,
) {
    if !directory.is_dir() {
        return;
    }
    let walker = WalkDir::new(directory)
        .follow_links(false)
        .max_depth(8)
        .sort_by_file_name();
    let mut files = Vec::new();
    for (inspected, entry) in walker.into_iter().enumerate() {
        if inspected == MAX_MEMORY_ENTRIES {
            inventory.warnings.push(Warning::at(
                "claude-memory-entry-limit",
                format!("only the first {MAX_MEMORY_ENTRIES} memory entries were inspected"),
                directory.to_path_buf(),
            ));
            break;
        }
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

    let human_detail_anchor = context.human_directory_detail.as_ref().and_then(|_| {
        files
            .iter()
            .find(|path| **path == directory.join("MEMORY.md"))
            .or_else(|| {
                files.iter().find(|path| {
                    path.extension().and_then(|extension| extension.to_str()) == Some("md")
                        || context.include_unknown
                })
            })
            .cloned()
    });

    for path in files {
        let is_markdown = path.extension().and_then(|extension| extension.to_str()) == Some("md");
        if !is_markdown && !context.include_unknown {
            continue;
        }
        let is_index = path == directory.join("MEMORY.md");
        let is_human_detail_anchor = human_detail_anchor.as_ref() == Some(&path);
        let (role, kind, load_state, detail, human_detail) = if is_index {
            let (enabled_state, load_detail) = memory_index_state(&path, inventory);
            (
                SourceRole::RetainedMemory,
                SourceKind::MemoryIndex,
                match context.memory_state {
                    MemoryState::Enabled => enabled_state,
                    MemoryState::Disabled => LoadState::Disabled,
                    MemoryState::Unknown => LoadState::Unknown,
                },
                Some(match &context.association_detail {
                    Some(association) => {
                        format!("{load_detail}; {association}")
                    }
                    None => load_detail.clone(),
                }),
                HumanDetail::Replacement(
                    match (is_human_detail_anchor, &context.human_directory_detail) {
                        (true, Some(directory_detail)) => {
                            format!("{load_detail}; {directory_detail}")
                        }
                        _ => load_detail,
                    },
                ),
            )
        } else if is_markdown {
            (
                SourceRole::RetainedMemory,
                SourceKind::MemoryTopic,
                memory_load_state(context.memory_state, true),
                context.association_detail.clone(),
                match (is_human_detail_anchor, &context.human_directory_detail) {
                    (true, Some(detail)) => HumanDetail::Replacement(detail.clone()),
                    _ => HumanDetail::Hidden,
                },
            )
        } else {
            (
                SourceRole::Unknown,
                SourceKind::Unknown,
                LoadState::Unknown,
                context.association_detail.clone(),
                match (is_human_detail_anchor, &context.human_directory_detail) {
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
                scope: context.scope,
                load_state,
                association: context.association,
                detail,
                human_detail,
            },
        );
    }
}

fn memory_index_state(path: &Path, inventory: &mut ProviderInventory) -> (LoadState, String) {
    match safe_io::read_to_string(path, MEMORY_INDEX_MAX_BYTES) {
        Ok(raw) if raw.lines().count() <= MEMORY_INDEX_MAX_LINES => (
            LoadState::Loaded,
            format!(
                "startup index loaded in full (within {MEMORY_INDEX_MAX_LINES} lines and {} KiB)",
                MEMORY_INDEX_MAX_BYTES / 1024
            ),
        ),
        Ok(_) => (
            LoadState::Truncated,
            format!(
                "startup index: first {MEMORY_INDEX_MAX_LINES} lines or {} KiB",
                MEMORY_INDEX_MAX_BYTES / 1024
            ),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => (
            LoadState::Truncated,
            format!(
                "startup index: first {MEMORY_INDEX_MAX_LINES} lines or {} KiB",
                MEMORY_INDEX_MAX_BYTES / 1024
            ),
        ),
        Err(error) => {
            inventory.warnings.push(Warning::at(
                "claude-memory-source-inaccessible",
                error.to_string(),
                path.to_path_buf(),
            ));
            (
                LoadState::Unknown,
                "startup index load state could not be determined".to_string(),
            )
        }
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
    fn auto_memory_directory_preserves_managed_precedence_and_trusted_fallback() {
        let settings = vec![
            (
                show::Scope::User,
                PathBuf::from("user.json"),
                serde_json::json!({ "autoMemoryDirectory": "/user" }),
            ),
            (
                show::Scope::Project,
                PathBuf::from("project.json"),
                serde_json::json!({ "autoMemoryDirectory": "/project" }),
            ),
            (
                show::Scope::Local,
                PathBuf::from("local.json"),
                serde_json::json!({ "autoMemoryDirectory": "/local" }),
            ),
        ];

        assert_eq!(
            effective_auto_memory_directory(&settings, true),
            Some((serde_json::json!("/local"), show::Scope::Local))
        );
        assert_eq!(
            effective_auto_memory_directory(&settings, false),
            Some((serde_json::json!("/user"), show::Scope::User))
        );

        let mut with_managed = settings;
        with_managed.push((
            show::Scope::Managed,
            PathBuf::from("managed.json"),
            serde_json::json!({ "autoMemoryDirectory": "/managed" }),
        ));
        let managed = Some((serde_json::json!("/managed"), show::Scope::Managed));
        assert_eq!(
            effective_auto_memory_directory(&with_managed, true),
            managed.clone()
        );
        assert_eq!(
            effective_auto_memory_directory(&with_managed, false),
            managed
        );
    }

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
        assert!(rule_is_path_scoped(&fs::read_to_string(scoped).unwrap()));
        assert!(!rule_is_path_scoped(&fs::read_to_string(plain).unwrap()));
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
                external_import_approval: ExternalImportApproval::Unknown,
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
            .contains("incomplete transcript evidence")
        );
    }
}
