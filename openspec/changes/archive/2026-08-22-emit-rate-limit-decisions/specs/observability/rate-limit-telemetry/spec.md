# observability/rate-limit-telemetry

## ADDED Requirements

### Requirement: Every allowance decision is counted where it is decided

The per-actor rate limiter SHALL emit one counter increment per admission decision, at the moment
the decision is made, so that every caller of the limiter is counted without a per-call-site line.
The series SHALL be named `platform_rate_limit_decisions_total` and SHALL carry exactly one label
`outcome` with the value `admitted` when a token was spent and `refused` when the actor's allowance
was exhausted.

#### Scenario: A refused actor is visible on the metrics surface

WHEN an actor exhausts its allowance and its next request is refused with the contract 429 fault,
THEN the metrics endpoint reports `platform_rate_limit_decisions_total{outcome="refused"}` having
advanced by one for the process, and the same request is otherwise indistinguishable from a refusal
before this change.

#### Scenario: An admitted request is counted as admitted

WHEN an authenticated request spends a token and proceeds to its handler,
THEN `platform_rate_limit_decisions_total{outcome="admitted"}` advances by one.

#### Scenario: The instrument set stays documented

WHEN the metric name set is compared against the documented set,
THEN `platform_rate_limit_decisions_total` is a member, and no name leaves or enters the exported
set without breaking the pinned test first.
