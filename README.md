# ferryman

> Small L7 reverse proxy in Rust — prefix routing, per-upstream circuit
> breakers, active health checks, hot-reloaded TOML config. Pingora-pattern
> at miniature scale. Built to be readable in one sitting.

[![ci](https://github.com/Bunty9/ferryman/actions/workflows/ci.yml/badge.svg)](https://github.com/Bunty9/ferryman/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ferryman.svg)](https://crates.io/crates/ferryman)
[![docs.rs](https://img.shields.io/docsrs/ferryman)](https://docs.rs/ferryman)
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

## Install

**Prebuilt binaries** are attached to every
[GitHub release](https://github.com/Bunty9/ferryman/releases) as
`ferryman-v<version>-<target>.tar.gz` (`.zip` on Windows), each holding
the binary, README, CHANGELOG, both licenses and an example `config.toml`.
Required targets: `x86_64`/`aarch64-unknown-linux-musl` (static),
`x86_64`/`aarch64-apple-darwin`, `x86_64-pc-windows-msvc`. Best effort
(may be missing from a release): `armv7`/`arm-unknown-linux-musleabihf`,
`i686-unknown-linux-musl`, `riscv64gc-unknown-linux-gnu`,
`aarch64-pc-windows-msvc`, `x86_64-unknown-freebsd`.

```bash
v=0.2.2   # or the latest release (binaries ship from 0.2.2 on)
t=x86_64-unknown-linux-musl
base=https://github.com/Bunty9/ferryman/releases/download/v$v
curl -fsSLO $base/ferryman-v$v-$t.tar.gz -O $base/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS      # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
tar xzf ferryman-v$v-$t.tar.gz
./ferryman-v$v-$t/ferryman --version
```

Each archive also has a standalone `<archive>.sha256` file. Releases from
0.2.3 on also carry build provenance attestations; verify an archive with:

```bash
gh attestation verify ferryman-v$v-$t.tar.gz --repo Bunty9/ferryman
```

Tier-1 binaries embed their dependency list (`cargo auditable`), so
`cargo audit bin ./ferryman` works on them.

```bash
cargo binstall ferryman          # downloads the release archive above (releases after 0.2.1)
cargo install --locked ferryman  # builds from crates.io (Rust 1.88+)
docker build -t ferryman .       # static musl binary in a scratch image
```

On Windows, Ctrl-C drains gracefully; console close / system shutdown
starts the drain, but Windows terminates the process after about 5 s.

## Quick start

```bash
ferryman --config config.toml

# Build and run against the example config (2 upstreams, 5s health interval).
cargo run -p ferryman -- --config config.toml

# Optional: local upstreams that answer 200 on every path.
cargo run --release -p ferryman --example echo_upstream -- 127.0.0.1:8001 &

# In another shell, hit a route:
curl -i http://localhost:8080/svc-a/hello
# HTTP/1.1 502 bad gateway          (no svc-a:8001 listening yet)
# HTTP/1.1 503 upstream unavailable (once its circuit has opened)
curl -i http://localhost:8080/no-such-route
# HTTP/1.1 404 no route

# Optional TLS termination (HTTP/2 via ALPN):
cargo run -p ferryman -- --config config.toml \
    --tls-cert cert.pem --tls-key key.pem

# Scrape Prometheus metrics:
curl -s http://localhost:9090/metrics | head
```

Edit `config.toml` while ferryman is running — the routing table reloads
atomically without dropping live connections.

## Examples

Reference examples live under [`examples/`](./examples) (index:
[`examples/README.md`](./examples/README.md)):

| Example | What it shows |
| --- | --- |
| [`examples/full-stack/`](./examples/full-stack) | Full Docker Compose deployment: ferryman, TLS, three demo upstreams, Prometheus + Grafana, an asserted `demo.sh` tour. |
| [`examples/embedded/`](./examples/embedded) | Embedding `ferryman::serve` as a library inside your own async Rust app, with its own admin server. |
| [`crates/core/examples/guarded_client.rs`](./crates/core/examples/guarded_client.rs) | `ferryman-core`'s circuit breaker guarding any fallible async call, no proxy involved. |

## Configuration

See [`config.toml`](./config.toml). Unknown keys are rejected, so typos
fail loudly instead of silently falling back to defaults.

| Key                              | Default | Meaning                                                                          |
| -------------------------------- | ------- | -------------------------------------------------------------------------------- |
| `health_interval_secs`           | 5       | Active `/health` probe interval (restart to change).                             |
| `default_cooldown_secs`          | 30      | How long an open circuit refuses traffic before one probe.                       |
| `failure_threshold`              | 3       | Consecutive failures that open a closed circuit.                                 |
| `upstream_timeout_secs`          | 30      | Time allowed for an upstream to send response headers.                           |
| `keepalive_timeout_secs`         | 10      | HTTP/1 keep-alive idle timeout, 1-86400 (ALB: 75, GCLB: 620).                    |
| `request_body_idle_timeout_secs` | 30      | Longest gap between request-body frames, 1-86400.                                |
| `request_body_timeout_secs`      | 300     | Total time to receive a request body, 1-86400 (408 when exceeded).               |
| `trusted_proxies`                | `[]`    | CIDRs/IPs whose forwarding headers are trusted (see below); v4 clients match only v4 ranges. Hot-reloads (applied per request). |
| `[[routes]] prefix`              | —       | Path prefix, matched on segment boundaries (`/a` ≠ `/ab`).                       |
| `[[routes]] upstream`            | —       | `http://host:port` — no path, no query, no https.                                |
| `[[routes]] cooldown_secs`       | default | Per-route cooldown override.                                                     |

All `*_secs` values must be between 1 and 86400 (one day). An upstream
with an empty host (`http://:80`) is rejected.

Routes pointing at the same `host:port` share one circuit breaker. A hot
reload keeps each surviving upstream's breaker, so an open circuit stays
open across a config edit. An invalid config on reload is logged and the
old table stays live.

CLI flags (env var in brackets): `--config` (`FERRYMAN_CONFIG`), `--bind`
(`FERRYMAN_BIND`, `0.0.0.0:8080`), `--metrics-bind`
(`FERRYMAN_METRICS_BIND`, `0.0.0.0:9090`), `--tls-cert` / `--tls-key`
(`FERRYMAN_TLS_CERT` / `FERRYMAN_TLS_KEY`).

## Running behind a load balancer

An LB that reuses backend connections needs ferryman to keep them open
longer than the LB does, or it may send a request just as ferryman closes
the socket and return 502. Set `keepalive_timeout_secs` accordingly:

| Load balancer | Setting |
| ------------- | ------- |
| AWS ALB       | ALB idle timeout + 15 s: `75` at the default 60 s. (Or set the ALB idle timeout to 9 s or less and keep the default 10.) |
| Google Cloud LB | `620` (GCLB holds backend connections 600 s). |
| Direct clients | keep the default `10`. |

The first request on a new connection is always bounded at 10 s. A larger
value also widens the header-read window on reused connections: a client
can send one request and then hold the connection for up to this value, so
raise it only behind a load balancer. Changes apply to new connections on
hot reload.

## Behaviour

| Situation                                              | Response | Counts against breaker |
| ------------------------------------------------------ | -------- | ---------------------- |
| No route matches                                       | 404      | —                      |
| Circuit open                                           | 503      | —                      |
| `Upgrade` / `CONNECT` (e.g. WebSocket)                 | 501      | —                      |
| Connect / transport error                              | 502      | yes                    |
| No response headers within `upstream_timeout_secs` of the request body completing (of the request start if bodyless) | 504 | yes |
| Client's request body fails mid-upload                 | 400      | no                     |
| Client stalls its upload for `request_body_idle_timeout_secs`, or exceeds `request_body_timeout_secs` in total | 408 | no |
| Upstream answers 502/503/504                           | passed through | yes              |
| Anything else from upstream                            | passed through | success          |

Request and response bodies are streamed, never buffered. Hop-by-hop
headers are stripped both ways; forwarding headers are set as described
under "Forwarded headers" below; the client's `Host` is kept (HTTP/2
`:authority` becomes `Host`).
Upstreams always get HTTP/1.1. The TLS handshake and the first request on a
connection must complete within 10s; later HTTP/1 request heads are bounded
by `keepalive_timeout_secs` (default 10). SIGINT/SIGTERM stop accepting and drain
in-flight connections for up to 25s.

### Forwarded headers and `trusted_proxies`

What the upstream sees depends on whether the connecting peer is in
`trusted_proxies` (an empty list means every peer is untrusted):

| Header              | Untrusted peer                       | Trusted peer                                                                                   |
| ------------------- | ------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `X-Forwarded-Proto` | set from the connection              | incoming (first value) kept; set from the connection if absent                                 |
| `X-Forwarded-For`   | peer IP appended                     | peer IP appended                                                                               |
| `X-Real-IP`         | overwritten with the peer IP         | always overwritten: the rightmost `X-Forwarded-For` entry that is not a trusted proxy (`ip:port` / `[v6]:port` tolerated; an unparsable entry stops the walk), else the peer |
| `Forwarded`         | stripped                             | kept                                                                                           |
| `X-Forwarded-Host`  | stripped                             | kept                                                                                           |

ferryman does not add `X-Forwarded-Host` itself; the `Host` header is
unchanged. Before 0.2.3 clients could forge `X-Real-IP`, `Forwarded` and
`X-Forwarded-Host`; if ferryman sits behind nginx or a load balancer, add that
hop's address range to `trusted_proxies`, otherwise its values are replaced or
stripped.

**Only trust a hop that overwrites or strips `Forwarded` and
`X-Forwarded-Host`.** AWS ALB and most cloud load balancers pass
client-supplied values of these through, so downstream apps should not trust
those headers behind such a load balancer. (`X-Real-IP` is safe either way:
ferryman derives it itself.)

**Vaultwarden:** upgrade to 0.2.3. ferryman then sets `X-Real-IP` from the TCP
peer (or, behind a trusted proxy, from `X-Forwarded-For`), so Vaultwarden's
default `IP_HEADER=X-Real-IP` is correct. On 0.2.2 the only non-spoofable
setting is `IP_HEADER=none`, which makes all clients share ferryman's IP for
rate limiting.

## Metrics endpoints

The Prometheus exporter binds a separate listener (default `:9090`).
Surface:

| Metric                              | Labels                                          | Description                                                  |
| ----------------------------------- | ----------------------------------------------- | ------------------------------------------------------------ |
| `ferryman_requests_total`           | `route`, `upstream`, `status` (`"none"` on 404) | Counter of inbound requests.                                 |
| `ferryman_request_duration_seconds` | `route`, `upstream`                             | Histogram (buckets `le`), time from request start (includes the upload) to upstream response headers. |
| `ferryman_upstream_alive`           | `upstream`                                      | Gauge: 1 = circuit closed, 0 otherwise.                      |
| `ferryman_circuit_state`            | `upstream`                                      | Gauge: 0 closed / 1 open / 2 half-open.                      |

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

- Upstreams are plain HTTP only; `https://` upstreams are rejected.
- No WebSocket / `Upgrade` passthrough (501).
- No per-upstream load balancing: one route, one upstream.
- Kubernetes ConfigMap mounts update via a `..data` symlink swap that
  the file watcher does not see; restart the pod after a ConfigMap edit.
- The response body has no idle timeout once headers have arrived.

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
