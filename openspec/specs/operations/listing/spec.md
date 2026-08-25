# operations/listing Specification

## Purpose
An authenticated owner can enumerate their own operations with explicit filters and a stable cursor, so operational views render lists without fetching identifiers one by one.

## Requirements

### Requirement: Listing is scoped to the calling owner

The Edge API SHALL expose `GET /v1/operations` to authenticated sessions. It SHALL return only operations owned by the authenticated user, newest accepted first, in pages. Unauthenticated calls receive the standard refusal.

#### Scenario: another tenant's operations stay invisible

- **WHEN** two users own disjoint sets of operations and one lists operations
- **THEN** the response contains only that user's operations, and no response field reveals the count or existence of the other user's operations

#### Scenario: unauthenticated call

- **WHEN** the listing endpoint is called without valid session credentials
- **THEN** the answer is the standard unauthenticated refusal envelope

### Requirement: Pagination walks a stable keyset cursor

Pagination SHALL use an opaque continuation cursor anchored on acceptance time and identity, never numeric offsets, so concurrent inserts cannot shift results between pages. Walking the cursor from page to page SHALL visit each matching operation exactly once, newest accepted first. When no further pages exist the response SHALL say so explicitly rather than returning a cursor that yields an empty page forever.

#### Scenario: paging through a fixture set

- **WHEN** an owner owns more operations than one page holds and the client follows the returned cursor until exhaustion
- **THEN** every operation appears exactly once, ordered newest accepted first, and the final page marks itself as the last

#### Scenario: inserts during a walk do not shift old pages

- **WHEN** a new operation is accepted after a client received a page and the client continues from that page's cursor
- **THEN** the remaining pages contain neither duplicates of already-seen operations nor a skipped older operation

#### Scenario: exhausted listing

- **WHEN** the client requests the page after the final operation
- **THEN** the response contains zero rows and explicitly indicates there is no next page

### Requirement: Page size is explicit and bounded

The listing SHALL accept an explicit page size parameter bounded above by a fixed maximum, with a documented default applied when absent. A page size outside the permitted range SHALL be refused as a client error.

#### Scenario: default and maximum page sizes

- **WHEN** a client omits the page size, or asks for a size within the permitted range
- **THEN** the response carries at most that many operations, using the documented default when omitted

#### Scenario: out-of-range page size

- **WHEN** a client requests zero, a negative, or an above-maximum page size
- **THEN** the answer is the standard invalid-request refusal envelope and no page is served

### Requirement: Filters are explicit and validated

The listing SHALL accept a status filter restricted to the operation status vocabulary and a kind filter matched exactly. Filters combine by conjunction. A filter value outside its vocabulary or grammar SHALL be refused as a client error, never silently ignored.

#### Scenario: filtering by state

- **WHEN** an owner lists operations filtered to a single valid status
- **THEN** every returned row carries that status and rows in other statuses are absent

#### Scenario: filtering by kind

- **WHEN** an owner lists operations filtered to a kind they use
- **THEN** every returned row carries exactly that kind, including rows identical in kind but different in outcome

#### Scenario: combining filters

- **WHEN** an owner supplies both a status filter and a kind filter
- **THEN** only operations satisfying both are returned

#### Scenario: invalid filter values

- **WHEN** a client passes a status outside the vocabulary or a kind that violates the kind grammar
- **THEN** the answer is the standard invalid-request refusal envelope and no page is served

### Requirement: Rows carry the summary projection without heavy payloads

Each listed row SHALL carry the same identification and lifecycle fields the singular operation endpoint exposes — identifier, kind, status, stage, progress, retryability, correlation, and the lifecycle timestamps — and SHALL NOT carry result references, errors, or warnings. Reading heavy payloads remains the singular endpoint's job.

#### Scenario: a succeeded row omits payloads

- **WHEN** an operation that reported result references and errors appears in a listing
- **THEN** its row shows the lifecycle fields truthfully and contains no result references, errors, or warnings, while the singular endpoint for that operation still returns them

### Requirement: Malformed cursors are client errors

A cursor value the service cannot decode SHALL be refused as a client error rather than restarting the listing from the beginning.

#### Scenario: malformed cursor

- **WHEN** a client presents a cursor string the service cannot decode into a continuation point
- **THEN** the answer is the standard invalid-request refusal envelope and no page is served
