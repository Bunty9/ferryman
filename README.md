# ferryman

> Small L7 reverse proxy in Rust — prefix routing, per-upstream circuit
> breakers, active health checks, hot-reloaded TOML config. Pingora-pattern
> at miniature scale. Built to be readable in one sitting.

[![ci](https://img.shields.io/badge/ci-pending-lightgrey.svg)](./.github/workflows/ci.yml)
[![crates.io](https://img.shields.io/badge/crates.io-pending-lightgrey.svg)](#)
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
                       |  2. lookup in Arc<Vec<(prefix, Uri)>>
                       |     sorted DESC by prefix length
                       |  3. check upstream.alive (AtomicBool)
                       |  4. rebuild URI, forward via hyper client
                       v
       +---------------+---------+----------+
       |               |         |          |
   svc-A:8001     svc-B:8002  svc-C:8003  ...
       ^               ^         ^
       |               |         |
       +-------+-------+---------+
               |
        Background task:
        - active /health every 5s
        - circuit breaker state machine
        - on success: alive = true
        - on N consecutive fails: alive = false, cooldown=30s

Routing rules: TOML, hot-reloaded via `notify` filesystem watch + arc-swap.
```

## Stack

| Layer                | Crate / Tool                                                  |
| -------------------- | ------------------------------------------------------------- |
| Async runtime        | `tokio` 1.47 (full)                                           |
| HTTP server          | `hyper` 1.5 + `hyper-util` + `tower-http`                     |
| HTTP client (proxy)  | `hyper-util` legacy `Client<HttpConnector>`                   |
| HTTP client (health) | `reqwest` 0.12 (rustls-tls)                                   |
| TLS (Phase 2+)       | `rustls` 0.23 + `tokio-rustls`                                |
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

# In another shell, hit a route (404 expected until svc-a/svc-b are up):
curl -i http://localhost:8080/svc-a/hello
# HTTP/1.1 502 ...   (no svc-a:8001 listening yet)
curl -i http://localhost:8080/no-such-route
# HTTP/1.1 404 no route

# Scrape Prometheus metrics:
curl -s http://localhost:9090/metrics | head
```

Edit `config.toml` while ferryman is running — the routing table reloads
atomically without dropping live connections.

## Metrics endpoints

The Prometheus exporter binds a separate listener (default `:9090`).
Surface:

| Metric                              | Labels                                                    | Description                              |
| ----------------------------------- | --------------------------------------------------------- | ---------------------------------------- |
| `ferryman_requests_total`           | `status`, `route` (`"none"` on 404), `upstream` (Phase 2) | Counter of inbound requests.             |
| `ferryman_request_duration_seconds` | `upstream`                                                | Histogram of end-to-end forward latency. |
| `ferryman_upstream_alive`           | `upstream`                                                | Gauge: 1.0 = alive, 0.0 = circuit open.  |
| `ferryman_circuit_state` (Phase 2)  | `upstream`                                                | 0 closed / 1 open / 2 half-open.         |

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
    core/                    # Upstream, RouteTable, health loop, TOML schema
    server/                  # hyper service, proxy handler, reload watcher
  benches/wrk2.lua           # wrk2 harness for the throughput target
  scripts/wrk2-smoke.sh      # CI smoke wrapper
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

## Roadmap

Phase 1 (scaffold + `cargo check` green + first 404 served) is the current
sprint — see
[`docs/plans/2026-05-28-ferryman-phase-1-scaffold.md`](./docs/plans/2026-05-28-ferryman-phase-1-scaffold.md).
Subsequent phases (real upstream forwarding under load, full circuit-breaker
state machine, TLS termination via rustls, Fly.io demo) are tracked in
[`PROGRESS.md`](./PROGRESS.md). P4 (ferryman-edge) layers mTLS + JWT +
hot-reload on top of this base; see `projects-l3-l4.md` § P4.

## License <a id="license"></a>

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](./LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.
