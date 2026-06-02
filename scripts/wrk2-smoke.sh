#!/usr/bin/env bash
# wrk2 smoke bench placeholder.
#
# Phase 1: prints the planned invocation and exits 0 so CI is green even
# without a real docker-compose fixture wired up yet. Phase 2 will boot
# upstream stubs via `docker compose -f docker-compose.bench.yml up -d`,
# wait for health, then run the real wrk2 invocation below against the
# proxy on :8080.

set -euo pipefail

TARGET="${TARGET:-http://127.0.0.1:8080/svc-a/echo}"
DURATION="${DURATION:-10s}"
CONNS="${CONNS:-100}"
THREADS="${THREADS:-4}"
RATE="${RATE:-5000}"

echo "[wrk2-smoke] would run:"
echo "  wrk2 -c ${CONNS} -t ${THREADS} -R ${RATE} -d ${DURATION} \\"
echo "       -s benches/wrk2.lua ${TARGET}"
echo "[wrk2-smoke] Phase 1 placeholder — no compose fixture wired yet."
exit 0
