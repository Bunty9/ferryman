# ferryman

> The small, readable Rust reverse proxy with a production-grade circuit
> breaker, which you can also embed as a crate. Prefix routing, per-upstream
> circuit breakers, active health checks, hot-reloaded TOML config.
> Pingora-pattern at miniature scale.

**Documentation: <https://bunty9.github.io/ferryman/>**

[![ci](https://github.com/Bunty9/ferryman/actions/workflows/ci.yml/badge.svg)](https://github.com/Bunty9/ferryman/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ferryman.svg)](https://crates.io/crates/ferryman)
[![docs.rs](https://img.shields.io/docsrs/ferryman)](https://docs.rs/ferryman)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## Features

- Longest-prefix routing on path-segment boundaries; optional `strip_prefix`
  and `rewrite_host` per route.
- Lock-free per-upstream circuit breaker with `Admission` tickets, plus active
  health checks that close a recovered circuit within one interval.
- Hot reload of config and TLS certificate (file watch, `SIGHUP`, Kubernetes
  ConfigMap/Secret swaps); a bad config never replaces a working one.
- Streaming bodies, HTTP/1.1 and HTTP/2 listener (ALPN), TLS termination.
- Forwarded headers that clients cannot forge (`trusted_proxies`).
- Admin listener (`/metrics`, `/healthz`, `/readyz`), opt-in access log and
  request ids, graceful drain tuned for load balancers and PaaS.
- Publishable crates: embed the proxy (`ferryman`) or just the breaker
  (`ferryman-core`).

Limitations today: plain-HTTP upstreams only, one upstream per route, no
WebSocket/`Upgrade` passthrough (501), no built-in ACME. See the
[comparison](https://bunty9.github.io/ferryman/comparison.html) for when to
choose Caddy, Traefik, NGINX, rpxy or Pingora instead.

## Install

```bash
docker run --rm --read-only -p 8080:8080 -p 127.0.0.1:9090:9090 \
  -v "$PWD/config:/etc/ferryman:ro" \
  ghcr.io/bunty9/ferryman:latest --config /etc/ferryman/ferryman.toml

cargo binstall ferryman          # downloads the release archive (releases after 0.2.1)
cargo install --locked ferryman  # builds from crates.io (Rust 1.88+)
```

Prebuilt binaries (with checksums and build provenance attestations,
`gh attestation verify`) are attached to every
[GitHub release](https://github.com/Bunty9/ferryman/releases). The image is
`FROM scratch`, multi-arch and runs as uid 65532; mounted files must be
readable by it. Details: [Quickstarts](https://bunty9.github.io/ferryman/quickstarts.html).

## Quick start

```bash
ferryman --config config.toml
curl -i http://localhost:8080/svc-a/hello     # 502, then 503 once the circuit opens
curl -i http://localhost:8080/no-such-route   # 404 no route
curl -s http://localhost:9090/metrics | head
```

```toml
[[routes]]
prefix = "/app"
upstream = "http://app:8000"
```

Edit the file while ferryman runs: the routing table reloads atomically
without dropping live connections. Unknown keys are rejected;
`ferryman check --config X` validates a file without binding.

## Documentation

The [book](https://bunty9.github.io/ferryman/) is the detailed home:

| Topic | Chapter |
| --- | --- |
| Docker, binstall, systemd, Kubernetes, Compose, Cloud Run/Render/Railway/Heroku/Fly | [Quickstarts](https://bunty9.github.io/ferryman/quickstarts.html) |
| Every config key, default and reload behaviour | [Configuration reference](https://bunty9.github.io/ferryman/configuration.html) |
| Prefix matching, normalisation, `strip_prefix`, `rewrite_host` | [Routing](https://bunty9.github.io/ferryman/routing.html) |
| `X-Forwarded-*`, `Forwarded`, `X-Real-IP`, `trusted_proxies` | [Forwarded headers](https://bunty9.github.io/ferryman/forwarded-headers.html) |
| States, `Admission`, health-driven recovery, failure statuses | [Circuit breaker](https://bunty9.github.io/ferryman/circuit-breaker.html) |
| Keep-alive/ALB, body and upstream timeouts, shutdown sizing | [Timeouts & limits](https://bunty9.github.io/ferryman/timeouts.html) |
| Admin endpoints, probes, reload, drain, access log, metrics, Grafana | [Operations](https://bunty9.github.io/ferryman/operations.html) |
| Cargo features, `serve`, standalone `Breaker` | [Embedding the library](https://bunty9.github.io/ferryman/embedding.html) |
| Reporting, attestations, hardening | [Security](https://bunty9.github.io/ferryman/security.html) |
| vs Caddy/Traefik/NGINX/rpxy/Pingora; nginx and Caddy migration | [Comparison & migration](https://bunty9.github.io/ferryman/comparison.html) |

## Deploy

| Where | Recipe | Checked in CI |
| --- | --- | --- |
| Docker Compose | [`examples/full-stack/`](./examples/full-stack): TLS, upstreams, Prometheus, Grafana | `demo.sh --ci` |
| Kubernetes | [`deploy/k8s/`](./deploy/k8s): Restricted PSS, probes, ConfigMap reload, graceful shutdown | kind job |
| systemd | [`deploy/systemd/`](./deploy/systemd) | unit tests |
| Fly.io | [`fly.toml`](./fly.toml) | n/a |
| Cloud Run, Render, Railway, Heroku, ECS | [Quickstarts](https://bunty9.github.io/ferryman/quickstarts.html#paas-and-orchestrators) | `crates/server/tests/paas.rs` |

## Examples

Reference examples live under [`examples/`](./examples) (index:
[`examples/README.md`](./examples/README.md)): `full-stack` (Compose with TLS,
Prometheus, Grafana), `embedded` (`ferryman::serve` inside your own app, plus
a standalone `Breaker` around a `reqwest` call) and the core crate's
[`guarded_client.rs`](./crates/core/examples/guarded_client.rs).

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

## Repository layout

```
ferryman/
  Cargo.toml                 # workspace
  config.toml                # example routing config
  crates/
    core/                    # breaker, RouteTable, health loop, TOML schema
      benches/lookup.rs      # criterion bench for RouteTable::lookup
      examples/guarded_client.rs  # breaker guarding a fallible async call
    server/                  # serve loop, proxy handler, TLS, reload watcher
      tests/proxy.rs         # end-to-end tests on real sockets
  examples/                  # reference examples, see examples/README.md
    full-stack/              # Docker Compose: ferryman + upstreams + Prometheus/Grafana
    embedded/                # ferryman::serve embedded as a library
  benches/wrk2.lua           # wrk2 harness for the throughput target
  benches/bench.toml         # routing config for the compose fixture
  docker-compose.bench.yml   # ferryman + two http-echo upstreams
  scripts/wrk2-smoke.sh      # boots the compose fixture and runs wrk2
  Dockerfile                 # cargo-chef multi-stage, scratch final (musl)
  fly.toml                   # Fly.io 2-region (sin + iad)
  deploy/                    # k8s manifests (kind-tested) and the systemd unit
  deny.toml                  # cargo-deny config
  rust-toolchain.toml        # stable channel
  .github/workflows/ci.yml   # nextest + clippy + fmt + deny + bench + wrk2 + examples
  docs/
    architecture.md          # internals: request path, breaker, reload
    specs/2026-05-28-ferryman-design.md
    plans/2026-05-28-ferryman-phase-1-scaffold.md
    plans/2026-09-26-ferryman-phase-2.md
  PROGRESS.md
```

## Limitations

See [Comparison & migration](https://bunty9.github.io/ferryman/comparison.html#current-limitations).

## Roadmap

How it works inside: [`docs/architecture.md`](./docs/architecture.md).

Phases and bench numbers are tracked in [`PROGRESS.md`](./PROGRESS.md).
P4 (ferryman-edge) layers mTLS + JWT + cert hot-reload on top of this
base; see `projects-l3-l4.md` § P4.

## Security

Report vulnerabilities privately, see [SECURITY.md](./SECURITY.md).

## License <a id="license"></a>

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](./LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.
