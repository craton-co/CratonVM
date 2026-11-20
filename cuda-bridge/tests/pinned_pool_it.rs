// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration tests for `cuda-bridge::pinned_pool`, `cratonvm_types::gpu_event_census`,
//! and `cuda-bridge::critical` GPU telemetry reporting.
//!
//! All tests run without requiring a GPU driver and compile both with and
//! without `--features cuda`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use cratonvm_cuda_bridge as cuda_bridge;
use cratonvm_types::gpu_event_census;
use cuda_bridge::critical::{
    CriticalMetricsRaw, CriticalSectionReport, GpuTelemetrySnapshot, Registry,
};
use cuda_bridge::pinned_pool::{HostRegionPinCache, PinStatus, PinnedHostPool};

#[test]
fn pinned_pool_reuses_same_stream_and_larger_size_classes() {
    let pool = PinnedHostPool::with_cap(64 * 1024);
    let (hits0, misses0) = gpu_event_census::pinned_pool_totals();

    let mut lease1 = pool.acquire(1000, 7).expect("acquire 1000");
    assert_eq!(lease1.len(), 1000);
    assert!(lease1.capacity_bytes() >= 4096);
    assert_eq!(lease1.stream_id(), 7);
    let slice = lease1.as_typed_mut_slice::<u32>().expect("u32 view");
    slice[0] = 0x1234_5678;
    pool.release(lease1);

    let lease2 = pool.acquire(512, 7).expect("reuse from 4096 class");
    assert_eq!(lease2.len(), 512);
    assert!(lease2.capacity_bytes() >= 4096);
    pool.release(lease2);

    let stats = pool.stats();
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.ready_buffers, 1);

    let (hits1, misses1) = gpu_event_census::pinned_pool_totals();
    assert!(hits1 - hits0 >= 1);
    assert!(misses1 - misses0 >= 1);
}

#[test]
fn pinned_pool_prefers_same_stream_affinity_on_reuse() {
    let pool = PinnedHostPool::with_cap(128 * 1024);
    let mut a = pool.acquire(4096, 1).expect("stream 1");
    let mut b = pool.acquire(4096, 2).expect("stream 2");
    a.as_mut_slice()[0] = 0xAA;
    b.as_mut_slice()[0] = 0xBB;
    pool.release(a);
    pool.release(b);

    let re_a = pool.acquire(4096, 1).expect("reacquire stream 1");
    assert_eq!(re_a.as_slice()[0], 0xAA);
    pool.release(re_a);
}

#[test]
fn pinned_pool_deferred_release_waits_for_event_completion() {
    let pool = PinnedHostPool::with_cap(64 * 1024);
    let lease = pool.acquire(4096, 11).expect("acquire");
    let completed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&completed);

    pool.release_deferred(lease, 11, move || flag.load(Ordering::Acquire));

    assert_eq!(pool.reclaim_completed(), 0);
    assert_eq!(pool.stats().deferred_buffers, 1);
    assert_eq!(pool.stats().ready_buffers, 0);

    // While deferred is still in flight, a second acquire misses.
    let other = pool.acquire(4096, 11).expect("acquire while deferred");
    pool.release(other);
    assert_eq!(pool.stats().misses, 2);

    completed.store(true, Ordering::Release);
    assert_eq!(pool.reclaim_completed(), 1);
    assert_eq!(pool.stats().deferred_buffers, 0);
    assert_eq!(pool.stats().ready_buffers, 2);
}

#[test]
fn pinned_pool_evicts_oldest_ready_buffers_over_cap() {
    // Cap allows two 4096-byte buffers (8192 total), third must evict oldest.
    let pool = PinnedHostPool::with_cap(8192);
    let l1 = pool.acquire(4096, 1).expect("l1");
    let l2 = pool.acquire(4096, 1).expect("l2");
    let l3 = pool.acquire(4096, 1).expect("l3");
    pool.release(l1);
    pool.release(l2);
    pool.release(l3);

    let stats = pool.stats();
    assert!(stats.ready_bytes <= 8192);
    assert_eq!(stats.ready_buffers, 2);
    assert_eq!(stats.evictions, 1);

    pool.clear_ready();
    assert_eq!(pool.stats().ready_buffers, 0);
    assert_eq!(pool.stats().ready_bytes, 0);
}

#[test]
fn host_pin_cache_dedups_refcounts_and_unregisters_cleanly() {
    let cache = HostRegionPinCache::new();
    let backing = vec![0u8; 4096];
    let base = backing.as_ptr() as usize;
    let len = backing.len();

    let (hits0, regs0) = gpu_event_census::host_pin_cache_totals();

    let g1 = cache.pin_span(base, len).expect("first pin");
    assert_eq!(cache.cached_span_count(), 1);
    assert_eq!(cache.span_refcount(base, len), 1);
    assert!(cache.is_span_cached(base + 128, 256));

    // Exact span hit + enclosing sub-span hit both bump refcount of `(base, len)`.
    let g2 = cache.pin_span(base, len).expect("exact reuse");
    let g3 = cache
        .pin_span(base + 64, 512)
        .expect("enclosing span reuse");
    assert_eq!(cache.cached_span_count(), 1);
    assert_eq!(cache.span_refcount(base, len), 3);
    assert_eq!(g1.span(), (base, len));
    assert_eq!(g2.span(), (base, len));
    assert_eq!(g3.span(), (base, len));

    #[cfg(not(feature = "cuda"))]
    assert_eq!(g1.status(), PinStatus::UnpinnedFallback);
    #[cfg(feature = "cuda")]
    assert!(matches!(
        g1.status(),
        PinStatus::Pinned | PinStatus::UnpinnedFallback
    ));

    // Unregister refuses with Err while active guards exist.
    assert!(cache.unregister_span(base, len).is_err());

    drop(g3);
    drop(g2);
    assert_eq!(cache.span_refcount(base, len), 1);
    let g1_pinned = g1.is_pinned();
    drop(g1);
    assert_eq!(cache.span_refcount(base, len), 0);

    assert!(cache.unregister_span(base, len).expect("unregister at 0"));
    assert_eq!(cache.cached_span_count(), 0);
    assert!(!cache.is_span_cached(base, len));

    cache.clear();
    assert_eq!(cache.cached_span_count(), 0);

    let (hits1, regs1) = gpu_event_census::host_pin_cache_totals();
    assert!(hits1 - hits0 >= 2);
    if g1_pinned {
        assert!(regs1 - regs0 >= 1);
    }
}

#[test]
fn host_pin_cache_rejects_zero_length_or_overflow_spans() {
    let cache = HostRegionPinCache::new();
    assert!(cache.pin_span(0x1000, 0).is_err());
    assert!(cache.pin_span(usize::MAX - 4, 16).is_err());
    assert!(!cache.is_span_pinned(0x1000, 0));
    assert!(!cache.is_span_pinned(usize::MAX - 4, 16));
}

#[test]
fn gpu_event_census_tracks_peak_vram_and_bounded_hit_rate() {
    let (_, peak0) = gpu_event_census::vram_used_and_peak();
    let high = peak0.saturating_add(16 * 1024);
    let low = peak0.saturating_add(4 * 1024);

    gpu_event_census::set_vram_used_bytes(high);
    gpu_event_census::set_vram_used_bytes(low);

    let (used, peak) = gpu_event_census::vram_used_and_peak();
    assert_eq!(used, low);
    assert!(peak >= high);

    let rate = gpu_event_census::vram_pool_hit_rate();
    assert!(rate.is_finite());
    assert!((0.0..=1.0).contains(&rate));
}

#[test]
fn critical_report_renders_vram_and_pinned_telemetry() {
    let gpu = GpuTelemetrySnapshot {
        vram_used_bytes: 1_048_576,
        vram_peak_bytes: 2_097_152,
        vram_pool_hit_rate: 0.75,
        alloc_pool_hits: 30,
        alloc_pool_misses: 10,
        alloc_pool_parked: 4,
        stream_ordered_reuses: 12,
        gc_triggered_by_vram_pressure: 2,
        vmm_remaps: 5,
        pinned_pool_hits: 18,
        pinned_pool_misses: 3,
        host_pin_cache_hits: 9,
        host_pin_cache_registers: 1,
        ..Default::default()
    };
    let report = CriticalSectionReport::from_raw_with_gpu(CriticalMetricsRaw::default(), gpu);
    let text = report.to_string();
    assert!(
        text.contains("[GPU] vram: used_bytes=1048576 peak_bytes=2097152 pool_hit_rate=0.7500"),
        "{text}"
    );
    assert!(
        text.contains("stream_reuses=12 gc_vram_triggers=2 vmm_remaps=5"),
        "{text}"
    );
    assert!(
        text.contains("[GPU] pinned: pool_hits=18 pool_misses=3 pin_cache_hits=9 pin_cache_registers=1"),
        "{text}"
    );

    // Also ensure Registry::metrics_report captures census without panicking.
    let reg = Registry::new();
    let live = reg.metrics_report();
    assert!(live.gpu.vram_pool_hit_rate.is_finite());
}

#[test]
fn host_pin_cache_clear_preserves_active_guards() {
    let cache = HostRegionPinCache::new();
    let backing = vec![0u8; 4096];
    let base = backing.as_ptr() as usize;
    let guard = cache.pin_span(base, backing.len()).expect("pin");
    cache.clear();
    assert_eq!(cache.cached_span_count(), 1, "clear() must not drop active spans");
    drop(guard);
    cache.clear();
    assert_eq!(cache.cached_span_count(), 0, "clear() must drop idle spans");
}

#[test]
fn pinned_pool_drop_with_deferred_entries_drains_cleanly() {
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let polls_clone = Arc::clone(&polls);
    {
        let pool = PinnedHostPool::with_cap(64 * 1024);
        let lease = pool.acquire(1024, 1).expect("acquire");
        pool.release_deferred(lease, 1, move || {
            let n = polls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            n >= 1
        });
    }
    assert!(polls.load(std::sync::atomic::Ordering::Relaxed) >= 1);
}

#[test]
fn host_pin_cache_concurrent_pin_span_is_race_free() {
    let cache = HostRegionPinCache::new();
    let backing = vec![0u8; 8192];
    let base = backing.as_ptr() as usize;
    let len = backing.len();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let c = cache.clone();
            std::thread::spawn(move || {
                let g = c.pin_span(base, len).expect("concurrent pin_span");
                assert_eq!(g.span(), (base, len));
                g
            })
        })
        .collect();
    let guards: Vec<_> = handles.into_iter().map(|h| h.join().expect("join")).collect();
    assert_eq!(cache.cached_span_count(), 1);
    drop(guards);
    assert!(cache.unregister_span(base, len).expect("unregister"));
}

#[test]
fn host_pin_cache_vm_identity_sandboxing_and_max_spans() {
    use cuda_bridge::pinned_pool::{
        clear_vm_host_pin_quota, set_vm_host_pin_quota, vm_host_pin_stats, HostPinQuota, PinStatus,
    };

    let vm_a = 9101u64;
    let vm_b = 9102u64;
    clear_vm_host_pin_quota(vm_a);
    clear_vm_host_pin_quota(vm_b);
    set_vm_host_pin_quota(
        vm_a,
        HostPinQuota {
            soft_pinned_bytes: 0,
            hard_pinned_bytes: 0,
            max_spans: 2,
        },
    );
    set_vm_host_pin_quota(
        vm_b,
        HostPinQuota {
            soft_pinned_bytes: 0,
            hard_pinned_bytes: 0,
            max_spans: 4,
        },
    );

    let b1 = vec![0u8; 4096];
    let b2 = vec![0u8; 4096];
    let b3 = vec![0u8; 4096];
    let cache_a = HostRegionPinCache::new_for_vm(vm_a);
    let cache_b = HostRegionPinCache::new_for_vm(vm_b);

    let g1 = cache_a.pin_span(b1.as_ptr() as usize, 4096).unwrap();
    let g2 = cache_a.pin_span(b2.as_ptr() as usize, 4096).unwrap();
    assert_eq!(cache_a.stats().span_count, 2);

    // Both spans are active (refcount=1), so 3rd span on VM A is denied cleanly.
    let g3_denied = cache_a.pin_span(b3.as_ptr() as usize, 4096).unwrap();
    assert_eq!(g3_denied.status(), PinStatus::UnpinnedFallback);
    assert_eq!(cache_a.stats().quota_denials, 1);
    assert_eq!(cache_a.stats().span_count, 2);
    drop(g3_denied);

    // VM B is isolated and can still cache spans freely.
    let gb = cache_b.pin_span(b3.as_ptr() as usize, 4096).unwrap();
    assert_eq!(cache_b.stats().quota_denials, 0);
    assert_eq!(cache_b.stats().span_count, 1);
    drop(gb);

    // Release one active span on VM A so the next pin evicts the idle LRU entry.
    drop(g1);
    let _g3_evict = cache_a.pin_span(b3.as_ptr() as usize, 4096).unwrap();
    assert!(cache_a.stats().evictions >= 1);
    assert_eq!(cache_a.stats().span_count, 2);
    drop(g2);
    drop(_g3_evict);
    drop(cache_a);
    drop(cache_b);
    assert_eq!(vm_host_pin_stats(vm_a).span_count, 0);
    clear_vm_host_pin_quota(vm_a);
    clear_vm_host_pin_quota(vm_b);
}

#[cfg(feature = "cuda")]
mod cuda_hw {
    use std::sync::Arc;

    use super::cuda_bridge;
    use cuda_bridge::allocator::reclaim_all_completed_pools;
    use cuda_bridge::graph::CaptureMode;
    use cuda_bridge::pinned_pool::{HostRegionPinCache, PinStatus, PinnedHostPool};
    use cuda_bridge::{
        DeviceBuffer, DeviceContext, DeviceModule, Event, KernelArgs, LaunchConfig, Stream,
    };

    const COPY_ADD_PTX: &str = r#"
.version 7.0
.target sm_70
.address_size 64

.visible .entry copy_add(
    .param .u64 src_ptr,
    .param .u64 dst_ptr,
    .param .s32 n
)
{
    .reg .pred  %p<2>;
    .reg .s32   %r<7>;
    .reg .u64   %rd<7>;

    ld.param.u64 %rd1, [src_ptr];
    ld.param.u64 %rd2, [dst_ptr];
    ld.param.s32 %r1, [n];
    mov.u32 %r2, %ctaid.x;
    mov.u32 %r3, %ntid.x;
    mov.u32 %r4, %tid.x;
    mad.lo.s32 %r5, %r2, %r3, %r4;
    setp.ge.s32 %p1, %r5, %r1;
    @%p1 bra DONE;
    cvta.to.global.u64 %rd3, %rd1;
    cvta.to.global.u64 %rd4, %rd2;
    mul.wide.s32 %rd5, %r5, 4;
    add.u64 %rd6, %rd3, %rd5;
    ld.global.u32 %r6, [%rd6];
    add.s32 %r6, %r6, 1;
    add.u64 %rd6, %rd4, %rd5;
    st.global.u32 [%rd6], %r6;
DONE:
    ret;
}
"#;

    #[test]
    fn cuda_pinned_host_pool_cu_mem_alloc_host_reuse() {
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let stream = Stream::new(&ctx).expect("stream");
        let pool = PinnedHostPool::with_context(ctx.clone(), 64 * 1024);

        let mut l1 = pool
            .acquire(4096, stream.stream_id())
            .expect("acquire pinned");
        assert!(
            l1.is_pinned(),
            "with_context pool should back leases with cuMemAllocHost_v2"
        );
        l1.as_mut_slice()[..4].copy_from_slice(&[1, 2, 3, 4]);

        let ev = Arc::new(Event::new(&ctx).expect("event"));
        stream.record_event(&ev).expect("record");
        let ev_poll = Arc::clone(&ev);
        pool.release_deferred(l1, stream.stream_id(), move || {
            ev_poll.query().unwrap_or(false)
        });

        stream.synchronize().expect("sync");
        ev.synchronize().expect("event sync");
        let _ = pool.reclaim_completed();

        let l2 = pool
            .acquire(4096, stream.stream_id())
            .expect("re-acquire pinned");
        assert!(l2.is_pinned());
        assert_eq!(pool.stats().hits, 1);
        pool.release(l2);
    }

    #[test]
    fn cuda_host_region_pin_cache_registers_and_unregisters_heap_span() {
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        #[repr(C, align(4096))]
        #[derive(Clone, Copy)]
        struct Page([u8; 4096]);

        let backing = vec![Page([0u8; 4096]); 2].into_boxed_slice();
        let base = backing.as_ptr() as usize;
        let len = std::mem::size_of_val(&*backing);

        let cache = HostRegionPinCache::with_context(ctx);
        let (_, regs_before) = super::gpu_event_census::host_pin_cache_totals();
        let guard = cache.pin_span(base, len).expect("pin heap span");
        let (_, regs_after) = super::gpu_event_census::host_pin_cache_totals();
        assert_eq!(guard.span(), (base, len));
        assert_eq!(cache.span_refcount(base, len), 1);
        assert!(matches!(
            guard.status(),
            PinStatus::Pinned | PinStatus::UnpinnedFallback
        ));
        if guard.is_pinned() {
            assert!(regs_after > regs_before);
            assert!(cache.is_span_pinned(base + 128, 512));
        }
        assert!(
            cache.unregister_span(base, len).is_err(),
            "refcount > 0 must prevent unregister"
        );

        drop(guard);
        assert!(
            cache
                .unregister_span(base, len)
                .expect("unregister at refcount 0")
        );
        assert!(!cache.is_span_pinned(base, len));
        assert_eq!(cache.cached_span_count(), 0);
    }

    #[test]
    fn cuda_host_region_pin_cache_unpinned_super_span_does_not_shadow_pinned_subspan() {
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        #[repr(C, align(4096))]
        #[derive(Clone, Copy)]
        struct Page([u8; 4096]);

        let backing = vec![Page([0u8; 4096]); 2].into_boxed_slice();
        let base = backing.as_ptr() as usize;
        let cache = HostRegionPinCache::with_context(ctx);

        let g_small = cache.pin_span(base, 4096).expect("small pin");
        assert!(g_small.is_pinned());

        // Overlapping larger span cannot cuMemHostRegister because `(base, 4096)` is registered.
        let g_large = cache.pin_span(base, 8192).expect("large fallback");
        assert!(!g_large.is_pinned());

        // Lookup of `(base, 4096)` or a strict sub-span must still find `(base, 4096)`,
        // never shadowed by the unpinned `(base, 8192)` entry.
        let g_sub = cache.pin_span(base + 512, 1024).expect("sub-span lookup");
        assert!(
            g_sub.is_pinned(),
            "pinned sub-span must not be shadowed by unpinned super-span"
        );
        assert_eq!(g_sub.span(), (base, 4096));

        drop(g_sub);
        drop(g_large);
        drop(g_small);
        let _ = cache.unregister_span(base, 4096);
        let _ = cache.unregister_span(base, 8192);
    }

    #[test]
    fn cuda_async_d2h_read_fence_defers_buffer_drop_until_dma_completes() {
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let stream = Stream::new(&ctx).expect("stream");
        let module = DeviceModule::from_ptx(&ctx, COPY_ADD_PTX, &["copy_add"]).expect("ptx");

        let input: Vec<u32> = (1..=1024).collect();
        let src = DeviceBuffer::from_host(&ctx, &input).expect("src");
        let dst = DeviceBuffer::<u32>::zeros(&ctx, 1024).expect("dst");

        let cfg = LaunchConfig::elementwise(1024);
        let args = KernelArgs::new()
            .push_device_ptr(&src)
            .push_device_ptr(&dst)
            .push_i32(1024);
        module
            .launch_on_stream(&ctx, "copy_add", &cfg, args, &stream)
            .expect("launch");

        let mut host_full = vec![0u32; 1024];
        let mut host_range = vec![0u32; 256];
        // SAFETY: `host_full` stays live and untouched until `stream.synchronize()` below.
        unsafe {
            dst.to_host_async_unchecked(&mut host_full, &stream)
                .expect("d2h full");
        }
        // SAFETY: `host_range` stays live and untouched until `stream.synchronize()` below.
        unsafe {
            dst.to_host_async_range_unchecked(&mut host_range, 128, &stream)
                .expect("d2h range");
        }

        // Drop device buffers immediately before the async D2H finishes.
        drop(dst);
        drop(src);

        // Allocate/free a decoy on a second stream to check retired memory stays fenced.
        let stream2 = Stream::new(&ctx).expect("stream2");
        let decoy_host = vec![0xDEAD_BEEFu32; 1024];
        // SAFETY: `decoy_host` outlives `stream2.synchronize()` below.
        let decoy = unsafe { DeviceBuffer::from_host_async_unchecked(&ctx, &decoy_host, &stream2) }
            .expect("decoy");
        stream2.synchronize().expect("sync2");
        drop(decoy);

        stream.synchronize().expect("sync1");
        let _ = reclaim_all_completed_pools();

        let expected: Vec<u32> = input.iter().map(|&v| v + 1).collect();
        assert_eq!(host_full, expected);
        assert_eq!(host_range, expected[128..384]);
    }

    #[test]
    fn cuda_graph_exec_keepalive_preserves_dropped_device_buffer_across_replays() {
        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let stream = Stream::new(&ctx).expect("stream");
        let module = DeviceModule::from_ptx(&ctx, COPY_ADD_PTX, &["copy_add"]).expect("ptx");

        let src = DeviceBuffer::from_host(&ctx, &[41u32; 512]).expect("src");
        let dst = DeviceBuffer::<u32>::zeros(&ctx, 512).expect("dst");

        stream
            .begin_capture(CaptureMode::ThreadLocal)
            .expect("begin capture");
        let cfg = LaunchConfig::elementwise(512);
        let args = KernelArgs::new()
            .push_device_ptr(&src)
            .push_device_ptr(&dst)
            .push_i32(512);
        module
            .launch_on_stream(&ctx, "copy_add", &cfg, args, &stream)
            .expect("captured launch");
        let graph = stream.end_capture(&ctx).expect("end capture");
        let exec = graph.instantiate().expect("instantiate");
        assert!(
            exec.tracked_buffer_count() >= 2,
            "captured graph must track both src and dst keep-alives"
        );

        // Drop `src` before replay; `GraphExec` must retain its backing allocation.
        drop(src);
        let _ = reclaim_all_completed_pools();

        // Allocate and zero a same-sized decoy buffer to verify `src` was not recycled.
        let decoy = DeviceBuffer::<u32>::zeros(&ctx, 512).expect("decoy");
        drop(decoy);

        for _ in 0..3 {
            let done = exec.launch(&ctx, &stream).expect("replay");
            done.synchronize().expect("replay event");
        }
        stream.synchronize().expect("sync");

        let mut out = vec![0u32; 512];
        dst.to_host(&mut out).expect("to_host");
        assert_eq!(out, vec![42u32; 512]);
    }

    #[test]
    fn cuda_host_pin_cache_hard_byte_quota_sandboxing() {
        use cuda_bridge::pinned_pool::{clear_vm_host_pin_quota, set_vm_host_pin_quota, HostPinQuota};

        let Ok(ctx) = DeviceContext::new(0) else {
            return;
        };
        let vm_a = 9201u64;
        let vm_b = 9202u64;
        clear_vm_host_pin_quota(vm_a);
        clear_vm_host_pin_quota(vm_b);
        set_vm_host_pin_quota(
            vm_a,
            HostPinQuota {
                soft_pinned_bytes: 16 * 1024,
                hard_pinned_bytes: 64 * 1024,
                max_spans: 8,
            },
        );
        set_vm_host_pin_quota(
            vm_b,
            HostPinQuota {
                soft_pinned_bytes: 32 * 1024,
                hard_pinned_bytes: 128 * 1024,
                max_spans: 8,
            },
        );

        let cache_a = HostRegionPinCache::with_context_for_vm(ctx.clone(), vm_a);
        let cache_b = HostRegionPinCache::with_context_for_vm(ctx, vm_b);
        let a1 = vec![1u8; 40 * 1024];
        let a2 = vec![2u8; 40 * 1024];
        let b1 = vec![3u8; 40 * 1024];

        let ga1 = cache_a.pin_span(a1.as_ptr() as usize, a1.len()).unwrap();
        if ga1.is_pinned() {
            assert!(cache_a.stats().soft_watermark_triggers >= 1);
            // Active 40 KiB + incoming 40 KiB exceeds 64 KiB hard quota -> UnpinnedFallback.
            let ga2 = cache_a.pin_span(a2.as_ptr() as usize, a2.len()).unwrap();
            assert_eq!(ga2.status(), PinStatus::UnpinnedFallback);
            assert_eq!(cache_a.stats().quota_denials, 1);
            drop(ga2);

            // VM B has its own quota and is unaffected by VM A's saturation.
            let gb1 = cache_b.pin_span(b1.as_ptr() as usize, b1.len()).unwrap();
            assert_eq!(gb1.status(), PinStatus::Pinned);
            assert_eq!(cache_b.stats().quota_denials, 0);
            drop(gb1);
        }
        drop(ga1);
        drop(cache_a);
        drop(cache_b);
        clear_vm_host_pin_quota(vm_a);
        clear_vm_host_pin_quota(vm_b);
    }
}
