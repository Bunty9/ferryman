---
title: ferryman Phase 1 — Scaffold + Compile
status: draft
date: 2026-05-28
related:
    - ../specs/2026-05-28-ferryman-design.md
    - ../../../projects-l3-l4.md
    - ../../../backend-cloud-roadmap.md
---

# ferryman Phase 1 — Scaffold + Compile

> **Goal:** lay down the workspace, the routing/health/reload primitives,
> the container story, and CI so that `cargo check --workspace` is green
> and `cargo run -p ferryman-server -- --config config.toml` binds `:8080`
> and serves a `404 no route` for any unmapped path. No real upstream
> traffic in Phase 1; both example upstreams (`svc-a`, `svc-b`) are
> expected to be down and tripping their circuit breakers.

**Spec source:** [`../specs/2026-05-28-ferryman-design.md`](../specs/2026-05-28-ferryman-design.md).

## File inventory (checklist)

- [x] `Cargo.toml` — workspace root with `crates/core` + `crates/server`
      members and workspace deps pinned from `backend-cloud-roadmap.md` § 3.
- [x] `rust-toolchain.toml` — `channel = "stable"` + clippy + rustfmt.
- [x] `deny.toml` — minimal `cargo-deny` config (advisories deny, license
      allowlist for MIT/Apache/BSD/ISC/MPL/Unicode/CC0).
- [x] `.gitignore` — Rust + `.env` + `target/` + `dist/` + `.venv/`.
- [x] `config.toml` — two example routes (`/svc-a`, `/svc-b`), 5 s health
      interval, 30 s default cooldown.
- [x] `crates/core/Cargo.toml` + `crates/core/src/lib.rs` — re-exports
      `route`, `health`, `config`.
- [x] `crates/core/src/route.rs` — `Upstream`, `RouteTable`, `SharedTable`,
      `is_routable` / `mark_failed`.
- [x] `crates/core/src/health.rs` — `health_loop`.
- [x] `crates/core/src/config.rs` — `ConfigToml`, `RouteToml`, `build_table`.
- [x] `crates/server/Cargo.toml` + `crates/server/src/main.rs` —
      hyper-util `Builder::serve_connection`, Prometheus exporter, spawns
      `health_loop`, owns the FS watcher returned by `watch_config`.
- [x] `crates/server/src/proxy.rs` — `handle` (404 / 502 / forward).
- [x] `crates/server/src/reload.rs` — `watch_config` (notify + arc-swap).
- [x] `Dockerfile` — cargo-chef multi-stage, target
      `x86_64-unknown-linux-musl`, `FROM scratch` final.
- [x] `fly.toml` — 2-region (sin + iad), single machine each.
- [x] `.github/workflows/ci.yml` — matrix stable + beta; `cargo fmt
      --check`, `cargo clippy -- -D warnings`, `cargo nextest`, `cargo deny
      check`, `cargo bench --no-run` (non-blocking), `wrk2` smoke
      (non-blocking).
- [x] `benches/wrk2.lua` — wrk2 script for the throughput target.
- [x] `scripts/wrk2-smoke.sh` — placeholder smoke wrapper (echoes the
      planned invocation; Phase 2 wires a real compose fixture).
- [x] `README.md` — problem, ASCII architecture, stack table, quick-start,
      metrics surface, bench targets, license.
- [x] `docs/specs/2026-05-28-ferryman-design.md` — full P2 design spec.
- [x] `docs/plans/2026-05-28-ferryman-phase-1-scaffold.md` — this plan.
- [x] `PROGRESS.md` — per-sprint tracker, P2 bench targets recorded.

## Exit criteria

1. **`cargo check --workspace` passes** from the project root.
2. **`cargo run -p ferryman-server -- --config config.toml`** binds `:8080`
   (proxy) and `:9090` (Prometheus exporter) without panicking.
3. **`curl -i http://localhost:8080/no-such-route`** returns `HTTP/1.1 404`
   with body `no route`.
4. **`curl -i http://localhost:8080/svc-a/anything`** returns `HTTP/1.1
   502` while the dead upstream's circuit is still closed, then `503
   upstream unavailable` once `failure_threshold` failures (requests or
   health probes) have opened it. *(Superseded Phase 1 behaviour: lookup
   used to skip dead upstreams and answer 404.)*

## Out of scope (deferred to later phases)

- Real upstream forwarding under load + the `wrk2` 50k rps run.
- Per-route metrics with `route` label (currently we only emit
  `route="none"` on 404; Phase 2 plumbs the matched prefix into labels).
- Full circuit-breaker state machine with explicit `closed/open/half-open`
  states and the `ferryman_circuit_state` gauge.
- TLS termination via rustls (Phase 2 / P4).
- mTLS + JWT validation + hot-reload of cert chain (P4 = ferryman-edge).
- `cargo bench` real Criterion harness (CI step exists, no targets yet).
- Production `wrk2` compose fixture wired into CI (Phase 2; placeholder
  script is in place).

## Verification recipe

```bash
cd ferryman
cargo check --workspace                                # exit-criterion 1
cargo run -p ferryman-server -- --config config.toml & # exit-criterion 2
sleep 1
curl -sS -i http://localhost:8080/no-such-route        # exit-criterion 3
curl -sS -i http://localhost:8080/svc-a/anything       # exit-criterion 4
kill %1
```
