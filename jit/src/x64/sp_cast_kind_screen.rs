// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 7 (lane T1DISP21, DISPATCH21-4's single-pass twin;
//! `CRATONVM_JIT_SP_IC_CAST_KIND_SCREEN_ELIDE`, default ON since 2026-10-28
//! (j27 WARNING flip; =0 restores)): the single-pass
//! inline-cache cascade skips its receiver kind screen at a call whose
//! receiver is the result of the `checkcast` right before it, when that cast's
//! target is a type no array is assignable to.
//!
//! The optimizing tier's rule (`ir_lower::ic_receiver_cast_excludes_arrays`)
//! reads the receiver's producer from the graph. This backend has no graph,
//! and a backward look at `code[pc - 3]` alone can read an operand byte as a
//! `checkcast` (why the t1 wave 8 lane did not build it). The proof here is
//! three facts, each checked:
//!
//! 1. the call takes the receiver only (`num_jit_args == 1`), so the receiver
//!    is the top of the stack the previous instruction left;
//! 2. the call's pc is no branch target (the walk's map, which also marks the
//!    entry, every exception handler start and so every OSR entry, a backward
//!    branch's target), so the previous instruction is its only predecessor;
//! 3. that previous instruction is a `checkcast` that starts at `pc - 3` on
//!    the linear walk from pc 0 (`bytecode_analysis::step`), not an operand.
//!
//! Then the receiver is the object the cast let through: `null` (which the
//! cascade's own null test takes first) or an instance of the cast's target.
//! JVMS 4.10.1.2: an array is assignable only to `Object`, `Cloneable`,
//! `Serializable` and array types (`inline_cache_pic::
//! ic_static_receiver_admits_arrays`, the rule both tiers already share), so
//! for any other target no array reaches the site and the screen is a compare
//! that never fails -- the same answer `ic_receiver_may_be_array` already
//! gives an `invokevirtual` on a non-`Object` class. The megamorphic gate's
//! entry contract ("kind-screened where an array can reach the site") is kept
//! for the same reason.

use super::*;

/// The byte length of `checkcast` (opcode + u2 constant-pool index).
const CHECKCAST_LEN: usize = 3;

/// `CRATONVM_JIT_SP_IC_CAST_KIND_SCREEN_ELIDE` (j27 wave 7, lane T1DISP21,
/// DISPATCH21-4 single-pass twin; perf, default ON since 2026-10-28 (j27
/// WARNING flip; =0 restores)): see the module comment. Like its
/// optimizing-tier twin `CRATONVM_JIT_IR_IC_CAST_KIND_SCREEN_ELIDE`, default
/// ON. Read per site compile.
pub(super) fn sp_ic_cast_kind_screen_elide_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// Whether `target` is the first byte of an instruction on the linear walk
/// from pc 0 of `code[..code_len]`.
fn is_instruction_start(code: &[u8], code_len: usize, target: usize) -> bool {
    let end = code_len.min(code.len());
    let mut p = 0usize;
    while p < target && p < end {
        p += crate::bytecode_analysis::step(code, p);
    }
    p == target && target < end
}

/// The pc of the `checkcast` whose result is the receiver of the instance
/// call at `pc` (facts 1-3 of the module comment), or `None`.
pub(super) fn sp_receiver_cast_pc(
    code: &[u8],
    code_len: usize,
    pc: usize,
    num_jit_args: usize,
    branch_targets: &[bool],
) -> Option<usize> {
    if num_jit_args != 1 || pc < CHECKCAST_LEN || pc >= code_len.min(code.len()) {
        return None;
    }
    if branch_targets.get(pc).copied().unwrap_or(true) {
        return None;
    }
    let cast_pc = pc - CHECKCAST_LEN;
    if code[cast_pc] != 0xc0 || !is_instruction_start(code, code_len, cast_pc) {
        return None;
    }
    Some(cast_pc)
}

/// Whether a `checkcast` to `target` (internal name; `None` = unresolved)
/// proves its non-null result is no array.
pub(super) fn cast_target_excludes_arrays(target: Option<&str>) -> bool {
    target.is_some_and(|n| !crate::inline_cache_pic::ic_static_receiver_admits_arrays(n))
}

impl Compiler {
    /// DISPATCH21-4 single-pass twin: has the `checkcast` right before the
    /// instance call at `pc` proved its receiver a plain object (or null)?
    /// `false` with the switch off. `CRATONVM_DBG_JITC` names each elision.
    pub(super) fn sp_ic_receiver_cast_proves_plain(
        &self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        num_jit_args: usize,
        branch_targets: &[bool],
    ) -> bool {
        if !sp_ic_cast_kind_screen_elide_enabled() {
            return false;
        }
        let Some(cast_pc) = sp_receiver_cast_pc(code, code_len, pc, num_jit_args, branch_targets)
        else {
            return false;
        };
        let target = self.typecheck_info_idx.get(&cast_pc).and_then(|&i| {
            let (_, ptr, len) = self.typecheck_info[i];
            // SAFETY: a `typecheck_info` pair names a constant-pool string the
            // compile request keeps alive for the compile (as `op_object`'s
            // `checkcast` arm reads it), or is null / empty, which answers
            // `None`.
            unsafe { crate::typecheck_site_name(ptr, len) }
        });
        let proved = cast_target_excludes_arrays(target);
        if proved && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] sp ic kind screen elided by the checkcast before it: {} pc={pc} cast_pc={cast_pc} target={}",
                self.method_label,
                target.unwrap_or("")
            );
        }
        proved
    }
}

#[cfg(test)]
mod j27_t1disp21_sp_cast_kind_screen_tests;
