use super::*;

/// A span that burns CPU adds a positive figure where a clock exists.
/// Windows charges thread CPU per clock tick (~15.6 ms), so the span
/// burns until the thread's clock moves (bounded by wall time).
#[test]
fn a_span_adds_the_threads_cpu() {
    let total = AtomicU64::new(0);
    let start = current_thread_cpu_ns();
    let span = CpuSpan::begin();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut acc = 0u64;
    loop {
        for i in 0..1_000_000u64 {
            acc = acc.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(acc);
        if current_thread_cpu_ns() != start || std::time::Instant::now() > deadline {
            break;
        }
    }
    span.end(&total);
    if current_thread_cpu_ns().is_some() && gc_thread_cpu_on() {
        assert!(total.load(Ordering::Relaxed) > 0);
    }
}
