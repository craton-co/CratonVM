// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot's `VerifyError` text for class files below major 50.
//!
//! Interpreter round i5 wave 102, lane L7. HotSpot verifies such a class with
//! libverify's type-inferencing verifier (`check_code.c`, reached through
//! `Verifier::inference_verify`), whose `getMessage()` is
//!
//! ```text
//! (class: p/C, method: run signature: ()V) Expecting to find integer on stack
//! ```
//!
//! (`print_CCerror_info` then the `CCerror` format). A major-50 class that
//! fails the split verifier fails over to this verifier, but when both fail
//! HotSpot reports the SPLIT verifier's message (`Verifier::verify`), so only
//! majors below 50 use these texts ([`uses_old_texts`]).
//!
//! The reason is recomputed from the frame the failing instruction started
//! from, by replaying `check_code.c`'s checks in its order:
//! `check_register_values` (loads), `check_flags` (`return` in `<init>`),
//! `pop_stack` (the operand "shape" table `opcode_in_out`, then the
//! assignability checks that follow it). Where the replay finds nothing this
//! verifier's own refusal can be matched to, `None` keeps our text.

use std::sync::Arc;

use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::instruction::Instruction;

use super::verify_frame::{HsType, VerificationFrame, VerifyFault};
use super::vtype::VType;

/// Does a class file of `major` get libverify's texts?
#[inline]
pub fn uses_old_texts(major: u16) -> bool {
    major < 50
}

/// `print_CCerror_info` + the reason: the whole `getMessage()`.
pub fn message(class_name: &str, method_name: &str, descriptor: &str, reason: &str) -> String {
    format!("(class: {class_name}, method: {method_name} signature: {descriptor}) {reason}")
}

/// Is `message` one of [`message`]'s (so `linkage_throwable` prints it
/// without our `Class.method: ` prefix)?
pub fn is_old_verifier_message(message: &str) -> bool {
    message.starts_with("(class: ")
}

/// What the failing instruction was and the frame it started from.
pub struct OldCtx<'a> {
    pub insn: &'a Instruction,
    /// The frame BEFORE the instruction ran.
    pub pre: &'a VerificationFrame,
    /// What this verifier's failing frame operation recorded, if anything.
    pub fault: Option<&'a VerifyFault>,
    pub cp: &'a ConstantPool,
    pub class_name: &'a str,
    pub method_name: &'a str,
    pub descriptor: &'a str,
    pub is_static: bool,
}

/// `check_code.c`'s reason for the instruction this verifier refused, or
/// `None` when the replay cannot name it.
pub fn instruction_reason(c: &OldCtx<'_>) -> Option<String> {
    // `verify_opcode_operands`' constant pool checks (`ldc` / `ldc_w` /
    // `ldc2_w` / `new`), made before the dataflow: `verify_constant_pool_type`
    // (interpreter round i5 wave 108, lane L7).
    if let Some(VerifyFault::BadCpIndex { header, .. }) = c.fault {
        return Some(
            if header.starts_with("Illegal constant pool index") {
                "Illegal constant pool index"
            } else {
                "Illegal type in constant pool"
            }
            .to_string(),
        );
    }
    // `verify_opcode_operands`' invoke checks, also before the dataflow
    // (interpreter round i5 wave 109, lane L7: these fell back to this VM's
    // own text). A method reference named `<init>` under anything but
    // `invokespecial` is "Must call initializers using invokespecial"; any
    // other `<...>` name is "Illegal call to internal method"; a
    // non-`<init>` `invokespecial` of a class that is not a superclass is
    // "Illegal use of nonvirtual function call"; a `lookupswitch` whose keys
    // are not strictly increasing is "Unsorted lookup switch".
    if let Some(VerifyFault::BadCode { header }) = c.fault {
        match header.as_str() {
            "Illegal use of nonvirtual function call" => return Some(header.clone()),
            "Bad lookupswitch instruction" => return Some("Unsorted lookup switch".to_string()),
            // Interpreter round i6 wave 116, lane L7: libverify's
            // `verify_opcode_operands` word for the `invokeinterface` count.
            "Inconsistent args count operand in invokeinterface" => {
                return Some("Inconsistent args_size for invokeinterface".to_string());
            }
            "Illegal newarray instruction" => {
                return Some("Bad type passed to newarray".to_string());
            }
            "Illegal call to internal method" => {
                let index = match c.insn {
                    Instruction::Invokevirtual(i)
                    | Instruction::Invokespecial(i)
                    | Instruction::Invokestatic(i)
                    | Instruction::Invokeinterface { index: i, .. } => Some(*i),
                    _ => None,
                };
                let is_init = index
                    .and_then(|i| super::verify_insn::member_ref_parts(c.cp, i))
                    .is_some_and(|(_, name, _)| name == "<init>");
                return Some(
                    if is_init && !matches!(c.insn, Instruction::Invokespecial(_)) {
                        "Must call initializers using invokespecial"
                    } else {
                        "Illegal call to internal method"
                    }
                    .to_string(),
                );
            }
            _ => {}
        }
    }
    if let Some(reason) = register_reason(c) {
        return Some(reason);
    }
    // `check_flags` (before `pop_stack`; `return` pops nothing).
    if matches!(c.insn, Instruction::Return)
        && c.method_name == "<init>"
        && c.class_name != "java/lang/Object"
        && c.pre.has_uninitialized_this()
    {
        return Some("Constructor must call super() or this()".to_string());
    }
    match pop_reason(c) {
        PopOutcome::Error(reason) => return Some(reason.to_string()),
        PopOutcome::Unknown => return None,
        PopOutcome::Passed => {}
    }
    // `run_dataflow`: the size check follows the push.
    if matches!(c.fault, Some(VerifyFault::StackOverflow)) {
        return Some("Stack size too large".to_string());
    }
    post_pop_reason(c)
}

/// The register count `check_register_values` compares against: the
/// argument slots, grown by every store (approximated by the highest
/// non-`Top` local of the frame).
fn register_count(c: &OldCtx<'_>) -> usize {
    let mut args = usize::from(!c.is_static);
    for t in super::vtype::param_types_from_descriptor(c.descriptor) {
        args += if t.is_category2() { 2 } else { 1 };
    }
    let locals: &[VType] = &c.pre.locals;
    let mut high = 0;
    for (i, t) in locals.iter().enumerate() {
        if *t != VType::Top {
            high = i + 1 + usize::from(t.is_category2());
        }
    }
    args.max(high)
}

/// `check_register_values`.
fn register_reason(c: &OldCtx<'_>) -> Option<String> {
    let (index, kind) = match c.insn {
        Instruction::Iload(i) | Instruction::Iinc { index: i, .. } => (*i, b'I'),
        Instruction::Fload(i) => (*i, b'F'),
        Instruction::Aload(i) => (*i, b'A'),
        Instruction::Lload(i) => (*i, b'L'),
        Instruction::Dload(i) => (*i, b'D'),
        // `ret` checks its register for `ITEM_ReturnAddress` (wave 104
        // hand-back, lane L7; text as `check_register_values` gives it for a
        // register of the wrong type, not yet measured for `ret`).
        Instruction::Ret(i) => (*i, b'R'),
        _ => return None,
    };
    let locals: &[VType] = &c.pre.locals;
    let count = register_count(c);
    let idx = usize::from(index);
    if kind == b'L' || kind == b'D' {
        if idx + 1 >= count {
            return Some(format!(
                "Accessing value from uninitialized register pair {}/{}",
                idx,
                idx + 1
            ));
        }
        let base = if kind == b'L' { VType::Long } else { VType::Double };
        if locals.get(idx) == Some(&base) && locals.get(idx + 1) == Some(&VType::Top) {
            return None;
        }
        return Some(format!("Register pair {}/{} contains wrong type", idx, idx + 1));
    }
    if idx >= count {
        return Some(format!("Accessing value from uninitialized register {idx}"));
    }
    let reg = locals.get(idx).unwrap_or(&VType::Top);
    let ok = match kind {
        b'I' => *reg == VType::Int,
        b'F' => *reg == VType::Float,
        b'R' => matches!(reg, VType::ReturnAddress(_)),
        // An object, an array, null, or an uninitialized object
        // (`ITEM_InitObject` / `ITEM_NewObject`).
        _ => reg.is_reference(),
    };
    if ok {
        None
    } else if kind != b'R' && matches!(reg, VType::ReturnAddress(_)) {
        Some(format!("Cannot load return address from register {idx}"))
    } else {
        Some(format!("Register {idx} contains wrong type"))
    }
}

/// A `check_code.c` stack item (a `long` / `double` is ONE item there).
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Int,
    Float,
    Long,
    Double,
    /// A non-array object.
    Obj,
    Null,
    /// An array of the descriptor.
    Arr(Arc<str>),
    /// `ITEM_NewObject`.
    NewObj,
    /// `ITEM_InitObject`.
    InitObj,
    RetAddr,
    Bogus,
}

fn items(stack: &[VType]) -> Vec<Item> {
    let mut out = Vec::with_capacity(stack.len());
    let mut i = 0;
    while i < stack.len() {
        let item = match &stack[i] {
            VType::Int => Item::Int,
            VType::Float => Item::Float,
            VType::Long | VType::Double => {
                let item = if stack[i] == VType::Long { Item::Long } else { Item::Double };
                if stack.get(i + 1) == Some(&VType::Top) {
                    i += 1;
                }
                item
            }
            VType::Null => Item::Null,
            VType::ObjectRef(_) => Item::Obj,
            VType::ArrayRef(d) => Item::Arr(Arc::clone(d)),
            VType::UninitializedThis => Item::InitObj,
            VType::Uninitialized(_) => Item::NewObj,
            VType::ReturnAddress(_) => Item::RetAddr,
            VType::Top => Item::Bogus,
        };
        out.push(item);
        i += 1;
    }
    out
}

/// `signature_to_fieldtype`'s character for one field descriptor.
fn field_char(desc: &str) -> u8 {
    match desc.as_bytes().first() {
        Some(b'F') => b'F',
        Some(b'J') => b'L',
        Some(b'D') => b'D',
        Some(b'L') | Some(b'[') => b'A',
        _ => b'I',
    }
}

/// The parameter characters of a method descriptor.
fn param_chars(descriptor: &str) -> String {
    let mut out = String::new();
    for t in super::vtype::param_types_from_descriptor(descriptor) {
        out.push(match t {
            VType::Float => 'F',
            VType::Long => 'L',
            VType::Double => 'D',
            VType::Int => 'I',
            _ => 'A',
        });
    }
    out
}

/// `opcode_in_out[opcode][0]`, or the operand list `pop_stack` builds for a
/// field / invoke / `multianewarray` instruction. `None` for what the replay
/// does not model.
fn operands(c: &OldCtx<'_>) -> Option<String> {
    use Instruction as I;
    let fixed = match c.insn {
        I::Iaload => "[I]I",
        I::Laload => "[L]I",
        I::Faload => "[F]I",
        I::Daload => "[D]I",
        I::Aaload => "[A]I",
        I::Baload => "[B]I",
        I::Caload => "[C]I",
        I::Saload => "[S]I",
        I::Istore(_) => "I",
        I::Lstore(_) => "L",
        I::Fstore(_) => "F",
        I::Dstore(_) => "D",
        I::Astore(_) => "A",
        I::Iastore => "[I]II",
        I::Lastore => "[L]IL",
        I::Fastore => "[F]IF",
        I::Dastore => "[D]ID",
        I::Aastore => "[A]IA",
        I::Bastore => "[B]II",
        I::Castore => "[C]II",
        I::Sastore => "[S]II",
        I::Pop => "1",
        I::Pop2 => "2+1",
        I::Dup => "1",
        I::DupX1 => "21",
        I::DupX2 => "3+21",
        I::Dup2 => "2+1",
        I::Dup2X1 => "32+1",
        I::Dup2X2 => "4+32+1",
        I::Swap => "21",
        I::Iadd | I::Isub | I::Imul | I::Idiv | I::Irem | I::Ishl | I::Ishr | I::Iushr => "II",
        I::Iand | I::Ior | I::Ixor => "II",
        I::Ladd | I::Lsub | I::Lmul | I::Ldiv | I::Lrem | I::Land | I::Lor | I::Lxor => "LL",
        I::Lshl | I::Lshr | I::Lushr => "LI",
        I::Fadd | I::Fsub | I::Fmul | I::Fdiv | I::Frem => "FF",
        I::Dadd | I::Dsub | I::Dmul | I::Ddiv | I::Drem => "DD",
        I::Ineg | I::I2l | I::I2f | I::I2d | I::I2b | I::I2c | I::I2s => "I",
        I::Lneg | I::L2i | I::L2f | I::L2d => "L",
        I::Fneg | I::F2i | I::F2l | I::F2d => "F",
        I::Dneg | I::D2i | I::D2l | I::D2f => "D",
        I::Lcmp => "LL",
        I::Fcmpl | I::Fcmpg => "FF",
        I::Dcmpl | I::Dcmpg => "DD",
        I::Ifeq(_) | I::Ifne(_) | I::Iflt(_) | I::Ifge(_) | I::Ifgt(_) | I::Ifle(_) => "I",
        I::IfIcmpeq(_)
        | I::IfIcmpne(_)
        | I::IfIcmplt(_)
        | I::IfIcmpge(_)
        | I::IfIcmpgt(_)
        | I::IfIcmple(_) => "II",
        I::IfAcmpeq(_) | I::IfAcmpne(_) => "AA",
        I::Tableswitch(_) | I::Lookupswitch(_) => "I",
        I::Ireturn => "I",
        I::Lreturn => "L",
        I::Freturn => "F",
        I::Dreturn => "D",
        I::Areturn => "A",
        I::Newarray(_) | I::Anewarray(_) => "I",
        I::Arraylength => "[?]",
        I::Athrow => "O",
        I::Checkcast(_) | I::Instanceof(_) | I::Monitorenter | I::Monitorexit => "A",
        I::Ifnull(_) | I::Ifnonnull(_) => "A",
        I::Getfield(_) => "A",
        I::Putstatic(index) | I::Putfield(index) => {
            let (_, _, desc) = super::verify_insn::member_ref_parts(c.cp, *index)?;
            let mut s = String::new();
            if matches!(c.insn, I::Putfield(_)) {
                s.push('A');
            }
            s.push(char::from(field_char(desc)));
            return Some(s);
        }
        I::Invokevirtual(index)
        | I::Invokespecial(index)
        | I::Invokestatic(index)
        | I::Invokeinterface { index, .. } => {
            let (_, name, desc) = super::verify_insn::member_ref_parts(c.cp, *index)?;
            let mut s = String::new();
            if !matches!(c.insn, I::Invokestatic(_)) {
                s.push(if matches!(c.insn, I::Invokespecial(_)) && name == "<init>" {
                    '@'
                } else {
                    'A'
                });
            }
            s.push_str(&param_chars(desc));
            return Some(s);
        }
        I::Multianewarray { dimensions, .. } => {
            return Some("I".repeat(usize::from(*dimensions)));
        }
        I::Invokedynamic(_) | I::Wide => return None,
        _ => "",
    };
    Some(fixed.to_string())
}

enum PopOutcome {
    Passed,
    Error(&'static str),
    Unknown,
}

/// The shape half of `pop_stack`: each operand character, from the last,
/// against the stack top.
fn pop_reason(c: &OldCtx<'_>) -> PopOutcome {
    let Some(ops) = operands(c) else {
        return PopOutcome::Unknown;
    };
    let ops = ops.as_bytes();
    let mut stack = items(&c.pre.stack);
    let mut p = ops.len();
    while p > 0 {
        p -= 1;
        let ty = ops[p];
        let Some(top) = stack.pop() else {
            return PopOutcome::Error("Unable to pop operand off an empty stack");
        };
        match ty {
            b'I' if top != Item::Int => {
                return PopOutcome::Error("Expecting to find integer on stack");
            }
            b'F' if top != Item::Float => {
                return PopOutcome::Error("Expecting to find float on stack");
            }
            b'D' if top != Item::Double => {
                return PopOutcome::Error("Expecting to find double on stack");
            }
            b'L' if top != Item::Long => {
                return PopOutcome::Error("Expecting to find long on stack");
            }
            b'I' | b'F' | b'D' | b'L' => {}
            b'A' => {
                let is_ref = matches!(top, Item::Obj | Item::Null | Item::Arr(_));
                let special = match top {
                    Item::RetAddr => matches!(c.insn, Instruction::Astore(_)),
                    Item::NewObj | Item::InitObj => matches!(
                        c.insn,
                        Instruction::Astore(_)
                            | Instruction::Aload(_)
                            | Instruction::Ifnull(_)
                            | Instruction::Ifnonnull(_)
                    ) || (top == Item::InitObj && putfield_of_own_field(c)),
                    _ => false,
                };
                if !is_ref && !special {
                    return PopOutcome::Error("Expecting to find object/array on stack");
                }
            }
            b'@' => {
                if !matches!(top, Item::NewObj | Item::InitObj) {
                    return PopOutcome::Error("Expecting to find uninitialized object on stack");
                }
            }
            b'O' => {
                if !matches!(top, Item::Obj | Item::Null) {
                    return PopOutcome::Error("Expecting to find object on stack");
                }
            }
            b']' => {
                let Some(&elem) = p.checked_sub(1).and_then(|i| ops.get(i)) else {
                    return PopOutcome::Unknown;
                };
                if top != Item::Null {
                    if let Some(text) = array_operand_error(elem, &top) {
                        return PopOutcome::Error(text);
                    }
                }
                // Skip the element character and the `[`.
                p = p.saturating_sub(2);
            }
            b'1'..=b'4' => {
                if matches!(top, Item::Long | Item::Double) {
                    if p > 0 && ops[p - 1] == b'+' {
                        p -= 2.min(p);
                    } else {
                        return PopOutcome::Error("Attempt to split long or double on the stack");
                    }
                } else if p > 0 && ops[p - 1] == b'+' {
                    p -= 1;
                }
            }
            _ => return PopOutcome::Unknown,
        }
    }
    PopOutcome::Passed
}

/// A `putfield` of a field the current class declares (the 2nd-edition rule
/// that lets a constructor set its own fields before `super()`).
fn putfield_of_own_field(c: &OldCtx<'_>) -> bool {
    match c.insn {
        Instruction::Putfield(index) => super::verify_insn::member_ref_parts(c.cp, *index)
            .is_some_and(|(owner, _, _)| owner == c.class_name),
        _ => false,
    }
}

/// The `]` case of `pop_stack` for a non-null array operand.
fn array_operand_error(elem: u8, top: &Item) -> Option<&'static str> {
    let desc: &str = match top {
        Item::Arr(d) => d,
        _ => "",
    };
    let second = desc.as_bytes().get(1).copied();
    let (ok, text) = match elem {
        b'I' => (desc == "[I", "Expecting to find array of ints on stack"),
        b'L' => (desc == "[J", "Expecting to find array of longs on stack"),
        b'F' => (desc == "[F", "Expecting to find array of floats on stack"),
        b'D' => (desc == "[D", "Expecting to find array of doubles on stack"),
        b'A' => (
            matches!(second, Some(b'L') | Some(b'[')),
            "Expecting to find array of objects or arrays on stack",
        ),
        b'B' => (
            desc == "[B" || desc == "[Z",
            "Expecting to find array of bytes or Booleans on stack",
        ),
        b'C' => (desc == "[C", "Expecting to find array of chars on stack"),
        b'S' => (desc == "[S", "Expecting to find array of shorts on stack"),
        b'?' => (!desc.is_empty(), "Expecting to find array on stack"),
        _ => (true, ""),
    };
    (!ok).then_some(text)
}

/// The slot index (this verifier's numbering: a `long` is two slots) of the
/// stack slot a fault names, if it names one.
fn fault_slot(fault: Option<&VerifyFault>) -> Option<usize> {
    match fault? {
        VerifyFault::BadStackType { index, .. }
        | VerifyFault::BadTypeWith { index, .. }
        | VerifyFault::BadReturn { index, .. }
        | VerifyFault::InvalidType { index, .. } => Some(*index),
        _ => None,
    }
}

fn fault_is_protected(fault: Option<&VerifyFault>) -> bool {
    matches!(fault, Some(VerifyFault::BadTypeWith { header, .. }) if header.contains("protected"))
}

fn slots_of(descriptor: &str) -> usize {
    super::vtype::param_types_from_descriptor(descriptor)
        .iter()
        .map(|t| if t.is_category2() { 2 } else { 1 })
        .sum()
}

/// The assignability half of `pop_stack`, for an instruction whose operand
/// shapes passed: which check this verifier's refusal corresponds to.
fn post_pop_reason(c: &OldCtx<'_>) -> Option<String> {
    use Instruction as I;
    let text = match c.insn {
        I::Getfield(_) | I::Putfield(_) | I::Putstatic(_) => {
            if fault_is_protected(c.fault) {
                "Bad access to protected data"
            } else if matches!(c.insn, I::Putstatic(_)) {
                "Bad type in putfield/putstatic"
            } else if matches!(c.insn, I::Getfield(_)) {
                "Incompatible type for getting or setting field"
            } else {
                // putfield: [objectref, value]; the value is the top slot(s).
                let index = match c.insn {
                    I::Putfield(index) => *index,
                    _ => return None,
                };
                let (_, _, desc) = super::verify_insn::member_ref_parts(c.cp, index)?;
                let value_slots = if matches!(desc.as_bytes().first(), Some(b'J') | Some(b'D')) {
                    2
                } else {
                    1
                };
                let value_at = c.pre.stack.len().checked_sub(value_slots)?;
                match fault_slot(c.fault) {
                    Some(slot) if slot >= value_at => "Bad type in putfield/putstatic",
                    Some(_) => "Incompatible type for getting or setting field",
                    None => return None,
                }
            }
        }
        I::Athrow => "Can only throw Throwable objects",
        I::Invokevirtual(index) | I::Invokespecial(index) | I::Invokeinterface { index, .. } => {
            let (_, name, desc) = super::verify_insn::member_ref_parts(c.cp, *index)?;
            if name == "<init>" {
                // `invokeinit` (interpreter round i5 wave 108, lane L7): the
                // split rules' faults name which check failed. libverify
                // checks the object first: an `ITEM_NewObject` whose `new`
                // created another class, or an `ITEM_InitObject` whose
                // constructor is neither the current class's nor the
                // superclass's, is "Call to wrong initialization method";
                // then each argument ("Incompatible argument to function").
                return match c.fault {
                    Some(VerifyFault::BadTypeNoFrame { header, .. })
                        if header == "Call to wrong <init> method"
                            || header == "Bad <init> method call" =>
                    {
                        Some("Call to wrong initialization method".to_string())
                    }
                    _ => {
                        let receiver_at = c.pre.stack.len().checked_sub(slots_of(desc) + 1)?;
                        match fault_slot(c.fault) {
                            Some(slot) if slot > receiver_at => {
                                Some("Incompatible argument to function".to_string())
                            }
                            _ => None,
                        }
                    }
                };
            }
            let receiver_at = c.pre.stack.len().checked_sub(slots_of(desc) + 1)?;
            match fault_slot(c.fault) {
                Some(slot) if slot == receiver_at => {
                    if fault_is_protected(c.fault) {
                        "Bad access to protected data"
                    } else if matches!(c.insn, I::Invokespecial(_))
                        && matches!(
                            c.fault,
                            Some(VerifyFault::BadStackType { expected: HsType::V(VType::ObjectRef(n)), .. })
                                | Some(VerifyFault::BadTypeWith { expected: HsType::V(VType::ObjectRef(n)), .. })
                                if &**n == c.class_name
                        )
                    {
                        "Incompatible object argument for invokespecial"
                    } else {
                        "Incompatible object argument for function call"
                    }
                }
                Some(_) => "Incompatible argument to function",
                None => return None,
            }
        }
        I::Invokestatic(_) => match fault_slot(c.fault) {
            Some(_) => "Incompatible argument to function",
            None => return None,
        },
        I::Ireturn | I::Lreturn | I::Freturn | I::Dreturn | I::Areturn => {
            "Wrong return type in function"
        }
        I::Return => {
            if c.descriptor.ends_with(")V") {
                return None;
            }
            "Wrong return type in function"
        }
        _ => return None,
    };
    Some(text.to_string())
}

/// `merge_stack`'s refusal when `incoming` (the stack leaving an instruction)
/// meets `existing` (the stack recorded at its successor): "Inconsistent
/// stack height N != M" (incoming, then recorded size, in check_code's slots,
/// a `long` counting two) or "Mismatched stack types" when two slots have no
/// common type. `None` when the stacks merge.
pub fn merge_reason(
    existing: &VerificationFrame,
    incoming: &VerificationFrame,
    hierarchy: &dyn super::vtype::ClassHierarchy,
) -> Option<String> {
    if existing.stack.len() != incoming.stack.len() {
        return Some(format!(
            "Inconsistent stack height {} != {}",
            incoming.stack.len(),
            existing.stack.len()
        ));
    }
    for (old, new) in existing.stack.iter().zip(incoming.stack.iter()) {
        if *old == VType::Top || *new == VType::Top || new.is_assignable_to(old, hierarchy) {
            continue;
        }
        if old.merge(new, hierarchy) == VType::Top {
            return Some("Mismatched stack types".to_string());
        }
    }
    None
}
