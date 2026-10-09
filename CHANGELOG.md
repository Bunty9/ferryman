# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
crates follow [Semantic Versioning](https://semver.org/). `ferryman-core`
and `ferryman` are released together with the same version.

## [Unreleased]

### Security

- Untrusted peers can no longer forge `X-Forwarded-Ssl`, `X-Forwarded-Scheme`,
  `X-Forwarded-Port` or `X-Forwarded-Prefix`: all instances are stripped
  (`X-Forwarded-Port` is then set by ferryman, see Added). Trusted peers keep
  them. Migration: an app behind ferryman that relied on a client-supplied
  `X-Forwarded-Ssl` / `-Scheme` / `-Prefix` arriving from an untrusted hop now
  sees them removed; if a proxy or load balancer in front of ferryman sets
  them, add its range to `trusted_proxies`.

- Host hardening (low severity, defence in depth): when the request target
  has no authority, the `Host` header is now validated before routing and
  must be a single bare `host[:port]` (no userinfo, comma list, path,
  obs-text, empty value, `*`, or a port that is not 1-5 digits <= 65535); an HTTP/1.1
  request with neither `Host` nor an authority is rejected too (HTTP/1.0
  without `Host` is still forwarded); more than one `Host` is rejected even when an
  authority is present. Failures are `400 bad host` (route/upstream `none`,
  no breaker ticket). `host` named in `Connection` is no longer stripped
  from requests or responses. Migration: clients sending malformed, missing (1.1) or
  duplicate `Host` now get 400. h2 `:authority` and absolute-form targets
  still take precedence over `Host`; `local_health_path` is answered before
  this check.

- A client that stalled its upload could open the breaker for everyone: the
  upstream's own body-read timeout produced a 502 (or a forwarded 502-504)
  that was counted as an upstream failure, so three slow uploads yielded 503
  for all clients. When the client left its upload idle for at least
  min(1 s, `request_body_idle_timeout_secs` / 2), such a result now releases
  the breaker ticket instead of counting; the response status is unchanged (`handle_streaming` only; the deprecated
  `handle` is unchanged).

### Added

- Trust model completion: `X-Forwarded-Port` (the listener's port for
  untrusted peers; trusted peers' rightmost valid value is kept, else the
  listener port), optional RFC 7239 `Forwarded` output (`forwarded_header =
  true`, default false; `for=<peer>;proto=;host=`, IPv6 quoted and bracketed),
  `xff = "append" | "replace"` (default append), and a one-time warning when
  `trusted_proxies` is empty and a private/loopback/link-local peer connects.
  New public items: `XffMode`, `RouteTable::{with_forwarded_header,
  forwarded_header, with_xff_mode, xff_mode}`, `proxy::handle_streaming_at`
  (takes the local port; `serve` uses it; `handle_streaming` and the
  deprecated `handle` take the port from `Host`, if any).

- Reload model. The watcher now watches the config file's directory and
  reacts to any event there, so Kubernetes ConfigMap mounts (an atomic
  `..data` symlink swap) are picked up without a restart. Reloads are
  de-duplicated by a content hash: touching a file or rewriting identical
  bytes does not swap the table. SIGHUP (Unix) forces a reload of the config
  and the TLS certificate (`systemctl reload`; `ExecReload=/bin/kill -HUP $MAINPID`);
  with inline `FERRYMAN_CONFIG_TOML` it logs "reload not applicable". Windows
  has no SIGHUP; the file watch is the trigger there.
- TLS certificate hot reload: when `--tls-cert`/`--tls-key` change (watched
  directory, so cert-manager Secret mounts work, or SIGHUP) the pair is
  rebuilt and swapped in for new handshakes; established connections are
  unaffected. A bad pair is logged and the old certificate stays. New public
  API (additive): `tls::load_reloadable`, `tls::TlsReloader`,
  `reload::Reloader`; `tls::load_acceptor`, `reload::watch_config` and
  `serve` are unchanged.

- Shutdown tuning: `drain_timeout_secs` (default 25, the previous constant)
  and `shutdown_delay_secs` (default 0). On SIGTERM/SIGINT `/readyz` flips to
  503 at once, the proxy keeps serving for the delay, then drains for up to
  `drain_timeout_secs`. `serve` keeps its signature and reads the drain value
  from the table (`RouteTable::drain_timeout()` / `shutdown_delay()`).
- PaaS support: bind resolves `--bind` > `FERRYMAN_BIND` > `0.0.0.0:$PORT` >
  `0.0.0.0:8080`; `FERRYMAN_CONFIG_TOML` supplies the config inline (wins over
  `--config`/`FERRYMAN_CONFIG`, disables the file watch; `check` honours it). A non-empty invalid `PORT`
  is a startup error. A second signal during the delay/drain exits with 130.
- Subcommands `ferryman check [--config]` (validate config + TLS files
  without binding; exit 0/1) and `ferryman healthcheck [--url]` (GET the
  admin `/healthz`, exit 0 on 2xx; no curl needed). The Docker image gains a
  `HEALTHCHECK` using it. Running with no subcommand is unchanged.

- Admin server on the metrics bind: `/metrics`, `/healthz` (liveness) and
  `/readyz` (200, then 503 once shutdown begins; not tied to upstream
  health). Public `ferryman::admin::serve_admin`. New top-level config key
  `local_health_path` (default none): the proxy answers `GET`/`HEAD` on that
  path with `200 ok` before routing (no upstream, no breaker, metric
  `route="local_health"`); it shadows route prefixes it falls under and is
  rejected if equal to a prefix. `RouteTable::local_health_path` /
  `with_local_health_path`.

- `ferryman_core::path` module: `bad_path` (moved from ferryman, same
  semantics) and `ambiguous_route(&RouteTable, raw_path)`. Per-route
  `rewrite_host` (default `false`) sends the upstream's authority as `Host`,
  also for HTTP/2 and absolute-form requests. `Route::new` and
  `Route::with_rewrite_host`.

- `ConfigToml::from_table(toml::Table)` and `ConfigToml: FromStr` (shared
  parse path with `load_config`). Core never claims the top-level names
  `mtls`, `jwt`, `limits`, `tls`, `tenant_rps` (reserved for ferryman-edge).

- Per-route `health_path` (default `/health`; absolute, no `?`/`#`) and
  `health_disabled` (default `false`) keys, plus `Upstream::with_health`,
  `health_path()` and `health_disabled()`. Disabled upstreams get no probes
  and no breaker effect; `ferryman_upstream_alive` for them still follows
  the circuit state. Routes sharing a `host:port` must agree
  (`Error::ConflictingHealth`); a bad path is `Error::InvalidHealthPath`.
  Changing them on reload keeps the breaker's state.

- `ferryman_core::Breaker` and `BreakerConfig` are public: a standalone
  circuit breaker (`Breaker::new(config)?`, `try_acquire`, `record_success`,
  `record_failure`, `state`) usable without a routing table. Defaults:
  threshold 3, cooldown 30 s. `Breaker::new` rejects a cooldown under 1 ms
  and a zero threshold. See `examples/embedded` (`guarded.rs`).

- `Breaker::release` / `Upstream::release`: hand back a ticket that ended
  without a verdict on the upstream. A `Probe` re-arms the half-open slot
  immediately, at most once per cooldown (so abandoned requests cannot turn
  into a probe flood); `Normal` is a no-op; state and counts never change.
  `ferryman` calls it on the 408 (stalled/over-long upload), client
  body error (400) and URI-rebuild 502 paths, so a client hanging up while
  holding the half-open probe no longer blocks recovery for a cooldown.

### Changed (breaking)

- SIGHUP no longer terminates the process: it reloads the config and TLS
  certificate (see Added). Migration: anything that used SIGHUP to stop or
  restart ferryman must send SIGTERM (or SIGINT) instead.
- The metrics listener is now ferryman's own admin server, not the
  Prometheus exporter's. The old one answered `OK` on `/health` and metrics
  on every other path and method; now `GET`/`HEAD` `/metrics` is metrics,
  `/healthz` (alias `/health`) is `200 ok`, `/readyz` is readiness, other
  paths 404 and other methods 405. Scrape configs and probes must use
  `/metrics`, `/healthz` (or `/health`) or `/readyz`. The admin server is
  HTTP/1 only. The `http-listener` feature of `metrics-exporter-prometheus`
  is no longer used. `ConfigToml` gains a field (it is `#[non_exhaustive]`; no migration).

- `build_table` rejects route prefixes containing `;`, `\`, `%2F` or `%5C`
  (`Error::NonCanonicalPrefix`): every request to them would be 400.
- `Route` is `#[non_exhaustive]` with a new pub `rewrite_host` field: replace
  struct literals with `Route::new(prefix, upstream)`.
- Paths that are ambiguous between ferryman and an upstream that treats
  `%2F`/`%5C`/`\` as `/` or drops `;params` now get 400 `bad path` when that
  reading would select a different route (`/api%2Fsecret`, `/api;x/x`).
  `;params` are dropped up to the next raw `/` before decoding (Tomcat and
  Spring order) as well. `/app;jsessionid=X` where `/app` is a route now
  gets 400 (it used to be routed to `/`); params after a later segment
  (`/app/x;jsessionid=X`) still pass. Migration: send plain `/` separators; encoded separators inside a segment
  that do not change the route (`group%2Fproject`) still pass.
- Route matching uses a normalised path: `%XX` of unreserved characters
  (`A-Za-z0-9-._~`) is decoded, hex of other escapes is uppercased, and
  repeated `/` is merged. Requests such as `/%61pi/x` or `//api/x` may now
  match a more specific route than before (here `/api` instead of `/`).
  Matching stays case-sensitive and `%2f` stays encoded; the forwarded path
  is unchanged. **Migration:** write route prefixes in normalised form
  (unreserved characters unescaped, uppercase hex, no `//`); non-normal
  prefixes are rejected at load (`Error::NonCanonicalPrefix`).

- `Error::Toml { path }` is now `Option<PathBuf>` (`None` for `from_str` /
  `from_table`). Migration: `path.display()` becomes `path.as_deref()`;
  `Display` is unchanged when the path is set.

- `CircuitState` and `Admission` are now `#[non_exhaustive]`.
  **Migration:** add a `_ =>` arm to any `match` on either enum.
- `ferryman_core::Error` (new, `#[non_exhaustive]`, `std::error::Error +
  Send + Sync + 'static`) replaces `anyhow::Error` in every public
  `ferryman-core` signature: `build_table`, `load_config`,
  `TrustedProxies::parse`, `Breaker::new`, `BreakerConfig::validate`.
  Variants are matchable by category (`InvalidConfig`, `InvalidPrefix`,
  `DuplicatePrefix`, `InvalidUpstream`, `InvalidCooldown`,
  `ConflictingCooldown`, `InvalidCidr`, `InvalidBreakerConfig`,
  `ReadConfig`, `Toml`). `Display` messages are unchanged; `ReadConfig` and
  `Toml` expose the cause via `source()`, so print the chain with
  `anyhow::Error::from(e)` and `{:#}`. `ferryman-core` no longer depends on
  `anyhow` (it now uses `thiserror`). **Migration:** `?` into
  `anyhow::Result` keeps working; code naming `anyhow` types for these
  results, or relying on `{:#}` of the raw error to show the cause, must
  change. `ferryman::serve` still returns `anyhow::Result<()>`.
- `Upstream::new(uri, BreakerConfig) -> Result<Upstream, Error>` replaces
  `Upstream::new(uri, cooldown, failure_threshold) -> Upstream` and now
  rejects a cooldown under 1 ms or a zero threshold
  (`Error::InvalidBreakerConfig`). **Migration:**
  `Upstream::new(uri, BreakerConfig::default().with_cooldown(c).with_failure_threshold(n))?`.
  `BreakerConfig::name` is ignored here (the breaker is named `host:port`).
- `toml::de::Error` and `http::uri::InvalidUri` are exposed as `Error` sources
  (`ReadConfig`/`Toml`/`InvalidUpstream`); a major bump of those crates is a
  semver break for `ferryman-core`.

### Fixed

- Health-driven recovery: a failing health check while the circuit is open
  no longer re-stamps the cooldown. Before, with a cooldown longer than
  `health_interval_secs` (the defaults), a broken `/health` kept the circuit
  open forever even though real requests would succeed. Now the request-path
  half-open probe gets its slot after the cooldown regardless; only a failed
  half-open probe restarts the cooldown. Health successes and the
  success-while-closed no-op are unchanged.

## [0.2.3] - 2026-10-05

### Security

- Requests whose path contains a dot segment now get 400 `bad path` before
  routing and are never forwarded. Previously `/api/../admin` was
  forwarded verbatim and an upstream that normalises it could serve paths
  outside the routed prefix. Rejected: `.`/`..` segments, including
  `%2e`-encoded and `..;` forms, and including after an encoded or literal
  separator (`..%2f`, `a%2f..`, `..%5c`, `a\..\b`); plus `%00`, `%u`
  escapes, and double-encoded dot or slash (`%252e`, `%252f`, `%255c`).
  Encoded slashes inside an otherwise ordinary segment (e.g. GitLab
  `group%2Fproject`) remain allowed. **Behaviour change:** clients relying on
  `..` passthrough now get 400. The forwarded path is never rewritten; the
  query string is not inspected.
  **Migration:** normalise paths client-side before sending.

- Forwarded-header spoofing: a client could send `X-Real-IP`, `Forwarded`,
  `X-Forwarded-Host` (and `X-Forwarded-Proto` where it was not overwritten)
  and have them reach the upstream unchanged; e.g. Vaultwarden trusts
  `X-Real-IP` by default. Now, for every peer not in `trusted_proxies` (all
  peers when the list is empty, the default), `X-Real-IP` is overwritten with
  the peer IP and `Forwarded` / `X-Forwarded-Host` are stripped;
  `X-Forwarded-Proto` is still set from the connection and `X-Forwarded-For`
  still appended. Peers in `trusted_proxies` keep their incoming
  `X-Forwarded-Proto` (rightmost value), `X-Forwarded-Host` and
  `Forwarded`, but `X-Real-IP` is always derived (rightmost `X-Forwarded-For` entry that is not a trusted
  proxy, else the peer) because cloud LBs pass a client's `X-Real-IP`
  through (see README "Forwarded headers").
  **Migration:** if ferryman runs behind nginx or a load balancer that sets
  these headers, add its address range to `trusted_proxies`, otherwise they
  are replaced or stripped. Vaultwarden: upgrade to this release
  (default `IP_HEADER=X-Real-IP` is then correct); on 0.2.2 the only
  non-spoofable setting is `IP_HEADER=none` (all clients share ferryman's IP
  for rate limiting).

### Changed

- `fly.toml`: removed the public port-9090 service; added a `[metrics]`
  section for Fly's managed Prometheus.

### Added

- Circuit breaker state changes are logged with `upstream`, `from` and `to`
  fields (`warn` when opening, `info` otherwise), only on an actual change.
- Config key `request_body_timeout_secs` (default 300, 1..=86400) and
  `RouteTable::request_body_timeout()` / `with_request_body_timeout()`:
  total cap on receiving a request body (streaming path only).
- `ferryman::StreamingClient`, `ferryman::proxy::handle_streaming` and
  `ferryman::proxy::RequestBody` (idle-timeout and end-of-body-signalling
  request body) for embedders.

- `keepalive_timeout_secs` is now applied: it is the HTTP/1 keep-alive idle
  timeout (hyper `header_read_timeout`), read per connection so hot reload
  affects new connections. Default stays 10 s; the first request on a
  connection is still bounded at 10 s. See README "Running behind a load
  balancer" for ALB/GCLB values.

- New optional config keys `keepalive_timeout_secs` (default 10),
  `request_body_idle_timeout_secs` (default 30) and `trusted_proxies`
  (default empty; see Security above for its effect), validated at load and
  exposed on `RouteTable` (`keepalive_timeout()`,
  `request_body_idle_timeout()`, `trusted_proxies()`, `with_*` builders) plus
  the `TrustedProxies` CIDR type. Defaults keep today's behaviour apart from
  the forwarded-header fix under Security.

- Release archives carry GitHub build provenance attestations
  (`gh attestation verify <archive> --repo Bunty9/ferryman`); tier-1
  binaries are built with `cargo auditable`.
- `SECURITY.md` (private vulnerability reporting).

### Fixed

- Long uploads no longer fail with 504: `upstream_timeout_secs` used to cover
  the whole request including streaming the client's body. It now starts when
  the request body is complete (immediately for bodyless requests).
  `request_body_idle_timeout_secs` is now applied: a client that stalls its
  upload longer than that gets 408, with no breaker effect. Uploads are now
  bounded by the new `request_body_timeout_secs` (default 300) instead of
  `upstream_timeout_secs`; exceeding it also gets 408.
  An upstream that stops reading the upload for a whole `upstream_timeout_secs`
  window gets 504 and counts toward the breaker.
  **Behaviour change:** an upstream timeout after the body completed now
  counts as a breaker failure for requests with bodies too (previously only
  bodyless requests counted).
  `ferryman::serve` uses the new streaming path.
  **Migration:** if you raised `upstream_timeout_secs` to work around slow or long uploads, set `request_body_timeout_secs` (and `request_body_idle_timeout_secs`) instead.

- An upstream with an empty host (`http://:80`) is now rejected at load
  instead of producing a permanently failing route. Migration: fix the
  `upstream` value.
- Duration keys (`health_interval_secs`, `upstream_timeout_secs`,
  `default_cooldown_secs`, per-route `cooldown_secs`) are bounded to
  <= 86400; a huge `health_interval_secs` used to panic at boot. Migration:
  lower any value above one day.

### Deprecated

- `ferryman::ProxyClient` and `ferryman::proxy::handle` keep their 0.2.2
  signature and old timeout semantics (whole-request timeout; a timeout on a
  request with a body does not count against the breaker). Use
  `StreamingClient` / `handle_streaming`.

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

[Unreleased]: https://github.com/Bunty9/ferryman/compare/v0.2.3...HEAD
[0.2.3]: https://github.com/Bunty9/ferryman/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Bunty9/ferryman/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Bunty9/ferryman/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Bunty9/ferryman/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Bunty9/ferryman/releases/tag/v0.1.0
