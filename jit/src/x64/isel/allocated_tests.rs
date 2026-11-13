// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::{encode_allocated_alu, select, Mem, Op, Operand, Req, Ty};

fn selected(op: Op, ty: Ty, dst: Operand, src: Operand) -> Vec<u8> {
    select(&Req::new(op, ty, dst, src))
        .expect("the reference instruction has a checked pattern")
        .encoded
        .bytes
}

#[test]
fn allocated_alu_rejects_destructive_operand_and_address_overlap() {
    assert!(encode_allocated_alu(Op::Sub, Ty::I64, 12, 3, Operand::Gpr(12)).is_none());
    for rhs in [
        Operand::Mem(Mem::base_disp(12, 0)),
        Operand::Mem(Mem::base_index(5, 12, 2, 0)),
    ] {
        assert!(encode_allocated_alu(Op::Add, Ty::I64, 12, 3, rhs).is_none());
    }
    assert!(encode_allocated_alu(Op::Add, Ty::I64, 16, 3, Operand::Gpr(1)).is_none());
    assert!(encode_allocated_alu(Op::Add, Ty::F64, 12, 3, Operand::Gpr(1)).is_none());
}

/// Register destinations and operands are bound independently. The native
/// result checks destructive aliases, extended registers, frame operands,
/// immediate boundaries, 32-bit zero extension and Java integer wrapping.
#[test]
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn allocated_alu_executes_register_frame_and_immediate_locations() {
    let inputs = [
        (0xDEAD_BEEF_8000_0001u64, 0x1234_5678_0000_0080u64),
        (u64::MAX, 0x8000_0000_0000_0000u64),
    ];
    for ty in [Ty::I32, Ty::I64] {
        for op in [
            Op::Add,
            Op::Sub,
            Op::And,
            Op::Or,
            Op::Xor,
            Op::Imul,
            Op::Shl,
            Op::Shr,
            Op::Sar,
        ] {
            for dst in [0, 1, 3, 8, 12, 13, 14, 15] {
                for lhs in [3, 12] {
                    for rhs in [
                        Operand::Gpr(dst),
                        Operand::Gpr(1),
                        Operand::Gpr(13),
                        Operand::Imm(0),
                        Operand::Imm(-1),
                        Operand::Imm(32),
                        Operand::Imm(64),
                        Operand::Imm(-129),
                        Operand::Imm(-128),
                        Operand::Imm(127),
                        Operand::Imm(128),
                        Operand::Imm(i64::from(i32::MIN)),
                        Operand::Imm(i64::from(i32::MAX)),
                        Operand::Mem(Mem::base_disp(5, 0)),
                        Operand::Mem(Mem::base_disp(4, 0)),
                        Operand::Mem(Mem::base_disp(12, 0)),
                        Operand::Mem(Mem::base_disp(13, 8)),
                        Operand::Mem(Mem::base_index(5, 11, 8, 0)),
                    ] {
                        let bytes = encode_allocated_alu(op, ty, dst, lhs, rhs);
                        if matches!(op, Op::Shl | Op::Shr | Op::Sar)
                            && !matches!(rhs, Operand::Imm(_))
                        {
                            assert!(bytes.is_none());
                            continue;
                        }
                        if op == Op::Sub && rhs == Operand::Gpr(dst) && dst != lhs {
                            assert!(bytes.is_none());
                            continue;
                        }
                        if let Operand::Mem(mem) = rhs {
                            if mem.base == lhs {
                                continue;
                            }
                            if mem.base == dst || mem.index.is_some_and(|index| index.reg == dst) {
                                assert!(bytes.is_none());
                                continue;
                            }
                        }
                        let bytes = bytes.expect("encode assigned operands without an accumulator");
                        for (a, b) in inputs {
                            let b = match rhs {
                                Operand::Gpr(r) if r == lhs => a,
                                Operand::Imm(k) => k as u64,
                                _ => b,
                            };
                            let mut code = vec![
                                0x55, 0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48,
                                0x83, 0xEC,
                                0x10, // save nonvolatile registers and a frame word
                                0x48, 0x89, 0xE5, // RBP = RSP
                            ];
                            code.extend(selected(
                                Op::Mov,
                                Ty::I64,
                                Operand::Gpr(0),
                                Operand::Imm(b as i64),
                            ));
                            if let Operand::Mem(mem) = rhs {
                                if mem.base != 4 && mem.base != 5 {
                                    code.extend(selected(
                                        Op::Mov,
                                        Ty::I64,
                                        Operand::Gpr(mem.base),
                                        Operand::Gpr(5),
                                    ));
                                }
                                if let Some(index) = mem.index {
                                    code.extend(selected(
                                        Op::Mov,
                                        Ty::I64,
                                        Operand::Gpr(index.reg),
                                        Operand::Imm(0),
                                    ));
                                }
                                code.extend(selected(Op::Mov, Ty::I64, rhs, Operand::Gpr(0)));
                            }
                            code.extend(selected(
                                Op::Mov,
                                Ty::I64,
                                Operand::Gpr(dst),
                                Operand::Imm(0x1122_3344),
                            ));
                            code.extend(selected(
                                Op::Mov,
                                Ty::I64,
                                Operand::Gpr(lhs),
                                Operand::Imm(a as i64),
                            ));
                            if let Operand::Gpr(r) = rhs {
                                code.extend(selected(
                                    Op::Mov,
                                    Ty::I64,
                                    Operand::Gpr(r),
                                    Operand::Imm(b as i64),
                                ));
                            }
                            code.extend_from_slice(&bytes);
                            code.extend(selected(
                                Op::Mov,
                                Ty::I64,
                                Operand::Gpr(0),
                                Operand::Gpr(dst),
                            ));
                            code.extend_from_slice(&[
                                0x48, 0x83, 0xC4, 0x10, 0x41, 0x5F, 0x41, 0x5E, 0x41, 0x5D, 0x41,
                                0x5C, 0x5B, 0x5D, 0xC3,
                            ]);
                            let mut buffer =
                                crate::ExecutableBuffer::new(256).expect("native fixture");
                            buffer.emit(&code);
                            buffer.finalize();
                            // SAFETY: the leaf saves and restores all nonvolatile
                            // registers, accesses its own frame, and returns RAX.
                            let run = unsafe {
                                std::mem::transmute::<*const u8, extern "C" fn() -> u64>(
                                    buffer.as_ptr(),
                                )
                            };
                            let expected = match op {
                                Op::Add => a.wrapping_add(b),
                                Op::Sub => a.wrapping_sub(b),
                                Op::And => a & b,
                                Op::Or => a | b,
                                Op::Xor => a ^ b,
                                Op::Imul => a.wrapping_mul(b),
                                Op::Shl if ty == Ty::I32 => {
                                    u64::from((a as u32).wrapping_shl(b as u32))
                                }
                                Op::Shl => a.wrapping_shl(b as u32),
                                Op::Shr if ty == Ty::I32 => {
                                    u64::from((a as u32).wrapping_shr(b as u32))
                                }
                                Op::Shr => a.wrapping_shr(b as u32),
                                Op::Sar if ty == Ty::I32 => {
                                    (a as i32).wrapping_shr(b as u32) as u32 as u64
                                }
                                Op::Sar => (a as i64).wrapping_shr(b as u32) as u64,
                                _ => unreachable!(),
                            };
                            let expected = if ty == Ty::I32 {
                                u64::from(expected as u32)
                            } else {
                                expected
                            };
                            assert_eq!(
                                run(),
                                expected,
                                "{op:?} {ty:?} d={dst} lhs={lhs} rhs={rhs:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

use super::{AddrSource, ImmForm, IrAddr, MInst};
#[test]
fn selected_integer_forms_use_the_available_allocated_patterns() {
    for ty in [Ty::I32, Ty::I64] {
        for op in [Op::Add, Op::Sub, Op::And, Op::Or, Op::Xor, Op::Imul] {
            for (imm, form) in [(7, ImmForm::Imm8), (1000, ImmForm::Imm32)] {
                let instruction = MInst::AluRI {
                    op,
                    ty,
                    dst: 2,
                    lhs: 1,
                    imm,
                    form,
                };
                assert!(instruction.probe().is_ok(), "{instruction:?}");
            }
            let instruction = MInst::AluRM {
                op,
                ty,
                dst: 2,
                lhs: 1,
                addr: AddrSource::Expr(IrAddr {
                    base: Some(0),
                    index: Some(1),
                    scale: 4,
                    disp: 128,
                }),
                load: 3,
            };
            assert!(instruction.probe().is_ok(), "{instruction:?}");
        }
    }
    assert!(MInst::Move {
        dst: 2,
        ty: Ty::I32,
        src: 1
    }
    .probe()
    .is_ok());
}
