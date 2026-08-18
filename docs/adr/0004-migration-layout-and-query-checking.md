# ADR-0004: One migration directory, and runtime-checked queries

> Status: Accepted
> Date: 2026-08-18
> Milestones: 2, 3

## Context

`docs/ARCHITECTURE.md` S3 draws two migration directories, `migrations/identity/` and
`migrations/operations/`, matching the two schemas Platform owns. Separately, `sqlx` offers two ways
to check SQL: the `query!` macros, which verify every statement against a live database at compile
time, and the plain `query`/`query_as` builders, which are checked when they run.

Both choices had to be made before the first migration was written, because both are expensive to
reverse once a schema is deployed and a build pipeline depends on them.

## Drivers

- A migration ledger must be unambiguous: exactly one recorded order of application.
- `cargo build` should depend on the source tree and nothing else.
- A wrong column name must fail before it reaches a deployment.
- ARCHITECTURE S3 is a target layout, not a constraint that outranks a tool's actual behaviour.

## Options

**Migration layout.** (a) Two directories as drawn. (b) One directory, owning schema in the file name.

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

(c) makes `cargo build` require a database, which breaks a clean checkout and the CI build job.
(d) avoids that but introduces a generated artifact that must be regenerated and reviewed on every
query change, and that goes stale silently when it is not. (e) keeps the build a pure function of the
source tree and moves the check to the place that can also catch a wrong constraint, a wrong trigger
and a wrong `on conflict` clause, none of which the macros verify.

## Consequences

- A typo in a column name fails in the integration suite, not at compile time. That suite is
  therefore not optional: CI runs a PostgreSQL service and every statement in `identity` and
  `operations` is exercised by a test that talks to it.
- `migrations/` is flat. A reader finds the owning schema in the file name and in the `create schema`
  statement at the top of each file.
- The advisory lock `sqlx` takes during `run` still makes a rolling deployment safe.

## Security and privacy

None. Neither choice changes what is stored or who can read it.

## Compatibility and migration

Both decisions are pre-release. Reversing (b) later would mean rewriting the ledger; reversing (e)
means adopting `cargo sqlx prepare` and committing `.sqlx`, which is additive and can be done at any
time without touching a migration.

## Validation

`crates/identity/tests/identity.rs` and `crates/operations/tests/lifecycle.rs` execute every
statement in both crates against a disposable database created from the embedded migrations.

## Follow-up

Revisit (e) when the CI build job has a database service of its own; at that point the macros cost
nothing extra and would move the check earlier.
