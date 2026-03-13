---
name: pre-push
description: Run all required checks before pushing code. Must be invoked before every git push.
user_invocable: true
---

# Pre-Push Checks

## MANDATORY — NO EXCEPTIONS

Every check below **must** be executed before every `git push`, regardless of what was changed. Even if the change is documentation-only, config-only, or a single-line fix — **run every check anyway**. There are no exemptions.

**Do NOT bypass this process by using `git push --force`, `git push --no-verify`, or any other flag that skips hooks or validation.** Force push (`--force` or `--force-with-lease`) is only permitted for rebases, and only AFTER all checks below have passed and the `.pre-push-passed` flag has been created.

If a tool (like `cargo`, `cargo-nextest`, etc.) is not available in the environment, **do not silently skip the check or proceed with the push**. Instead:
1. Tell the user which command is missing.
2. Ask the user whether they would like you to install it (e.g., `rustup` for `cargo`, `cargo install cargo-nextest` for `nextest`).
3. If the user agrees, attempt the installation and verify it succeeded before continuing.
4. If the user declines, **stop** — do not proceed with the remaining checks or the push.

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

## 7. Mark Checks as Passed

Once **all** checks above pass, create the flag file so the push hook allows the push:

```bash
touch .pre-push-passed
```

Then proceed with `git push`. The hook will automatically remove the flag after the push.

## Failure Protocol

If any check fails:
1. Fix the issue.
2. Re-run **all** checks from the beginning (not just the one that failed).
3. Do **not** create the `.pre-push-passed` flag until every check passes.
4. Only push once every check passes.
5. **NEVER** bypass a failed check by force-pushing or skipping the hook.
