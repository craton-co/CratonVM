> **STATUS (2026-10-19, zt t6/tc): OPEN -- GN-5, GN-7, GN-11, GN-17 (first step), GN-18 FIXED IN CODE, PENDING BUILD/PROBE; GN-8, GN-9, GN-10, GN-12 open with their deciding rows.** GN-17: the young-aged sweep (and the aged pass (1) of the young complement) shards over `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS=<n>` (opt-in, default 1 = the serial walk; `sweep_young_aged_sharded` in `gc/src/zgc/sweep.rs`, with the audit of the generational survivor writes); engagement `young_sweep_sharded=` / `young_sweep_shards=` on `[GC] zgc-gen-cost:`; test `zt6tc_the_sharded_young_aged_sweep_agrees_with_the_serial_one` (registry, free bytes, cursor, old-live / nursery-old / below-floor / promoted figures equal to the serial arm on both sweep arms). Deciding row: `Zr1GnGenShape 64 400 200000 5 8 64 -Xmx1g` with `CRATONVM_ZGC_GENERATIONAL=1 CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT=1`, `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS=1` vs `=4`, `minor_sweep_us` and process CPU, interleaved; `young_sweep_sharded` must equal `minor_cycles` on the `=4` arm. The complement already sweeps the nursery in proportion to its LIVE set; what stays O(young allocated) is the below-floor per-object pass, now shardable. GN-18: `CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE` (default ON in the mode, `=0` kill) leaves the old large-object survivors out of the major's old live figure on the floor arm (test `zt6tc_an_old_large_array_is_counted_once_on_the_floor_arm`). GN-5: `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES` (default ON in the mode, `=0` kill) ages every survivor of a `System.gc()` major to the promotion age (test `zt6tc_a_system_gc_major_tenures_every_young_survivor`; engagement `system_gc_tenures=`). GN-7: `CRATONVM_ZGC_GEN_JIT_PROMOTION_FLOOR` (default ON in the mode, `=0` kill) publishes `promotion_age << 4` as the JIT young floor, ordered against promotion-age changes (test `zt6tc_the_jit_young_floor_is_the_helpers_age_test`); row E-put decides its cost. GN-11: `CRATONVM_ZGC_GEN_YOUNG_POOL_LIVE` (default ON in the mode, `=0` kill) keeps the young survivors in the young pool (test `zt6tc_a_minor_leaves_its_young_survivors_in_the_young_pool`). GN-8 (compiled element cards: row E1 with the JIT on), GN-9 (a decision at the flip), GN-10 (`minor_pre_us + minor_sweep_us` vs 25 % of `minor_pause_us` in row R1) and GN-12 (rows R1/R2) are unchanged. All of these act only with `CRATONVM_ZGC_GENERATIONAL=1`.

> **STATUS (2026-10-19, zt t5/gn): OPEN -- GN-6 CLOSED BY READING (VM side), 9 items open.** GN-6 audit: every raw reference-slot writer outside the collector's accessors was checked on `1805be85ae` -- `write_ref_slot` has three callers outside `gc/` (`field_fast.rs` `array_store_ref`: `note_ref_store_element`; `vm/src/jit/helpers.rs` the `aastore` helper: `heap.write_barrier`; the `StringBuilder` grow helper: `heap.write_barrier`), `NativeContext::set_field_raw` is `heap.set_field` (vm_exec.rs `set_field_raw`), the Unsafe reference CAS arms call `ref_cas_post_barrier`, the post-GC referent restore uses `set_field_suppress_satb` (which cards), class statics are VM-table roots, and the remaining raw 8-byte writes in `vm_exec.rs` are primitive-array copies. So a field of an old mirror, loader or metadata object is carded like any old object's; the wholesale-root case needs no tracing. Not re-audited here: compiled reference stores (the JIT emitters' post gate, lane ca's t2 audit). The verify arm stays a proposal. GN-7 / GN-8 are stage **S2b** of the restaged plan. New open items GN-17 (the young sweep is O(allocated): the S0 gate's failure) and GN-18 below; GN-15 / GN-16 were fixed in code by t5/gn (`t5-gn-report.md`).

> **STATUS (2026-10-19, zt t5/pg3): OPEN (GN-5..GN-12, opt-in generational mode).** ZGC collector work, each a stage of `zgc-zt3gn-proposal-generational-as-the-default-20261019.md`; now also carries zr1gn's X2 (GN-8: `jit_write_barrier_element`, row E1 with the JIT on). The gen mode is not the default, so none reaches a default run.

> **STATUS (2026-10-19, zt t3/gn): OPEN -- 8 items, none fixed in code.** Filed by the wave t3 generational review (`docs/internal/zgc-round-zt-20261018/t3-gn-report.md`). Generational mode (`CRATONVM_ZGC_GENERATIONAL=1`) is opt-in, so none of these reaches a default run today; each is a precondition or a cost item of the default plan, `docs/internal/proposals/gc/zgc-zt3gn-proposal-generational-as-the-default-20261019.md` (stage named per item). Line numbers are on `zt3-gn` (base `ac2059ae81`).

# ZGC generational mode: review residuals (wave t3)

Fixed by the same review, for reference (not open here): GN-1 `System.gc()` ran a young cycle
(`CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR`), GN-2 the G5 occupancy clause read `allocated`, GN-3 the
interpreter `aastore` fast path now produces element cards (zr z2/g2's X1). GN-4 (young cycles
process references and finalizers) has its own page,
`zgc-zt3gn-young-cycles-process-references-and-finalizers-20261019.md`.

## GN-5. `System.gc()` does not tenure the young survivors (stage S3)

HotSpot's `System.gc()` major promotes every live young object (gci i47w/sg47's
`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON, reaches the generational backend and the
`ZGenerationalHeap` model, `gc/src/zgc/generation.rs:762`). The real ZGC heap ignores the request:
`age_survivor` (`gc/src/zgc.rs:10447`) adds one to each survivor's age on every cycle, a major
included. Not Java-visible in pools (the ZGC pool split is the cycle watermark,
`jmx_young_old_split`), but after a `System.gc()` the survivors are still young, so the next young
cycles trace them again, up to `promotion_age - 1` more times. **Closes with:** in the sweep of a
whole-heap generational cycle with `gc_quiescence::system_gc_tenure_requested()`, age every
survivor straight to `promotion_age()` (card + `note_old_start`, as `age_survivor` does at
promotion). Row: `Zr1GnGenShape` with a `System.gc()` per round, `minor_mark_us` falls.

## GN-6. Old wholesale roots are not traced by a young cycle (audit, stage S0)

`young_extra_roots` section (0) (`gc/src/zgc.rs:10148`) roots every pinned loader, pinned mirror,
metadata object and native-overlay entry WHOLESALE. An old one is pre-marked, so the marker's
dedup drops it and its fields are never traced on a young cycle (the comment that called every
extra root "young by construction" was corrected in this wave). That is sound only if every young
object stored into a field of an old `Class` mirror, class loader or metadata object arrived
through a carded store. Stores reach the card through `set_field_no_satb` / `set_array_element` /
`write_barrier` / `ref_cas_post_barrier` and the interpreter fast paths; a VM native that writes a
mirror or loader field through a raw slot write (no accessor) would be a dropped edge. **Closes
with:** a census of the VM's raw reference-slot writers on ZGC (`rg 'write_ref_slot|set_field_raw'`
outside the accessors), each either routed through `note_ref_store` or shown to write only young
receivers; and a verify arm: under `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY=1`, after a young mark,
walk the old wholesale roots' slots and count young unmarked targets (must be 0).

## GN-7. The JIT post-barrier gate skips only age-0 receivers (stage S2b)

The ZGC heap publishes `young_floor = JIT_YOUNG_FLOOR_AGE_ZERO` (`gc/src/zgc.rs:4170`;
`gc/src/gen_heap.rs:64970`), so a compiled reference store into a receiver of age
`1..promotion_age-1` calls the barrier helper (`write_barrier` -> `note_ref_store_slow`, a header
read and a return) on every store once the heap has an old generation. A floor at the promotion
age skips every young receiver, but must be lowered BEFORE the promotion age it mirrors is lowered
(`gen_heap.rs`'s note on `JIT_YOUNG_FLOOR_AGE_ZERO`). **Closes with:** publish
`promotion_age << 4` from `set_gen_promotion_age` (lower first, then the age; raise the age first,
then the floor) and from construction. Row E-put (proposal): `Zr1GnGenShape S1` CPU on the G arm,
JIT on, before/after; `SP_REF_STORE_*` helper counts fall.

## GN-8. Compiled `aastore` cards the whole array (stage S2b)

zr z2/g2's X2 is not built: the JIT post barrier passes only the receiver, so under
`CRATONVM_ZGC_GEN_ELEMENT_CARDS=1` every compiled store into a large old array re-dirties an
object card that the next young cycle converts with a whole-array scan
(`young_extra_roots`' conversion arm). **Closes with:** a `jit_write_barrier_element(obj, slot,
len)` helper calling `ZgcRealHeap::note_ref_store_element`, the x64 `aastore` emitter passing the
element address it already computed. Row E1 of `docs/internal/zgc-round-zr-20261012/z2-g2-report.md`
with the JIT on.

## GN-9. Young cycles are stop-the-world only (stage S5 decision)

A concurrently marked cycle is always whole-heap (`marked_concurrently` in `force_major`,
`gc/src/zgc.rs:30438`), and `CRATONVM_ZGC_CONC_EVERY_TRIGGER` is inert in generational mode
(`conc_every_trigger_armed`). HotSpot marks young concurrently. A young STW pause near full is
the stage-1 lever; a concurrent young mark needs the SATB pre-barrier to cover young-only marking
and the card barrier to stay sound during it. **Closes with:** a decision at the flip: either
generational keeps STW young cycles (and the opt-in concurrent modes stay whole-heap), or a
proposal for a concurrent young mark with its own rows.

## GN-10. A young cycle still streams O(heap / 64) bytes (stage S4)

Four passes per young cycle are proportional to the bumped arena, not to the young generation:
the registry snapshot (`snapshot_within`, `gc/src/zgc.rs:30367`, heap/64 bytes copied), the mark
clear (`mark_clear_all`, 30517, heap/512), the pre-mark (`premark_from_old_bits`, 10658: reads
registry + old bits, heap/64 + heap/512) and the young-aged sweep's word walk
(`sweep_young_aged`, 10761: the same two again). At a 256 MiB heap that is ~4 MiB copied and
~9 MiB read: by arithmetic (~5 GB/s) 2-3 ms, against the ~39 ms whole-heap trace (zt t3/tp).
Not the first lever, but the floor of a young pause. **Closes with:** a per-word "has a non-old
base" summary maintained by the sweep and the allocator, so the young-cycle passes visit only
words that can hold young objects. Decided by `minor_pre_us` + `minor_sweep_us` on
`[GC] zgc-gen-cost:` in row R1 of the proposal: worth building only if they exceed 25 % of
`minor_pause_us`.

## GN-11. The young pool reads zero after every collection (MXBeans; ob lane)

`jmx_young_old_split` (`gc/src/zgc.rs`, zr z2/t2) reports the young pool as bytes allocated since
the last collection, so on a generational heap the young pool is 0 right after a minor although
the minor's young survivors are still young. HotSpot's `ZGC Young Generation` pool after a minor
holds the survivor pages. **Closes with:** on a generational heap, young = live young bytes
(`live_bytes - gen_old_live_bytes - gen_nursery_old_bytes` after the cycle) + allocated since.
Row: a probe printing the young pool's `getCollectionUsage().getUsed() > 0` after churn with a
live young set, HotSpot twin.

## GN-12. `gen_force_major_next` escalates only on a young cycle that freed NOTHING (stage S1 rows)

`gen_force_major_next.store(bytes_freed == 0)` (`gc/src/zgc.rs:31791`). Near full, a young cycle
that frees a few KiB is repeated at the near-full progress floor's rate until an allocation fails
(`after_alloc_failure` then forces the major). That failure is the HotSpot shape (an allocation
stall triggers its major), so this is not obviously wrong; it is the count to read in rows R1/R2
(`minor_cycles` against `minor_dead`) before the flip. **Closes with:** the R1/R2 rows showing no
run of more than a few minors that each free under 1 % of capacity; else an escalation on
`bytes_freed < nursery_budget / 8`.

## GN-17. A young cycle sweeps every object allocated since the last cycle (stage S2) -- zt t5

The S0 gate failed on `Zr1GnGenShape 64 400 200000 5 8 64 -Xmx1g`: a young cycle ~210 ms, a major
~190 ms (orchestrator rows). The default young sweep, `sweep_young_aged`, visits every non-old
registered base through `sweep_one` -- a header read of a cold object, `alloc_size`, a run merge --
so it is O(objects allocated since the last cycle, dead or alive), serial
(`sweep_workers(gen_on)` is 1). A major's complement sweep never reads a dead object's header.
**Closes with:** stage S2 of the restaged proposal -- first the row with the opt-in
`CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT=1` (G6), then a sharded young complement, then (only if still
short) a real young space.

## GN-18. The floor arm counts the large-object region's old objects twice -- zt t5

Only under `CRATONVM_ZGC_GEN_OLD_BITMAP=0` (the floor-bounded young sweep). That arm sweeps every
base from the floor upward, which includes the large-object region's OLD objects (pre-marked, so
they survive and land in `bytes_copied`), while the major already put them in
`gen_old_live_bytes`. GN-15's fix (young high-region survivors) does not cover it. **Closes
with:** on the floor arm, count only non-old survivors above the floor, or retire the arm (it is
the unsound fallback `gen_young_refused_for_bitmap` already avoids by default).
