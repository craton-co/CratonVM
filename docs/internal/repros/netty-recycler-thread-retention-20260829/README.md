# Thread-retention probes for `RecyclerTest`

Three probes written to kill hypotheses about
`RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced`, which
times out for 4 of 6 parameterisations on CratonVM and 0 of 6 on HotSpot. See
`known-issues/netty/recyclertest-thread-not-collected-once-the-jit-warms-up-20260829.md`.

All three run the netty test's own liveness loop — `System.gc();
System.runFinalization(); Thread.sleep(50)` until a `finalize()` override fires
— so a row that says RETAINED means the same thing the test means.

**Every one of them came back negative, and that is why they are kept.** The
page's value is mostly in what it can rule out, and a hypothesis killed by a
control is worth more than one argued away.

## `ThreadCollectProbe` — is it Threads, or finalizers?

Seven arms: a plain object, an unstarted Thread, a started-and-joined Thread,
one that set a `ThreadLocal`, one whose allocation is still held from a static,
one that parked, one that took a monitor.

All seven collect on both VMs (52-70 ms). So finalizers run, and a dead Thread
mirror is not rooted by the VM's own thread bookkeeping. Run this FIRST — if it
had gone red the netty test would have been the wrong place to look.

## `UpdaterRetainProbe` — does a field updater root its target?

The failing set is exactly the GUARDED parameterisations, and netty's guarded
handle (`Recycler$DefaultHandle`) is the one carrying an
`AtomicIntegerFieldUpdater`; the unguarded handle has none. So "an updater
puts its target in a side table that is a GC root" explains the split exactly,
and this tree does keep object-keyed side tables that ARE scanned as roots.

Eight arms — `AtomicInteger`/`Long`/`ReferenceFieldUpdater`, a `VarHandle`, a
plain volatile write, and an untouched control. All collect on both VMs. The
theory is dead, and so is reading the guarded/unguarded split as a Recycler
property: it is where the JIT warm-up curve crossed.

## `RecyclerRetainProbe` — which link of the Recycler holds the Thread?

Rebuilds the test's shape across {NONE, PINNED, FAST_THREAD_LOCAL} x
{unguarded, guarded} with the thread body progressively stripped:

| variant | body |
|---|---|
| `bare` | no Recycler at all (control) |
| `noget` | Recycler created, `get()` never called |
| `nokeep` | `get()` called, object dropped |
| `nopin` | the test's body WITHOUT `Recycler.unpinOwner` |
| `full` | the test's body |

Every `full` cell collects on CratonVM. The only retaining cell is `nopin`, and
it retains on **both** VMs — correct behaviour, since a pinned Recycler is
supposed to hold its owner. Keep that column: it is the probe's positive
control, and without it a table of all-green cells would say nothing.

## `FrameRetainProbe` — is it the frame that HELD the reference?

The netty test nulls `thread` and loops IN THE SAME METHOD;
`RecyclerRetainProbe` looped in a callee. Three arms isolate that: loop inline
in the frame that held it, loop in a callee, and a looping frame that never had
it. All collect, under `BG_COMPILE=0`. So a compiled frame holding a nulled
local does not by itself retain.

## `RecyclerInlineProbe` — the cell neither of the other two covered

The netty body AND the inline loop, six owner/guard combinations, two rounds,
`BG_COMPILE=0`. All twelve collect. **The failure cannot be reproduced outside
JUnit**, which is what points the next investigation at JUnit's invocation
machinery rather than at netty or at the test's own frame.

## The lever the page actually turns

None of these. It is `CRATONVM_BG_COMPILE=0` against the six tests in
isolation: 0 failed becomes 6 failed on one binary.

---

## 2026-08-30 — CLOSED, and the section above is wrong

"**The failure cannot be reproduced outside JUnit**" was the conclusion these
probes supported, and it was an artefact of how they were run. See
`fixed-suite-bugs/netty/finalizers-never-run-once-the-collecting-loop-is-compiled-FIXED-20260830.md`.

The defect is one deferral in `run_finalizers` that never un-defers once the
loop asking for the collection is compiled, so **every object with a
`finalize()` override is immortal** — nothing to do with Threads, the Recycler,
or JUnit.

### Why these probes said "collects"

`ThreadCollectProbe` is structurally identical to `ThreadRetainMin.java`, which
reproduces. It passed because it was not run at the failing JIT dose, and it
*could not be*: every arm collects on the first iteration, so its loop never
gets hot, so it is never compiled, so the bug never appears. The probe's own
success prevented the compilation that causes the failure. Add
`CRATONVM_BG_COMPILE=0` and it fails — the same lever the page had already
identified as the one that turns the netty test.

Verified both ways on one binary: at the stock dose `ThreadRetainMin` collects
on all five arms; with `CRATONVM_BG_COMPILE=0` its three finalizable arms never
collect.

### The probes added here

* `ThreadRetainMin.java` (kept in `probes/` at the repo root, since it needs
  only a stock JDK) — five arms that isolate the ingredient. It is the
  `finalize()` override, not the Thread: a plain `new Object(){ finalize(){} }`
  retains, and a started+joined Thread WITHOUT the override collects.
* `ThreadFinalizeJUnitProbe.java` — netty's shape under JUnit plus a
  `WeakReference` witness. The test's assertion cannot tell "still reachable"
  from "collected but never finalized"; the weak ref can. It is genuine
  retention.
* `ThreadRetainMatrix.java` — the same body invoked five ways (plain `main`,
  `@Test`, `@Timeout`, `@ParameterizedTest`, and netty's exact combination).
  All five retain, which is what removed JUnit from the picture entirely.
