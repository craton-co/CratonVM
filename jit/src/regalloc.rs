// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Graph-coloring register allocator for JVM bytecode locals.
//!
//! Performs backward dataflow liveness analysis on the bytecode CFG,
//! builds an interference graph, and uses Chaitin-Briggs graph coloring
//! to assign callee-saved registers to locals optimally.
//!
//! ## 64-local cap
//!
//! Liveness sets, gen/kill sets, and the interference graph are all
//! represented as `u64` bitsets, one bit per local. This imposes a hard
//! ceiling of **64 locals**: only locals with index `< 64` participate in
//! allocation. Methods with more than 64 locals are not rejected — locals
//! at index `>= 64` are simply never assigned a register and fall back to
//! the unallocated (frame-slot / spill) path, which is always correct.
//!
//! This cap is a deliberate simplification: methods with >64 locals are
//! rare and the bitset representation keeps liveness analysis fast for the
//! common case. Raising it would require switching every `u64` bitset (and
//! the `interference: Vec<u64>` graph) to a wider/dynamic bitset type.

use super::x64::{LOCAL_REGS, LOCAL_XMMS};

/// The GPR pool this allocator may colour Java locals into.
///
/// Normally the whole of [`LOCAL_REGS`], which is **7 registers on Windows**
/// (`R12–R15, RBX, RSI, RDI`) and **5 on System V** (no `RSI`/`RDI`). That
/// difference is not cosmetic: it is the stated mechanism behind a family of
/// OSR miscompiles that reproduce only on Linux, because the smaller pool
/// forces the coalescing the OSR dead-mask exists to handle. See
/// `internal/fixed-suite-bugs/jit/arrays-sort-long-osr-miscompile-FIXED.md`.
///
/// `CRATONVM_JIT_LOCAL_REGS=<n>` truncates the pool to its first `n` entries,
/// so a Windows host can be put under System V's register pressure — the one
/// variable a Windows reproduction attempt otherwise cannot control, and the
/// reason "it passed on Windows" has repeatedly had to be written off as a
/// vacuous negative rather than an exoneration.
///
/// Unset (the default) returns the full slice, so every compile is
/// byte-identical to a tree without this knob. A value above the pool size is
/// clamped rather than rejected: the intent is always "at most this many".
pub fn local_gpr_pool() -> &'static [u8] {
    use std::sync::OnceLock;
    static POOL_LEN: OnceLock<usize> = OnceLock::new();
    let n = *POOL_LEN.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_LOCAL_REGS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map(|n| n.min(LOCAL_REGS.len()))
            .unwrap_or(LOCAL_REGS.len())
    });
    &LOCAL_REGS[..n]
}

// Used only by the IR-level linear-scan allocator at the bottom of this file.
use crate::bailout::{Bailout, BailoutReason, CompileResult};
use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};
use crate::ir_schedule::Schedule;
use crate::metrics::CompileRecorder;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

/// Conservative cap on `tableswitch` table size used by [`bc_len`].
///
/// JVM method code is at most 65535 bytes, which by itself caps a real
/// `tableswitch` payload at ~16383 entries. We keep an explicit cap two
/// orders of magnitude above that (consistent with `x64::MAX_TABLESWITCH_ENTRIES`)
/// so that adversarial or truncated bytecode cannot trick `(high - low + 1)`
/// into wrapping when cast to `usize`. On overflow / cap exceeded `bc_len`
/// falls back to length 1, which is always safe: any further validation is
/// the caller's responsibility (`jit_scan` / `compile_bytecode` reject the
/// method outright at the same opcode).
const MAX_TABLESWITCH_ENTRIES: usize = 1 << 24;

/// Conservative cap on `lookupswitch` `npairs` used by [`bc_len`].
///
/// Same rationale as [`MAX_TABLESWITCH_ENTRIES`]. The JVM spec stores
/// `npairs` as a signed `i32`; any negative value is rejected outright
/// rather than reinterpreted as a huge `usize` (which previously caused
/// out-of-bounds reads in liveness scanning).
const MAX_LOOKUPSWITCH_NPAIRS: usize = 1 << 20;

/// ARM64 callee-saved GPR registers for locals: X19-X28 (10 registers).
pub const ARM64_LOCAL_GPRS: [u8; 10] = [19, 20, 21, 22, 23, 24, 25, 26, 27, 28];

/// ARM64 callee-saved FP/SIMD registers for float/double locals: D8-D15 (8 registers).
/// On AAPCS64 only D8-D15 are callee-saved (the lower 64 bits of V8-V15).
pub const ARM64_LOCAL_FPS: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];

/// Result of register allocation.
pub struct RegAllocResult {
    /// Per-local GPR register assignment. `None` means the local spills to the frame
    /// or has an XMM/FP assignment (for float/double locals).
    pub assignments: Vec<Option<u8>>,
    /// Per-local XMM/FP register assignment for float/double locals.
    /// `None` means the local spills to the frame.
    pub xmm_assignments: Vec<Option<u8>>,
    /// Callee-saved GPR registers actually used (for prologue/epilogue save/restore).
    pub used_callee_saved: Vec<u8>,
    /// XMM/FP registers actually used for float/double locals (for save/restore).
    pub used_xmm_regs: Vec<u8>,
    /// Per-basic-block live-in local set, as `(block_start_pc, live_in_bitset)`.
    /// Bit `i` set means local `i` is live on entry to the block at that PC.
    /// OSR entry PCs are loop-header block starts, so this lets the OSR
    /// trampoline skip loading locals that are *dead* at the entry — which is
    /// required for correctness: the graph-colouring allocator coalesces two
    /// non-interfering locals onto one register, and loading the dead one in
    /// index order would clobber the live one that shares the register.
    pub block_live_in: Vec<(usize, u64)>,
}

/// A basic block in the bytecode CFG.
struct BasicBlock {
    start_pc: usize,
    end_pc: usize,          // exclusive
    successors: Vec<usize>, // indices into blocks vec
    gen: u64,               // locals used before defined in this block
    kill: u64,              // locals defined in this block
    live_in: u64,
    live_out: u64,
}

/// Compute the length of a bytecode instruction at `pc`.
///
/// CM-FASTMATH root cause: `ldc` (0x12), `ldc_w` (0x13), and `ldc2_w` (0x14)
/// were absent from this table (the x64.rs `bytecode_len_at` twin had them
/// fixed; this copy was not kept in sync), so they fell through to `_ => 1`.
/// Every liveness/CFG walk then read the constant-pool index operand bytes as
/// opcodes. For classes with a small constant pool the bytes decode as benign
/// 1-byte ops and the walk resyncs; once indices grow past ~0xAC the high byte
/// decodes as a return/`athrow`/multi-byte op — a phantom block terminator that
/// makes every later use of a local invisible to liveness. The allocator then
/// coalesced two *live* doubles onto one XMM register (commons-math3
/// `FastMath.polySine` compiled `p*x2*x` as `p*x2*x2`), which is the
/// `FastMath.sin(3π/4) = 1.2252` transform-suite miscompile. Keep this table
/// and `x64::bytecode_len_at` in lockstep.
pub(crate) fn bc_len(code: &[u8], pc: usize) -> usize {
    match code[pc] {
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xa6
        | 0xa7
        | 0xa8
        | 0xb2
        | 0xb3
        | 0xb4
        | 0xb5
        | 0xb6
        | 0xb7
        | 0xb8
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        0xbb => 3,
        0xc5 => 4,
        // 5-byte instructions: invokeinterface (0xb9: opcode, cp_hi, cp_lo,
        // count, 0), invokedynamic (0xba: opcode, cp_hi, cp_lo, 0, 0), and the
        // wide-offset branches goto_w (0xc8) / jsr_w (0xc9: opcode + 4-byte
        // signed offset). `invokeinterface` and `invokedynamic` are both
        // reachable here today (`jit_scan` accepts both — invokedynamic
        // unconditionally deopts to the interpreter at that instruction, see
        // the x64.rs 0xba codegen arm); `goto_w` and `jsr_w` are still
        // rejected by `jit_scan` (its catch-all returns `None`), so no
        // compiled method contains them today — but, exactly as for `wide`
        // (0xc4) below, the length table must stay correct as defense-in-depth
        // so every PC-stepping consumer (liveness/`bc_len`, branch-target
        // precompute, DCE, OSR/unroll, oop maps) stays in lockstep if any of
        // them is ever accepted. A missing entry under-counts the instruction
        // by 4 bytes and desyncs the walk — the same class of liveness-desync
        // miscompile that the missing-`ldc` bug caused. Keep the x64.rs
        // `bytecode_len_at` twin in sync.
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        // wide (0xc4) — prefix modifies the following opcode to use a 2-byte
        // local index. JVMS §6.5 wide: `wide <opcode> <indexbyte1> <indexbyte2>`
        // is 4 bytes for the load/store/ret family, and `wide iinc <index>
        // <const>` is 6 bytes (extra 2-byte signed constant). The modified
        // opcode is the byte at `pc + 1`: only `iinc` (0x84) takes the 6-byte
        // form. NOT latent, whatever this comment used to say: `jit_scan`
        // accepts the widened load/store and `iinc` forms
        // (`x64/bytecode_compat.rs`), so compiled methods DO contain `wide` and
        // every PC-stepping consumer of this table is load-bearing rather than
        // defensive. Keep the x64.rs `bytecode_len_at` twin in
        // sync.
        0xc4 => {
            if pc + 1 < code.len() && code[pc + 1] == 0x84 {
                6 // wide iinc
            } else {
                4 // wide <load/store/ret>
            }
        }
        // tableswitch — variable length.
        //
        // HIGH security fix: adversarial bytecode can craft `high < low - 1`
        // such that `(high - low + 1)` either wraps (signed-overflow UB in
        // debug, two's-complement wrap in release) or, after `.max(0) as usize`,
        // becomes an enormous value that overflows subsequent address
        // arithmetic. We compute the count with `checked_sub`/`checked_add`,
        // clamp against `MAX_TABLESWITCH_ENTRIES`, and fall back to length 1
        // on overflow — the caller (`jit_scan` / `compile_bytecode`) re-checks
        // and bails the method out of JIT compilation.
        0xaa => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            if p + 12 > code.len() {
                return 1;
            }
            let low = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            let high = i32::from_be_bytes([code[p + 8], code[p + 9], code[p + 10], code[p + 11]]);
            let count = match (high as i64)
                .checked_sub(low as i64)
                .and_then(|d| d.checked_add(1))
            {
                Some(n) if n >= 0 && (n as u64) <= MAX_TABLESWITCH_ENTRIES as u64 => n as usize,
                _ => return 1, // overflow or cap exceeded — bail (caller will reject method)
            };
            // Saturate the address arithmetic too: a pathological but in-cap
            // count multiplied by 4 still fits in u64, but using checked_*
            // documents intent and protects future cap raises.
            match count
                .checked_mul(4)
                .and_then(|x| x.checked_add(p + 12))
                .and_then(|x| x.checked_sub(pc))
            {
                Some(len) => len,
                None => 1,
            }
        }
        // lookupswitch — variable length.
        //
        // HIGH security fix: previously `npairs` was cast from `i32` directly
        // to `usize`, so a negative `npairs` (e.g. `i32::MIN`) became a huge
        // `usize` and the subsequent multiply/add produced wildly OOB
        // pointers. Reject negative `npairs` and clamp positive values against
        // `MAX_LOOKUPSWITCH_NPAIRS`; fall back to length 1 on violation.
        0xab => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            if p + 8 > code.len() {
                return 1;
            }
            let npairs_raw =
                i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            if npairs_raw < 0 {
                return 1;
            }
            let npairs = npairs_raw as usize;
            if npairs > MAX_LOOKUPSWITCH_NPAIRS {
                return 1;
            }
            match npairs
                .checked_mul(8)
                .and_then(|x| x.checked_add(p + 8))
                .and_then(|x| x.checked_sub(pc))
            {
                Some(len) => len,
                None => 1,
            }
        }
        _ => 1,
    }
}

/// Compute the branch target PC for a branch instruction at `pc`, if any.
/// Returns `None` if the instruction is not a branch or the target is out of bounds.
fn branch_target(code: &[u8], pc: usize) -> Option<usize> {
    match code[pc] {
        0x99..=0xa6 | 0xa7 | 0xc6 | 0xc7 => {
            // JVM branch offsets are big-endian signed i16.
            let offset = i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]) as i32;
            let target = pc as i32 + offset;
            // Validate the target is non-negative (valid bytecode offset)
            if target < 0 {
                return None;
            }
            Some(target as usize)
        }
        0xc8 | 0xc9 => {
            // goto_w / jsr_w use a signed 32-bit branch offset.
            let offset = i32::from_be_bytes([
                *code.get(pc + 1)?,
                *code.get(pc + 2)?,
                *code.get(pc + 3)?,
                *code.get(pc + 4)?,
            ]);
            pc.checked_add_signed(offset as isize)
        }
        _ => None,
    }
}

/// Returns true if the opcode is an unconditional control transfer (goto, return, athrow, switch).
fn is_unconditional(op: u8) -> bool {
    matches!(
        op,
        0xa7    // goto
        | 0xc8  // goto_w
        | 0xaa  // tableswitch
        | 0xab  // lookupswitch
        | 0xac  // ireturn
        | 0xad  // lreturn
        | 0xae  // freturn
        | 0xaf  // dreturn
        | 0xb0  // areturn
        | 0xb1  // return (void)
        | 0xbf // athrow
    )
}

/// Collect all branch targets from a switch instruction at `pc`.
fn switch_targets(code: &[u8], pc: usize, code_len: usize) -> Vec<usize> {
    let mut targets = Vec::new();
    let op = code[pc];
    let base_pc = pc;
    let mut p = pc + 1;
    while p % 4 != 0 {
        p += 1;
    }

    match op {
        // HIGH security fix: same overflow audit as `bc_len` above. The
        // inner per-target loop already has a `code_len` bound, but if
        // `count` is allowed to be `i32::MAX` (or wrap via signed overflow)
        // we still spin billions of iterations, which is a DoS in itself.
        0xaa => {
            // tableswitch
            if p + 12 > code_len {
                return targets;
            }
            let default_off = i32::from_be_bytes([code[p], code[p + 1], code[p + 2], code[p + 3]]);
            // Use checked arithmetic: a malformed/negative offset must not
            // wrap to a huge usize. Skip targets that fall outside the code.
            if let Some(t) = base_pc.checked_add_signed(default_off as isize) {
                if t < code_len {
                    targets.push(t);
                }
            }
            let low = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            let high = i32::from_be_bytes([code[p + 8], code[p + 9], code[p + 10], code[p + 11]]);
            let count = match (high as i64)
                .checked_sub(low as i64)
                .and_then(|d| d.checked_add(1))
            {
                Some(n) if n >= 0 && (n as u64) <= MAX_TABLESWITCH_ENTRIES as u64 => n as usize,
                _ => return targets, // overflow / cap exceeded → no targets harvested
            };
            p += 12;
            for _ in 0..count {
                if p + 4 > code_len {
                    break;
                }
                let off = i32::from_be_bytes([code[p], code[p + 1], code[p + 2], code[p + 3]]);
                if let Some(t) = base_pc.checked_add_signed(off as isize) {
                    if t < code_len {
                        targets.push(t);
                    }
                }
                p += 4;
            }
        }
        0xab => {
            // lookupswitch
            if p + 8 > code_len {
                return targets;
            }
            let default_off = i32::from_be_bytes([code[p], code[p + 1], code[p + 2], code[p + 3]]);
            if let Some(t) = base_pc.checked_add_signed(default_off as isize) {
                if t < code_len {
                    targets.push(t);
                }
            }
            let npairs_raw =
                i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            if npairs_raw < 0 {
                return targets;
            }
            let npairs = npairs_raw as usize;
            if npairs > MAX_LOOKUPSWITCH_NPAIRS {
                return targets;
            }
            p += 8;
            for _ in 0..npairs {
                if p + 8 > code_len {
                    break;
                }
                let off = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
                if let Some(t) = base_pc.checked_add_signed(off as isize) {
                    if t < code_len {
                        targets.push(t);
                    }
                }
                p += 8;
            }
        }
        _ => {}
    }
    targets
}

/// RBC.6 local-handler-safety fix v2 — proper CFG-based "definitely
/// assigned" forward dataflow, replacing an earlier raw-pc-order
/// approximation in `jit/src/lib.rs::local_handler_reads_unsafe_local`
/// that had two real bugs:
///
/// 1. **Over-conservative** (false rejects): it scanned linearly from a
///    handler's `handler_pc` all the way to the END OF THE METHOD,
///    ignoring `athrow`/`return` terminators — so a short, simple handler
///    (e.g. `astore; new Ex; ...; athrow`) was incorrectly judged unsafe
///    whenever *unrelated, later code in the same method* (reachable only
///    via a completely different branch, never through this handler)
///    happened to read a non-param local. This was confirmed to be the
///    actual, sole remaining blocker for `Response.toAbsolute()` — the
///    doc's own motivating case — even after the RBC.6 gate itself and the
///    unrelated BUG-LQB-SCOPE gate were both fixed.
/// 2. **Under-conservative** (a latent, never-triggered soundness gap):
///    the raw-pc-order check ("is there SOME store to this slot at a lower
///    pc, reachable from handler_pc") is not path-sensitive. For
///    `if (c) { x = ...; } else { read x; }` inside a handler, javac emits
///    the true-branch (the store) at a LOWER pc than the false-branch (the
///    read) regardless of which branch actually executes — the old check
///    would see "a reachable store exists at a lower pc" and wrongly
///    accept the method even on a run that takes the false branch, where
///    `x` was never actually written.
///
/// Both are fixed by real dominance: a load is safe only if the slot is
/// written on EVERY control-flow path from `entry_pc` to that load. This
/// is a standard forward "must" (available-values) dataflow: each freshly
/// discovered program point's safe-set starts as its first predecessor's
/// propagated set; every ADDITIONAL predecessor INTERSECTS its own
/// propagated set in (a slot is only safe at a merge point if every
/// incoming path made it safe), re-queuing successors whenever a point's
/// safe-set shrinks. Terminates because safe-sets are monotonically
/// non-increasing, bounded below by the empty set.
///
/// `initial_safe_slots` is a bitmask (bit `i` = local slot `i`) of locals
/// safe at `entry_pc` before any handler-local code runs (`this` + declared
/// params). Slots numbered >= 64 cannot be represented in the bitmask;
/// any load of such a slot is conservatively treated as unsafe (this can
/// only make the check MORE conservative, never less — matching every
/// other "when in doubt, don't compile" gate in this scanner).
///
/// Reuses this module's own, already load-bearing bytecode-width/branch
/// decoding (`bc_len`, `branch_target`, `is_unconditional`,
/// `switch_targets` — the exact functions `build_cfg` itself uses) so this
/// can never diverge from the CFG this backend already trusts for register
/// allocation.
pub(crate) fn handler_has_unsafe_local_read(
    code: &[u8],
    code_len: usize,
    entry_pc: usize,
    initial_safe_slots: u64,
) -> bool {
    use std::collections::{HashMap, VecDeque};

    if entry_pc >= code_len {
        return false;
    }

    fn merge_successor(
        safe_at: &mut HashMap<usize, u64>,
        worklist: &mut VecDeque<usize>,
        target: usize,
        code_len: usize,
        incoming: u64,
    ) {
        if target >= code_len {
            return;
        }
        match safe_at.get(&target).copied() {
            None => {
                safe_at.insert(target, incoming);
                worklist.push_back(target);
            }
            Some(existing) => {
                let merged = existing & incoming;
                if merged != existing {
                    safe_at.insert(target, merged);
                    worklist.push_back(target);
                }
            }
        }
    }

    let mut safe_at: HashMap<usize, u64> = HashMap::new();
    safe_at.insert(entry_pc, initial_safe_slots);
    let mut worklist: VecDeque<usize> = VecDeque::new();
    worklist.push_back(entry_pc);
    // Bound iterations defensively (methods here are already capped far
    // below this by other JIT scan limits) so a pathological CFG can only
    // ever fall through to "conservatively reject", never hang.
    let mut steps = 0usize;
    const MAX_STEPS: usize = 2_000_000;

    while let Some(pc) = worklist.pop_front() {
        steps += 1;
        if steps > MAX_STEPS {
            return true;
        }
        if pc >= code_len {
            continue;
        }
        let safe = *safe_at.get(&pc).unwrap_or(&0);
        let op = code[pc];
        let len = bc_len(code, pc);
        if pc + len > code_len + 1 {
            // Truncated instruction at the tail — cannot safely decode
            // further; conservatively reject rather than read out of bounds.
            return true;
        }

        let mut propagated = safe;
        let mut unsafe_read = false;

        // Mirrors jit_scan's own load/store/iinc decoding exactly (same
        // opcode ranges and slot-index arithmetic as `local_slot_ops` in
        // jit/src/x64.rs).
        match op {
            0x15..=0x19 => {
                if pc + 1 < code.len() {
                    let slot = code[pc + 1] as u32;
                    unsafe_read = slot >= 64 || (safe & (1u64 << slot)) == 0;
                } else {
                    unsafe_read = true;
                }
            }
            0x1a..=0x1d => {
                let slot = (op - 0x1a) as u32;
                unsafe_read = (safe & (1u64 << slot)) == 0;
            }
            0x1e..=0x21 => {
                let slot = (op - 0x1e) as u32;
                unsafe_read = (safe & (1u64 << slot)) == 0;
            }
            0x22..=0x29 => {
                let slot = ((op - 0x22) % 4) as u32;
                unsafe_read = (safe & (1u64 << slot)) == 0;
            }
            0x2a..=0x2d => {
                let slot = (op - 0x2a) as u32;
                unsafe_read = (safe & (1u64 << slot)) == 0;
            }
            0x36..=0x3a => {
                if pc + 1 < code.len() {
                    let slot = code[pc + 1] as u32;
                    if slot < 64 {
                        propagated |= 1u64 << slot;
                    }
                }
            }
            0x3b..=0x3e => {
                let slot = (op - 0x3b) as u32;
                if slot < 64 {
                    propagated |= 1u64 << slot;
                }
            }
            0x3f..=0x42 => {
                let slot = (op - 0x3f) as u32;
                if slot < 64 {
                    propagated |= 1u64 << slot;
                }
            }
            0x43..=0x4a => {
                let slot = ((op - 0x43) % 4) as u32;
                if slot < 64 {
                    propagated |= 1u64 << slot;
                }
            }
            0x4b..=0x4e => {
                let slot = (op - 0x4b) as u32;
                if slot < 64 {
                    propagated |= 1u64 << slot;
                }
            }
            // wide <load|store|iinc|ret> <u16 index> — the same reads and
            // writes with a 16-bit slot. Without this arm a `wide iload 70`
            // or a `wide iinc` of an unassigned slot read as safe.
            0xc4 => {
                if pc + 3 < code.len() {
                    let real = code[pc + 1];
                    let slot = u32::from(u16::from_be_bytes([code[pc + 2], code[pc + 3]]));
                    let is_safe = slot < 64 && (safe & (1u64 << slot)) != 0;
                    match real {
                        0x15..=0x19 | 0xa9 => unsafe_read = !is_safe,
                        0x36..=0x3a => {
                            if slot < 64 {
                                propagated |= 1u64 << slot;
                            }
                        }
                        0x84 => {
                            unsafe_read = !is_safe;
                            if slot < 64 {
                                propagated |= 1u64 << slot;
                            }
                        }
                        _ => unsafe_read = true,
                    }
                } else {
                    unsafe_read = true;
                }
            }
            // ret reads its return-address local.
            0xa9 => {
                if pc + 1 < code.len() {
                    let slot = code[pc + 1] as u32;
                    unsafe_read = slot >= 64 || (safe & (1u64 << slot)) == 0;
                } else {
                    unsafe_read = true;
                }
            }
            0x84 => {
                // iinc — reads then writes the same slot.
                if pc + 1 < code.len() {
                    let slot = code[pc + 1] as u32;
                    unsafe_read = slot >= 64 || (safe & (1u64 << slot)) == 0;
                    if slot < 64 {
                        propagated |= 1u64 << slot;
                    }
                } else {
                    unsafe_read = true;
                }
            }
            _ => {}
        }

        if unsafe_read {
            return true;
        }

        if is_unconditional(op) {
            if matches!(op, 0xaa | 0xab) {
                for t in switch_targets(code, pc, code_len) {
                    merge_successor(&mut safe_at, &mut worklist, t, code_len, propagated);
                }
            } else if let Some(t) = branch_target(code, pc) {
                merge_successor(&mut safe_at, &mut worklist, t, code_len, propagated);
            }
            // return-family / athrow: no successors — this control-flow
            // path ends here, which is exactly the case the old linear
            // scan got wrong by continuing past it.
        } else {
            if let Some(t) = branch_target(code, pc) {
                merge_successor(&mut safe_at, &mut worklist, t, code_len, propagated);
            }
            let next = pc + len;
            merge_successor(&mut safe_at, &mut worklist, next, code_len, propagated);
        }
    }

    false
}

/// Build the control flow graph from bytecode.
fn build_cfg(code: &[u8], code_len: usize) -> Vec<BasicBlock> {
    build_cfg_with_leaders(code, code_len, &[])
}

/// [`build_cfg`] plus caller-supplied extra leaders. Exception handler entry
/// pcs are leaders even though no branch targets them — the runtime's
/// exception router enters them directly — and a handler that is not a block
/// start has no `live_in` for [`live_locals_per_pc_with_handlers`] to read.
fn build_cfg_with_leaders(
    code: &[u8],
    code_len: usize,
    extra_leaders: &[usize],
) -> Vec<BasicBlock> {
    // First pass: find all block-starting PCs
    let mut block_starts = vec![false; code_len + 1];
    block_starts[0] = true;
    for &leader in extra_leaders {
        if leader < code_len {
            block_starts[leader] = true;
        }
    }

    let mut pc = 0;
    while pc < code_len {
        let len = bc_len(code, pc);
        if let Some(target) = branch_target(code, pc) {
            if target < code_len {
                block_starts[target] = true;
            }
            // Fallthrough after branch
            let next = pc + len;
            if next < code_len {
                block_starts[next] = true;
            }
        }
        // Handle switch instructions — mark all targets as block starts
        if matches!(code[pc], 0xaa | 0xab) {
            for target in switch_targets(code, pc, code_len) {
                if target < code_len {
                    block_starts[target] = true;
                }
            }
            let next = pc + len;
            if next < code_len {
                block_starts[next] = true;
            }
        }
        // Mark the instruction FOLLOWING any unconditional control transfer as
        // a block start.
        //
        // `goto`/`goto_w` and the two switches already reach this via the
        // `branch_target` / switch arms above; the missing family was
        // `ireturn..return` (0xac-0xb1) and `athrow` (0xbf), which have no
        // branch target and so never marked their successor PC. Second pass
        // below SKIPS any PC that is not a block start once the first block
        // exists:
        //
        //     if !block_starts[pc] && !blocks.is_empty() { pc += bc_len(..); continue; }
        //
        // so a region that begins right after a `return`/`athrow` and is not
        // otherwise a branch target belonged to NO basic block at all. Its
        // local loads/stores were invisible to `compute_gen_kill`,
        // `build_interference` and `live_locals_per_pc` — which is exactly the
        // CM-FASTMATH failure shape (invisible uses ⇒ two simultaneously-live
        // locals coalesced onto one register), just reached through the CFG
        // instead of through a bad instruction length.
        //
        // The reachable instance of that region is an **exception handler**
        // whose protected block ends in `return`/`athrow` (javac's ordinary
        // shape when the `try` body returns). Handlers are entered by the
        // runtime's exception router, never by a fall-through or branch, so
        // nothing else marks them. Today a handler that reads a non-parameter
        // local also fails the RBC.6 admission gate in
        // `jit/src/lib.rs::local_handler_reads_unsafe_local`, so the method is
        // refused before it can be miscompiled — this fix removes the reliance
        // on that coincidence and is a prerequisite for relaxing it (see
        // `jit-regalloc-and-deopt.md`).
        //
        // Direction of the change is monotone-safe: more block starts ⇒ more
        // blocks ⇒ strictly MORE code covered by gen/kill and interference.
        // Extra interference edges can only make the coloring more
        // conservative (a local spills to its frame slot, which is always
        // correct), never less. Instruction stepping is unchanged — the skip
        // loop already walked the region with the same `bc_len`.
        if is_unconditional(code[pc]) {
            let next = pc + len;
            if next < code_len {
                block_starts[next] = true;
            }
        }
        pc += len;
    }

    // Second pass: build blocks
    let mut blocks = Vec::new();
    let mut block_idx_at: Vec<usize> = vec![usize::MAX; code_len + 1]; // pc → block index
    pc = 0;
    while pc < code_len {
        if !block_starts[pc] && !blocks.is_empty() {
            pc += bc_len(code, pc);
            continue;
        }
        let start = pc;
        let idx = blocks.len();
        block_idx_at[start] = idx;

        // Walk to end of block
        loop {
            let len = bc_len(code, pc);
            let next = pc + len;
            let is_branch = branch_target(code, pc).is_some();
            let is_uncond = is_unconditional(code[pc]);

            pc = next;

            // End of block if: next PC starts a new block, or this is a branch/return
            if is_branch || is_uncond || pc >= code_len || block_starts[pc] {
                break;
            }
        }

        blocks.push(BasicBlock {
            start_pc: start,
            end_pc: pc,
            successors: Vec::new(),
            gen: 0,
            kill: 0,
            live_in: 0,
            live_out: 0,
        });
    }

    // Third pass: fill in successors
    for i in 0..blocks.len() {
        let end_pc = blocks[i].end_pc;
        let last_pc = {
            // Find the last instruction in the block
            let mut p = blocks[i].start_pc;
            let mut last = p;
            while p < end_pc {
                last = p;
                p += bc_len(code, p);
            }
            last
        };

        let op = code[last_pc];

        // Add branch target
        if let Some(target) = branch_target(code, last_pc) {
            if target < code_len {
                let target_idx = block_idx_at[target];
                if target_idx != usize::MAX {
                    blocks[i].successors.push(target_idx);
                }
            }
        }

        // Add switch targets
        if matches!(op, 0xaa | 0xab) {
            for target in switch_targets(code, last_pc, code_len) {
                if target < code_len {
                    let target_idx = block_idx_at[target];
                    if target_idx != usize::MAX && !blocks[i].successors.contains(&target_idx) {
                        blocks[i].successors.push(target_idx);
                    }
                }
            }
        }

        // Add fallthrough (for conditional branches and non-terminal instructions)
        if !is_unconditional(op) && end_pc < code_len {
            let fall_idx = block_idx_at[end_pc];
            // May need to search forward if end_pc isn't exactly a block start
            if fall_idx != usize::MAX {
                blocks[i].successors.push(fall_idx);
            }
        }
    }

    blocks
}

/// Extract the local index from a bytecoded load/store instruction.
/// Returns (local_idx, is_use, is_def).
fn local_access(code: &[u8], pc: usize) -> Option<(usize, bool, bool)> {
    match code[pc] {
        // iload_0..iload_3
        0x1a..=0x1d => Some(((code[pc] - 0x1a) as usize, true, false)),
        // lload_0..lload_3
        0x1e..=0x21 => Some(((code[pc] - 0x1e) as usize, true, false)),
        // fload_0..fload_3
        0x22..=0x25 => Some(((code[pc] - 0x22) as usize, true, false)),
        // dload_0..dload_3
        0x26..=0x29 => Some(((code[pc] - 0x26) as usize, true, false)),
        // aload_0..aload_3
        0x2a..=0x2d => Some(((code[pc] - 0x2a) as usize, true, false)),
        // iload, lload, fload, dload, aload (u8 index)
        0x15..=0x19 => Some((*code.get(pc + 1)? as usize, true, false)),

        // istore_0..istore_3
        0x3b..=0x3e => Some(((code[pc] - 0x3b) as usize, false, true)),
        // lstore_0..lstore_3
        0x3f..=0x42 => Some(((code[pc] - 0x3f) as usize, false, true)),
        // fstore_0..fstore_3
        0x43..=0x46 => Some(((code[pc] - 0x43) as usize, false, true)),
        // dstore_0..dstore_3
        0x47..=0x4a => Some(((code[pc] - 0x47) as usize, false, true)),
        // astore_0..astore_3
        0x4b..=0x4e => Some(((code[pc] - 0x4b) as usize, false, true)),
        // istore, lstore, fstore, dstore, astore (u8 index)
        0x36..=0x3a => Some((*code.get(pc + 1)? as usize, false, true)),

        // iinc: both reads and writes the local
        0x84 => Some((*code.get(pc + 1)? as usize, true, true)),

        // wide: widened local index for load/store/ret or widened iinc.
        // `ret` uses a return-address local, not a Java value local this
        // allocator should color, and plain `ret` is ignored above too.
        0xc4 => {
            let modified = *code.get(pc + 1)?;
            let idx = u16::from_be_bytes([*code.get(pc + 2)?, *code.get(pc + 3)?]) as usize;
            match modified {
                0x15..=0x19 => Some((idx, true, false)),
                0x36..=0x3a => Some((idx, false, true)),
                0x84 => Some((idx, true, true)),
                _ => None,
            }
        }

        _ => None,
    }
}

/// Compute gen/kill sets for a basic block.
fn compute_gen_kill(code: &[u8], block: &mut BasicBlock) {
    let mut pc = block.start_pc;
    while pc < block.end_pc {
        if let Some((idx, is_use, is_def)) = local_access(code, pc) {
            // 64-local cap: gen/kill sets are `u64` bitsets (one bit per
            // local), so only locals with index < 64 can be tracked.
            // Locals at index >= 64 are skipped here and therefore never
            // receive a register — they fall back to the unallocated
            // frame-slot path, which is always correct. See the
            // module-level docs for the rationale. Do NOT widen this
            // bound without also widening the bitset type.
            if idx < 64 {
                let bit = 1u64 << idx;
                // For iinc: use comes before def
                if is_use && (block.kill & bit) == 0 {
                    block.gen |= bit;
                }
                if is_def {
                    block.kill |= bit;
                }
            }
        }
        pc += bc_len(code, pc);
    }
}

/// Maximum number of liveness fixpoint iterations before bailing out.
const MAX_LIVENESS_ITERATIONS: usize = 1000;

/// Bitmask of the JVM local slots that hold an incoming parameter.
///
/// `param_slots` is the backend's `param_jvm_slots`: the JVM local slot of
/// parameter `i`, category-2 aware. An empty slice selects the identity layout
/// `0..num_params`, which is what every category-1-only signature reduces to
/// (and what the unit tests and the ARM64 backend pass).
///
/// Why this is not simply `(1 << num_params) - 1`: a `long`/`double` parameter
/// occupies TWO JVM local slots (JVMS 2.6.1), so as soon as a signature
/// contains one, its LAST parameter lives at slot `num_params` or beyond.
/// Seeding only `0..num_params` left that parameter dead-on-entry; the
/// colouring was then free to hand it the same register as a live earlier
/// parameter, and the prologue -- which stores EVERY incoming argument into
/// its home -- overwrote the live one on the way in.
///
/// Reproduced by a lambda body `(long, Object, Void)`, where the trailing
/// always-null `Void` shared `r12` with the `Object`:
///
/// ```text
///   mov r15, rsi   ; this
///   mov r14, rdx   ; long
///   mov r12, rcx   ; Object   <-- live, read by the body
///   mov r12, r8    ; Void     <-- clobbers it with null
/// ```
pub fn param_live_in_mask(num_params: usize, param_slots: &[usize]) -> u64 {
    if param_slots.is_empty() {
        return if num_params >= 64 {
            u64::MAX
        } else {
            (1u64 << num_params) - 1
        };
    }
    // The 64-local cap of this module's bitsets: a parameter at slot >= 64
    // never receives a register home (`color_graph` caps at 64), so its
    // canonical frame slot is authoritative and omitting it is safe.
    param_slots
        .iter()
        .filter(|&&slot| slot < 64)
        .fold(0u64, |mask, &slot| mask | (1u64 << slot))
}

/// Solve liveness to fixpoint using worklist iteration.
///
/// `param_live_mask` is [`param_live_in_mask`] -- the slots holding an incoming
/// parameter, all of which are live-in at the entry block by definition.
fn solve_liveness(blocks: &mut [BasicBlock], param_live_mask: u64) {
    // Seed: parameters are live-in at entry block
    if !blocks.is_empty() {
        blocks[0].gen |= param_live_mask & !blocks[0].kill;
    }

    // Worklist iteration (backward dataflow) with iteration limit
    let mut changed = true;
    let mut iterations = 0;
    while changed && iterations < MAX_LIVENESS_ITERATIONS {
        changed = false;
        iterations += 1;
        for i in (0..blocks.len()).rev() {
            // live_out = union of live_in of successors
            let mut new_out = 0u64;
            for &succ in &blocks[i].successors {
                new_out |= blocks[succ].live_in;
            }
            // live_in = gen | (live_out - kill)
            let new_in = blocks[i].gen | (new_out & !blocks[i].kill);

            if new_in != blocks[i].live_in || new_out != blocks[i].live_out {
                blocks[i].live_in = new_in;
                blocks[i].live_out = new_out;
                changed = true;
            }
        }
    }
}

/// Per-protected-range handler liveness: `(start_pc, end_pc, handler_live_in)`.
///
/// Built after [`solve_liveness`] so each entry carries the *live-in* set of its
/// handler's block — the locals the handler reads before defining them.
fn handler_live_in_ranges(
    blocks: &[BasicBlock],
    handlers: &[(usize, usize, usize)],
) -> Vec<(usize, usize, u64)> {
    handlers
        .iter()
        .filter_map(|&(start, end, handler)| {
            blocks
                .iter()
                .find(|b| b.start_pc == handler)
                .map(|b| (start, end, b.live_in))
        })
        .filter(|&(_, _, live_in)| live_in != 0)
        .collect()
}

/// Locals that are live at `pc` *because* an exception thrown there would enter
/// a handler covering it.
///
/// A block-level exception edge (`build_cfg_with_handlers`) is NOT sufficient on
/// its own. It only contributes the handler's live-in to the block's `live_out`,
/// and the backward intra-block walk then applies every def between `pc` and the
/// block's end — so a local the handler reads, but that the protected code
/// *redefines* after `pc`, comes out dead exactly at `pc`. That is wrong: on the
/// exception path those later defs never execute, and control reaches the
/// handler with the locals as they stand AT `pc`.
///
/// Concretely (`C2Handlers.loopInsideTry`, the witness for this fix):
///
/// ```text
///   29: iload_1        sum pushed on the operand stack
///   33: invokestatic   <- throws here; handler at 41 reads local 1
///   37: istore_1       sum redefined -- kills local 1 walking backward
///   41: astore_2 / iload_1 / ineg / ireturn      (handler: returns -sum)
/// ```
///
/// `live_out` of the block did contain local 1 (via the handler edge), but the
/// `istore_1` at 37 cleared it before the walk reached 33, so the precise
/// exceptional frame recorded `sum` as `FrameValue::Undefined` and the handler
/// returned `-0`. Unioning the handler's live-in at every covered pc is what
/// makes the snapshot — and the register allocator's interference graph — agree
/// with the exception edge they both claim to model.
fn handler_live_mask(pc: usize, handler_ranges: &[(usize, usize, u64)]) -> u64 {
    let mut mask = 0u64;
    for &(start, end, live_in) in handler_ranges {
        if pc >= start && pc < end {
            mask |= live_in;
        }
    }
    mask
}

/// Build interference graph from liveness data.
/// Returns `interference[i]` = bitmask of locals that interfere with local `i`.
///
/// Walks backward through each block at instruction granularity to capture
/// intra-block interference (e.g., a local defined and used within the same
/// block that overlaps with another local's live range).
///
/// `handler_ranges` (empty for every compile that does not reconstruct a handler
/// frame from register homes) keeps a local the handler reads from sharing a
/// register with something defined inside the protected range — see
/// [`handler_live_mask`].
fn build_interference(
    code: &[u8],
    blocks: &[BasicBlock],
    num_locals: usize,
    handler_ranges: &[(usize, usize, u64)],
) -> Vec<u64> {
    let mut interference = vec![0u64; num_locals];
    let n = num_locals.min(64);

    for block in blocks {
        // Collect instruction PCs in this block for backward walk
        let mut pcs = Vec::new();
        {
            let mut pc = block.start_pc;
            while pc < block.end_pc {
                pcs.push(pc);
                pc += bc_len(code, pc);
            }
        }

        // Start with live_out and walk backward
        let mut live = block.live_out;

        for &pc in pcs.iter().rev() {
            // Locals the handler will read if this pc throws. They are live
            // here on the exception path even when a later def in this block
            // kills them on the normal path.
            let hmask = handler_live_mask(pc, handler_ranges);
            if let Some((idx, is_use, is_def)) = local_access(code, pc) {
                if idx < n {
                    let bit = 1u64 << idx;
                    if is_def {
                        // At a def point, the defined local interferes with everything
                        // currently live (excluding itself).
                        let others = (live | hmask) & !(bit);
                        interference[idx] |= others;
                        for j in 0..n {
                            if others & (1u64 << j) != 0 {
                                interference[j] |= bit;
                            }
                        }
                        // After def (going backward = before def), remove from live
                        // unless it's also a use (iinc)
                        if !is_use {
                            live &= !bit;
                        }
                    }
                    if is_use {
                        live |= bit;
                    }
                }
            }
        }

        // Also mark interference for live_in (block boundary liveness)
        let combined = block.live_in;
        for i in 0..n {
            if combined & (1u64 << i) != 0 {
                interference[i] |= combined & !(1u64 << i);
            }
        }
    }

    interference
}

/// Per-level loop weight multiplier (HotSpot-style). A use at nesting depth `d`
/// contributes `LOOP_WEIGHT_PER_DEPTH^d`, capped at `MAX_LOOP_DEPTH_WEIGHT` to
/// keep saturating arithmetic well-behaved and bound the influence of
/// pathologically deep loops on register-allocation priorities.
pub const LOOP_WEIGHT_PER_DEPTH: u32 = 10;
/// Cap on the loop-depth weight contributed by a single use.
///
/// Corresponds to depth = 6 with `LOOP_WEIGHT_PER_DEPTH = 10` (=> 10^6).
/// At this cap a deeply-nested use still dominates a non-loop use by 1e6×,
/// which is more than enough for the graph-coloring spill heuristic.
pub const MAX_LOOP_DEPTH_WEIGHT: u32 = 1_000_000;

/// Count uses of each local in the bytecode, weighted by loop-nesting depth.
///
/// For each use we compute `depth` = the number of distinct loop ranges in
/// `loops` whose `[header, back_edge]` interval contains the use's PC, and
/// contribute `LOOP_WEIGHT_PER_DEPTH^depth` (capped) to the local's count.
/// This mirrors HotSpot's C2 frequency model where each enclosing loop
/// multiplies the perceived execution frequency, making locals that live in
/// the hottest part of the method strongly prefer callee-saved registers.
///
/// Loop ranges that are malformed (header > back_edge) or out of bounds
/// (back_edge >= code_len) are ignored for safety.
fn count_uses(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    loops: &[(usize, usize)],
) -> Vec<u32> {
    let mut counts = vec![0u32; num_locals];

    let mut pc = 0;
    while pc < code_len {
        if let Some((idx, _, _)) = local_access(code, pc) {
            if idx < num_locals {
                // Loop-nesting depth = number of valid enclosing loop ranges.
                let depth: u32 = loops
                    .iter()
                    .filter(|&&(header, back_edge)| {
                        header <= back_edge
                            && back_edge < code_len
                            && pc >= header
                            && pc <= back_edge
                    })
                    .count() as u32;
                // Exponential weight by depth, capped to avoid overflow.
                // depth == 0 -> weight == 1 (out-of-loop use).
                let weight = LOOP_WEIGHT_PER_DEPTH
                    .checked_pow(depth)
                    .unwrap_or(MAX_LOOP_DEPTH_WEIGHT)
                    .min(MAX_LOOP_DEPTH_WEIGHT);
                counts[idx] = counts[idx].saturating_add(weight);
            }
        }
        pc += bc_len(code, pc);
    }

    counts
}

/// Graph coloring using simplified Chaitin-Briggs.
fn color_graph(
    interference: &[u64],
    num_locals: usize,
    available_regs: &[u8],
    use_counts: &[u32],
) -> Vec<Option<u8>> {
    let k = available_regs.len();
    if num_locals == 0 || k == 0 {
        return vec![None; num_locals];
    }

    // 64-local cap: the interference graph and the `removed` working set
    // are `u64` bitsets, so coloring only ever considers the first 64
    // locals. Any local with index >= 64 gets `None` (no register) and
    // falls back to the unallocated frame-slot path. See the module-level
    // docs for the rationale; widening this requires a wider bitset type.
    let n = num_locals.min(64);
    let mut removed = 0u64; // bitmask of removed nodes
    let mut stack: Vec<(usize, bool)> = Vec::with_capacity(n); // (local_idx, is_potential_spill)

    // Compute initial degrees
    let mut degrees = vec![0u32; n];
    for i in 0..n {
        degrees[i] = (interference[i] & !removed).count_ones();
    }

    // Simplify + potential spill
    for _ in 0..n {
        // Try to find a node with degree < K
        let mut found = None;
        for i in 0..n {
            if removed & (1u64 << i) != 0 {
                continue;
            }
            if (degrees[i] as usize) < k {
                found = Some((i, false));
                break;
            }
        }

        // If none found, pick potential spill: lowest use_count / degree ratio
        if found.is_none() {
            let mut best = None;
            let mut best_score = f64::MAX;
            for i in 0..n {
                if removed & (1u64 << i) != 0 {
                    continue;
                }
                let score = if degrees[i] == 0 {
                    0.0
                } else {
                    use_counts[i] as f64 / degrees[i] as f64
                };
                if score < best_score {
                    best_score = score;
                    best = Some(i);
                }
            }
            found = best.map(|i| (i, true));
        }

        let Some((node, is_spill)) = found else {
            break;
        };

        // Remove node from graph, reduce neighbors' degrees
        removed |= 1u64 << node;
        let neighbors = interference[node] & !removed;
        for j in 0..n {
            if neighbors & (1u64 << j) != 0 {
                degrees[j] = degrees[j].saturating_sub(1);
            }
        }
        stack.push((node, is_spill));
    }

    // Select: pop nodes, assign colors
    let mut assignments = vec![None; num_locals];
    while let Some((node, _is_spill)) = stack.pop() {
        // Find colors used by already-colored neighbors
        let mut used_colors = 0u64; // bitmask of color indices
        let neighbors = interference[node];
        for j in 0..n {
            if neighbors & (1u64 << j) != 0 {
                if let Some(reg) = assignments[j] {
                    // Find the color index for this register
                    if let Some(ci) = available_regs.iter().position(|&r| r == reg) {
                        used_colors |= 1u64 << ci;
                    }
                }
            }
        }

        // Assign the lowest available color
        for (ci, &reg) in available_regs.iter().enumerate() {
            if used_colors & (1u64 << ci) == 0 {
                assignments[node] = Some(reg);
                break;
            }
        }
        // If no color available (spill), assignments[node] stays None
    }

    assignments
}

/// Detect which locals are used as float/double (cannot be stored in GPR registers).
fn find_float_locals(code: &[u8], code_len: usize, num_locals: usize) -> u64 {
    let mut float_mask = 0u64;
    let mut pc = 0;
    while pc < code_len {
        match code[pc] {
            // fload_0..fload_3
            0x22..=0x25 => {
                let idx = (code[pc] - 0x22) as usize;
                if idx < 64 {
                    float_mask |= 1u64 << idx;
                }
            }
            // dload_0..dload_3
            0x26..=0x29 => {
                let idx = (code[pc] - 0x26) as usize;
                if idx < 64 {
                    float_mask |= 1u64 << idx;
                }
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                let idx = (code[pc] - 0x43) as usize;
                if idx < 64 {
                    float_mask |= 1u64 << idx;
                }
            }
            // dstore_0..dstore_3
            0x47..=0x4a => {
                let idx = (code[pc] - 0x47) as usize;
                if idx < 64 {
                    float_mask |= 1u64 << idx;
                }
            }
            // fload, dload (u8 index)
            0x17 | 0x18 => {
                let Some(&raw_idx) = code.get(pc + 1) else {
                    pc += bc_len(code, pc);
                    continue;
                };
                let idx = raw_idx as usize;
                if idx < 64 {
                    float_mask |= 1u64 << idx;
                }
            }
            // fstore, dstore (u8 index)
            0x38 | 0x39 => {
                let Some(&raw_idx) = code.get(pc + 1) else {
                    pc += bc_len(code, pc);
                    continue;
                };
                let idx = raw_idx as usize;
                if idx < 64 {
                    float_mask |= 1u64 << idx;
                }
            }
            // wide fload/dload/fstore/dstore: same float-category mark with
            // a widened local index.
            0xc4 => {
                if matches!(code.get(pc + 1), Some(&0x17 | &0x18 | &0x38 | &0x39)) {
                    if let (Some(&hi), Some(&lo)) = (code.get(pc + 2), code.get(pc + 3)) {
                        let idx = u16::from_be_bytes([hi, lo]) as usize;
                        if idx < 64 {
                            float_mask |= 1u64 << idx;
                        }
                    }
                }
            }
            _ => {}
        }
        pc += bc_len(code, pc);
    }
    let _ = num_locals; // used for documentation
    float_mask
}

/// Detect which locals are accessed as `int` / `long` / reference — the GPR
/// category, i.e. the complement of [`find_float_locals`].
///
/// Exists only to find slots that appear in BOTH scans. javac reuses a JVM
/// local slot the moment the previous variable's scope ends, and it does not
/// care whether the next occupant has the same type category:
///
/// ```java
/// for (int i = 0; i < N; i++) { sum += (i % 17) * 0.5 + acc0; }
/// double tail = sum / 3.0;      // <- javac gives `tail` the slot `i` had
/// ```
///
/// compiles to `istore 11` / `iload 11` / `iinc 11` inside the loop and
/// `dstore 11` / `dload 11` after it. [`find_float_locals`] marks slot 11
/// float because *something* stored a double there, so the slot gets an XMM
/// home for the whole method — while its integer accesses go through the
/// canonical frame slot, since `reg_for_local` returns `None` for a
/// float-masked local.
///
/// Each category is internally consistent, so an ordinary entry compiles
/// correct code. The OSR trampoline, however, seeds locals by index and
/// elides the frame-slot store for any local that has a register home. For a
/// dual-category slot that deposits the interpreter's `int i` into an XMM
/// register nothing reads and leaves the frame slot the loop actually reads
/// **uninitialised** — an OSR-entered loop starts its counter at garbage.
/// Measured on `probes/SlotReuseCategoryProbe.java`: `reused` returns
/// 1.7716133435E9 against the correct 5599982.5, while the three controls
/// that avoid the cross-category reuse are all exact (2026-07-31).
///
/// `allocate_registers_with` gives such slots no register home at all, so
/// both categories use the frame slot and the trampoline seeds it.
fn find_non_float_locals(code: &[u8], code_len: usize) -> u64 {
    fn set(idx: usize, mask: &mut u64) {
        if idx < 64 {
            *mask |= 1u64 << idx;
        }
    }
    let mut mask = 0u64;
    let mut pc = 0;
    while pc < code_len {
        match code[pc] {
            // iload_0..3 / lload_0..3 / aload_0..3
            0x1a..=0x1d => set((code[pc] - 0x1a) as usize, &mut mask),
            0x1e..=0x21 => set((code[pc] - 0x1e) as usize, &mut mask),
            0x2a..=0x2d => set((code[pc] - 0x2a) as usize, &mut mask),
            // istore_0..3 / lstore_0..3 / astore_0..3
            0x3b..=0x3e => set((code[pc] - 0x3b) as usize, &mut mask),
            0x3f..=0x42 => set((code[pc] - 0x3f) as usize, &mut mask),
            0x4b..=0x4e => set((code[pc] - 0x4b) as usize, &mut mask),
            // iload / lload / aload / istore / lstore / astore / iinc / ret
            0x15 | 0x16 | 0x19 | 0x36 | 0x37 | 0x3a | 0x84 | 0xa9 => {
                if let Some(&raw) = code.get(pc + 1) {
                    set(raw as usize, &mut mask);
                }
            }
            // wide forms of the same
            0xc4 => {
                if matches!(
                    code.get(pc + 1),
                    Some(&0x15 | &0x16 | &0x19 | &0x36 | &0x37 | &0x3a | &0x84 | &0xa9)
                ) {
                    if let (Some(&hi), Some(&lo)) = (code.get(pc + 2), code.get(pc + 3)) {
                        set(u16::from_be_bytes([hi, lo]) as usize, &mut mask);
                    }
                }
            }
            _ => {}
        }
        pc += bc_len(code, pc);
    }
    mask
}

/// Bitmask of locals that are ever accessed as a REFERENCE (`aload`/`astore`
/// in any encoding, including the `wide` forms). Mirrors
/// [`find_float_locals`]' structure.
///
/// Used by the pure-kernel callee-saved-GPR local-homes path in the x64
/// backend: reference locals must keep their canonical frame-slot homes so
/// the conservative JIT-frame root scan (and every deopt/exception path that
/// reads locals from the frame) still sees them — only non-reference locals
/// may live exclusively in callee-saved registers. javac reuses local slots
/// across unrelated scopes, so a single `aload` anywhere taints the slot for
/// the whole method (conservative and sound).
pub(crate) fn find_reference_locals(code: &[u8], code_len: usize, num_locals: usize) -> u64 {
    let mut ref_mask = 0u64;
    let mut pc = 0;
    while pc < code_len {
        match code[pc] {
            // aload_0..aload_3
            0x2a..=0x2d => {
                let idx = (code[pc] - 0x2a) as usize;
                if idx < 64 {
                    ref_mask |= 1u64 << idx;
                }
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                let idx = (code[pc] - 0x4b) as usize;
                if idx < 64 {
                    ref_mask |= 1u64 << idx;
                }
            }
            // aload / astore (u8 index)
            0x19 | 0x3a => {
                let Some(&raw_idx) = code.get(pc + 1) else {
                    pc += bc_len(code, pc);
                    continue;
                };
                let idx = raw_idx as usize;
                if idx < 64 {
                    ref_mask |= 1u64 << idx;
                }
            }
            // wide aload/astore
            0xc4 => {
                if matches!(code.get(pc + 1), Some(&0x19 | &0x3a)) {
                    if let (Some(&hi), Some(&lo)) = (code.get(pc + 2), code.get(pc + 3)) {
                        let idx = u16::from_be_bytes([hi, lo]) as usize;
                        if idx < 64 {
                            ref_mask |= 1u64 << idx;
                        }
                    }
                }
            }
            _ => {}
        }
        pc += bc_len(code, pc);
    }
    let _ = num_locals;
    ref_mask
}

// ---------------------------------------------------------------------------
// Safepoint publication planning (cross-call register residency)
// ---------------------------------------------------------------------------

/// Which register-homed locals a GC-capable call site actually has to copy back
/// into their canonical frame slots.
///
/// ## Why this exists
///
/// Every local this allocator colours receives a **callee-saved** register
/// ([`super::x64::LOCAL_REGS`] = `R12..R15, RBX` on System V, plus `RSI/RDI` on
/// Windows; [`ARM64_LOCAL_GPRS`] = `X19..X28`). Callee-saved means the value
/// survives a call *by ABI* — the callee's own prologue saves and its epilogue
/// restores it. So "keeping a hot value in a register across a call" is not a
/// missing capability here; it is the default, and it has been since the
/// graph-colouring allocator landed.
///
/// What still costs a store on every call is **publication**: the x64 emitter's
/// pre-safepoint sequence copies *every* register-homed local to
/// `[rbp - (idx+1)*8]` before *every* GC-capable call, because the GC's oop maps
/// and the conservative frame walk read stack memory only — there is no
/// register-level pointer map (`OopMapEntry` carries `frame_slot_offsets`, and
/// the long-standing `reg_oops` bitmap TODO is still unimplemented). A local
/// that can never hold an object reference contributes nothing to that scan, so
/// its store is pure overhead — paid on the hottest possible instruction of a
/// recursion-bound workload.
///
/// This plan separates the two populations so a call site can publish the oops
/// and leave the primitives in their registers.
///
/// ## Soundness
///
/// * **GC.** A local excluded from [`Self::publish_always`] is one that no
///   `astore`/`aload` in the whole method ever touches (`find_reference_locals`
///   taints a slot for the entire method on a single reference access, so
///   javac's cross-scope slot reuse cannot defeat it), unioned with the
///   caller-supplied reference-parameter mask. Such a slot can only ever hold a
///   primitive, so leaving it in a callee-saved register across a call cannot
///   create an invisible GC root. This is the same argument the pure-kernel
///   register-home path already relies on, applied per-local instead of
///   per-method.
/// * **Deopt.** The x64 snapshot builder already prefers a register descriptor
///   over a slot descriptor for a register-homed local
///   (`FrameValue::Register` / `RegisterLong` / `RegisterRef`), and the
///   frame-deopt stub spills the whole GPR file into
///   [`crate::deopt::SavedRegisters`], so reconstruction reads the *register*,
///   not the frame slot. An unpublished slot is therefore never observed.
/// * **Value.** The register is authoritative between accesses; the frame slot
///   is a copy. Skipping the copy cannot change a computed value.
pub struct SafepointPublishPlan {
    /// Locals that may hold an object reference anywhere in the method
    /// (`find_reference_locals` ∪ caller-supplied reference parameters).
    /// Bit `i` ⇒ local `i`. Locals `>= 64` are not represented and must be
    /// treated as references by the caller.
    pub reference_locals: u64,
    /// `reference_locals` restricted to locals that actually received a
    /// register home. This is the **only** population whose register residency
    /// can hide a GC root; when it is zero, no register-homed local can hold an
    /// oop and a call site may skip local publication entirely.
    pub register_homed_reference_locals: u64,
    /// Bci-independent publish set — always sound, no liveness precondition.
    /// Equal to `register_homed_reference_locals`.
    pub publish_always: u64,
    /// Per-bci publish set, additionally narrowed by liveness: a reference
    /// local whose last read is already behind us is not a live root and need
    /// not be published.
    ///
    /// Liveness here models EXCEPTION EDGES when the caller passes the method's
    /// handler ranges, so a reference local that only a catch block reads is
    /// still published at every safepoint inside the protected range. That used
    /// to be guaranteed the other way round — by
    /// `jit/src/lib.rs::local_handler_reads_unsafe_local` refusing to compile
    /// such a method at all — but the RBC.6 precise-handler-frame relaxation
    /// admits exactly that population, so the guarantee has to live here.
    /// Passing no handlers keeps the old ordinary-CFG answer (correct for a
    /// method with no exception table). Indexed by bci; entries past the end are
    /// absent, and a caller must fall back to `publish_always`.
    pub publish_at: Vec<u64>,
}

impl SafepointPublishPlan {
    /// Publish set at `bci`, falling back to the bci-independent set when the
    /// liveness table has no entry (unreached PC, out-of-range bci).
    pub fn publish_at_bci(&self, bci: usize) -> u64 {
        match self.publish_at.get(bci) {
            Some(&m) => m,
            None => self.publish_always,
        }
    }

    /// True when no register-homed local can ever hold an object reference, so
    /// a call site may omit local publication (and the conservative full-GPR
    /// spill, as far as *locals* are concerned) altogether.
    ///
    /// This is the exact predicate the x64 backend's
    /// `can_elide_self_call_register_spill` should test instead of
    /// `local_assignments.iter().any(Option::is_some)`; see the cross-owner
    /// request in `jit-regalloc-and-deopt.md`.
    pub fn no_reference_in_registers(&self) -> bool {
        self.register_homed_reference_locals == 0
    }
}

/// Build a [`SafepointPublishPlan`] for a compiled method.
///
/// `assignments` is [`RegAllocResult::assignments`] (GPR homes). XMM homes are
/// deliberately ignored: a float/double local is never a reference, so it never
/// participates in publication.
///
/// `extra_reference_locals` lets the caller union in reference *parameters*
/// (the x64 backend's `param_oop_mask`). Passing `0` is safe for any method
/// whose reference parameters are read at least once, since a single `aload`
/// taints the slot — but callers that already have a descriptor-derived mask
/// should pass it rather than rely on that.
///
/// Locals with index `>= 64` are outside the `u64` bitset used throughout this
/// module; they never receive a register home in the first place
/// ([`color_graph`] caps at 64), so they cannot appear in any of the returned
/// masks and their canonical frame slot is always authoritative.
pub fn plan_safepoint_publication(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    param_slots: &[usize],
    assignments: &[Option<u8>],
    extra_reference_locals: u64,
    handlers: &[(usize, usize, usize)],
) -> SafepointPublishPlan {
    let reference_locals =
        find_reference_locals(code, code_len, num_locals) | extra_reference_locals;

    let mut register_homed = 0u64;
    for (i, a) in assignments.iter().enumerate().take(64) {
        if a.is_some() {
            register_homed |= 1u64 << i;
        }
    }
    let register_homed_reference_locals = reference_locals & register_homed;

    // Liveness-narrowed per-bci sets.
    //
    // A pc that no basic block covers reads back as "nothing live" from the
    // liveness table, which is the DEFAULT value, not a computed answer.
    // Trusting it would publish nothing at a safepoint in unreached-but-
    // executed code (an exception handler the CFG cannot see) and drop a live
    // oop. Uncovered PCs therefore fall back to the bci-independent set.
    let (live_at, covered) =
        live_locals_per_pc_inner(code, code_len, num_params, param_slots, handlers);
    let publish_at: Vec<u64> = live_at
        .iter()
        .zip(covered.iter())
        .map(|(&live, &is_covered)| {
            if is_covered {
                live & register_homed_reference_locals
            } else {
                register_homed_reference_locals
            }
        })
        .collect();

    SafepointPublishPlan {
        reference_locals,
        register_homed_reference_locals,
        publish_always: register_homed_reference_locals,
        publish_at,
    }
}

/// Run register allocation for a method (x86-64).
pub fn allocate_registers(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_with(
        code,
        code_len,
        num_locals,
        num_params,
        &[],
        loops,
        local_gpr_pool(),
        &LOCAL_XMMS,
        &[],
    )
}

/// [`allocate_registers`] with the method's exception table modelled.
///
/// `handlers` is `(start_pc, end_pc, handler_pc)` per entry. Modelling them adds
/// the "protected code can branch to the handler" edges to the CFG the
/// interference graph is built from, so a local that only the handler reads
/// stays live across the protected range and cannot be given a register another
/// local is already using there.
///
/// Callers pass a non-empty slice only for methods compiled with precise
/// exceptional frames — the population whose handler genuinely reads
/// non-parameter locals AND whose handler frame is reconstructed from register
/// homes. Every other compile passes `&[]` and allocates byte-identically.
pub fn allocate_registers_with_handlers(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    param_slots: &[usize],
    loops: &[(usize, usize)],
    handlers: &[(usize, usize, usize)],
) -> RegAllocResult {
    allocate_registers_with(
        code,
        code_len,
        num_locals,
        num_params,
        param_slots,
        loops,
        local_gpr_pool(),
        &LOCAL_XMMS,
        handlers,
    )
}

/// Compute, for every bytecode PC, the set of locals live-IN at that
/// instruction (bit `i` set ⇒ local `i` may still be read before its next
/// definition). Unlike [`RegAllocResult::block_live_in`] (block-boundary
/// granularity only), this walks each basic block backward at *instruction*
/// granularity — the same per-instruction gen/kill walk [`build_interference`]
/// already does, just kept instead of discarded after building interference
/// edges — so a caller can ask "is local `i` still live at this exact bci"
/// rather than only "at this block's start".
///
/// Written for the OSR-exit deopt snapshot (`x64.rs`'s
/// `build_and_record_deopt_point`): a local whose machine location can't be
/// decoded at a snapshot bci (`FrameValue::Unsupported`) used to force the
/// WHOLE snapshot to reject, even when that local is provably dead there
/// (e.g. a loop induction variable read for the last time inside the loop,
/// snapshotted at a trap several instructions after the loop exits). A
/// rejected OSR-exit snapshot falls back to "continue interpreting the
/// pre-OSR-entry frame", which re-runs every loop iteration the OSR-compiled
/// code already executed — silently duplicating side effects (see
/// `testoutputbuffer-writespeed-content-length-mismatch-FIXED.md`).
/// Knowing a local is dead at the snapshot bci lets the caller substitute a
/// safe placeholder instead of rejecting outright.
///
/// Same 64-local cap as the rest of this module (bit `i` only meaningful for
/// `i < 64`); a caller must treat locals `>= 64` as conservatively live (as
/// they already do for `block_live_in`).
pub fn live_locals_per_pc(code: &[u8], code_len: usize, num_params: usize) -> Vec<u64> {
    live_locals_per_pc_with_coverage(code, code_len, num_params).0
}

/// [`live_locals_per_pc_with_coverage`] with EXCEPTION EDGES modelled.
///
/// `handlers` is `(start_pc, end_pc, handler_pc)` per exception-table entry.
/// Every instruction inside `[start_pc, end_pc)` can transfer control to
/// `handler_pc`, so each block overlapping that range gains the handler's
/// block as a successor and the handler's `live_in` flows back into the
/// protected code.
///
/// Without this, a local that ONLY the handler reads is computed dead at every
/// pc in the protected range — and that is precisely where the precise
/// exceptional-frame snapshot is taken, so the reconstructed handler frame
/// dropped the value it was about to read. With a reference local that also
/// unroots a live object: the collector reclaims it, the address is recycled,
/// and an unrelated object shows up in its place (observed as json-smart
/// re-parses returning a key `String`, and as
/// `ClassCastException: java.lang.Object cannot be cast to JSONArray`).
pub fn live_locals_per_pc_with_handlers(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    param_slots: &[usize],
    handlers: &[(usize, usize, usize)],
) -> (Vec<u64>, Vec<bool>) {
    live_locals_per_pc_inner(code, code_len, num_params, param_slots, handlers)
}

/// [`live_locals_per_pc`] plus the parallel *coverage* bitmap: `covered[pc]` is
/// `true` exactly when `pc` is an instruction start inside some basic block, so
/// `live_at[pc]` is a computed answer rather than the `0` default.
///
/// The distinction matters for any consumer that treats "nothing live" as
/// permission to drop state: an *uncovered* pc (code no CFG path reaches —
/// genuinely dead bytecode, or the interior of a mis-decoded region) also reads
/// as `0`, and acting on that would silently discard live values. Every such
/// consumer must fall back to its conservative answer instead.
fn live_locals_per_pc_with_coverage(
    code: &[u8],
    code_len: usize,
    num_params: usize,
) -> (Vec<u64>, Vec<bool>) {
    live_locals_per_pc_inner(code, code_len, num_params, &[], &[])
}

/// Build the CFG with EXCEPTION EDGES modelled: every handler pc becomes a
/// block leader, and every block overlapping a protected range gains that
/// range's handler block as a successor.
///
/// `handlers` is `(start_pc, end_pc, handler_pc)` per exception-table entry; an
/// empty slice yields exactly [`build_cfg`]'s result, so callers that do not
/// model handlers are unaffected.
///
/// Shared by the two analyses that must agree about handler liveness: the
/// per-pc liveness behind the precise exceptional-frame snapshot, and the
/// interference graph behind register allocation. When only the first modelled
/// them, the snapshot correctly said "local `i` is live in register `r`" while
/// the allocator had already handed `r` to someone else.
fn build_cfg_with_handlers(
    code: &[u8],
    code_len: usize,
    handlers: &[(usize, usize, usize)],
) -> Vec<BasicBlock> {
    let leaders: Vec<usize> = handlers.iter().map(|&(_, _, h)| h).collect();
    let mut blocks = build_cfg_with_leaders(code, code_len, &leaders);
    if !handlers.is_empty() {
        // Exception edges: protected block -> handler block. Collect first, so
        // the successor lists can be mutated without holding a borrow.
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for &(start, end, handler) in handlers {
            let Some(handler_idx) = blocks.iter().position(|b| b.start_pc == handler) else {
                continue;
            };
            for (i, block) in blocks.iter().enumerate() {
                if block.start_pc < end && block.end_pc > start {
                    edges.push((i, handler_idx));
                }
            }
        }
        for (from, to) in edges {
            if !blocks[from].successors.contains(&to) {
                blocks[from].successors.push(to);
            }
        }
    }
    blocks
}

/// [`live_locals_per_pc_with_handlers`] for **every** local slot, not only the
/// first 64.
///
/// Everything else in this module is capped at 64 locals because its `gen` /
/// `kill` / `live_in` / `live_out` sets are `u64`, and for REGISTER ALLOCATION
/// that cap costs nothing: a local above slot 63 simply never receives a
/// register and lives in its canonical frame slot, which is always correct.
///
/// It is not free for the deopt snapshot, which is the other consumer. There a
/// local the analysis cannot call DEAD must be described, and a slot the
/// whole-method kind classifier had to call [`LocalKind::Ambiguous`] — the
/// ordinary shape of javac reusing one slot for an `int` in one region and a
/// `double` in another — has no describable encoding, so it publishes
/// `FrameValue::Unsupported`. One such slot makes the whole frame unresumable,
/// and `osr_exit_policy` then refuses OSR ENTRY at every back edge of the
/// method. Measured on Apache Commons Math's `BOBYQAOptimizer`: `trsbox`
/// (slots 86/87/89) and `bobyqb` (slots 64/67/68) — every undescribable slot in
/// both methods above 63 and none below it, which is the shape of a truncated
/// mask rather than of a real analysis failure. Those slots are read a handful
/// of times each across a 3 000-byte method, so they are dead at almost every
/// bci a snapshot is taken at, and "dead" is an encoding the resume already has:
/// `FrameValue::Undefined`.
///
/// So this runs the SAME analysis once per 64-slot WINDOW and returns one row
/// of `words` bitsets per pc: `rows[pc * words + w]` covers slots
/// `[w * 64, w * 64 + 64)`. Window 0 is bit-for-bit what
/// [`live_locals_per_pc_with_handlers`] returns, so a method with 64 locals or
/// fewer is unchanged; the cost of the extra windows is one more fixpoint per
/// 64 slots, at compile time, for the small minority of methods that have them.
///
/// `covered` is shared across windows: it records instruction starts, which do
/// not depend on which slots are being tracked.
pub fn live_locals_per_pc_all(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    param_slots: &[usize],
    handlers: &[(usize, usize, usize)],
    num_locals: usize,
) -> (Vec<u64>, Vec<bool>, usize) {
    let words = num_locals.div_ceil(64).max(1);
    let mut blocks = build_cfg_with_handlers(code, code_len, handlers);
    let mut rows = vec![0u64; (code_len + 1).saturating_mul(words)];
    let mut covered = vec![false; code_len + 1];
    for w in 0..words {
        let base = w * 64;
        for block in &mut blocks {
            block.gen = 0;
            block.kill = 0;
            block.live_in = 0;
            block.live_out = 0;
            compute_gen_kill_windowed(code, block, base);
        }
        solve_liveness(
            &mut blocks,
            param_live_in_mask_windowed(num_params, param_slots, base),
        );
        let handler_ranges = handler_live_in_ranges(&blocks, handlers);
        for block in &blocks {
            let mut pcs = Vec::new();
            {
                let mut pc = block.start_pc;
                while pc < block.end_pc {
                    pcs.push(pc);
                    pc += bc_len(code, pc);
                }
            }
            let mut live = block.live_out;
            for &pc in pcs.iter().rev() {
                if let Some((idx, is_use, is_def)) = local_access(code, pc) {
                    if let Some(bit_no) = idx.checked_sub(base).filter(|b| *b < 64) {
                        let bit = 1u64 << bit_no;
                        if is_def && !is_use {
                            live &= !bit;
                        }
                        if is_use {
                            live |= bit;
                        }
                    }
                }
                rows[pc * words + w] = live | handler_live_mask(pc, &handler_ranges);
                covered[pc] = true;
            }
        }
    }
    (rows, covered, words)
}

/// [`compute_gen_kill`] for the 64-slot window starting at `base`.
///
/// `base == 0` is [`compute_gen_kill`] exactly; the window form exists for
/// [`live_locals_per_pc_all`], which is the only analysis in this module that
/// needs to see a local above slot 63.
fn compute_gen_kill_windowed(code: &[u8], block: &mut BasicBlock, base: usize) {
    let mut pc = block.start_pc;
    while pc < block.end_pc {
        if let Some((idx, is_use, is_def)) = local_access(code, pc) {
            if let Some(bit_no) = idx.checked_sub(base).filter(|b| *b < 64) {
                let bit = 1u64 << bit_no;
                // For iinc: use comes before def
                if is_use && (block.kill & bit) == 0 {
                    block.gen |= bit;
                }
                if is_def {
                    block.kill |= bit;
                }
            }
        }
        pc += bc_len(code, pc);
    }
}

/// [`param_live_in_mask`] for the 64-slot window starting at `base`.
fn param_live_in_mask_windowed(num_params: usize, param_slots: &[usize], base: usize) -> u64 {
    if param_slots.is_empty() {
        // No explicit slot list: parameters occupy slots `0..num_params`.
        let mut mask = 0u64;
        for slot in base..base.saturating_add(64) {
            if slot < num_params {
                mask |= 1u64 << (slot - base);
            }
        }
        return mask;
    }
    param_slots
        .iter()
        .filter_map(|&slot| slot.checked_sub(base).filter(|b| *b < 64))
        .fold(0u64, |mask, bit_no| mask | (1u64 << bit_no))
}

fn live_locals_per_pc_inner(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    param_slots: &[usize],
    handlers: &[(usize, usize, usize)],
) -> (Vec<u64>, Vec<bool>) {
    let mut blocks = build_cfg_with_handlers(code, code_len, handlers);
    for block in &mut blocks {
        compute_gen_kill(code, block);
    }
    solve_liveness(&mut blocks, param_live_in_mask(num_params, param_slots));

    let handler_ranges = handler_live_in_ranges(&blocks, handlers);

    let mut live_at = vec![0u64; code_len + 1];
    let mut covered = vec![false; code_len + 1];
    for block in &blocks {
        let mut pcs = Vec::new();
        {
            let mut pc = block.start_pc;
            while pc < block.end_pc {
                pcs.push(pc);
                pc += bc_len(code, pc);
            }
        }

        // Backward walk, exactly mirroring `build_interference`'s per-instruction
        // update, but recording the live-in set at every pc instead of only using
        // it to derive interference edges.
        let mut live = block.live_out;
        for &pc in pcs.iter().rev() {
            if let Some((idx, is_use, is_def)) = local_access(code, pc) {
                if idx < 64 {
                    let bit = 1u64 << idx;
                    if is_def && !is_use {
                        live &= !bit;
                    }
                    if is_use {
                        live |= bit;
                    }
                }
            }
            // Union the covering handlers' live-in: an exception thrown at this
            // pc enters the handler with the locals as they are HERE, so a later
            // def in this block must not be allowed to kill them. See
            // `handler_live_mask` for the witness this fixes.
            live_at[pc] = live | handler_live_mask(pc, &handler_ranges);
            covered[pc] = true;
        }
    }
    (live_at, covered)
}

/// Run register allocation for a method (ARM64).
pub fn allocate_registers_arm64(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_arm64_with_param_slots(code, code_len, num_locals, num_params, &[], loops)
}

/// [`allocate_registers_arm64`] with the real argument layout.
///
/// `param_slots` is the JVM local slot of each incoming argument
/// (`compute_param_jvm_slots`); an empty slice selects the identity layout
/// `0..num_params`. The ARM64 backend used to pass `&[]` unconditionally while
/// its prologue also assumed the identity layout, so the two agreed -- and were
/// both wrong for every signature with a `long`/`double` before its last
/// parameter. See [`param_live_in_mask`] for what the wrong seed costs.
pub fn allocate_registers_arm64_with_param_slots(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    param_slots: &[usize],
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_with(
        code,
        code_len,
        num_locals,
        num_params,
        param_slots,
        loops,
        &ARM64_LOCAL_GPRS,
        &ARM64_LOCAL_FPS,
        &[],
    )
}

/// Platform-generic register allocation entry point.
#[allow(clippy::too_many_arguments)]
fn allocate_registers_with(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    param_slots: &[usize],
    loops: &[(usize, usize)],
    gpr_regs: &[u8],
    fp_regs: &[u8],
    handlers: &[(usize, usize, usize)],
) -> RegAllocResult {
    let available = gpr_regs;

    // Bail out for trivial cases: use fixed mapping
    if num_locals == 0 {
        return RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
    }

    // Detect float/double locals — these use XMM registers, not GPRs
    let float_mask = find_float_locals(code, code_len, num_locals);

    // Slots javac reused across type categories — an `int` loop counter and,
    // once its scope ends, a `double`. Such a slot must get NO register home:
    // the two categories read different places (`iload` the frame slot,
    // `dload` the XMM), and the OSR trampoline only seeds one of them. See
    // [`find_non_float_locals`] for the measured failure.
    let dual_category_mask = float_mask & find_non_float_locals(code, code_len);

    // Build CFG and solve liveness. `handlers` is empty for every compile that
    // does not reconstruct a handler frame from register homes, in which case
    // this is exactly `build_cfg`.
    let mut blocks = build_cfg_with_handlers(code, code_len, handlers);
    for block in &mut blocks {
        compute_gen_kill(code, block);
    }
    solve_liveness(&mut blocks, param_live_in_mask(num_params, param_slots));

    // Build full interference graph. The handler ranges keep a local that only
    // the catch block reads from sharing a register with something defined
    // inside the protected range — the interference-graph half of the same fix
    // applied to the liveness map in `live_locals_per_pc_inner`.
    let handler_ranges = handler_live_in_ranges(&blocks, handlers);
    let interference = build_interference(code, &blocks, num_locals, &handler_ranges);

    // Count uses (loop-weighted)
    let use_counts = count_uses(code, code_len, num_locals, loops);

    // -----------------------------------------------------------------------
    // GPR pass: color non-float locals using callee-saved GPR registers.
    // Filter the interference graph to exclude float locals entirely
    // (they will be allocated to XMMs in the separate pass below).
    // -----------------------------------------------------------------------
    let mut gpr_interference = interference.clone();
    let n = num_locals.min(64);
    for i in 0..n {
        if float_mask & (1u64 << i) != 0 {
            // Remove this float local from all GPR interference entries
            gpr_interference[i] = 0;
            for j in 0..num_locals {
                gpr_interference[j] &= !(1u64 << i);
            }
        }
    }
    // Zero use counts for float locals so they never get GPR colors
    let mut gpr_use_counts = use_counts.clone();
    for i in 0..n {
        if float_mask & (1u64 << i) != 0 {
            gpr_use_counts[i] = 0;
        }
    }
    let mut assignments = color_graph(&gpr_interference, num_locals, available, &gpr_use_counts);
    // Ensure float locals never end up with a GPR (belt-and-suspenders)
    for i in 0..n {
        if float_mask & (1u64 << i) != 0 {
            assignments[i] = None;
        }
    }

    // -----------------------------------------------------------------------
    // XMM pass: color float/double locals using XMM registers.
    // Only float-to-float interference matters (GPR vs XMM are separate files).
    // -----------------------------------------------------------------------
    let xmm_available = fp_regs;
    let mut xmm_interference = vec![0u64; num_locals];
    for i in 0..n {
        if float_mask & (1u64 << i) != 0 {
            // Keep only float-float interference
            xmm_interference[i] = interference[i] & float_mask;
        }
    }
    let xmm_raw = color_graph(&xmm_interference, num_locals, xmm_available, &use_counts);
    // Only propagate XMM assignments for float/double locals — and never for a
    // slot javac also uses as an int/long/reference. The GPR pass above already
    // refused it (it is float-masked), so leaving the XMM home off too puts the
    // slot wholly in its canonical frame slot, which both categories read and
    // which the OSR trampoline seeds. See `dual_category_mask`.
    let mut xmm_assignments = vec![None; num_locals];
    for i in 0..n {
        if float_mask & (1u64 << i) != 0 && dual_category_mask & (1u64 << i) == 0 {
            xmm_assignments[i] = xmm_raw[i];
        }
    }

    // -----------------------------------------------------------------------
    // Collect used callee-saved GPR and XMM registers
    // -----------------------------------------------------------------------
    let mut gpr_used_set = 0u64;
    for &a in &assignments {
        if let Some(reg) = a {
            if let Some(pos) = available.iter().position(|&r| r == reg) {
                gpr_used_set |= 1u64 << pos;
            }
        }
    }
    let used_callee_saved: Vec<u8> = available
        .iter()
        .enumerate()
        .filter(|(i, _)| gpr_used_set & (1u64 << i) != 0)
        .map(|(_, &r)| r)
        .collect();

    let mut xmm_used_set = 0u64;
    for &a in &xmm_assignments {
        if let Some(xmm) = a {
            if let Some(pos) = xmm_available.iter().position(|&r| r == xmm) {
                xmm_used_set |= 1u64 << pos;
            }
        }
    }
    let used_xmm_regs: Vec<u8> = xmm_available
        .iter()
        .enumerate()
        .filter(|(i, _)| xmm_used_set & (1u64 << i) != 0)
        .map(|(_, &r)| r)
        .collect();

    // -----------------------------------------------------------------------
    // T1.1.22-25 — structural invariants on the coloring output.
    //
    // Catches three categories of bugs that historically manifested as
    // "hash-table loop miscompile" or "interface-default parameter
    // misassignment" (see NEW-1.3/1.4 in docs/roadmap.md):
    //
    //   1. GPR × XMM conflict — a single local must not be assigned
    //      both a GPR and an XMM register. This would mean the load
    //      path reads one register and the store path writes the
    //      other, silently corrupting subsequent reads.
    //
    //   2. Float/non-float category misassignment — a local whose
    //      type tag is float must land in `xmm_assignments` (not
    //      `assignments`) and vice versa.
    //
    //   3. Interference violation — two interfering locals must not
    //      share the same physical register. A graph-coloring bug
    //      here causes one to clobber the other across a safepoint.
    //
    // When any invariant fails we return an empty (fallback) result
    // so the JIT bails to the interpreter instead of producing a
    // miscompile. Each failure is traced at `warn` level so the
    // regression shows up in CI output.
    if !regalloc_invariants_hold(
        &assignments,
        &xmm_assignments,
        &interference,
        float_mask,
        num_locals,
    ) {
        // Log to stderr — the jit crate intentionally avoids a
        // tracing dependency so the logger falls back to a plain
        // eprintln! under a debug_assertions gate (keeps release
        // builds quiet).
        #[cfg(debug_assertions)]
        eprintln!(
            "regalloc: structural invariants failed for method \
             (num_locals={num_locals}, num_params={num_params}); \
             bailing to interpreter"
        );
        return RegAllocResult {
            assignments: vec![None; num_locals],
            xmm_assignments: vec![None; num_locals],
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
    }

    let block_live_in: Vec<(usize, u64)> = blocks.iter().map(|b| (b.start_pc, b.live_in)).collect();
    RegAllocResult {
        assignments,
        xmm_assignments,
        used_callee_saved,
        used_xmm_regs,
        block_live_in,
    }
}

/// T1.1.22-25 — validate the graph-coloring output against three
/// structural invariants (GPR/XMM non-overlap, category match,
/// interference respected). Returns `true` iff all invariants hold.
///
/// Extracted so the check is both called from
/// [`allocate_registers_with`] and exercised by a dedicated unit
/// test battery below.
pub fn regalloc_invariants_hold(
    gpr_assignments: &[Option<u8>],
    xmm_assignments: &[Option<u8>],
    interference: &[u64],
    float_mask: u64,
    num_locals: usize,
) -> bool {
    let n = num_locals.min(64);
    // Invariant 1: GPR/XMM non-overlap.
    for i in 0..num_locals {
        if gpr_assignments[i].is_some() && xmm_assignments[i].is_some() {
            return false;
        }
    }
    // Invariant 2: category match.
    for i in 0..n {
        let is_float = float_mask & (1u64 << i) != 0;
        if is_float && gpr_assignments[i].is_some() {
            return false;
        }
        if !is_float && xmm_assignments[i].is_some() {
            return false;
        }
    }
    // Invariant 3: interference respected within the GPR pool.
    for i in 0..n {
        let gi = match gpr_assignments[i] {
            Some(r) => r,
            None => continue,
        };
        let mask = interference[i];
        for j in (i + 1)..n {
            if mask & (1u64 << j) == 0 {
                continue;
            }
            if gpr_assignments[j] == Some(gi) {
                return false;
            }
        }
    }
    // Invariant 3b: interference respected within the XMM pool.
    for i in 0..n {
        let xi = match xmm_assignments[i] {
            Some(r) => r,
            None => continue,
        };
        let mask = interference[i];
        for j in (i + 1)..n {
            if mask & (1u64 << j) == 0 {
                continue;
            }
            if xmm_assignments[j] == Some(xi) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_local_read_in_a_handler_is_decoded() {
        // wide iload 5; ireturn -- slot 5 is not safe on entry.
        let code = [0xc4, 0x15, 0x00, 0x05, 0xac];
        assert!(handler_has_unsafe_local_read(&code, code.len(), 0, 0));
        assert!(!handler_has_unsafe_local_read(&code, code.len(), 0, 1 << 5));
        // wide istore 5; wide iinc 5 1; wide iload 5; ireturn -- the store
        // makes the later reads safe.
        let code = [
            0x03, 0xc4, 0x36, 0x00, 0x05, 0xc4, 0x84, 0x00, 0x05, 0x00, 0x01, 0xc4, 0x15, 0x00,
            0x05, 0xac,
        ];
        assert!(!handler_has_unsafe_local_read(&code, code.len(), 0, 0));
        // wide iinc 70 1 reads a slot the mask cannot name.
        let code = [0xc4, 0x84, 0x00, 70, 0x00, 0x01, 0xb1];
        assert!(handler_has_unsafe_local_read(&code, code.len(), 0, u64::MAX));
    }

    // T1.1.22-25 — invariant checker unit tests.
    //
    // These exercise `regalloc_invariants_hold` directly so any
    // future change to the graph-coloring allocator is immediately
    // cross-checked against the three structural invariants.

    #[test]
    fn invariants_accept_clean_assignment() {
        // 2 non-interfering GPR locals in different registers.
        let gpr = vec![Some(12), Some(13), None];
        let xmm = vec![None, None, Some(8)];
        let interference = vec![0u64, 0, 0];
        let float_mask = 0b100; // local 2 is float
        assert!(regalloc_invariants_hold(
            &gpr,
            &xmm,
            &interference,
            float_mask,
            3
        ));
    }

    #[test]
    fn invariants_reject_gpr_xmm_overlap() {
        let gpr = vec![Some(12)];
        let xmm = vec![Some(8)]; // same local in both!
        let interference = vec![0u64];
        assert!(!regalloc_invariants_hold(&gpr, &xmm, &interference, 0, 1));
    }

    #[test]
    fn invariants_reject_float_in_gpr() {
        let gpr = vec![Some(12)];
        let xmm = vec![None];
        let interference = vec![0u64];
        let float_mask = 0b1; // local 0 is float but assigned a GPR
        assert!(!regalloc_invariants_hold(
            &gpr,
            &xmm,
            &interference,
            float_mask,
            1
        ));
    }

    #[test]
    fn invariants_reject_int_in_xmm() {
        let gpr = vec![None];
        let xmm = vec![Some(8)];
        let interference = vec![0u64];
        let float_mask = 0; // local 0 is int but assigned an XMM
        assert!(!regalloc_invariants_hold(
            &gpr,
            &xmm,
            &interference,
            float_mask,
            1
        ));
    }

    // CM-FASTMATH — `ldc`/`ldc_w`/`ldc2_w` were missing from `bc_len` (the
    // x64.rs `bytecode_len_at` twin had them; this copy didn't), so liveness
    // walks read constant-pool index operand bytes as opcodes. Pin the
    // constant-load lengths so the tables cannot drift apart again.
    #[test]
    fn bc_len_constant_loads() {
        // opcode byte + dummy operand bytes; bc_len only looks at code[pc]
        // for these arms.
        assert_eq!(bc_len(&[0x12, 0xBB], 0), 2, "ldc");
        assert_eq!(bc_len(&[0x13, 0x00, 0xBB], 0), 3, "ldc_w");
        assert_eq!(bc_len(&[0x14, 0x00, 0xBB], 0), 3, "ldc2_w");
        assert_eq!(bc_len(&[0x10, 0x7F], 0), 2, "bipush");
        assert_eq!(bc_len(&[0x11, 0x12, 0x34], 0), 3, "sipush");
        assert_eq!(bc_len(&[0xa8, 0x00, 0x10], 0), 3, "jsr");
        assert_eq!(bc_len(&[0xa9, 0x04], 0), 2, "ret");
    }

    // Defense-in-depth (same class as the missing-`ldc` CM-FASTMATH bug): the
    // 5-byte instructions invokeinterface (0xb9), invokedynamic (0xba), goto_w
    // (0xc8) and jsr_w (0xc9). Only invokeinterface is reachable today (the
    // other three are rejected by `jit_scan`), but a missing length entry
    // under-counts the instruction by 4 bytes and desyncs every PC-stepping
    // walk — so all four must read 5, and must match the x64.rs
    // `bytecode_len_at` twin. Operand bytes are dummies; `bc_len` only reads
    // `code[pc]` for these arms.
    #[test]
    fn bc_len_five_byte_ops() {
        assert_eq!(
            bc_len(&[0xb9, 0x00, 0x10, 0x02, 0x00], 0),
            5,
            "invokeinterface"
        );
        assert_eq!(
            bc_len(&[0xba, 0x00, 0x10, 0x00, 0x00], 0),
            5,
            "invokedynamic"
        );
        assert_eq!(bc_len(&[0xc8, 0x00, 0x00, 0x00, 0x10], 0), 5, "goto_w");
        assert_eq!(bc_len(&[0xc9, 0x00, 0x00, 0x00, 0x10], 0), 5, "jsr_w");
    }

    /// `live_locals_per_pc_all` must be a strict SUPERSET of what the 64-local
    /// analysis could answer: window 0 identical, and a window above it that
    /// the old code could not express at all.
    ///
    /// Two assertions, and only the second one can fail without the change —
    /// the first would pass against the old code too, which is exactly why it
    /// is not on its own.
    #[test]
    fn live_locals_per_pc_all_answers_above_slot_63() {
        // iload 70; istore 70; iload 70; return
        //  0: 0x15 0x46   iload 70
        //  2: 0x36 0x46   istore 70
        //  4: 0x15 0x46   iload 70
        //  6: 0xb1        return
        let code = [0x15, 0x46, 0x36, 0x46, 0x15, 0x46, 0xb1];
        let code_len = code.len();
        let (rows, covered, words) = live_locals_per_pc_all(&code, code_len, 0, &[], &[], 71);
        assert_eq!(words, 2, "71 locals needs two 64-slot windows");
        assert!(covered[0] && covered[2] && covered[4]);

        // Window 0 is bit-for-bit the pre-existing answer, so a method with 64
        // locals or fewer cannot have changed.
        let (old_rows, old_covered) =
            live_locals_per_pc_with_handlers(&code, code_len, 0, &[], &[]);
        for pc in 0..=code_len {
            assert_eq!(
                rows[pc * words],
                old_rows[pc],
                "window 0 must equal the 64-local analysis at pc {pc}"
            );
            assert_eq!(covered[pc], old_covered[pc], "coverage differs at pc {pc}");
        }

        // Slot 70 lives in window 1, bit 6. It is USED at pc 0 and pc 4 and
        // DEFINED at pc 2, so it is live at 0 (a use), dead at 2 (a def whose
        // old value nothing reads) and live again at 4. The old analysis could
        // express none of this: every row was a single `u64` and slot 70 had
        // no bit at all, so the snapshot had to assume it live everywhere.
        let bit = 1u64 << (70 - 64);
        assert_ne!(rows[0 * words + 1] & bit, 0, "slot 70 is live at its use");
        assert_eq!(
            rows[2 * words + 1] & bit,
            0,
            "slot 70 is dead at the store that overwrites it"
        );
        assert_ne!(
            rows[4 * words + 1] & bit,
            0,
            "slot 70 is live at its second use"
        );
    }

    /// The window form of the parameter seed must place a parameter slot in the
    /// window that owns it, and nowhere else.
    #[test]
    fn param_live_in_mask_windowed_places_a_high_param_in_its_own_window() {
        assert_eq!(param_live_in_mask_windowed(0, &[70], 0), 0);
        assert_eq!(param_live_in_mask_windowed(0, &[70], 64), 1u64 << 6);
        // The `num_params`-only form (no explicit slot list) windows the same way.
        assert_eq!(param_live_in_mask_windowed(3, &[], 0), 0b111);
        assert_eq!(param_live_in_mask_windowed(3, &[], 64), 0);
        assert_eq!(param_live_in_mask_windowed(66, &[], 64), 0b11);
    }

    #[test]
    fn local_access_decodes_wide_locals() {
        assert_eq!(
            local_access(&[0xc4, 0x15, 0x00, 0x3f], 0),
            Some((63, true, false)),
            "wide iload"
        );
        assert_eq!(
            local_access(&[0xc4, 0x39, 0x00, 0x3e], 0),
            Some((62, false, true)),
            "wide dstore"
        );
        assert_eq!(
            local_access(&[0xc4, 0x84, 0x00, 0x05, 0x00, 0x01], 0),
            Some((5, true, true)),
            "wide iinc"
        );
        assert_eq!(
            local_access(&[0xc4, 0xa9, 0x00, 0x05], 0),
            None,
            "wide ret is not a Java-value local access for register allocation"
        );
        assert_eq!(local_access(&[0x15], 0), None, "truncated iload");
    }

    #[test]
    fn find_float_locals_decodes_wide_float_and_double_locals() {
        #[rustfmt::skip]
        let code = [
            0xc4, 0x17, 0x00, 0x05, // wide fload 5
            0xc4, 0x39, 0x00, 0x06, // wide dstore 6
            0xaf,                   // dreturn
        ];
        let mask = find_float_locals(&code, code.len(), 7);
        assert_ne!(
            mask & (1u64 << 5),
            0,
            "wide fload local should be float-classified"
        );
        assert_ne!(
            mask & (1u64 << 6),
            0,
            "wide dstore local should be float-classified"
        );
    }

    #[test]
    fn find_non_float_locals_covers_every_gpr_category_encoding() {
        #[rustfmt::skip]
        let code = [
            0x1a,                   // iload_0
            0x1f,                   // lload_1
            0x2c,                   // aload_2
            0x3e,                   // istore_3
            0x15, 0x04,             // iload 4
            0x37, 0x05,             // lstore 5
            0x3a, 0x06,             // astore 6
            0x84, 0x07, 0x01,       // iinc 7, 1
            0xc4, 0x15, 0x00, 0x08, // wide iload 8
            0xc4, 0x84, 0x00, 0x09, 0x00, 0x01, // wide iinc 9, 1
            0xb1,                   // return
        ];
        let mask = find_non_float_locals(&code, code.len());
        for slot in 0..=9u32 {
            assert_ne!(
                mask & (1u64 << slot),
                0,
                "slot {slot} is accessed in the GPR category and must be flagged"
            );
        }
        assert_eq!(mask & (1u64 << 10), 0, "untouched slot must stay clear");
    }

    /// A slot javac reuses across type categories (an `int` loop counter, then
    /// a `double` once the counter's scope ends) must get NEITHER a GPR nor an
    /// XMM home: `iload` would read the frame slot while `dload` read the XMM,
    /// and the OSR trampoline seeds only the register. See
    /// `find_non_float_locals` for the measured miscompile.
    #[test]
    fn dual_category_slot_gets_no_register_home() {
        #[rustfmt::skip]
        let code = [
            0x0e,                   // dconst_0
            0x39, 0x00,             // dstore 0        (double sum -> slot 0)
            0x03,                   // iconst_0
            0x36, 0x02,             // istore 2        (int i    -> slot 2)
            // loop head @6
            0x15, 0x02,             // iload 2
            0x10, 0x64,             // bipush 100
            0xa2, 0x00, 0x0a,       // if_icmpge +10 -> 20
            0x84, 0x02, 0x01,       // iinc 2, 1
            0xa7, 0xff, 0xf9,       // goto -7 -> 6
            // @20: slot 2 is reused for a double
            0x18, 0x00,             // dload 0
            0x39, 0x02,             // dstore 2        (double tail -> slot 2)
            0x18, 0x02,             // dload 2
            0xaf,                   // dreturn
        ];
        let r = allocate_registers(&code, code.len(), 4, 0, &[]);
        assert_eq!(
            r.assignments[2], None,
            "a dual-category slot must not get a GPR home"
        );
        assert_eq!(
            r.xmm_assignments[2], None,
            "a dual-category slot must not get an XMM home either -- the OSR \
             trampoline would seed the XMM and leave the frame slot the int \
             accesses read uninitialised"
        );
        // The control: slot 0 is only ever a double, so it keeps its XMM home.
        assert!(
            r.xmm_assignments[0].is_some(),
            "a single-category double local must still be XMM-homed"
        );
    }

    #[test]
    fn branch_target_decodes_wide_branches() {
        let goto_w = [0xc8, 0x00, 0x00, 0x00, 0x05, 0xb1];
        let jsr_w = [0xc9, 0x00, 0x00, 0x00, 0x05, 0xb1];
        assert_eq!(branch_target(&goto_w, 0), Some(5));
        assert_eq!(branch_target(&jsr_w, 0), Some(5));
        assert!(
            is_unconditional(0xc8),
            "goto_w is terminal for CFG fallthrough"
        );
        assert!(
            !is_unconditional(0xc9),
            "jsr_w still has a fallthrough return-address path"
        );
    }

    #[test]
    fn build_cfg_goto_w_has_target_successor_without_fallthrough() {
        // goto_w +6 -> return at pc 6. The byte at pc 5 is dead fallthrough.
        let code = [0xc8, 0x00, 0x00, 0x00, 0x06, 0x03, 0xb1];
        let blocks = build_cfg(&code, code.len());
        let entry = blocks
            .iter()
            .find(|block| block.start_pc == 0)
            .expect("entry block");
        assert_eq!(
            entry.successors.len(),
            1,
            "goto_w should not add fallthrough"
        );
        let succ = &blocks[entry.successors[0]];
        assert_eq!(succ.start_pc, 6);
    }

    // CM-FASTMATH end-to-end regression: the exact bytecode shape of
    // commons-math3 `FastMath.polySine(D)D` as compiled into the published
    // 3.6.1 jar, where the `ldc2_w` constant-pool indices are large enough
    // that their operand bytes decode as multi-byte opcodes / `athrow`
    // (0xBB `new`, 0xBF `athrow`, ...). With the broken length table the
    // liveness walk hit a phantom block terminator inside an operand, saw no
    // further uses of local 0 (`x`), and coalesced it with local 2 (`x2`)
    // onto one XMM register — so `p * x2 * x` compiled as `p * x2 * x2`
    // (the FastMath.sin(3π/4)=1.2252 transform-suite miscompile). Locals 0
    // and 2 interfere (both live at pc 41-43) and must never share a register.
    #[test]
    fn polysine_high_cp_indices_do_not_coalesce_live_doubles() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x26,             // 0:  dload_0        x
            0x26,             // 1:  dload_0        x
            0x6b,             // 2:  dmul           x*x
            0x49,             // 3:  dstore_2       x2 =
            0x14, 0x00, 0xBB, // 4:  ldc2_w #187    (0xBB = `new` if misread)
            0x39, 0x04,       // 7:  dstore 4       p =
            0x18, 0x04,       // 9:  dload 4
            0x28,             // 11: dload_2
            0x6b,             // 12: dmul
            0x14, 0x00, 0xBD, // 13: ldc2_w #189    (0xBD = `anewarray`)
            0x63,             // 16: dadd
            0x39, 0x04,       // 17: dstore 4
            0x18, 0x04,       // 19: dload 4
            0x28,             // 21: dload_2
            0x6b,             // 22: dmul
            0x14, 0x00, 0xBF, // 23: ldc2_w #191    (0xBF = `athrow`!)
            0x63,             // 26: dadd
            0x39, 0x04,       // 27: dstore 4
            0x18, 0x04,       // 29: dload 4
            0x28,             // 31: dload_2
            0x6b,             // 32: dmul
            0x14, 0x00, 0xC1, // 33: ldc2_w #193    (0xC1 = `instanceof`)
            0x63,             // 36: dadd
            0x39, 0x04,       // 37: dstore 4
            0x18, 0x04,       // 39: dload 4
            0x28,             // 41: dload_2        x2 — still live
            0x6b,             // 42: dmul
            0x26,             // 43: dload_0        x  — still live!
            0x6b,             // 44: dmul
            0x39, 0x04,       // 45: dstore 4
            0x18, 0x04,       // 47: dload 4
            0xaf,             // 49: dreturn
        ];
        let result = allocate_registers(&code, code.len(), 6, 2, &[]);
        let x = result.xmm_assignments[0];
        let x2 = result.xmm_assignments[2];
        if let (Some(rx), Some(rx2)) = (x, x2) {
            assert_ne!(
                rx, rx2,
                "locals 0 (x) and 2 (x2) are simultaneously live across pc 41-44 \
                 and must not share an XMM register"
            );
        }
    }

    #[test]
    fn invariants_reject_interfering_locals_in_same_gpr() {
        let gpr = vec![Some(12), Some(12)]; // same reg, interfere
        let xmm = vec![None, None];
        let interference = vec![0b10, 0b01]; // mutual interference
        assert!(!regalloc_invariants_hold(&gpr, &xmm, &interference, 0, 2));
    }

    #[test]
    fn invariants_accept_non_interfering_same_gpr() {
        // Two locals sharing a register is fine iff they don't interfere.
        let gpr = vec![Some(12), Some(12)];
        let xmm = vec![None, None];
        let interference = vec![0u64, 0]; // no interference
        assert!(regalloc_invariants_hold(&gpr, &xmm, &interference, 0, 2));
    }

    #[test]
    fn invariants_reject_interfering_locals_in_same_xmm() {
        let gpr = vec![None, None];
        let xmm = vec![Some(8), Some(8)];
        let interference = vec![0b10, 0b01];
        let float_mask = 0b11;
        assert!(!regalloc_invariants_hold(
            &gpr,
            &xmm,
            &interference,
            float_mask,
            2
        ));
    }

    #[test]
    fn trivial_method_passes_invariants() {
        // iload_0; ireturn; + padding
        let code = [0x1a, 0xac, 0, 0];
        let result = allocate_registers(&code, 2, 1, 1, &[]);
        // The trivial method should not trip the invariant guard.
        assert!(result.assignments.iter().any(|a| a.is_some()));
    }

    #[test]
    fn test_trivial_method() {
        // iload_0; ireturn; + padding
        let code = [0x1a, 0xac, 0, 0];
        let result = allocate_registers(&code, 2, 1, 1, &[]);
        assert_eq!(result.assignments.len(), 1);
        assert!(
            result.assignments[0].is_some(),
            "param 0 should get a register"
        );
    }

    #[test]
    fn test_two_non_interfering_locals() {
        // Two locals used in sequence (no overlap):
        // iload_0; istore_2; iload_1; istore_3; iload_2; iload_3; iadd; ireturn
        let code = [
            0x1a, // iload_0
            0x3d, // istore_2
            0x1b, // iload_1
            0x3e, // istore_3
            0x1c, // iload_2
            0x1d, // iload_3
            0x60, // iadd
            0xac, // ireturn
            0, 0, // padding
        ];
        let result = allocate_registers(&code, 8, 4, 2, &[]);
        assert_eq!(result.assignments.len(), 4);
        // All 4 locals should get registers since K >= 4
        for i in 0..4 {
            assert!(
                result.assignments[i].is_some(),
                "local {i} should get a register"
            );
        }
    }

    #[test]
    fn test_interference_detected() {
        // Both locals used simultaneously:
        // iload_0; iload_1; iadd; ireturn
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let result = allocate_registers(&code, 4, 2, 2, &[]);
        // Both should get registers but DIFFERENT ones
        assert!(result.assignments[0].is_some());
        assert!(result.assignments[1].is_some());
        assert_ne!(
            result.assignments[0], result.assignments[1],
            "interfering locals must get different registers"
        );
    }

    #[test]
    fn test_loop_uses_weighted() {
        // Simple loop: local 2 used in loop body gets higher weight
        let code = [
            0x1a, // 0: iload_0
            0x3d, // 1: istore_2
            0x1c, // 2: iload_2 (loop body start)
            0x04, // 3: iconst_1
            0x60, // 4: iadd
            0x3d, // 5: istore_2
            0xa7, 0xff, 0xfb, // 6: goto -5 → 2
            0x1c, // 9: iload_2
            0xac, // 10: ireturn
            0, 0, // padding
        ];
        let loops = vec![(2, 6)]; // loop from PC 2 to PC 6
        let counts = count_uses(&code, 11, 3, &loops);
        // local 0: 1 use outside loop
        assert_eq!(counts[0], 1);
        // local 2: used in loop (iload_2 @ 2, istore_2 @ 5) = 2 * 10 = 20, plus outside (iload_0→istore_2 @1, iload_2 @9) = 2
        assert!(
            counts[2] >= 20,
            "loop uses should be weighted higher: got {}",
            counts[2]
        );
    }

    #[test]
    fn test_empty_method() {
        let code = [0xb1, 0, 0]; // return; padding
        let result = allocate_registers(&code, 1, 0, 0, &[]);
        assert!(result.assignments.is_empty());
        assert!(result.used_callee_saved.is_empty());
    }

    #[test]
    fn test_many_locals_spill() {
        // More locals than registers — some must spill
        let k = LOCAL_REGS.len();
        // Create bytecode that uses k+2 locals simultaneously
        let mut code = Vec::new();
        for i in 0..=(k + 1) {
            code.push(0x15); // iload
            code.push(i as u8);
        }
        code.push(0xac); // ireturn
        code.push(0);
        code.push(0);

        let code_len = code.len() - 2;
        let num_locals = k + 2;
        let result = allocate_registers(&code, code_len, num_locals, num_locals, &[]);

        // At most K locals should get registers
        let assigned_count = result.assignments.iter().filter(|a| a.is_some()).count();
        assert!(
            assigned_count <= k,
            "should assign at most K={k} registers, got {assigned_count}"
        );
        // At least some should spill
        let spill_count = result.assignments.iter().filter(|a| a.is_none()).count();
        assert!(spill_count >= 2, "should have at least 2 spills");
    }

    #[test]
    fn test_build_cfg_linear() {
        // Linear bytecode: iload_0; iconst_1; iadd; ireturn
        let code = [0x1a, 0x04, 0x60, 0xac, 0, 0];
        let blocks = build_cfg(&code, 4);
        assert_eq!(blocks.len(), 1, "linear code = 1 block");
        assert_eq!(blocks[0].start_pc, 0);
        assert_eq!(blocks[0].end_pc, 4);
    }

    #[test]
    fn test_liveness_iteration_limit() {
        // Just verify that solve_liveness terminates even with loops.
        let code = [
            0x1a, // 0: iload_0
            0xa7, 0xff, 0xfe, // 1: goto -2 → 0 (infinite loop)
            0xac, // 4: ireturn (unreachable)
            0, 0,
        ];
        let mut blocks = build_cfg(&code, 5);
        for block in &mut blocks {
            compute_gen_kill(&code, block);
        }
        // Should terminate without hanging due to MAX_LIVENESS_ITERATIONS.
        solve_liveness(&mut blocks, 1);
    }

    #[test]
    fn test_loop_weight_validation() {
        // Loop with header > back_edge (invalid) should not apply weight.
        let code = [0x1a, 0xac, 0, 0];
        let invalid_loops = vec![(5, 2)]; // header > back_edge
        let counts = count_uses(&code, 2, 1, &invalid_loops);
        assert_eq!(counts[0], 1, "invalid loop range should not apply weight");
    }

    #[test]
    fn test_branch_target_negative_returns_none() {
        // Branch at pc=0 with a large negative offset should return None.
        let code = [0xa7, 0x80, 0x00]; // goto with offset -32768
        assert!(branch_target(&code, 0).is_none());
    }

    #[test]
    fn test_build_cfg_branch() {
        // iload_0; ifne +3; iconst_0; ireturn; iconst_1; ireturn
        let code = [
            0x1a, // 0: iload_0
            0x9a, 0x00, 0x05, // 1: ifne +5 → 6
            0x03, // 4: iconst_0
            0xac, // 5: ireturn
            0x04, // 6: iconst_1
            0xac, // 7: ireturn
            0, 0,
        ];
        let blocks = build_cfg(&code, 8);
        assert!(
            blocks.len() >= 3,
            "should have at least 3 blocks, got {}",
            blocks.len()
        );
    }

    // ===================================================================
    // ARM64 register allocator tests (Phase 95)
    // ===================================================================

    #[test]
    fn p95_arm64_simple_method() {
        // iload_0; ireturn — single param, single local
        let code = [0x1a, 0xac, 0, 0];
        let result = allocate_registers_arm64(&code, 2, 1, 1, &[]);
        assert_eq!(result.assignments.len(), 1);
        assert!(
            result.assignments[0].is_some(),
            "param 0 should get ARM64 register"
        );
        // Should be one of X19-X28
        let reg = result.assignments[0].unwrap();
        assert!((19..=28).contains(&reg), "should be callee-saved X{reg}");
    }

    #[test]
    fn p95_arm64_many_locals_use_all_callee_saved() {
        // 10 locals all alive simultaneously — should fill X19-X28 exactly.
        // Create bytecode that loads all 10 locals:
        let mut code = Vec::new();
        for i in 0..10u8 {
            code.push(0x15); // iload
            code.push(i);
        }
        code.push(0xac); // ireturn
        let code_len = code.len() - 0; // no padding needed for this test
        let result = allocate_registers_arm64(&code, code_len, 10, 10, &[]);
        let assigned_count = result.assignments.iter().filter(|a| a.is_some()).count();
        assert_eq!(assigned_count, 10, "all 10 locals should fit in X19-X28");
        // All should be distinct
        let mut regs: Vec<u8> = result.assignments.iter().filter_map(|a| *a).collect();
        regs.sort();
        regs.dedup();
        assert_eq!(regs.len(), 10, "all registers should be distinct");
        // All should be in 19..=28
        for r in &regs {
            assert!(
                (19..=28).contains(r),
                "register X{r} not in callee-saved range"
            );
        }
    }

    #[test]
    fn p95_arm64_spill_with_12_locals() {
        // 12 locals all alive — 10 get registers, 2 must spill (ARM64 has 10 callee-saved GPRs).
        let mut code = Vec::new();
        for i in 0..12u8 {
            code.push(0x15); // iload
            code.push(i);
        }
        code.push(0xac); // ireturn
        let code_len = code.len();
        let result = allocate_registers_arm64(&code, code_len, 12, 12, &[]);
        let assigned_count = result.assignments.iter().filter(|a| a.is_some()).count();
        assert!(
            assigned_count <= 10,
            "at most 10 ARM64 callee-saved GPRs, got {assigned_count}"
        );
        let spill_count = result.assignments.iter().filter(|a| a.is_none()).count();
        assert!(
            spill_count >= 2,
            "should have at least 2 spills, got {spill_count}"
        );
    }

    #[test]
    fn p95_arm64_fp_registers() {
        // Float locals should get D8-D15 (callee-saved FP regs).
        // fload_0; fload_1; fadd; freturn
        let code = [
            0x22, // fload_0
            0x23, // fload_1
            0x62, // fadd
            0xae, // freturn
            0, 0,
        ];
        let result = allocate_registers_arm64(&code, 4, 2, 2, &[]);
        // Both locals should be recognized as float and get FP assignments
        assert!(
            result.xmm_assignments[0].is_some(),
            "float local 0 should get FP register"
        );
        assert!(
            result.xmm_assignments[1].is_some(),
            "float local 1 should get FP register"
        );
        // FP regs should be in 8..=15 (D8-D15)
        let fp0 = result.xmm_assignments[0].unwrap();
        let fp1 = result.xmm_assignments[1].unwrap();
        assert!((8..=15).contains(&fp0), "FP reg {fp0} not in D8-D15 range");
        assert!((8..=15).contains(&fp1), "FP reg {fp1} not in D8-D15 range");
        assert_ne!(
            fp0, fp1,
            "interfering float locals must get different FP regs"
        );
        // GPR assignments should be None for float locals
        assert!(
            result.assignments[0].is_none(),
            "float local 0 should NOT get GPR"
        );
        assert!(
            result.assignments[1].is_none(),
            "float local 1 should NOT get GPR"
        );
    }

    #[test]
    fn p95_arm64_mixed_int_float_locals() {
        // local 0: int param, local 1: float computed from local 0
        // iload_0; i2f; fstore_1; fload_1; freturn
        let code = [
            0x1a, // iload_0
            0x86, // i2f
            0x44, // fstore_1
            0x23, // fload_1
            0xae, // freturn
            0, 0,
        ];
        let result = allocate_registers_arm64(&code, 5, 2, 1, &[]);
        // local 0 = int → GPR
        assert!(
            result.assignments[0].is_some(),
            "int local 0 should get GPR"
        );
        assert!((19..=28).contains(&result.assignments[0].unwrap()));
        // local 1 = float → FP reg
        assert!(
            result.xmm_assignments[1].is_some(),
            "float local 1 should get FP register"
        );
        assert!((8..=15).contains(&result.xmm_assignments[1].unwrap()));
    }

    #[test]
    fn p95_arm64_used_callee_saved_tracking() {
        // 3 locals alive — should report exactly 3 callee-saved regs used.
        let code = [
            0x1a, // iload_0
            0x1b, // iload_1
            0x60, // iadd
            0x1c, // iload_2
            0x60, // iadd
            0xac, // ireturn
            0, 0,
        ];
        let result = allocate_registers_arm64(&code, 6, 3, 3, &[]);
        assert_eq!(
            result.used_callee_saved.len(),
            3,
            "should use exactly 3 callee-saved GPRs"
        );
        for &r in &result.used_callee_saved {
            assert!((19..=28).contains(&r), "callee-saved reg X{r} not in range");
        }
    }

    /// Every parameter slot must be live-in at entry, including the ones a
    /// category-2 parameter pushes past `num_params`.
    ///
    /// `private void f(long, Object, Void)` on an instance: `this`=0,
    /// `long`=1..2, `Object`=3, `Void`=4 — four parameters over five slots.
    /// Seeding liveness with `(1 << 4) - 1` left slot 4 (the `Void`) dead on
    /// entry, so the colouring was free to give it the same register as the
    /// live `Object` in slot 3 — and the prologue, which stores EVERY incoming
    /// argument into its home, then overwrote the `Object` with the always-null
    /// `Void` on the way in. Observed as `mov r12, rcx ; mov r12, r8`.
    #[test]
    fn every_parameter_slot_is_live_in_even_past_a_category_two_parameter() {
        // aload_0; lload_1; aload_3; aconst_null; invokevirtual #1; return
        let code = [0x2a, 0x1f, 0x2d, 0x01, 0xb6, 0x00, 0x01, 0xb1];
        let param_slots = [0usize, 1, 3, 4];

        let result =
            allocate_registers_with_handlers(&code, code.len(), 5, 4, &param_slots, &[], &[]);
        let homes: Vec<Option<u8>> = param_slots
            .iter()
            .map(|&s| result.assignments.get(s).copied().flatten())
            .collect();
        for (i, a) in homes.iter().enumerate() {
            for (j, b) in homes.iter().enumerate() {
                if i != j {
                    if let (Some(x), Some(y)) = (a, b) {
                        assert_ne!(
                            x, y,
                            "parameter slots {} and {} share register {x}; the prologue \
                             stores both and would clobber the live one",
                            param_slots[i], param_slots[j]
                        );
                    }
                }
            }
        }

        // The mask helper itself: slot-derived, not count-derived.
        assert_eq!(param_live_in_mask(4, &param_slots), 0b1_1011);
        // Empty slot map keeps the historical identity layout.
        assert_eq!(param_live_in_mask(4, &[]), 0b1111);
    }

    // ── live_locals_per_pc (OSR-exit dead-local detection) ──────────────────

    /// A local that ONLY the catch handler reads is live throughout the
    /// protected range — the handler is a successor of every instruction in
    /// it. Without exception edges the liveness scan says "dead", and the
    /// consumer that acts on that (the x64 deopt/exceptional-frame snapshot)
    /// then drops the value — and, for a reference, drops the GC's only view
    /// of that object through this frame.
    #[test]
    fn live_locals_per_pc_sees_a_local_only_the_handler_reads() {
        // 0: iconst_0            9: istore_2
        // 1: istore_1           10: return
        // 2: invokestatic #2    11: return
        // 5: goto 11
        // 8: iload_1   <- handler entry, reads local 1
        // exception table: [2, 5) -> handler 8
        let code = [
            0x03, 0x3c, 0xb8, 0x00, 0x02, 0xa7, 0x00, 0x06, 0x1b, 0x3d, 0xb1, 0xb1,
        ];
        let len = code.len();
        let blind = live_locals_per_pc(&code, len, 1);
        assert_eq!(
            blind[2] & (1 << 1),
            0,
            "precondition: with no exception edges the handler's read is invisible"
        );

        let (aware, covered) = live_locals_per_pc_with_handlers(&code, len, 1, &[], &[(2, 5, 8)]);
        assert_ne!(
            aware[2] & (1 << 1),
            0,
            "local 1 must be live at the protected pc — the handler reads it"
        );
        assert!(
            covered[8],
            "the handler pc must become a covered block start"
        );
        assert_eq!(
            aware[1] & (1 << 1),
            0,
            "the exception edge must not make the local live BEFORE its store"
        );
    }

    #[test]
    fn live_locals_per_pc_marks_loop_counter_dead_after_loop_exit() {
        // Mirrors the shape of `TestOutputBuffer.WritingServlet.doGet`'s
        // `for (int i = 0; i < writeCount; i++) w.write(...)` loop: a counter
        // local that is live throughout the loop body but genuinely dead once
        // control reaches the post-loop code (here, returning a DIFFERENT
        // local). Local 0 = returned after the loop, local 1 = loop counter
        // `i`, local 2 = the loop bound.
        #[rustfmt::skip]
        let code: [u8; 15] = [
            0x03,             // 0:  iconst_0
            0x3c,             // 1:  istore_1        i = 0
            0x1b,             // 2:  iload_1          <- LOOP HEADER
            0x1c,             // 3:  iload_2
            0xa2, 0x00, 0x09, // 4:  if_icmpge +9 -> pc13 (EXIT)
            0x84, 0x01, 0x01, // 7:  iinc 1, 1
            0xa7, 0xff, 0xf8, // 10: goto -8 -> pc2 (LOOP HEADER)
            0x1a,             // 13: iload_0          <- EXIT
            0xac,             // 14: ireturn
        ];
        let live_at = live_locals_per_pc(&code, code.len(), 3);

        // Inside the loop (the `iinc`), the counter is live: the loop
        // condition and the increment itself both still need it.
        assert_ne!(
            live_at[7] & (1 << 1),
            0,
            "loop counter (local 1) must be live at the iinc inside the loop"
        );

        // At the post-loop `iload_0` the counter is dead — nothing after this
        // point reads local 1 before the method returns.
        assert_eq!(
            live_at[13] & (1 << 1),
            0,
            "loop counter (local 1) must be dead once the loop has exited"
        );
        // The local the post-loop code actually reads IS live there.
        assert_ne!(
            live_at[13] & (1 << 0),
            0,
            "local 0 must be live at the iload_0 that reads it"
        );
    }

    #[test]
    fn live_locals_per_pc_straight_line_no_locals_live_after_last_use() {
        let code: [u8; 6] = [
            0x1a, // 0: iload_0
            0x1b, // 1: iload_1
            0x60, // 2: iadd
            0x3c, // 3: istore_1   (local 1 redefined — its old value is dead)
            0x1c, // 4: iload_2
            0xac, // 5: ireturn (return value must be int on stack; irrelevant here)
        ];
        let live_at = live_locals_per_pc(&code, code.len(), 3);
        // At the final `iload_2`, locals 0 and 1 are both dead: local 0's only
        // use was at pc0, and local 1 was just overwritten at pc3 without a
        // subsequent read before the method returns.
        assert_eq!(live_at[4] & (1 << 0), 0, "local 0 dead after its only use");
        assert_eq!(
            live_at[4] & (1 << 1),
            0,
            "local 1 dead after being overwritten"
        );
        assert_ne!(
            live_at[4] & (1 << 2),
            0,
            "local 2 live at the iload_2 that reads it"
        );
    }

    // -----------------------------------------------------------------------
    // CFG coverage after an unconditional terminator (handler-shaped region)
    // -----------------------------------------------------------------------

    /// The shape this fixes: a protected region that ends in `ireturn`,
    /// followed by an exception handler that is not a branch target. Before
    /// the fix, `build_cfg`'s second pass skipped every PC from the handler's
    /// `astore` to the end of the method (nothing marked them as block
    /// starts), so the handler's local accesses were invisible to liveness and
    /// interference.
    const HANDLER_AFTER_RETURN: [u8; 9] = [
        0x1a, // 0: iload_0
        0xac, // 1: ireturn                  <- protected region ends here
        0x4c, // 2: astore_1  (handler entry: store the pending exception)
        0x12, 0x01, // 3: ldc #1
        0x4d, // 5: astore_2
        0x2c, // 6: aload_2
        0x2b, // 7: aload_1
        0xbf, // 8: athrow
    ];

    #[test]
    fn cfg_covers_code_after_an_unconditional_terminator() {
        let code = HANDLER_AFTER_RETURN;
        let live = live_locals_per_pc(&code, code.len(), 1);
        // The handler reads locals 1 and 2 at pc7/pc6. If the region were
        // uncovered, every entry here would be the `0` default.
        assert_ne!(
            live[7] & (1 << 1),
            0,
            "local 1 must be live at the aload_1 inside the handler"
        );
        assert_ne!(
            live[6] & (1 << 2),
            0,
            "local 2 must be live at the aload_2 inside the handler"
        );
    }

    #[test]
    fn interference_sees_locals_defined_after_a_return() {
        let code = HANDLER_AFTER_RETURN;
        let mut blocks = build_cfg(&code, code.len());
        for b in &mut blocks {
            compute_gen_kill(&code, b);
        }
        solve_liveness(&mut blocks, 1);
        let interference = build_interference(&code, &blocks, 3, &[]);
        // Locals 1 and 2 are simultaneously live at the `aload_2; aload_1`
        // pair, so they MUST interfere — otherwise the colourer is free to
        // give them the same register and the athrow rethrows the wrong ref.
        assert_ne!(
            interference[1] & (1 << 2),
            0,
            "locals 1 and 2 are simultaneously live in the handler and must interfere"
        );
        assert_ne!(
            interference[2] & (1 << 1),
            0,
            "interference must be symmetric"
        );
    }

    /// A local that ONLY the exception handler reads must not be given the
    /// register of a local that is live across the protected range.
    ///
    /// Without exception edges the handler block has no predecessor, so such a
    /// local's live range is the handler alone: it interferes with nothing in
    /// the try and the colorer hands both locals the same physical register.
    /// The precise exceptional frame then reconstructs the handler's local from
    /// that register and reads the OTHER local's value — an unrelated object
    /// standing where a live one was, which is exactly the json-smart symptom
    /// this whole family produced.
    #[test]
    fn handler_only_local_does_not_share_a_register_with_a_live_local() {
        // 0: aconst_null   1: astore_1        (local 1 — read ONLY by the handler)
        // 2: aconst_null   3: astore_2        (local 2 — read after the try)
        // 4: nop           5: nop             protected [4,6)
        // 6: goto +6 -> 12
        // 9: astore_3     10: aload_1  11: areturn      <- handler at 9
        // 12: aload_2     13: areturn
        let code: Vec<u8> = vec![
            0x01, 0x4c, 0x01, 0x4d, 0x00, 0x00, 0xa7, 0x00, 0x06, 0x4e, 0x2b, 0xb0, 0x2c, 0xb0,
        ];
        let code_len = code.len();
        let handlers = [(4usize, 6usize, 9usize)];

        // Assert the INTERFERENCE relation, not a coloring outcome: with four
        // locals and a full callee-saved file the colorer has no pressure to
        // alias, so "they got the same register" is not a reliable statement of
        // the hazard — "the allocator does not know they are simultaneously
        // live" is.
        let interference = |handlers: &[(usize, usize, usize)]| -> u64 {
            let mut blocks = build_cfg_with_handlers(&code, code_len, handlers);
            for block in &mut blocks {
                compute_gen_kill(&code, block);
            }
            solve_liveness(&mut blocks, 0);
            build_interference(&code, &blocks, 4, &[])[1]
        };

        assert_eq!(
            interference(&[]) & (1 << 2),
            0,
            "documents the hazard: without exception edges the handler-only local \
             does not interfere with the local live across the try, so the colorer \
             is free to give them the same register"
        );
        assert_ne!(
            interference(&handlers) & (1 << 2),
            0,
            "with exception edges the handler-only local is live across the \
             protected range and must interfere with the local live there"
        );

        // ...and the allocator built on that graph must keep them apart.
        let modelled = allocate_registers_with_handlers(&code, code_len, 4, 0, &[], &[], &handlers);
        assert!(
            modelled.assignments[1].is_some() && modelled.assignments[2].is_some(),
            "modelling handlers must not de-register-home either local"
        );
        assert_ne!(modelled.assignments[1], modelled.assignments[2]);
    }

    #[test]
    fn find_reference_locals_sees_handler_locals_after_a_return() {
        // `find_reference_locals` walks raw bytecode, not the CFG, so it was
        // never affected — pin that, because the publish plan's soundness
        // rests on it covering handler code the CFG previously missed.
        let code = HANDLER_AFTER_RETURN;
        let refs = find_reference_locals(&code, code.len(), 3);
        assert_ne!(refs & (1 << 1), 0, "local 1 is astore_1/aload_1'd");
        assert_ne!(refs & (1 << 2), 0, "local 2 is astore_2/aload_2'd");
        assert_eq!(refs & 1, 0, "local 0 is only ever iload'ed");
    }

    // -----------------------------------------------------------------------
    // Safepoint publication planning
    // -----------------------------------------------------------------------

    /// `int fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }`-shaped
    /// body: one int local, two `invokestatic`s. Nothing here can hold an oop,
    /// so a GC-capable call site has nothing to publish.
    #[test]
    fn publish_plan_is_empty_for_an_int_only_recursive_kernel() {
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0
            0x05, // 1: iconst_2
            0xa2, 0x00, 0x05, // 2: if_icmpge +5 -> 7
            0x1a, // 5: iload_0
            0xac, // 6: ireturn
            0x1a, // 7: iload_0
            0x04, // 8: iconst_1
            0x64, // 9: isub
            0xb8, 0x00, 0x01, // 10: invokestatic #1
            0x1a, // 13: iload_0
            0x05, // 14: iconst_2
            0x64, // 15: isub
            0xb8, 0x00, 0x01, // 16: invokestatic #1
            0x60, // 19: iadd
            0xac, // 20: ireturn
        ];
        let code_len = code.len();
        // Pretend the colourer gave local 0 a register home.
        let assignments = vec![Some(12u8)];
        let plan = plan_safepoint_publication(&code, code_len, 1, 1, &[], &assignments, 0, &[]);

        assert_eq!(plan.reference_locals, 0, "no aload/astore anywhere");
        assert_eq!(plan.register_homed_reference_locals, 0);
        assert_eq!(plan.publish_always, 0);
        assert!(
            plan.no_reference_in_registers(),
            "an int-only kernel must be allowed to skip local publication"
        );
        // At both call sites there is nothing to publish.
        assert_eq!(plan.publish_at_bci(10), 0);
        assert_eq!(plan.publish_at_bci(16), 0);
    }

    #[test]
    fn publish_plan_keeps_register_homed_reference_locals() {
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0   (local 0 is a reference)
            0x4c, // 1: astore_1  (local 1 is a reference)
            0x1c, // 2: iload_2   (local 2 is an int)
            0xb8, 0x00, 0x01, // 3: invokestatic #1
            0x2b, // 6: aload_1   (keeps local 1 live across the call)
            0xb0, // 7: areturn
        ];
        let code_len = code.len();
        // Locals 0 and 1 are refs, local 2 is an int; all three get registers.
        let assignments = vec![Some(12u8), Some(13u8), Some(14u8)];
        let plan = plan_safepoint_publication(&code, code_len, 3, 1, &[], &assignments, 0, &[]);

        assert_eq!(plan.reference_locals, 0b011);
        assert_eq!(plan.register_homed_reference_locals, 0b011);
        assert!(
            !plan.no_reference_in_registers(),
            "a register-homed reference local must force publication"
        );
        // Local 1 is read after the call, so it must be published at the call.
        assert_ne!(
            plan.publish_at_bci(3) & (1 << 1),
            0,
            "a reference local live across the call must be published"
        );
        // Local 2 is an int — never published regardless of liveness.
        assert_eq!(
            plan.publish_at_bci(3) & (1 << 2),
            0,
            "a primitive local is never a GC root and must not be published"
        );
    }

    #[test]
    fn publish_plan_narrows_by_liveness_but_never_below_a_dead_reference() {
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x4c, // 1: astore_1
            0x2b, // 2: aload_1     <- local 1's LAST read
            0x57, // 3: pop
            0xb8, 0x00, 0x01, // 4: invokestatic #1  (local 1 is dead here)
            0xb1, // 7: return
        ];
        let code_len = code.len();
        let assignments = vec![Some(12u8), Some(13u8)];
        let plan = plan_safepoint_publication(&code, code_len, 2, 1, &[], &assignments, 0, &[]);
        assert_ne!(plan.register_homed_reference_locals & (1 << 1), 0);
        assert_eq!(
            plan.publish_at_bci(4) & (1 << 1),
            0,
            "a reference local whose last read is behind the call is not a live root"
        );
        // The bci-independent set is unaffected by liveness — a consumer that
        // does not want the exception-handler-CFG precondition uses this.
        assert_ne!(plan.publish_always & (1 << 1), 0);
    }

    /// Uncovered PCs (no basic block reaches them) read `0` from the liveness
    /// table by default, which must NOT be mistaken for "nothing live".
    #[test]
    fn publish_plan_falls_back_to_conservative_set_on_uncovered_pcs() {
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x4c, // 1: astore_1
            0xb1, // 2: return
        ];
        let code_len = code.len();
        let assignments = vec![Some(12u8), Some(13u8)];
        let plan = plan_safepoint_publication(&code, code_len, 2, 1, &[], &assignments, 0, &[]);
        // pc past the end of the code is definitionally uncovered.
        assert_eq!(
            plan.publish_at_bci(code_len + 32),
            plan.publish_always,
            "an out-of-range bci must fall back to the conservative set"
        );
    }

    #[test]
    fn publish_plan_ignores_locals_without_a_register_home() {
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x4c, // 1: astore_1
            0xb8, 0x00, 0x01, // 2: invokestatic #1
            0x2b, // 5: aload_1
            0xb0, // 6: areturn
        ];
        let code_len = code.len();
        // Local 1 spilled (no register home) — its canonical frame slot is
        // already authoritative, so it is not part of the publish set.
        let assignments = vec![Some(12u8), None];
        let plan = plan_safepoint_publication(&code, code_len, 2, 1, &[], &assignments, 0, &[]);
        assert_ne!(plan.reference_locals & (1 << 1), 0);
        assert_eq!(
            plan.register_homed_reference_locals & (1 << 1),
            0,
            "a frame-homed reference local needs no publication"
        );
    }

    #[test]
    fn publish_plan_unions_the_caller_supplied_reference_parameter_mask() {
        // `void f(Object unused)` whose parameter is never read: no `aload`
        // taints slot 0, so only the caller's descriptor-derived mask knows.
        let code: Vec<u8> = vec![0xb1]; // return
        let assignments = vec![Some(12u8)];
        let plan = plan_safepoint_publication(&code, code.len(), 1, 1, &[], &assignments, 0b1, &[]);
        assert_ne!(plan.reference_locals & 1, 0);
        assert_ne!(plan.register_homed_reference_locals & 1, 0);
        assert!(!plan.no_reference_in_registers());
    }

    // -----------------------------------------------------------------------
    // Save-area sizing — the historical spill-overflow defect
    // -----------------------------------------------------------------------
    //
    // The x64 prologue sizes its callee-saved save area as
    // `used_callee_saved.len() * 8` and writes register `used_callee_saved[i]`
    // at `callee_saved_base + i*8`; the per-safepoint spill region is sized
    // from the SAME vector. If `used_callee_saved` ever reported fewer
    // registers than `assignments` actually uses, the prologue would under-
    // reserve and the safepoint spill would write past its region into the
    // caller's saved registers — the documented `Token.duplicate` corruption.
    // Pin the contract from this side.

    fn assert_save_area_contract(result: &RegAllocResult, pool: &[u8]) {
        let mut seen: Vec<u8> = result.assignments.iter().flatten().copied().collect();
        seen.sort_unstable();
        seen.dedup();
        let mut reported = result.used_callee_saved.clone();
        reported.sort_unstable();
        assert_eq!(
            seen, reported,
            "used_callee_saved must be exactly the deduped set of assigned registers"
        );
        for r in &result.used_callee_saved {
            assert!(
                pool.contains(r),
                "assigned register {r} is outside the allocation pool {pool:?}"
            );
        }
        let mut dedup = result.used_callee_saved.clone();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            result.used_callee_saved.len(),
            "used_callee_saved must not contain duplicates — each entry owns one 8-byte save slot"
        );
        assert!(
            result.used_callee_saved.len() <= pool.len(),
            "save area can never need more slots than the pool has registers"
        );
    }

    #[test]
    fn save_area_contract_holds_for_a_register_pressured_method() {
        // 12 distinct, simultaneously-live locals against a 5/7-register pool:
        // forces spilling, which is exactly when the reported save-area set
        // and the actual assignments are most likely to drift apart.
        let mut code: Vec<u8> = Vec::new();
        for slot in 0..12u8 {
            code.push(0x15); // iload <slot>
            code.push(slot);
        }
        for slot in (0..12u8).rev() {
            code.push(0x36); // istore <slot>
            code.push(slot);
        }
        code.push(0xb1); // return
        let code_len = code.len();
        let result = allocate_registers_with(
            &code,
            code_len,
            12,
            12,
            &[],
            &[],
            &ARM64_LOCAL_GPRS,
            &ARM64_LOCAL_FPS,
            &[],
        );
        assert_save_area_contract(&result, &ARM64_LOCAL_GPRS);
    }

    #[test]
    fn save_area_contract_holds_for_the_handler_shaped_method() {
        let code = HANDLER_AFTER_RETURN;
        let result = allocate_registers_with(
            &code,
            code.len(),
            3,
            1,
            &[],
            &[],
            &ARM64_LOCAL_GPRS,
            &ARM64_LOCAL_FPS,
            &[],
        );
        assert_save_area_contract(&result, &ARM64_LOCAL_GPRS);
    }

    #[test]
    fn save_area_contract_holds_when_every_local_spills() {
        // Zero available registers → every local must spill and the save area
        // must be empty (a non-empty one would reserve slots the prologue
        // never writes and shift every later frame region).
        let code: Vec<u8> = vec![0x1a, 0x1b, 0x60, 0x3c, 0xb1];
        let result = allocate_registers_with(&code, code.len(), 2, 2, &[], &[], &[], &[], &[]);
        assert!(result.assignments.iter().all(Option::is_none));
        assert!(result.used_callee_saved.is_empty());
    }
}

#[cfg(test)]
mod handler_liveness_tests {
    use super::*;

    /// A local the handler reads, redefined by the protected code AFTER the
    /// throwing instruction, must still be live AT that instruction.
    ///
    /// This is `C2Handlers.loopInsideTry`'s shape reduced to its essentials.
    /// The block-level exception edge alone reported it dead — the backward
    /// walk applied the `istore_1` that follows the call — so the precise
    /// exceptional frame recorded `sum` as `Undefined` and the handler
    /// returned `-0` instead of `-sum`.
    #[test]
    fn local_redefined_after_throw_site_stays_live_for_the_handler() {
        // 0: iconst_0
        // 1: istore_1        sum = 0
        // 2: iload_1         <- protected range starts; sum pushed
        // 3: iload_0
        // 4: invokestatic #1 <- THROW SITE
        // 7: iadd
        // 8: istore_1        sum redefined (kills local 1 on the normal path)
        // 9: goto 15
        // 12: astore_2       <- handler: reads local 1
        // 13: iload_1
        // 14: ireturn
        // 15: iload_1
        // 16: ireturn
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1
            0x1a, // 3: iload_0
            0xb8, 0x00, 0x01, // 4: invokestatic
            0x60, // 7: iadd
            0x3c, // 8: istore_1
            0xa7, 0x00, 0x06, // 9: goto +6 -> 15
            0x4d, // 12: astore_2
            0x1b, // 13: iload_1
            0xac, // 14: ireturn
            0x1b, // 15: iload_1
            0xac, // 16: ireturn
        ];
        let handlers = [(2usize, 9usize, 12usize)];

        let (live_with, covered_with) =
            live_locals_per_pc_with_handlers(&code, code.len(), 1, &[], &handlers);
        assert!(covered_with[4], "the throw site must be a covered pc");
        assert!(
            live_with[4] & (1 << 1) != 0,
            "local 1 is read by the handler at pc 13 and an exception at pc 4 \
             reaches that handler, so local 1 must be live at pc 4; the \
             `istore_1` at pc 8 never executes on the exception path. \
             live_at[4] = {:#x}",
            live_with[4]
        );

        // Control: with no handlers modelled the same local is genuinely dead
        // there, so this test is pinning the exception edge and not a
        // tautology.
        let (live_without, _) = live_locals_per_pc_with_handlers(&code, code.len(), 1, &[], &[]);
        assert!(
            live_without[4] & (1 << 1) == 0,
            "without the exception edge local 1 is dead at pc 4 (redefined at \
             pc 8 before its only normal-path read) — if this ever becomes \
             live the test above proves nothing"
        );
    }

    /// The interference-graph half: whatever is defined inside the protected
    /// range must not be allowed to share a register with a local the handler
    /// reads.
    #[test]
    fn handler_read_local_interferes_with_defs_inside_the_protected_range() {
        // Same shape, but local 2 is assigned inside the try so it competes
        // with local 1 (which only the handler reads after the redefinition).
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1
            0x1a, // 3: iload_0
            0xb8, 0x00, 0x01, // 4: invokestatic
            0x3d, // 7: istore_2      def inside the range
            0x3c, // 8: istore_1
            0xa7, 0x00, 0x06, // 9: goto -> 15
            0x4e, // 12: astore_3
            0x1b, // 13: iload_1      handler reads local 1
            0xac, // 14: ireturn
            0x1c, // 15: iload_2
            0xac, // 16: ireturn
        ];
        let handlers = [(2usize, 9usize, 12usize)];
        let mut blocks = build_cfg_with_handlers(&code, code.len(), &handlers);
        for block in &mut blocks {
            compute_gen_kill(&code, block);
        }
        solve_liveness(&mut blocks, 1);
        let ranges = handler_live_in_ranges(&blocks, &handlers);
        let interference = build_interference(&code, &blocks, 4, &ranges);
        assert!(
            interference[2] & (1 << 1) != 0,
            "local 2 is defined at pc 7, inside the protected range, while \
             local 1 must survive for the handler — they must interfere so the \
             allocator cannot give them the same register (interference[2] = \
             {:#x})",
            interference[2]
        );
    }
}

// ═════════════════════════════════════════════════════════════════════
// Linear-scan register allocation over the scheduled IR
// ═════════════════════════════════════════════════════════════════════
//
// Everything above this line allocates **JVM bytecode locals** to callee-saved
// registers by graph colouring, for the single-pass backend. Everything below
// allocates **IR values** (`ir::Graph` nodes, post-`ir_schedule`) to the
// physical register file, for the optimizing backend. The two share this file
// and the loop-frequency weights, and nothing else — different inputs,
// different units, different clients.
//
// ## Relationship to `ir_lower`'s frame-slot colouring
//
// `ir_lower::plan_slots` already computes, for the same graph, a liveness
// colouring that packs every value into the smallest set of 8-byte frame
// words. This allocator answers the *next* question: of those values, which
// can live in a machine register instead of a frame word, and for how long.
//
// The two must agree about liveness or they will disagree about aliasing, so
// [`build_live_model`] reproduces `plan_slots`' position model **exactly**:
//
//   * one linear position per scheduled node, in block order, then one for the
//     block terminator, then one for the block's *outgoing edge*;
//   * a phi's value inputs are consumed at the **predecessor's edge position**,
//     never at the phi;
//   * ranges are widened to whole block spans wherever the backward liveness
//     fixed point says the value is live-in / live-out, which is what makes a
//     loop-carried value live across the entire loop;
//   * overlap is **endpoint-inclusive**: `[4, 7]` and `[7, 9]` overlap.
//
// It is a deliberate *re-implementation* rather than a call: `plan_slots`,
// `SlotPlan`, `LiveRange` and `SlotClass` are all private to `ir_lower.rs`, and
// this wave may not edit that file. See `docs/jit/linear-scan-regalloc.md` for
// the one-line visibility change that would let the two share a single
// implementation, which is the right end state.
//
// ## What is *not* modelled
//
//   * **Lifetime holes.** An interval is one contiguous `[lo, hi]` range, as in
//     `plan_slots`. A value that is dead across the middle of its range still
//     holds its register there. This over-approximates liveness, which is the
//     safe direction: it costs registers, it cannot alias two live values.
//   * **Register pairs / sub-registers.** Every value occupies exactly one
//     register of its class. `Long` and `Int` are both one GP register (x86-64),
//     `Float` and `Double` both one XMM.
//   * **Coalescing.** Phi webs are resolved with explicit copies
//     ([`resolve_parallel_copy`]), not by biasing the allocation.

/// Which physical register bank a value is drawn from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegClass {
    /// General-purpose integer registers (`Int`, `Long`, `Ref`).
    Gp,
    /// SSE registers (`Float`, `Double`).
    Xmm,
}

impl RegClass {
    /// The bank an [`IrType`] must live in, or `None` for the types that carry
    /// no runtime value at all (`Void`, `Control`, `Memory`).
    ///
    /// `Ref` maps to [`RegClass::Gp`] like any other 64-bit integer — the
    /// *class* is not what keeps a reference describable to the collector; see
    /// the safepoint rule on [`allocate_linear_scan`].
    pub fn of(ty: IrType) -> Option<RegClass> {
        match ty {
            IrType::Int | IrType::Long | IrType::Ref => Some(RegClass::Gp),
            IrType::Float | IrType::Double => Some(RegClass::Xmm),
            IrType::Void | IrType::Control | IrType::Memory => None,
        }
    }
}

/// Who owns which XMM register, in one place.
///
/// Three components hand out XMM registers, and until this module existed each
/// held its own private answer: `ir_lower`'s FP value tier (XMM0/XMM1),
/// `ir_lower`'s linear-scan file, and `x64::vec_emit`'s vector pool. The three
/// **overlap**, and the failure that overlap produces is silent — a scalar
/// `double` living in XMM3 destroyed by a vector region that took XMM3 for a
/// lane accumulator, with nothing to notice but a wrong number much later.
///
/// The overlap is **gone as of 2026-08-04**: `ir_lower::emit_prologue` now
/// carries a callee-saved XMM save area ([`IR_PROLOGUE_SAVED`]), which bought
/// the two registers that let the scalar file grow and moved the vector pool
/// entirely out of the scalar range. What this module keeps is the property
/// that made the fix checkable — the ranges are declared *together*, so a
/// wiring that puts two authorities on one register is a visible fact rather
/// than a discovery, and [`disjointness_violation`] is the enforced half.
///
/// `docs/jit/vectorization-emitter.md` names this the single highest-risk
/// prerequisite for wiring `emit_vector_loop`, and
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`
/// increment 4 is the item that closed it.
pub mod xmm_roles {
    /// `ir_lower`'s FP value tier: the scratch pair every float/double
    /// arithmetic arm computes in — XMM0 the first operand and the result,
    /// XMM1 the second.
    pub const IR_FP_SCRATCH: [u8; 2] = [0, 1];

    /// `ir_lower`'s linear-scan file — the registers a *long-lived* FP value
    /// may be promoted into for the whole method.
    ///
    /// Disjoint from [`IR_FP_SCRATCH`] by construction, which is why residency
    /// and the arithmetic arms can coexist.
    ///
    /// XMM6/XMM7 are here because [`IR_PROLOGUE_SAVED`] pays for them. The
    /// ceiling is XMM7, not register pressure: `ir_lower`'s `fp_load` /
    /// `fp_store` / `fp_binop` emit ModRM with `(xmm & 7) << 3` and **no REX**,
    /// so XMM8 and up are not addressable by that emitter at all. Raising the
    /// ceiling is a REX change in those three functions, not a save-area
    /// change.
    pub const IR_LINEAR_SCAN: [u8; 6] = [2, 3, 4, 5, 6, 7];

    /// The XMM registers `ir_lower::emit_prologue` saves and every exit
    /// restores — empty on System V, where the ABI makes every XMM volatile
    /// and there is nothing to save.
    ///
    /// This is the list that makes the two ranges above and below able to
    /// coexist with a *caller*. Win64 makes XMM6–XMM15 non-volatile; without a
    /// save area, any use of one silently corrupts the caller's floating-point
    /// state on one platform and not the other — the worst possible shape for
    /// a bug, and the reason `IR_LINEAR_SCAN` stopped at XMM5 until now.
    ///
    /// Only XMM6/XMM7 appear, not XMM6–XMM15: they are what `IR_LINEAR_SCAN`
    /// can address (see above), and a frame does not pay 16 bytes to save a
    /// register nothing can name. [`VECTOR_REGION_MAX`] is XMM8–XMM15 and owes
    /// its own save area — see [`vector_pool_is_encodable`].
    #[cfg(windows)]
    pub const IR_PROLOGUE_SAVED: &[u8] = &[6, 7];
    /// System V: every XMM is caller-saved, so the prologue saves none.
    #[cfg(not(windows))]
    pub const IR_PROLOGUE_SAVED: &[u8] = &[];

    /// The widest pool a vector region may be given.
    ///
    /// **Disjoint from [`IR_FP_SCRATCH`] and [`IR_LINEAR_SCAN`]**, which is the
    /// whole point: a vector region can no longer destroy a scalar `double` a
    /// caller left live, so `emit_vector_loop`'s caller owes no
    /// prove-the-scalars-are-dead argument. It owes a different one, stated by
    /// [`vector_pool_is_encodable`]: on Windows every register here is
    /// callee-saved, so a frame that hands one out must save it.
    ///
    /// XMM8–XMM15 rather than the low half because `vec_emit` encodes with VEX,
    /// which carries the high bit for free — the constraint that pins
    /// `IR_LINEAR_SCAN` to XMM0–XMM7 does not apply here.
    pub const VECTOR_REGION_MAX: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];

    /// May a vector region take `reg`, given the set of XMM registers the
    /// calling frame's prologue saves?
    ///
    /// Two conditions, and the second is the one that used to be prose:
    ///
    ///   1. `reg` is in [`VECTOR_REGION_MAX`] — no other register is the
    ///      vector pool's to give;
    ///   2. it is caller-saved on this target, **or** `frame_saved` says the
    ///      caller's prologue saved it. On System V (1) implies (2); on
    ///      Windows every register in the pool is non-volatile, so a caller
    ///      that saves nothing gets an empty pool and `emit_vector_loop`
    ///      refuses — which is the correct answer, not a missed optimization.
    pub fn vector_pool_is_encodable(reg: u8, frame_saved: &[u8]) -> bool {
        if !VECTOR_REGION_MAX.contains(&reg) {
            return false;
        }
        #[cfg(windows)]
        {
            frame_saved.contains(&reg)
        }
        #[cfg(not(windows))]
        {
            let _ = frame_saved;
            true
        }
    }

    /// `ir_lower`'s **general-purpose** linear-scan file — the registers a
    /// long-lived `int`/`long` value may be promoted into for a whole method.
    ///
    /// RBX and R12–R15, and every part of that choice is forced:
    ///
    ///   * **Callee-saved on the target ABI.** `ir_lower` emits calls
    ///     constantly — runtime helpers, inline-cache dispatch, JIT-to-JIT
    ///     direct calls — and this wiring has no reload machinery, so a
    ///     value's register must survive a call by the calling convention
    ///     rather than by analysis. That rules out every caller-saved
    ///     register.
    ///
    ///     RSI/RDI are caller-saved on **System V** and callee-saved on
    ///     **Win64**, so they are in the file on Windows and out of it
    ///     elsewhere — the same platform split [`IR_PROLOGUE_SAVED`] already
    ///     makes for XMM6/XMM7, and for the same reason. This constant said
    ///     they were caller-saved unconditionally until 2026-09-10: a System V
    ///     fact stated as an ABI-independent one, and it cost the optimizing
    ///     tier two of the seven callee-saved registers the single-pass
    ///     backend has been colouring locals into all along (`x64::LOCAL_REGS`
    ///     is `[u8; 7]` on Windows and `[u8; 5]` elsewhere).
    ///   * **Untouched by this emitter.** The value tier is RAX/RCX/RDX, the
    ///     safepoint and shadow-stack scratch is R10/R11, and call arguments go
    ///     in `ENTRY_ABI_REGS`. None of those overlaps this set.
    ///   * RBP and RSP are excluded for the obvious reason. R13 is *included*:
    ///     it is only awkward as an addressing BASE (no `mod=00` form), and
    ///     nothing here uses one of these as a base.
    ///
    /// The frame stays authoritative — this is a write-through read cache,
    /// exactly like [`IR_LINEAR_SCAN`] — so no oop map, deopt frame state or
    /// phi copy changes. What does change is that the prologue must save these
    /// and every exit restore them: see [`IR_GP_PROLOGUE_SAVED`].
    ///
    /// **`IrType::Ref` is never promoted**, and that is not a tuning choice. A
    /// GC safepoint walks a frame it did not stop, through RBP, and
    /// `OopMapEntry` can only name frame slots — so no reference may be
    /// register-resident at one. The XMM file discharged that obligation
    /// structurally, by having no register a `Ref` could occupy; a GP file has
    /// to discharge it by refusing the type, which `plan_register_residency`
    /// does and which its own test pins.
    /// Win64: RBX, R12–R15 **and RSI/RDI**, which this ABI makes
    /// callee-saved. The two extra registers are appended rather than
    /// interleaved so that a method whose peak live set fits in five is
    /// allocated exactly as it was before they existed.
    #[cfg(windows)]
    pub const IR_GP_LINEAR_SCAN: [u8; 7] = [3, 12, 13, 14, 15, 6, 7];
    /// System V: RSI/RDI are argument registers and caller-saved, so the file
    /// is RBX and R12–R15 alone.
    #[cfg(not(windows))]
    pub const IR_GP_LINEAR_SCAN: [u8; 5] = [3, 12, 13, 14, 15];

    /// The first [`IR_GP_LINEAR_SCAN`] entries that were the file before the
    /// Win64 widening — what `CRATONVM_JIT_IR_GP_WIDE=0` restores. Five on
    /// every platform, which on System V is the whole file.
    pub const IR_GP_LINEAR_SCAN_NARROW: usize = 5;

    /// The GP registers `ir_lower::emit_prologue` saves and every exit
    /// restores — all of [`IR_GP_LINEAR_SCAN`], because every one of them is
    /// callee-saved on the target ABI and that is exactly why they were
    /// chosen.
    ///
    /// Platform-conditional only through [`IR_GP_LINEAR_SCAN`]: System V and
    /// Win64 agree about RBX and R12–R15 and disagree about RSI/RDI.
    /// `Lowerer::saved_gpr_regs` emits a save only for a register the
    /// residency plan actually handed out, so a widened file that goes unused
    /// costs frame bytes and no instructions.
    pub const IR_GP_PROLOGUE_SAVED: &[u8] = &IR_GP_LINEAR_SCAN;

    /// The first register claimed by two of the three authorities, if any.
    ///
    /// The invariant this module exists to make checkable, as a value rather
    /// than a paragraph. `None` is the healthy answer and
    /// `the_three_xmm_authorities_are_disjoint` pins it.
    pub fn disjointness_violation() -> Option<u8> {
        for reg in 0u8..16 {
            let claims = u8::from(IR_FP_SCRATCH.contains(&reg))
                + u8::from(IR_LINEAR_SCAN.contains(&reg))
                + u8::from(VECTOR_REGION_MAX.contains(&reg));
            if claims > 1 {
                return Some(reg);
            }
        }
        None
    }
}

/// One physical register: a bank plus the encoding number the backend uses
/// (`RAX` = 0 … `R15` = 15 for [`RegClass::Gp`], `XMM0` = 0 … `XMM15` = 15 for
/// [`RegClass::Xmm`]) — the same numbering as `x64.rs` and `ir_lower.rs`.
///
/// The class is part of the identity on purpose: `Gp(8)` (R8) and `Xmm(8)`
/// (XMM8) are different registers that would otherwise compare equal, and the
/// aliasing check in [`verify_allocation`] is exactly a comparison of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysReg {
    /// Which bank.
    pub class: RegClass,
    /// Encoding number within the bank.
    pub num: u8,
}

impl PhysReg {
    /// A general-purpose register by encoding number.
    pub const fn gp(num: u8) -> PhysReg {
        PhysReg {
            class: RegClass::Gp,
            num,
        }
    }

    /// An SSE register by encoding number.
    pub const fn xmm(num: u8) -> PhysReg {
        PhysReg {
            class: RegClass::Xmm,
            num,
        }
    }
}

impl std::fmt::Display for PhysReg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.class {
            RegClass::Gp => write!(f, "r{}", self.num),
            RegClass::Xmm => write!(f, "xmm{}", self.num),
        }
    }
}

/// One allocatable register and the only ABI fact the allocator needs about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegSpec {
    /// The register.
    pub reg: PhysReg,
    /// True when a call destroys it. A value live across a call may only be
    /// placed in a register for which this is false; otherwise the interval is
    /// split at the call (see [`allocate_linear_scan`]).
    pub caller_saved: bool,
}

/// The set of registers the allocator may hand out, in preference order.
///
/// Deliberately a *value*, not a constant: the platform file is what the
/// compiler uses, but a test needs to be able to say "two registers, both
/// caller-saved" and get a deterministic, over-pressure allocation out of it.
/// Every heuristic below is written against this type, so the small-file tests
/// exercise the same code the real one does.
#[derive(Clone, Debug, Default)]
pub struct RegFile {
    regs: Vec<RegSpec>,
}

impl RegFile {
    /// A register file from an explicit list, in preference order (earlier
    /// registers are handed out first). Duplicates are dropped, keeping the
    /// first occurrence, so a caller cannot accidentally double-count a bank.
    pub fn from_specs(specs: impl IntoIterator<Item = RegSpec>) -> RegFile {
        let mut regs: Vec<RegSpec> = Vec::new();
        for spec in specs {
            if !regs.iter().any(|r| r.reg == spec.reg) {
                regs.push(spec);
            }
        }
        RegFile { regs }
    }

    /// The x86-64 file this crate's optimizing backend could allocate over.
    ///
    /// GP: [`LOCAL_REGS`] (callee-saved on the host ABI — R12–R15 + RBX, plus
    /// RSI/RDI on Win64) followed by R8/R9, which mirror `x64.rs`'s
    /// `SCRATCH_REGS` and are caller-saved on both ABIs. RAX/RCX/RDX/R10/R11
    /// are **excluded**: `ir_lower` uses them as fixed scratch for essentially
    /// every opcode, so they are modelled as clobbers rather than as allocatable
    /// (see [`MachineModel::for_graph`]).
    ///
    /// XMM: [`LOCAL_XMMS`] (XMM8–XMM15). Win64 makes XMM6–15 callee-saved; the
    /// SysV ABI makes **every** XMM caller-saved, so on Linux/macOS no FP value
    /// survives a call in a register and every FP interval spanning a call is
    /// split there. That is not a limitation of this allocator, it is the ABI.
    pub fn x86_64() -> RegFile {
        let mut specs: Vec<RegSpec> = Vec::new();
        for &num in LOCAL_REGS.iter() {
            specs.push(RegSpec {
                reg: PhysReg::gp(num),
                caller_saved: false,
            });
        }
        // R8 / R9 — caller-saved on both Win64 and SysV.
        for num in [8u8, 9u8] {
            specs.push(RegSpec {
                reg: PhysReg::gp(num),
                caller_saved: true,
            });
        }
        let xmm_caller_saved = !cfg!(target_os = "windows");
        for &num in LOCAL_XMMS.iter() {
            specs.push(RegSpec {
                reg: PhysReg::xmm(num),
                caller_saved: xmm_caller_saved,
            });
        }
        RegFile::from_specs(specs)
    }

    /// Every allocatable register, in preference order.
    pub fn specs(&self) -> &[RegSpec] {
        &self.regs
    }

    /// How many registers of `class` this file offers.
    pub fn class_size(&self, class: RegClass) -> usize {
        self.regs.iter().filter(|r| r.reg.class == class).count()
    }

    /// The registers of `class`, in preference order.
    pub fn of_class(&self, class: RegClass) -> impl Iterator<Item = PhysReg> + '_ {
        self.regs
            .iter()
            .filter(move |r| r.reg.class == class)
            .map(|r| r.reg)
    }

    /// Is `reg` allocatable at all?
    pub fn contains(&self, reg: PhysReg) -> bool {
        self.regs.iter().any(|r| r.reg == reg)
    }

    /// Does a call destroy `reg`? `None` when `reg` is not in the file, which
    /// callers must treat as "not allocatable", never as "callee-saved".
    pub fn is_caller_saved(&self, reg: PhysReg) -> Option<bool> {
        self.regs
            .iter()
            .find(|r| r.reg == reg)
            .map(|r| r.caller_saved)
    }
}

/// A closed interval of linear emission positions.
///
/// The mirror of `ir_lower`'s private `LiveRange`, down to the
/// endpoint-inclusive overlap rule: a value whose last use is at `p` and a
/// value defined at `p` are treated as simultaneously live, because a node's
/// lowering may allocate its result before or after reading its operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PosRange {
    /// First position, inclusive.
    pub lo: usize,
    /// Last position, inclusive.
    pub hi: usize,
}

impl PosRange {
    /// Endpoint-inclusive overlap: `[4, 7]` and `[7, 9]` overlap.
    pub fn overlaps(self, other: PosRange) -> bool {
        self.lo <= other.hi && other.lo <= self.hi
    }

    /// Is `pos` inside this range?
    pub fn contains(self, pos: usize) -> bool {
        self.lo <= pos && pos <= self.hi
    }
}

/// How a value can be recomputed instead of reloaded.
///
/// A rematerializable value is never *stored*: its defining instruction is
/// cheaper than the round trip through memory, so evicting it from a register
/// costs a re-materialisation at the next use and nothing at the eviction
/// point. This is what makes constants nearly free to spill, and it is why the
/// eviction heuristic prefers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remat {
    /// `Op::Const` — a `mov reg, imm` with no memory traffic.
    Const(i64),
    /// `Op::ConstF` — the raw bit pattern; the backend materialises it via an
    /// integer immediate and a `movq`, still no memory traffic.
    ConstF(u64),
    /// `Op::Param` — a reload from the *incoming* argument slot, which the
    /// prologue wrote and which nothing in the method may overwrite. Costs one
    /// load, but never a store, and the slot exists whether or not we use it.
    Param(u16),
}

/// A value that must occupy a specific register at a specific position.
///
/// Two sources, in principle: the entry ABI (`Op::Param(i)` arrives in the
/// platform's i-th argument register — see [`MachineModel::pin_entry_params`])
/// and fixed-operand instructions (x86 `idiv` reads its dividend in RAX, a
/// variable shift reads its count in RCX). The second kind never reaches the
/// allocator today because `ir_lower` keeps RAX/RCX/RDX outside the allocatable
/// file and loads them from frame slots per opcode; those are modelled as
/// [`MachineModel::clobbers`] instead. The constraint machinery is here, and
/// verified, so an ABI-aware lowering does not have to invent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedConstraint {
    /// The value that must be in `reg`.
    pub node: NodeId,
    /// The position at which the requirement holds.
    pub pos: usize,
    /// The register it must be in.
    pub reg: PhysReg,
}

/// Everything about the target the allocator needs beyond the liveness model.
#[derive(Clone, Debug, Default)]
pub struct MachineModel {
    /// Registers the allocator may hand out.
    pub regs: RegFile,
    /// `(position, registers destroyed at that position)`. A register may not
    /// hold a value across a position at which it is clobbered.
    pub clobbers: Vec<(usize, Vec<PhysReg>)>,
    /// Values pinned to specific registers at specific positions.
    pub fixed: Vec<FixedConstraint>,
    /// Positions at which the collector may run and read the oop map. Sorted,
    /// deduplicated.
    pub safepoints: Vec<usize>,
    /// May a reference hold a register across one of those positions?
    ///
    /// **`false` (the default) is the only setting that is safe on its own.**
    /// The oop map names frame slots, so a collector can neither see nor
    /// relocate a reference held in a register, and a value whose range covers
    /// a safepoint is therefore refused a register outright.
    ///
    /// `true` says the CALLER discharges that obligation by other means, and
    /// there is exactly one such caller: `ir_lower`, whose register file is a
    /// write-through read cache over a frame slot the map does name. It keeps
    /// the home word authoritative and invalidates the cached copy at every
    /// point a collector could have run, so a reload after a collection reads
    /// the slot the collector updated. See `ir_lower::invalidate_ref_residency`
    /// — and note that it does NOT trust `safepoints` for that, because
    /// `ir_op_is_safepoint` is a model of oop-map publication sites and
    /// `ir_lower` also emits helper calls at ops that are not on that list.
    pub refs_may_cross_safepoints: bool,
}

/// Work budget for the liveness fixed point, in `nodes × blocks` units.
///
/// The same budget `ir_lower::plan_slots` uses, for the same reason: past this
/// the analysis is skipped, every value is treated as live for the whole method
/// and nothing is promoted to a register. A compiler must not turn a
/// pathological graph into a pathological compile.
const LIVE_MODEL_WORK_BUDGET: usize = 8_000_000;

/// Hard cap on liveness fixed-point iterations; mirrors
/// `ir_lower::SLOT_PLAN_MAX_ITERATIONS`. A truncated may-analysis
/// under-approximates liveness, so a non-converging fixed point discards the
/// answer (nothing is promoted) rather than using a partial one.
const LIVE_MODEL_MAX_ITERATIONS: usize = 256;

/// The liveness model the allocator runs on: positions, intervals, uses and
/// frequencies for one scheduled graph.
///
/// Produced by [`build_live_model`], which is total — it never panics and never
/// fails. When it cannot analyse a graph, `converged` is false and every value
/// is given the whole method as its range, which makes the allocator promote
/// nothing and leave the frame layout exactly as `ir_lower` would have it.
#[derive(Clone, Debug)]
pub struct LiveModel {
    /// `pos_of[id]` = the linear emission position of node `id`, or `None` when
    /// the schedule never places it.
    pub pos_of: Vec<Option<usize>>,
    /// `span[b]` = `(first position in block b, block b's outgoing-edge
    /// position)`. Phi arguments are consumed at the second element.
    pub span: Vec<(usize, usize)>,
    /// One past the last position.
    pub total_positions: usize,
    /// `block_of_pos[p]` = the block position `p` belongs to.
    pub block_of_pos: Vec<usize>,
    /// `wants_loc[id]` = node `id` produces a value that needs a location.
    /// Exactly `ir_lower`'s `wants_slot`.
    pub wants_loc: Vec<bool>,
    /// `range[id]` = the live interval of node `id`, or `None` when
    /// `!wants_loc[id]`.
    pub range: Vec<Option<PosRange>>,
    /// `class[id]` = which register bank `id`'s value belongs to.
    pub class: Vec<Option<RegClass>>,
    /// `is_ref[id]` = node `id` produces an object reference.
    pub is_ref: Vec<bool>,
    /// `pinned[id]` = node `id` may never share a frame slot, and is never
    /// promoted to a register: phis (whose home the edge copies write) and any
    /// value a deopt frame names. Mirrors `ir_lower`'s `SlotClass::Pinned`.
    pub pinned: Vec<bool>,
    /// `uses[id]` = the sorted, deduplicated positions at which `id`'s value is
    /// *read* — direct operand reads plus the predecessor-edge reads of phi
    /// arguments. Does not include the definition.
    pub uses: Vec<Vec<usize>>,
    /// `weight[id]` = the loop-frequency-weighted use count, using the same
    /// [`LOOP_WEIGHT_PER_DEPTH`] model the bytecode allocator above uses.
    pub weight: Vec<u64>,
    /// `loop_depth[b]` = nesting depth of block `b`.
    pub loop_depth: Vec<u32>,
    /// `carried[id]` = this value's live range spans a BACK EDGE, i.e. it is
    /// live on entry to an iteration and still live at the end of one.
    ///
    /// The distinction the spill heuristic could not otherwise draw. Classic
    /// linear scan evicts the interval whose next use is FURTHEST away, which
    /// is right when the reload is paid once — and exactly wrong here, because
    /// a loop-carried value's "far" next use is across the back edge and
    /// evicting it costs a store and a reload on EVERY ITERATION. Loop-depth
    /// weighting cannot separate the two: a loop-carried phi and a temporary in
    /// the same loop body sit at the same depth, and the phi usually has FEWER
    /// uses, so it scored as the better victim of the two.
    pub carried: Vec<bool>,
    /// Maximum number of intervals covering any one position. The floor a
    /// perfect allocation could reach; reported, never sized from.
    pub peak_live: usize,
    /// False when the fixed point was skipped or did not converge. Every range
    /// is then the whole method and nothing is promotable.
    pub converged: bool,
}

/// Does this op define a value that needs a machine location?
///
/// **Verbatim mirror of `ir_lower::op_defines_result_slot`.** The two lists
/// must stay in lockstep: a value this predicate claims exists but `ir_lower`
/// never allocates for has no home to spill to, and a value `ir_lower`
/// allocates for but this predicate omits is invisible to the interference
/// check.
///
/// # The comment that guarded against drift WAS the drift
///
/// This paragraph used to read "including its omissions (`I2B`/`I2C`/`I2S`,
/// `ArrayLength`, `NewArray` are absent there and absent here)". `I2B`/`I2C`/
/// `I2S` were and are absent from both. **`ArrayLength` and `NewArray` were
/// present in `ir_lower`'s list the whole time**, so the two disagreed on every
/// method containing an `arraylength` or a `newarray` — which is every counted
/// loop written `for (i = 0; i < a.length; i++)`.
///
/// The consequence was silent and total: `plan_register_residency`'s agreement
/// check compares `wants_loc` against `node_color`, and a single disagreement
/// declines register residency **for the whole method**. With
/// `CRATONVM_DBG_IR_LINEAR_SCAN=1` that showed as *"refused: liveness and
/// colourer disagree about which values want a home"* on 5 of 5 array-touching
/// probe kernels, while `BinTrees.itemCheck` — which touches no array —
/// promoted 7 values fine. The check did its job; nothing was miscompiled, and
/// the optimization was simply never available where arrays are.
///
/// The lockstep claim is enforced now rather than asserted:
/// `the_two_value_defining_enumerations_agree` in `ir_lower`'s tests reads both
/// function bodies out of the source and compares the sets.
fn ir_op_defines_value(op: &Op) -> bool {
    matches!(
        op,
        Op::Const(_)
            | Op::ConstF(_)
            | Op::Param(_)
            | Op::Phi
            | Op::ScalarIntrinsic(_)
            | Op::Unbox { .. }
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
            | Op::I2L
            | Op::L2I
            | Op::I2F
            | Op::I2D
            | Op::L2F
            | Op::L2D
            | Op::F2I
            | Op::F2L
            | Op::F2D
            | Op::D2I
            | Op::D2L
            | Op::D2F
            | Op::Load(_)
            | Op::ArrayLoad(_)
            | Op::ArrayStore(_)
            // Both of these are in `op_defines_result_slot` and were missing
            // here, which is what made every array-touching method decline
            // register residency. See this function's doc comment.
            | Op::ArrayLength
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::Call { .. }
            // cov-01 — mirrors `ir_lower::op_defines_result_slot`.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::LambdaIntToDouble
            // cov-05 — mirrors `ir_lower::op_defines_result_slot`.
            | Op::InstanceOf { .. }
            | Op::CheckCast { .. }
    )
}

/// Does this op reach a safepoint at which the collector may read the oop map?
///
/// Conservative superset of what `ir_lower` emits (`emit_safepoint_map` at
/// `Op::Call`, `emit_safepoint_poll` at back edges): allocation and the lambda
/// adapter can also transfer to the runtime, and `Op::Guard` transfers to the
/// deopt trampoline. Over-approximating costs promotions, never correctness.
fn ir_op_is_safepoint(op: &Op) -> bool {
    matches!(
        op,
        Op::Call { .. }
            | Op::New { .. }
            | Op::NewArray { .. }
            // cov-01: the two `ldc` constants always call a helper; `getstatic`
            // does whenever the class is not already initialized at compile
            // time. Over-approximating a site that took the direct load costs a
            // promotion, never correctness — see this function's doc.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::LambdaIntToDouble
            | Op::Guard { .. }
            // cov-05: `jit_instanceof`/`jit_checkcast` can allocate a Class
            // mirror on first touch, same as the three above.
            | Op::InstanceOf { .. }
            | Op::CheckCast { .. }
            // `ir_lower` publishes an oop map before each of these: a
            // contended monitor acquire parks the thread for a whole
            // collection, and `jit_aastore` allocates its
            // `ArrayStoreException`.
            | Op::MonitorEnter
            | Op::MonitorExit
            | Op::ArrayStore(crate::ir::MemKind::Ref)
    )
}

/// Does this op destroy the caller-saved registers?
///
/// A **superset** of the ops that are obviously calls, because `ir_lower`
/// lowers three more to a `MOV RAX,helper ; CALL RAX` that returns into the
/// body — and a caller-saved register live across a call the model does not
/// know about is silent wrong code, not a missed optimization:
///
/// * `Op::Rem` on `Float`/`Double` calls `jit_frem` / `jit_drem`
///   (`ir_lower.rs`, the `Op::Rem` arm — IEEE remainder has no single SSE
///   instruction);
/// * `Op::Load(_)` calls `jit_getfield` whenever the helper is wired, which is
///   every real compile (`compact_ref_fields_enabled` makes the inline
///   displacement wrong, and a `Ref` field has no correct inline lowering at
///   all);
/// * `Op::Store(_)` calls `jit_putfield_int` for the same reason.
///
/// `Op::Guard` is deliberately **absent**. Its failure edge jumps to the shared
/// deopt stub, which calls `ir_deopt_entry` and then runs the epilogue — it
/// never returns into the body, so what it destroys cannot be read again.
/// Treating every guard as a clobber would deny a register to every value in a
/// bounds-checked loop, which is most of them, for no soundness gain.
///
/// This predicate covers ops. It cannot cover the calls a backend emits
/// *between* ops — `ir_lower`'s cooperative safepoint poll calls its slow path
/// on a loop back edge, at the block's edge position and not at any node. A
/// backend allocating over caller-saved registers must add those positions to
/// [`MachineModel::clobbers`] itself; `ir_lower::ir_lower_machine_model` does.
fn ir_op_is_call(op: &Op) -> bool {
    matches!(
        op,
        Op::Call { .. }
            | Op::New { .. }
            | Op::NewArray { .. }
            // cov-01 — same superset reasoning as `Op::Load`/`Op::Store` above:
            // these lower to `MOV RAX,helper ; CALL RAX` and return into the
            // body, so a caller-saved register live across one must not be
            // assumed to survive. `Op::LoadStatic`'s direct route emits no call
            // at all, and being listed here merely denies it a register it did
            // not need.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::LambdaIntToDouble
            | Op::Rem
            | Op::Load(_)
            | Op::Store(_)
            // cov-05: `MOV RAX,jit_instanceof|jit_checkcast ; CALL RAX`,
            // returns into the body — same superset reasoning as the other
            // helper calls above.
            | Op::InstanceOf { .. }
            | Op::CheckCast { .. }
            // `emit_monitor_stub` calls `jit_monitor_enter`/`jit_monitor_exit`,
            // and a reference `ArrayStore` is `MOV RAX, jit_aastore ; CALL RAX`;
            // all three return into the body. They were missing, so a
            // float/double interval in a caller-saved XMM (XMM2-5 on both ABIs,
            // XMM2-7 on System V) was not split across the Rust helper.
            | Op::MonitorEnter
            | Op::MonitorExit
            | Op::ArrayStore(crate::ir::MemKind::Ref)
    )
}

/// Set bit `i`; out-of-range indices are ignored rather than panicking (the
/// model runs on a graph nothing has verified yet).
#[inline]
fn ls_bit_set(bits: &mut [u64], i: usize) {
    if let Some(word) = bits.get_mut(i / 64) {
        *word |= 1u64 << (i % 64);
    }
}

/// Is bit `i` set?
#[inline]
fn ls_bit_get(bits: &[u64], i: usize) -> bool {
    bits.get(i / 64)
        .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
}

/// Visit every set bit, in increasing order.
#[inline]
fn ls_bits_for_each(bits: &[u64], mut f: impl FnMut(usize)) {
    for (w, &word) in bits.iter().enumerate() {
        let mut rest = word;
        while rest != 0 {
            let b = rest.trailing_zeros() as usize;
            f(w * 64 + b);
            rest &= rest - 1;
        }
    }
}

/// Resolve the block that produces control token `ctrl`, so a phi's value
/// inputs are attributed to exactly the predecessor block `ir_lower`'s
/// `emit_phi_copies` copies them from. Mirror of `ir_lower::ctrl_block_of`.
fn ls_ctrl_block_of(graph: &Graph, schedule: &Schedule, mut ctrl: NodeId) -> Option<usize> {
    for _ in 0..graph.nodes.len() {
        if ctrl == NO_NODE {
            return None;
        }
        let blk = *schedule.node_to_block.get(ctrl as usize)?;
        if blk != usize::MAX && schedule.blocks.get(blk).map(|b| b.ctrl) == Some(ctrl) {
            return Some(blk);
        }
        let node = graph.nodes.get(ctrl as usize)?;
        match node.inputs.first() {
            Some(&next) if next != ctrl => ctrl = next,
            _ => return None,
        }
    }
    None
}

/// Make a phi interfere with the sources read on its incoming edges --
/// **DEFAULT ON** since 2026-09-06; `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=0` is
/// the kill switch.
///
/// It shipped opt-in for one day because it is a register-pressure change and
/// the first numbers came from a synthetic probe. They were the wrong numbers:
/// that probe is built to CONTAIN the aliasing, so extending intervals there
/// genuinely adds interference (+2 spills, +3 reloads). On code that does not
/// alias, extending a phi's interval by one position changes nothing, and the
/// measurement that matters is the deterministic one.
///
/// `CratonBench`, all seven phases, allocator counters (load-proof, unlike
/// wall clock on a shared host):
///
/// ```text
///   arithmetic fib sieve matrix hashmap stringregex   IDENTICAL off vs on
///   bintrees   splits 23->21  scan_reloads 10->7  reg_publishes 4->6
/// ```
///
/// Six of seven byte-identical; the seventh allocates BETTER. The timing arm
/// over the same phases spread 0.910-1.035 with `publish_deferred` at zero on
/// every one of them -- i.e. the flag provably could not have done anything,
/// so that spread is this host's noise floor and not a cost.
///
/// Engagement is real and not synthetic. `publish_deferred` with the flag off,
/// on netty: `DefaultPromiseTest` 8, `ByteBufUtilTest` 2 -- both 0 with it on.
/// That is the aliasing actually occurring in shipped code, which is also what
/// says the two downstream guards (`emit_phi_copies`'s deferral screen and
/// `gp_reg_owner`'s reader interlock) are load-bearing rather than theoretical.
/// They stay: this removes the CAUSE, they catch anything that still mints one.
fn phi_edge_interfere_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_PHI_EDGE_INTERFERE") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// Build the liveness model for one scheduled graph.
///
/// Total: never panics, never fails. See [`LiveModel`] for the fallback when
/// the analysis is skipped or does not converge.
pub fn build_live_model(graph: &Graph, schedule: &Schedule) -> LiveModel {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();

    // ── 1. Which values need a location ──────────────────────────────
    let mut wants_loc = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::Phi) {
            wants_loc[id] = true;
        }
    }
    for block in &schedule.blocks {
        for &nid in &block.nodes {
            if let Some(node) = graph.nodes.get(nid as usize) {
                if ir_op_defines_value(&node.op) {
                    wants_loc[nid as usize] = true;
                }
            }
        }
    }

    // ── 2. Linear positions and block spans ──────────────────────────
    let mut pos_of: Vec<Option<usize>> = vec![None; n];
    let mut span: Vec<(usize, usize)> = Vec::with_capacity(nb);
    let mut block_of_pos: Vec<usize> = Vec::new();
    let mut seq = 0usize;
    for (b, block) in schedule.blocks.iter().enumerate() {
        let start = seq;
        for &nid in &block.nodes {
            if let Some(cell) = pos_of.get_mut(nid as usize) {
                *cell = Some(seq);
            }
            block_of_pos.push(b);
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = pos_of.get_mut(term as usize) {
                *cell = Some(seq);
            }
            block_of_pos.push(b);
            seq += 1;
        }
        // One position past the block's last instruction: the outgoing edge,
        // where `emit_phi_copies` reads this block's phi arguments.
        let end = seq;
        block_of_pos.push(b);
        seq += 1;
        span.push((start, end));
    }
    let total_positions = seq;

    // ── 3. Loop depth and per-position frequency weight ──────────────
    //
    // A back edge is a successor at or before its own block index — exactly the
    // test `ir_lower::lower_block` uses to decide where to place a safepoint
    // poll, so the two agree about what a loop is.
    let mut loop_depth = vec![0u32; nb];
    for (b, block) in schedule.blocks.iter().enumerate() {
        for &s in &block.successors {
            if s <= b && s < nb {
                for depth in loop_depth.iter_mut().take(b + 1).skip(s) {
                    *depth = depth.saturating_add(1);
                }
            }
        }
    }
    let pos_weight = |p: usize| -> u64 {
        let b = block_of_pos.get(p).copied().unwrap_or(0);
        let depth = loop_depth.get(b).copied().unwrap_or(0);
        LOOP_WEIGHT_PER_DEPTH
            .checked_pow(depth)
            .unwrap_or(MAX_LOOP_DEPTH_WEIGHT)
            .min(MAX_LOOP_DEPTH_WEIGHT) as u64
    };

    // ── 4. Pinned classes ────────────────────────────────────────────
    //
    // Same two sources `plan_slots` pins: phis (their home is what the edge
    // copies write, and `zero_ref_phi_slots` publishes `Ref` phi slots as GC
    // roots from the prologue) and every value a deopt frame names (the slot
    // must still hold the value at *any* recorded bci, which is not a property
    // a register allocator can establish).
    let mut pinned = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if wants_loc[id] && matches!(node.op, Op::Phi) {
            pinned[id] = true;
        }
    }
    for sp in &graph.safepoints {
        for &v in sp.locals.iter().chain(sp.stack.iter()) {
            if v != NO_NODE {
                if let Some(slot) = pinned.get_mut(v as usize) {
                    *slot = true;
                }
            }
        }
    }

    // ── 5. Uses, definitions and the backward fixed point ────────────
    let mut lo = vec![usize::MAX; n];
    let mut hi = vec![0usize; n];
    let mut uses: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut weight = vec![0u64; n];
    let mut converged = n.saturating_mul(nb) <= LIVE_MODEL_WORK_BUDGET;

    if converged {
        let words = n.div_ceil(64).max(1);
        let mut def_bits = vec![0u64; words * nb];
        let mut use_bits = vec![0u64; words * nb];
        let mut phi_out_bits = vec![0u64; words * nb];

        let mut local_def = vec![0u64; words];
        for b in 0..nb {
            let base = b * words;
            local_def.fill(0);
            let block = &schedule.blocks[b];
            for u in block.nodes.iter().copied().chain(block.terminator) {
                let ui = u as usize;
                let node = match graph.nodes.get(ui) {
                    Some(node) => node,
                    None => continue,
                };
                let upos = pos_of.get(ui).copied().flatten().unwrap_or(span[b].0);
                // A phi's value inputs are NOT read here — the predecessor's
                // edge copy reads them, below; `inputs[0]` is control.
                if !matches!(node.op, Op::Phi) {
                    for &inp in &node.inputs {
                        let ii = inp as usize;
                        if inp == NO_NODE || ii >= n || !wants_loc[ii] {
                            continue;
                        }
                        lo[ii] = lo[ii].min(upos);
                        hi[ii] = hi[ii].max(upos);
                        uses[ii].push(upos);
                        weight[ii] = weight[ii].saturating_add(pos_weight(upos));
                        if !ls_bit_get(&local_def, ii) {
                            ls_bit_set(&mut use_bits[base..base + words], ii);
                        }
                    }
                }
                if wants_loc[ui] {
                    ls_bit_set(&mut local_def, ui);
                    ls_bit_set(&mut def_bits[base..base + words], ui);
                    lo[ui] = lo[ui].min(upos);
                    hi[ui] = hi[ui].max(upos);
                }
            }
        }

        // Phi edge copies: a phi's k-th value input is read at the outgoing-edge
        // position of the k-th predecessor, not at the phi.
        let mut phis_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (id, node) in graph.nodes.iter().enumerate() {
            if !matches!(node.op, Op::Phi) {
                continue;
            }
            if let Some(&merge) = node.inputs.first() {
                if merge != NO_NODE {
                    phis_of.entry(merge).or_default().push(id as NodeId);
                }
            }
        }
        for succ in 0..nb {
            let merge_ctrl = schedule.blocks[succ].ctrl;
            let merge_node = match graph.nodes.get(merge_ctrl as usize) {
                Some(m) if matches!(m.op, Op::Merge | Op::Region) => m,
                _ => continue,
            };
            let phis = match phis_of.get(&merge_ctrl) {
                Some(phis) => phis,
                None => continue,
            };
            for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                let pred = match ls_ctrl_block_of(graph, schedule, ctrl_in) {
                    Some(pred) if pred < nb => pred,
                    _ => continue,
                };
                let at = span[pred].1;
                for &pid in phis {
                    let v = match graph
                        .nodes
                        .get(pid as usize)
                        .and_then(|p| p.inputs.get(k + 1))
                    {
                        Some(&v) if v != NO_NODE => v,
                        _ => continue,
                    };
                    let vi = v as usize;
                    if vi >= n || !wants_loc[vi] {
                        continue;
                    }
                    lo[vi] = lo[vi].min(at);
                    hi[vi] = hi[vi].max(at);
                    uses[vi].push(at);
                    weight[vi] = weight[vi].saturating_add(pos_weight(at));
                    ls_bit_set(&mut phi_out_bits[pred * words..(pred + 1) * words], vi);

                    // OPT-IN (`CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=1`): the phi
                    // is DEFINED at this same position, so extend its interval
                    // back to it and let the allocator see that it overlaps
                    // every source read on this edge.
                    //
                    // Without this the two do not interfere by the allocator's
                    // reckoning -- a phi's range begins at its def in the merge
                    // block, a dying source's ends here -- so one register can
                    // serve both, and `ir_lower::emit_copy_op`'s publish then
                    // overwrites a register a later copy still has to read.
                    // That is the 2026-09-05 wrong-code bug two lanes fixed
                    // downstream (`emit_phi_copies`'s deferral screen, and
                    // `gp_reg_owner`'s reader interlock). This is the same bug
                    // taken at the source, where the false "no interference"
                    // is minted.
                    //
                    // Only `lo` moves, and deliberately: the phi is not USED
                    // here, so pushing a use would distort the spill
                    // heuristics, and setting `phi_out_bits` would make the phi
                    // live-OUT of a block that does not define it -- which the
                    // backward dataflow below would then propagate live-IN
                    // through every predecessor, turning a one-position
                    // extension into a whole-CFG one.
                    if phi_edge_interfere_enabled() {
                        let pi = pid as usize;
                        if pi < n && wants_loc[pi] {
                            lo[pi] = lo[pi].min(at);
                            hi[pi] = hi[pi].max(at);
                        }
                    }
                }
            }
        }

        let mut live_in = vec![0u64; words * nb];
        let mut live_out = vec![0u64; words * nb];
        let mut scratch = vec![0u64; words];
        let mut settled = false;
        for _ in 0..LIVE_MODEL_MAX_ITERATIONS {
            let mut changed = false;
            for b in (0..nb).rev() {
                let base = b * words;
                scratch.copy_from_slice(&phi_out_bits[base..base + words]);
                for &s in &schedule.blocks[b].successors {
                    if s >= nb {
                        continue;
                    }
                    for w in 0..words {
                        scratch[w] |= live_in[s * words + w];
                    }
                }
                for w in 0..words {
                    let merged_out = live_out[base + w] | scratch[w];
                    if merged_out != live_out[base + w] {
                        live_out[base + w] = merged_out;
                        changed = true;
                    }
                    let entering = use_bits[base + w] | (merged_out & !def_bits[base + w]);
                    let merged_in = live_in[base + w] | entering;
                    if merged_in != live_in[base + w] {
                        live_in[base + w] = merged_in;
                        changed = true;
                    }
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        converged = settled;

        // Widen each range over every block it is live in. This is where loops
        // are handled: a value used inside a loop is live-out of every block on
        // the back edge's path, so its range covers the whole loop without the
        // model ever knowing what a loop is.
        if converged {
            for b in 0..nb {
                let base = b * words;
                let (start, end) = span[b];
                ls_bits_for_each(&live_in[base..base + words], |v| {
                    if v < n && wants_loc[v] {
                        lo[v] = lo[v].min(start);
                    }
                });
                ls_bits_for_each(&live_out[base..base + words], |v| {
                    if v < n && wants_loc[v] {
                        hi[v] = hi[v].max(end);
                    }
                });
            }
        }
    }

    // ── 6. Finalise ──────────────────────────────────────────────────
    let whole_method = PosRange {
        lo: 0,
        hi: total_positions.saturating_sub(1),
    };
    let mut range: Vec<Option<PosRange>> = vec![None; n];
    let mut class: Vec<Option<RegClass>> = vec![None; n];
    let mut is_ref = vec![false; n];
    for id in 0..n {
        if !wants_loc[id] {
            continue;
        }
        if !converged {
            pinned[id] = true;
        }
        range[id] = Some(if lo[id] == usize::MAX || !converged {
            whole_method
        } else {
            PosRange {
                lo: lo[id],
                hi: hi[id].max(lo[id]),
            }
        });
        let ty = graph.nodes[id].ty;
        class[id] = RegClass::of(ty);
        is_ref[id] = ty == IrType::Ref;
        uses[id].sort_unstable();
        uses[id].dedup();
    }

    // Peak simultaneous liveness, by sweeping the interval endpoints.
    let mut delta = vec![0i64; total_positions + 2];
    for r in range.iter().flatten() {
        if r.lo < delta.len() && r.hi + 1 < delta.len() {
            delta[r.lo] += 1;
            delta[r.hi + 1] -= 1;
        }
    }
    let mut running = 0i64;
    let mut peak_live = 0i64;
    for d in &delta {
        running += d;
        peak_live = peak_live.max(running);
    }

    // Which positions are back edges: the outgoing edge of a block that
    // branches to itself or to any earlier block. Same test
    // `MachineModel::for_graph` uses to place its safepoint polls, because it
    // is the same event — the point an iteration ends.
    let mut back_edges: Vec<usize> = Vec::new();
    for (b, block) in schedule.blocks.iter().enumerate() {
        if block.successors.iter().any(|&s| s <= b) {
            if let Some(&(_, edge)) = span.get(b) {
                back_edges.push(edge);
            }
        }
    }
    let mut carried = vec![false; n];
    if !back_edges.is_empty() {
        for (id, r) in range.iter().enumerate() {
            if let Some(r) = r {
                carried[id] = back_edges.iter().any(|&e| r.lo <= e && e <= r.hi);
            }
        }
    }

    LiveModel {
        pos_of,
        span,
        total_positions,
        block_of_pos,
        wants_loc,
        range,
        class,
        is_ref,
        pinned,
        uses,
        weight,
        loop_depth,
        carried,
        peak_live: peak_live.max(0) as usize,
        converged,
    }
}

impl LiveModel {
    /// The first read of `node` at or after `pos`, if any.
    pub fn next_use_at_or_after(&self, node: NodeId, pos: usize) -> Option<usize> {
        let uses = self.uses.get(node as usize)?;
        let idx = uses.partition_point(|&u| u < pos);
        uses.get(idx).copied()
    }

    /// The first read of `node` strictly after `pos`, if any.
    pub fn next_use_after(&self, node: NodeId, pos: usize) -> Option<usize> {
        self.next_use_at_or_after(node, pos.saturating_add(1))
    }

    /// Drop the **deopt-frame** pins, keeping the phi pins. Returns how many
    /// values were released.
    ///
    /// ## Why this exists
    ///
    /// `build_step_4` pins every value a `SafepointSnapshot` names, because the
    /// home word must hold the value at any recorded bci and a general
    /// allocator cannot establish that. On a graph built from real bytecode
    /// that is *every value*: `ir::IrBuilder` records a snapshot of the locals
    /// and the operand stack at **every bytecode boundary**, so a temporary
    /// sitting on the stack across one boundary — which is every temporary —
    /// is named by a frame state. Measured on any `IrBuilder` graph, the pin
    /// set is the whole value set and [`allocate_linear_scan`] promotes
    /// nothing. The allocator is not conservative here, it is inert.
    ///
    /// ## When it is sound to release them
    ///
    /// Exactly when the consumer is **write-through**: it emits the home store
    /// at every definition regardless of the register it also assigns, so the
    /// frame image is complete at every instruction boundary and a deopt frame
    /// built from home words is bit-identical to the one a register-free
    /// lowering would build. `ir_lower`'s wiring is that consumer — see
    /// `docs/jit/linear-scan-wiring.md`.
    ///
    /// ## What a caller gives up
    ///
    /// [`Allocation::stack_slot`] and [`Allocation::stack_slots`]. Releasing a
    /// pin also moves the value out of the `Pinned` home pool, so the home
    /// colouring this model produces may put two deopt-named values in one
    /// word — correct for their live ranges, and NOT what a deopt frame
    /// reconstructor expecting one word per named value assumes. A caller that
    /// calls this **must** take its home layout from somewhere else
    /// (`ir_lower` takes it from `plan_slots`, which keeps every pin). Nothing
    /// enforces that; it is why this is an explicit call and not a default.
    ///
    /// A no-op on an unconverged model: there every value is pinned *because
    /// nothing was analysed*, which no write-through property repairs.
    pub fn release_deopt_pins(&mut self, graph: &Graph) -> usize {
        if !self.converged {
            return 0;
        }
        let mut released = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !self.pinned.get(id).copied().unwrap_or(false) {
                continue;
            }
            // A phi's home is what the edge copies write; its pin is structural
            // and has nothing to do with deopt.
            if matches!(node.op, Op::Phi) {
                continue;
            }
            self.pinned[id] = false;
            released += 1;
        }
        released
    }
    /// Release the STRUCTURAL pin on every phi, for a consumer that publishes
    /// a promoted phi's register itself at every incoming edge (`ir_lower`'s
    /// `emit_phi_copies` does, behind `CRATONVM_JIT_IR_PHI_RESIDENCY`). The
    /// pin exists because no definition arm ever writes a phi; a consumer
    /// that supplies that write at the edges has discharged the reason.
    ///
    /// Same caveat as [`Self::release_deopt_pins`]: the home layout must
    /// still come from a colouring that keeps every phi in its own word,
    /// which `ir_lower::plan_slots` does. A no-op on an unconverged model.
    pub fn release_phi_pins(&mut self, graph: &Graph) -> usize {
        if !self.converged {
            return 0;
        }
        let mut released = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !matches!(node.op, Op::Phi) {
                continue;
            }
            if self.pinned.get(id).copied().unwrap_or(false) {
                self.pinned[id] = false;
                released += 1;
            }
        }
        released
    }
}

impl MachineModel {
    /// Derive the machine model for one scheduled graph over `regs`.
    ///
    /// Produces the clobber set (every call position destroys every
    /// caller-saved register in the file; `Div`/`Rem` destroy RAX/RDX and a
    /// variable shift destroys RCX, which matter only if a caller puts those in
    /// the file) and the safepoint set. The fixed-constraint list starts empty;
    /// see [`MachineModel::pin_entry_params`].
    ///
    /// **The clobber set is derived from ops alone.** It is complete for calls
    /// a *node* makes (`ir_op_is_call` is a superset of those, helper calls
    /// included) and necessarily silent about calls a backend emits *between*
    /// nodes. `ir_lower`'s cooperative safepoint poll is one: on a loop back
    /// edge it calls its slow path at the block's outgoing-edge position, which
    /// belongs to no node. This function marks that position a **safepoint**
    /// but not a clobber. A backend that allocates over caller-saved registers
    /// must add its own emission's clobbers on top — see
    /// `ir_lower::ir_lower_machine_model`, which does exactly that.
    pub fn for_graph(graph: &Graph, schedule: &Schedule, live: &LiveModel, regs: RegFile) -> Self {
        let caller_saved: Vec<PhysReg> = regs
            .specs()
            .iter()
            .filter(|s| s.caller_saved)
            .map(|s| s.reg)
            .collect();
        let mut clobbers: Vec<(usize, Vec<PhysReg>)> = Vec::new();
        let mut safepoints: Vec<usize> = Vec::new();

        for (b, block) in schedule.blocks.iter().enumerate() {
            for &nid in block.nodes.iter().chain(block.terminator.iter()) {
                let node = match graph.nodes.get(nid as usize) {
                    Some(node) => node,
                    None => continue,
                };
                let pos = match live.pos_of.get(nid as usize).copied().flatten() {
                    Some(pos) => pos,
                    None => continue,
                };
                if ir_op_is_safepoint(&node.op) {
                    safepoints.push(pos);
                }
                let mut destroyed: Vec<PhysReg> = Vec::new();
                if ir_op_is_call(&node.op) {
                    destroyed.extend_from_slice(&caller_saved);
                }
                // Fixed-operand x86 instructions. Only meaningful when a caller
                // puts these registers in the file; `RegFile::x86_64` does not.
                match node.op {
                    Op::Div | Op::Rem => {
                        destroyed.push(PhysReg::gp(0)); // RAX
                        destroyed.push(PhysReg::gp(2)); // RDX
                    }
                    Op::Shl | Op::Shr | Op::UShr => destroyed.push(PhysReg::gp(1)), // RCX
                    _ => {}
                }
                destroyed.retain(|r| regs.contains(*r));
                if !destroyed.is_empty() {
                    destroyed.sort_unstable();
                    destroyed.dedup();
                    clobbers.push((pos, destroyed));
                }
            }
            // `lower_terminator` / the fall-through edge emit a safepoint poll
            // on any back edge, which publishes the oop map exactly as a call
            // site does. Both the terminator position and the edge position are
            // covered because the poll sits between them.
            if block.successors.iter().any(|&s| s <= b) {
                if let Some(&(_, edge)) = live.span.get(b) {
                    safepoints.push(edge.saturating_sub(1));
                    safepoints.push(edge);
                }
            }
        }

        clobbers.sort_by_key(|(pos, _)| *pos);
        safepoints.sort_unstable();
        safepoints.dedup();
        MachineModel {
            regs,
            clobbers,
            fixed: Vec::new(),
            safepoints,
            refs_may_cross_safepoints: false,
        }
    }

    /// Pin each `Op::Param(i)` to the register the entry ABI delivers it in.
    ///
    /// `abi` is the platform's integer argument register list *as the callee
    /// sees it* — `ir_lower::ENTRY_ABI_REGS`, offset by one when the callee
    /// takes the hidden VM-context pointer. Parameters whose ABI register is
    /// not in the allocatable file are skipped: the prologue spills those to
    /// their frame slot and the allocator has nothing to say about them.
    pub fn pin_entry_params(&mut self, graph: &Graph, live: &LiveModel, abi: &[PhysReg]) {
        for (id, node) in graph.nodes.iter().enumerate() {
            let Op::Param(i) = node.op else { continue };
            if !live.wants_loc.get(id).copied().unwrap_or(false) {
                continue;
            }
            let Some(&reg) = abi.get(i as usize) else {
                continue;
            };
            if !self.regs.contains(reg) || RegClass::of(node.ty) != Some(reg.class) {
                continue;
            }
            self.fixed.push(FixedConstraint {
                node: id as NodeId,
                pos: 0,
                reg,
            });
        }
        self.fixed.sort_by_key(|f| (f.pos, f.reg, f.node));
    }

    /// Every register destroyed at `pos`.
    fn clobbered_at(&self, pos: usize) -> &[PhysReg] {
        match self.clobbers.binary_search_by_key(&pos, |(p, _)| *p) {
            Ok(idx) => &self.clobbers[idx].1,
            Err(_) => &[],
        }
    }

    /// Is any position in `range` a safepoint?
    fn range_covers_safepoint(&self, range: PosRange) -> bool {
        let idx = self.safepoints.partition_point(|&p| p < range.lo);
        self.safepoints.get(idx).is_some_and(|&p| p <= range.hi)
    }
}

// ── Allocation result ────────────────────────────────────────────────

/// Where a value is at some position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueLoc {
    /// In a physical register.
    Reg(PhysReg),
    /// In its home frame slot (an 8-byte word index, as `SlotPlan::node_color`
    /// numbers them).
    Slot(u32),
}

/// One stretch of a value's life spent in one location.
///
/// A value's segments tile its whole live range with no gaps and no overlaps —
/// [`verify_allocation`] checks exactly that. `reg == None` means "in the home
/// slot", which is the location `ir_lower` uses today for every value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The positions this segment covers.
    pub range: PosRange,
    /// The register held over `range`, or `None` for the home slot.
    pub reg: Option<PhysReg>,
}

/// What the backend must emit at a location transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillKind {
    /// Register → home slot. Counted in [`Allocation::spills`]. At most one per
    /// value: the IR is SSA, so once the home is written it stays correct.
    Store,
    /// Home slot → register. Counted in [`Allocation::reloads`].
    Load,
    /// Value recomputed into a register instead of being loaded. Counted in
    /// [`Allocation::remats`]; costs no memory traffic and no preceding store.
    Remat,
    /// Register → a different register. Counted in [`Allocation::reg_moves`].
    Move,
}

/// One transition the backend must materialise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpillEvent {
    /// The value.
    pub node: NodeId,
    /// For [`SpillKind::Store`], the **last** position at which the register
    /// still holds the value; the store may be sunk anywhere between the
    /// definition and here. For every other kind, the position at which the
    /// value must already be in its new location.
    pub pos: usize,
    /// What to emit.
    pub kind: SpillKind,
    /// Source register, when there is one.
    pub from: Option<PhysReg>,
    /// Destination register, when there is one.
    pub to: Option<PhysReg>,
}

/// The result of [`allocate_linear_scan`].
#[derive(Clone, Debug, Default)]
pub struct Allocation {
    /// `segments[id]` = node `id`'s location timeline, ordered and contiguous.
    /// Empty for nodes that produce no value.
    pub segments: Vec<Vec<Segment>>,
    /// `stack_slot[id]` = the home word index, or `None` when the value never
    /// touches memory. A drop-in replacement for `ir_lower`'s
    /// `SlotPlan::node_color`: same numbering, same Ref/Prim/pinned pool
    /// separation.
    pub stack_slot: Vec<Option<u32>>,
    /// Distinct home words the plan needs.
    pub stack_slots: usize,
    /// Register → memory transitions. Feeds `CompilationReport::spills`.
    pub spills: usize,
    /// Memory → register transitions. Feeds `CompilationReport::reloads`.
    pub reloads: usize,
    /// Values recomputed rather than reloaded. Reported separately so a
    /// dashboard cannot mistake a free re-materialisation for memory traffic.
    pub remats: usize,
    /// Register → register transitions.
    pub reg_moves: usize,
    /// Interval splits performed.
    pub splits: usize,
    /// Values that spent at least one position in a register.
    pub promoted: usize,
    /// Every transition, in position order.
    pub events: Vec<SpillEvent>,
    /// Copied from [`LiveModel::peak_live`] so a report has the pressure and
    /// the outcome in one place.
    pub peak_live: usize,
}

impl Allocation {
    /// Where is `node`'s value at `pos`? `None` when `pos` is outside its live
    /// range or the node produces no value.
    pub fn location_at(&self, node: NodeId, pos: usize) -> Option<ValueLoc> {
        let segs = self.segments.get(node as usize)?;
        let seg = segs.iter().find(|s| s.range.contains(pos))?;
        match seg.reg {
            Some(reg) => Some(ValueLoc::Reg(reg)),
            None => self
                .stack_slot
                .get(node as usize)
                .copied()
                .flatten()
                .map(ValueLoc::Slot),
        }
    }

    /// Does `node` ever occupy a register?
    pub fn is_promoted(&self, node: NodeId) -> bool {
        self.segments
            .get(node as usize)
            .is_some_and(|segs| segs.iter().any(|s| s.reg.is_some()))
    }

    /// The register `node` holds over its first segment, if any.
    pub fn first_reg(&self, node: NodeId) -> Option<PhysReg> {
        self.segments.get(node as usize)?.first()?.reg
    }
}

// ── Spill cost model ─────────────────────────────────────────────────

/// Scale factor for the eviction score, so integer division by the frequency
/// weight keeps useful resolution.
const SPILL_DISTANCE_SCALE: u64 = 1024;

/// How much a LOOP-CARRIED value's next-use distance is discounted when
/// choosing a spill victim -- **default 64, i.e. ON** since 2026-09-05;
/// `CRATONVM_JIT_LS_CARRY_RELIEF=0` restores the distance-only rule and is the
/// kill switch.
///
/// Off by default because it is not shown to PAY, not because it is wrong. It
/// moves the residency census deterministically (`resident=1` to `2`,
/// `split_or_spilled=3` to `2` on `probes/OsrTierBench.java`, saturating by
/// 64), and it changes register allocation on every IR compile — a real
/// behaviour change that has no measured benefit behind it yet. The timing arm
/// that would settle it was attempted at host load 22 on 8 cores and is not
/// usable; see `JIT_OPTIMIZATION.md`.
///
/// Not a tuning knob so much as a units correction. The score compares "how
/// long until this value is needed again", which prices a reload paid ONCE. A
/// value that crosses the back edge is needed again every iteration, so its
/// distance overstates its availability by roughly the trip count -- and the
/// heuristic then evicts precisely the values whose eviction is paid the most
/// times. Dividing keeps the ordering AMONG carried values intact while moving
/// all of them behind the uncarried ones.
fn ls_carry_relief() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        // 2026-09-05: DEFAULT 64, i.e. ON. `=0` restores the old
        // distance-only rule and is the kill switch.
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_LS_CARRY_RELIEF") {
            Ok(v) => v.trim().parse::<u64>().unwrap_or(64),
            Err(_) => 64,
        }
    })
}

/// Score handed to a rematerializable value, which is always the first thing
/// evicted: dropping it costs no store and its reload is an immediate.
const SPILL_SCORE_REMAT: u64 = u64::MAX;

/// Splits allowed before the allocator gives up.
///
/// Every split strictly advances the suffix's start, so the scan terminates
/// regardless; this budget is about *compile time*. A graph that needs more
/// splits than this over the given register file is one where every promotion
/// immediately costs a reload, i.e. one where linear scan buys nothing — so it
/// bails with [`BailoutReason::RegisterPressure`] rather than spending the
/// compile budget arriving at the frame layout `ir_lower` already had.
fn split_budget(intervals: usize) -> usize {
    intervals.saturating_mul(4).saturating_add(16)
}

/// How `node` can be recomputed, if it can.
fn remat_of(graph: &Graph, node: NodeId) -> Option<Remat> {
    match graph.nodes.get(node as usize)?.op {
        Op::Const(v) => Some(Remat::Const(v)),
        Op::ConstF(bits) => Some(Remat::ConstF(bits)),
        Op::Param(i) => Some(Remat::Param(i)),
        _ => None,
    }
}

/// An interval waiting to be allocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Pending {
    lo: usize,
    hi: usize,
    node: NodeId,
}

/// An interval currently holding a register.
#[derive(Clone, Copy, Debug)]
struct Active {
    reg: PhysReg,
    lo: usize,
    hi: usize,
    node: NodeId,
    /// Index into `reg_segments[node]`, so an eviction can shorten the segment
    /// that was already recorded.
    seg: usize,
}

/// Which interval loses its register when nothing is free.
enum Victim {
    /// The interval being allocated stays in memory until its next use.
    Current,
    /// An active interval is cut short at the current position.
    Active(usize),
}

/// Linear-scan register allocation over a scheduled IR graph.
///
/// ## Algorithm
///
/// Poletto–Sarkar linear scan, extended with interval splitting, a
/// frequency-weighted spill cost model and re-materialisation:
///
///  1. Intervals are the [`LiveModel`]'s contiguous ranges, processed in
///     increasing start order.
///  2. Actives whose range ends before the current start expire (the overlap
///     rule is endpoint-inclusive, so an active expires at `hi < lo`).
///  3. A register is chosen from the free set, preferring one with no clobber
///     and no foreign fixed constraint anywhere in the interval. If only a
///     partially-usable register is free, the interval is **split** at the
///     first blocking position: the prefix keeps the register, the suffix is
///     re-queued at its next use. This is what makes a call cost a spill/reload
///     pair for exactly the values that could not be given a callee-saved
///     register, and nothing for the ones that could.
///  4. With nothing free, a victim is chosen among the current interval and the
///     actives of the same class that started strictly earlier, maximising
///     `next_use_distance × 1024 / frequency_weight`, with rematerializable
///     values scored [`SPILL_SCORE_REMAT`] so they always go first. The victim
///     is cut at the current position and its suffix re-queued at its next use.
///
/// Every re-queue strictly advances the interval's start, so the scan
/// terminates; [`split_budget`] bounds compile time on top of that.
///
/// ## The subset that is promoted
///
/// A value is a candidate only if **all** of these hold. Everything else keeps
/// the home slot `ir_lower` gives it today, which is always correct:
///
///   * the liveness fixed point converged (otherwise nothing is promoted);
///   * it is not pinned — not a phi (its home is what the edge copies write)
///     and not named by any deopt frame state (the home must hold the value at
///     *any* recorded bci, which is not a property this pass establishes);
///   * its type has a register class (`Void`/`Control`/`Memory` do not);
///   * **if it is a `Ref`, its live range contains no safepoint.**
///
/// ## The `Ref`-at-safepoint decision
///
/// References are kept **in memory across every safepoint**. A `Ref` whose
/// range covers a safepoint position is never promoted at all; one that lives
/// and dies strictly between safepoints may take a register.
///
/// The alternative — allowing a `Ref` in a register across a safepoint and
/// naming that register in the oop map — requires a register bank in the oop
/// map, in the GC's frame walker, and in the deopt frame reconstructor, and it
/// requires the collector to be able to *update* the register on a moving
/// collection. `ir_lower::emit_safepoint_map` publishes frame slots and nothing
/// else. Promoting a reference across a safepoint without that would either
/// hide a live oop from the collector or hand it a stale one after evacuation;
/// both are silent heap corruption. The conservative rule costs GP registers in
/// reference-heavy code and costs nothing in the arithmetic and loop kernels
/// this pass exists for.
pub fn allocate_linear_scan(
    graph: &Graph,
    live: &LiveModel,
    model: &MachineModel,
) -> CompileResult<Allocation> {
    let n = graph.nodes.len();

    // ── Fixed constraints: index them and refuse the unsatisfiable ───
    //
    // Two values requiring the same register at the same position is not a
    // heuristic failure, it is an unsatisfiable model. That is what
    // `RegisterPressure` means.
    let mut fixed_at: HashMap<(usize, PhysReg), NodeId> = HashMap::new();
    let mut fixed_of: HashMap<NodeId, Vec<(usize, PhysReg)>> = HashMap::new();
    for f in &model.fixed {
        if !model.regs.contains(f.reg) {
            // Outside the allocatable file: the emitter's fixed scratch, which
            // `MachineModel::for_graph` already records as a clobber.
            continue;
        }
        if let Some(&other) = fixed_at.get(&(f.pos, f.reg)) {
            if other != f.node {
                return Err(Bailout::with_context(
                    BailoutReason::RegisterPressure,
                    format!(
                        "n{} and n{other} both require {} at position {}",
                        f.node, f.reg, f.pos
                    ),
                ));
            }
        }
        fixed_at.insert((f.pos, f.reg), f.node);
        let slots = fixed_of.entry(f.node).or_default();
        if let Some(&(pos, reg)) = slots.iter().find(|(pos, _)| *pos == f.pos) {
            if reg != f.reg {
                return Err(Bailout::with_context(
                    BailoutReason::RegisterPressure,
                    format!(
                        "n{} requires both {reg} and {} at position {pos}",
                        f.node, f.reg
                    ),
                ));
            }
        }
        slots.push((f.pos, f.reg));
    }

    // ── Candidate set ────────────────────────────────────────────────
    let mut promotable = vec![false; n];
    let mut unhandled: BinaryHeap<Reverse<Pending>> = BinaryHeap::new();
    for id in 0..n {
        if !live.wants_loc.get(id).copied().unwrap_or(false) {
            continue;
        }
        let Some(range) = live.range.get(id).copied().flatten() else {
            continue;
        };
        if !live.converged || live.pinned[id] || live.class[id].is_none() {
            continue;
        }
        if live.is_ref[id]
            && !model.refs_may_cross_safepoints
            && model.range_covers_safepoint(range)
        {
            continue;
        }
        if model
            .regs
            .class_size(live.class[id].unwrap_or(RegClass::Gp))
            == 0
        {
            continue;
        }
        promotable[id] = true;
        unhandled.push(Reverse(Pending {
            lo: range.lo,
            hi: range.hi,
            node: id as NodeId,
        }));
    }

    let budget = split_budget(unhandled.len());
    let mut reg_segments: Vec<Vec<(PosRange, PhysReg)>> = vec![Vec::new(); n];
    let mut active: Vec<Active> = Vec::new();
    let mut splits = 0usize;

    // ── The scan ─────────────────────────────────────────────────────
    while let Some(Reverse(current)) = unhandled.pop() {
        let Pending { lo, hi, node } = current;
        if lo > hi {
            continue;
        }
        let Some(class) = live.class.get(node as usize).copied().flatten() else {
            continue;
        };
        active.retain(|a| a.hi >= lo);

        // A register this value is *required* to hold somewhere in this
        // interval overrides free choice.
        let required: Option<PhysReg> = fixed_of.get(&node).and_then(|slots| {
            slots
                .iter()
                .find(|(pos, _)| lo <= *pos && *pos <= hi)
                .map(|(_, reg)| *reg)
        });

        let mut attempts = 0usize;
        let choice = loop {
            attempts += 1;
            if attempts > model.regs.class_size(class).saturating_add(2) {
                break None;
            }
            if let Some(found) = ls_select_register(model, live, &active, class, required, current)?
            {
                break Some(found);
            }
            match ls_pick_victim(graph, live, &active, class, current) {
                Victim::Current => break None,
                Victim::Active(idx) => {
                    let victim = active.remove(idx);
                    splits += 1;
                    if splits > budget {
                        return Err(Bailout::with_context(
                            BailoutReason::RegisterPressure,
                            format!(
                                "linear scan exceeded {budget} splits over {} registers \
                                 (peak live {})",
                                model.regs.class_size(class),
                                live.peak_live
                            ),
                        ));
                    }
                    // The victim keeps the register up to, but not including,
                    // the position that displaced it.
                    if let Some(seg) = reg_segments
                        .get_mut(victim.node as usize)
                        .and_then(|segs| segs.get_mut(victim.seg))
                    {
                        seg.0.hi = lo.saturating_sub(1);
                    }
                    if let Some(u) = live.next_use_at_or_after(victim.node, lo) {
                        if u <= victim.hi {
                            unhandled.push(Reverse(Pending {
                                lo: u,
                                hi: victim.hi,
                                node: victim.node,
                            }));
                        }
                    }
                }
            }
        };

        match choice {
            Some((reg, until)) => {
                // `until` is the first position at which the register is NOT
                // available (a clobber, or somebody else's fixed constraint),
                // so the segment must stop one short of it. Ending *at* it
                // would leave the value live in a register a call destroys —
                // which `verify_allocation` now rejects outright.
                let end = match until {
                    Some(blocked) => blocked.saturating_sub(1),
                    None => hi,
                };
                let segs = &mut reg_segments[node as usize];
                segs.push((PosRange { lo, hi: end }, reg));
                active.push(Active {
                    reg,
                    lo,
                    hi: end,
                    node,
                    seg: segs.len() - 1,
                });
                if let Some(split_at) = until {
                    splits += 1;
                    if splits > budget {
                        return Err(Bailout::with_context(
                            BailoutReason::RegisterPressure,
                            format!("linear scan exceeded {budget} splits at a clobber point"),
                        ));
                    }
                    // The suffix resumes at the first use at or after the
                    // blocking position; between the split and that use the
                    // value simply sits in its home slot.
                    if let Some(u) = live.next_use_at_or_after(node, split_at + 1) {
                        if u <= hi {
                            unhandled.push(Reverse(Pending { lo: u, hi, node }));
                        }
                    }
                }
            }
            None => {
                // Nothing free and this interval lost the eviction contest: it
                // stays in memory until its next use, then competes again.
                if let Some(u) = live.next_use_after(node, lo) {
                    if u <= hi {
                        splits += 1;
                        if splits > budget {
                            return Err(Bailout::with_context(
                                BailoutReason::RegisterPressure,
                                format!(
                                    "linear scan exceeded {budget} splits; {} registers of \
                                     class {class:?} for peak live {}",
                                    model.regs.class_size(class),
                                    live.peak_live
                                ),
                            ));
                        }
                        unhandled.push(Reverse(Pending { lo: u, hi, node }));
                    }
                }
                // A required register that could not be honoured is a genuine
                // ABI failure, not a missed optimisation: the value must be in
                // that register at that position and it is not.
                if required.is_some() {
                    return Err(Bailout::with_context(
                        BailoutReason::RegisterPressure,
                        format!(
                            "n{node} requires a fixed register in [{lo}, {hi}] that no \
                             allocation could honour"
                        ),
                    ));
                }
            }
        }
    }

    // The allocation is checked before it is returned, so no caller can use an
    // unverified one: `verify_allocation` is not an opt-in debug pass.
    let alloc = ls_finish(graph, live, reg_segments, splits)?;
    verify_allocation(graph, live, model, &alloc)?;
    Ok(alloc)
}

/// Pick a free register for `current`, or `None` when nothing is usable.
///
/// Returns `(register, split_position)`: `split_position` is `Some(p)` when the
/// register is only usable up to `p - 1` because a clobber or a foreign fixed
/// constraint claims it at `p`.
#[allow(clippy::type_complexity)]
fn ls_select_register(
    model: &MachineModel,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    required: Option<PhysReg>,
    current: Pending,
) -> CompileResult<Option<(PhysReg, Option<usize>)>> {
    let range = PosRange {
        lo: current.lo,
        hi: current.hi,
    };
    let busy: HashSet<PhysReg> = active.iter().map(|a| a.reg).collect();

    // The first position in `range` at which `reg` is unavailable to
    // `current.node`: a clobber, or a fixed constraint belonging to someone
    // else. Positions are scanned through the model's sorted clobber list plus
    // the (small) fixed list, so this stays linear in the blocking events.
    let first_block = |reg: PhysReg| -> Option<usize> {
        let mut earliest: Option<usize> = None;
        let start = model.clobbers.partition_point(|(p, _)| *p < range.lo);
        for (pos, regs) in model.clobbers[start..].iter() {
            if *pos > range.hi {
                break;
            }
            if regs.contains(&reg) {
                earliest = Some(earliest.map_or(*pos, |e: usize| e.min(*pos)));
                break;
            }
        }
        for f in &model.fixed {
            if f.reg == reg && f.node != current.node && range.contains(f.pos) {
                earliest = Some(earliest.map_or(f.pos, |e: usize| e.min(f.pos)));
            }
        }
        earliest
    };

    if let Some(reg) = required {
        if busy.contains(&reg) {
            return Ok(None);
        }
        return match first_block(reg) {
            // A clobber inside the range of a value pinned to that register is
            // unsatisfiable by construction; the caller turns this into a
            // structured bailout rather than a wrong assignment.
            Some(_) => Ok(None),
            None => Ok(Some((reg, None))),
        };
    }

    let mut fallback: Option<(PhysReg, usize)> = None;
    for reg in model.regs.of_class(class) {
        if busy.contains(&reg) {
            continue;
        }
        match first_block(reg) {
            None => return Ok(Some((reg, None))),
            Some(p) if p > range.lo => {
                // Usable for the prefix. Prefer the register that survives
                // longest — it is the one that needs the fewest splits.
                if fallback.is_none_or(|(_, best)| p > best) {
                    fallback = Some((reg, p));
                }
            }
            Some(_) => {}
        }
    }
    let _ = live;
    Ok(fallback.map(|(reg, p)| (reg, Some(p))))
}

/// Choose what loses its register when nothing is free.
///
/// Maximises `next_use_distance × SPILL_DISTANCE_SCALE / frequency_weight`, so
/// a value whose next use is far away and whose uses are cold goes first, and a
/// value used on the next instruction inside a nested loop goes last.
/// Rematerializable values score [`SPILL_SCORE_REMAT`] and always go first:
/// evicting one costs no store at all.
fn ls_pick_victim(
    graph: &Graph,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    current: Pending,
) -> Victim {
    let score = |node: NodeId, from: usize, end: usize| -> u64 {
        if remat_of(graph, node).is_some() {
            return SPILL_SCORE_REMAT;
        }
        let next = live.next_use_at_or_after(node, from).unwrap_or(end);
        let mut dist = next.saturating_sub(from) as u64;
        // See `ls_carry_relief`: a carried value's distance is measured in the
        // wrong units for the cost it represents.
        if live.carried.get(node as usize).copied().unwrap_or(false) {
            let relief = ls_carry_relief();
            if relief > 1 {
                dist /= relief;
            }
        }
        let w = live.weight.get(node as usize).copied().unwrap_or(1).max(1);
        dist.saturating_mul(SPILL_DISTANCE_SCALE) / w
    };

    // The current interval's own next use is strictly after its start: it is
    // being defined (or reloaded) at `lo`, so a use *at* `lo` cannot be served
    // from memory.
    let self_next = live
        .next_use_after(current.node, current.lo)
        .unwrap_or(current.hi);
    let mut best_key = (
        score(current.node, current.lo, current.hi),
        self_next.saturating_sub(current.lo),
        0u8,
        u32::MAX - current.node,
    );
    let mut best = Victim::Current;

    for (idx, a) in active.iter().enumerate() {
        if a.reg.class != class {
            continue;
        }
        // An active that starts at the same position cannot be cut short —
        // there is no prefix to keep — and evicting it would not advance
        // anything. Skipping it is what guarantees progress.
        if a.lo >= current.lo {
            continue;
        }
        let next = live
            .next_use_at_or_after(a.node, current.lo)
            .unwrap_or(a.hi);
        let key = (
            score(a.node, current.lo, a.hi),
            next.saturating_sub(current.lo),
            1u8,
            u32::MAX - a.node,
        );
        if key > best_key {
            best_key = key;
            best = Victim::Active(idx);
        }
    }
    best
}

/// Turn the raw register segments into a checked [`Allocation`]: tile every
/// live range, colour the home slots, and derive the transition events.
fn ls_finish(
    graph: &Graph,
    live: &LiveModel,
    mut reg_segments: Vec<Vec<(PosRange, PhysReg)>>,
    splits: usize,
) -> CompileResult<Allocation> {
    let n = graph.nodes.len();
    let mut segments: Vec<Vec<Segment>> = vec![Vec::new(); n];
    let mut needs_home = vec![false; n];
    let mut promoted = 0usize;

    for id in 0..n {
        if !live.wants_loc.get(id).copied().unwrap_or(false) {
            continue;
        }
        let Some(range) = live.range.get(id).copied().flatten() else {
            continue;
        };
        let segs = &mut reg_segments[id];
        segs.retain(|(r, _)| r.lo <= r.hi);
        segs.sort_by_key(|(r, _)| (r.lo, r.hi));
        // Two register segments of the same value may not overlap: that would
        // mean the value is in two registers at once and the later store wins.
        for pair in segs.windows(2) {
            if pair[0].0.hi >= pair[1].0.lo {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value holds two registers at once"),
                    format!(
                        "n{id}: [{}, {}] and [{}, {}]",
                        pair[0].0.lo, pair[0].0.hi, pair[1].0.lo, pair[1].0.hi
                    ),
                ));
            }
        }

        let mut timeline: Vec<Segment> = Vec::with_capacity(segs.len() * 2 + 1);
        let mut cursor = range.lo;
        for &(r, reg) in segs.iter() {
            if r.lo < range.lo || r.hi > range.hi {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a register segment escapes its live range"),
                    format!(
                        "n{id}: segment [{}, {}] outside [{}, {}]",
                        r.lo, r.hi, range.lo, range.hi
                    ),
                ));
            }
            if r.lo > cursor {
                timeline.push(Segment {
                    range: PosRange {
                        lo: cursor,
                        hi: r.lo - 1,
                    },
                    reg: None,
                });
                needs_home[id] = true;
            }
            timeline.push(Segment {
                range: r,
                reg: Some(reg),
            });
            cursor = r.hi.saturating_add(1);
        }
        if cursor <= range.hi {
            timeline.push(Segment {
                range: PosRange {
                    lo: cursor,
                    hi: range.hi,
                },
                reg: None,
            });
            needs_home[id] = true;
        }
        if timeline.iter().any(|s| s.reg.is_some()) {
            promoted += 1;
        }
        segments[id] = timeline;
    }

    let (stack_slot, stack_slots) = ls_color_homes(graph, live, &needs_home);

    // ── Transition events ────────────────────────────────────────────
    let mut events: Vec<SpillEvent> = Vec::new();
    let (mut spills, mut reloads, mut remats, mut reg_moves) = (0usize, 0usize, 0usize, 0usize);
    for id in 0..n {
        let timeline = &segments[id];
        if timeline.len() < 2 {
            continue;
        }
        let node = id as NodeId;
        let rematerializable = remat_of(graph, node).is_some();
        // SSA: the home is written once and stays correct, so a value never
        // needs a second store.
        let mut stored = false;
        for pair in timeline.windows(2) {
            let (prev, cur) = (pair[0], pair[1]);
            match (prev.reg, cur.reg) {
                (Some(a), Some(b)) if a != b => {
                    reg_moves += 1;
                    events.push(SpillEvent {
                        node,
                        pos: cur.range.lo,
                        kind: SpillKind::Move,
                        from: Some(a),
                        to: Some(b),
                    });
                }
                (Some(a), None) => {
                    if !rematerializable && !stored {
                        stored = true;
                        spills += 1;
                        events.push(SpillEvent {
                            node,
                            pos: prev.range.hi,
                            kind: SpillKind::Store,
                            from: Some(a),
                            to: None,
                        });
                    }
                }
                (None, Some(b)) => {
                    if rematerializable {
                        remats += 1;
                        events.push(SpillEvent {
                            node,
                            pos: cur.range.lo,
                            kind: SpillKind::Remat,
                            from: None,
                            to: Some(b),
                        });
                    } else {
                        reloads += 1;
                        events.push(SpillEvent {
                            node,
                            pos: cur.range.lo,
                            kind: SpillKind::Load,
                            from: None,
                            to: Some(b),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    events.sort_by_key(|e| (e.pos, e.node));

    Ok(Allocation {
        segments,
        stack_slot,
        stack_slots,
        spills,
        reloads,
        remats,
        reg_moves,
        splits,
        promoted,
        events,
        peak_live: live.peak_live,
    })
}

/// Which home-slot pool a value draws from. Mirror of `ir_lower`'s `SlotClass`:
/// a colour's class is fixed at its first assignment and the pools are
/// disjoint, so a word the oop map names can never come to hold a primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HomeClass {
    Ref,
    Prim,
    Pinned,
}

/// Colour the home frame words for every value that needs one.
///
/// The same expiry-driven colouring `ir_lower::plan_slots` performs, reproduced
/// here because the register allocator changes *which* values need a home: a
/// value that lives its whole life in a register needs none at all, which is
/// the frame-size win on top of the colouring's.
fn ls_color_homes(
    graph: &Graph,
    live: &LiveModel,
    needs_home: &[bool],
) -> (Vec<Option<u32>>, usize) {
    let n = graph.nodes.len();
    let mut color: Vec<Option<u32>> = vec![None; n];
    let mut next_color: u32 = 0;

    // Pinned values first, in node-id order — the order `prealloc_phi_slots`
    // walks — so a phi web's words stay where they have always been.
    for id in 0..n {
        if needs_home.get(id).copied().unwrap_or(false) && live.pinned[id] {
            color[id] = Some(next_color);
            next_color = next_color.saturating_add(1);
        }
    }

    // `Ref` results of `Op::Call` may donate a colour but never receive a
    // recycled one: `emit_safepoint_map` publishes every `Ref` slot defined so
    // far, and the self-recursive route's shadow reload would overwrite a
    // recycled word after the call result was stored into it.
    let fresh_only = |id: usize| -> bool {
        graph
            .nodes
            .get(id)
            .is_some_and(|node| node.ty == IrType::Ref && matches!(node.op, Op::Call { .. }))
    };

    let mut order: Vec<usize> = (0..n)
        .filter(|&id| needs_home.get(id).copied().unwrap_or(false) && !live.pinned[id])
        .collect();
    order.sort_by_key(|&id| (live.range[id].map_or(0, |r| r.lo), id));

    let mut active: BinaryHeap<Reverse<(usize, u32, bool)>> = BinaryHeap::new();
    let mut free_ref: Vec<u32> = Vec::new();
    let mut free_prim: Vec<u32> = Vec::new();
    for id in order {
        let Some(r) = live.range[id] else { continue };
        while let Some(&Reverse((active_hi, active_color, active_is_ref))) = active.peek() {
            if active_hi >= r.lo {
                break;
            }
            active.pop();
            if active_is_ref {
                free_ref.push(active_color);
            } else {
                free_prim.push(active_color);
            }
        }
        let is_ref = live.is_ref.get(id).copied().unwrap_or(false);
        let recycled = if fresh_only(id) {
            None
        } else if is_ref {
            free_ref.pop()
        } else {
            free_prim.pop()
        };
        let c = match recycled {
            Some(c) => c,
            None => {
                let c = next_color;
                next_color = next_color.saturating_add(1);
                c
            }
        };
        color[id] = Some(c);
        active.push(Reverse((r.hi, c, is_ref)));
    }

    (color, next_color as usize)
}

/// The home-slot pool a value belongs to, for the aliasing check.
fn ls_home_class(live: &LiveModel, id: usize) -> HomeClass {
    if live.pinned.get(id).copied().unwrap_or(false) {
        HomeClass::Pinned
    } else if live.is_ref.get(id).copied().unwrap_or(false) {
        HomeClass::Ref
    } else {
        HomeClass::Prim
    }
}

// ── Verification ─────────────────────────────────────────────────────

/// Reject overlapping ranges within one shared resource.
///
/// `group` is every `(range, owner)` pair that occupies the resource. Sorting
/// by start and comparing each entry against the furthest-reaching predecessor
/// is linear and still catches a *nested* range, which a naive
/// compare-with-the-previous-entry sweep misses.
fn ls_check_disjoint(
    what: &'static str,
    resource: &str,
    mut group: Vec<(PosRange, NodeId)>,
) -> CompileResult<()> {
    group.sort_by_key(|(r, id)| (r.lo, r.hi, *id));
    let mut furthest: Option<(PosRange, NodeId)> = None;
    for (r, id) in group {
        if let Some((p, pid)) = furthest {
            if p.overlaps(r) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(what),
                    format!(
                        "{resource}: n{pid} over [{}, {}] and n{id} over [{}, {}]",
                        p.lo, p.hi, r.lo, r.hi
                    ),
                ));
            }
        }
        if furthest.is_none_or(|(p, _)| r.hi > p.hi) {
            furthest = Some((r, id));
        }
    }
    Ok(())
}

/// Prove an [`Allocation`] is one a backend may emit.
///
/// This is the pass that turns "the heuristic looked right" into "the heuristic
/// is right on this graph". [`allocate_linear_scan`] runs it before returning,
/// so no caller can obtain an unverified allocation; it is exported because a
/// consumer that *builds* or *rewrites* an allocation (a coalescer, a
/// hand-written test fixture) has to be able to re-check its work.
///
/// Every failure is [`BailoutReason::Internal`] — a violation here is a
/// compiler bug, not a property of the input program, and the pressure case is
/// already reported as [`BailoutReason::RegisterPressure`] by the allocator
/// itself. The method loses its optimized body; the VM does not die.
///
/// ## What is proved
///
/// 1. **Shape.** One timeline per graph node, and a value's segments tile its
///    whole live range in order with no gap and no overlap.
/// 2. **Register class.** Every register a value holds is in that value's class
///    ([`RegClass::of`] its type) and is in the allocatable [`RegFile`].
/// 3. **No aliasing.** Two *different* values never hold the same [`PhysReg`]
///    at the same position, under the same endpoint-inclusive overlap rule the
///    allocator and `ir_lower::plan_slots` use. This is the invariant the whole
///    pass exists to keep.
/// 4. **ABI.** No value is live in a register across a position that clobbers
///    it, no value holds a register another value is pinned to at that
///    position, and a pinned value never holds a *different* register at its
///    pinned position. (A pinned value that was not promoted at all is not a
///    violation: the prologue leaves it in its home slot, which is where every
///    reader looks today.)
/// 5. **The GC rule.** A `Ref` never holds a register across a safepoint — see
///    the decision recorded on [`allocate_linear_scan`].
/// 6. **Homes.** Two values that share a home word have disjoint live ranges,
///    and a home word never mixes [`HomeClass`] pools, so the word the oop map
///    names can never come to hold a primitive.
/// 7. **Bookkeeping.** The event list is in position order and its kinds sum to
///    the reported spill / reload / remat / move counts, so a metrics consumer
///    cannot be handed a number the events do not support.
pub fn verify_allocation(
    graph: &Graph,
    live: &LiveModel,
    model: &MachineModel,
    alloc: &Allocation,
) -> CompileResult<()> {
    let n = graph.nodes.len();
    if alloc.segments.len() != n || alloc.stack_slot.len() != n {
        return Err(Bailout::with_context(
            BailoutReason::Internal("regalloc: allocation is not sized for the graph"),
            format!(
                "{n} nodes, {} timelines, {} homes",
                alloc.segments.len(),
                alloc.stack_slot.len()
            ),
        ));
    }
    // `ls_select_register` binary-searches the clobber list, so an unsorted one
    // silently hides a clobber from the allocator. Check the invariant here
    // rather than trust every producer of a `MachineModel`.
    if model.clobbers.windows(2).any(|w| w[0].0 > w[1].0) {
        return Err(Bailout::with_context(
            BailoutReason::Internal("regalloc: the clobber list is not sorted by position"),
            format!("{} clobber sites", model.clobbers.len()),
        ));
    }

    let mut by_reg: BTreeMap<PhysReg, Vec<(PosRange, NodeId)>> = BTreeMap::new();
    let mut by_home: BTreeMap<u32, Vec<(PosRange, NodeId)>> = BTreeMap::new();
    let mut home_pool: BTreeMap<u32, (HomeClass, NodeId)> = BTreeMap::new();

    for id in 0..n {
        let node = id as NodeId;
        let segs = &alloc.segments[id];
        let wants = live.wants_loc.get(id).copied().unwrap_or(false);
        let Some(range) = live.range.get(id).copied().flatten().filter(|_| wants) else {
            if !segs.is_empty() {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value-less node was given a location"),
                    format!("n{node} has {} segments", segs.len()),
                ));
            }
            continue;
        };

        // ── 1. The timeline tiles the live range ─────────────────────
        let (Some(first), Some(last)) = (segs.first(), segs.last()) else {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: a live value has no location timeline"),
                format!("n{node} over [{}, {}]", range.lo, range.hi),
            ));
        };
        if first.range.lo != range.lo || last.range.hi != range.hi {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: a timeline does not cover the live range"),
                format!(
                    "n{node}: timeline [{}, {}] vs range [{}, {}]",
                    first.range.lo, last.range.hi, range.lo, range.hi
                ),
            ));
        }
        for w in segs.windows(2) {
            if w[0].range.hi.saturating_add(1) != w[1].range.lo {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a timeline has a gap or an overlap"),
                    format!(
                        "n{node}: [{}, {}] then [{}, {}]",
                        w[0].range.lo, w[0].range.hi, w[1].range.lo, w[1].range.hi
                    ),
                ));
            }
        }

        // ── 2/4/5. Per-segment class, ABI and GC rules ───────────────
        let mut needs_home = false;
        for seg in segs {
            if seg.range.lo > seg.range.hi {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: an empty segment"),
                    format!("n{node}: [{}, {}]", seg.range.lo, seg.range.hi),
                ));
            }
            let Some(reg) = seg.reg else {
                needs_home = true;
                continue;
            };
            if live.class.get(id).copied().flatten() != Some(reg.class) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value is in the wrong register class"),
                    format!(
                        "n{node} is {:?} but holds {reg}",
                        live.class.get(id).copied().flatten()
                    ),
                ));
            }
            if !model.regs.contains(reg) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value holds a non-allocatable register"),
                    format!("n{node} holds {reg}"),
                ));
            }
            if live.pinned.get(id).copied().unwrap_or(false) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a pinned value was promoted"),
                    format!(
                        "n{node} holds {reg} over [{}, {}]",
                        seg.range.lo, seg.range.hi
                    ),
                ));
            }
            // The same rule the candidate set applies, checked again over what
            // was actually produced — and lifted by the same knob, so the
            // verifier cannot be the thing that quietly forbids what the model
            // permits. `MachineModel::refs_may_cross_safepoints` documents who
            // may set it and what they owe for it.
            if live.is_ref.get(id).copied().unwrap_or(false)
                && !model.refs_may_cross_safepoints
                && model.range_covers_safepoint(seg.range)
            {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: a reference holds a register across a safepoint",
                    ),
                    format!(
                        "n{node} holds {reg} over [{}, {}], which the oop map cannot describe",
                        seg.range.lo, seg.range.hi
                    ),
                ));
            }
            // No value may be live in a register a call (or a fixed-operand
            // instruction) destroys.
            let start = model.clobbers.partition_point(|(p, _)| *p < seg.range.lo);
            for entry in model.clobbers[start..].iter() {
                let pos = entry.0;
                if pos > seg.range.hi {
                    break;
                }
                if model.clobbered_at(pos).contains(&reg) {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal(
                            "regalloc: a value is live in a clobbered register",
                        ),
                        format!(
                            "n{node} holds {reg} over [{}, {}], clobbered at {pos}",
                            seg.range.lo, seg.range.hi
                        ),
                    ));
                }
            }
            by_reg.entry(reg).or_default().push((seg.range, node));
        }

        // ── 6. Home words ────────────────────────────────────────────
        if needs_home {
            let Some(color) = alloc.stack_slot.get(id).copied().flatten() else {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a spilled value has no home word"),
                    format!("n{node} over [{}, {}]", range.lo, range.hi),
                ));
            };
            if color as usize >= alloc.stack_slots {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a home word is outside the frame plan"),
                    format!("n{node} at word {color} of {}", alloc.stack_slots),
                ));
            }
            let pool = ls_home_class(live, id);
            // Copied out before the match so the `_` arm can insert: an
            // outstanding `&` into the map would make that arm a borrow error.
            let seen = home_pool.get(&color).copied();
            match seen {
                Some((existing, owner)) if existing != pool => {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: a home word mixes two slot pools"),
                        format!("word {color}: n{owner} is {existing:?}, n{node} is {pool:?}"),
                    ));
                }
                _ => {
                    home_pool.insert(color, (pool, node));
                }
            }
            by_home.entry(color).or_default().push((range, node));
        }
    }

    // ── 3. No two values in one register at one position ─────────────
    for (reg, group) in by_reg {
        ls_check_disjoint(
            "regalloc: two values hold one register at the same position",
            &reg.to_string(),
            group,
        )?;
    }
    for (color, group) in by_home {
        ls_check_disjoint(
            "regalloc: two values share one home word",
            &format!("word {color}"),
            group,
        )?;
    }

    // ── 4b. Fixed (ABI) constraints ──────────────────────────────────
    for f in &model.fixed {
        if !model.regs.contains(f.reg) {
            continue;
        }
        // The constrained value must not be sitting somewhere else.
        if let Some(ValueLoc::Reg(actual)) = alloc.location_at(f.node, f.pos) {
            if actual != f.reg {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: an ABI-pinned value is in the wrong register",
                    ),
                    format!(
                        "n{} needs {} at {} but holds {actual}",
                        f.node, f.reg, f.pos
                    ),
                ));
            }
        }
        // …and nobody else may be occupying the register it is pinned to.
        for id in 0..n {
            let other = id as NodeId;
            if other == f.node {
                continue;
            }
            if alloc.location_at(other, f.pos) == Some(ValueLoc::Reg(f.reg)) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: an ABI-pinned register is held by another value",
                    ),
                    format!(
                        "n{other} holds {} at {}, pinned to n{}",
                        f.reg, f.pos, f.node
                    ),
                ));
            }
        }
    }

    // ── 7. The event list supports the reported counts ───────────────
    let (mut stores, mut loads, mut remats, mut moves) = (0usize, 0usize, 0usize, 0usize);
    let mut prev_pos = 0usize;
    for (i, e) in alloc.events.iter().enumerate() {
        if i > 0 && e.pos < prev_pos {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: spill events are not in position order"),
                format!("event {i} at {} follows {prev_pos}", e.pos),
            ));
        }
        prev_pos = e.pos;
        match e.kind {
            SpillKind::Store => stores += 1,
            SpillKind::Load => loads += 1,
            SpillKind::Remat => remats += 1,
            SpillKind::Move => moves += 1,
        }
    }
    let reported = (alloc.spills, alloc.reloads, alloc.remats, alloc.reg_moves);
    if (stores, loads, remats, moves) != reported {
        return Err(Bailout::with_context(
            BailoutReason::Internal("regalloc: the reported counts do not match the events"),
            format!(
                "events say {stores}/{loads}/{remats}/{moves}, report says {}/{}/{}/{}",
                alloc.spills, alloc.reloads, alloc.remats, alloc.reg_moves
            ),
        ));
    }

    Ok(())
}

// ── Phi resolution: the parallel copy ────────────────────────────────

/// One machine-level move in a *sequentialised* parallel copy.
///
/// The scratch is unnamed on purpose: it is whatever the backend already keeps
/// free at an edge (RAX for `ir_lower::emit_phi_copies`, which routes every
/// copy through it). Naming a register here would make the sequence wrong on
/// any backend with a different scratch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOp {
    /// `to ← from`.
    Move {
        /// Where the value is read from.
        from: ValueLoc,
        /// Where it is written to.
        to: ValueLoc,
    },
    /// `scratch ← from`. Emitted only to break a cycle, always followed later
    /// by exactly one [`CopyOp::Restore`].
    Save {
        /// The location whose value is about to be overwritten.
        from: ValueLoc,
    },
    /// `to ← scratch`.
    Restore {
        /// Where the saved value lands.
        to: ValueLoc,
    },
}

/// Sequentialise a parallel copy: `(destination, source)` pairs that all read
/// their sources **before** any destination is written.
///
/// A phi web on one CFG edge is exactly such a copy. Emitting the pairs in
/// order is wrong whenever a destination is also somebody's source — the
/// classic case being a loop that swaps two locals, whose two phis are each
/// other's incoming value:
///
/// ```text
///   a' ← b
///   b' ← a          emitted naively: a' ← b ; b' ← a'   →   both become b
/// ```
///
/// This returns an order in which every read still sees the pre-copy value:
/// destinations that nothing else reads go first, and when only cycles remain
/// one element is parked in the scratch, which frees its predecessor and
/// unwinds the rest of the cycle. Costs at most one [`CopyOp::Save`] /
/// [`CopyOp::Restore`] pair per cycle and nothing at all on the acyclic
/// majority.
///
/// Errors (as [`BailoutReason::Internal`]) when one destination is written
/// twice, which is not a copy this can sequentialise — it is a malformed web.
pub fn resolve_parallel_copy(copies: &[(ValueLoc, ValueLoc)]) -> CompileResult<Vec<CopyOp>> {
    // `None` as a source means "the scratch", which is where a broken cycle
    // parks the value the last move of that cycle has to read.
    let mut pending: Vec<(ValueLoc, Option<ValueLoc>)> = Vec::with_capacity(copies.len());
    for &(dst, src) in copies {
        if dst == src {
            // A copy to itself is not a move, and treating it as one would
            // invent a false dependency that forces a scratch.
            continue;
        }
        // Copied out before the match: an outstanding `&` into `pending` would
        // make the `None` arm's push a borrow error.
        let existing = pending.iter().find(|(d, _)| *d == dst).map(|(_, s)| *s);
        match existing {
            // The same pair listed twice is idempotent — a merge reached along
            // two edges of the same predecessor asks for the identical write.
            Some(s) if s == Some(src) => continue,
            Some(_) => {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: a parallel copy writes one destination twice",
                    ),
                    format!("{dst:?}"),
                ));
            }
            None => pending.push((dst, Some(src))),
        }
    }

    let mut out: Vec<CopyOp> = Vec::with_capacity(pending.len() + 2);
    // Each pass either retires an entry or breaks one cycle (which immediately
    // makes an entry retirable), so twice the entry count plus a constant
    // bounds the loop even if the invariant above were ever violated.
    let mut guard = pending.len().saturating_mul(2).saturating_add(4);
    while !pending.is_empty() {
        if guard == 0 {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: parallel copy sequencing did not terminate"),
                format!("{} copies left", pending.len()),
            ));
        }
        guard -= 1;

        // A destination nothing still reads can be written now.
        let ready = pending
            .iter()
            .position(|(d, _)| !pending.iter().any(|(_, s)| *s == Some(*d)));
        match ready {
            Some(i) => {
                let (dst, src) = pending.remove(i);
                out.push(match src {
                    Some(from) => CopyOp::Move { from, to: dst },
                    None => CopyOp::Restore { to: dst },
                });
            }
            None => {
                // Every remaining destination is also a source. Each has
                // exactly one source (destinations are unique) and at least one
                // reader, so what is left is a disjoint union of pure cycles.
                // Park one element and its cycle unwinds.
                let (cycle, _) = pending[0];
                out.push(CopyOp::Save { from: cycle });
                for (_, src) in pending.iter_mut() {
                    if *src == Some(cycle) {
                        *src = None;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The parallel copy `pred_block`'s outgoing edge must perform.
///
/// One `(destination, source)` pair per phi value that flows along this edge,
/// with the source read at the predecessor's **edge position** — the position
/// [`build_live_model`] charges phi arguments to, so the locations here are the
/// ones the allocation actually holds at the moment the copies run.
///
/// Destinations are always home words: phis are `pinned` (see [`LiveModel`]),
/// so a phi is never promoted and its home is what every reader looks at.
/// Feed the result to [`resolve_parallel_copy`] — the pairs are simultaneous,
/// not sequential.
pub fn phi_edge_copies(
    graph: &Graph,
    schedule: &Schedule,
    live: &LiveModel,
    alloc: &Allocation,
    pred_block: usize,
) -> CompileResult<Vec<(ValueLoc, ValueLoc)>> {
    let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::new();
    let Some(&(_, edge_pos)) = live.span.get(pred_block) else {
        return Ok(copies);
    };
    let Some(pred) = schedule.blocks.get(pred_block) else {
        return Ok(copies);
    };
    for &succ in &pred.successors {
        let Some(merge_ctrl) = schedule.blocks.get(succ).map(|b| b.ctrl) else {
            continue;
        };
        let Some(merge) = graph.nodes.get(merge_ctrl as usize) else {
            continue;
        };
        if !matches!(merge.op, Op::Merge | Op::Region) {
            continue;
        }
        for (pid, phi) in graph.nodes.iter().enumerate() {
            if !matches!(phi.op, Op::Phi) {
                continue;
            }
            // Memory and control phis are bookkeeping tokens with no machine
            // value — the same filter `ir_lower::emit_phi_copies` applies.
            if matches!(phi.ty, IrType::Memory | IrType::Control | IrType::Void) {
                continue;
            }
            if phi.inputs.first().copied() != Some(merge_ctrl) {
                continue;
            }
            for (k, &ctrl_in) in merge.inputs.iter().enumerate() {
                if ls_ctrl_block_of(graph, schedule, ctrl_in) != Some(pred_block) {
                    continue;
                }
                let Some(&v) = phi.inputs.get(k + 1) else {
                    continue;
                };
                if v == NO_NODE {
                    continue;
                }
                let Some(color) = alloc.stack_slot.get(pid).copied().flatten() else {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: a phi has no home word to copy into"),
                        format!("n{pid}"),
                    ));
                };
                let Some(src) = alloc.location_at(v, edge_pos) else {
                    return Err(Bailout::with_context(
                        BailoutReason::UnallocatedValue { node: v },
                        format!("n{v} has no location at edge position {edge_pos}"),
                    ));
                };
                copies.push((ValueLoc::Slot(color), src));
            }
        }
    }
    Ok(copies)
}

// ── Metrics ──────────────────────────────────────────────────────────

/// Publish an allocation's cost to a [`CompileRecorder`].
///
/// `CompilationReport::spills` / `::reloads` are declared `Measured<T>` and
/// report "not measured" precisely because nothing computed them; this is the
/// producer. Note the asymmetry with `peak_live_values`, which the compiler
/// also records ambiently through `metrics::note_current_peak_live_values`
/// because `ir_lower::lower_inner`'s signature is pinned and cannot take a
/// recorder. The optimizing pipeline will need the same ambient form for these
/// two — `metrics::note_current_spills` / `note_current_reloads`, mirroring
/// `metrics.rs`'s existing `note_current_peak_live_values` — which this wave
/// may not add; see `docs/jit/linear-scan-regalloc.md`.
pub fn record_allocation_metrics(recorder: &CompileRecorder, alloc: &Allocation) {
    recorder.set_peak_live_values(alloc.peak_live);
    recorder.set_spills(alloc.spills);
    recorder.set_reloads(alloc.reloads);
}

#[cfg(test)]
mod linear_scan_tests {
    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_schedule;

    // ── Fixtures ─────────────────────────────────────────────────────

    /// A hand-written single-block liveness model.
    ///
    /// [`build_live_model`] is exercised end to end at the bottom of this
    /// module, on real scheduled graphs. These fixtures exist because the
    /// allocator's interesting behaviour — who wins an eviction, where a split
    /// lands, which register a clobber pushes a value out of — is a function of
    /// *intervals*, and steering a scheduler into an exact pair of positions is
    /// neither reliable nor the thing under test.
    struct Fixture {
        graph: Graph,
        live: LiveModel,
    }

    impl Fixture {
        fn new(total_positions: usize) -> Fixture {
            Fixture {
                graph: Graph {
                    nodes: Vec::new(),
                    entry: 0,
                    exit: NO_NODE,
                    safepoints: Vec::new(),
                    uses: Default::default(),
                    receiver_param: None,
                },
                live: LiveModel {
                    pos_of: Vec::new(),
                    span: vec![(0, total_positions.saturating_sub(1))],
                    total_positions,
                    block_of_pos: vec![0; total_positions],
                    wants_loc: Vec::new(),
                    range: Vec::new(),
                    class: Vec::new(),
                    is_ref: Vec::new(),
                    pinned: Vec::new(),
                    uses: Vec::new(),
                    weight: Vec::new(),
                    loop_depth: vec![0],
                    peak_live: 0,
                    carried: Vec::new(),
                    converged: true,
                },
            }
        }

        fn value(&mut self, op: Op, ty: IrType, lo: usize, hi: usize, uses: &[usize]) -> NodeId {
            self.push(op, ty, lo, hi, uses, false)
        }

        /// A value `ir_lower` owns the home of — a phi, or anything a deopt
        /// frame names. Never promoted.
        fn home_bound(&mut self, ty: IrType, lo: usize, hi: usize, uses: &[usize]) -> NodeId {
            self.push(Op::Phi, ty, lo, hi, uses, true)
        }

        fn push(
            &mut self,
            op: Op,
            ty: IrType,
            lo: usize,
            hi: usize,
            uses: &[usize],
            pin: bool,
        ) -> NodeId {
            let id = self.graph.add(op, ty, vec![], None);
            assert_eq!(
                id as usize,
                self.live.wants_loc.len(),
                "fixture node ids must stay dense"
            );
            assert!(
                lo <= hi && hi < self.live.total_positions,
                "interval [{lo}, {hi}] does not fit the fixture"
            );
            self.live.pos_of.push(Some(lo));
            self.live.wants_loc.push(true);
            self.live.range.push(Some(PosRange { lo, hi }));
            self.live.class.push(RegClass::of(ty));
            self.live.is_ref.push(ty == IrType::Ref);
            self.live.pinned.push(pin);
            let mut u: Vec<usize> = uses.to_vec();
            u.sort_unstable();
            u.dedup();
            self.live.weight.push(u.len().max(1) as u64);
            self.live.uses.push(u);
            id
        }

        fn finish(mut self) -> (Graph, LiveModel) {
            let mut delta = vec![0i64; self.live.total_positions + 2];
            for r in self.live.range.iter().flatten() {
                delta[r.lo] += 1;
                delta[r.hi + 1] -= 1;
            }
            let (mut running, mut peak) = (0i64, 0i64);
            for d in &delta {
                running += d;
                peak = peak.max(running);
            }
            self.live.peak_live = peak.max(0) as usize;
            (self.graph, self.live)
        }
    }

    /// `n` callee-saved GP registers, handed out r0 first.
    fn gp(n: u8) -> RegFile {
        RegFile::from_specs((0..n).map(|num| RegSpec {
            reg: PhysReg::gp(num),
            caller_saved: false,
        }))
    }

    /// `volatile` caller-saved GP registers **first**, then `saved`
    /// callee-saved ones. The preference order is what makes "a value that
    /// crosses a call has to skip the volatile registers" observable: with the
    /// callee-saved ones first every value would trivially survive.
    fn gp_volatile_first(volatile: u8, saved: u8) -> RegFile {
        RegFile::from_specs(
            (0..volatile)
                .map(|num| RegSpec {
                    reg: PhysReg::gp(num),
                    caller_saved: true,
                })
                .chain((volatile..volatile + saved).map(|num| RegSpec {
                    reg: PhysReg::gp(num),
                    caller_saved: false,
                })),
        )
    }

    fn bare_model(regs: RegFile) -> MachineModel {
        MachineModel {
            regs,
            clobbers: Vec::new(),
            fixed: Vec::new(),
            safepoints: Vec::new(),
            refs_may_cross_safepoints: false,
        }
    }

    fn internal_message(err: &Bailout) -> String {
        match &err.reason {
            BailoutReason::Internal(msg) => (*msg).to_string(),
            other => panic!("expected an Internal bailout, got {other:?}"),
        }
    }

    // ── The allocator core ───────────────────────────────────────────

    #[test]
    fn non_overlapping_values_share_one_register() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let b = f.value(Op::Add, IrType::Int, 5, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));
        assert_eq!(
            alloc.first_reg(b),
            Some(PhysReg::gp(0)),
            "a dies at 3, so b may reuse r0 — that reuse is the whole point"
        );
        assert_eq!(alloc.promoted, 2);
        assert_eq!((alloc.spills, alloc.reloads), (0, 0));
        assert_eq!(alloc.stack_slots, 0, "neither value ever touches memory");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn overlapping_values_never_share_a_register() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let b = f.value(Op::Add, IrType::Int, 2, 6, &[6]);
        // `c` starts exactly where `a` ends. Overlap is endpoint-inclusive —
        // a node's lowering may write its result before reading its operands —
        // so this must NOT reuse a's register either.
        let c = f.value(Op::Add, IrType::Int, 8, 10, &[10]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));
        assert_ne!(alloc.first_reg(b), alloc.first_reg(a));
        assert_ne!(
            alloc.first_reg(c),
            alloc.first_reg(a),
            "[0, 8] and [8, 10] touch at 8, which counts as overlapping"
        );
        assert_eq!(live.peak_live, 2);
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// The default: a reference live across a safepoint gets no register,
    /// because the oop map names frame slots and a collector could neither see
    /// nor relocate one held in a register.
    #[test]
    fn a_reference_live_across_a_safepoint_gets_no_register_by_default() {
        let mut f = Fixture::new(12);
        let across = f.value(
            Op::Load(crate::ir::MemKind::Ref),
            IrType::Ref,
            0,
            8,
            &[3, 8],
        );
        let prim = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(4));
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(across),
            None,
            "a reference crossing a safepoint took a register the oop map cannot name"
        );
        assert!(
            alloc.first_reg(prim).is_some(),
            "the primitive must be promoted, or the assertion above is vacuous"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// `refs_may_cross_safepoints` lifts exactly that refusal and nothing else.
    ///
    /// The caller that sets it — `ir_lower`, whose file is a write-through read
    /// cache over a slot the map DOES name — discharges the obligation by
    /// invalidating the cached copy at every point a collector could have run.
    /// See `ir_lower::invalidate_ref_residency`. This test's job is only to
    /// pin that the knob is what decides it, so that turning it on cannot
    /// become a silent property of some other change.
    #[test]
    fn refs_may_cross_safepoints_lifts_that_refusal_and_only_that_one() {
        let mut f = Fixture::new(12);
        let across = f.value(
            Op::Load(crate::ir::MemKind::Ref),
            IrType::Ref,
            0,
            8,
            &[3, 8],
        );
        let pinned = f.home_bound(IrType::Ref, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(4));
        model.safepoints = vec![5];
        model.refs_may_cross_safepoints = true;

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            alloc.first_reg(across).is_some(),
            "the knob is on, so the reference must be allocatable"
        );
        assert_eq!(
            alloc.first_reg(pinned),
            None,
            "the knob lifts the SAFEPOINT refusal only — a home-bound value is              still refused for its own reason"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_call_clobbers_the_caller_saved_registers() {
        let mut f = Fixture::new(12);
        let across = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let short = f.value(Op::Add, IrType::Int, 1, 3, &[3]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 1));
        model.clobbers = vec![(5, vec![PhysReg::gp(0)])];
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(across),
            Some(PhysReg::gp(1)),
            "a value live across the call must land in the callee-saved register"
        );
        assert_eq!(
            alloc.first_reg(short),
            Some(PhysReg::gp(0)),
            "a value that dies before the call may still use the volatile one"
        );
        for seg in &alloc.segments[across as usize] {
            assert!(
                !(seg.reg == Some(PhysReg::gp(0)) && seg.range.contains(5)),
                "nothing may hold r0 at the call"
            );
        }
        assert_eq!(
            (alloc.spills, alloc.reloads),
            (0, 0),
            "the callee-saved register made the call cost nothing"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_value_crossing_a_call_with_no_callee_saved_register_is_split_and_reloaded() {
        let mut f = Fixture::new(12);
        let v = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        // One register, and a call destroys it.
        let mut model = bare_model(gp_volatile_first(1, 0));
        model.clobbers = vec![(5, vec![PhysReg::gp(0)])];
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let segs = &alloc.segments[v as usize];
        assert_eq!(
            segs.len(),
            3,
            "prefix in r0, memory over the call, then back"
        );
        assert_eq!(
            segs[0],
            Segment {
                range: PosRange { lo: 0, hi: 4 },
                reg: Some(PhysReg::gp(0)),
            },
            "the register segment stops one short of the clobber, not on it"
        );
        assert_eq!(segs[1].reg, None);
        assert_eq!(
            segs[2],
            Segment {
                range: PosRange { lo: 8, hi: 8 },
                reg: Some(PhysReg::gp(0)),
            }
        );
        assert_eq!((alloc.spills, alloc.reloads, alloc.splits), (1, 1, 1));

        let store = alloc
            .events
            .iter()
            .find(|e| e.kind == SpillKind::Store)
            .expect("one store");
        assert_eq!(store.pos, 4, "the store may sink to the last live position");
        let load = alloc
            .events
            .iter()
            .find(|e| e.kind == SpillKind::Load)
            .expect("one reload");
        assert_eq!(
            load.pos, 8,
            "the reload lands on the next use, not on the split point"
        );
        assert_eq!(
            alloc.stack_slot[v as usize],
            Some(0),
            "a value that touches memory needs a home word"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_rematerializable_constant_is_evicted_but_never_stored() {
        let mut f = Fixture::new(12);
        // The constant is long-lived and cheap to recompute; the arithmetic
        // value that wants the same single register is not.
        let k = f.value(Op::Const(7), IrType::Int, 0, 10, &[2, 10]);
        let x = f.value(Op::Add, IrType::Int, 1, 9, &[9]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(1));

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(x),
            Some(PhysReg::gp(0)),
            "the constant loses the register to the value that cannot be rebuilt"
        );
        assert_eq!(alloc.spills, 0, "a constant is never stored to memory");
        assert_eq!(alloc.reloads, 0, "and never loaded back from it");
        assert_eq!(alloc.remats, 1, "it is recomputed at its next use instead");
        assert!(
            !alloc.events.iter().any(|e| e.kind == SpillKind::Store),
            "no store may be emitted for a rematerializable value"
        );
        let remat = alloc
            .events
            .iter()
            .find(|e| e.kind == SpillKind::Remat)
            .expect("one remat");
        assert_eq!((remat.node, remat.pos), (k, 10));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn an_entry_parameter_keeps_its_abi_register() {
        let mut f = Fixture::new(10);
        let p = f.value(Op::Param(0), IrType::Int, 0, 8, &[8]);
        let q = f.value(Op::Add, IrType::Int, 1, 7, &[7]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(3));
        model.fixed = vec![FixedConstraint {
            node: p,
            pos: 0,
            reg: PhysReg::gp(2),
        }];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(p),
            Some(PhysReg::gp(2)),
            "the pinned register wins over the preference order"
        );
        assert_ne!(alloc.first_reg(q), Some(PhysReg::gp(2)));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── The reference-at-a-safepoint decision ────────────────────────

    #[test]
    fn a_reference_live_across_a_safepoint_stays_in_memory() {
        let mut f = Fixture::new(12);
        let across = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 0, 8, &[8]);
        let between = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 5, 7, &[7]);
        let prim = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(4));
        model.safepoints = vec![4];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.is_promoted(across),
            "the oop map describes frame words, so a reference that is live at a \
             safepoint must be in one"
        );
        assert!(
            alloc.stack_slot[across as usize].is_some(),
            "and it must therefore have a home word"
        );
        assert!(
            alloc.is_promoted(between),
            "a reference that lives and dies strictly after the safepoint may take \
             a register"
        );
        assert!(
            alloc.is_promoted(prim),
            "the rule is about references, not about crossing a safepoint"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── The verifier ─────────────────────────────────────────────────

    #[test]
    fn verify_allocation_rejects_a_planted_register_alias() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let b = f.value(Op::Add, IrType::Int, 2, 6, &[6]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_ne!(alloc.first_reg(a), alloc.first_reg(b));

        // Two values live at the same position, one register.
        let a_reg = alloc.first_reg(a);
        alloc.segments[b as usize][0].reg = a_reg;
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("an alias must be rejected");
        assert!(
            internal_message(&err).contains("one register"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_a_register_class_violation() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(0),
                caller_saved: false,
            },
            RegSpec {
                reg: PhysReg::xmm(0),
                caller_saved: false,
            },
        ]));
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));

        // An integer parked in an SSE register: allocatable, but the wrong bank.
        alloc.segments[a as usize][0].reg = Some(PhysReg::xmm(0));
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("a class violation must be rejected");
        assert!(
            internal_message(&err).contains("wrong register class"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_an_abi_violation() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Param(0), IrType::Int, 0, 8, &[8]);
        let b = f.value(Op::Add, IrType::Int, 2, 6, &[6]);
        // Home-bound: `ir_lower` owns this one's location, so it is never
        // promoted and the pin below can only be about who else is in the way.
        let homed = f.home_bound(IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));
        assert_eq!(alloc.first_reg(b), Some(PhysReg::gp(1)));
        assert!(!alloc.is_promoted(homed));

        // (a) the pinned value itself is in the wrong register.
        let mut wrong_reg = bare_model(gp(2));
        wrong_reg.fixed = vec![FixedConstraint {
            node: a,
            pos: 0,
            reg: PhysReg::gp(1),
        }];
        let err = verify_allocation(&graph, &live, &wrong_reg, &alloc)
            .expect_err("a misplaced ABI value must be rejected");
        assert!(
            internal_message(&err).contains("ABI-pinned value"),
            "wrong diagnosis: {err:?}"
        );

        // (b) somebody else is occupying the register the ABI reserved.
        let mut occupied = bare_model(gp(2));
        occupied.fixed = vec![FixedConstraint {
            node: homed,
            pos: 4,
            reg: PhysReg::gp(1),
        }];
        let err = verify_allocation(&graph, &live, &occupied, &alloc)
            .expect_err("an occupied ABI register must be rejected");
        assert!(
            internal_message(&err).contains("ABI-pinned register"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_a_reference_in_a_register_at_a_safepoint() {
        let mut f = Fixture::new(12);
        let r = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.safepoints = vec![4];
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(!alloc.is_promoted(r));

        // The exact shape that produced a moving-GC use-after-free before: a
        // live oop in a location the collector's frame walker cannot see.
        alloc.segments[r as usize] = vec![Segment {
            range: PosRange { lo: 0, hi: 8 },
            reg: Some(PhysReg::gp(1)),
        }];
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("an invisible oop must be rejected");
        assert!(
            internal_message(&err).contains("across a safepoint"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_a_timeline_gap() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");

        alloc.segments[a as usize][0].range.hi = 4;
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("an uncovered position must be rejected");
        assert!(
            internal_message(&err).contains("live range"),
            "wrong diagnosis: {err:?}"
        );
    }

    // ── Bailing rather than guessing ─────────────────────────────────

    #[test]
    fn an_unsatisfiable_fixed_constraint_bails_with_register_pressure() {
        let mut f = Fixture::new(8);
        let a = f.value(Op::Param(0), IrType::Int, 0, 6, &[6]);
        let b = f.value(Op::Param(1), IrType::Int, 0, 6, &[6]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.fixed = vec![
            FixedConstraint {
                node: a,
                pos: 0,
                reg: PhysReg::gp(0),
            },
            FixedConstraint {
                node: b,
                pos: 0,
                reg: PhysReg::gp(0),
            },
        ];

        let err = allocate_linear_scan(&graph, &live, &model)
            .expect_err("two values cannot share one register at one position");
        assert!(
            matches!(err.reason, BailoutReason::RegisterPressure),
            "{err:?}"
        );
    }

    #[test]
    fn an_over_pressure_graph_bails_with_register_pressure() {
        // Four long-lived values, each used every fourth position, over a single
        // register: every promotion immediately costs a reload, which is the
        // case linear scan must decline rather than churn through.
        const LAST: usize = 120;
        let mut f = Fixture::new(LAST + 1);
        for i in 0..4usize {
            let uses: Vec<usize> = ((i + 4)..=LAST).step_by(4).collect();
            f.value(Op::Add, IrType::Int, i, LAST, &uses);
        }
        let (graph, live) = f.finish();
        let model = bare_model(gp(1));

        let err = allocate_linear_scan(&graph, &live, &model)
            .expect_err("one register cannot serve four live values");
        assert!(
            matches!(err.reason, BailoutReason::RegisterPressure),
            "{err:?}"
        );
        assert_eq!(live.peak_live, 4, "the report must carry the pressure");
    }

    // ── Parallel copy ────────────────────────────────────────────────

    fn slot(n: u32) -> ValueLoc {
        ValueLoc::Slot(n)
    }

    /// Apply a *sequentialised* copy and return the resulting location→value
    /// map, so a test can compare it against the parallel semantics.
    fn run_copies(start: &[(ValueLoc, u32)], ops: &[CopyOp]) -> HashMap<ValueLoc, u32> {
        let mut state: HashMap<ValueLoc, u32> = start.iter().copied().collect();
        let mut scratch: Option<u32> = None;
        for op in ops {
            match *op {
                CopyOp::Move { from, to } => {
                    let v = match state.get(&from) {
                        Some(&v) => v,
                        None => panic!("read of an uninitialised location {from:?}"),
                    };
                    state.insert(to, v);
                }
                CopyOp::Save { from } => {
                    scratch = state.get(&from).copied();
                }
                CopyOp::Restore { to } => {
                    let v = match scratch {
                        Some(v) => v,
                        None => panic!("restore with nothing saved"),
                    };
                    state.insert(to, v);
                }
            }
        }
        state
    }

    /// The semantics a phi web has: every destination receives its source's
    /// value as it was *before* any copy ran.
    fn parallel_result(
        start: &[(ValueLoc, u32)],
        copies: &[(ValueLoc, ValueLoc)],
    ) -> HashMap<ValueLoc, u32> {
        let before: HashMap<ValueLoc, u32> = start.iter().copied().collect();
        let mut after = before.clone();
        for &(dst, src) in copies {
            match before.get(&src) {
                Some(&v) => {
                    after.insert(dst, v);
                }
                None => panic!("the fixture does not initialise {src:?}"),
            }
        }
        after
    }

    /// Give every location the copy mentions a distinct starting value.
    fn seed(copies: &[(ValueLoc, ValueLoc)]) -> Vec<(ValueLoc, u32)> {
        let mut start: Vec<(ValueLoc, u32)> = Vec::new();
        for &(dst, src) in copies {
            for loc in [dst, src] {
                if !start.iter().any(|(l, _)| *l == loc) {
                    let next = 1000 + start.len() as u32;
                    start.push((loc, next));
                }
            }
        }
        start
    }

    fn check_parallel(copies: &[(ValueLoc, ValueLoc)]) -> Vec<CopyOp> {
        let ops = resolve_parallel_copy(copies).expect("sequentialises");
        let start = seed(copies);
        assert_eq!(
            run_copies(&start, &ops),
            parallel_result(&start, copies),
            "sequentialised copy disagrees with the parallel one: {ops:?}"
        );
        ops
    }

    #[test]
    fn a_copy_chain_writes_the_readers_first() {
        // Naive order would emit `s1 ← s0` first and then read the overwritten
        // s1 into s2.
        let copies = [(slot(1), slot(0)), (slot(2), slot(1))];
        let ops = check_parallel(&copies);
        assert_eq!(
            ops,
            vec![
                CopyOp::Move {
                    from: slot(1),
                    to: slot(2),
                },
                CopyOp::Move {
                    from: slot(0),
                    to: slot(1),
                },
            ],
            "no scratch is needed for an acyclic web"
        );
    }

    #[test]
    fn a_two_element_phi_cycle_uses_the_scratch() {
        // The swap loop's back edge: each phi's incoming value is the other phi.
        let copies = [(slot(0), slot(1)), (slot(1), slot(0))];
        let ops = check_parallel(&copies);
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, CopyOp::Save { .. }))
                .count(),
            1,
            "exactly one save breaks a single cycle: {ops:?}"
        );
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, CopyOp::Restore { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn a_three_element_phi_cycle_resolves() {
        let copies = [(slot(0), slot(1)), (slot(1), slot(2)), (slot(2), slot(0))];
        let ops = check_parallel(&copies);
        assert_eq!(
            ops.len(),
            4,
            "one save, two moves, one restore — the third move IS the restore: {ops:?}"
        );
    }

    #[test]
    fn a_cycle_with_a_fan_out_resolves() {
        // Two phis swap, a third reads one of them, and a register copy hangs
        // off the other — a shape a real loop header produces.
        let copies = [
            (slot(0), slot(1)),
            (slot(1), slot(0)),
            (slot(2), slot(0)),
            (ValueLoc::Reg(PhysReg::gp(3)), slot(1)),
        ];
        check_parallel(&copies);
    }

    #[test]
    fn two_disjoint_cycles_resolve() {
        let copies = [
            (slot(0), slot(1)),
            (slot(1), slot(0)),
            (slot(2), slot(3)),
            (slot(3), slot(2)),
        ];
        check_parallel(&copies);
    }

    #[test]
    fn a_self_copy_is_dropped() {
        let ops = resolve_parallel_copy(&[(slot(0), slot(0))]).expect("resolves");
        assert!(ops.is_empty(), "copying a location to itself emits nothing");
    }

    #[test]
    fn a_parallel_copy_with_a_duplicate_destination_is_rejected() {
        let err = resolve_parallel_copy(&[(slot(0), slot(1)), (slot(0), slot(2))])
            .expect_err("one destination, two sources is not a copy");
        assert!(
            internal_message(&err).contains("destination twice"),
            "wrong diagnosis: {err:?}"
        );
    }

    // ── End to end, on a real scheduled graph ────────────────────────

    /// `int f(int n) { int a=1, b=2; for (int i=0; i<n; i++) swap(a, b); return a; }`
    ///
    /// The swap is done on the operand stack (`iload_1; iload_2; istore_1;
    /// istore_2`), so it needs no temporary local — which makes the loop
    /// header's two phis each other's back-edge value, i.e. a copy cycle.
    fn swap_loop() -> (Graph, ir_schedule::Schedule) {
        let code = [
            0x04, 0x3c, // 0: iconst_1; istore_1        a = 1
            0x05, 0x3d, // 2: iconst_2; istore_2        b = 2
            0x03, 0x3e, // 4: iconst_0; istore_3        i = 0
            0x1d, 0x1a, // 6: iload_3; iload_0          loop header
            0xa2, 0x00, 0x0d, // 8: if_icmpge 21
            0x1b, 0x1c, // 11: iload_1; iload_2         push a, then b
            0x3c, 0x3d, // 13: istore_1; istore_2       a = b; b = old a
            0x84, 0x03, 0x01, // 15: iinc 3, 1
            0xa7, 0xff, 0xf4, // 18: goto 6
            0x1b, 0xac, // 21: iload_1; ireturn
            0, 0,
        ];
        let graph = IrBuilder::new(1, 4)
            .build(&code, 23)
            .expect("the swap loop builds");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    #[test]
    fn phi_arguments_are_consumed_at_the_predecessor_edge() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        assert!(live.converged, "a small loop must reach the fixed point");

        let edges: HashSet<usize> = live.span.iter().map(|&(_, e)| e).collect();
        let mut checked = 0usize;
        for (pid, phi) in graph.nodes.iter().enumerate() {
            if !matches!(phi.op, Op::Phi) || !live.wants_loc[pid] {
                continue;
            }
            for v in phi.phi_value_inputs().flatten() {
                if !live.wants_loc[v as usize] {
                    continue;
                }
                assert!(
                    live.uses[v as usize].iter().any(|u| edges.contains(u)),
                    "n{v} feeds phi n{pid} but records no use at any outgoing edge — \
                     the position model disagrees with ir_lower::plan_slots"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "the swap loop must have phi arguments to check"
        );
    }

    #[test]
    fn a_loop_that_swaps_two_locals_needs_a_parallel_phi_copy() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("the loop allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");

        let mut saw_hazard = false;
        for b in 0..schedule.blocks.len() {
            let copies = phi_edge_copies(&graph, &schedule, &live, &alloc, b).expect("edge copies");
            if copies.is_empty() {
                continue;
            }
            // A destination that is also somebody's source is exactly what a
            // naive sequential emission clobbers.
            if copies
                .iter()
                .any(|(dst, _)| copies.iter().any(|(_, src)| src == dst))
            {
                saw_hazard = true;
            }
            let ops = resolve_parallel_copy(&copies).expect("sequentialises");
            let start = seed(&copies);
            assert_eq!(
                run_copies(&start, &ops),
                parallel_result(&start, &copies),
                "block {b}: the emitted order clobbers a source"
            );
        }
        assert!(
            saw_hazard,
            "the back edge of a swap loop must produce a copy whose destination is \
             also a source"
        );
    }

    #[test]
    fn a_scheduled_loop_allocates_and_verifies() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        assert!(
            !model.safepoints.is_empty(),
            "a back edge polls, which publishes the oop map"
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
        assert_eq!(alloc.peak_live, live.peak_live);
        // Every phi keeps the home word `ir_lower::emit_phi_copies` writes.
        for (id, node) in graph.nodes.iter().enumerate() {
            if matches!(node.op, Op::Phi) && live.wants_loc[id] {
                assert!(
                    !alloc.is_promoted(id as NodeId),
                    "phi n{id} must stay home-bound"
                );
            }
        }
    }

    #[test]
    fn an_unconverged_model_promotes_nothing() {
        let (graph, schedule) = swap_loop();
        let mut live = build_live_model(&graph, &schedule);
        // Simulate the skipped / non-converging fixed point: every value gets
        // the whole method and is pinned, exactly as `build_live_model` leaves
        // it when it cannot analyse a graph.
        live.converged = false;
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.promoted, 0,
            "an unanalysed graph keeps the frame layout ir_lower already had"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// The clobber model must count every op `ir_lower` lowers to a `CALL`
    /// that RETURNS into the body, not only the ops whose name says "call".
    ///
    /// Found while giving this allocator its first production call site: with
    /// a caller-saved register file, an FP value held across an `Op::Load`
    /// (which calls `jit_getfield`) or an `Op::Rem` (which calls `jit_drem`)
    /// would have been destroyed by the helper and read back as garbage. The
    /// old predicate named only `Call`/`New`/`NewArray`/`LambdaIntToDouble`.
    #[test]
    fn every_op_ir_lower_lowers_to_a_returning_call_is_a_clobber() {
        for op in [
            Op::Call { info_ptr: 0 },
            Op::New {
                class_id: 0,
                num_fields: 0,
            },
            Op::NewArray {
                element_type: 0,
                component_class_id: 0,
            },
            Op::LambdaIntToDouble,
            // `jit_frem` / `jit_drem`.
            Op::Rem,
            // `jit_getfield` / `jit_putfield_int`.
            Op::Load(crate::ir::MemKind::Double),
            Op::Store(crate::ir::MemKind::Int),
            // `jit_monitor_enter` / `jit_monitor_exit` / `jit_aastore`.
            Op::MonitorEnter,
            Op::MonitorExit,
            Op::ArrayStore(crate::ir::MemKind::Ref),
        ] {
            assert!(
                ir_op_is_call(&op),
                "{op:?} is lowered to a CALL that returns into the body, so it \
                 destroys the caller-saved registers"
            );
        }
        // A guard's failure edge never returns into the body: the deopt stub
        // runs the epilogue. Counting it would cost a register in every
        // bounds-checked loop for no soundness gain.
        assert!(!ir_op_is_call(&Op::Guard { bci: 0 }));
        assert!(!ir_op_is_call(&Op::Add));
        assert!(!ir_op_is_call(&Op::ArrayLoad(crate::ir::MemKind::Double)));
    }
}
