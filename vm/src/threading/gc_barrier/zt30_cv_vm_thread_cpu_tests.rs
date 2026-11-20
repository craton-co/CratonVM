use super::*;

/// One pause's share: the same thread's CPU delta, nothing for a pause
/// another thread completed or a clock that could not be read, never
/// negative.
#[test]
fn zt30_cv_a_pause_books_its_initiators_cpu_delta_only() {
    assert_eq!(
        vm_op_cpu_share(Some((7, 1_000)), Some(7), Some(4_500)),
        3_500
    );
    assert_eq!(vm_op_cpu_share(Some((7, 1_000)), Some(8), Some(4_500)), 0);
    assert_eq!(vm_op_cpu_share(Some((7, 1_000)), Some(7), None), 0);
    assert_eq!(vm_op_cpu_share(None, Some(7), Some(4_500)), 0);
    assert_eq!(vm_op_cpu_share(Some((7, 9_000)), Some(7), Some(4_500)), 0);
}

/// Spin `ms` milliseconds of this thread's CPU. Counted on the thread's
/// own CPU clock where there is one: on a loaded host a wall-clock spin
/// can get a fraction of a core (8 ms of CPU for a 20 ms spin on a host
/// at load 23 of 16 cores), which failed the 10 ms floor below. The wall
/// cap keeps a clock that does not advance from hanging the test.
fn burn(ms: u64) {
    let wall_cap = Instant::now() + Duration::from_millis(ms * 20);
    let start = cratonvm_gc::thread_cpu::current_thread_cpu_ns();
    let until = Instant::now() + Duration::from_millis(ms);
    let mut x = 0u64;
    loop {
        for _ in 0..1024 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(1));
        }
        let done = match (start, cratonvm_gc::thread_cpu::current_thread_cpu_ns()) {
            (Some(s), Some(now)) => now.saturating_sub(s) >= ms * 1_000_000,
            _ => Instant::now() >= until,
        };
        if done || Instant::now() >= wall_cap {
            break;
        }
    }
}


