use super::*;
use cratonvm_gc_core::gpu_critical::{
    GPU_CRITICAL_COUNT, gpu_coordination, gpu_relocation_forbidden, wait_for_gpu_critical_drain,
};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn token_lifecycle_drives_global_counter() {
    let _serial = lock_test();
    let heap = VmHeap::new(GcBackend::Generational, 1 << 20);
    let live_count =
        || GPU_CRITICAL_COUNT.load(Ordering::Acquire) & !crate::safepoint::GPU_COLLECTING;
    let before = live_count();
    {
        let _t = heap.enter_gpu_critical();
        assert_eq!(live_count(), before + 1);
        assert_eq!(heap.gpu_critical_count(), before + 1);
        {
            let _t2 = heap.enter_gpu_critical();
            assert_eq!(live_count(), before + 2);
        }
        assert_eq!(live_count(), before + 1);
    }
    assert_eq!(live_count(), before);
}

/// Hold a direct token on a worker thread until signalled (bounded at 200 ms);
/// the drain on the main thread waits its budget, then GIVES UP and vetoes
/// relocation for the cycle instead of spinning until the token drops.
#[test]
fn drain_gives_up_after_its_budget_and_vetoes_relocation() {
    let _serial = lock_test();
    let heap = std::sync::Arc::new(VmHeap::new(GcBackend::Generational, 1 << 20));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let heap2 = heap.clone();
    let holder = std::thread::spawn(move || {
        let _t = heap2.enter_gpu_critical();
        started_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_millis(200));
    });
    started_rx.recv().unwrap();
    let budget = gpu_coordination::collector_wait_budget();
    let start = Instant::now();
    let _gpu_drain = wait_for_gpu_critical_drain();
    let elapsed = start.elapsed();
    assert!(
        elapsed >= budget,
        "drain returned in {elapsed:?}, before its {budget:?} budget"
    );
    assert!(
        elapsed < Duration::from_millis(190),
        "drain waited {elapsed:?} for a token held up to 200 ms: the wait is still unbounded"
    );
    assert!(
        gpu_relocation_forbidden(),
        "a wait that gave up must veto relocation for the cycle"
    );
    let _ = release_tx.send(());
    holder.join().unwrap();
    // What the collector does at the end of the cycle: clear the veto.
    drop(_gpu_drain);
    assert!(!gpu_relocation_forbidden());
}

/// zr z4/ab (`common-watched-referent-set-is-per-thread-not-per-heap-20261012.md` §4): VM B's
/// cycle ending must not clear the veto VM A's cycle raised while A's
/// cycle is still running. Before z4 the veto was one process flag that
/// any `after_collection` stored `false`.
#[test]
fn another_cycles_end_does_not_clear_this_cycles_veto() {
    let _serial = lock_test();
    let (vetoed_tx, vetoed_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    // Cycle A, on its own collecting thread: raises the veto mid-cycle.
    let a = std::thread::spawn(move || {
        let cycle = gpu_coordination::before_collection();
        gpu_coordination::forbid_relocation_this_cycle();
        vetoed_tx.send(()).unwrap();
        done_rx.recv().unwrap();
        let still = gpu_relocation_forbidden();
        cycle.after_collection(&[]);
        still
    });
    vetoed_rx.recv().unwrap();
    // Cycle B, here: starts and ends while A is mid-cycle.
    let b = gpu_coordination::before_collection();
    assert!(gpu_relocation_forbidden(), "A's veto covers B's cycle too");
    b.after_collection(&[]);
    assert!(
        gpu_relocation_forbidden(),
        "B's cycle end cleared A's veto while A was still running"
    );
    done_tx.send(()).unwrap();
    assert!(a.join().unwrap(), "A saw its own veto until its end");
    assert!(!gpu_relocation_forbidden(), "A's end withdrew it");
}
