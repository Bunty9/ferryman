# Routing

For each request ferryman picks the **longest matching prefix** on a
path-segment boundary: `/a` matches `/a` and `/a/b`, never `/ab`, among the
routes of one *host group* (see [Host routing](#host-routing)). No match is a
404.

Request path order, before anything reaches an upstream:

1. `local_health_path` is answered first, if configured.
2. Path checks on the raw path (`bad path` -> 400): `.`/`..` segments (also
   `%2e`, `..;`, or after an encoded `%2f`/`%5c`/`\` separator), `%00`, `%u`,
   and double-encoded dots or slashes (`%252e`, `%252f`). When the raw path
   contains `%`, the normalised path is checked too.
3. Ambiguity check (`bad path` -> 400): a path whose reading with `%2F`/`%5C`/`\`
   as `/` and `;params` dropped selects a *different* route than the raw path
   (`/api%2Fsecret`, `/api;x/secret`; `group%2Fproject` under `/api` is fine).
4. `Host` validation (`bad host` -> 400): a single bare `host[:port]` with a
   valid port, present on HTTP/1.1, when the URI has no authority. More than
   one `Host` header is always 400.
5. Route lookup (host group, then longest prefix), then the upstream's circuit breaker ([Circuit
   breaker](circuit-breaker.md)).

## Normalisation

Matching uses a normalised copy of the path; **the forwarded path is never
rewritten** (except by `strip_prefix`, below):

- `%XX` escapes of unreserved characters (`A-Za-z0-9-._~`) are decoded.
- Hex digits of the remaining escapes are uppercased.
- Repeated `/` are merged.

Decoding is a single pass: double-encoded characters (`%2561`) are not decoded
further. Nothing else is decoded, so `/api%2fx` does not match `/api`, and
`/API/x` does not either (matching is case-sensitive). Prefixes must be
written in this normalised form; the config is rejected otherwise. Routes are
case-sensitive and `%c0%af` / `%3B` are not interpreted, so case-folding or
overlong-UTF-8 upstreams are not detected.

## Host routing

A route may set `host = "app.example.com"` or `host = "*.example.com"`.
Routes without `host` are the default (hostless) set.

The host of a request is the host of the URI authority (HTTP/1 absolute
form, HTTP/2 `:authority`) when there is one, else the single `Host` header:
the same value the `Host` validation above judges, so the two never
disagree. It is lowercased, its port removed and ONE trailing dot removed
(`App.Example.com.:8443` is `app.example.com`).

1. An exact `host` route group wins.
2. Else the wildcard group with the longest suffix. `*.example.com` matches
   any name with at least one extra leading label (`a.example.com`,
   `a.b.example.com`) and never `example.com` itself.
3. Else the hostless routes (unknown host, no `Host` on HTTP/1.0, IP
   literals).

Inside the chosen group the longest prefix wins. **There is no fallback**:
if the chosen host group has no matching prefix the answer is 404, not a
hostless or wildcard route (that would send `/admin` on a host you
configured to a different service). A dead upstream is 503 as always.

`host` must be lowercase ASCII (write an IDN as `xn--` punycode), a valid DNS
name (labels 1-63 of `[a-z0-9-]`, no leading or trailing `-`, at most 253
bytes), without port or trailing dot, and not an IP address. `*` is only
allowed as the whole first label. The same `host` and `prefix` twice is
rejected; the same prefix on different hosts is fine. The `route` metric
label of a host route is `host` + prefix (`app.example.com/api`); hostless
routes keep the plain prefix. Routes in different groups may share an
upstream.

```toml
[[routes]]
host = "app.example.com"
prefix = "/"
upstream = "http://127.0.0.1:3000"

[[routes]]
host = "*.example.com"
prefix = "/"
upstream = "http://127.0.0.1:3001"

[[routes]]
prefix = "/"          # everything else
upstream = "http://127.0.0.1:3002"
```

## Routing is not access control

Case variants and `%2f` variants fall to a less specific route (especially
`/`) and may be decoded or merged into a more specific path by the upstream.
Don't rely on a route to hide paths of an upstream that another route also
reaches. A dead upstream is always a 503 for its prefix: lookup ignores
health and never falls back to a shorter prefix.

## `strip_prefix`

`strip_prefix = true` removes the matched prefix from the path sent upstream
and sets `X-Forwarded-Prefix` to it:

| Request | Route prefix | Sent upstream |
| --- | --- | --- |
| `/api/users?x=1` | `/api` | `/users?x=1` |
| `/api` | `/api` | `/` |

The path is checked (`bad_path`) before and after stripping and is forwarded
normalised (repeated `/` merged, unreserved `%XX` decoded, other escapes' hex
uppercased); the query is untouched. If the remainder fails the check the
answer is 400. `X-Forwarded-Prefix` is written after the forwarded headers
are applied, so trust rules apply ([Forwarded headers](forwarded-headers.md)).

Apps mounted under a prefix need `strip_prefix = true` plus their own setting
to honour `X-Forwarded-Prefix`: FastAPI `root_path` (or `uvicorn
--root-path`), Grafana `server.serve_from_sub_path = true` with a `root_url`
ending in the prefix, Spring `server.forward-headers-strategy = framework`
(`ForwardedHeaderFilter`).

## `rewrite_host`

By default the client's `Host` is kept (HTTP/2 `:authority` becomes `Host`).
`rewrite_host = true` sends the upstream's `host[:port]` instead, for
upstreams that route on their own name (virtual-host setups, some SaaS
backends). Routes that share an upstream may differ on this key.

Upstreams always get HTTP/1.1. Request and response bodies are streamed,
never buffered, and hop-by-hop headers are stripped both ways.

## Responses ferryman generates

| Situation | Response |
| --- | --- |
| No route matches | 404 `no route` |
| Bad path or ambiguous route | 400 `bad path` |
| Bad `Host` | 400 `bad host` |
| Circuit open | 503 `upstream unavailable` |
| `Upgrade` / `CONNECT` (e.g. WebSocket) | 501 |
| Connect or transport error | 502 `bad gateway` |
| No response headers in time | 504 |
| Client stalled or over-long upload | 408 |

Whether each one counts against the breaker is in
[Circuit breaker & health checks](circuit-breaker.md#what-counts-as-a-failure).
