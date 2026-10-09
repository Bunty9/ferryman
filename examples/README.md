# Reference examples

| Example | Integration depth | When to use it | Command |
| --- | --- | --- | --- |
| [`full-stack/`](./full-stack) | Whole deployment: Docker Compose running ferryman, three demo upstreams, TLS termination, Prometheus and Grafana | You want to see the shape of a real deployment — routing, breaker, failover, alerting, hot reload — asserted end to end | `bash examples/full-stack/demo.sh --ci` |
| [`embedded/`](./embedded) | Library embed: `ferryman::serve` running inside your own async Rust process, with its own admin server | You want to embed ferryman in your own app instead of running the standalone binary | `cargo run -p ferryman-embedded-example -- --config examples/embedded/config.toml` |
| [`embedded/src/guarded.rs`](./embedded/src/guarded.rs) | A standalone `Breaker` guarding a `reqwest` call, using only the public API | You want the breaker around an outbound HTTP client | `cargo test -p ferryman-embedded-example --test guarded` |
| [`guarded_client.rs`](../crates/core/examples/guarded_client.rs) | Core only: `ferryman-core`'s circuit breaker guarding any fallible async call, no proxy or HTTP involved | You want just the breaker (e.g. around a DB query or another service's SDK), not the reverse proxy | `cargo run -p ferryman-core --example guarded_client` |

Each example's own README has more detail. `full-stack` and `embedded`
are workspace members with `publish = false`; `guarded_client` is a
Cargo example of the published `ferryman-core` crate, not a separate
package. All three use only ferryman's public API. `embedded` and
`guarded_client` build and run the same way a consumer of the published
crates would; `full-stack` builds ferryman from this checkout (see
`examples/full-stack/README.md`'s "Using crates.io instead of this repo"
to use the published binary; its latency panel and `FerrymanSlowP99`
alert need ferryman 0.2.0 or later).
