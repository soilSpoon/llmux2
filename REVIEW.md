# Review rules for llmux2

Review against [AGENTS.md](AGENTS.md). Report each of these as a finding, with the file and line:

- **Untested behavior change.** Code under `crates/*/src/` changes what a caller, client, or upstream observes, and no test in the PR would have failed before the change. Name the missing case (input, expected status/body/attempt/event). A refactor needs evidence the same checks pass before and after.
- **Missing red/green evidence.** The PR description's Verification section does not name the test command, the failure seen before the fix, and the pass after, or does not explain why red/green does not apply.
- **Tests that cannot fail.** Assertions on incidental details, mocks that replace the code under test, `#[ignore]`d or timing-dependent tests, or live upstreams and real credentials instead of `llmux-testkit` fixtures.
- **Reliability regressions.** Retries that can replay a committed stream response; missing setup or idle timeouts; unbounded buffers, queues, or retries; permits or limits not released on error or cancellation; fallback that ignores priority, cooldown, RPM, or concurrency limits.
- **Credential exposure.** A secret, token, or key value that can reach an error message, log line, dashboard, generated config, or test fixture.
- **Unsafe defaults or silent breaks.** A config or wire-format change without validation of bad input, a safe default, and a migration or rollback note.
- **Quality drift.** Provider-specific branches in the routing core, duplicated fixtures or helpers, new dependencies without a stated need, `unwrap`/`expect` on input-derived values in non-test code, or unrelated changes bundled into the PR.

Do not report style that `cargo fmt` or `cargo clippy -D warnings` already enforces.
