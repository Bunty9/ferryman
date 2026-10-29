//! Compile test: pins the public `unstable-hooks` surface. If this stops
//! compiling, the hook API changed; that is allowed only in a proxy minor.
//!
//! `PreDispatch` has no body field and its head fields are shared
//! references: a hook cannot drain the body or rewrite the URI, `Host` or
//! `Upgrade` (checked by type, so there is nothing to test at runtime).

use ferryman_core::{TrustedProxies, XffMode};
use ferryman_proxy::{
    AssertedHeaders, BoxFuture, ConnCtx, HeaderPolicy, Hooks, Policy, PreDispatch, ResponseBody,
    StampCtx, VerdictPolicy,
};
use http::{Extensions, HeaderMap, HeaderName, HeaderValue, Method, Uri, Version};
use hyper::Response;
use std::sync::Arc;

struct Dummy;

impl Hooks for Dummy {
    fn pre_dispatch<'a>(
        &'a self,
        req: PreDispatch<'a>,
        ctx: &'a ConnCtx,
    ) -> BoxFuture<'a, Result<(), Response<ResponseBody>>> {
        let _: &Method = req.method;
        let _: &Uri = req.uri;
        let _: Version = req.version;
        let _: &HeaderMap = req.headers;
        let _: &mut Extensions = req.extensions;
        let _ = ctx;
        Box::pin(async { Ok(()) })
    }

    fn stamp(&self, cx: &StampCtx<'_>, headers: &mut AssertedHeaders<'_>) {
        let _: &http::Extensions = cx.extensions;
        let _: &ConnCtx = cx.conn;
        let _: Option<&str> = cx.client_host;
        let accepted: bool = headers.insert(
            &HeaderName::from_static("x-tenant"),
            HeaderValue::from_static("t"),
        );
        let _ = accepted;
    }
}

#[test]
fn policy_fields_compile() {
    let mut p = Policy::default();
    p.hooks = Some(Arc::new(Dummy));
    p.verdict = VerdictPolicy::HeadOnly;
    p.max_request_body_bytes = Some(1);
    let mut h = HeaderPolicy::default();
    h.xff = Some(XffMode::Append);
    h.trusted_proxies = Some(TrustedProxies::default());
    h.forwarded_header = Some(true);
    h.asserted = vec![HeaderName::from_static("x-tenant")];
    p.headers = h;
    let _ = ferryman_proxy::full_body("x");
    assert!(p.hooks.is_some());
}
