/// Phase 6 #3: detect a `craton.gpu.GpuArray` Java object and read
/// its (element_type, length, native handle) tuple from the
/// resident-array store. Returns `None` for any non-GpuArray object so
/// callers can fall through to other arg shapes.
///
/// **Shape only, deliberately.** This used to return the host bytes
/// too, which meant every submit copied the whole array out of the
/// resident store — including the overwhelmingly common case where the
/// buffer was already on the device and the copy was thrown away
/// unread. For a weight tensor that is resident for the life of the
/// process (a language model's, say) that is the entire point of
/// residency undone: gigabytes of `memcpy` per kernel launch. The bytes
/// are now fetched by `marshal_resident_array_arg` on the cache-miss
/// path only.
///
/// The GpuArray Java layout (P3-2):
///   field 0: `long handle`
///   field 1: `Class<?> elementType`  (unused here; type comes from
///                                     the resident-store record)
#[cfg(feature = "gpu-offload")]
fn try_gpu_array_shape(
    shared: &crate::vm::SharedVm,
    cache: &OffloadCache,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<(cratonvm_types::ArrayElementType, usize, u64)> {
    cratonvm_types::gpu_dispatch_memo_census::note_array_probe();
    // One integer compare against a class id resolved once for the process.
    // This used to take the class-manager read lock and clone the class's
    // name into a fresh `String` to compare it -- per ARGUMENT, per
    // dispatch. GPULlama3 marshals ~5 arguments across 453 dispatches a
    // token, so that was 2,265 lock acquisitions and 2,265 allocations per
    // token to answer a question that is one `==`.
    let cid = shared.mem.heap.class_id_of(obj_ref);
    if cache.gpu_array_class_id(shared) != Some(cid) {
        return None;
    }
    // field 0 holds the long `handle`.
    let handle = match shared.mem.heap.get_field(obj_ref, 0) {
        cratonvm_types::Value::Long(h) => h as u64,
        _ => return None,
    };
    let (etype, len) = cratonvm_native_builtins::craton_gpu::array_shape(handle)?;
    Some((etype, len, handle))
}

/// Remember one declared parameter's array length, growing the vector
/// as needed. Indexed by the analyzer's declared-parameter index — the
/// same index `writes_param_mask` is bit-indexed by, and the one
/// `WorkBound::ParamLen` names.
#[cfg(feature = "gpu-offload")]
/// Remember an `int` scalar argument's value by declared-parameter
/// index, for [`jit_cuda::emitter::WorkBound::ParamScalar`].
fn record_param_scalar(param_scalars: &mut Vec<Option<i32>>, index: usize, value: i32) {
    if param_scalars.len() <= index {
        param_scalars.resize(index + 1, None);
    }
    param_scalars[index] = Some(value);
}

fn record_param_len(param_lens: &mut Vec<Option<usize>>, index: usize, len: usize) {
    if param_lens.len() <= index {
        param_lens.resize(index + 1, None);
    }
    param_lens[index] = Some(len);
}

/// Build kernel argument push closure + optional resident writeback from an
/// already-cloned `device_cache::CachedBuffer` so callers (`chain_submission_from_native`
/// and `marshal_resident_array_arg`) share one path once the `Arc<DeviceBuffer<T>>`
/// has been atomically retrieved under the cache lock.
#[cfg(feature = "gpu-offload")]
fn marshal_from_cached_entry(
    arr_handle: u64,
    cached: device_cache::CachedBuffer,
    len: usize,
    produce_writeback: bool,
) -> Result<
    (
        Box<dyn FnOnce(cuda_bridge::KernelArgs) -> cuda_bridge::KernelArgs>,
        Option<MarshalWriteback>,
        std::sync::Arc<dyn std::any::Any + Send + Sync>,
    ),
    String,
> {
    let retained = cached.as_any_arc();
    let len32 = len as i32;
    macro_rules! cached_arm {
        ($ty:ty, $arc:expr, $wb_variant:ident) => {{
            let wb_arc = $arc.clone();
            let push_arc = $arc;
            let device_ptr =
                std::sync::Arc::as_ptr(&push_arc) as *const cuda_bridge::DeviceBuffer<$ty>;
            let push: Box<dyn FnOnce(_) -> _> = Box::new(move |args: cuda_bridge::KernelArgs| {
                // SAFETY: `push_arc` stays alive across `args.push_device_ptr`
                // and either `MarshalWriteback` or `retained` keeps the buffer
                // alive for the submission's lifetime.
                let _ = &push_arc;
                let buf_ref: &cuda_bridge::DeviceBuffer<$ty> = unsafe { &*device_ptr };
                args.push_device_ptr(buf_ref).push_i32(len32)
            });
            let wb = if produce_writeback {
                Some(MarshalWriteback::$wb_variant {
                    handle: arr_handle,
                    buf: wb_arc,
                    len,
                })
            } else {
                None
            };
            (push, wb)
        }};
    }
    let (push, wb) = match cached {
        device_cache::CachedBuffer::I32(arc) => cached_arm!(i32, arc, ResidentI32),
        device_cache::CachedBuffer::I64(arc) => cached_arm!(i64, arc, ResidentI64),
        device_cache::CachedBuffer::F32(arc) => cached_arm!(f32, arc, ResidentF32),
        device_cache::CachedBuffer::F64(arc) => cached_arm!(f64, arc, ResidentF64),
        device_cache::CachedBuffer::I16(arc) => {
            if produce_writeback {
                return Err(
                    "marshal_from_cached_entry: I16 resident writeback unsupported".to_string(),
                );
            }
            let push_arc = arc;
            let device_ptr =
                std::sync::Arc::as_ptr(&push_arc) as *const cuda_bridge::DeviceBuffer<i16>;
            let push: Box<dyn FnOnce(_) -> _> = Box::new(move |args: cuda_bridge::KernelArgs| {
                let _ = &push_arc;
                let buf_ref: &cuda_bridge::DeviceBuffer<i16> = unsafe { &*device_ptr };
                args.push_device_ptr(buf_ref).push_i32(len32)
            });
            (push, None)
        }
    };
    Ok((push, wb, retained))
}

/// Marshal a resident GpuArray as a kernel arg. The shape of the
/// closure + writeback record mirrors `marshal_array_arg` but the
/// source bytes are the resident-store snapshot rather than a JVM
/// array — and the writeback target is the resident store (so a
/// subsequent `GpuArray.toHost()` reads the post-kernel content).
///
/// The host bytes are read from the resident store only when the
/// device cache misses; a hit never touches them.
#[cfg(feature = "gpu-offload")]
fn marshal_resident_array_arg(
    ctx: &cuda_bridge::DeviceContext,
    element_type: cratonvm_types::ArrayElementType,
    len: usize,
    arr_handle: u64,
    vm_identity: usize,
) -> Result<
    (
        Box<dyn FnOnce(cuda_bridge::KernelArgs) -> cuda_bridge::KernelArgs>,
        MarshalWriteback,
    ),
    String,
> {
    use cratonvm_types::ArrayElementType;

    // Each arm atomically clones `Arc<DeviceBuffer<T>>` via
    // `device_cache::get_typed` under the cache lock, uploads and validates
    // post-install liveness on miss, then builds the push closure + writeback
    // via `marshal_from_cached_entry`.
    macro_rules! resident_arm {
        ($ty:ty, $cached_ctor:path, $cache_put:path, $tag:literal) => {{
            let arc = if let Some(arc) = device_cache::get_typed::<$ty>(arr_handle) {
                arc
            } else {
                let host_bytes = cratonvm_native_builtins::craton_gpu::array_snapshot(arr_handle)
                    .map(|(_, _, bytes)| bytes)
                    .ok_or_else(|| {
                        format!("GpuArray handle {arr_handle} released before its first upload")
                    })?;
                let host: &[$ty] = bytemuck::cast_slice(&host_bytes);
                let buf = crate::runtime::gpu_marshal::upload(ctx, host)
                    .map_err(|e| format!("upload {} (GpuArray, len={len}): {e}", $tag))?;
                let arc = std::sync::Arc::new(buf);
                $cache_put(arr_handle, arc.clone(), vm_identity);
                if cratonvm_native_builtins::craton_gpu::array_shape(arr_handle).is_none() {
                    device_cache::release(arr_handle);
                    return Err(format!(
                        "GpuArray handle {arr_handle} released concurrently during upload"
                    ));
                }
                arc
            };
            let (push, wb_opt, _retained) =
                marshal_from_cached_entry(arr_handle, $cached_ctor(arc), len, true)?;
            let wb = wb_opt
                .ok_or_else(|| "marshal_resident_array_arg: missing writeback".to_string())?;
            (push, wb)
        }};
    }

    let (push, wb) = match element_type {
        ArrayElementType::Int => resident_arm!(
            i32,
            device_cache::CachedBuffer::I32,
            device_cache::put_i32,
            "i32"
        ),
        ArrayElementType::Long => resident_arm!(
            i64,
            device_cache::CachedBuffer::I64,
            device_cache::put_i64,
            "i64"
        ),
        ArrayElementType::Float => resident_arm!(
            f32,
            device_cache::CachedBuffer::F32,
            device_cache::put_f32,
            "f32"
        ),
        ArrayElementType::Double => resident_arm!(
            f64,
            device_cache::CachedBuffer::F64,
            device_cache::put_f64,
            "f64"
        ),
        other => {
            return Err(format!(
                "submitMethod: GpuArray element type {other:?} unsupported"
            ));
        }
    };
    Ok((push, wb))
}

/// Phase 6 #2: detect Java's autoboxed primitives and unwrap them
/// to the underlying `Value::Int/Long/Float/Double`. Java's
/// `Object[] args` always boxes scalars passed via varargs (`int 5`
/// becomes `Integer.valueOf(5)`), so without this every scalar arg
/// to `submitMethod` would be misclassified as "not an array".
///
/// Returns `None` if the object is not one of the eight wrapper
/// classes, or if its `value` field (slot 0) doesn't carry a
/// primitive variant. Byte/Short/Character/Boolean wrappers all
/// store an `Int` internally (matching the JVM's stack
/// representation) and surface here as `Value::Int(...)`.
#[cfg(feature = "gpu-offload")]
fn try_unbox_primitive(
    shared: &crate::vm::SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::Value> {
    let cid = shared.mem.heap.class_id_of(obj_ref);
    let cm = shared.classes.class_manager.read();
    let cls_name = cm.get_class(cid).map(|c| c.name.to_string())?;
    drop(cm);
    let inner = shared.mem.heap.get_field(obj_ref, 0);
    match cls_name.as_str() {
        "java/lang/Integer"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Boolean"
        | "java/lang/Character" => match inner {
            cratonvm_types::Value::Int(_) => Some(inner),
            _ => None,
        },
        "java/lang/Long" => match inner {
            cratonvm_types::Value::Long(_) => Some(inner),
            // Some MethodHandle paths store the long as Int — coerce.
            cratonvm_types::Value::Int(v) => Some(cratonvm_types::Value::Long(v as i64)),
            _ => None,
        },
        "java/lang/Float" => match inner {
            cratonvm_types::Value::Float(_) => Some(inner),
            cratonvm_types::Value::Int(v) => {
                Some(cratonvm_types::Value::Float(f32::from_bits(v as u32)))
            }
            _ => None,
        },
        "java/lang/Double" => match inner {
            cratonvm_types::Value::Double(_) => Some(inner),
            cratonvm_types::Value::Long(v) => {
                Some(cratonvm_types::Value::Double(f64::from_bits(v as u64)))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Marshal one Java primitive-array arg. Returns:
///   - a closure that pushes `(device_ptr, length)` into the
///     in-flight `KernelArgs` (the closure form sidesteps borrowing
///     `kernel_args` mutably while we still hold `shared`)
///   - a `MarshalWriteback` recording how to copy the device buffer
///     back into the source Java array after the kernel finishes.
///
/// Phase 10 #1 — input-residency: before doing the H->D copy, we
/// consult `input_cache` keyed by `ObjectRef`. On a hit (same Java
/// array seen on a previous submit, contents still in sync with the
/// device buffer), the H->D upload AND the `host_view_*` heap memcpy
/// are skipped entirely — we just hand back the cached
/// `Arc<DeviceBuffer<T>>`. On a miss the existing
/// `host_view → upload` path runs and the resulting Arc is installed
/// into the cache so the next submit hits.
///
/// Phase 10 #2 — `is_kernel_written` tells us whether the kernel
/// body actually `*astore`s into this parameter slot. When it does
/// not (read-only input, e.g. `a` and `b` in `vectorAdd(a, b, out)`),
/// we return `None` for the writeback — the post-launch D→H copy
/// (256MB for a 2^26-int input at vectorAdd scale) is pure waste
/// because the device contents already match the host. Returning
/// `None` saves the matching `wb_arc` clone the cache-only path
/// would have produced; the Arc inside `input_cache` keeps the
/// buffer alive across submits.
/// The array element types [`marshal_array_arg`] can hand to a device.
///
/// **INVARIANT: this is exactly the set
/// `jit_cuda::analyzer::ParamKind::from_field` admits for
/// `FieldType::Array(_)`.** The two are on opposite sides of the
/// offload pipeline and nothing structural forces them to agree, so
/// both directions of disagreement are guarded:
///
/// * A type the *analyzer* admits and this refuses is INVISIBLE at
///   runtime. The kernel is analyzed, lowered to PTX, compiled and
///   cached; the dispatch then fails at marshalling and the VM falls
///   back to the interpreter. The answers stay correct, so no
///   differential test can see it -- the arm is comparing the
///   interpreter with itself. That is exactly how `short[]` and
///   `byte[]` stayed unreachable from the day their marshalling was
///   written until 2026-09-02, with `gpu_marshal`'s
///   `direct_xfer!(upload_obj_i16, ...)` fully implemented and unit
///   tested the whole time.
/// * A type this claims and `marshal_array_arg` has no arm for trips
///   the `debug_assert!` in that function's catch-all.
///
/// `analyzer_and_marshaller_admit_the_same_arrays` in
/// `vm/tests/gpu_offload_features.rs` pins the first direction; it
/// needs no device, which is the point -- the gap it guards is one
/// that only end-to-end hardware runs could otherwise expose, and then
/// only if they counted dispatches rather than compared values.
pub fn is_marshallable_array_element(t: cratonvm_types::ArrayElementType) -> bool {
    use cratonvm_types::ArrayElementType as A;
    matches!(
        t,
        A::Int | A::Long | A::Float | A::Double | A::Short | A::Byte | A::Char | A::Boolean
    )
}

#[cfg(feature = "gpu-offload")]
fn marshal_array_arg(
    shared: &crate::vm::SharedVm,
    ctx: &cuda_bridge::DeviceContext,
    stream: &cuda_bridge::Stream,
    pin_cache: &cuda_bridge::pinned_pool::HostRegionPinCache,
    pool: &cuda_bridge::pinned_pool::PinnedHostPool,
    obj_ref: cratonvm_types::ObjectRef,
    element_type: cratonvm_types::ArrayElementType,
    is_kernel_written: bool,
    can_fail_after_store: bool,
    token: &cratonvm_gc::safepoint::SafepointToken<'_>,
) -> Result<
    (
        Box<dyn FnOnce(cuda_bridge::KernelArgs) -> cuda_bridge::KernelArgs>,
        Option<MarshalWriteback>,
        usize,
        crate::runtime::gpu_marshal::AsyncDmaLifetime,
        std::sync::Arc<dyn std::any::Any + Send + Sync>,
    ),
    String,
> {
    use crate::runtime::gpu_marshal;
    use cratonvm_types::ArrayElementType;
    use std::sync::Arc;

    // Resolve the array length without copying the payload — the
    // cache key is (ObjectRef, element_type, len). A mismatch on
    // length forces a fresh upload (an array can't change length
    // on the JVM heap without becoming a different ObjectRef, but
    // we treat length as part of the validity envelope defensively).
    let len = shared.mem.heap.array_length(obj_ref);

    // Macro to keep the four arms readable. Each arm:
    //   (a) consult input_cache via the type's getter — on hit, skip
    //       host_view + upload and reuse the cached Arc.
    //   (b) on miss, perform async H2D via `upload_obj_async_*` (pinned
    //       zero-copy arena DMA or `PinnedHostPool` fallback), then
    //       install the Arc into the cache so future submits hit.
    //   (c) build the (push closure, optional-writeback, retained Arc)
    //       tuple. Read-only params still keep their `Arc` alive on
    //       `FinalizeState::retained_buffers` until kernel retirement.
    macro_rules! arm {
        (
            $ty:ty,
            $variant:ident,
            $upload_obj_async:path,
            $cache_get:path,
            $cache_put:path,
            $tag:literal,
            $elem_size:expr
        ) => {{
            // (0) On integrated shared-memory GPUs (Ryzen AI APUs), alias
            //     read-only arrays — and writable arrays when no kernel
            //     bounds/deopt failure flag can abort mid-execution — directly
            //     over JVM heap memory without VRAM allocation or copies.
            if ctx.is_shared_memory_gpu()
                && gpu_marshal::zerocopy_enabled()
                && (!is_kernel_written || !can_fail_after_store)
                && len > 0
                && gpu_marshal::zerocopy_shape_ok(obj_ref, &shared.mem.heap, element_type)
            {
                if let Some(ptr) = shared.mem.heap.array_data_ptr(obj_ref) {
                    let byte_len = len * $elem_size;
                    // SAFETY: `token` keeps the safepoint stable and `zerocopy_shape_ok`
                    // confirmed the object's element layout matches `$ty`.
                    let host_slice: &[$ty] =
                        unsafe { std::slice::from_raw_parts(ptr as *const $ty, len) };
                    let span_guard = gpu_marshal::try_pin_arena_span_for_dma(
                        ptr as usize,
                        byte_len,
                        &shared.mem.heap,
                        pin_cache,
                    );
                    let mapped_dev_ptr = span_guard
                        .as_ref()
                        .and_then(|g| g.device_ptr_for(ptr as usize, byte_len));
                    // SAFETY: `AsyncDmaLifetime::SharedHostSpan` retains `Relocation::Forbidden`
                    // and stream synchronization until `finalize_submission`.
                    if let Ok(buf) = unsafe {
                        cuda_bridge::DeviceBuffer::<$ty>::from_shared_host_unchecked(
                            ctx,
                            host_slice,
                            mapped_dev_ptr,
                            None,
                        )
                    } {
                        let arc = Arc::new(buf);
                        let len32 = len as i32;
                        let wb_opt = if is_kernel_written {
                            Some(MarshalWriteback::$variant {
                                obj: obj_ref,
                                buf: arc.clone(),
                                len,
                            })
                        } else {
                            None
                        };
                        let retained: Arc<dyn std::any::Any + Send + Sync> = arc.clone();
                        let device_ptr = Arc::as_ptr(&arc) as *const cuda_bridge::DeviceBuffer<$ty>;
                        let push: Box<dyn FnOnce(_) -> _> =
                            Box::new(move |args: cuda_bridge::KernelArgs| {
                                let _ = &arc;
                                // SAFETY: `arc` stays live in `retained_buffers` / `writebacks`.
                                let buf_ref: &cuda_bridge::DeviceBuffer<$ty> =
                                    unsafe { &*device_ptr };
                                args.push_device_ptr(buf_ref).push_i32(len32)
                            });
                        return Ok((
                            push,
                            wb_opt,
                            0,
                            gpu_marshal::AsyncDmaLifetime::SharedHostSpan(span_guard),
                            retained,
                        ));
                    }
                }
            }
            // (a) Cache check.
            let (arc, uploaded, dma_life): (
                Arc<cuda_bridge::DeviceBuffer<$ty>>,
                bool,
                gpu_marshal::AsyncDmaLifetime,
            ) = if let Some(arc) = $cache_get(shared.vm_identity, obj_ref, len) {
                (arc, false, gpu_marshal::AsyncDmaLifetime::Completed)
            } else {
                // (b) Miss — async upload on `stream`, installing the Arc into
                //     the input cache on success.
                let (buf, dma_life) = $upload_obj_async(
                    ctx,
                    obj_ref,
                    &shared.mem.heap,
                    token,
                    stream,
                    pin_cache,
                    pool,
                )
                .map_err(|e| format!("upload {} (len={len}): {e}", $tag))?;
                let arc = Arc::new(buf);
                $cache_put(shared.vm_identity, obj_ref, len, arc.clone());
                (arc, true, dma_life)
            };
            // (c) Build push closure + optional writeback. Read-only
            //     params skip the writeback (no D→H copy on the
            //     finalize path) while returning a retained `Arc` so the
            //     buffer outlives any concurrent cache eviction.
            let len32 = len as i32;
            let wb_opt = if is_kernel_written {
                Some(MarshalWriteback::$variant {
                    obj: obj_ref,
                    buf: arc.clone(),
                    len,
                })
            } else {
                None
            };
            let retained: Arc<dyn std::any::Any + Send + Sync> = arc.clone();
            let device_ptr = Arc::as_ptr(&arc) as *const cuda_bridge::DeviceBuffer<$ty>;
            let push: Box<dyn FnOnce(_) -> _> = Box::new(move |args: cuda_bridge::KernelArgs| {
                // SAFETY: `arc` is moved into this closure (kept
                // alive at least until it fires). The closure
                // runs once during the dispatch sequence and
                // immediately pushes the pointer into KernelArgs;
                // after that `retained_buffers` / writeback keep the buffer
                // alive through launch and completion.
                let _ = &arc;
                let buf_ref: &cuda_bridge::DeviceBuffer<$ty> = unsafe { &*device_ptr };
                args.push_device_ptr(buf_ref).push_i32(len32)
            });
            let bytes_uploaded = if uploaded { len * $elem_size } else { 0 };
            (push, wb_opt, bytes_uploaded, dma_life, retained)
        }};
    }

    let (push, wb, bytes_uploaded, dma_life, retained) = match element_type {
        ArrayElementType::Int => arm!(
            i32,
            I32,
            gpu_marshal::upload_obj_async_i32,
            input_cache::get_i32,
            input_cache::put_i32,
            "i32",
            4
        ),
        ArrayElementType::Long => arm!(
            i64,
            I64,
            gpu_marshal::upload_obj_async_i64,
            input_cache::get_i64,
            input_cache::put_i64,
            "i64",
            8
        ),
        ArrayElementType::Float => arm!(
            f32,
            F32,
            gpu_marshal::upload_obj_async_f32,
            input_cache::get_f32,
            input_cache::put_f32,
            "f32",
            4
        ),
        ArrayElementType::Double => arm!(
            f64,
            F64,
            gpu_marshal::upload_obj_async_f64,
            input_cache::get_f64,
            input_cache::put_f64,
            "f64",
            8
        ),
        // short[]/byte[] marshal exactly like the four above:
        // `gpu_marshal`'s `direct_xfer!` generates their upload/download
        // pair from the same macro, and their heap slots are
        // natural-width and packed, so the zero-copy reinterpret has the
        // same contiguous-span guarantee.
        //
        // They were unreachable until 2026-09-02. The analyzer admitted a
        // short[]/byte[] kernel and the lowering emitted PTX for it, but
        // this match had no arm, so every such dispatch died below with
        // "unsupported array element type" and fell back to the
        // interpreter -- correct answers, never offloaded, and nothing
        // said so above `WARN`.
        ArrayElementType::Short => arm!(
            i16,
            I16,
            gpu_marshal::upload_obj_async_i16,
            input_cache::get_i16,
            input_cache::put_i16,
            "i16",
            2
        ),
        // `boolean[]` shares every byte of `byte[]`'s path deliberately:
        // `element_byte_size` is 1 for both, so the upload, the cache
        // buffer and the write-back are bit-identical. The two types
        // diverge only on the DEVICE, where `bastore` into a boolean
        // array must mask to bit 0 (JVMS 6.5) -- which is why the
        // analyzer gives `boolean[]` its own `ParamKind::BoolArray` while
        // the marshaller does not need to tell them apart.
        ArrayElementType::Byte | ArrayElementType::Boolean => arm!(
            i8,
            I8,
            gpu_marshal::upload_obj_async_i8,
            input_cache::get_i8,
            input_cache::put_i8,
            "i8",
            1
        ),
        // char[] (2026-09-03). The emitter already lowered `caload` /
        // `castore`; only the analyzer's admission and this arm were
        // missing, so a char[] kernel was refused here exactly as
        // short[]/byte[] were before 2026-09-02.
        ArrayElementType::Char => arm!(
            u16,
            U16,
            gpu_marshal::upload_obj_async_u16,
            input_cache::get_u16,
            input_cache::put_u16,
            "u16",
            2
        ),
        other => {
            // If the predicate says this type is marshallable, the match
            // above owes it an arm. Loud in a debug build rather than a
            // silent interpreter fallback.
            debug_assert!(
                !is_marshallable_array_element(other),
                concat!(
                    "is_marshallable_array_element admits {:?} but the ",
                    "marshal loop has no arm for it -- a kernel taking ",
                    "this array type will compile, fail to dispatch, and ",
                    "quietly re-run on the interpreter"
                ),
                other
            );
            return Err(format!(
                "submitMethod: unsupported array element type: {other:?}"
            ));
        }
    };
    Ok((push, wb, bytes_uploaded, dma_life, retained))
}
