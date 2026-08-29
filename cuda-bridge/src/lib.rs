// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

#![deny(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::undocumented_unsafe_blocks
)]

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

/// Default 1-D block size used by [`LaunchConfig::elementwise`] when no
/// occupancy autotune is available (stub mode, or when the driver query
/// returns nothing). Kept as a named constant so the autotune fallback
/// in [`DeviceModule::elementwise_for_kernel`] uses the same value.
pub(crate) const DEFAULT_ELEMENTWISE_BLOCK: u32 = 256;

impl LaunchConfig {
    /// One-dimensional launch sized to cover `n` elements with the
    /// default block size of 256 threads.
    ///
    /// For an occupancy-tuned block size, use
    /// [`DeviceModule::elementwise_for_kernel`], which queries the driver
    /// for the kernel's optimal block size and falls back to this default.
    pub fn elementwise(n: u32) -> Self {
        Self::elementwise_with_block(n, DEFAULT_ELEMENTWISE_BLOCK)
    }

    /// One-dimensional launch sized to cover `n` elements with an
    /// explicit `block` size. The grid is `ceil(n / block)` (at least
    /// one block). `block` is clamped to at least 1 so a degenerate
    /// `0` never produces a div-by-zero or a zero-thread launch.
    /// Public so a caller that has already paid for the occupancy
    /// query once can rebuild the config for a different element count
    /// without paying for it again — the block size a kernel wants does
    /// not depend on how many elements a particular launch covers.
    pub fn elementwise_with_block(n: u32, block: u32) -> Self {
        let block = block.max(1);
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
/// Shorthand for [`probe_device(0)`](probe_device).
pub fn probe() -> Result<DeviceCaps> {
    probe_device(0)
}

/// Probe a specific device ordinal.
///
/// [`probe`] always described device 0, which was fine while its only
/// caller was `--gpu-info`, but the offload cache needs the compute
/// capability of the device it is actually going to launch on: that is
/// what picks the `sm_XX` its kernels are lowered for. Reporting device
/// 0's capability for a run pinned to `--gpu-device 1` would silently
/// target the wrong architecture.
pub fn probe_device(device_ordinal: u32) -> Result<DeviceCaps> {
    backend::probe_device(device_ordinal)
}

/// A CUDA context bound to one device. Cheap to clone; the underlying
/// driver handle is shared via Arc.
#[derive(Clone)]
pub struct DeviceContext(backend::DeviceContextInner);

// cudarc 0.13's `CudaStream` does not impl `Send`/`Sync` because it
// holds a raw `sys::CUstream` (a `*mut CUstream_st`). The CUDA driver
// docs explicitly permit using a stream from any thread that has
// initialized the context, so the raw pointer is logically thread-safe;
// cudarc itself impls `Send`/`Sync` for `CudaDevice`, `CudaModule`, and
// `CudaFunction` on exactly the same grounds, and the missing impls on
// `CudaStream` are an upstream oversight (filed: `coreylowman/cudarc#318`).
//
// The `cuda`-backed `DeviceContextInner` owns four `Arc<CudaStream>`s,
// so it inherits the missing impls. `stream.rs`'s `Stream` wrapper and
// `event.rs`'s `EventCuda` already carry the identical `unsafe impl`
// pair for the same reason; this asserts the same safety condition for
// `DeviceContext` so downstream consumers (notably the VM's
// `OffloadCache`, which is reachable from the `Send + Sync` `SharedVm`)
// can store it across threads.
//
// In stub mode `DeviceContextInner` is a unit struct and is trivially
// `Send + Sync`; these impls are harmless there.
// SAFETY: every driver-backed operation binds the retained owning context on
// the current thread, and stub mode contains no raw handle.
unsafe impl Send for DeviceContext {}
// SAFETY: the identical driver/context-binding argument above also permits
// shared references to be used from bound threads.
unsafe impl Sync for DeviceContext {}

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
    /// it publicly. Stream/event constructors pull the
    /// `Arc<CudaDevice>` from here via `DeviceContextInner::device()`.
    #[allow(dead_code)]
    pub(crate) fn inner(&self) -> &backend::DeviceContextInner {
        &self.0
    }

    /// AUDIT 2026-05-29 (H10b fix): crate-internal constructor wrapping
    /// an existing backend context. Used by `backend_cuda`'s
    /// `launch_raw_inner` to build a transient `Event` (which needs a
    /// `&DeviceContext`) without re-attaching the driver context — the
    /// backend `DeviceContextInner` is `Clone` (an Arc bump).
    #[allow(dead_code)]
    pub(crate) fn from_inner(inner: backend::DeviceContextInner) -> Self {
        Self(inner)
    }

    /// Stub-only test constructor: build a synthetic `DeviceContext`
    /// without going through the driver. Used by the in-crate stub-mode
    /// event-ordering integration tests in `launch.rs`; exposed to the
    /// rest of the crate so sibling test modules can drive
    /// `from_host_async` / `launch_on_stream` / `to_host_async` against
    /// the stub op log without depending on a real `probe()`.
    #[cfg(all(test, not(feature = "cuda")))]
    pub(crate) fn for_test() -> Self {
        // Stub `DeviceContextInner` is a unit struct — costs nothing
        // to construct.
        Self(backend::DeviceContextInner)
    }
}

/// Hard cap on the number of distinct kernel names the process-wide
/// interner will ever leak. Real workloads compile a small, finite set
/// of kernel identifiers (tens to low hundreds), so this bound is far
/// above any legitimate need while capping the worst case at a fixed,
/// bounded amount of leaked memory (≈ this many short boxed strings).
#[cfg(feature = "cuda")]
const MAX_INTERNED_KERNEL_NAMES: usize = 4096;

/// `'static` sentinel returned once the interner is saturated. It is a
/// deliberately invalid kernel name: any `get_func` lookup against it
/// fails with [`DeviceError::KernelNotFound`] (a clean, surfaced error)
/// rather than silently growing the process heap without bound.
#[cfg(feature = "cuda")]
const INTERN_OVERFLOW_SENTINEL: &str = "__cratonvm_kernel_name_intern_overflow__";

/// Process-wide kernel-name interner.
///
/// AUDIT 2026-05-20 (PERF Fix #2): cudarc 0.13 needs `&'static str`
/// kernel names. Rather than `Box::leak`-ing on every `from_ptx` call
/// (which leaks unboundedly when callers load modules with generated /
/// unique kernel names in a loop), each distinct name is leaked at most
/// once and cached here. Subsequent loads of the same name return the
/// already-interned `'static` slot — zero new allocation, zero leak.
///
/// AUDIT 2026-05-29 (FINDING 5 — bound the leak): the previous "the set
/// only grows by distinct name, so it's bounded" reasoning held only for
/// trusted callers. A caller that loads modules with attacker- or
/// codegen-controlled *unique* kernel names in a loop could leak one
/// boxed string per name without limit — an unbounded-growth DoS vector.
/// The interner now caps the number of distinct names it will leak at
/// [`MAX_INTERNED_KERNEL_NAMES`]. Past the cap it logs once and returns a
/// shared `'static` overflow sentinel ([`INTERN_OVERFLOW_SENTINEL`])
/// instead of leaking further; the subsequent kernel lookup fails
/// cleanly with `KernelNotFound` rather than corrupting state or growing
/// the heap. The cap is far above any legitimate kernel-name count.
#[cfg(feature = "cuda")]
fn intern_kernel_name(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::sync::OnceLock;

    static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    static WARNED: AtomicBool = AtomicBool::new(false);
    let set = INTERNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = set.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(&existing) = guard.get(name) {
        return existing;
    }
    // First sighting of this name. Refuse to leak past the cap so a
    // caller feeding unbounded distinct names cannot grow the heap
    // without limit.
    if guard.len() >= MAX_INTERNED_KERNEL_NAMES {
        // Log exactly once to avoid spamming on a hot mis-use path.
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "cuda-bridge: kernel-name interner saturated at {MAX_INTERNED_KERNEL_NAMES} \
                 distinct names; further names are not interned and their loads will fail \
                 with KernelNotFound (possible unbounded-name misuse)"
            );
        }
        return INTERN_OVERFLOW_SENTINEL;
    }
    // Leak exactly one boxed string and record the `'static` reference
    // so future calls reuse it.
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// Mint a process-unique PTX module name.
///
/// cudarc registers each loaded module in a per-device/per-context map
/// keyed by the name handed to `load_ptx`, and resolves kernels by that
/// same key. A constant name therefore makes every `DeviceModule` alias
/// the same map slot, so a later load silently overwrites an earlier one.
/// A monotonic counter guarantees every load gets its own slot; `u64`
/// never wraps in practice (one load per nanosecond for ~585 years), so a
/// plain `fetch_add` with no overflow handling is sufficient.
fn next_module_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cratonvm_mod_{n}")
}

/// A loaded PTX module containing one or more named kernel entry points.
pub struct DeviceModule(backend::DeviceModuleInner);

impl DeviceModule {
    /// Load a PTX text module and resolve the named kernels.
    ///
    /// AUDIT 2026-06-21 (FINDING — unique module name): the module name
    /// previously passed to the backend was the constant `"module"`.
    /// cudarc keys its per-device/per-context module map by that name, so a
    /// second `DeviceModule` loaded with the same constant clobbered the
    /// first — kernels in module B would resolve against module A's PTX (or
    /// fail outright). Each load now mints a process-unique name via a
    /// monotonic counter ([`next_module_name`]); the backend retains it in
    /// `DeviceModuleInner::module_name` so subsequent `get_func` lookups
    /// stay consistent and multiple modules coexist.
    pub fn from_ptx(ctx: &DeviceContext, ptx: &str, kernel_names: &[&str]) -> Result<Self> {
        let module_name = next_module_name();
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
            backend::DeviceModuleInner::from_ptx(&ctx.0, ptx, &module_name, &static_names).map(Self)
        }
        #[cfg(not(feature = "cuda"))]
        {
            backend::DeviceModuleInner::from_ptx(&ctx.0, ptx, &module_name, kernel_names).map(Self)
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

    /// Build a 1-D elementwise [`LaunchConfig`] for `kernel` sized to
    /// cover `n` elements, using the kernel's occupancy-optimal block
    /// size when the driver can report it.
    ///
    /// Round-8 autotune: replaces the hardcoded block size of
    /// [`LaunchConfig::elementwise`] with a per-kernel value queried via
    /// `cuOccupancyMaxPotentialBlockSize` (wrapped by
    /// `DeviceModuleInner::optimal_block_size`). If the query is
    /// unavailable — stub mode has no driver, and the cuda path falls
    /// back when the driver returns nothing — the default block size of
    /// [`crate::DEFAULT_ELEMENTWISE_BLOCK`] (256) is used, so the result
    /// is identical to `LaunchConfig::elementwise(n)` in that case.
    pub fn elementwise_for_kernel(
        &self,
        ctx: &DeviceContext,
        kernel: &str,
        n: u32,
    ) -> LaunchConfig {
        #[cfg(feature = "cuda")]
        let block = self
            .0
            .optimal_block_size(&ctx.0, kernel)
            .unwrap_or(DEFAULT_ELEMENTWISE_BLOCK);
        #[cfg(not(feature = "cuda"))]
        let block = {
            // Stub mode: no driver to query, so the autotune degenerates
            // to the default block size. `ctx` / `kernel` are unused here
            // but kept on the signature for backend parity.
            let _ = (ctx, kernel);
            DEFAULT_ELEMENTWISE_BLOCK
        };
        LaunchConfig::elementwise_with_block(n, block)
    }

    /// Stub-only test constructor — see [`DeviceContext::for_test`].
    /// Builds a synthetic `DeviceModule` whose `launch_on_stream` only
    /// records OpLog ops (no driver call).
    #[cfg(all(test, not(feature = "cuda")))]
    pub(crate) fn for_test() -> Self {
        Self(backend::DeviceModuleInner)
    }
}

/// Builder for a CUDA kernel argument list.
///
/// Each kernel parameter slot in PTX is a fixed-size scalar (pointer,
/// `i32`, `i64`, `f32`, `f64`). This builder accumulates aligned bytes
/// in the order the kernel expects them.
#[derive(Default, Clone)]
pub struct KernelArgs {
    pub(crate) raw: Vec<KernelArg>,
}

impl KernelArgs {
    // (see with_tid_base below for the chunked-launch clone)
    pub fn new() -> Self {
        Self::default()
    }

    /// Clone these args with the trailing `tid_base` slot replaced.
    ///
    /// Every lowered kernel ends with a `.param .s32 tid_base` (see
    /// `jit_cuda::lowering::ptx_params`), and a chunked dispatch launches
    /// the SAME kernel several times over disjoint slices of one iteration
    /// space, differing only in that value. Cloning is cheap: the device
    /// pointer args carry an `Arc` keep-alive and a shared `last_write`
    /// slot, so a clone shares both rather than duplicating anything.
    ///
    /// Returns `None` if the last argument is not an `i32`, which would
    /// mean these args were not built for a `tid_base`-taking kernel.
    pub fn with_tid_base(&self, base: i32) -> Option<Self> {
        let mut cloned = self.clone();
        match cloned.raw.last_mut() {
            Some(KernelArg::I32(slot)) => {
                *slot = base;
                Some(cloned)
            }
            _ => None,
        }
    }

    pub fn push_device_ptr<T: Send + Sync + 'static>(mut self, buf: &DeviceBuffer<T>) -> Self {
        // AUDIT 2026-05-16 (CRIT-1 fix): plumb the device pointer
        // returned by `CudaSlice::device_ptr` into the `KernelArg`.
        // In cudarc 0.13, SyncRecord was removed; stream ordering is
        // now handled via CudaDevice::wait_for and fork_default_stream.
        //
        // AUDIT 2026-05-22 (UAF fix): `device_ptr_arg` now also returns
        // a type-erased keep-alive handle (a clone of the buffer's
        // `Arc<CudaSlice<T>>`). It is stored inside the
        // `KernelArg::DevicePtr` so the device allocation behind `addr`
        // is kept alive for as long as this `KernelArgs` lives — i.e.
        // until `launch_raw` consumes it. Previously only the bare
        // `u64` address was copied in, so dropping the originating
        // `DeviceBuffer` before the launch left the kernel reading
        // freed device memory (use-after-free).
        //
        // AUDIT 2026-05-24 (HIGH correctness — cross-stream ordering):
        // also clone the buffer's `last_write` slot into the
        // `KernelArg`. `launch_on_stream` (1) reads the slot to wait on
        // the buffer's last write before launching the kernel and
        // (2) overwrites it with the kernel-completion event so
        // subsequent `to_host_async` / re-launch calls wait on the
        // kernel rather than the upload.
        let last_write = buf.last_write.clone();
        #[cfg(feature = "cuda")]
        {
            let (addr, keep_alive) = buf.inner.device_ptr_arg();
            self.raw.push(KernelArg::DevicePtr {
                addr,
                _keep_alive: keep_alive,
                last_write,
            });
        }
        #[cfg(not(feature = "cuda"))]
        {
            self.raw.push(KernelArg::DevicePtr {
                addr: buf.inner.device_ptr_arg(),
                last_write,
            });
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
// AUDIT 2026-05-22 (UAF fix): the `cuda`-mode variant also carries a
// type-erased keep-alive handle (`backend::BufferKeepAlive`, an
// `Arc<dyn Any + Send + Sync>` cloned from the buffer's
// `Arc<CudaSlice<T>>`). It is never read — its sole job is to keep the
// device allocation behind `addr` alive for as long as this `KernelArg`
// (and the owning `KernelArgs`) exists, which spans the kernel launch.
// This makes "buffer dropped before launch" a compile-checked
// impossibility instead of a silent use-after-free.
//
// In stub mode (no `cuda` feature) the variant degenerates to a bare
// `u64` since there is no real allocation to guard.
#[derive(Clone)]
pub(crate) enum KernelArg {
    #[cfg(feature = "cuda")]
    DevicePtr {
        addr: u64,
        /// Keep-alive only; not read. See the variant comment above.
        #[allow(dead_code)]
        _keep_alive: backend::BufferKeepAlive,
        /// AUDIT 2026-05-24 (HIGH correctness): shared `last_write`
        /// slot from the originating `DeviceBuffer`. `launch_on_stream`
        /// reads this to wait on the buffer's last write before the
        /// launch and overwrites it with the kernel-completion event
        /// after the launch.
        last_write: LastWriteSlot,
    },
    #[cfg(not(feature = "cuda"))]
    DevicePtr {
        // Never read in stub mode — the stub `launch_raw` returns
        // `NoDriver` without inspecting args. We still carry the field
        // so the variant shape matches the real backend for any future
        // code that pattern-matches across both cfgs.
        #[allow(dead_code)]
        addr: u64,
        /// AUDIT 2026-05-24 (HIGH correctness): see cuda variant.
        /// Active in stub mode too because `launch_on_stream`'s
        /// event-ordering bookkeeping is what the stub-backend op-log
        /// tests assert on.
        last_write: LastWriteSlot,
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
///
/// AUDIT 2026-05-24 (HIGH correctness — cross-stream ordering): every
/// `DeviceBuffer<T>` now carries a `last_write` event slot. The slot is
/// initially empty; `from_host_async` installs an event recorded on the
/// upload stream, and `launch_on_stream` installs a kernel-completion
/// event recorded on the user stream for every device-pointer arg. The
/// slot is `Arc<Mutex<…>>` because:
///   * `Arc` lets `KernelArgs::push_device_ptr` share the *same* slot
///     with the buffer — `launch_on_stream` updates the slot through the
///     `KernelArg`, and the buffer's `to_host_async` reads it back.
///   * `Mutex` gives interior mutability through the `&self` API of

/// A page-locked ("pinned") host buffer, for chunked writeback staging.
///
/// An async device->host copy only overlaps with kernel execution when its
/// host destination is page-locked. Measured on this box, 11 MB out of an
/// RTX 2060: 12.95 GB/s into page-locked memory against 3.98 GB/s for the
/// async form into ordinary pageable memory. The Java heap arena is
/// pageable, so a chunked writeback has to land somewhere page-locked and
/// be memcpy'd on from there — the memcpy runs at ~26 GB/s and overlaps
/// with the GPU work still queued behind it.
///
/// `cuMemHostRegister` on the Java array itself was measured too and
/// rejected: registering is cheap (0.08 ms for 11 MB) but UNregistering
/// costs 0.69 ms, which is most of the win, and caching a registration
/// across calls would have to survive a moving collector.
///
/// Allocation is one `cuMemAllocHost` and the buffer is reused across
/// dispatches, so the cost is paid once per size class rather than per
/// call. In stub mode this is a plain heap `Vec` and nothing is pinned.
pub struct PinnedHostBuffer<T: Copy> {
    inner: backend::PinnedHostInner<T>,
}

impl<T: Copy + Default> PinnedHostBuffer<T> {
    /// Allocate `len` page-locked elements.
    pub fn new(ctx: &DeviceContext, len: usize) -> Result<Self> {
        backend::PinnedHostInner::new(ctx, len).map(|inner| Self { inner })
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The staging elements as a mutable slice.
    ///
    /// # Safety
    ///
    /// The caller must not read or write a range that a queued DMA still
    /// owns. Ordering is the caller's job — wait on the event recorded
    /// after the copy that filled the range.
    pub unsafe fn as_mut_slice(&self) -> &mut [T] {
        self.inner.as_mut_slice()
    }
}
///     `to_host_async` and the `&KernelArgs` flow of `launch_on_stream`.
pub struct DeviceBuffer<T> {
    pub(crate) inner: backend::DeviceBufferInner<T>,
    /// Per-buffer "last write" event slot. See type-level docs.
    pub(crate) last_write: LastWriteSlot,
    /// Owned host staging buffers that may still be read by queued H2D
    /// copies. The safe async upload path stores its private staging
    /// copy here so the caller's borrowed host slice can be dropped as
    /// soon as `from_host_async` returns.
    pub(crate) _host_uploads: Vec<std::sync::Arc<Vec<T>>>,
}

/// Shared handle to a buffer's last-write event. See [`DeviceBuffer`].
pub(crate) type LastWriteSlot = std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<Event>>>>;

/// Allocate a fresh empty [`LastWriteSlot`].
pub(crate) fn new_last_write_slot() -> LastWriteSlot {
    std::sync::Arc::new(std::sync::Mutex::new(None))
}

// ── Device-element bound ─────────────────────────────────────────────
//
// `DeviceBuffer<T>`'s allocation/transfer methods (`uninit`, `zeros`,
// `from_host`, `to_host`) need different `T` bounds depending on the
// build mode:
//
//   * `cuda`     — cudarc requires `T: DeviceRepr + ValidAsZeroBits +
//                  Unpin` (plus `bytemuck::Pod + Send + Sync + 'static`).
//   * stub       — no driver, so only `bytemuck::Pod + Send + Sync +
//                  'static` is meaningful.
//
// Downstream crates (the VM's `gpu_marshal` wrappers) are generic over
// `T` but cannot name cudarc's traits — `cudarc` is not, and must not
// become, a direct dependency of `cratonvm-vm`. `DeviceElem` is the
// single public bound that bundles whatever the active backend needs,
// so callers write `T: DeviceElem` and stay backend-agnostic.
//
// It is a sealed trait with a blanket impl: any `T` that satisfies the
// underlying per-mode bounds automatically implements `DeviceElem`, and
// no downstream crate can add its own impls.
mod device_elem_seal {
    pub trait Sealed {}
}

/// Marker bound for types that can back a [`DeviceBuffer`].
///
/// Bundles every per-backend trait requirement (`bytemuck::Pod`,
/// `Send`/`Sync`, `'static`, and — under the `cuda` feature — cudarc's
/// `DeviceRepr`/`ValidAsZeroBits`/`Unpin`) behind one name so generic
/// callers do not have to depend on `cudarc` directly. Implemented
/// automatically for every qualifying primitive (`i8`, `i32`, `i64`,
/// `f32`, `f64`, …); sealed so the set cannot be widened downstream.
#[cfg(feature = "cuda")]
pub trait DeviceElem:
    bytemuck::Pod
    + Send
    + Sync
    + 'static
    + cudarc::driver::DeviceRepr
    + cudarc::driver::ValidAsZeroBits
    + std::marker::Unpin
    + device_elem_seal::Sealed
{
}

/// Marker bound for types that can back a [`DeviceBuffer`].
///
/// See the `cuda`-feature variant for the full rationale. In stub mode
/// there is no driver, so the bound collapses to `bytemuck::Pod + Send +
/// Sync + 'static`.
#[cfg(not(feature = "cuda"))]
pub trait DeviceElem: bytemuck::Pod + Send + Sync + 'static + device_elem_seal::Sealed {}

#[cfg(feature = "cuda")]
impl<T> device_elem_seal::Sealed for T where
    T: bytemuck::Pod
        + Send
        + Sync
        + 'static
        + cudarc::driver::DeviceRepr
        + cudarc::driver::ValidAsZeroBits
        + std::marker::Unpin
{
}

#[cfg(feature = "cuda")]
impl<T> DeviceElem for T where
    T: bytemuck::Pod
        + Send
        + Sync
        + 'static
        + cudarc::driver::DeviceRepr
        + cudarc::driver::ValidAsZeroBits
        + std::marker::Unpin
{
}

#[cfg(not(feature = "cuda"))]
impl<T> device_elem_seal::Sealed for T where T: bytemuck::Pod + Send + Sync + 'static {}

#[cfg(not(feature = "cuda"))]
impl<T> DeviceElem for T where T: bytemuck::Pod + Send + Sync + 'static {}

// `DeviceBufferInner<T>` (cuda backend) holds a `CudaSlice<T>` — which
// cudarc *does* mark `Send`/`Sync` for `T: Send`/`T: Sync` — plus a set
// of `Arc<CudaStream>` retained for stream-ordered `to_host`. The
// `CudaStream` fields are the only thing keeping the buffer off the
// `Send`/`Sync` auto-traits; the same `coreylowman/cudarc#318` reasoning
// used for `DeviceContext` (above) and `Stream` applies. We mirror
// `CudaSlice`'s own conditional bounds (`T: Send` / `T: Sync`) so the
// buffer is exactly as thread-safe as its payload type.
//
// In stub mode `DeviceBufferInner<T>` is just a `PhantomData<T>`, so the
// conditional impls reduce to the auto-trait behaviour anyway.
// SAFETY: CudaSlice already permits transfer when T: Send; retained streams
// stay live and are driven only after binding their context.
unsafe impl<T: Send> Send for DeviceBuffer<T> {}
// SAFETY: shared access never mutates host `T`; CUDA ordering is mediated by
// driver streams/events, so Sync follows exactly when `T: Sync`.
unsafe impl<T: Sync> Sync for DeviceBuffer<T> {}

#[cfg(feature = "cuda")]
impl<T: DeviceElem> DeviceBuffer<T> {
    // Reject zero-sized types: a `DeviceBuffer<T>` with `size_of::<T>()
    // == 0` would compute a zero-byte allocation and zero-length copies,
    // which the device-transfer paths are not meant to handle. (The
    // `DeviceRepr` bound itself is enforced by the `DeviceElem` trait
    // bound on this impl, not by this assert.)
    const ASSERT_DEVICE_REPR: () = assert!(
        std::mem::size_of::<T>() > 0,
        "DeviceBuffer<T> rejects zero-sized types (size_of::<T>() must be > 0)"
    );
    /// Allocate `len` elements on the device, contents undefined.
    pub fn uninit(ctx: &DeviceContext, len: usize) -> Result<Self> {
        let _ = Self::ASSERT_DEVICE_REPR;
        backend::DeviceBufferInner::uninit(&ctx.0, len).map(|inner| Self {
            inner,
            last_write: new_last_write_slot(),
            _host_uploads: Vec::new(),
        })
    }

    /// Allocate `len` elements, zero-initialised.
    pub fn zeros(ctx: &DeviceContext, len: usize) -> Result<Self> {
        let _ = Self::ASSERT_DEVICE_REPR;
        backend::DeviceBufferInner::zeros(&ctx.0, len).map(|inner| Self {
            inner,
            last_write: new_last_write_slot(),
            _host_uploads: Vec::new(),
        })
    }

    /// Allocate and upload from `host` in one shot.
    pub fn from_host(ctx: &DeviceContext, host: &[T]) -> Result<Self> {
        let _ = Self::ASSERT_DEVICE_REPR;
        backend::DeviceBufferInner::from_host(&ctx.0, host).map(|inner| Self {
            inner,
            last_write: new_last_write_slot(),
            _host_uploads: Vec::new(),
        })
    }

    /// Allocate and upload from `host` ordered against `stream`.
    ///
    /// Mirrors [`DeviceBuffer::from_host`] but submits the H→D copy
    /// against an explicit [`Stream`] (Phase 2). The transfer is
    /// recorded as a [`StreamOp::UploadAsync`] on `stream` so stub-mode
    /// callers can inspect the op log; in `cuda` mode `record_op` is a
    /// no-op (the driver owns the queue).
    ///
    /// The H→D copy is
    /// submitted on `stream.raw()` (the user stream) and the buffer's
    /// `last_write` event is recorded on that same stream. Previously
    /// the copy ran on the context's `copy_h2d` stream while the doc
    /// promised a user-stream sync was sufficient — a user-stream sync
    /// does not order a copy on a different stream, so a caller that
    /// dropped `host` after only `stream.synchronize()` left the driver
    /// DMA-reading freed host memory (host-buffer use-after-free).
    ///
    /// The recorded `last_write` event also lets a subsequent
    /// `launch_on_stream` order its kernel behind this upload (via
    /// `cuStreamWaitEvent` on the per-buffer event) without racing.
    ///
    /// Current safety contract: this safe wrapper owns a staging copy
    /// of `host` inside the returned buffer, so the caller may drop or
    /// reuse `host` immediately after return. Use
    /// [`DeviceBuffer::from_host_async_unchecked`] for borrowed-host
    /// non-blocking DMA.
    pub fn from_host_async(ctx: &DeviceContext, host: &[T], stream: &Stream) -> Result<Self> {
        let staging = std::sync::Arc::new(host.to_vec());
        // SAFETY: `staging` owns a stable heap allocation for the host
        // bytes. On success it is stored in the returned buffer; on
        // failure we synchronize before dropping it, and leak it if the
        // cleanup synchronize itself fails.
        match unsafe { Self::from_host_async_unchecked(ctx, staging.as_slice(), stream) } {
            Ok(mut buffer) => {
                buffer._host_uploads.push(staging);
                Ok(buffer)
            }
            Err(err) => {
                if let Err(sync_err) = stream.synchronize() {
                    std::mem::forget(staging);
                    return Err(DeviceError::Memcpy(format!(
                        "from_host_async failed ({err}); cleanup stream synchronize failed \
                         ({sync_err}); leaked host staging to keep any queued DMA valid"
                    )));
                }
                Err(err)
            }
        }
    }

    /// Borrowed-host async upload variant.
    ///
    /// Unlike [`DeviceBuffer::from_host_async`], this function does not
    /// copy `host` into owned staging memory.
    ///
    /// # Safety
    ///
    /// `host` must remain allocated at the same address and unmodified
    /// until `stream.synchronize()` (or an equivalent event wait known
    /// to observe this upload's completion) has returned. This
    /// requirement holds even if this function returns an error after
    /// submitting work to the stream.
    pub unsafe fn from_host_async_unchecked(
        ctx: &DeviceContext,
        host: &[T],
        stream: &Stream,
    ) -> Result<Self> {
        let _ = Self::ASSERT_DEVICE_REPR;
        // H10c: bind the context to this thread before driving any
        // CUDA handle. Cheap per-thread TLS check; required for the
        // `unsafe impl Send + Sync` blocks to be sound off the
        // constructing thread.
        ctx.0.bind_to_thread()?;
        // Allocate the per-buffer last_write event up front so we can
        // record it on the user stream right after the upload DMA.
        let event = std::sync::Arc::new(Event::new(ctx)?);
        let event_for_stamp = std::sync::Arc::clone(&event);
        // Submit the H→D copy AND record `last_write` on the USER
        // stream (see `from_host_async_unchecked` / `upload_on_stream`).
        // SAFETY: this function's caller upholds the documented host lifetime;
        // the event and stream handles are owned by live wrappers above.
        let inner = unsafe {
            backend::DeviceBufferInner::from_host_async_unchecked(
                &ctx.0,
                host,
                stream.raw(),
                event.cu_event_raw(),
            )?
        };
        // `upload_on_stream` recorded `event` on exactly `stream`, but
        // it took the raw handle rather than the wrapper, so stamp the
        // bookkeeping here. Without it every later launch on this same
        // stream issues a `cuStreamWaitEvent` for an ordering the stream
        // already provides -- see `EventCuda::recorded_on`.
        #[cfg(feature = "cuda")]
        event_for_stamp.set_recorded_on(stream.raw());
        // Surface the upload on the user `stream`'s op log for callers
        // that introspect the queue. In cuda mode `record_op` is a
        // no-op so this collapses to nothing.
        stream.record_op(StreamOp::UploadAsync {
            bytes: std::mem::size_of_val(host),
        });
        let last_write = new_last_write_slot();
        *last_write.lock().unwrap_or_else(|p| p.into_inner()) = Some(event);
        Ok(Self {
            inner,
            last_write,
            _host_uploads: Vec::new(),
        })
    }

    /// Copy `len()` elements back into `dst` (must be at least
    /// `self.len()` long).
    ///
    /// AUDIT 2026-05-29 (H10b fix): waits on the buffer's own
    /// `last_write` event (not the context-wide `e_k`) so the D→H copy
    /// is ordered behind THIS buffer's producing kernel / upload, even
    /// when concurrent pipelines launch on other buffers.
    /// Overwrite this buffer in place from `host`, keeping the device
    /// pointer.
    ///
    /// [`DeviceBuffer::from_host`] allocates. This does not, and that
    /// is the whole point: a captured CUDA graph bakes each argument
    /// pointer into its nodes, so the only way to hand a replay new
    /// input is to write through the pointer the graph already holds.
    /// Reallocating would leave the graph pointing at freed memory.
    ///
    /// Synchronous. The bytes are on the device when this returns, so
    /// `host` may be reused immediately and a replay submitted
    /// afterwards observes them. Because the host blocks, no event
    /// bookkeeping is needed to order a later kernel behind this
    /// write -- the write already happened.
    ///
    /// `host.len()` must equal [`DeviceBuffer::len`]. A short slice
    /// would leave the buffer half-updated, which is a wrong answer
    /// rather than a failure.
    pub fn copy_from_host(&self, host: &[T]) -> Result<()> {
        let _ = Self::ASSERT_DEVICE_REPR;
        self.inner.copy_from_host(host)
    }

    pub fn to_host(&self, dst: &mut [T]) -> Result<()> {
        // H10c: bind the context to this thread before driving CUDA.
        self.inner.bind_to_thread()?;
        // H10b: hold the buffer's last_write `Arc<Event>` for the whole
        // call so the raw CUevent handle handed to the backend stays
        // valid; pass its raw handle as the D→H wait dependency.
        let last_write = self.last_write_event();
        let wait = last_write.as_ref().map(|ev| ev.cu_event_raw());
        self.inner.to_host(dst, wait)
    }

    /// Copy `len()` elements back into `dst` ordered against `stream`.
    ///
    /// Mirrors [`DeviceBuffer::to_host`] but submits the D→H copy
    /// against an explicit [`Stream`] (Phase 2), recorded as a
    /// [`StreamOp::DownloadAsync`] on `stream`.
    ///
    /// AUDIT 2026-05-29 (H10b fix): waits on the buffer's OWN
    /// `last_write` event (recorded on the stream its producing kernel
    /// / upload actually ran on) before issuing the D→H copy, then
    /// submits a genuinely-async `cuMemcpyDtoHAsync` on `stream`. The
    /// previous code waited on the context-wide singleton `e_k`, which
    /// is clobbered by every launch on any buffer, so the copy could be
    /// released before its own producing kernel retired and read stale
    /// device memory. It also called the host-blocking synchronous
    /// `to_host`, which defeated the async contract.
    ///
    /// Current safety contract: this safe wrapper downloads into owned
    /// staging memory, synchronizes `stream`, then copies into `dst`.
    /// Use [`DeviceBuffer::to_host_async_unchecked`] for borrowed-host
    /// non-blocking DMA.
    pub fn to_host_async(&self, dst: &mut [T], stream: &Stream) -> Result<()> {
        if dst.len() != self.len() {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async length mismatch: dst.len()={}, slice.len()={}",
                dst.len(),
                self.len()
            )));
        }
        let mut staging = vec![<T as bytemuck::Zeroable>::zeroed(); dst.len()];
        // SAFETY: `staging` is owned by this function and remains alive
        // until after the stream synchronize below. If synchronization
        // fails, leak the staging buffer rather than invalidating a
        // destination that the driver might still own.
        if let Err(err) = unsafe { self.to_host_async_unchecked(staging.as_mut_slice(), stream) } {
            if let Err(sync_err) = stream.synchronize() {
                std::mem::forget(staging);
                return Err(DeviceError::Memcpy(format!(
                    "to_host_async failed ({err}); cleanup stream synchronize failed \
                     ({sync_err}); leaked host staging to keep any queued DMA valid"
                )));
            }
            return Err(err);
        }
        if let Err(sync_err) = stream.synchronize() {
            std::mem::forget(staging);
            return Err(sync_err);
        }
        dst.copy_from_slice(&staging);
        Ok(())
    }

    /// Borrowed-destination async download variant.
    ///
    /// Unlike [`DeviceBuffer::to_host_async`], this function returns
    /// without synchronizing `stream` and writes directly into `dst`.
    ///
    /// # Safety
    ///
    /// `dst` must remain allocated at the same address and unread by the
    /// CPU until `stream.synchronize()` (or an equivalent event wait
    /// known to observe this download's completion) has returned. This
    /// requirement holds even if this function returns an error after
    /// submitting work to the stream.
    pub unsafe fn to_host_async_unchecked(&self, dst: &mut [T], stream: &Stream) -> Result<()> {
        // H10c: bind the context to this thread before driving CUDA.
        self.inner.bind_to_thread()?;
        let bytes = std::mem::size_of_val(dst);
        // Hold the last_write `Arc<Event>` for the whole call so the
        // raw handle stays valid across the FFI calls below.
        let last_write = self.last_write_event();
        let wait = last_write.as_ref().map(|ev| ev.cu_event_raw());
        // Submit the wait + async D→H on the USER stream.
        self.inner.to_host_async_raw(dst, stream.raw(), wait)?;
        stream.record_op(StreamOp::DownloadAsync { bytes });
        Ok(())
    }

    /// Async device->host copy of a SUB-RANGE: `dst.len()` elements
    /// starting at element `offset` of this buffer.
    ///
    /// This is what makes a chunked writeback possible. A whole-buffer
    /// download cannot overlap with anything, because it can only be
    /// issued once the entire kernel has finished; splitting the launch
    /// into chunks and giving each chunk its own slice-sized copy lets
    /// chunk N's DMA run while chunk N+1's kernel is still executing.
    ///
    /// # Safety
    ///
    /// Same contract as [`to_host_async_unchecked`](Self::to_host_async_unchecked):
    /// `dst` must stay allocated at the same address and unread by the CPU
    /// until a synchronize or event wait has observed this download. For a
    /// real overlap `dst` should be page-locked (see [`PinnedHostBuffer`]) —
    /// an async copy into ordinary pageable memory is staged by the driver
    /// and does not overlap.
    pub unsafe fn to_host_async_range_unchecked(
        &self,
        dst: &mut [T],
        offset: usize,
        stream: &Stream,
    ) -> Result<()> {
        self.inner.bind_to_thread()?;
        let bytes = std::mem::size_of_val(dst);
        let last_write = self.last_write_event();
        let wait = last_write.as_ref().map(|ev| ev.cu_event_raw());
        self.inner
            .to_host_async_range_raw(dst, offset, stream.raw(), wait)?;
        stream.record_op(StreamOp::DownloadAsync { bytes });
        Ok(())
    }

    /// Snapshot the buffer's current `last_write` event, if any.
    ///
    /// Returns a clone of the `Arc<Event>` so the caller can keep the
    /// underlying `CUevent` alive across FFI calls (the raw handle is
    /// only valid while at least one `Arc<Event>` clone lives).
    fn last_write_event(&self) -> Option<std::sync::Arc<Event>> {
        self.last_write
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(not(feature = "cuda"))]
impl<T: DeviceElem> DeviceBuffer<T> {
    /// Allocate `len` elements on the device, contents undefined.
    pub fn uninit(ctx: &DeviceContext, len: usize) -> Result<Self> {
        backend::DeviceBufferInner::uninit(&ctx.0, len).map(|inner| Self {
            inner,
            last_write: new_last_write_slot(),
            _host_uploads: Vec::new(),
        })
    }

    /// Allocate `len` elements, zero-initialised.
    pub fn zeros(ctx: &DeviceContext, len: usize) -> Result<Self> {
        backend::DeviceBufferInner::zeros(&ctx.0, len).map(|inner| Self {
            inner,
            last_write: new_last_write_slot(),
            _host_uploads: Vec::new(),
        })
    }

    /// Allocate and upload from `host` in one shot.
    pub fn from_host(ctx: &DeviceContext, host: &[T]) -> Result<Self> {
        backend::DeviceBufferInner::from_host(&ctx.0, host).map(|inner| Self {
            inner,
            last_write: new_last_write_slot(),
            _host_uploads: Vec::new(),
        })
    }

    /// Allocate and upload from `host` ordered against `stream`.
    ///
    /// Records the H→D copy as a [`StreamOp::UploadAsync`] on `stream`.
    ///
    /// CONTRACT (AUDIT 2026-05-24, HIGH correctness): the host slice
    /// `host` must outlive `stream`'s next synchronization point at
    /// which this upload (and any kernel that consumes the resulting
    /// buffer) completes — i.e. `stream.synchronize()` after the
    /// upload. Concretely: the buffer carries a `last_write` event
    /// recorded on `stream` right after the upload; `host` must remain
    /// valid until that event has fired, which `stream.synchronize()`
    /// guarantees. (In the cuda backend the H→D copy is currently a
    /// synchronous `htod_sync_copy`, so the slice is borrowed only for
    /// the duration of this call — the broader contract is what
    /// downstream async refactors must keep honouring.)
    ///
    /// AUDIT 2026-05-24 (HIGH correctness): also records a synthetic
    /// "upload complete" event on `stream` and stashes it in the
    /// buffer's `last_write` slot. The stub op log surfaces the event
    /// id so the integration tests can assert that a subsequent
    /// `launch_on_stream` waits on this very event before launching.
    /// In stub mode the underlying allocation returns
    /// `Err(DeviceError::NoDriver)` so the event-record and op
    /// bookkeeping are performed *before* the allocation attempt; the
    /// op log thus faithfully reflects the submitted work even when
    /// the no-driver allocation immediately errors.
    pub fn from_host_async(ctx: &DeviceContext, host: &[T], stream: &Stream) -> Result<Self> {
        // SAFETY: stub mode never submits DMA to a real driver, so the
        // borrowed-host lifetime contract is vacuous here.
        unsafe { Self::from_host_async_unchecked(ctx, host, stream) }
    }

    /// Stub-mode counterpart of the borrowed-host async upload API.
    ///
    /// # Safety
    ///
    /// In real cuda builds the host slice must outlive the queued DMA;
    /// in stub mode no DMA is submitted.
    pub unsafe fn from_host_async_unchecked(
        ctx: &DeviceContext,
        host: &[T],
        stream: &Stream,
    ) -> Result<Self> {
        // Allocate the last_write event up front. `Event::new` in stub
        // mode never fails (it allocates a Mutex + atomic id) so this
        // is allowed to succeed even on the no-driver path.
        let event = std::sync::Arc::new(Event::new(ctx)?);
        stream.record_event(&event)?;
        stream.record_op(StreamOp::UploadAsync {
            bytes: std::mem::size_of_val(host),
        });
        let inner = backend::DeviceBufferInner::from_host(&ctx.0, host)?;
        let last_write = new_last_write_slot();
        *last_write.lock().unwrap_or_else(|p| p.into_inner()) = Some(event);
        Ok(Self {
            inner,
            last_write,
            _host_uploads: Vec::new(),
        })
    }

    /// Overwrite this buffer in place from `host`, keeping the device
    /// pointer.
    ///
    /// [`DeviceBuffer::from_host`] allocates. This does not, and that
    /// is the whole point: a captured CUDA graph bakes each argument
    /// pointer into its nodes, so the only way to hand a replay new
    /// input is to write through the pointer the graph already holds.
    /// Reallocating would leave the graph pointing at freed memory.
    ///
    /// Synchronous. The bytes are on the device when this returns, so
    /// `host` may be reused immediately and a replay submitted
    /// afterwards observes them. Because the host blocks, no event
    /// bookkeeping is needed to order a later kernel behind this
    /// write -- the write already happened.
    ///
    /// `host.len()` must equal [`DeviceBuffer::len`]. A short slice
    /// would leave the buffer half-updated, which is a wrong answer
    /// rather than a failure.
    pub fn copy_from_host(&self, host: &[T]) -> Result<()> {
        self.inner.copy_from_host(host)
    }

    /// Copy `len()` elements back into `dst` (must be at least
    /// `self.len()` long).
    pub fn to_host(&self, dst: &mut [T]) -> Result<()> {
        self.inner.to_host(dst)
    }

    /// Copy `len()` elements back into `dst` ordered against `stream`.
    ///
    /// Mirrors [`DeviceBuffer::to_host`] but records the D→H copy as a
    /// [`StreamOp::DownloadAsync`] on `stream`.
    ///
    /// AUDIT 2026-05-24 (HIGH correctness): waits on the buffer's
    /// `last_write` event (recorded by `from_host_async` or the
    /// most recent `launch_on_stream`) before issuing the D→H copy.
    /// The op log thus shows `EventWait { event_id: <last_write> }`
    /// immediately before `DownloadAsync`.
    pub fn to_host_async(&self, dst: &mut [T], stream: &Stream) -> Result<()> {
        // SAFETY: stub mode never submits DMA to a real driver, so the
        // borrowed-destination lifetime contract is vacuous here.
        unsafe { self.to_host_async_unchecked(dst, stream) }
    }

    /// Stub-mode counterpart of the borrowed-destination async
    /// download API.
    ///
    /// # Safety
    ///
    /// In real cuda builds the destination slice must outlive the
    /// queued DMA; in stub mode no DMA is submitted.
    pub unsafe fn to_host_async_unchecked(&self, dst: &mut [T], stream: &Stream) -> Result<()> {
        // Wait on last_write BEFORE the download.
        if let Some(ev) = self
            .last_write
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .cloned()
        {
            stream.wait_event(&ev)?;
        }
        stream.record_op(StreamOp::DownloadAsync {
            bytes: std::mem::size_of_val(dst),
        });
        self.inner.to_host(dst)
    }

    /// Stub-mode counterpart of the chunked-writeback download.
    ///
    /// # Safety
    ///
    /// In real cuda builds the destination slice must outlive the queued
    /// DMA; in stub mode no DMA is submitted.
    pub unsafe fn to_host_async_range_unchecked(
        &self,
        dst: &mut [T],
        offset: usize,
        stream: &Stream,
    ) -> Result<()> {
        let end = offset.checked_add(dst.len()).ok_or_else(|| {
            DeviceError::Memcpy("to_host_async_range: offset + len overflows".into())
        })?;
        if end > self.len() {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async_range out of bounds: offset={offset} len={} buffer={}",
                dst.len(),
                self.len()
            )));
        }
        if let Some(ev) = self
            .last_write
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .cloned()
        {
            stream.wait_event(&ev)?;
        }
        stream.record_op(StreamOp::DownloadAsync {
            bytes: std::mem::size_of_val(dst),
        });
        self.inner.to_host_range(dst, offset)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(all(test, not(feature = "cuda")))]
impl<T: DeviceElem> DeviceBuffer<T> {
    /// Stub-only test constructor: build a `DeviceBuffer<T>` without
    /// going through the driver. Mirrors the `for_test` constructors on
    /// `DeviceContext` / `DeviceModule`; needed so the in-crate
    /// stub-mode event-ordering test can stand up a buffer that
    /// `from_host_async` would otherwise refuse to allocate (the stub
    /// backend's `from_host` returns `Err(NoDriver)`).
    ///
    /// The returned buffer has a fresh empty `last_write` slot, so it
    /// behaves like a buffer that has had no prior writes recorded.
    pub(crate) fn for_test() -> Self {
        Self {
            inner: backend::DeviceBufferInner {
                _phantom: std::marker::PhantomData,
            },
            last_write: new_last_write_slot(),
            _host_uploads: Vec::new(),
        }
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

pub mod critical;
pub mod event;
pub mod graph;
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

    /// AUDIT 2026-05-29 (H10b): the per-buffer `last_write` slot must be
    /// *shared* (same `Arc`) between a `DeviceBuffer` and the
    /// `KernelArg::DevicePtr` built from it. This is the mechanism that
    /// lets `launch_on_stream` stamp the kernel-completion event into
    /// the originating buffer's slot so a subsequent `to_host_async`
    /// waits on THIS buffer's producing kernel rather than a clobbered
    /// context-wide event. Driver-free: exercises the slot wiring only.
    #[test]
    fn push_device_ptr_shares_last_write_slot() {
        let buf: DeviceBuffer<f32> = DeviceBuffer::for_test();
        let buf_slot = buf.last_write.clone();
        let args = KernelArgs::new().push_device_ptr(&buf);
        let arg_slot = match &args.raw[0] {
            KernelArg::DevicePtr { last_write, .. } => last_write.clone(),
            _ => panic!("expected DevicePtr arg"),
        };
        // The two handles must point at the SAME allocation.
        assert!(
            std::sync::Arc::ptr_eq(&buf_slot, &arg_slot),
            "buffer and KernelArg must share the same last_write slot"
        );
        // A write through the arg's handle must be observable through
        // the buffer's handle (this is how launch_on_stream propagates
        // kernel_done back to the buffer).
        let ctx = DeviceContext::for_test();
        let ev = std::sync::Arc::new(Event::new(&ctx).expect("stub event"));
        let ev_id = ev.id();
        *arg_slot.lock().unwrap() = Some(ev);
        let seen = buf_slot.lock().unwrap().as_ref().map(|e| e.id());
        assert_eq!(seen, Some(ev_id));
    }

    #[test]
    fn unchecked_upload_records_ordering_before_stub_error() {
        let ctx = DeviceContext::for_test();
        let stream = Stream::for_test();
        let host = [1_u32, 2, 3, 4];

        // SAFETY: this module is `cfg(all(test, not(feature = "cuda")))`, so the
        // stub backend is the one compiled in and no DMA is ever submitted to a
        // driver. The borrowed-host contract — `host` must stay valid until the
        // upload's `last_write` event fires — is therefore vacuous; `host`
        // outlives the call regardless. Same argument as the `SAFETY` on
        // `from_host_async`'s own delegation above.
        match unsafe { DeviceBuffer::<u32>::from_host_async_unchecked(&ctx, &host, &stream) } {
            Err(DeviceError::NoDriver) => {}
            Err(other) => panic!("expected NoDriver, got {other:?}"),
            Ok(_) => panic!("expected NoDriver, got Ok(DeviceBuffer)"),
        }

        let ops = stream.ops();
        assert_eq!(
            ops.len(),
            2,
            "unchecked upload should record event + upload before stub allocation error"
        );
        assert!(
            matches!(ops[0], StreamOp::EventRecord { .. }),
            "upload must publish its last_write event before the UploadAsync op: {ops:?}"
        );
        assert_eq!(
            ops[1],
            StreamOp::UploadAsync {
                bytes: std::mem::size_of_val(&host)
            }
        );
    }

    #[test]
    fn unchecked_download_records_borrowed_dma_without_synchronizing() {
        let ctx = DeviceContext::for_test();
        let stream = Stream::for_test();
        let buf: DeviceBuffer<f32> = DeviceBuffer::for_test();
        let last_write = std::sync::Arc::new(Event::new(&ctx).expect("stub event"));
        let last_write_id = last_write.id();
        *buf.last_write.lock().unwrap_or_else(|p| p.into_inner()) = Some(last_write);

        let mut dst = [0.0_f32; 2];
        // SAFETY: stub-only module (see the sibling upload test), so no DMA is
        // queued and the borrowed-destination contract — `dst` unread and at a
        // fixed address until the download completes — is vacuous. `dst` is a
        // stack local that outlives the call and is only read after it returns.
        match unsafe { buf.to_host_async_unchecked(&mut dst, &stream) } {
            Err(DeviceError::NoDriver) => {}
            Err(other) => panic!("expected NoDriver, got {other:?}"),
            Ok(()) => panic!("expected NoDriver, got Ok(())"),
        }

        let ops = stream.ops();
        assert_eq!(
            ops,
            vec![
                StreamOp::EventWait {
                    event_id: last_write_id
                },
                StreamOp::DownloadAsync {
                    bytes: std::mem::size_of_val(&dst)
                },
            ],
            "unchecked download must not insert a Synchronize op"
        );
    }

    /// AUDIT 2026-06-21: every PTX load must get a process-unique module
    /// name so cudarc's per-context module map does not have one
    /// `DeviceModule` clobber another. Verify the minted names are
    /// well-formed and never repeat across successive calls.
    #[test]
    fn module_names_are_unique() {
        const N: usize = 1024;
        let names: Vec<String> = (0..N).map(|_| next_module_name()).collect();
        for name in &names {
            assert!(
                name.starts_with("cratonvm_mod_"),
                "unexpected module name {name:?}"
            );
        }
        let distinct: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(
            distinct.len(),
            N,
            "module names must be unique across loads"
        );
    }
}
