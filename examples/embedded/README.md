# ferryman-embedded-example

A reference example: **embedding ferryman as a library** inside your own
async Rust application, instead of running the standalone `ferryman` binary
as a separate process.

## What this demonstrates

- Booting ferryman's proxy (`ferryman::serve`), its active health checker
  (`ferryman_core::health_loop`), and its hot-reload watcher
  (`ferryman::reload::watch_config`) as tasks inside your own process,
  behind a small `Settings` / `start` / `Running::shutdown` API
  (`src/lib.rs`).
- Sharing one metrics recorder between ferryman's own metrics and your
  app's, and rendering `/metrics` yourself instead of letting ferryman open
  a second listener for it.
- A tiny hand-rolled admin HTTP server (`src/admin.rs`) exposing
  `GET /healthz` (liveness), `GET /status` (JSON breaker state per
  upstream), and `GET /metrics` (Prometheus exposition format) — routes
  ferryman's own binary doesn't offer as a single family.
- Cooperative shutdown with a `tokio::sync::watch` flag, so the proxy
  listener, the admin listener, and the metrics-upkeep task all stop
  together on one signal.

`src/main.rs` is a thin CLI wrapper around exactly that API (parse args,
install the metrics recorder, `start`, wait for Ctrl-C/SIGTERM, `shutdown`),
so the crate is both a library you can copy from and something you can
literally run.

## Running it

Start two upstream stubs (the same `echo_upstream` example the rest of this
repo uses for local testing):

```bash
cargo run --release -p ferryman --example echo_upstream -- 127.0.0.1:8001
cargo run --release -p ferryman --example echo_upstream -- 127.0.0.1:8002
```

Then, in another terminal, run this example against `examples/embedded/config.toml`
(routes `/svc-a` -> `:8001`, `/svc-b` -> `:8002`):

```bash
cargo run -p ferryman-embedded-example -- --config examples/embedded/config.toml
```

It listens on `0.0.0.0:8080` (proxy) and `0.0.0.0:9091` (admin) by default —
override with `--bind` / `--admin-bind`. If those ports are taken on your
box, pick free ones and edit `config.toml`'s upstreams to match wherever you
started the two `echo_upstream` instances.

While it's running:

```bash
curl http://127.0.0.1:9091/healthz     # -> 200 ok
curl http://127.0.0.1:9091/status      # -> {"upstreams":[{"name":"127.0.0.1:8001",...,"state":"closed"},...]}
curl http://127.0.0.1:9091/metrics     # -> Prometheus exposition text
curl http://127.0.0.1:8080/svc-a/hi    # -> proxied through to the :8001 stub
```

`Ctrl-C` (or `SIGTERM`) triggers the same graceful shutdown path
`Running::shutdown` uses: stop accepting, drain in-flight connections, then
exit.

## Copying this into your own project

1. Copy `src/lib.rs` and `src/admin.rs` (or just the parts you need) into
   your crate.
2. In your `Cargo.toml`, add the two ferryman dependencies **with the
   `path` key deleted** — this example's `Cargo.toml` keeps `path` only so
   this repo's workspace builds against the local checkout; a real
   consumer depends on the published crates:

   ```toml
   ferryman = "0.1"
   ferryman-core = "0.1"
   ```

3. From your own `main` (or wherever your app starts its background work),
   call `start(settings, metrics)` once and hold on to the returned
   `Running` for as long as you want the proxy up; call
   `running.shutdown().await` as part of your own shutdown sequence.

## The "install the recorder once" rule

`metrics_exporter_prometheus::PrometheusBuilder::install_recorder()` sets
the **process-global** metrics recorder (via `metrics::set_global_recorder`).
Call it **exactly once per process**, before `start` (or anything else)
records a metric — a second call fails, because the global recorder is
already set. That's why `start` takes an already-built `PrometheusHandle`
rather than building its own recorder: an app embedding ferryman is
expected to own that one global install, typically alongside whatever else
it records metrics for, and hand the resulting handle to `start`.

Tests are the one place this rule doesn't apply: `tests/embedded.rs` builds
a private, *uninstalled* recorder per test with
`PrometheusBuilder::new().build_recorder().handle()`, so many
`#[tokio::test]`s in the same process never collide over the single global
slot.

## The "run upkeep periodically" rule

The Prometheus recorder buffers histogram/summary state internally and
needs periodic "upkeep" — decaying old data, dropping metrics past their
configured `idle_timeout` — or it grows unboundedly. `PrometheusBuilder::install`
(what `ferryman`'s own binary uses) spawns this for you, bundled with its
own HTTP-listener exporter task. `install_recorder` (what this example uses,
since the admin server renders `/metrics` itself instead of ferryman opening
its own listener) does **not** spawn anything — the caller is responsible
for calling `PrometheusHandle::run_upkeep()` periodically. `start` does
this once, every 5 seconds, for as long as the instance is running; a real
embedding app following this pattern for its *own* recorder needs the same
loop.

## Embed vs. run the binary

Run the standalone `ferryman` binary as its own process when:

- You want ferryman patched/restarted independently of your app.
- Your app is in another language, or you'd rather not couple its release
  cadence to ferryman's.
- One shared metrics/health surface per process isn't something you need —
  a separate process with its own `/metrics` port is simpler to reason
  about operationally (that's what `crates/server/src/main.rs` does).

Embed ferryman as a library (this pattern) when:

- Your app and the proxy should live and die together — one process to
  deploy, one process to signal, one crash domain.
- You want ferryman's metrics folded into an admin/observability surface
  your app already runs, instead of a second `/metrics` port.
- You want programmatic access to the live routing table (`Running::table`)
  from your own code — e.g. to drive an admin UI, or to build the initial
  config from something other than a TOML file on disk.
- The extra wiring in `src/lib.rs` (spawning the health loop, the watcher,
  the two listeners, and tying them to one shutdown signal) is something
  you're happy to own and keep working as ferryman's API evolves, in
  exchange for that control.
