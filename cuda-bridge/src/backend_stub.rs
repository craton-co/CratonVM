// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! No-driver backend.
//!
//! Used when the crate is built without the `cuda` Cargo feature, or on
//! platforms where CUDA isn't available. Every fallible entry point
//! returns [`DeviceError::NoDriver`] so the rest of the workspace can
//! still build, test, and run on commodity hardware.

use crate::{DeviceCaps, DeviceError, KernelArgs, LaunchConfig, Result};
use std::marker::PhantomData;

pub(crate) fn probe_device(_device_ordinal: u32) -> Result<DeviceCaps> {
    Err(DeviceError::NoDriver)
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
    /// Stub `from_ptx`: produces an inert `DeviceModuleInner` that
    /// carries no driver state.
    ///
    /// Returning `Ok` here is a deliberate exception to the "every
    /// fallible entry point returns `NoDriver`" rule, mirroring
    /// `Stream::new`'s op-log-friendly stub behaviour. The module is
    /// only useful as a `&self` receiver for
    /// `DeviceModule::launch_on_stream`, whose stub branch records a
    /// `StreamOp::Launch` and never touches the module — so an inert
    /// fixture is sufficient to exercise the op-log integration tests
    /// in `tests/stub_op_log.rs` without a real driver. The
    /// driver-bound `DeviceModule::launch_raw` (synchronous, no
    /// stream) still returns `NoDriver` via `launch_raw` below.
    pub(crate) fn from_ptx(
        _ctx: &DeviceContextInner,
        _ptx: &str,
        _module_name: &str,
        _kernel_names: &[&str],
    ) -> Result<Self> {
        Ok(Self)
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

    /// Stub twin of the in-place upload. No driver, no buffer to write.
    pub(crate) fn copy_from_host(&self, _host: &[T]) -> Result<()> {
        Err(DeviceError::NoDriver)
    }

    /// Stub twin of the sub-range download. No driver, so nothing to copy.
    pub(crate) fn to_host_range(&self, _dst: &mut [T], _offset: usize) -> Result<()> {
        Err(DeviceError::NoDriver)
    }

    pub(crate) fn len(&self) -> usize {
        0
    }

    pub(crate) fn device_ptr_arg(&self) -> u64 {
        0
    }
}

/// Stub twin of the page-locked staging buffer: an ordinary heap
/// allocation, since there is no driver to pin anything with.
pub(crate) struct PinnedHostInner<T: Copy> {
    buf: Vec<T>,
}

impl<T: Copy + Default> PinnedHostInner<T> {
    pub(crate) fn new(_ctx: &crate::DeviceContext, len: usize) -> Result<Self> {
        Ok(Self {
            buf: vec![T::default(); len],
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// # Safety
    /// See [`crate::PinnedHostBuffer::as_mut_slice`]. Stub mode queues no
    /// DMA, so there is nothing to race with.
    pub(crate) unsafe fn as_mut_slice(&self) -> &mut [T] {
        std::slice::from_raw_parts_mut(self.buf.as_ptr() as *mut T, self.buf.len())
    }
}
