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

    pub(crate) fn len(&self) -> usize {
        0
    }

    pub(crate) fn device_ptr_arg(&self) -> u64 {
        0
    }
}
