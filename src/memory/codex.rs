use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;

use super::{
    Adapter, Association, DiscoveryRequest, LoadState, MemoryState, Provider, ProviderInventory,
    Scope, SourceKind, SourceRole, SourceSpec, Warning,
};
use crate::paths::Env;
use crate::safe_io;

const MAX_PROFILE_FILES: usize = 256;

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Config {
    project_doc_fallback_filenames: Vec<String>,
    project_doc_max_bytes: Option<usize>,
    project_root_markers: Option<Vec<String>>,
    features: Features,
    memories: Memories,
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Features {
    memories: Option<bool>,
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Memories {
    use_memories: Option<bool>,
}

pub(super) struct CodexAdapter<'a> {
    env: &'a Env,
}

impl<'a> CodexAdapter<'a> {
    pub(super) fn new(env: &'a Env) -> Self {
        Self { env }
    }
}

impl Adapter for CodexAdapter<'_> {
    fn discover(&self, request: &DiscoveryRequest<'_>) -> ProviderInventory {
        let (repository_roots, resolved) =
            read_all_config_layers(self.env, request.target, &SystemConfigPaths::default());
        // Filesystem discovery cannot observe one-off CLI overrides, the
        // selected profile, cloud requirements, or macOS MDM. Report the
        // provider state conservatively even when the visible disk layers all
        // agree.
        let memory_state = MemoryState::Unknown;
        let configuration_unresolved = true;
        let configured = self.env.codex_home.is_dir()
            || !resolved.sources.is_empty()
            || self.env.codex_memories_dir().is_dir();
        let mut inventory = ProviderInventory::new(Provider::Codex, configured, memory_state);

        let mut config_paths = std::collections::BTreeSet::new();
        for source in &resolved.sources {
            if !config_paths.insert(source.path.clone()) {
                continue;
            }
            let spec = SourceSpec::new(
                SourceRole::OperationalState,
                SourceKind::Configuration,
                source.scope,
                if source.trust_conditional || source.profile_conditional {
                    LoadState::Unknown
                } else {
                    LoadState::Loaded
                },
                source.association,
            )
            .with_detail(if source.uninspected {
                "project config resolves outside the repository and was not inspected"
            } else if source.profile_conditional {
                "profile configuration; the active --profile selection is unobservable"
            } else if source.trust_conditional {
                "project config is effective only when Codex trusts the project"
            } else {
                source.detail
            });
            if source.uninspected {
                inventory.push_uninspected(source.path.clone(), spec);
            } else {
                inventory.push_path(source.path.clone(), spec);
            }
        }
        for (path, error) in resolved.errors {
            inventory.warnings.push(Warning::at(
                "invalid-codex-config",
                format!(
                    "could not read or parse Codex config; effective configuration is unknown: {error}"
                ),
                path,
            ));
        }
        if resolved.has_project_layers {
            inventory.warnings.push(Warning::new(
                "codex-project-trust-unresolved",
                "project config layers were resolved, but midden cannot observe whether Codex trusts this project",
            ));
        }
        if resolved.has_profile_files {
            inventory.warnings.push(Warning::new(
                "codex-profile-unresolved",
                "profile files are present, but midden cannot observe which --profile Codex selected for a session",
            ));
        }
        if resolved.has_legacy_inline_profiles {
            inventory.warnings.push(Warning::new(
                "obsolete-inline-codex-profile",
                "top-level profile/profiles keys are obsolete and were not applied; use a named <profile>.config.toml file with --profile",
            ));
        }
        inventory.warnings.push(Warning::new(
            "codex-session-configuration-unresolved",
            "command-line overrides, the active profile, cloud requirements, and macOS MDM are not observable from a filesystem inventory; effective memory and instruction states remain unknown",
        ));

        let mut fallback_sets = Vec::new();
        for config in &resolved.possible_configs {
            let names = valid_fallback_names(config, &mut inventory);
            if !fallback_sets.contains(&names) {
                fallback_sets.push(names);
            }
        }
        if fallback_sets.is_empty() {
            fallback_sets.push(Vec::new());
        }
        let mut instructions = std::collections::BTreeMap::new();
        if let Some(global) = first_nonempty(&[
            self.env.codex_home.join("AGENTS.override.md"),
            self.env.codex_home.join("AGENTS.md"),
        ]) {
            instructions.insert(global, (Scope::Global, None));
        }

        for repository_root in &repository_roots {
            for directory in path_chain(repository_root, request.target) {
                for fallback_names in &fallback_sets {
                    let mut candidates = vec![
                        directory.join("AGENTS.override.md"),
                        directory.join("AGENTS.md"),
                    ];
                    candidates.extend(fallback_names.iter().map(|name| directory.join(name)));
                    if let Some(path) = first_nonempty(&candidates) {
                        let scope = if directory == *repository_root {
                            Scope::Repository
                        } else {
                            Scope::Path
                        };
                        instructions
                            .entry(path)
                            .or_insert((scope, Some(repository_root.clone())));
                    }
                }
            }
        }
        let mut remaining = usize::MAX;
        for (path, (scope, trust_root)) in instructions {
            push_instruction(
                &mut inventory,
                path,
                scope,
                trust_root.as_deref(),
                configuration_unresolved,
                &mut remaining,
            );
        }

        collect_memory_sources(&mut inventory, self.env, request, memory_state);
        inventory.configured = inventory.configured || !inventory.sources.is_empty();
        inventory
    }
}

struct ConfigSource {
    path: PathBuf,
    scope: Scope,
    association: Association,
    trust_conditional: bool,
    profile_conditional: bool,
    uninspected: bool,
    detail: &'static str,
}

struct ResolvedConfig {
    #[cfg(test)]
    config: Config,
    possible_configs: Vec<Config>,
    untrusted_configs: Vec<Config>,
    sources: Vec<ConfigSource>,
    errors: Vec<(PathBuf, String)>,
    has_project_layers: bool,
    has_profile_files: bool,
    has_legacy_inline_profiles: bool,
}

struct SystemConfigPaths {
    config: PathBuf,
    managed_config: PathBuf,
    requirements: PathBuf,
}

impl Default for SystemConfigPaths {
    fn default() -> Self {
        Self {
            config: PathBuf::from("/etc/codex/config.toml"),
            managed_config: PathBuf::from("/etc/codex/managed_config.toml"),
            requirements: PathBuf::from("/etc/codex/requirements.toml"),
        }
    }
}

#[cfg(test)]
fn read_config_layers(env: &Env, repository_root: &Path, target: &Path) -> ResolvedConfig {
    read_config_layers_with_paths(env, repository_root, target, &SystemConfigPaths::default())
}

fn read_all_config_layers(
    env: &Env,
    target: &Path,
    system: &SystemConfigPaths,
) -> (Vec<PathBuf>, ResolvedConfig) {
    // Project-root markers come only from layers that Codex can read before it
    // knows the project root. Seed with cwd as the root, then resolve every
    // base/profile alternative without trusting a project layer.
    let seed = read_config_layers_with_paths(env, target, target, system);
    let mut roots = seed
        .untrusted_configs
        .iter()
        .map(|config| project_root(target, config))
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        roots.push(target.to_path_buf());
    }

    let mut resolved = read_config_layers_with_paths(env, &roots[0], target, system);
    for root in roots.iter().skip(1) {
        resolved.merge(read_config_layers_with_paths(env, root, target, system));
    }
    (roots, resolved)
}

fn project_root(target: &Path, config: &Config) -> PathBuf {
    let default = vec![".git".to_string()];
    let markers = config.project_root_markers.as_ref().unwrap_or(&default);
    if markers.is_empty() {
        return target.to_path_buf();
    }
    let markers = markers
        .iter()
        .filter(|marker| {
            let path = Path::new(marker.as_str());
            !marker.is_empty()
                && path.components().count() == 1
                && !matches!(marker.as_str(), "." | "..")
        })
        .collect::<Vec<_>>();
    if markers.is_empty() {
        return target.to_path_buf();
    }
    target
        .ancestors()
        .find(|directory| {
            markers
                .iter()
                .any(|marker| fs::symlink_metadata(directory.join(marker)).is_ok())
        })
        .unwrap_or(target)
        .to_path_buf()
}

impl ResolvedConfig {
    fn merge(&mut self, other: Self) {
        append_unique(&mut self.possible_configs, other.possible_configs);
        append_unique(&mut self.untrusted_configs, other.untrusted_configs);
        for source in other.sources {
            if !self.sources.iter().any(|existing| {
                existing.path == source.path
                    && existing.scope == source.scope
                    && existing.trust_conditional == source.trust_conditional
                    && existing.profile_conditional == source.profile_conditional
            }) {
                self.sources.push(source);
            }
        }
        for error in other.errors {
            if !self.errors.contains(&error) {
                self.errors.push(error);
            }
        }
        self.has_project_layers |= other.has_project_layers;
        self.has_profile_files |= other.has_profile_files;
        self.has_legacy_inline_profiles |= other.has_legacy_inline_profiles;
    }
}

fn append_unique<T: PartialEq>(destination: &mut Vec<T>, values: Vec<T>) {
    for value in values {
        if !destination.contains(&value) {
            destination.push(value);
        }
    }
}

fn read_config_layers_with_paths(
    env: &Env,
    repository_root: &Path,
    target: &Path,
    system: &SystemConfigPaths,
) -> ResolvedConfig {
    let mut effective = TomlValue::Table(Default::default());
    let mut sources = Vec::new();
    let mut errors = Vec::new();

    let base_layers = [
        (
            system.config.clone(),
            Scope::Global,
            "system configuration layer",
        ),
        (
            env.codex_config(),
            Scope::Global,
            "user configuration layer",
        ),
    ];
    for (path, scope, detail) in base_layers {
        let _ = load_config_layer(
            &path,
            scope,
            Association::Global,
            false,
            None,
            detail,
            &mut effective,
            &mut sources,
            &mut errors,
        );
    }

    let pre_project = effective.clone();
    let (has_profile_files, profile_layers) =
        collect_profile_sources(env, &mut sources, &mut errors);

    let mut has_project_layers = false;
    let mut project_layers = Vec::new();
    for directory in path_chain(repository_root, target) {
        let path = directory.join(".codex/config.toml");
        let before = sources.len();
        let scope = if directory == repository_root {
            Scope::Repository
        } else {
            Scope::Path
        };
        if let Some(layer) = load_config_layer(
            &path,
            scope,
            Association::Target,
            true,
            Some(repository_root),
            "project configuration layer",
            &mut effective,
            &mut sources,
            &mut errors,
        ) {
            project_layers.push(layer);
        }
        has_project_layers |= sources.len() != before;
    }

    let managed_layer = load_config_layer(
        &system.managed_config,
        Scope::Managed,
        Association::Global,
        false,
        None,
        "managed defaults layer",
        &mut effective,
        &mut sources,
        &mut errors,
    );
    let requirements_layer = load_requirements_layer(
        &system.requirements,
        &mut effective,
        &mut sources,
        &mut errors,
    );

    let diagnostic_path = sources
        .last()
        .map(|source| source.path.clone())
        .unwrap_or_else(|| env.codex_config());
    let config = decode_config(&effective, &diagnostic_path, &mut errors).unwrap_or_default();

    let mut untrusted_values = Vec::new();
    let mut untrusted = pre_project.clone();
    merge_optional(&mut untrusted, managed_layer.as_ref());
    merge_optional(&mut untrusted, requirements_layer.as_ref());
    untrusted_values.push((diagnostic_path.clone(), untrusted));
    for (profile_path, profile_layer) in &profile_layers {
        let mut alternative = pre_project.clone();
        merge_toml(&mut alternative, profile_layer.clone());
        merge_optional(&mut alternative, managed_layer.as_ref());
        merge_optional(&mut alternative, requirements_layer.as_ref());
        untrusted_values.push((profile_path.clone(), alternative));
    }

    let mut possible_values = vec![(diagnostic_path.clone(), effective.clone())];
    for (profile_path, profile_layer) in &profile_layers {
        let mut alternative = pre_project.clone();
        merge_toml(&mut alternative, profile_layer.clone());
        for project_layer in &project_layers {
            merge_toml(&mut alternative, project_layer.clone());
        }
        merge_optional(&mut alternative, managed_layer.as_ref());
        merge_optional(&mut alternative, requirements_layer.as_ref());
        possible_values.push((profile_path.clone(), alternative));
    }
    possible_values.extend(untrusted_values.iter().cloned());

    let mut possible_configs = Vec::new();
    for (path, value) in possible_values {
        if let Some(config) = decode_config(&value, &path, &mut errors)
            && !possible_configs.contains(&config)
        {
            possible_configs.push(config);
        }
    }
    if !possible_configs.contains(&config) {
        possible_configs.push(config.clone());
    }

    let mut untrusted_configs = Vec::new();
    for (path, value) in untrusted_values {
        if let Some(config) = decode_config(&value, &path, &mut errors)
            && !untrusted_configs.contains(&config)
        {
            untrusted_configs.push(config);
        }
    }
    if untrusted_configs.is_empty() {
        untrusted_configs.push(Config::default());
    }

    let has_legacy_inline_profiles = has_inline_profile(&effective)
        || has_inline_profile(&pre_project)
        || profile_layers
            .iter()
            .any(|(_, layer)| has_inline_profile(layer));
    ResolvedConfig {
        #[cfg(test)]
        config,
        possible_configs,
        untrusted_configs,
        sources,
        errors,
        has_project_layers,
        has_profile_files,
        has_legacy_inline_profiles,
    }
}

#[allow(clippy::too_many_arguments)]
fn load_config_layer(
    path: &Path,
    scope: Scope,
    association: Association,
    trust_conditional: bool,
    trust_root: Option<&Path>,
    detail: &'static str,
    effective: &mut TomlValue,
    sources: &mut Vec<ConfigSource>,
    errors: &mut Vec<(PathBuf, String)>,
) -> Option<TomlValue> {
    if let Some(trust_root) = trust_root
        && let Ok(identity) = path.canonicalize()
        && !identity.starts_with(
            trust_root
                .canonicalize()
                .unwrap_or_else(|_| trust_root.to_path_buf()),
        )
    {
        sources.push(ConfigSource {
            path: identity,
            scope,
            association,
            trust_conditional,
            profile_conditional: false,
            uninspected: true,
            detail,
        });
        return None;
    }
    let raw = match safe_io::read_to_string(path, safe_io::MAX_CONFIG_BYTES) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            errors.push((path.to_path_buf(), error.to_string()));
            return None;
        }
    };
    sources.push(ConfigSource {
        path: path.to_path_buf(),
        scope,
        association,
        trust_conditional,
        profile_conditional: false,
        uninspected: false,
        detail,
    });
    match toml::from_str::<TomlValue>(&raw) {
        Ok(layer) => {
            merge_toml(effective, layer.clone());
            Some(layer)
        }
        Err(error) => {
            errors.push((path.to_path_buf(), error.to_string()));
            None
        }
    }
}

fn collect_profile_sources(
    env: &Env,
    sources: &mut Vec<ConfigSource>,
    errors: &mut Vec<(PathBuf, String)>,
) -> (bool, Vec<(PathBuf, TomlValue)>) {
    let entries = match fs::read_dir(&env.codex_home) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (false, Vec::new()),
        Err(error) => {
            errors.push((env.codex_home.clone(), error.to_string()));
            return (false, Vec::new());
        }
    };
    let mut profiles = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push((env.codex_home.clone(), error.to_string()));
                continue;
            }
        };
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".config.toml") && name != "config.toml")
            && fs::metadata(&path).is_ok_and(|metadata| metadata.is_file())
        {
            profiles.push(path);
        }
    }
    profiles.sort();
    if profiles.len() > MAX_PROFILE_FILES {
        errors.push((
            env.codex_home.clone(),
            format!("more than {MAX_PROFILE_FILES} Codex profile files were found"),
        ));
        profiles.truncate(MAX_PROFILE_FILES);
    }
    let found = !profiles.is_empty();
    let mut layers = Vec::new();
    for path in profiles {
        sources.push(ConfigSource {
            path: path.clone(),
            scope: Scope::Global,
            association: Association::Global,
            trust_conditional: false,
            profile_conditional: true,
            uninspected: false,
            detail: "profile configuration layer",
        });
        let raw = match safe_io::read_to_string(&path, safe_io::MAX_CONFIG_BYTES) {
            Ok(raw) => raw,
            Err(error) => {
                errors.push((path, error.to_string()));
                continue;
            }
        };
        match toml::from_str::<TomlValue>(&raw) {
            Ok(layer) => layers.push((path, layer)),
            Err(error) => errors.push((path, error.to_string())),
        }
    }
    (found, layers)
}

fn load_requirements_layer(
    path: &Path,
    effective: &mut TomlValue,
    sources: &mut Vec<ConfigSource>,
    errors: &mut Vec<(PathBuf, String)>,
) -> Option<TomlValue> {
    let raw = match safe_io::read_to_string(path, safe_io::MAX_CONFIG_BYTES) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            errors.push((path.to_path_buf(), error.to_string()));
            return None;
        }
    };
    sources.push(ConfigSource {
        path: path.to_path_buf(),
        scope: Scope::Managed,
        association: Association::Global,
        trust_conditional: false,
        profile_conditional: false,
        uninspected: false,
        detail: "administrator-enforced requirements layer",
    });
    let requirements: TomlValue = match toml::from_str(&raw) {
        Ok(requirements) => requirements,
        Err(error) => {
            errors.push((path.to_path_buf(), error.to_string()));
            return None;
        }
    };
    if let Some(features) = requirements.get("features").cloned() {
        let mut overlay = toml::map::Map::new();
        overlay.insert("features".to_string(), features);
        let overlay = TomlValue::Table(overlay);
        merge_toml(effective, overlay.clone());
        Some(overlay)
    } else {
        None
    }
}

fn merge_optional(base: &mut TomlValue, overlay: Option<&TomlValue>) {
    if let Some(overlay) = overlay {
        merge_toml(base, overlay.clone());
    }
}

fn decode_config(
    value: &TomlValue,
    path: &Path,
    errors: &mut Vec<(PathBuf, String)>,
) -> Option<Config> {
    match value.clone().try_into() {
        Ok(config) => Some(config),
        Err(error) => {
            let diagnostic = (
                path.to_path_buf(),
                format!("invalid effective config: {error}"),
            );
            if !errors.contains(&diagnostic) {
                errors.push(diagnostic);
            }
            None
        }
    }
}

fn has_inline_profile(value: &TomlValue) -> bool {
    value.get("profile").is_some() || value.get("profiles").is_some()
}

fn merge_toml(base: &mut TomlValue, overlay: TomlValue) {
    match (base, overlay) {
        (TomlValue::Table(base), TomlValue::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn valid_fallback_names(config: &Config, inventory: &mut ProviderInventory) -> Vec<String> {
    config
        .project_doc_fallback_filenames
        .iter()
        .filter_map(|name| {
            let path = Path::new(name);
            if !name.is_empty() && path.components().count() == 1 {
                Some(name.clone())
            } else {
                inventory.warnings.push(Warning::new(
                    "invalid-codex-fallback-name",
                    format!("ignored fallback instruction name {name:?}"),
                ));
                None
            }
        })
        .collect()
}

fn first_nonempty(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find_map(|path| {
        fs::metadata(path)
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.len() > 0)
            .map(|_| path.clone())
    })
}

fn path_chain(root: &Path, target: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut current = target;
    loop {
        if !current.starts_with(root) {
            return vec![target.to_path_buf()];
        }
        chain.push(current.to_path_buf());
        if current == root {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    chain.reverse();
    chain
}

fn push_instruction(
    inventory: &mut ProviderInventory,
    path: PathBuf,
    scope: Scope,
    trust_root: Option<&Path>,
    configuration_unresolved: bool,
    remaining: &mut usize,
) {
    if let Some(trust_root) = trust_root
        && let Ok(identity) = path.canonicalize()
        && !identity.starts_with(
            trust_root
                .canonicalize()
                .unwrap_or_else(|_| trust_root.to_path_buf()),
        )
    {
        inventory.push_uninspected(
            identity,
            SourceSpec::new(
                SourceRole::Authority,
                SourceKind::Instruction,
                scope,
                LoadState::Unknown,
                Association::Target,
            )
            .with_detail("external instruction approval is unresolved"),
        );
        return;
    }
    let Ok(bytes) = fs::metadata(&path).map(|metadata| metadata.len() as usize) else {
        inventory.warnings.push(Warning::at(
            "source-inaccessible",
            "could not inspect instruction source",
            path,
        ));
        return;
    };
    let (mut load_state, mut detail, consumed) = if *remaining == 0 {
        (
            LoadState::Disabled,
            Some("not loaded because the combined instruction limit was reached".to_string()),
            0,
        )
    } else if bytes > *remaining {
        (
            LoadState::Truncated,
            Some(format!(
                "{} of {bytes} bytes fit within the combined instruction limit",
                *remaining
            )),
            *remaining,
        )
    } else {
        (LoadState::Loaded, None, bytes)
    };
    *remaining = remaining.saturating_sub(consumed);
    if configuration_unresolved {
        load_state = LoadState::Unknown;
        detail = Some(match detail {
            Some(detail) => format!(
                "configuration or project trust is unresolved; if effective, {detail}"
            ),
            None => "configuration or project trust is unresolved; this source's load state depends on it"
                .to_string(),
        });
    }
    inventory.push_path(
        path,
        SourceSpec {
            role: SourceRole::Authority,
            kind: SourceKind::Instruction,
            scope,
            load_state,
            association: if scope == Scope::Global {
                Association::Global
            } else {
                Association::Target
            },
            detail,
            human_detail: super::HumanDetail::Stored,
        },
    );
}

fn memory_load_state(memory_state: MemoryState, on_demand: bool) -> LoadState {
    match memory_state {
        MemoryState::Enabled if on_demand => LoadState::OnDemand,
        MemoryState::Enabled => LoadState::Loaded,
        MemoryState::Disabled => LoadState::Disabled,
        MemoryState::Unknown => LoadState::Unknown,
    }
}

fn collect_memory_sources(
    inventory: &mut ProviderInventory,
    env: &Env,
    request: &DiscoveryRequest<'_>,
    memory_state: MemoryState,
) {
    let memories = env.codex_memories_dir();
    for (name, role, kind, on_demand) in [
        (
            "memory_summary.md",
            SourceRole::RetainedMemory,
            SourceKind::MemorySummary,
            false,
        ),
        (
            "MEMORY.md",
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            true,
        ),
        (
            "raw_memories.md",
            SourceRole::Evidence,
            SourceKind::EvidenceStore,
            true,
        ),
    ] {
        inventory.push_path(
            memories.join(name),
            SourceSpec::new(
                role,
                kind,
                Scope::Global,
                memory_load_state(memory_state, on_demand),
                Association::Global,
            ),
        );
    }

    for (name, role) in [
        ("rollout_summaries", SourceRole::Evidence),
        ("skills", SourceRole::RetainedMemory),
    ] {
        inventory.push_path(
            memories.join(name),
            SourceSpec::new(
                role,
                SourceKind::EvidenceStore,
                Scope::Global,
                memory_load_state(memory_state, true),
                Association::Global,
            )
            .with_detail("contents are resolved lazily"),
        );
    }

    if !request.include_unassociated {
        return;
    }
    let Ok(entries) = fs::read_dir(&memories) else {
        return;
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if matches!(
            name,
            ".git"
                | "memory_summary.md"
                | "MEMORY.md"
                | "raw_memories.md"
                | "rollout_summaries"
                | "skills"
        ) {
            continue;
        }
        inventory.push_path(
            path,
            SourceSpec::new(
                SourceRole::Unknown,
                SourceKind::Unknown,
                Scope::Unknown,
                LoadState::Unknown,
                Association::Unknown,
            )
            .with_detail("unrecognized Codex memory source"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_chain_orders_root_to_target() {
        let root = Path::new("/repo");
        let target = Path::new("/repo/crates/api");
        assert_eq!(
            path_chain(root, target),
            vec![
                PathBuf::from("/repo"),
                PathBuf::from("/repo/crates"),
                PathBuf::from("/repo/crates/api")
            ]
        );
    }

    #[test]
    fn rejects_fallback_paths() {
        let config = Config {
            project_doc_fallback_filenames: vec![
                "TEAM.md".into(),
                "../outside.md".into(),
                "nested/RULES.md".into(),
            ],
            ..Config::default()
        };
        let mut inventory = ProviderInventory::new(Provider::Codex, true, MemoryState::Enabled);
        assert_eq!(valid_fallback_names(&config, &mut inventory), ["TEAM.md"]);
        assert_eq!(inventory.warnings.len(), 2);
    }

    #[test]
    fn project_config_layers_merge_root_to_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("nested");
        std::fs::create_dir_all(target.join(".codex")).unwrap();
        std::fs::create_dir_all(root.path().join(".codex")).unwrap();
        std::fs::write(
            root.path().join(".codex/config.toml"),
            "project_doc_fallback_filenames = [\"ROOT.md\"]\nproject_doc_max_bytes = 10\n",
        )
        .unwrap();
        std::fs::write(
            target.join(".codex/config.toml"),
            "project_doc_fallback_filenames = [\"NESTED.md\"]\n",
        )
        .unwrap();
        let codex_home = root.path().join("codex-home");
        let env = Env::new(None, None).with_codex_home(Some(codex_home));

        let resolved = read_config_layers(&env, root.path(), &target);

        assert!(resolved.errors.is_empty());
        assert!(resolved.has_project_layers);
        assert_eq!(
            resolved.config.project_doc_fallback_filenames,
            ["NESTED.md"]
        );
        assert_eq!(resolved.config.project_doc_max_bytes, Some(10));
    }

    #[test]
    fn profile_files_are_inventoried_but_not_applied_without_cli_state() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            "profile = \"legacy\"\n[features]\nmemories = false\n[profiles.legacy.features]\nmemories = true\n",
        )
        .unwrap();
        std::fs::write(
            codex_home.join("work.config.toml"),
            "[features]\nmemories = true\n",
        )
        .unwrap();
        let env = Env::new(None, None).with_codex_home(Some(codex_home));
        let system = SystemConfigPaths {
            config: root.path().join("missing-system.toml"),
            managed_config: root.path().join("missing-managed.toml"),
            requirements: root.path().join("missing-requirements.toml"),
        };

        let resolved = read_config_layers_with_paths(&env, root.path(), root.path(), &system);

        assert_eq!(resolved.config.features.memories, Some(false));
        assert!(resolved.has_profile_files);
        assert!(resolved.has_legacy_inline_profiles);
        assert!(
            resolved
                .possible_configs
                .iter()
                .any(|config| config.features.memories == Some(true))
        );
    }

    #[test]
    fn managed_defaults_and_requirements_override_observed_config() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            "[features]\nmemories = true\n",
        )
        .unwrap();
        let managed = root.path().join("managed.toml");
        let requirements = root.path().join("requirements.toml");
        std::fs::write(&managed, "[features]\nmemories = true\n").unwrap();
        std::fs::write(&requirements, "[features]\nmemories = false\n").unwrap();
        let env = Env::new(None, None).with_codex_home(Some(codex_home));
        let system = SystemConfigPaths {
            config: root.path().join("missing-system.toml"),
            managed_config: managed.clone(),
            requirements: requirements.clone(),
        };

        let resolved = read_config_layers_with_paths(&env, root.path(), root.path(), &system);

        assert_eq!(resolved.config.features.memories, Some(false));
        assert!(resolved.sources.iter().any(|source| source.path == managed));
        assert!(
            resolved
                .sources
                .iter()
                .any(|source| source.path == requirements)
        );
    }
}
