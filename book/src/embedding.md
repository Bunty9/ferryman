# Embedding the library

ferryman is two published crates:

| Crate | What it is |
| --- | --- |
| [`ferryman-core`](https://docs.rs/ferryman-core) | `Breaker`, `BreakerConfig`, `Admission`, `RouteTable`, `SharedTable`, TOML schema (`ConfigToml`, `build_table`), `health_loop`, `TrustedProxies`. No HTTP server. |
| [`ferryman`](https://docs.rs/ferryman) | The serve loop (`serve`), the request handler (`proxy::handle_streaming`), `tls`, and optionally `admin` and `reload`, plus the binary. |

Both are released together with the same version. `ConfigToml` and
`RouteToml` are `#[non_exhaustive]`: outside the core crate construct them by
parsing TOML.

## Cargo features of `ferryman`

Embedding as a library? Turn off the default features so the CLI's
dependencies (`clap`, `tracing-subscriber`, ...) are not pulled in:

```toml
ferryman = { version = "0.3", default-features = false, features = ["reload"] }
```

| Feature | Default | Adds |
| --- | --- | --- |
| `bin` | yes | the `ferryman` binary (`clap`, `tracing-subscriber`, `metrics-util`); implies `admin` and `reload` |
| `admin` | via `bin` | `ferryman::admin` (admin server; its API takes a `metrics-exporter-prometheus` `PrometheusHandle`) |
| `reload` | via `bin` | `ferryman::reload` (config/TLS file watcher, uses `notify`) |

Without features you still get `serve`, `proxy` and `tls`. `cargo install`,
`cargo binstall`, the release archives and the Docker image keep the defaults.

## The proxy inside your process

```rust,ignore
use ferryman_core::{build_table, load_config};
use std::sync::Arc;
use arc_swap::ArcSwap;

let cfg = load_config("ferryman.toml".as_ref())?;
let table = Arc::new(ArcSwap::from_pointee(build_table(cfg, None)?)); // SharedTable
let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;

tokio::spawn(ferryman_core::health_loop(table.clone(), std::time::Duration::from_secs(5)));
ferryman::serve(listener, table, None, shutdown_signal()).await?;
```

Notes:

- `serve` stops accepting when the `shutdown` future resolves, then drains for
  the table's `drain_timeout_secs`. It does **not** apply
  `shutdown_delay_secs`: your `shutdown` future should flip your readiness
  signal, sleep `table.load().shutdown_delay()` and then resolve.
- There is no health loop unless you spawn `health_loop`. It has no
  cancellation hook of its own; abort the task after the proxy has drained.
- `ferryman::reload::watch_config` (feature `reload`; keep the returned watcher alive) gives you the same
  directory watcher, content-hash de-dup and failed-reload-keeps-old-table
  behaviour as the binary.
- `proxy::handle_streaming` is the per-request handler, if you want your own
  accept loop; `handle_streaming_at` also takes the listener port for
  `X-Forwarded-Port`. The older `proxy::handle` is deprecated.

[`examples/embedded/`](https://github.com/Bunty9/ferryman/tree/main/examples/embedded)
is a complete, tested version: proxy, health loop and reload watcher as tasks
in your process, a shared metrics recorder, a hand-rolled admin server with a
`/status` JSON endpoint, and a shutdown that stops everything it spawned.

## The breaker on its own

`Breaker` needs no routing table and no HTTP. Wrap any fallible async call
(a database query, a gRPC call, another service's SDK):

```rust,ignore
use ferryman_core::{Breaker, BreakerConfig};
use std::time::Duration;

let breaker = Breaker::new(
    BreakerConfig::default()
        .with_failure_threshold(3)
        .with_cooldown(Duration::from_secs(30))
        .with_name("inventory"),
)?;

let Some(ticket) = breaker.try_acquire() else {
    return Err(Unavailable); // open: fail fast, do not spin-retry
};
match call().await {
    Ok(v) => { breaker.record_success(ticket); Ok(v) }
    Err(e) => { breaker.record_failure(ticket); Err(e) }
}
```

Rules worth knowing ([Circuit breaker](circuit-breaker.md#admission-tickets)):

- Report the outcome on the exact ticket `try_acquire` returned. If the call
  is abandoned for a reason that says nothing about the dependency (the
  caller went away), `release` the ticket instead.
- Put a timeout *inside* the guarded future, not around it. If a timeout
  drops the guarded future after admission, the ticket never reaches the
  breaker, a hung dependency never trips it, and a dropped half-open probe
  leaves the breaker stuck until the next cooldown.
- Defaults: threshold 3, cooldown 30 s.

Runnable versions:
[`crates/core/examples/guarded_client.rs`](https://github.com/Bunty9/ferryman/blob/main/crates/core/examples/guarded_client.rs)
(`cargo run -p ferryman-core --example guarded_client`) and
[`examples/embedded/src/guarded.rs`](https://github.com/Bunty9/ferryman/blob/main/examples/embedded/src/guarded.rs),
a `Breaker` around a `reqwest` call (`cargo test -p ferryman-embedded-example
--test guarded`).

## Examples index

| Example | What it shows |
| --- | --- |
| [`examples/full-stack/`](https://github.com/Bunty9/ferryman/tree/main/examples/full-stack) | Full Docker Compose deployment: ferryman, TLS, three demo upstreams, Prometheus + Grafana, an asserted `demo.sh` tour. |
| [`examples/embedded/`](https://github.com/Bunty9/ferryman/tree/main/examples/embedded) | Embedding `ferryman::serve` as a library inside your own async Rust app, with its own admin server. |
| `examples/embedded/src/guarded.rs` | A standalone `ferryman_core::Breaker` guarding a `reqwest` call. |
| `crates/core/examples/guarded_client.rs` | The breaker guarding any fallible async call, no proxy involved. |

The examples use only ferryman's public API. See also
[`examples/README.md`](https://github.com/Bunty9/ferryman/blob/main/examples/README.md).
