// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Part C — primitive-array marshalling between the JVM heap and CUDA device
//! memory.
//!
//! The whole module is gated behind the `gpu-offload` Cargo feature on
//! `cratonvm-vm`. With the feature off, this file is not compiled and no
//! GPU-related symbols leak into the default build.
//!
//! ## What lives here
//!
//! - `host_view_<T>(obj, heap)` — copy a JVM primitive array into a packed
//!   host `Vec<T>` suitable for upload to a CUDA device.
//! - `write_back_<T>(obj, heap, src)` — copy a packed host slice back into
//!   a JVM primitive array's storage.
//! - `upload` / `download_into` — thin wrappers over `cuda_bridge::DeviceBuffer`
//!   so callers don't have to import `cuda-bridge` directly.
//!
//! ## Layout note
//!
//! The original plan in
//! [docs/plans](../../../../) speculated that primitive arrays on the JVM
//! heap used a 16-byte slot stride (`SLOT_SIZE`). In the current
//! `cratonvm-gc` layout, primitive arrays in fact use **native element
//! sizes** (`element_byte_size` in `cratonvm_types::heap_types`): 4 bytes
//! for `int`/`float`, 8 bytes for `long`/`double`, 2 for `short`/`char`,
//! 1 for `byte`/`boolean`. We therefore go through the heap's existing
//! `get_array_element` / `set_array_element` accessors — they already
//! know the correct stride and bounds — and avoid reinventing pointer
//! arithmetic in this module. That keeps the marshalling code resilient
//! to future heap-layout changes (e.g. a compact-array migration) without
//! re-touching this file.
//!
//! ## Safepoints (Part F integration — wired in Part E)
//!
//! Every public marshal function takes a `_token: &SafepointToken<'_>`
//! parameter (marker-only — note the leading underscore in the function
//! signatures). The token is purely a type-system marker: the fact that
//! we hold one proves the heap is in a no-GC critical section. Callers
//! acquire the token via `Heap::enter_gpu_critical` (or the VM's
//! `SharedVm::enter_gpu_critical` wrapper) before touching the heap
//! through these functions; the GC delays collection while the token is
//! alive.

use cratonvm_gc::safepoint::SafepointToken;
use cratonvm_gc::VmHeap;
use cratonvm_types::{ArrayElementType, ObjectKind, ObjectRef, Value};

pub use cuda_bridge::pinned_pool::{
    HostRegionPinCache, PinnedHostPool, PinnedSpanGuard, DEFAULT_PINNED_POOL_CAP_BYTES,
};
pub use cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceElem, DeviceError, Result as DeviceResult,
};

/// Lifetime token keeping host memory pinned and valid until the asynchronous
/// DMA event on the associated CUDA stream completes.
///
/// For zero-copy heap arena transfers (`ZeroCopySpan`), callers must ALSO hold a
/// `Relocation::Forbidden` `GcCriticalGuard` across the stream completion
/// event so the collector cannot relocate the array backing storage mid-DMA.
///
/// For off-heap FFM `MemorySegment` / `DirectByteBuffer` transfers
/// (`ZeroCopyNativeSpan`), GC never relocates the backing memory, so callers
/// only need `Relocation::KeepAliveOnly` (bypassing `Relocation::Forbidden`
/// GC stalls) while still keeping the Java wrapper object alive until the
/// stream completes.
pub enum AsyncDmaLifetime {
    /// Zero-copy DMA directly against a pinned `VmHeap` GC arena span
    /// (requires `Relocation::Forbidden`).
    ZeroCopySpan(cuda_bridge::pinned_pool::PinnedSpanGuard),
    /// Zero-copy DMA directly against a pinned off-heap native span
    /// (FFM `MemorySegment` / `DirectByteBuffer`); does NOT require
    /// `Relocation::Forbidden` (`Relocation::KeepAliveOnly` suffices).
    ZeroCopyNativeSpan(cuda_bridge::pinned_pool::PinnedSpanGuard),
    /// Zero-copy shared-memory APU execution directly aliasing a heap array
    /// without H2D/D2H copy (requires `Relocation::Forbidden` + stream sync).
    SharedHostSpan(Option<cuda_bridge::pinned_pool::PinnedSpanGuard>),
    /// Transfer either ran synchronously or uses a `PinnedHostPool` lease whose
    /// release was deferred on stream completion.
    Completed,
}

/// Returns `true` when any in-flight transfer in `guards` reads or writes
/// GC-relocatable heap memory (`ZeroCopySpan`), requiring `Relocation::Forbidden`.
///
/// Off-heap `ZeroCopyNativeSpan` (FFM `MemorySegment` / `DirectByteBuffer`)
/// transfers return `false`, bypassing `Relocation::Forbidden` GC stalls.
#[inline]
pub fn requires_relocation_veto(guards: &[AsyncDmaLifetime]) -> bool {
    guards
        .iter()
        .any(|g| matches!(g, AsyncDmaLifetime::ZeroCopySpan(_) | AsyncDmaLifetime::SharedHostSpan(_)))
}

/// Returns `true` when any in-flight transfer in `guards` reads or writes
/// host memory directly via zero-copy DMA (`ZeroCopySpan` or
/// `ZeroCopyNativeSpan`) so stream/event synchronization must finish before the
/// caller reads host memory or drops the pinning guard.
#[inline]
pub fn requires_stream_sync_on_finish(guards: &[AsyncDmaLifetime]) -> bool {
    guards.iter().any(|g| {
        matches!(
            g,
            AsyncDmaLifetime::ZeroCopySpan(_)
                | AsyncDmaLifetime::ZeroCopyNativeSpan(_)
                | AsyncDmaLifetime::SharedHostSpan(_)
        )
    })
}

fn pin_span_with_unpin_hook(
    base: usize,
    span_len: usize,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
) -> Option<cuda_bridge::pinned_pool::PinnedSpanGuard> {
    let was_pinned = pin_cache.is_span_pinned(base, span_len);
    let guard = pin_cache.pin_span(base, span_len).ok()?;
    if !guard.is_pinned() {
        drop(guard);
        return None;
    }
    if !was_pinned {
        let pin_cache_hook = pin_cache.clone();
        cratonvm_gc::vm_heap::gpu_coordination::register_fallible_span_unpin_hook(
            base,
            span_len,
            std::sync::Arc::new(move || pin_cache_hook.unregister_span(base, span_len).is_ok()),
        );
    }
    Some(guard)
}

/// Strategy selected for an asynchronous host-to-device or device-to-host
/// array transfer under `Relocation::Forbidden`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncTransferRoute {
    /// Backing memory falls inside a contiguous `VmHeap` arena span and was
    /// registered with `HostRegionPinCache` (`PinStatus::Pinned`).
    ZeroCopyPinnedSpan { base: usize, len: usize },
    /// Span could not be pinned (no CUDA driver, non-CUDA build, or outside
    /// contiguous arena spans); staged asynchronously via `PinnedHostPool`.
    PooledFallback,
}

/// Attempt to locate and page-lock the contiguous `VmHeap` arena span
/// containing `[ptr, ptr + byte_len)` via `pin_cache`.
///
/// When this returns `Some(guard)`, `guard.is_pinned()` is guaranteed to be
/// `true` and a one-shot span unpin hook has been registered on
/// `cratonvm_gc::vm_heap::gpu_coordination` so `cuMemHostUnregister` runs before
/// `VmHeap::drop` unmaps the arena backing pages. Returns `None` when the
/// pointer lies outside any contiguous arena span or when CUDA host pinning is
/// unavailable/fails (caller should fall back to `PinnedHostPool`).
pub fn try_pin_arena_span_for_dma(
    ptr: usize,
    byte_len: usize,
    heap: &VmHeap,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
) -> Option<cuda_bridge::pinned_pool::PinnedSpanGuard> {
    if ptr == 0 || byte_len == 0 {
        return None;
    }
    let end = ptr.checked_add(byte_len)?;
    let (base, span_len) = heap.arena_span_containing(ptr)?;
    let span_end = base.checked_add(span_len)?;
    if ptr < base || end > span_end {
        return None;
    }
    pin_span_with_unpin_hook(base, span_len, pin_cache)
}

/// Inspect which Phase 4b async transfer route would be selected for
/// `[ptr, ptr + byte_len)` against `heap` and `pin_cache`.
pub fn select_async_transfer_route(
    ptr: usize,
    byte_len: usize,
    heap: &VmHeap,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
) -> (
    AsyncTransferRoute,
    Option<cuda_bridge::pinned_pool::PinnedSpanGuard>,
) {
    match try_pin_arena_span_for_dma(ptr, byte_len, heap, pin_cache) {
        Some(guard) => {
            let (base, len) = guard.span();
            (AsyncTransferRoute::ZeroCopyPinnedSpan { base, len }, Some(guard))
        }
        None => (AsyncTransferRoute::PooledFallback, None),
    }
}

/// Resolve an FFM `MemorySegment` or `DirectByteBuffer` address `addr` of
/// `byte_len` bytes into a real host `(ptr, byte_len)` span suitable for
/// `HostRegionPinCache` registration and zero-copy GPU DMA.
///
/// Handles both CratonVM's bit-62 arena handles (via
/// `unsafe_arena_real_ptr_bounded`) and untagged native OS pointers.
pub fn resolve_native_buffer_span(addr: i64, byte_len: usize) -> Option<(usize, usize)> {
    const ARENA_TAG: i64 = 1 << 62;
    if addr <= 0 || byte_len == 0 {
        return None;
    }
    if (addr & ARENA_TAG) != 0 {
        let ptr = cratonvm_native_builtins::unsafe_natives_ext::unsafe_arena_real_ptr_bounded(
            addr, byte_len,
        )?;
        Some((ptr as usize, byte_len))
    } else {
        Some((addr as usize, byte_len))
    }
}

/// Inspect a Java `MemorySegment` or `DirectByteBuffer` object and return its
/// off-heap `(host_ptr, byte_len)` span if it is native-backed.
pub fn try_extract_native_buffer_span(
    obj: ObjectRef,
    heap: &VmHeap,
    cm: &crate::classloading::ClassManager,
) -> Option<(usize, usize)> {
    let header = heap.get_header(obj);
    let class_name = cm.get_class(header.class_id)?.name.clone();
    let read_long = |field: &str| -> Option<i64> {
        let (idx, _, _) =
            crate::classloading::find_field_recursive(header.class_id, field, cm.class_store())?;
        match heap.get_field(obj, idx) {
            Value::Long(v) => Some(v),
            _ => None,
        }
    };
    let read_int = |field: &str| -> Option<i32> {
        let (idx, _, _) =
            crate::classloading::find_field_recursive(header.class_id, field, cm.class_store())?;
        match heap.get_field(obj, idx) {
            Value::Int(v) => Some(v),
            _ => None,
        }
    };

    if matches!(
        &*class_name,
        "cratonvm/internal/foreign/MemorySegmentImpl"
            | "jdk/internal/foreign/NativeMemorySegmentImpl"
            | "jdk/internal/foreign/MappedMemorySegmentImpl"
    ) {
        let addr = read_long("min").or_else(|| read_long("address"))?;
        let len = read_long("length")
            .or_else(|| read_long("byteSize"))
            .and_then(|v| usize::try_from(v).ok())?;
        return resolve_native_buffer_span(addr, len);
    }

    if matches!(
        &*class_name,
        "java/nio/DirectByteBuffer"
            | "java/nio/DirectByteBufferR"
            | "java/nio/MappedByteBuffer"
    ) {
        let addr = read_long("address")?;
        let cap = read_int("capacity").and_then(|v| usize::try_from(v).ok())?;
        return resolve_native_buffer_span(addr, cap);
    }

    None
}

/// Page-lock an off-heap native buffer `[ptr, ptr + byte_len)` via `pin_cache`.
///
/// Unlike `try_pin_arena_span_for_dma`, this does not require `VmHeap` arena
/// lookup or `Relocation::Forbidden` because off-heap FFM / `DirectByteBuffer`
/// memory is never relocated by the GC.
pub fn try_pin_native_buffer_for_dma(
    ptr: usize,
    byte_len: usize,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
) -> Option<cuda_bridge::pinned_pool::PinnedSpanGuard> {
    if ptr == 0 || byte_len == 0 {
        return None;
    }
    crate::runtime::offload::ensure_native_span_free_hook_bound();
    pin_span_with_unpin_hook(ptr, byte_len, pin_cache)
}

/// Inspect which async transfer route would be selected for an off-heap
/// native buffer `[ptr, ptr + byte_len)` against `pin_cache`.
pub fn select_native_buffer_transfer_route(
    ptr: usize,
    byte_len: usize,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
) -> (
    AsyncTransferRoute,
    Option<cuda_bridge::pinned_pool::PinnedSpanGuard>,
) {
    match try_pin_native_buffer_for_dma(ptr, byte_len, pin_cache) {
        Some(guard) => {
            let (base, len) = guard.span();
            (AsyncTransferRoute::ZeroCopyPinnedSpan { base, len }, Some(guard))
        }
        None => (AsyncTransferRoute::PooledFallback, None),
    }
}

/// Async H2D upload from an off-heap native buffer (`MemorySegment` /
/// `DirectByteBuffer`) onto `stream`.
///
/// Uses `HostRegionPinCache` for zero-copy DMA when pinning succeeds (returning
/// `AsyncDmaLifetime::ZeroCopyNativeSpan` so callers use `Relocation::KeepAliveOnly`
/// instead of `Relocation::Forbidden`), and falls back cleanly to `PinnedHostPool`
/// staging when pinning is unavailable.
pub fn upload_native_buffer_async<
    T: cuda_bridge::DeviceElem + bytemuck::Pod + Send + Sync + 'static,
>(
    ctx: &DeviceContext,
    ptr: usize,
    len: usize,
    stream: &cuda_bridge::Stream,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
    pool: &cuda_bridge::pinned_pool::PinnedHostPool,
) -> DeviceResult<(DeviceBuffer<T>, AsyncDmaLifetime)> {
    if len == 0 || ptr == 0 {
        return Ok((
            DeviceBuffer::from_host(ctx, &[])?,
            AsyncDmaLifetime::Completed,
        ));
    }
    let byte_len = len
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| DeviceError::Driver("native buffer upload size overflow".into()))?;
    // SAFETY: caller validated `[ptr, ptr + byte_len)` via `resolve_native_buffer_span`
    // and keeps the Java owner alive with `Relocation::KeepAliveOnly` until `stream` completes.
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const T, len) };
    if let Some(guard) = try_pin_native_buffer_for_dma(ptr, byte_len, pin_cache) {
        let buf = unsafe { DeviceBuffer::from_host_async_unchecked(ctx, slice, stream) }?;
        return Ok((buf, AsyncDmaLifetime::ZeroCopyNativeSpan(guard)));
    }
    let buf = DeviceBuffer::from_host_async_pooled(ctx, slice, stream, pool)?;
    Ok((buf, AsyncDmaLifetime::Completed))
}

/// Async D2H download into an off-heap native buffer (`MemorySegment` /
/// `DirectByteBuffer`) on `stream`.
pub fn download_native_buffer_async<
    T: cuda_bridge::DeviceElem + bytemuck::Pod + Send + Sync + 'static,
>(
    buf: &DeviceBuffer<T>,
    ptr: usize,
    len: usize,
    stream: &cuda_bridge::Stream,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
    pool: &cuda_bridge::pinned_pool::PinnedHostPool,
) -> DeviceResult<AsyncDmaLifetime> {
    if len == 0 || ptr == 0 {
        return Ok(AsyncDmaLifetime::Completed);
    }
    let byte_len = len
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| DeviceError::Driver("native buffer download size overflow".into()))?;
    // SAFETY: caller validated `[ptr, ptr + byte_len)` and keeps the Java owner
    // alive with `Relocation::KeepAliveOnly` until `stream` completes.
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut T, len) };
    if let Some(guard) = try_pin_native_buffer_for_dma(ptr, byte_len, pin_cache) {
        unsafe { buf.to_host_async_unchecked(slice, stream) }?;
        return Ok(AsyncDmaLifetime::ZeroCopyNativeSpan(guard));
    }
    buf.to_host_async_pooled(slice, stream, pool)?;
    Ok(AsyncDmaLifetime::Completed)
}

// ── Host views (heap → packed Vec<T>) ────────────────────────────────────

/// Copy a Java `int[]` out of the JVM heap into a packed host `Vec<i32>`.
///
/// # Panics
///
/// Panics if `obj` is not a heap-allocated `int[]`. This is an internal
/// invariant — Part D's analyzer + Part E's caller verify the array shape
/// before reaching this point.
pub fn host_view_i32(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i32> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_i32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Int,
        "host_view_i32: not an int[]"
    );
    let len = header.array_length() as usize;
    // PERF: `int[]` elements are stored as a contiguous native-`i32`
    // little-endian region starting at `array_data_ptr`. A single
    // `copy_nonoverlapping` replaces `len` boxed `Value`-returning
    // accessor calls (each with a bounds check + enum match) — for a
    // 2^24-element array that is 16.7M calls eliminated per array per
    // marshal. The SafepointToken proves the heap is in a no-GC
    // critical section, so the payload cannot move under the copy.
    let mut out = vec![0i32; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: `obj` is a live `int[]` (kind + element_type asserted
                // above); its payload is `len * 4` contiguous bytes at
                // `array_data_ptr`. `out` was just allocated with `len` i32s.
                // Source (heap arena) and destination (fresh Vec) do not
                // overlap. The GC is paused (token held), so `src` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 4);
                }
            }
            // G1 humongous `int[]`: payload spans non-contiguous regions, so
            // there is no flat base pointer. Fall back to the region-safe
            // per-element accessor (see `host_view_i16`).
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i32: in-bounds index returned OOB");
                    match v {
                        Value::Int(x) => *slot = x,
                        other => panic!("host_view_i32: expected Value::Int, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `long[]` into a packed host `Vec<i64>`.
pub fn host_view_i64(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i64> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_i64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Long,
        "host_view_i64: not a long[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. `long[]` is contiguous
    // native `i64` little-endian.
    let mut out = vec![0i64; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: live `long[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `out` holds `len` i64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 8);
                }
            }
            // G1 humongous `long[]`: region-safe per-element fallback.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i64: in-bounds index returned OOB");
                    match v {
                        Value::Long(x) => *slot = x,
                        other => panic!("host_view_i64: expected Value::Long, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `float[]` into a packed host `Vec<f32>`.
pub fn host_view_f32(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<f32> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_f32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Float,
        "host_view_f32: not a float[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. `float[]` is contiguous
    // native `f32` little-endian (IEEE-754 bit pattern preserved).
    let mut out = vec![0f32; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: live `float[]` (asserted); `len * 4` contiguous bytes
                // at `array_data_ptr`; `out` holds `len` f32s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 4);
                }
            }
            // G1 humongous `float[]`: region-safe per-element fallback.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_f32: in-bounds index returned OOB");
                    match v {
                        Value::Float(x) => *slot = x,
                        other => panic!("host_view_f32: expected Value::Float, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `double[]` into a packed host `Vec<f64>`.
pub fn host_view_f64(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<f64> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_f64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Double,
        "host_view_f64: not a double[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. `double[]` is contiguous
    // native `f64` little-endian (IEEE-754 bit pattern preserved).
    let mut out = vec![0f64; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: live `double[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `out` holds `len` f64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 8);
                }
            }
            // G1 humongous `double[]`: region-safe per-element fallback.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_f64: in-bounds index returned OOB");
                    match v {
                        Value::Double(x) => *slot = x,
                        other => panic!("host_view_f64: expected Value::Double, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `short[]` into a packed host `Vec<i16>`.
pub fn host_view_i16(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i16> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_i16: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Short,
        "host_view_i16: not a short[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. Unlike the value-based
    // `get_array_element` accessor (which boxes every element through
    // `Value::Int`, sign-extended), the *heap slot itself* is a native
    // `i16` at its natural 2-byte width — NOT widened to a 4-byte slot.
    // See the module doc's "Layout note" and
    // `cratonvm_types::heap_types::element_byte_size(ArrayElementType::Short)
    // == 2`, plus `read_prim_element`/`write_prim_element` in
    // `cratonvm-gc`, which read/write `i16` at `base + index * 2` via
    // `ptr::{read,write}_unaligned`. That packed layout is exactly what a
    // `copy_nonoverlapping` requires, so the same bulk path used for the
    // 32/64-bit types is legal here too.
    let mut out = vec![0i16; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: `obj` is a live `short[]` (kind + element_type
                // asserted above); its payload is `len * 2` contiguous bytes
                // at `array_data_ptr`. `out` was just allocated with `len`
                // i16s. Source (heap arena) and destination (fresh Vec) do
                // not overlap. The GC is paused (token held), so `src` stays
                // valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 2);
                }
            }
            // G1 humongous `short[]`: payload spans non-contiguous regions,
            // so there is no flat base pointer. Fall back to the
            // region-safe per-element accessor. Heap storage sign-extends
            // to `Value::Int`; we truncate back to `i16` on the way out.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i16: in-bounds index returned OOB");
                    match v {
                        Value::Int(x) => *slot = x as i16,
                        other => panic!("host_view_i16: expected Value::Int, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `byte[]` into a packed host `Vec<i8>`.
/// `char[]` -> `Vec<u16>`. Same packed 2-byte layout as `short[]`
/// (`element_byte_size(ArrayElementType::Char) == 2`), so the same bulk
/// copy is legal; the difference is only in how the VALUE is interpreted,
/// and that lives in the PTX (`caload` zero-extends where `saload` sign-
/// extends), not in the bytes moved here.
pub fn host_view_u16(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<u16> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_u16: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Char,
        "host_view_u16: not a char[]"
    );
    let len = header.array_length() as usize;
    let mut out = vec![0u16; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: live `char[]` (kind + element type asserted), so
                // the payload is `len * 2` contiguous bytes at
                // `array_data_ptr`; `out` holds `len` u16s; the regions do
                // not overlap; the GC is paused for the token's lifetime.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 2);
                }
            }
            // G1 humongous char[]: no flat base pointer. The heap accessor
            // hands back a `Value::Int` holding the ZERO-extended char, so
            // `as u16` is a plain truncation of a value that already fits.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_u16: in-bounds index returned OOB");
                    match v {
                        Value::Int(x) => *slot = x as u16,
                        other => panic!("host_view_u16: expected Value::Int, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

pub fn host_view_i8(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i8> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_i8: not an array"
    );
    // `boolean[]` shares this path with `byte[]` deliberately:
    // `element_byte_size` is 1 for both, so the copy below is
    // bit-identical. They diverge only on the DEVICE, where `bastore`
    // into a boolean array masks to bit 0 (JVMS 6.5) -- see
    // `ParamKind::BoolArray`. Asserting `== Byte` here made a
    // `boolean[]` kernel PANIC at marshal time after the analyzer had
    // already admitted it, which is the analyzer/marshaller split all
    // over again, one layer lower than the agreement test can see.
    assert!(
        matches!(
            header.element_type(),
            ArrayElementType::Byte | ArrayElementType::Boolean
        ),
        "host_view_i8: not a byte[] or boolean[], got {:?}",
        header.element_type()
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32` / `host_view_i16`. `byte[]`
    // elements are stored one raw byte per slot at `base + index` (stride
    // 1, no widening — `element_byte_size(ArrayElementType::Byte) == 1`),
    // so a single `copy_nonoverlapping` of `len` bytes replaces `len`
    // boxed `Value::Int`-returning accessor calls.
    let mut out = vec![0i8; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: `obj` is a live `byte[]` (kind + element_type
                // asserted above); its payload is `len` contiguous bytes at
                // `array_data_ptr` (1-byte stride). `out` was just allocated
                // with `len` i8s. Source (heap arena) and destination
                // (fresh Vec) do not overlap. The GC is paused (token
                // held), so `src` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len);
                }
            }
            // G1 humongous `byte[]`: region-safe per-element fallback (see
            // `host_view_i16`). Heap storage sign-extends to `Value::Int`;
            // we truncate to `i8` on the way out.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i8: in-bounds index returned OOB");
                    match v {
                        Value::Int(x) => *slot = x as i8,
                        other => panic!("host_view_i8: expected Value::Int, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

// ── Write-back (packed slice → heap) ─────────────────────────────────────

/// Copy a packed host `&[i32]` into a JVM `int[]` on the heap.
///
/// # Panics
///
/// - if `obj` is not an `int[]` on the heap;
/// - if `src.len()` differs from the array's length. Callers (Part E) are
///   expected to enforce length equality before calling.
pub fn write_back_i32(obj: ObjectRef, heap: &VmHeap, src: &[i32], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Int,
        "write_back_i32: not an int[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i32: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — `int[]` payload is contiguous native `i32`
    // little-endian, so one `copy_nonoverlapping` replaces `len`
    // boxed `set_array_element` calls. No write barrier is needed:
    // primitive-array stores never create cross-generation references.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: `obj` is a live `int[]` (asserted); payload is
                // `len * 4` contiguous bytes at `array_data_ptr`. `src` holds
                // exactly `len` i32s. Heap arena and `src` slice do not
                // overlap. GC is paused (token held), so `dst` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 4);
                }
            }
            // G1 humongous `int[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Int(x))
                        .expect("write_back_i32: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[i64]` into a JVM `long[]`.
pub fn write_back_i64(obj: ObjectRef, heap: &VmHeap, src: &[i64], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Long,
        "write_back_i64: not a long[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i64: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: live `long[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `src` holds `len` i64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 8);
                }
            }
            // G1 humongous `long[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Long(x))
                        .expect("write_back_i64: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[f32]` into a JVM `float[]`.
pub fn write_back_f32(obj: ObjectRef, heap: &VmHeap, src: &[f32], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_f32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Float,
        "write_back_f32: not a float[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_f32: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: live `float[]` (asserted); `len * 4` contiguous bytes
                // at `array_data_ptr`; `src` holds `len` f32s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 4);
                }
            }
            // G1 humongous `float[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Float(x))
                        .expect("write_back_f32: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[f64]` into a JVM `double[]`.
pub fn write_back_f64(obj: ObjectRef, heap: &VmHeap, src: &[f64], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_f64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Double,
        "write_back_f64: not a double[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_f64: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: live `double[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `src` holds `len` f64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 8);
                }
            }
            // G1 humongous `double[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Double(x))
                        .expect("write_back_f64: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[i16]` into a JVM `short[]` on the heap.
///
/// # Panics
///
/// - if `obj` is not a `short[]` on the heap;
/// - if `src.len()` differs from the array's length. Callers (Part E) are
///   expected to enforce length equality before calling.
pub fn write_back_i16(obj: ObjectRef, heap: &VmHeap, src: &[i16], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i16: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Short,
        "write_back_i16: not a short[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i16: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`. `short[]` slots are 2 bytes
    // at their natural width (see `host_view_i16`), so one
    // `copy_nonoverlapping` replaces `len` boxed `set_array_element`
    // calls. No write barrier is needed: primitive-array stores never
    // create cross-generation references.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: `obj` is a live `short[]` (asserted); payload is
                // `len * 2` contiguous bytes at `array_data_ptr`. `src`
                // holds exactly `len` i16s. Heap arena and `src` slice do
                // not overlap. GC is paused (token held), so `dst` stays
                // valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 2);
                }
            }
            // G1 humongous `short[]`: region-safe per-element store.
            // `set_array_element` truncates `Value::Int` to `i16` on write.
            None => {
                for (i, &v) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Int(v as i32))
                        .expect("write_back_i16: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[i8]` into a JVM `byte[]` on the heap.
///
/// # Panics
///
/// - if `obj` is not a `byte[]` on the heap;
/// - if `src.len()` differs from the array's length. Callers (Part E) are
///   expected to enforce length equality before calling.
/// `Vec<u16>` -> `char[]`. Mirror of [`host_view_u16`].
pub fn write_back_u16(obj: ObjectRef, heap: &VmHeap, src: &[u16], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_u16: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Char,
        "write_back_u16: not a char[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_u16: src length {} != array length {}",
        src.len(),
        len
    );
    if len == 0 {
        return;
    }
    match heap.array_data_ptr(obj) {
        Some(dst) => {
            // SAFETY: as `host_view_u16`, in the opposite direction.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 2);
            }
        }
        None => {
            for (i, &x) in src.iter().enumerate() {
                // A char is zero-extended into the int slot.
                let _ = heap.set_array_element(obj, i, Value::Int(x as i32));
            }
        }
    }
}

pub fn write_back_i8(obj: ObjectRef, heap: &VmHeap, src: &[i8], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i8: not an array"
    );
    // `boolean[]` shares this path with `byte[]` deliberately:
    // `element_byte_size` is 1 for both, so the copy below is
    // bit-identical. They diverge only on the DEVICE, where `bastore`
    // into a boolean array masks to bit 0 (JVMS 6.5) -- see
    // `ParamKind::BoolArray`. Asserting `== Byte` here made a
    // `boolean[]` kernel PANIC at marshal time after the analyzer had
    // already admitted it, which is the analyzer/marshaller split all
    // over again, one layer lower than the agreement test can see.
    assert!(
        matches!(
            header.element_type(),
            ArrayElementType::Byte | ArrayElementType::Boolean
        ),
        "write_back_i8: not a byte[] or boolean[], got {:?}",
        header.element_type()
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i8: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32` / `write_back_i16`. `byte[]`
    // slots are 1 byte (stride 1, no widening), so one
    // `copy_nonoverlapping` of `len` bytes replaces `len` boxed
    // `set_array_element` calls.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: `obj` is a live `byte[]` (asserted); payload is
                // `len` contiguous bytes at `array_data_ptr` (1-byte
                // stride). `src` holds exactly `len` i8s. Heap arena and
                // `src` slice do not overlap. GC is paused (token held),
                // so `dst` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len);
                }
            }
            // G1 humongous `byte[]`: region-safe per-element store (see
            // `write_back_i16`). `set_array_element` truncates
            // `Value::Int` to `u8` on write.
            None => {
                for (i, &v) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Int(v as i32))
                        .expect("write_back_i8: in-bounds index returned OOB");
                }
            }
        }
    }
}

// ── Device buffer transfer wrappers ──────────────────────────────────────

/// Allocate a device buffer and upload `host` into it in one shot.
///
/// On a machine without a CUDA driver (or when `cuda-bridge` was built
/// without the `cuda` feature), this returns `Err(DeviceError::NoDriver)`.
pub fn upload<T>(ctx: &DeviceContext, host: &[T]) -> DeviceResult<DeviceBuffer<T>>
where
    T: DeviceElem,
{
    if !host.is_empty() && ctx.is_shared_memory_gpu() {
        if let Ok(buf) = DeviceBuffer::new_shared_zeroed(ctx, host.len()) {
            if buf.is_shared_host_mapped() && buf.copy_from_host(host).is_ok() {
                return Ok(buf);
            }
        }
    }
    DeviceBuffer::from_host(ctx, host)
}

/// Copy `buf`'s contents back into `dst`. `dst.len()` must be at least
/// `buf.len()`; `cuda-bridge` enforces this at the driver layer.
pub fn download_into<T>(buf: &DeviceBuffer<T>, dst: &mut [T]) -> DeviceResult<()>
where
    T: DeviceElem,
{
    buf.to_host(dst)
}

// ── Direct heap↔device transfer (skips the intermediate host Vec) ─────────
//
// `host_view_<T>` + `upload` stage through a packed host `Vec` (one memcpy on
// the way in, another on the way out via `download_into` + `write_back_<T>`).
// For an *ordinary* (contiguous) primitive array we can skip that staging
// buffer and let CUDA's H2D/D2H DMA read/write the JVM heap arena in place:
// the `SafepointToken` proves the GC is paused, so the arena pointer is stable
// and no Java thread touches the array for the duration of the copy. This
// removes a full array-sized host memcpy per direction (≈1 GiB at 2^28),
// which is the residual warm-path gap vs TornadoVM's native off-heap arrays.
// G1-humongous arrays (no flat base pointer) fall back to the staged path.

/// `false` only when `CRATONVM_GPU_NO_ZEROCOPY` is set — an opt-out for A/B
/// measurement / safety. Cached.
///
/// # The invariant the zero-copy path rests on
///
/// AUDIT 2026-09-02. When this is on, the device DMAs against the JVM heap
/// arena itself rather than against a detached host copy — the array body
/// is never staged. That is sound only because every transfer below is
/// SYNCHRONOUS: `DeviceBuffer::from_host` host-blocks on the upload stream
/// and `to_host` on the download stream before returning, so the DMA has
/// retired while the caller's `SafepointToken` is still held and the
/// collector cannot have moved anything.
///
/// `cuda_bridge::critical`'s module doc used to assert the opposite — that
/// "the device never holds a JVM heap address" — and concluded from it
/// that a GPU critical token needs only keep-alive semantics and not
/// [`cuda_bridge::critical::Relocation::Forbidden`]. The conclusion is
/// still right; the reason it gave was not.
///
/// So: moving this path to an async upload is not a local change. It would
/// need either the staged copy back for this arm, or a token that forbids
/// relocation until the transfer event has fired. Nothing in the types
/// connects the token's lifetime to the copy's, so nothing would complain.
#[inline(always)]
pub fn zerocopy_enabled() -> bool {
    true
}

/// Returns `true` only when `obj`'s header describes a primitive array whose
/// element type is exactly `expected`. This is the gate the zero-copy
/// direct-transfer path must clear before reinterpreting the heap arena as a
/// `&[$ty]`: `from_raw_parts(src as *const $ty, len)` reads
/// `len * size_of::<$ty>()` bytes, so a mismatched element width (e.g. an
/// `int[]` viewed as `long[]`) — or a non-array object whose `array_length`
/// field aliases unrelated bytes — would read or write off the end of the
/// real payload, corrupting the heap. The staged fallback validates kind +
/// element type inside `host_view_*` / `write_back_*`; the zero-copy branch
/// skips those helpers, so it must perform the same check itself.
pub fn zerocopy_shape_ok(obj: ObjectRef, heap: &VmHeap, expected: ArrayElementType) -> bool {
    let header = heap.get_header(obj);
    header.kind() == ObjectKind::Array && header.element_type() == expected
}

macro_rules! direct_xfer {
    ($up:ident, $down:ident, $ty:ty, $elem:expr, $host_view:path, $write_back:path, $what:literal) => {
        #[doc = concat!("Upload a Java `", $what, "` to a fresh device buffer, reading the heap")]
        #[doc = "arena directly when the array is contiguous (else staged via a host `Vec`)."]
        pub fn $up(
            ctx: &DeviceContext,
            obj: ObjectRef,
            heap: &VmHeap,
            token: &SafepointToken<'_>,
        ) -> DeviceResult<DeviceBuffer<$ty>> {
            let len = heap.get_header(obj).array_length() as usize;
            // Only take the zero-copy fast path once we have proven the object
            // really is an array of the expected element type — otherwise the
            // raw `from_raw_parts` below would read past the payload (OOB).
            // A failed check (wrong kind / element type) falls through to the
            // staged path, whose `host_view_*` asserts produce a clean panic.
            if zerocopy_enabled() && len != 0 && zerocopy_shape_ok(obj, heap, $elem) {
                if let Some(src) = heap.array_data_ptr(obj) {
                    // SAFETY: `obj` is a live contiguous `$elem` array of `len`
                    // elements at `src` (native-aligned primitive storage, kind
                    // + element type verified by `zerocopy_shape_ok`, so the
                    // payload is exactly `len * size_of::<$ty>()` bytes); the
                    // held `token` pins the GC, so the arena cannot move and no
                    // Java thread observes it during this read-only upload.
                    let slice = unsafe { std::slice::from_raw_parts(src as *const $ty, len) };
                    return DeviceBuffer::from_host(ctx, slice);
                }
            }
            DeviceBuffer::from_host(ctx, &$host_view(obj, heap, token))
        }

        #[doc = concat!("Download a device buffer back into a Java `", $what, "`, writing the heap")]
        #[doc = "arena directly when the array is contiguous (else staged + write-back)."]
        pub fn $down(
            buf: &DeviceBuffer<$ty>,
            obj: ObjectRef,
            heap: &VmHeap,
            token: &SafepointToken<'_>,
        ) -> DeviceResult<()> {
            let len = heap.get_header(obj).array_length() as usize;
            // As in `$up`: validate kind + element type before reinterpreting
            // the arena as `&mut [$ty]`, or a mismatched element width would
            // write `len * size_of::<$ty>()` bytes off the end of the real
            // payload, corrupting the heap. A failed check falls through to
            // the staged path (`write_back_*` re-asserts the shape).
            if zerocopy_enabled() && len != 0 && zerocopy_shape_ok(obj, heap, $elem) {
                if let Some(dst) = heap.array_data_ptr(obj) {
                    // SAFETY: live contiguous `$elem` array of `len` elements at
                    // `dst` (kind + element type verified by `zerocopy_shape_ok`,
                    // so the payload is exactly `len * size_of::<$ty>()` bytes);
                    // GC paused (token) so this dispatch thread has exclusive
                    // access to the arena for the device→host copy.
                    let slice = unsafe { std::slice::from_raw_parts_mut(dst as *mut $ty, len) };
                    return buf.to_host(slice);
                }
            }
            let mut staged = vec![<$ty>::default(); len];
            buf.to_host(&mut staged)?;
            $write_back(obj, heap, &staged, token);
            Ok(())
        }
    };
}

direct_xfer!(
    upload_obj_i32,
    download_obj_i32,
    i32,
    ArrayElementType::Int,
    host_view_i32,
    write_back_i32,
    "int[]"
);
direct_xfer!(
    upload_obj_i64,
    download_obj_i64,
    i64,
    ArrayElementType::Long,
    host_view_i64,
    write_back_i64,
    "long[]"
);
direct_xfer!(
    upload_obj_f32,
    download_obj_f32,
    f32,
    ArrayElementType::Float,
    host_view_f32,
    write_back_f32,
    "float[]"
);
direct_xfer!(
    upload_obj_f64,
    download_obj_f64,
    f64,
    ArrayElementType::Double,
    host_view_f64,
    write_back_f64,
    "double[]"
);
// `short[]`/`byte[]` admit the same zero-copy `from_raw_parts` reinterpret
// as the 32/64-bit types above: their heap slots are natural-width and
// packed (see `host_view_i16`/`host_view_i8`), so `zerocopy_shape_ok` +
// `array_data_ptr` give exactly the same contiguous-span guarantee. The
// element types also satisfy `cuda_bridge::DeviceElem`: `i16`/`i8` are
// `bytemuck::Pod`, and (under the `cuda` feature) `cudarc::driver`
// implements `DeviceRepr`/`ValidAsZeroBits`/`Unpin` for every primitive
// integer width, `i16`/`i8` included — see `cuda_bridge::DeviceElem`'s own
// doc comment listing "i8, i32, i64, f32, f64, …".
direct_xfer!(
    upload_obj_i16,
    download_obj_i16,
    i16,
    ArrayElementType::Short,
    host_view_i16,
    write_back_i16,
    "short[]"
);
direct_xfer!(
    upload_obj_i8,
    download_obj_i8,
    i8,
    ArrayElementType::Byte,
    host_view_i8,
    write_back_i8,
    "byte[]"
);
// char[] joined on 2026-09-03. `u16` satisfies `cuda_bridge::DeviceElem`
// through its blanket `T: bytemuck::Pod + Send + Sync` impl, and the heap
// slot is natural-width and packed exactly as `short[]`'s is.
direct_xfer!(
    upload_obj_u16,
    download_obj_u16,
    u16,
    ArrayElementType::Char,
    host_view_u16,
    write_back_u16,
    "char[]"
);

macro_rules! async_xfer {
    ($up:ident, $down:ident, $ty:ty, $elem:expr, $host_view:path, $write_back:path, $what:literal) => {
        #[doc = concat!("Async upload of a Java `", $what, "` onto `stream`, using zero-copy pinned")]
        #[doc = "arena DMA when `HostRegionPinCache` pins the containing `VmHeap` span, or"]
        #[doc = "falling back cleanly to `PinnedHostPool` async staging."]
        pub fn $up(
            ctx: &DeviceContext,
            obj: ObjectRef,
            heap: &VmHeap,
            token: &SafepointToken<'_>,
            stream: &cuda_bridge::Stream,
            pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
            pool: &cuda_bridge::pinned_pool::PinnedHostPool,
        ) -> DeviceResult<(DeviceBuffer<$ty>, AsyncDmaLifetime)> {
            let len = heap.get_header(obj).array_length() as usize;
            if len == 0 {
                return Ok((
                    DeviceBuffer::from_host(ctx, &[])?,
                    AsyncDmaLifetime::Completed,
                ));
            }
            if zerocopy_enabled() && zerocopy_shape_ok(obj, heap, $elem) {
                if let Some(src) = heap.array_data_ptr(obj) {
                    // SAFETY: `obj` is a verified contiguous `$elem` array of
                    // `len` elements at `src`; `token` pins GC during setup and
                    // `Relocation::Forbidden` + `PinnedSpanGuard` (or pooled
                    // staging copy below) protect the transfer on `stream`.
                    let slice = unsafe { std::slice::from_raw_parts(src as *const $ty, len) };
                    if let Some(guard) = try_pin_arena_span_for_dma(
                        src as usize,
                        std::mem::size_of_val(slice),
                        heap,
                        pin_cache,
                    ) {
                        // SAFETY: `[src, src + bytes)` is inside the pinned arena
                        // span; caller keeps both `guard` and
                        // `Relocation::Forbidden` alive until `stream` retires.
                        let buf = unsafe {
                            DeviceBuffer::from_host_async_unchecked(ctx, slice, stream)
                        }?;
                        return Ok((buf, AsyncDmaLifetime::ZeroCopySpan(guard)));
                    }
                    let buf = DeviceBuffer::from_host_async_pooled(ctx, slice, stream, pool)?;
                    return Ok((buf, AsyncDmaLifetime::Completed));
                }
            }
            let staged = $host_view(obj, heap, token);
            let buf = DeviceBuffer::from_host_async_pooled(ctx, &staged, stream, pool)?;
            Ok((buf, AsyncDmaLifetime::Completed))
        }

        #[doc = concat!("Async download of a device buffer back into a Java `", $what, "` on `stream`,")]
        #[doc = "issuing zero-copy pinned D2H DMA into the Java arena when pinned or falling back"]
        #[doc = "to `PinnedHostPool` async staging."]
        pub fn $down(
            buf: &DeviceBuffer<$ty>,
            obj: ObjectRef,
            heap: &VmHeap,
            token: &SafepointToken<'_>,
            stream: &cuda_bridge::Stream,
            pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
            pool: &cuda_bridge::pinned_pool::PinnedHostPool,
        ) -> DeviceResult<AsyncDmaLifetime> {
            let len = heap.get_header(obj).array_length() as usize;
            if len == 0 {
                return Ok(AsyncDmaLifetime::Completed);
            }
            if zerocopy_enabled() && zerocopy_shape_ok(obj, heap, $elem) {
                if let Some(dst) = heap.array_data_ptr(obj) {
                    // SAFETY: verified contiguous `$elem` array of `len` elements
                    // at `dst`; `token` and `Relocation::Forbidden` guard writeback.
                    let slice = unsafe { std::slice::from_raw_parts_mut(dst as *mut $ty, len) };
                    if let Some(guard) = try_pin_arena_span_for_dma(
                        dst as usize,
                        std::mem::size_of_val(slice),
                        heap,
                        pin_cache,
                    ) {
                        // SAFETY: `[dst, dst + bytes)` is pinned; caller
                        // synchronizes `stream` before releasing `guard` or
                        // `Relocation::Forbidden`.
                        unsafe { buf.to_host_async_unchecked(slice, stream) }?;
                        return Ok(AsyncDmaLifetime::ZeroCopySpan(guard));
                    }
                    buf.to_host_async_pooled(slice, stream, pool)?;
                    return Ok(AsyncDmaLifetime::Completed);
                }
            }
            let mut staged = vec![<$ty>::default(); len];
            buf.to_host_async_pooled(&mut staged, stream, pool)?;
            $write_back(obj, heap, &staged, token);
            Ok(AsyncDmaLifetime::Completed)
        }
    };
}

async_xfer!(
    upload_obj_async_i32,
    download_obj_async_i32,
    i32,
    ArrayElementType::Int,
    host_view_i32,
    write_back_i32,
    "int[]"
);
async_xfer!(
    upload_obj_async_i64,
    download_obj_async_i64,
    i64,
    ArrayElementType::Long,
    host_view_i64,
    write_back_i64,
    "long[]"
);
async_xfer!(
    upload_obj_async_f32,
    download_obj_async_f32,
    f32,
    ArrayElementType::Float,
    host_view_f32,
    write_back_f32,
    "float[]"
);
async_xfer!(
    upload_obj_async_f64,
    download_obj_async_f64,
    f64,
    ArrayElementType::Double,
    host_view_f64,
    write_back_f64,
    "double[]"
);
async_xfer!(
    upload_obj_async_i16,
    download_obj_async_i16,
    i16,
    ArrayElementType::Short,
    host_view_i16,
    write_back_i16,
    "short[]"
);
async_xfer!(
    upload_obj_async_i8,
    download_obj_async_i8,
    i8,
    ArrayElementType::Byte,
    host_view_i8,
    write_back_i8,
    "byte[]"
);
async_xfer!(
    upload_obj_async_u16,
    download_obj_async_u16,
    u16,
    ArrayElementType::Char,
    host_view_u16,
    write_back_u16,
    "char[]"
);

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

/// Copy `src` into `obj`'s payload starting at element `lo`.
///
/// The ranged twin of the whole-array `write_back_*`, for the chunked
/// writeback: each chunk lands in its own slice of the Java array as its
/// DMA completes, rather than the whole array arriving at once. Same
/// no-write-barrier argument (primitive arrays never hold references) and
/// same contiguity fallback.
macro_rules! write_back_range {
    ($name:ident, $ty:ty, $elem:expr, $vconv:expr, $what:literal) => {
        pub fn $name(
            obj: ObjectRef,
            heap: &VmHeap,
            src: &[$ty],
            lo: usize,
            _token: &SafepointToken<'_>,
        ) -> Result<(), String> {
            let header = heap.get_header(obj);
            if header.kind() != ObjectKind::Array || header.element_type() != $elem {
                return Err(concat!(stringify!($name), ": not a ", $what).to_string());
            }
            let len = header.array_length() as usize;
            let end = lo
                .checked_add(src.len())
                .ok_or_else(|| format!("{}: lo + len overflows", stringify!($name)))?;
            if end > len {
                return Err(format!(
                    "{}: range [{lo}, {end}) exceeds array length {len}",
                    stringify!($name)
                ));
            }
            if src.is_empty() {
                return Ok(());
            }
            match heap.array_data_ptr(obj) {
                Some(dst) => {
                    // SAFETY: `obj` is a live `$what` of `len` elements
                    // (kind + element type checked above), the range is
                    // bounds-checked against it, and GC is paused for the
                    // duration (token held), so `dst` stays valid. `src` is
                    // page-locked staging owned by the caller and cannot
                    // overlap the heap arena.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src.as_ptr() as *const u8,
                            dst.add(lo * std::mem::size_of::<$ty>()),
                            std::mem::size_of_val(src),
                        );
                    }
                }
                // G1 humongous: region-safe per-element store.
                None => {
                    for (i, &x) in src.iter().enumerate() {
                        heap.set_array_element(obj, lo + i, $vconv(x))
                            .map_err(|_| format!("{}: OOB at {}", stringify!($name), lo + i))?;
                    }
                }
            }
            Ok(())
        }
    };
}

write_back_range!(
    write_back_range_i32,
    i32,
    ArrayElementType::Int,
    Value::Int,
    "int[]"
);
write_back_range!(
    write_back_range_i64,
    i64,
    ArrayElementType::Long,
    Value::Long,
    "long[]"
);
write_back_range!(
    write_back_range_f32,
    f32,
    ArrayElementType::Float,
    Value::Float,
    "float[]"
);
write_back_range!(
    write_back_range_f64,
    f64,
    ArrayElementType::Double,
    Value::Double,
    "double[]"
);
// short[]/byte[] became offloadable on 2026-09-02 but were left out of the
// chunked writeback, so any dispatch carrying one gave up copy/compute
// overlap for the whole dispatch (`take_chunkable_writeback` bails when it
// sees an array writeback it does not recognise). These close that.
write_back_range!(
    write_back_range_i16,
    i16,
    ArrayElementType::Short,
    // The slow per-element arm stores through `Value::Int`, so the
    // narrow element widens on the way in; `set_array_element` narrows
    // it back per the array's own element type.
    |v: i16| Value::Int(v as i32),
    "short[]"
);
write_back_range!(
    write_back_range_u16,
    u16,
    ArrayElementType::Char,
    // Zero-extended into the int slot, unlike short[]'s sign extension.
    |v: u16| Value::Int(v as i32),
    "char[]"
);
write_back_range!(
    write_back_range_i8,
    i8,
    ArrayElementType::Byte,
    // The slow per-element arm stores through `Value::Int`, so the
    // narrow element widens on the way in; `set_array_element` narrows
    // it back per the array's own element type.
    |v: i8| Value::Int(v as i32),
    "byte[]"
);
