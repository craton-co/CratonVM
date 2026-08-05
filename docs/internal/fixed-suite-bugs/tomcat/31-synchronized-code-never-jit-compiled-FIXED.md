# 31 - synchronized code can compile safely (FIXED 2026-07-28)

**Status:** FIXED. Retired from `docs/known-issues/tomcat/` after validating
both implicit `ACC_SYNCHRONIZED` methods and javac's explicit
`synchronized (...)` cleanup-handler form with the real Tomcat fixture.

## Defect

Two independent admission rules left lock-bearing Java code interpreted for
the life of a process:

1. `ACC_SYNCHRONIZED` was both an initial JIT refusal and a permanent skip.
2. A protected `putfield` could not retain the precise exceptional frame that
   javac's monitor-cleanup handler needs, so the synchronized-block form was
   rejected before code generation.

## Fix

`JitSynchronizedMonitorGuard` now owns the implicit method monitor around a
compiled body. It pins the receiver/class mirror across native execution,
uses the collector-forwarded reference for the matching release, and hands
ownership to an interpreter exception frame exactly once when an in-method
handler is selected. Background tiering is allowed to publish this wrapped
entry; raw direct-call APIs deliberately continue through generic invocation
so they cannot bypass the monitor contract.

The x64 backend now emits a precise `putfield` null trap for protected sites.
It publishes the pending NPE and its exceptional frame before routing to the
Java handler, allowing javac's synthetic `monitorexit` cleanup to execute.
`getfield`/`putfield` are therefore admitted at the relevant precise-frame
sites instead of being rejected by RBC.6.

> **Correction (2026-08-02).** The last sentence stopped being true within days
> of this doc: `getfield`/`putfield` were taken back OUT of
> `precise_frame_publishing_opcode` after `probes/Rbc6FieldProbe.java` found
> the precise null trap had a single call site, on the inlined-callee
> `putfield` path, while the top-level arms still took an inline fast path.
> Both top-level arms grew one later (`cd451facc` for `putfield`; every
> non-raw `getfield` path routes a null receiver through
> `emit_post_invoke_exception_check`), and the admission was restored on
> 2026-08-02 — but between 2026-07-28 and then, this paragraph was the only
> doc saying so, and it was wrong. See
> [rbc6-protected-field-ops-FIXED-20260802.md](../../rbc6-protected-field-ops-FIXED-20260802.md).

## Validation

Task artifact: `cratonvm-tomcat-syncjit-r3.exe`.

* `SynchronizedJitContractProbe` passed with JIT and with
  `CRATONVM_DISABLE_JIT=1`. It checks instance/static monitor ownership,
  caught and propagated exceptions, cross-thread exclusion, and a null
  `putfield` inside a synchronized block.
* `CRATONVM_DBG_JITC=1 SyncMethodProbe` recorded C1 and C2 compilation of
  `SyncMethodProbe.sync(II)I`, plus compilation of `syncBlock(II)I`; the old
  `rbc6-handler-reads-unsafe-local` bail is absent.
* `MonitorCostProbe` and `ByteReadProbe` passed in JIT and no-JIT modes.
* The exact Tomcat regression was run through the suite classpath and one
  JUnit fixture using `RunMethods`:
  `TestHostConfigAutomaticDeploymentAddition.testAdditionWarAddDir` completed
  normally in both JIT and no-JIT modes. Each run deployed the WAR, exercised
  the competing directory addition, shut Tomcat down, and ended with
  `main-vm run() returned Ok` (one selected method, zero failures).

The full addition class was also allowed to run until its suite runner's
25-minute class-level ceiling. Its log showed sequential deployment cases;
the ceiling is not a test failure and is unrelated to the synchronized-method
admission defect. The documented exact method above is the authoritative
regression gate.

## Follow-up ownership

This closes the synchronized-JIT correctness/admission bug. Independent
Tomcat performance assertions remain tracked in known issue 32 and must not
be attributed to a permanent synchronized-method JIT exclusion.
