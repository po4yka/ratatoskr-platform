-- What `ratatoskr-ingest` and `ratatoskr-scheduler` may see. Run AFTER `ratatoskr-edge` has started
-- at least once, because it grants on tables that `schema.sql` creates.
--
--     sudo -u postgres psql -d ratatoskr -f deploy/postgres/02-grants.sql
--
-- Re-run it after any schema change that adds a table one of those two roles must WRITE. The default
-- privileges at the end cover reading; writing is granted by name, on purpose, so that adding a
-- table never silently hands two services write access to it.
--
-- Re-runnable: every statement is a `grant`.

-- ---------------------------------------------------------------------------------------------
-- schema access — the boundary that matters
-- ---------------------------------------------------------------------------------------------
--
-- `ratatoskr_edge` owns all three schemas because it created them, so it needs nothing here. The
-- other two get `usage` on what they use and nothing on the rest, which is the whole point:
--
--   * `ratatoskr_ingest` cannot READ `identity`. It may append to exactly one table there —
--     `audit_events` — and hold no other privilege on the schema, so the process with the largest
--     unauthenticated attack surface in the system still cannot reach a session credential hash, an
--     OAuth relay, or a user's provider identity, and cannot read back what it wrote. That narrowing
--     is deliberate: `usage` on a schema grants the right to NAME an object in it and nothing else,
--     and `insert` without `select` is an append-only right. The reason it exists at all is that a
--     webhook credential presented at another source's URL is an attributable security decision, and
--     an audit trail that omits the one process most exposed to the internet is not an audit trail.
--   * `ratatoskr_scheduler` cannot read `identity` or `platform_ingest`. It publishes commands from
--     rows an operator wrote, and has no reason to see either.
--
-- Neither may create anything anywhere: `create` on a schema is what `schema.sql` needs, and only
-- `ratatoskr-edge` applies it.

grant usage on schema platform_ingest to ratatoskr_ingest;
grant usage on schema operations      to ratatoskr_ingest;
grant usage on schema identity        to ratatoskr_ingest;
grant usage on schema operations      to ratatoskr_scheduler;

-- ---------------------------------------------------------------------------------------------
-- table access
-- ---------------------------------------------------------------------------------------------

-- Ingest: authenticate a source, reserve an idempotency key, create an operation, enqueue a
-- command.
--
-- `delete` on the ledger, and ONLY there. `platform_idempotency::reserve` opens by removing the
-- expired reservation for this key, so that the insert after it either succeeds cleanly or collides
-- with a LIVE row — without that, the caller would have to reason about whether a conflict it was
-- told about is still in force. This grant was missing until milestone 10, and the symptom was a
-- webhook answering 504 with "the signal could not be reserved" while the identical code path on
-- edge worked, because edge owns the schema. It was written by reading the handler instead of
-- everything the handler calls.
grant select                 on platform_ingest.webhook_sources to ratatoskr_ingest;
-- Append only, and to one table. No `select`, so a compromised adapter cannot read the trail it is
-- writing to; no `update` or `delete`, so it cannot rewrite one either. Every other table in
-- `identity` stays unreachable, which the verification block at the bottom checks by name.
grant insert                 on identity.audit_events           to ratatoskr_ingest;
grant select, insert, update, delete on operations.idempotency_records to ratatoskr_ingest;
grant select, insert         on operations.operations           to ratatoskr_ingest;
grant select, insert         on operations.outbox               to ratatoskr_ingest;

-- Scheduler: read due schedules, move them forward, record occurrences, create operations, enqueue
-- commands. `update` on `schedules` and on nothing else: it may move a schedule's next due time and
-- may not change what the schedule publishes.
grant select, update on operations.schedules            to ratatoskr_scheduler;
grant select, insert on operations.schedule_occurrences to ratatoskr_scheduler;
grant select, insert on operations.operations           to ratatoskr_scheduler;
grant select, insert on operations.outbox               to ratatoskr_scheduler;

-- ---------------------------------------------------------------------------------------------
-- future tables
-- ---------------------------------------------------------------------------------------------
--
-- `schema.sql` creates tables owned by `ratatoskr_edge`, and a new table grants nothing to anybody.
-- These make the next new table readable without a second manual step — inside `operations` only,
-- and as `select` only.

alter default privileges for role ratatoskr_edge in schema operations
    grant select on tables to ratatoskr_ingest, ratatoskr_scheduler;

-- ---------------------------------------------------------------------------------------------
-- verification
-- ---------------------------------------------------------------------------------------------
--
-- The scheduler has no reach into `identity` at all, and ingest can neither read the audit trail it
-- appends to nor touch anything else there. All five `f`:
--
--   select has_schema_privilege('ratatoskr_scheduler', 'identity', 'usage'),
--          has_table_privilege('ratatoskr_ingest', 'identity.audit_events', 'select'),
--          has_table_privilege('ratatoskr_ingest', 'identity.sessions',     'select'),
--          has_table_privilege('ratatoskr_ingest', 'identity.identities',   'select'),
--          has_table_privilege('ratatoskr_ingest', 'identity.oauth_relays', 'select');
--
-- and the one thing it may do, `t`:
--
--   select has_table_privilege('ratatoskr_ingest', 'identity.audit_events', 'insert');
--
-- And the one the handler does not name but its callee does:
--
--   select has_table_privilege('ratatoskr_ingest', 'operations.idempotency_records', 'delete');
--
-- All four `t`:
--
--   select has_table_privilege('ratatoskr_scheduler', 'operations.schedules',            'select'),
--          has_table_privilege('ratatoskr_scheduler', 'operations.schedule_occurrences', 'insert'),
--          has_table_privilege('ratatoskr_scheduler', 'operations.operations',           'insert'),
--          has_table_privilege('ratatoskr_scheduler', 'operations.outbox',               'insert');
