#!/usr/bin/env bash
# One-time setup for the demo Grafana finale: downloads Prometheus + Grafana as static
# tarballs (vast.ai instances are containers — no nested docker) and provisions Grafana
# with the repo's sequencer dashboard. Safe to re-run; needs internet + ~400MB disk.
set -euo pipefail

OBS_DIR="${OBS_DIR:-$HOME/demo-observability}"
PROM_VERSION="${PROM_VERSION:-3.13.0}"
GRAFANA_VERSION="${GRAFANA_VERSION:-13.1.0}"
REPO_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"

mkdir -p "$OBS_DIR"
cd "$OBS_DIR"

if [ ! -d "prometheus-$PROM_VERSION.linux-amd64" ]; then
  curl -fL -O "https://github.com/prometheus/prometheus/releases/download/v$PROM_VERSION/prometheus-$PROM_VERSION.linux-amd64.tar.gz"
  tar xzf "prometheus-$PROM_VERSION.linux-amd64.tar.gz"
fi
ln -sfn "prometheus-$PROM_VERSION.linux-amd64" prometheus

# Grafana tarballs have flip-flopped between grafana-v<ver>/ and grafana-<ver>/ as the
# extracted dir name across major versions — accept either.
if [ ! -d "grafana-v$GRAFANA_VERSION" ] && [ ! -d "grafana-$GRAFANA_VERSION" ]; then
  curl -fL -O "https://dl.grafana.com/oss/release/grafana-$GRAFANA_VERSION.linux-amd64.tar.gz"
  tar xzf "grafana-$GRAFANA_VERSION.linux-amd64.tar.gz"
fi
GRAFANA_DIR="grafana-v$GRAFANA_VERSION"
[ -d "$GRAFANA_DIR" ] || GRAFANA_DIR="grafana-$GRAFANA_VERSION"
ln -sfn "$GRAFANA_DIR" grafana

# 1s scrape: the whole run is 60s — default 15s would give four points per panel.
cat > prometheus.yml <<EOF
global:
  scrape_interval: 1s
scrape_configs:
  - job_name: zksync-os
    static_configs:
      - targets: ['127.0.0.1:3312']
EOF

mkdir -p provisioning/datasources provisioning/dashboards dashboards

# The uid must match what grafana_dashboard.json references as its datasource.
cat > provisioning/datasources/prometheus.yml <<EOF
apiVersion: 1
datasources:
  - name: Prometheus
    uid: cep2ohiclks1sb
    type: prometheus
    access: proxy
    url: http://127.0.0.1:9090
    isDefault: true
    jsonData:
      # Matches prometheus.yml's scrape_interval so \$__rate_interval stays tight (~4s
      # windows) instead of assuming 15s scrapes and over-smoothing a 60s run.
      timeInterval: 1s
EOF

cat > provisioning/dashboards/dashboards.yml <<EOF
apiVersion: 1
providers:
  - name: demo
    type: file
    options:
      path: $OBS_DIR/dashboards
EOF

# The demo dashboard queries the node's ACTUAL exported metric names (execution_* — the
# repo-root grafana_dashboard.json predates the vise group prefixes and its queries come
# back empty against this node).
cp "$REPO_DIR/docs/demo/grafana/demo-dashboard.json" dashboards/
# Localized copy of the production dashboard (see localize-dashboard.py), if present.
cp "$REPO_DIR/docs/demo/grafana/prod-dashboard-local.json" dashboards/ 2>/dev/null || true

echo "setup complete in $OBS_DIR — run start.sh next"
