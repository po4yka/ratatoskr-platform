//! The operation transition table, transcribed from ADR-0002.
//!
//! `ratatoskr-contracts` declines to publish one because a transition table is a business workflow.
//! Platform owns the rule, and ADR-0002 fixed its address, its shape and its semantics at milestone
//! 1 so that this milestone transcribes an accepted decision instead of inventing one under schema
//! pressure.
//!
//! The four outcomes matter and a boolean would lose them. Under at-least-once delivery
//! (`ARCHITECTURE.md` S19 invariant 7) a duplicate and a late-arriving older status are NORMAL
//! traffic, not errors; only two producers claiming different terminal outcomes is a defect.

use ratatoskr_operation_contracts::OperationStatus;

/// What to do with an incoming status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transition {
    /// Legal. The projection must apply the carried status.
    Advance(OperationStatus),
    /// The identical status re-delivered. A no-op plus a counter — this is what makes at-least-once
    /// redelivery idempotent.
    Duplicate,
    /// An older status after a newer one. Ignored plus a counter, NOT an error: a late `running`
    /// after `succeeded` is ordinary under at-least-once delivery.
    Stale,
    /// Two different terminal statuses. Rejected and alarmed: two producers disagreeing about the
    /// outcome is a real defect that must not be silently absorbed.
    Conflict,
}

/// Decide what an incoming status means for an operation currently in `current`.
///
/// Pure: no `async`, no database, no I/O. Exhaustively testable over all 49 status pairs.
///
/// Skips are legal. At-least-once delivery loses and reorders messages, so `accepted -> running` and
/// `queued -> succeeded` must both advance.
#[must_use]
pub fn apply(current: OperationStatus, incoming: OperationStatus) -> Transition {
    if same(current, incoming) {
        return Transition::Duplicate;
    }

    match rank(incoming).cmp(&rank(current)) {
        core::cmp::Ordering::Greater => Transition::Advance(incoming),
        core::cmp::Ordering::Less => Transition::Stale,
        // Equal rank and different statuses. Only rank 3 has more than one member, so this is two
        // different terminal outcomes.
        core::cmp::Ordering::Equal => Transition::Conflict,
    }
}

/// Whether an operation in this status has finished, however it finished.
#[must_use]
pub const fn is_terminal(status: OperationStatus) -> bool {
    rank(status) == TERMINAL_RANK
}

/// The rank of the terminal statuses, which they share.
const TERMINAL_RANK: u8 = 3;

/// The lifecycle rank, mirroring `operations.status_rank` in SQL and the table in ADR-0002.
///
/// The four terminal statuses share the top rank rather than being ordered, so no reading of this
/// code implies that `failed` precedes `cancelled`.
#[must_use]
pub const fn rank(status: OperationStatus) -> u8 {
    match status {
        OperationStatus::Accepted => 0,
        OperationStatus::Queued => 1,
        OperationStatus::Running => 2,
        OperationStatus::Succeeded
        | OperationStatus::PartiallySucceeded
        | OperationStatus::Failed
        | OperationStatus::Cancelled => TERMINAL_RANK,
        // `OperationStatus` is `#[non_exhaustive]`, so a later contracts release may add a variant
        // this binary predates. Ranked above every terminal status, so an unknown status neither
        // advances into nor out of anything: `apply` reports `Stale` for it rather than guessing.
        _ => u8::MAX,
    }
}

/// A total, stable discriminant. Distinct from [`rank`], which collapses the terminal statuses.
const fn discriminant(status: OperationStatus) -> u8 {
    match status {
        OperationStatus::Accepted => 0,
        OperationStatus::Queued => 1,
        OperationStatus::Running => 2,
        OperationStatus::Succeeded => 3,
        OperationStatus::PartiallySucceeded => 4,
        OperationStatus::Failed => 5,
        OperationStatus::Cancelled => 6,
        _ => u8::MAX,
    }
}

const fn same(left: OperationStatus, right: OperationStatus) -> bool {
    discriminant(left) == discriminant(right)
}

/// Every status, in lifecycle order.
pub const ALL: [OperationStatus; 7] = [
    OperationStatus::Accepted,
    OperationStatus::Queued,
    OperationStatus::Running,
    OperationStatus::Succeeded,
    OperationStatus::PartiallySucceeded,
    OperationStatus::Failed,
    OperationStatus::Cancelled,
];

#[cfg(test)]
mod tests {
    use super::{ALL, Transition, apply, is_terminal, rank};
    use ratatoskr_operation_contracts::OperationStatus;

    /// T-1. All 49 pairs are classified exactly as ADR-0002's rank rule says.
    #[test]
    fn every_pair_matches_the_rank_rule() {
        for current in ALL {
            for incoming in ALL {
                let outcome = apply(current, incoming);
                let expected = if super::same(current, incoming) {
                    Transition::Duplicate
                } else if rank(incoming) > rank(current) {
                    Transition::Advance(incoming)
                } else if rank(incoming) < rank(current) {
                    Transition::Stale
                } else {
                    Transition::Conflict
                };
                assert_eq!(outcome, expected, "{current:?} -> {incoming:?}");
            }
        }
    }

    /// T-2. A skip advances. Losing a message must not stall an operation.
    #[test]
    fn a_skipped_status_still_advances() {
        assert_eq!(
            apply(OperationStatus::Accepted, OperationStatus::Running),
            Transition::Advance(OperationStatus::Running)
        );
        assert_eq!(
            apply(OperationStatus::Queued, OperationStatus::Succeeded),
            Transition::Advance(OperationStatus::Succeeded)
        );
    }

    /// T-3. A late older status is ordinary traffic, not a failure.
    #[test]
    fn a_late_running_after_a_terminal_status_is_stale_not_an_error() {
        assert_eq!(
            apply(OperationStatus::Succeeded, OperationStatus::Running),
            Transition::Stale
        );
    }

    /// T-4. Two different terminal outcomes are a conflict, and nothing else is.
    #[test]
    fn only_two_different_terminal_outcomes_conflict() {
        for current in ALL {
            for incoming in ALL {
                if apply(current, incoming) == Transition::Conflict {
                    assert!(
                        is_terminal(current) && is_terminal(incoming),
                        "{current:?} -> {incoming:?} was reported as a conflict but is not two \
                         terminal outcomes"
                    );
                }
            }
        }
        assert_eq!(
            apply(OperationStatus::Succeeded, OperationStatus::Failed),
            Transition::Conflict
        );
    }
}
