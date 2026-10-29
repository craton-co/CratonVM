// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane REFS): page
//! `j27-referent-legacy-arm-skips-the-zgc-resurrection-gate-FIXED-20261029.md`.
//! On a heap whose ZGC referent read gate may arm, every referent read --
//! handed out (`Graph::referent_load_pcs`) or compare-only
//! (`Graph::no_keepalive_referent_loads`) -- takes the checked `jit_getfield`
//! helper, which runs `gate_referent_read`; no inline arm (legacy-only
//! included) runs that gate.

use crate::direct_helpers::{DirectHelperFnThreadLocalIrLayout, REFERENT_GATE_MAY_ARM_QUERY_WORDS};
use crate::DirectHelperTable;

unsafe extern "C" fn may_arm(ctx: i64, out: i64, n: i64) -> i64 {
    if ctx != 0x1000 || n != REFERENT_GATE_MAY_ARM_QUERY_WORDS as i64 {
        return 0;
    }
    (out as *mut u64).write(1);
    1
}

unsafe extern "C" fn never_arms(_ctx: i64, out: i64, n: i64) -> i64 {
    if n != REFERENT_GATE_MAY_ARM_QUERY_WORDS as i64 {
        return 0;
    }
    (out as *mut u64).write(0);
    1
}

unsafe extern "C" fn declines(_ctx: i64, _out: i64, _n: i64) -> i64 {
    0
}

fn table(f: DirectHelperFnThreadLocalIrLayout) -> DirectHelperTable {
    DirectHelperTable {
        thread_local_ir_layout: f as *const () as usize,
        static_base_resolver_ctx: 0x1000,
        ..DirectHelperTable::EMPTY
    }
}

/// The table reader: `true` only on the VM's explicit `1`; a VM that
/// declines the query (or no VM) answers `false`, the pre-j27 behaviour.
#[test]
fn j27_refs_the_gate_query_reads_the_vm_answer() {
    assert!(table(may_arm).referent_gate_may_arm());
    assert!(!table(never_arms).referent_gate_may_arm());
    assert!(!table(declines).referent_gate_may_arm());
    assert!(!DirectHelperTable::EMPTY.referent_gate_may_arm());
    // The producer tells its queries apart by `n` alone.
    for other in [
        crate::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS,
        crate::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS,
        crate::direct_helpers::THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS,
        crate::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS,
        crate::direct_helpers::REFERENCE_GET_LAYOUT_QUERY_WORDS,
        crate::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS,
    ] {
        assert_ne!(REFERENT_GATE_MAY_ARM_QUERY_WORDS, other);
    }
}

/// The lowering's rule: a referent read (either kind) owes the helper
/// exactly when the gate may arm; an ordinary load never does.
#[test]
fn j27_refs_a_referent_read_owes_the_helper_when_the_gate_may_arm() {
    use crate::ir_lower::referent_read_owes_the_checked_helper as owes;
    assert!(owes(true, true, false), "handed-out read, gate may arm");
    assert!(owes(true, false, true), "compare-only read, gate may arm");
    assert!(!owes(false, true, false), "gate never arms: inline as before");
    assert!(!owes(false, false, true));
    assert!(!owes(true, false, false), "not a referent read");
}

/// The rule is asked before every row and arm of the inline `getfield`,
/// the legacy-only arm included, and from the compile's helper table.
#[test]
fn j27_refs_the_inline_getfield_asks_the_rule_first() {
    let src = include_str!("ir_lower.rs").replace("\r\n", "\n");
    let start = src
        .find("    fn emit_inline_compact_getfield(")
        .expect("the inline getfield arm");
    let body = &src[start..];
    let body = &body[..body.find("\n    \x7d\n").expect("its end")];
    let rule = body
        .find("referent_read_owes_the_checked_helper(")
        .expect("the arm asks the rule");
    let legacy = body
        .find("let legacy_only = row.is_none()")
        .expect("the legacy-only arm");
    assert!(rule < legacy, "the rule comes before the legacy-only arm");
    assert!(body.contains("self.direct_helpers.referent_gate_may_arm()"));
}

// ── j27 wave 7 (lane T1ACCEPT, `CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW`):
// the spliced `Reference.get` referent row on a ZGC heap whose gate never arms.

const ZGC_SPLICE_SWITCH: &str = "CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW";

/// The referent-row answer of a ZGC heap whose referent gate never arms: no
/// handed-out row (word 1), the compare-only row 17 (word 3).
unsafe extern "C" fn zgc_rows(ctx: i64, out: i64, n: i64) -> i64 {
    let want = crate::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS as i64;
    if ctx != 0x1000 || n != want {
        return 0;
    }
    let out = out as *mut u64;
    for (i, w) in [0u64, 0, 0x44, 17].into_iter().enumerate() {
        out.add(i).write(w);
    }
    crate::direct_helpers::REFERENCE_REFERENT_ROW_ANSWER_WORDS as i64
}

/// The choice: the handed-out row wins; with none, the compare-only row
/// only under the switch.
#[test]
fn j27_t1accept_splice_row_choice() {
    use crate::ir::spliced_referent_row_choice as choose;
    assert_eq!(choose(9, 9, false), 9, "Gen / G1: the handed-out row");
    assert_eq!(choose(9, 9, true), 9);
    assert_eq!(choose(0, 17, false), 0, "ZGC, switch off: no row (the helper)");
    assert_eq!(choose(0, 17, true), 17, "ZGC, gate never arms: the row");
    assert_eq!(choose(0, 0, true), 0, "ZGC, gate may arm: no row either way");
}

/// End to end through the compile's helper table: a ZGC-shaped answer gives
/// the splice a row only with the switch on; default off.
#[test]
fn j27_t1accept_splice_takes_the_zgc_row_only_under_the_switch() {
    let t = table(zgc_rows);
    let row = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(&[(ZGC_SPLICE_SWITCH, v)], || {
            crate::ir::spliced_referent_row_from_table(&t)
        })
    };
    assert_eq!(row(None), 0, "opt-in: default off");
    assert_eq!(row(Some("0")), 0);
    let want = if cratonvm_types::compact_ref_fields_enabled() { 17 } else { 0 };
    assert_eq!(row(Some("1")), want);
    // A VM that declines the query names no row under the switch either.
    let none = cratonvm_types::flags::with_thread_overrides(&[(ZGC_SPLICE_SWITCH, Some("1"))], || {
        crate::ir::spliced_referent_row_from_table(&table(declines))
    });
    assert_eq!(none, 0);
}

/// What the row's soundness rests on in the lowering: a `referent_load_pcs`
/// read with a row tests the pre-gate byte after its compact read (the
/// keep-alive), and the colored arm (an armed ZGC barrier) refuses volatile
/// loads, which referent loads are.
#[test]
fn j27_t1accept_a_referent_read_with_a_row_keeps_its_gates() {
    let src = include_str!("ir_lower.rs").replace("\r\n", "\n");
    let start = src
        .find("    fn emit_inline_compact_getfield(")
        .expect("the inline getfield arm");
    let body = &src[start..];
    let body = &body[..body.find("\n    \x7d\n").expect("its end")];
    let read = body
        .find("self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]")
        .expect("the compact reference read");
    let gate = body[read..]
        .find("if referent_gate {\n                    self.emit_mov_reg_imm64(R11, self.satb_pre_gate as u64);")
        .expect("the after-read pre-gate test");
    assert!(gate > 0);
    assert!(
        body.contains("Some(b) if node_ty == IrType::Ref && !legacy_only && !volatile_access => Some(b)"),
        "the colored arm admits no volatile load"
    );
}
