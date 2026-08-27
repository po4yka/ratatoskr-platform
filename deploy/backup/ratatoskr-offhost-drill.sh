#!/usr/bin/env bash
# Restore the prior UTC day's encrypted off-host PostgreSQL dump into a disposable verifier database.
set -euo pipefail

usage() {
    printf 'usage: %s [--date YYYY-MM-DD] [--dry-run]\n' "${0##*/}" >&2
}

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}

day="$(date -u -v-1d +%F 2> /dev/null || date -u -d yesterday +%F)"
dry_run=false
while (($# > 0)); do
    case "$1" in
        --date)
            (($# >= 2)) || { usage; exit 64; }
            day="$2"
            shift 2
            ;;
        --dry-run)
            dry_run=true
            shift
            ;;
        *)
            usage
            exit 64
            ;;
    esac
done

bucket="${RATATOSKR_OFFHOST_BUCKET:?RATATOSKR_OFFHOST_BUCKET is required}"
prefix="${RATATOSKR_OFFHOST_PREFIX:-ratatoskr-platform}"
identity="${RATATOSKR_OFFHOST_AGE_IDENTITY_FILE:?RATATOSKR_OFFHOST_AGE_IDENTITY_FILE is required}"
container="${RATATOSKR_OFFHOST_DRILL_CONTAINER:?RATATOSKR_OFFHOST_DRILL_CONTAINER is required}"
database="${RATATOSKR_OFFHOST_DRILL_DATABASE:-ratatoskr_offhost_drill}"
stage_dir="${RATATOSKR_OFFHOST_DRILL_STAGE_DIR:-/var/lib/ratatoskr-offhost-drill}"

for command in age aws docker; do
    command -v "$command" > /dev/null || fail "dependency"
done
[[ -r "$identity" ]] || fail "identity"

remote_prefix="$prefix/$day/postgresql/"
if "$dry_run"; then
    printf 'DRY-RUN: download yesterday remote dump under s3://%s/%s\n' "$bucket" "$remote_prefix"
    printf 'DRY-RUN: decrypt with %s and restore into %s on %s\n' "$identity" "$database" "$container"
    exit 0
fi

install -d -m 0700 "$stage_dir" || fail "staging"
work="$(mktemp -d "$stage_dir/.drill.XXXXXX")" || fail "staging"
scratch_created=false
cleanup() {
    if "$scratch_created"; then
        docker exec "$container" dropdb -U postgres --if-exists "$database" > /dev/null 2>&1 || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

remote_key="$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "$remote_prefix" \
    --query 'reverse(sort_by(Contents,&LastModified))[0].Key' --output text)" || fail "download"
[[ -n "$remote_key" && "$remote_key" != None ]] || fail "download"

encrypted_dump="$work/remote.dump.age"
aws s3 cp "s3://$bucket/$remote_key" "$encrypted_dump" --only-show-errors || fail "download"
[[ -s "$encrypted_dump" ]] || fail "download"

dump="$work/remote.dump"
age --decrypt --identity "$identity" --output "$dump" "$encrypted_dump" || fail "decrypt"
[[ -s "$dump" ]] || fail "decrypt"

docker exec "$container" dropdb -U postgres --if-exists "$database" > /dev/null || fail "scratch-create"
docker exec "$container" createdb -U postgres "$database" --template=template0 --locale-provider=icu \
    --icu-locale=und-x-icu --encoding=UTF8 > /dev/null || fail "scratch-create"
scratch_created=true
docker exec -i "$container" pg_restore -U postgres --dbname="$database" --no-owner --exit-on-error < "$dump" || \
    fail "restore"

schema_count="$(docker exec "$container" psql -U postgres -d "$database" -Atc \
    "select count(*) from information_schema.schemata where schema_name in
       ('identity', 'operations', 'platform_ingest')")" || fail "verify"
[[ "$schema_count" == 3 ]] || fail "verify"
constraint_count="$(docker exec "$container" psql -U postgres -d "$database" -Atc \
    "select count(*) from pg_constraint where connamespace::regnamespace::text in
       ('identity', 'operations', 'platform_ingest')")" || fail "verify"
[[ "$constraint_count" =~ ^[1-9][0-9]*$ ]] || fail "verify"

docker exec "$container" dropdb -U postgres "$database" > /dev/null || fail "cleanup"
scratch_created=false
printf 'PASS: off-host restore drill\n'
