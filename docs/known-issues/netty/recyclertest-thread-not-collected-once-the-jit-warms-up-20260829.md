# `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced` — a finished Thread stays reachable, and the dose is JIT compilation

## Status

**OPEN, 2026-08-29.** Deterministic, HotSpot-clean, and reduced to a
one-binary one-lever A/B. Not root-caused to a frame.

| arm | failures of 6 |
|---|---:|
| HotSpot 25, same host and classpath | **0** (3/3 runs) |
| CratonVM, `--nojit` | **0** |
| CratonVM, default | **4** (3/3 runs, same four every time) |
| CratonVM, `CRATONVM_BG_COMPILE=0` | **6** |

Found while triaging five netty classes that were non-clean on `dev` and
tracked nowhere. Of those five this is the only new defect: two were a harness
gap (see `not-cratonvm-bugs-consolidated.md`), one was already documented, and
one was never failing at all.

## Symptom

```
java.util.concurrent.TimeoutException:
  testThreadCanBeCollectedEvenIfHandledObjectIsReferenced(OwnerType, boolean)
  timed out after 5000 milliseconds
	at io.netty.util.RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced(RecyclerTest.java:216)
```

Line 216 is the test's own collection loop:

```java
thread = null;
// Loop until the Thread was collected. If we can not collect it the Test will fail due of a timeout.
while (!collected.get()) {
    System.gc();
    System.runFinalization();
    Thread.sleep(50);
}
```

`collected` is set by a `finalize()` override on the `Thread` subclass. So the
test asserts one thing: a Thread that has finished, been joined, and had its
last named reference nulled must become collectable — even though an object it
allocated is still strongly referenced.

Failing parameterisations, identical on every run: `[2] NONE,false`,
`[4] PINNED,false`, `[5] FAST_THREAD_LOCAL,true`, `[6] FAST_THREAD_LOCAL,false`.
Passing: `[1] NONE,true`, `[3] PINNED,true`. **That split is a red herring** —
see below.

## The dose-response, which is the whole finding

The 6 invocations are the FIRST six tests the class runs; nothing precedes
them. So it is not state left by other test methods. What decides the outcome
is how much has been JIT-compiled by the time the loop runs:

| what is discovered | default (background compile) | `CRATONVM_BG_COMPILE=0` |
|---|---:|---:|
| only these 6 tests (`MethodRunner` method selector) | **0 failed** | **6 failed** |
| the whole class, 67 tests (`CratonRunner`) | 4 failed | 6 failed |

Same binary, same six tests, one lever, 0/6 → 6/6. And within the default
full-class run the *order* is monotone with warm-up: `#1` passes (689 ms,
still warming), `#3` passes, and `#6 #2 #5 #4` — everything after — fails.

So the guarded/unguarded split in the failing set is not a Recycler property.
It is where the warm-up curve happened to cross.

## What it is NOT

Four hypotheses, each killed by a control rather than by argument. The probes
are in `internal/repros/netty-recycler-thread-retention-20260829/`.

**Not "finalizers do not run", and not "dead Thread mirrors leak".**
`ThreadCollectProbe` runs the test's own `System.gc(); System.runFinalization()`
loop over seven shapes — a plain object, an unstarted Thread, a started and
joined Thread, one that set a `ThreadLocal`, one whose allocation is still held
from a static, one that parked, one that took a monitor. **All seven collect on
both VMs**, in 52-70 ms.

**Not the field updater.** The guarded/unguarded split is suggestive: netty's
guarded handle (`Recycler$DefaultHandle`) carries an
`AtomicIntegerFieldUpdater` and the unguarded one has no updater, so "an
updater roots its target" would explain the split exactly. It is false.
`UpdaterRetainProbe` touches an object through `AtomicInteger`/`Long`/
`ReferenceFieldUpdater` and a `VarHandle` and drops it: **all eight arms
collect on both VMs.**

**Not the Recycler's own reference graph.** `RecyclerRetainProbe` rebuilds the
test's shape standalone across all six owner/guard combinations with the body
progressively stripped (`bare`, `noget`, `nokeep`, `nopin`, `full`).
Every `full` cell collects on CratonVM. The one cell that retains — `nopin`,
i.e. the test's body without `Recycler.unpinOwner` — retains on **both** VMs,
which is correct: a pinned Recycler is supposed to hold its owner.

**Not the inline-splice family.** `CRATONVM_JIT_INLINE_CALLS=0` and
`CRATONVM_JIT_IR_INLINE=0` both leave it at 4 failures, so this is not the
"a spliced callee's locals are named by no oop map" defect fixed on
2026-08-28.

## It does not reproduce outside JUnit, and that is the sharpest thing known

Two more shapes were tried, and both collect. Together they say the retaining
reference is **not in the test's own frame**.

`FrameRetainProbe`. The netty test nulls `thread` and then runs the
`System.gc()` loop IN THE SAME METHOD; the earlier `RecyclerRetainProbe` ran it
in a callee. That is the difference between "a compiled frame pins a dead
local" and something else, and nobody had tested it. Three arms — loop inline
in the frame that held the reference, loop in a callee, and a looping frame
that never held the reference at all — **all collect**, on both VMs, under
`CRATONVM_BG_COMPILE=0`. A compiled frame holding a nulled local does not, on
its own, retain.

`RecyclerInlineProbe` is then the one cell neither probe covered: the netty
body (Recycler allocated on the thread, its object kept) **and** the loop
inline, all six owner/guard combinations, `BG_COMPILE=0`, two rounds. **All
twelve cells collect**, in 65-75 ms.

So the standalone shape cannot be made to fail. What is left that the suite has
and the probe does not is JUnit itself: reflective invocation, the parameterized
machinery that holds the `Object[]` arguments and the test instance, and — for
this method — the `@Timeout` executor that runs each invocation on its own
thread. The retainer is somewhere in that, and the probes above are what
narrowed it to there rather than to netty or to the test's own frame.

A successor should start from that, not from the Recycler. The instrument to
build is "which root reaches this object", asked at the `System.gc()`
safepoint; the nearest existing thing is the
`CRATONVM_DBG=remap-residue` frame walk from the 2026-08-28 splice fix, which
answers a different question (words still pointing into from-space) and would
need turning around.

## What it is, and how far the narrowing got

A reference the bytecode has already nulled is still reachable from
somewhere on the collecting thread's stack once enough is compiled. `--nojit`
removes it; more compilation makes it worse; nothing else moves it. Note the
section above: it is NOT simply the frame that held the local, so "conservative
scan pins a dead slot" is too small an explanation.

The precision levers do **not** move it, which is the part that still needs
explaining:

| lever | failures |
|---|---:|
| `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` | 4 |
| `CRATONVM_NO_CONSERVATIVE_LOCALS=1` | 4 |
| `CRATONVM_JIT_OSR=0` | 4 |
| `CRATONVM_ZGC_RELOCATE=0` | 4 |
| `-XX:+UseG1GC` | 4 |
| `--Xmx 256m` / `1500m` / `6g` | 4 / 4 / 4 |

Collector-independent and heap-size-independent, so it is a *reachability*
answer and not a collection-policy one.

`CRATONVM_JIT_DENY=RecyclerTest` (and the slashed spelling) does not help
either, under `BG_COMPILE=0`: still 6. Read that carefully — `JIT_DENY` reaches
the compile doors only, and a denied method can still be inline-spliced into a
caller, so this does NOT establish that the test method's own frame is
innocent. It establishes that denying it at the doors is not enough.

The obvious next instrument is the one that named the 2026-08-28 splice defect:
walk the collecting thread's JIT frames at the `System.gc()` safepoint and
report every word that still points at the Thread mirror, with the method it
belongs to. A callee-saved register that was never spilled is the shape this
tree has seen before and the one the conservative-locals lever would not reach.

## Why it matters beyond this test

The failure mode is "an object Java semantics says is unreachable stays alive
while a compiled frame is on the stack". Anything whose correctness depends on
*collectability* rather than on values sees it: finalizers, `WeakReference`
and `PhantomReference` clearing, `Cleaner`, `ReferenceQueue` draining, and
every cache keyed on a weak key. A test is where it is visible; a leak is where
it is expensive.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.RecyclerTest                     # 4 of 6

# the lever, on the SIX tests alone -- 0 failed, then 6
cratonvm.exe ... MethodRunner \
  'io.netty.util.RecyclerTest#testThreadCanBeCollectedEvenIfHandledObjectIsReferenced(io.netty.util.RecyclerTest$OwnerType, boolean)'
CRATONVM_BG_COMPILE=0 cratonvm.exe ... MethodRunner '<same>'
```

`MethodRunner.java` (already in the suite runner, unstaged fixture) is what
takes a `Class#method(params)` selector and prints `@@BEGIN`/`@@END` per test —
the per-invocation ordering above is unreadable without it.

Note `-Djunit.jupiter.execution.timeout.mode=disabled` does not turn this into
a pass: the class then runs past **240 s** on invocation `#6`. It is a real
retention, not a slow collection.

## Related

* `internal/repros/netty-recycler-thread-retention-20260829/` — the three
  probes and what each rules out.
* `internal/fixed-suite-bugs/netty/longlonghashmaptest-npe-spliced-ctor-this-not-a-gc-root-FIXED-20260828.md`
  — the same family (a JIT frame the GC cannot describe), the opposite
  direction: there a live reference was *lost*, here a dead one is *kept*. Its
  `CRATONVM_DBG=remap-residue` instrument is the nearest thing to the one this
  page wants.
