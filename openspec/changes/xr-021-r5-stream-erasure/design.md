## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md R5-12. The contract text is the source of truth; this file records the Platform-side decisions.

## Decisions

- **Edge erases, producers ask.** Edge is the only identity with `$JS.API.>`; no producer receives a stream-management permission. A producer's reach is one publish subject, and what it can erase is limited by the handler's own checks (the producer name and the tenant on the stored message), so a compromised producer can erase only messages that carry its own name and the tenant it names.
- **The handler is a `consumer::Handler`.** It runs inside the transaction that holds the inbox row, so a replay of the same command id is absorbed by the inbox and erases nothing twice. The erasure itself is idempotent anyway: a sequence that is gone is `missing`.
- **Raw JetStream requests.** async-nats exposes `get_raw_message(sequence)` and `delete_message(sequence)`, which send `STREAM.MSG.GET` and `STREAM.MSG.DELETE`. The handler uses them through the Edge connection's context. `delete_message` uses the default secure erase.
- **Validation order.** Envelope first (Term on failure, because a retry cannot fix it), then each sequence. A message is erased only if all three of subject, producer and tenant match; otherwise it is `refused`. A message whose payload is not an envelope is `refused` too. A transient failure of the broker request returns a Nak so the command is redelivered.
- **Audit without content.** `identity.audit_events` has no column for counts (by design: no payload), so each command writes one `operations.stream_erasure_audit` row with the producer, owner, request reference and the three counts, and also one `identity.audit_events` row (`stream.erasure`, allowed when nothing was refused, denied otherwise) that the counts row names by `audit_event_id`. Neither holds a body or a stream sequence.
- **A transient failure is a Nak.** `consumer::run` now negatively acknowledges a message whose handler returned an error with a 2 second delay (CONTRACTS.md S02 rule 7), instead of leaving it for the 30 second ack wait. A rejection is an outcome, not an error, so a Termed command is recorded `rejected` in the inbox and acknowledged. If a transient failure comes after some sequences were erased, the rollback drops the audit row and the redelivery counts those sequences `missing`; the erasure itself is idempotent.
- **Invariant 7.** The static ACL test asserts that no stanza but EDGE holds a publish allow starting with `$JS.API.STREAM.` except the browser worker's single `STREAM.INFO.KV_browser_worker_completions` read, which S03 already fixes and which cannot touch messages.

## Risks / Trade-offs

- A producer can erase its own facts for the tenant it names. That is the point of the feature; the residual risk is recorded in the README: a compromised identity could already publish forged facts.
- The erasure is eventual: the producer enqueues the command in its deletion transaction and Edge handles it when it can.
