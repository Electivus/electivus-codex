---
status: accepted
---

# Keep the Runtime State Contract Backend-Independent

SQLite and PostgreSQL will implement the same Runtime State Contract for ordering, pagination and cursors, search, retention, claims, thread lifecycle, resume, and public errors. Backend differences remain limited to configuration and operations, and the same contract suite will run against both implementations. PostgreSQL coverage uses a real PostgreSQL 18 service in a required Linux CI job rather than SQL mocks, while non-PostgreSQL builds and tests remain multiplatform. This prevents backend branches in core, app-server, and TUI, at the cost of postponing PostgreSQL-specific behavior that would alter observable semantics and maintaining dedicated integration infrastructure.
