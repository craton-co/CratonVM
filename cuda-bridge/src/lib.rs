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

// AUDIT 2026-05-16: `#[non_exhaustive]` — variants (e.g. `OutOfMemory`,
// `InvalidLayout`, `StreamSync`)Z may be added as the cudarc backend
// matures; we don't want a downstream `match` to break.
#[derive(Debug, Error)]
#[non_exhaustive]
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
///
/// AUDIT 2026-05-16: `#[non_exhaustive]` — fields may be added (e.g.
/// `pci_bus_id`, `multi_processor_count`, `clock_rate_khz`). Construct
/// via this crate's APIs rather than struct literals.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
    ///
    /// 256 is a portable "good enough" pick — it divides cleanly into the
    /// warp size (32) on every CUDA arch and fits in shared-memory budgets
    /// up to Hopper. For per-kernel autotune (which can beat 256 on
    /// register-pressure or smem-bound kernels) call
    /// [`LaunchConfig::elementwise_for_kernel`] with the resolved
    /// `DeviceModule` and kernel name.
    pub fn elementwise(n: u32) -> Self {
        Self::elementwise_with_block(n, 256)
    }

    /// One-dimensional launch sized to cover `n` elements with the given
    /// `block` size (rounded down to a positive value if zero is passed).
    ///
    /// Shared between [`Self::elementwise`] and the kernel-aware
    /// [`Self::elementwise_for_kernel`] path so they agree on grid sizing.
    pub fn elementwise_with_block(n: u32, block: u32) -> Self {
        let block = block.max(1);
        let grid = n.div_ceil(block).max(1);
        Self {
            grid: (grid, 1, 1),
            block: (block, 1, 1),
            shared_bytes: 0,
        }
    }

    /// Like [`Self::elementwise`] but queries the driver's
    /// `cuOccupancyMaxPotentialBlockSize` (via cudarc) for the named
    /// kernel and uses the returned block size. Falls back to 256 if the
    /// query fails or the bridge was built without the `cuda` feature.
    ///
    /// Round-8 fix for the "hardcoded block=256" TODO. The autotune cost
    /// is a single driver call at launch site; for kernels launched in
    /// tight loops, hoist this above the loop and reuse the returned
    /// `LaunchConfig`.
    pub fn elementwise_for_kernel(
        module: &DeviceModule,
        ctx: &DeviceContext,
        kernel: &str,
        n: u32,
    ) -> Self {
        let block = module.0.optimal_block_size(&ctx.0, kernel).unwrap_or(256);
        Self::elementwise_with_block(n, block)
    }
}

/// Probe the system for an attached CUDA device.
///
/// Returns `Err(DeviceError::NoDriver)` on machines without a driver
/// or when the crate was built without the `cuda` feature.
pub fn probe() -> Result<DeviceCaps> {
    backend::probe()
}

/// Number of CUDA-capable devices visible to the driver.
///
/// Returns `Ok(0)` when no driver is loaded or no GPU is attached, and
/// `Err(DeviceError::Driver)` only for genuine driver errors (e.g.
/// version mismatch). Stub builds always return `Ok(0)` so callers
/// can branch on the count without special-casing the no-driver case.
///
/// Round-10 multi-GPU enumeration entry point. The current offload
/// pipeline binds to `gpu_device_ordinal` (a single device) — this
/// helper lets a future scheduler enumerate over `0..device_count()`
/// to pick the least-loaded ordinal at startup.
pub fn device_count() -> Result<u32> {
    backend::device_count()
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

    /// Synchronize: wait for all in-flight work on this context to drain.
    pub fn synchronize(&self) -> Result<()> {
        self.0.synchronize()
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
    ///
    /// **Sync default.** This entry point records a post-launch event on
    /// the compute stream and makes the D→H copy stream wait on it, so a
    /// subsequent [`DeviceBuffer::to_host`] is correctly stream-ordered
    /// behind the kernel. Callers that know no `to_host` follows (e.g.
    /// fire-and-forget kernels, or back-to-back launches on the compute
    /// stream where the next launch already orders behind this one) should
    /// prefer [`Self::launch_raw_no_sync`] to skip the event-pool
    /// bookkeeping (~3µs of CPU-side driver overhead per launch).
    pub fn launch_raw(
        &self,
        ctx: &DeviceContext,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.0.launch_raw(&ctx.0, kernel, cfg, args)
    }

    /// Launch a kernel without recording the post-launch D→H sync event.
    ///
    /// Use this when the caller knows no [`DeviceBuffer::to_host`] reads
    /// the result of this launch. Skipping the event-record/wait pair
    /// saves a cudarc per-context event-pool allocation per launch
    /// (~3µs CPU-side), which is measurable on tight back-to-back
    /// microkernel loops.
    ///
    /// **Safety contract (logical, not memory).** If a `to_host` call on
    /// any buffer this kernel wrote runs after a launch made through
    /// this entry point — without an intervening [`Self::launch_raw`] or
    /// [`DeviceContext::synchronize`] — the host may read stale bytes.
    /// The compute stream still serialises back-to-back launches on
    /// itself, so the only failure mode is host read-back.
    pub fn launch_raw_no_sync(
        &self,
        ctx: &DeviceContext,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.0.launch_raw_no_d2h_sync(&ctx.0, kernel, cfg, args)
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
pub struct DeviceBuffer<T>(backend::DeviceBufferInner<T>);

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

// We need a trivial Pod-trait shim because cudarc requires
// `bytemuck::Pod` for safe transfer. Re-export so callers don't have
// to add bytemuck themselves.
pub use bytemuck;

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
    fn device_count_returns_zero_in_stub_mode() {
        // Round-10 multi-GPU helper: in no-driver mode we always
        // report zero devices so callers can branch on the count
        // without a NoDriver special-case.
        assert_eq!(device_count().unwrap(), 0);
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
