# Promote Linux quality signals before merge

Now that Sustainable fork CI is stable, the fork will deepen the Merge gate inside the Linux
support boundary instead of restoring infrastructure parity. Conservatively detected Rust-impacting
pull requests will require independent Bazel and Cargo validation: native x64 test and lint graphs,
release-only compilation, an x64 musl Release portability check, a bounded PostgreSQL 18 contract
shard sharing the x64 nextest archive, and Change-triggered validation for the Linux V8 matrix.

## Consequences

- A test retry leaves the gate red; inherited flaky ignores must be certified or fixed, and a
  Quarantined check expires within seven days.
- The full Merge gate targets 120 minutes at the 95th percentile, reviewed after 20 eligible runs.
- Stability certification requires one complete retry-free successful Merge gate run plus one
  successful run of every retained Extended validation suite for the same rollout/workflow version.
- Promoted checks are not repeated after merge, normal ruleset bypass is removed, and post-merge
  automation retains only Extended validation that has not been promoted.
- Docker remote-executor, macOS, Windows, and new coverage or sanitizer tooling remain deferred
  until separately certified.
