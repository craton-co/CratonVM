# `CollectionBinderTests` CRASHES the whole process: `String cannot be cast to org.junit.platform.engine.TestDescriptor`

**Status: FIXED (verified 2026-07-15) and retired to `docs/internal`.** The
historical process-fatal JUnit-engine-reporting crash no longer reproduces on
current `dev`. A freshly-built isolated CratonVM binary completed the full
37-test class cleanly in five independent JIT-enabled runs, as well as in a
JIT-disabled control and a HotSpot control. The original crash executed zero
tests; every verification run completed all 37 with zero failures, aborts,
skips, or failed containers.

## Resolution and verification (2026-07-15)

The original report recorded a single crash from an earlier `crashfail` shard
and an explicitly unconfirmed HashMap native-dispatch hypothesis. It did not
identify a primary exception or establish a causal code change, so this
retirement deliberately does **not** attribute the resolution to a guessed
commit.

Current `dev` was built from scratch in an isolated worktree and target
directory. The uniquely named binary
`cratonvm-springboot-collectionbinder-closure-20260715.exe` was then run
against the existing Spring Boot checkout with the suite runner. Results:

| Mode | Runs | Result |
|---|---:|---|
| CratonVM, JIT on | 5 | 37/37 tests passed in every run (8.3-17.1 s) |
| CratonVM, `--nojit` | 1 | 37/37 tests passed (10.0 s) |
| HotSpot control | 1 | 37/37 tests passed (3.3 s) |

The first JIT-on run also enabled `CRATONVM_DBG_CCE=1`; it completed normally
without the formerly process-fatal `String cannot be cast to
org.junit.platform.engine.TestDescriptor` path. This is a direct re-run of
the precise class named by this note, not a static-only closure. With no
residual failure in the original JIT mode or its controls, the issue is
retired.

## Symptom

`org.springframework.boot.context.properties.bind.CollectionBinderTests`
(`core/spring-boot`) terminates the whole `main` thread with an uncaught
`ClassCastException`, before a single `@Test` method is reported:

```
[2m2026-07-14T14:38:13.079710Z[0m [33m WARN[0m cratonvm_vm::vm::vm_util: Post-clinit fixup: Unsafe ARRAY_*_BASE_OFFSET/INDEX_SCALE populated (18/18)
[2m2026-07-14T14:38:13.101026Z[0m [33m WARN[0m cratonvm_vm::vm::vm_util: Post-clinit fixup: UnsafeConstants populated (5/5)
[2m2026-07-14T14:38:14.630138Z[0m [33m WARN[0m cratonvm_vm::vm::vm_util: Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/ClassCastException: java.lang.String cannot be cast to org.junit.platform.engine.TestDescriptor
	at SbRunner.main(SbRunner.java:36)
	at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute(SessionPerRequestLauncher.java:67)
	at org/junit/platform/launcher/core/DelegatingLauncher.execute(DelegatingLauncher.java:48)
	at org/junit/platform/launcher/core/InterceptingLauncher.execute(InterceptingLauncher.java:40)
	at org/junit/platform/launcher/core/ClasspathAlignmentCheckingLauncherInterceptor.intercept(ClasspathAlignmentCheckingLauncherInterceptor.java:25)
	at org/junit/platform/launcher/core/InterceptingLauncher.lambda$execute$0(InterceptingLauncher.java:41)
	at org/junit/platform/launcher/core/DelegatingLauncher.execute(DelegatingLauncher.java:48)
	at org/junit/platform/launcher/core/DefaultLauncher.execute(DefaultLauncher.java:93)
	at org/junit/platform/launcher/core/DefaultLauncher.execute(DefaultLauncher.java:114)
	at org/junit/platform/launcher/core/DefaultLauncher.execute(DefaultLauncher.java:125)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute(EngineExecutionOrchestrator.java:65)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.withInterceptedStreams(EngineExecutionOrchestrator.java:157)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.lambda$execute$0(EngineExecutionOrchestrator.java:66)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute(EngineExecutionOrchestrator.java:108)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute(EngineExecutionOrchestrator.java:179)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.failOrExecuteEngine(EngineExecutionOrchestrator.java:218)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.executeEngine(EngineExecutionOrchestrator.java:263)
	at org/junit/platform/launcher/core/OutcomeDelayingEngineExecutionListener.reportEngineFailure(OutcomeDelayingEngineExecutionListener.java:94)
	at org/junit/platform/launcher/core/DelegatingEngineExecutionListener.executionFinished(DelegatingEngineExecutionListener.java:47)
	at org/junit/platform/launcher/core/StackTracePruningEngineExecutionListener.executionFinished(StackTracePruningEngineExecutionListener.java:39)
	at org/junit/platform/launcher/core/StackTracePruningEngineExecutionListener.getTestClassNames(StackTracePruningEngineExecutionListener.java:67)
```

(identical trace repeated in the `[debug]` variant immediately below it in
the log; stdout log is empty — nothing was ever printed to stdout, no test
result lines at all). `results.tsv`: `rc=1`, `status=CRASH`,
`tests=0 failed=0 aborted=0 skipped=0`, elapsed `11.818s`.

## Historical analysis (hypothesis only; not a confirmed root cause)

**This is not a failure inside a `CollectionBinderTests` test method.** The
stack trace is JUnit Platform's *own internal machinery*, and specifically
its **engine-failure reporting path**:
`EngineExecutionOrchestrator.failOrExecuteEngine` →
`executeEngine` → `OutcomeDelayingEngineExecutionListener.reportEngineFailure`
→ `...executionFinished` → `StackTracePruningEngineExecutionListener.getTestClassNames`.
`failOrExecuteEngine`/`reportEngineFailure` only run when the JUnit Jupiter
**engine itself already failed** (e.g. during discovery for this class) —
i.e. there was a *primary* failure first, and JUnit's own code, while
trying to build a report describing that primary failure (walking test
descriptors to collect the set of affected class names for the summary),
crashes a *second* time with an unrelated `ClassCastException`. The
original/primary failure's message and stack trace are never printed —
this secondary crash pre-empts it entirely, which is itself notable: on a
correct JVM, `getTestClassNames` would iterate a `Set`/`Collection` of real
`TestDescriptor` objects (populated during discovery) and produce a report;
here one element handed back from that collection is a `java.lang.String`
where JUnit's own code expects a `TestDescriptor` and issues a `checkcast`.

This shape — the VM itself handing calling code an object of the wrong
type from a collection lookup, not any application-level logic error —
matches this codebase's recurring "wrong-type-from-native-collection"
corruption family (see `docs/internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md`,
which cross-references the now-fixed `NativeContextImpl::read_string`
`String[]`-misidentified-as-`String` bug, commit `e7e3bb91f`). **That
specific bug is already fixed and now has an explicit regression test**
(`read_string_returns_none_for_reference_array_field` in
`vm/src/vm/vm_object.rs`, "Pre-fix this returned Some(garbage); post-fix
must return None") — so this is very likely a **sibling occurrence with a
different mechanism**, not a reversion of that fix.

A concrete, timing-plausible candidate mechanism specific to *this* build:
this worktree's HEAD (`1021533f9`) sits directly on top of
`77f8b37e5` ("perf(vm): HashMap/Integer native-dispatch fast paths"),
committed the same day as this crashfail run (`2026-07-14`). That commit
added a **new early-exit fast path** in `jit_invoke_dispatch`
(`vm/src/jit/helpers.rs`, ~line 4453) that, for virtual/interface call
sites (`invoke_kind` 0 or 2), checks a thread-local
`HASHMAP_NATIVE_DISPATCH_CACHE` keyed by `info_key` *before* the normal
virtual-dispatch machinery runs, and — if the cached entry's
`receiver_class_id` matches the actual receiver's live class ID — calls
`call_hashmap_native_raw` directly, bypassing the interface-dispatch
resolution path entirely for that call. `getTestClassNames`-style code in
JUnit Platform is built almost entirely on `Map`/`Set`
(`LinkedHashMap`/`LinkedHashSet` collections of `TestDescriptor`), so any
correctness gap in that fast path's per-callsite caching (rather than
per-actual-target caching) for a real `HashMap`/`LinkedHashMap` receiver
holding heterogeneous JUnit bookkeeping objects is a plausible source of a
wrong-value-for-a-given-key read that would surface exactly as "got a
`String` where a `TestDescriptor` was expected." **This has not been
confirmed** — it is the most timing- and shape-consistent hypothesis
available from static inspection of the diff, not something reproduced
with `CRATONVM_DBG_CCE=1` in this session (see "What's missing" below).

The original/primary engine failure that triggered `reportEngineFailure`
in the first place is **completely unknown** — it never gets printed. It
could be anything (a discovery-time reflection error specific to
`CollectionBinderTests`'s large flat `@Test` method set, or something
environmental); finding it requires re-running with instrumentation that
intercepts the original `TestExecutionResult` before the reporting path
crashes.

## Historical limitation (resolved by the 2026-07-15 rerun)

The `apps\spring-boot` Spring Boot checkout is **not present in this
worktree** (`C:\craton\CratonVM-spring-boot-crashfail-20260714\apps\spring-boot`
does not exist — only the suite-runner scaffolding and prior results were
carried into this worktree). A live repro/bisection
(`CRATONVM_DBG_CCE=1` checkcast trace, `-Jit off` to rule out the new
fast path, a `--nojit` A/B against a pre-`77f8b37e5` binary) needs that
checkout restored first — see "Repro" below for the restoration command,
copied from `apps\spring-boot-suite-runner\run-spring-boot-suite.md`.
This doc records the log evidence and the leading hypothesis only.

## Repro

Restore the checkout (not present in this worktree) if needed:

```powershell
cd apps\spring-boot
git config core.longpaths true
git checkout HEAD -- settings.gradle gradle core module starter cli test-support `
  configuration-metadata loader config platform documentation smoke-test integration-test system-test
```

Then:

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot <repo>\apps\spring-boot `
  -ClassList <TSV row: core/spring-boot	org.springframework.boot.context.properties.bind.CollectionBinderTests> `
  -Start 1 -Count 1 -Exe target\release\cratonvm-spring-boot-suite.exe
```

Recommended follow-ups once repro is confirmed: `-Jit off` (rules the new
`HASHMAP_NATIVE_DISPATCH_CACHE` early-exit path, and JIT generally, in or
out); `CRATONVM_DBG_CCE=1` to get the exact failing `checkcast` bytecode
site inside `getTestClassNames`/whatever JUnit Platform code populates the
collection it reads from; bisecting the binary against a build from just
before `77f8b37e5` (parent `0063b0b0e`).

## Related

- [`onclasscondition-npe-cast-string-array-cluster-FIXED.md`](-string-array-cluster-FIXED.md) —
  same general *shape* (wrong-type-from-collection cast crash), root cause
  already fixed+regression-tested (`e7e3bb91f`); this is a plausible but
  unconfirmed sibling, not a reversion of that fix.
- [`brave-baggagefields-classcast-summary-printing-FIXED.md`](brave-baggagefields-classcast-summary-printing-FIXED.md) —
  another "clean" (non-memory-corrupt) `ClassCastException` during JUnit's
  own summary-printing path, different mechanism (stale `Collections`
  static field), same general symptom category (a CCE that surfaces deep
  inside JUnit Platform's own bookkeeping rather than application code).
- `perf(vm): HashMap/Integer native-dispatch fast paths` (`77f8b37e5`) —
  leading (unconfirmed) suspect; see `docs/internal/hashmap-native-dispatch-overhead.md`
  for the retired/fixed baseline bug this new fast path builds on top of.
