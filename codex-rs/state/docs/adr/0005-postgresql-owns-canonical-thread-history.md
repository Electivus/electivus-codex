---
status: accepted
---

# Make PostgreSQL Authoritative for Complete Thread History

When PostgreSQL is selected, it will own the Canonical Thread History as well as thread metadata; Runtime Replicas will not depend on local rollout JSONL files for durability or resume. JSONL may remain an export or diagnostic representation but not a second authoritative store. This makes threads portable across replicas and preserves a single authority, at the cost of implementing full-fidelity history persistence and migration in PostgreSQL.
