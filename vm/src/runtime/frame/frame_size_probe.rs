use super::*;

/// Not an assertion — a measurement printed so the fixed per-call cost can
/// be reasoned about with a number instead of an estimate. `Frame` is moved
/// by value into `FrameStack::push` on EVERY call, so its size is memory
/// traffic paid per invocation.
#[test]
fn report_frame_size() {
    eprintln!("size_of::<Frame>()      = {}", std::mem::size_of::<Frame>());
    eprintln!(
        "size_of::<FrameInner>() = {}",
        std::mem::size_of::<FrameInner>()
    );
    eprintln!(
        "size_of::<ValueStack>() = {}",
        std::mem::size_of::<ValueStack>()
    );
    eprintln!(
        "align_of::<Frame>()     = {}",
        std::mem::align_of::<Frame>()
    );
}
