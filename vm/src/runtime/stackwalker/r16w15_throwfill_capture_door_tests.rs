use super::{
    capture_throwable_backtrace_from_walk, capture_throwable_backtrace_without_store,
    throwable_capture_compiled_frames_with,
};
use crate::classloading::ClassId;
use crate::runtime::frame::Frame;

fn owned(class: &str, method: &str, descriptor: &str) -> Frame {
    Frame::new(
        ClassId::new(1),
        class.to_string(),
        method.to_string(),
        descriptor.to_string(),
        None,
        vec![0; 16],
        vec![],
        4,
        4,
        &[],
    )
}

fn names(trace: &[super::BacktraceFrame]) -> Vec<String> {
    trace
        .iter()
        .map(|f| {
            let e = f.to_entry();
            format!(
                "{}.{}{} @{}",
                e.class_name,
                e.method_name,
                e.method_descriptor.as_deref().unwrap_or(""),
                e.byte_code_index
            )
        })
        .collect()
}

/// Both walk modes, every depth cap, with and without the fill frames on
/// top: the door built on the caller's walk answers the one-call capture.
#[test]
fn the_door_on_a_scoped_walk_is_the_capture() {
    // A unit test has no compiled frames of its own; a JIT-enabled test
    // harness thread may, and then the walk is not this test's to pin.
    if !crate::jit::stack_trace::active_compiled_frames().is_empty() {
        return;
    }
    let frames = vec![
        owned("probe/Fill", "main", "([Ljava/lang/String;)V"),
        owned("probe/Fill", "depth", "(I)V"),
        owned("probe/Fill", "depth", "(I)V"),
        owned("probe/Fill", "thrower", "()V"),
        owned("java/lang/Throwable", "<init>", "(Ljava/lang/String;)V"),
        owned(
            "java/lang/Throwable",
            "fillInStackTrace",
            "()Ljava/lang/Throwable;",
        ),
    ];
    for max_depth in [0usize, 1, 2, 3, 1024] {
        for walk_scoped in [false, true] {
            let mut is_a = |_: Option<ClassId>, holder: &str| holder == "java/lang/Throwable";
            let jit =
                throwable_capture_compiled_frames_with(frames.len(), walk_scoped, max_depth);
            assert!(jit.is_empty());
            let door =
                capture_throwable_backtrace_from_walk(&frames, jit, max_depth, &mut is_a, &[]);
            let mut is_a = |_: Option<ClassId>, holder: &str| holder == "java/lang/Throwable";
            let whole =
                capture_throwable_backtrace_without_store(&frames, max_depth, &mut is_a, &[]);
            assert_eq!(names(&door), names(&whole), "{max_depth} {walk_scoped}");
            // The fill frames are trimmed in both.
            assert!(names(&door)
                .iter()
                .all(|n| !n.starts_with("java/lang/Throwable")));
        }
    }
}
