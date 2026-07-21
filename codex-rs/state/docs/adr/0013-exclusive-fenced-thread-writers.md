---
status: accepted
---

# Enforce Exclusive Fenced Thread Writers

Each PostgreSQL-backed thread will have at most one active writer, represented by a renewable Thread Writer Lease and monotonically advancing fencing token. Other Runtime Replicas may read the thread but cannot append until the writer closes or its lease expires; every append verifies the token so a paused or partitioned former writer cannot resume after ownership changes. This preserves linear thread history across replicas, at the cost of lease renewal and takeover handling.
