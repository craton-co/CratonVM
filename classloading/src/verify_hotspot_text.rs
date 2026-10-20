// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot's `VerifyError` text for the split (StackMapTable) verifier.
//!
//! Interpreter round i5 wave 101, lane L7. HotSpot 25's
//! `ClassVerifier::exception_message` is the verify error's header line
//! (`"Bad type on operand stack"`, `"Expecting a stackmap frame at branch
//! target 7"`, ...) followed by `ErrorContext::details`:
//!
//! ```text
//! Exception Details:
//!   Location:
//!     p/C.m(I)V @3: invokevirtual
//!   Reason:
//!     Type integer (current frame, stack[0]) is not assignable to 'java/lang/String'
//!   Current Frame:
//!     bci: @3
//!     flags: { }
//!     locals: { integer }
//!     stack: { integer }
//!   Bytecode:
//!     0000000: 1a3c b600 07b1
//!   Exception Handler Table:
//!     bci [0, 4] => handler: 7
//!   Stackmap Table:
//!     same_frame(@7)
//! ```
//!
//! (`verifier.cpp` `ErrorContext::{location,reason,frame,bytecode,handler,
//! stackmap}_details`, `stackMapFrame.cpp` `StackMapFrame::print_on`,
//! `verificationType.cpp` `VerificationType::print_on`,
//! `stackMapTableFormat.hpp` `*_frame::print_on`, `ostream.cpp`
//! `outputStream::print_data`.) The message is the whole `getMessage()`:
//! `linkage_throwable` (vm) uses it verbatim, without the `Class.method: `
//! prefix our own texts carry, when it contains [`DETAILS_MARKER`].

use std::fmt::Write as _;

use cratonvm_reader::attribute::ExceptionTableEntry;
use cratonvm_reader::stack_map::{StackMapFrame, StackMapTable, VerificationTypeInfo};

use super::verify_frame::{HsType, VerificationFrame, VerifyFault};
use super::vtype::VType;

/// The line every HotSpot-shaped verify message carries after its header.
pub const DETAILS_MARKER: &str = "\nException Details:\n";

/// The method a verify error is reported against.
pub struct HsMethod<'a> {
    /// Internal (slash) name of the class.
    pub class_name: &'a str,
    pub method_name: &'a str,
    pub descriptor: &'a str,
    /// The method's code bytes (exactly `code_length` of them).
    pub code: &'a [u8],
    pub exception_table: &'a [ExceptionTableEntry],
    /// The raw `StackMapTable` attribute, when the method has one.
    pub stack_map_raw: Option<&'a [u8]>,
}

/// Where a type in a `Reason:` line came from (`TypeOrigin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `(current frame, stack[i])`.
    CfStack(usize),
    /// `(current frame, locals[i])`.
    CfLocals(usize),
    /// `(stack map, stack[i])`.
    SmStack(usize),
    /// `(stack map, locals[i])`.
    SmLocals(usize),
    /// `(constant pool N)`.
    Cp(u16),
    /// Nothing printed after the type (`TypeOrigin::implicit`).
    Implicit,
    /// `(from method signature)` (`TypeOrigin::signature`; interpreter round
    /// i5 wave 102, lane L7).
    Signature,
}

/// The `Reason:` line (`ErrorContext::FaultType`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// `WRONG_TYPE` with an expected type.
    WrongType {
        actual: HsType,
        actual_origin: Origin,
        expected: HsType,
        expected_origin: Origin,
    },
    /// `WRONG_TYPE` without an expected type: "Invalid type: T ..."
    /// (interpreter round i5 wave 102, lane L7).
    InvalidType { actual: HsType, actual_origin: Origin },
    /// `INVALID_BYTECODE`.
    BadCode,
    /// `BAD_LOCAL_INDEX`.
    BadLocalIndex(u16),
    /// `LOCALS_SIZE_MISMATCH`.
    LocalsSizeMismatch,
    /// `STACK_SIZE_MISMATCH`.
    StackSizeMismatch,
    /// `STACK_OVERFLOW`.
    StackOverflow,
    /// `STACK_UNDERFLOW`.
    StackUnderflow,
    /// `MISSING_STACKMAP`.
    MissingStackmap,
    /// `BAD_CP_INDEX` (interpreter round i5 wave 107, lane L7).
    BadCpIndex(u16),
    /// `FLAGS_MISMATCH` with a stack map frame (`ErrorContext::bad_flags`;
    /// interpreter round i5 wave 108, lane L7).
    FlagsMismatch,
    /// `BAD_STACKMAP` (`ErrorContext::bad_stackmap`, "StackMapTable error:
    /// bad offset"; interpreter round i5 wave 109, lane L7).
    BadStackmap,
}

/// A frame as `StackMapFrame::print_on` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HsFrame {
    pub bci: usize,
    pub this_uninit: bool,
    pub locals: Vec<HsType>,
    pub stack: Vec<HsType>,
}

impl HsFrame {
    /// The verifier's CURRENT frame at `bci`. HotSpot prints the frame's
    /// `locals_size` slots: the slots the method's signature and its stores
    /// reached. This walk pads locals to `max_locals` with `Top`, so trailing
    /// `Top`s that are not the upper half of a `long` / `double` are dropped
    /// (a declared trailing `Top` is the one case this cannot tell apart).
    pub fn current(frame: &VerificationFrame, bci: usize) -> Self {
        let slots: &[VType] = &frame.locals;
        let mut size = slots.len();
        while size > 0 && slots[size - 1] == VType::Top {
            let upper = size >= 2 && matches!(slots[size - 2], VType::Long | VType::Double);
            if upper {
                break;
            }
            size -= 1;
        }
        Self::with_locals(frame, bci, size)
    }

    /// A DECLARED (StackMapTable) frame at `bci`: its locals are exactly the
    /// declared slots (the walk keeps declared frames compact).
    pub fn declared(frame: &VerificationFrame, bci: usize) -> Self {
        Self::with_locals(frame, bci, frame.locals.len())
    }

    fn with_locals(frame: &VerificationFrame, bci: usize, size: usize) -> Self {
        let slots: &[VType] = &frame.locals;
        let size = size.min(slots.len());
        HsFrame {
            bci,
            this_uninit: frame.has_uninitialized_this(),
            locals: (0..size).map(|i| VerifyFault::slot_type(slots, i)).collect(),
            stack: (0..frame.stack.len())
                .map(|i| VerifyFault::slot_type(&frame.stack, i))
                .collect(),
        }
    }

    /// HotSpot's `locals_size` of this frame.
    pub fn locals_size(&self) -> usize {
        self.locals.len()
    }
}

/// `VerificationType::print_on`.
pub fn type_text(t: &HsType) -> String {
    match t {
        HsType::Long2nd => "long_2nd".to_string(),
        HsType::Double2nd => "double_2nd".to_string(),
        HsType::Reference => "reference type".to_string(),
        HsType::V(v) => match v {
            VType::Top | VType::ReturnAddress(_) => "top".to_string(),
            VType::Int => "integer".to_string(),
            VType::Float => "float".to_string(),
            VType::Long => "long".to_string(),
            VType::Double => "double".to_string(),
            VType::Null => "null".to_string(),
            VType::ObjectRef(name) | VType::ArrayRef(name) => format!("'{name}'"),
            VType::UninitializedThis => "uninitializedThis".to_string(),
            VType::Uninitialized(bci) => format!("uninitialized {bci}"),
        },
    }
}

/// `TypeOrigin::details`.
fn origin_text(t: &HsType, origin: Origin) -> String {
    let ty = type_text(t);
    match origin {
        Origin::CfStack(i) => format!("{ty} (current frame, stack[{i}])"),
        Origin::CfLocals(i) => format!("{ty} (current frame, locals[{i}])"),
        Origin::SmStack(i) => format!("{ty} (stack map, stack[{i}])"),
        Origin::SmLocals(i) => format!("{ty} (stack map, locals[{i}])"),
        Origin::Cp(i) => format!("{ty} (constant pool {i})"),
        Origin::Implicit => ty,
        Origin::Signature => format!("{ty} (from method signature)"),
    }
}

fn reason_text(reason: &Reason) -> String {
    match reason {
        Reason::WrongType {
            actual,
            actual_origin,
            expected,
            expected_origin,
        } => format!(
            "Type {} is not assignable to {}",
            origin_text(actual, *actual_origin),
            origin_text(expected, *expected_origin)
        ),
        Reason::InvalidType {
            actual,
            actual_origin,
        } => format!("Invalid type: {}", origin_text(actual, *actual_origin)),
        Reason::BadCode => "Error exists in the bytecode".to_string(),
        Reason::BadLocalIndex(i) => format!("Local index {i} is invalid"),
        Reason::LocalsSizeMismatch => "Current frame's local size doesn't match stackmap.".to_string(),
        Reason::StackSizeMismatch => "Current frame's stack size doesn't match stackmap.".to_string(),
        Reason::StackOverflow => "Exceeded max stack size.".to_string(),
        Reason::StackUnderflow => "Attempt to pop empty stack.".to_string(),
        Reason::MissingStackmap => "Expected stackmap frame at this location.".to_string(),
        Reason::BadCpIndex(i) => format!("Constant pool index {i} is invalid"),
        Reason::FlagsMismatch => {
            "Current frame's flags are not assignable to stack map frame's.".to_string()
        }
        Reason::BadStackmap => "Invalid stackmap specification.".to_string(),
    }
}

/// `StackMapFrame::print_on` at indent 4.
fn frame_text(out: &mut String, f: &HsFrame) {
    let _ = writeln!(out, "    bci: @{}", f.bci);
    let _ = writeln!(
        out,
        "    flags: {{{} }}",
        if f.this_uninit { " flagThisUninit" } else { "" }
    );
    let list = |types: &[HsType]| {
        let mut s = String::new();
        for (i, t) in types.iter().enumerate() {
            s.push(' ');
            s.push_str(&type_text(t));
            if i + 1 != types.len() {
                s.push(',');
            }
        }
        s
    };
    let _ = writeln!(out, "    locals: {{{} }}", list(&f.locals));
    let _ = writeln!(out, "    stack: {{{} }}", list(&f.stack));
}

/// `outputStream::print_data(code, len, false)` at indent 4 (relative
/// addresses): sixteen bytes a line, a space before every byte pair, the last
/// line padded to sixteen byte columns, and one extra blank line when the
/// length is a multiple of sixteen (the loop runs to `(len + 16) / 16 * 16`).
fn bytecode_text(out: &mut String, code: &[u8]) {
    let len = code.len();
    let limit = (len + 16) / 16 * 16;
    for i in 0..limit {
        if i % 16 == 0 {
            let _ = write!(out, "    {i:07x}:");
        }
        if i % 2 == 0 {
            out.push(' ');
        }
        match code.get(i) {
            Some(b) if i < len => {
                let _ = write!(out, "{b:02x}");
            }
            _ => out.push_str("  "),
        }
        if (i + 1) % 16 == 0 {
            out.push('\n');
        }
    }
}

/// `verification_type_info::print_on`.
fn vti_text(v: &VerificationTypeInfo) -> String {
    match v {
        VerificationTypeInfo::Top => "Top".to_string(),
        VerificationTypeInfo::Integer => "Integer".to_string(),
        VerificationTypeInfo::Float => "Float".to_string(),
        VerificationTypeInfo::Double => "Double".to_string(),
        VerificationTypeInfo::Long => "Long".to_string(),
        VerificationTypeInfo::Null => "Null".to_string(),
        VerificationTypeInfo::UninitializedThis => "UninitializedThis".to_string(),
        VerificationTypeInfo::Object { cpool_index } => format!("Object[#{cpool_index}]"),
        VerificationTypeInfo::Uninitialized { offset } => format!("Uninitialized[#{offset}]"),
    }
}

fn vti_list(types: &[VerificationTypeInfo]) -> String {
    types.iter().map(vti_text).collect::<Vec<_>>().join(",")
}

/// `stack_map_frame::print_on` of each entry, at indent 4.
fn stack_map_text(out: &mut String, raw: &[u8]) {
    let Ok(table) = StackMapTable::parse(raw) else {
        return;
    };
    out.push_str("  Stackmap Table:\n");
    let mut current: i64 = -1;
    for frame in &table.entries {
        let delta = i64::from(match frame {
            StackMapFrame::SameFrame { offset_delta }
            | StackMapFrame::SameLocals1StackItem { offset_delta, .. }
            | StackMapFrame::SameLocals1StackItemExtended { offset_delta, .. }
            | StackMapFrame::ChopFrame { offset_delta, .. }
            | StackMapFrame::SameFrameExtended { offset_delta }
            | StackMapFrame::AppendFrame { offset_delta, .. }
            | StackMapFrame::FullFrame { offset_delta, .. } => *offset_delta,
        });
        let bci = current + delta + 1;
        let text = match frame {
            StackMapFrame::SameFrame { .. } => format!("same_frame(@{bci})"),
            StackMapFrame::SameLocals1StackItem { stack, .. } => {
                format!("same_locals_1_stack_item_frame(@{bci},{})", vti_text(stack))
            }
            StackMapFrame::SameLocals1StackItemExtended { stack, .. } => {
                format!("same_locals_1_stack_item_extended(@{bci},{})", vti_text(stack))
            }
            StackMapFrame::ChopFrame { chopped, .. } => format!("chop_frame(@{bci},{chopped})"),
            StackMapFrame::SameFrameExtended { .. } => format!("same_frame_extended(@{bci})"),
            StackMapFrame::AppendFrame { locals, .. } => {
                format!("append_frame(@{bci},{})", vti_list(locals))
            }
            StackMapFrame::FullFrame { locals, stack, .. } => {
                format!("full_frame(@{bci},{{{}}},{{{}}})", vti_list(locals), vti_list(stack))
            }
        };
        let _ = writeln!(out, "    {text}");
        current = bci;
    }
}

/// HotSpot's name of the bytecode at `bci` (`Bytecodes::name`).
fn bytecode_name(code: &[u8], bci: usize) -> &'static str {
    match code.get(bci) {
        Some(&op) if op < 0xca => cratonvm_reader::quickened::opcode_mnemonic(op),
        Some(_) => "<illegal>",
        None => "<invalid>",
    }
}

/// The whole `getMessage()` of a split-verifier `VerifyError`: `header`, then
/// `Exception Details:` for an error at `bci` with `reason`, the current
/// frame and the stack map frame when the error has them.
pub fn render(
    m: &HsMethod<'_>,
    header: &str,
    bci: usize,
    reason: &Reason,
    current: Option<&HsFrame>,
    stack_map: Option<&HsFrame>,
) -> String {
    let mut out = String::with_capacity(256 + m.code.len() * 3);
    out.push_str(header);
    out.push_str(DETAILS_MARKER);
    out.push_str("  Location:\n");
    let _ = writeln!(
        out,
        "    {}.{}{} @{}: {}",
        m.class_name,
        m.method_name,
        m.descriptor,
        bci,
        bytecode_name(m.code, bci)
    );
    out.push_str("  Reason:\n");
    let _ = writeln!(out, "    {}", reason_text(reason));
    if let Some(f) = current {
        out.push_str("  Current Frame:\n");
        frame_text(&mut out, f);
    }
    if let Some(f) = stack_map {
        out.push_str("  Stackmap Frame:\n");
        frame_text(&mut out, f);
    }
    out.push_str("  Bytecode:\n");
    bytecode_text(&mut out, m.code);
    if !m.exception_table.is_empty() {
        out.push_str("  Exception Handler Table:\n");
        for e in m.exception_table {
            let _ = writeln!(
                out,
                "    bci [{}, {}] => handler: {}",
                e.start_pc, e.end_pc, e.handler_pc
            );
        }
    }
    if let Some(raw) = m.stack_map_raw {
        stack_map_text(&mut out, raw);
    }
    out
}

/// The header, reason and whether the current frame is printed, for a fault
/// a frame operation recorded at `bci`. `pre` is the frame as it was before
/// the instruction at `bci` ran (HotSpot restores the popped operands before
/// printing it).
pub fn render_fault(m: &HsMethod<'_>, bci: usize, fault: &VerifyFault, pre: &HsFrame) -> String {
    match fault {
        VerifyFault::BadStackType {
            index,
            actual,
            expected,
        } => render(
            m,
            "Bad type on operand stack",
            bci,
            &Reason::WrongType {
                actual: actual.clone(),
                actual_origin: Origin::CfStack(*index),
                expected: expected.clone(),
                expected_origin: Origin::Implicit,
            },
            Some(pre),
            None,
        ),
        VerifyFault::StackUnderflow => render(
            m,
            "Operand stack underflow",
            bci,
            &Reason::StackUnderflow,
            Some(pre),
            None,
        ),
        VerifyFault::StackOverflow => render(
            m,
            "Operand stack overflow",
            bci,
            &Reason::StackOverflow,
            Some(pre),
            None,
        ),
        VerifyFault::BadLocalType {
            index,
            actual,
            expected,
        } => render(
            m,
            "Bad local variable type",
            bci,
            &Reason::WrongType {
                actual: actual.clone(),
                actual_origin: Origin::CfLocals(usize::from(*index)),
                expected: expected.clone(),
                expected_origin: Origin::Implicit,
            },
            Some(pre),
            None,
        ),
        VerifyFault::BadCode { header } => render(m, header, bci, &Reason::BadCode, None, None),
        VerifyFault::BadTypeWith {
            header,
            index,
            actual,
            expected,
            expected_cp,
        } => render(
            m,
            header,
            bci,
            &Reason::WrongType {
                actual: actual.clone(),
                actual_origin: Origin::CfStack(*index),
                expected: expected.clone(),
                expected_origin: expected_cp.map_or(Origin::Implicit, Origin::Cp),
            },
            Some(pre),
            None,
        ),
        VerifyFault::BadReturn {
            header,
            index,
            actual,
            expected,
        } => render(
            m,
            header,
            bci,
            &Reason::WrongType {
                actual: actual.clone(),
                actual_origin: Origin::CfStack(*index),
                expected: expected.clone(),
                expected_origin: Origin::Signature,
            },
            Some(pre),
            None,
        ),
        VerifyFault::InvalidType {
            header,
            index,
            actual,
        } => render(
            m,
            header,
            bci,
            &Reason::InvalidType {
                actual: actual.clone(),
                actual_origin: Origin::CfStack(*index),
            },
            Some(pre),
            None,
        ),
        VerifyFault::BadTypeNoFrame {
            header,
            actual,
            actual_cp,
            expected,
            expected_cp,
        } => {
            let actual_origin = actual_cp.map_or(Origin::Implicit, Origin::Cp);
            let reason = match expected {
                Some(expected) => Reason::WrongType {
                    actual: actual.clone(),
                    actual_origin,
                    expected: expected.clone(),
                    expected_origin: expected_cp.map_or(Origin::Implicit, Origin::Cp),
                },
                None => Reason::InvalidType {
                    actual: actual.clone(),
                    actual_origin,
                },
            };
            render(m, header, bci, &reason, None, None)
        }
        VerifyFault::BadCpIndex { header, index } => {
            render(m, header, bci, &Reason::BadCpIndex(*index), None, None)
        }
        VerifyFault::BadLocalPair {
            index,
            slot,
            actual,
            expected,
        } => {
            // `get_local_2`: `index >= _locals_size - 1` (int arithmetic).
            if usize::from(*index) + 1 >= pre.locals_size() {
                render(
                    m,
                    "get long/double overflows locals",
                    bci,
                    &Reason::BadLocalIndex(*index),
                    None,
                    None,
                )
            } else {
                render(
                    m,
                    "Bad local variable type",
                    bci,
                    &Reason::WrongType {
                        actual: actual.clone(),
                        actual_origin: Origin::CfLocals(usize::from(*slot)),
                        expected: expected.clone(),
                        expected_origin: Origin::Implicit,
                    },
                    Some(pre),
                    None,
                )
            }
        }
    }
}

/// Where `current` first fails to be assignable to the declared frame
/// `target` (HotSpot's `StackMapFrame::is_assignable_to` order: stack size,
/// then each declared local, then each stack slot). `None` when it is
/// assignable by this comparison.
pub fn frame_mismatch(
    current: &VerificationFrame,
    target: &VerificationFrame,
    hierarchy: &dyn super::vtype::ClassHierarchy,
) -> Option<FrameMismatch> {
    if current.stack.len() != target.stack.len() {
        return Some(FrameMismatch::StackSize);
    }
    let cur_locals: &[VType] = &current.locals;
    let tgt_locals: &[VType] = &target.locals;
    for (i, t) in tgt_locals.iter().enumerate() {
        let c = cur_locals.get(i).unwrap_or(&VType::Top);
        if !c.is_assignable_to(t, hierarchy) {
            return Some(FrameMismatch::Local(i));
        }
    }
    for (i, t) in target.stack.iter().enumerate() {
        if !current.stack[i].is_assignable_to(t, hierarchy) {
            return Some(FrameMismatch::Stack(i));
        }
    }
    None
}

/// The first difference [`frame_mismatch`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMismatch {
    StackSize,
    Local(usize),
    Stack(usize),
}

/// The message of a frame mismatch: `header` at `at_bci` (the branch, or the
/// pc whose declared frame the fall-through reached), `target_bci` the
/// declared frame's pc. `pre` is the current frame as printed, `current` the
/// one compared.
#[allow(clippy::too_many_arguments)]
pub fn render_mismatch(
    m: &HsMethod<'_>,
    header: &str,
    at_bci: usize,
    target_bci: usize,
    mismatch: FrameMismatch,
    pre: &HsFrame,
    current: &VerificationFrame,
    target: &VerificationFrame,
) -> String {
    let sm = HsFrame::declared(target, target_bci);
    let (bci, reason) = match mismatch {
        // `stack_size_mismatch(_offset, ...)`: the current frame's offset.
        FrameMismatch::StackSize => (at_bci, Reason::StackSizeMismatch),
        // `bad_type(target->offset(), ...)`: the declared frame's offset.
        FrameMismatch::Local(i) => (
            target_bci,
            Reason::WrongType {
                actual: VerifyFault::slot_type(&current.locals, i),
                actual_origin: Origin::CfLocals(i),
                expected: VerifyFault::slot_type(&target.locals, i),
                expected_origin: Origin::SmLocals(i),
            },
        ),
        FrameMismatch::Stack(i) => (
            target_bci,
            Reason::WrongType {
                actual: VerifyFault::slot_type(&current.stack, i),
                actual_origin: Origin::CfStack(i),
                expected: VerifyFault::slot_type(&target.stack, i),
                expected_origin: Origin::SmStack(i),
            },
        ),
    };
    render(m, header, bci, &reason, Some(pre), Some(&sm))
}

/// A frame whose `flagThisUninit` the declared frame `target` at `target_bci`
/// lacks (`StackMapFrame::is_assignable_to`'s last check,
/// `ErrorContext::bad_flags(target->offset(), frame, sm)`): `header` at the
/// declared frame's offset, `current` printed as given (its `this_uninit`
/// is the verifier's flag, not this walk's locals). Interpreter round i5
/// wave 108, lane L7.
pub fn render_flags_mismatch(
    m: &HsMethod<'_>,
    header: &str,
    target_bci: usize,
    current: &HsFrame,
    target: &VerificationFrame,
) -> String {
    let sm = HsFrame::declared(target, target_bci);
    render(m, header, target_bci, &Reason::FlagsMismatch, Some(current), Some(&sm))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method<'a>(code: &'a [u8]) -> HsMethod<'a> {
        HsMethod {
            class_name: "p/C",
            method_name: "m",
            descriptor: "()V",
            code,
            exception_table: &[],
            stack_map_raw: None,
        }
    }

    #[test]
    fn bad_type_block_matches_hotspot_layout() {
        // iconst_0; invokestatic #7; return
        let code = [0x03, 0xb8, 0x00, 0x07, 0xb1];
        let pre = HsFrame {
            bci: 1,
            this_uninit: false,
            locals: vec![],
            stack: vec![HsType::V(VType::Int)],
        };
        let fault = VerifyFault::BadStackType {
            index: 0,
            actual: HsType::V(VType::Int),
            expected: HsType::V(VType::ObjectRef("java/lang/Object".into())),
        };
        let text = render_fault(&method(&code), 1, &fault, &pre);
        let expected = "Bad type on operand stack\n\
Exception Details:\n  \
Location:\n    \
p/C.m()V @1: invokestatic\n  \
Reason:\n    \
Type integer (current frame, stack[0]) is not assignable to 'java/lang/Object'\n  \
Current Frame:\n    \
bci: @1\n    \
flags: { }\n    \
locals: { }\n    \
stack: { integer }\n  \
Bytecode:\n    \
0000000: 03b8 0007 b1";
        assert!(text.starts_with(expected), "{text}");
        // The last line is padded to sixteen byte columns.
        let last = text.lines().last().unwrap();
        assert_eq!(last.len(), "    0000000:".len() + 8 * 5);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn a_sixteen_byte_method_gets_a_blank_padding_line() {
        let code = [0u8; 16];
        let mut out = String::new();
        bytecode_text(&mut out, &code);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("    0000010:"));
        assert!(lines[1][12..].chars().all(|c| c == ' '));
    }

    #[test]
    fn frames_name_long_halves_and_trim_padding() {
        let mut frame = VerificationFrame::initial_frame("p/C", "m", "(J)V", true, 4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        let f = HsFrame::current(&frame, 0);
        assert_eq!(f.locals, vec![HsType::V(VType::Long), HsType::Long2nd]);
        assert_eq!(f.stack, vec![HsType::V(VType::Long), HsType::Long2nd]);
        let mut out = String::new();
        frame_text(&mut out, &f);
        assert!(out.contains("locals: { long, long_2nd }"), "{out}");
    }

    #[test]
    fn stack_map_entries_print_absolute_offsets() {
        // two entries: same_frame delta 7 (@7), append_frame delta 2 (+Integer) (@10)
        let raw = [0x00, 0x02, 0x07, 252, 0x00, 0x02, 0x01];
        let mut out = String::new();
        stack_map_text(&mut out, &raw);
        assert_eq!(
            out,
            "  Stackmap Table:\n    same_frame(@7)\n    append_frame(@10,Integer)\n"
        );
    }
}
