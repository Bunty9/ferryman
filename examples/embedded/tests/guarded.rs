//! A standalone `Breaker` around a real reqwest call to a local stub.

use ferryman_core::{Breaker, BreakerConfig, CircuitState};
use ferryman_embedded_example::guarded::{guarded_get, GuardedError};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn stub(status: Arc<AtomicU16>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let status = status.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let code = status.load(Ordering::Relaxed);
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {code} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\nhi"
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    url
}

#[tokio::test]
async fn breaker_guards_reqwest_call() {
    let status = Arc::new(AtomicU16::new(500));
    let url = stub(status.clone()).await;
    let client = reqwest::Client::new();
    let b = Breaker::new(
        BreakerConfig::default()
            .with_failure_threshold(2)
            .with_cooldown(Duration::from_millis(200)),
    )
    .unwrap();

    for _ in 0..2 {
        assert!(matches!(
            guarded_get(&b, &client, &url).await,
            Err(GuardedError::Failed(_))
        ));
    }
    assert_eq!(b.state(), CircuitState::Open);
    assert!(matches!(
        guarded_get(&b, &client, &url).await,
        Err(GuardedError::Open)
    ));

    status.store(200, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(guarded_get(&b, &client, &url).await.unwrap(), "hi");
    assert_eq!(b.state(), CircuitState::Closed);
}
