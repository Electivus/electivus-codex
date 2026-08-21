---
status: accepted
---

# Permit Declared PostgreSQL Capabilities

The PostgreSQL Runtime State Backend may use standard PostgreSQL features, extensions, and optional
languages when they are useful and justified. Each dependency must be declared and documented, and
it must be provisioned for the operation that needs it. Migration 0021 currently uses `plpgsql` to
create a temporary repository-identity migration function and drops that function before the
migration completes, so the migration operation requires `plpgsql` to be provisioned.

This decision supersedes only the `requires no extensions` clause in
[ADR 0001](0001-selectable-integral-runtime-state-backends.md). Every other decision in ADR 0001
remains in force.
