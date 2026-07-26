---
status: accepted
---

# Migrate Runtime State Explicitly Before Cutover

Switching an existing Codex installation from SQLite to PostgreSQL will require an explicit offline Runtime State Migration that copies the complete namespace from a quiescent SQLite source into an empty PostgreSQL destination and preserves the source. Startup will never migrate automatically or silently start with empty PostgreSQL state. A new deployment without SQLite state may instead use the separate explicit Runtime State Initialization workflow described in ADR 0015. The migration tool prepares, copies, validates, and reports readiness but never edits configuration; the operator performs cutover separately. The first delivery is forward-only: once PostgreSQL accepts new writes, the preserved SQLite source is stale and cannot be used for lossless rollback. This makes the authority change deliberate and observable, avoiding races between Runtime Replicas, at the cost of an operator-visible downtime window and a post-cutover point of no return.
