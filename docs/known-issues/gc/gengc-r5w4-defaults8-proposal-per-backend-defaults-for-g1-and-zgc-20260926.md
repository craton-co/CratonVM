# Proposal: decide the share sizer and the HotSpot soft-ref inputs for G1 and ZGC, then collapse the per-backend defaults

*Filed 2026-09-26 by gen round 5, wave 4, lane `defaults8`
(`docs/internal/reviews/gengc-round5-w4-defaults8-20260926.md`). A direction,
not a finding. Nothing here was measured.*

- **Status:** OPEN (proposal, S to run, XS to apply).
- **Owner:** a G1 or ZGC round. The measurements touch only those backends.
- **Code:** `types/src/flags.rs` (`alloc_policy_defaults::{BackendDefault,
  TLAB_SHARE_SIZER, SOFTREF_HOTSPOT_LRU}`), `gc/src/vm_heap.rs`
  (`VmHeap::tlab_share_sizer_enabled`), `gc/src/reference.rs`
  (`ReferenceProcessor::new_for_heap`), `gc/src/tlab.rs` (the ladder arms
  `TLAB_SIZE_RETIRED` / `TLAB_WASTE_SHRINK`).

## Where things stand

Gen r5w4 made two backend-agnostic switches default ON for the Generational
heap only. They are `CRATONVM_TLAB_SHARE_SIZER` and
`CRATONVM_SOFTREF_HOTSPOT_LRU`. Their A/B was taken on Generational alone,
and G1 and ZGC were out of the round's scope. The mechanism is a two-field
`BackendDefault { generational, other }`, resolved per VM against
`VmHeap::is_generational()`.

That leaves three costs:

1. **Two policies per switch.** A soft-reference-cache bug report now depends
   on the collector: the same program keeps a cache on `-XX:+UseGenerationalGC`
   and loses it on the default ZGC. HotSpot's `LRUMaxHeapPolicy` is
   collector-independent, and so is its TLAB sizing.
2. **Dead code that cannot be deleted.** The ladder arms (`TLAB_SIZE_RETIRED`,
   `TLAB_WASTE_SHRINK`, and `TlabPressureTracker`'s fill-time sizing that they
   modify) never run on a default Generational VM now. They still run on G1
   and ZGC, so they cannot be retired as protocol A intended ("if S flips, B and
   C are retired next round").
3. **A second resolution path.** Every per-backend switch needs its
   heap-aware read (`*_for(generational)`) and an `off_word`. The generated
   inventory also cannot render the default truthfully yet (see
   `../../internal/gc/gengc-r5w4-defaults8-flag-inventory-renders-per-backend-default-as-on-FIXED-20260928.md`).

## Proposal

Run the missing arm of protocol A: W6 of
`docs/internal/reviews/gengc-round5-w2-alloc6-20260926.md`, plus the soft-ref
probe, on the other two backends. Then either flip `other` or record the
reason not to.

| Run | Command (three interleaved reps, medians) | Flip `other = true` iff |
|---|---|---|
| S on G1 | `cratonvm -XX:+UseG1GC -Xmx512m --verbose:gc -cp tools/bench GenR4W2ParkAllocProbe 8 20000 64`, `GenR4W4SkewedShareProbe`, `GenR4W6AllocRateProbe` (at `-Xmx256m`); arms `CRATONVM_TLAB_SHARE_SIZER=0` / `=1` | `sum=` / `ok` lines identical; `[GC] tlab-waste: unused=` % not above the `=0` arm; the G1 young-pause count not higher; W5 medians within noise or better |
| S on ZGC | the same with no collector flag (ZGC is the default) | the same, with ZGC's cycle count in place of young pauses |
| soft-ref on G1 / ZGC | `cratonvm [-XX:+UseG1GC] -Xmx32m -cp tools/bench GenR5W2SoftRefIdleYoungProbe`, arms `=0` / `=1` | the `=1` arm prints `lost=0` `PASS` (HotSpot's lines), and a retention gauntlet (`MtChurnProbe`, `HashMapOnly`, `BinT 14` at `-Xmx256m`) shows no rise in peak heap occupancy past noise |

Apply:

- **Both backends flip:** set `other: true`. The `BackendDefault` then
  degenerates to a plain default-on arm. Turn the constant back into a
  `bool` parsed by `alloc_policy_switch`, and make the field a `bool` again
  (delete `tlab_share_sizer_for` / `softref_hotspot_lru_for`,
  `VmHeap::tlab_share_sizer_enabled`, `Tlab::arm_share_sizer` and
  `tlab_census_lines_for`; `ReferenceProcessor::new_for_heap` keeps only its
  policy argument). If `TLAB_SHARE_SIZER` flipped, delete the ladder arms
  and their pages
  (`gengc-alloc-tlab-sizer-is-blind-to-waste-DONE-20260929.md`,
  `gengc-alloc2-adaptive-tlab-sizer-is-bypassed-after-every-early-retire-DONE-20260929.md`).
- **One backend loses:** keep the per-backend default. Record the losing
  measurement on the constant, the way the Generational evidence is recorded
  now, so the split is a decision rather than a leftover.

## How to verify

After a flip: `types` `per_backend_switches_resolve_against_the_heap_family`
must be rewritten (or deleted, with the type). Then every arm of
`every_allocation_policy_arm_parses_against_its_one_default` covers both
switches. The `[GC] tlab-sizer:` line must print `sizer_on=true` on all three
backends with nothing set.
