//! Optional TLS termination.
//!
//! Loads a cert/key PEM pair into a `rustls` `ServerConfig` with ALPN
//! negotiation for h2 + http/1.1, and unifies a plain `TcpStream` with a
//! TLS-terminated one behind [`MaybeTlsStream`] so `serve` can hand either
//! to the hyper auto builder without duplicating the accept loop.

use anyhow::Context;
use arc_swap::ArcSwap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskCx, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls::crypto::ring::default_provider;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// Build a `TlsAcceptor` from a cert/key PEM pair.
///
/// Only rustls's `ring` provider is compiled in (see workspace
/// `Cargo.toml`), so `ServerConfig::builder()` picks it up without an
/// explicit `install_default`.
pub fn load_acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building TLS server config")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Serves whatever [`CertifiedKey`] is currently stored; swapping it affects
/// new handshakes only (established connections keep their session keys).
#[derive(Debug)]
struct SwapResolver(ArcSwap<CertifiedKey>);

impl ResolvesServerCert for SwapResolver {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.load_full())
    }
}

fn load_certified(cert_path: &Path, key_path: &Path) -> anyhow::Result<CertifiedKey> {
    CertifiedKey::from_der(
        load_certs(cert_path)?,
        load_key(key_path)?,
        &default_provider(),
    )
    .context("building TLS certified key (cert/key mismatch or unsupported key)")
}

/// Handle that re-reads the cert/key files and swaps them into a running
/// acceptor made by [`load_reloadable`]. A bad pair is an `Err` and leaves
/// the previous certificate in place.
#[derive(Clone, Debug)]
pub struct TlsReloader {
    cert: PathBuf,
    key: PathBuf,
    resolver: Arc<SwapResolver>,
}

impl TlsReloader {
    /// The cert and key paths, for the reload watcher.
    pub fn files(&self) -> [&Path; 2] {
        [&self.cert, &self.key]
    }

    /// Re-read the files and swap the certificate used for new handshakes.
    pub fn reload(&self) -> anyhow::Result<()> {
        self.resolver
            .0
            .store(Arc::new(load_certified(&self.cert, &self.key)?));
        Ok(())
    }
}

/// Like [`load_acceptor`], but the certificate can later be replaced through
/// the returned [`TlsReloader`] (pass it to
/// [`reload::Reloader`](crate::reload::Reloader) for file-watch and SIGHUP
/// driven rotation). The acceptor is passed to [`serve`](crate::serve) as usual.
pub fn load_reloadable(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<(TlsAcceptor, TlsReloader)> {
    let resolver = Arc::new(SwapResolver(ArcSwap::from_pointee(load_certified(
        cert_path, key_path,
    )?)));
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok((
        TlsAcceptor::from(Arc::new(config)),
        TlsReloader {
            cert: cert_path.to_path_buf(),
            key: key_path.to_path_buf(),
            resolver,
        },
    ))
}

fn load_certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .with_context(|| format!("loading TLS cert {}", path.display()))
}

fn load_key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path)
        .with_context(|| format!("loading TLS key {}", path.display()))
}

/// A connection that may or may not be TLS-terminated, unified behind one
/// `AsyncRead + AsyncWrite` type so the hyper auto builder can serve either.
pub enum MaybeTlsStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskCx<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTlsStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskCx<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTlsStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MaybeTlsStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MaybeTlsStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}
