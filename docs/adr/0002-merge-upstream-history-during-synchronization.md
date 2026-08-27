# Merge upstream history during synchronization

Each Upstream synchronization will merge the selected Synchronization baseline into the Electivus
history instead of rebasing fork commits or squashing upstream changes. Explicit merge commits
preserve both lineages, keep later synchronizations incremental, and avoid repeatedly rewriting the
fork's independently developed history.

Issue #170 later hardened release selection. Automatic discovery selects only the greatest stable
Semantic Version, while manual dispatch may name one exact published stable or pre-release tag; the
tag's SemVer is authoritative. Closing a Synchronization PR without merge abandons that attempt
permanently.
