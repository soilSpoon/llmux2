# Manage the gateway over HTTP, CLI, and MCP

## Why

Operators manage llmux2 today by editing `llmux.toml`, sending signals, and
reading the dashboard. There is no programmatic way to ask the running gateway
what it is doing, and no way to change routing state without a restart. Every
consumer that wants management data would otherwise grow its own ad-hoc
endpoint.

## What changes

- One shared management surface on the running server, exposed as a small
  authenticated HTTP admin API under `/admin`.
- A `llmux` CLI binary that talks to that API — a thin client, no second
  implementation of the logic.
- A `llmux-mcp` binary that serves the same operations as MCP tools over
  stdio, again as a thin client of the same core.
- The request path stays overhead-free: admin handling lives off the request
  hot path, and config reload atomically swaps a rebuilt executor rather than
  touching in-flight routing state.

## Scope of the management surface

- **Status**: uptime, version, listen address, live response counters.
- **Deployments**: provider, priority, limits, cooldown, in-flight, counters.
- **Credentials**: id, kind, provider — never secret values — plus the shared
  enabled/disabled routing state, which can be toggled.
- **Models**: canonical names, deployments, allowed-key restrictions.
- **Config reload**: re-read the config file, validate and rebuild, atomically
  swap; report success or the validation error.

Non-goals: editing the config file itself, managing client keys, streaming
logs, or a second dashboard.
