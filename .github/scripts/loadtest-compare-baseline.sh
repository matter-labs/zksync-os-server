#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 5 ]]; then
  echo "Usage: $0 <workflow_file> <current_summary_json> <out_dir> <sample_count> <repo>" >&2
  exit 1
fi

WORKFLOW_FILE="$1"
CURRENT_SUMMARY="$2"
OUT_DIR="$3"
SAMPLE_COUNT="$4"
REPO="$5"

if [[ -z "${GITHUB_TOKEN:-}" ]]; then
  echo "GITHUB_TOKEN is required" >&2
  exit 1
fi

API_ROOT="${GITHUB_API_URL:-https://api.github.com}"
AUTH_HEADER="Authorization: Bearer ${GITHUB_TOKEN}"
ACCEPT_HEADER="Accept: application/vnd.github+json"

mkdir -p "${OUT_DIR}/baseline-runs"

RUNS_JSON="${OUT_DIR}/runs.json"
curl -fsSL \
  -H "${AUTH_HEADER}" \
  -H "${ACCEPT_HEADER}" \
  "${API_ROOT}/repos/${REPO}/actions/workflows/${WORKFLOW_FILE}/runs?branch=main&status=success&per_page=50" \
  -o "${RUNS_JSON}"

mapfile -t RUN_IDS < <(jq -r '.workflow_runs[].id' "${RUNS_JSON}")

if [[ ${#RUN_IDS[@]} -eq 0 ]]; then
  echo "No successful main runs found for workflow ${WORKFLOW_FILE}" >&2
  exit 0
fi

COLLECTED=0
for RUN_ID in "${RUN_IDS[@]}"; do
  [[ "${COLLECTED}" -ge "${SAMPLE_COUNT}" ]] && break

  ARTIFACTS_JSON="${OUT_DIR}/artifacts-${RUN_ID}.json"
  curl -fsSL \
    -H "${AUTH_HEADER}" \
    -H "${ACCEPT_HEADER}" \
    "${API_ROOT}/repos/${REPO}/actions/runs/${RUN_ID}/artifacts?per_page=100" \
    -o "${ARTIFACTS_JSON}"

  ARTIFACT_ID="$(jq -r '.artifacts[] | select(.name | startswith("loadbase-benchmark-")) | .id' "${ARTIFACTS_JSON}" | head -n 1)"
  if [[ -z "${ARTIFACT_ID}" ]]; then
    continue
  fi

  ZIP_FILE="${OUT_DIR}/artifact-${RUN_ID}.zip"
  curl -fsSL \
    -H "${AUTH_HEADER}" \
    -H "${ACCEPT_HEADER}" \
    "${API_ROOT}/repos/${REPO}/actions/artifacts/${ARTIFACT_ID}/zip" \
    -o "${ZIP_FILE}"

  BASELINE_SUMMARY="${OUT_DIR}/baseline-runs/summary-${RUN_ID}.json"
  if unzip -p "${ZIP_FILE}" "loadbase-artifacts/summary.json" > "${BASELINE_SUMMARY}" 2>/dev/null; then
    COLLECTED=$((COLLECTED + 1))
  else
    rm -f "${BASELINE_SUMMARY}"
  fi
done

if [[ "${COLLECTED}" -eq 0 ]]; then
  echo "No baseline summary files collected from main artifacts" >&2
  exit 0
fi

BASELINE_JSON="${OUT_DIR}/baseline.json"
jq -s '
  def median:
    if length == 0 then 0
    else sort | .[(length - 1) / 2 | floor]
    end;
  {
    sample_count: length,
    median_tps10: ([.[].summary.median_tps10] | median),
    p95_tps10: ([.[].summary.p95_tps10] | median),
    median_include_p50_10s_s: ([.[].summary.median_include_p50_10s_s] | median),
    receipt_timeouts: ([.[].summary.receipt_timeouts // 0] | median),
    receipt_errors: ([.[].summary.receipt_errors // 0] | median)
  }
' "${OUT_DIR}"/baseline-runs/summary-*.json > "${BASELINE_JSON}"

REGRESSION_JSON="${OUT_DIR}/regression.json"
jq \
  --argfile baseline "${BASELINE_JSON}" \
  '
  def pct_delta(current; base):
    if (base == 0) then 0
    else ((current - base) / base) * 100
    end;
  {
    baseline: $baseline,
    current: .summary,
    deltas: {
      median_tps10_pct: pct_delta(.summary.median_tps10; $baseline.median_tps10),
      p95_tps10_pct: pct_delta(.summary.p95_tps10; $baseline.p95_tps10),
      include_p50_pct: pct_delta(.summary.median_include_p50_10s_s; $baseline.median_include_p50_10s_s),
      receipt_timeouts_pct: pct_delta((.summary.receipt_timeouts // 0); $baseline.receipt_timeouts),
      receipt_errors_pct: pct_delta((.summary.receipt_errors // 0); $baseline.receipt_errors)
    },
    warnings: {
      tps_median_regression: (pct_delta(.summary.median_tps10; $baseline.median_tps10) < -15),
      tps_p95_regression: (pct_delta(.summary.p95_tps10; $baseline.p95_tps10) < -20),
      include_p50_regression: (pct_delta(.summary.median_include_p50_10s_s; $baseline.median_include_p50_10s_s) > 25),
      receipt_timeout_regression: (pct_delta((.summary.receipt_timeouts // 0); $baseline.receipt_timeouts) > 50)
    }
  }
  ' "${CURRENT_SUMMARY}" > "${REGRESSION_JSON}"

WARN_COUNT="$(jq '[.warnings[] | select(.)] | length' "${REGRESSION_JSON}")"
if [[ "${WARN_COUNT}" -gt 0 ]]; then
  echo "warning_status=WARN" >> "${GITHUB_OUTPUT}"
else
  echo "warning_status=OK" >> "${GITHUB_OUTPUT}"
fi
echo "baseline_sample_count=${COLLECTED}" >> "${GITHUB_OUTPUT}"
echo "baseline_json=${BASELINE_JSON}" >> "${GITHUB_OUTPUT}"
echo "regression_json=${REGRESSION_JSON}" >> "${GITHUB_OUTPUT}"
