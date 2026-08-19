# Platform implementation plan

> **Status:** items 1 through 7 are implemented (see `DEVELOPMENT.md`). Items 8 through 10 are planned,
> and none of them is scaffolded or stubbed in the checkout.
>
> Item 5 is where the earlier ones meet: `ratatoskr-edge` opens a pool, applies the migrations,
> authenticates a session, and writes the idempotency reservation, the operation and the outbox
> command in one transaction. Item 6 closes the loop — the publisher and the event consumer run in
> the process, and progress streams over SSE — so a capture now travels from a request to the bus
> and back into the projection a client reads. Item 7 opens a second door onto the same room:
> `ratatoskr-ingest` serves a webhook adapter that produces the SAME command, and a client can now
> ask which doors a deployment actually has.

Open questions that block a later item are recorded in `DEVELOPMENT.md`. Q2 (the `platform_ingress` versus `platform_ingest` schema spelling) blocked item 7 and is closed by [ADR-0009](adr/0009-one-spelling-for-generic-ingest.md). Q4 (the `correlation` entity kind in `contracts.toml`) is a one-line change to a sibling repository and is still open.

1. Create Rust workspace, typed config, errors, telemetry, health/readiness. *(implemented)*
2. Implement `identity` schema, users, sessions, devices, and revocation. *(implemented)*
3. Implement `operations` schema and state machine. *(implemented)*
4. Add transactional outbox/inbox and NATS identities/subjects. *(implemented)*
5. Implement authenticated capture API and idempotency. *(implemented)*
6. Project progress/results and expose SSE. *(implemented)*
7. Add capabilities and generic ingress. *(implemented)*
8. Add OAuth callback facade and Telegram assertion exchange.
9. Add thin Scheduler command publication.
10. Run the first workspace end-to-end vertical slice.

Definition of Done: requirements, persistence constraints, auth, retries, tests, telemetry, OpenAPI drift, migrations, and workspace integration pass. Deferred: broad automation, direct domain queries, and multi-tenant SaaS controls.
