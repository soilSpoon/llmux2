# OpenSpec in llmux2

llmux2 manages its living specifications with [OpenSpec](https://openspec.dev/).

## Layout

- `openspec/project.md` — project context, goals, and non-goals.
- `openspec/specs/<capability>/spec.md` — the current truth for each
  capability. A spec describes what the code does now or what is decided and
  planned; it is not a wish list.
- `openspec/changes/<change-id>/` — proposed changes before they land, each
  with `proposal.md`, `tasks.md`, optional `design.md`, and spec deltas.

## Working with specs

- When behavior or a decided requirement changes, update the affected spec in
  the same PR (or through a change proposal under `openspec/changes/`).
- Requirements use `### Requirement:` with SHALL language and `#### Scenario:`
  examples in WHEN/THEN form.
- If `openspec` CLI is available, prefer `openspec validate` and the
  propose/apply/archive workflow; otherwise edit these files directly and keep
  the same format.
