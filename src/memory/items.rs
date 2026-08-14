use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{Association, Provider, Scope, Source, SourceKind};

const DEFAULT_MAX_SOURCE_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_ITEMS: usize = 4096;
const DEFAULT_MAX_EVIDENCE_REFS: usize = 64;
/// Advisory: a fenced header is ambiguous and does not by itself prove data loss.
const ISSUE_FENCED_HEADER_SUPPRESSED: &str = "memory-item-fenced-header-suppressed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReadLimits {
    pub max_source_bytes: usize,
    pub max_items: usize,
    pub max_evidence_refs_per_item: usize,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            max_items: DEFAULT_MAX_ITEMS,
            max_evidence_refs_per_item: DEFAULT_MAX_EVIDENCE_REFS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemFormat {
    CodexGeneratedSummary,
    CodexTaskGroups,
    CodexThreads,
    ClaudeIndex,
    ClaudeTopic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemRole {
    RetainedMemory,
    Evidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemGranularity {
    Source,
    Section,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdStability {
    Native,
    Source,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum EvidenceRefKind {
    File,
    ProviderRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct EvidenceRef {
    pub kind: EvidenceRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SourceRange {
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MemoryItem {
    pub id: String,
    pub id_stability: IdStability,
    pub provider: Provider,
    pub source_id: String,
    pub source_path: PathBuf,
    pub role: ItemRole,
    pub granularity: ItemGranularity,
    pub scope: Scope,
    pub association: Association,
    pub range: SourceRange,
    pub fingerprint: String,
    pub title: Option<String>,
    pub native_kind: Option<String>,
    pub repository_targets: Vec<String>,
    pub evidence: Vec<EvidenceRef>,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExtractionIssue {
    pub code: &'static str,
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ItemExtraction {
    pub format: Option<ItemFormat>,
    pub items: Vec<MemoryItem>,
    pub bytes_read: usize,
    pub complete: bool,
    pub issues: Vec<ExtractionIssue>,
}

impl ItemExtraction {
    fn unsupported() -> Self {
        Self {
            format: None,
            items: Vec::new(),
            bytes_read: 0,
            complete: true,
            issues: Vec::new(),
        }
    }

    fn recognized(format: ItemFormat) -> Self {
        Self {
            format: Some(format),
            items: Vec::new(),
            bytes_read: 0,
            complete: true,
            issues: Vec::new(),
        }
    }

    fn issue(&mut self, code: &'static str, line: Option<usize>, message: impl Into<String>) {
        self.issues.push(ExtractionIssue {
            code,
            line,
            message: message.into(),
        });
    }

    fn incomplete_issue(
        &mut self,
        code: &'static str,
        line: Option<usize>,
        message: impl Into<String>,
    ) {
        self.complete = false;
        self.issue(code, line, message);
    }
}

pub(super) fn extract_source(source: &Source) -> Result<ItemExtraction> {
    extract_source_with_limits(source, ReadLimits::default())
}

pub(super) fn extract_source_with_limits(
    source: &Source,
    limits: ReadLimits,
) -> Result<ItemExtraction> {
    let Some(format) = item_format(source) else {
        return Ok(ItemExtraction::unsupported());
    };
    let mut extraction = ItemExtraction::recognized(format);
    let mut bytes = match read_bounded(&source.path, limits.max_source_bytes) {
        Ok(bytes) => bytes,
        Err(error) => {
            extraction.incomplete_issue(
                "memory-item-source-unreadable",
                None,
                format!("could not read memory source: {error}"),
            );
            return Ok(extraction);
        }
    };
    extraction.bytes_read = bytes.bytes_read;
    if bytes.truncated {
        extraction.incomplete_issue(
            "memory-item-source-byte-limit",
            None,
            format!(
                "only the first {} bytes were inspected",
                limits.max_source_bytes
            ),
        );
    }

    let raw = match std::str::from_utf8(&bytes.content) {
        Ok(raw) => raw,
        Err(error) if bytes.truncated && error.error_len().is_none() => {
            bytes.content.truncate(error.valid_up_to());
            extraction.bytes_read = bytes.content.len();
            std::str::from_utf8(&bytes.content).expect("valid UTF-8 prefix")
        }
        Err(error) => {
            extraction.incomplete_issue(
                "memory-item-source-invalid-utf8",
                None,
                format!("source is not UTF-8 at byte {}", error.valid_up_to()),
            );
            return Ok(extraction);
        }
    };

    match format {
        ItemFormat::CodexGeneratedSummary => extract_source_item(
            source,
            raw,
            bytes.truncated,
            "generated-summary",
            &mut extraction,
        ),
        ItemFormat::CodexTaskGroups => {
            extract_codex_task_groups(source, raw, bytes.truncated, limits, &mut extraction)
        }
        ItemFormat::CodexThreads => {
            extract_codex_threads(source, raw, bytes.truncated, limits, &mut extraction)
        }
        ItemFormat::ClaudeIndex => extract_source_item(
            source,
            raw,
            bytes.truncated,
            "memory-index",
            &mut extraction,
        ),
        ItemFormat::ClaudeTopic => {
            extract_claude_topic(source, raw, bytes.truncated, limits, &mut extraction)
        }
    }

    Ok(extraction)
}

fn item_format(source: &Source) -> Option<ItemFormat> {
    match (source.provider, source.kind) {
        (Provider::Codex, SourceKind::MemorySummary) => Some(ItemFormat::CodexGeneratedSummary),
        (Provider::Codex, SourceKind::MemoryIndex) => Some(ItemFormat::CodexTaskGroups),
        (Provider::Codex, SourceKind::EvidenceStore)
            if source.path.file_name().and_then(|name| name.to_str())
                == Some("raw_memories.md") =>
        {
            Some(ItemFormat::CodexThreads)
        }
        (Provider::Claude, SourceKind::MemoryIndex) => Some(ItemFormat::ClaudeIndex),
        (Provider::Claude, SourceKind::MemoryTopic) => Some(ItemFormat::ClaudeTopic),
        _ => None,
    }
}

struct BoundedRead {
    content: Vec<u8>,
    bytes_read: usize,
    truncated: bool,
}

fn read_bounded(path: &Path, max_bytes: usize) -> Result<BoundedRead> {
    let file = File::open(path)?;
    let read_limit = max_bytes.saturating_add(1);
    let mut content = Vec::with_capacity(read_limit.min(64 * 1024));
    file.take(read_limit as u64).read_to_end(&mut content)?;
    let truncated = content.len() > max_bytes;
    if truncated {
        content.truncate(max_bytes);
    }
    Ok(BoundedRead {
        bytes_read: content.len(),
        content,
        truncated,
    })
}

fn extract_source_item(
    source: &Source,
    raw: &str,
    source_truncated: bool,
    native_kind: &str,
    extraction: &mut ItemExtraction,
) {
    if source_truncated || raw.trim().is_empty() {
        return;
    }
    let range = whole_source_range(raw);
    extraction.items.push(source_item(
        source,
        ItemSpec {
            role: ItemRole::RetainedMemory,
            granularity: ItemGranularity::Source,
            range,
            content: raw,
            title: None,
            native_kind: Some(native_kind.to_string()),
            repository_targets: Vec::new(),
            evidence: Vec::new(),
        },
    ));
}

fn extract_codex_task_groups(
    source: &Source,
    raw: &str,
    source_truncated: bool,
    limits: ReadLimits,
    extraction: &mut ItemExtraction,
) {
    let sections = section_ranges(raw, source_truncated, limits.max_items, |line| {
        if let Some(title) = line.strip_prefix("# Task Group: ") {
            let title = title.trim();
            if title.is_empty() {
                HeaderMatch::Unparsed
            } else {
                HeaderMatch::Parsed(title)
            }
        } else if line.starts_with("# Task Group:") {
            HeaderMatch::Unparsed
        } else {
            HeaderMatch::None
        }
    });
    if sections.item_limit_reached {
        extraction.incomplete_issue(
            "memory-item-count-limit",
            None,
            format!("only the first {} items were extracted", limits.max_items),
        );
    }
    for line in &sections.unparsed_headers {
        extraction.incomplete_issue(
            "memory-item-record-header-unparsed",
            Some(*line),
            "could not parse Codex Task Group header",
        );
    }
    if let Some(line) = sections.unclosed_fence_line {
        extraction.incomplete_issue(
            "memory-item-unclosed-code-fence",
            Some(line),
            "unclosed code fence prevented complete Codex Task Group extraction",
        );
    }
    if !source_truncated && sections.unclosed_fence_line.is_none() {
        for line in &sections.fenced_header_candidates {
            extraction.incomplete_issue(
                ISSUE_FENCED_HEADER_SUPPRESSED,
                Some(*line),
                "header-shaped text inside a code fence may hide a Codex Task Group",
            );
        }
    }
    if !sections.saw_header_candidate
        && !source_truncated
        && !sections.item_limit_reached
        && sections.unclosed_fence_line.is_none()
        && !raw.trim().is_empty()
    {
        extraction.incomplete_issue(
            "codex-memory-task-group-format-unrecognized",
            None,
            "Codex durable memory contains no Task Group sections",
        );
    }

    let mut occurrences = BTreeMap::new();
    for section in sections.ranges {
        let content = &raw[section.range.start_byte..section.range.end_byte];
        let targets = codex_repository_targets(content);
        let evidence = limit_evidence_refs(
            codex_task_group_evidence(content),
            limits.max_evidence_refs_per_item,
            section.range.start_line,
            extraction,
        );
        let occurrence = next_occurrence(&mut occurrences, &section.title, content);
        extraction.items.push(ephemeral_item(
            source,
            ItemSpec {
                role: ItemRole::RetainedMemory,
                granularity: ItemGranularity::Section,
                range: section.range,
                content,
                title: Some(section.title),
                native_kind: Some("task-group".to_string()),
                repository_targets: targets,
                evidence,
            },
            occurrence,
        ));
    }
}

fn extract_codex_threads(
    source: &Source,
    raw: &str,
    source_truncated: bool,
    limits: ReadLimits,
    extraction: &mut ItemExtraction,
) {
    let sections = section_ranges(raw, source_truncated, limits.max_items, |line| {
        if !line.starts_with("## Thread ") {
            return HeaderMatch::None;
        }
        line.strip_prefix("## Thread `")
            .and_then(|thread| thread.strip_suffix('`'))
            .map(str::trim)
            .filter(|thread| !thread.is_empty())
            .map_or(HeaderMatch::Unparsed, HeaderMatch::Parsed)
    });
    if sections.item_limit_reached {
        extraction.incomplete_issue(
            "memory-item-count-limit",
            None,
            format!("only the first {} items were extracted", limits.max_items),
        );
    }
    for line in &sections.unparsed_headers {
        extraction.incomplete_issue(
            "memory-item-record-header-unparsed",
            Some(*line),
            "could not parse Codex Thread header",
        );
    }
    if let Some(line) = sections.unclosed_fence_line {
        extraction.incomplete_issue(
            "memory-item-unclosed-code-fence",
            Some(line),
            "unclosed code fence prevented complete Codex Thread extraction",
        );
    }
    if !source_truncated && sections.unclosed_fence_line.is_none() {
        for line in &sections.fenced_header_candidates {
            extraction.incomplete_issue(
                ISSUE_FENCED_HEADER_SUPPRESSED,
                Some(*line),
                "header-shaped text inside a code fence may hide a Codex Thread",
            );
        }
    }
    if !sections.saw_header_candidate
        && !source_truncated
        && !sections.item_limit_reached
        && sections.unclosed_fence_line.is_none()
        && !raw.trim().is_empty()
    {
        extraction.incomplete_issue(
            "codex-memory-thread-format-unrecognized",
            None,
            "Codex raw memory contains no Thread sections",
        );
    }

    let mut native_ids = BTreeSet::new();
    let mut occurrences = BTreeMap::new();
    for section in sections.ranges {
        let content = &raw[section.range.start_byte..section.range.end_byte];
        let evidence = limit_evidence_refs(
            codex_thread_evidence(content),
            limits.max_evidence_refs_per_item,
            section.range.start_line,
            extraction,
        );
        let targets = field_value(content, "cwd:")
            .filter(|cwd| !cwd.is_empty())
            .map(|cwd| vec![cwd.to_string()])
            .unwrap_or_default();
        let native_id = (!section.title.is_empty()).then_some(section.title.as_str());
        let duplicate = native_id.is_some_and(|id| !native_ids.insert(id.to_string()));
        let item = if let Some(native_id) = native_id.filter(|_| !duplicate) {
            native_item(
                source,
                native_id,
                ItemSpec {
                    role: ItemRole::Evidence,
                    granularity: ItemGranularity::Section,
                    range: section.range,
                    content,
                    title: Some(format!("Thread {native_id}")),
                    native_kind: Some("thread".to_string()),
                    repository_targets: targets,
                    evidence,
                },
            )
        } else {
            if duplicate {
                extraction.issue(
                    "memory-item-duplicate-native-id",
                    Some(section.range.start_line),
                    format!("duplicate Codex thread ID {:?}", section.title),
                );
            } else {
                extraction.issue(
                    "memory-item-native-id-missing",
                    Some(section.range.start_line),
                    "Codex thread record has no native ID",
                );
            }
            let occurrence = next_occurrence(&mut occurrences, &section.title, content);
            ephemeral_item(
                source,
                ItemSpec {
                    role: ItemRole::Evidence,
                    granularity: ItemGranularity::Section,
                    range: section.range,
                    content,
                    title: Some(section.title),
                    native_kind: Some("thread".to_string()),
                    repository_targets: targets,
                    evidence,
                },
                occurrence,
            )
        };
        extraction.items.push(item);
    }
}

fn extract_claude_topic(
    source: &Source,
    raw: &str,
    source_truncated: bool,
    limits: ReadLimits,
    extraction: &mut ItemExtraction,
) {
    if source_truncated || raw.trim().is_empty() {
        return;
    }
    let metadata = match claude_frontmatter(raw) {
        Ok(metadata) => metadata,
        Err((code, message)) => {
            if code == "claude-memory-frontmatter-malformed" {
                extraction.incomplete_issue(code, Some(1), message);
            } else {
                extraction.issue(code, Some(1), message);
            }
            ClaudeFrontmatter::default()
        }
    };
    let range = whole_source_range(raw);
    let evidence = limit_evidence_refs(
        metadata
            .origin_session_id
            .iter()
            .map(|session| EvidenceRef {
                kind: EvidenceRefKind::ProviderRecord,
                value: session.clone(),
            })
            .collect(),
        limits.max_evidence_refs_per_item,
        1,
        extraction,
    );
    let title = metadata.name.clone().or_else(|| {
        source
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    });
    let item = if let Some(native_id) = metadata.name.as_deref().filter(|name| !name.is_empty()) {
        native_item(
            source,
            native_id,
            ItemSpec {
                role: ItemRole::RetainedMemory,
                granularity: ItemGranularity::Source,
                range,
                content: raw,
                title,
                native_kind: metadata.native_kind,
                repository_targets: Vec::new(),
                evidence,
            },
        )
    } else {
        extraction.issue(
            "memory-item-native-id-missing",
            Some(1),
            "Claude memory topic has no frontmatter name",
        );
        source_item(
            source,
            ItemSpec {
                role: ItemRole::RetainedMemory,
                granularity: ItemGranularity::Source,
                range,
                content: raw,
                title,
                native_kind: metadata.native_kind,
                repository_targets: Vec::new(),
                evidence,
            },
        )
    };
    extraction.items.push(item);
}

fn whole_source_range(raw: &str) -> SourceRange {
    SourceRange {
        start_line: 1,
        end_line: raw.lines().count().max(1),
        start_byte: 0,
        end_byte: raw.len(),
    }
}

struct NamedRange {
    title: String,
    range: SourceRange,
}

struct SectionRanges {
    ranges: Vec<NamedRange>,
    item_limit_reached: bool,
    unparsed_headers: Vec<usize>,
    fenced_header_candidates: Vec<usize>,
    unclosed_fence_line: Option<usize>,
    saw_header_candidate: bool,
}

#[derive(Clone, Copy)]
enum HeaderMatch<'a> {
    None,
    Parsed(&'a str),
    Unparsed,
}

fn section_ranges<'a>(
    raw: &'a str,
    source_truncated: bool,
    max_items: usize,
    mut header: impl FnMut(&'a str) -> HeaderMatch<'a>,
) -> SectionRanges {
    let mut ranges = Vec::new();
    let mut active = None;
    let mut item_limit_reached = false;
    let mut unparsed_headers = Vec::new();
    let mut fenced_header_candidates = Vec::new();
    let mut saw_header_candidate = false;
    let mut fence = None;
    let mut byte_offset = 0;
    for (line_index, raw_line) in raw.split_inclusive('\n').enumerate() {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if let Some(marker) = markdown_fence(line) {
            match fence {
                Some((open, _)) if marker.closes(open) => fence = None,
                None => fence = Some((marker, line_index + 1)),
                Some(_) => {}
            }
            byte_offset += raw_line.len();
            continue;
        }
        if fence.is_some() {
            if !matches!(header(line), HeaderMatch::None) {
                fenced_header_candidates.push(line_index + 1);
            }
            byte_offset += raw_line.len();
            continue;
        }

        let matched = header(line);
        if !matches!(matched, HeaderMatch::None) {
            saw_header_candidate = true;
            if let Some(active) = active.take() {
                push_completed_range(
                    &mut ranges,
                    &mut item_limit_reached,
                    max_items,
                    active,
                    byte_offset,
                    line_index,
                );
            }
            match matched {
                HeaderMatch::Parsed(title) => {
                    active = Some(ActiveHeader {
                        start_byte: byte_offset,
                        start_line: line_index + 1,
                        title: title.to_string(),
                    });
                }
                HeaderMatch::Unparsed => {
                    let partial_final_line = source_truncated
                        && byte_offset + raw_line.len() == raw.len()
                        && !raw_line.ends_with('\n');
                    if !partial_final_line {
                        unparsed_headers.push(line_index + 1);
                    }
                }
                HeaderMatch::None => unreachable!(),
            }
        }
        byte_offset += raw_line.len();
    }

    let unclosed_fence_line = if source_truncated {
        None
    } else {
        fence.map(|(_, line)| line)
    };
    if !source_truncated
        && fence.is_none()
        && let Some(active) = active
    {
        push_completed_range(
            &mut ranges,
            &mut item_limit_reached,
            max_items,
            active,
            raw.len(),
            raw.lines().count().max(1),
        );
    }
    SectionRanges {
        ranges,
        item_limit_reached,
        unparsed_headers,
        fenced_header_candidates,
        unclosed_fence_line,
        saw_header_candidate,
    }
}

struct ActiveHeader {
    start_byte: usize,
    start_line: usize,
    title: String,
}

fn push_completed_range(
    ranges: &mut Vec<NamedRange>,
    item_limit_reached: &mut bool,
    max_items: usize,
    header: ActiveHeader,
    end_byte: usize,
    end_line: usize,
) {
    if ranges.len() == max_items {
        *item_limit_reached = true;
        return;
    }
    ranges.push(NamedRange {
        title: header.title,
        range: SourceRange {
            start_line: header.start_line,
            end_line: end_line.max(header.start_line),
            start_byte: header.start_byte,
            end_byte,
        },
    });
}

#[derive(Clone, Copy)]
struct Fence {
    marker: char,
    width: usize,
    trailing_content: bool,
}

impl Fence {
    fn closes(self, open: Self) -> bool {
        self.marker == open.marker && self.width >= open.width && !self.trailing_content
    }
}

fn markdown_fence(line: &str) -> Option<Fence> {
    let indentation = line
        .chars()
        .take_while(|character| *character == ' ')
        .count();
    if indentation > 3 {
        return None;
    }
    let content = &line[indentation..];
    let marker = content
        .chars()
        .next()
        .filter(|marker| matches!(marker, '`' | '~'))?;
    let width = content
        .chars()
        .take_while(|character| *character == marker)
        .count();
    (width >= 3).then(|| Fence {
        marker,
        width,
        trailing_content: !content[width..].trim().is_empty(),
    })
}

fn codex_repository_targets(content: &str) -> Vec<String> {
    let Some(applies_to) = field_value(content, "applies_to:") else {
        return Vec::new();
    };
    let Some(cwd) = metadata_value(applies_to, "cwd=", &[';']) else {
        return Vec::new();
    };
    (!cwd.is_empty())
        .then(|| cwd.to_string())
        .into_iter()
        .collect()
}

fn codex_task_group_evidence(content: &str) -> Vec<EvidenceRef> {
    // Generated MEMORY.md indexes use memory-root-relative summary paths.
    let mut references = Vec::new();
    let mut in_rollout_summaries = false;
    for line in content.lines() {
        if line == "### rollout_summary_files" {
            in_rollout_summaries = true;
            continue;
        }
        if in_rollout_summaries && line.starts_with('#') {
            in_rollout_summaries = false;
            continue;
        }
        let Some(bullet) = in_rollout_summaries
            .then_some(line)
            .and_then(|line| line.strip_prefix("- "))
        else {
            continue;
        };
        if let Some(summary) = bullet
            .split_whitespace()
            .next()
            .filter(|value| value.starts_with("rollout_summaries/") && value.ends_with(".md"))
        {
            references.push(EvidenceRef {
                kind: EvidenceRefKind::File,
                value: summary.to_string(),
            });
        }
        if let Some(path) = metadata_value(bullet, "rollout_path=", &[',', ')']) {
            references.push(EvidenceRef {
                kind: EvidenceRefKind::File,
                value: path.to_string(),
            });
        }
        if let Some(thread) = metadata_value(bullet, "thread_id=", &[',', ')']) {
            references.push(EvidenceRef {
                kind: EvidenceRefKind::ProviderRecord,
                value: thread.to_string(),
            });
        }
    }
    references
}

fn codex_thread_evidence(content: &str) -> Vec<EvidenceRef> {
    let mut references = Vec::new();
    if let Some(path) = field_value(content, "rollout_path:").filter(|value| !value.is_empty()) {
        references.push(EvidenceRef {
            kind: EvidenceRefKind::File,
            value: path.to_string(),
        });
    }
    if let Some(summary) =
        field_value(content, "rollout_summary_file:").filter(|value| !value.is_empty())
    {
        // Generated indexes use memory-root-relative paths, while raw records use basenames.
        references.push(EvidenceRef {
            kind: EvidenceRefKind::File,
            value: if summary.starts_with("rollout_summaries/") {
                summary.to_string()
            } else {
                format!("rollout_summaries/{summary}")
            },
        });
    }
    references
}

fn field_value<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(key).map(str::trim))
}

fn metadata_value<'a>(content: &'a str, key: &str, terminators: &[char]) -> Option<&'a str> {
    let (index, _) = content.match_indices(key).find(|(index, _)| {
        *index == 0
            || content[..*index]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_alphanumeric() && character != '_')
    })?;
    let value = &content[index + key.len()..];
    let end = value
        .find(|character| terminators.contains(&character))
        .unwrap_or(value.len());
    Some(value[..end].trim())
}

fn limit_evidence_refs(
    references: Vec<EvidenceRef>,
    limit: usize,
    line: usize,
    extraction: &mut ItemExtraction,
) -> Vec<EvidenceRef> {
    let mut seen = BTreeSet::new();
    let mut retained = Vec::new();
    let mut limit_reached = false;
    for reference in references {
        if !seen.insert(reference.clone()) {
            continue;
        }
        if retained.len() == limit {
            limit_reached = true;
            break;
        }
        retained.push(reference);
    }
    if limit_reached {
        extraction.incomplete_issue(
            "memory-item-evidence-reference-limit",
            Some(line),
            format!("only the first {limit} evidence references were retained"),
        );
    }
    retained
}

#[derive(Default)]
struct ClaudeFrontmatter {
    name: Option<String>,
    native_kind: Option<String>,
    origin_session_id: Option<String>,
}

fn claude_frontmatter(raw: &str) -> std::result::Result<ClaudeFrontmatter, (&'static str, String)> {
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err((
            "claude-memory-frontmatter-missing",
            "Claude memory topic does not start with frontmatter".to_string(),
        ));
    }

    let mut metadata = ClaudeFrontmatter::default();
    let mut in_metadata = false;
    let mut closed = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            closed = true;
            break;
        }
        if trimmed.is_empty() {
            continue;
        }
        let indentation = line.len() - line.trim_start().len();
        if indentation == 0 {
            in_metadata = trimmed == "metadata:";
            if let Some(value) = trimmed.strip_prefix("name:") {
                metadata.name = yaml_scalar(value);
            }
            continue;
        }
        if !in_metadata {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("type:") {
            metadata.native_kind = yaml_scalar(value);
        } else if let Some(value) = trimmed.strip_prefix("originSessionId:") {
            metadata.origin_session_id = yaml_scalar(value);
        }
    }
    if !closed {
        return Err((
            "claude-memory-frontmatter-malformed",
            "Claude memory topic frontmatter has no closing delimiter".to_string(),
        ));
    }
    Ok(metadata)
}

fn yaml_scalar(value: &str) -> Option<String> {
    let value = strip_yaml_comment(value).trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('"') {
        return serde_json::from_str(value).ok();
    }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return Some(value[1..value.len() - 1].replace("''", "'"));
    }
    Some(value.to_string())
}

fn strip_yaml_comment(value: &str) -> &str {
    let opening_quote = value
        .char_indices()
        .find(|(_, character)| !character.is_whitespace())
        .map(|(index, _)| index);
    let mut characters = value.char_indices().peekable();
    let mut quote = None;
    let mut escaped = false;
    while let Some((index, character)) = characters.next() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some('"') if character == '"' => quote = None,
            Some('\'') if character == '\'' => {
                if characters.peek().is_some_and(|(_, next)| *next == '\'') {
                    characters.next();
                } else {
                    quote = None;
                }
            }
            Some(_) => {}
            None if matches!(character, '"' | '\'') && Some(index) == opening_quote => {
                quote = Some(character);
            }
            None if character == '#'
                && value[..index]
                    .chars()
                    .next_back()
                    .is_none_or(char::is_whitespace) =>
            {
                return value[..index].trim_end();
            }
            None => {}
        }
    }
    value
}

struct ItemSpec<'a> {
    role: ItemRole,
    granularity: ItemGranularity,
    range: SourceRange,
    content: &'a str,
    title: Option<String>,
    native_kind: Option<String>,
    repository_targets: Vec<String>,
    evidence: Vec<EvidenceRef>,
}

fn native_item(source: &Source, native_id: &str, spec: ItemSpec<'_>) -> MemoryItem {
    MemoryItem {
        id: format!("{}#native:{}", source.id, encode_id_component(native_id)),
        id_stability: IdStability::Native,
        provider: source.provider,
        source_id: source.id.clone(),
        source_path: source.path.clone(),
        role: spec.role,
        granularity: spec.granularity,
        scope: source.scope,
        association: source.association,
        range: spec.range,
        fingerprint: fingerprint(spec.content.as_bytes()),
        title: spec.title,
        native_kind: spec.native_kind,
        repository_targets: spec.repository_targets,
        evidence: spec.evidence,
        content: spec.content.to_string(),
    }
}

fn source_item(source: &Source, spec: ItemSpec<'_>) -> MemoryItem {
    MemoryItem {
        id: format!("{}#source", source.id),
        id_stability: IdStability::Source,
        provider: source.provider,
        source_id: source.id.clone(),
        source_path: source.path.clone(),
        role: spec.role,
        granularity: spec.granularity,
        scope: source.scope,
        association: source.association,
        range: spec.range,
        fingerprint: fingerprint(spec.content.as_bytes()),
        title: spec.title,
        native_kind: spec.native_kind,
        repository_targets: spec.repository_targets,
        evidence: spec.evidence,
        content: spec.content.to_string(),
    }
}

fn ephemeral_item(source: &Source, spec: ItemSpec<'_>, occurrence: usize) -> MemoryItem {
    let content_fingerprint = fingerprint(spec.content.as_bytes());
    let title = spec.title.as_deref().unwrap_or_default();
    let identity = fingerprint(
        format!(
            "{}\0{title}\0{content_fingerprint}\0{occurrence}",
            source.id
        )
        .as_bytes(),
    );
    MemoryItem {
        id: format!("{}#ephemeral:{identity}", source.id),
        id_stability: IdStability::Ephemeral,
        provider: source.provider,
        source_id: source.id.clone(),
        source_path: source.path.clone(),
        role: spec.role,
        granularity: spec.granularity,
        scope: source.scope,
        association: source.association,
        range: spec.range,
        fingerprint: content_fingerprint,
        title: spec.title,
        native_kind: spec.native_kind,
        repository_targets: spec.repository_targets,
        evidence: spec.evidence,
        content: spec.content.to_string(),
    }
}

fn next_occurrence(
    occurrences: &mut BTreeMap<(String, String), usize>,
    title: &str,
    content: &str,
) -> usize {
    let occurrence = occurrences
        .entry((title.to_string(), fingerprint(content.as_bytes())))
        .or_default();
    let current = *occurrence;
    *occurrence += 1;
    current
}

fn encode_id_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn fingerprint(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let mut encoded = String::with_capacity(7 + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{HumanDetail, LoadState, SourceRole, SourceSpec};
    use std::fs;

    const CODEX_MEMORY: &str = include_str!("../../tests/fixtures/memory-items/codex/MEMORY.md");
    const CODEX_RAW_MEMORIES: &str =
        include_str!("../../tests/fixtures/memory-items/codex/raw_memories.md");
    const CLAUDE_TOPIC: &str =
        include_str!("../../tests/fixtures/memory-items/claude/memory-inventory.md");
    const CLAUDE_INDEX: &str = include_str!("../../tests/fixtures/memory-items/claude/MEMORY.md");

    fn source(
        path: PathBuf,
        provider: Provider,
        role: SourceRole,
        kind: SourceKind,
        scope: Scope,
        association: Association,
    ) -> Source {
        Source::from_path(
            provider,
            path,
            SourceSpec {
                role,
                kind,
                scope,
                load_state: LoadState::Loaded,
                association,
                detail: None,
                human_detail: HumanDetail::Stored,
            },
        )
        .unwrap()
    }

    fn assert_item_ranges(raw: &str, items: &[MemoryItem]) {
        for item in items {
            assert_eq!(
                &raw[item.range.start_byte..item.range.end_byte],
                item.content
            );
            assert_eq!(
                item.content.lines().count(),
                item.range.end_line - item.range.start_line + 1
            );
        }
    }

    #[test]
    fn paired_provider_items_preserve_native_granularity_and_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let codex_path = dir.path().join("MEMORY.md");
        let claude_path = dir.path().join("memory-inventory.md");
        fs::write(&codex_path, CODEX_MEMORY).unwrap();
        fs::write(&claude_path, CLAUDE_TOPIC).unwrap();

        let codex = extract_source(&source(
            codex_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        let claude = extract_source(&source(
            claude_path,
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap();

        assert_eq!(codex.format, Some(ItemFormat::CodexTaskGroups));
        assert_eq!(codex.items.len(), 2);
        assert!(codex.complete);
        let midden = &codex.items[0];
        assert_eq!(midden.id_stability, IdStability::Ephemeral);
        assert_eq!(midden.granularity, ItemGranularity::Section);
        assert_eq!(midden.repository_targets, ["/workspace/midden"]);
        assert_eq!(midden.evidence.len(), 6);
        assert!(
            midden
                .evidence
                .iter()
                .any(|reference| reference.kind == EvidenceRefKind::ProviderRecord)
        );
        assert!(midden.evidence.contains(&EvidenceRef {
            kind: EvidenceRefKind::File,
            value: "rollout_summaries/2026-07-21-memory-inventory.md".to_string(),
        }));
        assert!(midden.evidence.contains(&EvidenceRef {
            kind: EvidenceRefKind::File,
            value: "rollout_summaries/2026-07-22-memory-evidence.md".to_string(),
        }));
        assert!(midden.fingerprint.starts_with("sha256:"));

        assert_eq!(claude.format, Some(ItemFormat::ClaudeTopic));
        assert_eq!(claude.items.len(), 1);
        assert!(claude.complete);
        let topic = &claude.items[0];
        assert_eq!(topic.id_stability, IdStability::Native);
        assert_eq!(topic.granularity, ItemGranularity::Source);
        assert_eq!(topic.native_kind.as_deref(), Some("project"));
        assert!(topic.id.ends_with("#native:memory-inventory"));
        assert_eq!(
            topic.evidence,
            [EvidenceRef {
                kind: EvidenceRefKind::ProviderRecord,
                value: "00000000-0000-4000-8000-000000000001".to_string(),
            }]
        );
        assert_item_ranges(CODEX_MEMORY, &codex.items);
        assert_item_ranges(CLAUDE_TOPIC, &claude.items);
    }

    #[test]
    fn claude_index_is_a_recognized_source_level_item() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        fs::write(&index_path, CLAUDE_INDEX).unwrap();
        let extraction = extract_source(&source(
            index_path,
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap();

        assert_eq!(extraction.format, Some(ItemFormat::ClaudeIndex));
        assert_eq!(extraction.items.len(), 1);
        assert_eq!(extraction.items[0].id_stability, IdStability::Source);
        assert!(extraction.items[0].id.ends_with("#source"));
        assert_eq!(
            extraction.items[0].native_kind.as_deref(),
            Some("memory-index")
        );
    }

    #[test]
    fn codex_thread_records_keep_native_ids_and_file_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("raw_memories.md");
        fs::write(&raw_path, CODEX_RAW_MEMORIES).unwrap();
        let extraction = extract_source(&source(
            raw_path,
            Provider::Codex,
            SourceRole::Evidence,
            SourceKind::EvidenceStore,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();

        assert_eq!(extraction.format, Some(ItemFormat::CodexThreads));
        assert_eq!(extraction.items.len(), 2);
        assert!(extraction.complete);
        let thread = &extraction.items[0];
        assert_eq!(thread.role, ItemRole::Evidence);
        assert_eq!(thread.id_stability, IdStability::Native);
        assert!(
            thread
                .id
                .ends_with("#native:00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(thread.repository_targets, ["/workspace/midden"]);
        assert_eq!(
            thread.evidence,
            [
                EvidenceRef {
                    kind: EvidenceRefKind::File,
                    value: "/codex/sessions/rollout-memory-inventory.jsonl".to_string(),
                },
                EvidenceRef {
                    kind: EvidenceRefKind::File,
                    value: "rollout_summaries/2026-07-21-memory-inventory.md".to_string(),
                },
            ]
        );
        assert_item_ranges(CODEX_RAW_MEMORIES, &extraction.items);
    }

    #[test]
    fn codex_generated_summary_is_one_source_level_item() {
        let dir = tempfile::tempdir().unwrap();
        let summary_path = dir.path().join("memory_summary.md");
        fs::write(
            &summary_path,
            "# Memory summary\n\n## Repository\n\n- Durable summary.\n",
        )
        .unwrap();
        let extraction = extract_source(&source(
            summary_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemorySummary,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();

        assert_eq!(extraction.format, Some(ItemFormat::CodexGeneratedSummary));
        assert_eq!(extraction.items.len(), 1);
        assert_eq!(extraction.items[0].granularity, ItemGranularity::Source);
        assert_eq!(
            extraction.items[0].native_kind.as_deref(),
            Some("generated-summary")
        );
        assert_eq!(extraction.items[0].id_stability, IdStability::Source);
        assert!(extraction.items[0].id.ends_with("#source"));
    }

    #[test]
    fn byte_limit_never_emits_a_partial_item() {
        let dir = tempfile::tempdir().unwrap();
        let claude_path = dir.path().join("topic.md");
        fs::write(&claude_path, CLAUDE_TOPIC).unwrap();
        let extraction = extract_source_with_limits(
            &source(
                claude_path,
                Provider::Claude,
                SourceRole::RetainedMemory,
                SourceKind::MemoryTopic,
                Scope::Repository,
                Association::Target,
            ),
            ReadLimits {
                max_source_bytes: 64,
                ..ReadLimits::default()
            },
        )
        .unwrap();

        assert_eq!(extraction.bytes_read, 64);
        assert!(extraction.items.is_empty());
        assert!(!extraction.complete);
        assert_eq!(extraction.issues[0].code, "memory-item-source-byte-limit");
    }

    #[test]
    fn section_extraction_keeps_only_records_completed_before_the_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let codex_path = dir.path().join("MEMORY.md");
        fs::write(&codex_path, CODEX_MEMORY).unwrap();
        let second = CODEX_MEMORY.find("# Task Group: Unrelated").unwrap();
        let extraction = extract_source_with_limits(
            &source(
                codex_path,
                Provider::Codex,
                SourceRole::RetainedMemory,
                SourceKind::MemoryIndex,
                Scope::Global,
                Association::Global,
            ),
            ReadLimits {
                max_source_bytes: second + 40,
                ..ReadLimits::default()
            },
        )
        .unwrap();

        assert_eq!(extraction.items.len(), 1);
        assert_eq!(
            extraction.items[0].title.as_deref(),
            Some("Midden dual-provider memory inventory")
        );
        assert!(
            extraction
                .issues
                .iter()
                .any(|issue| issue.code == "memory-item-source-byte-limit")
        );
    }

    #[test]
    fn truncation_mid_header_does_not_report_a_malformed_record() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        let content = "# Task Group: Complete\n\ncomplete body\n\n# Task Group: Truncated\n\ntruncated body\n";
        fs::write(&index_path, content).unwrap();
        let second = content.find("# Task Group: Truncated").unwrap();
        let extraction = extract_source_with_limits(
            &source(
                index_path,
                Provider::Codex,
                SourceRole::RetainedMemory,
                SourceKind::MemoryIndex,
                Scope::Global,
                Association::Global,
            ),
            ReadLimits {
                max_source_bytes: second + "# Task Group:".len(),
                ..ReadLimits::default()
            },
        )
        .unwrap();

        assert_eq!(extraction.items.len(), 1);
        assert_eq!(extraction.items[0].title.as_deref(), Some("Complete"));
        assert!(!extraction.complete);
        assert_eq!(
            extraction
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-source-byte-limit"]
        );
    }

    #[test]
    fn item_and_evidence_limits_are_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let codex_path = dir.path().join("MEMORY.md");
        fs::write(&codex_path, CODEX_MEMORY).unwrap();
        let extraction = extract_source_with_limits(
            &source(
                codex_path,
                Provider::Codex,
                SourceRole::RetainedMemory,
                SourceKind::MemoryIndex,
                Scope::Global,
                Association::Global,
            ),
            ReadLimits {
                max_items: 1,
                max_evidence_refs_per_item: 1,
                ..ReadLimits::default()
            },
        )
        .unwrap();

        assert_eq!(extraction.items.len(), 1);
        assert_eq!(extraction.items[0].evidence.len(), 1);
        assert_eq!(
            extraction
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "memory-item-count-limit",
                "memory-item-evidence-reference-limit",
            ])
        );
    }

    #[test]
    fn frontmatterless_claude_topic_uses_source_identity() {
        let dir = tempfile::tempdir().unwrap();
        let claude_path = dir.path().join("legacy-topic.md");
        fs::write(&claude_path, "Legacy topic without frontmatter.\n").unwrap();
        let extraction = extract_source(&source(
            claude_path,
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap();

        assert_eq!(extraction.items.len(), 1);
        assert_eq!(extraction.items[0].id_stability, IdStability::Source);
        assert!(extraction.items[0].id.ends_with("#source"));
        assert!(extraction.complete);
        assert_eq!(
            extraction
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "claude-memory-frontmatter-missing",
                "memory-item-native-id-missing",
            ])
        );
    }

    #[test]
    fn malformed_claude_frontmatter_reports_incomplete_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let claude_path = dir.path().join("malformed-topic.md");
        fs::write(
            &claude_path,
            "---\nname: malformed-topic\nmetadata:\n  type: project\n\
             originSessionId: SESSION-1\n\nTopic without a closing delimiter.\n",
        )
        .unwrap();
        let extraction = extract_source(&source(
            claude_path,
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap();

        assert_eq!(extraction.items.len(), 1);
        assert!(extraction.items[0].evidence.is_empty());
        assert!(!extraction.complete);
        assert!(
            extraction
                .issues
                .iter()
                .any(|issue| issue.code == "claude-memory-frontmatter-malformed")
        );
    }

    #[test]
    fn native_ids_survive_content_changes_while_ephemeral_ids_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let claude_path = dir.path().join("memory-inventory.md");
        fs::write(&claude_path, CLAUDE_TOPIC).unwrap();
        let original_claude = extract_source(&source(
            claude_path.clone(),
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap()
        .items
        .remove(0);
        fs::write(
            &claude_path,
            CLAUDE_TOPIC.replace(
                "paired Codex and Claude coverage",
                "bounded provider coverage",
            ),
        )
        .unwrap();
        let changed_claude = extract_source(&source(
            claude_path,
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap()
        .items
        .remove(0);
        assert_eq!(original_claude.id, changed_claude.id);
        assert_ne!(original_claude.fingerprint, changed_claude.fingerprint);

        let codex_path = dir.path().join("MEMORY.md");
        fs::write(&codex_path, CODEX_MEMORY).unwrap();
        let original_codex = extract_source(&source(
            codex_path.clone(),
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap()
        .items
        .remove(0);
        fs::write(
            &codex_path,
            CODEX_MEMORY.replace(
                "paired Codex and Claude coverage",
                "bounded provider coverage",
            ),
        )
        .unwrap();
        let changed_codex = extract_source(&source(
            codex_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap()
        .items
        .remove(0);
        assert_ne!(original_codex.id, changed_codex.id);
        assert_ne!(original_codex.fingerprint, changed_codex.fingerprint);
    }

    #[test]
    fn source_ids_survive_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let summary_path = dir.path().join("memory_summary.md");
        fs::write(&summary_path, "# Summary\n\nOriginal.\n").unwrap();
        let original = extract_source(&source(
            summary_path.clone(),
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemorySummary,
            Scope::Global,
            Association::Global,
        ))
        .unwrap()
        .items
        .remove(0);
        fs::write(&summary_path, "# Summary\n\nChanged.\n").unwrap();
        let changed = extract_source(&source(
            summary_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemorySummary,
            Scope::Global,
            Association::Global,
        ))
        .unwrap()
        .items
        .remove(0);

        assert_eq!(original.id_stability, IdStability::Source);
        assert_eq!(original.id, changed.id);
        assert_ne!(original.fingerprint, changed.fingerprint);

        let legacy_path = dir.path().join("legacy-topic.md");
        fs::write(&legacy_path, "Original legacy topic.\n").unwrap();
        let original_legacy = extract_source(&source(
            legacy_path.clone(),
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap()
        .items
        .remove(0);
        fs::write(&legacy_path, "Changed legacy topic.\n").unwrap();
        let changed_legacy = extract_source(&source(
            legacy_path,
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap()
        .items
        .remove(0);

        assert_eq!(original_legacy.id_stability, IdStability::Source);
        assert_eq!(original_legacy.id, changed_legacy.id);
        assert_ne!(original_legacy.fingerprint, changed_legacy.fingerprint);
    }

    #[test]
    fn unrelated_prefix_edits_do_not_reidentify_later_ephemeral_items() {
        let dir = tempfile::tempdir().unwrap();
        let codex_path = dir.path().join("MEMORY.md");
        fs::write(&codex_path, CODEX_MEMORY).unwrap();
        let original = extract_source(&source(
            codex_path.clone(),
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        fs::write(
            &codex_path,
            CODEX_MEMORY.replacen("scope: Inventory", "\nscope: Inventory", 1),
        )
        .unwrap();
        let shifted = extract_source(&source(
            codex_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();

        assert_ne!(original.items[0].id, shifted.items[0].id);
        assert_eq!(original.items[1].content, shifted.items[1].content);
        assert_eq!(original.items[1].id, shifted.items[1].id);
    }

    #[test]
    fn unparsed_record_headers_are_boundaries_and_report_incomplete_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("raw_memories.md");
        fs::write(
            &raw_path,
            "## Thread `ID-1`\ncwd: /one\nrollout_path: /r/one.jsonl\n\
             ## Thread `ID-2` (resumed)\ncwd: /two\nrollout_path: /r/two.jsonl\n",
        )
        .unwrap();
        let threads = extract_source(&source(
            raw_path,
            Provider::Codex,
            SourceRole::Evidence,
            SourceKind::EvidenceStore,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert_eq!(threads.items.len(), 1);
        assert!(!threads.items[0].content.contains("ID-2"));
        assert!(!threads.complete);
        assert!(
            threads
                .issues
                .iter()
                .any(|issue| issue.code == "memory-item-record-header-unparsed")
        );

        let index_path = dir.path().join("MEMORY.md");
        fs::write(
            &index_path,
            "# Task Group: Good\nscope: good\n# Task Group:Malformed\nscope: bad\n",
        )
        .unwrap();
        let groups = extract_source(&source(
            index_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert_eq!(groups.items.len(), 1);
        assert!(!groups.items[0].content.contains("Malformed"));
        assert!(!groups.complete);
    }

    #[test]
    fn metadata_keys_must_start_at_a_token_boundary() {
        assert_eq!(
            metadata_value(
                "a.md (cwd=/w, parent_thread_id=PARENT, thread_id=REAL)",
                "thread_id=",
                &[',', ')'],
            ),
            Some("REAL")
        );
        assert_eq!(
            metadata_value("reuse_rule=set old_cwd=/tmp; cwd=/w/repo", "cwd=", &[';'],),
            Some("/w/repo")
        );
    }

    #[test]
    fn yaml_comments_follow_unquoted_apostrophes() {
        assert_eq!(
            yaml_scalar(" don't-do-this # explanatory comment"),
            Some("don't-do-this".to_string())
        );
        assert_eq!(
            yaml_scalar(" 'quoted # value' # explanatory comment"),
            Some("quoted # value".to_string())
        );
    }

    #[test]
    fn fenced_headers_do_not_split_codex_sections() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        fs::write(
            &index_path,
            "# Task Group: Real\n\n```markdown\n# Task Group: Not a record\n```\n\
             still real\n\n# Task Group: Second\n\nsecond body\n",
        )
        .unwrap();
        let extraction = extract_source(&source(
            index_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();

        assert_eq!(extraction.items.len(), 2);
        assert_eq!(extraction.items[0].title.as_deref(), Some("Real"));
        assert!(extraction.items[0].content.contains("Not a record"));
        assert_eq!(extraction.items[1].title.as_deref(), Some("Second"));
        assert!(!extraction.complete);
        assert_eq!(
            extraction
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-fenced-header-suppressed"]
        );
    }

    #[test]
    fn unclosed_fences_do_not_emit_cross_record_items() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        fs::write(
            &index_path,
            "# Task Group: A\napplies_to: cwd=/w/a; reuse_rule=test.\n```rust\n\
             let x = 1;\n# Task Group: B\napplies_to: cwd=/w/b; reuse_rule=test.\n",
        )
        .unwrap();
        let groups = extract_source(&source(
            index_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert!(groups.items.is_empty());
        assert!(!groups.complete);
        assert_eq!(
            groups
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-unclosed-code-fence"]
        );

        let raw_path = dir.path().join("raw_memories.md");
        fs::write(
            &raw_path,
            "## Thread `T1`\ncwd: /w/one\n   ~~~\nsome code\n## Thread `T2`\ncwd: /w/two\n",
        )
        .unwrap();
        let threads = extract_source(&source(
            raw_path,
            Provider::Codex,
            SourceRole::Evidence,
            SourceKind::EvidenceStore,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert!(threads.items.is_empty());
        assert!(!threads.complete);
        assert_eq!(
            threads
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-unclosed-code-fence"]
        );

        let preamble_path = dir.path().join("preamble.md");
        fs::write(
            &preamble_path,
            "```markdown\npreamble example\n# Task Group: Not a record\n",
        )
        .unwrap();
        let preamble = extract_source(&source(
            preamble_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert!(preamble.items.is_empty());
        assert!(!preamble.complete);
        assert_eq!(
            preamble
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-unclosed-code-fence"]
        );
    }

    #[test]
    fn fenced_record_headers_are_reported_as_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        fs::write(
            &index_path,
            "# Task Group: A\nscope: a\n```rust\ncode\n# Task Group: B\nscope: hidden\n```\n\
             # Task Group: C\nscope: c\n",
        )
        .unwrap();
        let groups = extract_source(&source(
            index_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert_eq!(
            groups
                .items
                .iter()
                .filter_map(|item| item.title.as_deref())
                .collect::<Vec<_>>(),
            ["A", "C"]
        );
        assert!(!groups.complete);
        assert_eq!(groups.issues.len(), 1);
        assert_eq!(
            groups.issues[0].code,
            "memory-item-fenced-header-suppressed"
        );
        assert_eq!(groups.issues[0].line, Some(5));

        let raw_path = dir.path().join("raw_memories.md");
        fs::write(
            &raw_path,
            "## Thread `T1`\ncwd: /w/one\n```\ncode\n## Thread `T2`\ncwd: /w/two\n```\n\
             ## Thread `T3`\ncwd: /w/three\n",
        )
        .unwrap();
        let threads = extract_source(&source(
            raw_path,
            Provider::Codex,
            SourceRole::Evidence,
            SourceKind::EvidenceStore,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert_eq!(threads.items.len(), 2);
        assert!(!threads.complete);
        assert_eq!(threads.issues.len(), 1);
        assert_eq!(
            threads.issues[0].code,
            "memory-item-fenced-header-suppressed"
        );
        assert_eq!(threads.issues[0].line, Some(5));

        let preamble_path = dir.path().join("balanced-preamble.md");
        fs::write(
            &preamble_path,
            "```markdown\n# Task Group: Example only\n```\n",
        )
        .unwrap();
        let preamble = extract_source(&source(
            preamble_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert!(preamble.items.is_empty());
        assert!(!preamble.complete);
        assert_eq!(
            preamble
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            [
                "memory-item-fenced-header-suppressed",
                "codex-memory-task-group-format-unrecognized",
            ]
        );
    }

    #[test]
    fn zero_item_limit_does_not_misclassify_a_recognized_layout() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        fs::write(&index_path, CODEX_MEMORY).unwrap();
        let extraction = extract_source_with_limits(
            &source(
                index_path,
                Provider::Codex,
                SourceRole::RetainedMemory,
                SourceKind::MemoryIndex,
                Scope::Global,
                Association::Global,
            ),
            ReadLimits {
                max_items: 0,
                ..ReadLimits::default()
            },
        )
        .unwrap();

        assert!(extraction.items.is_empty());
        assert!(!extraction.complete);
        assert_eq!(
            extraction
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-count-limit"]
        );
    }

    #[test]
    fn native_ids_are_namespaced_and_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let topic_path = dir.path().join("special.md");
        fs::write(
            &topic_path,
            "---\nname: 'ephemeral:dead#beef' # legal inline comment\nmetadata:\n\
             type: project\n---\n\nRetained content.\n",
        )
        .unwrap();
        let extraction = extract_source(&source(
            topic_path,
            Provider::Claude,
            SourceRole::RetainedMemory,
            SourceKind::MemoryTopic,
            Scope::Repository,
            Association::Target,
        ))
        .unwrap();

        assert_eq!(extraction.items[0].id_stability, IdStability::Native);
        assert!(
            extraction.items[0]
                .id
                .ends_with("#native:ephemeral%3Adead%23beef")
        );
        assert!(extraction.complete);
        assert!(extraction.issues.is_empty());
    }

    #[test]
    fn unreadable_recognized_sources_report_incomplete_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        fs::write(&index_path, CODEX_MEMORY).unwrap();
        let source = source(
            index_path.clone(),
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        );
        fs::remove_file(index_path).unwrap();

        let extraction = extract_source(&source).unwrap();

        assert_eq!(extraction.format, Some(ItemFormat::CodexTaskGroups));
        assert!(extraction.items.is_empty());
        assert!(!extraction.complete);
        assert_eq!(extraction.issues[0].code, "memory-item-source-unreadable");
    }

    #[test]
    fn utf8_boundary_backoff_reports_the_bytes_actually_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let summary_path = dir.path().join("memory_summary.md");
        let content = "# Summary\n💾 retained\n";
        fs::write(&summary_path, content).unwrap();
        let emoji = content.find('💾').unwrap();
        let extraction = extract_source_with_limits(
            &source(
                summary_path,
                Provider::Codex,
                SourceRole::RetainedMemory,
                SourceKind::MemorySummary,
                Scope::Global,
                Association::Global,
            ),
            ReadLimits {
                max_source_bytes: emoji + 1,
                ..ReadLimits::default()
            },
        )
        .unwrap();

        assert_eq!(extraction.bytes_read, emoji);
        assert!(extraction.items.is_empty());
        assert!(!extraction.complete);
        assert_eq!(
            extraction
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-source-byte-limit"]
        );
    }

    #[test]
    fn source_and_thread_truncation_never_emit_partial_items() {
        let dir = tempfile::tempdir().unwrap();
        let summary_path = dir.path().join("memory_summary.md");
        fs::write(&summary_path, "# Summary\n\nRetained content.\n").unwrap();
        let summary = extract_source_with_limits(
            &source(
                summary_path,
                Provider::Codex,
                SourceRole::RetainedMemory,
                SourceKind::MemorySummary,
                Scope::Global,
                Association::Global,
            ),
            ReadLimits {
                max_source_bytes: 12,
                ..ReadLimits::default()
            },
        )
        .unwrap();
        assert!(summary.items.is_empty());
        assert!(!summary.complete);

        let raw_path = dir.path().join("raw_memories.md");
        fs::write(&raw_path, CODEX_RAW_MEMORIES).unwrap();
        let second = CODEX_RAW_MEMORIES
            .find("## Thread `00000000-0000-4000-8000-000000000002`")
            .unwrap();
        let threads = extract_source_with_limits(
            &source(
                raw_path,
                Provider::Codex,
                SourceRole::Evidence,
                SourceKind::EvidenceStore,
                Scope::Global,
                Association::Global,
            ),
            ReadLimits {
                max_source_bytes: second + 40,
                ..ReadLimits::default()
            },
        )
        .unwrap();
        assert_eq!(threads.items.len(), 1);
        assert!(!threads.complete);
        assert!(!threads.items[0].content.contains("000000000002"));
    }

    #[test]
    fn empty_source_level_memory_does_not_create_an_item() {
        let dir = tempfile::tempdir().unwrap();
        let summary_path = dir.path().join("memory_summary.md");
        fs::write(&summary_path, "").unwrap();
        let extraction = extract_source(&source(
            summary_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemorySummary,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();

        assert!(extraction.items.is_empty());
        assert!(extraction.complete);
        assert!(extraction.issues.is_empty());
    }

    #[test]
    fn junk_rollout_bullets_are_ignored() {
        let references = codex_task_group_evidence(
            "### rollout_summary_files\n\n- none recorded for this task\n\
             - rollout_summaries/real.md (rollout_path=/sessions/real.jsonl, thread_id=REAL)\n",
        );

        assert_eq!(
            references,
            [
                EvidenceRef {
                    kind: EvidenceRefKind::File,
                    value: "rollout_summaries/real.md".to_string(),
                },
                EvidenceRef {
                    kind: EvidenceRefKind::File,
                    value: "/sessions/real.jsonl".to_string(),
                },
                EvidenceRef {
                    kind: EvidenceRefKind::ProviderRecord,
                    value: "REAL".to_string(),
                },
            ]
        );
    }

    #[test]
    fn unrecognized_codex_layout_and_duplicate_native_ids_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("MEMORY.md");
        fs::write(&index_path, "# Durable entries\n").unwrap();
        let index = extract_source(&source(
            index_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert!(index.items.is_empty());
        assert_eq!(
            index.issues[0].code,
            "codex-memory-task-group-format-unrecognized"
        );

        let raw_path = dir.path().join("raw_memories.md");
        let duplicate = CODEX_RAW_MEMORIES.replace(
            "00000000-0000-4000-8000-000000000002",
            "00000000-0000-4000-8000-000000000001",
        );
        fs::write(&raw_path, duplicate).unwrap();
        let evidence = extract_source(&source(
            raw_path,
            Provider::Codex,
            SourceRole::Evidence,
            SourceKind::EvidenceStore,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();
        assert_eq!(evidence.items.len(), 2);
        assert_eq!(evidence.items[0].id_stability, IdStability::Native);
        assert_eq!(evidence.items[1].id_stability, IdStability::Ephemeral);
        assert!(evidence.complete);
        assert!(
            evidence
                .issues
                .iter()
                .any(|issue| issue.code == "memory-item-duplicate-native-id")
        );
    }

    #[test]
    fn codex_raw_memory_limits_and_unrecognized_layout_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("raw_memories.md");
        let raw_source = || {
            source(
                raw_path.clone(),
                Provider::Codex,
                SourceRole::Evidence,
                SourceKind::EvidenceStore,
                Scope::Global,
                Association::Global,
            )
        };

        fs::write(&raw_path, CODEX_RAW_MEMORIES).unwrap();
        let limited = extract_source_with_limits(
            &raw_source(),
            ReadLimits {
                max_items: 0,
                ..ReadLimits::default()
            },
        )
        .unwrap();
        assert!(limited.items.is_empty());
        assert!(!limited.complete);
        assert_eq!(
            limited
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["memory-item-count-limit"]
        );

        fs::write(&raw_path, "# Evidence without thread records\n").unwrap();
        let unrecognized = extract_source(&raw_source()).unwrap();
        assert!(unrecognized.items.is_empty());
        assert!(!unrecognized.complete);
        assert_eq!(
            unrecognized
                .issues
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["codex-memory-thread-format-unrecognized"]
        );
    }

    #[test]
    fn unsupported_sources_are_not_read_or_silently_reclassified() {
        let source = Source {
            id: "claude:/missing/unknown.bin".to_string(),
            provider: Provider::Claude,
            role: SourceRole::Unknown,
            kind: SourceKind::Unknown,
            scope: Scope::Unknown,
            load_state: LoadState::Unknown,
            association: Association::Unknown,
            path: PathBuf::from("/missing/unknown.bin"),
            bytes: None,
            inventory_order: 0,
            detail: None,
            human_detail: HumanDetail::Stored,
        };
        let extraction = extract_source(&source).unwrap();

        assert_eq!(extraction.format, None);
        assert_eq!(extraction.bytes_read, 0);
        assert!(extraction.items.is_empty());
        assert!(extraction.complete);
    }

    #[test]
    fn recognized_non_utf8_sources_report_an_issue() {
        let dir = tempfile::tempdir().unwrap();
        let codex_path = dir.path().join("MEMORY.md");
        fs::write(&codex_path, b"# Task Group: valid\n\xff\n").unwrap();
        let extraction = extract_source(&source(
            codex_path,
            Provider::Codex,
            SourceRole::RetainedMemory,
            SourceKind::MemoryIndex,
            Scope::Global,
            Association::Global,
        ))
        .unwrap();

        assert!(extraction.items.is_empty());
        assert!(!extraction.complete);
        assert_eq!(extraction.issues[0].code, "memory-item-source-invalid-utf8");
    }
}
