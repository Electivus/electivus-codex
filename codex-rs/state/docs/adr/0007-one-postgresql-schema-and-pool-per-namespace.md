---
status: accepted
---

# Use One PostgreSQL Schema and Pool per Namespace

A PostgreSQL Runtime State Namespace will use one schema, one connection pool, and one migration history rather than reproducing the five separate SQLite database files. The schema name is configurable and defaults to `codex`; state responsibilities remain separated by tables and Rust modules. PostgreSQL concurrency removes the file-lock motivation for separate databases, while a unified pool permits transactions spanning Canonical Thread History and its projections, at the cost of sharing one pool budget across all state workloads.
