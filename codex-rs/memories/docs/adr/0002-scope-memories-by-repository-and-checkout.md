# Scope memories by repository and checkout

At the Memories boundary, memory extraction and consolidation will derive an optional,
credential-free Repository identity from persisted `git_info.repository_url` and propagate it as
`repository`; matching identities permit, but do not require, task-affine memories to cross
Checkout scopes, while `cwd` remains a qualifier and fallback. Different origins remain separate,
and evidence may cross Repository scopes only when it is explicitly repository-agnostic. This
preserves the Runtime State and API value while avoiding both worktree fragmentation and raw remote
credentials in memory artifacts; the latest persisted metadata is authoritative during
consolidation, and existing selected memories migrate incrementally without re-extraction.
