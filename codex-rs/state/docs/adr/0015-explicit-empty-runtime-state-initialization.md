---
status: accepted
---

# Initialize Empty PostgreSQL Runtime State Explicitly

New deployments may provision a PostgreSQL Runtime State Namespace without first creating or
migrating SQLite state by running `codex state initialize`. The command applies the current schema
migrations, accepts only an empty namespace, publishes the baseline empty Memory Generation,
validates every runtime readiness invariant, and records an origin-specific readiness fence. It
does not read SQLite or edit `config.toml`. Runtime startup remains read-only and never initializes
an absent or unready namespace automatically. Existing deployments that must preserve SQLite state
continue to use the explicit offline Runtime State Migration. This makes clean PostgreSQL-first
deployments possible while keeping authority selection deliberate, at the cost of maintaining two
explicit readiness workflows.
