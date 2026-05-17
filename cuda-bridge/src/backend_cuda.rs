//! cudarc-backed real CUDA backend.
//!
//! PHASE2-CUDA-TODO: the cudarc 0.13.9 API pinned in this workspace does
//! NOT expose `CudaContext` / `CudaStream::launch_builder` / `new_stream`
//! / `memcpy_stod` etc. — the entire pre-existing implementation of this
//! file was written against an unreleased / future cudarc surface and
//! never actually compiled against the pinned version. To unblock the
//! `cuda` feature build, this module is currently stubbed: every entry
//! point returns `DeviceError::NoDriver` (mirroring `backend_stub`) and
//! the cudarc crate is not referenced. The real implementation needs
//! to be ported to the cudarc 0.13 `CudaDevice` API (see
//! `cudarc::driver::safe::CudaDevice` / `fork_default_stream` /
//! `htod_sync_copy` / `LaunchAsync::launch`).
//!
//! Only compiled when the `cuda` Cargo feature is enabled.

use crate::{DeviceCaps, DeviceError, KernelArgs, LaunchConfig, Result};
use std::marker::PhantomData;
use std::sync::Arc;

pub(crate) fn probe() -> Result<DeviceCaps> {
    // PHASE2-CUDA-TODO: port to `cudarc::driver::CudaDevice::new(0)`.
    Err(DeviceError::NoDriver)
}

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    // Held only so the type has a non-zero size for any future field;
    // unused while the backend is stubbed.
    _marker: Arc<()>,
}

impl DeviceContextInner {
    pub(crate) fn new(_device_ordinal: u32) -> Result<Self> {
        // PHASE2-CUDA-TODO: real CUDA init lives here.
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        Err(DeviceError::NoDriver)
    }
}

pub(crate) struct DeviceModuleInner {
    _marker: PhantomData<()>,
}

impl DeviceModuleInner {
    pub(crate) fn from_ptx(
        _ctx: &DeviceContextInner,
        _ptx: &str,
        _kernel_names: &[&str],
    ) -> Result<Self> {
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn launch_raw(
        &self,
        _ctx: &DeviceContextInner,
        _kernel: &str,
        _cfg: &LaunchConfig,
        _args: KernelArgs,
    ) -> Result<()> {
        Err(DeviceError::NoDriver)
    }
}

pub(crate) struct DeviceBufferInner<T> {
    pub(crate) _phantom: PhantomData<T>,
}

impl<T> DeviceBufferInner<T> {
    pub(crate) fn uninit(_ctx: &DeviceContextInner, _len: usize) -> Result<Self> {
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn zeros(_ctx: &DeviceContextInner, _len: usize) -> Result<Self> {
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn from_host(_ctx: &DeviceContextInner, _host: &[T]) -> Result<Self> {
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn to_host(&self, _dst: &mut [T]) -> Result<()> {
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn len(&self) -> usize {
        0
    }

    pub(crate) fn device_ptr(&self) -> u64 {
        0
    }
}
