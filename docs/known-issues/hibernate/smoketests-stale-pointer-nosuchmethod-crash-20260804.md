# `SmokeTests.testQueryConcurrency` — stale-pointer receiver crashes the whole run

**Status:** OPEN (2026-08-04). New witness of an already-tracked, accepted-open
mechanism (`java/lang/Object` register-invisible-root stale pointer, "Layer 1"
in `fixed-suite-bugs/tomcat/dohead-jit-heap-corruption-register-invisibility-FIXED.md`),
not a new root cause. Filed separately from
[`smoketests-concurrent-println-timeout-20260723.md`](smoketests-concurrent-println-timeout-20260723.md)
because the symptom is different (fatal crash, not a throughput timeout) even
though it's the same test method.

## Symptom

`org.hibernate.orm.test.sql.exec.SmokeTests#testQueryConcurrency`, real-JDK,
JIT on, `apps/hib-suite-runner` (dev tip `a43a74ded`, `CratonVM-hib-local-0712-v3`).
The 50-fork/400-iteration concurrency test itself fails an assertion first
(`testQueryConcurrency(SessionFactoryScope) FAILED`), then — instead of the run
continuing to the class's other tests — the whole process dies with an
**uncaught `NoSuchMethodError` propagating out of `CratonRunner.main`**, so no
`@@RESULT` line is ever printed and the harness records the class as `CRASH`
(status went `FAIL` in the pre-merge baseline -> `CRASH` after today's dev
merge + rebuild; see the residual-rerun report earlier this session).

```
[cratonvm_vm::runtime::interpreter::invoke] WARN Stale pointer detected in
  invokevirtual receiver (ptr=0x16cc325d598, all-zero header) — falling back
  to CP class java/util/Iterator
[cratonvm::gc::guard] WARN gen_heap::get_field: out-of-bounds field read
  dropped (...) obj=0x16cc325d598 index=0 num_slots=0 class_id=ClassId(0)
  class_name=java/lang/Object real_field_count=Some(0)
[RESID-DIAG READ] class=java/lang/Object index=0 num_slots=0 obj=0x16cc325d598
[cratonvm_vm::vm::vm_exec] WARN NoSuchMethodError
  method="java/lang/Object.awaitFinished()V"
  caller="org/junit/platform/engine/support/hierarchical/ThrowableCollector.execute(...)"
...
[cratonvm_vm::vm::vm_exec] WARN NoSuchMethodError method="java/lang/Object.hasNext()Z"
  caller="org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute(...)"
[cratonvm] main-vm run() returned Err: Exception in thread "main"
  java/lang/NoSuchMethodError: java.lang.Object.hasNext()Z
	at CratonRunner.main(CratonRunner.java:56)
	at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute(...)
	...
```

Full raw log:
`apps/hib-suite-runner/runs/run-20260804-113511-custom/on-real/shard-1/raw.log`
(search `org.hibernate.orm.test.sql.exec.SmokeTests`).

## Mechanism (already root-caused elsewhere — this is a new call site, not a new bug)

Only **one** stale-pointer event occurs (not a flood), and it is fatal because
of *where* it lands, not because of volume:

1. Some earlier object — the exact holder wasn't re-derived here, only that it
   reads `java/lang/Object`/`num_slots=0`/`class_id=0` once dereferenced — was
   reclaimed while a reference to it was still live in a register or stack slot
   invisible to the conservative root scan. This is exactly Layer 1 of the
   DoHead family: *"register-invisible roots → survivable all-zero-header
   stale-receiver flood... UNCHANGED — the real fix remains precise oop maps /
   shadow stack"* (accepted-open residual as of that doc's last update).
2. `cratonvm_native_collections::native_snapshot_itr_has_next`
   (`native-collections/src/lib.rs:34015`) calls `ctx.get_field(this, 0)` on
   the stale receiver expecting a snapshot-iterator's backing array field.
   `get_field` correctly detects the corrupted header and drops the read
   (`gen_heap::get_field: out-of-bounds field read dropped`) rather than
   reading garbage — the guard is doing its job.
3. But the *caller* (`native_snapshot_itr_has_next`'s fallback path, or the
   `invokevirtual` dispatch one frame up — the exact branch wasn't traced
   further) then resolves the method lookup against the corrupted header's
   `java/lang/Object` class identity instead of the receiver's real type
   (`ThrowableCollector$Executable` in one occurrence, a real `Iterator` in
   the other), producing `NoSuchMethodError: java.lang.Object.awaitFinished()V`
   / `java.lang.Object.hasNext()Z` — methods that only exist on the *intended*
   receiver class.
4. Unlike the Tomcat DoHead case (where the flood eventually SIGSEGVs via an
   unrelated free-list walk desync), here the `NoSuchMethodError` is a clean
   Java-level exception — but it propagates from deep inside JUnit Platform's
   own internals (`ThrowableCollector`, `EngineExecutionOrchestrator`) where
   nothing catches a `NoSuchMethodError`, so it unwinds all the way out of
   `CratonRunner.main` and kills the whole process. **The suite-level
   "CRASH" is a controlled fatal exception, not a native memory-safety
   crash** — worth knowing before spending native-crash-debugging effort
   (addr2line/symbolize/pdb) on it; the interesting bug is upstream, in
   whatever reclaimed the object while it was still root-reachable.

## Why this isn't HIB-CV-37 reopened

`HIB-CV-37`
closed a SIGSEGV in this exact test via native-collection-callback pinning
fixes (Rust locals holding `ObjectRef`s across *allocating* calls, invalidated
by a *moving* GC relocation). That mechanism and this one are different:
HIB-CV-37's fix targets relocation invalidating a callback-held reference
during a *moving* cycle; this crash's signature (`all-zero header`,
`num_slots=0`) is the *reclamation* case — a **non-moving** sweep freeing an
object whose only remaining reference lived in a register/stack slot the
conservative scanner never saw, the documented Layer-1 mechanism. HIB-CV-37's
own closure note already flags that it "did not rerun `SmokeTests.
testQueryConcurrency` end to end" — so this doc doesn't contradict that
closure, it just means HIB-CV-37's fixes (real and still present on dev) were
never sufficient to guarantee this test crash-free; a different mechanism
recurred here.

## Status of the underlying mechanism

Do not attempt a fresh root-cause here — read
`dohead-jit-heap-corruption-register-invisibility-FIXED.md` first; "Layer 1"
there is explicitly still open ("the real fix remains precise oop maps /
shadow stack", deferred). This doc's contribution is scope: the same
mechanism reaches Hibernate/JUnit-internals code (`ThrowableCollector`,
`EngineExecutionOrchestrator`, `native_snapshot_itr_has_next`), not just
Tomcat's AQS-park path — useful if/when someone picks up the precise-oop-maps
work and wants a second, independent repro family to validate against.

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "<jdk25>" --Xmx 1500m \
  @common.args -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.sql.exec.SmokeTests
```

Not yet confirmed deterministic — this is a single occurrence from one run.
Before spending investigation time, re-run 3-5x interleaved to establish a hit
rate (per [[feedback_interleave_ab_arms_never_run_them_in_separate_blocks]]),
since `dohead`'s own notes stress this family is timing-sensitive and can flip
from 0/6 to 3/6 depending on heap size and GC frequency alone.
