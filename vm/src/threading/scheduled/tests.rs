use super::*;
use std::thread;

#[test]
fn elapsed_ticks_starts_at_zero() {
    let t = ScheduledTask {
        id: 1,
        start: Instant::now(),
        initial_delay_ms: 0,
        period_ms: 50,
        tick_count: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
    };
    // Right after registration the immediate (initial_delay==0) tick
    // is already due, so elapsed_ticks() returns ≥ 1.
    assert!(t.elapsed_ticks() >= 1);
}

#[test]
fn periodic_accrues_ticks_over_time() {
    let reg = ScheduledRegistry::new();
    let task = reg.register(0, 50);
    thread::sleep(Duration::from_millis(220));
    let drained = reg.drain_due_ticks();
    assert_eq!(drained.len(), 1);
    let (t, n) = &drained[0];
    assert_eq!(t.id, task.id);
    // 220ms / 50ms ≈ 4-5 ticks (tolerate scheduler jitter).
    assert!(*n >= 3 && *n <= 7, "expected 3..=7 ticks, got {n}");
}

#[test]
fn cancel_stops_accrual() {
    let reg = ScheduledRegistry::new();
    let task = reg.register(0, 30);
    thread::sleep(Duration::from_millis(100));
    let _ = reg.drain_due_ticks();
    let acked_before = task.tick_count.load(Ordering::Acquire);
    assert!(task.cancel());
    thread::sleep(Duration::from_millis(100));
    let drained = reg.drain_due_ticks();
    assert_eq!(drained.len(), 0);
    // tick_count after cancel() must not advance.
    let acked_after = task.tick_count.load(Ordering::Acquire);
    assert_eq!(acked_before, acked_after);
}

#[test]
fn gc_removes_cancelled_tasks() {
    let reg = ScheduledRegistry::new();
    let t1 = reg.register(0, 50);
    let _t2 = reg.register(0, 50);
    t1.cancel();
    assert_eq!(reg.live_count(), 1);
    reg.gc();
    // live_count is unchanged after gc(): both still in the list, only
    // one is live.
    let count_after = reg.tasks.lock().len();
    assert_eq!(count_after, 1, "cancelled task should be GC'd");
}

#[test]
fn one_shot_caps_at_one_tick() {
    let task = ScheduledTask {
        id: 1,
        start: Instant::now() - Duration::from_millis(500),
        initial_delay_ms: 50,
        period_ms: 0,
        tick_count: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
    };
    // 500ms elapsed, 50ms initial_delay, 0 period → exactly 1 tick.
    assert_eq!(task.elapsed_ticks(), 1);
}

#[test]
fn cancel_all_marks_every_task() {
    let reg = ScheduledRegistry::new();
    let _t1 = reg.register(0, 30);
    let _t2 = reg.register(0, 30);
    reg.cancel_all();
    assert_eq!(reg.live_count(), 0);
}

#[test]
fn registry_singleton() {
    let r1 = registry() as *const _;
    let r2 = registry() as *const _;
    assert_eq!(r1, r2);
}
