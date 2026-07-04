# HIB-CV-39 — `DynamicBatchFetchTest` SIGSEGV: possible regression / reopening of HIB-CV-37

**Status:** CLOSED 2026-07-03 — not a new bug, not a reopening of HIB-CV-37.
**Original mode:** real-JDK, JIT on. Dev at time of original observation: `d9cb7be8`.
**Test:** `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`

## Original symptom

```
process-died rc=139 (SIGSEGV)
```

Observed in an isolated single-class rerun (TIMEOUT=600, `apps/hib-suite-runner`,
`cvjit.exe` built from dev `d9cb7be8`), 2026-07-03, before the HIB-CV-38
Boolean fix had landed.

## Closure note

The original doc posed three hypotheses (a native-callback GC-root gap the
HIB-CV-37 pinning sweeps missed, an unrelated one-off crash, or non-deterministic
timing like the `type.temporal.*` cluster) and left them undistinguished pending
a backtrace.

**What actually happened**, established by rebuilding `cratonvm-cli` from dev in
a fresh worktree (branch `investigate/hib-cv-39-dynamicbatchfetch-sigsegv`) and
rerunning the identical repro three times against a binary that includes the
HIB-CV-38 fix (`docs/internal/hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md`):

- **0/3 reruns SIGSEGV'd.** All three (JIT on default heap; JIT on with
  `CRATONVM_JIT_DISABLE_INLINE_NEW=1`; JIT on with `-Xmx 6000m`) instead
  deterministically reproduced the separate, already-tracked, then-**open**
  `docs/internal/fixed-suite-bugs/jit-inline-alloc-array-header-corruption-hibernate-batch.md`
  bug (the "inconsistent header — kind=Object but array_length=N" GC
  corruption warning storm, terminating in an `OutOfMemoryError` at default
  heap, or a slower failure at a larger heap).
- The HIB-CV-38 doc itself predicted this outcome: it explicitly notes that
  `Boolean.FALSE`/`Boolean.valueOf(false)` being unconditionally null (the
  pre-fix corruption) "is about as foundational a corruption as CratonVM can
  have ... so it very plausibly also explains sporadic/rotating crash
  signatures elsewhere (SIGSEGV in the original report vs. this NPE here)
  depending on what happens to dereference the bad `TRUE`-as-`Class` object or
  unbox the null `FALSE` at a given call site and heap layout."

Putting these together: the original SIGSEGV was most likely a downstream
crash caused by the (at-the-time-unfixed) HIB-CV-38 Boolean static-slot
corruption — some code path dereferenced the corrupted `Boolean.TRUE` (a
`Class` mirror masquerading as a `Boolean`) or unboxed the always-null
`Boolean.FALSE` in a way that faulted rather than throwing. None of the three
original hypotheses (HIB-CV-37 native-callback gap, unrelated one-off, or
temporal-cluster-style non-determinism) hold up: the crash is gone on a
HIB-CV-38-fixed binary, and what's left in its place is a **pre-existing,
separately-tracked, already-open** bug, not a new one.

This doc is archived rather than kept open because its own question — "is
this a regression/reopening of HIB-CV-37, or something new" — is answered
(no to both). The residual failure mode it happened to observe has since been
fixed and archived at
`docs/internal/fixed-suite-bugs/jit-inline-alloc-array-header-corruption-hibernate-batch.md`,
which was updated with this session's truth-table evidence (ruling out the
plain-`new` JIT inline-TLAB path and confirming arrays never had an inline
path to begin with).

## Repro (for reference)

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner "<listfile-with-DynamicBatchFetchTest>" 0
```

Needs a binary built with the HIB-CV-38 Boolean fix to get past JUnit-launcher
bootstrap at all; see the OOM doc above for the current, still-open residual.
