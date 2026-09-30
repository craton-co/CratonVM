# Proposal: precise interpreter oop maps from the verifier's type maps

**Status: open (stage 2 complete: the >64-slot mask landed in wave 3, the worklist fixpoint and the 64 KiB code cap in wave 4; stage 1, the shadow comparison, landed in wave 5, reads the VM's own store and covers the remap since wave 7, every thread's remap since wave 8, and needs its suite runs; stages 3, 4 remain) — filed 2026-09-23 by interpreter round i1, lane L4.**

## Why

An interpreter frame's GC roots are decided today by three runtime heuristics
stacked on each other:

1. **Runtime tags.** `ValueStack::scan_object_refs` and
   `Frame::scan_local_objects` (`vm/src/runtime/value_stack.rs`,
   `vm/src/runtime/frame.rs`) root a slot when its NaN-box tag says "object".
   A `long`/`double` cannot carry a tag, so two parallel kind arrays
   (`ValueStack::kinds`, `Frame::local_kinds`) are maintained by every push and
   store to stop a tag-colliding primitive from being rooted.
2. **Lost-tag probes.** A reference local whose tag was lost is recovered by
   probing up to three pointer candidates per non-reference local with
   `is_object_address` (`lost_tag_local_candidates`), on every scan, and the
   operand stack roots untagged words that pass `is_heap_addr` (see
   `docs/internal/fixed-bugs/interpreter-L4-operand-stack-roots-unminted-primitive-longs-FIXED-20260925.md`).
3. **Liveness.** `runtime/local_liveness.rs` computes a per-pc may-live mask,
   but only for methods with at most 64 locals and 32 KiB of code
   (`MAX_TRACKED_LOCALS`, `MAX_CODE_LEN`); anything larger is all-live, which is
   exactly the generated parser / big-switch method population that holds the
   most scoped-out temporaries.

Meanwhile the verifier computes the exact verification type of every local and
stack slot at every pc and now retains it (`classloading/src/type_maps.rs`,
`MethodTypeMaps` / `FrameOopMap`, "producer complete, no consumers yet" per
`docs/internal/arch-2026-07-26/verifier-type-maps.md`). HotSpot's interpreter
oop maps are exactly type-map ∩ liveness.

## Design

Per method (keyed where `local_liveness` keys today, by code-blob identity, or
better on `CachedBytecodeMethod`):

* `oop(pc)`: the verifier's reference bits for locals and stack at `pc`
  (`FrameOopMap::locals_for_runtime_len` / `stack_for_runtime_depth`).
* `live(pc)`: `local_liveness`'s live-in mask, widened to a bitset of
  `max_locals` bits so the 64-slot cap disappears.
* Root set of a frame at `pc` = locals in `oop(pc) & live(pc)` plus stack
  slots in `oop(pc)`, read as raw pointers with NO tag inspection.

The type map only exists at verified instruction starts; `pc` and
`last_instr_pc` are both instruction starts in the interpreter, and a frame
whose pc is not in the map (e.g. `--noverify`, `jsr`/`ret` methods, synthetic
frames) keeps today's heuristic scan. That fallback must stay until every
frame producer (deopt, OSR exit, continuation thaw) is known to leave the
frame at a mapped pc.

## Staged plan

1. **Shadow mode.** Compute the precise root set next to the current one at
   every scan under a `CRATONVM_DBG_` gate and report differences (a precise
   root the heuristic missed is a latent use-after-free; a heuristic root the
   map calls non-reference is a primitive being rooted). Run the suites.
2. **Liveness without the cap.** Replace the `u64` mask with a small bitset
   (`SmallVec<[u64; 2]>` or a `Box<[u64]>` row per pc) so methods with more
   than 64 locals get liveness; raise or remove `MAX_CODE_LEN` with a worklist
   fixpoint instead of the full re-sweep.
3. **Consume for scanning.** Switch `scan_local_objects` /
   `scan_object_refs` / `update_*_refs` to the map when present; keep the
   heuristics only for unmapped frames.
4. **Delete the kind arrays** once no reader needs them: the pops that need to
   know "is this slot a long" get the answer from the opcode (they already do,
   verified bytecode) and the scanners from the map. This is the change the
   `ValueStack::kinds` doc says to make "first", and it removes a `Vec<u8>` and
   a byte store per push/store from every frame.

## Expected benefit

* Correctness: no primitive is ever rooted or remapped, no reference is ever
  missed because a tag was lost; the conservative non-moving fallbacks
  (`scan_locals_conservative`, `scan_object_refs_conservative`) stop being
  needed for interpreter frames.
* Retention: liveness for >64-local methods.
* Speed: scans become bit iteration instead of per-slot decode + probes; the
  per-push kind store disappears in stage 4.

## Risk

The map must describe the frame the collector actually sees. Any VM path that
leaves a frame at a pc the verifier did not reach, or with a stack depth other
than the map's, would be scanned wrongly — which is why stage 1 exists and why
unmapped frames keep the heuristic scan.

## Progress (wave 2)

**Landed: the 64-local cap no longer disables liveness** (`vm/src/runtime/
local_liveness.rs`). A method with more than 64 locals is now analysed; its
slots 0..63 get exact per-pc liveness and slots from 64 up are *untracked*
(their loads/stores contribute nothing to the dataflow). That needed no
API change: every consumer of the `u64` mask already treats `i >= 64` as live
(`Frame::scan_local_objects_inner`, `memory/gc.rs` stale-slot check,
`memory/reclaim_guard.rs` x3), and liveness is per-slot independent, so the
untracked slots cannot change any tracked bit. Only the `max_locals` guard
went (the parameter stays, renamed `_max_locals`); `MAX_CODE_LEN` still
bounds the fixpoint. Test: `local_liveness::tests::
slots_beyond_64_are_untracked_not_fatal` (a >64-local method is analysed, a
scoped-out temp in a low slot is dead, a `dload 63` straddling the boundary
keeps slot 63 live).

What is left of stage 2, now concrete:

* **Slots >= 64.** Widen the mask type. The narrowest change is a
  `LiveMask` newtype (`u64` inline + `Option<Box<[u64]>>` spill, or
  `SmallVec<[u64; 1]>`) with `is_live(i)`, returned by
  `live_locals_mask` and `Frame::live_locals_mask_here`; the five consumers
  above switch from `i >= 64 || mask & (1 << i) != 0` to `mask.is_live(i)`.
  The dataflow's `use_mask`/`def_mask` become the same type. Two consumers are
  in `memory/` (another lane), hence not done here.
* **`MAX_CODE_LEN` (32 KiB).** `analyze` iterates reverse sweeps until no
  change, O(n x depth-of-loop-nesting). A worklist fixpoint (push
  predecessors of a changed instruction) bounds it at O(edges x slots/64)
  and would let the cap rise to the class-file maximum (64 KiB).
* **Memory.** `LivenessTable::live_in` is an `FxHashMap<u32, u64>` per
  instruction start. For the large methods this change newly admits, a dense
  `Box<[u64]>` indexed by an instruction-start rank (as `QuickenedCode`
  already builds for pc -> instruction) is smaller and a cheaper lookup.

Stage 1 (shadow mode against `classloading/src/type_maps.rs`) is untouched and
still the next step for the precise-map direction proper.

## Progress (wave 3)

**Landed: the mask type is widened** — slots at and beyond 64 now get
per-pc liveness, up to `MAX_TRACKED_LOCALS` = 512 (slots above that stay
untracked, i.e. live).

* `vm/src/runtime/local_liveness.rs`: `live_locals_mask` returns a new
  `LiveMask` (`is_live(slot)`, `all_live()`, `low_word()`). Slots 0..64 are an
  inline `u64`, so a method with at most 64 locals pays exactly what the bare
  `u64` did; a wider method's other words are read in place from the cached
  table (one `Arc` clone per query, no allocation). The dataflow runs over
  `ceil(slots / 64)`-word rows sized from the highest slot the bytecode names;
  `Instr` carries slot ranges (`Slots`) instead of `u64` masks, so a `wide`
  load/store at any index is modelled. The table is now `row_of: pc -> row`
  plus one dense `Box<[u64]>` of rows (smaller than the old
  `FxHashMap<u32, u64>` values for the common single-word case).
* Consumers switched from `i >= 64 || mask & (1 << i) != 0` to
  `mask.is_live(i)`: `Frame::scan_local_objects_inner` and
  `Frame::live_locals_mask_here` (`frame.rs`), `memory/gc.rs` (stale-slot
  check, two sites), `memory/reclaim_guard.rs` (three sites) and
  `vm/src/vm/vm_exec.rs` (the blocked-deposit `SlotOrigin::live` and the
  `blockgc` gap reporter). The misplaced doc comment that described
  `live_locals_mask_here` but sat on `local_kind_at` was moved back.

Tests: `local_liveness::tests::slots_beyond_64_are_tracked` (slot 71 stored
before any read is dead, 64/70 live, an untouched tracked slot dead, a slot
past the row live) and `slots_beyond_the_tracked_cap_are_live` (a `wide astore
600` stays live; tracked slot 511 is exact). Existing tests adapted to
`low_word()` / `is_live`.

Still open from stage 2: the `MAX_CODE_LEN` (32 KiB) cap and the worklist
fixpoint. Stages 1, 3 and 4 are untouched.

## Progress (wave 4)

**Landed: the worklist fixpoint and the class-file code cap** — stage 2 is
complete (`vm/src/runtime/local_liveness.rs`).

* `analyze` pass 2 is a worklist over predecessor lists: an instruction is
  re-evaluated only when a successor's live-in row changed. Rows only grow,
  so the work is bounded by edges x words x (bits that can turn on), with no
  dependence on loop nesting; the old "sweep the whole method until nothing
  changes" loop is gone. The seed order is end-to-start, the order the old
  sweep used, so straight-line code still settles in one visit.
* Exception edges are attached by binary search over the pc-sorted
  instruction list (`partition_point`) instead of a scan of the whole method
  per exception-table entry.
* `MAX_CODE_LEN` is now `u16::MAX + 2` — every method a class file can hold
  (`code_length < 65536`) plus the two bytes `frame::padded_bytecode` appends.
  Generated parser / big-switch methods between 32 and 64 KiB now get
  liveness. Worst-case table: 8 words x 64 Ki instructions = 4 MiB, for a
  64 KiB method with 512 locals.
* `global_table` no longer holds the process-wide cache `Mutex` while
  analysing: it probes under the lock, analyses unlocked, and re-locks to
  insert (two threads missing the same blob both analyse; the tables are
  identical). With the cap doubled, a large method's analysis would otherwise
  have stalled every other thread's root scan that missed its memo.

Tests: `local_liveness::tests::methods_above_32k_are_analysed` (a 40 KB
method: a scoped-out temp is dead in the trailing loop, the loop variable is
live, and liveness crosses 40 000 straight-line instructions),
`code_past_the_class_file_limit_falls_back`, and
`nested_back_edges_reach_the_fixpoint` (a slot read only at an outer loop head
is live throughout an inner loop). Every existing test covers the unchanged
semantics (the fixpoint is the same least fixpoint).

Left, optional: `LivenessTable::row_of` is still an `FxHashMap<u32, u32>`; a
dense pc-rank index (as `QuickenedCode` builds) would make the lookup and the
memory smaller for the newly admitted large methods. Stages 1 (shadow mode
against `classloading/src/type_maps.rs`), 3 and 4 are untouched.

## Progress (wave 5)

**Stage 1 landed: the shadow comparison** (interpreter round i1 wave 5,
lane L4). Opt-in, reports only, never changes a root set.

* Gate: `CRATONVM_DBG_VERIFY_OOP_MAPS` — the JIT oop-map oracle's existing
  gate; the question is the same one asked of interpreter frames, so no new
  flag. Off: one cached load per liveness-filtered frame scan
  (`scan_local_objects`, not `_all_live`, which serves the non-moving
  sweep).
* Where: `Frame::shadow_compare_oop_maps` (`vm/src/runtime/frame.rs`), called
  from `scan_local_objects_inner` after the real scan; the verdict logic and
  counters are `local_liveness::oop_map_shadow`.
* What it compares. The heuristic decision is the real scan's, asked again
  one slot at a time (the loop bodies became `Frame::scan_locals_in` and
  `ValueStack::scan_object_refs_in` over a slot range, so there is one copy of
  each rule; the stack's `CRATONVM_DBG_LONGROOT` census is not re-counted).
  The precise side is `type_maps_for_named(class_id, name, descriptor)` read
  at BOTH pcs the liveness filter unions (`pc`, `last_instr_pc`): a slot is a
  reference if either mapped pc says so, a non-reference only if every mapped
  pc does, unknown if neither pc has a row. The operand stack is compared only
  at a pc whose recorded depth equals the runtime depth (a frame stopped
  inside an invoke has popped its arguments and sits between two rows).
* Verdicts: `Missed` (the map says a live reference, the slot holds a
  non-zero value, the scan did not root it — a latent use-after-free),
  `Extra` (the scan rooted a slot every mapped pc calls a primitive or
  `Top` — over-retention, and on a moving collection a word the remap may
  rewrite), `Unknown`. Output: the first 40 differences as
  `[interp-oopmap] Missed Local[3] bits=0x... in Cls.m(desc) pc=.. last_pc=..`,
  and a `[interp-oopmap] summary frames= unmapped_frames= stack_depth_mismatch=
  slots= unknown= missed= extra=` line at every power-of-two difference count.
* Tests: `local_liveness::oop_map_shadow::tests::{map_answers_fold_across_the_mapped_pcs,
  classify_names_each_disagreement}`, and
  `frame::tests::oop_map_shadow_reports_missed_and_extra_locals` (a published
  map; a reference-typed slot holding an int is `Missed`, an `int`-typed slot
  holding a live object is `Extra`).

**What to run, and how to read it (the orchestrator; this lane may not run
the VM).**

1. `CRATONVM_DBG_VERIFY_OOP_MAPS=1 --nojit` on: the H2 `TestAll` subset used
   for the JIT oracle, a Spring Boot petclinic boot, and the BouncyCastle
   math-ec suite (the long-collision workload). Use a small heap so there are
   many collections: the comparison runs once per frame per root scan.
2. Read `missed` first. Every `Missed` line names a class, method, pc and
   slot; any non-zero count is a finding to reproduce before stage 3,
   because stage 3 makes the map authoritative and a heuristic that misses a
   reference the map names is exactly what it would fix. Expected sources:
   a lost tag the three lost-tag candidates do not recover.
3. Then `extra`. Expected, and the reason for stage 3: dead-but-live-by-union
   slots (`Top` at one pc, a reference at the other, counted as agreement by
   the union rule, so these should be few), and `int`/`float` slots whose
   bits happen to parse as a live object. A large `extra` with a small
   `missed` is the go signal for stage 3.
4. `unmapped_frames` over `frames + unmapped_frames` is the share stage 3
   cannot cover without a fallback (unverified classes, `--noverify`,
   synthetic frames). `stack_depth_mismatch` is the share of frames whose
   operand stack is between rows; stage 3 must keep the heuristic stack scan
   for those, or record a per-frame "mid-instruction depth" the map can be
   consulted with.

**Caveats found while landing it.**

* `type_maps` is a process-global table keyed by `ClassId`, and a `ClassId`
  is per VM: with two VMs in one process a frame can be compared against the
  other VM's class's map. Harmless for a diagnostic (it shows up as
  disagreements in both directions); stage 3 must not consume the table
  until it is keyed per VM (filed in wave 5; or key it by
  `CachedBytecodeMethod`, as the Design
  section suggests). **Fixed in wave 6**: each VM has its own `TypeMapStore`
  (`ClassRealm::type_maps`); see
  `docs/internal/fixed-bugs/interpreter-L4-verifier-type-maps-keyed-by-per-vm-class-id-FIXED-20260924.md`.
  The shadow still reads the process store (a mirror while the flag is
  armed), because the frame scan holds no VM; stage 3 must read the VM's
  store.
* The locals index space matches (JVMS slots, category-2 upper halves are
  `Top`); the stack is the map's runtime (compressed) index space, which is
  `ValueStack`'s.

## Progress (wave 6)

* **The store stage 3 needs is per VM now.** `cratonvm_classloading::TypeMapStore`,
  one per `ClassStore`, reachable lock-free as `shared.classes.type_maps`; the
  verifier publishes into it through `ClassHierarchy::type_map_store`, and the
  interpreter's fast-path gate already reads it
  (`interpreter::fast_path_admitted_uncached`). The free functions
  (`type_maps_for_named`, ...) now read the process store, which is only a
  diagnostic mirror while `CRATONVM_DBG_VERIFY_OOP_MAPS` is armed.
* **Stage 2 is done** (waves 3-4, `local_liveness.rs`): 512 tracked slots in
  a `LiveMask`, a worklist fixpoint, and methods up to the class-file code
  limit. Nothing left there for this proposal.
* **Next stage, not landed here: thread the store into the frame scan.** Stage
  3 must read `shared.classes.type_maps`, not the mirror, and
  `Frame::scan_local_objects*` / `scan_object_refs` take only the heap. The
  smallest shape is an `Option<&TypeMapStore>` parameter on
  `Frame::scan_local_objects_inner`, supplied by the per-thread root walk in
  `memory::roots` (which has `shared`) and `None` from the paths that do not
  (they keep the heuristic). That is a `memory/roots.rs` signature change on
  the GC root path, so it waits for the shadow numbers (wave 5's "What to
  run"): with `missed=0` and a large `extra`, land the parameter and the
  map-authoritative locals scan behind the existing
  `CRATONVM_NO_LOCAL_LIVENESS`-style kill switch in the same change.

## Progress (wave 7)

**The frame scan and its remap twin read the VM's own store** (interpreter
round i1 wave 7, lane L4). Still shadow mode: no root set and no rewrite
changed.

* `vm/src/runtime/frame.rs`: `Frame::scan_local_objects_mapped(roots, heap,
  maps)` is `scan_local_objects` plus the VM's `TypeMapStore`
  (`shared.classes.type_maps`); `scan_local_objects_inner` takes
  `Option<&TypeMapStore>`, and `shadow_compare_oop_maps` reads that store,
  never the process mirror. The remap twin is `Frame::update_frame_refs_mapped
  (pointer_map, heap, maps)`: exactly `update_local_refs` +
  `stack.update_object_refs`, and while `CRATONVM_DBG_VERIFY_OOP_MAPS` is armed
  it snapshots the frame first and compares what the remap rewrote
  (`shadow_compare_remap`). The row lookup both comparisons use is one helper,
  `Frame::shadow_map_rows`.
* Callers switched (the one-root-list pair first): `memory::roots::collect_roots`
  (the initiator's frame scan) and its remap twin `memory::gc::update_all_roots`;
  also the two published root snapshots other threads' frames reach the
  collector through, `interpreter::gc_and_alloc::update_root_snapshot` (default
  path) and the blocked deposit `NativeContextImpl::deposit_root_snapshot_inner`
  (`vm/src/vm/vm_exec.rs`). Every other caller of `scan_local_objects` /
  `update_local_refs` passes no store and is not compared (the cached
  root-snapshot path `scan_frame_roots`, the frozen-peer scan in
  `gc_and_alloc::stw_take_over_and_wait`, the `CRATONVM_DBG_MTROOTS` probes,
  and the non-initiator remaps `vm_exec::apply_pending_blocked_fixups`,
  `NativeContextImpl::check_post_block_gc_refs` and
  `gc_and_alloc::apply_pointer_map_to_thread`).
  The last three are the remaps of NON-initiator threads; switching them to
  `update_frame_refs_mapped` is a one-line change each if the initiator's
  remap numbers say it is worth it.
* The remap verdicts (`oop_map_shadow::Area::RemapLocal` / `RemapStack`,
  counted apart from the scan's): `remap_missed` = a slot every mapped pc
  calls a LIVE reference whose old value was a `pointer_map` key and that the
  remap left unchanged (a stale pointer the precise map would have fixed);
  `remap_extra` = a live slot the remap rewrote that every mapped pc calls a
  non-reference (a primitive the remap may have corrupted). Dead locals are
  neither.
* An exact exit reading: `oop_map_shadow::report_at_exit`, called from the
  launcher's shutdown reports (`vm-cli/src/main.rs`
  `maybe_dump_shutdown_reports`), prints
  `[interp-oopmap-summary] frames= unmapped_frames= stack_depth_mismatch=
  slots= unknown= missed= extra= remap_frames= remap_missed= remap_extra=`
  whenever the gate is armed (the running summary line stops at power-of-two
  difference counts, so it never gave the final numbers).
* The process mirror in `classloading/src/type_maps.rs` stays: the
  compiled-frame oracle (`conservative_roots::verifier_local_verdict`) still
  reads it by class name. See
  `docs/internal/fixed-bugs/interpreter-L4-type-map-process-mirror-kept-for-compiled-frame-oracle-FIXED-20260924.md`
  (removed in wave 8).

Tests: `frame::tests::oop_map_shadow_reports_missed_and_extra_locals` (now
against a private `TypeMapStore`, plus: the same frame against an empty store
is unmapped, i.e. the process store is not consulted) and
`frame::tests::oop_map_remap_shadow_reports_missed_and_extra_locals` (a
reference-typed local carrying a `long` kind mark is left stale by the remap
and counted `remap_missed`; an `int`-typed slot the remap rewrote is
`remap_extra`; `update_frame_refs_mapped` rewrites exactly as the direct pair).

Workload for the stage-1 run: `tools/probes/interp/L4/OopMapShadowWorkload.java`
(scoped-out temps, `long`s beside references, a reference on the stack across
an allocating call, a handler-only local). Run it, and the suites the wave-5
section lists, with `CRATONVM_DBG_VERIFY_OOP_MAPS=1 --nojit` and a small heap;
the go signal for stage 3 is unchanged (`missed=0` with a large `extra`), and
`remap_missed=0` is now a second precondition: stage 3 switches BOTH halves,
and a remap that already misses a map reference would keep missing it.

## Progress (wave 8)

Still shadow mode; no root set and no rewrite changed (interpreter round i1
wave 8, lane L4).

* **The three non-initiator remaps are compared too.** `vm_exec::apply_pending_blocked_fixups`,
  `NativeContextImpl::check_post_block_gc_refs` and
  `gc_and_alloc::apply_pointer_map_to_thread` now call
  `Frame::update_frame_refs_mapped(.., &shared.classes.type_maps)` instead of
  the direct `update_local_refs` + `stack.update_object_refs` pair.
  `apply_pointer_map_to_thread` gained a `maps: &TypeMapStore` parameter for
  it; its five production callers pass `shared.classes.type_maps`. Unarmed the
  rewrite is the same two calls (`update_frame_refs_mapped` snapshots and
  compares only while `CRATONVM_DBG_VERIFY_OOP_MAPS` is set), so the
  `remap_*` counters of an armed run now cover every thread's remap, not only
  the initiator's.
* **The process mirror is gone.** The compiled-frame oracle reads the VM's
  store as well (bound per thread by `conservative_roots::OracleTypeMapsScope`,
  keyed by `CompiledMethod::owner_class_id`); `classloading/src/type_maps.rs`
  no longer mirrors or keeps a class-name index. See
  `docs/internal/fixed-bugs/interpreter-L4-type-map-process-mirror-kept-for-compiled-frame-oracle-FIXED-20260924.md`.

Still not compared: the cached root-snapshot path (`scan_frame_roots`), the
frozen-peer scan in `gc_and_alloc::stw_take_over_and_wait`, and the
`CRATONVM_DBG_MTROOTS` probes, which pass no store. The go signal for stage 3
is unchanged.
