-- Milestone 3: the `operations` schema and the state machine.
--
-- Scope. DATA_MODEL.md lists operations, attempts, progress entries, results, safe errors,
-- idempotency records, projections, outbox and inbox under `operations.*`. This migration creates
-- only the first five: the outbox and inbox are milestone 4 and idempotency records are milestone 5,
-- and a table nothing writes to is a schema claim nothing tests.
--
-- The conventions established in 0001_identity.sql apply unchanged. One is worth restating because
-- it is visible here: DATA_MODEL.md forbids cross-schema foreign keys, so `owner_user_id` is a plain
-- `uuid` and NOT a REFERENCES identity.users. The reference is real but it is enforced by the
-- application, because enforcing it here would couple the two schemas' migration order forever.

create schema if not exists operations;

comment on schema operations is
    'Platform-owned durable record of user-visible asynchronous work: operations, their attempts, '
    'progress history, typed result references and safe errors.';

-- ---------------------------------------------------------------------------------------------
-- The status vocabulary and its ordering
-- ---------------------------------------------------------------------------------------------

-- The seven states of ARCHITECTURE S5.4, which are exactly the seven variants of
-- `ratatoskr_operation_contracts::OperationStatus`. A test asserts that this list and the Rust enum
-- agree, so the two cannot drift.
--
-- `rank` exists to express DOMAIN.md invariant 4 ("progress cannot move a terminal operation
-- backward") and ARCHITECTURE S19 invariant 10 ("terminal operation states do not regress") as a
-- comparison rather than as a table of pairs. The four terminal states share rank 3: they are
-- mutually unreachable, which the transition guard enforces separately.
create function operations.status_rank(status text) returns int
    language sql immutable strict parallel safe
as $$
    select case status
        when 'accepted'            then 0
        when 'queued'              then 1
        when 'running'             then 2
        when 'succeeded'           then 3
        when 'partially_succeeded' then 3
        when 'failed'              then 3
        when 'cancelled'           then 3
    end
$$;

comment on function operations.status_rank(text) is
    'Lifecycle rank of an operation status. Monotonically non-decreasing along every legal '
    'transition; the four terminal states share the top rank and never transition to one another.';

create function operations.status_is_terminal(status text) returns boolean
    language sql immutable strict parallel safe
as $$
    select status in ('succeeded', 'partially_succeeded', 'failed', 'cancelled')
$$;

-- ---------------------------------------------------------------------------------------------
-- operations
-- ---------------------------------------------------------------------------------------------

create table operations.operations (
    operation_id       uuid        primary key,
    owner_user_id      uuid        not null,
    kind               text        not null,
    status             text        not null,
    stage              text,
    progress_percent   smallint,
    correlation_id     text        not null,
    causation_id       text,
    idempotency_key    text,
    retryable          boolean     not null default false,
    cancellation_requested_at timestamptz,
    accepted_at        timestamptz not null,
    status_changed_at  timestamptz not null,
    terminated_at      timestamptz,

    constraint operations_status_is_known
        check (status in ('accepted', 'queued', 'running',
                          'succeeded', 'partially_succeeded', 'failed', 'cancelled')),
    constraint operations_kind_is_bounded
        check (kind ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    constraint operations_stage_is_bounded
        check (stage is null or length(stage) between 1 and 60),
    constraint operations_progress_percent_is_a_percentage
        check (progress_percent is null or progress_percent between 0 and 100),
    -- The namespaced wire form from ratatoskr-contracts. The same grammar as identity.audit_events,
    -- deliberately, so an operation and its audit trail join on one string.
    constraint operations_correlation_id_is_namespaced
        check (correlation_id ~ '^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9._~-]{1,128}$'),
    constraint operations_causation_id_is_namespaced
        check (causation_id is null or causation_id ~ '^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9._~-]{1,128}$'),
    constraint operations_idempotency_key_is_bounded
        check (idempotency_key is null or length(idempotency_key) between 1 and 255),
    constraint operations_status_changed_at_is_not_before_accepted_at
        check (status_changed_at >= accepted_at),
    -- `terminated_at` is set if and only if the status is terminal. This is what makes
    -- "is it finished?" answerable from one column without knowing the vocabulary.
    constraint operations_terminated_at_matches_terminal_status
        check (operations.status_is_terminal(status) = (terminated_at is not null)),
    constraint operations_terminated_at_is_not_before_accepted_at
        check (terminated_at is null or terminated_at >= accepted_at)
);

comment on table operations.operations is
    'The durable user-visible record of asynchronous work (DOMAIN.md, Operation). It is the source '
    'from which `ratatoskr_operation_contracts::OperationSnapshot` is projected; every column here '
    'either appears in that contract or supports a rule this schema enforces.';
comment on column operations.operations.owner_user_id is
    'References identity.users(user_id) semantically, with no foreign key: DATA_MODEL.md forbids '
    'cross-schema foreign keys. The application enforces it.';
comment on column operations.operations.retryable is
    'Whether the CLIENT may resubmit. Not an internal retry budget: an attempt count belongs to '
    'operations.operation_attempts, and conflating the two is how a user is told to retry work that '
    'the platform is already retrying.';
comment on column operations.operations.cancellation_requested_at is
    'A request, not a state. The operation reaches `cancelled` only when the owning service confirms '
    'it stopped; ARCHITECTURE S14 forbids rolling back external actions that already completed.';

create index operations_owner_user_id_idx
    on operations.operations (owner_user_id, accepted_at desc);

create index operations_live_idx
    on operations.operations (status, status_changed_at)
    where terminated_at is null;

create index operations_correlation_id_idx
    on operations.operations (correlation_id);

-- ARCHITECTURE S8.1: the idempotency key is scoped by actor, route and operation kind. The route
-- arrives with the public API at milestone 5; actor and kind are enforceable now, and a partial
-- uniqueness rule that is correct today is better than none.
create unique index operations_idempotency_key_scope_key
    on operations.operations (owner_user_id, kind, idempotency_key)
    where idempotency_key is not null;

-- ---------------------------------------------------------------------------------------------
-- The transition guard
-- ---------------------------------------------------------------------------------------------

create function operations.guard_status_transition() returns trigger
    language plpgsql
as $$
begin
    if new.status = old.status then
        -- Not a transition. Annotation of a terminal operation is permitted by DATA_MODEL.md
        -- ("terminal operation transitions are immutable except approved annotation"), so a status
        -- that did not change is never rejected here.
        return new;
    end if;

    if operations.status_is_terminal(old.status) then
        raise exception
            'operation % is terminal in status % and cannot transition to %',
            old.operation_id, old.status, new.status
            using errcode = 'check_violation';
    end if;

    if operations.status_rank(new.status) <= operations.status_rank(old.status) then
        raise exception
            'operation % cannot move backward from % to %',
            old.operation_id, old.status, new.status
            using errcode = 'check_violation';
    end if;

    if new.status_changed_at < old.status_changed_at then
        raise exception
            'operation % changed status at % which is before its previous change at %',
            old.operation_id, new.status_changed_at, old.status_changed_at
            using errcode = 'check_violation';
    end if;

    return new;
end;
$$;

comment on function operations.guard_status_transition() is
    'The durable backstop for DOMAIN.md invariant 4 and ARCHITECTURE S19 invariant 10. The '
    'authoritative transition table is `ratatoskr_platform_operations::Transition` in Rust; this '
    'trigger enforces the same rule for any writer that bypasses it, including a manual UPDATE. A '
    'test asserts the two agree, because two enforcement points that disagree are worse than one.';

create trigger operations_guard_status_transition
    before update of status, status_changed_at on operations.operations
    for each row
    execute function operations.guard_status_transition();

-- ---------------------------------------------------------------------------------------------
-- operation_attempts
-- ---------------------------------------------------------------------------------------------

create table operations.operation_attempts (
    attempt_id    uuid        primary key,
    operation_id  uuid        not null references operations.operations (operation_id) on delete cascade,
    attempt_number integer    not null,
    stage         text        not null,
    started_at    timestamptz not null,
    finished_at   timestamptz,
    outcome       text,

    constraint operation_attempts_attempt_number_is_positive
        check (attempt_number >= 1),
    constraint operation_attempts_stage_is_bounded
        check (length(stage) between 1 and 60),
    constraint operation_attempts_outcome_is_known
        check (outcome is null or outcome in ('succeeded', 'failed', 'abandoned')),
    constraint operation_attempts_finished_at_is_not_before_started_at
        check (finished_at is null or finished_at >= started_at),
    -- An outcome is recorded when, and only when, the attempt finished.
    constraint operation_attempts_outcome_matches_completion
        check ((outcome is not null) = (finished_at is not null))
);

comment on table operations.operation_attempts is
    'One execution of one operation step (DOMAIN.md, Attempt). Separate from the operation because '
    'at-least-once delivery means the same step runs more than once and the operation must stay one '
    'row (ARCHITECTURE S19 invariant 7).';

create unique index operation_attempts_operation_stage_number_key
    on operations.operation_attempts (operation_id, stage, attempt_number);

create index operation_attempts_operation_id_idx
    on operations.operation_attempts (operation_id);

-- ---------------------------------------------------------------------------------------------
-- operation_progress
-- ---------------------------------------------------------------------------------------------

create table operations.operation_progress (
    progress_id      uuid        primary key,
    operation_id     uuid        not null references operations.operations (operation_id) on delete cascade,
    observed_at      timestamptz not null,
    status           text        not null,
    stage            text,
    progress_percent smallint,
    message          text,

    constraint operation_progress_status_is_known
        check (status in ('accepted', 'queued', 'running',
                          'succeeded', 'partially_succeeded', 'failed', 'cancelled')),
    constraint operation_progress_stage_is_bounded
        check (stage is null or length(stage) between 1 and 60),
    constraint operation_progress_percent_is_a_percentage
        check (progress_percent is null or progress_percent between 0 and 100),
    -- A user-safe message only. ARCHITECTURE S5.4 calls these "user-safe messages" and S15 forbids
    -- internal detail on the public surface; the length bound is what stops a stack trace fitting.
    constraint operation_progress_message_is_a_short_safe_string
        check (message is null or (length(message) between 1 and 200 and message !~ '[\n\r]'))
);

comment on table operations.operation_progress is
    'The append-only progress history. The current status also lives denormalised on '
    'operations.operations because the public read path must not aggregate history on every poll; '
    'this table is what a client replays after an SSE reconnect (ARCHITECTURE S5.5).';

create index operation_progress_operation_observed_idx
    on operations.operation_progress (operation_id, observed_at);

-- ---------------------------------------------------------------------------------------------
-- operation_results
-- ---------------------------------------------------------------------------------------------

create table operations.operation_results (
    result_id     uuid        primary key,
    operation_id  uuid        not null references operations.operations (operation_id) on delete cascade,
    result_kind   text        not null,
    target        text        not null,
    blob_ref      text,
    recorded_at   timestamptz not null,

    -- `OperationResultRef.result_kind`: what the target IS, e.g. `content.document`. Stored rather
    -- than derived from the target's entity kind, because the two answer different questions and
    -- deriving one from the other would fabricate a contract value at projection time.
    constraint operation_results_result_kind_is_a_dotted_name
        check (result_kind ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    -- The namespaced entity reference from ratatoskr-contracts, e.g. `document:<uuid7>`.
    constraint operation_results_target_is_namespaced
        check (target ~ '^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9._~-]{1,128}$'),
    constraint operation_results_blob_ref_is_bounded
        check (blob_ref is null or length(blob_ref) between 1 and 255)
);

comment on table operations.operation_results is
    'Typed result REFERENCES, never result content. ARCHITECTURE S4.2: Platform does not own '
    'extracted documents, summaries or snapshots. A result is a pointer into the owning service.';

create index operation_results_operation_id_idx
    on operations.operation_results (operation_id);

-- ---------------------------------------------------------------------------------------------
-- operation_errors
-- ---------------------------------------------------------------------------------------------

create table operations.operation_errors (
    error_id      uuid        primary key,
    operation_id  uuid        not null references operations.operations (operation_id) on delete cascade,
    severity      text        not null,
    code          text        not null,
    message       text        not null,
    retryable     boolean     not null,
    recorded_at   timestamptz not null,

    constraint operation_errors_severity_is_known
        check (severity in ('error', 'warning')),
    -- The stable machine-readable code grammar of ratatoskr-contracts' ErrorCode.
    constraint operation_errors_code_is_a_stable_code
        check (code ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    constraint operation_errors_message_is_a_short_safe_string
        check (length(message) between 1 and 200 and message !~ '[\n\r]')
);

comment on table operations.operation_errors is
    'Safe errors and warnings attached to an operation, in the shape that projects onto '
    'ratatoskr-contracts ErrorEnvelope and WarningEnvelope. ARCHITECTURE S15 and the contracts '
    'threat model both forbid a provider response or a stack trace reaching this surface, which is '
    'why there is no details column and why message is bounded and newline-free.';
comment on column operations.operation_errors.severity is
    'ARCHITECTURE S14: a partial outcome is `partially_succeeded` with warnings, not a false '
    'success. Warnings and terminal errors therefore share a table and are distinguished here.';

create index operation_errors_operation_id_idx
    on operations.operation_errors (operation_id, recorded_at);
