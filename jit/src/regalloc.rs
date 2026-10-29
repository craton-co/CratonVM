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
use crate::bytecode_analysis;

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
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};

// j27 wave 2 (lane RESIDENCY): the loop half of the spill cost
// (`CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`, opt-in).
mod loop_residency;
pub(crate) use loop_residency::{loop_phi_residency_enabled, ranking_weight};

// j27 wave 4 (lane REMAT): `x + c` recomputed at a split reload
// (`CRATONVM_JIT_IR_LS_REMAT_ADD`, opt-in; consumer `ir_lower::ls_remat_add`).
mod remat_add;
pub use remat_add::{remat_add_of, RematAdd};

/// Conservative cap on `tableswitch` table size used by [`bytecode_analysis::step`].
///
/// JVM method code is at most 65535 bytes, which by itself caps a real
/// `tableswitch` payload at ~16383 entries. We keep an explicit cap two
/// orders of magnitude above that (consistent with `x64::MAX_TABLESWITCH_ENTRIES`)
/// so that adversarial or truncated bytecode cannot trick `(high - low + 1)`
/// into wrapping when cast to `usize`. On overflow / cap exceeded `bytecode_analysis::step`
/// falls back to length 1, which is always safe: any further validation is
/// the caller's responsibility (`jit_scan` / `compile_bytecode` reject the
/// method outright at the same opcode).
const MAX_TABLESWITCH_ENTRIES: usize = 1 << 24;

/// ARM64 callee-saved GPR registers for locals: X19-X28 (10 registers).
pub const ARM64_LOCAL_GPRS: [u8; 10] = [19, 20, 21, 22, 23, 24, 25, 26, 27, 28];

/// ARM64 callee-saved FP/SIMD registers for float/double locals: D8-D15 (8 registers).
/// On AAPCS64 only D8-D15 are callee-saved (the lower 64 bits of V8-V15).
///
/// # A10: the colouring over this pool is currently discarded
///
/// `allocate_registers_arm64_with_param_slots` passes this to
/// `allocate_registers_with`, which colours float locals into it — and
/// `aarch64_backend::compile_pass` then gives every float local `None`, because
/// this prologue saves only GPRs and homing a float local in a callee-saved FP
/// register destroyed the caller's copy (parity audit, 2026-08-01). So the FP
/// half of every AArch64 bytecode-tier allocation is computed and thrown away.
///
/// It is kept rather than gated off because the pass is a `k = 8` colouring over
/// at most 64 locals — a few microseconds against a compile — and because the
/// answer becomes live the moment the prerequisite lands. That prerequisite is
/// one thing and it is the same one for both tiers: an FP save area in
/// `Arm64FrameLayout::compute`, driven by `RegAllocResult::used_xmm_regs` for
/// this tier and by `fp_saved` for [`RegFile::aarch64`]'s. Until then, deleting
/// the colouring would only hide the fact that the register file exists and the
/// frame does not.
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
    /// Exception-handler blocks an instruction of this block can throw to — a
    /// subset of `successors`, filled only by [`build_cfg_with_handlers`].
    ///
    /// Kept apart because an exception edge is not an ordinary edge for
    /// liveness: it can be taken from ANY instruction in the block, including
    /// one that precedes a def of a local the handler reads. So the handler's
    /// live-in is live-IN here, not merely live-out — see [`solve_liveness`].
    handler_successors: Vec<usize>,
    gen: u64,  // locals used before defined in this block
    kill: u64, // locals defined in this block
    live_in: u64,
    live_out: u64,
}

/// Does `op` end a block with no fall-through edge, as liveness models it?
///
/// [`bytecode_analysis::falls_through`], except that `ret` keeps its
/// fall-through edge here. Liveness wants a superset of the real
/// successors, and `ret`'s real ones (the instructions after its `jsr`s)
/// are not modelled.
fn is_unconditional(op: u8) -> bool {
    !bytecode_analysis::falls_through(op) && op != 0xa9
}

/// Every local slot below 64 that some instruction in `code[..code_len]`
/// stores to (`[ilfda]store*`, `iinc`, and their `wide` forms), as a bitmask.
/// A linear walk: whether the store is reachable does not matter to the one
/// caller, which only needs "might this slot differ from its incoming value".
fn locals_written_anywhere(code: &[u8], code_len: usize) -> u64 {
    let end = code_len.min(code.len());
    let mut mask = 0u64;
    let mut pc = 0usize;
    while pc < end {
        if let Some((idx, _, true)) = local_access(code, pc) {
            if idx < 64 {
                mask |= 1u64 << idx;
            }
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    mask
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
/// decoding (`bytecode_analysis::step`, `bytecode_analysis::offset_branch_target`, `is_unconditional`,
/// `bytecode_analysis::switch_targets_lenient` — the exact functions `build_cfg` itself uses) so this
/// can never diverge from the CFG this backend already trusts for register
/// allocation.
pub(crate) fn handler_has_unsafe_local_read(
    code: &[u8],
    code_len: usize,
    entry_pc: usize,
    initial_safe_slots: u64,
) -> bool {
    use std::collections::VecDeque;
    // nio21 wave 15 L4: Fx, not std's SipHash. The VM asks this predicate on a
    // compiled handler's resume route (`exception_dispatch`'s
    // `handler_resume_needs_precise_locals`), so a randomly seeded SipHash
    // map per call showed in a caught-AIOOBE loop's profile; the keys are bcis
    // of one method, which no adversary chooses.
    type HashMap<K, V> = rustc_hash::FxHashMap<K, V>;

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

    // A parameter the method WRITES anywhere is not safe on handler entry
    // (round 11). The caller's premise for `initial_safe_slots` is that the
    // interpreter's params-only reconstruction (`exception_dispatch`'s
    // `incoming_args` route) restores them — and it restores the INCOMING
    // value. `static int f(int x) { try { x = 42; thrower(); } catch (E e) {
    // return x; } }` then answered "safe", and the compiled catch returned the
    // original argument instead of 42. Dropping written slots here sends such
    // a method down the precise-frame path instead; a handler that stores the
    // slot itself before reading it is still accepted by the dataflow below.
    let initial_safe_slots = initial_safe_slots & !locals_written_anywhere(code, code_len);

    let mut safe_at: HashMap<usize, u64> = HashMap::default();
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
        let len = bytecode_analysis::step(code, pc);
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
                for t in bytecode_analysis::switch_targets_lenient(code, code_len, pc) {
                    merge_successor(&mut safe_at, &mut worklist, t, code_len, propagated);
                }
            } else if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                merge_successor(&mut safe_at, &mut worklist, t, code_len, propagated);
            }
            // return-family / athrow: no successors — this control-flow
            // path ends here, which is exactly the case the old linear
            // scan got wrong by continuing past it.
        } else {
            if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
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
/// start has no `live_in` for `live_locals_per_pc_inner` to read.
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
        let len = bytecode_analysis::step(code, pc);
        if let Some(target) = bytecode_analysis::offset_branch_target(code, pc) {
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
            for target in bytecode_analysis::switch_targets_lenient(code, code_len, pc) {
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
        //     if !block_starts[pc] && !blocks.is_empty() { pc += bytecode_analysis::step(..); continue; }
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
        // loop already walked the region with the same `bytecode_analysis::step`.
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
            pc += bytecode_analysis::step(code, pc);
            continue;
        }
        let start = pc;
        let idx = blocks.len();
        block_idx_at[start] = idx;

        // Walk to end of block
        loop {
            let len = bytecode_analysis::step(code, pc);
            let next = pc + len;
            let is_branch = bytecode_analysis::offset_branch_target(code, pc).is_some();
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
            handler_successors: Vec::new(),
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
                p += bytecode_analysis::step(code, p);
            }
            last
        };

        let op = code[last_pc];

        // Add branch target
        if let Some(target) = bytecode_analysis::offset_branch_target(code, last_pc) {
            if target < code_len {
                let target_idx = block_idx_at[target];
                if target_idx != usize::MAX {
                    blocks[i].successors.push(target_idx);
                }
            }
        }

        // Add switch targets
        if matches!(op, 0xaa | 0xab) {
            for target in bytecode_analysis::switch_targets_lenient(code, code_len, last_pc) {
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
        pc += bytecode_analysis::step(code, pc);
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
///
/// Returns `true` iff the fixpoint actually CONVERGED, i.e. the loop ran to a
/// sweep in which nothing changed rather than falling out of
/// [`MAX_LIVENESS_ITERATIONS`].
///
/// Why the caller must look at that flag, and why this is not a cosmetic
/// nicety. The transfer function here is monotone-increasing from the all-zero
/// seed: every sweep can only ADD bits to `live_in`/`live_out`, never remove
/// them. So a run cut short by the iteration cap does not produce "an
/// approximate answer" in the harmless direction — it produces a strict
/// **subset** of the true live sets. For the interference graph that is exactly
/// the wrong direction: a missing live bit is a missing interference EDGE, and
/// `color_graph` is then free to hand two simultaneously-live locals the same
/// callee-saved register. The result is a silently wrong value at run time with
/// no diagnostic, because `regalloc_invariants_hold` validates the assignment
/// against the very same truncated graph that produced it and therefore sees
/// nothing wrong.
///
/// A truncated run needs information to travel backwards across more than
/// `MAX_LIVENESS_ITERATIONS` reverse-index sweeps — an irreducible or deeply
/// back-edge-chained CFG, the shape obfuscators and `goto`-heavy decompiler
/// output produce. Rare, but "rare and silently wrong" is the worst class of
/// bug this file can ship, so every caller either bails to a no-register result
/// (`allocate_registers_with`) or widens to the all-live over-approximation
/// (the per-pc liveness maps, where "more locals are live" is the fail-closed
/// direction: it can only make a deopt frame describe more than it had to).
///
/// This also makes the two allocators in this file consistent: the IR linear
/// scan already threads a `converged` flag out of its own fixpoint
/// (`LIVE_MODEL_MAX_ITERATIONS`) and promotes nothing when it is `false`.
#[must_use]
fn solve_liveness(blocks: &mut [BasicBlock], param_live_mask: u64) -> bool {
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
            // What a handler of this block reads is live on ENTRY to it, not
            // only on exit: the throw can happen at the block's first
            // instruction, before any def in the block has run, so `kill` must
            // not be subtracted from it. Taking it only through `live_out`
            // (as this did) let a def later in the protected range hide the
            // handler's read from everything BEFORE the range — and a local
            // defined before a `try`, redefined inside it and read by the
            // `catch`, came out dead between its first def and the `try`. The
            // colouring then shared its register with a temporary living
            // there, and the catch block read the temporary.
            let mut exc_in = 0u64;
            for &h in &blocks[i].handler_successors {
                exc_in |= blocks[h].live_in;
            }
            // live_in = gen | (live_out - kill) | handler live-in
            let new_in = blocks[i].gen | (new_out & !blocks[i].kill) | exc_in;

            if new_in != blocks[i].live_in || new_out != blocks[i].live_out {
                blocks[i].live_in = new_in;
                blocks[i].live_out = new_out;
                changed = true;
            }
        }
    }
    // The loop exits either because a whole sweep changed nothing (`!changed`,
    // the real fixpoint) or because the iteration cap was hit with `changed`
    // still set. Only the first is a converged answer.
    !changed
}

/// Widen an unconverged [`solve_liveness`] result to "every tracked slot is
/// live everywhere".
///
/// This is the fail-closed direction for the per-pc liveness MAPS (as opposed
/// to register allocation, which bails out entirely). Those maps answer "which
/// locals must a deopt/OSR frame describe at this pc"; over-stating liveness
/// can only make a frame describe a slot it could have skipped, and in the
/// worst case make a frame undescribable, which costs OSR entry at that bci.
/// Under-stating it — what a truncated fixpoint does — publishes a live local
/// as `Undefined` and resumes the interpreter with a garbage value.
///
/// The per-block backwards walk that the map builders run on top of this stays
/// exact, so the rows remain a sound over-approximation rather than a constant
/// `u64::MAX`.
fn widen_liveness_to_all_live(blocks: &mut [BasicBlock]) {
    for block in blocks.iter_mut() {
        block.live_in = u64::MAX;
        block.live_out = u64::MAX;
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
///
/// # The mask is live-IN, and it must flow backward (r9)
///
/// The first version of this fix unioned the mask into the ANSWER at each pc
/// (`live_at[pc] = live | mask`) but not into the running `live` set of the
/// backward walk, and [`solve_liveness`] took the handler's live-in only
/// through `live_out`. So the handler's read stopped at the first def inside
/// the range, walking backward, and never reached the code BEFORE the range:
///
/// ```text
///   0: iconst_0 / istore_1     state = 0         <- local 1 live from here...
///   2: iconst_5 / istore_2     tmp = 5
///   4: iload_2  / pop          ...but was computed DEAD here, so `tmp` and
///                              `state` did not interfere and shared a register
///   6: invokestatic            <- try [6, 11): throws, handler reads local 1
///   9: iconst_1 / istore_1     state = 1
///  13: astore_3 / iload_1 / ireturn             <- handler
/// ```
///
/// Every walk now does `live |= mask` after the instruction's own def/use, so
/// the value flows backward like any other live-in, and `solve_liveness` adds
/// the handler's live-in to the block's (see `BasicBlock::handler_successors`).
/// Witness: `a_local_the_handler_reads_is_live_before_the_protected_range`.
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
    let valid: u64 = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };

    for block in blocks {
        // Collect instruction PCs in this block for backward walk
        let mut pcs = Vec::new();
        {
            let mut pc = block.start_pc;
            while pc < block.end_pc {
                pcs.push(pc);
                pc += bytecode_analysis::step(code, pc);
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
                        // Visit only the set bits below `n` (the old loop
                        // tested all `n` per def).
                        let mut rest = others & valid;
                        while rest != 0 {
                            let j = rest.trailing_zeros() as usize;
                            rest &= rest - 1;
                            interference[j] |= bit;
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
            // Live-IN at `pc` includes what a handler covering `pc` reads, and
            // it has to stay in `live` so it reaches the defs BEFORE this pc
            // too — see `handler_live_mask`'s r9 note.
            live |= hmask;
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
        pc += bytecode_analysis::step(code, pc);
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
                    pc += bytecode_analysis::step(code, pc);
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
                    pc += bytecode_analysis::step(code, pc);
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
        pc += bytecode_analysis::step(code, pc);
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
        pc += bytecode_analysis::step(code, pc);
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
                    pc += bytecode_analysis::step(code, pc);
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
        pc += bytecode_analysis::step(code, pc);
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

/// Run register allocation for a method (x86-64), without the exception
/// table. Test-only (round 11 wave 4): production goes through
/// [`allocate_registers_with_handlers`].
#[cfg(test)]
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

/// Register allocation for a method (x86-64) with its exception table modelled.
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
///
/// Test-only (round 11 wave 5): the deopt snapshot now reads
/// [`live_locals_per_pc_all`], which models handlers and every 64-slot window.
#[cfg(test)]
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
///
/// Test-only (round 11 wave 5): production reads window 0 of
/// [`live_locals_per_pc_all`], which is bit-for-bit this answer.
#[cfg(test)]
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
#[cfg(test)]
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
    // The handler pc is a leader because nothing else marks it. The protected
    // range's two ends are leaders for PRECISION: a block that straddles a
    // range start would otherwise have the handler's live-in charged to its
    // first instruction (see `BasicBlock::handler_successors`), i.e. to code
    // that cannot throw into this handler. More leaders only ever means more,
    // smaller blocks — the same monotone-safe direction `build_cfg_with_leaders`
    // documents. An end that is not an instruction boundary (malformed table)
    // is simply never reached by the instruction walk and marks nothing.
    let mut leaders: Vec<usize> = Vec::with_capacity(handlers.len() * 3);
    for &(start, end, handler) in handlers {
        leaders.push(handler);
        leaders.push(start);
        leaders.push(end);
    }
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
            if !blocks[from].handler_successors.contains(&to) {
                blocks[from].handler_successors.push(to);
            }
        }
    }
    blocks
}

/// `live_locals_per_pc_with_handlers` (test-only) for **every** local slot, not only the
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
/// `live_locals_per_pc_with_handlers` returns, so a method with 64 locals or
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
        if !solve_liveness(
            &mut blocks,
            param_live_in_mask_windowed(num_params, param_slots, base),
        ) {
            // See `widen_liveness_to_all_live`: a truncated fixpoint is a
            // SUBSET of the true live sets, and for a deopt frame "not live"
            // is the unsound answer.
            widen_liveness_to_all_live(&mut blocks);
        }
        let handler_ranges = handler_live_in_ranges(&blocks, handlers);
        for block in &blocks {
            let mut pcs = Vec::new();
            {
                let mut pc = block.start_pc;
                while pc < block.end_pc {
                    pcs.push(pc);
                    pc += bytecode_analysis::step(code, pc);
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
                // Into `live`, not only into the row: see `handler_live_mask`.
                live |= handler_live_mask(pc, &handler_ranges);
                rows[pc * words + w] = live;
                covered[pc] = true;
            }
        }
    }
    (rows, covered, words)
}

/// Forward "definitely assigned" dataflow for **every** local slot, in
/// [`live_locals_per_pc_all`]'s layout: bit `i % 64` of word `pc * words + i
/// / 64` is set when slot `i` holds a value on entry to `pc` on every path from
/// the method's entry, exception edges included — a parameter, or a slot
/// every such path stores. `covered[pc]` is `false` at a pc no block reachable
/// from the entry covers, whose row is `0`.
///
/// Its consumer is the debugger's view of a frame resumed from a mode exit
/// (interpreter round i1 wave 19, lane L1): a slot [`live_locals_per_pc_all`]
/// calls dead there is published `Undefined` and resumed as `0`, which is
/// exact for the program but not for a JDWP `StackFrame.GetValues` when the
/// slot is assigned here. Since wave 21 (lane L3) a debugger's compile keeps
/// such a slot alive instead: the table widens the register allocation
/// ([`allocate_registers_keeping_assigned`]) and the snapshot describes the
/// slot from its home; `x64::Compiler::frame_hides_an_assigned_local` refuses
/// an exit only for a slot that still cannot be described.
///
/// Every imprecision errs towards a SMALLER set, i.e. an exit admitted that
/// shows a dead value as `0` (the answer before this analysis existed) — and,
/// for the keep-alive, a slot left to the ordinary liveness, which is sound
/// because the snapshot then publishes it `Undefined` as before: an
/// exception edge carries its source block's ENTRY state (the throw may come
/// before any of the block's stores), and a window whose fixpoint does not
/// converge answers `0` everywhere. The one exception is a store into the
/// upper slot of a `long` / `double`, which is not modelled as unassigning
/// the pair's lower slot: that errs towards an exit refused.
pub fn assigned_locals_per_pc_all(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    param_slots: &[usize],
    handlers: &[(usize, usize, usize)],
    num_locals: usize,
) -> (Vec<u64>, Vec<bool>, usize) {
    let words = num_locals.div_ceil(64).max(1);
    let blocks = build_cfg_with_handlers(code, code_len, handlers);
    let mut rows = vec![0u64; (code_len + 1).saturating_mul(words)];
    let mut covered = vec![false; code_len + 1];
    if blocks.is_empty() {
        return (rows, covered, words);
    }
    // Every block the entry reaches, exception edges included.
    let mut reachable = vec![false; blocks.len()];
    let mut work = vec![0usize];
    reachable[0] = true;
    while let Some(b) = work.pop() {
        for &s in &blocks[b].successors {
            if s < blocks.len() && !reachable[s] {
                reachable[s] = true;
                work.push(s);
            }
        }
    }
    // `(predecessor, the edge is an exception edge)` per block.
    let mut preds: Vec<Vec<(usize, bool)>> = vec![Vec::new(); blocks.len()];
    for (p, block) in blocks.iter().enumerate() {
        if !reachable[p] {
            continue;
        }
        for &s in &block.successors {
            if let Some(list) = preds.get_mut(s) {
                list.push((p, block.handler_successors.contains(&s)));
            }
        }
    }
    for w in 0..words {
        let base = w * 64;
        let bit_of = |pc: usize| -> u64 {
            match local_access(code, pc) {
                Some((idx, _, true)) => idx
                    .checked_sub(base)
                    .filter(|b| *b < 64)
                    .map_or(0, |b| 1u64 << b),
                _ => 0,
            }
        };
        // The stores of each block, in this window.
        let defs: Vec<u64> = blocks
            .iter()
            .map(|block| {
                let mut defs = 0u64;
                let mut pc = block.start_pc;
                while pc < block.end_pc {
                    defs |= bit_of(pc);
                    pc += bytecode_analysis::step(code, pc);
                }
                defs
            })
            .collect();
        let entry = param_live_in_mask_windowed(num_params, param_slots, base);
        // A must-analysis: start at "everything" and only ever shrink.
        let mut ins = vec![u64::MAX; blocks.len()];
        let mut converged = false;
        for _ in 0..MAX_LIVENESS_ITERATIONS {
            let mut changed = false;
            for b in 0..blocks.len() {
                if !reachable[b] {
                    continue;
                }
                let mut state = if b == 0 { entry } else { u64::MAX };
                for &(p, exceptional) in &preds[b] {
                    state &= if exceptional {
                        ins[p]
                    } else {
                        ins[p] | defs[p]
                    };
                }
                if state != ins[b] {
                    ins[b] = state;
                    changed = true;
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
        for (b, block) in blocks.iter().enumerate() {
            if !reachable[b] {
                continue;
            }
            let mut state = if converged { ins[b] } else { 0 };
            let mut pc = block.start_pc;
            while pc < block.end_pc {
                if converged {
                    rows[pc * words + w] = state;
                }
                covered[pc] = true;
                state |= bit_of(pc);
                pc += bytecode_analysis::step(code, pc);
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
        pc += bytecode_analysis::step(code, pc);
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
    if !solve_liveness(&mut blocks, param_live_in_mask(num_params, param_slots)) {
        // See `widen_liveness_to_all_live`: a truncated fixpoint is a SUBSET of
        // the true live sets, and for a deopt frame "not live" is the unsound
        // answer.
        widen_liveness_to_all_live(&mut blocks);
    }

    let handler_ranges = handler_live_in_ranges(&blocks, handlers);

    let mut live_at = vec![0u64; code_len + 1];
    let mut covered = vec![false; code_len + 1];
    for block in &blocks {
        let mut pcs = Vec::new();
        {
            let mut pc = block.start_pc;
            while pc < block.end_pc {
                pcs.push(pc);
                pc += bytecode_analysis::step(code, pc);
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
            // `handler_live_mask` for the witness this fixes — and for why the
            // union goes into `live` itself (so it keeps flowing backward to
            // the pcs before this one) rather than only into this row.
            live |= handler_live_mask(pc, &handler_ranges);
            live_at[pc] = live;
            covered[pc] = true;
        }
    }
    (live_at, covered)
}

/// Run register allocation for a method (ARM64) with the identity argument
/// layout. Test-only (round 11 wave 4): the ARM64 backend calls
/// [`allocate_registers_arm64_with_param_slots`].
#[cfg(test)]
pub fn allocate_registers_arm64(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_arm64_with_param_slots(code, code_len, num_locals, num_params, &[], loops)
}

/// Register allocation for a method (ARM64) with the real argument layout.
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

/// [`allocate_registers_with_handlers`] for a compile whose frames a debugger
/// may read (`x64::BackendRequest::debugger_observes_locals`): a local keeps
/// its home — and so its value — from its store until the next store to it,
/// whether or not anything reads it again, as HotSpot keeps every local alive
/// under `can_access_local_variables`.
///
/// `assigned` is [`assigned_locals_per_pc_all`]'s table (`assigned_words`
/// words per pc; window 0 is the one a register home can serve) for the same
/// method, computed with the method's FULL exception table; see
/// [`keep_assigned_interference`] for how it widens the graph. The deopt
/// snapshot must describe a dead local only when it is set in the SAME table,
/// or it would read a home this allocation did not protect. Interpreter
/// round i1 wave 21, lane L3
/// (`i19-L1-proposal-keep-assigned-locals-alive-under-a-debugger`).
#[allow(clippy::too_many_arguments)]
pub fn allocate_registers_keeping_assigned(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    param_slots: &[usize],
    loops: &[(usize, usize)],
    handlers: &[(usize, usize, usize)],
    assigned: &[u64],
    assigned_words: usize,
) -> RegAllocResult {
    allocate_registers_inner(
        code,
        code_len,
        num_locals,
        num_params,
        param_slots,
        loops,
        local_gpr_pool(),
        &LOCAL_XMMS,
        handlers,
        Some((assigned, assigned_words)),
    )
}

/// The window-0 word of an [`assigned_locals_per_pc_all`] table at `pc`, or
/// `0` past its end (the "nothing to keep" answer).
fn assigned_word_at(assigned: &[u64], words: usize, pc: usize) -> u64 {
    if words == 0 {
        return 0;
    }
    assigned.get(pc.saturating_mul(words)).copied().unwrap_or(0)
}

/// Widen `interference` so no two locals that both hold a value share a home,
/// by the definite-assignment table `assigned` (window 0, `words` words per
/// pc). Two kinds of edge:
///
/// * at every pc, the locals assigned there pairwise;
/// * at every store to local `u`, `u` against every OTHER local assigned on
///   entry to the store.
///
/// The second is the one the argument needs. Suppose `i` is assigned at `pc`,
/// its last store before `pc` on some execution is at `q`, and a store to `u`
/// sharing `i`'s home sits at `pc'` between them. Every path `P` to `pc'`
/// continues, along that execution's own instructions, to `pc` without a store
/// to `i`; `i` is assigned at `pc` on `P` plus that continuation, so it is
/// assigned on `P` already, i.e. at `pc'` — and the second edge forbids the
/// share. The table's own imprecision (an exception edge carries its source
/// block's ENTRY state) only removes bits, and a bit absent at `pc'` stays
/// absent along the continuation, so the argument holds for the computed
/// table, not only for the exact one. The first kind is implied by the second
/// plus the ordinary def-vs-live edges and is added for robustness; both cost
/// compile time only, and only on a debugger's compiles.
fn keep_assigned_interference(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    assigned: &[u64],
    words: usize,
    interference: &mut [u64],
) {
    let n = num_locals.min(64).min(interference.len());
    if n == 0 {
        return;
    }
    let valid: u64 = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    let mut pc = 0usize;
    while pc < code_len {
        let held = assigned_word_at(assigned, words, pc) & valid;
        let mut rest = held;
        while rest != 0 {
            // Cast: a bit index below 64.
            let i = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            interference[i] |= held & !(1u64 << i);
        }
        if let Some((idx, _, true)) = local_access(code, pc) {
            if idx < n {
                let bit = 1u64 << idx;
                let others = held & !bit;
                interference[idx] |= others;
                let mut rest = others;
                while rest != 0 {
                    // Cast: a bit index below 64.
                    let j = rest.trailing_zeros() as usize;
                    rest &= rest - 1;
                    interference[j] |= bit;
                }
            }
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
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
    allocate_registers_inner(
        code,
        code_len,
        num_locals,
        num_params,
        param_slots,
        loops,
        gpr_regs,
        fp_regs,
        handlers,
        None,
    )
}

/// [`allocate_registers_with`], plus the debugger's keep-alive table
/// ([`allocate_registers_keeping_assigned`]); `None` is byte-identical to the
/// allocation before that table existed.
#[allow(clippy::too_many_arguments)]
fn allocate_registers_inner(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    param_slots: &[usize],
    loops: &[(usize, usize)],
    gpr_regs: &[u8],
    fp_regs: &[u8],
    handlers: &[(usize, usize, usize)],
    keep_assigned: Option<(&[u64], usize)>,
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
    if !solve_liveness(&mut blocks, param_live_in_mask(num_params, param_slots)) {
        // The liveness fixpoint did not converge within MAX_LIVENESS_ITERATIONS.
        // The sets in `blocks` are therefore a strict SUBSET of the true live
        // sets (the transfer function only ever adds bits), so
        // `build_interference` below would be missing edges and `color_graph`
        // could alias two simultaneously-live locals — a silent miscompile that
        // `regalloc_invariants_hold` cannot detect, because it would validate
        // the assignment against the same truncated graph.
        //
        // Take the same all-`None` fallback the invariant-failure path below
        // takes: every local keeps its canonical frame slot, which is always
        // correct, and the method is simply compiled without register homes.
        #[cfg(debug_assertions)]
        eprintln!(
            "regalloc: liveness did not converge in {MAX_LIVENESS_ITERATIONS} \
             iterations (num_locals={num_locals}, num_params={num_params}, \
             blocks={}); allocating no register homes",
            blocks.len()
        );
        return RegAllocResult {
            assignments: vec![None; num_locals],
            xmm_assignments: vec![None; num_locals],
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
    }

    // Build full interference graph. The handler ranges keep a local that only
    // the catch block reads from sharing a register with something defined
    // inside the protected range — the interference-graph half of the same fix
    // applied to the liveness map in `live_locals_per_pc_inner`.
    let handler_ranges = handler_live_in_ranges(&blocks, handlers);
    let mut interference = build_interference(code, &blocks, num_locals, &handler_ranges);

    // Every parameter slot interferes with every other one (round 11).
    //
    // The prologue writes EVERY incoming argument into its home, in parameter
    // order (`x64::frames`' prologue, and `param_live_in_mask`'s doc says the
    // same). That is a simultaneous definition of all of them at entry, and
    // liveness alone does not model it: `solve_liveness` seeds only
    // `param_live_mask & !blocks[0].kill`, so a parameter the entry block
    // overwrites before reading is not live-in, and it interferes with nothing
    // that is dead by the time of its overwrite. `static int f(int a, int b) {
    // b = a * 2; return b; }` gave `a` and `b` one register, and the prologue's
    // `mov r12, arg0; mov r12, arg1` handed `iload_0` the value of `b`.
    // Adding the edges costs nothing where they were already implied and is
    // exact where they were not.
    {
        let n = num_locals.min(64);
        let valid = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        let params = param_live_in_mask(num_params, param_slots) & valid;
        for (p, row) in interference.iter_mut().enumerate().take(n) {
            if params & (1u64 << p) != 0 {
                *row |= params & !(1u64 << p);
            }
        }
    }
    // Under a debugger, a dead but assigned local keeps its home too.
    if let Some((assigned, words)) = keep_assigned {
        keep_assigned_interference(
            code,
            code_len,
            num_locals,
            assigned,
            words,
            &mut interference,
        );
    }

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

    // Under a debugger a local assigned at a block start is seeded by the OSR
    // trampoline as if live (it holds the value the snapshot describes), so a
    // dead local coalesced with it is masked instead of clobbering it
    // (`x64::osr`'s `osr_dead_mask`).
    let block_live_in: Vec<(usize, u64)> = blocks
        .iter()
        .map(|b| {
            let kept = keep_assigned.map_or(0, |(assigned, words)| {
                assigned_word_at(assigned, words, b.start_pc)
            });
            (b.start_pc, b.live_in | kept)
        })
        .collect();
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
///
/// Two banks, named after x86-64 because that is where this pass was written.
/// On AArch64 ([`RegFile::aarch64`]) [`RegClass::Xmm`] is the **FP/SIMD** bank
/// and a [`PhysReg::num`] in it is the `D`/`V` register number — the variant
/// name is a legacy spelling, not a target restriction. Nothing in the
/// allocator reads the name; it reads "are these two registers the same
/// physical resource", and the class is what makes `Gp(8)` and `Xmm(8)`
/// different answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegClass {
    /// General-purpose integer registers (`Int`, `Long`, `Ref`).
    Gp,
    /// The floating-point bank: SSE on x86-64, FP/SIMD (`D0`–`D31`) on AArch64.
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
/// `ir_lower`'s linear-scan file, and the vector pool (reserved for a future
/// vector emitter; `x64/vec_emit.rs`, its first consumer, was deleted in round
/// 12 wave 3 with no production caller, so nothing hands it out today). The three
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
/// `docs/jit/vectorization-emitter.md` named this the single highest-risk
/// prerequisite for wiring `emit_vector_loop` (deleted, unwired, in round 12
/// wave 3), and
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
    /// XMM6/XMM7 are here because [`IR_PROLOGUE_SAVED`] pays for them.
    ///
    /// # Why the ceiling is XMM7, and what it is NOT (R14)
    ///
    /// It **was** an encoder limit: `ir_lower`'s `fp_load` / `fp_store` /
    /// `fp_binop` emitted ModRM with `(xmm & 7) << 3` and no REX byte, so XMM8
    /// encoded as XMM0 and the pool could not be widened without producing
    /// silently wrong machine code. That is fixed as of 2026-09-17 — those
    /// three emit REX, and so does `ir_lower::emit_xmm_frame_move`, the fourth
    /// one (it takes its register from [`IR_PROLOGUE_SAVED`], so it would have
    /// been the one to break first).
    /// `every_xmm_encoder_reaches_xmm8_through_rex` pins all four.
    ///
    /// What the ceiling is now is **ownership**, which is a policy decision and
    /// not a defect. XMM8–XMM15 belongs to [`VECTOR_REGION_MAX`]; growing this
    /// array into that range trips [`disjointness_violation`], and the right
    /// answer to that is to decide which authority owns those eight registers,
    /// not to widen both and route around the check. Two things fall out of any
    /// such decision and neither is discretionary:
    ///
    ///   1. every register added here must be added to [`IR_PROLOGUE_SAVED`] on
    ///      **Windows**, which makes XMM6–XMM15 non-volatile — otherwise the
    ///      first use corrupts the caller's floating-point state on one
    ///      platform and not the other;
    ///   2. `ir_lower`'s frame pays 16 bytes per saved register
    ///      (`ir_saved_xmm_bytes`), on every IR-compiled method, used or not.
    ///
    /// `docs/jit/vectorization-emitter.md` is where the ownership question
    /// belongs.
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
    /// hands out, and a frame does not pay 16 bytes to save a register nothing
    /// can name. (Until 2026-09-17 the reason was stronger — the emitter could
    /// not *encode* anything above XMM7 — and that half is now false; see the
    /// R14 note on [`IR_LINEAR_SCAN`].) [`VECTOR_REGION_MAX`] is XMM8–XMM15 and
    /// owes its own save area — see [`vector_pool_is_encodable`].
    ///
    /// `ir_lower::emit_xmm_frame_move` is the emitter that consumes this list,
    /// and it encodes REX, so a wider list would be *emitted* correctly. What
    /// it would not be is free: 16 frame bytes per entry, on every method.
    #[cfg(windows)]
    pub const IR_PROLOGUE_SAVED: &[u8] = &[6, 7];
    /// System V: every XMM is caller-saved, so the prologue saves none.
    #[cfg(not(windows))]
    pub const IR_PROLOGUE_SAVED: &[u8] = &[];

    /// VEC21-7 (the r23 L8 close, L8-2): the save list of a method whose
    /// optimizing body carries a vector pre-loop
    /// (`ir_vector_loop::plan_vector_loops` admitted a loop). On Windows
    /// XMM8-XMM11 are non-volatile, so [`vector_pool_is_encodable`] gives the
    /// pre-loop none of them unless the frame saves them; with them an FP
    /// min/max step (four registers) and a multi-compare scan fit. The method
    /// pays 64 more frame bytes and four `MOVUPS` pairs; every other method
    /// keeps [`IR_PROLOGUE_SAVED`]. System V saves nothing either way.
    #[cfg(windows)]
    pub const IR_PROLOGUE_SAVED_VECTOR: &[u8] = &[6, 7, 8, 9, 10, 11];
    /// System V: as [`IR_PROLOGUE_SAVED`].
    #[cfg(not(windows))]
    pub const IR_PROLOGUE_SAVED_VECTOR: &[u8] = &[];

    /// The widest pool a vector region may be given.
    ///
    /// **Disjoint from [`IR_FP_SCRATCH`] and [`IR_LINEAR_SCAN`]**, which is the
    /// whole point: a vector region can no longer destroy a scalar `double` a
    /// caller left live, so a vector emitter's caller owes no
    /// prove-the-scalars-are-dead argument. It owes a different one, stated by
    /// [`vector_pool_is_encodable`]: on Windows every register here is
    /// callee-saved, so a frame that hands one out must save it.
    ///
    /// XMM8–XMM15 rather than the low half because a vector emitter encodes
    /// with VEX (the deleted `x64/vec_emit.rs` did),
    /// which carries the high bit for free. `IR_LINEAR_SCAN`'s own encoder used
    /// to have no such luxury; since R14 it does (REX rather than VEX), so this
    /// range is now held by **ownership** — the two authorities must be
    /// disjoint and this one claimed it first — rather than by the scalar
    /// emitter being unable to name it.
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
    ///      that saves nothing gets an empty pool and a vector emitter
    ///      must refuse (the deleted `emit_vector_loop` did) — which is the
    ///      correct answer, not a missed optimization.
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

    /// JIT round 25 (item 1b, `CRATONVM_JIT_IR_LS_CALLER_SAVED`, default ON since j27): the
    /// CALLER-saved GPRs `ir_lower` may hand to a value whose live range lies
    /// inside one block and crosses only nodes whose lowering writes none of
    /// them (`ir_lower::ir_op_caller_saved_transparent`). R8-R11 on both ABIs.
    ///
    /// * Never saved by the prologue: they are not in [`IR_GP_LINEAR_SCAN`],
    ///   so `IR_GP_PROLOGUE_SAVED`, the save bands and `ir_gp_file()` are
    ///   unchanged, and a value that uses one costs no save and no restore.
    /// * Not RAX/RCX/RDX (the value tier, `IDIV`, `LCmp`/`FCmp`'s `SETcc DL`),
    ///   not RSI/RDI (callee-saved and in the wide file on Win64; argument
    ///   registers on System V, whose byte forms also need a REX prefix this
    ///   emitter's byte paths were never audited for).
    /// * R8/R9 are argument and deopt-trampoline registers and R10/R11 the
    ///   poll / shadow-stack / epoch-guard scratch, but every one of those
    ///   uses is inside a CALL, a poll, a guard pad or a field / allocation
    ///   sequence -- none of which a transparent node emits, and every one of
    ///   which ends a caller-saved value's residency (`Lowerer::
    ///   invalidate_caller_saved_residency`). The poison stress mode's
    ///   `PUSH R11 ... POP R11` preserves R11.
    ///
    /// Never named by a deopt frame, a pad write-back or a mode exit: every
    /// such site filters on `IR_GP_LINEAR_SCAN`, and the home word stays
    /// written (`Lowerer::install_caller_saved_residency`).
    pub const IR_GP_LINEAR_SCAN_CALLER_SAVED: [u8; 4] = [8, 9, 10, 11];

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

/// RAX — `idiv`'s low dividend word and its quotient.
pub const X86_RAX: PhysReg = PhysReg::gp(0);
/// RCX — the only register a variable shift will read its count from.
pub const X86_RCX: PhysReg = PhysReg::gp(1);
/// RDX — `idiv`'s high dividend word and its remainder.
pub const X86_RDX: PhysReg = PhysReg::gp(2);

/// The AArch64 **general-purpose** linear-scan file, in preference order:
/// X19–X28 (callee-saved under AAPCS64) then X9–X15 (caller-saved temporaries).
///
/// Every exclusion is forced, and none of them is a tuning choice:
///
///   * **X0–X7** are the argument and result registers. A backend marshalling a
///     call writes them *between* nodes, which is work the clobber model cannot
///     see.
///   * **X8** is the indirect result location register.
///   * **X16/X17** are IP0/IP1, and `aarch64_backend` uses X16 for four
///     different scratch purposes — the same "the emitter owns this register"
///     situation RAX/RCX/RDX are in on x86, and the same answer: keep it out of
///     the file rather than try to describe it.
///   * **X18** is the platform register, reserved on Darwin and on Windows.
///   * **X29/X30** are FP and LR.
///   * **SP** is encoding 31, which is also XZR — see the contract at
///     `aarch64::XZR`. Not allocatable on any architecture, and the aliasing
///     makes it the worst possible register to hand out by accident.
///
/// The callee-saved half comes first because this file's whole value on a
/// backend that emits calls constantly is a value surviving one, and that is a
/// property of the register rather than of the analysis.
pub const ARM64_GP_LINEAR_SCAN: [u8; 17] = [
    19, 20, 21, 22, 23, 24, 25, 26, 27, 28, // callee-saved
    9, 10, 11, 12, 13, 14, 15, // caller-saved temporaries
];

/// The AArch64 **FP/SIMD** linear-scan file: D8–D15.
///
/// The same registers as [`ARM64_LOCAL_FPS`] and for the same reason — AAPCS64
/// makes the low 64 bits of V8–V15 the only callee-saved part of the FP bank.
/// Whether a value may actually *live* across a call in one of them is not a
/// property of this list; see the `fp_saved` argument of [`RegFile::aarch64`].
pub const ARM64_FP_LINEAR_SCAN: [u8; 8] = ARM64_LOCAL_FPS;

/// The GPRs [`RegFile::x86_64`] adds over [`RegFile::x86_64_scratch_reserved`]:
/// RAX, RCX, RDX, R10, R11 — every GPR that is neither RSP/RBP nor already in
/// the narrow file.
///
/// R10/R11 carry no fixed-operand rule at all; they are here because the only
/// reason they were ever excluded was that they shared a sentence with the three
/// that do. See the R6 section on [`RegFile::x86_64`].
const FIXED_OPERAND_GPRS: [u8; 5] = [10, 11, 0, 1, 2];

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
    /// RSI/RDI on Win64), then R8/R9 (which mirror `x64.rs`'s `SCRATCH_REGS`),
    /// then **RAX, RCX, RDX, R10 and R11** — every remaining GPR that is not
    /// RSP or RBP. All five of the last group are caller-saved on both ABIs.
    ///
    /// # R6: why those five used to be missing, and what changed
    ///
    /// They were excluded because x86 has *fixed-operand* instructions — `idiv`
    /// reads its dividend in RDX:RAX and returns the quotient in RAX, a variable
    /// shift reads its count in CL — and the allocator had no producer for the
    /// [`FixedConstraint`]s that describe them. [`MachineModel::for_graph`]
    /// pushed RAX/RDX/RCX onto the *clobber* list and the very next line
    /// (`destroyed.retain(|r| regs.contains(*r))`) removed them again, because
    /// they were not in this file. The constraint was therefore not "modelled as
    /// a clobber": it was **dodged**, by permanently costing every method in the
    /// program five of the sixteen GPRs so that no method ever had to describe
    /// it.
    ///
    /// `for_graph` now emits real constraints (see its `Op::Div` / `Op::Rem` /
    /// shift arms), [`allocate_linear_scan`] honours them through
    /// `ls_select_register`'s `required` arm, and [`verify_allocation`]'s proof
    /// 4b checks them — so the five registers can come back.
    ///
    /// # This is not the file `ir_lower` uses
    ///
    /// Worth stating plainly, because the R6 write-up assumed otherwise: this
    /// constructor has **no production caller**. `ir_lower::plan_register_residency`
    /// assembles its own file from [`xmm_roles::IR_GP_LINEAR_SCAN`] and
    /// [`xmm_roles::IR_LINEAR_SCAN`], and that file must NOT gain these five —
    /// `ir_lower` uses RAX/RCX/RDX as its per-opcode value tier and R10/R11 as
    /// safepoint and shadow-stack scratch, none of which is visible to this
    /// model. A backend in that position asks for
    /// [`RegFile::x86_64_scratch_reserved`] by name instead of getting the
    /// narrow file by accident.
    ///
    /// XMM: [`LOCAL_XMMS`] (XMM8–XMM15). Win64 makes XMM6–15 callee-saved; the
    /// SysV ABI makes **every** XMM caller-saved, so on Linux/macOS no FP value
    /// survives a call in a register and every FP interval spanning a call is
    /// split there. That is not a limitation of this allocator, it is the ABI.
    pub fn x86_64() -> RegFile {
        let mut specs = Self::x86_64_scratch_reserved().regs;
        // The fixed-operand registers, in a deliberate preference order: they
        // are handed out only after everything else is taken, so a method that
        // fits in the narrow file is allocated exactly as it was before R6.
        // RAX/RCX/RDX are last within the group because they are the ones a
        // `Div`/`Rem`/shift can take away again.
        for num in FIXED_OPERAND_GPRS {
            specs.push(RegSpec {
                reg: PhysReg::gp(num),
                caller_saved: true,
            });
        }
        RegFile::from_specs(specs)
    }

    /// The x86-64 file **minus** the five registers an emitter is likely to be
    /// using as per-opcode fixed scratch: RAX, RCX, RDX, R10 and R11.
    ///
    /// This is exactly what [`RegFile::x86_64`] returned before R6, kept under a
    /// name that says what it is. A backend whose lowering claims those five for
    /// itself — `ir_lower` does, for every opcode — must allocate over this one,
    /// because the allocator cannot see a use that belongs to no node and a
    /// value placed in RAX would be destroyed by the next opcode's expansion.
    ///
    /// GP: [`LOCAL_REGS`] then R8/R9. XMM: [`LOCAL_XMMS`].
    pub fn x86_64_scratch_reserved() -> RegFile {
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

    /// The AArch64 file, over [`ARM64_GP_LINEAR_SCAN`] and
    /// [`ARM64_FP_LINEAR_SCAN`].
    ///
    /// # A10 / R13: the target hook
    ///
    /// There was no `RegFile::aarch64` at all, so [`allocate_linear_scan`] was
    /// unreachable on AArch64 and the backend ran the bytecode graph colourer
    /// alone. That is a *structural* limit rather than a decision: nothing about
    /// the linear-scan pass is x86-specific — `MachineModel::for_graph`'s only
    /// target-shaped rules are the two fixed-operand ones, and both are gated on
    /// their registers being in the file, so an AArch64 file gets neither.
    ///
    /// What is still missing after this is the **consumer**: `ir_lower` is an
    /// x86-64 emitter, so no AArch64 backend asks for an [`Allocation`] yet.
    /// This constructor is the half that belongs in this file; the other half is
    /// an `aarch64_backend` that calls [`build_live_model`] and
    /// [`allocate_linear_scan`] and reads the result, which is a much larger
    /// piece of work. Adding the file first is still right: it pins the register
    /// choice, the ABI facts and the save-area obligation below in the place
    /// they belong, so the consumer is not also the place they get invented.
    ///
    /// # `fp_saved` — why this takes an argument and `x86_64` does not
    ///
    /// `fp_saved` is the set of FP registers **the calling frame's prologue
    /// saves and every exit restores**, in the same currency as
    /// [`xmm_roles::IR_PROLOGUE_SAVED`] and `vector_pool_is_encodable`'s
    /// `frame_saved`.
    ///
    /// AAPCS64 makes D8–D15 callee-saved, so the ABI *permits* a value to live
    /// across a call in one — but `aarch64_backend`'s prologue saves **only
    /// GPRs**, so today a value left in D8 across a call is destroyed by the
    /// callee restoring it and the caller's copy is gone. That is exactly the
    /// bug that made this backend stop homing float locals in FP registers at
    /// all (2026-08-01; see the header of `aarch64_backend` and
    /// [`ARM64_LOCAL_FPS`]).
    ///
    /// "Is it callee-saved" is therefore the wrong question here, and
    /// `cfg!(target_os = ...)` cannot answer the right one. The right one is
    /// "does THIS frame give it back", which only the caller knows. Pass `&[]`
    /// for the prologue that exists today and get D8–D15 as volatile registers:
    /// correct, and pessimistic in the safe direction — every FP interval
    /// spanning a call is split there. Pass [`ARM64_FP_LINEAR_SCAN`] once the FP
    /// save area exists and the registers start paying.
    ///
    /// On x86-64 the same question has a `cfg`-knowable answer (`ir_lower`'s
    /// prologue saves exactly what its file hands out), which is why
    /// [`RegFile::x86_64`] needs no argument. The asymmetry is the finding, not
    /// an inconsistency.
    pub fn aarch64(fp_saved: &[u8]) -> RegFile {
        let mut specs: Vec<RegSpec> = Vec::new();
        for &num in ARM64_GP_LINEAR_SCAN.iter() {
            specs.push(RegSpec {
                reg: PhysReg::gp(num),
                // X19–X28 are callee-saved under AAPCS64 and X9–X15 are not.
                // Unlike the FP half this needs no save-area argument: a
                // callee-saved GPR is given back by the CALLEE, so the caller
                // owes nothing for reading one it never wrote. A frame that
                // WRITES one still has to save it, but that is the emitter's
                // obligation at the point of the write, not a fact about
                // whether a value may live across a call.
                caller_saved: !(19..=28).contains(&num),
            });
        }
        for &num in ARM64_FP_LINEAR_SCAN.iter() {
            specs.push(RegSpec {
                reg: PhysReg::xmm(num),
                // Not "the ABI says so" but "does this frame give it back".
                caller_saved: !fp_saved.contains(&num),
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
/// One producer today: fixed-operand instructions (x86 `idiv` reads its
/// dividend in RAX, a variable shift reads its count in CL), which
/// [`MachineModel::for_graph`] emits for every graph whose register file
/// contains the register in question. (An entry-ABI producer,
/// `pin_entry_params`, pinned every `Op::Param` at position 0 — which a
/// `Param` scheduled later does not cover — and had no caller; round 11 wave 2
/// deleted it.)
///
/// # What a constraint does and does not promise
///
/// It promises two things, both checked by [`verify_allocation`]'s proof 4b:
///
/// * no OTHER value occupies `reg` at `pos`;
/// * if `node` is in a register at `pos`, it is `reg` and not some other one.
///
/// It does **not** promise that `node` is in a register at `pos` at all. A
/// pinned value, a value that lost the eviction contest, or a value whose
/// interval was split before `pos` sits in its home word there, and the emitter
/// loads it into `reg` itself — which is exactly what an emitter has to be able
/// to do anyway for an operand the allocator never promoted. Requiring more
/// would turn a register-pressure event into a failed compile for no gain.
///
/// # Early clobber
///
/// A constraint at `pos` and a clobber of the same register at `pos` are not a
/// contradiction: `idiv` reads RAX and then writes it. The allocator lets the
/// constrained value hold `reg` *through* `pos` and releases it at `pos + 1`
/// (see [`ls_select_register`]), and proof 4 exempts exactly that one segment.
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
    ///
    /// **Invariant: positions are STRICTLY INCREASING — sorted *and* unique —
    /// and each entry's register vector is itself sorted and deduplicated.**
    /// Not a tidiness preference: [`MachineModel::clobbered_at`] finds an entry
    /// with `binary_search_by_key`, which returns *an arbitrary* match among
    /// equal keys. Two entries at one position therefore make one of them
    /// invisible, and an invisible clobber is a value left in a caller-saved
    /// register across the call that destroys it.
    ///
    /// Use [`MachineModel::add_clobbers`] to add to this list, or restore the
    /// invariant with [`MachineModel::canonicalize_clobbers`] after building it
    /// some other way. [`verify_allocation`] rejects a model that violates it,
    /// so a producer that forgets costs its method the optimizing tier rather
    /// than miscompiling it.
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
    /// Clobber positions at which the instruction reads its register operands
    /// **before** it destroys anything. Sorted, deduplicated. **Empty by
    /// default** — [`MachineModel::for_graph`] never fills it.
    ///
    /// At such a position a value whose live range ENDS there, because that
    /// position is its last use, may hold a clobbered register through it: the
    /// read happens first, and under the endpoint-inclusive rule a segment
    /// `[lo, p]` does not survive `p`. Without this, a `double` whose last use
    /// is a call's argument gets `[lo, p-1] reg, [p, p] memory` — two segments
    /// — and a consumer that promotes only single-segment values loses it for
    /// its whole life. See
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    ///
    /// **It is a property of the EMITTER, not of the op**, which is why this
    /// module never sets it on its own: `cqo` destroys RDX before `idiv` reads
    /// its divisor, a backend may emit a frame record (itself a `CALL`) before
    /// marshalling a call's arguments, and a back edge's safepoint poll runs
    /// BEFORE the edge's phi copies read their sources. A consumer lists a
    /// position only after auditing that every instruction it emits there
    /// reads its operands before the first one that clobbers a register — see
    /// [`MachineModel::mark_operand_reads_first_where`] for the op-derived set.
    ///
    /// Honoured by `ls_select_register` and exempted by [`verify_allocation`]'s
    /// proof 4 under the same three conditions: the position is listed, it is
    /// the end of the value's live range, and it is the value's last use.
    pub reads_precede_clobber: Vec<usize>,
}

/// Work budget for the liveness fixed point, in `nodes × blocks` units.
///
/// The same budget `ir_lower::plan_slots` uses, for the same reason: past this
/// the analysis is skipped, every value is treated as live for the whole method
/// and nothing is promoted to a register. A compiler must not turn a
/// pathological graph into a pathological compile.
pub(crate) const LIVE_MODEL_WORK_BUDGET: usize = 8_000_000;

/// Hard cap on liveness fixed-point iterations; mirrors
/// `ir_lower::SLOT_PLAN_MAX_ITERATIONS`. A truncated may-analysis
/// under-approximates liveness, so a non-converging fixed point discards the
/// answer (nothing is promoted) rather than using a partial one.
pub(crate) const LIVE_MODEL_MAX_ITERATIONS: usize = 256;

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
    /// `loop_depth[b]` = nesting depth of block `b`, derived from the dominator
    /// relation by [`ls_loop_depths`] — **not** from block order, which after
    /// `ir_schedule::layout_blocks` is not a reverse postorder and over-counted
    /// every block a frequency-driven layout moved into a loop's index window.
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
    ///
    /// Over-inclusive for that purpose: a value live THROUGH a loop it is never
    /// read in is carried too. [`LiveModel::carried_and_read`] is the narrower
    /// set the spill heuristic uses.
    pub carried: Vec<bool>,
    /// `carried_and_read[id]` = [`LiveModel::carried`] AND the value is read
    /// inside a loop whose back edge its range covers — the values whose
    /// eviction really is paid per iteration (round 11 wave 5).
    ///
    /// A value defined before a loop and read only after it is `carried` (its
    /// one contiguous range covers the back edge) but evicting it costs one
    /// store and one reload around the loop. `ls_pick_victim` used to divide
    /// its next-use distance by `ls_carry_relief()` like a real carried value's
    /// — to 0 for any loop shorter than 64 positions — and so evicted in-loop
    /// temporaries in its place, paying a store and a reload per iteration to
    /// keep a register nothing in the loop reads.
    pub carried_and_read: Vec<bool>,
    /// Maximum number of intervals covering any one position. The floor a
    /// perfect allocation could reach; reported, never sized from.
    pub peak_live: usize,
    /// False when the fixed point was skipped or did not converge. Every range
    /// is then the whole method and nothing is promotable.
    pub converged: bool,
    /// `block_live_in[b]` = the bitset (bit `id` of word `id / 64`) of values
    /// live on entry to block `b`, exactly as the backward fixed point computed
    /// it. **Empty when the model did not converge** (or was hand-built): ask
    /// through [`LiveModel::live_in_at`], which answers `None` — "unknown" —
    /// rather than a guess.
    ///
    /// A phi of `b` is NOT live-in at `b` (it is defined there), and a phi
    /// source is live-OUT of the predecessor, not live-in at the merge: the
    /// edge copies read it at the predecessor's edge position. Kept (round 9
    /// wave 2) for [`split_edge_resolution`], which must not write a register
    /// on an edge for a value nothing past the edge reads — the range is one
    /// contiguous interval and over-approximates exactly that question.
    pub block_live_in: Vec<Vec<u64>>,
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
            // Round 11 wave 13 — mirrors `ir_lower::op_defines_result_slot`.
            | Op::ExactClassIs { .. }
            // JIT round 19 (lane osr19) — mirrors `ir_lower::op_defines_result_slot`.
            | Op::ClassWordOf
            // Round 11 wave 14 — mirrors `ir_lower::op_defines_result_slot`.
            | Op::OsrEntryFlag { .. }
            | Op::OsrLocal(_)
            // nio21 wave 3 L2 — mirrors `ir_lower::op_defines_result_slot`.
            | Op::CallThrew { .. }
            // JIT round 19 wave 3 (lane eager3) — mirrors `op_defines_result_slot`.
            | Op::ClassInitTest { .. }
            | Op::CurrentThreadMirror { .. }
            // JIT round 20 wave 11 (lane pIndy) -- mirrors `op_defines_result_slot`.
            | Op::IndyRootRef { .. }
            // JIT round 19 wave 5 (lane handler5) — mirrors `op_defines_result_slot`.
            | Op::CatchEntryFlag { .. }
            | Op::CaughtException
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
            // JIT round 21 (lane charat, STRINTR21-5) — mirrors
            // `ir_lower::op_defines_result_slot`.
            | Op::StrCharDecode
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::Call { .. }
            // cov-01 — mirrors `ir_lower::op_defines_result_slot`.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::LambdaIntToDouble
            | Op::IndyBridge { .. }
            // Round 16 (lane matrix) — mirrors `ir_lower::op_defines_result_slot`.
            | Op::MultiNewArray { .. }
            // nio21 w15 L3 — mirrors `ir_lower::op_defines_result_slot`.
            | Op::NewCp { .. }
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
            // `putstatic` always calls its helper, which can run `<clinit>`;
            // `ir_lower` publishes the map before it.
            | Op::StoreStatic { .. }
            | Op::LambdaIntToDouble
            | Op::IndyBridge { .. }
            // Round 16 (lane matrix): `jit_multianewarray_n` allocates.
            | Op::MultiNewArray { .. }
            // nio21 w15 L3: `jit_new_object_cp` resolves and allocates.
            | Op::NewCp { .. }
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
    // zt t19/zb2: under an armed ZGC load barrier `ir_lower` lowers a reference
    // `ArrayLoad` to `MOV RAX, jit_aaload ; CALL RAX`, returning into the body
    // (`lower_ref_array_load_through_barrier_helper`). Same process flag the
    // lowering reads; production sets it once, before the first compile.
    // zt t21/zb4: and the same predicate -- a colored-store arming decodes the
    // element inline (no call), and both sides ask
    // `zgc_ref_element_load_calls_helper`.
    if matches!(op, Op::ArrayLoad(crate::ir::MemKind::Ref))
        && crate::x64::zgc_ref_element_load_calls_helper()
    {
        return true;
    }
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
            // `MOV RAX, jit_putstatic_* ; CALL RAX`, always, returning into
            // the body.
            | Op::StoreStatic { .. }
            | Op::LambdaIntToDouble
            | Op::IndyBridge { .. }
            // Round 16 (lane matrix): `MOV RAX, jit_multianewarray_n ; CALL RAX`.
            | Op::MultiNewArray { .. }
            // nio21 w15 L3: `CALL jit_new_object_cp` (`emit_new_object_cp_stub`).
            | Op::NewCp { .. }
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

/// Is `node` an `Op::Rem` that `ir_lower` lowers INLINE, i.e. not a call?
///
/// [`ir_op_is_call`] sees only the op, so it has to list `Op::Rem` whole — the
/// `Float`/`Double` remainder really is `MOV RAX, jit_frem|jit_drem ; CALL RAX`.
/// The integral one is not: `ir_lower`'s `Op::Rem` arm emits
/// `CDQ|CQO ; IDIV ECX|RCX` between an inline `MIN / -1` guard and a
/// zero-divisor guard whose failure edge is a deopt jump (the same
/// never-returns-into-the-body shape `Op::Guard` is exempted for). No helper is
/// called and no XMM register is touched.
///
/// Treating it as a call cost exactly the values this allocator exists for:
/// every `double` live across an `i % k` in a loop body — the
/// `sum += (i % 17) * 0.5` shape — was split at the remainder on every file
/// where the XMM registers are caller-saved (all of them on System V, XMM2–5
/// on Win64), and the production consumer promotes no split value at all, so
/// the accumulator lost its register for the whole loop. `MachineModel::for_graph`
/// already gates its `Div`/`Rem` RAX/RDX rule on an integral type for the same
/// reason. Pinned by `an_integer_remainder_is_not_a_call`.
fn is_inline_integer_rem(node: &crate::ir::Node) -> bool {
    matches!(node.op, Op::Rem) && matches!(node.ty, IrType::Int | IrType::Long)
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
pub(crate) fn phi_edge_interfere_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_PHI_EDGE_INTERFERE")
}

/// `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE=1` (opt-in; interpreter round i2
/// wave 65, lane L2; `i64-L2-proposal-one-liveness-core-for-plan-slots-and-the-live-model`):
/// [`build_live_model`] and `ir_lower::plan_slots` both run their position
/// numbering, upward-exposed uses, φ edge reads, backward fixed point and
/// per-block widening through ONE function, [`liveness_core`], instead of
/// their two hand-kept copies.
///
/// A pure refactor: with the same [`LivenessRules`] the core's answer is the
/// legacy answer, field for field (`i65_l2_*` unit tests in both files compare
/// them on the test graphs). Opt-in only until the host has run those tests and
/// the byte-identity check the proposal names (`CRATONVM_DBG_IR_SLOTS` frame
/// words and the `[ir] linear scan:` census, switch on vs off); after that the
/// legacy copies can be deleted and this switch retired.
pub fn shared_liveness_core_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_SHARED_LIVENESS_CORE")
}

/// `CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES` (**default ON** since 2026-10-11,
/// `=0` kill switch; interpreter round i2 wave 65, lane L2;
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// `ir_lower::plan_slots` applies the φ edge-interference rule of
/// [`phi_edge_interfere_enabled`] to its home ranges too, so a φ's range
/// covers every incoming edge position exactly as the allocator's interval
/// does. Without it `plan_register_residency`'s `ranges_agree` is false in
/// every method with a φ (every loop), and `CRATONVM_JIT_IR_LS_SPLITS` can
/// never admit a split value there.
///
/// Implies [`shared_liveness_core_enabled`] for the slot plan (only the core
/// knows the rule) and follows the `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE` kill
/// switch, so the two models stay one model. φs are pinned, so the colouring
/// and the frame layout do not move; what moves is the φ's published range
/// (read by the dead-home clears and the residency second opinions, both of
/// which only become more conservative over a longer range) and `peak_live`.
pub fn slots_phi_edge_ranges_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES")
}

/// `CRATONVM_JIT_IR_ONE_LIVENESS_PASS=1` (opt-in; interpreter round i2 wave
/// 66, lane L2; `i65-L2-proposal-one-liveness-pass-per-compile`): the
/// optimizing lowering entry runs [`liveness_core`] ONCE, with the live
/// model's rules ([`live_model_rules`]) and the read list kept, and hands the
/// answer to both consumers: `ir_lower::plan_slots` (through
/// [`LivenessCore::slot_view`]) and the allocator's model
/// ([`live_model_from_core`], used by the residency planner and the OSR entry
/// stubs). Today each runs the same fixed point on the same graph.
///
/// It applies only when the two consumers would have asked the same question:
/// the same rules (the slot plan's token and φ rules equal the model's) and
/// the same value set. Otherwise the compile keeps two passes, exactly as
/// without the switch. In practice the rules agree only when
/// `CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES=1` is on (or the φ edge rule is
/// killed with `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=0`), and the token rules
/// agree unless only `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` is on.
///
/// When it applies, the answer is the one the shared-core path
/// (`CRATONVM_JIT_IR_SHARED_LIVENESS_CORE`) gives for both consumers: the
/// slot plan and the live model are byte-identical to it (`i66_l2_*` unit
/// tests). Read once per compile.
pub fn one_liveness_pass_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_ONE_LIVENESS_PASS")
}

/// `CRATONVM_JIT_IR_LIVENESS_DENSE_VALUES=1` (opt-in; interpreter round i2
/// wave 67, lane L2; `i66-L2-proposal-dense-value-numbering-for-the-liveness-core`):
/// [`liveness_core`] numbers the wanted values densely (`0..values`) before
/// the fixed point and sizes its per-block bit sets by `values` instead of by
/// arena node count. Control nodes, memory tokens, frame states, unplaced
/// nodes and unwanted constants no longer occupy bits the fixed point and the
/// widening scan walk.
///
/// A pure refactor: the answer, the `live_in`/`live_out` sets included (they
/// are mapped back to node-id bits, `words` unchanged), is the sparse core's,
/// field for field (`i67_l2_*` unit tests, every test graph and every rule
/// combination). The work budget is still `nodes × blocks`; see
/// [`liveness_budget_by_values_enabled`] for the behaviour change. Read once
/// per [`liveness_core`] call. Applies wherever the core runs: the live model
/// under `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE`, the slot plan under the
/// core's slot path, and the single pass of `CRATONVM_JIT_IR_ONE_LIVENESS_PASS`;
/// the legacy copies are not touched.
pub fn liveness_dense_values_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LIVENESS_DENSE_VALUES")
}

/// `CRATONVM_JIT_IR_LIVENESS_BUDGET_BY_VALUES=1` (opt-in; interpreter round i2
/// wave 67, lane L2): [`liveness_core`]'s work budget counts `values × blocks`
/// (the wanted values) instead of `nodes × blocks`. A **behaviour change**: a
/// method whose arena is over budget today but whose value set is not starts
/// being analysed (ranges instead of the whole-method fallback). Implies
/// [`liveness_dense_values_enabled`], because only the dense sets are sized by
/// the quantity the budget then counts.
pub fn liveness_budget_by_values_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LIVENESS_BUDGET_BY_VALUES")
}

/// `CRATONVM_JIT_IR_LIVENESS_WORKLIST=1` (opt-in; interpreter round i2 wave
/// 69, lane L2; `i67-L2-proposal-worklist-liveness-fixed-point`):
/// [`liveness_core`]'s backward fixed point visits, in each pass, only the
/// blocks one of whose successors' live-in set changed since the block's last
/// visit ([`ls_fixed_point_worklist`]). The passes keep the round-robin order
/// (reverse block index), so a skipped block is exactly one the round-robin
/// pass would have recomputed to no change: the state after every pass, the
/// pass count and the `converged` flag are the round-robin ones, converged or
/// cut off by the iteration cap.
///
/// A pure refactor: the answer is the round-robin core's, field for field
/// (`i69_l2_*` unit tests: every test graph, rule and value set, every
/// iteration cap, and random CFGs with exception-style, irreducible, self-loop
/// and out-of-range edges). Read once per [`liveness_core`] call; combines
/// with [`liveness_dense_values_enabled`].
pub fn liveness_worklist_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LIVENESS_WORKLIST")
}

/// `CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST=1` (opt-in; interpreter round i2
/// wave 70, lane L2; `i69-L2-proposal-one-dirty-pass-driver-for-every-liveness-copy`):
/// the hand-written copies of the backward fixed point the default JIT runs
/// ([`build_live_model_legacy`], `ir_lower::plan_slots_liveness_legacy`,
/// `ir_lower::value_use_ranges` and `ir_lower::chain_read_colour_ranges`) run
/// it through [`ls_fixed_point_worklist`] instead of their own whole-pass
/// loop. Builds on the wave-69 driver (`CRATONVM_JIT_IR_LIVENESS_WORKLIST`,
/// which routes only [`liveness_core`]) but is independent of that switch.
///
/// A pure refactor: each copy's transfer is the driver's (`lout |= phi_out |
/// li[succ]`, `li |= uses | (lout & !defs)`; the chain copy has no φ-out term
/// and passes zeros), and the driver keeps the round-robin pass order, so the
/// sets after every pass, the pass count and the `converged` verdict are the
/// legacy loop's, converged or cut off (`i70_l2_*` unit tests). Read once per
/// copy per compile.
pub fn legacy_liveness_worklist_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST")
}

/// `CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS=<n>` (a knob, default unset
/// = no gate; interpreter round i3 wave 71, lane L2;
/// `i70-L2-proposal-one-predecessor-index-per-schedule`): the two worklist
/// switches ([`liveness_worklist_enabled`], [`legacy_liveness_worklist_enabled`])
/// route a fixed point through the dirty-block driver only on a schedule of
/// at least `n` blocks; a smaller one keeps the round robin. The round-i2
/// benches measured the worklist at 0.54-0.91x of the round robin on graphs
/// of 40+ blocks but 1.07-1.21x on method-sized ones (1-16 blocks), so the
/// gate keeps each graph on the side that wins.
///
/// A pure refactor whatever `n` is: both fixed points give the same answer,
/// pass for pass (the `i69_l2_*` / `i70_l2_*` tests). Unset, empty, `0` or
/// junk is no gate (`0`). Read once per fixed point, and only when one of the
/// worklist switches is on.
pub fn liveness_worklist_min_blocks() -> usize {
    cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

/// `CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS=1` (opt-in; interpreter round i3
/// wave 71, lane L2; `i70-L2-proposal-one-predecessor-index-per-schedule`):
/// a worklist fixed point over a schedule reads each block's own
/// `predecessors` list (which `ir_schedule` builds beside `successors`, edge
/// for edge, and remaps with them in the layout pass) instead of inverting
/// the successor lists into a fresh CSR index on every call. Under the
/// legacy worklist a default compile ran that inversion three or four times
/// over the same blocks (live model, slot plan, value-read ranges, chain-read
/// ranges): two edge walks and three allocations each.
///
/// A pure refactor: the dirty sets depend only on the predecessor SETS, which
/// are the inverted successor lists on every schedule `ir_schedule` builds
/// (`i71_l2_*` tests check it on every scheduled test graph), so the answer,
/// the pass count and the visit count are the CSR driver's. Out-of-range
/// entries are ignored, as the CSR build ignores out-of-range successors.
/// Only has an effect where a worklist switch is on.
pub fn liveness_schedule_preds_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS")
}

/// The fixed point a worklist switch runs on a schedule of `nb` blocks under
/// the wave-71 knobs: the round robin below the block-count gate
/// ([`liveness_worklist_min_blocks`]), otherwise the worklist, reading the
/// schedule's own predecessor lists under [`liveness_schedule_preds_enabled`].
pub(crate) fn worklist_fixed_point_for(nb: usize) -> LivenessFixedPoint {
    if nb < liveness_worklist_min_blocks() {
        return LivenessFixedPoint::RoundRobin;
    }
    let schedule_preds = liveness_schedule_preds_enabled();
    if liveness_postorder_enabled() {
        LivenessFixedPoint::WorklistPostorder { schedule_preds }
    } else if schedule_preds {
        LivenessFixedPoint::WorklistSchedulePreds
    } else {
        LivenessFixedPoint::Worklist
    }
}

/// `CRATONVM_JIT_IR_LIVENESS_POSTORDER=1` (opt-in; interpreter round i3 wave
/// 71, lane L2; `i69-L2-proposal-postorder-pass-order-for-the-backward-fixed-point`):
/// where a worklist switch runs the worklist, its passes visit the blocks in
/// a CFG postorder ([`ls_postorder`]) instead of reverse layout index, so a
/// block is visited after its successors whatever the hot/cold layout did
/// (a cold block laid out after the code it branches back into costs the
/// index order one extra pass per such edge on a propagation path).
///
/// A **behaviour change only for cut-off runs**: the converged answer is the
/// least fixed point whatever the order, so it is the round robin's, set for
/// set; the pass count differs, so a graph that settles in exactly the cap
/// under one order may settle sooner under the other, and the sets of a
/// cut-off run differ (every caller treats an unconverged run as the whole
/// method). Combines with the gate and with
/// [`liveness_schedule_preds_enabled`]. Read with the other worklist knobs.
pub fn liveness_postorder_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LIVENESS_POSTORDER")
}

/// The fixed point the legacy liveness copies run under the current switches
/// on a schedule of `nb` blocks ([`legacy_liveness_worklist_enabled`], then
/// [`worklist_fixed_point_for`]).
pub(crate) fn legacy_liveness_fixed_point(nb: usize) -> LivenessFixedPoint {
    let picked = if legacy_liveness_worklist_enabled() {
        worklist_fixed_point_for(nb)
    } else {
        LivenessFixedPoint::RoundRobin
    };
    // JIT round 19 wave 10 (lane lv10): the round robin through the shared
    // driver instead of each copy's own whole-pass loop.
    if picked == LivenessFixedPoint::RoundRobin && liveness_one_driver_enabled() {
        LivenessFixedPoint::RoundRobinDriver
    } else {
        picked
    }
}

/// `CRATONVM_JIT_IR_LIVENESS_ONE_DRIVER=1` (opt-in; JIT round 19 proposal
/// wave 10, lane lv10; `i71-L2-proposal-retire-the-four-hand-written-round-robin-loops`):
/// the four legacy liveness copies ([`build_live_model_legacy`],
/// `ir_lower::plan_slots_liveness_legacy`, `ir_lower::value_use_ranges`,
/// `ir_lower::chain_read_colour_ranges`) run their round robin through
/// [`ls_fixed_point_round_robin`] ([`LivenessFixedPoint::RoundRobinDriver`])
/// instead of their own hand-written whole-pass loop. The hand-written loops
/// stay, as the switch-off arm and as the oracle the `lv10_*` tests compare
/// the driver against.
///
/// A pure refactor: the transfer is the same (`lout |= phi_out | li[succ]`,
/// `li |= uses | (lout & !defs)`; the chain copy's zero φ-out), the pass order
/// is the same (reverse block index), and both count `nb` visits per pass,
/// so the sets, the verdict and the visit count are the loop's, converged or
/// cut off (`lv10_*` unit tests, caps 0 to 3 and the real cap). Read once per
/// copy per compile, with the other legacy liveness knobs.
pub fn liveness_one_driver_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LIVENESS_ONE_DRIVER")
}

/// `CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH=1` (opt-in; JIT round 19 proposal
/// wave 10, lane lv10; `i71-L2-proposal-one-liveness-scratch-per-compile`):
/// the optimizing lowering entry (`ir_lower::lower_inner_attempt`) owns one
/// [`LivenessScratch`] per compile and hands it to the legacy liveness copies
/// it runs (the slot plan's, the dead-home value ranges', the live model's):
/// the position model and the φ edge table are built once and the per-copy
/// bit sets come from a reusable pool instead of fresh allocations.
///
/// A pure refactor: each copy keeps its own value set and rules; the table is
/// the walk each copy made, in the same order (`lv10_*` unit tests: every
/// copy's answer with and without the scratch, two compiles in a row through
/// one scratch). Read once per compile.
pub fn liveness_shared_scratch_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH")
}

/// The linear position model every legacy liveness copy numbers the same way:
/// each scheduled node, then the block's terminator, then one outgoing-edge
/// position per block (JIT round 19 wave 10, lane lv10, part of
/// [`LivenessScratch`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LsPositionModel {
    pub(crate) pos_of: Vec<Option<usize>>,
    pub(crate) span: Vec<(usize, usize)>,
    pub(crate) block_of_pos: Vec<usize>,
    pub(crate) total_positions: usize,
}

impl LsPositionModel {
    /// The walk [`build_live_model_legacy`] makes (the slot plan's is the same
    /// without `block_of_pos`).
    pub(crate) fn build(n: usize, schedule: &Schedule) -> Self {
        let nb = schedule.blocks.len();
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
            let end = seq;
            block_of_pos.push(b);
            seq += 1;
            span.push((start, end));
        }
        LsPositionModel {
            pos_of,
            span,
            block_of_pos,
            total_positions: seq,
        }
    }
}

/// One incoming edge of a merge that has φs: the predecessor block whose
/// outgoing-edge position reads the φs' `k+1`-th inputs, and the φs of that
/// merge (`phis[lo..hi]` of [`LsPhiEdges::phis`], ascending node id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LsPhiEdge {
    pub(crate) pred: usize,
    pub(crate) k: usize,
    pub(crate) lo: usize,
    pub(crate) hi: usize,
}

/// The φ edge walk of the legacy copies, flattened (JIT round 19 wave 10,
/// lane lv10): for each block in index order whose control is a `Merge` or
/// `Region` with φs, for each control input `k` that resolves to a block
/// (`ls_ctrl_block_of`), one [`LsPhiEdge`]. Every φ of the merge is listed,
/// whatever its type: a copy that skips some φ types (the value ranges skip
/// memory/control/void φs) skips them at the visit, which leaves the order of
/// the rest unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LsPhiEdges {
    pub(crate) edges: Vec<LsPhiEdge>,
    pub(crate) phis: Vec<NodeId>,
}

impl LsPhiEdges {
    pub(crate) fn build(graph: &Graph, schedule: &Schedule) -> Self {
        let nb = schedule.blocks.len();
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
        let mut out = LsPhiEdges::default();
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
            let lo = out.phis.len();
            out.phis.extend_from_slice(phis);
            let hi = out.phis.len();
            for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                let pred = match ls_ctrl_block_of(graph, schedule, ctrl_in) {
                    Some(pred) if pred < nb => pred,
                    _ => continue,
                };
                out.edges.push(LsPhiEdge { pred, k, lo, hi });
            }
        }
        out
    }

    /// The φs of `edge`.
    #[inline]
    pub(crate) fn phis_of(&self, edge: &LsPhiEdge) -> &[NodeId] {
        self.phis.get(edge.lo..edge.hi).unwrap_or(&[])
    }
}

/// What a [`LivenessScratch`] reused and built, for the tests and the bench.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LivenessScratchStats {
    pub(crate) positions_built: usize,
    pub(crate) positions_reused: usize,
    pub(crate) phi_edges_built: usize,
    pub(crate) phi_edges_reused: usize,
    pub(crate) bufs_fresh: usize,
    pub(crate) bufs_reused: usize,
}

/// One per optimizing compile (JIT round 19 proposal wave 10, lane lv10,
/// [`liveness_shared_scratch_enabled`]): the parts of a legacy liveness run
/// that do not depend on the copy's value set or rules, built on first use
/// and kept, plus a pool of `u64` buffers the copies hand back when done.
///
/// Bound to one `(graph, schedule)`: the cached parts are rebuilt whenever a
/// call presents a different node count, block count or block array (a
/// cheap guard; the owner creates one scratch per compile and drops it at the
/// end, so in practice the key never changes).
#[derive(Debug, Default)]
pub(crate) struct LivenessScratch {
    key: Option<(usize, usize, usize)>,
    positions: Option<LsPositionModel>,
    phi_edges: Option<LsPhiEdges>,
    pool: Vec<Vec<u64>>,
    pub(crate) stats: LivenessScratchStats,
}

impl LivenessScratch {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn rekey(&mut self, graph: &Graph, schedule: &Schedule) {
        let key = (
            graph.nodes.len(),
            schedule.blocks.len(),
            schedule.blocks.as_ptr() as usize,
        );
        if self.key != Some(key) {
            self.key = Some(key);
            self.positions = None;
            self.phi_edges = None;
        }
    }

    /// The position model of `(graph, schedule)`, built on first use.
    pub(crate) fn positions(&mut self, graph: &Graph, schedule: &Schedule) -> &LsPositionModel {
        self.rekey(graph, schedule);
        if self.positions.is_some() {
            self.stats.positions_reused += 1;
        } else {
            self.stats.positions_built += 1;
        }
        self.positions
            .get_or_insert_with(|| LsPositionModel::build(graph.nodes.len(), schedule))
    }

    /// The φ edge table of `(graph, schedule)`, built on first use. Taken out
    /// of the scratch (so a copy can hold it while it takes buffers) and put
    /// back with [`Self::put_phi_edges`].
    pub(crate) fn take_phi_edges(&mut self, graph: &Graph, schedule: &Schedule) -> LsPhiEdges {
        self.rekey(graph, schedule);
        match self.phi_edges.take() {
            Some(edges) => {
                self.stats.phi_edges_reused += 1;
                edges
            }
            None => {
                self.stats.phi_edges_built += 1;
                LsPhiEdges::build(graph, schedule)
            }
        }
    }

    /// Hand a table from [`Self::take_phi_edges`] back for the next copy.
    pub(crate) fn put_phi_edges(&mut self, edges: LsPhiEdges) {
        self.phi_edges = Some(edges);
    }

    /// An all-zero buffer of `len` words: a pooled one when there is one.
    pub(crate) fn take_zeroed(&mut self, len: usize) -> Vec<u64> {
        match self.pool.pop() {
            Some(mut buf) => {
                self.stats.bufs_reused += 1;
                buf.clear();
                buf.resize(len, 0);
                buf
            }
            None => {
                self.stats.bufs_fresh += 1;
                vec![0u64; len]
            }
        }
    }

    /// Return a buffer to the pool (its contents are dropped on reuse).
    pub(crate) fn give(&mut self, buf: Vec<u64>) {
        // A handful of buffers covers the largest copy (six sets); keep the
        // pool bounded whatever a caller does.
        if self.pool.len() < 16 {
            self.pool.push(buf);
        }
    }
}

/// `vec![0u64; len]`, or a pooled buffer when a scratch is present.
#[inline]
pub(crate) fn ls_zeroed(scratch: &mut Option<&mut LivenessScratch>, len: usize) -> Vec<u64> {
    match scratch {
        Some(s) => s.take_zeroed(len),
        None => vec![0u64; len],
    }
}

/// Give `buf` back to the scratch, if any (dropped otherwise).
#[inline]
pub(crate) fn ls_give(scratch: &mut Option<&mut LivenessScratch>, buf: Vec<u64>) {
    if let Some(s) = scratch {
        s.give(buf);
    }
}

/// What one legacy liveness copy's fixed point did, for the `i70_l2_*` tests
/// and the bench (interpreter round i2 wave 70, lane L2): whether it ran (the
/// work budget or an empty value set can skip it), its verdict, the raw
/// per-block sets it ended with (cut-off runs included), and how many blocks
/// it visited. Filled only when a caller asks for it; the compile path passes
/// `None` and pays nothing.
// Read only by the tests and the bench; the compile path never asks for one.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LsLegacyRun {
    pub(crate) ran: bool,
    pub(crate) converged: bool,
    pub(crate) live_in: Vec<u64>,
    pub(crate) live_out: Vec<u64>,
    pub(crate) visits: usize,
}

#[cfg(test)]
impl LsLegacyRun {
    /// Everything but the visit count: what the two fixed points must agree on.
    pub(crate) fn answer(&self) -> (bool, bool, &[u64], &[u64]) {
        (self.ran, self.converged, &self.live_in, &self.live_out)
    }
}

/// The [`LivenessRules`] [`build_live_model`] runs under the current switches:
/// the token rule of `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` (implied by
/// `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES`), the φ edge rule of
/// `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE`, and the read list (interpreter round
/// i2 wave 66, lane L2: so `ir_lower` can ask whether the slot plan's rules
/// are the same question, [`one_liveness_pass_enabled`]).
pub(crate) fn live_model_rules() -> LivenessRules {
    LivenessRules {
        token_edges_not_uses: ls_token_edges_not_uses_enabled()
            || slots_token_edges_not_uses_enabled(),
        phi_edge_interfere: phi_edge_interfere_enabled(),
        collect_reads: true,
    }
}

/// `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES=1` (opt-in; JIT round 17 wave 3,
/// lane irlower; OD16-1 = OI-5, the token-edge half the proposal names as its
/// first step): [`build_live_model`] stops counting a memory-TOKEN edge as a
/// read of the value that produced it.
///
/// Two edges were interval length with no read behind it:
///
/// * a node's memory-token input (`ir::is_memory_token_slot`, slot 1 of every
///   memory op). When the previous writer is value-typed -- a `LoadStatic`,
///   a `Call` -- its interval ran on to the next memory op that names it as
///   its token, although that op's lowering reads only its value inputs (the
///   token is an ordering edge; `ir_lower` never `slot_of`s or registers one);
/// * a MEMORY φ's incoming edge. `ir_lower::gather_phi_copies` copies value φs
///   only (`phis_by_merge` filters memory φs out), so the source of a memory
///   φ's back edge -- again a value-typed token, or an inner loop's memory φ --
///   was kept live to the latch for a copy that is never emitted.
///
/// What stays: every VALUE use, the memory φ's own interval and pinning (its
/// `wants_loc` is unchanged, so the lockstep with `ir_lower`'s `wants_slot`,
/// `the_two_value_defining_enumerations_agree`, is untouched), and the slot
/// plan (`ir_lower::plan_slots`), which keeps its own, longer, home ranges.
/// Shorter intervals only: a register freed earlier, a value no longer
/// published live across a call it is not read after, a header live-in set
/// without the token (the OSR door's excuse becomes redundant for it).
///
/// Read once per model (not per edge), like [`phi_edge_interfere_enabled`].
fn ls_token_edges_not_uses_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES")
}

/// `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES=1` (opt-in; interpreter round
/// i2 wave 64, lane L2; `i63-L2-proposal-frame-home-ranges-skip-memory-token-reads`):
/// `ir_lower::plan_slots` applies the token-edge rule of
/// [`ls_token_edges_not_uses_enabled`] to the FRAME-HOME ranges too: a
/// memory-token input (`ir::is_memory_token_slot`) and a memory φ's incoming
/// edge stop holding the producer's home word. Pinning (φs, deopt-named
/// values, scalar-replacement fields) is untouched.
///
/// Two couplings, both deliberate:
///
/// * it IMPLIES the allocator's rule ([`build_live_model`] trims too). A home
///   shorter than the register interval the scan assigned would let a
///   split/reload transition at a token "use" read a word the colouring has
///   already handed to another value; with both models trimmed identically
///   `plan_register_residency`'s `ranges_agree` holds exactly as it does with
///   both off;
/// * it IMPLIES `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD` (`ir_lower`): a
///   shortened home some lowering arm still reads is exactly the bug that
///   guard refuses, so no A/B run of this switch can go without it.
pub fn slots_token_edges_not_uses_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES")
}

/// Loop nesting depth per block, from the **dominator relation** rather than
/// from block order.
///
/// A back edge is an edge `u -> v` whose target *dominates* its source; its
/// natural loop is `v` plus everything that reaches `u` without passing through
/// `v`. Both halves are order-independent by construction, which is the whole
/// point of R10.
///
/// # What this replaced, and why the old answer was wrong
///
/// This used to be inferred from block ORDER: a successor `s <= b` was declared
/// a back edge and every block in the index window `[s, b]` was declared one
/// level deeper. That is exact for a reverse postorder layout and only for one —
/// and the block order this model runs on is not an RPO, because
/// `ir_schedule::layout_blocks` reorders blocks by measured frequency before
/// anything here sees them. It only ever moved *preferences* (every consumer of
/// [`LiveModel::weight`] and [`LiveModel::loop_depth`] is a heuristic), never
/// soundness.
///
/// # One model, not two (round 11 wave 3)
///
/// This was a second, hand-maintained copy of `ir_schedule::loop_depths`, kept
/// because that one was private to a file this lane could not widen; the copy
/// lagged a round behind it once already (latches unioned per header, dead
/// latches skipped). One lane now owns both files, so this delegates to
/// `ir_schedule::loop_depths` and there is nothing left to drift.
///
/// # The fallback
///
/// `Schedule::dom` is sized for the final block numbering. If it is not — a
/// hand-built `Schedule` in a test, or a scheduler that grew the block list
/// after computing dominators — every depth is zero rather than guessed at.
/// Depth zero is the neutral element of the weight model: every position gets
/// weight 1 and the spill heuristic falls back to pure next-use distance, which
/// is classic linear scan. That is a worse heuristic, not a wrong one.
fn ls_loop_depths(schedule: &Schedule) -> Vec<u32> {
    if schedule.dom.len() != schedule.blocks.len() {
        return vec![0u32; schedule.blocks.len()];
    }
    crate::ir_schedule::loop_depths(&schedule.blocks, &schedule.dom)
}

/// Build the liveness model for one scheduled graph.
///
/// Total: never panics, never fails. See [`LiveModel`] for the fallback when
/// the analysis is skipped or does not converge.
///
/// Reads `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` once and delegates to
/// [`build_live_model_with_token_edges`] (interpreter round i2 wave 63, lane
/// L2: split out so [`token_trim_census`] can build both answers side by side
/// in one compile without a flag override).
pub fn build_live_model(graph: &Graph, schedule: &Schedule) -> LiveModel {
    // Interpreter round i2 wave 64 (lane L2): the slot-plan switch implies
    // the allocator's (see `slots_token_edges_not_uses_enabled`).
    build_live_model_with_token_edges(
        graph,
        schedule,
        ls_token_edges_not_uses_enabled() || slots_token_edges_not_uses_enabled(),
    )
}

/// [`build_live_model`] with the token-edge decision passed in rather than
/// read from `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES`. `token_edges_not_uses`
/// true: a memory-token input (`ir::is_memory_token_slot`) and a memory φ's
/// incoming edge are not uses (see [`ls_token_edges_not_uses_enabled`]);
/// false: every input is a use, the default model.
///
/// Reads `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE` once, and (interpreter round i2
/// wave 65, lane L2) `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE`, which routes the
/// analysis through [`liveness_core`] instead of the legacy copy below. Both
/// give the same model.
pub fn build_live_model_with_token_edges(
    graph: &Graph,
    schedule: &Schedule,
    token_edges_not_uses: bool,
) -> LiveModel {
    let rules = LivenessRules {
        token_edges_not_uses,
        phi_edge_interfere: phi_edge_interfere_enabled(),
        collect_reads: true,
    };
    if shared_liveness_core_enabled() {
        build_live_model_from_core(graph, schedule, rules)
    } else {
        build_live_model_legacy(graph, schedule, rules)
    }
}

/// Which values need a location: every φ, plus every scheduled node whose op
/// defines a value ([`ir_op_defines_value`]). Exactly `ir_lower`'s
/// `wants_slot`.
pub(crate) fn live_model_wants_loc(graph: &Graph, schedule: &Schedule) -> Vec<bool> {
    let n = graph.nodes.len();
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
    wants_loc
}

/// The model's pinned classes: φs and every value a deopt frame names. See
/// the step-4 comment in [`build_live_model_legacy`].
fn live_model_pins(graph: &Graph, wants_loc: &[bool]) -> Vec<bool> {
    let n = graph.nodes.len();
    let mut pinned = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if wants_loc.get(id).copied().unwrap_or(false) && matches!(node.op, Op::Phi) {
            pinned[id] = true;
        }
    }
    for sp in &graph.safepoints {
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            if v != NO_NODE {
                if let Some(slot) = pinned.get_mut(v as usize) {
                    *slot = true;
                }
            }
        }
    }
    pinned
}

/// The legacy (pre-wave-65) spelling of [`build_live_model_with_token_edges`]:
/// its own copy of the position model and the liveness fixed point. Kept,
/// unchanged except that the φ edge rule arrives in `rules`, until
/// `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE` has been confirmed byte-identical on
/// a host; [`build_live_model_from_core`] is the replacement.
fn build_live_model_legacy(graph: &Graph, schedule: &Schedule, rules: LivenessRules) -> LiveModel {
    build_live_model_legacy_with(
        graph,
        schedule,
        rules,
        LIVE_MODEL_MAX_ITERATIONS,
        legacy_liveness_fixed_point(schedule.blocks.len()),
        None,
    )
}

/// [`build_live_model_legacy`] with the iteration cap and the fixed point
/// explicit (interpreter round i2 wave 70, lane L2), and the fixed point's
/// raw result handed back in `trace` when asked for. The round-robin arm is
/// the legacy loop, unchanged.
pub(crate) fn build_live_model_legacy_with(
    graph: &Graph,
    schedule: &Schedule,
    rules: LivenessRules,
    max_iterations: usize,
    fixed_point: LivenessFixedPoint,
    trace: Option<&mut LsLegacyRun>,
) -> LiveModel {
    build_live_model_legacy_impl(
        graph,
        schedule,
        rules,
        max_iterations,
        fixed_point,
        trace,
        None,
    )
}

/// [`build_live_model`] through a per-compile [`LivenessScratch`] (JIT round
/// 19 wave 10, lane lv10, [`liveness_shared_scratch_enabled`]): the same
/// switches and the same model; the legacy copy takes its position model, φ
/// edge table and bit sets from `scratch`. The shared-core path
/// (`CRATONVM_JIT_IR_SHARED_LIVENESS_CORE`) does not use it.
pub(crate) fn build_live_model_scratch(
    graph: &Graph,
    schedule: &Schedule,
    scratch: &mut LivenessScratch,
) -> LiveModel {
    let rules = LivenessRules {
        token_edges_not_uses: ls_token_edges_not_uses_enabled()
            || slots_token_edges_not_uses_enabled(),
        phi_edge_interfere: phi_edge_interfere_enabled(),
        collect_reads: true,
    };
    if shared_liveness_core_enabled() {
        build_live_model_from_core(graph, schedule, rules)
    } else {
        build_live_model_legacy_impl(
            graph,
            schedule,
            rules,
            LIVE_MODEL_MAX_ITERATIONS,
            legacy_liveness_fixed_point(schedule.blocks.len()),
            None,
            Some(scratch),
        )
    }
}

/// The legacy copy's own position walk (step 2 of [`build_live_model_legacy`],
/// unchanged; split out in JIT round 19 wave 10, lane lv10, so the scratch
/// path can take [`LivenessScratch::positions`] instead).
fn live_model_positions_legacy(
    n: usize,
    schedule: &Schedule,
) -> (Vec<Option<usize>>, Vec<(usize, usize)>, Vec<usize>, usize) {
    let nb = schedule.blocks.len();
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
    (pos_of, span, block_of_pos, seq)
}

/// [`build_live_model_legacy_with`] with an optional [`LivenessScratch`]
/// (JIT round 19 wave 10, lane lv10). `None` is the legacy copy exactly.
pub(crate) fn build_live_model_legacy_impl(
    graph: &Graph,
    schedule: &Schedule,
    rules: LivenessRules,
    max_iterations: usize,
    fixed_point: LivenessFixedPoint,
    trace: Option<&mut LsLegacyRun>,
    mut scratch: Option<&mut LivenessScratch>,
) -> LiveModel {
    let token_edges_not_uses = rules.token_edges_not_uses;
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();

    // ── 1. Which values need a location ──────────────────────────────
    let wants_loc = live_model_wants_loc(graph, schedule);

    // ── 2. Linear positions and block spans ──────────────────────────
    // JIT round 19 wave 10 (lane lv10): from the per-compile scratch when
    // there is one (the same walk, `LsPositionModel::build`).
    let (pos_of, span, block_of_pos, total_positions) = match scratch.as_deref_mut() {
        Some(s) => {
            let m = s.positions(graph, schedule);
            (
                m.pos_of.clone(),
                m.span.clone(),
                m.block_of_pos.clone(),
                m.total_positions,
            )
        }
        None => live_model_positions_legacy(n, schedule),
    };

    // ── 3. Loop depth and per-position frequency weight ──────────────
    let loop_depth = ls_loop_depths(schedule);
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
    let pinned = live_model_pins(graph, &wants_loc);

    // ── 5. Uses, definitions and the backward fixed point ────────────
    let mut lo = vec![usize::MAX; n];
    let mut hi = vec![0usize; n];
    let mut uses: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut weight = vec![0u64; n];
    let mut converged = n.saturating_mul(nb) <= LIVE_MODEL_WORK_BUDGET;
    // Filled only once the fixed point has settled; see the field's doc.
    let mut block_live_in: Vec<Vec<u64>> = Vec::new();

    if converged {
        let words = n.div_ceil(64).max(1);
        // JIT round 19 wave 10 (lane lv10): pooled when a scratch is present.
        let mut def_bits = ls_zeroed(&mut scratch, words * nb);
        let mut use_bits = ls_zeroed(&mut scratch, words * nb);
        let mut phi_out_bits = ls_zeroed(&mut scratch, words * nb);

        // OD16-1 token-edge half (opt-in): `token_edges_not_uses` is the
        // caller's, read once per model by `build_live_model`.
        let mut local_def = ls_zeroed(&mut scratch, words);
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
                    for (slot, &inp) in node.inputs.iter().enumerate() {
                        let ii = inp as usize;
                        if inp == NO_NODE || ii >= n || !wants_loc[ii] {
                            continue;
                        }
                        // JIT round 17 wave 3 (lane irlower, OD16-1): an
                        // ordering edge, not a read. See
                        // `ls_token_edges_not_uses_enabled`.
                        if token_edges_not_uses && crate::ir::is_memory_token_slot(node, slot) {
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
        // Read ONCE per model, not once per phi per edge. It is an uncached
        // `runtime_var` read (a declared-name lookup plus an `OsString` clone),
        // and this loop runs `phis x predecessors` times on every IR compile —
        // the shape `flags::runtime_var_os`' read census exists to catch.
        // (Since wave 65 the read is the caller's, in `rules`.)
        let phi_edge_interfere = rules.phi_edge_interfere;
        // JIT round 19 wave 10 (lane lv10): one incoming edge's reads, as a
        // closure so the legacy walk below and the scratch's prebuilt table
        // (`LsPhiEdges`, the same edges in the same order) share it.
        let mut on_edge = |pred: usize, k: usize, phis: &[NodeId]| {
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
                // JIT round 17 wave 3 (lane irlower, OD16-1): a memory φ
                // gets no edge copy (`ir_lower::gather_phi_copies`), so
                // its source is not read here. The φ's own interval below
                // is left exactly as it was.
                let source_unread = token_edges_not_uses
                    && graph
                        .nodes
                        .get(pid as usize)
                        .is_some_and(|p| p.ty == IrType::Memory);
                if !source_unread {
                    lo[vi] = lo[vi].min(at);
                    hi[vi] = hi[vi].max(at);
                    uses[vi].push(at);
                    weight[vi] = weight[vi].saturating_add(pos_weight(at));
                    ls_bit_set(&mut phi_out_bits[pred * words..(pred + 1) * words], vi);
                }

                // DEFAULT ON (`CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=0` is the
                // kill switch -- see `phi_edge_interfere_enabled`, which is
                // `runtime_flag_default_on`): the phi is DEFINED at this
                // same position, so extend its interval to cover it and let
                // the allocator see that it overlaps every source read on
                // this edge.
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
                // What moves, and what deliberately does not.
                //
                // BOTH ENDS of the interval move: `lo` down to `at` and
                // `hi` up to it. For a forward edge `at` precedes the
                // phi's def and only `lo` changes; for a BACK edge `at` is
                // later than the def, so `hi` is what moves and the phi's
                // range is extended forward to the latch. Both directions
                // are the same statement -- the phi's location is spoken
                // for at the position its incoming copies run -- and both
                // are over-approximations, which is the safe direction for
                // an interference question. The loop-carried case is the
                // one that matters: that is exactly where a phi and the
                // source feeding it back are simultaneously live.
                //
                // (This paragraph used to say "only `lo` moves", which had
                // stopped describing the code. Corrected 2026-09; the flag
                // is default-ON, so the wrong reasoning was the live one.)
                //
                // What does NOT move: no USE is pushed -- the phi is not
                // read here, and a spurious use would distort the spill
                // heuristics that weight by use count -- and `phi_out_bits`
                // is NOT set for the phi, because that would make it
                // live-OUT of a block that does not define it, which the
                // backward dataflow below would propagate live-IN through
                // every predecessor and turn a bounded extension into a
                // whole-CFG one.
                if phi_edge_interfere {
                    let pi = pid as usize;
                    if pi < n && wants_loc[pi] {
                        lo[pi] = lo[pi].min(at);
                        hi[pi] = hi[pi].max(at);
                    }
                }
            }
        };
        match scratch.as_deref_mut() {
            Some(s) => {
                let edges = s.take_phi_edges(graph, schedule);
                for e in &edges.edges {
                    on_edge(e.pred, e.k, edges.phis_of(e));
                }
                s.put_phi_edges(edges);
            }
            None => {
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
                        on_edge(pred, k, phis);
                    }
                }
            }
        }

        let mut live_in = ls_zeroed(&mut scratch, words * nb);
        let mut live_out = ls_zeroed(&mut scratch, words * nb);
        let mut visits = 0usize;
        let settled = match fixed_point {
            LivenessFixedPoint::RoundRobin => {
                let mut scratch = vec![0u64; words];
                let mut settled = false;
                for _ in 0..max_iterations {
                    let mut changed = false;
                    visits += nb;
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
                settled
            }
            // Interpreter round i2 wave 70, lane L2: the same answer, pass for
            // pass, visiting only dirty blocks (wave 71: the predecessor
            // lists per `fixed_point`). JIT round 19 wave 10 (lane lv10): the
            // round robin through the shared driver too.
            LivenessFixedPoint::Worklist
            | LivenessFixedPoint::WorklistSchedulePreds
            | LivenessFixedPoint::WorklistPostorder { .. }
            | LivenessFixedPoint::RoundRobinDriver => ls_fixed_point_on_schedule(
                schedule.blocks.as_slice(),
                fixed_point,
                LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out_bits,
                    uses: &use_bits,
                    defs: &def_bits,
                },
                &mut live_in,
                &mut live_out,
                max_iterations,
                &mut visits,
            ),
        };
        converged = settled;
        if let Some(trace) = trace {
            *trace = LsLegacyRun {
                ran: true,
                converged: settled,
                live_in: live_in.clone(),
                live_out: live_out.clone(),
                visits,
            };
        }

        // Widen each range over every block it is live in. This is where loops
        // are handled: a value used inside a loop is live-out of every block on
        // the back edge's path, so its range covers the whole loop without the
        // model ever knowing what a loop is.
        if converged {
            block_live_in = (0..nb)
                .map(|b| live_in[b * words..(b + 1) * words].to_vec())
                .collect();
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
        // JIT round 19 wave 10 (lane lv10): back to the pool, if any.
        for buf in [
            def_bits,
            use_bits,
            phi_out_bits,
            local_def,
            live_in,
            live_out,
        ] {
            ls_give(&mut scratch, buf);
        }
    }

    finish_live_model(
        graph,
        schedule,
        LiveModelHead {
            pos_of,
            span,
            total_positions,
            block_of_pos,
            wants_loc,
            pinned,
            lo,
            hi,
            uses,
            weight,
            loop_depth,
            converged,
            block_live_in,
        },
    )
}

/// What [`finish_live_model`] needs from the analysis half of a
/// [`LiveModel`] build: the legacy copy ([`build_live_model_legacy`]) and the
/// shared core ([`build_live_model_from_core`]) both produce one.
struct LiveModelHead {
    pos_of: Vec<Option<usize>>,
    span: Vec<(usize, usize)>,
    total_positions: usize,
    block_of_pos: Vec<usize>,
    wants_loc: Vec<bool>,
    pinned: Vec<bool>,
    /// Per value, `usize::MAX` when nothing placed it.
    lo: Vec<usize>,
    hi: Vec<usize>,
    /// Unsorted, possibly repeated read positions.
    uses: Vec<Vec<usize>>,
    weight: Vec<u64>,
    loop_depth: Vec<u32>,
    converged: bool,
    block_live_in: Vec<Vec<u64>>,
}

/// [`build_live_model_from_core`]: the model over [`liveness_core`]'s answer.
/// Uses and weights are a layer on top of the core's read list; ranges,
/// positions and the live-in sets are the core's.
fn build_live_model_from_core(
    graph: &Graph,
    schedule: &Schedule,
    rules: LivenessRules,
) -> LiveModel {
    let wants_loc = live_model_wants_loc(graph, schedule);
    let core = liveness_core(
        graph,
        schedule,
        &wants_loc,
        LivenessRules {
            collect_reads: true,
            ..rules
        },
        LIVE_MODEL_WORK_BUDGET,
        LIVE_MODEL_MAX_ITERATIONS,
    );
    live_model_from_core(graph, schedule, wants_loc, core)
}

/// The live model over a [`liveness_core`] answer the caller already holds
/// (interpreter round i2 wave 66, lane L2; split out of
/// [`build_live_model_from_core`] so the lowering entry can run the core once
/// per compile, [`one_liveness_pass_enabled`]).
///
/// `wants_loc` must be [`live_model_wants_loc`]'s answer and `core` must have
/// been computed over it with [`LivenessRules::collect_reads`] on and the
/// live model's work budget and iteration cap; the uses and weights are built
/// from `core.reads`, which is empty otherwise.
pub(crate) fn live_model_from_core(
    graph: &Graph,
    schedule: &Schedule,
    wants_loc: Vec<bool>,
    core: LivenessCore,
) -> LiveModel {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    let block_of_pos = block_of_pos_from_spans(&core.span);
    let loop_depth = ls_loop_depths(schedule);
    let pos_weight = |p: usize| -> u64 {
        let b = block_of_pos.get(p).copied().unwrap_or(0);
        let depth = loop_depth.get(b).copied().unwrap_or(0);
        LOOP_WEIGHT_PER_DEPTH
            .checked_pow(depth)
            .unwrap_or(MAX_LOOP_DEPTH_WEIGHT)
            .min(MAX_LOOP_DEPTH_WEIGHT) as u64
    };
    let pinned = live_model_pins(graph, &wants_loc);
    let mut uses: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut weight = vec![0u64; n];
    for &(v, at) in &core.reads {
        let vi = v as usize;
        if let (Some(list), Some(w)) = (uses.get_mut(vi), weight.get_mut(vi)) {
            list.push(at);
            *w = w.saturating_add(pos_weight(at));
        }
    }
    let words = core.words;
    let block_live_in: Vec<Vec<u64>> = if core.converged && core.live_in.len() == words * nb {
        (0..nb)
            .map(|b| core.live_in[b * words..(b + 1) * words].to_vec())
            .collect()
    } else {
        Vec::new()
    };
    let LivenessCore {
        pos_of,
        span,
        total_positions,
        lo,
        hi,
        converged,
        ..
    } = core;
    finish_live_model(
        graph,
        schedule,
        LiveModelHead {
            pos_of,
            span,
            total_positions,
            block_of_pos,
            wants_loc,
            pinned,
            lo,
            hi,
            uses,
            weight,
            loop_depth,
            converged,
            block_live_in,
        },
    )
}

/// `block_of_pos[p]` from the block spans: block `b` owns `span[b].0 ..=
/// span[b].1` (its nodes, its terminator and its edge position), and the
/// spans are contiguous in block order.
fn block_of_pos_from_spans(span: &[(usize, usize)]) -> Vec<usize> {
    let mut out = Vec::with_capacity(span.last().map_or(0, |&(_, e)| e + 1));
    for (b, &(start, end)) in span.iter().enumerate() {
        for _ in start..=end {
            out.push(b);
        }
    }
    out
}

/// The rules [`liveness_core`] applies on top of "every input is a read".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LivenessRules {
    /// A memory-token input (`ir::is_memory_token_slot`) and a memory φ's
    /// incoming edge are not reads (`CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES`,
    /// `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES`).
    pub(crate) token_edges_not_uses: bool,
    /// A φ's interval covers each incoming edge position whose source is a
    /// located value (`CRATONVM_JIT_IR_PHI_EDGE_INTERFERE`; see the comment at
    /// its use in [`build_live_model_legacy`]).
    pub(crate) phi_edge_interfere: bool,
    /// Record every read in [`LivenessCore::reads`] (the live model's uses
    /// and weights). Off for the slot plan, which has no use for them.
    pub(crate) collect_reads: bool,
}

/// [`liveness_core`]'s answer: the position model and the value intervals
/// both `ir_lower::plan_slots` and [`build_live_model`] are built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LivenessCore {
    /// The linear emission position of each node, `None` when unscheduled.
    pub(crate) pos_of: Vec<Option<usize>>,
    /// `(first position, outgoing-edge position)` per block.
    pub(crate) span: Vec<(usize, usize)>,
    /// One past the last position.
    pub(crate) total_positions: usize,
    /// Per value: the lowest position it is placed, read, or live at;
    /// `usize::MAX` when nothing placed it. Meaningful only when `converged`.
    pub(crate) lo: Vec<usize>,
    /// Per value: the highest such position (0 when nothing placed it).
    pub(crate) hi: Vec<usize>,
    /// Per value: `lo` was `usize::MAX` until the φ edge rule set it, i.e. a
    /// φ no code reads, that no block defines and no block has live, whose
    /// interval is its incoming edges alone. All false without the rule.
    /// `ir_lower`'s `phi_code_unused` asks the pre-rule question.
    pub(crate) set_only_by_phi_rule: Vec<bool>,
    /// The fixed point ran (the graph was within the work budget) and settled.
    pub(crate) converged: bool,
    /// Words per block bit set: `max(1, ceil(nodes / 64))`.
    pub(crate) words: usize,
    /// `words × blocks` live-in bits, flattened; empty when the work budget
    /// skipped the analysis. Valid only when `converged`.
    pub(crate) live_in: Vec<u64>,
    /// `words × blocks` live-out bits, as `live_in`.
    pub(crate) live_out: Vec<u64>,
    /// `(value, position)` for every read, in visiting order, repeats kept;
    /// only with [`LivenessRules::collect_reads`].
    pub(crate) reads: Vec<(NodeId, usize)>,
}

impl LivenessCore {
    /// The part of this answer `ir_lower::plan_slots` reads: positions, spans,
    /// ranges, the φ-rule bit and the convergence flag, copied; the bit sets
    /// and the read list left empty (interpreter round i2 wave 66, lane L2,
    /// [`one_liveness_pass_enabled`]). `O(nodes + blocks)`; the fixed point is
    /// not rerun. The slot plan consumes its core by value, and the live model
    /// keeps the original.
    pub(crate) fn slot_view(&self) -> LivenessCore {
        LivenessCore {
            pos_of: self.pos_of.clone(),
            span: self.span.clone(),
            total_positions: self.total_positions,
            lo: self.lo.clone(),
            hi: self.hi.clone(),
            set_only_by_phi_rule: self.set_only_by_phi_rule.clone(),
            converged: self.converged,
            words: self.words,
            live_in: Vec::new(),
            live_out: Vec::new(),
            reads: Vec::new(),
        }
    }
}

/// One liveness computation for one scheduled graph, over the value set
/// `wants` (interpreter round i2 wave 65, lane L2,
/// `i64-L2-proposal-one-liveness-core-for-plan-slots-and-the-live-model`).
///
/// The position numbering (each scheduled node, then the terminator, then one
/// outgoing-edge position per block), the upward-exposed use pass (a φ's
/// value inputs are read at its predecessors' edge positions, not at the φ),
/// the backward fixed point in reverse block order, and the per-block range
/// widening -- the analysis `ir_lower::plan_slots` and
/// [`build_live_model_legacy`] each spelled out by hand. `rules` holds the
/// two places they may differ.
///
/// Total: never panics. A graph past `work_budget` (`nodes × blocks`) is not
/// analysed; a fixed point that has not settled after `max_iterations` passes
/// is reported unconverged. Either way the caller treats every range as the
/// whole method.
///
/// Interpreter round i2 wave 67, lane L2: with
/// [`liveness_dense_values_enabled`] (or [`liveness_budget_by_values_enabled`])
/// the bit sets are indexed by a dense value numbering
/// ([`liveness_core_dense`]); the answer is the same.
pub(crate) fn liveness_core(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
) -> LivenessCore {
    let budget_by_values = liveness_budget_by_values_enabled();
    let dense = budget_by_values || liveness_dense_values_enabled();
    if liveness_worklist_enabled() {
        // Interpreter round i2 wave 69, lane L2: same answer, fewer visits.
        // Wave 71: below the block-count gate the round robin below runs.
        let fixed_point = worklist_fixed_point_for(schedule.blocks.len());
        if fixed_point != LivenessFixedPoint::RoundRobin {
            return liveness_core_with(
                graph,
                schedule,
                wants,
                rules,
                work_budget,
                max_iterations,
                dense,
                budget_by_values,
                fixed_point,
            )
            .0;
        }
    }
    if dense {
        liveness_core_dense(
            graph,
            schedule,
            wants,
            rules,
            work_budget,
            max_iterations,
            budget_by_values,
        )
    } else {
        liveness_core_sparse(graph, schedule, wants, rules, work_budget, max_iterations)
    }
}

/// How [`liveness_core`] runs its backward fixed point (interpreter round i2
/// wave 69, lane L2). Both give the same answer, pass for pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LivenessFixedPoint {
    /// Every block, every pass ([`ls_fixed_point_round_robin`]).
    #[default]
    RoundRobin,
    /// Only the blocks a successor's live-in change made dirty, in the same
    /// pass order ([`ls_fixed_point_worklist`]).
    Worklist,
    /// [`Self::Worklist`] reading a schedule's own `predecessors` lists
    /// instead of building a predecessor index per call (interpreter round
    /// i3 wave 71, lane L2, [`liveness_schedule_preds_enabled`]). Over a bare
    /// adjacency list (the tests and the bench) the index is built once by
    /// the caller ([`LsPredIndex`]). Same answer, pass for pass.
    WorklistSchedulePreds,
    /// The worklist with its passes in CFG postorder ([`ls_postorder`],
    /// [`ls_fixed_point_worklist_ordered`]; interpreter round i3 wave 71,
    /// lane L2, [`liveness_postorder_enabled`]), the predecessor lists the
    /// schedule's own when `schedule_preds`. The same converged answer; the
    /// pass count, and so a cut-off run, can differ.
    WorklistPostorder { schedule_preds: bool },
    /// [`Self::RoundRobin`] through the shared driver
    /// ([`ls_fixed_point_round_robin`]) in the legacy copies, which otherwise
    /// run their own hand-written whole-pass loop for `RoundRobin` (JIT round
    /// 19 wave 10, lane lv10, [`liveness_one_driver_enabled`]). The same
    /// answer and visit count, pass for pass. Everywhere else it is
    /// `RoundRobin`.
    RoundRobinDriver,
}

/// [`liveness_core`] with every choice explicit, whatever the switches say:
/// the dense numbering (`dense`), the value budget (`budget_by_values`,
/// implies nothing here: pass `dense` too, as [`liveness_core`] does) and the
/// fixed point. Also returns the number of block visits the fixed point made
/// (0 when the work budget skipped it), for the tests and the bench.
#[allow(clippy::too_many_arguments)]
pub(crate) fn liveness_core_with(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
    dense: bool,
    budget_by_values: bool,
    fixed_point: LivenessFixedPoint,
) -> (LivenessCore, usize) {
    let mut visits = 0usize;
    let core = if dense {
        liveness_core_body::<true>(
            graph,
            schedule,
            wants,
            rules,
            work_budget,
            max_iterations,
            budget_by_values,
            fixed_point,
            &mut visits,
        )
    } else {
        liveness_core_body::<false>(
            graph,
            schedule,
            wants,
            rules,
            work_budget,
            max_iterations,
            budget_by_values,
            fixed_point,
            &mut visits,
        )
    };
    (core, visits)
}

/// [`liveness_core`] with its bit sets indexed by node id, `ceil(nodes / 64)`
/// words per block: the pre-wave-67 core, whatever the switches say.
pub(crate) fn liveness_core_sparse(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
) -> LivenessCore {
    liveness_core_body::<false>(
        graph,
        schedule,
        wants,
        rules,
        work_budget,
        max_iterations,
        false,
        LivenessFixedPoint::RoundRobin,
        &mut 0usize,
    )
}

/// [`liveness_core`] with its bit sets indexed by a dense numbering of the
/// wanted values, `ceil(values / 64)` words per block, whatever the switches
/// say (interpreter round i2 wave 67, lane L2). `budget_by_values` makes the
/// work budget count `values × blocks` (a behaviour change; false gives the
/// sparse core's answer exactly).
pub(crate) fn liveness_core_dense(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
    budget_by_values: bool,
) -> LivenessCore {
    liveness_core_body::<true>(
        graph,
        schedule,
        wants,
        rules,
        work_budget,
        max_iterations,
        budget_by_values,
        LivenessFixedPoint::RoundRobin,
        &mut 0usize,
    )
}

/// Reads a block's successor list: the schedule's blocks, or (in the tests
/// and the bench) a bare adjacency list (interpreter round i2 wave 69, lane
/// L2). An index `>= nb` is ignored by both fixed points.
pub(crate) trait LsSuccessors {
    fn ls_successors(&self, b: usize) -> &[usize];
}

impl LsSuccessors for [crate::ir_schedule::Block] {
    #[inline]
    fn ls_successors(&self, b: usize) -> &[usize] {
        &self[b].successors
    }
}

impl LsSuccessors for [Vec<usize>] {
    #[inline]
    fn ls_successors(&self, b: usize) -> &[usize] {
        &self[b]
    }
}

/// The per-block sets the backward fixed point runs over, `words` per block,
/// `nb` blocks, flattened: what a block's outgoing edge reads (`phi_out`),
/// its upward-exposed reads (`uses`) and its definitions (`defs`).
#[derive(Clone, Copy)]
pub(crate) struct LsFixedPointSets<'a> {
    pub(crate) nb: usize,
    pub(crate) words: usize,
    pub(crate) phi_out: &'a [u64],
    pub(crate) uses: &'a [u64],
    pub(crate) defs: &'a [u64],
}

/// One block's transfer: `lout |= phi_out | li[succ]…`, `li |= uses | (lout &
/// !defs)`. Returns `(lout changed, li changed)`. `scratch` is `words` long.
#[inline]
fn ls_fixed_point_visit<S: LsSuccessors + ?Sized>(
    succ: &S,
    sets: LsFixedPointSets<'_>,
    b: usize,
    li: &mut [u64],
    lout: &mut [u64],
    scratch: &mut [u64],
) -> (bool, bool) {
    let (nb, words) = (sets.nb, sets.words);
    let base = b * words;
    scratch.copy_from_slice(&sets.phi_out[base..base + words]);
    for &s in succ.ls_successors(b) {
        if s >= nb {
            continue;
        }
        for w in 0..words {
            scratch[w] |= li[s * words + w];
        }
    }
    let (mut out_changed, mut in_changed) = (false, false);
    for w in 0..words {
        let merged_out = lout[base + w] | scratch[w];
        if merged_out != lout[base + w] {
            lout[base + w] = merged_out;
            out_changed = true;
        }
        let entering = sets.uses[base + w] | (merged_out & !sets.defs[base + w]);
        let merged_in = li[base + w] | entering;
        if merged_in != li[base + w] {
            li[base + w] = merged_in;
            in_changed = true;
        }
    }
    (out_changed, in_changed)
}

/// The backward fixed point as whole passes: each pass visits every block in
/// reverse index order; it stops after the first pass that changes nothing
/// (`true`), or after `max_iterations` passes (`false`, unconverged). `li` and
/// `lout` start as given (all zero in the core). Counts block visits.
#[inline]
pub(crate) fn ls_fixed_point_round_robin<S: LsSuccessors + ?Sized>(
    succ: &S,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool {
    let mut scratch = vec![0u64; sets.words];
    for _ in 0..max_iterations {
        let mut changed = false;
        for b in (0..sets.nb).rev() {
            *visits += 1;
            let (out_changed, in_changed) =
                ls_fixed_point_visit(succ, sets, b, li, lout, &mut scratch);
            changed |= out_changed | in_changed;
        }
        if !changed {
            return true;
        }
    }
    false
}

/// The highest set bit strictly below `limit`, if any.
#[inline]
fn ls_highest_set_below(bits: &[u64], limit: usize) -> Option<usize> {
    if limit == 0 {
        return None;
    }
    let last = limit - 1;
    let mut wi = last / 64;
    let keep = last % 64 + 1;
    let mask = if keep >= 64 {
        u64::MAX
    } else {
        (1u64 << keep) - 1
    };
    let mut word = bits.get(wi).copied().unwrap_or(0) & mask;
    loop {
        if word != 0 {
            return Some(wi * 64 + 63 - word.leading_zeros() as usize);
        }
        if wi == 0 {
            return None;
        }
        wi -= 1;
        word = bits.get(wi).copied().unwrap_or(0);
    }
}

/// [`ls_fixed_point_round_robin`] that skips clean blocks (interpreter round
/// i2 wave 69, lane L2, [`liveness_worklist_enabled`]).
///
/// A block is dirty until visited; visiting it clears it, and a change to its
/// `li` dirties each predecessor (from the successor lists, inverted here,
/// self loops and duplicate edges included). Each pass still runs from the
/// highest block index down, visiting only dirty blocks: a predecessor below
/// the changed block is visited later in the same pass, one at or above it in
/// the next, which is exactly when the round-robin pass would first see the
/// change. A clean block's successors' `li` are unchanged since its last
/// visit, so recomputing it would change nothing. Hence the state after each
/// pass is the round-robin state after that pass, and so are the pass count,
/// the convergence verdict (a pass with nothing dirty changes nothing) and
/// the sets of a cut-off run.
pub(crate) fn ls_fixed_point_worklist<S: LsSuccessors + ?Sized>(
    succ: &S,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool {
    let preds = LsPredIndex::build(succ, sets.nb);
    ls_fixed_point_worklist_preds(succ, &preds, sets, li, lout, max_iterations, visits)
}

/// Reads a block's predecessor list for [`ls_fixed_point_worklist_preds`]
/// (interpreter round i3 wave 71, lane L2). Entries `>= nb` are ignored;
/// duplicates and extra entries cost visits, never the answer; a MISSING
/// predecessor would (a block would not be revisited when a successor's
/// live-in grows), so an implementation must list at least every `p` with
/// `b` among `p`'s in-range successors.
pub(crate) trait LsPredecessors {
    fn ls_predecessors(&self, b: usize) -> &[usize];
}

/// The predecessor index inverted from successor lists, CSR (what
/// [`ls_fixed_point_worklist`] builds on every call; interpreter round i3
/// wave 71, lane L2 split it out so a caller can build it once).
pub(crate) struct LsPredIndex {
    start: Vec<usize>,
    preds: Vec<usize>,
}

impl LsPredIndex {
    /// Invert `succ`'s first `nb` lists, ignoring successors `>= nb`; self
    /// loops and duplicate edges kept.
    pub(crate) fn build<S: LsSuccessors + ?Sized>(succ: &S, nb: usize) -> Self {
        let mut start = vec![0usize; nb + 1];
        for b in 0..nb {
            for &s in succ.ls_successors(b) {
                if s < nb {
                    start[s + 1] += 1;
                }
            }
        }
        for b in 0..nb {
            start[b + 1] += start[b];
        }
        let mut fill = start.clone();
        let mut preds = vec![0usize; start[nb]];
        for b in 0..nb {
            for &s in succ.ls_successors(b) {
                if s < nb {
                    preds[fill[s]] = b;
                    fill[s] += 1;
                }
            }
        }
        LsPredIndex { start, preds }
    }
}

impl LsPredecessors for LsPredIndex {
    #[inline]
    fn ls_predecessors(&self, b: usize) -> &[usize] {
        match (self.start.get(b), self.start.get(b + 1)) {
            (Some(&lo), Some(&hi)) => &self.preds[lo..hi],
            _ => &[],
        }
    }
}

/// A schedule's own predecessor lists (`Block::predecessors`), which
/// `ir_schedule` pushes beside every successor edge and remaps with them in
/// the layout pass (interpreter round i3 wave 71, lane L2,
/// [`liveness_schedule_preds_enabled`]).
pub(crate) struct LsSchedulePreds<'a>(pub(crate) &'a [crate::ir_schedule::Block]);

impl LsPredecessors for LsSchedulePreds<'_> {
    #[inline]
    fn ls_predecessors(&self, b: usize) -> &[usize] {
        match self.0.get(b) {
            Some(block) => block.predecessors.as_slice(),
            None => &[],
        }
    }
}

/// The fixed point `fixed_point` names over a schedule's blocks: the round
/// robin, the worklist with a per-call predecessor index, or the worklist
/// over the blocks' own predecessor lists (interpreter round i3 wave 71,
/// lane L2). The one entry the worklist arms of every liveness copy call.
pub(crate) fn ls_fixed_point_on_schedule(
    blocks: &[crate::ir_schedule::Block],
    fixed_point: LivenessFixedPoint,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool {
    match fixed_point {
        LivenessFixedPoint::RoundRobin | LivenessFixedPoint::RoundRobinDriver => {
            ls_fixed_point_round_robin(blocks, sets, li, lout, max_iterations, visits)
        }
        LivenessFixedPoint::Worklist => {
            ls_fixed_point_worklist(blocks, sets, li, lout, max_iterations, visits)
        }
        LivenessFixedPoint::WorklistSchedulePreds => ls_fixed_point_worklist_preds(
            blocks,
            &LsSchedulePreds(blocks),
            sets,
            li,
            lout,
            max_iterations,
            visits,
        ),
        LivenessFixedPoint::WorklistPostorder { schedule_preds } => {
            let (order, rank) = ls_postorder(blocks, sets.nb);
            if schedule_preds {
                ls_fixed_point_worklist_ordered(
                    blocks,
                    &LsSchedulePreds(blocks),
                    &order,
                    &rank,
                    sets,
                    li,
                    lout,
                    max_iterations,
                    visits,
                )
            } else {
                let preds = LsPredIndex::build(blocks, sets.nb);
                ls_fixed_point_worklist_ordered(
                    blocks,
                    &preds,
                    &order,
                    &rank,
                    sets,
                    li,
                    lout,
                    max_iterations,
                    visits,
                )
            }
        }
    }
}

/// A pass order for the backward fixed point (interpreter round i3 wave 71,
/// lane L2, [`liveness_postorder_enabled`]): `order[k]` is the block a pass
/// visits at step `nb - 1 - k` (the drivers walk `k` downward), `rank` its
/// inverse. The visiting sequence is a DFS postorder over the in-range
/// successors, from block 0 first, then from each block not yet reached in
/// index order (unreachable code, which no reachable block depends on): every
/// block after its DFS successors, so only retreating (loop) edges carry a
/// change into a later pass.
pub(crate) fn ls_postorder<S: LsSuccessors + ?Sized>(
    succ: &S,
    nb: usize,
) -> (Vec<usize>, Vec<usize>) {
    let mut seen = vec![false; nb];
    let mut post: Vec<usize> = Vec::with_capacity(nb);
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..nb {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        stack.push((root, 0));
        while let Some(top) = stack.last_mut() {
            let (b, next) = *top;
            let succs = succ.ls_successors(b);
            if next < succs.len() {
                top.1 += 1;
                let s = succs[next];
                if s < nb && !seen[s] {
                    seen[s] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
    }
    // The driver visits `order[nb - 1]` first: the first block finished.
    let mut order = vec![0usize; nb];
    let mut rank = vec![0usize; nb];
    for (i, &b) in post.iter().enumerate() {
        let k = nb - 1 - i;
        order[k] = b;
        rank[b] = k;
    }
    (order, rank)
}

/// [`ls_fixed_point_worklist_preds`] with the pass order given (interpreter
/// round i3 wave 71, lane L2): each pass visits the dirty blocks by
/// decreasing `rank` (`order[rank[b]] == b`, a permutation of `0..nb`).
/// The same argument as the index-order worklist makes it the round robin in
/// that order, pass for pass; with the identity order it IS the index-order
/// worklist. Any order converges to the same least fixed point.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ls_fixed_point_worklist_ordered<S, P>(
    succ: &S,
    preds: &P,
    order: &[usize],
    rank: &[usize],
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool
where
    S: LsSuccessors + ?Sized,
    P: LsPredecessors + ?Sized,
{
    let nb = sets.nb.min(order.len()).min(rank.len());
    let mut dirty_one = [0u64; 1];
    let mut dirty_many: Vec<u64>;
    let dirty: &mut [u64] = if nb <= 64 {
        &mut dirty_one[..]
    } else {
        dirty_many = vec![0u64; nb.div_ceil(64)];
        &mut dirty_many[..]
    };
    for k in 0..nb {
        ls_bit_set(&mut *dirty, k);
    }
    let mut scratch = vec![0u64; sets.words];
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut limit = nb;
        while let Some(k) = ls_highest_set_below(&*dirty, limit) {
            limit = k;
            dirty[k / 64] &= !(1u64 << (k % 64));
            let b = order[k];
            if b >= nb {
                continue;
            }
            *visits += 1;
            let (out_changed, in_changed) =
                ls_fixed_point_visit(succ, sets, b, li, lout, &mut scratch);
            changed |= out_changed | in_changed;
            if in_changed {
                for &p in preds.ls_predecessors(b) {
                    if p < nb {
                        ls_bit_set(&mut *dirty, rank[p]);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// [`ls_fixed_point_worklist`] over a predecessor index the caller supplies
/// (interpreter round i3 wave 71, lane L2). The dirty set lives on the stack
/// for graphs of up to 64 blocks (one word), so on method-sized graphs the
/// driver allocates only the transfer scratch, as the round robin does.
pub(crate) fn ls_fixed_point_worklist_preds<S, P>(
    succ: &S,
    preds: &P,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool
where
    S: LsSuccessors + ?Sized,
    P: LsPredecessors + ?Sized,
{
    let nb = sets.nb;
    let mut dirty_one = [0u64; 1];
    let mut dirty_many: Vec<u64>;
    let dirty: &mut [u64] = if nb <= 64 {
        &mut dirty_one[..]
    } else {
        dirty_many = vec![0u64; nb.div_ceil(64)];
        &mut dirty_many[..]
    };
    for b in 0..nb {
        ls_bit_set(&mut *dirty, b);
    }
    let mut scratch = vec![0u64; sets.words];
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut limit = nb;
        while let Some(b) = ls_highest_set_below(&*dirty, limit) {
            limit = b;
            dirty[b / 64] &= !(1u64 << (b % 64));
            *visits += 1;
            let (out_changed, in_changed) =
                ls_fixed_point_visit(succ, sets, b, li, lout, &mut scratch);
            changed |= out_changed | in_changed;
            if in_changed {
                for &p in preds.ls_predecessors(b) {
                    if p < nb {
                        ls_bit_set(&mut *dirty, p);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// The lowest set bit at or above `from` and below `limit`, if any.
#[inline]
fn ls_lowest_set_from(bits: &[u64], from: usize, limit: usize) -> Option<usize> {
    if from >= limit {
        return None;
    }
    let mut wi = from / 64;
    let mut word = bits.get(wi).copied().unwrap_or(0) & (u64::MAX << (from % 64));
    loop {
        if word != 0 {
            let b = wi * 64 + word.trailing_zeros() as usize;
            return (b < limit).then_some(b);
        }
        wi += 1;
        if wi * 64 >= limit {
            return None;
        }
        word = bits.get(wi).copied().unwrap_or(0);
    }
}

/// A dirty set of `nb` bits, on the stack up to 64 blocks.
enum LsDirty {
    One([u64; 1]),
    Many(Vec<u64>),
}

impl LsDirty {
    fn all(nb: usize) -> Self {
        let mut d = if nb <= 64 {
            LsDirty::One([0u64; 1])
        } else {
            LsDirty::Many(vec![0u64; nb.div_ceil(64)])
        };
        for b in 0..nb {
            ls_bit_set(d.bits(), b);
        }
        d
    }

    #[inline]
    fn bits(&mut self) -> &mut [u64] {
        match self {
            LsDirty::One(w) => &mut w[..],
            LsDirty::Many(v) => &mut v[..],
        }
    }
}

/// The backward dirty-block driver with the transfer as a closure (JIT round
/// 19 proposal wave 10, lane lv10;
/// `i70-L2-proposal-closure-transfer-dirty-driver-for-the-dead-ref-planner`,
/// `CRATONVM_JIT_IR_DEAD_REF_LIVENESS_WORKLIST`).
///
/// `visit(b)` recomputes block `b` from its own state and its successors'
/// published sets and returns whether `b`'s published set changed. Each pass
/// visits the dirty blocks from the highest index down; a change dirties
/// `b`'s predecessors (`preds`). The argument of [`ls_fixed_point_worklist`]
/// holds for any such transfer: a clean block's successors have not changed
/// since its last visit, so recomputing it would change nothing, and the
/// state after every pass, the pass count and the verdict are the whole-pass
/// loop's (`for b in (0..nb).rev()`, stop after a pass with no change, at
/// most `max_iterations` passes), converged or cut off.
pub(crate) fn ls_closure_fixed_point_backward<P: LsPredecessors + ?Sized>(
    preds: &P,
    nb: usize,
    max_iterations: usize,
    visits: &mut usize,
    mut visit: impl FnMut(usize) -> bool,
) -> bool {
    let mut dirty = LsDirty::all(nb);
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut limit = nb;
        while let Some(b) = ls_highest_set_below(dirty.bits(), limit) {
            limit = b;
            dirty.bits()[b / 64] &= !(1u64 << (b % 64));
            *visits += 1;
            if visit(b) {
                changed = true;
                for &p in preds.ls_predecessors(b) {
                    if p < nb {
                        ls_bit_set(dirty.bits(), p);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// The forward mirror of [`ls_closure_fixed_point_backward`] (JIT round 19
/// wave 10, lane lv10): `visit(b)` recomputes `b` from its predecessors'
/// published sets; each pass visits the dirty blocks from the LOWEST index
/// up, and a change dirties `b`'s successors (`succs`, read through
/// [`LsPredecessors`] as a plain per-block list). The same pass-for-pass
/// argument makes it the whole-pass loop `for b in 0..nb`.
pub(crate) fn ls_closure_fixed_point_forward<S: LsPredecessors + ?Sized>(
    succs: &S,
    nb: usize,
    max_iterations: usize,
    visits: &mut usize,
    mut visit: impl FnMut(usize) -> bool,
) -> bool {
    let mut dirty = LsDirty::all(nb);
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut from = 0usize;
        while let Some(b) = ls_lowest_set_from(dirty.bits(), from, nb) {
            from = b + 1;
            dirty.bits()[b / 64] &= !(1u64 << (b % 64));
            *visits += 1;
            if visit(b) {
                changed = true;
                for &s in succs.ls_predecessors(b) {
                    if s < nb {
                        ls_bit_set(dirty.bits(), s);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// Copy a dense per-block bit set (`dense_words` per block, bit `k` = the
/// value `sparse_of[k]`) into the node-id layout (`words` per block, bit `id`
/// of word `id / 64`) the core publishes.
fn ls_dense_bits_to_node_bits(
    dense: &[u64],
    dense_words: usize,
    words: usize,
    nb: usize,
    sparse_of: &[usize],
) -> Vec<u64> {
    let mut out = vec![0u64; words * nb];
    for b in 0..nb {
        let from = match dense.get(b * dense_words..(b + 1) * dense_words) {
            Some(from) => from,
            None => continue,
        };
        let to = &mut out[b * words..(b + 1) * words];
        ls_bits_for_each(from, |k| {
            if let Some(&id) = sparse_of.get(k) {
                ls_bit_set(&mut *to, id);
            }
        });
    }
    out
}

/// The one body behind [`liveness_core_sparse`] (`DENSE = false`: bit `i` is
/// node `i`) and [`liveness_core_dense`] (`DENSE = true`: bit `k` is the k-th
/// wanted node). Monomorphised, so the sparse core pays nothing for the
/// numbering. Every bit operation is guarded by `wanted`, so only wanted
/// values ever reach a set, and the dense sets are an exact relabelling of the
/// sparse ones: the same bits change on the same pass, the fixed point runs
/// the same number of passes, and mapping the sets back gives the sparse sets.
#[allow(clippy::too_many_arguments)]
fn liveness_core_body<const DENSE: bool>(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
    budget_by_values: bool,
    fixed_point: LivenessFixedPoint,
    visits: &mut usize,
) -> LivenessCore {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    let wanted = |i: usize| i < n && wants.get(i).copied().unwrap_or(false);

    // ── The value numbering (wave 67) ────────────────────────────────
    // `dense_of[i]` is node `i`'s bit when wanted (`usize::MAX`, which every
    // bit helper ignores, otherwise); `sparse_of[k]` is bit `k`'s node.
    let mut dense_of: Vec<usize> = Vec::new();
    let mut sparse_of: Vec<usize> = Vec::new();
    if DENSE {
        dense_of = vec![usize::MAX; n];
        for (i, cell) in dense_of.iter_mut().enumerate() {
            if wanted(i) {
                *cell = sparse_of.len();
                sparse_of.push(i);
            }
        }
    }
    let values = if DENSE { sparse_of.len() } else { n };
    let bit = |i: usize| -> usize {
        if DENSE {
            dense_of.get(i).copied().unwrap_or(usize::MAX)
        } else {
            i
        }
    };
    let node_of_bit = |k: usize| -> usize {
        if DENSE {
            sparse_of.get(k).copied().unwrap_or(usize::MAX)
        } else {
            k
        }
    };

    // ── Positions and block spans ────────────────────────────────────
    let mut pos_of: Vec<Option<usize>> = vec![None; n];
    let mut span: Vec<(usize, usize)> = Vec::with_capacity(nb);
    let mut seq = 0usize;
    for block in &schedule.blocks {
        let start = seq;
        for &nid in &block.nodes {
            if let Some(cell) = pos_of.get_mut(nid as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = pos_of.get_mut(term as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        // The outgoing edge, where `emit_phi_copies` reads this block's φ
        // arguments.
        let end = seq;
        seq += 1;
        span.push((start, end));
    }
    let total_positions = seq;

    let words = n.div_ceil(64).max(1);
    let mut lo = vec![usize::MAX; n];
    let mut hi = vec![0usize; n];
    let mut set_only_by_phi_rule = vec![false; n];
    let mut reads: Vec<(NodeId, usize)> = Vec::new();
    let mut live_in: Vec<u64> = Vec::new();
    let mut live_out: Vec<u64> = Vec::new();
    let mut converged = false;

    // The published `words` stays node-indexed; the sets themselves use
    // `bit_words` (equal to `words` in the sparse core).
    let bit_words = values.div_ceil(64).max(1);
    let budget_units = if budget_by_values { values } else { n };
    if budget_units.saturating_mul(nb) <= work_budget {
        let node_words = words;
        let words = bit_words;
        let mut def_bits = vec![0u64; words * nb];
        let mut use_bits = vec![0u64; words * nb];
        let mut phi_out_bits = vec![0u64; words * nb];

        // ── Direct uses and definitions, in block order ──────────────
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
                if !matches!(node.op, Op::Phi) {
                    for (slot, &inp) in node.inputs.iter().enumerate() {
                        let ii = inp as usize;
                        if inp == NO_NODE || !wanted(ii) {
                            continue;
                        }
                        if rules.token_edges_not_uses && crate::ir::is_memory_token_slot(node, slot)
                        {
                            continue;
                        }
                        lo[ii] = lo[ii].min(upos);
                        hi[ii] = hi[ii].max(upos);
                        if rules.collect_reads {
                            reads.push((inp, upos));
                        }
                        if !ls_bit_get(&local_def, bit(ii)) {
                            ls_bit_set(&mut use_bits[base..base + words], bit(ii));
                        }
                    }
                }
                if wanted(ui) {
                    ls_bit_set(&mut local_def, bit(ui));
                    ls_bit_set(&mut def_bits[base..base + words], bit(ui));
                    lo[ui] = lo[ui].min(upos);
                    hi[ui] = hi[ui].max(upos);
                }
            }
        }

        // ── φ edge reads (and the φ edge rule) ───────────────────────
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
        // `(φ, edge position)` pairs, applied after the widening so the
        // pre-rule "placed by nothing" question can still be asked; `lo`/`hi`
        // are a min/max, so the order is not otherwise observable.
        let mut phi_edges: Vec<(usize, usize)> = Vec::new();
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
            // merge.inputs[k] is the control token for φ value k (= input k+1).
            for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                let pred = match ls_ctrl_block_of(graph, schedule, ctrl_in) {
                    Some(pred) if pred < nb => pred,
                    _ => continue,
                };
                let at = span[pred].1;
                for &pid in phis {
                    let phi = graph.nodes.get(pid as usize);
                    let v = match phi.and_then(|p| p.inputs.get(k + 1)) {
                        Some(&v) if v != NO_NODE => v,
                        _ => continue,
                    };
                    let vi = v as usize;
                    if !wanted(vi) {
                        continue;
                    }
                    // A memory φ gets no edge copy, so under the token rule
                    // its source is not read here.
                    let source_unread =
                        rules.token_edges_not_uses && phi.is_some_and(|p| p.ty == IrType::Memory);
                    if !source_unread {
                        lo[vi] = lo[vi].min(at);
                        hi[vi] = hi[vi].max(at);
                        if rules.collect_reads {
                            reads.push((v, at));
                        }
                        ls_bit_set(&mut phi_out_bits[pred * words..(pred + 1) * words], bit(vi));
                    }
                    if rules.phi_edge_interfere {
                        let pi = pid as usize;
                        if wanted(pi) {
                            phi_edges.push((pi, at));
                        }
                    }
                }
            }
        }

        // ── The backward fixed point ─────────────────────────────────
        let mut li = vec![0u64; words * nb];
        let mut lout = vec![0u64; words * nb];
        let sets = LsFixedPointSets {
            nb,
            words,
            phi_out: &phi_out_bits,
            uses: &use_bits,
            defs: &def_bits,
        };
        let blocks: &[crate::ir_schedule::Block] = &schedule.blocks;
        converged = ls_fixed_point_on_schedule(
            blocks,
            fixed_point,
            sets,
            &mut li,
            &mut lout,
            max_iterations,
            visits,
        );

        // ── Widen each range over every block it is live in ──────────
        if converged {
            for b in 0..nb {
                let base = b * words;
                let (start, end) = span[b];
                ls_bits_for_each(&li[base..base + words], |k| {
                    let v = node_of_bit(k);
                    if wanted(v) {
                        lo[v] = lo[v].min(start);
                    }
                });
                ls_bits_for_each(&lout[base..base + words], |k| {
                    let v = node_of_bit(k);
                    if wanted(v) {
                        hi[v] = hi[v].max(end);
                    }
                });
            }
        }

        // ── The φ edge rule ──────────────────────────────────────────
        for &(pi, _) in &phi_edges {
            if lo[pi] == usize::MAX {
                set_only_by_phi_rule[pi] = true;
            }
        }
        for &(pi, at) in &phi_edges {
            lo[pi] = lo[pi].min(at);
            hi[pi] = hi[pi].max(at);
        }

        if DENSE {
            // Publish in the node-id layout, so every consumer (the live
            // model's `block_live_in`, the OSR live-in checks) reads the bits
            // it always read.
            live_in = ls_dense_bits_to_node_bits(&li, words, node_words, nb, &sparse_of);
            live_out = ls_dense_bits_to_node_bits(&lout, words, node_words, nb, &sparse_of);
        } else {
            live_in = li;
            live_out = lout;
        }
    }

    LivenessCore {
        pos_of,
        span,
        total_positions,
        lo,
        hi,
        set_only_by_phi_rule,
        converged,
        words,
        live_in,
        live_out,
        reads,
    }
}

/// The shared tail of a [`LiveModel`] build: finalised ranges, register
/// classes, sorted uses, `peak_live`, and the back-edge (`carried`) facts.
fn finish_live_model(graph: &Graph, schedule: &Schedule, head: LiveModelHead) -> LiveModel {
    let LiveModelHead {
        pos_of,
        span,
        total_positions,
        block_of_pos,
        wants_loc,
        mut pinned,
        lo,
        hi,
        mut uses,
        weight,
        loop_depth,
        converged,
        block_live_in,
    } = head;
    let n = graph.nodes.len();

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
    //
    // **Deliberately NOT the dominator test `ls_loop_depths` uses (R10).** That
    // one answers "is this block inside a loop", a question about the graph, and
    // an order-based answer to it is simply wrong. This one answers "does a
    // safepoint poll get emitted at this block's outgoing edge", a question
    // about what `ir_lower::lower_block` and `MachineModel::for_graph` actually
    // do — and both of them use the index comparison. Replacing it here would
    // make `carried` name edges that carry no poll and miss edges that do,
    // which is a worse answer however much more principled the test looks.
    //
    // Each back edge is kept with its WINDOW, `[first position of the lowest
    // target block, edge]` — the positions the index test counts as the loop.
    let mut back_edges: Vec<(usize, usize)> = Vec::new();
    for (b, block) in schedule.blocks.iter().enumerate() {
        let window_lo = block
            .successors
            .iter()
            .filter(|&&s| s <= b)
            .filter_map(|&s| span.get(s).map(|&(start, _)| start))
            .min();
        if let (Some(window_lo), Some(&(_, edge))) = (window_lo, span.get(b)) {
            back_edges.push((edge, window_lo));
        }
    }
    // `carried` asks only whether the range covers a back edge;
    // `carried_and_read` also asks whether the value is READ inside that
    // edge's window, i.e. whether evicting it costs a reload per iteration
    // (round 11 wave 5; see the field's doc for the eviction it corrects).
    //
    // `back_edges` is ascending (block spans are laid out in block order), so
    // the edges inside `[lo, hi]` start at a binary-searched index (round 11
    // wave 4). A value spanning more than `CARRIED_EDGE_SCAN` edges is
    // answered `carried_and_read = carried`, which bounds the refinement at
    // O(values × 64 × log uses); so is every value of an unconverged model,
    // which has no uses to consult.
    const CARRIED_EDGE_SCAN: usize = 64;
    let mut carried = vec![false; n];
    let mut carried_and_read = vec![false; n];
    if !back_edges.is_empty() {
        for (id, r) in range.iter().enumerate() {
            let Some(r) = r else { continue };
            let first = back_edges.partition_point(|&(e, _)| e < r.lo);
            let in_range = back_edges
                .get(first..)
                .unwrap_or(&[])
                .iter()
                .take_while(|&&(e, _)| e <= r.hi);
            let value_uses = uses.get(id).map(Vec::as_slice).unwrap_or(&[]);
            for (scanned, &(edge, window_lo)) in in_range.enumerate() {
                carried[id] = true;
                if scanned >= CARRIED_EDGE_SCAN || !converged {
                    carried_and_read[id] = true;
                    break;
                }
                let at = value_uses.partition_point(|&u| u < window_lo);
                if value_uses.get(at).is_some_and(|&u| u <= edge) {
                    carried_and_read[id] = true;
                    break;
                }
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
        carried_and_read,
        peak_live: peak_live.max(0) as usize,
        converged,
        block_live_in,
    }
}

/// `CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS=1` (opt-in, diagnostic only;
/// interpreter round i2 wave 63, lane L2, the first measurement of
/// `i58-L2-proposal-the-live-model-skips-memory-token-reads`): print one
/// [`token_trim_census`] line per optimizing compile. Changes no byte of any
/// compiled body; costs a second [`build_live_model_with_token_edges`] pass
/// per compile while on.
pub fn ls_token_trim_census_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS")
}

/// How many examples [`TokenTrimCensus::examples`] keeps.
pub const TOKEN_TRIM_CENSUS_EXAMPLES: usize = 4;

/// What `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` would change in one graph's
/// liveness model: the population whose interval END (or start, through the
/// loop dataflow) is set only by a memory-token read or a memory φ's edge.
///
/// The i58-L2 proposal names this as its first measurement ("zero means close
/// this"): the switch it proposes already exists (JIT round 17 wave 3, KEEP
/// OFF "within noise" on `R17IrLowerCopies`), and that triage never counted
/// how many values the switch actually moves in shipped code. This answers
/// it per compile without timing anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenTrimCensus {
    /// Values that want a location (identical in both models).
    pub values: usize,
    /// Values whose interval is strictly shorter with the switch on.
    pub trimmed: usize,
    /// Of [`Self::trimmed`], the value-typed ones (`Int` / `Long` / `Float` /
    /// `Double` / `Ref`): the population the proposal is about, a volatile
    /// `getstatic`, an `instanceof` / `checkcast` result, a call's result
    /// reused as the memory chain's producer.
    pub trimmed_value_typed: usize,
    /// Of [`Self::trimmed`], memory φs (their source edges stop being reads).
    pub trimmed_memory: usize,
    /// Sum over trimmed values of the positions removed from the interval.
    pub positions_trimmed: usize,
    /// Values that span a back edge with the switch off and none with it on.
    pub carried_dropped: usize,
    /// `(loop header, value)` pairs live into the header with the switch off
    /// and not with it on: the OSR-entry refusal population
    /// (`CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_ORDER_USES` excuses these at the
    /// entry only).
    pub header_live_in_dropped: usize,
    /// [`LiveModel::peak_live`] with the switch off.
    pub peak_live_off: usize,
    /// [`LiveModel::peak_live`] with the switch on.
    pub peak_live_on: usize,
    /// The first [`TOKEN_TRIM_CENSUS_EXAMPLES`] trimmed values, ascending id:
    /// `(id, type, range off, range on)`.
    pub examples: Vec<(NodeId, IrType, PosRange, PosRange)>,
    /// Interpreter round i2 wave 64 (lane L2,
    /// `i63-L2-proposal-frame-home-ranges-skip-memory-token-reads`): the
    /// slot-plan column, what `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES`
    /// would change in the FRAME. Filled by `ir_lower` (which owns
    /// `plan_slots`); `None` from [`token_trim_census`] alone.
    pub slot_plan: Option<TokenTrimSlotColumn>,
}

/// Frame words with and without the slot-plan token-edge rule (see
/// [`TokenTrimCensus::slot_plan`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenTrimSlotColumn {
    /// `SlotPlan::slots` (8-byte spill words) without the rule.
    pub words_off: usize,
    /// `SlotPlan::slots` with the rule.
    pub words_on: usize,
    /// Of [`Self::words_off`], the colours that may hold a reference (every
    /// colour that is not a primitive-only one): what the prologue zeroes and
    /// the oop maps can name.
    pub ref_words_off: usize,
    /// Of [`Self::words_on`], the same.
    pub ref_words_on: usize,
    /// Values whose frame-home range is strictly shorter with the rule.
    pub homes_trimmed: usize,
}

impl TokenTrimCensus {
    /// One `[ir-ls]` line.
    pub fn render(&self) -> String {
        let examples: Vec<String> = self
            .examples
            .iter()
            .map(|(id, ty, off, on)| {
                format!("n{id} {ty:?} {}..{}->{}..{}", off.lo, off.hi, on.lo, on.hi)
            })
            .collect();
        // Wave 64: the slot-plan column goes LAST so a reader of the wave-63
        // fields (everything up to `examples=[..]`) parses this line as before.
        let slot_plan = match &self.slot_plan {
            Some(c) => format!(
                " slot-words={}->{} ref-words={}->{} homes-trimmed={}",
                c.words_off, c.words_on, c.ref_words_off, c.ref_words_on, c.homes_trimmed
            ),
            None => String::new(),
        };
        format!(
            "[ir-ls] token-trim census: values={} trimmed={} (value-typed={} memory={}) \
             positions={} carried-dropped={} header-live-in-dropped={} peak-live={}->{} \
             examples=[{}]{}",
            self.values,
            self.trimmed,
            self.trimmed_value_typed,
            self.trimmed_memory,
            self.positions_trimmed,
            self.carried_dropped,
            self.header_live_in_dropped,
            self.peak_live_off,
            self.peak_live_on,
            examples.join(", "),
            slot_plan,
        )
    }
}

/// Build the liveness model both ways and compare (see [`TokenTrimCensus`]).
/// `None` when either model did not converge (every range is then the whole
/// method, and a difference would mean nothing).
pub fn token_trim_census(graph: &Graph, schedule: &Schedule) -> Option<TokenTrimCensus> {
    let off = build_live_model_with_token_edges(graph, schedule, false);
    let on = build_live_model_with_token_edges(graph, schedule, true);
    token_trim_census_of(graph, schedule, &off, &on)
}

/// [`token_trim_census`] over two models the caller built (`off` without the
/// token-edge rule, `on` with it).
pub fn token_trim_census_of(
    graph: &Graph,
    schedule: &Schedule,
    off: &LiveModel,
    on: &LiveModel,
) -> Option<TokenTrimCensus> {
    if !off.converged || !on.converged {
        return None;
    }
    let n = graph.nodes.len();
    let mut census = TokenTrimCensus {
        peak_live_off: off.peak_live,
        peak_live_on: on.peak_live,
        ..TokenTrimCensus::default()
    };
    for id in 0..n {
        let (Some(Some(r_off)), Some(Some(r_on))) = (off.range.get(id), on.range.get(id)) else {
            continue;
        };
        census.values += 1;
        let len_off = r_off.hi.saturating_sub(r_off.lo);
        let len_on = r_on.hi.saturating_sub(r_on.lo);
        if r_off != r_on && len_on < len_off {
            census.trimmed += 1;
            census.positions_trimmed += len_off - len_on;
            let ty = graph.nodes[id].ty;
            match ty {
                IrType::Int | IrType::Long | IrType::Float | IrType::Double | IrType::Ref => {
                    census.trimmed_value_typed += 1;
                }
                IrType::Memory => census.trimmed_memory += 1,
                _ => {}
            }
            if census.examples.len() < TOKEN_TRIM_CENSUS_EXAMPLES {
                // Cast: an index into the node arena is a `NodeId`.
                census.examples.push((id as NodeId, ty, *r_off, *r_on));
            }
        }
        if off.carried.get(id).copied().unwrap_or(false)
            && !on.carried.get(id).copied().unwrap_or(false)
        {
            census.carried_dropped += 1;
        }
    }
    // Loop headers by the same index test the back-edge positions use: a
    // block some block at or after it branches to.
    let mut is_header = vec![false; schedule.blocks.len()];
    for (p, blk) in schedule.blocks.iter().enumerate() {
        for &s in &blk.successors {
            if s <= p {
                if let Some(cell) = is_header.get_mut(s) {
                    *cell = true;
                }
            }
        }
    }
    for (h, &header) in is_header.iter().enumerate() {
        if !header {
            continue;
        }
        for id in 0..n {
            // Cast: an index into the node arena is a `NodeId`.
            let v = id as NodeId;
            if off.live_in_at(h, v) == Some(true) && on.live_in_at(h, v) == Some(false) {
                census.header_live_in_dropped += 1;
            }
        }
    }
    Some(census)
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

    /// Is `node` live on entry to `block`? `None` when the model does not know
    /// — it did not converge, or it was built by hand without
    /// [`LiveModel::block_live_in`] — so a caller can fall back to its own
    /// conservative answer instead of mistaking "unknown" for "dead".
    pub fn live_in_at(&self, block: usize, node: NodeId) -> Option<bool> {
        if !self.converged || self.block_live_in.is_empty() {
            return None;
        }
        let words = self.block_live_in.get(block)?;
        Some(ls_bit_get(words, node as usize))
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

    /// Pin values this model cannot see a reason to pin but the caller can —
    /// today, the scalar-replacement field values `ir_lower::plan_slots` pins
    /// from its `sr_map`, which [`build_live_model`] is never handed. Returns
    /// how many were newly pinned.
    ///
    /// Call it BEFORE [`Self::release_deopt_pins`], so the extra pins are
    /// released on exactly the same write-through argument as the deopt pins
    /// they are a kind of, and a consumer that does not release keeps them.
    /// Ids that name no value (`!wants_loc`) are ignored: a pin on a node with
    /// no location means nothing. See
    /// `docs/known-issues/jit/regalloc-live-model-misses-scalar-replacement-pins-20260918.md`.
    pub fn pin_values(&mut self, ids: impl IntoIterator<Item = NodeId>) -> usize {
        let mut pinned = 0usize;
        for id in ids {
            let i = id as usize;
            if id == NO_NODE || !self.wants_loc.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Some(cell) = self.pinned.get_mut(i) {
                if !*cell {
                    *cell = true;
                    pinned += 1;
                }
            }
        }
        pinned
    }
}

/// JIT round 19 wave 4 (lane irops3, IROPS2-3): every general-purpose
/// register `ir_lower`'s `Op::ScalarIntrinsic` arm writes for `sop` -- the
/// operand loads (`a` into RAX, `b` into RCX) and each arm's scratch (RDX) --
/// so [`MachineModel::for_graph`] can clobber all of them at the node's
/// position. A superset where the arm has two forms (the `LZCNT`/`TZCNT` row
/// writes only RAX, the `BSR`/`BSF` one RAX, RCX and RDX). The FP families
/// write XMM0/XMM1 only, which no file this crate builds contains.
///
/// A pure modelling fix: `ir_lower`'s own file excludes RAX/RCX/RDX and
/// `destroyed.retain(|r| regs.contains(*r))` drops these clobbers for it, so
/// production allocations are unchanged. It is what keeps a value out of RCX
/// across `multiplyHigh` (and out of RDX across `abs`, ...) the day the IR
/// file gains the [`FIXED_OPERAND_GPRS`]. Exhaustive on purpose: a new
/// family is a compile error here until its writes are stated.
pub(crate) fn scalar_op_gp_writes(sop: crate::ir::ScalarOp) -> &'static [PhysReg] {
    use crate::ir::ScalarOp as S;
    const A: &[PhysReg] = &[X86_RAX];
    const AC: &[PhysReg] = &[X86_RAX, X86_RCX];
    const AD: &[PhysReg] = &[X86_RAX, X86_RDX];
    const ACD: &[PhysReg] = &[X86_RAX, X86_RCX, X86_RDX];
    const NONE: &[PhysReg] = &[];
    match sop {
        S::MinI | S::MaxI | S::MinL | S::MaxL => AC,
        S::AbsI | S::AbsL => AD,
        S::CompareI | S::CompareL => ACD,
        S::NlzI | S::NlzL | S::NtzI | S::NtzL => ACD,
        S::ReverseBytesI | S::ReverseBytesL => A,
        S::LowestOneBitI | S::LowestOneBitL => AC,
        S::HighestOneBitI | S::HighestOneBitL => ACD,
        S::RotateLeftI | S::RotateLeftL | S::RotateRightI | S::RotateRightL => AC,
        // The sign mask is staged through RAX.
        S::AbsF | S::AbsD => A,
        S::DoubleToRawLongBits | S::LongBitsToDouble => A,
        S::FloatToRawIntBits | S::IntBitsToFloat => A,
        S::MultiplyHighL | S::UnsignedMultiplyHighL => ACD,
        S::BitCountI | S::BitCountL => A,
        // nio20 wave 6 (dev): `SUB EAX, lo; CMP EAX, 0x400; SETB AL; MOVZX`.
        S::IsHighSurrogate | S::IsLowSurrogate => A,
        // JIT r20 wave 11 (lane pIR): `CMP EAX, imm; SETcc AL; MOVZX [; INC]`.
        S::CharCount | S::IsBmpCodePoint | S::IsValidCodePoint => A,
        // JIT r22 wave 1 (lane intrinsics): `Math.round`'s bit arithmetic
        // stages the bits in RAX, the exponent / shift in RCX / RDX.
        S::RoundD | S::RoundF => ACD,
        // j27 wave 6 (lane T1BCE): `PEXT` / `PDEP RAX, RAX, RCX`, the mask
        // staged in RCX.
        S::CompressI | S::CompressL | S::ExpandI | S::ExpandL => AC,
        S::SqrtD
        | S::MinF
        | S::MaxF
        | S::MinD
        | S::MaxD
        | S::FloorD
        | S::CeilD
        | S::RintD
        | S::FmaD
        | S::FmaF => NONE,
    }
}

impl MachineModel {
    /// Derive the machine model for one scheduled graph over `regs`.
    ///
    /// Produces the clobber set (every call position destroys every
    /// caller-saved register in the file), the x86 **fixed-operand** constraints
    /// and the safepoint set. There are no entry-ABI constraints: a backend
    /// that wants a `Param` pinned to its incoming register adds a
    /// [`FixedConstraint`] at the `Param`'s own position (`live.pos_of`).
    ///
    /// # R6: the fixed-operand rules are constraints, not clobbers
    ///
    /// This function used to push RAX/RDX (for `Div`/`Rem`) and RCX (for the
    /// shifts) onto the *clobber* list, with a comment saying they mattered
    /// "only if a caller puts those in the file" — and `RegFile::x86_64` did
    /// not, so `destroyed.retain(|r| regs.contains(*r))` deleted every one of
    /// them on the next line. The whole constraint path was unreachable and the
    /// question `idiv` asks was answered by removing five GPRs from the file.
    ///
    /// The two forms are not interchangeable and only one of them is true:
    ///
    /// * `idiv r/m` reads the dividend in **RDX:RAX** and writes the quotient to
    ///   RAX and the remainder to RDX. So RAX and RDX *are* destroyed — the
    ///   clobber is right — **and** the dividend must be in RAX, which only a
    ///   [`FixedConstraint`] can say. Both are emitted, and they meet at one
    ///   position: see the early-clobber rule on [`ls_select_register`].
    /// * `shl r/m, cl` reads **CL** and writes only `r/m`. RCX is *not*
    ///   destroyed, so the clobber was an over-approximation that forced every
    ///   value out of RCX for nothing. Only the constraint is emitted.
    ///
    /// Every constraint is emitted **only when the register it names is in
    /// `regs`**. A backend that keeps RAX/RCX/RDX as private scratch (which is
    /// what `ir_lower` does — see [`RegFile::x86_64_scratch_reserved`]) gets a
    /// model byte-for-byte identical to the one it got before R6, because none
    /// of those registers is in its file.
    ///
    /// The `Div`/`Rem` rule is also gated on an **integral** result type, which
    /// the old clobber rule was not: `Op::Div` on `IrType::Float`/`Double`
    /// lowers to `divss`/`divsd` and touches no GPR at all. That was harmless
    /// while the registers were filtered out and is a live wrong answer now.
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
        let mut fixed: Vec<FixedConstraint> = Vec::new();

        // Pin `node`'s `k`-th input to `reg` at `pos`, if that is a thing this
        // model can say. Three refusals, each of which makes the constraint
        // meaningless rather than merely unhelpful:
        //
        //   * `reg` is not allocatable — the emitter owns it, and a constraint
        //     naming a register the allocator cannot hand out is filtered out
        //     again by `allocate_linear_scan` anyway;
        //   * the input edge is absent or out of range — a malformed graph must
        //     not produce a constraint on node 0;
        //   * the input's register class is not `reg`'s (`RegClass::of`): an
        //     `IrType` with no GP class cannot be required to sit in a GPR.
        //
        // A constraint on a value that is never promoted (a phi, or anything a
        // deopt frame names) is still emitted and is still worth emitting: the
        // allocator gives such a value no interval, so nothing honours it, but
        // proof 4b's second half still keeps every OTHER value out of `reg` at
        // `pos` — which is the half the emitter depends on when it loads the
        // operand from the home slot itself.
        fn pin_input(
            fixed: &mut Vec<FixedConstraint>,
            graph: &Graph,
            regs: &RegFile,
            node: &crate::ir::Node,
            k: usize,
            pos: usize,
            reg: PhysReg,
        ) {
            if !regs.contains(reg) {
                return;
            }
            let Some(&input) = node.inputs.as_slice().get(k) else {
                return;
            };
            if input == NO_NODE {
                return;
            }
            let Some(operand) = graph.nodes.get(input as usize) else {
                return;
            };
            if RegClass::of(operand.ty) != Some(reg.class) {
                return;
            }
            fixed.push(FixedConstraint {
                node: input,
                pos,
                reg,
            });
        }

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
                if ir_op_is_call(&node.op) && !is_inline_integer_rem(node) {
                    destroyed.extend_from_slice(&caller_saved);
                }
                // ── Fixed-operand x86 instructions (R6) ──────────────
                match node.op {
                    // `idiv r/m` (and `div`): dividend in RDX:RAX, quotient to
                    // RAX, remainder to RDX. The `cqo`/`cdq` that sign-extends
                    // into RDX is part of the same expansion and destroys RDX
                    // before the divide, so RDX is unusable across `pos` even
                    // for a value the divide itself would not have touched.
                    //
                    // Integral results only: `Op::Div` on `Float`/`Double` is
                    // `divss`/`divsd`, which reads and writes one XMM and no
                    // GPR whatsoever.
                    Op::Div | Op::Rem if matches!(node.ty, IrType::Int | IrType::Long) => {
                        destroyed.push(X86_RAX);
                        destroyed.push(X86_RDX);
                        // Input 0 is the dividend (`add_data(Op::Div, ty,
                        // vec![a, b])`), input 1 the divisor. Only the dividend
                        // is placeable: the divisor is the `r/m` operand and may
                        // be any register except the two above, which the
                        // clobber at `pos` already achieves.
                        //
                        // The RESULT is deliberately NOT pinned. It would want
                        // RAX (quotient) or RDX (remainder) at the same `pos`
                        // the dividend wants RAX, and two values requiring one
                        // register at one position is the unsatisfiable model
                        // `allocate_linear_scan` bails on. The clobber keeps the
                        // result out of RAX/RDX, so the emitter moves it out —
                        // one `mov`, which is what every x86 backend emits here
                        // anyway.
                        //
                        // A backend that also pins the DIVISOR — `ir_lower`
                        // emits `CQO` + `IDIV RCX` for a `Long` divide, so it
                        // does — owes that its own `add_clobbers(pos, &[RCX])`
                        // or its own constraint. That is a choice of *that
                        // emitter*, not a requirement of `idiv`, whose `r/m`
                        // operand is any register or memory word; this function
                        // states only what the instruction demands.
                        pin_input(&mut fixed, graph, &regs, node, 0, pos, X86_RAX);
                    }
                    // `shl`/`shr`/`sar r/m, cl`: the count is read from CL and
                    // **nothing writes RCX**. Emitting a clobber here (which is
                    // what this function used to do) forced every value out of
                    // RCX across every shift for no machine reason; the count
                    // being IN RCX is the only real requirement, and a count
                    // used by two shifts now keeps its register between them.
                    //
                    // Input 1 is the count. Input 0 is the two-address operand:
                    // `shl` reads and writes it, so in SSA the emitter always
                    // emits `mov result, in0` first and neither end of that pair
                    // is constrained to any particular register.
                    Op::Shl | Op::Shr | Op::UShr => {
                        pin_input(&mut fixed, graph, &regs, node, 1, pos, X86_RCX);
                    }
                    // JIT round 19 (lane irops19, R15B-31): one-operand
                    // `imul`/`mul r/m64` writes the 128-bit product to RDX:RAX.
                    // JIT round 19 wave 4 (lane irops3, IROPS2-3): and every
                    // other GP register `ir_lower`'s arm writes, RCX (`b`)
                    // included -- see `scalar_op_gp_writes`.
                    Op::ScalarIntrinsic(sop) => {
                        destroyed.extend_from_slice(scalar_op_gp_writes(sop));
                    }
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

        safepoints.sort_unstable();
        safepoints.dedup();
        // Sorted by `(pos, reg, node)`, so a second producer can merge into
        // the same model and `allocate_linear_scan`'s per-node index sees each node's constraints
        // in position order — which is what makes "the earliest constraint in
        // this interval" a well-defined choice.
        fixed.sort_by_key(|f| (f.pos, f.reg, f.node));
        fixed.dedup();
        let mut model = MachineModel {
            regs,
            clobbers,
            fixed,
            safepoints,
            refs_may_cross_safepoints: false,
            reads_precede_clobber: Vec::new(),
        };
        // The loop above pushes one entry per NODE, and two nodes can share a
        // position (`live.pos_of` is not injective for every op). A plain
        // `sort_by_key` left those as two entries at one position, which
        // `clobbered_at`'s binary search resolves to an arbitrary one of them.
        // Canonicalise instead of sorting, so the invariant the field's doc
        // comment states is established by CONSTRUCTION here rather than
        // remembered by every caller.
        model.canonicalize_clobbers();
        model
    }

    /// Declare that every CALL node `admit` accepts — an op [`ir_op_is_call`]
    /// lists, less the inline integer remainder — reads its register operands
    /// before the call destroys the caller-saved registers, i.e. add its
    /// position to [`MachineModel::reads_precede_clobber`]. Returns how many
    /// positions were added.
    ///
    /// **Opt-in, and the caller's claim.** It is true of a call that
    /// marshals its arguments and then executes `CALL`; it is the emitter's to
    /// prove for its own lowering of every op it admits (a frame record or a
    /// safepoint map emitted BEFORE the marshalling, if it calls, breaks it).
    /// Only positions that already carry a clobber are added — anything else
    /// would be a no-op entry. Call it AFTER every clobber the backend adds on
    /// top of [`MachineModel::for_graph`] (`add_clobbers`).
    ///
    /// Round 9 wave 4: `ir_lower` admits the ops that read every operand before
    /// their only `CALL`, and not `Op::Call`, whose safepoint map calls out
    /// before the arguments are marshalled. (The admit-everything form
    /// `mark_call_operand_reads_first` had no production caller and was
    /// deleted in round 11 wave 2; `|_| true` is that form.)
    pub fn mark_operand_reads_first_where(
        &mut self,
        graph: &Graph,
        live: &LiveModel,
        admit: impl Fn(&crate::ir::Node) -> bool,
    ) -> usize {
        let mut added = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !ir_op_is_call(&node.op) || is_inline_integer_rem(node) || !admit(node) {
                continue;
            }
            let Some(pos) = live.pos_of.get(id).copied().flatten() else {
                continue;
            };
            if self.clobbered_at(pos).is_empty() {
                continue;
            }
            if let Err(idx) = self.reads_precede_clobber.binary_search(&pos) {
                self.reads_precede_clobber.insert(idx, pos);
                added += 1;
            }
        }
        added
    }

    /// May `node` hold a register clobbered at `pos` THROUGH `pos`? Only when
    /// `pos` is listed in [`MachineModel::reads_precede_clobber`] and is both
    /// the end of `node`'s live range and its last use — the operand is read,
    /// and nothing reads it again. See that field.
    ///
    /// `pub(crate)` so a consumer's second opinion on the clobber property
    /// (`ir_lower::plan_register_residency`) applies the SAME exemption the
    /// scan and `verify_allocation` apply, rather than a restatement of it.
    pub(crate) fn read_before_clobber(&self, live: &LiveModel, node: NodeId, pos: usize) -> bool {
        let id = node as usize;
        self.reads_precede_clobber.binary_search(&pos).is_ok()
            && live
                .range
                .get(id)
                .copied()
                .flatten()
                .is_some_and(|r| r.hi == pos)
            && live.uses.get(id).and_then(|u| u.last()) == Some(&pos)
    }

    /// Record that `regs` are destroyed at `pos`, preserving the
    /// [`MachineModel::clobbers`] invariant.
    ///
    /// This is the API `MachineModel::for_graph`'s doc comment means when it
    /// says "a backend that allocates over caller-saved registers must add its
    /// own emission's clobbers on top". The obvious spelling —
    /// `model.clobbers.push((pos, regs))` — is WRONG whenever `for_graph`
    /// already recorded something at `pos` (a call position, exactly where a
    /// backend's extra clobbers land): the list then holds two entries for one
    /// position and `clobbered_at`'s binary search sees only one of them.
    ///
    /// Registers outside the allocatable file are dropped, matching what
    /// `for_graph` does with its own fixed-operand rules: the allocator has
    /// nothing to say about a register it cannot hand out.
    ///
    /// Insertion is `O(n)` in the worst case, which is the right trade for a
    /// list built once per compile and then binary-searched per interval. A
    /// producer adding many entries may instead push and call
    /// [`MachineModel::canonicalize_clobbers`] once at the end.
    pub fn add_clobbers(&mut self, pos: usize, regs: &[PhysReg]) {
        let mut add: Vec<PhysReg> = regs
            .iter()
            .copied()
            .filter(|r| self.regs.contains(*r))
            .collect();
        if add.is_empty() {
            return;
        }
        match self.clobbers.binary_search_by_key(&pos, |(p, _)| *p) {
            Ok(idx) => {
                let entry = &mut self.clobbers[idx].1;
                entry.append(&mut add);
                entry.sort_unstable();
                entry.dedup();
            }
            Err(idx) => {
                add.sort_unstable();
                add.dedup();
                self.clobbers.insert(idx, (pos, add));
            }
        }
    }

    /// Restore the [`MachineModel::clobbers`] invariant: sort by position,
    /// MERGE entries that share a position, and sort/dedup each entry's
    /// registers.
    ///
    /// For a producer that built the list by pushing. Idempotent, and a no-op
    /// on a list that already holds the invariant.
    pub fn canonicalize_clobbers(&mut self) {
        // `sort_by_key` is stable, so entries that share a position keep their
        // insertion order and the merge below is deterministic.
        self.clobbers.sort_by_key(|(pos, _)| *pos);
        let mut merged: Vec<(usize, Vec<PhysReg>)> = Vec::with_capacity(self.clobbers.len());
        for (pos, regs) in self.clobbers.drain(..) {
            match merged.last_mut() {
                Some((last_pos, last_regs)) if *last_pos == pos => last_regs.extend(regs),
                _ => merged.push((pos, regs)),
            }
        }
        for (_, regs) in merged.iter_mut() {
            regs.sort_unstable();
            regs.dedup();
        }
        // An entry with no registers is indistinguishable from no entry, and
        // dropping it keeps `clobbered_at`'s `Ok`/`Err` split meaningful.
        merged.retain(|(_, regs)| !regs.is_empty());
        self.clobbers = merged;
    }

    /// Every register destroyed at `pos`.
    ///
    /// Correct only because [`MachineModel::clobbers`] holds one entry per
    /// position: `binary_search_by_key` promises *an* index among equal keys,
    /// not the first or the only one.
    /// The invariant is checked once per compile by [`verify_allocation`]
    /// rather than asserted here: this runs once per position per interval,
    /// and an `O(n)` assertion in it would make the verifier quadratic.
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

/// [`MachineModel::clobbers`] transposed: for each register, the sorted,
/// deduplicated positions at which it is destroyed.
///
/// Round 11 wave 9, compile time. `ls_select_register::first_block` and
/// [`verify_allocation`]'s proof 4 both asked "the first position in
/// `[lo, hi]` at which `reg` is clobbered" by walking EVERY clobber site in
/// the range and testing each entry for `reg`. A callee-saved register is
/// clobbered nowhere, so for it that walk visited every call and every
/// back-edge poll the interval spans and found nothing — per register, per
/// attempt, per interval in the scan, and once more per segment in the proof.
/// A long-lived value in a call-heavy method paid O(calls) per question. One
/// binary search in the register's own list answers the same question.
///
/// Same answers as the walk on every model that holds the `clobbers`
/// invariant (strictly increasing positions), which `verify_allocation`
/// checks before it builds one of these; on a model that does not, both the
/// walk and this are meaningless and the verifier refuses the model.
struct ClobberIndex {
    by_reg: HashMap<PhysReg, Vec<usize>>,
}

impl ClobberIndex {
    fn of(model: &MachineModel) -> Self {
        let mut by_reg: HashMap<PhysReg, Vec<usize>> = HashMap::new();
        for (pos, regs) in &model.clobbers {
            for &reg in regs {
                by_reg.entry(reg).or_default().push(*pos);
            }
        }
        for positions in by_reg.values_mut() {
            positions.sort_unstable();
            positions.dedup();
        }
        ClobberIndex { by_reg }
    }

    /// The first position in `[lo, hi]` at which `reg` is clobbered, not
    /// counting `hi` itself when `exempt_hi`.
    fn first_in(&self, reg: PhysReg, lo: usize, hi: usize, exempt_hi: bool) -> Option<usize> {
        let positions = self.by_reg.get(&reg)?;
        let at = positions.partition_point(|&p| p < lo);
        let p = *positions.get(at)?;
        // Positions are unique, so when `hi` itself is exempt the next one is
        // already past the range.
        if p > hi || (exempt_hi && p == hi) {
            None
        } else {
            Some(p)
        }
    }
}

// ── Allocation result ────────────────────────────────────────────────

/// Where a value is at some position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueLoc {
    /// In a physical register.
    Reg(PhysReg),
    /// In its home frame slot, as an 8-byte word index.
    ///
    /// **Whose numbering** is the [`Allocation`] this came out of: by default
    /// [`ls_color_homes`]', and the consumer's own after
    /// [`Allocation::adopt_home_colouring`]. It is NOT automatically
    /// `SlotPlan::node_color`'s, which this used to claim — see the R7 note on
    /// [`Allocation::stack_slot`].
    ///
    /// A backend building [`ValueLoc`] values itself (rather than reading them
    /// out of an allocation) is of course free to put whatever word index its
    /// own frame uses in here; `ir_lower` does, in two different currencies, and
    /// neither is this one.
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
    /// touches memory.
    ///
    /// # R7: this is **not** a drop-in replacement for `SlotPlan::node_color`
    ///
    /// It said it was — "same numbering, same Ref/Prim/pinned pool separation" —
    /// and the pool separation half is true. The *numbering* half is false, and
    /// false in a way that cannot be fixed by tuning [`ls_color_homes`], because
    /// the two colourings are answering different questions:
    ///
    /// * `ir_lower::plan_slots` gives **every** value a home word; the frame it
    ///   lays out has to hold every value, promoted or not, because that file's
    ///   register file is a write-through cache over the home.
    /// * [`ls_color_homes`] gives a home only to a value that spends some
    ///   position in memory. A value that lives its whole life in a register
    ///   consumes no colour — which is the frame-size win the allocator exists
    ///   to produce — and every colour after it therefore shifts down.
    ///
    /// So the two agree exactly when nothing was promoted for its whole range,
    /// and disagree by construction the moment anything was. A `debug_assert!`
    /// that they match, which is what `NOTES-regalloc.md` §4 proposed, would
    /// fire on every successful allocation.
    ///
    /// **What this means for a consumer:** a `u32` coming out of
    /// [`Allocation::location_at`] as [`ValueLoc::Slot`] is in *this* numbering
    /// and addresses this plan's frame, not the emitter's. A consumer with a
    /// frame of its own calls [`Allocation::adopt_home_colouring`] and gets an
    /// allocation whose `Slot` payloads name words its encoder can address; a
    /// consumer with no frame of its own uses this one as is. What it must not
    /// do is mix them, which is what the old contract invited.
    pub stack_slot: Vec<Option<u32>>,
    /// Distinct home words the plan needs. In the same numbering as
    /// [`Self::stack_slot`] — see the R7 note there.
    pub stack_slots: usize,
    /// Register → memory transitions **this plan calls for**.
    ///
    /// # R5: this is the PLANNED number and it does not feed the report
    ///
    /// `CompilationReport::spills` is fed by `metrics::note_current_spills`,
    /// which `ir_lower` calls with its own `ls_spills` — what it **emitted**.
    /// That is the smaller number: `plan_register_residency` executes only the
    /// subset of this plan it can prove, so the difference between the two is
    /// exactly the gap between what the allocator decided and what the backend
    /// was able to carry out.
    ///
    /// Nothing reports that gap today, and pointing both numbers at one
    /// `Measured<u32>` field would make the report mean whichever producer ran
    /// last. The fix is a second pair of report fields (`planned_spills` /
    /// `planned_reloads`) in `metrics.rs`; until then this number is for a
    /// caller that asks the allocation directly, and the producer that used to
    /// publish it to the shared field is gone — see the R5 note in the metrics
    /// section at the bottom of this file.
    pub spills: usize,
    /// Memory → register transitions this plan calls for. Planned, not emitted;
    /// see [`Self::spills`].
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
    ///
    /// Read by `ir_lower::ls_split_transitions` behind
    /// `CRATONVM_JIT_IR_LS_SPLITS`, which is what turns this list into machine
    /// code; by [`verify_allocation`], which proves it against the timeline;
    /// and by nothing else. A consumer that follows this list must also read
    /// [`Self::segments`], because there is no event for "the register stops
    /// being good": a [`SpillKind::Store`]'s `pos` is the last position at
    /// which it still IS good, and a rematerializable value produces no
    /// `Store` at all while still leaving its register.
    pub events: Vec<SpillEvent>,
    /// Copied from [`LiveModel::peak_live`] so a report has the pressure and
    /// the outcome in one place.
    pub peak_live: usize,
}

impl Allocation {
    /// Where is `node`'s value at `pos`? `None` when `pos` is outside its live
    /// range or the node produces no value.
    ///
    /// # A `Slot` answer assumes a write-through consumer for remat values
    ///
    /// A rematerializable value (`Const`, `ConstF`, `Param` — [`remat_of`])
    /// gets NO [`SpillKind::Store`] when it leaves a register, by design: its
    /// reload is a [`SpillKind::Remat`]. But a memory segment of such a value
    /// still answers `Slot(home)` here, and [`split_edge_resolution`] turns a
    /// memory delivery into a reload from that word. That word holds the value
    /// only because every consumer in the tree (`ir_lower`) writes every
    /// definition to its home. A consumer that follows [`Allocation::events`]
    /// literally and writes homes only at `Store` events must rematerialise
    /// such a value instead of loading `Slot` — the one place this plan is not
    /// self-sufficient (round 11 wave 2 review; see
    /// `docs/feature-designs/jit-ir-optimizations.md`).
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

    /// Does `node` ever occupy a register? Test-only (round 11 wave 4).
    #[cfg(test)]
    pub fn is_promoted(&self, node: NodeId) -> bool {
        self.segments
            .get(node as usize)
            .is_some_and(|segs| segs.iter().any(|s| s.reg.is_some()))
    }

    /// The register `node` holds over its first segment, if any. Test-only
    /// (round 11 wave 4).
    #[cfg(test)]
    pub fn first_reg(&self, node: NodeId) -> Option<PhysReg> {
        self.segments.get(node as usize)?.first()?.reg
    }

    /// Does `node` spend any position of its life in memory?
    ///
    /// Exactly the population that needs a home word, and therefore exactly the
    /// population [`Self::adopt_home_colouring`] checks a candidate layout
    /// against.
    pub fn needs_home(&self, node: NodeId) -> bool {
        self.segments
            .get(node as usize)
            .is_some_and(|segs| segs.iter().any(|s| s.reg.is_none()))
    }

    /// Replace this plan's home colouring with the consumer's authoritative one.
    ///
    /// # R7: the answer to "two frame colourings and nothing compares them"
    ///
    /// [`ls_color_homes`] and `ir_lower::plan_slots` both colour home words, and
    /// only the second one lays out a frame anybody addresses. They cannot be
    /// *compared* — see the R7 note on [`Self::stack_slot`] for why a
    /// `stack_slot[id] == node_color[id]` assertion is wrong rather than merely
    /// unimplemented — and [`ls_color_homes`] cannot be *deleted*, because an
    /// [`Allocation`] has to be able to answer [`Self::location_at`] for a
    /// consumer that has no frame of its own.
    ///
    /// So: adopt instead of compare. A consumer that lays out its own frame
    /// hands it over here, and from that point every [`ValueLoc::Slot`] this
    /// allocation produces names a word that consumer's encoder can address.
    /// Re-running [`verify_allocation`] afterwards then makes proof 6 — two
    /// values sharing a home word have disjoint ranges, and a word never mixes
    /// [`HomeClass`] pools — a statement about **the frame that is emitted**
    /// rather than about a parallel invention.
    ///
    /// # What is refused
    ///
    /// A layout with no word for a value that spends time in memory. That is
    /// not a divergence to record, it is a value with nowhere to be spilled to,
    /// and accepting it would make `location_at` return `None` at a position
    /// inside the value's own live range. `colours.len()` must also match the
    /// graph, for the same reason [`verify_allocation`] checks it.
    ///
    /// A value that needs NO home and is given a colour anyway is accepted
    /// without comment: that is the normal shape of a write-through consumer,
    /// which writes every definition to its home whether or not the allocator
    /// asked for a word there.
    pub fn adopt_home_colouring(
        &mut self,
        colours: &[Option<u32>],
        slots: usize,
    ) -> CompileResult<()> {
        if colours.len() != self.segments.len() {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: a home colouring is not sized for the graph"),
                format!(
                    "{} colours, {} timelines",
                    colours.len(),
                    self.segments.len()
                ),
            ));
        }
        for (id, &colour) in colours.iter().enumerate() {
            let node = id as NodeId;
            if !self.needs_home(node) {
                continue;
            }
            match colour {
                Some(c) if (c as usize) < slots => {}
                Some(c) => {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: a home word is outside the frame plan"),
                        format!("n{node} at word {c} of {slots}"),
                    ))
                }
                None => {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: a spilled value has no home word"),
                        format!("n{node} spends part of its life in memory"),
                    ))
                }
            }
        }
        self.stack_slot.clear();
        self.stack_slot.extend_from_slice(colours);
        self.stack_slots = slots;
        Ok(())
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
/// It moves the residency census deterministically (`resident=1` to `2`,
/// `split_or_spilled=3` to `2` on `probes/OsrTierBench.java`, saturating by
/// 64), and it changes register allocation on every IR compile. Its timing
/// benefit is still unmeasured: the arm that would settle it was attempted at
/// host load 22 on 8 cores and is not usable; see `JIT_OPTIMIZATION.md`. (This
/// paragraph used to open "Off by default", which stopped being true on
/// 2026-09-05 — the code below defaults to 64.)
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
/// Every re-queue strictly advances the re-queued interval's start, so the scan
/// terminates regardless — see the termination section on
/// [`allocate_linear_scan`], which names the guard that makes that true of the
/// eviction path. This budget is about *compile time*. A graph that needs more
/// splits than this over the given register file is one where every promotion
/// immediately costs a reload, i.e. one where linear scan buys nothing — so it
/// bails with [`BailoutReason::RegisterPressure`] rather than spending the
/// compile budget arriving at the frame layout `ir_lower` already had.
fn split_budget(intervals: usize) -> usize {
    intervals.saturating_mul(4).saturating_add(16)
}

/// How `node` can be recomputed, if it can.
///
/// `pub` because a backend consuming [`SpillKind::Remat`] needs it: the event
/// says "recompute this value into `to`" and this is the only thing that says
/// *how*.
///
/// **The consumer exists since R4** — `ir_lower::ls_split_transitions`, behind
/// `CRATONVM_JIT_IR_LS_SPLITS` — and it takes only the [`Remat::Const`] arm as
/// a true re-materialisation. [`Remat::ConstF`] would need a GP scratch for the
/// immediate-plus-`movq` sequence and [`Remat::Param`] is a second home word
/// for the same value; both lower there as a load from the value's own home
/// word instead, which is bit-identical because that backend writes every home
/// at the definition. The classification here is unchanged by that: what a
/// value CAN be recomputed from is a property of the graph, not of one
/// backend's scratch budget.
///
/// `x + c` is deliberately NOT here (j27 wave 4): it is recomputable only
/// while `x` is in a register, so the scan must still store it and score it
/// like any other value. Its classification is [`remat_add_of`], read by the
/// split consumer at a planned `Load`.
pub fn remat_of(graph: &Graph, node: NodeId) -> Option<Remat> {
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
/// ## Termination (R12)
///
/// The claim is the one this paragraph always made — **every re-queue strictly
/// advances the re-queued interval's start** — and it is true of all three
/// re-queue sites. What was missing is *why* it is true of the third one, which
/// is the only one where it is not obvious from the line that does the pushing:
///
/// * the **clobber split** pushes at `next_use_at_or_after(node, split_at + 1)`,
///   and `split_at ≥ lo`, so the new start is `> lo` by inspection;
/// * the **lost-contest** path pushes at `next_use_after(node, lo)`, which is
///   strictly after `lo` by that method's definition;
/// * the **eviction** path pushes the victim at
///   `next_use_at_or_after(victim.node, lo)` — `lo` being the *current*
///   interval's start, not the victim's — so nothing on this line bounds the
///   result by `victim.lo` at all. It advances because
///   [`ls_pick_victim`] refuses to nominate an active with `a.lo >= current.lo`,
///   which puts `victim.lo < lo ≤ u`.
///
/// That guard is therefore **load-bearing for termination**, not the local
/// tidiness its own comment makes it sound like ("evicting it would not advance
/// anything"), and removing it would give this loop a way to evict an interval
/// and re-queue it at its own unchanged start forever.
/// `ls_pick_victim_never_nominates_an_active_that_starts_here` pins it.
///
/// [`split_budget`] is a **compile-time** bound layered on top, not the
/// termination argument: it exists so that a graph needing thousands of splits
/// bails out instead of spending the compile budget arriving at the frame layout
/// `ir_lower` already had.
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
            // Outside the allocatable file: the emitter's fixed scratch. This is
            // the arm every production model takes today, and it is why
            // `for_graph`'s new fixed-operand rules change nothing for a backend
            // that keeps RAX/RCX/RDX for itself.
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
    // j27 wave 2 (lane RESIDENCY): read once per scan, see `loop_residency`.
    let loop_aware = loop_phi_residency_enabled();
    let clobbers = ClobberIndex::of(model);
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
        // interval overrides free choice. The EARLIEST such constraint wins:
        // `model.fixed` is sorted by position by both producers, so `min_by_key`
        // is only defensive against a hand-built model.
        //
        // A second constraint later in the same interval — a value used as
        // `idiv`'s dividend and then as a shift count, say — is not honoured by
        // this assignment and is not meant to be. It shows up instead as a
        // BLOCKING position inside `ls_select_register::first_block`, which cuts
        // the register segment before it, so the value is in its home word at
        // the second constraint's position and the emitter loads it into the
        // register that one names. The alternative — pretending one interval can
        // satisfy two different registers — is what proof 4b would reject.
        let required: Option<(usize, PhysReg)> = fixed_of.get(&node).and_then(|slots| {
            slots
                .iter()
                .filter(|(pos, _)| lo <= *pos && *pos <= hi)
                .min_by_key(|(pos, _)| *pos)
                .copied()
        });

        let mut attempts = 0usize;
        let choice = loop {
            attempts += 1;
            if attempts > model.regs.class_size(class).saturating_add(2) {
                break None;
            }
            if let Some(found) =
                ls_select_register(model, &clobbers, live, &active, class, required, current)?
            {
                break Some(found);
            }
            // With a required register, only ITS holder is worth evicting:
            // `ls_select_register` answered `None` because that one register is
            // busy (or blocked, which no eviction fixes), and cutting any other
            // active short frees a register the retry will not take. Every such
            // eviction used to be a split and a spill paid for nothing, up to
            // `class_size + 2` of them per interval.
            let only_reg = required.map(|(_, reg)| reg);
            match ls_pick_victim_with(
                graph,
                live,
                &active,
                class,
                current,
                only_reg,
                loop_aware,
            ) {
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
                // An OWN constraint this register does not satisfy blocks it
                // too, and for a sharper reason than somebody else's does.
                //
                // R6 demotes an unhonourable constraint instead of bailing, on
                // the ground that `FixedConstraint` promises only that nobody
                // else is in `reg` there AND that this value is not somewhere
                // else in a register -- both true of a value sitting in its
                // home word. The second half is the one that has to be MADE
                // true: if the value keeps a different register across `pos`,
                // the allocation reports it as resident in a register the
                // instruction cannot use, and an emitter that trusts that
                // reads the wrong operand rather than loading the fixed one
                // from the home word.
                //
                // So an own constraint at `pos` that this register does not
                // satisfy is treated exactly like a clobber at `pos`: the
                // segment stops one short, the value is home across `pos`, and
                // the suffix resumes at the next use. That reuses the split
                // bookkeeping below rather than inventing a second way to end
                // a segment.
                // Bounded by `hi` as well as `lo`. This interval can be a
                // re-queued SUFFIX whose `hi` is short of the value's range end
                // (an evicted victim is re-queued up to the end of the segment
                // it lost, and a later suffix is queued separately). An own
                // constraint beyond `hi` belongs to that later suffix; letting
                // it set `until` here made `end = until - 1` run PAST `hi`, so
                // the segment escaped this interval and overlapped the suffix —
                // an `Internal` error out of `ls_finish` instead of an
                // allocation.
                let own_unhonoured = model
                    .fixed
                    .iter()
                    .filter(|f| f.node == node && f.reg != reg && f.pos >= lo && f.pos <= hi)
                    .map(|f| f.pos)
                    .min();
                let until = match (until, own_unhonoured) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
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
                // A required register that could not be honoured used to be a
                // hard `RegisterPressure` bailout here, on the reading that "the
                // value must be in that register at that position and it is
                // not".
                //
                // **R6 corrected that.** A [`FixedConstraint`] does not promise
                // the value is in a register at `pos` — see its doc comment. It
                // promises that nobody ELSE is in `reg` there and that this
                // value is not somewhere else *in a register*, both of which
                // still hold when the value simply sits in its home word: proof
                // 4b's first arm only fires on `ValueLoc::Reg`. An emitter has
                // to be able to load a fixed operand out of a frame slot in any
                // case, because a pinned or unpromoted value never had a
                // register to begin with.
                //
                // With `for_graph` now emitting a constraint per integer
                // `Div`/`Rem` and per variable shift, keeping the bailout would
                // have turned every register-pressured method containing a
                // division into a refused compile — trading a `mov` for the
                // whole optimized body. The value keeps its home word, which is
                // exactly today's behaviour for it.
                let _ = required;
            }
        }
    }

    // The allocation is checked before it is returned, so no caller can use an
    // unverified one: `verify_allocation` is not an opt-in debug pass.
    let alloc = ls_finish(graph, live, model, reg_segments, splits)?;
    verify_allocation(graph, live, model, &alloc)?;
    Ok(alloc)
}

/// Pick a free register for `current`, or `None` when nothing is usable.
///
/// Returns `(register, split_position)`: `split_position` is `Some(p)` when the
/// register is only usable up to `p - 1` because a clobber or a fixed constraint
/// claims it at `p`.
///
/// `required` is `(position, register)` — the earliest [`FixedConstraint`] on
/// `current.node` that falls inside `current`'s range, if any.
#[allow(clippy::type_complexity)]
fn ls_select_register(
    model: &MachineModel,
    clobbers: &ClobberIndex,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    required: Option<(usize, PhysReg)>,
    current: Pending,
) -> CompileResult<Option<(PhysReg, Option<usize>)>> {
    let range = PosRange {
        lo: current.lo,
        hi: current.hi,
    };
    // Compile time (round 11 wave 4): this runs once per interval per attempt
    // and asks every register of the class. `active` holds at most one entry
    // per register, so a linear probe replaces a per-call `HashSet` build, and
    // the fixed constraints inside `range` — the only ones `first_block` can
    // act on — are filtered once here instead of rescanned for each register.
    // Same answers: both are pure re-expressions of the old filters.
    let busy = |reg: PhysReg| active.iter().any(|a| a.reg == reg);
    let fixed_in_range: Vec<&FixedConstraint> = model
        .fixed
        .iter()
        .filter(|f| range.contains(f.pos))
        .collect();

    // The first position in `range` at which `reg` is unavailable to
    // `current.node`: a clobber, a fixed constraint belonging to someone else,
    // or — since R6 — a fixed constraint belonging to `current.node` itself that
    // names a DIFFERENT register. Clobbers are found by one binary search in
    // `reg`'s own position list (`ClobberIndex`, round 11 wave 9 — it used to
    // walk every clobber site in `range`, which for a callee-saved register
    // visited every call the interval spans and found nothing), fixed
    // constraints by a scan of the (small) in-range fixed list.
    //
    // That third case is the one R6 added, and it is what keeps a value with two
    // fixed operand roles honest. Without it, a value pinned to RAX at position
    // 10 and to RCX at position 20 would be given RAX for [lo, hi] ⊇ {20} and
    // `verify_allocation`'s proof 4b would reject the result as an internal
    // error — costing the method its optimized body over a shape the allocator
    // can simply split. With it, the segment ends at 19, the value is in its
    // home word at 20, and the emitter loads RCX from there.
    //
    // The index answers "the first clobber of `reg` at or after `lo`", which
    // is the earliest one in `range` because the list is canonically unique
    // and sorted (`MachineModel::canonicalize_clobbers`; `verify_allocation`
    // refuses a model that is not).
    //
    // One clobber is not a block: the one at the value's LAST USE, when the
    // consumer has declared that position reads its operands before it
    // destroys anything (`MachineModel::reads_precede_clobber`, empty unless a
    // consumer opts in). The value is read and dies there, so holding the
    // register through it is the early-clobber shape again, and proof 4
    // exempts it under the same three conditions.
    let last_use_exempt = model.read_before_clobber(live, current.node, range.hi);
    let first_block = |reg: PhysReg| -> Option<usize> {
        let mut earliest: Option<usize> =
            clobbers.first_in(reg, range.lo, range.hi, last_use_exempt);
        for f in fixed_in_range.iter() {
            let blocks = if f.node == current.node {
                f.reg != reg
            } else {
                f.reg == reg
            };
            if blocks {
                earliest = Some(earliest.map_or(f.pos, |e: usize| e.min(f.pos)));
            }
        }
        earliest
    };

    if let Some((req_pos, reg)) = required {
        if busy(reg) {
            return Ok(None);
        }
        return match first_block(reg) {
            // Nothing in the way at all.
            None => Ok(Some((reg, None))),
            // ── Early clobber ────────────────────────────────────────
            //
            // A block landing exactly ON the constraint's own position is the
            // `idiv` shape: RAX carries the dividend IN and the quotient OUT, so
            // the register is both required at `req_pos` and destroyed at
            // `req_pos`. Those are consistent — the read happens before the
            // write inside one instruction — and the endpoint-inclusive overlap
            // rule this file uses says exactly that: a segment ending at
            // `req_pos` does not survive `req_pos`, which is all "clobbered at
            // `req_pos`" forbids.
            //
            // So the value keeps `reg` through `req_pos` and gives it up at
            // `req_pos + 1`. `verify_allocation`'s proof 4 exempts this one
            // segment, and only this one: it must end at the constrained
            // position and the constraint must name this exact node and
            // register.
            //
            // The block at `req_pos` can only be a clobber. A FOREIGN fixed
            // constraint at `(req_pos, reg)` was already rejected by
            // `allocate_linear_scan`'s duplicate scan ("n and n' both require
            // r"), and one of `current.node`'s OWN at `req_pos` naming a
            // different register was rejected by the same scan ("n requires both
            // r and r'"). `clobbered_at` is asked anyway rather than assumed,
            // because this is the one place where being wrong produces a value
            // living in a register a call destroyed; a model built by hand that
            // slips past the duplicate scan falls through to `Ok(None)` and
            // costs the method its promotion instead.
            Some(p) if p == req_pos => {
                if !model.clobbered_at(req_pos).contains(&reg) {
                    return Ok(None);
                }
                if req_pos >= range.hi {
                    Ok(Some((reg, None)))
                } else {
                    Ok(Some((reg, Some(req_pos + 1))))
                }
            }
            // A block strictly BEFORE the constraint: this interval cannot carry
            // the value as far as the position that needs it, so there is no
            // assignment to make. The caller evicts or leaves the value in
            // memory; either way the emitter loads `reg` from the home word.
            Some(p) if p < req_pos => Ok(None),
            // A block strictly after: usable through the constraint, split there.
            Some(p) => Ok(Some((reg, Some(p)))),
        };
    }

    let mut fallback: Option<(PhysReg, usize)> = None;
    // A register nothing blocks over the whole interval, in preference order —
    // except that a CALLER-saved one wins over a callee-saved one. An interval
    // with no blocking position in a caller-saved register crosses no call, so
    // it does not need a register that survives one; handing it the
    // callee-saved register anyway (because the file lists those first, as
    // `RegFile::x86_64` and `RegFile::aarch64` do) leaves the next interval that
    // DOES cross a call to split, and makes the prologue save a register a
    // volatile one would have served.
    //
    // A no-op for `ir_lower`'s files as they stand: its GP file is all
    // callee-saved, and its XMM file lists the volatile registers first on both
    // ABIs, so "first unblocked" and "first unblocked caller-saved, else first
    // unblocked" are the same register there.
    let mut free_callee_saved: Option<PhysReg> = None;
    for spec in model.regs.specs().iter().filter(|s| s.reg.class == class) {
        let reg = spec.reg;
        if busy(reg) {
            continue;
        }
        match first_block(reg) {
            None if spec.caller_saved => return Ok(Some((reg, None))),
            None => {
                if free_callee_saved.is_none() {
                    free_callee_saved = Some(reg);
                }
            }
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
    if let Some(reg) = free_callee_saved {
        return Ok(Some((reg, None)));
    }
    Ok(fallback.map(|(reg, p)| (reg, Some(p))))
}

/// Choose what loses its register when nothing is free.
///
/// Maximises `next_use_distance × SPILL_DISTANCE_SCALE / frequency_weight`, so
/// a value whose next use is far away and whose uses are cold goes first, and a
/// value used on the next instruction inside a nested loop goes last.
/// Rematerializable values score [`SPILL_SCORE_REMAT`] and always go first:
/// evicting one costs no store at all.
///
/// `only_reg`, when set, restricts the candidates to the active holding that
/// register — the caller passes the register `current` is REQUIRED to take,
/// because evicting anything else cannot make room for it. With no such
/// holder (the register is blocked rather than busy) the answer is
/// [`Victim::Current`].
///
/// Test-only since j27 wave 2: the scan calls [`ls_pick_victim_with`] with the
/// switch it read once; this is the switch-off form the fixtures pin.
#[cfg(test)]
fn ls_pick_victim(
    graph: &Graph,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    current: Pending,
    only_reg: Option<PhysReg>,
) -> Victim {
    ls_pick_victim_with(graph, live, active, class, current, only_reg, false)
}

/// The eviction score of `node` asked at `from` (see [`ls_pick_victim_with`]).
/// `loop_aware` is `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`: a loop invariant then
/// keeps no carry relief and a per-trip value's weight counts its definition
/// (`loop_residency`); off, the pre-j27 score exactly.
fn ls_spill_score(
    graph: &Graph,
    live: &LiveModel,
    node: NodeId,
    from: usize,
    end: usize,
    loop_aware: bool,
) -> u64 {
    if remat_of(graph, node).is_some() {
        return SPILL_SCORE_REMAT;
    }
    let next = live.next_use_at_or_after(node, from).unwrap_or(end);
    let mut dist = next.saturating_sub(from) as u64;
    // See `ls_carry_relief`: a carried value's distance is measured in the
    // wrong units for the cost it represents. Only one READ in the loop
    // pays per iteration (`LiveModel::carried_and_read`, round 11 wave 5).
    if loop_residency::keeps_carry_relief(live, node as usize, loop_aware) {
        let relief = ls_carry_relief();
        if relief > 1 {
            dist /= relief;
        }
    }
    let w = ranking_weight(live, node as usize, loop_aware).max(1);
    dist.saturating_mul(SPILL_DISTANCE_SCALE) / w
}

/// Choose what loses its register when nothing is free -- the rule documented
/// on `ls_pick_victim` (the test-only switch-off form), with the loop switch
/// passed in (read once per scan by
/// [`allocate_linear_scan`]). Under it a carried phi redefined every trip
/// loses every tie on score: `evictable`, right after the score, is 0 for it
/// (`loop_residency::needed_every_trip` says why it is a key of its own and
/// not a zero distance; j27 wave 4, lane RESIDFLIP). Off, `evictable` is 1 for
/// every candidate and the order is the pre-j27 one. j27 wave 5 (lane
/// UNROLLRES): on, it is `loop_residency::eviction_rank` -- a remainder loop's
/// phi ranks between a main loop's phi and every other value.
fn ls_pick_victim_with(
    graph: &Graph,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    current: Pending,
    only_reg: Option<PhysReg>,
    loop_aware: bool,
) -> Victim {
    let score = |node: NodeId, from: usize, end: usize| -> u64 {
        ls_spill_score(graph, live, node, from, end, loop_aware)
    };
    let evictable = |node: NodeId| -> u8 {
        loop_residency::eviction_rank(graph, live, node as usize, loop_aware)
    };

    // The current interval's own next use is strictly after its start: it is
    // being defined (or reloaded) at `lo`, so a use *at* `lo` cannot be served
    // from memory.
    let self_next = live
        .next_use_after(current.node, current.lo)
        .unwrap_or(current.hi);
    let mut best_key = (
        score(current.node, current.lo, current.hi),
        evictable(current.node),
        self_next.saturating_sub(current.lo),
        0u8,
        u32::MAX - current.node,
    );
    let mut best = Victim::Current;

    for (idx, a) in active.iter().enumerate() {
        if a.reg.class != class {
            continue;
        }
        if only_reg.is_some_and(|r| r != a.reg) {
            continue;
        }
        // An active that starts at the same position cannot be cut short —
        // there is no prefix to keep — and evicting it would not advance
        // anything.
        //
        // **This is the termination argument for the eviction path, not a
        // tidiness rule.** The caller re-queues an evicted victim at
        // `next_use_at_or_after(victim.node, current.lo)`, a position bounded
        // below by `current.lo` and by nothing else; only `victim.lo <
        // current.lo`, which is what this line establishes, makes that a strict
        // advance on the victim's own start. Drop it and the scan can evict one
        // interval and re-queue it unchanged, for ever, with only
        // `split_budget` between that and a hung compile. See the termination
        // section on `allocate_linear_scan` (R12).
        if a.lo >= current.lo {
            continue;
        }
        let next = live
            .next_use_at_or_after(a.node, current.lo)
            .unwrap_or(a.hi);
        let key = (
            score(a.node, current.lo, a.hi),
            evictable(a.node),
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
    model: &MachineModel,
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
        // Where the value is at each position, for the early-clobber store
        // below.
        let reg_at = |p: usize| -> Option<Option<PhysReg>> {
            timeline.iter().find(|s| s.range.contains(p)).map(|s| s.reg)
        };
        // SSA: the home is written once and stays correct, so a value never
        // needs a second store.
        //
        // …and a value DEFINED into its home needs none at all (round 11 wave
        // 2): when the timeline is in memory at the definition's own position,
        // the defining instruction writes the home, and a later register →
        // memory transition finds it already correct. This used to start
        // `false`, so a `memory → register → memory` timeline planned a Store
        // nothing needs and inflated `Allocation::spills`. Keyed on the
        // DEFINITION, not on the first segment: a range widened to block spans
        // can begin in memory before the definition, and then lives in a
        // register when it is defined.
        let mut stored = live
            .pos_of
            .get(id)
            .copied()
            .flatten()
            .and_then(reg_at)
            .is_some_and(|r| r.is_none());
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
                        // An EARLY-CLOBBER segment (round 11 wave 2) ends AT
                        // the fixed-operand instruction that destroys `a` —
                        // `idiv` reads its dividend in RAX and writes the
                        // quotient there. The register holds the value going
                        // INTO that instruction, not out of it, so a store at
                        // `prev.range.hi` would save the quotient. It belongs
                        // one position earlier, from whatever register holds
                        // the value there; if the value is already in memory
                        // there, its home is valid and nothing is stored.
                        let early_clobber = model.fixed.iter().any(|f| {
                            f.node == node
                                && f.reg == a
                                && f.pos == prev.range.hi
                                && model.regs.contains(f.reg)
                        }) && model.clobbered_at(prev.range.hi).contains(&a);
                        let store = if early_clobber {
                            prev.range
                                .hi
                                .checked_sub(1)
                                .and_then(|p| reg_at(p).flatten().map(|r| (p, r)))
                        } else {
                            Some((prev.range.hi, a))
                        };
                        if let Some((pos, from)) = store {
                            spills += 1;
                            events.push(SpillEvent {
                                node,
                                pos,
                                kind: SpillKind::Store,
                                from: Some(from),
                                to: None,
                            });
                        }
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
///
/// # R7: why this is not the frame anybody emits, and why it stays anyway
///
/// `ir_lower::plan_slots` lays out the frame the encoder addresses, and it
/// colours a strictly larger value set (every value, not only the ones that
/// touch memory), so the two numberings diverge the moment anything is promoted
/// for its whole range. That is by design on both sides and is spelled out at
/// [`Allocation::stack_slot`].
///
/// The exit recorded for R7 is neither "assert they agree" (they must not) nor
/// "delete this" — an [`Allocation`] has to answer [`Allocation::location_at`]
/// for a consumer that has no frame of its own, and proof 6 already validates a
/// frame that IS emitted, because `ir_lower::mir_alloc_verdict` builds an
/// `Allocation` around `plan.node_color` and runs [`verify_allocation`] over it.
/// The exit is [`Allocation::adopt_home_colouring`]: a consumer with a frame
/// hands it over, and proof 6 then speaks about that frame for the linear-scan
/// path too.
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
    //
    // The same four ops `ir_lower::plan_slots` treats this way (cov-01): the
    // two `ldc` constants and `getstatic` produce a `Ref` from a helper call
    // too, and publish the map before it. This list named `Op::Call` alone,
    // so for those three the two colourings disagreed about exactly the rule
    // that keeps a word the map already named from being recycled — invisible
    // only because `ir_lower` adopts `plan_slots`' layout
    // (`Allocation::adopt_home_colouring`), and wrong for any consumer that
    // uses this one as is. Pinned by `a_helper_produced_reference_never_recycles_a_home_word`.
    // j27 wave 2 (lane GCMAP): ONE predicate for both colourings, so the list
    // cannot drift again (it is `ir_lower`'s, which `plan_slots_from_core` and
    // `recolour_slot_plan` already share).
    let fresh_only = |id: usize| -> bool {
        graph
            .nodes
            .get(id)
            .is_some_and(crate::ir_lower::colour_is_fresh_only)
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
///    reader looks today.) The clobber half has exactly one exemption, the
///    early-clobber shape `idiv` needs — see [`FixedConstraint`] and the comment
///    at the check.
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
    // The clobber list is read through two access patterns — `clobbered_at`
    // binary-searches it, and `ls_select_register::first_block` and proof 4
    // below read its per-register transpose (`ClobberIndex`, round 11 wave 9;
    // both used to forward-scan it from a `partition_point`) — and only the
    // transpose tolerates two entries at one position. The
    // binary search returns *an* arbitrary match among equals, so a duplicate
    // position silently hides whichever entry it did not land on: a value then
    // holds a caller-saved register across a call that destroys it.
    //
    // Hence STRICTLY increasing, not merely sorted. Checking only `sorted`
    // (`>` rather than `>=`) is what let the invariant that
    // `MachineModel::for_graph`'s own doc comment asks second producers to
    // maintain go unenforced; `canonicalize_clobbers` now establishes it and
    // this rejects any model that lost it. See R2/R3.
    if model.clobbers.windows(2).any(|w| w[0].0 >= w[1].0) {
        return Err(Bailout::with_context(
            BailoutReason::Internal(
                "regalloc: the clobber list is not strictly increasing by position",
            ),
            format!("{} clobber sites", model.clobbers.len()),
        ));
    }
    let clobbers = ClobberIndex::of(model);

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
            //
            // ── The one exemption: early clobber ─────────────────────
            //
            // `idiv` reads its dividend in RAX and writes its quotient there, so
            // the model carries both a [`FixedConstraint`] on the dividend at
            // the divide's position and a clobber of RAX at that same position.
            // A segment that ENDS at that position is legal: under this file's
            // endpoint-inclusive overlap rule a segment `[lo, p]` does not
            // survive `p`, and "clobbered at `p`" forbids nothing else.
            //
            // The exemption is deliberately the narrowest statement that covers
            // the case. All four must hold — this exact node, this exact
            // register, this exact position, and the segment must end there — so
            // it cannot be reached by a value that merely happens to be live
            // across a call that also clobbers a register it is pinned to
            // somewhere else in its life.
            //
            // The second disjunct is the same shape without a constraint: a
            // position the consumer declared reads its operands before it
            // clobbers (`MachineModel::reads_precede_clobber`, empty by
            // default), which is this value's last use and the end of its live
            // range. The segment must still END there — `seg.range.hi` is what
            // is tested, so a segment running past the call is still rejected.
            let early_clobber = model.fixed.iter().any(|f| {
                f.node == node
                    && f.reg == reg
                    && f.pos == seg.range.hi
                    && model.regs.contains(f.reg)
            }) || model.read_before_clobber(live, node, seg.range.hi);
            // Round 11 wave 9: one binary search in `reg`'s clobber positions
            // (the list's invariant is checked above), not a walk over every
            // clobber site the segment spans.
            if let Some(pos) = clobbers.first_in(reg, seg.range.lo, seg.range.hi, early_clobber) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value is live in a clobbered register"),
                    format!(
                        "n{node} holds {reg} over [{}, {}], clobbered at {pos}",
                        seg.range.lo, seg.range.hi
                    ),
                ));
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
/// free at an edge. For `ir_lower::emit_copy_op` it is a reserved FRAME WORD
/// (`phi_copy_scratch_slot_off`), not a register — RAX is only the staging
/// temporary every memory-to-memory move passes through, so a backend that
/// parked a `Save` in RAX would lose it on the next `Move`. (Round 11: this
/// said "RAX", which is the one choice that does not work.) Naming a location
/// here would make the sequence wrong on any backend with a different scratch.
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
    // Round 11 wave 8: the two scans below used to be linear searches of
    // `pending` — a duplicate-destination search per input pair and a
    // "does anyone still read this destination" search per candidate per
    // pass — which made one edge O(n³) in its copy count. A merge's copy
    // count is its φ count, which a 20 000-node graph can put in the
    // hundreds on every edge of a switch join. Two maps made it O(n²) (the
    // lowest-index ready scan and `Vec::remove` stayed until wave 9, below),
    // and the emitted sequence is unchanged: the same entry is chosen at
    // every step.
    //
    // `src_of[dst]` = the source already recorded for `dst`.
    let mut src_of: HashMap<ValueLoc, ValueLoc> = HashMap::with_capacity(copies.len());
    for &(dst, src) in copies {
        if dst == src {
            // A copy to itself is not a move, and treating it as one would
            // invent a false dependency that forces a scratch.
            continue;
        }
        match src_of.get(&dst).copied() {
            // The same pair listed twice is idempotent — a merge reached along
            // two edges of the same predecessor asks for the identical write.
            Some(s) if s == src => continue,
            Some(_) => {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: a parallel copy writes one destination twice",
                    ),
                    format!("{dst:?}"),
                ));
            }
            None => {
                src_of.insert(dst, src);
                pending.push((dst, Some(src)));
            }
        }
    }
    // `readers[loc]` = how many pending entries still read `loc` (a `Some`
    // source). A destination is ready exactly when its count is zero.
    let mut readers: HashMap<ValueLoc, usize> = HashMap::with_capacity(pending.len());
    for &(_, src) in &pending {
        if let Some(s) = src {
            *readers.entry(s).or_insert(0) += 1;
        }
    }

    // Round 11 wave 9 (irback proposal W8-2): the step that used to rescan
    // `pending` for the lowest-index ready entry and then `Vec::remove` it is
    // now a min-heap of ready indices, so one edge is O(n log n). A MIN-heap
    // rather than a FIFO keeps the emitted sequence the one the rescanning
    // version chose, op for op (`ir_lower`'s φ-edge bytes do not move): an
    // entry enters the heap exactly when its destination's reader count
    // reaches zero (counts only fall), so the heap is always the ready set,
    // and its minimum is the lowest-index ready entry. The cycle breaker
    // takes the lowest-index live entry, as `pending.first()` did.
    //
    // `dst_index[loc]` = the entry writing `loc`; `reads[loc]` = the entries
    // whose source is `loc`.
    let n = pending.len();
    let mut dst_index: HashMap<ValueLoc, usize> = HashMap::with_capacity(n);
    let mut reads: HashMap<ValueLoc, Vec<usize>> = HashMap::with_capacity(n);
    for (i, &(dst, src)) in pending.iter().enumerate() {
        dst_index.insert(dst, i);
        if let Some(s) = src {
            reads.entry(s).or_default().push(i);
        }
    }
    let mut done = vec![false; n];
    let mut queued = vec![false; n];
    let mut ready: BinaryHeap<Reverse<usize>> = BinaryHeap::new();
    for (i, &(dst, _)) in pending.iter().enumerate() {
        if readers.get(&dst).copied().unwrap_or(0) == 0 {
            queued[i] = true;
            ready.push(Reverse(i));
        }
    }
    let mut first_live = 0usize;
    let mut left = n;

    let mut out: Vec<CopyOp> = Vec::with_capacity(n + 2);
    // Each pass either retires an entry or breaks one cycle (which immediately
    // makes an entry retirable), so twice the entry count plus a constant
    // bounds the loop even if the invariant above were ever violated.
    let mut guard = n.saturating_mul(2).saturating_add(4);
    while left > 0 {
        if guard == 0 {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: parallel copy sequencing did not terminate"),
                format!("{left} copies left"),
            ));
        }
        guard -= 1;

        // A destination nothing still reads can be written now.
        match ready.pop() {
            Some(Reverse(i)) => {
                let Some(&(dst, src)) = pending.get(i) else {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: parallel copy lost its state"),
                        format!("ready index {i} of {n}"),
                    ));
                };
                if done[i] {
                    continue;
                }
                done[i] = true;
                left -= 1;
                out.push(match src {
                    Some(from) => {
                        if let Some(r) = readers.get_mut(&from) {
                            *r = r.saturating_sub(1);
                            if *r == 0 {
                                if let Some(&j) = dst_index.get(&from) {
                                    if !done[j] && !queued[j] {
                                        queued[j] = true;
                                        ready.push(Reverse(j));
                                    }
                                }
                            }
                        }
                        CopyOp::Move { from, to: dst }
                    }
                    None => CopyOp::Restore { to: dst },
                });
            }
            None => {
                // Every remaining destination is also a source. Each has
                // exactly one source (destinations are unique) and at least one
                // reader, so what is left is a disjoint union of pure cycles.
                // Park one element and its cycle unwinds.
                while first_live < n && done[first_live] {
                    first_live += 1;
                }
                let Some(&(cycle, _)) = pending.get(first_live) else {
                    // Unreachable: `left > 0` means some entry is not done.
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: parallel copy lost its state"),
                        format!("{left} copies left, none live"),
                    ));
                };
                out.push(CopyOp::Save { from: cycle });
                if let Some(readers_of_cycle) = reads.get(&cycle) {
                    for &j in readers_of_cycle {
                        if !done[j] {
                            if let Some(entry) = pending.get_mut(j) {
                                if entry.1 == Some(cycle) {
                                    entry.1 = None;
                                }
                            }
                        }
                    }
                }
                readers.insert(cycle, 0);
                if !queued[first_live] {
                    queued[first_live] = true;
                    ready.push(Reverse(first_live));
                }
            }
        }
    }
    Ok(out)
}

// `phi_edge_copies` — the per-edge phi parallel copy derived from an
// `Allocation` — was deleted in round 11 wave 2. It had no production caller
// (`ir_lower::gather_phi_copies` + `emit_copy_op` are the live path, and are
// tested there), its doc said a phi is never promoted (false since
// `LiveModel::release_phi_pins`, where it failed with "a phi has no home
// word"), and it rescanned every node per predecessor block.

// ── CFG-edge consistency of a split timeline ─────────────────────────

/// A layout-order emitter's compile-time belief about one value at one linear
/// position, as [`split_edge_conflicts`] and [`split_edge_resolution`] model it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LsLinearState {
    /// Outside the value's live range.
    Dead,
    /// In its home word — or not yet defined in LAYOUT order, which a
    /// write-through emitter treats the same way (it has published nothing).
    Memory,
    /// In this register.
    Reg(PhysReg),
}

/// What a layout-order emitter believes about `id` at `pos`.
///
/// The timeline, except that a position laid out BEFORE the value's own
/// definition is [`LsLinearState::Memory`]: liveness is widened to whole block
/// spans, so a loop-carried value's range (and its first register segment) can
/// start before the node that defines it, and an emitter walking the blocks in
/// layout order has published nothing there. `ir_lower::ls_split_transitions`
/// refuses every transition before the definition for the same reason. Reading
/// "the register the timeline names" there would be reading a register the
/// emitter never filled on that path.
fn ls_linear_state(live: &LiveModel, segs: &[Segment], id: usize, pos: usize) -> LsLinearState {
    let Some(seg) = segs.iter().find(|s| s.range.contains(pos)) else {
        return LsLinearState::Dead;
    };
    if live
        .pos_of
        .get(id)
        .copied()
        .flatten()
        .is_some_and(|def| pos < def)
    {
        return LsLinearState::Memory;
    }
    match seg.reg {
        Some(reg) => LsLinearState::Reg(reg),
        None => LsLinearState::Memory,
    }
}

/// One value whose location on a CFG edge does not match what the code at the
/// edge's target was emitted against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LsEdgeMismatch {
    pred: usize,
    succ: usize,
    node: usize,
    /// The register the target block's code reads the value from.
    want: PhysReg,
    /// Where the edge actually delivers it: `Reg`, `Memory`, or `Dead` — the
    /// last only when the model says the value IS live-in at the target and
    /// the timeline has no location for it at the predecessor's edge, which
    /// no well-formed allocation produces and which no copy can repair.
    have: LsLinearState,
}

/// Every [`LsEdgeMismatch`] of every multi-segment value, in `(pred, succ)`
/// edge order. The one enumeration both [`split_edge_conflicts`] and
/// [`split_edge_resolution`] read, so the value set the gate refuses and the
/// value set the resolver repairs cannot drift apart.
fn ls_edge_mismatches(
    schedule: &Schedule,
    live: &LiveModel,
    alloc: &Allocation,
) -> Vec<LsEdgeMismatch> {
    let mut out: Vec<LsEdgeMismatch> = Vec::new();
    // Only a value with more than one segment can change location, and that is
    // a handful per method: gather them once rather than scanning every node
    // on every edge.
    let multi: Vec<usize> = alloc
        .segments
        .iter()
        .enumerate()
        .filter(|(_, segs)| segs.len() > 1)
        .map(|(id, _)| id)
        .collect();
    if multi.is_empty() {
        return out;
    }
    for (p, block) in schedule.blocks.iter().enumerate() {
        let Some(&(_, p_edge)) = live.span.get(p) else {
            continue;
        };
        for &s in &block.successors {
            let Some(&(s_start, _)) = live.span.get(s) else {
                continue;
            };
            // Position 0 has no linear predecessor (the emitter enters it with
            // nothing resident, which every edge satisfies); and the layout
            // fall-through is the one edge a linear emitter follows exactly.
            if s_start == 0 || p_edge.saturating_add(1) == s_start {
                continue;
            }
            let carried = s_start - 1;
            for &id in &multi {
                let segs = alloc.segments[id].as_slice();
                // A register claim at the top of `s` is the only belief an
                // edge can contradict: memory is always right for a
                // write-through emitter, and so is "not resident".
                let LsLinearState::Reg(want) = ls_linear_state(live, segs, id, carried) else {
                    continue;
                };
                let live_in = live.live_in_at(s, id as NodeId);
                if live_in == Some(false) {
                    // Nothing on any path from `s` reads it before it is dead:
                    // a stale belief about its register is never acted on.
                    continue;
                }
                let have = ls_linear_state(live, segs, id, p_edge);
                match have {
                    LsLinearState::Reg(r) if r == want => {}
                    // Not live on this edge by the range, and the model has no
                    // better answer: a value live-out of `p` has `span[p].1`
                    // inside its range by the construction of
                    // `build_live_model`, so nothing on this path reads it.
                    LsLinearState::Dead if live_in.is_none() => {}
                    _ => out.push(LsEdgeMismatch {
                        pred: p,
                        succ: s,
                        node: id,
                        want,
                        have,
                    }),
                }
            }
        }
    }
    out
}

/// Which values a consumer that follows the allocation in **linear** order
/// would get wrong on some CFG edge. `result[id]` is `true` for such a value.
///
/// # The gap this names
///
/// An [`Allocation`] is a timeline over LINEAR positions — block layout order —
/// and [`allocate_linear_scan`] performs no edge resolution during the scan:
/// when it splits a value, the location it records at the start of a block is
/// simply whatever the previous position in layout order had. That is the
/// location on the fall-through edge only. On any other incoming edge the value
/// may be somewhere else:
///
/// ```text
///   header (pos 3..)      v in r0          <- timeline carried in from the preheader
///   body   (pos 6)        v evicted; r0 handed to w
///   latch  (edge 9)       v in memory      -> back edge to the header
/// ```
///
/// The header's code reads `v` from `r0` on every iteration, and from the
/// second iteration on `r0` holds `w`. The same shape arises at any join whose
/// predecessors are not all laid out immediately before it.
///
/// A consumer that emits ONE register for a value's whole range (every
/// production path by default) never sees this: a single segment has one
/// location everywhere. A consumer that follows the per-position timeline —
/// `ir_lower`'s split residency, `CRATONVM_JIT_IR_LS_SPLITS`, default off — does,
/// because it carries its residency state across block boundaries in layout
/// order too.
///
/// # The rule
///
/// For every edge `p -> s` that is NOT the layout fall-through into `s`
/// (`span[p].1 + 1 != span[s].0`), and every value with more than one segment:
/// if the emitter believes the value is in register `r` at `span[s].0 - 1` —
/// the position whose state a linear emitter carries into `s` — then it must be
/// in `r` at `p`'s edge position too. Being in memory at `s` is always
/// consistent for a write-through consumer (the home word is written at the
/// definition); being in a register there is consistent only if every
/// predecessor put it there.
///
/// "Believes" is the timeline with one correction (round 9 wave 2): at a
/// position laid out before the value's own definition the emitter has
/// published nothing, so it believes "memory" there whatever the timeline's
/// register says — see `ls_linear_state`. That makes a predecessor laid out
/// before the definition deliver the value in memory (flagged if the target
/// claims a register; the wave-1 rule compared raw timelines and passed it),
/// and a target laid out before the definition claim nothing (never flagged).
///
/// A value the model knows is not live-in at `s` ([`LiveModel::live_in_at`])
/// is skipped: nothing on a path from `s` reads it. When liveness is unknown,
/// a value whose range does not cover `p`'s edge position is skipped instead
/// (a value live-out of `p` has `span[p].1` inside its range by the
/// construction of [`build_live_model`]).
///
/// Conservative in one direction only: a value this returns `false` for is
/// consistent on every edge; one it returns `true` for may still have been
/// safe (a register claim immediately dropped at the block's first position,
/// say). The cost of that imprecision is one declined promotion.
///
/// # The fix
///
/// [`split_edge_resolution`] computes, for exactly the mismatches this flags
/// (they share one enumeration), the per-edge parallel copy that repairs them.
/// A consumer that emits those copies may keep the values this flags; one that
/// does not must refuse them.
pub fn split_edge_conflicts(
    schedule: &Schedule,
    live: &LiveModel,
    alloc: &Allocation,
) -> Vec<bool> {
    let mut conflict = vec![false; alloc.segments.len()];
    for m in ls_edge_mismatches(schedule, live, alloc) {
        if let Some(cell) = conflict.get_mut(m.node) {
            *cell = true;
        }
    }
    conflict
}

/// Where the copies of one CFG edge may be emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeCopyPlacement {
    /// At the end of the predecessor, after its phi copies' reads: the
    /// predecessor has this one successor, so every execution of that point
    /// takes this edge.
    PredEnd,
    /// At the start of the successor, before its first node AND before the
    /// timeline's own transitions at that node (`Allocation::events` at
    /// `span[s].0`, which assume the carried state these copies establish):
    /// the successor has this one predecessor, so every entry into it runs
    /// them.
    SuccStart,
    /// A critical edge (several successors, several predecessors). The copies
    /// need a block of their own on the edge; a consumer that cannot split an
    /// edge must refuse every value in [`EdgeResolution::nodes`].
    Critical,
}

/// The parallel copy one CFG edge needs so that the successor's code — emitted
/// in layout order against the fall-through state — finds every value where it
/// looks for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeResolution {
    /// Source block of the edge.
    pub pred: usize,
    /// Target block of the edge.
    pub succ: usize,
    /// Where the copies may run.
    pub placement: EdgeCopyPlacement,
    /// `(destination, source)` pairs, simultaneous — the convention a phi
    /// edge's parallel copy uses, so a consumer can append these to the edge's phi
    /// copies and sequence both through ONE [`resolve_parallel_copy`] (a phi
    /// copy may read a register a resolution copy writes, and vice versa).
    /// Destinations are always registers; a source is a register or the
    /// value's home word.
    pub copies: Vec<(ValueLoc, ValueLoc)>,
    /// `nodes[i]` = the value `copies[i]` moves.
    pub nodes: Vec<NodeId>,
}

/// [`split_edge_resolution`]'s answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitEdgeResolution {
    /// One entry per edge that needs at least one copy, in `(pred, succ)`
    /// order. Every copy of an `unresolvable` value has been removed.
    pub edges: Vec<EdgeResolution>,
    /// `unresolvable[id]` = some edge needs a copy for `id` that cannot be
    /// expressed or cannot be emitted without destroying another value (see
    /// [`split_edge_resolution`]). A consumer must refuse split residency for
    /// such a value exactly as it refuses a [`split_edge_conflicts`] value.
    pub unresolvable: Vec<bool>,
}

/// **Edge resolution** for a split timeline — the second half of Wimmer's
/// linear scan, which [`allocate_linear_scan`] does not perform during the scan
/// and [`split_edge_conflicts`] only detects.
///
/// For every mismatch [`split_edge_conflicts`] flags on an edge `p -> s` — the
/// code of `s` reads value `v` from register `want`, and the edge delivers it
/// elsewhere — this emits the copy that puts it there:
///
/// * delivered in another register `r` → `(Reg(want), Reg(r))`;
/// * delivered in memory (a memory segment at `p`'s edge position, or `p` laid
///   out before `v`'s definition) → `(Reg(want), Slot(home))`, a reload from
///   the home word. Correct for a **write-through** consumer, which writes the
///   home at the definition, and the definition dominates every block `v` is
///   live-in at. A non-write-through consumer additionally needs its one
///   `SpillKind::Store` to dominate that reload, which `ls_finish` does not
///   arrange (it places the store at the first register → memory transition in
///   layout order).
///
/// The copies target the state at `span[s].0 - 1`, NOT at `span[s].0`: a
/// layout-order emitter carries its state from the previous position and then
/// applies the timeline's own events at `span[s].0` (a `Load` or `Move` there
/// is in [`Allocation::events`]), so the edge must reproduce the carried state
/// and nothing more.
///
/// # What makes a value unresolvable
///
/// * The edge delivers it nowhere the timeline can name (`Dead` while the model
///   says it is live-in) — no source to copy from.
/// * It must be reloaded and has no home word in this allocation
///   ([`Allocation::stack_slot`] is `None`; never the case after
///   [`Allocation::adopt_home_colouring`] with a write-through consumer's
///   frame).
/// * Two values would be copied into one register on one edge (a malformed
///   allocation; [`verify_allocation`]'s proof 3 excludes it for a verified one).
/// * **The destination register belongs to another value at the top of `s`.**
///   A value `y` whose timeline puts it in `want` at `span[s].0` itself (its
///   range, widened to block spans, can begin exactly there) would be destroyed
///   by the copy — on a path where, without the copy, `want` still held `y`.
///
/// Every copy of an unresolvable value is removed from every edge, because a
/// consumer that refuses the value must not then write its register.
///
/// # What the consumer still owes
///
/// * Filter [`EdgeResolution::nodes`] to the values it actually keeps in split
///   residency before sequencing: a copy for a value the consumer refused for
///   its own reasons writes a register that value no longer owns.
/// * Run the copies AFTER anything on the edge that can invalidate a register
///   (a back edge's safepoint poll calls into the runtime, and a `Ref` reloaded
///   before it would be a stale root), and sequence them together with the
///   edge's phi copies.
/// * Honour [`EdgeResolution::placement`]; a [`EdgeCopyPlacement::Critical`]
///   edge needs a split block or a refusal.
///
/// Deterministic: edges in `(pred, succ)` enumeration order, copies in node
/// order within an edge.
pub fn split_edge_resolution(
    schedule: &Schedule,
    live: &LiveModel,
    alloc: &Allocation,
) -> SplitEdgeResolution {
    let n = alloc.segments.len();
    let mut unresolvable = vec![false; n];
    let mismatches = ls_edge_mismatches(schedule, live, alloc);
    if mismatches.is_empty() {
        return SplitEdgeResolution {
            edges: Vec::new(),
            unresolvable,
        };
    }

    // Does any value other than `except` hold `reg` at `pos` by the timeline —
    // split or not?
    let held_by_other = |reg: PhysReg, pos: usize, except: usize| -> bool {
        alloc.segments.iter().enumerate().any(|(id, segs)| {
            id != except
                && segs
                    .iter()
                    .any(|seg| seg.reg == Some(reg) && seg.range.contains(pos))
        })
    };

    let mut edges: Vec<EdgeResolution> = Vec::new();
    for m in &mismatches {
        let src = match m.have {
            LsLinearState::Reg(r) => ValueLoc::Reg(r),
            LsLinearState::Memory => match alloc.stack_slot.get(m.node).copied().flatten() {
                Some(word) => ValueLoc::Slot(word),
                None => {
                    unresolvable[m.node] = true;
                    continue;
                }
            },
            LsLinearState::Dead => {
                unresolvable[m.node] = true;
                continue;
            }
        };
        let Some(&(s_start, _)) = live.span.get(m.succ) else {
            unresolvable[m.node] = true;
            continue;
        };
        if held_by_other(m.want, s_start, m.node) {
            unresolvable[m.node] = true;
            continue;
        }
        let same_edge = edges
            .last()
            .is_some_and(|e| (e.pred, e.succ) == (m.pred, m.succ));
        if !same_edge {
            let placement = if schedule.blocks[m.pred].successors.len() == 1 {
                EdgeCopyPlacement::PredEnd
            } else if schedule
                .blocks
                .get(m.succ)
                .is_some_and(|b| b.predecessors.len() == 1)
            {
                EdgeCopyPlacement::SuccStart
            } else {
                EdgeCopyPlacement::Critical
            };
            edges.push(EdgeResolution {
                pred: m.pred,
                succ: m.succ,
                placement,
                copies: Vec::new(),
                nodes: Vec::new(),
            });
        }
        if let Some(edge) = edges.last_mut() {
            edge.copies.push((ValueLoc::Reg(m.want), src));
            edge.nodes.push(m.node as NodeId);
        }
    }

    // One destination written twice on one edge: every writer is refused.
    for edge in &edges {
        for (i, (dst, _)) in edge.copies.iter().enumerate() {
            if edge.copies[..i].iter().any(|(d, _)| d == dst) {
                for (j, (d, _)) in edge.copies.iter().enumerate() {
                    if d == dst {
                        unresolvable[edge.nodes[j] as usize] = true;
                    }
                }
            }
        }
    }

    // A refused value's copies go everywhere, not only on the edge that
    // refused it.
    for edge in edges.iter_mut() {
        let (copies, nodes): (Vec<(ValueLoc, ValueLoc)>, Vec<NodeId>) = edge
            .copies
            .iter()
            .copied()
            .zip(edge.nodes.iter().copied())
            .filter(|&(_, v)| !unresolvable[v as usize])
            .unzip();
        edge.copies = copies;
        edge.nodes = nodes;
    }
    edges.retain(|e| !e.copies.is_empty());

    SplitEdgeResolution {
        edges,
        unresolvable,
    }
}

// ── Metrics ──────────────────────────────────────────────────────────
//
// # R5: `record_allocation_metrics` was deleted here, and that was the decision
//
// It read:
//
// ```ignore
// pub fn record_allocation_metrics(recorder: &CompileRecorder, alloc: &Allocation) {
//     recorder.set_peak_live_values(alloc.peak_live);
//     recorder.set_spills(alloc.spills);
//     recorder.set_reloads(alloc.reloads);
// }
// ```
//
// and it had **no caller anywhere in the workspace** — not in production, not
// in this module's tests, despite its own doc comment claiming the latter. What
// it was waiting for arrived from the other direction: `ir_lower` now calls
// `metrics::note_current_spills` / `note_current_reloads` with its own
// `ls_spills` / `ls_reloads` under `ls_active`, so `CompilationReport::spills`
// and `::reloads` are measured on the linear-scan path and left `NotMeasured`
// on the colourer-only path — which is the correct report, not a missing one.
//
// The two producers do not measure the same thing:
//
// * this one reported what the ALLOCATION **planned** — `Allocation::spills` /
//   `::reloads`, the whole split/spill/remat schedule;
// * the ambient form reports what the backend **emitted**, which is the smaller
//   number, because `plan_register_residency` executes only the subset of the
//   plan it can prove.
//
// Two different numbers pointed at one `Measured<u32>` field is a report whose
// meaning depends on which producer ran last, and only one of the two describes
// machine code that exists. `NOTES-misc.md` §2 offered the alternative — keep
// it and give `CompilationReport` a second pair of fields, `planned_spills` /
// `planned_reloads`, so the gap between plan and emission is the thing being
// measured. That is a `metrics.rs` change and `metrics.rs` is not this lane's
// file; re-adding this function without it would re-create the collision.
//
// So: deleted, with the gap recorded at `Allocation::spills`.
//
// # Where that stands now (2026-09-17)
//
// **The `metrics.rs` half landed** (`NOTES-opts7.md` §2):
// `CompilationReport::planned_spills` / `::planned_reloads` exist, with
// `CompileRecorder::set_planned_spills` / `set_planned_reloads` and their own
// JSON keys, and `::spills` / `::reloads` are documented as the EMITTED figures
// and only those. So the collision above no longer forbids a producer — a
// re-added function writing the PLANNED pair would be correct.
//
// It would also still have no caller, which is why it has not been re-added.
// The only holder of an `Allocation` is `ir_lower::plan_register_residency`,
// and it holds no `CompileRecorder`; that is the open question, not which
// setter to call. `CompilationReport::planned_spills` states it, with the three
// candidate shapes. The stale prose mentions this section used to list are
// gone from `metrics.rs`.

#[cfg(test)]
pub(crate) mod linear_scan_tests {
    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_schedule;
    use std::collections::HashSet;

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
                graph: Graph::empty(0),
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
                    carried_and_read: Vec::new(),
                    converged: true,
                    block_live_in: Vec::new(),
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
            reads_precede_clobber: Vec::new(),
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
            "the knob lifts the SAFEPOINT refusal only — a home-bound value is \
             still refused for its own reason"
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

    // ── The clobber-list invariant (R2/R3) ───────────────────────────

    #[test]
    fn add_clobbers_merges_into_a_position_that_is_already_recorded() {
        // The failing shape: `for_graph` records the caller-saved set at a call
        // position, then a backend adds the registers ITS OWN emission destroys
        // at the same position — which is what `for_graph`'s doc comment tells
        // backends to do. Pushing produces two entries at one position and
        // `clobbered_at`'s binary search then resolves to an arbitrary one.
        let mut model = bare_model(gp_volatile_first(2, 0));
        model.add_clobbers(5, &[PhysReg::gp(0)]);
        model.add_clobbers(5, &[PhysReg::gp(1)]);

        assert_eq!(model.clobbers.len(), 1, "one entry per position");
        assert_eq!(
            model.clobbered_at(5),
            &[PhysReg::gp(0), PhysReg::gp(1)][..],
            "BOTH producers' registers must be visible to the binary search"
        );
    }

    #[test]
    fn add_clobbers_keeps_the_list_ordered_and_drops_registers_outside_the_file() {
        let mut model = bare_model(gp_volatile_first(2, 0));
        model.add_clobbers(9, &[PhysReg::gp(1)]);
        model.add_clobbers(3, &[PhysReg::gp(0)]);
        // Not in the file: the allocator cannot hand r7 out, so a clobber of it
        // says nothing. `for_graph` drops its own out-of-file fixed-operand
        // rules the same way.
        model.add_clobbers(6, &[PhysReg::gp(7)]);

        let positions: Vec<usize> = model.clobbers.iter().map(|(p, _)| *p).collect();
        assert_eq!(positions, vec![3, 9]);
        assert!(model.clobbered_at(6).is_empty());
    }

    #[test]
    fn canonicalize_clobbers_merges_a_list_that_was_built_by_pushing() {
        let mut model = bare_model(gp_volatile_first(2, 0));
        model.clobbers = vec![
            (9, vec![PhysReg::gp(1)]),
            (5, vec![PhysReg::gp(1), PhysReg::gp(0)]),
            (5, vec![PhysReg::gp(1)]),
            (5, vec![PhysReg::gp(0)]),
        ];
        model.canonicalize_clobbers();

        assert_eq!(
            model.clobbers,
            vec![
                (5, vec![PhysReg::gp(0), PhysReg::gp(1)]),
                (9, vec![PhysReg::gp(1)]),
            ]
        );
        // Idempotent: running it on an already-canonical list changes nothing.
        let once = model.clobbers.clone();
        model.canonicalize_clobbers();
        assert_eq!(model.clobbers, once);
    }

    #[test]
    fn verify_allocation_rejects_a_duplicate_clobber_position() {
        // Sorted-but-not-unique used to pass: the verifier checked `w[0].0 >
        // w[1].0`, which a duplicate satisfies. That left the ONE guard on the
        // invariant blind to the ONE way it actually gets broken.
        let mut f = Fixture::new(12);
        let _across = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 1));
        model.add_clobbers(5, &[PhysReg::gp(0)]);
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies as built");

        // Plant the duplicate a naive second producer would have created, in
        // sorted order so only the uniqueness half of the check can catch it.
        model.clobbers.push((5, vec![PhysReg::gp(1)]));
        model.clobbers.sort_by_key(|(pos, _)| *pos);
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("a duplicate position must be refused");
        assert_eq!(
            internal_message(&err),
            "regalloc: the clobber list is not strictly increasing by position"
        );
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

    // ── R6: the fixed / pre-coloured register path ───────────────────

    #[test]
    fn the_x86_64_file_recovers_the_five_fixed_operand_registers() {
        let full = RegFile::x86_64();
        let narrow = RegFile::x86_64_scratch_reserved();

        for (num, name) in [
            (0u8, "RAX"),
            (1, "RCX"),
            (2, "RDX"),
            (10, "R10"),
            (11, "R11"),
        ] {
            assert!(
                full.contains(PhysReg::gp(num)),
                "{name} is allocatable again now that the fixed-operand rules are \
                 modelled rather than dodged"
            );
            assert!(
                !narrow.contains(PhysReg::gp(num)),
                "{name} must stay out of the file a backend using it as private \
                 scratch asks for"
            );
        }
        // RSP and RBP are not registers this allocator may ever hand out, and
        // nothing about R6 changes that.
        assert!(!full.contains(PhysReg::gp(4)), "RSP");
        assert!(!full.contains(PhysReg::gp(5)), "RBP");
        assert_eq!(
            full.class_size(RegClass::Gp),
            narrow.class_size(RegClass::Gp) + 5,
            "exactly five registers came back"
        );
        assert_eq!(
            full.class_size(RegClass::Xmm),
            narrow.class_size(RegClass::Xmm),
            "R6 is a GP change; the XMM half is untouched"
        );
        for num in [0u8, 1, 2, 10, 11] {
            assert_eq!(
                full.is_caller_saved(PhysReg::gp(num)),
                Some(true),
                "every one of the five is volatile on both x86-64 ABIs"
            );
        }
    }

    /// `iload_0; iload_1; idiv` — the rule that has been unreachable since the
    /// constraint machinery was written.
    pub(crate) fn div_and_shift_graph() -> (Graph, ir_schedule::Schedule) {
        let code = [
            0x1a, 0x1b, 0x6c, 0x3d, // 0: iload_0; iload_1; idiv; istore_2
            0x1c, 0x1b, 0x78, 0x3e, // 4: iload_2; iload_1; ishl; istore_3
            0x1d, 0xac, // 8: iload_3; ireturn
            0, 0,
        ];
        let graph = IrBuilder::new(2, 4)
            .build(&code, 10)
            .expect("the divide-and-shift method builds");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    /// The position of the first node matching `pred`.
    fn pos_of_op(
        graph: &Graph,
        live: &LiveModel,
        pred: impl Fn(&Op, IrType) -> bool,
    ) -> Option<(NodeId, usize)> {
        graph.nodes.iter().enumerate().find_map(|(id, node)| {
            if !pred(&node.op, node.ty) {
                return None;
            }
            live.pos_of
                .get(id)
                .copied()
                .flatten()
                .map(|pos| (id as NodeId, pos))
        })
    }

    #[test]
    fn an_integer_division_pins_its_dividend_to_rax_and_clobbers_rax_and_rdx() {
        let (graph, schedule) = div_and_shift_graph();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());

        let (div, pos) = pos_of_op(&graph, &live, |op, ty| {
            matches!(op, Op::Div) && matches!(ty, IrType::Int)
        })
        .expect("the method divides");
        let dividend = graph.nodes[div as usize].inputs.as_slice()[0];

        assert!(
            model.fixed.contains(&FixedConstraint {
                node: dividend,
                pos,
                reg: X86_RAX,
            }),
            "`idiv` reads its dividend in RDX:RAX, and only a FixedConstraint can \
             say so — a clobber cannot. fixed = {:?}",
            model.fixed
        );
        let destroyed = model.clobbered_at(pos);
        assert!(
            destroyed.contains(&X86_RAX) && destroyed.contains(&X86_RDX),
            "the quotient lands in RAX and the remainder in RDX, and the `cqo` \
             that feeds the divide destroys RDX before it runs: {destroyed:?}"
        );
        assert!(
            !model.fixed.iter().any(|f| f.node == div),
            "the RESULT is deliberately unpinned: it would want RAX at the same \
             position the dividend does, which is the unsatisfiable model the \
             allocator bails on"
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_variable_shift_pins_its_count_to_rcx_and_destroys_nothing() {
        let (graph, schedule) = div_and_shift_graph();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());

        let (shift, pos) =
            pos_of_op(&graph, &live, |op, _| matches!(op, Op::Shl)).expect("the method shifts");
        let count = graph.nodes[shift as usize].inputs.as_slice()[1];

        assert!(
            model.fixed.contains(&FixedConstraint {
                node: count,
                pos,
                reg: X86_RCX,
            }),
            "input 1 is the shift count and CL is the only place it can be read \
             from. fixed = {:?}",
            model.fixed
        );
        assert!(
            !model.clobbered_at(pos).contains(&X86_RCX),
            "`shl r/m, cl` READS CL and writes only r/m. Clobbering RCX here was \
             an over-approximation that emptied the register for no machine reason"
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_fixed_operand_rule_says_nothing_when_its_register_is_not_in_the_file() {
        let (graph, schedule) = div_and_shift_graph();
        let live = build_live_model(&graph, &schedule);
        // The file `ir_lower` allocates over: RAX/RCX/RDX are its own per-opcode
        // scratch and are not handed out, so the model it gets must be exactly
        // the one it got before R6.
        let model =
            MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64_scratch_reserved());

        assert!(
            model.fixed.is_empty(),
            "a constraint naming a register the allocator cannot hand out is not \
             a constraint: {:?}",
            model.fixed
        );
        for (_, regs) in model.clobbers.iter() {
            for reg in regs {
                assert!(
                    *reg != X86_RAX && *reg != X86_RCX && *reg != X86_RDX,
                    "{reg} is outside this file and must not appear as a clobber"
                );
            }
        }
    }

    #[test]
    fn a_floating_point_division_carries_no_general_purpose_constraint() {
        // `dload_0; dload_2; ddiv; dreturn` — `divsd`, which reads and writes
        // one XMM and touches no GPR at all. The old clobber rule fired on
        // `Op::Div` regardless of type; that was invisible only because the
        // registers it named were filtered out on the next line.
        let code = [0x26, 0x28, 0x6f, 0xaf, 0, 0];
        // A builder refusal is not what is under test here: the assertion is
        // about `for_graph`'s type gate, and it is vacuous rather than wrong if
        // this shape never reaches it.
        // `build` answers `Option`, not `Result` -- a refusal carries no
        // reason out of the builder, it simply declines the method.
        // Two `double` parameters, typed: untyped, `dload` feeds an Int into
        // `ddiv`, which the operand type lane rejects.
        let mut builder = IrBuilder::new(2, 4);
        builder.set_param_types(&[IrType::Double, IrType::Double]);
        let Some(graph) = builder.build(&code, 4) else {
            return;
        };
        let schedule = ir_schedule::schedule(&graph);
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());

        let Some((_, pos)) = pos_of_op(&graph, &live, |op, ty| {
            matches!(op, Op::Div) && matches!(ty, IrType::Double)
        }) else {
            // The builder declined this shape; there is nothing to assert and
            // nothing to be wrong about.
            return;
        };
        assert!(
            !model.fixed.iter().any(|f| f.pos == pos),
            "`divsd` has no fixed GP operand"
        );
        let destroyed = model.clobbered_at(pos);
        assert!(
            !destroyed.contains(&X86_RAX) && !destroyed.contains(&X86_RDX),
            "an FP divide destroys no GPR: {destroyed:?}"
        );
    }

    /// JIT round 19 wave 4 (lane irops3, IROPS2-3): over the WIDE file, every
    /// GP register the scalar-intrinsic arm writes is destroyed at the node's
    /// position -- `multiplyHigh`'s RCX (its `b`) included, which the wave-1
    /// clobber left out -- and the narrow file `ir_lower` allocates over still
    /// carries none of them.
    #[test]
    fn every_scalar_intrinsic_clobbers_the_gp_registers_its_arm_writes() {
        use crate::ir::ScalarOp as S;
        const ALL: &[S] = &[
            S::MinI,
            S::MaxI,
            S::MinL,
            S::MaxL,
            S::AbsI,
            S::AbsL,
            S::CompareI,
            S::CompareL,
            S::NlzI,
            S::NlzL,
            S::NtzI,
            S::NtzL,
            S::ReverseBytesI,
            S::ReverseBytesL,
            S::LowestOneBitI,
            S::LowestOneBitL,
            S::HighestOneBitI,
            S::HighestOneBitL,
            S::RotateLeftI,
            S::RotateLeftL,
            S::RotateRightI,
            S::RotateRightL,
            S::AbsF,
            S::AbsD,
            S::DoubleToRawLongBits,
            S::LongBitsToDouble,
            S::FloatToRawIntBits,
            S::IntBitsToFloat,
            S::SqrtD,
            S::MinF,
            S::MaxF,
            S::MinD,
            S::MaxD,
            S::MultiplyHighL,
            S::UnsignedMultiplyHighL,
            S::FloorD,
            S::CeilD,
            S::RintD,
            S::FmaD,
            S::FmaF,
            S::BitCountI,
            S::BitCountL,
            S::RoundD,
            S::RoundF,
        ];
        assert!(scalar_op_gp_writes(S::MultiplyHighL).contains(&X86_RCX));
        for &sop in ALL {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            g.entry = start;
            let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let inputs: Vec<NodeId> = (0..sop.arity())
                .map(|k| g.add(Op::Param(k as u16), sop.input_ty(k), vec![start], None))
                .collect();
            let v = g.add(Op::ScalarIntrinsic(sop), sop.result_type(), inputs, Some(0));
            g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, v], Some(1));
            let schedule = ir_schedule::schedule(&g);
            let live = build_live_model(&g, &schedule);
            let Some((_, pos)) = pos_of_op(&g, &live, |op, _| *op == Op::ScalarIntrinsic(sop))
            else {
                panic!("{sop:?} has no position");
            };
            let wide = MachineModel::for_graph(&g, &schedule, &live, RegFile::x86_64());
            for reg in scalar_op_gp_writes(sop) {
                assert!(
                    wide.clobbered_at(pos).contains(reg),
                    "{sop:?} writes {reg} but the wide model keeps it live across"
                );
            }
            let narrow =
                MachineModel::for_graph(&g, &schedule, &live, RegFile::x86_64_scratch_reserved());
            assert!(
                narrow.clobbered_at(pos).is_empty(),
                "{sop:?}: the narrow file holds no register the arm writes"
            );
        }
    }

    #[test]
    fn a_dividend_keeps_its_pinned_register_through_the_clobber_that_ends_it() {
        // The early-clobber shape, in isolation: the value is REQUIRED in r0 at
        // position 4 and r0 is DESTROYED at position 4, because `idiv` reads RAX
        // and then writes it. It is also read again at 9, so the allocator has
        // to make a decision rather than getting the right answer by the value
        // being dead.
        let mut f = Fixture::new(12);
        let dividend = f.value(Op::Add, IrType::Int, 0, 9, &[4, 9]);
        let _other = f.value(Op::Add, IrType::Int, 1, 8, &[8]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(2));
        model.fixed = vec![FixedConstraint {
            node: dividend,
            pos: 4,
            reg: PhysReg::gp(0),
        }];
        model.add_clobbers(4, &[PhysReg::gp(0), PhysReg::gp(1)]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(dividend, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the dividend must reach the divide IN r0; a clobber at the same \
             position is the write half of the same instruction, not a conflict"
        );
        let holding = alloc.segments[dividend as usize]
            .iter()
            .find(|s| s.range.contains(4))
            .expect("a segment covers the constrained position");
        assert_eq!(
            (holding.range.lo, holding.range.hi),
            (0, 4),
            "and it must give the register up AT the clobber, not after it"
        );
        verify_allocation(&graph, &live, &model, &alloc)
            .expect("the verifier exempts exactly this segment");

        // The control. With no constraint the clobber is an ordinary one and
        // the value is in memory at 4 — which is what makes the assertion above
        // a statement about the constraint rather than about the fixture.
        let mut unpinned = bare_model(gp(2));
        unpinned.add_clobbers(4, &[PhysReg::gp(0), PhysReg::gp(1)]);
        let plain = allocate_linear_scan(&graph, &live, &unpinned).expect("allocates");
        assert!(
            !matches!(plain.location_at(dividend, 4), Some(ValueLoc::Reg(_))),
            "without the constraint nothing may hold a clobbered register at the \
             clobbering position"
        );
    }

    #[test]
    fn a_value_with_two_fixed_roles_is_split_rather_than_left_in_the_wrong_register() {
        // A value used as `idiv`'s dividend at 4 and as a shift's count at 8
        // requires two different registers inside one live range. Before R6 the
        // second constraint was invisible to `ls_select_register`, the value
        // kept r0 across position 8, and `verify_allocation`'s proof 4b rejected
        // the result as an INTERNAL error — costing the method its optimized
        // body over a shape that just needs a split.
        let mut f = Fixture::new(14);
        let v = f.value(Op::Add, IrType::Int, 0, 12, &[4, 8, 12]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(3));
        model.fixed = vec![
            FixedConstraint {
                node: v,
                pos: 4,
                reg: PhysReg::gp(0),
            },
            FixedConstraint {
                node: v,
                pos: 8,
                reg: PhysReg::gp(1),
            },
        ];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(v, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the earliest constraint in the interval is the one the assignment honours"
        );
        assert!(
            !matches!(
                alloc.location_at(v, 8),
                Some(ValueLoc::Reg(PhysReg { num: 0, .. }))
            ),
            "and the value must be out of r0 by the position that wants r1"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_clobber_between_definition_and_constraint_is_covered_by_a_split() {
        // r0 is destroyed at 2 and required at 6, so no single assignment covers
        // both. This used to be a hard `RegisterPressure` bailout on the reading
        // that "the value must be in that register and it is not" — which would
        // now refuse the optimized body of every pressured method containing a
        // division. The constraint's real promise is narrower (see
        // `FixedConstraint`): nobody ELSE is in r0 at 6, and this value is not
        // in some other register there. A home word satisfies both.
        let mut f = Fixture::new(10);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[6, 8]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(1));
        model.fixed = vec![FixedConstraint {
            node: a,
            pos: 6,
            reg: PhysReg::gp(0),
        }];
        model.add_clobbers(2, &[PhysReg::gp(0)]);

        let alloc = allocate_linear_scan(&graph, &live, &model)
            .expect("a clobber before the constraint is not a failed compile");
        // The constraint IS honoured, and this assertion was inverted before.
        //
        // The comment above reasons that "no single assignment covers both" —
        // true, and it is the wrong unit. A linear-scan interval is not one
        // assignment: the value takes r0 from its definition, gives it up one
        // short of the clobber at 2, sits in its home word across it, and
        // takes r0 again at its next use — which is 6, the constrained
        // position. Splitting is exactly what makes a clobber between a
        // definition and a constraint survivable, so demanding a demotion here
        // asked the allocator for the worse of two correct answers.
        //
        // The demotion path is still there and still right for a constraint
        // that genuinely cannot be met. What it must never do is leave the
        // value in a DIFFERENT register across the constrained position: an
        // emitter told "this value is in r1" cannot know to load the fixed
        // operand from the home word instead. That is enforced where the
        // segment is cut, not here.
        assert_eq!(
            alloc.location_at(a, 6),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the split gives r0 back at the constrained position"
        );
        assert!(
            !matches!(alloc.location_at(a, 2), Some(ValueLoc::Reg(_))),
            "and the value is out of r0 across the clobber that destroys it"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── R12: what actually bounds the eviction path ──────────────────

    /// The guard in [`ls_pick_victim`] that makes the eviction path's re-queue a
    /// strict advance, pinned as behaviour.
    ///
    /// `far` is defined at 4 and not read again until 14; `near` is defined at
    /// the same position 4 and read constantly. By the eviction score alone
    /// `near` — high frequency weight, so a LOW score — should keep the register
    /// and `far` should lose it. It is the other way round here only because
    /// `near` is already active and starts at exactly the position `far` starts
    /// at, so `ls_pick_victim` refuses to nominate it: an interval cut at its own
    /// start has no prefix to keep and would be re-queued at its own unchanged
    /// start, which is the shape that would let this loop run for ever.
    #[test]
    fn ls_pick_victim_never_nominates_an_active_that_starts_here() {
        let mut f = Fixture::new(18);
        // Pops first among equal starts: the heap orders by `(lo, hi, node)`.
        let near = f.value(Op::Add, IrType::Int, 4, 6, &[6]);
        let far = f.value(
            Op::Add,
            IrType::Int,
            4,
            15,
            &[5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        );
        let (graph, live) = f.finish();
        let model = bare_model(gp(1));

        // The score `far` would have won with: distance 1 over weight 11 is a
        // far worse victim score than `near`'s distance 2 over weight 1.
        assert!(live.weight[far as usize] > live.weight[near as usize]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(near, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the active that starts here keeps its register however attractive a \
             victim it looks"
        );
        assert!(
            !matches!(alloc.location_at(far, 4), Some(ValueLoc::Reg(_))),
            "and the arriving interval is the one that goes to memory"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── A10 / R13: the AArch64 file ──────────────────────────────────

    #[test]
    fn the_aarch64_file_excludes_every_register_the_emitter_owns() {
        let regs = RegFile::aarch64(&[]);

        for (num, why) in [
            (0u8, "X0 is an argument register"),
            (7, "X7 is an argument register"),
            (8, "X8 is the indirect result location register"),
            (16, "X16 is IP0, and this backend's scratch four times over"),
            (17, "X17 is IP1"),
            (18, "X18 is the platform register"),
            (29, "X29 is FP"),
            (30, "X30 is LR"),
            (31, "encoding 31 is SP, which is also XZR"),
        ] {
            assert!(!regs.contains(PhysReg::gp(num)), "{why}");
        }
        for num in 19..=28u8 {
            assert!(regs.contains(PhysReg::gp(num)), "X{num} is callee-saved");
        }
        for num in 9..=15u8 {
            assert!(regs.contains(PhysReg::gp(num)), "X{num} is a temporary");
        }
        assert_eq!(regs.class_size(RegClass::Gp), 17);
        assert_eq!(regs.class_size(RegClass::Xmm), 8);
    }

    #[test]
    fn the_aarch64_fp_half_is_volatile_until_a_frame_saves_it() {
        // AAPCS64 makes D8–D15 callee-saved, but `aarch64_backend`'s prologue
        // saves only GPRs — so with no save area a value left in D8 across a
        // call is destroyed and the caller's copy is gone. That is the bug that
        // made this backend stop homing float locals in FP registers at all, and
        // the file must not re-introduce it by quoting the ABI at the emitter.
        let no_save_area = RegFile::aarch64(&[]);
        for num in ARM64_FP_LINEAR_SCAN {
            assert_eq!(
                no_save_area.is_caller_saved(PhysReg::xmm(num)),
                Some(true),
                "with no save area D{num} must split every interval that spans a call"
            );
        }

        let with_save_area = RegFile::aarch64(&ARM64_FP_LINEAR_SCAN);
        for num in ARM64_FP_LINEAR_SCAN {
            assert_eq!(
                with_save_area.is_caller_saved(PhysReg::xmm(num)),
                Some(false),
                "and once the frame gives D{num} back, a value may live across a call in it"
            );
        }

        // The GP half does not depend on the save area: a callee-saved GPR is
        // given back by the CALLEE.
        for (file, label) in [(&no_save_area, "no save area"), (&with_save_area, "saved")] {
            assert_eq!(
                file.is_caller_saved(PhysReg::gp(19)),
                Some(false),
                "X19 under {label}"
            );
            assert_eq!(
                file.is_caller_saved(PhysReg::gp(9)),
                Some(true),
                "X9 under {label}"
            );
        }
    }

    /// The AArch64 file must run the same allocator the x86-64 one does — that
    /// is what "the pass is not x86-specific" has to mean to be worth saying.
    #[test]
    fn a_scheduled_loop_allocates_over_the_aarch64_file_too() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::aarch64(&[]));

        assert!(
            model.fixed.is_empty(),
            "the fixed-operand rules are gated on their registers being in the \
             file, and an AArch64 file has none of them: {:?}",
            model.fixed
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
        for seg in alloc.segments.iter().flatten() {
            if let Some(reg) = seg.reg {
                assert!(model.regs.contains(reg), "{reg} is not in the AArch64 file");
            }
        }
    }

    // ── R7: the two frame colourings ─────────────────────────────────

    /// The finding that makes `NOTES-regalloc.md` §4's "assert they agree"
    /// option wrong rather than merely unimplemented.
    #[test]
    fn the_allocators_home_colouring_is_not_the_emitters_numbering() {
        let mut f = Fixture::new(14);
        // `resident` never touches memory: two registers, and it is the only
        // long-lived value, so it holds one for its whole range.
        let resident = f.value(Op::Add, IrType::Int, 0, 12, &[12]);
        // `spilled` is forced out by a clobber in the middle of its range.
        let spilled = f.value(Op::Add, IrType::Int, 1, 11, &[4, 11]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        // Only r0. r1 survives the clobber, so `resident` — allocated first,
        // and the only value live at position 0 — takes it and never needs a
        // home word at all.
        model.add_clobbers(6, &[PhysReg::gp(0)]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.needs_home(resident),
            "a value that lives its whole life in a register needs no home word \
             — that frame-size win is the reason the two colourings differ"
        );
        assert!(alloc.needs_home(spilled));
        assert_eq!(
            alloc.stack_slot[resident as usize], None,
            "and so it consumes no COLOUR either, which shifts every colour after \
             it relative to a colouring that gives every value a word"
        );
        assert_eq!(
            alloc.stack_slot[spilled as usize],
            Some(0),
            "the spilled value takes word 0, not the word a plan_slots-style \
             colouring — which would have spent word 0 on `resident` — would give it"
        );
    }

    #[test]
    fn adopting_the_emitters_frame_makes_every_slot_name_a_word_it_can_address() {
        let mut f = Fixture::new(14);
        let resident = f.value(Op::Add, IrType::Int, 0, 12, &[12]);
        let spilled = f.value(Op::Add, IrType::Int, 1, 11, &[4, 11]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        // Only r0. r1 survives the clobber, so `resident` — allocated first,
        // and the only value live at position 0 — takes it and never needs a
        // home word at all.
        model.add_clobbers(6, &[PhysReg::gp(0)]);
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");

        // What a write-through consumer's own colourer produces: a word for
        // EVERY value, in its own numbering.
        let emitter = vec![Some(0u32), Some(1u32)];
        alloc
            .adopt_home_colouring(&emitter, 2)
            .expect("the emitter's frame has a word for everything that needs one");

        assert_eq!(alloc.stack_slot, emitter);
        assert_eq!(alloc.stack_slots, 2);
        assert_eq!(
            alloc.location_at(spilled, 6),
            Some(ValueLoc::Slot(1)),
            "a Slot payload now names the emitter's word, not the allocator's"
        );
        assert!(matches!(
            alloc.location_at(resident, 6),
            Some(ValueLoc::Reg(_))
        ));
        verify_allocation(&graph, &live, &model, &alloc)
            .expect("and proof 6 is now a statement about the frame that is emitted");
    }

    #[test]
    fn adopting_a_frame_with_no_word_for_a_spilled_value_is_refused() {
        let mut f = Fixture::new(14);
        let _resident = f.value(Op::Add, IrType::Int, 0, 12, &[12]);
        let _spilled = f.value(Op::Add, IrType::Int, 1, 11, &[4, 11]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        // Only r0. r1 survives the clobber, so `resident` — allocated first,
        // and the only value live at position 0 — takes it and never needs a
        // home word at all.
        model.add_clobbers(6, &[PhysReg::gp(0)]);
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");

        // No word for the value that spends positions 6..=10 in memory. Silently
        // accepting this would make `location_at` answer `None` at a position
        // inside that value's own live range.
        let err = alloc
            .adopt_home_colouring(&[Some(0), None], 1)
            .expect_err("a value with nowhere to spill to must be refused");
        assert!(
            internal_message(&err).contains("no home word"),
            "wrong diagnosis: {err:?}"
        );

        let err = alloc
            .adopt_home_colouring(&[Some(0)], 1)
            .expect_err("a colouring that is not sized for the graph must be refused");
        assert!(
            internal_message(&err).contains("sized for the graph"),
            "wrong diagnosis: {err:?}"
        );
    }

    // ── R10: loop depth from the dominators ──────────────────────────

    /// A CFG whose block *order* is not a reverse postorder, which is the state
    /// `ir_schedule::layout_blocks` leaves every method in.
    ///
    /// ```text
    ///   0 ──▶ 1 ──▶ 2 ──▶ 4          1 is the loop header
    ///         │╰────▶ 3 ─╯(back)     3 is the only body block
    ///         ╰───────────┘          2 is a loop EXIT, laid out inside the
    ///                                index window [1, 3]
    /// ```
    ///
    /// The RPO of this graph is `0, 1, 3, 2, 4`; the layout here is `0..=4`.
    /// Block 2 therefore sits between the header and the back-edge source
    /// without being in the loop.
    fn non_rpo_loop_schedule() -> ir_schedule::Schedule {
        let edges: [(Vec<usize>, Vec<usize>); 5] = [
            (vec![1], vec![]),        // 0: entry
            (vec![2, 3], vec![0, 3]), // 1: loop header
            (vec![4], vec![1]),       // 2: loop EXIT, not in the loop
            (vec![1], vec![1]),       // 3: loop body, back edge to 1
            (vec![], vec![2]),        // 4: exit
        ];
        let blocks: Vec<ir_schedule::Block> = edges
            .into_iter()
            .enumerate()
            .map(|(id, (successors, predecessors))| ir_schedule::Block {
                id,
                ctrl: NO_NODE,
                nodes: Vec::new(),
                terminator: None,
                successors,
                predecessors,
            })
            .collect();
        let dom = ir_schedule::Dominators::compute(&blocks);
        ir_schedule::Schedule {
            blocks,
            node_to_block: Vec::new(),
            dom,
            freq: Default::default(),
            layout: Default::default(),
            pairing: Default::default(),
        }
    }

    #[test]
    fn loop_depth_comes_from_the_dominators_and_not_from_block_order() {
        let schedule = non_rpo_loop_schedule();
        assert!(
            schedule.dom.dominates(1, 3),
            "3 -> 1 is a back edge because its target dominates its source"
        );

        let depths = ls_loop_depths(&schedule);
        assert_eq!(
            depths,
            vec![0, 1, 0, 1, 0],
            "the natural loop of 3 -> 1 is {{1, 3}}. The old model declared every \
             block in the INDEX WINDOW [1, 3] one level deeper, which charges \
             block 2 — a loop exit — ten times the frequency it earns, and with \
             it ten times the spill cost"
        );
    }

    /// **One loop-depth model (round 11 wave 3).** `ls_loop_depths` used to be
    /// a second copy of `ir_schedule::loop_depths`, and a doc-comment scan was
    /// the only test tying the two together. It now delegates, so the pin is
    /// behavioural: on a schedule whose dominators match its blocks the two
    /// answer identically, and on one whose dominators do not, this side
    /// answers all zeros rather than asking the scheduler's copy to index a
    /// relation sized for another CFG.
    #[test]
    fn the_allocator_reads_the_schedulers_loop_depths() {
        for schedule in [non_rpo_loop_schedule(), swap_loop().1] {
            assert_eq!(
                ls_loop_depths(&schedule),
                ir_schedule::loop_depths(&schedule.blocks, &schedule.dom)
            );
        }
        let mut mis_sized = non_rpo_loop_schedule();
        mis_sized.dom = ir_schedule::Dominators::compute(&[]);
        assert_eq!(
            ls_loop_depths(&mis_sized),
            vec![0u32; mis_sized.blocks.len()]
        );
    }

    #[test]
    fn a_real_loop_still_reads_as_one_level_deep() {
        // The dominator model must not have made every depth zero: the whole
        // point of `weight` is that a value used inside a loop outranks one used
        // outside it.
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        assert!(
            live.loop_depth.iter().any(|&d| d >= 1),
            "a method with a back edge must have a block inside a loop: {:?}",
            live.loop_depth
        );
        assert!(
            live.loop_depth.iter().all(|&d| d <= 1),
            "and a singly-nested loop must not read as doubly nested: {:?}",
            live.loop_depth
        );
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

    /// Round 11 wave 8: `resolve_parallel_copy` keeps reader counts instead of
    /// rescanning `pending`. The emitted sequence must be the one the
    /// rescanning version chose, op for op, so `ir_lower`'s φ-edge code is
    /// byte-identical; this is that version, run against pseudo-random webs
    /// (chains, fan-out, several cycles, self-copies, repeated pairs).
    #[test]
    fn r11w8_parallel_copy_matches_the_rescanning_reference() {
        fn reference(copies: &[(ValueLoc, ValueLoc)]) -> Vec<CopyOp> {
            let mut pending: Vec<(ValueLoc, Option<ValueLoc>)> = Vec::new();
            for &(dst, src) in copies {
                if dst == src {
                    continue;
                }
                match pending.iter().find(|(d, _)| *d == dst).map(|(_, s)| *s) {
                    Some(_) => continue,
                    None => pending.push((dst, Some(src))),
                }
            }
            let mut out = Vec::new();
            while !pending.is_empty() {
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
            out
        }

        let loc = |k: u64| -> ValueLoc {
            if k % 5 == 4 {
                ValueLoc::Reg(PhysReg::gp((k % 7) as u8))
            } else {
                slot((k % 9) as u32)
            }
        };
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        let mut cyclic = 0usize;
        for _ in 0..500 {
            let len = (next() % 12) as usize;
            let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::new();
            for _ in 0..len {
                let dst = loc(next());
                let src = loc(next());
                // Keep the web well formed: one source per destination, but
                // let an identical pair repeat.
                match copies.iter().find(|(d, _)| *d == dst) {
                    Some(&(_, s)) if s != src => continue,
                    _ => copies.push((dst, src)),
                }
            }
            let ops = check_parallel(&copies);
            if ops.iter().any(|o| matches!(o, CopyOp::Save { .. })) {
                cyclic += 1;
            }
            assert_eq!(ops, reference(&copies), "copies: {copies:?}");
        }
        assert!(cyclic > 0, "the generator must produce some cycles");

        // Round 11 wave 9: the ready set is now a min-heap. Wide webs (a
        // switch join's φ count) with many interleaved cycles, chains that
        // feed cycles and fan-out, still op-for-op the reference.
        let mut wide_cyclic = 0usize;
        // (The reference is cubic in the width, so the widths stay modest.)
        for round in 0..40u32 {
            let width = 40 + round * 2;
            let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::new();
            for d in 0..width {
                let src = match next() % 4 {
                    // a rotation of the whole web: long cycles
                    0 => slot((d + 1) % width),
                    // pairwise swaps
                    1 => slot(d ^ 1),
                    // a chain into a location outside the web
                    2 => slot(width + (next() % 8) as u32),
                    // fan-out from a random member
                    _ => slot((next() % u64::from(width)) as u32),
                };
                copies.push((slot(d), src));
            }
            let ops = check_parallel(&copies);
            if ops.iter().any(|o| matches!(o, CopyOp::Save { .. })) {
                wide_cyclic += 1;
            }
            assert_eq!(ops, reference(&copies), "width {width}");
        }
        assert!(wide_cyclic > 0, "the wide webs must contain cycles");
    }

    /// Round 11 wave 9: `ClobberIndex::first_in` answers exactly what the walk
    /// it replaced in `ls_select_register` and proof 4 answered — the first
    /// clobber of `reg` in `[lo, hi]`, `hi` excluded when exempt — on every
    /// canonical clobber list.
    #[test]
    fn r11w9_clobber_index_matches_the_clobber_walk() {
        fn walk(
            clobbers: &[(usize, Vec<PhysReg>)],
            reg: PhysReg,
            lo: usize,
            hi: usize,
            exempt_hi: bool,
        ) -> Option<usize> {
            let start = clobbers.partition_point(|(p, _)| *p < lo);
            for (pos, regs) in &clobbers[start..] {
                if *pos > hi {
                    break;
                }
                if exempt_hi && *pos == hi {
                    continue;
                }
                if regs.contains(&reg) {
                    return Some(*pos);
                }
            }
            None
        }
        let regs = [
            PhysReg::gp(0),
            PhysReg::gp(1),
            PhysReg::gp(2),
            PhysReg::xmm(0),
            PhysReg::xmm(1),
        ];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let mut model = MachineModel::default();
            let mut pos = 0usize;
            for _ in 0..(next() % 20) {
                pos += 1 + (next() % 4) as usize;
                let mut at: Vec<PhysReg> =
                    regs.iter().copied().filter(|_| next() % 3 == 0).collect();
                if at.is_empty() {
                    continue;
                }
                at.sort_unstable();
                model.clobbers.push((pos, at));
            }
            let index = ClobberIndex::of(&model);
            for _ in 0..40 {
                let lo = (next() % 90) as usize;
                let hi = lo + (next() % 30) as usize;
                let reg = regs[(next() % regs.len() as u64) as usize];
                for exempt in [false, true] {
                    assert_eq!(
                        index.first_in(reg, lo, hi, exempt),
                        walk(&model.clobbers, reg, lo, hi, exempt),
                        "{reg} over [{lo}, {hi}] exempt={exempt}: {:?}",
                        model.clobbers
                    );
                }
            }
        }
    }

    /// Round 11 wave 9: `width` disjoint swaps. Every one is a cycle, so the
    /// breaker runs `width` times; each break must touch only its own
    /// cycle's reader (the `reads` index), and every save is restored before
    /// the next save reuses the scratch.
    #[test]
    fn r11w9_many_disjoint_swaps_pair_each_save_with_its_restore() {
        let width = 400u32;
        let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::new();
        for k in 0..width / 2 {
            copies.push((slot(2 * k), slot(2 * k + 1)));
            copies.push((slot(2 * k + 1), slot(2 * k)));
        }
        let ops = check_parallel(&copies);
        let mut open = false;
        for op in &ops {
            match op {
                CopyOp::Save { .. } => {
                    assert!(!open, "a save before the previous restore: {ops:?}");
                    open = true;
                }
                CopyOp::Restore { .. } => {
                    assert!(open, "a restore with no save: {ops:?}");
                    open = false;
                }
                CopyOp::Move { .. } => {}
            }
        }
        assert!(!open);
        assert_eq!(
            ops.len(),
            (width / 2 * 3) as usize,
            "save + move + restore per swap"
        );
    }

    // ── End to end, on a real scheduled graph ────────────────────────

    /// `int f(int n) { int a=1, b=2; for (int i=0; i<n; i++) swap(a, b); return a; }`
    ///
    /// The swap is done on the operand stack (`iload_1; iload_2; istore_1;
    /// istore_2`), so it needs no temporary local — which makes the loop
    /// header's two phis each other's back-edge value, i.e. a copy cycle.
    pub(crate) fn swap_loop() -> (Graph, ir_schedule::Schedule) {
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

    /// JIT round 17 wave 3 (lane irlower, OD16-1 token-edge half): the opt-in
    /// switch leaves the value set alone (the `wants_slot` lockstep) and only
    /// ever REMOVES recorded uses -- never adds or moves one.
    #[test]
    fn r17w3_irlower_token_edges_not_uses_only_shrinks_uses() {
        let key = "CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES";
        for (graph, schedule) in [swap_loop(), div_and_shift_graph()] {
            let off = cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
                build_live_model(&graph, &schedule)
            });
            let on = cratonvm_types::flags::with_thread_overrides(&[(key, Some("1"))], || {
                build_live_model(&graph, &schedule)
            });
            assert_eq!(off.wants_loc, on.wants_loc, "the value set is unchanged");
            for id in 0..graph.nodes.len() {
                for u in &on.uses[id] {
                    assert!(
                        off.uses[id].contains(u),
                        "n{id}: use at {u} appears only with the switch on"
                    );
                }
            }
        }
    }

    /// `static int f(int n){ int s=0; for(int i=0;i<n;i++) for(int j=0;j<n;j++) s+=j; return s; }`
    /// with the i58-L2 proposal's shape added by hand: a volatile
    /// `getstatic stop` (`Int`) in the outer body, and a `getstatic burnt` in
    /// the inner header that names it ONLY as its memory token. Answers the
    /// graph, its schedule, the inner header block and `stop`.
    pub(crate) fn nested_loop_with_a_token_read() -> (Graph, ir_schedule::Schedule, usize, NodeId) {
        let code: [u8; 36] = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0x1d, 0x1a, 0xa2,
            0x00, 0x0d, 0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf4, 0x84, 0x02,
            0x01, 0xa7, 0xff, 0xe7, 0x1b, 0xac, 0, 0,
        ];
        let mut graph = IrBuilder::new(1, 4)
            .build(&code, 34)
            .expect("the nested loop builds");
        let mut schedule = ir_schedule::schedule(&graph);
        let inner = schedule
            .blocks
            .iter()
            .position(|b| {
                let ctrl = &graph.nodes[b.ctrl as usize];
                ctrl.bytecode_pc == Some(11) && matches!(ctrl.op, Op::Merge | Op::Region)
            })
            .expect("the inner header is a block");
        let outer_body = schedule
            .dom
            .idom(inner)
            .expect("the inner header has an idom");
        let place = |schedule: &mut ir_schedule::Schedule, id: NodeId, b: usize| {
            let i = id as usize;
            if schedule.node_to_block.len() <= i {
                schedule.node_to_block.resize(i + 1, usize::MAX);
            }
            schedule.node_to_block[i] = b;
            schedule.blocks[b].nodes.push(id);
        };
        let outer_ctrl = schedule.blocks[outer_body].ctrl;
        let stop = graph.add(
            Op::LoadStatic {
                class_id: 1,
                field_index: 0,
                type_tag: b'Z',
                is_volatile: true,
            },
            IrType::Int,
            vec![outer_ctrl, outer_ctrl],
            None,
        );
        place(&mut schedule, stop, outer_body);
        let inner_ctrl = schedule.blocks[inner].ctrl;
        let burnt = graph.add(
            Op::LoadStatic {
                class_id: 1,
                field_index: 1,
                type_tag: b'J',
                is_volatile: false,
            },
            IrType::Long,
            vec![inner_ctrl, stop],
            None,
        );
        place(&mut schedule, burnt, inner);
        assert!(
            crate::ir::is_memory_token_slot(&graph.nodes[burnt as usize], 1),
            "`burnt` names `stop` in its token slot"
        );
        (graph, schedule, inner, stop)
    }

    /// Interpreter round i2 wave 63, lane L2 (i58-L2 proposal, first
    /// measurement): the census sees the proposal's shape. Off, `stop` is live
    /// into the inner header and across its back edge only because `burnt`
    /// names it as a token; on, its interval is its definition alone.
    #[test]
    fn i63_l2_token_trim_census_sees_a_value_held_across_a_loop_by_a_token_read() {
        let (graph, schedule, inner, stop) = nested_loop_with_a_token_read();
        let off = build_live_model_with_token_edges(&graph, &schedule, false);
        let on = build_live_model_with_token_edges(&graph, &schedule, true);
        assert!(
            off.converged && on.converged,
            "a small nested loop converges"
        );
        assert_eq!(
            off.live_in_at(inner, stop),
            Some(true),
            "off: live into the inner header"
        );
        assert_eq!(
            on.live_in_at(inner, stop),
            Some(false),
            "on: not live into it"
        );
        // `carried` and the census's header set use the block-INDEX test
        // (a successor at or before its source); a frequency layout may
        // place the inner latch first, so those two are asked only when the
        // index test sees the inner header as one.
        let inner_is_indexed_header = schedule
            .blocks
            .iter()
            .enumerate()
            .any(|(p, b)| p >= inner && b.successors.contains(&inner));
        assert!(!on.carried[stop as usize], "on: spans no back edge");
        let r_on = on.range[stop as usize].expect("`stop` wants a location");
        assert_eq!(
            r_on.lo, r_on.hi,
            "on: no read, so the interval is the definition"
        );
        assert!(on.uses[stop as usize].is_empty(), "on: no use recorded");

        let census = token_trim_census_of(&graph, &schedule, &off, &on).expect("both converge");
        assert_eq!(
            census,
            token_trim_census(&graph, &schedule).expect("both converge")
        );
        assert!(census.trimmed >= 1);
        assert!(census.trimmed_value_typed >= 1, "{}", census.render());
        if off.carried[stop as usize] {
            assert!(census.carried_dropped >= 1, "{}", census.render());
        }
        if inner_is_indexed_header {
            assert!(census.header_live_in_dropped >= 1, "{}", census.render());
        }
        assert!(census.peak_live_on <= census.peak_live_off);
        assert!(
            census
                .examples
                .iter()
                .any(|&(id, ty, _, _)| id == stop && ty == IrType::Int),
            "{}",
            census.render()
        );
        assert!(census.render().starts_with("[ir-ls] token-trim census: "));
    }

    /// The switch only removes token reads, so a graph whose only token
    /// producers are memory-typed (no volatile read, no type check, no call)
    /// trims no value-typed interval; and the refactor keeps
    /// `build_live_model` equal to the explicit form of its own flag value.
    #[test]
    fn i63_l2_token_trim_census_is_quiet_without_value_typed_token_producers() {
        for (graph, schedule) in [swap_loop(), div_and_shift_graph()] {
            let census = token_trim_census(&graph, &schedule).expect("converges");
            assert_eq!(census.trimmed_value_typed, 0, "{}", census.render());

            let key = "CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES";
            for (value, explicit) in [(None, false), (Some("1"), true)] {
                let via_flag =
                    cratonvm_types::flags::with_thread_overrides(&[(key, value)], || {
                        build_live_model(&graph, &schedule)
                    });
                let direct = build_live_model_with_token_edges(&graph, &schedule, explicit);
                assert_eq!(via_flag.range, direct.range);
                assert_eq!(via_flag.uses, direct.uses);
                assert_eq!(via_flag.block_live_in, direct.block_live_in);
            }
        }
    }

    /// The soundness half of the proposal's risk: with the token reads gone
    /// the allocator still produces an allocation `verify_allocation`
    /// accepts, on the shape the switch shortens.
    #[test]
    fn i63_l2_a_trimmed_model_allocates_and_verifies() {
        let (graph, schedule, _, _) = nested_loop_with_a_token_read();
        let live = build_live_model_with_token_edges(&graph, &schedule, true);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// Interpreter round i2 wave 64 (lane L2): the slot-plan column renders
    /// LAST, after the wave-63 fields, and only when `ir_lower` filled it.
    #[test]
    fn i64_l2_the_census_line_carries_the_slot_plan_column_last() {
        let (graph, schedule, _, _) = nested_loop_with_a_token_read();
        let mut census = token_trim_census(&graph, &schedule).expect("both converge");
        let wave63 = census.render();
        assert!(!wave63.contains("slot-words="), "{wave63}");
        census.slot_plan = Some(TokenTrimSlotColumn {
            words_off: 9,
            words_on: 7,
            ref_words_off: 3,
            ref_words_on: 2,
            homes_trimmed: 1,
        });
        let wave64 = census.render();
        assert!(wave64.starts_with(&wave63), "{wave64}");
        assert!(
            wave64.ends_with(" slot-words=9->7 ref-words=3->2 homes-trimmed=1"),
            "{wave64}"
        );
    }

    /// The slot-plan switch implies the allocator's token-edge rule.
    #[test]
    fn i64_l2_the_slots_switch_trims_the_live_model_too() {
        let (graph, schedule, _, stop) = nested_loop_with_a_token_read();
        let key = "CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES";
        let via_flag = cratonvm_types::flags::with_thread_overrides(
            &[
                (key, Some("1")),
                ("CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES", None),
            ],
            || build_live_model(&graph, &schedule),
        );
        let on = build_live_model_with_token_edges(&graph, &schedule, true);
        assert_eq!(via_flag.range, on.range);
        assert!(
            via_flag.uses[stop as usize].is_empty(),
            "the token read is gone"
        );
    }

    // ── Interpreter round i2 wave 65, lane L2: one liveness core ─────────

    /// The test graphs the wave-65 core is compared on: a φ loop, a
    /// straight-line graph, and a nested loop with a value-typed memory token
    /// (memory φs, a token-only read).
    pub(crate) fn i65_l2_graphs() -> Vec<(Graph, ir_schedule::Schedule)> {
        let (graph, schedule, _, _) = nested_loop_with_a_token_read();
        vec![swap_loop(), div_and_shift_graph(), (graph, schedule)]
    }

    /// Every combination of the two rules, reads collected.
    pub(crate) fn i65_l2_rule_grid() -> Vec<LivenessRules> {
        let mut out = Vec::new();
        for token_edges_not_uses in [false, true] {
            for phi_edge_interfere in [false, true] {
                out.push(LivenessRules {
                    token_edges_not_uses,
                    phi_edge_interfere,
                    collect_reads: true,
                });
            }
        }
        out
    }

    /// Field-for-field equality of two models (`LiveModel` has no
    /// `PartialEq`).
    pub(crate) fn i65_l2_assert_live_models_equal(a: &LiveModel, b: &LiveModel, what: &str) {
        assert_eq!(a.pos_of, b.pos_of, "{what}: pos_of");
        assert_eq!(a.span, b.span, "{what}: span");
        assert_eq!(
            a.total_positions, b.total_positions,
            "{what}: total_positions"
        );
        assert_eq!(a.block_of_pos, b.block_of_pos, "{what}: block_of_pos");
        assert_eq!(a.wants_loc, b.wants_loc, "{what}: wants_loc");
        assert_eq!(a.range, b.range, "{what}: range");
        assert_eq!(a.class, b.class, "{what}: class");
        assert_eq!(a.is_ref, b.is_ref, "{what}: is_ref");
        assert_eq!(a.pinned, b.pinned, "{what}: pinned");
        assert_eq!(a.uses, b.uses, "{what}: uses");
        assert_eq!(a.weight, b.weight, "{what}: weight");
        assert_eq!(a.loop_depth, b.loop_depth, "{what}: loop_depth");
        assert_eq!(a.carried, b.carried, "{what}: carried");
        assert_eq!(
            a.carried_and_read, b.carried_and_read,
            "{what}: carried_and_read"
        );
        assert_eq!(a.peak_live, b.peak_live, "{what}: peak_live");
        assert_eq!(a.converged, b.converged, "{what}: converged");
        assert_eq!(a.block_live_in, b.block_live_in, "{what}: block_live_in");
    }

    /// The refactor is behaviour-identical: for every rule combination, the
    /// model built over `liveness_core` is the legacy model, field for field.
    #[test]
    fn i65_l2_the_core_live_model_is_the_legacy_live_model() {
        for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
            for rules in i65_l2_rule_grid() {
                let legacy = build_live_model_legacy(graph, schedule, rules);
                let core = build_live_model_from_core(graph, schedule, rules);
                assert!(legacy.converged, "graph {i}: a small graph converges");
                i65_l2_assert_live_models_equal(&legacy, &core, &format!("graph {i} {rules:?}"));
            }
        }
    }

    /// `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE` routes `build_live_model`
    /// through the core and changes nothing it answers.
    #[test]
    fn i65_l2_the_switch_routes_build_live_model_through_the_core() {
        let key = "CRATONVM_JIT_IR_SHARED_LIVENESS_CORE";
        for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
            let off = cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
                build_live_model(graph, schedule)
            });
            let on = cratonvm_types::flags::with_thread_overrides(&[(key, Some("1"))], || {
                build_live_model(graph, schedule)
            });
            i65_l2_assert_live_models_equal(&off, &on, &format!("graph {i}"));
        }
    }

    /// The core's read list is the legacy model's use list (as a set per
    /// value), and turning `collect_reads` off changes nothing but the list.
    #[test]
    fn i65_l2_the_core_reads_are_the_legacy_uses() {
        for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
            let wants = live_model_wants_loc(graph, schedule);
            for rules in i65_l2_rule_grid() {
                let core = liveness_core(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                );
                let legacy = build_live_model_legacy(graph, schedule, rules);
                let mut uses: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
                for &(v, at) in &core.reads {
                    uses[v as usize].push(at);
                }
                for list in &mut uses {
                    list.sort_unstable();
                    list.dedup();
                }
                assert_eq!(uses, legacy.uses, "graph {i} {rules:?}");

                let quiet = liveness_core(
                    graph,
                    schedule,
                    &wants,
                    LivenessRules {
                        collect_reads: false,
                        ..rules
                    },
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                );
                assert!(quiet.reads.is_empty());
                assert_eq!(
                    LivenessCore {
                        reads: Vec::new(),
                        ..core
                    },
                    quiet,
                    "graph {i} {rules:?}: collect_reads moves nothing else"
                );
            }
        }
    }

    /// The φ edge rule moves φ intervals only, only outward, and never the
    /// live-in/live-out sets; on the swap loop it moves at least one.
    #[test]
    fn i65_l2_the_phi_edge_rule_moves_only_phi_intervals() {
        let (graph, schedule) = swap_loop();
        let wants = live_model_wants_loc(&graph, &schedule);
        let base = LivenessRules::default();
        let off = liveness_core(
            &graph,
            &schedule,
            &wants,
            base,
            LIVE_MODEL_WORK_BUDGET,
            LIVE_MODEL_MAX_ITERATIONS,
        );
        let on = liveness_core(
            &graph,
            &schedule,
            &wants,
            LivenessRules {
                phi_edge_interfere: true,
                ..base
            },
            LIVE_MODEL_WORK_BUDGET,
            LIVE_MODEL_MAX_ITERATIONS,
        );
        assert!(off.converged && on.converged);
        assert_eq!(off.live_in, on.live_in);
        assert_eq!(off.live_out, on.live_out);
        assert_eq!(off.pos_of, on.pos_of);
        assert!(
            off.set_only_by_phi_rule.iter().all(|&b| !b),
            "no rule, no flag"
        );
        let mut moved = 0usize;
        for id in 0..graph.nodes.len() {
            if !matches!(graph.nodes[id].op, Op::Phi) {
                assert_eq!((off.lo[id], off.hi[id]), (on.lo[id], on.hi[id]), "n{id}");
                continue;
            }
            assert!(on.lo[id] <= off.lo[id] && on.hi[id] >= off.hi[id], "n{id}");
            if (on.lo[id], on.hi[id]) != (off.lo[id], off.hi[id]) {
                moved += 1;
            }
            if off.lo[id] != usize::MAX {
                assert!(
                    !on.set_only_by_phi_rule[id],
                    "n{id} was placed before the rule"
                );
            }
        }
        assert!(moved > 0, "a loop φ's interval reaches its entry edge");
    }

    /// Past the work budget nothing is analysed; out of iterations the fixed
    /// point is reported unconverged. Both are the callers' whole-method case.
    #[test]
    fn i65_l2_the_core_fallbacks() {
        let (graph, schedule) = swap_loop();
        let wants = live_model_wants_loc(&graph, &schedule);
        let rules = LivenessRules {
            token_edges_not_uses: false,
            phi_edge_interfere: true,
            collect_reads: true,
        };
        let skipped = liveness_core(
            &graph,
            &schedule,
            &wants,
            rules,
            0,
            LIVE_MODEL_MAX_ITERATIONS,
        );
        assert!(!skipped.converged);
        assert!(skipped.live_in.is_empty() && skipped.live_out.is_empty());
        assert!(skipped.reads.is_empty());
        assert!(skipped.lo.iter().all(|&l| l == usize::MAX));
        let full = build_live_model_legacy(&graph, &schedule, rules);
        assert_eq!(skipped.total_positions, full.total_positions);
        assert_eq!(skipped.span, full.span);

        let stuck = liveness_core(&graph, &schedule, &wants, rules, LIVE_MODEL_WORK_BUDGET, 0);
        assert!(!stuck.converged);
        assert_eq!(stuck.live_in.len(), stuck.words * schedule.blocks.len());
    }

    /// Compile-time bench probe (interpreter round i2 wave 65, lane L2): the
    /// legacy model vs the model over the shared core, min ns per build over
    /// 9 reps, to stderr. Not a pass/fail test.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i65_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i65_l2_bench_live_model_legacy_vs_core() {
        const REPS: usize = 9;
        const ITERS: u32 = 2000;
        let rules = LivenessRules {
            token_edges_not_uses: false,
            phi_edge_interfere: true,
            collect_reads: true,
        };
        let names = ["swap_loop", "div_and_shift", "nested_token"];
        for (name, (graph, schedule)) in names.iter().zip(i65_l2_graphs()) {
            let (mut best_legacy, mut best_core) = (f64::MAX, f64::MAX);
            // Interleaved, so a noisy stretch of the host hits both columns.
            for _ in 0..REPS {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..ITERS {
                    sink = sink.wrapping_add(
                        build_live_model_legacy(&graph, &schedule, rules).total_positions,
                    );
                }
                std::hint::black_box(sink);
                best_legacy = best_legacy.min(t0.elapsed().as_nanos() as f64 / f64::from(ITERS));

                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..ITERS {
                    sink = sink.wrapping_add(
                        build_live_model_from_core(&graph, &schedule, rules).total_positions,
                    );
                }
                std::hint::black_box(sink);
                best_core = best_core.min(t0.elapsed().as_nanos() as f64 / f64::from(ITERS));
            }
            eprintln!(
                "[i65-l2 bench] {name:<14} live-model legacy {best_legacy:>9.0} ns/op  core {best_core:>9.0} ns/op"
            );
        }
    }

    // ── Interpreter round i2 wave 66, lane L2: one liveness pass ─────────

    /// `slot_view` keeps every field the slot plan reads and drops the bit
    /// sets and the read list; `live_model_from_core` over a held core is
    /// `build_live_model_from_core`.
    #[test]
    fn i66_l2_a_held_core_serves_both_consumers() {
        for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
            let wants = live_model_wants_loc(graph, schedule);
            for rules in i65_l2_rule_grid() {
                let core = liveness_core(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                );
                let view = core.slot_view();
                assert_eq!(view.pos_of, core.pos_of, "graph {i}");
                assert_eq!(view.span, core.span, "graph {i}");
                assert_eq!(view.total_positions, core.total_positions, "graph {i}");
                assert_eq!(view.lo, core.lo, "graph {i}");
                assert_eq!(view.hi, core.hi, "graph {i}");
                assert_eq!(
                    view.set_only_by_phi_rule, core.set_only_by_phi_rule,
                    "graph {i}"
                );
                assert_eq!(view.converged, core.converged, "graph {i}");
                assert_eq!(view.words, core.words, "graph {i}");
                assert!(
                    view.live_in.is_empty() && view.live_out.is_empty() && view.reads.is_empty()
                );

                let held = live_model_from_core(graph, schedule, wants.clone(), core);
                let fresh = build_live_model_from_core(graph, schedule, rules);
                i65_l2_assert_live_models_equal(&held, &fresh, &format!("graph {i} {rules:?}"));
            }
        }
    }

    /// `live_model_rules` is the rule set `build_live_model` runs under.
    #[test]
    fn i66_l2_live_model_rules_reads_the_switches() {
        let cases: [(Option<&str>, Option<&str>, Option<&str>, bool, bool); 5] = [
            (None, None, None, false, true),
            (Some("1"), None, None, true, true),
            (None, Some("1"), None, true, true),
            (None, None, Some("0"), false, false),
            (Some("1"), Some("1"), Some("0"), true, false),
        ];
        for (ls, slots, interfere, token, phi) in cases {
            let rules = cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES", ls),
                    ("CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES", slots),
                    ("CRATONVM_JIT_IR_PHI_EDGE_INTERFERE", interfere),
                ],
                live_model_rules,
            );
            assert_eq!(
                rules,
                LivenessRules {
                    token_edges_not_uses: token,
                    phi_edge_interfere: phi,
                    collect_reads: true,
                },
                "{ls:?} {slots:?} {interfere:?}"
            );
        }
    }

    // ── Interpreter round i2 wave 67, lane L2: dense value numbering ─────

    /// `static int f(int n){ int s=0; for(i..n) s+=i; for(i..n) s+=i; ... return s; }`
    /// with `loops` sequential counted loops: a handful of blocks and a few
    /// dozen arena nodes (control, frame states, φs, values) per loop. The
    /// large synthetic graph of the wave-67 tests and bench.
    pub(crate) fn i67_l2_sequential_loops(loops: usize) -> (Graph, ir_schedule::Schedule) {
        let mut code: Vec<u8> = vec![0x03, 0x3c]; // iconst_0; istore_1   s = 0
        for _ in 0..loops {
            code.extend_from_slice(&[
                0x03, 0x3d, // +0: iconst_0; istore_2          i = 0
                0x1c, 0x1a, // +2: iload_2; iload_0            header
                0xa2, 0x00, 0x0d, // +4: if_icmpge +13 (to +17)
                0x1b, 0x1c, 0x60, 0x3c, // +7: iload_1; iload_2; iadd; istore_1
                0x84, 0x02, 0x01, // +11: iinc 2, 1
                0xa7, 0xff, 0xf4, // +14: goto -12 (to +2)
            ]);
        }
        code.extend_from_slice(&[0x1b, 0xac]); // iload_1; ireturn
        let len = code.len();
        code.extend_from_slice(&[0, 0]);
        let graph = IrBuilder::new(1, 4)
            .build(&code, len)
            .expect("the sequential loops build");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    /// `graph` with `pad` extra unscheduled constants appended: arena nodes
    /// no block places and no value set wants (dead nodes the scheduler left
    /// behind), i.e. the bits the dense numbering exists to drop.
    pub(crate) fn i67_l2_padded(
        (mut graph, schedule): (Graph, ir_schedule::Schedule),
        pad: usize,
    ) -> (Graph, ir_schedule::Schedule) {
        for i in 0..pad {
            let _ = graph.add(Op::Const(1_000_000 + i as i64), IrType::Int, vec![], None);
        }
        (graph, schedule)
    }

    /// The wave-65 test graphs plus the wave-67 synthetic ones: small and
    /// medium sequential loops, and two graphs mostly made of unwanted nodes.
    pub(crate) fn i67_l2_graphs() -> Vec<(&'static str, Graph, ir_schedule::Schedule)> {
        let mut out: Vec<(&'static str, Graph, ir_schedule::Schedule)> = Vec::new();
        let names = ["swap_loop", "div_and_shift", "nested_token"];
        for (name, (graph, schedule)) in names.iter().zip(i65_l2_graphs()) {
            out.push((*name, graph, schedule));
        }
        let (g, s) = i67_l2_sequential_loops(3);
        out.push(("loops3", g, s));
        let (g, s) = i67_l2_sequential_loops(20);
        out.push(("loops20", g, s));
        let (g, s) = i67_l2_padded(swap_loop(), 1000);
        out.push(("swap+1000dead", g, s));
        let (g, s) = i67_l2_padded(i67_l2_sequential_loops(5), 3000);
        out.push(("loops5+3000dead", g, s));
        out
    }

    /// Value sets to run the cores over: the live model's, a sparser subset
    /// of it, every node (with a `wants` longer than the arena), and none.
    fn i67_l2_value_sets(graph: &Graph, schedule: &ir_schedule::Schedule) -> Vec<Vec<bool>> {
        let loc = live_model_wants_loc(graph, schedule);
        let two_of_three: Vec<bool> = loc
            .iter()
            .enumerate()
            .map(|(i, &w)| w && i % 3 != 1)
            .collect();
        vec![
            loc,
            two_of_three,
            vec![true; graph.nodes.len() + 5],
            Vec::new(),
        ]
    }

    /// The default live-model rules, without reading any switch.
    fn i67_l2_model_rules() -> LivenessRules {
        LivenessRules {
            token_edges_not_uses: false,
            phi_edge_interfere: true,
            collect_reads: true,
        }
    }

    /// The refactor is byte-identical: the dense core's answer is the sparse
    /// core's, field for field (the bit sets mapped back to node ids), on
    /// every test graph, every rule combination with and without the read
    /// list, every value set, and the budget and iteration fallbacks,
    /// including fixed points cut off after one and two passes.
    #[test]
    fn i67_l2_the_dense_core_is_the_sparse_core() {
        let limits = [
            (LIVE_MODEL_WORK_BUDGET, LIVE_MODEL_MAX_ITERATIONS),
            (0, LIVE_MODEL_MAX_ITERATIONS),
            (LIVE_MODEL_WORK_BUDGET, 0),
            (LIVE_MODEL_WORK_BUDGET, 1),
            (LIVE_MODEL_WORK_BUDGET, 2),
        ];
        let mut compared = 0usize;
        for (name, graph, schedule) in i67_l2_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for base in i65_l2_rule_grid() {
                    for collect_reads in [true, false] {
                        let rules = LivenessRules {
                            collect_reads,
                            ..base
                        };
                        for (budget, iters) in limits {
                            let sparse = liveness_core_sparse(
                                &graph, &schedule, &wants, rules, budget, iters,
                            );
                            let dense = liveness_core_dense(
                                &graph, &schedule, &wants, rules, budget, iters, false,
                            );
                            assert_eq!(
                                sparse, dense,
                                "{name} {rules:?} budget {budget} iterations {iters}"
                            );
                            compared += 1;
                        }
                    }
                }
            }
        }
        assert!(compared > 0);
    }

    /// The full-budget runs above are real answers, not two empty ones: on
    /// every test graph the live model's value set converges, the published
    /// layout is node-indexed, and (outside the straight-line graph) some
    /// value is live into some block.
    #[test]
    fn i67_l2_the_compared_answers_are_converged_and_nonempty() {
        for (name, graph, schedule) in i67_l2_graphs() {
            let wants = live_model_wants_loc(&graph, &schedule);
            let dense = liveness_core_dense(
                &graph,
                &schedule,
                &wants,
                i67_l2_model_rules(),
                LIVE_MODEL_WORK_BUDGET,
                LIVE_MODEL_MAX_ITERATIONS,
                false,
            );
            assert!(dense.converged, "{name}");
            assert_eq!(dense.words, graph.nodes.len().div_ceil(64).max(1), "{name}");
            assert_eq!(
                dense.live_in.len(),
                dense.words * schedule.blocks.len(),
                "{name}"
            );
            assert_eq!(dense.live_out.len(), dense.live_in.len(), "{name}");
            if name != "div_and_shift" {
                assert!(
                    dense.live_in.iter().any(|&w| w != 0),
                    "{name}: something is live-in"
                );
            }
        }
    }

    /// The switches route `liveness_core` (and through it the live model)
    /// through the dense core and change nothing it answers on graphs within
    /// the budget.
    #[test]
    fn i67_l2_the_switches_route_the_core_and_change_nothing_within_budget() {
        const DENSE: &str = "CRATONVM_JIT_IR_LIVENESS_DENSE_VALUES";
        const BY_VALUES: &str = "CRATONVM_JIT_IR_LIVENESS_BUDGET_BY_VALUES";
        const CORE: &str = "CRATONVM_JIT_IR_SHARED_LIVENESS_CORE";
        type Edits = [(&'static str, Option<&'static str>); 2];
        let read = |edits: Edits| {
            cratonvm_types::flags::with_thread_overrides(&edits, || {
                (
                    liveness_dense_values_enabled(),
                    liveness_budget_by_values_enabled(),
                )
            })
        };
        assert_eq!(read([(DENSE, None), (BY_VALUES, None)]), (false, false));
        assert_eq!(read([(DENSE, Some("1")), (BY_VALUES, None)]), (true, false));
        assert_eq!(read([(DENSE, None), (BY_VALUES, Some("1"))]), (false, true));
        for (name, graph, schedule) in i67_l2_graphs() {
            let wants = live_model_wants_loc(&graph, &schedule);
            let rules = i67_l2_model_rules();
            let run = |edits: Edits| {
                cratonvm_types::flags::with_thread_overrides(&edits, || {
                    liveness_core(
                        &graph,
                        &schedule,
                        &wants,
                        rules,
                        LIVE_MODEL_WORK_BUDGET,
                        LIVE_MODEL_MAX_ITERATIONS,
                    )
                })
            };
            let off = run([(DENSE, None), (BY_VALUES, None)]);
            let sparse = liveness_core_sparse(
                &graph,
                &schedule,
                &wants,
                rules,
                LIVE_MODEL_WORK_BUDGET,
                LIVE_MODEL_MAX_ITERATIONS,
            );
            assert_eq!(off, sparse, "{name}: off is the sparse core");
            assert_eq!(
                off,
                run([(DENSE, Some("1")), (BY_VALUES, None)]),
                "{name}: dense"
            );
            assert_eq!(
                off,
                run([(DENSE, None), (BY_VALUES, Some("1"))]),
                "{name}: by values"
            );

            let model = |dense: Option<&'static str>| {
                cratonvm_types::flags::with_thread_overrides(
                    &[(CORE, Some("1")), (DENSE, dense), (BY_VALUES, None)],
                    || build_live_model(&graph, &schedule),
                )
            };
            i65_l2_assert_live_models_equal(&model(None), &model(Some("1")), name);
        }
    }

    /// `CRATONVM_JIT_IR_LIVENESS_BUDGET_BY_VALUES` is the one behaviour
    /// change: a graph whose arena is over the budget but whose value set is
    /// not gets the answer the sparse core gives it under an unlimited budget,
    /// where without the switch it gets the skipped (whole-method) answer.
    #[test]
    fn i67_l2_the_budget_by_values_admits_a_graph_over_the_node_budget() {
        let (graph, schedule) = i67_l2_padded(i67_l2_sequential_loops(5), 3000);
        let wants = live_model_wants_loc(&graph, &schedule);
        let rules = i67_l2_model_rules();
        let nb = schedule.blocks.len();
        let values = wants.iter().filter(|&&w| w).count();
        let n = graph.nodes.len();
        assert!(values * 10 < n, "the padded graph is mostly unwanted nodes");
        let budget = values * nb;
        assert!(budget > 0 && n * nb > budget);

        let skipped = liveness_core_sparse(&graph, &schedule, &wants, rules, budget, 256);
        assert!(
            !skipped.converged && skipped.live_in.is_empty(),
            "over the node budget"
        );
        let dense_by_nodes =
            liveness_core_dense(&graph, &schedule, &wants, rules, budget, 256, false);
        assert_eq!(skipped, dense_by_nodes, "without the switch, the same skip");

        let admitted = liveness_core_dense(&graph, &schedule, &wants, rules, budget, 256, true);
        let unlimited = liveness_core_sparse(&graph, &schedule, &wants, rules, usize::MAX, 256);
        assert!(admitted.converged);
        assert_eq!(admitted, unlimited, "the analysed answer is the sparse one");

        let one_short =
            liveness_core_dense(&graph, &schedule, &wants, rules, budget - 1, 256, true);
        assert!(
            !one_short.converged && one_short.live_in.is_empty(),
            "the budget still bites"
        );
    }

    /// Compile-time bench probe (interpreter round i2 wave 67, lane L2): the
    /// sparse core vs the dense core, ns per `liveness_core` run (live-model
    /// rules, read list kept, the live model's value set), min over 15
    /// interleaved reps, with `values / nodes` and the block count, to stderr.
    /// Not a pass/fail test beyond the two answers being equal.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i67_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i67_l2_bench_sparse_vs_dense_liveness_core() {
        const REPS: usize = 15;
        let rules = i67_l2_model_rules();
        let mut graphs = i67_l2_graphs();
        for (name, loops) in [("loops50", 50usize), ("loops150", 150), ("loops250", 250)] {
            let (g, s) = i67_l2_sequential_loops(loops);
            graphs.push((name, g, s));
        }
        let (g, s) = i67_l2_padded(i67_l2_sequential_loops(150), 20_000);
        graphs.push(("loops150+20000dead", g, s));
        for (name, graph, schedule) in &graphs {
            let wants = live_model_wants_loc(graph, schedule);
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let values = wants.iter().filter(|&&w| w).count();
            let sparse_run = || {
                liveness_core_sparse(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                )
            };
            let dense_run = || {
                liveness_core_dense(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                    false,
                )
            };
            let once = sparse_run();
            assert_eq!(once, dense_run(), "{name}");
            // Roughly the same amount of work per column per rep on every
            // graph, at least 3 runs.
            let iters = (20_000_000 / (n * nb / 64 + n + 1)).clamp(3, 20_000);
            let (mut best_sparse, mut best_dense) = (f64::MAX, f64::MAX);
            for _ in 0..REPS {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(sparse_run().live_in.len());
                }
                std::hint::black_box(sink);
                best_sparse = best_sparse.min(t0.elapsed().as_nanos() as f64 / iters as f64);

                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(dense_run().live_in.len());
                }
                std::hint::black_box(sink);
                best_dense = best_dense.min(t0.elapsed().as_nanos() as f64 / iters as f64);
            }
            eprintln!(
                "[i67-l2 bench] {name:<20} nodes {n:>6} values {values:>6} ({:>5.3}) blocks {nb:>5} \
                 converged {:<5} sparse {best_sparse:>12.0} ns/run dense {best_dense:>12.0} ns/run \
                 dense/sparse {:>5.3}",
                values as f64 / n.max(1) as f64,
                once.converged,
                best_dense / best_sparse,
            );
        }
    }

    // ── Interpreter round i2 wave 69, lane L2: the worklist fixed point ──

    /// A small deterministic generator (xorshift64*), so the random CFGs and
    /// sets are the same on every host.
    struct I69Rng(u64);

    impl I69Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() % n as u64) as usize
            }
        }
    }

    /// A synthetic CFG: block `b`'s successor list, blocks in layout order.
    type I69Cfg = Vec<Vec<usize>>;

    /// A typical method body: `segments` straight-line diamonds (if/else
    /// joins), every third segment a single-block-body loop instead.
    fn i69_l2_typical(segments: usize) -> I69Cfg {
        let mut succ: I69Cfg = vec![vec![1]];
        for k in 0..segments {
            let b = succ.len();
            if k % 3 == 2 {
                // header -> body, exit (the next segment); body -> header
                succ.push(vec![b + 1, b + 2]);
                succ.push(vec![b]);
            } else {
                // b -> b+1, b+2; both -> b+3 (the next segment)
                succ.push(vec![b + 1, b + 2]);
                succ.push(vec![b + 3]);
                succ.push(vec![b + 3]);
            }
        }
        succ.push(Vec::new()); // the return block
        succ
    }

    /// `depth` perfectly nested loops: headers `1..=depth`, the innermost
    /// body, then each loop's exit (innermost first), which branches back to
    /// the enclosing header, and the return block.
    fn i69_l2_loop_nest(depth: usize) -> I69Cfg {
        let d = depth.max(1);
        let header = |i: usize| 1 + i;
        let body = 1 + d;
        let exit = |i: usize| 2 + d + (d - 1 - i);
        let ret = 2 + 2 * d;
        let mut succ: I69Cfg = vec![Vec::new(); ret + 1];
        succ[0] = vec![header(0)];
        for i in 0..d {
            let inner = if i + 1 < d { header(i + 1) } else { body };
            succ[header(i)] = vec![inner, exit(i)];
            succ[exit(i)] = vec![if i == 0 { ret } else { header(i - 1) }];
        }
        succ[body] = vec![header(d - 1)];
        succ
    }

    /// A loop whose header is an `arms`-way switch: every arm joins, and the
    /// join branches back to the header.
    fn i69_l2_wide_switch(arms: usize) -> I69Cfg {
        let join = 2 + arms;
        let exit = join + 1;
        let mut succ: I69Cfg = vec![Vec::new(); exit + 1];
        succ[0] = vec![1];
        succ[1] = (2..2 + arms).chain([exit]).collect();
        for arm in succ.iter_mut().take(2 + arms).skip(2) {
            *arm = vec![join];
        }
        succ[join] = vec![1];
        succ
    }

    /// `regions` two-entry (irreducible) loops in sequence; with `handlers`,
    /// every block of the middle third also has an exception-style edge to
    /// one handler block laid out after the return block, which rejoins the
    /// return block (a retreating edge in layout order).
    fn i69_l2_irreducible(regions: usize, handlers: bool) -> I69Cfg {
        let mut succ: I69Cfg = vec![vec![1]];
        for _ in 0..regions {
            let c = succ.len();
            succ.push(vec![c + 1, c + 2]); // enter at a or at b
            succ.push(vec![c + 2, c + 3]); // a -> b, out
            succ.push(vec![c + 1, c + 3]); // b -> a, out
        }
        let ret = succ.len();
        succ.push(Vec::new());
        if handlers {
            let handler = succ.len();
            succ.push(vec![ret]);
            for b in succ.iter_mut().take(2 * ret / 3).skip(ret / 3) {
                b.push(handler);
            }
        }
        succ
    }

    /// A random CFG: 0 to 3 successors per block (sometimes 9, a switch),
    /// duplicate edges, self loops and out-of-range indices (which both fixed
    /// points ignore) included.
    fn i69_l2_random_cfg(rng: &mut I69Rng, nb: usize) -> I69Cfg {
        (0..nb)
            .map(|_| {
                let k = if rng.below(10) == 0 { 9 } else { rng.below(4) };
                (0..k).map(|_| rng.below(nb + 2)).collect::<Vec<usize>>()
            })
            .collect()
    }

    /// Random `(phi_out, uses, defs)` sets, `words` per block: sparse reads,
    /// denser definitions.
    fn i69_l2_random_sets(
        rng: &mut I69Rng,
        nb: usize,
        words: usize,
    ) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
        let len = nb * words;
        let phi_out: Vec<u64> = (0..len)
            .map(|_| rng.next() & rng.next() & rng.next() & rng.next())
            .collect();
        let uses: Vec<u64> = (0..len)
            .map(|_| rng.next() & rng.next() & rng.next())
            .collect();
        let defs: Vec<u64> = (0..len).map(|_| rng.next() & rng.next()).collect();
        (phi_out, uses, defs)
    }

    /// Run one fixed point from all-zero sets.
    fn i69_l2_run(
        succ: &I69Cfg,
        sets: LsFixedPointSets<'_>,
        max_iterations: usize,
        fixed_point: LivenessFixedPoint,
    ) -> (bool, Vec<u64>, Vec<u64>, usize) {
        let mut li = vec![0u64; sets.nb * sets.words];
        let mut lout = vec![0u64; sets.nb * sets.words];
        let mut visits = 0usize;
        let converged = match fixed_point {
            LivenessFixedPoint::RoundRobin | LivenessFixedPoint::RoundRobinDriver => {
                ls_fixed_point_round_robin(
                    succ.as_slice(),
                    sets,
                    &mut li,
                    &mut lout,
                    max_iterations,
                    &mut visits,
                )
            }
            LivenessFixedPoint::Worklist => ls_fixed_point_worklist(
                succ.as_slice(),
                sets,
                &mut li,
                &mut lout,
                max_iterations,
                &mut visits,
            ),
            // Wave 71: a bare list has no predecessor lists of its own; the
            // caller-built index stands in for the schedule's.
            LivenessFixedPoint::WorklistSchedulePreds => ls_fixed_point_worklist_preds(
                succ.as_slice(),
                &LsPredIndex::build(succ.as_slice(), sets.nb),
                sets,
                &mut li,
                &mut lout,
                max_iterations,
                &mut visits,
            ),
            LivenessFixedPoint::WorklistPostorder { .. } => {
                let (order, rank) = ls_postorder(succ.as_slice(), sets.nb);
                ls_fixed_point_worklist_ordered(
                    succ.as_slice(),
                    &LsPredIndex::build(succ.as_slice(), sets.nb),
                    &order,
                    &rank,
                    sets,
                    &mut li,
                    &mut lout,
                    max_iterations,
                    &mut visits,
                )
            }
        };
        (converged, li, lout, visits)
    }

    /// The two fixed points agree pass for pass on `succ`: for every cap from
    /// 0 to one past the round-robin pass count, the same verdict and the
    /// same `li`/`lout` (cut-off runs included), and the worklist never visits
    /// more blocks. Returns the round-robin pass count.
    fn i69_l2_assert_same_fixed_point(
        name: &str,
        succ: &I69Cfg,
        sets: LsFixedPointSets<'_>,
    ) -> usize {
        let nb = sets.nb;
        let (converged, _, _, visits) =
            i69_l2_run(succ, sets, 100_000, LivenessFixedPoint::RoundRobin);
        assert!(converged, "{name}: the round-robin fixed point settles");
        let passes = if nb == 0 { 1 } else { visits / nb };
        for cap in 0..=passes + 1 {
            let rr = i69_l2_run(succ, sets, cap, LivenessFixedPoint::RoundRobin);
            let wl = i69_l2_run(succ, sets, cap, LivenessFixedPoint::Worklist);
            assert_eq!(rr.0, wl.0, "{name} cap {cap}: converged");
            assert_eq!(rr.0, cap >= passes, "{name} cap {cap}: passes {passes}");
            assert_eq!(rr.1, wl.1, "{name} cap {cap}: live-in");
            assert_eq!(rr.2, wl.2, "{name} cap {cap}: live-out");
            assert!(wl.3 <= rr.3, "{name} cap {cap}: visits {} > {}", wl.3, rr.3);
            // Wave 71: the driver over a caller-built index (and the stack
            // dirty set below 65 blocks) is the per-call one, visits included.
            let wp = i69_l2_run(succ, sets, cap, LivenessFixedPoint::WorklistSchedulePreds);
            assert_eq!(wl, wp, "{name} cap {cap}: prebuilt predecessor index");
        }
        passes
    }

    #[test]
    fn i69_l2_highest_set_below() {
        let mut bits = vec![0u64; 3];
        assert_eq!(ls_highest_set_below(&bits, 192), None);
        for i in [0usize, 5, 63, 64, 127, 130] {
            ls_bit_set(&mut bits, i);
        }
        assert_eq!(ls_highest_set_below(&bits, 0), None);
        assert_eq!(ls_highest_set_below(&bits, 1), Some(0));
        assert_eq!(ls_highest_set_below(&bits, 5), Some(0));
        assert_eq!(ls_highest_set_below(&bits, 6), Some(5));
        assert_eq!(ls_highest_set_below(&bits, 64), Some(63));
        assert_eq!(ls_highest_set_below(&bits, 65), Some(64));
        assert_eq!(ls_highest_set_below(&bits, 127), Some(64));
        assert_eq!(ls_highest_set_below(&bits, 128), Some(127));
        assert_eq!(ls_highest_set_below(&bits, 131), Some(130));
        assert_eq!(ls_highest_set_below(&bits, 192), Some(130));
        // A limit past the words reads nothing out of range.
        assert_eq!(ls_highest_set_below(&bits, 1000), Some(130));
    }

    /// The shaped CFGs (typical, deep nest, wide switch, irreducible with and
    /// without exception-style edges) and 300 random ones, one- and
    /// multi-word sets: the worklist is the round-robin fixed point, pass for
    /// pass.
    #[test]
    fn i69_l2_the_worklist_is_the_round_robin_fixed_point_on_every_cfg_shape() {
        let mut rng = I69Rng(0x9E37_79B9_7F4A_7C15);
        let mut shaped: Vec<(String, I69Cfg)> = vec![
            ("empty".into(), Vec::new()),
            ("one block".into(), vec![Vec::new()]),
            ("self loop".into(), vec![vec![0, 1], vec![]]),
            ("typical".into(), i69_l2_typical(12)),
            ("nest8".into(), i69_l2_loop_nest(8)),
            ("switch40".into(), i69_l2_wide_switch(40)),
            ("irreducible".into(), i69_l2_irreducible(6, false)),
            ("irreducible+handlers".into(), i69_l2_irreducible(6, true)),
        ];
        for i in 0..300 {
            let nb = 1 + rng.below(70);
            shaped.push((format!("random{i}"), i69_l2_random_cfg(&mut rng, nb)));
        }
        let mut multi_pass = 0usize;
        for (name, succ) in &shaped {
            let nb = succ.len();
            for words in [1usize, 3] {
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out,
                    uses: &uses,
                    defs: &defs,
                };
                if i69_l2_assert_same_fixed_point(name, succ, sets) > 2 {
                    multi_pass += 1;
                }
            }
        }
        assert!(
            multi_pass > 10,
            "the grid exercises graphs that need several passes"
        );
    }

    /// On every scheduled test graph, value set, rule combination, budget
    /// and iteration cap (0 to 3 passes and the real cap), sparse and dense:
    /// the core with the worklist is the round-robin core, field for field,
    /// and the round-robin core is the pre-wave-69 one.
    #[test]
    fn i69_l2_the_worklist_core_is_the_round_robin_core() {
        let limits = [
            (LIVE_MODEL_WORK_BUDGET, LIVE_MODEL_MAX_ITERATIONS),
            (0, LIVE_MODEL_MAX_ITERATIONS),
            (LIVE_MODEL_WORK_BUDGET, 0),
            (LIVE_MODEL_WORK_BUDGET, 1),
            (LIVE_MODEL_WORK_BUDGET, 2),
            (LIVE_MODEL_WORK_BUDGET, 3),
        ];
        let mut compared = 0usize;
        for (name, graph, schedule) in i67_l2_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for base in i65_l2_rule_grid() {
                    for collect_reads in [true, false] {
                        let rules = LivenessRules {
                            collect_reads,
                            ..base
                        };
                        for (budget, iters) in limits {
                            for dense in [false, true] {
                                let what = format!(
                                    "{name} {rules:?} budget {budget} iterations {iters} dense {dense}"
                                );
                                let (rr, rr_visits) = liveness_core_with(
                                    &graph,
                                    &schedule,
                                    &wants,
                                    rules,
                                    budget,
                                    iters,
                                    dense,
                                    false,
                                    LivenessFixedPoint::RoundRobin,
                                );
                                let (wl, wl_visits) = liveness_core_with(
                                    &graph,
                                    &schedule,
                                    &wants,
                                    rules,
                                    budget,
                                    iters,
                                    dense,
                                    false,
                                    LivenessFixedPoint::Worklist,
                                );
                                assert_eq!(rr, wl, "{what}");
                                assert!(wl_visits <= rr_visits, "{what}");
                                let before = if dense {
                                    liveness_core_dense(
                                        &graph, &schedule, &wants, rules, budget, iters, false,
                                    )
                                } else {
                                    liveness_core_sparse(
                                        &graph, &schedule, &wants, rules, budget, iters,
                                    )
                                };
                                assert_eq!(rr, before, "{what}: the refactored round robin");
                                compared += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(compared > 0);
    }

    /// The worklist does save visits where the proposal said it would: on
    /// sequential loops it visits fewer blocks than whole passes do.
    #[test]
    fn i69_l2_the_worklist_skips_settled_blocks_on_sequential_loops() {
        let (graph, schedule) = i67_l2_sequential_loops(20);
        let wants = live_model_wants_loc(&graph, &schedule);
        let run = |fixed_point| {
            liveness_core_with(
                &graph,
                &schedule,
                &wants,
                i67_l2_model_rules(),
                LIVE_MODEL_WORK_BUDGET,
                LIVE_MODEL_MAX_ITERATIONS,
                false,
                false,
                fixed_point,
            )
        };
        let (rr, rr_visits) = run(LivenessFixedPoint::RoundRobin);
        let (wl, wl_visits) = run(LivenessFixedPoint::Worklist);
        assert!(rr.converged);
        assert_eq!(rr, wl);
        let nb = schedule.blocks.len();
        assert!(
            rr_visits >= 2 * nb,
            "at least the settling pass and the check pass"
        );
        assert!(wl_visits < rr_visits, "{wl_visits} < {rr_visits}");
    }

    /// `CRATONVM_JIT_IR_LIVENESS_WORKLIST` is off by default, routes
    /// `liveness_core` (alone and with the dense numbering) and changes
    /// nothing it answers, nor the live model built on it.
    #[test]
    fn i69_l2_the_switch_routes_the_core_and_changes_nothing() {
        const WL: &str = "CRATONVM_JIT_IR_LIVENESS_WORKLIST";
        const DENSE: &str = "CRATONVM_JIT_IR_LIVENESS_DENSE_VALUES";
        const CORE: &str = "CRATONVM_JIT_IR_SHARED_LIVENESS_CORE";
        let read = |v: Option<&'static str>| {
            cratonvm_types::flags::with_thread_overrides(&[(WL, v)], liveness_worklist_enabled)
        };
        assert!(!read(None));
        assert!(read(Some("1")));
        for (name, graph, schedule) in i67_l2_graphs() {
            let wants = live_model_wants_loc(&graph, &schedule);
            let rules = i67_l2_model_rules();
            let run = |wl: Option<&'static str>, dense: Option<&'static str>| {
                cratonvm_types::flags::with_thread_overrides(&[(WL, wl), (DENSE, dense)], || {
                    liveness_core(
                        &graph,
                        &schedule,
                        &wants,
                        rules,
                        LIVE_MODEL_WORK_BUDGET,
                        LIVE_MODEL_MAX_ITERATIONS,
                    )
                })
            };
            let off = run(None, None);
            assert_eq!(off, run(Some("1"), None), "{name}: worklist");
            assert_eq!(off, run(Some("1"), Some("1")), "{name}: worklist + dense");

            let model = |wl: Option<&'static str>| {
                cratonvm_types::flags::with_thread_overrides(&[(CORE, Some("1")), (WL, wl)], || {
                    build_live_model(&graph, &schedule)
                })
            };
            i65_l2_assert_live_models_equal(&model(None), &model(Some("1")), name);
        }
    }

    /// The iteration caps the `i70_l2_*` tests run every legacy copy under:
    /// none, one to three passes (cut-off runs on every looping graph), and
    /// the real cap.
    pub(crate) const I70_L2_CAPS: [usize; 5] = [0, 1, 2, 3, LIVE_MODEL_MAX_ITERATIONS];

    /// The φ-free transfer (the chain copy's, `ir_lower::chain_read_colour_ranges`):
    /// with an all-zero φ-out set the worklist is still the round-robin fixed
    /// point, pass for pass, on every shaped CFG and 100 random ones.
    #[test]
    fn i70_l2_the_worklist_is_the_round_robin_fixed_point_without_phi_out() {
        let mut rng = I69Rng(0x2545_F491_4F6C_DD1D);
        let mut shaped: Vec<(String, I69Cfg)> = vec![
            ("empty".into(), Vec::new()),
            ("self loop".into(), vec![vec![0, 1], vec![]]),
            ("typical".into(), i69_l2_typical(12)),
            ("nest8".into(), i69_l2_loop_nest(8)),
            ("switch40".into(), i69_l2_wide_switch(40)),
            ("irreducible+handlers".into(), i69_l2_irreducible(6, true)),
        ];
        for i in 0..100 {
            let nb = 1 + rng.below(70);
            shaped.push((format!("random{i}"), i69_l2_random_cfg(&mut rng, nb)));
        }
        let mut multi_pass = 0usize;
        for (name, succ) in &shaped {
            let nb = succ.len();
            for words in [1usize, 2] {
                let (_, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let zeros = vec![0u64; nb * words];
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &zeros,
                    uses: &uses,
                    defs: &defs,
                };
                if i69_l2_assert_same_fixed_point(name, succ, sets) > 2 {
                    multi_pass += 1;
                }
            }
        }
        assert!(
            multi_pass > 5,
            "the grid exercises graphs that need several passes"
        );
    }

    /// `build_live_model_legacy`'s fixed point through the worklist is its own
    /// whole-pass loop: on every test graph, rule combination and iteration
    /// cap (cut-off runs included), the same live-in, live-out and verdict,
    /// never more block visits, and the same live model, field for field.
    #[test]
    fn i70_l2_the_legacy_live_model_worklist_is_its_round_robin() {
        let mut cut_off = 0usize;
        let mut compared = 0usize;
        for (name, graph, schedule) in i67_l2_graphs() {
            for rules in i65_l2_rule_grid() {
                for cap in I70_L2_CAPS {
                    let what = format!("{name} {rules:?} cap {cap}");
                    let run = |fixed_point| {
                        let mut trace = LsLegacyRun::default();
                        let model = build_live_model_legacy_with(
                            &graph,
                            &schedule,
                            rules,
                            cap,
                            fixed_point,
                            Some(&mut trace),
                        );
                        (model, trace)
                    };
                    let (rr, rr_trace) = run(LivenessFixedPoint::RoundRobin);
                    let (wl, wl_trace) = run(LivenessFixedPoint::Worklist);
                    assert_eq!(rr_trace.answer(), wl_trace.answer(), "{what}");
                    assert!(wl_trace.visits <= rr_trace.visits, "{what}: visits");
                    i65_l2_assert_live_models_equal(&rr, &wl, &what);
                    if rr_trace.ran && !rr_trace.converged {
                        cut_off += 1;
                    }
                    if cap == LIVE_MODEL_MAX_ITERATIONS {
                        // The real cap: the explicit round robin is what
                        // `build_live_model_legacy` answers with the switch off.
                        let default = cratonvm_types::flags::with_thread_overrides(
                            &[("CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST", None)],
                            || build_live_model_legacy(&graph, &schedule, rules),
                        );
                        i65_l2_assert_live_models_equal(&rr, &default, &what);
                    }
                    compared += 1;
                }
            }
        }
        assert!(compared > 0);
        assert!(cut_off > 0, "some run is cut off by the cap");
    }

    /// `CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST` is off by default, routes
    /// `build_live_model`'s legacy copy, and changes nothing it answers; it
    /// does not touch the shared core (which `CRATONVM_JIT_IR_LIVENESS_WORKLIST`
    /// routes).
    #[test]
    fn i70_l2_the_legacy_switch_routes_the_live_model_and_changes_nothing() {
        const KEY: &str = "CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST";
        const CORE: &str = "CRATONVM_JIT_IR_SHARED_LIVENESS_CORE";
        let read = |v: Option<&'static str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    (KEY, v),
                    ("CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS", None),
                    ("CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS", None),
                    ("CRATONVM_JIT_IR_LIVENESS_POSTORDER", None),
                ],
                || {
                    (
                        legacy_liveness_worklist_enabled(),
                        legacy_liveness_fixed_point(1),
                    )
                },
            )
        };
        assert_eq!(read(None), (false, LivenessFixedPoint::RoundRobin));
        assert_eq!(read(Some("1")), (true, LivenessFixedPoint::Worklist));
        let core_untouched = cratonvm_types::flags::with_thread_overrides(
            &[
                (KEY, Some("1")),
                ("CRATONVM_JIT_IR_LIVENESS_WORKLIST", None),
            ],
            liveness_worklist_enabled,
        );
        assert!(!core_untouched);
        for (name, graph, schedule) in i67_l2_graphs() {
            let model = |v: Option<&'static str>| {
                cratonvm_types::flags::with_thread_overrides(&[(CORE, None), (KEY, v)], || {
                    build_live_model(&graph, &schedule)
                })
            };
            i65_l2_assert_live_models_equal(&model(None), &model(Some("1")), name);
        }
    }

    // ── Interpreter round i3 wave 71, lane L2: the block-count gate and the
    //    schedule's own predecessor lists ────────────────────────────────

    const I71_MIN_BLOCKS: &str = "CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS";
    const I71_SCHEDULE_PREDS: &str = "CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS";
    const I71_POSTORDER: &str = "CRATONVM_JIT_IR_LIVENESS_POSTORDER";

    /// The scheduled graphs the wave-71 tests run over: the wave-67 test
    /// graphs, sequential loops of several lengths, and the same loops
    /// scheduled with the hot-path layout (which remaps both edge lists).
    pub(crate) fn i71_l2_scheduled_graphs() -> Vec<(String, Graph, ir_schedule::Schedule)> {
        let mut out: Vec<(String, Graph, ir_schedule::Schedule)> = i67_l2_graphs()
            .into_iter()
            .map(|(name, g, s)| (name.to_string(), g, s))
            .collect();
        for loops in [1usize, 2, 8, 30] {
            let (g, s) = i67_l2_sequential_loops(loops);
            out.push((format!("loops{loops}"), g, s));
            let (g, _) = i67_l2_sequential_loops(loops);
            let opts = ir_schedule::ScheduleOptions {
                layout_hot_paths: true,
                ..Default::default()
            };
            let s = ir_schedule::schedule_with_options(&g, &opts);
            out.push((format!("loops{loops} hot layout"), g, s));
        }
        out
    }

    /// `CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS` parses as a block count
    /// (unset, empty, `0` and junk are no gate), and with
    /// `CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS` picks the fixed point a
    /// worklist switch runs; the legacy pick is the round robin whenever its
    /// own switch is off.
    #[test]
    fn i71_l2_the_gate_and_the_preds_switch_pick_the_fixed_point() {
        let min = |v: Option<&'static str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[(I71_MIN_BLOCKS, v)],
                liveness_worklist_min_blocks,
            )
        };
        assert_eq!(min(None), 0);
        assert_eq!(min(Some("")), 0);
        assert_eq!(min(Some("junk")), 0);
        assert_eq!(min(Some("-3")), 0);
        assert_eq!(min(Some("0")), 0);
        assert_eq!(min(Some(" 40 ")), 40);
        let preds = |v: Option<&'static str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[(I71_SCHEDULE_PREDS, v)],
                liveness_schedule_preds_enabled,
            )
        };
        assert!(!preds(None));
        assert!(preds(Some("1")));

        let pick = |m: Option<&'static str>, p: Option<&'static str>, nb: usize| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    (I71_MIN_BLOCKS, m),
                    (I71_SCHEDULE_PREDS, p),
                    (I71_POSTORDER, None),
                ],
                || worklist_fixed_point_for(nb),
            )
        };
        assert_eq!(pick(None, None, 0), LivenessFixedPoint::Worklist);
        assert_eq!(pick(None, None, 3), LivenessFixedPoint::Worklist);
        assert_eq!(pick(Some("40"), None, 39), LivenessFixedPoint::RoundRobin);
        assert_eq!(pick(Some("40"), None, 40), LivenessFixedPoint::Worklist);
        assert_eq!(
            pick(None, Some("1"), 3),
            LivenessFixedPoint::WorklistSchedulePreds
        );
        assert_eq!(
            pick(Some("40"), Some("1"), 39),
            LivenessFixedPoint::RoundRobin
        );
        assert_eq!(
            pick(Some("40"), Some("1"), 400),
            LivenessFixedPoint::WorklistSchedulePreds
        );

        let legacy = |l: Option<&'static str>, m: Option<&'static str>, nb: usize| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST", l),
                    (I71_MIN_BLOCKS, m),
                    (I71_SCHEDULE_PREDS, None),
                    (I71_POSTORDER, None),
                ],
                || legacy_liveness_fixed_point(nb),
            )
        };
        assert_eq!(legacy(None, None, 100), LivenessFixedPoint::RoundRobin);
        assert_eq!(legacy(None, Some("1"), 100), LivenessFixedPoint::RoundRobin);
        assert_eq!(
            legacy(Some("1"), Some("40"), 10),
            LivenessFixedPoint::RoundRobin
        );
        assert_eq!(
            legacy(Some("1"), Some("40"), 40),
            LivenessFixedPoint::Worklist
        );

        let postorder = |m: Option<&'static str>, p: Option<&'static str>, nb: usize| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    (I71_MIN_BLOCKS, m),
                    (I71_SCHEDULE_PREDS, p),
                    (I71_POSTORDER, Some("1")),
                ],
                || (liveness_postorder_enabled(), worklist_fixed_point_for(nb)),
            )
        };
        assert_eq!(
            postorder(None, None, 3),
            (
                true,
                LivenessFixedPoint::WorklistPostorder {
                    schedule_preds: false
                }
            )
        );
        assert_eq!(
            postorder(None, Some("1"), 3),
            (
                true,
                LivenessFixedPoint::WorklistPostorder {
                    schedule_preds: true
                }
            )
        );
        assert_eq!(
            postorder(Some("40"), Some("1"), 39),
            (true, LivenessFixedPoint::RoundRobin)
        );
        let postorder_off = cratonvm_types::flags::with_thread_overrides(
            &[(I71_POSTORDER, None)],
            liveness_postorder_enabled,
        );
        assert!(!postorder_off);
    }

    /// The invariant `CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS` rests on: on
    /// every schedule `ir_schedule` builds (hot-path layout included), each
    /// block's own predecessor set is the inverted in-range successor lists.
    #[test]
    fn i71_l2_schedule_predecessors_are_the_inverted_successors() {
        let mut edges = 0usize;
        for (name, _graph, schedule) in i71_l2_scheduled_graphs() {
            let blocks = schedule.blocks.as_slice();
            let nb = blocks.len();
            let index = LsPredIndex::build(blocks, nb);
            let own = LsSchedulePreds(blocks);
            for b in 0..nb {
                let mut inverted: Vec<usize> = index.ls_predecessors(b).to_vec();
                inverted.sort_unstable();
                inverted.dedup();
                let mut listed: Vec<usize> = own
                    .ls_predecessors(b)
                    .iter()
                    .copied()
                    .filter(|&p| p < nb)
                    .collect();
                listed.sort_unstable();
                listed.dedup();
                assert_eq!(inverted, listed, "{name}: block {b}");
                edges += inverted.len();
            }
        }
        assert!(edges > 0);
    }

    /// The worklist over the schedule's own predecessor lists is the
    /// per-call-index worklist, visit for visit, and so the round robin's
    /// answer: in the shared core (every value set, rule combination and cap),
    /// in the live model's legacy copy and in `ir_lower`'s three legacy copies.
    #[test]
    fn i71_l2_the_schedule_preds_worklist_is_the_per_call_worklist() {
        let mut compared = 0usize;
        for (name, graph, schedule) in i71_l2_scheduled_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for rules in i65_l2_rule_grid() {
                    for cap in I70_L2_CAPS {
                        let run = |fixed_point| {
                            liveness_core_with(
                                &graph,
                                &schedule,
                                &wants,
                                rules,
                                LIVE_MODEL_WORK_BUDGET,
                                cap,
                                false,
                                false,
                                fixed_point,
                            )
                        };
                        let what = format!("{name} {rules:?} cap {cap}");
                        let rr = run(LivenessFixedPoint::RoundRobin);
                        let wl = run(LivenessFixedPoint::Worklist);
                        let wp = run(LivenessFixedPoint::WorklistSchedulePreds);
                        assert_eq!(rr.0, wp.0, "{what}: core");
                        assert_eq!(wl, wp, "{what}: core visits");
                        compared += 1;
                    }
                }
            }
            for rules in i65_l2_rule_grid() {
                for cap in I70_L2_CAPS {
                    let what = format!("{name} {rules:?} cap {cap}");
                    let run = |fixed_point| {
                        let mut trace = LsLegacyRun::default();
                        let model = build_live_model_legacy_with(
                            &graph,
                            &schedule,
                            rules,
                            cap,
                            fixed_point,
                            Some(&mut trace),
                        );
                        (model, trace)
                    };
                    let (rr, rr_trace) = run(LivenessFixedPoint::RoundRobin);
                    let (_, wl_trace) = run(LivenessFixedPoint::Worklist);
                    let (wp, wp_trace) = run(LivenessFixedPoint::WorklistSchedulePreds);
                    assert_eq!(rr_trace.answer(), wp_trace.answer(), "{what}: live model");
                    assert_eq!(wl_trace, wp_trace, "{what}: live model visits");
                    i65_l2_assert_live_models_equal(&rr, &wp, &what);
                    compared += 1;
                }
            }
            for (copy, run) in crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                &graph, &schedule,
            ) {
                for cap in I70_L2_CAPS {
                    let rr = run(LivenessFixedPoint::RoundRobin, cap);
                    let wl = run(LivenessFixedPoint::Worklist, cap);
                    let wp = run(LivenessFixedPoint::WorklistSchedulePreds, cap);
                    assert_eq!(rr.answer(), wp.answer(), "{name} {copy} cap {cap}");
                    assert_eq!(wl, wp, "{name} {copy} cap {cap}: visits");
                    compared += 1;
                }
            }
        }
        assert!(compared > 0);
    }

    /// The two wave-71 switches route `liveness_core` and `build_live_model`
    /// (both worklist switches on) and change nothing either answers, gate
    /// below, at and above the graph's block count.
    #[test]
    fn i71_l2_the_gate_and_the_preds_switch_change_nothing() {
        const WL: &str = "CRATONVM_JIT_IR_LIVENESS_WORKLIST";
        const LEGACY: &str = "CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST";
        const CORE: &str = "CRATONVM_JIT_IR_SHARED_LIVENESS_CORE";
        for (name, graph, schedule) in i71_l2_scheduled_graphs() {
            let nb = schedule.blocks.len();
            let wants = live_model_wants_loc(&graph, &schedule);
            let rules = i67_l2_model_rules();
            let gates = [None, Some(nb.to_string()), Some((nb + 1).to_string())];
            let off_core = cratonvm_types::flags::with_thread_overrides(
                &[
                    (WL, None),
                    (I71_MIN_BLOCKS, None),
                    (I71_SCHEDULE_PREDS, None),
                ],
                || {
                    liveness_core(
                        &graph,
                        &schedule,
                        &wants,
                        rules,
                        LIVE_MODEL_WORK_BUDGET,
                        LIVE_MODEL_MAX_ITERATIONS,
                    )
                },
            );
            let off_models: Vec<LiveModel> = [None, Some("1")]
                .into_iter()
                .map(|core| {
                    cratonvm_types::flags::with_thread_overrides(
                        &[
                            (CORE, core),
                            (WL, None),
                            (LEGACY, None),
                            (I71_MIN_BLOCKS, None),
                            (I71_SCHEDULE_PREDS, None),
                            (I71_POSTORDER, None),
                        ],
                        || build_live_model(&graph, &schedule),
                    )
                })
                .collect();
            // Every graph here converges well inside the cap, so the postorder
            // (which may change only a cut-off run) changes nothing either.
            for gate in &gates {
                for (preds, postorder) in [
                    (None, None),
                    (Some("1"), None),
                    (None, Some("1")),
                    (Some("1"), Some("1")),
                ] {
                    let what =
                        format!("{name} gate {gate:?} preds {preds:?} postorder {postorder:?}");
                    let edits = |core: Option<&'static str>| {
                        vec![
                            (CORE, core),
                            (WL, Some("1")),
                            (LEGACY, Some("1")),
                            (I71_MIN_BLOCKS, gate.as_deref()),
                            (I71_SCHEDULE_PREDS, preds),
                            (I71_POSTORDER, postorder),
                        ]
                    };
                    let on_core =
                        cratonvm_types::flags::with_thread_overrides(&edits(None), || {
                            liveness_core(
                                &graph,
                                &schedule,
                                &wants,
                                rules,
                                LIVE_MODEL_WORK_BUDGET,
                                LIVE_MODEL_MAX_ITERATIONS,
                            )
                        });
                    assert_eq!(off_core, on_core, "{what}: core");
                    for (i, core) in [None, Some("1")].into_iter().enumerate() {
                        let on = cratonvm_types::flags::with_thread_overrides(&edits(core), || {
                            build_live_model(&graph, &schedule)
                        });
                        i65_l2_assert_live_models_equal(&off_models[i], &on, &what);
                    }
                }
            }
        }
    }

    /// The shaped CFGs and 100 random ones the postorder tests run over.
    fn i71_l2_cfgs(rng: &mut I69Rng) -> Vec<(String, I69Cfg)> {
        let mut out: Vec<(String, I69Cfg)> = vec![
            ("empty".into(), Vec::new()),
            ("one".into(), vec![Vec::new()]),
            ("self loop".into(), vec![vec![0, 1], vec![]]),
            ("typical".into(), i69_l2_typical(12)),
            ("nest8".into(), i69_l2_loop_nest(8)),
            ("switch40".into(), i69_l2_wide_switch(40)),
            ("irreducible".into(), i69_l2_irreducible(6, false)),
            ("irreducible+handlers".into(), i69_l2_irreducible(6, true)),
            ("reversed chain".into(), i71_l2_reversed_chain(30)),
        ];
        for i in 0..100 {
            let nb = 1 + rng.below(90);
            out.push((format!("random{i}"), i69_l2_random_cfg(rng, nb)));
        }
        out
    }

    /// A chain laid out backwards: the entry (block 0) jumps to the last
    /// block, each block `b >= 2` falls to `b - 1`, block 1 returns. Every
    /// edge but the entry's retreats in index order, so the index-order
    /// passes move a live value one block per pass; in postorder it is one
    /// pass.
    fn i71_l2_reversed_chain(nb: usize) -> I69Cfg {
        let nb = nb.max(3);
        let mut succ: I69Cfg = vec![Vec::new(); nb];
        succ[0] = vec![nb - 1];
        for (b, s) in succ.iter_mut().enumerate().skip(2) {
            *s = vec![b - 1];
        }
        succ
    }

    /// `ls_postorder` is a permutation with `rank` its inverse, and on a DAG
    /// every edge goes from a lower rank to a higher one (the driver visits
    /// the higher rank first: successors before predecessors).
    #[test]
    fn i71_l2_postorder_is_a_permutation_and_orders_dag_edges() {
        let mut rng = I69Rng(0x0123_4567_89AB_CDEF);
        for (name, succ) in i71_l2_cfgs(&mut rng) {
            let nb = succ.len();
            let (order, rank) = ls_postorder(succ.as_slice(), nb);
            let mut seen = vec![false; nb];
            for k in 0..nb {
                assert!(!seen[order[k]], "{name}: block {} twice", order[k]);
                seen[order[k]] = true;
                assert_eq!(rank[order[k]], k, "{name}: rank is the inverse");
            }
        }
        for nb in [1usize, 2, 5, 17, 64, 65, 130] {
            let dag: I69Cfg = (0..nb)
                .map(|b| {
                    let picks = rng.below(4);
                    (0..picks)
                        .filter_map(|_| {
                            let room = nb - b - 1;
                            (room > 0).then(|| b + 1 + rng.below(room))
                        })
                        .collect::<Vec<usize>>()
                })
                .collect();
            let (_, rank) = ls_postorder(dag.as_slice(), nb);
            for (b, ss) in dag.iter().enumerate() {
                for &s in ss {
                    assert!(rank[s] > rank[b], "dag {nb}: edge {b} -> {s}");
                }
            }
        }
    }

    /// The ordered driver with the identity order is the index-order worklist
    /// at every cap, visits included; with the postorder it converges to the
    /// round robin's sets (the least fixed point) on every CFG, with and
    /// without a φ-out term.
    #[test]
    fn i71_l2_the_postorder_worklist_converges_to_the_round_robin() {
        let mut rng = I69Rng(0x5DEE_CE66_D1CE_4E5B);
        let mut fewer = 0usize;
        for (name, succ) in i71_l2_cfgs(&mut rng) {
            let nb = succ.len();
            for words in [1usize, 2] {
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let zeros = vec![0u64; nb * words];
                for phi in [&phi_out, &zeros] {
                    let sets = LsFixedPointSets {
                        nb,
                        words,
                        phi_out: phi,
                        uses: &uses,
                        defs: &defs,
                    };
                    let rr = i69_l2_run(&succ, sets, 100_000, LivenessFixedPoint::RoundRobin);
                    assert!(rr.0, "{name}: the round robin settles");
                    let passes = if nb == 0 { 1 } else { rr.3 / nb };
                    let identity: Vec<usize> = (0..nb).collect();
                    let index = LsPredIndex::build(succ.as_slice(), nb);
                    for cap in 0..=passes + 1 {
                        let wl = i69_l2_run(&succ, sets, cap, LivenessFixedPoint::Worklist);
                        let mut li = vec![0u64; nb * words];
                        let mut lout = vec![0u64; nb * words];
                        let mut visits = 0usize;
                        let converged = ls_fixed_point_worklist_ordered(
                            succ.as_slice(),
                            &index,
                            &identity,
                            &identity,
                            sets,
                            &mut li,
                            &mut lout,
                            cap,
                            &mut visits,
                        );
                        assert_eq!(
                            wl,
                            (converged, li, lout, visits),
                            "{name} cap {cap}: identity"
                        );
                    }
                    let po = i69_l2_run(
                        &succ,
                        sets,
                        100_000,
                        LivenessFixedPoint::WorklistPostorder {
                            schedule_preds: false,
                        },
                    );
                    assert!(po.0, "{name}: the postorder settles");
                    assert_eq!(
                        (&rr.1, &rr.2),
                        (&po.1, &po.2),
                        "{name}: the least fixed point"
                    );
                    if po.3 < rr.3 {
                        fewer += 1;
                    }
                }
            }
        }
        assert!(fewer > 0);

        // The reversed chain: one value read in the return block, defined
        // nowhere. Index order: about a pass per block; postorder: two.
        let nb = 40;
        let succ = i71_l2_reversed_chain(nb);
        let mut uses = vec![0u64; nb];
        uses[1] = 1;
        let zeros = vec![0u64; nb];
        let sets = LsFixedPointSets {
            nb,
            words: 1,
            phi_out: &zeros,
            uses: &uses,
            defs: &zeros,
        };
        let rr = i69_l2_run(&succ, sets, 100_000, LivenessFixedPoint::RoundRobin);
        let po = i69_l2_run(
            &succ,
            sets,
            100_000,
            LivenessFixedPoint::WorklistPostorder {
                schedule_preds: false,
            },
        );
        assert_eq!((&rr.1, &rr.2), (&po.1, &po.2));
        assert!(rr.3 >= (nb - 2) * nb, "index order: {} visits", rr.3);
        assert!(po.3 <= 2 * nb, "postorder: {} visits", po.3);
        // Cut off after one pass, the postorder has already settled.
        let po1 = i69_l2_run(
            &succ,
            sets,
            2,
            LivenessFixedPoint::WorklistPostorder {
                schedule_preds: false,
            },
        );
        assert!(po1.0);
        let rr1 = i69_l2_run(&succ, sets, 2, LivenessFixedPoint::RoundRobin);
        assert!(!rr1.0);
    }

    /// On the scheduled graphs the postorder worklist (either predecessor
    /// source) gives the round robin's converged answer in the shared core
    /// and in all four legacy copies, at the real caps.
    #[test]
    fn i71_l2_the_postorder_worklist_answers_like_the_round_robin_on_schedules() {
        let mut compared = 0usize;
        for (name, graph, schedule) in i71_l2_scheduled_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for rules in i65_l2_rule_grid() {
                    let run = |fixed_point| {
                        liveness_core_with(
                            &graph,
                            &schedule,
                            &wants,
                            rules,
                            LIVE_MODEL_WORK_BUDGET,
                            LIVE_MODEL_MAX_ITERATIONS,
                            false,
                            false,
                            fixed_point,
                        )
                        .0
                    };
                    let rr = run(LivenessFixedPoint::RoundRobin);
                    for schedule_preds in [false, true] {
                        let po = run(LivenessFixedPoint::WorklistPostorder { schedule_preds });
                        assert_eq!(rr, po, "{name} {rules:?} {schedule_preds}");
                        compared += 1;
                    }
                }
            }
            for rules in i65_l2_rule_grid() {
                let run = |fixed_point| {
                    let mut trace = LsLegacyRun::default();
                    let model = build_live_model_legacy_with(
                        &graph,
                        &schedule,
                        rules,
                        LIVE_MODEL_MAX_ITERATIONS,
                        fixed_point,
                        Some(&mut trace),
                    );
                    (model, trace)
                };
                let (rr, rr_trace) = run(LivenessFixedPoint::RoundRobin);
                for schedule_preds in [false, true] {
                    let (po, po_trace) =
                        run(LivenessFixedPoint::WorklistPostorder { schedule_preds });
                    let what = format!("{name} {rules:?} {schedule_preds}: live model");
                    assert_eq!(rr_trace.answer(), po_trace.answer(), "{what}");
                    i65_l2_assert_live_models_equal(&rr, &po, &what);
                    compared += 1;
                }
            }
            for (copy, run) in crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                &graph, &schedule,
            ) {
                let rr = run(LivenessFixedPoint::RoundRobin, LIVE_MODEL_MAX_ITERATIONS);
                for schedule_preds in [false, true] {
                    let po = run(
                        LivenessFixedPoint::WorklistPostorder { schedule_preds },
                        LIVE_MODEL_MAX_ITERATIONS,
                    );
                    assert_eq!(rr.answer(), po.answer(), "{name} {copy} {schedule_preds}");
                    compared += 1;
                }
            }
        }
        assert!(compared > 0);
    }

    /// Compile-time bench probe (interpreter round i2 wave 69, lane L2): the
    /// round-robin fixed point vs the worklist, ns per graph (min over 15
    /// interleaved reps) and block visits, to stderr. Two tables: the fixed
    /// point alone on synthetic CFGs ((a) typical method bodies, (b) deep
    /// loop nests, (c) wide switches, (d) irreducible graphs with and without
    /// exception-style edges; random sets of 4 values per block), and the
    /// whole `liveness_core` (live-model rules and value set, sparse) on the
    /// scheduled test graphs and the long sequential-loop graphs. Not a
    /// pass/fail test beyond the answers being equal.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i69_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i69_l2_bench_round_robin_vs_worklist_liveness() {
        const REPS: usize = 15;
        // Min ns per call of `f` over REPS reps of `iters` calls each, for
        // two closures interleaved.
        fn best_pair(
            iters: usize,
            mut a: impl FnMut() -> usize,
            mut b: impl FnMut() -> usize,
        ) -> (f64, f64) {
            let (mut best_a, mut best_b) = (f64::MAX, f64::MAX);
            for _ in 0..REPS {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(a());
                }
                std::hint::black_box(sink);
                best_a = best_a.min(t0.elapsed().as_nanos() as f64 / iters as f64);
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(b());
                }
                std::hint::black_box(sink);
                best_b = best_b.min(t0.elapsed().as_nanos() as f64 / iters as f64);
            }
            (best_a, best_b)
        }

        let mut rng = I69Rng(0xD1B5_4A32_D192_ED03);
        let cfgs: Vec<(&str, I69Cfg)> = vec![
            ("(a) typical 10", i69_l2_typical(10)),
            ("(a) typical 40", i69_l2_typical(40)),
            ("(a) typical 200", i69_l2_typical(200)),
            ("(b) nest 4", i69_l2_loop_nest(4)),
            ("(b) nest 16", i69_l2_loop_nest(16)),
            ("(b) nest 64", i69_l2_loop_nest(64)),
            ("(c) switch 64", i69_l2_wide_switch(64)),
            ("(c) switch 1000", i69_l2_wide_switch(1000)),
            ("(d) irreducible 10", i69_l2_irreducible(10, false)),
            ("(d) irreducible 100", i69_l2_irreducible(100, false)),
            ("(d) irred+exc 100", i69_l2_irreducible(100, true)),
        ];
        for (name, succ) in &cfgs {
            let nb = succ.len();
            let words = (4 * nb).div_ceil(64).max(1);
            let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
            let sets = LsFixedPointSets {
                nb,
                words,
                phi_out: &phi_out,
                uses: &uses,
                defs: &defs,
            };
            let max = LIVE_MODEL_MAX_ITERATIONS;
            let rr = i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin);
            let wl = i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist);
            assert_eq!((rr.0, &rr.1, &rr.2), (wl.0, &wl.1, &wl.2), "{name}");
            let iters = (20_000_000 / (rr.3 * words + 1)).clamp(3, 100_000);
            let (ns_rr, ns_wl) = best_pair(
                iters,
                || i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin).3,
                || i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist).3,
            );
            eprintln!(
                "[i69-l2 bench] fixed point {name:<20} blocks {nb:>5} words {words:>3} converged {:<5} \
                 visits rr {:>7} wl {:>7} round-robin {ns_rr:>11.0} ns/graph worklist {ns_wl:>11.0} \
                 ns/graph wl/rr {:>5.3}",
                rr.0,
                rr.3,
                wl.3,
                ns_wl / ns_rr,
            );
        }

        let rules = i67_l2_model_rules();
        let mut graphs = i67_l2_graphs();
        for (name, loops) in [("loops50", 50usize), ("loops150", 150), ("loops250", 250)] {
            let (g, s) = i67_l2_sequential_loops(loops);
            graphs.push((name, g, s));
        }
        for (name, graph, schedule) in &graphs {
            let wants = live_model_wants_loc(graph, schedule);
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let run = |fixed_point| {
                liveness_core_with(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                    false,
                    false,
                    fixed_point,
                )
            };
            let (rr, rr_visits) = run(LivenessFixedPoint::RoundRobin);
            let (wl, wl_visits) = run(LivenessFixedPoint::Worklist);
            assert_eq!(rr, wl, "{name}");
            let iters = (20_000_000 / (n * nb / 64 + n + 1)).clamp(3, 20_000);
            let (ns_rr, ns_wl) = best_pair(
                iters,
                || run(LivenessFixedPoint::RoundRobin).0.live_in.len(),
                || run(LivenessFixedPoint::Worklist).0.live_in.len(),
            );
            eprintln!(
                "[i69-l2 bench] core {name:<20} nodes {n:>6} blocks {nb:>5} converged {:<5} \
                 visits rr {rr_visits:>7} wl {wl_visits:>7} round-robin {ns_rr:>11.0} ns/run \
                 worklist {ns_wl:>11.0} ns/run wl/rr {:>5.3}",
                rr.converged,
                ns_wl / ns_rr,
            );
        }

        // Interpreter round i2 wave 70, lane L2: the legacy copies the
        // default JIT runs (`CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST`), each
        // timed whole (its set-up included, which the switch does not touch):
        // the live model's (`build_live_model_legacy`) and `ir_lower`'s slot
        // plan, value-read ranges and chain-read ranges.
        for (name, graph, schedule) in &graphs {
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let live_model: crate::ir_lower::i70_l2_legacy_liveness_tests::I70Copy<'_> =
                Box::new(move |fixed_point: LivenessFixedPoint, cap: usize| {
                    let mut trace = LsLegacyRun::default();
                    let _ = build_live_model_legacy_with(
                        graph,
                        schedule,
                        rules,
                        cap,
                        fixed_point,
                        Some(&mut trace),
                    );
                    trace
                });
            let mut copies = vec![("live model", live_model)];
            copies.extend(
                crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                    graph, schedule,
                ),
            );
            for (copy, run) in &copies {
                let max = LIVE_MODEL_MAX_ITERATIONS;
                let rr = run(LivenessFixedPoint::RoundRobin, max);
                let wl = run(LivenessFixedPoint::Worklist, max);
                assert_eq!(rr.answer(), wl.answer(), "{name} {copy}");
                let iters = (20_000_000 / (n * nb / 64 + n + 1)).clamp(3, 20_000);
                let (ns_rr, ns_wl) = best_pair(
                    iters,
                    || run(LivenessFixedPoint::RoundRobin, max).visits,
                    || run(LivenessFixedPoint::Worklist, max).visits,
                );
                eprintln!(
                    "[i70-l2 bench] legacy {copy:<12} {name:<20} nodes {n:>6} blocks {nb:>5} \
                     ran {:<5} converged {:<5} visits rr {:>7} wl {:>7} round-robin {ns_rr:>11.0} \
                     ns/run worklist {ns_wl:>11.0} ns/run wl/rr {:>5.3}",
                    rr.ran,
                    rr.converged,
                    rr.visits,
                    wl.visits,
                    ns_wl / ns_rr,
                );
            }
        }
    }

    /// Min ns per call of each of three closures, interleaved, over 11 reps;
    /// the batch size is calibrated on the first closure to about 2 ms
    /// (interpreter round i3 wave 71, lane L2).
    fn i71_l2_best3(
        a: &mut dyn FnMut() -> usize,
        b: &mut dyn FnMut() -> usize,
        c: &mut dyn FnMut() -> usize,
    ) -> [f64; 3] {
        let mut fs: [&mut dyn FnMut() -> usize; 3] = [a, b, c];
        let mut sink = 0usize;
        for _ in 0..4 {
            sink = sink.wrapping_add((fs[0])());
        }
        let t0 = std::time::Instant::now();
        for _ in 0..16 {
            sink = sink.wrapping_add((fs[0])());
        }
        let per_call = (t0.elapsed().as_nanos() as f64 / 16.0).max(1.0);
        std::hint::black_box(sink);
        let iters = ((2_000_000.0 / per_call) as usize).clamp(3, 200_000);
        let mut best = [f64::MAX; 3];
        for _ in 0..11 {
            for (k, f) in fs.iter_mut().enumerate() {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add((*f)());
                }
                std::hint::black_box(sink);
                best[k] = best[k].min(t0.elapsed().as_nanos() as f64 / iters as f64);
            }
        }
        best
    }

    /// The smallest block count from which every row of a family (sorted by
    /// block count) has a ratio below 1, if any: where a block-count gate
    /// would go for that family.
    fn i71_l2_crossover(rows: &[(usize, f64)]) -> Option<usize> {
        let mut from = None;
        for &(nb, ratio) in rows.iter().rev() {
            if ratio < 1.0 {
                from = Some(nb);
            } else {
                break;
            }
        }
        from
    }

    /// Compile-time bench probe (interpreter round i3 wave 71, lane L2;
    /// `i70-L2-proposal-one-predecessor-index-per-schedule`): where the
    /// worklist starts to beat the round robin, by block count, for the
    /// `CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS` gate, and what reading
    /// the schedule's own predecessor lists
    /// (`CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS`) saves. Three columns per
    /// row, ns per run (min over 11 interleaved reps): round robin (`rr`),
    /// worklist with its per-call predecessor index (`wl`), worklist over a
    /// predecessor index built once (`wp`; on a schedule, its own lists).
    /// Tables, to stderr:
    ///   * `fixed point`: the driver alone on synthetic CFG families of 2 to
    ///     ~130 blocks (typical method bodies, loop nests; random sets of 4
    ///     values per block), the index for `wp` built outside the timing;
    ///   * `core`: the whole `liveness_core` (live-model rules and values,
    ///     sparse) on scheduled sequential-loop graphs of 1 to 24 loops and
    ///     the three small test graphs;
    ///   * `postorder` / `core postorder`: the postorder pass order
    ///     (`CRATONVM_JIT_IR_LIVENESS_POSTORDER`) against the round robin and
    ///     the index-order worklist, on the families plus a chain laid out
    ///     backwards, and in the core on the scheduled graphs;
    ///   * `legacy`: the four legacy copies the default JIT runs, on the same
    ///     graphs, timed whole;
    ///   * `crossover`: per family, the smallest block count from which `wl`
    ///     (and `wp`) beats `rr` on every larger row.
    /// Not a pass/fail test beyond the answers being equal.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i71_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i71_l2_bench_worklist_crossover_by_block_count() {
        let max = LIVE_MODEL_MAX_ITERATIONS;
        let mut rng = I69Rng(0x9E37_79B9_7F4A_7C15);
        let families: Vec<(&str, Vec<I69Cfg>)> = vec![
            (
                "typical",
                [0usize, 1, 2, 3, 5, 6, 9, 12, 15, 18, 24, 36, 48]
                    .iter()
                    .map(|&s| i69_l2_typical(s))
                    .collect(),
            ),
            (
                "nest",
                [1usize, 2, 4, 6, 8, 12, 16, 24, 32, 64]
                    .iter()
                    .map(|&d| i69_l2_loop_nest(d))
                    .collect(),
            ),
            (
                "irred+exc",
                [1usize, 2, 4, 8, 12, 16, 24, 40]
                    .iter()
                    .map(|&r| i69_l2_irreducible(r, true))
                    .collect(),
            ),
        ];
        let mut crossovers: Vec<(String, Option<usize>, Option<usize>)> = Vec::new();
        for (family, cfgs) in &families {
            let mut wl_rows: Vec<(usize, f64)> = Vec::new();
            let mut wp_rows: Vec<(usize, f64)> = Vec::new();
            for succ in cfgs.iter() {
                let nb = succ.len();
                let words = (4 * nb).div_ceil(64).max(1);
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out,
                    uses: &uses,
                    defs: &defs,
                };
                let rr = i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin);
                let wl = i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist);
                let wp = i69_l2_run(succ, sets, max, LivenessFixedPoint::WorklistSchedulePreds);
                assert_eq!((rr.0, &rr.1, &rr.2), (wl.0, &wl.1, &wl.2), "{family} {nb}");
                assert_eq!(wl, wp, "{family} {nb}");
                let index = LsPredIndex::build(succ.as_slice(), nb);
                let mut run_rr = || i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin).3;
                let mut run_wl = || i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist).3;
                let mut run_wp = || {
                    let mut li = vec![0u64; nb * words];
                    let mut lout = vec![0u64; nb * words];
                    let mut visits = 0usize;
                    let _ = ls_fixed_point_worklist_preds(
                        succ.as_slice(),
                        &index,
                        sets,
                        &mut li,
                        &mut lout,
                        max,
                        &mut visits,
                    );
                    visits
                };
                let [ns_rr, ns_wl, ns_wp] = i71_l2_best3(&mut run_rr, &mut run_wl, &mut run_wp);
                eprintln!(
                    "[i71-l2 bench] fixed point {family:<10} blocks {nb:>4} words {words:>2} \
                     visits rr {:>6} wl {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} ns/run \
                     wp {ns_wp:>9.0} ns/run wl/rr {:>5.3} wp/rr {:>5.3}",
                    rr.3,
                    wl.3,
                    ns_wl / ns_rr,
                    ns_wp / ns_rr,
                );
                wl_rows.push((nb, ns_wl / ns_rr));
                wp_rows.push((nb, ns_wp / ns_rr));
            }
            wl_rows.sort_by_key(|r| r.0);
            wp_rows.sort_by_key(|r| r.0);
            crossovers.push((
                format!("fixed point {family}"),
                i71_l2_crossover(&wl_rows),
                i71_l2_crossover(&wp_rows),
            ));
        }

        // The postorder pass order (`CRATONVM_JIT_IR_LIVENESS_POSTORDER`):
        // round robin vs the index-order worklist vs the postorder worklist
        // (its order and index built per run, as on a schedule), on the same
        // families plus a chain laid out backwards (every edge retreats).
        let mut po_families = families.clone();
        po_families.push((
            "reversed",
            [4usize, 8, 16, 32, 64, 128]
                .iter()
                .map(|&n| i71_l2_reversed_chain(n))
                .collect(),
        ));
        for (family, cfgs) in &po_families {
            for succ in cfgs.iter() {
                let nb = succ.len();
                let words = (4 * nb).div_ceil(64).max(1);
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out,
                    uses: &uses,
                    defs: &defs,
                };
                let po_fp = LivenessFixedPoint::WorklistPostorder {
                    schedule_preds: false,
                };
                let rr = i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin);
                let wl = i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist);
                let po = i69_l2_run(succ, sets, max, po_fp);
                assert_eq!((rr.0, &rr.1, &rr.2), (po.0, &po.1, &po.2), "{family} {nb}");
                let mut run_rr = || i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin).3;
                let mut run_wl = || i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist).3;
                let mut run_po = || i69_l2_run(succ, sets, max, po_fp).3;
                let [ns_rr, ns_wl, ns_po] = i71_l2_best3(&mut run_rr, &mut run_wl, &mut run_po);
                eprintln!(
                    "[i71-l2 bench] postorder {family:<10} blocks {nb:>4} visits rr {:>6} wl {:>6} \
                     po {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} ns/run po {ns_po:>9.0} \
                     ns/run po/rr {:>5.3} po/wl {:>5.3}",
                    rr.3,
                    wl.3,
                    po.3,
                    ns_po / ns_rr,
                    ns_po / ns_wl,
                );
            }
        }

        // Scheduled graphs: the three small test graphs, then sequential
        // loops of growing length.
        let rules = i67_l2_model_rules();
        let mut graphs: Vec<(String, Graph, ir_schedule::Schedule)> = Vec::new();
        for (name, (g, s)) in ["swap_loop", "div_and_shift", "nested_token"]
            .iter()
            .zip(i65_l2_graphs())
        {
            graphs.push((name.to_string(), g, s));
        }
        for loops in [1usize, 2, 3, 4, 6, 8, 12, 16, 24] {
            let (g, s) = i67_l2_sequential_loops(loops);
            graphs.push((format!("loops{loops}"), g, s));
        }
        let mut core_wl: Vec<(usize, f64)> = Vec::new();
        let mut core_wp: Vec<(usize, f64)> = Vec::new();
        let mut legacy_wl: Vec<(usize, f64)> = Vec::new();
        let mut legacy_wp: Vec<(usize, f64)> = Vec::new();
        for (name, graph, schedule) in &graphs {
            let wants = live_model_wants_loc(graph, schedule);
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let core = |fixed_point| {
                liveness_core_with(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    max,
                    false,
                    false,
                    fixed_point,
                )
            };
            let (rr, rr_visits) = core(LivenessFixedPoint::RoundRobin);
            let wl = core(LivenessFixedPoint::Worklist);
            let wp = core(LivenessFixedPoint::WorklistSchedulePreds);
            assert_eq!(rr, wl.0, "{name}");
            assert_eq!(wl, wp, "{name}");
            let [ns_rr, ns_wl, ns_wp] = i71_l2_best3(
                &mut || core(LivenessFixedPoint::RoundRobin).1,
                &mut || core(LivenessFixedPoint::Worklist).1,
                &mut || core(LivenessFixedPoint::WorklistSchedulePreds).1,
            );
            eprintln!(
                "[i71-l2 bench] core {name:<14} nodes {n:>5} blocks {nb:>4} visits rr {rr_visits:>6} \
                 wl {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} ns/run wp {ns_wp:>9.0} ns/run \
                 wl/rr {:>5.3} wp/rr {:>5.3}",
                wl.1,
                ns_wl / ns_rr,
                ns_wp / ns_rr,
            );
            if name.starts_with("loops") {
                core_wl.push((nb, ns_wl / ns_rr));
                core_wp.push((nb, ns_wp / ns_rr));
            }
            let po_fp = LivenessFixedPoint::WorklistPostorder {
                schedule_preds: true,
            };
            let po = core(po_fp);
            assert_eq!(rr, po.0, "{name}: postorder");
            let [ns_rr2, ns_wp2, ns_po] = i71_l2_best3(
                &mut || core(LivenessFixedPoint::RoundRobin).1,
                &mut || core(LivenessFixedPoint::WorklistSchedulePreds).1,
                &mut || core(po_fp).1,
            );
            eprintln!(
                "[i71-l2 bench] core postorder {name:<14} blocks {nb:>4} visits wp {:>6} po {:>6} \
                 rr {ns_rr2:>9.0} ns/run wp {ns_wp2:>9.0} ns/run po {ns_po:>9.0} ns/run \
                 po/rr {:>5.3} po/wp {:>5.3}",
                wp.1,
                po.1,
                ns_po / ns_rr2,
                ns_po / ns_wp2,
            );

            let live_model: crate::ir_lower::i70_l2_legacy_liveness_tests::I70Copy<'_> =
                Box::new(move |fixed_point: LivenessFixedPoint, cap: usize| {
                    let mut trace = LsLegacyRun::default();
                    let _ = build_live_model_legacy_with(
                        graph,
                        schedule,
                        rules,
                        cap,
                        fixed_point,
                        Some(&mut trace),
                    );
                    trace
                });
            let mut copies = vec![("live model", live_model)];
            copies.extend(
                crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                    graph, schedule,
                ),
            );
            let (mut sum_rr, mut sum_wl, mut sum_wp) = (0.0f64, 0.0f64, 0.0f64);
            for (copy, run) in &copies {
                let rr = run(LivenessFixedPoint::RoundRobin, max);
                let wl = run(LivenessFixedPoint::Worklist, max);
                let wp = run(LivenessFixedPoint::WorklistSchedulePreds, max);
                assert_eq!(rr.answer(), wl.answer(), "{name} {copy}");
                assert_eq!(wl, wp, "{name} {copy}");
                let [ns_rr, ns_wl, ns_wp] = i71_l2_best3(
                    &mut || run(LivenessFixedPoint::RoundRobin, max).visits,
                    &mut || run(LivenessFixedPoint::Worklist, max).visits,
                    &mut || run(LivenessFixedPoint::WorklistSchedulePreds, max).visits,
                );
                sum_rr += ns_rr;
                sum_wl += ns_wl;
                sum_wp += ns_wp;
                eprintln!(
                    "[i71-l2 bench] legacy {copy:<12} {name:<14} nodes {n:>5} blocks {nb:>4} \
                     ran {:<5} visits rr {:>6} wl {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} \
                     ns/run wp {ns_wp:>9.0} ns/run wl/rr {:>5.3} wp/rr {:>5.3}",
                    rr.ran,
                    rr.visits,
                    wl.visits,
                    ns_wl / ns_rr,
                    ns_wp / ns_rr,
                );
            }
            eprintln!(
                "[i71-l2 bench] legacy all four   {name:<14} nodes {n:>5} blocks {nb:>4} \
                 rr {sum_rr:>9.0} ns/compile wl {sum_wl:>9.0} ns/compile wp {sum_wp:>9.0} \
                 ns/compile wl/rr {:>5.3} wp/rr {:>5.3}",
                sum_wl / sum_rr,
                sum_wp / sum_rr,
            );
            if name.starts_with("loops") {
                legacy_wl.push((nb, sum_wl / sum_rr));
                legacy_wp.push((nb, sum_wp / sum_rr));
            }
        }
        for (what, mut wl_rows, mut wp_rows) in [
            ("core loops", core_wl, core_wp),
            ("legacy loops", legacy_wl, legacy_wp),
        ] {
            wl_rows.sort_by_key(|r| r.0);
            wp_rows.sort_by_key(|r| r.0);
            crossovers.push((
                what.to_string(),
                i71_l2_crossover(&wl_rows),
                i71_l2_crossover(&wp_rows),
            ));
        }
        for (what, wl_from, wp_from) in &crossovers {
            eprintln!(
                "[i71-l2 bench] crossover {what:<22} wl beats rr from {} blocks, wp from {} blocks",
                wl_from.map_or("never".to_string(), |b| b.to_string()),
                wp_from.map_or("never".to_string(), |b| b.to_string()),
            );
        }
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
            // Round 16 (lane indy): `jit_indy_bridge`.
            Op::IndyBridge { site: 0 },
            // Round 16 (lane matrix): `jit_multianewarray_n`.
            Op::MultiNewArray { site: 0 },
            // nio21 w15 L3: `jit_new_object_cp`.
            Op::NewCp {
                holder_class_id: 0,
                cp_idx: 0,
            },
            // `jit_putstatic_*`.
            Op::StoreStatic {
                class_id: 0,
                field_index: 0,
                type_tag: b'D',
                is_volatile: false,
            },
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
        // `jit_putstatic_*` can also run `<clinit>` (arbitrary Java, a GC):
        // a safepoint as well as a clobber.
        assert!(ir_op_is_safepoint(&Op::StoreStatic {
            class_id: 0,
            field_index: 0,
            type_tag: b'L',
            is_volatile: true,
        }));
        assert!(!ir_op_is_call(&Op::Guard { bci: 0 }));
        assert!(!ir_op_is_call(&Op::Add));
        assert!(!ir_op_is_call(&Op::ArrayLoad(crate::ir::MemKind::Double)));
    }

    /// `Op::Phi` DEFINES a value, and this enumeration has to say so.
    ///
    /// Not a restatement of the match arm. `ir_op_defines_value` and
    /// `ir_lower::op_defines_result_slot` are two enumerations of one question,
    /// and `ir_lower`'s `the_two_value_defining_enumerations_agree` compares
    /// the two SETS — so it fails the day one list drops `Op::Phi` and the
    /// other keeps it, but it passes the day BOTH drop it, and a phi that
    /// neither enumeration calls a value is a phi with no home and no register.
    /// That is the state the phi-residency work exists to leave behind, so it
    /// gets an absolute assertion rather than a relative one.
    ///
    /// The relative check is what caught the real bug (`Op::ArrayLength` and
    /// `Op::NewArray` in one list and not the other, silently declining
    /// residency for every method containing an `arraylength`); this is the
    /// floor beneath it.
    #[test]
    fn a_phi_defines_a_value_in_this_enumeration() {
        assert!(
            ir_op_defines_value(&Op::Phi),
            "a phi with no location has no home word for `emit_phi_copies` to \
             write and no interval for the scan to colour"
        );
    }

    /// `release_phi_pins` releases the phis and NOTHING else.
    ///
    /// The pin it drops is structural — "no definition arm ever writes a phi,
    /// so its home word must always hold the value" — and it is legal to drop
    /// exactly for a consumer that supplies that write itself at every incoming
    /// edge. `ir_lower::emit_phi_copies` does. What must not happen is this
    /// call also releasing a DEOPT pin, whose premise ("the home word must hold
    /// the value at any recorded bci") no edge copy discharges: that is
    /// `release_deopt_pins`' separate decision, made by a caller with its own
    /// argument for it.
    ///
    /// Asserted as a difference between two models rather than as an absolute,
    /// so a fixture whose deopt pins happen to be empty cannot make it vacuous.
    #[test]
    fn release_phi_pins_releases_the_phis_and_leaves_every_other_pin_alone() {
        let (graph, schedule) = swap_loop();
        let mut live = build_live_model(&graph, &schedule);
        assert!(live.converged, "a small loop must reach the fixed point");

        let before = live.pinned.clone();
        let pinned_phis: Vec<usize> = (0..graph.nodes.len())
            .filter(|&id| matches!(graph.nodes[id].op, Op::Phi) && before[id])
            .collect();
        assert!(
            !pinned_phis.is_empty(),
            "the swap loop's header phis must be pinned, or this test proves \
             nothing"
        );
        let pinned_others: Vec<usize> = (0..graph.nodes.len())
            .filter(|&id| !matches!(graph.nodes[id].op, Op::Phi) && before[id])
            .collect();
        assert!(
            !pinned_others.is_empty(),
            "the builder records a frame state per bci, so some non-phi value \
             must be deopt-pinned, or the second half of this test is vacuous"
        );

        let released = live.release_phi_pins(&graph);
        assert_eq!(
            released,
            pinned_phis.len(),
            "the count must be the phis it actually unpinned"
        );
        for id in pinned_phis {
            assert!(!live.pinned[id], "phi n{id} kept its structural pin");
        }
        for id in pinned_others {
            assert!(
                live.pinned[id],
                "n{id} is not a phi and lost its pin; `release_phi_pins` has no \
                 argument for releasing a deopt pin"
            );
        }
    }

    // ── r9 ───────────────────────────────────────────────────────────

    /// An integral `Op::Rem` is `CDQ/CQO; IDIV` inline and calls nothing, so
    /// it must not destroy the caller-saved half of the file. The FP remainder
    /// still does (`jit_frem` / `jit_drem`).
    #[test]
    fn an_integer_remainder_is_not_a_call() {
        let mut f = Fixture::new(1);
        let int_rem = f.graph.add(Op::Rem, IrType::Int, vec![], None);
        let long_rem = f.graph.add(Op::Rem, IrType::Long, vec![], None);
        let fp_rem = f.graph.add(Op::Rem, IrType::Double, vec![], None);
        let float_rem = f.graph.add(Op::Rem, IrType::Float, vec![], None);
        assert!(is_inline_integer_rem(&f.graph.nodes[int_rem as usize]));
        assert!(is_inline_integer_rem(&f.graph.nodes[long_rem as usize]));
        assert!(!is_inline_integer_rem(&f.graph.nodes[fp_rem as usize]));
        assert!(!is_inline_integer_rem(&f.graph.nodes[float_rem as usize]));

        // End to end through `for_graph`: `iload_0; iload_1; irem; istore_2;
        // iload_2; ireturn` over a file with one volatile XMM register.
        let code = [0x1a, 0x1b, 0x70, 0x3d, 0x1c, 0xac, 0, 0];
        let graph = IrBuilder::new(2, 4)
            .build(&code, 6)
            .expect("the remainder method builds");
        let schedule = ir_schedule::schedule(&graph);
        let live = build_live_model(&graph, &schedule);
        let xmm2 = PhysReg::xmm(2);
        let file = RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(3),
                caller_saved: false,
            },
            RegSpec {
                reg: xmm2,
                caller_saved: true,
            },
        ]);
        let model = MachineModel::for_graph(&graph, &schedule, &live, file);
        let (_, pos) = pos_of_op(&graph, &live, |op, ty| {
            matches!(op, Op::Rem) && matches!(ty, IrType::Int)
        })
        .expect("the method takes a remainder");
        assert!(
            !model.clobbered_at(pos).contains(&xmm2),
            "an integer remainder calls no helper, so a double in a volatile XMM \
             register survives it: {:?}",
            model.clobbered_at(pos)
        );
    }

    /// With a REQUIRED register busy, only that register's holder may be
    /// nominated: evicting any other active frees a register the retry will
    /// not take, and costs a split and a spill for nothing.
    #[test]
    fn a_required_register_is_made_room_for_by_its_holder_only() {
        let mut f = Fixture::new(12);
        let every: Vec<usize> = (1..=10).collect();
        let a = f.value(Op::Add, IrType::Int, 0, 10, &every);
        let b = f.value(Op::Add, IrType::Int, 1, 10, &[10]);
        let c = f.value(Op::Add, IrType::Int, 5, 9, &[6, 9]);
        let (graph, live) = f.finish();
        let active = vec![
            Active {
                reg: PhysReg::gp(0),
                lo: 0,
                hi: 10,
                node: a,
                seg: 0,
            },
            Active {
                reg: PhysReg::gp(1),
                lo: 1,
                hi: 10,
                node: b,
                seg: 0,
            },
        ];
        let current = Pending {
            lo: 5,
            hi: 9,
            node: c,
        };

        // Unrestricted, `b` — one far use — is the obvious victim.
        assert!(
            matches!(
                ls_pick_victim(&graph, &live, &active, RegClass::Gp, current, None),
                Victim::Active(1)
            ),
            "precondition: by score alone `b` is the victim"
        );
        // But `c` must take r0, which `a` holds. Cutting `b` short frees r1,
        // which `c` cannot use; `a` is used on every position and is a worse
        // victim than `c` itself, so `c` is the one that waits.
        assert!(
            matches!(
                ls_pick_victim(
                    &graph,
                    &live,
                    &active,
                    RegClass::Gp,
                    current,
                    Some(PhysReg::gp(0))
                ),
                Victim::Current
            ),
            "only the holder of the required register may be nominated"
        );
        // Nobody holds the required register (it is blocked, not busy): no
        // eviction can help.
        assert!(matches!(
            ls_pick_victim(
                &graph,
                &live,
                &active,
                RegClass::Gp,
                current,
                Some(PhysReg::gp(2))
            ),
            Victim::Current
        ));
    }

    /// A re-queued suffix whose `hi` stops short of an own fixed constraint
    /// must not have its segment stretched to that constraint.
    ///
    /// `v` takes r0 for its constraint at 4 and is cut at 9 by r0's clobber at
    /// 10; `w` then evicts it at 6, and its piece `[7, 9]` is re-queued with no
    /// constraint inside it. It gets r0 again — and the own constraint at 20
    /// (on r1) used to set the segment's end to 19, through the clobber at 10,
    /// which `ls_finish`/`verify_allocation` then refused as an internal error.
    #[test]
    fn an_own_constraint_beyond_a_suffix_does_not_stretch_its_segment() {
        let mut f = Fixture::new(24);
        let v = f.value(Op::Add, IrType::Int, 0, 22, &[7, 22]);
        let dense: Vec<usize> = (2..=21).collect();
        let _x = f.value(Op::Add, IrType::Int, 1, 21, &dense);
        let _w = f.value(Op::Add, IrType::Int, 6, 6, &[6]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(2));
        model.fixed = vec![
            FixedConstraint {
                node: v,
                pos: 4,
                reg: PhysReg::gp(0),
            },
            FixedConstraint {
                node: v,
                pos: 20,
                reg: PhysReg::gp(1),
            },
        ];
        model.add_clobbers(10, &[PhysReg::gp(0)]);

        let alloc = allocate_linear_scan(&graph, &live, &model)
            .expect("a suffix is allocated within its own bounds");
        for seg in &alloc.segments[v as usize] {
            if seg.reg == Some(PhysReg::gp(0)) {
                assert!(
                    !seg.range.contains(10),
                    "n{v} holds r0 across the clobber at 10: {:?}",
                    alloc.segments[v as usize]
                );
            }
        }
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// A helper-produced reference (`ldc` String here) may donate a home word
    /// but never receive a recycled one — the rule `ir_lower::plan_slots`
    /// applies to all four such ops and this colouring applied to `Op::Call`
    /// alone.
    #[test]
    fn a_helper_produced_reference_never_recycles_a_home_word() {
        let mut f = Fixture::new(12);
        let early = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 0, 2, &[2]);
        let string = f.value(
            Op::ConstString {
                holder_class_id: 0,
                cp_idx: 1,
                slot_addr: 0,
            },
            IrType::Ref,
            4,
            8,
            &[8],
        );
        let (graph, live) = f.finish();
        // No register at all, so both values live in their home words and the
        // colouring is the only thing under test.
        let model = bare_model(RegFile::default());

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.stack_slot[early as usize], Some(0));
        assert_eq!(
            alloc.stack_slot[string as usize],
            Some(1),
            "`early`'s word is free by position 4, and a `ConstString` result \
             must still not be handed it"
        );
        assert_eq!(alloc.stack_slots, 2);
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// An interval that crosses no call takes a free caller-saved register in
    /// preference to a callee-saved one listed before it, leaving the
    /// callee-saved one for an interval that does cross a call.
    #[test]
    fn an_interval_that_crosses_no_call_prefers_a_volatile_register() {
        let mut f = Fixture::new(12);
        let short = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let across = f.value(Op::Add, IrType::Int, 1, 8, &[2, 8]);
        let (graph, live) = f.finish();
        // Callee-saved FIRST, as `RegFile::x86_64` and `RegFile::aarch64` list
        // them.
        let file = RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(0),
                caller_saved: false,
            },
            RegSpec {
                reg: PhysReg::gp(1),
                caller_saved: true,
            },
        ]);
        let mut model = bare_model(file);
        model.add_clobbers(5, &[PhysReg::gp(1)]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(short),
            Some(PhysReg::gp(1)),
            "`short` dies before the call, so the volatile register serves it"
        );
        assert_eq!(
            alloc.first_reg(across),
            Some(PhysReg::gp(0)),
            "which leaves the callee-saved register for the value that crosses \
             the call"
        );
        assert_eq!(alloc.splits, 0, "and nothing has to be split");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// Three blocks, `0 -> 1`, `1 -> {1, 2}`: block 1 is a self loop.
    fn self_loop_schedule() -> ir_schedule::Schedule {
        let edges: [(Vec<usize>, Vec<usize>); 3] = [
            (vec![1], vec![]),        // 0: preheader
            (vec![1, 2], vec![0, 1]), // 1: loop, back edge to itself
            (vec![], vec![1]),        // 2: exit
        ];
        let blocks: Vec<ir_schedule::Block> = edges
            .into_iter()
            .enumerate()
            .map(|(id, (successors, predecessors))| ir_schedule::Block {
                id,
                ctrl: NO_NODE,
                nodes: Vec::new(),
                terminator: None,
                successors,
                predecessors,
            })
            .collect();
        let dom = ir_schedule::Dominators::compute(&blocks);
        ir_schedule::Schedule {
            blocks,
            node_to_block: Vec::new(),
            dom,
            freq: Default::default(),
            layout: Default::default(),
            pairing: Default::default(),
        }
    }

    #[test]
    fn a_split_that_leaves_a_register_on_the_back_edge_is_an_edge_conflict() {
        let mut f = Fixture::new(13);
        // Evicted inside the loop and never reloaded before the back edge: the
        // loop's first positions read r0, which from the second iteration on
        // holds whatever took it at 6.
        let evicted = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        // Reloaded into the SAME register before the back edge: consistent.
        let reloaded = f.value(Op::Add, IrType::Int, 1, 9, &[4, 8, 9]);
        // One register for its whole range: nothing to disagree about.
        let whole = f.value(Op::Add, IrType::Int, 1, 9, &[9]);
        let (_graph, mut live) = f.finish();
        live.span = vec![(0, 2), (3, 9), (10, 12)];
        let schedule = self_loop_schedule();

        let seg = |lo: usize, hi: usize, reg: Option<u8>| Segment {
            range: PosRange { lo, hi },
            reg: reg.map(PhysReg::gp),
        };
        let alloc = Allocation {
            segments: vec![
                vec![seg(1, 5, Some(0)), seg(6, 9, None)],
                vec![seg(1, 5, Some(1)), seg(6, 7, None), seg(8, 9, Some(1))],
                vec![seg(1, 9, Some(2))],
            ],
            stack_slot: vec![Some(0), Some(1), None],
            stack_slots: 2,
            ..Default::default()
        };

        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert_eq!(conflicts.len(), 3);
        assert!(
            conflicts[evicted as usize],
            "the back edge arrives with n{evicted} in memory, and block 1's code \
             reads it from r0"
        );
        assert!(
            !conflicts[reloaded as usize],
            "n{reloaded} is back in r1 by the back edge, which is where the loop \
             reads it"
        );
        assert!(
            !conflicts[whole as usize],
            "a single segment cannot disagree"
        );
    }

    /// A caller-supplied pin (a scalar-replacement field value, which
    /// `build_live_model` cannot see) keeps the value out of registers and in a
    /// home word of its own, and is released with the deopt pins.
    #[test]
    fn a_caller_supplied_pin_keeps_a_value_home_bound_until_released() {
        let mut f = Fixture::new(12);
        let field = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let other = f.value(Op::Add, IrType::Int, 9, 10, &[10]);
        let (graph, mut live) = f.finish();

        assert_eq!(live.pin_values([field, NO_NODE, 9999]), 1);
        assert_eq!(
            live.pin_values([field]),
            0,
            "already pinned: not counted twice"
        );
        let model = bare_model(gp(2));
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.is_promoted(field),
            "a pinned value is never promoted"
        );
        assert!(alloc.is_promoted(other));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");

        // The deopt-pin release covers it, on the same write-through argument.
        assert_eq!(live.release_deopt_pins(&graph), 1);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(alloc.is_promoted(field));
    }

    /// On a real loop the helper runs and never flags a value that holds one
    /// register (or none) for its whole range.
    #[test]
    fn edge_conflicts_never_flag_an_unsplit_value() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert_eq!(conflicts.len(), graph.nodes.len());
        for (id, segs) in alloc.segments.iter().enumerate() {
            if segs.len() <= 1 {
                assert!(!conflicts[id], "n{id} has one segment and was flagged");
            }
        }
    }

    // ── Round 9, wave 2 (lane regalloc2) ─────────────────────────────

    /// A schedule with `n` empty blocks and exactly `edges` — the
    /// `ir_schedule` tests' `cfg` fixture, rebuilt here because that one is
    /// private to its module.
    fn schedule_of(n: usize, edges: &[(usize, usize)]) -> ir_schedule::Schedule {
        let mut blocks: Vec<ir_schedule::Block> = (0..n)
            .map(|id| ir_schedule::Block {
                id,
                ctrl: NO_NODE,
                nodes: Vec::new(),
                terminator: None,
                successors: Vec::new(),
                predecessors: Vec::new(),
            })
            .collect();
        for &(from, to) in edges {
            blocks[from].successors.push(to);
            blocks[to].predecessors.push(from);
        }
        let dom = ir_schedule::Dominators::compute(&blocks);
        ir_schedule::Schedule {
            blocks,
            node_to_block: Vec::new(),
            dom,
            freq: Default::default(),
            layout: Default::default(),
            pairing: Default::default(),
        }
    }

    /// A `while` whose `continue` jumps straight back to the header has two
    /// latches and is ONE loop. The per-edge model counted the body both
    /// latches reach two levels deep. Port of `ir_schedule`'s
    /// `a_loop_with_two_latches_is_one_level_deep` (irsched request 1).
    #[test]
    fn a_loop_with_two_latches_is_one_level_deep() {
        let schedule = schedule_of(5, &[(0, 1), (1, 2), (2, 3), (3, 1), (2, 1), (1, 4)]);
        assert_eq!(ls_loop_depths(&schedule), vec![0, 1, 1, 1, 0]);
    }

    /// An edge out of a block the entry never reaches is not a back edge,
    /// although `dominates` answers `true` for every dominator of dead code.
    /// Port of `ir_schedule`'s `an_edge_from_dead_code_does_not_make_a_loop`.
    #[test]
    fn an_edge_from_dead_code_does_not_make_a_loop() {
        // 0 -> 1 -> 2 is live; 3 is unreachable and jumps into 1.
        let schedule = schedule_of(4, &[(0, 1), (1, 2), (3, 1)]);
        assert!(!schedule.dom.is_reachable(3));
        assert_eq!(ls_loop_depths(&schedule), vec![0, 0, 0, 0]);
    }

    /// A value whose LAST USE is at a clobber the consumer declared
    /// read-before-clobber keeps its caller-saved register through the call;
    /// one that is still live after the call is split exactly as before; and
    /// with the declaration absent (the default) nothing changes.
    #[test]
    fn a_last_use_at_a_declared_call_keeps_its_register() {
        let build = || {
            let mut f = Fixture::new(8);
            let dies = f.value(Op::Add, IrType::Int, 0, 5, &[3, 5]);
            let (graph, live) = f.finish();
            (graph, live, dies)
        };
        let clobbered = |declare: bool| {
            let mut model = bare_model(gp_volatile_first(1, 0));
            model.add_clobbers(5, &[PhysReg::gp(0)]);
            if declare {
                model.reads_precede_clobber = vec![5];
            }
            model
        };

        // Declared: one segment, through the call.
        let (graph, live, dies) = build();
        let model = clobbered(true);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.segments[dies as usize],
            vec![Segment {
                range: PosRange { lo: 0, hi: 5 },
                reg: Some(PhysReg::gp(0)),
            }],
            "the operand is read before the call destroys r0, and nothing reads \
             it again"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("proof 4 exempts it");

        // The same allocation against the default model is the old violation.
        let err = verify_allocation(&graph, &live, &clobbered(false), &alloc)
            .expect_err("without the declaration a register live at a clobber is refused");
        assert_eq!(
            internal_message(&err),
            "regalloc: a value is live in a clobbered register"
        );

        // Default: the pre-change split.
        let model = clobbered(false);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            alloc.segments[dies as usize].len() > 1,
            "no declaration, no exemption: {:?}",
            alloc.segments[dies as usize]
        );

        // Declared, but the value is read AGAIN after the call: still split.
        let mut f = Fixture::new(8);
        let survives = f.value(Op::Add, IrType::Int, 0, 6, &[3, 6]);
        let (graph, live) = f.finish();
        let model = clobbered(true);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            alloc.segments[survives as usize]
                .iter()
                .all(|s| !(s.reg == Some(PhysReg::gp(0)) && s.range.contains(5))),
            "a value live past the call must not keep a register the call \
             destroys: {:?}",
            alloc.segments[survives as usize]
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// The admit-everything form (`|_| true`) lists exactly the call positions that
    /// carry a clobber: an FP remainder (a helper call) is listed, an integer
    /// remainder (inline `idiv`) is not, and a call position the model records
    /// no clobber at is not.
    #[test]
    fn marking_call_operand_reads_lists_only_clobbered_call_positions() {
        let mut f = Fixture::new(12);
        let fp_rem = f.value(Op::Rem, IrType::Double, 4, 6, &[6]);
        let _int_rem = f.value(Op::Rem, IrType::Int, 7, 8, &[8]);
        let _unclobbered = f.value(Op::Rem, IrType::Double, 9, 10, &[10]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 0));
        model.add_clobbers(4, &[PhysReg::gp(0)]);
        model.add_clobbers(7, &[PhysReg::gp(0)]);
        assert!(model.reads_precede_clobber.is_empty(), "empty by default");
        assert_eq!(
            model.mark_operand_reads_first_where(&graph, &live, |_| true),
            1
        );
        assert_eq!(model.reads_precede_clobber, vec![4]);
        assert_eq!(live.pos_of[fp_rem as usize], Some(4));
        assert_eq!(
            model.mark_operand_reads_first_where(&graph, &live, |_| true),
            0,
            "idempotent"
        );
    }

    /// Round 9 wave 4 (irlower4): the filtered form lists only the call
    /// positions the backend admitted -- here the store, not the FP remainder
    /// -- and keeps the unfiltered form's filters (a non-call and an
    /// unclobbered position are never listed).
    #[test]
    fn marking_admitted_call_operand_reads_lists_only_admitted_ops() {
        let mut f = Fixture::new(12);
        let _fp_rem = f.value(Op::Rem, IrType::Double, 4, 6, &[6]);
        let store = f.value(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Double,
            7,
            8,
            &[8],
        );
        let _add = f.value(Op::Add, IrType::Double, 9, 10, &[10]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 0));
        model.add_clobbers(4, &[PhysReg::gp(0)]);
        model.add_clobbers(7, &[PhysReg::gp(0)]);
        model.add_clobbers(9, &[PhysReg::gp(0)]);
        let added =
            model.mark_operand_reads_first_where(&graph, &live, |n| matches!(n.op, Op::Store(_)));
        assert_eq!(added, 1);
        assert_eq!(model.reads_precede_clobber, vec![7]);
        assert_eq!(live.pos_of[store as usize], Some(7));
        // ...and the unfiltered form still adds the remainder on top.
        assert_eq!(
            model.mark_operand_reads_first_where(&graph, &live, |_| true),
            1
        );
        assert_eq!(model.reads_precede_clobber, vec![4, 7]);
    }

    /// Four blocks laid out `latch, entry, header, exit`: the latch (block 0)
    /// comes BEFORE the definitions in block 1, and its back edge `0 -> 2` is
    /// not a fall-through. Spans `0:(0,2) 1:(3,5) 2:(6,9) 3:(10,12)`.
    fn latch_first_fixture() -> (Fixture, ir_schedule::Schedule) {
        let schedule = schedule_of(4, &[(0, 2), (1, 2), (2, 0), (2, 3)]);
        let mut f = Fixture::new(13);
        f.live.span = vec![(0, 2), (3, 5), (6, 9), (10, 12)];
        f.live.block_of_pos = vec![0, 0, 0, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3];
        (f, schedule)
    }

    /// The wave-1 rule compared raw timelines, so a predecessor laid out
    /// before a value's definition "delivered" the register the timeline names
    /// there — a register the layout-order emitter never filled on that path.
    /// It now delivers memory, and the conflict is flagged; and a target laid
    /// out before the definition claims nothing, so it is never flagged.
    #[test]
    fn a_predecessor_laid_out_before_the_definition_delivers_memory() {
        let (mut f, schedule) = latch_first_fixture();
        let v = f.value(Op::Add, IrType::Int, 0, 9, &[4, 7, 9]);
        let w = f.value(Op::Add, IrType::Int, 0, 9, &[8, 9]);
        let (_graph, mut live) = f.finish();
        // `v` is defined at 4 (block 1), `w` at 7 (block 2); both ranges were
        // widened back to 0 by the loop.
        live.pos_of[v as usize] = Some(4);
        live.pos_of[w as usize] = Some(7);
        let seg = |lo: usize, hi: usize, reg: Option<u8>| Segment {
            range: PosRange { lo, hi },
            reg: reg.map(PhysReg::gp),
        };
        let alloc = Allocation {
            segments: vec![
                vec![seg(0, 7, Some(0)), seg(8, 9, None)],
                vec![seg(0, 5, Some(1)), seg(6, 9, Some(2))],
            ],
            stack_slot: vec![Some(0), Some(1)],
            stack_slots: 2,
            ..Default::default()
        };

        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert!(
            conflicts[v as usize],
            "block 2 was emitted with n{v} in r0 (carried from its definition in \
             block 1); the latch, laid out before that definition, never put it \
             there"
        );
        assert!(
            !conflicts[w as usize],
            "n{w} is defined inside block 2, so nothing carried into block 2 \
             claims a register for it"
        );

        let res = split_edge_resolution(&schedule, &live, &alloc);
        assert_eq!(res.unresolvable, vec![false, false]);
        assert_eq!(
            res.edges,
            vec![EdgeResolution {
                pred: 0,
                succ: 2,
                placement: EdgeCopyPlacement::PredEnd,
                copies: vec![(ValueLoc::Reg(PhysReg::gp(0)), ValueLoc::Slot(0))],
                nodes: vec![v],
            }],
            "the latch has one successor, so the reload goes at its end"
        );
    }

    /// Resolution on the wave-1 back-edge fixture, plus a register exchange
    /// across the back edge (a copy CYCLE, which only a parallel sequencing
    /// gets right) and a value whose target register already belongs to
    /// another value at the top of the loop (unresolvable, copies withdrawn).
    #[test]
    fn edge_resolution_repairs_exactly_the_flagged_values() {
        let mut f = Fixture::new(13);
        let evicted = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let a = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let b = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let blocked = f.value(Op::Add, IrType::Int, 1, 9, &[9]);
        let squatter = f.value(Op::Add, IrType::Int, 3, 9, &[9]);
        let (_graph, mut live) = f.finish();
        live.span = vec![(0, 2), (3, 9), (10, 12)];
        let schedule = self_loop_schedule();

        let seg = |lo: usize, hi: usize, reg: Option<u8>| Segment {
            range: PosRange { lo, hi },
            reg: reg.map(PhysReg::gp),
        };
        let alloc = Allocation {
            segments: vec![
                // evicted: r0 into the loop, memory at the back edge.
                vec![seg(1, 5, Some(0)), seg(6, 9, None)],
                // a and b trade r1 and r2 inside the loop.
                vec![seg(1, 5, Some(1)), seg(6, 9, Some(2))],
                vec![seg(1, 5, Some(2)), seg(6, 9, Some(1))],
                // blocked: r3 up to the loop top, memory after...
                vec![seg(1, 2, Some(3)), seg(3, 9, None)],
                // ...because the squatter holds r3 from the loop top on.
                vec![seg(3, 9, Some(3))],
            ],
            stack_slot: vec![Some(0), Some(1), Some(2), Some(3), None],
            stack_slots: 4,
            ..Default::default()
        };

        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert_eq!(conflicts, vec![true, true, true, true, false]);

        let res = split_edge_resolution(&schedule, &live, &alloc);
        assert_eq!(
            res.unresolvable,
            vec![false, false, false, true, false],
            "n{blocked}'s copy would overwrite n{squatter}, which the loop top \
             reads from r3"
        );
        assert_eq!(res.edges.len(), 1);
        let edge = &res.edges[0];
        assert_eq!((edge.pred, edge.succ), (1, 1));
        assert_eq!(
            edge.placement,
            EdgeCopyPlacement::Critical,
            "block 1 has two successors and two predecessors"
        );
        let r = |n: u8| ValueLoc::Reg(PhysReg::gp(n));
        assert_eq!(edge.nodes, vec![evicted, a, b]);
        assert_eq!(
            edge.copies,
            vec![(r(0), ValueLoc::Slot(0)), (r(1), r(2)), (r(2), r(1))]
        );

        // Every flagged value is either repaired or refused — the gate and the
        // resolver read one enumeration.
        for (id, &flagged) in conflicts.iter().enumerate() {
            let repaired = res.edges.iter().any(|e| e.nodes.contains(&(id as NodeId)));
            assert_eq!(
                flagged,
                repaired || res.unresolvable[id],
                "n{id}: flagged={flagged} repaired={repaired}"
            );
        }

        // And the copy is a real parallel copy: after sequencing, each register
        // holds what the loop top expects. Tokens: 100+id for a value's home
        // word, and the back-edge register contents as the timeline has them.
        let start = vec![
            (ValueLoc::Slot(0), 100 + evicted),
            (r(2), 100 + a),
            (r(1), 100 + b),
        ];
        let ops = check_parallel(&edge.copies);
        let after = run_copies(&start, &ops);
        assert_eq!(after.get(&r(0)), Some(&(100 + evicted)));
        assert_eq!(after.get(&r(1)), Some(&(100 + a)));
        assert_eq!(after.get(&r(2)), Some(&(100 + b)));
    }

    /// A value the model KNOWS is not live-in at the loop top is not held
    /// against the back edge: nothing past the edge reads it.
    #[test]
    fn a_value_not_live_into_the_target_is_not_an_edge_conflict() {
        let mut f = Fixture::new(13);
        let evicted = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let (_graph, mut live) = f.finish();
        live.span = vec![(0, 2), (3, 9), (10, 12)];
        let schedule = self_loop_schedule();
        let alloc = Allocation {
            segments: vec![vec![
                Segment {
                    range: PosRange { lo: 1, hi: 5 },
                    reg: Some(PhysReg::gp(0)),
                },
                Segment {
                    range: PosRange { lo: 6, hi: 9 },
                    reg: None,
                },
            ]],
            stack_slot: vec![Some(0)],
            stack_slots: 1,
            ..Default::default()
        };
        assert!(split_edge_conflicts(&schedule, &live, &alloc)[evicted as usize]);

        // Block 1's live-in set, as a converged model would record it, without
        // `evicted` in it.
        live.block_live_in = vec![vec![0], vec![0], vec![0]];
        assert_eq!(live.live_in_at(1, evicted), Some(false));
        assert!(!split_edge_conflicts(&schedule, &live, &alloc)[evicted as usize]);
        assert_eq!(
            split_edge_resolution(&schedule, &live, &alloc),
            SplitEdgeResolution {
                edges: Vec::new(),
                unresolvable: vec![false],
            }
        );

        // And with it in: flagged again.
        live.block_live_in[1][0] |= 1u64 << evicted;
        assert!(split_edge_conflicts(&schedule, &live, &alloc)[evicted as usize]);
    }

    /// On a real scheduled loop the model keeps per-block live-in sets, and a
    /// loop-carried value is live into the header.
    #[test]
    fn a_converged_model_keeps_per_block_live_in_sets() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        assert!(live.converged);
        assert_eq!(live.block_live_in.len(), schedule.blocks.len());
        let mut phis = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !matches!(node.op, Op::Phi) || !live.wants_loc[id] {
                continue;
            }
            let Some(pos) = live.pos_of[id] else { continue };
            let b = live.block_of_pos[pos];
            phis += 1;
            assert_eq!(
                live.live_in_at(b, id as NodeId),
                Some(false),
                "a phi is defined in its own block, not live into it"
            );
        }
        assert!(phis > 0, "the swap loop has loop-header phis");
        // Resolution runs on a real allocation and every copy it proposes is
        // a well-formed parallel copy.
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let res = split_edge_resolution(&schedule, &live, &alloc);
        for edge in &res.edges {
            resolve_parallel_copy(&edge.copies).expect("sequentialises");
        }
    }

    /// Round 11 wave 5: a value live THROUGH a loop it is never read in is
    /// `carried` (its range covers the back edge) but not `carried_and_read`,
    /// so the spill heuristic no longer shields it like a per-iteration value;
    /// the loop's own phis are both.
    #[test]
    fn a_value_live_through_a_loop_it_never_reads_is_not_carried_and_read() {
        // static int f(int a, int n) { int s = a, i = 0;
        //     do { s += i; i++; } while (i < n); return s + a; }
        let code = [
            0x1a, 0x3d, // 0: iload_0; istore_2          s = a
            0x03, 0x3e, // 2: iconst_0; istore_3         i = 0
            0x1c, 0x1d, 0x60, 0x3d, // 4: s += i           loop header
            0x84, 0x03, 0x01, // 8: iinc 3, 1
            0x1d, 0x1b, // 11: iload_3; iload_1
            0xa1, 0xff, 0xf7, // 13: if_icmplt 4
            0x1c, 0x1a, 0x60, 0xac, // 16: return s + a
            0, 0,
        ];
        let mut graph = IrBuilder::new(2, 4)
            .build(&code, 20)
            .expect("the do-while builds");
        // The builder gives every local a header φ; the optimizer drops the
        // trivial one for `a`, so `a` is the parameter itself, as in production.
        crate::ir_optimize::optimize(&mut graph);
        let schedule = ir_schedule::schedule(&graph);
        let live = build_live_model(&graph, &schedule);
        assert!(live.converged);
        let a = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Param(0))
            .expect("parameter a");
        assert!(
            live.carried[a],
            "fixture: `a` is defined before the loop and read after it, so its \
             range covers the back edge (range {:?})",
            live.range[a]
        );
        assert!(
            !live.carried_and_read[a],
            "`a` is never read inside the loop; evicting it costs one reload, not \
             one per iteration"
        );
        // The loop's own φs (`s`, `i`) are read every iteration. Only "some φ"
        // is asserted: the optimizer may unroll and add exit φs of its own.
        assert!(
            graph
                .nodes
                .iter()
                .enumerate()
                .any(|(id, node)| node.op == Op::Phi && live.carried_and_read[id]),
            "a loop-header φ is carried and read"
        );
        // `carried_and_read` never names a value `carried` does not.
        for id in 0..graph.nodes.len() {
            assert!(!live.carried_and_read[id] || live.carried[id], "n{id}");
        }
    }

    /// `ir_lower::scalar_replacement_field_values` fed to `pin_values` pins exactly `plan_slots`' third
    /// population: every field value, skipping `None` and `NO_NODE`.
    #[test]
    fn scalar_replacement_field_values_are_pinned_from_the_map() {
        let mut f = Fixture::new(12);
        let field = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let other = f.value(Op::Add, IrType::Int, 9, 10, &[10]);
        let (graph, mut live) = f.finish();
        let mut sr = crate::ir_lower::ScalarReplacementMap::default();
        sr.objects.insert(
            1000,
            crate::ir_lower::VirtualObjectInfo {
                loop_phis: None,
                class_id: 1,
                array_element_type: None,
                num_fields: 3,
                field_values: vec![Some(field), None, Some(NO_NODE)],
                new_ctrl: NO_NODE,
                store_ctrls: Vec::new(),
                store_nodes: Vec::new(),
                store_fields: Vec::new(),
            },
        );
        assert_eq!(
            live.pin_values(crate::ir_lower::scalar_replacement_field_values(&sr)),
            1
        );
        assert!(live.pinned[field as usize]);
        assert!(!live.pinned[other as usize]);
        let model = bare_model(gp(2));
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.is_promoted(field),
            "a field value keeps its home word"
        );
        assert!(alloc.is_promoted(other));
    }

    // ── Round 11 wave 2 (lane ircore) ────────────────────────────────────

    /// An early-clobber segment ends AT the `idiv` that destroys its register;
    /// the value's one store must be planned BEFORE that instruction, not at
    /// it (where the register already holds the quotient).
    #[test]
    fn an_early_clobber_segment_is_stored_before_the_clobbering_instruction() {
        let mut f = Fixture::new(12);
        let dividend = f.value(Op::Add, IrType::Int, 0, 9, &[4, 9]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.fixed = vec![FixedConstraint {
            node: dividend,
            pos: 4,
            reg: PhysReg::gp(0),
        }];
        model.add_clobbers(4, &[PhysReg::gp(0), PhysReg::gp(1)]);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(dividend, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the fixture must take the early-clobber path"
        );
        let stores: Vec<&SpillEvent> = alloc
            .events
            .iter()
            .filter(|e| e.node == dividend && e.kind == SpillKind::Store)
            .collect();
        assert_eq!(stores.len(), 1, "{:?}", alloc.events);
        assert_eq!(
            (stores[0].pos, stores[0].from),
            (3, Some(PhysReg::gp(0))),
            "the store must run before the idiv at 4 overwrites r0"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// A value DEFINED into its home (it lost the register contest at its
    /// definition) needs no store when a later register segment ends: the
    /// home already holds it. Only its reloads are planned.
    #[test]
    fn a_value_defined_in_memory_is_never_stored() {
        let mut f = Fixture::new(12);
        let w = f.value(Op::Add, IrType::Int, 0, 4, &[2, 4]);
        let v = f.value(Op::Add, IrType::Int, 1, 9, &[6, 9]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(1));
        model.add_clobbers(8, &[PhysReg::gp(0)]);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let segs = &alloc.segments[v as usize];
        assert_eq!(segs.first().map(|s| s.reg), Some(None), "{segs:?}");
        assert!(
            segs.iter().any(|s| s.reg.is_some()),
            "the value must visit a register for the case to mean anything: {segs:?}"
        );
        assert!(
            segs.windows(2)
                .any(|p| p[0].reg.is_some() && p[1].reg.is_none()),
            "…and leave it again: {segs:?}"
        );
        assert!(
            !alloc
                .events
                .iter()
                .any(|e| e.node == v && e.kind == SpillKind::Store),
            "{:?}",
            alloc.events
        );
        assert_eq!(alloc.first_reg(w), Some(PhysReg::gp(0)));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }
}

// ── REVIEW-NOTE ──────────────────────────────────────────────────────
//
// Changes this pass could not make in place, recorded here rather than made
// silently. Nothing below is applied. Each is either a statement about a file
// outside the two this pass was allowed to touch, or a deliberate deviation
// found inside them and left alone on purpose.
//
// REVIEW-NOTE 1 — `docs/JIT_OPTIMIZATION.md` is stale in two load-bearing
// places, and one of them is the paragraph this pass was pointed at.
//
//   (a) The closing paragraph of "Register residency in the optimizing tier"
//       reads:
//
//         > **Known limit, named rather than guessed at:** a loop counter and
//         > a loop accumulator are `Op::Phi` at the loop header, and phis are
//         > excluded — the allocator refuses them […] Reaching loop-carried
//         > values therefore needs the allocator to admit phis and
//         > `emit_phi_copies` to publish
//
//       Phis have been admitted since 2026-09-02. `ir_op_defines_value` lists
//       `Op::Phi`, `LiveModel::release_phi_pins` drops the structural pin,
//       `ir_lower::plan_register_residency` admits them behind
//       `ir_phi_residency_enabled` (default ON *within* the outer flag), and
//       `emit_phi_copies` publishes at every incoming edge. Replace that
//       paragraph with the state of the code:
//
//         **Loop-carried values reach the file.** A loop counter and a loop
//         accumulator are `Op::Phi` at the loop header, and phis are admitted
//         (`CRATONVM_JIT_IR_PHI_RESIDENCY`, default ON within the outer flag).
//         The publish site a phi never had is `emit_phi_copies`: it writes the
//         home word at every incoming edge exactly as before and then moves
//         the value into the phi's register, so the write-through contract is
//         unchanged and the register is a cache. The allocator's structural
//         pin is dropped by `release_phi_pins` on the same condition. A
//         `Ref`-typed phi is still refused, by type — `OopMapEntry` names
//         frame slots only, and a phi's publish site sits outside the per-node
//         invalidation loop that `CRATONVM_JIT_IR_REF_RESIDENCY` rests on.
//         `[ir-ls] phis=N: resident=… disabled=… ref_type=… no_register=…
//         no_home=… wrong_bank=… demoted=…` is the per-cause census, and its
//         `disabled=` field is what the old `phi=` field measured.
//
//   (b) The same section says `CRATONVM_JIT_IR_LINEAR_SCAN` is "default off"
//       and that "the flip is a separate decision that still wants a
//       measurement on a quiet host". `ir_lower::linear_scan_enabled` has been
//       default ON since 2026-09-02 and its own doc comment says so. "Off is
//       exactly the pre-change emission" is still true and should stay;
//       "default off" should become "default ON since 2026-09-02; `=0` is the
//       kill switch". This matters because the section is what a reader
//       consults before deciding what a measurement is measuring, and it
//       currently states the opposite default.
//
// REVIEW-NOTE 2 — the write-through contract has a deliberate exception, and
// this pass did not introduce it and did not remove it.
//
//   The contract as the doc states it — "the home word is written at every
//   definition, so `emit_safepoint_map`, `build_deopt_points` and
//   `emit_phi_copies` read exactly what they always did" — is no longer
//   unconditional in `ir_lower`. `Lowerer::phi_home_droppable` (which requires
//   `CRATONVM_JIT_IR_DROP_PHI_HOME`, `ir-deopt-regs`, `ir-phi-copy-regs` and
//   `ir-skip-republish` together) lets `emit_copy_op` SKIP a resident phi's
//   home store, and `value_home_droppable` does the same for ordinary values.
//
//   Left alone, deliberately. It is confined to `Int`/`Long`, so no oop map
//   entry and no `Ref` root is affected; the one reader that used to trust the
//   home unconditionally is guarded (`ir_phi_home_publish_guard_enabled` in
//   `emit_phi_copies`, added after `Select.processGroupResult` published a phi
//   from a word nothing wrote and collapsed H2 window-query result sets); and
//   that guard has a source-scanning test pinning its reader set. Restoring
//   the literal contract would delete a measured optimisation and its safety
//   net in one move.
//
//   What is worth doing is a documentation change, not a code one: the
//   sentence quoted above should read "the home word is written at every
//   definition **unless `ir-drop-phi-home` / `ir-drop-home` prove no reader
//   needs it** — an `Int`/`Long`-only rule that never applies to a `Ref`."
//
// REVIEW-NOTE 3 — `plan_register_residency` and the skip census live in
// `jit/src/ir_lower.rs`, not here.
//
//   The brief this pass worked from names `regalloc::plan_register_residency`,
//   `regalloc::wants_loc` and "the skip census" as this module's. `wants_loc`
//   is this module's (a `LiveModel` field built by `build_live_model`);
//   `plan_register_residency` and both censuses are `ir_lower`'s, and that is
//   the right home for them — the census describes what *that* emitter could
//   take, and this module must not know which registers that emitter reserves
//   for its own tiers. Recorded so the next reader of that brief does not go
//   looking here.

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
        assert!(handler_has_unsafe_local_read(
            &code,
            code.len(),
            0,
            u64::MAX
        ));
    }

    /// Round 11: a parameter the method reassigns is not restored by the
    /// params-only handler frame (it restores the INCOMING value), so a
    /// handler reading it is unsafe.
    ///
    /// `static int f(int x) { try { x = 42; thrower(); } catch (E e) { return x; } return -1; }`
    #[test]
    fn a_handler_reading_a_reassigned_parameter_is_unsafe() {
        //  0: bipush 42   2: istore_0   3: invokestatic #2   6: iconst_m1   7: ireturn
        //  8: astore_1  (handler)       9: iload_0          10: ireturn
        let code = [
            0x10, 0x2a, 0x3b, 0xb8, 0x00, 0x02, 0x02, 0xac, 0x4c, 0x1a, 0xac,
        ];
        assert!(
            handler_has_unsafe_local_read(&code, code.len(), 8, 1),
            "the catch reads x, which the try body overwrote"
        );
        // Control: the same method without the store keeps x safe.
        let code = [
            0x00, 0x00, 0x00, 0xb8, 0x00, 0x02, 0x02, 0xac, 0x4c, 0x1a, 0xac,
        ];
        assert!(!handler_has_unsafe_local_read(&code, code.len(), 8, 1));
        // A handler that stores the parameter itself before reading it is
        // still safe: the dataflow sees the handler-local definition.
        //  8: astore_1  9: iconst_5  10: istore_0  11: iload_0  12: ireturn
        let code = [
            0x10, 0x2a, 0x3b, 0xb8, 0x00, 0x02, 0x02, 0xac, 0x4c, 0x08, 0x3b, 0x1a, 0xac,
        ];
        assert!(!handler_has_unsafe_local_read(&code, code.len(), 8, 1));
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

    // CM-FASTMATH — `ldc`/`ldc_w`/`ldc2_w` were missing from `bytecode_analysis::step` (the
    // x64.rs `bytecode_analysis::step` twin had them; this copy didn't), so liveness
    // walks read constant-pool index operand bytes as opcodes. Pin the
    // constant-load lengths so the tables cannot drift apart again.
    #[test]
    fn bc_len_constant_loads() {
        // opcode byte + dummy operand bytes; `bytecode_analysis::step` only looks at code[pc]
        // for these arms.
        assert_eq!(bytecode_analysis::step(&[0x12, 0xBB], 0), 2, "ldc");
        assert_eq!(bytecode_analysis::step(&[0x13, 0x00, 0xBB], 0), 3, "ldc_w");
        assert_eq!(bytecode_analysis::step(&[0x14, 0x00, 0xBB], 0), 3, "ldc2_w");
        assert_eq!(bytecode_analysis::step(&[0x10, 0x7F], 0), 2, "bipush");
        assert_eq!(bytecode_analysis::step(&[0x11, 0x12, 0x34], 0), 3, "sipush");
        assert_eq!(bytecode_analysis::step(&[0xa8, 0x00, 0x10], 0), 3, "jsr");
        assert_eq!(bytecode_analysis::step(&[0xa9, 0x04], 0), 2, "ret");
    }

    // Defense-in-depth (same class as the missing-`ldc` CM-FASTMATH bug): the
    // 5-byte instructions invokeinterface (0xb9), invokedynamic (0xba), goto_w
    // (0xc8) and jsr_w (0xc9). Only invokeinterface is reachable today (the
    // other three are rejected by `jit_scan`), but a missing length entry
    // under-counts the instruction by 4 bytes and desyncs every PC-stepping
    // walk — so all four must read 5, and must match the x64.rs
    // `bytecode_analysis::step` twin. Operand bytes are dummies; `bytecode_analysis::step` only reads
    // `code[pc]` for these arms.
    #[test]
    fn bc_len_five_byte_ops() {
        assert_eq!(
            bytecode_analysis::step(&[0xb9, 0x00, 0x10, 0x02, 0x00], 0),
            5,
            "invokeinterface"
        );
        assert_eq!(
            bytecode_analysis::step(&[0xba, 0x00, 0x10, 0x00, 0x00], 0),
            5,
            "invokedynamic"
        );
        assert_eq!(
            bytecode_analysis::step(&[0xc8, 0x00, 0x00, 0x00, 0x10], 0),
            5,
            "goto_w"
        );
        assert_eq!(
            bytecode_analysis::step(&[0xc9, 0x00, 0x00, 0x00, 0x10], 0),
            5,
            "jsr_w"
        );
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
        assert_eq!(bytecode_analysis::offset_branch_target(&goto_w, 0), Some(5));
        assert_eq!(bytecode_analysis::offset_branch_target(&jsr_w, 0), Some(5));
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
        assert!(
            solve_liveness(&mut blocks, 1),
            "a two-block loop converges well inside MAX_LIVENESS_ITERATIONS"
        );
    }

    /// A chain of `n` blocks in which block `i`'s only successor is block
    /// `i - 1`, and only block 0 uses local 0.
    ///
    /// `solve_liveness` sweeps block indices in REVERSE order, so a successor
    /// with a LOWER index is always read from the previous sweep: the live bit
    /// advances exactly one block per iteration. That makes the number of
    /// sweeps needed equal to the chain length, which is the cheapest way to
    /// build a CFG that genuinely exhausts MAX_LIVENESS_ITERATIONS. Real
    /// bytecode reaches the same shape through back-edge chains in
    /// obfuscated or decompiler-generated methods.
    fn descending_liveness_chain(n: usize) -> Vec<BasicBlock> {
        (0..n)
            .map(|i| BasicBlock {
                start_pc: i,
                end_pc: i + 1,
                successors: if i == 0 { Vec::new() } else { vec![i - 1] },
                handler_successors: Vec::new(),
                gen: if i == 0 { 1 } else { 0 },
                kill: 0,
                live_in: 0,
                live_out: 0,
            })
            .collect()
    }

    #[test]
    fn solve_liveness_reports_convergence_when_the_fixpoint_is_reached() {
        let mut blocks = descending_liveness_chain(16);
        assert!(solve_liveness(&mut blocks, 0));
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.live_in, 1, "local 0 must be live-in at block {i}");
        }
    }

    #[test]
    fn solve_liveness_reports_failure_when_the_iteration_cap_truncates_it() {
        // One block more than the cap can propagate through, plus the extra
        // sweep a converged run needs to observe "nothing changed".
        let n = MAX_LIVENESS_ITERATIONS + 8;
        let mut blocks = descending_liveness_chain(n);
        assert!(
            !solve_liveness(&mut blocks, 0),
            "a chain longer than MAX_LIVENESS_ITERATIONS cannot have converged"
        );
        // The point of the flag: what is left behind is not "approximate", it
        // is a strict SUBSET of the truth. Local 0 IS live-in at the top of
        // the chain, and the truncated run says it is not — which as an
        // interference graph means a missing edge.
        assert_eq!(
            blocks[n - 1].live_in,
            0,
            "the truncated result under-states liveness, which is why the \
             caller must not use it"
        );
    }

    /// Bytecode whose liveness fixpoint cannot converge inside
    /// `MAX_LIVENESS_ITERATIONS`: `iload_0; ireturn` followed by a chain of
    /// `goto`s each branching to the previous one. Block indices follow pc
    /// order, every `goto` block's successor has a lower index, and only the
    /// first block uses local 0 — the `descending_liveness_chain` shape,
    /// expressed as real bytecode so it reaches `allocate_registers_with`.
    fn non_converging_goto_chain(gotos: usize) -> Vec<u8> {
        let mut code = vec![0x1a, 0xac]; // 0: iload_0  1: ireturn
        for k in 0..gotos {
            // The goto at pc `2 + 3k` targets pc `2 + 3(k-1)`, i.e. -3; the
            // first one targets pc 0, i.e. -2.
            let off: i16 = if k == 0 { -2 } else { -3 };
            code.push(0xa7);
            code.extend_from_slice(&off.to_be_bytes());
        }
        code
    }

    #[test]
    fn allocate_registers_hands_out_no_homes_when_liveness_does_not_converge() {
        let code = non_converging_goto_chain(MAX_LIVENESS_ITERATIONS + 8);
        let code_len = code.len();

        // Sanity: this really is the shape the fixpoint cannot chew through.
        let mut blocks = build_cfg(&code, code_len);
        for block in &mut blocks {
            compute_gen_kill(&code, block);
        }
        assert!(
            blocks.len() > MAX_LIVENESS_ITERATIONS,
            "expected one block per goto, got {}",
            blocks.len()
        );
        assert!(!solve_liveness(&mut blocks, 1));

        // The allocator must refuse rather than colour a graph that is missing
        // interference edges. Every local keeps its canonical frame slot.
        let result = allocate_registers_with(
            &code,
            code_len,
            4, // num_locals
            1, // num_params
            &[],
            &[],
            &LOCAL_REGS,
            &LOCAL_XMMS,
            &[],
        );
        assert!(
            result.assignments.iter().all(|a| a.is_none()),
            "no local may receive a register home from a non-converged \
             liveness solution: {:?}",
            result.assignments
        );
        assert!(result.xmm_assignments.iter().all(|a| a.is_none()));
        assert!(result.used_callee_saved.is_empty());
        assert!(result.used_xmm_regs.is_empty());
    }

    #[test]
    fn the_per_pc_liveness_map_widens_to_all_live_when_it_does_not_converge() {
        // The map builders cannot simply bail — their callers need a row for
        // every pc — so they take the other fail-closed direction: over-state
        // liveness. Under-stating it would publish a live local as
        // `Undefined` in a deopt frame.
        let code = non_converging_goto_chain(MAX_LIVENESS_ITERATIONS + 8);
        let live = live_locals_per_pc(&code, code.len(), 1);
        assert_eq!(
            live[2],
            u64::MAX,
            "a goto block deep in the chain must be reported all-live, not \
             all-dead, once the fixpoint is known to be truncated"
        );
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
        assert!(bytecode_analysis::offset_branch_target(&code, 0).is_none());
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

    /// Round 11: a parameter the entry block OVERWRITES before reading is not
    /// live-in, so liveness gave it no edge to an earlier parameter that is
    /// dead by the time of the overwrite — and the two shared a register the
    /// prologue writes twice.
    ///
    /// `static int f(int a, int b) { b = a * 2; return b; }`
    #[test]
    fn a_parameter_overwritten_before_it_is_read_still_interferes_with_the_others() {
        // 0: iload_0  1: iconst_2  2: imul  3: istore_1  4: iload_1  5: ireturn
        let code = [0x1a, 0x05, 0x68, 0x3c, 0x1b, 0xac];
        let result = allocate_registers(&code, code.len(), 2, 2, &[]);
        let a = result.assignments.first().copied().flatten();
        let b = result.assignments.get(1).copied().flatten();
        if let (Some(x), Some(y)) = (a, b) {
            assert_ne!(
                x, y,
                "parameters 0 and 1 share register {x}: the prologue stores arg1 over \
                 arg0 before `iload_0` reads it"
            );
        }
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
        assert!(solve_liveness(&mut blocks, 1));
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
            assert!(solve_liveness(&mut blocks, 0));
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
        assert!(solve_liveness(&mut blocks, 1));
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

    /// r9: `state` is defined before the `try`, redefined inside it, and read
    /// by the `catch`. `tmp` lives and dies BEFORE the `try`.
    ///
    /// ```java
    /// int state = 0;
    /// int tmp = 5; use(tmp);
    /// try { call(); state = 1; return state; }
    /// catch (Throwable t) { return state; }
    /// ```
    ///
    /// Walking backward, the `istore_1` inside the range used to kill `state`
    /// for everything before it — the handler's read reached the protected pcs'
    /// ROWS but never the running live set, and never the block's live-in — so
    /// `state` was dead across `tmp`'s whole life, the two did not interfere,
    /// and a colouring was free to put both in one register. An exception at
    /// pc 6 then entered the handler with `tmp`'s 5 in place of `state`'s 0.
    const STATE_BEFORE_TRY: [u8; 16] = [
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1        state = 0
        0x08, // 2: iconst_5
        0x3d, // 3: istore_2        tmp = 5
        0x1c, // 4: iload_2
        0x57, // 5: pop             tmp dies here
        0xb8, 0x00, 0x01, // 6: invokestatic   <- protected [6, 11)
        0x04, // 9: iconst_1
        0x3c, // 10: istore_1       state = 1
        0x1b, // 11: iload_1
        0xac, // 12: ireturn
        0x4e, // 13: astore_3       <- handler
        0x1b, // 14: iload_1        reads state
        0xac, // 15: ireturn
    ];
    const STATE_BEFORE_TRY_HANDLERS: [(usize, usize, usize); 1] = [(6, 11, 13)];

    #[test]
    fn a_local_the_handler_reads_is_live_before_the_protected_range() {
        let code = STATE_BEFORE_TRY;
        let (live, covered) =
            live_locals_per_pc_with_handlers(&code, code.len(), 0, &[], &STATE_BEFORE_TRY_HANDLERS);
        for pc in [2usize, 3, 4, 5] {
            assert!(covered[pc], "pc {pc} must be covered");
            assert_ne!(
                live[pc] & (1 << 1),
                0,
                "local 1 is read by the handler, the throw at pc 6 reaches it, and \
                 nothing between its def at pc 1 and pc 6 redefines it — so it is \
                 live at pc {pc}. live_at[{pc}] = {:#x}",
                live[pc]
            );
        }
        assert_eq!(
            live[1] & (1 << 1),
            0,
            "the exception edge must not make local 1 live before its own first def"
        );

        // The same answer from the windowed analysis the deopt snapshot uses.
        let (rows, _, words) =
            live_locals_per_pc_all(&code, code.len(), 0, &[], &STATE_BEFORE_TRY_HANDLERS, 4);
        assert_eq!(words, 1);
        assert_ne!(
            rows[4] & (1 << 1),
            0,
            "window 0 must agree with the 64-slot map"
        );

        // Control: without the exception table, local 1 is genuinely dead there.
        let (blind, _) = live_locals_per_pc_with_handlers(&code, code.len(), 0, &[], &[]);
        assert_eq!(
            blind[4] & (1 << 1),
            0,
            "precondition: with no exception edge the read is invisible, so the \
             assertion above is about the edge and not about the fixture"
        );
    }

    #[test]
    fn a_local_the_handler_reads_interferes_with_a_temporary_that_dies_before_the_try() {
        let code = STATE_BEFORE_TRY;
        let mut blocks = build_cfg_with_handlers(&code, code.len(), &STATE_BEFORE_TRY_HANDLERS);
        for block in &mut blocks {
            compute_gen_kill(&code, block);
        }
        assert!(solve_liveness(&mut blocks, 0));
        let ranges = handler_live_in_ranges(&blocks, &STATE_BEFORE_TRY_HANDLERS);
        let interference = build_interference(&code, &blocks, 4, &ranges);
        assert_ne!(
            interference[2] & (1 << 1),
            0,
            "`tmp` (local 2) is defined at pc 3 while `state` (local 1) must \
             survive for the handler; sharing a register would hand the catch \
             block `tmp`'s value. interference[2] = {:#x}",
            interference[2]
        );
        assert_ne!(
            interference[1] & (1 << 2),
            0,
            "interference must be symmetric"
        );

        // And the allocation built on that graph keeps them apart.
        let result = allocate_registers_with_handlers(
            &code,
            code.len(),
            4,
            0,
            &[],
            &[],
            &STATE_BEFORE_TRY_HANDLERS,
        );
        if let (Some(a), Some(b)) = (result.assignments[1], result.assignments[2]) {
            assert_ne!(a, b, "locals 1 and 2 share r{a}");
        }
    }
}

/// `assigned_locals_per_pc_all` (interpreter round i1 wave 19, lane L1).
#[cfg(test)]
mod assigned_locals_tests {
    use super::*;

    /// The row of `pc` in a one-word table.
    fn row(table: &(Vec<u64>, Vec<bool>, usize), pc: usize) -> u64 {
        assert_eq!(table.2, 1, "one word per pc");
        assert!(table.1[pc], "pc {pc} is covered");
        table.0[pc]
    }

    /// `int t = 42; int i = 0; while (i < 1000) i++; return i;` with `t`
    /// never read: dead everywhere, but assigned from pc 3 on, the loop
    /// header included, where liveness calls it dead.
    #[test]
    fn a_dead_local_stays_assigned_across_a_loop() {
        #[rustfmt::skip]
        let code: [u8; 22] = [
            0x10, 42,         // 0:  bipush 42
            0x3c,             // 2:  istore_1        t
            0x03,             // 3:  iconst_0
            0x3b,             // 4:  istore_0        i
            0x1a,             // 5:  iload_0         <- loop header
            0x11, 0x03, 0xe8, // 6:  sipush 1000
            0xa2, 0x00, 0x09, // 9:  if_icmpge +9 -> 18
            0x84, 0x00, 0x01, // 12: iinc 0, 1
            0xa7, 0xff, 0xf6, // 15: goto -10 -> 5
            0x1a,             // 18: iload_0
            0xac,             // 19: ireturn
            0, 0,
        ];
        let assigned = assigned_locals_per_pc_all(&code, 20, 0, &[], &[], 2);
        assert_eq!(row(&assigned, 0), 0, "nothing on entry");
        assert_eq!(row(&assigned, 3), 0b10, "t after its store");
        assert_eq!(row(&assigned, 5), 0b11, "both at the header");
        assert_eq!(row(&assigned, 18), 0b11);
        let (live, _, _) = live_locals_per_pc_all(&code, 20, 0, &[], &[], 2);
        assert_eq!(live[5] & 0b10, 0, "t is dead at the header");
    }

    /// A slot one arm of a diamond does not store is not assigned at the
    /// merge; a parameter is assigned everywhere.
    #[test]
    fn a_slot_one_path_skips_is_not_assigned_at_the_merge() {
        #[rustfmt::skip]
        let code: [u8; 11] = [
            0x1a,             // 0: iload_0          (parameter)
            0x99, 0x00, 0x06, // 1: ifeq +6 -> 7
            0x04,             // 4: iconst_1
            0x3c,             // 5: istore_1
            0x00,             // 6: nop
            0x1a,             // 7: iload_0          <- merge
            0xac,             // 8: ireturn
            0, 0,
        ];
        let assigned = assigned_locals_per_pc_all(&code, 9, 1, &[], &[], 2);
        assert_eq!(row(&assigned, 0), 0b01, "the parameter on entry");
        assert_eq!(row(&assigned, 6), 0b11, "the storing arm");
        assert_eq!(row(&assigned, 7), 0b01, "one path skips the store");
    }

    /// A handler is entered with its protected block's ENTRY state: the
    /// throw may come before the block's store.
    #[test]
    fn a_handler_sees_the_protected_blocks_entry_state() {
        #[rustfmt::skip]
        let code: [u8; 9] = [
            0x04, // 0: iconst_1          <- protected [0, 4)
            0x3c, // 1: istore_1
            0x03, // 2: iconst_0
            0xac, // 3: ireturn
            0x4d, // 4: astore_2          <- handler
            0x1a, // 5: iload_0
            0xac, // 6: ireturn
            0, 0,
        ];
        let assigned = assigned_locals_per_pc_all(&code, 7, 1, &[], &[(0, 4, 4)], 3);
        assert_eq!(row(&assigned, 2), 0b011);
        assert_eq!(
            row(&assigned, 4),
            0b001,
            "slot 1 may be unassigned at the throw"
        );
        assert_eq!(row(&assigned, 5), 0b101, "the caught exception");
    }

    /// `int t = 42; int i = 0; while (i < 1000) i++; return i;` — the loop
    /// of [`a_dead_local_stays_assigned_across_a_loop`], `t` never read.
    #[rustfmt::skip]
    const DEAD_T_LOOP: [u8; 22] = [
        0x10, 42,         // 0:  bipush 42
        0x3c,             // 2:  istore_1        t
        0x03,             // 3:  iconst_0
        0x3b,             // 4:  istore_0        i
        0x1a,             // 5:  iload_0         <- loop header
        0x11, 0x03, 0xe8, // 6:  sipush 1000
        0xa2, 0x00, 0x09, // 9:  if_icmpge +9 -> 18
        0x84, 0x00, 0x01, // 12: iinc 0, 1
        0xa7, 0xff, 0xf6, // 15: goto -10 -> 5
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];

    /// Interpreter round i1 wave 21, lane L3: a store to `u` while `t` holds
    /// a value it will never be asked for again still may not land in `t`'s
    /// home. Ordinary liveness gives the two no edge (`t` is dead before `u`
    /// is stored); the keep-alive table does, at `u`'s store and at every pc
    /// both are assigned.
    #[test]
    fn a_store_interferes_with_every_local_assigned_across_it() {
        #[rustfmt::skip]
        let code: [u8; 11] = [
            0x10, 42, // 0: bipush 42
            0x3b,     // 2: istore_0         t
            0x1a,     // 3: iload_0          t's last read
            0x57,     // 4: pop
            0x03,     // 5: iconst_0
            0x3c,     // 6: istore_1         u
            0x1b,     // 7: iload_1
            0xac,     // 8: ireturn
            0, 0,
        ];
        let (assigned, _, words) = assigned_locals_per_pc_all(&code, 9, 0, &[], &[], 2);
        let mut interference = vec![0u64; 2];
        keep_assigned_interference(&code, 9, 2, &assigned, words, &mut interference);
        assert_eq!(interference, vec![0b10, 0b01], "t and u interfere");
        // An empty table widens nothing.
        let mut untouched = vec![0u64; 2];
        keep_assigned_interference(&code, 9, 2, &[], 0, &mut untouched);
        assert_eq!(untouched, vec![0, 0]);
    }

    /// The allocation under the keep-alive: `t` and the loop counter never
    /// share a home, and the loop header's block live-in names `t`, so the
    /// OSR trampoline seeds it (and masks a dead local coalesced with it)
    /// instead of treating it as dead. Without the table the allocation is
    /// the ordinary one.
    #[test]
    fn the_keep_alive_allocation_keeps_a_dead_assigned_local_home() {
        let (assigned, _, words) = assigned_locals_per_pc_all(&DEAD_T_LOOP, 20, 0, &[], &[], 2);
        let kept = allocate_registers_keeping_assigned(
            &DEAD_T_LOOP,
            20,
            2,
            0,
            &[],
            &[],
            &[],
            &assigned,
            words,
        );
        if let (Some(i), Some(t)) = (kept.assignments[0], kept.assignments[1]) {
            assert_ne!(i, t, "i and t share r{i}");
        }
        let header_live_in = |result: &RegAllocResult| {
            result
                .block_live_in
                .iter()
                .find(|(pc, _)| *pc == 5)
                .map(|(_, live_in)| *live_in)
        };
        assert_eq!(
            header_live_in(&kept).map(|m| m & 0b10),
            Some(0b10),
            "t is seeded at the header"
        );
        let plain = allocate_registers_with_handlers(&DEAD_T_LOOP, 20, 2, 0, &[], &[], &[]);
        assert_eq!(
            header_live_in(&plain).map(|m| m & 0b10),
            Some(0),
            "without a debugger t is dead there"
        );
        // A table that keeps nothing allocates exactly as without one.
        let zeros = vec![0u64; assigned.len()];
        let none_kept = allocate_registers_keeping_assigned(
            &DEAD_T_LOOP,
            20,
            2,
            0,
            &[],
            &[],
            &[],
            &zeros,
            words,
        );
        assert_eq!(none_kept.assignments, plain.assignments);
        assert_eq!(none_kept.xmm_assignments, plain.xmm_assignments);
        assert_eq!(none_kept.block_live_in, plain.block_live_in);
    }
}
