// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane LATCHES: compile-time switch readers that were
//! `OnceLock<bool>` latches now read per call, so a per-thread (and per-VM)
//! override reaches them. One test per converted family; each shows the
//! default AND the registry row's off (or on) word arriving through
//! `with_thread_overrides`, which a latch answered with whatever the first
//! reader in the process saw.

use cratonvm_types::flags::with_thread_overrides;

/// `f` read with `key` overridden to `value` (`None` = as if unset).
fn read(key: &str, value: Option<&str>, f: fn() -> bool) -> bool {
    with_thread_overrides(&[(key, value)], f)
}

/// A default-ON switch: on when unset, off at `=0`, on again when unset.
fn assert_default_on(key: &str, f: fn() -> bool) {
    assert!(read(key, None, f), "{key} unset must be ON");
    assert!(!read(key, Some("0"), f), "{key}=0 must be OFF");
    assert!(read(key, None, f), "{key} unset again must be ON");
}

/// An opt-in switch: off when unset, on at `=1`, off again when unset.
fn assert_opt_in(key: &str, f: fn() -> bool) {
    assert!(!read(key, None, f), "{key} unset must be OFF");
    assert!(read(key, Some("1"), f), "{key}=1 must be ON");
    assert!(!read(key, None, f), "{key} unset again must be OFF");
}

/// An opt-out `NO_` key read as `!runtime_flag_on(key)`: on when unset, off
/// at `=1`.
fn assert_no_key(key: &str, f: fn() -> bool) {
    assert!(read(key, None, f), "{key} unset must leave the feature ON");
    assert!(!read(key, Some("1"), f), "{key}=1 must turn the feature OFF");
}

/// `ir_schedule`: the three production schedule options and the RPO layout.
#[test]
fn a_thread_override_reaches_the_schedule_options() {
    use crate::ir_schedule::production_schedule_options as opts;
    let hot = |v| with_thread_overrides(&[("CRATONVM_JIT_IR_HOT_LAYOUT", v)], opts);
    assert!(hot(None).layout_hot_paths);
    assert!(!hot(Some("0")).layout_hot_paths);
    let list = |v| with_thread_overrides(&[("CRATONVM_JIT_IR_LIST_SCHED", v)], opts);
    assert!(list(None).priority_within_blocks);
    assert!(!list(Some("0")).priority_within_blocks);
    let pair = |v| with_thread_overrides(&[("CRATONVM_JIT_IR_PAIR_OPERANDS", v)], opts);
    assert!(pair(None).pair_single_use_operands);
    assert!(!pair(Some("0")).pair_single_use_operands);
    assert_default_on("CRATONVM_JIT_IR_RPO_LAYOUT", crate::ir_schedule::rpo_layout_enabled);
}

/// `ir_optimize`: LICM, unroll, the unreachable-frames relaxation, branchy.
#[test]
fn a_thread_override_reaches_the_optimizer_pass_gates() {
    use crate::ir_optimize as opt;
    assert_default_on("CRATONVM_JIT_LICM", opt::licm_enabled);
    assert_default_on("CRATONVM_JIT_UNROLL", opt::unroll_enabled);
    assert_default_on(
        "CRATONVM_JIT_IR_UNROLL_UNREACHABLE_FRAMES",
        opt::unroll_over_unreachable_frames,
    );
    assert_no_key("CRATONVM_NO_IR_BRANCHY", opt::ir_branchy_enabled);
}

/// `ir`: the scoped fold, the OSR-arm fresh-`new` spare, String intrinsics
/// (whose EMPTY value stays off, as before the conversion).
#[test]
fn a_thread_override_reaches_the_ir_builder_gates() {
    assert_default_on("CRATONVM_JIT_IR_SCOPED_FOLD", crate::ir::ir_scoped_fold_enabled);
    assert_default_on(
        "CRATONVM_JIT_IR_OSR_ARM_SPARES_FRESH_NEW",
        crate::ir::ir_osr_arm_spares_fresh_new_enabled,
    );
    let strings = "CRATONVM_JIT_IR_STRING_INTRINSICS";
    assert_default_on(strings, crate::ir::ir_string_intrinsics_enabled);
    assert!(!read(strings, Some(""), crate::ir::ir_string_intrinsics_enabled));
}

/// `lib.rs`: the tier-admission and table-building gates read per compile.
#[test]
fn a_thread_override_reaches_the_tier_admission_gates() {
    assert_opt_in("CRATONVM_JIT_C2_ALLOC_UPGRADE", crate::c2_alloc_upgrade_enabled);
    assert_default_on("CRATONVM_JIT_IR_CALL_ANEWARRAY", crate::ir_calls_with_anewarray_enabled);
    assert_default_on("CRATONVM_JIT_IR_VH_FIELD_FOLD", crate::ir_vh_field_fold_enabled);
    assert_default_on("CRATONVM_JIT_ELIDE_TRIVIAL_CTOR", crate::elide_trivial_ctor_enabled);
    assert_default_on(
        "CRATONVM_JIT_SCALAR_UNDER_PRECISE_FRAMES",
        crate::scalar_under_precise_frames_enabled,
    );
    assert_default_on(
        "CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD",
        crate::ir_unresumable_trap_guard_enabled,
    );
    assert_default_on(
        "CRATONVM_JIT_IR_SPLICE_REFUSE_UNBINDABLE",
        crate::ir_splice_refuse_unbindable_call_enabled,
    );
    assert_default_on(
        "CRATONVM_JIT_OBJECT_HASHCODE_STRING",
        crate::object_hashcode_string_guard_enabled,
    );
    assert_default_on("CRATONVM_C2_ACCEPT_MEMO", crate::ir_evidence::refused_methods_memo_enabled);
    let charseq = "CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC";
    assert_opt_in(charseq, crate::charseq_blind_guard_enabled);
    assert!(!read(charseq, Some(""), crate::charseq_blind_guard_enabled));
    assert_opt_in(
        "CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND",
        crate::jfr_compile_decision::deferred_new_retry_blind,
    );
    // An opt-out key read as a "disabled" predicate: unset = not disabled.
    let exc = "CRATONVM_JIT_NO_EXC_TABLE_C2";
    assert!(!read(exc, None, crate::exc_table_c2_disabled));
    assert!(read(exc, Some("1"), crate::exc_table_c2_disabled));
}

/// `lib.rs`: the direct-helper (thin bind) switches, all default ON.
#[test]
fn a_thread_override_reaches_the_direct_helper_switches() {
    let rows: [(&str, fn() -> bool); 14] = [
        ("CRATONVM_JIT_BUFFER_SESSION_DIRECT", crate::buffer_session_direct_enabled),
        ("CRATONVM_JIT_SCOPED_MEMORY_DIRECT", crate::scoped_memory_direct_enabled),
        ("CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT", crate::unsafe_accessor_direct_enabled),
        ("CRATONVM_JIT_UNSAFE_REFERENCE_DIRECT", crate::unsafe_reference_direct_enabled),
        ("CRATONVM_JIT_UNSAFE_CAS_DIRECT", crate::unsafe_cas_direct_enabled),
        ("CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER", crate::md_update_direct_helper_enabled),
        ("CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS", crate::nio_byte_direct_helpers_enabled),
        ("CRATONVM_JIT_TL_GET_DIRECT", crate::thread_local_get_direct_enabled),
        ("CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS", crate::long_box_direct_helpers_enabled),
        ("CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT", crate::varhandle_ref_read_direct_enabled),
        (
            "CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS",
            crate::varhandle_read_direct_helpers_enabled,
        ),
        (
            "CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS",
            crate::varhandle_write_direct_helpers_enabled,
        ),
        ("CRATONVM_JIT_VARHANDLE_CAS_DIRECT_HELPERS", crate::varhandle_cas_direct_helpers_enabled),
        ("CRATONVM_JIT_CENSUS_DIRECT_HELPERS", crate::census_direct_helpers_enabled),
    ];
    for (key, f) in rows {
        assert_default_on(key, f);
    }
}

/// `ir_verify`: the master gate (build-profile default) and the explicit `=0`.
#[test]
fn a_thread_override_reaches_the_verify_gates() {
    use crate::ir_verify::{pre_lower_verify_disabled, verify_enabled};
    let key = "CRATONVM_JIT_VERIFY_IR";
    assert_eq!(read(key, None, verify_enabled), cfg!(debug_assertions));
    assert!(read(key, Some("1"), verify_enabled));
    assert!(!read(key, Some("0"), verify_enabled));
    assert!(!read(key, None, pre_lower_verify_disabled));
    assert!(read(key, Some("0"), pre_lower_verify_disabled));
}

/// Single-pass and EA compile gates: lock coarsening, bulk byte loops, the C1
/// vector veto, the inline reserve path, the OSR frame-slot seed (whose strict
/// on-word set is kept: an unknown word stays off).
#[test]
fn a_thread_override_reaches_the_single_pass_compile_gates() {
    assert_opt_in("CRATONVM_JIT_LOCK_COARSEN", crate::ea_ir_bridge::lock_coarsening_enabled);
    assert_default_on("CRATONVM_JIT_BULK_BYTE_LOOPS", crate::x64::bulk_byte_loops_enabled);
    assert_default_on("CRATONVM_JIT_C1_VECTOR_VETO", crate::x64::c1_vector_veto_enabled);
    assert_no_key("CRATONVM_JIT_NO_INLINE_RESERVE_PATH", crate::x64::inline_reserve_path_enabled);
    let seed = "CRATONVM_JIT_OSR_SEED_FRAME_SLOTS";
    assert_opt_in(seed, crate::osr_entry::osr_always_seed_frame_slot);
    assert!(!read(seed, Some("2"), crate::osr_entry::osr_always_seed_frame_slot));
    assert_opt_in("CRATONVM_JIT_RANGE_BCE", crate::x64::range_bce_enabled);
    let arm64 = "CRATONVM_JIT_ARM64";
    assert_opt_in(arm64, crate::aarch64_backend::arm64_jit_enabled);
    assert!(!read(arm64, Some("2"), crate::aarch64_backend::arm64_jit_enabled));
}
