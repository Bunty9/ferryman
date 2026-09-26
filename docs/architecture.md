# ferryman architecture

How the proxy works inside, and why it is built this way. The external
contract (config keys, status codes, metrics) is in the
[README](../README.md); the original design brief is in
[`specs/2026-05-28-ferryman-design.md`](specs/2026-05-28-ferryman-design.md).

## Crates

| Crate            | Files                                   | Responsibility                                                   |
| ---------------- | --------------------------------------- | ---------------------------------------------------------------- |
| `ferryman-core`  | `breaker.rs`                            | Lock-free circuit breaker, `Admission` tickets, state gauges     |
|                  | `route.rs`                              | `Upstream`, `Route`, `RouteTable`, `SharedTable`, prefix lookup  |
|                  | `config.rs`                             | TOML schema, `load_config`, validating `build_table`             |
|                  | `health.rs`                             | Concurrent active health checks                                  |
| `ferryman-server`| `lib.rs`                                | `serve`: accept loop, deadlines, TLS, graceful shutdown          |
|                  | `proxy.rs`                              | Per-request handler                                              |
|                  | `tls.rs`                                | rustls acceptor, `MaybeTlsStream`                                |
|                  | `reload.rs`                             | Debounced config file watcher                                    |
|                  | `main.rs`                               | CLI, tracing, metrics exporter, signal handling                  |

`ferryman-server` is a library plus a thin binary so the end-to-end tests
can drive the real `serve` loop on an ephemeral port.

## Request lifecycle

```
accept ── set TCP_NODELAY
  │
  ├─ TLS? handshake (10s deadline)
  │
  ├─ hyper-util auto builder (HTTP/1.1 or HTTP/2)
  │    first request must arrive within 10s, each h1 head within 10s
  │
  └─ proxy::handle
       1. table.load_full()             one Arc for the whole request
       2. lookup(path)                  none -> 404
       3. Upgrade (not h2c) / CONNECT   -> 501
       4. upstream.try_acquire()        None -> 503, else Admission
       5. rewrite request:
            strip hop-by-hop, join h2 cookies, Host from authority,
            URI = upstream scheme+authority + original path+query,
            version = HTTP/1.1, x-forwarded-for / -proto
       6. client.request() under upstream_timeout (to response headers)
            timeout          -> 504 (breaker failure only if bodyless)
            client body err  -> 400 (no breaker effect)
            transport err    -> 502 + breaker failure
            502/503/504      -> passed through + breaker failure
            anything else    -> passed through + breaker success
       7. strip hop-by-hop from the response, stream the body back
```

Bodies are never buffered: the inbound `Incoming` is handed to the hyper
client and the upstream's `Incoming` is returned boxed. Metrics are
recorded at step 6, measuring time to response headers.

## Routing

`RouteTable` holds routes sorted by prefix length, longest first, and
`lookup` returns the first one matching on a path-segment boundary:
`/svc-a` matches `/svc-a`, `/svc-a/`, `/svc-a/x` but not `/svc-ab`; a
prefix ending in `/` is a plain `starts_with`, so `/` is a catch-all. A
linear scan wins over a trie for the tens of routes this targets (see
`crates/core/benches/lookup.rs`).

Lookup deliberately ignores health. If `/api/v1` is down, falling back to
`/api` would send the request to a different service, so the answer is a
503.

## Circuit breaker

One breaker per upstream `host:port` (lowercased), shared by every route
pointing there and kept across hot reloads.

```
             failures >= threshold
   ┌────────┐  (Normal or Probe)   ┌──────┐
   │ Closed │ ───────────────────▶ │ Open │ ◀──────────────┐
   └────────┘                      └──────┘                │
       ▲                              │ cooldown elapsed,  │ Probe failure
       │ Probe success                │ one caller wins    │ (restamps
       │ (request probe or            ▼ the timestamp CAS  │  opened_at)
       │  health check)          ┌──────────┐              │
       └──────────────────────── │ HalfOpen │ ─────────────┘
                                 └──────────┘
                     probe lost for > cooldown: a new probe is admitted
```

- All state is atomics (`state`, `consecutive_failures`, `opened_at` in
  monotonic millis, plus `cooldown` and `failure_threshold` so reloads can
  retune a live breaker). No lock on the hot path.
- The single probe is picked by a CAS on `opened_at`, not on `state`, so
  both the `Open` and stuck-`HalfOpen` paths share one mechanism
  (`claim_probe_slot`). `opened_at` is always stamped *before* `Open` is
  published, so nobody pairs `Open` with a stale timestamp.
- `try_acquire` returns an `Admission` ticket. `Normal` results only
  count while the circuit is closed; they are late news otherwise (a slow
  request admitted before the trip must not close or reopen it). `Probe`
  results are authoritative.
- Health checks report as `Probe`: a healthy answer closes an open circuit
  at once (fast failover recovery), but a healthy answer while closed is a
  no-op, so it can't mask failing real traffic by resetting the count.
- Gauges publish the breaker's *current* state, so racing transitions
  can't leave a stale value.

## Health checks

`health_loop` ticks every `health_interval_secs`, probes every distinct
upstream's `/health` concurrently (`JoinSet`, 2s timeout, no redirects),
waits for all of them, then sleeps. Any answer below 500 counts as up: many
upstreams have no `/health` route, and a 3xx or 404 still proves the
process is serving. The loop also republishes all gauges each tick; the
exporter expires gauges idle for three intervals, which is how upstreams
removed by a reload disappear from `/metrics`.

## Hot reload

`reload::watch_config` watches the config file's parent directory (so
rename-replace saves are seen) and forwards matching events to a thread
that waits for 200ms of quiet before reloading. The reload runs
`load_config` then `build_table(cfg, Some(&current))`:

1. Validate every route and global setting. Any error: log it, keep the
   old table, touch nothing.
2. For each upstream name that already exists, reuse its breaker and
   apply the new cooldown/threshold; otherwise create a new one.
3. Swap the table in with `ArcSwap::store` and publish gauges.

In-flight requests keep the table they loaded; because breakers are
shared, their results still land on the live breaker.

Not reloadable: `health_interval_secs` (ticker is fixed at startup), bind
addresses, TLS files. Kubernetes ConfigMap updates (a `..data` symlink
swap) are not seen by the watcher.

## Connection handling and shutdown

- TLS handshake: 10s. First request on a connection: 10s (covers the
  auto builder's h1/h2 sniff, which has no deadline of its own, and
  clients idling after a handshake). HTTP/1 header read, including idle
  keep-alive: 10s. HTTP/2: keep-alive ping every 30s.
- Accept errors (e.g. `EMFILE`) are logged; the loop sleeps 100ms and
  continues.
- On SIGINT/SIGTERM the listener closes, `GracefulShutdown` asks open
  connections to finish, and the process exits after at most 25s (under
  `fly.toml`'s 30s `kill_timeout`).

## Dependencies and build

- TLS is rustls with the `ring` provider only. The default `aws-lc-rs`
  needs a C toolchain the musl build lacks, and with a single provider
  compiled in `ServerConfig::builder()` needs no `install_default`.
- reqwest (health checks) has no TLS stack; upstreams are http only.
- The Docker build (cargo-chef, `x86_64-unknown-linux-musl`, `FROM
  scratch`) produces a ~9 MB image. `rust-toolchain.toml` is excluded from
  the build context so the image's toolchain, which has the musl target,
  is used.

## Known limits

- One upstream per route (no load balancing), http-only upstreams.
- No WebSocket / `Upgrade` passthrough.
- No idle timeout on a response body once headers have arrived.
- No connection cap beyond the OS file descriptor limit.
