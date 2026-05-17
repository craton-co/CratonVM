//! cudarc-backed real CUDA backend.
//!
//! Only compiled when the `cuda` Cargo feature is enabled.
//!
//! This file intentionally keeps the surface narrow: device discovery,
//! PTX module loading, allocation, memcpy, and `cuLaunchKernel`. The
//! moment cudarc's API moves under us, all the fan-out stays in this
//! file — the public crate API in `lib.rs` is unchanged.

use crate::{DeviceCaps, DeviceError, KernelArg, KernelArgs, LaunchConfig, Result};
use cudarc::driver::{
    CudaContext, CudaFunction, CudaModule, CudaSlice, CudaStream, DeviceRepr, LaunchConfig as CudarcLaunchConfig,
    PushKernelArg,
};
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

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        let ctx = CudaContext::new(device_ordinal as usize).map_err(map_err("CudaContext::new"))?;
        let stream = ctx.default_stream();
        Ok(Self { ctx, stream })
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        self.stream.synchronize().map_err(map_err("stream synchronize"))
    }
}

pub(crate) struct DeviceModuleInner {
    module: Arc<CudaModule>,
    functions: HashMap<String, CudaFunction>,
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
        let func = self
            .functions
            .get(kernel)
            .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?;
        let cudarc_cfg = CudarcLaunchConfig {
            grid_dim: cfg.grid,
            block_dim: cfg.block,
            shared_mem_bytes: cfg.shared_bytes,
        };
        let mut builder = ctx.stream.launch_builder(func);
        // AUDIT 2026-05-16 (CRIT-1 fix): `args` is bound here for the
        // whole body of `launch_raw` and only goes out of scope after
        // `builder.launch(...)` returns. That keeps every
        // `KernelArg::DevicePtr { _record, .. }` (and its embedded
        // `cudarc::driver::SyncRecord`) alive across the launch, which
        // is the entire point of plumbing the record through — it is
        // load-bearing via `Drop` ordering. Do not refactor this into
        // a function that consumes `args` before `launch` is called.
        let args = args;
        // The argument-builder API requires each scalar to be referenced
        // by a stable address that outlives the builder. The struct
        // variants in `args.raw` already own their bytes by-value, so
        // we hand the builder direct references into them. For
        // `DevicePtr` we need a `u64` address slot the builder can
        // reference, which we keep in a parallel vec alongside `args`.
        let mut ptr_h: Vec<u64> = Vec::new();
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
        unsafe {
            builder
                .launch(cudarc_cfg)
                .map_err(map_err("kernel launch"))?;
        }
        // Explicit drop site: `args` (with its SyncRecords) and `ptr_h`
        // are dropped here, AFTER the launch has been submitted. Do not
        // move this drop earlier — see the audit note above.
        drop(ptr_h);
        drop(args);
        Ok(())
    }
}

pub(crate) struct DeviceBufferInner<T> {
    slice: CudaSlice<T>,
    stream: Arc<CudaStream>,
}

impl<T: bytemuck::Pod + DeviceRepr + Send + Sync + 'static> DeviceBufferInner<T> {
    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        let slice = unsafe {
            ctx.stream
                .alloc::<T>(len)
                .map_err(map_err("alloc uninit"))?
        };
        Ok(Self {
            slice,
            stream: ctx.stream.clone(),
        })
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        let slice = ctx
            .stream
            .alloc_zeros::<T>(len)
            .map_err(map_err("alloc_zeros"))?;
        Ok(Self {
            slice,
            stream: ctx.stream.clone(),
        })
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        let slice = ctx
            .stream
            .memcpy_stod(host)
            .map_err(map_err("memcpy host→device"))?;
        Ok(Self {
            slice,
            stream: ctx.stream.clone(),
        })
    }

    pub(crate) fn to_host(&self, dst: &mut [T]) -> Result<()> {
        self.stream
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
    pub(crate) fn device_ptr_arg(&self) -> (u64, cudarc::driver::SyncRecord) {
        self.slice.device_ptr(&self.stream)
    }
}
