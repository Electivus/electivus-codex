# Promote Windows x64 and ARM64 to Essential platforms

The fork promotes Windows x64 and Windows ARM64 into the Fork validation boundary because it ships
both architectures and a Windows-specific regression must block merging. The mandatory path adapts
the current upstream Cargo, nextest, Bazel x64, argument-comment-lint, and V8 source-build topology
to standard public `windows-2025` and `windows-11-arm` runners while retaining Electivus planning,
inventory, retry-free evidence, optional-accelerator fallback, and fail-closed aggregation.

This decision supersedes only the Windows-deferral statements in ADR-0001 and ADR-0003. Their
compatibility-patch model, Linux promotion, baseline-infrastructure constraint, and macOS deferral
remain in force.

## Consequences

- Windows x64 and ARM64 validation runs for every pull request without private runner groups,
  larger runners, paid capacity, or correctness dependencies on infrastructure secrets.
- Native ARM64 nextest runs on `windows-11-arm`; its archive and both V8 targets are cross-built on
  `windows-2025`. Bazel remains x64-only because upstream has no Windows ARM64 Bazel lane.
- The inherited Windows skip baseline is fixed to upstream commit
  `6c108912eeacabfc82723bf44f8a23f6e2f86585`; new or modified Windows-effective skips fail policy.
  All unconditional ignores are also locked to the inspected Electivus baseline so a newly classified
  platform-neutral ignore cannot create a Windows blind spot.
- Promoted Windows validation is not repeated after merge, and release publishing, signing,
  installers, macOS promotion, and post-merge Windows native-main remain outside this decision.
- `CI required` remains the single repository-owned required context. Initial certification needs
  two complete retry-free Fresh merge gate runs on one unchanged candidate, including one cold or
  isolated cache path; the first 20 eligible post-promotion runs determine the 120-minute p95
  feedback result.
