//! What identifier an occurrence gets, and when the next one is due.
//!
//! Pure functions over a clock that is passed in. `ARCHITECTURE.md` S17 lists "scheduler
//! occurrence IDs" as a **unit** test subject, and this file is what that means: no database, no
//! broker, no `Timestamp::now()`.

use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

/// The namespace every occurrence identifier is minted under.
///
/// A fixed, arbitrary UUID that exists only to separate these names from every other name-based
/// identifier anyone might mint. Changing it renames every future occurrence and silently disables
/// duplicate suppression across the change, so it is a constant and never a parameter.
const OCCURRENCE_NAMESPACE: Uuid = Uuid::from_u128(0x8c1f_4a2e_6d73_4b90_9f21_5e0c_7a48_d3b6);

/// How far behind a `catch_up` schedule may be before it stops catching up.
///
/// A foot-gun guard, not a policy. Enabling a schedule whose `next_due_at` is a year in the past
/// would otherwise publish every missed occurrence — half a million commands for a one-minute
/// interval — into a command stream that refuses a publish when it is full. Beyond this many
/// intervals the schedule jumps to the present and reports what it discarded, which is a visible
/// gap rather than an invisible flood.
const MAX_CATCH_UP_INTERVALS: i64 = 64;

/// What a schedule does about occurrences it missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUp {
    /// Publish the current occurrence and move to the next grid point after now. Correct for a
    /// snapshot: only the latest state matters, and ten stale snapshots are nine units of work
    /// whose results are already superseded.
    Skip,
    /// Advance one interval at a time, so every missed occurrence is eventually published. Correct
    /// for an incremental synchronisation, where a gap is a hole in the data rather than a delay.
    CatchUp,
}

impl CatchUp {
    /// The stored spelling, matching the `schedules_catch_up_is_known` constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::CatchUp => "catch_up",
        }
    }

    /// Read a stored value.
    ///
    /// `None` for anything else, which the caller treats as a defective row rather than guessing:
    /// the CHECK constraint makes an unknown value impossible, so seeing one means the constraint
    /// was dropped and the safe reading of that is "stop", not "assume skip".
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "skip" => Some(Self::Skip),
            "catch_up" => Some(Self::CatchUp),
            _ => None,
        }
    }
}

/// The identifier of the occurrence of `schedule_id` due at `due_at`.
///
/// Name-based (RFC 9562 version 5) rather than random, which is the whole mechanism behind
/// `ARCHITECTURE.md` S14's "Scheduler occurrences use deterministic IDs to prevent duplicate work":
/// two processes, or one process twice, compute the same identifier for the same due time, so the
/// primary key of `operations.schedule_occurrences` and the `message_id` of `operations.outbox`
/// both refuse the second copy without either of them having to coordinate.
///
/// SHA-1 is what version 5 is defined over. It is not a security control here: both inputs are
/// values this system minted, nothing is authenticated by the digest, and an adversary who could
/// choose them could write the row directly.
///
/// The name is built from microseconds because that is `timestamptz`'s own resolution. Anything
/// finer would make the identifier depend on whether the timestamp had been through the database
/// yet.
#[must_use]
pub fn occurrence_id(schedule_id: Uuid, due_at: Timestamp) -> Uuid {
    let name = format!("{schedule_id}:{}", due_at.as_microsecond());
    Uuid::new_v5(&OCCURRENCE_NAMESPACE, name.as_bytes())
}

/// Where a schedule goes after an occurrence has been handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Advance {
    /// The next due time. Always strictly after `due_at`, and — except on the catch-up path —
    /// strictly after `now`.
    pub next_due_at: Timestamp,
    /// How many grid points were passed over without being published. Zero on the ordinary path;
    /// non-zero means the process was not running, or the schedule was disabled, across them.
    pub skipped: u32,
}

/// The next due time after handling the occurrence at `due_at`.
///
/// The grid is `due_at + k * interval`, so a schedule keeps the phase its `next_due_at` was seeded
/// with: an interval of 86 400 anchored at 03:00 stays at 03:00 whether or not the process was
/// running yesterday.
#[must_use]
pub fn advance(
    due_at: Timestamp,
    interval: SignedDuration,
    now: Timestamp,
    catch_up: CatchUp,
) -> Advance {
    let interval_seconds = interval.as_secs().max(1);
    let elapsed = now.duration_since(due_at).as_secs().max(0);

    if catch_up == CatchUp::CatchUp
        && elapsed <= interval_seconds.saturating_mul(MAX_CATCH_UP_INTERVALS)
    {
        return Advance {
            next_due_at: due_at.saturating_add(interval).unwrap_or(Timestamp::MAX),
            skipped: 0,
        };
    }

    // The smallest `k` for which `due_at + k * interval` is strictly after `now`. Integer division
    // then `+ 1`, so a schedule that fired exactly on its grid point still moves a whole interval
    // forward rather than becoming due again immediately.
    let steps = elapsed / interval_seconds + 1;
    let forward = SignedDuration::from_secs(interval_seconds.saturating_mul(steps));
    Advance {
        next_due_at: due_at.saturating_add(forward).unwrap_or(Timestamp::MAX),
        skipped: u32::try_from(steps - 1).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "assertions in a test module"
    )]

    use super::{Advance, CatchUp, advance, occurrence_id};
    use jiff::{SignedDuration, Timestamp};
    use uuid::Uuid;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).expect("a timestamp inside the supported range")
    }

    const MINUTE: SignedDuration = SignedDuration::from_mins(1);

    /// P-1. The same schedule and the same due time always produce the same identifier — which is
    /// the property every duplicate-suppression guarantee below the database rests on.
    #[test]
    fn an_occurrence_identifier_is_a_function_of_the_schedule_and_the_due_time() {
        let schedule = Uuid::now_v7();
        assert_eq!(
            occurrence_id(schedule, at(1_700_000_000)),
            occurrence_id(schedule, at(1_700_000_000)),
        );
        assert_ne!(
            occurrence_id(schedule, at(1_700_000_000)),
            occurrence_id(schedule, at(1_700_000_060)),
        );
        assert_ne!(
            occurrence_id(schedule, at(1_700_000_000)),
            occurrence_id(Uuid::now_v7(), at(1_700_000_000)),
        );
    }

    /// P-2. An identifier survives a round trip through `timestamptz`, whose resolution is
    /// microseconds. A name built from nanoseconds would not.
    #[test]
    fn an_occurrence_identifier_ignores_precision_no_column_can_hold() {
        let schedule = Uuid::now_v7();
        let exact = at(1_700_000_000);
        let with_nanos = exact + SignedDuration::from_nanos(999);
        assert_eq!(
            occurrence_id(schedule, exact),
            occurrence_id(schedule, with_nanos)
        );
    }

    /// P-3. On time: one interval forward, nothing skipped, under either policy.
    #[test]
    fn a_punctual_occurrence_moves_one_interval_forward() {
        let due = at(1_700_000_000);
        for policy in [CatchUp::Skip, CatchUp::CatchUp] {
            assert_eq!(
                advance(due, MINUTE, due, policy),
                Advance {
                    next_due_at: at(1_700_000_060),
                    skipped: 0,
                },
                "{policy:?}",
            );
        }
    }

    /// P-4. `skip` discards the backlog and lands on the first grid point after now, counting what
    /// it passed over. Ten minutes late on a one-minute schedule is nine discarded occurrences and
    /// one published, not ten published.
    #[test]
    fn skip_lands_after_now_and_counts_what_it_discarded() {
        let due = at(1_700_000_000);
        let now = at(1_700_000_000 + 600);
        assert_eq!(
            advance(due, MINUTE, now, CatchUp::Skip),
            Advance {
                next_due_at: at(1_700_000_000 + 660),
                skipped: 10,
            },
        );
    }

    /// P-5. `catch_up` advances one interval at a time, so the backlog is published across
    /// successive passes rather than discarded.
    #[test]
    fn catch_up_advances_one_interval_at_a_time() {
        let due = at(1_700_000_000);
        let now = at(1_700_000_000 + 600);
        assert_eq!(
            advance(due, MINUTE, now, CatchUp::CatchUp),
            Advance {
                next_due_at: at(1_700_000_060),
                skipped: 0,
            },
        );
    }

    /// P-6. The guard: a `catch_up` schedule enabled with a due time far in the past jumps to the
    /// present instead of publishing the whole backlog. Without this, enabling one row is a way to
    /// fill the command stream.
    #[test]
    fn catch_up_stops_catching_up_once_the_backlog_is_absurd() {
        let due = at(1_700_000_000);
        let a_year_later = at(1_700_000_000 + 365 * 24 * 3600);
        let advanced = advance(due, MINUTE, a_year_later, CatchUp::CatchUp);
        assert!(advanced.next_due_at > a_year_later, "{advanced:?}");
        assert!(advanced.skipped > 500_000, "{advanced:?}");
    }

    /// P-7. Every path moves the schedule strictly forward. A policy that could return the due time
    /// it was given would make the publisher spin on one row forever.
    #[test]
    fn every_policy_moves_the_schedule_strictly_forward() {
        let due = at(1_700_000_000);
        for late in [0_i64, 1, 59, 60, 61, 100_000] {
            for policy in [CatchUp::Skip, CatchUp::CatchUp] {
                let advanced = advance(due, MINUTE, due + SignedDuration::from_secs(late), policy);
                assert!(advanced.next_due_at > due, "{late} {policy:?} {advanced:?}");
            }
        }
    }
}
