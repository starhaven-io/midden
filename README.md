# midden

<!-- fleet:block badges -->

[![CI](https://github.com/starhaven-io/midden/actions/workflows/ci.yml/badge.svg)](https://github.com/starhaven-io/midden/actions/workflows/ci.yml)
[![License: AGPL-3.0-only](https://img.shields.io/badge/License-AGPL--3.0--only-blue.svg)](LICENSE)

<!-- fleet:end -->

A CLI tool that resolves, audits, visualizes, and cleans the heap of context and state Codex and Claude Code accumulate.

The name: **midden**, an archaeological term for a refuse heap — kitchen scraps, broken pottery, lost things — that tells you what life was like in the layer below. Agent memory and `~/.claude.json` are exactly that.

## Why

For installing skills, managing MCP server lists, or browsing the marketplace, plenty of tools already exist. Use those.

midden picks up where they leave off. Coding agents accumulate generated memories, layered repository instructions, session evidence, project mappings, local settings, and ephemeral worktrees, while almost nothing in the loop makes that state legible or removes it. midden surfaces what's actually active for a directory with provenance, flags what's stale or leaking, and prunes the state nothing else cleans up.

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

Download a prebuilt binary from [GitHub Releases](https://github.com/starhaven-io/midden/releases). There is no committed CHANGELOG — release notes live on the GitHub Releases page. Binaries are available for:

- Linux amd64, glibc — `x86_64-unknown-linux-gnu`
- Linux arm64, glibc — `aarch64-unknown-linux-gnu`
- Linux amd64, musl (static) — `x86_64-unknown-linux-musl`
- Linux arm64, musl (static) — `aarch64-unknown-linux-musl`
- macOS Apple Silicon — `aarch64-apple-darwin`

The `gnu` builds link against the system glibc. They are built on Ubuntu 24.04, so they require **glibc 2.39 or newer** (Ubuntu 24.04+, Debian 13+, Fedora 40+). On older releases — Ubuntu 22.04, Debian 12, RHEL 9 and the like — use the statically linked `musl` builds instead. The `musl` builds carry no glibc requirement and also run on musl-based distributions such as Alpine, as well as minimal or distroless containers.

### From git (unreleased HEAD)

To try unreleased changes from `main`:

```bash
cargo install --git https://github.com/starhaven-io/midden
```

## Usage

All commands default to safe modes: dry-run for prune, read-only for show and memory inventory, and report-only for doctor. Rewriting `~/.claude.json` always requires an explicit flag, creates an owner-only timestamped backup first, and replaces the file atomically while preserving its file mode. A symlinked config is refused explicitly; pass `--config` with its resolved target when mutation is intended. Transcript cleanup is the documented backup exception because duplicating append-only logs would defeat garbage collection. Use `--json` for machine-readable output.

```bash
# Compare Codex and Claude memory sources for this repo
midden memory show

# Inspect only one provider
midden memory show --provider codex

# Include unrelated and unassociated memory sources
midden memory show --all

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

# Also report orphaned transcript artifacts under ~/.claude/projects/
midden prune --transcripts

# Remove dead project entries and orphaned transcript artifacts
midden prune --transcripts --apply

# Only consider ephemeral worktree entries
midden prune --worktrees-only --apply

# Override the safety gate when claude is running
midden prune --apply --force

# Unmask secret-looking values in output (dangerous)
midden show --show-secrets

# Generate shell completions
midden completions zsh
```

### Memory show

`memory show` resolves Codex and Claude Code through one normalized, read-only inventory while preserving each provider's native loading behavior:

```text
$ midden memory show .
memory for /Users/me/myproject

codex  memory unknown  management read-only
  instructions
    [repository; load unknown] /Users/me/myproject/AGENTS.md (2.4 KiB)
      configuration or project trust is unresolved; this source's load state depends on it
  retained memory
    [global; load unknown] /Users/me/.codex/memories/memory_summary.md (1.1 KiB)

claude  memory unknown  management read-only
  instructions
    [repository; loaded] /Users/me/myproject/CLAUDE.md (31 B)
    [repository; loaded] /Users/me/myproject/AGENTS.md (2.4 KiB)
      imported by /Users/me/myproject/CLAUDE.md
  retained memory
    [repository; load unknown] /Users/me/.claude/projects/example/memory/MEMORY.md (3.2 KiB)
      startup index loaded in full (within 200 lines and 25 KiB)

provider coverage
  codex: 1 instruction, 1 retained memory
  claude: 2 instructions, 1 retained memory
```

Codex discovery follows `AGENTS.override.md`, `AGENTS.md`, configured fallback names, and `project_root_markers` (defaulting to `.git`; an empty list makes the target directory the root). It resolves system, user, root-to-target project, `/etc/codex/managed_config.toml`, and `/etc/codex/requirements.toml` layers, then inventories the union of instruction candidates across named `<profile>.config.toml` and trusted/untrusted project alternatives. Obsolete inline `profile` / `[profiles.*]` keys are not applied. The active profile, one-off command-line overrides, cloud-managed requirements, and macOS MDM payloads are not observable from a filesystem inventory, so Codex memory and instruction load states remain unknown even when the visible disk layers agree. Generated summaries, the durable-memory index, and evidence stores are inventoried without recursively reading rollout history.

Claude discovery includes managed, user, ancestor, project, local, imported, and path-scoped instruction sources. Rule-directory symlinks are followed with bounded traversal, and project external imports use Claude's recorded approval state when available; imports stop after four hops. It associates default per-repository auto-memory with the target from transcript `cwd` evidence rather than decoding Claude's lossy project-directory slugs. Claude accepts `autoMemoryDirectory` from every settings scope; project and local values are reported as repository-associated candidates whose use depends on workspace trust, alongside the user/policy or default fallback that may apply when the workspace is untrusted. The `--settings` layer, environment disable, in-session memory toggle, workspace trust decision, and higher managed tiers are not observable here, so effective Claude memory state and location remain unknown. `MEMORY.md` is loaded in full only when it stays within both 200 lines and 25 KiB, otherwise reported as truncated, with topic files available on demand. Claude loads a `CLAUDE.md` through 4 MiB and skips a larger file; midden reports that boundary rather than treating its own bounded-read failure as a loaded provider source. Incomplete or conflicting transcript evidence produces an unknown association; `--all` exposes the affected sources.

The default provider is `all`. `--provider codex` or `--provider claude` filters the same schema to one adapter. `--all` additionally includes unrelated and unassociated sources for forensic work. Memory content is not printed; JSON output contains source metadata, loading state, association, capabilities, and warnings. Reads are bounded by source class and accept regular files only; oversized, malformed, trust-dependent, or inaccessible sources stay visible as disabled, truncated, or unknown instead of being silently omitted.

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
  [user; loaded] /Users/me/.claude/CLAUDE.md (8421 bytes)
  [project; loaded] /Users/me/myproject/CLAUDE.md (10442 bytes)

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

Settings precedence is **Managed → Local → Project → User**. Scalars from a higher scope override; arrays concat and deduplicate across scopes. `show` tags every value with its source and marks contributions shadowed by a higher scope. `CLAUDE.md` files do not follow precedence — all applicable files load simultaneously, so midden lists every contributor and runs a heuristic contradiction-detection pass instead of picking a winner. Claude loads files through 4 MiB and skips larger files; midden reports those larger sources as disabled and leaves an inaccessible file unknown without aborting the rest of `show`. Duplicate directives are collapsed, and the optional contradiction report is limited to 128 entries; JSON exposes `contradictions_truncated` when additional matches are omitted. See [Claude memory loading](https://code.claude.com/docs/en/memory#claudemd-files) for the provider contract.

Present but malformed or unreadable settings and MCP files make `show` fail with exit code 2 and name the affected path. They are never treated as an absent configuration layer. Unreadable managed settings directories follow the same rule. Inaccessible optional instruction, skill, command, agent, and worktree inventories emit masked, terminal-safe warnings on stderr while keeping the readable parts of the report; missing optional directories are normal.

Hooks show command, HTTP, MCP tool, prompt, and agent handlers with provenance. JSON retains the complete handler definition with secrets masked by default.

MCP servers are gathered from all four scopes: user (`~/.claude.json`), **local** (the per-project entry inside `~/.claude.json` — where `claude mcp add` writes by default), project (`.mcp.json`), and managed (`.claude/managed-mcp.json`).

Secrets are masked to `abcd***` by default — both by key name (`*_token`, `*_api_key`, `password`, `credential`, …) and by value shape under innocent keys: known token prefixes (`sk-`, `ghp_`, `xoxb-`, AWS key ids, JWTs, private-key blocks), `user:pass` URLs, credential-named query parameters, and `Bearer` tokens inside hook commands. Pass `--show-secrets` to unmask.

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

would shrink .claude.json by ~20.0 KiB (69.9 KiB -> 49.8 KiB).

dry run. re-run with --apply to remove these entries.
quit all Claude Code sessions first; it rewrites this file live.
```

An entry is a removal candidate only if its directory is provably absent from disk — a path that merely *fails to stat* (permission denied, an unreachable mount) is kept, and midden never guesses from value contents. `--worktrees-only` restricts to entries under a `.claude/worktrees/` path. If nearly all entries resolve missing — usually a sign you are on a different machine or an unmounted volume rather than that they are all dead — `prune --apply` refuses unless you pass `--force`.

Pass `--transcripts` to also inspect `~/.claude/projects/`, where Claude Code stores per-project session transcripts. These directories are named with lossy path slugs, so midden never decodes the directory name. Instead it reads only the head of each `*.jsonl` transcript and uses the first `cwd` field it can derive. A transcript directory is skipped if its transcripts disagree, any transcript lacks usable `cwd` evidence, or it has no `*.jsonl` files.

When a derived `cwd` is provably absent, `prune --transcripts` reports the session artifacts it would remove: `*.jsonl` files and bare UUID-named session artifact directories. `memory/` is durable user data and is never deleted. Apply revalidates identity, cwd evidence, and the complete artifact set, then atomically moves each selected artifact into a private random quarantine before rechecking its identity and unlinking it. If only `memory/` remains, the transcript project directory is kept and reported as memory preserved; unknown entries are left in place and reported as partially cleaned. A failed or interrupted deletion can leave remaining data in a `.midden-delete-*` directory inside that transcript project directory. Inspect that data before moving or removing it; a retry preserves these recovery directories. Unlike `.claude.json` rewrites, transcript deletion does not create `.bak` copies, because copying hundreds of MB of append-only logs would make cleanup impractical; the same dry-run, running-`claude`, mass-deletion, and `--force` gates still apply.

`prune --transcripts` also reports the largest kept transcript directories. Those are not removal candidates because their project directories still exist, but the inventory shows where live Claude Code history is consuming disk so retention-policy work can be deliberate rather than guesswork.

## What doctor checks

| ID | Severity | Auto-fixable | What it catches |
|----|----------|--------------|-----------------|
| `orphaned-project` | Warn | yes | `projects` entry whose directory no longer exists |
| `claude-json-bloat` | Info | no | `~/.claude.json` over 512 KiB (Claude Code never prunes it) |
| `orphaned-transcript` | Warn | no | Transcript directory whose derived cwd no longer exists |
| `claude-transcript-storage` | Info | no | Kept transcript history under `~/.claude/projects/` exceeds 64 MiB |
| `stale-worktree` | Info | no | Ephemeral worktree dir untouched for >30 days |
| `config-path-inaccessible` | Warn | no | Config/worktree path could not be inspected because of permissions or filesystem errors |
| `malformed-json-config` | Warn | no | Settings or MCP JSON could not be parsed, so key-aware checks were skipped |
| `secret-in-malformed-config` | Error | no | Token-shaped secret appears in malformed Git-tracked JSON |
| `secret-in-malformed-unignored-config` | Warn | no | Token-shaped secret appears in malformed untracked, non-ignored JSON |
| `secret-in-malformed-unverifiable-config` | Warn | no | Token-shaped secret appears in malformed JSON whose Git state cannot be established |
| `secret-in-committed-settings` | Error | no | Suspect secret in a Git-tracked `settings.json`, by key name, argument context, or value shape (masked by default) |
| `secret-in-unignored-settings` | Warn | no | Suspect secret in an untracked, non-ignored `settings.json` |
| `secret-exposure-unverifiable-settings` | Warn | no | Suspect settings secret whose Git state cannot be established |
| `secret-in-committed-mcp` | Error | no | Suspect secret in a Git-tracked `.mcp.json` / `managed-mcp.json`; pure `${VAR}` references are exempt |
| `secret-in-unignored-mcp` | Warn | no | Suspect secret in an untracked, non-ignored MCP JSON file |
| `secret-exposure-unverifiable-mcp` | Warn | no | Suspect MCP secret whose Git state cannot be established |
| `local-settings-tracked` | Warn | no | `settings.local.json` tracked by git (meant to stay machine-local) |
| `local-settings-not-ignored` | Warn | no | `settings.local.json` not gitignored — one `git add` from being committed |
| `missing-credential-deny` | Warn | no | No `permissions.deny` covers `.env` or `secrets/` paths |
| `skill-missing-skill-md` | Warn | no | Skill directory missing its `SKILL.md` |
| `empty-config-file` | Warn | no | Slash command or subagent markdown file is empty |
| `missing-frontmatter` | Warn | no | Subagent markdown file missing the required `---` YAML frontmatter (slash commands don't need it) |
| `mcp-server-unreachable` | Warn | no | MCP server defined with no `command` or `url` (any scope, including local) |
| `mcp-server-plaintext-http` | Warn | no | MCP server uses plaintext `http://` or `ws://` for a non-local URL (`localhost`, `*.localhost`, loopback, `0.0.0.0`, and `[::]` are local) |
| `mcp-server-disabled` | Info | no | MCP server defined but disabled — `disabled: true`, or listed in the project's `disabledMcpjsonServers` |
| `stale-mcp-approval` | Info | no | `enabledMcpjsonServers`/`disabledMcpjsonServers` names a server `.mcp.json` no longer defines |

## Configuration

midden does not have a config file — all behavior is controlled by CLI flags. Settings precedence and CLAUDE.md merge rules come from Claude Code itself, not midden.

| Flag | Purpose |
|------|---------|
| `--config <PATH>` | Override the path to `~/.claude.json` (for testing) |
| `--claude-home <PATH>` | Override the path to `~/.claude/` (for testing) |
| `--codex-home <PATH>` | Override `$CODEX_HOME` / `~/.codex/` (for testing) |
| `--json` | Emit machine-readable JSON instead of styled text |
| `--color auto\|always\|never` | Control color output |
| `--show-secrets` | Unmask secret-looking values in `show` / `doctor` output |
| `--force` | Allow writes while a `claude` process is running, and override the mass-deletion guard |

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Successful read, dry-run, or apply; doctor has no error-severity findings |
| 1 | Findings present (doctor with errors) |
| 2 | Error — bad input, missing file, write blocked by running claude |

When output is piped and the reader exits early (`midden show | head`), midden
dies of SIGPIPE like any other Unix filter — shells report that as 141, never
as a panic.

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
just lychee         # Check public documentation links
just audit          # Audit GitHub Actions workflows (zizmor)
just check          # Run the full local gate, including release-note tests
just install-hooks  # Install git hooks: pre-push check + DCO sign-off (once per clone)
```

## Contributing

Commits must follow [Conventional Commits](https://www.conventionalcommits.org/) format and include a DCO sign-off (`git commit -s`). Run `just install-hooks` once per clone to enable the git hooks (the full local `just check` gate and DCO sign-off enforcement). Hosted CI additionally runs platform, coverage-upload, link, and workflow checks selected for the changed paths.

The manual release workflow accepts only `main`. Unprivileged jobs validate the crate, build binaries, and format generated notes; separate no-checkout jobs sign or attest those exact artifacts. The publication job validates all five archives before minting its narrowly scoped GitHub App token. A retry accepts an existing tag only when it resolves to the same commit, byte-compares every existing release asset, uploads only missing assets, and refuses any non-identical or unexpected asset instead of overwriting published bytes. Because a newly timestamped macOS signature changes the archive bytes, do not rerun a successful macOS build/sign job after assets are published; rerun only failed downstream jobs while the original artifacts are retained. Crates.io publication skips an already published version. Homebrew reconciliation fetches its public inputs in a token-isolated step, then renders and validates a credential-free cask plan even when the version already matches. A no-checkout job uses the Git data API to write only that pre-hashed candidate, an unprivileged job reads the exact remote head back, and the merge job waits for checks before minting its token, revalidating the PR, and directly merging the same head SHA.

<!-- fleet:block license-section -->

## License

This project is licensed under the [GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`).

Copyright (C) 2026 Patrick Linnane

<!-- fleet:end -->
