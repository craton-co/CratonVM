# ZGC major cycles pay about 2.5 us per live Reference row inside the pause (HotSpot: nothing) — 2026-11-07

Filed by the retirement of `docs/internal/architecture-and-subsystems/gc/p3-collection-object-hot-paths-review-RETIRED-20261107.md`
(its hypothesis 3, and the open gate of
`docs/internal/performance-and-benchmarks/performance/w2a-reference-init-and-identity-hash-inlining-RETIRED-20261107.md`).

## What is measured

`tools/probes/gc/WeakRowPopulationPause.java` (K live `WeakReference`s to live referents, then 1 GB of garbage), default
collector (ZGC, generational; every cycle in the probe is a `Major Collection (High Usage)`), `-Xmx600m`, host
root@64.177.43.150 `taskset -c 0-7`, no-LTO build, `CRATONVM_DBG_GCPAUSE=1` for the split:

| K | pause (ms) | `pre_collect` | `collect` | `refs` |
|---:|---:|---:|---:|---:|
| 20 000 | 94 | 16 | 45 | 30 |
| 100 000 | 254 | 82 | 68 | 103 |
| 400 000 | 906-1304 | 188-384 | 109-133 | 608-786 |

Per collection with `-XX:-ZGenerational`: 18 / 29 / 70 / 417 ms at K = 0 / 20 000 / 100 000 / 400 000; HotSpot ZGC:
1.1 / 1.3 / 3.2 / 1.5 ms. The reference passes cost about 2.5 us per row per major cycle, linear in the number of
`Reference` objects, live or not, and they run stop-the-world (`refs` is the VM's post-collection round). javac keeps
10-17 K such rows (P3), so about 25-45 ms a major cycle; a Spring application with hundreds of thousands of
`WeakHashMap` entries pays whole seconds.

The per-allocation side of the same protocol: `weakref.alloc+get` 128 ns (HotSpot 29), `weakref.queue-alloc` 125 (38).

## Why the cheap fixes do not apply

* No young cycle exists to skip rows in: the probe, and the launcher's defaults, collect with majors only. G1 and
  Generational skip old rows on young pauses (`ReferenceProcessor::stash_settled_rows`); ZGC cannot, because every
  major cycle must find out which referents died.
* Profile (frame-pointer build, 512 MB probe, 3 939 samples): `run_collection_pause` 43 % of the process, of which
  `reference_round` 17 %, `weakref_null_referents_pre_gc` 16 % (6.8 % of that is `bind_allocated_references_pre_gc`),
  `collect_garbage` 7.8 %. Inside the passes the cost is spread over about thirty operations, none above 12 %:
  `ZgcRealHeap::set_field` 12 % of the round, `update_after_gc` 9 %, `watched_pre_gc_addr_survived` 7.5 %,
  `retire_entries_the_pre_gc_pass_found_cleared` 6 %, the `referent_class_stamp`, `identity_stamp` and
  `was_nulled_by_pre_gc_pass` lookups 5 % each, and in the pre-collection pass `referent_class_stamps.insert` 14 %,
  the examined-set inserts in `weak_phantom_active_triples` 14 %, `stamp_referent_class` 9 %. The five address-keyed
  tables (`pre_gc_examined`, `pre_gc_nulled`, `identity_stamps`, `referent_class_stamps`, `app_row_index`) are a
  quarter to a third of both passes.
* Carrying those marks on the row (the "cheap idea" of the P3 review) bounds the win at about 1.3x on the passes and
  rewrites the guards that keep a write from landing on a re-issued address; not built.
* `CRATONVM_ZGC_CONCURRENT=1` (stage 1 of the concurrent collector program) moves the round out of the pause
  (`refs` 195 -> 0.2 ms at K = 400 000, pause 396 -> 166 ms) but leaves `pre_collect` at about 120 ms: the pre-collection
  pass still walks every row to build the hidden set.

## Re-measured on an idle host (load 0-3, dev 68b9543bb0, no-LTO), K = 400 000

| arm | ms per collection | pause split of the last collection |
|---|---:|---|
| default (generational) | 337 | pre_collect 138, collect 74, refs 299 |
| `-XX:-ZGenerational` | 320-336 | pre_collect 102-127, collect 76-89, refs 294 |
| `-XX:-ZGenerational CRATONVM_ZGC_CONCURRENT_REFS=1` | 338-350 | refs unchanged: the round is still in the pause |
| `-XX:-ZGenerational CRATONVM_ZGC_CONCURRENT=1` (whole profile) | 93 | pre_collect 97, collect 31, refs 0.2 |

The loaded-host figures above (906-1304 ms) overstate the idle ones about 3x; the shape is the same. What follows from
the table:

* The round (`refs`, 300 ms) is the part the existing program already removes from the pause; only the whole profile
  does it, and it is opt-in and refuses generational mode (`ZgcRealHeap::concurrent_refs_skip_mode`), the launcher's
  default. Offering the hide to a cycle `young_cycle_predicted` calls a major is the smallest step to the default, with
  the mispredicted-young direction needing a fallback (a hidden old referent reads unmarked in a young cycle, so
  `clear_dead_vm_referents_stw` would null a live one).
* What is left after that is `pre_collect`, about 240 ns a row (97 ms): `weak_phantom_active_triples` rebuilds the
  examined set, the stamp table and the active list from the rows every cycle, then the screen (subclass test, identity
  stamp, slot read, class stamp, hidden push) runs per row. A flat profile of the whole process puts hash-set work
  (`usize` equivalence, `contains_key`, `insert`) and `ZObjectStarts::contains` at the top. It reaches 4.5 ms only when
  the hidden set is the persistent member bitmap (`zgc/skip_bits.rs`) kept in step with the rows, not rebuilt.

## What a fix needs

Discovery during marking, as HotSpot does: a `Reference` is found when the marker visits it, only reachable ones are
touched, and nothing is nulled and restored. In this VM that is the `CRATONVM_ZGC_CONCURRENT_REFS` /
`CRATONVM_ZGC_CONCURRENT_REF_ROUND` program (`docs/gc-tuning.md`), plus a pre-collection pass that no longer walks the
rows. Acceptance: `WeakRowPopulationPause` on the default collector within 3x of HotSpot's ms per collection at
K = 400 000 (HotSpot 1.5 ms), `weakref.alloc+get` under 60 ns, and the existing weak/soft/phantom/finalizer probes
unchanged.
