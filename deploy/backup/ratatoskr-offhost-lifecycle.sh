#!/usr/bin/env bash
# Print the bucket lifecycle configuration that the storage administrator applies out of band.
set -euo pipefail

usage() {
    printf 'usage: %s [--remote-keep-days DAYS] [--dry-run]\n' "${0##*/}" >&2
}

keep_days="${RATATOSKR_OFFHOST_REMOTE_KEEP_DAYS:-90}"
dry_run=false
while (($# > 0)); do
    case "$1" in
        --remote-keep-days)
            (($# >= 2)) || { usage; exit 64; }
            keep_days="$2"
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

[[ "$keep_days" =~ ^[1-9][0-9]*$ ]] || {
    printf 'ratatoskr-offhost-lifecycle: keep days must be a positive integer\n' >&2
    exit 64
}

prefix="${RATATOSKR_OFFHOST_PREFIX:-ratatoskr-platform}"
if "$dry_run"; then
    printf 'DRY-RUN: generate lifecycle policy for s3://<bucket>/%s/* with %s-day retention\n' \
        "$prefix" "$keep_days" >&2
fi

printf '%s\n' '{'
printf '%s\n' '  "Rules": ['
for material in postgresql borg configuration; do
    comma=','
    if [[ "$material" == configuration ]]; then
        comma=''
    fi
    printf '%s\n' '    {'
    printf '      "ID": "ratatoskr-%s-retention",\n' "$material"
    printf '      "Filter": { "Prefix": "%s/*/%s/" },\n' "$prefix" "$material"
    printf '%s\n' '      "Status": "Enabled",'
    printf '      "Expiration": { "Days": %s },\n' "$keep_days"
    printf '%s\n' '      "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 7 }'
    printf '    }%s\n' "$comma"
done
printf '%s\n' '  ]'
printf '%s\n' '}'
