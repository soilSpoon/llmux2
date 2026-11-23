# Design

## One core, three thin layers

The management logic lives once, in the server's admin handlers. The HTTP API
is the contract; the CLI and the MCP server are pure clients of it and contain
no routing or config logic of their own. Request/response bodies are the same
JSON the dashboard already shapes (`DeploymentStats`, counters), so the API
adds no new state model.

## Authentication

Admin endpoints mount only when `server.dashboard_key` is configured, and they
authenticate with that key as a Bearer token — the same constant-time check as
`/dashboard/stats`. No new credential kind is introduced. When no key is set,
`/admin` routes do not exist and return 404 like any unmounted path.

## Endpoints (under `/admin`)

- `GET /admin/status` — uptime, version, listen address, response counters.
- `GET /admin/deployments` — every deployment: model, id, provider, priority,
  rpm/concurrency limits, disabled, cooldown remaining, in-flight, counters.
- `POST /admin/credentials/{id}/disabled` — body `{"disabled": true|false}`.
  Flips the shared per-credential flag the router already checks; disabling a
  credential disables every deployment that draws on it.
- `GET /admin/models` — canonical names with their deployment ids and whether
  allowed keys restrict them.
- `GET /admin/credentials` — id, kind (`api_key`/`oauth`), provider, quota;
  secret material is never returned.
- `POST /admin/reload` — reload the config path the server started with,
  validate and rebuild the executor, swap atomically. Returns the new config
  summary, or a 4xx with the validation error and the old config stays live.

## Hot reload

`AppState` holds the executor behind an atomic pointer (`arc-swap`, already a
workspace dependency). A request loads the pointer once per request — one
atomic read, no locks — so the hot path stays allocation- and wait-free.
Reload builds the whole new executor first; only on success is it swapped in,
so a bad config can never take the gateway down. Bindings and stats counters
are rebuilt fresh, matching process-restart semantics.

## CLI

`llmux` subcommands mirror the endpoints: `status`, `deployments`,
`credentials`, `models`, `enable <credential>`, `disable <credential>`,
`reload`. Target via `--url`/`--key` or `LLMUX_ADMIN_URL`/`LLMUX_ADMIN_KEY`.
Output is JSON by default for scripting.

## MCP

`llmux-mcp` speaks MCP over stdio (initialize, tools/list, tools/call) and
exposes the same operations as tools (`llmux_status`, `llmux_deployments`,
`llmux_credentials`, `llmux_models`, `llmux_set_disabled`, `llmux_reload`),
calling the same admin API configured by the same env vars. No external MCP
framework dependency: the JSON-RPC envelope is ~200 lines, consistent with
the project's low-dependency stance.
