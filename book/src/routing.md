# Routing

One route has one prefix and one upstream. For each request ferryman picks the
**longest matching prefix** on a path-segment boundary: `/a` matches `/a` and
`/a/b`, never `/ab`. No match is a 404. There is no host-based routing and
no load balancing across several upstreams (one route, one upstream).

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
5. Route lookup, then the upstream's circuit breaker ([Circuit
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
