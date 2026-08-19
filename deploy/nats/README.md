# The bus credential

One identity, `ratatoskr-edge`. `ratatoskr-ingest` and `ratatoskr-scheduler` hold none: they write
commands into `operations.outbox` and edge is the only process that moves them onto the bus
(ADR-0013).

## Generating it

An nkey pair, not a `.creds` file — the reasoning is in `ratatoskr.conf`. Either tool produces one,
and both ship an `arm64` binary:

```bash
# nk, from github.com/nats-io/nkeys
nk -gen user -pubout
# nsc, from github.com/nats-io/nsc
nsc generate nkey --user
```

Both print two lines. The one starting with `U` is the **public** key: it goes into
`ratatoskr.conf`, in the repository, and is not a secret. The one starting with `SU` is the
**seed**: it is the credential.

```bash
sudo install -d -m 0750 -o root -g ratatoskr /etc/ratatoskr
printf '%s' 'SU...' | sudo tee /etc/ratatoskr/edge.nkey > /dev/null
sudo chown root:ratatoskr-edge /etc/ratatoskr/edge.nkey
sudo chmod 0640 /etc/ratatoskr/edge.nkey
```

The seed never appears in the environment, in a URL or in a log line: the unit names its **path**,
startup rule V16 refuses a relative path or a missing file, and `NatsPublisher::connect_with_nkey`
reads it once and hands it straight to the client. Startup rule V13 refuses a
`RATATOSKR__BUS__URL` that carries user information, so there is no second place to put it.

## Rotating it

1. Generate a new pair.
2. Add the new public nkey to `ratatoskr.conf` as a **second** user with the same permissions.
3. `nats-server --signal reload` — the server accepts both.
4. Replace `/etc/ratatoskr/edge.nkey` and `systemctl restart ratatoskr-edge`.
5. Remove the old user from `ratatoskr.conf` and reload again.

Steps 2 and 5 are separate reloads on purpose: with one user removed in the same change, a restart
that fails leaves the deployment with no working credential.

## Streams

Declared by `ratatoskr-edge` at startup, from `platform_eventing::stream`, so the names here are
that module's constants and not a second copy of them:

| Stream | Subjects | When full | Retention |
|---|---|---|---|
| `ratatoskr_commands` | `cmd.>` | **refuse the publish** — the outbox is the durable copy and a refusal becomes a visible retry | 1 GiB / 7 days |
| `ratatoskr_events` | `evt.>` | drop the oldest — an event is a fact its producer already recorded | 1 GiB / 7 days |

Durable consumer: `platform_edge_projection` on `ratatoskr_events`.

**A stream that already exists is not reconciled.** `get_or_create_stream` returns the existing one
and says nothing about the difference, so a stream created once from the client's defaults keeps
`max_bytes: -1` and `DiscardPolicy::Old` forever while every later deployment reports success and
changes nothing. `platform_eventing::stream::ensure` computes the difference instead and edge logs
it at WARN, naming each differing field. Fixing it is an operator action against the broker, not a
redeploy:

```bash
nats stream rm ratatoskr_commands -f   # then restart ratatoskr-edge, which recreates it correctly
```

Removing a command stream discards whatever it held. The outbox rows are the durable copy, so the
commands come back — but only those the pump has not yet marked published.
