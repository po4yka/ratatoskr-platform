# Platform requirements

> Status: Proposed  
> Last reviewed: 2026-08-17

## Goals

1. Expose one versioned authenticated public API.
2. Own users, sessions, devices, capabilities, operations, and progress projections.
3. Accept long-running work asynchronously with idempotency and truthful status.
4. Normalize generic ingress and publish typed commands.
5. Schedule commands without importing domain implementations.

## Non-goals

Scraping, Git, LLM inference, provider synchronization, provider token storage, cross-schema writes, and long work inside HTTP handlers.

## Requirements

- Every accepted long operation returns a durable `operation_id`.
- Idempotency is scoped to principal and operation type.
- Authorization occurs before private existence is disclosed.
- Operation state and partial effects are truthful and replay-safe.
- Outbox/inbox supports at-least-once delivery.
- Capabilities declare unavailable or degraded features explicitly.
- Public errors are stable, safe, and correlated.

First slice: authenticated capture request -> operation -> command -> progress event -> completed projection exposed through REST/SSE.
