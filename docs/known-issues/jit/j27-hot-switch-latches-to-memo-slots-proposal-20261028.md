> **j27 wave 4 (lane MEMOSLOT, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.**
> First batch implemented: 13 class (b) readers (every caller compile-time,
> per emitted op or per node) are now `MemoSlot`s through the new
> `jit/src/memo_flag.rs::memo_bool` (one relaxed load; 1 off / 2 on, the
> `env_cache` encoding; cold `derive` publishes unless an override is live).
> Each closure body is the old `get_or_init` body verbatim, so every truth
> table is unchanged by construction. Converted: `ir_lower.rs`
> `ir_carry_second_operand_enabled`, `ir_carry_rcx_folded_enabled`,
> `ir_add_lea_enabled`, `ir_branch_layout_polarity_enabled`,
> `ir_phi_copy_direct_enabled`, `ir_cmp_in_place_enabled`,
> `ir_epoch_guard_rip_enabled`, `ir_aastore_enabled`, `staged_arg_slot_enabled`;
> `x64/licm.rs` `jit_sp_field_layout_guard_enabled`,
> `jit_sp_epoch_guard_rip_enabled`, `jit_fused_bounds_load_enabled`,
> `checkcast_inline_enabled` (callers checked by grep: all in `ir_lower.rs`
> lowering, `x64/*` emission, or tests). Retired test thread-locals
> `BRANCH_LAYOUT_POLARITY_FORCE` / `PHI_COPY_DIRECT_FORCE` (their five test
> sites now use `with_thread_overrides`). Statics ratchet delta **-2**
> (trail comment in `jit/tests/process_global_statics_ratchet.rs`; BASELINE
> left to lane BUILDFIX). No new switch. Deliberately left latched (class (c)
> or unsure): `ir_gc_point_maps_enabled`, `ir_deopt_regs` gate,
> `frame_block_enabled`, `ir_gp_wide_enabled` (register file vs prologue save
> set), `ir_register_authoritative_enabled` / `ir_this_nonnull_enabled`
> (deopt-home and null-check facts), and the `x64/licm.rs` map / shadow-stack /
> safepoint family. Verify: `cargo test -p cratonvm-jit --lib j27_memoslot_tests`,
> plus the rewritten `a_counted_loop_falls_through_to_its_body_and_still_sums`,
> `a_phi_copy_*`, `w7_inc6_*` tests; ratchet must read 861 against the
> reconciled baseline. The compile-CPU A/B in "Verify" below is still owed.

> **j27 wave 7 (lane T1IC, 2026-10-28): VM-side batch, PENDING HOST VERIFICATION.** In `vm/src/jit/**`, 139
> function-local `OnceLock<bool>` switch latches were handled as follows:
>
> | Outcome | Count | Sites |
> |---|---|---|
> | Now `MemoSlot`s read through `cratonvm_jit::memo_flag::memo_bool` | 132 | every other latch, mostly `conservative_roots.rs`, `xt_root_scan.rs` and `helpers/*` (`mega_*`, `site_cache_*`, `lambda_get_*` and others) |
> | Now plain per-call reads | 4 | `offload_site_late_register`, `lambda_resume_failure_raises`, `division_guard_retier_enabled`, `jit_typecheck_null_name_trace` |
> | Kept as `OnceLock` | 3 | `moving_young_enabled` and `oldmark_census_on` (their closures publish or install something), `helper_panics_fatal` (panic path) |
>
> * **How the `MemoSlot`s were made.** A binary-mode script did the conversion. Each closure is the old
>   `get_or_init` body unchanged, so every default and truth table is the same. With no override installed, each
>   slot answers what its latch did.
> * **Module.** `jit/src/memo_flag.rs` is now `pub mod` with a `pub fn memo_bool`.
> * **Ratchet.** The vm statics ratchet baseline goes from 1527 to 1523.
> * **Tests.**
>   * `cratonvm-jit`: `memo_flag::j27_t1ic_memo_bool_tests::an_override_reaches_the_slot_in_both_directions`.
>   * `cratonvm-vm`: `jit::helpers::thread::j27_t1ic_mega_bake_access_tests::the_mega_class_slot_switches_follow_a_thread_override`.
>   * Both fail on a `OnceLock`.
> * **Risk.** The GC-walker switches in `conservative_roots.rs` / `xt_root_scan.rs` are this page's class (c). A
>   test that overrides one of them on one thread now changes that thread's answer while other threads keep the
>   process answer, as any `runtime_flag_on` reader already does. Production, with no override, is unchanged.
> * **Not converted.** The non-`bool` latches (`Option<String>` filters, numeric caps) are left as they are. A
>   `MemoSlot` holds a `u8`, and several of them hand out borrowed `&'static` data.

# Proposal: hot-path JIT switch latches as `MemoSlot`s

**Status:** proposal (j27 wave 4, lane LATCHES, 2026-10-28). Not implemented.

**Area:** `jit/src` switch readers; `cratonvm_types::flags::MemoSlot`.

## Problem

Lane LATCHES converted 45 compile-time `OnceLock<bool>` switch latches in
`jit/src` to direct `runtime_flag_on` / `runtime_flag_default_on` reads. About
164 latches remain. Many stay latched for a real reason: they are read per
emitted op in a compile loop, or on a runtime path (code free, publication,
OSR entry, helper dispatch), where a flag read (two hash lookups and an
`OsString` clone) is too costly.

A `OnceLock<bool>` still ignores `with_thread_overrides`, so those switches
cannot be unit-tested in both states in one test binary. Tests work around this
with ad-hoc `#[cfg(test)]` thread-local forces (`*_FORCE`,
`RANGE_BCE_TEST_OVERRIDE`, `box_unbox_forced`, `check_elim_forced`), and each
one is another `static` line for the ratchet.

## Idea

`types/src/flags.rs` already has `MemoSlot`: one relaxed `AtomicU8` load per
read, reset by installing or dropping an override, and it refuses to publish
while an override is live. `vm/src/runtime/env_cache.rs` uses it for about 90
per-bytecode flags. A small `jit` helper (for example
`memo_flag_default_on(&SLOT, "KEY")`) would let the class (b) readers keep
their one-load cost and still honour test overrides. The `*_FORCE` test
thread-locals could then be deleted.

Limits:
- A `MemoSlot` is still a process `static`, so the ratchet count does not
  drop. The gain is that tests can reach the switch.
- It is not per-VM either.
- Class (c) latches (code layout or maps that a runtime walker reads on another
  thread) must stay process-invariant. Keep them as they are.

## Verify

A/B one per-op switch (for example `ir_add_lea_enabled`) for compile-worker
CPU with the `JkrCost` kit, then convert the family.
