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

pub struct Options {
    pub apply: bool,
    pub worktrees_only: bool,
    pub force: bool,
    pub json: bool,
}

pub fn run(env: &Env, opts: Options) -> Result<ExitCode> {
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
            Some(apply_prune(env, &opts)?)
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

    let (backup_path, removed, _) = apply_prune(env, &opts)?;
    println!();
    println!("backed up to {}", backup_path.display());
    println!(
        "removed {removed} entries from {}",
        env.claude_json.display()
    );
    Ok(ExitCode::SUCCESS)
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
/// Returns (backup path, entries removed, bytes after).
fn apply_prune(env: &Env, opts: &Options) -> Result<(PathBuf, usize, usize)> {
    let mut config = ClaudeJson::load(&env.claude_json)?;
    let total = config.projects().map(|p| p.len()).unwrap_or(0);
    let drop: BTreeSet<String> = match config.projects() {
        Some(projects) => orphans::find(projects, opts.worktrees_only)
            .into_iter()
            .map(|o| o.path)
            .collect(),
        None => BTreeSet::new(),
    };

    if !opts.force && orphans::looks_like_wrong_host(drop.len(), total) {
        bail!(
            "{} of {} project entries resolve missing — this usually means you are on a \
             different machine or an unmounted volume, not that they are all dead. \
             Re-run with --force to prune them anyway.",
            drop.len(),
            total
        );
    }
    if !opts.force && process::claude_is_running() {
        bail!(
            "a `claude` process is running — quit it first, or pass --force \
             (Claude Code rewrites {} live and may overwrite our changes)",
            env.claude_json.display()
        );
    }

    let removed = match config.projects_mut() {
        Some(map) => {
            let before = map.len();
            map.retain(|k, _| !drop.contains(k));
            before - map.len()
        }
        None => 0,
    };
    let new_raw = claude_json::render(&config.data)?;
    let backup_path = backup::timestamped_copy(&env.claude_json)?;
    claude_json::write_atomic(&env.claude_json, &new_raw)?;
    Ok((backup_path, removed, new_raw.len()))
}
