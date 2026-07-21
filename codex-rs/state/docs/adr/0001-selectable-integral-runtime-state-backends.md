---
status: accepted
---

# Keep Runtime State Backends Integral and Selectable

Codex will support SQLite and PostgreSQL 18 or later as selectable Runtime State Backends. SQLite remains the default for local and offline use; when PostgreSQL is selected, every Runtime State Store responsibility uses PostgreSQL exclusively, without SQLite writes or fallback reads. The PostgreSQL backend uses core database features and requires no extensions. If the selected PostgreSQL backend is unavailable, startup fails instead of degrading, resetting state, or creating local storage. The existing `sqlite_home` setting remains valid for SQLite and as an explicit migration source; PostgreSQL runtime startup never accesses it and warns when it is configured but inactive. PostgreSQL initially ships as an opt-in experimental feature behind `features.postgresql_state`, while its `[state]` configuration shape is intended to remain stable when the gate is removed. Implementation may land incrementally, but PostgreSQL cannot become selectable until every state responsibility, migration, and contract test is complete. This preserves the existing zero-service local experience while preventing split authority and synchronization ambiguity, at the cost of maintaining two complete backend implementations and migration paths.
