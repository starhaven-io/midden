use anyhow::{Result, bail};
use colored::Colorize;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::backup;
use crate::claude_json::{self, ClaudeJson};
use crate::orphans;
use crate::output;
use crate::paths::Env;
use crate::process;
use crate::transcripts;

pub struct Options {
    pub apply: bool,
    pub transcripts: bool,
    pub worktrees_only: bool,
    pub force: bool,
    pub json: bool,
}

type ProjectApplyResult = Option<(PathBuf, usize, usize)>;
type TranscriptApplyResult = Option<transcripts::Report>;

pub fn run(env: &Env, opts: Options) -> Result<ExitCode> {
    if opts.transcripts {
        return run_with_transcripts(env, opts);
    }

    let path = &env.claude_json;
    if !path.exists() {
        bail!("not found: {}", path.display());
    }

    let config = ClaudeJson::load(path)?;
    let Some(projects) = config.projects() else {
        if opts.json {
            println!("{}", json!({"total": 0, "orphans": [], "removed": false}));
        } else {
            println!("no 'projects' map found; nothing to do");
        }
        return Ok(ExitCode::SUCCESS);
    };
    let total = projects.len();
    let orphans = orphans::find(projects, opts.worktrees_only);

    if orphans.is_empty() {
        if opts.json {
            println!(
                "{}",
                json!({"total": total, "orphans": [], "removed": false})
            );
        } else {
            println!("clean. {total} project entries, none orphaned.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (wt, other) = orphans::counts(&orphans);
    let new_raw = preview_pruned(&config, &orphans)?;
    let saved = config.raw.len().saturating_sub(new_raw.len());

    if opts.json {
        let applied = if opts.apply {
            apply_prune(env, &opts)?
        } else {
            None
        };
        println!(
            "{}",
            json!({
                "total": total,
                "orphans": orphans.iter().map(|o| json!({
                    "path": o.path,
                    "is_worktree": o.is_worktree,
                })).collect::<Vec<_>>(),
                "bytes_before": config.raw.len(),
                "bytes_after": applied.as_ref().map(|(_, _, b)| *b).unwrap_or(new_raw.len()),
                "removed": applied.is_some(),
                "backup": applied.as_ref().map(|(p, _, _)| p.display().to_string()),
            })
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{total} project entries total; {} orphaned ({} worktree, {} other):",
        orphans.len().to_string().yellow(),
        wt,
        other,
    );
    println!();
    for o in &orphans {
        let tag = if o.is_worktree {
            "   [worktree]".dimmed().to_string()
        } else {
            String::new()
        };
        println!("  - {}{tag}", o.path);
    }
    println!();
    println!(
        "would shrink {} by ~{} ({} -> {}).",
        path.file_name().unwrap_or_default().to_string_lossy(),
        output::kb(saved),
        output::kb(config.raw.len()),
        output::kb(new_raw.len()),
    );

    if !opts.apply {
        println!();
        println!("dry run. re-run with --apply to remove these entries.");
        println!("quit all Claude Code sessions first; it rewrites this file live.");
        return Ok(ExitCode::SUCCESS);
    }

    match apply_prune(env, &opts)? {
        Some((backup_path, removed, _)) => {
            println!();
            println!("backed up to {}", backup_path.display());
            println!(
                "removed {removed} entries from {}",
                env.claude_json.display()
            );
        }
        None => {
            println!();
            println!("nothing left to remove on re-check; file left unmodified.");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_with_transcripts(env: &Env, opts: Options) -> Result<ExitCode> {
    let path = &env.claude_json;
    if !path.exists() {
        bail!("not found: {}", path.display());
    }

    let config = ClaudeJson::load(path)?;
    let total = config.projects().map(|p| p.len()).unwrap_or(0);
    let orphans = config
        .projects()
        .map(|projects| orphans::find(projects, opts.worktrees_only))
        .unwrap_or_default();
    let transcript_report = transcripts::discover(&env.claude_home, opts.worktrees_only)?;

    let new_raw = if orphans.is_empty() {
        None
    } else {
        Some(preview_pruned(&config, &orphans)?)
    };

    if opts.json {
        let (applied_prune, applied_transcripts) = if opts.apply {
            apply_prune_and_transcripts(env, &opts)?
        } else {
            (None, None)
        };
        let transcript_json = applied_transcripts
            .as_ref()
            .unwrap_or(&transcript_report)
            .to_json();
        println!(
            "{}",
            json!({
                "total": total,
                "orphans": orphans.iter().map(|o| json!({
                    "path": o.path,
                    "is_worktree": o.is_worktree,
                })).collect::<Vec<_>>(),
                "bytes_before": config.raw.len(),
                "bytes_after": applied_prune
                    .as_ref()
                    .map(|(_, _, b)| *b)
                    .or_else(|| new_raw.as_ref().map(String::len))
                    .unwrap_or(config.raw.len()),
                "removed": applied_prune.is_some(),
                "backup": applied_prune.as_ref().map(|(p, _, _)| p.display().to_string()),
                "transcripts": transcript_json,
            })
        );
        return Ok(ExitCode::SUCCESS);
    }

    print_project_preview(path, &config, total, &orphans, new_raw.as_deref())?;
    print_transcript_preview(&transcript_report);

    if !opts.apply {
        println!();
        println!("dry run. re-run with --apply to remove these entries.");
        println!(
            "quit all Claude Code sessions first; it rewrites this file live and may add transcripts."
        );
        return Ok(ExitCode::SUCCESS);
    }

    let (applied_prune, applied_transcripts) = apply_prune_and_transcripts(env, &opts)?;

    print_project_apply(env, applied_prune);
    if let Some(report) = applied_transcripts {
        print_transcript_apply(&report);
    } else {
        println!();
        println!("no orphaned transcript artifacts to remove.");
    }

    Ok(ExitCode::SUCCESS)
}

fn print_project_preview(
    path: &std::path::Path,
    config: &ClaudeJson,
    total: usize,
    orphans: &[orphans::Orphan],
    new_raw: Option<&str>,
) -> Result<()> {
    let Some(_projects) = config.projects() else {
        println!("no 'projects' map found.");
        return Ok(());
    };

    if orphans.is_empty() {
        println!("clean. {total} project entries, none orphaned.");
        return Ok(());
    }

    let (wt, other) = orphans::counts(orphans);
    let new_raw = new_raw.expect("new_raw is present when orphans are present");
    let saved = config.raw.len().saturating_sub(new_raw.len());
    println!(
        "{total} project entries total; {} orphaned ({} worktree, {} other):",
        orphans.len().to_string().yellow(),
        wt,
        other,
    );
    println!();
    for o in orphans {
        let tag = if o.is_worktree {
            "   [worktree]".dimmed().to_string()
        } else {
            String::new()
        };
        println!("  - {}{tag}", o.path);
    }
    println!();
    println!(
        "would shrink {} by ~{} ({} -> {}).",
        path.file_name().unwrap_or_default().to_string_lossy(),
        output::kb(saved),
        output::kb(config.raw.len()),
        output::kb(new_raw.len()),
    );
    Ok(())
}

fn print_project_apply(env: &Env, applied: ProjectApplyResult) {
    match applied {
        Some((backup_path, removed, _)) => {
            println!();
            println!("backed up to {}", backup_path.display());
            println!(
                "removed {removed} entries from {}",
                env.claude_json.display()
            );
        }
        None => {
            println!();
            println!("no project entries removed.");
        }
    }
}

fn print_transcript_preview(report: &transcripts::Report) {
    println!();
    println!("transcripts");
    if report.total() == 0 {
        println!(
            "  no transcript project dirs found under {}",
            report.projects_dir.display()
        );
        return;
    }

    println!(
        "  {} dirs; {} dead, {} kept, {} skipped; would reclaim ~{}; kept uses ~{}.",
        report.total(),
        report.dead_count().to_string().yellow(),
        report.kept_count(),
        report.skipped_count(),
        output::human_bytes(report.bytes()),
        output::human_bytes(report.kept_storage_bytes()),
    );

    for dir in &report.dirs {
        match dir.status {
            transcripts::DirStatus::Dead => {
                println!(
                    "  - {} -> {} ({})",
                    dir.path.display(),
                    dir.derived_cwd.as_deref().unwrap_or("<unknown>"),
                    output::human_bytes(dir.bytes)
                );
                for target in &dir.delete {
                    println!("      delete {}", target.display());
                }
                print_cleanup_note(dir);
            }
            transcripts::DirStatus::Skipped => {
                println!(
                    "  - {} [skipped: {}]",
                    dir.path.display(),
                    dir.reason.unwrap_or("cannot-tell")
                );
            }
            transcripts::DirStatus::Kept => {}
        }
    }

    print_kept_transcript_storage(report);
}

fn print_kept_transcript_storage(report: &transcripts::Report) {
    let dirs = report.top_kept_by_storage(5);
    if dirs.is_empty() {
        return;
    }

    println!("  largest kept transcript dirs:");
    for dir in dirs {
        println!(
            "    - {} -> {} ({})",
            dir.path.display(),
            dir.derived_cwd.as_deref().unwrap_or("<unknown>"),
            output::human_bytes(dir.storage_bytes),
        );
    }
}

fn print_transcript_apply(report: &transcripts::Report) {
    println!();
    println!("transcripts");
    if report.dead_count() == 0 {
        println!("  no orphaned transcript artifacts to remove.");
        return;
    }

    println!(
        "  removed {} artifacts from {} dead dirs; reclaimed ~{}.",
        report.dirs.iter().map(|d| d.deleted.len()).sum::<usize>(),
        report.dead_count(),
        output::human_bytes(report.bytes()),
    );
    for dir in report.dirs.iter().filter(|d| d.is_dead()) {
        println!("  - {}", dir.path.display());
        for target in &dir.deleted {
            println!("      deleted {}", target.display());
        }
        print_cleanup_note(dir);
    }
}

fn print_cleanup_note(dir: &transcripts::DirReport) {
    match dir.cleanup {
        transcripts::Cleanup::MemoryPreserved => {
            println!("      memory preserved");
        }
        transcripts::Cleanup::PartiallyCleaned => {
            println!("      partially cleaned; unknown entries left in place");
        }
        transcripts::Cleanup::WouldRemoveDir => {
            println!("      directory would be removed");
        }
        transcripts::Cleanup::RemovedDir => {
            println!("      directory removed");
        }
        transcripts::Cleanup::None => {}
    }
}

fn apply_prune_and_transcripts(
    env: &Env,
    opts: &Options,
) -> Result<(ProjectApplyResult, TranscriptApplyResult)> {
    let mut config = ClaudeJson::load(&env.claude_json)?;
    let total = config.projects().map(|p| p.len()).unwrap_or(0);
    let drop: BTreeSet<String> = match config.projects() {
        Some(projects) => orphans::find(projects, opts.worktrees_only)
            .into_iter()
            .map(|o| o.path)
            .collect(),
        None => BTreeSet::new(),
    };
    let transcript_report = transcripts::discover(&env.claude_home, opts.worktrees_only)?;

    ensure_apply_gates(env, opts, drop.len(), total, &transcript_report)?;

    let applied_prune = apply_project_drop(env, &mut config, &drop)?;
    let applied_transcripts = if transcript_report.dead_count() == 0 {
        None
    } else {
        Some(transcripts::delete_dead(transcript_report)?)
    };

    Ok((applied_prune, applied_transcripts))
}

fn ensure_apply_gates(
    env: &Env,
    opts: &Options,
    project_removing: usize,
    project_total: usize,
    transcript_report: &transcripts::Report,
) -> Result<()> {
    ensure_project_wrong_host_gate(opts, project_removing, project_total)?;
    ensure_transcript_wrong_host_gate(opts, transcript_report)?;
    if project_removing == 0 && transcript_report.dead_count() == 0 {
        return Ok(());
    }

    ensure_claude_not_running(
        env,
        opts,
        running_claude_live_state(env, project_removing, transcript_report.dead_count()),
    )
}

fn running_claude_live_state(
    env: &Env,
    project_removing: usize,
    transcript_removing: usize,
) -> String {
    let config = env.claude_json.display();
    let projects = env.claude_home.join("projects");
    let projects = projects.display();

    match (project_removing > 0, transcript_removing > 0) {
        (true, true) => {
            format!("Claude Code rewrites {config} and may add transcripts under {projects} live")
        }
        (true, false) => {
            format!("Claude Code rewrites {config} live and may overwrite our changes")
        }
        (false, true) => format!("Claude Code may add transcripts under {projects} live"),
        (false, false) => "Claude Code state may change live".to_string(),
    }
}

fn ensure_project_wrong_host_gate(opts: &Options, removing: usize, total: usize) -> Result<()> {
    if !opts.force && orphans::looks_like_wrong_host(removing, total) {
        bail!(
            "{} of {} project entries resolve missing — this usually means you are on a \
             different machine or an unmounted volume, not that they are all dead. \
             Re-run with --force to prune them anyway.",
            removing,
            total
        );
    }
    Ok(())
}

fn ensure_transcript_wrong_host_gate(opts: &Options, report: &transcripts::Report) -> Result<()> {
    if !opts.force
        && orphans::looks_like_wrong_host_with_scope(
            report.dead_count(),
            report.resolvable_count(),
            report.total(),
        )
    {
        bail!(
            "{} of {} resolvable transcript dirs point at missing projects — this usually means \
             you are on a different machine or an unmounted volume, not that they are all dead \
             ({} total transcript dirs discovered). \
             Re-run with --force to prune them anyway.",
            report.dead_count(),
            report.resolvable_count(),
            report.total()
        );
    }
    Ok(())
}

fn ensure_claude_not_running(env: &Env, opts: &Options, live_state: String) -> Result<()> {
    ensure_claude_not_running_state(
        opts.force,
        || !fixture_assumes_no_claude_process(env) && process::claude_is_running(),
        live_state,
    )
}

fn ensure_claude_not_running_state(
    force: bool,
    running: impl FnOnce() -> bool,
    live_state: String,
) -> Result<()> {
    if !force && running() {
        bail!(
            "a `claude` process is running — quit it first, or pass --force \
             ({live_state})"
        );
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn fixture_assumes_no_claude_process(env: &Env) -> bool {
    let Some(config) = std::env::var_os("MIDDEN_TEST_ASSUME_NO_CLAUDE_PROCESS") else {
        return false;
    };
    if config != env.claude_json.as_os_str() {
        return false;
    }

    let tmp = std::env::temp_dir();
    // Fixture-only apply tests use --config/--claude-home under temp dirs; a
    // developer's real Claude Code session should not make those tests flaky.
    env.claude_json.starts_with(&tmp) && env.claude_home.starts_with(&tmp)
}

#[cfg(not(debug_assertions))]
fn fixture_assumes_no_claude_process(_env: &Env) -> bool {
    false
}

fn preview_pruned(config: &ClaudeJson, orphans: &[orphans::Orphan]) -> Result<String> {
    let drop: BTreeSet<&str> = orphans.iter().map(|o| o.path.as_str()).collect();
    let mut new_data = config.data.clone();
    if let Some(map) = new_data.get_mut("projects").and_then(|v| v.as_object_mut()) {
        map.retain(|k, _| !drop.contains(k.as_str()));
    }
    claude_json::render(&new_data)
}

/// Re-read the config immediately before writing — so a concurrent Claude Code
/// rewrite of unrelated keys survives — and re-check existence so a directory
/// re-created since detection is not pruned. Backs up, then writes atomically.
/// Returns (backup path, entries removed, bytes after), or None when the
/// re-check leaves nothing to remove and the file is left untouched.
fn apply_prune(env: &Env, opts: &Options) -> Result<ProjectApplyResult> {
    let mut config = ClaudeJson::load(&env.claude_json)?;
    let total = config.projects().map(|p| p.len()).unwrap_or(0);
    let drop: BTreeSet<String> = match config.projects() {
        Some(projects) => orphans::find(projects, opts.worktrees_only)
            .into_iter()
            .map(|o| o.path)
            .collect(),
        None => BTreeSet::new(),
    };

    ensure_project_wrong_host_gate(opts, drop.len(), total)?;
    ensure_claude_not_running(
        env,
        opts,
        format!(
            "Claude Code rewrites {} live and may overwrite our changes",
            env.claude_json.display()
        ),
    )?;

    apply_project_drop(env, &mut config, &drop)
}

fn apply_project_drop(
    env: &Env,
    config: &mut ClaudeJson,
    drop: &BTreeSet<String>,
) -> Result<ProjectApplyResult> {
    let removed = match config.projects_mut() {
        Some(map) => {
            let before = map.len();
            map.retain(|k, _| !drop.contains(k));
            before - map.len()
        }
        None => 0,
    };
    if removed == 0 {
        // Everything re-checked as live since detection — rewriting an
        // identical file (and leaving a backup behind) serves nobody.
        return Ok(None);
    }
    let new_raw = claude_json::render(&config.data)?;
    let backup_path = backup::timestamped_copy(&env.claude_json)?;
    claude_json::write_atomic(&env.claude_json, &new_raw)?;
    Ok(Some((backup_path, removed, new_raw.len())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn test_env() -> Env {
        Env::new(
            Some(PathBuf::from("/tmp/test/.claude.json")),
            Some(PathBuf::from("/tmp/test/.claude")),
        )
    }

    #[test]
    fn running_claude_message_names_only_mutated_surfaces() {
        let env = test_env();

        let config_only = running_claude_live_state(&env, 1, 0);
        assert_eq!(
            config_only,
            "Claude Code rewrites /tmp/test/.claude.json live and may overwrite our changes"
        );

        let transcripts_only = running_claude_live_state(&env, 0, 1);
        assert_eq!(
            transcripts_only,
            "Claude Code may add transcripts under /tmp/test/.claude/projects live"
        );

        let both = running_claude_live_state(&env, 1, 1);
        assert_eq!(
            both,
            "Claude Code rewrites /tmp/test/.claude.json and may add transcripts under /tmp/test/.claude/projects live"
        );
    }

    #[test]
    fn running_claude_gate_blocks_unforced_writes() {
        let error = ensure_claude_not_running_state(false, || true, "live state".into())
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "a `claude` process is running — quit it first, or pass --force (live state)"
        );
        assert!(ensure_claude_not_running_state(false, || false, String::new()).is_ok());
        assert!(
            ensure_claude_not_running_state(
                true,
                || panic!("must not scan when forced"),
                String::new(),
            )
            .is_ok()
        );
    }

    #[test]
    fn apply_prune_skips_the_write_when_recheck_finds_nothing() {
        let home = tempfile::tempdir().unwrap();
        let config_path = home.path().join(".claude.json");
        let live = tempfile::tempdir().unwrap();
        let key = live.path().to_string_lossy().into_owned();
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({ "projects": { &key: {} } })).unwrap(),
        )
        .unwrap();
        let env = Env::new(Some(config_path.clone()), Some(home.path().join(".claude")));
        let opts = Options {
            apply: true,
            transcripts: false,
            worktrees_only: false,
            force: true,
            json: true,
        };
        let before = std::fs::read_to_string(&config_path).unwrap();

        let applied = apply_prune(&env, &opts).unwrap();

        assert!(applied.is_none(), "no orphans on re-check -> no write");
        assert_eq!(before, std::fs::read_to_string(&config_path).unwrap());
        let backups = std::fs::read_dir(home.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(backups, 0, "no backup for a no-op");
    }
}
