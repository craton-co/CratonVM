use super::{stamp_direct_throw_bci, ActiveCompiledFrame, InlinedLevel};

fn artifact(sp_id_slot_off: i32, used_ir_backend: bool) -> cratonvm_jit::CompiledMethod {
    let mut cm = cratonvm_jit::CompiledMethod::new(
        cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests"),
    );
    cm.sp_id_slot_off = sp_id_slot_off;
    cm.used_ir_backend = used_ir_backend;
    cm
}

/// The shape `active_compiled_frames` builds for an innermost frame whose
/// slot held `SP_ID_UNSET_BC_PC` -- or a stale id with a stale chain.
fn frame(cm: &cratonvm_jit::CompiledMethod, depth: u32, bci: i32) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: depth,
        chain_index: 0,
        label: std::sync::Arc::from("p/R.rec:(II)I"),
        owner_class_id: 1,
        cm_ptr: cm as *const cratonvm_jit::CompiledMethod as usize,
        bci,
        inline_chain: vec![InlinedLevel {
            label: std::sync::Arc::from("p/R.stale:()V"),
            bci: 3,
            class_id: 1,
            hidden: None,
        }],
        chain_exact: false,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

/// `R11RtFibTraceLines`: the innermost `rec(0, d)` frame has made no call,
/// so its slot holds the unset sentinel and the walk answered `-1`. The
/// stub's stamp is the division's own bci and must win.
#[test]
fn the_stamped_bci_places_the_frame_that_raised() {
    let cm = artifact(24, false);
    let mut frames = vec![frame(&cm, 2, 9), frame(&cm, 2, -1)];
    assert!(stamp_direct_throw_bci(&mut frames, 5, 2));
    assert_eq!(frames[1].bci, 5);
    assert!(
        frames[1].inline_chain.is_empty(),
        "a stale chain must not survive"
    );
    assert!(
        !frames[1].trap_site_exact,
        "only an NPE trap site feeds JEP 358"
    );
    // Only the innermost frame is the one that raised.
    assert_eq!(frames[0].bci, 9);
    assert_eq!(frames[0].inline_chain.len(), 1);
}

/// A stale-but-valid id (the frame made a call before dividing) is
/// overwritten too: the walk's answer there was a real, wrong line.
#[test]
fn a_stale_slot_answer_is_replaced() {
    let cm = artifact(24, false);
    let mut frames = vec![frame(&cm, 0, 40)];
    assert!(stamp_direct_throw_bci(&mut frames, 12, 0));
    assert_eq!(frames[0].bci, 12);
}

#[test]
fn nothing_stamped_or_an_out_of_spec_bci_changes_nothing() {
    let cm = artifact(24, false);
    for stamped in [-1_i64, 65_536, i64::from(u32::MAX - 1), i64::MIN] {
        let mut frames = vec![frame(&cm, 0, -1)];
        assert!(
            !stamp_direct_throw_bci(&mut frames, stamped, 0),
            "{stamped}"
        );
        assert_eq!(frames[0].bci, -1);
        assert_eq!(frames[0].inline_chain.len(), 1);
    }
}

/// Frames from an OUTER chain entry, an optimizing-tier artifact and an
/// artifact with no slot (non-precise, aarch64) are all mismatches: the
/// stamp belongs to a single-pass frame the snapshot does not end with.
#[test]
fn a_frame_the_stub_cannot_belong_to_is_left_alone() {
    let single_pass = artifact(24, false);
    let ir = artifact(24, true);
    let no_slot = artifact(0, false);
    let mut outer = vec![frame(&single_pass, 1, -1)];
    assert!(!stamp_direct_throw_bci(&mut outer, 5, 2));
    let mut ir_top = vec![frame(&ir, 0, -1)];
    assert!(!stamp_direct_throw_bci(&mut ir_top, 5, 0));
    let mut slotless = vec![frame(&no_slot, 0, -1)];
    assert!(!stamp_direct_throw_bci(&mut slotless, 5, 0));
    let mut empty: Vec<ActiveCompiledFrame> = Vec::new();
    assert!(!stamp_direct_throw_bci(&mut empty, 5, 0));
    for f in outer.iter().chain(&ir_top).chain(&slotless) {
        assert_eq!(f.bci, -1);
    }
}

/// JIT round 20 wave 7 (lane fuzzC; `R20FuzzCAthrowNull1`): the `athrow`
/// stamp also places an optimizing-tier frame (its `Op::Throw` passes its
/// own bci), and keeps every other refusal.
#[test]
fn the_athrow_stamp_admits_an_optimizing_tier_frame() {
    use super::stamp_throw_bci;
    let ir = artifact(24, true);
    let mut ir_top = vec![frame(&ir, 0, 40)];
    assert!(stamp_throw_bci(&mut ir_top, 7, 0, true));
    assert_eq!(ir_top[0].bci, 7);
    assert!(ir_top[0].inline_chain.is_empty());
    assert!(!ir_top[0].trap_site_exact);
    let no_slot = artifact(0, true);
    let mut slotless = vec![frame(&no_slot, 0, -1)];
    assert!(!stamp_throw_bci(&mut slotless, 7, 0, true));
    let mut outer = vec![frame(&ir, 1, -1)];
    assert!(!stamp_throw_bci(&mut outer, 7, 2, true));
    let mut unstamped = vec![frame(&ir, 0, 40)];
    assert!(!stamp_throw_bci(&mut unstamped, -1, 0, true));
    assert_eq!(unstamped[0].bci, 40);
}
