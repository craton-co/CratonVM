# The pinned young copy's pause is 4-10x HotSpot Serial's on JIT-warm 4-thread javac

> **STATUS (2026-10-27, gcj j1yg-close): OPEN (perf).** The pause half of
> `docs/internal/gc/gcj-j1yg-jit-warm-young-cycles-still-sweep-under-term-3-and-near-full-heaps-RETIRED-20261027.md`,
> whose coverage half is fixed: on dev after `gcj-j1yg-close`, 96-100 % of the young cycles of
> 4-thread javac run the pinned in-place copy by default (0 % before). Nothing here is a wrong
> answer: javac's output digest is HotSpot's on every run. Rows and method:
> `docs/internal/gc-round-gcj-20261019/j1yg-close-report.md`.
>
> **UPDATE (2026-10-27, gcj j1yg-pause round 1): still OPEN, pause halved.** On the branch's defaults
> the javac pause median is 539 / 706 / 548 ms against dev's 897 / 929 / 1194 and HotSpot's
> 170 / 141 / 154 on the same loaded rounds (3-4x, was 5-8x): the dirty-card scan runs on the
> evacuation workers (`CRATONVM_GC_PAR_CARD_SCAN`, new, default ON), the card roots are forwarded on
> them (`CRATONVM_GC_PAR_EVAC_CARD_SEED`, flipped ON), and the pinned plan roots only what a word
> names (`CRATONVM_GEN_PINNED_COPY_NAMED_ROOTS`, flipped ON after its PX rows: promotion 12.7-15.9 ->
> 4.6-6.8 MB a cycle). A new `[vmpause]` split (`CRATONVM_DBG_GCPAUSE`) shows a part this page did
> not list: reference processing walks every `Reference` row each pause (10-17 K weak, 2-3 K soft on
> javac by mid-run), 35-140 ms of the pause between its two passes. Rows, checks and the ordered
> remainder: `docs/internal/gc-round-gcj-20261019/j1yg-pause-report.md`.
>
> **Round 2 (2026-10-27):** on a quieter host (load 10-18) the median is 158-247 ms against HotSpot's
> 62-88 and dev's 379-516: **2.3-3.2x**. Reference-row survival checks skip the pointer-map probe for
> old rows, and the card buffers are kept between cycles. Both HotSpot and CratonVM settle on
> tenuring threshold 1, so the remaining 3-4x promotion is survivors that outlive the next pause
> (retention), not a threshold difference.
>
> **Round 3 (2026-10-27):** pinned-cycle seeds come only from pin words the root slice also holds
> (`CRATONVM_GEN_PINNED_COPY_ROOT_SLICE_NAMES`, ON: seeds ~26 K -> ~9.6 K a run, overflow promotions
> ~0 on most runs), and a minor pause sets aside its old/old reference rows (`..._SETS_ASIDE_OLD_ROWS`
> with the `CRATONVM_GEN_MINOR_PROMISE` that makes most javac pauses eligible). Quiet-host rows: 80 /
> 147 / 153 ms against HotSpot's 41 / 73 / 53 (**1.95-2.9x**); the best root-slice rows 100 / 110 / 121
> against 47 / 58 / 60 (2.0-2.1x). Survivors 14-16 MB vs HotSpot's 10-12: the rest of the retention is
> in the root slice itself (the JIT frames' conservative band), which `-Xint` does not have.

> **Round 4 (2026-10-28, gkr, branch `gc-known-issues-retire`): still OPEN.** Host 176.57.150.136
> (4 vCPU, shared, load 7-17), `-XX:+UseSerialGC -Xmx512m`, 240 s time-boxed runs (9-12 pauses each),
> interleaved with HotSpot 25. On this host the pause is **5-7x HotSpot**: dev-default median
> 493-628 ms against HotSpot's 85-92 ms (3 reps). What was found and done:
>
> - **The GC CPU itself is ~5x HotSpot's, not only a starved pool.** One GC thread
>   (`CRATONVM_GC_PAR_THREADS=1`) is slower than four (1740 vs 960 ms), so the drain does parallelise.
>   A 499 Hz `perf` profile over three pauses puts ~30 % of the GC samples in the object-start walk
>   (`probe_grid_at_sized`, `objstart_chunk`, `all_recorded_starts_in`, `ObjectStartBits::contains`,
>   the layout lookups behind `gen_object_total_size`), ~20 % in the copy (`evacuate`, `scan_object`,
>   `forward_ref_slots_at`), and the rest in the pointer-map fold, page faults and TLB shootdowns.
> - **The JIT compile workers run through every pause.** They are GC-neutral, and in each pause window
>   the three `cratonvm-jit-compiler` threads took as many samples as all GC threads together (38 % of
>   the process's CPU over the run). Renicing them to +19 for a run lowered the median 645 -> 513 ms
>   (one pair, noisy); no code change, noted as a lever.
> - **The lazy object-start grid walks ~80 % of eden here** (`objstart_lazy_bytes` 125 of 179 MB, median).
>   Lazy costs `pre_cards_misc` ~61 ms (proving the pages the dirty-card values name) and a longer
>   drain. The eager walk (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS=0`) gave 526 / 469 / 505 ms against
>   lazy's 613 / 628 / 493. **Added, opt-in:** `CRATONVM_GEN_LAZY_YOUNG_STARTS_DENSE_BACKOFF=1`. A lazy
>   cycle that walked at least half of from-space runs the next 7 cycles eager, then re-measures, so
>   sparse rows (`R23LeanLoop`) keep the grid. It stays OFF: in the next round the build with it
>   lost to the base, 683 against 632 ms median over 2 reps (`evac_drain` 257 against 282), inside
>   this host's noise. The object-start chunk walk now sizes through the walk size memo
>   (`CRATONVM_GEN_WALK_SIZE_MEMO`, default ON). It changes no answer; its pause effect was not
>   measured.
> - **Remaining split (eager arm, 32 pauses, medians):** collect 438 ms = `evac_drain` 125 +
>   `objstart_walk` 122 + `map_merge` 66 + `dirty_card_scan` 41 + `cardclear+young_reset` 30; plus
>   `pre_collect` 28 and `refs` 13 outside the collector.
>
> Next, in order of size: (1) let precise field edges skip the object-start proof, so the walk is
> paid only for conservative words (the in-place cycle's dead-span mirror already makes the dead
> chunks need no headers; the destination-written screen must stay); (2) a pointer map that is not
> refilled per pause (G1's sorted, pooled form thaws on the later inserts of the reference and
> finalizer drains, so it needs an overflow side first); (3) compile workers that yield to a pause.

> **Round 5 (2026-10-29, gcj pcp, branch `pcp-pause`): still OPEN; all three items above done.**
> Host 64.177.43.150 (16 cores, shared, load 8-55 during the round), `-XX:+UseSerialGC -Xmx512m`,
> `GciPn1JavacTreeProbe src 96 2 4` interleaved with HotSpot 25 (`HS / dev / branch` per round).
> javac's digest is HotSpot's (`770eb3087e5e714d`) on every run of the round (60+ runs).
>
> | Change | Switch | Effect |
> |---|---|---|
> | Precise edges (a scanned object's slots, dirty-card slots) forward without the object-start proof; only conservative words ask the lazy bitmap. A chunk walked after a forward sizes forwarded headers through their copy and waits out a claim in flight | `CRATONVM_GEN_PRECISE_EDGES_SKIP_STARTS`, new, ON | lazy walk 100-118 -> 51-59 MB a pause; `card_words_prove` (9-17 ms) gone; card values no longer collected |
> | Card slots keep the in-place destination screen, behind a 256-byte-grain bitmap; field slots need none | (same) | the span binary search was 13 % of GC CPU on the first cut |
> | The evacuation shards' lists and the card scan's per-group buffers kept between cycles | -- | Vec growth and first-touch faults were ~6 % of GC CPU |
> | The pointer map built SORTED on the pool (G1's region bucket sort, kept buffers); later inserts join an overlay instead of thawing (`types::PointerMap`) | `CRATONVM_GEN_POINTER_MAP_SORTED`, new, ON | `map_merge` 24-34 -> 12-19 ms |
> | The in-place rebuild sorts and merges its ~300 K survivors in parallel slices | -- | the serial sort was half of `cardclear+young_reset` |
> | Elastic pool dispatch: a helper that has not started when the driver's share returns is released; the evacuation drain joins by membership and its card seed is claimed in chunks | (pool) | ~320 helpers released a run (`pool_elastic_skipped`) |
> | Background compiles do not start inside a collection pause | `CRATONVM_JIT_COMPILE_YIELDS_TO_GC`, new, ON | |
> | `[gcpause]` rows `in_place_absorb` / `shard_fold` split out of `map_merge`; `rp_*` root-proof population counts | debug | |
>
> GC CPU over a run (perf, 397 Hz): 3290 -> 2291 samples (-30 %). Pause medians, same rounds:
>
> | rounds (load) | HotSpot | dev | branch |
> |---|---|---|---|
> | p6 x3 (32-45) | 121 / 99 / 153 | 403 / 217 / 293 | 203 / 226 / 290 |
> | q6 x5 (12-40) | 79 / 145 / 146 / 103 / 90 | 207 / 308 / 231 / 429 / 223 | 254 / 239 / 177 / 239 / 145 |
>
> So 1.6-2.4x HotSpot. On this host every runnable thread gets ~0.35 core: wall ~= serial CPU / 0.35
> + parallel CPU / (workers x 0.35), and HotSpot's one thread pays the same factor. What is left
> (branch, q6 medians, ms): `evac_drain` 46-68 (its hottest instruction is the forwarding CAS, i.e. the
> first miss on each of ~300 K survivors' headers), `dirty_card_scan` 10-43 (~60 K cards, ~690 K
> edges: retention), `pre_cards_misc` 18-30 (the conservative words' chunks: ~15 K words on ~115
> TLAB-sized chunks, ~48 MB), `cardclear+young_reset` 18-24, `map_merge` 12-16; outside the collector
> `pre_collect` 14-22 and `refs` 9-15 (O(rows) reference passes on the driver, ~11 K weak rows).
> Next: retention (survivors 14-16 MB vs HotSpot's 10-12, the JIT frames' conservative band), a
> reference pre-pass that is not O(rows), smaller lazy chunks for the conservative words.

## Measured

`GciPn1JavacTreeProbe <1361 files> 96 2 4`, `-XX:+UseSerialGC -Xmx512m`, shared 16-core host at load
28-56, interleaved with HotSpot 25.0.4:

| | CratonVM (dev defaults) | HotSpot Serial |
|---|---|---|
| young pause median, 3 runs | 489 / 1113 / 772 ms | 133 / 69 / 123 ms |
| first young pause (old generation empty) | 566-679 ms | 100-210 ms |
| young survivors after a cycle (median) | 21-25 MB | 10-11 MB |
| promoted per cycle (median) | 11-12 MB | 0.4-1.9 MB |
| old generation at the end | 120-180 MB | 29-34 MB |
| process CPU | 514-661 s | 34-35 s |

`GenR4W4EvacThroughputProbe -Xmx256m` (single mutator): 100 % pinned copy, pause median 315-702 ms
against HotSpot's 163-274 ms on the same rounds.

## Where a javac pause goes (`CRATONVM_DBG=gcpause` medians, dev defaults)

1. **`dirty_card_scan`, ~300-400 ms, serial.** ~90 K dirty cards (~46 MB of old generation) and
   ~690 K old->young edges a cycle. Per object it pays a layout lookup
   (`with_compact_oop_scan_ids` / `compact_shape`), the block-offset walk, and page faults on the
   growing hit and edge lists. The count of dirty cards is the promotion below: old objects that
   died after promotion keep pointing at young survivors, whose cards are re-dirtied every cycle
   (`deferred_dirty_cards`), and those survivors are kept alive and promoted in turn.
   The block-offset table is a `RefCell` (`old_gen/bot.rs`), so the walk cannot simply be split
   across workers.
2. **`card_root_forward`, ~105-170 ms, serial.** The driver forwards every edge before the pool
   opens. The opt-in that moves this onto the workers, `CRATONVM_GC_PAR_EVAC_CARD_SEED=1`, aborted on
   this row (`FATAL (CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS) ... cannot be proven during the
   evacuation`) until the chunk-walk fix of the same session
   (`docs/internal/gc/gcj-j1yg-lazy-object-start-chunk-walk-crossed-a-skip-at-the-chunk-end-FIXED-20261027.md`);
   on the fixed binary it completes with HotSpot's digest and the phase total median was 520 vs
   588 ms in one pair. Its flip needs its own sweep.
3. **`evac_drain`, ~150-210 ms.** 8 workers on a host with more runnable threads than cores; the
   workers' own CPU is a fraction of the wall time. The forwarding CAS
   (`try_claim_forwarding`) and the lazy object-start grid (`LazyStartGrid::ensure_chunk`,
   `ObjectStartBits::contains`, which walk most of eden: `objstart_lazy_bytes` ~147 MB of ~177 MB)
   lead its profile.
4. **`pinned_plan`, ~120 ms, and `map_merge`, ~50-100 ms.** The plan resolves up to ~5 K ledger
   words a pause to object starts; the pointer map takes one insert per survivor (the old item 4,
   "pin10 item 5").
5. **Allocator churn, ~10 % of GC CPU.** With the 64 KiB mimalloc arena cap of the boot-RSS close
   (`CRATONVM_MIMALLOC_ARENA_MAX_OBJECT_KB`, `vm-cli/src/mimalloc_tuning.rs`) every GC temporary
   over 64 KiB is mapped and unmapped per pause (page-fault zeroing, `munmap`, `mmap_lock`). Lifting
   the cap for a run (`=262144`) lowered every phase; the pause median of that pair was 1044 vs
   1762 ms. The fix is to reuse the pause's large buffers, not to lift the cap.

## Why survivors are twice HotSpot's

The interpreter-only control (`-Xint`, precise Cheney copies) keeps HotSpot's survivor sizes and
promotes ~3 MB a cycle, and switching off the compiled-frame conservative scan
(`CRATONVM_DBG_NO_JIT_ROOT_SCAN=1`, a measurement only) changes nothing. What the JIT adds is the
young pin ledger's words: up to 1381 distinct young words a thread, ~70 % of them stale values in
Rust frames between JIT entries, and the pinned plan seeds every object on each pinned 4 KiB page as
a root (~17 K objects a cycle). `CRATONVM_GEN_PINNED_COPY_NAMED_ROOTS=1` (opt-in, the
`gci-i30wy` proposal) roots only what a word names and halves promotion (5.4-6.6 MB a cycle); its
flip needs the stale-objref canary that proposal names.

## What would close it

The two rows above show `moving` or `moving-pinned-pages` on at least 90 % of their young cycles (true
today) **and** a young-pause median within 1.25x of HotSpot Serial's `-Xlog:gc` median, CPU time
compared interleaved. In order of measured size: fewer promoted bytes (named roots; fewer stale
ledger words), a card scan whose cost follows the live old->young edges (a parallel or reusable
walk), card roots forwarded on the workers (`_PAR_EVAC_CARD_SEED`, usable since the chunk-walk fix), the pause's large
buffers reused across cycles, and the lazy object-start walk and pointer map out of the per-survivor
path.

After round 1 (`j1yg-pause-report.md`): the card scan, the card seed and the named rule are done.
What is left, in order of size: reference processing that visits only the rows a young pause can
change (both `Reference` and referent old: nothing to do); the copy itself (`evac_drain`); retention
(survivors 1.5-1.8x HotSpot's, promotion 3-4x, which is what keeps ~690 K old-to-young edges and
~92 K dirty cards a cycle); the serial survivor-list passes of `map_merge` and the in-place finish.
