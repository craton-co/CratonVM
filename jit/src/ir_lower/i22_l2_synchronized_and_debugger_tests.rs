use super::{
    ir_artifact_hides_an_assigned_local, ir_artifact_hides_an_assigned_local_scoped,
    ir_entry_poll_mode_exits, lower_inner_with_mode_exit_polls, IrPollModeExits,
    PollExitBytecode,
};
use crate::deopt::{
    speculation_id, DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue,
    ResumeSemantics,
};
use crate::ir::{IrBuilder, IrInlineFrameSites, IrType};
use crate::{CompiledMethod, JitRuntimeHelpers};
use std::collections::{HashMap, HashSet};

/// `HelperFnSafepointSlowPath` that always answers the polling-body bit,
/// the one a method-entry exit leaves on.
extern "C" fn always_leave(_flag_addr: i64, _body_id: i64) -> i64 {
    crate::SAFEPOINT_VERDICT_POLLING_BODY
}

/// `static int f() { int i = 0; while (i < 1000) i++; return i; }`:
/// nothing in its graph can trap or call.
const COUNT: [u8; 19] = [
    0x03, // 0:  iconst_0
    0x3b, // 1:  istore_0
    0x1a, // 2:  iload_0             <- loop header
    0x11, 0x03, 0xe8, // 3:  sipush 1000
    0xa2, 0x00, 0x09, // 6:  if_icmpge +9 -> 15
    0x84, 0x00, 0x01, // 9:  iinc 0, 1
    0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
    0x1a, // 15: iload_0
    0xac, // 16: ireturn
    0, 0,
];

/// `static int g(int n) { int i = 0; while (i < 1000) i += 1000 / n;
/// return i; }`: the division by a parameter can trap.
const DIV_LOOP: [u8; 24] = [
    0x03, // 0:  iconst_0
    0x3c, // 1:  istore_1
    0x1b, // 2:  iload_1             <- loop header
    0x11, 0x03, 0xe8, // 3:  sipush 1000
    0xa2, 0x00, 0x0e, // 6:  if_icmpge +14 -> 20
    0x1b, // 9:  iload_1
    0x11, 0x03, 0xe8, // 10: sipush 1000
    0x1a, // 13: iload_0
    0x6c, // 14: idiv
    0x60, // 15: iadd
    0x3c, // 16: istore_1
    0xa7, 0xff, 0xf1, // 17: goto -15 -> 2
    0x1b, // 20: iload_1
    0xac, // 21: ireturn
    0, 0,
];

/// `static int h(int n) { int t = n * 2; int i = 0; while (i < n) i++;
/// return i; }`: `t` is assigned and never read again, so it is dead at
/// the header (6) but a debugger can still read it. `max_locals` 3.
const DEAD_TEMP: [u8; 21] = [
    0x1a, // 0:  iload_0
    0x05, // 1:  iconst_2
    0x68, // 2:  imul
    0x3c, // 3:  istore_1           (t, never read)
    0x03, // 4:  iconst_0
    0x3d, // 5:  istore_2
    0x1c, // 6:  iload_2             <- loop header
    0x1a, // 7:  iload_0
    0xa2, 0x00, 0x09, // 8:  if_icmpge +9 -> 17
    0x84, 0x02, 0x01, // 11: iinc 2, 1
    0xa7, 0xff, 0xf8, // 14: goto -8 -> 6
    0x1c, // 17: iload_2
    0xac, // 18: ireturn
    0, 0,
];

/// `static long w(long a) { return a; }`: one `long` parameter, two slots.
const WIDE_PARAM: [u8; 4] = [
    0x1e, // 0: lload_0
    0xad, // 1: lreturn
    0, 0,
];

fn dead_temp_bytecode() -> PollExitBytecode<'static> {
    PollExitBytecode {
        code: &DEAD_TEMP,
        code_len: DEAD_TEMP.len() - 2,
        num_params: 1,
        num_locals: 3,
        handlers: &[],
    }
}

/// An empty artifact carrying one point at `bci` with `locals`, in its
/// boxes (as a poll exit or an IR guard box) or in `deopt_points`.
fn artifact_with_point(bci: u32, locals: Vec<FrameValue>, boxed: bool) -> CompiledMethod {
    let mut buf = crate::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]); // ret; metadata only
    let mut cm = CompiledMethod::new(buf);
    let reason = DeoptReason::NullCheck;
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: DeoptAction::Reinterpret,
        speculation_id: speculation_id(bci, reason),
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci,
            locals,
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(reason),
    };
    if boxed {
        cm._deopt_point_boxes = vec![Box::new(point)];
    } else {
        cm.deopt_points = vec![point];
    }
    cm
}


/// The backstop's question on hand-made frames of `DEAD_TEMP`: `t` (slot
/// 1) published `Undefined` where it is assigned hides it, in the boxes
/// or in `deopt_points`; before its store, or described by a home, it does
/// not.
#[test]
fn an_undefined_assigned_local_is_a_hidden_one() {
    let hidden = vec![
        FrameValue::StackSlot(-8),
        FrameValue::Undefined,
        FrameValue::StackSlot(-24),
    ];
    for boxed in [true, false] {
        let at_header = artifact_with_point(6, hidden.clone(), boxed);
        assert!(
            ir_artifact_hides_an_assigned_local(&at_header, dead_temp_bytecode()),
            "t is assigned at the header (boxed {boxed})"
        );
    }
    let right_after_the_store = artifact_with_point(4, hidden.clone(), true);
    assert!(ir_artifact_hides_an_assigned_local(
        &right_after_the_store,
        dead_temp_bytecode()
    ));
    let before_the_store = artifact_with_point(3, hidden, true);
    assert!(
        !ir_artifact_hides_an_assigned_local(&before_the_store, dead_temp_bytecode()),
        "t is not assigned on entry to its own istore"
    );
    let described = artifact_with_point(
        6,
        vec![
            FrameValue::StackSlot(-8),
            FrameValue::StackSlot(-16),
            FrameValue::StackSlot(-24),
        ],
        true,
    );
    assert!(!ir_artifact_hides_an_assigned_local(
        &described,
        dead_temp_bytecode()
    ));
    // A code length past the buffer is not walked: fail closed.
    let past = PollExitBytecode {
        code_len: DEAD_TEMP.len() + 1,
        ..dead_temp_bytecode()
    };
    assert!(ir_artifact_hides_an_assigned_local(&described, past));
}

/// Round 13 wave 13 (lane jitfix;
/// `r13w12-snapliv-debugger-discard-check-ignores-chain-inner-scopes`): an
/// INNER scope of a caller chain is asked too. `DEAD_TEMP` (as `C.h`) is
/// spliced at combined pc 5 behind `static int o(int n) { return h(n); }`;
/// the callee frame publishing `t` `Undefined` at its header hides it once
/// the body table is given, and not before `t`'s store. Without the table
/// only the outer frame -- which hides nothing -- is asked, as before. A
/// scope the table does not name fails closed.
#[test]
fn r13w13_an_inner_chain_scope_hiding_an_assigned_local_is_seen() {
    // 0: iload_0  1: invokestatic #1  4: ireturn
    const OUTER: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let outer = PollExitBytecode {
        code: &OUTER,
        code_len: 5,
        num_params: 1,
        num_locals: 1,
        handlers: &[],
    };
    let mut combined: Vec<u8> = OUTER[..5].to_vec();
    combined.extend_from_slice(&DEAD_TEMP);
    let mut sites = std::collections::HashMap::new();
    sites.insert(
        1usize,
        crate::ir::IrInlineSite {
            base: 5,
            code_len: DEAD_TEMP.len() - 2,
            num_args: 1,
            max_locals: 3,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "C.h:(I)I".to_string(),
            class_id: 0,
        },
    );
    let bodies = crate::ir::IrInlineFrameSites::from_sites(&sites);
    let chained = |callee_key: &str, callee_bci: u32| {
        let mut cm = artifact_with_point(1, Vec::new(), true);
        cm._deopt_point_boxes[0].frame_state = FrameState {
            class_id: 0,
            method_key: callee_key.to_string(),
            bci: callee_bci,
            locals: vec![
                FrameValue::StackSlot(-8),
                FrameValue::Undefined,
                FrameValue::StackSlot(-24),
            ],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: Some(Box::new(FrameState {
                class_id: 0,
                method_key: String::new(),
                bci: 1,
                locals: vec![FrameValue::StackSlot(-32)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            })),
        };
        cm
    };
    let spliced = Some((combined.as_slice(), &bodies));
    let at_header = chained("C.h:(I)I", 6);
    assert!(
        !ir_artifact_hides_an_assigned_local(&at_header, outer),
        "the outermost scope alone hides nothing"
    );
    assert!(
        ir_artifact_hides_an_assigned_local_scoped(&at_header, outer, spliced),
        "h's `t` is assigned at its header"
    );
    assert!(
        !ir_artifact_hides_an_assigned_local_scoped(&chained("C.h:(I)I", 3), outer, spliced),
        "`t` is not assigned on entry to its own istore"
    );
    assert!(
        ir_artifact_hides_an_assigned_local_scoped(&chained("C.x:()V", 6), outer, spliced),
        "a scope the body table does not name fails closed"
    );
}

/// The upper half of a two-slot value is `Undefined` by design, although
/// the assigned table calls both slots of a `long` parameter assigned.
#[test]
fn the_upper_half_of_a_long_is_not_a_hidden_local() {
    let wide = |cm: &CompiledMethod| {
        ir_artifact_hides_an_assigned_local(
            cm,
            PollExitBytecode {
                code: &WIDE_PARAM,
                code_len: WIDE_PARAM.len() - 2,
                num_params: 2,
                num_locals: 2,
                handlers: &[],
            },
        )
    };
    for value in [
        FrameValue::StackSlotLong(-8),
        FrameValue::RegisterLong(3),
        FrameValue::Long(7),
    ] {
        let cm = artifact_with_point(0, vec![value.clone(), FrameValue::Undefined], true);
        assert!(!wide(&cm), "{value:?} then its upper half");
    }
    let lost = artifact_with_point(0, vec![FrameValue::Undefined, FrameValue::Undefined], true);
    assert!(wide(&lost), "the parameter itself is hidden");
}

