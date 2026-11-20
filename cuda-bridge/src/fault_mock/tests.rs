    use super::*;

    #[test]
    fn vmm_pool_rolls_back_on_map_and_set_access_faults() {
        let drv = FaultMockVmmDriver::new(DEFAULT_VMM_CHUNK_BYTES);
        let ledger = drv.ledger_clone();
        drv.fail_after(VmmOp::Map, 0);
        let pool = VmmPool::new(drv, DEFAULT_VMM_CHUNK_BYTES * 2);

        let err = pool.acquire_slab(DEFAULT_VMM_CHUNK_BYTES);
        assert!(err.is_err(), "injected map failure must propagate");
        FaultMockVmmDriver::assert_clean(&ledger);

        // Retry with SetAccess failure after Map succeeds: must unmap, release handle, free VA.
        pool.driver().clear_fault(VmmOp::Map);
        pool.driver().fail_after(VmmOp::SetAccess, 0);
        let err = pool.acquire_slab(DEFAULT_VMM_CHUNK_BYTES);
        assert!(err.is_err(), "injected set_access failure must propagate");
        FaultMockVmmDriver::assert_clean(&ledger);
    }

    #[test]
    fn vmm_pool_multi_chunk_partial_create_physical_oom_releases_all() {
        let drv = FaultMockVmmDriver::new(DEFAULT_VMM_CHUNK_BYTES);
        let ledger = drv.ledger_clone();
        // 4-chunk slab fails on 3rd physical handle (index 2).
        drv.fail_after(VmmOp::CreatePhysical, 2);
        let pool = VmmPool::new(drv, DEFAULT_VMM_CHUNK_BYTES * 4);

        let err = pool.acquire_slab(DEFAULT_VMM_CHUNK_BYTES * 4);
        assert!(err.is_err());
        FaultMockVmmDriver::assert_clean(&ledger);
    }

    #[test]
    fn graph_vmm_arena_partial_commit_fault_rolls_back_cleanly() {
        let drv = FaultMockVmmDriver::new(DEFAULT_VMM_CHUNK_BYTES);
        let ledger = drv.ledger_clone();
        let pool = VmmPool::new(drv, DEFAULT_VMM_CHUNK_BYTES * 4);
        let arena = pool
            .reserve_graph_arena(DEFAULT_VMM_CHUNK_BYTES * 3)
            .expect("reserve VA");

        // Fail SetAccess on the 2nd chunk when mapping all 3 chunks.
        pool.driver().fail_after(VmmOp::SetAccess, 1);
        let err = arena.alloc(DEFAULT_VMM_CHUNK_BYTES * 3);
        assert!(err.is_err());
        // Clean up chunk #0 & VA by dropping arena; no handles/VAs may leak.
        drop(arena);
        drop(pool);
        FaultMockVmmDriver::assert_clean(&ledger);
    }

    #[test]
    fn suballocator_fault_rollback_for_managed_spill_and_native_stream_pool() {
        struct StreamPoolReset(bool);
        impl Drop for StreamPoolReset {
            fn drop(&mut self) {
                crate::allocator::set_native_stream_pool_enabled(self.0);
            }
        }
        let prev_pool = crate::allocator::native_stream_pool_enabled();
        let _reset = StreamPoolReset(prev_pool);

        // 1. Native stream pool failure with no spill propagates cleanly and leaks nothing.
        crate::allocator::set_native_stream_pool_enabled(true);
        let backend_sp = FaultMockSlabBackend::new();
        let ledger_sp = backend_sp.ledger_clone();
        backend_sp.fail_after(SlabOp::StreamPoolAlloc, 0);
        let alloc_sp = SubAllocator::<_, MockFence>::new_arc(backend_sp, 1024 * 1024, true);
        assert!(alloc_sp.alloc(1024, None).is_err());
        drop(alloc_sp);
        FaultMockSlabBackend::assert_clean(&ledger_sp);

        // 2. Portable slab mode: fail AllocSlab AND fail AllocManaged -> spill_used_bytes stays 0.
        crate::allocator::set_native_stream_pool_enabled(false);
        let backend = FaultMockSlabBackend::new();
        let ledger = backend.ledger_clone();
        let alloc = SubAllocator::<_, MockFence>::new_arc(backend, 1024 * 1024, true);
        alloc.set_spill_config(crate::allocator::SpillConfig {
            enabled: true,
            budget_bytes: 8 * 1024 * 1024,
            min_tensor_bytes: 512,
            read_mostly_only: false,
        });
        let lease = alloc.alloc(1024, None).expect("initial slab alloc");
        alloc.free_deferred(lease, None, None);

        alloc.backend().fail_after(SlabOp::AllocSlab, 0);
        alloc.backend().fail_after(SlabOp::AllocManaged, 0);
        let err = alloc.alloc_with_hint(
            2 * 1024 * 1024,
            None,
            crate::allocator::AllocHint::ReadMostlyTensor,
        );
        assert!(err.is_err(), "OOM + failed managed spill must error");
        assert_eq!(
            alloc.spill_stats().used_bytes,
            0,
            "failed alloc_managed_slab must roll back spill_used_bytes"
        );
        drop(alloc);
        FaultMockSlabBackend::assert_clean(&ledger);
    }

    #[test]
    fn host_pin_cache_fault_injection_degrades_and_evicts_cleanly() {
        let vm_id = 0xDEAD_BEEF_CAFE_0001u64;
        set_vm_host_pin_quota(
            vm_id,
            HostPinQuota {
                soft_pinned_bytes: 4096,
                hard_pinned_bytes: 16 * 1024,
                max_spans: 2,
            },
        );
        // Allow 1 successful registration (`span_a`), then fail call #1 (`span_b`).
        let ops = Arc::new(FaultMockHostPinOps::new(Some(1)));
        let cache = HostRegionPinCache::with_test_pin_ops_for_vm(vm_id, ops.clone());

        // Call #0 succeeds -> Pinned (4 KiB).
        let guard_a = cache.pin_span(0x10_0000, 4096).expect("pin a");
        assert!(guard_a.is_pinned());
        assert_eq!(cache.stats().pinned_bytes, 4096);

        // Call #1 fails (`remaining_successes == 0`): must roll back pinned_bytes to 4096 and degrade.
        let guard_b = cache.pin_span(0x20_0000, 4096).expect("pin b fallback");
        assert!(guard_b.is_fallback());
        assert_eq!(cache.stats().pinned_bytes, 4096);
        drop(guard_a);
        drop(guard_b);

        // Re-arm ops: `span_c` evicts unpinned fallback `span_b` first (`unregister_calls == 0`),
        // then `span_d` evicts pinned `span_a` (`unregister_calls == 1`).
        *ops.remaining_successes.lock().unwrap() = Some(2);
        let guard_c = cache.pin_span(0x30_0000, 4096).expect("pin c evicts fallback");
        assert!(guard_c.is_pinned());
        drop(guard_c);
        assert_eq!(ops.unregister_calls.load(Ordering::Relaxed), 0);

        let guard_d = cache.pin_span(0x40_0000, 4096).expect("pin d evicts pinned span_a");
        assert!(guard_d.is_pinned());
        assert_eq!(ops.unregister_calls.load(Ordering::Relaxed), 1);
        drop(guard_d);

        drop(cache);
        assert!(
            ops.live_pins.lock().unwrap().is_empty(),
            "dropping cache must unregister all remaining live spans"
        );
        assert_eq!(ops.unregister_calls.load(Ordering::Relaxed), 3);
    }
