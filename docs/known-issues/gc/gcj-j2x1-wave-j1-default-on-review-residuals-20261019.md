# Wave j1's default-ON changes: adversarial review residuals (lane x1)

> **STATUS (2026-10-19, gcj j2/x1): OPEN -- residuals of the review; the items fixed in this lane are in
> `docs/internal/gc-round-gcj-20261019/j2-x1-report.md`.** By reading tree `c82304324b`; nothing built
> or run. None of the items below is a soundness defect on the default path; each says what closes it.

## og (`CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX`, `CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT`, Phase 0 shared starts)

1. **The pinned compactor's index arm has no ON-vs-`=0` test.** `old_gen.rs:8525`
   (`gcj_j1og_the_forward_index_compacts_exactly_as_the_hash_map`, through `gcj_j1og_compact_snapshot`
   `:8481`) drives `og.compact()` only, i.e. `compact_walked`. `compact_around_pins_watching`'s index arm
   (`old_gen.rs` ~6609) is compared with nothing. Close: wrap a pinned-compaction test
   (`og.compact_around_pins(..)` at `old_gen.rs` ~11103) in `with_thread_overrides` for `"1"` / `"0"`,
   compare `pointer_map`, the young-ref holders and `used()`, and assert `forward_index_compactions()` rose
   only under `"1"`.
2. **The loader-snapshot test cannot tell its arms apart.** `gen_heap.rs:60982`
   (`gcj_j1og_the_old_mark_loader_snapshot_keeps_an_old_instances_loader`) passes with the snapshot arm
   never taken (both arms give the same answer; no engagement counter). An equivalence test, not an
   engagement test. Close: a `[gcphase]` count or a census slot for snapshot lookups.
3. **R1 (`for_each_old_gen_ref`'s compact arm capped by the walked size, `old_gen.rs:4234`) has no test**
   (commit `d92c635aa5`). Close: a compact object whose registered layout is larger than its walked size;
   assert no callback offset >= the walked size (model: `old_gen_mark_rejects_an_interior_address_instead_of_decoding_it`,
   `gen_heap.rs` ~61025).
4. Doc: with the shared start set, `CRATONVM_GC_OLD_CLOSE_BITMAP=0` no longer turns the bitmap off for
   Phase 0 (answers identical). The switch's row should say so.

## pn (`CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED`, `CRATONVM_GEN_YOUNG_PIN_SHORT_SWEEP_INCOMPLETE`)

5. **No counter shows the seed protection engage.** `pxr_dropped_sum` now leaves protected seeds out and
   nothing counts the seeds kept unvisited, so rows PK def-vs-kill cannot show the switch working. Close:
   one census slot in `note_pinned_young_cycle` (`gen_heap.rs` ~38497) counting
   `ip.protect_seeds && !ip.visited[i] && seed`, printed on `pinned_exact_roots_census_line`.
6. **A kept but unvisited seed is never traced** (older than j1; j1 changed "freed" into "kept untraced").
   The serial exits (tag screen, suspect header, `gen_heap.rs` ~39467 / ~39539) fire only on words no real
   object has, so nothing is lost there. The parallel exit, `gen_evac.rs` `claim_in_place`'s `Err(_)` arm,
   is FIXED by lane x1 (retry; see the report). `gcj1pn_an_unvisited_seed_is_never_dropped`
   (`gen_heap.rs:70712`) tests the predicate only, never a cycle.
7. **Perf parity, not soundness:** where `own_stack_top()` is 0 (targets other than Windows / Linux), every
   licensing deposit with a non-empty chain is now incomplete, so the pinned copy never engages while
   compiled frames are live. Also, after `mark_incomplete` the layout-free sweep still runs although `push`
   drops every word (`gc_quiescence.rs:7642`): skipping it saves the sweep.

## ob (launcher typed values, Serial selector)

8. **`-XX:+Name` / `-XX:-Name` of a value flag is not refused.** `gen_hotspot_value_flag_refusal`
   (`vm-cli/src/main.rs:7772`, `a.strip_prefix("-XX:")?.split_once('=')?`) lets `-XX:+MaxTenuringThreshold`
   fall through. HotSpot's `Arguments::process_argument` answers `Unexpected +/- setting in VM option
   '<Name>'`. Not built: the table's rule is a local oracle row first. Row:
   `p x1_vf_plus 60 "" "$GEN -XX:+MaxTenuringThreshold" "Gcj1ObValueFlags"` (rc 1 both, `.err` vs `.hse`).
9. **Two declared types are unverified:** `InitiatingHeapOccupancyPercent` and `G1HeapWastePercent` are
   `Uintx` (`main.rs:7684-7685`); JDK 20+ may declare both `uint` (matters only from 2^32 up). Close:
   `java -XX:+PrintFlagsFinal -version | grep -E 'InitiatingHeap|G1HeapWaste'` on 25.0.3.
10. `MaxTenuringThreshold=4294967297`: HotSpot reads a julong and narrows it (may accept as 1) where the
    table prints a range line. Oracle row: `-XX:MaxTenuringThreshold=4294967297 -XX:+PrintFlagsFinal -version`.
11. The internal spelling `--XX:UseGc Generational` is invisible to `scan_effective_generational`
    (`main.rs:7784`), so it skips the early refusal (pre-existing; `scan_effective_zgc` has the same blind
    spot).

11b. **An unknown `-XX:+Use<X>GC` selector is a warning and a Generational fallback; HotSpot refuses it**
    (`vm-cli/src/main.rs` ~9523-9540: `Unrecognized VM option 'UseserialGC'` / `Did you mean
    '(+/-)UseSerialGC'?`, rc 1 -- row `x1_ss_lower` on j2a). Pre-existing launcher policy ("a `java`
    drop-in keeps booting"); a parity change needs the user's decision. Parallel / Shenandoah / Epsilon are
    real HotSpot options HotSpot accepts, so only a selector HotSpot does not know would be refused.

## rf, st (`CRATONVM_GEN_REFERENT_READ_KEEP_ALIVE`, `CRATONVM_GC_WATCHED_SET_PER_HEAP`, `CRATONVM_GEN_SOFT_CLEAR_ALL_ADVANCES_CLOCK`, the ASCII string path)

Nothing broken. The one SUSPECT item -- `unsafe_compare_exchange_heap_slot`'s witness returned with no
keep-alive (`native-builtins/src/unsafe_natives_ext.rs` ~10158) -- is FIXED by lane x1 (off ZGC; see the
report). Low nits left:

14. The referent-slot test is a hard-coded `slot == 0` (`gen_referent_read_keep_alive`,
    `vm/src/vm/vm_exec.rs` ~29473; `slot0_read_keep_alive`, `unsafe_natives.rs:1478`) rather than HotSpot's
    `is_referent_field(base, offset)`; true for every `Reference` layout today. Close: a `debug_assert!` that
    `Reference.referent` resolves to slot 0 when the class is loaded.
15. The watched-set key is computed in two spellings (`gen_heap.rs` ~34672 and `old_gen.rs`'s two
    compactors); one helper (`GenerationalHeap::watched_set_key`) would keep them from drifting.
16. Two debug readers (`gen_heap.rs` ~30996, ~31128) answer for the CURRENT row, not the heap's key:
    diagnostic only.

## cm, yg, jt

12. cm `loop_top_poll`'s ungated handshake poll (`interpreter.rs`): checked, sound and free on the default
    path -- the only production queuer is `request_handshake` behind the opt-in
    `CRATONVM_GC_HANDSHAKE_FRAME_TRACE`, and the poll sits on the cold arm. No action.
13. yg `CRATONVM_XLOG_GEN_TLAB_TOTALS_ALONE` arms the per-pause before-snapshots (heap, clock, cpu /
    metaspace gated on their tags) on a `gc+tlab=debug`-only spec: the same shape as the `gc+ref=debug`
    arm, no extra output. Its kill switch is an uncached env read once per pause in that configuration
    only. No action.
