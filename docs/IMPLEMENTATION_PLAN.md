# Platform implementation plan

> **Status:** every item is implemented. Item 10 ran on the deployment target on 2026-08-19: the
> `linux/arm64` artifact was built, installed as three systemd units on the Raspberry Pi, and a
> capture submitted through the public API became an operation, a durable outbox row and a command on
> `JetStream` — alongside the same command from the webhook adapter and one from the scheduler.
>
> Item 5 is where the earlier ones meet: `ratatoskr-edge` opens a pool, applies `schema.sql`,
> authenticates a session, and writes the idempotency reservation, the operation and the outbox
> command in one transaction. Item 6 closes the loop — the publisher and the event consumer run in
> the process, and progress streams over SSE — so a capture now travels from a request to the bus
> and back into the projection a client reads. Item 7 opens a second door onto the same room:
> `ratatoskr-ingest` serves a webhook adapter that produces the SAME command, and a client can now
> ask which doors a deployment actually has.
>
> Items 9 and 10 were reworded on 2026-08-19, before item 9 was built. Item 9 already owned the
> deployment profile — five code and documentation sites deferred decisions to "the deployment
> profile at milestone 9" — while being worded as a scheduler feature, so an agent reading only the
> plan would have shipped a scheduler and left the profile deferred to nobody. All five deferrals are
> now discharged by [ADR-0013](adr/0013-single-host-deployment-profile.md). Item 10 as written passed
> entirely on a developer's machine and claimed end-to-end success without touching the machine the
> system runs on. The `linux/arm64` build is not a separate item: it is the precondition of running
> the slice, and an item after the one it enables lets that item's Definition of Done pass on the one
> check it exists for. See `ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`.

Open questions and their resolutions are recorded in `DEVELOPMENT.md`. Q2 (the
`platform_ingress` versus `platform_ingest` schema spelling) closed at item 7 through
[ADR-0009](adr/0009-one-spelling-for-generic-ingest.md). Q4 closed at item 4 without a contracts
change: `correlation` remains legal through the open wire vocabulary and deliberately absent from
the contracts repository's closed fixture vocabulary.

1. Create Rust workspace, typed config, errors, telemetry, health/readiness. *(implemented)*
2. Implement `identity` schema, users, sessions, devices, and revocation. *(implemented)*
3. Implement `operations` schema and state machine. *(implemented)*
4. Add transactional outbox/inbox and NATS identities/subjects. *(implemented)*
5. Implement authenticated capture API and idempotency. *(implemented)*
6. Project progress/results and expose SSE. *(implemented)*
7. Add capabilities and generic ingress. *(implemented)*
8. Add OAuth callback facade and Telegram assertion exchange. *(implemented)*
9. Add thin Scheduler command publication **and the single-host deployment profile**: the `deploy/`
   systemd units with their resource limits and start ordering, stream and consumer naming with
   explicit retention limits, the NATS credential, the three database roles inside the host's
   existing PostgreSQL cluster, the storage layout and the port map. *(implemented)*
10. Build the `linux/arm64` artifact and run the first workspace end-to-end vertical slice **on the
    deployment target**. *(implemented)*

The rewording of items 9 and 10 paid for itself in item 10, which is the only reason the note above
is worth keeping. Running on the machine found four things reading could not: the host's PostgreSQL
is a container and not a service, so half the profile's administrative commands named a user and a
unit that do not exist; `systemd` sets a process's primary group and not its other memberships, so
the units put every service in a group that could not read its own credentials; `ufw` drops the
metrics path, which an earlier verification missed because it was performed with a container rather
than a host process; and one database grant was written from reading a request handler instead of
everything the handler calls. None of those is visible from a developer's machine, and item 10 as
first written would have passed on one.

Definition of Done: requirements, persistence constraints, auth, retries, tests, telemetry, OpenAPI drift, schema apply, and workspace integration pass — and, from item 10 onward, the `linux/arm64` artifact builds and its boot tests pass on the deployment target. Deferred: broad automation, direct domain queries, and multi-tenant SaaS controls.
