## Context

The workspace `github-fleet-bus-runtime` specification and changeset `GHB-017` define six classified subjects. Platform already owns bounded `ratatoskr_commands` and `ratatoskr_events` streams, deterministic stream validation, fixed durable provisioning, and a real disposable-broker permission matrix. GitHub must use those existing boundaries and must not gain topology authority.

## Goals / Non-Goals

**Goals:** provision four independent deliver-all, explicit-ack durables with finite acknowledgement wait and deliveries; give GitHub only the exact two outbound subjects and the API paths needed to inspect/fetch/acknowledge those durables; make repeated provisioning idempotent and drift fail closed.

**Non-Goals:** changing envelope payloads, creating a GitHub runtime, generating production NKeys, operating the deployment host, or granting wildcard stream access.

## Decisions

Use one consumer per inbound family: `ratatoskr_github_sync` on `ratatoskr_commands`, plus `ratatoskr_github_analysis_completed`, `ratatoskr_github_analysis_failed`, and `ratatoskr_github_vault_policy_ack` on `ratatoskr_events`. Reuse the repository's explicit consumer comparison and ensure path. GitHub receives only the corresponding consumer-info/next/ack API subjects, `_INBOX.>` subscription, and publication of `evt.knowledge.repository_analysis.requested.v1` and `cmd.vault.target.desired.v1`. Existing mismatched consumers are reported and left unchanged.

## Risks / Trade-offs

- Four cursors add topology, but isolate poison/backlog and make each authority independently observable.
- Exact API subjects are more verbose than `$JS.API.>`, but prevent application self-escalation.
- Public-key placeholders make checked-in configuration deployable without ever storing seeds; synthetic tests generate both halves transiently.

## Migration Plan

Merge and deploy this additive topology before GitHub runtime activation. Rollback stops GitHub first; the unused identity and durable cursors remain in place. Deleting or resetting them is a separate operator action.
