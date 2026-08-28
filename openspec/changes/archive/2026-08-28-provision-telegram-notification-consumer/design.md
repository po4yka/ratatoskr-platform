## Context

See `proposal.md` for motivation. Platform already creates the bounded `ratatoskr_events` stream and fixed provider consumers at edge startup. Its checked-in NATS configuration is the reviewed authority for service permissions, while seed files are host-only secrets. Telegram will be a separate process and repository, so it cannot share edge's broad JetStream control-plane credential.

The cross-repository behavior is coordinated by workspace change `telegram-notification-deployment-integration`; this local change owns only Platform's topology and NATS authorization. The notification payload and event type already exist in `ratatoskr-notification-contracts`.

## Goals / Non-Goals

**Goals:**

- Give Telegram the minimum bus authority needed to resume one durable notification cursor.
- Make consumer drift a deterministic startup failure rather than an implicit filter change.
- Prove the effective permission set against a real local JetStream server with disposable credentials.

**Non-Goals:**

- Consuming, validating, routing, or persisting notification payloads in Platform.
- Producing new notification facts or changing their contract.
- Holding Telegram bot/webhook credentials or deploying Telegram binaries.
- Reconciling a mismatched existing durable automatically.

## Decisions

### 1. Edge pre-provisions an event consumer from a fixed inventory

Add an event-consumer specification with constants for stream, durable name, filter subject, pull mode, and explicit acknowledgements. Edge ensures it after ensuring `ratatoskr_events`, using the same fail-on-consumer-drift pattern as fixed social command consumers.

This keeps topology creation with the process that already owns stream creation. Allowing Telegram to call `$JS.API.CONSUMER.CREATE.>` was rejected because it could choose a broader filter and observe unrelated events. A manually created durable was rejected because deployment correctness would then depend on undocumented host state.

### 2. Existing consumer cursor is never reset by startup

`get_or_create_consumer` may return an existing consumer, after which Platform compares all security- and delivery-relevant fields. A match succeeds without deletion or recreation; any mismatch fails readiness with a safe error.

Automatic delete/recreate was rejected because it can discard or replay the cursor and turns a configuration rollout into a data-affecting operation. The runbook must make any deliberate removal explicit.

### 3. Telegram receives exact JetStream API subjects

The Telegram user stanza permits only:

- `$JS.API.CONSUMER.INFO.ratatoskr_events.ratatoskr_telegram_notifications`;
- `$JS.API.CONSUMER.MSG.NEXT.ratatoskr_events.ratatoskr_telegram_notifications`;
- `$JS.ACK.ratatoskr_events.ratatoskr_telegram_notifications.>`;
- `_INBOX.>` subscription for request replies.

It has no direct data subscription and no publish authority outside the JetStream request/ack subjects above. Reusing edge's identity was rejected because it would add `cmd.>` publishing and arbitrary `$JS.API.>` authority to the Telegram compromise boundary.

### 4. Permission behavior is tested at both structural and broker levels

Extend the deployment-profile test to compare checked-in configuration/documentation with exported runtime constants. Add a real-NATS integration test that generates disposable edge and Telegram NKeys, renders a temporary server configuration, provisions the durable through edge authority, and proves both the allow and deny matrix through the same async NATS client version used in production.

Text-only assertions were rejected as the sole proof because NATS permissions apply to request subjects that are easy to spell correctly yet authorize incorrectly. Production seeds or host state are never used.

### 5. Provisioning is a prerequisite, not runtime coupling

Rollout creates the stream and durable and reloads the NATS public-key configuration before starting Telegram. Platform does not wait for Telegram and an idle durable has no user-visible effect. Telegram readiness independently verifies that its fixed durable exists.

This avoids a cross-service startup RPC and permits Platform and Telegram to restart independently.

## Risks / Trade-offs

- **[Client request subjects change in a future async-nats release]** → Pin the workspace dependency, validate permissions with that exact client, and treat a dependency bump as a deployment-profile change.
- **[A pre-provisioned idle durable retains notifications before Telegram is deployed]** → Keep the bounded seven-day event-stream retention and deploy Telegram immediately after provisioning; composed evidence records the order.
- **[A mismatched durable blocks Platform edge startup]** → Emit the durable name and mismatched safe field, document read-only inspection and explicit repair, and never auto-delete cursor state.
- **[NKey configuration contains a copy/paste error]** → Require the structural test and real-broker permission matrix before the repository gate.

## Migration Plan

1. Publish Platform code and NATS configuration with the fixed consumer inventory and Telegram public-NKey placeholder replaced during host preparation.
2. Generate the Telegram user NKey on the host, install only its seed at the documented root-owned path, and reload NATS with the public key.
3. Restart edge so it idempotently creates/verifies the durable; inspect consumer state before starting Telegram.
4. Deploy Telegram with its seed-file path and verify private readiness plus composed notification flow.
5. To roll back, stop Telegram first and preserve the durable cursor. Remove Telegram authorization only after the dispatcher is stopped. Retain the durable for a near-term rollback; delete it only as an explicit Platform operator action after accepting loss of its cursor/backlog.
