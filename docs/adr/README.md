# Platform ADRs

Use `NNNN-short-title.md` with status, context, drivers, options, decision, consequences, security/privacy, compatibility/migration, validation, and follow-up.

The numbers below are the FILES, not a plan. Milestone 1 wrote a backlog here that reserved numbers
for titles it guessed at, and later milestones spent three of those numbers on other decisions — 0004
on migrations, 0005 on NATS subjects, 0006 on `OpenAPI`. A reserved number is a promise about work
nobody has done yet, so this list no longer makes them: an ADR gets the next free number in the pull
request that writes it.

- ADR-0001: unused. Milestone 1 reserved it for "public authentication and device credential model"
  and never wrote it; the number is left empty rather than reused, because a dangling reference to
  ADR-0001 in an old review comment must not resolve to something else.
- [ADR-0002](0002-operation-state-machine-and-progress-semantics.md): Operation state machine and
  progress semantics. **Accepted.**
- [ADR-0003](0003-service-identity-and-producer-name.md): Service identity and the wire producer
  name. **Accepted.**
- [ADR-0004](0004-migration-layout-and-query-checking.md): Schema layout and query checking. Its migration-layout half is amended: there is one `schema.sql` and no ledger.
  **Accepted.**
- [ADR-0005](0005-nats-subjects-and-delivery.md): NATS subjects and delivery. **Accepted.**
- [ADR-0006](0006-public-api-versioning-and-openapi.md): REST versioning, and who owns the public
  `OpenAPI` document. **Accepted**, and discharged at milestone 7: the document is generated.
- [ADR-0007](0007-correlation-identity-and-trace-context.md): Correlation identity and trace context.
  **Accepted.**
- [ADR-0008](0008-capability-discovery.md): What a capability is, and what it is computed from.
  **Accepted.**
- [ADR-0009](0009-one-spelling-for-generic-ingest.md): One spelling for generic ingest. **Accepted**,
  and closes open question Q2.
- [ADR-0010](0010-single-node-deployment.md): One process per role, and why the locks stay.
  **Accepted.**
- [ADR-0011](0011-identity-assertion-trust-model.md): What an identity assertion is, and what
  Platform must hold to believe one. **Accepted.**
- [ADR-0012](0012-oauth-callback-relay.md): How an authorization code reaches the service that owns
  it. **Accepted**, with an amendment recorded in place: the first design bound a relay to the
  claiming session's audience and could never have worked.
- [ADR-0013](0013-single-host-deployment-profile.md): The single-host deployment profile — where
  schedules live, which process publishes, the NATS credential, the three database roles, and what
  the units enforce. **Accepted**, and discharges what ADR-0005 reserved and ADR-0010 scoped out.

Still unwritten, and deliberately unnumbered until somebody writes one: idempotency retention, which
should be written in the same pull request as the retention loop rather than left here; and the
device credential model, which is the half of public authentication milestone 8 did not touch.
