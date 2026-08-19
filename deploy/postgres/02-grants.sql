-- What `ratatoskr-ingest` and `ratatoskr-scheduler` may see. Run AFTER `ratatoskr-edge` has started
-- at least once, because it grants on tables that its migrations create.
--
--     sudo -u postgres psql -d ratatoskr -f deploy/postgres/02-grants.sql
--
-- Re-run it after any migration that adds a table one of those two roles must WRITE. The default
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
--   * `ratatoskr_ingest` cannot read `identity` at all. The process with the largest
--     unauthenticated attack surface in the system cannot reach a session credential hash, an OAuth
--     relay, or a user's provider identity.
--   * `ratatoskr_scheduler` cannot read `identity` or `platform_ingest`. It publishes commands from
--     rows an operator wrote, and has no reason to see either.
--
-- Neither may create anything anywhere: `create` on a schema is what a migration needs, and only
-- `ratatoskr-edge` migrates.

grant usage on schema platform_ingest to ratatoskr_ingest;
grant usage on schema operations      to ratatoskr_ingest;
grant usage on schema operations      to ratatoskr_scheduler;

-- ---------------------------------------------------------------------------------------------
-- table access
-- ---------------------------------------------------------------------------------------------

-- Ingest: authenticate a source, reserve an idempotency key, create an operation, enqueue a
-- command. `delete` appears nowhere — nothing on this path removes a row, and a role that cannot
-- delete cannot be made to.
grant select                 on platform_ingest.webhook_sources to ratatoskr_ingest;
grant select, insert, update on operations.idempotency_records  to ratatoskr_ingest;
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
-- A migration creates a table owned by `ratatoskr_edge`, and a new table grants nothing to anybody.
-- These make the next migration's tables readable without a second manual step — inside
-- `operations` only, and as `select` only.

alter default privileges for role ratatoskr_edge in schema operations
    grant select on tables to ratatoskr_ingest, ratatoskr_scheduler;

-- ---------------------------------------------------------------------------------------------
-- verification
-- ---------------------------------------------------------------------------------------------
--
-- Both `f`:
--
--   select has_schema_privilege('ratatoskr_ingest',    'identity', 'usage'),
--          has_schema_privilege('ratatoskr_scheduler', 'identity', 'usage');
--
-- All four `t`:
--
--   select has_table_privilege('ratatoskr_scheduler', 'operations.schedules',            'select'),
--          has_table_privilege('ratatoskr_scheduler', 'operations.schedule_occurrences', 'insert'),
--          has_table_privilege('ratatoskr_scheduler', 'operations.operations',           'insert'),
--          has_table_privilege('ratatoskr_scheduler', 'operations.outbox',               'insert');
