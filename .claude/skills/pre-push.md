---
name: pre-push
description: Run all required checks before pushing code. Must be invoked before every git push.
user_invocable: true
---

# Pre-Push Checks

Run **all** of the following checks before every `git push`. Do not skip any step. Do not push if any check fails — fix the issue first and re-run all checks.

## 1. Format

```bash
cargo fmt --all --check
```

If formatting issues are found, run `cargo fmt --all` to fix them, then stage and commit the formatting changes.

## 2. Lint

```bash
cargo clippy --all-targets --all-features --workspace -- -D warnings
```

Fix all warnings before proceeding. Do not suppress warnings with `#[allow(...)]` unless there is a justified reason.

## 3. Unit Tests

```bash
cargo nextest run --release --workspace --exclude zksync_os_integration_tests
```

All unit tests must pass.

## 4. Integration Tests

```bash
cargo nextest run -p zksync_os_integration_tests
```

No live anvil instance is needed — each test manages its own L1/node. All integration tests must pass.

## 5. Test Coverage for the Change

Before pushing, judge whether the change warrants new tests:

- **Bug fix or new logic** — there must be a unit test covering the case.
- **New subsystem interaction or cross-component flow** — there must be an integration test in `zksync_os_integration_tests`.
- **Pure refactor, doc change, or config tweak** — tests may not be needed.

Any bigger change to server logic **must** have corresponding integration tests included in the same push.

## 6. Wire Format Immutability

Verify that no existing versioned wire format files under `lib/network/src/wire/replays/v*.rs` were modified. If wire format changes are needed, add a new versioned file instead.

## Failure Protocol

If any check fails:
1. Fix the issue.
2. Re-run **all** checks from the beginning (not just the one that failed).
3. Only push once every check passes.
