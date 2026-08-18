# Platform implementation plan

> **Status:** item 1 is implemented (see `DEVELOPMENT.md`). Items 2 through 10 are planned, and none of them is scaffolded or stubbed in the checkout.

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
