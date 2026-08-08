# Merge upstream history during synchronization

Each Upstream synchronization will merge the selected Synchronization baseline into the Electivus
history instead of rebasing fork commits or squashing upstream changes. Explicit merge commits
preserve both lineages, keep later synchronizations incremental, and avoid repeatedly rewriting the
fork's independently developed history.
