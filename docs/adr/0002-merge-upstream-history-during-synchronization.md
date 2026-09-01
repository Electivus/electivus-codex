# Merge upstream history during synchronization

Each Upstream synchronization will merge the selected Synchronization baseline into the Electivus
history instead of rebasing fork commits or squashing upstream changes. Explicit merge commits
preserve both lineages, keep later synchronizations incremental, and avoid repeatedly rewriting the
fork's independently developed history.

Issue #170 later hardened release selection. Automatic discovery selects only the greatest stable
Semantic Version, while manual dispatch may name one exact published stable or pre-release tag; the
tag's SemVer is authoritative. Closing a Synchronization PR without merge abandons that attempt
permanently.

Preparation freezes both the selected Release baseline and the current Fork baseline in a canonical
Synchronization manifest. A clean Baseline reconciliation is an exact fork-first merge with those
two commits as parents. If that merge conflicts, preparation records the complete conflict set and
starts a draft, release-first branch without prematurely importing Fork ancestry; Semantic
reconciliation remains explicit review work.

The manifest, not mutable pull-request text, is authoritative for new attempts. A retry validates
the owned branch, manifest chain, normalization, topology, and frozen conflict evidence, then reuses
the existing head without another push or moving either baseline. Legacy open attempts are frozen
only when their title, branch, and body identify one release unambiguously.

Published Codex Release commits are snapshots and do not necessarily descend from the preceding
tagged Release commit. The manifest predecessor therefore forms a logical immutable chain. Git
progression accepts direct Release-commit ancestry or, for two single-parent snapshot commits, an
upstream source parent that strictly descends from the predecessor's source parent. Reversed,
unrelated, or ambiguous snapshot lineage fails closed in both preparation and topology validation.
