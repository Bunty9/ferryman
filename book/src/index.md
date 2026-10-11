# ferryman

**The small, readable Rust reverse proxy with a production-grade circuit
breaker, which you can also embed as a crate.**

ferryman is an L7 reverse proxy built on hyper 1.x. It does the boring parts
well (prefix routing, TLS termination, forwarded headers, graceful drain) and
keeps the interesting trade-offs (a lock-free circuit breaker, atomic config
hot reload, longest-prefix routing) in code small enough to read in one
sitting.

## Why

Typical self-hosted setups expose services on direct ports or tunnels with no
L7 in front. Real teams put L7 proxies between clients and origin: routing,
health checks, observability, TLS termination. ferryman is a hyper-based
reverse proxy that does the boring parts well and exposes the interesting
trade-offs in code small enough to read top-to-bottom.

## What you get

- **Prefix routing**: longest match on path-segment boundaries, one upstream
  per route, optional `strip_prefix` and `rewrite_host`.
- **Per-upstream circuit breakers** with `Admission` tickets, plus active
  health checks that close a recovered circuit within one interval.
- **Hot reload** of the TOML config and the TLS certificate (file watch,
  `SIGHUP`, Kubernetes ConfigMap/Secret swaps). A bad config never replaces a
  working one.
- **Streaming** of request and response bodies, HTTP/1.1 and HTTP/2 on the
  listener (ALPN with TLS), HTTP/1.1 to upstreams.
- **Safe forwarded headers**: client-supplied `X-Real-IP`, `Forwarded` and
  `X-Forwarded-*` are replaced or stripped unless the peer is in
  `trusted_proxies`.
- **Operations built in**: admin listener with `/metrics`, `/healthz`,
  `/readyz`; opt-in access log and request ids; `shutdown_delay_secs` and
  `drain_timeout_secs` for load balancers and PaaS kill timeouts.
- **A library too**: `ferryman-core` (breaker, routing table, config) and
  `ferryman` (the serve loop) are published crates.

## What it is not

ferryman is deliberately small. Today it has plain-HTTP upstreams only, one
upstream per route, no WebSocket/`Upgrade` passthrough (answered with 501),
and no built-in ACME. [Comparison & migration](comparison.md) says where
Caddy, Traefik, NGINX, rpxy and Pingora are the better choice.

## Where to start

| I want to... | Read |
| --- | --- |
| Run it in Docker, systemd, Kubernetes, Compose or a PaaS | [Quickstarts](quickstarts.md) |
| Look up a config key | [Configuration reference](configuration.md) |
| Understand what the upstream sees | [Forwarded headers](forwarded-headers.md) |
| Understand when circuits open and close | [Circuit breaker & health checks](circuit-breaker.md) |
| Run it behind a load balancer | [Timeouts & limits](timeouts.md), [Operations](operations.md) |
| Use the breaker or the proxy from Rust | [Embedding the library](embedding.md) |
| Decide whether it fits | [Comparison & migration](comparison.md) |

Internals (request path, breaker, reload) are described in
[`docs/architecture.md`](https://github.com/Bunty9/ferryman/blob/main/docs/architecture.md).
API docs are on [docs.rs/ferryman](https://docs.rs/ferryman) and
[docs.rs/ferryman-core](https://docs.rs/ferryman-core).
