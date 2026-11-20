use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn make_ctx() -> MockNativeContext {
    MockNativeContext::new()
}

// Several concurrency primitives (StampedLock, and — since the
// descriptor-collision fix — ReentrantLock/Condition) keep their shim
// state in process-global per-object side maps keyed by the object's
// identity (`ObjectRef.as_ptr()` / `identity_hash_code`).  Every fresh
// `MockNativeContext` starts with `next_ptr = 8`, so the first object
// each test allocates reuses address `0x8` — tests running in parallel
// therefore share the same side-map state and race.  Serialize every
// test that touches one of those global maps with this test-only Mutex.
// Production code is unaffected because real `ObjectRef`s produced by the
// heap allocator have unique, GC-stable identities per live object.
fn stamped_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn default_real_aqs_does_not_override_semaphore() {
    // This test deliberately leaves the process environment untouched:
    // production's default is real AQS. Synthetic-AQS test runs exercise
    // the legacy registration surface instead.
    if crate::nbflags().synthetic_aqs && !crate::nbflags().real_aqs {
        return;
    }

    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    register_concurrent_natives(&mut registry);
    let sem = "java/util/concurrent/Semaphore";
    for (name, descriptor) in [
        ("<init>", "(I)V"),
        ("<init>", "(IZ)V"),
        ("acquire", "()V"),
        ("release", "()V"),
        ("availablePermits", "()I"),
    ] {
        assert!(
            registry.find(sem, name, descriptor).is_none(),
            "real AQS must execute Semaphore.{name}{descriptor} bytecode"
        );
    }
}

// -----------------------------------------------------------------------
// ReentrantLock
// -----------------------------------------------------------------------

#[test]
fn rl_lock_unlock_basic() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    // Lock: hold count should become 1
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);

    // Unlock: hold count should become 0, owner should become 0
    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
}

#[test]
fn rl_try_lock_succeeds_when_unlocked() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    let result = native_rl_try_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(result, Some(Value::Int(1))); // true
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
}

#[test]
fn rl_reentrant_locking() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    // Lock twice -- should increment hold count to 2
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 2);

    // First unlock decrements to 1 (still locked)
    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
    assert_ne!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);

    // Second unlock fully releases
    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
}

#[test]
fn rl_unlock_notifies_on_full_release() {
    // Unlock when hold count drops to 0 should call monitor_notify
    // In MockNativeContext, monitor_notify is a no-op but doesn't error
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    // This should succeed (calls monitor_notify internally)
    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
}

// -----------------------------------------------------------------------
// Condition
// -----------------------------------------------------------------------

#[test]
fn cond_await_releases_lock() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    // Create condition associated with this lock
    let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
        .unwrap()
        .unwrap();
    let cond = match cond_val {
        Value::Object(Some(c)) => c,
        _ => panic!("expected condition object"),
    };

    // await releases the lock (in mock, monitor_wait returns immediately
    // so the lock is released then reacquired within the same call)
    native_cond_await(&mut ctx, &[Value::Object(Some(cond))]).unwrap();

    // After await returns, lock should be reacquired
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
}

#[test]
fn cond_signal_does_not_error() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
        .unwrap()
        .unwrap();
    let cond = match cond_val {
        Value::Object(Some(c)) => c,
        _ => panic!("expected condition object"),
    };

    // signal and signalAll should complete without error
    native_cond_signal(&mut ctx, &[Value::Object(Some(cond))]).unwrap();
    native_cond_signal_all(&mut ctx, &[Value::Object(Some(cond))]).unwrap();
}

// -----------------------------------------------------------------------
// CountDownLatch
// -----------------------------------------------------------------------

// NOTE: since the descriptor-coercion fix the count lives in an int[1]
// holder object in slot 0 (the real CountDownLatch's only field is the
// reference-typed `sync`, so a raw Int there was coerced to null in
// real-JDK builds). Assert through `cdl_count` — the storage accessor —
// rather than the raw slot.
#[test]
fn cdl_countdown_to_zero_triggers_notify() {
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(2)]).unwrap();

    assert_eq!(cdl_count(&mut ctx, cdl), 2);

    native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
    assert_eq!(cdl_count(&mut ctx, cdl), 1);

    // Second countDown reaches zero -- triggers monitor_notify_all (no-op in mock)
    native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
    assert_eq!(cdl_count(&mut ctx, cdl), 0);
}

#[test]
fn cdl_await_returns_when_count_is_zero() {
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    // Init with count=0 means await should return immediately
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(0)]).unwrap();

    // Should not block (count is already 0)
    native_cdl_await(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
    assert_eq!(cdl_count(&mut ctx, cdl), 0);
}

/// Interpreter round i3 wave 79, lane L1: `register_synthetic_aqs_natives`
/// states its own kind. Called under an ambient `Bridge` scope (what an
/// enclosing registrar could leave), every row is still `SyntheticStub`,
/// and the caller's scope is restored afterwards.
#[test]
fn i79_synthetic_aqs_natives_state_their_kind() {
    use cratonvm_native_api::NativeKind;
    let mut r = NativeMethodRegistry::new();
    register_synthetic_aqs_natives(&mut r);
    for (c, m, d) in [
        ("java/util/concurrent/locks/ReentrantLock", "lock", "()V"),
        ("java/util/concurrent/locks/ReentrantLock", "<init>", "()V"),
        ("java/util/concurrent/locks/Lock", "unlock", "()V"),
        ("java/util/concurrent/locks/Condition", "await", "()V"),
    ] {
        assert_eq!(
            r.kind_of(c, m, d),
            Some(NativeKind::SyntheticStub),
            "{c}.{m}{d}"
        );
    }
}

/// A mock `TimeUnit` the natives resolve through slot 0 (its ordinal).
fn i79_time_unit(ctx: &mut MockNativeContext, ordinal: i32) -> ObjectRef {
    let unit = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(unit, 0, Value::Int(ordinal));
    unit
}

/// Interpreter round i3 wave 79, lane L1: a pending interrupt raises
/// `InterruptedException` from both `await` forms even when the count is
/// already zero (AQS tests `Thread.interrupted()` first), and is cleared.
#[test]
fn i79_cdl_await_on_an_open_latch_raises_a_pending_interrupt() {
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(0)]).unwrap();

    ctx.set_interrupted(true);
    let r = native_cdl_await(&mut ctx, &[Value::Object(Some(cdl))]);
    assert!(r.is_err(), "await() with a pending interrupt raises");
    assert!(!ctx.is_interrupted(false), "the interrupt is consumed");

    let unit = i79_time_unit(&mut ctx, 3);
    ctx.set_interrupted(true);
    let r = native_cdl_await_timeout(
        &mut ctx,
        &[
            Value::Object(Some(cdl)),
            Value::Long(1),
            Value::Object(Some(unit)),
        ],
    );
    assert!(r.is_err(), "await(1 s) with a pending interrupt raises");
    assert!(!ctx.is_interrupted(false), "the interrupt is consumed");

    // No interrupt: the open latch answers at once.
    native_cdl_await(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
    let r = native_cdl_await_timeout(
        &mut ctx,
        &[
            Value::Object(Some(cdl)),
            Value::Long(1),
            Value::Object(Some(unit)),
        ],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(1)));
}

/// Interpreter round i3 wave 79, lane L1: `await(-1, SECONDS)` and
/// `await(0, SECONDS)` on a closed latch answer `false` without waiting
/// (`nanosTimeout <= 0L`); the negative timeout used to wait forever.
#[test]
fn i79_cdl_await_non_positive_timeout_answers_false_at_once() {
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(1)]).unwrap();
    let unit = i79_time_unit(&mut ctx, 3);
    for t in [-1_i64, 0, i64::MIN] {
        let r = native_cdl_await_timeout(
            &mut ctx,
            &[
                Value::Object(Some(cdl)),
                Value::Long(t),
                Value::Object(Some(unit)),
            ],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(0)), "await({t} s) on a closed latch");
    }
    assert!(
        ctx.monitor_wait_calls.borrow().is_empty(),
        "a non-positive timeout never waits"
    );
    assert_eq!(cdl_count(&mut ctx, cdl), 1);
}

/// Interpreter round i3 wave 79, lane L1: a sub-millisecond timeout waits
/// (it used to truncate to 0 ms and answer `false` without a wait).
#[test]
fn i79_cdl_await_sub_millisecond_timeout_waits() {
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(1)]).unwrap();
    let unit = i79_time_unit(&mut ctx, 1); // MICROSECONDS
    let r = native_cdl_await_timeout(
        &mut ctx,
        &[
            Value::Object(Some(cdl)),
            Value::Long(500),
            Value::Object(Some(unit)),
        ],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(0)), "nobody counts down: false");
    assert!(
        !ctx.monitor_wait_calls.borrow().is_empty(),
        "the 500 us timeout waits at least once"
    );
}

#[test]
fn i79_cdl_timeout_nanos_saturates_like_time_unit() {
    assert_eq!(cdl_timeout_nanos(500, 0), 500);
    assert_eq!(cdl_timeout_nanos(500, 1), 500_000);
    assert_eq!(cdl_timeout_nanos(2, 2), 2_000_000);
    assert_eq!(cdl_timeout_nanos(-1, 3), -1_000_000_000);
    assert_eq!(cdl_timeout_nanos(1, 6), 86_400_000_000_000);
    assert_eq!(cdl_timeout_nanos(i64::MAX, 3), i64::MAX);
    assert_eq!(cdl_timeout_nanos(i64::MIN / 2, 4), i64::MIN);
}

/// Interpreter round i1 wave 25, lane L7: an interrupted wait leaves the
/// latch's monitor (it used to return through `?` still owning it, and the
/// next `countDown` blocked forever).
#[test]
fn cdl_await_interrupted_exits_the_monitor() {
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(1)]).unwrap();
    ctx.monitor_wait_interrupts.set(true);
    assert!(native_cdl_await(&mut ctx, &[Value::Object(Some(cdl))]).is_err());
    assert_eq!(ctx.monitor_depth.get(), 0);
    ctx.monitor_wait_interrupts.set(false);
    native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
    assert_eq!(ctx.monitor_depth.get(), 0);
    assert_eq!(cdl_count(&mut ctx, cdl), 0);
}

/// Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
/// the served `await()` waits as the AQS park it stands for (`Parked`),
/// the timed `await(long, TimeUnit)` as a park with a deadline
/// (`TimedParked`: TIMED_WAITING, JVMTI PARKED, as HotSpot reads it), and
/// neither as a plain monitor wait. The mock's wait answers an interrupt,
/// so each native waits exactly once.
#[test]
fn cdl_await_waits_as_the_park_it_stands_for() {
    use cratonvm_native_api::BlockKind;
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(1)]).unwrap();
    ctx.monitor_wait_interrupts.set(true);
    assert!(native_cdl_await(&mut ctx, &[Value::Object(Some(cdl))]).is_err());
    assert_eq!(*ctx.monitor_wait_kinds.borrow(), vec![BlockKind::Parked]);

    // A `TimeUnit` the mock resolves through slot 0: MILLISECONDS (2).
    let unit = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(unit, 0, Value::Int(2));
    let r = native_cdl_await_timeout(
        &mut ctx,
        &[
            Value::Object(Some(cdl)),
            Value::Long(60_000),
            Value::Object(Some(unit)),
        ],
    );
    assert!(r.is_err(), "the interrupted wait raises");
    assert_eq!(
        *ctx.monitor_wait_kinds.borrow(),
        vec![BlockKind::Parked, BlockKind::TimedParked]
    );
    assert_eq!(
        ctx.monitor_depth.get(),
        0,
        "the monitor is left on both arms"
    );
    let waits = ctx.monitor_wait_calls.borrow();
    assert_eq!(waits.len(), 2);
    assert!(
        matches!(waits[1].1, Some(ms) if (1..=10).contains(&ms)),
        "a bounded slice with a positive timeout: {:?}",
        waits[1].1
    );
}

#[test]
fn cdl_countdown_below_zero_stays_at_zero() {
    let mut ctx = make_ctx();
    let cdl =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
            .unwrap();
    native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(1)]).unwrap();

    native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
    native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap(); // extra
    assert_eq!(cdl_count(&mut ctx, cdl), 0);
}

// -----------------------------------------------------------------------
// Semaphore
// -----------------------------------------------------------------------

// NOTE: since the descriptor-coercion fix the permits/fair pair lives in
// an int[2] holder object in slot 0 (the real Semaphore's only field is
// the reference-typed `sync`). Assert through `sem_permits`.
#[test]
fn sem_acquire_decrements_permits() {
    let mut ctx = make_ctx();
    let sem =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
    native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();

    assert_eq!(sem_permits(&mut ctx, sem), 3);

    native_sem_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
    assert_eq!(sem_permits(&mut ctx, sem), 2);
}

#[test]
fn sem_acquire_uninterruptibly_n_decrements_requested_permits() {
    let mut ctx = make_ctx();
    let sem =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
    native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();

    native_sem_acquire_n(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();
    assert_eq!(sem_permits(&mut ctx, sem), 0);
}

#[test]
fn sem_release_increments_and_notifies() {
    let mut ctx = make_ctx();
    let sem =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
    native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(1)]).unwrap();

    // Release adds a permit (calls monitor_notify internally)
    native_sem_release(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
    assert_eq!(sem_permits(&mut ctx, sem), 2);
}

#[test]
fn sem_try_acquire_with_no_permits_returns_zero() {
    let mut ctx = make_ctx();
    let sem =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
    native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(0)]).unwrap();

    let result = native_sem_try_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
    assert_eq!(result, Some(Value::Int(0))); // false -- no permits available
    assert_eq!(sem_permits(&mut ctx, sem), 0);
}

#[test]
fn sem_try_acquire_with_permits_succeeds() {
    let mut ctx = make_ctx();
    let sem =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
    native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(5)]).unwrap();

    let result = native_sem_try_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
    assert_eq!(result, Some(Value::Int(1))); // true
    assert_eq!(sem_permits(&mut ctx, sem), 4);
}

// -----------------------------------------------------------------------
// Unsafe.setMemory
// -----------------------------------------------------------------------

#[test]
fn unsafe_set_memory_fills_fields() {
    let mut ctx = make_ctx();
    // Allocate an object with 5 fields, all initially 0
    let obj = ctx.alloc_object(ClassId::new(0), 5);
    assert_eq!(ctx.get_field(obj, 0), Value::Int(0));

    // setMemory(unsafe_this, obj, offset=1, bytes=3, value=0x42)
    // Should fill fields [1], [2], [3] with Int(0x42)
    native_unsafe_set_memory(
        &mut ctx,
        &[
            Value::Object(None), // unsafe this (ignored)
            Value::Object(Some(obj)),
            Value::Long(1),   // offset
            Value::Long(3),   // bytes (field count)
            Value::Int(0x42), // fill value
        ],
    )
    .unwrap();

    // Fields 0 and 4 should be unchanged
    assert_eq!(ctx.get_field(obj, 0), Value::Int(0));
    assert_eq!(ctx.get_field(obj, 4), Value::Int(0));
    // Fields 1, 2, 3 should be filled with 0x42
    assert_eq!(ctx.get_field(obj, 1), Value::Int(0x42));
    assert_eq!(ctx.get_field(obj, 2), Value::Int(0x42));
    assert_eq!(ctx.get_field(obj, 3), Value::Int(0x42));
}

// M3: oversized requests are rejected before any heap access, and array
// targets that fail the bounds-checked path no longer fall through to the
// unbounded slot loop.

#[test]
fn unsafe_set_memory_rejects_oversized_request() {
    let mut ctx = make_ctx();
    let obj = ctx.alloc_object(ClassId::new(0), 5);
    let huge = (MAX_UNSAFE_COPY_SIZE + 1) as i64;
    let res = native_unsafe_set_memory(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Long(huge),
            Value::Int(0x42),
        ],
    );
    assert!(res.is_err(), "oversized setMemory must be rejected");
    // Nothing was written despite the huge byte count.
    assert_eq!(ctx.get_field(obj, 0), Value::Int(0));
}

#[test]
fn unsafe_copy_memory_rejects_oversized_request() {
    let mut ctx = make_ctx();
    let src = ctx.alloc_object(ClassId::new(0), 4);
    let dst = ctx.alloc_object(ClassId::new(0), 4);
    let huge = (MAX_UNSAFE_COPY_SIZE + 1) as i64;
    let res = native_unsafe_copy_memory(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(src)),
            Value::Long(0),
            Value::Object(Some(dst)),
            Value::Long(0),
            Value::Long(huge),
        ],
    );
    assert!(res.is_err());
    assert_eq!(ctx.get_field(dst, 0), Value::Int(0));
}

// ===================================================================
// M18: Real StampedLock, monitor-based contention, condition timeouts
// ===================================================================

// ------- StampedLock -------

fn stamped_lock_obj(ctx: &mut MockNativeContext) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/locks/StampedLock")
        .expect("mock StampedLock class id");
    ctx.alloc_object(cid, 8)
}

/// Interpreter round i3 wave 79, lane L1
/// (`i68-L1-stamped-lock-stand-ins-reuse-a-pre-region-receiver`): a
/// collection that completes while the acquire is in its blocking region
/// moves the lock; the `state` mirror is written through the address the
/// region's end hands back, not the pre-region local (which the moving mock
/// retires, so a stale write panics there).
#[test]
fn i79_stamped_acquires_mirror_through_the_post_region_address() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    // A private VM identity: the mock's region-end relocation re-addresses
    // this VM's weak lock-key slots (as the VM's GC epilogue does), and
    // mock heaps of concurrently running tests reuse the same addresses.
    const VM: usize = 0x1797_5A1B;
    ctx.set_vm_identity(VM);
    let sl = stamped_lock_obj(&mut ctx);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let root = ctx.add_global_root(sl);
    ctx.set_moving_heap_vm_rule(true);
    ctx.set_region_end_gc_point(true);

    let before = ctx.relocations_for_test();
    let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert!(
        ctx.relocations_for_test() > before,
        "the region's end relocated the heap"
    );
    let sl = ctx.resolve_global_root(root).expect("rooted lock");
    native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();

    let sl = ctx.resolve_global_root(root).expect("rooted lock");
    let stamp = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    let sl = ctx.resolve_global_root(root).expect("rooted lock");
    native_stamped_unlock_read(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
    ctx.set_region_end_gc_point(false);
    ctx.set_moving_heap_vm_rule(false);
    crate::forget_vm_lock_keys(VM);
}

#[test]
fn m18_stamped_init_creates_state() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    // tryOptimisticRead should return STAMPED_ORIGIN (256) since no writer
    let stamp = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(stamp, Value::Long(STAMPED_ORIGIN));
}

/// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
/// the `StampedLock` acquires' regions publish the lock itself as the
/// park blocker (the JDK's `LockSupport.park(this)`); a view's acquire
/// publishes its owning lock.
#[test]
fn i68_stamped_acquires_publish_the_lock_as_their_blocker() {
    use cratonvm_native_api::BlockKind;
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(
        *ctx.region_wait_objects.borrow(),
        vec![(sl, BlockKind::Parked)],
        "writeLock parks on the lock"
    );
    native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
    let stamp = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(
        ctx.region_wait_objects.borrow().last().copied(),
        Some((sl, BlockKind::Parked)),
        "readLock parks on the lock"
    );
    native_stamped_unlock_read(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
    assert_eq!(
        ctx.blocking_region_counts(),
        (2, 2),
        "each publishing region is still one region"
    );
}

/// Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): `getBlocker` is
/// the JDK body (the thread's `parkBlocker`, NPE for a null thread) with
/// the switch on, the old constant null with it off.
#[test]
fn i68_get_blocker_reads_the_park_blocker() {
    let mut ctx = make_ctx();
    let thread = i68_thread_with_park_blocker(&mut ctx);
    let blocker = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(thread, 1, Value::Object(Some(blocker)));
    assert_eq!(
        native_lock_support_get_blocker(&mut ctx, &[Value::Object(Some(thread))]).unwrap(),
        Some(Value::Object(Some(blocker)))
    );
    assert!(native_lock_support_get_blocker(&mut ctx, &[Value::Object(None)]).is_err());
    ctx.stand_in_wait_objects_answer.set(false);
    assert_eq!(
        native_lock_support_get_blocker(&mut ctx, &[Value::Object(Some(thread))]).unwrap(),
        Some(Value::Object(None))
    );
}

/// Wave 68 (lane L1): the `park(Object)` / `parkNanos(Object, long)`
/// bridges leave `parkBlocker` cleared, as the JDK body's `finally` does,
/// and leave it alone with the switch off.
#[test]
fn i68_park_with_a_blocker_clears_it_after_the_park() {
    let mut ctx = make_ctx();
    let thread = i68_thread_with_park_blocker(&mut ctx);
    let blocker = ctx.alloc_object(ClassId::new(0), 1);
    let sentinel = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(thread, 1, Value::Object(Some(sentinel)));
    native_lock_support_park(&mut ctx, &[Value::Object(Some(blocker))]).unwrap();
    assert_eq!(
        ctx.get_field(thread, 1),
        Value::Object(None),
        "park: cleared"
    );
    ctx.set_field(thread, 1, Value::Object(Some(sentinel)));
    native_lock_support_park_nanos(&mut ctx, &[Value::Object(Some(blocker)), Value::Long(1)])
        .unwrap();
    assert_eq!(
        ctx.get_field(thread, 1),
        Value::Object(None),
        "parkNanos: cleared"
    );
    ctx.stand_in_wait_objects_answer.set(false);
    ctx.set_field(thread, 1, Value::Object(Some(sentinel)));
    native_lock_support_park(&mut ctx, &[Value::Object(Some(blocker))]).unwrap();
    assert_eq!(
        ctx.get_field(thread, 1),
        Value::Object(Some(sentinel)),
        "=0: the bridge does not touch the field"
    );
}

/// The mock's current thread, with `parkBlocker` declared at slot 1.
fn i68_thread_with_park_blocker(ctx: &mut MockNativeContext) -> ObjectRef {
    let thread = ctx.stable_current_thread();
    let cid = ctx.class_id_of_object(thread);
    ctx.set_declared_fields(
        cid,
        vec![cratonvm_native_api::FieldMetadata {
            name: "parkBlocker".to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: 1,
            declaring_class_id: cid,
            is_static: false,
        }],
    );
    thread
}

// bug nb-lib-gckeys §1 regression: the StampedLock/RWL state tables must
// be keyed by a GC-STABLE identity, not the raw moving heap address.
// Here we verify the contract `gc_stable_lock_key` guarantees:
//   (a) the SAME object yields the SAME key on every call (so a thread
//       that locks before a GC unlocks the same slot afterwards), and
//   (b) DISTINCT objects yield DISTINCT keys (no cross-lock aliasing).
// We can't simulate a real relocation under MockNativeContext (its
// `identity_hash_code` is derived from the address), but the stability +
// distinctness invariants are exactly what the production key must hold.
#[test]
fn m18_gc_stable_lock_key_is_stable_and_distinct() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let a = ctx.alloc_object(ClassId::new(0), 1);
    let b = ctx.alloc_object(ClassId::new(0), 1);

    let ka1 = gc_stable_lock_key(&mut ctx, a).unwrap();
    let ka2 = gc_stable_lock_key(&mut ctx, a).unwrap();
    let kb = gc_stable_lock_key(&mut ctx, b).unwrap();

    // (a) stable for the same object across calls
    assert_eq!(ka1, ka2, "key must be stable for the same lock object");
    // (b) distinct objects → distinct keys
    assert_ne!(
        ka1, kb,
        "distinct lock objects must not alias the same slot"
    );
}

// bug nb-lib-gckeys §1 regression: a full write-lock / unlock round-trip
// through the native entry points (which now route the key via `ctx` and
// `gc_stable_lock_key`) must leave the lock un-held, proving lock and
// unlock agree on the same GC-stable slot.
#[test]
fn m18_stamped_roundtrip_via_gc_stable_key() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    // Acquire the write lock, then release it.
    let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    // Real JDK layout: a write stamp's mode field is WBIT (128). It used
    // to be bit 0, which made `StampedLock.isWriteLockStamp` — real JDK
    // bytecode, since nothing registers it — call this a READ stamp.
    assert!(
        matches!(stamp, Value::Long(v) if v & 255 == 128),
        "got {stamp:?}"
    );
    // `unlockWrite` now CHECKS its stamp, so it must be the real one.
    native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();

    // After unlock the lock must NOT report write-locked — i.e. the unlock
    // hit the SAME slot the lock established (would fail under the old
    // raw-address keying if lock and unlock disagreed on the key).
    let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(0), "lock/unlock must agree on the slot");
}

#[test]
fn m18_stamped_real_jdk_registry_keeps_write_and_unstamped_unlock_coherent() {
    let _guard = stamped_test_lock();
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    register_concurrent_natives(&mut registry);
    register_stamped_lock_natives(&mut registry);
    let sl_class = "java/util/concurrent/locks/StampedLock";
    let init = registry
        .find(sl_class, "<init>", "()V")
        .expect("StampedLock.<init> should be registered");
    let write_lock = registry
        .find(sl_class, "writeLock", "()J")
        .expect("StampedLock.writeLock should be registered");
    let unstamped_unlock_write = registry
        .find(sl_class, "unstampedUnlockWrite", "()V")
        .expect("StampedLock.unstampedUnlockWrite should be registered");
    let is_write_locked = registry
        .find(sl_class, "isWriteLocked", "()Z")
        .expect("StampedLock.isWriteLocked should be registered");

    let mut ctx = make_ctx();
    let sl = stamped_lock_obj(&mut ctx);
    init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    unstamped_unlock_write(&mut ctx, &[Value::Object(Some(sl))])
        .expect("writeLock and unstampedUnlockWrite must use the same state backend");

    let locked = is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(0));
}

/// Interpreter round i2 wave 70, lane L1 (`JdkStampedLock`,
/// `CRATONVM_COMPAT_JDK_STAMPED_LOCK`): over a real JDK, `--compatible`
/// with the row (the default) registers NONE of the 31 rows -- the whole
/// surface goes, never part of it -- and with the row off all 31 stay.
#[test]
fn stamped_lock_surface_is_all_or_nothing_under_the_stamped_lock_row() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let rows_on_the_class = |rows: HotSpotAlignments| {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_hotspot_alignments(rows);
        register_stamped_lock_natives(&mut registry);
        registry
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| c.starts_with("java/util/concurrent/locks/StampedLock"))
            .count()
    };
    assert_eq!(rows_on_the_class(HotSpotAlignments::defaults()), 0);
    assert_eq!(
        rows_on_the_class(HotSpotAlignments::defaults().with(Row::JdkStampedLock, false)),
        31
    );
}

/// nio21 wave 2, lane L4 (`JdkAdders`, `CRATONVM_COMPAT_ALIGN=-jdk-adders`):
/// over a real JDK, `--compatible` with the row (the default) registers
/// none of the 15 `LongAdder` / `DoubleAdder` stand-ins -- whose contended
/// adds live in a Rust cell strip the JDK's `Striped64` bodies never see,
/// so the surface goes whole -- and with the row off all 15 stay. The
/// `Atomic*Array` rows of the same registrar are not the row's.
#[test]
fn r21w2_adder_surface_is_all_or_nothing_under_the_adders_row() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let count = |rows: HotSpotAlignments, prefix: &str| {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_hotspot_alignments(rows);
        register_atomic_extras_natives(&mut registry);
        registry
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| c.starts_with(prefix))
            .count()
    };
    let adders = |rows: HotSpotAlignments| {
        count(rows, "java/util/concurrent/atomic/LongAdder")
            + count(rows, "java/util/concurrent/atomic/DoubleAdder")
    };
    assert_eq!(adders(HotSpotAlignments::defaults()), 0);
    assert_eq!(
        adders(HotSpotAlignments::defaults().with(Row::JdkAdders, false)),
        10 + 5
    );
    assert_eq!(
        count(
            HotSpotAlignments::defaults(),
            "java/util/concurrent/atomic/AtomicIntegerArray"
        ),
        count(
            HotSpotAlignments::defaults().with(Row::JdkAdders, false),
            "java/util/concurrent/atomic/AtomicIntegerArray"
        )
    );
}

/// Interpreter round i3 wave 71, lane L1 (`JdkCountDownLatch`,
/// `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH`): over a real JDK,
/// `register_concurrent_natives` with the row registers NONE of its six
/// `CountDownLatch` rows, and with the row off all six stay. The row is
/// set explicitly both ways (no assertion on its default).
#[test]
fn count_down_latch_surface_is_all_or_nothing_under_the_latch_row() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let rows_on_the_class = |on: bool| {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_hotspot_alignments(
            HotSpotAlignments::defaults().with(Row::JdkCountDownLatch, on),
        );
        register_concurrent_natives(&mut registry);
        registry
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| c.starts_with("java/util/concurrent/CountDownLatch"))
            .count()
    };
    assert_eq!(rows_on_the_class(true), 0);
    assert_eq!(rows_on_the_class(false), 6);
}

/// Interpreter round i3 wave 72, lane L1 (`JdkAqls`,
/// `CRATONVM_COMPAT_JDK_AQLS`): over a real JDK, `register_rwlock_natives`
/// with the row registers none of the three
/// `AbstractQueuedLongSynchronizer` state stand-ins, and with the row off
/// all three stay. The row is set explicitly both ways.
#[test]
fn aqls_state_stand_ins_follow_the_aqls_row() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let rows_on_the_class = |on: bool| {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_hotspot_alignments(HotSpotAlignments::defaults().with(Row::JdkAqls, on));
        register_rwlock_natives(&mut registry);
        registry
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| {
                *c == "java/util/concurrent/locks/AbstractQueuedLongSynchronizer"                })
            .count()
    };
    assert_eq!(rows_on_the_class(true), 0);
    assert_eq!(rows_on_the_class(false), 3);
}

/// Interpreter round i3 wave 72, lane L1 (`JdkCopyOnWrite`,
/// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE`): over a real JDK,
/// `register_concurrent_natives` with the row registers none of its
/// fourteen `CopyOnWriteArrayList` bridges (every one a retired row).
/// Wave 76 (the row graduated for these stand-ins): with the row OFF over a
/// real JDK none is registered either, and the synthetic class library
/// keeps all fourteen, row on or off. The row is set explicitly both ways.
#[test]
fn copy_on_write_bridges_follow_the_copy_on_write_row() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let rows_on_the_class = |real_jdk: bool, on: bool| {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(real_jdk);
        registry.set_hotspot_alignments(
            HotSpotAlignments::defaults().with(Row::JdkCopyOnWrite, on),
        );
        register_concurrent_natives(&mut registry);
        registry
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == "java/util/concurrent/CopyOnWriteArrayList")
            .count()
    };
    assert_eq!(rows_on_the_class(true, true), 0);
    assert_eq!(rows_on_the_class(true, false), 0);
    assert_eq!(rows_on_the_class(false, true), 14);
    assert_eq!(rows_on_the_class(false, false), 14);
}

#[test]
fn m18_stamped_write_view_lock_unlock_updates_parent_state() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = stamped_lock_obj(&mut ctx);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let view_cid = ctx
        .ensure_class_initialized("java/util/concurrent/locks/StampedLock$WriteLockView")
        .expect("mock StampedLock WriteLockView class id");
    let view = ctx.alloc_object(view_cid, 1);
    ctx.set_field_by_name(view, "this$0", Value::Object(Some(sl)));

    native_stamped_write_view_lock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
    let write_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_ne!(write_state & 128, 0);

    native_stamped_write_view_unlock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
    let post_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_eq!(post_state & 128, 0);    }

#[test]
fn m18_stamped_read_view_lock_unlock_updates_parent_state() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = stamped_lock_obj(&mut ctx);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let view_cid = ctx
        .ensure_class_initialized("java/util/concurrent/locks/StampedLock$ReadLockView")
        .expect("mock StampedLock ReadLockView class id");
    let view = ctx.alloc_object(view_cid, 1);
    ctx.set_field_by_name(view, "this$0", Value::Object(Some(sl)));

    native_stamped_read_view_lock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
    let read_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_eq!(read_state & 127, 1);

    native_stamped_read_view_unlock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
    let post_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_eq!(post_state & 127, 0);    }

#[test]
fn m18_stamped_unstamped_write_unlock_updates_side_table_and_state() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = stamped_lock_obj(&mut ctx);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    assert_eq!(
        ctx.get_field_by_name(sl, "state"),
        Value::Long(STAMPED_ORIGIN)
    );

    native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let write_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_ne!(write_state & 128, 0, "JDK WBIT must be visible");

    native_stamped_unstamped_unlock_write(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(0));
    let post_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_eq!(post_state & 128, 0, "JDK WBIT must clear after unlock");    }

#[test]
fn m18_stamped_unstamped_read_unlock_updates_side_table_and_state() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = stamped_lock_obj(&mut ctx);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(count, Value::Int(1));
    let read_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_ne!(read_state & 127, 0, "read count must be visible");

    native_stamped_unstamped_unlock_read(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(count, Value::Int(0));
    let post_state = match ctx.get_field_by_name(sl, "state") {
        Value::Long(v) => v,
        other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
    };
    assert_eq!(post_state & 127, 0, "read count must clear after unlock");    }

#[test]
fn m18_stamped_write_lock_returns_wbit_stamp() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    match stamp {
        Value::Long(v) => {
            // JDK `isWriteLockStamp`: `(stamp & ABITS) == WBIT`.
            assert_eq!(v & 255, 128, "write stamp mode field must be WBIT, got {v}");
        }
        _ => panic!("expected Long stamp"),
    }
}

#[test]
fn m18_stamped_write_lock_blocks_optimistic_read() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    // Take write lock
    native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    // tryOptimisticRead should return 0 while writer is active
    let opt = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(
        opt,
        Value::Long(0),
        "optimistic read should fail during write lock"
    );
}

#[test]
fn m18_stamped_unlock_write_advances_stamp() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let before = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
    let after = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();

    match (before, after) {
        (Value::Long(b), Value::Long(a)) => {
            assert!(
                a > b,
                "stamp should advance after write: before={}, after={}",
                b,
                a
            );
            // JDK `isOptimisticReadStamp`: `(stamp & ABITS) == 0`.
            assert_eq!(a & 255, 0, "post-unlock stamp must be optimistic, got {a}");
        }
        _ => panic!("expected Long stamps"),
    }
}

#[test]
fn m18_stamped_validate_succeeds_when_no_write() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let stamp = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap()
    {
        Value::Long(v) => v,
        _ => panic!("expected Long"),
    };
    // No writes happened — validate should succeed
    let valid =
        native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(stamp)])
            .unwrap()
            .unwrap();
    assert_eq!(valid, Value::Int(1));
}

#[test]
fn m18_stamped_validate_fails_after_write() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let stamp = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap()
    {
        Value::Long(v) => v,
        _ => panic!("expected Long"),
    };
    // Perform a write lock/unlock cycle — stamp advances
    let wstamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), wstamp]).unwrap();
    // Old stamp should now be invalid
    let valid =
        native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(stamp)])
            .unwrap()
            .unwrap();
    assert_eq!(
        valid,
        Value::Int(0),
        "stamp should be invalid after a write"
    );
}

#[test]
fn m18_stamped_validate_zero_always_invalid() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let valid = native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(0)])
        .unwrap()
        .unwrap();
    assert_eq!(valid, Value::Int(0));
}

#[test]
fn m18_stamped_read_lock_returns_reader_count_stamp() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let stamp = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    match stamp {
        // JDK `isReadLockStamp`: `(stamp & RBITS) != 0`. The low 7 bits
        // are the READER COUNT, so the sole reader's stamp ends in 1.
        Value::Long(v) => assert_eq!(v & 255, 1, "read stamp mode field must be 1, got {v}"),
        _ => panic!("expected Long"),
    }
}

#[test]
fn m18_stamped_read_lock_count() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    // No readers initially
    let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(count, Value::Int(0));

    // One read lock
    let stamp1 = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(count, Value::Int(1));

    // Two read locks
    let _stamp2 = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(count, Value::Int(2));

    // Unlock one — back to 1
    native_stamped_unlock_read(&mut ctx, &[Value::Object(Some(sl)), stamp1]).unwrap();
    let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(count, Value::Int(1));
}

#[test]
fn m18_stamped_is_write_locked() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(0));

    let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(1));

    native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
    let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(0));
}

#[test]
fn m18_stamped_try_write_lock_succeeds_when_free() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let stamp = native_stamped_try_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    match stamp {
        Value::Long(v) => assert!(v != 0, "tryWriteLock should succeed when free"),
        _ => panic!("expected Long"),
    }
}

#[test]
fn m18_stamped_try_write_lock_fails_when_read_locked() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let stamp = native_stamped_try_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(
        stamp,
        Value::Long(0),
        "tryWriteLock should fail when readers active"
    );
}

#[test]
fn m18_stamped_try_read_lock_fails_when_write_locked() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
    let stamp = native_stamped_try_read_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(
        stamp,
        Value::Long(0),
        "tryReadLock should fail when writer active"
    );
}

#[test]
fn m18_stamped_convert_read_to_write() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let read_stamp = match native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap()
    {
        Value::Long(v) => v,
        _ => panic!("expected Long"),
    };
    // Only one reader — conversion should succeed
    let write_stamp = native_stamped_try_convert_to_write(
        &mut ctx,
        &[Value::Object(Some(sl)), Value::Long(read_stamp)],
    )
    .unwrap()
    .unwrap();
    match write_stamp {
        Value::Long(v) => assert!(
            v != 0,
            "conversion to write should succeed with single reader"
        ),
        _ => panic!("expected Long"),
    }
    // Should now be write-locked
    let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(1));
    // Reader count should be 0
    let readers = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(readers, Value::Int(0));
}

#[test]
fn m18_stamped_convert_write_to_read() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    // `tryConvertToReadLock` is stamp-checked: it must be handed the stamp
    // `writeLock()` actually returned (stamp 0 is the JDK's "no hold"
    // sentinel and correctly converts to nothing).
    let write_stamp = match native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap()
    {
        Value::Long(v) => v,
        other => panic!("expected Long write stamp, got {other:?}"),
    };
    let read_stamp = native_stamped_try_convert_to_read(
        &mut ctx,
        &[Value::Object(Some(sl)), Value::Long(write_stamp)],
    )
    .unwrap()
    .unwrap();
    match read_stamp {
        Value::Long(v) => {
            assert!(v != 0, "conversion to read should succeed");
            // Downgrade leaves exactly one reader, so `& ABITS == 1`.
            assert_eq!(
                v & 255,
                1,
                "converted read stamp must be a read stamp, got {v}"
            );
        }
        _ => panic!("expected Long"),
    }
    // Should not be write-locked anymore
    let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(locked, Value::Int(0));
    // Should have 1 reader
    let readers = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap();
    assert_eq!(readers, Value::Int(1));
}

// ------- ReentrantLock with monitor-based contention -------

#[test]
fn m18_rl_lock_uses_monitor_parking() {
    // Verify that ReentrantLock lock/unlock cycle works with monitor-based contention
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
    assert_ne!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);

    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
}

// ------- Condition with real timeout -------

#[test]
fn m18_cond_await_timeout_returns_true_on_signal() {
    // In single-threaded mock, monitor_wait returns immediately → not timed out
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
        .unwrap()
        .unwrap();
    let cond = match cond_val {
        Value::Object(Some(c)) => c,
        _ => panic!("expected condition object"),
    };

    // await with timeout should return true (not timed out) since mock wakes immediately
    let result =
        native_cond_await_timeout(&mut ctx, &[Value::Object(Some(cond)), Value::Long(1000)])
            .unwrap()
            .unwrap();
    // In mock, returns immediately so not timed out
    assert_eq!(result, Value::Int(1));
    // Lock should be reacquired
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
}

#[test]
fn m18_cond_await_nanos_returns_remaining() {
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
        .unwrap()
        .unwrap();
    let cond = match cond_val {
        Value::Object(Some(c)) => c,
        _ => panic!("expected condition object"),
    };

    // awaitNanos returns remaining nanoseconds (>= 0)
    let result = native_cond_await_nanos(
        &mut ctx,
        &[Value::Object(Some(cond)), Value::Long(5_000_000_000)],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Long(v) => assert!(v >= 0, "remaining nanos should be non-negative"),
        _ => panic!("expected Long"),
    }
    // Lock should be reacquired
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
}

// ------- CopyOnWriteArrayList (already real) -------

#[test]
fn m18_cowal_iterator_snapshot_isolation() {
    // CopyOnWriteArrayList iterator should see a snapshot, not live data
    let mut ctx = make_ctx();
    let cowal = try_alloc_concurrent_synthetic(
        &mut ctx,
        "java/util/concurrent/CopyOnWriteArrayList",
        2,
    )
    .unwrap();
    cratonvm_native_collections::native_al_init(&mut ctx, &[Value::Object(Some(cowal))])
        .unwrap();

    // Add two elements
    cratonvm_native_collections::native_al_add(
        &mut ctx,
        &[Value::Object(Some(cowal)), Value::Int(10)],
    )
    .unwrap();
    cratonvm_native_collections::native_al_add(
        &mut ctx,
        &[Value::Object(Some(cowal)), Value::Int(20)],
    )
    .unwrap();

    // Size should be 2
    let size =
        cratonvm_native_collections::native_al_size(&mut ctx, &[Value::Object(Some(cowal))])
            .unwrap()
            .unwrap();
    assert_eq!(size, Value::Int(2));

    // Verify elements
    let elem0 = cratonvm_native_collections::native_al_get(
        &mut ctx,
        &[Value::Object(Some(cowal)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(elem0, Value::Int(10));
    let elem1 = cratonvm_native_collections::native_al_get(
        &mut ctx,
        &[Value::Object(Some(cowal)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(elem1, Value::Int(20));
}

#[test]
fn m18_stamped_optimistic_read_validate_cycle() {
    let _guard = stamped_test_lock();
    // Full optimistic read cycle: tryOptimisticRead → validate (no intervening write)
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let stamp = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
        .unwrap()
        .unwrap()
    {
        Value::Long(v) => v,
        _ => panic!("expected Long"),
    };
    assert!(stamp != 0, "optimistic read should succeed when no writer");

    // Simulate reading shared data (no actual fields to read in mock, just validate)
    let valid =
        native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(stamp)])
            .unwrap()
            .unwrap();
    assert_eq!(
        valid,
        Value::Int(1),
        "validate should succeed when no write occurred"
    );
}

#[test]
fn m18_stamped_multiple_write_cycles_advance_stamp() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let sl = ctx.alloc_object(ClassId::new(0), 1);
    native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

    let mut prev_stamp = STAMPED_ORIGIN;
    for _ in 0..5 {
        let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
        let cur = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap()
        {
            Value::Long(v) => v,
            _ => panic!("expected Long"),
        };
        assert!(cur > prev_stamp, "stamp should monotonically increase");
        assert_eq!(
            cur & 255,
            0,
            "stamp must be optimistic after unlock, got {cur}"
        );
        prev_stamp = cur;
    }
}

// =======================================================================
// Phase D (RD.1 .. RD.10) — concurrency primitives
// =======================================================================

use crate::phases_early;

fn register_atomics() -> cratonvm_native_api::NativeMethodRegistry {
    let mut r = cratonvm_native_api::NativeMethodRegistry::new();
    phases_early::register_phase54_atomics(&mut r);
    r
}

/// j27 wave 4 (lane TRACEROWS, FS3-2): the real `AtomicIntegerArray` /
/// `AtomicLongArray` have ONE instance field (`array`), so slot 1 of a
/// real-layout object is a subclass's field (netty's `MpscIntQueue`
/// extends `AtomicIntegerArray`). The synthetic `<init>(I)V` must not
/// write it, and `length()` must answer the backing array's length.
#[test]
fn fs3_2_atomic_int_long_arrays_keep_no_length_slot() {
    let reg = register_atomics();
    for (class, ctor_len) in [
        ("java/util/concurrent/atomic/AtomicIntegerArray", 5),
        ("java/util/concurrent/atomic/AtomicLongArray", 3),
    ] {
        let mut ctx = make_ctx();
        let obj = try_alloc_concurrent_synthetic(&mut ctx, class, 2).unwrap();
        // A subclass field that happens to sit at slot 1.
        ctx.set_field(obj, 1, Value::Int(77));
        let init = reg.find(class, "<init>", "(I)V").unwrap();
        init(&mut ctx, &[Value::Object(Some(obj)), Value::Int(ctor_len)]).unwrap();
        assert_eq!(ctx.get_field(obj, 1), Value::Int(77), "{class}: <init> wrote slot 1");
        let length = reg.find(class, "length", "()I").unwrap();
        let got = length(&mut ctx, &[Value::Object(Some(obj))]).unwrap();
        assert_eq!(got, Some(Value::Int(ctor_len)), "{class}: length()");
    }
}

/// RD.1 — AtomicInteger CAS semantics (single-threaded mock).
#[test]
fn rd1_atomic_integer_cas_and_rmw() {
    let reg = register_atomics();
    let ai = "java/util/concurrent/atomic/AtomicInteger";
    let mut ctx = make_ctx();
    let obj = try_alloc_concurrent_synthetic(&mut ctx, ai, 1).unwrap();

    let init = reg.find(ai, "<init>", "(I)V").unwrap();
    init(&mut ctx, &[Value::Object(Some(obj)), Value::Int(10)]).unwrap();
    assert_eq!(ctx.get_field(obj, 0), Value::Int(10));

    let cas = reg.find(ai, "compareAndSet", "(II)Z").unwrap();
    let ok = cas(
        &mut ctx,
        &[Value::Object(Some(obj)), Value::Int(10), Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    assert_eq!(ctx.get_field(obj, 0), Value::Int(42));

    let ok2 = cas(
        &mut ctx,
        &[Value::Object(Some(obj)), Value::Int(10), Value::Int(99)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok2, Value::Int(0));
    assert_eq!(ctx.get_field(obj, 0), Value::Int(42));

    let gaa = reg.find(ai, "getAndAdd", "(I)I").unwrap();
    let prev = gaa(&mut ctx, &[Value::Object(Some(obj)), Value::Int(8)])
        .unwrap()
        .unwrap();
    assert_eq!(prev, Value::Int(42));
    assert_eq!(ctx.get_field(obj, 0), Value::Int(50));

    let iag = reg.find(ai, "incrementAndGet", "()I").unwrap();
    let got = iag(&mut ctx, &[Value::Object(Some(obj))]).unwrap().unwrap();
    assert_eq!(got, Value::Int(51));

    let caex = reg.find(ai, "compareAndExchange", "(II)I").unwrap();
    let old = caex(
        &mut ctx,
        &[Value::Object(Some(obj)), Value::Int(51), Value::Int(7)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old, Value::Int(51));
    assert_eq!(ctx.get_field(obj, 0), Value::Int(7));

    let old2 = caex(
        &mut ctx,
        &[Value::Object(Some(obj)), Value::Int(99), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old2, Value::Int(7));
    assert_eq!(ctx.get_field(obj, 0), Value::Int(7));
}

/// RD.1 — AtomicLong wraps and CAS behaves like AtomicInteger.
#[test]
fn rd1_atomic_long_rmw() {
    let reg = register_atomics();
    let al = "java/util/concurrent/atomic/AtomicLong";
    let mut ctx = make_ctx();
    let obj = try_alloc_concurrent_synthetic(&mut ctx, al, 1).unwrap();

    let init = reg.find(al, "<init>", "(J)V").unwrap();
    init(&mut ctx, &[Value::Object(Some(obj)), Value::Long(i64::MAX)]).unwrap();

    let iag = reg.find(al, "incrementAndGet", "()J").unwrap();
    let wrapped = iag(&mut ctx, &[Value::Object(Some(obj))]).unwrap().unwrap();
    assert_eq!(wrapped, Value::Long(i64::MIN));

    let cas = reg.find(al, "compareAndSet", "(JJ)Z").unwrap();
    let ok = cas(
        &mut ctx,
        &[
            Value::Object(Some(obj)),
            Value::Long(i64::MIN),
            Value::Long(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    assert_eq!(ctx.get_field(obj, 0), Value::Long(0));

    let gai = reg.find(al, "getAndIncrement", "()J").unwrap();
    for _ in 0..100 {
        let _ = gai(&mut ctx, &[Value::Object(Some(obj))]).unwrap();
    }
    assert_eq!(ctx.get_field(obj, 0), Value::Long(100));
}

// -----------------------------------------------------------------------
// AtomicLong.VMSupportsCS8 — static <clinit>-time query (T19 N4)
// -----------------------------------------------------------------------

#[test]
fn t19_n4_atomic_long_vm_supports_cs8_on_64bit_returns_true() {
    let mut ctx = make_ctx();
    let result = super::native_atomic_long_vm_supports_cs8(&mut ctx, &[])
        .unwrap()
        .unwrap();
    // Our CI runs on x86_64 / aarch64 — both 64-bit.  Assert true (1).
    assert_eq!(result, Value::Int(1));
}

#[test]
fn t19_n4_atomic_long_vm_supports_cs8_ignores_extra_args() {
    // Spec: no params.  The Java verifier guarantees correct arity, so
    // trailing junk arguments must be silently ignored (no panic, no
    // validation error).
    let mut ctx = make_ctx();
    let result = super::native_atomic_long_vm_supports_cs8(&mut ctx, &[Value::Int(42)])
        .unwrap()
        .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
fn t19_n4_atomic_long_vm_supports_cs8_registered() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    super::register_atomic_long_natives(&mut registry);
    assert!(registry
        .find(
            "java/util/concurrent/atomic/AtomicLong",
            "VMSupportsCS8",
            "()Z",
        )
        .is_some());
}

#[test]
fn t19_n4_atomic_long_vm_supports_cs8_registered_in_phase54() {
    // The synthetic-JDK atomics bundle also wires VMSupportsCS8 so that
    // real-JDK bytecode can initialize AtomicLong without hitting
    // UnsatisfiedLinkError during <clinit>.
    let reg = register_atomics();
    assert!(reg
        .find(
            "java/util/concurrent/atomic/AtomicLong",
            "VMSupportsCS8",
            "()Z",
        )
        .is_some());
}

/// RD.2 — AtomicReference CAS uses reference identity, not .equals().
#[test]
fn atomic_reference_to_string_delegates_to_value() {
    let reg = register_atomics();
    let ar = "java/util/concurrent/atomic/AtomicReference";
    let mut ctx = make_ctx();
    let obj = try_alloc_concurrent_synthetic(&mut ctx, ar, 1).unwrap();
    let value = ctx.create_string("CLOSED");

    let init = reg.find(ar, "<init>", "(Ljava/lang/Object;)V").unwrap();
    init(
        &mut ctx,
        &[Value::Object(Some(obj)), Value::Object(Some(value))],
    )
    .unwrap();

    let to_string = reg.find(ar, "toString", "()Ljava/lang/String;").unwrap();
    let result = to_string(&mut ctx, &[Value::Object(Some(obj))]).unwrap();
    let rendered = match result {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap(),
        other => panic!("AtomicReference.toString returned {other:?}"),
    };
    assert_eq!(rendered, "CLOSED");
}

#[test]
fn rd2_atomic_reference_identity_cas() {
    let reg = register_atomics();
    let ar = "java/util/concurrent/atomic/AtomicReference";
    let mut ctx = make_ctx();
    let obj = try_alloc_concurrent_synthetic(&mut ctx, ar, 1).unwrap();

    let s_a1 = ctx.create_string("hello");
    let s_a2 = ctx.create_string("hello");

    let init = reg.find(ar, "<init>", "(Ljava/lang/Object;)V").unwrap();
    init(
        &mut ctx,
        &[Value::Object(Some(obj)), Value::Object(Some(s_a1))],
    )
    .unwrap();

    let cas = reg
        .find(
            ar,
            "compareAndSet",
            "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        )
        .unwrap();
    let ok = cas(
        &mut ctx,
        &[
            Value::Object(Some(obj)),
            Value::Object(Some(s_a2)),
            Value::Object(Some(s_a2)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        ok,
        Value::Int(0),
        "reference-identity CAS must not treat .equals() as =="
    );
    assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(s_a1)));

    let ok2 = cas(
        &mut ctx,
        &[
            Value::Object(Some(obj)),
            Value::Object(Some(s_a1)),
            Value::Object(Some(s_a2)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok2, Value::Int(1));
    assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(s_a2)));
}

/// RD.4 — ReentrantLock reentrance counter increments and decrements.
#[test]
fn rd4_reentrant_lock_depth_tracking() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    let held = native_rl_is_held_by_current_thread(&mut ctx, &[Value::Object(Some(lock))])
        .unwrap()
        .unwrap();
    assert_eq!(held, Value::Int(1));
    let hc = native_rl_get_hold_count(&mut ctx, &[Value::Object(Some(lock))])
        .unwrap()
        .unwrap();
    assert_eq!(hc, Value::Int(3));

    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 2);
    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
    native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);

    // Unlock without holding must raise IllegalMonitorStateException.
    let err = native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]);
    assert!(err.is_err());
}

/// RD.5 — Condition.await(long, TimeUnit) uses TimeUnit conversion and
/// reacquires the lock on return.
#[test]
fn rd5_condition_await_timeunit_conversion() {
    let _guard = stamped_test_lock();
    let mut ctx = make_ctx();
    let lock =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 1)
            .unwrap();
    native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
    native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

    let cond_v = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
        .unwrap()
        .unwrap();
    let cond = match cond_v {
        Value::Object(Some(c)) => c,
        _ => panic!("condition"),
    };

    // TimeUnit.SECONDS ordinal = 3; field 0 carries the ordinal.
    let tu = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(tu, 0, Value::Int(3));
    let res = native_cond_await_timeout(
        &mut ctx,
        &[
            Value::Object(Some(cond)),
            Value::Long(1),
            Value::Object(Some(tu)),
        ],
    )
    .unwrap()
    .unwrap();
    match res {
        Value::Int(v) => assert!(v == 0 || v == 1, "result must be boolean (got {v})"),
        _ => panic!("expected Int"),
    }
    // Lock reacquired on return.
    assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
}

/// JIT r23 wave 8 (lane W8-L2): a `CompletableFuture` that is not the
/// fabricated stub (the mock answers `is_class_synthetic_stub` with its
/// `false` default, as a real JDK does) is allocated at the real class's
/// two fields whatever model the factory writes, so a floor-exempt real
/// class is never built wider than it declares.
#[test]
fn r23_w8_cf_model_is_real_width_over_a_real_class() {
    let mut ctx = make_ctx();
    assert!(!ctx.is_class_synthetic_stub("java/util/concurrent/CompletableFuture"));
    for model in [CfModel::Three, CfModel::Four] {
        let cf = try_alloc_completable_future_model(&mut ctx, model).unwrap();
        assert_eq!(ctx.object_num_fields(cf), 2, "{model:?}");
    }
    // The factories that use it still answer the slot-0/1 contract.
    let v = ctx.create_string("v");
    let done = native_cf_completed(&mut ctx, &[Value::Object(Some(v))])
        .unwrap()
        .unwrap();
    let Value::Object(Some(done)) = done else {
        panic!("expected a future")
    };
    assert_eq!(ctx.object_num_fields(done), 2);
    assert_eq!(ctx.get_field(done, FUT_FIELD_RESULT), Value::Object(Some(v)));
    assert_eq!(ctx.get_field(done, FUT_FIELD_DONE), Value::Int(1));
}

/// RD.8 — CompletableFuture.thenApply propagates exceptional state
/// downstream without invoking the Function.
#[test]
fn rd8_completable_future_exceptional_passthrough() {
    let mut ctx = make_ctx();
    let cf =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CompletableFuture", 4)
            .unwrap();
    let err_msg = ctx.create_string("boom");
    ctx.set_field(cf, FUT_FIELD_RESULT, Value::Object(Some(err_msg)));
    ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(2));

    let func = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let result = native_cf_then_apply(
        &mut ctx,
        &[Value::Object(Some(cf)), Value::Object(Some(func))],
    )
    .unwrap()
    .unwrap();
    let downstream = match result {
        Value::Object(Some(o)) => o,
        _ => panic!("expected downstream CF"),
    };
    assert_eq!(ctx.get_field(downstream, FUT_FIELD_DONE), Value::Int(2));
    assert_eq!(
        ctx.get_field(downstream, FUT_FIELD_RESULT),
        Value::Object(Some(err_msg))
    );
}

/// RD.9 — Thread.sleep(0, 500_000) honours sub-millisecond precision.
#[test]
fn rd9_thread_sleep_sub_millisecond_precision() {
    let mut ctx = make_ctx();
    let start = std::time::Instant::now();
    crate::lang_system::native_thread_sleep_millis_nanos(
        &mut ctx,
        &[Value::Long(0), Value::Int(500_000)],
    )
    .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_nanos() >= 500_000,
        "sleep(0, 500_000) too short: {}ns",
        elapsed.as_nanos()
    );
}

/// RD.9 — argument validation: negative millis and out-of-range nanos.
#[test]
fn rd9_thread_sleep_argument_validation() {
    let mut ctx = make_ctx();
    let neg = crate::lang_system::native_thread_sleep_millis_nanos(
        &mut ctx,
        &[Value::Long(-1), Value::Int(0)],
    );
    assert!(neg.is_err(), "negative millis must raise");
    let bad_nanos = crate::lang_system::native_thread_sleep_millis_nanos(
        &mut ctx,
        &[Value::Long(0), Value::Int(1_000_000)],
    );
    assert!(bad_nanos.is_err(), "nanos >= 1_000_000 must raise");
}

/// RD.10 — Thread.join(long) rejects negative timeouts.
#[test]
fn rd10_thread_join_rejects_negative_timeout() {
    let mut ctx = make_ctx();
    let t = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
    let err = crate::lang_system::native_thread_join_timed(
        &mut ctx,
        &[Value::Object(Some(t)), Value::Long(-5)],
    );
    assert!(err.is_err());
}

/// RD.10 — Thread.join(long, int) validates nanosecond range.
#[test]
fn rd10_thread_join_nanos_range() {
    let mut ctx = make_ctx();
    let t = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
    let err = crate::lang_system::native_thread_join_millis_nanos(
        &mut ctx,
        &[
            Value::Object(Some(t)),
            Value::Long(10),
            Value::Int(2_000_000),
        ],
    );
    assert!(err.is_err());
    let ok = crate::lang_system::native_thread_join_millis_nanos(
        &mut ctx,
        &[Value::Object(Some(t)), Value::Long(0), Value::Int(1)],
    );
    assert!(ok.is_ok());
}

/// An AQLS-shaped receiver: the class `AbstractQueuedLongSynchronizer`
/// declaring `state` at slot 2 of 4, the slot pre-typed as the VM types a
/// fresh `long` field (`Long(0)`).
fn i73_aqls_receiver(ctx: &mut MockNativeContext) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(AQLS_CLASS).unwrap();
    ctx.set_declared_fields(
        cid,
        vec![cratonvm_native_api::FieldMetadata {
            name: "state".to_string(),
            descriptor: "J".to_string(),
            access_flags: 0x0042, // private volatile
            slot_index: 2,
            declaring_class_id: cid,
            is_static: false,
        }],
    );
    let synchronizer = ctx.alloc_object(cid, 4);
    ctx.set_field(synchronizer, 2, Value::Long(0));
    synchronizer
}

/// Interpreter round i3 wave 73, lane L1
/// (`i72-L1-proposal-retire-the-aqls-side-table-20261105`): the `=0`
/// arm's stand-ins keep AQLS's word in the object's own `state` field,
/// not in a process-global side table, so a field read (reflection, a
/// debugger, serialization) and `getState()` agree, and no VM's state can
/// reach another's.
#[test]
fn aqls_state_natives_preserve_atomic_long_transitions() {
    let mut ctx = make_ctx();
    let synchronizer = i73_aqls_receiver(&mut ctx);
    let this = Value::Object(Some(synchronizer));

    assert_eq!(
        native_aqls_get_state(&mut ctx, &[this]).unwrap().unwrap(),
        Value::Long(0)
    );
    // A fresh synchronizer's first acquire: CAS from the field's default.
    assert_eq!(
        native_aqls_compare_and_set_state(&mut ctx, &[this, Value::Long(0), Value::Long(1)])
            .unwrap()
            .unwrap(),
        Value::Int(1)
    );
    native_aqls_set_state(&mut ctx, &[this, Value::Long(4)]).unwrap();
    assert_eq!(
        native_aqls_compare_and_set_state(&mut ctx, &[this, Value::Long(4), Value::Long(8)])
            .unwrap()
            .unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        native_aqls_compare_and_set_state(&mut ctx, &[this, Value::Long(4), Value::Long(12)])
            .unwrap()
            .unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        native_aqls_get_state(&mut ctx, &[this]).unwrap().unwrap(),
        Value::Long(8)
    );
    // The word is the field: what a `getfield` of `state` reads.
    assert_eq!(ctx.get_field(synchronizer, 2), Value::Long(8));
    // And a field write (a deserialized synchronizer) is what
    // `getState()` answers.
    ctx.set_field(synchronizer, 2, Value::Long(1 << 40));
    assert_eq!(
        native_aqls_get_state(&mut ctx, &[this]).unwrap().unwrap(),
        Value::Long(1 << 40)
    );
}

/// Two VMs (two contexts) never share a synchronizer's word: each lives
/// in its own heap's field. Under the side table both were rows of one
/// process-wide map.
#[test]
fn aqls_state_natives_keep_no_state_across_contexts() {
    let mut first = make_ctx();
    let mut second = make_ctx();
    let a = i73_aqls_receiver(&mut first);
    let b = i73_aqls_receiver(&mut second);
    native_aqls_set_state(&mut first, &[Value::Object(Some(a)), Value::Long(7)]).unwrap();
    assert_eq!(
        native_aqls_get_state(&mut second, &[Value::Object(Some(b))])
            .unwrap()
            .unwrap(),
        Value::Long(0)
    );
    assert_eq!(first.get_field(a, 2), Value::Long(7));
}

/// No `state` field resolvable (a class library with no AQLS): the
/// stand-ins fail loudly rather than answer from nowhere.
#[test]
fn aqls_state_natives_without_the_field_raise_no_such_field() {
    let mut ctx = make_ctx();
    let other = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let got = native_aqls_get_state(&mut ctx, &[Value::Object(Some(other))]);
    assert!(
        matches!(
            got,
            Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Linkage(
                    cratonvm_types::error::LinkageError::NoSuchFieldError { .. }
                )
            ))
        ),
        "{got:?}"
    );
}
