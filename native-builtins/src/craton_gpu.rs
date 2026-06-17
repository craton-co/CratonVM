// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::NativeMethodRegistry;

#[cfg(feature = "gpu-offload")]
use std::collections::HashMap;
#[cfg(feature = "gpu-offload")]
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "gpu-offload")]
use cratonvm_types::{ArrayElementType, Value};

// ---------------------------------------------------------------------------
// Public entry point: stub when feature is off, real registration when on
// ---------------------------------------------------------------------------

/// Register every `craton/gpu/internal/Native` native shim.
///
/// Called once from `lib.rs::register_essential_natives`. On a default
/// (no-feature) build this is a no-op.
#[cfg(feature = "gpu-offload")]
pub(crate) fn register(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    const KLASS: &str = "craton/gpu/internal/Native";

    // Device enumeration — delegates to `cuda_bridge::probe()` via the
    // `NativeContext::gpu_device_info` escape hatch. Truthful on a
    // driverless host: `deviceCount` returns 0 and the per-device
    // queries return null / 0 for every ordinal.
    registry.register(KLASS, "deviceCount",              "()I", builtin_device_count);
    registry.register(KLASS, "deviceName",               "(I)Ljava/lang/String;", builtin_device_name);
    registry.register(KLASS, "deviceTotalMemory",        "(I)J", builtin_device_total_memory);
    registry.register(KLASS, "deviceComputeCapability",  "(I)I", builtin_device_compute_capability);

    registry.register(KLASS, "openExecutor", "(I)Lcraton/gpu/GpuExecutor;", builtin_open_executor);
    registry.register(KLASS, "submit",       "(JLcraton/gpu/GpuCallable;)Lcraton/gpu/GpuFuture;", builtin_submit);
    registry.register(KLASS, "launch",       "(JLcraton/gpu/GpuRunnable;)Lcraton/gpu/GpuFuture;", builtin_launch);
    registry.register(
        KLASS,
        "submitMethod",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_method,
    );
    registry.register(
        KLASS,
        "submitWithArg",
        "(JLjava/lang/Object;Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_with_arg,
    );
    registry.register(
        KLASS,
        "submitWithArgs",
        "(JLjava/lang/Object;[Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_with_args,
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
    registry.set_category(__prev_cat);
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
        Done { result_obj: Option<cratonvm_types::ObjectRef> },
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
// Public accessors for cross-crate use (Phase 6 #3: GpuArray routing)
// ---------------------------------------------------------------------------
//
// The `state` module is intentionally private (the native-handler
// implementation is the only thing meant to touch the synthetic
// store directly). But the GPU dispatch code in
// `cratonvm_vm::runtime::offload` needs to read array bytes back
// when it sees a `craton.gpu.GpuArray` Java argument to a kernel —
// it gets the long handle from the GpuArray's `handle` field and
// looks up the bytes here.
//
// Two thin accessors that take + return owned data are enough; no
// raw reference into the mutex is exposed.

/// (Phase 6 #3) Snapshot the host bytes + element type for the
/// array handle returned by `Native.arrayWrap*`. Returns `None` if
/// the handle is unknown or has been released. The returned
/// `Vec<u8>` is a fresh allocation; the caller may modify it
/// without affecting the store.
#[cfg(feature = "gpu-offload")]
pub fn array_snapshot(
    handle: u64,
) -> Option<(cratonvm_types::ArrayElementType, usize, Vec<u8>)> {
    state::with(|s| {
        s.arrays
            .get(&handle)
            .map(|e| (e.element_type, e.element_count, e.bytes.clone()))
    })
}

/// (Phase 6 #3) Replace the host bytes for the array handle. The
/// `element_count` and `element_type` stay as set by `arrayWrap`;
/// only the byte payload is overwritten. Called after a kernel
/// completes so a subsequent `GpuArray.toHost()` reads the new
/// contents.
#[cfg(feature = "gpu-offload")]
pub fn array_replace_bytes(handle: u64, bytes: Vec<u8>) {
    state::with(|s| {
        if let Some(e) = s.arrays.get_mut(&handle) {
            e.bytes = bytes;
        }
    });
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
fn arg_object(args: &[Value], idx: usize) -> Option<cratonvm_types::ObjectRef> {
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    element_type: ArrayElementType,
    element_count: usize,
    bytes: &[u8],
) -> cratonvm_types::ObjectRef {
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    array: cratonvm_types::ObjectRef,
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
// Device enumeration
// ---------------------------------------------------------------------------
//
// These are the only `Native.*` handlers that touch real hardware
// information. They delegate to `NativeContext::gpu_device_info`, whose
// VM override calls `cuda_bridge::probe()`. On a driverless host (or a
// stub-mode `cuda-bridge`, or the trait default) the escape hatch yields
// an empty list, so `deviceCount` returns 0 and the per-ordinal queries
// return null / 0. No handler ever fabricates a device.

/// One entry of the device list as produced by
/// `NativeContext::gpu_device_info`: `(name, compute_major,
/// compute_minor, total_global_mem_bytes)`.
#[cfg(feature = "gpu-offload")]
type DeviceInfo = (String, u32, u32, u64);

/// Bounds-safe ordinal lookup shared by the per-device queries.
///
/// A negative `ordinal` (Java `int` can be negative) or one past the
/// last device yields `None`. Factored out so the bounds logic is
/// covered by a driver-free unit test against synthetic device lists.
#[cfg(feature = "gpu-offload")]
fn device_at(devices: &[DeviceInfo], ordinal: i32) -> Option<&DeviceInfo> {
    usize::try_from(ordinal).ok().and_then(|i| devices.get(i))
}

/// Pack a `(major, minor)` compute capability into the `major*10 + minor`
/// integer the Java side expects (sm_75 → `75`). Kept separate so the
/// packing is unit-tested directly.
#[cfg(feature = "gpu-offload")]
fn pack_compute_capability(major: u32, minor: u32) -> i32 {
    (major * 10 + minor) as i32
}

/// `Native.deviceCount() -> int`
///
/// Number of attached CUDA devices. `0` when there is no driver.
#[cfg(feature = "gpu-offload")]
fn builtin_device_count(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let count = ctx.gpu_device_info().len();
    Ok(Some(Value::Int(count as i32)))
}

/// `Native.deviceName(int ordinal) -> String`
///
/// Device product name (e.g. `"NVIDIA GeForce RTX 2060"`), or `null`
/// when `ordinal` is out of range / no device is present.
#[cfg(feature = "gpu-offload")]
fn builtin_device_name(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let ordinal = arg_int(args, 0);
    let devices = ctx.gpu_device_info();
    let name = device_at(&devices, ordinal).map(|(name, _, _, _)| name.clone());
    let value = match name {
        Some(text) => Value::Object(Some(ctx.create_string(&text))),
        None => Value::Object(None),
    };
    Ok(Some(value))
}

/// `Native.deviceTotalMemory(int ordinal) -> long`
///
/// Total global device memory in bytes, or `0` when `ordinal` is out of
/// range / no device is present.
#[cfg(feature = "gpu-offload")]
fn builtin_device_total_memory(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let ordinal = arg_int(args, 0);
    let devices = ctx.gpu_device_info();
    let bytes = device_at(&devices, ordinal)
        .map(|(_, _, _, mem)| *mem)
        .unwrap_or(0);
    Ok(Some(Value::Long(bytes as i64)))
}

/// `Native.deviceComputeCapability(int ordinal) -> int`
///
/// Compute capability packed as `major * 10 + minor` (e.g. sm_75 →
/// `75`), or `0` when `ordinal` is out of range / no device is present.
/// `0` is an impossible real capability, so it doubles as a sentinel.
#[cfg(feature = "gpu-offload")]
fn builtin_device_compute_capability(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let ordinal = arg_int(args, 0);
    let devices = ctx.gpu_device_info();
    let packed = device_at(&devices, ordinal)
        .map(|(_, major, minor, _)| pack_compute_capability(*major, *minor))
        .unwrap_or(0);
    Ok(Some(Value::Int(packed)))
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    class_name: &str,
    handle: u64,
) -> cratonvm_types::error::MethodCallResult {
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let device = arg_int(args, 0);
    let handle = state::with(|s| {
        let h = s.fresh_handle();
        s.executors.insert(h, device);
        h
    });
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuExecutorImpl", handle)
}

/// `Native.releaseExecutor(long handle)`
///
/// Phase 10 #1: also flushes the explicit-submit input-residency
/// cache so device buffers cached for plain JVM primitive arrays
/// are freed when the Java `GpuExecutor` is closed. (Stricter than
/// strictly needed — multiple executors share the same global
/// cache, so closing one wipes residency for the others too — but
/// `GpuExecutor.close()` is rare and idempotent eviction is safe.)
#[cfg(feature = "gpu-offload")]
fn builtin_release_executor(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    state::with(|s| {
        s.executors.remove(&handle);
    });
    ctx.gpu_clear_input_cache();
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
/// Phase 6 #5: tries to resolve the GpuCallable lambda's target
/// method via `ctx.gpu_resolve_lambda_target`. If successful, the
/// captured values become the kernel args and we dispatch through
/// the real path (same as if the user had called the explicit
/// `submit(class, method, descriptor, args)` form). Otherwise we
/// fall back to recording a synthetic Failed future so the Java
/// side surfaces a clear error message.
#[cfg(feature = "gpu-offload")]
fn builtin_submit(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let _exec = arg_long(args, 0) as u64;
    let callable = match arg_object(args, 1) {
        Some(o) => o,
        None => {
            let h = record_failed_future_with_message("submit: callable was null");
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h);
        }
    };
    // Phase 6 #5: try to resolve the lambda's target.
    if let Some((class_name, method_name, descriptor, captures)) =
        ctx.gpu_resolve_lambda_target(callable)
    {
        if let Some(handle) = ctx.gpu_dispatch_method(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
        ) {
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                handle,
            );
        }
    }
    // Fallback — the callable is not a recognized GPU-dispatchable
    // lambda (anonymous-class implementation, non-static target,
    // capture types we don't understand, etc.). Surface a clear
    // failure rather than silently running on CPU.
    let h = record_failed_future_with_message(
        "submit: callable is not a GPU-dispatchable lambda \
         (expected a static-method reference with capture-only args)",
    );
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h)
}

/// Phase 7 #3 — `Native.submitWithArg(long execHandle, Object
/// lambda, Object samArg) -> GpuFuture`
///
/// For `GpuFunction<T, R>` (and any other single-arg SAM) lambdas.
/// Resolves the lambda target like Phase 6 #5, then appends
/// `samArg` to the captures so the kernel sees
/// `(capture0, capture1, ..., samArg)` in order. If the lambda
/// isn't a static-method-reference shape, returns a synthetic
/// Failed future; the Java caller can fall back to CPU evaluation.
#[cfg(feature = "gpu-offload")]
fn builtin_submit_with_arg(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let _exec = arg_long(args, 0) as u64;
    let lambda = match arg_object(args, 1) {
        Some(o) => o,
        None => {
            let h = record_failed_future_with_message("submitWithArg: lambda was null");
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h);
        }
    };
    // samArg may be null (e.g. for a SAM whose input is a reference
    // type and the caller passed null) — preserve it as
    // Value::Object(None). The dispatcher will surface "null arg"
    // if the target descriptor needs an array there.
    let sam_arg = args.get(2).copied().unwrap_or(Value::Object(None));

    if let Some((class_name, method_name, descriptor, mut captures)) =
        ctx.gpu_resolve_lambda_target(lambda)
    {
        captures.push(sam_arg);
        if let Some(handle) = ctx.gpu_dispatch_method(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
        ) {
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                handle,
            );
        }
    }
    let h = record_failed_future_with_message(
        "submitWithArg: lambda is not a GPU-dispatchable single-arg SAM",
    );
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h)
}

/// Phase 8 #6 — `Native.submitWithArgs(long execHandle, Object
/// lambda, Object[] samArgs) -> GpuFuture`
///
/// Generalised companion to `submitWithArg` (Phase 7 #3) for SAMs
/// with arity > 1: BiFunction, TriFunction, custom multi-param
/// @FunctionalInterface types. Captures are followed by every
/// element of `samArgs` in declaration order; the resulting list
/// is passed to `gpu_dispatch_method`. `samArgs` is allowed to
/// be empty (in which case this collapses to the Phase 6 #5
/// zero-arg behavior of `submit`).
#[cfg(feature = "gpu-offload")]
fn builtin_submit_with_args(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let _exec = arg_long(args, 0) as u64;
    let lambda = match arg_object(args, 1) {
        Some(o) => o,
        None => {
            let h = record_failed_future_with_message("submitWithArgs: lambda was null");
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h);
        }
    };
    // samArgs may be null; we treat that as the empty arg array.
    let sam_args_obj = arg_object(args, 2);

    if let Some((class_name, method_name, descriptor, mut captures)) =
        ctx.gpu_resolve_lambda_target(lambda)
    {
        if let Some(arr_obj) = sam_args_obj {
            let n = ctx.array_length(arr_obj);
            for i in 0..n {
                captures.push(ctx.get_array_element(arr_obj, i));
            }
        }
        if let Some(handle) = ctx.gpu_dispatch_method(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
        ) {
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                handle,
            );
        }
    }
    let h = record_failed_future_with_message(
        "submitWithArgs: lambda is not a GPU-dispatchable static-method reference",
    );
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h)
}

/// `Native.launch(long execHandle, GpuRunnable r) -> GpuFuture`
///
/// Same shape as `submit` — Phase 6 #5 resolution applies. The
/// only difference is the SAM (`run()` returns void), so the
/// returned future's parametric type is `Void`.
#[cfg(feature = "gpu-offload")]
fn builtin_launch(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let _exec = arg_long(args, 0) as u64;
    let runnable = match arg_object(args, 1) {
        Some(o) => o,
        None => {
            let h = record_failed_future_with_message("launch: runnable was null");
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h);
        }
    };
    if let Some((class_name, method_name, descriptor, captures)) =
        ctx.gpu_resolve_lambda_target(runnable)
    {
        if let Some(handle) = ctx.gpu_dispatch_method(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
        ) {
            return instantiate_handle_wrapper(
                ctx,
                "craton/gpu/internal/GpuFutureImpl",
                handle,
            );
        }
    }
    let h = record_failed_future_with_message(
        "launch: runnable is not a GPU-dispatchable lambda \
         (expected a static-method reference with capture-only args)",
    );
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", h)
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
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
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(Some(cratonvm_types::Value::Object(None)))
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
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
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    // Phase 6 #4 — prefer the real submission registry. The
    // synthetic state is the fallback for stub-only fixtures
    // (`builtin_submit` / `builtin_launch` records that never go
    // through the real dispatcher).
    if let Some(code) = ctx.gpu_future_status(handle) {
        return Ok(Some(Value::Int(code)));
    }
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    // Phase 6 #4 — if the handle is a real submission, block on
    // its event. Otherwise no-op (synthetic Failed futures are
    // immediately observable, no waiting needed).
    let _ = ctx.gpu_future_synchronize(handle);
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
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
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
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayWrapLong(long[]) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_long(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayWrapFloat(float[]) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_float(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayWrapDouble(double[]) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_double(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    wrap_primitive_array(ctx, args)
}

/// `Native.arrayToHost(long arrayHandle) -> Object`
///
/// Looks up the handle in our state and rebuilds a fresh Java primitive
/// array of the same shape from the stored bytes.
#[cfg(feature = "gpu-offload")]
fn builtin_array_to_host(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    // Phase 9 #1 — pull any pending device-side writes into the
    // resident store BEFORE we read it. The Resident writebacks
    // (Phase 7 #2 + Phase 9 #1) defer the D→H copy so kernel
    // pipelines that don't read host bytes between steps don't
    // pay for them; this is where the copy finally happens (or
    // doesn't, if the entry is clean).
    if let Some(fresh_bytes) = ctx.gpu_array_download_if_dirty(handle) {
        array_replace_bytes(handle, fresh_bytes);
    }
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
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let resident = state::with(|s| {
        s.arrays.get(&handle).map(|e| e.resident).unwrap_or(false)
    });
    Ok(Some(Value::Int(if resident { 1 } else { 0 })))
}

/// `Native.releaseArray(long arrayHandle)`
#[cfg(feature = "gpu-offload")]
fn builtin_release_array(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    // Phase 8 #1 — drop both the host-side resident entry AND any
    // cached device buffer keyed by the same handle. Without the
    // device-cache eviction, a long-running Java program that
    // wraps + releases many GpuArrays would accumulate device
    // memory until process exit.
    state::with(|s| {
        s.arrays.remove(&handle);
    });
    ctx.gpu_release_array_cache(handle);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests (gpu-offload only)
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "gpu-offload"))]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    fn synthetic_devices() -> Vec<DeviceInfo> {
        vec![
            ("NVIDIA GeForce RTX 2060".to_string(), 7, 5, 6 * 1024 * 1024 * 1024),
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
}
