# midden

[![CI](https://github.com/starhaven-io/midden/actions/workflows/ci.yml/badge.svg)](https://github.com/starhaven-io/midden/actions/workflows/ci.yml)
[![License: AGPL-3.0-only](https://img.shields.io/badge/License-AGPL--3.0--only-blue.svg)](LICENSE)

A CLI tool that resolves, audits, and garbage-collects the heap of state Claude Code accumulates.

The name: **midden**, an archaeological term for a refuse heap — kitchen scraps, broken pottery, lost things — that tells you what life was like in the layer below. `~/.claude.json` is exactly that.

## Why

For installing skills, managing MCP server lists, or browsing the marketplace, plenty of tools already exist. Use those.

midden picks up where they leave off. Claude Code writes a lot of state across many files — `~/.claude.json`'s `projects` map appended to on every visit and never pruned, settings layered across user/project/local/managed scopes with non-obvious precedence, ephemeral worktrees with adjective-scientist names, memory files (`CLAUDE.md`) loading from many locations simultaneously — and almost nothing in the loop *removes* any of it. midden surfaces what's actually active for a given directory with provenance, flags what's stale or leaking, and prunes the state nothing else cleans up.

## Installation

### Homebrew

```bash
brew install starhaven-io/tap/midden
```

### crates.io

```bash
cargo install midden
```

### From releases

Download a prebuilt binary from [GitHub Releases](https://github.com/starhaven-io/midden/releases).

### From git (unreleased HEAD)

To try unreleased changes from `main`:

```bash
cargo install --git https://github.com/starhaven-io/midden
```

## Usage

All commands default to safe modes: dry-run for prune, read-only for show, and report-only for doctor. Writes always require an explicit flag, create a timestamped backup first, and replace the file atomically, preserving its file mode. Use `--json` for machine-readable output.

```bash
# Show what's actually active for a directory with provenance
midden show

# Target a specific repo
midden show /path/to/repo

# Hygiene + audit lint
midden doctor

# Auto-resolve safe findings (orphaned projects, etc.)
midden doctor --fix

# Garbage-collect dead `projects` entries (dry-run)
midden prune

# Apply the prune
midden prune --apply

# Only consider ephemeral worktree entries
midden prune --worktrees-only --apply

# Override the safety gate when claude is running
midden prune --apply --force

# Unmask secret-looking values in output (dangerous)
midden show --show-secrets

# Generate shell completions
midden completions zsh
```

### Show

Resolve every configuration surface for a target directory with provenance:

```
$ midden show .
resolved for /Users/me/myproject

settings
  permissions.defaultMode = "bypass"
    [user shadowed] /Users/me/.claude/settings.json = "ask"
    [project] /Users/me/myproject/.claude/settings.json = "bypass"
  permissions.deny = ["Bash(rm:*)", "Read(./.env)"]
    [user merged] /Users/me/.claude/settings.json = ["Bash(rm:*)"]
    [project merged] /Users/me/myproject/.claude/settings.json = ["Read(./.env)"]

CLAUDE.md
  [user] /Users/me/.claude/CLAUDE.md (8421 bytes)
  [project] /Users/me/myproject/CLAUDE.md (10442 bytes)

hooks
  PreToolUse
    [local] Bash (command): bash -c '… DCO sign-off check …'
      /Users/me/myproject/.claude/settings.local.json

mcp servers
  [user] github -> https://api.githubcopilot.com/mcp
    /Users/me/.claude.json
  [project] astro-docs -> https://mcp.docs.astro.build/mcp
    /Users/me/myproject/.mcp.json
```

Settings precedence is **Managed → Local → Project → User**. Scalars from a higher scope override; arrays concat and deduplicate across scopes. `show` tags every value with its source and marks contributions shadowed by a higher scope. `CLAUDE.md` files do not follow precedence — all applicable files load simultaneously, so midden lists every contributor and runs a heuristic contradiction-detection pass instead of picking a winner.

Secret-looking keys (`*_token`, `*_api_key`, `password`, `credential`, …) are masked to `abcd***` by default. Pass `--show-secrets` to unmask.

### Doctor

Hygiene + audit lint over the layered state, emitting structured `Finding`s:

```
$ midden doctor
warn [missing-credential-deny] no deny rule covers .env, secrets — Claude could read credentials here
  at /Users/me/.claude/settings.json:permissions.deny
  fix: add e.g. "Read(./.env)", "Read(./.env.*)", "Read(./secrets/**)" to permissions.deny in ~/.claude/settings.json to cover every project

warn [orphaned-project] worktree directory no longer exists: /Users/me/Developer/foo/.claude/worktrees/witty-curie (auto-fixable)
  at /Users/me/.claude.json:projects./Users/me/Developer/foo/.claude/worktrees/witty-curie
  fix: remove this entry with `midden prune --apply`

0 error, 2 warn, 0 info — 1 auto-fixable
```

Run with `--fix` to apply auto-fixable findings. As with prune, this writes a timestamped backup first and refuses to write while a `claude` process is running (override with `--force`).

### Prune

Garbage-collect dead `projects` entries from `~/.claude.json` (dry-run by default):

```
$ midden prune
43 project entries total; 24 orphaned (13 worktree, 11 other):

  - /Users/me/Developer/Brewy
  - /Users/me/Developer/Brewy/.claude/worktrees/competent-fermat   [worktree]
  - /Users/me/Developer/macOSdb
  …

would shrink .claude.json by ~20.0 KB (69.9 KB -> 49.8 KB).

dry run. re-run with --apply to remove these entries.
quit all Claude Code sessions first; it rewrites this file live.
```

An entry is a removal candidate only if its directory is provably absent from disk — a path that merely *fails to stat* (permission denied, an unreachable mount) is kept, and midden never guesses from value contents. `--worktrees-only` restricts to entries under a `.claude/worktrees/` path. If nearly all entries resolve missing — usually a sign you are on a different machine or an unmounted volume rather than that they are all dead — `prune --apply` refuses unless you pass `--force`.

## What doctor checks

| ID | Severity | Auto-fixable | What it catches |
|----|----------|--------------|-----------------|
| `orphaned-project` | Warn | yes | `projects` entry whose directory no longer exists |
| `claude-json-bloat` | Info | no | `~/.claude.json` over 512 KB (Claude Code never prunes it) |
| `stale-worktree` | Info | no | Ephemeral worktree dir untouched for >30 days |
| `secret-in-committed-settings` | Error | no | Suspect secret in a `settings.json` that git doesn't ignore (masked by default) |
| `local-settings-tracked` | Warn | no | `settings.local.json` tracked by git (meant to stay machine-local) |
| `local-settings-not-ignored` | Warn | no | `settings.local.json` not gitignored — one `git add` from being committed |
| `missing-credential-deny` | Warn | no | No `permissions.deny` covers `.env` or `secrets/` paths |
| `skill-missing-skill-md` | Warn | no | Skill directory missing its `SKILL.md` |
| `empty-config-file` | Warn | no | Slash command or subagent markdown file is empty |
| `mcp-server-unreachable` | Warn | no | MCP server defined with no `command` or `url` |
| `mcp-server-disabled` | Info | no | MCP server defined but `disabled: true` |

## Configuration

midden does not yet have a config file — all behavior is controlled by CLI flags. Settings precedence and CLAUDE.md merge rules come from Claude Code itself, not midden.

| Flag | Purpose |
|------|---------|
| `--config <PATH>` | Override the path to `~/.claude.json` (for testing) |
| `--claude-home <PATH>` | Override the path to `~/.claude/` (for testing) |
| `--json` | Emit machine-readable JSON instead of styled text |
| `--color auto\|always\|never` | Control color output |
| `--show-secrets` | Unmask secret-looking values in `show` / `doctor` output |
| `--force` | Allow writes while a `claude` process is running, and override the mass-deletion guard |

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Clean — no findings, or successful apply |
| 1 | Findings present (doctor with errors) |
| 2 | Error — bad input, missing file, write blocked by running claude |

## Building

A [justfile](https://github.com/casey/just) provides common tasks:

```bash
just build          # Build the project
just build-release  # Build in release mode
just test           # Run tests
just clippy         # Run clippy
just fmt            # Format code
just typos          # Check for typos
just deny           # cargo-deny: license + advisory + source checks
just lychee         # Check README links
just audit          # Audit GitHub Actions workflows (zizmor)
just check          # Run all checks
just install-hooks  # Install git hooks: pre-push check + DCO sign-off (once per clone)
```

## Contributing

Commits must follow [Conventional Commits](https://www.conventionalcommits.org/) format and include a DCO sign-off (`git commit -s`). Run `just install-hooks` once per clone to enable the git hooks (a pre-push `just check` and DCO sign-off enforcement).

## Acknowledgements

Built with [Claude Code](https://claude.ai/code).

## License

This project is licensed under the [GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`).

Copyright (C) 2026 Patrick Linnane
