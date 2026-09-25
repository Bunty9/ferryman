# ferryman

> Small L7 reverse proxy in Rust — prefix routing, per-upstream circuit
> breakers, active health checks, hot-reloaded TOML config. Pingora-pattern
> at miniature scale. Built to be readable in one sitting.

[![ci](https://github.com/Bunty9/ferryman/actions/workflows/ci.yml/badge.svg)](https://github.com/Bunty9/ferryman/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## The problem

Typical self-hosted setups expose services on direct ports or tunnels with
no L7 in front. Real teams put L7 proxies between clients and origin:
routing, health checks, observability, TLS termination. **ferryman** is a
hyper-based reverse proxy that does the boring parts well and exposes the
interesting tradeoffs — circuit-breaker state machine, atomic config
hot-reload, prefix-longest-match routing — in code small enough to read
top-to-bottom.

## Architecture

```
                    +-----------------------------+
                    |    ferryman :443/:80        |
                    |  hyper 1.x + tower stack    |
                    +--+--------------------------+
                       |
                       |  1. parse req.uri.path
                       |  2. longest prefix match on a path-segment
                       |     boundary (no match -> 404)
                       |  3. ask the upstream's breaker (open -> 503)
                       |  4. rebuild URI, stream via hyper client
                       v
       +---------------+---------+----------+
       |               |         |          |
   svc-A:8001     svc-B:8002  svc-C:8003  ...
       ^               ^         ^
       |               |         |
       +-------+-------+---------+
               |
        Background task:
        - active /health every 5s (any answer < 500 = up)
        - lock-free closed / open / half-open breaker
        - N consecutive failures: open for cooldown (30s)
        - after cooldown: exactly one half-open probe
        - health success: close immediately

Routing rules: TOML, hot-reloaded via `notify` filesystem watch + arc-swap.
```

## Stack

| Layer                | Crate / Tool                                                  |
| -------------------- | ------------------------------------------------------------- |
| Async runtime        | `tokio` 1.47 (full)                                           |
| HTTP server          | `hyper` 1.x + `hyper-util` auto (HTTP/1.1 + HTTP/2)           |
| HTTP client (proxy)  | `hyper-util` legacy `Client<HttpConnector>`                   |
| HTTP client (health) | `reqwest` 0.12 (plain HTTP, no TLS stack)                     |
| TLS termination      | `rustls` 0.23 (`ring`) + `tokio-rustls`, ALPN h2 / http/1.1   |
| Config               | `serde` + `toml` + `notify` (FS watch)                        |
| Hot-swap             | `arc-swap` (atomic `RouteTable` swap)                         |
| Observability        | `tracing` + `metrics-exporter-prometheus`                     |
| CLI                  | `clap` 4                                                      |
| Container build      | `cargo-chef` multi-stage, `FROM scratch`                      |
| Deploy               | Fly.io 2-region (`sin` + `iad`)                               |
| CI                   | GHA (stable + beta) + `cargo-deny` + `cargo-nextest` + `wrk2` |

Full pinned versions live in [`Cargo.toml`](./Cargo.toml). Default routing
rules: [`config.toml`](./config.toml).

## Quick start

```bash
# Build and run against the example config (2 upstreams, 5s health interval).
cargo run -p ferryman-server -- --config config.toml

# Optional: local upstreams that answer 200 on every path.
cargo run --release -p ferryman-server --example echo_upstream -- 127.0.0.1:8001 &

# In another shell, hit a route:
curl -i http://localhost:8080/svc-a/hello
# HTTP/1.1 502 bad gateway          (no svc-a:8001 listening yet)
# HTTP/1.1 503 upstream unavailable (once its circuit has opened)
curl -i http://localhost:8080/no-such-route
# HTTP/1.1 404 no route

# Optional TLS termination (HTTP/2 via ALPN):
cargo run -p ferryman-server -- --config config.toml \
    --tls-cert cert.pem --tls-key key.pem

# Scrape Prometheus metrics:
curl -s http://localhost:9090/metrics | head
```

Edit `config.toml` while ferryman is running — the routing table reloads
atomically without dropping live connections.

## Configuration

See [`config.toml`](./config.toml). Unknown keys are rejected, so typos
fail loudly instead of silently falling back to defaults.

| Key                         | Default | Meaning                                                      |
| --------------------------- | ------- | ------------------------------------------------------------ |
| `health_interval_secs`      | 5       | Active `/health` probe interval (restart to change).         |
| `default_cooldown_secs`     | 30      | How long an open circuit refuses traffic before one probe.   |
| `failure_threshold`         | 3       | Consecutive failures that open a closed circuit.             |
| `upstream_timeout_secs`     | 30      | Time allowed for an upstream to send response headers.       |
| `[[routes]] prefix`         | —       | Path prefix, matched on segment boundaries (`/a` ≠ `/ab`).   |
| `[[routes]] upstream`       | —       | `http://host:port` — no path, no query, no https.            |
| `[[routes]] cooldown_secs`  | default | Per-route cooldown override.                                 |

Routes pointing at the same `host:port` share one circuit breaker. A hot
reload keeps each surviving upstream's breaker, so an open circuit stays
open across a config edit. An invalid config on reload is logged and the
old table stays live.

CLI flags (env var in brackets): `--config` (`FERRYMAN_CONFIG`), `--bind`
(`FERRYMAN_BIND`, `0.0.0.0:8080`), `--metrics-bind`
(`FERRYMAN_METRICS_BIND`, `0.0.0.0:9090`), `--tls-cert` / `--tls-key`
(`FERRYMAN_TLS_CERT` / `FERRYMAN_TLS_KEY`).

## Behaviour

| Situation                                              | Response | Counts against breaker |
| ------------------------------------------------------ | -------- | ---------------------- |
| No route matches                                       | 404      | —                      |
| Circuit open                                           | 503      | —                      |
| `Upgrade` / `CONNECT` (e.g. WebSocket)                 | 501      | —                      |
| Connect / transport error                              | 502      | yes                    |
| No response headers within `upstream_timeout_secs`     | 504      | only for bodyless requests |
| Client's request body fails mid-upload                 | 400      | no                     |
| Upstream answers 502/503/504                           | passed through | yes              |
| Anything else from upstream                            | passed through | success          |

Request and response bodies are streamed, never buffered. Hop-by-hop
headers are stripped both ways; `x-forwarded-for` and `x-forwarded-proto`
are set; the client's `Host` is kept (HTTP/2 `:authority` becomes `Host`).
Upstreams always get HTTP/1.1. Slow clients are cut off after 10s of
header reading or TLS handshake. SIGINT/SIGTERM stop accepting and drain
in-flight connections for up to 25s.

## Metrics endpoints

The Prometheus exporter binds a separate listener (default `:9090`).
Surface:

| Metric                              | Labels                                             | Description                                   |
| ----------------------------------- | -------------------------------------------------- | --------------------------------------------- |
| `ferryman_requests_total`           | `route`, `upstream`, `status` (`"none"` on 404)    | Counter of inbound requests.                  |
| `ferryman_request_duration_seconds` | `route`, `upstream`                                | Histogram, time to upstream response headers. |
| `ferryman_upstream_alive`           | `upstream`                                         | Gauge: 1 = circuit closed, 0 otherwise.       |
| `ferryman_circuit_state`            | `upstream`                                         | Gauge: 0 closed / 1 open / 2 half-open.       |

`route` is the configured prefix and `upstream` is `host:port`, so label
cardinality is bounded by the config, never by client input.

## Bench targets (per `projects-l3-l4.md` § P2)

Run with [`benches/wrk2.lua`](./benches/wrk2.lua):

```bash
wrk2 -c 1000 -t 16 -R 50000 -d 60s \
     -s benches/wrk2.lua \
     http://localhost:8080/svc-a/echo
```

| Metric                                  | Target                                 |
| --------------------------------------- | -------------------------------------- |
| Throughput                              | 50,000 rps                             |
| p50 latency                             | < 1 ms                                 |
| p99 latency                             | < 5 ms                                 |
| p999 latency                            | < 20 ms                                |
| RSS at 50k rps idle                     | < 20 MB                                |
| Failover recovery after upstream `kill` | < 5 s (one health interval)            |
| CPU vs NGINX on same hardware           | >= 50% reduction (Pingora story: ~70%) |

## Repository layout

```
ferryman/
  Cargo.toml                 # workspace
  config.toml                # example routing config
  crates/
    core/                    # breaker, RouteTable, health loop, TOML schema
      benches/lookup.rs      # criterion bench for RouteTable::lookup
    server/                  # serve loop, proxy handler, TLS, reload watcher
      tests/proxy.rs         # end-to-end tests on real sockets
  benches/wrk2.lua           # wrk2 harness for the throughput target
  benches/bench.toml         # routing config for the compose fixture
  docker-compose.bench.yml   # ferryman + two http-echo upstreams
  scripts/wrk2-smoke.sh      # boots the compose fixture and runs wrk2
  Dockerfile                 # cargo-chef multi-stage, scratch final (musl)
  fly.toml                   # Fly.io 2-region (sin + iad)
  deny.toml                  # cargo-deny config
  rust-toolchain.toml        # stable channel
  .github/workflows/ci.yml   # nextest + clippy + fmt + deny + bench + wrk2
  docs/
    specs/2026-05-28-ferryman-design.md
    plans/2026-05-28-ferryman-phase-1-scaffold.md
  PROGRESS.md
```

## Limitations

- Upstreams are plain HTTP only; `https://` upstreams are rejected.
- No WebSocket / `Upgrade` passthrough (501).
- No per-upstream load balancing: one route, one upstream.
- Kubernetes ConfigMap mounts update via a `..data` symlink swap that
  the file watcher does not see; restart the pod after a ConfigMap edit.
- The response body has no idle timeout once headers have arrived.

## Roadmap

Phases and bench numbers are tracked in [`PROGRESS.md`](./PROGRESS.md).
P4 (ferryman-edge) layers mTLS + JWT + cert hot-reload on top of this
base; see `projects-l3-l4.md` § P4.

## License <a id="license"></a>

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](./LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.
