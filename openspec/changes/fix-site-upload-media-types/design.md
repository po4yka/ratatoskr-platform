## Context

Workspace SITE-027 task 12.3 exposed a 502 when the real Notes service refused a control request
without a JSON media type. Existing test stubs accepted it. The workspace spec
`site-owner-editing` defines the public workflow.

## Goals / Non-Goals

Goals: send the media type of each existing Notes request and test all three upload steps.
Non-goals: change the public route, Notes validation, claims, body limits, schemas or dependencies.

## Decisions

Create separate header sets for the two JSON control requests and for the raw chunk in
`upload_content`. Retain the original claims for session cleanup and unrelated GET calls. This
keeps the body type explicit at the upload call site instead of assigning a media type by HTTP
method in the shared response decoder.

The regression Notes stub uses Axum `Json` for open and finalize and refuses a chunk without
`application/octet-stream`. A successful public upload therefore proves both JSON requests and
the raw request pass the same media-type boundary as the real service.

## Risks / Trade-offs

No public contract changes. The workspace integration run must confirm the real Notes service
accepts the request after the repository checks pass. No deployment target is changed.
