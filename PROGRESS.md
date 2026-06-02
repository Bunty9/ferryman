# PROGRESS — ferryman

> Per-sprint tracker. Template adapted from `project-plan.md` § 7,
> customised for P2 (ferryman) bench targets and the Phase B sequencing in
> `backend-cloud-roadmap.md` § 2 (weeks 11–16).

## Sprint — Phase 1 scaffold

- [x] Workspace `Cargo.toml` with `crates/core` + `crates/server` and
      pinned stack deps
- [x] `crates/core` — `Upstream`, `RouteTable`, `SharedTable`, circuit
      breaker primitives
- [x] `crates/core` — `health_loop` active probe with reqwest
- [x] `crates/core` — `ConfigToml`, `RouteToml`, `build_table`
- [x] `crates/server` — hyper-util binary, axum-free service-fn handler
- [x] `crates/server` — `proxy::handle` with 404 / 502 / forward + metrics
- [x] `crates/server` — `reload::watch_config` (notify + arc-swap)
- [x] `config.toml` — two upstreams, 5 s interval, 30 s cooldown
- [x] `Dockerfile` — cargo-chef + musl + scratch final
- [x] `fly.toml` — 2-region (sin + iad)
- [x] `.github/workflows/ci.yml` — fmt + clippy + nextest + deny + bench +
      wrk2 smoke
- [x] `benches/wrk2.lua` — throughput target script
- [x] `scripts/wrk2-smoke.sh` — placeholder smoke wrapper
- [x] `deny.toml`, `rust-toolchain.toml`, `.gitignore`
- [x] `README.md`, design spec, phase plan
- [ ] `cargo check --workspace` passes locally (verified at end of scaffold)
- [ ] `cargo run` binds `:8080` and `:9090`, serves a 404 for unmapped paths

## Next sprint — Phase 2: real upstream forwarding + full circuit breaker

- [ ] Wire per-prefix `route` label into `ferryman_requests_total` and
      `ferryman_request_duration_seconds`
- [ ] Explicit circuit-breaker state machine (`closed` / `open` /
      `half_open`) + `ferryman_circuit_state` gauge
- [ ] Compose fixture (`docker-compose.bench.yml`) with two upstream stubs
      so `scripts/wrk2-smoke.sh` runs the real wrk2 invocation in CI
- [ ] First production-scale `wrk2` 50k rps run against the compose fixture
- [ ] Criterion bench harness in `crates/core/benches/` for `RouteTable::lookup`
- [ ] TLS termination via rustls 0.23 (lays groundwork for P4)
- [ ] Fly.io 2-region deploy + failover demo screencast

## Done

(none yet — scaffold landing is the first commit)

## Blocked

- (none)

## Bench numbers (targets per `projects-l3-l4.md` § P2; updated weekly)

| metric                                                            | target          | current | as-of      |
|-------------------------------------------------------------------|-----------------|---------|------------|
| Throughput (`wrk2 -c 1000 -t 16 -R 50000 -d 60s`)                 | 50,000 rps      |         |            |
| p50 latency                                                       | < 1 ms          |         |            |
| p99 latency                                                       | < 5 ms          |         |            |
| p999 latency                                                      | < 20 ms         |         |            |
| RSS at 50k rps idle                                               | < 20 MB         |         |            |
| Failover recovery after upstream `kill`                           | < 5 s           |         |            |
| CPU vs NGINX, same hardware                                       | >= 50% reduction|         |            |

## Blog topics surfacing

- (none yet)
