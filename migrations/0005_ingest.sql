-- Milestone 7: the schema `ratatoskr-ingest` owns.
--
-- `docs/ARCHITECTURE.md` S4.1 lists `platform_ingest.*` among the three schemas Platform owns, and
-- ADR-0009 settled the spelling that milestone 2 could not: the word is `ingest` wherever it is an
-- identifier, so this schema, the crate, the binary, the database role of S18 and the `/v2/ingest`
-- path prefix are all one string.
--
-- One table. `ARCHITECTURE.md` S9 gives ingest six steps, and only the first — "source
-- authentication or signature validation" — needs state this schema does not already have:
--
--   * receipt deduplication (step 2) reuses `operations.idempotency_records`, whose scope is
--     already actor + route + kind + key. The source is folded into the key, so two sources owned
--     by one user cannot collide on a shared external identifier;
--   * normalization (step 3) is a pure function of the request body and stores nothing;
--   * routing (step 4) is the `target` column below;
--   * command publication (step 5) is `operations.outbox`, unchanged;
--   * receipt status projection (step 6) IS the operation: the 202 returns its identifier and
--     `GET /v2/operations/{id}` reports its status. A second status column here would be a copy
--     that can disagree with the record it copies.
--
-- The conventions of 0001 to 0004 apply unchanged: UUID primary keys minted by the writer, `text`
-- with a bounding CHECK rather than `varchar(n)`, `timestamptz` everywhere, and no foreign key that
-- crosses a schema boundary.

create schema platform_ingest;

comment on schema platform_ingest is
    'Generic ingress: sources that push signals into Platform without justifying a provider '
    'repository of their own (ARCHITECTURE.md S9). It holds no extracted content, no provider '
    'credential for a dedicated service, and no domain record.';

-- ---------------------------------------------------------------------------------------------
-- webhook_sources
-- ---------------------------------------------------------------------------------------------

create table platform_ingest.webhook_sources (
    source_id      uuid        primary key,
    owner_user_id  uuid        not null,
    label          text        not null,
    token_hash     bytea       not null,
    target         text        not null,
    created_at     timestamptz not null,
    disabled_at    timestamptz,

    -- An operator-facing name. Bounded because it is chosen by whoever registers the source and is
    -- read back in an operator tool.
    constraint webhook_sources_label_is_bounded
        check (length(label) between 1 and 120),
    -- SHA-256 of the bearer credential the source presents, hashed exactly as `identity.sessions`
    -- hashes a session credential. S9 step 1 allows "source authentication OR signature
    -- validation"; authentication is chosen because an HMAC signature needs the shared secret back
    -- in plaintext, and a store that can return a secret needs key management this repository does
    -- not have yet. See ADR-0009.
    constraint webhook_sources_token_hash_is_a_digest
        check (length(token_hash) = 32),
    -- The bounded context this source's signals are routed to (S9 step 4). The grammar is checked
    -- here for the same reason `operations.outbox.subject` is: a value that reaches a subject is a
    -- security boundary, and the closed list of legal targets lives in Rust where an operator
    -- cannot extend it by writing a row.
    constraint webhook_sources_target_is_a_dotted_name
        check (target ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    constraint webhook_sources_disabled_at_is_not_before_created_at
        check (disabled_at is null or disabled_at >= created_at)
);

comment on table platform_ingest.webhook_sources is
    'A third party that PUSHES signals to Platform. Named for what it is rather than `sources`, '
    'because a source Platform PULLS from — the RSS/Atom polling of S9 — authenticates in the '
    'opposite direction and shares none of these columns: it has a URL, a poll interval and a '
    'validator, and no credential of ours to present. It gets its own table when it exists.';
comment on column platform_ingest.webhook_sources.owner_user_id is
    'References identity.users(user_id) semantically, with no foreign key: DATA_MODEL.md forbids '
    'cross-schema foreign keys. The operation a signal creates belongs to this user, which is what '
    'makes a webhook submission reachable at GET /v2/operations/{id} by its owner and by nobody '
    'else.';
comment on column platform_ingest.webhook_sources.target is
    'Which bounded context this source feeds, e.g. `content.capture`. Data rather than code so a '
    'second source can route somewhere else without a deployment, bounded by a closed Rust list so '
    'a row cannot invent a command family no consumer subscribes to.';
comment on column platform_ingest.webhook_sources.disabled_at is
    'A disabled source authenticates as unknown. A row rather than a delete, so a source that '
    'turned abusive keeps the operations it already created attributable.';

-- The lookup on every inbound signal, and the same uniqueness rule as `identity.sessions`: one
-- credential authenticates at most one source.
create unique index webhook_sources_token_hash_key
    on platform_ingest.webhook_sources (token_hash);

create index webhook_sources_owner_user_id_idx
    on platform_ingest.webhook_sources (owner_user_id);
