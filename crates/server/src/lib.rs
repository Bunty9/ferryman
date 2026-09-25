//! ferryman-server — hyper-based reverse proxy service: the per-request
//! handler ([`proxy`]), optional TLS termination ([`tls`]), and the
//! hot-reload watcher ([`reload`]).
//!
//! `main.rs` is a thin CLI wrapper around [`serve`] so integration tests can
//! drive the same accept loop on an ephemeral port.

pub mod proxy;
pub mod reload;
pub mod tls;

use ferryman_core::SharedTable;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as HttpAutoBuilder;
use hyper_util::server::graceful::GracefulShutdown;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tls::MaybeTlsStream;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Shared hyper client used to forward requests to upstreams. Bodies are
/// streamed straight through (`Incoming` in, `Incoming` out), no buffering.
pub type ProxyClient = Client<HttpConnector, Incoming>;

/// How long to wait for in-flight connections to finish after `shutdown`
/// resolves, before dropping them anyway. Kept under fly.toml's 30s
/// `kill_timeout` so the drain finishes before a SIGKILL.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(25);

/// Deadline for a client to finish the TLS handshake, to send its first
/// request, and (HTTP/1) to send each complete request head. Stops idle sockets from pinning file
/// descriptors (slowloris).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTP/2 keep-alive ping interval; a peer that doesn't answer within
/// hyper's default ping timeout (20s) is disconnected.
const H2_KEEP_ALIVE: Duration = Duration::from_secs(30);

/// Accept connections on `listener` and serve the proxy until `shutdown`
/// resolves, then stop accepting and wait (up to
/// [`GRACEFUL_SHUTDOWN_TIMEOUT`]) for in-flight connections to finish.
///
/// TLS-terminates each connection first when `tls` is `Some`; otherwise
/// serves plain HTTP. Either way, both HTTP/1 and HTTP/2 are auto-detected.
pub async fn serve(
    listener: TcpListener,
    table: SharedTable,
    tls: Option<TlsAcceptor>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let mut connector = HttpConnector::new();
    // Streamed bodies are many small writes; Nagle + delayed ACK would add
    // ~40ms stalls.
    connector.set_nodelay(true);
    let client: ProxyClient = Client::builder(TokioExecutor::new()).build(connector);
    let mut builder = HttpAutoBuilder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(H2_KEEP_ALIVE);
    let graceful = GracefulShutdown::new();
    let proto = if tls.is_some() { "https" } else { "http" };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        // e.g. EMFILE — transient, log and keep serving.
                        tracing::warn!(?e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);
                let table = table.clone();
                let client = client.clone();
                let tls = tls.clone();
                let builder = builder.clone();
                // Taken before the handshake so a connection accepted just
                // before shutdown is still drained, but the handshake itself
                // is bounded so it can't hold the drain open.
                let watcher = graceful.watcher();
                tokio::spawn(async move {
                    let stream = match tls {
                        Some(acceptor) => {
                            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(s)) => MaybeTlsStream::Tls(Box::new(s)),
                                Ok(Err(e)) => {
                                    tracing::debug!(?peer, ?e, "tls handshake failed");
                                    return;
                                }
                                Err(_) => {
                                    tracing::debug!(?peer, "tls handshake timed out");
                                    return;
                                }
                            }
                        }
                        None => MaybeTlsStream::Plain(stream),
                    };
                    let io = TokioIo::new(stream);
                    let seen_request = Arc::new(AtomicBool::new(false));
                    let seen = seen_request.clone();
                    let svc = service_fn(move |req| {
                        seen.store(true, Ordering::Relaxed);
                        proxy::handle(table.clone(), client.clone(), peer, proto, req)
                    });
                    let conn = watcher.watch(builder.serve_connection(io, svc));
                    tokio::pin!(conn);
                    // The auto builder's h1-vs-h2 sniff runs before hyper's
                    // own header timeout exists, so a client that sends a
                    // byte or two (or an h2 preface) and stalls would sit
                    // there forever. Drop connections that haven't produced
                    // a first request in time; nothing is in flight yet.
                    let first_request_deadline = async {
                        tokio::time::sleep(HANDSHAKE_TIMEOUT).await;
                        if seen_request.load(Ordering::Relaxed) {
                            std::future::pending::<()>().await;
                        }
                    };
                    tokio::select! {
                        res = &mut conn => {
                            if let Err(e) = res {
                                tracing::debug!(?peer, ?e, "connection closed with error");
                            }
                        }
                        _ = first_request_deadline => {
                            tracing::debug!(?peer, "no request before deadline; closing");
                        }
                    }
                });
            }
        }
    }

    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT) => {
            tracing::warn!("graceful shutdown timed out waiting for in-flight connections");
        }
    }
    Ok(())
}
