# Proposal: the young copy, round three: what is left of the 3.7x on `GenR4W4EvacThroughputProbe`

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 13
> of 54).** None of items 1-5 is built as specified; two neighbours landed.
> Item 1 is designed in full on
> `gcd-d5q-proposal-known-zero-young-free-list-20260928.md` (same flag name).
> Item 3's sweep half landed opt-in as `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL`
> (gcd d5/q, `gen_heap.rs::adapt_sweep_trigger_to_pause`), not measured on d7.
> Item 4 is still open at `gen_heap.rs` (`dhdr.add_gc_flags(GC_FLAG_OLD_GEN)`,
> `h.set_gc_age(h.gc_age().saturating_add(1))`). The gap on d7,
> `GenR4W4EvacThroughputProbe -Xmx256m`, one run each: evac_throughput
> (`CRATONVM_DBG=gcpause`) `time_ms=23140`, w3_evac_default `time_ms=17853`,
> w3_evac_pinned_par (pinned copy + parallel + card seed) `time_ms=14052`, all
> `PASS ... corrupt=0`; HotSpot Serial about 3 s. **Gate:** per item, ABBA
> against the base on one host with the page's two commands: `PASS evac ...
> checksum=1021046735439613382 corrupt=0`, `BinT 14` `327670`, and `time_ms` /
> minor count medians lower beyond the A-vs-A band. **Size:** S (item 4, no
> flag), M (items 1-3 each), L (item 5).

*Filed 2026-09-27 by gen round 5, wave 6, lane `pin10`. A proposal: nothing
here is landed. It carries forward
`../../internal/gc/gengc-r5w3-evac7-proposal-young-copy-round-two-RETIRED-20260927.md` (none of whose
items landed) and adds what this wave's reading of the copy found.*

## Where the gap stands

The measured numbers, on the wave-5 build: `GenR4W4EvacThroughputProbe`
takes 14.5 s on the default arm and 11.2 s with the pinned copy and its
parallel arm on. HotSpot Serial takes 3 s. The checksum is the same in all
three.

This wave landed three bit-identical cuts in the copy itself:

- the destination mark is one folded store;
- the pinned rebuild's `live` sort merges runs;
- on a pinned cycle, a span copy's re-encounter no longer probes the map.

By reading they are worth a few percent of the pinned arm. They are not the
gap. What follows is ranked by expected effect, and each item names the file
that owns it. None of them was this lane's.

## 1. Stop zeroing reclaimed young memory twice (expected: the largest single mutator-side cost)

- **Where:** `gc/src/gen_heap.rs` `zero_young_hand_out` and
  `refill_tlab` / `refill_fragmentation_fallback` (TLAB lane), and
  `gc/src/arena.rs` (free-block metadata).
- **Today:** two pause paths zero every dead byte inside the pause:
  - a pinned in-place cycle (`finish_in_place_young_cycle`, which calls
    `zero_spans_parallel`);
  - the non-moving sweep.

  They then free-list those bytes, and every free-list hand-out is zeroed
  AGAIN on the mutator (`zero_young_hand_out`, with `bump_tail = false`).
  The zero-once skip covers only the bump tail. On the probe that is
  ~14-40 MB per cycle, twice.
- **Proposal:** tag each free block the pause zeroed with the pause's
  epoch; the tag is dropped by any write the arena does not own.
  `zero_young_hand_out` then skips a hand-out wholly inside a tagged block,
  after the same `known_zero_sample_is_clean` sample the bump tail uses.
- **Flag:** `CRATONVM_GEN_ZERO_ONCE_FREE_LIST` (opt-in).
- **Measure:** `young_zeroed_bytes` against `young_zero_skipped_bytes`, and
  `time_ms`, on both arms.

## 2. Survivor-overflow tenuring (expected: copy volume down by up to 3x on this probe)

- **Where:** `gc/src/gen_heap_tenuring.rs`, and `CopyPolicy`'s
  `promotion_age` where `collect_garbage_inner_with_pins` builds it.
- **Today:** a ring node survives several cycles and is copied young-to-young
  twice before `PROMOTION_AGE` 3 promotes it. HotSpot's survivor space
  overflows at `TargetSurvivorRatio` and tenures the oldest ages first.
- **Proposal:** when the previous cycle's survivor bytes exceed 50 % of the
  destination headroom, set the next cycle's `promotion_age` to the age at
  which the cumulative age table crosses that bound. The table is already
  kept when the census is on.
- **Flag:** `CRATONVM_GC_SURVIVOR_OVERFLOW_TENURE` (opt-in).
- **Measure:** `objects_copied` summed over the run, `objects_promoted`, the
  major count and `time_ms`.

## 3. The young trigger on the in-place arms (expected: cycles 130-170 down toward 34)

- **Where:** `needs_gc` and `young_gc_trigger_bytes` (sizing lane).
- **Today:** on the sweep and on the pinned arm, the trigger counts LIVE
  bytes (`used - free_list_bytes`). It is capped at the moving threshold
  (at most 50 % of the semi-space) by the pause goal, so with ~15 MB
  retained a cycle follows every ~15 MB of allocation. See the evac7 JIT-warm
  page, item 2.
- **Proposal:** on a cycle that did not copy into to-space (a sweep or a
  pinned cycle), cap the trigger by the semi-space's free bytes, not by the
  moving threshold. The moving threshold exists so that to-space can hold the
  survivors, and neither arm uses to-space.
- **Flag:** `CRATONVM_GEN_IN_PLACE_TRIGGER_FREE` (opt-in).
- **Measure:** the minor count, the `[gcpause]` median, `time_ms`.

## 4. Fold the sweep's remaining header RMWs (expected: small, bit-identical)

- **Where:** `gc/src/gen_heap.rs`, the sweep's in-place aging
  (`h.set_gc_age(h.gc_age().saturating_add(1))` at two sites, and `set_gc_age`
  in the selective-promotion age pass) and the selective promotion's
  destination (`dhdr.add_gc_flags(GC_FLAG_OLD_GEN)`).
- **Today:** a compare-exchange loop or a locked `fetch_or` per survivor.
  All mutators are stopped, and a promotion's destination is visible to no
  one.
- **Proposal:**
  - The promotion destination: store
    `ObjectHeader::mark_with_gc_flags(snapshot, GC_FLAG_OLD_GEN)` once, as
    `forward_object_impl` does since gen r5w6/pin10.
  - The in-place aging: keep an atomic, but make it a plain
    `store(mark_with_gc_age(m, age + 1))` from one relaxed load. It is safe
    because no mutator runs during the sweep. A concurrent marker that can
    set `GC_FLAG_MARKED` at the same time would need the CAS kept; check
    that first.
- **Flag:** none needed if the concurrent-marker question comes back "no";
  otherwise the CAS stays.
- **Measure:** the sweep's `[gcphase]` rows.

## 5. The pointer-map insert (expected: medium; design)

- **Where:** `forward_object_impl`, `gen_evac` `map_merge`, and every
  consumer of `GcResult::pointer_map`.
- **Today:** one hash insert per survivor. The parallel arm merges them on
  `par_workers` threads.
- **Proposal (design):** a young cycle's map is fully described by the
  forwarding words in from-space until from-space is reset. On the pinned
  arm from-space is never reset, and its vacated sources are zeroed by
  `finish_in_place_young_cycle`. So the map could be materialized lazily:
  1. hand the consumers a view that answers `get(old)` from `old`'s header
     while the vacated bytes are still intact;
  2. zero them only after the VM's remap (`update_all_roots`) has run.

  The identity entries of pinned and kept objects are the hard part.
  Reference processing reads "in the map" as "survived". A per-cycle side
  bitmap over from-space would answer that for them.
- **Measure first:** `map_merge` plus the serial insert share of
  `cheney_drain`, and the VM's `update_all_roots` time
  (`CRATONVM_DBG_ROOTFIXUP`). Only then decide.

## 6. The remaining round-two items (unchanged)

These are carried forward from the round-two page as written:

- the remap range filter;
- no OS thread spawns inside the pause (`young_mark.rs`, `par_extend_pairs`);
- streamed dirty-card roots;
- keeping the evacuated semi-space committed.

## How to verify any of them

The probe, on both arms, ABBA against the base on one host:

```
CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" \
  -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL=1 CRATONVM_GC_PAR_EVAC_CARD_SEED=1 \
  CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" \
  -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
```

Both must print
`PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`.
BinT 14 must still print `327670`. Compare `time_ms`, the minor count, and
the named rows. In-JVM timings swing ~3x between reps here, so take medians
of interleaved runs.
