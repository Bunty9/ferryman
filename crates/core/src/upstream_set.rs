//! A route's set of endpoints and the load-balancing policy that picks one.
//!
//! The breaker stays per `host:port` (an [`Upstream`] shared by every route
//! naming it); the set only decides the ORDER in which endpoints are offered
//! to [`Upstream::try_acquire`]. [`UpstreamSet::acquire`] calls it at most
//! once per endpoint and stops at the first that admits, so a refusing
//! endpoint is skipped and "every endpoint refuses" is `None` (the caller
//! answers 503; never another route).

use crate::route::{Admission, Upstream};
use crate::Error;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Weight range accepted by [`UpstreamSet::new`] and the `weight` config key.
pub const MAX_WEIGHT: u32 = 1000;

/// How a route picks among its endpoints. Config key `lb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LbPolicy {
    /// Smooth weighted round robin (nginx style).
    #[default]
    RoundRobin,
    /// Fewest in-flight requests; ties rotate round-robin. Weights ignored.
    LeastConn,
    /// Power of two choices: two distinct endpoints drawn by weight, the one
    /// with fewer in-flight requests wins.
    P2c,
}

/// One endpoint of a route: the shared upstream (and its breaker) plus this
/// route's weight for it.
#[derive(Clone)]
#[non_exhaustive]
pub struct Endpoint {
    pub upstream: Upstream,
    /// `1..=`[`MAX_WEIGHT`].
    pub weight: u32,
}

impl Endpoint {
    pub fn new(upstream: Upstream, weight: u32) -> Self {
        Self { upstream, weight }
    }
}

/// The endpoints of one route plus selection state (round-robin credits and
/// cursor, rng). A rebuilt table gets a fresh set; the `Arc<Breaker>`s inside
/// the endpoints are what survive a reload.
pub struct UpstreamSet {
    endpoints: Vec<Endpoint>,
    policy: LbPolicy,
    total_weight: i64,
    /// Smooth-WRR current weights.
    wrr: Mutex<Vec<i64>>,
    /// Least-conn tie rotation.
    cursor: AtomicUsize,
    rng: AtomicU64,
}

impl UpstreamSet {
    /// A one-endpoint set (the `upstream = "..."` config form).
    pub fn single(upstream: Upstream) -> Self {
        Self::new(vec![Endpoint::new(upstream, 1)], LbPolicy::default())
            .expect("one endpoint of weight 1 is valid")
    }

    /// Fails with [`Error::InvalidConfig`] on an empty list or a weight
    /// outside `1..=1000`. Duplicate endpoints are not checked here
    /// (`build_table` rejects them).
    pub fn new(endpoints: Vec<Endpoint>, policy: LbPolicy) -> Result<Self, Error> {
        let bad = |reason: &str| Error::InvalidConfig {
            key: "upstreams",
            reason: reason.into(),
        };
        if endpoints.is_empty() {
            return Err(bad("must not be empty"));
        }
        if endpoints
            .iter()
            .any(|e| !(1..=MAX_WEIGHT).contains(&e.weight))
        {
            return Err(bad("weights must be 1..=1000"));
        }
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Ok(Self {
            total_weight: endpoints.iter().map(|e| i64::from(e.weight)).sum(),
            wrr: Mutex::new(vec![0; endpoints.len()]),
            endpoints,
            policy,
            cursor: AtomicUsize::new(0),
            rng: AtomicU64::new(seed),
        })
    }

    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    pub fn policy(&self) -> LbPolicy {
        self.policy
    }

    /// The first endpoint (the only one for `upstream = "..."` routes).
    pub fn first(&self) -> &Upstream {
        &self.endpoints[0].upstream
    }

    /// Endpoints in configured order.
    pub fn upstreams(&self) -> impl Iterator<Item = &Upstream> {
        self.endpoints.iter().map(|e| &e.upstream)
    }

    /// `upstream` metric label for events with no chosen endpoint (all
    /// refused, rejected before selection): the endpoint's `host:port` for a
    /// one-endpoint set, else `"multiple"`. Config-bounded either way.
    pub fn label(&self) -> &str {
        if self.endpoints.len() == 1 {
            &self.endpoints[0].upstream.name
        } else {
            "multiple"
        }
    }

    /// Pick an endpoint whose breaker admits. Offers endpoints in policy
    /// order, calling `try_acquire` at most once each; the returned ticket
    /// belongs to the returned endpoint's breaker. `None`: every endpoint
    /// refused.
    pub fn acquire(&self) -> Option<(&Upstream, Admission)> {
        if self.endpoints.len() == 1 {
            let u = self.first();
            return u.try_acquire().map(|a| (u, a));
        }
        self.order().into_iter().find_map(|i| {
            let u = &self.endpoints[i].upstream;
            u.try_acquire().map(|a| (u, a))
        })
    }

    /// Endpoint indices, most preferred first. Advances policy state once.
    fn order(&self) -> Vec<usize> {
        let n = self.endpoints.len();
        let mut ord: Vec<usize> = (0..n).collect();
        match self.policy {
            LbPolicy::RoundRobin => {
                let mut cur = self.wrr.lock().unwrap_or_else(|p| p.into_inner());
                for (c, e) in cur.iter_mut().zip(&self.endpoints) {
                    *c += i64::from(e.weight);
                }
                // Stable: equal credit keeps configured order.
                ord.sort_by_key(|&i| std::cmp::Reverse(cur[i]));
                // Debit the preferred endpoint even if it then refuses, so a
                // dead endpoint can't bank credit and burst on recovery.
                cur[ord[0]] -= self.total_weight;
            }
            LbPolicy::LeastConn => {
                let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
                ord.sort_by_key(|&i| (self.endpoints[i].upstream.inflight(), (i + n - start) % n));
            }
            LbPolicy::P2c => {
                let a = self.weighted_pick(None);
                let b = self.weighted_pick(Some(a));
                let (first, second) = if self.endpoints[b].upstream.inflight()
                    < self.endpoints[a].upstream.inflight()
                {
                    (b, a)
                } else {
                    (a, b)
                };
                ord.retain(|&i| i != first && i != second);
                // Shuffle the fallback tail so a refusing pair doesn't always
                // spill onto the lowest-numbered endpoint.
                for k in (1..ord.len()).rev() {
                    let j = (self.next_rand() % (k as u64 + 1)) as usize;
                    ord.swap(k, j);
                }
                ord.insert(0, second);
                ord.insert(0, first);
            }
        }
        ord
    }

    fn next_rand(&self) -> u64 {
        let x = self.rng.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        // splitmix64 finaliser
        let x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }

    /// Weighted random index, never `skip`.
    fn weighted_pick(&self, skip: Option<usize>) -> usize {
        let x = self.next_rand();
        let skipped = skip.map_or(0, |s| i64::from(self.endpoints[s].weight));
        let mut r = (x % (self.total_weight - skipped) as u64) as i64;
        for (i, e) in self.endpoints.iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            r -= i64::from(e.weight);
            if r < 0 {
                return i;
            }
        }
        unreachable!("r < total - skipped")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BreakerConfig;
    use std::time::Duration;

    fn up(port: u16) -> Upstream {
        Upstream::new(
            format!("http://h:{port}").parse().unwrap(),
            BreakerConfig::default()
                .with_failure_threshold(1)
                .with_cooldown(Duration::from_millis(300)),
        )
        .unwrap()
    }

    fn set(ws: &[u32], policy: LbPolicy) -> UpstreamSet {
        let eps = ws
            .iter()
            .enumerate()
            .map(|(i, w)| Endpoint::new(up(1 + i as u16), *w))
            .collect();
        UpstreamSet::new(eps, policy).unwrap()
    }

    fn port(u: &Upstream) -> usize {
        u.uri.port_u16().unwrap() as usize
    }

    #[test]
    fn rejects_empty_and_bad_weights() {
        assert!(UpstreamSet::new(vec![], LbPolicy::RoundRobin).is_err());
        for w in [0, 1001] {
            let e = vec![Endpoint::new(up(1), w)];
            assert!(UpstreamSet::new(e, LbPolicy::P2c).is_err());
        }
    }

    #[test]
    fn smooth_wrr_is_exact_over_a_cycle() {
        let s = set(&[5, 1, 1], LbPolicy::RoundRobin);
        let picks: Vec<usize> = (0..7).map(|_| port(s.acquire().unwrap().0)).collect();
        // nginx's documented smooth sequence for 5/1/1.
        assert_eq!(picks, [1, 1, 2, 1, 3, 1, 1]);
        for _ in 0..5 {
            // every further cycle repeats exactly
            let mut counts = [0; 4];
            for _ in 0..7 {
                counts[port(s.acquire().unwrap().0)] += 1;
            }
            assert_eq!(counts, [0, 5, 1, 1]);
        }
    }

    #[test]
    fn least_conn_picks_min_inflight_and_rotates_ties() {
        let s = set(&[1, 1, 1], LbPolicy::LeastConn);
        let ties: Vec<usize> = (0..3).map(|_| port(s.acquire().unwrap().0)).collect();
        assert_eq!(ties, [1, 2, 3]);
        let _g1 = s.endpoints()[0].upstream.track_inflight();
        let _g1b = s.endpoints()[0].upstream.track_inflight();
        let _g2 = s.endpoints()[1].upstream.track_inflight();
        for _ in 0..5 {
            assert_eq!(port(s.acquire().unwrap().0), 3);
        }
    }

    #[test]
    fn p2c_is_weighted_and_prefers_idle() {
        // Two endpoints: p2c always sees both, so the idle one always wins.
        let s = set(&[1, 1], LbPolicy::P2c);
        let _g = s.endpoints()[0].upstream.track_inflight();
        for _ in 0..50 {
            assert_eq!(port(s.acquire().unwrap().0), 2);
        }
        // Three endpoints, all idle: ties go to the first drawn, so picks
        // follow the weights roughly (9:1:1 -> endpoint 1 well over half).
        let s = set(&[9, 1, 1], LbPolicy::P2c);
        let mut counts = [0; 4];
        for _ in 0..3000 {
            counts[port(s.acquire().unwrap().0)] += 1;
        }
        assert!(
            counts[1] > 1500 && counts[2] > 100 && counts[3] > 100,
            "{counts:?}"
        );
    }

    #[test]
    fn skips_open_endpoint_and_none_when_all_open() {
        for policy in [LbPolicy::RoundRobin, LbPolicy::LeastConn, LbPolicy::P2c] {
            let s = set(&[1, 1, 1], policy);
            let (u, a) = s.acquire().unwrap();
            let dead = port(u);
            u.record_failure(a); // threshold 1: opens
            for _ in 0..20 {
                let (u, a) = s.acquire().unwrap();
                assert_ne!(port(u), dead, "{policy:?}");
                u.record_success(a);
            }
            for e in s.endpoints() {
                let a = Admission::Normal;
                e.upstream.record_failure(a);
            }
            assert!(s.acquire().is_none(), "{policy:?}");
        }
    }
}
