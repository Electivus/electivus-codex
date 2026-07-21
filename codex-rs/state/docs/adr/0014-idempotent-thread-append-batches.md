---
status: accepted
---

# Make Thread Append Batches Idempotent

Each PostgreSQL thread append will carry an idempotency key and commit its ordered items atomically. Repeating the same key and content returns the previously committed result, while reusing a key with different content fails; the Thread Writer Lease fencing token and expected stream version are still validated. This prevents duplicate history after ambiguous commit responses, at the cost of retaining and validating append identities.
