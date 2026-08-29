// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! x86-64 register encoding tables.
//!
//! Moved verbatim out of `x64.rs`'s `Register encoding for x86-64`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

#[allow(dead_code)]
pub(super) const RAX: u8 = 0;
#[allow(dead_code)]
pub(super) const RCX: u8 = 1;
#[allow(dead_code)]
pub(super) const RDX: u8 = 2;
#[allow(dead_code)]
pub(super) const RBX: u8 = 3;
#[allow(dead_code)]
pub(super) const RSP: u8 = 4;
#[allow(dead_code)]
pub(super) const RBP: u8 = 5;
#[allow(dead_code)]
pub(super) const RSI: u8 = 6;
#[allow(dead_code)]
pub(super) const RDI: u8 = 7;
#[allow(dead_code)]
pub(super) const R8: u8 = 8;
#[allow(dead_code)]
pub(super) const R9: u8 = 9;
#[allow(dead_code)]
pub(super) const R10: u8 = 10;
#[allow(dead_code)]
pub(super) const R11: u8 = 11;
#[allow(dead_code)]
pub(super) const R12: u8 = 12;
#[allow(dead_code)]
pub(super) const R13: u8 = 13;
#[allow(dead_code)]
pub(super) const R14: u8 = 14;
#[allow(dead_code)]
pub(super) const R15: u8 = 15;

/// 64-bit general-purpose register mnemonics, indexed by encoding number.
///
/// Only the instruction-selector's round-trip disassembly uses these: an
/// equivalence failure that prints `mov r13, [rsp + 8]` is actionable where a
/// bare `[4C, 8B, 6C, 24, 08]` is not. Kept beside the encoding numbers so the
/// table and the numbers cannot drift apart.
#[allow(dead_code)]
pub(super) const GPR64_NAMES: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

/// XMM register mnemonics, indexed by encoding number.
#[allow(dead_code)]
pub(super) const XMM_NAMES: [&str; 16] = [
    "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8", "xmm9", "xmm10",
    "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
];

/// Build the REX prefix byte `0100WRXB`.
///
/// * `w` — 64-bit operand size.
/// * `r` — fourth bit of the ModRM `reg` field.
/// * `x` — fourth bit of the SIB `index` field.
/// * `b` — fourth bit of the ModRM `r/m` / SIB `base` / opcode-embedded
///   register field.
///
/// `0x40` (all four clear) is a legal but pointless prefix; callers that emit
/// REX only on demand compare against it. The hand-written emitters in
/// `x64.rs` spell the same arithmetic inline (`0x48 | 0x04 | 0x01`); this is
/// the named form the pattern table encodes with.
#[allow(dead_code)]
pub(super) const fn rex(w: bool, r: bool, x: bool, b: bool) -> u8 {
    0x40 | ((w as u8) << 3) | ((r as u8) << 2) | ((x as u8) << 1) | (b as u8)
}

/// Does this register number need its fourth bit carried in a REX prefix?
#[allow(dead_code)]
pub(super) const fn is_extended(reg: u8) -> bool {
    reg >= 8
}

// Argument registers per platform
#[cfg(target_os = "windows")]
pub(super) const ARG_REGS: [u8; 4] = [RCX, RDX, R8, R9];

#[cfg(not(target_os = "windows"))]
pub(super) const ARG_REGS: [u8; 6] = [RDI, RSI, RDX, RCX, R8, R9];

/// Native-stack probe stride used by the x64 JIT prologue.
///
/// Windows grows a thread stack one guard page at a time, and Unix kernels use
/// the same page granularity for stack expansion / guard detection. A JIT frame
/// that subtracts more than one page from RSP without probing can skip over the
/// guard page; deep compiled recursion then faults later in arbitrary helper or
/// shadow-stack code. Probe one page at a time before the subtract, then keep a
/// one-page headroom probe below the final RSP.
pub(super) const STACK_BANG_PAGE_SIZE: i32 = 4096;

/// Keep code size bounded for malformed or extreme bytecode. A method needing a
/// frame larger than this falls back to the interpreter instead of emitting a
/// giant inline probe sequence.
pub(super) const MAX_STACK_BANG_PROBES: usize = 512;

pub(super) fn jit_stack_bang_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_STACK_BANG").is_some() {
            return false;
        }
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_STACK_BANG") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        }
    })
}

/// RSP-relative displacements to probe before `sub rsp, frame_size`.
///
/// Returned values are negative displacements from the pre-subtract RSP. They
/// cover every page crossed by the frame allocation and include the exact final
/// frame bottom when it is not page-aligned.
pub(super) fn stack_bang_frame_probe_disps(frame_size: i32) -> Option<Vec<i32>> {
    if frame_size <= 0 {
        return Some(Vec::new());
    }
    let mut disps = Vec::new();
    let mut off = STACK_BANG_PAGE_SIZE;
    while off <= frame_size {
        if disps.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        disps.push(-off);
        off = off.saturating_add(STACK_BANG_PAGE_SIZE);
        if off <= 0 {
            return None;
        }
    }
    if frame_size % STACK_BANG_PAGE_SIZE != 0 {
        if disps.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        disps.push(-frame_size);
    }
    Some(disps)
}
