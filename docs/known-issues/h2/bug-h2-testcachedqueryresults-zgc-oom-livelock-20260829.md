# `TestCachedQueryResults` — a ZGC `OutOfMemoryError` LIVELOCK, thousands per run, not a single failure

## Status

**OPEN, and the chain is now traced to one frame — see §"2026-08-29 (second)".**
The `xt_cov=(accepted=0 refused=1730)` lead this page shipped with turned out to
be four measurements deep: the peers DO park, some of their own proofs return
false, the obligation is `UNPUBLISHED_FRAME_OOP` in 3 of 4, and six of the seven
unpublished words belong to frames whose safepoint-id slot carries no usable id.
The discriminator then split THAT into two defects: **10 of 13 such frames have
`sp_id == 0` — they have not reached their first safepoint — and 3 have a heap
pointer sitting in the reserved slot.** It is not a shifted `rbp`.

Split out 2026-08-29 from
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`, which is
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

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of: the whole fragmentation diagnosis, the four
  repairs, and the counters to read.
- `docs/known-issues/gc/zgc-arena-fragmentation-occurrences-to-reverify-20260829.md`
  — the Spring Framework class with the same exponential-retry shape, and the
  re-verification matrix both are waiting for.
