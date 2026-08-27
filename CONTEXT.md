# Fork CI

This context defines the language used for validation and automation owned by the Electivus fork
while it continues to track the upstream Codex repository.

## Language

**Sustainable fork CI**:
A validation model that preserves relevant coverage and conceptual alignment with upstream while
remaining independently operable within the fork's current support boundary.
_Avoid_: Infrastructure parity, mirrored CI

**Merge gate**:
The minimum automated validation contract that a pull request must satisfy, in addition to human
review, before entering the default branch.
_Avoid_: Required job, branch check, full CI

**Stable workflow**:
An automatic workflow that succeeds when applicable or explicitly skips a capability outside the
fork's support boundary, while continuing to expose real failures.
_Avoid_: Always-green workflow, tolerated failure, permanently red workflow

**Essential platform**:
A supported operating-system and architecture combination whose validation belongs to the merge
gate because a platform-specific regression must prevent merging.
_Avoid_: Primary runner, required architecture

**Extended platform**:
A supported operating-system, architecture, or build variant validated after merge because its
signal matters but does not justify adding latency to every pull request.
_Avoid_: Optional platform, unsupported platform

**Linux support boundary**:
The current fork-validation scope: native Linux x64 in the merge gate, plus a Release portability
check and Change-triggered validation for relevant V8 changes. Linux ARM64 tests, remaining GNU/musl
build variants, and other specialized Linux paths stay in extended validation, while macOS and
Windows remain product platforms but are deferred from fork validation until sustainable runners
are certified.
_Avoid_: Codex platform support, permanent platform removal

**Validation workflow**:
An automation path that produces evidence about a proposed or integrated code change without
publishing releases or changing external product state.
_Avoid_: Release workflow, deployment workflow

**Dormant workflow**:
An upstream automation path retained for synchronization but intentionally not operated by the
fork until its ownership and prerequisites are defined.
_Avoid_: Broken workflow, ignored failure, supported workflow

**Compatibility patch**:
The minimal maintained difference that adapts upstream validation automation to the fork without
duplicating the validation suite.
_Avoid_: Electivus workflow suite, infrastructure mirror

**Baseline infrastructure**:
The resources required to execute validation, available to the public fork without billing,
private runner fleets, or infrastructure secrets.
_Avoid_: Free tier, OpenAI infrastructure, optional accelerator

**Essential validation**:
Native Linux x64 tests whose success is required by the current merge gate.
_Avoid_: Cross-compile check, build-only signal, smoke test

**Release portability check**:
A required x64 musl release-profile compilation and lint signal that protects the static Linux
release path without treating musl as an Essential platform test lane.
_Avoid_: Essential validation, musl platform support, release test

**Change-triggered validation**:
A required merge-gate check whose expensive matrix runs only when a repository-owned detector marks
the affected surface; unrelated changes complete through a bounded metadata-only path.
_Avoid_: Optional check, informational check, path-filtered workflow

**Merge feedback budget**:
The target elapsed time from a pull-request head update to completion of the full Merge gate path:
120 minutes at the 95th percentile, reviewed after the first 20 eligible runs.
_Avoid_: Job timeout, average job duration

**Quarantined check**:
A demonstrably intermittent validation temporarily removed from the merge gate with tracking,
justification, continued execution in Extended validation, and a restoration deadline no later
than seven days after quarantine begins.
_Avoid_: Ignored failure, permanent skip, continue-on-error

**Coordinated baseline pin**:
A temporary shared commit baseline for concurrent fork changes that postpones upstream
synchronization until conflict risk is lower and requires full revalidation after synchronization.
_Avoid_: Maintained release line, indefinite divergence

**Upstream synchronization**:
The controlled integration of new `openai/codex` history into the Electivus default branch while
preserving fork-owned behavior and validation.
_Avoid_: Fork reset, upstream replacement, automatic update

**Release baseline**:
The immutable commit of a published, non-draft `openai/codex` Release selected for synchronization.
Automation selects the greatest stable Semantic Version; manual dispatch may select one exact stable
or pre-release Semantic Version.
_Avoid_: Upstream main, publication timestamp, automatically selected pre-release

**Fork development version**:
The `0.0.0` Rust workspace version that identifies Electivus source builds independently of the
published version attached to a Release baseline.
_Avoid_: Upstream release version, package version

**Synchronization PR**:
The single reviewable change that carries one pending Upstream synchronization through the Merge
gate before it enters the default branch.
_Avoid_: Sync branch, upstream alert, direct sync

**Synchronization baseline**:
The Release baseline fixed when a Synchronization PR is created and retained unchanged until that
PR is merged or closed. Closing without merge abandons that attempt; automation preserves it as
history and does not reopen it.
_Avoid_: Rolling release target, latest available release, automatic retry

**Validation PR**:
A draft pull request used to prove a compatibility patch on real fork infrastructure before its
merge gate is activated in repository rules.
_Avoid_: Experimental main push, local-only validation

**Extended validation**:
A post-merge validation contract for supported Linux architectures and specialized build paths that
have not already been promoted into the Merge gate, preserved as actionable signal without adding
latency to pull requests or repeating promoted validation.
_Avoid_: Merge gate, best-effort CI, optional tests

**Stability certification**:
Initial evidence on one commit consisting of one complete retry-free successful Merge gate run and
one successful run of every retained Extended validation suite for the same rollout/workflow
version.
_Avoid_: Merge-gate-only evidence, continuous reliability measurement

**Test reactivation certification**:
Evidence for one temporarily ignored test on one immutable commit consisting of exactly 20
consecutive retry-free Linux x64 executions, required before restoring ordinary test selection.
_Avoid_: Stability certification, flaky retry, quarantine

**Fresh merge gate**:
A successful merge gate evaluated against the current default-branch head rather than an older
base revision.
_Avoid_: Head-only success, stale green check
