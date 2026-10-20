# Generational old generation, majors and the concurrent cycle: review residuals (gcj j2/og2)

> **STATUS (2026-10-19, gcj j3/og3): PARTLY FIXED IN CODE, PENDING BUILD/PROBE.** Fixed: N2, N3, O5, O6 (two of three items), O9 (the `is_marked` half), O10, C7 (the dedupe half); N4 = B2 and N5 = B3 built opt-in; N6 superseded (both halves of the Phase 0 drop built, opt-in). Still open, with this wave's findings: `gcj-j3og3-old-gen-review-residuals-20261019.md`.

> **STATUS (2026-10-19, gcj j2/og2): OPEN -- found by reading; nothing here is measured.** Generational only unless an item says otherwise. Carries the still-open items of `gcj-j1og-concurrent-cycle-and-old-gen-review-residuals-20261019.md` (their text is there; one line each here) and adds this wave's findings (N1-N6). Line numbers are against branch `gcj2-og2`; search the names.

## Carried from the j1/og page, still open

- **C1** (MEDIUM-HIGH perf): each concurrent sweep slice re-sorts the free list. Not cheap to close inside the slice: `OldGen::free` dirties the sorted cache, and `walk_objects_from`'s linear skip needs a prefix-max over nested blocks. Proposal `gcj-j2og2-proposal-concurrent-sweep-keeps-the-sorted-free-view-20261019.md`.
- **C4** (MEDIUM pause): the initial mark walks old gen into a `Vec` and builds two start sets from it.
- **C5** (MEDIUM perf): a stripe mutex per legacy slot also in the stop-the-world remark drain. Needs an `in_pause` argument threaded through `scan_object_into` and its callers.
- **C6** (LOW): cleanups. One of them, `finish_sweep_as`'s `bitmap.clear()` on a bitmap "about to be dropped", is left in on purpose: a test-reused marker (gci i13/cm item 1) is not dropped, and `initial_mark` clears it anyway, so the clear is cheap insurance.
- **C7** (LOW): dead finalizables retained and re-traced every concurrent cycle; `retained` accumulates duplicates.
- **O4, O5, O6, O9, O10** (LOW): as on the j1 page.
- **O7** (LOW, release hazard), refined: refusing a misaligned or short `release_unused_tail` span in release is NOT the fix it looks like. The span would stay counted in `used_bytes` with no header in it, and the next walk would break on it (a walk gap, so a compaction abandons). Close it by rounding INWARD (offset up and size down to 8, nothing pushed when under `HEADER_SIZE`), so only whole aligned bytes reach the free list, with the remainder written as a filler header. `free` needs the same.

## New this wave

**N1. FIXED IN CODE (this wave's C2 fix, follow-up commit): the reference-skip screen and the class-unload capture read headers inside the span they are given.** `gen_conc_reference_skip_candidates` (`gc_and_alloc.rs`) filters active `Reference` rows to `[old_base, old_base + span)` and then reads each row's header; `ClassUnloadTables::capture` filters loader rows the same way. Handing them the bitmap's new span (`growth_max`) would admit an address in the uncommitted headroom. Only `ConcurrentMarker::with_shared` takes `old_gen_mark_span()`; the other two keep `old_gen_info()`.

**N2. LOW (doc): `OldGen::resize_after_collection`'s safety list said "the card table and the concurrent marker's bitmap cover the RESERVATION and are not resized".** The card table does (`CardTable::new(base, growth_max)`); the marker's bitmap covered only the capacity at the driver's read until this wave's C2 fix. The premise is true now by default and false under `CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN=0`; it was never load-bearing for the shrink (a shrink lowers the committed end, not the capacity), so only the sentence was wrong. Reword when the kill switch is retired.

**N3. LOW: `OldGen::grow_in_place_to` reports a store that did not grow in place as "`OldGen::grow_after_refusal`: the store did not grow in place" for both callers** (since j1/og split it out, `grow_to_min_free_ratio` reaches it too). Name the function and pass the caller's label.

**N4. MEDIUM (parity): the old capacity never shrinks, so the young trigger cap is one-way.** Blocker B2 of `gcj-j2og2-free-ratio-growth-flip-blockers-20261019.md`; proposal `gcj-j2og2-proposal-old-capacity-shrinks-back-so-the-young-cede-is-reversible-20261019.md`.

**N5. LOW-MEDIUM (parity, observability): `Tenured Gen`'s pool `max` is the current capacity, so it moves with every growth.** Blocker B3 of the same page; cross-lane hunk X2 of `j2-og2-report.md`.

**N6. INFO: the cheap half of the Phase 0 drop does not exist.** The in-mark closures (and with them the escape verdict the in-place sweep reuses, `reuse_pre_close`) run only under the true-root seed, which is in-place only (`true_roots` requires `!compact` in `old_gen_gc_inner`). The compacting arm always runs Phase 0. See the STATUS line on `gcj-j1og-proposal-full-gc-drops-the-redundant-phase-0-closure-20261019.md`.

## How to verify

Perf items (C1, C4, C5, O5) on `GenR5W5ConcUnloadProbe`, `GenR4W4SteadyPromotionProbe 4 -Xmx512m` and `Gcj1OgFullGcCompactProbe 1000000 12 10`, as interleaved CPU pairs. O7 with a unit test that releases a 12-byte and a misaligned tail and walks the generation after. N3 by reading.
