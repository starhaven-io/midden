# Security Policy

## Reporting a vulnerability

Report vulnerabilities privately through [GitHub security advisories for this
repository](https://github.com/starhaven-io/midden/security/advisories/new).
Do not open a public issue for a security report.

midden reads local agent state from `~/.claude.json`, `~/.claude/`, and
`~/.codex/`. Behind explicit flags it atomically rewrites `~/.claude.json` and
can delete selected orphaned transcript artifacts under `~/.claude/projects/`.
Codex state is read, never written.
Reports in these areas are especially valuable:

- secret-masking bypasses (`--show-secrets` is the only sanctioned unmasking
  path in terminal or JSON output);
- `~/.claude.json` writes that escape the backup + atomic-write +
  running-writer gate discipline, or that broaden file permissions;
- transcript deletions that escape anchored directory handles, remove durable
  `memory/`, or ignore a changed file identity or newly live project path;
- parser abuse through adversarial transcript, memory, or instruction content
  (JSONL heads, Markdown frontmatter and fences, `@` imports), including
  unbounded reads, special files, symlink escape, or terminal-control output;
- incorrect provider provenance, especially treating unresolved Codex trust,
  sampled Claude transcript evidence, or unapproved/unresolved external imports
  as confirmed active.

All configuration and instruction reads are byte-bounded and require a regular
file after opening. Secret masking is applied at both typed fields and final
human/JSON output boundaries. `--show-secrets` is the only sanctioned secret
unmasking path; it does not disable terminal-control escaping.

## Supported versions

Only the latest release receives security fixes.
