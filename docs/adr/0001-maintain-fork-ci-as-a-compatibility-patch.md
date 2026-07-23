# Maintain fork CI as a compatibility patch

The fork will adapt upstream validation workflows in place while preserving their jobs, steps, and
check names, limiting divergence to runner selection, gates, and repository guards. Duplicating the
inherited workflows would create a second validation implementation that drifts, while private
runner requirements would move correctness beyond Baseline infrastructure.

Validation correctness must depend only on standard GitHub-hosted runners. BuildBuddy may accelerate
inherited paths when a successful local fallback exists, but it is not part of Baseline
infrastructure. Pull requests receive native Essential validation on Linux x64, macOS ARM64, and
Windows x64; the full cross-platform Rust and V8 matrices remain Extended validation after merge.
