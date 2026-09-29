//! Guarding a fallible async call with `ferryman-core`'s circuit breaker,
//! without any of the proxy machinery.
//!
//! `Upstream` here isn't routing HTTP requests — it's just an `Arc`-shared
//! breaker with a label. Anything you can `.await` and get a `Result` from
//! (a database query, a gRPC call, another service's SDK) can be wrapped the
//! same way: acquire a ticket, make the call only if admitted, report the
//! ticket back.
//!
//! ## Ticket semantics
//!
//! `try_acquire` hands back an [`Admission`](ferryman_core::Admission):
//! `Normal` while the circuit is closed, or the single half-open `Probe`
//! once the cooldown after opening has elapsed. Only `Probe` results can
//! close or re-open the circuit — a `Normal` call that was admitted before
//! the breaker tripped, and finishes late (after a sibling call has already
//! opened it), is stale news: it says nothing about whether *now* is
//! different, so it's silently ignored. Without that rule, a slow request
//! racing an outage could flip a freshly-opened breaker straight back to
//! closed.
//!
//! ```bash
//! cargo run -p ferryman-core --example guarded_client
//! ```

use ferryman_core::{CircuitState, Upstream};
use std::future::Future;
use std::time::Duration;

/// Outcome of a guarded call that didn't return `Ok`.
enum Guarded<E> {
    /// The circuit was open (or mid-cooldown): the call was never made.
    Open,
    /// The call was made and failed.
    Failed(E),
}

/// Run `call` only if the breaker admits it, and report the outcome back on
/// the exact ticket `try_acquire` returned. Never calls `call` when the
/// circuit refuses admission.
async fn guarded<T, E>(
    up: &Upstream,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T, Guarded<E>> {
    let Some(ticket) = up.try_acquire() else {
        return Err(Guarded::Open);
    };
    match call.await {
        Ok(value) => {
            up.record_success(ticket);
            Ok(value)
        }
        Err(err) => {
            up.record_failure(ticket);
            Err(Guarded::Failed(err))
        }
    }
}

/// Stand-in for a flaky dependency: broken for calls 3-8, healthy otherwise.
async fn simulated_call(n: u32) -> Result<(), &'static str> {
    if (3..=8).contains(&n) {
        Err("dependency unavailable")
    } else {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    // threshold 3, cooldown 500ms vs. the 100ms call spacing below: three
    // consecutive failures (calls 3-5) open the circuit, and the first
    // try_acquire that lands 500ms+ after that gets the half-open probe.
    let up = Upstream::new(
        // The URI is only an identity/label for the breaker (the `upstream`
        // metric label and log lines) — no network I/O happens here.
        "http://inventory.internal".parse().expect("valid uri"),
        Duration::from_millis(500),
        3,
    );

    for n in 1..=20u32 {
        // Captured before the call: nothing else touches this breaker
        // concurrently, so it's exactly the state `guarded` will act on.
        let state_before = up.state();
        let result = guarded(&up, simulated_call(n)).await;
        let (state, outcome) = match &result {
            Err(Guarded::Open) => ("Open", "fail-fast"),
            // Closed circuits always admit as `Normal`, so an admitted call
            // observed while `state_before` was `Open` was the half-open
            // probe (try_acquire flips it to HalfOpen before `call` runs).
            Ok(_) if state_before == CircuitState::Open => ("HalfOpen", "ok"),
            Err(Guarded::Failed(_)) if state_before == CircuitState::Open => ("HalfOpen", "err"),
            Ok(_) => ("Closed", "ok"),
            Err(Guarded::Failed(_)) => ("Closed", "err"),
        };
        println!("call {n:2}  state={state:<8} -> {outcome}");

        if n < 20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
