---
status: accepted
---

# Use one policy for model-controlled tool execution timing

Model-controlled execution surfaces use one effective timing policy so short deadlines and
observation windows do not prematurely terminate or background long-running work and consume extra
model inferences through retries and polling. The policy covers hard execution timeouts where a
surface supports them and yield windows across the execution lifecycle; it does not govern Codex's
internal subprocesses or impose an absolute lifetime on persistent unified-exec processes or
code-mode cells.

The public configuration is:

```toml
[tool_execution.timeout]
min_ms = 10000
default_ms = 600000
max_ms = 3600000

[tool_execution.yield]
min_ms = 10000
default_ms = 30000
max_ms = 300000
```

Each field may be overridden independently at the root or in a named profile. Missing fields
inherit the built-in values before Codex validates that every value is positive, technically
representable, and ordered as `min_ms <= default_ms <= max_ms`. Invalid effective configuration
fails to load. Codex imposes no additional product ceiling above an administrator-configured
maximum.

Explicit timing arguments outside the effective range are clamped to the nearest bound, and the
tool result tells the model about the adjustment. Affected tool contracts advertise the resolved
default and range, including numeric schema constraints where their format supports them.
Configuration reloads apply to future calls, including later observations of existing sessions,
but do not change calls already waiting.

The timeout range applies to model-controlled shell and shell-tool calls that have hard-deadline
semantics. It does not turn unified-exec sessions or code-mode cells into lifetime-bounded
resources. The yield range applies to non-interactive unified-exec launches, empty `write_stdin`
polls, and code-mode `exec` and `wait` observations. Commands and cells still return before the
yield deadline when they finish or explicitly yield.

Interactive operations remain responsive exceptions: initial unified-exec launches with
`tty = true` and non-empty `write_stdin` calls retain their 250-30000 ms yield range. This
distinguishes input-response interaction from background observation without introducing
per-surface configurable policies.

`background_terminal_max_timeout` remains as a deprecated alias for
`tool_execution.yield.max_ms` only when the new key is absent; setting both is an error.
Legacy alias values below the new 30-second yield default are raised to that default so existing
configuration keeps loading with an ordered range.
`code_mode_buffered_exec` remains accepted as a deprecated no-op because the global 30-second
yield default supersedes its only behavior.

Enforcement belongs on the Codex host before dispatch so local and remote executors observe the
same policy. Resolved values are included in config locks. Remote-executor-aware integration tests
must cover the model-visible execution paths in addition to configuration validation and tool
contract tests.

## Considered options

Per-surface policies and global policies with per-surface overrides were rejected because their
precedence and divergent behavior would make the timing contract harder for users and models to
predict. Rejecting out-of-range tool calls was rejected because correction necessarily consumes
another inference; clamping advances the work while still disclosing the policy adjustment.
