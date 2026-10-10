## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md section S06. The receipt binding constants (`RECEIPT_PATH`, `ARCHIVE_MEDIA_TYPE`, the six `HEADER_*` names, the capability document helpers) live in `ratatoskr_ai_archive_contracts::platform_receipt` and Platform imports them.

## Decisions

- **The binding lives in one place.** Method, content type and claim headers are set inside `Gateway::forward_archive_receipt`, so no caller can forward with a different binding. `ArchiveReceipt` carries a `Body` and the declared byte size, and finalize builds the body with `Body::from_stream` over the verified file, so an archive is never buffered whole.
- **The chunk limit is a route layer.** The outer transfer-budget layer and the public listener limit stay; only the chunk handler gets `DefaultBodyLimit::max(16777216)`. The exact-length check in `put_chunk` remains. A deployment on the compiled 1 MiB public default still cannot take chunks above 1 MiB, which rule V4 already prevents in the shipped config.
- **Ceiling versus budget.** The archive ceiling is its own configuration value because the transfer body budget bounds one request, not one archive. Staging needs about twice the ceiling in free space per in-flight archive (chunks plus the assembled file), which the operator documentation says.
- **Invalid reports are rejected where they are read.** `ProgressReport::read` already returns "unreadable" for a report it cannot decode and the projection records those as rejected; calling `validate()` there reuses that path, so no new outcome exists and the stored snapshot stays readable.

## Risks / Trade-offs

- Staging removal after a successful forward is best effort and logged; a leftover directory is reclaimed by the existing staging hygiene and is not a correctness issue.
