# Context Map

## Contexts

- [Fork CI](./CONTEXT.md) — owns the vocabulary for validation and automation maintained by the
  Electivus fork
- [Runtime State](./codex-rs/state/CONTEXT.md) — owns the vocabulary for durable state maintained by the Codex runtime
- [Memories](./codex-rs/memories/CONTEXT.md) — owns the vocabulary for extracting, scoping, and consolidating reusable memory

## Relationships

- **Runtime State → Memories**: Runtime State supplies persisted thread metadata and memory inputs; Memories determines how those inputs are scoped and consolidated into reusable artifacts
