# Maintain fork CI as a compatibility patch

The fork will adapt upstream validation workflows in place while preserving their jobs, steps, and
check names where they remain inside the Linux support boundary. Divergence is limited to runner
selection, active matrices, gates, and repository guards. Duplicating the inherited validation
workflows would create a second implementation that drifts, while requiring resources beyond
Baseline infrastructure would make the fork dependent on infrastructure it does not control.
macOS and Windows validation can return by restoring inherited jobs and widening matrices after
standard-runner certification.
