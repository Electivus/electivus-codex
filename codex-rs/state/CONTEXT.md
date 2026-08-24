# Runtime State

This context defines the durable state owned and maintained by the Codex runtime.

## Language

**Runtime State Store**:
The persistence boundary for runtime-owned thread metadata and history, logs, goals, memories, and remote-control enrollment data.
_Avoid_: State DB, SQLite state

**Runtime State Backend**:
The single selected persistence implementation that serves every part of the Runtime State Store for a process. Backends are not combined or used as fallback within the same runtime.
_Avoid_: Database mode, hybrid backend

**Runtime State Contract**:
The backend-independent observable behavior of runtime-state operations, including ordering, pagination, search, retention, claims, lifecycle changes, and public errors.
_Avoid_: SQLite semantics, PostgreSQL semantics

**Runtime State Namespace**:
The complete, isolated runtime state belonging to one logical Codex deployment. A namespace may be shared by replicas of that deployment but not by independent users or deployments.
_Avoid_: User state, account state, tenant

**PostgreSQL mTLS Connection Descriptor**:
A passwordless PostgreSQL endpoint, user, and database together with `verify-full` and explicit absolute CA, client-certificate, and client-key file paths. Every connection source must compile to this descriptor before a Runtime State Backend pool is created.
_Avoid_: PostgreSQL credential, permissive connection URL, TLS options

**Client-observable mTLS Session Evidence**:
The current backend's `pg_stat_ssl` result showing TLS active and a non-empty client-certificate distinguished name. This proves that the session uses TLS and presented a client certificate, but not which PostgreSQL HBA rule authenticated it; operators separately enforce the server-side `cert` or `clientcert=verify-full` invariant.
_Avoid_: Authentication-policy proof, HBA evidence, certificate authorization

**Runtime Replica**:
A Codex process that concurrently reads and writes a shared Runtime State Namespace.
_Avoid_: Tenant, user

**Runtime State Migration**:
An explicit, one-time transfer of a complete Runtime State Namespace from a quiescent source to an empty destination before authority switches.
_Avoid_: Backfill, synchronization, replication

**Runtime State Initialization**:
Explicit provisioning of a new, empty Runtime State Namespace, including every baseline record and readiness invariant required before runtime use.
_Avoid_: Empty migration, automatic startup migration

**Canonical Thread History**:
The complete, ordered record used to resume a thread and reconstruct its model-visible context.
_Avoid_: Rollout file, history projection

**Thread Writer Lease**:
A time-bounded grant allowing one Runtime Replica to append to a thread, paired with a fencing token that invalidates superseded writers.
_Avoid_: Process lock, permanent ownership

**Append Batch**:
An ordered group of thread-history items submitted atomically under one idempotency key. Retrying the same batch cannot add the items more than once.
_Avoid_: Turn, transport attempt

**Thread History Projection**:
A query-oriented view derived from Canonical Thread History for metadata, turn, item, search, or pagination reads. It can be rebuilt and is never the source used to establish historical truth.
_Avoid_: Canonical history, rollout

**Repository Identity**:
A credential-free canonical identity derived from a thread's Git origin and shared by equivalent supported remote URL forms.
_Avoid_: Repository remote, origin URL, repository path

**Project Session Scope**:
The branch-independent thread-discovery boundary for the current project: the same Repository Identity or the exact working directory, preserving sessions when Git identity is unavailable.
_Avoid_: Cwd filter, repository filter

**Recorded Working Directory**:
The source-native working directory persisted for a thread. It remains valid discovery and display metadata across Runtime Replicas even when its path convention is not executable on the current host.
_Avoid_: Local cwd, normalized cwd, portable path

**Memory Artifact**:
A versioned piece of consolidated memory content, identified by its path within a Runtime State Namespace. A filesystem copy is a disposable materialization, not the authoritative artifact.
_Avoid_: Memory file, local memory

**Memory Generation**:
A complete, immutable snapshot of the Memory Artifacts published together by one successful consolidation.
_Avoid_: File batch, partial update
