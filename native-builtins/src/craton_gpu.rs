// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Native shims for `craton.gpu.internal.Native` (Phase 3 — Item P3-4).
//!
//! These are the Rust-side handlers behind every `Native.*` method the
//! Phase-3 Java surface declares (see `phase3-spec.md` §2.2).
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
//! `gpu-offload-followups-20260711.md` items #1/#3).
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
//! array). Neither `phase3-spec.md` §2.2 nor any
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
use std::collections::{HashMap, HashSet, hash_map::Entry};
#[cfg(feature = "gpu-offload")]
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "gpu-offload")]
use cratonvm_native_api::registry::{DeviceArrayElem, GpuFutureResult};
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
pub fn register(registry: &mut NativeMethodRegistry) {
    const KLASS: &str = "craton/gpu/internal/Native";
    let kind = cratonvm_native_api::NativeKind::Bridge;

    // Device enumeration — delegates to `cuda_bridge::probe()` via the
    // `NativeContext::gpu_device_info` escape hatch. Truthful on a
    // driverless host: `deviceCount` returns 0 and the per-device
    // queries return null / 0 for every ordinal.
    registry.register(KLASS, "deviceCount", "()I", builtin_device_count, kind);
    registry.register(
        KLASS,
        "deviceName",
        "(I)Ljava/lang/String;",
        builtin_device_name,
        kind,
    );
    registry.register(
        KLASS,
        "deviceTotalMemory",
        "(I)J",
        builtin_device_total_memory,
        kind,
    );
    registry.register(
        KLASS,
        "deviceComputeCapability",
        "(I)I",
        builtin_device_compute_capability,
        kind,
    );

    registry.register(
        KLASS,
        "openExecutor",
        "(I)Lcraton/gpu/GpuExecutor;",
        builtin_open_executor,
        kind,
    );
    registry.register(
        KLASS,
        "submit",
        "(JLcraton/gpu/GpuCallable;)Lcraton/gpu/GpuFuture;",
        builtin_submit,
        kind,
    );
    registry.register(
        KLASS,
        "launch",
        "(JLcraton/gpu/GpuRunnable;)Lcraton/gpu/GpuFuture;",
        builtin_launch,
        kind,
    );
    registry.register(
        KLASS,
        "submitMethod",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_method,
        kind,
    );
    // Fire-and-forget: the same dispatch, answering the submission
    // handle rather than a `GpuFuture`. See `builtin_submit_method_handle`
    // for why the object is worth avoiding.
    registry.register(
        KLASS,
        "submitMethodHandle",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)J",
        builtin_submit_method_handle,
        kind,
    );
    registry.register(
        KLASS,
        "submitWithArg",
        "(JLjava/lang/Object;Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_with_arg,
        kind,
    );
    registry.register(
        KLASS,
        "submitWithArgs",
        "(JLjava/lang/Object;[Ljava/lang/Object;)Lcraton/gpu/GpuFuture;",
        builtin_submit_with_args,
        kind,
    );
    registry.register(
        KLASS,
        "newStream",
        "(J)Lcraton/gpu/GpuStream;",
        builtin_new_stream,
        kind,
    );
    registry.register(KLASS, "closeStream", "(J)V", builtin_close_stream, kind);
    registry.register(
        KLASS,
        "streamSynchronize",
        "(J)V",
        builtin_stream_synchronize,
        kind,
    );
    // Stream-scoped dispatch. `gpu_dispatch_method_on_stream` has always been
    // there and `submit_method_dispatch` has always called it -- but only ever
    // with a stream resolved from the executor, so a caller holding a
    // `GpuStream` could not submit onto it.
    registry.register(
        KLASS,
        "streamSubmitMethod",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)J",
        builtin_stream_submit_method,
        kind,
    );
    registry.register(
        KLASS,
        "futureThenApplyGpu",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)J",
        builtin_future_then_apply_gpu,
        kind,
    );

    registry.register(KLASS, "futureStatus", "(J)I", builtin_future_status, kind);
    registry.register(KLASS, "futureIsDone", "(J)Z", builtin_future_is_done, kind);
    registry.register(
        KLASS,
        "futureSynchronize",
        "(J)V",
        builtin_future_synchronize,
        kind,
    );
    registry.register(
        KLASS,
        "futureGetResult",
        "(J)Ljava/lang/Object;",
        builtin_future_get_result,
        kind,
    );
    registry.register(
        KLASS,
        "futureGetResultAsGpuArray",
        "(J)Lcraton/gpu/GpuArray;",
        builtin_future_get_result_as_gpu_array,
        kind,
    );
    registry.register(
        KLASS,
        "futureGetErrorMessage",
        "(J)Ljava/lang/String;",
        builtin_future_get_error_message,
        kind,
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
        kind,
    );
    // `GpuFuture.cancel(boolean)` calls this. It was never registered, so
    // every cancel attempt on a real CratonVM died with
    // `UnsatisfiedLinkError: Native.futureCancel` instead of returning the
    // documented "could not cancel" answer -- and it died from inside
    // `cancel()`, which the Java side documents as returning `false` rather
    // than throwing. Registered now; see `builtin_future_cancel` for why the
    // answer is always "rejected".
    registry.register(KLASS, "futureCancel", "(JZ)I", builtin_future_cancel, kind);

    registry.register(KLASS, "arrayWrapInt", "([I)J", builtin_array_wrap_int, kind);
    registry.register(KLASS, "arrayWrapLong", "([J)J", builtin_array_wrap_long, kind);
    registry.register(KLASS, "arrayWrapShort", "([S)J", builtin_array_wrap_short, kind);
    registry.register(KLASS, "arrayWrapFloat", "([F)J", builtin_array_wrap_float, kind);
    registry.register(KLASS, "arrayWrapDouble", "([D)J", builtin_array_wrap_double, kind);
    // 2026-07-11: device-only allocation (`GpuArray.allocate`, no host
    // source array) — see the module doc's "device-only array
    // allocation" note for why these names/descriptors are new rather
    // than spec-derived.
    registry.register(
        KLASS,
        "arrayAllocateInt",
        "(I)J",
        builtin_array_allocate_int,
        kind,
    );
    registry.register(
        KLASS,
        "arrayAllocateLong",
        "(I)J",
        builtin_array_allocate_long,
        kind,
    );
    registry.register(
        KLASS,
        "arrayAllocateShort",
        "(I)J",
        builtin_array_allocate_short,
        kind,
    );
    registry.register(
        KLASS,
        "arrayAllocateFloat",
        "(I)J",
        builtin_array_allocate_float,
        kind,
    );
    registry.register(
        KLASS,
        "arrayAllocateDouble",
        "(I)J",
        builtin_array_allocate_double,
        kind,
    );
    registry.register(
        KLASS,
        "arrayToHost",
        "(J)Ljava/lang/Object;",
        builtin_array_to_host,
        kind,
    );
    // Read back into the caller's array instead of a fresh one. See
    // `builtin_array_to_host_into`: the buffer-reusing GpuArray.toHost(dest)
    // overloads bounded the caller's garbage but not the allocation here.
    registry.register(
        KLASS,
        "arrayToHostInto",
        "(JLjava/lang/Object;)Z",
        builtin_array_to_host_into,
        kind,
    );
    registry.register(KLASS, "arrayIsResident", "(J)Z", builtin_array_is_resident, kind);
    registry.register(
        KLASS,
        "arrayCopyFromHost",
        "(JLjava/lang/Object;)Z",
        builtin_array_copy_from_host,
        kind,
    );

    registry.register(KLASS, "gemm", "(JJJIIIZZZJ)J", builtin_gemm, kind);
    registry.register(KLASS, "futureErrorKind", "(J)I", builtin_future_error_kind, kind);
    registry.register(KLASS, "futureAwait", "(JJ)I", builtin_future_await, kind);
    registry.register(KLASS, "releaseFuture", "(J)V", builtin_release_future, kind);
    registry.register(KLASS, "releaseArray", "(J)V", builtin_release_array, kind);
    registry.register(KLASS, "releaseExecutor", "(J)V", builtin_release_executor, kind);

    // CUDA graph capture and replay. The first argument of the three
    // capture-side entry points is the EXECUTOR handle, not a stream: a
    // capture belongs to the stream every dispatch on that executor lands
    // on, which is the one `resolve_or_create_default_stream` hands out.
    registry.register(
        KLASS,
        "graphBeginCapture",
        "(J)Z",
        builtin_graph_begin_capture,
        kind,
    );
    registry.register(KLASS, "graphEndCapture", "(J)J", builtin_graph_end_capture, kind);
    registry.register(KLASS, "graphReplay", "(JJ)J", builtin_graph_replay, kind);
    registry.register(
        KLASS,
        "graphBeginReplay",
        "(JJ)Z",
        builtin_graph_begin_replay,
        kind,
    );
    registry.register(KLASS, "graphEndReplay", "(J)J", builtin_graph_end_replay, kind);
    registry.register(KLASS, "graphNodeCount", "(J)I", builtin_graph_node_count, kind);
    registry.register(KLASS, "releaseGraph", "(J)V", builtin_release_graph, kind);
}

/// No-op registration when the `gpu-offload` feature is disabled.
#[cfg(not(feature = "gpu-offload"))]
pub fn register(_registry: &mut NativeMethodRegistry) {}

// ---------------------------------------------------------------------------
// Graph capture and replay
// ---------------------------------------------------------------------------
//
// Five thin shims. Every one of them resolves the executor's default
// stream and hands the work to the offload cache through a
// `NativeContext` escape hatch; none of them holds state here, because
// the graph itself and the "is this stream capturing" flag both live
// beside the stream in `OffloadCache` and a second copy of either in
// `state` would be a second thing to keep in sync.
//
// The handles are opaque `long`s in both directions, so all five are
// well-defined without a device: a driverless VM answers `false` / `0`
// / `-1` from the `NativeContext` defaults and the Java side reports
// that capture is unavailable, rather than reporting an empty graph
// that replays successfully and runs nothing.

/// `Native.graphBeginCapture(long execHandle) -> boolean`
///
/// `false` if this executor has no stream to capture on, if a capture is
/// already open, or if the driver refuses.
#[cfg(feature = "gpu-offload")]
fn builtin_graph_begin_capture(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let ok = match resolve_or_create_default_stream(ctx, exec) {
        Some(stream) => ctx.gpu_graph_begin_capture(stream),
        None => false,
    };
    Ok(Some(Value::Int(i32::from(ok))))
}

/// `Native.graphEndCapture(long execHandle) -> long`
///
/// The graph handle, or `0` for any failure: nothing was captured, the
/// capture was invalidated, or instantiation failed. Zero is never a
/// valid handle.
#[cfg(feature = "gpu-offload")]
fn builtin_graph_end_capture(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let handle = match resolve_or_create_default_stream(ctx, exec) {
        Some(stream) => ctx.gpu_graph_end_capture(stream),
        None => 0,
    };
    Ok(Some(Value::Long(handle as i64)))
}

/// `Native.graphReplay(long execHandle, long graphHandle) -> long`
///
/// One `cuGraphLaunch` onto the executor's stream. Asynchronous, exactly
/// like a dispatch, and answers a submission handle for the same reason:
/// the caller awaits it before reading what the graph wrote. `0` if the
/// replay was refused.
#[cfg(feature = "gpu-offload")]
fn builtin_graph_replay(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let graph = arg_long(args, 1) as u64;
    let submission = match resolve_or_create_default_stream(ctx, exec) {
        Some(stream) => ctx.gpu_graph_replay(stream, graph),
        None => 0,
    };
    Ok(Some(Value::Long(submission as i64)))
}

/// `Native.graphBeginReplay(long execHandle, long graphHandle) -> boolean`
///
/// Opens an argument-update pass. Between this and `graphEndReplay` the
/// caller re-issues the dispatch sequence it captured, and each dispatch
/// rewrites one node's arguments instead of launching.
#[cfg(feature = "gpu-offload")]
fn builtin_graph_begin_replay(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let graph = arg_long(args, 1) as u64;
    let ok = match resolve_or_create_default_stream(ctx, exec) {
        Some(stream) => ctx.gpu_graph_begin_replay(stream, graph),
        None => false,
    };
    Ok(Some(Value::Int(i32::from(ok))))
}

/// `Native.graphEndReplay(long execHandle) -> long`
///
/// The submission handle for the one launch the whole pass produces, or
/// `0` if the caller's sequence did not match the captured one.
#[cfg(feature = "gpu-offload")]
fn builtin_graph_end_replay(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let submission = match resolve_or_create_default_stream(ctx, exec) {
        Some(stream) => ctx.gpu_graph_end_replay(stream),
        None => 0,
    };
    Ok(Some(Value::Long(submission as i64)))
}

/// `Native.graphNodeCount(long graphHandle) -> int`
///
/// `-1` for an unknown handle. The count a caller checks against the
/// number of dispatches it issued: a graph holding fewer nodes than the
/// caller submitted launches means something was dropped, and that is
/// worth refusing to replay.
#[cfg(feature = "gpu-offload")]
fn builtin_graph_node_count(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let graph = arg_long(args, 0) as u64;
    Ok(Some(Value::Int(ctx.gpu_graph_node_count(graph))))
}

/// `Native.releaseGraph(long graphHandle) -> void`
///
/// Idempotent, and a no-op for an unknown handle.
#[cfg(feature = "gpu-offload")]
fn builtin_release_graph(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    if handle == 0 {
        return Ok(None);
    }
    ctx.gpu_graph_release(handle);
    Ok(None)
}

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
        /// Lazily materialized host mirror. Device-only allocations
        /// (`arrayAllocate*`, promoted `GpuArray` results) keep `None`
        /// until host bytes are explicitly written or snapshot-copied,
        /// avoiding redundant zeroed host allocations for device-resident buffers.
        pub bytes: Option<Vec<u8>>,
        pub resident: bool,
        pub in_flight_pins: usize,
        pub release_requested: bool,
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
            /// Failure category, mirrored from
            /// `cratonvm_native_api::registry::GpuErrorKind`. The
            /// synthetic store has no device behind it, so everything
            /// recorded here is a resolution failure rather than a
            /// device one — but `futureErrorKind` reads this store as
            /// its fallback, so the field has to exist.
            kind: cratonvm_native_api::registry::GpuErrorKind,
        },
    }

    #[derive(Debug, Default)]
    pub(super) struct NativeState {
        pub next_handle: u64,
        pub executors: HashMap<u64, i32>, // handle -> device ordinal
        pub futures: HashMap<u64, FutureState>,
        pub arrays: HashMap<u64, ArrayEntry>,
        /// Cached host-side byte snapshots for materialized ephemeral
        /// `DeviceArray` futures, keyed by `future_handle` and dropped in
        /// `builtin_release_future`. Lets repeated/concurrent `.get()` calls
        /// succeed after the first `gpu_array_download_if_dirty` clears the
        /// `device_cache` dirty bit without evicting the device buffer needed
        /// by `thenApplyGpu` fan-out.
        pub ephemeral_results: HashMap<u64, (ArrayElementType, usize, Vec<u8>)>,
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
        pub synthetic_streams: HashSet<u64>,
        /// Handle -> owning `vm_identity()` for per-VM teardown cleanup.
        pub handle_vms: HashMap<u64, usize>,
        /// Handle -> retained off-heap VRAM estimate (bytes) charged for live `GpuFutureImpl`.
        pub future_vram: HashMap<u64, u64>,
    }

    impl NativeState {
        pub fn fresh_handle(&mut self) -> u64 {
            self.next_handle += 1;
            self.next_handle
        }
    }

    pub(super) struct GpuTimingCounters {
        pub nanos: [std::sync::atomic::AtomicU64; 8],
        pub calls: std::sync::atomic::AtomicU64,
        pub on: OnceLock<bool>,
        pub reported: std::sync::atomic::AtomicBool,
    }

    impl GpuTimingCounters {
        const fn new() -> Self {
            Self {
                nanos: [const { std::sync::atomic::AtomicU64::new(0) }; 8],
                calls: std::sync::atomic::AtomicU64::new(0),
                on: OnceLock::new(),
                reported: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    pub(super) struct CratonGpuStatic {
        pub native: OnceLock<Mutex<NativeState>>,
        pub download_gate: Mutex<()>,
        pub timing: GpuTimingCounters,
    }

    impl CratonGpuStatic {
        const fn new() -> Self {
            Self {
                native: OnceLock::new(),
                download_gate: Mutex::new(()),
                timing: GpuTimingCounters::new(),
            }
        }
    }

    pub(super) static GPU_STATIC: CratonGpuStatic = CratonGpuStatic::new();

    pub(super) fn with<R>(f: impl FnOnce(&mut NativeState) -> R) -> R {
        let m = GPU_STATIC
            .native
            .get_or_init(|| Mutex::new(NativeState::default()));
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
fn resolve_entry_bytes(
    etype: cratonvm_types::ArrayElementType,
    count: usize,
    bytes: Option<Vec<u8>>,
) -> Vec<u8> {
    bytes.unwrap_or_else(|| vec![0u8; count.saturating_mul(element_byte_width(etype))])
}

#[cfg(feature = "gpu-offload")]
static ARRAY_CACHE_RELEASE_HOOKS: std::sync::Mutex<Vec<fn(u64)>> = std::sync::Mutex::new(Vec::new());

#[cfg(feature = "gpu-offload")]
static ARRAY_CACHE_VRAM_RELEASE_HOOKS: std::sync::Mutex<Vec<fn(usize, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// Register a process-wide callback invoked with a `GpuArray` handle when a
/// deferred release finishes inside `InFlightArrayPins::drop` (without a
/// `NativeContext` available to call `ctx.gpu_release_array_cache`).
#[cfg(feature = "gpu-offload")]
pub fn register_array_cache_release_hook(hook: fn(u64)) {
    let mut hooks = ARRAY_CACHE_RELEASE_HOOKS
        .lock()
        .expect("ARRAY_CACHE_RELEASE_HOOKS poisoned");
    if !hooks.iter().any(|&h| h as usize == hook as usize) {
        hooks.push(hook);
    }
}

/// Register a process-wide callback invoked with `(vm_identity, bytes)` when a
/// deferred `GpuArray` release finishes inside `InFlightArrayPins::drop` without
/// a `NativeContext` available to call `ctx.gpu_note_retained_vram`.
#[cfg(feature = "gpu-offload")]
pub fn register_array_cache_vram_release_hook(hook: fn(usize, usize)) {
    let mut hooks = ARRAY_CACHE_VRAM_RELEASE_HOOKS
        .lock()
        .expect("ARRAY_CACHE_VRAM_RELEASE_HOOKS poisoned");
    if !hooks.iter().any(|&h| h as usize == hook as usize) {
        hooks.push(hook);
    }
}

#[cfg(feature = "gpu-offload")]
fn notify_array_cache_release(handle: u64) {
    let hooks: Vec<fn(u64)> = {
        let guard = ARRAY_CACHE_RELEASE_HOOKS
            .lock()
            .expect("ARRAY_CACHE_RELEASE_HOOKS poisoned");
        guard.clone()
    };
    for hook in hooks {
        hook(handle);
    }
}

#[cfg(feature = "gpu-offload")]
fn notify_array_cache_vram_release(vm_identity: usize, bytes: usize) {
    let hooks: Vec<fn(usize, usize)> = {
        let guard = ARRAY_CACHE_VRAM_RELEASE_HOOKS
            .lock()
            .expect("ARRAY_CACHE_VRAM_RELEASE_HOOKS poisoned");
        guard.clone()
    };
    for hook in hooks {
        hook(vm_identity, bytes);
    }
}

/// Drop every native GPU executor, stream, array, and future handle owned by
/// `vm_identity` when that VM shuts down.
#[cfg(feature = "gpu-offload")]
pub fn clear_vm_state(vm_identity: usize) {
    state::with(|s| {
        let owned: Vec<u64> = s
            .handle_vms
            .iter()
            .filter_map(|(&h, &vm)| (vm == vm_identity).then_some(h))
            .collect();
        if owned.is_empty() {
            return;
        }
        let owned_set: std::collections::HashSet<u64> = owned.iter().copied().collect();
        for &h in &owned {
            s.handle_vms.remove(&h);
            s.executors.remove(&h);
            s.futures.remove(&h);
            s.future_vram.remove(&h);
            s.arrays.remove(&h);
            s.ephemeral_results.remove(&h);
            s.streams.remove(&h);
            s.synthetic_streams.remove(&h);
            s.executor_default_stream.remove(&h);
        }
        s.streams.retain(|_, exec| !owned_set.contains(exec));
        s.executor_default_stream
            .retain(|exec, sh| !owned_set.contains(exec) && !owned_set.contains(sh));
    });
}

#[cfg(feature = "gpu-offload")]
pub fn array_snapshot(handle: u64) -> Option<(cratonvm_types::ArrayElementType, usize, Vec<u8>)> {
    let raw = state::with(|s| {
        s.arrays
            .get(&handle)
            .map(|e| (e.element_type, e.element_count, e.bytes.clone()))
    });
    raw.map(|(et, count, bytes)| (et, count, resolve_entry_bytes(et, count, bytes)))
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
            e.bytes = Some(bytes);
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
    if exec == 0 {
        return None;
    }
    if let Some(h) = state::with(|s| s.executor_default_stream.get(&exec).copied()) {
        return Some(h);
    }
    let created = ctx.gpu_stream_create()?;
    let (winner, duplicate) = state::with(|s| match s.executor_default_stream.entry(exec) {
        Entry::Occupied(entry) => (*entry.get(), Some(created)),
        Entry::Vacant(entry) => {
            entry.insert(created);
            (created, None)
        }
    });
    if let Some(dup) = duplicate {
        ctx.gpu_stream_release(dup);
    }
    Some(winner)
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
    // Bulk first. The per-element arms below are the fallback for a
    // context with no bulk path (the mock contexts the unit tests use),
    // and they are also what this did exclusively until 2026-08-29.
    //
    // That mattered more than it looks. `GpuArray.toHost` on a 1024x1024
    // float result made 1,048,576 `set_array_element` calls at roughly
    // 14 ns each: 14.6 ms to hand back a 4 MB array, against a 0.7 ms
    // GEMM that produced it. The marshalling was twenty times the kernel,
    // and it hid two real kernel optimisations inside the run-to-run
    // noise of the end-to-end benchmark.
    //
    // Native-endian on both sides, so this is one memcpy.
    let want = element_count * element_byte_width(element_type);
    if want > 0 && bytes.len() >= want && ctx.write_primitive_array_bytes(obj, 0, &bytes[..want]) {
        return;
    }

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
        ArrayElementType::Short => {
            for i in 0..element_count {
                let off = i * 2;
                if off + 2 > bytes.len() {
                    break;
                }
                let v = i16::from_ne_bytes(bytes[off..off + 2].try_into().unwrap());
                ctx.set_array_element(obj, i, Value::Int(v as i32));
            }
        }
        _ => {
            // Reference / Boolean / Char / Byte. If a caller ever lands
            // here, leave the array zero-initialized.
            //
            // AUDIT 2026-08-29: `Short` used to be in this list, and
            // silently doing nothing was much worse than it looks. The
            // fp16 element type is a `short[]` of binary16 bit patterns,
            // so `arrayWrapShort` recorded a length with an EMPTY byte
            // vector, the GEMM path uploaded a zero-length device buffer,
            // and the kernel read past the end of it —
            // CUDA_ERROR_ILLEGAL_ADDRESS, surfacing several calls later at
            // an unrelated `releaseArray`. The snapshot side is below.
        }
    }
}

/// Bytes per element of a primitive array type, or `0` for one the GPU
/// marshalling does not handle.
///
/// Zero doubles as "no bulk path for this": the callers use it to decide
/// whether a byte-level copy is even meaningful, and a reference array
/// must never take one — writing raw bytes over object references would
/// hand the collector pointers it never issued.
#[cfg(feature = "gpu-offload")]
fn element_byte_width(element_type: ArrayElementType) -> usize {
    match element_type {
        ArrayElementType::Byte | ArrayElementType::Boolean => 1,
        ArrayElementType::Short | ArrayElementType::Char => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        ArrayElementType::Reference => 0,
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

    // Bulk first, for the same reason as `fill_java_array`: this used to
    // be a per-element read for every type but `int`, so uploading a
    // weight matrix cost a VM call per element. The arms below remain the
    // fallback for a context without the bulk path.
    let width = element_byte_width(element_type);
    if width > 0 && length > 0 {
        let mut bulk = vec![0u8; length * width];
        let got = ctx.read_primitive_array_bytes(array, 0, &mut bulk);
        if got == bulk.len() {
            return (element_type, length, bulk);
        }
    }

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
        ArrayElementType::Short => {
            // The fp16 element type: binary16 bit patterns carried in a
            // Java `short[]`. Nothing here interprets them.
            bytes.reserve_exact(length * 2);
            for i in 0..length {
                // The VM widens a short to Value::Int on read, as the JVM
                // does; narrowing back is what stores the two bytes.
                if let Value::Int(v) = ctx.get_array_element(array, i) {
                    bytes.extend_from_slice(&(v as i16).to_ne_bytes());
                } else {
                    bytes.extend_from_slice(&0i16.to_ne_bytes());
                }
            }
        }
        _ => {
            // Reference arrays are out-of-scope for the Phase-3 surface.
            //
            // Note that landing here yields an EMPTY byte vector alongside
            // a non-zero length, which every caller has to be ready for —
            // see the audit note in `fill_java_array` for what that cost
            // when `Short` was in this arm.
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
    // mcs21 (JNILAZY21-4): `new_object_initialized` roots the new object across
    // `<init>` (which registers with `StreamCleaner`, so it allocates) and
    // answers the FORWARDED reference; the `new_object` + `invoke("<init>")`
    // pair this replaced returned the pre-constructor address, stale after a
    // moving collection inside the constructor.
    match ctx.new_object_initialized(class_name, "(J)V", &[Value::Long(handle as i64)])? {
        Some(Value::Object(Some(obj))) => Ok(Some(Value::Object(Some(obj)))),
        _ => Ok(Some(Value::Object(None))),
    }
}

#[cfg(feature = "gpu-offload")]
fn wrap_future_or_release(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    handle: u64,
) -> cratonvm_types::error::MethodCallResult {
    match instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuFutureImpl", handle) {
        Ok(Some(Value::Object(Some(obj)))) => {
            if handle != 0 {
                let vm_id = ctx.vm_identity();
                let retained = ctx.gpu_submission_retained_vram(handle);
                state::with(|s| {
                    s.handle_vms.insert(handle, vm_id);
                    s.future_vram.insert(handle, retained);
                });
                let _ = ctx.gpu_note_retained_vram(i64::try_from(retained).unwrap_or(i64::MAX), 1);
            }
            Ok(Some(Value::Object(Some(obj))))
        }
        Ok(other) => {
            if handle != 0 {
                state::with(|s| {
                    s.futures.remove(&handle);
                    s.ephemeral_results.remove(&handle);
                    s.handle_vms.remove(&handle);
                });
                ctx.gpu_release_submission(handle);
            }
            Ok(other)
        }
        Err(err) => {
            if handle != 0 {
                state::with(|s| {
                    s.futures.remove(&handle);
                    s.ephemeral_results.remove(&handle);
                    s.handle_vms.remove(&handle);
                });
                ctx.gpu_release_submission(handle);
            }
            Err(err)
        }
    }
}

#[cfg(feature = "gpu-offload")]
fn read_object_handle_if_class(
    ctx: &dyn cratonvm_native_api::NativeContext,
    val: Value,
    expected_class: &str,
) -> Option<u64> {
    let Value::Object(Some(obj)) = val else {
        return None;
    };
    let cid = ctx.class_id_of_object(obj);
    if ctx.class_name_of_id(cid).as_deref() != Some(expected_class) {
        return None;
    }
    match ctx.get_field_by_name(obj, "handle") {
        Value::Long(h) if h != 0 => Some(h as u64),
        _ => None,
    }
}

#[cfg(feature = "gpu-offload")]
fn release_pinned_array_handles(handles: &[u64]) -> Vec<(u64, usize, Option<usize>)> {
    let mut to_evict = Vec::new();
    state::with(|s| {
        for &h in handles {
            let should_remove = if let Some(entry) = s.arrays.get_mut(&h) {
                entry.in_flight_pins = entry.in_flight_pins.saturating_sub(1);
                entry.in_flight_pins == 0 && entry.release_requested
            } else {
                false
            };
            if should_remove {
                let vm_id = s.handle_vms.remove(&h);
                if let Some(entry) = s.arrays.remove(&h) {
                    let bytes = entry
                        .element_count
                        .saturating_mul(element_byte_width(entry.element_type));
                    to_evict.push((h, bytes, vm_id));
                }
            }
        }
    });
    to_evict
}

/// Keeps user-owned `GpuArray` handles pinned in `state::arrays` and
/// `device_cache` across an in-flight dispatch's argument marshalling so a
/// concurrent `StreamCleaner` / `GpuArray.close()` cannot evict the entry
/// before `marshal_resident_array_arg` clones the `Arc<DeviceBuffer<T>>`.
#[cfg(feature = "gpu-offload")]
struct InFlightArrayPins {
    handles: Vec<u64>,
}

#[cfg(feature = "gpu-offload")]
impl InFlightArrayPins {
    fn pin(handles: Vec<u64>) -> Self {
        if !handles.is_empty() {
            state::with(|s| {
                for &h in &handles {
                    if let Some(entry) = s.arrays.get_mut(&h) {
                        entry.in_flight_pins += 1;
                    }
                }
            });
        }
        Self { handles }
    }

    fn release(mut self, ctx: &mut dyn cratonvm_native_api::NativeContext) {
        let handles = std::mem::take(&mut self.handles);
        if handles.is_empty() {
            return;
        }
        for (h, bytes, _) in release_pinned_array_handles(&handles) {
            let _ = ctx.gpu_note_retained_vram(-(i64::try_from(bytes).unwrap_or(i64::MAX)), -1);
            ctx.gpu_release_array_cache(h);
        }
    }
}

#[cfg(feature = "gpu-offload")]
impl Drop for InFlightArrayPins {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            for (h, bytes, vm_id) in release_pinned_array_handles(&self.handles) {
                if let Some(vm_id) = vm_id {
                    notify_array_cache_vram_release(vm_id, bytes);
                }
                notify_array_cache_release(h);
            }
        }
    }
}

#[cfg(feature = "gpu-offload")]
struct PreparedGpuDispatch {
    pred_future_handle: Option<u64>,
    pred_stream_handle: Option<u64>,
    in_flight_arrays: InFlightArrayPins,
}

/// Inspect dispatch arguments for `GpuArray` handles (kept resident and
/// user-owned, and pinned across in-flight marshalling) and chained
/// `GpuFutureImpl` handles (inferring predecessor stream affinity).
#[cfg(feature = "gpu-offload")]
fn prepare_gpu_dispatch_args(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    java_args: &[Value],
) -> PreparedGpuDispatch {
    let mut pred_future_handle: Option<u64> = None;
    let mut pred_stream_handle: Option<u64> = None;
    let mut pinned_array_handles: Vec<u64> = Vec::new();
    for &val in java_args {
        if let Some(arr_handle) = read_object_handle_if_class(ctx, val, "craton/gpu/GpuArray") {
            // User-owned GpuArray stays resident on GPU until GpuArray.close() / StreamCleaner.
            if !pinned_array_handles.contains(&arr_handle) {
                pinned_array_handles.push(arr_handle);
            }
            continue;
        }
        if let Some(fut_handle) =
            read_object_handle_if_class(ctx, val, "craton/gpu/internal/GpuFutureImpl")
        {
            pred_future_handle.get_or_insert(fut_handle);
            if pred_stream_handle.is_none() {
                pred_stream_handle = ctx.gpu_submission_stream_handle(fut_handle);
            }
        }
    }
    PreparedGpuDispatch {
        pred_future_handle,
        pred_stream_handle,
        in_flight_arrays: InFlightArrayPins::pin(pinned_array_handles),
    }
}

#[cfg(feature = "gpu-offload")]
fn dispatch_with_prepared(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[Value],
    stream: Option<u64>,
    explicit_stream_pin: bool,
    prepared: PreparedGpuDispatch,
) -> Option<u64> {
    let PreparedGpuDispatch {
        pred_future_handle,
        pred_stream_handle,
        in_flight_arrays,
    } = prepared;
    let out = (|| {
        if let Some(pred_handle) = pred_future_handle {
            if !explicit_stream_pin || stream.is_none() || stream == pred_stream_handle {
                match ctx.gpu_chain_submission(
                    pred_handle,
                    class_name,
                    method_name,
                    descriptor,
                    java_args,
                ) {
                    Ok(handle) => return Some(handle),
                    Err(err) if err == "unsupported" => {}
                    Err(err) => {
                        return Some(record_failed_future_with_message(&format!(
                            "chained GPU dispatch failed: {err}"
                        )));
                    }
                }
            }
        }
        let effective_stream = stream.or(pred_stream_handle);
        ctx.gpu_dispatch_method_on_stream(
            class_name,
            method_name,
            descriptor,
            java_args,
            effective_stream,
        )
    })();
    in_flight_arrays.release(ctx);
    out
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
    let vm_id = ctx.vm_identity();
    let handle = state::with(|s| {
        let h = s.fresh_handle();
        s.executors.insert(h, device);
        s.handle_vms.insert(h, vm_id);
        h
    });
    match instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuExecutorImpl", handle) {
        Ok(Some(Value::Object(Some(obj)))) => Ok(Some(Value::Object(Some(obj)))),
        Ok(other) => {
            state::with(|s| {
                s.executors.remove(&handle);
                s.handle_vms.remove(&handle);
            });
            Ok(other)
        }
        Err(err) => {
            state::with(|s| {
                s.executors.remove(&handle);
                s.handle_vms.remove(&handle);
            });
            Err(err)
        }
    }
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
    if handle == 0 {
        return Ok(None);
    }
    let default_stream = state::with(|s| {
        s.executors.remove(&handle);
        s.handle_vms.remove(&handle);
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
                kind: cratonvm_native_api::registry::GpuErrorKind::Unknown,
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
            return wrap_future_or_release(ctx, h);
        }
    };
    // Phase 6 #5: try to resolve the lambda's target.
    if let Some((class_name, method_name, descriptor, captures)) =
        ctx.gpu_resolve_lambda_target(callable)
    {
        let prepared = prepare_gpu_dispatch_args(ctx, &captures);
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = dispatch_with_prepared(
            ctx,
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
            false,
            prepared,
        ) {
            return wrap_future_or_release(ctx, handle);
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
    wrap_future_or_release(ctx, h)
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
            return wrap_future_or_release(ctx, h);
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
        let prepared = prepare_gpu_dispatch_args(ctx, &captures);
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = dispatch_with_prepared(
            ctx,
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
            false,
            prepared,
        ) {
            return wrap_future_or_release(ctx, handle);
        }
    }
    let h = record_failed_future_with_message(
        "submitWithArg: lambda is not a GPU-dispatchable single-arg SAM",
    );
    wrap_future_or_release(ctx, h)
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
            return wrap_future_or_release(ctx, h);
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
        let prepared = prepare_gpu_dispatch_args(ctx, &captures);
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = dispatch_with_prepared(
            ctx,
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
            false,
            prepared,
        ) {
            return wrap_future_or_release(ctx, handle);
        }
    }
    let h = record_failed_future_with_message(
        "submitWithArgs: lambda is not a GPU-dispatchable static-method reference",
    );
    wrap_future_or_release(ctx, h)
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
            return wrap_future_or_release(ctx, h);
        }
    };
    if let Some((class_name, method_name, descriptor, captures)) =
        ctx.gpu_resolve_lambda_target(runnable)
    {
        let prepared = prepare_gpu_dispatch_args(ctx, &captures);
        let stream = resolve_or_create_default_stream(ctx, exec);
        if let Some(handle) = dispatch_with_prepared(
            ctx,
            &class_name,
            &method_name,
            &descriptor,
            &captures,
            stream,
            false,
            prepared,
        ) {
            return wrap_future_or_release(ctx, handle);
        }
    }
    let h = record_failed_future_with_message(
        "launch: runnable is not a GPU-dispatchable lambda \
         (expected a static-method reference with capture-only args)",
    );
    wrap_future_or_release(ctx, h)
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
    let wrapper = wrap_future_or_release(ctx, handle);
    if timed {
        dispatch_timing::add(7, mark.elapsed().as_nanos() as u64);
    }
    wrapper
}

/// `Native.futureThenApplyGpu(long predFutureHandle, String className, String methodName, String descriptor, Object[] args) -> long`
///
/// Phase 3 — chain a downstream kernel onto `predFutureHandle`'s CUDA stream
/// via `gpu_chain_submission` without blocking on `get()`. Returns the new
/// submission handle (or a synthetic Failed future handle on invalid input /
/// offload failure).
#[cfg(feature = "gpu-offload")]
fn builtin_future_then_apply_gpu(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let pred_handle = arg_long(args, 0) as u64;
    if pred_handle == 0 {
        return Ok(Some(Value::Long(
            record_failed_future_with_message(
                "futureThenApplyGpu: predecessor future handle was 0",
            ) as i64,
        )));
    }
    let class_name = match arg_object(args, 1).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(Some(Value::Long(
                record_failed_future_with_message("futureThenApplyGpu: className was null") as i64,
            )));
        }
    };
    let method_name = match arg_object(args, 2).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(Some(Value::Long(
                record_failed_future_with_message("futureThenApplyGpu: methodName was null")
                    as i64,
            )));
        }
    };
    let descriptor = match arg_object(args, 3).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(Some(Value::Long(
                record_failed_future_with_message("futureThenApplyGpu: descriptor was null")
                    as i64,
            )));
        }
    };
    let mut java_args = Vec::new();
    if let Some(arr_obj) = arg_object(args, 4) {
        let n = ctx.array_length(arr_obj);
        java_args.reserve(n);
        for i in 0..n {
            java_args.push(ctx.get_array_element(arr_obj, i));
        }
    }
    let prepared = prepare_gpu_dispatch_args(ctx, &java_args);
    let PreparedGpuDispatch {
        in_flight_arrays,
        ..
    } = prepared;
    let handle = match ctx.gpu_chain_submission(
        pred_handle,
        &class_name,
        &method_name,
        &descriptor,
        &java_args,
    ) {
        Ok(h) => h,
        Err(msg) => record_failed_future_with_message(&format!("futureThenApplyGpu: {msg}")),
    };
    in_flight_arrays.release(ctx);
    Ok(Some(Value::Long(handle as i64)))
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
            return Ok(record_failed_future_with_message(
                "submitMethod: className was null",
            ));
        }
    };
    let method_name = match arg_object(args, 2).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(record_failed_future_with_message(
                "submitMethod: methodName was null",
            ));
        }
    };
    let descriptor = match arg_object(args, 3).and_then(|o| ctx.read_string(o)) {
        Some(s) => s,
        None => {
            return Ok(record_failed_future_with_message(
                "submitMethod: descriptor was null",
            ));
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
            return Ok(record_failed_future_with_message(
                "submitMethod: args array was null",
            ));
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
    // calls into `runtime::offload::dispatch_method_from_native_on_stream`,
    // or chains via `gpu_chain_submission` when a predecessor `GpuFutureImpl`
    // is passed without an incompatible explicit stream override.
    let prepared = prepare_gpu_dispatch_args(ctx, &java_args);
    let (stream, explicit_stream_pin) = match stream_source {
        StreamSource::ExecutorDefault => (resolve_or_create_default_stream(ctx, handle0), false),
        // A stream handle of 0 is "no particular stream": gpu_dispatch_method_on_stream
        // reads None as "a fresh private one-shot stream for this dispatch", which is
        // what the un-streamed entry points get anyway.
        StreamSource::Explicit if handle0 == 0 => (None, false),
        StreamSource::Explicit => (Some(handle0), true),
    };
    let submission_handle = match dispatch_with_prepared(
        ctx,
        &class_name,
        &method_name,
        &descriptor,
        &java_args,
        stream,
        explicit_stream_pin,
        prepared,
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
                // Every caller of this helper failed to resolve or
                // admit the kernel; none of them reached a device.
                kind: cratonvm_native_api::registry::GpuErrorKind::Compile,
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
/// Create a CUDA stream for `executorHandle` or allocate a synthetic stream
/// handle when running without a CUDA device so `GpuStream.synchronize()` and
/// `GpuStream.close()` still round-trip cleanly.
#[cfg(feature = "gpu-offload")]
fn builtin_new_stream(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let exec = arg_long(args, 0) as u64;
    let vm_id = ctx.vm_identity();
    let (handle, real_stream) = match ctx.gpu_stream_create() {
        Some(real_handle) => state::with(|s| {
            s.streams.insert(real_handle, exec);
            s.handle_vms.insert(real_handle, vm_id);
            (real_handle, true)
        }),
        None => state::with(|s| {
            let h = s.fresh_handle();
            s.streams.insert(h, exec);
            s.synthetic_streams.insert(h);
            s.handle_vms.insert(h, vm_id);
            (h, false)
        }),
    };
    match instantiate_handle_wrapper(ctx, "craton/gpu/internal/GpuStreamImpl", handle) {
        Ok(Some(Value::Object(Some(obj)))) => Ok(Some(Value::Object(Some(obj)))),
        Ok(other) => {
            state::with(|s| {
                s.streams.remove(&handle);
                s.synthetic_streams.remove(&handle);
                s.handle_vms.remove(&handle);
            });
            if real_stream {
                ctx.gpu_stream_release(handle);
            }
            Ok(other)
        }
        Err(err) => {
            state::with(|s| {
                s.streams.remove(&handle);
                s.synthetic_streams.remove(&handle);
                s.handle_vms.remove(&handle);
            });
            if real_stream {
                ctx.gpu_stream_release(handle);
            }
            Err(err)
        }
    }
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
    if handle == 0 {
        return Ok(None);
    }
    state::with(|s| {
        s.streams.remove(&handle);
        s.synthetic_streams.remove(&handle);
        s.handle_vms.remove(&handle);
        s.executor_default_stream.retain(|_, &mut sh| sh != handle);
    });
    ctx.gpu_stream_release(handle);
    Ok(None)
}

/// `Native.streamSynchronize(long streamHandle)`
#[cfg(feature = "gpu-offload")]
fn builtin_stream_synchronize(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    if handle == 0 || state::with(|s| s.synthetic_streams.contains(&handle)) {
        return Ok(None);
    }
    if let Err(message) = ctx.gpu_stream_synchronize(handle) {
        let msg_ref = ctx.create_string(&message);
        return Err(cratonvm_native_api::throwable_builder::throw_new(
            ctx,
            "craton/gpu/GpuException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(msg_ref))],
            || {
                cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::IllegalStateException { message },
                    ),
                )
            },
        ));
    }
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

/// `Native.gemm(long a, long b, long c, int m, int n, int k, boolean half,
///              boolean transA, boolean transB, long stream) -> long`
///
/// Built-in matrix multiply: `C[MxN] = A[MxK] * B[KxN]`, row-major.
///
/// The three arrays are `GpuArray` handles rather than Java arrays so
/// their device buffers stay resident between calls — a decode step
/// multiplies by the same weights every token, and re-uploading them
/// would cost more than the arithmetic does.
///
/// `half` selects fp16 inputs with an fp32 accumulator. `C` is fp32 in
/// both variants: the caller usually wants full precision, and writing
/// fp16 would round twice for nothing.
///
/// Returns a submission handle, or `0` when this VM has no GPU offload —
/// the same "not available" convention `submitMethodHandle` uses, which
/// the Java side turns into a clear exception rather than a silent
/// no-op.
#[cfg(feature = "gpu-offload")]
fn builtin_gemm(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let a = arg_long(args, 0) as u64;
    let b = arg_long(args, 1) as u64;
    let c = arg_long(args, 2) as u64;
    let m = arg_int(args, 3);
    let n = arg_int(args, 4);
    let k = arg_int(args, 5);
    let half = arg_int(args, 6) != 0;
    let trans_a = arg_int(args, 7) != 0;
    let trans_b = arg_int(args, 8) != 0;
    // 0 is "no stream named"; the Java side never mints handle 0.
    let stream = match arg_long(args, 9) as u64 {
        0 => None,
        h => Some(h),
    };

    let mut pin_handles = Vec::with_capacity(3);
    for h in [a, b, c] {
        if h != 0 && !pin_handles.contains(&h) {
            pin_handles.push(h);
        }
    }
    let pins = InFlightArrayPins::pin(pin_handles);
    let handle = ctx
        .gpu_dispatch_gemm(half, a, b, c, m, n, k, trans_a, trans_b, stream)
        .unwrap_or(0);
    pins.release(ctx);
    Ok(Some(Value::Long(handle as i64)))
}

/// Shim for a build without `gpu-offload`: report "not available".
#[cfg(not(feature = "gpu-offload"))]
fn builtin_gemm(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(Some(cratonvm_types::Value::Long(0)))
}

/// `Native.futureErrorKind(long futureHandle) -> int`
///
/// The failure category for a failed submission, so the Java side can
/// pick the right `GpuException` subclass without matching substrings
/// against the driver's wording.
///
/// Returns one of the `GpuErrorKind` wire values: `0` unknown (also the
/// answer for a handle that is not failed, and for an unknown handle —
/// a caller only asks after seeing a failure, and `0` tells it to fall
/// back to message matching, which is exactly what it did before this
/// entry point existed).
///
/// Prefers the real submission registry, then the synthetic store, in
/// the same order as `futureStatus`.
#[cfg(feature = "gpu-offload")]
fn builtin_future_error_kind(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    if let Some(kind) = ctx.gpu_future_error_kind(handle) {
        return Ok(Some(Value::Int(kind.as_i32())));
    }
    let kind = state::with(|s| match s.futures.get(&handle) {
        Some(state::FutureState::Failed { kind, .. }) => kind.as_i32(),
        _ => cratonvm_native_api::registry::GpuErrorKind::Unknown.as_i32(),
    });
    Ok(Some(Value::Int(kind)))
}

/// `Native.futureAwait(long futureHandle, long timeoutNanos) -> int`
///
/// Blocks until the submission reaches a terminal state or the timeout
/// elapses. Returns `1` completed, `2` timed out, `0` unsupported.
///
/// A synthetic future is already terminal the moment it is recorded, so
/// the fallback answers immediately rather than sleeping out the
/// caller's whole timeout. An unknown handle answers `completed` for the
/// same reason `futureStatus` reports a distinct code for it: no amount
/// of waiting will change the answer, and reporting a timeout would send
/// the caller round a loop that can never end.
#[cfg(feature = "gpu-offload")]
fn builtin_future_await(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    use cratonvm_native_api::registry::{GPU_AWAIT_COMPLETED, GPU_AWAIT_UNSUPPORTED};

    let handle = arg_long(args, 0) as u64;
    // A negative timeout would wrap to a colossal `u64`; treat anything
    // non-positive as a pure poll.
    let timeout_nanos = arg_long(args, 1).max(0) as u64;

    let code = ctx.gpu_future_await(handle, timeout_nanos);
    if code != GPU_AWAIT_UNSUPPORTED {
        return Ok(Some(Value::Int(code)));
    }
    // Fallback: the synthetic store. Every state it can hold is
    // terminal on arrival.
    let known = state::with(|s| s.futures.contains_key(&handle));
    let _ = known;
    Ok(Some(Value::Int(GPU_AWAIT_COMPLETED)))
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
        // Ask the real registry for the category it stamped at the
        // point of failure; `Unknown` only if it has none.
        let kind = ctx
            .gpu_future_error_kind(handle)
            .unwrap_or(cratonvm_native_api::registry::GpuErrorKind::Unknown);
        state::with(|s| {
            s.futures
                .insert(handle, state::FutureState::Failed { message, kind });
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
fn device_array_elem_to_type(elem: DeviceArrayElem) -> ArrayElementType {
    match elem {
        DeviceArrayElem::I32 => ArrayElementType::Int,
        DeviceArrayElem::I64 => ArrayElementType::Long,
        DeviceArrayElem::F32 => ArrayElementType::Float,
        DeviceArrayElem::F64 => ArrayElementType::Double,
    }
}

#[cfg(feature = "gpu-offload")]
fn rebuild_validated_array(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    etype: ArrayElementType,
    count: usize,
    bytes: &[u8],
) -> Value {
    let want = count.saturating_mul(element_byte_width(etype));
    if bytes.len() < want {
        Value::Object(None)
    } else {
        Value::Object(Some(rebuild_java_array(ctx, etype, count, bytes)))
    }
}

#[cfg(feature = "gpu-offload")]
fn materialize_device_array_result(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    future_handle: u64,
    array_handle: u64,
    elem: DeviceArrayElem,
    len: usize,
) -> Value {
    if array_handle == 0 {
        return Value::Object(None);
    }
    let etype = device_array_elem_to_type(elem);
    let want = len.saturating_mul(element_byte_width(etype));
    let user_owned = state::with(|s| s.arrays.contains_key(&array_handle));

    if !user_owned {
        if let Some((c_etype, c_count, c_bytes)) =
            state::with(|s| s.ephemeral_results.get(&future_handle).cloned())
        {
            return rebuild_validated_array(ctx, c_etype, c_count, &c_bytes);
        }
    }

    // Serialize first-time download+cache updates per handle so concurrent `.get()`
    // calls do not race on `gpu_array_download_if_dirty` clearing `dirty = false`.
    let dl_guard = state::GPU_STATIC
        .download_gate
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    if !user_owned {
        if let Some((c_etype, c_count, c_bytes)) =
            state::with(|s| s.ephemeral_results.get(&future_handle).cloned())
        {
            drop(dl_guard);
            return rebuild_validated_array(ctx, c_etype, c_count, &c_bytes);
        }
    }

    if let Some(fresh_bytes) = ctx.gpu_array_download_if_dirty(array_handle) {
        if fresh_bytes.len() < want {
            drop(dl_guard);
            return Value::Object(None);
        }
        if user_owned {
            array_replace_bytes(array_handle, fresh_bytes.clone());
        } else {
            // Keep the ephemeral device allocation owned by `StreamSubmission` until
            // `releaseFuture` (`builtin_release_future` / `StreamCleaner`) so concurrent
            // `.get()` / `thenApplyGpu` fan-out calls on the same `GpuFutureImpl` can
            // still reuse the device buffer; cache the downloaded bytes by `future_handle`
            // for repeated `.get()` materializations.
            state::with(|s| {
                s.ephemeral_results
                    .insert(future_handle, (etype, len, fresh_bytes.clone()));
            });
        }
        drop(dl_guard);
        return Value::Object(Some(rebuild_java_array(ctx, etype, len, &fresh_bytes)));
    }

    drop(dl_guard);

    let snapshot = state::with(|s| {
        s.arrays
            .get(&array_handle)
            .map(|entry| (entry.element_type, entry.element_count, entry.bytes.clone()))
    });
    match snapshot {
        Some((s_etype, s_count, opt_bytes)) => {
            let s_bytes = resolve_entry_bytes(s_etype, s_count, opt_bytes);
            rebuild_validated_array(ctx, s_etype, s_count, &s_bytes)
        }
        None if len == 0 => Value::Object(Some(rebuild_java_array(ctx, etype, 0, &[]))),
        None => Value::Object(None),
    }
}

#[cfg(feature = "gpu-offload")]
fn builtin_future_get_result(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    if handle == 0 {
        return Ok(Some(Value::Object(None)));
    }

    if let Some(real_result) = ctx.gpu_future_take_result(handle) {
        let result = match real_result {
            GpuFutureResult::Void => Value::Object(None),
            GpuFutureResult::ScalarI32(v) => box_scalar_result(ctx, Value::Int(v)),
            GpuFutureResult::ScalarI64(v) => box_scalar_result(ctx, Value::Long(v)),
            GpuFutureResult::ScalarF32(v) => box_scalar_result(ctx, Value::Float(v)),
            GpuFutureResult::ScalarF64(v) => box_scalar_result(ctx, Value::Double(v)),
            GpuFutureResult::DeviceArray {
                array_handle,
                elem,
                len,
            } => materialize_device_array_result(ctx, handle, array_handle, elem, len),
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

#[cfg(feature = "gpu-offload")]
fn device_array_elem_kind(elem: DeviceArrayElem) -> (ArrayElementType, i32) {
    match elem {
        DeviceArrayElem::I32 => (ArrayElementType::Int, 0),
        DeviceArrayElem::I64 => (ArrayElementType::Long, 1),
        DeviceArrayElem::F32 => (ArrayElementType::Float, 2),
        DeviceArrayElem::F64 => (ArrayElementType::Double, 3),
    }
}

/// `Native.futureGetResultAsGpuArray(long futureHandle) -> craton.gpu.GpuArray`
///
/// Promotes a completed kernel-returned ephemeral `DeviceArray` into a resident
/// user-owned `GpuArray` without host readback, returning `null` when none exists.
#[cfg(feature = "gpu-offload")]
fn builtin_future_get_result_as_gpu_array(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let future_handle = arg_long(args, 0) as u64;
    if future_handle == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let new_handle = state::with(|s| s.fresh_handle());
    let Some((elem, len)) = ctx.gpu_promote_ephemeral_array(future_handle, new_handle) else {
        return Ok(Some(Value::Object(None)));
    };
    let prev_fut_bytes = state::with(|s| {
        s.ephemeral_results.remove(&future_handle);
        s.future_vram
            .get_mut(&future_handle)
            .map(std::mem::take)
            .unwrap_or(0)
    });
    let (element_type, elem_kind) = device_array_elem_kind(elem);
    let Some(byte_len) = i32::try_from(len)
        .ok()
        .and_then(|len_i32| {
            len.checked_mul(element_byte_width(element_type))
                .map(|b| (len_i32, b))
        })
    else {
        ctx.gpu_release_array_cache(new_handle);
        if prev_fut_bytes > 0 {
            let _ = ctx.gpu_note_retained_vram(
                -(i64::try_from(prev_fut_bytes).unwrap_or(i64::MAX)),
                0,
            );
        }
        return Ok(Some(Value::Object(None)));
    };
    let (len_i32, byte_len) = byte_len;
    let vm_id = ctx.vm_identity();
    state::with(|s| {
        s.arrays.insert(
            new_handle,
            state::ArrayEntry {
                element_type,
                element_count: len,
                bytes: None,
                resident: true,
                in_flight_pins: 0,
                release_requested: false,
            },
        );
        s.handle_vms.insert(new_handle, vm_id);
    });
    let delta_bytes =
        i64::try_from(byte_len).unwrap_or(i64::MAX) - i64::try_from(prev_fut_bytes).unwrap_or(i64::MAX);
    let _ = ctx.gpu_note_retained_vram(delta_bytes, 1);
    let rollback_array = |ctx: &mut dyn cratonvm_native_api::NativeContext| {
        state::with(|s| {
            s.arrays.remove(&new_handle);
            s.handle_vms.remove(&new_handle);
        });
        let _ = ctx.gpu_note_retained_vram(-(i64::try_from(byte_len).unwrap_or(i64::MAX)), -1);
        ctx.gpu_release_array_cache(new_handle);
    };
    match ctx.new_object_initialized(
        "craton/gpu/GpuArray",
        "(JII)V",
        &[
            Value::Long(new_handle as i64),
            Value::Int(elem_kind),
            Value::Int(len_i32),
        ],
    ) {
        Ok(Some(Value::Object(Some(obj)))) => Ok(Some(Value::Object(Some(obj)))),
        Ok(other) => {
            rollback_array(ctx);
            Ok(other)
        }
        Err(err) => {
            rollback_array(ctx);
            Err(err)
        }
    }
}

/// `Native.futureGetErrorMessage(long futureHandle) -> String`
#[cfg(feature = "gpu-offload")]
fn builtin_future_get_error_message(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let msg = state::with(|s| match s.futures.get(&handle) {
        Some(state::FutureState::Failed { message, .. }) => Some(message.clone()),
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    if handle == 0 {
        return Ok(None);
    }
    let removed_vram = state::with(|s| {
        s.futures.remove(&handle);
        s.ephemeral_results.remove(&handle);
        s.handle_vms.remove(&handle);
        s.future_vram.remove(&handle)
    });
    if let Some(bytes) = removed_vram {
        let _ = ctx.gpu_note_retained_vram(-(i64::try_from(bytes).unwrap_or(i64::MAX)), -1);
    }
    // AUDIT 2026-09-02: the line above drops THIS crate's record of the
    // future. The offload runtime keeps a second, independent table --
    // `offload::SUBMISSIONS` -- keyed by the same handle, and until now
    // nothing removed from it: one insert, one remove, and the remove had
    // no production caller. `GpuExecutor.releaseSubmission(h)` compiles
    // to `Native.releaseFuture(h)`, so a program doing exactly what the
    // registry's own overflow warning tells it to do still leaked every
    // submission. Measured before this line: GpuAsyncChainBench, which
    // awaits and releases all 2000 of its handles, still tripped the
    // "1024 submissions are alive" warning.
    ctx.gpu_release_submission(handle);
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
    let byte_len = bytes.len();
    let vm_id = ctx.vm_identity();
    let handle = state::with(|s| {
        let h = s.fresh_handle();
        s.arrays.insert(
            h,
            state::ArrayEntry {
                element_type,
                element_count,
                bytes: Some(bytes),
                resident: true,
                in_flight_pins: 0,
                release_requested: false,
            },
        );
        s.handle_vms.insert(h, vm_id);
        h
    });
    let _ = ctx.gpu_note_retained_vram(i64::try_from(byte_len).unwrap_or(i64::MAX), 1);
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

/// `Native.arrayWrapShort(short[]) -> long`
///
/// The fp16 element type. Java has no half, so a `short[]` carries the
/// IEEE-754 binary16 bit patterns and nothing on this side interprets
/// them — `wrap_primitive_array` records the bytes and their
/// `ArrayElementType::Short` tag, and only the fp16 kernels read them as
/// `__half`. That keeps the representation honest: a `short[]` of half
/// bits IS what the device wants, not a lossy stand-in for one.
#[cfg(feature = "gpu-offload")]
fn builtin_array_wrap_short(
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
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    element_type: ArrayElementType,
    element_bytes: usize,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let length = arg_int(args, 0);
    if length < 0 {
        return Ok(Some(Value::Long(0)));
    }
    let element_count = length as usize;
    let byte_len = element_count.saturating_mul(element_bytes);
    let vm_id = ctx.vm_identity();
    let handle = state::with(|s| {
        let h = s.fresh_handle();
        s.arrays.insert(
            h,
            state::ArrayEntry {
                element_type,
                element_count,
                bytes: None,
                resident: true,
                in_flight_pins: 0,
                release_requested: false,
            },
        );
        s.handle_vms.insert(h, vm_id);
        h
    });
    let _ = ctx.gpu_note_retained_vram(i64::try_from(byte_len).unwrap_or(i64::MAX), 1);
    Ok(Some(Value::Long(handle as i64)))
}

/// `Native.arrayAllocateInt(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_int(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ctx, ArrayElementType::Int, 4, args)
}

/// `Native.arrayAllocateLong(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_long(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ctx, ArrayElementType::Long, 8, args)
}

/// `Native.arrayAllocateShort(int length) -> long`
///
/// Two bytes per element: the fp16 buffer type. See
/// `builtin_array_wrap_short`.
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_short(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ctx, ArrayElementType::Short, 2, args)
}

/// `Native.arrayAllocateFloat(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_float(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ctx, ArrayElementType::Float, 4, args)
}

/// `Native.arrayAllocateDouble(int length) -> long`
#[cfg(feature = "gpu-offload")]
fn builtin_array_allocate_double(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    allocate_primitive_array(ctx, ArrayElementType::Double, 8, args)
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
    let (etype, count, opt_bytes) = match snapshot {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    if ctx.array_length(dest) < count {
        return Ok(Some(Value::Int(0)));
    }
    let bytes = resolve_entry_bytes(etype, count, opt_bytes);
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
        Some((etype, count, opt_bytes)) => {
            let bytes = resolve_entry_bytes(etype, count, opt_bytes);
            let arr = rebuild_java_array(ctx, etype, count, &bytes);
            Ok(Some(Value::Object(Some(arr))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `Native.arrayCopyFromHost(long arrayHandle, Object src) -> boolean`
///
/// Overwrite a resident array's contents from `src`, in place.
///
/// The mirror of `arrayToHostInto`, and the only write path into an
/// already-uploaded array. `arrayWrap*` allocates a new handle and a new
/// device buffer, which is exactly wrong for the caller this exists for:
/// a captured CUDA graph holds the device pointers it was captured with,
/// so feeding a replay new input means writing through the pointer that
/// is already baked in, not making a new one.
///
/// Both copies are updated -- the host-side bytes and, if the array has
/// ever been marshalled to a kernel, the device buffer. Answers `false`
/// if the handle is unknown, the element type or length does not match,
/// or the device write failed; in none of those cases has anything been
/// changed.
#[cfg(feature = "gpu-offload")]
fn builtin_array_copy_from_host(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let src = match arg_object(args, 1) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let (element_type, element_count, bytes) = snapshot_java_array(ctx, src);
    let matches = state::with(|s| {
        s.arrays
            .get(&handle)
            .is_some_and(|e| e.element_type == element_type && e.element_count == element_count)
    });
    if !matches {
        return Ok(Some(Value::Int(0)));
    }
    // Device first: if the device write is refused, the host store is
    // left alone, so the two never disagree about what the array holds.
    // `None` means the array has no device buffer yet -- the host store
    // is then the only copy and the first dispatch will upload it.
    if ctx.gpu_array_upload_bytes(handle, &bytes) == Some(false) {
        return Ok(Some(Value::Int(0)));
    }
    array_replace_bytes(handle, bytes);
    Ok(Some(Value::Int(1)))
}

/// `Native.arrayIsResident(long arrayHandle) -> boolean`
#[cfg(feature = "gpu-offload")]
fn builtin_array_is_resident(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let handle = arg_long(args, 0) as u64;
    let resident = state::with(|s| {
        s.arrays
            .get(&handle)
            .map(|e| e.resident && !e.release_requested)
            .unwrap_or(false)
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
    if handle == 0 {
        return Ok(None);
    }
    // Phase 8 #1 — drop both the host-side resident entry AND any
    // cached device buffer keyed by the same handle. Without the
    // device-cache eviction, a long-running Java program that
    // wraps + releases many GpuArrays would accumulate device
    // memory until process exit.
    //
    // If an in-flight dispatch is currently marshalling this array, defer
    // removal until `InFlightArrayPins::release` finishes so `Cleaner` /
    // `close()` cannot race ahead of `marshal_resident_array_arg`.
    let release_now = state::with(|s| {
        if let Some(entry) = s.arrays.get_mut(&handle) {
            if entry.in_flight_pins > 0 {
                entry.release_requested = true;
                return None;
            }
        }
        s.handle_vms.remove(&handle);
        let removed_bytes = s
            .arrays
            .remove(&handle)
            .map(|e| e.element_count.saturating_mul(element_byte_width(e.element_type)));
        Some(removed_bytes)
    });
    if let Some(removed_bytes) = release_now {
        if let Some(bytes) = removed_bytes {
            let _ = ctx.gpu_note_retained_vram(-(i64::try_from(bytes).unwrap_or(i64::MAX)), -1);
        }
        ctx.gpu_release_array_cache(handle);
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests (gpu-offload only)
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "gpu-offload"))]
mod tests;

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
    use std::sync::atomic::Ordering;

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

    pub fn enabled() -> bool {
        *super::state::GPU_STATIC.timing.on.get_or_init(|| {
            None::<String>
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    pub fn add(phase: usize, nanos: u64) {
        let nanos_arr = &super::state::GPU_STATIC.timing.nanos;
        if phase < nanos_arr.len() {
            nanos_arr[phase].fetch_add(nanos, Ordering::Relaxed);
        }
    }

    pub fn note_call() {
        super::state::GPU_STATIC
            .timing
            .calls
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn report() {
        // The bridge's own engagement census, BEFORE the `calls == 0` gate:
        // both lines are self-gating (silent for a run with no GPU work) and
        // both count the `--gpu` auto-offload path, which never goes through
        // `submitMethod` and so never moves `CALLS`. Until 2026-09-02 they sat
        // behind the gate, so a transfer-floor or dot-product soak under
        // `--gpu` printed no census at all -- a run whose pool served nothing
        // read as a clean run.
        cratonvm_types::gpu_event_census::exit_summary();
        cratonvm_types::gpu_dispatch_memo_census::exit_summary();
        // What the residency cache did across collections. Self-gating
        // (silent unless the cache saw a GC) and, like the two above, it
        // reports a path `CALLS` cannot see. See `gpu_residency_census`.
        cratonvm_types::gpu_residency_census::exit_summary();
        // Whether the submission registry drained. `live_at_exit` should
        // be 0 for a program that releases what it takes.
        cratonvm_types::gpu_submission_census::exit_summary();
        // Whether the overlapped writeback path engaged. Silent unless
        // something was chunkable at all.
        cratonvm_types::gpu_chunk_census::exit_summary();
        // The transparent (`--gpu`) door's phase table, for the same
        // reason: it is self-gating and it counts the path `CALLS` cannot
        // see. See `gpu_offload_phase_census`.
        cratonvm_types::gpu_offload_phase_census::exit_summary();
        // The other half of the transparent door: the dispatches that
        // REFUSED, which the table above cannot see. See
        // `gpu_refusal_census`.
        cratonvm_types::gpu_refusal_census::exit_summary();
        // How much of the program `--gpu` moved off the JIT. See
        // `vm::runtime::offload_jit_gate`.
        cratonvm_types::gpu_jit_gate_census::exit_summary();
        cratonvm_types::gpu_compiled_offload_census::exit_summary();
        // The OSR side of the same question: the gate census counts
        // `caller_blocks_jit` verdicts, this counts refusals actually taken
        // at an OSR admission gate. They disagreed, which is the point.
        cratonvm_types::osr_refusal_census::exit_summary();
        let calls = super::state::GPU_STATIC.timing.calls.load(Ordering::Relaxed);
        if calls == 0
            || super::state::GPU_STATIC
                .timing
                .reported
                .swap(true, Ordering::Relaxed)
        {
            return;
        }
        let nanos = &super::state::GPU_STATIC.timing.nanos;
        let total: u64 = nanos.iter().map(|n| n.load(Ordering::Relaxed)).sum();
        eprintln!(
            "[cratonvm] gpu dispatch: calls={calls} accounted={:.1} us/call",
            total as f64 / calls as f64 / 1000.0
        );
        for (i, name) in PHASES.iter().enumerate() {
            let n = nanos[i].load(Ordering::Relaxed);
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
