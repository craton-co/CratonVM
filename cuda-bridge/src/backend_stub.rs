//! No-driver backend.
//!
//! Used when the crate is built without the `cuda` Cargo feature, or on
//! platforms where CUDA isn't available. Every fallible entry point
//! returns [`DeviceError::NoDriver`] so the rest of the workspace can
//! still build, test, and run on commodity hardware.

use crate::{DeviceCaps, DeviceError, KernelArgs, LaunchConfig, Result};
use std::marker::PhantomData;

pub(crate) fn probe() -> Result<DeviceCaps> {
    Err(DeviceError::NoDriver)
}

/// No-driver counterpart to the cuda backend's `device_count`.
/// Always reports zero devices so callers branching on the count
/// don't need to special-case `NoDriver`.
pub(crate) fn device_count() -> Result<u32> {
    Ok(0)
}

#[derive(Clone)]
pub(crate) struct DeviceContextInner;

impl DeviceContextInner {
    pub(crate) fn new(_device_ordinal: u32) -> Result<Self> {
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        Err(DeviceError::NoDriver)
    }
}

pub(crate) struct DeviceModuleInner;

impl DeviceModuleInner {
    pub(crate) fn from_ptx(
        _ctx: &DeviceContextInner,
        _ptx: &str,
        _module_name: &str,
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

    pub(crate) fn launch_raw_no_d2h_sync(
        &self,
        _ctx: &DeviceContextInner,
        _kernel: &str,
        _cfg: &LaunchConfig,
        _args: KernelArgs,
    ) -> Result<()> {
        Err(DeviceError::NoDriver)
    }

    /// Stub-mode counterpart to the cuda backend's autotune helper.
    /// Always returns `None` — `LaunchConfig::elementwise_for_kernel`
    /// then falls back to the portable 256-thread default.
    pub(crate) fn optimal_block_size(
        &self,
        _ctx: &DeviceContextInner,
        _kernel: &str,
    ) -> Option<u32> {
        None
    }
}

pub(crate) struct DeviceBufferInner<T> {
    _phantom: PhantomData<T>,
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

    pub(crate) fn device_ptr_arg(&self) -> u64 {
        0
    }
}
