use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Each test allocates a unique synthetic "object address" so the
/// global tables stay disjoint between tests.
fn fresh_addr() -> usize {
    static N: AtomicUsize = AtomicUsize::new(0x1000);
    // Spread addresses out by 64 to dodge any chance of accidental
    // collision with cache-line-rounded keys elsewhere.
    N.fetch_add(64, Ordering::SeqCst)
}

// --- StampedLock ---

#[test]
fn stamped_init_returns_origin() {
    let a = fresh_addr();
    stamped_init(a);
    assert_eq!(stamped_try_optimistic_read(a), STAMPED_ORIGIN);
}

/// The regression this whole module was re-encoded for (2026-08-07).
///
/// Measured on HotSpot 25.0.3 with `SLBits.java`:
///     first writeLock() = 384, & 255 = 128
///     isWriteLockStamp -> true, isReadLockStamp -> false
///     first readLock()  & 255 = 1  (the reader COUNT, not a flag)
///     tryOptimisticRead & 255 = 0
///
/// `isWriteLockStamp` and friends are unregistered `public static`
/// methods, so REAL JDK BYTECODE decodes these values. If this test ever
/// goes red the canonical release idiom takes the wrong branch again and
/// write holds leak.
#[test]
fn stamped_write_stamp_uses_the_real_jdk_bit_layout() {
    let a = fresh_addr();
    stamped_init(a);
    let s = stamped_write_lock(a);
    assert_eq!(s, STAMPED_ORIGIN | WBIT, "first write stamp must be 384");
    assert_eq!(s & ABITS, WBIT, "write stamp mode field must be WBIT");
    assert!(is_write_stamp(s), "JDK isWriteLockStamp must say write");
    assert!(!is_read_stamp(s), "JDK isReadLockStamp must NOT say read");
    assert!(!is_optimistic_stamp(s));
    assert!(stamped_unlock_write(a, s));
}

#[test]
fn stamped_read_and_optimistic_stamps_use_the_real_jdk_bit_layout() {
    let a = fresh_addr();
    stamped_init(a);
    let o = stamped_try_optimistic_read(a);
    assert_eq!(o, STAMPED_ORIGIN);
    assert_eq!(o & ABITS, 0, "optimistic stamp mode field must be 0");
    assert!(is_optimistic_stamp(o));
    assert!(!is_read_stamp(o) && !is_write_stamp(o));

    // Two concurrent read holds get DIFFERENT stamps whose low bits are
    // the reader count, exactly like the JDK (513, 514, ...).
    let r1 = stamped_read_lock(a);
    let r2 = stamped_read_lock(a);
    assert_eq!(r1 & ABITS, 1);
    assert_eq!(r2 & ABITS, 2);
    assert_eq!(r2 - r1, RUNIT);
    for r in [r1, r2] {
        assert!(is_read_stamp(r));
        assert!(!is_write_stamp(r));
        assert!(!is_optimistic_stamp(r));
    }
    assert!(stamped_unlock_read(a, r2));
    assert!(stamped_unlock_read(a, r1));
}

#[test]
fn stamped_unlock_write_advances_stamp() {
    let a = fresh_addr();
    stamped_init(a);
    let s0 = stamped_try_optimistic_read(a);
    let w = stamped_write_lock(a);
    assert!(stamped_unlock_write(a, w));
    let s1 = stamped_try_optimistic_read(a);
    assert!(s1 > s0, "stamp must advance: {} -> {}", s0, s1);
    // HotSpot advances the version by exactly one step per write release.
    assert_eq!(s1 - s0, VERSION_STEP);
    assert_eq!(s1 & ABITS, 0, "post-unlock stamp must be optimistic");
}

/// A stale or foreign write stamp must NOT release a live write hold —
/// the JDK's guard is an exact state-word match. Before 2026-08-07
/// `unlockWrite` ignored its stamp entirely.
#[test]
fn stamped_unlock_write_rejects_a_stale_stamp() {
    let a = fresh_addr();
    stamped_init(a);
    let w0 = stamped_write_lock(a);
    assert!(stamped_unlock_write(a, w0));
    // Same stamp, second time: the version has moved on.
    assert!(!stamped_unlock_write(a, w0));
    // A live hold taken by "someone else" is not releasable with w0.
    let w1 = stamped_write_lock(a);
    assert!(!stamped_unlock_write(a, w0));
    assert!(stamped_is_write_locked(a), "stale stamp must not release");
    // Nor with a read or optimistic stamp.
    assert!(!stamped_unlock_write(a, w1 & VERSION_MASK));
    assert!(!stamped_unlock_write(a, 0));
    assert!(stamped_unlock_write(a, w1));
}

#[test]
fn stamped_unlock_read_rejects_a_non_read_stamp() {
    let a = fresh_addr();
    stamped_init(a);
    let r = stamped_read_lock(a);
    assert!(!stamped_unlock_read(a, 0), "zero is never a read stamp");
    assert!(
        !stamped_unlock_read(a, r & VERSION_MASK),
        "an optimistic stamp holds nothing"
    );
    // WBIT is inside SBITS, so setting it makes the stamp name a
    // different lock state — the JDK rejects it and so must we.
    assert!(
        !stamped_unlock_read(a, r | WBIT),
        "SBITS mismatch must be rejected"
    );
    assert_eq!(stamped_get_read_lock_count(a), 1);
    assert!(stamped_unlock_read(a, r));
    assert!(!stamped_unlock_read(a, r), "no hold left to release");
}

#[test]
fn stamped_validate_zero_is_invalid() {
    let a = fresh_addr();
    stamped_init(a);
    assert!(!stamped_validate(a, 0));
}

#[test]
fn stamped_validate_after_write_fails() {
    let a = fresh_addr();
    stamped_init(a);
    let s = stamped_try_optimistic_read(a);
    let w = stamped_write_lock(a);
    assert!(stamped_unlock_write(a, w));
    assert!(!stamped_validate(a, s));
}

#[test]
fn stamped_try_write_blocked_by_reader() {
    let a = fresh_addr();
    stamped_init(a);
    let r = stamped_read_lock(a);
    assert_eq!(stamped_try_write_lock(a), 0);
    assert!(stamped_unlock_read(a, r));
}

#[test]
fn stamped_try_read_blocked_by_writer() {
    let a = fresh_addr();
    stamped_init(a);
    let w = stamped_write_lock(a);
    assert_eq!(stamped_try_read_lock(a), 0);
    assert!(stamped_unlock_write(a, w));
}

#[test]
fn stamped_convert_to_write_succeeds_with_one_reader() {
    let a = fresh_addr();
    stamped_init(a);
    let r = stamped_read_lock(a);
    let w = stamped_try_convert_to_write(a, r);
    assert!(w != 0);
    assert!(is_write_stamp(w));
    assert!(stamped_is_write_locked(a));
    assert_eq!(stamped_get_read_lock_count(a), 0);
    assert!(stamped_unlock_write(a, w));
}

#[test]
fn stamped_convert_to_write_fails_with_two_readers() {
    let a = fresh_addr();
    stamped_init(a);
    let r1 = stamped_read_lock(a);
    let r2 = stamped_read_lock(a);
    assert_eq!(stamped_try_convert_to_write(a, r1), 0);
    assert!(stamped_unlock_read(a, r1));
    assert!(stamped_unlock_read(a, r2));
}

#[test]
fn stamped_writer_blocks_then_releases() {
    // Cross-thread test: take write, spawn reader (blocks), release
    // write, reader makes progress.
    let a = fresh_addr();
    stamped_init(a);
    let w = stamped_write_lock(a);
    let join = std::thread::spawn(move || {
        let s = stamped_read_lock(a);
        assert!(stamped_unlock_read(a, s));
        s
    });
    // give the reader a moment to be queued
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(stamped_unlock_write(a, w));
    let stamp = join.join().unwrap();
    // The reader's stamp is a read stamp at the POST-release version.
    assert!(is_read_stamp(stamp));
    assert_eq!(stamp & VERSION_MASK, (w & VERSION_MASK) + VERSION_STEP);
}

#[test]
fn stamped_no_lost_wakeup_under_contention() {
    // Stress the wait/notify edge: many readers contending against a
    // single writer that bursts on/off. If the older
    // monitor_enter/wait/exit pattern lost a notification, this would
    // hang past the test timeout. With Mutex+Condvar held across the
    // check, every waker is observed.
    let a = fresh_addr();
    stamped_init(a);
    let mut handles = vec![];
    for _ in 0..8 {
        handles.push(std::thread::spawn(move || {
            for _ in 0..200 {
                let s = stamped_read_lock(a);
                // A writer cannot advance the version while a read hold
                // is outstanding, so this stamp is still current.
                assert!(stamped_unlock_read(a, s), "read stamp went stale");
            }
        }));
    }
    // Writer churns the lock alongside the readers.
    for _ in 0..50 {
        let w = stamped_write_lock(a);
        assert!(stamped_unlock_write(a, w));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(stamped_get_read_lock_count(a), 0);
}

/// The Agroal `StampedCopyOnWriteArrayList` shape: `writeLock()` is
/// released ONLY by `tryConvertToOptimisticRead(stamp)`. Before this was
/// implemented natively the write hold leaked and the next `readLock()`
/// blocked forever (Keycloak 26.6.1 boot deadlock in JPAConfig.startAll).
#[test]
fn stamped_convert_to_optimistic_releases_the_write_lock() {
    let a = fresh_addr();
    stamped_init(a);
    let w = stamped_write_lock(a);
    assert!(stamped_is_write_locked(a));
    let obs = stamped_try_convert_to_optimistic(a, w);
    assert!(obs != 0, "converting a held write stamp must succeed");
    assert!(!stamped_is_write_locked(a), "write lock must be released");
    assert!(stamped_validate(a, obs));
    // The whole point: a reader must now get through without blocking.
    let r = stamped_read_lock(a);
    assert!(is_read_stamp(r));
    assert!(stamped_unlock_read(a, r));
}

#[test]
fn stamped_convert_to_optimistic_releases_a_read_hold() {
    let a = fresh_addr();
    stamped_init(a);
    let r = stamped_read_lock(a);
    assert_eq!(stamped_get_read_lock_count(a), 1);
    let obs = stamped_try_convert_to_optimistic(a, r);
    assert!(obs != 0);
    assert_eq!(stamped_get_read_lock_count(a), 0);
    // A writer can now take the lock without blocking.
    let w = stamped_try_write_lock(a);
    assert!(w != 0);
    assert!(stamped_unlock_write(a, w));
}

#[test]
fn stamped_convert_to_optimistic_on_stale_stamp_is_zero() {
    let a = fresh_addr();
    stamped_init(a);
    let s = stamped_try_optimistic_read(a);
    let w = stamped_write_lock(a);
    assert!(stamped_unlock_write(a, w));
    assert_eq!(stamped_try_convert_to_optimistic(a, s), 0);
    assert_eq!(stamped_try_convert_to_optimistic(a, 0), 0);
}

#[test]
fn stamped_read_and_optimistic_stamps_are_distinguishable() {
    let a = fresh_addr();
    stamped_init(a);
    let opt = stamped_try_optimistic_read(a);
    let rd = stamped_read_lock(a);
    assert_ne!(
        opt, rd,
        "an optimistic stamp must not look like a read stamp"
    );
    // Converting the OPTIMISTIC stamp must not steal the reader's hold.
    assert!(stamped_try_convert_to_optimistic(a, opt) != 0);
    assert_eq!(stamped_get_read_lock_count(a), 1);
    assert!(stamped_unlock_read(a, rd));
}

#[test]
fn stamped_timed_try_write_lock_times_out_under_a_reader() {
    let a = fresh_addr();
    stamped_init(a);
    let r = stamped_read_lock(a);
    let t0 = std::time::Instant::now();
    assert_eq!(stamped_try_write_lock_timed(a, 20_000_000), 0);
    assert!(t0.elapsed() >= std::time::Duration::from_millis(15));
    assert!(stamped_unlock_read(a, r));
    let w = stamped_try_write_lock_timed(a, 20_000_000);
    assert!(w != 0);
    assert!(stamped_unlock_write(a, w));
}

#[test]
fn stamped_convert_to_read_downgrades_a_write_hold() {
    let a = fresh_addr();
    stamped_init(a);
    let w = stamped_write_lock(a);
    let r = stamped_try_convert_to_read(a, w);
    assert!(r != 0);
    assert!(is_read_stamp(r));
    assert!(!stamped_is_write_locked(a));
    assert_eq!(stamped_get_read_lock_count(a), 1);
    assert!(stamped_unlock_read(a, r));
}

#[test]
fn stamped_is_locked_covers_both_modes() {
    let a = fresh_addr();
    stamped_init(a);
    assert!(!stamped_is_locked(a));
    let r = stamped_read_lock(a);
    assert!(stamped_is_locked(a));
    assert!(stamped_unlock_read(a, r));
    let w = stamped_write_lock(a);
    assert!(stamped_is_locked(a));
    assert!(stamped_unlock_write(a, w));
    assert!(!stamped_is_locked(a));
}

// --- ReentrantReadWriteLock ---

#[test]
fn rw_basic_read_lock_unlock() {
    let a = fresh_addr();
    rw_init(a, false);
    rw_read_lock(a, 1);
    assert_eq!(rw_read_count(a), 1);
    rw_read_unlock(a, 1);
    assert_eq!(rw_read_count(a), 0);
}

#[test]
fn rw_basic_write_lock_unlock() {
    let a = fresh_addr();
    rw_init(a, false);
    rw_write_lock(a, 1);
    assert!(rw_is_write_locked(a));
    assert!(rw_write_is_held(a, 1));
    assert!(!rw_write_is_held(a, 2));
    rw_write_unlock(a, 1);
    assert!(!rw_is_write_locked(a));
}

#[test]
fn rw_reentrant_write() {
    let a = fresh_addr();
    rw_init(a, false);
    rw_write_lock(a, 1);
    rw_write_lock(a, 1); // same thread re-enters
    assert!(rw_write_is_held(a, 1));
    rw_write_unlock(a, 1);
    assert!(rw_write_is_held(a, 1)); // still held
    rw_write_unlock(a, 1);
    assert!(!rw_is_write_locked(a));
}

#[test]
fn rw_reentrant_read() {
    let a = fresh_addr();
    rw_init(a, false);
    rw_read_lock(a, 1);
    rw_read_lock(a, 1); // same thread re-enters
    assert_eq!(rw_read_count(a), 2);
    rw_read_unlock(a, 1);
    rw_read_unlock(a, 1);
    assert_eq!(rw_read_count(a), 0);
}

#[test]
fn rw_try_write_blocked_by_reader_from_other_thread() {
    let a = fresh_addr();
    rw_init(a, false);
    rw_read_lock(a, 1);
    assert!(!rw_try_write_lock(a, 2));
    rw_read_unlock(a, 1);
    assert!(rw_try_write_lock(a, 2));
    rw_write_unlock(a, 2);
}

#[test]
fn rw_try_read_blocked_by_writer_from_other_thread() {
    let a = fresh_addr();
    rw_init(a, false);
    rw_write_lock(a, 1);
    assert!(!rw_try_read_lock(a, 2));
    rw_write_unlock(a, 1);
    assert!(rw_try_read_lock(a, 2));
    rw_read_unlock(a, 2);
}

#[test]
fn rw_writer_blocks_on_readers_and_releases() {
    let a = fresh_addr();
    rw_init(a, false);
    rw_read_lock(a, 1);
    let h = std::thread::spawn(move || {
        rw_write_lock(a, 2);
        rw_write_unlock(a, 2);
    });
    std::thread::sleep(std::time::Duration::from_millis(20));
    rw_read_unlock(a, 1);
    h.join().unwrap();
    assert!(!rw_is_write_locked(a));
}

#[test]
fn rw_concurrent_readers_no_serialization() {
    let a = fresh_addr();
    rw_init(a, false);
    let n = 4;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
    let mut handles = vec![];
    for tid in 0..n as u64 {
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            rw_read_lock(a, tid + 1);
            // All readers must be holding simultaneously here.
            b.wait();
            let count = rw_read_count(a);
            rw_read_unlock(a, tid + 1);
            count
        }));
    }
    let counts: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // At the barrier rendezvous all 4 readers were holding.
    assert!(
        counts.iter().any(|c| *c == n as i32),
        "expected at least one observation of {} concurrent readers, got {:?}",
        n,
        counts
    );
    assert_eq!(rw_read_count(a), 0);
}
