# Comparison & migration

ferryman is a small proxy with a sharp feature: a circuit breaker that is
tied to active health checks and does not let a slow client or a broken
health endpoint wedge it. It is a good fit when you want a few routes in
front of a few HTTP services, readable code, and a breaker you can also reuse
from Rust. It is a poor fit for many things the established proxies do
better. This page tries to be straight about both.

## Current limitations

These are real as of 0.3 and are the usual reasons to pick something else:

- Upstreams are plain HTTP only; `https://` upstreams are rejected.
- No WebSocket / `Upgrade` passthrough (answered with 501).
- No load balancing: one route, one upstream. No retries.
- No host-based routing; routing is by path prefix only.
- No built-in ACME / automatic certificates. Use certbot or lego and let the
  [TLS reload](operations.md#reloading) pick up renewals.
- HTTP/1.1 to upstreams only (the listener speaks HTTP/1.1 and HTTP/2).
- The response body has no idle timeout once headers have arrived.
- Kubernetes `subPath` mounts are never updated by the kubelet: mount the
  ConfigMap as a directory.

## Versus the alternatives

| | Where it is better than ferryman | Where ferryman is the better fit |
| --- | --- | --- |
| **Caddy** | Automatic HTTPS (ACME), multiple upstreams with load-balancing policies, retries, WebSocket, HTTP/3, a large module ecosystem, virtual hosts, https upstreams, a mature Caddyfile. Its passive health tracking and active health checks cover most needs. | You want a half-open circuit breaker rather than a failure-window mark-down, a single static binary with a tiny config surface, readable code, an admin plane with Prometheus metrics by default, or to embed the breaker or proxy in a Rust program. |
| **Traefik** | Dynamic configuration from Docker, Kubernetes and other providers; ACME; middlewares (rate limiting, auth, retry); weighted load balancing; a dashboard; WebSocket and gRPC. It also ships a circuit-breaker middleware. | Static, reviewable TOML instead of provider discovery; a much smaller footprint and codebase; per-upstream breakers wired to health checks by default; no controller to operate. |
| **NGINX** | Decades of production use, the widest feature set (caching, rate limiting, stream/TCP, load balancing, many modules), the best-understood tuning, and enormous community knowledge. Active health checks are a commercial (NGINX Plus) feature, which is a gap ferryman fills. | Active health checks and a circuit breaker out of the box in the open-source binary; hot reload that cannot take down a working config; memory-safe implementation; config that is a handful of lines. |
| **rpxy** | The closest peer: also Rust and hyper-based, with built-in ACME, HTTP/3, host-based routing, and load balancing across several upstreams. | Per-upstream circuit breaking with health-driven recovery, trusted-proxy header handling, drain/readiness behaviour tuned for orchestrators and PaaS, and a standalone embeddable breaker crate. |
| **Pingora** | A framework for building proxies, not a proxy: far more capable and flexible, used at very large scale, with caching, load balancing, connection pooling control and TLS options. If you need to build a custom proxy, start there. | You want a finished, small, configured-by-TOML proxy and not a framework to build one. ferryman is deliberately "Pingora-pattern at miniature scale". |

Pick something else outright if you need WebSocket today, https upstreams,
several upstreams per route, host-based routing, or automatic certificates.
(Several of these are on the roadmap but not in 0.3.)

## Migrating an NGINX `proxy_pass` block

```nginx
location /api/ {
    proxy_pass         http://127.0.0.1:8000/;
    proxy_set_header   Host              $host;
    proxy_set_header   X-Real-IP         $remote_addr;
    proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header   X-Forwarded-Proto $scheme;
    proxy_read_timeout 30s;
}
```

becomes:

```toml
upstream_timeout_secs = 30

[[routes]]
prefix = "/api"
upstream = "http://127.0.0.1:8000"
strip_prefix = true
```

What maps to what:

- **Trailing slash on `proxy_pass`.** `proxy_pass http://host/;` (with a URI
  part) replaces the matched location with `/`, which is `strip_prefix =
  true`. `proxy_pass http://host;` (no URI) forwards the path unchanged, which
  is the default. ferryman matches on segment boundaries, so `/api` also
  covers `/api/x` and never `/apix`; write the prefix without a trailing
  slash.
- **`Host`.** `proxy_set_header Host $host` keeps the client's `Host`: that is
  ferryman's default. NGINX's *default* (no `proxy_set_header`) sends the
  upstream's name: use `rewrite_host = true` for that behaviour.
- **`X-Real-IP`, `X-Forwarded-For`, `X-Forwarded-Proto`.** ferryman sets all
  three itself, so delete those lines. `X-Real-IP` is always derived and
  cannot be forged by clients. If NGINX itself sits in front of ferryman, put
  its address in `trusted_proxies` so its `X-Forwarded-Proto` is honoured.
- **Timeouts.** `proxy_read_timeout` (gap between reads) is closest to
  `upstream_timeout_secs`, but ferryman's timer covers the wait for response
  *headers* only and starts when the request body is fully sent; there is no
  idle timeout on the response body. `proxy_connect_timeout` has no separate
  key. Client body timeouts map to `request_body_idle_timeout_secs` and
  `request_body_timeout_secs`.
- **`upstream { server a; server b; }`.** One route has one upstream; there
  is no load balancing yet.
- **`proxy_next_upstream`, caching, `limit_req`, `auth_request`, `rewrite`.**
  No equivalent.
- **Reload.** `nginx -s reload` becomes `kill -HUP` (or just save the file):
  ferryman watches the config and keeps the old one if the new one is bad.

## Migrating a Caddy `reverse_proxy` block

```caddyfile
example.com {
    handle_path /api/* {
        reverse_proxy localhost:8000
    }
    reverse_proxy localhost:3000
}
```

becomes (TLS terminated by ferryman with `--tls-cert/--tls-key`, or by a load
balancer in front):

```toml
trusted_proxies = []     # add your LB's CIDR if one sits in front

[[routes]]
prefix = "/api"
upstream = "http://localhost:8000"
strip_prefix = true      # handle_path strips the matched prefix

[[routes]]
prefix = "/"
upstream = "http://localhost:3000"
```

What maps to what:

- **`handle_path` vs `handle`.** `handle_path` strips the prefix
  (`strip_prefix = true`); `handle` does not (the default).
- **Site address.** ferryman does no host-based routing. The site name has no
  equivalent; run one ferryman per hostname, or route by path.
- **Automatic HTTPS.** Not built in. Provide a certificate and key
  (certbot/lego/cert-manager) and renew them in place: ferryman reloads them
  for new handshakes.
- **`Host`.** Caddy keeps the client's `Host` by default, like ferryman. A
  `header_up Host {upstream_hostport}` becomes `rewrite_host = true`.
- **`X-Forwarded-*`.** Both set them. Caddy trusts `trusted_proxies` from its
  global options; ferryman's `trusted_proxies` is the same idea (default:
  trust nobody).
- **Health.** `health_uri` becomes `health_path`; `health_interval` becomes
  `health_interval_secs` (global in ferryman, not per route). Passive
  `fail_duration` / `max_fails` map loosely to `default_cooldown_secs` /
  `failure_threshold`, with a half-open probe instead of a time window.
- **`lb_policy` with several upstreams, `lb_try_duration`, `encode`, `file_server`,
  `basicauth`.** No equivalent.

Validate the result before cutting over:

```bash
ferryman check --config ferryman.toml
```
