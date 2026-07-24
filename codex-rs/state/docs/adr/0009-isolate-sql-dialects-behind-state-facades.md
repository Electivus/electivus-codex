---
status: accepted
---

# Isolate SQL Dialects Behind Responsibility-Specific Facades

Runtime-state responsibilities such as threads, history, logs, goals, memories, and remote control will expose storage-neutral facades backed by private SQLite and PostgreSQL modules. Domain models and pure transformations remain shared, but SQL and database row decoding stay backend-specific; the implementation will not use a monolithic backend trait, `sqlx::AnyPool`, or pervasive `Database` generics. This contains dialect differences and keeps modules reviewable, at the cost of some deliberate query duplication.
