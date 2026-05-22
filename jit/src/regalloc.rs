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
}

/// A basic block in the bytecode CFG.
struct BasicBlock {
    start_pc: usize,
    end_pc: usize, // exclusive
    successors: Vec<usize>, // indices into blocks vec
    gen: u64,      // locals used before defined in this block
    kill: u64,     // locals defined in this block
    live_in: u64,
    live_out: u64,
}

/// Compute the length of a bytecode instruction at `pc`.
pub(crate) fn bc_len(code: &[u8], pc: usize) -> usize {
    match code[pc] {
        0x10 | 0x15..=0x19 | 0x36..=0x3a | 0xbc => 2,
        0x11 | 0x84 | 0x99..=0xa6 | 0xa7 | 0xb2 | 0xb3 | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb8
        | 0xbd | 0xc0 | 0xc1 | 0xc6 | 0xc7 => 3,
        0xbb => 3,
        0xc5 => 4,
        0xb9 => 5,
        // tableswitch — variable length
        0xaa => {
            let mut p = pc + 1;
            while p % 4 != 0 { p += 1; }
            if p + 12 > code.len() { return 1; }
            let low = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            let high = i32::from_be_bytes([code[p + 8], code[p + 9], code[p + 10], code[p + 11]]);
            let count = (high - low + 1).max(0) as usize;
            (p + 12 + count * 4) - pc
        }
        // lookupswitch — variable length
        0xab => {
            let mut p = pc + 1;
            while p % 4 != 0 { p += 1; }
            if p + 8 > code.len() { return 1; }
            let npairs = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]) as usize;
            (p + 8 + npairs * 8) - pc
        }
        _ => 1,
    }
}

/// Compute the branch target PC for a branch instruction at `pc`, if any.
/// Returns `None` if the instruction is not a branch or the target is out of bounds.
fn branch_target(code: &[u8], pc: usize) -> Option<usize> {
    match code[pc] {
        0x99..=0xa6 | 0xa7 | 0xc6 | 0xc7 => {
            if pc + 2 >= code.len() {
                return None;
            }
            // JVM branch offsets are big-endian signed i16.
            let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
            let target = pc as i32 + offset;
            // Validate the target is non-negative (valid bytecode offset)
            if target < 0 {
                return None;
            }
            Some(target as usize)
        }
        _ => None,
    }
}

/// Returns true if the opcode is an unconditional control transfer (goto, return, athrow, switch).
fn is_unconditional(op: u8) -> bool {
    matches!(
        op,
        0xa7    // goto
        | 0xaa  // tableswitch
        | 0xab  // lookupswitch
        | 0xac  // ireturn
        | 0xad  // lreturn
        | 0xae  // freturn
        | 0xaf  // dreturn
        | 0xb0  // areturn
        | 0xb1  // return (void)
        | 0xbf  // athrow
    )
}

/// Collect all branch targets from a switch instruction at `pc`.
fn switch_targets(code: &[u8], pc: usize, code_len: usize) -> Vec<usize> {
    let mut targets = Vec::new();
    let op = code[pc];
    let base_pc = pc;
    let mut p = pc + 1;
    while p % 4 != 0 { p += 1; }

    match op {
        0xaa => {
            // tableswitch
            if p + 12 > code_len { return targets; }
            let default_off = i32::from_be_bytes([code[p], code[p+1], code[p+2], code[p+3]]);
            // Use checked arithmetic: a malformed/negative offset must not
            // wrap to a huge usize. Skip targets that fall outside the code.
            if let Some(t) = base_pc.checked_add_signed(default_off as isize) {
                if t < code_len { targets.push(t); }
            }
            let low = i32::from_be_bytes([code[p+4], code[p+5], code[p+6], code[p+7]]);
            let high = i32::from_be_bytes([code[p+8], code[p+9], code[p+10], code[p+11]]);
            let count = (high - low + 1).max(0) as usize;
            p += 12;
            for _ in 0..count {
                if p + 4 > code_len { break; }
                let off = i32::from_be_bytes([code[p], code[p+1], code[p+2], code[p+3]]);
                if let Some(t) = base_pc.checked_add_signed(off as isize) {
                    if t < code_len { targets.push(t); }
                }
                p += 4;
            }
        }
        0xab => {
            // lookupswitch
            if p + 8 > code_len { return targets; }
            let default_off = i32::from_be_bytes([code[p], code[p+1], code[p+2], code[p+3]]);
            if let Some(t) = base_pc.checked_add_signed(default_off as isize) {
                if t < code_len { targets.push(t); }
            }
            let npairs = i32::from_be_bytes([code[p+4], code[p+5], code[p+6], code[p+7]]) as usize;
            p += 8;
            for _ in 0..npairs {
                if p + 8 > code_len { break; }
                let off = i32::from_be_bytes([code[p+4], code[p+5], code[p+6], code[p+7]]);
                if let Some(t) = base_pc.checked_add_signed(off as isize) {
                    if t < code_len { targets.push(t); }
                }
                p += 8;
            }
        }
        _ => {}
    }
    targets
}

/// Build the control flow graph from bytecode.
fn build_cfg(code: &[u8], code_len: usize) -> Vec<BasicBlock> {
    // First pass: find all block-starting PCs
    let mut block_starts = vec![false; code_len + 1];
    block_starts[0] = true;

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
        // iload, lload, fload, dload, aload (wide index)
        0x15..=0x19 => Some((code[pc + 1] as usize, true, false)),

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
        // istore, lstore, fstore, dstore, astore (wide index)
        0x36..=0x3a => Some((code[pc + 1] as usize, false, true)),

        // iinc: both reads and writes the local
        0x84 => Some((code[pc + 1] as usize, true, true)),

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

/// Solve liveness to fixpoint using worklist iteration.
fn solve_liveness(blocks: &mut [BasicBlock], num_params: usize) {
    // Seed: parameters are live-in at entry block
    if !blocks.is_empty() {
        let param_mask: u64 = if num_params >= 64 {
            u64::MAX
        } else {
            (1u64 << num_params) - 1
        };
        blocks[0].gen |= param_mask & !blocks[0].kill;
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

/// Build interference graph from liveness data.
/// Returns `interference[i]` = bitmask of locals that interfere with local `i`.
///
/// Walks backward through each block at instruction granularity to capture
/// intra-block interference (e.g., a local defined and used within the same
/// block that overlaps with another local's live range).
fn build_interference(code: &[u8], blocks: &[BasicBlock], num_locals: usize) -> Vec<u64> {
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
            if let Some((idx, is_use, is_def)) = local_access(code, pc) {
                if idx < n {
                    let bit = 1u64 << idx;
                    if is_def {
                        // At a def point, the defined local interferes with everything
                        // currently live (excluding itself).
                        let others = live & !(bit);
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
const LOOP_WEIGHT_PER_DEPTH: u32 = 10;
/// Cap on the loop-depth weight contributed by a single use.
///
/// Corresponds to depth = 6 with `LOOP_WEIGHT_PER_DEPTH = 10` (=> 10^6).
/// At this cap a deeply-nested use still dominates a non-loop use by 1e6×,
/// which is more than enough for the graph-coloring spill heuristic.
const MAX_LOOP_DEPTH_WEIGHT: u32 = 1_000_000;

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
                if idx < 64 { float_mask |= 1u64 << idx; }
            }
            // dload_0..dload_3
            0x26..=0x29 => {
                let idx = (code[pc] - 0x26) as usize;
                if idx < 64 { float_mask |= 1u64 << idx; }
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                let idx = (code[pc] - 0x43) as usize;
                if idx < 64 { float_mask |= 1u64 << idx; }
            }
            // dstore_0..dstore_3
            0x47..=0x4a => {
                let idx = (code[pc] - 0x47) as usize;
                if idx < 64 { float_mask |= 1u64 << idx; }
            }
            // fload, dload (wide)
            0x17 | 0x18 => {
                let idx = code[pc + 1] as usize;
                if idx < 64 { float_mask |= 1u64 << idx; }
            }
            // fstore, dstore (wide)
            0x38 | 0x39 => {
                let idx = code[pc + 1] as usize;
                if idx < 64 { float_mask |= 1u64 << idx; }
            }
            _ => {}
        }
        pc += bc_len(code, pc);
    }
    let _ = num_locals; // used for documentation
    float_mask
}

/// Run register allocation for a method (x86-64).
pub fn allocate_registers(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_with(code, code_len, num_locals, num_params, loops, &LOCAL_REGS, &LOCAL_XMMS)
}

/// Run register allocation for a method (ARM64).
pub fn allocate_registers_arm64(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_with(code, code_len, num_locals, num_params, loops, &ARM64_LOCAL_GPRS, &ARM64_LOCAL_FPS)
}

/// Platform-generic register allocation entry point.
fn allocate_registers_with(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
    gpr_regs: &[u8],
    fp_regs: &[u8],
) -> RegAllocResult {
    let available = gpr_regs;

    // Bail out for trivial cases: use fixed mapping
    if num_locals == 0 {
        return RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
        };
    }

    // Detect float/double locals — these use XMM registers, not GPRs
    let float_mask = find_float_locals(code, code_len, num_locals);

    // Build CFG and solve liveness
    let mut blocks = build_cfg(code, code_len);
    for block in &mut blocks {
        compute_gen_kill(code, block);
    }
    solve_liveness(&mut blocks, num_params);

    // Build full interference graph
    let interference = build_interference(code, &blocks, num_locals);

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
    // Only propagate XMM assignments for float/double locals
    let mut xmm_assignments = vec![None; num_locals];
    for i in 0..n {
        if float_mask & (1u64 << i) != 0 {
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
        };
    }

    RegAllocResult {
        assignments,
        xmm_assignments,
        used_callee_saved,
        used_xmm_regs,
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
        assert!(regalloc_invariants_hold(&gpr, &xmm, &interference, float_mask, 3));
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
        assert!(!regalloc_invariants_hold(&gpr, &xmm, &interference, float_mask, 1));
    }

    #[test]
    fn invariants_reject_int_in_xmm() {
        let gpr = vec![None];
        let xmm = vec![Some(8)];
        let interference = vec![0u64];
        let float_mask = 0; // local 0 is int but assigned an XMM
        assert!(!regalloc_invariants_hold(&gpr, &xmm, &interference, float_mask, 1));
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
        assert!(!regalloc_invariants_hold(&gpr, &xmm, &interference, float_mask, 2));
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
        assert!(result.assignments[0].is_some(), "param 0 should get a register");
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
            result.assignments[0],
            result.assignments[1],
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
        assert!(counts[2] >= 20, "loop uses should be weighted higher: got {}", counts[2]);
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
        assert!(blocks.len() >= 3, "should have at least 3 blocks, got {}", blocks.len());
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
        assert!(result.assignments[0].is_some(), "param 0 should get ARM64 register");
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
            assert!((19..=28).contains(r), "register X{r} not in callee-saved range");
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
        assert!(assigned_count <= 10, "at most 10 ARM64 callee-saved GPRs, got {assigned_count}");
        let spill_count = result.assignments.iter().filter(|a| a.is_none()).count();
        assert!(spill_count >= 2, "should have at least 2 spills, got {spill_count}");
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
        assert!(result.xmm_assignments[0].is_some(), "float local 0 should get FP register");
        assert!(result.xmm_assignments[1].is_some(), "float local 1 should get FP register");
        // FP regs should be in 8..=15 (D8-D15)
        let fp0 = result.xmm_assignments[0].unwrap();
        let fp1 = result.xmm_assignments[1].unwrap();
        assert!((8..=15).contains(&fp0), "FP reg {fp0} not in D8-D15 range");
        assert!((8..=15).contains(&fp1), "FP reg {fp1} not in D8-D15 range");
        assert_ne!(fp0, fp1, "interfering float locals must get different FP regs");
        // GPR assignments should be None for float locals
        assert!(result.assignments[0].is_none(), "float local 0 should NOT get GPR");
        assert!(result.assignments[1].is_none(), "float local 1 should NOT get GPR");
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
        assert!(result.assignments[0].is_some(), "int local 0 should get GPR");
        assert!((19..=28).contains(&result.assignments[0].unwrap()));
        // local 1 = float → FP reg
        assert!(result.xmm_assignments[1].is_some(), "float local 1 should get FP register");
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
            result.used_callee_saved.len(), 3,
            "should use exactly 3 callee-saved GPRs"
        );
        for &r in &result.used_callee_saved {
            assert!((19..=28).contains(&r), "callee-saved reg X{r} not in range");
        }
    }
}
