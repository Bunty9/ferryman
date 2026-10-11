//! Access log, request id and traceparent pass-through. Sole logging test
//! binary with a global subscriber (tracing caches callsite interest), one
//! sequential test so lines are attributable by unique paths.

use ferryman_core::{build_table, ConfigToml, SharedTable};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn access_lines(buf: &Buf, needle: &str) -> Vec<String> {
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    out.lines()
        .filter(|l| l.contains("\"target\":\"ferryman::access\"") && l.contains(needle))
        .map(str::to_owned)
        .collect()
}

async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let mut h: Vec<String> = req
        .headers()
        .iter()
        .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("")))
        .collect();
    h.sort();
    let _ = req.into_body().collect().await;
    Ok(Response::new(Full::new(Bytes::from(h.join("\n")))))
}

async fn stub() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(s), service_fn(echo))
                    .await;
            });
        }
    });
    addr
}

async fn proxy(toml: String) -> SocketAddr {
    let cfg: ConfigToml = toml.parse().unwrap();
    let table: SharedTable = Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(ferryman::serve(l, table, None, std::future::pending()));
    addr
}

async fn raw(addr: SocketAddr, req: &str) -> String {
    raw_bytes(addr, req.as_bytes()).await
}

async fn raw_bytes(addr: SocketAddr, req: &[u8]) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn one_access_line_per_request_request_id_and_traceparent() {
    let buf = Buf::default();
    let w = buf.clone();
    tracing_subscriber::fmt()
        .with_writer(move || w.clone())
        .json()
        .init();

    let up = stub().await;
    let p = proxy(format!(
        "access_log = true\nrequest_id = true\nfailure_threshold = 1\n\
         [[routes]]\nprefix = \"/api\"\nupstream = \"http://{up}\"\n\
         [[routes]]\nprefix = \"/dead\"\nupstream = \"http://127.0.0.1:1\"\n"
    ))
    .await;
    let c = reqwest::Client::new();

    // 200, query omitted, id generated + sent upstream + on the response,
    // traceparent / tracestate untouched.
    let r = c
        .get(format!("http://{p}/api/ok?secret=hunter2"))
        .header(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        )
        .header("tracestate", "vendor=1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let id = r.headers()["x-request-id"].to_str().unwrap().to_owned();
    assert_eq!(id.len(), 32);
    let body = r.text().await.unwrap();
    assert!(body.contains(&format!("x-request-id: {id}")), "{body}");
    assert!(body.contains("traceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"));
    assert!(body.contains("tracestate: vendor=1"));
    let l = access_lines(&buf, "\"path\":\"/api/ok\"");
    assert_eq!(l.len(), 1, "{l:?}");
    for want in [
        "\"method\":\"GET\"",
        "\"route\":\"/api\"",
        "\"status\":200",
        &format!("\"request_id\":\"{id}\""),
        "\"peer\":\"127.0.0.1\"",
        "\"duration_ms\":",
    ] {
        assert!(l[0].contains(want), "{want} in {}", l[0]);
    }
    assert!(l[0].contains(&format!("\"upstream\":\"{up}\"")), "{}", l[0]);
    assert!(!l[0].contains("hunter2"), "query leaked: {}", l[0]);

    // A valid incoming id is kept; an over-long one is replaced.
    let r = c
        .get(format!("http://{p}/api/keep"))
        .header("x-request-id", "abc-123")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["x-request-id"], "abc-123");
    let long = "a".repeat(200);
    let r = c
        .get(format!("http://{p}/api/long"))
        .header("x-request-id", &long)
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["x-request-id"].len(), 32);

    // 404, 400 (bad path), 502 then 503 each log exactly one line.
    assert_eq!(
        c.get(format!("http://{p}/nope"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let l = access_lines(&buf, "\"path\":\"/nope\"");
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(
        l[0].contains("\"status\":404") && l[0].contains("\"route\":\"none\""),
        "{}",
        l[0]
    );

    let resp = raw(
        p,
        "GET /api/%2e%2e/x HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert!(resp.to_lowercase().contains("x-request-id:"), "{resp}");
    let l = access_lines(&buf, "\"status\":400");
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(
        l[0].contains("\"path\":\"/api/%2e%2e/x\"") && l[0].contains("\"host\":\"t\""),
        "{}",
        l[0]
    );

    assert_eq!(
        c.get(format!("http://{p}/dead/a"))
            .send()
            .await
            .unwrap()
            .status(),
        502
    );
    assert_eq!(
        c.get(format!("http://{p}/dead/b"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    let l = access_lines(&buf, "\"path\":\"/dead/a\"");
    assert!(l.len() == 1 && l[0].contains("\"status\":502"), "{l:?}");
    let l = access_lines(&buf, "\"path\":\"/dead/b\"");
    assert!(l.len() == 1 && l[0].contains("\"status\":503"), "{l:?}");

    // Duplicate ids are ambiguous: replaced.
    let resp = raw(
        p,
        "GET /api/dup HTTP/1.1\r\nHost: t\r\nx-request-id: a\r\nx-request-id: b\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        !resp.contains("x-request-id: a\r\n") && !resp.contains("x-request-id: b\r\n"),
        "{resp}"
    );

    // Bytes that could forge log lines are sanitised.
    let resp = raw_bytes(
        p,
        b"GET /api/ctl HTTP/1.1\r\nHost: ev\xffil\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    let l = access_lines(&buf, "\"path\":\"/api/ctl\"");
    assert!(l.len() == 1 && l[0].contains("\"host\":\"ev?il\""), "{l:?}");

    // 504 logs one line.
    let hang = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hang_addr = hang.local_addr().unwrap();
    tokio::spawn(async move {
        let mut keep = Vec::new();
        loop {
            keep.push(hang.accept().await.unwrap());
        }
    });
    let t = proxy(format!(
        "access_log = true\nupstream_timeout_secs = 1\n[[routes]]\nprefix = \"/slow\"\nupstream = \"http://{hang_addr}\"\n"
    ))
    .await;
    assert_eq!(
        c.get(format!("http://{t}/slow/x"))
            .send()
            .await
            .unwrap()
            .status(),
        504
    );
    let l = access_lines(&buf, "\"path\":\"/slow/x\"");
    assert!(l.len() == 1 && l[0].contains("\"status\":504"), "{l:?}");

    // request_id off: client id reaches upstream untouched but is NOT echoed.
    let q = proxy(format!(
        "access_log = true\n[[routes]]\nprefix = \"/q\"\nupstream = \"http://{up}\"\n"
    ))
    .await;
    let r = c
        .get(format!("http://{q}/q/z"))
        .header("x-request-id", "client-1")
        .send()
        .await
        .unwrap();
    assert!(r.headers().get("x-request-id").is_none());
    assert!(r.text().await.unwrap().contains("x-request-id: client-1"));
    let l = access_lines(&buf, "\"path\":\"/q/z\"");
    assert!(
        l.len() == 1 && l[0].contains("\"request_id\":\"client-1\""),
        "{l:?}"
    );

    // Default (access_log unset): silent.
    let n = proxy(format!(
        "[[routes]]\nprefix = \"/quiet\"\nupstream = \"http://{up}\"\n"
    ))
    .await;
    c.get(format!("http://{n}/quiet/z")).send().await.unwrap();
    assert!(access_lines(&buf, "/quiet/z").is_empty());
}
