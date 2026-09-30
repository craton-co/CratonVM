# Proposal: a blocked thread replays each pause's map at wake instead of composing a chain inside every pause

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 40
> of 54).** Not built. The counter its w6-g status asked for now exists:
> `GcBlockState::blocked_folds` (gcd d5/f, `vm/src/threading/jvm_thread.rs`),
> bumped per fold in `fold_pointer_map_into_blocked_audited`; the `k`
> histogram on the `[blockgc] wake` line is not printed yet. **Gate:** the `k`
> histogram on a parked-pool run; retire the page if k=1 dominates. **Size:**
> XS (histogram), M (the replay).

> **STATUS (gc-common w6-g, 2026-09-24): refreshed, not landed; the safe
> first step is named precisely.** Re-read of the wake half this wave:
> `vm_exec.rs::check_post_block_gc_refs` still `mem::take`s ONE composed
> `gc_block_state.fixup` after `leave_blocked_region_flagged`, and applies
> it to the channel list recorded below. It has no way to tell how many
> pauses it slept through: `GcBlockState` has no fold count. So neither the
> cost the proposal moves (k maps × slots at wake) nor the one it removes
> (the in-pause fold) can be sized from the wake side.
>
> The safe first step is measurement, and it lives in the fold half (not
> lane G's). Add an `AtomicU32 folds` to `GcBlockState` (`jvm_thread.rs`),
> and increment it in `ThreadRegistry::fold_pointer_map_into_blocked_audited`
> (`thread_registry.rs`) for each blocked thread it composes into. Then the
> wake half (lane G) reads and resets it next to the `fixup` take, and feeds
> a `k` histogram (1, 2-4, 5-16, >16) printed with
> `CRATONVM_DBG_BLOCKGC`'s `[blockgc] wake` line. If nearly every wake has
> `k = 1`, composition costs nothing that replay would save, and this
> proposal should be retired. If `k` is routinely large, the replay's list
> bound (64) needs the fallback described below. Both halves have to land
> in one wave with one owner, as before.

> **STATUS (gc-common w5-g, 2026-09-24): unchanged, kept as the structural
> direction for a future round that owns `thread_registry.rs`,
> `jvm_thread.rs` and the wake half together.** The wake half
> (`check_post_block_gc_refs`, `apply_pending_blocked_fixups`) did not
> change this wave, so the replay's channel list is still the one w4-g
> recorded.

> **STATUS (gc-common w4-g, 2026-09-23): unchanged, not landed.** Same
> blocker as w3-g recorded: the pause-side half (`fold_pointer_map_into_blocked_audited`
> pushing `(Arc<PointerMap>, captures)` pairs) is in `thread_registry.rs` /
> `jvm_thread.rs`, which are not the G lane's, and the wake half
> (`check_post_block_gc_refs`, `apply_pending_blocked_fixups`, G's) cannot land
> alone -- it must know which representation it was handed. Re-read of the
> wake half for this wave: it now also applies the chain to JNI locals
> (`update_local_refs_after_gc`) and the deopt stash, so the replay's channel
> list is the one in `check_post_block_gc_refs` today, unchanged from the
> proposal's item 2. Keep for a round that owns all three files.

> **STATUS (gc-common w3-g, 2026-09-23): evaluated, not landed.** Sound in
> outline, and the cost argument holds. But the pause-side half (the fold
> pushing `Arc<PointerMap>`s instead of composing) is in `thread_registry.rs`
> and `jvm_thread.rs` (`GcBlockState`), not w3-g's; only the wake half
> (`check_post_block_gc_refs`, `apply_pending_blocked_fixups`) is, and one half
> cannot land alone -- the wake must know which representation it was handed.
> A precondition found while evaluating: the fold also adopts this pause's
> blocked-peer native-stack captures (`take_peer_stack_slots()`), which are
> keyed to THIS pause's map and drained in the pause, so the replay list has to
> carry `(map, captures)` pairs, not maps alone. The in-pause cost it targets
> is reduced, not removed, by
> `docs/internal/gc-common-round-20260923/applied/handoff-w3g-blocked-fold-costs.md`.
> Keep as the structural direction for a round that owns both files.

**Status:** OPEN (proposal) — filed 2026-09-23 by gc-common round, wave 2, lane G.

## Why

A thread asleep in a blocking region cannot apply a pause's pointer map, so
every relocating pause currently does it a favour inside the pause:
`fold_pointer_map_into_blocked_audited` composes the map into the thread's
`fixup` chain (`orig -> cur -> new`), seeds first moves from the snapshot,
advances `slot_origins` and the captured native slots, and remaps the
snapshot. The wake then applies the composed chain once.

That design carries three costs this round keeps paying for:

* **Pause time.** All of it runs serially on the initiator for every blocked
  thread (see `common-w2g-per-gc-remap-costs.md` §1).
* **Composition subtlety.** A composed chain is keyed by the ORIGINAL
  addresses and valued by the CURRENT ones, and under a sliding compactor a
  current value is routinely also some other entry's key. Anything that looks
  a composed value up again moves it onto the wrong object — the defect class
  behind the Throwable-trace double lookup, the `apply_native_slot_fixups`
  ordering and the "a second lookup misses" comment corrected this wave. The
  seeding step needs an ABA guard (`or_insert`) and a `chained` set for the
  same reason.
* **Two repair mechanisms.** Because a seed can be missed (a slot not in the
  filtered snapshot), `slot_origins` exists as a second, exact tracker, and the
  wake applies both.

## Proposal

Keep the snapshot remap in the fold (the NEXT collection marks from the
snapshot, so it must be current), and replace the rest with a list of the maps
themselves:

1. The fold pushes `Arc<PointerMap>` (the barrier already hands maps out as
   `Arc`s since this round) onto `gc_block_state.pending_maps` — one pointer
   copy per blocked thread per pause, no per-entry work.
2. The wake (`check_post_block_gc_refs`, `apply_pending_blocked_fixups`)
   applies the maps IN ORDER to every channel it applies the chain to today:
   frames, `monitor_on_exit`, the shared off-frame list, the deopt stash, JIT
   frames / register images / shadow stack, native slots, JNI locals,
   `extra_refs`. Each map is looked up once per slot, against the address the
   slot holds after the previous map — exactly what a running thread does at
   each safepoint, so no composition rule exists to get wrong.
3. `slot_origins` and the chain seeding are no longer needed for correctness
   (a slot that was not in the snapshot is still rewritten: replay does not
   depend on seeds). Keep `slot_origins` one release as a verifier.

Cost moves from the pause to the waking thread: `k` maps × slots instead of
one composed map × slots. For a thread that slept through many pauses, bound
it by composing on the waking side when `k` exceeds a threshold (the same
composition, done once, off the pause).

Memory: each retained map lives until the last blocked thread that slept
through its pause wakes. Bound the list (e.g. 64 maps); on overflow compose
the oldest two on the fold side — today's behaviour, as a fallback.

## Risks

* The object a slot names must still have been rooted during each pause —
  unchanged: it is the deposited snapshot, remapped in-pause as today.
* A leaked blocked region keeps maps alive until its heal; the heal already
  exists (`update_root_snapshot`, now clear-flag-then-heal).

## Retire when

The fold does no per-entry work beyond the snapshot remap, the wake replays
maps, and `ConcurrencyUnderGcSweep` / `MtChurnProbe` with a parked pool show
the fold's pause share gone with `CRATONVM_DBG_BLOCKGC` WAKE-STALE silent.
