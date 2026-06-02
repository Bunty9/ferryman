---
title: ferryman — L7 Reverse Proxy with circuit breakers (P2)
status: draft
date: 2026-05-28
related:
    - ../../backend-cloud-roadmap.md
    - ../../projects-l3-l4.md
---

# ferryman — Design Spec

> Companion spec lifted from `projects-l3-l4.md` § "P2 — L7 Reverse Proxy
> with circuit breakers (ferryman)". Code blocks are the authoritative
> implementation reference for the scaffold; downstream phases extend, they
> do not contradict. Default stack pins live in `backend-cloud-roadmap.md`
> § 3 and are mirrored verbatim in [`Cargo.toml`](../../Cargo.toml). P4
> (ferryman-edge) layers mTLS + JWT + hot-reload on top of this base.

## 1. Problem

Typical self-hosted setups expose services on direct ports or tunnels with
no L7 in front. Real teams put L7 proxies between clients and origin:
routing, health checks, observability, TLS termination. Build a
Pingora-pattern reverse proxy small enough to read in one sitting.
**Interview pitch:** Cloudflare Pingora replaced NGINX at 40M+ rps with 70%
less CPU — show you understand why.

## 2. Architecture

```
                    +-----------------------------+
                    |    ferryman :443/:80        |
                    |  hyper 1.x + tower stack    |
                    +--+--------------------------+
                       |
                       |  1. parse req.uri.path
                       |  2. lookup in Arc<Vec<(prefix, Uri)>>
                       |     sorted DESC by prefix length
                       |  3. check upstream.alive (AtomicBool)
                       |  4. rebuild URI, forward via hyper client
                       v
       +---------------+---------+----------+
       |               |         |          |
   svc-A:8001     svc-B:8002  svc-C:8003  ...
       ^               ^         ^
       |               |         |
       +-------+-------+---------+
               |
        Background task:
        - active /health every 5s
        - circuit breaker state machine
        - on success: alive = true
        - on N consecutive fails: alive = false, cooldown=30s

Routing rules: TOML, hot-reloaded via `notify` filesystem watch + arc-swap.

Metrics endpoint /metrics:
  ferryman_requests_total{route,upstream,status}
  ferryman_request_duration_seconds{route,upstream}
  ferryman_upstream_alive{upstream}
  ferryman_circuit_state{upstream}  # closed=0, open=1, half_open=2
```

## 3. Stack

- `hyper` 1.5, `hyper-util`, `tower`, `tower-http`, `rustls` (TLS terminate).
- `arc-swap` (atomic config reload), `parking_lot`, `notify` (FS watch).
- `serde` + `toml` for config.
- `metrics` + `metrics-exporter-prometheus`.

## 4. Key Rust code

### 4.1 Config + routing table (`crates/core/src/route.rs`)

```rust
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone)]
pub struct Upstream {
    pub uri: http::Uri,
    pub alive: Arc<AtomicBool>,
    pub last_failure_unix: Arc<AtomicU64>,
    pub cooldown_secs: u64,
}

impl Upstream {
    pub fn is_routable(&self) -> bool {
        if self.alive.load(Ordering::Relaxed) { return true; }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        // half-open after cooldown — let one probe through
        now.saturating_sub(self.last_failure_unix.load(Ordering::Relaxed)) >= self.cooldown_secs
    }
    pub fn mark_failed(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        self.last_failure_unix.store(now, Ordering::Relaxed);
    }
}

pub struct RouteTable {
    // Sorted DESC by prefix length so /api/v1/users beats /api
    rules: Vec<(String, Upstream)>,
}

impl RouteTable {
    pub fn lookup(&self, path: &str) -> Option<&Upstream> {
        self.rules.iter()
            .find(|(prefix, up)| path.starts_with(prefix.as_str()) && up.is_routable())
            .map(|(_, up)| up)
    }
}

pub type SharedTable = Arc<ArcSwap<RouteTable>>;
```

### 4.2 Service handler (`crates/server/src/proxy.rs`)

```rust
use hyper::{Request, Response, body::Incoming};
use hyper_util::client::legacy::Client;
use http_body_util::BodyExt;
use std::sync::atomic::Ordering;

type Body = http_body_util::Full<hyper::body::Bytes>;

pub async fn handle(
    table: SharedTable,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Body>,
    req: Request<Incoming>,
) -> Result<Response<Body>, anyhow::Error> {
    let started = std::time::Instant::now();
    let table = table.load();
    let path = req.uri().path().to_string();

    let Some(upstream) = table.lookup(&path) else {
        metrics::counter!("ferryman_requests_total", "status" => "404", "route" => "none").increment(1);
        return Ok(Response::builder().status(404).body(Body::new("no route".into()))?);
    };

    // Rebuild URI: upstream scheme+authority + req path+query
    let (mut parts, body) = req.into_parts();
    let mut up_parts = upstream.uri.clone().into_parts();
    up_parts.path_and_query = parts.uri.path_and_query().cloned();
    parts.uri = http::Uri::from_parts(up_parts)?;
    let bytes = body.collect().await?.to_bytes();
    let fwd = Request::from_parts(parts, Body::new(bytes));

    match client.request(fwd).await {
        Ok(resp) => {
            upstream.alive.store(true, Ordering::Relaxed); // recovery
            let status = resp.status().as_u16();
            let body = resp.into_body().collect().await?.to_bytes();
            metrics::histogram!("ferryman_request_duration_seconds",
                "upstream" => upstream.uri.host().unwrap_or("").to_string())
                .record(started.elapsed().as_secs_f64());
            metrics::counter!("ferryman_requests_total",
                "status" => status.to_string()).increment(1);
            Ok(Response::builder().status(status).body(Body::new(body))?)
        }
        Err(e) => {
            upstream.mark_failed();
            metrics::counter!("ferryman_requests_total", "status" => "502").increment(1);
            Ok(Response::builder().status(502).body(Body::new(format!("upstream: {e}").into()))?)
        }
    }
}
```

> **Phase-1 note:** the scaffold clones the matched `Upstream` out of the
> `Guard` returned by `ArcSwap::load` so it can be moved across an `await`
> point — clones are cheap (Arc bumps on the inner atomics).

### 4.3 Active health checker (`crates/core/src/health.rs`)

```rust
pub async fn health_loop(table: SharedTable, interval: std::time::Duration) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2)).build().unwrap();
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let table = table.load_full();
        for (_, up) in &table.rules {
            let url = format!("{}/health", up.uri);
            match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    up.alive.store(true, std::sync::atomic::Ordering::Relaxed);
                    metrics::gauge!("ferryman_upstream_alive",
                        "upstream" => up.uri.host().unwrap_or("").to_string()).set(1.0);
                }
                _ => {
                    up.mark_failed();
                    metrics::gauge!("ferryman_upstream_alive",
                        "upstream" => up.uri.host().unwrap_or("").to_string()).set(0.0);
                }
            }
        }
    }
}
```

### 4.4 Config hot-reload (`crates/server/src/reload.rs`)

```rust
use notify::{Watcher, RecursiveMode, RecommendedWatcher};
use std::path::Path;
use arc_swap::ArcSwap;

pub fn watch_config(path: &Path, table: SharedTable) -> notify::Result<RecommendedWatcher> {
    let p = path.to_path_buf();
    let mut w: RecommendedWatcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if matches!(ev.kind, notify::EventKind::Modify(_)) {
                match std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|s| toml::from_str::<ConfigToml>(&s).ok())
                    .map(build_table)
                {
                    Some(new_table) => {
                        table.store(std::sync::Arc::new(new_table));
                        tracing::info!("config reloaded");
                    }
                    None => tracing::error!("config reload failed; keeping old table"),
                }
            }
        }
    })?;
    w.watch(path, RecursiveMode::NonRecursive)?;
    Ok(w)
}
```

## 5. Deployment

- Single static binary, scratch Docker image.
- Fly.io 2-region (sin + iad) for failover demo.
- Local: deploy on your own machine or mesh, terminate existing service
  exposures behind it.

## 6. Eval / benchmarks

- `wrk2 -c 1000 -t 16 -R 50000 -d 60s https://ferryman/svc-a/echo`.
- p50 < 1 ms, p99 < 5 ms, p999 < 20 ms (over 2 upstreams).
- RSS < 20 MB at 50k rps idle.
- Failover: `kill` one upstream, watch p99 spike then recover within 1
  health interval (5 s).
- Compare numbers vs NGINX same machine — Pingora story is ~70% CPU
  reduction; aim for >50%.

## 7. Stretch to L4

This *is* the L4 jumping-off point — see **P4**. Add mTLS, JWT auth-plane,
HTTP/2 stream prioritization, cert hot-reload.

## 8. Source references

- Cloudflare Pingora blog series.
- gemini-research.md Domain 2 (lines covering RouterService + boxed_body
  tradeoff).
- `tower-http` examples.
