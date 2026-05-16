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
        // The argument-builder API requires each value to live as long
        // as the builder. We stage values in `holders` keyed by index.
        let mut i32_h = Vec::new();
        let mut i64_h = Vec::new();
        let mut f32_h = Vec::new();
        let mut f64_h = Vec::new();
        let mut ptr_h = Vec::new();
        for a in &args.raw {
            match a {
                KernelArg::I32(v) => {
                    i32_h.push(*v);
                }
                KernelArg::I64(v) => {
                    i64_h.push(*v);
                }
                KernelArg::F32(v) => {
                    f32_h.push(*v);
                }
                KernelArg::F64(v) => {
                    f64_h.push(*v);
                }
                KernelArg::DevicePtr(p) => {
                    ptr_h.push(*p);
                }
            }
        }
        // Bind in original order.
        let (mut ii, mut il, mut ff, mut fd, mut ip) = (0, 0, 0, 0, 0);
        for a in &args.raw {
            match a {
                KernelArg::I32(_) => {
                    builder.arg(&i32_h[ii]);
                    ii += 1;
                }
                KernelArg::I64(_) => {
                    builder.arg(&i64_h[il]);
                    il += 1;
                }
                KernelArg::F32(_) => {
                    builder.arg(&f32_h[ff]);
                    ff += 1;
                }
                KernelArg::F64(_) => {
                    builder.arg(&f64_h[fd]);
                    fd += 1;
                }
                KernelArg::DevicePtr(_) => {
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

    pub(crate) fn device_ptr(&self) -> u64 {
        // CudaSlice exposes a CUdeviceptr; cast to u64 for our arg list.
        let (ptr, _record) = self.slice.device_ptr(&self.stream);
        ptr
    }
}
