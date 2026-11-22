# User-systemd deployment

Linux and a user systemd manager are required. The deployment and guard tools
are standalone Rust binaries; hosts do not need Python or Cargo.
Run these commands as the service user. Have an administrator enable lingering
(`loginctl enable-linger USER`) so the user manager survives logout.

## Install

Build `cargo build --locked --release -p llmux-server -p llmux2-ops`. From the repository
(or copy the resulting binaries to the host before installing):

```sh
mkdir -p "$HOME/llmux2/bin" "$HOME/.config/systemd/user"
chmod 700 "$HOME/llmux2" "$HOME/llmux2/bin"
install -m 700 target/release/llmux2-deploy target/release/llmux2-guard \
  target/release/llmux2-alert "$HOME/llmux2/bin/"
install -m 600 deploy/systemd/* "$HOME/.config/systemd/user/"
systemctl --user daemon-reload
```

The host administrator should persist `net.ipv4.tcp_migrate_req=1` in sysctl
configuration. With SO_REUSEPORT, connections queued on a closing listener
otherwise risk resets instead of migrating to the other slot. Use a Linux
kernel supporting this setting; verify with `sysctl net.ipv4.tcp_migrate_req`.

Use absolute `file:` secret references (files mode 600). Environment references
must exist in both the deploy command's environment and the systemd user
manager; an interactive shell's environment is not automatically inherited.
Relative paths resolve from `$HOME/llmux2`. OAuth refresh-token files are shared
by both slots: verify your issuer tolerates overlapping refreshes or use
separate credentials per slot. Slot binaries and TOML files must never be edited
while a slot is active or restarting.

## Migrate a single llmux2.service

The old single-instance binary may not use SO_REUSEPORT. For this one-time
migration, schedule a maintenance window: stop and disable `llmux2.service`,
then deploy below. Zero-downtime handoffs begin once both slots use this binary.
Do not leave the old unit enabled. Keep its files for rollback.

## Deploy and inspect

```sh
"$HOME/llmux2/bin/llmux2-deploy" deploy \
  --binary "$PWD/target/release/llmux-server" --config /absolute/path/llmux.toml
systemctl --user enable --now llmux2-guard.timer
"$HOME/llmux2/bin/llmux2-deploy" status
```

Without arguments, sources are `$HOME/llmux2/bin/llmux-server` and
`$HOME/llmux2/llmux.toml`. The tool validates and atomically copies sources to the
idle slot, validates the installed copies, waits for systemd READY, then observes
the new unit's ActiveState, NRestarts and MainPID for three seconds. It must
remain active with zero restarts and the same nonzero PID. A port health probe
must also succeed, but cannot identify the new slot under SO_REUSEPORT: the old
slot might answer it. Only after both checks does deployment stop/drain the old
slot and record/enable the new one for boot.

Pass `--canary SECONDS` to hold the handoff: after the new slot is stable and
healthy, both slots keep running and the kernel splits new connections between
them over the shared SO_REUSEPORT listener (roughly half reach the new build).
For that window the new unit must keep the same PID with zero restarts; a
failure stops the new slot and leaves the old one serving, so a bad build never
takes the port alone. The port probe still cannot tell the two slots apart, so
the window verifies unit state, not per-slot responses. A successful window
promotes exactly like a plain deploy. `0` disables the window (the default).

Use the same listen address across deployments. A failed start, stability or health check
stops the new slot without stopping the old one. Both slots active is an
ambiguous interrupted handoff: inspect logs and stop the unwanted slot before
retrying. Failures at/after old-slot stop leave the new slot running and require
checking boot enablement and `active.json`. Deployment and guard share a lock.

The guard starts the recorded slot only if neither slot is active (including
restarting/stopping) and health fails. Restart=always and the HTTP-gated
watchdog handle crashes and wedged processes. Slot logs are
`$HOME/llmux2/server-a.log` and `server-b.log`; arrange log rotation externally.
On SIGTERM the old slot immediately sends STOPPING=1 and reports healthz 503,
but keeps accepting/serving for `server.drain_grace_ms` (default 2000 ms,
range 0–10000; 0 disables the grace). The 10-second cap stays well below
WatchdogSec=30 because the new slot's shared-port watchdog probe can reach
the draining old slot's 503. Its HTTP/1 responses carry
`Connection: close`, giving keep-alive clients time to migrate before the old
listener closes. Only then does the bounded in-flight/SSE drain start.
Keep `server.drain_grace_ms + server.shutdown_timeout_ms` below the service's
90-second stop timeout. The grace mitigates the close/send race; it does not
guarantee error-free handoffs for clients stalled beyond the grace interval.

## Alert thresholds

`llmux2-alert` probes the live gateway and prints one `ALERT` line per breach
on stderr, exiting 1; it exits 0 with `ok` when nothing breaches and 2 on a
malformed thresholds file. Run it on a timer so breaches land in the journal:

```sh
install -m 700 target/release/llmux2-alert "$HOME/llmux2/bin/"
systemctl --user enable --now llmux2-alert.timer
```

Thresholds live in `$HOME/llmux2/alerts.toml` (`--config` overrides). A missing
file means only reachability checks; every threshold is optional and unset
checks are skipped.

```toml
listen = "127.0.0.1:8090"          # default: recorded active.json listen
key_file = "/home/opc/llmux2/dashboard.key"  # bearer for /admin stats
failure_ratio = 0.2                # new failures / new attempts per deployment
min_attempts = 10                  # window floor for failure_ratio (default 10; 0 disables it)
max_cooldowns = 2                  # more than this many cooling deployments
max_disabled = 0                   # any disabled deployment alerts
```

`failure_ratio` compares new `failed_attempts + stream_errors` against new
attempts (`routed + failed_attempts` — `routed` already counts requests that
later error mid-stream) since the previous run's counters, persisted atomically
in `$HOME/llmux2/alert-state.json`. Values outside `0..=1` (including `nan`)
are rejected as config errors. The
first run (or any run after counters reset, e.g. a reload or restart) records
a baseline and cannot breach the ratio. Wire notifications with
`OnFailure=` on `llmux2-alert.service` or a journal watcher; the tool itself
only reports.

## Rollback and deliberate stop

Redeploy a known-good binary/config using the same command; the currently live
slot drains only after the rollback slot is ready. Retain release artifacts
outside the two slots (a subsequent deploy can overwrite the inactive slot).
For a deliberate outage, first stop `llmux2-guard.timer`, then stop both slot
units. To undo migration, stop the timer and both slots before re-enabling the
old single-instance unit; that transition requires a maintenance window.

## Synthetic route probes

`llmux2-probe` exercises the running gateway like a client would. Every run
checks `GET /healthz` and `GET /v1/models`, then sends a one-token
`POST /v1/chat/completions` to `--per-run` canonical models (default 1),
walking the sorted model list across runs via a cursor in
`--state-file` (`$HOME/llmux2/probe-state.json`). Rotation keeps provider
quota cost bounded: one synthetic request per interval instead of one per
model. `--all-formats` additionally probes `/v1/responses` and `/v1/messages`
once each with the first model, covering the other inbound wire formats.
`--models a,b` pins an explicit set instead of rotating.

The state file also accumulates each probed model's consecutive failure
streak. A model whose last `--dead-after` probes (default 2) all returned
404 — a deterministic "the upstream cannot serve this" — is listed under
`dead` in the report, and `--dead-file PATH` writes those names as a JSON
list that `llmux-import --exclude-models` consumes so the next import drops
them instead of advertising them. Transient failures (429 cooldowns, 502
upstream errors) never count toward dead.

`--listen` defaults to the gateway's default `127.0.0.1:8080`; the shipped
unit passes `127.0.0.1:8090` for the OCI slots.

A separate low-rate sweep covers the whole catalog: `llmux2-sweep.timer`
(daily) runs `llmux2-probe --sweep`, which probes every listed model once in
a single run without moving the rotation cursor, using its own state file
(`probe-sweep-state.json`) and writing the dead list to `dead-models.json`.
One sweep sends one token-bearing request per advertised model, so keep its
cadence low; the curated `--models` pin on `llmux2-probe.service` never
builds streaks for the long tail — the sweep is what feeds `--dead-file`
for the full catalog.

`llmux2-prune.timer` (weekly) then re-runs the import with
`--exclude-models dead-models.json` and deploys through `llmux2-deploy`, so
dead routes stop being advertised without manual pruning. The service runs
`llmux2-prune.sh` (installed alongside the binaries), which:

- regenerates `llmux-next.toml` with the site's supplemental importer flags
  read from `~/llmux2/import.flags` (one argument per line — the same
  `--omniroute-fallback`, `--nous-*`, `--kilo-*`, … arguments the original
  import used, so a prune never shrinks the catalog);
- passes the deployed slot's own `--listen` address read back from
  `active.json` (deploy rejects a regenerated config on a different port);
- removes only the stale candidate config — the shared `~/llmux2/secrets/`
  directory is never deleted since the importer skips existing secret
  files and both slots share them;
- skips the run entirely when no sweep output exists yet.

Refresh the OmniRoute export before each run via an `ExecStartPre` drop-in
on `llmux2-prune.service` (the export contains credentials). Deploy follows
the normal slot-swap rules; rollback is a redeploy of the known-good config.

```sh
install -m 700 deploy/prune.sh "$HOME/llmux2/bin/llmux2-prune.sh"
install -m 700 target/release/llmux-import "$HOME/llmux2/bin/"
systemctl --user enable --now llmux2-sweep.timer llmux2-prune.timer
```

The probe reads the gateway client key from `--key-file`
(`$HOME/llmux2/client.key`), prints a JSON report naming each check's status,
latency and serving deployment, and exits 0 only when every probe passes;
a failed probe exits 1 and lands in the journal where the unit shows failed.
Install with the other binaries and enable the timer:

```sh
install -m 700 target/release/llmux2-probe "$HOME/llmux2/bin/"
systemctl --user daemon-reload
systemctl --user enable --now llmux2-probe.timer
journalctl --user -u llmux2-probe.service
```

Tune cadence with a timer drop-in and breadth with `--per-run`/`--models` in a
service drop-in. Probes send real provider traffic (one token per model), so
keep the total request rate well under the lowest provider RPM limit.
