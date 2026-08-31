---
name: code-review-change-size
description: Review change cohesion and identify real staging boundaries without numeric diff-size limits.
---

Do not impose a numeric changed-line limit or treat total diff size as a finding
by itself. Review the actual diff, dependencies, affected call sites, risk, and
whether the behavior can be understood and validated as one coherent unit.

Recommend dependent stages only when they form independently useful and
verifiable boundaries. Do not request an artificial split solely to reduce a
line count. If a cohesive change is large, focus the review on its highest-risk
interfaces and invariants instead of reporting size as a blocker.
