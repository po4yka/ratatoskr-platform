## ADDED Requirements

### Requirement: Archive transfer authority follows live device authority

Every archive preparation, open, chunk, status and finalize request SHALL require a currently valid
credential for the owner-bound device recorded by the operation. The device SHALL have the active
export-agent kind at request time; preparation-time validity SHALL NOT keep later requests
authorized after revocation.

#### Scenario: Device revocation stops an in-progress transfer
- **WHEN** a device is revoked after one chunk was acknowledged and then requests another chunk
- **THEN** the common authentication boundary rejects the request and the acknowledged state is
  unchanged
