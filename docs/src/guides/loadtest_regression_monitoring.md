# Load Test Regression Monitoring

This project includes a `loadbase` benchmark workflow to detect performance regressions and provide monitoring-ready outputs.

## Workflows and Profiles

The workflow is defined in:

- `.github/workflows/loadtest.yml`

It runs in three modes:

1. `pull_request` (quick profile, warning-only regression check)
2. `workflow_dispatch` (manual run)
3. `schedule` (daily baseline run on `main`)

Current benchmark profiles:

- PR runs:
  - `--duration 2m`
  - `--wallets 20`
  - `--max-in-flight 15`
- Main/scheduled runs:
  - `--duration 5m`
  - `--wallets 30`
  - `--max-in-flight 25`

## Structured Outputs

`loadbase` writes benchmark files to `loadbase-artifacts/`:

- `summary.json`
- `report.md`
- `metrics.json`
- `metrics.csv`

These are uploaded as GitHub Action artifacts for each run.

## PR Regression Check (Rolling Baseline)

For PR runs, the workflow executes:

- `.github/scripts/loadtest-compare-baseline.sh`

The script:

1. Fetches recent successful `main` runs of `loadtest.yml`
2. Downloads benchmark artifacts
3. Extracts `summary.json` files
4. Builds a rolling baseline (median-based) from the last `BASELINE_SAMPLE_COUNT` runs (default: `7`)
5. Compares current PR summary against baseline
6. Emits:
   - `loadbase-artifacts/baseline.json`
   - `loadbase-artifacts/regression.json`

If no `main` baselines exist yet, comparison is skipped and the PR job still proceeds.

## Warning Thresholds (Non-Blocking)

PR regression is currently warning-only. A warning is reported when one or more are true:

- median TPS10 drop > 15%
- P95 TPS10 drop > 20%
- median include p50 (10s window) increase > 25%
- receipt timeouts increase > 50%

No PR failure is triggered by these checks at this stage.

## Receipt Timeout and Error Metrics

To avoid noisy failures in CI, receipt waiters are non-fatal:

- receipt timeout and receipt RPC errors are counted
- counters are exported in `summary.json`:
  - `summary.receipt_timeouts`
  - `summary.receipt_errors`

These values are also:

- shown in the GitHub job summary table
- pushed as metrics when Pushgateway is configured

## Monitoring Integration (Pushgateway/Prometheus/Grafana)

Optional secrets:

- `LOADBASE_PUSHGATEWAY_URL`
- `LOADBASE_PUSHGATEWAY_USERNAME` (optional)
- `LOADBASE_PUSHGATEWAY_PASSWORD` (optional)

When URL is set, workflow pushes:

- `loadbase_median_tps10`
- `loadbase_p95_tps10`
- `loadbase_median_include_p50_10s_seconds`
- `loadbase_final_included`
- `loadbase_sample_count`
- `loadbase_receipt_timeouts`
- `loadbase_receipt_errors`

Use a Pushgateway endpoint (not a Prometheus scrape endpoint directly), then configure Prometheus to scrape Pushgateway.

## Operational Notes

- The benchmark check is currently non-blocking by design.
- Once baseline stability is proven, warning thresholds can be converted into blocking gates.
- Artifact retention is set in workflow to preserve baseline history for rolling comparisons.
