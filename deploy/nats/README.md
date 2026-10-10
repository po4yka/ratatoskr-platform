# Bus credentials

`ratatoskr.conf` is the only deployed ACL and it holds thirteen nkey identities. `ratatoskr-edge`
is the only one that creates anything: it declares both streams, every fixed durable below and the
completion bucket, and it publishes every command the outbox pump emits. Every other identity can
inspect, pull from and acknowledge only the durables it owns, publish only the subjects it
produces, and receive only its private replies. None of them holds `$JS.API.>`, `cmd.>` or
`evt.>`: an identity allowed to create a consumer could pick a foreign filter and read another
service's messages through its private inbox. `ratatoskr-ingest` and `ratatoskr-scheduler` hold
none: they write commands into `operations.outbox` and edge is the only process that moves them
onto the bus (ADR-0013).

Each service that is not Platform carries a byte-equal copy of its own stanza in its repository
(`deploy/nats/identity*.conf`), and a workspace test fails when a copy diverges from this file.

| Identity | Seed file | Environment variable that names it | Used by |
|---|---|---|---|
| `EDGE` | `/etc/ratatoskr/edge.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-edge` |
| `TELEGRAM` | `/etc/ratatoskr/telegram.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-telegram-dispatcher` |
| `CHATGPT` | `/etc/ratatoskr/chatgpt.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-chatgpt` |
| `CLAUDE` | `/etc/ratatoskr/claude.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-claude` |
| `X` | `/etc/ratatoskr/x.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-x` |
| `INSTAGRAM` | `/etc/ratatoskr/instagram.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-instagram` |
| `THREADS` | `/etc/ratatoskr/threads.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-threads` |
| `EXTRACTOR` | `/etc/ratatoskr/extractor.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-extractor` |
| `EXTRACTOR_BROWSER_WORKER` | `/etc/ratatoskr/extractor-browser-worker.nkey` | `BROWSER_NKEY_SEED_PATH` | the extractor's browser worker |
| `KNOWLEDGE` | `/etc/ratatoskr/knowledge.nkey` | `RATATOSKR__CHANNEL_RECAP__BUS_CREDENTIALS_FILE` and `RATATOSKR__INGEST__BUS_CREDENTIALS_FILE` | `ratatoskr-knowledge` |
| `GITHUB` | `/etc/ratatoskr/github.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-github` |
| `VAULT` | `/etc/ratatoskr/vault.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` | `ratatoskr-vault` |
| `CHANNEL_DIGESTS` | `/etc/ratatoskr/channel-digests.nkey` | `RATATOSKR__BUS__NKEY_SEED_PATH` (worker role only) | `ratatoskr-channel-digests` |

The seed file name is the identity name in lowercase with dashes, which is also what
`render-nats-config` writes (see below).

## Fixed durables and the completion bucket

Edge creates every row below at startup through `platform_eventing::ensure_fixed_topology`,
after it has declared the two streams. All of them are pull consumers with instant replay and no
`deliver_subject`. A service hard-codes the same name and filter and verifies them at startup; it
never creates one. A durable that already exists and differs from its row makes Edge refuse to
start, and Edge never modifies one. The table is copied from the constants in
`crates/eventing/src/stream.rs`, and `services/edge/tests/deployment_profile.rs` fails when a name
or a filter here stops matching them.

| Durable | Stream | Filter | Ack wait | max_deliver | Owner identity |
|---|---|---|---|---|---|
| `platform_edge_projection` | `ratatoskr_events` | `evt.platform.operation.reported.v1` | 30 s | -1 | `EDGE` (created by Edge's own consumer) |
| `platform_ai_archive_chatgpt_projection` | `ratatoskr_events` | `evt.ai-archive.chatgpt.operation.reported.v1` | 30 s | -1 | `EDGE` |
| `platform_ai_archive_claude_projection` | `ratatoskr_events` | `evt.ai-archive.claude.operation.reported.v1` | 30 s | -1 | `EDGE` |
| `ratatoskr_telegram_notifications` | `ratatoskr_events` | `evt.platform.notification.raised.v1` | 30 s | -1 | `TELEGRAM` |
| `ratatoskr_x_browser_capture` | `ratatoskr_commands` | `cmd.x.capture.requested.v1` | 30 s | -1 | `X` |
| `ratatoskr_instagram_browser_capture` | `ratatoskr_commands` | `cmd.instagram.capture.requested.v1` | 30 s | -1 | `INSTAGRAM` |
| `threads_browser_capture` | `ratatoskr_commands` | `cmd.threads.capture.requested.v1` | 30 s | -1 | `THREADS` |
| `ratatoskr_extractor_capture` | `ratatoskr_commands` | `cmd.content.capture.requested.v1` | 30 s | 12 | `EXTRACTOR` |
| `ratatoskr_browser_worker` | `ratatoskr_commands` | `cmd.content.render.requested.v1` | 300 s | 12 | `EXTRACTOR_BROWSER_WORKER` |
| `ratatoskr_knowledge_channel_recap` | `ratatoskr_commands` | `cmd.knowledge.channel_digest_recap.requested.v1` | 30 s | -1 | `KNOWLEDGE` |
| `ratatoskr_channel_digest_subscriptions` | `ratatoskr_commands` | `cmd.channel_digest.subscription.set_requested.v1` | 30 s | -1 | `CHANNEL_DIGESTS` |
| `ratatoskr_channel_digest_runs` | `ratatoskr_commands` | `cmd.channel_digest.run.requested.v1` | 30 s | -1 | `CHANNEL_DIGESTS` |
| `ratatoskr_channel_digest_schedule_occurrences` | `ratatoskr_commands` | `cmd.channel_digest.schedule.occurrence_requested.v1` | 30 s | -1 | `CHANNEL_DIGESTS` |
| `ratatoskr_vault_backup_policy` | `ratatoskr_commands` | `cmd.vault.backup_policy.apply_requested.v1` | 30 s | -1 | `VAULT` |
| `ratatoskr_knowledge_documents` | `ratatoskr_events` | `evt.content.document.extracted.v1` | 120 s | -1 | `KNOWLEDGE` |
| `ratatoskr_knowledge_social_sources` | `ratatoskr_events` | `evt.social.source.>` | 120 s | -1 | `KNOWLEDGE` |
| `ratatoskr_knowledge_ai_archive` | `ratatoskr_events` | `evt.ai_archive.>` | 120 s | -1 | `KNOWLEDGE` |
| `ratatoskr_knowledge_repository_requests` | `ratatoskr_events` | `evt.knowledge.repository_analysis.requested.v1` | 120 s | -1 | `KNOWLEDGE` |
| `ratatoskr_github_analysis_completed` | `ratatoskr_events` | `evt.knowledge.repository_analysis.completed.v1` | 30 s | -1 | `GITHUB` |
| `ratatoskr_github_analysis_failed` | `ratatoskr_events` | `evt.knowledge.repository_analysis.failed.v1` | 30 s | -1 | `GITHUB` |
| `ratatoskr_github_policy_acknowledged` | `ratatoskr_events` | `evt.vault.backup_policy.acknowledged.v1` | 30 s | -1 | `GITHUB` |
| `ratatoskr_x_extractor_reports` | `ratatoskr_events` | `evt.platform.operation.reported.v1` | 30 s | -1 | `X` |
| `ratatoskr_channel_digest_recap_completed` | `ratatoskr_events` | `evt.knowledge.channel_digest_recap.completed.v1` | 30 s | -1 | `CHANNEL_DIGESTS` |
| `ratatoskr_channel_digest_recap_failed` | `ratatoskr_events` | `evt.knowledge.channel_digest_recap.failed.v1` | 30 s | -1 | `CHANNEL_DIGESTS` |
| `ratatoskr_extractor_render_awaits` | `ratatoskr_events` | `evt.content.render.>` | 30 s, no acks, deliver new | -1 | `EXTRACTOR` |

`platform_schedule_registration` (filter `cmd.platform.schedule.registration_requested.v1`) is
created by Edge's own consumer when it starts, so it is not in the fixed table.

The KV bucket `browser_worker_completions` (keys expire after 24 hours, direct get allowed) is
created by Edge and owned by `EXTRACTOR_BROWSER_WORKER`, which opens it with three grants:
`$JS.API.STREAM.INFO.KV_browser_worker_completions`,
`$JS.API.DIRECT.GET.KV_browser_worker_completions.>` and `$KV.browser_worker_completions.>`. The
real-broker matrix proves those three are enough for the client the binaries link. If a client
upgrade needs another KV subject, add exactly that subject and never `$JS.API.>`.

## Rollout order

Services fail startup, and systemd restarts them, until Edge has provisioned their durable. On a
fresh broker, and whenever this file changes:

1. Generate the seeds, put each public key into `ratatoskr.conf` and reload NATS
   (`nats-server --signal reload`). The extractor's old broad stanza is replaced in the same reload.
2. Restart Edge. It creates or verifies both streams, every durable and the bucket.
3. Start everything else: the extractor and its browser worker, Knowledge, channel-digests, GitHub,
   Vault, the social services and the AI archive importers.

In short: reload NATS, restart Edge, then start the services. Stop in the reverse order. Never
delete a provisioned durable to roll back: its cursor is the delivery state.

## Known residual risk

`evt.platform.operation.reported.v1` is one shared subject and the receiver trusts the envelope's
`producer` field, so the identities allowed to publish it (`EXTRACTOR`, `X`, `INSTAGRAM`,
`THREADS` and `CHANNEL_DIGESTS`) can forge a report for each other, and `X` can read every
tenant's operation-report metadata through `ratatoskr_x_extractor_reports`. Per-producer report
subjects would need a contract change.

## A refusal is silent

A denied publish is never answered, so a client reports "didn't receive ack in time" and the
outbox records "the message was not acknowledged by the bus", the same text a down broker
produces. The server log is the only place that says which: look for a `Publish Violation` naming
the subject and the identity.

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

Generate all thirteen seeds in one loop (the names are the seed file names of the table above),
then install each one as shown above with its own service group:

```bash
for identity in edge telegram chatgpt claude x instagram threads extractor \
                extractor-browser-worker knowledge github vault channel-digests; do
  nk -gen user -pubout > "$identity.pair"   # first line SU... (seed), second line U... (public)
  sed -n 1p "$identity.pair" > "$identity.nkey"
  printf '%s  %s\n' "$identity" "$(sed -n 2p "$identity.pair")"
done
```

The AI archive services configure their `bus.nkey_seed_path` to `/etc/ratatoskr/chatgpt.nkey` and
`/etc/ratatoskr/claude.nkey` respectively. Telegram's seed is installed as:

```bash
sudo install -m 0640 -o root -g ratatoskr-telegram-dispatcher \
  /path/to/generated-telegram-seed /etc/ratatoskr/telegram.nkey
```

For tests and CI there is a tool that does this to a throwaway copy. `render-nats-config` reads
`ratatoskr.conf`, replaces every placeholder with a freshly generated user key, and writes the
result and one `<identity>.nkey` seed per identity (mode 0644, because the consumers are
throwaway containers) into a directory:

```bash
cargo run --locked -p platform-nats-profile --bin render-nats-config -- \
  deploy/nats/ratatoskr.conf "$RUNNER_TEMP/nats"
```

It is dev tooling; nothing in production depends on it, and the unrendered file is what is
deployed (`nats-server -t` refuses it with "Not a valid public nkey for a user" until the
placeholders are replaced).

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

The durable consumers on both streams are the table under "Fixed durables and the completion
bucket" above.

## Provisioning and inspection for Telegram

Provision in this order so the least-privilege Telegram process never needs topology authority:

1. Generate `telegram.nkey`, install its seed at `/etc/ratatoskr/telegram.nkey`, replace the
   Telegram public-key placeholder in `ratatoskr.conf`, and validate the candidate configuration.
2. Reload NATS so the new public identity is accepted.
3. Restart Edge. It creates or verifies `ratatoskr_events`, `ratatoskr_telegram_notifications` and
   every other fixed durable; a mismatch makes Edge refuse startup without modifying the
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
