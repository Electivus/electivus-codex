---
status: accepted
---

# Publish Memory Artifacts as Atomic Generations

Each successful memory consolidation will publish an immutable Memory Generation containing the complete artifact set in one PostgreSQL transaction. Runtime Replicas observe either the previous generation or the new complete generation, never a partial mix; a worker may edit a temporary local workspace before committing its snapshot. This preserves cross-file consistency under concurrent reads, at the cost of snapshot storage and generation lifecycle management.
