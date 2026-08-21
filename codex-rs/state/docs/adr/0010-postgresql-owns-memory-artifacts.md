---
status: accepted
---

# Make PostgreSQL Authoritative for Memory Artifacts

When PostgreSQL is selected, the Runtime State Namespace will include versioned Memory Artifacts such as `MEMORY.md`, `memory_summary.md`, skills, rollout summaries, and imported resources, in addition to memory jobs and intermediate outputs. Runtime Replicas may materialize disposable local workspaces for existing filesystem-oriented tools, but local files are not authoritative. This gives every replica a consistent memory view without shared storage, at the cost of database-backed artifact versioning and materialization.
