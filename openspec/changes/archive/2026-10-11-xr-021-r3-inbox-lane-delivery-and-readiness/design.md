## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md sections R3-01, R3-02, R3-03, R3-13, R3-12, R3-11 and R3-19. The contract text is the source of truth; this file records the Platform-side decisions.

## Decisions

- **Inbox prefix, not inbox deny.** NATS compares whole tokens, so `_INBOX_x.>` does not match `_INBOX.>`. Each non-Edge stanza allows exactly `_INBOX_<name>.>`; Edge keeps `_INBOX.>` because it creates and administers. A client replies on `Client::new_inbox()`, which is `{prefix}.{id}` under `custom_inbox_prefix`, so a service that does not set the prefix times out on its first JetStream request.
- **Refusal is observed on the client.** A refused subscribe is only a `Subscription Violation` line in the server log and a server error delivered to the client's event callback. The real-broker tests register an event callback and assert on it, and also assert the positive path (a JetStream request completes on the identity's own prefix).
- **The lane lives in the operation row.** `operations.operations.reporter` is nullable text with a producer-name check. `accept` writes it. `AUTHORITY` stays as the closed list of producers a kind may name and now also bounds `accept`. `authorize` for a `Listed` kind requires the stored reporter to be non-null and equal to the report producer. `ProviderBound` keeps the acceptance-record check and AI-archive operations record no reporter.
- **A permanent refusal is decided once, in finalize.** `finalize_transfer` classifies the forwarded status: a 4xx other than 408 and 429 is permanent. It then marks the session `failed`, calls the existing `fail_ai_archive_delivery`, and removes the staging directory in the same request. 5xx, timeouts and the mapped 502 leave everything open so the client may retry.
- **401 and 407 never cross the gateway.** Edge sends no `Authorization` to a receiver, so an upstream 401 is a deployment fault and is mapped to `UpstreamInvalidResponse` (502) with the log class `upstream_unauthenticated`.
- **Readiness is about Edge.** The four receiver checks stay in `checks()` so the body and the capability document report them; `is_ready` ignores them.
- **Route diagnostics.** Gateway violations build `key` and `env_var` from the route name, one entry per failed field.

## Risks / Trade-offs

- A service built before this round cannot complete JetStream requests under the new ACL. Nothing is deployed, so the break is named, not staged.
- Docker-based real-broker tests cannot run on a machine without a Docker daemon; CI runs them. The local run uses the `PLATFORM_TEST_NATS_SERVER` seam documented in `DEVELOPMENT.md`.
