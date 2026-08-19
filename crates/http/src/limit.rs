//! The two limits `ARCHITECTURE.md` S14 names and this repository did not have.
//!
//! S14 requires "request, body, concurrency, and per-actor limits". The request timeout and the body
//! bound have been enforced since milestone 1; concurrency and per-actor allowance are here.
//!
//! # Why both are in this process rather than in front of it
//!
//! There is nothing in front of it. `cloudflared` terminates TLS and forwards; it is not a policy
//! layer this repository configures, and the deployment target has no load balancer, no ingress
//! controller and no API gateway (ADR-0010). A limit that is not in the process does not exist.
//!
//! That also makes the in-memory state here CORRECT rather than a compromise: exactly one process
//! per role runs, so a token bucket in a `Mutex` is the whole system's view of an actor, not one
//! replica's guess at it.
//!
//! # Why not `tower`'s
//!
//! `ConcurrencyLimitLayer` makes excess requests WAIT, which on four shared cores converts a load
//! spike into a timeout for everybody instead of a refusal for some. Adding `LoadShedLayer` fixes
//! that and produces a bare 503 from outside the router, which would be the one public response
//! without an `ErrorEnvelope` — `crates/http/src/lib.rs` documents that every non-2xx from the
//! public listener carries one, and a limiter is a bad place to make that sentence false.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use platform_core::FailureKind;
use uuid::Uuid;

/// How many distinct actors a limiter tracks before it starts reclaiming.
///
/// Ten thousand on a single-household deployment is not a number a legitimate workload reaches; it
/// is the size at which the map itself would become the problem. Reclaiming prefers buckets that
/// have refilled — an actor at full allowance is one that has stopped asking — and the fallback when
/// none has is to REFUSE the new actor rather than to admit it. Failing open there would mean the
/// way to bypass the limiter is to attack it hard enough, which is the wrong way round.
const MAX_TRACKED_ACTORS: usize = 10_000;

/// One actor's allowance, as a token bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens remaining, fractional so a refill is continuous rather than stepped.
    tokens: f64,
    /// When `tokens` was last computed.
    at: jiff::Timestamp,
}

/// A per-actor request allowance.
///
/// A token bucket rather than a fixed window: a fixed window lets a caller spend the whole minute's
/// allowance in the last second of one window and again in the first second of the next, which is
/// twice the intended rate at exactly the moment the limit was supposed to bite.
#[derive(Debug)]
pub struct ActorLimiter {
    per_minute: f64,
    buckets: Mutex<HashMap<Uuid, Bucket>>,
}

impl ActorLimiter {
    /// A limiter allowing `per_minute` requests per actor, with a burst of the same size.
    ///
    /// Burst equal to the rate, because the two knobs answer the same question for this workload and
    /// a second one nobody tunes is a second one to get wrong.
    #[must_use]
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute: f64::from(per_minute.max(1)),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Spend one token for `actor`, or report that it has none.
    ///
    /// `now` is passed rather than read, so the refill is testable without sleeping.
    #[must_use]
    pub fn admit(&self, actor: Uuid, now: jiff::Timestamp) -> bool {
        let Ok(mut buckets) = self.buckets.lock() else {
            // A poisoned mutex means another thread panicked while holding it. Admitting is the
            // safe side here: the alternative is a limiter that refuses every request for the life
            // of the process because of an unrelated panic.
            tracing::error!("the rate limiter is poisoned; admitting");
            return true;
        };

        if !buckets.contains_key(&actor) && buckets.len() >= MAX_TRACKED_ACTORS {
            buckets
                .retain(|_, bucket| Self::refill(bucket, self.per_minute, now) < self.per_minute);
            if buckets.len() >= MAX_TRACKED_ACTORS {
                return false;
            }
        }

        let bucket = buckets.entry(actor).or_insert(Bucket {
            tokens: self.per_minute,
            at: now,
        });
        let tokens = Self::refill(bucket, self.per_minute, now);
        if tokens < 1.0 {
            bucket.tokens = tokens;
            bucket.at = now;
            return false;
        }
        bucket.tokens = tokens - 1.0;
        bucket.at = now;
        true
    }

    /// The bucket's contents at `now`, capped at the burst.
    #[expect(
        clippy::cast_precision_loss,
        reason = "an elapsed time in seconds; f64 is exact past any interval a process lives through"
    )]
    fn refill(bucket: &Bucket, per_minute: f64, now: jiff::Timestamp) -> f64 {
        let elapsed = now.duration_since(bucket.at).as_secs().max(0) as f64;
        (bucket.tokens + elapsed * per_minute / 60.0).min(per_minute)
    }

    /// How many actors are currently tracked. For a test, and for nothing else.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.buckets.lock().map_or(0, |buckets| buckets.len())
    }
}

/// The concurrency bound for one listener.
///
/// A counter and not a `Semaphore`, because a permit that is awaited is a queue: the whole decision
/// here is to refuse immediately instead of holding a request until it times out.
#[derive(Debug)]
pub struct Concurrency {
    in_flight: AtomicUsize,
    limit: usize,
}

impl Concurrency {
    /// A bound of `limit` in-flight requests.
    #[must_use]
    pub fn new(limit: u32) -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            limit: limit.max(1) as usize,
        }
    }
}

/// Refuse a request that would exceed the listener's concurrency bound.
///
/// Applied inside the public router, so the refusal is rendered by the same middleware that renders
/// every other one and arrives with an `ErrorEnvelope` and a correlation identifier.
///
/// The guard releases on `Drop`, which is what makes this correct for a client that disconnects
/// mid-request: no code path runs on that exit, and `Drop` does.
pub async fn shed(
    axum::extract::State(concurrency): axum::extract::State<Arc<Concurrency>>,
    request: Request,
    next: Next,
) -> Response {
    let previous = concurrency.in_flight.fetch_add(1, Ordering::AcqRel);
    let _guard = InFlight(&concurrency);
    if previous >= concurrency.limit {
        return crate::reject(FailureKind::Overloaded);
    }
    next.run(request).await
}

/// Decrements the in-flight counter however the request ends.
struct InFlight<'a>(&'a Concurrency);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "assertions in a test module")]

    use super::ActorLimiter;
    use jiff::{SignedDuration, Timestamp};
    use uuid::Uuid;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).expect("a timestamp inside the supported range")
    }

    /// L-1. An actor spends its burst and is then refused.
    #[test]
    fn an_actor_spends_its_allowance_and_is_refused() {
        let limiter = ActorLimiter::new(3);
        let actor = Uuid::now_v7();
        let now = at(1_700_000_000);

        assert!(limiter.admit(actor, now));
        assert!(limiter.admit(actor, now));
        assert!(limiter.admit(actor, now));
        assert!(
            !limiter.admit(actor, now),
            "the fourth exceeds a burst of three"
        );
    }

    /// L-2. The allowance refills continuously, so a caller that waits gets some of it back rather
    /// than all of it at a window boundary.
    #[test]
    fn the_allowance_refills_with_time() {
        let limiter = ActorLimiter::new(60);
        let actor = Uuid::now_v7();
        let start = at(1_700_000_000);
        for _ in 0..60 {
            assert!(limiter.admit(actor, start));
        }
        assert!(!limiter.admit(actor, start));

        // Sixty a minute is one a second.
        assert!(limiter.admit(actor, start + SignedDuration::from_secs(1)));
        assert!(!limiter.admit(actor, start + SignedDuration::from_secs(1)));
    }

    /// L-3. One actor's spending does not touch another's. A shared bucket would let any caller
    /// deny service to every other by spending the allowance first.
    #[test]
    fn actors_do_not_share_an_allowance() {
        let limiter = ActorLimiter::new(1);
        let now = at(1_700_000_000);
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert!(limiter.admit(first, now));
        assert!(!limiter.admit(first, now));
        assert!(
            limiter.admit(second, now),
            "a different actor has its own allowance"
        );
        assert_eq!(limiter.tracked(), 2);
    }
}
