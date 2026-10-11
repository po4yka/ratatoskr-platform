## ADDED Requirements

### Requirement: Threads permalinks on threads.com are accepted

The social capture route SHALL accept an `https` permalink on `threads.net`, `www.threads.net`, `threads.com` or `www.threads.com` for the `threads` provider and SHALL still refuse `http` and any other host.

#### Scenario: a threads.com permalink is accepted

- **WHEN** a social capture names provider `threads` and a `https://www.threads.com/@name/post/<id>` permalink
- **THEN** the route answers 202

#### Scenario: a lookalike host is refused

- **WHEN** the permalink host is `threads.com.evil.example`
- **THEN** the route answers 400
