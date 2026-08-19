# Platform ADRs

Use `NNNN-short-title.md` with status, context, drivers, options, decision, consequences, security/privacy, compatibility/migration, validation, and follow-up.

Initial backlog:

- ADR-0001: Public authentication and device credential model. *(not written)*
- [ADR-0002](0002-operation-state-machine-and-progress-semantics.md): Operation state machine and
  progress semantics. **Accepted** — the decision binds milestone 3; no code exists yet.
- [ADR-0003](0003-service-identity-and-producer-name.md): NATS subject and service-identity model.
  **Accepted for the service-identity half only**; the NATS subject model is deferred to an
  amendment at milestone 4.
- ADR-0004: Idempotency scope and retention. *(not written)*
- ADR-0005: Capability discovery contract. *(not written)*
- ADR-0006: REST versioning and generated OpenAPI policy. *(accepted; SSE versioning arrives with milestone 6)*
- [ADR-0007](0007-correlation-identity-and-trace-context.md): Correlation identity and trace context.
  **Accepted.**
