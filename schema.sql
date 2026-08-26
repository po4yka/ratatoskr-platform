-- The Platform database, in one file.
--
-- `ratatoskr-edge` applies this at startup, to a fresh database. There is no migration ledger and no
-- incremental history: no database holds data that has to survive a schema change. A schema change
-- edits this file in place; the next fresh database has it.
--
-- Three schemas, in the order they appear below. `docs/ARCHITECTURE.md` S4.1 names all three and no
-- fourth:
--
--   * `identity`      — who a caller is, and what they may present to prove it.
--   * `operations`    — the durable record of user-visible asynchronous work, and the machinery that
--                       moves it: outbox, inbox, idempotency ledger, schedules.
--   * `platform_ingest` — generic ingress state, for sources that push signals in without justifying
--                       a provider repository of their own.
--
-- Conventions, applied uniformly and stated once here:
--
--   * Identifiers are UUIDv7 minted by the application, never by the database. `ratatoskr-contracts`
--     requires UUIDv7 for internally minted identity (ARCHITECTURE S5.1), and a database default
--     would produce v4. There is deliberately no DEFAULT on any id column, so a missing id is a
--     compile-or-insert error rather than a silently wrong version.
--
--   * Closed vocabularies are `text` with a CHECK, not a PostgreSQL enum. Adding a value to a PG enum
--     cannot run inside the same transaction that uses it, and removing one is a table rewrite;
--     a CHECK constraint is altered by one statement.
--
--   * A bounded `text` with a CHECK rather than `varchar(n)`: the bound is stated where every other
--     rule about the column is stated, and widening it is one statement either way.
--
--   * Every timestamp is `timestamptz`. `timestamp` would silently record the server's local time.
--
--   * A secret is stored as a hash in `bytea` and the column is named `*_hash`. There is no column
--     anywhere in this file that can hold a credential in a readable form (SECURITY.md), with one
--     stated and bounded exception: `identity.oauth_relays.code`, which says why at the column.
--
--   * No foreign key crosses a schema boundary. DATA_MODEL.md forbids it, so `operations` references
--     a user by an unenforced `uuid` column, not by a REFERENCES clause. Every such column says so.

-- =================================================================================================
-- identity
-- =================================================================================================
--
-- ARCHITECTURE.md S6.1 names the tables; DATA_MODEL.md adds grants, assertion nonces and audit
-- context.

create schema identity;

comment on schema identity is
    'Platform-owned identity. Internal user identity, external identity mappings, devices, sessions, '
    'refresh tokens, assertions, grants, revocations and the public-action audit trail.';

-- ---------------------------------------------------------------------------------------------
-- users
-- ---------------------------------------------------------------------------------------------

create table identity.users (
    user_id     uuid        primary key,
    status      text        not null,
    created_at  timestamptz not null,
    updated_at  timestamptz not null,

    constraint users_status_is_known
        check (status in ('active', 'suspended', 'deleted')),
    constraint users_updated_at_is_not_before_created_at
        check (updated_at >= created_at)
);

comment on table identity.users is
    'The internal user. ARCHITECTURE S6.1: internal user UUIDs are independent of Telegram IDs, '
    'GitHub IDs, email addresses and provider account IDs. This table therefore carries no provider '
    'column and no email column; both live in identity.identities.';
comment on column identity.users.status is
    'active | suspended | deleted. `deleted` is a tombstone, not a row removal: sessions, audit '
    'records and operation history reference this id and must stay readable for their own retention '
    'windows (DATA_MODEL.md, Retention).';

-- ---------------------------------------------------------------------------------------------
-- identities: an external identity mapped onto an internal user
-- ---------------------------------------------------------------------------------------------

create table identity.identities (
    identity_id  uuid        primary key,
    user_id      uuid        not null references identity.users (user_id),
    provider     text        not null,
    external_id  text        not null,
    created_at   timestamptz not null,
    last_seen_at timestamptz,

    constraint identities_provider_is_known
        check (provider in ('telegram', 'github', 'email')),
    constraint identities_external_id_is_bounded
        check (length(external_id) between 1 and 255)
);

comment on table identity.identities is
    'Maps a provider-side identity onto an internal user. ARCHITECTURE S6.1: a provider integration '
    'maps an external identity to an internal user but does not replace internal identity.';
comment on column identity.identities.external_id is
    'Opaque to Platform. A provider numeric id is stored as its decimal text so the column never '
    'has to change type when a provider widens its id space.';

-- One provider identity belongs to at most one internal user.
create unique index identities_provider_external_id_key
    on identity.identities (provider, external_id);

create index identities_user_id_idx
    on identity.identities (user_id);

-- ---------------------------------------------------------------------------------------------
-- registered_devices
-- ---------------------------------------------------------------------------------------------

create table identity.registered_devices (
    device_id     uuid        primary key,
    user_id       uuid        not null references identity.users (user_id),
    kind          text        not null,
    display_name  text,
    secret_hash   bytea       not null,
    created_at    timestamptz not null,
    last_seen_at  timestamptz,
    revoked_at    timestamptz,

    constraint registered_devices_kind_is_known
        check (kind in ('mobile', 'browser_extension', 'export_agent')),
    constraint registered_devices_display_name_is_bounded
        check (display_name is null or length(display_name) between 1 and 120),
    -- A 32-byte Argon2id/HMAC output. The length is checked so a plaintext secret, which would be
    -- printable and a different length, cannot be stored by mistake.
    constraint registered_devices_secret_hash_is_a_digest
        check (length(secret_hash) = 32),
    constraint registered_devices_revoked_at_is_not_before_created_at
        check (revoked_at is null or revoked_at >= created_at)
);

comment on table identity.registered_devices is
    'A registered installation with constrained credentials (DOMAIN.md, Device). The device secret '
    'is never stored, only its hash.';

create index registered_devices_user_id_idx
    on identity.registered_devices (user_id)
    where revoked_at is null;

-- ---------------------------------------------------------------------------------------------
-- sessions
-- ---------------------------------------------------------------------------------------------

create table identity.sessions (
    session_id    uuid        primary key,
    user_id       uuid        not null references identity.users (user_id),
    kind          text        not null,
    device_id     uuid        references identity.registered_devices (device_id),
    audience      text        not null,
    issued_at     timestamptz not null,
    expires_at    timestamptz not null,
    last_seen_at  timestamptz,
    revoked_at    timestamptz,
    token_hash    bytea,

    constraint sessions_kind_is_known
        check (kind in ('browser', 'device', 'telegram_mini_app', 'service', 'api_token')),
    constraint sessions_audience_is_bounded
        check (length(audience) between 1 and 120),
    constraint sessions_expires_after_it_is_issued
        check (expires_at > issued_at),
    constraint sessions_revoked_at_is_not_before_issued_at
        check (revoked_at is null or revoked_at >= issued_at),
    -- ARCHITECTURE S6.2 gives each session type separate audience, lifetime, rotation and revocation
    -- semantics. A `device` session without a device is one of those semantics violated.
    constraint sessions_device_kind_has_a_device
        check ((kind = 'device') = (device_id is not null)),
    constraint sessions_token_hash_is_a_digest
        check (token_hash is null or length(token_hash) = 32)
);

comment on table identity.sessions is
    'Revocable authentication state. ARCHITECTURE S6.2 lists the five kinds; each has its own '
    'audience, lifetime, rotation and revocation semantics, which is why kind and audience are '
    'separate columns rather than one conflated value.';
comment on column identity.sessions.revoked_at is
    'Set in place rather than deleting the row, so a revoked session remains auditable for its '
    'retention window. Liveness is `revoked_at is null and expires_at > now()`.';
comment on column identity.sessions.token_hash is
    'The digest of the bearer credential this session is presented with. Nullable because '
    '`session::create_session` accepts a session with no credential — `NewSession.token` is an '
    'Option — and a session minted that way simply never authenticates, which is the safe '
    'direction. Only a test has a reason to mint one; every route that opens a session for a '
    'client supplies a digest.';

create index sessions_user_id_live_idx
    on identity.sessions (user_id)
    where revoked_at is null;

create index sessions_expires_at_idx
    on identity.sessions (expires_at)
    where revoked_at is null;

-- The lookup key on every authenticated request, and a uniqueness rule: one credential authenticates
-- at most one session. Without it a hash collision — or a bug that reused a token — would silently
-- authenticate the wrong principal.
create unique index sessions_token_hash_key
    on identity.sessions (token_hash)
    where token_hash is not null;

-- ---------------------------------------------------------------------------------------------
-- refresh_tokens
-- ---------------------------------------------------------------------------------------------

create table identity.refresh_tokens (
    token_id     uuid        primary key,
    session_id   uuid        not null references identity.sessions (session_id),
    token_hash   bytea       not null,
    issued_at    timestamptz not null,
    expires_at   timestamptz not null,
    consumed_at  timestamptz,
    replaced_by  uuid        references identity.refresh_tokens (token_id),

    constraint refresh_tokens_expires_after_it_is_issued
        check (expires_at > issued_at),
    constraint refresh_tokens_secret_hash_is_a_digest
        check (length(token_hash) = 32),
    constraint refresh_tokens_consumed_at_is_not_before_issued_at
        check (consumed_at is null or consumed_at >= issued_at),
    -- A token is replaced only by being consumed. Without this, a rotation chain can be built that
    -- points at a successor while the predecessor is still usable, which is the replay THREAT_MODEL
    -- names.
    constraint refresh_tokens_replacement_implies_consumption
        check (replaced_by is null or consumed_at is not null),
    constraint refresh_tokens_is_not_its_own_successor
        check (replaced_by is distinct from token_id)
);

comment on table identity.refresh_tokens is
    'Rotating refresh credentials. Only the hash is stored. `consumed_at` plus `replaced_by` form '
    'the rotation chain, so replaying a consumed token is detectable rather than merely rejected.';

-- The hash is the lookup key on presentation, and a collision would be a cross-session credential.
create unique index refresh_tokens_token_hash_key
    on identity.refresh_tokens (token_hash);

create index refresh_tokens_session_id_idx
    on identity.refresh_tokens (session_id);

-- ---------------------------------------------------------------------------------------------
-- identity_assertions: the nonce store for short-lived third-party assertions
-- ---------------------------------------------------------------------------------------------

create table identity.identity_assertions (
    assertion_id  uuid        primary key,
    issuer        text        not null,
    subject       text        not null,
    audience      text        not null,
    nonce         text        not null,
    user_id       uuid        references identity.users (user_id),
    issued_at     timestamptz not null,
    expires_at    timestamptz not null,
    redeemed_at   timestamptz,

    constraint identity_assertions_issuer_is_known
        check (issuer in ('ratatoskr-telegram')),
    constraint identity_assertions_subject_is_bounded
        check (length(subject) between 1 and 255),
    constraint identity_assertions_audience_is_bounded
        check (length(audience) between 1 and 120),
    constraint identity_assertions_nonce_is_bounded
        check (length(nonce) between 16 and 128),
    constraint identity_assertions_expires_after_it_is_issued
        check (expires_at > issued_at),
    constraint identity_assertions_redeemed_at_is_not_before_issued_at
        check (redeemed_at is null or redeemed_at >= issued_at)
);

comment on table identity.identity_assertions is
    'ARCHITECTURE S6.3: ratatoskr-telegram validates raw Mini App initData because it owns the bot '
    'token, and returns a short-lived assertion bound to an internal user and an intended Edge '
    'audience. Platform never receives the bot token, so this table holds no provider secret. It is '
    'also the nonce store DATA_MODEL.md requires: the unique index below is what makes an assertion '
    'single-use, and single-use is what defeats replay (THREAT_MODEL.md).';

create unique index identity_assertions_issuer_nonce_key
    on identity.identity_assertions (issuer, nonce);

create index identity_assertions_expires_at_idx
    on identity.identity_assertions (expires_at)
    where redeemed_at is null;

-- ---------------------------------------------------------------------------------------------
-- grants
-- ---------------------------------------------------------------------------------------------

create table identity.grants (
    grant_id    uuid        primary key,
    user_id     uuid        not null references identity.users (user_id),
    capability  text        not null,
    granted_at  timestamptz not null,
    expires_at  timestamptz,
    revoked_at  timestamptz,

    constraint grants_capability_is_bounded
        check (length(capability) between 1 and 120),
    constraint grants_expires_after_it_is_granted
        check (expires_at is null or expires_at > granted_at),
    constraint grants_revoked_at_is_not_before_granted_at
        check (revoked_at is null or revoked_at >= granted_at)
);

comment on table identity.grants is
    'Authorization grants held by a user (ARCHITECTURE S7). Deliberately not a role table: '
    'S7 combines actor, ownership, capability, action and the owning service decision, and a role '
    'name would invite the ownership half to be skipped.';
comment on column identity.grants.capability is
    'An open vocabulary on purpose. The closed list is the capability projection (ARCHITECTURE S12), '
    'which is computed from deployment composition and health, not from this table.';

create unique index grants_user_capability_live_key
    on identity.grants (user_id, capability)
    where revoked_at is null;

-- ---------------------------------------------------------------------------------------------
-- revocations
-- ---------------------------------------------------------------------------------------------

create table identity.revocations (
    revocation_id  uuid        primary key,
    subject_kind   text        not null,
    subject_id     uuid        not null,
    reason         text        not null,
    revoked_at     timestamptz not null,
    revoked_by     uuid        references identity.users (user_id),

    constraint revocations_subject_kind_is_known
        check (subject_kind in ('user', 'session', 'device', 'refresh_token')),
    constraint revocations_reason_is_known
        check (reason in ('user_request', 'administrative', 'credential_rotation',
                          'suspected_compromise', 'expiry_policy'))
);

comment on table identity.revocations is
    'The append-only revocation record. The `revoked_at` column on sessions and devices is the fast '
    'path an authentication check reads; this table is the durable why-and-by-whom that survives the '
    'subject row and answers an audit question the subject row cannot.';
comment on column identity.revocations.subject_id is
    'Not a foreign key on purpose: a revocation must outlive its subject, including a user tombstone '
    'that is eventually purged under its retention policy.';

create index revocations_subject_idx
    on identity.revocations (subject_kind, subject_id);

-- ---------------------------------------------------------------------------------------------
-- audit_events
-- ---------------------------------------------------------------------------------------------

create table identity.audit_events (
    audit_event_id  uuid        primary key,
    occurred_at     timestamptz not null,
    actor_user_id   uuid        references identity.users (user_id),
    actor_session_id uuid       references identity.sessions (session_id),
    action          text        not null,
    target_kind     text        not null,
    target_id       uuid,
    outcome         text        not null,
    correlation_id  text        not null,

    constraint audit_events_action_is_bounded
        check (length(action) between 1 and 120),
    constraint audit_events_target_kind_is_bounded
        check (length(target_kind) between 1 and 60),
    constraint audit_events_outcome_is_known
        check (outcome in ('allowed', 'denied', 'failed')),
    -- The namespaced wire form from ratatoskr-contracts, e.g. `correlation:<uuid7>`. Stored as text
    -- because it is the same string the client saw in `x-correlation-id` and in the error body;
    -- splitting it would make the audit trail unjoinable to a support conversation.
    constraint audit_events_correlation_id_is_namespaced
        check (correlation_id ~ '^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9._~-]{1,128}$')
);

comment on table identity.audit_events is
    'ARCHITECTURE S15: audit records capture actor, action, target and result without copying '
    'sensitive content. There is deliberately no payload column: a free JSON blob here is how '
    'private content reaches an audit export.';

create index audit_events_actor_user_id_idx
    on identity.audit_events (actor_user_id, occurred_at desc);

create index audit_events_correlation_id_idx
    on identity.audit_events (correlation_id);

-- ---------------------------------------------------------------------------------------------
-- oauth_relays: the one-time, audience-bound record an OAuth callback is relayed through
-- ---------------------------------------------------------------------------------------------
--
-- `ARCHITECTURE.md` S6.4 assigns the halves: Edge may host the public callback route, and the owning
-- provider service generates or validates the state, exchanges the code, stores the tokens and
-- records the scopes. What crosses between them is this row. ADR-0012 records why it is a row rather
-- than a command payload — the command is written to `operations.outbox.payload` and then to a
-- `JetStream` file store, which is two durable copies of a live credential in the two places an
-- operator pages through while debugging.
--
-- It lives in `identity` rather than in a schema of its own: it is a step in authenticating a user's
-- authority over a provider account, and it references nothing outside identity.

create table identity.oauth_relays (
    relay_id      uuid        primary key,
    provider      text        not null,
    claim_grant   text        not null,
    state         text        not null,
    code          text,
    error         text,
    received_at   timestamptz not null,
    expires_at    timestamptz not null,
    claimed_at    timestamptz,

    -- A closed list, matching `identity.identities.provider`. Platform needs a provider's NAME and
    -- nothing else — no client id, no secret, no scope list — so this can be a vocabulary rather
    -- than configuration, and an attacker-chosen path segment cannot become an unbounded row.
    constraint oauth_relays_provider_is_known
        check (provider in ('telegram', 'github', 'email')),
    -- The capability a caller must HOLD to claim this relay, e.g. `oauth.claim.github`.
    --
    -- Not the claiming session's audience, which was the first design and is wrong:
    -- `identity.sessions.audience` names the LISTENER a session may be presented at — `edge`,
    -- `ingest` — so every service talking to the public API carries the same audience every person
    -- does, and binding a relay to it would have bound it to nothing. Worse, a session whose
    -- audience named a service could not authenticate at the edge listener at all, so no claim
    -- could ever have succeeded. Found by running it. `identity.grants` is the mechanism
    -- ARCHITECTURE S7 already gives for "this actor, this capability", and its vocabulary is open,
    -- so this needs no second table.
    constraint oauth_relays_claim_grant_is_a_dotted_name
        check (claim_grant ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    -- Attacker-supplied, every one of them: the callback is unauthenticated by construction, because
    -- Platform did not generate the `state` and holds no client secret with which to judge it. So
    -- each is bounded here as well as at the edge. `state` is opaque to Platform and is carried
    -- verbatim for the service that issued it.
    constraint oauth_relays_state_is_bounded
        check (length(state) between 1 and 512),
    constraint oauth_relays_code_is_bounded
        check (code is null or length(code) between 1 and 2048),
    constraint oauth_relays_error_is_bounded
        check (error is null or length(error) between 1 and 200),
    -- A callback carries a code or an error, never both and never neither: a provider that sent
    -- neither has told us nothing, and a row recording nothing is a row that can only confuse.
    constraint oauth_relays_carries_one_outcome
        check ((code is not null) <> (error is not null) or claimed_at is not null),
    constraint oauth_relays_expires_after_it_is_received
        check (expires_at > received_at),
    constraint oauth_relays_claimed_at_is_not_before_received_at
        check (claimed_at is null or claimed_at >= received_at)
);

comment on table identity.oauth_relays is
    'ARCHITECTURE.md S6.4: "Callbacks are relayed using one-time, audience-bound records." One-time '
    'is the claim updating at most one row. The audience binding is `claim_grant`: the record names '
    'the capability a caller must hold, and identity.grants is what answers whether they do. See '
    'ADR-0012.';
comment on column identity.oauth_relays.code is
    'The authorization code, in the clear, for the seconds between a redirect and a claim. It is '
    'NULLED by the claim, so a claimed row records that the callback arrived without holding a '
    'credential. Not hashed: it has to be returned verbatim, and hashing a value that must be '
    'replayed is a gesture rather than a control. What bounds it is that it is short-lived, '
    'single-use and unreachable without a service credential. It is never a provider TOKEN, which '
    'S6.4 forbids storing and which Platform never obtains, because Platform never exchanges it.';
comment on column identity.oauth_relays.state is
    'Opaque to Platform and carried verbatim. S6.4 gives state generation and validation to the '
    'owning service, which is the only party that can tell a real callback from a forged one.';
comment on column identity.oauth_relays.claimed_at is
    'Set once. The row survives its claim so an operator can answer "did that callback arrive" '
    'without the answer containing a credential.';

-- The claim's only lookup, and the sweep's.
create index oauth_relays_expires_at_idx
    on identity.oauth_relays (expires_at)
    where claimed_at is null;

-- ---------------------------------------------------------------------------------------------
-- pairing_codes: the single-use grant a new device presents to become trusted
-- ---------------------------------------------------------------------------------------------
--
-- ADR-0016. The one credential an untrusted party may ever present for enrollment, minted by a
-- session that already is trusted. The code itself exists in the response that carried it and in
-- whatever channel the user moved it through; like every other secret in this file, this table
-- holds its digest and nothing else.

create table identity.pairing_codes (
    pairing_code_id       uuid        primary key,
    user_id               uuid        not null references identity.users (user_id),
    created_by_session_id uuid        not null references identity.sessions (session_id),
    code_hash             bytea       not null,
    expected_kind         text,
    label                 text,
    created_at            timestamptz not null,
    expires_at            timestamptz not null,
    failed_attempts       integer     not null default 0,
    superseded_at         timestamptz,
    consumed_at           timestamptz,
    consumed_by_device_id uuid        references identity.registered_devices (device_id),

    -- What the initiating session approved. Null means "any kind"; a value must be honoured at
    -- consumption, which is what makes the approval UX contract more than decoration.
    constraint pairing_codes_expected_kind_is_known
        check (expected_kind is null or expected_kind in
               ('mobile', 'browser_extension', 'export_agent')),
    constraint pairing_codes_label_is_bounded
        check (label is null or length(label) between 1 and 120),
    constraint pairing_codes_code_hash_is_a_digest
        check (length(code_hash) = 32),
    constraint pairing_codes_expires_after_it_is_created
        check (expires_at > created_at),
    constraint pairing_codes_failed_attempts_is_bounded
        check (failed_attempts between 0 and 5),
    -- One end per code: consumed, or set aside, never both. There is deliberately no `expired`
    -- marker — expiry is `expires_at` compared against `now()`, a fact of time rather than a third
    -- column that could fall out of sync with it.
    constraint pairing_codes_ends_at_most_once
        check (((consumed_at is not null)::int + (superseded_at is not null)::int) <= 1),
    constraint pairing_codes_superseded_at_is_not_before_created_at
        check (superseded_at is null or superseded_at >= created_at),
    constraint pairing_codes_consumed_at_is_not_before_created_at
        check (consumed_at is null or consumed_at >= created_at)
);

comment on table identity.pairing_codes is
    'The short-lived, single-use bridge between an already-trusted session and a device that wants '
    'to become one (ADR-0016). Creating a code supersedes its owner''s previous pending one, so a '
    'user holds at most one live code; the partial indexes below are what make that hold under '
    'racing creates.';
comment on column identity.pairing_codes.expected_kind is
    'What the initiator approved pairing, when they pinned one. A presentation whose declared kind '
    'differs is refused exactly as an unknown code is.';
comment on column identity.pairing_codes.failed_attempts is
    'The durable, five-presentation brute-force budget for mismatched basic device attestation. '
    'A code at five is refused even when a later presentation has the matching attestation.';
comment on column identity.pairing_codes.label is
    'A human note from the initiator ("pixel phone"), echoed nowhere but listings an operator or '
    'the owner reads. Never used for authorization.';
comment on column identity.pairing_codes.superseded_at is
    'Set when a newer code replaces this one, including when the older code had already expired: '
    'supersession is what keeps an abandoned pending row from wedging the flow behind a sweep.';
comment on column identity.pairing_codes.consumed_by_device_id is
    'The device the code granted, recorded on the code so an audit question — "what did THIS code '
    'pair?" — answers from one row.';

-- The lookup when a code is presented, and the uniqueness behind single-use: consuming or
-- superseding removes the row from this index, so one digest can never be live twice.
create unique index pairing_codes_code_hash_key
    on identity.pairing_codes (code_hash)
    where consumed_at is null and superseded_at is null;

-- At most one pending code per user, enforced here rather than remembered in a handler: whichever
-- of two racing creations commits second loses, and no handler has to agree.
create unique index pairing_codes_one_pending_per_user_key
    on identity.pairing_codes (user_id)
    where consumed_at is null and superseded_at is null;

-- =================================================================================================
-- operations
-- =================================================================================================
--
-- DATA_MODEL.md lists operations, attempts, progress entries, results, safe errors, idempotency
-- records, projections, outbox and inbox under `operations.*`. Schedules live here too: a schedule
-- exists to produce an operation and an outbox row, both of which are already in this schema, so a
-- fourth schema for them would make every scheduler transaction a cross-schema write — which
-- DATA_MODEL.md prohibits — and would give the scheduler's database role reach into two schemas
-- instead of one. ADR-0013 records it. `platform_ingest` earned its own schema for the opposite
-- reason: it holds ingress state that neither `identity` nor `operations` has any claim on.

create schema operations;

comment on schema operations is
    'Platform-owned durable record of user-visible asynchronous work: operations, their attempts, '
    'progress history, typed result references and safe errors.';

-- ---------------------------------------------------------------------------------------------
-- The status vocabulary and its ordering
-- ---------------------------------------------------------------------------------------------

-- The seven states of ARCHITECTURE S5.4, which are exactly the seven variants of
-- `ratatoskr_operation_contracts::OperationStatus`. A test asserts that this list and the Rust enum
-- agree, so the two cannot drift.
--
-- `rank` exists to express DOMAIN.md invariant 4 ("progress cannot move a terminal operation
-- backward") and ARCHITECTURE S19 invariant 10 ("terminal operation states do not regress") as a
-- comparison rather than as a table of pairs. The four terminal states share rank 3: they are
-- mutually unreachable, which the transition guard enforces separately.
create function operations.status_rank(status text) returns int
    language sql immutable strict parallel safe
as $$
    select case status
        when 'accepted'            then 0
        when 'queued'              then 1
        when 'running'             then 2
        when 'succeeded'           then 3
        when 'partially_succeeded' then 3
        when 'failed'              then 3
        when 'cancelled'           then 3
    end
$$;

comment on function operations.status_rank(text) is
    'Lifecycle rank of an operation status. Monotonically non-decreasing along every legal '
    'transition; the four terminal states share the top rank and never transition to one another.';

create function operations.status_is_terminal(status text) returns boolean
    language sql immutable strict parallel safe
as $$
    select status in ('succeeded', 'partially_succeeded', 'failed', 'cancelled')
$$;

-- ---------------------------------------------------------------------------------------------
-- operations
-- ---------------------------------------------------------------------------------------------

create table operations.operations (
    operation_id       uuid        primary key,
    owner_user_id      uuid        not null,
    kind               text        not null,
    status             text        not null,
    stage              text,
    progress_percent   smallint,
    correlation_id     text        not null,
    causation_id       text,
    idempotency_key    text,
    retryable          boolean     not null default false,
    cancellation_requested_at timestamptz,
    accepted_at        timestamptz not null,
    status_changed_at  timestamptz not null,
    terminated_at      timestamptz,

    constraint operations_status_is_known
        check (status in ('accepted', 'queued', 'running',
                          'succeeded', 'partially_succeeded', 'failed', 'cancelled')),
    constraint operations_kind_is_bounded
        check (kind ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    constraint operations_stage_is_bounded
        check (stage is null or length(stage) between 1 and 60),
    constraint operations_progress_percent_is_a_percentage
        check (progress_percent is null or progress_percent between 0 and 100),
    -- The namespaced wire form from ratatoskr-contracts. The same grammar as identity.audit_events,
    -- deliberately, so an operation and its audit trail join on one string.
    constraint operations_correlation_id_is_namespaced
        check (correlation_id ~ '^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9._~-]{1,128}$'),
    constraint operations_causation_id_is_namespaced
        check (causation_id is null or causation_id ~ '^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9._~-]{1,128}$'),
    constraint operations_idempotency_key_is_bounded
        check (idempotency_key is null or length(idempotency_key) between 1 and 255),
    constraint operations_status_changed_at_is_not_before_accepted_at
        check (status_changed_at >= accepted_at),
    -- `terminated_at` is set if and only if the status is terminal. This is what makes
    -- "is it finished?" answerable from one column without knowing the vocabulary.
    constraint operations_terminated_at_matches_terminal_status
        check (operations.status_is_terminal(status) = (terminated_at is not null)),
    constraint operations_terminated_at_is_not_before_accepted_at
        check (terminated_at is null or terminated_at >= accepted_at)
);

comment on table operations.operations is
    'The durable user-visible record of asynchronous work (DOMAIN.md, Operation). It is the source '
    'from which `ratatoskr_operation_contracts::OperationSnapshot` is projected; every column here '
    'either appears in that contract or supports a rule this schema enforces.';
comment on column operations.operations.owner_user_id is
    'References identity.users(user_id) semantically, with no foreign key: DATA_MODEL.md forbids '
    'cross-schema foreign keys. The application enforces it.';
comment on column operations.operations.retryable is
    'Whether the CLIENT may resubmit. Not an internal retry budget: an attempt count belongs to '
    'operations.operation_attempts, and conflating the two is how a user is told to retry work that '
    'the platform is already retrying.';
comment on column operations.operations.cancellation_requested_at is
    'A request, not a state. The operation reaches `cancelled` only when the owning service confirms '
    'it stopped; ARCHITECTURE S14 forbids rolling back external actions that already completed.';

create index operations_owner_user_id_idx
    on operations.operations (owner_user_id, accepted_at desc);

create index operations_live_idx
    on operations.operations (status, status_changed_at)
    where terminated_at is null;

create index operations_correlation_id_idx
    on operations.operations (correlation_id);

-- ARCHITECTURE S8.1 scopes the idempotency key by actor, route and operation kind. The route is not
-- a column here — it belongs to the ledger below, which is where a reservation is taken — so what
-- this index enforces is the actor-and-kind half, on the operation itself.
create unique index operations_idempotency_key_scope_key
    on operations.operations (owner_user_id, kind, idempotency_key)
    where idempotency_key is not null;

-- ---------------------------------------------------------------------------------------------
-- The transition guard
-- ---------------------------------------------------------------------------------------------

create function operations.guard_status_transition() returns trigger
    language plpgsql
as $$
begin
    if new.status = old.status then
        -- Not a transition. Annotation of a terminal operation is permitted by DATA_MODEL.md
        -- ("terminal operation transitions are immutable except approved annotation"), so a status
        -- that did not change is never rejected here.
        return new;
    end if;

    if operations.status_is_terminal(old.status) then
        raise exception
            'operation % is terminal in status % and cannot transition to %',
            old.operation_id, old.status, new.status
            using errcode = 'check_violation';
    end if;

    if operations.status_rank(new.status) <= operations.status_rank(old.status) then
        raise exception
            'operation % cannot move backward from % to %',
            old.operation_id, old.status, new.status
            using errcode = 'check_violation';
    end if;

    if new.status_changed_at < old.status_changed_at then
        raise exception
            'operation % changed status at % which is before its previous change at %',
            old.operation_id, new.status_changed_at, old.status_changed_at
            using errcode = 'check_violation';
    end if;

    return new;
end;
$$;

comment on function operations.guard_status_transition() is
    'The durable backstop for DOMAIN.md invariant 4 and ARCHITECTURE S19 invariant 10. The '
    'authoritative transition table is `ratatoskr_platform_operations::Transition` in Rust; this '
    'trigger enforces the same rule for any writer that bypasses it, including a manual UPDATE. A '
    'test asserts the two agree, because two enforcement points that disagree are worse than one.';

create trigger operations_guard_status_transition
    before update of status, status_changed_at on operations.operations
    for each row
    execute function operations.guard_status_transition();

-- ---------------------------------------------------------------------------------------------
-- operation_attempts
-- ---------------------------------------------------------------------------------------------

create table operations.operation_attempts (
    attempt_id    uuid        primary key,
    operation_id  uuid        not null references operations.operations (operation_id) on delete cascade,
    attempt_number integer    not null,
    stage         text        not null,
    started_at    timestamptz not null,
    finished_at   timestamptz,
    outcome       text,

    constraint operation_attempts_attempt_number_is_positive
        check (attempt_number >= 1),
    constraint operation_attempts_stage_is_bounded
        check (length(stage) between 1 and 60),
    constraint operation_attempts_outcome_is_known
        check (outcome is null or outcome in ('succeeded', 'failed', 'abandoned')),
    constraint operation_attempts_finished_at_is_not_before_started_at
        check (finished_at is null or finished_at >= started_at),
    -- An outcome is recorded when, and only when, the attempt finished.
    constraint operation_attempts_outcome_matches_completion
        check ((outcome is not null) = (finished_at is not null))
);

comment on table operations.operation_attempts is
    'One execution of one operation step (DOMAIN.md, Attempt). Separate from the operation because '
    'at-least-once delivery means the same step runs more than once and the operation must stay one '
    'row (ARCHITECTURE S19 invariant 7).';

create unique index operation_attempts_operation_stage_number_key
    on operations.operation_attempts (operation_id, stage, attempt_number);

create index operation_attempts_operation_id_idx
    on operations.operation_attempts (operation_id);

-- ---------------------------------------------------------------------------------------------
-- operation_progress
-- ---------------------------------------------------------------------------------------------

create table operations.operation_progress (
    progress_id      uuid        primary key,
    operation_id     uuid        not null references operations.operations (operation_id) on delete cascade,
    observed_at      timestamptz not null,
    status           text        not null,
    stage            text,
    progress_percent smallint,
    message          text,

    constraint operation_progress_status_is_known
        check (status in ('accepted', 'queued', 'running',
                          'succeeded', 'partially_succeeded', 'failed', 'cancelled')),
    constraint operation_progress_stage_is_bounded
        check (stage is null or length(stage) between 1 and 60),
    constraint operation_progress_percent_is_a_percentage
        check (progress_percent is null or progress_percent between 0 and 100),
    -- A user-safe message only. ARCHITECTURE S5.4 calls these "user-safe messages" and S15 forbids
    -- internal detail on the public surface; the length bound is what stops a stack trace fitting.
    constraint operation_progress_message_is_a_short_safe_string
        check (message is null or (length(message) between 1 and 200 and message !~ '[\n\r]'))
);

comment on table operations.operation_progress is
    'The append-only progress history. The current status also lives denormalised on '
    'operations.operations because the public read path must not aggregate history on every poll; '
    'this table is what a client replays after an SSE reconnect (ARCHITECTURE S5.5).';

create index operation_progress_operation_observed_idx
    on operations.operation_progress (operation_id, observed_at);

-- ---------------------------------------------------------------------------------------------
-- operation_results
-- ---------------------------------------------------------------------------------------------

create table operations.operation_results (
    result_id     uuid        primary key,
    operation_id  uuid        not null references operations.operations (operation_id) on delete cascade,
    result_kind   text        not null,
    target        text        not null,
    payload       jsonb       not null,
    recorded_at   timestamptz not null,

    -- `OperationResultRef.result_kind`: what the target IS, e.g. `content.document`. Stored rather
    -- than derived from the target's entity kind, because the two answer different questions and
    -- deriving one from the other would fabricate a contract value at projection time.
    constraint operation_results_result_kind_is_a_dotted_name
        check (result_kind ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    -- The namespaced entity reference from ratatoskr-contracts, e.g. `document:<uuid7>`.
    constraint operation_results_target_is_namespaced
        check (target ~ '^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9._~-]{1,128}$'),
    constraint operation_results_payload_is_an_object
        check (jsonb_typeof(payload) = 'object')
);

comment on table operations.operation_results is
    'Typed result REFERENCES, never result content. The JSON payload preserves the published '
    'OperationResultRef, including its structured BlobRef and additive fields. ARCHITECTURE S4.2: '
    'Platform does not own extracted documents, summaries or snapshots.';

create index operation_results_operation_id_idx
    on operations.operation_results (operation_id);

-- ---------------------------------------------------------------------------------------------
-- operation_errors
-- ---------------------------------------------------------------------------------------------

create table operations.operation_errors (
    error_id      uuid        primary key,
    operation_id  uuid        not null references operations.operations (operation_id) on delete cascade,
    severity      text        not null,
    code          text        not null,
    message       text        not null,
    retryable     boolean     not null,
    payload       jsonb       not null,
    recorded_at   timestamptz not null,

    constraint operation_errors_severity_is_known
        check (severity in ('error', 'warning')),
    -- The stable machine-readable code grammar of ratatoskr-contracts' ErrorCode.
    constraint operation_errors_code_is_a_stable_code
        check (code ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    constraint operation_errors_message_is_a_short_safe_string
        check (length(message) between 1 and 200 and message !~ '[\n\r]'),
    constraint operation_errors_payload_is_an_object
        check (jsonb_typeof(payload) = 'object')
);

comment on table operations.operation_errors is
    'Safe errors and warnings attached to an operation. Core columns stay bounded and queryable; '
    'payload preserves the complete typed ErrorEnvelope or WarningEnvelope, including additive '
    'fields. ARCHITECTURE S15 and the contracts threat model forbid raw provider diagnostics.';
comment on column operations.operation_errors.severity is
    'ARCHITECTURE S14: a partial outcome is `partially_succeeded` with warnings, not a false '
    'success. Warnings and terminal errors therefore share a table and are distinguished here.';

create index operation_errors_operation_id_idx
    on operations.operation_errors (operation_id, recorded_at);

-- ---------------------------------------------------------------------------------------------
-- outbox
-- ---------------------------------------------------------------------------------------------
--
-- `ARCHITECTURE.md` S5.1 puts the outbox write in the same transaction as the operation write, and
-- S8.2 requires an inbox or processed-event record on the consuming side. Both live here because
-- they are the durable half of operation processing; neither is a general-purpose queue and nothing
-- outside `ratatoskr-platform-eventing` writes to them.

create table operations.outbox (
    outbox_id         uuid        primary key,
    message_id        uuid        not null,
    subject           text        not null,
    payload           jsonb       not null,
    operation_id      uuid        references operations.operations (operation_id) on delete cascade,
    enqueued_at       timestamptz not null,
    next_attempt_at   timestamptz not null,
    attempts          integer     not null default 0,
    claimed_until     timestamptz,
    claimed_by        text,
    published_at      timestamptz,
    last_error        text,
    dead_lettered_at  timestamptz,

    -- The NATS subject grammar, fixed by ADR-0005: a class token, then the contract type name whose
    -- own grammar `ratatoskr-contracts` already validates. Enforced here as well as in Rust because
    -- a subject is a security boundary: `ARCHITECTURE.md` S15 grants least-privilege publish
    -- permissions per subject, and a row that can hold an arbitrary string can hold one outside the
    -- allowlist the credential was issued for.
    constraint outbox_subject_is_a_valid_subject
        check (subject ~ '^(cmd|evt)\.[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}\.v[1-9][0-9]{0,2}$'),
    constraint outbox_attempts_is_not_negative
        check (attempts >= 0),
    constraint outbox_claim_is_whole
        check ((claimed_until is null) = (claimed_by is null)),
    constraint outbox_claimed_by_is_bounded
        check (claimed_by is null or length(claimed_by) between 1 and 120),
    -- A safe, bounded diagnostic. The publisher's error text reaches an operator through this
    -- column, so the same rule as `operation_errors.message` applies: no newline, so no stack trace.
    constraint outbox_last_error_is_a_short_safe_string
        check (last_error is null or (length(last_error) between 1 and 200 and last_error !~ '[\n\r]')),
    -- A row is published, or dead-lettered, or neither. Both at once would mean a message was
    -- delivered and also abandoned, which no reader could interpret.
    constraint outbox_is_not_both_published_and_dead_lettered
        check (published_at is null or dead_lettered_at is null),
    constraint outbox_published_at_is_not_before_enqueued_at
        check (published_at is null or published_at >= enqueued_at),
    -- A command may not be larger than the bus will carry.
    --
    -- The publisher serializes `payload` straight onto NATS. A server refuses a publish above
    -- `max_payload` — 1 MiB by default and the value `deploy/nats/ratatoskr.conf` leaves at its
    -- default — and the refusal arrives as a message that was not acknowledged. The outbox reads
    -- that as a transport failure and does exactly the wrong thing with it: it backs the row off and
    -- retries, forever, until twelve attempts are spent.
    --
    -- The cost is not one row. `pump::run_once` claims a batch of 64 and publishes them in order, so
    -- an oversized row consumes a claim slot every pass, and its retries are indistinguishable from
    -- a broker outage in `last_error`. The whole queue behind it is delayed by a message that cannot
    -- succeed on any attempt.
    --
    -- Reachable by an operator rather than a client: `operations.schedules.payload` is arbitrary
    -- jsonb an operator writes, and it becomes the `payload` member of the command envelope. The two
    -- client-facing producers both emit `{"url": ...}` bounded to 2048 characters.
    --
    -- Refusing the write is the right end to refuse at. The insert happens inside the transaction
    -- that accepts the work, so a payload that could never be delivered fails where the caller can
    -- be told, instead of being accepted durably and then discovering it is undeliverable in a
    -- background loop.
    constraint outbox_payload_fits_in_a_nats_message
        check (octet_length(payload::text) <= 786432)
);

comment on table operations.outbox is
    'The durable hand-off between a database transaction and the bus. A command or event is written '
    'here in the SAME transaction as the state change that justifies it (ARCHITECTURE.md S5.1 step '
    '9), so the two cannot disagree: either both are committed or neither is. A publisher then moves '
    'rows to the bus at least once, which is why every consumer deduplicates.';
comment on column operations.outbox.message_id is
    'The envelope identity carried inside `payload`, lifted out so it can be indexed. It is the '
    'deduplication key a consumer stores in its inbox, and the unique index below makes enqueuing '
    'idempotent: a retried request that re-runs the same transaction cannot produce two messages.';
comment on column operations.outbox.payload is
    'The serialized envelope. Bounded to 768 KiB, which is the NATS default max_payload of 1 MiB '
    'with room for the headers and for the difference between jsonb''s own text rendering and '
    'serde_json''s. Not a tuning knob: a larger message is a different design — a reference to a '
    'blob — rather than a larger limit. Raising max_payload on the server without raising this '
    'constraint is safe; the reverse is not.';
comment on column operations.outbox.claimed_until is
    'A lease, not a flag. A publisher that crashes mid-batch leaves rows claimed; the lease expiring '
    'is what returns them to the queue without an operator having to notice.';
comment on column operations.outbox.next_attempt_at is
    'When this row may next be claimed. Bounded exponential backoff (ARCHITECTURE.md S8.2) is '
    'expressed by moving this forward rather than by sleeping, so backoff survives a restart.';
comment on column operations.outbox.dead_lettered_at is
    'AGENTS.md: exhausted work goes to a diagnosable dead-letter path rather than being silently '
    'dropped. The row stays, with its last error and attempt count, and stops being claimable.';

create unique index outbox_message_id_key
    on operations.outbox (message_id);

-- The publisher's only query: the oldest due, unclaimed, unfinished rows. A partial index keeps it
-- proportional to the backlog rather than to the history.
create index outbox_due_idx
    on operations.outbox (next_attempt_at, enqueued_at)
    where published_at is null and dead_lettered_at is null;

create index outbox_operation_id_idx
    on operations.outbox (operation_id)
    where operation_id is not null;

-- ---------------------------------------------------------------------------------------------
-- inbox
-- ---------------------------------------------------------------------------------------------

create table operations.inbox (
    message_id    uuid        primary key,
    subject       text        not null,
    producer      text        not null,
    received_at   timestamptz not null,
    processed_at  timestamptz,
    outcome       text,

    constraint inbox_subject_is_a_valid_subject
        check (subject ~ '^(cmd|evt)\.[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}\.v[1-9][0-9]{0,2}$'),
    -- The wire producer identity, whose grammar `ratatoskr-contracts` validates. Bounded here so an
    -- unbounded value cannot arrive from a forged message (THREAT_MODEL.md, event forgery).
    constraint inbox_producer_is_bounded
        check (length(producer) between 1 and 64),
    constraint inbox_outcome_is_known
        check (outcome is null or outcome in ('applied', 'duplicate', 'stale', 'rejected')),
    constraint inbox_outcome_matches_completion
        check ((outcome is not null) = (processed_at is not null))
);

comment on table operations.inbox is
    'The processed-event record. Delivery is at-least-once (ARCHITECTURE.md S19 invariant 7), so the '
    'primary key IS the deduplication: inserting a message id that is already present is how a '
    'consumer discovers it has seen the message, in the same statement rather than in a separate '
    'read that another worker could race.';
comment on column operations.inbox.outcome is
    'What handling the message produced, in the vocabulary of the operation transition table: a '
    'duplicate and a stale delivery are ordinary traffic and are counted, not failed (ADR-0002).';

create index inbox_unprocessed_idx
    on operations.inbox (received_at)
    where processed_at is null;

-- ---------------------------------------------------------------------------------------------
-- idempotency_records
-- ---------------------------------------------------------------------------------------------
--
-- The ledger `ARCHITECTURE.md` S8.1 specifies. A separate table rather than columns on
-- `operations.operations` because a reservation exists BEFORE an operation does: the key is claimed,
-- then the work is decided, and a request that is rejected after reserving must still hold its key
-- so a retry gets the same answer.

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

-- ---------------------------------------------------------------------------------------------
-- schedules
-- ---------------------------------------------------------------------------------------------

create table operations.schedules (
    schedule_id       uuid        primary key,
    name              text        not null,
    owner_user_id     uuid        not null,
    command_type      text        not null,
    operation_kind    text        not null,
    payload           jsonb       not null default '{}'::jsonb,
    interval_seconds  integer     not null,
    next_due_at       timestamptz not null,
    catch_up          text        not null default 'skip',
    enabled           boolean     not null default false,
    created_at        timestamptz not null,
    updated_at        timestamptz not null,
    last_published_at timestamptz,

    -- An operator-chosen name that becomes a metric label, so its grammar is the cardinality
    -- bound: no dots, no spaces, no unbounded length. `ARCHITECTURE.md` S16 requires scheduler
    -- drift as a signal, and a label whose values are rows in this table is bounded by how many
    -- rows an operator writes.
    constraint schedules_name_is_a_label
        check (name ~ '^[a-z][a-z0-9_-]{0,63}$'),
    -- The contract type name, WITH its version suffix: it is published as `cmd.<command_type>`,
    -- and `operations.outbox.subject` enforces exactly this grammar one table over. Checked here
    -- as well so a row that could never be published is refused where it is written, not where it
    -- is read.
    constraint schedules_command_type_is_a_contract_type
        check (command_type ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}\.v[1-9][0-9]{0,2}$'),
    -- The type, never the subject. `cmd.` is a legal first segment of the grammar above, so
    -- `cmd.github.sync.requested.v1` — the string an operator copies out of `operations.outbox`
    -- when writing their first schedule — passes it and is then published to
    -- `cmd.cmd.github.sync.requested.v1`, a subject nothing subscribes to and no test looks at.
    -- The class prefix is added by the publisher and belongs to nobody else.
    constraint schedules_command_type_is_not_already_a_subject
        check (command_type !~ '^(cmd|evt)\.'),
    -- The same grammar as `operations.operations.kind`. Stated separately from `command_type`
    -- rather than derived from it: stripping a version suffix and guessing the rest is the kind of
    -- cleverness that produces an operation kind nobody chose.
    constraint schedules_operation_kind_is_bounded
        check (operation_kind ~ '^[a-z][a-z0-9_]{0,31}(\.[a-z][a-z0-9_]{0,31}){1,3}$'),
    -- The domain half of the command envelope. An object, never a scalar or an array: the envelope
    -- nests it under `payload`, and a consumer reading `payload.url` of a JSON number gets a type
    -- error at the far end of an asynchronous hop.
    constraint schedules_payload_is_an_object
        check (jsonb_typeof(payload) = 'object'),
    -- One minute to one year. The floor is not arbitrary: below it the tick interval and the drift
    -- it produces are the same order as the schedule itself, and four cores on one host
    -- (`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`) are not a load generator.
    constraint schedules_interval_is_between_a_minute_and_a_year
        check (interval_seconds between 60 and 31536000),
    constraint schedules_catch_up_is_known
        check (catch_up in ('skip', 'catch_up')),
    constraint schedules_updated_at_is_not_before_created_at
        check (updated_at >= created_at)
);

comment on table operations.schedules is
    'A periodic command, defined as data. ARCHITECTURE.md S10: Scheduler is a thin command '
    'publisher that does not import domain repositories and decides no domain behaviour, so what '
    'to publish and how often is a row rather than a constant in a binary. There is no route that '
    'writes this table: an operator inserts a schedule, which is why every column carries a CHECK '
    'an operator can trip.';
comment on column operations.schedules.owner_user_id is
    'References identity.users(user_id) semantically, with no foreign key: DATA_MODEL.md forbids '
    'cross-schema foreign keys. Every one of S10''s example schedules is per-user work — sync THIS '
    'account, snapshot THESE bookmarks — so the operation an occurrence creates has a real owner '
    'and the command carries a real principal. There is no system principal, and adding one would '
    'be a second kind of actor for the authorization model to grow a hole around.';
comment on column operations.schedules.next_due_at is
    'The next occurrence, and also the phase of the schedule: the grid is next_due_at + k * '
    'interval_seconds, so "every 24 hours at 03:00" is an interval of 86400 anchored at 03:00. '
    'That is why there is no cron expression here — a parser and its dependency would buy the '
    'phase this column already carries.';
comment on column operations.schedules.catch_up is
    'skip | catch_up. S10 requires catch-up policy to be explicit per schedule. `skip` publishes '
    'the current occurrence and moves to the next grid point after now, discarding what was missed '
    '— correct for a snapshot, where only the latest matters. `catch_up` advances one interval at '
    'a time, so every missed occurrence is eventually published — correct for an incremental sync '
    'that must not have gaps. Catch-up is bounded in code: a schedule enabled with a next_due_at '
    'far in the past jumps forward rather than publishing a year of commands.';
comment on column operations.schedules.enabled is
    'A schedule is created disabled. Enabling one starts publishing commands to a domain service '
    'that may not be deployed, which is an operator decision and not a consequence of writing a '
    'row.';

create unique index schedules_name_key
    on operations.schedules (name);

-- The only query the publisher runs. Partial, because a disabled schedule is never due.
create index schedules_due_idx
    on operations.schedules (next_due_at)
    where enabled;

-- ---------------------------------------------------------------------------------------------
-- schedule_occurrences
-- ---------------------------------------------------------------------------------------------

create table operations.schedule_occurrences (
    occurrence_id uuid        primary key,
    schedule_id   uuid        not null references operations.schedules (schedule_id) on delete cascade,
    due_at        timestamptz not null,
    published_at  timestamptz not null,
    drift_seconds bigint      not null,
    operation_id  uuid        references operations.operations (operation_id) on delete set null,

    constraint schedule_occurrences_drift_is_not_negative
        check (drift_seconds >= 0),
    constraint schedule_occurrences_published_at_is_not_before_due_at
        check (published_at >= due_at)
);

comment on table operations.schedule_occurrences is
    'One row per occurrence actually published. ARCHITECTURE.md S14: "Scheduler occurrences use '
    'deterministic IDs to prevent duplicate work", and this is where that determinism is enforced '
    'rather than assumed. occurrence_id is a name-based UUID over (schedule_id, due_at), so the '
    'same due time can only ever produce the same identifier, and the primary key refuses the '
    'second one.';
comment on column operations.schedule_occurrences.occurrence_id is
    'Also the outbox message_id and the operation''s idempotency key, so one identifier guards the '
    'same occurrence at all three layers. The outbox alone would nearly do it — enqueue is '
    'idempotent on message_id and no row is ever deleted — but "nearly" depends on the outbox '
    'never gaining a retention sweep, and a duplicate-suppression guarantee that a future cleanup '
    'can silently remove is not a guarantee.';
comment on column operations.schedule_occurrences.drift_seconds is
    'published_at - due_at. The scheduler-drift signal ARCHITECTURE.md S16 requires, stored as well '
    'as exported: the metric is the last value and this is the history.';
comment on column operations.schedule_occurrences.operation_id is
    'What the occurrence produced (S10 step 6, "record outcome"). Null only after an operation has '
    'been deleted, which cascades to nothing here: the record that the occurrence happened outlives '
    'the work it started.';

-- The database-level form of the same rule, independent of whether the derivation in Rust is
-- correct. A bug that produced colliding or diverging identifiers would otherwise publish twice
-- and leave no trace of having done so.
create unique index schedule_occurrences_schedule_id_due_at_key
    on operations.schedule_occurrences (schedule_id, due_at);

-- The operator's question: what has this schedule been doing lately.
--
-- The retention sweep prunes this table on `RATATOSKR__RETENTION__SCHEDULE_OCCURRENCE_DAYS`,
-- default 90 days. The consequence worth knowing: that window is how far back an operator may move
-- a schedule's `next_due_at` before a rewind republishes instead of being suppressed.
create index schedule_occurrences_schedule_id_published_at_idx
    on operations.schedule_occurrences (schedule_id, published_at desc);

-- =================================================================================================
-- platform_ingest
-- =================================================================================================
--
-- `docs/ARCHITECTURE.md` S4.1 lists `platform_ingest.*` among the three schemas Platform owns, and
-- ADR-0009 settled the spelling: the word is `ingest` wherever it is an identifier, so this schema,
-- the crate, the binary, the database role of S18 and the `/v1/ingest` path prefix are all one
-- string.
--
-- One table. `ARCHITECTURE.md` S9 gives ingest six steps, and only the first — "source
-- authentication or signature validation" — needs state the other two schemas do not already have:
--
--   * receipt deduplication (step 2) reuses `operations.idempotency_records`, whose scope is
--     already actor + route + kind + key. The source is folded into the key, so two sources owned
--     by one user cannot collide on a shared external identifier;
--   * normalization (step 3) is a pure function of the request body and stores nothing;
--   * routing (step 4) is the `target` column below;
--   * command publication (step 5) is `operations.outbox`, unchanged;
--   * receipt status projection (step 6) IS the operation: the 202 returns its identifier and
--     `GET /v1/operations/{id}` reports its status. A second status column here would be a copy
--     that can disagree with the record it copies.

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
    'makes a webhook submission reachable at GET /v1/operations/{id} by its owner and by nobody '
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
