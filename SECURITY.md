# Security Policy for Ratatoskr Platform

> Status: Proposed  
> Last reviewed: 2026-08-17

Report vulnerabilities privately through GitHub private vulnerability reporting when available or another established private channel. Do not place cookies, bearer tokens, device secrets, identity assertions, private captures, or production logs in public issues.

Security review is mandatory for authentication, sessions, device pairing, authorization, OAuth callbacks, Mini App assertions, idempotency, public endpoints, upload limits, audit, and service credentials.

Baseline: deny by default; validate issuer/audience/expiry/nonce; rotate and revoke credentials; redact secrets; authorize before disclosing existence; rate-limit public boundaries; authenticate NATS producers; never store provider tokens owned by domain services.
