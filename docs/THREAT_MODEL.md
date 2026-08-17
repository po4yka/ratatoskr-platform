# Platform threat model

## Assets

Identity, sessions, device credentials, authorization, operation integrity, captures, public API availability, service commands, and audit records.

## Threats and controls

- **Authentication bypass:** secure sessions/tokens, exact validation, rotation, revocation.
- **Replay:** nonce, expiry, idempotency, assertion/device binding.
- **Privilege escalation:** deny-by-default ownership and capability checks.
- **Operation spoofing:** authenticated service producers and valid state transitions.
- **Ingress/upload abuse:** rate, body, count, MIME, concurrency, and storage limits.
- **Event forgery:** constrained NATS subjects/service identities and schema validation.
- **Information disclosure:** authorization before existence, safe errors, redacted telemetry.
- **DoS/cost fan-out:** bounded queues, quotas, backpressure, cancellation, and per-principal limits.

Residual risks include compromised clients/administrators and dependency zero-days. Re-review for new auth methods, public routes, identity assertions, file types, or multi-tenancy.
