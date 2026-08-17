# Developing Ratatoskr Platform

> Status: Proposed  
> Last reviewed: 2026-08-17

The repository is in architecture bootstrap; Edge, Ingest, Scheduler, schemas, and APIs are not implemented. Expected stack: Rust/Tokio, Axum/Tower, SQLx/PostgreSQL, NATS JetStream, OpenAPI, and OpenTelemetry.

## Expected workflow

- Run Edge, Ingest, and Scheduler as separate roles from one repository.
- Use typed configuration and secret-aware values.
- Keep public request handlers short; durable work becomes an operation and command.
- Write only `identity` and `operations` owned schemas.
- Add outbox/inbox and idempotency tests with every asynchronous path.
- Generate the public API client from OpenAPI; never hand-maintain duplicate endpoint models.

The first scaffold PR must document exact Rust, PostgreSQL, NATS, migration, test, OpenAPI, and local-run commands. Production credentials are never required for the default tests.
