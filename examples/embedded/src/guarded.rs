//! A standalone [`Breaker`] guarding an outbound `reqwest` call, using only
//! ferryman-core's public API. The same pattern works for any fallible
//! async call; no proxy or routing table is involved.

use ferryman_core::{Breaker, BreakerConfig};
use std::time::Duration;

/// Why a guarded call produced no response body.
#[derive(Debug)]
pub enum GuardedError {
    /// Circuit open: the request was not sent.
    Open,
    /// The request was sent and failed (transport error or 5xx).
    Failed(String),
}

/// Breaker for calls to one downstream: opens after 3 consecutive failures,
/// retries after 30 s (the library defaults, spelled out).
pub fn client_breaker(name: &str) -> Result<Breaker, ferryman_core::Error> {
    Breaker::new(
        BreakerConfig::default()
            .with_failure_threshold(3)
            .with_cooldown(Duration::from_secs(30))
            .with_name(name),
    )
}

/// GET `url` only if `breaker` admits it, and report the outcome on the
/// ticket it handed back. A 5xx counts as a failure; 4xx does not.
pub async fn guarded_get(
    breaker: &Breaker,
    client: &reqwest::Client,
    url: &str,
) -> Result<String, GuardedError> {
    let Some(ticket) = breaker.try_acquire() else {
        return Err(GuardedError::Open);
    };
    let res = async {
        let resp = client.get(url).send().await?;
        let status = resp.status().as_u16();
        Ok::<_, reqwest::Error>((status, resp.text().await?))
    }
    .await;
    match res {
        Ok((status, body)) if status < 500 => {
            breaker.record_success(ticket);
            Ok(body)
        }
        Ok((status, _)) => {
            breaker.record_failure(ticket);
            Err(GuardedError::Failed(format!("status {status}")))
        }
        Err(e) => {
            breaker.record_failure(ticket);
            Err(GuardedError::Failed(e.to_string()))
        }
    }
}
