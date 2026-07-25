---
status: accepted
---

# Use One Codex Namespace per PostgreSQL Database or Schema

Each PostgreSQL database or schema will hold exactly one Runtime State Namespace. Replicas of the same logical Codex deployment may share it, while independent users and deployments must use separate databases or schemas. This keeps isolation at the PostgreSQL boundary and avoids adding tenant columns to every table and key, at the cost of requiring operators to provision separate namespaces when they need isolation.
