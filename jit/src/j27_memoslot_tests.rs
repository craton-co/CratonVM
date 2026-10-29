// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane MEMOSLOT: per-op compile switches that were
//! `OnceLock<bool>` latches are now [`crate::memo_flag::memo_bool`] slots, so
//! `with_thread_overrides` reaches them while the read stays one load. Each
//! test reads the default, the switch's own off (or on) word, and the default
//! again: a latch answers all three with whatever the process saw first.

use cratonvm_types::flags::with_thread_overrides;

/// `f` read with `key` overridden to `value` (`None` = as if unset).
fn read(key: &str, value: Option<&str>, f: fn() -> bool) -> bool {
    with_thread_overrides(&[(key, value)], f)
}

/// A default-ON switch: on unset, off at every word in `kills`, on unset
/// again; `keeps` lists words that must NOT kill it (the truth table kept).
fn assert_default_on(key: &str, f: fn() -> bool, kills: &[&str], keeps: &[&str]) {
    assert!(read(key, None, f), "{key} unset must be ON");
    for &word in kills {
        assert!(!read(key, Some(word), f), "{key}={word} must be OFF");
    }
    for &word in keeps {
        assert!(read(key, Some(word), f), "{key}={word} must stay ON");
    }
    assert!(read(key, None, f), "{key} unset again must be ON");
}

/// The optimizing tier's lowering encodings (`ir_lower.rs`), read per node.
#[test]
fn a_thread_override_reaches_the_ir_lowering_encodings() {
    use crate::ir_lower as lo;
    let three = ["0", "false", "off"];
    let four = ["0", "false", "off", "no"];
    let rows: [(&str, fn() -> bool, &[&str], &[&str]); 8] = [
        ("CRATONVM_JIT_IR_CARRY_2ND", lo::ir_carry_second_operand_enabled, &three, &["no"]),
        ("CRATONVM_JIT_IR_CARRY_RCX_FOLDED", lo::ir_carry_rcx_folded_enabled, &three, &["no"]),
        ("CRATONVM_JIT_IR_ADD_LEA", lo::ir_add_lea_enabled, &three, &["no"]),
        (
            "CRATONVM_JIT_IR_BRANCH_LAYOUT_POLARITY",
            lo::ir_branch_layout_polarity_enabled,
            &three,
            &["no"],
        ),
        ("CRATONVM_JIT_IR_PHI_COPY_DIRECT", lo::ir_phi_copy_direct_enabled, &three, &["no"]),
        ("CRATONVM_JIT_IR_CMP_IN_PLACE", lo::ir_cmp_in_place_enabled, &three, &["no"]),
        ("CRATONVM_JIT_IR_EPOCH_GUARD_RIP", lo::ir_epoch_guard_rip_enabled, &four, &["1"]),
        ("CRATONVM_JIT_IR_AASTORE", lo::ir_aastore_enabled, &four, &["1"]),
    ];
    for (key, f, kills, keeps) in rows {
        assert_default_on(key, f, kills, keeps);
    }
    // An opt-out `NO_` key: unset leaves the staged slot on, `=1` turns it off.
    let staged = "CRATONVM_NO_JIT_STAGED_ARG_SLOT";
    assert!(read(staged, None, lo::staged_arg_slot_enabled));
    assert!(!read(staged, Some("1"), lo::staged_arg_slot_enabled));
    assert!(read(staged, None, lo::staged_arg_slot_enabled));
}

/// The single-pass tier's per-op emission switches (`x64/licm.rs`); the
/// checkcast fast path is read by both tiers.
#[test]
fn a_thread_override_reaches_the_single_pass_emission_switches() {
    use crate::x64;
    // Only the exact word `0` kills these three.
    let rows: [(&str, fn() -> bool); 3] = [
        ("CRATONVM_JIT_SP_EPOCH_GUARD_RIP", x64::jit_sp_epoch_guard_rip_enabled),
        ("CRATONVM_JIT_FUSED_BOUNDS_LOAD", x64::jit_fused_bounds_load_enabled),
        ("CRATONVM_JIT_SP_FIELD_LAYOUT_GUARD", x64::jit_sp_field_layout_guard_enabled),
    ];
    for (key, f) in rows {
        assert_default_on(key, f, &["0"], &["false", "off"]);
    }
    assert_default_on(
        "CRATONVM_JIT_CHECKCAST_INLINE",
        x64::checkcast_inline_enabled,
        &["0", "false", " OFF ", "no"],
        &["1", ""],
    );
}
