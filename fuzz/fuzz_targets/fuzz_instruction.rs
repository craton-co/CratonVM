// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the JVM bytecode instruction decoder.
//!
//! Surface under test:
//!   * `cratonvm_reader::instruction::Instruction::decode(code, pc)` —
//!     decodes a single instruction from a `&[u8]` code array at byte
//!     offset `pc`, returning `(Instruction, next_pc)`. The decoder
//!     handles variable-length operands (`bipush`/`sipush`/`ldc`),
//!     `wide`-prefixed forms, and the alignment-padded
//!     `tableswitch`/`lookupswitch` instructions — all classic sources
//!     of off-by-one and out-of-bounds reads on adversarial bytecode.
//!
//! The harness treats the entire fuzz input as a code array and walks it
//! by repeatedly decoding at the returned `next_pc`. The oracle is
//! panic-only: any `Err` (e.g. `UnexpectedEndOfData`) is the documented
//! contract for truncated/garbage code; only a panic / abort is a bug.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_instruction

#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_reader::instruction::Instruction;

/// `Code.code_length` is a `u4` but HotSpot rejects methods whose code
/// exceeds 65 535 bytes. 256 KiB is a generous cap that keeps each
/// decode walk bounded.
const MAX_INPUT: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    // Walk the code array instruction-by-instruction. `decode` returns
    // the offset of the next instruction; a malformed/truncated operand
    // must surface as `Err`, never a panic. Guard against a decoder that
    // fails to advance `pc` (would otherwise loop forever) by requiring
    // strict forward progress.
    let mut pc = 0usize;
    while pc < data.len() {
        match Instruction::decode(data, pc) {
            Ok((_instr, next)) => {
                if next <= pc {
                    // No forward progress — stop rather than spin. A
                    // non-advancing decode is itself a contract bug, but
                    // we do not panic here: the harness's job is to feed
                    // the decoder, not to assert on it.
                    break;
                }
                pc = next;
            }
            Err(_) => break,
        }
    }

    // Also probe decoding at a few interior offsets so the fuzzer can
    // reach operand-parsing paths even when the byte-0 opcode would
    // otherwise terminate the linear walk early.
    if !data.is_empty() {
        let mid = data.len() / 2;
        let _ = Instruction::decode(data, mid);
        let _ = Instruction::decode(data, data.len() - 1);
    }
});
