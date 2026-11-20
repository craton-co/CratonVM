use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Serialize tests that mutate the global `Bits` accounting state.
/// Without this, parallel `cargo test` runs see racey reserved-byte
/// counters across tests.
fn bits_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// A pooled temporary direct buffer must not leak its global root when it
/// is handed back out. `TEMPORARY_BUFFER_POOL_LIMIT` bounds the pool at
/// three entries per thread; the JNI global-ref table backing those entries
/// is not bounded, so a `get` that reused an entry without releasing its
/// handle leaked one root per NIO transfer — 57 036 unreclaimable roots
/// against 7, over three H2 `TestFileSystem` filesystems in ~4.5 s. Without
/// the release this test sees exactly one leaked root per cycle.
#[test]
fn temporary_direct_buffer_reuse_releases_the_pooled_root() {
    // The pool is thread-local and outlives any one test; start from empty
    // so the baseline below describes only this test's roots.
    TEMPORARY_BUFFERS.with(|e| e.borrow_mut().clear());
    let mut ctx = MockNativeContext::new();
    let baseline = ctx.global_root_count();

    const CYCLES: usize = 50;
    let mut first = None;
    let mut last = None;
    for _ in 0..CYCLES {
        let Ok(Some(Value::Object(Some(buffer)))) =
            temporary_direct_buffer_get(&mut ctx, &[Value::Int(4096)])
        else {
            panic!("getTemporaryDirectBuffer returned no buffer");
        };
        // `capacity` is what the release path screens on; the mock's
        // constructor does not populate it.
        ctx.set_field_by_name(buffer, "capacity", Value::Int(4096));
        first.get_or_insert(buffer);
        last = Some(buffer);
        temporary_direct_buffer_release(&mut ctx, &[Value::Object(Some(buffer))])
            .expect("releaseTemporaryDirectBuffer");
    }

    // If the pool never hit, the leak this guards could not arise and the
    // count assertion below would pass for the wrong reason.
    assert_eq!(first, last, "the pooled buffer was never reused");
    let leaked = ctx.global_root_count();
    assert!(
        leaked <= baseline + TEMPORARY_BUFFER_POOL_LIMIT,
        "{CYCLES} get/release cycles left {leaked} global roots \
         (baseline {baseline}); the pool holds at most \
         {TEMPORARY_BUFFER_POOL_LIMIT}"
    );
    TEMPORARY_BUFFERS.with(|e| e.borrow_mut().clear());
}

#[test]
fn wp35_bits_reserve_unreserve_balances() {
    let _g = bits_test_lock();
    let baseline = bits().reserved.load(Ordering::Relaxed);
    let mut ctx = MockNativeContext::new();
    bits_reserve_memory(&mut ctx, &[Value::Long(4096), Value::Long(4096)]).unwrap();
    bits_reserve_memory(&mut ctx, &[Value::Long(8192), Value::Long(8192)]).unwrap();
    assert_eq!(
        bits().reserved.load(Ordering::Relaxed),
        baseline + 4096 + 8192
    );
    bits_unreserve_memory(&mut ctx, &[Value::Long(4096), Value::Long(4096)]).unwrap();
    bits_unreserve_memory(&mut ctx, &[Value::Long(8192), Value::Long(8192)]).unwrap();
    assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline);
}

#[test]
fn wp35_reserve_rejects_when_over_max() {
    let _g = bits_test_lock();
    let saved = bits().max.swap(1024, Ordering::Relaxed);
    let baseline = bits().reserved.load(Ordering::Relaxed);
    let mut ctx = MockNativeContext::new();
    // Drain headroom first.
    let used = (1024 - baseline).max(0);
    if used > 0 {
        bits_reserve_memory(&mut ctx, &[Value::Long(used), Value::Long(used)]).unwrap();
    }
    // Now any further reserve must fail with OOM.
    let err = bits_reserve_memory(&mut ctx, &[Value::Long(1), Value::Long(1)]);
    assert!(err.is_err(), "expected OOM, got {:?}", err);
    if used > 0 {
        bits_unreserve_memory(&mut ctx, &[Value::Long(used), Value::Long(used)]).unwrap();
    }
    bits().max.store(saved, Ordering::Relaxed);
}

#[test]
fn bug_h2_largeblob_configure_max_direct_memory_round_trips() {
    let _g = bits_test_lock();
    let saved = bits().max.load(Ordering::Relaxed);
    configure_max_direct_memory(777 * 1024 * 1024);
    assert_eq!(bits().max.load(Ordering::Relaxed), 777 * 1024 * 1024);
    // Negative input (e.g. an overflowed/garbage config value) must clamp
    // to 0 rather than going negative, which would make every reservation
    // trivially pass the `next > max` check below zero.
    configure_max_direct_memory(-5);
    assert_eq!(bits().max.load(Ordering::Relaxed), 0);
    bits().max.store(saved, Ordering::Relaxed);
}

#[test]
fn wp35_allocate_then_free_returns_to_pool_for_4k() {
    let _g = bits_test_lock();
    let baseline = bits().reserved.load(Ordering::Relaxed);
    let addr = dbb_allocate(4096).expect("alloc 4k");
    assert_ne!(addr, 0);
    assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline + 4096);
    dbb_free(addr, 4096);
    assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline);
    // Second 4k alloc should hit the pool — RSS-bounded behaviour.
    // We can't strictly require `addr2 == addr` because earlier tests
    // (or tests interleaved on parallel threads) may have populated
    // the same bucket; the invariant is that allocation succeeds and
    // accounting balances after free.
    let addr2 = dbb_allocate(4096).expect("alloc 4k from pool");
    assert_ne!(addr2, 0);
    assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline + 4096);
    dbb_free(addr2, 4096);
    assert_eq!(bits().reserved.load(Ordering::Relaxed), baseline);
}

#[test]
fn wp35_allocate_direct0_defaults_to_big_endian_order() {
    let _g = bits_test_lock();
    let baseline = bits().reserved.load(Ordering::Relaxed);
    let mut ctx = MockNativeContext::new();

    let result = dbb_allocate_direct0(&mut ctx, &[Value::Int(0)])
        .expect("allocateDirect0 should not throw");
    let buf = match result {
        Some(Value::Object(Some(o))) => o,
        other => panic!("allocateDirect0 returned {other:?}"),
    };

    assert_eq!(ctx.get_field_by_name(buf, "bigEndian"), Value::Int(1));
    assert_eq!(
        ctx.get_field_by_name(buf, "nativeByteOrder"),
        Value::Int(if cfg!(target_endian = "big") { 1 } else { 0 })
    );
    assert_eq!(
        bits().reserved.load(Ordering::Relaxed),
        baseline,
        "zero-capacity allocation should not touch Bits accounting"
    );
}

#[test]
fn wp35_zero_size_alloc_yields_null_address() {
    let addr = dbb_allocate(0).unwrap();
    assert_eq!(addr, 0);
    // No accounting impact for zero-size.
    dbb_free(0, 0);
}

#[test]
fn wp35_cleaner_expired_is_idempotent() {
    let pending_before = cleaners_pending_count();
    let id = register_cleaner(0xdead_beef, 0); // size=0 → free is no-op
    assert!(cleaners_pending_count() >= pending_before + 1);
    assert!(fire_cleaner(id));
    assert!(!fire_cleaner(id));
    assert_eq!(cleaners_pending_count(), pending_before);
}

#[test]
fn wp35_get_max_direct_memory_returns_soft_cap() {
    let mut ctx = MockNativeContext::new();
    let r = bits_get_max_direct_memory(&mut ctx, &[]).unwrap().unwrap();
    match r {
        Value::Long(v) => assert!(v > 0),
        other => panic!("unexpected return {:?}", other),
    }
}

#[test]
fn wp35_pool_bucket_classification() {
    assert_eq!(bucket_for(0), None);
    assert_eq!(bucket_for(1), Some(0)); // round to 64 (2^6) → idx 0
    assert_eq!(bucket_for(64), Some(0)); // exact bucket-0 fit
    assert_eq!(bucket_for(4096), Some(6)); // 2^12, idx = 12-6 = 6
                                           // Beyond 2 MiB falls through to direct system free.
    assert_eq!(bucket_for(8 * 1024 * 1024), None);
}

#[test]
fn wp35_canonical_alloc_size_floors_at_bucket_base() {
    // Sub-64-B requests round up to the 64-B base bucket size.
    assert_eq!(canonical_alloc_size(1), Some(64));
    assert_eq!(canonical_alloc_size(60), Some(64));
    assert_eq!(canonical_alloc_size(64), Some(64));
    // Above base: plain next-power-of-two.
    assert_eq!(canonical_alloc_size(65), Some(128));
    assert_eq!(canonical_alloc_size(100), Some(128));
    assert_eq!(canonical_alloc_size(4096), Some(4096));
    assert_eq!(canonical_alloc_size(4097), Some(8192));
    assert_eq!(canonical_alloc_size(0), None);
    // Overflowing round must not panic — it reports unallocatable.
    assert_eq!(canonical_alloc_size(usize::MAX), None);
}

#[test]
fn wp35_layout_is_pure_function_of_bucket() {
    // The core invariant this fix protects: any two logical sizes that
    // share a bucket must produce the IDENTICAL allocation `Layout`, so a
    // pooled block minted for one request and freed under a different
    // (same-bucket) request always deallocs with the size+align it was
    // allocated with — never a mismatched `Layout` (global-allocator UB).
    let a = dbb_layout(60).expect("layout 60");
    let b = dbb_layout(64).expect("layout 64");
    let c = dbb_layout(1).expect("layout 1");
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert_eq!(a.size(), 64);
    assert_eq!(a.align(), DBB_ALIGN);

    // A request that the pool serves from a larger same-bucket block:
    // size 100 and size 128 both canonicalise to 128.
    assert_eq!(dbb_layout(100), dbb_layout(128));
    // Different buckets => different Layouts (sanity).
    assert_ne!(dbb_layout(64), dbb_layout(128));
}

#[test]
fn wp35_pool_records_canonical_size_for_layout_safe_eviction() {
    let _g = bits_test_lock();
    // Allocate a block whose logical size (100) is smaller than its
    // canonical allocation size (128), free it (parks in the pool), then
    // confirm the parked entry carries the canonical size — the value used
    // to rebuild the original `Layout` on eviction. Without this, eviction
    // would dealloc 100 bytes against a 128-byte allocation (UB).
    let addr = dbb_allocate(100).expect("alloc 100");
    assert_ne!(addr, 0);
    dbb_free(addr, 100);
    let idx = bucket_for(100).expect("bucket");
    let bucket = pool().buckets[idx].lock().expect("lock");
    let parked = bucket
        .iter()
        .find(|e| e.addr == addr as usize)
        .expect("parked entry present");
    assert_eq!(
        parked.size,
        canonical_alloc_size(100).unwrap(),
        "pooled entry must record canonical alloc size, not logical request"
    );
}
