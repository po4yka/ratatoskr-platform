## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md sections R2-02, R2-04, R2-14, R2-11, R2-18, R2-10 and R2-01. The contract text is the source of truth; this file records the Platform-side decisions.

## Decisions

- **One table in `platform_operations`.** `authority.rs` holds `AUTHORITY: &[(&str, Producers)]` and `authorize(kind, producer, envelope_tenant, owner)`. `Producers::Listed` names the producers that may report for a kind; `Producers::ProviderBound` marks `ai_archive.import`, whose producer is checked by the existing acceptance binding (subject and producer for the bound provider). A kind that is not in the table allows no producer.
- **The projection loads the operation first.** `find` runs inside the handler transaction before `record_status`, so the tenant and kind compared are the ones the status write would apply to. An unknown operation is rejected as before. The log line carries the operation id and a class (`tenant_mismatch`, `producer_not_allowed`), never the report.
- **Tenant is read from the envelope.** `tenant_id` at the top of the envelope must parse as a `TenantRef` equal to the owner. A bare report with no envelope has no tenant and is rejected: this is the intended break.
- **The minted-kinds test reads sources.** `every_minted_operation_kind_has_an_authority_row` scans the kind constants in `public-api` and `Target::operation_kind` in `ingest` from the test, because `platform_operations` cannot depend on those crates. The schedule kind registered by channel-digests has a row and a direct assertion.
- **`max_payload: 8MB`.** NATS parses the unit as 8388608 bytes. JetStream stream `max_msg_size` stays unlimited. A Docker-free test strips comments from the deployed file and asserts the line; the real-broker test publishes 4 MiB as the X identity.
- **Owner check in the registration transaction.** One `select status from identity.users where user_id = $1` before the upsert, so a user suspended between the check and the upsert cannot matter.

## Risks / Trade-offs

- Producers of the same family can still report for each other's capture operations. Documented in `ARCHITECTURE.md`, not fixed here.
- The Docker real-broker test cannot run on a machine without Docker; CI runs it.
