# Circuit breaker & health checks

Each distinct upstream (`host:port`) has one lock-free circuit breaker,
shared by every route that points at it.

## States

| State | Behaviour |
| --- | --- |
| **Closed** | Requests flow. `failure_threshold` consecutive failures open the circuit. |
| **Open** | Requests get 503 without touching the upstream, for the cooldown (`default_cooldown_secs`, or the route's `cooldown_secs`). |
| **Half-open** | After the cooldown exactly one request is let through as the probe. Success closes the circuit; failure reopens it. |

The `ferryman_circuit_state` gauge reports 0 closed / 1 open / 2 half-open.
State changes are logged with `upstream`, `from` and `to` fields: `warn` when
a circuit opens, `info` otherwise.

## Admission tickets

`try_acquire` hands out an `Admission` ticket: `Normal` while closed, or the
single `Probe` once the cooldown has elapsed. Results are reported through the
ticket, and only a `Probe` may move the circuit out of open or half-open. A
`Normal` request that was admitted before the breaker tripped and finishes
late is ignored: it says nothing about whether the upstream is healthy *now*,
and without this rule a slow request racing an outage could flip a
freshly-opened breaker straight back to closed. Releasing a ticket without a
verdict (`release`) is used for client-side failures: it never counts as
success or failure.

## What counts as a failure

| Situation | Response | Counts against breaker |
| --- | --- | --- |
| No route matches | 404 | no |
| Bad path / bad host | 400 | no |
| Circuit open | 503 | no |
| `Upgrade` / `CONNECT` | 501 | no |
| Connect / transport error | 502 | yes |
| No response headers within `upstream_timeout_secs` of the request body completing (of the request start if bodyless) | 504 | yes |
| Client's request body fails mid-upload | 400 | no |
| Client stalls its upload for `request_body_idle_timeout_secs`, or exceeds `request_body_timeout_secs` in total | 408 | no |
| Upstream stops reading the upload for a whole `upstream_timeout_secs` window | 504 | yes |
| Transport error or upstream 502/503/504 after the client left its upload idle for >= min(1 s, `request_body_idle_timeout_secs` / 2) | 502 / passed through | no |
| Upstream answers 502/503/504 | passed through | yes |
| Anything else from upstream (including 4xx and other 5xx) | passed through | success |

The rule of thumb: a slow or broken *client* never trips the breaker; a slow
or broken *upstream* does.

## Health-driven recovery

The `ferryman` binary runs a health loop: every `health_interval_secs` it
probes each upstream's `health_path` (default `/health`). **Any answer below
500 is healthy**; transport errors, timeouts and 5xx are not. The probes are
plain HTTP. An embedder calling `ferryman::serve` alone has no health loop
unless it spawns `health_loop` itself.

- A passing check closes an open or half-open circuit immediately; it does
  not wait for the cooldown. Passing while already closed is a no-op and does
  not reset the request failure count. A recovered upstream is routable again
  within about one `health_interval_secs`.
- A failing check while already open changes nothing: it does not restart the
  cooldown. Once the cooldown has elapsed, one request is let through as the
  half-open probe whether or not `/health` is failing, and if it succeeds the
  circuit closes. So a broken health endpoint cannot pin a circuit open while
  real traffic works. Failing checks while closed still count toward
  `failure_threshold`, and a failed half-open probe or a failing check
  landing while half-open reopens the circuit. Without a running health loop,
  recovery is the same: cooldown expiry plus one successful half-open
  request.
- With a broken health endpoint and sparse traffic the circuit can flap (open
  on health failures, close on the next half-open request). Set
  `health_disabled = true` for an upstream whose health endpoint is broken or
  absent by design.
- `health_disabled = true` makes the loop skip the upstream entirely: no
  probe and no effect on its breaker, so only request traffic drives it.
  `ferryman_upstream_alive` is then published from the circuit state
  (1 = closed, 0 = open/half-open), not from probes.
- Changing `health_path` or `health_disabled` on reload keeps the breaker's
  state. Failing checks while open are not logged.

Health state is **not** consulted by routing: a dead upstream answers 503 for
its prefix; ferryman never falls back to a shorter prefix.

## Using the breaker without the proxy

The same breaker is a public type; see [Embedding the
library](embedding.md#the-breaker-on-its-own).
