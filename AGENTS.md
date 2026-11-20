# llmux2 engineering standards

## Plan

- Living specs are managed with [OpenSpec](https://openspec.dev/) under `openspec/` (`openspec/AGENTS.md` has the workflow). When behavior or a decided requirement changes, update the spec in the same PR.
- Keep each PR independently mergeable and focused on one outcome. Name the observable behavior, the most important failure case, and what would prove the change before editing. Call out breaking config or wire changes and their migration or rollback.
- `.devin/config.json` requires [pstack](https://github.com/cursor/plugins/tree/main/pstack) and [Compound Engineering](https://every.to/compound-engineering), pinned to reviewed commits, so Devin sessions on this repo load both. Start pstack with `poteto-mode` and use the matching playbook, and follow Compound Engineering's plan → work → review → compound loop. Bump a pin only in its own PR after reading the upstream diff. If the skills did not load or could not run, follow the steps here and say so in the PR; do not claim the plugins ran.
- This file and `REVIEW.md` win over rules a plugin brings for its own source repository (for example Compound Engineering's Bun test and contribution instructions).

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

Run the applicable checks locally and require the GitHub Actions `check` job to pass on the PR head. The `pr-policy` job fails a PR whose template sections are empty, or whose `crates/*/src` changes come without a test change (a `crates/*/tests/` or `llmux-testkit` file, or an added `#[test]` or `assert!`); when no deterministic test applies, write `Test exemption: <reason>` in the Verification section. The `coverage` job fails when workspace line coverage drops below `.github/coverage-floor` or a PR lowers that number; raise it when coverage rises. Address Devin Review findings before merging; explain any finding you do not act on.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo test --locked -p llmux2-ops # standalone deployment and guard tools; no real sleeps
cargo xtask pr-policy # PR_BODY, BASE_SHA, HEAD_SHA
cargo xtask coverage-ratchet coverage.json # optional BASE_SHA
cargo test --locked -p llmux-bench
python3 -B -m unittest discover -s scripts -v
```

## Merge

- Head-specific Devin Review evidence is either a marked review from `devin-ai-integration` or a successful latest `Devin Review` commit status created by that app. A pending, failed, or errored latest status blocks even with a marked review. The context and body markers are configured in `.github/merge-policy.json`.
- Run the gate from a trusted, up-to-date `main` checkout so a PR cannot weaken its own policy. Bootstrap changes to the gate require independent review before using the updated gate.
- Verification markers count only from accounts in `.github/merge-policy.json`'s `verifier_logins` allowlist (`devin-ai-integration` and `soilspoon`, case-insensitive with `[bot]` stripped).
- The owner never merges. Devin bots merge as the `devin-ai-integration` app, only after `python3 scripts/merge_gate.py <n>` succeeds; then run `command gh pr merge <n> --squash --match-head-commit <sha>` using the exact SHA the gate passed. Until the ruleset is imported, this gate is the enforcement; do not bypass it.
- Put exactly one `Author-Session: <id>` line in the PR body. Session IDs use letters, digits, `.`, `_`, `:`, or `-`. A different verifier session must check every applicable `REVIEW.md` item against the diff and inspect the red/green evidence, then post an issue comment with `<!-- llmux-verify:v1 sha=<full head sha> verdict=pass reviewer=<session id> -->` (use `verdict=fail` for a failure). The latest marker for that SHA wins. These are trusted session attestations, not authenticated identities.
- Any new commit invalidates Devin Review and independent verification: obtain both for the new head. Resolve all review threads and blocking latest reviews. The gate reads required checks from `.github/rulesets/main.json` and reviewer markers from `.github/merge-policy.json`; API errors and unknown mergeability block merging.

## Compound what was learned

When a fix or review corrects an assumption likely to recur, keep the lesson in the smallest durable form: a regression test or lint for a checkable invariant, an update to the existing docs or examples for user-visible behavior, or a short linked note in `docs/solutions/` if the rationale is not recoverable from those. Include the symptom, why it happened, and how to verify it. Update stale guidance rather than duplicating it. Link the result in the PR, or say why no new lesson belongs in the repo.
