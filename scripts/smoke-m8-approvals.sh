#!/usr/bin/env bash
# M8 smoke test: policy-driven awaiting-approval ledger entry -> /api/v1/ledger endpoint.
#
# Usage:  ./scripts/smoke-m8-approvals.sh
#
# Requires: cargo build -p agentstatedeveloper-mcp -p agentstatedeveloper-cli
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Prime with a policy-driven awaiting-approval entry.
(
  cd examples/sample-py-repo
  rm -f .asd-state.db
  "$ROOT/target/debug/asd" init
  "$ROOT/target/debug/asd" index .
  "$ROOT/target/debug/asd" --policy "$ROOT/examples/policies.json" ledger append \
    payments.charge_card \
    --kind hazard \
    --summary "driver fails on amounts > 10000" \
    --author-id alice \
    --author-kind human
)

# Launch the server, hit the new endpoint, tear down.
ASD_DB="$ROOT/examples/sample-py-repo/.asd-state.db" \
ASD_LENS_DIR="$ROOT/web/build" \
  "$ROOT/target/debug/asd-serve" &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT

sleep 1

echo "--- GET /api/v1/ledger?tag=awaiting-approval ---"
curl -s "http://localhost:4120/api/v1/ledger?tag=awaiting-approval" | python3 -m json.tool | head -40

kill $SERVER_PID 2>/dev/null || true
