# Merge upstream history during synchronization

Each Upstream synchronization will merge the selected Synchronization baseline into the Electivus
history instead of rebasing fork commits or squashing upstream changes. Explicit merge commits
preserve both lineages, keep later synchronizations incremental, and avoid repeatedly rewriting the
fork's independently developed history.

Issue #170 later hardened this decision. Automatic discovery selects only the greatest stable
Semantic Version, while manual dispatch may name one exact published stable or pre-release tag; the
tag's SemVer is authoritative. Each attempt freezes both lineages in a canonical, predecessor-linked
Synchronization manifest. Clean preparation creates the fork-first Baseline reconciliation;
conflicting preparation stays release-first and draft until explicit Semantic reconciliation.
Closing without merge abandons the attempt permanently.
