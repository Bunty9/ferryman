# ferryman-proxy

The request pipeline of the [ferryman](https://github.com/Bunty9/ferryman)
reverse proxy: path and host validation, routing lookup, circuit-breaker
admission, forwarding-header policy, and the streaming upstream forward.

Most users want the [`ferryman`](https://crates.io/crates/ferryman) crate,
which re-exports this one (`ferryman::proxy`). Licensed under MIT OR Apache-2.0.
