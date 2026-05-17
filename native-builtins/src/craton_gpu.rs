//! Native shims for `craton.gpu.internal.Native` (Phase 3 — Item P3-4).
//!
//! These are the Rust-side handlers behind every `Native.*` method the
//! Phase-3 Java surface declares (see `docs/gpu/phase3-spec.md` §2.2).
//!
//! The whole module is gated behind the `gpu-offload` Cargo feature. On
//! a default build it compiles down to an empty `register()` that does
//! nothing, so the CPU path is byte-identical to before the GPU work
//! began.
//!
//! ## Design notes for the stub-friendly Phase-3 implementation
//!
//! We don't have a CUDA device on the dev box. Every handler is therefore
//! a **synthetic stand-in**: it stores its state in a process-wide
//! `OnceLock<Mutex<NativeState>>` (counters + maps for executor / future /
//! array handles) and returns either a synthetic handle, a synthetic
//! `GpuFuture` that immediately fails with `"no CUDA device"`, or a
//! plausible value drawn from the in-memory state.
//!
//! The aim is for the Java side to be able to call
//! `Native.openExecutor` → `submit` → `futureGetResult` and have it
//! round-trip via a synthetic future whose `futureGetErrorMessage`
//! returns `"no CUDA device"`.
//!
//! ## Status (Phase 3.5)
//!
//! All five handlers that previously carried a `PHASE3-GUESS` marker
//! now instantiate real Java impl objects via the
//! `instantiate_handle_wrapper` helper:
//!
//!   * `builtin_open_executor`  → `craton/gpu/internal/GpuExecutorImpl`
//!   * `builtin_submit`         → `craton/gpu/internal/GpuFutureImpl`
//!   * `builtin_launch`         → `craton/gpu/internal/GpuFutureImpl`
//!   * `builtin_new_stream`     → `craton/gpu/internal/GpuStreamImpl`
//!   * `builtin_future_get_result` returns the stored `ObjectRef` from
//!     `FutureState::Done` (today every synthetic future is `Failed`,
//!     so the call returns `null` until real CUDA work lands).
//!
//! The remaining `PHASE4-CUDA-TODO` is the underlying cuda backend
//! port — when that finishes, the futures stop being unconditionally
//! `Failed` and the `Done` path becomes hot.

#![cfg_attr(not(feature = "gpu-offload"), allow(dead_code))]

use rustjvm_native_api::NativeMethodRegistry;

#[cfg(feature = "gpu-offload")]
use std::collections::HashMap;
#[cfg(feature = "gpu-offload")]
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "gpu-offload")]
use rustjvm_types::{ArrayElementType, Value};

// ---------------------------------------------------------------------------
// Public entry point: stub when feature is off, real registration when on
// ---------------------------------------------------------------------------

/// Register every `craton/gpu/internal/Native` native shim.
///
/// Called once from `lib.rs::register_essential_natives`. On a default
/// (no-feature) build this is a no-op.
#[cfg(feature = "gpu-offload")]
pub(crate) fn register(registry: &mut NativeMethodRegistry) {
    const KLASS: &str = "craton/gpu/internal/Native";

    registry.register(KLASS, "openExecutor", "(I)Lcraton/gpu/GpuExecutor;", builtin_open_executor);
    registry.register(KLASS, "submit",       "(JLcraton/gpu/GpuCallable;)Lcraton/gpu/GpuFuture;", builtin_submit);
    registry.register(KLASS, "launch",       "(JLcraton/gpu/GpuRunnable;)Lcraton/gpu/GpuFuture;", builtin_launch);
    registry.register(
        KLASS,
        "submitMethod",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_method,
    );
    registry.register(KLASS, "newStream",    "(J)Lcraton/gpu/GpuStream;", builtin_new_stream);
    registry.register(KLASS, "closeStream",  "(J)V", builtin_close_stream);

    registry.register(KLASS, "futureStatus",            "(J)I", builtin_future_status);
    registry.register(KLASS, "futureSynchronize",       "(J)V", builtin_future_synchronize);
    registry.register(KLASS, "futureGetResult",         "(J)Ljava/lang/Object;", builtin_future_get_result);
    registry.register(KLASS, "futureGetErrorMessage",   "(J)Ljava/lang/String;", builtin_future_get_error_message);

    registry.register(KLASS, "arrayWrapInt",    "([I)J", builtin_array_wrap_int);
    registry.register(KLASS, "arrayWrapLong",   "([J)J", builtin_array_wrap_long);
    registry.register(KLASS, "arrayWrapFloat",  "([F)J", builtin_array_wrap_float);
    registry.register(KLASS, "arrayWrapDouble", "([D)J", builtin_array_wrap_double);
    registry.register(KLASS, "arrayToHost",     "(J)Ljava/lang/Object;", builtin_array_to_host);
    registry.register(KLASS, "arrayIsResident", "(J)Z", builtin_array_is_resident);

    registry.register(KLASS, "releaseFuture",   "(J)V", builtin_release_future);
    registry.register(KLASS, "releaseArray",    "(J)V", builtin_release_array);
    registry.register(KLASS, "releaseExecutor", "(J)V", builtin_release_executor);
}

/// No-op registration when the `gpu-offload` feature is disabled.
#[cfg(not(feature = "gpu-offload"))]
pub(crate) fn register(_registry: &mut NativeMethodRegistry) {}

// ---------------------------------------------------------------------------
// Process-wide synthetic state (gpu-offload only)
// ---------------------------------------------------------------------------

#[cfg(feature = "gpu-offload")]
mod state {
    use super::*;

    /// Synthetic host-side storage for one `arrayWrap*` upload.
    ///
    /// Holds the element bytes as a `Vec<u8>` plus an `ArrayElementType`
    /// tag so `arrayToHost` can rebuild a fresh Java primitive array of
    /// the right shape. When the real `ResidencyTracker` (Item P3-7)
    /// lands this struct is replaced by a tracker-handle field.
    #[derive(Debug)]
    pub(super) struct ArrayEntry {
        pub element_type: ArrayElementType,
        pub element_count: usize,
        pub bytes: Vec<u8>,
        pub resident: bool,
    }

    /// Synthetic future state. Phase-3 stub-mode always lands in `Failed`
    /// because there is no real device behind `submit` / `launch`.
    #[derive(Debug)]
    pub(super) enum FutureState {
        Pending,
        Done { result_obj: Option<rustjvm_types::ObjectRef> },
        Failed { message: String },
    }

    #[derive(Debug, Default)]
    pub(super) struct NativeState {
        pub next_handle: u64,
        pub executors: HashMap<u64, i32>, // handle -> device ordinal
        pub futures: HashMap<u64, FutureState>,
        pub arrays: HashMap<u64, ArrayEntry>,
        pub streams: HashMap<u64, u64>, // stream handle -> owning exec handle
    }

    impl NativeState {
        pub fn fresh_handle(&mut self) -> u64 {
            self.next_handle += 1;
            self.next_handle
        }
    }

    pub(super) static STATE: OnceLock<Mutex<NativeState>> = OnceLock::new();

    pub(super) fn with<R>(f: impl FnOnce(&mut NativeState) -> R) -> R {
        let m = STATE.get_or_init(|| Mutex::new(NativeState::default()));
        let mut guard = m.lock().expect("craton_gpu state mutex poisoned");
        f(&mut guard)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "gpu-offload")]
const STUB_FAILURE_MESSAGE: &str = "no CUDA device";

#[cfg(feature = "gpu-offload")]
fn arg_long(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

#[cfg(feature = "gpu-offload")]
fn arg_int(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

#[cfg(feature = "gpu-offload")]
fn arg_object(args: &[Value], idx: usize) -> Option<rustjvm_types::ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(o)) => *o,
        _ => None,
    }
}

/// Materialize a fresh primitive Java array from a host byte buffer.
///
/// `element_type` selects the array kind. The buffer is interpreted as a
/// flat little-endian sequence (native order on x86/x64; matches the
/// `arrayWrap*` upload path below).
#[cfg(feature = "gpu-offload")]
fn rebuild_java_array(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    element_type: ArrayElementType,
    element_count: usize,
    bytes: &[u8],
) -> rustjvm_types::ObjectRef {
    let obj = ctx.new_array(element_type, element_count);
    match element_type {
        ArrayElementType::Int => {
            for i in 0..element_count {
                let off = i * 4;
                if off + 4 > bytes.len() { break; }
                let v = i32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Int(v));
            }
        }
        ArrayElementType::Long => {
            for i in 0..element_count {
                let off = i * 8;
                if off + 8 > bytes.len() { break; }
                let v = i64::from_ne_bytes(bytes[off..off + 8].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Long(v));
            }
        }
        ArrayElementType::Float => {
            for i in 0..element_count {
                let off = i * 4;
                if off + 4 > bytes.len() { break; }
                let v = f32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Float(v));
            }
        }
        ArrayElementType::Double => {
            for i in 0..element_count {
                let off = i * 8;
                if off + 8 > bytes.len() { break; }
                let v = f64::from_ne_bytes(bytes[off..off + 8].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Double(v));
            }
        }
        _ => {
            // Reference / Boolean / Char / Byte / Short — Phase 3 surface
            // only declares int/long/float/double wraps. If a caller ever
            // lands here, leave the array zero-initialized.
        }
    }
    obj
}

/// Read every element of a Java primitive array into a flat byte buffer.
/// Returns `(element_type, element_count, bytes)`.
#[cfg(feature = "gpu-offload")]
fn snapshot_java_array(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    array: rustjvm_types::ObjectRef,
) -> (ArrayElementType, usize, Vec<u8>) {
    let element_type = ctx.heap_element_type_of(array);
    let length = ctx.array_length(array);
    let mut bytes = Vec::new();
    match element_type {
        ArrayElementType::Int => {
            bytes.reserve_exact(length * 4);
            for i in 0..length {
                if let Value::Int(v) = ctx.get_array_element(array, i) {
                    bytes.extend_from_slice(&v.to_ne_bytes());
                } else {
                    bytes.extend_from_slice(&0i32.to_ne_bytes());
                }
            }
        }
        ArrayElementType::Long => {
            bytes.reserve_exact(length * 8);
            for i in 0..length {
                if let Value::Long(v) = ctx.get_array_element(array, i) {
                    bytes.extend_from_slice(&v.to_ne_bytes());
                } else {
                    bytes.extend_from_slice(&0i64.to_ne_bytes());
                }
            }
        }
        ArrayElementType::Float => {
            bytes.reserve_exact(length * 4);
            for i in 0..length {
                if let Value::Float(v) = ctx.get_array_element(array, i) {
                    bytes.extend_from_slice(&v.to_ne_bytes());
                } else {
                    bytes.extend_from_slice(&0f32.to_ne_bytes());
                }
            }
        }
        ArrayElementType::Double => {
            bytes.reserve_exact(length * 8);
            for i in 0..length {
                if let Value::Double(v) = ctx.get_array_element(array, i) {
                    bytes.extend_from_slice(&v.to_ne_bytes());
                } else {
                    bytes.extend_from_slice(&0f64.to_ne_bytes());
                }
            }
        }
        _ => {
            // Reference arrays are out-of-scope for the Phase-3 surface.
        }
    }
    (element_type, length, bytes)
}

// ---------------------------------------------------------------------------
// Executor lifecycle
// ---------------------------------------------------------------------------

/// Allocate a new Java object of `class_name`, run its `<init>(J)V`
/// constructor with the supplied handle, and return the boxed
/// `ObjectRef` ready to hand back to the JVM.
///
/// Used by the five handlers that return an opaque Java wrapper around
/// a native handle (executor / future / stream impls). The impl
/// classes' constructors do two things: write `handle` and register
/// with `StreamCleaner`; both must run, so we invoke the real `<init>`
/// rather than poke `handle` via `set_field_by_name`.
#[cfg(feature = "gpu-offload")]
fn instantiate_handle_wrapper(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    class_name: &str,
    handle: u64,
) -> rustjvm_types::error::MethodCallResult {
    let allocated = ctx.new_object(class_name)?;
    match allocated {
        Some(Value::Object(Some(obj))) => {
            ctx.invoke(
                class_name,
                "<init>",
                "(J)V",
                &[Value::Object(Some(obj)), Value::Long(handle as i64)],
            )?;
            Ok(Some(Value::Object(Some(obj))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Native.openExecutor(int device) -> GpuExecutor`
///
/// Allocates a synthetic executor handle, stores the device-ordinal
/// in our process-wide state map, then instantiates a
/// `GpuExecutorImpl` wrapping the handle.
#[cfg(feature = "gpu-offload")]
fn builtin_open_executor(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let device = arg_int(args, 0);
    let handle = state::with(|s| {
        let h = s.fresh_handle();
        s.executors.insert(h, device);
        h
    });
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuExecutorImpl", handle)
}

/// `Native.releaseExecutor(long handle)`
#[cfg(feature = "gpu-offload")]
fn builtin_release_executor(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    state::with(|s| {
        s.executors.remove(&handle);
    });
    Ok(None)
}

// ---------------------------------------------------------------------------
// Submission / launch — both lead to a synthetic Failed future
// ---------------------------------------------------------------------------

#[cfg(feature = "gpu-offload")]
fn record_failed_future() -> u64 {
    state::with(|s| {
        let h = s.fresh_handle();
        s.futures.insert(
            h,
            state::FutureState::Failed { message: STUB_FAILURE_MESSAGE.to_string() },
        );
        h
    })
}

/// `Native.submit(long execHandle, GpuCallable c) -> GpuFuture`
///
/// Records a Failed future in our state (no CUDA device available),
/// then wraps the handle in a real `GpuFutureImpl` Java object so
/// `Native.futureSynchronize` + `Native.futureGetErrorMessage` can
/// drive the round-trip end-to-end.
#[cfg(feature = "gpu-offload")]
fn builtin_submit(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let _exec = arg_long(args, 0) as u64;
    let _callable = arg_object(args, 1);
    let future_handle = record_failed_future();
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", future_handle)
}

/// `Native.launch(long execHandle, GpuRunnable r) -> GpuFuture`
///
/// Same shape as `submit`.
#[cfg(feature = "gpu-offload")]
fn builtin_launch(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let _exec = arg_long(args, 0) as u64;
    let _runnable = arg_object(args, 1);
    let future_handle = record_failed_future();
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", future_handle)
}

/// `Native.submitMethod(long execHandle, String className, String methodName,
///                      String descriptor, Object[] args) -> GpuFuture`
///
/// Phase 5: explicit named-method dispatch. Bypasses lambda
/// resolution. Resolves the target method via the class manager,
/// marshals `args` into `KernelArgs`, and dispatches asynchronously
/// through `OffloadCache::dispatch_async`. Returns a `GpuFutureImpl`
/// wrapping the submission handle the Java side polls.
///
/// Stub-mode behavior: the underlying
/// `dispatch_method_from_native` records a `Failed` submission with
/// message "no CUDA device" / "class not loaded" / etc., depending
/// on which check trips first. The Java side surfaces it as
/// `GpuException` via `futureGetErrorMessage`.
#[cfg(feature = "gpu-offload")]
fn builtin_submit_method(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let _exec = arg_long(args, 0) as u64;

    // Read the three string params. If any is null/unreadable, fail
    // synthetically and let the Java side surface it.
    let class_name = match arg_object(args, 1).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            let h = record_failed_future_with_message("submitMethod: className was null");
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                h,
            );
        }
    };
    let method_name = match arg_object(args, 2).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            let h = record_failed_future_with_message("submitMethod: methodName was null");
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                h,
            );
        }
    };
    let descriptor = match arg_object(args, 3).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            let h = record_failed_future_with_message("submitMethod: descriptor was null");
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                h,
            );
        }
    };

    // Convert the Object[] argument array into a Vec<Value>. Each
    // slot is read via NativeContext::get_array_element so the JVM
    // layer can unbox / reference-pass as it normally would for a
    // varargs call.
    let java_args_obj = match arg_object(args, 4) {
        Some(o) => o,
        None => {
            let h = record_failed_future_with_message("submitMethod: args array was null");
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                h,
            );
        }
    };
    let n = ctx.array_length(java_args_obj);
    let mut java_args: Vec<Value> = Vec::with_capacity(n);
    for i in 0..n {
        java_args.push(ctx.get_array_element(java_args_obj, i));
    }

    // Dispatch via the NativeContext escape hatch. The VM's impl
    // calls into `runtime::offload::dispatch_method_from_native`.
    let submission_handle = match ctx.gpu_dispatch_method(
        &class_name,
        &method_name,
        &descriptor,
        &java_args,
    ) {
        Some(h) => h,
        None => {
            // gpu-offload feature off on the VM side. Fall back to
            // the synthetic Failed-future path so the Java side gets
            // a coherent error.
            record_failed_future_with_message(
                "submitMethod: gpu-offload feature is disabled in this build",
            )
        }
    };

    instantiate_handle_wrapper(
        ctx,
        "craton/gpu/internal/GpuFutureImpl",
        submission_handle,
    )
}

/// gpu-offload-off shim — submitMethod is unreachable in default
/// builds because `register()` is a no-op. Kept here so callers can
/// always name the function regardless of feature.
#[cfg(not(feature = "gpu-offload"))]
fn builtin_submit_method(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    _args: &[rustjvm_types::Value],
) -> rustjvm_types::error::MethodCallResult {
    Ok(Some(rustjvm_types::Value::Object(None)))
}

#[cfg(feature = "gpu-offload")]
fn record_failed_future_with_message(message: &str) -> u64 {
    state::with(|s| {
        let h = s.fresh_handle();
        s.futures.insert(
            h,
            state::FutureState::Failed { message: message.to_string() },
        );
        h
    })
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// `Native.newStream(long execHandle) -> GpuStream`
///
/// Records the stream → executor mapping in our state and wraps the
/// new handle in a `GpuStreamImpl`.
#[cfg(feature = "gpu-offload")]
fn builtin_new_stream(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let handle = state::with(|s| {
        let h = s.fresh_handle();
        s.streams.insert(h, exec);
        h
    });
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuStreamImpl", handle)
}

/// `Native.closeStream(long streamHandle)`
#[cfg(feature = "gpu-offload")]
fn builtin_close_stream(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    state::with(|s| {
        s.streams.remove(&handle);
    });
    Ok(None)
}

// ---------------------------------------------------------------------------
// Futures — these are the only Native.* surface that round-trips entirely
// through `long` handles, so they work end-to-end in stub mode.
// ---------------------------------------------------------------------------

/// `Native.futureStatus(long futureHandle) -> int`
///
/// Status codes (mirrors the Java side enum-ordinal layout in the spec):
///   `0` = PENDING, `1` = DONE, `2` = FAILED, `3` = UNKNOWN
#[cfg(feature = "gpu-offload")]
fn builtin_future_status(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let code = state::with(|s| match s.futures.get(&handle) {
        Some(state::FutureState::Pending) => 0i32,
        Some(state::FutureState::Done { .. }) => 1,
        Some(state::FutureState::Failed { .. }) => 2,
        None => 3,
    });
    Ok(Some(Value::Int(code)))
}

/// `Native.futureSynchronize(long futureHandle)`
///
/// In stub mode futures are never `Pending` after construction, so this
/// is a no-op. With a real device we would `cuStreamSynchronize` here.
#[cfg(feature = "gpu-offload")]
fn builtin_future_synchronize(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    _args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    Ok(None)
}

/// `Native.futureGetResult(long futureHandle) -> Object`
///
/// Returns the stored mirror for `Done` futures, `null` otherwise.
/// PHASE4-CUDA-TODO: today every synthetic future is `Failed` (no
/// device); the stored-`Done` path will be exercised once the real
/// CUDA launch path lands and populates `FutureState::Done` with a
/// freshly-built primitive-array `ObjectRef`.
#[cfg(feature = "gpu-offload")]
fn builtin_future_get_result(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let result = state::with(|s| match s.futures.get(&handle) {
        Some(state::FutureState::Done { result_obj }) => *result_obj,
        _ => None,
    });
    Ok(Some(Value::Object(result)))
}

/// `Native.futureGetErrorMessage(long futureHandle) -> String`
#[cfg(feature = "gpu-offload")]
fn builtin_future_get_error_message(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let msg = state::with(|s| match s.futures.get(&handle) {
        Some(state::FutureState::Failed { message }) => Some(message.clone()),
        _ => None,
    });
    let value = match msg {
        Some(text) => {
            let s = ctx.create_string(&text);
            Value::Object(Some(s))
        }
        None => Value::Object(None),
    };
    Ok(Some(value))
}

/// `Native.releaseFuture(long futureHandle)`
#[cfg(feature = "gpu-offload")]
fn builtin_release_future(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    state::with(|s| {
        s.futures.remove(&handle);
    });
    Ok(None)
}

// ---------------------------------------------------------------------------
// Array wrap / readback — pure-handle interface, works end-to-end in stub
// mode. When the real `ResidencyTracker` lands these are rewritten to call
// `shared_vm.residency.upload(...)` / `.download(...)` instead of holding
// the bytes locally.
// ---------------------------------------------------------------------------

#[cfg(feature = "gpu-offload")]
fn wrap_primitive_array(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let array = match arg_object(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let (element_type, element_count, bytes) = snapshot_java_array(ctx, array);
    let handle = state::with(|s| {
        let h = s.fresh_handle();
        s.arrays.insert(
            h,
            state::ArrayEntry {
                element_type,
                element_count,
                bytes,
                resident: true,
            },
        );
        h
    });
    Ok(Some(Value::Long(handle as i64)))
}

/// `Native.arrayWrapInt(int[]) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_int(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayWrapLong(long[]) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_long(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayWrapFloat(float[]) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_float(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayWrapDouble(double[]) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_double(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayToHost(long arrayHandle) -> Object`
///
/// Looks up the handle in our state and rebuilds a fresh Java primitive
/// array of the same shape from the stored bytes.
#[cfg(feature = "gpu-offload")]
fn builtin_array_to_host(
    ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let snapshot = state::with(|s| {
        s.arrays.get(&handle).map(|entry| {
            (entry.element_type, entry.element_count, entry.bytes.clone())
        })
    });
    match snapshot {
        Some((etype, count, bytes)) => {
            let arr = rebuild_java_array(ctx, etype, count, &bytes);
            Ok(Some(Value::Object(Some(arr))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `Native.arrayIsResident(long arrayHandle) -> boolean`
#[cfg(feature = "gpu-offload")]
fn builtin_array_is_resident(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let resident = state::with(|s| {
        s.arrays.get(&handle).map(|e| e.resident).unwrap_or(false)
    });
    Ok(Some(Value::Int(if resident { 1 } else { 0 })))
}

/// `Native.releaseArray(long arrayHandle)`
#[cfg(feature = "gpu-offload")]
fn builtin_release_array(
    _ctx: &mut dyn rustjvm_native_api::NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    state::with(|s| {
        s.arrays.remove(&handle);
    });
    Ok(None)
}
