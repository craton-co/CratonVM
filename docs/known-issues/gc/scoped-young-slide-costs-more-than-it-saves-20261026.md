# The nursery-scoped young slide costs more than it saves (t1 wave 8, lane ZT3GN)

Status: OPEN (perf, generational ZGC, opt-in path only). Switch: `CRATONVM_ZGC_GEN_YOUNG_EVACUATE`
(default OFF).

> **gkr status (2026-10-28, branch `gc-known-issues-retire`): the remembered-set part is FIXED, the slide
> still loses; the switch stays OFF (opt-in), the page stays open.** Host 176.57.150.136 (4 vCPU, shared,
> load 7-17), same command, 3 interleaved reps per binary; checksum `-2647901454014 ok=true` on every run,
> `missed_rewrites=0` under `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1`.
>
> - **Found:** the scope's phase marks (`ye_age` / `ye_rset` / `ye_cand` / `ye_select`, diagnostic)
>   put 24 of the ~28 ms of a scoped slide's `selector_us` in the remembered-set registration loop. It
>   allocated a 32 KiB set for every aged page on every slide, for the next young cycle's
>   `young_extra_roots` to drop because the set held no card. The card barrier registers a page on
>   its first store (`card_object`), so a scoped slide now registers nothing. Test:
>   `t1y4_a_scoped_slide_registers_no_uncarded_page`, red with the loop forced on (pages
>   `[2, 4, 1, 3]` registered with no card), green with the fix. After it, `ye_rset_us=0` and the
>   relocate phases of a scoped slide total ~0.5 ms.
> - **What is left of the slide's cost is the scope build**, `[zgc-young-evac-scope]` `build_us` median
>   7.2 ms (b4). Almost all of it is the young walk below the scope floor
>   (`[zgc-young-evac-scope-parts]` `young_us` 9.2-12.6 ms, even with `young=0`). It reads the
>   registry's start and old bitmap words over the whole `[base, lo)`, ~690-930 MB of old space a
>   slide. Masking each word before visiting its bits (b4) did not move it, so the word reads are the
>   cost. The fix is a summary of where young bases below the floor can be (per page, kept by the
>   allocator and the sweep), so the walk visits only those pages.
> - **Rows** (per-minor pause, ms): b3 W 31.6 / 46.0 / 44.3, WE 57.0 / 74.7 / 72.4; b4 W 52.8 / 76.0 /
>   70.1, WE 48.8 / 96.8 / 85.6. The pass rule (WE at or below W) still fails. The sweep and mark that the
>   slide should save swing more between runs than the slide costs, so this host cannot resolve a
>   gain under ~20 %.

> **t1 status (2026-10-27, orchestrator): MEASURED -- the wave-10 fixes help, the slide still loses; the
> switch stays OFF (opt-in), the page stays open.** Host 64.177.43.150, binary cvm-w11 (waves 9-10),
> `taskset -c 0-7 ... --verbose:gc -Xmx1g Zr1GnGenShape 64 400 200000 5 8 64`, generational ZGC, 3 reps
> each, checksum `-2647901454014 ok=true` and `missed_rewrites=0` in every run:
>
> | arm | minor pause per cycle (ms, 21 minors) | wall (s) |
> |---|---|---|
> | W (no slide) | 21.6 / 22.7 / 21.5 | 3.91 / 4.12 / 3.75 |
> | WE (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE=1`, all three fixes) | 41.9 / 40.7 / 40.9 | 4.31 / 4.41 / 4.31 |
> | WE, `..._SCOPED_PINS=0` | 39.7 / 41.9 / 40.1 | 4.24 / 4.29 / 4.63 |
> | WE, `..._LOCAL_INSTALL=0` | 48.2 / 46.8 / 46.8 | 4.54 / 4.44 / 4.48 |
> | WE, `..._DEFER_VACATED_ZERO=0` | 49.1 / 48.2 / 49.5 | 4.46 / 4.77 / 4.45 |
> | WE, `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1` (1 rep) | 109.5 | 5.96 |
>
> Reading: fixes 2 and 2b each save 6-8 ms per minor and stay ON inside the opt-in; fix 1 (scoped pins)
> is neutral on this shape. The pass rule below (WE at or below W) fails by 1.9x. The
> `[zgc-relocate-phases]` total of a scoped slide is about 5 ms (`logical_us` ~4-5 ms, `select_us`
> ~60 us), so ~14 ms of the ~19 ms gap is outside the relocate phases: the next suspects are the
> whole-grid walks this page already lists as NOT DONE (`age_pages_and_split`, remembered-set
> registration) and the post-slide free-list / cursor work. Measure them with phase timers before
> another fix.

> **t1 status (2026-10-26, wave 10, lane ZT3GN): FIXED IN CODE, PENDING HOST VERIFICATION (the A/B).**
> Three sub-switches, each default ON inside the opt-in, `=0` kill (`gc/src/zgc/young_evacuate.rs`):
>
> | Fix | Switch | Where |
> |---|---|---|
> | 1. `select_us`: the pin screen skips pin words below `scope.lo` and resolves interior words by binary search over `scope.live` (`young_evacuate::scoped_pin_base`), not the registry's backward bitmap walk across the dead nursery | `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_SCOPED_PINS` | `relocate_stw_admitted_scoped`, the pin loop (`scoped_pins`) |
> | 2. `cursor_us`: `Arena::compact_nursery_to` leaves every free block wholly below the scope floor in place and edits only blocks reaching `[lo, old cursor)` (one pass, binary-searched vacated test) instead of `compact_low_to`'s sort + clear + re-add with the O(blocks x vacated) test; unit-tested against `compact_low_to` | `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_LOCAL_INSTALL` | `gc/src/arena.rs` `compact_nursery_to`; call site beside `phases.mark("vacate")` |
> | 2b. the vacated spans are free-listed without the memset (the hand-out zeroes them), only where the sweep's dead-header skip is on (same audit) and not under `CRATONVM_DBG_ZGC_CORPSE` | `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_DEFER_VACATED_ZERO` | same call site |
> | 3. pinned newest page: the vacated per-page spans are published merged, so `[dest, pinned page)` is one free block; the floor hold keeps it above the floor; the cursor stays (accepted) | (part of LOCAL_INSTALL) | `compact_nursery_to` |
>
> Diagnostics: on a scoped slide, `[zgc-relocate-phases]` now splits the old `select_us` into
> `selector_us`, `pins_us` and `select_us` (the gates). `[GC] zgc-gen-young-evac:` gained
> `young_evac_pins_skipped=`, `young_evac_local_installs=` and `young_evac_deferred_zero_bytes=`.
> NOT DONE: the census in `logical_pages` still builds views over the whole low grid (cheap: one
> `Arc` per 2 MiB page; the per-live loop is already O(scope live)), and `age_pages_and_split` /
> the remembered-set registration still walk every page (whole-grid semantics, not changed).
> If `selector_us` is still large on the host, those are the next suspects.

## Symptom

On `Zr1GnGenShape 64 400 200000 5 8 64 -Xmx1g` with `CRATONVM_ZGC_GENERATIONAL=1`:
- slice 1 as built in wave 6: CPU E/G is 1.22 and wall 1.29 (5 ABBA pairs, Windows). The host
  measured wall 6.0 -> 8.1 s.
- with this wave's fixes (floor hold, sweep switches) the minor pause is 68 ms with the slide
  (arm WE) against 28 ms without it (arm W).

## Evidence

`CRATONVM_DBG_ZGC_RELOCATE_PHASES=1` prints `[zgc-relocate-phases]` and, on a scoped slide, the new
`[zgc-young-evac-cursor]` line (`gc/src/zgc.rs`, next to `let proposed = dest.max(highest_pinned_end)`).

1. **The cursor rarely drops.** `pinned_end_off == low_end_off` on most minors. A survivor
   pinned conservatively (a JIT frame's root into the newest objects) sits on the newest nursery
   page. Its page is withheld, so `new_cursor` stays at the top. The slide's `reclaimed` is then 0,
   and the vacated space only becomes free-list holes (`vacated` spans).
2. **The G2 floor advance made it worse** (fixed this wave). The floor went to the post-slide
   cursor, i.e. the top, on every young cycle. Once it met the arena's end, the nursery was
   allocated in holes below the floor, and pass (1) swept them per object (`pass1_us` 150-200 ms).
   Fix: `CRATONVM_ZGC_GEN_YOUNG_EVACUATE_FLOOR_HOLD` (default ON inside the opt-in, `=0` kill). A
   scoped slide no longer advances the floor.
3. **What the slide still costs per minor** (arm WE, after the fix): `select_us` grows to about 19 ms
   and `cursor_us` is about 11 ms. `select_us` is the page census over the whole low page grid,
   while the scope admits only nursery pages. `cursor_us` is `Arena::compact_low_to`: it sorts,
   clears and re-adds the whole low free list, and its `inside_vacated` test is O(blocks x vacated).
   The copy itself (`slide_us`) and the rewrite take about 2 ms.

## HotSpot

A young collection evacuates every young page and frees the whole pages. It has no conservative
pins (precise roots), and its relocation-set selection is O(young pages).

## Suggested fix

1. On a scoped slide, build the page views only for `[scope.lo, low_end)`. Candidates outside the
   scope are discarded anyway (`kept.retain(|c| s.admits_page(c.page_id))`).
2. On a scoped slide, replace `compact_low_to` with a nursery-local install: drop the free blocks
   inside `[lo, old cursor)` and add `[dest, pinned page start)` plus the vacated spans. Blocks
   below `lo` are untouched, so no whole-list rebuild is needed.
3. Pins on the newest page: let the slide pack below the pinned page and publish
   `[dest, pinned page)` as ONE free span (it does already, through `vacated`). Accept that the
   cursor stays. The floor hold keeps that span above the floor.

## Verify

```
G="CRATONVM_ZGC_GENERATIONAL=1 CRATONVM_ZGC_GEN_PRUNE_KEYS=1 CRATONVM_ZGC_GEN_YOUNG_BULK_DEAD=1 CRATONVM_ZGC_GEN_YOUNG_BELOW_FLOOR_COMPLEMENT=1 CRATONVM_GC_STATS=1"
$G CRATONVM_ZGC_GEN_YOUNG_EVACUATE=1 CRATONVM_DBG_ZGC_RELOCATE_PHASES=1 cratonvm --verbose:gc -Xmx1g Zr1GnGenShape 64 400 200000 5 8 64
$G cratonvm --verbose:gc -Xmx1g Zr1GnGenShape 64 400 200000 5 8 64
```

Pass: the WE minor pause (`minor_pause_us / minor_cycles` on `[GC] zgc-gen-cost:`) is at or below W's.
`select_us + cursor_us` is under 3 ms per scoped slide. `missed_rewrites=0` under
`CRATONVM_DBG_ZGC_VERIFY_SLIDE=1`. Checksum `-2647901454014`.
