# library-api Specification

## Purpose

Provides the session-authenticated Platform façade for bounded library search, effective read-state filtering, and idempotent read-state replacement owned by Knowledge.

## Requirements

### Requirement: Search is a documented session-authenticated route

Edge SHALL serve and document `GET /v1/library/search` from the same route table. It SHALL accept optional `q` and `read_state`, `limit` from 1 through 100, and non-negative `offset`; it SHALL reject unknown parameters and invalid values with the stable invalid-request error. The response SHALL contain typed item summaries plus `limit`, `offset`, and `has_more` and SHALL carry `Cache-Control: no-store`.

#### Scenario: OpenAPI and router stay aligned

- **WHEN** the generated OpenAPI document and the Edge router are inspected
- **THEN** both contain the authenticated GET library search operation with the same parameters, response type, and stable error envelopes

#### Scenario: Invalid parameters stop before delegation

- **WHEN** a request carries an invalid read state, page bound, oversized query, unknown field, or tenant selector
- **THEN** Edge returns the invalid-request envelope and the Knowledge harness records no call

### Requirement: Edge derives and enforces tenant authority

Edge SHALL derive the canonical Knowledge tenant from the authenticated principal and pass it only through its dedicated loopback client. Client-supplied identity and reserved headers SHALL be removed and SHALL NOT influence that tenant. A library page SHALL contain only results owned by the authenticated principal.

#### Scenario: Forged identity cannot select another tenant

- **WHEN** a valid principal sends forged reserved identity headers and a foreign tenant selector
- **THEN** the request cannot produce a Knowledge query under that foreign tenant and exposes no foreign item

### Requirement: Public item summaries minimize internal detail

Each result SHALL contain `analysis_id`, `document_id`, `title` of at most 256 Unicode scalar values, optional `snippet` of at most 512 Unicode scalar values, optional finite positive `score`, and closed `read_state`. Edge SHALL NOT expose Knowledge tenant references, owner-context strings, table identifiers, provider errors, or internal route details. Missing optional fields SHALL remain absent rather than becoming invented values.

#### Scenario: Internal fields do not cross the public boundary

- **WHEN** Knowledge returns a valid result together with internal tenant and owner-context fields
- **THEN** Edge's response contains only the public item fields and omits the internal values

### Requirement: Read state is an idempotent documented resource

Edge SHALL serve and document session-authenticated `PUT /v1/library/items/{analysis_id}/read-state`. The exact JSON body SHALL contain one closed `read_state` value, and the successful response SHALL return the authoritative state. Repeating the same replacement SHALL be safe. Foreign and missing identifiers SHALL map to the same not-found envelope.

#### Scenario: Repeated read replacement succeeds

- **WHEN** the owner puts `read` twice for one accepted analysis
- **THEN** both responses return authoritative `read` and Knowledge observes no change to favorite or other user content

#### Scenario: Foreign and missing targets map identically

- **WHEN** Knowledge reports scoped absence for a foreign target and for a nonexistent target
- **THEN** Edge returns the same status and error code for both without exposing upstream detail

### Requirement: Knowledge delegation is bounded and safely mapped

The dedicated Knowledge client SHALL apply finite connect, response-header, and total deadlines and bounded response size. Invalid or uncontracted success payloads SHALL map to an invalid-upstream response; connection failure and timeout SHALL map to distinct retryable Platform error classes; raw upstream bodies and topology SHALL NOT reach the client or ordinary logs.

#### Scenario: Knowledge times out

- **WHEN** the Knowledge harness accepts a request but does not answer within the total deadline
- **THEN** Edge returns the stable retryable timeout envelope and emits only a safe dependency failure class

#### Scenario: Knowledge returns malformed success

- **WHEN** the Knowledge harness answers success with an invalid item state or non-finite score
- **THEN** Edge returns the stable invalid-upstream envelope and leaks none of that body

### Requirement: Capabilities describe working library behavior

The closed capability vocabulary SHALL include `library.search` and `library.read_state`. Each SHALL appear only when Edge serves its route, the session/database path is available, and the last background Knowledge observation is healthy. The observation SHALL not be performed on the public request path.

#### Scenario: Last observation becomes unhealthy

- **WHEN** the background dependency state changes from healthy to unhealthy
- **THEN** both names disappear from the next capability response and their availability gauges report zero
