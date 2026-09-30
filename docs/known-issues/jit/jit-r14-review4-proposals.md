# JIT round 14 wave 4, lane review4: proposals

Ranked. Each came out of reviewing the wave-3 commit (6e4d18012); findings are
on `r14w4-review4-wave3-review-findings-FIXED-20260929.md`.

## RV4-1: one `sync_direct_lookup` per site per compile

Benefit: a static synchronized site whose callee has no published wrapped body
costs one `sync_direct_target` (and at most one eager callee compile) instead of
two; the SS-2 mirror path stops re-running the caller-held path's refusals.
Cost: small -- a `HashMap<u16, Option<SyncDirectTarget>>` memo local to
`ir_tier_attempt`, wrapping `req.sync_direct_lookup` at its five call sites
(`jit/src/lib.rs` ~39644, ~39719, ~39818, ~40596). Risk: low; the lookup is a
pure function of the class state during one compile, except the eager compile
it may trigger, which the memo only de-duplicates.
First step: count calls per compile under `CRATONVM_DBG_JITC` on
`R14SyncSpliceStatic` to confirm the double call happens.

## Round 14 wave 5 (lane fixup2): RV4-1 landed

Memo per compile REQUEST rather than per `ir_tier_attempt`:
`try_compile_request` wraps the lookup (`sync_direct_lookup_memoized`), so the
IR rebuild attempts, the local-handler retry, the single-pass fall-through and
tables, and the parity shadow share it too. Switch
`CRATONVM_JIT_SYNC_DIRECT_LOOKUP_MEMO` (default on).

## RV4-2: a per-VM `Random` layout memo for the SH3-2 field road

Benefit: each `Random` draw native does `class_id_by_name` plus two field-index
resolutions today (`securerandom.rs` `exact_random_seed_cell`); a memo of
`(random class id, seed slot, AtomicLong value slot)` makes that three loads,
which matters for `nextBytes`-free hot loops calling `nextInt` per element.
Cost: one `OnceLock`-free per-VM slot (an `env_cache` `MemoSlot` or a field on
the native registry's per-VM state -- no process global). Risk: low; invalidate
on redefinition of `java.util.Random` (never happens in practice).
First step: A/B `apps/probes/RandomBench` with `CRATONVM_RANDOM_REAL_SEED_FIELD=0/1`.

## Round 14 wave 5 (lane fixup2): RV4-2 landed

A per-THREAD `RandomLayout` cell keyed by `vm_identity` (never recycled, never
0 for a real VM), not a shared slot: no lock between drawing threads, no
process global, nothing to forget at VM disposal (one row per thread). Every
object read is still checked; a miss re-derives. Switch
`CRATONVM_RANDOM_LAYOUT_MEMO` (default on). Unmeasured: A/B with the switch on
`R14Fixup2RandomLayout`.

## RV4-3: auto-arena sweep off the allocating thread

Benefit: FFM7-1's sweep (resolve every row's weak handle, free dead blocks)
runs on whichever mutator's allocation crosses the threshold, and since wave 4
only one thread sweeps at a time, so a burst of short-lived arenas concentrates
the whole cost on one allocation. Running it from the GC's post-collection hook
(where the weak handles were just cleared) would free promptly and keep the
allocation path to a record. Cost: a per-VM post-GC callback into
`native-builtins` (the GC round is closed: needs an owner decision), plus the
`free_native_memory` calls outside any GC lock. Risk: medium (GC-adjacent).
First step: measure `R14FfmArenaAutoFree` allocation latency percentiles with
and without `CRATONVM_FFM_AUTO_ARENA_BLOCK_FREE`.

## RV4-4: `hot_loop_ranges` and `osr_entry_loop_ranges` share one back-edge decoder

Benefit: the two disagree on `goto_w` today (the first ignores it), and the
`single_bytecode_decoder_ratchet` intent is one decoder per question. Cost: a
small shared `backward_branch_target(code, pc)` helper in `jit/src/lib.rs`.
Risk: low (pricing only). First step: unit test with a `goto_w` loop.

## Round 14 wave 5 (lane fixup2): RV4-4 landed

`loop_back_edge_header` (offset via `bytecode_analysis::offset_branch_target`)
now serves `hot_loop_ranges`, `bytecode_loop_ranges`, `osr_entry_loop_ranges`
and `bytecode_pc_in_loop`. The premise was stale: `hot_loop_ranges` already
decoded `goto_w`; the four copies were merely one edit from disagreeing. No
switch (a pure refactor; answers are unchanged except that a `code_len` past
the slice no longer indexes out of bounds).
