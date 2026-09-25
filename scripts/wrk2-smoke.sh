#!/usr/bin/env bash
# wrk2 smoke bench using docker-compose.bench.yml fixture.
#
# Boots svc-a, svc-b, and ferryman via docker compose, waits for health,
# then runs wrk2 against the proxy.

set -euo pipefail

TARGET="${TARGET:-http://127.0.0.1:8080/svc-a/echo}"
DURATION="${DURATION:-10s}"
CONNS="${CONNS:-100}"
THREADS="${THREADS:-4}"
RATE="${RATE:-5000}"

# Cleanup on exit (trap runs even on error).
cleanup() {
  echo "[wrk2-smoke] Tearing down docker-compose..."
  docker compose -f docker-compose.bench.yml down
}
trap cleanup EXIT

# Start the fixture.
echo "[wrk2-smoke] Starting docker-compose fixture..."
docker compose -f docker-compose.bench.yml up -d --build

# Wait up to 60s for ferryman to be ready by polling the health endpoint.
echo "[wrk2-smoke] Waiting for ferryman health..."
max_wait=60
waited=0
while true; do
  if curl -fsS http://127.0.0.1:8080/svc-a/echo >/dev/null 2>&1; then
    echo "[wrk2-smoke] ferryman is healthy."
    break
  fi
  if [ "$waited" -ge "$max_wait" ]; then
    echo "[wrk2-smoke] FAILED: ferryman did not become healthy within ${max_wait}s" >&2
    exit 1
  fi
  sleep 1
  waited=$((waited + 1))
done

# Run the real wrk2 invocation.
echo "[wrk2-smoke] Running wrk2..."
"${WRK2:-wrk2}" -c "$CONNS" -t "$THREADS" -R "$RATE" -d "$DURATION" \
  -s benches/wrk2.lua "$TARGET"
