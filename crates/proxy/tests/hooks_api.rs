//! Compile test: pins the public `unstable-hooks` surface. If this stops
//! compiling, the hook API changed; that is allowed only in a proxy minor.

use ferryman_proxy::{
    AssertedHeaders, BoxFuture, ConnCtx, HeaderPolicy, Hooks, Policy, ResponseBody, StampCtx,
};
use http::{HeaderName, HeaderValue};
use hyper::body::Incoming;
use hyper::{Request, Response};
use std::sync::Arc;

struct Dummy;

impl Hooks for Dummy {
    fn pre_dispatch<'a>(
        &'a self,
        req: &'a mut Request<Incoming>,
        ctx: &'a ConnCtx,
    ) -> BoxFuture<'a, Result<(), Response<ResponseBody>>> {
        let _ = (req, ctx);
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
    let mut h = HeaderPolicy::default();
    h.asserted = vec![HeaderName::from_static("x-tenant")];
    p.headers = h;
    let _ = ferryman_proxy::full_body("x");
    assert!(p.hooks.is_some());
}
