//! cudarc-backed real CUDA backend.
//!
//! Ported against the actual cudarc 0.13.9 surface as it lives in the
//! Cargo cache (`cudarc::driver::safe`). The cudarc 0.13 type model is:
//!
//! - `CudaDevice` is the single Arc-shared handle that owns the primary
//!   context, the default stream, and a per-device event for sync
//!   bookkeeping. Constructed via `CudaDevice::new(ordinal: usize)
//!   -> Result<Arc<Self>, DriverError>`.
//! - `CudaSlice<T>` is a typed device allocation tied to the owning
//!   `Arc<CudaDevice>`. The safe alloc/copy methods on `CudaDevice`
//!   require `T: DeviceRepr` (and `ValidAsZeroBits` for `alloc_zeros`).
//!   Our `lib.rs` only requires `T: bytemuck::Pod + Send + Sync +
//!   'static`, so to avoid forcing extra trait bounds on the public
//!   API we go through the raw `result::*` namespace for alloc / copy /
//!   free. `result::malloc_sync`, `result::memcpy_htod_sync<T>`,
//!   `result::memcpy_dtoh_sync<T>`, and `result::memset_d8_sync` all
//!   accept `T` without any extra trait bound.
//! - `CudaStream` is a non-default stream created via
//!   `device.fork_default_stream()`. The default stream is implicit and
//!   stored inside `CudaDevice`. Stream-bound copy/launch helpers live
//!   in `stream.rs` / `async_memcpy.rs` / `launch.rs`.
//! - `CudaModule` is `pub(crate)` and is _not_ a separately addressable
//!   handle. cudarc stores it inside a `BTreeMap` on the device, keyed
//!   by user-supplied module name. The safe `load_ptx` insists on
//!   `&[&'static str]` for the function names; our `kernel_names:
//!   &[&str]` parameter cannot satisfy that without leaking, so we
//!   side-step the safe wrapper and call `result::module::load_data` +
//!   `result::module::get_function` directly. This matches cudarc's
//!   own implementation pattern and is the route documented for "if
//!   the safe layer doesn't admit what you need, drop down to result".
//! - `CudaFunction` is just a `cu_function: sys::CUfunction` + an
//!   `Arc<CudaDevice>`. We hold the raw `sys::CUfunction` directly
//!   alongside the module so we never have to round-trip through
//!   `CudaDevice::get_func`.
//!
//! The `lib.rs` `KernelArg::DevicePtr(u64)` tuple variant pre-marshals
//! addresses to `u64` (== cudarc's `sys::CUdeviceptr`), so the launch
//! path just needs to point `kernel_params` at the storage cells of
//! the `KernelArg::*` enum payloads, then call `result::launch_kernel`.
//!
//! Only compiled when the `cuda` Cargo feature is enabled.

use crate::{DeviceCaps, DeviceError, KernelArg, KernelArgs, LaunchConfig, Result};
use cudarc::driver::safe::CudaDevice;
use cudarc::driver::{result, sys};
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::marker::PhantomData;
use std::sync::Arc;

fn map_err<E: std::fmt::Debug>(stage: &'static str) -> impl FnOnce(E) -> DeviceError {
    move |e| DeviceError::Driver(format!("{stage}: {e:?}"))
}

pub(crate) fn probe() -> Result<DeviceCaps> {
    let dev = CudaDevice::new(0).map_err(map_err("CudaDevice::new(0)"))?;
    let name = dev.name().map_err(map_err("device name"))?;
    let attr = |a| dev.attribute(a).map_err(map_err("device attribute"));
    let major = attr(sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR)?;
    let minor = attr(sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR)?;
    // `result::device::total_mem` is unsafe (raw cuDeviceTotalMem_v2)
    // and takes the underlying `sys::CUdevice`. cudarc's `CudaDevice::
    // cu_device()` returns `&sys::CUdevice` — we copy the value.
    let cu_dev = *dev.cu_device();
    let total_mem =
        unsafe { result::device::total_mem(cu_dev) }.map_err(map_err("device total_mem"))?;
    Ok(DeviceCaps {
        ordinal: 0,
        name,
        compute_major: major as u32,
        compute_minor: minor as u32,
        total_global_mem: total_mem as u64,
    })
}

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    /// The cudarc device handle. `Arc<CudaDevice>` is the only context
    /// representation cudarc 0.13 exposes — primary context, default
    /// stream, and a per-device event for sync all live inside it.
    pub(crate) device: Arc<CudaDevice>,
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        let device = CudaDevice::new(device_ordinal as usize)
            .map_err(map_err("CudaDevice::new"))?;
        Ok(Self { device })
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        // `CudaDevice::synchronize` drains the default stream that the
        // device owns. This is the cudarc analogue of
        // `cuStreamSynchronize` on the default stream.
        self.device
            .synchronize()
            .map_err(map_err("device synchronize"))
    }

    /// Crate-internal accessor used by `stream.rs` / `event.rs` /
    /// `async_memcpy.rs` to reach the underlying `CudaDevice` without
    /// re-creating it. The returned `Arc` is cheap to clone.
    pub(crate) fn device(&self) -> &Arc<CudaDevice> {
        &self.device
    }
}

/// A loaded PTX module. We hold the raw `sys::CUmodule` ourselves
/// (alongside one `sys::CUfunction` per requested kernel) because
/// cudarc's safe `load_ptx` requires `&[&'static str]` for the function
/// names — incompatible with our `&[&str]` parameter without leaking.
/// Going through `result::module::*` is exactly what cudarc does
/// internally so this is not a layering violation.
pub(crate) struct DeviceModuleInner {
    /// Retained so we can call `cuModuleUnload` in `Drop`.
    cu_module: sys::CUmodule,
    /// Retained so the underlying `Arc<CudaDevice>` (and therefore the
    /// primary context) outlives the module's `cu_module`. Also used
    /// in `launch_raw` to bind the calling thread before launching.
    device: Arc<CudaDevice>,
    /// Resolved function handles, keyed by user-facing kernel name.
    functions: HashMap<String, sys::CUfunction>,
}

// `sys::CUmodule` and `sys::CUfunction` are raw pointers and not
// `Send`/`Sync` by default. cuda modules and functions are safe to
// share across threads once loaded (the primary context handles the
// binding). cudarc's own `CudaModule` ships with the same unsafe impls.
unsafe impl Send for DeviceModuleInner {}
unsafe impl Sync for DeviceModuleInner {}

impl Drop for DeviceModuleInner {
    fn drop(&mut self) {
        // Bind to the owning context before unloading — same pattern
        // cudarc uses in `CudaDevice::drop`.
        let _ = self.device.bind_to_thread();
        unsafe {
            // Ignore the unload error: dropping with an outstanding
            // launch is a use-after-free that the user already
            // committed, and panicking from Drop would mask the real
            // bug.
            let _ = result::module::unload(self.cu_module);
        }
    }
}

impl DeviceModuleInner {
    pub(crate) fn from_ptx(
        ctx: &DeviceContextInner,
        ptx: &str,
        kernel_names: &[&str],
    ) -> Result<Self> {
        ctx.device
            .bind_to_thread()
            .map_err(map_err("bind_to_thread"))?;
        // Load the PTX as a null-terminated C string. cudarc's safe
        // path (`load_ptx` with `PtxKind::Src`) does exactly this; we
        // inline because we don't want the safe path's
        // `BTreeMap<String, CudaModule>` storage (it forces
        // `&[&'static str]` for function names).
        let c_ptx = CString::new(ptx)
            .map_err(|e| DeviceError::Load(format!("PTX contains interior NUL: {e:?}")))?;
        let cu_module = unsafe { result::module::load_data(c_ptx.as_ptr() as *const _) }
            .map_err(|e| DeviceError::Load(format!("cuModuleLoadData: {e:?}")))?;
        let mut functions = HashMap::with_capacity(kernel_names.len());
        for &name in kernel_names {
            let c_name = CString::new(name).map_err(|e| {
                DeviceError::KernelNotFound(format!("kernel name `{name}` invalid C string: {e:?}"))
            })?;
            let cu_function = unsafe { result::module::get_function(cu_module, c_name) }
                .map_err(|e| DeviceError::KernelNotFound(format!("{name}: {e:?}")))?;
            functions.insert(name.to_string(), cu_function);
        }
        Ok(Self {
            cu_module,
            device: ctx.device.clone(),
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
        ctx.device
            .bind_to_thread()
            .map_err(map_err("bind_to_thread"))?;
        // Launch on the device's default stream — the cudarc-0.13
        // single-stream model.
        let stream = *ctx.device.cu_stream();
        launch_on_raw_stream(self, kernel, cfg, args, stream)
    }

}

/// Common launch path shared between `launch_raw` (default stream) and
/// `launch.rs`'s `launch_on_stream` (caller-supplied stream).
///
/// `cuLaunchKernel` (which cudarc's `result::launch_kernel` thinly
/// wraps) expects `kernel_params` to be an array of pointers to each
/// argument's *storage cell* — NOT the value itself. We therefore
/// build the `Vec<*mut c_void>` against `args.raw` (which lives on the
/// stack across the call) and rely on `KernelArg::DevicePtr(u64)`'s
/// payload being bit-compatible with `sys::CUdeviceptr` (both are
/// `u64`).
pub(crate) fn launch_on_raw_stream(
    module: &DeviceModuleInner,
    kernel: &str,
    cfg: &LaunchConfig,
    args: KernelArgs,
    stream: sys::CUstream,
) -> Result<()> {
    let func = *module
        .functions
        .get(kernel)
        .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?;
    let mut params: Vec<*mut c_void> = Vec::with_capacity(args.raw.len());
    for a in &args.raw {
        let p: *const () = match a {
            KernelArg::DevicePtr(addr) => addr as *const u64 as *const (),
            KernelArg::I32(v) => v as *const i32 as *const (),
            KernelArg::I64(v) => v as *const i64 as *const (),
            KernelArg::F32(v) => v as *const f32 as *const (),
            KernelArg::F64(v) => v as *const f64 as *const (),
        };
        params.push(p as *mut c_void);
    }
    unsafe {
        result::launch_kernel(
            func,
            cfg.grid,
            cfg.block,
            cfg.shared_bytes,
            stream,
            &mut params,
        )
    }
    .map_err(|e| DeviceError::Launch(format!("{kernel}: {e:?}")))?;
    // Hold `args` (and the pointer cells `params` borrows from) alive
    // until after the launch is submitted. The kernel itself runs
    // async on the stream, but `cuLaunchKernel` reads `kernel_params`
    // synchronously before returning, so a sync-mode drop here is
    // fine; we just must NOT drop earlier.
    drop(args);
    Ok(())
}

/// A typed device-side allocation. We bypass `CudaSlice<T>` because its
/// alloc/copy helpers require `T: DeviceRepr`, which lib.rs does not
/// promise. Instead we hold the raw `sys::CUdeviceptr` and free it in
/// `Drop` via `result::free_sync`.
pub(crate) struct DeviceBufferInner<T> {
    cu_device_ptr: sys::CUdeviceptr,
    len: usize,
    /// Keeps the owning context alive for the buffer's lifetime — both
    /// for `Drop` (must call `bind_to_thread` before `cuMemFree`) and
    /// for accessor methods (`to_host` issues a D→H copy and needs to
    /// know which device the pointer lives on).
    device: Arc<CudaDevice>,
    _marker: PhantomData<T>,
}

// `sys::CUdeviceptr` is a `u64` typedef; the buffer is safe to send
// across threads as long as the owning context is. cudarc's `CudaSlice`
// makes the same `Send`/`Sync` claim under `T: Send`/`T: Sync`.
unsafe impl<T: Send> Send for DeviceBufferInner<T> {}
unsafe impl<T: Sync> Sync for DeviceBufferInner<T> {}

impl<T> Drop for DeviceBufferInner<T> {
    fn drop(&mut self) {
        let _ = self.device.bind_to_thread();
        unsafe {
            // Sync free: works regardless of whether the device's
            // async-pool is supported. cudarc's `CudaSlice::drop`
            // branches on `is_async`; we keep things simple here
            // because the public API never promised async free.
            let _ = unsafe { result::free_sync(self.cu_device_ptr) };
        }
    }
}

impl<T: bytemuck::Pod + Send + Sync + 'static> DeviceBufferInner<T> {
    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        ctx.device
            .bind_to_thread()
            .map_err(map_err("bind_to_thread"))?;
        let num_bytes = len.saturating_mul(std::mem::size_of::<T>());
        let cu_device_ptr = unsafe { result::malloc_sync(num_bytes) }
            .map_err(map_err("malloc_sync (uninit)"))?;
        Ok(Self {
            cu_device_ptr,
            len,
            device: ctx.device.clone(),
            _marker: PhantomData,
        })
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        ctx.device
            .bind_to_thread()
            .map_err(map_err("bind_to_thread"))?;
        let num_bytes = len.saturating_mul(std::mem::size_of::<T>());
        let cu_device_ptr = unsafe { result::malloc_sync(num_bytes) }
            .map_err(map_err("malloc_sync (zeros)"))?;
        // `memset_d8_sync` writes one byte at a time; zeroing the whole
        // buffer with byte=0 is correct for all `bytemuck::Pod` types
        // (Pod implies zero is a valid bit pattern; cudarc's
        // `ValidAsZeroBits` is the same idea).
        unsafe { result::memset_d8_sync(cu_device_ptr, 0, num_bytes) }
            .map_err(|e| {
                // Free the allocation we just made before propagating.
                let _ = unsafe { result::free_sync(cu_device_ptr) };
                DeviceError::Driver(format!("memset_d8_sync (zeros): {e:?}"))
            })?;
        Ok(Self {
            cu_device_ptr,
            len,
            device: ctx.device.clone(),
            _marker: PhantomData,
        })
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        ctx.device
            .bind_to_thread()
            .map_err(map_err("bind_to_thread"))?;
        let len = host.len();
        let num_bytes = std::mem::size_of_val(host);
        let cu_device_ptr = unsafe { result::malloc_sync(num_bytes) }
            .map_err(map_err("malloc_sync (from_host)"))?;
        unsafe { result::memcpy_htod_sync::<T>(cu_device_ptr, host) }.map_err(|e| {
            let _ = unsafe { result::free_sync(cu_device_ptr) };
            DeviceError::Memcpy(format!("memcpy_htod_sync: {e:?}"))
        })?;
        // `memcpy_htod_sync` submits on the default stream; the cudarc
        // safe-wrapper pattern is to synchronize before returning so
        // the caller may immediately reuse the host buffer. We follow
        // the same convention here.
        ctx.device
            .synchronize()
            .map_err(map_err("synchronize after htod"))?;
        Ok(Self {
            cu_device_ptr,
            len,
            device: ctx.device.clone(),
            _marker: PhantomData,
        })
    }

    pub(crate) fn to_host(&self, dst: &mut [T]) -> Result<()> {
        if dst.len() != self.len {
            return Err(DeviceError::Memcpy(format!(
                "to_host length mismatch: dst.len()={}, slice.len()={}",
                dst.len(),
                self.len
            )));
        }
        self.device
            .bind_to_thread()
            .map_err(map_err("bind_to_thread"))?;
        unsafe { result::memcpy_dtoh_sync::<T>(dst, self.cu_device_ptr) }
            .map_err(|e| DeviceError::Memcpy(format!("memcpy_dtoh_sync: {e:?}")))?;
        self.device
            .synchronize()
            .map_err(map_err("synchronize after dtoh"))?;
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

// Crate-internal accessors usable on any T. `device_ptr` is here
// (not in the `Pod + Send + Sync` block above) because `lib.rs::
// push_device_ptr` is generic in `T` with no bounds and would
// otherwise hit E0599 method-not-found.
impl<T> DeviceBufferInner<T> {
    /// Return the raw device address. `sys::CUdeviceptr` is a `u64`
    /// typedef in the driver API, so this is just an integer copy.
    pub(crate) fn device_ptr(&self) -> u64 {
        self.cu_device_ptr as u64
    }

    pub(crate) fn raw_ptr(&self) -> sys::CUdeviceptr {
        self.cu_device_ptr
    }

    pub(crate) fn device_arc(&self) -> &Arc<CudaDevice> {
        &self.device
    }

    /// Stream-aware allocation used by `async_memcpy.rs`'s
    /// `from_host_async`: allocates `len * size_of::<T>()` bytes,
    /// records the upload on `stream`, and returns the wrapper. The
    /// caller is responsible for issuing the actual memcpy.
    ///
    /// This is the only allocation entry point that returns a
    /// not-yet-initialized buffer without bouncing through the
    /// `T: bytemuck::Pod` path; `async_memcpy.rs` fills it
    /// immediately after.
    pub(crate) fn uninit_raw(ctx: &DeviceContextInner, num_bytes: usize, len: usize) -> Result<Self>
    where
        T: 'static,
    {
        ctx.device
            .bind_to_thread()
            .map_err(map_err("bind_to_thread"))?;
        let cu_device_ptr = unsafe { result::malloc_sync(num_bytes) }
            .map_err(map_err("malloc_sync (async uninit)"))?;
        Ok(Self {
            cu_device_ptr,
            len,
            device: ctx.device.clone(),
            _marker: PhantomData,
        })
    }
}
