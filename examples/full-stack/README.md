# full-stack reference example

A complete docker-compose stack that puts ferryman in front of three fake
backends, TLS-terminates in front of it, and wires up Prometheus + Grafana
behind it — the shape of a real deployment, small enough to read in one
sitting. `demo.sh` drives it through routing, TLS/HTTP2, streaming, the
circuit breaker, failover, alerting, hot reload, upgrade rejection, and
graceful shutdown, asserting on the real HTTP responses the whole way.

## Topology

```
                          host
        ┌──────────────────────────────────────┐
        │ :8443 HTTPS   :9090 metrics           │
        └───────────────┬───────────┬───────────┘
                         │           │
                 ┌───────▼───────────▼───────┐
                 │         ferryman          │
                 │  TLS term, routing,       │
                 │  breaker, hot reload       │
                 └──┬───────┬────────┬────────┘
                    │       │        │
         /api/users │       │/api/orders     /api/orders/v2
         /account    │       │(not /v2)        (longest match)
                    ▼       ▼        ▼
                 ┌──────┐┌──────┐┌──────────┐
                 │users ││orders││orders-v2 │   demo-upstream
                 │:8080 ││:8080 ││:8080     │   (fake JSON echo
                 └──────┘└──────┘└──────────┘    backend, one env
                                                  var: SERVICE_NAME)

        ┌──────────────┐        ┌──────────────┐
        │  Prometheus  │◀──5s───│   ferryman   │
        │  :9091→9090  │ scrape │   :9090      │
        └──────┬───────┘        └──────────────┘
               │ datasource (uid: prometheus)
               ▼
        ┌──────────────┐
        │   Grafana    │  anonymous Viewer, dashboard uid: ferryman
        │  :3000       │
        └──────────────┘

        ┌──────────────┐
        │   certgen    │  alpine + openssl, writes ./certs once, exits
        └──────────────┘  (ferryman waits on service_completed_successfully)
```

`/api/orders/v2` is a *longer* prefix than `/api/orders`, so it wins the
routing match even though both would otherwise match a request under
`/api/orders/v2/...` — this is the same longest-prefix-first rule the core
library uses everywhere (see `docs/architecture.md` in the repo root).

## What each file demonstrates

| File | Demonstrates |
| --- | --- |
| `docker-compose.yml` | Full topology: build-from-source ferryman + demo-upstream, pinned Prometheus/Grafana images, directory mounts, `stop_grace_period`, `depends_on` with `service_completed_successfully` |
| `config/ferryman.toml` | Prefix routing, two routes sharing one upstream (shared breaker), a per-route `cooldown_secs` override, longest-prefix-first (`/api/orders` vs `/api/orders/v2`) |
| `scripts/gen-certs.sh` | A throwaway CA + server cert (SANs: `localhost`, `127.0.0.1`, `ferryman`), idempotent, run once by the `certgen` service |
| `prometheus/prometheus.yml` | Scraping ferryman's `/metrics` on a 5s interval |
| `prometheus/alerts.yml` | Five alert rules over ferryman's own metrics: target down, circuit open, upstream down, high 5xx ratio, slow p99 |
| `grafana/provisioning/` | Auto-provisioned Prometheus datasource (uid `prometheus`) and dashboard, no manual clicking |
| `grafana/dashboards/ferryman.json` | Requests/s by route×status, 5xx ratio, p50/p99 latency, circuit state timeline, upstream-alive, scrape `up` |
| `demo.sh` | A scripted, asserted tour of routing, TLS/HTTP2, forwarded headers, streaming, timeouts, the breaker, failover, alerting, hot reload, `Upgrade` rejection, and graceful shutdown |

## Quick start

```bash
cd examples/full-stack
./demo.sh          # interactive: pauses between steps
./demo.sh --ci      # non-interactive: no pauses, no color (what CI runs)
```

Needs `curl`, `jq`, and Docker Compose v2 on the host; the script checks
for all three up front. The first `--build` compiles ferryman from source
(cargo-chef + musl, a few minutes); after that, only genuinely changed
layers rebuild.

The root `Dockerfile` builds natively on an amd64 or an arm64 host (e.g.
Apple Silicon's Docker Desktop, which defaults to `linux/arm64` — it picks
the right musl target via BuildKit's `TARGETARCH`). Cross-arch builds
(building a `linux/arm64` image from an amd64 host, or vice versa) aren't
supported; see the comment at the top of the `Dockerfile`.

While the stack is up:

- ferryman: `https://localhost:8443` (see `certs/ca.pem` for the CA)
- ferryman metrics: `http://localhost:9090/metrics`
- Prometheus: `http://localhost:9091`
- Grafana: `http://localhost:3000` (anonymous Viewer access; the
  "ferryman" dashboard is auto-provisioned, no login or setup needed —
  find it under Dashboards, or go straight to `/d/ferryman/ferryman`)

All four ports are overridable: `FERRYMAN_HTTPS_PORT`,
`FERRYMAN_METRICS_PORT`, `PROMETHEUS_PORT`, `GRAFANA_PORT`. `docker-compose.yml`
publishes all of them on `127.0.0.1` only — this is a local demo (and
Grafana's default `admin`/`admin` login is live, not something to expose);
to deliberately reach a service from another host, drop the `127.0.0.1:`
prefix on that service's `ports:` entry.

Importing `grafana/dashboards/ferryman.json` into a Grafana you already run
(rather than this example's auto-provisioned one): the dashboard's panels
are pinned to datasource uid `prometheus` (provisioned dashboards can't use
the `${DS_PROMETHEUS}` template-variable `__inputs` prompt Grafana's export
UI normally offers on import), so either name your existing Prometheus
datasource's uid `prometheus`, or edit the uid in the JSON after importing.

`demo.sh` tears the stack down (`docker compose down -v`) on exit,
including on failure. Set `KEEP=1` to leave it running for manual
poking afterward.

## The tour, step by step

Every step below is exactly what `demo.sh` asserts, plus the equivalent
curl you can run by hand once the stack is up (`export CA=certs/ca.pem`
first).

**1. Bring the stack up**

```bash
docker compose up -d --build
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/users/echo
```

**2. Routing: prefix match, longest-prefix-first, 404**

```bash
curl -sS --cacert "$CA" https://localhost:8443/api/users/echo | jq .              # x-served-by: users
curl -sSD - --cacert "$CA" https://localhost:8443/api/orders/v2/echo -o /dev/null # x-served-by: orders-v2 (longest match)
curl -sSD - --cacert "$CA" https://localhost:8443/api/orders/echo -o /dev/null    # x-served-by: orders
curl -sSD - --cacert "$CA" https://localhost:8443/account/echo -o /dev/null       # x-served-by: users (shared upstream)
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/usersx  # 404
curl -sS --cacert "$CA" https://localhost:8443/nope                              # 404 "no route"
```

**3. TLS termination + HTTP/2 ALPN**

```bash
curl -sS -o /dev/null --cacert "$CA" --http2   -w '%{http_version}\n' https://localhost:8443/api/users/echo  # 2
curl -sS -o /dev/null --cacert "$CA" --http1.1 -w '%{http_version}\n' https://localhost:8443/api/users/echo  # 1.1
```

**4. Forwarded headers; hop-by-hop headers stripped**

```bash
curl -sS --cacert "$CA" --http1.1 -H 'Connection: x-secret' -H 'x-secret: 1' \
  https://localhost:8443/api/users/echo | jq '.headers["x-forwarded-proto"], .headers["x-forwarded-for"], (.headers | has("x-secret"))'
# "https", "<client ip>", false — x-secret never reached the upstream
```

**5. Streaming responses; large bodies aren't buffered**

```bash
curl -sS -o /dev/null --cacert "$CA" -w 'ttfb=%{time_starttransfer}s total=%{time_total}s\n' \
  "https://localhost:8443/api/users/stream?chunks=5&interval_ms=400"
# ttfb well under 1s (first chunk streams immediately), total >= 1.6s (5 chunks * 400ms, minus the first chunk's own interval)

head -c 8388608 /dev/urandom | \
  curl -sS --cacert "$CA" --data-binary @- https://localhost:8443/api/users/echo | jq .body_bytes
# 8388608
```

**6. Upstream timeout**

```bash
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" --max-time 10 \
  "https://localhost:8443/api/users/slow?ms=4000"   # 504 (upstream_timeout_secs=2 in this demo)
```

**7. A passed-through upstream 5xx trips the breaker; a restart clears it**

```bash
curl -sS --cacert "$CA" "https://localhost:8443/api/orders/admin/fail?on=1"    # 200
curl -sSD - --cacert "$CA" https://localhost:8443/api/orders/echo             # 503 "failing", x-served-by: orders (passed through)
# a few requests later (health probes count too, so the exact one races):
curl -sSD - --cacert "$CA" https://localhost:8443/api/orders/echo             # 503 "upstream unavailable", NO x-served-by (ferryman refused)
curl -s http://localhost:9090/metrics | grep 'ferryman_circuit_state{upstream="orders:8080"}'  # ... 1  (open)

# Recovery: the breaker now blocks the proxied admin/fail?on=0 too, so the
# real move is to restart (or replace) the sick instance:
docker compose restart orders
# a passing health probe closes an open circuit immediately (it's the
# breaker's half-open Probe admission, and success closes on the spot) —
# the cooldown only gates *when* a probe is admitted, and it's typically
# already elapsed by the time you restart (it started ticking back when the
# circuit first opened, not at restart time). So the wait here is ≈ one
# health_interval_secs (2s in this demo), not health_interval + cooldown:
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/orders/echo  # 200
```

**8. Failover: one upstream's outage doesn't touch another route**

```bash
docker compose stop orders-v2
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/orders/v2/echo  # gateway error at first, then 503 (breaker open)
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/orders/echo     # still 200 — different breaker, different upstream
docker compose start orders-v2
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/orders/v2/echo  # back to 200 within ~15s
```

> **502 vs 504 for a stopped upstream:** which "gateway error" status you
> see first depends on how the host notices the container is gone. A
> fast connection refused or NXDOMAIN is a **502**. If DNS resolution or
> the connect attempt instead hangs — observed on GitHub Actions'
> `ubuntu-latest` runners — ferryman's own `upstream_timeout_secs` (2s in
> this demo) fires first and it's a **504** instead. Both mean "couldn't
> reach the upstream" and both count as a breaker failure; `demo.sh`
> treats them as the same signal rather than assuming 502 specifically.



**9. Prometheus: scraping, histograms, and the circuit-open alert**

```bash
curl -s 'http://localhost:9091/api/v1/query?query=up{job="ferryman"}' | jq '.data.result[0].value[1]'   # "1"
curl -s 'http://localhost:9091/api/v1/query?query=ferryman_request_duration_seconds_bucket' | jq '.data.result | length'  # > 0
curl -s http://localhost:9091/api/v1/alerts | jq '.data.alerts[] | select(.labels.alertname=="FerrymanCircuitOpen")'
```

**10. Grafana: health check and the provisioned dashboard**

```bash
curl -s http://localhost:3000/api/health | jq .database         # "ok"
curl -sS -o /dev/null -w '%{http_code}\n' http://localhost:3000/api/dashboards/uid/ferryman  # 200
```
Or just open `http://localhost:3000/d/ferryman/ferryman` in a browser —
anonymous viewer access is enabled, no login needed.

**11. Hot reload: add a route, reject invalid TOML, restore**

```bash
# add /api/inventory by writing a temp file and renaming it over the target
# (the pattern editors and `mv`-based deploy tooling use):
cp config/ferryman.toml /tmp/orig.toml
{ cat config/ferryman.toml; echo; echo '[[routes]]'; echo 'prefix = "/api/inventory"'; echo 'upstream = "http://users:8080"'; } > config/.tmp
mv config/.tmp config/ferryman.toml
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/inventory/echo  # 200, within ~1s

echo 'not valid toml [[[' > config/.tmp && rm -f config/ferryman.toml && mv config/.tmp config/ferryman.toml
docker compose logs ferryman | grep 'parsing config file'   # logged, with the parse error — wait for this line first
# /api/inventory only exists because the *previous* swap added it, so it
# still answering 200 here (checked *after* the parse error is logged) is
# the actual proof the bad reload was rejected and the old table is still
# live — /api/users would return 200 either way and proves nothing:
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" https://localhost:8443/api/inventory/echo  # still 200 — old table kept

cp /tmp/orig.toml config/ferryman.toml   # restore
```

> **Docker Desktop / VM-backed file sharing:** if Docker itself runs
> inside a VM (Docker Desktop on macOS/Windows, or a nested setup like
> this repo's own dev host — check `docker info | grep -i kernel` for a
> `-linuxkit` or similar kernel), a *second* bare `mv` onto a
> bind-mounted path that was already rename-replaced once may not
> propagate into the container promptly (or at all) — the container can
> keep serving the previous version indefinitely. A plain Linux Docker
> host doesn't have this problem. If a reload isn't picking up on such a
> host, `rm` the target before the `mv` (an unlink forces the mount to
> re-resolve the path) or just restart the container. `demo.sh` does
> this automatically after its first config swap (see `swap_config` vs
> `swap_config_unlink` in the script).

**12. `Upgrade` requests are rejected, not proxied**

```bash
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" --http1.1 \
  -H 'Connection: upgrade' -H 'Upgrade: websocket' https://localhost:8443/api/users/echo  # 501
```

**13. Graceful shutdown drains an in-flight request**

```bash
curl -sS -o /dev/null -w '%{http_code}\n' --cacert "$CA" "https://localhost:8443/api/users/slow?ms=1500" &
sleep 0.3
docker compose kill -s SIGTERM ferryman   # sent mid-request
wait   # the backgrounded curl still prints 200 — SIGTERM drains, doesn't cut off
docker compose up -d ferryman
```

## Timings shortened for the demo

So the tour above finishes in well under a minute instead of the length
of a real cooldown/timeout, `config/ferryman.toml` uses:

| Setting | Demo value | Production value |
| --- | --- | --- |
| `health_interval_secs` | 2 | 5 |
| `default_cooldown_secs` | 5 | 30 |
| `failure_threshold` | 3 | 3 (unchanged) |
| `upstream_timeout_secs` | 2 | 30 |

Each is commented in `config/ferryman.toml` with its production value.

## Taking it to production

- **Real values.** Use the production column above (or your own numbers)
  — a 2s health interval and 2s upstream timeout are fine for a demo, not
  for real upstreams with real tail latency.
- **Real certs.** `scripts/gen-certs.sh` makes a throwaway self-signed CA
  purely so `demo.sh` can talk TLS without `-k`. In production, terminate
  with certs from a real CA (ACME/Let's Encrypt, your org's internal CA,
  or a cloud load balancer in front of ferryman) — never ship this script
  or its output past a laptop.
- **`stop_grace_period: 30s`.** ferryman's own SIGTERM handling drains
  in-flight connections for up to 25s (see `docs/architecture.md`); the
  container orchestrator's grace period must be longer than that or it
  will SIGKILL mid-drain. 30s here, matching `fly.toml`'s
  `kill_timeout` in the main repo.
- **Directory mounts, not single-file mounts.** `ferryman` mounts
  `./config:/etc/ferryman:ro` — a *directory* — because hot reload watches
  the config file's **parent directory** to catch rename-replace saves
  (the pattern most editors and deploy tools use, and the same pattern
  `demo.sh`'s hot-reload step exercises). A single-file bind mount
  (`-v ./config/ferryman.toml:/etc/ferryman/ferryman.toml:ro`) replaces
  the mounted inode on the container side in a way the watcher can't see
  the same way; always mount the containing directory.
- **Kubernetes ConfigMaps.** A ConfigMap mounted as a volume updates via
  an atomic swap of a `..data` symlink, not a rename-replace or in-place
  write on the file ferryman actually opens. The watcher does not see
  that swap, so hot reload silently stops working under a ConfigMap mount
  — restart the pod (or use a sidecar/init pattern that does a real
  rename-replace) after a config change, and don't rely on live reload
  there the way this demo does with a plain bind mount.
- **No `docker HEALTHCHECK` is possible.** The ferryman image is `FROM
  scratch` — no shell, no `curl`, nothing a `HEALTHCHECK` instruction
  could exec. Rely on Prometheus's own `up` metric (see
  `prometheus/alerts.yml`'s `FerrymanDown` rule) or an external prober
  hitting `/metrics` or a route through the proxy, not a container-level
  health check.

## Using crates.io instead of this repo

The image this example builds compiles ferryman from a local checkout.
Building from the published crate instead only needs a `Cargo.toml`-free
Dockerfile:

```dockerfile
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
RUN cargo install --locked ferryman

FROM scratch
COPY --from=builder /usr/local/cargo/bin/ferryman /usr/local/bin/ferryman
COPY ferryman.toml /etc/ferryman/ferryman.toml
EXPOSE 8080 9090
ENTRYPOINT ["/usr/local/bin/ferryman"]
CMD ["--config", "/etc/ferryman/ferryman.toml"]
```

`rust:1-alpine` already has a musl target compiler, so `cargo install`
produces a static binary without cargo-chef's dependency-caching dance —
you're trading a slower single-stage build (recompiles ferryman's whole
dependency tree on every version bump) for a much shorter Dockerfile,
which is usually the right trade for a consumer of the crate rather than
a contributor to it.

**Use ferryman 0.2.0 or later.** 0.1.0 exported request duration as a
Prometheus *summary* (`ferryman_request_duration_seconds{quantile=...}`)
rather than the histogram (`..._bucket`) that this example's
`prometheus/alerts.yml` and Grafana latency panel query with
`histogram_quantile(...)`. Against a 0.1.0 binary the latency panel stays
empty and `FerrymanSlowP99` never fires; everything else works.
