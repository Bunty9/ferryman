//! Embedder hooks on the request path (`unstable-hooks`).

use crate::pipeline::ResponseBody;
use crate::ConnCtx;
use http::{HeaderMap, HeaderName, HeaderValue};
use hyper::body::Incoming;
use hyper::{Request, Response};
use std::future::Future;
use std::pin::Pin;

/// A boxed, sendable future (what [`Hooks::pre_dispatch`] returns).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Embedder code on the request path, set with `Policy::hooks`.
///
/// **Stability.** Everything behind the `unstable-hooks` feature is outside
/// semver: it changes only in `ferryman-proxy` *minor* releases (a
/// `ferryman-proxy = "0.4"` requirement with the feature must be bumped
/// together with the proxy minor). `tests/hooks_api.rs` pins the signatures.
///
/// Every method has a no-op default, so later hooks are added as further
/// defaulted methods.
///
/// # Exactly-once breaker accounting
///
/// Each admitted request owns a ticket from `try_acquire` until its single
/// breaker verdict. **No hook that can return a response may run between
/// `try_acquire` and the verdict:** an early return there would skip the
/// verdict or give a second one. So [`pre_dispatch`](Hooks::pre_dispatch)
/// (and the reserved, unpublished `pre_admission`) run *before*
/// `try_acquire`, and [`stamp`](Hooks::stamp) cannot return a response. A
/// panic in `stamp` unwinds through the ticket, whose `Drop` releases it (no
/// success or failure is recorded). Any future short-circuiting hook must
/// run before `try_acquire` or take the ticket explicitly.
pub trait Hooks: Send + Sync + 'static {
    /// Runs after the local health answer and before every check (path,
    /// host, lookup, upgrade, body cap) and before admission, on the RAW
    /// client request: unvalidated path and `Host`, hop-by-hop headers
    /// (e.g. `Proxy-Authorization`) and client copies of trust-governed or
    /// asserted headers. Trust none of them as proxy-asserted.
    ///
    /// **It must not touch the body** (only the head: headers, extensions,
    /// URI). It may insert extensions (e.g. claims) for [`stamp`](Hooks::stamp)
    /// or short-circuit with `Err(response)`: the upstream is never
    /// contacted, no ticket exists, and the response is recorded once with
    /// `route="none"`, `upstream="none"` (the `x-request-id` echo still
    /// applies). Headers it adds still pass through the strips and the
    /// forwarding policy; to assert a header upstream use `stamp`.
    fn pre_dispatch<'a>(
        &'a self,
        req: &'a mut Request<Incoming>,
        ctx: &'a ConnCtx,
    ) -> BoxFuture<'a, Result<(), Response<ResponseBody>>> {
        let _ = (req, ctx);
        Box::pin(async { Ok(()) })
    }

    /// Runs last, after the hop-by-hop strip, the non-canonical strip, the
    /// forwarding policy, `X-Forwarded-Prefix` and the removal of every
    /// `Policy.headers.asserted` name. It can set only those names, through
    /// [`AssertedHeaders`], and cannot produce a response. See the type docs
    /// for the panic behaviour.
    fn stamp(&self, cx: &StampCtx<'_>, headers: &mut AssertedHeaders<'_>) {
        let _ = (cx, headers);
    }
}

/// What [`Hooks::stamp`] can read.
#[non_exhaustive]
pub struct StampCtx<'a> {
    /// What [`Hooks::pre_dispatch`] inserted.
    pub extensions: &'a http::Extensions,
    pub conn: &'a ConnCtx,
    /// The client's effective host (URI authority without userinfo, else
    /// `Host`), captured before `rewrite_host`. Only captured when
    /// `Policy.headers.asserted` is non-empty or `forwarded_header` is on.
    pub client_host: Option<&'a str>,
}

/// Write access to the upstream request headers, limited to the names listed
/// in `Policy.headers.asserted`.
pub struct AssertedHeaders<'a> {
    headers: &'a mut HeaderMap,
    allowed: &'a [HeaderName],
}

impl<'a> AssertedHeaders<'a> {
    pub(crate) fn new(headers: &'a mut HeaderMap, allowed: &'a [HeaderName]) -> Self {
        Self { headers, allowed }
    }

    /// Sets `name` (replacing any value). Returns `false`, and changes
    /// nothing, when `name` is not in `asserted`.
    pub fn insert(&mut self, name: &HeaderName, value: HeaderValue) -> bool {
        if !self.allowed.contains(name) {
            tracing::debug!(target: "ferryman::proxy", header = %name, "stamp of an unlisted header refused");
            return false;
        }
        self.headers.insert(name.clone(), value);
        true
    }
}
