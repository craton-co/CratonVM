# `TestCachedQueryResults` — a ZGC `OutOfMemoryError` LIVELOCK, thousands per run, not a single failure

## Status

**OPEN, and the chain is now traced to one frame — see §"2026-08-29 (second)".**
The `xt_cov=(accepted=0 refused=1730)` lead this page shipped with turned out to
be four measurements deep: the peers DO park, some of their own proofs return
false, the obligation is `UNPUBLISHED_FRAME_OOP` in 3 of 4, and six of the seven
unpublished words belong to a single frame whose safepoint-id slot holds the low
32 bits of a heap pointer.

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

### 4. What to do next, in order

1. **Decide which of two shapes the garbage id is**, because they are opposite
   repairs and the evidence above does not separate them:
   * **something wrote an oop into the reserved sp-id slot** (a codegen defect:
     a store whose offset lands in the reserved-locals tail), or
   * **`rbp` is wrong for this frame**, so `[rbp - sp_id_slot_off]` lands on a
     neighbouring slot that legitimately holds an oop (a frame-resolution
     defect, the same family as the 2026-08-26 "two innermost-frame mirrors were
     not moving together" fix on the parent page).

   The discriminator is cheap: print `sp_id_slot_off` and the whole reserved
   tail beside the id. If the pointer sits at exactly `sp_id_slot_off` the store
   is the bug; if the tail's OTHER slots also look shifted by one, the rbp is.
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
