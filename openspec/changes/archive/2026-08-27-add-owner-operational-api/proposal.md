## Why

Operators currently cannot inspect deployment-wide operation, schedule, or audit state through the
public Edge boundary, and users have no anonymous status page source. The fleet change
`add-operational-status-workspace-integration` defines the shared behavior and rollout order; this
producer change implements the Platform portion against Contracts commit
`9a4df8126b495ffc3ad0647441da1690594f25bc`.

## What Changes

- Publish three operational capabilities only while the authenticated user holds the live
  `platform.owner` grant.
- Add bounded owner-only operation, schedule, and redacted audit queries under `/v1/admin/`.
- Add anonymous `/v1/status` as a cached, sanitized projection of Platform observations with no
  request-time dependency calls.
- Generate the OpenAPI from the shared operational contracts and document provisioning, privacy,
  and the difference between public status and operator health.

## Capabilities

### New Capabilities

- `owner-operational-api`: Live owner authorization, bounded operational inspection routes, and
  capability discovery behavior.
- `public-status-api`: Anonymous sanitized status projection and cache behavior.

### Modified Capabilities

None.

## Impact

The change affects the public API crate, operation/identity database query adapters, RuntimeState
readiness observations, OpenAPI generation, and Platform documentation. It adds no schema or
migration and exposes no service-private payload, credential, address, or diagnostic text.
