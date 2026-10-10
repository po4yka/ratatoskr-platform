## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md section S08. The contract types (`ChannelDigestSubscriptionSetRequested`, `ChannelDigestRunRequested`, `ChannelDigestScheduleOccurrenceRequested`, the view types and `PlatformScheduleRegistrationRequested`) are owned by `ratatoskr-contracts`; Platform builds and parses them and defines none of its own.

## Decisions

- **Commands follow `captures.rs`.** Reserve the idempotency key under the route template, accept the operation (kinds `channel_digest.subscription.set` and `channel_digest.run`), build the envelope, enqueue it with the command id as the outbox message id, complete the ledger and write the audit record in one transaction. The payload's idempotency key is derived from the operation id so an HTTP retry (replayed by the ledger) and an outbox redelivery both collapse in channel-digests' inbox.
- **A run window is 24 hours closed-open ending at acceptance.** The trigger carries the same instant as the window end.
- **Reads use a dedicated typed client, not the gateway proxy.** The generic proxy strips `Authorization` by design; the channel-digests API is bearer-protected, so the reads go through a fixed-path client (bounded request, no redirects, response cap 262144 bytes) and the three failure shapes are explicit: upstream 404 is 404, transport failure is unavailable or timeout, anything else is an invalid upstream response.
- **Config is all or nothing.** The listener (loopback, port 8098) and the secret are set together or both absent; absent makes the read routes answer upstream-unavailable while the command routes keep working.
- **The scheduler selects the envelope by command type.** `previous_due_at` is the cron grid point strictly before the due time, clamped to seven days, using `cron` 0.17 `after(..).next_back()`.

## Risks / Trade-offs

- The occurrence owner must exist in `identity.users`; Platform has no system principal. This is an operator prerequisite and is documented.
