---
status: accepted
---

# Apply PostgreSQL Schema Migrations Explicitly

PostgreSQL schema migrations will run through an explicit operator command under a namespace-scoped advisory lock, not during Runtime Replica startup. The command creates the configured schema when absent and applies its migrations; startup only verifies that the schema is within the application's supported compatibility range. Migrations follow expand/contract across releases so the new schema remains compatible with the immediately previous Codex version during a rolling deploy. This permits runtime credentials without DDL privileges and makes rollout timing deliberate, at the cost of an additional deployment step and slower removal of obsolete schema.
