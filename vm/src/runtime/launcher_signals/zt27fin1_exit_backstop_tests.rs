use super::*;

/// The backstop is default ON with a `=0` kill; the stall knob is 0 unless
/// set to a number.


/// A job that started in time disarms the backstop: the thread returns
/// without acting (were it to act, the test process would end).
#[test]
fn zt27fin1_a_started_job_disarms_the_backstop() {
    let entered = Arc::new(AtomicBool::new(true));
    // An identity no VM has: the disposition would read `Shutdown`.
    arm_exit_backstop(usize::MAX, 15, &entered);
    std::thread::sleep(EXIT_BACKSTOP_POLL * 2);
}

/// Source pin: the job raises `entered` before it delivers, and the
/// dispatcher arms the backstop before it runs the job.
#[test]
fn zt27fin1_the_job_raises_entered_before_delivering() {
    let src = include_str!("../launcher_signals.rs").replace("\r\n", "\n");
    let arm = src
        .find(concat!(
            "arm_exit_backstop(shared.vm_",
            "identity, signal, &entered);"
        ))
        .expect("armed");
    let raise = src
        .find(concat!("entered.store(true, ", "Ordering::Release);"))
        .expect("raised");
    let deliver = src
        .find(concat!("deliver_host_signal(shared, ", "thread, signal);"))
        .expect("delivered");
    let run = src
        .find(concat!("crate::native::jni::run_", "attached_service("))
        .expect("run");
    assert!(arm < raise && raise < deliver && deliver < run);
}
