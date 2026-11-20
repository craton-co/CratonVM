use super::live_trap_snapshot_fits_stack;
use crate::jit::conservative_roots::ActiveCompiledFrame;

fn frame(depth: u32, bci: i32) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: depth,
        chain_index: 0,
        label: std::sync::Arc::from("p/C.div0:(II)I"),
        owner_class_id: 1,
        cm_ptr: 0,
        bci,
        inline_chain: Vec::new(),
        chain_exact: false,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

/// The `Nio21ImplicitCatch` division window: `hot`'s OSR continuation
/// under the interpreted `call` (depth 2 of 3), the trapping `div0` above
/// it (depth 3). The ordinary stand-in test refuses this shape; a live
/// snapshot of it stands in.
#[test]
fn nio21_w18_a_live_snapshot_spanning_an_interpreter_frame_stands_in() {
    let snap = vec![frame(2, 40), frame(3, 4)];
    assert!(live_trap_snapshot_fits_stack(3, &snap));
    assert!(!crate::runtime::stackwalker::trap_snapshot_can_stand_in_for_walk(&[], &snap));
}

/// Empty, or a frame recorded above the stack as it is now: refused.
#[test]
fn nio21_w18_an_empty_or_too_deep_live_snapshot_is_refused() {
    assert!(!live_trap_snapshot_fits_stack(3, &[]));
    assert!(!live_trap_snapshot_fits_stack(
        3,
        &[frame(3, 4), frame(4, 1)]
    ));
    assert!(live_trap_snapshot_fits_stack(3, &[frame(3, 4)]));
}

/// The one door that builds with a live snapshot passes `true`, and the
/// shared doors keep `false` (a source pin: the property is the caller's).
#[test]
fn nio21_w18_only_the_compiled_local_handler_builds_live() {
    let helpers = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = helpers
        .split("unsafe fn local_handler_enter_implicit(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("local_handler_enter_implicit body");
    assert!(body.contains("drained.npe_action,"));
    assert!(body.contains("        true,\n    ) {"));
    assert!(helpers
        .contains("materialize_implicit_signal_or_frames(vm, thread, signal, trap_frames, npe_action, false)"));
}
