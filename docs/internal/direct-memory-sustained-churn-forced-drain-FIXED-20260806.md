# Direct memory exhausts under sustained multi-threaded churn — the forced drain was refused

## Status
**FIXED 2026-08-06.** Retired from `docs/known-issues/`.

`org.h2.test.store.TestMVStore` now runs to completion under `--Xmx 1g` on the
Azure Linux host, JDK 25:

| build | `Direct buffer memory` | `TestMVStore` |
| --- | --- | --- |
| `dev` @ `801c89115` | 4 | dies at `1014/4500` (23 min) |
| + drain on ordinary GC (fix 1) | 4 | dies at `2196/4500` (45 min) |
| + **forced drain runs (fix 2)** | **0** | **`4515/4500`, `rc=0`** (2 h 19 m) |

At the four points where the cap was reached, all four reservations were
granted after a single collection:

```
[dm] reserveMemory round=0 before=1068998656 after=8257536 freed=1060741120
[dm] reserveMemory GRANTED after round=0
```

`probes/DirectBufProbe.java` still matches HotSpot exactly (3200 MiB, with and
without `--nojit`).

Note this class was previously believed unable to pass on either VM — the older
write-up recorded stock HotSpot failing it earlier at `testCacheSize`. On this
host, with this JDK, CratonVM now completes it.

## What it was

Two defects, one behind the other, both producing the identical end-to-end
symptom. A third hypothesis was disproven.

### 1. The drain had one call site — the backlog

`run_cleaner_actions` was called only from `force_gc_from_native`, i.e. only on
an explicit `System.gc()` or on `Bits.reserveMemory`'s reclaim-and-retry, while
`run_finalizers` was called from the ordinary allocation-triggered GC paths as
well. Reference processing emitted cleaner actions on *every* collection and
nothing ran them:

```
[dm] refproc round=40 emitted=2 cum_emitted=1018 discovered=1162 ...
[dm] run_cleaner_actions DRAIN empty x1 (calls=2 jit_blocked=0 cum_drained=992)
```

1018 actions emitted; `run_cleaner_actions` entered **twice in seven minutes**.
Each queued action holds a direct buffer's reservation, so the backlog *is*
reserved memory that nothing will give back.

Fixed by giving it the same two `maybe_gc` call sites `run_finalizers` has.
Alone this roughly doubled how far the class got (`1014` → `2196`) without
curing it.

### 2. The forced drain bailed under a JIT borrow — the failure

`run_cleaner_actions` returns early on `crate::jit::helpers::is_jit_thread_set()`,
leaving actions queued for "the next top-level (non-JIT) safepoint". On the path
that matters there is no next safepoint: H2 allocates its direct buffers on a
`FileStore` writer-pool worker running compiled code, so the guard held **every
time**, and `Bits.reserveMemory` throws as soon as the reclaim returns empty:

```
[dm] reserveMemory REFUSED size=9773056 reserved=1072705536 max=1073741824 thread=ThreadId(583)
[dm] run_cleaner_actions BLOCKED jit_thread_set: blocked=15 pending=156 calls=46 thread=ThreadId(583)
[dm] reserveMemory round=0 before=1072705536 after=1072705536 freed=0
[dm] reserveMemory GIVING UP
```

156 reclaimable buffers, refused.

## The fix, and why it is scoped the way it is

The guard is not wrong — it protects against a genuine aliasing `&mut JvmThread`
fabrication and was added for a real avrora `FileCleanable` SEGV. It was
therefore **not** relaxed globally. The drain is split in two:

* `run_cleaner_actions` — unchanged, still defers. This is what `maybe_gc`
  calls, and `maybe_gc` is reachable via `jit_invoke_dispatch` →
  `bail_to_interpreter` → interpreter, where the interpreter's `&mut JvmThread`
  is itself fabricated from the JIT TLS. A cleaner's `run()` re-entering JIT
  there would be a genuine *sibling* aliasing borrow. That is the avrora shape;
  it is left alone.

* `run_cleaner_actions_forced` — called only from `force_gc_from_native`, i.e.
  from inside a native method invocation, where `thread` is the native
  dispatcher's own legitimate `&mut JvmThread`. It opens a nested scope with
  `set_jit_thread`/`restore_jit_thread`, making the cleaner's `run()` a *child
  reborrow* rather than an aliasing sibling — the same boundary the interpreter
  already crosses to re-enter JIT from a bail. An RAII guard restores the outer
  level even if an action unwinds.

Deferring on the forced path was never a deferral: its two callers
(`System.gc()` and `Bits.reserveMemory`) have no later safepoint to wait for,
and the second throws `OutOfMemoryError` the moment it returns.

**Not validated against avrora.** The guard's original crash was a DaCapo
`avrora` run, and DaCapo is not present on this host, so the untouched
`maybe_gc` path is argued-safe rather than re-measured. That is the reason the
change is scoped to the forced path instead of removing the guard.

## Disproven — worth not re-testing

* **The buffers were NOT still reachable.** This was the third lead on the
  original page. The instrument counts `runs_cleaner` phantoms whose referent is
  still live; it sat between 7 and 154 across whole runs and was flat at
  ~144–147 at every failure. The buffers were dead and waiting on a drain, not
  retained by H2's pending write futures.

* **A JDK-style exponential back-off in `Bits.reserveMemory` does nothing here.**
  Implemented (`MAX_SLEEPS = 9`, 1…256 ms, sleeping inside a blocking region so
  a concurrent STW could still proceed), built, measured: it failed
  *identically* to fix 1 alone. Once the drain actually runs, the back-off is
  never reached — `TestMVStore` granted 4/4 at `round=0` and an 8-thread churn
  probe 5/5 at `round=0`. Reverted rather than landed: an unexercised sleep path
  in GC/STW-adjacent code costs up to 511 ms and three extra full collections on
  the OOM path and buys nothing measurable. Reinstate it only alongside a
  workload that actually reaches it.

## Verification

* `org.h2.test.store.TestMVStore` — `4515/4500`, `rc=0`, zero
  `Direct buffer memory` (was 4).
* `probes/DirectBufProbe.java` — 3200 MiB, matches HotSpot, with and without
  `--nojit`.
* `probes/DirectBufChurnProbe.java` (**new**) — 8 threads × 16 MiB × 40 rounds.
  5120 MiB on CratonVM and on stock HotSpot. Note this passes on the *unfixed*
  binary too, so it is a multi-threaded **regression check, not a reproducer**.
* `cargo test --release`: `cratonvm-gc` 971 passed, `cratonvm-native-io` 387
  passed, `cratonvm-vm` 2404 passed. 0 failures.
* The `STW cross-thread JIT takeover is still waiting` warning seen during the
  churn probe is **pre-existing** — a control run on the unfixed binary produced
  5 of them against 2 on the fixed one.

## The instrument

`CRATONVM_DBG_DM=1` traces every stage of the chain in one run — Cleaner
discovery, action emission, live-referent count, the drain (with
`jit_thread_set` and the pending backlog), and each `Bits.reserveMemory` round
with bytes freed. Cached `OnceLock` gate, so it is one bool read when unset; it
is deliberately **not** a per-call `runtime_var_os`, which would take the
process-env lock on the post-GC path.

Kept, because it is what separated the two defects — they produce the same
end-to-end symptom, and only a per-stage counter distinguishes a partial fix
from a wrong one.

One caveat learned the hard way: the `BLOCKED` line was originally rate-limited
to every 200th occurrence and was therefore **silent at the exact moment it
mattered**, which made a run look like the drain had executed and found nothing.
It now always prints when the backlog is non-empty. A sampling diagnostic can
hide the single event you are hunting.

## Related

* the retired `direct-bytebuffers-are-never-reclaimed-20260805` page — the
  parent bug (Cleaners were discovered but never run at all);
* the retired `h2-testindex-testmvstore-unmasked-20260802` page — why
  `TestMVStore` got far enough to hit this.

## Merge addendum — a concurrent session landed two of these independently

While this was being measured, `claude/bytebuffer-jdk-contract-e3e6c6` landed on
`dev`:

* **the same fix 1** (`run_cleaner_actions` beside both ordinary-GC
  `run_finalizers` calls in `maybe_gc`) — the merge conflict was comment-only
  and dev's comment was kept;
* **a third gap this page's original leads did not name**: the
  `Unsafe.allocateMemory0` path that the real `DirectByteBuffer(int)`
  constructor uses had no reclaim-and-retry at all, and
  `dbb_allocate_collecting` now gives it one.

Fix 2 here (the forced drain under a JIT borrow) is independent of both and was
not landed by that session.

### One correction to the record

That page states our `bits_reserve_memory` is dead code in real-JDK mode,
because `java/nio/Bits` is not in `force_native_over_real_jdk_bytecode`, and
concludes its retry "never ran". **Measurement contradicts that.** The
`[dm] reserveMemory …` traces exist only inside `bits_reserve_memory`, and they
fire on `TestMVStore` at the fork point `801c89115` — *before* that session's
changes:

```
[dm] reserveMemory REFUSED size=9777152 reserved=1065761792 max=1073741824 thread=ThreadId(609)
[dm] reserveMemory round=0 before=1065761792 after=1065761792 freed=0
[dm] reserveMemory GIVING UP size=9777152 reserved=1065761792
```

and after fix 2 the same function is what grants them:

```
[dm] reserveMemory round=0 before=1068998656 after=8257536 freed=1060741120
[dm] reserveMemory GRANTED after round=0
```

So `bits_reserve_memory` is reached on this workload and its retry is
load-bearing. The `Unsafe.allocateMemory0` path may well *also* have needed its
own retry — that is a separate, plausible gap — but "our native is dead code"
is not the reason this page stayed open.

### Combination re-verified

`fix+fix` is not a proven fix, so `TestMVStore` was re-run on the **merged**
tree rather than on either branch alone. See the verification section above for
the merged numbers.
