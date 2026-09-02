---
status: accepted
---

# Prefer a Direct PostgreSQL mTLS URL

PostgreSQL Runtime State configuration accepts exactly one connection source: a direct `url`, or
the compatible `url_env` environment-variable reference. The direct source is preferred because an
auxiliary app-server can initialize from a shared Codex home without inheriting a launcher-specific
process environment. `url_env` remains fully supported without deprecation or startup warnings.

Both sources compile to the same PostgreSQL mTLS Connection Descriptor before pool creation. The
descriptor therefore enforces the same passwordless identity, canonical TLS parameters, absolute
file policy, physical-connection session evidence, and fail-closed Runtime State Backend boundary;
neither source enables password authentication or SQLite fallback.

A direct URL is stable configuration and may be returned by configuration-read APIs. Runtime
errors and debug representations redact it, and diagnostics inspect TLS file metadata without
reading private-key contents. Operators must therefore protect the shared Codex home and the
referenced certificate files with appropriate filesystem access controls. This trades the
environment source's process-local indirection for deterministic auxiliary-process startup without
introducing a keyring, wrapper, launcher shim, or plugin-specific environment path.

This decision supersedes only ADR 0001's requirement that the PostgreSQL connection URL come from a
named environment variable. ADR 0001's backend selection, feature gate, migration readiness,
PostgreSQL exclusivity, and no-fallback behavior remain unchanged.
