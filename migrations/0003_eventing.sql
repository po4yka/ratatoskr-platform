-- Milestone 4: the transactional outbox and the inbox.
--
-- `ARCHITECTURE.md` S5.1 puts the outbox write in the same transaction as the operation write, and
-- S8.2 requires an inbox or processed-event record on the consuming side. Both live in the
-- `operations` schema because they are the durable half of operation processing; neither is a
-- general-purpose queue and nothing outside `ratatoskr-platform-eventing` writes to them.
--
-- The conventions of 0001 and 0002 apply unchanged.

-- ---------------------------------------------------------------------------------------------
-- outbox
-- ---------------------------------------------------------------------------------------------

create table operations.outbox (
    outbox_id         uuid        primary key,
    message_id        uuid        not null,
    subject           text        not null,
    payload           jsonb       not null,
    operation_id      uuid        references operations.operations (operation_id) on delete cascade,
    enqueued_at       timestamptz not null,
    next_attempt_at   timestamptz not null,
    attempts          integer     not null default 0,
    claimed_until     timestamptz,
    claimed_by        text,
    published_at      timestamptz,
    last_error        text,
    dead_lettered_at  timestamptz,

    -- The NATS subject grammar, fixed by ADR-0005: a class token, then the contract type name whose
    -- own grammar `ratatoskr-contracts` already validates. Enforced here as well as in Rust because
    -- a subject is a security boundary: `ARCHITECTURE.md` S15 grants least-privilege publish
    -- permissions per subject, and a row that can hold an arbitrary string can hold one outside the
    -- allowlist the credential was issued for.
    constraint outbox_subject_is_a_valid_subject
        check (subject ~ '^(cmd|evt)\.[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}\.v[1-9][0-9]{0,2}$'),
    constraint outbox_attempts_is_not_negative
        check (attempts >= 0),
    constraint outbox_claim_is_whole
        check ((claimed_until is null) = (claimed_by is null)),
    constraint outbox_claimed_by_is_bounded
        check (claimed_by is null or length(claimed_by) between 1 and 120),
    -- A safe, bounded diagnostic. The publisher's error text reaches an operator through this
    -- column, so the same rule as `operation_errors.message` applies: no newline, so no stack trace.
    constraint outbox_last_error_is_a_short_safe_string
        check (last_error is null or (length(last_error) between 1 and 200 and last_error !~ '[\n\r]')),
    -- A row is published, or dead-lettered, or neither. Both at once would mean a message was
    -- delivered and also abandoned, which no reader could interpret.
    constraint outbox_is_not_both_published_and_dead_lettered
        check (published_at is null or dead_lettered_at is null),
    constraint outbox_published_at_is_not_before_enqueued_at
        check (published_at is null or published_at >= enqueued_at)
);

comment on table operations.outbox is
    'The durable hand-off between a database transaction and the bus. A command or event is written '
    'here in the SAME transaction as the state change that justifies it (ARCHITECTURE.md S5.1 step '
    '9), so the two cannot disagree: either both are committed or neither is. A publisher then moves '
    'rows to the bus at least once, which is why every consumer deduplicates.';
comment on column operations.outbox.message_id is
    'The envelope identity carried inside `payload`, lifted out so it can be indexed. It is the '
    'deduplication key a consumer stores in its inbox, and the unique index below makes enqueuing '
    'idempotent: a retried request that re-runs the same transaction cannot produce two messages.';
comment on column operations.outbox.claimed_until is
    'A lease, not a flag. A publisher that crashes mid-batch leaves rows claimed; the lease expiring '
    'is what returns them to the queue without an operator having to notice.';
comment on column operations.outbox.next_attempt_at is
    'When this row may next be claimed. Bounded exponential backoff (ARCHITECTURE.md S8.2) is '
    'expressed by moving this forward rather than by sleeping, so backoff survives a restart.';
comment on column operations.outbox.dead_lettered_at is
    'AGENTS.md: exhausted work goes to a diagnosable dead-letter path rather than being silently '
    'dropped. The row stays, with its last error and attempt count, and stops being claimable.';

create unique index outbox_message_id_key
    on operations.outbox (message_id);

-- The publisher's only query: the oldest due, unclaimed, unfinished rows. A partial index keeps it
-- proportional to the backlog rather than to the history.
create index outbox_due_idx
    on operations.outbox (next_attempt_at, enqueued_at)
    where published_at is null and dead_lettered_at is null;

create index outbox_operation_id_idx
    on operations.outbox (operation_id)
    where operation_id is not null;

-- ---------------------------------------------------------------------------------------------
-- inbox
-- ---------------------------------------------------------------------------------------------

create table operations.inbox (
    message_id    uuid        primary key,
    subject       text        not null,
    producer      text        not null,
    received_at   timestamptz not null,
    processed_at  timestamptz,
    outcome       text,

    constraint inbox_subject_is_a_valid_subject
        check (subject ~ '^(cmd|evt)\.[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}\.v[1-9][0-9]{0,2}$'),
    -- The wire producer identity, whose grammar `ratatoskr-contracts` validates. Bounded here so an
    -- unbounded value cannot arrive from a forged message (THREAT_MODEL.md, event forgery).
    constraint inbox_producer_is_bounded
        check (length(producer) between 1 and 64),
    constraint inbox_outcome_is_known
        check (outcome is null or outcome in ('applied', 'duplicate', 'stale', 'rejected')),
    constraint inbox_outcome_matches_completion
        check ((outcome is not null) = (processed_at is not null))
);

comment on table operations.inbox is
    'The processed-event record. Delivery is at-least-once (ARCHITECTURE.md S19 invariant 7), so the '
    'primary key IS the deduplication: inserting a message id that is already present is how a '
    'consumer discovers it has seen the message, in the same statement rather than in a separate '
    'read that another worker could race.';
comment on column operations.inbox.outcome is
    'What handling the message produced, in the vocabulary of the operation transition table: a '
    'duplicate and a stale delivery are ordinary traffic and are counted, not failed (ADR-0002).';

create index inbox_unprocessed_idx
    on operations.inbox (received_at)
    where processed_at is null;
