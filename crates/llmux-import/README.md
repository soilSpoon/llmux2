# OmniRoute importer

```sh
cargo run --locked -p llmux-import -- --input export.json --output llmux.toml
# Optional: --secrets-dir /absolute/private/directory
cargo run --locked -p llmux-server -- llmux.toml
```

Use OmniRoute's operator-only `GET /api/settings/export-json` export
(`_meta.version = "omniroute-v3-legacy-export"`). Treat the input as sensitive:
it contains plaintext credentials. Unknown fields are tolerated. Unversioned
legacy JSON is accepted with a warning, including combos stored as `data` JSON.

The output directory must already exist. The default secret directory is
`secrets/` beside the output. On Unix, the directory is secured to 0700 and each
secret is exclusively created at 0600; existing output/secret files are never
overwritten. TOML contains absolute `file:` references, never credential values.
Move the secrets and update references together when relocating the config.
On failure, newly created secret files may remain; review/remove them before
retrying. Non-Unix secure file creation is not supported.

Active API-key connections become credentials. Each nonempty combo becomes a
canonical model; connection priorities sort ascending within each step. Pinned
connections and allowed-connection lists restrict selection. Nested combos expand
inline (cycle/depth/deployment limits and a 1024-step work budget per top-level
combo, including references producing no deployments); nested routing policy is not preserved.
Round-robin combos use equal deployment priority; other unsupported strategies
are approximated as sequential priority. Warnings and an import summary go to
stderr. Model names may contain spaces or punctuation in llmux; generated IDs
are sanitized and deduplicated. If there are no client keys, set
`LLMUX_CLIENT_KEY` before starting the server.

Built-ins: openai, anthropic, openrouter, groq, deepseek (`ds` step alias),
mistral, xai, together, fireworks, cerebras. The URL/protocol table comes from
OmniRoute `open-sse/config/providers/registry/<provider>/index.ts`.
Custom `openai-compatible` chat and `anthropic-compatible` nodes use `baseUrl`;
custom prefix aliases are resolved. llmux appends `/chat/completions` or
`/messages`, so built-in URLs omit that suffix, retaining `/v1` where present.

Not imported: OAuth/token/cookie auth, encrypted credentials, unsupported
providers/protocols (including custom Responses/custom-JSON nodes), custom chat
paths/headers, wildcard models, legacy model aliases, per-key policies, shared
account quotas, global priority, runtime state, and combo config/transforms.
Only unrestricted active gateway keys are imported. Keys with model/connection/
combo/endpoint/scope restrictions, schedules, expiry/revocation, bans, rate/session/
spending limits or other recognized access-limiting policies are skipped, never
promoted to unrestricted llmux access. Provider-specific model transformations (e.g. Fireworks model-prefix
rewrites) are not applied: combo models must contain the actual upstream ID.
The importer validates generated URLs/config with llmux-config before writing;
it does not contact upstream providers or validate that credentials work.
