// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 11, wave 8, lane `x64` — page
//! `r11w7-orch-putstatic-in-a-synchronized-block-keeps-the-method-interpreted`.
//!
//! javac compiles `synchronized (x) { .. }` with a catch-any handler that reads
//! the monitor local, which is not a parameter, so RBC.6 demands that every
//! throwing site in the block publish a precise exceptional frame. `putstatic`
//! was the one opcode in a typical block that `precise_frame_publishing_opcode`
//! did not admit, so `R11LockInlineHitRate.plain` stayed interpreted
//! (`rbc6-handler-reads-unsafe-local(pc=20,op=0xb3)`). The `0xb3` arm always
//! ended in the publishing post-call check; the admission was bookkeeping.
//! The lowering half is unit-tested in `x64/deopt_stubs.rs`
//! (`a_protected_putstatic_publishes_a_reason_9_frame_at_its_own_bci`).

#![cfg(target_arch = "x86_64")]

use cratonvm_reader::attribute::ExceptionTableEntry;

/// `static int plain(int k) { synchronized (PLAIN) { if (a != b) torn++;
/// a += k; b += k; return a; } }`, byte for byte as javac 25 emits it.
const PLAIN: [u8; 50] = [
    0xb2, 0x00, 0x01, // 0: getstatic PLAIN
    0x59, // 3: dup
    0x4c, // 4: astore_1
    0xc2, // 5: monitorenter
    0xb2, 0x00, 0x02, // 6: getstatic a
    0xb2, 0x00, 0x03, // 9: getstatic b
    0x9f, 0x00, 0x0b, // 12: if_icmpeq 23
    0xb2, 0x00, 0x04, // 15: getstatic torn
    0x04, // 18: iconst_1
    0x60, // 19: iadd
    0xb3, 0x00, 0x04, // 20: putstatic torn   <- the refused site
    0xb2, 0x00, 0x02, // 23: getstatic a
    0x1a, // 26: iload_0
    0x60, // 27: iadd
    0xb3, 0x00, 0x02, // 28: putstatic a
    0xb2, 0x00, 0x03, // 31: getstatic b
    0x1a, // 34: iload_0
    0x60, // 35: iadd
    0xb3, 0x00, 0x03, // 36: putstatic b
    0xb2, 0x00, 0x02, // 39: getstatic a
    0x2b, // 42: aload_1
    0xc3, // 43: monitorexit
    0xac, // 44: ireturn
    0x4d, // 45: astore_2
    0x2b, // 46: aload_1
    0xc3, // 47: monitorexit
    0x2c, // 48: aload_2
    0xbf, // 49: athrow
];

fn plain_table() -> [ExceptionTableEntry; 2] {
    [
        ExceptionTableEntry {
            start_pc: 6,
            end_pc: 44,
            handler_pc: 45,
            catch_type: 0,
        },
        ExceptionTableEntry {
            start_pc: 45,
            end_pc: 48,
            handler_pc: 45,
            catch_type: 0,
        },
    ]
}

#[test]
fn a_javac_synchronized_block_that_stores_a_static_is_admitted() {
    assert_eq!(
        cratonvm_jit::first_unsupported_precise_frame_site(&PLAIN, PLAIN.len(), &plain_table()),
        None,
        "every throwing site in the block publishes a precise frame; a protected \
         putstatic must not keep the method interpreted"
    );
}

/// Not vacuous: the walk really reaches the putstatic. Cut the stream in the
/// middle of its operand and the scan loses sync AT that instruction, which it
/// reports as the refusing site — so an answer of `None` above was a verdict
/// about pc 20, not a walk that stopped short of it.
#[test]
fn the_scan_reaches_the_protected_putstatic() {
    assert_eq!(
        cratonvm_jit::first_unsupported_precise_frame_site(&PLAIN, 22, &plain_table()),
        Some((20, 0xb3))
    );
}
