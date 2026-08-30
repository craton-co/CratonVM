# `finalize()` never runs once the loop asking for the collection is compiled — FIXED 2026-08-30

Retires `known-issues/netty/recyclertest-thread-not-collected-once-the-jit-warms-up-20260829.md`.

## What it actually was

One line in `run_finalizers` (`vm/src/runtime/interpreter/gc_and_alloc.rs`):

```rust
if crate::jit::helpers::is_jit_thread_set() {
    return;   // "Defer to the next top-level safepoint"
}
```

The guard is real — a `finalize()` invoked while a JIT helper holds the
`&mut JvmThread` could re-enter the JIT and alias the borrow. The deferral is
not. **For an application that asks for collection from inside a compiled loop,
the next top-level safepoint never comes**: every `System.gc()` arrives through
a JIT helper, `is_jit_thread_set()` is true at every call, and the drain is
deferred for the life of the process.

So the queue fills and is never drained. `finalizable_roots` re-adds the pending
queue as GC roots each cycle — correctly, to keep an object alive for a
finalizer that is about to run — and the result is that **every object with a
`finalize()` override becomes immortal.**

`CRATONVM_DBG_FINCAND=1` (added by this change) shows it directly. The objects
are found dead and resurrected on *every* cycle:

```
[fincand] unregistered=0 marked_alive=69 dead_resurrected=1     x30
[fincand] unregistered=0 marked_alive=69 dead_resurrected=2     x30
[fincand] unregistered=0 marked_alive=69 dead_resurrected=3     x31
```

Under `--nojit` the same probe resurrects once and stops.

## The fix

`run_finalizers_forced`, called from `force_gc_from_native` — exactly what
`run_cleaner_actions_forced` already did for the sibling queue, one function
away in the same file, with the same one-line justification ("deferring here is
not a deferral"). It re-installs the JIT thread pointer for the nested Java call
under RAII, so an unwinding `finalize()` cannot leave the outer level
un-restored, and the re-entrancy the guard protects against cannot happen.

The allocation-triggered callers keep the guarded variant: they really will be
back within an allocation or two.

`CRATONVM_FORCED_FINALIZERS=0` reverts, so both arms are one binary.

## Measured

`probes/ThreadRetainMin.java` (new, plain JDK — no netty, no JUnit), rounds to
collect, `CRATONVM_BG_COMPILE=0`:

| arm | fix on | reverted | real HotSpot |
|---|---|---|---|
| Thread + `finalize()`, started | **2** | never (43+) | 2 |
| Thread, no `finalize()` | 1 | 1 | 2 |
| Thread + `finalize()`, never started | **2** | never (45+) | 1 |
| plain `Object` + `finalize()` | **2** | never (43+) | 1 |
| plain object, no finalizer | 1 | 1 | 1 |

`io.netty.util.RecyclerTest`:

| arm | fix on | reverted |
|---|---|---|
| the 6 parameterisations, `BG_COMPILE=0` | **ok=6 failed=0**, 3.5 s | ok=0 failed=6, 31.6 s |
| whole class, default | **ok=59 failed=0** aborted=8, 6.0 s | ok=55 failed=4 aborted=8, 25.4 s |

`aborted=8` is unchanged in both arms — pre-existing assumption skips, not
failures.

## Three things the old page had wrong, and why

The page was careful and its negative results were sound; what it got wrong is
worth recording because each error has a reusable shape.

### 1. "It does not reproduce outside JUnit, and that is the sharpest thing known"

It reproduces in a plain `main` with no JUnit, no netty and no `Recycler`. The
minimal shape is an object with a `finalize()` override.

The page's own `ThreadCollectProbe` is structurally identical to the probe that
does reproduce — same `System.gc(); System.runFinalization(); Thread.sleep(50)`
loop, same `finalize()` flag. It passed because **it was not run at the failing
JIT dose**, and it could not be: every arm collects on the first iteration, so
the loop never gets hot, so it is never compiled, so the bug never appears.
*The probe's own success prevented the compilation that causes the failure.*
`CRATONVM_BG_COMPILE=0` breaks the circularity by compiling on first entry —
which is also why that flag took netty's test from 4 failures to 6, a
dose-response the page recorded and read as evidence for root scanning.

**Shape: a probe has to carry the same JIT dose as the failing arm, and a
self-defeating probe reports a clean bill of health.**

### 2. "The precision levers do not move it, which is the part that still needs explaining"

Both levers were vacuous, and neither null was evidence of anything:

* `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` suppresses the conservative JIT scan only
  when a coverage proof passes, and **its own doc comment records that firing on
  ~0.1 % of collections** (2 of 14 420, 31 of 46 135, 84 of 70 144). The other
  999-in-1000 collections scanned conservatively anyway.
* `CRATONVM_NO_CONSERVATIVE_LOCALS` gates the INTERPRETER's local scan, and
  `conservative_locals_enabled()` additionally requires
  `flags().natives.real_forkjoinpool`, which is **off by default** — so it
  disabled something that was never on.

**Shape: read what a lever gates before reading its null. Both of these are the
"zero from an instrument armed where it cannot fire" pattern.**

### 3. The suggested next instrument would have found nothing

The page proposed walking the collecting thread's JIT frames at the
`System.gc()` safepoint for words still pointing at the Thread mirror, naming a
never-spilled callee-saved register as the expected shape.

`CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` (added by this change) suppresses the
conservative JIT frame scan **unconditionally**, at the one place all three
doors meet (`memory::roots::collect_roots`, `vm_exec`'s safepoint deposit, and
`interpreter::gc_and_alloc`'s blocked-deposit — gating only the first is a lever
that reads as "no effect" while never having been applied). It logs when it
engages, so the arm cannot be misread as a null.

Result: **still 6 of 6**, with `engaged=1`. So were both cross-thread peer
scanners off (`CRATONVM_XT_JIT_ROOT_SCAN=0`,
`CRATONVM_XT_HELPER_WINDOW_SCAN=0`), and all of them together. The retaining
reference was never on any stack; the object was a GC root because the finalizer
queue is a root and the queue was never drained.

## What the old page got right

Its four "what it is NOT" sections all stand, and all of them were necessary:
the field updater, the Recycler's reference graph, the inline-splice family, and
dead Thread mirrors are none of them involved. The dose-response table is
correct and is now explained. `FrameRetainProbe`'s finding — that a compiled
frame holding a nulled local does not on its own retain — was true and pointed
away from the wrong answer.

The one premise never tested was the title's: that the Thread *stays reachable*.
A `WeakReference` alongside the `finalize()` flag separates "still reachable"
from "collected but never finalized", and the test's assertion cannot. It turned
out to be genuine retention — but for the opposite reason to the one assumed,
and the witness is what made the difference readable.

## Why it matters beyond this test

The old page's "Why it matters beyond this test" understated it. This was not a
Thread-shaped defect and not a netty-shaped one:

* every `finalize()` override in the process, in any application, once the
  collecting loop is compiled — which is to say, in any long-running one;
* the objects are not merely un-finalized, they are **unreclaimable**, and they
  are resurrected and re-scanned on every subsequent collection, so the cost
  grows with uptime.

`Cleaner`, `WeakReference` and `PhantomReference` are unaffected — the cleaner
queue already had its forced drain, which is precisely the fix this one was
missing.

## Repro

```bash
# minimal, no netty and no JUnit -- rounds=2 is correct, "never" is the bug
javac -d . probes/ThreadRetainMin.java
cratonvm --java-home <jdk25> -Dsecs=3 -cp . ThreadRetainMin                     # fixed
CRATONVM_FORCED_FINALIZERS=0 CRATONVM_BG_COMPILE=0 cratonvm ... ThreadRetainMin  # the bug

# the classification counters
CRATONVM_DBG_FINCAND=1 CRATONVM_BG_COMPILE=0 cratonvm ... ThreadRetainMin

# netty
cd apps/netty-suite-runner
cratonvm --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.RecyclerTest
```

`CRATONVM_BG_COMPILE=0` is not optional in the reverted arm: at the stock dose
the minimal probe collects, for the self-defeating reason in §1.

## Related

* `repros/netty-recycler-thread-retention-20260829/` — the original probes.
  `ThreadRetainMin.java` and the two JUnit probes that ruled JUnit out
  (`ThreadFinalizeJUnitProbe`, `ThreadRetainMatrix`) are added there.
* `fixed-suite-bugs/netty/longlonghashmaptest-npe-spliced-ctor-this-not-a-gc-root-FIXED-20260828.md`
  — the family the old page reasonably suspected, and which this was not.
