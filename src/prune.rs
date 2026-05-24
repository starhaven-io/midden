use anyhow::{Result, bail};
use colored::Colorize;
use serde_json::json;
use std::collections::BTreeSet;
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

    let mut config = ClaudeJson::load(path)?;
    let total = match config.projects() {
        Some(p) => p.len(),
        None => {
            if opts.json {
                println!("{}", json!({"total": 0, "orphans": [], "removed": false}));
            } else {
                println!("no 'projects' map found; nothing to do");
            }
            return Ok(ExitCode::SUCCESS);
        }
    };

    let projects = config.projects().expect("checked above");
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
        let removed = if opts.apply {
            apply_prune(env, &mut config, &orphans, &new_raw, &opts)?
        } else {
            false
        };
        let backup_path = if removed {
            // We just wrote the backup; sibling glob would be brittle. Use the
            // value we returned from apply_prune in the future. For now this
            // is implicit (caller knows it was created).
            Some(())
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
                "bytes_after": new_raw.len(),
                "removed": removed,
                "backup": backup_path.map(|_| true),
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

    apply_prune(env, &mut config, &orphans, &new_raw, &opts)?;
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

fn apply_prune(
    env: &Env,
    config: &mut ClaudeJson,
    orphans: &[orphans::Orphan],
    new_raw: &str,
    opts: &Options,
) -> Result<bool> {
    if !opts.force && process::claude_is_running() {
        bail!(
            "a `claude` process is running — quit it first, or pass --force \
             (Claude Code rewrites {} live and may overwrite our changes)",
            env.claude_json.display()
        );
    }

    let drop: BTreeSet<&str> = orphans.iter().map(|o| o.path.as_str()).collect();
    if let Some(map) = config.projects_mut() {
        map.retain(|k, _| !drop.contains(k.as_str()));
    }

    let backup_path = backup::timestamped_copy(&env.claude_json)?;
    std::fs::write(&env.claude_json, new_raw)?;
    if !opts.json {
        println!();
        println!("backed up to {}", backup_path.display());
        println!(
            "removed {} entries from {}",
            orphans.len(),
            env.claude_json.display()
        );
    }
    Ok(true)
}
