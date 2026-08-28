# Bus credentials

`ratatoskr-edge` publishes commands from Platform's outbox. `ratatoskr-x`,
`ratatoskr-instagram`, and `ratatoskr-threads` each use an individual NKey for a filtered durable
JetStream consumer: respectively `cmd.x.capture.requested.v1`,
`cmd.instagram.capture.requested.v1`, and `cmd.threads.capture.requested.v1`. The three identities
can inspect, pull from, and acknowledge only their named pre-provisioned durable and receive only
their private replies. Edge creates and verifies those fixed durable/filter pairs at startup. The
owners cannot create consumers: granting `$JS.API.>` would let a compromised identity choose a
foreign filter and observe another provider's commands. `ratatoskr-ingest` and
`ratatoskr-scheduler` hold none: they write commands into `operations.outbox` and edge is the only
process that moves them onto the bus (ADR-0013).

`ratatoskr-telegram-dispatcher` has its own equally narrow identity. It can inspect, pull, and
acknowledge only `ratatoskr_telegram_notifications` on `ratatoskr_events`, filtered to
`evt.platform.notification.raised.v1`. Edge pre-provisions and validates that pull consumer;
Telegram never receives consumer-create authority and must refuse readiness if the durable differs.

The Threads identity additionally publishes only its durable facts:
`evt.platform.operation.reported.v1`, `evt.social.source.captured.v1`, and
`evt.social.source.updated.v1`. X and Instagram receive no event-publish permission until they
have a durable outbox publisher for a fact they actually produce.

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

`ratatoskr-edge` there is the role's OWN group, which exists only if the users were created with
`--user-group` (`deploy/README.md` step 1). It is also what the unit must name in `Group=`: systemd
sets the primary group and does not add the user's other memberships, so a unit that says
`Group=ratatoskr` produces a process that cannot read this file. That is not hypothetical — it is
how milestone 10's first start failed, with "the bus credential could not be read".

Repeat generation and installation for `telegram.nkey`, `x.nkey`, `instagram.nkey`, and
`threads.nkey`, using their matching service group. Telegram's seed is installed as:

```bash
sudo install -m 0640 -o root -g ratatoskr-telegram-dispatcher \
  /path/to/generated-telegram-seed /etc/ratatoskr/telegram.nkey
```

Put only each public `U...` key in `ratatoskr.conf`, replacing its matching
`UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_*` token before reloading NATS. The seed stays
outside Git and is referenced only by the owning service's NKey seed-path setting.

The seed never appears in the environment, in a URL or in a log line: the unit names its **path**,
startup rule V16 refuses a relative path or a missing file, and `NatsPublisher::connect_with_nkey`
reads it once and hands it straight to the client. Startup rule V13 refuses a
`RATATOSKR__BUS__URL` that carries user information, so there is no second place to put it.

## Rotating an NKey

1. Generate a new pair.
2. Add the new public nkey to `ratatoskr.conf` as a **second** user with exactly the old user's
   permissions.
3. `nats-server --signal reload` — the server accepts both.
4. Atomically replace the owning role's seed file and restart only that role. For Telegram:

   ```bash
   sudo install -m 0640 -o root -g ratatoskr-telegram-dispatcher \
     /path/to/new-telegram-seed /etc/ratatoskr/telegram.nkey.new
   sudo mv /etc/ratatoskr/telegram.nkey.new /etc/ratatoskr/telegram.nkey
   sudo systemctl restart ratatoskr-telegram-dispatcher
   curl --fail --silent http://127.0.0.1:9468/health/ready
   ```

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

Durable consumers on `ratatoskr_events` are `platform_edge_projection` and the pull consumer
`ratatoskr_telegram_notifications`, whose sole filter is
`evt.platform.notification.raised.v1` and whose acknowledgement policy is explicit.

## Provisioning and inspection for Telegram

Provision in this order so the least-privilege Telegram process never needs topology authority:

1. Generate `telegram.nkey`, install its seed at `/etc/ratatoskr/telegram.nkey`, replace the
   Telegram public-key placeholder in `ratatoskr.conf`, and validate the candidate configuration.
2. Reload NATS so the new public identity is accepted.
3. Restart Edge. It creates or verifies `ratatoskr_events` and
   `ratatoskr_telegram_notifications`; a mismatch makes Edge refuse startup without modifying the
   existing durable.
4. Inspect the durable with Edge's operator credential before starting Telegram:

   ```bash
   nats --server nats://127.0.0.1:4222 --nkey /etc/ratatoskr/edge.nkey \
     consumer info ratatoskr_events ratatoskr_telegram_notifications
   ```

   Confirm `Filter Subject: evt.platform.notification.raised.v1`, explicit acknowledgements, and
   pull delivery. Then start `ratatoskr-telegram-dispatcher` and check its private readiness port.

   ```bash
   sudo systemctl start ratatoskr-telegram-dispatcher
   curl --fail --silent http://127.0.0.1:9468/health/ready
   ```

Rollback stops the dispatcher first and preserves the durable cursor:

```bash
sudo systemctl stop ratatoskr-telegram-dispatcher
```

Restore the prior Telegram binary/configuration and public-key stanza, reload NATS, and restart the
prior dispatcher only after Edge and the consumer inspection are healthy. Do not delete or recreate
`ratatoskr_telegram_notifications`: that discards its delivery cursor and can replay or skip work.

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
