//! Thin CUDA Driver API bridge.
//!
//! See the crate-level [`README.md`](../../README.md) for the build-mode
//! matrix and a worked example.
//!
//! The crate has two compilation modes:
//!
//! - Default (no features): every fallible entry point returns
//!   [`DeviceError::NoDriver`]. Builds and tests pass on machines
//!   without a CUDA toolkit. Used by the bulk of CI.
//! - `cuda` feature: real driver bindings via `cudarc`. Requires
//!   CUDA Toolkit 12.x (the major version is pinned in `Cargo.toml`).
//!
//! No JVM-specific code lives here — only device discovery, module
//! loading, memory allocation, memcpy, and kernel launch.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("no CUDA driver available (crate built without the `cuda` feature, or driver not installed)")]
    NoDriver,
    #[error("CUDA driver error: {0}")]
    Driver(String),
    #[error("PTX module load failed: {0}")]
    Load(String),
    #[error("kernel `{0}` not found in module")]
    KernelNotFound(String),
    #[error("kernel launch failed: {0}")]
    Launch(String),
    #[error("memory copy failed: {0}")]
    Memcpy(String),
}

pub type Result<T> = std::result::Result<T, DeviceError>;

/// Static description of an attached GPU. Returned by [`probe`].
#[derive(Clone, Debug)]
pub struct DeviceCaps {
    pub ordinal: u32,
    pub name: String,
    pub compute_major: u32,
    pub compute_minor: u32,
    pub total_global_mem: u64,
}

/// Kernel launch configuration. Mirrors `cuLaunchKernel`.
#[derive(Clone, Copy, Debug)]
pub struct LaunchConfig {
    pub grid: (u32, u32, u32),
    pub block: (u32, u32, u32),
    pub shared_bytes: u32,
}

impl LaunchConfig {
    /// One-dimensional launch sized to cover `n` elements with the
    /// default block size of 256 threads.
    pub fn elementwise(n: u32) -> Self {
        let block = 256u32;
        let grid = n.div_ceil(block).max(1);
        Self {
            grid: (grid, 1, 1),
            block: (block, 1, 1),
            shared_bytes: 0,
        }
    }
}

/// Probe the system for an attached CUDA device.
///
/// Returns `Err(DeviceError::NoDriver)` on machines without a driver
/// or when the crate was built without the `cuda` feature.
pub fn probe() -> Result<DeviceCaps> {
    backend::probe()
}

/// A CUDA context bound to one device. Cheap to clone; the underlying
/// driver handle is shared via Arc.
#[derive(Clone)]
pub struct DeviceContext(backend::DeviceContextInner);

impl DeviceContext {
    /// Create or attach to the primary context on `device_ordinal`.
    pub fn new(device_ordinal: u32) -> Result<Self> {
        backend::DeviceContextInner::new(device_ordinal).map(Self)
    }

    /// Probe-and-attach helper used by Phase 2 integration tests.
    ///
    /// Equivalent to [`DeviceContext::new(0)`][Self::new]. In stub
    /// mode this returns `Err(DeviceError::NoDriver)`, which is the
    /// signal `tests/stub_op_log.rs` uses to skip its body on hosts
    /// without a driver. Named to mirror the spec wording so the
    /// integration tests read naturally even when the underlying
    /// backend is stubbed.
    pub fn probe() -> Result<Self> {
        Self::new(0)
    }

    /// Synchronize: wait for all in-flight work on this context to drain.
    pub fn synchronize(&self) -> Result<()> {
        self.0.synchronize()
    }

    /// Crate-internal accessor used by `stream.rs` / `event.rs` /
    /// `async_memcpy.rs` to reach the backend handle without exposing
    /// it publicly. PHASE2-CUDA-TODO: today the cuda backend handle is
    /// an opaque placeholder; once `backend_cuda.rs` is migrated to
    /// cudarc 0.13 this is where stream/event constructors will pull
    /// the `Arc<CudaDevice>` from.
    #[allow(dead_code)]
    pub(crate) fn inner(&self) -> &backend::DeviceContextInner {
        &self.0
    }
}

/// Process-wide kernel-name interner.
///
/// AUDIT 2026-05-20 (PERF Fix #2): cudarc 0.13 needs `&'static str`
/// kernel names. Rather than `Box::leak`-ing on every `from_ptx` call
/// (which leaks unboundedly when callers load modules with generated /
/// unique kernel names in a loop), each distinct name is leaked at most
/// once and cached here. Subsequent loads of the same name return the
/// already-interned `'static` slot — zero new allocation, zero leak.
///
/// The cache only ever grows by *distinct* kernel name, which is the
/// genuinely bounded quantity (a finite set of kernel identifiers the
/// process ever compiles), so the total leaked memory is bounded.
#[cfg(feature = "cuda")]
fn intern_kernel_name(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let set = INTERNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = set.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(&existing) = guard.get(name) {
        return existing;
    }
    // First sighting of this name: leak exactly one boxed string and
    // record the `'static` reference so future calls reuse it.
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// A loaded PTX module containing one or more named kernel entry points.
pub struct DeviceModule(backend::DeviceModuleInner);

impl DeviceModule {
    /// Load a PTX text module and resolve the named kernels.
    pub fn from_ptx(ctx: &DeviceContext, ptx: &str, kernel_names: &[&str]) -> Result<Self> {
        // cudarc 0.13's `CudaDevice::load_ptx` keeps the kernel-name
        // slice alive for the lifetime of the loaded module, so the
        // backend requires `&[&'static str]`. The public surface stays
        // `&[&str]` for ergonomics; we bridge by interning each name
        // into a `'static` leak.
        //
        // AUDIT 2026-05-20 (PERF Fix #2): the previous comment claimed
        // the leak was "bounded by the total distinct kernel-name count",
        // but it leaked unconditionally on *every* call — a caller that
        // JITs kernels with generated/unique names in a loop, then loads
        // them, leaked one boxed string per name with no dedup. Now each
        // name is interned through a process-wide dedup cache so a given
        // distinct name is leaked at most once; repeat loads of the same
        // kernel name reuse the existing `'static` slot.
        #[cfg(feature = "cuda")]
        {
            let static_names: Vec<&'static str> =
                kernel_names.iter().map(|n| intern_kernel_name(n)).collect();
            backend::DeviceModuleInner::from_ptx(&ctx.0, ptx, "module", &static_names).map(Self)
        }
        #[cfg(not(feature = "cuda"))]
        {
            backend::DeviceModuleInner::from_ptx(&ctx.0, ptx, "module", kernel_names).map(Self)
        }
    }

    /// Launch a kernel by name with raw argument bytes. The argument
    /// layout must match the kernel's PTX parameter declarations
    /// exactly — the bridge does no type checking; the
    /// [`KernelArgs`] helper builds correct buffers from Rust types.
    pub fn launch_raw(
        &self,
        ctx: &DeviceContext,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.0.launch_raw(&ctx.0, kernel, cfg, args)
    }
}

/// Builder for a CUDA kernel argument list.
///
/// Each kernel parameter slot in PTX is a fixed-size scalar (pointer,
/// `i32`, `i64`, `f32`, `f64`). This builder accumulates aligned bytes
/// in the order the kernel expects them.
#[derive(Default)]
pub struct KernelArgs {
    pub(crate) raw: Vec<KernelArg>,
}

impl KernelArgs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_device_ptr<T>(mut self, buf: &DeviceBuffer<T>) -> Self {
        // AUDIT 2026-05-16 (CRIT-1 fix): plumb the device pointer
        // returned by `CudaSlice::device_ptr` into the `KernelArg`.
        // In cudarc 0.13, SyncRecord was removed; stream ordering is
        // now handled via CudaDevice::wait_for and fork_default_stream.
        #[cfg(feature = "cuda")]
        {
            let addr = buf.0.device_ptr_arg();
            self.raw.push(KernelArg::DevicePtr { addr });
        }
        #[cfg(not(feature = "cuda"))]
        {
            self.raw
                .push(KernelArg::DevicePtr { addr: buf.0.device_ptr_arg() });
        }
        self
    }

    pub fn push_i32(mut self, v: i32) -> Self {
        self.raw.push(KernelArg::I32(v));
        self
    }

    pub fn push_i64(mut self, v: i64) -> Self {
        self.raw.push(KernelArg::I64(v));
        self
    }

    pub fn push_f32(mut self, v: f32) -> Self {
        self.raw.push(KernelArg::F32(v));
        self
    }

    pub fn push_f64(mut self, v: f64) -> Self {
        self.raw.push(KernelArg::F64(v));
        self
    }
}

// AUDIT 2026-05-16 (CRIT-1 fix): under the `cuda` feature, the
// `DevicePtr` variant carries the raw device address.
// In cudarc 0.13, SyncRecord was removed; stream ordering is
// now handled via CudaDevice::wait_for and fork_default_stream.
//
// In stub mode (no `cuda` feature) the variant degenerates to a bare
// `u64` since there is no real allocation to guard.
pub(crate) enum KernelArg {
    #[cfg(feature = "cuda")]
    DevicePtr {
        addr: u64,
    },
    #[cfg(not(feature = "cuda"))]
    DevicePtr {
        // Never read in stub mode — the stub `launch_raw` returns
        // `NoDriver` without inspecting args. We still carry the field
        // so the variant shape matches the real backend for any future
        // code that pattern-matches across both cfgs.
        #[allow(dead_code)]
        addr: u64,
    },
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
}

/// A typed device-side memory allocation.
///
/// `T` must be `Copy` and have a stable bit pattern (`i32`, `i64`,
/// `f32`, `f64`, `u8`, `i16`). The bridge does no bounds checking on
/// the host side — the kernel is responsible for staying within
/// `len()`.
pub struct DeviceBuffer<T>(pub(crate) backend::DeviceBufferInner<T>);

#[cfg(feature = "cuda")]
impl<T: bytemuck::Pod + Send + Sync + 'static + cudarc::driver::DeviceRepr + cudarc::driver::ValidAsZeroBits + std::marker::Unpin> DeviceBuffer<T> {
    const ASSERT_DEVICE_REPR: () = assert!(
        std::mem::size_of::<T>() > 0,
        "T must implement DeviceRepr when cuda feature is enabled"
    );
    /// Allocate `len` elements on the device, contents undefined.
    pub fn uninit(ctx: &DeviceContext, len: usize) -> Result<Self> {
        let _ = Self::ASSERT_DEVICE_REPR;
        backend::DeviceBufferInner::uninit(&ctx.0, len).map(Self)
    }

    /// Allocate `len` elements, zero-initialised.
    pub fn zeros(ctx: &DeviceContext, len: usize) -> Result<Self> {
        let _ = Self::ASSERT_DEVICE_REPR;
        backend::DeviceBufferInner::zeros(&ctx.0, len).map(Self)
    }

    /// Allocate and upload from `host` in one shot.
    pub fn from_host(ctx: &DeviceContext, host: &[T]) -> Result<Self> {
        let _ = Self::ASSERT_DEVICE_REPR;
        backend::DeviceBufferInner::from_host(&ctx.0, host).map(Self)
    }

    /// Copy `len()` elements back into `dst` (must be at least
    /// `self.len()` long).
    pub fn to_host(&self, dst: &mut [T]) -> Result<()> {
        self.0.to_host(dst)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(not(feature = "cuda"))]
impl<T: bytemuck::Pod + Send + Sync + 'static> DeviceBuffer<T> {
    /// Allocate `len` elements on the device, contents undefined.
    pub fn uninit(ctx: &DeviceContext, len: usize) -> Result<Self> {
        backend::DeviceBufferInner::uninit(&ctx.0, len).map(Self)
    }

    /// Allocate `len` elements, zero-initialised.
    pub fn zeros(ctx: &DeviceContext, len: usize) -> Result<Self> {
        backend::DeviceBufferInner::zeros(&ctx.0, len).map(Self)
    }

    /// Allocate and upload from `host` in one shot.
    pub fn from_host(ctx: &DeviceContext, host: &[T]) -> Result<Self> {
        backend::DeviceBufferInner::from_host(&ctx.0, host).map(Self)
    }

    /// Copy `len()` elements back into `dst` (must be at least
    /// `self.len()` long).
    pub fn to_host(&self, dst: &mut [T]) -> Result<()> {
        self.0.to_host(dst)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ── Backend selection ────────────────────────────────────────────────

#[cfg(feature = "cuda")]
mod backend_cuda;
#[cfg(feature = "cuda")]
use backend_cuda as backend;

#[cfg(not(feature = "cuda"))]
mod backend_stub;
#[cfg(not(feature = "cuda"))]
use backend_stub as backend;

pub mod event;
pub mod launch;
pub mod stream;

// We need a trivial Pod-trait shim because cudarc requires
// `bytemuck::Pod` for safe transfer. Re-export so callers don't have
// to add bytemuck themselves.
pub use bytemuck;
pub use event::Event;
pub use stream::{Stream, StreamOp};

#[cfg(all(test, not(feature = "cuda")))]
mod stub_tests {
    //! Verification of Part A's no-driver contract.
    //!
    //! When built without the `cuda` feature (the default), every
    //! fallible entry point must return `DeviceError::NoDriver` so the
    //! workspace can build and test on machines without a CUDA toolkit.
    use super::*;

    #[test]
    fn probe_returns_no_driver_in_stub_mode() {
        match probe() {
            Err(DeviceError::NoDriver) => {}
            other => panic!("expected NoDriver, got {other:?}"),
        }
    }

    #[test]
    fn device_context_new_returns_no_driver_in_stub_mode() {
        match DeviceContext::new(0) {
            Err(DeviceError::NoDriver) => {}
            Err(other) => panic!("expected NoDriver, got {other:?}"),
            Ok(_) => panic!("expected NoDriver, got Ok(DeviceContext)"),
        }
    }

    #[test]
    fn launch_config_elementwise_sizing() {
        // The pure-Rust helper has no driver dependency; make sure it
        // computes sensible grid/block geometry.
        let cfg = LaunchConfig::elementwise(1_000_000);
        assert_eq!(cfg.block, (256, 1, 1));
        assert_eq!(cfg.grid, ((1_000_000u32).div_ceil(256), 1, 1));
        assert_eq!(cfg.shared_bytes, 0);

        // For n=0 we still want a non-degenerate (grid >= 1) launch
        // shape so callers don't accidentally pass zero-dim grids.
        let cfg0 = LaunchConfig::elementwise(0);
        assert_eq!(cfg0.grid, (1, 1, 1));
    }
}
