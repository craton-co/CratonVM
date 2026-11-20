// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration tests for GPU VRAM accounting mirror, pressure-triggered
//! collection re-entrancy, `VmHeap::arena_span_containing`, heap-teardown
//! unpin hooks, and relocation-veto `CycleGuard` invariants.

use cratonvm_gc::vm_heap::{GcBackend, VmHeap};
use cratonvm_types::{ArrayElementType, ClassId};
use std::sync::Mutex;

#[cfg(feature = "gpu-offload")]
static COORD_LOCK: Mutex<()> = Mutex::new(());

#[cfg(feature = "gpu-offload")]
mod gpu_coord_tests {
    use super::*;
    use cratonvm_gc::vm_heap::{gpu_coordination, gpu_relocation_forbidden};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    struct VramResetGuard;
    impl Drop for VramResetGuard {
        fn drop(&mut self) {
            let (used, limit) = gpu_coordination::vram_mirror_atomics();
            used.store(0, Ordering::Release);
            limit.store(0, Ordering::Release);
        }
    }

    #[test]
    fn vram_mirror_atomics_and_pressure_threshold() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _reset = VramResetGuard;

        let (used_atomic, limit_atomic) = gpu_coordination::vram_mirror_atomics();
        used_atomic.store(0, Ordering::Release);
        gpu_coordination::set_vram_reservation_limit(0);

        assert_eq!(gpu_coordination::vram_used_bytes(), 0);
        assert_eq!(gpu_coordination::vram_reservation_limit(), 0);
        assert!(
            !gpu_coordination::is_vram_under_pressure(),
            "limit == 0 means unbounded"
        );

        gpu_coordination::set_vram_reservation_limit(1_000);
        assert_eq!(limit_atomic.load(Ordering::Acquire), 1_000);

        // Exactly 80% is not strictly over 80%.
        used_atomic.store(800, Ordering::Release);
        assert!(!gpu_coordination::is_vram_under_pressure());

        // > 80% trips pressure detection.
        used_atomic.store(801, Ordering::Release);
        assert!(gpu_coordination::is_vram_under_pressure());

        // Large values must not overflow u64 multiplication.
        gpu_coordination::set_vram_reservation_limit(u64::MAX);
        used_atomic.store(u64::MAX / 2, Ordering::Release);
        assert!(!gpu_coordination::is_vram_under_pressure());
        used_atomic.store(u64::MAX, Ordering::Release);
        assert!(gpu_coordination::is_vram_under_pressure());
    }

    #[test]
    fn pressure_collector_drops_locks_before_reentrant_callbacks() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _reset = VramResetGuard;

        let (used_atomic, _) = gpu_coordination::vram_mirror_atomics();
        used_atomic.store(900, Ordering::Release);
        gpu_coordination::set_vram_reservation_limit(1_000);

        let ran_vm1 = Arc::new(AtomicBool::new(false));
        let ran_vm1_cb = Arc::clone(&ran_vm1);

        gpu_coordination::register_vm_pressure_collector(
            101,
            Arc::new(move || {
                ran_vm1_cb.store(true, Ordering::Release);
                // Re-enter gpu_coordination APIs during callback (simulating
                // Cleaner -> StreamCleaner -> free_deferred + VM registration
                // mutation). This would deadlock if COORD locks remained held.
                let (used, _limit) = gpu_coordination::vram_mirror_atomics();
                used.fetch_sub(400, Ordering::AcqRel);
                let _ = gpu_coordination::vram_used_bytes();
                let _ = gpu_coordination::is_vram_under_pressure();
                gpu_coordination::unregister_vm_pressure_collector(102);
                gpu_coordination::register_vm_pressure_collector(103, Arc::new(|| false));
                true
            }),
        );
        gpu_coordination::register_vm_pressure_collector(102, Arc::new(|| false));

        let gcs_before = gpu_coordination::gc_triggered_by_vram_pressure();
        let triggered = gpu_coordination::collect_garbage_for_gpu_pressure();

        gpu_coordination::unregister_vm_pressure_collector(101);
        gpu_coordination::unregister_vm_pressure_collector(102);
        gpu_coordination::unregister_vm_pressure_collector(103);

        assert!(triggered);
        assert!(ran_vm1.load(Ordering::Acquire));
        assert_eq!(gpu_coordination::vram_used_bytes(), 500);
        assert!(!gpu_coordination::is_vram_under_pressure());
        assert_eq!(
            gpu_coordination::gc_triggered_by_vram_pressure(),
            gcs_before + 1
        );

        // With all collectors unregistered, pressure trigger is a no-op.
        assert!(!gpu_coordination::collect_garbage_for_gpu_pressure());
        assert_eq!(
            gpu_coordination::gc_triggered_by_vram_pressure(),
            gcs_before + 1
        );
    }

    #[test]
    fn relocation_veto_before_collection_and_cycle_guard_isolation() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        assert!(!gpu_relocation_forbidden());
        let cycle_a = gpu_coordination::before_collection();
        assert!(cycle_a.extra_roots.is_empty());
        assert!(!gpu_relocation_forbidden());

        gpu_coordination::forbid_relocation_this_cycle();
        assert!(gpu_relocation_forbidden());

        // Another collecting thread finishing its cycle must not clear our
        // per-cycle veto while `cycle_a` is still active.
        let still_forbidden = std::thread::spawn(|| {
            let cycle_b = gpu_coordination::before_collection();
            let during_b = gpu_relocation_forbidden();
            cycle_b.after_collection(&[]);
            during_b
        })
        .join()
        .expect("worker thread completes");

        assert!(still_forbidden);
        assert!(gpu_relocation_forbidden());

        cycle_a.after_collection(&[]);
        assert!(!gpu_relocation_forbidden());
    }

    #[test]
    fn heap_drop_invokes_matching_unpin_hooks_before_teardown() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let span_hits = Arc::new(AtomicUsize::new(0));
        let global_hits = Arc::new(AtomicUsize::new(0));
        let unrelated_hits = Arc::new(AtomicUsize::new(0));

        let heap = VmHeap::new(GcBackend::Generational, 16 << 20);
        let obj = heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 64)
            .expect("allocate array in generational heap");
        let addr = obj.as_ptr() as usize;
        let (base, len) = heap
            .arena_span_containing(addr)
            .expect("allocated object falls inside an arena span");

        let span_hits_cb = Arc::clone(&span_hits);
        gpu_coordination::register_span_unpin_hook(
            base,
            len,
            Box::new(move || {
                span_hits_cb.fetch_add(1, Ordering::AcqRel);
            }),
        );

        let unrelated_cb = Arc::clone(&unrelated_hits);
        gpu_coordination::register_span_unpin_hook(
            usize::MAX - 4095,
            4096,
            Box::new(move || {
                unrelated_cb.fetch_add(1, Ordering::AcqRel);
            }),
        );

        let global_hits_cb = Arc::clone(&global_hits);
        gpu_coordination::register_heap_unpin_hook(Box::new(move || {
            global_hits_cb.fetch_add(1, Ordering::AcqRel);
        }));

        drop(heap);

        assert_eq!(span_hits.load(Ordering::Acquire), 1);
        assert_eq!(global_hits.load(Ordering::Acquire), 1);
        assert_eq!(
            unrelated_hits.load(Ordering::Acquire),
            0,
            "unpin hook for an unrelated address range must not fire"
        );

        // Drain the unrelated hook so it does not linger in COORD.
        gpu_coordination::run_heap_unpin_hooks();
        assert_eq!(unrelated_hits.load(Ordering::Acquire), 1);
    }

    #[test]
    fn wait_for_gpu_critical_drain_blocks_enter_gpu_critical_until_released() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let heap = VmHeap::new(GcBackend::Generational, 4 << 20);
        assert_eq!(heap.gpu_critical_count(), 0);

        let drain = cratonvm_gc::vm_heap::wait_for_gpu_critical_drain();
        assert_eq!(heap.gpu_critical_count(), 0);
        assert!(!gpu_relocation_forbidden());

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let entered = Arc::new(AtomicBool::new(false));
        let entered_cb = Arc::clone(&entered);
        let worker = std::thread::spawn(move || {
            let worker_heap = VmHeap::new(GcBackend::Generational, 4 << 20);
            started_tx.send(()).unwrap();
            let _token = worker_heap.enter_gpu_critical();
            entered_cb.store(true, Ordering::Release);
            assert_eq!(worker_heap.gpu_critical_count(), 1);
        });

        started_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            !entered.load(Ordering::Acquire),
            "enter_gpu_critical must stay blocked while GpuCriticalDrain holds GPU_COLLECTING"
        );

        drop(drain);
        worker.join().unwrap();
        assert!(entered.load(Ordering::Acquire));
        assert_eq!(heap.gpu_critical_count(), 0);
    }

    #[test]
    fn decommit_and_reset_invoke_matching_span_unpin_hooks_only() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        gpu_coordination::run_heap_unpin_hooks();

        let granule = cratonvm_gc::reservation::GRANULE;
        let mut store = cratonvm_gc::reservation::HeapStore::new(2 * granule, "gpu_unpin_test");
        if matches!(store, cratonvm_gc::reservation::HeapStore::Owned(_)) {
            return;
        }
        assert!(store.commit_range(0, 2 * granule));
        let base = store.as_ptr() as usize;

        let g0_hits = Arc::new(AtomicUsize::new(0));
        let g1_hits = Arc::new(AtomicUsize::new(0));
        let global_hits = Arc::new(AtomicUsize::new(0));

        let g0_cb = Arc::clone(&g0_hits);
        gpu_coordination::register_span_unpin_hook(
            base,
            4096,
            Box::new(move || {
                g0_cb.fetch_add(1, Ordering::AcqRel);
            }),
        );
        let g1_cb = Arc::clone(&g1_hits);
        gpu_coordination::register_span_unpin_hook(
            base + granule,
            4096,
            Box::new(move || {
                g1_cb.fetch_add(1, Ordering::AcqRel);
            }),
        );
        let global_cb = Arc::clone(&global_hits);
        gpu_coordination::register_heap_unpin_hook(Box::new(move || {
            global_cb.fetch_add(1, Ordering::AcqRel);
        }));

        let decommitted = store.decommit_range(0, granule, "gpu_unpin_test");
        assert!(decommitted >= granule);
        assert_eq!(g0_hits.load(Ordering::Acquire), 1);
        assert_eq!(g1_hits.load(Ordering::Acquire), 0);
        assert_eq!(
            global_hits.load(Ordering::Acquire),
            0,
            "decommitting a range must not drain/fire global heap teardown hooks"
        );

        let (unmapped, advised) = store.reset_range(granule, granule, "gpu_unpin_test");
        assert!(unmapped + advised >= granule);
        assert_eq!(g1_hits.load(Ordering::Acquire), 1);
        assert_eq!(global_hits.load(Ordering::Acquire), 0);

        gpu_coordination::run_heap_unpin_hooks();
    }

    #[test]
    fn multi_heap_drop_preserves_global_unpin_hooks_for_surviving_heaps() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        gpu_coordination::run_heap_unpin_hooks();

        let heap1 = VmHeap::new(GcBackend::Generational, 4 << 20);
        let heap2 = VmHeap::new(GcBackend::Generational, 4 << 20);

        let global_hits = Arc::new(AtomicUsize::new(0));
        let global_cb = Arc::clone(&global_hits);
        gpu_coordination::register_heap_unpin_hook(Box::new(move || {
            global_cb.fetch_add(1, Ordering::AcqRel);
        }));

        drop(heap1);
        assert_eq!(global_hits.load(Ordering::Acquire), 1);

        drop(heap2);
        assert_eq!(
            global_hits.load(Ordering::Acquire),
            2,
            "dropping one VmHeap must not discard global unpin hooks needed by surviving VmHeaps"
        );

        gpu_coordination::run_heap_unpin_hooks();
    }

    #[test]
    fn wait_for_gpu_critical_drain_does_not_double_count_forced_non_moving() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        static FORCED: AtomicUsize = AtomicUsize::new(0);
        struct CountingBridge;
        impl gpu_coordination::GpuCriticalBridge for CountingBridge {
            fn collector_wait_budget(&self) -> std::time::Duration {
                std::time::Duration::from_millis(50)
            }
            fn record_forced_non_moving_collection(&self) {
                FORCED.fetch_add(1, Ordering::AcqRel);
            }
            fn relocation_forbidden(&self) -> bool {
                false
            }
            fn relocation_forbidden_for_vm(&self, _scope_vm: Option<u64>) -> bool {
                false
            }
            fn outstanding(&self) -> usize {
                0
            }
            fn outstanding_for_vm(&self, _scope_vm: Option<u64>) -> usize {
                0
            }
            fn wait_for_relocation_clearance(&self, _budget: std::time::Duration) -> bool {
                true
            }
            fn wait_for_relocation_clearance_for_vm(
                &self,
                _scope_vm: Option<u64>,
                _budget: std::time::Duration,
            ) -> bool {
                true
            }
            fn begin_moving_cycle(&self) {}
            fn try_begin_moving_cycle(&self) -> bool {
                true
            }
            fn end_moving_cycle(&self) {}
            fn outstanding_keepalive_addrs(&self) -> Vec<usize> {
                Vec::new()
            }
            fn outstanding_keepalive_addrs_for_vm(&self, _scope_vm: Option<u64>) -> Vec<usize> {
                Vec::new()
            }
            fn remap_keepalive(&self, _remap: &dyn Fn(usize) -> Option<usize>) {}
        }
        gpu_coordination::register_gpu_critical_bridge(Arc::new(CountingBridge));
        let before = FORCED.load(Ordering::Acquire);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let heap = VmHeap::new(GcBackend::Generational, 4 << 20);
            let _token = heap.enter_gpu_critical();
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(std::time::Duration::from_millis(500));
        });

        entered_rx.recv().unwrap();
        let cycle = gpu_coordination::before_collection();
        let after_cycle = FORCED.load(Ordering::Acquire);
        assert!(gpu_relocation_forbidden());

        let drain = cratonvm_gc::vm_heap::wait_for_gpu_critical_drain();
        let after_second_wait = FORCED.load(Ordering::Acquire);

        let _ = release_tx.send(());
        holder.join().unwrap();
        drop(drain);
        cycle.after_collection(&[]);
        assert!(!gpu_relocation_forbidden());

        assert_eq!(
            after_cycle - before,
            1,
            "before_collection timing out on direct GPU_CRITICAL_COUNT must count once, not twice"
        );
        assert_eq!(
            after_second_wait - after_cycle,
            0,
            "subsequent wait_for_gpu_critical_drain when relocation is already forbidden must not double-count"
        );
    }

    #[test]
    fn fallible_span_unpin_hook_refuses_decommit_and_stays_registered() {
        let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        gpu_coordination::run_heap_unpin_hooks();

        let granule = cratonvm_gc::reservation::GRANULE;
        let mut store = cratonvm_gc::reservation::HeapStore::new(2 * granule, "gpu_fallible_unpin");
        if matches!(store, cratonvm_gc::reservation::HeapStore::Owned(_)) {
            return;
        }
        assert!(store.commit_range(0, 2 * granule));
        let base = store.as_ptr() as usize;

        let pinned = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let stale_calls = Arc::new(AtomicUsize::new(0));
        let stale_calls_cb = Arc::clone(&stale_calls);
        // Re-registering a Fallible hook for the same (base, len) span must replace
        // the prior entry so LRU re-pinning does not accumulate duplicate hooks.
        gpu_coordination::register_fallible_span_unpin_hook(
            base,
            4096,
            Arc::new(move || {
                stale_calls_cb.fetch_add(1, Ordering::AcqRel);
                true
            }),
        );
        let pinned_cb = Arc::clone(&pinned);
        let calls_cb = Arc::clone(&calls);
        gpu_coordination::register_fallible_span_unpin_hook(
            base,
            4096,
            Arc::new(move || {
                calls_cb.fetch_add(1, Ordering::AcqRel);
                !pinned_cb.load(Ordering::Acquire)
            }),
        );

        // Active pin refuses decommit and reset, preserving committed memory and hook.
        assert_eq!(store.decommit_range(0, granule, "gpu_fallible_unpin"), 0);
        assert_eq!(store.reset_range(0, granule, "gpu_fallible_unpin"), (0, 0));
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert_eq!(stale_calls.load(Ordering::Acquire), 0);

        // Releasing pin allows subsequent decommit to succeed and consume the hook.
        pinned.store(false, Ordering::Release);
        assert!(store.decommit_range(0, granule, "gpu_fallible_unpin") >= granule);
        assert_eq!(calls.load(Ordering::Acquire), 3);

        // Hook is now consumed: further reset does not call it again.
        let _ = store.reset_range(0, granule, "gpu_fallible_unpin");
        assert_eq!(calls.load(Ordering::Acquire), 3);
    }
}

#[test]
fn arena_span_containing_validates_bounds_across_backends() {
    #[cfg(feature = "gpu-offload")]
    let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backends = [
        GcBackend::Generational,
        GcBackend::G1,
        #[cfg(feature = "zgc")]
        GcBackend::Zgc,
    ];

    let stack_local: u64 = 0x1234_5678;
    let stack_addr = &stack_local as *const u64 as usize;

    for backend in backends {
        let heap = VmHeap::new(backend, 16 << 20);
        let arr = heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 128)
            .expect("allocate test array");
        let obj_addr = arr.as_ptr() as usize;
        let data_addr = heap
            .array_data_ptr(arr)
            .expect("array payload pointer") as usize;

        let (base, len) = heap
            .arena_span_containing(obj_addr)
            .expect("object base address must resolve to its backing span");
        assert!(base != 0 && len > 0);
        assert!(obj_addr >= base && (obj_addr - base) < len);

        let payload_span = heap
            .arena_span_containing(data_addr)
            .expect("array data pointer must fall inside the same span");
        assert_eq!(payload_span, (base, len));

        assert_eq!(heap.arena_span_containing(0), None);
        assert_eq!(heap.arena_span_containing(base - 1), None);
        assert_eq!(heap.arena_span_containing(base + len), None);
        assert_eq!(heap.arena_span_containing(stack_addr), None);
        assert_eq!(heap.arena_span_containing(usize::MAX), None);

        #[cfg(feature = "zgc")]
        if heap.is_zgc() {
            let z = &heap;
            assert_eq!(z.zgc_committed_span_containing(obj_addr), Some((base, len)));
            assert!(z.zgc_committed_arena_spans().contains(&(base, len)));
            if z.zgc_os_committed_bytes() < z.committed_bytes() {
                assert!(
                    len < z.committed_bytes(),
                    "committed ZGC span must not cover uncommitted reserved VA"
                );
            }
        }
    }
}

struct ExternalPressureNoopMonitors;

impl cratonvm_gc::collector::MonitorCleanup for ExternalPressureNoopMonitors {
    fn remap_after_gc(&self, _map: &cratonvm_types::PointerMap) {}
}

fn verify_external_vram_pacing_and_hysteresis(backend: GcBackend) {
    #[cfg(feature = "gpu-offload")]
    let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: single-threaded integration test.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    let heap = VmHeap::new(backend, 64 * 1024 * 1024);
    heap.set_external_gpu_pressure_policy(
        32 * 1024 * 1024, // hard retained ceiling
        2 * 1024 * 1024,  // 2 MiB / wrapper ratio triggers external bias
        8 * 1024 * 1024,  // floor before per-object ratio triggers
    );

    assert!(
        !heap.needs_gc(),
        "empty heap without external GPU wrappers should not request GC"
    );

    // One small 1 MiB wrapper: below min_floor (8 MiB), no premature GC.
    assert!(!heap.note_external_gpu_retained(1024 * 1024, 1));
    assert_eq!(heap.external_gpu_retained_bytes(), 1024 * 1024);
    assert_eq!(heap.external_gpu_live_wrappers(), 1);
    assert!(!heap.needs_gc());

    // Add 10 MiB GpuFuture/GpuArray wrapper on a nearly empty Java heap:
    // total = 11 MiB across 2 wrappers -> 5.5 MiB/wrapper >= 2 MiB bias & >= 8 MiB floor.
    assert!(heap.note_external_gpu_retained(10 * 1024 * 1024, 1));
    assert!(
        heap.young_spill_pressure(),
        "external GPU pressure must arm native-boundary young_spill_pressure"
    );
    assert!(
        heap.needs_gc(),
        "external GPU pressure must cause {backend:?} needs_gc() to fire even at low Java heap rate"
    );

    // Collect while both Java wrappers remain live (no release happened).
    let mut roots = [];
    let monitors = ExternalPressureNoopMonitors;
    let _ = heap.collect_garbage(&stw, &mut roots, &monitors);

    // Post-cycle anti-storm hysteresis: surviving live wrappers must NOT keep triggering.
    assert!(
        !heap.needs_gc(),
        "{backend:?} must suppress GC storms when live GPU wrappers survive collection"
    );

    // Clear the native boundary latch and verify one more small 512 KiB allocation stays quiet.
    heap.clear_young_spill_pressure();
    assert!(!heap.note_external_gpu_retained(512 * 1024, 1));
    assert!(!heap.needs_gc());

    // Exceeding hysteresis delta re-arms pacing cleanly.
    assert!(heap.note_external_gpu_retained(6 * 1024 * 1024, 1));
    assert!(heap.needs_gc());

    // Releasing all wrappers drains accounting and re-arms clean baseline.
    let total_retained = heap.external_gpu_retained_bytes() as i64;
    let total_wrappers = heap.external_gpu_live_wrappers() as i64;
    assert!(!heap.note_external_gpu_retained(-total_retained, -total_wrappers));
    assert_eq!(heap.external_gpu_retained_bytes(), 0);
    assert_eq!(heap.external_gpu_live_wrappers(), 0);
    let _ = heap.collect_garbage(&stw, &mut roots, &monitors);
    assert!(!heap.needs_gc());

    // Re-allocating above floor after full release triggers again as expected.
    assert!(heap.note_external_gpu_retained(9 * 1024 * 1024, 1));
    assert!(heap.needs_gc());
}

#[test]
fn generational_external_gpu_vram_pressure_pacing_and_hysteresis() {
    verify_external_vram_pacing_and_hysteresis(GcBackend::Generational);
}

#[test]
fn g1_external_gpu_vram_pressure_pacing_and_hysteresis() {
    verify_external_vram_pacing_and_hysteresis(GcBackend::G1);
}

#[cfg(feature = "zgc")]
#[test]
fn zgc_external_gpu_vram_pressure_pacing_and_hysteresis() {
    verify_external_vram_pacing_and_hysteresis(GcBackend::Zgc);
}

#[cfg(feature = "gpu-offload")]
#[test]
fn per_vm_gpu_coord_isolates_relocation_veto_and_vram_pressure() {
    use cratonvm_gc::vm_heap::gpu_coordination;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    use std::time::Duration;

    struct VmScopedBridge {
        vetoed_vm: AtomicU64,
    }
    impl gpu_coordination::GpuCriticalBridge for VmScopedBridge {
        fn collector_wait_budget(&self) -> Duration {
            Duration::from_millis(5)
        }
        fn record_forced_non_moving_collection(&self) {}
        fn relocation_forbidden(&self) -> bool {
            self.vetoed_vm.load(Ordering::Acquire) != u64::MAX
        }
        fn relocation_forbidden_for_vm(&self, scope_vm: Option<u64>) -> bool {
            let vetoed = self.vetoed_vm.load(Ordering::Acquire);
            vetoed != u64::MAX && scope_vm.is_none_or(|id| id == vetoed)
        }
        fn outstanding(&self) -> usize {
            0
        }
        fn outstanding_for_vm(&self, _scope_vm: Option<u64>) -> usize {
            0
        }
        fn wait_for_relocation_clearance(&self, _budget: Duration) -> bool {
            true
        }
        fn wait_for_relocation_clearance_for_vm(
            &self,
            _scope_vm: Option<u64>,
            _budget: Duration,
        ) -> bool {
            true
        }
        fn begin_moving_cycle(&self) {}
        fn try_begin_moving_cycle(&self) -> bool {
            true
        }
        fn end_moving_cycle(&self) {}
        fn outstanding_keepalive_addrs(&self) -> Vec<usize> {
            Vec::new()
        }
        fn outstanding_keepalive_addrs_for_vm(&self, _scope_vm: Option<u64>) -> Vec<usize> {
            Vec::new()
        }
        fn remap_keepalive(&self, _remap: &dyn Fn(usize) -> Option<usize>) {}
    }

    let _lock = COORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let heap_a = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    let heap_b = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    let vm_a = 0xA001u64;
    let vm_b = 0xB002u64;
    heap_a.gpu_coord().set_owner_vm_id(vm_a);
    heap_b.gpu_coord().set_owner_vm_id(vm_b);

    // 1. Per-VM VRAM pressure mirrors are isolated between VmHeaps.
    heap_a.gpu_coord().set_vram_reservation_limit(1_000);
    heap_b.gpu_coord().set_vram_reservation_limit(10_000);
    let (used_a, _) = heap_a.gpu_coord().vram_mirror_atomics();
    let (used_b, _) = heap_b.gpu_coord().vram_mirror_atomics();
    used_a.store(900, Ordering::Release);
    used_b.store(900, Ordering::Release);
    assert!(heap_a.gpu_coord().is_vram_under_pressure());
    assert!(!heap_b.gpu_coord().is_vram_under_pressure());

    // 2. A Forbidden bridge scoped to VM A vetoes relocation on VM A only,
    // never forcing non-moving collection on VM B.
    let bridge = Arc::new(VmScopedBridge {
        vetoed_vm: AtomicU64::new(vm_a),
    });
    heap_a.gpu_coord().register_bridge(bridge.clone());
    heap_b.gpu_coord().register_bridge(bridge);

    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    let mut roots_a = [];
    let mut roots_b = [];
    let monitors = ExternalPressureNoopMonitors;

    assert!(heap_a.gpu_coord().relocation_forbidden());
    assert!(!heap_b.gpu_coord().relocation_forbidden());

    let _ = heap_a.collect_garbage(&stw, &mut roots_a, &monitors);
    let _ = heap_b.collect_garbage(&stw, &mut roots_b, &monitors);
    // After cycle ends, VM A still has bridge veto while VM B remains unvetoed.
    assert!(heap_a.gpu_coord().relocation_forbidden());
    assert!(!heap_b.gpu_coord().relocation_forbidden());
}
