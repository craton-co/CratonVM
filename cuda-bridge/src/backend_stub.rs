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

/// Stub twin of the cuda backend's driver-version query. No driver, no
/// version — the caller reads this as "do not clamp the target".
pub(crate) fn driver_cuda_version() -> Result<u32> {
    Err(DeviceError::NoDriver)
}

#[derive(Clone)]
pub(crate) struct DeviceContextInner;

impl DeviceContextInner {
    pub(crate) fn new(_device_ordinal: u32) -> Result<Self> {
        Err(DeviceError::NoDriver)
    }

    /// Stub twin of the cuda backend's allocator-event recording. No
    /// driver, no memset, nothing to order.
    pub(crate) fn record_alloc_event(&self, _event: &crate::Event) -> Result<()> {
        Ok(())
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
    /// in `tests/stub_op_log.rs` without a real driver.
    pub(crate) fn from_ptx(
        _ctx: &DeviceContextInner,
        _ptx: &str,
        _module_name: &str,
        _kernel_names: &[&'static str],
    ) -> Result<Self> {
        Ok(Self)
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

// ── Backend contract ─────────────────────────────────────────────────
//
// The stub answers the portable surface honestly: it has no device, so
// every operation that would need one returns the same "no CUDA device"
// error the rest of the crate already treats as "run on the CPU".
// Implementing the trait rather than omitting the methods is the point
// -- it is what makes the contract checkable instead of conventional.

/// Marker for the no-device backend.
pub(crate) struct StubBackend;

impl crate::backend_api::BackendApi for StubBackend {
    type Context = DeviceContextInner;
    type Module = DeviceModuleInner;
    /// No device, so nothing to order against.
    type Stream = ();

    fn probe_device(device_ordinal: u32) -> Result<DeviceCaps> {
        probe_device(device_ordinal)
    }

    fn driver_cuda_version() -> Result<u32> {
        driver_cuda_version()
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
        // Nothing is current because nothing exists; succeeding here
        // keeps the caller's control flow identical in both builds.
        Ok(())
    }

    fn record_alloc_event(&self, event: &crate::Event) -> Result<()> {
        self.record_alloc_event(event)
    }
}

impl crate::backend_api::DeviceModuleApi for DeviceModuleInner {
    type Ctx = DeviceContextInner;
    type Stream = ();

    fn from_ptx(
        ctx: &Self::Ctx,
        ptx: &str,
        module_name: &str,
        kernel_names: &[&'static str],
    ) -> Result<Self> {
        Self::from_ptx(ctx, ptx, module_name, kernel_names)
    }

    fn optimal_block_size(&self, _ctx: &Self::Ctx, _kernel: &str) -> Option<u32> {
        // `None` is the documented "cannot answer" reply, and the caller
        // falls back to a fixed block size. Not an error.
        None
    }

    fn launch_raw_on_stream(
        &self,
        _ctx: &Self::Ctx,
        _stream: &Self::Stream,
        kernel: &str,
        _cfg: &LaunchConfig,
        _args: KernelArgs,
    ) -> Result<()> {
        let _ = kernel;
        // `NoDriver` is the variant the rest of the crate already reads as
        // "there is no device; run on the CPU". Inventing a new one here
        // would make the stub's refusal look different from every other
        // no-device refusal.
        Err(DeviceError::NoDriver)
    }
}
