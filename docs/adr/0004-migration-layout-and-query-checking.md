# ADR-0004: One schema definition, and runtime-checked queries

> Status: Accepted
> Date: 2026-08-18
> Milestones: 2, 3

## Context

When this decision was made, `docs/ARCHITECTURE.md` S3 drew two migration directories,
`migrations/identity/` and `migrations/operations/`, matching the two schemas Platform owned then.
Separately, `sqlx` offers two ways to check SQL: the `query!` macros, which verify every statement
against a live database at compile time, and the plain `query`/`query_as` builders, which are
checked when they run.

Both choices had to be made before the first schema statement was written, because both are expensive
to reverse once a schema is deployed and a build pipeline depends on them.

## Drivers

- A schema definition must be unambiguous: exactly one recorded order of application.
- `cargo build` should depend on the source tree and nothing else.
- A wrong column name must fail before it reaches a deployment.
- ARCHITECTURE S3 is a target layout, not a constraint that outranks a tool's actual behaviour.

## Options

**Migration layout.** (a) Two directories as drawn. (b) One directory, owning schema in the file name.
(Superseded — see the amendment under Decision.)

**Query checking.** (c) `sqlx::query!` macros with a live `DATABASE_URL` at compile time.
(d) `sqlx::query!` macros with a committed `.sqlx` offline cache. (e) Runtime-checked builders plus
an integration suite that runs every statement against a real PostgreSQL.

## Decision

**(b) one directory** and **(e) runtime-checked builders**.

`sqlx::Migrator` records applied versions in a single `_sqlx_migrations` table and exposes no way to
change that table's name. Verified against sqlx 0.8.6: `Migrator`'s only setters are
`set_ignore_missing` and `set_locking`. Two directories would therefore share one ledger and collide
on version numbers — `0001_` in each — which is a corruption, not an inconvenience. The owning schema
is carried in the file name instead: `0001_identity.sql`, `0002_operations.sql`.

**Amended when the project recorded its in-development status. (b) is superseded: there is no
migration directory and no ledger. One file, `schema.sql` at the repository root, applied to a fresh
database.** The eight migrations it replaces stay in this repository's history. A database that
already ran them keeps its `_sqlx_migrations` ledger, which nothing here removes; see
`deploy/README.md` for what that means for a host that has one. The
reason (b) was ever a question — which directory a ledger belongs to — has no answer left to give,
because there is no ledger: no database holds data that has to survive a schema change, and a new
database is created from this file in one statement batch. What the ledger cost is what decided it:
a committed migration is immutable, comments included, so a comment that turned out to be wrong
could not be corrected where it was written. That rule bought ordering guarantees for data that has
to survive a schema change, and no database here holds any. The owning schema is now carried by the
section of the file a table sits in, and `docs/ARCHITECTURE.md` S3's two directories are superseded
rather than deferred.

**(e) is unchanged and still correct.** Nothing below about query checking depends on how the schema
is laid out.

(c) makes `cargo build` require a database, which breaks a clean checkout and the CI build job.
(d) avoids that but introduces a generated artifact that must be regenerated and reviewed on every
query change, and that goes stale silently when it is not. (e) keeps the build a pure function of the
source tree and moves the check to the place that can also catch a wrong constraint, a wrong trigger
and a wrong `on conflict` clause, none of which the macros verify.

## Consequences

- A typo in a column name fails in the integration suite, not at compile time. That suite is
  therefore not optional: CI runs a PostgreSQL service and every statement in `identity` and
  `operations` is exercised by a test that talks to it.
- `schema.sql` is organised by owning schema — `identity`, then `operations`, then
  `platform_ingest` — and a reader finds the owner in the section heading and in the `create schema`
  statement that opens it.
- **A schema change edits `schema.sql` in place.** There is no checksum and no applied record, so a
  comment that turned out to be wrong is corrected where it is wrong. **Amended when the ledger
  became one schema file.** This consequence used to read "a committed migration is immutable,
  comments included", and it was learned the hard way at milestone 9: a comment in `0007` was
  corrected to describe a retention sweep that had just been added, and every already-migrated
  database refused to start with the message "the database schema could not be brought up to date"
  and nothing about which file. That rule is gone with the ledger that imposed it, and the correction
  it forced into `0008`'s header now sits on the index it describes.
- **`schema.sql` is a build input.** `crates/persistence/src/lib.rs` `include_str!`s it, so editing
  it rebuilds the crate and everything that links it. This replaces `crates/persistence/build.rs` and
  the directory-listing test, which existed because `sqlx::migrate!` emitted change tracking per FILE
  and a file that did not exist yet was tracked by nothing — an already-built artifact then kept the
  set it was compiled with and reported success one migration short. One file cannot go missing from
  a set of one. Test M-1 now checks what is left to get wrong: the file applies, all three schemas
  appear, and applying it to a database that already has it is a no-op.
- The advisory lock still makes overlapping applications safe.
  **Amended at milestone 9.** This line originally read "still makes a rolling deployment safe".
  There is no rolling deployment: the target is one host with one process per role (ADR-0010).
  The lock is kept, and re-founded on the case that does happen — a restart that overlaps the
  previous process's grace window is two processes applying the schema at once.
  **Amended again when the ledger became one schema file.** The lock was sqlx's, taken inside
  `Migrator::run`; it is now `pg_advisory_xact_lock` taken by `Database::apply_schema`, in the same
  transaction as the presence check and the apply, so the second process waits, then sees the schema
  and does nothing.

## Security and privacy

None. Neither choice changes what is stored or who can read it.

## Compatibility and migration

Both decisions are pre-release, and (b) has been reversed because no database holds data that has to
survive a schema change. It is not free. `Database::apply_schema` skips the apply when the `identity`
schema is already present, so a database that already has a schema never receives another one and
cannot report that it has drifted from `schema.sql`. The remedy is to recreate that database, and
`deploy/README.md` says so where an operator reads it. Reinstating a ledger later means writing
`schema.sql` as
`0001_*.sql` and starting the numbering there, which is what the first database with data to keep
would need anyway. Reversing (e) means adopting `cargo sqlx prepare` and committing `.sqlx`, which
is additive and can be done at any time without touching the schema.

## Validation

`crates/identity/tests/identity.rs` and `crates/operations/tests/lifecycle.rs` execute every
statement in both crates against a disposable database created from the embedded `schema.sql`.
`crates/persistence/tests/schema.rs` is test M-1: the file applies, the three schemas appear, and a
second apply returns without error.

## Follow-up

Revisit (e) when the CI build job has a database service of its own; at that point the macros cost
nothing extra and would move the check earlier.
