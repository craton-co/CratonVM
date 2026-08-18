# `checkcast` after `AtomicReference.get()` reads a reclaimed object — a JIT precise-root-map gap, not a checkcast logic bug

**Status: OPEN, reproduced and diagnosed 2026-08-17, not fixed.**

Found triaging the Apache Commons Math test suite
(`apps/commons-math/RESULTS-20260817.md`): `DerivativeStructureTest` fails
~10 of its 124 JUnit4 methods with

```
java.lang.ClassCastException: class [Ljava.lang.Object; cannot be cast to
  class [[Lorg.apache.commons.math4.legacy.analysis.differentiation.DSCompiler;
  ([Ljava.lang.Object; is in module java.base of loader 'bootstrap';
  [[Lorg.apache.commons.math4.legacy.analysis.differentiation.DSCompiler;
  is in unnamed module of loader 'app')
	at org.apache.commons.math4.legacy.analysis.differentiation.DSCompiler.getCompiler(DSCompiler.java:190)
```

Line 190 is:

```java
private static AtomicReference<DSCompiler[][]> compilers = new AtomicReference<>(null);
...
final DSCompiler[][] cache = compilers.get();   // <- line 190, the failing checkcast
```

## What it is not

This looked at first like a `multianewarray`/checkcast logic bug — CratonVM
producing the wrong runtime class for a freshly allocated `DSCompiler[][]`, or
mishandling the type-erasure checkcast that `AtomicReference<T>.get()` compiles
to. Both hypotheses were disproved directly:

* `MultiArrayRepro.java` / `CasArrayRepro.java` (minimal `new Foo[3][4]` through
  `AtomicReference.set()`/`.get()`/`.compareAndSet(null, ...)`) — passes on
  CratonVM, identical to HotSpot, at every step including the exact
  `compareAndSet(null, newCache)` publish pattern `DSCompiler.getCompiler` uses.
* `DSCompilerDriver.java` — drove the real `DSCompiler.getCompiler` through the
  same incremental-growth sequence the test suite exercises (`(0,0)→(1,0)→…→(3,3)`,
  including the `System.arraycopy` row-preservation path) directly, outside
  JUnit — passes.
* `DSCompilerHot.java` — 50 000 warm-up calls to `getCompiler(2,1)` (well past
  the default `CRATONVM_TIER_C1_THRESHOLD=500` / `CRATONVM_TIER_C2_THRESHOLD=20000`)
  then a single growth call to `getCompiler(6,1)` — passes, with the method
  demonstrably hot enough to be JIT-compiled at both tiers.

None of these converge on the failure. **The real test class does**, and two
observations point at *why* a repro has to be the real thing, not a
hand-written stand-in:

1. `--nojit`: **124/124 PASS**, `0` `ClassCastException`s (confirmed on the
   unmodified class, `timeout 600`, full run — see reproduction below).
2. Copying `DSCompiler.java`, adding *pure diagnostic* `System.getenv(...)` +
   `System.err.println` calls inside `getCompiler` (no behavioural change) and
   shadowing the original on the classpath: **124/124 PASS**, bug gone. Adding
   unrelated bytecode is enough to make the JIT compile a different artifact for
   this method and the corruption stops reproducing.

Both are the standard signature of a JIT-only, compilation-shape-sensitive
correctness bug, not a stable logic error — nothing a source-level reasoning
about `getCompiler`'s own code should have to explain, and it doesn't.

## The actual mechanism — a GC root-publishing gap at the `checkcast` site

CratonVM's own reclaimed-receiver guard names it directly. Re-running the real
`DerivativeStructureTest` with `CRATONVM_DBG_JIT_NAMES=1` and the guard active,
the exact moment of failure logs (`vm/src/memory/reclaim_guard.rs`,
`report_root_slice_provenance`):

```
ERROR cratonvm::gc::guard: …and this is where that address stood in the OWNING
thread's own GC bookkeeping. `in_published_snapshot=false` means the snapshot
the collector marks this thread from did not contain a slot the thread's
frames hold — a root COLLECTION gap, not a mark or sweep one.
  obj=0x12844b62bb8 site=checkcast in_published_snapshot=false
  published_roots=120 last_publish_at_collection=1 collections_now=1
  last_publish_pc=35 holder=<not found in frames> in_blocked_region=false
  frames=43 top_frame=DSCompiler.getCompiler pc=9
```

Reading this against `reclaim_guard.rs`'s own doc comments: this reporter only
fires once `report_reclaimed_receiver` has already established the object at
`addr` sits in a **reclaimed hole** — i.e. the checkcast is not looking at a
wrong-but-live object, it is looking at memory the collector already reused.
`in_published_snapshot=false` says why: whatever slot the compiled frame was
holding the array reference in at `getCompiler` pc=9 (right around the
checkcast on `compilers.get()`'s result) was **not included** in this thread's
published root snapshot. A GC that ran while this frame was live at that pc
therefore never saw the reference, treated the array as garbage, reclaimed its
memory, and by the time the checkcast dereferences it to read the class
pointer, it reads whatever now occupies that address — which decodes as
`[Ljava.lang.Object;`'s class rather than the array's real
`[[Lorg…DSCompiler;` class. `holder=<not found in frames>` is consistent: by
the time this diagnostic runs (after the fact, from the terminal error path),
the frame slot no longer contains the stale reference either — it was live
only at the moment the GC's snapshot should have captured it and wasn't.

This explains every observation above:

* `--nojit` — the interpreter has no compiled root map to get wrong; passes.
* Adding unrelated instrumentation changes the compiled method's register
  allocation / stack-map shape enough that the array reference either lands in
  a slot that IS published, or the timing shifts so no GC lands exactly at
  pc 9 while it's live only in the gap — either way the race window closes.
* Pure driver repros (`DSCompilerDriver`, `DSCompilerHot`) don't reproduce it
  because they don't generate the same allocation pressure / GC timing as the
  full 124-method JUnit4 class, which is consistent with this being a race
  between "the compiled frame reaches pc 9 with the live reference in an
  unpublished slot" and "a GC actually runs at that exact moment" — a
  timing-dependent root-map bug, not a deterministic logic bug.

## What this affects

`site=checkcast` in the guard's log line is a general tag, not one specific to
this array type — the bug class is "a live reference at a `checkcast` program
point in JIT-compiled code is not always included in the precise root map",
which is not intrinsically specific to arrays, `AtomicReference`, or this
method. `DSCompiler.getCompiler` is simply the shape that reliably surfaces it
(a value read from a field/`AtomicReference`, immediately checkcast, under
allocation pressure sufficient to trigger a GC at that pc).

## Reproduction

```bash
CV="<worktree>/target/release/cratonvm.exe"
JDK="<jdk25>"
CP="<see apps/commons-math/RESULTS-20260817.md for how to build the aggregate
     test classpath; junit-platform-launcher must be added manually, it is not
     a transitive test-scope dependency>"
RUNNER="<dir containing CratonRunner.java from apps/netty-suite-runner/,
         compiled standalone>"

# Fails ~10/124 with the ClassCastException above:
"$CV" --java-home "$JDK" --Xmx 1g -c "$RUNNER;$CP" CratonRunner \
  org.apache.commons.math4.legacy.analysis.differentiation.DerivativeStructureTest

# Passes 124/124 — confirms JIT-only:
"$CV" --java-home "$JDK" --Xmx 1g --nojit -c "$RUNNER;$CP" CratonRunner \
  org.apache.commons.math4.legacy.analysis.differentiation.DerivativeStructureTest

# Re-run with CRATONVM_DBG_JIT_NAMES=1 to see the cratonvm::gc::guard
# site=checkcast in_published_snapshot=false line at the moment of failure.
```

## What would fix it

Not attempted here — this is a JIT precise-root-map / safepoint-publishing
correctness bug, the same general class flagged repeatedly elsewhere in this
project's history as needing careful, methodical isolation (see
`docs/known-issues/jit/` siblings and the GC-guard machinery's own doc
comments in `vm/src/memory/reclaim_guard.rs`). The next step is almost
certainly instrumenting the compiled artifact for `DSCompiler.getCompiler`
directly — dump its root/stack map at pc 9 (`CRATONVM_DBG_JIT_NAMES`, and
whatever prints the per-pc oop map for a published `CompiledMethod`) and
compare against what the checkcast bytecode's operand-stack/local state
actually needs live at that point, rather than reasoning from the Java source.

## Related

* `docs/known-issues/jit/osr-refuses-any-method-with-an-exception-table-20260817.md`
  — same day, same general territory (JIT correctness gaps around exception
  handling / precise state), different mechanism.
* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
