# Loadbase CI Benchmarking Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add structured benchmark outputs to `loadbase` and add a daily CI workflow on `main` that runs the benchmark, publishes human-readable results, uploads JSON/CSV artifacts, and pushes key metrics to Pushgateway.

**Architecture:** Extend the existing `Metrics` reporter in `loadbase` to emit per-second samples into in-memory timeseries while preserving current terminal output format. Add a small output module that serializes samples and run metadata into `metrics.json`, `metrics.csv`, and `summary.json`. Add a dedicated GitHub Actions workflow that boots local dependencies, executes `loadbase` with deterministic parameters, uploads artifacts, writes a job summary, and conditionally pushes metrics to Pushgateway via secret-configured URL.

**Tech Stack:** Rust (`tokio`, `serde_json`, std I/O), GitHub Actions YAML, shell (`bash`, `jq`, `curl`)

---

### Task 1: Add Output Serialization Surface (TDD)

**Files:**
- Create: `loadbase/src/output.rs`
- Modify: `loadbase/src/main.rs`
- Test: `loadbase/src/output.rs` (unit tests)

**Step 1: Write the failing tests**
- Add tests for:
  - output format parsing defaults (`text` by default, `all` includes JSON+CSV+summary).
  - JSON serialization of one benchmark sample.
  - CSV serialization header and first row.
  - summary generation from a sample set.

**Step 2: Run tests to verify they fail**
- Run: `cargo test --manifest-path loadbase/Cargo.toml output::tests -- --nocapture`
- Expected: FAIL due to missing module/types.

**Step 3: Write minimal implementation**
- Implement `OutputFormat` enum and output writer helpers in `output.rs`.
- Add run metadata and sample structs consumed by output writers.

**Step 4: Run tests to verify they pass**
- Run: `cargo test --manifest-path loadbase/Cargo.toml output::tests -- --nocapture`
- Expected: PASS.

### Task 2: Extend Metrics with Sample Collection (TDD)

**Files:**
- Modify: `loadbase/src/metrics.rs`
- Test: `loadbase/src/metrics.rs` (unit tests)

**Step 1: Write failing tests**
- Add tests that:
  - collect a sample snapshot with expected counters.
  - ensure percentile stats are stable with known values.

**Step 2: Run tests to verify they fail**
- Run: `cargo test --manifest-path loadbase/Cargo.toml metrics::tests -- --nocapture`
- Expected: FAIL due to absent sample/snapshot APIs.

**Step 3: Implement minimal metrics extensions**
- Add per-second sample struct.
- Append samples in report loop while preserving existing stdout output.
- Add snapshot / export accessor from `Metrics`.

**Step 4: Run tests to verify they pass**
- Run: `cargo test --manifest-path loadbase/Cargo.toml metrics::tests -- --nocapture`
- Expected: PASS.

### Task 3: Wire Outputs into Runtime Flow

**Files:**
- Modify: `loadbase/src/main.rs`
- Modify: `loadbase/src/Cargo.toml`
- Test: `loadbase/src/output.rs`, `loadbase/src/metrics.rs`

**Step 1: Write failing test (where feasible)**
- Add output-config parsing tests for CLI args (format + output-dir).

**Step 2: Run targeted tests and confirm failure**
- Run: `cargo test --manifest-path loadbase/Cargo.toml`
- Expected: Fails until integration is complete.

**Step 3: Implement runtime integration**
- Add CLI options:
  - `--output-dir`
  - `--output-format text|json|csv|all`
- At end of run, obtain metrics samples and write:
  - `metrics.json`
  - `metrics.csv`
  - `summary.json`
  - `report.md` (human-readable)
- Keep existing line-by-line console output unchanged.

**Step 4: Verify tests pass**
- Run: `cargo test --manifest-path loadbase/Cargo.toml`
- Expected: PASS.

### Task 4: Add Daily CI Workflow

**Files:**
- Create: `.github/workflows/loadbase-benchmark.yml`

**Step 1: Add workflow with daily schedule**
- Trigger daily on `main`; allow manual dispatch.

**Step 2: Implement benchmark job**
- Checkout + runner setup.
- Start local chain and `zksync-os-server`.
- Run `loadbase` with fixed profile and `--output-dir`.
- Upload artifacts (`loadbase.log`, `metrics.json`, `metrics.csv`, `summary.json`, `report.md`).
- Write GitHub Job Summary from `summary.json`.
- Push key metrics to Pushgateway if secret URL is present.

**Step 3: Validate workflow syntax**
- Run: `yamllint` if available, else `python -c` YAML parse fallback unavailable, so perform manual review and minimal shell lint checks.

### Task 5: Final Verification

**Files:**
- Verify all modified files above.

**Step 1: Run full loadbase verification**
- Run: `cargo test --manifest-path loadbase/Cargo.toml`
- Run: `cargo check --manifest-path loadbase/Cargo.toml`

**Step 2: Collect diff and confirm scope**
- Run: `git status --short`
- Run: `git diff --stat`

**Step 3: Summarize evidence**
- Report commands + exit codes + key outputs.
