---
status: accepted
---

# Store Append-Only Thread History with Transactional Projections

PostgreSQL will persist Canonical Thread History as an ordered, append-only stream of full-fidelity `RolloutItem` JSONB documents. Query-oriented relational projections for thread metadata, turns, items, search, and pagination will be updated in the same transaction as each append. This preserves replay compatibility and accommodates future protocol variants while retaining efficient queries, at the cost of storing some data twice and maintaining projection logic.
