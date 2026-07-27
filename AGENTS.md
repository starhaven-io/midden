# Agent Instructions for midden

midden is a Rust CLI for resolving, auditing, visualizing, and cleaning the context and state Codex and Claude Code accumulate. Its provider-neutral memory inventory makes generated memories, repository instructions, and session evidence legible with provenance while preserving each provider's native behavior. Its existing Claude-specific commands resolve layered configuration, audit hygiene, and garbage-collect `~/.claude.json` project entries and orphaned transcript artifacts. It is deliberately *not* another MCP-server-list editor.

## Project overview

- **Language:** Rust, 2024 edition, MSRV 1.95.
- **Platforms:** macOS and Linux. Windows is out of scope for v0 (a `cfg(windows)` branch may exist but is untested/unsupported).
- **License:** AGPL-3.0-only.
- **Binary:** one CLI; subcommands `memory show`, `prune`, `doctor`, `show`, plus `completions` for shell completion.
- **Deps:** clap/clap_complete, serde/serde_json (with `preserve_order`), walkdir, colored, anyhow, time, sysinfo. Error handling is anyhow throughout — there are no `thiserror` types.

## Project-specific notes

### Commands

- `memory show [PATH] [--provider all|codex|claude] [--all]` — inventory Codex and Claude Code instructions, retained memory, and evidence stores through one normalized read-only schema while preserving provider-native loading behavior. The default provider is `all`; `--all` includes unrelated and unassociated sources.
- `prune [--apply] [--transcripts] [--worktrees-only] [--force]` — GC dead `projects` entries from `~/.claude.json`. Dry-run by default; removes only entries whose directory is provably absent. `--transcripts` additionally cleans orphaned session artifacts under `~/.claude/projects/` by deriving cwd from JSONL heads, never from lossy slugs. `--apply` backs up then writes `.claude.json`; transcript deletion deliberately does not create backups. `--worktrees-only` restricts to `.claude/worktrees/` paths; `--force` overrides the write gates.
- `doctor [PATH] [--fix] [--force] [--show-secrets]` — emit structured `Finding { id, severity, location, message, suggested_fix, auto_fixable }`. `--fix` applies the `auto_fixable` findings under the same backup + atomic-write discipline as prune (today only `orphaned-project` is auto-fixable). See the README for the full check list.
- `show [PATH] [--show-secrets]` — resolve every config surface for a directory with provenance: settings (with shadow/merge tags), every contributing `CLAUDE.md`, skills, commands, subagents, hooks, MCP servers (user/local/project/managed — local being the per-project `mcpServers` map inside `~/.claude.json`), worktrees.

Global flags: `--json` (machine output; disables color), `--color auto|always|never`, `--config <PATH>` (override `~/.claude.json`), `--claude-home <PATH>` (override `~/.claude`), `--codex-home <PATH>` (override `$CODEX_HOME` / `~/.codex`). The three override flags exist for testing — integration tests point them at fixture dirs.

### Gotchas

- `~/.claude.json` is the central, live file Claude Code rewrites constantly — the highest-stakes thing midden touches. All mutations go through backup → atomic write → the running-claude gate.
- Orphan detection is existence-only (`fs::metadata`): only `NotFound` / `NotADirectory` (or an existing non-directory) count as provably absent — any other stat failure (permission denied, I/O error, dead mount) means "can't tell" and the entry is kept. "Not visible on this host" ≠ "dead", which is why the mass-deletion gate also exists.
- `git.rs` shells out; treat `None` as "can't tell" (not a repo / no git), never as "false".
- Integration tests isolate `HOME`, `XDG_CONFIG_HOME`, and `GIT_CONFIG_NOSYSTEM` so a developer's global gitignore can't sway doctor's git checks — preserve that in `tests/common/mod.rs`.

### CI workflows (`.github/workflows/`)

`ci.yml` (dynamic matrix: conventional-commit check, lint, test on Linux/Linux-ARM/macOS, coverage to Codecov, zizmor on workflow changes); `release.yml` (manual dispatch: cross-platform signed + notarized binaries, build-provenance attestation, crates.io publish via OIDC, Homebrew cask bump); `codeql.yml`, `zizmor.yml`, `pinprick-audit.yml` (dogfood), `link-check.yml`; `cargo-deny.yml` (Monday scheduled/dispatch advisory drift scan that opens or updates a tracking issue when the result flips); weekly `bump-cargo-tools.yml`. Third-party actions are SHA-pinned and workflows are least-privilege (`permissions: {}` at the top, granted per-job). `release.yml` deliberately omits shell `-x` so secrets don't leak into public logs.

### Dependency automation

Dependency automation is deliberately split. `.github/dependabot.yml` is fleet-rendered and owns Cargo manifests and GitHub Actions. `renovate.json` opts this repository into the shared preset at a pinned `local>starhaven-io/.github:renovate-config#<fleet-release>` reference, which owns the pins no Dependabot ecosystem covers: the `rust-toolchain.toml` channel and the `cargo install <tool> --locked --version <version>` pins in `ci.yml` and `cargo-deny.yml`. midden is the pilot consumer — the first repository to adopt Renovate in the estate.

`bump-cargo-tools.yml` is a **temporary parallel fallback**, not dead code. It keeps running on its existing weekly Monday schedule until the repository maintainer records pilot signoff in a separate removal PR. Signoff requires a hosted run log extracting Rust plus all five cargo-pin occurrences, a merged and passing `rust-toolchain` PR, a merged and passing cargo-install PR, and proof that both `cargo-deny` occurrences move together. Until then expect overlap: the bump workflow has no cooldown, so it can beat Renovate's seven-day minimum release age to a new version and can conflict with a Renovate branch on the same lines. "Renovate opened no PR this week" is therefore not on its own evidence of a Renovate fault. Do not resolve a Renovate PR by editing `bump-cargo-tools.yml`, and do not extend that workflow to cover new tools.

Three standing constraints:

- The preset pin moves only through a pull request here. A change to `renovate-config.json` in `starhaven-io/.github` reaches this repository only once a fleet release tags it and a pull request advances the `#<fleet-release>` reference to that exact tag. Never drop the tag or point the reference at a branch: an unpinned preset would let hub `main` change dependency behavior with no review. Revalidate with `renovate-config-validator --strict --no-global renovate.json` whenever the pin moves, remembering that validation does not resolve the reference and so cannot tell you the tag exists or contains the preset.
- The hosted Mend app must stay installed with **Only select repositories**. Expand that selection one adopter at a time; an org-wide installation would onboard every repository at once, including private `orrery`.
- Before a second repository adopts Renovate, the consumer stub should become a fleet-rendered file rather than a hand-copied one. Do not hand-copy this `renovate.json` into another repository.

## Repository structure

- `main.rs` — clap CLI definition and dispatch.
- `prune.rs` — prune command.
- `doctor.rs` — doctor command: the `Finding` lint and its checks.
- `show.rs` — show command. `resolve_settings` is the single source of truth for settings precedence + provenance.
- `orphans.rs` — orphan detection (shared by prune + doctor) and `looks_like_wrong_host` (the mass-deletion heuristic).
- `transcripts.rs` — transcript cwd derivation and artifact cleanup under `~/.claude/projects/`; deletes only `*.jsonl` and UUID session dirs, never `memory/`.
- `claude_json.rs` — read/render `~/.claude.json` (order-preserving) and `write_atomic`.
- `paths.rs` — user/project/managed path helpers. `managed_settings_paths()` are hardcoded system paths (not overridable — so managed-scope file discovery isn't reachable from integration tests yet).
- `backup.rs` — timestamped sibling copy taken before any write.
- `process.rs` — detect a running `claude` process for the write gate.
- `secrets.rs` — sensitive-key detection (`key_looks_sensitive`) and masking (`mask`, `mask_value`).
- `git.rs` — `is_tracked` / `is_ignored` via the `git` CLI; both return `None` when git is unavailable or the path isn't in a repo.
- `output.rs` — KB formatting.

Tests: `tests/{prune,doctor,show}.rs` (integration, via `assert_cmd`) + `tests/common/mod.rs` (the `Fixture` helper) + `#[cfg(test)]` unit modules in `src/`.

### Settings precedence (the `show` model)

`settings.json`, highest scope to lowest: **Managed → Local → Project → User**. Scalars: highest scope wins, lower contributions are tagged `shadowed`. Arrays: concatenate + dedupe across all scopes (no shadowing). This mirrors Claude Code's documented behavior — arrays merge, scalars override, objects deep-merge — and Managed cannot be overridden. `resolve_settings` in `show.rs` is the single source of truth; keep it so.

There is **no** "command-line args" settings scope — the `Scope` enum is exactly `User | Project | Local | Managed`.

### CLAUDE.md handling

CLAUDE.md does **not** follow settings precedence. All applicable files load simultaneously; midden does a no-precedence merge and runs a heuristic contradiction-detection pass instead of picking a winner. The one ordering exception: `CLAUDE.local.md` loads after `CLAUDE.md` in the same directory.

## Safety / do-not-touch rules

- **Read-only by default.** Mutation requires an explicit flag (`--apply` / `--fix`).
- **Backup before every write.** `backup::timestamped_copy` writes a `.bak-YYYYMMDD-HHMMSS` sibling (suffix-bumped on a same-second collision) before any mutation.
- **Transcript deletion is the backup exception.** `prune --transcripts --apply` deletes append-only `*.jsonl` logs and UUID session artifact dirs without making `.bak` copies, because backing up hundreds of MB defeats GC. It never deletes `memory/` and still uses dry-run, running-`claude`, and mass-deletion gates.
- **Atomic writes.** `claude_json::write_atomic` streams to a temp sibling, fsyncs, then renames over the target — never an in-place truncating write of `~/.claude.json`. The temp mirrors the target's file mode (owner-only `0600` for new files) so a rewrite never broadens permissions on a credential-bearing file; backups are clamped to `0600` likewise.
- **Running-`claude` gate.** Refuses to write while a `claude` process runs (Claude Code rewrites the file live). `--force` overrides.
- **Mass-deletion gate.** Refuses to prune when ≥90% of a non-trivial `projects` map resolves missing — that usually means the wrong host or an unmounted volume, not real orphans. `--force` overrides.
- **Conservative deletion.** Removes only entries whose directory is provably absent; never guesses from value contents. Apply re-reads and re-derives immediately before writing, so concurrent edits to unrelated keys survive.
- **Mask secrets by default.** Sensitive-named keys (`token`, `secret`, `password`, `apikey`, `auth`, `credential`, …) have their string values masked — including strings nested in arrays/objects under such a key. `--show-secrets` opts out.
- `doctor`'s `Error`-severity secret findings (`secret-in-committed-settings`, `secret-in-committed-mcp`, `secret-in-malformed-config`) are suppressed for git-ignored files.
- **`--json` on every command** for CI/scripting.

## Required checks

`rust-toolchain.toml` pins CI and rustup-based workstations to the reviewed
stable toolchain. Homebrew's standalone Rust does not honor that file, so verify
`rustc --version` matches its `channel` before running the required checks.

`just` recipes (raw command in parens):
- `just build` / `just test` (`cargo build --locked` / `cargo test --locked`)
- `just clippy` — `cargo clippy --locked --all-targets -- -D warnings` (zero warnings required)
- `just fmt` / `just fmt-check` — rustfmt, 2024 style edition (`rustfmt.toml`)
- `just typos`, `just deny` (`cargo deny check`), `just lychee`, `just audit` (zizmor)
- `just check` — run everything; skips tools that aren't installed
- Errors: anyhow throughout (`Result`, `.with_context`, `bail!`). Flat modules.
  Comments explain *why*, not *what*.

### Exit codes

`0` clean / successful apply · `1` doctor found an `Error`-severity finding · `2` error (bad input, missing file, or a write blocked by a gate).

<!-- fleet:block commit-and-pr-conventions -->

## Commit and PR conventions

- Conventional Commits: `type(scope): description`. Valid types: `feat`,
  `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`.
- Sign off every commit with `git commit -s` for DCO (enforced by the
  `.githooks/commit-msg` hook; run `just install-hooks` once per clone to
  enable it).
- When authored with an AI coding agent, add a `Co-authored-by` trailer before
  `Signed-off-by` (git-native order: `git commit -s` appends the sign-off last),
  naming the agent and model. Current example:
  `Co-authored-by: Claude Opus 5 <noreply@anthropic.com>`. Bump the model
  version as newer ones ship.
- Never commit directly to `main`; create a feature branch and open a PR.
- PR descriptions should contain only a concise summary of changes. Do not add
  test-plan sections, bot attribution, or generated-with footers.
- Keep each prose paragraph in a PR description on one source line. Do not
  hard-wrap PR body prose like a commit message; preserve intentional Markdown
  line breaks in lists, code blocks, and other structured content.
- Comments must earn their keep: a comment states a constraint or rationale the
  code cannot express. Never add comments that narrate what the code does,
  restate names, or explain a change to its reviewer.

<!-- fleet:end -->
