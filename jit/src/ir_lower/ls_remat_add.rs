// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane REMAT (page
//! `docs/feature-designs/jit-ir-optimizations.md`,
//! item 2 "Rematerialise `x + c`"): a split value's reload, when the value is
//! `x + c` and `x` is in a register at that reload, is recomputed instead of
//! read back from the home word.
//!
//! # Shape
//!
//! The linear scan plans a `SpillKind::Load` for every reload of a split
//! `x + c` (it is not a [`crate::regalloc::remat_of`] value). Under
//! `CRATONVM_JIT_IR_LS_REMAT_ADD=1` the consumer plans
//! [`LsTransition::RematAdd`] for it instead ([`ls_reload_transition`]), and at
//! emission ([`Lowerer::ls_emit_remat_add`]):
//!
//! ```text
//!   int:  LEA r32, [base + disp32]          ; 32-bit sum, zero-extended
//!   long: LEA r64, [base + disp32]          ; addend fits a sign-extended disp32
//!   long: MOV r64, imm64 ; ADD r64, base    ; wider addend, flags dead only
//! ```
//!
//! otherwise the planned home load, unchanged.
//!
//! # What does not move
//!
//! * **The home word.** It is still written at the definition (write-through,
//!   and a managed value's home is never dropped), so a deopt, the OSR exit and
//!   the edge-resolution copies read exactly what they read before. The remat
//!   replaces one LOAD; it skips no store.
//! * **Deopt naming wins.** A value a reachable deopt names keeps its home load
//!   (`deopt_named_reachable`), so the increment that will skip the home store
//!   inherits a population no frame state reads.
//! * **GC maps.** Only `int` / `long` (`remat_add_of`); a `Ref` keeps its load.
//! * **The plan.** Admission (`split_register_benefit` prices a remat like a
//!   load) and sequencing (a remat's source is `Slot(node)`, like a load's) are
//!   the same, so the switch changes bytes at a reload and nothing else. The
//!   base register is read at the moment the remat is emitted and only through
//!   `resident_gpr`, which every earlier transition at the same position keeps
//!   true; when it answers `None`, or the base is carried, or it names the
//!   destination, the home load runs.
//!
//! # Flags
//!
//! `LEA` leaves the flags alone; `ADD` does not. A transition sits at a node
//! position, before the node's first byte, and no lowering leaves flags live
//! across a node boundary: a fused compare emits nothing at its own position
//! and is emitted with its `Jcc` by the terminator (the same invariant the
//! owed volatile fence relies on in `lower_block`). [`FLAGS_LIVE_AT_NODE_POSITION`]
//! names that fact; [`remat_add_form`] refuses the `ADD` form whenever flags
//! are live.

use super::{lea_reg_base_disp_bytes, Lowerer, LsTransition};
use crate::ir::{Graph, NodeId};
use crate::regalloc::{remat_add_of, RematAdd};

/// `CRATONVM_JIT_IR_LS_REMAT_ADD=1` -- **default OFF**, inert unless
/// `CRATONVM_JIT_IR_LS_SPLITS` produces split values. Opt-in because it was
/// written without a build: the decision row is the parent page's table.
/// Read uncached, like `ir_ls_splits_enabled` (once per split value).
pub(super) fn ir_ls_remat_add_enabled() -> bool {
    false}

/// Are the flags live at a split transition? Never: see the module comment.
pub(super) const FLAGS_LIVE_AT_NODE_POSITION: bool = false;

/// How a remat is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RematAddForm {
    /// `LEA dst, [base + disp]`, 32-bit for `int`, 64-bit for `long`.
    Lea(i32),
    /// `MOV dst, imm64 ; ADD dst, base` (64-bit): a `long` addend no `disp32`
    /// holds. Clobbers the flags.
    MovAdd(i64),
}

/// The encoding for `addend` at `wide`, or `None` when there is none that is
/// safe here. An `int` addend always fits (`remat_add_of` keeps it in `i32`).
pub(super) fn remat_add_form(addend: i64, wide: bool, flags_live: bool) -> Option<RematAddForm> {
    match i32::try_from(addend) {
        Ok(disp) => Some(RematAddForm::Lea(disp)),
        Err(_) if wide && !flags_live => Some(RematAddForm::MovAdd(addend)),
        Err(_) => None,
    }
}

/// The bytes of `form` into `dst` from `base`, and how many.
///
/// `MovAdd` is always 64-bit (only a `long` takes it) and needs
/// `dst != base`, which [`Lowerer::ls_emit_remat_add`] guarantees for both
/// forms (a base in the destination is a base the plan is overwriting).
pub(super) fn remat_add_bytes(
    dst: u8,
    base: u8,
    form: RematAddForm,
    wide: bool,
) -> ([u8; 13], usize) {
    let mut out = [0u8; 13];
    match form {
        RematAddForm::Lea(disp) => {
            let (lea, n) = lea_reg_base_disp_bytes(dst, base, disp, wide);
            out[..n].copy_from_slice(&lea[..n]);
            (out, n)
        }
        RematAddForm::MovAdd(imm) => {
            out[0] = 0x48 | u8::from(dst >= 8); // REX.W [+B]
            out[1] = 0xB8 + (dst & 7); // MOV r64, imm64
            out[2..10].copy_from_slice(&imm.to_le_bytes());
            out[10] = 0x48 | (u8::from(base >= 8) << 2) | u8::from(dst >= 8);
            out[11] = 0x01; // ADD r/m64 (dst), r64 (base)
            out[12] = 0xC0 | ((base & 7) << 3) | (dst & 7);
            (out, 13)
        }
    }
}

/// The transition a planned reload of `node` into `reg` becomes: a
/// [`LsTransition::RematAdd`] when the switch is on, the bank is GP and
/// `node` is `x + c`; the home load otherwise.
pub(super) fn ls_reload_transition(graph: &Graph, node: NodeId, reg: u8, gp: bool) -> LsTransition {
    if gp && ir_ls_remat_add_enabled() {
        if let Some(remat) = remat_add_of(graph, node) {
            return LsTransition::RematAdd { node, reg, remat };
        }
    }
    LsTransition::Load { node, reg, gp }
}

impl Lowerer<'_> {
    /// Recompute `node` into `reg` from its resident base; answers whether it
    /// emitted. `false` emits nothing, and the caller then runs the home load.
    pub(super) fn ls_emit_remat_add(&mut self, node: NodeId, reg: u8, remat: RematAdd) -> bool {
        let Some(base_reg) = self.ls_remat_add_base(node, reg, remat) else {
            return false;
        };
        let Some(form) = remat_add_form(remat.addend, remat.wide, FLAGS_LIVE_AT_NODE_POSITION)
        else {
            return false;
        };
        let (bytes, len) = remat_add_bytes(reg, base_reg, form, remat.wide);
        self.buf.emit(&bytes[..len]);
        // The stress mode poisons an `int` definition's high half; the value
        // a home load would have read carries that poison, so this one does.
        if self.poisons_int_def(node) {
            self.emit_poison_int_high(reg);
        }
        self.ls_publish(node, reg, true);
        true
    }

    /// The register `remat.base` can be read from at this reload, or `None`:
    /// a deopt-named `node` (deopt naming wins), a carried base (only its
    /// consumer may read a carry), a base in no register, or a base in the
    /// destination itself.
    fn ls_remat_add_base(&self, node: NodeId, reg: u8, remat: RematAdd) -> Option<u8> {
        if self
            .deopt_named_reachable
            .get(node as usize)
            .copied()
            .unwrap_or(true)
        {
            return None;
        }
        if self.carry_names(remat.base) {
            return None;
        }
        self.resident_gpr(remat.base).filter(|&r| r != reg)
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
