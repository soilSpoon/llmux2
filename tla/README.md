# TLA+ models

| Spec | Mirrors | Invariants |
| --- | --- | --- |
| `RequestPath` | `crates/llmux-core/src/machine.rs` (`State::step`) | `NoReplayAfterCommit`, `AttemptsBounded`, `NoAttemptAfterDeadline`, `NoSelectInCooldown`, `PermitReleasedOnExhaust` |
| `StreamLifecycle` | `Executor::attempt` + `hold_permit` | `NoRetryAfterCommit`, `PermitReleasedWhenClosed` |
| `DeploySlots` | `deploy/src/lib.rs` handoff | `AlwaysOneAccepting`, `DrainingNotAccepting`, `AcceptingIsActive` |

Run (Java 17+):

```sh
java -jar tla2tools.jar -config RequestPath.cfg RequestPath.tla
```

`tla2tools.jar` is v1.8.0, sha256 `fdffef247f563ef6edbfd44958d2a82db79e7564e07cb673262d896353fa6713`
(`https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar`). CI (`.github/workflows/tla.yml`) verifies the hash and fails on any violation.

Models are hand-written abstractions: only the Rust `step` table is tied to code by unit tests, so keep the two in sync when changing either.
