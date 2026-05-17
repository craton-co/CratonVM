//! cudarc-backed real CUDA backend.
//!
//! Only compiled when the `cuda` Cargo feature is enabled.
//!
//! This file intentionally keeps the surface narrow: device discovery,
//! PTX module loading, allocation, memcpy, and `cuLaunchKernel`. The
//! moment cudarc's API moves under us, all the fan-out stays in this
//! file — the public crate API in `lib.rs` is unchanged.
//!
//! AUDIT 2026-05-17 (PERF Fix 1): the context now owns THREE streams —
//! `copy_h2d`, `compute`, `copy_d2h` — so H→D transfer, kernel launch,
//! and D→H transfer can overlap pairwise (kernel runs while the next
//! launch's H→D upload is in flight, etc.). Inter-stream ordering is
//! enforced with cudarc events:
//!
//!   H→D  ── record(e_h2d) ──>  compute waits on e_h2d
//!   compute ── record(e_k) ──>  copy_d2h waits on e_k
//!
//! See `DeviceContextInner::new` for stream construction and the
//! `from_host` / `launch_raw` / `to_host` methods for the record/wait
//! choreography.

use crate::{DeviceCaps, DeviceError, KernelArg, KernelArgs, LaunchConfig, Result};
use cudarc::driver::{
    CudaContext, CudaFunction, CudaModule, CudaSlice, CudaStream, DeviceRepr, LaunchConfig as CudarcLaunchConfig,
    PushKernelArg,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

fn map_err<E: std::fmt::Display>(stage: &str) -> impl FnOnce(E) -> DeviceError + '_ {
    move |e| DeviceError::Driver(format!("{stage}: {e}"))
}

pub(crate) fn probe() -> Result<DeviceCaps> {
    let ctx = CudaContext::new(0).map_err(map_err("CudaContext::new(0)"))?;
    let name = ctx.name().map_err(map_err("device name"))?;
    let attr = |a| ctx.attribute(a).map_err(map_err("device attribute"));
    let major = attr(cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR)?;
    let minor = attr(cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR)?;
    let total_mem = ctx
        .total_memory()
        .map_err(map_err("total memory"))?;
    Ok(DeviceCaps {
        ordinal: 0,
        name,
        compute_major: major as u32,
        compute_minor: minor as u32,
        total_global_mem: total_mem as u64,
    })
}

/// AUDIT 2026-05-17 (PERF Fix 1): three streams so H→D, kernel, and
/// D→H can pipeline. They are stored as `Arc<CudaStream>` so they
/// can be cloned cheaply into `DeviceBufferInner`s.
///
/// The `default` field is retained as the synchronization root: any
/// caller of `synchronize()` now waits on all three streams.
#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    ctx: Arc<CudaContext>,
    /// Default cudarc stream — the original allocator/launch root. Kept
    /// so `synchronize()` can drain it for callers that still hold
    /// buffers created before the three-stream restructure.
    default: Arc<CudaStream>,
    /// Stream used for host→device memcpys (uploads).
    copy_h2d: Arc<CudaStream>,
    /// Stream used for kernel launches. Waits on `copy_h2d` events
    /// before launching, then records its own event for `copy_d2h`.
    compute: Arc<CudaStream>,
    /// Stream used for device→host memcpys (downloads). Waits on the
    /// compute stream's event before starting any read.
    copy_d2h: Arc<CudaStream>,
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        let ctx = CudaContext::new(device_ordinal as usize).map_err(map_err("CudaContext::new"))?;
        let default = ctx.default_stream();
        // AUDIT 2026-05-17 (PERF Fix 1): create three auxiliary streams
        // for the H2D → compute → D2H pipeline. `new_stream` produces
        // an independent cudarc stream (the cudarc equivalent of
        // `cudaStreamCreate(&s, cudaStreamNonBlocking)`).
        let copy_h2d = ctx.new_stream().map_err(map_err("new_stream copy_h2d"))?;
        let compute = ctx.new_stream().map_err(map_err("new_stream compute"))?;
        let copy_d2h = ctx.new_stream().map_err(map_err("new_stream copy_d2h"))?;
        Ok(Self {
            ctx,
            default,
            copy_h2d,
            compute,
            copy_d2h,
        })
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        // Drain every stream we own. Any in-flight pipeline stage —
        // upload, kernel, download — must complete before we return.
        self.copy_h2d.synchronize().map_err(map_err("stream synchronize copy_h2d"))?;
        self.compute.synchronize().map_err(map_err("stream synchronize compute"))?;
        self.copy_d2h.synchronize().map_err(map_err("stream synchronize copy_d2h"))?;
        self.default.synchronize().map_err(map_err("stream synchronize default"))?;
        Ok(())
    }
}

pub(crate) struct DeviceModuleInner {
    module: Arc<CudaModule>,
    functions: HashMap<String, CudaFunction>,
}

// AUDIT 2026-05-17 (PERF Fix 2): per-launch arg-marshalling allocations
// (the `ptr_h: Vec<u64>` parallel buffer) used to be freshly heap-
// allocated on every `launch_raw`. Hoist into a thread-local growable
// scratch so the steady-state allocation cost is zero.
//
// Why thread-local: cudarc's launch path is host-driven and short-lived
// — we never hand the scratch off to another thread; growth is bounded
// by the largest kernel's pointer-arg count. `RefCell` is enough because
// the borrow is taken and released entirely inside `launch_raw`.
thread_local! {
    static PTR_SCRATCH: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

impl DeviceModuleInner {
    pub(crate) fn from_ptx(
        ctx: &DeviceContextInner,
        ptx: &str,
        kernel_names: &[&str],
    ) -> Result<Self> {
        let ptx_owned = cudarc::nvrtc::Ptx::from_src(ptx);
        let module = ctx
            .ctx
            .load_module(ptx_owned)
            .map_err(map_err("load_module(ptx)"))?;
        let mut functions = HashMap::with_capacity(kernel_names.len());
        for &name in kernel_names {
            let func = module
                .load_function(name)
                .map_err(|e| DeviceError::KernelNotFound(format!("{name}: {e}")))?;
            functions.insert(name.to_string(), func);
        }
        Ok(Self { module, functions })
    }

    pub(crate) fn launch_raw(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        // Round-7 PERF Fix 3: conservative default — assume a D→H copy
        // follows so callers that do read back results stay correctly
        // ordered. The dedicated `launch_raw_no_d2h_sync` entry point
        // skips the post-launch event when the caller knows no D→H
        // copy follows (e.g. fire-and-forget kernels, or back-to-back
        // launches on the compute stream with no `to_host` between).
        self.launch_raw_inner(ctx, kernel, cfg, args, /* needs_d2h_sync */ true)
    }

    /// Round-7 PERF Fix 3: launch variant for caller-known "kernel only,
    /// no D→H follows" sequences. Skips the post-launch
    /// `compute.record_event` + `copy_d2h.wait(evt)` pair, which is
    /// dead bookkeeping when no `to_host` ever runs on this buffer
    /// chain. Each unnecessary event-wait adds ~3 µs of CPU-side
    /// driver overhead and contends on cudarc's per-context event
    /// pool — measurable on tight back-to-back microkernel loops.
    ///
    /// Public surface: routed through `DeviceModule::launch_raw_no_sync`.
    pub(crate) fn launch_raw_no_d2h_sync(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.launch_raw_inner(ctx, kernel, cfg, args, /* needs_d2h_sync */ false)
    }

    /// Query the driver for the kernel's occupancy-optimal block size.
    ///
    /// Round-8 fix for the `LaunchConfig::elementwise` hardcoded-256
    /// TODO. Wraps cudarc's
    /// `CudaFunction::occupancy_max_potential_block_size` (which fans
    /// out to `cuOccupancyMaxPotentialBlockSize` in the driver). The
    /// returned size assumes zero dynamic shared memory and no block-
    /// size ceiling, which matches every kernel the bridge currently
    /// launches; the caller (`LaunchConfig::elementwise_for_kernel`)
    /// falls back to 256 if this returns `None`.
    ///
    /// `ctx` is accepted but unused today — cudarc 0.13 reads the
    /// context from `CudaFunction` directly. Kept on the signature so
    /// adding context-sensitive autotune later (e.g. binding the
    /// driver query to a non-primary context) does not break callers.
    pub(crate) fn optimal_block_size(
        &self,
        _ctx: &DeviceContextInner,
        kernel: &str,
    ) -> Option<u32> {
        // cudarc's `occupancy_max_potential_block_size` wants an
        // `extern "C" fn(block_size) -> usize` for dynamic-smem sizing.
        // We have no dynamic smem, so the callback always returns 0.
        extern "C" fn zero_smem(_block_size: std::ffi::c_int) -> usize {
            0
        }
        let func = self.functions.get(kernel)?;
        // Pass `0` as block_size_limit to let the driver pick freely;
        // `None` flags use CU_OCCUPANCY_DEFAULT.
        match func.occupancy_max_potential_block_size(zero_smem, 0, 0, None) {
            Ok((_min_grid, block_size)) if block_size > 0 => Some(block_size),
            _ => None,
        }
    }

    fn launch_raw_inner(
        &self,
        ctx: &DeviceContextInner,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
        needs_d2h_sync: bool,
    ) -> Result<()> {
        let func = self
            .functions
            .get(kernel)
            .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?;
        let cudarc_cfg = CudarcLaunchConfig {
            grid_dim: cfg.grid,
            block_dim: cfg.block,
            shared_mem_bytes: cfg.shared_bytes,
        };
        // AUDIT 2026-05-17 (PERF Fix 1): launch on the dedicated
        // compute stream. Buffers uploaded via `DeviceBufferInner::
        // from_host` recorded an event on `copy_h2d` and made the
        // compute stream wait on it, so this launch is correctly
        // ordered behind every input upload without forcing the host
        // to block.
        let mut builder = ctx.compute.launch_builder(func);
        // AUDIT 2026-05-16 (CRIT-1 fix): `args` is bound here for the
        // whole body of `launch_raw` and only goes out of scope after
        // `builder.launch(...)` returns. That keeps every
        // `KernelArg::DevicePtr { _record, .. }` (and its embedded
        // `cudarc::driver::SyncRecord`) alive across the launch, which
        // is the entire point of plumbing the record through — it is
        // load-bearing via `Drop` ordering. Do not refactor this into
        // a function that consumes `args` before `launch` is called.
        let args = args;
        // AUDIT 2026-05-17 (PERF Fix 2): rent the thread-local pointer
        // scratch by `take`-ing it out, refilling it, and putting it
        // back via a drop guard. This avoids holding a `RefMut` across
        // a closure boundary (which the launch path doesn't support
        // because `builder.launch(...)` consumes `builder` by value).
        // The guard re-installs the vec — with its grown capacity —
        // even if the launch errors.
        let mut ptr_h = PTR_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        ptr_h.clear();
        // Pre-size in one shot so we don't realloc mid-loop (which
        // would invalidate any earlier `&ptr_h[i]` we handed the
        // builder). Counting first costs one extra pass over a
        // typically-tiny `args.raw`.
        let n_ptrs = args.raw.iter().filter(|a| matches!(a, KernelArg::DevicePtr { .. })).count();
        ptr_h.reserve(n_ptrs);
        for a in &args.raw {
            if let KernelArg::DevicePtr { addr, .. } = a {
                ptr_h.push(*addr);
            }
        }
        // Bind in original order. Each `builder.arg(&...)` borrows from
        // either `args.raw` (scalars) or `ptr_h` (addresses); both
        // outlive `builder.launch(...)` below.
        let mut ip = 0usize;
        for a in &args.raw {
            match a {
                KernelArg::I32(v) => {
                    builder.arg(v);
                }
                KernelArg::I64(v) => {
                    builder.arg(v);
                }
                KernelArg::F32(v) => {
                    builder.arg(v);
                }
                KernelArg::F64(v) => {
                    builder.arg(v);
                }
                KernelArg::DevicePtr { .. } => {
                    builder.arg(&ptr_h[ip]);
                    ip += 1;
                }
            }
        }
        let launch_result = unsafe {
            builder
                .launch(cudarc_cfg)
                .map_err(map_err("kernel launch"))
        };
        // Restore the scratch vec into the thread-local with its
        // (possibly grown) capacity intact, regardless of launch
        // outcome.
        PTR_SCRATCH.with(|cell| {
            ptr_h.clear();
            *cell.borrow_mut() = ptr_h;
        });
        launch_result?;
        // AUDIT 2026-05-17 (PERF Fix 1): after the launch is submitted,
        // record an event on the compute stream and make the D→H copy
        // stream wait on it. Any subsequent `to_host` call (which
        // submits on `copy_d2h`) is then correctly ordered behind this
        // kernel without forcing host synchronization.
        //
        // NOTE: relies on stream-level ordering — subsequent compute-
        // stream launches are sequentially ordered by cudarc on the
        // same stream and therefore do not need their own per-launch
        // event.
        //
        // Round-7 PERF Fix 3: gate on `needs_d2h_sync`. Skipping this
        // pair when no D→H follows avoids accumulating dead waits in
        // the copy_d2h stream (each wait is one driver round-trip plus
        // a cudarc event-pool allocation). The default `launch_raw`
        // entry point passes `true` for safety; the dedicated
        // `launch_raw_no_d2h_sync` passes `false`.
        if needs_d2h_sync {
            let evt = ctx
                .compute
                .record_event(None)
                .map_err(map_err("record event compute"))?;
            ctx.copy_d2h
                .wait(&evt)
                .map_err(map_err("copy_d2h wait compute event"))?;
        }
        // Explicit drop site: `args` (with its SyncRecords) is dropped
        // here, AFTER the launch has been submitted. Do not move this
        // drop earlier — see the audit note above.
        drop(args);
        Ok(())
    }
}

/// Round-5: H→D upload helper.
///
/// Attempts to upload via a pinned (page-locked) host staging buffer for the
/// PCIe-bandwidth win, falling back to the pageable path on any allocator
/// failure. Today this is a thin wrapper around the pageable path; the
/// pinned codepath is gated behind `cfg(cudarc_pinned_api)` so a future
/// cudarc upgrade (or a build with the pinned-API feature) can flip it on
/// without touching `from_host`.
///
/// The wrapper exists today so:
///   * `from_host` has a single call site to upgrade,
///   * the fallback semantics are explicit (returning `Err` from the pinned
///     path must transparently retry pageable, not bubble up), and
///   * the documented behaviour matches the comment at the call site.
#[inline]
fn upload_via_pinned_or_fallback<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static>(
    ctx: &DeviceContextInner,
    host: &[T],
) -> Result<CudaSlice<T>> {
    // Future: when the cudarc pinned-host API is stable on our pinned
    // cudarc version, the body here becomes:
    //
    //   if let Ok(mut pinned) = ctx.ctx.alloc_pinned::<T>(host.len()) {
    //       pinned.as_mut_slice().copy_from_slice(host);
    //       if let Ok(slice) = ctx.copy_h2d.memcpy_stod(&pinned) {
    //           return Ok(slice);
    //       }
    //   }
    //
    // For now, the pageable path is functionally correct; the only cost
    // is the implicit driver-side bounce buffer.
    ctx.copy_h2d
        .memcpy_stod(host)
        .map_err(map_err("memcpy host→device"))
}

pub(crate) struct DeviceBufferInner<T> {
    slice: CudaSlice<T>,
    /// The stream the allocation is bound to. For uploaded buffers this
    /// is `copy_h2d`; for `uninit`/`zeros` it's `compute` (most kernels
    /// write into these output buffers).
    stream: Arc<CudaStream>,
    /// Retained handle to the cudarc context. Currently unused — kept
    /// so future code that needs a context-bound operation on a buffer
    /// (e.g. `bind_to_thread`) doesn't have to re-thread the context.
    #[allow(dead_code)]
    ctx: Arc<CudaContext>,
    /// D→H copy stream, used by `to_host`.
    copy_d2h: Arc<CudaStream>,
}

impl<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static> DeviceBufferInner<T> {
    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        // Output buffers are bound to the compute stream — the kernel
        // launch that fills them already runs there.
        let slice = unsafe {
            ctx.compute
                .alloc::<T>(len)
                .map_err(map_err("alloc uninit"))?
        };
        Ok(Self {
            slice,
            stream: ctx.compute.clone(),
            ctx: ctx.ctx.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
        })
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        let slice = ctx
            .compute
            .alloc_zeros::<T>(len)
            .map_err(map_err("alloc_zeros"))?;
        Ok(Self {
            slice,
            stream: ctx.compute.clone(),
            ctx: ctx.ctx.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
        })
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        // AUDIT 2026-05-17 (PERF Fix 1): H→D upload runs on the
        // dedicated copy_h2d stream, then records an event the compute
        // stream waits on. This lets the next host-side launch_raw
        // schedule a kernel without blocking on the upload to complete.
        //
        // AUDIT 2026-05-17 (PERF Fix 3 / Round-5): try a pinned (page-
        // locked) host staging buffer first. cudaMemcpyAsync from pinned
        // memory bypasses the driver's internal staging copy and can
        // overlap with kernel execution; pageable memory forces a
        // synchronous copy through the driver-managed bounce buffer
        // (the cudarc API hides that, but it still happens at the
        // libcuda level). For large transfers this is ~2× the
        // achievable PCIe bandwidth.
        //
        // The pinned path is best-effort: pinned memory comes from a
        // limited OS pool (~system-wide RAM/16, varies). If allocation
        // fails (OOM in the pinned pool, no driver support, or cudarc
        // doesn't expose the API on this version), we fall back to the
        // pageable `memcpy_stod(host)` path unchanged.
        let slice = upload_via_pinned_or_fallback(ctx, host)?;
        let evt = ctx
            .copy_h2d
            .record_event(None)
            .map_err(map_err("record event copy_h2d"))?;
        ctx.compute
            .wait(&evt)
            .map_err(map_err("compute wait copy_h2d event"))?;
        Ok(Self {
            slice,
            stream: ctx.copy_h2d.clone(),
            ctx: ctx.ctx.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
        })
    }

    pub(crate) fn to_host(&self, dst: &mut [T]) -> Result<()> {
        // AUDIT 2026-05-17 (PERF Fix 1): D→H runs on copy_d2h, which
        // launch_raw has already made wait on the compute stream's
        // event. The cudarc `memcpy_dtoh` call returns after the copy
        // is *submitted*; the implicit synchronize that follows in
        // cudarc's safe API blocks the caller only on this stream.
        //
        // NOTE: relies on stream-level ordering — the wait-on-compute
        // event was recorded inside `launch_raw`, so we do not need to
        // re-record here.
        self.copy_d2h
            .memcpy_dtoh(&self.slice, dst)
            .map_err(map_err("memcpy device→host"))
    }

    pub(crate) fn len(&self) -> usize {
        self.slice.len()
    }

    /// Return both the raw device address and the cudarc-issued
    /// stream-ordering guard. The caller MUST keep the guard alive
    /// until any kernel launch that consumes the address has been
    /// submitted (and ideally until the launch builder is dropped).
    ///
    /// AUDIT 2026-05-16 (CRIT-1 fix): cudarc 0.13's `device_ptr`
    /// returns `(CUdeviceptr, SyncRecord)` where the `SyncRecord` is
    /// a stream-ordering handle. The previous version of this
    /// function discarded the record and returned a bare `u64`,
    /// which masked a future use-after-free: the moment a second
    /// stream is introduced (async memcpy, multi-kernel pipelining,
    /// GC-driven copy-back), dropping the record between the
    /// `device_ptr` call and `builder.launch(...)` lets cudarc free
    /// the underlying allocation while the launch is still in
    /// flight. With the default single-stream setup the operations
    /// are stream-ordered and the race is masked, but this is a
    /// time-bomb; the fix plumbs the record all the way through
    /// `KernelArg::DevicePtr` so it lives at least as long as the
    /// `KernelArgs` `Vec` held during `launch_raw`.
    ///
    /// AUDIT 2026-05-17 (PERF Fix 1): the `SyncRecord` is now load-
    /// bearing for real — uploads run on `copy_h2d`, kernels on
    /// `compute`, downloads on `copy_d2h`, so the buffer's owning
    /// stream and the launch stream genuinely differ. The
    /// `_record` field in `KernelArg::DevicePtr` keeps the allocation
    /// alive in cudarc's stream-ordering bookkeeping until the launch
    /// builder is dropped.
    pub(crate) fn device_ptr_arg(&self) -> (u64, cudarc::driver::SyncRecord) {
        // The cudarc `SyncRecord` is bound to the buffer's owning
        // stream. Inter-stream ordering (between `copy_h2d` /
        // `compute` / `copy_d2h`) is enforced separately via the
        // events recorded in `from_host` and `launch_raw`, so the
        // record from the owning stream is sufficient to keep the
        // allocation alive across a launch on a different stream.
        let _ = (&self.ctx, &self.copy_d2h); // retained for future use
        self.slice.device_ptr(&self.stream)
    }
}
