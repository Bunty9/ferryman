# Operations

## Admin endpoints

A small admin server binds a separate listener (`--metrics-bind`, default
`0.0.0.0:9090`). **Keep it off the public internet.**

| Path | Response |
| --- | --- |
| `/metrics` | Prometheus text format. |
| `/healthz` | `200 ok` while the process is alive (liveness). |
| `/readyz` | `200 ok` while serving; `503` once shutdown (SIGTERM/SIGINT) has begun. Not tied to upstream health: a dead upstream never takes ferryman out of rotation. |

Anything else is `404`; methods other than `GET`/`HEAD` are `405`.

### Probes

- Kubernetes: `livenessProbe` -> `/healthz`, `readinessProbe` -> `/readyz`,
  both on port 9090.
- ECS/ALB health checks can target 9090 `/healthz`, or the proxy port with
  `local_health_path`.
- PaaS platforms that only probe the serving port (Fly, Render, Railway,
  Cloud Run) should set `local_health_path = "/up"` and probe that. It
  answers `200 ok` through the shutdown delay too; only the admin `/readyz`
  signals draining.
- Docker: the image's `HEALTHCHECK` runs `ferryman healthcheck`.

## CLI flags and environment

| Flag | Environment | Default |
| --- | --- | --- |
| `--config` | `FERRYMAN_CONFIG` | none (or inline `FERRYMAN_CONFIG_TOML`) |
| `--bind` | `FERRYMAN_BIND` | `--bind` > `FERRYMAN_BIND` > `0.0.0.0:$PORT` (ignored with a warning if not a valid port) > `0.0.0.0:8080` |
| `--metrics-bind` | `FERRYMAN_METRICS_BIND` | `0.0.0.0:9090` |
| `--tls-cert` / `--tls-key` | `FERRYMAN_TLS_CERT` / `FERRYMAN_TLS_KEY` | none (plain HTTP) |
| `--log-format text\|json` | `FERRYMAN_LOG_FORMAT` | `json` |
| n/a | `RUST_LOG` | `info` |

### Subcommands

- `ferryman` (no subcommand) runs the proxy.
- `ferryman check [--config PATH]` validates the config (parse, route table,
  and TLS cert/key when `--tls-cert`/`--tls-key` are set) without binding
  anything. Prints `config ok: N routes` or the error chain to stderr; exit 0
  or 1. Use it as a CI gate before deploying a config. It checks the config
  file and TLS files only; bind addresses are not tested for availability.
- `ferryman healthcheck [--url URL]` does an HTTP/1 GET (3 s budget) on
  `http://127.0.0.1:<--metrics-bind port>/healthz` and exits 0 on 2xx, else
  1. The Docker image has no shell or curl, so this is its `HEALTHCHECK`
  (`CMD ["/usr/local/bin/ferryman", "healthcheck"]`); in an ECS task
  definition use `"healthCheck": {"command": ["CMD",
  "/usr/local/bin/ferryman", "healthcheck"]}`. If you set a non-default
  metrics port via flag, pass `--metrics-bind` (or `FERRYMAN_METRICS_BIND`)
  to the healthcheck too, or give `--url`.

## Reloading

- **File watch.** ferryman watches the *directory* of the config file (and of
  the TLS cert/key). Any change there schedules a reload after 200 ms of
  quiet, but only if the file's bytes actually changed (content hash), so
  `touch` and identical rewrites are ignored. This covers editors that
  rename-replace and Kubernetes ConfigMap/Secret mounts, which swap a
  `..data` symlink instead of touching `config.toml`. Mount the whole
  ConfigMap as a directory (not with `subPath`, which never updates) and
  point `--config` at the file in it.
- **SIGHUP (Unix).** Forces a reload of the config and the TLS certificate
  even if nothing changed. systemd: `ExecReload=/bin/kill -HUP $MAINPID`.
  With inline `FERRYMAN_CONFIG_TOML` it logs "reload not applicable": the
  config is not re-read, but TLS files still are. Windows has no SIGHUP; only
  the file watch applies. (Before 0.3, SIGHUP terminated the process.)
- **TLS cert/key.** A changed `--tls-cert`/`--tls-key` pair (for example a
  cert-manager Secret renewal) is picked up the same way and used for *new*
  handshakes; open connections keep their old session. A bad or mismatched
  pair is logged and the previous certificate stays. Turning TLS on or off,
  or changing the cert/key paths, needs a restart.
- **Failure.** A config that fails to parse or validate is logged and the old
  table stays live. Bind addresses, the metrics bind and
  `health_interval_secs` need a restart.

## Graceful shutdown

On SIGTERM/SIGINT: `/readyz` -> 503, keep serving for `shutdown_delay_secs`,
stop accepting, drain for up to `drain_timeout_secs`, exit. Sizing against
platform kill timeouts is in [Timeouts & limits](timeouts.md#shutdown-timing).

## Access log

Logs go to stdout as JSON by default; `--log-format text` gives the
human-readable format. Filter with `RUST_LOG`.

With `access_log = true` (opt-in, to keep log volume down at high request
rates) every request, including early rejections (400, 404, 501, 503, 408,
502, 504 and `local_health_path`), logs one `info` line with target
`ferryman::access` and fields: `method`, `host` (URI authority or `Host`, `-`
if none), `path`, `route` (config prefix, `none`, or `local_health`),
`upstream` (`host:port` or `none`), `status`, `bytes_out`, `duration_ms`,
`request_id`, `peer` (connecting IP, not `X-Forwarded-For`), `scheme`,
`version`.

- The query string is never logged (it may carry secrets). `path` is the raw
  path, non-visible-ASCII bytes replaced by `?`, truncated to 512 bytes.
- `duration_ms` runs to the response headers (like the metric), not the end of
  the streamed body. `bytes_out` is the response `Content-Length` when the
  upstream sent one; it is omitted otherwise and for ferryman's own error
  responses.
- Each request runs in a `request` span with bounded fields `method`,
  `route`, `upstream` (never the raw path), so breaker transition logs emitted
  during the request appear under it. The span is only created when
  `access_log` or `request_id` is on.
- Not logged: requests hyper rejects before the service runs (malformed
  request line, 431 oversized headers) and requests whose client aborts before
  a response is produced.
- `request_id = true`: a valid incoming `x-request-id` (1-128 visible ASCII
  bytes, a single header) is kept, else a random 32-hex id replaces it. It is
  sent upstream, echoed on the response (unless the upstream sets its own) and
  logged. With `request_id = false` ferryman does not touch the headers (the
  response never gets an `x-request-id` from ferryman) but the access log
  still shows a valid incoming id.
- `traceparent` and `tracestate` are end-to-end headers: forwarded to the
  upstream and returned untouched. ferryman does not emit spans over OTLP.

## Metrics

| Metric | Labels | Description |
| --- | --- | --- |
| `ferryman_requests_total` | `route`, `upstream`, `status` (`"none"` on 404 and 400 bad path) | Counter of inbound requests. |
| `ferryman_request_duration_seconds` | `route`, `upstream` | Histogram (buckets `le`), time from request start (includes the upload) to upstream response headers. |
| `ferryman_upstream_alive` | `upstream` | Gauge: 1 = circuit closed, 0 otherwise (also for `health_disabled` upstreams). |
| `ferryman_circuit_state` | `upstream` | Gauge: 0 closed / 1 open / 2 half-open. |

`route` is the configured prefix and `upstream` is `host:port`, so label
cardinality is bounded by the config, never by client input.

### Grafana and alerts

[`examples/full-stack/`](https://github.com/Bunty9/ferryman/tree/main/examples/full-stack)
ships a provisioned Grafana dashboard
([`ferryman.json`](https://github.com/Bunty9/ferryman/blob/main/examples/full-stack/grafana/dashboards/ferryman.json))
and Prometheus alert rules
([`alerts.yml`](https://github.com/Bunty9/ferryman/blob/main/examples/full-stack/prometheus/alerts.yml):
`FerrymanDown`, `FerrymanCircuitOpen`, `FerrymanUpstreamDown`,
`FerrymanHigh5xxRatio`, `FerrymanSlowP99`). Import the JSON into your own
Grafana, or run `docker compose up` in that directory. The latency panel and
`FerrymanSlowP99` need ferryman 0.2.0 or later.
