#!/usr/bin/env bash
# Installs the lockfile-pinned decoder generator when it is missing or stale.
set -euo pipefail
cd "$(dirname "$0")/.."

CARBON_CLI="node_modules/.bin/carbon-cli"
LOCK_FILE="package-lock.json"
STAMP_FILE="node_modules/.microscope-package-lock.json"

if [[ -x "$CARBON_CLI" && -f "$STAMP_FILE" ]] && cmp -s "$LOCK_FILE" "$STAMP_FILE"; then
    exit 0
fi

npm ci --ignore-scripts --legacy-peer-deps --no-audit --no-fund
cp "$LOCK_FILE" "$STAMP_FILE"
