// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! cuda-oxide backend, built on NVlabs' `cuda-core` host runtime.
//!
//! # What this is
//!
//! [cuda-oxide] is NVIDIA's experimental Rust-to-PTX compiler. It is not
//! itself a driver bridge, so it cannot be dropped in where `cudarc`
//! sits. What it *runs on* can: cuda-oxide's host crate (`cuda-host`)
//! is layered over [`cuda-core`], the "idiomatic CUDA API" from
//! NVlabs/cutile-rs, and `cuda-core` is exactly a cudarc-shaped driver
//! bridge -- contexts, streams, events, modules, device buffers.
//!
//! This module implements the crate's backend contract against
//! `cuda-core`, so `--features cuda-oxide` selects NVIDIA's own host
//! stack instead of `cudarc`.
//!
//! [cuda-oxide]: https://github.com/NVlabs/cuda-oxide
//! [`cuda-core`]: https://crates.io/crates/cuda-core
//!
//! # Why the PTX path, not `#[cuda_module]`
//!
//! cuda-oxide's headline API is `#[cuda_module]`, which embeds artifact
//! bundles compiled from Rust at BUILD time. CratonVM cannot use it:
//! `jit-cuda` lowers JVM bytecode to PTX text at RUN time, and the
//! kernel does not exist until the VM has seen the method. The entry
//! point here is therefore `load_module_from_ptx_src`, cuda-core's
//! runtime `cuModuleLoadData` wrapper, which takes exactly the PTX
//! string the emitter already produces.
//!
//! # The zeroing memset must be ordered, not assumed
//!
//! An earlier draft of this backend zeroed with `cuMemsetD8_v2` and
//! called it synchronous, which made `record_alloc_event` a no-op. That
//! was wrong twice over: `cuMemsetD8` is asynchronous with respect to
//! the host for device memory, and it runs on the NULL stream, whose
//! implicit synchronisation reaches only BLOCKING streams -- while every
//! compute stream here is `CU_STREAM_NON_BLOCKING`. The memset was
//! therefore free to land AFTER a kernel's stores and wipe them.
//!
//! `concurrent_dispatch_it::one_context_many_threads` caught it, which
//! is the same test that caught the identical defect in the cudarc
//! backend (see that file's header). The zeroing is now issued on
//! `copy_h2d` and `record_alloc_event` records the buffer's last-write
//! marker on that stream, so a following launch waits on it.
//!
//! # Windows
//!
//! `cuda-core` 0.3.1 does not compile on Windows/MSVC: all 113 CUDA
//! driver enums bind as `c_int` there (MSVC gives a plain C enum a
//! SIGNED underlying type) while the crate expects `u32`, which is what
//! clang reports on Linux for an all-non-negative enum. That is an
//! upstream portability bug, not a design limit -- see
//! `docs/known-issues/gpu/` for the reproduction and the 13-edit fix.

use crate::{DeviceArch, DeviceCaps, DeviceError, KernelArgs, LaunchConfig, Result};
use cuda_core::{CudaContext, CudaFunction, CudaModule, CudaStream, IntoResult};
use std::collections::HashMap;
use std::sync::Arc;

/// Raw driver types and entry points, under the name the rest of the
/// crate uses. `cuda-core` re-exports the generated `cuda-bindings` FFI
/// as `cuda_core::sys`; the handle types (`CUevent`, `CUstream`,
/// `CUdeviceptr`, `CUgraph`, ...) are the same driver handles cudarc's
/// `driver::sys` exposes, so shared code can name them through this
/// alias without caring which backend is compiled in.
pub(crate) mod sys {
    pub(crate) use cuda_core::sys::*;
}

/// Map a `cuda-core` driver error into this crate's error type.
///
/// Takes the driver entry point's name so a failure says which call
/// refused, the same way the cudarc backend's mapper does.
fn map_err(what: &'static str) -> impl Fn(cuda_core::DriverError) -> DeviceError {
    move |e| DeviceError::Driver(format!("{what}: {e}"))
}

pub(crate) fn probe_device(device_ordinal: u32) -> Result<DeviceCaps> {
    let ctx = CudaContext::new(device_ordinal as usize).map_err(map_err("cuCtxCreate"))?;
    let name = ctx.device_name().map_err(map_err("cuDeviceGetName"))?;
    let (major, minor) = ctx
        .compute_capability()
        .map_err(map_err("cuDeviceComputeCapability"))?;
    let mut total: usize = 0;
    // SAFETY: `total` is a live out-param for the duration of the call
    // and `cu_device()` is this context's device handle.
    unsafe {
        sys::cuDeviceTotalMem_v2(&mut total, ctx.cu_device())
            .result()
            .map_err(map_err("cuDeviceTotalMem"))?;
    }
    Ok(DeviceCaps {
        ordinal: device_ordinal,
        name,
        compute_major: major as u32,
        compute_minor: minor as u32,
        total_global_mem: total as u64,
        arch: DeviceArch::Cuda {
            major: major as u32,
            minor: minor as u32,
        },
    })
}

/// The driver's CUDA version as `major * 1000 + minor * 10`.
pub(crate) fn driver_cuda_version() -> Result<u32> {
    let mut v: i32 = 0;
    // SAFETY: `v` is a live out-param; `cuDriverGetVersion` is callable
    // before any context exists.
    unsafe {
        sys::cuDriverGetVersion(&mut v)
            .result()
            .map_err(map_err("cuDriverGetVersion"))?;
    }
    Ok(v as u32)
}

// -- Event pool -------------------------------------------------------

/// How many idle events one context parks. Mirrors the cudarc backend:
/// two per in-flight submission, and the offload path warns at 1024 live
/// submissions.
const EVENT_POOL_CAP: usize = 4096;

/// Recycles `CUevent` handles so a submission does not pay
/// `cuEventCreate` per launch.
pub(crate) struct EventPool {
    free: std::sync::Mutex<Vec<sys::CUevent>>,
    ctx: Arc<CudaContext>,
}

// SAFETY: the pool holds raw `CUevent` handles plus the context that
// owns them; every method binds that context before touching a handle.
unsafe impl Send for EventPool {}
// SAFETY: the free list is behind a `Mutex`, and CUDA serializes event
// creation and destruction on the bound context.
unsafe impl Sync for EventPool {}

impl EventPool {
    /// Take a recycled handle, or `None` when the free list is empty.
    ///
    /// Deliberately does NOT create on a miss, matching the cudarc
    /// backend: the caller already has the create path, and a pool hit
    /// must not pay for `bind_to_thread`.
    pub(crate) fn take(&self) -> Option<sys::CUevent> {
        self.free.lock().unwrap_or_else(|p| p.into_inner()).pop()
    }

    /// Return a handle, or destroy it if the pool is full.
    pub(crate) fn put(&self, ev: sys::CUevent) {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        if free.len() < EVENT_POOL_CAP {
            free.push(ev);
            return;
        }
        drop(free);
        let _ = self.ctx.bind_to_thread();
        // SAFETY: the handle is uniquely owned here (its last `Event`
        // has just dropped) and its context was bound immediately above.
        unsafe {
            let _ = sys::cuEventDestroy_v2(ev);
        }
    }
}

impl Drop for EventPool {
    fn drop(&mut self) {
        let _ = self.ctx.bind_to_thread();
        for ev in self
            .free
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            // SAFETY: the pool is being dropped, so no `Event` holds any
            // of these handles; the context was bound above.
            unsafe {
                let _ = sys::cuEventDestroy_v2(ev);
            }
        }
    }
}

// -- Context ----------------------------------------------------------

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    ctx: Arc<CudaContext>,
    /// Dedicated transfer streams, mirroring the cudarc backend.
    ///
    /// These must NOT be `default_stream()`. cuda-core's default stream
    /// is the NULL stream, whose implicit synchronisation covers only
    /// BLOCKING streams -- and every compute stream this crate hands out
    /// comes from `fork`, which creates `CU_STREAM_NON_BLOCKING`. A
    /// blocking copy issued on the NULL stream is therefore NOT ordered
    /// after a kernel on a forked stream, so a `to_host` could read the
    /// buffer before its producing kernel had run. That is a measured
    /// failure, not a hypothetical: `concurrent_dispatch_it`'s
    /// `one_context_many_threads` read zeros for `4005`.
    copy_h2d: Arc<CudaStream>,
    copy_d2h: Arc<CudaStream>,
    event_pool: Arc<EventPool>,
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        let ctx = CudaContext::new(device_ordinal as usize).map_err(map_err("cuCtxCreate"))?;
        let copy_h2d = ctx
            .new_stream()
            .map_err(map_err("cuStreamCreate copy_h2d"))?;
        let copy_d2h = ctx
            .new_stream()
            .map_err(map_err("cuStreamCreate copy_d2h"))?;
        let event_pool = Arc::new(EventPool {
            free: std::sync::Mutex::new(Vec::new()),
            ctx: ctx.clone(),
        });
        Ok(Self {
            ctx,
            copy_h2d,
            copy_d2h,
            event_pool,
        })
    }

    /// Record `event` on the stream allocations are zeroed on.
    ///
    /// `DeviceBuffer::zeros` issues an ASYNCHRONOUS memset; this marker
    /// is what a later consumer waits on. Without it the zeroing can
    /// land after a kernel's stores and wipe them -- measured, not
    /// theoretical. See the module docs.
    pub(crate) fn record_alloc_event(&self, event: &crate::Event) -> Result<()> {
        // SAFETY: the event is owned by the caller's live `Event` and
        // the stream belongs to this context, bound just below.
        self.bind_to_thread()?;
        unsafe { drv::event_record(event.cu_event_raw(), drv::stream_raw(&self.copy_h2d)) }
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        self.ctx.synchronize().map_err(map_err("cuCtxSynchronize"))
    }

    pub(crate) fn bind_to_thread(&self) -> Result<()> {
        self.ctx
            .bind_to_thread()
            .map_err(map_err("cuCtxSetCurrent"))
    }

    /// The handle whose context owns everything this backend allocates.
    ///
    /// Named `device` to match the cudarc backend's accessor, so
    /// `event.rs` and `stream.rs` can hold "the thing that keeps the
    /// context alive" without naming a vendor type. Here that is the
    /// `CudaContext` itself; under cudarc it is a `CudaDevice`.
    pub(crate) fn device(&self) -> &Arc<CudaContext> {
        &self.ctx
    }

    /// The stream host-to-device copies are issued on.
    pub(crate) fn copy_h2d(&self) -> &Arc<CudaStream> {
        &self.copy_h2d
    }

    /// The stream device-to-host copies are issued on.
    pub(crate) fn copy_d2h(&self) -> &Arc<CudaStream> {
        &self.copy_d2h
    }

    pub(crate) fn event_pool(&self) -> &Arc<EventPool> {
        &self.event_pool
    }
}

// -- Module -----------------------------------------------------------

pub(crate) struct DeviceModuleInner {
    /// Held so the loaded module outlives every `CudaFunction` resolved
    /// out of it; the driver invalidates function handles when their
    /// module is unloaded.
    _module: Arc<CudaModule>,
    functions: HashMap<String, CudaFunction>,
}

impl DeviceModuleInner {
    pub(crate) fn from_ptx(
        ctx: &DeviceContextInner,
        ptx: &str,
        module_name: &str,
        kernel_names: &[&'static str],
    ) -> Result<Self> {
        let module = ctx
            .device()
            .load_module_from_ptx_src(ptx)
            .map_err(|e| DeviceError::Load(format!("{module_name}: {e}")))?;
        let mut functions = HashMap::with_capacity(kernel_names.len());
        for name in kernel_names {
            let func = module
                .load_function(name)
                .map_err(|_| DeviceError::KernelNotFound((*name).to_string()))?;
            functions.insert((*name).to_string(), func);
        }
        Ok(Self {
            _module: module,
            functions,
        })
    }

    /// The block size the driver's occupancy calculator prefers.
    ///
    /// `cuda-core` exposes `max_active_blocks_per_multiprocessor` and
    /// the cluster occupancy queries but NOT
    /// `cuOccupancyMaxPotentialBlockSize`, so this calls the driver
    /// entry point directly through `cuda_core::sys`. `None` on any
    /// refusal: the caller falls back to a fixed block size, and a
    /// missing autotune is not an error.
    pub(crate) fn optimal_block_size(&self, ctx: &DeviceContextInner, kernel: &str) -> Option<u32> {
        let func = self.functions.get(kernel)?;
        ctx.bind_to_thread().ok()?;
        let mut min_grid: i32 = 0;
        let mut block: i32 = 0;
        // SAFETY: both out-params are live for the call; the function
        // handle belongs to this module and its context is bound above.
        // A null block-size-to-dynamic-shared-mem callback with a zero
        // dynamic allocation is the documented "fixed shared memory"
        // form of this query.
        unsafe {
            sys::cuOccupancyMaxPotentialBlockSize(
                &mut min_grid,
                &mut block,
                func.cu_function(),
                None,
                0,
                0,
            )
            .result()
            .ok()?;
        }
        (block > 0).then_some(block as u32)
    }

    pub(crate) fn launch_raw_on_stream(
        &self,
        ctx: &DeviceContextInner,
        stream: &Arc<CudaStream>,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        let func = self
            .functions
            .get(kernel)
            .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?;
        ctx.bind_to_thread()?;

        // The CUDA kernel-parameter ABI wants a pointer TO each argument
        // value, not the value itself. Device addresses therefore need a
        // stable 8-byte slot to point at; `ptrs` provides that storage.
        //
        // LIFETIME INVARIANT: every pointer in `params` borrows into
        // either `args.raw` or `ptrs`. Both must stay live and
        // un-reallocated until the launch returns, which is why `ptrs`
        // is fully built (and never pushed to again) before any address
        // of an element is taken.
        let mut ptrs: Vec<u64> = Vec::with_capacity(args.raw.len());
        for a in &args.raw {
            if let crate::KernelArg::DevicePtr { addr, .. } = a {
                ptrs.push(*addr);
            }
        }
        let mut ip = 0usize;
        let mut params: Vec<*mut std::ffi::c_void> = Vec::with_capacity(args.raw.len());
        for a in &args.raw {
            params.push(match a {
                crate::KernelArg::I32(v) => v as *const i32 as *mut std::ffi::c_void,
                crate::KernelArg::I64(v) => v as *const i64 as *mut std::ffi::c_void,
                crate::KernelArg::F32(v) => v as *const f32 as *mut std::ffi::c_void,
                crate::KernelArg::F64(v) => v as *const f64 as *mut std::ffi::c_void,
                crate::KernelArg::DevicePtr { .. } => {
                    let slot = &ptrs[ip] as *const u64 as *mut std::ffi::c_void;
                    ip += 1;
                    slot
                }
            });
        }

        // SAFETY: `func` belongs to this module on the bound context;
        // every pointer in `params` points into `args`/`ptrs`, both of
        // which outlive the call (anchored by the keep-alive below).
        let launched = unsafe {
            cuda_core::launch_kernel_on_stream(
                func,
                cfg.grid,
                cfg.block,
                cfg.shared_bytes,
                stream,
                &mut params,
            )
        };
        // Anchor both stores past the launch so a future refactor that
        // drops either early fails to compile rather than launching a
        // kernel over freed argument storage.
        let _keep_alive = (&args, &ptrs);
        launched.map_err(map_err("cuLaunchKernel"))
    }
}

// -- Device buffer ----------------------------------------------------

/// A device allocation and the context that owns it.
///
/// Unlike the cudarc backend, which wraps `CudaSlice<T>`, this owns a
/// raw `CUdeviceptr`: the raw driver entry points are generic over
/// nothing, which keeps `DeviceElem` free of any `cuda-core` trait and
/// lets the public bound stay `Pod + Send + Sync + 'static`.
/// One device allocation, owned.
///
/// This is a separate `Arc`-able value rather than a plain field for one
/// reason: `device_ptr_arg` has to hand a launch something that KEEPS
/// THE MEMORY ALIVE. `KernelArgs` is `'static` and holds no borrow of
/// the originating `DeviceBuffer`, so a caller may drop the buffer
/// between building the args and launching. The cudarc backend clones
/// its `Arc<CudaSlice>` for this; without an equivalent the kernel reads
/// freed device memory -- the defect its "AUDIT 2026-05-22 (UAF fix)"
/// note records.
struct Alloc {
    ptr: sys::CUdeviceptr,
    ctx: DeviceContextInner,
}

// SAFETY: the allocation is owned by this value and only reached through
// driver calls that bind the owning context first. Same argument the
// cudarc backend makes for `CudaSlice`.
unsafe impl Send for Alloc {}
// SAFETY: as above; shared references hand out no interior mutability.
unsafe impl Sync for Alloc {}

impl Drop for Alloc {
    fn drop(&mut self) {
        if self.ptr == 0 {
            return;
        }
        let _ = self.ctx.bind_to_thread();
        // SAFETY: this is the last owner of the address (the `Arc` has
        // just hit zero), and the owning context was bound above.
        unsafe {
            let _ = sys::cuMemFree_v2(self.ptr);
        }
    }
}

pub(crate) struct DeviceBufferInner<T> {
    alloc: Arc<Alloc>,
    len: usize,
    _marker: std::marker::PhantomData<T>,
}

impl<T> DeviceBufferInner<T> {
    fn bytes(len: usize) -> Result<usize> {
        len.checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| DeviceError::Memcpy("device alloc size overflows".into()))
    }

    fn alloc(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        let bytes = Self::bytes(len)?;
        ctx.bind_to_thread()?;
        let mut ptr: sys::CUdeviceptr = 0;
        // SAFETY: `ptr` is a live out-param and the context is bound
        // above. A zero-length buffer still takes one byte so the
        // pointer is non-null and unique, matching the cudarc backend.
        unsafe {
            sys::cuMemAlloc_v2(&mut ptr, bytes.max(1))
                .result()
                .map_err(map_err("cuMemAlloc"))?;
        }
        // Counted as a pool MISS, because that is what it is: this
        // backend has no allocation pool, so every buffer is a fresh
        // driver allocation. Without this the exit census would print
        // `cuMemAlloc=0 pooled=0` no matter how much was allocated --
        // a zero from an instrument that cannot fire, which reads as
        // "no allocations" rather than "no pool".
        cratonvm_types::gpu_event_census::note_alloc_pool_miss();
        Ok(Self {
            alloc: Arc::new(Alloc {
                ptr,
                ctx: ctx.clone(),
            }),
            len,
            _marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        Self::alloc(ctx, len)
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        let buf = Self::alloc(ctx, len)?;
        let bytes = Self::bytes(len)?;
        // Issued on `copy_h2d`, NOT the NULL stream: the caller pairs
        // this with `record_alloc_event` on the same stream to give the
        // buffer a last-write marker. See the module docs.
        // SAFETY: `buf.alloc.ptr` owns `bytes` bytes and the context is bound
        // by `alloc`.
        unsafe {
            sys::cuMemsetD8Async(buf.alloc.ptr, 0, bytes, drv::stream_raw(ctx.copy_h2d()))
                .result()
                .map_err(map_err("cuMemsetD8Async"))?;
        }
        Ok(buf)
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        let buf = Self::alloc(ctx, host.len())?;
        buf.copy_from_host(host)?;
        Ok(buf)
    }

    /// Allocate and upload on `upload_stream` WITHOUT blocking, then
    /// record `last_write_event` on that stream.
    ///
    /// The event is what consumers (`launch_on_stream`, `to_host_async`)
    /// order behind, so recording it here is not optional bookkeeping --
    /// it is the only thing that makes the async upload safe to read.
    ///
    /// # Safety
    ///
    /// `host` must stay live and unmodified until the copy completes on
    /// `upload_stream`. The caller owns that; nothing here can enforce
    /// it, which is why this is `unsafe` and the blocking `from_host` is
    /// not.
    pub(crate) unsafe fn from_host_async_unchecked(
        ctx: &DeviceContextInner,
        host: &[T],
        upload_stream: sys::CUstream,
        last_write_event: sys::CUevent,
    ) -> Result<Self> {
        let buf = Self::alloc(ctx, host.len())?;
        if host.is_empty() {
            // Nothing to copy, but the event must still fire: a
            // consumer waits on it unconditionally, and an event that is
            // never recorded would hang or -- worse -- be treated as
            // already-complete.
            // SAFETY: the context is bound by `alloc` above.
            unsafe { drv::event_record(last_write_event, upload_stream)? };
            return Ok(buf);
        }
        let bytes = Self::bytes(host.len())?;
        // SAFETY: `buf.alloc.ptr` owns `bytes`; the context is bound by
        // `alloc`. The caller upholds `host`'s lifetime through
        // completion of `upload_stream`, per this function's contract.
        unsafe {
            sys::cuMemcpyHtoDAsync_v2(
                buf.alloc.ptr,
                host.as_ptr() as *const std::ffi::c_void,
                bytes,
                upload_stream,
            )
            .result()
            .map_err(map_err("cuMemcpyHtoDAsync"))?;
            drv::event_record(last_write_event, upload_stream)?;
        }
        Ok(buf)
    }

    pub(crate) fn copy_from_host(&self, host: &[T]) -> Result<()> {
        if host.len() != self.len {
            return Err(DeviceError::Memcpy(format!(
                "copy_from_host length mismatch: host.len()={}, buffer.len()={}",
                host.len(),
                self.len
            )));
        }
        if self.len == 0 {
            return Ok(());
        }
        self.alloc.ctx.bind_to_thread()?;
        let bytes = Self::bytes(self.len)?;
        let stream = drv::stream_raw(self.alloc.ctx.copy_h2d());
        // SAFETY: `host` is valid for `bytes` and `self.alloc.ptr` owns at
        // least that much; the context is bound above. The copy is
        // issued asynchronously and then waited on below, so `host` is
        // still borrowed for the whole transfer.
        unsafe {
            sys::cuMemcpyHtoDAsync_v2(
                self.alloc.ptr,
                host.as_ptr() as *const std::ffi::c_void,
                bytes,
                stream,
            )
            .result()
            .map_err(map_err("cuMemcpyHtoDAsync"))?;
            // Synchronous contract: the bytes must be on the device when
            // this returns, so the caller may reuse `host` immediately.
            // Blocking on this stream alone leaves compute running.
            drv::stream_synchronize(stream)?;
        }
        Ok(())
    }

    pub(crate) fn to_host(&self, dst: &mut [T], wait_event: Option<sys::CUevent>) -> Result<()> {
        if dst.len() != self.len {
            return Err(DeviceError::Memcpy(format!(
                "to_host length mismatch: dst.len()={}, buffer.len()={}",
                dst.len(),
                self.len
            )));
        }
        self.alloc.ctx.bind_to_thread()?;
        let stream = drv::stream_raw(self.alloc.ctx.copy_d2h());
        // The wait, the copy and the host-block all name `copy_d2h`.
        // They have to: a wait enqueued on one stream orders nothing on
        // another, and a SYNCHRONOUS `cuMemcpyDtoH_v2` runs against the
        // NULL stream, which does not synchronise with the
        // `CU_STREAM_NON_BLOCKING` streams kernels launch on.
        if let Some(ev) = wait_event {
            // SAFETY: the event is owned by the caller's buffer and the
            // context is bound above.
            unsafe { drv::stream_wait_event(stream, ev)? };
        }
        if self.len == 0 {
            return Ok(());
        }
        let bytes = Self::bytes(self.len)?;
        // SAFETY: `dst` is writable for `bytes`; `self.alloc.ptr` owns at
        // least that much. The copy is async, so the stream is drained
        // below before `dst` is handed back.
        unsafe {
            sys::cuMemcpyDtoHAsync_v2(
                dst.as_mut_ptr() as *mut std::ffi::c_void,
                self.alloc.ptr,
                bytes,
                stream,
            )
            .result()
            .map_err(map_err("cuMemcpyDtoHAsync"))?;
            drv::stream_synchronize(stream)?;
        }
        Ok(())
    }

    pub(crate) fn to_host_async_raw(
        &self,
        dst: &mut [T],
        user_stream: sys::CUstream,
        wait_event: Option<sys::CUevent>,
    ) -> Result<()> {
        if dst.len() != self.len {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async length mismatch: dst.len()={}, buffer.len()={}",
                dst.len(),
                self.len
            )));
        }
        self.to_host_async_range_raw(dst, 0, user_stream, wait_event)
    }

    pub(crate) fn to_host_async_range_raw(
        &self,
        dst: &mut [T],
        offset: usize,
        user_stream: sys::CUstream,
        wait_event: Option<sys::CUevent>,
    ) -> Result<()> {
        let end = offset.checked_add(dst.len()).ok_or_else(|| {
            DeviceError::Memcpy("to_host_async_range: offset + len overflows".into())
        })?;
        if end > self.len {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async_range out of bounds: offset={offset} len={} buffer={}",
                dst.len(),
                self.len
            )));
        }
        self.alloc.ctx.bind_to_thread()?;
        if let Some(ev) = wait_event {
            // SAFETY: see `to_host`.
            unsafe {
                sys::cuStreamWaitEvent(user_stream, ev, 0)
                    .result()
                    .map_err(map_err("cuStreamWaitEvent"))?;
            }
        }
        if dst.is_empty() {
            return Ok(());
        }
        let bytes = Self::bytes(dst.len())?;
        let src = self.alloc.ptr + Self::bytes(offset)? as sys::CUdeviceptr;
        // SAFETY: the range is bounds-checked above, so `src` covers
        // `bytes`. ASYNC: `dst` must stay live until the caller
        // synchronises `user_stream`, which is this function's contract
        // (mirrors the cudarc backend's `to_host_async_range_raw`).
        unsafe {
            sys::cuMemcpyDtoHAsync_v2(
                dst.as_mut_ptr() as *mut std::ffi::c_void,
                src,
                bytes,
                user_stream,
            )
            .result()
            .map_err(map_err("cuMemcpyDtoHAsync"))?;
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn bind_to_thread(&self) -> Result<()> {
        self.alloc.ctx.bind_to_thread()
    }

    /// No pooled allocator on this backend yet, so retiring to a pool is
    /// a no-op rather than a lie: the allocation is freed at drop.
    pub(crate) fn set_retire_to_pool(&self) {}
}

/// Type-erased keep-alive for a device allocation referenced by a
/// pending launch. Same shape as the cudarc backend's.
pub(crate) type BufferKeepAlive = Arc<dyn std::any::Any + Send + Sync + 'static>;

impl<T: Send + Sync + 'static> DeviceBufferInner<T> {
    /// The raw device address plus a type-erased keep-alive handle.
    ///
    /// The keep-alive is a clone of the allocation's `Arc`, so the
    /// device memory outlives every `KernelArgs` referring to it. It is
    /// NOT decorative: `KernelArgs` is `'static` and borrows nothing, so
    /// a caller may drop the originating `DeviceBuffer` between building
    /// the args and launching. Handing back a bare address here is
    /// exactly the use-after-free the cudarc backend records under
    /// "AUDIT 2026-05-22".
    pub(crate) fn device_ptr_arg(&self) -> (u64, BufferKeepAlive) {
        (
            self.alloc.ptr as u64,
            Arc::clone(&self.alloc) as BufferKeepAlive,
        )
    }
}

// -- Pinned host staging ----------------------------------------------

pub(crate) struct PinnedHostInner<T: Copy> {
    ptr: *mut T,
    len: usize,
    ctx: DeviceContextInner,
    _marker: std::marker::PhantomData<T>,
}

// SAFETY: a plain page-locked host region owned by this value; the raw
// pointer is only dereferenced through `as_mut_slice`, whose safety
// contract puts DMA ordering on the caller.
unsafe impl<T: Copy + Send> Send for PinnedHostInner<T> {}
// SAFETY: as above; shared references hand out no interior mutability.
unsafe impl<T: Copy + Sync> Sync for PinnedHostInner<T> {}

impl<T: Copy> PinnedHostInner<T> {
    pub(crate) fn new(ctx: &crate::DeviceContext, len: usize) -> Result<Self> {
        let inner = ctx.inner().clone();
        inner.bind_to_thread()?;
        let bytes = len
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| DeviceError::Memcpy("pinned host alloc size overflows".into()))?;
        let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `raw` is a live out-param and the context is bound
        // above. `max(1)` keeps a zero-length buffer's pointer unique.
        unsafe {
            sys::cuMemAllocHost_v2(&mut raw, bytes.max(1))
                .result()
                .map_err(map_err("cuMemAllocHost"))?;
        }
        Ok(Self {
            ptr: raw as *mut T,
            len,
            ctx: inner,
            _marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// # Safety
    /// See [`crate::PinnedHostBuffer::as_mut_slice`].
    pub(crate) unsafe fn as_mut_slice(&self) -> &mut [T] {
        std::slice::from_raw_parts_mut(self.ptr, self.len)
    }
}

impl<T: Copy> Drop for PinnedHostInner<T> {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        let _ = self.ctx.bind_to_thread();
        // SAFETY: the buffer is being dropped, so nothing else refers to
        // the region; the owning context was bound above.
        unsafe {
            let _ = sys::cuMemFreeHost(self.ptr as *mut std::ffi::c_void);
        }
    }
}

// -- Driver operations ------------------------------------------------

/// The `extern "C"` function the driver calls back, and the sole
/// consumer of the double-`Box` `launch_host_func` hands over.
///
/// A twin of `backend_cuda`'s, not a shared copy: the ABI it matches is
/// `sys::CUhostFn` from *this* backend's bindings, and keeping vendor
/// types out of shared code is the entire reason
/// `DriverApi::launch_host_func` takes a `Box<dyn FnOnce>`.
unsafe extern "C" fn host_callback_trampoline(user_data: *mut std::ffi::c_void) {
    // SAFETY: `user_data` was produced by `launch_host_func` below via
    // `Box::into_raw` on a `Box<Box<dyn FnOnce() + Send>>`. The driver
    // invokes a given submission's trampoline exactly once and never
    // again afterwards, so reclaiming ownership here with
    // `Box::from_raw` cannot double-free or run twice.
    let f = unsafe { Box::from_raw(user_data as *mut Box<dyn FnOnce() + Send>) };
    // CALLBACK RULE: `f` must not call any Driver or Runtime API. This
    // runs on an internal driver callback thread and re-entering the
    // driver from it is undefined behaviour. The `catch_unwind` guards
    // the `extern "C"` boundary: unwinding into driver-owned code is
    // also undefined, and the `void`-returning ABI has no channel to
    // report a panic through, so it is swallowed here.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
}

impl crate::backend_api::DriverApi for OxideBackend {
    type DeviceHandle = std::sync::Arc<cuda_core::CudaContext>;
    type StreamHandle = std::sync::Arc<cuda_core::CudaStream>;
    type RawStream = sys::CUstream;
    type RawEvent = sys::CUevent;

    fn device_bind(device: &Self::DeviceHandle) -> Result<()> {
        device
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e}")))
    }

    fn fork_default_stream(device: &Self::DeviceHandle) -> Result<Self::StreamHandle> {
        device
            .default_stream()
            .fork()
            .map_err(|e| DeviceError::Driver(format!("fork_default_stream: {e}")))
    }

    fn stream_raw(stream: &Self::StreamHandle) -> Self::RawStream {
        stream.cu_stream()
    }

    fn stream_token(stream: Self::RawStream) -> usize {
        stream as usize
    }

    fn event_create() -> Result<Self::RawEvent> {
        let mut ev: sys::CUevent = std::ptr::null_mut();
        // SAFETY: `ev` is a live out-param for the duration of the call.
        unsafe {
            sys::cuEventCreate(
                &mut ev,
                sys::CUevent_flags_enum_CU_EVENT_DISABLE_TIMING as u32,
            )
            .result()
            .map_err(|e| DeviceError::Driver(format!("cuEventCreate: {e}")))?;
        }
        Ok(ev)
    }

    unsafe fn event_record(event: Self::RawEvent, stream: Self::RawStream) -> Result<()> {
        // SAFETY: the trait's precondition -- both handles live and
        // sharing the bound context -- is exactly what `cuEventRecord`
        // requires, and the caller has discharged it.
        unsafe { sys::cuEventRecord(event, stream) }
            .result()
            .map_err(|e| DeviceError::Driver(format!("cuEventRecord: {e}")))
    }

    unsafe fn event_query(event: Self::RawEvent) -> Result<bool> {
        // SAFETY: `event` is live and its context bound, per the
        // trait's precondition.
        let raw = unsafe { sys::cuEventQuery(event) };
        if raw == sys::cudaError_enum_CUDA_SUCCESS {
            return Ok(true);
        }
        if raw == sys::cudaError_enum_CUDA_ERROR_NOT_READY {
            return Ok(false);
        }
        Err(DeviceError::Driver(format!("cuEventQuery: {raw:?}")))
    }

    unsafe fn event_synchronize(event: Self::RawEvent) -> Result<()> {
        // SAFETY: `event` is live and its context bound, per the
        // trait's precondition.
        unsafe { sys::cuEventSynchronize(event) }
            .result()
            .map_err(|e| DeviceError::Driver(format!("cuEventSynchronize: {e}")))
    }

    unsafe fn event_destroy(event: Self::RawEvent) {
        // SAFETY: the trait's precondition makes this the unique owner,
        // so destroying cannot race a concurrent use.
        unsafe {
            let _ = sys::cuEventDestroy_v2(event);
        }
    }

    unsafe fn stream_wait_event(stream: Self::RawStream, event: Self::RawEvent) -> Result<()> {
        // SAFETY: both handles live and sharing the bound context, per
        // the trait's precondition.
        unsafe {
            sys::cuStreamWaitEvent(
                stream,
                event,
                sys::CUevent_wait_flags_enum_CU_EVENT_WAIT_DEFAULT as u32,
            )
        }
        .result()
        .map_err(|e| DeviceError::Driver(format!("cuStreamWaitEvent: {e}")))
    }

    unsafe fn stream_synchronize(stream: Self::RawStream) -> Result<()> {
        // SAFETY: `stream` is live and its context bound, per the
        // trait's precondition.
        unsafe { sys::cuStreamSynchronize(stream) }
            .result()
            .map_err(|e| DeviceError::Driver(format!("cuStreamSynchronize: {e}")))
    }

    unsafe fn launch_host_func(stream: Self::RawStream, f: Box<dyn FnOnce() + Send>) -> Result<()> {
        // Double-box: a `Box<dyn FnOnce>` is a fat pointer and the
        // driver's `void*` is thin. Ownership transfers to the
        // trampoline on success, or is reclaimed here on a synchronous
        // submission failure.
        let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(f);
        let user_data = Box::into_raw(boxed) as *mut std::ffi::c_void;
        // SAFETY: `stream` is live in the bound context per the trait's
        // precondition; `user_data` is the `Box::into_raw` allocation
        // just made, and ownership of it transfers to the trampoline
        // exactly when this call reports success.
        let outcome =
            unsafe { sys::cuLaunchHostFunc(stream, Some(host_callback_trampoline), user_data) }
                .result()
                .map_err(|e| DeviceError::Driver(format!("cuLaunchHostFunc: {e}")));
        if outcome.is_err() {
            // The driver will never call the trampoline for a submission
            // that failed synchronously, so it will never reclaim
            // `user_data`. Reclaim it here or it leaks forever.
            // SAFETY: ownership never transferred; this is the original
            // `Box::into_raw` pointer.
            unsafe {
                drop(Box::from_raw(user_data as *mut Box<dyn FnOnce() + Send>));
            }
        }
        outcome
    }
}

// -- Backend contract -------------------------------------------------

/// Marker for the cuda-oxide (`cuda-core`) backend.
pub(crate) struct OxideBackend;

/// The compiled-in backend, under a name `crate::drv` can reach
/// without a `cfg` cascade. Every backend defines this alias; it is
/// how one generic forwarding module serves all of them.
pub(crate) type Backend = OxideBackend;

impl crate::backend_api::BackendApi for OxideBackend {
    type Context = DeviceContextInner;
    type Module = DeviceModuleInner;
    type Stream = Arc<CudaStream>;

    fn probe_device(device_ordinal: u32) -> Result<DeviceCaps> {
        probe_device(device_ordinal)
    }

    fn backend_kind() -> crate::BackendKind {
        crate::BackendKind::CudaOxide
    }

    fn toolchain_version() -> Result<crate::ToolchainVersion> {
        driver_cuda_version().map(crate::ToolchainVersion::CudaDriver)
    }
}

impl crate::backend_api::DeviceContextApi for DeviceContextInner {
    fn new(device_ordinal: u32) -> Result<Self> {
        Self::new(device_ordinal)
    }

    fn synchronize(&self) -> Result<()> {
        self.synchronize()
    }

    fn bind_to_thread(&self) -> Result<()> {
        self.bind_to_thread()
    }

    fn record_alloc_event(&self, event: &crate::Event) -> Result<()> {
        self.record_alloc_event(event)
    }
}

impl crate::backend_api::DeviceModuleApi for DeviceModuleInner {
    type Ctx = DeviceContextInner;
    type Stream = Arc<CudaStream>;

    fn from_kernel_image(
        ctx: &Self::Ctx,
        image: &crate::KernelImage<'_>,
        module_name: &str,
        kernel_names: &[&'static str],
    ) -> Result<Self> {
        match image {
            crate::KernelImage::Ptx(ptx) => Self::from_ptx(ctx, ptx, module_name, kernel_names),
            // Not `NoDriver`: there IS a driver, and it is the wrong
            // one for this kernel. Reporting "no device" here would
            // send the VM down its run-on-the-CPU path and hide a
            // target-selection bug behind correct answers.
            other => Err(crate::DeviceError::UnsupportedImage(match other {
                crate::KernelImage::HipSource { .. } => {
                    "HIP source handed to a CUDA backend; this driver JITs PTX"
                }
                crate::KernelImage::HipCodeObject(_) => {
                    "HIP code object handed to a CUDA backend; this driver JITs PTX"
                }
                crate::KernelImage::Ptx(_) => unreachable!("matched above"),
            })),
        }
    }

    fn optimal_block_size(&self, ctx: &Self::Ctx, kernel: &str) -> Option<u32> {
        self.optimal_block_size(ctx, kernel)
    }

    fn launch_raw_on_stream(
        &self,
        ctx: &Self::Ctx,
        stream: &Self::Stream,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.launch_raw_on_stream(ctx, stream, kernel, cfg, args)
    }
}
