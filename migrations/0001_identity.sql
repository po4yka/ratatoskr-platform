-- Milestone 2: the `identity` schema.
--
-- ARCHITECTURE.md S6.1 names the tables; DATA_MODEL.md adds grants, assertion nonces and audit
-- context. This migration creates all of them and nothing else: `operations` is 0002, and
-- `platform_ingest` is milestone 7.
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
--     a CHECK constraint is altered by one statement and matches the expand/migrate/contract policy
--     DATA_MODEL.md requires.
--
--   * Every timestamp is `timestamptz`. `timestamp` would silently record the server's local time.
--
--   * A secret is stored as a hash in `bytea` and the column is named `*_hash`. There is no column
--     anywhere in this schema that can hold a credential in a readable form (SECURITY.md).
--
--   * Foreign keys stay inside this schema. DATA_MODEL.md forbids cross-schema foreign keys, so
--     `operations` will reference a user by an unenforced `uuid` column, not by a REFERENCES clause.

create schema if not exists identity;

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
        check ((kind = 'device') = (device_id is not null))
);

comment on table identity.sessions is
    'Revocable authentication state. ARCHITECTURE S6.2 lists the five kinds; each has its own '
    'audience, lifetime, rotation and revocation semantics, which is why kind and audience are '
    'separate columns rather than one conflated value.';
comment on column identity.sessions.revoked_at is
    'Set in place rather than deleting the row, so a revoked session remains auditable for its '
    'retention window. Liveness is `revoked_at is null and expires_at > now()`.';

create index sessions_user_id_live_idx
    on identity.sessions (user_id)
    where revoked_at is null;

create index sessions_expires_at_idx
    on identity.sessions (expires_at)
    where revoked_at is null;

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
