# User-systemd deployment

Linux, Python 3.9+ (stdlib only), and a user systemd manager are required.
Run these commands as the service user. Have an administrator enable lingering
(`loginctl enable-linger USER`) so the user manager survives logout.

## Install

Build `cargo build --locked --release -p llmux-server`. From the repository:

```sh
mkdir -p "$HOME/llmux2/bin" "$HOME/.config/systemd/user"
chmod 700 "$HOME/llmux2" "$HOME/llmux2/bin"
install -m 700 deploy/llmux2-deploy deploy/llmux2-guard "$HOME/llmux2/bin/"
install -m 600 deploy/llmux2_ops.py "$HOME/llmux2/bin/"
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
range 0–86400000; 0 disables the grace). Its HTTP/1 responses carry
`Connection: close`, giving keep-alive clients time to migrate before the old
listener closes. Only then does the bounded in-flight/SSE drain start.
Keep `server.drain_grace_ms + server.shutdown_timeout_ms` below the service's
90-second stop timeout. The grace mitigates the close/send race; it does not
guarantee error-free handoffs for clients stalled beyond the grace interval.

## Rollback and deliberate stop

Redeploy a known-good binary/config using the same command; the currently live
slot drains only after the rollback slot is ready. Retain release artifacts
outside the two slots (a subsequent deploy can overwrite the inactive slot).
For a deliberate outage, first stop `llmux2-guard.timer`, then stop both slot
units. To undo migration, stop the timer and both slots before re-enabling the
old single-instance unit; that transition requires a maintenance window.
