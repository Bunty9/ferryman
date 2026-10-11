# FAQ

**Why do I get 502, then 503?**
The first failures are 502 (connect error, transport error). After
`failure_threshold` consecutive failures the circuit opens and requests get
503 without touching the upstream until the cooldown or a passing health
check ([Circuit breaker](circuit-breaker.md)).

**My upstream is healthy but I get 503.**
Its circuit is open or half-open. Check `ferryman_circuit_state`, the
`warn` log line when it opened, and whether routes sharing the upstream
(`host:port`) agree. A passing health check at `health_path` closes it within
`health_interval_secs`. If the health endpoint 404s, any answer below 500 is
still healthy.

**Why do my clients see 502 behind an AWS ALB?**
ferryman closed an idle keep-alive connection that the ALB was about to
reuse. Set `keepalive_timeout_secs = 75` (ALB idle timeout + 15) or lower the
ALB idle timeout to 9 s or less ([Timeouts](timeouts.md#behind-a-load-balancer)).

**The app sees ferryman's IP, or the wrong scheme.**
Add the front proxy or load balancer to `trusted_proxies`. Untrusted peers'
`X-Forwarded-*` values are replaced or stripped
([Forwarded headers](forwarded-headers.md)).

**My ConfigMap edit never takes effect.**
You mounted it with `subPath` (the kubelet never updates those), or pointed
`--config` at `.../..data/config.toml`. Mount the ConfigMap as a directory and
use the top-level file. Allow up to 60-90 s for the kubelet sync.

**The container fails at startup with a permission error.**
The image runs as uid 65532. Make the config, cert and key readable by it
(`chmod 0644`, or group-readable plus `--group-add`/`fsGroup`).

**Does it support WebSocket, gRPC, https upstreams, several upstreams per route, ACME?**
Not yet: `Upgrade` is answered with 501, upstreams are plain HTTP/1.1, and
there is one upstream per route. See [Comparison](comparison.md#current-limitations).

**Does `/readyz` go red when an upstream is down?**
No. It only flips to 503 during shutdown, so a dead upstream never takes
ferryman out of rotation (and cannot cause an outage of the healthy routes).

**Can a route hide a path from the upstream?**
No, routes are not access control ([Routing](routing.md#routing-is-not-access-control)).

**Is the metrics endpoint authenticated?**
No. Bind it to a private address and keep it off public ingress.

**Does ferryman export traces?**
No. `traceparent` / `tracestate` pass through untouched; ferryman does not
emit spans over OTLP. Use `request_id` and the access log for correlation.

## Performance and footprint

The repository's benchmark harness
([`benches/wrk2.lua`](https://github.com/Bunty9/ferryman/blob/main/benches/wrk2.lua),
`scripts/wrk2-smoke.sh`) runs in CI, and CI numbers are the reference; numbers
from a shared dev machine are not. The original **targets** (goals, not
measurements) were:

```bash
wrk2 -c 1000 -t 16 -R 50000 -d 60s -s benches/wrk2.lua http://localhost:8080/svc-a/echo
```

| Metric | Target |
| --- | --- |
| Throughput | 50,000 rps |
| p50 / p99 / p999 latency | < 1 ms / < 5 ms / < 20 ms |
| RSS at 50k rps idle | < 20 MB |
| Failover recovery after upstream `kill` | < 5 s (one health interval) |
| CPU vs NGINX on same hardware | >= 50% reduction (Pingora story: ~70%) |

## Built with

| Layer | Crate / tool |
| --- | --- |
| Async runtime | `tokio` |
| HTTP server | `hyper` 1.x + `hyper-util` auto (HTTP/1.1 + HTTP/2) |
| HTTP client (proxy) | `hyper-util` legacy `Client<HttpConnector>` |
| HTTP client (health) | `reqwest` (plain HTTP, no TLS stack) |
| TLS termination | `rustls` (`ring`) + `tokio-rustls`, ALPN h2 / http/1.1 |
| Config | `serde` + `toml` + `notify` (FS watch) |
| Hot swap | `arc-swap` (atomic `RouteTable` swap) |
| Observability | `tracing` + `metrics-exporter-prometheus` |
| CLI | `clap` 4 |
| Container build | `cargo-chef` multi-stage, `FROM scratch` |
| CI | GitHub Actions (stable + beta) + `cargo-deny` + `cargo-nextest` + `wrk2` |

Pinned versions are in
[`Cargo.toml`](https://github.com/Bunty9/ferryman/blob/main/Cargo.toml).

## Contributing to these docs

The book lives in `book/` and is built by `.github/workflows/docs.yml`
(`mdbook build book`; deployed to GitHub Pages on pushes to `main`).
Preview locally with `mdbook serve book`. The changelog page is included
from `CHANGELOG.md`.
