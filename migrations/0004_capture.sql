-- Milestone 5: the credential a session presents, and the idempotency ledger.
--
-- Two things the earlier milestones left for the first real request path.
--
-- `identity.sessions` had no credential column. Milestone 2 built the session lifecycle — kinds,
-- audiences, expiry, revocation — but nothing a client could present, because nothing served a
-- request yet. `DATA_MODEL.md` already anticipated it: "Session/device secrets are hashed or
-- encrypted and never emitted in events".
--
-- `operations.idempotency_records` is the ledger `ARCHITECTURE.md` S8.1 specifies. It is a separate
-- table rather than columns on `operations.operations` because a reservation exists BEFORE an
-- operation does: the key is claimed, then the work is decided, and a request that is rejected after
-- reserving must still hold its key so a retry gets the same answer.

-- ---------------------------------------------------------------------------------------------
-- The session credential
-- ---------------------------------------------------------------------------------------------

alter table identity.sessions
    add column token_hash bytea;

comment on column identity.sessions.token_hash is
    'The digest of the bearer credential this session is presented with. Nullable, not because a '
    'session without one is useful, but because this column is added to a table that already exists: '
    'a session minted before this migration has no credential and simply never authenticates, which '
    'is the safe direction. It becomes NOT NULL when the first release makes a backfill meaningful.';

alter table identity.sessions
    add constraint sessions_token_hash_is_a_digest
        check (token_hash is null or length(token_hash) = 32);

-- The lookup key on every authenticated request, and a uniqueness rule: one credential authenticates
-- at most one session. Without it a hash collision — or a bug that reused a token — would silently
-- authenticate the wrong principal.
create unique index sessions_token_hash_key
    on identity.sessions (token_hash)
    where token_hash is not null;

-- ---------------------------------------------------------------------------------------------
-- The idempotency ledger
-- ---------------------------------------------------------------------------------------------

create table operations.idempotency_records (
    record_id            uuid        primary key,
    owner_user_id        uuid        not null,
    route                text        not null,
    operation_kind       text        not null,
    key_hash             bytea       not null,
    request_fingerprint  bytea       not null,
    operation_id         uuid        references operations.operations (operation_id) on delete cascade,
    response_status      smallint,
    reserved_at          timestamptz not null,
    completed_at         timestamptz,
    expires_at           timestamptz not null,

    -- `ARCHITECTURE.md` S5.3 versions public routes, so the stored route carries its version. The
    -- bound is what stops an attacker-chosen path becoming an unbounded row.
    constraint idempotency_route_is_a_versioned_path
        check (route ~ '^/v[1-9][0-9]{0,2}(/[a-z][a-z0-9_-]{0,63}){1,6}$'),
    constraint idempotency_operation_kind_is_a_dotted_name
        check (operation_kind ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    -- Both are SHA-256 digests. The key is hashed rather than stored, because a client-chosen
    -- `Idempotency-Key` may carry meaning the client considers private, and this table is read by
    -- operators (SECURITY.md, redact secrets).
    constraint idempotency_key_hash_is_a_digest
        check (length(key_hash) = 32),
    constraint idempotency_request_fingerprint_is_a_digest
        check (length(request_fingerprint) = 32),
    constraint idempotency_response_status_is_an_http_status
        check (response_status is null or response_status between 100 and 599),
    constraint idempotency_expires_after_it_is_reserved
        check (expires_at > reserved_at),
    constraint idempotency_completed_at_is_not_before_reserved_at
        check (completed_at is null or completed_at >= reserved_at),
    -- A completed reservation has an answer to replay; an outstanding one does not. Storing a status
    -- without a completion instant, or the reverse, would leave a retry unable to tell whether the
    -- first attempt finished.
    constraint idempotency_completion_is_whole
        check ((completed_at is null) = (response_status is null))
);

comment on table operations.idempotency_records is
    'ARCHITECTURE.md S8.1. A reservation is taken in the same transaction as the operation it '
    'protects, so a crash between them cannot leave a key claimed for work that never started. '
    'Retrying with the same payload returns the original operation; reusing the key with a different '
    'payload is rejected, which is what the fingerprint is for.';
comment on column operations.idempotency_records.owner_user_id is
    'References identity.users(user_id) semantically, with no foreign key: DATA_MODEL.md forbids '
    'cross-schema foreign keys.';
comment on column operations.idempotency_records.request_fingerprint is
    'A digest of the canonical request body. S8.1: "Reusing a key with a different payload is '
    'rejected." Comparing digests rather than bodies means the ledger never holds request content.';
comment on column operations.idempotency_records.expires_at is
    'The replay window. After it passes the row is collectable and the key may be used again; '
    'DATA_MODEL.md lists the idempotency window as its own retention class for exactly this reason.';

-- The scope S8.1 fixes: actor, route, and operation kind. A key is only meaningful inside it, so two
-- different clients, or the same client on two routes, never collide.
create unique index idempotency_scope_key
    on operations.idempotency_records (owner_user_id, route, operation_kind, key_hash);

create index idempotency_expires_at_idx
    on operations.idempotency_records (expires_at);

create index idempotency_operation_id_idx
    on operations.idempotency_records (operation_id)
    where operation_id is not null;
