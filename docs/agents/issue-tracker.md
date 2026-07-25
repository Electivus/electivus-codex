# Issue tracker: GitHub

Issues and PRDs for this repo live in GitHub Issues for
`Electivus/electivus-codex`. Use the `gh` CLI for all operations.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`.
  Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering
  comments with `jq` and also fetching labels.
- **List issues**:
  `gh issue list --state open --json number,title,body,labels,comments`
  with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply or remove labels**:
  `gh issue edit <number> --add-label "..."` or
  `gh issue edit <number> --remove-label "..."`
- **Close an issue**: `gh issue close <number> --comment "..."`

Run commands from this clone so `gh` resolves the `origin` repository. When
repository inference is ambiguous, pass `--repo Electivus/electivus-codex`.

## Pull requests as a triage surface

**PRs as a request surface: no.**

When set to `yes`, PRs run through the same labels and states as issues, using
the `gh pr` equivalents:

- **Read a PR**: `gh pr view <number> --comments` and
  `gh pr diff <number>`.
- **List external PRs for triage**:
  `gh pr list --state open --json number,title,body,labels,author,authorAssociation,comments`,
  retaining only `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, or `NONE`.
- **Comment, label, or close**: use `gh pr comment`, `gh pr edit`, or
  `gh pr close`.

GitHub shares one number space across issues and PRs. Resolve an ambiguous
`#42` with `gh pr view 42`, then fall back to `gh issue view 42`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue in `Electivus/electivus-codex`.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.

## Wayfinding operations

Used by `$mattpocock-skills:wayfinder`. The map is a single issue with child
issues as tickets.

- **Map**: an issue labelled `wayfinder:map`, holding Notes,
  Decisions-so-far, and Fog.
- **Child ticket**: an issue linked to the map as a GitHub sub-issue.
  If sub-issues are unavailable, add the child to a task list in the map and
  place `Part of #<map>` at the top of the child body. Use a
  `wayfinder:<type>` label: `research`, `prototype`, `grilling`, or `task`.
- **Blocking**: use GitHub's native issue dependencies. If unavailable, add
  `Blocked by: #<n>, #<n>` at the top of the child body.
- **Frontier query**: inspect the map's open children, discard tickets with
  open blockers or an assignee, and take the first remaining ticket in map
  order.
- **Claim**: `gh issue edit <number> --add-assignee @me`.
- **Resolve**: comment with the answer, close the child, and add a context
  pointer to the map's Decisions-so-far section.
