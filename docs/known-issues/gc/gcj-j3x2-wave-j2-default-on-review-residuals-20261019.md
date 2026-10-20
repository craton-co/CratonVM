# Wave j2's default-ON changes: adversarial review residuals (lane j3/x2)

> **STATUS (2026-10-19, gcj j3/x2): OPEN -- residuals of the review of every default-ON change wave j2
> landed; the items fixed in this lane are in `docs/internal/gc-round-gcj-20261019/j3-x2-report.md`.**
> By reading the j3 base `43edda6dbe` (line numbers are that tree's unless marked "now"); nothing built
> or run. Four read-only helper passes split the reading (rf2; yg2 + og2; pn2 + `claim_in_place`;
> jn2 + ob2 + cm2); every item below was re-read by the lane before it was filed, and an item resting on
> a helper's reading of HotSpot from memory says so. Each item says what closes it.

## R1 -- RISK (parity): the ec47 deferral also defers finalizables already QUEUED for `finalize()`

`CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS` (rf2, default ON). The candidate list the VM hands a
collection is `finalizable_roots` (`vm/src/runtime/interpreter/gc_and_alloc.rs` ~4427): the registered
finalizables PLUS `finalizer_thread.pending_addresses()`, objects already claimed and waiting in the
finalizer queue. With the switch ON a queued YOUNG finalizable Q is no longer a true-root seed
(`gc/src/gen_heap.rs` ~35123-35129, the `defer_fin_seeds` arm); it is marked in the major's resurrection
round, so Q's OLD children land in the depth-2 set and a weak reference to such a child is cleared at
that major while Q is still queued. In HotSpot a queued `Finalizer` is strongly reachable from
`Finalizer.queue`: its closure is strong and that weak reference is not cleared. `=0` keeps the young
Q a seed (HotSpot's answer); an OLD queued Q already took the candidate path before the change
(`seed_word` ignores old addresses), so the deferral extends a pre-existing divergence to young Q rather
than creating a new class.

Probe: `tools/probes/Gcj3X2QueuedFinalizableOldChildProbe.java` (rows in the report, block QF).
Closes it: the queued set is a STRONG root set, not a candidate set (HotSpot's model) -- proposal
`docs/internal/proposals/gc/gcj-j3x2-proposal-queued-finalizables-are-strong-roots-20261019.md`.

## R2 -- RISK (contract): the resurrection closure carries post-copy addresses on the moving path

`take_resurrection_closure` promises PRE-collection addresses (`gen_heap.rs` ~18295; consumer
`gc_and_alloc.rs` ~4468, ~4493). With the deferral, the round pops the promoted resurrected
finalizables (post-copy) and promotion destinations reached from deferred young candidates, and pushes
them into `pass.closure` (~35431). By reading harmless (a destination was free at cycle start, so it
cannot match a reference-processor row's pre-GC address; destinations could already reach `closure`
through an old dead finalizable's young edges). UNSURE under a held concurrent-sweep span (rows naming
freed old memory). Closes it: skip a pop in this pause's promotion-destination set, or map it back.

## R3 -- the moving-path hunk of ec47 (`d24c24efc0`) has no unit test

Reverting `d24c24efc0` leaves every `gcj_j2rf2_*` test green (`gc/src/gen_heap_gcj_j2rf2_tests.rs`).
Fail-first test: a tenured old child C, a young finalizable F (`F.0 = C`, no root), a moving cycle
with a same-cycle major and `finalizer_addrs = [F]`; assert `take_resurrection_closure()` holds C with
the switch ON and not with `=0`, and (P-C) that C and F's destination are still allocated. Also: the
switch is read twice per moving major (Phase 5 ~25897 and `old_gen_gc_inner` ~34285; the doc says
once); a disagreement is safe both ways (the deferred seeds are always honoured). Latching it into
`OldGenFinalizerPass` would make the doc true.

## R4 -- RISK (yg2): a free-listed sunk TLAB tail joins the known-zero set on a two-word check

`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY` (default ON). `gc/src/arena.rs` ~5326-5334 and `gen_heap.rs`
~37855-37863: a tail free-listed through the keeping tail door now claims the WHOLE tail known-zero
(`note_known_zero_span`), and every later refill from it skips the memset. Release builds check only the
tail's first and last word (`tlab.rs` ~2404-2414; the whole tail only under
`CRATONVM_DBG_DEADREF_STORE`). A stray write in the middle of a tail (a JIT or runtime bug writing past
the TLAB cursor) used to be scrubbed by the next free-list memset; now it persists. Not a bug today (the
retraction arm always had this trust). Closes it: claim the tail only under `dbg_deadref_store`, or
sample it with `known_zero_sample_is_clean`; and the missing soundness test (every byte of
`known_zero_within(free_blocks_sorted())` reads zero after a cycle that reused and re-freed a claimed
block).

## R5 -- RISK (og2): the growth-span bitmaps are commit-charged at growth_max / 64

`CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN` (default ON). Both the mark bitmap and the reference-skip bitmap
are built over `growth_max` (`concurrent_mark.rs` ~4275, ~4562) with `alloc_zeroed`
(`heap_bitmap.rs` ~291); a failure aborts in `handle_alloc_error`, and on Windows a large zeroed
allocation is commit-charged though never touched. Closes it: cap the span or fall back to capacity past a
size, with a row on a large `-Xmx` (`-Xmx8g`, a concurrent cycle) reading the process commit charge.
(The soundness defect of the same switch -- the class-unload capture's stale span -- is FIXED, report F4.)

## R6 -- BUG, cross-lane (ob2, `serviceability.rs` is not this wave's to edit): `jcmd VM.stringtable -verbose`

Filed separately with the diff: `gcj-j3x2-jcmd-stringtable-verbose-is-not-hotspots-20261019.md`.

## R7 -- RISK (cm2, pre-existing, narrowed not closed): a registered native thread is counted before its OS thread exists

`register_native_thread` (`vm/src/vm/vm_exec.rs` ~41584-41588) registers with `stw_ready = true`
(`thread_registry.rs` ~3301); both blocker spawners then allocate the carrier mirror BEFORE `spawn`
(`alloc_carrier_thread_mirror` -> `create_string`, which may run a moving young collection:
`native-builtins/src/lib.rs` ~8576-8580). The entry is alive, `stw_ready`, has no join handle and
`os_tid` 0, so the counted census counts it while the take-over roster skips tid 0: a pause in that
window -- even one the spawner itself starts -- waits for a thread that does not run yet, and its spawn
sits behind the pause. `abandon_native_thread` (cm2) does not repair a census already taken. Closes it:
the two blocker spawners register with `register_starting_with_daemon` (`stw_ready = false`) and
`VmNativeThreadBlocker::publish_os_tid` (first on the spawned thread, `xnio_io_thread.rs` ~640) calls
`mark_stw_ready`, Generational and the switch only; fail-first test: the entry is absent from the counted
census until `publish_os_tid`. Owner this wave: rt3 (`vm/src/threading/`). Also (NIT): no `gcj2cm2_*`
test runs `retire` with a pause requested, the case the switch exists for.

## R8 -- NIT (jn2): no test can catch a broken `NewString` gate

`CRATONVM_JNI_NEW_STRING_HOTSPOT_EDGES` is sound by reading (`jni.rs` ~17622-17669: Generational gate,
per-VM latch, `=0` exact; `NewStringUTF` untouched), but the only test is the latch value rule
(~27377): deleting the `is_generational()` check or the whole new branch passes. Closes it: a source pin
that `jni_new_string_hotspot_edges` tests `is_generational()`, or a two-heap `jni_new_string(env, null,
0 | -3)` test. Owner this wave: rt3 (`jni.rs`). UNSURE (helper's reading from memory): under
`-XX:-CompactStrings` HotSpot allocates `len << 1`, so the NASE message for `len = -5` would be `-10`.

## R9 -- pn2 / `claim_in_place` residuals

- (NIT) the retry in `claim_in_place` (`gen_evac.rs` ~1403) accepts any non-forwarded `now`, including
  one whose kind / element tags differ from what the caller screened (and sized `kept_size` from). No
  in-cycle writer of a from-space mark word was found (the only `set_gc_age` in `gen_evac.rs` is a test),
  so both the retry and this lane's F3 are defensive; a tag re-screen (`note_refusal` + `old_ptr` on a
  changed tag) would make them exact.
- (RISK, pn2 item 16, unchanged) `REFUSALS_PER_SHARD = 64` (`gen_evac.rs` ~403): a refusal past the cap
  never reaches `note_refused_forward`, so `CRATONVM_GEN_PINNED_REFUSED_FORWARD_KEPT`'s guarantee is
  incomplete for that object (it falls back to the j1 drop).
- (NIT) `gcj2pn2_an_unmounted_continuation_claims_no_carrier_tid` (`thread_registry.rs` ~13537) pins
  the pure table, not the fold's `heap.is_generational()` gate (~8129-8132) -- the "G1 / ZGC unchanged"
  claim of `CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED` rests on reading.
- (NIT) `docs/internal/gc-round-gcj-20261019/j2-pn2-report.md` line 36 still cites pn2's `claim_in_place`
  `Err(_)` arm as part of its finding 3's fix; the merge (`3bde291693`) dropped that arm for x1's retry.

## R10 -- the parked-split arm has the dedupe arm's exposure

The dedupe arm is now consumed only on its arming thread (report F1). The default-ON parked-split arm
(`arm_parked_split_gather`, gci i13/vg, `thread_registry.rs` ~2322) is still taken by the FIRST plain
`collect_all_root_snapshots()` on any thread; a foreign gather between the arm and the door's would get
the split (its old-only section diverted) and the door's gather the whole set. Serialised by the pause
today. Closes it: the same owner-thread rule.
