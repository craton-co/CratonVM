# ZGC: three side structures kept their peak allocation for the rest of the run

> **STATUS (2026-10-19, zt t5/pg3): HOST.** All three fixes are default ON with kills and built; what is owed is the Linux row, because `Zr3FpRss` reads VmRSS / VmHWM from `/proc/self/status` (prints -1 elsewhere) and the pass criteria are maxrss: `CRATONVM_DBG=gc-stats /usr/bin/time -f '%U %S %M' cratonvm -Xmx256m Zr3FpRss steady` and `-Xmx8g Zr3FpRss burst`, default vs `CRATONVM_ZGC_FOOTPRINT_TRIM=0` / `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR=0` (pass: `fp_trims>0`, `fp_sparse_skipped>0`, maxrss <= the kill arm's, CPU within noise). Retire on that row.

> **STATUS (2026-10-19, zt t3/rl): no code change; its rows (fp_trims>0 vs the `=0` kill, CPU row) still unrun. Its step-2 relative, the bitmap pages, is built opt-in (`CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`, 10edde1478).**

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- All three fixes are in code and default ON: `CRATONVM_ZGC_FOOTPRINT_TRIM` (`prune_relocations`, `zgc_fp_trim_target`), `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR` (`clear_within_sparse`, heap_bitmap.rs:1006), and `Arena::decommit_span_at` / `decommit_free_blocks_above`. The host evidence is partial: zr orch-notes z3b recorded fp_sparse_skipped=6.2 M and burst RSS (1g +892 MB vs HotSpot +830). The trim rows (fp_trims>0, the `=0` kill arm) and the CPU regression row were never recorded.**

> **STATUS (2026-10-12, zr z3/fp): FIXED IN CODE, PENDING HOST PROBE.** `CRATONVM_ZGC_FOOTPRINT_TRIM` (default ON, `=0` kill) trims the forwarding table and the dead scratch; `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR` (default ON, `=0` kill) stops the mark-bitmap clear from faulting in every page of the bounded span. Engagement on `[GC] zgc-footprint:` (`fp_trims`, `fp_trimmed_bytes`, `fp_sparse_skipped`). Rows below.

Area: footprint (RSS after a peak; part of
`peak-rss-is-four-times-hotspot-20261012.md`).

## 1. The forwarding table (`ZgcCounters::relocations`)

One `FxHashMap<usize, (usize, u32)>` entry (24 bytes plus a control byte) per
object the slide moved and whose old address is not a live base again,
amortized-pruned (zr z2/r2) and cleared past a 4 M-entry ceiling
(`ZGC_RELOCATIONS_CEILING`). hashbrown's `retain` and `clear` never shrink, so
a table that once approached the ceiling kept ~8 M buckets, about **200 MB**,
for the rest of the run, whatever it held afterwards. The ceiling is absolute,
so a `-Xmx256m` heap of small objects (a 64 MiB evacuation budget is 1-2 M
moves per slide) reaches it as easily as an `-Xmx8g` one.

Fix: `prune_relocations` (walked prunes and the empty table) shrinks the table
to twice what survived when its capacity is more than four times that and the
shrink at least halves it (`zgc_fp_trim_target`, floor 16 Ki entries). A
skipped prune never trims (it has not looked at which entries are dead).

## 2. The sweep's dead-address scratch (`ZgcCounters::dead_scratch`)

Held across collections on purpose (the 104 MB final allocation of a 13 M-dead
cycle is paid once), and `swap_is_worth_it` keeps the larger buffer when a small
cycle follows a large one. Nothing ever gave it back: a burst that once killed
13 M objects pinned 104 MB of resident scratch for the life of the process,
including all the cycles where `wants_dead_addresses` said no and nothing was
pushed at all.

Fix: after `MonitorCleanup::prune_dead` (its one reader), a scratch that stayed
under a quarter full for 8 consecutive collections is shrunk to twice its
length (floor 16 Ki entries). One small cycle after a large one still keeps the
large buffer (`the_dead_scratch_is_cleared_and_its_capacity_reused` passes).

## 3. The mark bitmap's pages

`mark_clear_all` (and the opt-in old-start bitmap's rebuild) cleared the words
over the bounded live ranges with a store per word. The bitmap is an
`alloc_zeroed` block, and a store faults a private page in even when it writes
the zero already there, so after the first collection every page over the
whole bumped arena was resident (1/64 of the arena span the cursors ever
passed: 16 MB per GB), although marking only ever set the words over live
objects. `HeapBitmap::clear_within_sparse` loads first and stores only into
non-zero words: pages over garbage and holes stay unbacked (a load of an
untouched page maps the shared zero page). Same final state; stop-the-world
only, like `clear_within`.

## Also

- A release the `-Xms` floor budget refused reset the granule's idle clock
  (`Arena::decommit_span_at`), so once commitment above the floor came back it
  sat idle a whole new `ZUncommitDelay`. The stamp is now kept unless the
  granule really went back (decommitted or reset); under the trim switch.
- `Arena::decommit_free_blocks_above` sorted the whole low free list at every
  collection start although only blocks of at least one granule can release
  anything; it now walks the span tier's keys from `GRANULE` up (same spans,
  same order; no-op for the other collectors, which release nothing from
  sub-granule blocks either).

## Rows (host)

`CRATONVM_DBG=gc-stats`, `/usr/bin/time -f '%U %S %M'`, both modes.

| row | pass | positive control | kill control |
|---|---|---|---|
| `cratonvm -Xmx256m Zr3FpRss steady` | `fp_relocations_bytes <= fp_relocations_peak / 2` when `fp_relocations_peak > 32 MB`; maxrss <= the kill arm's | `fp_trims > 0` on any run whose `fp_relocations_peak` exceeds ~1 MB and whose last prune walked | `CRATONVM_ZGC_FOOTPRINT_TRIM=0`: `fp_trims=0`, `fp_relocations_bytes == fp_relocations_peak` |
| `cratonvm -Xmx1g Zr3FpRss steady 4` | as above; CPU within noise of `=0` (the trims are rehashes inside the pause) | as above | as above |
| `cratonvm -Xmx8g Zr3FpRss burst` | `fp_mark_bits_noted < fp_mark_bits_reserved * fp_arena_written / fp_arena_reserved` (sparse clear left pages over garbage unbacked) | `fp_sparse_skipped > 0` | `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR=0`: `fp_sparse_skipped=0`; maxrss difference is the clear's cost |
| regression suite, `ZgcTlabStress 8 200`, `R16Bench bintrees` | identical stdout; CPU within noise | | `=0` both switches |
