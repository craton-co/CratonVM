# `TestCachedQueryResults` — a ZGC `OutOfMemoryError` LIVELOCK, thousands per run, not a single failure

## Status

**OPEN, split out 2026-08-29** from
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

## What to do first

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
