mod backup;
mod claude_json;
mod doctor;
mod git;
mod orphans;
mod output;
mod paths;
mod process;
mod prune;
mod secrets;
mod show;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use colored::control;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
enum ColorMode {
    Always,
    Auto,
    Never,
}

#[derive(Parser)]
#[command(
    name = "midden",
    about = "Resolve, audit, and garbage-collect Claude Code's accumulated state",
    version,
    propagate_version = true
)]
struct Cli {
    /// When to use colors: auto, always, never
    #[arg(long, default_value = "auto", global = true)]
    color: ColorMode,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,

    /// Path to `~/.claude.json` (default: $HOME/.claude.json)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Path to the Claude user-scope directory (default: $HOME/.claude)
    #[arg(long, global = true, value_name = "PATH")]
    claude_home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Garbage-collect dead `projects` entries from ~/.claude.json
    Prune {
        /// Actually remove entries (default is a dry run)
        #[arg(long)]
        apply: bool,

        /// Only consider entries under a .claude/worktrees/ path
        #[arg(long = "worktrees-only")]
        worktrees_only: bool,

        /// Write even if a `claude` process appears to be running
        #[arg(long)]
        force: bool,
    },
    /// Hygiene and audit lint for Claude Code configuration
    Doctor {
        /// Target directory (resolves project-scope state for this path)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Auto-resolve findings marked auto-fixable
        #[arg(long)]
        fix: bool,

        /// Write even if a `claude` process appears to be running
        #[arg(long)]
        force: bool,

        /// Unmask secret values in the output (dangerous)
        #[arg(long = "show-secrets")]
        show_secrets: bool,
    },
    /// Resolve every config surface for a target directory with provenance
    Show {
        /// Target directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Unmask secret values in the output (dangerous)
        #[arg(long = "show-secrets")]
        show_secrets: bool,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.color {
        ColorMode::Always => control::set_override(true),
        ColorMode::Never => control::set_override(false),
        ColorMode::Auto => {
            if cli.json {
                control::set_override(false);
            }
        }
    }

    let env = paths::Env::new(cli.config.clone(), cli.claude_home.clone());

    let result = match cli.command {
        Command::Prune {
            apply,
            worktrees_only,
            force,
        } => prune::run(
            &env,
            prune::Options {
                apply,
                worktrees_only,
                force,
                json: cli.json,
            },
        ),
        Command::Doctor {
            ref path,
            fix,
            force,
            show_secrets,
        } => doctor::run(
            &env,
            doctor::Options {
                path: path.clone(),
                fix,
                force,
                show_secrets,
                json: cli.json,
            },
        ),
        Command::Show {
            ref path,
            show_secrets,
        } => show::run(
            &env,
            show::Options {
                path: path.clone(),
                show_secrets,
                json: cli.json,
            },
        ),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "midden", &mut std::io::stdout());
            return ExitCode::SUCCESS;
        }
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            if cli.json {
                let err = serde_json::json!({ "error": format!("{e:#}") });
                eprintln!("{err}");
            } else {
                eprintln!("error: {e:#}");
            }
            ExitCode::from(2)
        }
    }
}
