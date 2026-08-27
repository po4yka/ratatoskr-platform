## 1. Contract-backed projection

- [x] 1.1 RED: add an operation-projection integration test with a valid typed AI archive summary;
  run it against PostgreSQL and verify it fails at the target namespace constraint.
- [x] 1.2 GREEN: pin the published contract revision, align current schema entity-reference checks
  with the canonical grammar, and update result literals; rerun the focused projection test green.

## 2. Full validation and lifecycle

- [x] 2.1 Run the complete `DEVELOPMENT.md` gate through `build-gate`, including debug/release
  builds and the full test suite; verify every command result.
- [x] 2.2 Sync the delta spec, archive this change, and run `openspec validate --archived`.
