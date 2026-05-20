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
pub fn verify_instruction(
    insn: &Instruction,
    pc: usize,
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    _class_name: &str,
    _method_name: &str,
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
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Long, hierarchy) {
                return Err(verify_err(&format!(
                    "lload: local {index} is {local:?}, expected Long"
                )));
            }
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
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Double, hierarchy) {
                return Err(verify_err(&format!(
                    "dload: local {index} is {local:?}, expected Double"
                )));
            }
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
            frame.local_store(*index, VType::Long)?;
            frame.local_store(*index + 1, VType::Top)?;
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
            frame.local_store(*index, VType::Double)?;
            frame.local_store(*index + 1, VType::Top)?;
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
            if val1 == VType::Top
                || val2 == VType::Top
                || is_cat2_upper_half(frame, &val2)
            {
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

        Instruction::Tableswitch {
            default,
            low: _,
            high: _,
            offsets,
        } => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let mut targets: Vec<u16> = offsets.iter().map(|off| branch_target(pc, *off)).collect();
            targets.push(branch_target(pc, *default));
            // Deduplicate
            targets.sort();
            targets.dedup();
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: targets,
            })
        }

        Instruction::Lookupswitch { default, pairs } => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let mut targets: Vec<u16> = pairs
                .iter()
                .map(|(_, off)| branch_target(pc, *off))
                .collect();
            targets.push(branch_target(pc, *default));
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
            pop_reference(frame, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Return => ok_no_fallthrough(),

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
            let (method_name, method_desc) =
                resolve_method_or_imethod_name_and_type(cp, *index)?;
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
                // For UninitializedThis, replace with the current class
                // (this.<init> chained to super(); `this` is still of the
                // current class, not the superclass being invoked).
                // For Uninitialized(n), replace with the method owner
                // (which matches the `new` site's class).
                let replacement_class: Option<Arc<str>> = match &receiver {
                    VType::UninitializedThis => Some(Arc::from(current_class_name)),
                    VType::Uninitialized(_) => resolve_method_owner_name_and_type(cp, *index)
                        .map(|(o, _, _)| Arc::from(o.as_str())),
                    _ => None,
                };
                if let Some(cls) = replacement_class {
                    let replacement = VType::ObjectRef(cls);
                    let to_replace = receiver.clone();
                    replace_vtype_in_frame(frame, &to_replace, &replacement);
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
        Instruction::New(_index) => {
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
            // Pop `dimensions` int values
            for _ in 0..*dimensions {
                frame.pop_expect(&VType::Int, hierarchy)?;
            }
            let class_name = cp.get_class_name_arc(*index).ok_or_else(|| {
                verify_err(&format!("multianewarray: invalid class index {index}"))
            })?;
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
            // Pop the throwable reference
            pop_reference(frame, hierarchy)?;
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

        Instruction::Instanceof(_index) => {
            pop_reference(frame, hierarchy)?;
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
            let local = frame.local_load(*index as u16).map_err(|_| {
                verify_err(&format!(
                    "ret: local variable {index} out of range"
                ))
            })?;
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
    *popped == VType::Top
        && matches!(
            frame.stack.last(),
            Some(VType::Long) | Some(VType::Double)
        )
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
            // Dynamic constant — resolve type from NameAndType
            if let Some((_, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                let vtype = VType::from_field_descriptor(descriptor);
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

/// Verify ldc2_w: push Long or Double.
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
        _ => {
            return Err(verify_err(&format!(
                "ldc2_w: invalid constant pool index {index}"
            )));
        }
    }
    ok_through()
}

/// Resolve the field type descriptor from a FieldReference CP entry.
fn resolve_field_type(cp: &ConstantPool, index: u16) -> Result<VType, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::FieldReference {
            name_and_type_index,
            ..
        }) => {
            if let Some((_, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                Ok(VType::from_field_descriptor(descriptor))
            } else {
                Err(verify_err(&format!(
                    "field ref: invalid NameAndType at index {name_and_type_index}"
                )))
            }
        }
        _ => Err(verify_err(&format!(
            "expected FieldReference at constant pool index {index}"
        ))),
    }
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
        }) => {
            if let Some((name, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                Ok((name.to_string(), descriptor.to_string()))
            } else {
                Err(verify_err(&format!(
                    "method ref: invalid NameAndType at index {name_and_type_index}"
                )))
            }
        }
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
        }) => {
            if let Some((name, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                Ok((name.to_string(), descriptor.to_string()))
            } else {
                Err(verify_err(&format!(
                    "method ref: invalid NameAndType at index {name_and_type_index}"
                )))
            }
        }
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
        }) => {
            if let Some((name, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                Ok((name.to_string(), descriptor.to_string()))
            } else {
                Err(verify_err(&format!(
                    "interface method ref: invalid NameAndType at index {name_and_type_index}"
                )))
            }
        }
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
        }) => {
            if let Some((_, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                Ok(descriptor.to_string())
            } else {
                Err(verify_err(&format!(
                    "invokedynamic: invalid NameAndType at index {name_and_type_index}"
                )))
            }
        }
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
            child == parent || parent == "java/lang/Object"
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
            ConstantPoolEntry::Tombstone,                            // 0
            ConstantPoolEntry::Integer(42),                          // 1
            ConstantPoolEntry::Float(3.125),                         // 2
            ConstantPoolEntry::Long(100),                            // 3
            ConstantPoolEntry::Tombstone,                            // 4 (second slot of long)
            ConstantPoolEntry::Double(2.75),                         // 5
            ConstantPoolEntry::Tombstone,                            // 6 (second slot of double)
            ConstantPoolEntry::Utf8("java/lang/Object".into()), // 7
            ConstantPoolEntry::ClassReference { name_index: 7 },     // 8
            ConstantPoolEntry::Utf8("value".into()),            // 9
            ConstantPoolEntry::Utf8("I".into()),                // 10
            ConstantPoolEntry::NameAndType {
                name_index: 9,
                descriptor_index: 10,
            }, // 11
            ConstantPoolEntry::FieldReference {
                class_index: 8,
                name_and_type_index: 11,
            }, // 12
            ConstantPoolEntry::Utf8("toString".into()),         // 13
            ConstantPoolEntry::Utf8("()Ljava/lang/String;".into()), // 14
            ConstantPoolEntry::NameAndType {
                name_index: 13,
                descriptor_index: 14,
            }, // 15
            ConstantPoolEntry::MethodReference {
                class_index: 8,
                name_and_type_index: 15,
            }, // 16
            ConstantPoolEntry::StringReference { string_index: 7 },  // 17
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

        verify_instruction(&Instruction::Iadd, 0, &mut frame, &cp, "Test", "test", &h).unwrap();

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
        let result = verify_instruction(&Instruction::Iadd, 0, &mut frame, &cp, "Test", "test", &h);
        assert!(result.is_err());
    }

    #[test]
    fn verify_ldc_integer() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(&Instruction::Ldc(1), 0, &mut frame, &cp, "Test", "test", &h).unwrap();

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
            &h,
        )
        .unwrap();

        assert_eq!(
            frame.pop().unwrap(),
            VType::ObjectRef(Arc::from("java/lang/String"))
        );
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

        let result =
            verify_instruction(&Instruction::Return, 0, &mut frame, &cp, "Test", "test", &h)
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

        verify_instruction(&Instruction::Dup, 0, &mut frame, &cp, "Test", "test", &h).unwrap();

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

        let result =
            verify_instruction(&Instruction::Athrow, 0, &mut frame, &cp, "Test", "test", &h)
                .unwrap();

        assert!(!result.falls_through);
    }
}
