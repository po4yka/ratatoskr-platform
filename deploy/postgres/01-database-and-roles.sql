-- The database and the three roles. Run FIRST, before `ratatoskr-edge` has ever started.
--
--     sudo -u postgres psql -f deploy/postgres/01-database-and-roles.sql
--
-- There are two files because a migration creates the schemas. `grant usage on schema identity`
-- cannot be written before `identity` exists, and `identity` is created by `migrations/0001` — which
-- runs as `ratatoskr_edge`, from inside `ratatoskr-edge`, after this file has given it the
-- `create` privilege on the database. So: this file, then the first edge start, then
-- `02-grants.sql`.
--
-- By hand and not by a container entry point: `docker-entrypoint-initdb.d` never runs again against
-- a non-empty data directory, and the target's cluster is not empty
-- (`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`). By hand and not by a migration: a role that can
-- create roles is not a least-privilege role.
--
-- Re-runnable, apart from `create database`, which reports that the database exists and is skipped.
-- It carries no password: set them separately, so this file can be read by anyone and pasted into a
-- ticket.
--
--     \password ratatoskr_edge

-- ---------------------------------------------------------------------------------------------
-- roles
-- ---------------------------------------------------------------------------------------------
--
-- None of the three may create a role, create a database, bypass row security or replicate.

do $$
begin
    if not exists (select 1 from pg_roles where rolname = 'ratatoskr_edge') then
        create role ratatoskr_edge login;
    end if;
    if not exists (select 1 from pg_roles where rolname = 'ratatoskr_ingest') then
        create role ratatoskr_ingest login;
    end if;
    if not exists (select 1 from pg_roles where rolname = 'ratatoskr_scheduler') then
        create role ratatoskr_scheduler login;
    end if;
end
$$;

alter role ratatoskr_edge      nosuperuser nocreatedb nocreaterole noreplication nobypassrls;
alter role ratatoskr_ingest    nosuperuser nocreatedb nocreaterole noreplication nobypassrls;
alter role ratatoskr_scheduler nosuperuser nocreatedb nocreaterole noreplication nobypassrls;

-- ---------------------------------------------------------------------------------------------
-- the database
-- ---------------------------------------------------------------------------------------------
--
-- The collation is STATED, never inherited. The cluster's existing databases use the libc provider,
-- and glibc changes its collation silently across a distribution upgrade — `apt-daily-upgrade.timer`
-- is enabled on this host. PostgreSQL tracks the ICU version and warns on a mismatch instead. A text
-- btree index that no longer holds is not a performance problem: if
-- `identities_provider_external_id_key` stops holding, one external account maps to two internal
-- users, which is an authentication defect. `compose.yaml`, CI and
-- `crates/persistence/src/test_support.rs` create theirs with the same three clauses, so the SQL is
-- verified against the collation it will run under.
--
-- `template0`, because `template1` carries whatever the cluster was initialised with.

\set ON_ERROR_STOP off
create database ratatoskr
    owner ratatoskr_edge
    template template0
    locale_provider icu
    icu_locale 'und-x-icu'
    encoding 'UTF8';
\set ON_ERROR_STOP on

-- Deny by default: `public` is granted `connect` on every new database.
revoke all on database ratatoskr from public;
grant connect on database ratatoskr to ratatoskr_edge, ratatoskr_ingest, ratatoskr_scheduler;

-- `create` on the DATABASE is what lets `ratatoskr_edge` run `create schema` from a migration. It is
-- granted to that role and to no other: only one process migrates (ADR-0010).
grant create on database ratatoskr to ratatoskr_edge;

\connect ratatoskr

-- PostgreSQL 15 and later already revoke `create` on `public` from `public`; stated anyway, because
-- this cluster may outlive the assumption and a writable `public` schema is where an intruder puts
-- a function that shadows one on the search path.
revoke all on schema public from public;
