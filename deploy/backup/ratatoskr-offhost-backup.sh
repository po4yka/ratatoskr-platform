#!/usr/bin/env bash
# Encrypt and copy the completed Platform database dump and latest Borg recovery export to off-host
# S3-compatible storage. The Pi has an age recipient only; it never receives a private identity.
set -euo pipefail

usage() {
    printf 'usage: %s [--date YYYY-MM-DD] [--dry-run]\n' "${0##*/}" >&2
}

day="$(date -u +%F)"
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

dump_dir="${RATATOSKR_OFFHOST_DUMP_DIR:-/mnt/nvme/backups/ratatoskr}"
stage_dir="${RATATOSKR_OFFHOST_STAGE_DIR:-/mnt/nvme/backups/ratatoskr/offhost-stage}"
config_root="${RATATOSKR_OFFHOST_CONFIG_ROOT:-/}"
bucket="${RATATOSKR_OFFHOST_BUCKET:?RATATOSKR_OFFHOST_BUCKET is required}"
prefix="${RATATOSKR_OFFHOST_PREFIX:-ratatoskr-platform}"
recipient="${RATATOSKR_OFFHOST_AGE_RECIPIENT:?RATATOSKR_OFFHOST_AGE_RECIPIENT is required}"
borg_repository="${RATATOSKR_BORG_REPOSITORY:?RATATOSKR_BORG_REPOSITORY is required}"

for command in age aws borg tar; do
    command -v "$command" > /dev/null || {
        printf 'ratatoskr-offhost-backup: missing required command: %s\n' "$command" >&2
        exit 69
    }
done

shopt -s nullglob
dumps=("$dump_dir"/ratatoskr-*.dump)
(( ${#dumps[@]} > 0 )) || {
    printf 'ratatoskr-offhost-backup: no completed dump in %s\n' "$dump_dir" >&2
    exit 66
}
# shellcheck disable=SC2012 # the dump names are a controlled path pattern and the count policy
# requires newest-first ordering; GNU find has no equivalent ordering without a second sort.
dump="$(ls -1t -- "${dumps[@]}" | head -n 1)"
[[ -s "$dump" ]] || {
    printf 'ratatoskr-offhost-backup: dump is empty: %s\n' "$dump" >&2
    exit 66
}
day_stamp="${day//-/}"
[[ "$(basename "$dump")" == *-"$day_stamp"T*.dump ]] || {
    printf 'ratatoskr-offhost-backup: newest dump is not from %s UTC: %s\n' "$day" "$dump" >&2
    exit 66
}

archive="${RATATOSKR_BORG_ARCHIVE:-}"
if [[ -z "$archive" ]]; then
    archive="$(borg list --short --last 1 "$borg_repository" | tail -n 1)"
fi
[[ -n "$archive" ]] || {
    printf 'ratatoskr-offhost-backup: no completed Borg archive in %s\n' "$borg_repository" >&2
    exit 66
}

# The upload credential and every age identity are intentionally absent: recovering those from the
# same bucket would make a stolen object and the credentials that open it one compromise.
config_files=(
    etc/ratatoskr/edge.conf
    etc/ratatoskr/ingest.conf
    etc/ratatoskr/scheduler.conf
    etc/systemd/system/ratatoskr-edge.service
    etc/systemd/system/ratatoskr-ingest.service
    etc/systemd/system/ratatoskr-scheduler.service
    etc/nats/ratatoskr.conf
    etc/nats/compose.yaml
    etc/logrotate.d/ratatoskr
)
for file in "${config_files[@]}"; do
    [[ -f "$config_root/$file" ]] || {
        printf 'ratatoskr-offhost-backup: required configuration is absent: %s\n' "$config_root/$file" >&2
        exit 66
    }
done

remote_base="s3://$bucket/$prefix/$day"
if "$dry_run"; then
    printf 'DRY-RUN: validate dump %s\n' "$dump"
    printf 'DRY-RUN: export Borg archive %s from %s\n' "$archive" "$borg_repository"
    printf 'DRY-RUN: archive allowlisted Platform configuration from %s\n' "$config_root"
    printf 'DRY-RUN: encrypt and upload PostgreSQL, Borg, and configuration recovery material to %s\n' \
        "$remote_base"
    exit 0
fi

install -d -m 0700 "$stage_dir"
work="$(mktemp -d "$stage_dir/.offhost.XXXXXX")"
trap 'rm -rf "$work"' EXIT

borg_tar="$work/$archive.tar"
borg export-tar "${borg_repository}::${archive}" "$borg_tar"
[[ -s "$borg_tar" ]] || {
    printf 'ratatoskr-offhost-backup: Borg export is empty: %s\n' "$archive" >&2
    exit 65
}
config_tar="$work/configuration.tar"
tar --create --file "$config_tar" --directory "$config_root" -- "${config_files[@]}"
[[ -s "$config_tar" ]] || {
    printf 'ratatoskr-offhost-backup: configuration snapshot is empty\n' >&2
    exit 65
}

encrypt() {
    local input="$1"
    local output="$2"

    age --encrypt --recipient "$recipient" --output "$output" "$input"
    [[ -s "$output" ]] || {
        printf 'ratatoskr-offhost-backup: age output is empty: %s\n' "$input" >&2
        exit 65
    }
}

ciphertext_sha256() {
    local file="$1"

    if command -v sha256sum > /dev/null; then
        sha256sum "$file" | awk '{ print $1 }'
    else
        shasum -a 256 "$file" | awk '{ print $1 }'
    fi
}

upload() {
    local file="$1"
    local key="$2"
    local digest

    digest="$(ciphertext_sha256 "$file")"
    aws s3 cp "$file" "$remote_base/$key" --only-show-errors --metadata "sha256=$digest"
}

dump_age="$work/$(basename "$dump").age"
borg_age="$borg_tar.age"
config_age="$config_tar.age"
encrypt "$dump" "$dump_age"
encrypt "$borg_tar" "$borg_age"
encrypt "$config_tar" "$config_age"

upload "$dump_age" "postgresql/$(basename "$dump_age")"
upload "$borg_age" "borg/$(basename "$borg_age")"
upload "$config_age" "configuration/$(basename "$config_age")"
printf 'ratatoskr-offhost-backup: uploaded encrypted recovery material for %s\n' "$day"
