// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 1 (lane TYPECHK): text pins and switch tests for the referent
//! changes of page
//! `weakref-get-and-refersto-read-a-compact-referent-through-jit-getfield-FIXED-20261029.md`.
//! An emitted referent read needs a whole lowering with a wired SATB gate, so
//! the shape is pinned on the source, as `jr21_tl3_referent_gate_tests` does.

/// `CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW` is default ON, `=0` kills,
/// and an unwired table names no row.
#[test]
fn j27_referent_splice_row_switch_defaults_on() {
    let key = "CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW";
    let table = crate::DirectHelperTable::default();
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert!(crate::ir::ir_referent_splice_compact_row_enabled());
        assert_eq!(crate::ir::spliced_referent_row_from_table(&table), 0);
    });
    cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
        assert!(!crate::ir::ir_referent_splice_compact_row_enabled());
        assert_eq!(crate::ir::spliced_referent_row_from_table(&table), 0);
    });
}

/// The proven-receiver form (no SATB pre-gate test after its read) is never
/// taken by a handed-out referent read.
#[test]
fn j27_a_referent_read_never_takes_the_proven_receiver_form() {
    let src = include_str!("ir_lower.rs").replace("\r\n", "\n");
    let start = src
        .find("    fn emit_inline_compact_getfield(")
        .expect("the inline getfield arm");
    let body = &src[start..];
    let body = &body[..body.find("\n    \x7d\n").expect("its end")];
    let gate = body
        .find("if self.proven_fields_on && !referent_gate")
        .expect("the proven form excludes referent reads");
    let proven = body
        .find("self.emit_proven_receiver_getfield(")
        .expect("the proven form");
    assert!(gate < proven);
}

/// The builder gives a spliced referent read (`referent_load_pcs`, slot 0,
/// a reference) the spliced row, and the compile door sets it.
#[test]
fn j27_the_spliced_referent_read_carries_the_row() {
    let ir = include_str!("ir.rs").replace("\r\n", "\n");
    assert!(ir.contains("self.note_spliced_referent_row(load);"));
    let lib = include_str!("lib.rs").replace("\r\n", "\n");
    assert!(lib.contains(
        "builder.set_spliced_referent_row(ir::spliced_referent_row_from_table(direct_helpers));"
    ));
}
