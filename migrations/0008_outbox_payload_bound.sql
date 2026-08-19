-- Milestone 9 follow-up: a command may not be larger than the bus will carry.
--
-- `operations.outbox.payload` has been unbounded since 0003, and the publisher serializes it
-- straight onto NATS. A server refuses a publish above `max_payload` — 1 MiB by default and the
-- value `deploy/nats/ratatoskr.conf` leaves at its default — and the refusal arrives as a message
-- that was not acknowledged. The outbox reads that as a transport failure and does exactly the
-- wrong thing with it: it backs the row off and retries, forever, until twelve attempts are spent.
--
-- The cost is not one row. `pump::run_once` claims a batch of 64 and publishes them in order, so an
-- oversized row consumes a claim slot every pass, and its retries are indistinguishable from a
-- broker outage in `last_error`. The whole queue behind it is delayed by a message that cannot
-- succeed on any attempt.
--
-- This is reachable today, and by an operator rather than a client: `operations.schedules.payload`
-- is arbitrary jsonb an operator writes, and it becomes the `payload` member of the command
-- envelope. The two client-facing producers both emit `{"url": ...}` bounded to 2048 characters.
--
-- Refusing the write is the right end to refuse at. The insert happens inside the transaction that
-- accepts the work, so a payload that could never be delivered fails where the caller can be told,
-- instead of being accepted durably and then discovering it is undeliverable in a background loop.

-- One correction that belongs to 0007 and cannot be written there. Its comment says nothing prunes
-- `operations.schedule_occurrences`; the retention sweep now does, on
-- `RATATOSKR__RETENTION__SCHEDULE_OCCURRENCE_DAYS`, default 90 days. 0007 has been applied, and
-- `sqlx` hashes a migration's FILE — comments included — so editing it to say so would make every
-- database that already ran it refuse to migrate. A superseded comment plus a note here is the
-- cheaper of the two wrongs.
--
-- The consequence worth knowing: that window is how far back an operator may move a schedule's
-- `next_due_at` before a rewind republishes instead of being suppressed.

alter table operations.outbox
    add constraint outbox_payload_fits_in_a_nats_message
    check (octet_length(payload::text) <= 786432);

comment on column operations.outbox.payload is
    'The serialized envelope. Bounded to 768 KiB, which is the NATS default max_payload of 1 MiB '
    'with room for the headers and for the difference between jsonb''s own text rendering and '
    'serde_json''s. Not a tuning knob: a larger message is a different design — a reference to a '
    'blob — rather than a larger limit. Raising max_payload on the server without raising this '
    'constraint is safe; the reverse is not.';
