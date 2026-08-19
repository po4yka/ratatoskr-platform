-- Milestone 8: the one-time, audience-bound record an OAuth callback is relayed through.
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
--
-- The conventions of 0001 to 0005 apply unchanged.

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
