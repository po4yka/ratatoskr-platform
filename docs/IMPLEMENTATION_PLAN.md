# Platform implementation plan

1. Create Rust workspace, typed config, errors, telemetry, health/readiness.
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
