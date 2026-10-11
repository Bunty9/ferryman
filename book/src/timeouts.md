# Timeouts & limits

| Limit | Default | Applies to | On expiry |
| --- | --- | --- | --- |
| TLS handshake + first request on a connection | 10 s (fixed) | New connections | Connection closed |
| `keepalive_timeout_secs` | 10 s | Later HTTP/1 request heads on a reused connection | Connection closed |
| `request_body_idle_timeout_secs` | 30 s | Gap between request-body frames | 408, no breaker effect |
| `request_body_timeout_secs` | 300 s | Whole request body | 408, no breaker effect |
| `upstream_timeout_secs` | 30 s | Upstream response *headers*, counted from the end of the request body (from the start if bodyless) | 504, counts as a breaker failure |
| `health_interval_secs` | 5 s | Health probe cadence | n/a |
| `drain_timeout_secs` | 25 s | In-flight requests after the listener closes | Connections cut |
| `shutdown_delay_secs` | 0 s | Keep accepting with `/readyz` at 503 | n/a |
| HTTP/2 keep-alive ping | 30 s (fixed) | HTTP/2 clients | Peer disconnected if no answer |

Request and response bodies are streamed, never buffered. The response body
has **no idle timeout** once headers have arrived. `duration_ms` and
`ferryman_request_duration_seconds` run to the response headers, not the end
of the streamed body.

## Behind a load balancer

An LB that reuses backend connections needs ferryman to keep them open longer
than the LB does, or it may send a request just as ferryman closes the socket
and return 502. Set `keepalive_timeout_secs` accordingly:

| Load balancer | Setting |
| --- | --- |
| AWS ALB | ALB idle timeout + 15 s: `75` at the default 60 s. (Or set the ALB idle timeout to 9 s or less and keep the default 10.) |
| Google Cloud LB | `620` (GCLB holds backend connections 600 s). |
| Direct clients | Keep the default `10`. |

The first request on a new connection is always bounded at 10 s. A larger
value also widens the header-read window on reused connections: a client can
send one request and then hold the connection for up to this value, so raise
it only behind a load balancer. Changes apply to new connections on hot
reload. Also add the LB's range to `trusted_proxies`
([Forwarded headers](forwarded-headers.md)).

## Shutdown timing

On SIGINT/SIGTERM `/readyz` goes 503 immediately and ferryman keeps serving
for `shutdown_delay_secs`, then stops accepting and drains in-flight
connections for up to `drain_timeout_secs`. **Delay + drain must stay below
the platform's kill timeout**:

| Platform | Kill timeout | Suggested |
| --- | --- | --- |
| Cloud Run | 10 s after SIGTERM | delay 0, drain <= 8 |
| Heroku | 30 s | defaults (0 + 25) |
| Kubernetes | `terminationGracePeriodSeconds` (30) | delay 10, drain 15 |
| systemd | `TimeoutStopSec` (35 s in the unit) | defaults |
| Fly | `kill_timeout` (30 s in `fly.toml`) | defaults |

A second SIGTERM/SIGINT during the delay or drain exits immediately with code
130. Signals received before the listeners are up use the OS default action.
