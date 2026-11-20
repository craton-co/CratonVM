use super::*;

#[test]
fn device_array_serialized_result_len_zero_and_missing_handle() {
    let empty = SerializedResult::DeviceArray {
        array_handle: 0,
        elem: cratonvm_native_api::registry::DeviceArrayElem::I32,
        len: 0,
    };
    assert_eq!(empty.materialize_array_bytes(), Some(Vec::new()));

    let missing = SerializedResult::DeviceArray {
        array_handle: 0x8000_0000_0099_9999,
        elem: cratonvm_native_api::registry::DeviceArrayElem::F32,
        len: 16,
    };
    assert_eq!(missing.materialize_array_bytes(), None);
}

#[test]
fn release_ephemeral_leaves_user_handles_and_unregistered_handles_untouched() {
    // Unregistered/ephemeral zero handles are clean no-ops.
    assert!(!device_cache::release_ephemeral(0));
    assert!(!device_cache::is_ephemeral_handle(0));
    assert!(device_cache::is_ephemeral_handle(0x8000_0000_0000_0001));
    release_ephemeral_array_after_enqueue(0x8000_0000_0099_9999);
}

#[test]
fn offload_cache_registry_drop_unregisters_vm_pressure_collector() {
    let config = crate::config::VmConfig::new();
    let vm = crate::vm::Vm::new(config);
    let vm_id = vm.shared.vm_identity as u64;
    assert_ne!(vm_id, u64::MAX);
    register_vm_offload_hooks(&vm.shared);
    assert_eq!(
        vm.shared
            .offload_registry
            .registered_vm_identity
            .load(std::sync::atomic::Ordering::Acquire),
        vm_id
    );
    drop(vm);
    // Dropping the final `Vm` tears down `OffloadCacheRegistry` and
    // unregisters `vm_id`, so invoking pressure collection is safe.
    let _ = cratonvm_gc::vm_heap::gpu_coordination::collect_garbage_for_gpu_pressure();
    assert_eq!(
        crate::runtime::offload_jit_gate::vm_entry_count(vm_id as usize),
        0
    );
}

#[test]
fn offload_cache_registry_drop_evicts_jit_gate_cache_without_hooks() {
    let mut config = crate::config::VmConfig::new();
    config.gpu_offload_enabled = true;
    let shared = std::sync::Arc::new(crate::vm::SharedVm::new(config));
    let vm_id = shared.vm_identity;
    // Even with no `self_arc` and no `ensure_vm_hooks` call, querying the
    // JIT gate records `vm_identity` and populates `GATE_CACHE`.
    let _ = crate::runtime::offload_jit_gate::caller_blocks_jit(
        &shared,
        cratonvm_types::ClassId::new(1),
        0,
    );
    assert!(crate::runtime::offload_jit_gate::vm_entry_count(vm_id) >= 1);
    drop(shared);
    assert_eq!(crate::runtime::offload_jit_gate::vm_entry_count(vm_id), 0);
}

#[test]
fn chain_submission_fanout_and_ephemeral_submission_ownership() {
    let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::new()));
    let pred_handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pred_sub = std::sync::Arc::new(StreamSubmission {
        handle: pred_handle,
        stream: None,
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Completed {
            result: SerializedResult::ScalarI32(42),
        }),
        finalize: parking_lot::Mutex::new(None),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(pred_sub);

    // Two fan-out `chain_submission_from_native` calls from the same
    // predecessor submission both observe the live submission (here failing
    // cleanly at `stream: None` -> "unsupported") without consuming it.
    let err1 = chain_submission_from_native(&shared, pred_handle, "NoClass", "k", "(I)I", &[])
        .expect_err("no stream must fail cleanly");
    let err2 = chain_submission_from_native(&shared, pred_handle, "NoClass", "k", "(I)I", &[])
        .expect_err("second fan-out call must also observe live predecessor");
    assert_eq!(err1, "unsupported");
    assert_eq!(err2, "unsupported");
    release_submission(pred_handle);
    assert!(lookup_submission(pred_handle).is_none());

    // Full `device_cache` + `StreamSubmission::drop` + pin atomicity check
    // when a CUDA context is available on this host.
    let Ok(ctx) = cuda_bridge::DeviceContext::new(0) else {
        return;
    };
    let buf = std::sync::Arc::new(
        cuda_bridge::DeviceBuffer::<i32>::zeros(&ctx, 16).expect("alloc ephemeral buf"),
    );
    let arr_handle = device_cache::insert_ephemeral(
        device_cache::CachedBuffer::I32(buf.clone()),
        shared.vm_identity,
    );
    let sub_handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sub = std::sync::Arc::new(StreamSubmission {
        handle: sub_handle,
        stream: None,
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Completed {
            result: SerializedResult::DeviceArray {
                array_handle: arr_handle,
                elem: cratonvm_native_api::registry::DeviceArrayElem::I32,
                len: 16,
            },
        }),
        finalize: parking_lot::Mutex::new(None),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(arr_handle),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(sub);

    // `future.get()` calls `release_ephemeral_array_after_enqueue` — must
    // keep the submission-owned entry alive for subsequent fan-out calls.
    release_ephemeral_array_after_enqueue(arr_handle);
    assert!(device_cache::get_typed::<i32>(arr_handle).is_some());

    // Two fan-out `thenApplyGpu` calls atomically acquire pins and build args.
    let (_, len1, buf1) =
        device_cache::get_entry_meta_and_pin(arr_handle).expect("first fan-out pin");
    let (_, len2, buf2) =
        device_cache::get_entry_meta_and_pin(arr_handle).expect("second fan-out pin");
    assert!(marshal_from_cached_entry(arr_handle, buf1, len1, false).is_ok());
    assert!(marshal_from_cached_entry(arr_handle, buf2, len2, false).is_ok());

    // Releasing the two enqueue pins leaves the entry alive until the
    // owning `StreamSubmission` itself is released.
    release_ephemeral_array_after_enqueue(arr_handle);
    release_ephemeral_array_after_enqueue(arr_handle);
    assert!(device_cache::get_typed::<i32>(arr_handle).is_some());

    // Simulate a third chain holding an active pin when `StreamCleaner`
    // releases the predecessor submission concurrently.
    let (_, _, buf3) =
        device_cache::get_entry_meta_and_pin(arr_handle).expect("third fan-out pin");
    drop(buf3);
    release_submission(sub_handle);
    assert!(
        device_cache::get_typed::<i32>(arr_handle).is_some(),
        "active consumer pin must keep entry alive past StreamSubmission::drop"
    );
    release_ephemeral_array_after_enqueue(arr_handle);
    assert!(
        device_cache::get_typed::<i32>(arr_handle).is_none(),
        "final pin release must evict unowned ephemeral entry"
    );
}

#[test]
fn promote_submission_device_array_single_claim_zero_and_cuda_buffer() {
    let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::new()));
    let empty_handle =
        NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let empty_sub = std::sync::Arc::new(StreamSubmission {
        handle: empty_handle,
        stream: None,
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Completed {
            result: SerializedResult::DeviceArray {
                array_handle: 0,
                elem: cratonvm_native_api::registry::DeviceArrayElem::I32,
                len: 0,
            },
        }),
        finalize: parking_lot::Mutex::new(None),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(empty_sub);

    assert_eq!(
        promote_submission_device_array(&shared, empty_handle, 7001),
        Some((cratonvm_native_api::registry::DeviceArrayElem::I32, 0))
    );
    assert_eq!(
        promote_submission_device_array(&shared, empty_handle, 7002),
        None,
        "second getAsGpuArray claim must be rejected once promoted"
    );
    release_submission(empty_handle);

    let Ok(ctx) = cuda_bridge::DeviceContext::new(0) else {
        return;
    };
    let buf = std::sync::Arc::new(
        cuda_bridge::DeviceBuffer::<i32>::zeros(&ctx, 8).expect("alloc ephemeral buf"),
    );
    let ephem = device_cache::insert_ephemeral(
        device_cache::CachedBuffer::I32(buf),
        shared.vm_identity,
    );
    let sub_handle =
        NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sub = std::sync::Arc::new(StreamSubmission {
        handle: sub_handle,
        stream: None,
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Completed {
            result: SerializedResult::DeviceArray {
                array_handle: ephem,
                elem: cratonvm_native_api::registry::DeviceArrayElem::I32,
                len: 8,
            },
        }),
        finalize: parking_lot::Mutex::new(None),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(ephem),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(sub);

    let resident_handle = 8001;
    assert_eq!(
        promote_submission_device_array(&shared, sub_handle, resident_handle),
        Some((cratonvm_native_api::registry::DeviceArrayElem::I32, 8))
    );
    assert!(device_cache::get_typed::<i32>(ephem).is_none());
    assert!(device_cache::get_typed::<i32>(resident_handle).is_some());
    assert!(device_cache::download_into_bytes_if_dirty(resident_handle).is_some());
    assert_eq!(
        promote_submission_device_array(&shared, sub_handle, 8002),
        None
    );
    device_cache::release(resident_handle);
    release_submission(sub_handle);
}

#[test]
fn trigger_vm_pressure_gc_waits_synchronously_when_foreign_attached() {
    let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::new()));
    let weak_vm = std::sync::Arc::downgrade(&shared);
    let shutdown = std::sync::atomic::AtomicBool::new(false);
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    crate::native::jni::attach_foreign_thread_idle_for_test(
        shared.clone(),
        "GpuPressureTestForeign",
    );
    struct DetachGuard;
    impl Drop for DetachGuard {
        fn drop(&mut self) {
            crate::native::jni::detach_foreign_thread_idle_for_test();
        }
    }
    let _detach = DetachGuard;

    // 1. Foreign-attached idle (`in_blocked_region == true`, `!can_join`):
    // must join GpuPressureGc so collection_count() increments BEFORE return.
    let before_idle = shared.mem.heap.collection_count();
    assert!(trigger_vm_pressure_gc(&weak_vm, &shutdown, &in_flight));
    assert!(
        shared.mem.heap.collection_count() > before_idle,
        "idle foreign-attached thread must observe completed GC before trigger_vm_pressure_gc returns"
    );
    assert!(!in_flight.load(std::sync::atomic::Ordering::Acquire));

    // 2. Foreign-attached running (`in_blocked_region == false`, inside ForeignCallGuard):
    // must enter foreign idle safepoint cleanly so STW does not deadlock and GC completes
    // before returning.
    crate::native::jni::run_in_foreign_call_guard_for_test(|| {
        let before_running = shared.mem.heap.collection_count();
        assert!(trigger_vm_pressure_gc(&weak_vm, &shutdown, &in_flight));
        assert!(
            shared.mem.heap.collection_count() > before_running,
            "running foreign-attached thread must observe completed GC before trigger_vm_pressure_gc returns"
        );
    });
}

#[test]
fn trigger_vm_pressure_gc_waits_synchronously_for_main_mutator() {
    let vm = crate::vm::Vm::new(crate::config::VmConfig::new());
    let shared = vm.shared.clone();
    assert_eq!(
        shared.threads.thread_registry.current_os_tid_claimed(),
        Some(true)
    );
    let weak_vm = std::sync::Arc::downgrade(&shared);
    let shutdown = std::sync::atomic::AtomicBool::new(false);
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let before = shared.mem.heap.collection_count();
    assert!(trigger_vm_pressure_gc(&weak_vm, &shutdown, &in_flight));
    assert!(
        shared.mem.heap.collection_count() > before,
        "claimed mutator thread must complete pressure GC synchronously before return"
    );
    assert!(!in_flight.load(std::sync::atomic::Ordering::Acquire));
}
