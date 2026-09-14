// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Instruction type effects for bytecode verification.
//!
//! Each JVM instruction has a well-defined effect on the operand stack and local
//! variables. This module describes these effects in terms of `VType` — the
//! verifier applies them to a `VerificationFrame` as it walks bytecode.
//!
//! This is the core of Pass 3 verification (JVM spec 4.10.1).

use std::sync::Arc;

use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::instruction::Instruction;

use super::verify_frame::VerificationFrame;
use super::vtype::{return_type_from_descriptor, ClassHierarchy, VType};
use cratonvm_types::error::LinkageError;

/// The result of verifying a single instruction's type effects.
#[derive(Debug)]
pub struct InsnVerifyResult {
    /// Whether control falls through to the next instruction.
    pub falls_through: bool,
    /// Branch targets (absolute offsets) this instruction may jump to.
    pub branch_targets: Vec<u16>,
}

/// Verify the type effects of a single instruction on the verification frame.
///
/// Modifies `frame` in place to reflect the instruction's stack/local effects.
/// Returns branch targets and whether control falls through.
///
/// `method_descriptor` is the descriptor of the method being verified (e.g.
/// `"(I)Ljava/lang/String;"`). It is required so that `areturn` can check the
/// returned reference is assignable to the method's declared return type
/// (JVMS §4.10.1.6 / §6.5 areturn) — without it the verifier would accept any
/// reference for `areturn`, a soundness hole.
pub fn verify_instruction(
    insn: &Instruction,
    pc: usize,
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    _class_name: &str,
    _method_name: &str,
    method_descriptor: &str,
    hierarchy: &dyn ClassHierarchy,
) -> Result<InsnVerifyResult, LinkageError> {
    let current_class_name = _class_name;
    match insn {
        // =====================================================================
        // Constants — push known types
        // =====================================================================
        Instruction::Nop => ok_through(),

        Instruction::AconstNull => {
            frame.push(VType::Null)?;
            ok_through()
        }

        Instruction::IconstM1
        | Instruction::Iconst0
        | Instruction::Iconst1
        | Instruction::Iconst2
        | Instruction::Iconst3
        | Instruction::Iconst4
        | Instruction::Iconst5
        | Instruction::Bipush(_)
        | Instruction::Sipush(_) => {
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lconst0 | Instruction::Lconst1 => {
            frame.push(VType::Long)?;
            frame.push(VType::Top)?; // category-2 second slot
            ok_through()
        }

        Instruction::Fconst0 | Instruction::Fconst1 | Instruction::Fconst2 => {
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dconst0 | Instruction::Dconst1 => {
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Ldc(index) => verify_ldc(frame, cp, *index as u16),
        Instruction::LdcW(index) => verify_ldc(frame, cp, *index),
        Instruction::Ldc2W(index) => verify_ldc2w(frame, cp, *index),

        // =====================================================================
        // Loads — read from local variable, push to stack
        // =====================================================================
        Instruction::Iload(index) => {
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Int, hierarchy) {
                return Err(verify_err(&format!(
                    "iload: local {index} is {local:?}, expected Int"
                )));
            }
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lload(index) => {
            // JVMS §4.10.1.6: the pair (`index`, `index+1`) must still be an
            // intact category-2 value. Checking only the base would accept a
            // pair whose upper half an intervening category-1 store replaced.
            frame
                .local_load_wide(*index, &VType::Long)
                .map_err(|e| prefix_verify_err("lload", e))?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Fload(index) => {
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Float, hierarchy) {
                return Err(verify_err(&format!(
                    "fload: local {index} is {local:?}, expected Float"
                )));
            }
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dload(index) => {
            frame
                .local_load_wide(*index, &VType::Double)
                .map_err(|e| prefix_verify_err("dload", e))?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Aload(index) => {
            let local = frame.local_load(*index)?.clone();
            if !local.is_reference() && local != VType::ReturnAddress(0) {
                // ReturnAddress is also loadable by aload (for jsr/ret)
                // We need a more permissive check for ReturnAddress
                match &local {
                    VType::ReturnAddress(_) => {}
                    _ if !local.is_reference() => {
                        return Err(verify_err(&format!(
                            "aload: local {index} is {local:?}, expected reference"
                        )));
                    }
                    _ => {}
                }
            }
            frame.push(local)?;
            ok_through()
        }

        // Array loads: pop index (int), pop arrayref, push element type
        Instruction::Iaload | Instruction::Baload | Instruction::Caload | Instruction::Saload => {
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            pop_array_ref(frame, hierarchy)?; // arrayref
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Laload => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_ref(frame, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Faload => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_ref(frame, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Daload => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_ref(frame, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Aaload => {
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            let arrayref = frame.pop()?; // arrayref
                                         // Determine element type from array descriptor
            let elem_type = match &arrayref {
                VType::ArrayRef(desc) => {
                    let elem_desc = &desc[1..];
                    VType::from_field_descriptor(elem_desc)
                }
                VType::Null => VType::Null,
                _ => VType::ObjectRef(Arc::from("java/lang/Object")),
            };
            frame.push(elem_type)?;
            ok_through()
        }

        // =====================================================================
        // Stores — pop from stack, store to local variable
        // =====================================================================
        Instruction::Istore(index) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.local_store(*index, VType::Int)?;
            ok_through()
        }

        Instruction::Lstore(index) => {
            frame.pop()?; // Top (second slot)
            frame.pop_expect(&VType::Long, hierarchy)?;
            // `local_store_wide` bounds-checks BOTH slots against `max_locals`
            // and computes `index + 1` with `checked_add` — the previous
            // `*index + 1` overflowed for a `wide lstore 65535`.
            frame
                .local_store_wide(*index, VType::Long)
                .map_err(|e| prefix_verify_err("lstore", e))?;
            ok_through()
        }

        Instruction::Fstore(index) => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.local_store(*index, VType::Float)?;
            ok_through()
        }

        Instruction::Dstore(index) => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame
                .local_store_wide(*index, VType::Double)
                .map_err(|e| prefix_verify_err("dstore", e))?;
            ok_through()
        }

        Instruction::Astore(index) => {
            let val = frame.pop()?;
            if !val.is_reference() && !matches!(val, VType::ReturnAddress(_)) {
                return Err(verify_err(&format!(
                    "astore: expected reference, found {val:?}"
                )));
            }
            frame.local_store(*index, val)?;
            ok_through()
        }

        // Array stores: pop value, pop index, pop arrayref
        Instruction::Iastore
        | Instruction::Bastore
        | Instruction::Castore
        | Instruction::Sastore => {
            frame.pop_expect(&VType::Int, hierarchy)?; // value
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            pop_array_ref(frame, hierarchy)?; // arrayref
            ok_through()
        }

        Instruction::Lastore => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_ref(frame, hierarchy)?;
            ok_through()
        }

        Instruction::Fastore => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_ref(frame, hierarchy)?;
            ok_through()
        }

        Instruction::Dastore => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_ref(frame, hierarchy)?;
            ok_through()
        }

        Instruction::Aastore => {
            // T1.3.5 — verify the value is a reference and (when the
            // array's element type is statically known) is
            // assignment-compatible with the element type.
            //
            // Per JVMS §6.5 aastore: "the value must be of a type
            // that is assignment compatible with the component type
            // of the array". Full assignment compatibility against
            // arbitrary class hierarchies is checked at runtime to
            // raise `ArrayStoreException`, but the verifier rejects
            // anything that isn't a reference at all.
            let value = frame.pop()?;
            if !value.is_reference() {
                return Err(verify_err(&format!(
                    "aastore: expected reference value, found {value:?}"
                )));
            }
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            let arrayref = frame.pop()?;
            // Verify arrayref is either an ArrayRef or null (allowed —
            // it'll NPE at runtime, which is the spec'd outcome).
            match &arrayref {
                VType::ArrayRef(desc) => {
                    // Element type must itself be a reference (an array
                    // of references). Reject `int[]` here since aastore
                    // is reference-element only.
                    let elem_desc = &desc[1..];
                    let first = elem_desc.as_bytes().first().copied().unwrap_or(b'?');
                    if first != b'L' && first != b'[' {
                        return Err(verify_err(&format!(
                            "aastore: array element type {elem_desc} is not a reference"
                        )));
                    }
                }
                VType::Null => {} // runtime NPE
                _ => {
                    return Err(verify_err(&format!(
                        "aastore: expected array reference, found {arrayref:?}"
                    )));
                }
            }
            ok_through()
        }

        // =====================================================================
        // Stack manipulation
        // =====================================================================
        Instruction::Pop => {
            // Operates on a single slot. The top slot must be a complete
            // category-1 value — `Top` sitting above a `Long`/`Double` base
            // is the upper half of a category-2 value and may not be split.
            let val = frame.pop()?;
            if is_cat2_upper_half(frame, &val) {
                return Err(verify_err("pop: cannot pop category-2 value with pop"));
            }
            ok_through()
        }

        Instruction::Pop2 => {
            // Pops the top two slots: either two category-1 values, or one
            // category-2 value (`Long`/`Double` base + `Top`).
            let slots = pop_n_slots(frame, 2, "pop2")?;
            check_no_split(&slots, "pop2")?;
            ok_through()
        }

        Instruction::Dup => {
            // Duplicate the top single slot. The top slot must be a complete
            // category-1 value.
            let val = frame.pop()?;
            if is_cat2_upper_half(frame, &val) {
                return Err(verify_err("dup: cannot dup category-2 value"));
            }
            frame.push(val.clone())?;
            frame.push(val)?;
            ok_through()
        }

        Instruction::DupX1 => {
            // ..., v2, v1 -> ..., v1, v2, v1   (both single category-1 slots)
            let val1 = frame.pop()?;
            let val2 = frame.pop()?;
            if is_cat2_upper_half(frame, &val2) || val1 == VType::Top || val2 == VType::Top {
                return Err(verify_err("dup_x1: category-2 value not allowed"));
            }
            frame.push(val1.clone())?;
            frame.push(val2)?;
            frame.push(val1)?;
            ok_through()
        }

        Instruction::DupX2 => {
            // ..., {v3,v2}, v1 -> ..., v1, {v3,v2}, v1
            // v1 is a single category-1 slot; {v3,v2} is the next two slots
            // (either two category-1, or one category-2).
            let val1 = frame.pop()?;
            if is_cat2_upper_half(frame, &val1) || val1 == VType::Top {
                return Err(verify_err("dup_x2: val1 must be category-1"));
            }
            let lower = pop_n_slots(frame, 2, "dup_x2")?;
            check_no_split(&lower, "dup_x2")?;
            frame.push(val1.clone())?;
            for v in &lower {
                frame.push(v.clone())?;
            }
            frame.push(val1)?;
            ok_through()
        }

        Instruction::Dup2 => {
            // Duplicate the top two slots (either two category-1 values, or
            // one category-2 value).
            let slots = pop_n_slots(frame, 2, "dup2")?;
            check_no_split(&slots, "dup2")?;
            for v in &slots {
                frame.push(v.clone())?;
            }
            for v in &slots {
                frame.push(v.clone())?;
            }
            ok_through()
        }

        Instruction::Dup2X1 => {
            // ..., v3, {v2,v1} -> ..., {v2,v1}, v3, {v2,v1}
            // top two slots ({v2,v1}) followed by one category-1 slot (v3).
            let top = pop_n_slots(frame, 2, "dup2_x1")?;
            check_no_split(&top, "dup2_x1")?;
            let val3 = frame.pop()?;
            if is_cat2_upper_half(frame, &val3) || val3 == VType::Top {
                return Err(verify_err("dup2_x1: third value must be category-1"));
            }
            for v in &top {
                frame.push(v.clone())?;
            }
            frame.push(val3)?;
            for v in &top {
                frame.push(v.clone())?;
            }
            ok_through()
        }

        Instruction::Dup2X2 => {
            // ..., {v4,v3}, {v2,v1} -> ..., {v2,v1}, {v4,v3}, {v2,v1}
            // top two slots followed by next two slots; neither pair may be
            // split across a category-2 boundary.
            let top = pop_n_slots(frame, 2, "dup2_x2")?;
            check_no_split(&top, "dup2_x2")?;
            let lower = pop_n_slots(frame, 2, "dup2_x2")?;
            check_no_split(&lower, "dup2_x2")?;
            for v in &top {
                frame.push(v.clone())?;
            }
            for v in &lower {
                frame.push(v.clone())?;
            }
            for v in &top {
                frame.push(v.clone())?;
            }
            ok_through()
        }

        Instruction::Swap => {
            let val1 = frame.pop()?;
            let val2 = frame.pop()?;
            if val1 == VType::Top || val2 == VType::Top || is_cat2_upper_half(frame, &val2) {
                return Err(verify_err("swap: cannot swap category-2 values"));
            }
            frame.push(val1)?;
            frame.push(val2)?;
            ok_through()
        }

        // =====================================================================
        // Arithmetic — pop operands, push result (same type)
        // =====================================================================
        Instruction::Iadd
        | Instruction::Isub
        | Instruction::Imul
        | Instruction::Idiv
        | Instruction::Irem
        | Instruction::Iand
        | Instruction::Ior
        | Instruction::Ixor => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Ladd
        | Instruction::Lsub
        | Instruction::Lmul
        | Instruction::Ldiv
        | Instruction::Lrem
        | Instruction::Land
        | Instruction::Lor
        | Instruction::Lxor => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.pop()?; // Top
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Fadd
        | Instruction::Fsub
        | Instruction::Fmul
        | Instruction::Fdiv
        | Instruction::Frem => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dadd
        | Instruction::Dsub
        | Instruction::Dmul
        | Instruction::Ddiv
        | Instruction::Drem => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.pop()?; // Top
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Ineg => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lneg => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Fneg => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dneg => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        // Shifts: int operands, result is same type as first operand
        Instruction::Ishl | Instruction::Ishr | Instruction::Iushr => {
            frame.pop_expect(&VType::Int, hierarchy)?; // shift amount
            frame.pop_expect(&VType::Int, hierarchy)?; // value
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lshl | Instruction::Lshr | Instruction::Lushr => {
            frame.pop_expect(&VType::Int, hierarchy)?; // shift amount (int)
            frame.pop()?; // Top
            frame.pop_expect(&VType::Long, hierarchy)?; // value
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Iinc { index, .. } => {
            // iinc doesn't touch the stack; just verify the local is int
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Int, hierarchy) {
                return Err(verify_err(&format!(
                    "iinc: local {index} is {local:?}, expected Int"
                )));
            }
            ok_through()
        }

        // =====================================================================
        // Conversions
        // =====================================================================
        Instruction::I2l => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::I2f => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }
        Instruction::I2d => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::L2i => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }
        Instruction::L2f => {
            frame.pop()?;
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }
        Instruction::L2d => {
            frame.pop()?;
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::F2i => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }
        Instruction::F2l => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::F2d => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::D2i => {
            frame.pop()?;
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }
        Instruction::D2l => {
            frame.pop()?;
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::D2f => {
            frame.pop()?;
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }
        Instruction::I2b | Instruction::I2c | Instruction::I2s => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        // =====================================================================
        // Comparisons — pop operands, push int result
        // =====================================================================
        Instruction::Lcmp => {
            frame.pop()?;
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.pop()?;
            frame.pop_expect(&VType::Long, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Fcmpl | Instruction::Fcmpg => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Dcmpl | Instruction::Dcmpg => {
            frame.pop()?;
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.pop()?;
            frame.pop_expect(&VType::Double, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        // =====================================================================
        // Branches — pop operands, add branch target
        // =====================================================================
        Instruction::Ifeq(offset)
        | Instruction::Ifne(offset)
        | Instruction::Iflt(offset)
        | Instruction::Ifge(offset)
        | Instruction::Ifgt(offset)
        | Instruction::Ifle(offset) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: vec![target],
            })
        }

        Instruction::IfIcmpeq(offset)
        | Instruction::IfIcmpne(offset)
        | Instruction::IfIcmplt(offset)
        | Instruction::IfIcmpge(offset)
        | Instruction::IfIcmpgt(offset)
        | Instruction::IfIcmple(offset) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: vec![target],
            })
        }

        Instruction::IfAcmpeq(offset) | Instruction::IfAcmpne(offset) => {
            pop_reference(frame, hierarchy)?;
            pop_reference(frame, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: vec![target],
            })
        }

        Instruction::Ifnull(offset) | Instruction::Ifnonnull(offset) => {
            pop_reference(frame, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: vec![target],
            })
        }

        // =====================================================================
        // Control — goto, switch, return
        // =====================================================================
        Instruction::Goto(offset) => {
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: vec![target],
            })
        }

        Instruction::GotoW(offset) => {
            let target = branch_target(pc, *offset);
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: vec![target],
            })
        }

        Instruction::Tableswitch(ts) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let mut targets: Vec<u16> = ts
                .offsets
                .iter()
                .map(|off| branch_target(pc, *off))
                .collect();
            targets.push(branch_target(pc, ts.default));
            // Deduplicate
            targets.sort();
            targets.dedup();
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: targets,
            })
        }

        Instruction::Lookupswitch(ls) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let mut targets: Vec<u16> = ls
                .pairs
                .iter()
                .map(|(_, off)| branch_target(pc, *off))
                .collect();
            targets.push(branch_target(pc, ls.default));
            targets.sort();
            targets.dedup();
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: targets,
            })
        }

        Instruction::Ireturn => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Lreturn => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Long, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Freturn => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Dreturn => {
            frame.pop()?; // Top
            frame.pop_expect(&VType::Double, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Areturn => {
            // JVMS §4.10.1.6 / §6.5 areturn: the value popped must be a
            // reference that is assignable to the method's declared
            // return type. Popping any reference unchecked would let a
            // method declared to return type `T` actually return an
            // unrelated reference, breaking type safety for every caller.
            let value = pop_reference(frame, hierarchy)?;
            match return_type_from_descriptor(method_descriptor) {
                Some(declared @ (VType::ObjectRef(_) | VType::ArrayRef(_))) => {
                    // Loader-aware execution can hold distinct ClassIds for one
                    // binary type while Pass 3 frames retain only its binary
                    // name. That ambiguity is real, but it is already resolved
                    // upstream, not here: `ClassManager::define_class_with_options`
                    // defers this entire Pass 3 pass for `UserDefined`-loader
                    // classes (see `defer_loader_sensitive_pass3`), and
                    // `vm_util.rs::initialize_class_shared` does the same at
                    // link time. By the time this arm runs, the class being
                    // verified is NOT one of those deferred classes, so a
                    // mismatch here is a genuine violation, not a loader-name
                    // collision — reject it (JVMS §4.10.1.2 requires the
                    // verifier be conservative and reject what it cannot
                    // prove; see the RVERIF.3 note on `ClassHierarchy` in
                    // `vtype.rs` for the same principle applied to
                    // `is_assignable_to`).
                    if !value.is_assignable_to(&declared, hierarchy) {
                        return Err(verify_err(&format!(
                            "areturn: returned value {value:?} is not assignable to \
                             the method's declared return type {declared:?}"
                        )));
                    }
                }
                // Declared return type is a primitive (Int/Long/Float/
                // Double) — `areturn` is the wrong return instruction.
                Some(other) => {
                    return Err(verify_err(&format!(
                        "areturn: method's declared return type {other:?} is not a \
                         reference type"
                    )));
                }
                // `None` means the method is declared `void` — `areturn`
                // must not be used to return from a void method.
                None => {
                    return Err(verify_err(
                        "areturn: method is declared void; cannot return a value",
                    ));
                }
            }
            ok_no_fallthrough()
        }

        Instruction::Return => {
            // JVMS §4.10.1.9 (`return`): if the method being verified is
            // `<init>` and the receiver has not been initialized — i.e. no
            // `invokespecial` of `this.<init>` / `super.<init>` has replaced
            // `uninitializedThis` — the constructor may not return. Otherwise
            // a class could hand its caller a fully-typed reference to an
            // object whose superclass constructor never ran, defeating every
            // invariant a superclass constructor establishes (this is the
            // classic "uninitialized object escape").
            //
            // `<init>` of `java/lang/Object` is the one exception: it has no
            // superclass to chain to, so its `uninitializedThis` is never
            // replaced.
            if _method_name == "<init>"
                && current_class_name != "java/lang/Object"
                && frame.has_uninitialized_this()
            {
                return Err(verify_err(
                    "return: constructor returns while `this` is still uninitialized — \
                     no this()/super() constructor call was verified (JVMS §4.10.1.9)",
                ));
            }
            ok_no_fallthrough()
        }

        // =====================================================================
        // Field access — resolve type from constant pool
        // =====================================================================
        Instruction::Getstatic(index) => {
            let field_type = resolve_field_type(cp, *index)?;
            push_typed(frame, &field_type)?;
            ok_through()
        }

        Instruction::Putstatic(index) => {
            let field_type = resolve_field_type(cp, *index)?;
            pop_typed(frame, &field_type, hierarchy)?;
            ok_through()
        }

        Instruction::Getfield(index) => {
            let field_type = resolve_field_type(cp, *index)?;
            pop_reference(frame, hierarchy)?; // objectref
            push_typed(frame, &field_type)?;
            ok_through()
        }

        Instruction::Putfield(index) => {
            let field_type = resolve_field_type(cp, *index)?;
            pop_typed(frame, &field_type, hierarchy)?; // value
            pop_reference(frame, hierarchy)?; // objectref
            ok_through()
        }

        // =====================================================================
        // Method invocation
        // =====================================================================
        Instruction::Invokevirtual(index) => {
            let (_, method_desc) = resolve_method_name_and_type(cp, *index)?;
            // T1.3.7 — reject `Reference.get0` (and siblings) on a
            // receiver that is not a `java/lang/ref/Reference` or
            // subclass. The JVMS requires the objectref to be
            // assignable to the method's declaring class; we enforce
            // it for the `Reference.*0` surface specifically because
            // those natives crash the VM if invoked on the wrong
            // shape, and HotSpot's verifier rejects the same pattern.
            let owner_info = resolve_method_owner_name_and_type(cp, *index);
            // Pop arguments in reverse order
            let param_types = super::vtype::param_types_from_descriptor(&method_desc);
            for param in param_types.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            // Pop objectref
            let receiver = frame.pop()?;
            if !receiver.is_reference() {
                return Err(verify_err(&format!(
                    "invokevirtual: receiver must be a reference, found {receiver:?}"
                )));
            }
            if let Some((owner, method_name, _)) = owner_info {
                if owner == "java/lang/ref/Reference"
                    && (method_name == "get0"
                        || method_name == "refersTo0"
                        || method_name == "clear0")
                {
                    // Receiver must be Reference-shaped (Reference or subclass),
                    // or Null (runtime NPE), or Top (dead code).
                    let is_ok = match &receiver {
                        super::vtype::VType::Null | super::vtype::VType::Top => true,
                        super::vtype::VType::ObjectRef(name) => {
                            // `name` is `&Arc<str>`; deref to `&str` to
                            // compare against string literals and pass into
                            // the hierarchy trait method.
                            let n: &str = name;
                            n == "java/lang/ref/Reference"
                                || n == "java/lang/ref/WeakReference"
                                || n == "java/lang/ref/SoftReference"
                                || n == "java/lang/ref/PhantomReference"
                                || hierarchy.is_subclass(n, "java/lang/ref/Reference")
                        }
                        _ => false,
                    };
                    if !is_ok {
                        return Err(verify_err(&format!(
                            "invokevirtual Reference.{method_name}: receiver {receiver:?} \
                             is not a java/lang/ref/Reference subclass"
                        )));
                    }
                }
            }
            // Push return type
            if let Some(ret_type) = return_type_from_descriptor(&method_desc) {
                push_typed(frame, &ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokespecial(index) => {
            let (method_name, method_desc) = resolve_method_or_imethod_name_and_type(cp, *index)?;
            // Pop arguments in reverse order
            let param_types = super::vtype::param_types_from_descriptor(&method_desc);
            for param in param_types.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            // Pop objectref
            let receiver = pop_reference(frame, hierarchy)?;
            // JVMS 4.10.1.9: invokespecial on <init> initializes the receiver.
            // Replace all occurrences of the uninitialized type in both locals
            // and stack with the initialized ObjectRef(owner). Without this,
            // subsequent merges between the initialized path and the
            // uninitialized slot collapse to Top and break verification of
            // pre-Java-7 classes compiled without a StackMapTable (e.g.
            // picocli 4.x's `toCommandLine` helper).
            if method_name == "<init>" {
                // JVMS §4.10.1.9 (invokespecial <init>): the objectref MUST
                // be an *uninitialized* type — either `uninitializedThis` or
                // `uninitialized(offset)`. Calling `<init>` on an already
                // initialized reference (ObjectRef/ArrayRef), on `Null`, or on
                // a non-reference is a verification error: a sound verifier
                // must reject it (otherwise a class could re-run a constructor
                // on a fully-built object, or invoke `<init>` on a wrong type).
                //
                // After the call the spec replaces *all* occurrences of the
                // uninitialized type — in both locals and stack — with the
                // initialized class type:
                //   - For `uninitializedThis`, the initialized type is the
                //     current class (`this.<init>` chains to `super()`; `this`
                //     is still the current class, not the superclass owner).
                //     The declaring class of the invoked `<init>` must be the
                //     current class (this-delegating `this(...)`) or its direct
                //     superclass (`super(...)`).
                //   - For `uninitialized(offset)`, the initialized type is the
                //     method owner, which must match the class named by the
                //     `new` at `offset` (we resolve it from the method ref).
                match &receiver {
                    VType::UninitializedThis => {
                        // The invoked constructor's owner must be either the
                        // current class or its directly linked superclass.
                        // Use the linked direct-superclass edge rather than a
                        // generic ancestry lookup: the latter can resolve a
                        // different loader-visible copy and would also accept
                        // an invalid grandparent constructor invocation.
                        if let Some((owner, _, _)) = resolve_method_owner_name_and_type(cp, *index)
                        {
                            let ok = owner == current_class_name
                                || hierarchy.is_direct_superclass(current_class_name, &owner);
                            if !ok {
                                return Err(verify_err(&format!(
                                    "invokespecial <init>: uninitializedThis receiver requires \
                                     the constructor owner to be the current class \
                                     ({current_class_name}) or its superclass, found {owner}"
                                )));
                            }
                        }
                        let replacement = VType::ObjectRef(Arc::from(current_class_name));
                        replace_vtype_in_frame(frame, &receiver, &replacement);
                    }
                    VType::Uninitialized(_) => {
                        // The initialized type is the constructor's declaring
                        // class (which the bytecode's `new` site created).
                        let owner =
                            resolve_method_owner_name_and_type(cp, *index).map(|(o, _, _)| o);
                        let cls: Arc<str> = match owner {
                            Some(o) => Arc::from(o.as_str()),
                            None => {
                                return Err(verify_err(
                                    "invokespecial <init>: cannot resolve constructor owner class",
                                ));
                            }
                        };
                        let replacement = VType::ObjectRef(cls);
                        let to_replace = receiver.clone();
                        replace_vtype_in_frame(frame, &to_replace, &replacement);
                    }
                    // Already-initialized reference, Null, or any non-uninitialized
                    // type is not a legal receiver for `<init>` — reject.
                    other => {
                        return Err(verify_err(&format!(
                            "invokespecial <init>: receiver must be an uninitialized object \
                             (uninitializedThis or uninitialized(offset)), found {other:?}"
                        )));
                    }
                }
            }
            // Push return type
            if let Some(ret_type) = return_type_from_descriptor(&method_desc) {
                push_typed(frame, &ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokestatic(index) => {
            let (_, method_desc) = resolve_method_or_imethod_name_and_type(cp, *index)?;
            let param_types = super::vtype::param_types_from_descriptor(&method_desc);
            for param in param_types.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            if let Some(ret_type) = return_type_from_descriptor(&method_desc) {
                push_typed(frame, &ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokeinterface { index, .. } => {
            let (_, method_desc) = resolve_imethod_name_and_type(cp, *index)?;
            let param_types = super::vtype::param_types_from_descriptor(&method_desc);
            for param in param_types.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            pop_reference(frame, hierarchy)?; // objectref
            if let Some(ret_type) = return_type_from_descriptor(&method_desc) {
                push_typed(frame, &ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokedynamic(index) => {
            // Resolve the name and type from the InvokeDynamic CP entry
            let method_desc = resolve_invokedynamic_type(cp, *index)?;
            let param_types = super::vtype::param_types_from_descriptor(&method_desc);
            for param in param_types.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            if let Some(ret_type) = return_type_from_descriptor(&method_desc) {
                push_typed(frame, &ret_type)?;
            }
            ok_through()
        }

        // =====================================================================
        // Object creation and type checking
        // =====================================================================
        Instruction::New(index) => {
            // JVMS §4.9.1 / §6.5 new: the operand must be a `CONSTANT_Class`
            // entry naming a *class* type — not an array (`anewarray` /
            // `multianewarray` create those) and not an unrelated constant. The
            // previous arm ignored the index entirely, so `new #<any index>`
            // verified clean and the interpreter reached an entry of whatever
            // tag the class file happened to place there.
            let class_name = cp.get_class_name(*index).ok_or_else(|| {
                verify_err(&format!(
                    "new: constant pool index {index} is not a CONSTANT_Class entry \
                     with a resolvable name"
                ))
            })?;
            if class_name.starts_with('[') {
                return Err(verify_err(&format!(
                    "new: operand {class_name} is an array type; use anewarray / \
                     multianewarray (JVMS §6.5 new)"
                )));
            }
            if class_name.is_empty() {
                return Err(verify_err("new: operand names an empty class name"));
            }
            // Push Uninitialized(pc) — the object is uninitialized until <init> is called
            frame.push(VType::Uninitialized(pc as u16))?;
            ok_through()
        }

        Instruction::Newarray(_atype) => {
            frame.pop_expect(&VType::Int, hierarchy)?; // count
                                                       // Result is an array of the given primitive type
            let desc = match _atype {
                4 => "[Z",  // boolean
                5 => "[C",  // char
                6 => "[F",  // float
                7 => "[D",  // double
                8 => "[B",  // byte
                9 => "[S",  // short
                10 => "[I", // int
                11 => "[J", // long
                _ => return Err(verify_err(&format!("newarray: invalid atype {_atype}"))),
            };
            frame.push(VType::ArrayRef(Arc::from(desc)))?;
            ok_through()
        }

        Instruction::Anewarray(index) => {
            frame.pop_expect(&VType::Int, hierarchy)?; // count
            let class_name = cp
                .get_class_name(*index)
                .ok_or_else(|| verify_err(&format!("anewarray: invalid class index {index}")))?;
            let desc = if class_name.starts_with('[') {
                // Array of arrays
                format!("[{class_name}")
            } else {
                format!("[L{class_name};")
            };
            frame.push(VType::ArrayRef(Arc::from(desc.as_str())))?;
            ok_through()
        }

        Instruction::Multianewarray { index, dimensions } => {
            // JVMS §4.10.1.9 multianewarray: `dimensions` must be >= 1, and
            // it must not exceed the number of array dimensions of the
            // referenced type (the leading `[` bracket count of the
            // descriptor). HotSpot raises a VerifyError for both.
            if *dimensions == 0 {
                return Err(verify_err("multianewarray: dimensions must be >= 1"));
            }
            let class_name = cp.get_class_name_arc(*index).ok_or_else(|| {
                verify_err(&format!("multianewarray: invalid class index {index}"))
            })?;
            // The referenced type must be an array type with at least
            // `dimensions` leading brackets.
            let bracket_count = class_name.bytes().take_while(|&b| b == b'[').count();
            if bracket_count < *dimensions as usize {
                return Err(verify_err(&format!(
                    "multianewarray: dimensions {dimensions} exceeds array \
                     bracket count {bracket_count} of type {class_name}"
                )));
            }
            // Pop `dimensions` int values (counts), one per dimension.
            for _ in 0..*dimensions {
                frame.pop_expect(&VType::Int, hierarchy)?;
            }
            // The result is an array reference
            frame.push(VType::ArrayRef(class_name))?;
            ok_through()
        }

        Instruction::Arraylength => {
            pop_array_ref(frame, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Athrow => {
            // JVMS §4.10.1.6 / §6.5 athrow: the operand must be a
            // reference assignable to `java/lang/Throwable`. Throwing an
            // arbitrary non-Throwable reference is a type-safety
            // violation — the exception-dispatch machinery assumes a
            // Throwable shape.
            let value = pop_reference(frame, hierarchy)?;
            let throwable = VType::ObjectRef(Arc::from("java/lang/Throwable"));
            // `Null` athrow is legal bytecode: it raises a
            // NullPointerException at runtime, which is the spec'd
            // outcome — accept it. `UninitializedThis` / `Uninitialized`
            // are rejected by the assignability check below (an
            // uninitialized object is not assignable to Throwable).
            let ok = matches!(value, VType::Null) || value.is_assignable_to(&throwable, hierarchy);
            if !ok {
                return Err(verify_err(&format!(
                    "athrow: operand {value:?} is not assignable to java/lang/Throwable"
                )));
            }
            ok_no_fallthrough()
        }

        Instruction::Checkcast(index) => {
            pop_reference(frame, hierarchy)?;
            let class_name = cp
                .get_class_name_arc(*index)
                .ok_or_else(|| verify_err(&format!("checkcast: invalid class index {index}")))?;
            if class_name.starts_with('[') {
                frame.push(VType::ArrayRef(class_name))?;
            } else {
                frame.push(VType::ObjectRef(class_name))?;
            }
            ok_through()
        }

        Instruction::Instanceof(index) => {
            pop_reference(frame, hierarchy)?;
            // JVMS §6.5 instanceof: the operand must be a `CONSTANT_Class`
            // entry. Checked for the same reason as `new` / `checkcast` — the
            // index was previously ignored outright.
            if cp.get_class_name(*index).is_none() {
                return Err(verify_err(&format!(
                    "instanceof: constant pool index {index} is not a CONSTANT_Class \
                     entry with a resolvable name"
                )));
            }
            frame.push(VType::Int)?;
            ok_through()
        }

        // =====================================================================
        // Monitor — pop objectref
        // =====================================================================
        Instruction::Monitorenter | Instruction::Monitorexit => {
            pop_reference(frame, hierarchy)?;
            ok_through()
        }

        // =====================================================================
        // JSR / RET — legacy (pre-Java 7)
        // =====================================================================
        Instruction::Jsr(offset) => {
            // NEW-9: per JVMS §6.5.jsr, the value pushed is a
            // returnAddress whose value is the pc of the instruction
            // *immediately following* the jsr (pc+3). The prior code
            // pushed `ReturnAddress(target)` (the subroutine entry),
            // which is wrong: a later `ret N` would branch back to the
            // subroutine entry instead of to the call site.
            let target = branch_target(pc, *offset as i32);
            let next_pc = (pc + 3) as u16;
            frame.push(VType::ReturnAddress(next_pc))?;
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: vec![target],
            })
        }

        Instruction::JsrW(offset) => {
            // Same JVMS §6.5 contract. jsr_w is a 5-byte instruction
            // so the returnAddress is pc+5.
            let target = branch_target(pc, *offset);
            let next_pc = (pc + 5) as u16;
            frame.push(VType::ReturnAddress(next_pc))?;
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: vec![target],
            })
        }

        Instruction::Ret(index) => {
            // NEW-9: `ret N` jumps to the return address stored in local
            // slot N. The return address was pushed by an earlier `jsr`
            // and stored into the local by an `astore N` in the
            // subroutine prologue. The verifier reads the local's
            // current type-state — if it holds a `ReturnAddress(pc)`
            // the subroutine's exit flows to that pc. If it holds
            // anything else (or the slot is unset), the bytecode is
            // malformed: `ret` without a preceding `jsr` is invalid.
            //
            // This is an approximation of full JVMS §4.10.2.5 subroutine
            // verification — a rigorous implementation would also
            // unify the frame state across all jsr sites that target
            // the subroutine. For the worklist-based inference path we
            // use in pre-Java-7 classes, propagating the recorded
            // return pc as a branch target is sufficient to trace the
            // subroutine body back to the call site.
            let local = frame
                .local_load(*index as u16)
                .map_err(|_| verify_err(&format!("ret: local variable {index} out of range")))?;
            match local {
                VType::ReturnAddress(ret_pc) => Ok(InsnVerifyResult {
                    falls_through: false,
                    branch_targets: vec![*ret_pc],
                }),
                other => Err(verify_err(&format!(
                    "ret: local variable {index} must hold a returnAddress, \
                     got {other:?}"
                ))),
            }
        }

        // =====================================================================
        // Wide prefix — should have been decoded into wider variants
        // =====================================================================
        Instruction::Wide => {
            // Wide is a prefix handled during decoding; it should not appear standalone
            Err(verify_err("unexpected wide prefix in verification"))
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn ok_through() -> Result<InsnVerifyResult, LinkageError> {
    Ok(InsnVerifyResult {
        falls_through: true,
        branch_targets: vec![],
    })
}

fn ok_no_fallthrough() -> Result<InsnVerifyResult, LinkageError> {
    Ok(InsnVerifyResult {
        falls_through: false,
        branch_targets: vec![],
    })
}

fn verify_err(message: &str) -> LinkageError {
    LinkageError::VerifyError {
        class_name: String::new(),
        method_name: String::new(),
        message: message.to_string(),
    }
}

/// Prefix a `VerifyError` raised by a [`VerificationFrame`] helper with the
/// opcode that raised it, so the message names the rule *and* the instruction.
/// Non-`VerifyError` variants pass through untouched.
fn prefix_verify_err(opcode: &str, err: LinkageError) -> LinkageError {
    match err {
        LinkageError::VerifyError {
            class_name,
            method_name,
            message,
        } => LinkageError::VerifyError {
            class_name,
            method_name,
            message: format!("{opcode}: {message}"),
        },
        other => other,
    }
}

fn branch_target(pc: usize, offset: i32) -> u16 {
    // Use checked arithmetic to prevent overflow: truncating pc to i32
    // then to u16 could allow malformed bytecode to jump to arbitrary targets.
    let pc_i32: i32 = pc.try_into().unwrap_or(i32::MAX);
    let target = pc_i32.saturating_add(offset);
    if target < 0 || target > u16::MAX as i32 {
        // Out-of-range branch target — return 0 so the verifier rejects it
        // when it checks the target against the code length.
        0
    } else {
        target as u16
    }
}

/// Replace every occurrence of `old` in the frame's locals and stack with
/// `new`. Used to implement JVMS §4.10.1.9 rule for `invokespecial <init>`:
/// once the uninitialized receiver is initialized, every aliased copy of
/// the uninitialized type in the frame refers to the initialized object.
fn replace_vtype_in_frame(frame: &mut VerificationFrame, old: &VType, new: &VType) {
    // Only meaningful for uninitialized types; replacing a regular ObjectRef
    // is a no-op in normal flow but still safe.
    if !matches!(old, VType::Uninitialized(_) | VType::UninitializedThis) {
        return;
    }
    for slot in frame.locals.iter_mut() {
        if slot == old {
            *slot = new.clone();
        }
    }
    for slot in frame.stack.iter_mut() {
        if slot == old {
            *slot = new.clone();
        }
    }
}

/// Pop a reference type from the stack.
fn pop_reference(
    frame: &mut VerificationFrame,
    _hierarchy: &dyn ClassHierarchy,
) -> Result<VType, LinkageError> {
    let val = frame.pop()?;
    if !val.is_reference() {
        return Err(verify_err(&format!(
            "expected reference on stack, found {val:?}"
        )));
    }
    Ok(val)
}

/// Pop an array reference from the stack.
fn pop_array_ref(
    frame: &mut VerificationFrame,
    _hierarchy: &dyn ClassHierarchy,
) -> Result<VType, LinkageError> {
    let val = frame.pop()?;
    match &val {
        VType::ArrayRef(_) | VType::Null => Ok(val),
        _ => Err(verify_err(&format!(
            "expected array reference on stack, found {val:?}"
        ))),
    }
}

/// Push a VType onto the stack, handling category-2 types (push + Top).
/// True if `popped` is the upper (`Top`) half of a category-2 value whose
/// `Long`/`Double` base is now exposed at the top of `frame`'s stack.
///
/// The verifier represents `long`/`double` as two slots — a `Long`/`Double`
/// base followed by a `Top`. Stack-manipulation instructions (`pop`, `dup`,
/// `swap`, …) operate on slots and must never split such a pair. After
/// `popped` has been removed from the stack, this checks whether `popped`
/// was that upper half.
fn is_cat2_upper_half(frame: &VerificationFrame, popped: &VType) -> bool {
    *popped == VType::Top && matches!(frame.stack.last(), Some(VType::Long) | Some(VType::Double))
}

/// Pop exactly `n` slots off the operand stack, returning them in
/// bottom-to-top order (`result[n-1]` was the top of stack).
fn pop_n_slots(
    frame: &mut VerificationFrame,
    n: usize,
    op: &str,
) -> Result<Vec<VType>, LinkageError> {
    if frame.stack.len() < n {
        return Err(verify_err(&format!(
            "{op}: stack underflow (need {n} slots, have {})",
            frame.stack.len()
        )));
    }
    let mut slots = Vec::with_capacity(n);
    for _ in 0..n {
        slots.push(frame.pop()?);
    }
    slots.reverse();
    Ok(slots)
}

/// Verify a slice of popped slots (bottom-to-top order) does not split a
/// category-2 value across its lower boundary. A category-2 base
/// (`Long`/`Double`) must always be immediately followed by its `Top`
/// upper half; the first slot must not be a dangling `Top`.
fn check_no_split(slots: &[VType], op: &str) -> Result<(), LinkageError> {
    // The first (lowest) slot may not be a `Top`: that would mean its
    // `Long`/`Double` base is still on the stack below — i.e. the pair was
    // split.
    if slots.first() == Some(&VType::Top) {
        return Err(verify_err(&format!(
            "{op}: operand splits a category-2 value"
        )));
    }
    // A `Long`/`Double` base as the last (highest) slot would mean its
    // `Top` upper half is above it and was not included — also a split.
    if matches!(slots.last(), Some(VType::Long) | Some(VType::Double)) {
        return Err(verify_err(&format!(
            "{op}: operand splits a category-2 value"
        )));
    }
    Ok(())
}

fn push_typed(frame: &mut VerificationFrame, vtype: &VType) -> Result<(), LinkageError> {
    frame.push(vtype.clone())?;
    if vtype.is_category2() {
        frame.push(VType::Top)?;
    }
    Ok(())
}

/// Pop a VType from the stack, handling category-2 types (pop Top + pop value).
fn pop_typed(
    frame: &mut VerificationFrame,
    vtype: &VType,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    if vtype.is_category2() {
        frame.pop()?; // Top
    }
    frame.pop_expect(vtype, hierarchy)?;
    Ok(())
}

/// Verify ldc: push Int, Float, String, or Class.
fn verify_ldc(
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    index: u16,
) -> Result<InsnVerifyResult, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::Integer(_)) => {
            frame.push(VType::Int)?;
        }
        Some(ConstantPoolEntry::Float(_)) => {
            frame.push(VType::Float)?;
        }
        Some(ConstantPoolEntry::StringReference { .. }) => {
            frame.push(VType::ObjectRef(Arc::from("java/lang/String")))?;
        }
        Some(ConstantPoolEntry::ClassReference { .. }) => {
            frame.push(VType::ObjectRef(Arc::from("java/lang/Class")))?;
        }
        Some(ConstantPoolEntry::MethodHandle { .. }) => {
            frame.push(VType::ObjectRef(Arc::from("java/lang/invoke/MethodHandle")))?;
        }
        Some(ConstantPoolEntry::MethodType { .. }) => {
            frame.push(VType::ObjectRef(Arc::from("java/lang/invoke/MethodType")))?;
        }
        Some(ConstantPoolEntry::Dynamic {
            name_and_type_index,
            ..
        }) => {
            // Dynamic constant — resolve type from NameAndType.
            if let Some((_, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                let vtype = VType::from_field_descriptor(descriptor);
                // SPEC-COMPLIANCE FIX (MED): `ldc` (and `ldc_w`) load a
                // *category-1* value only — JVMS §6.5 ldc requires the
                // referenced entry to NOT be `long`/`double`. A Dynamic
                // constant whose declared type is category-2 must be loaded
                // with `ldc2_w`; accepting it under `ldc` would push the
                // `Long`/`Double` base WITHOUT its paired `Top` upper half,
                // corrupting the verifier's category-2 slot model (every
                // subsequent stack offset would be off by one). Reject it.
                if vtype.is_category2() {
                    return Err(verify_err(
                        "ldc: Dynamic constant has a category-2 type (long/double); \
                         ldc loads category-1 only — use ldc2_w",
                    ));
                }
                frame.push(vtype)?;
            } else {
                return Err(verify_err("ldc: invalid Dynamic constant pool entry"));
            }
        }
        _ => {
            return Err(verify_err(&format!(
                "ldc: invalid constant pool index {index}"
            )));
        }
    }
    ok_through()
}

/// Verify ldc2_w: push Long or Double (the category-2 forms).
fn verify_ldc2w(
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    index: u16,
) -> Result<InsnVerifyResult, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::Long(_)) => {
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
        }
        Some(ConstantPoolEntry::Double(_)) => {
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
        }
        Some(ConstantPoolEntry::Dynamic {
            name_and_type_index,
            ..
        }) => {
            // SPEC-COMPLIANCE FIX (MED): `ldc2_w` is the category-2 loader and
            // since Java 11 may reference a Dynamic constant whose resolved
            // type is `long`/`double` (JVMS §6.5 ldc2_w). Push the base plus
            // its paired `Top` upper half so the category-2 slot model stays
            // consistent. A category-1 Dynamic constant under `ldc2_w` is
            // ill-formed (it belongs under `ldc`) — reject it.
            let descriptor = cp.get_name_and_type(*name_and_type_index).map(|(_, d)| d);
            match descriptor {
                Some(d) => {
                    let vtype = VType::from_field_descriptor(d);
                    if !vtype.is_category2() {
                        return Err(verify_err(
                            "ldc2_w: Dynamic constant has a category-1 type; \
                             ldc2_w loads category-2 (long/double) only — use ldc",
                        ));
                    }
                    frame.push(vtype)?;
                    frame.push(VType::Top)?;
                }
                None => {
                    return Err(verify_err("ldc2_w: invalid Dynamic constant pool entry"));
                }
            }
        }
        _ => {
            return Err(verify_err(&format!(
                "ldc2_w: invalid constant pool index {index}"
            )));
        }
    }
    ok_through()
}

/// Resolve the field type descriptor from a FieldReference CP entry.
///
/// JVMS §4.4.2 / §4.4.6: a `Fieldref`'s `NameAndType` must carry a **field**
/// descriptor, and the referenced class must itself be a `CONSTANT_Class`.
/// Both are checked here.
///
/// SECURITY: rejecting a malformed descriptor is not pedantry.
/// [`VType::from_field_descriptor`] maps anything it does not recognise to
/// [`VType::Top`], and `Top` is the top of the assignability lattice — every
/// value is assignable to it. So a field whose descriptor is `"Q"` used to make
/// `putstatic` pop an arbitrary operand with no type check at all, and
/// `getstatic` push a `Top` that later merges could not constrain.
fn resolve_field_type(cp: &ConstantPool, index: u16) -> Result<VType, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) => {
            if cp.get_class_name(*class_index).is_none() {
                return Err(verify_err(&format!(
                    "field ref at index {index}: class_index {class_index} is not a \
                     CONSTANT_Class entry with a resolvable name"
                )));
            }
            let Some((name, descriptor)) = cp.get_name_and_type(*name_and_type_index) else {
                return Err(verify_err(&format!(
                    "field ref: invalid NameAndType at index {name_and_type_index}"
                )));
            };
            if name.is_empty() {
                return Err(verify_err(&format!(
                    "field ref at index {index}: empty field name"
                )));
            }
            if !super::vtype::is_valid_field_descriptor(descriptor) {
                return Err(verify_err(&format!(
                    "field ref at index {index}: {descriptor:?} is not a well-formed \
                     field descriptor (JVMS §4.3.2)"
                )));
            }
            Ok(VType::from_field_descriptor(descriptor))
        }
        _ => Err(verify_err(&format!(
            "expected FieldReference at constant pool index {index}"
        ))),
    }
}

/// Shared `NameAndType` cross-check for the `invoke*` family (JVMS §4.4.6).
///
/// The name must be non-empty and the descriptor must be a well-formed
/// **method** descriptor. A malformed descriptor would otherwise be parsed by
/// [`super::vtype::param_types_from_descriptor`] into a *short* parameter list,
/// so the verifier would pop fewer operands than the interpreter does at run
/// time — an operand-stack desynchronisation between the two.
fn checked_name_and_type(
    cp: &ConstantPool,
    name_and_type_index: u16,
    what: &str,
) -> Result<(String, String), LinkageError> {
    let Some((name, descriptor)) = cp.get_name_and_type(name_and_type_index) else {
        return Err(verify_err(&format!(
            "{what}: invalid NameAndType at index {name_and_type_index}"
        )));
    };
    if name.is_empty() {
        return Err(verify_err(&format!("{what}: empty method name")));
    }
    if !super::vtype::is_valid_method_descriptor(descriptor) {
        return Err(verify_err(&format!(
            "{what}: {descriptor:?} is not a well-formed method descriptor \
             (JVMS §4.3.3)"
        )));
    }
    Ok((name.to_string(), descriptor.to_string()))
}

/// Resolve method name and descriptor from a MethodReference CP entry.
fn resolve_method_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Result<(String, String), LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::MethodReference {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "method ref"),
        _ => Err(verify_err(&format!(
            "expected MethodReference at constant pool index {index}"
        ))),
    }
}

/// T1.3.7 — Resolve (owner_class, method_name, descriptor) from a
/// MethodReference so the verifier can cross-check the receiver type
/// at Invokevirtual against the method-owning class.
///
/// Used to reject calls like `Reference.get0` invoked on a receiver
/// that isn't `java/lang/ref/Reference`-shaped — a class of verifier
/// rejection HotSpot performs at link time.
fn resolve_method_owner_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Option<(String, String, String)> {
    match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        } => {
            let owner = cp.get_class_name(*class_index)?.to_string();
            let (name, descriptor) = cp.get_name_and_type(*name_and_type_index)?;
            Some((owner, name.to_string(), descriptor.to_string()))
        }
        _ => None,
    }
}

/// Resolve method name and descriptor from either a MethodReference or
/// InterfaceMethodReference CP entry.  Per JVMS 4.9.1, `invokestatic` and
/// `invokespecial` may reference either kind since Java 8 (interface static
/// and default methods).
fn resolve_method_or_imethod_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Result<(String, String), LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::MethodReference {
            name_and_type_index,
            ..
        })
        | Some(ConstantPoolEntry::InterfaceMethodReference {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "method ref"),
        _ => Err(verify_err(&format!(
            "expected MethodReference or InterfaceMethodReference at constant pool index {index}"
        ))),
    }
}

/// Resolve method name and descriptor from an InterfaceMethodReference CP entry.
fn resolve_imethod_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Result<(String, String), LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::InterfaceMethodReference {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "interface method ref"),
        _ => Err(verify_err(&format!(
            "expected InterfaceMethodReference at constant pool index {index}"
        ))),
    }
}

/// Resolve the method type from an InvokeDynamic CP entry.
fn resolve_invokedynamic_type(cp: &ConstantPool, index: u16) -> Result<String, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::InvokeDynamic {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "invokedynamic").map(|(_, d)| d),
        _ => Err(verify_err(&format!(
            "expected InvokeDynamic at constant pool index {index}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::verify_frame::VerificationFrame;
    use super::super::vtype::ClassHierarchy;
    use super::*;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    struct MockHierarchy;

    impl ClassHierarchy for MockHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            if child == parent || parent == "java/lang/Object" {
                return true;
            }
            // Model the JDK exception hierarchy so the `athrow` operand
            // check (operand must be assignable to java/lang/Throwable)
            // can be exercised by the tests.
            matches!(
                (child, parent),
                ("java/lang/Exception", "java/lang/Throwable")
                    | ("java/lang/RuntimeException", "java/lang/Throwable")
                    | ("java/lang/RuntimeException", "java/lang/Exception")
                    | ("java/lang/Error", "java/lang/Throwable")
            )
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    fn simple_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                        // 0
            ConstantPoolEntry::Integer(42),                      // 1
            ConstantPoolEntry::Float(3.125),                     // 2
            ConstantPoolEntry::Long(100),                        // 3
            ConstantPoolEntry::Tombstone,                        // 4 (second slot of long)
            ConstantPoolEntry::Double(2.75),                     // 5
            ConstantPoolEntry::Tombstone,                        // 6 (second slot of double)
            ConstantPoolEntry::Utf8("java/lang/Object".into()),  // 7
            ConstantPoolEntry::ClassReference { name_index: 7 }, // 8
            ConstantPoolEntry::Utf8("value".into()),             // 9
            ConstantPoolEntry::Utf8("I".into()),                 // 10
            ConstantPoolEntry::NameAndType {
                name_index: 9,
                descriptor_index: 10,
            }, // 11
            ConstantPoolEntry::FieldReference {
                class_index: 8,
                name_and_type_index: 11,
            }, // 12
            ConstantPoolEntry::Utf8("toString".into()),          // 13
            ConstantPoolEntry::Utf8("()Ljava/lang/String;".into()), // 14
            ConstantPoolEntry::NameAndType {
                name_index: 13,
                descriptor_index: 14,
            }, // 15
            ConstantPoolEntry::MethodReference {
                class_index: 8,
                name_and_type_index: 15,
            }, // 16
            ConstantPoolEntry::StringReference { string_index: 7 }, // 17
        ])
    }

    fn make_frame(max_locals: u16, max_stack: u16) -> VerificationFrame {
        VerificationFrame::initial_frame("Test", "test", "()V", true, max_locals, max_stack)
    }

    #[test]
    fn verify_iconst_pushes_int() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let result = verify_instruction(
            &Instruction::Iconst0,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(result.falls_through);
        assert!(result.branch_targets.is_empty());
        assert_eq!(frame.stack_depth(), 1);
        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_iadd_type_effect() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        frame.push(VType::Int).unwrap();
        frame.push(VType::Int).unwrap();

        verify_instruction(
            &Instruction::Iadd,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.stack_depth(), 1);
        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_iadd_wrong_types_fails() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        frame.push(VType::Float).unwrap();
        frame.push(VType::Int).unwrap();

        // Second pop expects Int but finds Float
        let result = verify_instruction(
            &Instruction::Iadd,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(result.is_err());
    }

    #[test]
    fn verify_ldc_integer() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc(1),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_ldc_string() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc(17),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(
            frame.pop().unwrap(),
            VType::ObjectRef(Arc::from("java/lang/String"))
        );
    }

    /// Constant pool with two Dynamic constants for the ldc/ldc2_w
    /// category tests: index #3 is a category-1 Dynamic (`I`), index #6 is
    /// a category-2 Dynamic (`J`).
    fn dynamic_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,        // 0
            ConstantPoolEntry::Utf8("d".into()), // 1
            ConstantPoolEntry::Utf8("I".into()), // 2 (category-1 descriptor)
            ConstantPoolEntry::NameAndType {
                name_index: 1,
                descriptor_index: 2,
            }, // 3 (NameAndType for cat-1)
            ConstantPoolEntry::Dynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 3,
            }, // 4 (category-1 Dynamic)
            ConstantPoolEntry::Utf8("J".into()), // 5 (category-2 descriptor)
            ConstantPoolEntry::NameAndType {
                name_index: 1,
                descriptor_index: 5,
            }, // 6 (NameAndType for cat-2)
            ConstantPoolEntry::Dynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 6,
            }, // 7 (category-2 Dynamic)
        ])
    }

    #[test]
    fn ldc_of_category2_dynamic_is_rejected() {
        // SPEC-COMPLIANCE FIX (MED): `ldc` of a Dynamic constant whose type
        // is category-2 (long/double) must be rejected — pushing it without
        // the paired `Top` would corrupt the verifier stack model.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let res = verify_instruction(
            &Instruction::Ldc(7), // category-2 Dynamic
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            res.is_err(),
            "ldc of a category-2 Dynamic constant must be rejected, got {res:?}"
        );
        // The frame must be untouched on rejection (no half-pushed cat-2).
        assert_eq!(frame.stack_depth(), 0);
    }

    #[test]
    fn ldc_of_category1_dynamic_is_accepted() {
        // A category-1 Dynamic constant is the legal `ldc` case: pushes a
        // single slot, no Top.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc(4), // category-1 Dynamic (`I`)
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .expect("ldc of a category-1 Dynamic must verify");
        assert_eq!(frame.stack_depth(), 1);
        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn ldc2w_of_category2_dynamic_pushes_top_slot() {
        // `ldc2_w` is the correct loader for a category-2 Dynamic constant:
        // it must push the Long/Double base plus its paired Top upper half.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc2W(7), // category-2 Dynamic (`J`)
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .expect("ldc2_w of a category-2 Dynamic must verify");
        assert_eq!(frame.stack_depth(), 2);
        assert_eq!(frame.pop().unwrap(), VType::Top);
        assert_eq!(frame.pop().unwrap(), VType::Long);
    }

    #[test]
    fn ldc2w_of_category1_dynamic_is_rejected() {
        // A category-1 Dynamic under `ldc2_w` is ill-formed (belongs under
        // `ldc`) and must be rejected.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let res = verify_instruction(
            &Instruction::Ldc2W(4), // category-1 Dynamic (`I`)
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            res.is_err(),
            "ldc2_w of a category-1 Dynamic constant must be rejected, got {res:?}"
        );
        assert_eq!(frame.stack_depth(), 0);
    }

    #[test]
    fn verify_getstatic() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        // getstatic index=12 → field "value" of type "I"
        verify_instruction(
            &Instruction::Getstatic(12),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_goto_no_fallthrough() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let result = verify_instruction(
            &Instruction::Goto(10),
            5,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(!result.falls_through);
        assert_eq!(result.branch_targets, vec![15]);
    }

    #[test]
    fn verify_ifeq_branch() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Int).unwrap();

        let result = verify_instruction(
            &Instruction::Ifeq(20),
            5,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(result.falls_through);
        assert_eq!(result.branch_targets, vec![25]);
        assert_eq!(frame.stack_depth(), 0);
    }

    #[test]
    fn verify_return_no_fallthrough() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let result = verify_instruction(
            &Instruction::Return,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(!result.falls_through);
    }

    #[test]
    fn verify_new_pushes_uninitialized() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        // new at PC=10, class index=8 (Object)
        verify_instruction(
            &Instruction::New(8),
            10,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Uninitialized(10));
    }

    #[test]
    fn verify_aconst_null_pushes_null() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::AconstNull,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Null);
    }

    #[test]
    fn verify_dup() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Int).unwrap();

        verify_instruction(
            &Instruction::Dup,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.stack_depth(), 2);
    }

    #[test]
    fn verify_athrow_no_fallthrough() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/Exception")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Athrow,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(!result.falls_through);
    }

    // --- RVERIF.3 soundness regressions -----------------------------------

    #[test]
    fn athrow_rejects_non_throwable() {
        // A reference that is not assignable to java/lang/Throwable must
        // be rejected by athrow (JVMS §6.5 athrow).
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/String")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Athrow,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(result.is_err(), "athrow of a non-Throwable must fail");
    }

    #[test]
    fn athrow_accepts_null() {
        // `athrow null` is legal bytecode (raises NPE at runtime).
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();

        verify_instruction(
            &Instruction::Athrow,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();
    }

    #[test]
    fn areturn_rejects_unassignable_return() {
        // Method declared to return java/lang/String must reject an
        // areturn of an unrelated reference type (JVMS §6.5 areturn).
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/Thread")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Areturn,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()Ljava/lang/String;",
            &h,
        );
        assert!(
            result.is_err(),
            "areturn of a type not assignable to the declared return type must fail"
        );
    }

    #[test]
    fn areturn_accepts_assignable_return() {
        // Returning the declared type itself succeeds.
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/String")))
            .unwrap();

        verify_instruction(
            &Instruction::Areturn,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()Ljava/lang/String;",
            &h,
        )
        .unwrap();
    }

    #[test]
    fn areturn_rejects_void_method() {
        // areturn must not appear in a method declared void.
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();

        let result = verify_instruction(
            &Instruction::Areturn,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(result.is_err(), "areturn in a void method must fail");
    }

    /// Build a constant pool whose entry #6 is a `MethodReference` to
    /// `java/lang/Object.<init>()V`, for `invokespecial <init>` tests.
    fn init_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                        // 0
            ConstantPoolEntry::Utf8("java/lang/Object".into()),  // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("<init>".into()),            // 3
            ConstantPoolEntry::Utf8("()V".into()),               // 4
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
        ])
    }

    #[test]
    fn invokespecial_init_on_initialized_ref_rejected() {
        // JVMS §4.10.1.9: `<init>` may only be invoked on an *uninitialized*
        // receiver. An already-initialized ObjectRef must be rejected.
        let cp = init_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/Object")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            result.is_err(),
            "invokespecial <init> on an initialized ObjectRef must fail verification"
        );
    }

    #[test]
    fn invokespecial_init_on_null_rejected() {
        // `Null` is a reference but not an uninitialized object — reject.
        let cp = init_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();

        let result = verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            result.is_err(),
            "invokespecial <init> on Null must fail verification"
        );
    }

    #[test]
    fn invokespecial_init_on_uninitialized_initializes_slot() {
        // A `Uninitialized(offset)` receiver is legal; after the call every
        // aliased copy of that uninitialized type becomes the owner ObjectRef.
        let cp = init_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        // local 0 aliases the same uninitialized object (as if `dup`'d into it)
        frame.locals[0] = VType::Uninitialized(0);
        frame.push(VType::Uninitialized(0)).unwrap();

        verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .expect("invokespecial <init> on an uninitialized receiver must verify");

        // The receiver was popped; the aliased local must now be initialized.
        assert_eq!(
            frame.locals[0],
            VType::ObjectRef(Arc::from("java/lang/Object")),
            "uninitialized slot must be replaced with the initialized owner type"
        );
    }

    #[test]
    fn invokespecial_init_on_uninitialized_this_accepts_direct_superclass() {
        struct DirectSuperclassHierarchy;

        impl ClassHierarchy for DirectSuperclassHierarchy {
            fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
                false
            }

            fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
                child == "Example" && parent == "java/lang/Object"
            }

            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }

            fn is_interface(&self, _name: &str) -> bool {
                false
            }
        }

        let cp = init_cp();
        let h = DirectSuperclassHierarchy;
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        frame.push(VType::UninitializedThis).unwrap();

        verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect("uninitializedThis must be allowed to invoke its direct superclass constructor");

        assert_eq!(frame.locals[0], VType::ObjectRef(Arc::from("Example")));
    }

    #[test]
    fn invokespecial_init_on_uninitialized_this_rejects_indirect_superclass() {
        struct IndirectSuperclassHierarchy;

        impl ClassHierarchy for IndirectSuperclassHierarchy {
            fn is_subclass(&self, child: &str, parent: &str) -> bool {
                child == "Example" && parent == "java/lang/Object"
            }

            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }

            fn is_interface(&self, _name: &str) -> bool {
                false
            }
        }

        let cp = init_cp();
        let h = IndirectSuperclassHierarchy;
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        frame.push(VType::UninitializedThis).unwrap();

        let error = verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect_err("uninitializedThis must not invoke an indirect superclass constructor");

        assert!(error
            .to_string()
            .contains("uninitializedThis receiver requires the constructor owner"));
    }

    // =====================================================================
    // Constant-pool cross-checks (JVMS §4.9.1)
    // =====================================================================

    fn run(
        insn: &Instruction,
        frame: &mut VerificationFrame,
        cp: &ConstantPool,
        class: &str,
        method: &str,
        descriptor: &str,
    ) -> Result<InsnVerifyResult, LinkageError> {
        verify_instruction(
            insn,
            0,
            frame,
            cp,
            class,
            method,
            descriptor,
            &MockHierarchy,
        )
    }

    #[test]
    fn new_with_a_valid_class_entry_is_accepted() {
        let cp = init_cp(); // index 2 is a ClassReference → java/lang/Object
        let mut frame = make_frame(1, 4);
        run(&Instruction::New(2), &mut frame, &cp, "Test", "m", "()V")
            .expect("`new java/lang/Object` is well-formed");
        assert_eq!(frame.stack, vec![VType::Uninitialized(0)]);
    }

    #[test]
    fn new_with_a_non_class_cp_entry_rejected() {
        // Index 4 is a Utf8, not a CONSTANT_Class. The operand used to be
        // ignored entirely, so `new #4` verified clean.
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        let err = run(&Instruction::New(4), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("`new` must reject a non-Class operand");
        assert!(err.to_string().contains("CONSTANT_Class"), "{err}");
    }

    #[test]
    fn new_with_an_out_of_range_cp_index_rejected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        assert!(run(&Instruction::New(9999), &mut frame, &cp, "Test", "m", "()V").is_err());
    }

    #[test]
    fn new_of_an_array_type_rejected() {
        // JVMS §6.5 new: array types are created by anewarray / multianewarray.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("[Ljava/lang/Object;".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 },   // 2
        ]);
        let mut frame = make_frame(1, 4);
        let err = run(&Instruction::New(2), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("`new` of an array type must be rejected");
        assert!(err.to_string().contains("array type"), "{err}");
    }

    #[test]
    fn instanceof_with_a_non_class_cp_entry_rejected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();
        assert!(run(
            &Instruction::Instanceof(4),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V"
        )
        .is_err());
    }

    #[test]
    fn instanceof_with_a_valid_class_entry_accepted() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();
        run(
            &Instruction::Instanceof(2),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect("`instanceof java/lang/Object` is well-formed");
        assert_eq!(frame.stack, vec![VType::Int]);
    }

    #[test]
    fn getstatic_with_a_malformed_field_descriptor_rejected() {
        // A descriptor `VType::from_field_descriptor` cannot parse becomes
        // `Top`, and everything is assignable to `Top` — so a malformed
        // descriptor used to disable the type check on this operand entirely.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Test".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("f".into()),    // 3
            ConstantPoolEntry::Utf8("Ljava/lang/String".into()), // 4 — no ';'
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::FieldReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
        ]);
        let mut frame = make_frame(1, 4);
        let err = run(
            &Instruction::Getstatic(6),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect_err("a malformed field descriptor must be a VerifyError");
        assert!(err.to_string().contains("field descriptor"), "{err}");
    }

    #[test]
    fn getstatic_with_a_well_formed_field_descriptor_accepted() {
        let cp = simple_cp(); // index 12 is Test.value : I
        let mut frame = make_frame(1, 4);
        run(
            &Instruction::Getstatic(12),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect("a well-formed field ref must verify");
        assert_eq!(frame.stack, vec![VType::Int]);
    }

    #[test]
    fn invokestatic_with_a_malformed_method_descriptor_rejected() {
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Test".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("m".into()),    // 3
            ConstantPoolEntry::Utf8("(Ljava/lang/String)V".into()), // 4 — no ';'
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
        ]);
        let mut frame = make_frame(1, 4);
        let err = run(
            &Instruction::Invokestatic(6),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect_err("a malformed method descriptor must be a VerifyError");
        assert!(err.to_string().contains("method descriptor"), "{err}");
    }

    // =====================================================================
    // Category-2 local slot pairing (JVMS §4.10.1.6)
    // =====================================================================

    #[test]
    fn lstore_then_lload_round_trips() {
        let cp = simple_cp();
        let mut frame = make_frame(4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V").unwrap();
        run(&Instruction::Lload(1), &mut frame, &cp, "Test", "m", "()V")
            .expect("an intact long pair must load back");
        assert_eq!(frame.stack, vec![VType::Long, VType::Top]);
    }

    #[test]
    fn splitting_a_long_pair_then_loading_it_rejected() {
        // `lstore_1; istore_2; lload_1` — the `istore_2` overwrote the long's
        // upper half, so `lload_1` must not verify.
        let cp = simple_cp();
        let mut frame = make_frame(4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V").unwrap();
        frame.push(VType::Int).unwrap();
        run(&Instruction::Istore(2), &mut frame, &cp, "Test", "m", "()V").unwrap();
        let err = run(&Instruction::Lload(1), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("a split category-2 pair must not load back as a long");
        assert!(err.to_string().contains("lload"), "{err}");
    }

    #[test]
    fn lstore_past_max_locals_rejected() {
        // `max_locals = 2` leaves slots 0 and 1; a long at slot 1 needs slot 2.
        let cp = simple_cp();
        let mut frame = make_frame(2, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        let err = run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("a long store must fit entirely inside max_locals");
        assert!(err.to_string().contains("lstore"), "{err}");
    }

    #[test]
    fn wide_lstore_at_u16_max_does_not_panic() {
        // REGRESSION: `local_store(*index + 1, ..)` overflowed for a
        // `wide lstore 65535` — a debug panic on attacker-supplied bytecode.
        let cp = simple_cp();
        let mut frame = make_frame(4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        assert!(run(
            &Instruction::Lstore(u16::MAX),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V"
        )
        .is_err());
    }

    // =====================================================================
    // `<init>` must run before a constructor returns (JVMS §4.10.1.9)
    // =====================================================================

    #[test]
    fn constructor_return_before_super_init_rejected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        let err = run(
            &Instruction::Return,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
        )
        .expect_err("a constructor may not return with `this` uninitialized");
        assert!(err.to_string().contains("uninitialized"), "{err}");
    }

    #[test]
    fn constructor_return_after_super_init_accepted() {
        struct DirectSuper;
        impl ClassHierarchy for DirectSuper {
            fn is_subclass(&self, child: &str, parent: &str) -> bool {
                child == parent || parent == "java/lang/Object"
            }
            fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
                child == "Example" && parent == "java/lang/Object"
            }
            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }
            fn is_interface(&self, _name: &str) -> bool {
                false
            }
        }

        let cp = init_cp();
        let h = DirectSuper;
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        frame.push(VType::UninitializedThis).unwrap();
        verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect("super() call");
        verify_instruction(
            &Instruction::Return,
            4,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect("returning after super() is legal");
    }

    #[test]
    fn object_constructor_may_return_uninitialized() {
        // `java/lang/Object.<init>` has no superclass to chain to, so its
        // `uninitializedThis` is never replaced.
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        run(
            &Instruction::Return,
            &mut frame,
            &cp,
            "java/lang/Object",
            "<init>",
            "()V",
        )
        .expect("Object.<init> must be allowed to return");
    }

    #[test]
    fn ordinary_method_return_unaffected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        run(&Instruction::Return, &mut frame, &cp, "Example", "m", "()V")
            .expect("an ordinary void return is unaffected");
    }
}
