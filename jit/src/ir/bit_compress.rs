// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT j27 wave 6, lane T1BCE: `Integer.compress(II)I`, `Long.compress(JJ)J`,
//! `Integer.expand(II)I` and `Long.expand(JJ)J` as [`ScalarOp`] rows lowered
//! to one BMI2 instruction each, as HotSpot 25's C2 does (`_compress_i` /
//! `_compress_l` / `_expand_i` / `_expand_l`, `PEXT` / `PDEP` when the CPU
//! has BMI2). Without the switch, or on a host without BMI2, nothing here
//! answers and the site stays the JDK body (a loop of shifts and masks).
//!
//! # Why it is exact
//!
//! The JDK specifies `compress(i, mask)` as the bits of `i` at the set
//! positions of `mask`, packed into the low end in order, the rest zero; and
//! `expand(i, mask)` as the low bits of `i`, in order, deposited at the set
//! positions of `mask`, the rest zero. Those are, word for word, Intel's
//! definitions of `PEXT` (parallel extract) and `PDEP` (parallel deposit)
//! with the value in the VEX `vvvv` operand and the mask in `r/m` (SDM
//! vol. 2B, `PEXT` / `PDEP`). The 32-bit forms zero the upper half of the
//! destination, which is this tier's `int` register convention.
//!
//! # Encoding
//!
//! `PEXT r, r, r/m` is `VEX.LZ.F3.0F38.W0/W1 F5 /r`, `PDEP` the same with
//! `F2`. The lowering always emits `op rAX, rAX, rCX`: value in RAX (the
//! scalar arm's operand 0 load), mask in RCX. Three-byte VEX (`C4`):
//!
//! * byte 1 `R̄ X̄ B̄ m-mmmm` = `1 1 1 00010` = `E2` (no extended register,
//!   map `0F38`);
//! * byte 2 `W v̄v̄v̄v̄ L pp` = `W 1111 0 pp`: `vvvv` = RAX (0) inverted,
//!   `L = 0` (`LZ`), `pp` = `10` for `F3` (PEXT) / `11` for `F2` (PDEP);
//! * opcode `F5`; ModRM `11 000 001` = `C1` (reg = RAX, r/m = RCX).
//!
//! # Switch
//!
//! `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND`, default ON since 2026-10-28 (j27
//! WARNING flip; =0 restores; was opt-in: unmeasured, and `PEXT` / `PDEP`
//! are microcoded and slow on AMD before Zen 3, so a host A/B decides), under `CRATONVM_JIT_IR_SCALAR_INTRINSICS`. Read per call,
//! no `static`.

use super::ScalarOp;

/// `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND` (j27 wave 6, lane T1BCE; default
/// ON since 2026-10-28 (j27 WARNING flip; =0 restores)): the four rows of
/// this module.
pub(crate) fn ir_bit_compress_expand_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The family `class.method descriptor` names, when the switch is on and
/// the host has BMI2. `try_ir_scalar_intrinsic` asks it (after its own
/// `CRATONVM_JIT_IR_SCALAR_INTRINSICS` gate).
pub(super) fn bit_compress_row(class: &str, method: &str, descriptor: &str) -> Option<ScalarOp> {
    let sop = match (class, method, descriptor) {
        ("java/lang/Integer", "compress", "(II)I") => ScalarOp::CompressI,
        ("java/lang/Long", "compress", "(JJ)J") => ScalarOp::CompressL,
        ("java/lang/Integer", "expand", "(II)I") => ScalarOp::ExpandI,
        ("java/lang/Long", "expand", "(JJ)J") => ScalarOp::ExpandL,
        _ => return None,
    };
    (ir_bit_compress_expand_enabled() && crate::x64::has_bmi2()).then_some(sop)
}

/// The five bytes of `PEXT` / `PDEP rAX, rAX, rCX` for `sop` (module notes);
/// `None` for every other family.
pub(crate) fn pext_pdep_rax_rax_rcx(sop: ScalarOp) -> Option<[u8; 5]> {
    let (pp, w): (u8, u8) = match sop {
        ScalarOp::CompressI => (0b10, 0),
        ScalarOp::CompressL => (0b10, 1),
        ScalarOp::ExpandI => (0b11, 0),
        ScalarOp::ExpandL => (0b11, 1),
        _ => return None,
    };
    // W | inverted vvvv (RAX = 0000 -> 1111) | L = 0 | pp.
    let byte2 = (w << 7) | (0b1111 << 3) | pp;
    Some([0xC4, 0xE2, byte2, 0xF5, 0xC1])
}


