## Context

See [proposal.md](proposal.md). The AI archive services publish operation reports with no envelope `tenant_id` and the ChatGPT and Claude work orders for round 2 do not add one.

## Decisions

- **The acceptance record is the stronger binding.** Platform wrote it when it accepted the upload, and it carries the owner, the provider and the digest. The projection already requires the provider subject and the provider producer for it. Requiring the record to name the operation's owner closes the one remaining path: an acceptance row for a different owner does not bind.
- **A present tenant is always checked.** A wrong tenant is a bug or a forgery on any kind, so relaxing the missing case does not relax the mismatch case.
- **No binding, no relaxation.** An `ai_archive.import` operation with no acceptance record, or one whose record names another owner, still needs the owner's tenant like any other kind.
- **Platform-only fix.** Changing the two services to publish a tenant would work too, but it blocks the Platform pin on two more repositories and the acceptance record already carries the evidence.
