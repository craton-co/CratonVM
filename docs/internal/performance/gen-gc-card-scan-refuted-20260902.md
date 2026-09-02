# The dirty-card scan is not where the generational pause is — two probes that say so

Slug: `gen-gc-card-scan-refuted` · 2026-09-02
Refutes two of the eleven findings in the generational review, and corrects a
figure `gen-gc-five-20260902.md` left standing in its own residual list.

---

## VERDICT

Two ranked findings said the old→young dirty-card scan was a growing cost:

* **"Card precision is per OBJECT, not per slot."** The barrier marks the card
  of the receiver's base, so one store into a tenured 1M-element `Object[]`
  makes the scan re-read all million elements to rediscover one edge.
* **"Old gen has no object-start index."** `walk_objects_in_card_ranges`
  reconstructs boundaries by striding header-to-header from the start of each
  allocated region, so a dirty card near the end of a large old generation
  costs a walk over everything before it.

Both are true as *descriptions of the code*. **Neither is worth building**, on
every shape that can be constructed to expose it:

| shape | what is scaled | `refinement_ms` per collection |
|---|---|---|
| `OldGenRsetProbe 19 700 16` | large tenured tree, **zero** edges | **0.48 ms** |
| `OldGenSparseCardProbe`, depth 16 → 19 | tenured tree 8×, **~1 dirty card/cycle** | **0.61 → 0.49 → 0.52 ms** |
| `OldGenWideArrayCardProbe`, 64k → 1M | tenured array length **16×**, 4 stores/round | **0.92 → 0.77 → 0.93 ms** |
| `OldToYoungEdgeProbe`, 60k → 480k nodes | edges 125k → 2.88M | 0.94 → 5.10 → 16.05 ms (**34–50 ns/edge, flat**) |

The scan costs **0.5–1.0 ms of a ~105 ms pause — under 1%** — and it does not
grow with the tenured set, with array length, or with anything except the
number of edges it legitimately delivers, at a flat per-edge rate.

---

## The figure that made them look urgent, and where it came from

`gen-gc-five` listed, in its own "not established" section:

> `OldToYoungEdgeProbe` shows the item-7 shape … with dirty cards near the end
> of a large old gen, `scan_dirty_cards` is O(old objects) (137–161 ms spikes
> in the r1 A/B's `PAR_EVAC=0` arm).

Those spikes were real and they are **gone**, for a reason that has nothing to
do with the card scan. That arm ran with the pause-goal loop's shrink
pathology live: pauses over the goal halved the nursery repeatedly, which
promoted far more and ran 203–219 collections instead of 29. A bloated old
generation and 7× the collections is what produced a 161 ms refinement pass.

`gen-gc-five`'s own item-1 residual fixed that — a halving is now a trial that
reverts when it does not move the pause. On the merged binary the same probe
reports `refinement_ms=13.978 passes=29`, i.e. **0.48 ms per collection**, with
`dirty_scanned=0`.

**A cost attributed to one mechanism was a symptom of another**, and the
attribution survived only as long as nobody re-measured it after the unrelated
fix landed. That is the finding worth keeping from this page.

---

## The two probes, and why the existing ones could not decide it

Neither existing probe can produce the shape these findings are about:

* `OldGenRsetProbe` retains a large tree and never stores into it, so the
  dirty set is **empty** and `scan_dirty_cards_inner` returns before walking
  anything. Its scan cost is a floor, not a measurement of the walk.
* `OldToYoungEdgeProbe` stores into **every** retained node, so its cost is
  dominated by the edges it legitimately delivers. Measured across a 23×
  growth in edges the per-edge rate is flat at 34–50 ns, which is the scan
  doing its job, not overhead.

So two were added, and both verify every edge they create rather than only
timing it:

**`bench/OldGenSparseCardProbe`** — a large tenured tree with a *handful* of
old→young edges per collection, the write targets chosen down the right spine
so their cards sit late in the old generation. Holding the dirty-card count
fixed at ~1 per cycle and scaling only the retained tree is what separates the
O(old objects) prefix walk from the O(edges) term. Measured: 30–31 dirty cards
over 33–34 collections, refinement **flat at 0.49–0.61 ms** across retained
trees of depth 16 → 19.

**`bench/OldGenWideArrayCardProbe`** — one large tenured reference array with a
few element stores per round, which is the worst case for per-object card
granularity. Holding the store count fixed and scaling only the array length
is what would expose a whole-array rescan. Measured: refinement **flat at
0.77–0.93 ms** across 65,536 → 1,048,576 elements.

The shape is genuine, not vacuous: under `CRATONVM_GC_VERIFY_RSET=1` the wide
array run reports `edges=5777 missing=0 seeded=5777` on every one of 16
verified collections, so the tenured structure really does hold young
references and the card machinery really does deliver them.

---

## What this decides

**Do not build slot-precise carding or an old-gen object-start index for
performance.** Both would touch the three places this tree's own history says
lost-edge and walk-desync defects come from — every reference store in the VM,
the JIT's inline post-barrier, and the old-generation walker — to attack under
1% of a pause.

The remaining ranked findings the review left open are unaffected and are
where the next look should go:

* **Compiled reference stores still pay a full helper call under
  `-XX:+UseGenerationalGC`.** `publish_jit_ref_store_plan` is called only from
  `zgc.rs`; Generational cannot express its post-barrier as an age floor, so
  `ref_store_gates()` declines and the inline fast path is dead there. This is
  mutator throughput on the hottest path in pointer-chasing code, and it is
  unmeasured.
* **Tenuring is a fixed `PROMOTION_AGE = 3` plus a binary force-promote-all**,
  with no age histogram and no survivor target.

## Not established

* These are all single-host measurements on a shared 32-core Windows box. What
  they establish is a **ratio and a scaling shape**, not this collector's
  speed — the flatness across 8× and 16× growths is the claim, not the
  absolute milliseconds.
* `OldGenWideArrayCardProbe`'s churn tree also promotes, so its edge count is
  larger than its own stores. That does not affect the length-scaling result,
  which is what it was built for, but it means the probe is not a clean
  per-store measurement.
* Both probes drive the NON-MOVING sweep on this host (`site=non-moving` in
  the verifier output), because JIT frames are live. The card scan runs on
  both paths; a moving-path repeat is owed if the question is ever reopened.
