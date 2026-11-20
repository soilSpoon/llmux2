# llmux2 engineering standards

## Plan

- Keep each PR independently mergeable and focused on one outcome. Name the observable behavior, the most important failure case, and what would prove the change before editing. Call out breaking config or wire changes and their migration or rollback.
- Where available, start [pstack](https://github.com/cursor/plugins/tree/main/pstack) with `poteto-mode` and use the matching playbook. Follow [Compound Engineering](https://every.to/compound-engineering)'s plan → work → review → compound loop. If the agent cannot run the skills, follow the steps here and say so in the PR; do not claim the plugins ran.

## Work test-first

- For a fix or new behavior, add the smallest test that exercises the observable contract. Run it **before** changing the implementation; it must fail for the expected reason, not because the harness is broken. Make the smallest fix, rerun the same test to green, then refactor and run the relevant suite.
- Prefer a unit test for pure logic and existing `llmux-testkit` loopback fixtures for HTTP, OAuth, routing, and SSE boundaries. Use synthetic credentials and fixtures, not live upstreams or production secrets. Assert outcomes (status, body, attempts, events, or absence of an upstream call), not incidental implementation details.
- Keep the red/green command and outcomes in the PR even when commits are squashed. For behavior-preserving refactors, show the same checks pass before and after. If a deterministic regression test does not apply (for example, a docs-only change or a performance-only measurement), explain why and show what was verified instead.

## Review for reliability and code quality

Check the risks the change touches, not every row in every PR:

| Area | What to verify |
| --- | --- |
| Routing and retries | Limits, deadline, exhaustion, and that retry does not replay a committed response. |
| Streams and async work | Setup versus idle timeouts, bounded buffering, cancellation, and permit release. |
| Providers, config and imports | Protocol compatibility, rejected inputs, safe defaults, and no credential values in errors, logs, or generated config. |
| Hot paths | Direct and via-gateway measurements on comparable builds; for the 1,000 RPS p99 target, use the documented warm-up and paired median, not one noisy run. |

Keep provider-specific behavior out of the routing core where the adapter contract suffices. Reuse shared fixtures and abstractions; avoid speculative dependencies, unbounded work, and unrelated cleanup.

Run the applicable checks locally and require the GitHub Actions `check` job to pass on the PR head. The `pr-policy` job fails a PR whose template sections are empty, or whose `crates/*/src` changes come without a test change (a `crates/*/tests/` or `llmux-testkit` file, or an added `#[test]` or `assert!`); when no deterministic test applies, write `Test exemption: <reason>` in the Verification section. Address Devin Review findings before merging; explain any finding you do not act on.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
python3 -B -m unittest discover -s bench -v
```

## Compound what was learned

When a fix or review corrects an assumption likely to recur, keep the lesson in the smallest durable form: a regression test or lint for a checkable invariant, an update to the existing docs or examples for user-visible behavior, or a short linked note in `docs/solutions/` if the rationale is not recoverable from those. Include the symptom, why it happened, and how to verify it. Update stale guidance rather than duplicating it. Link the result in the PR, or say why no new lesson belongs in the repo.
