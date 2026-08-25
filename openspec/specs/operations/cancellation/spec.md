# operations/cancellation Specification

## Purpose
An authenticated owner can ask Platform to stop one of their own non-terminal operations, receive truthful idempotent answers for operations already finished, and set cooperative downstream stop in motion through the event bus; Platform itself never claims an outcome the owning service did not report.

## Requirements

### Requirement: Cancelling a pending operation records a request, not an outcome

The Edge API SHALL expose `POST /v1/operations/{operation_id}/cancel` to authenticated sessions. Against an operation in `accepted`, `queued`, or `running` state owned by the caller, it SHALL record that cancellation was requested and SHALL publish exactly one cancellation command onto the bus grammar's command class naming the operation. The response SHALL be an acceptance carrying the operation's current projection, whose status remains whatever the work has actually reached — the operation reaches `cancelled` only when its owning service reports so through the existing progress contract.

#### Scenario: first cancellation of a running operation

- **WHEN** the owner cancels their own running operation
- **THEN** the answer accepts the request, carries the operation snapshot with status still `running`, and one cancellation command naming that operation is durably queued for delivery

#### Scenario: repeated cancellation while still pending

- **WHEN** the owner cancels the same pending operation again before any terminal report arrives
- **THEN** the answer is acceptance again with the current truth, and no second cancellation command is queued

#### Scenario: concurrent cancellations queue one command

- **WHEN** two cancel calls for the same pending operation race
- **THEN** both answers are acceptances or truthful states, and the durable queue holds exactly one cancellation command for that operation

### Requirement: Terminal operations answer with current truth

Against an operation already in `succeeded`, `partially_succeeded`, `failed`, or `cancelled`, the endpoint SHALL return the current snapshot unchanged without recording a request or publishing a command. Cancellation of a finished operation is never an error.

#### Scenario: cancelling a succeeded operation

- **WHEN** the owner cancels an operation that already reached `succeeded`
- **THEN** the answer carries the succeeded snapshot, no cancellation is recorded, and no command is published

#### Scenario: cancelling an already-cancelled operation

- **WHEN** the owner cancels an operation whose owning service already reported `cancelled`
- **THEN** the answer carries the cancelled snapshot as plain truth, not a conflict

### Requirement: Ownership is enforced before existence

The endpoint SHALL act only on operations owned by the authenticated user. Another user's operation SHALL be indistinguishable from a missing one, and unauthenticated calls receive the standard refusal.

#### Scenario: another user's operation

- **WHEN** a user cancels an operation identifier owned by a different user
- **THEN** the answer is the standard not-found refusal envelope, identical to the answer for a nonexistent identifier, and nothing is recorded or published

#### Scenario: unauthenticated call

- **WHEN** the cancel endpoint is called without valid session credentials
- **THEN** the answer is the standard unauthenticated refusal envelope

### Requirement: The cancellation command follows the bus grammar

The published command SHALL use the platform-scoped command subject `cmd.platform.operation.cancel_requested.v1` and carry the operation identifier together with the tenant and correlation context of the original request, so any service executing the operation can recognize work it should stop cooperatively. The command rides the transactional outbox: if the database transaction rolls back, no command exists; when it commits, exactly one does.

#### Scenario: the queued command names its target

- **WHEN** a cancellation request is accepted for an operation
- **THEN** the durable outgoing message carries subject `cmd.platform.operation.cancel_requested.v1`, the operation identifier, the owner as tenant context, and a correlation identifier linking back to the original acceptance

#### Scenario: a rolled-back request leaves no trace

- **WHEN** the database transaction behind a cancellation attempt does not commit
- **THEN** neither the recorded request nor the outgoing command exists

### Requirement: Races resolve to one truthful terminal outcome

Cancellation requests MUST interleave safely with completion reports and stale-operation reconciliation. Whichever side observes an operation first decides what the other sees: a completion report that wins leaves the operation terminal so later cancels answer truth; a reconciliation pass that fails a lifeless operation does so regardless of a pending request, because claiming a service confirmed stopping when it did not would be untruthful. No interleaving SHALL produce a second command after a terminal state is reached.

#### Scenario: completion wins against cancellation

- **WHEN** an owning service reports `succeeded` concurrently with the owner's cancel call
- **THEN** the operation ends `succeeded` or `cancelled` — never both — and whichever terminal state won, a subsequent cancel returns that truth without error and without a new command

#### Scenario: reconciliation outlives a pending request

- **WHEN** an operation with a recorded cancellation request shows no sign of life past the staleness threshold
- **THEN** reconciliation terminates it as failed with the stable staleness code rather than marking it cancelled, and a later cancel answers with that failed snapshot as truth

#### Scenario: cancellation request survives a late completion

- **WHEN** the owning service reports a terminal state after a cancellation was requested but before it stopped
- **THEN** the reported terminal state stands, the recorded request remains visible in the audit trail of the operation, and no cancellation command is retroactively published
