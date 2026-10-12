## ADDED Requirements

### Requirement: Each deletion producer has its own acknowledgement durable

Edge SHALL create the durables `ratatoskr_chatgpt_deletion_acks`, `ratatoskr_claude_deletion_acks`, `ratatoskr_x_deletion_acks`, `ratatoskr_instagram_deletion_acks` and `ratatoskr_threads_deletion_acks` on `ratatoskr_events`, each with filter `evt.knowledge.deletion.acknowledged.v1`, explicit acknowledgements, deliver all, a 30 second ack wait and unlimited redelivery. The deployed ACL SHALL let KNOWLEDGE publish that subject and SHALL let each of CHATGPT, CLAUDE, X, INSTAGRAM and THREADS inspect, pull from and acknowledge its own durable and no other.

#### Scenario: a fresh broker gets the five durables

- **WHEN** Edge provisions the topology on a broker holding only the two streams
- **THEN** the five durables exist with exactly the stated settings

#### Scenario: a producer reads its own durable and no sibling's

- **WHEN** the CHATGPT identity describes, fetches from and acknowledges `ratatoskr_chatgpt_deletion_acks` and then describes `ratatoskr_claude_deletion_acks` on a broker running the deployed file
- **THEN** the first three succeed and the last request is not answered

#### Scenario: only Knowledge can publish the acknowledgement

- **WHEN** any identity other than KNOWLEDGE publishes `evt.knowledge.deletion.acknowledged.v1` on a broker running the deployed file
- **THEN** the publish is not acknowledged
