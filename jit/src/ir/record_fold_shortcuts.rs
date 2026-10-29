// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5, lane RECORDS (RECORDS21-3 of
//! `docs/internal/proposals/jit/tier2/jit-r21-records-proposals.md`, page
//! `docs/known-issues/jit/jr21-records-record-objectmethods-fold-20261015.md`):
//! two shortcuts in front of the record fold's per-reference-component
//! `Objects` calls (`IrBuilder::try_indy_record_fold`).
//!
//! ```text
//! hashCode, a `Ljava/lang/String;` component s:
//!   s == null ? 0 : (s.hash != 0 ? s.hash : Objects.hashCode(s))
//! equals, any reference component pair (a, b):
//!   a == b ? true : Objects.equals(a, b)
//! ```
//!
//! # Why they are exact
//!
//! * `Objects.hashCode(o)` is `o != null ? o.hashCode() : 0`, and
//!   `String.hashCode()` returns its `hash` field whenever that is non-zero
//!   (the JDK body's first test); a zero `hash` (not yet computed, or a string
//!   whose hash is 0) takes the call, which computes and caches it. The field
//!   is typed `String` and `java.lang.String` is a final boot class (no other
//!   loader may define `java.*`), so a non-null value IS a `String`. Neither
//!   arm can throw, so the `Objects.hashCode` frame a skipped call would have
//!   shown is never visible.
//! * `Objects.equals(a, b)` is `(a == b) || (a != null && a.equals(b))`: the
//!   same object answers `true` before any `equals` runs, so the shortcut
//!   skips only a call that would have run no Java code.
//!
//! The `String.hash` slot is the one `DirectHelperTable::string_hash_field`
//! answers for the optimizing tier's `Object.hashCode()` String prefix (the
//! boot `String` with the `byte[]`/`coder` shape), filled in by `lib.rs`
//! ([`record_fold_admit_string_hash`]); the VM marks which components are
//! `String`-typed (`CpResolvers::indy_record_fold`). The cached read is an
//! ordinary field-index load at the fold's synthetic pc, as the components'
//! own reads are.
//!
//! Switches (perf, **default ON**, `=0` restores the plain call):
//! `CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH` (read per compile, by
//! [`record_fold_admit_string_hash`]) and
//! `CRATONVM_JIT_IR_INDY_RECORD_FOLD_REF_IDENTITY` (read per fold build).

use super::{CmpOp, IndyRecordFold, IrBuilder, IrType, NodeId, Op};
use std::collections::HashMap;

/// `CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH` (j27 wave 5, lane RECORDS;
/// perf, **default ON**, `=0` off): may a record fold read a `String`
/// component's cached `hash` before calling `Objects.hashCode`?
pub(crate) fn ir_indy_record_fold_string_hash_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH")
}

/// `CRATONVM_JIT_IR_INDY_RECORD_FOLD_REF_IDENTITY` (j27 wave 5, lane RECORDS;
/// perf, **default ON**, `=0` off): may a record fold's `equals` answer a
/// reference component pair that is the same object without calling
/// `Objects.equals`?
pub(crate) fn ir_indy_record_fold_ref_identity_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_INDY_RECORD_FOLD_REF_IDENTITY")
}

/// Give every fold of `folds` that has a `String`-typed component the
/// `String.hash` slot of `string_hash_field` (`(String class id, slot)`, the
/// `DirectHelperTable::string_hash_field` answer), or none: when the answer
/// is missing or the switch is off, every fold keeps the plain call.
pub(crate) fn record_fold_admit_string_hash(
    folds: &mut HashMap<usize, IndyRecordFold>,
    string_hash_field: Option<(u32, usize)>,
) {
    let slot = string_hash_field
        .filter(|_| ir_indy_record_fold_string_hash_enabled())
        .and_then(|(_, slot)| u32::try_from(slot).ok());
    for fold in folds.values_mut() {
        let has_string = fold.string_components.contains(&true);
        fold.string_hash_slot = slot.filter(|_| has_string);
    }
}

impl IrBuilder {
    /// The hash of reference component `position` of `fold`, value `v`: the
    /// plain `Objects.hashCode(v)` call, or, for a `String` component with an
    /// admitted slot, the cached-hash shortcut.
    pub(super) fn record_fold_ref_hash(
        &mut self,
        pc: usize,
        fold: &IndyRecordFold,
        position: usize,
        info_ptr: usize,
        v: NodeId,
    ) -> NodeId {
        let is_string = fold.string_components.get(position) == Some(&true);
        match fold.string_hash_slot.filter(|_| is_string) {
            Some(slot) => self.record_fold_string_hash(pc, info_ptr, v, slot),
            None => self.record_fold_call(pc, info_ptr, &[v]),
        }
    }

    /// `s == null ? 0 : (s.hash != 0 ? s.hash : Objects.hashCode(s))`.
    fn record_fold_string_hash(
        &mut self,
        pc: usize,
        info_ptr: usize,
        s: NodeId,
        slot: u32,
    ) -> NodeId {
        let mem0 = self.mem;
        let zero = self.iconst(0);
        let null = self.aconst_null();
        let is_null = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![s, null], pc);
        let ctrl = self.ctrl;
        let (null_edge, live) = self.record_fold_branch(pc, ctrl, is_null);
        self.ctrl = live;
        let cached = self.record_fold_load(pc, s, slot, b'I');
        let mem_cached = self.mem;
        let have = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cached, zero], pc);
        let (hit, miss) = self.record_fold_branch(pc, live, have);
        self.ctrl = miss;
        let call = self.record_fold_call(pc, info_ptr, &[s]);
        self.record_fold_join(
            pc,
            &[
                (null_edge, mem0, zero),
                (hit, mem_cached, cached),
                (miss, call, call),
            ],
        )
    }

    /// Whether reference components `a` and `b` are equal: the plain
    /// `Objects.equals(a, b)` call, or `a == b ? true : Objects.equals(a, b)`.
    pub(super) fn record_fold_ref_equal(
        &mut self,
        pc: usize,
        info_ptr: usize,
        a: NodeId,
        b: NodeId,
    ) -> NodeId {
        if !ir_indy_record_fold_ref_identity_enabled() {
            return self.record_fold_call(pc, info_ptr, &[a, b]);
        }
        let mem0 = self.mem;
        let one = self.iconst(1);
        let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![a, b], pc);
        let ctrl = self.ctrl;
        let (is_same, distinct) = self.record_fold_branch(pc, ctrl, same);
        self.ctrl = distinct;
        let call = self.record_fold_call(pc, info_ptr, &[a, b]);
        self.record_fold_join(pc, &[(is_same, mem0, one), (distinct, call, call)])
    }

    /// Join `arms` (`(control, memory, Int value)` each) with one `Merge` and
    /// a memory and a value `Phi`; control and memory continue from the join,
    /// and the value `Phi` is returned.
    pub(super) fn record_fold_join(
        &mut self,
        pc: usize,
        arms: &[(NodeId, NodeId, NodeId)],
    ) -> NodeId {
        let join = self.graph.add(
            Op::Merge,
            IrType::Control,
            arms.iter().map(|a| a.0).collect(),
            Some(pc),
        );
        let mut mem_in = vec![join];
        mem_in.extend(arms.iter().map(|a| a.1));
        let mut value_in = vec![join];
        value_in.extend(arms.iter().map(|a| a.2));
        self.ctrl = join;
        self.mem = self.graph.add(Op::Phi, IrType::Memory, mem_in, Some(pc));
        self.graph.add(Op::Phi, IrType::Int, value_in, Some(pc))
    }
}
