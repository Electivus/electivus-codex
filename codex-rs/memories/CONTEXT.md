# Memories

This context defines the language used to extract and consolidate reusable knowledge from Codex
threads.

## Language

**Repository identity**:
A credential-free canonical identifier for a Git repository that treats equivalent remote URL
forms as the same repository.
_Avoid_: Repository URL, origin URL

**Repository scope**:
Memory applicability shared by threads with the same Repository identity, even when they use
different worktrees or working directories. It permits, but does not require, consolidating
task-affine memories across Checkout scopes.
_Avoid_: CWD family, project path

**Checkout scope**:
Memory applicability specific to a thread's working directory. It qualifies repository-scoped
memory and is the fallback when no repository is known.
_Avoid_: Repository scope, workspace
