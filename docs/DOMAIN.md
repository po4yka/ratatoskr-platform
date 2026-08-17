# Platform domain model

## Terms

- **Principal:** authenticated user, device, or trusted service assertion.
- **Session:** revocable browser/application authentication state.
- **Device:** registered installation with constrained credentials.
- **Operation:** durable user-visible asynchronous work record.
- **Attempt:** one execution of an operation step.
- **Capability:** runtime declaration that a feature and dependencies are usable.
- **Ingress envelope:** normalized capture/webhook input before domain routing.

## Operation lifecycle

`accepted -> queued -> running -> succeeded | partially_succeeded | failed | cancelled`

## Invariants

1. Public clients use Platform, not internal services.
2. Long work never blocks the request lifecycle.
3. Duplicate requests/events do not duplicate effects.
4. Progress cannot move a terminal operation backward.
5. Provider credentials never enter Platform persistence.
6. Service authentication does not replace user authorization.
7. Scheduler publishes commands only; it owns no domain repositories.
