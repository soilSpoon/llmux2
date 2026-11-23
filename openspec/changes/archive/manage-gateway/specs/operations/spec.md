# operations delta

## ADDED Requirements

### Requirement: Management over one shared core

Operators SHALL be able to inspect and control the running gateway through an
authenticated HTTP admin API, a CLI, and an MCP server. The CLI and MCP server
SHALL be thin clients of the same admin API and contain no routing or config
logic of their own. Admin endpoints SHALL exist only when
`server.dashboard_key` is configured and SHALL authenticate with that key as a
Bearer token using the same constant-time check as the dashboard. Secret
material SHALL never appear in any management response.

#### Scenario: Same answer everywhere

WHEN an operator asks for deployment state via the API, the CLI, or an MCP
tool call
THEN all three report the same data because the latter two call the same API.

#### Scenario: No key, no admin surface

WHEN `server.dashboard_key` is not configured
THEN `/admin` routes do not exist.

### Requirement: Runtime management operations

The admin API SHALL expose gateway status (uptime, version, counters),
deployment state (provider, priority, limits, disabled, cooldown, in-flight,
counters), credential state (id, kind, provider — never secrets), and the
canonical model map. It SHALL allow enabling or disabling a credential's
routing state, which affects every deployment drawing on it.

#### Scenario: Disable a provider's key

WHEN an operator disables a credential
THEN new requests skip every deployment that uses it until it is re-enabled.

### Requirement: Validated hot config reload

The admin API SHALL offer `POST /admin/reload` that re-reads the config file,
validates and rebuilds the executor, and swaps it in atomically. A failed
reload SHALL leave the running config untouched and report the error. The
request path SHALL stay overhead-free: one atomic read per request, no locks.

#### Scenario: Bad config rejected safely

WHEN the new config fails validation
THEN the reload reports the error and the old config keeps serving.
