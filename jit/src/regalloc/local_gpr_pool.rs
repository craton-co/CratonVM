#[allow(unused_imports)]
use super::*;


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
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map(|n| n.min(LOCAL_REGS.len()))
            .unwrap_or(LOCAL_REGS.len())
    });
    &LOCAL_REGS[..n]
}


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
pub(in crate::regalloc) const MAX_TABLESWITCH_ENTRIES: usize = 1 << 24;

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
pub(in crate::regalloc) struct BasicBlock {
    pub(in crate::regalloc) start_pc: usize,
    pub(in crate::regalloc) end_pc: usize,          // exclusive
    pub(in crate::regalloc) successors: Vec<usize>, // indices into blocks vec
    /// Exception-handler blocks an instruction of this block can throw to — a
    /// subset of `successors`, filled only by [`build_cfg_with_handlers`].
    ///
    /// Kept apart because an exception edge is not an ordinary edge for
    /// liveness: it can be taken from ANY instruction in the block, including
    /// one that precedes a def of a local the handler reads. So the handler's
    /// live-in is live-IN here, not merely live-out — see [`solve_liveness`].
    pub(in crate::regalloc) handler_successors: Vec<usize>,
    pub(in crate::regalloc) gen: u64,  // locals used before defined in this block
    pub(in crate::regalloc) kill: u64, // locals defined in this block
    pub(in crate::regalloc) live_in: u64,
    pub(in crate::regalloc) live_out: u64,
}

/// Does `op` end a block with no fall-through edge, as liveness models it?
///
/// [`bytecode_analysis::falls_through`], except that `ret` keeps its
/// fall-through edge here. Liveness wants a superset of the real
/// successors, and `ret`'s real ones (the instructions after its `jsr`s)
/// are not modelled.
pub(in crate::regalloc) fn is_unconditional(op: u8) -> bool {
    !bytecode_analysis::falls_through(op) && op != 0xa9
}

/// Every local slot below 64 that some instruction in `code[..code_len]`
/// stores to (`[ilfda]store*`, `iinc`, and their `wide` forms), as a bitmask.
/// A linear walk: whether the store is reachable does not matter to the one
/// caller, which only needs "might this slot differ from its incoming value".
pub(in crate::regalloc) fn locals_written_anywhere(code: &[u8], code_len: usize) -> u64 {
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
pub(in crate::regalloc) fn build_cfg(code: &[u8], code_len: usize) -> Vec<BasicBlock> {
    build_cfg_with_leaders(code, code_len, &[])
}

/// [`build_cfg`] plus caller-supplied extra leaders. Exception handler entry
/// pcs are leaders even though no branch targets them — the runtime's
/// exception router enters them directly — and a handler that is not a block
/// start has no `live_in` for `live_locals_per_pc_inner` to read.
pub(in crate::regalloc) fn build_cfg_with_leaders(
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
pub(in crate::regalloc) fn local_access(code: &[u8], pc: usize) -> Option<(usize, bool, bool)> {
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
pub(in crate::regalloc) fn compute_gen_kill(code: &[u8], block: &mut BasicBlock) {
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
pub(in crate::regalloc) const MAX_LIVENESS_ITERATIONS: usize = 1000;

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
pub(in crate::regalloc) fn solve_liveness(blocks: &mut [BasicBlock], param_live_mask: u64) -> bool {
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
pub(in crate::regalloc) fn widen_liveness_to_all_live(blocks: &mut [BasicBlock]) {
    for block in blocks.iter_mut() {
        block.live_in = u64::MAX;
        block.live_out = u64::MAX;
    }
}

/// Per-protected-range handler liveness: `(start_pc, end_pc, handler_live_in)`.
///
/// Built after [`solve_liveness`] so each entry carries the *live-in* set of its
/// handler's block — the locals the handler reads before defining them.
pub(in crate::regalloc) fn handler_live_in_ranges(
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
pub(in crate::regalloc) fn handler_live_mask(pc: usize, handler_ranges: &[(usize, usize, u64)]) -> u64 {
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
pub(in crate::regalloc) fn build_interference(
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
pub(in crate::regalloc) fn count_uses(
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
pub(in crate::regalloc) fn color_graph(
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
pub(in crate::regalloc) fn find_float_locals(code: &[u8], code_len: usize, num_locals: usize) -> u64 {
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
pub(in crate::regalloc) fn find_non_float_locals(code: &[u8], code_len: usize) -> u64 {
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
pub(in crate::regalloc) fn build_cfg_with_handlers(
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
pub(in crate::regalloc) fn compute_gen_kill_windowed(code: &[u8], block: &mut BasicBlock, base: usize) {
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
pub(in crate::regalloc) fn param_live_in_mask_windowed(num_params: usize, param_slots: &[usize], base: usize) -> u64 {
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

pub(in crate::regalloc) fn live_locals_per_pc_inner(
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
pub(in crate::regalloc) fn assigned_word_at(assigned: &[u64], words: usize, pc: usize) -> u64 {
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
pub(in crate::regalloc) fn keep_assigned_interference(
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
pub(in crate::regalloc) fn allocate_registers_with(
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
pub(in crate::regalloc) fn allocate_registers_inner(
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
    // misassignment" (see NEW-1.3/1.4 in docs/feature-designs/jit-machine-level-and-instruction-selection.md):
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
    pub(in crate::regalloc) regs: Vec<RegSpec>,
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
pub(in crate::regalloc) const FIXED_OPERAND_GPRS: [u8; 5] = [10, 11, 0, 1, 2];

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
/// value defined at `p` overlap. A consumer may separately declare an audited
/// [`ReadBeforeWrite`] register handoff; frame-home colouring stays inclusive.
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

/// A result written only after these inputs have been read. Input order is
/// the emitter's register-reuse preference (the tied left operand first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadBeforeWrite {
    pub result: NodeId,
    pub inputs: Vec<NodeId>,
    pub pos: usize,
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
    /// Audited instruction boundaries where a dying input register can become
    /// the result register. Sorted by position, one result at each position.
    /// This changes register overlap only; frame words remain endpoint-inclusive.
    pub read_before_write: Vec<ReadBeforeWrite>,
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
pub(in crate::regalloc) fn ir_op_defines_value(op: &Op) -> bool {
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
            | Op::VthreadBoundaryTest { .. }
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
            | Op::ByteArrayCopy
    )
}

/// Does this op reach a safepoint at which the collector may read the oop map?
///
/// Conservative superset of what `ir_lower` emits (`emit_safepoint_map` at
/// `Op::Call`, `emit_safepoint_poll` at back edges): allocation and the lambda
/// adapter can also transfer to the runtime, and `Op::Guard` transfers to the
/// deopt trampoline. Over-approximating costs promotions, never correctness.
pub(in crate::regalloc) fn ir_op_is_safepoint(op: &Op) -> bool {
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
pub(in crate::regalloc) fn ir_op_is_call(op: &Op) -> bool {
    // zt t19/zb2: under an armed ZGC load barrier `ir_lower` lowers a reference
    // `ArrayLoad` to `MOV RAX, jit_aaload ; CALL RAX`, returning into the body
    // (`lower_ref_array_load_through_barrier_helper`). Same process flag the
    // lowering reads; production sets it once, before the first compile.
    // zt t21/zb4: and the same predicate -- a colored-store arming decodes the
    // element inline (no call), and both sides ask
    // `zgc_ref_element_load_calls_helper`.
    if matches!(op, Op::ScalarIntrinsic(crate::ir::ScalarOp::MathCall(_))) {
        return true;
    }
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
pub(in crate::regalloc) fn is_inline_integer_rem(node: &crate::ir::Node) -> bool {
    matches!(node.op, Op::Rem) && matches!(node.ty, IrType::Int | IrType::Long)
}

/// Set bit `i`; out-of-range indices are ignored rather than panicking (the
/// model runs on a graph nothing has verified yet).
#[inline]
pub(in crate::regalloc) fn ls_bit_set(bits: &mut [u64], i: usize) {
    if let Some(word) = bits.get_mut(i / 64) {
        *word |= 1u64 << (i % 64);
    }
}

/// Is bit `i` set?
#[inline]
pub(in crate::regalloc) fn ls_bit_get(bits: &[u64], i: usize) -> bool {
    bits.get(i / 64)
        .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
}

/// Visit every set bit, in increasing order.
#[inline]
pub(in crate::regalloc) fn ls_bits_for_each(bits: &[u64], mut f: impl FnMut(usize)) {
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
pub(in crate::regalloc) fn ls_ctrl_block_of(graph: &Graph, schedule: &Schedule, mut ctrl: NodeId) -> Option<usize> {
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
    true
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
    false
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
    true
}

/// `CRATONVM_JIT_IR_ONE_LIVENESS_PASS` (default on; interpreter round i2 wave
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
    true
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
    false
}

/// `CRATONVM_JIT_IR_LIVENESS_BUDGET_BY_VALUES=1` (opt-in; interpreter round i2
/// wave 67, lane L2): [`liveness_core`]'s work budget counts `values × blocks`
/// (the wanted values) instead of `nodes × blocks`. A **behaviour change**: a
/// method whose arena is over budget today but whose value set is not starts
/// being analysed (ranges instead of the whole-method fallback). Implies
/// [`liveness_dense_values_enabled`], because only the dense sets are sized by
/// the quantity the budget then counts.
pub fn liveness_budget_by_values_enabled() -> bool {
    false
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
    false
}

/// `CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST` (default on; interpreter round i2
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
    true
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
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
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
    false
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
    false
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
    false
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
    false
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
    pub(in crate::regalloc) key: Option<(usize, usize, usize)>,
    pub(in crate::regalloc) positions: Option<LsPositionModel>,
    pub(in crate::regalloc) phi_edges: Option<LsPhiEdges>,
    pub(in crate::regalloc) pool: Vec<Vec<u64>>,
    pub(crate) stats: LivenessScratchStats,
}

impl LivenessScratch {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(in crate::regalloc) fn rekey(&mut self, graph: &Graph, schedule: &Schedule) {
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

/// `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` (default ON since 2026-11-02, `=0`
/// kills; JIT round 17 wave 3, lane irlower; OD16-1 = OI-5, the token-edge half
/// the proposal names as its first step): [`build_live_model`] stops counting a
/// memory-TOKEN edge as a read of the value that produced it. IRLOWER17-3's
/// flip rule held on 2026-11-02: 30 probe arms equal HotSpot, the token-read
/// guard refused no compile of the Spring smoke in either mode, and `peak_live`
/// summed over its 905 compiles fell 11398 -> 10535.
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
pub(in crate::regalloc) fn ls_token_edges_not_uses_enabled() -> bool {
    // WARNING j27-user-flip-20261102 (default ON; =0 restores the old default)
    true
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
    false
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
pub(in crate::regalloc) fn ls_loop_depths(schedule: &Schedule) -> Vec<u32> {
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
pub(in crate::regalloc) fn live_model_pins(graph: &Graph, wants_loc: &[bool]) -> Vec<bool> {
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
pub(in crate::regalloc) fn build_live_model_legacy(graph: &Graph, schedule: &Schedule, rules: LivenessRules) -> LiveModel {
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
pub(in crate::regalloc) fn live_model_positions_legacy(
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

