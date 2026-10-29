//! Per-connection context handed to [`crate::Proxy::handle`].

use rustls::pki_types::CertificateDer;
use std::net::SocketAddr;

/// Built once per connection by the accept loop; borrowed by every request on it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ConnCtx {
    pub peer: SocketAddr,
    pub local: SocketAddr,
    /// `Some` = this hop is TLS (`X-Forwarded-Proto` is `https`).
    pub tls: Option<TlsInfo>,
}

/// What the TLS layer negotiated on a connection.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct TlsInfo {
    pub alpn: Option<Vec<u8>>,
    /// Verified client chain as DER, leaf first. Empty without client auth
    /// (ferryman never requests it).
    pub client_chain: Vec<CertificateDer<'static>>,
}

impl ConnCtx {
    pub fn new(peer: SocketAddr, local: SocketAddr) -> Self {
        Self {
            peer,
            local,
            tls: None,
        }
    }

    pub fn with_tls(mut self, tls: TlsInfo) -> Self {
        self.tls = Some(tls);
        self
    }

    /// `"https"` when `tls` is set, else `"http"`.
    pub fn proto(&self) -> &'static str {
        if self.tls.is_some() {
            "https"
        } else {
            "http"
        }
    }
}

impl TlsInfo {
    pub fn new(alpn: Option<Vec<u8>>, client_chain: Vec<CertificateDer<'static>>) -> Self {
        Self { alpn, client_chain }
    }

    /// Reads ALPN and the peer chain off a finished server handshake.
    pub fn from_rustls(conn: &rustls::ServerConnection) -> Self {
        Self {
            alpn: conn.alpn_protocol().map(<[u8]>::to_vec),
            client_chain: conn
                .peer_certificates()
                .map(|c| c.iter().map(|d| d.clone().into_owned()).collect())
                .unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_follows_tls() {
        let a: SocketAddr = "1.2.3.4:5".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let plain = ConnCtx::new(a, b);
        assert_eq!((plain.peer, plain.local, plain.proto()), (a, b, "http"));
        assert!(plain.tls.is_none());
        let tls = plain.with_tls(TlsInfo::new(Some(b"h2".to_vec()), vec![]));
        assert_eq!(tls.proto(), "https");
        let t = tls.tls.unwrap();
        assert_eq!(t.alpn.as_deref(), Some(&b"h2"[..]));
        assert!(t.client_chain.is_empty());
        assert!(TlsInfo::default().alpn.is_none());
    }
}
