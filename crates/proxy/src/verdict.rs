//! Breaker verdicts: [`VerdictPolicy`], the exactly-once [`Ticket`], the
//! response-body wrapper that gives the deferred verdict, and the
//! client-fault classifier.

use crate::pipeline::{BodyIdleTimeout, BodyState, BodyTooLarge, BodyTotalTimeout};
use ferryman_core::{Admission, InflightGuard, Upstream};
use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

/// When a normal admission's success reaches the breaker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerdictPolicy {
    /// The verdict is given at the response head (ferryman's behaviour). A
    /// failure after the head (body reset) is never recorded.
    #[default]
    HeadOnly,
    /// A `Normal` admission whose head is not 502-504 and whose body is not
    /// empty gets its verdict at the end of the response body: a clean end is
    /// a success, an upstream body error a failure (unless the client is to
    /// blame), a client hang-up no verdict. `Probe` admissions still get
    /// theirs at the head, and the body is wrapped for every response so a
    /// later body error counts as one `Normal` failure. Without the deferral
    /// a 200 head would reset the failure count on every request, so an
    /// upstream that always resets mid-body would never open the breaker.
    DeferNormalToBodyEnd,
}

/// One admitted request's breaker report, created right after
/// `Upstream::try_acquire`.
///
/// **Exactly-once invariant.** `success`, `failure` and `release` take
/// `self`, so a ticket reports at most once, and `Drop` without a verdict
/// calls `Upstream::release`, so it reports at least once: a client error
/// (400/408/413), a request future dropped mid-flight (client hung up, h2
/// reset, panic unwinding) or a response body dropped before its end all hand
/// the admission back instead of leaking a half-open probe slot. `release`
/// re-arms a probe slot at most once per cooldown and is a no-op for `Normal`.
/// The single sanctioned exception is `abandon` (the deprecated `handle`'s
/// bodied 504, kept as 0.3.0 behaved).
/// The only `Upstream::record_*` call in the request path outside a `Ticket`
/// is `WatchedBody`'s single `record_failure(Normal)` for a body error after
/// the head already gave its verdict.
///
/// The invariant holds only if no code that can return a response early runs
/// between `try_acquire` and the verdict without owning the ticket: such a
/// hook must run before `try_acquire` or take the ticket explicitly.
pub(crate) struct Ticket {
    upstream: Upstream,
    admission: Option<Admission>,
}

impl Ticket {
    pub(crate) fn new(upstream: Upstream, admission: Admission) -> Self {
        Self {
            upstream,
            admission: Some(admission),
        }
    }

    pub(crate) fn admission(&self) -> Admission {
        self.admission
            .expect("admission is only taken by a verdict")
    }

    pub(crate) fn success(mut self) {
        let a = self.admission.take();
        debug_assert!(a.is_some());
        if let Some(a) = a {
            self.upstream.record_success(a);
        }
    }

    pub(crate) fn failure(mut self) {
        let a = self.admission.take();
        debug_assert!(a.is_some());
        if let Some(a) = a {
            self.upstream.record_failure(a);
        }
    }

    /// No verdict on the upstream: hand the admission back (see `Drop`).
    pub(crate) fn release(self) {}

    /// Legacy bodied 504: no verdict and no release (0.3.0); the stale-probe
    /// rule frees the slot after a cooldown.
    pub(crate) fn abandon(mut self) {
        self.admission = None;
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(a) = self.admission.take() {
            self.upstream.release(a);
        }
    }
}

/// How a failure that the *client* caused maps to a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientFault {
    /// Upload past the cap: 413.
    TooLarge,
    /// Upload idle or total deadline: 408.
    Timeout,
    /// The client's body failed or it hung up (hyper user error): 400.
    BodyError,
}

/// Is `e`, or anything in its source chain starting at `e` itself, a failure
/// of the client's request body? Never the upstream's fault, so it must never
/// reach the breaker. (A response-body error from hyper is checked on itself:
/// `is_user` is set on that error, not on a source.)
pub(crate) fn client_fault(e: &(dyn std::error::Error + 'static)) -> Option<ClientFault> {
    let chain = || std::iter::successors(Some(e), |c| c.source());
    for c in chain() {
        if c.is::<BodyTooLarge>() {
            return Some(ClientFault::TooLarge);
        }
        if c.is::<BodyIdleTimeout>() || c.is::<BodyTotalTimeout>() {
            return Some(ClientFault::Timeout);
        }
    }
    chain()
        .any(|c| {
            c.downcast_ref::<hyper::Error>()
                .is_some_and(|h| h.is_user())
        })
        .then_some(ClientFault::BodyError)
}

/// The upstream's response body under `DeferNormalToBodyEnd`. Built at the
/// head for every forwarded response. `ticket` is `Some` only for a `Normal`
/// admission with a non-502-504 head and a non-empty body.
///
/// While it holds the ticket: a clean end is `success`; an error is `failure`
/// if blamed, else `release`; being dropped first is `release`. Without a
/// ticket (the head gave the verdict), a blamed error records one
/// `failure(Normal)` unless the head already failed (`failed`): one request,
/// one failure.
///
/// Blamed = the error is not a client fault, the request body never failed,
/// the client never stalled its upload for theta, and never stalled pulling
/// the response: hyper polls this body only when the client side can take
/// more, so a gap of at least theta between the head (or a frame) and the
/// next poll means the client paused, and an upstream with a write timeout
/// then hangs up reacting to it.
pub(crate) struct WatchedBody {
    pub(crate) inner: Incoming,
    /// In-flight count for least_conn / p2c; held until the body is dropped.
    pub(crate) _inflight: Option<InflightGuard>,
    pub(crate) upstream: Upstream,
    pub(crate) ticket: Option<Ticket>,
    /// `None` on the legacy path (no request-side tracking).
    pub(crate) request: Option<Arc<BodyState>>,
    pub(crate) stall: Duration,
    pub(crate) failed: bool,
    /// When the head or the last frame was handed on and not yet followed by
    /// another poll. Starts at the head.
    pub(crate) ready_at: Option<tokio::time::Instant>,
    /// Sticky: the client once took at least theta to pull the next frame.
    pub(crate) client_stalled_reading: bool,
}

impl Body for WatchedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        let now = tokio::time::Instant::now();
        if let Some(t) = this.ready_at.take() {
            if now - t >= this.stall {
                this.client_stalled_reading = true;
            }
        }
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(_))) => {
                this.ready_at = Some(now);
                // hyper does not poll again once a sized body reports its end.
                if this.inner.is_end_stream() {
                    if let Some(t) = this.ticket.take() {
                        t.success();
                    }
                }
            }
            Poll::Ready(None) => {
                if let Some(t) = this.ticket.take() {
                    t.success();
                }
            }
            // The upstream may answer before it has read the whole upload. If
            // the client then aborts or stalls, hyper tears the connection
            // down and surfaces a plain "connection error" here, with no
            // trace of the client's error in its source chain. So besides the
            // classifier, check whether the request body itself failed.
            Poll::Ready(Some(Err(e))) => {
                let blamed = client_fault(e).is_none()
                    && !this
                        .request
                        .as_ref()
                        .is_some_and(|r| r.client_failed.load(Ordering::Acquire))
                    && !this
                        .request
                        .as_ref()
                        .is_some_and(|r| r.stalled_during_request())
                    && !this.client_stalled_reading;
                if let Some(t) = this.ticket.take() {
                    if blamed {
                        t.failure();
                    } else {
                        t.release();
                    }
                    this.failed = true;
                } else if blamed && !this.failed {
                    this.failed = true;
                    this.upstream.record_failure(Admission::Normal);
                }
            }
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// `HeadOnly` response body: forwards `inner` and keeps the in-flight guard
/// until the body ends or is dropped (no verdict logic).
pub(crate) struct Held<B> {
    pub(crate) inner: B,
    pub(crate) _inflight: InflightGuard,
}

impl<B: Body + Unpin> Body for Held<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferryman_core::{BreakerConfig, CircuitState};

    fn upstream() -> Upstream {
        let mut cfg = BreakerConfig::default();
        cfg.failure_threshold = 1;
        cfg.cooldown = Duration::from_millis(200);
        Upstream::new("http://127.0.0.1:1".parse().unwrap(), cfg).unwrap()
    }

    /// Open the breaker and wait out the cooldown; returns the probe ticket.
    async fn probe(up: &Upstream) -> Ticket {
        let a = up.try_acquire().unwrap();
        up.record_failure(a);
        assert_eq!(up.state(), CircuitState::Open);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let a = up.try_acquire().unwrap();
        assert_eq!(a, Admission::Probe);
        Ticket::new(up.clone(), a)
    }

    #[tokio::test]
    async fn dropped_ticket_releases_the_probe_slot() {
        let up = upstream();
        let t = probe(&up).await;
        assert!(up.try_acquire().is_none(), "slot is held");
        drop(t);
        assert_eq!(up.try_acquire(), Some(Admission::Probe), "re-armed");
    }

    #[tokio::test]
    async fn explicit_release_equals_drop() {
        let up = upstream();
        probe(&up).await.release();
        assert_eq!(up.try_acquire(), Some(Admission::Probe));
    }

    #[tokio::test]
    async fn abandon_holds_the_probe_slot_until_the_cooldown_passes() {
        let up = upstream();
        probe(&up).await.abandon();
        assert!(up.try_acquire().is_none(), "abandon does not re-arm");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            up.try_acquire(),
            Some(Admission::Probe),
            "stale probe freed"
        );
    }

    #[tokio::test]
    async fn verdict_is_given_once_and_never_followed_by_a_release() {
        // success: closes; the consumed ticket's Drop must not re-arm anything.
        let up = upstream();
        probe(&up).await.success();
        assert_eq!(up.state(), CircuitState::Closed);
        // failure: reopens and restamps; Drop must not re-arm the slot.
        let up = upstream();
        probe(&up).await.failure();
        assert_eq!(up.state(), CircuitState::Open);
        assert!(up.try_acquire().is_none(), "no release after a failure");
    }

    #[test]
    fn client_fault_checks_the_error_itself_then_its_sources() {
        use std::io;
        assert_eq!(client_fault(&BodyTooLarge), Some(ClientFault::TooLarge));
        assert_eq!(client_fault(&BodyIdleTimeout), Some(ClientFault::Timeout));
        assert_eq!(client_fault(&BodyTotalTimeout), Some(ClientFault::Timeout));
        assert_eq!(client_fault(&io::Error::other("x")), None);
        #[derive(Debug)]
        struct Wrap(BodyTooLarge);
        impl std::fmt::Display for Wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("wrap")
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let wrapped = Wrap(BodyTooLarge);
        assert_eq!(client_fault(&wrapped), Some(ClientFault::TooLarge));
    }
}
