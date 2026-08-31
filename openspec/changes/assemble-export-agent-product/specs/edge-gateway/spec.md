## ADDED Requirements

### Requirement: Archive receipt routes are fixed and independently available

The single-host gateway SHALL route ChatGPT receipts to `127.0.0.1:8096` and Claude receipts to
`127.0.0.1:8097` at the fixed receipt path. Configuration SHALL reject a missing route, a provider
mapped to the other provider's service, or a port collision. Preparation SHALL refuse only the
provider whose receipt or terminal-report path is unavailable.

#### Scenario: Both fixed provider routes are configured
- **WHEN** the production Edge configuration is validated
- **THEN** ChatGPT and Claude resolve to their distinct canonical loopback receipt routes

#### Scenario: One provider loses receipt readiness
- **WHEN** one configured provider receiver becomes unavailable while the other remains healthy
- **THEN** preparation for the unavailable provider returns bounded not-found and the healthy
  provider remains available
