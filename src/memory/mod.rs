mod claude;
mod codex;
#[allow(
    dead_code,
    reason = "the item contract is consumed by the next memory diff slice"
)]
mod items;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use colored::{ColoredString, Colorize};
use serde::{Serialize, Serializer};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::output;
use crate::paths::Env;

pub struct ShowOptions {
    pub path: PathBuf,
    pub provider: ProviderFilter,
    pub include_unassociated: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderFilter {
    All,
    Codex,
    Claude,
}

impl ProviderFilter {
    fn includes(self, provider: Provider) -> bool {
        self == Self::All
            || matches!(
                (self, provider),
                (Self::Codex, Provider::Codex) | (Self::Claude, Provider::Claude)
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Provider {
    Codex,
    Claude,
}

impl Provider {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn styled(self) -> ColoredString {
        match self {
            Self::Codex => self.as_str().blue().bold().underline(),
            Self::Claude => self.as_str().magenta().bold().underline(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SourceRole {
    Authority,
    RetainedMemory,
    Evidence,
    OperationalState,
    Unknown,
}

impl SourceRole {
    fn label(self) -> &'static str {
        match self {
            Self::Authority => "instructions",
            Self::RetainedMemory => "retained memory",
            Self::Evidence => "evidence",
            Self::OperationalState => "operational state",
            Self::Unknown => "unclassified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SourceKind {
    Instruction,
    Rule,
    ImportedInstruction,
    MemorySummary,
    MemoryIndex,
    MemoryTopic,
    EvidenceStore,
    Configuration,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Scope {
    Managed,
    Global,
    Repository,
    Path,
    Unknown,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Global => "global",
            Self::Repository => "repository",
            Self::Path => "path",
            Self::Unknown => "unknown scope",
        }
    }

    fn styled(self) -> ColoredString {
        self.label().dimmed()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum LoadState {
    Loaded,
    Truncated,
    OnDemand,
    Disabled,
    Unknown,
}

impl LoadState {
    fn label(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Truncated => "truncated",
            Self::OnDemand => "on demand",
            Self::Disabled => "disabled",
            Self::Unknown => "load unknown",
        }
    }

    fn styled(self) -> ColoredString {
        match self {
            Self::Loaded => self.label().green(),
            Self::Truncated => self.label().yellow(),
            Self::OnDemand => self.label().cyan(),
            Self::Disabled => self.label().red(),
            Self::Unknown => self.label().yellow(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Association {
    Global,
    Target,
    Other,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum MemoryState {
    Enabled,
    Disabled,
    Unknown,
}

impl MemoryState {
    fn label(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Unknown => "unknown",
        }
    }

    fn styled(self) -> ColoredString {
        match self {
            Self::Enabled => self.label().green().bold(),
            Self::Disabled => self.label().red().bold(),
            Self::Unknown => self.label().yellow().bold(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Support {
    Supported,
    ReadOnly,
}

#[derive(Debug, Serialize)]
pub(super) struct Capabilities {
    pub instruction_inventory: Support,
    pub memory_inventory: Support,
    pub evidence_inventory: Support,
    pub management: Support,
}

impl Capabilities {
    pub(super) fn read_only() -> Self {
        Self {
            instruction_inventory: Support::Supported,
            memory_inventory: Support::Supported,
            evidence_inventory: Support::Supported,
            management: Support::ReadOnly,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Source {
    pub id: String,
    pub provider: Provider,
    pub role: SourceRole,
    pub kind: SourceKind,
    pub scope: Scope,
    pub load_state: LoadState,
    pub association: Association,
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    pub bytes: Option<u64>,
    pub inventory_order: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip)]
    pub human_detail: HumanDetail,
}

#[derive(Debug)]
pub(super) enum HumanDetail {
    Stored,
    Hidden,
    Replacement(String),
}

pub(super) struct SourceSpec {
    pub role: SourceRole,
    pub kind: SourceKind,
    pub scope: Scope,
    pub load_state: LoadState,
    pub association: Association,
    pub detail: Option<String>,
    pub human_detail: HumanDetail,
}

impl SourceSpec {
    pub(super) fn new(
        role: SourceRole,
        kind: SourceKind,
        scope: Scope,
        load_state: LoadState,
        association: Association,
    ) -> Self {
        Self {
            role,
            kind,
            scope,
            load_state,
            association,
            detail: None,
            human_detail: HumanDetail::Stored,
        }
    }

    pub(super) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl Source {
    pub(super) fn from_path(
        provider: Provider,
        path: PathBuf,
        spec: SourceSpec,
    ) -> std::io::Result<Self> {
        let metadata = fs::metadata(&path)?;
        Ok(Self {
            id: format!("{}:{}", provider.as_str(), path.display()),
            provider,
            role: spec.role,
            kind: spec.kind,
            scope: spec.scope,
            load_state: spec.load_state,
            association: spec.association,
            path,
            bytes: metadata.is_file().then_some(metadata.len()),
            inventory_order: 0,
            detail: spec.detail,
            human_detail: spec.human_detail,
        })
    }

    fn human_detail(&self) -> Option<&str> {
        match &self.human_detail {
            HumanDetail::Stored => self.detail.as_deref(),
            HumanDetail::Hidden => None,
            HumanDetail::Replacement(detail) => Some(detail),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Warning {
    pub code: &'static str,
    pub message: String,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_path"
    )]
    pub path: Option<PathBuf>,
}

impl Warning {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
        }
    }

    pub(super) fn at(code: &'static str, message: impl Into<String>, path: PathBuf) -> Self {
        Self {
            code,
            message: message.into(),
            path: Some(path),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct ProviderInventory {
    pub provider: Provider,
    pub configured: bool,
    pub memory_state: MemoryState,
    pub capabilities: Capabilities,
    pub sources: Vec<Source>,
    pub warnings: Vec<Warning>,
}

impl ProviderInventory {
    pub(super) fn new(provider: Provider, configured: bool, memory_state: MemoryState) -> Self {
        Self {
            provider,
            configured,
            memory_state,
            capabilities: Capabilities::read_only(),
            sources: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub(super) fn push(&mut self, mut source: Source) {
        source.inventory_order = self.sources.len();
        self.sources.push(source);
    }

    pub(super) fn push_path(&mut self, path: PathBuf, spec: SourceSpec) {
        match Source::from_path(self.provider, path.clone(), spec) {
            Ok(source) => self.push(source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => self.warnings.push(Warning::at(
                "source-inaccessible",
                format!("could not inspect source: {error}"),
                path,
            )),
        }
    }
}

pub(super) struct DiscoveryRequest<'a> {
    pub target: &'a Path,
    pub include_unassociated: bool,
}

pub(super) trait Adapter {
    fn discover(&self, request: &DiscoveryRequest<'_>) -> ProviderInventory;
}

#[derive(Debug, Serialize)]
struct Inventory {
    #[serde(serialize_with = "serialize_path")]
    target: PathBuf,
    include_unassociated: bool,
    providers: Vec<ProviderInventory>,
}

pub fn run_show(env: &Env, opts: ShowOptions) -> Result<ExitCode> {
    let target = opts
        .path
        .canonicalize()
        .with_context(|| format!("target directory not found: {}", opts.path.display()))?;
    if !target.is_dir() {
        bail!("target is not a directory: {}", target.display());
    }

    let request = DiscoveryRequest {
        target: &target,
        include_unassociated: opts.include_unassociated,
    };
    let mut providers = Vec::new();
    if opts.provider.includes(Provider::Codex) {
        providers.push(codex::CodexAdapter::new(env).discover(&request));
    }
    if opts.provider.includes(Provider::Claude) {
        providers.push(claude::ClaudeAdapter::new(env).discover(&request));
    }

    let inventory = Inventory {
        target,
        include_unassociated: opts.include_unassociated,
        providers,
    };
    if opts.json {
        let json = serde_json::to_string_pretty(&inventory)
            .context("could not serialize memory inventory")?;
        println!("{json}");
    } else {
        emit_human(&inventory);
    }
    Ok(ExitCode::SUCCESS)
}

// JSON strings require Unicode while Unix paths do not. Match the CLI's human
// rendering instead of letting one byte-oriented path abort the whole report.
fn serialize_path<S>(path: &Path, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&path.display().to_string())
}

fn serialize_optional_path<S>(
    path: &Option<PathBuf>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match path {
        Some(path) => serializer.serialize_some(&path.display().to_string()),
        None => serializer.serialize_none(),
    }
}

fn emit_human(inventory: &Inventory) {
    println!(
        "{} {}",
        "memory for".bold(),
        inventory.target.display().to_string().bold()
    );

    for provider in &inventory.providers {
        println!(
            "\n{}  memory {}  management {}",
            provider.provider.styled(),
            provider.memory_state.styled(),
            "read-only".dimmed()
        );

        if provider.sources.is_empty() {
            println!("  {}", "no sources found".dimmed());
        }
        for role in [
            SourceRole::Authority,
            SourceRole::RetainedMemory,
            SourceRole::Evidence,
            SourceRole::OperationalState,
            SourceRole::Unknown,
        ] {
            let sources = provider
                .sources
                .iter()
                .filter(|source| source.role == role)
                .collect::<Vec<_>>();
            if sources.is_empty() {
                continue;
            }
            println!("  {}", role.label().bold());
            for source in sources {
                let bytes = source
                    .bytes
                    .map(|bytes| {
                        format!(" ({})", output::human_bytes(bytes))
                            .dimmed()
                            .to_string()
                    })
                    .unwrap_or_default();
                println!(
                    "    [{}; {}] {}{}",
                    source.scope.styled(),
                    source.load_state.styled(),
                    source.path.display().to_string().bold(),
                    bytes
                );
                if let Some(detail) = source.human_detail() {
                    println!("      {}", detail.dimmed());
                }
            }
        }
        for warning in &provider.warnings {
            match &warning.path {
                Some(path) => println!(
                    "  {} [{}] {}: {}",
                    "warning".yellow().bold(),
                    warning.code.yellow(),
                    path.display().to_string().bold(),
                    warning.message
                ),
                None => println!(
                    "  {} [{}] {}",
                    "warning".yellow().bold(),
                    warning.code.yellow(),
                    warning.message
                ),
            }
        }
    }

    if inventory.providers.len() > 1 {
        println!("\n{}", "provider coverage".bold().underline());
        for provider in &inventory.providers {
            let instructions = provider
                .sources
                .iter()
                .filter(|source| source.role == SourceRole::Authority)
                .count();
            let memories = provider
                .sources
                .iter()
                .filter(|source| source.role == SourceRole::RetainedMemory)
                .count();
            let instruction_label = if instructions == 1 {
                "instruction"
            } else {
                "instructions"
            };
            let memory_label = if memories == 1 {
                "retained memory"
            } else {
                "retained memories"
            };
            println!(
                "  {}: {instructions} {instruction_label}, {memories} {memory_label}",
                provider.provider.styled(),
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn json_inventory_serializes_non_utf8_paths_without_panicking() {
        let path = PathBuf::from(OsString::from_vec(b"invalid-\xff-path".to_vec()));
        let mut provider = ProviderInventory::new(Provider::Claude, true, MemoryState::Unknown);
        provider.push(Source {
            id: "claude:invalid-path".to_string(),
            provider: Provider::Claude,
            role: SourceRole::Unknown,
            kind: SourceKind::Unknown,
            scope: Scope::Unknown,
            load_state: LoadState::Unknown,
            association: Association::Unknown,
            path: path.clone(),
            bytes: None,
            inventory_order: 0,
            detail: None,
            human_detail: HumanDetail::Stored,
        });
        provider.warnings.push(Warning::at(
            "invalid-path",
            "path is not UTF-8",
            path.clone(),
        ));
        let inventory = Inventory {
            target: path.clone(),
            include_unassociated: false,
            providers: vec![provider],
        };

        let encoded = serde_json::to_string_pretty(&inventory).unwrap();
        let json: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let displayed = path.display().to_string();
        assert_eq!(json["target"], displayed);
        assert_eq!(json["providers"][0]["sources"][0]["path"], displayed);
        assert_eq!(json["providers"][0]["warnings"][0]["path"], displayed);
    }
}
