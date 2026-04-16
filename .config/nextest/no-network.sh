#!/usr/bin/env bash
set -euo pipefail

if ! command -v unshare >/dev/null 2>&1; then
  echo "unshare is required to disable networking for tests" >&2
  exit 1
fi

exec unshare --user --map-root-user --net -- "$@"
