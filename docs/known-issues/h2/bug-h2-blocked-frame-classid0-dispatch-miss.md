# `NoSuchMethodError java/lang/Object.hasNext()Z` — a live iterator reads back as `ClassId(0)`

## Status
**OPEN, and for the first time reproducible on demand.** Updated 2026-08-03 on
`fix/h2-classid0-close-20260803`; originally filed 2026-08-02 against `dev` @
`750a95f8e3`.

Three things changed on 2026-08-03, and the third is the one to read first.

1. **The reproduction was impossible, and now is not.**
   `org.h2.test.db.TestMultiThread` — the only vehicle this page has — failed
   **100 % of runs in 2-9 s** on `origin/dev` @ `c3187d54b4`, against the ~450 s
   it needs to reach `testConcurrentUpdate` at all. Every rate quoted below,
   and every "0 in N runs" hunt, was measured against a class dying in its
   first four seconds. The cause had nothing to do with GC: an `invokevirtual`
   was bound to the compiled entry of the method its CONSTANT POOL resolved,
   with no receiver guard, so calls reached a base body instead of the
   override. Fixed in `12769bb23c`; see
   `../../internal/fixed-suite-bugs/jit-invokevirtual-bound-to-resolved-base-entry-FIXED.md`.
2. **The family reproduces at ~1 run in 6, on faces other than this page's
   name.** 18 runs post-unblock produced 3 events: `CloneNotSupportedException`
   twice and `[Lorg.h2.mvstore.Page$PageReference; cannot be cast to [J` once —
   and **zero** `NoSuchMethodError java/lang/Object.hasNext()Z`. Which bytecode
   touches a bad receiver first is incidental; chasing the named face is what
   made this look like 1-in-16.
3. **The clone face now has a verdict, and it says NOT RECLAIMED MEMORY.** The
   first occurrence to reach the new reporter:

   ```
   ERROR cratonvm::gc::guard: clone() dispatched to java.lang.Thread.clone,
     which only ever throws.
     obj="0x2001956f3e8" receiver_kind=Object receiver_class_id=29
   ```

   `receiver_kind=Object` — the caller was in `Arrays.copyOf(long[], int)`
   doing `original.clone()`, so the receiver must be an **array**, and the
   object at that address is an ordinary `java.lang.Thread`. And
   `report_reclaimed_receiver` printed **nothing** after it: the address is not
   in a free-list hole, not past the allocation frontier, not in the inactive
   semispace, and **neither reclamation ring has a covering record**.

   So for this occurrence the receiver is not a collected object read through a
   stale reference. It is a **live, unrelated object that the caller's
   reference should never have pointed at** — the same shape as the JIT defect
   fixed in (1), which is a wrong-object-from-a-virtual-call bug, not a GC bug.
   That does not prove the same root cause, but it does mean this page's
   premise — "a still-referenced object is read back as an all-zero header" —
   is **not established for the clone face**, and the GC framing should not be
   assumed for the others either until each has its own verdict.

Sibling page, same family, old-gen face:
`bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md`. It is **also
still open**: its own interior-conservative-root mechanism was found and fixed
this session, and its residual reproduced anyway on a block that was *not*
interior-rooted. Read it for the four things `ClassId(0)` can mean.

## Severity
**HIGH** — silent. A still-referenced object is read back as an all-zero header.
Here it surfaces as a linkage error on a `java.util.Iterator`; the same corrupt
read reaching a field access is a wrong value or a SIGSEGV with no Java frame.

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
     method="java/lang/Object.hasNext()Z"
     caller="org/h2/test/db/TestMultiThread.testConcurrentUpdate()V @pc=252"

CRATONVM_DBG_CCE_BT: site=nsme_dispatch method=java/lang/Object.hasNext()Z
  CCE-BT-STK[3] org/h2/test/db/TestMultiThread.testConcurrentUpdate pc=252
  CCE-BT-STK[2] org/h2/test/db/TestMultiThread.test pc=28
  CCE-BT-STK[1] org/h2/test/TestBase.testFromMain pc=11
  CCE-BT-STK[0] org/h2/test/db/TestMultiThread.main pc=9
  NSME-RECV addr=0x200625bf088 tid=0 blocked=false epoch=54
  NSME-RECV SHAPE kind=Object num_fields=0 mirror_of=<not a registered mirror>
```

`@pc=252` is the `for (Future<Void> job : jobs)` result loop
(`TestMultiThread.java:381`). The receiver is the synthetic `Iterator` local
javac emits for that loop — held only by `testConcurrentUpdate`'s frame while
the thread is parked inside `job.get(5, TimeUnit.MINUTES)`.

`num_fields=0` and a class of `java/lang/Object` are the ambiguous
`ClassId(0)` face. An `ArrayList$Itr` has fields; this one has none.

A second witness, same loop shape, different method, seen 2026-08-01 on the
pre-`750a95f8e3` tree:

```
NoSuchMethodError method="java/lang/Object.next()Ljava/lang/Object;"
     caller="org/h2/test/db/TestMultiThread.testConcurrentInsert()V @pc=197"
```

HotSpot emits neither, ever.

## What is NOT yet established

The all-zero header does **not** by itself say the young sweep did it. Per the
sibling page, `ClassId(0)` is also an un-hashed `new Object()`, an old-gen free
block whose bytes were re-zeroed by `OldGen::allocate`, and the zeroed tail of
an old-gen compaction. **Do not repeat the inference that page had to retract.**

Two facts that narrow it, both from the reproducing run:

* every young collection in that run took the **non-moving** fallback
  (`[moving-young] fallback #64: reason=compiled-frame-oop-not-published`, and
  the same reason at #4/#5/#6/#7/#16/#32). Nothing was relocated, so this is a
  *reclamation* gap, not a remap gap — which rules out one of the two
  mechanisms, not three of the four causes;
* the victim is reachable only from a Java frame local of a thread inside a
  blocking region, which is the `deposit_root_snapshot` path
  (`vm/src/vm/vm_exec.rs`), not the `collect_roots` path.

## What this session added: the verdict is now flag-free

`GenerationalHeap::reclaimed_hole_at` (added by the sibling investigation) asks
the heap whether an address is inside a free-list hole, past the allocation
frontier, or in the inactive semispace — none of which a live object can be.
It was wired into the interpreted `checkcast` path and the compiled `checkcast`
path, and **into nothing else**. A dispatch miss wears the identical face and
reported nothing, so the run above produced an ambiguous dump and no verdict.

`vm/src/memory/reclaim_guard.rs` now holds that verdict once, and the `invoke`
dispatch terminal in `vm_exec.rs` calls it — gated on `ClassId(0)`, because
that terminal is also reached in bulk on healthy runs (a missing method on a
synthetic classpath stub logs there on every call) and the probe takes the
young and old heap locks. `jit::helpers`' copy was migrated onto the same
function; the two `checkcast` copies had already drifted (only the
interpreter's consulted the young-sweep ring), and a third copy would have
compounded that.

The next occurrence prints which region the receiver is in, and — via the
unconditional old-gen reclamation ring — what the block held and which
collector freed it, **with no flag set in advance**. That is the one thing
neither this page nor its sibling could get from the run that reproduces.

## Reproducing

> **Superseded rates below.** Everything in this section was measured before
> `12769bb23c`, i.e. against a `TestMultiThread` that died in its first four
> seconds for a reason unrelated to this defect. The command is still correct;
> the "1 in 6" and "1 in 16" numbers are not. Use the 2026-08-03 campaign
> table further down, and count all four faces rather than the
> `NoSuchMethodError` one alone.

```bash
cd <fresh writable dir>          # H2 writes ./data
CRATONVM_DBG_CCE_BT=1 <cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Rate: **1 occurrence in 6 runs** — ~2 400 s of class runtime, 2026-08-02,
16-core Azure host at load 6-15. Budget several runs; it is nowhere near a
per-run certainty.

| run | flags | outcome | NSME |
| --- | --- | --- | --- |
| 1 | `DBG_CCE_BT` | pass, 727 s | **1** |
| 2 | `+DBG_REMAP_TRACE +DBG_ZERO_RANGES` | fail (`LOCK_TIMEOUT`), 348 s | 0 |
| 3 | same | pass, 428 s | 0 |
| 4 | same | pass, 420 s | 0 |
| 5 | none (guard binary) | pass, 430 s | 0 |
| 6 | none (guard binary) | pass, 445 s | 0 |

Note runs 2-4 carry `CRATONVM_DBG_REMAP_TRACE`, which maintains a per-object
ring and changes timing. It is NOT one of the flags that feed
`retain_dead_objects` (checked — those are `DBG_SWEEP_ZERO`, `DBG_A2`,
`DBG_SWEEP_CENSUS`, `DBG_WATCHREF`), so it does not swap the collector; but 0/3
under it against 1/1 without it is too small a sample to say it does not perturb
the race. Prefer an uninstrumented run now that the verdict is unconditional.

**Hunted again 2026-08-02 with the verdict in place: 0 in a further 10 runs**
(dev `5a18a9db1c`, ~1 hour of class runtime, no debug flags). So the rate is
1 in 16, not 1 in 6 — the first estimate was one event over a small sample and
should not be quoted as a rate. The verdict has therefore **not yet fired on a
real occurrence**; what is verified is that the new call site is live and safe
(a provoked `NoSuchMethodError` on a healthy receiver reaches it, takes no heap
locks, prints nothing, and the error is still thrown and caught exactly as
HotSpot does).

Those 10 runs needed
`CRATONVM_JIT_DENY=MVMap.evaluateMemoryForKey,MVMap.evaluateMemoryForValue`,
because dev tip at the time died in under 3 seconds with
`InternalError: … refusing side-effecting replay`. **That is fixed as of
2026-08-02 and the deny is no longer needed** — the optimizing tier was hoisting
`org.h2.util.MemoryEstimator.estimateMemory`'s `ldiv` (and its zero-divisor
trap) above the branch that guarantees a non-zero divisor, and the deopt frame
it stashed carried no method identity, so the failure was blamed on whichever
method happened to be on the JIT-dispatch boundary. See the retired
`unresumable-unconditional-trap-mvmap-20260802` write-up. The deny forced two
one-line methods to stay interpreted and was not otherwise load-bearing here.

**A cheaper handle on what is probably the same defect.** Two of those same ten
runs failed with `CloneNotSupportedException` on a `COMMIT` in
`testConcurrentUpdate`, at 77 s and 112 s. `Object.clone()` throws that when the
receiver's class is not `Cloneable` — and a zeroed header resolves to
`java.lang.Object`, which is not. If that is this family's third face it is
**20 % in under two minutes** against this page's 1-in-16, which makes it the
better thing to instrument first. `reclaim_guard` is not wired into the clone
path, so those two occurrences produced no verdict; wiring it there is the
cheap next step. Tracked on
`bug-h2-testmultithread-concurrent-update-timeout.md`.

`docs/internal/repros/h2-blocked-frame-roots/BlockedFrameRootProbe.java` is a
reduced driver — `main` parked in `Future.get()` over the same enhanced-for
while N workers allocate hard, with a canary object and the `jobs` list checked
after each round. **It does not reproduce** (40 rounds × 8 threads at
`--Xmx 64m`, 0 events). It is committed as a *negative* result so the next
person does not rebuild it: whatever the trigger is, a shallow interpreted main
frame parked in `get()` is not sufficient. The H2 run has deep JIT-compiled
frames on the parked thread — the fallback reason names exactly that
(`compiled-frame-oop-not-published`) — and that difference is the next thing to
put into a probe.

## 2026-08-03 campaign — what was measured, and what it eliminated

All on `fix/h2-classid0-close-20260803`, `--Xmx 1g`, 16-core Azure host, two
workers, no debug flags beyond `CRATONVM_DBG=cce-bt`.

| phase | binary | runs | family events | other |
| --- | --- | --- | --- | --- |
| 1 | `12769bb23c` (JIT fix, no clone verdict) | 9 | 2 — `CloneNotSupportedException` ×4 in one run, `ClassCastException` ×12 in another | 1 `TimeoutException`, 6 clean |
| 2 | `583021945b` (+ clone verdict) | 9 | 1 — `CloneNotSupportedException` ×12, **with a verdict** | 1 `TimeoutException`, 7 clean |

`TimeoutException` is the separate throughput defect tracked on
`bug-h2-testmultithread-concurrent-update-timeout.md`, not this one.

### Eliminated, with measurements

* **The per-bci live-local mask is not the gap.** The synthetic enhanced-for
  `Iterator` local is read only across the loop's BACK EDGE from after the
  blocking call, so an analysis that did not reach a fixpoint over that edge
  would call it dead exactly where the thread parks — and that mask is what
  `Frame::scan_local_objects` filters the blocked-thread root snapshot with.
  `local_liveness::tests::enhanced_for_iterator_is_live_at_the_blocking_call`
  models the real method's bytecode (loop head 7, `Future.get` at 37, the whole
  range inside a `try` whose handler never reads the slot) and asserts slot 9
  live at pc 37/42/43. It **passes**, and it is differential: slot 10 (`job`)
  is correctly dead at pc 42, so the analysis is doing real work rather than
  returning `ALL_LIVE`.
* **The young sweep is not dropping a published root.** The new
  root-in-dead-span invariant compares `roots` + `finalizer_addrs` — the exact
  set the mark phase was handed — against every span the sweep is about to
  zero, and RETAINS any span a live (non-forwarded) root points into. It
  measured **zero** across the whole campaign (`rootdead=0` on all 18 runs).
  That is an elimination, not a silence: the check runs unconditionally and
  prints when it fires.
* **The blocked-frame slot audit found nothing** (`audit=0` on all 18 runs).
  It checks every live local and stack slot of every frame at blocked-region
  entry and at wake for `class_id == 0 && kind == Object`, and asks the heap
  whether such an address is in a reclaimed hole.

### Instrumentation added (all unconditional)

* `audit_frames_for_reclaimed_slots`, filtered by the collector's OWN liveness
  mask — a *dead* local pointing into a reclaimed span is the filter working as
  designed — and gated on `class_id == 0 && kind == Object`, because a
  primitive array header also carries class id 0 and without the kind test
  every `long[]` local flags on every wake. Cost is bounded to one frame walk
  per COLLECTION per thread rather than one per blocking call (`f2a19c30af`).
* A lock-free **young-span reclamation ring**, one record per COALESCED span.
  The pre-existing `record_swept` is gated on `CRATONVM_DBG_SWEEP_ZERO`, which
  also swaps the young collector off its parallel sweep prefix — the instrument
  changed the thing it measured, which is this family's entire "reproduces
  plain, never instrumented" history.
* The **clone-face verdict** described in *Status* (`583021945b`).

## What to try next

1. **Find where the reference goes wrong, not where it is read.** The verdict
   says the clone-face receiver is a live `java.lang.Thread` at an address the
   caller's `long[]` reference should never hold. Work backwards from
   `BitSetHelper.flip` → `Arrays.copyOf(long[], int)`: which load produced it?
   `CRATONVM_JIT_BISECT_ONLY` narrowed the sibling JIT defect to two classes in
   about ten runs and is the tool for this too.
2. **Re-take anything measured before `12769bb23c`.** A wrong-object return
   from a virtual call is not distinguishable, at the reader end, from a stale
   reference, and that defect was live for this family's whole history.
3. **Count all four faces**, not the one in this page's title —
   `NoSuchMethodError` on an `Object` receiver, `CloneNotSupportedException`,
   `cannot be cast`, and `SIGSEGV`. The family is ~4× more visible that way.
4. **Do not re-derive the eliminations above.** Each cost a build-and-soak
   cycle and each is recorded with its measurement.

## Related

* `bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md` — same family,
  old-gen face, has the `reclaimed_hole_at` verdict and the reclamation ring.
* `../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`
  — the same all-zero receiver reaching `Thread.clone`.
* `bug-h2-testmultithread-concurrent-update-timeout.md` — the class this was
  found in, whose own problem is throughput, not this.
* `../../internal/fixed-suite-bugs/jit-invokevirtual-bound-to-resolved-base-entry-FIXED.md`
  — the JIT miscompile that made this page's reproduction impossible, and the
  reason a wrong-object return has to be excluded before a stale reference is
  assumed.
