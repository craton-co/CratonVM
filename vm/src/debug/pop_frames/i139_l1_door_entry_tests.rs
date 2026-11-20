use super::{DoorEntryScope, DOOR_ENTRY_DEPTH};

fn mark() -> usize {
    DOOR_ENTRY_DEPTH.with(std::cell::Cell::get)
}

#[test]
fn the_door_mark_names_the_innermost_door_and_nested_scopes_restore_it() {
    assert_eq!(mark(), 0);
    {
        let _outer = DoorEntryScope::enter(4);
        assert_eq!(mark(), 4);
        {
            let _inner = DoorEntryScope::enter(9);
            assert_eq!(mark(), 9);
        }
        assert_eq!(mark(), 4);
    }
    assert_eq!(mark(), 0);
}

#[test]
fn the_loop_pushes_the_door_method_again_before_the_ordinary_pop_answer() {
    let src = concat!(include_str!("../../runtime/interpreter/mod.rs"), "\n", include_str!("../../runtime/interpreter/cached_invoke_target.rs"), "\n", include_str!("../../runtime/interpreter/execute.rs"), "\n", include_str!("../../runtime/interpreter/resume_continuation.rs"), "\n", include_str!("../../runtime/interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../runtime/interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../runtime/interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../runtime/interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let restart = src.find("crate::debug::pop_frames::take_door_restart(thread)");
    let popped = src.find("if thread.frames.len() < depth {\n                return Some(DebuggerStop::FramesPopped);");
    assert!(restart.is_some() && popped.is_some() && restart < popped);
}
