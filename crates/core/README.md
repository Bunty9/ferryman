# ferryman-core

[![crates.io](https://img.shields.io/crates/v/ferryman-core.svg)](https://crates.io/crates/ferryman-core)
[![docs.rs](https://img.shields.io/docsrs/ferryman-core)](https://docs.rs/ferryman-core)

Building blocks of the [ferryman](https://github.com/Bunty9/ferryman)
reverse proxy, usable on their own:

- **Circuit breaker**: lock-free closed / open / half-open state machine
  with a single half-open probe per cooldown. Every admission returns an
  `Admission` ticket, so results from requests that finish after a trip
  can't flip the state.
- **Routing table**: longest-prefix match on path-segment boundaries
  (`/svc-a` matches `/svc-a/x`, not `/svc-ab`), hot-swappable through
  `arc-swap`.
- **Config**: TOML schema with strict validation; a rebuilt table keeps
  each surviving upstream's breaker across reloads.
- **Health checks**: concurrent active probes that feed the breakers.

```rust
use ferryman_core::{build_table, ConfigToml};

let cfg: ConfigToml = toml::from_str(r#"
    [[routes]]
    prefix = "/svc-a"
    upstream = "http://127.0.0.1:8001"
"#)?;
let table = build_table(cfg, None)?;

let route = table.lookup("/svc-a/users").expect("route");
if let Some(ticket) = route.upstream.try_acquire() {
    // ... forward the request, then report the outcome:
    route.upstream.record_success(ticket);
} else {
    // circuit open: answer 503
}
```

See the [architecture notes](https://github.com/Bunty9/ferryman/blob/main/docs/architecture.md)
for how the breaker and reload work.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
