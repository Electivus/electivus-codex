---
status: accepted
---

# Make Project Session Scope a First-Class Thread Listing Filter

Thread discovery will define the current project as threads whose working directory exactly matches
the current working directory or whose credential-free canonical Git origin has the same Repository
Identity. `thread/list` will expose this Project Session Scope as a first-class filter implemented by
every Runtime State Backend, and the resume picker will default to `Project` while retaining `Cwd`
and `All`. Clients will identify the current project by its native working directory, allowing the
app-server to resolve the Git origin in the environment that owns that filesystem before passing a
canonical Repository Identity to the store. The Runtime State Store will preserve the observed
origin URL while maintaining an indexable derived Repository Identity; migrations will populate
that projection for historical threads so they participate without being reopened. This preserves
ordered cursor pagination, supports discovery across worktrees, remote workspaces, and operating
systems, and keeps sessions without Git metadata reachable, at the cost of expanding the app-server
API, its backend query contract, and both backend schemas; selecting a thread will continue to use
the existing `tui.resume_cwd` policy. The scope will be the default for every contextual lookup,
including resume and fork pickers, `--last`, and exec/name selection; explicit thread IDs remain
global, and `--all` continues to bypass contextual filtering. Repository matching is independent of
branch, so `Project` includes every branch and worktree while `Cwd` remains the narrower checkout
view. Picker rows show their saved working directory in `Project` and `All`, but omit that redundant
metadata in `Cwd`. If the app-server cannot resolve a valid Repository Identity for the requested
project working directory, the lookup degrades to `Cwd` without failing or widening to `All`. The
new `thread/list.projectCwd` request field will be experimental initially; the TUI already opts into
the experimental API capability, while external clients must opt in until the contract matures.
`--last` selects the most recently updated thread across the complete Project Session Scope without
preferring an older exact-cwd match.

Cross-platform discovery must preserve the Recorded Working Directory instead of normalizing it as
a host-local path. The app-server keeps the existing string wire representation through
`LegacyAppPathString`, converts it to `PathUri` for cross-platform manipulation and display, and
does not persist URIs. A foreign Recorded Working Directory is never reinterpreted for execution:
without a configured resume policy, Codex resumes in the current working directory and warns; an
explicit `tui.resume_cwd = "session"` fails with guidance to use `--cd` or select `current`.
Resuming from a checkout with a different origin does not reclassify the thread automatically; its
Repository Identity changes only with an explicit `gitInfo.originUrl` metadata update, which updates
the derived projection atomically.

## Delivery sequence

1. Preserve foreign Recorded Working Directories across the app-server boundary and enforce safe
   cross-platform resume behavior.
2. Add the derived Repository Identity projection, historical migration, backend indexes, Runtime
   State Contract filter, and experimental `thread/list.projectCwd` API.
3. Adopt Project Session Scope in TUI and exec contextual lookups, add `Project Cwd All`, and cover
   the user-visible behavior with snapshots.
