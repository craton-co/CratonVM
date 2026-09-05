# `TestCachedQueryResults` — a ZGC `OutOfMemoryError` LIVELOCK, thousands per run, not a single failure


## RESOLVED 2026-09-04 -- two complementary fixes, and the crash was never a GC root

| configuration | SIGSEGV | ref-array OOM | completes | `actual` |
|---|---|---|---|---|
| credit, no arena fix | 2-3 of 4 | 0 | when it survives | 99 96x |
| arena fix, no credit | 0 | **6264** | **no** | -- |
| **both** | **0 of 5** | **0** | **yes, 555-728 s** | **99953-99978** |

Against this page's opening state: `98304` with **1497** ref-array
`OutOfMemoryError`s in ~1519 s. Regression suite 88/88. Compaction fully intact
(25 cycles, 545893 objects relocated).

**Two independent defects, one symptom each.**

1. **The OOM** is what this page is about, and the repair is the ZGC pinned-peer
   credit plus the blocked-peer shadow-stack scan: a blocked peer's coverage can
   be discharged by pinning, so relocation is no longer refused on nearly every
   cycle. `CRATONVM_XT_PINNED_PEER_DEPTH=1` + `CRATONVM_XT_PEER_SHADOW_SCAN=1`.

2. **The SIGSEGV** that discharge exposed is a separate, older collector bug and
   has nothing to do with JIT roots: `relocate_stw`'s slide writes into a
   granule the arena DECOMMITTED, because the destination search screens by page
   and liveness and never by commit state. `Arena::commit_for_relocation`
   fixes it (dev's name for what this branch called
   `ensure_committed_span`). Full evidence on the retired
   `bug-box-unbox-intrinsic-segv-under-relocation-20260902` write-up and the
   `zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904`
   record it points at.

The credit never corrupted anything. It raises compaction, compaction runs
slides, and slides are what land in a decommitted granule -- which is why the
crash tracked the credit so convincingly, and why seven repairs aimed at stale
references in compiled frames all changed nothing.

**Neither fix alone retires this page.** Without the arena fix the class
crashes; without the credit it logs 6264 OOMs and never finishes.

## ADDENDUM 2026-08-30 (L7 corpus lane): the shortfall accounts EXACTLY, and three alternatives are eliminated

The `--jdk-only` corpus hit this class, so it got the three arms. Both CratonVM
modes fail with the *same* assertion, HotSpot passes:

```text
HotSpot          PASS     9s
CratonVM compat  FAIL  1078s   AssertionError: Expected: 100000 actual: 98304
CratonVM strict  FAIL  1365s   AssertionError: Expected: 100000 actual: 98304
```

### The 1696 missing entries are accounted for, to the unit

```text
100000 - 98304                                    = 1696
OutOfMemoryError (length=65536) raised in tasks   = 1691
SQLException caught by the callable and printed   =    5
                                                    ----
                                                    1696
```

**That is why this page's OOM framing is right, and it also explains the thing
an OOM does not obviously explain — why the symptom is a WRONG ANSWER instead of
a crash.** The callable catches `SQLException` only. An `OutOfMemoryError` is an
`Error`, so it goes straight past that `catch`, is captured by the `FutureTask`
`invokeAll` created for it, and **the test never calls `get()` on any of the
futures it gets back**. 1691 tasks therefore die completely silently, each one
simply never reaching `concurrentSet.add(countAfter)`, and the only trace is the
final count.

### Three things it is NOT, each checked rather than assumed

* **Not `ConcurrentHashMap.newKeySet()` losing entries.**
  `apps/probes/ChmKeySetGrowth.java` — 5 threads, 100000 distinct adds, the same
  shape the test uses — reports `adds-returned-true 100000`, `size 100000`,
  `contains-misses 0` on CratonVM. Identical to HotSpot.
* **Not `ExecutorService.invokeAll` dropping tasks**, which was a live suspicion
  because `docs/known-issues/` records an `invokeAll` that copied 3 of 8 on a
  ForkJoinTask arm. `apps/probes/InvokeAllCount.java` submits 100000 callables
  through `invokeAll` on a 5-thread pool: `futures 100000`, `done 100000`,
  `executed 100000`. Identical to HotSpot.
* **Not a lost update, and not this VM mishandling `FOR UPDATE`** — which the
  assertion's own shape suggests, since the set holds distinct COUNTER VALUES
  and `add` returning false is the test's lost-update detector. The run printed
  **zero** `LOST UPDATE!` lines and **zero** `countAfter != countAtLock` lines.
  `TestBase.println` is NOT gated behind a verbosity flag — it goes straight to
  `System.out` — so those absences are evidence rather than silence. Every task
  that reached the lock saw a value no other task had seen.

### One thing retracted

`98304 == 131072 - (131072 >>> 2)` is exactly `ConcurrentHashMap`'s resize
threshold for a 131072-bucket table, and the same number appearing in two
independent runs in two modes made a deterministic growth failure look likely.
Both probes above refute it. The resemblance is a coincidence, and it is
recorded here so the next reader does not spend the same hour on it.

## ADDENDUM 2026-08-30 (b): THE LEAD THIS PAGE SHIPPED WITH IS REFUTED, and four real defects were fixed on the way to refuting it

Read this section before any other. The page's headline lead --
`xt_cov=(accepted=0 refused=1730)`, "attack the refusal before attacking the
allocator" -- is **not what gates this class**, and that is now measured rather
than argued.

### 1. The refutation, in one table

`CRATONVM_XT_JIT_COVERAGE_ASSUME=1` (added for this, and documented as unsafe)
makes the peer accounting accept whatever the ledger says. One binary,
`--Xmx 1g`, 1800 s cap, idle host:

| arm | `xt_cov` | rc | secs | `arena` | OOM | assertion |
|---|---|---:|---:|---:|---:|---|
| default | `accepted=0 refused=1731` | 1 | 641 | 11 | 1696 | 100000 vs **98304** |
| assume | `accepted=1731 refused=0` | 1 | 636 | 11 | 1696 | 100000 vs **98304** |

Byte for byte the same failure. Taking the handshake from "refuses every cycle"
to "accepts every cycle" changed nothing at all.

**And the reason it changed nothing is itself the lesson, so read this counter
before believing either row**: `relocation_on_proven_jit=0` and
`relocation_skipped_jit=1731` in BOTH arms. Accepting the handshake removes ONE
contribution to `moving_young_coverage_incomplete()`; the cycle had others, so
relocation never ran in either arm. A run whose `relocation_on_proven_jit` is
zero cannot say anything about compaction.

### 2. What actually refuses, counted

Same binary, 300 s, `CRATONVM_DBG_JIT_ROOTSCAN=1`, by `incomplete_reason` label:

| reason | count |
|---|---:|
| `xt-helper-window-conservative-scan` | **219** |
| `compiled-frame-oop-not-published` | 5 |
| `unregistered-jit-frame-on-stack` | 3 |

`helper_windows=15446` on the run's `[GC] xt_peer_scan` line. **A peer caught
inside a JIT helper is the dominant refusal by two orders of magnitude.** That
is a different obligation from the one this page chased: the peer's COMPILED
frames are registered and provable, but the helper's own Rust frame holds
`ObjectRef`s in Rust locals, which the conservative scan can mark and cannot
rewrite. Discharging it is a PINNING question, not a proof repair.

`unregistered-jit-frame-on-stack` is third and should NOT be attacked as stack
residue: `CRATONVM_DBG_A5_CENSUS=1` reports `hits=2 shaped=2`, so the hits carry
real frame shape and the existing shape filter would not convert them.

### 3. What the failing allocation is -- and the retraction is itself retracted

The page never said WHICH request fails. It is one shape, 1696 times:

```text
OutOfMemoryError { message: "Java heap space (native reference array of length 65536)" }
    at org/h2/test/jdbc/TestCachedQueryResults.lambda$test$0
```

A reference array of length 65536 -- 524 304 bytes. The collector's own
`zgc frag:` diagnostic, which was in the log all along, gives the arena state:

```text
request=524304 spans=219425 largest_span=246704 free_bytes=864745648 walls=219424
zgc frag: the CHEAPEST window that could serve this request - 34288 live bytes
    in 13 run(s) are all that stand between 490112 free bytes spread over
    524400 bytes of contiguous arena
    window_bytes=524400 window_free=490112 wall_bytes=34288 walls=13
```

**865 MB free in 219 425 spans, largest 241 KiB, against a 512 KiB request**,
and the cheapest window that would serve it is walled by **13 runs totalling
34 KB** on a heap that is 96 % free (`free_permille_at_worst=962`).

**This un-retracts the retraction.** The page retracted
`98304 == 131072 - (131072 >>> 2)` as "a coincidence". It is not: 98304 is where
`ConcurrentHashMap` needs its next table, that table IS a 65536-slot reference
array, and that array is the allocation that fails. `ChmKeySetGrowth` passes
because it never builds this arena state -- the probe was right about
`ConcurrentHashMap` and wrong about what the number meant.

### 4. Four defects fixed on the way, all measured, none of them the cause

All four are real, all four are one-binary A/Bs, and together they take the
per-frame side of the coverage proof to **perfect**.

**(a) The IR backend's safepoint POLL recorded no id and no map.**
`emit_safepoint_poll` emitted a bare `TEST`/`JZ`/`CALL`. A thread parked in the
slow path left its sp-id slot holding the prologue sentinel or the id of an
EARLIER safepoint. The single-pass backend has always bracketed its poll
(spill, sp-id, call, map); this is that bracketing, emitted inside the taken
branch so the fast path is byte-identical.

**(b) `Op::New` recorded no safepoint map.** Excluded on the reasoning above
the `Op::NewArray` arm -- "unlike `Op::New`'s arm, which has no operand of its
own to protect" -- which reads the map as protection for the NODE. It is not:
it describes every live reference in the FRAME. The last surviving
`no-map-for-id` frame after (a) was `java/util/ArrayList.iterator()`, a method
whose whole body is `new Itr(this)`, reading `sp_id=0` against
`maps=2 ids=[1, 2]`. It was parked in the allocation stub.

| `CRATONVM_JIT_IR_GC_POINT_MAPS` | `no_map` | `no-map-for-id` lines |
|---|---:|---:|
| off | 11 | 18 |
| poll only | 2 | 2 |
| poll + `Op::New` | **0** | **0** |

**(c) The single-pass prologue established no sp-id sentinel** -- the caveat
section 4a explicitly deferred. Its ids are bytecode pcs and bci 0 is legal, so
`0` cannot serve there; `SP_ID_UNSET_BC_PC` (`u32::MAX - 1`) can. This closes
the half that is not loud: a dense id space means an uninitialised slot can read
as a VALID id and be relocated against the wrong program point's map.

**(d) A direct call's staged argument oops were declared unmappable.**
`reserve_direct_call_service_slots` already copies every argument into a
contiguous frame range; the sibling dispatch-helper site NAMES its equivalent
buffer, the two direct sites set `pending_staged_args_unmapped` instead. This
was the whole remaining `map_incomplete` population -- 121x the next cause:

| `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS` | `no_map` | `incomplete` | `ok` | `staged_unmappable` |
|---|---:|---:|---:|---:|
| 0 | 0 | 93 | 408 | 8989 |
| 1 | 0 | **0** | 601 | **74** |

After all four, a 1800 s run reports
`frame_cov=(no_slot=0 misaligned=0 no_map=0 incomplete=0 ok=994)` and **zero**
band words. Every per-frame coverage proof in the run succeeds. The class still
fails, in 641 s instead of 1078 s.

### 5. What to do next, in order (replaces the older list)

1. **Price the window, not the proof.** 13 runs / 34 KB wall a 524 KB window on
   a 96 %-free heap, and `zgc-high-compaction: cycles=0 declined=0
   objects_relocated=0` says the targeted compactor engaged **zero** times. Ask
   what those 13 runs are and why nothing is asked to move them.
2. ~~**Then the helper window**, 219 of 227 refusals.~~ **DONE 2026-09-01** --
   discharged by pinning the peer's conservative roots rather than refusing the
   cycle; see the 2026-09-01 addendum. Whether it makes THIS class compact is
   still unmeasured (it needs a quiet host).
3. `compiled-frame-oop-not-published` (5) and `unregistered-jit-frame-on-stack`
   (3) are not worth attacking until 1 and 2 are answered.
4. `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` settles "is the handshake the gate?" in
   one run. It is unsafe; read `relocation_on_proven_jit` before believing
   anything it produces.

## ADDENDUM 2026-09-01: the dominant refusal is NOT discharged -- read the correction below before the claim above

The 2026-08-30 (b) census named `xt-helper-window-conservative-scan` as **219 of
227** refusals. It is now gone, and the repair is not a proof at all -- it is a
root set that had nowhere to go.

### What a helper window actually is

A peer thread whose `Rip` is OUTSIDE compiled code but which has JIT frames on
its native stack: it is inside a **Rust helper called from compiled code** when
the collector's signal reaches it. `classify_slot_helper_window` freezes it,
reads its published register file and every readable word of its stack from
`rsp` up, and recovers a conservative root set. Those roots kept the objects
alive and the cycle then refused to relocate anything at all.

### Why it refused, and why that was avoidable

ZGC has consumed `pinned_jit_roots_snapshot()` since 2026-08-13: conservative
JIT roots pin their page, the objects stay still, and everything else may move.
That mechanism was already right there. The helper-window peer simply had no
way to use it:

**`PINNED_JIT_ROOTS_BY_THREAD` is published BY EACH THREAD**, at its own
safepoint arrival or blocking-region entry. A helper-window peer is exactly the
thread that reached NEITHER -- a signal interrupted it mid-helper. And the scan
that recovers its roots runs on the COLLECTOR's thread, so it cannot publish
under the peer's `ThreadId` either. So the roots existed, the pin machinery
existed, and nothing connected them; the cycle refused instead.

The connection is a per-CYCLE pin set (`XT_CYCLE_PINNED_JIT_ROOTS`), unioned
into `pinned_jit_roots_snapshot()` and cleared where the coverage verdict it
used to be expressed as is cleared.

**Soundness rests on COMPLETENESS, not on trust.** The Linux classifier reads
the register file AND the whole readable stack band, so no address that peer can
reach is missing -- which is exactly the property that lets pinning substitute
for refusing. Pinned objects do not move; their FIELDS are still rewritten
through the pointer map, as for any other pinned root. `classify_slot_helper_window`
now returns `(has_jit, complete)`, and its two early returns -- `rsp` unusable,
no readable region -- report `complete = false`. A partial scan is NOT pinned
and keeps refusing, because pinning what you found does not help when what you
missed is unrewritable too. The Windows arm keeps refusing outright: its
completeness is not measurable here.

### MEASURED, one binary, `org.h2.test.db.TestMultiThread`, both arms `rc=0`

| arm | `helper_windows` | `hw_pinned` | `hw_refused` | `relocation_skipped_jit` |
|---|---:|---:|---:|---:|
| `CRATONVM_XT_HELPER_WINDOW_PIN=1` | 62 | **62** | **0** | 8 |
| `=0` | 44 | 0 | 44 | 14 |

and the coverage census, which is the readable half:

```text
PIN=0   xt-helper-window-conservative-scan=13   compiled-frame-oop-not-published=1
PIN=1   (absent)                                compiled-frame-oop-not-published=5
```

The reason does not merely get rarer -- it leaves the census. `hw_pinned` and
`hw_refused` ride on the `[GC] xt_peer_scan` line so neither has to be inferred
from the other, and `CRATONVM_XT_HELPER_WINDOW_PIN=0` restores the refusal on
the same binary.

### What this does NOT yet establish

`TestMultiThread` compacts either way (`objects_relocated` 64841 vs 66624), so
it proves the refusal is discharged and NOT that this class now compacts. The
run that would say so is `TestCachedQueryResults` itself, and the host it must
run on has to be quiet: at load 26 the class does not finish inside 1800 s,
`timeout` kills it, and **a killed run prints no `[GC]` summary at all** -- which
is how one earlier attempt produced an empty gate census and a different failure
(`Timeout trying to lock table "COUNTER"`) that says nothing about the heap.
Read `relocation_on_proven_jit` before believing any arm of it.

### 2026-09-01 (correction): the helper-window pin does NOT discharge the refusal

**The claim in the section above is wrong and is corrected here.** Pinning a
frozen peer's conservative roots removes the LABEL
`xt-helper-window-conservative-scan` from the coverage census; it does not make
the cycle relocatable, and the refusal has been restored.

Two things were missed.

**1. There is a SECOND refusal site, and it is unlabelled.**
`interpreter::gc_and_alloc`'s root gather ends with

```rust
if taken.count() > 0 || helper_windows > 0 {
    mark_moving_young_coverage_incomplete();      // no reason code
    mark_unrewritable_peer_state();
}
```

so a helper window marks the cycle incomplete a second time, with no reason
attached. That is why the `PIN=1` arm of the `TestMultiThread` A/B still reports
`coverage-proof-incomplete=8` with `none=2` among the reasons: the labelled
refusal moved into the unlabelled bucket. `relocation_on_proven_jit` was 1 with
the pin and 2 without — i.e. **engagement did not improve**, which the label's
disappearance had made look like progress.

**2. The pin set cannot be complete, because the scan probes with the wrong
predicate.** `helper_window_pass` is called with
`|a| shared.mem.heap.is_object_address(a)`, and ZGC's `is_object_address` is
`registry.contains(addr)` — **exact object bases only**. A frozen peer holding a
DERIVED pointer (a compiled loop's pointer into an array body is the ordinary
case) contributes no candidate, so its base is never pinned and relocating it
strands the peer. The second site's own comment says precisely this: *"a frozen
peer's registers can hold only a derived/interior pointer whose base would
otherwise be evacuated from under it, then zeroed and re-served"*.

### What that leaves, and it is a smaller, sharper question than before

The refusal IS dischargeable — the obstacle is one predicate, and it is no
longer expensive. `is_heap_addr` resolves an interior pointer to its base in one
backwards bit scan plus one header dereference (`nearest_base_at_or_below`), not
the O(live) registry iteration it used to be, and `vm_heap.rs` already feeds it
per-slot conservative scanning. So the remaining work is:

1. probe the helper window with `is_heap_addr` instead of `is_object_address`,
   so derived pointers resolve to the base that must be pinned;
2. price the resulting WIDER conservative root set — every `long` that lands
   inside a live object's extent becomes a root — which is the trade that has
   not been measured and is the reason this is not simply switched;
3. then, and only then, discharge BOTH sites together.

The pins are still published (`CRATONVM_XT_HELPER_WINDOW_PIN`, default on)
because they are strictly additive — a pin can only keep a page out of one CSet
— and because `hw_pinned`/`hw_refused` on the `[GC] xt_peer_scan` line are what
size the work above: **62 of 62 windows** on `TestMultiThread`.

### 2026-09-02: the widening is MEASURED, and it is cheap -- the discharge is now justified

The correction above left one thing unpriced: adopting `is_heap_addr` for the
helper-window probe widens the conservative root set, and "every `long` that
lands inside a live object's extent becomes a root" sounds like it could be an
order of magnitude. It is not.

`CRATONVM_XT_HELPER_WINDOW_INTERIOR=1` swaps the predicate and changes nothing
else -- both refusal sites still fire -- so the two arms differ only in which
words the helper-window scan admits. `org.h2.test.db.TestMultiThread`, one
binary, `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` summing the pass's own root counts:

| predicate | windows | conservative roots | per window |
|---|---:|---:|---:|
| `is_object_address` (exact bases) | 64 | 7 134 | 111 |
| `is_heap_addr` (resolves derived) | 60 | 8 365 | **139** |

**+25 % per window.** Read per WINDOW, not per run: the two arms ran at load
5.6 and 17.7 and therefore took different numbers of passes (16 and 17), so the
totals are not directly comparable and the ratio is.

That is a small price for the property that makes pinning sound at all -- a
derived pointer resolving to the base that must not move. So the remaining work
is no longer blocked on an unknown:

1. probe the helper window with `is_heap_addr`;
2. pin the resolved bases (the machinery is already there and already publishes
   -- `hw_pinned` is 62 of 62 / 64 of 64 / 60 of 60 across every run measured);
3. discharge BOTH refusal sites together -- the labelled one in `xt_root_scan`
   and the unlabelled `mark_moving_young_coverage_incomplete()` +
   `mark_unrewritable_peer_state()` in `interpreter::gc_and_alloc`, which is the
   one the first attempt missed;
4. and only then read `relocation_on_proven_jit` on this class, which is the
   number the whole page turns on.

Step 3 is the one to be careful with: discharging only the labelled site is what
made the first attempt look like progress while engagement did not move at all.

### 2026-09-02 (decisive): compaction relieves the OOM -- but see the 32-core correction below before reading this as a fix

`CRATONVM_ZGC_ASSUME_REWRITABLE=1` forces the whole relocation gate. One binary,
`--Xmx 1g`, 3000 s cap:

| arm | `arena allocation failed` | 65536-slot ref-array OOM |
|---|---:|---:|
| `CRATONVM_ZGC_ASSUME_REWRITABLE=1` | **0** | **0** |
| off | 14 | **13 999** |

**The failing allocation disappears completely.** Not fewer, not slower --
absent. So every framing question this page has carried is now settled:

* the OOM framing is RIGHT: it is fragmentation of the low end, and relocation
  relieves it;
* `98304` really is where `ConcurrentHashMap` needs its 65536-slot table, and
  that table really is the request that cannot be served;
* the four JIT safepoint defects fixed on 2026-08-30 are real repairs to the
  COVERAGE PROOF, and none of them could ever have fixed this class on their
  own, because the proof is a conjunction and they each closed one term;
* and the helper-window work is likewise a term, not the answer.

**Both arms still hit the cap** with `Timeout trying to lock table "COUNTER"`,
which is the load artefact described above and not a heap result -- the runs
were at load 13-48. The heap evidence is per-cycle and survives that; the
pass/fail does not, and is still owed on a quiet host.

### What this makes the work

The question is no longer "which obligation" but "how many". Discharging one at
a time provably does not move `relocation_on_proven_jit` -- three attempts, each
removing a different term, each leaving engagement where it was. The remaining
terms measured on `TestMultiThread` after the helper window is discharged are:

```text
zgc-relocation-coverage-reason: compiled-frame-oop-not-published=8
zgc-relocation-coverage-reason: cross-thread-jit-peer=5
```

so the next work is those two, together, with `relocation_on_proven_jit` as the
only acceptance test -- and `CRATONVM_ZGC_ASSUME_REWRITABLE=1` as the upper
bound that says what winning looks like.



### 2026-09-02 (32-core box, legitimate discharge): `relocation_on_proven_jit` moves OFF ZERO with NO corruption

The Windows helper-window arm had none of the pin/discharge work -- it was
written for Linux -- and on this box that is where the refusals are:

```text
zgc-relocation-coverage-reason: xt-helper-window-conservative-scan=1468   (95% of 1539)
                                compiled-frame-oop-not-published=59
                                active-safepoint-map-incomplete=8
                                unregistered-jit-frame-on-stack=3
                                parent-frame-map-incomplete=1
helper_windows=9087  hw_pinned=0  hw_refused=0      <- none of the code ran
```

With the Windows arm implemented (a counted window there is complete by
construction: `snapshot_peer` returning `Some` means the whole GPR range and the
whole `[rsp, committed_region_end)` band were captured), one binary,
`org.h2.test.db.TestMultiThread`, both arms `rc=0`:

| arm | `hw_pinned` | `skipped_jit` | `on_proven_jit` | coverage reasons | **NPE** |
|---|---:|---:|---:|---|---:|
| base | 71 | 18 | **0** | `helper-window=12`, `oop-not-published=6` | 0 |
| `HELPER_WINDOW_DISCHARGE=1` | 69 | **7** | **2** | *(helper-window absent)*, `xt-peer=3`, `oop-not-published=3`, `unregistered=1` | **0** |

**This is the first change in this investigation that moves
`relocation_on_proven_jit` off zero without bypassing the proof, and it does so
with zero NPEs** -- against the 48 that `ASSUME_REWRITABLE` produces on the same
box. So pinning a frozen peer's conservative roots, with the interior-resolving
probe so a derived pointer resolves to the base that must stay still, is SOUND
where bypassing the proof is not.

**It is not sufficient on its own.** `TestCachedQueryResults` with the discharge
alone still capped at 2400 s with 6 354 reference-array OOMs. That is the
conjunction again: removing 1 468 of 1 539 refusals still leaves
`oop-not-published=59`, `active-safepoint-map-incomplete=8` and
`unregistered=3`, and ONE of those per cycle refuses that cycle -- while this
class needs MOST cycles to relocate to keep the arena from shattering.

So the next term is `compiled-frame-oop-not-published`, and the acceptance test
is unchanged: `relocation_on_proven_jit > 0` with 0 OOM and 0 NPE together.


#### And on the class itself: 98 304 -> 99 100, OOMs halved, no corruption

Same binary, `TestCachedQueryResults`, discharge on:

| arm | secs | `actual` | ref-array OOM | COUNTER | **NPE** | `on_proven_jit` |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 1519 | 98 304 | 1 497 | 199 | 0 | 1 |
| `HELPER_WINDOW_DISCHARGE=1` | **997** | **99 100** | **834** | 66 | **0** | 2 |

**+796 entries recovered, reference-array OOMs almost halved, 34 % faster, and
zero NPEs** -- all 5 111 helper windows pinned, none refused. This is the first
arm to move the class's own assertion without corrupting anything.

And it re-sorts what is left. The reason census is FIRST-WINS, so removing the
helper window exposes the terms behind it -- `unregistered-jit-frame-on-stack`
goes from 3 to 360 not because anything got worse but because 1 468 cycles that
used to stop earlier now reach it:

```text
cross-thread-jit-peer             448   (51 % of 877)
unregistered-jit-frame-on-stack   360   (41 %)
compiled-frame-oop-not-published   68   ( 8 %)
active-safepoint-map-incomplete     1
```

So the next term is `cross-thread-jit-peer`, and there is a specific reason to
think it is now WRONG rather than merely unsatisfied: the peers it refuses for
are blocked threads that never reach `publish_peer_jit_coverage_for_stw`, and
those are exactly the threads whose helper windows this change now PINS. A
pinned peer's frames cannot move, so it does not need to prove them rewritable
-- but `peer_coverage_accounted` still counts its JIT entries in `peer_depth`
and finds no deposit against them.

Fixing that means excluding the pinned-blocked population from `peer_depth`
(e.g. a process-wide blocked-JIT-depth counter maintained at blocked-region
enter/leave), and it is the same accounting that produced a false positive
earlier on this page -- so it must be measured on `relocation_on_proven_jit`,
not on whether the label disappears.

### 2026-09-02 (32-core local box): the baseline reproduces EXACTLY, and forcing relocation CORRUPTS

Moved off the shared Azure host, which spent the day between load 1 and 477 and
twice had H2's `target/` deleted underneath a run. This box is 32 cores /
64 GB, uncontended, with the corpus pinned locally. Release binary, `--Xmx 1g`,
`TestCachedQueryResults`:

| arm | secs | `actual` | ref-array OOM | arena | COUNTER | **NPE** | relocation |
|---|---:|---:|---:|---:|---:|---:|---|
| baseline | 1519 | **98 304** | 1 497 | 11 | 199 | **0** | skipped 1539, proven 1 |
| `ASSUME_REWRITABLE=1` | **629** | 99 952 | **0** | **0** | **0** | **48** | skipped 0, proven 24 |

**The baseline reproduces this page's number to the digit** -- `actual: 98304` --
so the local box is a faithful reproduction and every earlier arm can be
re-read against it.

**And the forced arm CORRUPTS.** Its 48 missing entries are not OOM and not lock
timeouts; they are

```text
General error: "java.lang.NullPointerException"; SQL statement:
SELECT counter FROM Counter WHERE id = 1 FOR UPDATE WAIT 0.5
```

**48 in the forced arm against 0 in the baseline**, same binary, same corpus,
one flag apart. That is what `CRATONVM_ZGC_ASSUME_REWRITABLE`'s own doc
promises -- *"it relocates under frames nobody proved rewritable; expect
corruption if the answer is no"* -- and this is the first run where the
corruption is visible rather than theoretical. The Azure arms did not show it
(their residual was exactly their COUNTER count); 32 cores and a 2.4x faster
run expose the race that 8 contended cores hid.

### What that corrects, and it is a correction to this page's own 2026-09-02 entry

The earlier entry said compaction "fixes" the class and called the forced arm an
upper bound on what winning looks like. Half of that stands and half does not:

* **Stands:** compaction eliminates the OOM. 1 497 reference-array failures and
  11 arena failures go to ZERO, and the wall clock more than halves. The
  fragmentation diagnosis is right and relocation is the relief.
* **Does NOT stand:** the forced arm is not a preview of a correct fix. A
  correct fix must have **zero NPEs as well as zero OOMs**, and this arm trades
  one for the other.

So the coverage conjunction is **load-bearing, not over-conservative**. The
obligations cannot be bypassed to buy compaction; they have to be SATISFIED so
that the frames really are rewritable. That makes the remaining work narrower
and strictly harder than the previous entry implied:

1. discharge `compiled-frame-oop-not-published` and `cross-thread-jit-peer` by
   making the frames genuinely provable -- not by skipping the proof;
2. the acceptance test is `relocation_on_proven_jit > 0` **with 0 NPE and 0
   OOM**, which no arm has yet produced together;
3. `ASSUME_REWRITABLE` remains useful for exactly one thing: showing that the
   OOM is relievable at all. It is not a target.

### 2026-09-02 (idle host): the OOM is gone and the RESIDUAL IS PAUSE DURATION, not the heap

Three release-profile arms, `--Xmx 1g`, on a box that was briefly idle. The
accounting is exact in every one -- **missing == COUNTER timeouts, OOM == 0**:

| arm | load | `actual` | missing | COUNTER | OOM | conc cycles |
|---|---:|---:|---:|---:|---:|---|
| `ASSUME_REWRITABLE` | 8.92 | 99 988 | 12 | 12 | **0** | 0 of 26 |
| `ASSUME_REWRITABLE` | 3.29 | 99 984 | 16 | 16 | **0** | 0 of 27 |
| `ASSUME` + `CONC_START=60` | 8.48 | **99 990** | **10** | **10** | **0** | 4 of 26 |
| `XT_HELPER_WINDOW_DISCHARGE` alone | 1.4 | *(capped)* | -- | 28 | **4 587** | -- |

Two conclusions, and the second is new.

**The helper-window discharge alone does not fix the class.** 4 587 reference-array
OOMs, capped at 1800 s. That is the conjunction result reproduced on the class
itself rather than on `TestMultiThread`, and it is why no single obligation is
worth repairing on its own.

**The residual failures are GC PAUSE DURATION, not host load.** The arm at load
3.29 lost 16 and the arm at load 8.48 lost 10 -- fewer losses at HIGHER load, so
contention cannot be the driver. What changed is that four of its cycles marked
concurrently. And the mechanism is documented in the flag's own measurements
(`Z_CONC_START_PERCENT_DEFAULT`):

| arm | mean pause |
|---|---:|
| 8 mutator threads, stop-the-world | **609 ms** |
| 8 mutator threads, concurrent, 2 workers | **253 ms** |

**609 ms is above H2's `FOR UPDATE WAIT 0.5`.** So every STW compaction cycle
that lands inside a lock wait costs one entry, and `CRATONVM_ZGC_CONC_START`
(default `0`, i.e. every cycle stop-the-world) is what decides how many do.
`zgc-real` reports `occupancy=64503936/1073741824` -- the heap is 6 % occupied
once compaction runs, so the 60 % trigger rarely opens and only 4 of 26 cycles
were concurrent. A lower `CONC_START` should open it on nearly all of them.

So the path to a PASS is now fully specified and has nothing left in it that is
mysterious:

1. discharge the coverage conjunction so relocation runs without the unsafe
   instrument (`compiled-frame-oop-not-published` + `cross-thread-jit-peer`
   together);
2. keep the compaction pause under 500 ms -- `CRATONVM_ZGC_CONC_START` low
   enough that most cycles mark concurrently;
3. measure on a box with headroom. This one spent the session between load 1 and
   155, and at 155 with 1 GB free the OOM killer takes the JVM (`rc=137`).

### 2026-09-02 (RELEASE + forced relocation): the shortfall goes 1696 -> 12, and NONE of the 12 is an OOM

The livedbg arm above could not produce a pass/fail because its `opt-level=1`
slowness tripped H2's `FOR UPDATE WAIT 0.5`. This is the same experiment on a
`--profile release` binary, load 8.92:

```text
CRATONVM_ZGC_ASSUME_REWRITABLE=1     rc=1   1018 s
    Expected: 100000 actual: 99988
    Timeout trying to lock table "COUNTER"   12
    java.lang.OutOfMemoryError                0
    native reference array of length 65536    0
    arena allocation failed                   0
    LOST UPDATE                               0
    relocation_skipped_jit 0   relocation_on_proven_jit 26
    compaction_cycles 26       objects_relocated 536508
```

Apply this page's OWN accounting method -- the one the L7 addendum used to
show that 1696 = 1691 OOM + 5 SQLException:

```text
100000 - 99988                                 = 12
OutOfMemoryError raised in tasks                  0
SQLException (COUNTER lock timeout) caught       12
                                                ---
                                                 12
```

**The OOM population is gone. All twelve survivors are H2's own half-second
lock timeout**, which the callable catches as `SQLException` and prints -- the
same class of loss the original accounting attributed 5 of 1696 to.

So the defect this page is about is FULLY explained and FULLY relieved by
relocation:

| arm | actual | OOM | SQLException |
|---|---:|---:|---:|
| as this page found it | 98 304 | 1 691 | 5 |
| relocation forced (release) | **99 988** | **0** | 12 |

The residual 12 are a shared-host artefact, not a VM defect: `WAIT 0.5` is half
a second, the host was at load 8.9 on 8 cores, and 26 compaction pauses moving
536 508 objects sit inside that window. HotSpot passes this class in 9 s on the
same box. An idle host is what would turn 12 into 0, and that run is the only
thing between this page and a PASS.

**What this makes the remaining work.** The coverage conjunction is no longer a
theory about what might help -- it is the only thing standing between the
measured state above and a passing class. The terms left after the helper-window
discharge are `compiled-frame-oop-not-published` and `cross-thread-jit-peer`,
they must be closed together, and `relocation_on_proven_jit > 0` is the
acceptance test.

#### The paired control, and why it did not finish

Same binary, same host window, flag off:

| arm | ref-array OOM | arena failures | outcome |
|---|---:|---:|---|
| `CRATONVM_ZGC_ASSUME_REWRITABLE=1` | **0** | **0** | reached the assertion, `actual: 99988` |
| control (flag off) | **6 617** | **13** | `rc=137` (SIGKILL) at 2 489 s, 131 COUNTER timeouts |

`rc=137` is the Linux OOM killer, not the VM: the host was at load 138 with
**1 GB of 31 GB available** while other tenants built. The control therefore
never reached its assertion, and its numbers are a lower bound on a truncated
run rather than a matched endpoint. What it does establish is the only thing it
is used for here -- that the same binary in the same window produces thousands
of the exact failure the treatment arm produces none of.

**Host-condition note for anyone rerunning this.** Every arm of this page is
sensitive to the box in three separate ways, and all three have now bitten:

1. `FOR UPDATE WAIT 0.5` turns CPU contention into `SQLException`s that look
   like data loss (12 of them even in the good arm);
2. a run killed by `timeout` prints no `[GC]` summary, so the counters the
   question turns on vanish;
3. at load 130+ with memory exhausted the OOM killer takes the JVM (`rc=137`)
   or `rustc` during a fat-LTO link, and a `cargo build && cp` then copies a
   STALE binary while printing success.

Record `/proc/loadavg` and `free -g` beside every arm.

### 2026-09-02 (quiet host): ZERO OOMs, 24 compaction cycles, and the gate never refuses

The 3000 s arms above both capped under contention. This one ran on a quiet host
(load 7.64 / 6.02 / 5.79) and **exited cleanly**, so it carries the `[GC]`
summary the capped runs could not:

```text
CRATONVM_ZGC_ASSUME_REWRITABLE=1     rc=1   3575 s
    arena allocation failed          0
    native reference array OOM       0
    java.lang.OutOfMemoryError       0
    relocation_skipped_jit           0
    relocation_on_proven_jit         24
    compaction_cycles                24     objects_relocated=641125
    zgc-relocation-skip-reason:      (none)
```

**Zero OutOfMemoryError of any kind, and the relocation gate refuses nothing.**
Against the same binary with the flag off, which produced an 11.6 MB error log
of exactly the OOM this page is about. So the heap defect is not merely reduced
by compaction; under compaction it does not exist.

**The class still fails, and the reason is now the INSTRUMENT, not the heap.**
`rc=1` here is `Timeout trying to lock table "COUNTER"` again -- but this arm ran
at load 7.6, so contention is no longer a sufficient explanation. The likelier
cause is that this binary is built `--profile livedbg` (`opt-level = 1`,
`lto = false`), chosen because the fat-LTO release link was being OOM-killed by
other tenants at load 130+. An `opt-level=1` VM is several times slower than
release, which is enough on its own to trip H2's `FOR UPDATE WAIT 0.5`. The
release build reaches the ASSERTION (`Expected: 100000 actual: 98304`) in 641 s;
this one never gets that far.

So the pass/fail verdict needs `--profile release` **and** the flag, and until
that run exists this page should claim exactly what is measured: compaction
removes the OOM, and the class's remaining failure under the instrument is not
attributable to the heap.

### 2026-09-02 (later): the discharge WORKS and changes nothing -- `coverage-proof-incomplete` is a conjunction

`CRATONVM_XT_HELPER_WINDOW_DISCHARGE=1` gates BOTH refusal sites off one
per-cycle condition and forces the interior-resolving probe, so the helper
window is genuinely discharged this time -- not just relabelled. One binary,
`org.h2.test.db.TestMultiThread`:

| discharge | helper windows | coverage reasons | `skipped_jit` | `on_proven_jit` | compaction |
|---|---:|---|---:|---:|---:|
| off | 49 | **`xt-helper-window=12`**, `oop-not-published=3` | 15 | 1 | 1 cycle / 2 220 |
| on | 37 | *(absent)*, `cross-thread-jit-peer=5`, `oop-not-published=8` | 13 | **0** | **0** |

The reason leaves the census. **Engagement does not improve** -- it goes to
zero, and `coverage-proof-incomplete` barely moves (15 -> 13). The refusals
simply redistribute to the next obligations in line.

**That is the third time the same lesson has arrived, and it should be the
page's headline.** `coverage-proof-incomplete` is a CONJUNCTION over many
obligations, and discharging them one at a time can never move
`relocation_on_proven_jit`:

* `CRATONVM_XT_JIT_COVERAGE_ASSUME` took the handshake to
  `accepted=1731 refused=0` -> the class failed identically;
* `CRATONVM_XT_HELPER_WINDOW_PIN` removed the label only -> engagement 1 vs 2;
* `CRATONVM_XT_HELPER_WINDOW_DISCHARGE` removed the obligation outright ->
  engagement 1 -> 0.

So the next measurement is not another repair. It is
`CRATONVM_ZGC_ASSUME_REWRITABLE=1`, which forces the WHOLE term and is
unsafe by construction, and it answers the only question worth asking before
any further work: **if every compiled frame were rewritable, would compaction
fix this class at all?** If it would not, the page's OOM framing is wrong at the
root and every coverage repair above it is beside the point.

### 2026-09-01 (later): two reproduction attempts that FAILED, and what each eliminates

`probes/ZgcRefArrayFragProbe.java` is checked in because it does NOT reproduce.
Both shapes were run at `--Xmx 1g` against a HotSpot control:

| probe shape | HotSpot | CratonVM | arena failures |
|---|---|---|---:|
| `Object[65536]` in a loop + churn | `served=254483 failed=0` | `served=185964 failed=0` | 0 |
| `ConcurrentHashMap` keySet to 100000 + churn | `size=100000 missing=0 oom=0` | `size=100000 missing=0 oom=0` | 0 |

**`ConcurrentHashMap` growth to 100000 is not sufficient**, even with four churn
threads shattering the arena underneath it. That settles a question this page
has answered twice in opposite directions: the number 98304 IS the CHM resize
threshold and the failing allocation IS that 65536-slot table (so the
2026-08-30 un-retraction stands), AND the resize alone does not fail (so
`ChmKeySetGrowth` was not wrong either). What fails it is the ARENA STATE H2
builds -- `spans=219425 largest_span=246704` -- which takes sustained
mixed-lifetime allocation over hundreds of seconds, not a burst.

**And `helper_windows=0` in every probe run.** A pure-Java allocation workload
never puts a peer inside a Rust helper at collection time; H2 does, through file
I/O, MVStore chunk compression and the JDBC path. So the helper-window repair
cannot be A/B'd on a probe like this -- an instrument armed where it cannot fire
-- which is why `org.h2.test.db.TestMultiThread` is the class used for it above.

The second shape also showed relocation ENGAGING on CratonVM
(`relocation_on_proven_jit=118`, `compaction_cycles=116`, `objects_relocated=101058`)
with zero arena failures. A heap that compacts does not reach this failure,
which is consistent with everything else on this page and is the reason the
class's `relocation_on_proven_jit=0` is the number that matters.

### A measurement trap this class carries, and it is not the heap

`TestCachedQueryResults` cannot be measured on a loaded host, for a reason that
has nothing to do with GC. Its inner statement is

```sql
SELECT counter FROM Counter WHERE id = 1 FOR UPDATE WAIT 0.5
```

so under contention it fails with `Timeout trying to lock table "COUNTER"` --
a starved half-second SQL lock. Two capped runs on a host at load 20-60
produced exactly that and nothing else. Worse, both were killed by `timeout`
(`rc=124`), and **a run killed by `timeout` prints no `[GC]` summary at all**,
so `relocation_on_proven_jit`, the gate census and the frag windows are simply
absent from it. Record `/proc/loadavg` beside every arm, give the run a cap it
can finish inside, and treat a `COUNTER` timeout as "no measurement", not as a
result.

## Status

**OPEN. The chain this Status line describes is REFUTED -- see the 2026-08-30 (b) addendum above, which measures the handshake at accepted=1731 refused=0 and gets the identical failure. Kept verbatim below because the four repairs it led to are real. Original text: the chain is traced to one frame — see §"2026-08-29 (second)".**
The `xt_cov=(accepted=0 refused=1730)` lead this page shipped with turned out to
be four measurements deep: the peers DO park, some of their own proofs return
false, the obligation is `UNPUBLISHED_FRAME_OOP` in 3 of 4, and six of the seven
unpublished words belong to frames whose safepoint-id slot carries no usable id.
The discriminator then split THAT into two defects: **10 of 13 such frames have
`sp_id == 0` — they have not reached their first safepoint — and 3 have a heap
pointer sitting in the reserved slot.** It is not a shifted `rbp`.

Split out 2026-08-29 from
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`, which is
retired: that page's own class passes 2/2 and the four fragmentation defects it
ended on are fixed. **This class is not fixed by them.**

Measured on the merged tip with all four repairs, `--Xmx 1g`, 900 s cap:

```text
org.h2.test.jdbc.TestCachedQueryResults  rc=124  secs=900  oom=2990  arena=11
```

and the same-binary control beside it:

| arm | rc | secs | `oom` | `arena` | load at start |
|---|---:|---:|---:|---:|---:|
| default (all four repairs) | 124 (cap) | 900 | **2 990** | 11 | 9.0 |
| all three switches `=0` (pre-2026-08-29) | 124 (cap) | 900 | **6 318** | 12 | 20.9 |

**The repairs halve the OOM rate and change nothing else.** This is also the one
class in the family where a rate is measurable at all — thousands of events per
run rather than one pass/fail — so it is the strongest evidence on that page
that the four repairs do something, and simultaneously the proof that they are
not enough here.

Read the load column before over-reading the factor: the control ran at more
than twice the load, and on this collector load decides how often a cycle is
allowed to compact at all (`relocation_on_proven_jit`). The direction is solid,
the exact ratio is not.

Against the parent page's older reading — `rc=124` at a **1 500 s** cap with
**18 048** `OutOfMemoryError` and 14 arena failures — the per-second rate is
down about 3.6x. **A rate improvement on a livelock is not a fix**, and this
page exists so that distinction is not lost in the parent's Status line.

## Why it is a different shape from the rest of that family

Every other class on that page failed ONCE: an allocation could not be served,
`OutOfMemoryError` propagated, the process ended. This one throws thousands and
keeps running, which means something is catching them and retrying. The parent
page recorded the same signature in Spring Framework's
`SimpleClientHttpResponseTests`, where *"the guard's own `occurrence` counter
doubles on every subsequent firing (32768 -> 1048576 -> …)"* — an exponential
retry.

So the question here is not only "why can the heap not serve this request" but
**"what is retrying, and why does it never give up"**. Those are different
repairs, and the second one is not a collector question at all.

## The lead this page ships with: the cross-thread handshake refuses 1 730 times and accepts 0

`CRATONVM_DBG_JIT_ROOTSCAN=1`, same class, 2026-08-29 tip:

```text
frame_cov=(no_slot=0 misaligned=0 no_map=77 incomplete=0 ok=6962)
xt_cov=(accepted=0 refused=1730 deposits=469)
```

**`accepted=0 refused=1730`.** The cross-thread coverage handshake — which marks
a cycle unprovable whenever a thread OTHER than the collection initiator is in
compiled code — refuses every single time on this class. On the same binary,
`org.h2.test.db.TestMultiThread` reports `accepted=1 refused=8 deposits=26` and
passes.

That is a far better lead than "the heap is fragmented". A cycle the handshake
refuses does not relocate at all, so none of the four 2026-08-29 repairs runs on
it, and the heap fragments with nothing to repair it. **Attack the refusal
before attacking the allocator.**

`incomplete=0` on the same line, which retires a residual the parent page
carried: it recorded `incomplete=5` here as *"the first time anywhere that a map
refuses on its OWN claim"*. It does not reproduce on this tip.

`CRATONVM_DBG_OOPCOV=1` on the same class says which shapes DO make a
safepoint's shadow claim incomplete at compile time:

```text
scauses(gate=0 desync=0 marks=83 scratch=0 locals64=0
        dataflow=151 nopush=0 inline_scope=5)
```

`dataflow=151` (the forward "must be oop" dataflow never reached that bytecode
pc, so there is no local oop mask to publish from) and `marks=83` (the
operand-stack oop marks are not exact there — a revived dead-code merge
reconstructing the stack at a nonzero depth). Both are compiler shapes, not
collector ones, and both feed the refusal above.

## 2026-08-29 (second): the handshake refuses because PEER PROOFS FAIL — and one frame's safepoint id is half an object pointer

The lead this page shipped with was `xt_cov=(accepted=0 refused=1730)`. Four
measurements later the chain is complete, and the bottom of it is one frame.

### 1. The peers DO park. Their own proofs fail.

The shortfall was assumed to be peers the handshake cannot see — an OS-frozen
thread, or one blocked in a native with compiled frames below it, which deposits
nothing. `CRATONVM_DBG_XT_COVERAGE=1` says otherwise: the deposits happen, and
some of them carry `proven=false`.

```text
[xt-coverage] peer_depth=3 proven=0 accounted=false
[xt-coverage] peer_depth=9 proven=0 accounted=false
[xt-coverage] peer_depth=3 proven=3 accounted=true
...
6 × peer deposit proven=true  depth=N
4 × peer deposit proven=false depth=N
```

A peer that parks, runs its own per-thread coverage proof and gets `false`
deposits nothing — and the initiator's test is `proven >= peer_depth`, so ONE
failing peer refuses the whole cycle. That is a different repair target from
"reach the parked peers", and it is where the work belongs.

### 2. WHICH obligation — and the counter that could not say

`proven=false` has six possible causes and they want six different repairs, so
the peer-deposit line now names the one that fired. The first attempt at that
diagnostic diffed `moving_young_fallback_reason_counts()` around the proof and
reported `why=none` for every failure — **a vacuous read**: `bump_reason_count`
has exactly one caller, `record_moving_young_coverage_fallback`, which is the
GENERATIONAL collector's per-cycle accounting. On ZGC those counters never move
at all. The reason MASK (`incomplete_reason_mask_add`, called on every mark) is
the signal, and diffing it gives:

```text
2 × proven=false why=compiled-frame-oop-not-published
1 × proven=false why=compiled-frame-band-unbounded,innermost-rbp-belongs-to-unguarded-callee
1 × proven=false why=active-safepoint-map-incomplete,compiled-frame-oop-not-published
```

**`UNPUBLISHED_FRAME_OOP` in 3 of 4.** That is the obligation the parent page
spent 2026-08-26/27 on and relaxed for dead slots; the relaxation is not enough
here.

### 3. The band census, and the one frame under all of it

`CRATONVM_MOVING_YOUNG_BAND_DBG=1` — seven unpublished words in the run,
4 `operand-spill` and 3 `reserved-locals-tail`. **Six of the seven are one
frame**, and its header is the finding:

```text
off=48 region=reserved-locals-tail value=0x200671a2c18
       sp_id=Some(1729768472) live_hi=None
       layout={ java_locals_hi: 32, locals_hi: 88, spill_lo: 88, spill_hi: 192 }
```

`1729768472` is not a bytecode pc — those are bounded by 65535. It is
**`0x671a2c18`, the low 32 bits of `0x200671a2c18`** — the heap pointer this same
scan reports at `off=48` of the same frame. **The frame's safepoint-id slot
holds half an object pointer.**

`live_hi=None` on the same line is the same fact from the other side: no map
matched the id, so `moving_young_frame_live_hi` had nothing to return. And
because `frame_active_map_slots` also returns `None`, the 2026-08-27 dead-slot
relaxation deliberately does not fire — which is why all six of that frame's
words are reported and why its proof returns `UNPUBLISHED_FRAME_OOP`.

The other frame in the same run reports `sp_id=Some(3) live_hi=Some(160)` and
exactly one word. The machinery works; one frame's id does not.

> The report used to print `in_map=Some(false)` for this, which reads as "the
> dataflow calls this slot dead" and sends a reader at the band verifier. It
> conflated "a map was found and does not name the slot" with "no map exists for
> this id". It now prints `no-map-for-id`, and that is the line to grep.

### 4. The discriminator, run — and it is TWO defects, not one

`sp_id_off` and the whole reserved-locals tail are now printed beside a
`no-map-for-id` frame, which separates "something stored an oop into the
reserved slot" from "rbp is wrong so the read landed on a neighbour". One run,
13 such frames:

```text
no-map-for-id sp_id_off=24 tail(8..64): [8]=0x2006689f670 [16]=0x20012385010
    [24]=0x2004264ebc0 [32]=0x7305aeff7408 [40]=0x0 [48]=0x200161f0030 ...
no-map-for-id sp_id_off=48 tail(32..88): [32]=0x0 [40]=0x20012385010
    [48]=0x0 [56]=0x7305adff5408 [64]=0x80 [72]=0x20016ef0168 ...
```

| what is in the sp-id slot | frames | reading |
|---|---:|---|
| **`0`** | **10** | the frame has not reached its first safepoint — the slot is still the prologue's zero |
| **a heap pointer** | **3** | the reserved slot has been OVERWRITTEN with an oop |
| anything else | 0 | — |

**It is not a shifted `rbp`.** In the first line the pointer sits at exactly
`sp_id_off=24`, and the rest of that tail is plausible for this frame
(`0x7305aeff7408` is a native/stack pointer — the cached JIT thread or the stack
floor; `[40]=0x0`). A wrong `rbp` would have made the whole tail read like some
other frame's, and it does not.

So the one lead has become two, with very different sizes and repairs:

* **10 of 13 — `sp_id == 0`, a frame that has not reached a safepoint yet.**
  `find_oop_map_for_safepoint_id(0)` finds nothing, `frame_active_map_slots`
  returns `None`, and the 2026-08-27 dead-slot relaxation FAILS CLOSED by
  design — so every movable-looking word in that frame's band refuses the whole
  collection. This is the dominant population and it is not a corruption at
  all; it is a frame the machinery has no statement about. Whether it can be
  discharged is a real question: its java locals hold incoming arguments, so a
  relocation still has to rewrite them, and with no map the shadow stack is the
  only channel that could. **Start here — it is 77 % of the refusals.**
* ~~**3 of 13 — an oop AT `sp_id_off`.** A store whose offset lands in the
  reserved-locals tail … a genuine codegen defect~~ — **WRONG, see §4a.** There
  is no store. The slot was never initialised, so it read whatever the previous
  frame at that stack depth left; zeroing it in the prologue takes this
  population to 0 in both measured rounds.

### 4a. 2026-08-27 — it is ONE defect, not two: the sp-id slot is never initialised

The split above is wrong, and the correction is a one-line fix.

**Nothing writes an oop into the reserved slot. Nothing writes the slot at
all** until the first safepoint. `emit_prologue` zeroes
`shadow_thread_slot_off` and `shadow_savetop_slot_off` — with a comment giving
exactly the reason, *"it must read 0, not uninitialised stack. The single-pass
backend zero-initialises for exactly this reason"* — and does **not** zero
`sp_id_slot_off` beside them. Ids start at 1 precisely so `0` can mean "no
safepoint reached" (the slot's own allocation comment says so), but the
prologue never established the sentinel.

So both populations are the same thing, read at two different pieces of stack:
`0` where the region happened to be clean, a stale oop where a previous frame
at that depth had left one. Not "a store whose offset lands in the
reserved-locals tail".

**MEASURED**, same class, one binary, `CRATONVM_JIT_ZERO_SPID` as the A/B, two
rounds — the census split by what sits in the slot:

| arm | `no-map-for-id` | `sp_id == 0` | sp-id out of range (a stale word) |
|---|---:|---:|---:|
| OFF (today) | 22 | 1 | **9** |
| ON | 20 | 10 | **0** |
| OFF (today) | 16 | 1 | **7** |
| ON | 6 | 3 | **0** |

The out-of-range population goes to **zero and stays there**, and the frames
reappear in the `sp_id == 0` bucket. That is the predicted signature of
uninitialised stack and not of a stray store.

**The hazard this closes is worse than the refusal it was found through.**
Safepoint ids are small consecutive integers, so a stale word can equal a
*valid* id for that method — and then `find_oop_map_for_safepoint_id` matches
the map for a DIFFERENT program point and relocation rewrites against it. A
silent wrong answer, not a refused cycle. The 13-frame census only ever showed
the loud half.

**It does NOT fix this class.** `xt_cov` still reads `accepted=0` on both arms
(refused 22/35 and 30/28 over 300 s), because a zeroed slot fails closed
exactly as a garbage one did. What it does is remove the corruption hazard and
collapse the two populations into one, so the remaining question is single and
clean: **can a frame that has taken no safepoint be discharged?** That is now
100 % of `no-map-for-id`, not 77 %.

**Caveat on the single-pass backend, not fixed here.** `x64/safepoint.rs` stores
`cur_bc_pc` as the id, and **bytecode pc 0 is legal** — so for those frames `0`
is ambiguous between "at bci 0" and "never stored", and zeroing the prologue
slot there could make an unsafepointed frame match the bci-0 map. The IR
backend has no such ambiguity (ids start at 1), which is why the fix is scoped
to it. Giving the single-pass backend a +1-encoded id would remove the
ambiguity and let it take the same repair.

**And this class's own symptom did not reproduce here**: `oom=0` on both arms
at a 300 s cap on an idle host, against the page's `oom=2990` at 900 s. Either
the cap or the load matters; the band census above is what the A/B rests on,
not an OOM rate.

### 5. What to do next, in order

1. **Take the `sp_id == 0` population first** — now 100 % of `no-map-for-id`
   after §4a removed the stale-word half, and the question is
   whether a frame that has taken no safepoint can be discharged at all rather
   than refusing every cycle it is live for.
2. Only then look at the `operand-spill` words. Four of the seven are on the
   frame with the garbage id and may simply be its neighbours.
3. `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0` remains the same-binary control: it
   restores the blanket refusal, so it should change nothing here (the handshake
   is already refusing every cycle) and a difference would mean the accounting,
   not the proof, is the problem.

## What to do first (superseded by the section above)

1. **Find the retry loop.** `rc=124` at the cap with `oom` in the thousands is a
   caller swallowing `OutOfMemoryError` — H2's own code, or a
   `java.util.concurrent` path retrying an allocation. The Java stack at the
   first OOM is the lead; the 2990th tells you nothing.
2. **Then read the arena state at a failure**, exactly as the parent page does:
   `request=`, `largest_free_block=`, `span_hist=`, `high_*`, and the
   `[GC] zgc-high-compaction:` line. If `largest_free_block` is in the megabytes
   the collector is doing its job and the defect is upstream of it.
3. `CRATONVM_ZGC_PUBLISH_VACATED=0` / `CRATONVM_ZGC_HIGH_COMPACTION=0` are the
   same-binary bisects for the 2026-08-29 repairs, and `CRATONVM_ZGC_TLAB=0` is
   the blunt control that makes the whole TLAB path go away.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
CRATONVM_GC_STATS=1 timeout 900 <cratonvm-bin> --java-home /data/toolchain/jdk-25 \
    --Xmx 1g -c "$CP" org.h2.test.jdbc.TestCachedQueryResults
```

## 2026-09-02 — `cross-thread-jit-peer` cannot be discharged by pinning

`cross-thread-jit-peer` was 448 of the 877 relocation refusals left after the
helper-window discharge, and it looked WRONG rather than merely unsatisfied: it
refuses for blocked peers that never reach `publish_peer_jit_coverage_for_stw`,
and those are exactly the peers the discharge now PINS. A pinned peer's objects
cannot move, so — the argument went — its frames need no rewritability proof.

That argument is false, and the class says so in the least ambiguous way
available.

### The measurement

One binary, `CRATONVM_XT_PINNED_PEER_DEPTH` the only difference, no debug I/O:

| arm | outcome |
|---|---|
| credit ON, 4 runs | **SIGSEGV at 138 s, 218 s, 95 s**; one reached the 600 s cap |
| credit OFF, 3 runs | no crash — 3600 s, 601 s, 600 s |

All three crashes at the SAME pc, inside compiled code. `accounted=true` on 10
of 35 cycles, so relocation ran where it used to refuse and a compiled frame
then used a pointer that had moved. On `TestMultiThread` the same credit is
clean and moves `relocation_on_proven_jit` 3 → 5 — a smoke test is not a
verdict here.

### Why: a JIT frame's oops are not on the stack the pin covers

`helper_window_pass` scans a frozen peer's register file and
`[rsp, stack_base)`. It contained no mention of the shadow stack — and the
shadow stack is where a JIT frame publishes its oops. It is a per-thread heap
`Box<[usize]>`, a separate allocation.

| peer state | shadow stack scanned | remapped |
|---|---|---|
| initiator | yes (`collect_roots`) | n/a |
| cooperatively parked | yes (`root_snapshot`) | yes, on resume |
| **blocked** | **no** | **no** — `apply_pending_blocked_fixups` never calls `shadow_stack.remap` |

So a blocked peer's shadow-stack oops were unpinned, unremapped and possibly
unmarked. `ShadowStack`'s own safety comment states the collector reads another
thread's "only after that thread has **parked**"; a blocked peer never parks.

### This qualifies the 2026-09-02 discharge entry above

That entry reports the helper-window discharge as sound on the strength of
`0 NPE`. The measurement was real; the conclusion was too broad. The discharge
was safe **in conjunction with** `cross-thread-jit-peer` still refusing, which
held `relocation_on_proven_jit` at 2 for a whole class run — the pin was barely
exercised. Remove the backstop and it is exercised properly, and it fails. A
near-zero relocation count is weak evidence for a safety claim, not vindication
of one.

### Two defects found in the credit itself

Both real, both fixed, neither shown to BE the crash:

- a recycled OS tid inherited a dead thread's published depth (the entry
  outlives its owner and a recycled thread that never enters JIT never
  overwrites it) — slots are now dropped by a TLS guard at thread exit and
  reset on re-registration;
- `publish_self_jit_depth` used `with`, which PANICS on a destroyed
  thread-local, and `pop_jit_entry` can run during teardown — now `try_with`.

### 2026-09-03 -- IT WORKS, and it is blocked on an open dev defect

On merged dev (box/unbox intrinsic now default-OFF), credit + shadow scan,
1500 s cap, 3 runs, against 2 discharge-only controls on the SAME binary:

| | credit + shadow scan | discharge only (control) |
|---|---|---|
| best run | **`actual: 99978`** | did not complete |
| ref-array OOM | **0** | **20000** |
| NPE | 0 | 0 |
| `relocation_skipped_jit` | **3** (was 877) | — |
| `relocation_on_proven_jit` | **22** (was 2) | — |
| compaction | 22 cycles, **519932 objects relocated** | — |
| outcome | 1 completed, 2 SIGSEGV (185 s, 100 s) | 2 x rc=124 at the cap |

99978 of 100000 with **zero** OutOfMemoryError is the best result this class has
produced -- better than the unsafe `ASSUME_REWRITABLE` bypass (99952 with 48
NPEs), and obtained by satisfying the obligation rather than skipping it. The
fragmentation diagnosis is right and the mechanism now demonstrably clears it.

It still SIGSEGVs 2 of 3, and the cause is very likely NOT this accounting.

> **REFUTED, and re-measure this section (2026-09-04).** What follows rests on
> the box/unbox page's reading of its own switch table, and that reading was
> wrong. The mechanism was never an unnamed oop-map root: `ZgcRealHeap`'s
> relocation slides were copying into arena granules
> `Arena::decommit_free_blocks` had already returned to the OS, and both slides
> now commit their destination first. See
> `zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`.
>
> This section's own evidence points the same way and is worth re-reading with
> that in hand: the fault signature recorded below is `rdi` page-aligned at the
> fault, which is a `memmove` running off the end of a mapping, not a read
> through a stale reference. `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` removed
> it by removing the slide, not by fixing a root.
>
> **The arms below have not been re-run on a tree carrying that fix.** Whoever
> takes this page next starts there, and the cheapest discriminator is
> `CRATONVM_GC_RESERVE=0`: if it removes the SIGSEGV on the pre-fix binary, this
> section's crash is the same defect and closes with it.

The reading this section was written under, kept because the argument it
supports is still the one to re-test: that page established, with
`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` as the narrow switch (0/3), that
relocation under LIVE COMPILED FRAMES was implicated, and linked it to
`bug-h2-testrandommapops-small-heap-corruption-20260829.md`, "hunting an
unnamed root in a compiled frame for days".

Enabling relocation under live JIT frames is precisely and only what this credit
does. So it is a powerful EXPOSER of that defect, and no arm on this workload
can separate the two: every switch that removes the crash (`PUBLISH_ONLY`,
credit-off, `RELOCATE_UNDER_PROVEN_JIT=0`) also removes relocation-under-JIT.

**New fact for that page: the box/unbox intrinsic is not required.** Every run
above had it DISABLED (merged-dev default; the enable flag appears nowhere in
the logs). That page's crash needed both relocation and the intrinsic; this one
needs only relocation under live JIT frames. So there is a second, independent
trigger of the same shape -- which supports "an unnamed root in a compiled
frame" over any account that makes the box/unbox sequence itself the mechanism.

**Status: this work is BLOCKED on that defect, not refuted by it.** When the
unnamed root is found and fixed, re-run these arms; if the SIGSEGVs go, the
credit ships and takes the class from 98304 to ~99978 with no OOMs.

### 2026-09-03 (later): reproduced on a THIRD binary; the wide-locals fix does not help

Rebuilt on dev with `fix/jit-precise-oop-maps-wide-locals-20260903` included
(`2632fb2c1` confirmed an ancestor):

| | credit + shadow scan | discharge-only control |
|---|---|---|
| best run | **`actual: 99977`**, 0 OOM, 0 NPE | did not complete in 1500 s, 14040 OOM |
| `relocation_on_proven_jit` | 24 | — |
| shadow scan | 104 windows, 4309 roots, 0 untrusted | — |
| SIGSEGV | 2 / 3 (103 s, 119 s) | 0 |

Third independent binary, same two facts: the credit clears the fragmentation
-- 99977-99978 with ZERO OutOfMemoryError against a control that cannot finish
-- and it still trips the open relocation defect on 2 runs in 3.

"Methods above 64 locals had no precise oop maps at all" was the closest
published candidate for the unnamed root, and fixing it changes nothing here.
Ruled out, and recorded on that page too.

### 2026-09-03: the guard PRICED -- it removes the crash by restoring the OOM

`CRATONVM_ZGC_JIT_BLANKET_REFUSAL=1` makes a live compiled frame refuse
relocation on its own, without consulting the coverage proof -- the rule
`gen_heap` and `g1` both apply and this collector replaced on 2026-08-21.
`relocate_stw` already computed the term (`compiled_frames_live`); it simply was
not a refusal.

One binary, the flag the only difference, 1500 s cap:

| arm | exit | ref-array OOM | `actual` |
|---|---|---|---|
| guard ON, run 1 | timeout 1501 s | 8344 | — |
| guard ON, run 2 | timeout 1500 s | 9382 | — |
| guard ON, run 3 | timeout 1500 s | 10118 | — |
| guard ON, run 4 | timeout 1500 s | 11004 | — |
| guard OFF, run 1 | **SIGSEGV** 306 s | 0 | — |
| guard OFF, run 2 | completed 462 s | **0** | **99966** |

Guard ON: **0 SIGSEGV in 4**, mean **9712** OOMs, NOT ONE RUN COMPLETED. The
four counts rise monotonically (8344 -> 11004), so they are FLOORS -- what
accumulated before the cap killed each run, not totals.

The comparison is generous to the guard: its OOMs are what 1500 s produced,
while the unguarded arm reached zero OOMs and finished in 462 s. There is no
duration at which the unguarded arm produces one.

So the guard WORKS as a crash fix, and it is not shippable: it trades the
SIGSEGV for the exact `OutOfMemoryError` this page exists to remove. For scale,
the same-dev discharge-only control logs 14040 OOMs and also does not complete,
so the guard is better than having no credit at all and an order of magnitude
worse than the credit running unguarded.

That prices the trade and closes the question. The three-way choice is now
explicit:

1. **credit, unguarded** -- 99966-99978, ZERO OOM, completes in ~460 s, and
   SIGSEGVs about 2 runs in 3 on the open codegen defect;
2. **credit + guard** -- no crash, ~8-9 k OOMs, never completes;
3. **neither** -- no crash, 14 k OOMs, never completes.

None ships. (1) is the only one that solves the page, and it is blocked on
naming every home the register allocator creates -- see the box/unbox page for
the identification, the three failed repairs, and what remains.

### Attribution closed 2026-09-03: it is dev's relocation defect, and the oracle does not see it

| arm | SIGSEGV |
|---|---|
| credit + shadow scan | 2 / 3 (185 s, 100 s) |
| credit + shadow scan + `CRATONVM_ZGC_RELOCATE=0` | **0 / 3** |
| `PUBLISH_ONLY` (plumbing, no credit) | 0 / 3 |
| discharge only (control) | 0 / 3 |

Relocation is REQUIRED -- the same 0/3 that
`zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md` measured on that
switch. And the fault signature matches that page's: `rdi` page-aligned at the
fault (`0x232ECD30000`, `0x28DEA7B0000`, `0x1CA01BB0000`), which that page reads
as "a read through a reference into a page the collector has already vacated".

So this is that defect, reached by a second route. The credit is an exposer.

**A negative worth recording for the root-cause hunt.** The obvious instrument
does not find it. `CRATONVM_DBG_VERIFY_OOP_MAPS=1` REFUTES the codegen's
`fully_oop_covered` bit within the first two log lines on both this class and
`TestMultiThread` -- but its own corroborating verdict is empty:

```
oop-map audit: frames=2712711 words=47361242 never_mapped=2804541
               (while_covered=2153089 of 2630854 claiming;
                while_shadow_covered=1511211 of 2085086 claiming)
oop-map audit: verifier_oop=0 verifier_not_oop=1222475 verifier_unknown=1582066
               name_index=(1237 names, 0 collisions)
```

`verifier_oop=0` with a POPULATED name index (1237 names): the class files say
not one of 2.8 M never-mapped words is a reference. The raw never-mapped count
is the documented false positive -- primitives whose bits land on a live object
header -- and the audit's own comment says only `verifier_oop` is a lead.

Two traps this cost, worth not repeating:

* the per-hit `[VERIFY-OOP-MAPS]` lines are capped at `STEP3_LOG_CAP` (64), so a
  slot-class breakdown taken from the log is a sample of the first 64, NOT a
  distribution over the 2.8 M. Read the audit summary for the population.
* both audit lines print only at NORMAL exit, so a run that SIGSEGVs (`rc=139`)
  or is killed at the cap (`rc=124`) yields no verdict at all. To get one on a
  crashing arm the workload has to be made to exit -- or the verdict has to move
  into the per-hit line.

### Superseded: the retraction that preceded this measurement
### Superseded: the retraction that preceded this measurement

The conclusion below is withdrawn. It is not known to be wrong; it is not
supported by the evidence that was offered for it.

`zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`
landed on dev the same day: the box/unbox intrinsic SIGSEGVs under a relocating
collector, **11 of 11 runs, 25-183 s**, and it takes BOTH relocation and that
intrinsic -- neither alone. Dev flipped the intrinsic to opt-in as the
mitigation.

Every binary crashed on this page was built before that flip, so it carried the
intrinsic default-ON. And the ONLY effect of the pinned-peer credit is to let
relocation happen on cycles that previously refused. So "relocation crashes this
workload" was already true on dev, independently of the credit, and the
`PUBLISH_ONLY` control cannot separate the two: suppressing the decision
suppresses relocation, which suppresses the known defect too. Same for
`CRATONVM_ZGC_RELOCATE=0`.

What survives the retraction:

* the relocation DECISION is what crashes -- still true, and still what the
  arms show;
* the plumbing is innocent (`PUBLISH_ONLY` 0/3, scan-only 0/2);
* the shadow stack is a genuinely uncovered channel, 2090 refs on a 216 s run
  -- an independent measurement that owes nothing to the crash;
* the three implementation defects found by auditing the scan.

What does not survive: any claim about whether a blocked peer CAN be discharged
by pinning. Re-running on merged dev with the intrinsic default-off.

The lesson is the cheap one: before concluding that a crash under your feature
indicts your feature, ask what else on dev crashes under exactly the condition
your feature creates. `git log origin/dev -- docs/known-issues/` would have
found it in seconds.

### SUPERSEDED CONCLUSION (read the retraction above first)

Settled by measurement. Every arm below is the same binary with flags as the
only difference.

| arm | crashes |
|---|---|
| credit ON, no shadow scan | 3/4, then 3/3, then 2/2 across two binaries |
| credit ON, shadow scan ON (engaged, 2090 roots recovered) | 3/3 |
| credit ON but `PUBLISH_ONLY` -- all plumbing, no decision | **0/3** |
| shadow scan only, no credit | **0/2** |
| plain control (discharge only) | 0/3, including one 3600 s run |

The plumbing is innocent: the TLS `Arc`, the per-tid registry, the deposits and
the shadow scan all run clean for 600 s when the DECISION is suppressed. What
crashes is crediting a pinned blocked peer and letting relocation proceed.

Three channels are now covered -- register file, `[rsp, stack_base)`, and the
shadow stack -- and it still crashes. There is at least a fourth
(`apply_pending_blocked_fixups` also skips `remap_register_image_words` and
`remap_active_jit_frames`), but the pattern is the point: the architecture makes
a peer's JIT state consistent BY THE PEER ITSELF -- park, publish, remap on
resume -- and a blocked peer sits outside that by design. Retrofitting
immobility means enumerating every channel the design never required anyone else
to know about, and being wrong once is a SIGSEGV.

**So `cross-thread-jit-peer` stays.** Discharging it needs REWRITABILITY, not
immobility: make a blocked peer remap its own JIT frames, register image and
shadow stack when it wakes, which is a substantial change to the blocked-wake
path and should be scoped as its own piece of work.

### Independent finding: a blocked peer's shadow stack is never scanned for ROOTS

Worth separating from the failed credit. `helper_window_pass` contributes a
blocked peer's registers and stack to the root set on every cycle it runs, but
never its shadow stack -- and `collect_roots` scans only the initiator's while a
parked peer publishes its own. On a 216 s `TestMultiThread` run the new scan
found `sh_windows=61 sh_slots=2376 sh_roots=2090`: 2090 heap references in
blocked peers' shadow stacks that nothing else was scanning.

Whether any of those 2090 is reachable ONLY through the shadow stack is NOT
established here -- the same values usually also sit in a stack slot or
register. But precise publication exists precisely because they sometimes do
not, so this is a real hole to close on its own merits.
`CRATONVM_XT_PEER_SHADOW_SCAN=1` closes it and measured clean (0/2 crashes,
0 NPE, 0 OOM on `TestMultiThread`). It stays default-OFF pending a run that
either demonstrates the loss or rules it out.

### The route that was tried and rejected

`CRATONVM_XT_PEER_SHADOW_SCAN=1` gives the owner-published route to the missing
coverage: each thread publishes its own `ShadowStack` ADDRESS (authoritative,
no frame→`CompiledMethod` attribution, stable for the thread's lifetime) and the
initiator reads `base`/`top` from it while the peer is blocked and therefore
stable. Every field is validated before dereference, and an untrusted window
REFUSES the pin rather than claiming coverage — that direction costs compaction,
not correctness.

The initiator cannot instead recover the window from the peer's frames:
`shadow_window_from_frame` trusts a frame only when its cached `JvmThread` is
the current thread's, and mis-attributing a `CompiledMethod` to a conservatively
found frame is what SIGSEGV'd the band verifier on a `base` of
`0x5555_0000_0004`.

If that does not close it, pinning is the wrong instrument for blocked peers and
the remaining route is to make a blocked peer remap its own JIT state on wake —
rewritability rather than immobility — which is a much larger change to the
blocked-wake path.

### Acceptance test, unchanged

`relocation_on_proven_jit > 0` with 0 OOM **and** 0 NPE **and** no crash,
together. Not whether the `cross-thread-jit-peer` label disappears: the census is
first-wins, so discharging one term merely exposes the next, and this is the same
style of depth accounting that produced a false positive earlier on this page.

### Bisect levers

- `CRATONVM_XT_PINNED_PEER_DEPTH=1` — the credit (default OFF).
- `CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY=1` — publish and deposit but credit
  nothing; separates the publisher from the decision, which one flag otherwise
  conflates.
- `CRATONVM_XT_PEER_SHADOW_SCAN=1` — the candidate fix (default OFF).
- `CRATONVM_DBG_XT_COVERAGE=1` prints `peer_depth= proven= pinned= accounted=`.
  It is an ENGAGEMENT counter: `pinned=0` throughout means the credit never
  engaged and everything downstream is vacuous. Do NOT leave it on while
  measuring outcomes — 25113 lines of stderr pushed a 997 s control to the
  3600 s cap and inflated its ref-array OOMs from 834 to 48132, because this
  class's `FOR UPDATE WAIT 0.5` turns added latency into failures.

## Related

- `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of: the whole fragmentation diagnosis, the four
  repairs, and the counters to read.
- `docs/known-issues/gc/zgc-arena-fragmentation-occurrences-to-reverify-20260829.md`
  — the Spring Framework class with the same exponential-retry shape, and the
  re-verification matrix both are waiting for.
