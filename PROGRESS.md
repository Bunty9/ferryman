# PROGRESS — ferryman

> Per-sprint tracker. Template adapted from `project-plan.md` § 7,
> customised for P2 (ferryman) bench targets and the Phase B sequencing in
> `backend-cloud-roadmap.md` § 2 (weeks 11–16).

## Sprint — Phase 1 scaffold (done)

- [x] Workspace `Cargo.toml` with `crates/core` + `crates/server`
- [x] `crates/core` — `Upstream`, `RouteTable`, `SharedTable`, health loop,
      TOML schema
- [x] `crates/server` — hyper-util service, proxy handler, reload watcher
- [x] `config.toml`, `Dockerfile`, `fly.toml`, CI, `benches/wrk2.lua`
- [x] `cargo check --workspace` passes
- [x] `cargo run` binds `:8080` and `:9090`, serves a 404 for unmapped paths

## Sprint — Phase 2: real forwarding + full circuit breaker (done)

- [x] Explicit `closed` / `open` / `half_open` breaker, lock-free, with a
      single half-open probe per cooldown and ticketed admissions so late
      results can't flip it (`crates/core/src/breaker.rs`)
- [x] `ferryman_circuit_state` gauge; `route` + `upstream` labels on
      `ferryman_requests_total` and `ferryman_request_duration_seconds`
- [x] Segment-boundary longest-prefix routing; dead upstream = 503
- [x] Config validation (unknown keys, zero durations, https upstreams,
      duplicate prefixes, conflicting cooldowns); breakers shared across
      hot reloads
- [x] Streaming bodies, hop-by-hop stripping, `x-forwarded-*`, h2 inbound
      to h1 upstream (Host + cookie handling), upstream timeout (504)
- [x] Slowloris protection: TLS handshake, first-request and header-read
      deadlines; h2 keep-alive pings
- [x] Graceful shutdown on SIGINT/SIGTERM (25s drain)
- [x] TLS termination via rustls 0.23 (`ring`), ALPN h2 + http/1.1
- [x] Hot reload survives rename-replace saves; debounced
- [x] Compose fixture (`docker-compose.bench.yml`) + real
      `scripts/wrk2-smoke.sh`, run in CI
- [x] Criterion bench for `RouteTable::lookup` (`crates/core/benches/`)
- [x] 32 core unit tests + 15 end-to-end proxy tests; clippy `-D warnings`,
      `cargo deny` clean; CI green on stable + beta
- [x] `examples/echo_upstream.rs` for local benching
- [ ] First production-scale `wrk2` 50k rps run on dedicated hardware
- [ ] Fly.io 2-region deploy + failover demo screencast (needs a Fly
      account; `fly.toml` and a 9 MB scratch image are ready)

## Next — P4 (ferryman-edge)

- mTLS, JWT auth plane, cert hot-reload, WebSocket/Upgrade passthrough,
  per-route load balancing across several upstreams.

## Blocked

- 50k rps target run: needs quiet, dedicated hardware (the dev box was at
  load average ~40 from unrelated builds; numbers below are from CI).
- Fly.io deploy: needs account credentials.

## Bench numbers (targets per `projects-l3-l4.md` § P2)

| metric                                                            | target           | current                     | as-of      |
|-------------------------------------------------------------------|------------------|-----------------------------|------------|
| Throughput (`wrk2 -c 1000 -t 16 -R 50000 -d 60s`)                 | 50,000 rps       | not yet measured            |            |
| CI smoke: `wrk2 -c 100 -t 4 -R 5000 -d 10s` (compose, GHA runner) | —                | 4,855 rps, 0 errors         | 2026-09-26 |
| p50 latency (CI smoke, incl. docker NAT + http-echo)              | < 1 ms           | 1.73 ms                     | 2026-09-26 |
| p99 latency (CI smoke)                                            | < 5 ms           | 4.16 ms                     | 2026-09-26 |
| p999 latency (CI smoke)                                           | < 20 ms          | 5.88 ms                     | 2026-09-26 |
| RSS idle (release, 2 routes)                                      | < 20 MB          | 7.6 MB                      | 2026-09-26 |
| RSS peak under saturating load (c=128)                            | —                | 24.6 MB                     | 2026-09-26 |
| Failover recovery after upstream `kill` + restart                 | < 5 s            | 2.3 s (next health tick)    | 2026-09-26 |
| Container image (scratch, musl, static)                           | —                | 9.0 MB                      | 2026-09-26 |
| CPU vs NGINX, same hardware                                       | >= 50% reduction | not yet measured            |            |

Reproduce locally:

```bash
cargo build --release -p ferryman-server --bin ferryman-server --example echo_upstream
./target/release/examples/echo_upstream 127.0.0.1:8001 &
./target/release/examples/echo_upstream 127.0.0.1:8002 &
./target/release/ferryman-server --config config.toml &
wrk2 -c 1000 -t 16 -R 50000 -d 60s -s benches/wrk2.lua http://127.0.0.1:8080
```

## Blog topics surfacing

- Why a dead upstream should be a 503, not a fallback to a shorter prefix.
- Ticketed admissions: keeping late results out of a lock-free breaker.
- The slowloris hole in hyper-util's auto builder (version sniff has no
  deadline) and closing it with a first-request timer.
