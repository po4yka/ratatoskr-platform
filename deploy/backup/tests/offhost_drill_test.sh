#!/usr/bin/env bash
set -euo pipefail

backup_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
drill_script="$backup_root/ratatoskr-offhost-drill.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/ratatoskr-offhost-drill-test.XXXXXX")"
container="ratatoskr-offhost-drill-test-$$"

cleanup() {
    docker rm -f "$container" > /dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

make_fake_aws() {
    mkdir -p "$work/bin"
    # shellcheck disable=SC2016 # literal script text is intentionally written for the fake command.
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        'set -euo pipefail' \
        'root="${MOCK_S3_ROOT:?}"' \
        'if [[ "$1" == s3 && "$2" == cp ]]; then' \
        '    source_path="$3"' \
        '    destination="$4"' \
        '    key="${source_path#s3://}"' \
        '    cp "$root/$key" "$destination"' \
        '    exit 0' \
        'fi' \
        'if [[ "$1" == s3api && "$2" == list-objects-v2 ]]; then' \
        '    object="$(find "$root" -path "*/postgresql/*.age" -type f -print -quit || true)"' \
        '    [[ -n "$object" ]] || { printf "None\\n"; exit 0; }' \
        '    key="${object#"$root/"}"' \
        '    printf "%s\\n" "${key#*/}"' \
        '    exit 0' \
        'fi' \
        'printf "unexpected aws invocation: %s\\n" "$*" >&2' \
        'exit 64' > "$work/bin/aws"
    chmod 0755 "$work/bin/aws"
}

wait_for_postgres() {
    for _ in $(seq 1 60); do
        if docker exec "$container" pg_isready -U postgres -d postgres > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    docker logs "$container" >&2 || true
    fail 'PostgreSQL test container did not become ready'
}

prepare_encrypted_dump() {
    local recipient
    local remote_dir="$work/s3/backup-test/ratatoskr-platform/2026-08-27/postgresql"

    docker run -d --name "$container" -e POSTGRES_PASSWORD=postgres postgres:17 > /dev/null
    wait_for_postgres
    docker exec -i "$container" psql -U postgres -d postgres > /dev/null <<'SQL'
CREATE DATABASE source;
\connect source
CREATE SCHEMA identity;
CREATE SCHEMA operations;
CREATE SCHEMA platform_ingest;
CREATE TABLE identity.users (id uuid PRIMARY KEY, email text UNIQUE NOT NULL);
CREATE TABLE operations.operations (id uuid PRIMARY KEY, user_id uuid NOT NULL REFERENCES identity.users(id));
CREATE TABLE platform_ingest.webhook_sources (id uuid PRIMARY KEY, source text UNIQUE NOT NULL);
SQL
    docker exec -i "$container" pg_dump -U postgres --format=custom source > "$work/source.dump"
    age-keygen -o "$work/identity.txt" > /dev/null 2>&1
    recipient="$(awk '/public key/ { print $4 }' "$work/identity.txt")"
    mkdir -p "$remote_dir"
    age --encrypt --recipient "$recipient" --output "$remote_dir/source.dump.age" "$work/source.dump"
}

run_drill() {
    env \
        PATH="$work/bin:$PATH" \
        MOCK_S3_ROOT="$work/s3" \
        RATATOSKR_OFFHOST_BUCKET="backup-test" \
        RATATOSKR_OFFHOST_PREFIX="ratatoskr-platform" \
        RATATOSKR_OFFHOST_AGE_IDENTITY_FILE="$work/identity.txt" \
        RATATOSKR_OFFHOST_DRILL_CONTAINER="$container" \
        RATATOSKR_OFFHOST_DRILL_DATABASE="ratatoskr_offhost_drill" \
        RATATOSKR_OFFHOST_DRILL_STAGE_DIR="$work/stage" \
        "$drill_script" --date 2026-08-27 "$@"
}

drill_decrypts_and_restores_a_custom_dump() {
    local output

    prepare_encrypted_dump
    output="$(run_drill 2>&1)" || fail "successful drill failed: $output"
    grep -Fxq 'PASS: off-host restore drill' <<< "$output" || fail 'drill did not emit PASS'
    [[ "$(docker exec "$container" psql -U postgres -d postgres -Atc \
        "select to_regclass('ratatoskr_offhost_drill') is null")" == t ]] || \
        fail 'drill left the scratch database behind'
}

drill_reports_fail_stage_for_missing_or_undecryptable_object() {
    local output

    rm -rf "$work/s3"
    if output="$(run_drill 2>&1)"; then
        fail 'drill accepted a missing remote object'
    fi
    grep -Fxq 'FAIL: download' <<< "$output" || fail "missing object did not report download stage: $output"
}

drill_dry_run_does_not_write_a_database() {
    local output

    output="$(run_drill --dry-run)" || fail "drill dry-run failed: $output"
    grep -Fq 'DRY-RUN: download yesterday remote dump' <<< "$output" || fail 'drill dry-run did not report work'
    [[ "$(docker exec "$container" psql -U postgres -d postgres -Atc \
        "select to_regclass('ratatoskr_offhost_drill') is null")" == t ]] || \
        fail 'drill dry-run created a scratch database'
}

make_fake_aws
drill_decrypts_and_restores_a_custom_dump
drill_dry_run_does_not_write_a_database
drill_reports_fail_stage_for_missing_or_undecryptable_object
printf 'PASS: off-host drill tests\n'
