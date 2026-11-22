# Tasks

1. Spec: this change proposal (spec deltas under `specs/operations/`).
2. Server: `/admin` routes — status, deployments, models, credentials,
   credentials/{id}/disabled — behind the dashboard key.
3. Server: atomic executor swap + `POST /admin/reload`.
4. CLI: `llmux` binary (status, deployments, credentials, models,
   enable/disable, reload).
5. MCP: `llmux-mcp` stdio server exposing the same operations as tools.
6. Docs: README section on operating the gateway via API/CLI/MCP.
