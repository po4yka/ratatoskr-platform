## Why

Telegram must consume the canonical notification fact without receiving broad JetStream administration authority. Platform already owns the `ratatoskr_events` stream and deployment-time NATS topology, so it must pre-provision the exact durable consumer and least-privilege service identity before Telegram can start safely.

## What Changes

- Pre-provision the durable pull consumer `ratatoskr_telegram_notifications` on `ratatoskr_events`, filtered exclusively to `evt.platform.notification.raised.v1`.
- Extend the Platform-owned NATS deployment configuration with a Telegram service NKey whose permissions are limited to inspecting, fetching from, and acknowledging that durable consumer plus its request inbox.
- Validate the configured durable name, stream, filter, acknowledgement policy, and permissions through executable configuration tests using a real local JetStream server and synthetic NKeys.
- Make edge startup ensure the Telegram consumer idempotently alongside the other Platform-owned stream topology, while never consuming notification payloads or holding Telegram credentials.
- Document the seed-file boundary, provisioning order, rollback behavior, and the exact configuration values consumed by the Telegram deployment profile.

## Capabilities

### New Capabilities

- `telegram-notification-consumer-provisioning`: Platform-owned JetStream topology and least-privilege NATS authorization required for the Telegram notification consumer.

### Modified Capabilities

(none)

## Impact

- Affected code: Platform eventing stream/consumer definitions, edge startup provisioning, NATS server configuration, deployment documentation, and configuration/integration tests.
- External systems: the existing local NATS JetStream server and the Telegram dispatcher identity. No public HTTP API, Platform database schema, or notification payload contract changes.
- Security: the Telegram seed remains a root-owned deployment secret outside the repository; the checked-in server configuration contains only the public NKey and exact JetStream API permissions.
- Compatibility is additive. Existing publishers and consumers are unchanged, and an idle pre-provisioned consumer has no delivery side effects before Telegram is deployed.
- Rollback stops Telegram first, removes its public NKey authorization only after no dispatcher uses it, and may retain or explicitly delete the empty durable through the Platform-owned provisioning path.
