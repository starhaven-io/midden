# Task Group: Midden dual-provider memory inventory

scope: Inventory Codex and Claude memory without changing provider state.
applies_to: cwd=/workspace/midden; reuse_rule=provider parity and bounded reads are reusable.

## Task 1: Implement the shared inventory

### rollout_summary_files

- rollout_summaries/2026-07-21-memory-inventory.md (cwd=/workspace/midden, rollout_path=/codex/sessions/rollout-memory-inventory.jsonl, updated_at=2026-07-21T04:50:33+00:00, thread_id=00000000-0000-4000-8000-000000000001, success)

### keywords

- midden, codex, claude, memory inventory

## Task 2: Refine the evidence model

### rollout_summary_files

- rollout_summaries/2026-07-22-memory-evidence.md (cwd=/workspace/midden, rollout_path=/codex/sessions/rollout-memory-evidence.jsonl, updated_at=2026-07-22T04:50:33+00:00, thread_id=00000000-0000-4000-8000-000000000003, success)

## Reusable knowledge

- Public memory features require paired Codex and Claude coverage.

# Task Group: Unrelated repository maintenance

scope: Keep an unrelated fixture item separate.
applies_to: cwd=/workspace/other; reuse_rule=fixture-only.

## Task 1: Update a dependency

### rollout_summary_files

- rollout_summaries/2026-07-22-other-maintenance.md (cwd=/workspace/other, rollout_path=/codex/sessions/rollout-other.jsonl, updated_at=2026-07-22T04:50:33+00:00, thread_id=00000000-0000-4000-8000-000000000002, success)
