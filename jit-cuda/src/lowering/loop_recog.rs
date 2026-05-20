//! Recognize the canonical element-wise loop pattern.
//!
//! The shape we're looking for is what `javac` produces for
//! `for (int i = 0; i < bound; i++) { body; }`:
//!
//! ```text
//!   <prelude — possibly computes the bound and stores it in a local>
//!   iconst_0 / iconst_*
//!   istore   iv_slot
//! header: iload iv_slot       (loop header — the back branch target)
//!   <load bound>
//!   if_icmp{ge,gt,le,lt}  exit
//!   <body — any of the analyzer-accepted opcodes>
//!   iinc iv_slot, +k
//!   goto header
//! exit: <post-loop / return>
//! ```
//!
//! Only one backward branch is allowed; the back branch must target the
//! loop header, the very first body instruction after the header must
//! be the loop exit `if_icmp*` (or the header itself is the comparison,
//! depending on `javac` version), and the induction variable's `iinc`
//! must appear in the loop body.
//!
//! Anything that doesn't match is rejected with a precise
//! [`crate::emitter::LoweringError::UnsupportedNode`] reason.

use crate::emitter::LoweringError;

/// Outcome of the loop scan.
#[derive(Clone, Debug)]
pub(crate) enum LoopShape {
    /// No backward branch — straight-line kernel.
    StraightLine,
    /// Single counted loop matching the canonical pattern.
    Counted(CountedLoop),
}

#[derive(Clone, Debug)]
pub(crate) struct CountedLoop {
    /// PC of the loop header (the back-branch target). Execution
    /// reaches this after the prelude.
    pub header_pc: usize,
    /// PC of the loop-exit `if_icmp*` opcode. Its operands are
    /// `iv_slot` (LHS) and the bound expression (RHS).
    pub exit_if_pc: usize,
    /// PC just past the loop-exit branch (first body instruction).
    pub body_start_pc: usize,
    /// PC of the back-branch `goto`/`goto_w` instruction.
    pub back_branch_pc: usize,
    /// PC of the first instruction after the loop (the exit branch's
    /// target).
    pub exit_pc: usize,
    /// Local-variable slot that holds the induction variable.
    pub iv_slot: u16,
    /// Loop-exit comparison opcode (`if_icmpge` / `if_icmplt` / ...).
    pub exit_op: u8,
}

/// Scan a method's bytecode and classify its loop shape.
pub(crate) fn detect_loop(bytes: &[u8]) -> Result<LoopShape, LoweringError> {
    let backs = collect_backward_branches(bytes)?;
    match backs.len() {
        0 => Ok(LoopShape::StraightLine),
        1 => {
            let (branch_pc, header_pc) = backs[0];
            classify_counted_loop(bytes, branch_pc, header_pc)
        }
        _ => Err(LoweringError::UnsupportedNode(
            "multi-loop or non-canonical control flow".into(),
        )),
    }
}

/// Return all (branch_pc, target_pc) pairs where target_pc < branch_pc.
/// Errors on malformed bytecode.
fn collect_backward_branches(bytes: &[u8]) -> Result<Vec<(usize, usize)>, LoweringError> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < bytes.len() {
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        // 0x99..=0xA7: if* + goto      (3-byte, i16 offset)
        // 0xC6 | 0xC7: ifnull/ifnonnull (3-byte, i16 offset)
        // 0xC8:        goto_w           (5-byte, i32 offset)
        if (0x99..=0xA7).contains(&op) || op == 0xC6 || op == 0xC7 {
            let off = i16::from_be_bytes([
                *bytes.get(pc + 1).ok_or_else(truncated)?,
                *bytes.get(pc + 2).ok_or_else(truncated)?,
            ]) as i32;
            let target = pc as i32 + off;
            // `target == bytes.len()` is one past the last byte — not a
            // valid instruction boundary — so reject it too (`>=`).
            if target < 0 || target as usize >= bytes.len() {
                return Err(LoweringError::UnsupportedNode(format!(
                    "branch at pc={pc} has out-of-range target {target}"
                )));
            }
            if (target as usize) < pc {
                out.push((pc, target as usize));
            }
        } else if op == 0xC8 {
            let off = i32::from_be_bytes([
                *bytes.get(pc + 1).ok_or_else(truncated)?,
                *bytes.get(pc + 2).ok_or_else(truncated)?,
                *bytes.get(pc + 3).ok_or_else(truncated)?,
                *bytes.get(pc + 4).ok_or_else(truncated)?,
            ]);
            let target = pc as i32 + off;
            // `target == bytes.len()` is one past the last byte — not a
            // valid instruction boundary — so reject it too (`>=`).
            if target < 0 || target as usize >= bytes.len() {
                return Err(LoweringError::UnsupportedNode(format!(
                    "goto_w at pc={pc} has out-of-range target {target}"
                )));
            }
            if (target as usize) < pc {
                out.push((pc, target as usize));
            }
        }
        pc += size;
    }
    Ok(out)
}

fn truncated() -> LoweringError {
    LoweringError::UnsupportedNode("truncated bytecode".into())
}

/// Given a back-branch and its target (the loop header), recover the
/// canonical-loop fields. Errors if the shape doesn't match.
fn classify_counted_loop(
    bytes: &[u8],
    back_branch_pc: usize,
    header_pc: usize,
) -> Result<LoopShape, LoweringError> {
    let back_op = bytes[back_branch_pc];
    if back_op != 0xA7 && back_op != 0xC8 {
        return Err(LoweringError::UnsupportedNode(format!(
            "non-canonical back-branch opcode 0x{back_op:02x} at pc={back_branch_pc}"
        )));
    }

    // Walk from header_pc forward. The exit comparison is the first
    // `if_icmp*` between header_pc and back_branch_pc whose target is
    // the post-loop region (i.e., > back_branch_pc).
    let mut exit_if_pc = None;
    let mut exit_pc = None;
    let mut iv_slot = None;
    let mut found_iinc = false;
    let mut pc = header_pc;
    while pc <= back_branch_pc {
        if pc >= bytes.len() {
            break;
        }
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;

        // Loop-exit comparisons we accept: if_icmplt/ge/gt/le (0x9F-0xA4)
        // and the unary if* (0x99-0x9E).
        if (0x99..=0xA4).contains(&op) {
            let off = i16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]) as i32;
            let target = (pc as i32 + off) as usize;
            if target > back_branch_pc && exit_if_pc.is_none() {
                exit_if_pc = Some(pc);
                exit_pc = Some(target);
            }
        }

        // Find the iinc to identify the induction variable slot.
        if op == 0x84 && !found_iinc {
            // iinc index, const  (3 bytes)
            iv_slot = Some(bytes[pc + 1] as u16);
            found_iinc = true;
        } else if op == 0xC4 && bytes.get(pc + 1) == Some(&0x84) && !found_iinc {
            // wide iinc index, const  (6 bytes)
            iv_slot = Some(u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]));
            found_iinc = true;
        }

        pc += size;
    }

    let exit_if_pc = exit_if_pc.ok_or_else(|| {
        LoweringError::UnsupportedNode(
            "no loop-exit comparison found inside loop body".into(),
        )
    })?;
    let exit_pc = exit_pc.unwrap();
    let iv_slot = iv_slot.ok_or_else(|| {
        LoweringError::UnsupportedNode(
            "no iinc found in loop body — induction variable unidentified".into(),
        )
    })?;

    let exit_op = bytes[exit_if_pc];
    let body_start_pc = exit_if_pc + instr_size(bytes, exit_if_pc)?;

    Ok(LoopShape::Counted(CountedLoop {
        header_pc,
        exit_if_pc,
        body_start_pc,
        back_branch_pc,
        exit_pc,
        iv_slot,
        exit_op,
    }))
}

/// JVM instruction size, used here for the loop scanner. Mirrors the
/// table in `analyzer.rs` but lives separately to keep the modules
/// loosely coupled.
pub(crate) fn instr_size(bytes: &[u8], pc: usize) -> Result<usize, LoweringError> {
    let op = bytes[pc];
    Ok(match op {
        0x00..=0x0F
        | 0x1A..=0x35
        | 0x3B..=0x4E
        | 0x4F..=0x56
        | 0x57..=0x5F
        | 0x60..=0x83
        | 0x85..=0x93
        | 0x94..=0x98
        | 0xAC..=0xB1
        | 0xBE
        | 0xBF
        | 0xC2
        | 0xC3 => 1,
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3A | 0xA9 | 0xBC => 2,
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xA8
        | 0xB2..=0xB8
        | 0xBB
        | 0xBD
        | 0xC0
        | 0xC1
        | 0xC6
        | 0xC7 => 3,
        0xC5 => 4,
        0xB9 | 0xBA | 0xC8 | 0xC9 => 5,
        0xC4 => {
            let sub = *bytes
                .get(pc + 1)
                .ok_or_else(|| LoweringError::UnsupportedNode("truncated wide".into()))?;
            if sub == 0x84 { 6 } else { 4 }
        }
        0xAA | 0xAB => {
            // Switches: analyzer rejected, but be defensive.
            return Err(LoweringError::UnsupportedNode(
                "switch opcode not supported".into(),
            ));
        }
        _ => {
            return Err(LoweringError::UnsupportedNode(format!(
                "unknown opcode 0x{op:02x} during size walk"
            )))
        }
    })
}
