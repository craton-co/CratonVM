use super::{derive_exec_depth_ceiling, init_thread_exec_depth_ceiling, EXEC_DEPTH_CEILING};

/// A repeat call with the same stack size (a virtual-thread carrier calls
/// it once per mount) keeps what is installed; a different size
/// re-derives. Interpreter round i1 wave 9, lane L5.
#[test]
fn a_repeat_init_with_the_same_stack_size_is_a_no_op() {
    let (kept, rederived) = std::thread::spawn(|| {
        init_thread_exec_depth_ceiling(8 * 1024 * 1024);
        EXEC_DEPTH_CEILING.with(|c| c.set(5));
        init_thread_exec_depth_ceiling(8 * 1024 * 1024);
        let kept = EXEC_DEPTH_CEILING.with(|c| c.get());
        init_thread_exec_depth_ceiling(16 * 1024 * 1024);
        (kept, EXEC_DEPTH_CEILING.with(|c| c.get()))
    })
    .join()
    .expect("ceiling probe thread must not panic");
    assert_eq!(kept, 5);
    assert_eq!(rederived, derive_exec_depth_ceiling(16 * 1024 * 1024));
}
