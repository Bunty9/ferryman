# Configuration reference

ferryman reads one TOML file ([`config.toml`](https://github.com/Bunty9/ferryman/blob/main/config.toml)
in the repository is a commented example). **Unknown keys are rejected**, so
typos fail loudly instead of silently falling back to defaults. Validate a
file with `ferryman check --config PATH`.

All `*_secs` values must be between 1 and 86400 (one day), except
`shutdown_delay_secs` which may be 0. An invalid config on reload is logged
and the old table stays live.

## Top-level keys

"Reload" says what happens when the file changes while ferryman runs.

| Key | Type | Default | Reload | Meaning |
| --- | --- | --- | --- | --- |
| `health_interval_secs` | integer | `5` | restart | Active health probe interval. |
| `default_cooldown_secs` | integer | `30` | hot | How long an open circuit refuses traffic before one probe. Per-route `cooldown_secs` overrides it. |
| `failure_threshold` | integer (>= 1) | `3` | hot | Consecutive failures that open a closed circuit. |
| `upstream_timeout_secs` | integer | `30` | hot | Time allowed for an upstream to send response headers, counted from the end of the request body. |
| `keepalive_timeout_secs` | integer | `10` | new connections | HTTP/1 keep-alive idle timeout (ALB: 75, GCLB: 620). See [Timeouts](timeouts.md). |
| `request_body_idle_timeout_secs` | integer | `30` | hot | Longest gap between request-body frames (408 when exceeded). |
| `request_body_timeout_secs` | integer | `300` | hot | Total time to receive a request body (408 when exceeded). |
| `drain_timeout_secs` | integer | `25` | read at shutdown | After the listener closes on shutdown, how long to wait for in-flight requests before cutting them. Delay + drain must stay under the platform kill timeout. |
| `shutdown_delay_secs` | integer (0-86400) | `0` | read at shutdown | On SIGTERM/SIGINT `/readyz` flips to 503 at once and ferryman keeps accepting for this long before the drain starts. |
| `local_health_path` | string | none | hot | Path the proxy answers itself (`GET`/`HEAD` -> `200 ok`, no upstream, no breaker; metric `route="local_health"`), for PaaS platforms that only probe the serving port. Absolute, no `?`/`#`; rejected if equal to a route prefix; shadows a route prefix it falls under (e.g. `/up` under a `/` catch-all). |
| `trusted_proxies` | array of CIDR/IP strings | `[]` | hot (per request) | Peers whose forwarding headers are trusted. IPv4 clients match only IPv4 ranges. See [Forwarded headers](forwarded-headers.md). |
| `forwarded_header` | bool | `false` | hot | Also add an RFC 7239 `Forwarded` element `for=<peer>;proto=<scheme>;host=<Host>` (IPv6 as `for="[2001:db8::1]"`). |
| `xff` | `"append"` / `"replace"` | `"append"` | hot | `append`: peer added to the incoming `X-Forwarded-For`. `replace`: set to the single client address ferryman puts in `X-Real-IP`. |
| `access_log` | bool | `false` | hot | One `ferryman::access` log line per request ([Operations](operations.md#access-log)). |
| `request_id` | bool | `false` | hot | Generate `x-request-id` when absent (or invalid) and set it on the upstream request and the response. |

Bind addresses, the metrics bind, TLS on/off and the TLS cert/key *paths* are
CLI flags or environment variables and need a restart
([CLI flags](operations.md#cli-flags-and-environment)).

## `[[routes]]` keys

At least one route is needed. Routes are matched by longest prefix
([Routing](routing.md)).

| Key | Type | Default | Reload | Meaning |
| --- | --- | --- | --- | --- |
| `prefix` | string | required | hot | Path prefix, matched on segment boundaries (`/a` does not match `/ab`) against the normalised, case-sensitive request path. Must start with `/` and be written in normalised form (rejected at load otherwise); may not contain `;`, `\`, `%2F` or `%5C`. Duplicates are rejected. |
| `upstream` | string | one of `upstream`/`upstreams` | hot | `http://host:port`: no path, no query, no `https`. A host is required (`http://:80` is rejected). One endpoint. |
| `upstreams` | array | one of `upstream`/`upstreams` | hot | Several endpoints: each a URI string (as `upstream`) or a table `{ addr = "http://host:port", weight = 3 }`. Non-empty; the same `host:port` twice in one route is rejected, as is setting both `upstream` and `upstreams`. |
| `lb` | string | `"round_robin"` | hot | How `upstreams` are picked: `round_robin` (smooth weighted), `least_conn` (fewest in-flight requests, counted until the response body finishes; ties rotate; weights must stay 1) or `p2c` (two endpoints drawn by weight, the less loaded wins). |
| `cooldown_secs` | integer | `default_cooldown_secs` | hot | Per-route cooldown override. |
| `health_path` | string | `"/health"` | hot | Health probe path: absolute, no `?` or `#`. |
| `health_disabled` | bool | `false` | hot | Skip active probing of this route's upstream ([Circuit breaker](circuit-breaker.md#health-driven-recovery)). |
| `rewrite_host` | bool | `false` | hot | `true` sends the upstream's `host[:port]` as `Host` (HTTP/1 and HTTP/2); default keeps the client's. |
| `strip_prefix` | bool | `false` | hot | `true` removes the matched prefix before forwarding (`/api/users?x=1` under `/api` becomes `/users?x=1`; `/api` becomes `/`) and sets `X-Forwarded-Prefix: /api`. See [Routing](routing.md#strip_prefix). A `/` route has nothing to strip. |

### Several upstreams

```toml
[[routes]]
prefix = "/api"
lb = "least_conn"
upstreams = ["http://api-1:8000", { addr = "http://api-2:8000", weight = 2 }]
```

`weight` is `1..=1000` (default 1) and applies to `round_robin` and `p2c`;
any weight other than 1 under `least_conn` is rejected at load.
Each endpoint (`host:port`) has its own circuit breaker and health probe. A
request is offered to endpoints in policy order and goes to the first whose
breaker admits; endpoints that refuse (open circuit) are skipped. If every
endpoint refuses, the answer is 503: the request never falls back to another
route. With `round_robin`, a recovering endpoint's request-path half-open
probe is only sent when its turn comes up, so recovery can lag; active health
probes cover this, so enable health checks on multi-endpoint routes (do not
set `health_disabled` there unless traffic is steady). The `upstream` metric label is the chosen endpoint's `host:port`; for
events with no chosen endpoint (503 with all endpoints refusing, a 501 or
413 before selection) it is the endpoint of a one-endpoint route and
`multiple` otherwise. A reload resets round-robin position but keeps every
surviving endpoint's breaker.

Routes pointing at the same `host:port` share one circuit breaker, so they
must agree on `cooldown_secs`, `health_path` and `health_disabled` (a
mismatch is rejected when the config is built). `rewrite_host` and
`strip_prefix` may differ between routes that share an upstream. A hot reload
keeps each surviving upstream's breaker and its state, so an open circuit
stays open across a config edit; cooldown and threshold changes apply to it.

## Inline config (`FERRYMAN_CONFIG_TOML`)

When `FERRYMAN_CONFIG_TOML` is set and non-empty, its value is the whole TOML
config and `--config` / `FERRYMAN_CONFIG` are ignored (the Docker image's
default `CMD` passes `--config`, so a flag cannot be the tiebreaker). No file
is watched, so the config is not hot-reloaded (a startup log line says so;
SIGHUP logs "reload not applicable"); change the variable and restart.
`ferryman check` honours it too.

## Example

```toml
health_interval_secs = 5
default_cooldown_secs = 30
failure_threshold = 3
upstream_timeout_secs = 30
trusted_proxies = ["10.0.0.0/8"]
access_log = true

[[routes]]
prefix = "/api"
upstream = "http://api:8000"
strip_prefix = true

[[routes]]
prefix = "/"
upstream = "http://web:3000"
health_path = "/up"
```
