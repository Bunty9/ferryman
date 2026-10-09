# ferryman — notes for Claude

Small L7 reverse proxy (hyper 1.x). `crates/core` = breaker, routing, config, health loop.
`crates/server` = lib (`serve`, proxy, tls, reload) + thin `main.rs`. Internals: `docs/architecture.md`.

## Commands

- `export PATH=$HOME/.cargo/bin:$PATH` - cargo is not on the default PATH here
- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` - CI runs clippy with `-D warnings`
- `cargo test --workspace` - unit + e2e tests across the workspace; ~10s because `idle_connection_is_dropped_after_deadline` waits out the real 10s deadline
- `cargo run -p ferryman -- check --config X` validates config+TLS without binding; `... -- healthcheck [--url]` GETs the admin `/healthz` (Docker HEALTHCHECK, no curl in image)
- `cargo deny check` - must stay clean (CI job); installed at `~/.cargo/bin/cargo-deny`
- `cargo bench -p ferryman-core --no-run` - criterion lookup bench must compile
- `cargo run --release -p ferryman --example echo_upstream -- 127.0.0.1:8001` - local upstream stub
- `bash scripts/wrk2-smoke.sh` - compose fixture + wrk2; runs in CI (wrk2 isn't buildable locally: no OpenSSL headers)
- `bash examples/full-stack/demo.sh --ci` - full-stack reference example; runs in CI (`examples` job)
- `cargo test -p ferryman-embedded-example` - embedded-library reference example's tests
- `cargo publish --workspace --dry-run` - packaging check; real publish is irreversible, follow `docs/plans/2026-09-28-publishing.md`
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` - no intra-doc links to private items
- CI jobs beyond test/deny: `semver` (cargo-semver-checks vs the latest crates.io release, blocking; additive-only public API), `msrv` (`cargo check` on 1.88 via `RUSTUP_TOOLCHAIN`), doctests run in the `test` job (`cargo test --doc`, nextest skips them)
- `gh run list -L 3` / `gh run view <id> --log` - CI status and smoke-bench numbers

## Invariants (don't regress)

- Request path order in `handle_inner`: `path::bad_path` -> `path::ambiguous_route` -> `bad_host` (single bare `host[:port]`, valid port, present on 1.1, when no URI authority; >1 Host always 400) -> `lookup` -> admission. `local_health_path` is answered first. Path checks take the RAW path and run before any breaker ticket; the forwarded path is never rewritten.
- Breaker results go through `Admission` tickets: only `Probe` may leave open/half-open; `Normal` results only count while closed.
- Health loop reports as `Probe`; a probe success while closed is a no-op (must not reset request failure counts). A probe failure while Open is a no-op too (no `opened_at` re-stamp); only HalfOpen -> Open stamps, so a broken health endpoint can't starve the request-path half-open probe.
- `release(Probe)` only re-arms the probe slot while HalfOpen, at most once per cooldown, and never changes state or counts; client-side failures (408, client body error, URI-rebuild 502) `release`, they never `record_*`.
- Routes sharing an upstream must agree on `cooldown_secs`, `health_path`, `health_disabled` (build_table error). Reload with changed health keys reuses the `Arc<Breaker>` (state kept); `health_disabled` upstreams are skipped by the health loop.
- `build_table` validates everything before touching `prev` breakers; reload reuses the same `Arc<Breaker>` per `host:port`.
- `lookup` ignores health: dead upstream = 503, never fall back to a shorter prefix.
- Client-side body errors (`hyper::Error::is_user`) and stalled or over-long uploads (`request_body_idle_timeout_secs` / `request_body_timeout_secs`, 408) never trip the breaker. `upstream_timeout` starts at request-body EOS (immediately if bodyless); a 504 after that is recorded as a breaker failure, as is an upstream that stops reading the upload for a whole `upstream_timeout` window (hyper not polling the body while not waiting on the client).
- A client that stalled its upload for >= theta = min(1s, `request_body_idle_timeout`/2) is never blamed for an upstream 502/503/504/transport error (ticket is `release`d, status unchanged); the 504 timer branch still counts. (handle_streaming only; the deprecated `handle` is unchanged.)
- Metric labels are config-bounded (`route` = prefix, `upstream` = host:port); never label with raw paths.
- Gauges: recorder is installed before any gauge write; health loop republishes every tick (removed upstreams expire via `idle_timeout`).
- `examples/*` crates are workspace members with `publish = false`.
- Examples use only ferryman's public API — no `pub(crate)`/internal access.
- Example demo timings (health interval, cooldown, timeouts) are shortened for a fast tour; each is commented with its production value.

## Dependency gotchas

- rustls/tokio-rustls use `ring` only (default-features off). Default `aws-lc-rs` breaks the musl scratch Docker build.
- reqwest has no TLS features (health probes are plain http); adding `rustls-tls` pulls `webpki-roots` (CDLA license, deny fails).
- metrics-exporter-prometheus: `default-features = false` (no features; `http-listener` is unused since the admin server is ours, push-gateway pulls hyper-rustls/aws-lc).
- Don't add `rustls-pemfile` (unmaintained advisory); use `rustls::pki_types::pem::PemObject`.
- `rust-toolchain.toml` is in `.dockerignore` on purpose: it made rustup switch to a toolchain without the musl target.
- CI test job sets `RUSTUP_TOOLCHAIN=${{ matrix.rust }}`; otherwise the toolchain file forces stable on the beta leg.

## Publishing

- Version lives in `[workspace.package]` AND the `ferryman-core` entry of `[workspace.dependencies]`; bump both together. On a minor/major bump also update the `version = "0.x"` reqs on both crates in `examples/embedded/Cargo.toml`, and commit the refreshed `Cargo.lock` (`release.yml` builds `--locked`).
- `crates/*/LICENSE-*` are symlinks to the root files; keep them (they ship the license texts in each `.crate`).
- `ConfigToml`/`RouteToml` are `#[non_exhaustive]`: construct via TOML parsing outside the core crate.
- Release binaries: `binaries` job in `release.yml` (`binaries` tier 1 gates attest and publish; `binaries-extra` tier 2 best effort, not attested); naming `ferryman-v<ver>-<target>.tar.gz|zip` is mirrored in `[package.metadata.binstall]`. Tier 1 builds with `cargo auditable` (tier 2 is built without it). `attest` job (only one with `id-token`/`attestations: write`; no checkout, no cargo; tier-1 archives only; skipped on dry runs) runs before `publish`. Dry run: run the workflow via `workflow_dispatch` (builds only, no publish). Actions are SHA-pinned; zizmor runs in CI.
- `.github/workflows/release.yml` publishes on a `v[0-9]+.[0-9]+.[0-9]+` or prerelease `v…-*` tag push (Trusted Publishing; jobs verify -> binaries (tier 1) -> attest -> publish -> release, plus best-effort binaries-extra; idempotent per-crate publish); see `docs/plans/2026-09-28-publishing.md` for the one remaining manual step, the release procedure, and half-published-release recovery.

## Testing patterns

- E2E tests in `crates/server/tests/proxy.rs` run `ferryman::serve` on `127.0.0.1:0` with hyper stubs.
- Simulate a dead upstream with `spawn_toggle_stub` (drops connections), never by dropping/rebinding a port (flaky in parallel).
- Breaker unit tests use >=200ms cooldowns; 20ms windows were flaky under parallel test load.
- Raw `TcpStream` writes are used for cases reqwest can't express (partial bodies, absolute-form targets, stalled prefaces).

## Environment

- Dev box is shared and often heavily loaded (load avg 20-40) and port 8001 may be taken: local latency numbers are unreliable; CI smoke numbers are the reference (PROGRESS.md).
- Commits: author `Bunty9 <Bunty9@users.noreply.github.com>`, no AI attribution trailers (global rule).
