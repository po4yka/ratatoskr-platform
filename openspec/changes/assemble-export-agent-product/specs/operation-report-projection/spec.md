## ADDED Requirements

### Requirement: AI archive terminal reports use provider-bound ingress

Platform SHALL consume the unchanged `platform.operation.reported.v1` envelope from the distinct
ingress subjects `evt.ai-archive.chatgpt.operation.reported.v1` and
`evt.ai-archive.claude.operation.reported.v1`. Before projection it SHALL verify that the ingress
subject, envelope producer and provider bound to the operation identify the same provider. A broker-
accepted mismatch SHALL be rejected without advancing the operation.

#### Scenario: Bound provider report advances its operation
- **WHEN** a ChatGPT or Claude report arrives on its matching ingress subject from its matching
  producer for an operation bound to that provider
- **THEN** Platform applies the unchanged operation report envelope to the owned operation

#### Scenario: Producer or provider mismatch is rejected
- **WHEN** an envelope producer, ingress subject or operation-bound provider disagrees
- **THEN** Platform rejects the projection and leaves the operation unchanged

### Requirement: Provider report permissions are least privilege

The secured bus SHALL give ChatGPT and Claude distinct deployment credentials. Each provider SHALL
publish only its own provider-scoped archive report subject and SHALL NOT publish the other
provider's ingress subject or subscribe to the global event wildcard. Anonymous publication SHALL
be refused. Credential material SHALL remain outside repository files and diagnostics.

#### Scenario: Provider cannot impersonate another provider
- **WHEN** ChatGPT credentials publish to the Claude ingress subject or Claude credentials publish
  to the ChatGPT ingress subject
- **THEN** the secured bus refuses publication

#### Scenario: Anonymous publication is refused
- **WHEN** an unauthenticated client publishes an archive operation report
- **THEN** the secured bus refuses publication

### Requirement: Archive readiness includes the complete report path

Platform SHALL include private staging health, provider receipt reachability and provider report-
consumer health in its existing admin readiness projection. It SHALL expose no new public capability
token for this behavior. A provider route SHALL refuse preparation whenever any dependency required
to complete that provider's operation is unavailable.

#### Scenario: Report consumption failure removes only the affected route
- **WHEN** one provider's durable report consumer is unavailable while the other provider path is
  healthy
- **THEN** the existing admin readiness projection reports the failed dependency and preparation is
  refused only for the affected provider
