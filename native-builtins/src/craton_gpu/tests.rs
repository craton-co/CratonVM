use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
// Brings `.get_field(...)` (and friends) into scope for direct calls
// on a concrete `MockNativeContext` in the scalar-boxing tests below
// (elsewhere in this file `ctx` only ever appears as `&mut dyn
// NativeContext`, which needs no import for method-call syntax).
use cratonvm_native_api::NativeContext;
// 2026-07-11: the transport enum `MockNativeContext::
// set_gpu_future_take_result` scripts, for the real-registry
// `futureGetResult` tests below.
use cratonvm_native_api::registry::GpuFutureResult;

fn synthetic_devices() -> Vec<DeviceInfo> {
    vec![
        (
            "NVIDIA GeForce RTX 2060".to_string(),
            7,
            5,
            6 * 1024 * 1024 * 1024,
        ),
        ("NVIDIA A100".to_string(), 8, 0, 40 * 1024 * 1024 * 1024),
    ]
}

// ── Pure ordinal-lookup + capability-packing logic ──────────────────

#[test]
fn device_at_in_range() {
    let d = synthetic_devices();
    assert_eq!(device_at(&d, 0).unwrap().0, "NVIDIA GeForce RTX 2060");
    assert_eq!(device_at(&d, 1).unwrap().0, "NVIDIA A100");
}

#[test]
fn device_at_out_of_range_is_none() {
    let d = synthetic_devices();
    // One past the end.
    assert!(device_at(&d, 2).is_none());
    // Java ints can be negative; must not panic or wrap into a valid
    // index.
    assert!(device_at(&d, -1).is_none());
    assert!(device_at(&d, i32::MIN).is_none());
}

#[test]
fn device_at_empty_list_is_none() {
    let empty: Vec<DeviceInfo> = Vec::new();
    assert!(device_at(&empty, 0).is_none());
    assert!(device_at(&empty, -5).is_none());
}

#[test]
fn compute_capability_packing() {
    // sm_75 → 75, sm_80 → 80, sm_90 → 90.
    assert_eq!(pack_compute_capability(7, 5), 75);
    assert_eq!(pack_compute_capability(8, 0), 80);
    assert_eq!(pack_compute_capability(9, 0), 90);
    // Two-digit minor (hypothetical) still packs deterministically.
    assert_eq!(pack_compute_capability(7, 2), 72);
}

#[test]
fn synthetic_device_fields_round_trip() {
    let d = synthetic_devices();
    let (name, major, minor, mem) = device_at(&d, 1).unwrap();
    assert_eq!(name, "NVIDIA A100");
    assert_eq!(pack_compute_capability(*major, *minor), 80);
    assert_eq!(*mem, 40 * 1024 * 1024 * 1024);
}

// ── Handler glue on the no-device fallback path ─────────────────────
//
// `MockNativeContext` uses the trait's default `gpu_device_info`,
// which returns an empty Vec — exactly the driverless-host /
// stub-`cuda-bridge` case this build degrades to. The handlers must
// report a truthful "no device" rather than fabricating one.

#[test]
fn device_count_no_device_is_zero() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_device_count(&mut ctx, &[]).unwrap();
    assert_eq!(r, Some(Value::Int(0)));
}

#[test]
fn device_name_no_device_is_null() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_device_name(&mut ctx, &[Value::Int(0)]).unwrap();
    assert_eq!(r, Some(Value::Object(None)));
}

#[test]
fn device_total_memory_no_device_is_zero() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_device_total_memory(&mut ctx, &[Value::Int(0)]).unwrap();
    assert_eq!(r, Some(Value::Long(0)));
}

#[test]
fn device_compute_capability_no_device_is_zero() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_device_compute_capability(&mut ctx, &[Value::Int(0)]).unwrap();
    assert_eq!(r, Some(Value::Int(0)));
}

#[test]
fn device_queries_negative_ordinal_no_device() {
    // Negative ordinal on the no-device path must still be a clean
    // null / 0, never a panic.
    let mut ctx = MockNativeContext::new();
    assert_eq!(
        builtin_device_name(&mut ctx, &[Value::Int(-1)]).unwrap(),
        Some(Value::Object(None))
    );
    assert_eq!(
        builtin_device_compute_capability(&mut ctx, &[Value::Int(-1)]).unwrap(),
        Some(Value::Int(0))
    );
}

// ── Future round-trip via the synthetic state store ─────────────────
//
// The failed-future path is fully handle-based and works end-to-end
// without a device, so it can be exercised directly.

#[test]
fn failed_future_reports_status_and_message() {
    let h = record_failed_future_with_message("test failure");
    // Status code 2 == FAILED in the spec's enum-ordinal layout.
    let mut ctx = MockNativeContext::new();
    let status = builtin_future_status(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(status, Some(Value::Int(2)));
    // Error message round-trips back as a (mock) String object.
    let msg = builtin_future_get_error_message(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert!(matches!(msg, Some(Value::Object(Some(_)))));
    // Result of a failed future is null.
    let res = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(res, Some(Value::Object(None)));
    // Releasing it removes it: status becomes UNKNOWN (3).
    builtin_release_future(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    let after = builtin_future_status(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(after, Some(Value::Int(3)));
}

#[test]
fn unknown_future_handle_is_status_unknown() {
    let mut ctx = MockNativeContext::new();
    // A handle that was never recorded.
    let status = builtin_future_status(&mut ctx, &[Value::Long(999_999)]).unwrap();
    assert_eq!(status, Some(Value::Int(3)));
}

// ── 2026-07-11: `futureIsDone` non-blocking probe ────────────────────
//
// `MockNativeContext` uses the trait's default `gpu_future_status`
// (returns `None`), so every case below exercises the local
// synthetic-registry fallback — the same fallback `futureStatus`
// already relies on for stub-only fixtures.

#[test]
fn future_is_done_unknown_handle_is_false() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_is_done(&mut ctx, &[Value::Long(424_242)]).unwrap();
    assert_eq!(r, Some(Value::Int(0)));
}

#[test]
fn future_is_done_pending_is_false() {
    let h = state::with(|s| {
        let h = s.fresh_handle();
        s.futures.insert(h, state::FutureState::Pending);
        h
    });
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_is_done(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(r, Some(Value::Int(0)));
}

#[test]
fn future_is_done_failed_is_true() {
    let h = record_failed_future_with_message("boom");
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_is_done(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(r, Some(Value::Int(1)));
}

#[test]
fn future_is_done_done_is_true() {
    let h = state::with(|s| {
        let h = s.fresh_handle();
        s.futures
            .insert(h, state::FutureState::Done { result_obj: None });
        h
    });
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_is_done(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(r, Some(Value::Int(1)));
}

#[test]
fn future_is_done_done_scalar_is_true() {
    let h = record_done_scalar(Value::Int(7));
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_is_done(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(r, Some(Value::Int(1)));
}

// ── 2026-07-11: `futureGetResult` scalar boxing ──────────────────────
//
// Stamps `FutureState::DoneScalar` directly into the local
// synthetic registry (there is no producer for it on the
// real-dispatch path yet — see the module doc's PHASE4-CUDA-TODO)
// and checks `builtin_future_get_result` boxes it via the crate's
// canonical `box_value` helper: the returned `Object` must not be
// the raw unboxed primitive, and field 0 of the boxed wrapper must
// round-trip the original value. (Not asserting on the wrapper's
// class name: `lang_math::alloc_wrapper`'s process-wide
// `WRAPPER_CIDS` cache is keyed by `ctx.vm_identity()`, which is
// `0` for every fresh `MockNativeContext` in this binary, so a
// fresh mock's own `class_names` map may not contain the class id
// a *different* test's mock resolved and cached first — a
// pre-existing cross-test-isolation quirk of the shared boxing
// helper, not something specific to this handler.)

/// Test-only helper: stamp a `DoneScalar` future directly into the
/// local synthetic registry, mirroring `record_failed_future_with_message`.
fn record_done_scalar(value: Value) -> u64 {
    state::with(|s| {
        let h = s.fresh_handle();
        s.futures
            .insert(h, state::FutureState::DoneScalar { value });
        h
    })
}

#[test]
fn scalar_future_get_result_boxes_int() {
    let h = record_done_scalar(Value::Int(42));
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Int(42));
        }
        other => panic!("expected a boxed Integer object, got {other:?}"),
    }
}

#[test]
fn scalar_future_get_result_boxes_long() {
    let h = record_done_scalar(Value::Long(123_456_789_012));
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Long(123_456_789_012));
        }
        other => panic!("expected a boxed Long object, got {other:?}"),
    }
}

#[test]
fn scalar_future_get_result_boxes_float() {
    let h = record_done_scalar(Value::Float(2.5));
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Float(2.5));
        }
        other => panic!("expected a boxed Float object, got {other:?}"),
    }
}

#[test]
fn scalar_future_get_result_boxes_double() {
    let h = record_done_scalar(Value::Double(3.140_000_1));
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Double(3.140_000_1));
        }
        other => panic!("expected a boxed Double object, got {other:?}"),
    }
}

#[test]
fn done_object_future_get_result_is_unaffected_by_scalar_boxing() {
    // Non-scalar `Done` futures (the array/object-result shape) must
    // keep returning `null` when `result_obj` is `None`, unchanged
    // from before this file's scalar-boxing addition.
    let h = state::with(|s| {
        let h = s.fresh_handle();
        s.futures
            .insert(h, state::FutureState::Done { result_obj: None });
        h
    });
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert_eq!(r, Some(Value::Object(None)));
}

// ── 2026-07-11: `futureGetResult` via the REAL submission registry ──
//
// `MockNativeContext::set_gpu_future_take_result` scripts the
// `NativeContext::gpu_future_take_result` escape hatch directly,
// standing in for a real GPU submission registry the same way
// `record_done_scalar` stands in for the local `FutureState::
// DoneScalar` fallback exercised above. `builtin_future_get_result`
// must consult this FIRST and only fall back to the synthetic map
// when it returns `None` (see the handler's doc comment).

#[test]
fn real_future_get_result_boxes_int() {
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_future_take_result(7, GpuFutureResult::ScalarI32(42));
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(7)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Int(42));
        }
        other => panic!("expected a boxed Integer object, got {other:?}"),
    }
}

#[test]
fn real_future_get_result_boxes_long() {
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_future_take_result(7, GpuFutureResult::ScalarI64(123_456_789_012));
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(7)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Long(123_456_789_012));
        }
        other => panic!("expected a boxed Long object, got {other:?}"),
    }
}

#[test]
fn real_future_get_result_boxes_float() {
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_future_take_result(7, GpuFutureResult::ScalarF32(2.5));
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(7)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Float(2.5));
        }
        other => panic!("expected a boxed Float object, got {other:?}"),
    }
}

#[test]
fn real_future_get_result_boxes_double() {
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_future_take_result(7, GpuFutureResult::ScalarF64(3.140_000_1));
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(7)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Double(3.140_000_1));
        }
        other => panic!("expected a boxed Double object, got {other:?}"),
    }
}

#[test]
fn real_future_get_result_void_is_null() {
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_future_take_result(7, GpuFutureResult::Void);
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(7)]).unwrap();
    assert_eq!(r, Some(Value::Object(None)));
}

#[test]
fn real_future_get_result_preferred_over_synthetic_map() {
    // Stamp BOTH a real-registry answer and a conflicting synthetic
    // `DoneScalar` for the same handle; the real-registry answer
    // must win, since `builtin_future_get_result` consults
    // `gpu_future_take_result` before the local map.
    let h = record_done_scalar(Value::Int(999));
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_future_take_result(h, GpuFutureResult::ScalarI32(42));
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Int(42));
        }
        other => panic!("expected the real-registry value (42), got {other:?}"),
    }
}

#[test]
fn future_get_result_falls_back_to_synthetic_map_when_no_real_answer() {
    // No `set_gpu_future_take_result` call for this handle — the
    // mock's override returns `None` (the trait default), so this
    // must fall through to the synthetic `DoneScalar` entry exactly
    // as the pre-2026-07-11 behavior did.
    let h = record_done_scalar(Value::Int(13));
    let mut ctx = MockNativeContext::new();
    let r = builtin_future_get_result(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    match r {
        Some(Value::Object(Some(obj))) => {
            assert_eq!(ctx.get_field(obj, 0), Value::Int(13));
        }
        other => panic!("expected the synthetic-map value (13), got {other:?}"),
    }
}

// ── GpuStream affinity — `resolve_or_create_default_stream` ─────────
//
// `MockNativeContext` uses the trait's default `gpu_stream_create`
// unless scripted via `set_gpu_stream_create_result`, mirroring
// every other GPU trait-method test pattern in this file.

/// `state::GPU_STATIC` is one process-wide store shared by every test in
/// this file (and this file's tests run in parallel by default).
/// Every test below that touches `executor_default_stream` or
/// `streams` MUST key off a freshly-minted handle from
/// `state::with(|s| s.fresh_handle())` rather than a hardcoded
/// literal — two tests hardcoding the same executor handle (e.g.
/// both using `1`) race on the same map entry and flake under
/// `cargo test`'s default parallelism.
fn fresh_test_handle() -> u64 {
    state::with(|s| s.fresh_handle())
}

#[test]
fn default_stream_no_device_returns_none_every_time() {
    // No override set -> `gpu_stream_create` always answers `None`
    // (the trait default, "no device"). Nothing should be cached
    // for a `None` answer, so a second call for the same executor
    // tries again rather than being stuck.
    let mut ctx = MockNativeContext::new();
    let exec = fresh_test_handle();
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec), None);
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec), None);
    assert_eq!(ctx.gpu_stream_create_call_count(), 2);
}

#[test]
fn default_stream_is_created_once_and_cached_per_executor() {
    // Script a real-looking answer; the SECOND call for the SAME
    // executor handle must reuse it without calling
    // `gpu_stream_create` again — this is the whole point of the
    // fix: repeated submits on one executor share one stream.
    let mut ctx = MockNativeContext::new();
    let exec = fresh_test_handle();
    ctx.set_gpu_stream_create_result(Some(777));
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec), Some(777));
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec), Some(777));
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec), Some(777));
    assert_eq!(
        ctx.gpu_stream_create_call_count(),
        1,
        "gpu_stream_create must be called exactly once per executor handle"
    );
}

#[test]
fn default_stream_is_independent_per_executor() {
    // Two different executor handles must not share a cache entry
    // — each gets its own default stream.
    let mut ctx = MockNativeContext::new();
    let exec_a = fresh_test_handle();
    let exec_b = fresh_test_handle();
    ctx.set_gpu_stream_create_result(Some(1));
    let a = resolve_or_create_default_stream(&mut ctx, exec_a);
    ctx.set_gpu_stream_create_result(Some(2));
    let b = resolve_or_create_default_stream(&mut ctx, exec_b);
    assert_eq!(a, Some(1));
    assert_eq!(b, Some(2));
    // Both cached: re-querying returns the same per-executor value
    // even though the mock's script has since moved on to `Some(2)`.
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec_a), Some(1));
    assert_eq!(ctx.gpu_stream_create_call_count(), 2);
}

#[test]
fn release_executor_releases_its_cached_default_stream() {
    // `builtin_release_executor` must forward the executor's
    // cached default-stream handle to `ctx.gpu_stream_release`
    // (not just drop it locally) — otherwise a real CUDA stream
    // leaks every time an app closes its executor without ever
    // calling `newStream`/`closeStream` explicitly.
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_stream_create_result(Some(555));
    let exec = state::with(|s| {
        let h = s.fresh_handle();
        s.executors.insert(h, 0);
        h
    });
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec), Some(555));

    builtin_release_executor(&mut ctx, &[Value::Long(exec as i64)]).unwrap();

    assert_eq!(ctx.gpu_stream_release_calls(), vec![555]);
    // The executor's cache entry is gone too, so a hypothetical
    // reuse of the same numeric handle after release would create
    // a fresh stream rather than resurrecting the released one.
    let still_cached = state::with(|s| s.executor_default_stream.get(&exec).copied());
    assert_eq!(still_cached, None);
}

#[test]
fn release_executor_without_a_default_stream_does_not_call_release() {
    // An executor that never submitted anything (no default stream
    // ever created) must not call `gpu_stream_release` at all —
    // there is nothing to release, and calling it with a bogus
    // handle would be misleading in a trace.
    let mut ctx = MockNativeContext::new();
    let exec = state::with(|s| {
        let h = s.fresh_handle();
        s.executors.insert(h, 0);
        h
    });

    builtin_release_executor(&mut ctx, &[Value::Long(exec as i64)]).unwrap();

    assert!(ctx.gpu_stream_release_calls().is_empty());
}

#[test]
fn close_stream_forwards_release_to_the_registry() {
    // `builtin_close_stream` must call `ctx.gpu_stream_release`
    // with the exact handle it was given, in addition to dropping
    // the local bookkeeping entry.
    let mut ctx = MockNativeContext::new();
    let handle: u64 = 4242;
    state::with(|s| {
        s.streams.insert(handle, 0);
    });

    builtin_close_stream(&mut ctx, &[Value::Long(handle as i64)]).unwrap();

    assert_eq!(ctx.gpu_stream_release_calls(), vec![handle]);
    assert!(state::with(|s| s.streams.get(&handle).is_none()));
}

/// The submission drain must stay wired, and only the shim can keep it
/// wired.
///
/// Two tables are keyed by the same handle: this crate's `state::futures`
/// and the offload runtime's `offload::SUBMISSIONS`. `builtin_release_future`
/// is the ONLY thing that touches both, so if the `ctx.gpu_release_submission`
/// call is ever dropped, the registry silently stops draining and every async
/// submission leaks its CUDA stream and event for the life of the process.
///
/// That is not hypothetical. It is how the defect `f6061f7b3` fixed was
/// born: `b6133b92d` moved the synchronous path onto `dispatch_method_sync`,
/// which never registers a submission and so correctly had nothing to
/// release — and that happened to delete the last caller of
/// `release_submission`. Nothing was watching, so it went unnoticed until a
/// test-only-public-API ratchet reported the orphaned function weeks later.
/// This is the thing that would have been watching.
///
/// Asserted at the SHIM boundary deliberately: the VM-side override reads
/// `CRATONVM_GPU_NO_SUBMISSION_DRAIN`, so asserting further in would make
/// this test a test of that flag. The contract here is "the shim forwards
/// the handle it was given", which is the half that was missing.
#[test]
fn release_future_forwards_the_drain_to_the_registry() {
    let mut ctx = MockNativeContext::new();
    let handle: u64 = 90125;
    state::with(|s| {
        s.futures.insert(handle, state::FutureState::Pending);
    });

    builtin_release_future(&mut ctx, &[Value::Long(handle as i64)]).unwrap();

    assert_eq!(
        ctx.gpu_release_submission_calls(),
        vec![handle],
        "builtin_release_future must forward the handle to the offload \
         registry's drain, not only drop this crate's own record of it"
    );
    assert!(
        state::with(|s| s.futures.get(&handle).is_none()),
        "the local bookkeeping entry must go too"
    );
}

#[test]
fn new_stream_wraps_the_real_handle_when_a_device_is_available() {
    // With `gpu_stream_create` scripted to succeed, the handle
    // `builtin_new_stream` records in local bookkeeping must be
    // the SAME real handle the registry minted — not a separately
    // counted local synthetic one — so `resolve_stream` (on the
    // `OffloadCache` side, not reachable from this mock) would
    // find the exact stream `newStream()` handed to Java.
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_stream_create_result(Some(9001));
    let exec = state::with(|s| {
        let h = s.fresh_handle();
        s.executors.insert(h, 0);
        h
    });
    let r = builtin_new_stream(&mut ctx, &[Value::Long(exec as i64)]);
    // `instantiate_handle_wrapper` calls `ctx.new_object` /
    // `ctx.invoke`, which `MockNativeContext` happily fabricates a
    // synthetic class + object for (it doesn't need a real
    // `GpuStreamImpl` on the classpath). What this test actually
    // checks is the bookkeeping side effect below, not the exact
    // shape of the wrapper's return value.
    assert!(r.is_ok());
    assert_eq!(
        state::with(|s| s.streams.get(&9001).copied()),
        Some(exec),
        "the real gpu_stream_create handle must be the one recorded, not a fresh local one"
    );
}

// ── 2026-07-11: `arrayAllocate*` — device-only allocation ───────────
//
// `GpuArray.allocate(exec, len)` has no host source array; the
// native shim mints a zero-filled host-bytes mirror instead of
// snapshotting a Java array (`wrap_primitive_array`'s job). These
// tests exercise allocation, the `arrayToHost` zero-fill round
// trip, the negative-length bad-arg convention, and
// `releaseArray`'s double-release idempotency.

#[test]
fn allocate_int_round_trips_zeros_via_to_host() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_array_allocate_int(&mut ctx, &[Value::Int(4)]).unwrap();
    let handle = match r {
        Some(Value::Long(h)) => h,
        other => panic!("expected a non-zero handle, got {other:?}"),
    };
    assert_ne!(handle, 0);
    assert_eq!(
        builtin_array_is_resident(&mut ctx, &[Value::Long(handle)]).unwrap(),
        Some(Value::Int(1)),
        "an allocated array is resident (host bytes tracked), same as arrayWrap*"
    );

    let host = builtin_array_to_host(&mut ctx, &[Value::Long(handle)]).unwrap();
    match host {
        Some(Value::Object(Some(arr))) => {
            assert_eq!(ctx.array_length(arr), 4);
            for i in 0..4 {
                assert_eq!(ctx.get_array_element(arr, i), Value::Int(0));
            }
        }
        other => panic!("expected a materialized int[] array, got {other:?}"),
    }
}

#[test]
fn allocate_long_round_trips_zeros_via_to_host() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_array_allocate_long(&mut ctx, &[Value::Int(3)]).unwrap();
    let handle = match r {
        Some(Value::Long(h)) => h,
        other => panic!("expected a non-zero handle, got {other:?}"),
    };
    let host = builtin_array_to_host(&mut ctx, &[Value::Long(handle)]).unwrap();
    match host {
        Some(Value::Object(Some(arr))) => {
            assert_eq!(ctx.array_length(arr), 3);
            for i in 0..3 {
                assert_eq!(ctx.get_array_element(arr, i), Value::Long(0));
            }
        }
        other => panic!("expected a materialized long[] array, got {other:?}"),
    }
}

#[test]
fn allocate_float_round_trips_zeros_via_to_host() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_array_allocate_float(&mut ctx, &[Value::Int(5)]).unwrap();
    let handle = match r {
        Some(Value::Long(h)) => h,
        other => panic!("expected a non-zero handle, got {other:?}"),
    };
    let host = builtin_array_to_host(&mut ctx, &[Value::Long(handle)]).unwrap();
    match host {
        Some(Value::Object(Some(arr))) => {
            assert_eq!(ctx.array_length(arr), 5);
            for i in 0..5 {
                assert_eq!(ctx.get_array_element(arr, i), Value::Float(0.0));
            }
        }
        other => panic!("expected a materialized float[] array, got {other:?}"),
    }
}

#[test]
fn allocate_double_round_trips_zeros_via_to_host() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_array_allocate_double(&mut ctx, &[Value::Int(2)]).unwrap();
    let handle = match r {
        Some(Value::Long(h)) => h,
        other => panic!("expected a non-zero handle, got {other:?}"),
    };
    let host = builtin_array_to_host(&mut ctx, &[Value::Long(handle)]).unwrap();
    match host {
        Some(Value::Object(Some(arr))) => {
            assert_eq!(ctx.array_length(arr), 2);
            for i in 0..2 {
                assert_eq!(ctx.get_array_element(arr, i), Value::Double(0.0));
            }
        }
        other => panic!("expected a materialized double[] array, got {other:?}"),
    }
}

#[test]
fn allocate_zero_length_is_a_valid_empty_array() {
    // `len == 0` is not an error — Java `new int[0]` is legal and
    // must round-trip as a real (empty) array, not the bad-arg
    // sentinel.
    let mut ctx = MockNativeContext::new();
    let r = builtin_array_allocate_int(&mut ctx, &[Value::Int(0)]).unwrap();
    let handle = match r {
        Some(Value::Long(h)) => h,
        other => panic!("expected a non-zero handle, got {other:?}"),
    };
    assert_ne!(handle, 0);
    let host = builtin_array_to_host(&mut ctx, &[Value::Long(handle)]).unwrap();
    match host {
        Some(Value::Object(Some(arr))) => assert_eq!(ctx.array_length(arr), 0),
        other => panic!("expected a materialized (empty) int[] array, got {other:?}"),
    }
}

#[test]
fn allocate_negative_length_is_the_bad_arg_sentinel() {
    // Mirrors `wrap_primitive_array`'s null-host-array convention:
    // a bad argument returns handle 0 without minting a state
    // entry, rather than throwing or `as usize`-wrapping into an
    // enormous allocation.
    let mut ctx = MockNativeContext::new();
    for len in [-1, -2, i32::MIN] {
        let r = builtin_array_allocate_int(&mut ctx, &[Value::Int(len)]).unwrap();
        assert_eq!(r, Some(Value::Long(0)), "length {len} must yield handle 0");
    }
    let r = builtin_array_allocate_long(&mut ctx, &[Value::Int(-5)]).unwrap();
    assert_eq!(r, Some(Value::Long(0)));
    let r = builtin_array_allocate_float(&mut ctx, &[Value::Int(-5)]).unwrap();
    assert_eq!(r, Some(Value::Long(0)));
    let r = builtin_array_allocate_double(&mut ctx, &[Value::Int(-5)]).unwrap();
    assert_eq!(r, Some(Value::Long(0)));
}

#[test]
fn allocate_release_then_double_release_is_idempotent() {
    let mut ctx = MockNativeContext::new();
    let r = builtin_array_allocate_int(&mut ctx, &[Value::Int(8)]).unwrap();
    let handle = match r {
        Some(Value::Long(h)) => h,
        other => panic!("expected a non-zero handle, got {other:?}"),
    };
    assert_eq!(
        builtin_array_is_resident(&mut ctx, &[Value::Long(handle)]).unwrap(),
        Some(Value::Int(1))
    );

    builtin_release_array(&mut ctx, &[Value::Long(handle)]).unwrap();
    assert_eq!(
        builtin_array_is_resident(&mut ctx, &[Value::Long(handle)]).unwrap(),
        Some(Value::Int(0)),
        "released handle reports not-resident, the same absent => false convention"
    );

    // A second release of the same (already-released) handle must
    // not panic and must leave the array absent, matching
    // `releaseFuture`'s remove-is-a-no-op-on-missing-key shape used
    // elsewhere in this file.
    //
    // (This used to cite `ResidencyTracker::release` in
    // `vm/src/runtime/gpu_residency.rs` as the convention being
    // matched. That module was dead code -- nothing outside its own
    // tests ever constructed one -- and was removed 2026-09-02. The
    // resident store in this file is the only implementation.)
    builtin_release_array(&mut ctx, &[Value::Long(handle)]).unwrap();
    assert_eq!(
        builtin_array_is_resident(&mut ctx, &[Value::Long(handle)]).unwrap(),
        Some(Value::Int(0))
    );

    // `arrayToHost` on a released handle returns null, same as an
    // always-unknown handle.
    let host = builtin_array_to_host(&mut ctx, &[Value::Long(handle)]).unwrap();
    assert_eq!(host, Some(Value::Object(None)));
}

#[test]
fn real_future_get_result_device_array_zero_handle_and_empty() {
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_future_take_result(
        7,
        GpuFutureResult::DeviceArray {
            array_handle: 0,
            elem: DeviceArrayElem::I32,
            len: 4,
        },
    );
    assert_eq!(
        builtin_future_get_result(&mut ctx, &[Value::Long(7)]).unwrap(),
        Some(Value::Object(None))
    );

    ctx.set_gpu_future_take_result(
        8,
        GpuFutureResult::DeviceArray {
            array_handle: 99,
            elem: DeviceArrayElem::F32,
            len: 0,
        },
    );
    match builtin_future_get_result(&mut ctx, &[Value::Long(8)]).unwrap() {
        Some(Value::Object(Some(arr))) => assert_eq!(ctx.array_length(arr), 0),
        other => panic!("expected empty primitive array, got {other:?}"),
    }
}

#[test]
fn real_future_get_result_device_array_materializes_user_array_and_preserves_residency() {
    let mut ctx = MockNativeContext::new();
    let arr_handle = match builtin_array_allocate_int(&mut ctx, &[Value::Int(3)]).unwrap() {
        Some(Value::Long(h)) if h != 0 => h as u64,
        other => panic!("expected array handle, got {other:?}"),
    };
    let bytes: Vec<u8> = [10i32, 20, 30]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    array_replace_bytes(arr_handle, bytes);

    let fut_handle = fresh_test_handle();
    ctx.set_gpu_future_take_result(
        fut_handle,
        GpuFutureResult::DeviceArray {
            array_handle: arr_handle,
            elem: DeviceArrayElem::I32,
            len: 3,
        },
    );

    match builtin_future_get_result(&mut ctx, &[Value::Long(fut_handle as i64)]).unwrap() {
        Some(Value::Object(Some(arr))) => {
            assert_eq!(ctx.array_length(arr), 3);
            assert_eq!(ctx.get_array_element(arr, 0), Value::Int(10));
            assert_eq!(ctx.get_array_element(arr, 1), Value::Int(20));
            assert_eq!(ctx.get_array_element(arr, 2), Value::Int(30));
        }
        other => panic!("expected int[] result, got {other:?}"),
    }
    assert_eq!(
        builtin_array_is_resident(&mut ctx, &[Value::Long(arr_handle as i64)]).unwrap(),
        Some(Value::Int(1)),
        "user-owned GpuArray handle must remain resident after DeviceArray .get()"
    );
    builtin_release_array(&mut ctx, &[Value::Long(arr_handle as i64)]).unwrap();
}

#[test]
fn future_then_apply_gpu_handles_invalid_inputs_and_unsupported_gracefully() {
    let mut ctx = MockNativeContext::new();
    let failed = match builtin_future_then_apply_gpu(
        &mut ctx,
        &[
            Value::Long(0),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap()
    {
        Some(Value::Long(h)) => h,
        other => panic!("expected long handle, got {other:?}"),
    };
    assert_ne!(failed, 0);
    assert_eq!(
        builtin_future_status(&mut ctx, &[Value::Long(failed)]).unwrap(),
        Some(Value::Int(2))
    );

    let cls = ctx.create_string("pkg/Kernel");
    let m = ctx.create_string("map");
    let desc = ctx.create_string("([I)[I");
    let unsupported = match builtin_future_then_apply_gpu(
        &mut ctx,
        &[
            Value::Long(123),
            Value::Object(Some(cls)),
            Value::Object(Some(m)),
            Value::Object(Some(desc)),
            Value::Object(None),
        ],
    )
    .unwrap()
    {
        Some(Value::Long(h)) => h,
        other => panic!("expected long handle, got {other:?}"),
    };
    assert_eq!(
        builtin_future_status(&mut ctx, &[Value::Long(unsupported)]).unwrap(),
        Some(Value::Int(2))
    );
}

#[test]
fn release_zero_handles_and_zero_exec_default_stream_are_safe() {
    let mut ctx = MockNativeContext::new();
    ctx.set_gpu_stream_create_result(Some(888));
    assert_eq!(resolve_or_create_default_stream(&mut ctx, 0), None);
    assert_eq!(ctx.gpu_stream_create_call_count(), 0);

    builtin_release_array(&mut ctx, &[Value::Long(0)]).unwrap();
    builtin_close_stream(&mut ctx, &[Value::Long(0)]).unwrap();
    builtin_release_executor(&mut ctx, &[Value::Long(0)]).unwrap();
    builtin_release_future(&mut ctx, &[Value::Long(0)]).unwrap();
    builtin_release_graph(&mut ctx, &[Value::Long(0)]).unwrap();
    assert!(ctx.gpu_stream_release_calls().is_empty());

    let exec = fresh_test_handle();
    assert_eq!(resolve_or_create_default_stream(&mut ctx, exec), Some(888));
    builtin_close_stream(&mut ctx, &[Value::Long(888)]).unwrap();
    assert_eq!(ctx.gpu_stream_release_calls(), vec![888]);
    assert_eq!(
        state::with(|s| s.executor_default_stream.get(&exec).copied()),
        None
    );
}

#[test]
fn ephemeral_device_array_result_survives_repeated_get_and_cleans_on_release_future() {
    let mut ctx = MockNativeContext::new();
    let fut_handle = fresh_test_handle();
    let ephem_arr = fresh_test_handle() | 0x8000_0000_0000_0000;
    let bytes: Vec<u8> = [7i32, 9].iter().flat_map(|v| v.to_le_bytes()).collect();
    state::with(|s| {
        s.ephemeral_results
            .insert(fut_handle, (ArrayElementType::Int, 2, bytes));
    });
    ctx.set_gpu_future_take_result(
        fut_handle,
        GpuFutureResult::DeviceArray {
            array_handle: ephem_arr,
            elem: DeviceArrayElem::I32,
            len: 2,
        },
    );

    for _ in 0..2 {
        match builtin_future_get_result(&mut ctx, &[Value::Long(fut_handle as i64)]).unwrap() {
            Some(Value::Object(Some(arr))) => {
                assert_eq!(ctx.array_length(arr), 2);
                assert_eq!(ctx.get_array_element(arr, 0), Value::Int(7));
                assert_eq!(ctx.get_array_element(arr, 1), Value::Int(9));
            }
            other => panic!("expected int[] result, got {other:?}"),
        }
    }

    builtin_release_future(&mut ctx, &[Value::Long(fut_handle as i64)]).unwrap();
    assert!(
        state::with(|s| !s.ephemeral_results.contains_key(&fut_handle)),
        "releaseFuture must drop cached ephemeral host bytes"
    );
    assert_eq!(ctx.gpu_release_submission_calls(), vec![fut_handle]);
}

#[test]
fn inflight_array_pins_defer_release_until_dispatch_finishes() {
    let mut ctx = MockNativeContext::new();
    let arr_handle = match builtin_array_allocate_int(&mut ctx, &[Value::Int(4)]).unwrap() {
        Some(Value::Long(h)) if h != 0 => h as u64,
        other => panic!("expected array handle, got {other:?}"),
    };

    let pins = InFlightArrayPins::pin(vec![arr_handle]);
    builtin_release_array(&mut ctx, &[Value::Long(arr_handle as i64)]).unwrap();
    assert!(
        array_snapshot(arr_handle).is_some(),
        "in-flight dispatch pin must keep array_snapshot alive across concurrent releaseArray"
    );
    assert_eq!(
        builtin_array_is_resident(&mut ctx, &[Value::Long(arr_handle as i64)]).unwrap(),
        Some(Value::Int(0))
    );

    pins.release(&mut ctx);
    assert!(
        array_snapshot(arr_handle).is_none(),
        "array must be evicted once in-flight dispatch pin releases"
    );
}

#[test]
fn stream_synchronize_handles_zero_synthetic_real_and_errors() {
    let mut ctx = MockNativeContext::new();
    assert_eq!(
        builtin_stream_synchronize(&mut ctx, &[Value::Long(0)]).unwrap(),
        None
    );

    let exec = fresh_test_handle();
    let syn = match builtin_new_stream(&mut ctx, &[Value::Long(exec as i64)]).unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected stream object, got {other:?}"),
    };
    let syn_handle = state::with(|s| {
        s.streams
            .iter()
            .find(|&(_, &e)| e == exec)
            .map(|(&h, _)| h)
            .expect("synthetic stream recorded")
    });
    assert_eq!(syn, syn);
    assert_eq!(
        builtin_stream_synchronize(&mut ctx, &[Value::Long(syn_handle as i64)]).unwrap(),
        None
    );
    assert!(
        ctx.gpu_stream_synchronize_calls().is_empty(),
        "synthetic no-device stream must not forward to NativeContext::gpu_stream_synchronize"
    );

    builtin_close_stream(&mut ctx, &[Value::Long(syn_handle as i64)]).unwrap();
    builtin_stream_synchronize(&mut ctx, &[Value::Long(syn_handle as i64)]).unwrap();
    assert_eq!(ctx.gpu_stream_synchronize_calls(), vec![syn_handle]);

    ctx.set_gpu_stream_synchronize_result(Err("unknown stream".to_string()));
    assert!(
        builtin_stream_synchronize(&mut ctx, &[Value::Long(42)]).is_err(),
        "driver/cache error from gpu_stream_synchronize must raise Java/VM error"
    );
}

#[test]
fn future_get_result_as_gpu_array_promotes_once_and_registers_resident_array() {
    let mut ctx = MockNativeContext::new();
    assert_eq!(
        builtin_future_get_result_as_gpu_array(&mut ctx, &[Value::Long(0)]).unwrap(),
        Some(Value::Object(None))
    );

    let fut_handle = fresh_test_handle();
    ctx.set_gpu_promote_ephemeral_array(fut_handle, (DeviceArrayElem::F32, 4));
    let obj = match builtin_future_get_result_as_gpu_array(
        &mut ctx,
        &[Value::Long(fut_handle as i64)],
    )
    .unwrap()
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected GpuArray object on first claim, got {other:?}"),
    };
    let _ = obj;

    let promoted = state::with(|s| {
        s.arrays
            .iter()
            .filter(|(_, e)| {
                e.element_type == ArrayElementType::Float && e.element_count == 4 && e.resident
            })
            .map(|(&h, _)| h)
            .max()
    });
    let new_handle = promoted.expect("promoted array must be registered in state.arrays");
    assert!(state::with(|s| s.arrays.contains_key(&new_handle)));

    assert_eq!(
        builtin_future_get_result_as_gpu_array(&mut ctx, &[Value::Long(fut_handle as i64)])
            .unwrap(),
        Some(Value::Object(None)),
        "second claim on same future must return null"
    );

    builtin_release_array(&mut ctx, &[Value::Long(new_handle as i64)]).unwrap();
    assert!(state::with(|s| !s.arrays.contains_key(&new_handle)));
}

#[test]
fn inflight_array_pins_drop_evicts_released_array_cache() {
    static EVICTED: std::sync::OnceLock<std::sync::Mutex<Vec<u64>>> =
        std::sync::OnceLock::new();
    fn spy_release(handle: u64) {
        EVICTED
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push(handle);
    }
    register_array_cache_release_hook(spy_release);

    let mut ctx = MockNativeContext::new();
    let h = match builtin_array_allocate_int(&mut ctx, &[Value::Int(4)]).unwrap() {
        Some(Value::Long(h)) if h > 0 => h as u64,
        other => panic!("expected positive array handle, got {other:?}"),
    };

    let pins = InFlightArrayPins::pin(vec![h]);
    builtin_release_array(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert!(
        state::with(|s| s.arrays.contains_key(&h)),
        "pinned array must stay alive until InFlightArrayPins drops"
    );
    drop(pins);

    assert!(
        state::with(|s| !s.arrays.contains_key(&h)),
        "dropping InFlightArrayPins after deferred release must remove array entry"
    );
    assert!(
        EVICTED
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .contains(&h),
        "dropping InFlightArrayPins must invoke array cache release hooks for released handles"
    );
}

#[test]
fn inflight_array_pins_drop_notifies_vram_release_hook() {
    static VRAM_EVENTS: std::sync::OnceLock<std::sync::Mutex<Vec<(usize, usize)>>> =
        std::sync::OnceLock::new();
    fn spy_vram(vm_id: usize, bytes: usize) {
        VRAM_EVENTS
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((vm_id, bytes));
    }
    register_array_cache_vram_release_hook(spy_vram);

    let mut ctx = MockNativeContext::new();
    let h = match builtin_array_allocate_int(&mut ctx, &[Value::Int(4)]).unwrap() {
        Some(Value::Long(h)) if h > 0 => h as u64,
        other => panic!("expected positive array handle, got {other:?}"),
    };

    let pins = InFlightArrayPins::pin(vec![h]);
    builtin_release_array(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    drop(pins);

    assert!(VRAM_EVENTS
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .contains(&(ctx.vm_identity(), 16)));
}

#[test]
fn allocate_primitive_array_keeps_host_bytes_lazy_until_materialized() {
    let mut ctx = MockNativeContext::new();
    let h = match builtin_array_allocate_int(&mut ctx, &[Value::Int(4)]).unwrap() {
        Some(Value::Long(h)) if h > 0 => h as u64,
        other => panic!("expected positive array handle, got {other:?}"),
    };
    assert!(
        state::with(|s| s.arrays.get(&h).is_some_and(|e| e.bytes.is_none())),
        "allocated GpuArray must not eagerly allocate a zeroed host Vec<u8>"
    );
    let (_, count, snap) = array_snapshot(h).expect("snapshot available");
    assert_eq!(count, 4);
    assert_eq!(snap, vec![0u8; 16]);
    let _ = builtin_array_to_host(&mut ctx, &[Value::Long(h as i64)]).unwrap();
    assert!(
        state::with(|s| s.arrays.get(&h).is_some_and(|e| e.bytes.is_none())),
        "reading unmaterialized array host view must keep backing bytes lazy"
    );
    array_replace_bytes(h, vec![1u8; 16]);
    assert!(state::with(|s| s.arrays.get(&h).is_some_and(|e| e.bytes.is_some())));
    builtin_release_array(&mut ctx, &[Value::Long(h as i64)]).unwrap();

    let fut = fresh_test_handle();
    ctx.set_gpu_promote_ephemeral_array(fut, (DeviceArrayElem::F32, 13));
    let _ = builtin_future_get_result_as_gpu_array(&mut ctx, &[Value::Long(fut as i64)]).unwrap();
    let promoted = state::with(|s| {
        s.arrays
            .iter()
            .filter(|(_, e)| e.element_type == ArrayElementType::Float && e.element_count == 13)
            .map(|(&id, _)| id)
            .max()
    })
    .expect("promoted array handle");
    assert!(
        state::with(|s| s.arrays.get(&promoted).is_some_and(|e| e.bytes.is_none())),
        "promoted device-resident GpuArray must keep host bytes lazy"
    );
    builtin_release_array(&mut ctx, &[Value::Long(promoted as i64)]).unwrap();
}

#[test]
fn clear_vm_state_removes_only_target_vm_native_resources() {
    let mut ctx_a = MockNativeContext::new();
    ctx_a.set_vm_identity(9101);
    let mut ctx_b = MockNativeContext::new();
    ctx_b.set_vm_identity(9102);

    let exec_a = builtin_open_executor(&mut ctx_a, &[Value::Int(0)]).unwrap();
    assert!(matches!(exec_a, Some(Value::Object(Some(_)))));
    let exec_a_h = state::with(|s| {
        s.handle_vms
            .iter()
            .filter_map(|(&h, &vm)| (vm == 9101 && s.executors.contains_key(&h)).then_some(h))
            .max()
            .expect("executor A registered")
    });
    let exec_b = builtin_open_executor(&mut ctx_b, &[Value::Int(0)]).unwrap();
    assert!(matches!(exec_b, Some(Value::Object(Some(_)))));
    let exec_b_h = state::with(|s| {
        s.handle_vms
            .iter()
            .filter_map(|(&h, &vm)| (vm == 9102 && s.executors.contains_key(&h)).then_some(h))
            .max()
            .expect("executor B registered")
    });
    let stream_a = builtin_new_stream(&mut ctx_a, &[Value::Long(exec_a_h as i64)]).unwrap();
    assert!(matches!(stream_a, Some(Value::Object(Some(_)))));
    let stream_a_h = state::with(|s| {
        s.handle_vms
            .iter()
            .filter_map(|(&h, &vm)| (vm == 9101 && s.streams.contains_key(&h)).then_some(h))
            .max()
            .expect("stream A registered")
    });
    let stream_b = builtin_new_stream(&mut ctx_b, &[Value::Long(exec_b_h as i64)]).unwrap();
    assert!(matches!(stream_b, Some(Value::Object(Some(_)))));
    let stream_b_h = state::with(|s| {
        s.handle_vms
            .iter()
            .filter_map(|(&h, &vm)| (vm == 9102 && s.streams.contains_key(&h)).then_some(h))
            .max()
            .expect("stream B registered")
    });
    let arr_a_h = match builtin_array_allocate_int(&mut ctx_a, &[Value::Int(2)]).unwrap() {
        Some(Value::Long(h)) => h as u64,
        other => panic!("expected array A, got {other:?}"),
    };
    let arr_b_h = match builtin_array_allocate_int(&mut ctx_b, &[Value::Int(2)]).unwrap() {
        Some(Value::Long(h)) => h as u64,
        other => panic!("expected array B, got {other:?}"),
    };

    clear_vm_state(9101);

    state::with(|s| {
        assert!(!s.executors.contains_key(&exec_a_h));
        assert!(!s.streams.contains_key(&stream_a_h));
        assert!(!s.arrays.contains_key(&arr_a_h));
        assert!(!s.handle_vms.contains_key(&exec_a_h));
        assert!(!s.handle_vms.contains_key(&stream_a_h));
        assert!(!s.handle_vms.contains_key(&arr_a_h));

        assert!(s.executors.contains_key(&exec_b_h));
        assert!(s.streams.contains_key(&stream_b_h));
        assert!(s.arrays.contains_key(&arr_b_h));
    });

    clear_vm_state(9102);
}

#[test]
fn promote_ephemeral_array_length_overflow_clears_future_vram() {
    let mut ctx = MockNativeContext::new();
    let fut = fresh_test_handle();
    state::with(|s| {
        s.future_vram.insert(fut, 4096);
        s.ephemeral_results
            .insert(fut, (ArrayElementType::Float, 16, vec![0u8; 64]));
    });
    ctx.set_gpu_promote_ephemeral_array(fut, (DeviceArrayElem::F32, (i32::MAX as usize) + 1));
    let res = builtin_future_get_result_as_gpu_array(&mut ctx, &[Value::Long(fut as i64)]).unwrap();
    assert!(matches!(res, Some(Value::Object(None))));
    state::with(|s| {
        assert_eq!(s.future_vram.get(&fut), Some(&0));
        assert!(!s.ephemeral_results.contains_key(&fut));
        s.future_vram.remove(&fut);
    });
}
