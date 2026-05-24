# midden — Claude Project Context

midden is a CLI for resolving, auditing, and garbage-collecting the state Claude Code accumulates. It exists because Claude Code writes a lot — `~/.claude.json`'s `projects` map grows forever, settings sprawl across four scopes with non-obvious precedence, ephemeral worktrees pile up, MCP servers and skills and slash commands and subagents live in their own places — and almost nothing has a lifecycle.

The product is deliberately not another MCP-server list editor. The three things nobody else does well are: **resolve** (what's actually active for this directory, with provenance), **audit** (what's stale, misconfigured, or leaking), **clean** (GC the state Claude Code writes but never prunes).

## Project overview

- **Language:** Rust (2024 edition)
- **Platform:** macOS, Linux (Windows out of scope for v0)
- **Architecture:** Single binary CLI with three subcommands (`prune`, `doctor`, `show`)
- **License:** AGPL-3.0-only
- **Dependencies:** clap/clap_complete (CLI), serde/serde_json with `preserve_order` (config parsing), walkdir (file discovery), globset (path patterns), colored (terminal output), time (timestamped backups), sysinfo (running-process detection)

## Repository structure

```
midden/
├── Cargo.toml
├── src/
│   ├── main.rs              # Entry point, clap CLI definition, command dispatch
│   ├── prune.rs             # Prune command — GC orphaned `projects` entries
│   ├── doctor.rs            # Doctor command — structured Finding lint
│   ├── show.rs              # Show command — multi-scope resolver with provenance
│   ├── orphans.rs           # Orphan detection, reusable by prune + doctor
│   ├── claude_json.rs       # ~/.claude.json read/render (preserves key order)
│   ├── paths.rs             # Path helpers (user + project + managed scopes)
│   ├── backup.rs            # Timestamped sibling copy before any write
│   ├── process.rs           # Detect a running `claude` for the write-safety gate
│   ├── secrets.rs           # Mask suspect keys (token, api_key, password…)
│   └── output.rs            # Small presentation helpers (KB formatting)
├── tests/
│   ├── common/mod.rs        # Fixture helpers shared across integration tests
│   ├── prune.rs             # Prune integration tests (incl. acceptance)
│   ├── doctor.rs            # Doctor integration tests
│   └── show.rs              # Show integration tests
├── scripts/
│   └── format-release-notes.py  # Categorize gh-generated release notes by conventional commit type
├── .github/
│   ├── workflows/           # CI, CodeQL, zizmor, release, link-check, dependabot lockfile refresh, cargo-tools bump, pinprick-audit
│   ├── dependabot.yml       # Cargo + Actions dependency updates
│   └── FUNDING.yml
├── justfile                 # Task runner (build, test, lint, check)
├── rustfmt.toml             # 2024 style edition
├── deny.toml                # cargo-deny config
├── lychee.toml              # Link checker config
├── _typos.toml              # typos-cli config
└── .gitignore
```

## Architecture

### Commands

- `midden prune [--apply] [--worktrees-only] [--force]` — garbage-collect dead `projects` entries from `~/.claude.json`. Dry-run by default. Removes only entries whose directory is provably absent. `--apply` writes a timestamped backup, then mutates. `--worktrees-only` restricts to `.claude/worktrees/`-pathed entries. `--force` allows writing while `claude` is running.
- `midden doctor [PATH] [--fix] [--force] [--show-secrets]` — emits structured `Finding { id, severity, location, message, suggested_fix, auto_fixable }`. Starter checks: orphaned `projects` entries, oversized `.claude.json`, stale worktree directories, secrets in committed `settings.json` (masked by default), missing credential `permissions.deny` coverage, broken/disabled MCP servers, skill directories without `SKILL.md`, empty command/agent files. `--fix` applies the `auto_fixable: true` findings under the same backup discipline as prune.
- `midden show [PATH] [--show-secrets]` — resolves every config surface for a target directory with provenance. Settings: scalars override (highest scope wins, lowers marked `shadowed`); arrays concat + dedupe across scopes. Lists every contributing `CLAUDE.md` (user, project, ancestor, local, nested) and runs a heuristic contradiction-detection pass. Lists skills, slash commands, subagents, MCP servers, worktrees.

### Global flags

- `--json` — Output as JSON for CI integration. Auto-disables color.
- `--color auto|always|never` — Control color output.
- `--config <PATH>` — Override `~/.claude.json` location (testing).
- `--claude-home <PATH>` — Override `~/.claude` location (testing).
- `--version` / `-V` — Print version.

### Settings precedence

For `settings.json`, precedence from highest to lowest is **Managed → command-line args → Local → Project → User**. Scalars from a higher scope override lower ones. Arrays concatenate and deduplicate across scopes. Managed settings cannot be overridden by anything.

`show` faithfully implements this precedence and tracks provenance per dotted key path. The `resolve_settings` function in `src/show.rs` is the single source of truth.

### CLAUDE.md handling

CLAUDE.md does **not** follow the same precedence model. All applicable CLAUDE.md files load simultaneously and conflicts are not resolved by precedence. midden treats this as a no-precedence merge and runs a contradiction-detection pass instead of picking a winner. The only documented exception is that `CLAUDE.local.md` loads after `CLAUDE.md` in the same directory.

### Safety discipline

- **Read-only by default.** Every write requires an explicit flag (`--apply` or `--fix`).
- **Timestamped backup before any mutation.** `backup::timestamped_copy` writes `<file>.bak-YYYYMMDD-HHMMSS` next to the original.
- **Refuses to write if a `claude` process is running.** Claude Code rewrites `~/.claude.json` live and may stomp our changes. `--force` overrides.
- **Conservative deletion.** Prune removes only `projects` entries whose directory is provably absent. Never guesses from value contents.
- **Mask secrets by default.** Suspect key names (token, secret, password, apikey, auth, credential…) have their string values masked to `abcd***`. `--show-secrets` opts in.
- **`--json` on every command** for scripting and CI.

### Exit codes

- `0` — clean (no findings, no orphans, or successful apply)
- `1` — findings present (doctor with errors)
- `2` — error (bad input, file not found, write blocked by running claude)

## Code style and conventions

- `cargo clippy --all-targets -- -D warnings` with zero warnings
- `cargo fmt` for formatting (2024 style edition, see `rustfmt.toml`)
- Flat module structure, no nested directories
- `thiserror` for typed errors in library code, `anyhow` for context-rich error propagation in commands
- Comments only when the *why* is non-obvious; let well-named identifiers explain the *what*

## CI workflows (.github/workflows/)

- **ci.yml** — Dynamic matrix PR checks: conventional commits, lint (clippy + rustfmt + typos + deny), test (Linux, Linux ARM, macOS), zizmor on workflow changes
- **codeql.yml** — CodeQL Actions analysis on push to main
- **zizmor.yml** — GitHub Actions security audit on push to main
- **pinprick-audit.yml** — Run pinprick on midden's own workflows with SARIF upload (dogfooding)
- **link-check.yml** — Weekly lychee scan of README links
- **release.yml** — Manual dispatch: cross-platform binaries (linux-amd64, linux-arm64, darwin-arm64) with macOS signing + notarization, build provenance attestations, GitHub release with formatted notes, crates.io publish, Homebrew cask bump
- **bump-cargo-tools.yml** — Weekly: scan crates.io for newer versions of `typos-cli` and `cargo-deny`, open a PR if any are stale
- **refresh-cargo-lockfile.yml** — Weekly `cargo update` to pick up transitive bumps Dependabot ignores

## Commit conventions

Conventional Commits format: `type(scope): description`

Common types: `feat`, `fix`, `refactor`, `docs`, `ci`, `chore`, `perf`, `test`, `style`, `build`

All commits must:
- Use `git commit -s` for DCO sign-off
- Include a `Co-authored-by: Claude Opus 4.7 <noreply@anthropic.com>` trailer when authored with Claude

## Git workflow

- Never commit directly to main — always create a feature branch and open a PR
- PR descriptions should contain only a summary of the changes — no test plan sections, no bot attribution, no "Generated with Claude Code" footers
