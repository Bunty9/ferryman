# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
crates follow [Semantic Versioning](https://semver.org/). `ferryman-core`
and `ferryman` are released together with the same version.

## [Unreleased]

### Added

- `--version`.
- `ferryman::LATENCY_BUCKETS`.
- Three reference examples: `examples/full-stack` (Docker Compose:
  ferryman + demo upstreams + Prometheus/Grafana, TLS, asserted tour),
  `examples/embedded` (embedding `ferryman::serve` as a library), and
  `crates/core/examples/guarded_client.rs` (`ferryman-core`'s circuit
  breaker guarding any async call). See `examples/README.md`.

### Changed

- Request duration is exported as a histogram with buckets instead of a
  summary. **Breaking for dashboards/alerts**: existing
  `ferryman_request_duration_seconds{quantile=...}` queries no longer
  match and must move to `histogram_quantile(...)` over the `_bucket`
  series.

## [0.1.0] - 2026-09-28

First public release.

### ferryman-core

- Lock-free closed / open / half-open circuit breaker with a single
  half-open probe per cooldown and `Admission` tickets that keep late
  results from flipping the state.
- `RouteTable` with longest-prefix matching on path-segment boundaries.
- Strictly validated TOML config (`ConfigToml`, `RouteToml`); rebuilt
  tables share each surviving upstream's breaker across hot reloads.
- Concurrent active health checks (`health_loop`).
- `ferryman_circuit_state` and `ferryman_upstream_alive` gauges.

### ferryman

- Streaming HTTP/1.1 + HTTP/2 reverse proxy with 404 / 501 / 502 / 503 /
  504 mapping, hop-by-hop header stripping and `x-forwarded-*` headers.
- Optional TLS termination (rustls with `ring`, ALPN h2 + http/1.1).
- Deadlines for TLS handshake, first request and header reads.
- Graceful shutdown on SIGINT / SIGTERM.
- Debounced, rename-safe hot reload of the routing config.
- Prometheus metrics on a separate listener.
- `echo_upstream` example for local benchmarking.

[Unreleased]: https://github.com/Bunty9/ferryman/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Bunty9/ferryman/releases/tag/v0.1.0
