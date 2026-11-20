use super::push_frozen_peer_deopt_stash_roots;
use crate::types::ObjectRef;
use cratonvm_jit::deopt::{FrameValue, ReconstructedFrame, ResumeSemantics};

fn clear_stashes() {
    while cratonvm_jit::deopt::take_last_deopt().is_some() {}
    while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
}

/// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`): the
/// frozen-peer walk roots the objects a peer's stashed deopt frame holds,
/// read through the cells the peer published with its OS tid, and only
/// when the pause holds that tid frozen. The "frozen" peer is the test
/// thread itself (the walk only reads).
#[test]
fn gci_i9ds_frozen_peer_walk_roots_the_stashed_deopt_frames() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let obj = shared
        .mem
        .heap
        .alloc_object(crate::classloading::ClassId::new(0), 1);
    let state = crate::threading::jvm_thread::GcBlockState::new();
    let own = crate::threading::thread_registry::current_os_tid();
    let other = own.wrapping_add(1).max(1);
    let mut out: Vec<ObjectRef> = Vec::new();
    // Nothing published: nothing read, whatever is frozen.
    assert_eq!(
        push_frozen_peer_deopt_stash_roots(&state, &[own, other], &shared.mem.heap, &mut out),
        (0, 0)
    );
    crate::threading::thread_registry::publish_deopt_stash_cells_into(&state, own);
    if own == 0 {
        // No OS-tid backend on this target: nothing is published.
        assert_eq!(state.published_deopt_stash_cells(), None);
        return;
    }
    clear_stashes();
    cratonvm_jit::deopt::restash_last_deopt(ReconstructedFrame {
        class_id: 0,
        method_key: "gci/i9ds/Probe.m:()V".to_string(),
        bci: 3,
        locals: vec![FrameValue::Object(obj.as_ptr() as u64), FrameValue::Object(0)],
        // A non-heap word is screened out.
        stack: vec![FrameValue::Object(0x10)],
        semantics: ResumeSemantics::REEXECUTE,
        ..Default::default()
    });
    // A tid the pause does not hold frozen: the stash is never read.
    assert_eq!(
        push_frozen_peer_deopt_stash_roots(&state, &[other], &shared.mem.heap, &mut out),
        (0, 0)
    );
    assert!(out.is_empty());
    // Frozen: the stashed frame's object is a root.
    assert_eq!(
        push_frozen_peer_deopt_stash_roots(&state, &[other, own], &shared.mem.heap, &mut out),
        (1, 1)
    );
    assert_eq!(out.len(), 1);
    assert!(out[0] == obj, "the frozen peer's stashed object must be a root");
    // The read took nothing from the owner's stash.
    assert_eq!(cratonvm_jit::deopt::stashed_deopt_object_count(), 2);
    clear_stashes();
    out.clear();
    // An empty stash adds nothing.
    assert_eq!(
        push_frozen_peer_deopt_stash_roots(&state, &[own], &shared.mem.heap, &mut out),
        (0, 0)
    );
}
