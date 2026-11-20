use super::{fused_capture_refusal, FusedRefusal};
use crate::classloading::ClassId;
use crate::jit::stack_trace::ActiveCompiledFrame;
use crate::runtime::frame::Frame;
use std::sync::Arc;

fn interp(class: &str, method: &str) -> Frame {
    Frame::new(
        ClassId::new(7),
        class.to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0; 16],
        vec![],
        4,
        4,
        &[],
    )
}

fn compiled(depth: u32, label: &str) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: depth,
        chain_index: 0,
        label: Arc::from(label),
        owner_class_id: 40,
        cm_ptr: 0,
        bci: 3,
        inline_chain: Vec::new(),
        chain_exact: true,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: Some(3),
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

/// The census's reason is the fused door's: an empty walk is the
/// interpreter window, and a walk of compiled frames under an unrelated
/// interpreter caller is laid out by the fused door itself.
#[test]
fn the_refusal_names_the_empty_walk_and_admits_a_plain_compiled_walk() {
    let frames = vec![interp("p/Main", "main")];
    assert_eq!(
        fused_capture_refusal(&frames, &[], &[]),
        Some(FusedRefusal::NoCompiledFrame)
    );
    let jit = vec![
        compiled(1, "p/Deep.altA:(II)J"),
        compiled(1, "p/Deep.altB:(II)J"),
        compiled(1, "p/Deep.altA:(II)J"),
    ];
    if super::compact_compiled_backtrace_enabled() {
        assert_eq!(fused_capture_refusal(&frames, &jit, &[]), None);
    } else {
        assert_eq!(
            fused_capture_refusal(&frames, &jit, &[]),
            Some(FusedRefusal::CompactOff)
        );
    }
}

/// The discriminants are the census columns, in its name order.
#[test]
fn the_refusal_columns_are_stable() {
    assert_eq!(FusedRefusal::NoCompiledFrame as usize, 0);
    assert_eq!(FusedRefusal::Splice as usize, 1);
    assert_eq!(FusedRefusal::CompactOff as usize, 2);
    assert_eq!(FusedRefusal::Rewritten as usize, 3);
    assert_eq!(
        crate::vm::realms::thread_realm::THROWFILL_SHAPE_FUSED_REFUSALS,
        4
    );
}
