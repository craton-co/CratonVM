//! Round 13 wave 9 (lane chain4): the self-lock hand-over's one shared
//! question (`point_hands_self_lock_over`) and the body check built on it
//! (`Compiler::self_lock_deopt_exits_hand_over`).

use super::{point_hands_self_lock_over, self_lock_method_monitor};
use crate::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue, MonitorInfo,
    ResumeSemantics,
};
use crate::x64::Compiler;

const OBJ_OFF: i32 = 8;

fn point(reason: DeoptReason, monitors: Vec<MonitorInfo>) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci: 4,
        reason,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "S.m:()I".to_string(),
            bci: 4,
            locals: vec![FrameValue::StackSlotRef(-OBJ_OFF)],
            stack: Vec::new(),
            monitors,
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    }
}

fn test_compiler() -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut c = Compiler::new(
        "r13w9-chain4-test".to_string(),
        crate::ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        cratonvm_jit_api::JitRuntimeHelpers::default(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );
    c.self_lock_obj_off = OBJ_OFF;
    c
}

/// File `p` as the snapshot a reason-`stub` stub at `site_pc` bakes.
fn file(c: &mut Compiler, site_pc: usize, stub: i64, p: DeoptimizationPoint) {
    let boxed = Box::new(p.clone());
    let key: *const DeoptimizationPoint = std::ptr::addr_of!(*boxed);
    // A fixture seeding the map, not an emitter filing site: `extend`
    // keeps it out of `r10_bybci_publication_key_space`'s census.
    c.deopt_box_ptr_by_bci.extend([((site_pc, p.reason), key)]);
    c.deopt_boxes.push(boxed);
    c.deopt_points.push(p);
    c.deopt_stubs.push((0, site_pc, stub));
}

#[test]
fn only_the_root_scope_self_lock_monitor_hands_over() {
    let handing = point(
        DeoptReason::ReceiverTypeChanged,
        vec![self_lock_method_monitor(OBJ_OFF)],
    );
    assert!(point_hands_self_lock_over(&handing, OBJ_OFF));
    assert!(
        !point_hands_self_lock_over(&handing, 0),
        "not a self-locking body"
    );
    assert!(
        !point_hands_self_lock_over(&handing, 16),
        "another frame word"
    );
    let bare = point(DeoptReason::ReceiverTypeChanged, Vec::new());
    assert!(!point_hands_self_lock_over(&bare, OBJ_OFF));
    let mut two = handing.clone();
    two.frame_state.monitors.push(MonitorInfo {
        object: FrameValue::StackSlotRef(-24),
        lock_depth: 1,
        relock: false,
    });
    assert!(!point_hands_self_lock_over(&two, OBJ_OFF));
    let mut chain = handing.clone();
    chain.frame_state.caller = Some(Box::new(bare.frame_state.clone()));
    assert!(!point_hands_self_lock_over(&chain, OBJ_OFF));
}


const SLOT: usize = 0x0123_4567_89AB_CDF0;
const EXIT: usize = 0x0123_4567_89AB_CDE7;
const SCRATCH_OFF: i32 = 24;


/// Emit the frame-deopt stubs of a static self-locking body with one
/// reason-6 guard whose point is `p`; the bytes past the body.
fn static_stub_bytes(p: DeoptimizationPoint) -> Vec<u8> {
    let mut c = super::super::emit::tests::test_compiler();
    c.direct_helpers.monitor_exit = EXIT;
    c.needs_heap = true;
    c.heap_local_offset = 16;
    c.self_lock_obj_off = SCRATCH_OFF;
    c.self_lock_mirror_slot = SLOT;
    c.self_lock_release_armed = true;
    c.deopt_regs_base = 512;
    // The guard's rel32, patched to the stub.
    let patch = c.buf.pos();
    c.buf.emit(&[0, 0, 0, 0]);
    let boxed = Box::new(p.clone());
    let key: *const DeoptimizationPoint = std::ptr::addr_of!(*boxed);
    c.deopt_box_ptr_by_bci.extend([((10, p.reason), key)]);
    c.deopt_boxes.push(boxed);
    c.deopt_points.push(p);
    c.deopt_stubs.push((patch, 10, 6));
    let start = c.buf.pos();
    c.emit_deopt_stubs();
    assert!(!c.failed);
    c.buf.as_slice()[start..].to_vec()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Round 13 wave 11 (lane sync7): the hand-over tail of a static body
/// loads the class mirror into the scratch word the point names (the
/// word holds `0` between uses) and leaves without releasing; a guard
/// whose point does not name the hold takes the ordinary tail, whose
/// armed epilogue releases.
#[test]
fn the_static_handover_tail_loads_the_mirror_and_keeps_the_hold() {
    // `MOV R11, imm64 SLOT`: the refresh's first instruction.
    let mut refresh = vec![0x49, 0xBB];
    refresh.extend_from_slice(&SLOT.to_le_bytes());
    let handing = static_stub_bytes(point(
        DeoptReason::ReceiverTypeChanged,
        vec![self_lock_method_monitor(SCRATCH_OFF)],
    ));
    assert!(
        find(&handing, &refresh).is_some(),
        "the word is loaded before the entry"
    );
    assert_eq!(
        find(&handing, &EXIT.to_le_bytes()),
        None,
        "the hold is handed over"
    );
    let plain = static_stub_bytes(point(DeoptReason::ReceiverTypeChanged, Vec::new()));
    assert!(
        find(&plain, &EXIT.to_le_bytes()).is_some(),
        "a stub whose point names no hold releases it"
    );
}
