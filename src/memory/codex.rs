use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use super::{
    Adapter, Association, DiscoveryRequest, LoadState, MemoryState, Provider, ProviderInventory,
    Scope, SourceKind, SourceRole, SourceSpec, Warning,
};
use crate::git;
use crate::paths::Env;

const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    project_doc_fallback_filenames: Vec<String>,
    project_doc_max_bytes: Option<usize>,
    features: Features,
    memories: Memories,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Features {
    memories: Option<bool>,
}

#[derive(Default, Deserialize)]
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
        let config_path = self.env.codex_config();
        let (config, config_valid) = read_config(&config_path);
        let memory_state = if !config_valid {
            MemoryState::Unknown
        } else if config.features.memories.unwrap_or(false)
            && config.memories.use_memories.unwrap_or(true)
        {
            MemoryState::Enabled
        } else {
            MemoryState::Disabled
        };
        let configured = self.env.codex_home.is_dir()
            || config_path.is_file()
            || self.env.codex_memories_dir().is_dir();
        let mut inventory = ProviderInventory::new(Provider::Codex, configured, memory_state);

        if config_path.exists() {
            inventory.push_path(
                config_path.clone(),
                SourceSpec::new(
                    SourceRole::OperationalState,
                    SourceKind::Configuration,
                    Scope::Global,
                    LoadState::Loaded,
                    Association::Global,
                ),
            );
        }
        if !config_valid {
            inventory.warnings.push(Warning::at(
                "invalid-codex-config",
                "could not parse Codex config; memory state and fallback instruction names are unknown",
                config_path,
            ));
        }

        let fallback_names = valid_fallback_names(&config, &mut inventory);
        let mut remaining = config
            .project_doc_max_bytes
            .unwrap_or(DEFAULT_PROJECT_DOC_MAX_BYTES);
        if let Some(global) = first_nonempty(&[
            self.env.codex_home.join("AGENTS.override.md"),
            self.env.codex_home.join("AGENTS.md"),
        ]) {
            push_instruction(&mut inventory, global, Scope::Global, &mut remaining);
        }

        let repository_root =
            git::repository_root(request.target).unwrap_or_else(|| request.target.to_path_buf());
        for directory in path_chain(&repository_root, request.target) {
            let mut candidates = vec![
                directory.join("AGENTS.override.md"),
                directory.join("AGENTS.md"),
            ];
            candidates.extend(fallback_names.iter().map(|name| directory.join(name)));
            if let Some(path) = first_nonempty(&candidates) {
                let scope = if directory == repository_root {
                    Scope::Repository
                } else {
                    Scope::Path
                };
                push_instruction(&mut inventory, path, scope, &mut remaining);
            }
        }

        collect_memory_sources(&mut inventory, self.env, request, memory_state);
        inventory.configured = inventory.configured || !inventory.sources.is_empty();
        inventory
    }
}

fn read_config(path: &Path) -> (Config, bool) {
    match fs::read_to_string(path) {
        Ok(raw) => toml::from_str(&raw).map_or_else(|_| (Config::default(), false), |v| (v, true)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Config::default(), true),
        Err(_) => (Config::default(), false),
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
    remaining: &mut usize,
) {
    let Ok(bytes) = fs::metadata(&path).map(|metadata| metadata.len() as usize) else {
        inventory.warnings.push(Warning::at(
            "source-inaccessible",
            "could not inspect instruction source",
            path,
        ));
        return;
    };
    let (load_state, detail, consumed) = if *remaining == 0 {
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
}
