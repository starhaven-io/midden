# Security Policy

## Reporting a vulnerability

Report vulnerabilities privately through [GitHub security advisories for this
repository](https://github.com/starhaven-io/midden/security/advisories/new).
Do not open a public issue for a security report.

midden reads local agent state from `~/.claude.json`, `~/.claude/`, and
`~/.codex/`, and behind explicit flags rewrites the Claude Code state in
`~/.claude.json` and `~/.claude/`. Codex state is read, never written.
Reports in these areas are especially valuable:

- secret-masking bypasses (`--show-secrets` is the only sanctioned unmasking
  path in terminal or JSON output);
- writes that escape the backup + atomic-write + running-writer gate
  discipline, or that broaden file permissions;
- parser abuse through adversarial transcript, memory, or instruction content
  (JSONL heads, Markdown frontmatter and fences, `@` imports).

## Supported versions

Only the latest release receives security fixes.
