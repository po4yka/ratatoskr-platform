## 1. Command routes

- [x] 1.1 Add `crates/public-api/tests/channel_digests.rs` with the six command tests (accepted and enqueued, 24 hour closed-open window, replay without a second row, 409 on a different body, 400 on a bad username, 400 without a key) and run them RED (404 today)
- [x] 1.2 Add `crates/public-api/src/channel_digests.rs`, register the routes and schemas, regenerate `openapi/openapi.json` and run them GREEN

## 2. Read routes and configuration

- [x] 2.1 Add the config tests in `crates/core/tests/config_archive_and_digests.rs` (`channel_digests_requires_loopback_8098_and_a_secret_together`, `secret_is_redacted_in_debug_and_check_config`) and `crates/public-api/tests/channel_digests_reads.rs` against a stub loopback server (`upstream_500_is_upstream_invalid_response` replaces the work order's `..._upstream_unavailable` name, because CONTRACTS.md S08 maps any non-2xx other than 404 to an invalid upstream response) and run them RED
- [x] 2.2 Add the optional `channel_digests` section, the bounded client, the three read handlers and their documents, document both keys in `.env.example` and `edge.conf.example`, regenerate `openapi/openapi.json` and run them GREEN

## 3. Scheduler and registration

- [x] 3.1 Add `crates/scheduling/tests/publication.rs::digest_occurrence_is_published_as_a_contract_envelope`, `previous_due_at_is_clamped_to_seven_days_for_a_sparse_cron` and `crates/scheduling/tests/registration.rs::typed_registration_from_contracts_is_accepted_for_channel_digests_and_rejected_for_other_producers` and run them RED
- [x] 3.2 Select the envelope builder by command type, parse the typed registration, set the registrar allowlist in the examples, update `openspec/specs/schedule-registration/spec.md` through this change's delta and run them GREEN

## 4. Documentation and final gate

- [x] 4.1 State in `DEVELOPMENT.md` and `deploy/README.md` which routes are real, the schedule-owner operator prerequisite, the new keys, the `ratatoskr-channel-digests` registrar, the capture and blob routes and the 16 MiB chunk note; this is documentation and cannot start from a failing test
- [x] 4.2 Regenerate `openapi/openapi.json` a last time and run the full gate from `DEVELOPMENT.md` plus `cargo deny --locked check` and `openspec validate --all --strict`; verification only, no behaviour
