## Purpose

Provides restart-safe, operation-owned archive staging so an authenticated export device can resume
only missing chunks and deliver verified bytes to the provider already bound to the operation.

## ADDED Requirements

### Requirement: Preparation opens one resumable transfer for one operation

Platform SHALL prepare and open a bounded archive transfer against one immutable operation, owner,
active export-agent device, provider, media type, byte size, digest and chunk declaration. Repeating
the same request SHALL return the original operation and current transfer state. An expired transfer
MAY be replaced under the same operation and immutable declaration, but recovery SHALL NOT create a
second operation.

#### Scenario: Restart resumes only missing chunks
- **WHEN** a device restarts after Platform acknowledged a subset of declared chunks
- **THEN** transfer status identifies the acknowledged chunks and the device completes the original
  operation by sending only the missing chunks

#### Scenario: An expired transfer is replaced inside its operation
- **WHEN** the bound device reopens an expired, non-finalized transfer with the identical declaration
- **THEN** Platform returns a fresh bounded transfer session for the existing operation

### Requirement: Transfer access is idempotent and authority-scoped

Platform SHALL authorize every transfer request through the common device authentication boundary
and SHALL disclose transfer state only when the authenticated owner, active export-agent device,
provider and operation all match the prepared binding. An identical chunk replay SHALL return its
recorded acknowledgement, while different bytes for an acknowledged chunk SHALL conflict without
changing stored state. A valid but foreign owner, device or provider SHALL receive the same bounded
not-found response. A revoked credential SHALL receive the common authentication rejection.

#### Scenario: Identical and divergent chunk replay
- **WHEN** the bound device repeats an acknowledged chunk and then attempts the same index with
  different content
- **THEN** the identical replay receives the original acknowledgement and the divergent replay
  conflicts without replacing the chunk

#### Scenario: A valid foreign binding is hidden
- **WHEN** a valid credential requests a transfer owned by another owner, device or provider
- **THEN** Platform returns bounded not-found without disclosing which binding was wrong

#### Scenario: A revoked credential is unauthenticated
- **WHEN** a revoked device credential requests transfer status or writes a chunk
- **THEN** the common authentication boundary rejects the request and stored chunks do not change

### Requirement: Finalization verifies before fixed-route delivery

Platform SHALL assemble declared chunks in order, stream-verify their exact byte size and digest,
and call only the fixed receipt route of the provider bound to the operation. Platform SHALL inject
the bound operation and verified declaration headers and SHALL NOT forward caller-supplied identity
or operation headers. A verification failure SHALL make no upstream request. Transport uncertainty
SHALL preserve enough state to retry idempotently without creating a second operation.

#### Scenario: Digest mismatch never reaches a provider
- **WHEN** finalization assembles bytes that do not match the bound declaration
- **THEN** Platform rejects finalization and makes no provider request

#### Scenario: Verified bytes reach only the bound provider
- **WHEN** all chunks match the immutable declaration
- **THEN** Platform sends the verified archive only to the bound provider receipt route with
  Platform-minted operation and declaration headers
