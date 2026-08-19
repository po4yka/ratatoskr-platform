# Platform implementation plan

> **Status:** items 1 through 5 are implemented (see `DEVELOPMENT.md`). Items 6 through 10 are planned,
> and none of them is scaffolded or stubbed in the checkout.
>
> Item 5 is where the earlier ones meet: `ratatoskr-edge` now opens a pool, applies the migrations,
> authenticates a session, and writes the idempotency reservation, the operation and the outbox
> command in one transaction. The outbox PUMP still has no caller — nothing publishes to NATS in a
> running process yet — and SSE arrives with item 6.

Open questions that block a later item are recorded in `DEVELOPMENT.md`. Q2 (the `platform_ingress` versus `platform_ingest` schema spelling) blocks item 2 and Q4 (the `correlation` entity kind in `contracts.toml`) blocks item 4; neither can be worked around in the item that hits it.

1. Create Rust workspace, typed config, errors, telemetry, health/readiness. *(implemented)*
2. Implement `identity` schema, users, sessions, devices, and revocation.
3. Implement `operations` schema and state machine.
4. Add transactional outbox/inbox and NATS identities/subjects.
5. Implement authenticated capture API and idempotency.
6. Project progress/results and expose SSE.
7. Add capabilities and generic ingress.
8. Add OAuth callback facade and Telegram assertion exchange.
9. Add thin Scheduler command publication.
10. Run the first workspace end-to-end vertical slice.

Definition of Done: requirements, persistence constraints, auth, retries, tests, telemetry, OpenAPI drift, migrations, and workspace integration pass. Deferred: broad automation, direct domain queries, and multi-tenant SaaS controls.
