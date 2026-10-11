## 1. AI archive report tenant (R2-02)

- [x] 1.1 Add the report-shape tests (bound report without a tenant, report naming another tenant, acceptance record for another owner, import with no record) to `crates/operations/tests/report_authority.rs` and run them RED on the missing acceptance of the tenantless report
- [x] 1.2 Add the `acceptance_bound` flag to `authorize`, load the owner-matched acceptance record in `ProgressProjection`, and run 1.1 GREEN
- [x] 1.3 Record the rule in `docs/ARCHITECTURE.md` section 11.2. Documentation cannot start from a failing test.
