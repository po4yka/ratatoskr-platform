## 1. Invariant 7 and the command grant (R5-12 rule 5)

- [x] 1.1 Extend `deployment_profile.rs`, `support/mod.rs` and `deployed_nats_config.rs` (CHATGPT and CLAUDE hold exactly the command allow in addition to their previous list, no stanza but EDGE holds a `$JS.API.STREAM.` publish allow beyond the browser worker's read of its own bucket, the matrix proves the command is published by the two importers only and the stream-management subjects are refused) and run them RED
- [x] 1.2 Edit `deploy/nats/ratatoskr.conf` and `deploy/nats/README.md`, and run 1.1 GREEN. This comes before the handler so the handler's real-broker test can publish its command through the deployed ACL

## 2. The handler (R5-12 rule 4)

- [x] 2.1 Add `crates/eventing/tests/stream_erasure.rs` (own message erased and neighbour kept, foreign producer refused, foreign owner refused, tombstone and non-ai_archive refused, missing sequence, replay absorbed, envelope mismatch Termed, one audit row with the counts and no content, outcome counters; three scenario tests, each on its own throwaway broker running the rendered deployed ACL, with the real `platform_stream_erasure` consumer), and a stub handler that acknowledges everything, and run it RED on the assertions
- [x] 2.2 Implement `platform_eventing::stream_erasure`, register the metric, add the audit table, spawn `platform_stream_erasure` in Edge, and run 2.1 GREEN
