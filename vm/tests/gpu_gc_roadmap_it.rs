// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration coverage for the GPU-GC Roadmap (Phases 2, 3, and 4b):
//!
//! - Contiguous `VmHeap::arena_span_containing` lookup and out-of-arena rejection.
//! - VRAM pressure collector registration, invocation, and `Drop` unregistration.
//! - Deferred stream close validation (`stream_release`, `submission_stream_handle`).
//! - Device-resident `SerializedResult::DeviceArray` materialization and
//!   ephemeral handle release contracts (`release_ephemeral_array_after_enqueue`,
//!   `chain_submission_from_native`).
//! - Zero-copy arena span pinning route selection (`select_async_transfer_route`),
//!   heap teardown unpin hooks (`register_span_unpin_hook`), and
//!   `PinnedHostPool` deferred release reclamation.

use cratonvm_gc::{GcBackend, VmHeap};
use cratonvm_types::{ArrayElementType, ClassId};

#[test]
fn vm_heap_arena_span_containing_finds_backing_span_and_rejects_foreign_addrs() {
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Int, 128);
    let data_ptr = heap
        .array_data_ptr(arr)
        .expect("generational primitive array must have contiguous backing pointer")
        as usize;

    let (base, span_len) = heap
        .arena_span_containing(data_ptr)
        .expect("array backing pointer must map to its containing arena span");
    assert!(data_ptr >= base);
    assert!(
        data_ptr + 128 * std::mem::size_of::<i32>() <= base + span_len,
        "array payload must fit strictly within containing arena span"
    );

    assert!(heap.arena_span_containing(0).is_none());
    let stack_local = 0x1234_5678u64;
    let foreign = (&stack_local as *const u64) as usize;
    assert!(heap.arena_span_containing(foreign).is_none());
}

#[cfg(feature = "gpu-offload")]
mod gpu_offload_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use cratonvm_gc::vm_heap::gpu_coordination;
    use cratonvm_gc::{GcBackend, VmHeap};
    use cratonvm_native_api::registry::DeviceArrayElem;
    use cratonvm_types::{ArrayElementType, ClassId};
    use cratonvm_vm::config::VmConfig;
    use cratonvm_vm::runtime::gpu_marshal::{
        requires_relocation_veto, requires_stream_sync_on_finish, select_async_transfer_route,
        try_pin_native_buffer_for_dma, AsyncDmaLifetime, AsyncTransferRoute, HostRegionPinCache,
        PinnedHostPool, DEFAULT_PINNED_POOL_CAP_BYTES,
    };
    use cratonvm_vm::runtime::offload::{
        chain_submission_from_native, ensure_process_gpu_gc_hooks_bound,
        register_vm_offload_hooks, release_ephemeral_array_after_enqueue,
        submission_stream_handle, OffloadCacheRegistry, SerializedResult,
    };
    use cratonvm_vm::vm::{SharedVm, Vm};

    #[test]
    fn vram_pressure_collector_registers_triggers_and_unregisters() {
        ensure_process_gpu_gc_hooks_bound();
        let (used_mirror, limit_mirror) = gpu_coordination::vram_mirror_atomics();
        let _ = (
            used_mirror.load(Ordering::Relaxed),
            limit_mirror.load(Ordering::Relaxed),
        );

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cb = Arc::clone(&hits);
        let custom_vm_id = 0x5A5A_0000_0000_0001u64;
        gpu_coordination::register_vm_pressure_collector(
            custom_vm_id,
            Arc::new(move || {
                hits_cb.fetch_add(1, Ordering::AcqRel);
                true
            }),
        );

        let before_count = gpu_coordination::gc_triggered_by_vram_pressure();
        assert!(gpu_coordination::collect_garbage_for_gpu_pressure());
        assert_eq!(hits.load(Ordering::Acquire), 1);
        assert!(gpu_coordination::gc_triggered_by_vram_pressure() > before_count);

        gpu_coordination::unregister_vm_pressure_collector(custom_vm_id);
        let _ = gpu_coordination::collect_garbage_for_gpu_pressure();
        assert_eq!(
            hits.load(Ordering::Acquire),
            1,
            "unregistered VM pressure collector must not fire on subsequent pressure events"
        );
    }

    #[test]
    fn register_vm_offload_hooks_binds_and_cleans_up_on_vm_drop() {
        let vm = Vm::new(VmConfig::new());
        register_vm_offload_hooks(&vm.shared);
        // Repeated registration on the same VM must remain idempotent.
        register_vm_offload_hooks(&vm.shared);

        drop(vm);
        // Collecting after the VM drops must not touch torn-down state.
        let _ = gpu_coordination::collect_garbage_for_gpu_pressure();
    }

    #[test]
    fn stream_release_unknown_handle_and_submission_stream_handle_miss() {
        let config = VmConfig::new();
        let registry = OffloadCacheRegistry::new();
        let cache = registry.get_or_create(config.gpu_device_ordinal, &config);

        cache.stream_release(9_999_999);
        assert!(cache.resolve_stream(9_999_999).is_none());
        assert_eq!(submission_stream_handle(0), None);
        assert_eq!(submission_stream_handle(9_999_999), None);
    }

    #[test]
    fn device_array_result_and_chain_submission_host_contracts() {
        let shared = Arc::new(SharedVm::new(VmConfig::new()));

        let empty = SerializedResult::DeviceArray {
            array_handle: 0,
            elem: DeviceArrayElem::I32,
            len: 0,
        };
        assert_eq!(empty.materialize_array_bytes(), Some(Vec::new()));

        let stale = SerializedResult::DeviceArray {
            array_handle: 0x8000_0000_0001_2345,
            elem: DeviceArrayElem::F64,
            len: 8,
        };
        assert_eq!(stale.materialize_array_bytes(), None);

        release_ephemeral_array_after_enqueue(0);
        release_ephemeral_array_after_enqueue(0x8000_0000_0001_2345);

        let zero_err = chain_submission_from_native(&shared, 0, "NoClass", "m", "([I)[I", &[])
            .expect_err("handle=0 predecessor must fail chain_submission_from_native");
        assert!(zero_err.contains("0"));

        let missing_err =
            chain_submission_from_native(&shared, 9_876_543, "NoClass", "m", "([I)[I", &[])
                .expect_err("unknown predecessor handle must fail chain_submission_from_native");
        assert!(missing_err.contains("unknown"));
    }

    #[test]
    fn arena_span_pin_route_fallback_unpin_hook_and_pooled_deferred_reclaim() {
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Int, 256);
        let ptr = heap.array_data_ptr(arr).expect("array backing ptr") as usize;
        let bytes = 256 * std::mem::size_of::<i32>();
        let (span_base, span_len) = heap
            .arena_span_containing(ptr)
            .expect("arena_span_containing must resolve heap array");

        let pin_cache = HostRegionPinCache::new();
        let (route, guard) = select_async_transfer_route(ptr, bytes, &heap, &pin_cache);
        match route {
            AsyncTransferRoute::ZeroCopyPinnedSpan { base, len } => {
                assert_eq!(base, span_base);
                assert_eq!(len, span_len);
                assert!(guard.as_ref().is_some_and(|g| g.is_pinned()));
            }
            AsyncTransferRoute::PooledFallback => {
                assert!(guard.is_none());
            }
        }
        drop(guard);

        let pool = PinnedHostPool::with_cap(DEFAULT_PINNED_POOL_CAP_BYTES);
        let lease = pool.acquire(4096, 1).expect("pooled fallback allocation");
        let done = Arc::new(AtomicBool::new(false));
        let done_cb = Arc::clone(&done);
        pool.release_deferred(lease, 1, move || done_cb.load(Ordering::Acquire));
        assert_eq!(pool.reclaim_completed(), 0);
        assert!(pool.stats().deferred_bytes >= 4096);

        done.store(true, Ordering::Release);
        assert_eq!(pool.reclaim_completed(), 1);
        assert_eq!(pool.stats().deferred_bytes, 0);
        assert!(pool.stats().ready_bytes >= 4096);

        let unpinned = Arc::new(AtomicBool::new(false));
        let unpinned_cb = Arc::clone(&unpinned);
        gpu_coordination::register_span_unpin_hook(
            span_base,
            span_len,
            Box::new(move || {
                unpinned_cb.store(true, Ordering::Release);
            }),
        );
        drop(heap);
        assert!(
            unpinned.load(Ordering::Acquire),
            "VmHeap::drop must invoke span unpin hook before unmapping arena backing pages"
        );
    }

    #[test]
    fn offload_jit_gate_evict_vm_and_shared_vm_drop_are_safe() {
        let mut config = VmConfig::new();
        config.gpu_offload_enabled = true;
        let shared = Arc::new(SharedVm::new(config));
        let vm_id = shared.vm_identity;
        let _ = cratonvm_vm::runtime::offload_jit_gate::caller_blocks_jit(
            &shared,
            ClassId::new(1),
            0,
        );
        drop(shared);
        cratonvm_vm::runtime::offload_jit_gate::evict_vm(vm_id);
    }

    #[test]
    fn ffm_native_buffer_zero_copy_bypasses_relocation_forbidden() {
        use cratonvm_vm::runtime::gpu_marshal::{
            requires_relocation_veto, resolve_native_buffer_span,
            select_native_buffer_transfer_route, AsyncDmaLifetime,
        };

        // 1. Arena-tagged native handle resolves to its real host pointer + bounds.
        let handle = cratonvm_native_builtins::unsafe_natives_ext::unsafe_arena_allocate(1024);
        let (real_ptr, len) =
            resolve_native_buffer_span(handle, 512).expect("live arena handle resolves");
        assert_eq!(len, 512);
        assert_ne!(real_ptr, 0);
        if (handle & (1i64 << 62)) != 0 {
            // Out-of-bounds length on an arena-tagged handle must be rejected.
            assert!(resolve_native_buffer_span(handle, 65536).is_none());
        }
        cratonvm_native_builtins::unsafe_natives_ext::unsafe_arena_free(handle);
        if (handle & (1i64 << 62)) != 0 {
            // Freed arena-tagged handle must be rejected.
            assert!(resolve_native_buffer_span(handle, 64).is_none());
        }

        // 2. Untagged real native pointer resolves directly without VmHeap lookup.
        let mut backing = vec![0u8; 256];
        let raw_ptr = backing.as_mut_ptr() as i64;
        let (resolved, resolved_len) =
            resolve_native_buffer_span(raw_ptr, 256).expect("raw pointer resolves");
        assert_eq!(resolved, backing.as_ptr() as usize);
        assert_eq!(resolved_len, 256);

        // 3. Pinning route + Relocation::Forbidden bypass check.
        let pin_cache = HostRegionPinCache::new();
        let (route, guard) = select_native_buffer_transfer_route(resolved, 256, &pin_cache);
        if let Some(g) = guard {
            assert!(matches!(route, AsyncTransferRoute::ZeroCopyPinnedSpan { .. }));
            let guards = [AsyncDmaLifetime::ZeroCopyNativeSpan(g)];
            assert!(
                !requires_relocation_veto(&guards),
                "off-heap FFM/DirectByteBuffer zero-copy must NOT require Relocation::Forbidden"
            );
        } else {
            assert!(matches!(route, AsyncTransferRoute::PooledFallback));
            let guards = [AsyncDmaLifetime::Completed];
            assert!(!requires_relocation_veto(&guards));
        }

        // Heap ZeroCopySpan still requires Relocation::Forbidden when pinned.
        let fallback_guard = pin_cache
            .pin_span(resolved, 256)
            .expect("pin_span returns Pinned or UnpinnedFallback");
        assert!(requires_relocation_veto(&[AsyncDmaLifetime::ZeroCopySpan(
            fallback_guard
        )]));
    }

    #[test]
    fn gpu4j_followup_bridge_registrations_and_zero_copy_promote_contract() {
        let mut reg = cratonvm_native_api::NativeMethodRegistry::new();
        cratonvm_native_builtins::register_essential_natives(&mut reg);

        for (name, desc) in [
            ("streamSynchronize", "(J)V"),
            ("futureGetResultAsGpuArray", "(J)Lcraton/gpu/GpuArray;"),
            (
                "streamSubmitMethod",
                "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)J",
            ),
            (
                "futureThenApplyGpu",
                "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)J",
            ),
        ] {
            let (_cb, kind) = reg
                .find_with_kind("craton/gpu/internal/Native", name, desc)
                .unwrap_or_else(|| panic!("missing Native.{name}{desc}"));
            assert_eq!(kind, cratonvm_native_api::NativeKind::Bridge);
        }

        let shared = Arc::new(SharedVm::new(VmConfig::new()));
        let cache = shared
            .offload_registry
            .get_or_create(shared.config.gpu_device_ordinal, &shared.config);
        let sync_err = cache
            .stream_synchronize(0xDEAD_BEEF)
            .expect_err("unknown stream handle must error");
        assert!(sync_err.contains("unknown"));
    }

    #[test]
    fn zero_copy_native_span_requires_stream_sync_without_relocation_veto() {
        let mut backing = vec![0u8; 4096];
        let cache = HostRegionPinCache::new();
        let guard = cache
            .pin_span(backing.as_mut_ptr() as usize, backing.len())
            .expect("pin_span in HostRegionPinCache must succeed");
        let guards = [AsyncDmaLifetime::ZeroCopyNativeSpan(guard)];
        assert!(
            !requires_relocation_veto(&guards),
            "off-heap ZeroCopyNativeSpan must not force Relocation::Forbidden"
        );
        assert!(
            requires_stream_sync_on_finish(&guards),
            "off-heap ZeroCopyNativeSpan must still synchronize DMA stream before dropping guard"
        );
        assert!(!requires_stream_sync_on_finish(&[AsyncDmaLifetime::Completed]));
    }

    #[test]
    fn shared_host_span_requires_relocation_veto_and_stream_sync() {
        let mut backing = vec![0u8; 4096];
        let cache = HostRegionPinCache::new();
        let guard = cache
            .pin_span(backing.as_mut_ptr() as usize, backing.len())
            .expect("pin_span must succeed");
        let with_guard = [AsyncDmaLifetime::SharedHostSpan(Some(guard))];
        assert!(requires_relocation_veto(&with_guard));
        assert!(requires_stream_sync_on_finish(&with_guard));

        let unpinned_shared = [AsyncDmaLifetime::SharedHostSpan(None)];
        assert!(requires_relocation_veto(&unpinned_shared));
        assert!(requires_stream_sync_on_finish(&unpinned_shared));
    }

    #[test]
    fn unsafe_arena_free_invokes_span_unpin_hooks_before_releasing_backing_memory() {
        ensure_process_gpu_gc_hooks_bound();
        let handle = cratonvm_native_builtins::unsafe_natives_ext::unsafe_arena_allocate(4096);
        assert_ne!(handle, 0);
        let (real_ptr, len) =
            cratonvm_native_builtins::unsafe_natives_ext::unsafe_arena_real_ptr(handle)
                .expect("allocated arena block must resolve to a real pointer");
        let base = real_ptr as usize;

        let unpinned = Arc::new(AtomicBool::new(false));
        let unpinned_cb = Arc::clone(&unpinned);
        gpu_coordination::register_span_unpin_hook(
            base,
            len,
            Box::new(move || {
                unpinned_cb.store(true, Ordering::Release);
            }),
        );

        let pin_cache = HostRegionPinCache::new();
        let guard = try_pin_native_buffer_for_dma(base, len, &pin_cache);
        let was_pinned = guard.is_some();
        drop(guard);

        cratonvm_native_builtins::unsafe_natives_ext::unsafe_arena_free(handle);

        assert!(
            unpinned.load(Ordering::Acquire),
            "unsafe_arena_free must invoke registered span unpin hook before releasing host memory"
        );
        if was_pinned {
            assert!(
                !pin_cache.is_span_cached(base, len),
                "pinned native span must be unregistered when unsafe_arena_free runs"
            );
        }
    }

    #[test]
    fn vm_config_gpu_quotas_sandbox_stats_and_jfr_emission() {
        use cratonvm_vm::config::VmConfig;
        use cratonvm_vm::runtime::offload::{
            all_vm_gpu_sandbox_stats, clear_vm_gpu_sandbox_quotas, emit_gpu_memory_stats_jfr,
            vm_gpu_sandbox_stats, OffloadCache,
        };

        let vm_a = 8801u64;
        let vm_b = 8802u64;
        let mut cfg_a = VmConfig::default();
        cfg_a.gpu_vram_soft_quota_bytes = 2 << 20;
        cfg_a.gpu_vram_hard_quota_bytes = 4 << 20;
        cfg_a.gpu_pin_soft_quota_bytes = 512 << 10;
        cfg_a.gpu_pin_hard_quota_bytes = 1 << 20;
        cfg_a.gpu_pin_max_spans = 3;

        let mut cfg_b = VmConfig::default();
        cfg_b.gpu_vram_soft_quota_bytes = 4 << 20;
        cfg_b.gpu_vram_hard_quota_bytes = 8 << 20;
        cfg_b.gpu_pin_max_spans = 6;

        let cache_a = OffloadCache::new_for_vm(&cfg_a, 0, vm_a);
        let cache_b = OffloadCache::new_for_vm(&cfg_b, 0, vm_b);

        let stats_a = vm_gpu_sandbox_stats(vm_a, 0);
        let stats_b = vm_gpu_sandbox_stats(vm_b, 0);
        assert_eq!(stats_a.vram.vram_soft_limit_bytes, 2 << 20);
        assert_eq!(stats_a.vram.vram_hard_limit_bytes, 4 << 20);
        assert_eq!(stats_a.host_pin.max_spans, 3);
        assert_eq!(stats_b.vram.vram_hard_limit_bytes, 8 << 20);
        assert_eq!(stats_b.host_pin.max_spans, 6);
        assert!(all_vm_gpu_sandbox_stats()
            .iter()
            .any(|s| s.vram.vm_identity == vm_a));

        let mut fr = cratonvm_jfr::create_flight_recorder();
        let rid = fr.new_recording(cratonvm_jfr::RecordingSettings::new("vm-gpu-sandbox"));
        fr.start_recording(rid);
        emit_gpu_memory_stats_jfr(&mut fr, vm_a, 0, 999_000);
        let shared = Arc::new(SharedVm::new(cfg_a));
        let _ = shared.offload_registry.get_or_create(0, &shared.config);
        cratonvm_vm::runtime::offload::emit_vm_gpu_memory_stats_jfr(&mut fr, &shared, 999_500);
        fr.drain_per_thread_into_repository();
        assert!(fr.type_registry.find_by_name("craton.GpuMemoryStats").is_some());

        drop(cache_a);
        drop(cache_b);
        clear_vm_gpu_sandbox_quotas(vm_a);
        clear_vm_gpu_sandbox_quotas(vm_b);
    }

    #[test]
    fn gpu_native_array_allocate_and_release_drives_vm_heap_external_vram_pressure() {
        use cratonvm_native_api::NativeMethodRegistry;
        use cratonvm_types::Value;
        use cratonvm_vm::config::{GcAlgorithm, VmConfig};
        use cratonvm_vm::threading::{JvmThread, ThreadId};
        use cratonvm_vm::vm::NativeContextImpl;

        let mut algs = vec![GcAlgorithm::G1];
        #[cfg(feature = "zgc")]
        algs.push(GcAlgorithm::Zgc);

        let mut registry = NativeMethodRegistry::new();
        cratonvm_native_builtins::register_essential_natives(&mut registry);
        let alloc_fn = registry
            .find("craton/gpu/internal/Native", "arrayAllocateFloat", "(I)J")
            .expect("Native.arrayAllocateFloat must be registered under gpu-offload");
        let release_fn = registry
            .find("craton/gpu/internal/Native", "releaseArray", "(J)V")
            .expect("Native.releaseArray must be registered under gpu-offload");

        for alg in algs {
            let mut cfg = VmConfig::new();
            cfg.gc_algorithm = alg;
            let shared = Arc::new(SharedVm::new(cfg));
            shared.mem.heap.set_external_gpu_pressure_policy(
                32 * 1024 * 1024,
                2 * 1024 * 1024,
                8 * 1024 * 1024,
            );
            let mut thread = JvmThread::new(ThreadId(1), "gpu-pressure-test");
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };

            let handle_val = alloc_fn(&mut ctx, &[Value::Int(4 * 1024 * 1024)])
                .expect("arrayAllocateFloat should succeed")
                .expect("arrayAllocateFloat must return handle");
            let handle = match handle_val {
                Value::Long(h) => h,
                other => panic!("expected Value::Long handle, got {other:?}"),
            };
            assert!(handle > 0);
            assert_eq!(shared.mem.heap.external_gpu_retained_bytes(), 16 * 1024 * 1024);
            assert_eq!(shared.mem.heap.external_gpu_live_wrappers(), 1);
            assert!(
                shared.mem.heap.young_spill_pressure(),
                "16 MiB GpuArray wrapper on idle heap must arm {alg:?} young_spill_pressure"
            );
            assert!(
                shared.mem.heap.needs_gc(),
                "16 MiB GpuArray wrapper on idle heap must bias {alg:?} needs_gc()"
            );

            release_fn(&mut ctx, &[Value::Long(handle)])
                .expect("releaseArray should succeed");
            assert_eq!(shared.mem.heap.external_gpu_retained_bytes(), 0);
            assert_eq!(shared.mem.heap.external_gpu_live_wrappers(), 0);
        }
    }

    #[test]
    fn per_vm_cuda_critical_token_isolates_sibling_vm_heap() {
        use cratonvm_vm::config::VmConfig;
        use cratonvm_vm::runtime::offload::{register_vm_offload_hooks, GcCriticalGuard};

        let vm_a = Arc::new(SharedVm::new(VmConfig::new()));
        let vm_b = Arc::new(SharedVm::new(VmConfig::new()));
        register_vm_offload_hooks(&vm_a);
        register_vm_offload_hooks(&vm_b);

        let tok_a = GcCriticalGuard::acquire_for(
            vm_a.vm_identity as u64,
            Some(42),
            "per_vm_isolation_test",
            cuda_bridge::critical::Relocation::Forbidden,
            &[],
        );

        assert!(vm_a.mem.heap.gpu_coord().relocation_forbidden());
        assert!(!vm_b.mem.heap.gpu_coord().relocation_forbidden());

        let cycle_b = vm_b.mem.heap.gpu_coord().before_collection();
        assert!(!vm_b.mem.heap.gpu_coord().relocation_forbidden());
        cycle_b.after_collection(&[]);

        drop(tok_a);
        assert!(!vm_a.mem.heap.gpu_coord().relocation_forbidden());
    }
}
