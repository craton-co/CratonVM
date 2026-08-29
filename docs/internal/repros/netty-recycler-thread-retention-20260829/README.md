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

## The lever the page actually turns

None of these. It is `CRATONVM_BG_COMPILE=0` against the six tests in
isolation: 0 failed becomes 6 failed on one binary.
