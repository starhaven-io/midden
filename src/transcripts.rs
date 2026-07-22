use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::orphans;
use crate::paths::WORKTREE_MARKER;

const MAX_CWD_SCAN_LINES: usize = 64;
const MAX_JSONL_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirStatus {
    Kept,
    Dead,
    Skipped,
}

impl DirStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::Dead => "dead",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleanup {
    None,
    WouldRemoveDir,
    RemovedDir,
    MemoryPreserved,
    PartiallyCleaned,
}

impl Cleanup {
    fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WouldRemoveDir => "would-remove-dir",
            Self::RemovedDir => "removed-dir",
            Self::MemoryPreserved => "memory-preserved",
            Self::PartiallyCleaned => "partially-cleaned",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DirReport {
    pub path: PathBuf,
    pub derived_cwd: Option<String>,
    pub status: DirStatus,
    pub reason: Option<&'static str>,
    pub storage_bytes: u64,
    pub bytes: u64,
    pub delete: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub memory_preserved: bool,
    pub cleanup: Cleanup,
}

impl DirReport {
    pub fn is_dead(&self) -> bool {
        self.status == DirStatus::Dead
    }

    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path.display().to_string(),
            "derived_cwd": self.derived_cwd,
            "status": self.status.as_str(),
            "reason": self.reason,
            "storage_bytes": self.storage_bytes,
            "bytes": self.bytes,
            "delete": self.delete.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "deleted": self.deleted.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "memory_preserved": self.memory_preserved,
            "cleanup": self.cleanup.as_str(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub projects_dir: PathBuf,
    pub dirs: Vec<DirReport>,
    pub applied: bool,
}

impl Report {
    pub fn total(&self) -> usize {
        self.dirs.len()
    }

    pub fn dead_count(&self) -> usize {
        self.dirs
            .iter()
            .filter(|d| d.status == DirStatus::Dead)
            .count()
    }

    pub fn kept_count(&self) -> usize {
        self.dirs
            .iter()
            .filter(|d| d.status == DirStatus::Kept)
            .count()
    }

    pub fn skipped_count(&self) -> usize {
        self.dirs
            .iter()
            .filter(|d| d.status == DirStatus::Skipped)
            .count()
    }

    pub fn resolvable_count(&self) -> usize {
        self.dead_count() + self.kept_count()
    }

    pub fn bytes(&self) -> u64 {
        self.dirs.iter().map(|d| d.bytes).sum()
    }

    pub fn storage_bytes(&self) -> u64 {
        self.dirs.iter().map(|d| d.storage_bytes).sum()
    }

    pub fn kept_storage_bytes(&self) -> u64 {
        self.dirs
            .iter()
            .filter(|d| d.status == DirStatus::Kept)
            .map(|d| d.storage_bytes)
            .sum()
    }

    pub fn top_kept_by_storage(&self, limit: usize) -> Vec<&DirReport> {
        let mut dirs = self
            .dirs
            .iter()
            .filter(|d| d.status == DirStatus::Kept && d.storage_bytes > 0)
            .collect::<Vec<_>>();
        dirs.sort_by(|a, b| {
            b.storage_bytes
                .cmp(&a.storage_bytes)
                .then(a.path.cmp(&b.path))
        });
        dirs.truncate(limit);
        dirs
    }

    pub fn to_json(&self) -> Value {
        json!({
            "projects_dir": self.projects_dir.display().to_string(),
            "total": self.total(),
            "resolvable": self.resolvable_count(),
            "kept": self.kept_count(),
            "dead": self.dead_count(),
            "skipped": self.skipped_count(),
            "storage_bytes": self.storage_bytes(),
            "bytes": self.bytes(),
            "applied": self.applied,
            "dirs": self.dirs.iter().map(DirReport::to_json).collect::<Vec<_>>(),
        })
    }
}

pub fn discover(claude_home: &Path, worktrees_only: bool) -> Result<Report> {
    let projects_dir = claude_home.join("projects");
    let mut dirs = Vec::new();

    if !projects_dir.exists() {
        return Ok(Report {
            projects_dir,
            dirs,
            applied: false,
        });
    }

    let mut entries = fs::read_dir(&projects_dir)
        .with_context(|| format!("read {}", projects_dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read {}", projects_dir.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                dirs.push(skipped(&path, "inaccessible"));
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let report = inspect_dir(&path).unwrap_or_else(|_| skipped(&path, "inaccessible"));
        if worktrees_only && !matches_worktree_filter(&report) {
            continue;
        }
        dirs.push(report);
    }

    Ok(Report {
        projects_dir,
        dirs,
        applied: false,
    })
}

pub fn delete_dead(mut report: Report) -> Result<Report> {
    for dir in &mut report.dirs {
        if !dir.is_dead() {
            continue;
        }

        for target in dir.delete.clone() {
            remove_artifact(&target).with_context(|| format!("delete {}", target.display()))?;
            dir.deleted.push(target);
        }

        dir.memory_preserved = has_memory_dir(&dir.path);
        dir.cleanup = cleanup_after_delete(&dir.path, &dir.delete)?;
    }
    report.applied = true;
    Ok(report)
}

pub(crate) fn project_cwds(path: &Path, limit: usize) -> Result<(Vec<PathBuf>, bool)> {
    let entries = fs::read_dir(path)
        .with_context(|| format!("read {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read {}", path.display()))?;
    let mut jsonl_files = entries
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        })
        .collect::<Vec<_>>();
    jsonl_files.sort();
    let truncated = jsonl_files.len() > limit;
    jsonl_files.truncate(limit);
    let mut cwds = BTreeSet::new();
    for jsonl in &jsonl_files {
        if let Some(cwd) = cwd_from_jsonl(jsonl)? {
            cwds.insert(PathBuf::from(cwd));
        }
    }
    Ok((cwds.into_iter().collect(), truncated))
}

fn inspect_dir(path: &Path) -> Result<DirReport> {
    let scan = scan_dir(path)?;
    let storage_bytes = scan.storage_bytes()?;
    if scan.jsonl_files.is_empty() {
        return Ok(skipped_with_storage(path, "no-jsonl", storage_bytes));
    }

    let mut cwds = BTreeSet::new();
    for jsonl in &scan.jsonl_files {
        match cwd_from_jsonl(jsonl) {
            Ok(Some(cwd)) => {
                cwds.insert(cwd);
            }
            Ok(None) => {}
            Err(_) => {
                return Ok(skipped_with_storage(
                    path,
                    "inaccessible-jsonl",
                    storage_bytes,
                ));
            }
        }
    }

    let Some(cwd) = cwds.iter().next().cloned() else {
        return Ok(skipped_with_storage(path, "no-cwd", storage_bytes));
    };
    if cwds.len() > 1 {
        return Ok(skipped_with_storage(
            path,
            "cwd-disagreement",
            storage_bytes,
        ));
    }

    let status = if orphans::provably_absent(Path::new(&cwd)) {
        DirStatus::Dead
    } else {
        DirStatus::Kept
    };

    let (delete, bytes, memory_preserved, cleanup) = if status == DirStatus::Dead {
        let delete = scan.delete_paths();
        let bytes = scan.delete_bytes()?;
        let memory_preserved = scan.memory_preserved;
        let cleanup = cleanup_for_remaining(&scan.remaining, false)?;
        (delete, bytes, memory_preserved, cleanup)
    } else {
        (Vec::new(), 0, false, Cleanup::None)
    };

    Ok(DirReport {
        path: path.to_path_buf(),
        derived_cwd: Some(cwd),
        status,
        reason: None,
        storage_bytes,
        bytes,
        delete,
        deleted: Vec::new(),
        memory_preserved,
        cleanup,
    })
}

fn skipped(path: &Path, reason: &'static str) -> DirReport {
    skipped_with_storage(path, reason, 0)
}

fn skipped_with_storage(path: &Path, reason: &'static str, storage_bytes: u64) -> DirReport {
    DirReport {
        path: path.to_path_buf(),
        derived_cwd: None,
        status: DirStatus::Skipped,
        reason: Some(reason),
        storage_bytes,
        bytes: 0,
        delete: Vec::new(),
        deleted: Vec::new(),
        memory_preserved: false,
        cleanup: Cleanup::None,
    }
}

fn matches_worktree_filter(report: &DirReport) -> bool {
    match report.derived_cwd.as_deref() {
        Some(cwd) => cwd.contains(WORKTREE_MARKER),
        None => report.status == DirStatus::Skipped,
    }
}

struct DirScan {
    jsonl_files: Vec<PathBuf>,
    delete: Vec<DeleteArtifact>,
    remaining: Vec<PathBuf>,
    memory_preserved: bool,
}

impl DirScan {
    fn delete_paths(&self) -> Vec<PathBuf> {
        self.delete
            .iter()
            .map(|artifact| artifact.path.clone())
            .collect()
    }

    fn delete_bytes(&self) -> Result<u64> {
        self.delete.iter().try_fold(0_u64, |sum, artifact| {
            artifact.size().map(|bytes| sum.saturating_add(bytes))
        })
    }

    fn storage_bytes(&self) -> Result<u64> {
        let delete_bytes = self.delete_bytes()?;
        self.remaining.iter().try_fold(delete_bytes, |sum, path| {
            dir_size(path).map(|bytes| sum.saturating_add(bytes))
        })
    }
}

struct DeleteArtifact {
    path: PathBuf,
    file_size: Option<u64>,
}

impl DeleteArtifact {
    fn size(&self) -> Result<u64> {
        match self.file_size {
            Some(size) => Ok(size),
            None => dir_size(&self.path),
        }
    }
}

fn scan_dir(path: &Path) -> Result<DirScan> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read {}", path.display()))?;
    entries.sort_by_key(|e| e.path());

    let mut jsonl_files = Vec::new();
    let mut delete = Vec::new();
    let mut remaining = Vec::new();
    let mut memory_preserved = false;

    for entry in entries {
        let artifact = entry.path();
        let meta = fs::symlink_metadata(&artifact)
            .with_context(|| format!("stat {}", artifact.display()))?;
        let file_type = meta.file_type();
        let is_jsonl = artifact.extension().and_then(|e| e.to_str()) == Some("jsonl");
        let is_uuid_dir = file_type.is_dir()
            && artifact
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(looks_like_uuid);

        if is_jsonl && !file_type.is_dir() {
            jsonl_files.push(artifact.clone());
        }

        if (is_jsonl && !file_type.is_dir()) || is_uuid_dir {
            delete.push(DeleteArtifact {
                path: artifact,
                file_size: (!file_type.is_dir()).then_some(meta.len()),
            });
        } else {
            if file_type.is_dir()
                && artifact
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name == "memory")
            {
                memory_preserved = true;
            }
            remaining.push(artifact);
        }
    }

    Ok(DirScan {
        jsonl_files,
        delete,
        remaining,
        memory_preserved,
    })
}

fn cleanup_after_delete(path: &Path, artifacts: &[PathBuf]) -> Result<Cleanup> {
    let remaining = remaining_after_artifacts(path, artifacts)?;
    if remaining.is_empty() {
        match fs::remove_dir(path) {
            Ok(()) => return Ok(Cleanup::RemovedDir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Cleanup::RemovedDir),
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
    }
    cleanup_for_remaining(&remaining, true)
}

fn remaining_after_artifacts(path: &Path, artifacts: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let artifact_names = artifacts
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_owned()))
        .collect::<BTreeSet<_>>();
    let mut remaining = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let entry = entry.with_context(|| format!("read {}", path.display()))?;
        if !artifact_names.contains(&entry.file_name()) {
            remaining.push(entry.path());
        }
    }
    remaining.sort();
    Ok(remaining)
}

fn cleanup_for_remaining(remaining: &[PathBuf], applied: bool) -> Result<Cleanup> {
    if remaining.is_empty() {
        return Ok(if applied {
            Cleanup::RemovedDir
        } else {
            Cleanup::WouldRemoveDir
        });
    }
    if remaining.len() == 1
        && remaining[0]
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == "memory")
    {
        return Ok(Cleanup::MemoryPreserved);
    }
    Ok(Cleanup::PartiallyCleaned)
}

fn has_memory_dir(path: &Path) -> bool {
    fs::symlink_metadata(path.join("memory"))
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false)
}

fn remove_artifact(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn dir_size(path: &Path) -> Result<u64> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.file_type().is_dir() {
        return Ok(meta.len());
    }

    let mut total = 0_u64;
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let entry = entry.with_context(|| format!("read {}", path.display()))?;
        total = total.saturating_add(dir_size(&entry.path())?);
    }
    Ok(total)
}

fn cwd_from_jsonl(path: &Path) -> Result<Option<String>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    cwd_from_reader(file)
}

fn cwd_from_reader(reader: impl Read) -> Result<Option<String>> {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();

    for _ in 0..MAX_CWD_SCAN_LINES {
        line.clear();
        match read_line_capped(&mut reader, &mut line)? {
            LineRead::Eof => break,
            LineRead::TooLong => return Ok(None),
            LineRead::Line => {
                if let Some(cwd) = cwd_from_line(&line) {
                    return Ok(Some(cwd));
                }
            }
        }
    }

    Ok(None)
}

enum LineRead {
    Eof,
    Line,
    TooLong,
}

fn read_line_capped(reader: &mut impl BufRead, out: &mut Vec<u8>) -> std::io::Result<LineRead> {
    let mut read_any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if read_any {
                LineRead::Line
            } else {
                LineRead::Eof
            });
        }

        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |pos| pos + 1);

        if out.len().saturating_add(take) > MAX_JSONL_LINE_BYTES {
            return Ok(LineRead::TooLong);
        }

        out.extend_from_slice(&available[..take]);
        reader.consume(take);
        read_any = true;

        if out.last() == Some(&b'\n') {
            return Ok(LineRead::Line);
        }
    }
}

fn cwd_from_line(line: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(line).ok()?;
    value.get("cwd")?.as_str().map(ToOwned::to_owned)
}

fn looks_like_uuid(name: &str) -> bool {
    fn hex(s: &str) -> bool {
        s.bytes().all(|b| b.is_ascii_hexdigit())
    }

    let parts = name.split('-').collect::<Vec<_>>();
    if parts.len() == 5 {
        let lens = [8, 4, 4, 4, 12];
        return parts
            .iter()
            .zip(lens)
            .all(|(part, len)| part.len() == len && hex(part));
    }

    name.len() == 32 && hex(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn cwd_extraction_reads_first_line_with_cwd() {
        let data = br#"{"cwd":"/tmp/project","type":"summary"}
{"cwd":"/tmp/other"}
"#;
        assert_eq!(
            cwd_from_reader(Cursor::new(data)).unwrap(),
            Some("/tmp/project".to_string())
        );
    }

    #[test]
    fn cwd_extraction_skips_malformed_lines() {
        let data = br#"not json
{"message":"no cwd"}
{"cwd":"/tmp/project"}
"#;
        assert_eq!(
            cwd_from_reader(Cursor::new(data)).unwrap(),
            Some("/tmp/project".to_string())
        );
    }

    #[test]
    fn cwd_extraction_caps_huge_lines() {
        let mut data = vec![b'x'; MAX_JSONL_LINE_BYTES + 10];
        data.extend_from_slice(
            br#"
{"cwd":"/tmp/project"}
"#,
        );

        assert_eq!(cwd_from_reader(Cursor::new(data)).unwrap(), None);
    }

    #[test]
    fn uuid_detection_accepts_canonical_and_plain_forms() {
        assert!(looks_like_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(looks_like_uuid("123e4567e89b12d3a456426614174000"));
        assert!(!looks_like_uuid("memory"));
        assert!(!looks_like_uuid("123e4567-e89b-12d3-a456"));
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_does_not_follow_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("link");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("large.txt"), vec![b'x'; 4096]).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let link_meta_len = std::fs::symlink_metadata(&link).unwrap().len();

        assert_eq!(dir_size(&link).unwrap(), link_meta_len);
    }
}
