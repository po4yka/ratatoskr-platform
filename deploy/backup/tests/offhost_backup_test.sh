#!/usr/bin/env bash
set -euo pipefail

backup_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
backup_script="$backup_root/ratatoskr-offhost-backup.sh"
lifecycle_script="$backup_root/ratatoskr-offhost-lifecycle.sh"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

make_fake_commands() {
    local bin="$1/bin"

    mkdir -p "$bin"
    # shellcheck disable=SC2016 # literal script text is intentionally written for the fake command.
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        'set -euo pipefail' \
        'root="${MOCK_S3_ROOT:?}"' \
        'if [[ "$1" == s3 && "$2" == cp ]]; then' \
        '    source_path="$3"' \
        '    destination="$4"' \
        '    key="${destination#s3://}"' \
        '    mkdir -p "$root/$(dirname "$key")"' \
        '    cp "$source_path" "$root/$key"' \
        '    exit 0' \
        'fi' \
        'printf "unexpected aws invocation: %s\\n" "$*" >&2' \
        'exit 64' > "$bin/aws"
    # shellcheck disable=SC2016 # literal script text is intentionally written for the fake command.
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        'set -euo pipefail' \
        'if [[ "$1" == list ]]; then' \
        '    printf "%s\\n" "daily-20260827"' \
        '    exit 0' \
        'fi' \
        'if [[ "$1" == export-tar ]]; then' \
        '    cp "${MOCK_BORG_EXPORT:?}" "$3"' \
        '    exit 0' \
        'fi' \
        'printf "unexpected borg invocation: %s\\n" "$*" >&2' \
        'exit 64' > "$bin/borg"
    chmod 0755 "$bin/aws" "$bin/borg"
}

make_fixture() {
    local root="$1"
    local config="$root/config"

    mkdir -p "$root/dumps" "$root/stage" "$root/s3" "$config/etc/ratatoskr" \
        "$config/etc/systemd/system" "$config/etc/nats" "$config/etc/logrotate.d"
    printf 'custom PostgreSQL dump fixture\n' > "$root/dumps/ratatoskr-20260827T023000Z.dump"
    printf 'borg recovery material\n' > "$root/borg-export.tar"
    printf 'edge configuration\n' > "$config/etc/ratatoskr/edge.conf"
    printf 'ingest configuration\n' > "$config/etc/ratatoskr/ingest.conf"
    printf 'scheduler configuration\n' > "$config/etc/ratatoskr/scheduler.conf"
    printf 'edge unit\n' > "$config/etc/systemd/system/ratatoskr-edge.service"
    printf 'ingest unit\n' > "$config/etc/systemd/system/ratatoskr-ingest.service"
    printf 'scheduler unit\n' > "$config/etc/systemd/system/ratatoskr-scheduler.service"
    printf 'nats configuration\n' > "$config/etc/nats/ratatoskr.conf"
    printf 'nats compose\n' > "$config/etc/nats/compose.yaml"
    printf 'logrotate configuration\n' > "$config/etc/logrotate.d/ratatoskr"
    printf 'AWS_SECRET_ACCESS_KEY=not-for-archive\n' > "$config/etc/ratatoskr/offhost-backup.env"
    age-keygen -o "$root/identity.txt" > /dev/null 2>&1
}

run_replication() {
    local root="$1"
    local recipient

    shift
    recipient="$(awk '/public key/ { print $4 }' "$root/identity.txt")"
    env \
        PATH="$root/bin:$PATH" \
        MOCK_S3_ROOT="$root/s3" \
        MOCK_BORG_EXPORT="$root/borg-export.tar" \
        RATATOSKR_OFFHOST_DUMP_DIR="$root/dumps" \
        RATATOSKR_OFFHOST_STAGE_DIR="$root/stage" \
        RATATOSKR_OFFHOST_CONFIG_ROOT="$root/config" \
        RATATOSKR_OFFHOST_BUCKET="backup-test" \
        RATATOSKR_OFFHOST_PREFIX="ratatoskr-platform" \
        RATATOSKR_OFFHOST_AGE_RECIPIENT="$recipient" \
        RATATOSKR_BORG_REPOSITORY="fixture-repository" \
        "$backup_script" --date 2026-08-27 "$@"
}

replication_encrypts_and_round_trips_dump() {
    local root
    local encrypted_dump
    local restored_dump

    root="$(mktemp -d "${TMPDIR:-/tmp}/ratatoskr-offhost-test.XXXXXX")"
    trap 'rm -rf "$root"' RETURN
    make_fake_commands "$root"
    make_fixture "$root"
    run_replication "$root"

    encrypted_dump="$(find "$root/s3/backup-test" -path '*/postgresql/*.age' -type f -print -quit)"
    [[ -n "$encrypted_dump" ]] || fail 'replication did not upload an encrypted PostgreSQL dump'
    ! grep -q 'custom PostgreSQL dump fixture' "$encrypted_dump" || fail 'uploaded dump is plaintext'
    restored_dump="$root/restored.dump"
    age --decrypt --identity "$root/identity.txt" --output "$restored_dump" "$encrypted_dump"
    cmp "$root/dumps/ratatoskr-20260827T023000Z.dump" "$restored_dump"
    [[ "$(find "$root/s3/backup-test" -path '*/borg/*.age' -type f | wc -l | tr -d '[:space:]')" == 1 ]] || \
        fail 'replication did not upload one encrypted Borg export'
}

replication_rejects_incomplete_input() {
    local root

    root="$(mktemp -d "${TMPDIR:-/tmp}/ratatoskr-offhost-test.XXXXXX")"
    trap 'rm -rf "$root"' RETURN
    make_fake_commands "$root"
    make_fixture "$root"
    rm "$root/dumps/ratatoskr-20260827T023000Z.dump"
    if run_replication "$root"; then
        fail 'replication accepted a missing dump'
    fi
    [[ ! -e "$root/s3/backup-test" ]] || fail 'replication uploaded despite missing input'
}

replication_rejects_dump_from_another_utc_day() {
    local root

    root="$(mktemp -d "${TMPDIR:-/tmp}/ratatoskr-offhost-test.XXXXXX")"
    trap 'rm -rf "$root"' RETURN
    make_fake_commands "$root"
    make_fixture "$root"
    mv "$root/dumps/ratatoskr-20260827T023000Z.dump" "$root/dumps/ratatoskr-20260826T023000Z.dump"
    if run_replication "$root"; then
        fail 'replication accepted a dump from a different UTC day'
    fi
    [[ ! -e "$root/s3/backup-test" ]] || fail 'replication uploaded a stale dump under today prefix'
}

configuration_snapshot_is_allowlisted_and_excludes_recovery_credentials() {
    local root
    local encrypted_config
    local config_tar

    root="$(mktemp -d "${TMPDIR:-/tmp}/ratatoskr-offhost-test.XXXXXX")"
    trap 'rm -rf "$root"' RETURN
    make_fake_commands "$root"
    make_fixture "$root"
    run_replication "$root"

    encrypted_config="$(find "$root/s3/backup-test" -path '*/configuration/*.age' -type f -print -quit)"
    [[ -n "$encrypted_config" ]] || fail 'replication did not upload an encrypted configuration snapshot'
    config_tar="$root/config.tar"
    age --decrypt --identity "$root/identity.txt" --output "$config_tar" "$encrypted_config"
    tar --list --file "$config_tar" | grep -qx 'etc/ratatoskr/edge.conf' || \
        fail 'configuration snapshot omitted the allowlisted edge configuration'
    ! tar --list --file "$config_tar" | grep -q 'offhost-backup.env' || \
        fail 'configuration snapshot included recovery credentials'
}

replication_dry_run_does_not_upload() {
    local root
    local output

    root="$(mktemp -d "${TMPDIR:-/tmp}/ratatoskr-offhost-test.XXXXXX")"
    trap 'rm -rf "$root"' RETURN
    make_fake_commands "$root"
    make_fixture "$root"
    output="$(run_replication "$root" --dry-run)"
    grep -Fq 'DRY-RUN: encrypt and upload' <<< "$output" || fail 'replication dry-run did not report work'
    [[ ! -e "$root/s3/backup-test" ]] || fail 'replication dry-run uploaded an object'
    [[ ! -e "$root/stage/.offhost" ]] || fail 'replication dry-run staged recovery material'
}

lifecycle_policy_covers_all_prefixes_for_ninety_days() {
    local policy

    policy="$($lifecycle_script --remote-keep-days 90)"
    for prefix in postgresql borg configuration; do
        grep -Fq "\"Prefix\": \"ratatoskr-platform/*/$prefix/\"" <<< "$policy" || \
            fail "lifecycle policy omitted $prefix prefix"
    done
    [[ "$(grep -Fc '"Days": 90' <<< "$policy")" == 3 ]] || \
        fail 'lifecycle policy did not retain every recovery prefix for ninety days'
    grep -Fq '"DaysAfterInitiation": 7' <<< "$policy" || \
        fail 'lifecycle policy did not abort incomplete multipart uploads after seven days'
}

lifecycle_policy_example_matches_generator() {
    diff -u "$backup_root/offhost-lifecycle-90-days.json" <("$lifecycle_script" --remote-keep-days 90) || \
        fail 'checked lifecycle example drifted from the generator'
}

remote_retention_is_independent_of_local_keep_count() {
    local default_policy
    local changed_local_policy

    default_policy="$($lifecycle_script --remote-keep-days 90)"
    changed_local_policy="$(RATATOSKR_BACKUP_KEEP=1 "$lifecycle_script" --remote-keep-days 90)"
    [[ "$default_policy" == "$changed_local_policy" ]] || \
        fail 'local dump keep count changed the remote lifecycle policy'
}

replication_encrypts_and_round_trips_dump
replication_rejects_incomplete_input
replication_rejects_dump_from_another_utc_day
configuration_snapshot_is_allowlisted_and_excludes_recovery_credentials
replication_dry_run_does_not_upload
lifecycle_policy_covers_all_prefixes_for_ninety_days
lifecycle_policy_example_matches_generator
remote_retention_is_independent_of_local_keep_count
printf 'PASS: off-host replication tests\n'
