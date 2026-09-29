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
//! When admission is refused (`Err(Guarded::Open)`), the caller has to
//! decide what "fail fast" means for it — return a fallback or cached
//! value, respond 503 to *its own* caller, or otherwise fail immediately —
//! but never spin-retry `guarded` itself: retrying doesn't get a different
//! answer until the cooldown elapses and the single half-open probe is
//! admitted, so a tight retry loop just burns CPU for the same `Open`.
//!
//! ## Timeouts: put them inside `guarded`, not outside it
//!
//! `guarded(&up, tokio::time::timeout(d, call))` — timeout *inside* the
//! guarded future — is correct: whatever ticket `try_acquire` handed back
//! stays live for the whole `call`-or-timeout race, so a call that hangs
//! past `d` still reports a failure back to the breaker (see
//! `guarded_with_deadline` below), exactly like a call that returned `Err`
//! on its own.
//!
//! `tokio::time::timeout(d, guarded(&up, call))` — timeout *outside* — is
//! the trap: `timeout` racing `guarded`'s future doesn't cancel and wait for
//! it, it just stops polling it and drops it. If `call` was already
//! admitted (the ticket was acquired) and the timeout fires first, that
//! ticket is dropped without ever reaching `record_success`/`record_failure`
//! — the breaker never learns the call was slow, so a genuinely hung
//! upstream never trips it. Worse if the dropped ticket was the single
//! half-open [`Admission::Probe`](ferryman_core::Admission::Probe): the
//! breaker is left stuck half-open (see "Ticket semantics" above) until the
//! next cooldown elapses on its own, instead of reopening for that outage.
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

/// Outcome of a call guarded with a deadline: either it ran and failed, or
/// it didn't finish before `deadline`.
enum TimedOut<E> {
    Failed(E),
    Elapsed,
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

/// Like [`guarded`], but races `call` against `deadline` *inside* the
/// guarded future, so the ticket `try_acquire` returned is still live (and
/// gets reported) if `call` doesn't finish in time — see the module doc
/// comment's "Timeouts" section for why the timeout has to go here and not
/// around the whole `guarded(..)` call.
async fn guarded_with_deadline<T, E>(
    up: &Upstream,
    deadline: Duration,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T, Guarded<TimedOut<E>>> {
    guarded(up, async move {
        match tokio::time::timeout(deadline, call).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(TimedOut::Failed(err)),
            Err(_elapsed) => Err(TimedOut::Elapsed),
        }
    })
    .await
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

    // A second, independent breaker demonstrating `guarded_with_deadline`:
    // a call that hangs well past its deadline still reports a failure,
    // because the timeout races `call` *inside* `guarded`'s ticket instead
    // of dropping the ticket from outside it (see the module doc comment's
    // "Timeouts" section).
    println!();
    println!("-- guarded_with_deadline: a hung call still counts as a breaker failure --");
    // threshold 1: a single timeout is enough to trip it, so the breaker
    // state below actually proves the hang was recorded (not just that the
    // call returned Elapsed).
    let slow_up = Upstream::new(
        "http://slow.internal".parse().expect("valid uri"),
        Duration::from_millis(500),
        1,
    );
    let hangs_forever = async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok::<(), &'static str>(())
    };
    match guarded_with_deadline(&slow_up, Duration::from_millis(200), hangs_forever).await {
        Err(Guarded::Failed(TimedOut::Elapsed)) => {
            let state = slow_up.state();
            println!(
                "call timed out after 200ms -> recorded as a failure, breaker state={state:?}"
            );
            assert_eq!(
                state,
                CircuitState::Open,
                "the timeout must have been counted as a failure, opening the breaker"
            );
            println!("breaker is Open: the hang was counted, not silently dropped");
        }
        other => panic!(
            "expected the call to time out and record a failure, got: {}",
            match other {
                Ok(_) => "Ok",
                Err(Guarded::Open) => "Open",
                Err(Guarded::Failed(TimedOut::Failed(_))) => "Failed",
                Err(Guarded::Failed(TimedOut::Elapsed)) => unreachable!(),
            }
        ),
    }
}
