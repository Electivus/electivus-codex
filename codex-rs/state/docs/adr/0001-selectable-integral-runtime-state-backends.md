---
status: accepted
---

# Keep Runtime State Backends Integral and Selectable

Codex will support SQLite and PostgreSQL 18 or later as selectable Runtime State Backends. SQLite remains the default for local and offline use; when PostgreSQL is selected, every Runtime State Store responsibility uses PostgreSQL exclusively, without SQLite writes or fallback reads. The PostgreSQL backend uses core database features and, as revised by [ADR 0017](0017-permit-declared-postgresql-capabilities.md), may also use extensions and optional languages; ADR 0017 supersedes only this ADR's original `requires no extensions` clause. If the selected PostgreSQL backend is unavailable, startup fails instead of degrading, resetting state, or creating local storage. The existing `sqlite_home` setting remains valid for SQLite and as an explicit migration source; PostgreSQL runtime startup never accesses it and warns when it is configured but inactive. PostgreSQL initially ships as an opt-in experimental feature behind `features.postgresql_state`, while its `[state]` configuration shape is intended to remain stable when the gate is removed. Implementation may land incrementally, but PostgreSQL cannot become selectable until every state responsibility, migration, and contract test is complete. This preserves the existing zero-service local experience while preventing split authority and synchronization ambiguity, at the cost of maintaining two complete backend implementations and migration paths.

## Stable configuration shape

The feature gate authorizes PostgreSQL selection but does not select it by itself. The connection
URL is read from the named environment variable and is never stored in the parsed configuration:

```toml
[features]
postgresql_state = true

[state]
backend = "postgresql"

[state.postgresql]
url_env = "CODEX_POSTGRES_URL"
schema = "codex"

[state.postgresql.pool]
max_connections = 10
acquire_timeout_ms = 10000
statement_timeout_ms = 30000
```

Omitting `[state]`, or selecting `backend = "sqlite"`, keeps the existing SQLite default. Selecting
PostgreSQL without the feature gate is a configuration error. `schema` and the pool table may be
omitted to use the values shown above.

PostgreSQL runtime startup is read-only with respect to schema management. It requires PostgreSQL
18 or later, the current migration layout, and the final ready fence written by the explicit
`codex state migrate` or `codex state initialize` workflow. An unavailable, incompatible, or
unready namespace fails startup with a redacted actionable error; it never triggers SQLite
recovery, fallback, initialization, or access to a configured `sqlite_home`.
