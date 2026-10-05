# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
crates follow [Semantic Versioning](https://semver.org/). `ferryman-core`
and `ferryman` are released together with the same version.

## [Unreleased]

## [0.2.2] - 2026-10-05

Prebuilt binaries. No API or proxy-behaviour changes on Unix.

### Added

- Prebuilt binaries attached to GitHub releases (`.tar.gz`, `.zip` on
  Windows) with a combined `SHA256SUMS` and per-archive `.sha256` files.
  Required: Linux musl x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64
  (MSVC). Best effort: ARM/i686 musl, riscv64 gnu, FreeBSD, Windows aarch64.
- `cargo binstall ferryman` metadata (`[package.metadata.binstall]`), used from 0.2.2 on.
- Windows support: Ctrl-C drains gracefully; console close / system
  shutdown starts the drain, but Windows terminates the process after
  about 5 s (previously Unix-only signal handling).
- `release.yml` accepts prerelease tags (`vX.Y.Z-rc.1`, marked as
  prerelease) and a manual `workflow_dispatch` dry run of the binary
  builds.
- CI builds `ferryman` on Windows and macOS, and lints workflows with
  zizmor.

### Changed

- GitHub Actions are pinned by commit SHA.

## [0.2.1] - 2026-09-29

No API or behaviour changes to the `ferryman` binary or either library.

### Added

- `.github/workflows/release.yml`: pushing a `vX.Y.Z` tag checks the tag
  (on `main`, matches the workspace version, has a CHANGELOG section),
  runs the tests, publishes both crates via crates.io Trusted Publishing
  (per crate, skipping versions already published), then creates the
  GitHub release from this file.

### Fixed

- `ferryman-core`'s `guarded_client` example: the timeout section now
  proves a hang is counted as a failure (the breaker opens).
- `examples/embedded`: shutdown is bounded by one 30 s deadline in total
  (was up to ~90 s); timeout errors name the bound.
- `examples/full-stack`: the ferryman container no longer mounts the CA
  private key; `gen-certs.sh` enforces `0600` on it, recovers from partial
  runs, and fails clearly if Docker created directories in place of cert
  files. README step 7 now correctly explains that health checks close an
  open circuit directly, without waiting for the cooldown.

### Changed

- CI runs the test suite with `--locked`.

## [0.2.0] - 2026-09-29

### Added

- `--version`.
- `ferryman::LATENCY_BUCKETS` (0.5 ms to 30 s).
- Native arm64 Docker builds: the Dockerfile picks the musl target from
  `TARGETARCH`, so Apple Silicon hosts build without emulation.
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

[Unreleased]: https://github.com/Bunty9/ferryman/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/Bunty9/ferryman/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Bunty9/ferryman/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Bunty9/ferryman/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Bunty9/ferryman/releases/tag/v0.1.0
