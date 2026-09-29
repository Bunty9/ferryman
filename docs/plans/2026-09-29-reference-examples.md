# Reference Examples Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship runnable, CI-verified examples that show every ferryman feature end to end, so someone adopting the crates can copy a working setup instead of reverse-engineering the README.

**Architecture:** Three examples at three integration depths. (1) `examples/full-stack/`: the `ferryman` binary deployed with Docker Compose in front of three demo services, with TLS, Prometheus alert rules and a provisioned Grafana dashboard, plus `demo.sh`, a guided tour that asserts every behaviour and doubles as the CI end-to-end test. (2) `examples/embedded/`: a Rust application that embeds `ferryman::serve` as a library and adds its own admin server (`/status`, `/metrics`, `/healthz`). (3) `crates/core/examples/guarded_client.rs`: `ferryman-core`'s circuit breaker guarding an arbitrary outbound call, with no proxy involved.

**Tech Stack:** Rust 1.88+ (hyper 1.x, tokio, metrics-exporter-prometheus 0.16), Docker Compose v2, Prometheus, Grafana, openssl (in an Alpine container), bash + curl + jq.

**Spec:** the user request (2026-09-29): "a working example setup of the ferryman crate in action, implement full feature end to end in the example so it can serve as a strong reference for someone implementing the package in their project". Feature inventory: README.md *Configuration*, *Behaviour* and *Metrics* sections, and `docs/architecture.md`.

## Global Constraints

- Example crates are workspace members with `publish = false` (`cargo publish --workspace` must skip them).
- Example dependencies on ferryman crates carry both a path and a version: `ferryman = { path = "../../crates/server", version = "0.1" }`, so a copied `Cargo.toml` works once the `path` is deleted.
- The examples use only the public API of ferryman 0.1.0. Exception: the histogram change in Task 1 is binary-internal, and the full-stack stack builds the binary from this repo.
- No new third-party crates beyond those already in `[workspace.dependencies]`, plus `serde_json` (already listed).
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny check` and `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` stay green.
- Commits: author `Bunty9 <Bunty9@users.noreply.github.com>`, no AI attribution trailers.
- Demo timings are shortened for the tour (health 2s, cooldown 5s, timeout 2s). Every shortened value carries a comment giving the production value.

## Review Focus

1. **A copied example Cargo.toml has to build outside this repo.** Path-only dependencies break the moment the file leaves the repo. Pin: Task 2 and Task 3 give every ferryman dependency a `version`, and Task 7 adds a CI check that fails if an example crate isn't `publish = false`.
2. **A single-file bind mount breaks hot reload** on editors and tools that save by rename (a known Docker gotcha). Pin: the compose file mounts the config *directory*, and `demo.sh` reloads by `mv`-ing a new file over the old one (Task 5).
3. **Compose's default `stop_grace_period` (10s) is shorter than ferryman's 25s drain**, so `docker compose stop` would SIGKILL in-flight requests. Pin: `stop_grace_period: 30s`, and `demo.sh` checks that a request in flight survives SIGTERM (Task 5).
4. **`install_recorder()` without periodic `run_upkeep()`** lets histograms grow and never expires idle gauges in an embedding app. Pin: the embedded example spawns an upkeep task, and its test checks `/metrics` renders `ferryman_upstream_alive` (Task 3).
5. **A TLS cert without the hostname clients use** only "works" with `curl -k`. Pin: `gen-certs.sh` issues SANs for `localhost`, `127.0.0.1` and `ferryman`, and `demo.sh` uses `--cacert certs/ca.pem`, never `-k` (Task 5).

---

## File structure

```
Cargo.toml                                   modify: add 2 workspace members
crates/server/src/main.rs                    modify: latency histogram buckets, --version, tidy comments
crates/server/tests/metrics.rs               create: histogram bucket rendering test
crates/core/examples/guarded_client.rs       create: breaker guarding a flaky call
examples/README.md                           create: index of the three examples
examples/embedded/Cargo.toml                 create
examples/embedded/README.md                  create
examples/embedded/config.toml                create
examples/embedded/src/lib.rs                 create: Settings, start(), Running
examples/embedded/src/admin.rs               create: /status /metrics /healthz service
examples/embedded/src/main.rs                create: CLI + tracing + recorder install
examples/embedded/tests/embedded.rs          create: end-to-end test on ephemeral ports
examples/full-stack/README.md                create: walkthrough
examples/full-stack/docker-compose.yml       create
examples/full-stack/config/ferryman.toml     create
examples/full-stack/certs/.gitignore         create
examples/full-stack/scripts/gen-certs.sh     create
examples/full-stack/prometheus/prometheus.yml create
examples/full-stack/prometheus/alerts.yml    create
examples/full-stack/grafana/provisioning/datasources/prometheus.yml create
examples/full-stack/grafana/provisioning/dashboards/ferryman.yml    create
examples/full-stack/grafana/dashboards/ferryman.json                create
examples/full-stack/demo.sh                  create
examples/full-stack/demo-upstream/Cargo.toml create
examples/full-stack/demo-upstream/Dockerfile create
examples/full-stack/demo-upstream/src/main.rs create
.github/workflows/ci.yml                     modify: examples job
README.md, CLAUDE.md, CHANGELOG.md           modify: link examples, record changes
```

---

### Task 0: Workspace skeleton (orchestrator)

**Files:** root `Cargo.toml`; `examples/embedded/{Cargo.toml,src/main.rs}`; `examples/full-stack/demo-upstream/{Cargo.toml,src/main.rs}`

**Produces:** workspace members `examples/embedded` (package `ferryman-embedded-example`) and `examples/full-stack/demo-upstream` (package `demo-upstream`), with every dependency declared up front and `Cargo.lock` settled, so later tasks running in parallel never race on the lockfile.

- [ ] Add `"examples/embedded"` and `"examples/full-stack/demo-upstream"` to `[workspace] members`, and a `ferryman = { path = "crates/server", version = "0.1" }` entry to `[workspace.dependencies]`.
- [ ] Create both `Cargo.toml` files with `publish = false`, the workspace-inherited fields, and the dependencies listed in Tasks 2 and 3. Create placeholder `fn main() {}` sources.
- [ ] Run `cargo check --workspace`. Expected: it compiles and `Cargo.lock` gains the two packages.
- [ ] Commit: `Add workspace skeleton for reference examples`.

### Task 1: Binary exports latency as a histogram; `--version`

**Files:** Modify `crates/server/src/main.rs`. Create `crates/server/tests/metrics.rs`. Modify the README *Metrics* table and the CHANGELOG `[Unreleased]` section.

**Why:** the exporter currently renders `ferryman_request_duration_seconds` as a summary (pre-computed quantiles). Summaries can't be aggregated across instances, which matters for the two-region deploy, and they can't feed `histogram_quantile`. The dashboard and alert in Task 5 need buckets.

**Produces:** a metric series `ferryman_request_duration_seconds_bucket{route,upstream,le}`, and `ferryman --version` printing `ferryman 0.1.0`.

- [ ] **Step 1: Write the failing test** `crates/server/tests/metrics.rs`:

```rust
//! The binary's exporter config must render request latency as a
//! Prometheus histogram (buckets), not a summary.
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

#[test]
fn latency_is_exported_as_histogram_buckets() {
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("ferryman_request_duration_seconds".into()),
            ferryman::LATENCY_BUCKETS,
        )
        .unwrap()
        .build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        metrics::histogram!("ferryman_request_duration_seconds", "route" => "/a", "upstream" => "h:1")
            .record(0.003);
    });
    let out = handle.render();
    assert!(out.contains("ferryman_request_duration_seconds_bucket{route=\"/a\",upstream=\"h:1\",le=\"0.005\"} 1"), "{out}");
}
```

- [ ] **Step 2:** Run `cargo test -p ferryman --test metrics`. Expected: it fails to compile because `ferryman::LATENCY_BUCKETS` doesn't exist.
- [ ] **Step 3: Implement.** In `crates/server/src/lib.rs`:

```rust
/// Latency buckets (seconds) for `ferryman_request_duration_seconds`,
/// spanning sub-millisecond proxy hops to multi-second slow upstreams.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
```

  In `main.rs`, add `.set_buckets_for_metric(Matcher::Full("ferryman_request_duration_seconds".into()), ferryman::LATENCY_BUCKETS)?` to the builder, add `version` to `#[command(...)]`, and delete the duplicated "Prometheus exporter binds…" comment block above `load_config`.
- [ ] **Step 4:** Run `cargo test -p ferryman --test metrics`. Expected: PASS. Then `cargo run -q -p ferryman -- --version` should print `ferryman 0.1.0`.
- [ ] **Step 5:** Update the README metrics row to "Histogram (buckets `le`), time to upstream response headers". Add a CHANGELOG `[Unreleased]` entry under *Changed*: "request duration is exported as a histogram with buckets instead of a summary"; under *Added*: "`--version`" and "`ferryman::LATENCY_BUCKETS`". Commit.

(`LATENCY_BUCKETS` is new public API, so it lands in 0.1.1. The embedded example defines its own copy of the bucket list so that it builds against 0.1.0.)

### Task 2: `demo-upstream` service

**Files:** `examples/full-stack/demo-upstream/{Cargo.toml,Dockerfile,src/main.rs}`

**Produces (HTTP contract used by Task 5).** The server binds `0.0.0.0:$PORT` (default 8080) and reads its name from `$SERVICE_NAME` (default `demo`). It dispatches on the path's final segment(s), because ferryman forwards the full path unchanged (`/api/users/slow` arrives as-is). Every response carries the header `x-served-by: $SERVICE_NAME`.

| Request (path suffix) | Response |
| --- | --- |
| exactly `/health` | 200 `ok`, or 503 `failing` while fail mode is on |
| `…/admin/fail?on=1` / `…/admin/fail?on=0` | 200, turns fail mode on/off (always reachable, even in fail mode) |
| any other path while fail mode is on | 503 `failing` |
| `…/slow?ms=N` | sleeps N ms (capped at 60000), then 200 `slow done` |
| `…/stream?chunks=N&interval_ms=M` | 200 chunked, N lines `chunk i\n`, M ms apart (N ≤ 100) |
| `…/status?code=NNN` | responds with NNN (100–599, else 400) |
| anything else (`/echo`, `/`, …) | 200 JSON `{"service","method","path","query","headers":{name:value},"body_bytes"}`; the body is drained and counted, never buffered whole |

- [ ] **Step 1:** Write unit tests in `src/main.rs` for the pure dispatch function `fn route(path: &str, query: Option<&str>, failing: bool) -> Action`, where `enum Action { Fail(bool), Failing, Health, Slow(u64), Stream { chunks: u32, interval_ms: u64 }, Status(u16), Echo, BadRequest(&'static str) }`. Precedence: `…/admin/fail` first (always reachable), then `failing` makes everything else `Failing` (including `/health`), then the table above. Cases: `/api/users/slow?ms=10` gives `Slow(10)`; `ms=999999` is capped at `Slow(60000)`; `/health` gives `Health`, or `Failing` when failing; `/x/admin/fail?on=1` gives `Fail(true)` even when failing; `/x/status?code=42` gives `BadRequest`; `/x/stream?chunks=500` gives `BadRequest`; `/api/users/x` gives `Echo`.
- [ ] **Step 2:** Run `cargo test -p demo-upstream`. Expected: FAIL (no `route`).
- [ ] **Step 3:** Implement with hyper 1 + hyper-util `TokioIo` over HTTP/1 (`hyper::server::conn::http1::Builder`), `Arc<AtomicBool>` for the fail flag, and `serde_json` for the echo response. For streaming, write a ~15-line `ChannelBody(tokio::sync::mpsc::Receiver<Bytes>)` that implements `hyper::body::Body` via `poll_recv`, with a spawned task that sends each chunk, so no new crates are needed. Response bodies use `BoxBody<Bytes, Infallible>`. Exit promptly on SIGTERM. Target about 250 lines.
- [ ] **Step 4:** Run `cargo test -p demo-upstream` (PASS). Then run `cargo run -p demo-upstream` and check `curl -s localhost:8080/x/echo`, `curl -N 'localhost:8080/x/stream?chunks=3&interval_ms=200'` and `curl -i 'localhost:8080/x/status?code=503'`.
- [ ] **Step 5:** Write the `Dockerfile` (build context = repo root): builder `rust:1-bookworm` running `cargo build --release -p demo-upstream`; runtime `gcr.io/distroless/cc-debian12:nonroot`, `EXPOSE 8080`, `ENTRYPOINT ["/demo-upstream"]`. Verify with `docker build -f examples/full-stack/demo-upstream/Dockerfile -t demo-upstream:dev .`. Commit.

### Task 3: Embedded-library example

**Files:** `examples/embedded/{Cargo.toml,README.md,config.toml,src/lib.rs,src/admin.rs,src/main.rs,tests/embedded.rs}`

**Consumes (ferryman 0.1.0 public API):** `ferryman::serve(TcpListener, SharedTable, Option<TlsAcceptor>, impl Future<Output=()>) -> anyhow::Result<()>`; `ferryman::reload::watch_config(&Path, SharedTable) -> notify::Result<RecommendedWatcher>`; `ferryman::tls::load_acceptor(&Path, &Path) -> anyhow::Result<TlsAcceptor>`; `ferryman_core::{load_config, build_table, health_loop, SharedTable, CircuitState}`; `RouteTable::{upstreams, publish_gauges}`; `Upstream::{name, uri, state()}`.

**Produces:**

```rust
pub struct Settings {
    pub config: PathBuf,
    pub proxy_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub tls: Option<(PathBuf, PathBuf)>,
}
pub struct Running {
    pub proxy_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub table: SharedTable,
    /* private: shutdown sender, join handles, watcher */
}
pub async fn start(settings: Settings, metrics: PrometheusHandle) -> anyhow::Result<Running>;
impl Running { pub async fn shutdown(self) -> anyhow::Result<()>; }
pub const LATENCY_BUCKETS: &[f64]; // same values as Task 1, duplicated on purpose
pub fn prometheus_builder(health_interval: Duration) -> anyhow::Result<PrometheusBuilder>;
```

Admin endpoints on `admin_bind`:
- `GET /healthz` returns 200 `ok` (liveness for the embedding app, which the scratch binary cannot offer).
- `GET /status` returns 200 JSON `{"upstreams":[{"name":"127.0.0.1:8001","uri":"http://127.0.0.1:8001/","state":"closed"|"open"|"half_open"}]}`.
- `GET /metrics` returns `handle.render()` with `content-type: text/plain; version=0.0.4`.
- Anything else returns 404.

- [ ] **Step 1: Write the failing test** `tests/embedded.rs`. It spawns a hyper stub upstream on `127.0.0.1:0` and writes a temp config with `failure_threshold = 1` and `default_cooldown_secs = 30` pointing at the stub. It calls `start(settings with 127.0.0.1:0 binds, PrometheusBuilder::new().build_recorder().handle())` and asserts:
  1. `GET http://{proxy}/svc/x` returns 200.
  2. `GET http://{admin}/status` contains `"state":"closed"`.
  3. After the stub is switched to dropping connections (toggle `AtomicBool`, as in `crates/server/tests/proxy.rs::spawn_toggle_stub`), a proxy request returns 502 and `/status` contains `"state":"open"`.
  4. `GET /healthz` returns 200.
  5. `running.shutdown().await` returns `Ok` within 5s.
- [ ] **Step 2:** Run `cargo test -p ferryman-embedded-example`. Expected: FAIL (missing `start`).
- [ ] **Step 3: Implement.**
  - `start` loads the config, builds the table, publishes gauges and wraps the table in `ArcSwap`.
  - It spawns `health_loop`, starts `watch_config` (keeping the watcher in `Running`), and binds both listeners (returning their `local_addr`).
  - It creates a `tokio::sync::watch::channel(false)` for shutdown, spawns `ferryman::serve(proxy_listener, table, tls, wait_for(rx.clone()))`, and spawns the admin accept loop, which `select!`s on the same signal.
  - It spawns an upkeep task calling `metrics.run_upkeep()` every 5s. `admin.rs` holds the admin service.
  - `main.rs` parses the CLI (clap: `--config`, `--bind`, `--admin-bind`, `--tls-cert`, `--tls-key`), sets up tracing, `prometheus_builder(interval)?.install_recorder()?`, calls `start`, waits for ctrl-c/SIGTERM, then calls `shutdown`.
  - Every non-obvious line gets a comment explaining *why*: this example is documentation.
- [ ] **Step 4:** Run `cargo test -p ferryman-embedded-example` three times (PASS, not flaky). Then do a manual run: `cargo run -p ferryman-embedded-example -- --config examples/embedded/config.toml` against two `echo_upstream` instances, and curl `/status` and `/metrics`.
- [ ] **Step 5:** Write `README.md` covering: what it demonstrates, how to run it, how to copy it into your project (drop `path =`), the recorder-install-once rule, the upkeep rule, and when to embed vs. run the binary. Commit.

### Task 4: `ferryman-core` guarded client example

**Files:** `crates/core/examples/guarded_client.rs`

**Consumes:** `Upstream::new(uri, cooldown, threshold)`, `try_acquire() -> Option<Admission>`, `record_success/record_failure(Admission)`, `state() -> CircuitState`.

- [ ] Write a deterministic, self-contained program (`cargo run -p ferryman-core --example guarded_client`):
  - A simulated dependency fails calls 3–8 and succeeds otherwise.
  - A `async fn guarded<T, E>(up: &Upstream, call: impl Future<Output = Result<T, E>>) -> Result<T, Guarded<E>>` helper returns `Guarded::Open` (fail fast, no call made) when `try_acquire` is `None`, and otherwise reports the outcome with the ticket.
  - The program loops 20 calls, 100ms apart, with cooldown 500ms and threshold 3. It prints `call  N  state=…  -> ok|err|fail-fast`, so the output shows closed → open (fail-fast) → half-open probe → closed.
  - A doc comment explains ticket semantics and why a late `Normal` result is ignored.
- [ ] Run it and check the transitions appear in the output. Run `cargo clippy -p ferryman-core --all-targets -- -D warnings`. Commit.

### Task 5: Full-stack Docker Compose reference + `demo.sh`

**Files:** everything under `examples/full-stack/` except `demo-upstream/`.

**Consumes:** the Task 2 HTTP contract, the Task 1 histogram series, and the repo `Dockerfile` (binary at `/usr/local/bin/ferryman`, scratch image).

**Topology (docker-compose.yml):**
- `certgen`: `alpine:3.20` runs `scripts/gen-certs.sh` into bind-mounted `./certs` if `ca.pem` is missing. The script creates a CA and a server cert with SANs `localhost, 127.0.0.1, ferryman`, sets mode 0644, and exits.
- `users`, `orders`, `orders-v2`: built from `demo-upstream/Dockerfile` (context `../..`), with env `SERVICE_NAME`.
- `ferryman`: built from the repo `Dockerfile` (context `../..`).
  - `command: ["--config","/etc/ferryman/ferryman.toml","--bind","0.0.0.0:8443","--metrics-bind","0.0.0.0:9090","--tls-cert","/certs/server.pem","--tls-key","/certs/server-key.pem"]`
  - Volumes: `./config:/etc/ferryman:ro` (a directory, which hot reload needs) and `./certs:/certs:ro`.
  - `depends_on: certgen (service_completed_successfully)`, plus the three services.
  - `stop_grace_period: 30s`.
  - Ports `${FERRYMAN_HTTPS_PORT:-8443}:8443` and `${FERRYMAN_METRICS_PORT:-9090}:9090`.
- `prometheus`: `prom/prometheus:v2.54.1`, port `${PROMETHEUS_PORT:-9091}:9090`, 5s scrape/eval, rules file.
- `grafana`: `grafana/grafana:11.2.0`, anonymous viewer on, port `${GRAFANA_PORT:-3000}:3000`, provisioned datasource + dashboard.

**`config/ferryman.toml`:**
- Globals: health 2, cooldown 5, threshold 3, timeout 2, each with a production-value comment.
- Routes:
  - `/api/users` → `http://users:8080`
  - `/account` → `http://users:8080` (shared breaker)
  - `/api/orders` → `http://orders:8080`
  - `/api/orders/v2` → `http://orders-v2:8080` with `cooldown_secs = 5` (longest match)

**`prometheus/alerts.yml`:**

```yaml
groups:
  - name: ferryman
    rules:
      - alert: FerrymanDown
        expr: up{job="ferryman"} == 0
        for: 1m
      - alert: FerrymanCircuitOpen
        expr: max by (upstream) (ferryman_circuit_state) == 1
        for: 30s
      - alert: FerrymanUpstreamDown
        expr: min by (upstream) (ferryman_upstream_alive) == 0
        for: 2m
      - alert: FerrymanHigh5xxRatio
        expr: sum(rate(ferryman_requests_total{status=~"5.."}[5m])) / clamp_min(sum(rate(ferryman_requests_total[5m])), 1e-9) > 0.05
        for: 5m
      - alert: FerrymanSlowP99
        expr: histogram_quantile(0.99, sum by (le, route) (rate(ferryman_request_duration_seconds_bucket[5m]))) > 0.5
        for: 10m
```

(Each rule has `labels.severity` and `annotations.summary`.)

**Grafana dashboard `ferryman.json` (uid `ferryman`):**
- Requests/s by route × status.
- 5xx ratio.
- p50/p99 latency by route (`histogram_quantile`).
- Circuit state per upstream (state timeline, value mappings 0/1/2 → closed/open/half-open).
- Upstream alive.
- Scrape `up`.

**`demo.sh` (bash, `set -euo pipefail`, `--ci` = non-interactive, no pauses):**
- A trap restores `config/ferryman.toml` from a backup and runs `docker compose down -v` unless `KEEP=1`.
- Helpers: `step "title"`, `expect_status URL CODE [curl args…]`, `expect_body_contains URL NEEDLE`, and `wait_until "desc" TIMEOUT cmd…`.
- All HTTPS calls use `--cacert certs/ca.pem https://localhost:${FERRYMAN_HTTPS_PORT:-8443}`.

Steps and assertions, in order:
1. `docker compose up -d --build`, then wait (≤120s) until `/api/users/echo` returns 200.
2. **Routing:**
   - `/api/users/echo` → 200 with `x-served-by: users`.
   - `/api/orders/v2/echo` → `orders-v2` (longest match); `/api/orders/echo` → `orders`.
   - `/account/echo` → `users`.
   - `/api/usersx` → 404; `/nope` → 404 `no route`.
3. **TLS + HTTP/2:** `curl --http2 -w '%{http_version}'` returns `2`; `--http1.1` returns `1.1`.
4. **Forwarded headers:** the echo JSON has `x-forwarded-proto` `https` and `x-forwarded-for`. Sending `Connection: x-secret` and `x-secret: 1` does not reach the upstream.
5. **Streaming:**
   - `/api/users/stream?chunks=5&interval_ms=400`: `time_starttransfer < 1.0` and `time_total >= 1.6`.
   - POST of 8 MiB from `/dev/urandom` to `/api/users/echo` reports `"body_bytes":8388608`.
6. **Timeout:** `/api/users/slow?ms=4000` → 504.
7. **Upstream 5xx pass-through trips the breaker:**
   - `/api/orders/admin/fail?on=1` → 200. The next `/api/orders/echo` → 503 `failing` *with* `x-served-by: orders` (passed through).
   - Within at most 5 more requests, one → 503 `upstream unavailable` *without* `x-served-by` (ferryman refused). Health probes also count failures, so the exact count races and asserting on it would be flaky.
   - `:9090/metrics` has `ferryman_circuit_state{upstream="orders:8080"} 1`.
   - Recovery: the breaker now blocks the proxied `admin/fail?on=0`, and health probes keep seeing 503. So the tour restarts the sick instance (`docker compose restart orders`, which clears its in-memory fail flag), then `wait_until` `/api/orders/echo` returns 200 within 15s (the next passing health check closes the circuit). The README explains this as the real-world move: restart or replace a failing instance, and the proxy closes the circuit on its own.
8. **Failover:** `docker compose stop orders-v2`, then `/api/orders/v2/echo` gives 502 at least once and 503 within 5 requests (the same tolerant pattern as step 7). `/api/orders/echo` stays 200 (isolation). `docker compose start orders-v2`, then `wait_until` 200 within 15s, and print the measured recovery time.
9. **Prometheus:** the target `ferryman` is `up`. A query for `ferryman_request_duration_seconds_bucket` returns series. `ALERTS{alertname="FerrymanCircuitOpen"}` was seen (poll during step 8, while orders-v2 is down: `wait_until` the alert is present in `/api/v1/alerts`).
10. **Grafana:** `/api/health` returns ok, and `/api/dashboards/uid/ferryman` returns 200.
11. **Hot reload:**
    - Write a new config to `config/.ferryman.toml.tmp` adding `/api/inventory` → `http://users:8080`, then `mv` it over `ferryman.toml`; `wait_until` `/api/inventory/echo` returns 200.
    - Write invalid TOML the same way: `/api/users/echo` is still 200, and `docker compose logs ferryman` contains `config reload failed`.
    - Restore the original.
12. **Upgrade:** `Connection: upgrade`, `Upgrade: websocket` → 501.
13. **Graceful shutdown:** start `curl …/api/users/slow?ms=1500 &` (timeout 2s > 1.5s), wait 0.3s, `docker compose kill -s SIGTERM ferryman`, wait for curl, and expect 200. Then `docker compose start ferryman` and wait until healthy.

Finish with a summary of pass counts; exit non-zero on the first failed assertion, printing the last 50 ferryman log lines.

- [ ] **Step 1:** Write all config/infra files and `demo.sh`. Run `bash -n demo.sh`, `docker compose config -q`, and `promtool check rules` (via `docker run --entrypoint promtool prom/prometheus:v2.54.1 check rules`).
- [ ] **Step 2:** Run `bash examples/full-stack/demo.sh --ci`. Expected: all steps PASS. Fix and repeat until green twice in a row.
- [ ] **Step 3:** Write `README.md`:
  - An ASCII topology diagram and a "what each file demonstrates" table.
  - Quick start (`./demo.sh`), and the URLs for Grafana and Prometheus.
  - Every tour step with the curl you can run by hand.
  - A "taking it to production" section: real values, real certs, `stop_grace_period`, directory mounts, no single-file mounts, Kubernetes ConfigMap caveat.
  - A "Using crates.io instead of this repo" Dockerfile snippet (`cargo install --locked ferryman` on `rust:1-alpine` → `scratch`).
- [ ] Commit.

### Task 6: Examples index + top-level docs

- [ ] `examples/README.md`: a table of the three examples (integration depth, when to use it, the command to run it).
- [ ] `README.md`: an "Examples" section after Quick start; the repository layout gains `examples/`.
- [ ] `CLAUDE.md`: commands (`bash examples/full-stack/demo.sh --ci`, `cargo test -p ferryman-embedded-example`) plus invariants: examples `publish = false`, examples use only public API, demo timings are shortened.
- [ ] `CHANGELOG.md` `[Unreleased]`: an *Added* entry for the examples. Commit.

### Task 7: CI

- [ ] In `.github/workflows/ci.yml`, add an `examples` job (blocking, `needs: test`) that runs `bash examples/full-stack/demo.sh --ci`, with `timeout-minutes: 20`.
- [ ] Add a step to the `test` job, "example crates are not publishable":

```bash
cargo metadata --no-deps --format-version 1 \
  | jq -e '[.packages[] | select(.manifest_path | contains("/examples/")) | .publish] | all(. == [])'
```

- [ ] Push, then watch CI until every job is green (fix and repeat otherwise). Commit.

### Task 8: Review and finish

- [ ] An Opus reviewer reviews the whole diff against this plan and the Review Focus list.
- [ ] Fix confirmed findings, re-run `demo.sh --ci` and the full cargo gate, push, and wait for CI to go green.
- [ ] `cargo clean`; `docker compose down -v`; remove built example images.
