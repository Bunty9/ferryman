#!/usr/bin/env bash
# Guided tour of the full-stack reference example: brings up ferryman +
# three demo-upstream instances + Prometheus + Grafana behind TLS, and
# exercises routing, breaker, failover, hot reload, and shutdown behaviour
# against the real containers.
#
# Usage:
#   ./demo.sh          interactive: pauses between steps, colored output
#   ./demo.sh --ci     non-interactive: no pauses, no colors (used by CI)
#
# Env overrides: FERRYMAN_HTTPS_PORT (8443), FERRYMAN_METRICS_PORT (9090),
# PROMETHEUS_PORT (9091), GRAFANA_PORT (3000), KEEP=1 (skip `down -v` on exit).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

CI=0
for arg in "$@"; do
    case "$arg" in
        --ci) CI=1 ;;
        *)
            echo "usage: $0 [--ci]" >&2
            exit 2
            ;;
    esac
done

# ---- host prerequisites -----------------------------------------------
for bin in curl jq docker; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "demo.sh needs '$bin' on PATH" >&2
        exit 1
    fi
done
if ! docker compose version >/dev/null 2>&1; then
    echo "demo.sh needs the 'docker compose' v2 plugin" >&2
    exit 1
fi

# ---- endpoints ----------------------------------------------------------
HTTPS_PORT="${FERRYMAN_HTTPS_PORT:-8443}"
METRICS_PORT="${FERRYMAN_METRICS_PORT:-9090}"
PROM_PORT="${PROMETHEUS_PORT:-9091}"
GRAFANA_PORT="${GRAFANA_PORT:-3000}"

BASE="https://localhost:${HTTPS_PORT}"
METRICS="http://localhost:${METRICS_PORT}"
PROM="http://localhost:${PROM_PORT}"
GRAFANA="http://localhost:${GRAFANA_PORT}"
CA="certs/ca.pem"

CONFIG_FILE="config/ferryman.toml"
CONFIG_TMP="config/.ferryman.toml.tmp"

# ---- output ---------------------------------------------------------
if [ "$CI" = 1 ] || [ ! -t 1 ]; then
    BOLD="" RED="" GREEN="" RESET=""
else
    BOLD=$'\033[1m'
    RED=$'\033[31m'
    GREEN=$'\033[32m'
    RESET=$'\033[0m'
fi

PASS=0
STEP_NUM=0

step() {
    STEP_NUM=$((STEP_NUM + 1))
    echo
    echo "${BOLD}== Step ${STEP_NUM}/13: $1 ==${RESET}"
    if [ "$CI" != 1 ]; then
        read -r -p "  press enter to run this step... " _ || true
    fi
}

ok() {
    PASS=$((PASS + 1))
    echo "  ${GREEN}ok${RESET}   $1"
}

fail() {
    echo "  ${RED}FAIL${RESET} $1" >&2
    echo >&2
    echo "---- last 50 ferryman log lines ----" >&2
    docker compose logs --no-color --tail=50 ferryman >&2 || true
    exit 1
}

# ---- cleanup ----------------------------------------------------------
CONFIG_BACKUP=$(mktemp)
cp "$CONFIG_FILE" "$CONFIG_BACKUP"
CAP_HDR_FILE=$(mktemp)
CAP_BODY_FILE=$(mktemp)

cleanup() {
    ec=$?
    if [ -f "$CONFIG_BACKUP" ]; then
        cp "$CONFIG_BACKUP" "$CONFIG_FILE" 2>/dev/null || true
        rm -f "$CONFIG_BACKUP"
    fi
    rm -f "$CAP_HDR_FILE" "$CAP_BODY_FILE" "$CONFIG_TMP"
    if [ "${KEEP:-0}" != "1" ]; then
        docker compose down -v
    fi
    exit "$ec"
}
trap cleanup EXIT

# ---- curl helpers (never let a network hiccup trip `set -e`) ------------

# status_of [curl args...] -> prints the HTTP status code, "000" on error.
status_of() {
    local code
    code=""
    code=$(curl -sS -o /dev/null -w '%{http_code}' "$@" 2>/dev/null) || true
    if [ -z "$code" ]; then code="000"; fi
    printf '%s' "$code"
}

# body_of [curl args...] -> prints the response body, "" on error.
body_of() {
    local out
    out=""
    out=$(curl -sS "$@" 2>/dev/null) || true
    printf '%s' "$out"
}

# swap_config: rename-replace $CONFIG_FILE with $CONFIG_TMP's content — a
# plain, atomic `mv` over the existing file. This is the exact save
# pattern (write a temp file, then rename it over the target) that
# ferryman's hot-reload watcher is documented to handle, and the only one
# this script uses for the *first* config swap in the hot-reload step, so
# that step actually pins rename-replace working, unmodified.
swap_config() {
    mv "$CONFIG_TMP" "$CONFIG_FILE"
}

# swap_config_unlink: like swap_config, but unlinks the old file first.
# NOT an atomic replace (the path is briefly absent) — used only for the
# *second and later* config swaps in a single demo.sh run. On this
# project's dev/CI host, Docker itself runs inside a nested LinuxKit VM
# (`docker info` reports a `-linuxkit` kernel), and a *second* bare `mv`
# onto a bind-mounted path that was already rename-replaced once was
# observed to never become visible inside the container (verified with
# `docker compose cp` diffs across 20+ real seconds) — see the task-5
# report for the repro. The preceding `rm -f` reliably makes the change
# visible again. The first swap_config call already exercises genuine
# rename-replace; see README's "Docker Desktop / VM-backed file sharing"
# note for the production implication.
swap_config_unlink() {
    rm -f "$CONFIG_FILE"
    mv "$CONFIG_TMP" "$CONFIG_FILE"
}

# capture [curl args...] -> sets CAP_STATUS, writes headers/body to
# $CAP_HDR_FILE / $CAP_BODY_FILE.
capture() {
    CAP_STATUS=""
    CAP_STATUS=$(curl -sS -D "$CAP_HDR_FILE" -o "$CAP_BODY_FILE" -w '%{http_code}' "$@" 2>/dev/null) || true
    if [ -z "$CAP_STATUS" ]; then CAP_STATUS="000"; fi
}

# header_from_capture NAME -> prints that response header's value (after
# the most recent `capture`), "" if absent.
header_from_capture() {
    grep -i "^$1:" "$CAP_HDR_FILE" 2>/dev/null | head -1 | tr -d '\r' | sed -E 's/^[^:]+:[[:space:]]*//' || true
}

expect_status() {
    local url="$1" want="$2"
    shift 2
    local got
    got=$(status_of "$@" "$url")
    if [ "$got" = "$want" ]; then
        ok "$url -> $got"
    else
        fail "$url expected status $want, got $got"
    fi
}

expect_body_contains() {
    local url="$1" needle="$2"
    shift 2
    local body
    body=$(body_of "$@" "$url")
    case "$body" in
        *"$needle"*) ok "$url body contains '$needle'" ;;
        *) fail "$url body missing '$needle' (got: ${body:0:300})" ;;
    esac
}

# wait_until DESC TIMEOUT_SECS PREDICATE_FN [args...]
wait_until() {
    local desc="$1" timeout="$2"
    shift 2
    local waited=0
    while ! "$@" >/dev/null 2>&1; do
        waited=$((waited + 1))
        if [ "$waited" -ge "$timeout" ]; then
            fail "timed out after ${timeout}s waiting for: $desc"
        fi
        sleep 1
    done
    ok "$desc (within ${waited}s)"
}

# ---- predicates used with wait_until -----------------------------------
users_up() { [ "$(status_of --cacert "$CA" "$BASE/api/users/echo")" = "200" ]; }
orders_up() { [ "$(status_of --cacert "$CA" "$BASE/api/orders/echo")" = "200" ]; }
orders_v2_up() { [ "$(status_of --cacert "$CA" "$BASE/api/orders/v2/echo")" = "200" ]; }
inventory_up() { [ "$(status_of --cacert "$CA" "$BASE/api/inventory/echo")" = "200" ]; }
inventory_gone() { [ "$(status_of --cacert "$CA" "$BASE/api/inventory/echo")" = "404" ]; }
# reload_parse_error_logged SINCE_TS -> true once ferryman has logged an
# actual TOML *parse* error (not just "config reload failed", which is
# also logged for a transient "file missing" read error the `rm` half of
# swap_config_unlink can itself trigger) at or after SINCE_TS. Scoping by
# time also keeps a leftover log line from a previous KEEP=1 run from
# making this pass without the bad TOML ever being parsed this run.
reload_parse_error_logged() {
    docker compose logs --no-color --since "$1" ferryman 2>/dev/null | grep -q "parsing config file"
}
ferryman_exited_cleanly() {
    local state
    state=""
    state=$(docker compose ps -a --format json ferryman 2>/dev/null | jq -r '.State + " " + (.ExitCode|tostring)') || true
    [ "$state" = "exited 0" ]
}
circuit_open_alert_visible() {
    body_of "$PROM/api/v1/alerts" | jq -e '.data.alerts[] | select(.labels.alertname=="FerrymanCircuitOpen")' >/dev/null
}
prom_target_up() {
    local v
    v=""
    v=$(body_of "$PROM/api/v1/query?query=up%7Bjob%3D%22ferryman%22%7D" | jq -r '.data.result[0].value[1] // empty')
    [ "$v" = "1" ]
}
prom_has_duration_buckets() {
    local n
    n="0"
    n=$(body_of "$PROM/api/v1/query?query=ferryman_request_duration_seconds_bucket" | jq -r '.data.result | length')
    [ -n "$n" ] && [ "$n" -gt 0 ] 2>/dev/null
}

########################################################################
# Step 1: bring the stack up
########################################################################
step "docker compose up --build, wait for ferryman + users"
docker compose up -d --build
wait_until "/api/users/echo returns 200" 120 users_up

########################################################################
# Step 2: routing (prefix match, longest-prefix, 404)
########################################################################
step "Routing: prefix match, longest-prefix-first, 404 on no match"
capture --cacert "$CA" "$BASE/api/users/echo"
served=$(header_from_capture "x-served-by")
if [ "$CAP_STATUS" = "200" ] && [ "$served" = "users" ]; then
    ok "/api/users/echo -> 200, x-served-by: users"
else
    fail "/api/users/echo: expected 200/x-served-by:users, got $CAP_STATUS/x-served-by:$served"
fi

capture --cacert "$CA" "$BASE/api/orders/v2/echo"
served=$(header_from_capture "x-served-by")
if [ "$CAP_STATUS" = "200" ] && [ "$served" = "orders-v2" ]; then
    ok "/api/orders/v2/echo -> longest-prefix match, x-served-by: orders-v2"
else
    fail "/api/orders/v2/echo: expected 200/x-served-by:orders-v2, got $CAP_STATUS/x-served-by:$served"
fi

capture --cacert "$CA" "$BASE/api/orders/echo"
served=$(header_from_capture "x-served-by")
if [ "$CAP_STATUS" = "200" ] && [ "$served" = "orders" ]; then
    ok "/api/orders/echo -> x-served-by: orders"
else
    fail "/api/orders/echo: expected 200/x-served-by:orders, got $CAP_STATUS/x-served-by:$served"
fi

capture --cacert "$CA" "$BASE/account/echo"
served=$(header_from_capture "x-served-by")
if [ "$CAP_STATUS" = "200" ] && [ "$served" = "users" ]; then
    ok "/account/echo -> shares the users upstream, x-served-by: users"
else
    fail "/account/echo: expected 200/x-served-by:users, got $CAP_STATUS/x-served-by:$served"
fi

expect_status "$BASE/api/usersx" 404 --cacert "$CA"
expect_status "$BASE/nope" 404 --cacert "$CA"
expect_body_contains "$BASE/nope" "no route" --cacert "$CA"

########################################################################
# Step 3: TLS termination + HTTP/2 ALPN
########################################################################
step "TLS termination and HTTP/2 ALPN negotiation"
ver2=$(curl -sS -o /dev/null --cacert "$CA" --http2 -w '%{http_version}' "$BASE/api/users/echo" 2>/dev/null) || ver2=""
if [ "$ver2" = "2" ]; then
    ok "--http2 negotiates HTTP/2 (ALPN h2)"
else
    fail "expected http_version 2 with --http2, got '$ver2'"
fi
ver1=$(curl -sS -o /dev/null --cacert "$CA" --http1.1 -w '%{http_version}' "$BASE/api/users/echo" 2>/dev/null) || ver1=""
if [ "$ver1" = "1.1" ]; then
    ok "--http1.1 negotiates HTTP/1.1"
else
    fail "expected http_version 1.1 with --http1.1, got '$ver1'"
fi

########################################################################
# Step 4: forwarded headers, hop-by-hop stripping
########################################################################
step "Forwarded headers set; hop-by-hop headers stripped"
# HTTP/2 forbids connection-specific header fields (RFC 9113 §8.2.2), so
# force HTTP/1.1 here where a `Connection:` header is meaningful.
json=$(body_of --cacert "$CA" --http1.1 -H "Connection: x-secret" -H "x-secret: 1" "$BASE/api/users/echo")
proto=$(printf '%s' "$json" | jq -r '.headers["x-forwarded-proto"] // empty')
xff=$(printf '%s' "$json" | jq -r '.headers["x-forwarded-for"] // empty')
has_secret=$(printf '%s' "$json" | jq -r 'if (.headers | has("x-secret")) then "yes" else "no" end')
if [ "$proto" = "https" ] && [ -n "$xff" ] && [ "$has_secret" = "no" ]; then
    ok "x-forwarded-proto=https, x-forwarded-for=$xff set; x-secret (hop-by-hop via Connection) stripped"
else
    fail "forwarded-header check failed: proto=$proto xff=$xff has_secret=$has_secret"
fi

########################################################################
# Step 5: streaming responses + large request bodies
########################################################################
step "Streaming responses stream; large request bodies aren't buffered"
times=$(curl -sS -o /dev/null --cacert "$CA" -w '%{time_starttransfer} %{time_total}' \
    "$BASE/api/users/stream?chunks=5&interval_ms=400" 2>/dev/null) || times="99 99"
tstart=$(printf '%s' "$times" | cut -d' ' -f1)
ttotal=$(printf '%s' "$times" | cut -d' ' -f2)
starttransfer_ok=$(awk -v a="$tstart" 'BEGIN{print (a<1.0) ? "yes" : "no"}')
total_ok=$(awk -v a="$ttotal" 'BEGIN{print (a>=1.6) ? "yes" : "no"}')
if [ "$starttransfer_ok" = "yes" ] && [ "$total_ok" = "yes" ]; then
    ok "stream: time_starttransfer=${tstart}s (<1.0s), time_total=${ttotal}s (>=1.6s)"
else
    fail "stream timing: time_starttransfer=${tstart}s time_total=${ttotal}s"
fi

bytes=$(dd if=/dev/urandom bs=1M count=8 2>/dev/null \
    | curl -sS --cacert "$CA" --data-binary @- "$BASE/api/users/echo" 2>/dev/null \
    | jq -r '.body_bytes') || bytes=""
if [ "$bytes" = "8388608" ]; then
    ok "8 MiB POST body echoed as body_bytes=8388608"
else
    fail "expected body_bytes=8388608 for an 8 MiB upload, got '$bytes'"
fi

########################################################################
# Step 6: upstream timeout
########################################################################
step "Upstream timeout: no headers within upstream_timeout_secs -> 504"
expect_status "$BASE/api/users/slow?ms=4000" 504 --cacert "$CA" --max-time 10

########################################################################
# Step 7: breaker trips on passed-through upstream 5xx, recovers on restart
########################################################################
step "Upstream 5xx pass-through trips the breaker; restart clears it"
expect_status "$BASE/api/orders/admin/fail?on=1" 200 --cacert "$CA"

capture --cacert "$CA" "$BASE/api/orders/echo"
served=$(header_from_capture "x-served-by")
body=$(cat "$CAP_BODY_FILE" 2>/dev/null || true)
if [ "$CAP_STATUS" = "503" ] && [ "$served" = "orders" ] && printf '%s' "$body" | grep -q "failing"; then
    ok "passthrough 503 'failing' with x-served-by: orders (upstream's own 5xx, forwarded)"
else
    fail "expected passthrough 503 'failing' with x-served-by:orders, got status=$CAP_STATUS served=$served body=$body"
fi

# Health probes count failures too, so the exact request that trips the
# breaker races with the health loop; don't assert an exact count, just
# that a refused (no x-served-by) 503 shows up within a handful of tries.
refused=0
i=0
while [ "$i" -lt 5 ]; do
    i=$((i + 1))
    capture --cacert "$CA" "$BASE/api/orders/echo"
    served=$(header_from_capture "x-served-by")
    if [ "$CAP_STATUS" = "503" ] && [ -z "$served" ]; then
        refused=1
        break
    fi
done
if [ "$refused" = 1 ]; then
    ok "breaker opened: ferryman-refused 503 'upstream unavailable' (no x-served-by) within 5 tries"
else
    fail "breaker never refused a request (no bare 503 without x-served-by) within 5 tries"
fi

expect_body_contains "$METRICS/metrics" 'ferryman_circuit_state{upstream="orders:8080"} 1'

# Recovery: the breaker now blocks the proxied admin/fail?on=0 too, and
# health probes keep seeing 503 from the still-failing process — so the
# real-world move is to restart (or replace) the sick instance. Restarting
# clears demo-upstream's in-memory fail flag; the next passing health
# check closes the circuit on its own.
docker compose restart orders
wait_until "/api/orders/echo recovers to 200 after restart" 15 orders_up

########################################################################
# Step 8: failover isolation (orders-v2 down doesn't affect orders)
########################################################################
step "Failover: stopping orders-v2 doesn't affect the orders route"
docker compose stop orders-v2

# A stopped upstream looks like a gateway error to ferryman either way,
# but which status depends on how fast the runner notices the container
# is gone: a fast connection refused / NXDOMAIN is 502; if DNS resolution
# or the connect attempt instead hangs past upstream_timeout_secs before
# failing (observed on GitHub Actions' ubuntu-latest runners — 2s per
# attempt there, vs near-instant on a typical dev box), ferryman's own
# timeout fires first and it's a 504 instead. Both mean "couldn't reach
# it" from the client's side, so treat them as the same signal. Each
# attempt can itself cost up to upstream_timeout_secs, so bound this by
# elapsed time (with a request cap as a backstop), not just a request
# count.
saw_gateway_error=0
saw_refused=0
codes_seen=""
start_ts=$(date +%s)
budget=20
tries=0
while :; do
    tries=$((tries + 1))
    code=$(status_of --cacert "$CA" --max-time 5 "$BASE/api/orders/v2/echo")
    codes_seen="$codes_seen $code"
    if [ "$code" = "502" ] || [ "$code" = "504" ]; then saw_gateway_error=1; fi
    if [ "$code" = "503" ]; then saw_refused=1; fi
    if [ "$saw_gateway_error" = 1 ] && [ "$saw_refused" = 1 ]; then break; fi
    if [ $(($(date +%s) - start_ts)) -ge "$budget" ] || [ "$tries" -ge 20 ]; then break; fi
done
if [ "$saw_gateway_error" = 1 ] && [ "$saw_refused" = 1 ]; then
    ok "orders-v2: saw a gateway error (502/504) then breaker-open 503 within ${budget}s (codes:${codes_seen})"
else
    fail "orders-v2: expected both a gateway error (502/504) and breaker-open 503 within ${budget}s (codes:${codes_seen})"
fi

expect_status "$BASE/api/orders/echo" 200 --cacert "$CA"
ok "orders route unaffected by orders-v2's outage (isolated breakers)"

########################################################################
# Step 9: Prometheus scraping, histogram series, alert firing
########################################################################
step "Prometheus: target up, histogram series, circuit-open alert fires"
# Prometheus's scrape_interval is 5s and the tour above can outrun that
# comfortably (cached-image runs finish steps 1-8 in well under 5s), so
# poll rather than asserting on the first query.
wait_until "Prometheus target job=ferryman is up" 15 prom_target_up
wait_until "ferryman_request_duration_seconds_bucket has series" 15 prom_has_duration_buckets

# orders-v2 is still down from step 8: poll here (while it's down) for the
# FerrymanCircuitOpen alert (for: 30s) to go from pending to firing.
wait_until "ALERTS{alertname=\"FerrymanCircuitOpen\"} visible in /api/v1/alerts" 60 circuit_open_alert_visible

echo "  bringing orders-v2 back up now that the alert has been observed"
recovery_start=$(date +%s)
docker compose start orders-v2
wait_until "/api/orders/v2/echo recovers to 200" 15 orders_v2_up
recovery_secs=$(($(date +%s) - recovery_start))
echo "  orders-v2 recovery time: ${recovery_secs}s"

########################################################################
# Step 10: Grafana health + provisioned dashboard
########################################################################
step "Grafana: health check and provisioned dashboard"
expect_status "$GRAFANA/api/health" 200
health_db=$(body_of "$GRAFANA/api/health" | jq -r '.database // empty')
if [ "$health_db" = "ok" ]; then
    ok "$GRAFANA/api/health database: ok"
else
    fail "$GRAFANA/api/health: expected .database == 'ok', got '$health_db'"
fi
expect_status "$GRAFANA/api/dashboards/uid/ferryman" 200

########################################################################
# Step 11: hot reload (valid, then invalid, then restore)
########################################################################
step "Hot reload: add a route, reject invalid TOML, restore"
{
    cat "$CONFIG_FILE"
    echo
    echo '[[routes]]'
    echo 'prefix = "/api/inventory"'
    echo 'upstream = "http://users:8080"'
} >"$CONFIG_TMP"
swap_config
wait_until "/api/inventory/echo returns 200 after reload" 15 inventory_up

invalid_since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
echo 'this is not valid toml [[[' >"$CONFIG_TMP"
swap_config_unlink
wait_until "invalid TOML logs a parse error (since ${invalid_since})" 15 reload_parse_error_logged "$invalid_since"
# The real proof the *old* table is still live: /api/inventory only
# exists because the previous (valid) swap added it, so it must still
# answer 200 here — checked *after* the parse-error log confirms the bad
# reload was rejected, not before (200 before that proves nothing about
# which table is serving).
expect_status "$BASE/api/inventory/echo" 200 --cacert "$CA"
expect_status "$BASE/api/users/echo" 200 --cacert "$CA"

cp "$CONFIG_BACKUP" "$CONFIG_TMP"
swap_config_unlink
wait_until "config restored: /api/inventory/echo goes back to 404" 15 inventory_gone

########################################################################
# Step 12: Upgrade requests are rejected
########################################################################
step "Upgrade requests (e.g. WebSocket) get 501, not proxied"
expect_status "$BASE/api/users/echo" 501 --cacert "$CA" --http1.1 -H "Connection: upgrade" -H "Upgrade: websocket"

########################################################################
# Step 13: graceful shutdown drains in-flight requests
########################################################################
step "Graceful shutdown drains an in-flight request on SIGTERM"
SHUTDOWN_STATUS_FILE=$(mktemp)
(status_of --cacert "$CA" --max-time 5 "$BASE/api/users/slow?ms=1500" >"$SHUTDOWN_STATUS_FILE") &
bgpid=$!
sleep 0.3
docker compose kill -s SIGTERM ferryman
wait "$bgpid" || true
shutdown_status=$(cat "$SHUTDOWN_STATUS_FILE" 2>/dev/null || true)
rm -f "$SHUTDOWN_STATUS_FILE"
if [ "$shutdown_status" = "200" ]; then
    ok "in-flight 1.5s request drained to 200 despite SIGTERM sent at 0.3s"
else
    fail "expected the in-flight request to drain with 200, got '$shutdown_status'"
fi
# A no-op SIGTERM would still let the in-flight request above finish (it
# was already being served) and still leave `users_up` passing once
# restarted below, so neither proves ferryman actually reacted to the
# signal. Assert the process itself exited(0) before restarting it.
wait_until "ferryman process exited(0) after SIGTERM" 30 ferryman_exited_cleanly
# `compose start` isn't dependency-aware the way `up` is (it can re-run
# certgen's completion check without ever actually starting ferryman
# afterwards); `up -d` on an existing, unchanged container just starts it,
# but properly waits on certgen first.
docker compose up -d ferryman
wait_until "ferryman back up after restart" 30 users_up

########################################################################
echo
echo "${BOLD}${GREEN}All ${PASS} assertions passed across ${STEP_NUM} steps.${RESET}"
