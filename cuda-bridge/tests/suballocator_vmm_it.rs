//! Integration tests for Phase 1 (`SubAllocator`, stream-ordered reuse,
//! `PendingFree` reclaim, two-tier OOM recovery) and Phase 5 (`VmmPool` 2 MiB
//! chunk mapping, partial release, and non-moving defragment/remap).
//!
//! Pure-CPU tests run unconditionally (`cargo test -p cuda-bridge`); real
//! hardware CUDA tests are gated behind `#[cfg(feature = "cuda")]` and
//! `#[ignore = "requires a real CUDA device; run with --ignored"]`.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cratonvm_cuda_bridge as cuda_bridge;

use cuda_bridge::allocator::{
    bind_vram_mirrors, notify_vram_freed, reclaim_all_completed_pools,
    register_vram_pressure_gc_hook, set_vram_reservation_limit, vram_reservation_limit_bytes,
    vram_usage_bytes, BackingSlab, FenceEvent, SlabBackend, SubAllocator, DEFAULT_SLAB_BYTES,
    MIN_BLOCK_ALIGN,
};
use cuda_bridge::vmm::{
    VmmDriver, VmmPool, DEFAULT_VMM_CHUNK_BYTES, DEFAULT_VMM_RESERVE_BYTES,
};
use cuda_bridge::{DeviceError, Result};

/// Serialize tests that mutate process-wide reservation limits or GC hooks.
static GLOBAL_COORD_LOCK: Mutex<()> = Mutex::new(());

struct LimitResetGuard;
impl Drop for LimitResetGuard {
    fn drop(&mut self) {
        set_vram_reservation_limit(0);
    }
}

#[derive(Clone)]
struct MockEvent {
    done: Arc<AtomicBool>,
    sync_calls: Arc<AtomicU64>,
}

impl MockEvent {
    fn new(initially_done: bool) -> Self {
        Self {
            done: Arc::new(AtomicBool::new(initially_done)),
            sync_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    fn complete(&self) {
        self.done.store(true, Ordering::Release);
    }

    fn sync_count(&self) -> u64 {
        self.sync_calls.load(Ordering::Relaxed)
    }
}

impl FenceEvent for MockEvent {
    fn is_complete(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn sync(&self) -> Result<()> {
        self.sync_calls.fetch_add(1, Ordering::Relaxed);
        self.done.store(true, Ordering::Release);
        Ok(())
    }
}

struct MockSlabBackend {
    next_ptr: AtomicU64,
    max_bytes: AtomicU64,
    allocated_bytes: AtomicU64,
    freed_slabs: AtomicU64,
}

impl MockSlabBackend {
    fn new(max_bytes: u64) -> Self {
        Self {
            next_ptr: AtomicU64::new(0x10_0000),
            max_bytes: AtomicU64::new(max_bytes),
            allocated_bytes: AtomicU64::new(0),
            freed_slabs: AtomicU64::new(0),
        }
    }
}

impl SlabBackend for MockSlabBackend {
    type Slab = u64;

    fn alloc_slab(&self, min_bytes: usize) -> Result<BackingSlab<Self::Slab>> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        let cur = self.allocated_bytes.load(Ordering::Relaxed);
        if max > 0 && cur.saturating_add(min_bytes as u64) > max {
            return Err(DeviceError::Driver("mock OOM".into()));
        }
        self.allocated_bytes
            .fetch_add(min_bytes as u64, Ordering::Relaxed);
        let ptr = self
            .next_ptr
            .fetch_add((min_bytes as u64).max(4096), Ordering::Relaxed);
        Ok(BackingSlab {
            base_ptr: ptr,
            size_bytes: min_bytes,
            handle: ptr,
        })
    }

    fn free_slab(&self, slab: BackingSlab<Self::Slab>) {
        self.allocated_bytes
            .fetch_sub(slab.size_bytes as u64, Ordering::Relaxed);
        self.freed_slabs.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn suballocator_splits_and_coalesces_free_blocks() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(0),
        4096,
        true,
    );

    let a = pool.alloc(1024, None).expect("alloc a");
    let b = pool.alloc(1024, None).expect("alloc b");
    let c = pool.alloc(2048, None).expect("alloc c");

    assert_eq!(a.alloc_bytes, 1024);
    assert_eq!(b.alloc_bytes, 1024);
    assert_eq!(c.alloc_bytes, 2048);
    assert_eq!(a.slab_id, b.slab_id);
    assert_eq!(b.slab_id, c.slab_id);
    assert_eq!(pool.stats().free_bytes, 0);

    // Free in non-adjacent order and ensure full coalescing restores 1 block of 4096.
    pool.free_deferred(a, None, None);
    pool.free_deferred(c, None, None);
    assert_eq!(pool.stats().free_block_count, 2);
    pool.free_deferred(b, None, None);

    let st = pool.stats();
    assert_eq!(st.free_bytes, 4096);
    assert_eq!(st.free_block_count, 1, "neighbors must coalesce into one 4096B block");

    let whole = pool.alloc(4096, None).expect("realloc coalesced 4096");
    assert_eq!(whole.ptr, a.ptr);
    assert_eq!(pool.stats().slab_count, 1);
    pool.free_deferred(whole, None, None);
}

#[test]
fn same_stream_pending_free_reuses_without_host_wait() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(4096),
        4096,
        true,
    );

    let ev = MockEvent::new(false);
    let buf = pool.alloc(2048, Some(7)).expect("initial alloc");
    let original_ptr = buf.ptr;

    pool.free_deferred(buf, Some(7), Some(ev.clone()));
    assert_eq!(pool.stats().pending_count, 1);
    assert_eq!(pool.stats().pending_bytes, 2048);

    // Reallocate 1024 on the SAME stream (7) while event is still incomplete.
    let reused = pool.alloc(1024, Some(7)).expect("same-stream reuse");
    assert_eq!(reused.ptr, original_ptr);
    assert_eq!(
        ev.sync_count(),
        0,
        "same-stream reuse must never block on host event sync"
    );
    assert!(!ev.is_complete());
    // Remaining 1024 stays pending on stream 7 with the same incomplete fence.
    assert_eq!(pool.stats().pending_bytes, 1024);

    pool.free_deferred(reused, Some(7), Some(ev.clone()));
    ev.complete();
    let reclaimed = reclaim_all_completed_pools();
    assert!(reclaimed >= 2);
    assert_eq!(pool.stats().pending_count, 0);
    assert_eq!(pool.stats().free_bytes, 4096);
}

#[test]
fn cross_stream_reclaim_polls_events_and_frees_vram_mirrors() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    static MIRROR_USED: AtomicU64 = AtomicU64::new(0);
    static MIRROR_LIMIT: AtomicU64 = AtomicU64::new(0);
    bind_vram_mirrors(&MIRROR_USED, &MIRROR_LIMIT);

    let _reset = LimitResetGuard;
    let prev_soft = cuda_bridge::allocator::vram_soft_watermark_percent();
    cuda_bridge::allocator::set_vram_soft_watermark_percent(0);
    set_vram_reservation_limit(4096);
    assert_eq!(vram_reservation_limit_bytes(), 4096);
    assert_eq!(MIRROR_LIMIT.load(Ordering::Relaxed), 4096);

    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(4096),
        4096,
        true,
    );

    let ev = MockEvent::new(false);
    let b1 = pool.alloc(2048, Some(1)).expect("b1");
    let b2 = pool.alloc(2048, Some(1)).expect("b2");
    assert_eq!(vram_usage_bytes(), 4096);
    assert_eq!(MIRROR_USED.load(Ordering::Relaxed), 4096);

    // Retire b2 on stream 1 with an uncompleted event: pending still counts as used.
    pool.free_deferred(b2, Some(1), Some(ev.clone()));
    assert_eq!(reclaim_all_completed_pools(), 0);
    assert_eq!(vram_usage_bytes(), 4096);

    // Complete event and reclaim non-blockingly.
    ev.complete();
    assert_eq!(reclaim_all_completed_pools(), 1);
    assert_eq!(vram_usage_bytes(), 2048);
    assert_eq!(MIRROR_USED.load(Ordering::Relaxed), 2048);

    // Cross-stream allocation on stream 2 fits immediately.
    let b3 = pool.alloc(2048, Some(2)).expect("cross-stream b3");
    assert_eq!(vram_usage_bytes(), 4096);

    pool.free_deferred(b1, None, None);
    pool.free_deferred(b3, None, None);
    assert_eq!(vram_usage_bytes(), 0);
    cuda_bridge::allocator::set_vram_soft_watermark_percent(prev_soft);
}

#[test]
fn tier2_oom_releases_locks_calls_gc_hook_and_wakes_on_condvar() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(4096);

    type TestPool = SubAllocator<MockSlabBackend, MockEvent>;
    static HELD_FOR_GC: Mutex<Option<(Arc<TestPool>, cuda_bridge::allocator::AllocLease)>> =
        Mutex::new(None);

    fn async_gc_hook() -> bool {
        let taken = HELD_FOR_GC.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some((pool, lease)) = taken {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(8));
                // `free_deferred` releases VRAM and calls `notify_vram_freed()`.
                pool.free_deferred(lease, None, None);
                notify_vram_freed();
            });
            true
        } else {
            false
        }
    }

    register_vram_pressure_gc_hook(async_gc_hook);

    let pool = TestPool::new_arc(MockSlabBackend::new(4096), 4096, true);
    let a = pool.alloc(2048, None).expect("alloc a");
    let b = pool.alloc(2048, None).expect("alloc b");
    *HELD_FOR_GC.lock().unwrap_or_else(|p| p.into_inner()) = Some((pool.clone(), b));

    // Request 2048 while at the 4096 limit: Tier 2 calls async_gc_hook, drops lock,
    // waits on the 50ms condvar, wakes when the background thread frees `b`, and succeeds.
    let c = pool
        .alloc(2048, Some(3))
        .expect("tier2 GC wakeup should recover allocation");
    assert_eq!(vram_usage_bytes(), 4096);

    // With nothing left for GC to reclaim, another alloc must return Err cleanly.
    let oom = pool.alloc(2048, Some(4));
    assert!(oom.is_err(), "exhausted VRAM limit must fail cleanly after Tier 2");

    pool.free_deferred(a, None, None);
    pool.free_deferred(c, None, None);
}

#[test]
fn minimum_alignment_rounds_up_to_256_bytes() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(0),
        DEFAULT_SLAB_BYTES,
        true,
    );
    let small = pool.alloc(13, None).expect("small alloc");
    assert_eq!(small.alloc_bytes, MIN_BLOCK_ALIGN);
    assert_eq!(small.requested_bytes, 13);
    pool.free_deferred(small, None, None);
}

// ---------------------------------------------------------------------------
// VMM 2 MiB chunk lifecycle tests (pure CPU MockVmmDriver)
// ---------------------------------------------------------------------------

struct MockVmmState {
    next_va: u64,
    next_handle: u64,
    reserved_vas: BTreeMap<u64, usize>,
    live_handles: HashSet<u64>,
    mapped: BTreeMap<u64, (usize, u64, bool)>,
    fail_after_creates: Option<usize>,
}

struct MockVmmDriver {
    state: Mutex<MockVmmState>,
}

impl MockVmmDriver {
    fn new() -> Self {
        Self {
            state: Mutex::new(MockVmmState {
                next_va: 0x7000_0000_0000,
                next_handle: 1,
                reserved_vas: BTreeMap::new(),
                live_handles: HashSet::new(),
                mapped: BTreeMap::new(),
                fail_after_creates: None,
            }),
        }
    }
}

impl VmmDriver for MockVmmDriver {
    type Handle = u64;

    fn granularity(&self) -> usize {
        DEFAULT_VMM_CHUNK_BYTES
    }

    fn reserve_va(&self, size: usize, _alignment: usize) -> Result<u64> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let va = st.next_va;
        st.next_va += size as u64 + (DEFAULT_VMM_CHUNK_BYTES as u64);
        st.reserved_vas.insert(va, size);
        Ok(va)
    }

    fn free_va(&self, va: u64, size: usize) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let recorded = st.reserved_vas.remove(&va).expect("reserved va exists");
        assert_eq!(recorded, size);
        Ok(())
    }

    fn create_physical(&self, size: usize) -> Result<Self::Handle> {
        assert_eq!(size % DEFAULT_VMM_CHUNK_BYTES, 0);
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(rem) = st.fail_after_creates.as_mut() {
            if *rem == 0 {
                return Err(DeviceError::Driver("injected create_physical OOM".into()));
            }
            *rem -= 1;
        }
        let h = st.next_handle;
        st.next_handle += 1;
        st.live_handles.insert(h);
        Ok(h)
    }

    fn release_physical(&self, handle: Self::Handle) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        assert!(st.live_handles.remove(&handle), "double free physical handle");
        Ok(())
    }

    fn map(&self, va: u64, size: usize, handle: Self::Handle) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        assert!(st.live_handles.contains(&handle));
        assert!(!st.mapped.contains_key(&va));
        st.mapped.insert(va, (size, handle, false));
        Ok(())
    }

    fn set_access(&self, va: u64, size: usize) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = st.mapped.get_mut(&va).expect("mapped before set_access");
        assert_eq!(entry.0, size);
        entry.2 = true;
        Ok(())
    }

    fn unmap(&self, va: u64, size: usize) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (sz, _, _) = st.mapped.remove(&va).expect("mapped before unmap");
        assert_eq!(sz, size);
        Ok(())
    }
}

#[test]
fn vmm_maps_unmaps_and_defragments_without_moving_active_chunks() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    let pool = VmmPool::new(MockVmmDriver::new(), DEFAULT_VMM_RESERVE_BYTES);

    let a = pool.acquire_slab(chunk).expect("slab A (2 MiB)");
    let b = pool.acquire_slab(chunk).expect("slab B (2 MiB)");
    let c = pool.acquire_slab(chunk).expect("slab C (2 MiB)");

    assert_eq!(a.size_bytes, chunk);
    assert_eq!(b.base_ptr, a.base_ptr + chunk as u64);
    assert_eq!(c.base_ptr, b.base_ptr + chunk as u64);

    // Keep B active in the middle, mark A and C idle so free physical chunks
    // are fragmented around B.
    pool.mark_chunks_idle(a.base_ptr, chunk);
    pool.mark_chunks_idle(c.base_ptr, chunk);

    let remapped = pool
        .defragment_or_remap(2)
        .expect("remap ok")
        .expect("should gather 2 idle chunks into contiguous 4 MiB slab");
    assert_eq!(remapped.size_bytes, 2 * chunk);
    assert_ne!(remapped.base_ptr, b.base_ptr);

    let st = pool.stats();
    assert_eq!(st.remap_count, 1);
    assert_eq!(st.mapped_physical_bytes, 3 * chunk);
    assert_eq!(st.parked_physical_bytes, 0);

    // Release remapped slab cleanly while B stays active & untouched.
    let unmapped = pool.unmap_chunks(remapped.base_ptr, remapped.size_bytes, true);
    assert_eq!(unmapped, 2 * chunk);
    assert_eq!(pool.stats().mapped_physical_bytes, chunk);
    assert_eq!(
        pool.unmap_chunks(b.base_ptr, chunk / 2, true),
        0,
        "unaligned unmap_chunks must be rejected"
    );
    assert_eq!(pool.stats().mapped_physical_bytes, chunk);

    // Finally unmap B and drop pool cleanly (MockVmmDriver verifies all unmaps/frees).
    assert_eq!(pool.unmap_chunks(b.base_ptr, b.size_bytes, true), chunk);
    assert_eq!(pool.release_unused_chunks(false), 0);
}

struct SharedMockVmmDriver(Arc<MockVmmDriver>);

impl VmmDriver for SharedMockVmmDriver {
    type Handle = u64;
    fn granularity(&self) -> usize { self.0.granularity() }
    fn reserve_va(&self, size: usize, alignment: usize) -> cuda_bridge::Result<u64> {
        self.0.reserve_va(size, alignment)
    }
    fn free_va(&self, va: u64, size: usize) -> cuda_bridge::Result<()> {
        self.0.free_va(va, size)
    }
    fn create_physical(&self, size: usize) -> cuda_bridge::Result<Self::Handle> {
        self.0.create_physical(size)
    }
    fn release_physical(&self, handle: Self::Handle) -> cuda_bridge::Result<()> {
        self.0.release_physical(handle)
    }
    fn map(&self, va: u64, size: usize, handle: Self::Handle) -> cuda_bridge::Result<()> {
        self.0.map(va, size, handle)
    }
    fn set_access(&self, va: u64, size: usize) -> cuda_bridge::Result<()> {
        self.0.set_access(va, size)
    }
    fn unmap(&self, va: u64, size: usize) -> cuda_bridge::Result<()> {
        self.0.unmap(va, size)
    }
}

#[test]
fn vmm_mid_slab_rollback_on_driver_error() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    let driver = Arc::new(MockVmmDriver::new());
    driver.state.lock().unwrap().fail_after_creates = Some(2);
    let pool = VmmPool::new(SharedMockVmmDriver(driver.clone()), 4 * chunk);

    let err = pool
        .acquire_slab(3 * chunk)
        .expect_err("3rd chunk create_physical must fail");
    assert!(format!("{err:?}").contains("injected create_physical OOM"));

    let st = driver.state.lock().unwrap();
    assert!(st.mapped.is_empty(), "partial mappings must roll back");
    assert!(st.live_handles.is_empty(), "partial handles must release");
    assert!(st.reserved_vas.is_empty(), "empty VA region must free");
}

#[test]
fn native_stream_pool_delegation_uses_driver_pool_and_respects_toggle() {
    use cuda_bridge::allocator::{
        set_native_stream_pool_enabled, NATIVE_STREAM_POOL_SLAB_ID,
    };

    struct MockNativeStreamBackend {
        alloc_calls: AtomicUsize,
        free_calls: AtomicUsize,
        trim_calls: AtomicUsize,
        next_ptr: AtomicU64,
    }

    impl SlabBackend for MockNativeStreamBackend {
        type Slab = ();

        fn alloc_slab(&self, min_bytes: usize) -> cuda_bridge::Result<BackingSlab<Self::Slab>> {
            let base_ptr = self.next_ptr.fetch_add(min_bytes as u64, Ordering::Relaxed);
            Ok(BackingSlab {
                base_ptr,
                size_bytes: min_bytes,
                handle: (),
            })
        }

        fn free_slab(&self, _slab: BackingSlab<Self::Slab>) {}

        fn supports_native_stream_pool(&self) -> bool {
            true
        }

        fn alloc_from_stream_pool(
            &self,
            bytes: usize,
            _stream_id: Option<u64>,
        ) -> cuda_bridge::Result<Option<u64>> {
            self.alloc_calls.fetch_add(1, Ordering::Relaxed);
            let ptr = self.next_ptr.fetch_add(bytes as u64, Ordering::Relaxed);
            Ok(Some(ptr))
        }

        fn free_to_stream_pool(&self, _ptr: u64, _stream_id: Option<u64>) -> cuda_bridge::Result<()> {
            self.free_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn trim_stream_pool(&self, _min_bytes_to_keep: usize) -> cuda_bridge::Result<()> {
            self.trim_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    let _guard = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(0);

    let backend = MockNativeStreamBackend {
        alloc_calls: AtomicUsize::new(0),
        free_calls: AtomicUsize::new(0),
        trim_calls: AtomicUsize::new(0),
        next_ptr: AtomicU64::new(0x9000_0000),
    };
    let pool = SubAllocator::<MockNativeStreamBackend, MockEvent>::new_arc(
        backend,
        DEFAULT_SLAB_BYTES,
        true,
    );

    // Default (disabled): uses portable SubAllocator slab path.
    set_native_stream_pool_enabled(false);
    let portable = pool.alloc(1024, Some(1)).expect("portable alloc");
    assert_ne!(portable.slab_id, NATIVE_STREAM_POOL_SLAB_ID);
    assert_eq!(pool.backend().alloc_calls.load(Ordering::Relaxed), 0);
    pool.free_deferred(portable, Some(1), None);
    assert_eq!(pool.trim_idle_slabs(), 1);

    // Enabled: delegates directly to native stream-ordered pool.
    set_native_stream_pool_enabled(true);
    let native = pool.alloc(2048, Some(42)).expect("native stream pool alloc");
    assert_eq!(native.slab_id, NATIVE_STREAM_POOL_SLAB_ID);
    assert_eq!(pool.backend().alloc_calls.load(Ordering::Relaxed), 1);

    pool.free_deferred(native, Some(42), None);
    assert_eq!(pool.backend().free_calls.load(Ordering::Relaxed), 1);
    assert_eq!(pool.trim_idle_slabs(), 0);
    assert_eq!(pool.backend().trim_calls.load(Ordering::Relaxed), 1);

    set_native_stream_pool_enabled(false);
}

#[test]
fn proactive_watermark_triggers_background_reclaim_and_gc_hint() {
    use cuda_bridge::allocator::{
        proactive_reclaim_count, register_vram_soft_watermark_gc_hook,
        set_vram_soft_watermark_percent, vram_soft_watermark_bytes,
    };

    static SOFT_GC_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn soft_gc_hint() -> bool {
        SOFT_GC_CALLS.fetch_add(1, Ordering::Relaxed);
        true
    }

    let _guard = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;

    SOFT_GC_CALLS.store(0, Ordering::Relaxed);
    set_vram_reservation_limit(100 * 1024);
    set_vram_soft_watermark_percent(80);
    assert_eq!(vram_soft_watermark_bytes(), 80 * 1024);
    register_vram_soft_watermark_gc_hook(soft_gc_hint);

    let before_reclaims = proactive_reclaim_count();
    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(0),
        64 * 1024,
        true,
    );

    // Allocate 84 KiB (> 80% of 100 KiB) without hitting 100 KiB Tier 2 OOM.
    let lease = pool.alloc(84 * 1024, None).expect("below hard limit");

    let deadline = Instant::now() + Duration::from_secs(2);
    while (proactive_reclaim_count() == before_reclaims || SOFT_GC_CALLS.load(Ordering::Relaxed) == 0)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        proactive_reclaim_count() > before_reclaims,
        "proactive background pre-reclaim should fire above 80% watermark"
    );
    assert!(
        SOFT_GC_CALLS.load(Ordering::Relaxed) >= 1,
        "async host GC hint should fire above soft watermark"
    );

    pool.free_deferred(lease, None, None);
    set_vram_soft_watermark_percent(80);
}

#[test]
fn per_vm_and_multi_gpu_quotas_prevent_starvation() {
    use cuda_bridge::allocator::{
        clear_vm_vram_quotas, partition_vram_limit, vm_device_vram_limit_bytes,
        vm_device_vram_usage_bytes,
    };

    let _guard = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;

    // 2 VMs x 2 GPUs partitioned evenly from 16 KiB -> 4 KiB per (vm, dev).
    partition_vram_limit(&[101, 202], &[0, 1], 16 * 1024);
    assert_eq!(vm_device_vram_limit_bytes(101, 0), 4096);
    assert_eq!(vm_device_vram_limit_bytes(202, 1), 4096);

    let vm1_gpu0 = SubAllocator::<MockSlabBackend, MockEvent>::new_arc_with_quota(
        MockSlabBackend::new(0),
        4096,
        true,
        101,
        0,
    );
    let vm2_gpu0 = SubAllocator::<MockSlabBackend, MockEvent>::new_arc_with_quota(
        MockSlabBackend::new(0),
        4096,
        true,
        202,
        0,
    );

    // VM 1 fills its 4 KiB quota on GPU 0; a further alloc fails without starving VM 2.
    let a = vm1_gpu0.alloc(4096, None).expect("vm1 within 4 KiB quota");
    assert_eq!(vm_device_vram_usage_bytes(101, 0), 4096);
    assert!(vm1_gpu0.alloc(256, None).is_err(), "vm1 quota exhausted");

    // VM 2 still gets its full 4 KiB quota on GPU 0.
    let b = vm2_gpu0.alloc(4096, None).expect("vm2 unaffected by vm1");
    assert_eq!(vm_device_vram_usage_bytes(202, 0), 4096);

    vm1_gpu0.free_deferred(a, None, None);
    vm2_gpu0.free_deferred(b, None, None);
    assert_eq!(vm_device_vram_usage_bytes(101, 0), 0);
    assert_eq!(vm_device_vram_usage_bytes(202, 0), 0);

    clear_vm_vram_quotas(101);
    clear_vm_vram_quotas(202);
}

#[test]
fn graph_vmm_arena_reuses_fixed_va_and_releases_physical_without_pinning_pool() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use cuda_bridge::vmm::GraphArenaHandle;

    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    let pool = VmmPool::new(MockVmmDriver::new(), DEFAULT_VMM_RESERVE_BYTES);

    // General pool starts completely empty.
    assert_eq!(pool.stats().region_count, 0);

    let arena = pool
        .reserve_graph_arena(2 * chunk)
        .expect("reserve graph-scoped VMM arena");
    assert_eq!(pool.stats().region_count, 0, "must not pin general pool slabs");

    let addr1 = arena.alloc(1024).expect("first graph buffer");
    let addr2 = arena.alloc(chunk).expect("second graph buffer");
    assert_eq!(addr1, arena.base_va());
    assert_eq!(addr2, arena.base_va() + 1024);
    assert!(arena.mapped_bytes() >= chunk);

    // Release physical backing between graph executions while keeping fixed VA range.
    arena
        .release_physical_keeping_va()
        .expect("release physical");
    assert_eq!(arena.mapped_bytes(), 0);

    // Re-mapping before next replay restores physical backing at the same fixed VAs.
    arena.ensure_mapped().expect("remap before replay");
    assert!(arena.mapped_bytes() >= chunk);
    assert_eq!(arena.base_va(), addr1);
}

#[test]
fn vmm_defragment_after_idle_slabs_does_not_alias_or_leave_unmapped_blocks() {
    use cuda_bridge::allocator::SlabBackend;
    use cuda_bridge::vmm::VmmSlab;

    struct MockVmmSubBackend {
        vmm: VmmPool<SharedMockVmmDriver>,
    }

    impl SlabBackend for MockVmmSubBackend {
        type Slab = VmmSlab;

        fn alloc_slab(&self, min_bytes: usize) -> Result<BackingSlab<Self::Slab>> {
            let slab = self.vmm.acquire_slab(min_bytes)?;
            Ok(BackingSlab {
                base_ptr: slab.base_ptr,
                size_bytes: slab.size_bytes,
                handle: slab,
            })
        }

        fn free_slab(&self, slab: BackingSlab<Self::Slab>) {
            let _ = self.vmm.unmap_chunks(slab.base_ptr, slab.size_bytes, true);
        }

        fn defragment_for(&self, min_bytes: usize) -> Result<Option<BackingSlab<Self::Slab>>> {
            let chunk = self.vmm.chunk_bytes().max(1);
            let min_chunks = min_bytes.div_ceil(chunk).max(1);
            Ok(self
                .vmm
                .defragment_or_remap(min_chunks)?
                .map(|slab| BackingSlab {
                    base_ptr: slab.base_ptr,
                    size_bytes: slab.size_bytes,
                    handle: slab,
                }))
        }

        fn supports_defragment(&self) -> bool {
            true
        }

        fn park_idle_slab(&self, slab: BackingSlab<Self::Slab>) {
            let _ = self.vmm.unmap_chunks(slab.base_ptr, slab.size_bytes, false);
        }

        fn trim_parked(&self) {
            let _ = self.vmm.release_unused_chunks(true);
        }

        fn mark_slab_idle(&self, slab: &BackingSlab<Self::Slab>) {
            self.vmm.mark_chunks_idle(slab.base_ptr, slab.size_bytes);
        }
    }

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(0);

    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    let driver = Arc::new(MockVmmDriver::new());
    driver.state.lock().unwrap().fail_after_creates = Some(3);
    let backend = MockVmmSubBackend {
        vmm: VmmPool::new(SharedMockVmmDriver(driver.clone()), 4 * chunk),
    };
    let pool = SubAllocator::<MockVmmSubBackend, MockEvent>::new_arc(backend, chunk, true);

    // Allocate 3 separate 2 MiB slabs: S1, S2 (active middle), S3.
    let b1 = pool.alloc(chunk, None).expect("slab 1");
    let b2 = pool.alloc(chunk, None).expect("slab 2");
    let b3 = pool.alloc(chunk, None).expect("slab 3");

    // Free b1 and b3 so S1 and S3 become completely idle around active S2.
    pool.free_deferred(b1, None, None);
    pool.free_deferred(b3, None, None);

    // Request chunk + 4 KiB: physical create fails (limit 3 chunks), so defragment_for gathers S1+S3.
    let big = pool
        .alloc(chunk + 4096, None)
        .expect("defragment_for must combine S1 + S3 into >1 chunk");

    // Every allocated block must remain mapped in driver AND never alias old unmapped VAs.
    let b5 = pool.alloc(MIN_BLOCK_ALIGN, None).expect("post-defrag small alloc");
    assert_ne!(b5.ptr, b1.ptr, "stale unmapped S1 free block must not remain allocatable");
    {
        let st = driver.state.lock().unwrap();
        let is_mapped = |ptr: u64| {
            st.mapped
                .iter()
                .any(|(&va, &(sz, _, _))| ptr >= va && ptr < va + sz as u64)
        };
        assert!(is_mapped(b2.ptr), "active S2 must remain mapped");
        assert!(is_mapped(big.ptr), "remapped 4 MiB slab must be mapped");
        assert!(is_mapped(b5.ptr), "subsequent alloc must land on mapped VA");
    }

    pool.free_deferred(b5, None, None);
    pool.free_deferred(big, None, None);
    pool.free_deferred(b2, None, None);
    assert_eq!(pool.trim_idle_slabs(), 2);
}

#[test]
fn graph_vmm_arena_tracks_global_vram_usage_and_enforces_reservation_limit() {
    use cuda_bridge::vmm::GraphArenaHandle;

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;

    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    set_vram_reservation_limit((2 * chunk) as u64);

    let pool = VmmPool::new(MockVmmDriver::new(), DEFAULT_VMM_RESERVE_BYTES);
    let arena = pool
        .reserve_graph_arena(3 * chunk)
        .expect("reserve 6 MiB graph VA");

    let base_used = vram_usage_bytes();
    let _p1 = arena.alloc(1024).expect("chunk 0 fits inside 4 MiB cap");
    assert_eq!(
        vram_usage_bytes(),
        base_used + chunk as u64,
        "graph arena physical mapping must update vram_usage_bytes"
    );

    let _p2 = arena.alloc(chunk).expect("chunk 1 fits inside 4 MiB cap");
    assert_eq!(vram_usage_bytes(), base_used + (2 * chunk) as u64);

    let oom = arena.alloc(chunk);
    assert!(
        oom.is_err(),
        "3rd chunk in graph arena must enforce vram_reservation_limit_bytes"
    );

    arena
        .release_physical_keeping_va()
        .expect("release graph physical chunks");
    assert_eq!(vram_usage_bytes(), base_used);
}

#[test]
fn per_vm_soft_watermark_gc_hook_fires_independently() {
    use cuda_bridge::allocator::{
        clear_vm_vram_quotas, register_vm_vram_soft_watermark_gc_hook,
        set_vm_device_vram_limit, set_vram_soft_watermark_percent,
    };

    static VM_SOFT_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn vm_soft_gc() -> bool {
        VM_SOFT_CALLS.fetch_add(1, Ordering::Relaxed);
        true
    }

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(0);
    set_vram_soft_watermark_percent(80);
    VM_SOFT_CALLS.store(0, Ordering::Relaxed);

    let vm_id = 77_001u64;
    set_vm_device_vram_limit(vm_id, 0, 100 * 1024);
    register_vm_vram_soft_watermark_gc_hook(vm_id, vm_soft_gc);

    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc_with_quota(
        MockSlabBackend::new(0),
        64 * 1024,
        true,
        vm_id,
        0,
    );
    let lease = pool.alloc(84 * 1024, None).expect("84 KiB below 100 KiB limit");

    let deadline = Instant::now() + Duration::from_secs(2);
    while VM_SOFT_CALLS.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        VM_SOFT_CALLS.load(Ordering::Relaxed) >= 1,
        "per-VM soft watermark GC hook must fire when VM quota crosses soft watermark"
    );

    pool.free_deferred(lease, None, None);
    clear_vm_vram_quotas(vm_id);
}

#[test]
fn tier2_reclaims_deferred_event_and_avoids_double_counting_census() {
    use cratonvm_types::gpu_event_census;
    use cuda_bridge::allocator::register_vm_vram_pressure_gc_hook;

    static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn hook_that_updates_census() -> bool {
        HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
        gpu_event_census::note_gc_triggered_by_vram_pressure();
        true
    }

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(4096);

    HOOK_CALLS.store(0, Ordering::Relaxed);
    let vm_id = 77_002u64;
    register_vm_vram_pressure_gc_hook(vm_id, hook_that_updates_census);

    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc_with_quota(
        MockSlabBackend::new(4096),
        4096,
        true,
        vm_id,
        0,
    );

    // Check 1: completed event passed to free_deferred is reclaimed non-blockingly on reclaim/alloc.
    let done_ev = MockEvent::new(true);
    let hooks_before_c1 = HOOK_CALLS.load(Ordering::Relaxed);
    let quick = pool.alloc(4096, Some(1)).expect("quick alloc");
    pool.free_deferred(quick, Some(1), Some(done_ev.clone()));
    let quick_cross = pool.alloc(4096, Some(2)).expect("completed event must reclaim non-blockingly");
    assert_eq!(done_ev.sync_count(), 0);
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), hooks_before_c1);
    pool.free_deferred(quick_cross, None, None);
    assert_eq!(vram_usage_bytes(), 0);

    // Check 2: when a concurrent thread retires a pending event onto another stream/event that completes
    // right as Tier-2 runs without calling notify_vram_freed(), Tier-2 must not miss completion,
    // and hook + Tier-2 must NOT double-count gpu_event_census::gc_triggered_by_vram_pressure().
    let a = pool.alloc(2048, Some(1)).expect("a");
    let b = pool.alloc(2048, Some(1)).expect("b");
    let ev = MockEvent::new(false);
    let ev_bg = ev.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(8));
        ev_bg.complete(); // No notify_vram_freed() call!
    });

    // Drop `b` after spawning completion: Tier-1 runs while `ev` isn't complete yet? Wait:
    // keep a hook that triggers GC cleanly when already at limit to test census single-count.
    pool.free_deferred(b, Some(1), Some(ev));
    let _ = pool.alloc(2048, Some(2)).expect("must succeed without OOM");

    // Force an actual Tier-2 hook invocation where allocation stays OOM.
    let census_before = gpu_event_census::gc_triggered_by_vram_pressure();
    let hooks_before = HOOK_CALLS.load(Ordering::Relaxed);
    let _ = pool.alloc(4096, Some(2));
    let hooks_delta = HOOK_CALLS.load(Ordering::Relaxed) - hooks_before;
    let census_delta = gpu_event_census::gc_triggered_by_vram_pressure() - census_before;
    assert_eq!(
        census_delta, hooks_delta as u64,
        "SubAllocator must not double-count gpu_event_census when the GC hook already notes it"
    );

    pool.free_deferred(a, None, None);
    let _ = pool.reclaim_completed();
}

#[test]
fn stream_ordered_reuse_not_double_counted_and_unpooled_respects_concurrent_limit() {
    use cratonvm_types::gpu_event_census::{
        alloc_totals, stream_ordered_reuses, vram_pool_hit_rate,
    };

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(0);

    // 1. Stream-ordered reuse must increment stream_ordered_reuses(), NOT alloc_hit.
    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(0),
        4096,
        true,
    );
    let ev = MockEvent::new(false);
    let b = pool.alloc(2048, Some(9)).expect("init alloc");
    pool.free_deferred(b, Some(9), Some(ev.clone()));

    let (hits_before, _, _) = alloc_totals();
    let reuses_before = stream_ordered_reuses();
    let b2 = pool.alloc(2048, Some(9)).expect("same-stream reuse");
    let (hits_after, _, _) = alloc_totals();
    let reuses_after = stream_ordered_reuses();
    assert_eq!(reuses_after, reuses_before + 1);
    assert_eq!(
        hits_after, hits_before,
        "same-stream reuse must not double-count into alloc_totals().0"
    );
    assert!(vram_pool_hit_rate() <= 1.0);
    pool.free_deferred(b2, None, None);
    ev.complete();
    let _ = pool.reclaim_completed();

    // 2. Concurrent unpooled allocs must never overshoot vram_reservation_limit_bytes().
    set_vram_reservation_limit(4096);
    let unpooled = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(0),
        4096,
        false,
    );
    let peak_seen = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let p = Arc::clone(&unpooled);
        let peak = Arc::clone(&peak_seen);
        handles.push(std::thread::spawn(move || {
            if let Ok(l) = p.alloc(2048, None) {
                peak.fetch_max(vram_usage_bytes(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
                p.free_deferred(l, None, None);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert!(
        peak_seen.load(Ordering::Relaxed) <= 4096,
        "concurrent unpooled allocs overshot 4096B limit: {}",
        peak_seen.load(Ordering::Relaxed)
    );
}

#[test]
fn per_stream_vmm_defrag_watermark_and_census_histograms() {
    use cratonvm_types::gpu_event_census;
    use cuda_bridge::critical::GpuTelemetrySnapshot;

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(0);

    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    let driver = Arc::new(MockVmmDriver::new());
    let pool = VmmPool::new(SharedMockVmmDriver(driver.clone()), 8 * chunk);
    pool.set_stream_defrag_watermark(Some(11), 2);
    pool.set_stream_defrag_watermark(Some(22), 10);

    let before_census = gpu_event_census::vmm_chunk_counts();
    let before_watermarks = gpu_event_census::vmm_defrag_watermark_hits();

    // Stream 11 allocates 2 slabs (chunks 0..2), Stream 22 allocates 1 slab (chunk 2..3).
    let s1_a = pool.acquire_slab_for_stream(chunk, Some(11)).expect("s1_a");
    let s1_b = pool.acquire_slab_for_stream(chunk, Some(11)).expect("s1_b");
    let _s2 = pool.acquire_slab_for_stream(chunk, Some(22)).expect("s2");

    let st = pool.stats();
    assert_eq!(st.active_chunks, 3);
    assert_eq!(st.idle_mapped_chunks, 0);
    assert_eq!(st.parked_chunks, 0);
    assert!(st.unmapped_chunks >= 5);
    assert_eq!(gpu_event_census::vmm_chunk_counts().active, before_census.active + 3);

    // Mark stream 11's first chunk idle (below watermark=2 -> stays IdleMapped).
    assert!(!pool.mark_chunks_idle_for_stream(s1_a.base_ptr, chunk, Some(11)));
    assert_eq!(pool.stats().idle_mapped_chunks, 1);

    // Mark stream 11's second chunk idle -> hits watermark=2 -> parks stream 11's 2 idle chunks,
    // leaving stream 22's active chunk untouched.
    assert!(pool.mark_chunks_idle_for_stream(s1_b.base_ptr, chunk, Some(11)));
    let after_st = pool.stats();
    assert_eq!(after_st.active_chunks, 1);
    assert_eq!(after_st.idle_mapped_chunks, 0);
    assert_eq!(after_st.parked_chunks, 2);
    assert_eq!(after_st.defrag_watermark_hits, 1);
    assert_eq!(
        gpu_event_census::vmm_defrag_watermark_hits(),
        before_watermarks + 1
    );

    drop(pool);
    // Drop cleans up the pool's chunk contributions from census.
    assert_eq!(gpu_event_census::vmm_chunk_counts().active, before_census.active);

    // Verify Tier-1 and Tier-2 latency histograms record recovery attempts.
    let t1_before = gpu_event_census::tier1_latency_histogram().count;
    let t2_before = gpu_event_census::tier2_latency_histogram().count;
    let sub = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(4096),
        4096,
        true,
    );
    let held = sub.alloc(4096, Some(1)).expect("fill 4096");
    let _ = sub.alloc(4096, Some(1)); // forces Tier-1 -> Tier-2 -> OOM
    let t1_after = gpu_event_census::tier1_latency_histogram();
    let t2_after = gpu_event_census::tier2_latency_histogram();
    assert!(t1_after.count > t1_before);
    assert!(t2_after.count > t2_before);
    assert_eq!(
        t1_after.buckets.iter().sum::<u64>(),
        t1_after.count,
        "tier1 bucket sum must match total count"
    );
    assert_eq!(
        t2_after.buckets.iter().sum::<u64>(),
        t2_after.count,
        "tier2 bucket sum must match total count"
    );
    let snap = GpuTelemetrySnapshot::capture();
    assert_eq!(snap.tier1_latency_count, t1_after.count);
    assert_eq!(snap.tier2_latency_count, t2_after.count);
    sub.free_deferred(held, Some(1), None);
}

#[test]
fn vmm_prewarm_async_populates_parked_handles_and_avoids_burst_creates() {
    use cratonvm_types::gpu_event_census;

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    let driver = Arc::new(MockVmmDriver::new());
    let pool = VmmPool::new(SharedMockVmmDriver(driver.clone()), 8 * chunk);

    let census_before = gpu_event_census::vmm_prewarm_created();
    pool.prewarm_async(3);
    pool.wait_for_prewarm();

    let st = pool.stats();
    assert_eq!(st.parked_chunks, 3);
    assert_eq!(st.prewarmed_chunks, 3);
    assert!(gpu_event_census::vmm_prewarm_created() >= census_before + 3);

    // First kernel burst across two streams consumes the pre-warmed handles with zero fresh creates.
    let handles_before_burst = driver.state.lock().unwrap().next_handle;
    let s1 = pool
        .acquire_slab_for_stream(chunk, Some(101))
        .expect("stream 101 slab");
    let s2 = pool
        .acquire_slab_for_stream(2 * chunk, Some(202))
        .expect("stream 202 slab");
    assert_eq!(
        driver.state.lock().unwrap().next_handle,
        handles_before_burst,
        "burst slabs must consume pre-warmed handles without calling create_physical"
    );
    assert_eq!(pool.stats().parked_chunks, 0);
    assert_eq!(pool.stats().active_chunks, 3);
    let _ = (s1, s2);

    drop(pool);
    assert!(
        driver.state.lock().unwrap().live_handles.is_empty(),
        "dropping VmmPool must release all mapped and parked physical handles"
    );
}

#[test]
fn vmm_drop_during_inflight_prewarm_releases_all_handles_cleanly() {
    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let chunk = DEFAULT_VMM_CHUNK_BYTES;
    let driver = Arc::new(MockVmmDriver::new());
    let pool = VmmPool::new(SharedMockVmmDriver(driver.clone()), 8 * chunk);
    pool.prewarm_async(4);
    drop(pool);
    assert!(
        driver.state.lock().unwrap().live_handles.is_empty(),
        "dropping VmmPool while prewarm may still be running must join worker and leak zero handles"
    );
}

#[test]
fn managed_memory_spillover_requires_opt_in_respects_budget_and_recovers_on_free() {
    use cratonvm_types::gpu_event_census;
    use cuda_bridge::allocator::{AllocHint, SpillConfig};

    struct SpillMockBackend {
        vram_budget: usize,
        vram_used: AtomicUsize,
        managed_allocs: AtomicUsize,
        managed_frees: AtomicUsize,
        next_ptr: AtomicU64,
    }

    impl SlabBackend for SpillMockBackend {
        type Slab = bool; // true = managed

        fn alloc_slab(&self, min_bytes: usize) -> Result<BackingSlab<Self::Slab>> {
            let cur = self.vram_used.load(Ordering::Relaxed);
            if cur + min_bytes > self.vram_budget {
                return Err(DeviceError::Driver("physical VRAM exhausted".into()));
            }
            self.vram_used.fetch_add(min_bytes, Ordering::Relaxed);
            let base_ptr = self.next_ptr.fetch_add(min_bytes as u64, Ordering::Relaxed);
            Ok(BackingSlab {
                base_ptr,
                size_bytes: min_bytes,
                handle: false,
            })
        }

        fn free_slab(&self, slab: BackingSlab<Self::Slab>) {
            if slab.handle {
                self.managed_frees.fetch_add(1, Ordering::Relaxed);
            } else {
                self.vram_used.fetch_sub(slab.size_bytes, Ordering::Relaxed);
            }
        }

        fn alloc_managed_slab(
            &self,
            min_bytes: usize,
            read_mostly: bool,
        ) -> Result<BackingSlab<Self::Slab>> {
            assert!(read_mostly);
            self.managed_allocs.fetch_add(1, Ordering::Relaxed);
            let base_ptr = self
                .next_ptr
                .fetch_add(min_bytes as u64, Ordering::Relaxed)
                | 0x8000_0000_0000;
            Ok(BackingSlab {
                base_ptr,
                size_bytes: min_bytes,
                handle: true,
            })
        }
    }

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(0);

    let backend = SpillMockBackend {
        vram_budget: 4096,
        vram_used: AtomicUsize::new(0),
        managed_allocs: AtomicUsize::new(0),
        managed_frees: AtomicUsize::new(0),
        next_ptr: AtomicU64::new(0x1000_0000),
    };
    let pool = SubAllocator::<SpillMockBackend, MockEvent>::new_arc(backend, 4096, true);
    let held = pool.alloc(4096, None).expect("fill physical VRAM");

    // Disabled by default -> hard OOM even with ReadMostlyTensor hint.
    assert!(pool
        .alloc_with_hint(4096, None, AllocHint::ReadMostlyTensor)
        .is_err());

    // Opt in with 8 KiB spill budget, >= 2 KiB read-mostly tensors only.
    pool.set_spill_config(SpillConfig {
        enabled: true,
        budget_bytes: 8192,
        min_tensor_bytes: 2048,
        read_mostly_only: true,
    });
    assert!(pool.alloc_with_hint(4096, None, AllocHint::Default).is_err());
    assert!(pool
        .alloc_with_hint(1024, None, AllocHint::ReadMostlyTensor)
        .is_err());

    let (before_cnt, before_bytes) = gpu_event_census::managed_spill_totals();
    let s1 = pool
        .alloc_with_hint(4096, None, AllocHint::ReadMostlyTensor)
        .expect("4 KiB read-mostly tensor must spill into Managed Memory");
    assert_eq!(pool.spill_stats().used_bytes, 4096);
    assert_eq!(pool.spill_stats().alloc_count, 1);
    assert_eq!(pool.backend().managed_allocs.load(Ordering::Relaxed), 1);
    let (after_cnt, after_bytes) = gpu_event_census::managed_spill_totals();
    assert_eq!(after_cnt, before_cnt + 1);
    assert_eq!(after_bytes, before_bytes + 4096);

    // Exceeding remaining 4 KiB spill budget must still fail hard OOM.
    assert!(pool
        .alloc_with_hint(8192, None, AllocHint::ReadMostlyTensor)
        .is_err());

    // Freeing a spilled tensor returns its budget without polluting VRAM Free buckets.
    pool.free_deferred(s1, None, None);
    assert_eq!(pool.spill_stats().used_bytes, 0);
    assert_eq!(pool.backend().managed_frees.load(Ordering::Relaxed), 1);
    assert_eq!(pool.stats().free_bytes, 0);

    let s2 = pool
        .alloc_with_hint(8192, None, AllocHint::ReadMostlyTensor)
        .expect("full 8 KiB budget usable again after free");
    pool.free_deferred(s2, None, None);
    pool.free_deferred(held, None, None);
}

#[test]
fn multi_vm_suballocator_soft_hard_quotas_and_reports_isolate_tenants() {
    use cuda_bridge::allocator::{
        all_vm_gpu_memory_reports, clear_vm_vram_quotas, partition_vram_quotas,
        register_vm_vram_soft_watermark_gc_hook, unregister_vm_vram_pressure_gc_hooks,
        vm_gpu_memory_report,
    };
    use std::sync::atomic::AtomicUsize;

    static VM_A_SOFT_HITS: AtomicUsize = AtomicUsize::new(0);
    static VM_B_SOFT_HITS: AtomicUsize = AtomicUsize::new(0);
    fn soft_hook_a() -> bool {
        VM_A_SOFT_HITS.fetch_add(1, Ordering::Relaxed);
        false
    }
    fn soft_hook_b() -> bool {
        VM_B_SOFT_HITS.fetch_add(1, Ordering::Relaxed);
        false
    }

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    let vm_a = 7101u64;
    let vm_b = 7102u64;
    clear_vm_vram_quotas(vm_a);
    clear_vm_vram_quotas(vm_b);
    VM_A_SOFT_HITS.store(0, Ordering::Relaxed);
    VM_B_SOFT_HITS.store(0, Ordering::Relaxed);

    // Partition 16 KiB total hard / 8 KiB total soft -> 4 KiB soft, 8 KiB hard per VM.
    partition_vram_quotas(&[vm_a, vm_b], &[0], 8192, 16384);
    register_vm_vram_soft_watermark_gc_hook(vm_a, soft_hook_a);
    register_vm_vram_soft_watermark_gc_hook(vm_b, soft_hook_b);

    let pool_a = SubAllocator::<MockSlabBackend, MockEvent>::new_arc_with_quota(
        MockSlabBackend::new(65536),
        4096,
        true,
        vm_a,
        0,
    );
    let pool_b = SubAllocator::<MockSlabBackend, MockEvent>::new_arc_with_quota(
        MockSlabBackend::new(65536),
        4096,
        true,
        vm_b,
        0,
    );

    let a1 = pool_a.alloc(4096, None).expect("VM A 4 KiB fits");
    for _ in 0..50 {
        if VM_A_SOFT_HITS.load(Ordering::Relaxed) >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(VM_A_SOFT_HITS.load(Ordering::Relaxed) >= 1);
    assert_eq!(VM_B_SOFT_HITS.load(Ordering::Relaxed), 0);

    let a2 = pool_a.alloc(4096, None).expect("VM A hits 8 KiB hard quota");
    assert!(
        pool_a.alloc(4096, None).is_err(),
        "VM A must fail OOM past its 8 KiB hard quota"
    );

    // VM B still has its full 8 KiB quota isolated from VM A's exhaustion.
    let b1 = pool_b.alloc(4096, None).expect("VM B unaffected by VM A");

    let rep_a = vm_gpu_memory_report(vm_a, 0);
    assert_eq!(rep_a.vram_used_bytes, 8192);
    assert_eq!(rep_a.vram_peak_bytes, 8192);
    assert_eq!(rep_a.vram_soft_limit_bytes, 4096);
    assert_eq!(rep_a.vram_hard_limit_bytes, 8192);
    assert!(rep_a.soft_watermark_triggers >= 1);
    assert_eq!(rep_a.oom_failures, 1);
    assert!(rep_a.alloc_count >= 2);
    assert!(all_vm_gpu_memory_reports().iter().any(|r| r.vm_identity == vm_b));

    pool_a.free_deferred(a1, None, None);
    pool_a.free_deferred(a2, None, None);
    pool_b.free_deferred(b1, None, None);
    drop(pool_a);
    drop(pool_b);
    unregister_vm_vram_pressure_gc_hooks(vm_a);
    unregister_vm_vram_pressure_gc_hooks(vm_b);
    clear_vm_vram_quotas(vm_a);
    clear_vm_vram_quotas(vm_b);
}

#[test]
fn reclaim_keeps_incomplete_siblings_and_extra_fences_and_drop_preserves_other_pool_spill() {
    use cuda_bridge::allocator::{
        clear_vm_vram_quotas, partition_vram_quotas, vm_gpu_memory_report, AllocHint, SpillConfig,
    };

    struct LocalSpillBackend {
        managed_allocs: AtomicU64,
        managed_frees: AtomicU64,
        next_ptr: AtomicU64,
    }
    impl SlabBackend for LocalSpillBackend {
        type Slab = bool;
        fn alloc_slab(&self, _min_bytes: usize) -> Result<BackingSlab<Self::Slab>> {
            Err(DeviceError::Driver("vram exhausted".into()))
        }
        fn free_slab(&self, slab: BackingSlab<Self::Slab>) {
            if slab.handle {
                self.managed_frees.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn alloc_managed_slab(
            &self,
            min_bytes: usize,
            _read_mostly: bool,
        ) -> Result<BackingSlab<Self::Slab>> {
            self.managed_allocs.fetch_add(1, Ordering::Relaxed);
            let base_ptr = self
                .next_ptr
                .fetch_add(min_bytes as u64, Ordering::Relaxed)
                | 0x8000_0000_0000;
            Ok(BackingSlab {
                base_ptr,
                size_bytes: min_bytes,
                handle: true,
            })
        }
    }

    let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _reset = LimitResetGuard;
    set_vram_reservation_limit(0);

    // 1. Concurrent has_pending_erased + multiple incomplete extra fences.
    let pool = SubAllocator::<MockSlabBackend, MockEvent>::new_arc(
        MockSlabBackend::new(4096),
        4096,
        true,
    );
    let ev_a = MockEvent::new(true);
    let ev_b = MockEvent::new(false);

    let l1 = pool.alloc(1024, Some(1)).expect("l1");
    let l2 = pool.alloc(1024, Some(2)).expect("l2");
    pool.free_deferred(l1, Some(1), Some(ev_a));
    pool.free_deferred(l2, Some(2), Some(ev_b.clone()));

    // Only l1 is complete; l2 must remain visible in pending.
    assert_eq!(pool.reclaim_completed(), 1);
    assert_eq!(pool.stats().pending_count, 1);

    ev_b.complete();
    assert_eq!(pool.reclaim_completed(), 1);
    assert_eq!(pool.stats().pending_count, 0);
    drop(pool);

    // 2. Dropping a zero-spill pool for a VM must preserve a sibling pool's spill_used.
    let vm = 7878u64;
    clear_vm_vram_quotas(vm);
    partition_vram_quotas(&[vm], &[0], 0, 0);
    let spill_pool = SubAllocator::<LocalSpillBackend, MockEvent>::new_arc_with_quota(
        LocalSpillBackend {
            managed_allocs: AtomicU64::new(0),
            managed_frees: AtomicU64::new(0),
            next_ptr: AtomicU64::new(0x1000_0000),
        },
        4096,
        true,
        vm,
        0,
    );
    spill_pool.set_spill_config(SpillConfig {
        enabled: true,
        budget_bytes: 8192,
        min_tensor_bytes: 1024,
        read_mostly_only: true,
    });
    let spilled = spill_pool
        .alloc_with_hint(2048, None, AllocHint::ReadMostlyTensor)
        .expect("spill alloc");
    assert_eq!(vm_gpu_memory_report(vm, 0).spill_used_bytes, 2048);

    let sibling = SubAllocator::<MockSlabBackend, MockEvent>::new_arc_with_quota(
        MockSlabBackend::new(4096),
        4096,
        true,
        vm,
        0,
    );
    drop(sibling);
    assert_eq!(
        vm_gpu_memory_report(vm, 0).spill_used_bytes,
        2048,
        "dropping a sibling pool must not wipe another pool's spill_used_bytes"
    );

    spill_pool.free_deferred(spilled, None, None);
    drop(spill_pool);
    assert_eq!(vm_gpu_memory_report(vm, 0).spill_used_bytes, 0);
    clear_vm_vram_quotas(vm);
}

// ---------------------------------------------------------------------------
// Hardware CUDA tests (auto-skip cleanly if no CUDA device is present)
// ---------------------------------------------------------------------------

#[cfg(feature = "cuda")]
mod cuda_hw {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::{cuda_bridge, LimitResetGuard, GLOBAL_COORD_LOCK};
    use cuda_bridge::allocator::{
        new_cuda_suballocator_for_context, notify_vram_freed, reclaim_all_completed_pools,
        register_vram_pressure_gc_hook, set_vram_reservation_limit, vram_usage_bytes, AllocLease,
        CudaSubAllocator,
    };
    use cuda_bridge::vmm::{
        probe_vmm_support_for_context, CudaVmmDriver, VmmPool, DEFAULT_VMM_CHUNK_BYTES,
        DEFAULT_VMM_RESERVE_BYTES,
    };
    use cuda_bridge::{DeviceBuffer, DeviceContext, Event, Stream};
    use std::sync::Arc;

    #[test]
    fn cuda_suballocator_stream_ordered_and_vmm_roundtrip() {
        let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let stream = Stream::new(&ctx).expect("stream");

        // Exercise >2 MiB allocation (hits VMM path when supported, or cuMemAlloc fallback).
        let big = DeviceBuffer::<u32>::zeros(&ctx, 1024 * 1024).expect("4 MiB zeros");
        let mut host = vec![0u32; 1024 * 1024];
        big.to_host(&mut host).expect("to_host");
        assert!(host.iter().all(|&x| x == 0));
        drop(big);

        let data: Vec<f32> = (0..4096).map(|i| i as f32).collect();
        // SAFETY: `data` stays alive and unmodified on the stack until `stream.synchronize()` below.
        let d1 = unsafe { DeviceBuffer::from_host_async_unchecked(&ctx, &data, &stream) }
            .expect("async upload 1");
        let _stream_id = stream.stream_id();
        drop(d1);

        // SAFETY: `data` outlives `stream.synchronize()` below.
        let d2 = unsafe { DeviceBuffer::from_host_async_unchecked(&ctx, &data, &stream) }
            .expect("async upload 2 on same stream");
        stream.synchronize().expect("sync");
        let mut out = vec![0.0f32; 4096];
        d2.to_host(&mut out).expect("readback");
        assert_eq!(out, data);
    }

    #[test]
    fn cuda_suballocator_same_stream_reuse_pending_free_reclaim_and_tier2_gc() {
        let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let stream = Stream::new(&ctx).expect("stream");
        let sid = stream.stream_id();

        let pool = new_cuda_suballocator_for_context(&ctx, true);

        // 1. Same-stream zero-wait reuse + PendingFree reclaim with real CUDA Event.
        let ev = Arc::new(Event::new(&ctx).expect("event"));
        let a = pool.alloc(2048, Some(sid)).expect("alloc a");
        let ptr_a = a.ptr;
        stream.record_event(&ev).expect("record event");
        pool.free_deferred(a, Some(sid), Some(ev.clone()));
        assert!(pool.stats().pending_count >= 1);

        // Same-stream allocation must reuse `ptr_a` immediately without blocking.
        let reused = pool.alloc(1024, Some(sid)).expect("same-stream reuse");
        assert_eq!(reused.ptr, ptr_a);
        pool.free_deferred(reused, Some(sid), Some(ev.clone()));

        stream.synchronize().expect("sync stream");
        ev.synchronize().expect("sync event");
        let reclaimed = reclaim_all_completed_pools();
        assert!(reclaimed >= 1);
        assert_eq!(pool.stats().pending_count, 0);

        // 2. Tier-2 OOM GC pressure wakeup under a hard VRAM reservation cap.
        let _reset = LimitResetGuard;
        let base_used = vram_usage_bytes();
        set_vram_reservation_limit(base_used + 4096);

        static HELD_CUDA_GC: Mutex<Option<(Arc<CudaSubAllocator>, AllocLease)>> = Mutex::new(None);
        fn cuda_gc_hook() -> bool {
            let taken = HELD_CUDA_GC.lock().unwrap_or_else(|p| p.into_inner()).take();
            if let Some((p, lease)) = taken {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(8));
                    p.free_deferred(lease, None, None);
                    notify_vram_freed();
                });
                true
            } else {
                false
            }
        }
        register_vram_pressure_gc_hook(cuda_gc_hook);

        let b1 = pool.alloc(2048, None).expect("b1");
        let b2 = pool.alloc(2048, None).expect("b2");
        *HELD_CUDA_GC.lock().unwrap_or_else(|p| p.into_inner()) = Some((pool.clone(), b2));

        let b3 = pool
            .alloc(2048, Some(sid))
            .expect("tier2 GC pressure wakeup should reclaim b2");
        pool.free_deferred(b1, None, None);
        pool.free_deferred(b3, None, None);
    }

    #[test]
    fn cuda_vmm_2mib_chunk_map_unmap_remap_roundtrip() {
        let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let support = probe_vmm_support_for_context(&ctx);
        if !support.supported {
            return;
        }
        assert!(support.chunk_bytes >= DEFAULT_VMM_CHUNK_BYTES);
        assert_eq!(support.chunk_bytes % DEFAULT_VMM_CHUNK_BYTES, 0);

        let drv = CudaVmmDriver::from_context(&ctx, support);
        let pool = VmmPool::new(drv, DEFAULT_VMM_RESERVE_BYTES);
        let chunk = pool.chunk_bytes();

        let s1 = pool.acquire_slab(chunk).expect("vmm slab 1");
        let s2 = pool.acquire_slab(chunk).expect("vmm slab 2");
        let s3 = pool.acquire_slab(chunk).expect("vmm slab 3");
        assert_eq!(pool.stats().mapped_physical_bytes, 3 * chunk);
        assert_eq!(pool.stats().parked_physical_bytes, 0);

        // Leave s2 active, mark s1 idle-mapped, and unmap+park s3.
        pool.mark_chunks_idle(s1.base_ptr, s1.size_bytes);
        let parked = pool.unmap_chunks(s3.base_ptr, s3.size_bytes, false);
        assert_eq!(parked, chunk);
        assert_eq!(pool.stats().parked_physical_bytes, chunk);

        // Remap idle + parked chunks contiguously without touching active s2.
        let remapped = pool
            .defragment_or_remap(2)
            .expect("defragment_or_remap")
            .expect("2 chunks available for remap");
        assert_eq!(remapped.chunk_count, 2);
        assert_eq!(pool.stats().remap_count, 1);
        assert_eq!(pool.stats().mapped_physical_bytes, 3 * chunk);
        assert_eq!(pool.stats().parked_physical_bytes, 0);

        // Mark remapped slab idle and release unused physical chunks.
        pool.mark_chunks_idle(remapped.base_ptr, remapped.size_bytes);
        let released = pool.release_unused_chunks(false);
        assert_eq!(released, 2 * chunk);
        assert_eq!(pool.stats().mapped_physical_bytes, chunk);
    }

    #[test]
    fn cuda_context_boot_prewarms_vmm_physical_handles_async() {
        use cuda_bridge::vmm::{
            set_vmm_prewarm_min_chunks, DEFAULT_VMM_PREWARM_CHUNKS,
        };

        struct PrewarmReset;
        impl Drop for PrewarmReset {
            fn drop(&mut self) {
                set_vmm_prewarm_min_chunks(DEFAULT_VMM_PREWARM_CHUNKS);
            }
        }

        let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _reset_lim = LimitResetGuard;
        set_vram_reservation_limit(0);
        let _reset_pw = PrewarmReset;
        set_vmm_prewarm_min_chunks(2);

        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let pool = new_cuda_suballocator_for_context(&ctx, true);
        let Some(vmm) = pool.backend().vmm_pool() else {
            return;
        };
        vmm.wait_for_prewarm();
        let st = vmm.stats();
        assert!(st.prewarmed_chunks >= 2);
        assert!(st.parked_chunks >= 2);
    }

    #[test]
    fn cuda_managed_spillover_roundtrip() {
        use cuda_bridge::allocator::SpillConfig;

        let _g = GLOBAL_COORD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _reset = LimitResetGuard;
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };

        let base_used = vram_usage_bytes();
        set_vram_reservation_limit(base_used + 4096);
        let _held = DeviceBuffer::<f32>::zeros(&ctx, 1024).expect("occupy 4 KiB VRAM quota");

        let host: Vec<f32> = (0..65_536).map(|i| (i as f32) * 0.5).collect();
        assert!(
            DeviceBuffer::<f32>::from_host(&ctx, &host).is_err(),
            "normal allocation must fail when spill is disabled and VRAM cap is exhausted"
        );

        ctx.set_spill_config(SpillConfig {
            enabled: true,
            budget_bytes: 8 * 1024 * 1024,
            min_tensor_bytes: 64 * 1024,
            read_mostly_only: true,
        });

        let spilled =
            DeviceBuffer::<f32>::from_host_read_mostly(&ctx, &host).expect("managed spill H2D");
        assert!(ctx.spill_stats().alloc_count >= 1);
        assert!(ctx.spill_stats().used_bytes >= 65_536 * 4);

        let mut out = vec![0.0f32; host.len()];
        spilled.to_host(&mut out).expect("managed spill D2H");
        assert_eq!(out, host);

        drop(spilled);
        ctx.synchronize().expect("sync after spill drop");
        assert_eq!(ctx.spill_stats().used_bytes, 0);
    }
}
