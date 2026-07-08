#!/usr/bin/env bash
# Start Prometheus (:9090) and Grafana (:3000, anonymous admin — demo box only) in the
# background. Logs land in $OBS_DIR. Safe to re-run (kills previous instances first).
set -euo pipefail

OBS_DIR="${OBS_DIR:-$HOME/demo-observability}"
cd "$OBS_DIR"

# Bracketed patterns so pkill can't match this script's own command line.
pkill -f 'prometheu[s] --config.file' 2>/dev/null || true
pkill -f 'grafan[a] server' 2>/dev/null || true
sleep 1

nohup ./prometheus/prometheus \
  --config.file="$OBS_DIR/prometheus.yml" \
  --storage.tsdb.path="$OBS_DIR/prom-data" \
  > prometheus.log 2>&1 &

GF_AUTH_ANONYMOUS_ENABLED=true \
GF_AUTH_ANONYMOUS_ORG_ROLE=Admin \
GF_PATHS_PROVISIONING="$OBS_DIR/provisioning" \
GF_PATHS_DATA="$OBS_DIR/grafana-data" \
GF_PATHS_LOGS="$OBS_DIR/grafana-logs" \
GF_PATHS_PLUGINS="$OBS_DIR/grafana-plugins" \
nohup ./grafana/bin/grafana server --homepath "$OBS_DIR/grafana" \
  > grafana.log 2>&1 &

echo "prometheus on :9090, grafana on :3000 (anonymous admin, no login)"
echo "dashboard: http://localhost:3000/d/zksync-demo (through the tunnel)"
