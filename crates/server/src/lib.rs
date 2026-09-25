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
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as HttpAutoBuilder;
use hyper_util::server::graceful::GracefulShutdown;
use std::future::Future;
use std::time::Duration;
use tls::MaybeTlsStream;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Shared hyper client used to forward requests to upstreams. Bodies are
/// streamed straight through (`Incoming` in, `Incoming` out), no buffering.
pub type ProxyClient = Client<HttpConnector, Incoming>;

/// How long to wait for in-flight connections to finish after `shutdown`
/// resolves, before dropping them anyway.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

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
    let client: ProxyClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
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
                let table = table.clone();
                let client = client.clone();
                let tls = tls.clone();
                let watcher = graceful.watcher();
                tokio::spawn(async move {
                    let stream = match tls {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(s) => MaybeTlsStream::Tls(Box::new(s)),
                            Err(e) => {
                                tracing::debug!(?peer, ?e, "tls handshake failed");
                                return;
                            }
                        },
                        None => MaybeTlsStream::Plain(stream),
                    };
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        proxy::handle(table.clone(), client.clone(), peer, proto, req)
                    });
                    let builder = HttpAutoBuilder::new(TokioExecutor::new());
                    let conn = builder.serve_connection(io, svc);
                    if let Err(e) = watcher.watch(conn).await {
                        tracing::debug!(?peer, ?e, "connection closed with error");
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
