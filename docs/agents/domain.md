# Domain docs

This repository uses a multi-context domain documentation layout.

## Before exploring, read these

1. Read the root `CONTEXT-MAP.md` when it exists.
2. Follow it to every `CONTEXT.md` relevant to the area being changed.
3. Read relevant system-wide ADRs under `docs/adr/`.
4. Read relevant context-specific ADRs under the nearest `docs/adr/`.
5. Read the nearest existing `README.md` for crate- or package-specific
   implementation guidance.

If these files do not exist, proceed silently. Do not suggest creating them
upfront. `$mattpocock-skills:domain-modeling` creates them lazily when domain
terms or architectural decisions are resolved.

## File structure

The root map indexes contexts across the monorepo:

```text
/
├── CONTEXT-MAP.md
├── docs/adr/                         # System-wide decisions
├── codex-rs/
│   ├── CONTEXT.md                    # Shared Rust domain
│   ├── docs/adr/                     # Rust-wide decisions
│   └── <crate>/
│       ├── README.md
│       ├── CONTEXT.md                # Crate context, when needed
│       └── docs/adr/                 # Crate-specific decisions
├── codex-cli/
│   ├── CONTEXT.md
│   └── docs/adr/
└── sdk/
    └── <sdk>/
        ├── README.md
        ├── CONTEXT.md
        └── docs/adr/
```

A context should live at the nearest stable project or crate root. Do not
create one `CONTEXT.md` per crate mechanically: add a crate-level context only
when that crate owns distinct terminology, behavior, or architectural
decisions. Otherwise, use the enclosing `codex-rs/CONTEXT.md`.

Existing README files remain the source for package usage and implementation
details. `CONTEXT.md` records domain vocabulary and boundaries; it does not
duplicate the README.

## Keep the context map current

When creating a context, add it to `CONTEXT-MAP.md` with:

- its path;
- the domain or subsystem it owns;
- when an agent should read it;
- links to relevant neighboring contexts.

## Use the glossary's vocabulary

When output names a domain concept—in an issue title, refactor proposal,
hypothesis, or test name—use the term defined by the relevant `CONTEXT.md`.
Do not drift to synonyms that the glossary explicitly avoids.

If a needed concept is absent, reconsider whether the term belongs to the
project. If it represents a real gap, note it for
`$mattpocock-skills:domain-modeling`.

## Flag ADR conflicts

If proposed work contradicts an existing ADR, identify the conflict explicitly
instead of silently overriding it:

> Contradicts ADR-0007 — worth reopening because…
