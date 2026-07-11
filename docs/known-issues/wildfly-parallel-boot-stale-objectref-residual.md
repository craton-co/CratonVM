# WildFly standalone boot still non-deterministically crashes after the ServiceLoader reflection fix — residual stale-`ObjectRef` sites

Status: OPEN — split off 2026-07-11 from
[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] (now FIXED at
`docs/internal/fixed-suite-bugs/wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md`)
after that fix's own verification showed it resolves the majority but not all of the affected
classes.
Severity: Moderate-to-High — still blocks a meaningful fraction (in a 10-class sample, ~40%) of
`testsuite/integration/basic` classes from booting the managed server at all.

## Background

The linked fixed-suite-bugs doc root-caused and fixed a stale-`ObjectRef`-across-GC bug in
`create_constructor_object`/`create_method_object`/`create_field_object`
(`native-builtins/src/lang_class.rs`) that corrupted several WildFly `Extension` SPI instances loaded
via `ServiceLoader` during standalone-server boot, cascading into a
`NullPointerException: ... "this.controller" is null` crash in `AbstractControllerService` — exit code
1, before `server.log` was ever written.

Verifying that fix against the real Surefire/Arquillian harness (not an isolated repro) on a 10-class
sample showed:

- 6/10 classes now boot far past the original crash point (60-70s of real subsystem-add processing
  instead of an instant 8-13s crash).
- 4/10 classes still hit the **identical** `this.controller is null` crash, at the identical point in
  boot (`registerModelControllerServiceInitializationBootStep` → `ServerService.boot` →
  `AbstractControllerService$1.run`), deterministically across repeated retries for at least one class
  (`org.jboss.as.test.integration.ee.concurrent.DefaultManagedThreadFactoryTestCase`, 3/3 identical
  failures) and non-deterministically for another
  (`org.jboss.as.test.integration.ejb.jndi.OverriddenAppNameTestCase`, which hit the *original* crash in
  one run and a *different*, later-stage failure — see below — in another run of the identical fixed
  binary).
- Critically: for classes that now boot further, **no** `ServiceLoader: Constructor.newInstance failed
  with an internal VM error` warnings appear anywhere in the log — the specific bug that was fixed is
  confirmed absent. This means the remaining `this.controller is null` crashes, and the new failures
  described below, stem from at least one **additional, different** unprotected-`ObjectRef`-across-GC
  call site (or a related-but-distinct mechanism) that the ServiceLoader fix didn't cover.

## New evidence: two further symptoms of the same general bug class

Re-running `org.jboss.as.test.integration.ejb.jndi.OverriddenAppNameTestCase` against the fixed binary
a second time (60s timeout instead of the default) produced a markedly different, much-further-along
failure than the first run of the same class/binary (which hit the original NPE in 8s) — direct
evidence this is a **timing-window** bug, not a fixed, deterministic-per-class defect:

```text
[WARN] NoSuchMethodError method="java/lang/Object.read([CII)I" caller="org/wildfly/common/cpu/ProcessorInfo.readCPUMask()I @pc=35"
[WARN] gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's
  layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative
  collection-layout probe dispatched on a non-matching receiver type) obj=0x20020533818 index=16
  num_slots=0 class_id=ClassId(0) class_name=java/lang/Object real_field_count=Some(0)
[WARN] Stale pointer detected in invokevirtual receiver (ptr=0x20020533818, all-zero header) —
  falling back to CP class org/jboss/as/controller/AbstractOperationContext
...
ERROR [org.jboss.as.controller.management-operation] WFLYCTL0403: Unexpected failure during execution
  of the following operation(s): null
    java.lang.NullPointerException: Cannot invoke "org.jboss.dmr.ModelValue.has(String)" because
    "this.value" is null
        at org.jboss.threads.EnhancedQueueExecutor$ThreadBody.run(EnhancedQueueExecutor.java:1377)
        at org.jboss.as.controller.ParallelBootOperationStepHandler$ParallelBootTask.run(ParallelBootOperationStepHandler.java:368)
        at org.jboss.as.controller.AbstractOperationContext.executeOperation(AbstractOperationContext.java:469)
        ...
        at org.jboss.dmr.ModelNode.has(ModelNode.java:1528)
```

Two distinct symptoms here, both bearing the same fingerprint as the already-fixed bug
(`class_id=ClassId(0) class_name=java/lang/Object` — a stale reference resolving to a reused,
all-zero-header slot):

1. **`ProcessorInfo.readCPUMask()` → `NoSuchMethodError: Object.read([CII)I`.** The receiver of a
   `.read(char[], int, int)` virtual call resolves to `java.lang.Object`'s vtable instead of the real
   `Reader`/`InputStreamReader` subtype — i.e. the receiver `ObjectRef` had already gone stale (its
   backing memory reused and zeroed) by the time the invokevirtual dispatch ran. `ProcessorInfo` reads
   `/proc/self/status` to determine the CPU count; find and audit whatever native code backs that read
   path for an unpinned `ObjectRef` held across an allocating call, analogous to the fix in the linked
   sibling doc.
2. **`ParallelBootOperationStepHandler$ParallelBootTask.run` (an `EnhancedQueueExecutor` worker thread)
   → `NullPointerException` on `ModelValue.has`.** WildFly's *parallel* boot-step execution runs
   multiple `ModelNode`/`ModelValue` operations concurrently across worker threads. This is a
   plausible place for a native allocation site to race with a concurrent GC in a genuinely
   multi-threaded way (as opposed to the single-threaded ServiceLoader loop the sibling fix closed) —
   worth checking whatever native code participates in `ParallelBootOperationStepHandler`'s
   `ModelNode`/`ModelValue` construction/mutation for the same missing-pin pattern, and whether
   CratonVM's `gen_heap::guard` fallback (the "Stale pointer detected... falling back to CP class"
   diagnostic) is being hit specifically from *concurrent* GC-moves rather than single-threaded
   sequencing — which would point at a different class of fix (e.g. a missing read/write barrier or
   GC-safepoint check on a hot multi-threaded path) rather than another missing `pin_native_root` call.

CratonVM already has defensive instrumentation for exactly this failure mode
(`cratonvm::gc::guard::gen_heap::get_field`/`set_field` "out-of-bounds field read/write dropped", and
the interpreter's "Stale pointer detected in invokevirtual receiver... falling back to CP class")
— these are guard rails that prevent an outright crash/UB on a detected stale pointer, but the
fallback itself doesn't recover the *correct* object, so the corruption still surfaces as an
application-visible NPE/NoSuchMethodError one or more frames later. Searching for other call sites
that trigger this same guard-rail path (not just in WildFly boot) may reveal further latent instances
of this bug class beyond WildFly.

## Suggested next steps

1. Confirm whether `ProcessorInfo.readCPUMask()`'s native backing (search `native-builtins` /
   `native-io` for `readCPUMask`, `ProcessorInfo`, or wherever `/proc/self/status` gets read) holds an
   unpinned `Reader`/byte-array `ObjectRef` across a GC-triggering call, mirroring the diagnosis
   pattern in the sibling fixed-suite-bugs doc.
2. For the `ParallelBootOperationStepHandler` case, first determine whether the corruption is
   single-threaded (same missing-pin pattern, just a different call site — the existing fix's
   technique applies directly) or genuinely concurrent (a GC-safepoint/barrier gap specific to
   multi-threaded execution — a different, likely harder fix). Reproduce by running
   `OverriddenAppNameTestCase` (or another affected class) repeatedly against
   `frozen-cratonvm-wf-surefire-boot-20260711-v2.bin` with `--class-to 90`+ and checking whether the
   `ModelValue.has` NPE recurs at the same call site each time it manifests.
3. Once a broader fix lands, re-run a full-suite round (not just a 10-class sample) to get an exact
   before/after count for the `testsuite/integration/basic`+`domain` clusters, since the 96%-of-failures
   figure from round 6 predates both this residual finding and the sibling fix.

## Evidence

```text
Azure host 20.83.144.174, worktree /data/data/wt-wf-surefire-boot-20260711 (repro harness copy),
  binary /data/data/frozen-cratonvm-wf-surefire-boot-20260711-v2.bin (post-ServiceLoader-fix)
/data/data/cratonvm/apps/wildfly/testsuite/integration/basic/target/surefire-reports/
  org.jboss.as.test.integration.ejb.jndi.OverriddenAppNameTestCase-output.txt (captured during the
  "verify-npe-again" re-run showing the ProcessorInfo/ParallelBootOperationStepHandler failure)
```
