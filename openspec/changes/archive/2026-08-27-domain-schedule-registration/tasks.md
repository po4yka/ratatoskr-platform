## 1. Registration contract and admission

- [x] 1.1 Add `crates/scheduling/tests/registration.rs::invalid_cron_registration_is_rejected` and run it; its rejected-outcome and zero-row assertions cover invalid registration input.
- [x] 1.2 Implement the registration command parser, five-field UTC cron validation, and command-subject filtering so task 1.1 passes.
- [x] 1.3 Add `crates/scheduling/tests/registration.rs::unauthorized_producer_registration_is_rejected` and run it; its no-change assertion covers producer authorization.
- [x] 1.4 Implement configured registrar allowlisting and producer/service equality so task 1.3 passes.

## 2. Durable schedule reconciliation

- [x] 2.1 Add `crates/scheduling/tests/registration.rs::redelivery_updates_one_service_named_schedule` and run it; its stable-id, one-row, inbox, and audit assertions cover durable redelivery.
- [x] 2.2 Implement transactional service/name upsert, registration audit decisions, and Edge's filtered command consumer so task 2.1 passes.
- [x] 2.3 Add `crates/scheduling/tests/registration.rs::update_and_disable_keep_the_schedule_identity` and run it; its update-in-place and no-publication assertions cover the transition.
- [x] 2.4 Implement update and disable reconciliation so task 2.3 passes.

## 3. Cron occurrence continuity and visibility

- [x] 3.1 Add `crates/scheduling/tests/registration.rs::due_occurrence_survives_a_schedule_edit_once` and run it; its deterministic occurrence assertion covers the edit boundary.
- [x] 3.2 Replace fixed-interval advancement with cron next-occurrence calculation and preserve an already-due next occurrence during updates so task 3.1 passes.
- [x] 3.3 Add `crates/scheduling/tests/registration.rs::schedule_status_reports_owner_next_due_and_last_outcome` and run it; its status-projection assertion covers visibility.
- [x] 3.4 Add the operator schedule-status projection and documentation so task 3.3 passes.

## 4. Delivery validation

- [x] 4.1 Update configuration, schema, PostgreSQL grants, NATS/deployment guidance, and schedule documentation; this task starts from no RED because it is configuration and documentation required by the implemented dependency.
- [x] 4.2 Run the complete `DEVELOPMENT.md` Rust gate and `openspec validate --strict --archived`; record the observed results before archiving.
