# operations/reconciliation

## ADDED Requirements

### Requirement: An operation with no signs of life reaches a terminal state within a bounded time

An operation that has not reached a terminal status SHALL be terminated as `failed` by the reaper
when no fact about it has been observed for the configured staleness window: neither a status change
nor a progress entry newer than the window. The newest observed fact is what counts, so an operation
whose worker reports progress regularly is never harvested however long the work takes.

#### Scenario: A silent operation is failed and carries its error

WHEN an operation has stood unterminated for longer than the staleness window, with no status change
and no progress entry in that time,
THEN a reaper pass advances it to `failed` through the same transition rule every other writer uses,
records one safe error with the stable code `platform.operation.stale`, marks it retryable, sets its
termination timestamp, and appends a user-safe progress entry — so a client polling the operation or
streaming its events sees the failure without any new deployment surface.

#### Scenario: A reporting operation is never harvested

WHEN an operation's status changed long ago but a progress entry was observed within the window,
THEN the reaper leaves the operation untouched.

### Requirement: The reaper touches only what is stale, and cannot resurrect or conflict

The reaper SHALL re-verify liveness under a row lock inside the same transaction that terminates the
operation, SHALL process at most a bounded batch per pass, oldest first, and SHALL be idempotent:
a second pass over the same rows changes nothing. A report arriving after reconciliation SHALL NOT
advance the operation out of `failed`; the existing transition rule already classifies it as stale
traffic.

#### Scenario: A late report after reconciliation does not resurrect

WHEN a worker publishes a progress report for an operation the reaper already failed,
THEN the projection records the report as ordinary stale traffic, and the operation remains `failed`
with its original error record intact.

#### Scenario: Two passes do not double-terminate

WHEN the reaper runs twice against the same database state,
THEN the first pass terminates each stale operation once, and the second pass reports nothing to do.

#### Scenario: A bounded pass on a large backlog

WHEN more operations are stale than one pass may claim,
THEN the pass terminates at most its batch, oldest first, and the remainder are harvested by later
passes rather than by one unbounded statement.

### Requirement: The window is configuration, refused at zero, and the decision is counted

The staleness window SHALL be set by `RATATOSKR__OPERATIONS__STALE_AFTER_SECONDS` with a documented
default; a value outside the validated range SHALL refuse startup, and zero SHALL NOT mean disabled.
Each termination the reaper performs SHALL increment
`platform_operations_reconciled_total`, whose name is pinned in the documented instrument set.

#### Scenario: An absurd window refuses startup

WHEN the environment sets the staleness window below the validated floor,
THEN the process exits with the configuration failure report naming the key and the rule, before any
listener opens.

#### Scenario: Reconciliations are visible on the metrics surface

WHEN a reaper pass terminates operations,
THEN `platform_operations_reconciled_total` has advanced by exactly that count, and the metric name
is a member of the documented instrument set.
