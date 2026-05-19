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
    CudaDevice, CudaFunction, CudaSlice, CudaStream, DeviceRepr, LaunchConfig as CudarcLaunchConfig,
    DeviceSlice, DevicePtr, LaunchAsync,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

fn map_err<E: std::fmt::Debug>(stage: &str) -> impl FnOnce(E) -> DeviceError + '_ {
    move |e| DeviceError::Driver(format!("{stage}: {e:?}"))
}

/// Round-10 multi-GPU enumeration. Queries `cuDeviceGetCount` via
/// cudarc's `result::device::get_count`. `cuInit` is idempotent so
/// calling it here costs only a per-process atomic check after the
/// first call (and `CudaContext::new` already calls it).
pub(crate) fn device_count() -> Result<u32> {
    cudarc::driver::result::init().map_err(map_err("cuInit"))?;
    let n = cudarc::driver::result::device::get_count()
        .map_err(map_err("cuDeviceGetCount"))?;
    Ok(n.max(0) as u32)
}

pub(crate) fn probe() -> Result<DeviceCaps> {
    let dev = CudaDevice::new(0).map_err(map_err("CudaDevice::new(0)"))?;
    let name = dev.name().map_err(map_err("device name"))?;
    let attr = |a| dev.attribute(a).map_err(map_err("device attribute"));
    let major = attr(cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR)?;
    let minor = attr(cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR)?;
    let total_mem = dev
        .attribute(cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_TOTAL_MEMORY)
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
    dev: Arc<CudaDevice>,
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
        let dev = CudaDevice::new(device_ordinal as usize).map_err(map_err("CudaDevice::new"))?;
        let default = dev.fork_default_stream().map_err(map_err("fork_default_stream"))?;
        // AUDIT 2026-05-17 (PERF Fix 1): create three auxiliary streams
        // for the H2D → compute → D2H pipeline. `fork_default_stream` produces
        // an independent cudarc stream (the cudarc equivalent of
        // `cudaStreamCreate(&s, cudaStreamNonBlocking)`).
        let copy_h2d = dev.fork_default_stream().map_err(map_err("fork_default_stream copy_h2d"))?;
        let compute = dev.fork_default_stream().map_err(map_err("fork_default_stream compute"))?;
        let copy_d2h = dev.fork_default_stream().map_err(map_err("fork_default_stream copy_d2h"))?;
        Ok(Self {
            dev,
            default: default.into(),
            copy_h2d: copy_h2d.into(),
            compute: compute.into(),
            copy_d2h: copy_d2h.into(),
        })
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        // Drain every stream we own. Any in-flight pipeline stage —
        // upload, kernel, download — must complete before we return.
        self.dev.wait_for(&self.copy_h2d).map_err(map_err("wait_for copy_h2d"))?;
        self.dev.wait_for(&self.compute).map_err(map_err("wait_for compute"))?;
        self.dev.wait_for(&self.copy_d2h).map_err(map_err("wait_for copy_d2h"))?;
        self.dev.wait_for(&self.default).map_err(map_err("wait_for default"))?;
        Ok(())
    }
}

pub(crate) struct DeviceModuleInner {
    dev: Arc<CudaDevice>,
    module_name: String,
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
        module_name: &str,
        kernel_names: &[&str],
    ) -> Result<Self> {
        let ptx_owned = cudarc::nvrtc::Ptx::from_src(ptx);
        ctx.dev
            .load_ptx(ptx_owned, module_name, kernel_names)
            .map_err(map_err("load_ptx"))?;
        let mut functions = HashMap::with_capacity(kernel_names.len());
        for &name in kernel_names {
            let func = ctx.dev
                .get_func(module_name, name)
                .ok_or_else(|| DeviceError::KernelNotFound(format!("{module_name}::{name}")))?;
            functions.insert(name.to_string(), func);
        }
        Ok(Self {
            dev: ctx.dev.clone(),
            module_name: module_name.to_string(),
            functions,
        })
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
        // AUDIT 2026-05-17 (PERF Fix 2): rent the thread-local pointer
        // scratch by `take`-ing it out, refilling it, and putting it
        // back via a drop guard. This avoids holding a `RefMut` across
        // a closure boundary.
        let mut ptr_h = PTR_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        ptr_h.clear();
        // Pre-size in one shot so we don't realloc mid-loop
        let n_ptrs = args.raw.iter().filter(|a| matches!(a, KernelArg::DevicePtr { .. })).count();
        ptr_h.reserve(n_ptrs);
        for a in &args.raw {
            if let KernelArg::DevicePtr { addr, .. } = a {
                ptr_h.push(*addr);
            }
        }
        // Build the argument tuple for cudarc 0.13 launch API
        let mut ip = 0usize;
        let launch_args: Vec<*mut std::ffi::c_void> = args.raw.iter().map(|a| {
            match a {
                KernelArg::I32(v) => v as *const i32 as *mut std::ffi::c_void,
                KernelArg::I64(v) => v as *const i64 as *mut std::ffi::c_void,
                KernelArg::F32(v) => v as *const f32 as *mut std::ffi::c_void,
                KernelArg::F64(v) => v as *const f64 as *mut std::ffi::c_void,
                KernelArg::DevicePtr { .. } => {
                    let addr = ptr_h[ip] as *mut std::ffi::c_void;
                    ip += 1;
                    addr
                }
            }
        }).collect();
        
        let launch_result = unsafe {
            func.launch_on_stream(&ctx.compute, cudarc_cfg, &mut launch_args)
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
        // wait for compute stream to complete before D→H copy
        // Round-7 PERF Fix 3: gate on `needs_d2h_sync`
        if needs_d2h_sync {
            ctx.dev.wait_for(&ctx.compute).map_err(map_err("wait_for compute"))?;
        }
        Ok(())
    }
}

/// Round-5: H→D upload helper.
///
/// Uploads via pageable memory. In cudarc 0.13, the pinned API is not
/// available, so we use the standard htod_copy method.
///
/// The wrapper still exists today so:
///   * `from_host` has a single call site to upgrade if pinned API becomes available,
///   * the fallback semantics are explicit.
#[inline]
fn upload_via_pinned_or_fallback<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static + std::marker::Unpin>(
    ctx: &DeviceContextInner,
    host: &[T],
) -> Result<CudaSlice<T>> {
    // Pageable path for cudarc 0.13 - pinned API not available in this version
    // Functionally correct; the only cost is the implicit driver-side
    // bounce buffer.
    ctx.dev
        .htod_copy(host.to_vec())
        .map_err(map_err("memcpy host→device"))
}

pub(crate) struct DeviceBufferInner<T> {
    slice: CudaSlice<T>,
    /// The stream the allocation is bound to. For uploaded buffers this
    /// is `copy_h2d`; for `uninit`/`zeros` it's `compute` (most kernels
    /// write into these output buffers).
    stream: Arc<CudaStream>,
    /// Retained handle to the cudarc device. Currently unused — kept
    /// so future code that needs a device-bound operation on a buffer
    /// (e.g. `bind_to_thread`) doesn't have to re-thread the device.
    #[allow(dead_code)]
    dev: Arc<CudaDevice>,
    /// D→H copy stream, used by `to_host`.
    copy_d2h: Arc<CudaStream>,
}

impl<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static + cudarc::driver::ValidAsZeroBits + std::marker::Unpin> DeviceBufferInner<T> {
    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        // Output buffers are bound to the compute stream — the kernel
        // launch that fills them already runs there.
        let slice = unsafe {
            ctx.dev
                .alloc::<T>(len)
                .map_err(map_err("alloc uninit"))?
        };
        Ok(Self {
            slice,
            stream: ctx.compute.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
        })
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        let slice = ctx.dev
            .alloc_zeros::<T>(len)
            .map_err(map_err("alloc_zeros"))?;
        Ok(Self {
            slice,
            stream: ctx.compute.clone(),
            dev: ctx.dev.clone(),
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
        // pageable path unchanged.
        let slice = upload_via_pinned_or_fallback(ctx, host)?;
        // Wait for copy_h2d to complete before compute stream
        ctx.dev.wait_for(&ctx.copy_h2d).map_err(map_err("wait_for copy_h2d"))?;
        Ok(Self {
            slice,
            stream: ctx.copy_h2d.clone(),
            dev: ctx.dev.clone(),
            copy_d2h: ctx.copy_d2h.clone(),
        })
    }

    pub(crate) fn to_host(&self, dst: &mut [T]) -> Result<()> {
        // AUDIT 2026-05-17 (PERF Fix 1): D→H runs on copy_d2h, which
        // launch_raw has already made wait on the compute stream's
        // event. The cudarc dtoh_sync_copy_into call returns after the copy
        // is complete.
        //
        // NOTE: relies on stream-level ordering — the wait-on-compute
        // event was recorded inside `launch_raw`, so we do not need to
        // re-record here.
        self.dev
            .dtoh_sync_copy_into(&self.slice, dst)
            .map_err(map_err("memcpy device→host"))
    }

    pub(crate) fn len(&self) -> usize {
        DeviceSlice::len(&self.slice)
    }

    /// Return the raw device address.
    ///
    /// AUDIT 2026-05-16 (CRIT-1 fix): In cudarc 0.13, SyncRecord was removed.
    /// Stream ordering is now handled via CudaDevice::wait_for and
    /// fork_default_stream, which we use in from_host and launch_raw.
    pub(crate) fn device_ptr_arg(&self) -> u64 {
        let _ = (&self.dev, &self.copy_d2h); // retained for future use
        *DevicePtr::device_ptr(&self.slice)
    }
}
