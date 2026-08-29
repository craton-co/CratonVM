// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native shims for `craton.gpu.internal.Native` (Phase 3 — Item P3-4).
//!
//! These are the Rust-side handlers behind every `Native.*` method the
//! Phase-3 Java surface declares (see `gpu/phase3-spec.md` §2.2).
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
//!
//! ## Status (2026-07-11 — scalar futures + non-blocking `isDone`)
//!
//! `vm::runtime::offload` gained a `SerializedResult::{ScalarI32,
//! ScalarI64, ScalarF32, ScalarF64}` family (stamped into
//! `SubmissionStatus::Completed` by `finalize_submission` for
//! reduction-style kernels that return a value instead of writing an
//! `out` array) plus a non-blocking `poll_submission_status` probe
//! that finalizes a submission inline once the device event is ready,
//! instead of requiring a blocking `futureSynchronize`/`get()` call
//! first (see `docs/gpu/async-api.md`'s "Completion model" note and
//! `fixed-suite-bugs/gpu-offload-followups-20260711.md` items #1/#3).
//!
//! This file's contribution:
//!   * `FutureState::DoneScalar` — a local-registry counterpart to
//!     `FutureState::Done` that carries a raw scalar `Value` (Int /
//!     Long / Float / Double) instead of an `ObjectRef`.
//!   * `builtin_future_get_result` boxes a stored `DoneScalar` payload
//!     via the crate's canonical `lang_class::box_value` helper (the
//!     same boxing path `lang_invoke.rs` uses for reflective/
//!     MethodHandle scalar returns) before handing it back as the
//!     `Object` the `(J)Ljava/lang/Object;` descriptor promises.
//!   * `Native.futureIsDone` (`builtin_future_is_done`) — a new,
//!     dedicated non-blocking completion probe: Running(0) => false,
//!     Completed(1)/Failed(2) => true, unknown handle => false (the
//!     same "absent => false" convention `arrayIsResident` uses).
//!
//! ## Status (2026-07-11, continued — real-submission scalar results)
//!
//! The gap above is closed: `NativeContext::gpu_future_status`'s VM
//! override now calls `offload::poll_submission_status` instead of
//! peeking `SubmissionStatus` directly, so it finalizes a submission
//! inline the first time it observes device-side completion —
//! `builtin_future_is_done` / `builtin_future_status` are live for a
//! real dispatch, not just the local synthetic map. A new escape hatch,
//! `NativeContext::gpu_future_take_result`, mirrors `gpu_future_status`
//! (same non-blocking contract) and hands back a completed submission's
//! `SerializedResult` translated into the `native-api`-side
//! `GpuFutureResult` transport enum. `builtin_future_get_result` (below)
//! tries this real-registry path FIRST and only falls back to the
//! local synthetic `FutureState` map (`Done`/`DoneScalar`) when it
//! returns `None` — which happens for any handle the real dispatcher
//! never registered (every stub-mode fixture) as well as for a
//! genuinely not-yet-complete real submission.
//!
//! PHASE4-CUDA-TODO (residual, out of this file's reach): there is
//! still no CUDA device in this environment, so the real-registry path
//! is exercised by unit tests that install a canned
//! `gpu_future_take_result` answer on `MockNativeContext` rather than by
//! an actual device dispatch — see the unit tests below. Primitive-array
//! future results remain unwired (see `GpuFutureResult`'s doc comment);
//! array outputs continue to flow through writeback into the caller's
//! own array, which this file's `arrayToHost`/`gpuArrayDownloadIfDirty`
//! handlers already cover independently of futures.
//!
//! ## Completion-model note (non-blocking `get()`)
//!
//! `builtin_future_get_result`'s real-registry branch is genuinely
//! non-blocking: it only returns a value when
//! `NativeContext::gpu_future_take_result` does, which in turn requires
//! the submission to already be observably complete (see that method's
//! doc comment) — it will not wait for a `Running` submission to
//! finish. That's fine for the Java-level contract IF the caller
//! already checked `isDone()` and got `true` first (the `Future.get()`
//! idiom this backs typically does exactly that, or accepts a `null`
//! "not ready" answer). A `get()` on a future the caller has *not* first
//! observed as done still needs to block, and that path is unchanged:
//! it goes through `Native.futureSynchronize`
//! (`builtin_future_synchronize`, below) → `gpu_future_synchronize` →
//! `offload::finalize_submission`, which does wait on the device event.
//!
//! ## Status (2026-07-11, continued — device-only array allocation)
//!
//! `docs/gpu/async-api.md` documented `GpuArray.allocate(GpuExecutor,
//! int)` (a device-side allocation with no host source array) but no
//! matching native shim existed — `register()` above only ever declared
//! `arrayWrapInt/Long/Float/Double` (upload from an existing Java
//! array). Neither `gpu/phase3-spec.md` §2.2 nor any
//! later phase spec defines an `allocate*` native name or descriptor
//! (`arrayWrap*` is the only `GpuArray`-backing surface either ever
//! lists), so the names below (`arrayAllocateInt/Long/Float/Double`,
//! each `"(I)J"`) are new, chosen to mirror the `arrayWrap*` family's
//! naming precisely — there is no spec text they had to match instead.
//!
//! `builtin_array_allocate_*` mints a `state::ArrayEntry` exactly the
//! way `wrap_primitive_array` does, except the `bytes` mirror starts
//! zero-filled (`vec![0u8; element_count * element_bytes]`) instead of
//! being snapshotted from a Java array — this matches Java `new
//! int[n]`/`new long[n]`/etc. zero-initialization semantics, so a
//! `toHost()` on a never-uploaded, never-kernel-written handle returns
//! an all-zero array via the existing `builtin_array_to_host` /
//! `rebuild_java_array` path unchanged. A negative `length` is a bad
//! argument; per the file-wide convention that no array shim ever
//! constructs a Java exception itself (only the submission family does,
//! via a synthetic `Failed` future — see `wrap_primitive_array`'s
//! null-host-array case), it returns the sentinel handle `0` without
//! minting a state entry, the same "bad arg -> handle 0, no entry"
//! shape `wrap_primitive_array` already uses.

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
    registry.register(KLASS, "deviceCount", "()I", builtin_device_count);
    registry.register(
        KLASS,
        "deviceName",
        "(I)Ljava/lang/String;",
        builtin_device_name,
    );
    registry.register(
        KLASS,
        "deviceTotalMemory",
        "(I)J",
        builtin_device_total_memory,
    );
    registry.register(
        KLASS,
        "deviceComputeCapability",
        "(I)I",
        builtin_device_compute_capability,
    );

    registry.register(
        KLASS,
        "openExecutor",
        "(I)Lcraton/gpu/GpuExecutor;",
        builtin_open_executor,
    );
    registry.register(
        KLASS,
        "submit",
        "(JLcraton/gpu/GpuCallable;)Lcraton/gpu/GpuFuture;",
        builtin_submit,
    );
    registry.register(
        KLASS,
        "launch",
        "(JLcraton/gpu/GpuRunnable;)Lcraton/gpu/GpuFuture;",
        builtin_launch,
    );
    registry.register(
        KLASS,
        "submitMethod",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_method,
    );
    // Fire-and-forget: the same dispatch, answering the submission
    // handle rather than a `GpuFuture`. See `builtin_submit_method_handle`
    // for why the object is worth avoiding.
    registry.register(
        KLASS,
        "submitMethodHandle",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)J",
        builtin_submit_method_handle,
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
    registry.register(
        KLASS,
        "newStream",
        "(J)Lcraton/gpu/GpuStream;",
        builtin_new_stream,
    );
    registry.register(KLASS, "closeStream", "(J)V", builtin_close_stream);
    // Stream-scoped dispatch. `gpu_dispatch_method_on_stream` has always been
    // there and `submit_method_dispatch` has always called it -- but only ever
    // with a stream resolved from the executor, so a caller holding a
    // `GpuStream` could not submit onto it.
    registry.register(
        KLASS,
        "streamSubmitMethod",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)J",
        builtin_stream_submit_method,
    );

    registry.register(KLASS, "futureStatus", "(J)I", builtin_future_status);
    registry.register(KLASS, "futureIsDone", "(J)Z", builtin_future_is_done);
    registry.register(
        KLASS,
        "futureSynchronize",
        "(J)V",
        builtin_future_synchronize,
    );
    registry.register(
        KLASS,
        "futureGetResult",
        "(J)Ljava/lang/Object;",
        builtin_future_get_result,
    );
    registry.register(
        KLASS,
        "futureGetErrorMessage",
        "(J)Ljava/lang/String;",
        builtin_future_get_error_message,
    );
    // `GpuFuture.get()` reads a failed submission's reason through
    // `futureGetError`, not `futureGetErrorMessage`. Registering only
    // the latter meant the DIAGNOSTIC path for every GPU failure was
    // itself a failure: a kernel that could not be compiled, marshalled
    // or launched surfaced as `UnsatisfiedLinkError:
    // Native.futureGetError` from inside `get()`, hiding the message
    // the Rust side had already produced. Same body, both names.
    registry.register(
        KLASS,
        "futureGetError",
        "(J)Ljava/lang/String;",
        builtin_future_get_error_message,
    );
    // `GpuFuture.cancel(boolean)` calls this. It was never registered, so
    // every cancel attempt on a real CratonVM died with
    // `UnsatisfiedLinkError: Native.futureCancel` instead of returning the
    // documented "could not cancel" answer -- and it died from inside
    // `cancel()`, which the Java side documents as returning `false` rather
    // than throwing. Registered now; see `builtin_future_cancel` for why the
    // answer is always "rejected".
    registry.register(KLASS, "futureCancel", "(JZ)I", builtin_future_cancel);

    registry.register(KLASS, "arrayWrapInt", "([I)J", builtin_array_wrap_int);
    registry.register(KLASS, "arrayWrapLong", "([J)J", builtin_array_wrap_long);
    registry.register(KLASS, "arrayWrapFloat", "([F)J", builtin_array_wrap_float);
    registry.register(KLASS, "arrayWrapDouble", "([D)J", builtin_array_wrap_double);
    // 2026-07-11: device-only allocation (`GpuArray.allocate`, no host
    // source array) — see the module doc's "device-only array
    // allocation" note for why these names/descriptors are new rather
    // than spec-derived.
    registry.register(
        KLASS,
        "arrayAllocateInt",
        "(I)J",
        builtin_array_allocate_int,
    );
    registry.register(
        KLASS,
        "arrayAllocateLong",
        "(I)J",
        builtin_array_allocate_long,
    );
    registry.register(
        KLASS,
        "arrayAllocateFloat",
        "(I)J",
        builtin_array_allocate_float,
    );
    registry.register(
        KLASS,
        "arrayAllocateDouble",
        "(I)J",
        builtin_array_allocate_double,
    );
    registry.register(
        KLASS,
        "arrayToHost",
        "(J)Ljava/lang/Object;",
        builtin_array_to_host,
    );
    // Read back into the caller's array instead of a fresh one. See
    // `builtin_array_to_host_into`: the buffer-reusing GpuArray.toHost(dest)
    // overloads bounded the caller's garbage but not the allocation here.
    registry.register(
        KLASS,
        "arrayToHostInto",
        "(JLjava/lang/Object;)Z",
        builtin_array_to_host_into,
    );
    registry.register(KLASS, "arrayIsResident", "(J)Z", builtin_array_is_resident);

    registry.register(KLASS, "releaseFuture", "(J)V", builtin_release_future);
    registry.register(KLASS, "releaseArray", "(J)V", builtin_release_array);
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
        Done {
            result_obj: Option<cratonvm_types::ObjectRef>,
        },
        /// 2026-07-11 — local-registry counterpart to `Done` for a
        /// kernel that returns a scalar (mirrors one of the
        /// `vm::runtime::offload::SerializedResult::Scalar{I32,I64,F32,
        /// F64}` variants) instead of writing into a caller-supplied
        /// output array. `value` is always `Value::Int` / `Value::Long`
        /// / `Value::Float` / `Value::Double` — never `Value::Object`
        /// (that shape stays on `Done`).
        DoneScalar {
            value: Value,
        },
        Failed {
            message: String,
        },
    }

    #[derive(Debug, Default)]
    pub(super) struct NativeState {
        pub next_handle: u64,
        pub executors: HashMap<u64, i32>, // handle -> device ordinal
        pub futures: HashMap<u64, FutureState>,
        pub arrays: HashMap<u64, ArrayEntry>,
        pub streams: HashMap<u64, u64>, // stream handle -> owning exec handle
        /// GpuStream affinity — executor handle -> that executor's
        /// lazily-created DEFAULT stream handle. Populated by
        /// `resolve_or_create_default_stream` the first time a given
        /// executor is used by `submit`/`launch`/`submitMethod`/
        /// `submitWithArg(s)`; every later call through the same
        /// executor handle reuses the cached value instead of asking
        /// `NativeContext::gpu_stream_create` for a new one. Absent
        /// entirely (not even a `None` sentinel) when creation last
        /// failed — no device / gpu-offload off — so a box that later
        /// gains a device isn't permanently stuck on the old answer.
        pub executor_default_stream: HashMap<u64, u64>,
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
pub fn array_snapshot(handle: u64) -> Option<(cratonvm_types::ArrayElementType, usize, Vec<u8>)> {
    state::with(|s| {
        s.arrays
            .get(&handle)
            .map(|e| (e.element_type, e.element_count, e.bytes.clone()))
    })
}

/// Element type and length for an `arrayWrap*`/`arrayAllocate*`
/// handle, without copying the payload. Returns `None` if the handle
/// is unknown or has been released.
///
/// This is what the dispatch path wants on every submit: it needs the
/// shape to build the `(ptr, len)` kernel-argument pair, and it needs
/// the bytes only when the device cache misses. Reading the two apart
/// is the difference between a resident weight tensor costing one
/// upload for the life of the process and costing a full `memcpy` of
/// itself per kernel launch.
#[cfg(feature = "gpu-offload")]
pub fn array_shape(handle: u64) -> Option<(cratonvm_types::ArrayElementType, usize)> {
    state::with(|s| {
        s.arrays
            .get(&handle)
            .map(|e| (e.element_type, e.element_count))
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

/// GpuStream affinity — resolve (lazily creating on first use) the
/// DEFAULT CUDA stream for `exec`, an executor handle from
/// `Native.openExecutor`/`builtin_open_executor`.
///
/// This is what closes the gap `docs/gpu/async-api.md` describes
/// under "GpuStream affinity is not wired up": previously every
/// `submit`/`submitMethod`/`launch`/`submitWithArg(s)` call minted a
/// fresh, private, one-shot stream (`dispatch_method_from_native`'s
/// old unconditional `CudaStream::new(ctx)`), so two calls through
/// the SAME executor never shared a stream and never got the CUDA
/// same-stream-serializes-in-submission-order guarantee the executor
/// docs promise. Now the first submit through a given `exec` handle
/// mints ONE real stream (`ctx.gpu_stream_create()`) and every
/// subsequent submit through that same `exec` handle reuses it.
///
/// Returns `None` when `ctx.gpu_stream_create()` does (no device /
/// `gpu-offload` off on the VM side) — callers pass that straight
/// through to `ctx.gpu_dispatch_method_on_stream(..., None)`, which
/// is byte-identical to the pre-existing `gpu_dispatch_method`
/// fresh-stream-per-call behavior. Nothing is cached for that case,
/// so a later call retries rather than being stuck on a stale `None`.
#[cfg(feature = "gpu-offload")]
fn resolve_or_create_default_stream(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    exec: u64,
) -> Option<u64> {
    if let Some(h) = state::with(|s| s.executor_default_stream.get(&exec).copied()) {
        return Some(h);
    }
    let h = ctx.gpu_stream_create()?;
    state::with(|s| {
        s.executor_default_stream.insert(exec, h);
    });
    Some(h)
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
    fill_java_array(ctx, obj, element_type, element_count, bytes);
    obj
}

/// Decodes `bytes` into an existing Java primitive array.
///
/// Split out of [`rebuild_java_array`] so `arrayToHostInto` can reuse the
/// decoding without the allocation: the two differ only in where the elements
/// land.
#[cfg(feature = "gpu-offload")]
fn fill_java_array(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    obj: cratonvm_types::ObjectRef,
    element_type: ArrayElementType,
    element_count: usize,
    bytes: &[u8],
) {
    match element_type {
        ArrayElementType::Int => {
            for i in 0..element_count {
                let off = i * 4;
                if off + 4 > bytes.len() {
                    break;
                }
                let v = i32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Int(v));
            }
        }
        ArrayElementType::Long => {
            for i in 0..element_count {
                let off = i * 8;
                if off + 8 > bytes.len() {
                    break;
                }
                let v = i64::from_ne_bytes(bytes[off..off + 8].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Long(v));
            }
        }
        ArrayElementType::Float => {
            for i in 0..element_count {
                let off = i * 4;
                if off + 4 > bytes.len() {
                    break;
                }
                let v = f32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Float(v));
            }
        }
        ArrayElementType::Double => {
            for i in 0..element_count {
                let off = i * 8;
                if off + 8 > bytes.len() {
                    break;
                }
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
            // Bulk, because this is the shape a large resident buffer
            // arrives in. `read_int_array_into` is one
            // `copy_nonoverlapping` over the heap arena in the VM's
            // override; the element-at-a-time loop below it was ~30 ns
            // per element, which for the 620 million words of a
            // 1B-parameter half-precision model is most of a minute
            // spent copying inside `GpuArray.wrap`.
            let mut words = vec![0i32; length];
            if ctx.read_int_array_into(array, 0, &mut words) == length {
                // Reading an `i32` slice AS bytes needs no alignment
                // beyond the slice's own.
                bytes.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(words.as_ptr() as *const u8, length * 4)
                });
            } else {
                bytes.reserve_exact(length * 4);
                for i in 0..length {
                    if let Value::Int(v) = ctx.get_array_element(array, i) {
                        bytes.extend_from_slice(&v.to_ne_bytes());
                    } else {
                        bytes.extend_from_slice(&0i32.to_ne_bytes());
                    }
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
///
/// GpuStream affinity: also releases the executor's lazily-created
/// default stream (`resolve_or_create_default_stream`), if it ever
/// minted one — an application that only ever called
/// `submit`/`submitMethod`/`launch` (never `newStream` explicitly)
/// still leaves a real CUDA stream registered in the `OffloadCache`
/// otherwise, with nothing else left to release it.
#[cfg(feature = "gpu-offload")]
fn builtin_release_executor(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let default_stream = state::with(|s| {
        s.executors.remove(&handle);
        s.executor_default_stream.remove(&handle)
    });
    if let Some(stream_handle) = default_stream {
        ctx.gpu_stream_release(stream_handle);
    }
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
            state::FutureState::Failed {
                message: STUB_FAILURE_MESSAGE.to_string(),
            },
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
///
/// GpuStream affinity: dispatches onto `execHandle`'s lazily-created
/// default stream (`resolve_or_create_default_stream`) rather than a
/// fresh one-shot stream, so repeated `submit` calls through the same
/// executor serialize on one real CUDA stream.
#[cfg(feature = "gpu-offload")]
fn builtin_submit(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
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
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = ctx.gpu_dispatch_method_on_stream(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
        ) {
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", handle);
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
    let exec = arg_long(args, 0) as u64;
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
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = ctx.gpu_dispatch_method_on_stream(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
        ) {
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", handle);
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
    let exec = arg_long(args, 0) as u64;
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
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = ctx.gpu_dispatch_method_on_stream(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
        ) {
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", handle);
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
    let exec = arg_long(args, 0) as u64;
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
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = ctx.gpu_dispatch_method_on_stream(
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
        ) {
            return instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", handle);
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
/// `dispatch_method_from_native_on_stream` records a `Failed`
/// submission with message "no CUDA device" / "class not loaded" /
/// etc., depending on which check trips first. The Java side
/// surfaces it as `GpuException` via `futureGetErrorMessage`.
///
/// GpuStream affinity: dispatches onto `execHandle`'s lazily-created
/// default stream (`resolve_or_create_default_stream`) rather than a
/// fresh one-shot stream — see that function's doc comment for why
/// this is what makes repeated `submitMethod` calls through the same
/// executor share a real, ordered CUDA stream.
#[cfg(feature = "gpu-offload")]
/// `Native.submitMethodHandle(..)` — the same dispatch as
/// [`builtin_submit_method`], answering the submission handle instead
/// of a `GpuFuture` object.
///
/// Minting the future was measured at 28.8 us of a 63.8 us dispatch —
/// the largest single item — because `GpuFutureImpl's` constructor
/// allocates two `AtomicBoolean`s, a `ReentrantReadWriteLock` and a
/// `Cleaner` registration, and a caller queueing a chain of kernels on
/// one stream discards all but the last of them. The handle is the same
/// one a future would have wrapped, so `futureSynchronize` /
/// `futureStatus` / `futureGetError` all accept it.
#[cfg(feature = "gpu-offload")]
fn builtin_submit_method_handle(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = submit_method_dispatch(ctx, args)?;
    Ok(Some(Value::Long(handle as i64)))
}

/// `Native.streamSubmitMethod(long streamHandle, String className,
/// String methodName, String descriptor, Object[] args) -> long`
///
/// The same dispatch as `submitMethodHandle`, onto a caller-named stream
/// instead of the submitting executor's default one, and answering the bare
/// submission handle.
///
/// Kernels submitted to one stream run in submission order, so a caller can
/// queue a chain and wait once on the last handle. That guarantee is what the
/// whole fire-and-forget path on the Java side rests on, and until this existed
/// there was no way to name the stream it applied to: `GpuStream` was a handle
/// and a `close()` with nothing that accepted it.
///
/// A `streamHandle` of 0 means "no particular stream", matching what
/// `gpu_dispatch_method_on_stream` already does with `None`.
#[cfg(feature = "gpu-offload")]
fn builtin_stream_submit_method(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = submit_method_dispatch_on(ctx, args, StreamSource::Explicit)?;
    Ok(Some(Value::Long(handle as i64)))
}

#[cfg(feature = "gpu-offload")]
fn builtin_submit_method(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let timed = dispatch_timing::enabled();
    let handle = submit_method_dispatch(ctx, args)?;
    let mark = std::time::Instant::now();
    let wrapper =
        instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", handle);
    if timed {
        dispatch_timing::add(7, mark.elapsed().as_nanos() as u64);
    }
    wrapper
}

/// The dispatch itself, shared by both entry points: read the three
/// string params, convert the `Object[]`, resolve the stream, and hand
/// the work to the VM. Answers the submission handle; a synthetic
/// failure handle carries the reason for the Java side to read back.
#[cfg(feature = "gpu-offload")]
/// Where the stream for a dispatch comes from.
///
/// The two `submitMethod` shapes differ only in this. `Native.submitMethod`
/// and `Native.submitMethodHandle` take an executor handle and run on that
/// executor's default stream; `Native.streamSubmitMethod` takes the stream
/// directly, which is what lets a caller order several kernels against each
/// other and wait once.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy)]
enum StreamSource {
    /// Argument 0 is an executor handle; use (or create) its default stream.
    ExecutorDefault,
    /// Argument 0 is the stream handle itself.
    Explicit,
}

#[cfg(feature = "gpu-offload")]
fn submit_method_dispatch(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> Result<u64, cratonvm_types::error::MethodCallFailed> {
    submit_method_dispatch_on(ctx, args, StreamSource::ExecutorDefault)
}

#[cfg(feature = "gpu-offload")]
fn submit_method_dispatch_on(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
    stream_source: StreamSource,
) -> Result<u64, cratonvm_types::error::MethodCallFailed> {
    let handle0 = arg_long(args, 0) as u64;
    let timed = dispatch_timing::enabled();
    let mut mark = std::time::Instant::now();
    if timed {
        dispatch_timing::note_call();
    }

    // Read the three string params. If any is null/unreadable, fail
    // synthetically and let the Java side surface it.
    let class_name = match arg_object(args, 1).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(record_failed_future_with_message("submitMethod: className was null"));
        }
    };
    let method_name = match arg_object(args, 2).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(record_failed_future_with_message("submitMethod: methodName was null"));
        }
    };
    let descriptor = match arg_object(args, 3).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(record_failed_future_with_message("submitMethod: descriptor was null"));
        }
    };

    if timed {
        dispatch_timing::add(0, mark.elapsed().as_nanos() as u64);
        mark = std::time::Instant::now();
    }

    // Convert the Object[] argument array into a Vec<Value>. Each
    // slot is read via NativeContext::get_array_element so the JVM
    // layer can unbox / reference-pass as it normally would for a
    // varargs call.
    let java_args_obj = match arg_object(args, 4) {
        Some(o) => o,
        None => {
            return Ok(record_failed_future_with_message("submitMethod: args array was null"));
        }
    };
    let n = ctx.array_length(java_args_obj);
    let mut java_args: Vec<Value> = Vec::with_capacity(n);
    for i in 0..n {
        java_args.push(ctx.get_array_element(java_args_obj, i));
    }

    if timed {
        dispatch_timing::add(1, mark.elapsed().as_nanos() as u64);
        mark = std::time::Instant::now();
    }

    // Dispatch via the NativeContext escape hatch. The VM's impl
    // calls into `runtime::offload::dispatch_method_from_native_on_stream`.
    let stream = match stream_source {
        StreamSource::ExecutorDefault => resolve_or_create_default_stream(ctx, handle0),
        // A stream handle of 0 is "no particular stream": gpu_dispatch_method_on_stream
        // reads None as "a fresh private one-shot stream for this dispatch", which is
        // what the un-streamed entry points get anyway.
        StreamSource::Explicit if handle0 == 0 => None,
        StreamSource::Explicit => Some(handle0),
    };
    let submission_handle = match ctx.gpu_dispatch_method_on_stream(
        &class_name,
        &method_name,
        &descriptor,
        &java_args,
        stream,
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

    if timed {
        dispatch_timing::add(6, mark.elapsed().as_nanos() as u64);
    }
    Ok(submission_handle)
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

/// Twin of the shim above for the handle-returning entry point.
#[cfg(not(feature = "gpu-offload"))]
fn builtin_submit_method_handle(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(Some(cratonvm_types::Value::Long(0)))
}

#[cfg(feature = "gpu-offload")]
fn record_failed_future_with_message(message: &str) -> u64 {
    state::with(|s| {
        let h = s.fresh_handle();
        s.futures.insert(
            h,
            state::FutureState::Failed {
                message: message.to_string(),
            },
        );
        h
    })
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// `Native.newStream(long execHandle) -> GpuStream`
///
/// Mints a REAL CUDA stream via `ctx.gpu_stream_create()`
/// (`OffloadCache::stream_create`) when a device is available, and
/// wraps that handle in a `GpuStreamImpl` — `GpuStream.handle()` is
/// now a genuine, dispatchable `OffloadCache` stream handle, not pure
/// bookkeeping. `None` (no driver, or `gpu-offload` off on the VM
/// side) falls back to a purely local synthetic handle so
/// `newStream()` still never fails outright — `GpuStreamImpl` still
/// round-trips through `closeStream`, matching every other
/// stub-executor "inert but doesn't throw" contract in this file.
///
/// Reachability note: per `gpu/phase3-spec.md` §2.2,
/// `GpuStream`'s only members are `handle()` and `close()` — there is
/// no `submit`/`synchronize` in the implemented spec or anywhere in
/// the registered `Native.*` surface, so nothing today lets Java code
/// aim a dispatch at THIS explicit stream (as opposed to an
/// executor's lazily-created default stream — see
/// `resolve_or_create_default_stream`, which every
/// `submit`/`submitMethod`/`launch`/`submitWithArg(s)` call already
/// routes through). Reaching that would need the external
/// `craton-gpu-java` library to add a stream-scoped submit
/// declaration; the handle this mints is ready to be resolved via
/// `OffloadCache::resolve_stream` the moment one exists.
#[cfg(feature = "gpu-offload")]
fn builtin_new_stream(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let handle = match ctx.gpu_stream_create() {
        Some(real_handle) => state::with(|s| {
            s.streams.insert(real_handle, exec);
            real_handle
        }),
        None => state::with(|s| {
            let h = s.fresh_handle();
            s.streams.insert(h, exec);
            h
        }),
    };
    instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuStreamImpl", handle)
}

/// `Native.closeStream(long streamHandle)`
///
/// Releases the real `OffloadCache`-registered stream (if `handle`
/// names one — `ctx.gpu_stream_release` is a safe no-op on a
/// no-device-fallback synthetic handle) in addition to the local
/// bookkeeping entry.
#[cfg(feature = "gpu-offload")]
fn builtin_close_stream(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    state::with(|s| {
        s.streams.remove(&handle);
    });
    ctx.gpu_stream_release(handle);
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
/// `Native.futureCancel(long handle, boolean mayInterruptIfRunning) -> int`
///
/// Returns `1` if the cancellation request was accepted and `0` if it was
/// rejected. This implementation always answers `0`.
///
/// That is not a stub: there is no device-side cancellation primitive anywhere
/// in this workspace. `NativeContext` exposes dispatch, status, synchronize,
/// take-result and release for a GPU submission, and nothing that revokes one.
/// CUDA itself offers no way to abort a launched kernel short of tearing down
/// the context, which would take every other submission on the device with it.
///
/// Answering `0` is the honest report of that, and it is exactly the contract
/// the Java side documents: `GpuFuture.cancel` returns `false` when the work
/// "could not be cancelled for some other GPU-specific reason (e.g. ... no
/// cancellation primitive is implemented for this future kind)".
///
/// What matters is that the method is *registered*. Before this, `cancel()`
/// raised `UnsatisfiedLinkError: Native.futureCancel` -- a hard failure out of
/// a method whose whole documented behaviour is to answer `false` when it
/// cannot do the job.
///
/// When a cancellation primitive does land, this is the single place to change:
/// accept the request, mark the submission failed so `futureStatus` reports
/// `2`, and return `1`.
#[cfg(feature = "gpu-offload")]
fn builtin_future_cancel(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(Some(Value::Int(0)))
}

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
        // 2026-07-11: a completed scalar-return kernel reports the same
        // DONE code as the object-result `Done` shape above — only the
        // payload representation differs (see `builtin_future_get_result`).
        Some(state::FutureState::DoneScalar { .. }) => 1,
        Some(state::FutureState::Failed { .. }) => 2,
        None => 3,
    });
    Ok(Some(Value::Int(code)))
}

/// `Native.futureIsDone(long futureHandle) -> boolean`
///
/// 2026-07-11 — dedicated non-blocking completion probe, additive
/// alongside `futureStatus` (which is left untouched: `futureStatus`
/// keeps backing `get()`'s failure check and the Java-side
/// `isDone()`/`getNow()` spec shape unchanged; see the module doc for
/// the exact `Native.futureStatus(handle) != 0` wiring the Phase-3.5
/// spec documents). This gives the Java surface a purpose-built
/// boolean entry point to switch `isDone()`/`getNow()` to, so the
/// intent ("did the device finish?") is not overloaded onto the
/// 4-way status code.
///
/// Prefers the real submission registry via
/// `NativeContext::gpu_future_status`, same as `futureStatus` above.
/// That accessor's VM override now calls `offload::poll_submission_status`
/// (see the module doc's "real-submission scalar results" note), so
/// this shim has genuine non-blocking-with-finalize semantics for a
/// real dispatch: once the device reports the kernel done, the very
/// next `futureIsDone` call observes `true` — no separate blocking call
/// is needed to make that transition visible.
///
/// Mapping: Running(0) => false; Completed(1) / Failed(2) => true.
/// Falls back to the local synthetic future map (same fallback shape
/// as `futureStatus`) for handles that never reached the real
/// dispatcher. An unrecognized handle in *both* registries returns
/// `false` — the same "absent => false" convention `arrayIsResident`
/// uses for a released/never-seen array handle, since a plain
/// `boolean` return has no slot for a dedicated UNKNOWN code the way
/// `futureStatus`'s `3` does.
#[cfg(feature = "gpu-offload")]
fn builtin_future_is_done(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let done = if let Some(code) = ctx.gpu_future_status(handle) {
        code != 0
    } else {
        state::with(|s| match s.futures.get(&handle) {
            Some(state::FutureState::Pending) => false,
            Some(state::FutureState::Done { .. }) => true,
            Some(state::FutureState::DoneScalar { .. }) => true,
            Some(state::FutureState::Failed { .. }) => true,
            None => false,
        })
    };
    Ok(Some(Value::Int(if done { 1 } else { 0 })))
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
    //
    // The failure message is REMEMBERED here rather than discarded.
    // `futureGetErrorMessage` reads the synthetic future store, which a
    // real dispatch never writes to, so a real submission that failed
    // reported its reason as `null` and `GpuFuture.get()` threw the
    // placeholder "kernel failed" — with the actual reason (a bounds
    // deopt, an unresolvable kernel, a launch error) already computed
    // and then dropped one frame earlier. Memoising it into the same
    // store the getter reads keeps both paths on one lookup.
    if let Some(Err(message)) = ctx.gpu_future_synchronize(handle) {
        state::with(|s| {
            s.futures
                .insert(handle, state::FutureState::Failed { message });
        });
    }
    Ok(None)
}

/// Box a scalar kernel-return `Value` into its Java wrapper object
/// (`java.lang.Integer` / `Long` / `Float` / `Double`).
///
/// Reuses the crate's canonical `lang_class::box_value` helper — the
/// exact boxing path `lang_invoke.rs` already uses for reflective /
/// MethodHandle scalar returns — rather than hand-rolling a second
/// allocator. Mirrors the four `SerializedResult::Scalar{I32,I64,F32,
/// F64}` variants in `vm::runtime::offload`: the `type_desc` fed to
/// `box_value` is derived from the `Value`'s own discriminant, so a
/// `Value::Long` always becomes a `java.lang.Long`, never an `Integer`.
/// Any non-scalar `Value` (e.g. an already-boxed `Object`, which
/// `FutureState::DoneScalar` should never hold, but a defensive
/// pass-through is cheap) is returned unchanged.
#[cfg(feature = "gpu-offload")]
fn box_scalar_result(ctx: &mut dyn cratonvm_native_api::NativeContext, value: Value) -> Value {
    let type_desc = match value {
        Value::Int(_) => "I",
        Value::Long(_) => "J",
        Value::Float(_) => "F",
        Value::Double(_) => "D",
        _ => return value,
    };
    crate::lang_class::box_value(ctx, value, type_desc)
}

/// `Native.futureGetResult(long futureHandle) -> Object`
///
/// 2026-07-11 — tries the real submission registry FIRST via
/// `NativeContext::gpu_future_take_result`. That accessor is
/// non-blocking (see its doc comment): it returns `Some` only once the
/// submission is observably complete, `None` for `Running` (or
/// `Failed`, or an unknown handle). This is safe to call unconditionally
/// here because the two ways Java reaches `futureGetResult` both
/// already guarantee completion (or accept a `null`/empty answer) by
/// this point:
///   * `GpuFuture.get()` calls the blocking `Native.futureSynchronize`
///     first (unchanged — see `builtin_future_synchronize`), so the
///     submission is always terminal by the time `futureGetResult` runs.
///   * `GpuFuture.getNow()` / `Optional<T>` peeks are only meaningful
///     after the caller observed `isDone()`/`futureStatus` report
///     completion; a `Running` submission correctly yields `null` here
///     rather than blocking.
///
/// `Some(GpuFutureResult::Scalar*)` is boxed via `box_scalar_result`.
/// `Some(GpuFutureResult::Void)` — a void-return kernel, or one whose
/// result went to a caller-owned array via writeback rather than the
/// future's result slot — maps to the same `null` convention every
/// other "no object result" case below uses.
///
/// Falls back to the local synthetic `FutureState` map (`Done` /
/// `DoneScalar`) when the real registry has nothing for this handle —
/// every stub-mode fixture (`builtin_submit`/`builtin_launch` records
/// that never reached the real dispatcher) still round-trips exactly as
/// before. `Done` futures return the stored mirror `ObjectRef` (e.g. a
/// primitive array a `Void`-return kernel wrote into); `DoneScalar`
/// futures get the same `box_scalar_result` boxing as the real path;
/// anything else (`Pending`, `Failed`, unknown) is `null`.
#[cfg(feature = "gpu-offload")]
fn builtin_future_get_result(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;

    if let Some(real_result) = ctx.gpu_future_take_result(handle) {
        let result = match real_result {
            cratonvm_native_api::registry::GpuFutureResult::Void => Value::Object(None),
            cratonvm_native_api::registry::GpuFutureResult::ScalarI32(v) => {
                box_scalar_result(ctx, Value::Int(v))
            }
            cratonvm_native_api::registry::GpuFutureResult::ScalarI64(v) => {
                box_scalar_result(ctx, Value::Long(v))
            }
            cratonvm_native_api::registry::GpuFutureResult::ScalarF32(v) => {
                box_scalar_result(ctx, Value::Float(v))
            }
            cratonvm_native_api::registry::GpuFutureResult::ScalarF64(v) => {
                box_scalar_result(ctx, Value::Double(v))
            }
        };
        return Ok(Some(result));
    }

    let stored = state::with(|s| match s.futures.get(&handle) {
        Some(state::FutureState::Done { result_obj }) => Some(Value::Object(*result_obj)),
        Some(state::FutureState::DoneScalar { value }) => Some(*value),
        _ => None,
    });
    let result = match stored {
        Some(v @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_))) => {
            box_scalar_result(ctx, v)
        }
        Some(already_boxed) => already_boxed,
        None => Value::Object(None),
    };
    Ok(Some(result))
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

/// Shared implementation for `Native.arrayAllocate{Int,Long,Float,
/// Double}` — mints a device-only array handle with a zero-filled
/// host-bytes mirror, `element_bytes` per element (4 for int/float, 8
/// for long/double). No `NativeContext` calls are needed: like
/// `wrap_primitive_array`, this only ever touches the local `state`
/// store; the Java array (if any) is materialized lazily by
/// `arrayToHost`.
///
/// `length` (Java `int`, so it can be negative) is validated first: a
/// negative value returns the sentinel handle `0` without minting a
/// state entry — the same bad-arg shape `wrap_primitive_array` uses for
/// a null host array — rather than `as usize`-wrapping into an
/// enormous allocation.
#[cfg(feature = "gpu-offload")]
fn allocate_primitive_array(
    element_type: ArrayElementType,
    element_bytes: usize,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let length = arg_int(args, 0);
    if length < 0 {
        return Ok(Some(Value::Long(0)));
    }
    let element_count = length as usize;
    let bytes = vec![0u8; element_count * element_bytes];
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

/// `Native.arrayAllocateInt(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_int(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ArrayElementType::Int, 4, args)
}

/// `Native.arrayAllocateLong(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_long(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ArrayElementType::Long, 8, args)
}

/// `Native.arrayAllocateFloat(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_float(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ArrayElementType::Float, 4, args)
}

/// `Native.arrayAllocateDouble(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_double(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ArrayElementType::Double, 8, args)
}

/// `Native.arrayToHost(long arrayHandle) -> Object`
///
/// Looks up the handle in our state and rebuilds a fresh Java primitive
/// array of the same shape from the stored bytes.
#[cfg(feature = "gpu-offload")]
/// `Native.arrayToHostInto(long arrayHandle, Object dest) -> boolean`
///
/// Reads the array back into `dest` rather than into a freshly allocated one,
/// and answers whether it did.
///
/// `arrayToHost` allocates a Java array per call. `GpuArray.toHost(dest)` then
/// copies out of that array and drops it, so the buffer-reusing overloads on
/// the Java side only ever bounded the *caller's* garbage — the full-size
/// allocation still happened here, on every read-back, in the hot loop those
/// overloads exist for.
///
/// Answers `false` rather than throwing when it cannot help: an unknown handle,
/// a `dest` that is not an array, or one shorter than the stored element count.
/// The Java side treats `false` as "fall back to `arrayToHost`", so a mismatch
/// degrades to the previous behaviour instead of failing the read.
#[cfg(feature = "gpu-offload")]
fn builtin_array_to_host_into(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let dest = match arg_object(args, 1) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };

    // Same as arrayToHost: settle any deferred device-to-host writeback before
    // reading the resident store.
    if let Some(fresh_bytes) = ctx.gpu_array_download_if_dirty(handle) {
        array_replace_bytes(handle, fresh_bytes);
    }
    let snapshot = state::with(|s| {
        s.arrays
            .get(&handle)
            .map(|entry| (entry.element_type, entry.element_count, entry.bytes.clone()))
    });
    let (etype, count, bytes) = match snapshot {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    if ctx.array_length(dest) < count {
        return Ok(Some(Value::Int(0)));
    }
    fill_java_array(ctx, dest, etype, count, &bytes);
    Ok(Some(Value::Int(1)))
}

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
        s.arrays
            .get(&handle)
            .map(|entry| (entry.element_type, entry.element_count, entry.bytes.clone()))
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
    let resident = state::with(|s| s.arrays.get(&handle).map(|e| e.resident).unwrap_or(false));
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::MockNativeContext;
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

    /// `state::STATE` is one process-wide store shared by every test in
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
        // `ResidencyTracker::release`'s documented idempotency
        // (`vm/src/runtime/gpu_residency.rs`) and `releaseFuture`'s
        // remove-is-a-no-op-on-missing-key shape used elsewhere in this
        // file.
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
}

/// Where the time in one `submitMethod` actually goes.
///
/// An inference step is hundreds of dispatches, so the per-dispatch
/// floor is multiplied by hundreds before any kernel runs — and two
/// rounds of plausible guessing (a pooled failure-flag buffer, a
/// memoised occupancy query) moved 117 us to 100 us, which is what
/// guessing usually buys. This exists so the next change is aimed.
///
/// Off unless `CRATONVM_GPU_TIME_DISPATCH=1`; the counters are plain
/// relaxed atomics and the report prints at VM shutdown beside the
/// other censuses.
#[cfg(feature = "gpu-offload")]
pub mod dispatch_timing {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub const PHASES: [&str; 8] = [
        "read_strings",
        "read_args",
        "resolve_method",
        "lookup_kernel",
        "marshal_args",
        "failure_flag",
        // NESTED: this one spans the whole VM-side dispatch, so it
        // CONTAINS resolve_method, lookup_kernel, marshal_args and
        // failure_flag. Read it as the total and those four as its
        // parts; what it holds beyond their sum is the launch itself.
        "vm_dispatch_all",
        "future_object",
    ];

    static NANOS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    static CALLS: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_GPU_TIME_DISPATCH")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    pub fn add(phase: usize, nanos: u64) {
        if phase < NANOS.len() {
            NANOS[phase].fetch_add(nanos, Ordering::Relaxed);
        }
    }

    pub fn note_call() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn report() {
        let calls = CALLS.load(Ordering::Relaxed);
        if calls == 0 {
            return;
        }
        let total: u64 = NANOS.iter().map(|n| n.load(Ordering::Relaxed)).sum();
        eprintln!(
            "[cratonvm] gpu dispatch: calls={calls} accounted={:.1} us/call",
            total as f64 / calls as f64 / 1000.0
        );
        for (i, name) in PHASES.iter().enumerate() {
            let n = NANOS[i].load(Ordering::Relaxed);
            if n == 0 {
                continue;
            }
            eprintln!(
                "[cratonvm] gpu dispatch:   {name:<15} {:>8.2} us/call",
                n as f64 / calls as f64 / 1000.0
            );
        }
    }
}

/// Stub so the reporting call site needs no `cfg`.
#[cfg(not(feature = "gpu-offload"))]
pub mod dispatch_timing {
    pub fn report() {}
}
