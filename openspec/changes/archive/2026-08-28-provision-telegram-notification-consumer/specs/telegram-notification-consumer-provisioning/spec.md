## Purpose

Defines the Platform-owned JetStream topology and least-privilege authorization that let Telegram consume notification facts without bus administration authority.

## ADDED Requirements

### Requirement: Platform provisions one fixed Telegram notification durable
Platform SHALL idempotently ensure a pull consumer named `ratatoskr_telegram_notifications` on `ratatoskr_events` with filter `evt.platform.notification.raised.v1`, explicit acknowledgements, and no push delivery subject. An existing durable whose name, stream, filter, acknowledgement policy, or delivery mode differs SHALL be reported as a startup error instead of silently accepted or modified.

#### Scenario: Missing durable is created
- **WHEN** Platform starts against the canonical event stream without the Telegram durable
- **THEN** it creates the durable with the fixed name, exact notification subject filter, explicit acknowledgement policy, and pull delivery mode

#### Scenario: Matching durable is reused
- **WHEN** Platform starts against a durable that already matches every required field
- **THEN** startup succeeds without resetting its cursor or recreating it

#### Scenario: Mismatched durable is refused
- **WHEN** Platform starts against a durable with a different filter or delivery policy
- **THEN** startup fails with a safe configuration error naming the durable and does not broaden or replace the stored configuration

### Requirement: Telegram bus identity has least privilege
The Platform-owned NATS configuration SHALL grant the Telegram identity only the request subjects needed to inspect, pull from, and acknowledge `ratatoskr_telegram_notifications`, plus subscription to its private reply inbox. It SHALL NOT grant stream or consumer creation, deletion or purge authority, direct `evt.>` subscription, command publishing, or access to another durable.

#### Scenario: Telegram identity consumes its durable
- **WHEN** the Telegram identity requests consumer information, fetches an available notification, and acknowledges it
- **THEN** the broker permits all three operations and delivers replies through the identity's private inbox

#### Scenario: Telegram identity cannot select another filter
- **WHEN** the Telegram identity attempts to create a consumer or inspect or fetch from another durable
- **THEN** the broker denies the request and exposes no foreign event payload

#### Scenario: Telegram identity cannot publish domain messages
- **WHEN** the Telegram identity attempts to publish an `evt.>` or `cmd.>` message
- **THEN** the broker denies the publish

### Requirement: Provisioning contract is deployment-verifiable
Platform SHALL document the stream, durable, filter, public-NKey placeholder, seed-file ownership boundary, provisioning order, and safe rollback in one deployment profile. Automated structural checks and a real local JetStream test SHALL verify that the documented profile agrees with the runtime constants and effective broker permissions.

#### Scenario: Deployment configuration matches runtime topology
- **WHEN** the structural deployment test reads the NATS configuration and operator documentation
- **THEN** every Telegram stream, durable, filter, permission subject, and seed-file path agrees with the runtime declaration

#### Scenario: Seed remains outside repository evidence
- **WHEN** the deployment profile is validated
- **THEN** checked-in artifacts contain only a public-NKey placeholder and a root-owned seed-file path, never a usable seed or credential value

#### Scenario: Broker permission test uses synthetic identity
- **WHEN** the integration test starts a local JetStream server with generated synthetic NKeys
- **THEN** it proves allowed Telegram fetch and acknowledgement operations and refusal of consumer creation, foreign-durable access, and domain publishing without any production credential
