// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Native contract of batching planned call-site clears.

use super::*;
use iced_x86::{Decoder, DecoderOptions, Mnemonic};

#[test]
fn planned_clear_batches_preserve_frame_contents_flags_and_resident_values() {
    let selections: &[&[i32]] = &[
        &[],
        &[8],
        &[8, 24, 40, 56],
        &[8, 16, 24, 32],
        &[
            232, 224, 168, 160, 152, 144, 120, 112, 104, 96, 56, 48, 40, 32, 16, 8,
        ],
        &[8, 16, 8, 16, 24, 32, 40, 48, 56, 64],
    ];
    for selected in [false, true] {
        for poison in [false, true] {
            for lazy in [false, true] {
                for &offsets in selections {
                    let mut lo = super::r16_codegen_tests::emitter_lowerer();
                    lo.operand_in_place_on = selected;
                    lo.dead_home_poison = poison;
                    lo.prologue_zero_stretch = false;
                    lo.prologue_zero_rax = false;
                    lo.buf.emit_byte(0x55); // PUSH RBP
                    #[cfg(target_os = "windows")]
                    lo.buf.emit(&[0x48, 0x89, 0xCD]); // MOV RBP, RCX
                    #[cfg(not(target_os = "windows"))]
                    lo.buf.emit(&[0x48, 0x89, 0xFD]); // MOV RBP, RDI
                    let registers = [RAX, RCX, RDX, R10, R11];
                    let sentinels = [
                        0x1122_3344_8765_4321,
                        0x2345_6789_ABCD_EF01,
                        0x3456_789A_BCDE_F012,
                        0x4567_89AB_CDEF_0123,
                        0x5678_9ABC_DEF0_1234,
                    ];
                    for (&reg, &bits) in registers.iter().zip(&sentinels) {
                        lo.emit_mov_reg_imm64(reg, bits);
                    }
                    // Every allocatable volatile FP register holds a live value.
                    for reg in 2..=5 {
                        lo.buf.emit(&[0xF3, 0x0F, 0x6F, 0x85 | (reg << 3)]);
                        lo.buf.emit(&64i32.to_le_bytes()); // MOVDQU xmm, [RBP+64]
                    }
                    lo.buf.emit(&[0x4D, 0x39, 0xDA]); // CMP R10, R11
                    lo.buf.emit(&[0x9C, 0x41, 0x58]); // PUSHFQ; POP R8
                    if lazy {
                        lo.lazy_param_homes = vec![(0, 8, 184), (0, 16, 192)];
                        lo.defined_nodes[0] = true;
                    }
                    let start = lo.buf.pos();
                    lo.emit_dead_home_clear_batch(offsets);
                    let batch = lo.buf.as_slice()[start..].to_vec();
                    for (i, &reg) in registers.iter().enumerate() {
                        let rex = 0x48 | if reg >= 8 { 4 } else { 0 };
                        lo.buf
                            .emit(&[rex, 0x89, 0x45 | ((reg & 7) << 3), (i * 8) as u8]);
                    }
                    for reg in 2..=5 {
                        lo.buf.emit(&[0xF3, 0x0F, 0x7F, 0x85 | (reg << 3)]);
                        lo.buf
                            .emit(&(128i32 + i32::from(reg - 2) * 16).to_le_bytes());
                    }
                    lo.buf.emit(&[0x9C, 0x58]); // PUSHFQ; POP RAX
                    lo.buf.emit(&[0x4C, 0x31, 0xC0]); // XOR RAX, R8
                    lo.buf.emit(&[0x5D, 0xC3]); // POP RBP; RET
                    assert!(!lo.buf.overflowed());
                    lo.buf.finalize();

                    let mut memory: Vec<u64> = (0..96).map(|i| 0x9988_7766_0000_0000 | i).collect();
                    let before = memory.clone();
                    // SAFETY: the buffer preserves RBP and all other ABI saves,
                    // calls nothing and writes only selected negative offsets
                    // and the positive output region inside this allocation.
                    let invoke: extern "C" fn(*mut u64) -> u64 =
                        unsafe { std::mem::transmute(lo.buf.as_ptr()) };
                    let flags_changed = invoke(memory.as_mut_ptr().wrapping_add(48));
                    assert_eq!(flags_changed, 0, "the clear preserves flags");
                    assert_eq!(&memory[48..53], &sentinels, "all scratch GPRs survive");
                    for reg in 2..=5 {
                        let output = 64 + (reg - 2) * 2;
                        assert_eq!(&memory[output..output + 2], &before[56..58]);
                    }
                    let mut cleared = offsets.to_vec();
                    if lazy {
                        for (home, word) in [(8, 184), (16, 192)] {
                            if offsets.contains(&home) {
                                cleared.push(word);
                            }
                        }
                    }
                    let value = if poison {
                        cratonvm_types::jit_activation::DEAD_HOME_POISON
                    } else {
                        0
                    };
                    for i in 0..48 {
                        let off = ((48 - i) * 8) as i32;
                        let expected = if cleared.contains(&off) {
                            value
                        } else {
                            before[i]
                        };
                        assert_eq!(memory[i], expected,
                            "off={off} selected={selected} poison={poison} lazy={lazy} offsets={offsets:?}");
                    }
                    let instructions: Vec<_> = Decoder::new(64, &batch, DecoderOptions::NONE)
                        .into_iter()
                        .collect();
                    if selected && !poison && offsets.len() == 16 {
                        assert!(
                            instructions.iter().any(|i| matches!(
                                i.mnemonic(),
                                Mnemonic::Movdqu | Mnemonic::Movups
                            )),
                            "adjacent planned words use a vector store"
                        );
                        assert!(
                            instructions.len() < cleared.len(),
                            "the batch reduces executed instructions"
                        );
                    }
                    if !selected || poison {
                        assert!(
                            instructions.iter().all(|i| i.mnemonic() == Mnemonic::Mov),
                            "OFF and poison use individual immediate stores"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn argument_stage_clear_preserves_other_words_registers_and_flags() {
    for enabled in [false, true] {
        for count in 0..8 {
            let mut lo = super::r16_codegen_tests::emitter_lowerer();
            lo.operand_in_place_on = enabled;
            lo.args_stage_words = count;
            lo.args_stage_top_off = 112;
            lo.prologue_zero_stretch = false;
            lo.prologue_zero_rax = false;
            lo.buf.emit_byte(0x55);
            #[cfg(target_os = "windows")]
            lo.buf.emit(&[0x48, 0x89, 0xCD]);
            #[cfg(not(target_os = "windows"))]
            lo.buf.emit(&[0x48, 0x89, 0xFD]);
            lo.emit_mov_reg_imm64(RAX, 0x123456789abcdef0);
            lo.emit_mov_reg_imm64(R10, 17);
            lo.emit_mov_reg_imm64(R11, 29);
            lo.buf.emit(&[0x4D, 0x39, 0xDA]); // CMP R10, R11
            lo.buf.emit(&[0x9C, 0x41, 0x58]); // PUSHFQ; POP R8
            let start = lo.buf.pos();
            lo.emit_dead_arg_stage_clears();
            let bytes = lo.buf.as_slice()[start..].to_vec();
            lo.buf.emit(&[0x48, 0x89, 0x45, 0x00]); // MOV [RBP], RAX
            lo.buf.emit(&[0x9C, 0x58, 0x4C, 0x31, 0xC0]); // flags XOR
            lo.buf.emit(&[0x5D, 0xC3]);
            lo.buf.finalize();
            let mut words: Vec<u64> = (0..32).map(|i| 0xface_0000 + i).collect();
            let before = words.clone();
            let run: extern "C" fn(*mut u64) -> u64 =
                // SAFETY: the finalized fixture emits this C ABI and its executable buffer remains live.
                unsafe { std::mem::transmute(lo.buf.as_ptr()) };
            assert_eq!(run(words.as_mut_ptr().wrapping_add(16)), 0);
            assert_eq!(words[16], 0x123456789abcdef0);
            for i in 0..16 {
                let cleared = (0..count).any(|n| i == 16 - (112 - 8 * n) / 8);
                assert_eq!(words[i], if cleared { 0 } else { before[i] });
            }
            let instructions: Vec<_> =
                Decoder::new(64, &bytes, DecoderOptions::NONE).into_iter().collect();
            if enabled && count >= 4 {
                assert!(instructions.len() < count);
            } else {
                assert_eq!(instructions.len(), count);
            }
        }
    }
}

#[test]
fn argument_stage_clear_stops_before_frame_base() {
    let mut lo = super::r16_codegen_tests::emitter_lowerer();
    lo.operand_in_place_on = true;
    lo.args_stage_words = 8;
    lo.args_stage_top_off = 24;
    lo.prologue_zero_stretch = false;
    lo.prologue_zero_rax = false;
    lo.buf.emit_byte(0x55);
    #[cfg(target_os = "windows")]
    lo.buf.emit(&[0x48, 0x89, 0xCD]);
    #[cfg(not(target_os = "windows"))]
    lo.buf.emit(&[0x48, 0x89, 0xFD]);
    lo.emit_dead_arg_stage_clears();
    lo.buf.emit(&[0x5D, 0xC3]);
    lo.buf.finalize();
    let mut words = [9_u64; 10];
    // SAFETY: the finalized fixture emits this C ABI and its executable buffer remains live.
    let run: extern "C" fn(*mut u64) = unsafe { std::mem::transmute(lo.buf.as_ptr()) };
    run(words.as_mut_ptr().wrapping_add(5));
    assert_eq!(words, [9, 9, 0, 0, 0, 9, 9, 9, 9, 9]);
}

#[test]
fn argument_stage_clear_keeps_the_previous_encoding() {
    for enabled in [false, true] {
        for top in [0, 8, 24, 40, 112, 2048, 2049] {
            for count in 0..20 {
                let mut actual = super::r16_codegen_tests::emitter_lowerer();
                let mut expected = super::r16_codegen_tests::emitter_lowerer();
                for lo in [&mut actual, &mut expected] {
                    lo.operand_in_place_on = enabled;
                    lo.args_stage_words = count;
                    lo.args_stage_top_off = top;
                    lo.prologue_zero_stretch = false;
                    lo.prologue_zero_rax = false;
                }
                actual.emit_dead_arg_stage_clears();

                let offsets: Vec<_> = (0..count)
                    .map_while(|i| {
                        i32::try_from(i).ok()
                            .and_then(|i| i.checked_mul(8))
                            .and_then(|delta| top.checked_sub(delta))
                            .filter(|&off| off > 0)
                    })
                    .collect();
                if enabled && offsets.len() >= 4 {
                    expected.buf.emit(&[0x66, 0x0F, 0xEF, 0xC0]);
                    let mut pairs = offsets.chunks_exact(2);
                    for pair in &mut pairs {
                        expected.emit_xmm_frame_move(XMM0, pair[0], true);
                    }
                    for &off in pairs.remainder() {
                        expected.emit_zero_frame_slot(off);
                    }
                } else {
                    for off in offsets {
                        expected.emit_zero_frame_slot(off);
                    }
                }
                assert_eq!(
                    actual.buf.as_slice(),
                    expected.buf.as_slice(),
                    "enabled={enabled} top={top} count={count}"
                );
            }
        }
    }
}
