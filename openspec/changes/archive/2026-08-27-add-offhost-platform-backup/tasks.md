## 1. Encrypted replication and configuration snapshot

- [x] 1.1 Add `deploy/backup/tests/offhost_backup_test.sh` cases
  `replication_encrypts_and_round_trips_dump` and `replication_rejects_incomplete_input`; run them
  before implementation and confirm they fail because `ratatoskr-offhost-backup.sh` is absent, not
  because the generated age fixture or fake S3 command is invalid.
- [x] 1.2 Implement `deploy/backup/ratatoskr-offhost-backup.sh` with explicit source validation,
  age encryption, immutable S3 object upload, ciphertext digest metadata, cleanup, and `--dry-run`;
  verify the two replication tests pass and that their fake target contains no plaintext dump.
- [x] 1.3 Add the failing `configuration_snapshot_is_allowlisted_and_excludes_recovery_credentials`
  case in `deploy/backup/tests/offhost_backup_test.sh`; run it and confirm the assertion fails before
  a configuration-snapshot implementation exists.
- [x] 1.4 Add the explicit configuration allowlist, root-only environment examples, and snapshot
  creation to `ratatoskr-offhost-backup.sh`; verify the configuration test passes and neither the
  S3 upload environment nor any age identity is archived.
- [x] 1.5 Add the failing `replication_rejects_dump_from_another_utc_day` case in
  `deploy/backup/tests/offhost_backup_test.sh`; run it and confirm it fails because a previous-day
  dump can currently be selected for a current-day recovery prefix.
- [x] 1.6 Require the generated dump filename's UTC date to match the recovery-set date before
  staging or upload; verify the stale-dump test and the existing replication suite pass.

## 2. Remote retention policy

- [x] 2.1 Add the failing `lifecycle_policy_covers_all_prefixes_for_ninety_days` and
  `remote_retention_is_independent_of_local_keep_count` cases in
  `deploy/backup/tests/offhost_backup_test.sh`; run them and confirm they fail because the policy
  generator is absent, not because fixture JSON parsing is broken.
- [x] 2.2 Implement `deploy/backup/ratatoskr-offhost-lifecycle.sh` and its checked policy example;
  verify the retention tests pass, the policy covers dump/Borg/config prefixes, retains them for
  ninety days, and aborts incomplete multipart uploads after seven days.

## 3. Off-host verification drill

- [x] 3.1 Add Docker-backed failing cases `drill_decrypts_and_restores_a_custom_dump` and
  `drill_reports_fail_stage_for_missing_or_undecryptable_object` in
  `deploy/backup/tests/offhost_drill_test.sh`; run them against a PostgreSQL 17 scratch container
  and confirm they fail because `ratatoskr-offhost-drill.sh` is absent, not because Docker or the
  generated dump fixture failed.
- [x] 3.2 Implement `deploy/backup/ratatoskr-offhost-drill.sh` with S3 download, age decryption,
  ICU scratch-database restore, schema/constraint verification, cleanup, `PASS`/`FAIL: <stage>`
  output, and `--dry-run`; verify both drill tests pass.
- [x] 3.3 Add the root-only verifier environment example and
  `ratatoskr-offhost-drill.{service,timer}` for the separate off-host verifier host; this is
  environment-bound infrastructure, so it cannot start with a unit test; verify `systemd-analyze
  verify` accepts both units and the drill's fixture-backed `--dry-run` performs no S3 or database
  write.

## 4. Pi scheduling and CI enforcement

- [x] 4.1 Add `ratatoskr-offhost-backup.{service,timer}` scheduled after the established Borg
  window with the narrow filesystem/network privileges needed for NVMe staging and S3 upload; this
  is environment-bound infrastructure, so it cannot start with a unit test; verify
  `systemd-analyze verify` accepts both units and the fixture-backed `--dry-run` does not upload.
- [x] 4.2 Add the backup shell-test runner and ShellCheck invocation to `.github/workflows/ci.yml`;
  verify CI-equivalent execution installs the required test tools, lints every backup shell script,
  and runs the encryption, retention, and real scratch-restore tests.

## 5. Runbook and recorded dry-run evidence

- [x] 5.1 Update `deploy/backup/README.md`, `deploy/README.md`, and `DEVELOPMENT.md` with package
  installation, root-readable environment-file permissions, least-privilege bucket policy,
  lifecycle application/inspection, verifier installation, remote-only replacement-board restore,
  local-versus-remote retention, and the written rationale for deferring WAL shipping; documentation
  cannot start with a failing unit test, so verify every command has a declared dry-run or a
  destructive-action warning and no committed value resembles a credential.
- [x] 5.2 Record fixture-backed Pi replication and verifier drill dry-run output in this OpenSpec
  change without secrets; this is required deployment evidence rather than product behavior; verify
  the evidence identifies the command, date, fixture target, exit status, and that no live S3 target
  was contacted.

## 6. Final validation and lifecycle

- [x] 6.1 Run `openspec validate add-offhost-platform-backup --strict`, the backup test runner,
  ShellCheck, and the full repository gate from `DEVELOPMENT.md` (using `build-gate --` for each
  compiler-backed Cargo command); verify every command exits zero and inspect the final diff for
  credentials, plaintext recovery artifacts, and scope creep.
- [x] 6.2 Verify archive readiness: all implementation tasks are checked and the delta specification
  is ready to sync to `openspec/specs/`; verify `openspec validate
  add-offhost-platform-backup --strict` passes. Archive is the next OpenSpec lifecycle action and
  must be followed by `openspec validate --archived` before commit.
