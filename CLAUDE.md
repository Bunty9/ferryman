# ferryman — notes for Claude

Small L7 reverse proxy (hyper 1.x). `crates/core` = breaker, routing, config, health loop.
`crates/server` = lib (`serve`, proxy, tls, reload) + thin `main.rs`. Internals: `docs/architecture.md`.

## Commands

- `export PATH=$HOME/.cargo/bin:$PATH` - cargo is not on the default PATH here
- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` - CI runs clippy with `-D warnings`
- `cargo test --workspace` - 32 unit + 15 e2e tests; ~10s because `idle_connection_is_dropped_after_deadline` waits out the real 10s deadline
- `cargo deny check` - must stay clean (CI job); installed at `~/.cargo/bin/cargo-deny`
- `cargo bench -p ferryman-core --no-run` - criterion lookup bench must compile
- `cargo run --release -p ferryman-server --example echo_upstream -- 127.0.0.1:8001` - local upstream stub
- `bash scripts/wrk2-smoke.sh` - compose fixture + wrk2; runs in CI (wrk2 isn't buildable locally: no OpenSSL headers)
- `gh run list -L 3` / `gh run view <id> --log` - CI status and smoke-bench numbers

## Invariants (don't regress)

- Breaker results go through `Admission` tickets: only `Probe` may leave open/half-open; `Normal` results only count while closed.
- Health loop reports as `Probe`; a probe success while closed is a no-op (must not reset request failure counts).
- `build_table` validates everything before touching `prev` breakers; reload reuses the same `Arc<Breaker>` per `host:port`.
- `lookup` ignores health: dead upstream = 503, never fall back to a shorter prefix.
- Client-side body errors (`hyper::Error::is_user`) and timeouts on requests with bodies must not trip the breaker.
- Metric labels are config-bounded (`route` = prefix, `upstream` = host:port); never label with raw paths.
- Gauges: exporter is installed before any gauge write; health loop republishes every tick (removed upstreams expire via `idle_timeout`).

## Dependency gotchas

- rustls/tokio-rustls use `ring` only (default-features off). Default `aws-lc-rs` breaks the musl scratch Docker build.
- reqwest has no TLS features (health probes are plain http); adding `rustls-tls` pulls `webpki-roots` (CDLA license, deny fails).
- metrics-exporter-prometheus: `default-features = false, features = ["http-listener"]` (push-gateway pulls hyper-rustls/aws-lc).
- Don't add `rustls-pemfile` (unmaintained advisory); use `rustls::pki_types::pem::PemObject`.
- `rust-toolchain.toml` is in `.dockerignore` on purpose: it made rustup switch to a toolchain without the musl target.
- CI test job sets `RUSTUP_TOOLCHAIN=${{ matrix.rust }}`; otherwise the toolchain file forces stable on the beta leg.

## Testing patterns

- E2E tests in `crates/server/tests/proxy.rs` run `ferryman_server::serve` on `127.0.0.1:0` with hyper stubs.
- Simulate a dead upstream with `spawn_toggle_stub` (drops connections), never by dropping/rebinding a port (flaky in parallel).
- Breaker unit tests use >=200ms cooldowns; 20ms windows were flaky under parallel test load.
- Raw `TcpStream` writes are used for cases reqwest can't express (partial bodies, absolute-form targets, stalled prefaces).

## Environment

- Dev box is shared and often heavily loaded (load avg 20-40) and port 8001 may be taken: local latency numbers are unreliable; CI smoke numbers are the reference (PROGRESS.md).
- Commits: author `Bunty9 <Bunty9@users.noreply.github.com>`, no AI attribution trailers (global rule).
