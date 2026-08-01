# `MVStore.readPageFromCache` — `java.lang.Object cannot be cast to Page` under sustained non-moving young sweeps

## Status
**OPEN.** Pre-existing, reproducible, silent heap corruption. Not caused by
— and not fixed by — the old-gen coalescing change committed alongside this
doc (`7303483521`); measured explicitly, see *Not the coalescer* below.

This is the residual recorded as "one unexplained repeat failure" in
[`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md).
It now has a repro that hits in **12–40 minutes at roughly a 40 % rate**,
instead of once in nine hours.

## Severity
**HIGH** — silent. No guard fires: zero `gen_heap::set_field`/`get_field`
out-of-bounds hits, zero `read_slot: corrupt Value cell` reports, no
`SIGSEGV`. The first observable symptom is application-level nonsense.

## Symptom
```
java.lang.ClassCastException: java.lang.Object cannot be cast to org.h2.mvstore.Page
    at org/h2/mvstore/FileStore.readPageFromCache(FileStore.java:2089)
    at org/h2/mvstore/FileStore.readPage(FileStore.java:1987)
    at org/h2/mvstore/MVStore.readPage(MVStore.java:1158)
    at org/h2/mvstore/MVMap.readPage(MVMap.java:632)
    at org/h2/mvstore/Page$NonLeaf.getChildPage(Page.java:1178)
    ...
    at org/h2/mvstore/MVMap.get(MVMap.java:417)
```

A reference that should point at an `org.h2.mvstore.Page` reads back as a
bare `java.lang.Object`. `java.lang.Object` is `ClassId(0)` — the ALL-ZERO
header shape. The non-moving young sweep **zeroes every span it reclaims**
(`young_mark::zero_spans_parallel`) before publishing it to the free list,
so "a live reference now points at a zeroed header" is the signature of an
object that was swept while still reachable: a **premature reclamation**,
i.e. a marking/root-coverage gap in the non-moving young sweep.

## Repro

```bash
cd apps/h2database/h2
CRATONVM_NO_MOVING_YOUNG=1 <cratonvm-bin> \
  --java-home /home/victor/jdk25 --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.store.TestMVStoreCachePerformance
```

`CRATONVM_NO_MOVING_YOUNG=1` **with the JIT on** is the whole trick. It
forces `divert_non_moving` true on every cycle that has conservative JIT
roots, so the workload runs the non-moving young sweep continuously
instead of occasionally. That is the same regime the
`xt-helper-window-conservative-scan` coverage fallback produces naturally
when 10–100 reader threads sit blocked in a native read under a compiled
frame — just held permanently instead of intermittently.

Do **not** add `--nojit`: with no JIT frames there are no conservative
roots, `divert_non_moving` stays false, and the collector takes the moving
Cheney path regardless of the flag. That configuration does not reproduce.

Failures land in rounds 3–4 (`testCache(10, …)`), typically 730–1750 s.

## Observed rate

18 runs, 2026-07-31/08-01, Azure box, `--Xmx 1g`, JIT on,
`CRATONVM_NO_MOVING_YOUNG=1`:

| arm | runs | CCE failures |
| --- | --- | --- |
| old-gen coalescing ON | 8 | 4 |
| old-gen coalescing OFF | 8 | 3 |
| pristine `origin/dev` binary (no coalescing code at all) | 2 | 1 |

**~40 % overall.** Every failure carries exactly 4 `cannot be cast`
lines (one per reader thread that trips over the same corrupted page).

### Not the coalescer
The first two samples happened to be a coalescing-ON failure against a
coalescing-OFF pass, which looks alarming. It was small-sample noise:
4/8 versus 4/10 across the full set (Fisher exact p ≈ 1.0), and the
**pristine `origin/dev` binary, which contains none of the coalescing
code, reproduces the identical signature**. The two are independent.

## Where to look

The non-moving young sweep marks from the full root set plus conservative
JIT roots, and the documented safe direction is over-retention — a
conservative false positive can only keep a dead object alive, never free
a live one. Something in that chain is under-marking under sustained
multi-threaded load. Candidates, in rough order of suspicion:

1. **Cross-thread root coverage.** 10–100 mutators, most blocked in a
   native `read` under a compiled frame. `xt_root_scan`'s helper-window
   pass scans a blocked peer's register file + `[rsp, region_end)`
   conservatively — but only for peers in `blocked_os_tids`, and only
   while `arm_slot`/`send_takeover_signal`/`wait_for_response(20ms)` all
   succeed. Any peer that misses that 20 ms window is scanned by nothing.
   `slot.clear()` on timeout silently drops the peer.
2. **Side-marked survivors.** The sweep's `side_marked_survivor` arm keeps
   an object without touching its header; the interaction between that set
   and the disposition walk is where the identity-map fixes have
   repeatedly landed.
3. **Selective promotion.** Survivors evacuated young→old mid-sweep, with
   the `evac_map` remap arriving after some consumer already read the old
   address.

The first suspicion is directly testable: count peers examined versus
peers that reached `STATE_PARKED` (`[xt-jit-roots] linux helper-window
pass: examined N blocked peer(s), M window(s)`) and see whether the gap
correlates with the failing runs. `CRATONVM_DBG_XT_JIT_ROOTS` prints it.

## Related
- Same family as [`reference_fork6_gcstress_jitfree_corruption`] and the
  OSR non-moving-sweep corruptor — all "the non-moving young sweep freed
  something live".
- The `xt-helper-window` fallback that produces this regime naturally is
  *correct* (a blocked peer's JIT-frame oops genuinely cannot be
  rewritten); it should not be weakened to avoid this bug.
- `7303483521` fixes the old-gen **fragmentation** consequence of the same
  regime. Unrelated cause, unrelated fix.
