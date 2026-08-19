-- Milestone 9: the schedules `ratatoskr-scheduler` publishes from.
--
-- Two tables, both in `operations`, and the schema choice is the first decision here.
-- `docs/ARCHITECTURE.md` S4.1 recommends three schemas — `identity`, `operations`,
-- `platform_ingest` — and a fourth was considered and rejected. A schedule exists to produce an
-- operation and an outbox row, both of which live here; putting it anywhere else would make every
-- scheduler transaction a cross-schema write, which DATA_MODEL.md prohibits, and would give the
-- scheduler's database role reach into two schemas instead of one. ADR-0013 records it.
--
-- `platform_ingest` earned its own schema for the opposite reason: it holds ingress state that
-- neither `identity` nor `operations` has any claim on. A schedule has no such state.
--
-- The conventions of 0001 to 0006 apply unchanged: UUID primary keys minted by the writer, `text`
-- with a bounding CHECK rather than `varchar(n)`, `timestamptz` everywhere, and no foreign key that
-- crosses a schema boundary.

-- ---------------------------------------------------------------------------------------------
-- schedules
-- ---------------------------------------------------------------------------------------------

create table operations.schedules (
    schedule_id       uuid        primary key,
    name              text        not null,
    owner_user_id     uuid        not null,
    command_type      text        not null,
    operation_kind    text        not null,
    payload           jsonb       not null default '{}'::jsonb,
    interval_seconds  integer     not null,
    next_due_at       timestamptz not null,
    catch_up          text        not null default 'skip',
    enabled           boolean     not null default false,
    created_at        timestamptz not null,
    updated_at        timestamptz not null,
    last_published_at timestamptz,

    -- An operator-chosen name that becomes a metric label, so its grammar is the cardinality
    -- bound: no dots, no spaces, no unbounded length. `ARCHITECTURE.md` S16 requires scheduler
    -- drift as a signal, and a label whose values are rows in this table is bounded by how many
    -- rows an operator writes.
    constraint schedules_name_is_a_label
        check (name ~ '^[a-z][a-z0-9_-]{0,63}$'),
    -- The contract type name, WITH its version suffix: it is published as `cmd.<command_type>`,
    -- and `operations.outbox.subject` enforces exactly this grammar one table over. Checked here
    -- as well so a row that could never be published is refused where it is written, not where it
    -- is read.
    constraint schedules_command_type_is_a_contract_type
        check (command_type ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}\.v[1-9][0-9]{0,2}$'),
    -- The type, never the subject. `cmd.` is a legal first segment of the grammar above, so
    -- `cmd.github.sync.requested.v1` — the string an operator copies out of `operations.outbox`
    -- when writing their first schedule — passes it and is then published to
    -- `cmd.cmd.github.sync.requested.v1`, a subject nothing subscribes to and no test looks at.
    -- The class prefix is added by the publisher and belongs to nobody else.
    constraint schedules_command_type_is_not_already_a_subject
        check (command_type !~ '^(cmd|evt)\.'),
    -- The same grammar as `operations.operations.kind`. Stated separately from `command_type`
    -- rather than derived from it: stripping a version suffix and guessing the rest is the kind of
    -- cleverness that produces an operation kind nobody chose.
    constraint schedules_operation_kind_is_bounded
        check (operation_kind ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    -- The domain half of the command envelope. An object, never a scalar or an array: the envelope
    -- nests it under `payload`, and a consumer reading `payload.url` of a JSON number gets a type
    -- error at the far end of an asynchronous hop.
    constraint schedules_payload_is_an_object
        check (jsonb_typeof(payload) = 'object'),
    -- One minute to one year. The floor is not arbitrary: below it the tick interval and the drift
    -- it produces are the same order as the schedule itself, and four cores on one host
    -- (`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`) are not a load generator.
    constraint schedules_interval_is_between_a_minute_and_a_year
        check (interval_seconds between 60 and 31536000),
    constraint schedules_catch_up_is_known
        check (catch_up in ('skip', 'catch_up')),
    constraint schedules_updated_at_is_not_before_created_at
        check (updated_at >= created_at)
);

comment on table operations.schedules is
    'A periodic command, defined as data. ARCHITECTURE.md S10: Scheduler is a thin command '
    'publisher that does not import domain repositories and decides no domain behaviour, so what '
    'to publish and how often is a row rather than a constant in a binary. There is no route that '
    'writes this table at milestone 9: an operator inserts a schedule, which is why every column '
    'carries a CHECK an operator can trip.';
comment on column operations.schedules.owner_user_id is
    'References identity.users(user_id) semantically, with no foreign key: DATA_MODEL.md forbids '
    'cross-schema foreign keys. Every one of S10''s example schedules is per-user work — sync THIS '
    'account, snapshot THESE bookmarks — so the operation an occurrence creates has a real owner '
    'and the command carries a real principal. There is no system principal, and adding one would '
    'be a second kind of actor for the authorization model to grow a hole around.';
comment on column operations.schedules.next_due_at is
    'The next occurrence, and also the phase of the schedule: the grid is next_due_at + k * '
    'interval_seconds, so "every 24 hours at 03:00" is an interval of 86400 anchored at 03:00. '
    'That is why there is no cron expression here — a parser and its dependency would buy the '
    'phase this column already carries.';
comment on column operations.schedules.catch_up is
    'skip | catch_up. S10 requires catch-up policy to be explicit per schedule. `skip` publishes '
    'the current occurrence and moves to the next grid point after now, discarding what was missed '
    '— correct for a snapshot, where only the latest matters. `catch_up` advances one interval at '
    'a time, so every missed occurrence is eventually published — correct for an incremental sync '
    'that must not have gaps. Catch-up is bounded in code: a schedule enabled with a next_due_at '
    'far in the past jumps forward rather than publishing a year of commands.';
comment on column operations.schedules.enabled is
    'A schedule is created disabled. Enabling one starts publishing commands to a domain service '
    'that may not be deployed, which is an operator decision and not a consequence of writing a '
    'row.';

create unique index schedules_name_key
    on operations.schedules (name);

-- The only query the publisher runs. Partial, because a disabled schedule is never due.
create index schedules_due_idx
    on operations.schedules (next_due_at)
    where enabled;

-- ---------------------------------------------------------------------------------------------
-- schedule_occurrences
-- ---------------------------------------------------------------------------------------------

create table operations.schedule_occurrences (
    occurrence_id uuid        primary key,
    schedule_id   uuid        not null references operations.schedules (schedule_id) on delete cascade,
    due_at        timestamptz not null,
    published_at  timestamptz not null,
    drift_seconds bigint      not null,
    operation_id  uuid        references operations.operations (operation_id) on delete set null,

    constraint schedule_occurrences_drift_is_not_negative
        check (drift_seconds >= 0),
    constraint schedule_occurrences_published_at_is_not_before_due_at
        check (published_at >= due_at)
);

comment on table operations.schedule_occurrences is
    'One row per occurrence actually published. ARCHITECTURE.md S14: "Scheduler occurrences use '
    'deterministic IDs to prevent duplicate work", and this is where that determinism is enforced '
    'rather than assumed. occurrence_id is a name-based UUID over (schedule_id, due_at), so the '
    'same due time can only ever produce the same identifier, and the primary key refuses the '
    'second one.';
comment on column operations.schedule_occurrences.occurrence_id is
    'Also the outbox message_id and the operation''s idempotency key, so one identifier guards the '
    'same occurrence at all three layers. The outbox alone would nearly do it — enqueue is '
    'idempotent on message_id and no row is ever deleted — but "nearly" depends on the outbox '
    'never gaining a retention sweep, and a duplicate-suppression guarantee that a future cleanup '
    'can silently remove is not a guarantee.';
comment on column operations.schedule_occurrences.drift_seconds is
    'published_at - due_at. The scheduler-drift signal ARCHITECTURE.md S16 requires, stored as well '
    'as exported: the metric is the last value and this is the history.';
comment on column operations.schedule_occurrences.operation_id is
    'What the occurrence produced (S10 step 6, "record outcome"). Null only after an operation has '
    'been deleted, which cascades to nothing here: the record that the occurrence happened outlives '
    'the work it started.';

-- The database-level form of the same rule, independent of whether the derivation in Rust is
-- correct. A bug that produced colliding or diverging identifiers would otherwise publish twice
-- and leave no trace of having done so.
create unique index schedule_occurrences_schedule_id_due_at_key
    on operations.schedule_occurrences (schedule_id, due_at);

-- The operator's question: what has this schedule been doing lately.
--
-- Nothing prunes this table, and that is a measurement rather than an oversight: the shortest
-- legal interval is 60 seconds, so one schedule produces at most 525 600 rows a year at roughly a
-- hundred bytes each — some tens of megabytes on a 466 GB device. A retention sweep would be more
-- moving parts than the growth it prevents.
create index schedule_occurrences_schedule_id_published_at_idx
    on operations.schedule_occurrences (schedule_id, published_at desc);
