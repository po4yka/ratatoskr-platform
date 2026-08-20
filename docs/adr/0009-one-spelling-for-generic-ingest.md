# ADR-0009: One spelling for generic ingest

> Status: Accepted
> Date: 2026-08-19
> Milestone: 7
> Resolves: open question Q2 in `DEVELOPMENT.md`

## Context

`README.md` spells the generic-ingress schema `platform_ingress.*`, and `DEVELOPMENT.md` records
`AGENTS.md` as agreeing with it. `docs/ARCHITECTURE.md` S4.1 spells it `platform_ingest.*`. Milestone 2 could not resolve the
contradiction and could not work around it either — a schema name is written into the schema
definition, and at the time that definition was a committed migration, the one artifact this
repository never rewrote — so it wrote `-- platform_ingest is milestone 7` into
`migrations/0001_identity.sql` and recorded the question as Q2.

Milestone 7 is where the schema is created. The question is due.

It is not only a schema name. The same word appears in a binary name, a crate name, a library name,
a database role, and now a public URL path. Every one of those is an operational contract that
outlives the pull request that writes it.

## Drivers

- A reader who greps for one spelling must find everything. Two spellings mean every search is
  half-blind, forever.
- The binary is already named `ratatoskr-ingest` and is already deployed under that name in
  `compose.yaml` and in `RuntimeRole::Ingest`. A schema that disagrees with the process that owns it
  makes an operator's `\dn` output fail to match their `ps` output.
- `docs/ARCHITECTURE.md` S18 already names the database role `platform_ingest`. A role and the
  schema it is granted on must match, or the `GRANT` statement reads as a typo.
- Milestone 1 already fixed the precedence: Q1 records that where `README.md` and
  `docs/ARCHITECTURE.md` disagree, S3 is normative and the README is the stale document.

## Options

| # | Option | Outcome |
|---|---|---|
| a | `platform_ingress`, and rename the binary and the role to match | **Rejected.** It renames three things that already exist and one that already ships, to make two documents agree with the one that milestone 1 declared stale. |
| b | `platform_ingest` everywhere: schema, crate, library, role, path | **Chosen.** |
| c | `platform_ingest` for the schema, `ingress` for the activity in prose | **Rejected.** It is option b plus a rule to remember, and the rule is what produced Q2. |

## Decision

**The word is `ingest`, everywhere it is an identifier.**

| Identifier | Value |
|---|---|
| `PostgreSQL` schema | `platform_ingest` |
| Cargo package | `ratatoskr-platform-ingest` |
| Rust library | `platform_ingest` |
| Binary and runtime role | `ratatoskr-ingest`, `RuntimeRole::Ingest` |
| Database role (S18) | `platform_ingest` |
| Public path prefix | `/v1/ingest/…` |

`README.md` carried the only `platform_ingress.*` identifier in the repository and is corrected.
`AGENTS.md` needed no correction at all: every "ingress" in it is prose naming the *activity* —
traffic arriving from outside — which reads correctly and collides with nothing, because prose is not
an identifier. Q2's claim that it agreed with the README was itself the kind of second-hand statement
this ADR exists to stop. The rule is narrow enough to hold: **if it can be typed into a shell, a URL
bar or a `use` statement, it is spelled `ingest`.**

## Consequences

- `schema.sql` creates `platform_ingest`, and the name is now permanent: reversing it later would be
  a schema rename with a data move, not an edit.
- `crates/ingest` is the first crate whose name a reader can predict from the binary that uses it.
- The public path is `/v1/ingest/webhooks/{source_id}`. It is versioned like every other route
  (ADR-0006) even though its callers are third parties rather than our own clients — a third party
  is exactly the caller who cannot be redeployed in step with us.

## Security and privacy

None directly. Indirectly: S18 grants `platform_ingest` least privilege on `platform_ingest.*`, and
that `GRANT` is only writable without ambiguity once the two names are the same string.

## Compatibility and migration

Nothing is deployed and no schema exists, so there is nothing to migrate. That is precisely why the
question had to be answered before `0005`, and why milestone 2 was right to refuse to answer it.

## Validation

The `platform_ingest` section of `schema.sql` applies under `Database::apply_schema`, and
`crates/ingest/tests/webhook.rs` reaches every table in it. A repository-wide grep for
`platform_ingress` returns nothing outside this ADR.

## Follow-up

None. Q2 is closed.
