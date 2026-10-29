// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane REMAT (page
//! `docs/known-issues/jit/j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md`,
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LS_REMAT_ADD")
}

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
mod tests {
    use super::*;
    use crate::ExecutableBuffer;

    const RAX: u8 = 0;
    const RBX: u8 = 3;
    const R12: u8 = 12;
    const R13: u8 = 13;
    const R14: u8 = 14;

    fn bytes(dst: u8, base: u8, form: RematAddForm, wide: bool) -> Vec<u8> {
        let (b, n) = remat_add_bytes(dst, base, form, wide);
        b[..n].to_vec()
    }

    // ── Encoding ────────────────────────────────────────────────────────

    #[test]
    fn j27_remat_add_encodes_the_int_lea_as_32_bit() {
        // LEA EBX, [R12 + 1]: REX.B only (no W), SIB for the R12 base.
        assert_eq!(bytes(RBX, R12, RematAddForm::Lea(1), false), [0x41, 0x8D, 0x5C, 0x24, 0x01]);
        // LEA EBX, [R13 + 0]: an R13 base always carries a displacement.
        assert_eq!(bytes(RBX, R13, RematAddForm::Lea(0), false), [0x41, 0x8D, 0x5D, 0x00]);
    }

    #[test]
    fn j27_remat_add_encodes_the_long_lea_as_64_bit() {
        // LEA R14, [RBX - 200]: REX.W + REX.R, disp32.
        assert_eq!(
            bytes(R14, RBX, RematAddForm::Lea(-200), true),
            [0x4C, 0x8D, 0xB3, 0x38, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn j27_remat_add_encodes_mov_add_for_a_wide_addend() {
        // MOV RBX, 0x1_0000_0000 ; ADD RBX, R12
        assert_eq!(
            bytes(RBX, R12, RematAddForm::MovAdd(1 << 32), true),
            [0x48, 0xBB, 0, 0, 0, 0, 1, 0, 0, 0, 0x4C, 0x01, 0xE3]
        );
    }

    // ── Form choice ─────────────────────────────────────────────────────

    #[test]
    fn j27_remat_add_takes_lea_whenever_the_addend_fits_a_disp32() {
        assert_eq!(remat_add_form(7, false, true), Some(RematAddForm::Lea(7)));
        assert_eq!(
            remat_add_form(i64::from(i32::MIN), false, false),
            Some(RematAddForm::Lea(i32::MIN))
        );
        assert_eq!(remat_add_form(-1, true, true), Some(RematAddForm::Lea(-1)));
    }

    #[test]
    fn j27_remat_add_refuses_add_while_flags_are_live() {
        assert_eq!(remat_add_form(1 << 32, true, false), Some(RematAddForm::MovAdd(1 << 32)));
        assert_eq!(remat_add_form(1 << 32, true, true), None, "ADD would clobber live flags");
        assert_eq!(
            remat_add_form(i64::from(i32::MAX) + 1, false, false),
            None,
            "an int never takes the 64-bit form"
        );
    }

    // ── Executed ────────────────────────────────────────────────────────

    fn mov64(dst: u8, src: u8) -> [u8; 3] {
        let rex = 0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8);
        [rex, 0x89, 0xC0 | ((src & 7) << 3) | (dst & 7)]
    }

    fn run(body: &[u8], arg: u64) -> u64 {
        let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
        buf.emit(body);
        buf.emit(&[0xC3]); // RET
        assert!(!buf.overflowed());
        buf.finalize();
        // SAFETY: `buf` is finalized and holds a complete
        // `extern "C" fn(u64) -> u64` that restores every callee-saved
        // register it writes (RBX, R12) and calls nothing.
        let f =
            unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
        f(arg)
    }

    /// `PUSH RBX; PUSH R12; MOV R12, arg0; MOV RBX, -1; <remat RBX from R12>;
    /// MOV RAX, RBX; POP R12; POP RBX` -- RBX starts all ones so a 32-bit form
    /// that failed to zero-extend shows.
    fn remat_of(arg: u64, form: RematAddForm, wide: bool) -> u64 {
        let mut body = vec![0x53, 0x41, 0x54];
        body.extend(mov64(R12, super::super::CALL_ARG_REGS[0]));
        body.extend([0x48, 0xC7, 0xC3, 0xFF, 0xFF, 0xFF, 0xFF]); // MOV RBX, -1
        body.extend(bytes(RBX, R12, form, wide));
        body.extend(mov64(RAX, RBX));
        body.extend([0x41, 0x5C, 0x5B]);
        run(&body, arg)
    }

    #[test]
    fn j27_remat_add_int_wraps_at_max_and_zero_extends() {
        let max = i32::MAX as u32 as u64;
        assert_eq!(remat_of(max, RematAddForm::Lea(1), false), 0x8000_0000);
        // The base's high half is not the int's: a 32-bit LEA ignores it.
        let dirty = 0xA5A5_A5A5_0000_0000 | max;
        assert_eq!(remat_of(dirty, RematAddForm::Lea(1), false), 0x8000_0000);
        // `x - MIN` as `x + MIN`, modulo 2^32.
        assert_eq!(remat_of(5, RematAddForm::Lea(i32::MIN), false), 0x8000_0005);
    }

    #[test]
    fn j27_remat_add_long_wraps_at_max() {
        assert_eq!(
            remat_of(i64::MAX as u64, RematAddForm::Lea(1), true) as i64,
            i64::MIN
        );
        assert_eq!(remat_of(5, RematAddForm::Lea(-6), true) as i64, -1);
        assert_eq!(
            remat_of(5, RematAddForm::MovAdd(1 << 40), true) as i64,
            (1 << 40) + 5
        );
        assert_eq!(
            remat_of(i64::MAX as u64, RematAddForm::MovAdd(i64::MIN), true) as i64,
            -1
        );
    }

    /// `XOR EAX,EAX; PUSH RBX; PUSH R12; MOV R12, arg0; TEST R12, R12;
    /// <remat>; SETZ AL; POP R12; POP RBX`: ZF from the TEST survives a LEA
    /// and not an ADD -- the reason the ADD form needs the flags dead.
    fn zf_after(arg: u64, form: RematAddForm) -> u64 {
        let mut body = vec![0x31, 0xC0, 0x53, 0x41, 0x54];
        body.extend(mov64(R12, super::super::CALL_ARG_REGS[0]));
        body.extend([0x4D, 0x85, 0xE4]); // TEST R12, R12
        body.extend(bytes(RBX, R12, form, true));
        body.extend([0x0F, 0x94, 0xC0]); // SETZ AL
        body.extend([0x41, 0x5C, 0x5B]);
        run(&body, arg)
    }

    #[test]
    fn j27_remat_add_lea_keeps_the_flags_and_add_does_not() {
        assert_eq!(zf_after(0, RematAddForm::Lea(9)), 1);
        assert_eq!(zf_after(0, RematAddForm::MovAdd(1 << 33)), 0);
    }
}
