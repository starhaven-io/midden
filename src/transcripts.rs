use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, RenameFlags, fchmod, fstat, mkdirat, openat,
    renameat_with, statat, unlinkat,
};

use crate::orphans;
use crate::paths::WORKTREE_MARKER;
use crate::safe_io;

const MAX_CWD_SCAN_LINES: usize = 64;
const MAX_JSONL_LINE_BYTES: usize = 1024 * 1024;
const MAX_PROJECT_TRANSCRIPT_ENTRIES: usize = 4096;
#[cfg(unix)]
const QUARANTINE_ATTEMPTS: usize = 32;

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
    identity: Option<FileIdentity>,
    delete_artifacts: Vec<DeleteArtifact>,
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
    projects_identity: Option<FileIdentity>,
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
    let configured_projects_dir = claude_home.join("projects");
    let mut dirs = Vec::new();

    if !configured_projects_dir.exists() {
        return Ok(Report {
            projects_dir: configured_projects_dir,
            dirs,
            applied: false,
            projects_identity: None,
        });
    }

    // Anchor mutation to the directory Claude's configured path resolved to at
    // discovery time. This supports a symlinked state root without relaxing
    // the O_NOFOLLOW checks used for every child and revalidates the target's
    // identity immediately before deletion.
    let projects_dir = configured_projects_dir.canonicalize().with_context(|| {
        format!(
            "resolve transcript state root {}",
            configured_projects_dir.display()
        )
    })?;

    let projects_identity = fs::metadata(&projects_dir)
        .ok()
        .map(FileIdentity::from_metadata);

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
        projects_identity,
    })
}

pub fn delete_dead(mut report: Report) -> Result<Report> {
    #[cfg(unix)]
    let projects = open_verified_directory(&report.projects_dir, report.projects_identity)
        .with_context(|| format!("re-open {}", report.projects_dir.display()))?;
    for dir in &mut report.dirs {
        if !dir.is_dead() {
            continue;
        }

        #[cfg(unix)]
        delete_dir_artifacts(&projects, &report.projects_dir, dir)?;
        #[cfg(not(unix))]
        {
            for target in dir.delete.clone() {
                remove_artifact(&target).with_context(|| format!("delete {}", target.display()))?;
                dir.deleted.push(target);
            }
            dir.memory_preserved = has_memory_dir(&dir.path);
            dir.cleanup = cleanup_after_delete(&dir.path, &dir.delete)?;
        }
    }
    report.applied = true;
    Ok(report)
}

pub(crate) fn project_cwds(path: &Path, limit: usize) -> Result<(Vec<PathBuf>, bool)> {
    let entries = fs::read_dir(path).with_context(|| format!("read {}", path.display()))?;
    let mut jsonl_files = Vec::new();
    let mut entry_limit_reached = false;
    for (index, entry) in entries.enumerate() {
        if index == MAX_PROJECT_TRANSCRIPT_ENTRIES {
            entry_limit_reached = true;
            break;
        }
        let entry = entry.with_context(|| format!("read {}", path.display()))?;
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file())
            && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        {
            jsonl_files.push(path);
        }
    }
    jsonl_files.sort();
    let mut incomplete = entry_limit_reached || jsonl_files.len() > limit;
    jsonl_files.truncate(limit);
    let mut cwds = BTreeSet::new();
    for jsonl in &jsonl_files {
        if let Some(cwd) = cwd_from_jsonl(jsonl)? {
            cwds.insert(PathBuf::from(cwd));
        } else {
            incomplete = true;
        }
    }
    Ok((cwds.into_iter().collect(), incomplete))
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
            Ok(None) => return Ok(skipped_with_storage(path, "no-cwd", storage_bytes)),
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
        identity: fs::metadata(path).ok().map(FileIdentity::from_metadata),
        delete_artifacts: scan.delete,
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
        identity: fs::metadata(path).ok().map(FileIdentity::from_metadata),
        delete_artifacts: Vec::new(),
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

#[derive(Debug, Clone)]
struct DeleteArtifact {
    path: PathBuf,
    file_size: Option<u64>,
    identity: FileIdentity,
    kind: ArtifactKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    #[cfg(unix)]
    fn from_metadata(metadata: std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(_metadata: std::fs::Metadata) -> Self {
        Self {
            device: 0,
            inode: 0,
        }
    }

    #[cfg(unix)]
    // st_dev is already u64 on Linux but i32 on macOS, so the widening cast is
    // load-bearing on one target and redundant on the other.
    #[allow(clippy::unnecessary_cast, reason = "st_dev width is platform-specific")]
    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
        }
    }
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

        if is_jsonl && file_type.is_file() {
            jsonl_files.push(artifact.clone());
        }

        if (is_jsonl && file_type.is_file()) || is_uuid_dir {
            delete.push(DeleteArtifact {
                path: artifact,
                file_size: file_type.is_file().then_some(meta.len()),
                identity: FileIdentity::from_metadata(meta),
                kind: if is_uuid_dir {
                    ArtifactKind::Directory
                } else {
                    ArtifactKind::File
                },
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

#[cfg(not(unix))]
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

#[cfg(not(unix))]
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

#[cfg(not(unix))]
fn has_memory_dir(path: &Path) -> bool {
    fs::symlink_metadata(path.join("memory"))
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn remove_artifact(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn open_verified_directory(
    path: &Path,
    expected: Option<FileIdentity>,
) -> Result<std::os::fd::OwnedFd> {
    let expected = expected.context("directory identity was unavailable during discovery")?;
    let directory = openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let actual = FileIdentity::from_stat(&fstat(&directory).map_err(std::io::Error::from)?);
    if actual != expected {
        bail_identity(path, "directory changed after discovery")?;
    }
    Ok(directory)
}

#[cfg(unix)]
fn delete_dir_artifacts(
    projects: &std::os::fd::OwnedFd,
    projects_path: &Path,
    report: &mut DirReport,
) -> Result<()> {
    let name = report
        .path
        .strip_prefix(projects_path)
        .ok()
        .filter(|path| path.components().count() == 1)
        .context("transcript directory escaped the discovered projects directory")?;
    let directory = openat(
        projects,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .with_context(|| format!("re-open {}", report.path.display()))?;
    let actual = FileIdentity::from_stat(&fstat(&directory).map_err(std::io::Error::from)?);
    if Some(actual) != report.identity {
        bail_identity(&report.path, "transcript directory changed after discovery")?;
    }

    revalidate_transcript_evidence(&directory, report)?;
    revalidate_artifact_set(&directory, report)?;
    let (quarantine_name, quarantine) = create_quarantine(&directory)?;
    for artifact in report.delete_artifacts.clone() {
        let artifact_name = artifact
            .path
            .file_name()
            .context("transcript artifact has no file name")?;
        renameat_with(
            &directory,
            artifact_name,
            &quarantine,
            artifact_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)
        .with_context(|| format!("quarantine {}", artifact.path.display()))?;
        let stat = match statat(&quarantine, artifact_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) => {
                restore_quarantined(
                    &directory,
                    &quarantine,
                    artifact_name,
                    &report.path.join(&quarantine_name),
                )?;
                unlinkat(&directory, &quarantine_name, AtFlags::REMOVEDIR)
                    .map_err(std::io::Error::from)?;
                return Err(std::io::Error::from(error))
                    .with_context(|| format!("re-stat quarantined {}", artifact.path.display()));
            }
        };
        let actual = FileIdentity::from_stat(&stat);
        let actual_kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => Some(ArtifactKind::File),
            FileType::Directory => Some(ArtifactKind::Directory),
            _ => None,
        };
        if actual != artifact.identity || actual_kind != Some(artifact.kind) {
            restore_quarantined(
                &directory,
                &quarantine,
                artifact_name,
                &report.path.join(&quarantine_name),
            )?;
            unlinkat(&directory, &quarantine_name, AtFlags::REMOVEDIR)
                .map_err(std::io::Error::from)?;
            bail_identity(
                &artifact.path,
                "transcript artifact changed after discovery",
            )?;
        }
        match artifact.kind {
            ArtifactKind::File => unlinkat(&quarantine, artifact_name, AtFlags::empty())
                .map_err(std::io::Error::from)
                .map_err(anyhow::Error::from),
            ArtifactKind::Directory => {
                remove_directory_at(&quarantine, artifact_name, Some(artifact.identity))
            }
        }
        .with_context(|| {
            format!(
                "deletion stopped for {}; inspect remaining data in {} before moving or removing it",
                artifact.path.display(),
                report.path.join(&quarantine_name).display()
            )
        })?;
        report.deleted.push(artifact.path);
    }
    if !anchored_remaining_names(&quarantine)?.is_empty() {
        anyhow::bail!(
            "transcript quarantine was not empty after deletion: {}",
            report.path.join(&quarantine_name).display()
        );
    }
    unlinkat(&directory, &quarantine_name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
    let remaining = anchored_remaining_names(&directory)?;
    report.memory_preserved = remaining.iter().any(|name| name == "memory");
    report.cleanup = if remaining.is_empty() {
        remove_empty_directory_at(projects, name, actual, &report.path)?;
        Cleanup::RemovedDir
    } else if remaining.len() == 1 && report.memory_preserved {
        Cleanup::MemoryPreserved
    } else {
        Cleanup::PartiallyCleaned
    };
    Ok(())
}

#[cfg(unix)]
fn random_quarantine_name(prefix: &str) -> Result<std::ffi::OsString> {
    use std::fmt::Write as _;

    let mut random = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut random)
        .context("read /dev/urandom")?;
    let mut name = prefix.to_string();
    for byte in random {
        write!(&mut name, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(name.into())
}

#[cfg(unix)]
fn create_quarantine(
    directory: &std::os::fd::OwnedFd,
) -> Result<(std::ffi::OsString, std::os::fd::OwnedFd)> {
    for _ in 0..QUARANTINE_ATTEMPTS {
        let name = random_quarantine_name(".midden-delete-")?;
        match mkdirat(directory, &name, Mode::RWXU) {
            Ok(()) => {
                let quarantine = openat(
                    directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)?;
                fchmod(&quarantine, Mode::RWXU).map_err(std::io::Error::from)?;
                return Ok((name, quarantine));
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
    anyhow::bail!("could not create a unique transcript quarantine")
}

#[cfg(unix)]
fn restore_quarantined(
    directory: &std::os::fd::OwnedFd,
    quarantine: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    quarantine_path: &Path,
) -> Result<()> {
    renameat_with(quarantine, name, directory, name, RenameFlags::NOREPLACE)
        .map_err(std::io::Error::from)
        .with_context(|| {
            format!(
                "restore changed transcript artifact; preserved replacement in {}",
                quarantine_path.display()
            )
        })
}

#[cfg(unix)]
fn remove_empty_directory_at(
    parent: &std::os::fd::OwnedFd,
    name: &Path,
    expected: FileIdentity,
    display_path: &Path,
) -> Result<()> {
    for _ in 0..QUARANTINE_ATTEMPTS {
        let quarantine_name = random_quarantine_name(".midden-empty-")?;
        match renameat_with(
            parent,
            name,
            parent,
            &quarantine_name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                let stat = statat(parent, &quarantine_name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(std::io::Error::from)?;
                if FileIdentity::from_stat(&stat) != expected
                    || FileType::from_raw_mode(stat.st_mode) != FileType::Directory
                {
                    renameat_with(
                        parent,
                        &quarantine_name,
                        parent,
                        name,
                        RenameFlags::NOREPLACE,
                    )
                    .map_err(std::io::Error::from)
                    .with_context(|| {
                        format!(
                            "restore changed transcript directory; preserved replacement as {}",
                            quarantine_name.to_string_lossy()
                        )
                    })?;
                    bail_identity(display_path, "transcript directory changed before removal")?;
                }
                unlinkat(parent, &quarantine_name, AtFlags::REMOVEDIR)
                    .map_err(std::io::Error::from)?;
                return Ok(());
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
    anyhow::bail!("could not reserve a unique transcript-directory quarantine")
}

#[cfg(unix)]
fn anchored_remaining_names(directory: &std::os::fd::OwnedFd) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(unix)]
fn revalidate_transcript_evidence(
    directory: &std::os::fd::OwnedFd,
    report: &DirReport,
) -> Result<()> {
    let expected_cwd = report
        .derived_cwd
        .as_deref()
        .context("dead transcript directory has no cwd evidence")?;
    if !orphans::provably_absent(Path::new(expected_cwd)) {
        anyhow::bail!("project became live after transcript discovery: {expected_cwd}");
    }
    for artifact in report
        .delete_artifacts
        .iter()
        .filter(|artifact| artifact.kind == ArtifactKind::File)
    {
        let name = artifact
            .path
            .file_name()
            .context("JSONL artifact has no name")?;
        let file = openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let stat = fstat(&file).map_err(std::io::Error::from)?;
        if FileIdentity::from_stat(&stat) != artifact.identity
            || FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        {
            bail_identity(&artifact.path, "JSONL artifact changed after discovery")?;
        }
        let file = std::fs::File::from(file);
        let current_cwd = cwd_from_reader(file)?;
        if current_cwd.as_deref() != Some(expected_cwd) {
            bail_identity(
                &artifact.path,
                "transcript cwd evidence changed after discovery",
            )?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn revalidate_artifact_set(directory: &std::os::fd::OwnedFd, report: &DirReport) -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let expected = report
        .delete_artifacts
        .iter()
        .filter_map(|artifact| artifact.path.file_name().map(ToOwned::to_owned))
        .collect::<BTreeSet<_>>();
    let mut current = BTreeSet::new();
    for entry in Dir::read_from(directory).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name);
        let path = Path::new(name);
        let stat =
            statat(directory, path, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        let kind = FileType::from_raw_mode(stat.st_mode);
        let eligible = (path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            && kind == FileType::RegularFile)
            || (kind == FileType::Directory
                && path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(looks_like_uuid));
        if eligible {
            current.insert(name.to_owned());
        }
    }
    if current != expected {
        anyhow::bail!("transcript artifact set changed after discovery");
    }
    Ok(())
}

#[cfg(unix)]
fn remove_directory_at(
    parent: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    expected: Option<FileIdentity>,
) -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let directory = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let identity = FileIdentity::from_stat(&fstat(&directory).map_err(std::io::Error::from)?);
    if expected.is_some_and(|expected| expected != identity) {
        anyhow::bail!("quarantined transcript directory changed before traversal");
    }
    fchmod(&directory, Mode::RWXU).map_err(std::io::Error::from)?;
    let entries = Dir::read_from(&directory).map_err(std::io::Error::from)?;
    for entry in entries {
        let entry = entry.map_err(std::io::Error::from)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let child = OsStr::from_bytes(bytes);
        let stat =
            statat(&directory, child, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            remove_directory_at(&directory, child, Some(FileIdentity::from_stat(&stat)))?;
        } else {
            unlinkat(&directory, child, AtFlags::empty()).map_err(std::io::Error::from)?;
        }
    }
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
    if FileIdentity::from_stat(&stat) != identity
        || FileType::from_raw_mode(stat.st_mode) != FileType::Directory
    {
        anyhow::bail!("quarantined transcript directory changed before removal");
    }
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(unix)]
fn bail_identity(path: &Path, message: &str) -> Result<()> {
    anyhow::bail!("{message}: {}", path.display())
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
    let file =
        safe_io::open_regular(path, false).with_context(|| format!("open {}", path.display()))?;
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
    let cwd = value.get("cwd")?.as_str()?;
    (!cwd.is_empty() && Path::new(cwd).is_absolute()).then(|| cwd.to_owned())
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
    fn cwd_extraction_rejects_empty_and_relative_paths() {
        let data = br#"{"cwd":""}
{"cwd":"relative/project"}
"#;
        assert_eq!(cwd_from_reader(Cursor::new(data)).unwrap(), None);
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

    #[cfg(unix)]
    fn dead_transcript_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let claude_home = root.path().join("claude");
        let project = claude_home.join("projects/project-slug");
        let session = project.join("session.jsonl");
        let missing = root.path().join("missing-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            &session,
            format!("{{\"cwd\":{:?}}}\n", missing.display().to_string()),
        )
        .unwrap();
        (root, claude_home, project, session)
    }

    #[cfg(unix)]
    #[test]
    fn deletion_refuses_a_replaced_transcript_file() {
        let (_root, claude_home, project, session) = dead_transcript_fixture();
        let report = discover(&claude_home, false).unwrap();
        assert_eq!(report.dead_count(), 1);

        let replacement = project.join("replacement");
        std::fs::write(&replacement, std::fs::read(&session).unwrap()).unwrap();
        std::fs::rename(&replacement, &session).unwrap();

        let error = delete_dead(report).unwrap_err().to_string();
        assert!(error.contains("changed after discovery"), "{error}");
        assert!(session.exists(), "the replacement must not be deleted");
    }

    #[cfg(unix)]
    #[test]
    fn deletion_refuses_a_replaced_transcript_directory() {
        let (root, claude_home, project, session) = dead_transcript_fixture();
        let report = discover(&claude_home, false).unwrap();
        assert_eq!(report.dead_count(), 1);

        let original = root.path().join("original-project-slug");
        std::fs::rename(&project, &original).unwrap();
        std::fs::create_dir(&project).unwrap();
        std::fs::write(
            &session,
            std::fs::read(original.join("session.jsonl")).unwrap(),
        )
        .unwrap();

        let error = delete_dead(report).unwrap_err().to_string();
        assert!(error.contains("changed after discovery"), "{error}");
        assert!(
            session.exists(),
            "the replacement directory must be untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deletion_rechecks_that_the_project_is_still_absent() {
        let (root, claude_home, _project, session) = dead_transcript_fixture();
        let report = discover(&claude_home, false).unwrap();
        assert_eq!(report.dead_count(), 1);
        std::fs::create_dir(root.path().join("missing-project")).unwrap();

        let error = delete_dead(report).unwrap_err().to_string();
        assert!(error.contains("became live"), "{error}");
        assert!(
            session.exists(),
            "live-project evidence must prevent deletion"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deletion_supports_a_symlinked_projects_root() {
        let root = tempfile::tempdir().unwrap();
        let claude_home = root.path().join("claude");
        let real_projects = root.path().join("state/projects");
        let project = real_projects.join("project-slug");
        let session = project.join("session.jsonl");
        let missing = root.path().join("missing-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&claude_home).unwrap();
        std::fs::write(
            &session,
            format!("{{\"cwd\":{:?}}}\n", missing.display().to_string()),
        )
        .unwrap();
        std::os::unix::fs::symlink(&real_projects, claude_home.join("projects")).unwrap();

        let report = discover(&claude_home, false).unwrap();
        assert_eq!(report.projects_dir, real_projects.canonicalize().unwrap());
        let applied = delete_dead(report).unwrap();

        assert!(applied.applied);
        assert!(!session.exists());
        assert!(claude_home.join("projects").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn transcript_discovery_does_not_follow_jsonl_symlinks() {
        let (root, claude_home, project, session) = dead_transcript_fixture();
        let external = root.path().join("external.jsonl");
        std::fs::rename(&session, &external).unwrap();
        std::os::unix::fs::symlink(&external, &session).unwrap();

        let report = discover(&claude_home, false).unwrap();
        assert_eq!(report.dead_count(), 0);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.dirs[0].reason, Some("no-jsonl"));
        assert!(project.exists());
    }
}
