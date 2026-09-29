# Reference examples

| Example | Integration depth | When to use it | Command |
| --- | --- | --- | --- |
| [`full-stack/`](./full-stack) | Whole deployment: Docker Compose running ferryman, three demo upstreams, TLS termination, Prometheus and Grafana | You want to see the shape of a real deployment — routing, breaker, failover, alerting, hot reload — asserted end to end | `bash examples/full-stack/demo.sh --ci` |
| [`embedded/`](./embedded) | Library embed: `ferryman::serve` running inside your own async Rust process, with its own admin server | You want to embed ferryman in your own app instead of running the standalone binary | `cargo run -p ferryman-embedded-example -- --config examples/embedded/config.toml` |
| [`guarded_client.rs`](../crates/core/examples/guarded_client.rs) | Core only: `ferryman-core`'s circuit breaker guarding any fallible async call, no proxy or HTTP involved | You want just the breaker (e.g. around a DB query or another service's SDK), not the reverse proxy | `cargo run -p ferryman-core --example guarded_client` |

Each example's own README has more detail. All three are workspace
members with `publish = false` and use only ferryman's public API — they
build and run the same way a consumer of the published crates would.
