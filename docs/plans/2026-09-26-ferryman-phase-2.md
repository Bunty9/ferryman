---
title: ferryman Phase 2 — Real forwarding + full circuit breaker
status: done
date: 2026-09-26
related:
    - ./2026-05-28-ferryman-phase-1-scaffold.md
    - ../specs/2026-05-28-ferryman-design.md
    - ../architecture.md
---

# ferryman Phase 2 — Real forwarding + full circuit breaker

> **Goal:** turn the Phase 1 scaffold into a proxy that is correct under
> failure and hostile clients: explicit breaker state machine, per-route
> metrics, streaming, TLS, graceful shutdown, a real bench fixture, and
> tests. Internals are described in [`../architecture.md`](../architecture.md).

## Delivered

- [x] Lock-free closed / open / half-open breaker with a single probe per
      cooldown, `Admission` tickets, and `ferryman_circuit_state`.
- [x] Segment-boundary longest-prefix routing; unhealthy upstream = 503.
- [x] Validated config (`deny_unknown_fields`, zero durations, scheme,
      duplicates, conflicting cooldowns); breakers shared across reloads.
- [x] Concurrent health checks; `< 500` = healthy; no redirects.
- [x] Streaming proxy with hop-by-hop stripping, `x-forwarded-*`,
      h2-to-h1 translation (Host, cookies), 502/503/504/501/400 mapping.
- [x] `route` / `upstream` / `status` labels on request metrics.
- [x] rustls TLS termination (ring, ALPN h2 + http/1.1).
- [x] Deadlines for TLS handshake, first request, and header reads.
- [x] SIGINT/SIGTERM graceful drain (25s).
- [x] Debounced, rename-safe config reload.
- [x] `docker-compose.bench.yml` + real `scripts/wrk2-smoke.sh` in CI.
- [x] Criterion bench for `RouteTable::lookup`; `echo_upstream` example.
- [x] 32 unit + 15 end-to-end tests; clippy, fmt, `cargo deny` clean.
- [x] Docker musl build fixed (toolchain file excluded from context).
- [x] CI beta leg really runs beta; `actions/checkout@v5`.

## Deferred

- [ ] 50k rps wrk2 run on dedicated hardware (dev box too loaded).
- [ ] Fly.io 2-region deploy and failover screencast (needs account).
- [ ] WebSocket passthrough, multi-upstream load balancing, response body
      idle timeout — candidates for P4.

## Review log

Two independent reviews ran against the code:

1. After the first rewrite, 22 findings. Critical: no header/handshake
   deadlines (slowloris), and client-caused errors tripping the breaker
   for everyone. High: gauges missing before the exporter was installed,
   reload copying breaker state instead of sharing it, late results
   flipping the breaker, zero-valued durations panicking the health task.
2. After the fixes, 9 findings: `Upgrade: h2c` answered with 501, health
   successes resetting request failure counts, health probes following
   redirects into TLS, the first-byte peek being bypassable with a
   partial h2 preface, absolute-form Host handling, and stale gauges for
   removed upstreams.

All findings were fixed, each with a regression test where one was
practical.

## Verification recipe

```bash
export PATH=$HOME/.cargo/bin:$PATH
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
cargo bench -p ferryman-core --no-run
docker build -t ferryman:dev .        # ~9 MB scratch image
bash scripts/wrk2-smoke.sh            # needs docker + wrk2
```
