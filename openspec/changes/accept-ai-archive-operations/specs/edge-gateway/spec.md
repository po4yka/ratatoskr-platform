## MODIFIED Requirements

### Requirement: Identity crosses only as minted claims
The proxy SHALL strip every inbound `x-ratatoskr-*` header and forward only the authenticated user id,
optional device id, and minted correlation id. For an accepted AI archive transfer, it SHALL also
forward the Platform-minted operation id; callers cannot choose or replace that id.

#### Scenario: Client-supplied identity headers are replaced
- **WHEN** an authenticated request includes forged `x-ratatoskr-*` headers
- **THEN** the downstream receives only Edge-minted claims for the authenticated principal

#### Scenario: An archive transfer carries its minted operation identifier
- **WHEN** Edge forwards an accepted archive transfer to its configured provider receiver
- **THEN** the receiver receives the operation identifier created for that request and no
  caller-supplied operation header
