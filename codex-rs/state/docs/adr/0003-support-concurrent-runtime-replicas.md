---
status: accepted
---

# Support Concurrent Runtime Replicas

The PostgreSQL Runtime State Backend will support multiple Runtime Replicas using the same namespace concurrently. Mutations and job claims must remain atomic across processes, and schema migrations must be serialized. This enables PostgreSQL to serve a replicated deployment rather than merely acting as a remote single-process database, at the cost of replacing process-local and SQLite-specific coordination assumptions with database-enforced concurrency.
