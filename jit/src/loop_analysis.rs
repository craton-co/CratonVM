// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loop analysis scaffolding for LICM (Loop-Invariant Code Motion).
//!
//! Round-8 wave-3 HIGH fix (Fix 3): scaffold for generic LICM of
//! `getfield` / `getstatic` whose receiver is loop-invariant. This
//! module supplies the structural primitives (loop nest discovery and
//! a per-loop scan for invariant heap loads) so subsequent rounds can
//! wire hoisting into the x64 emitter without re-implementing the
//! analysis.
//!
//! The existing `LoopHoist` / `FpLoopHoist` records in `x64.rs` are
//! pattern-specific (aaload chains, scalar fp reloads). This module
//! is the generic foundation that future passes will share with
//! aarch64 and the IR optimizer.
//!
//! ## Status
//!
//! Analysis-only. Hoisting itself is intentionally deferred: real
//! hoisting requires (a) safepoint placement adjustment so the
//! hoisted load is observed at the loop-pre-header oop map and not
//! at every iteration, (b) a value-numbering pass to ensure the
//! receiver expression is itself invariant under aliasing
//! conservative assumptions, and (c) regalloc participation so the
//! hoisted value owns a frame slot for the loop's lifetime. Each of
//! those is its own subsystem.
//!
//! TODO(round-12+): wire hoisting consumer into `x64::compile_method`
//! and the IR optimizer.

/// A natural loop discovered by scanning backward branches.
///
/// The set of body blocks is the closure under predecessor traversal
/// from `back_edges` back to `header_pc`. For the scaffold we store
/// the bytecode PC range conservatively as a half-open interval; a
/// future pass should refine to the precise reachable set when the
/// loop body contains forward exits to non-immediate-postdominators.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoopInfo {
    /// Bytecode PC of the loop header — the target of every back edge.
    pub header_pc: usize,
    /// Bytecode PCs of back-edge instructions (`goto` / cond-branch
    /// whose target is `header_pc`). A reducible loop has at least
    /// one; an irreducible loop has multiple distinct headers and
    /// is currently *excluded* from detection (we treat irreducible
    /// CFGs as opaque and refuse to hoist).
    pub back_edges: Vec<usize>,
    /// Conservative body extent `[header_pc, body_end_pc)`. Includes
    /// the back-edge instruction. A future refinement may shrink
    /// this to exclude blocks that exit early (e.g. `return` from
    /// inside the loop).
    pub body_blocks: (usize, usize),
}

/// A getfield/getstatic candidate for hoisting out of a loop.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InvariantLoad {
    /// Bytecode PC of the loop header the load is invariant for.
    pub loop_header: usize,
    /// Bytecode PC of the `getfield` (0xb4) or `getstatic` (0xb2).
    pub load_pc: usize,
    /// Constant-pool index of the field reference (CONSTANT_Fieldref).
    pub cp_index: u16,
    /// For `getfield`: the local index whose value is the receiver
    /// (verified non-modified in the loop body). For `getstatic`:
    /// `None` — no receiver needed.
    pub receiver_local: Option<u16>,
}

/// Detect natural loops by scanning bytecode for backward branches.
///
/// Coalesces back edges that share a header into one `LoopInfo`.
/// Multiple back edges to the same header are tracked together (they
/// form a single loop in the standard SSA sense).
///
/// This is structurally similar to `x64::detect_loops` but groups by
/// header and records the body extent. The two implementations
/// should converge in a later round.
///
/// ANALYSIS-ONLY — NOT WIRED INTO CODEGEN. This pass is descriptive
/// scaffolding for the (deferred) generic LICM consumer; nothing in
/// the x64/aarch64 emitter currently calls it, and its output must
/// not be treated as a hoisting decision. See the module-level
/// "Status" doc for why hoisting is deferred. Do not assume the
/// returned `LoopInfo` body extent is exact: it is a conservative
/// half-open PC interval, not a precise reachable-block set.
pub fn detect_loops(code: &[u8], code_len: usize) -> Vec<LoopInfo> {
    use std::collections::BTreeMap;
    let mut by_header: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        let len = inst_len_at(code, pc);
        match op {
            // goto / conditional branches: signed i16 offset at pc+1..=pc+2.
            0xa7 | 0x99..=0xa6 | 0xc6 | 0xc7 => {
                if pc + 2 < code_len {
                    // JVM branch offsets are big-endian signed i16.
                    // Use `from_be_bytes` to avoid the debug overflow
                    // panic that `(byte_hi as i16) << 8` triggers when
                    // the high bit is set.
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_opt = pc.checked_add_signed(offset as isize);
                    if let Some(target) = target_opt {
                        if target < code_len && target <= pc {
                            by_header.entry(target).or_default().push(pc);
                        }
                    }
                }
            }
            _ => {}
        }
        pc += len;
    }
    by_header
        .into_iter()
        .map(|(header, edges)| {
            let body_end = edges.iter().copied().max().map(|e| e + 3).unwrap_or(header);
            LoopInfo {
                header_pc: header,
                back_edges: edges,
                body_blocks: (header, body_end),
            }
        })
        .collect()
}

/// Identify `getfield` / `getstatic` invocations whose receiver
/// expression is loop-invariant in the given loop.
///
/// SCAFFOLD: this implementation finds candidate sites and conservatively
/// reports them. Receiver-locality is approximated by requiring the
/// receiver to be a single `aload` of a local that is NOT in the
/// modified-locals set of the loop body. The modified-locals set is
/// computed cheaply via a single bytecode scan.
///
/// ANALYSIS-ONLY — NOT WIRED INTO CODEGEN. Consumers must NOT yet act
/// on the returned list: the hoisting itself requires safepoint and
/// oop-map adjustments (see module "Status") not implemented here, and
/// no emitter path currently calls this function. It exists so future
/// hoisting passes can be developed and tested against detection that
/// is already validated, without re-implementing it. Treating the
/// returned candidates as "safe to hoist" today would be incorrect.
pub fn find_invariant_loads(loop_info: &LoopInfo, code: &[u8]) -> Vec<InvariantLoad> {
    let (start, end) = loop_info.body_blocks;
    if end > code.len() || start >= end {
        return Vec::new();
    }
    let modified = modified_locals_in_range(code, start, end);

    let mut out = Vec::new();
    let mut pc = start;
    let mut prev_op: Option<u8> = None;
    let mut prev_local: Option<u16> = None;
    while pc < end {
        let op = code[pc];
        let len = inst_len_at(code, pc);
        match op {
            // aload_0..aload_3 → local index = op - 0x2a
            0x2a..=0x2d => {
                prev_op = Some(op);
                prev_local = Some((op - 0x2a) as u16);
            }
            // aload <wide_index>
            0x19 if pc + 1 < end => {
                prev_op = Some(op);
                prev_local = Some(code[pc + 1] as u16);
            }
            // getstatic — no receiver; always invariant if the field
            // resolution itself is invariant (true unless the loop
            // contains class loading that could re-trigger
            // resolution, which we conservatively assume it doesn't
            // for the scaffold).
            0xb2 if pc + 2 < end => {
                let cp_index = ((code[pc + 1] as u16) << 8) | code[pc + 2] as u16;
                out.push(InvariantLoad {
                    loop_header: loop_info.header_pc,
                    load_pc: pc,
                    cp_index,
                    receiver_local: None,
                });
                prev_op = None;
                prev_local = None;
            }
            // getfield — invariant iff the preceding push was an
            // aload of an unmodified local.
            0xb4 if pc + 2 < end => {
                if let (Some(load_op), Some(local)) = (prev_op, prev_local) {
                    let is_aload = matches!(load_op, 0x19 | 0x2a..=0x2d);
                    let local_is_invariant =
                        (local as usize) < 64 && (modified & (1u64 << local)) == 0;
                    if is_aload && local_is_invariant {
                        let cp_index = ((code[pc + 1] as u16) << 8) | code[pc + 2] as u16;
                        out.push(InvariantLoad {
                            loop_header: loop_info.header_pc,
                            load_pc: pc,
                            cp_index,
                            receiver_local: Some(local),
                        });
                    }
                }
                prev_op = None;
                prev_local = None;
            }
            _ => {
                prev_op = Some(op);
                prev_local = None;
            }
        }
        pc += len;
    }
    out
}

/// Compute the set of locals (0..=63) written within the byte range.
/// Locals above 63 are conservatively reported as modified (we treat
/// any reference to them as a hoisting blocker).
fn modified_locals_in_range(code: &[u8], start: usize, end: usize) -> u64 {
    let mut modified = 0u64;
    let mut pc = start;
    while pc < end {
        match code[pc] {
            // istore/lstore/fstore/dstore/astore (wide index — 1 byte index)
            0x36..=0x3a if pc + 1 < end => {
                let local = code[pc + 1] as usize;
                if local < 64 {
                    modified |= 1u64 << local;
                }
            }
            // istore_0..istore_3
            0x3b..=0x3e => {
                modified |= 1u64 << (code[pc] - 0x3b);
            }
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                modified |= 1u64 << (code[pc] - 0x3f);
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                modified |= 1u64 << (code[pc] - 0x43);
            }
            // dstore_0..dstore_3
            0x47..=0x4a => {
                modified |= 1u64 << (code[pc] - 0x47);
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                modified |= 1u64 << (code[pc] - 0x4b);
            }
            // iinc <index> <const>
            0x84 if pc + 2 < end => {
                let local = code[pc + 1] as usize;
                if local < 64 {
                    modified |= 1u64 << local;
                }
            }
            _ => {}
        }
        pc += inst_len_at(code, pc);
    }
    modified
}

/// Bytecode instruction length in bytes for the opcode at `pc`.
///
/// Returns 1 for genuinely-unknown opcodes so the linear scanner
/// cannot underflow; this may slightly overcount loop bodies for
/// esoteric ops, which only loses hoisting opportunities (never
/// produces incorrect ones).
///
/// The variable-length `tableswitch`/`lookupswitch` instructions are
/// decoded precisely (padding + payload). Returning the wrong length
/// for them is a genuine correctness bug: any caller that advances
/// `pc += inst_len_at(..)` would land in the middle of the switch
/// operand bytes and decode padding / jump offsets as opcodes,
/// desyncing the whole scan for the remainder of the method.
fn inst_len_at(code: &[u8], pc: usize) -> usize {
    if pc >= code.len() {
        return 1;
    }
    match code[pc] {
        // 2-byte: bipush, ldc, [ifsda]load, [ifsda]store, newarray
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xbc => 2,
        // 3-byte: sipush, ldc_w/ldc2_w, branches, getfield/static,
        // putfield/static, invokestatic/special/virtual, new,
        // anewarray, checkcast, instanceof, iinc.
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xa6
        | 0xa7
        | 0xb2..=0xb8
        | 0xbb
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        // 5-byte: invokedynamic / invokeinterface / multianewarray
        0xb9 | 0xba => 5,
        0xc5 => 4,
        // goto_w / jsr_w: 1 opcode + 4-byte offset.
        0xc8 | 0xc9 => 5,
        // tableswitch (0xaa): 1 opcode byte, then 0..=3 padding bytes
        // aligning the next byte to a 4-byte boundary *relative to the
        // start of the code array* (the method's first bytecode is
        // offset 0), then defaultbyte (4) + low (4) + high (4), then
        // (high - low + 1) jump offsets of 4 bytes each.
        0xaa => {
            // First operand byte sits at the next 4-aligned offset.
            let base = (pc + 1 + 3) & !3;
            // Need low at base+4..base+8 and high at base+8..base+12.
            let high_end = base + 12;
            if high_end > code.len() {
                // Truncated/garbage — fall back to a 1-byte step so
                // the scan terminates safely rather than reading OOB.
                return 1;
            }
            let low = i32::from_be_bytes([
                code[base + 4],
                code[base + 5],
                code[base + 6],
                code[base + 7],
            ]);
            let high = i32::from_be_bytes([
                code[base + 8],
                code[base + 9],
                code[base + 10],
                code[base + 11],
            ]);
            // n = high - low + 1 entries. Guard against malformed
            // (high < low) tables that would make n negative.
            let n = (high as i64) - (low as i64) + 1;
            if n < 0 {
                return 1;
            }
            // total = (base - pc) header skip + 12 (default/low/high)
            //         + n * 4 jump offsets.
            (base - pc) + 12 + (n as usize) * 4
        }
        // lookupswitch (0xab): 1 opcode byte, then 0..=3 padding bytes
        // aligning to a 4-byte boundary (same rule as tableswitch),
        // then defaultbyte (4) + npairs (4), then npairs match/offset
        // pairs of 8 bytes each.
        0xab => {
            let base = (pc + 1 + 3) & !3;
            // Need npairs at base+4..base+8.
            let npairs_end = base + 8;
            if npairs_end > code.len() {
                return 1;
            }
            let npairs = i32::from_be_bytes([
                code[base + 4],
                code[base + 5],
                code[base + 6],
                code[base + 7],
            ]);
            if npairs < 0 {
                return 1;
            }
            // total = (base - pc) header skip + 8 (default/npairs)
            //         + npairs * 8 (match/offset pairs).
            (base - pc) + 8 + (npairs as usize) * 8
        }
        // wide-prefixed instruction; nominal 4 bytes for most
        // (3-byte payload follows), 6 for iinc.
        0xc4 => {
            if pc + 1 < code.len() && code[pc + 1] == 0x84 {
                6
            } else {
                4
            }
        }
        // All other opcodes are 1 byte (arithmetic, stack manip,
        // returns, monitor, etc.).
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Tests — verify the analysis identifies loops and invariants the
// expected way. Hoisting consumer tests live with the (future) emitter.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_simple_backward_goto_as_loop() {
        // PC 0: iconst_0
        // PC 1: istore_1
        // PC 2: iload_1   ; (loop header)
        // PC 3: iconst_5
        // PC 4: if_icmpge +6  → exits at PC 10
        // PC 7: iinc 1, 1
        // PC 10: ireturn  (note: the if jumps here, the body ends sooner)
        // -- but we need a back edge; reshape:
        //
        // PC 0:  iconst_0   ; (no-op header context)
        // PC 1:  istore_1
        // PC 2:  iload_1                ; HEADER
        // PC 3:  bipush 10              ; 2 bytes
        // PC 5:  if_icmpge +8           ; 3 bytes (exit, forward)
        // PC 8:  iinc 1, 1              ; 3 bytes
        // PC 11: goto -9                ; 3 bytes → target PC 2
        // PC 14: return
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x08, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
            0xb1,
        ];
        let loops = detect_loops(&code, code.len());
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].header_pc, 2);
        assert_eq!(loops[0].back_edges, vec![11]);
    }

    #[test]
    fn modified_locals_finds_iinc_and_stores() {
        // istore_1 ; iinc 2, 1 ; istore_3 ; nop
        let code: Vec<u8> = vec![0x3c, 0x84, 0x02, 0x01, 0x3e, 0x00];
        let m = modified_locals_in_range(&code, 0, code.len());
        // bits 1, 2, 3 set
        assert_eq!(m, 0b1110);
    }

    #[test]
    fn invariant_load_finds_aload_then_getstatic_or_getfield() {
        // PC 0: aload_0
        // PC 1: getfield #0x0001
        // PC 4: pop
        // PC 5: getstatic #0x0002
        // PC 8: pop
        let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, 0x57, 0xb2, 0x00, 0x02, 0x57];
        let li = LoopInfo {
            header_pc: 0,
            back_edges: vec![],
            body_blocks: (0, code.len()),
        };
        let v = find_invariant_loads(&li, &code);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].load_pc, 1);
        assert_eq!(v[0].cp_index, 1);
        assert_eq!(v[0].receiver_local, Some(0));
        assert_eq!(v[1].load_pc, 5);
        assert_eq!(v[1].cp_index, 2);
        assert_eq!(v[1].receiver_local, None);
    }

    #[test]
    fn inst_len_tableswitch_padded_and_jumptable() {
        // Layout (PC shown). tableswitch must align its first operand
        // byte to a 4-byte boundary relative to code start.
        //
        // PC 0: nop                     ; force pc=1 for the switch so
        //                               ; padding is non-trivial.
        // PC 1: tableswitch (0xaa)
        //   pad PC 2,3 (operand byte aligns to PC 4)
        //   PC 4..8:   default = 0
        //   PC 8..12:  low     = 0
        //   PC 12..16: high    = 2   → n = high-low+1 = 3 jump offsets
        //   PC 16..20: jump[0]
        //   PC 20..24: jump[1]
        //   PC 24..28: jump[2]
        // operand begins at (1+1+3)&!3 = 4, so padding = PC 2,3 (2 bytes).
        // → instruction spans PC 1..28, i.e. length 27.
        let mut code: Vec<u8> = Vec::new();
        code.push(0x00); // PC 0: nop
        code.push(0xaa); // PC 1: tableswitch opcode
                         // operand begins at (1+1+3)&!3 = 4, so padding = PC 2,3 (2 bytes)
        code.push(0x00); // PC 2 padding
        code.push(0x00); // PC 3 padding
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 4..8 default
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 8..12 low
        code.extend_from_slice(&2i32.to_be_bytes()); // PC 12..16 high
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 16..20 jump[0]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 20..24 jump[1]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 24..28 jump[2]
        assert_eq!(code.len(), 28);
        // Length of the tableswitch at PC 1 must carry the scanner to
        // PC 28 (end of code): 28 - 1 = 27.
        assert_eq!(inst_len_at(&code, 1), 27);
        // And a linear scan from PC 0 lands exactly on the end, never
        // mis-decoding a padding/offset byte as an opcode.
        let mut pc = 0;
        let mut steps = 0;
        while pc < code.len() {
            pc += inst_len_at(&code, pc);
            steps += 1;
            assert!(steps < 100, "scan failed to terminate (desync)");
        }
        assert_eq!(pc, code.len());
        assert_eq!(steps, 2); // nop, then tableswitch
    }

    #[test]
    fn inst_len_lookupswitch_padded_and_pairs() {
        // PC 0: lookupswitch (0xab)
        //   operand at (0+1+3)&!3 = 4, so 3 padding bytes at PC 1,2,3
        //   PC 4..8:   default = 0
        //   PC 8..12:  npairs  = 2
        //   PC 12..20: pair[0] (match,offset)
        //   PC 20..28: pair[1] (match,offset)
        // → spans PC 0..28, length 28.
        let mut code: Vec<u8> = Vec::new();
        code.push(0xab); // PC 0: lookupswitch opcode
        code.push(0x00); // PC 1 padding
        code.push(0x00); // PC 2 padding
        code.push(0x00); // PC 3 padding
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 4..8 default
        code.extend_from_slice(&2i32.to_be_bytes()); // PC 8..12 npairs
        code.extend_from_slice(&1i32.to_be_bytes()); // PC 12..16 match[0]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 16..20 offset[0]
        code.extend_from_slice(&2i32.to_be_bytes()); // PC 20..24 match[1]
        code.extend_from_slice(&0i32.to_be_bytes()); // PC 24..28 offset[1]
        assert_eq!(code.len(), 28);
        assert_eq!(inst_len_at(&code, 0), 28);
    }

    #[test]
    fn inst_len_truncated_switch_is_safe() {
        // A bare tableswitch opcode with no room for the header must
        // not read out of bounds; it falls back to a 1-byte step.
        let code: Vec<u8> = vec![0xaa, 0x00, 0x00];
        assert_eq!(inst_len_at(&code, 0), 1);
        // Likewise a malformed lookupswitch with negative npairs.
        let mut bad: Vec<u8> = vec![0xab, 0x00, 0x00, 0x00];
        bad.extend_from_slice(&0i32.to_be_bytes()); // default
        bad.extend_from_slice(&(-5i32).to_be_bytes()); // npairs < 0
        assert_eq!(inst_len_at(&bad, 0), 1);
    }

    #[test]
    fn invariant_load_skips_when_receiver_local_modified() {
        // PC 0: astore_0  ; (modifies local 0)
        // PC 1: aload_0
        // PC 2: getfield #0x0001
        // PC 5: pop
        let code: Vec<u8> = vec![0x4b, 0x2a, 0xb4, 0x00, 0x01, 0x57];
        let li = LoopInfo {
            header_pc: 0,
            back_edges: vec![],
            body_blocks: (0, code.len()),
        };
        let v = find_invariant_loads(&li, &code);
        // getfield is filtered out because local 0 is stored within
        // the body extent (modified). getstatic would still pass —
        // none here.
        assert!(v.is_empty());
    }
}
