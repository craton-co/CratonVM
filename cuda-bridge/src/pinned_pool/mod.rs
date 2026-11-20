// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Reusable page-locked host staging pool ([`PinnedHostPool`]) and GC-arena
//! registration cache ([`HostRegionPinCache`]).
//!
//! # Why two mechanisms
//!
//! 1. **[`PinnedHostPool`]** serves async H2D/D2H transfers that copy out of or
//!    into pageable host memory (for example JVM heap arrays when direct
//!    pinning is unavailable). Buffers are bucketed by power-of-two size class,
//!    backed by `cuMemAllocHost_v2` on `cuda` builds (and 64-byte-aligned heap
//!    buffers on non-`cuda` builds), and returned via [`PinnedHostPool::release_deferred`]
//!    so an in-flight DMA buffer is never reused by the CPU before its GPU
//!    completion event fires.
//! 2. **[`HostRegionPinCache`]** caches `cuMemHostRegister_v2` registrations
//!    over contiguous GC arena spans `(base_addr, len)` with reference counting.
//!    `cuMemHostUnregister` is expensive (~0.69 ms), so keeping spans pinned
//!    across dispatches amortizes registration while `unregister_span` / `clear`
//!    let the collector tear down unmapped arenas cleanly. On non-`cuda` builds
//!    or if the driver refuses registration, it degrades gracefully with
//!    [`PinStatus::UnpinnedFallback`].
//!
//! Synchronous `DeviceBuffer::from_host` deliberately stays un-staged because
//! staging synchronous H2D copies through pinned memory was measured 10-23%
//! slower than the driver's internal pipeline (`PinnedHostBuffer` docs).

use crate::{DeviceContext, DeviceError, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Default upper bound on idle pooled bytes retained by [`PinnedHostPool`] (64 MiB).
pub const DEFAULT_PINNED_POOL_CAP_BYTES: usize = 64 << 20;

/// Minimum size class for pooled staging buffers (one 4 KiB OS page).
pub const MIN_PINNED_CLASS_BYTES: usize = 4096;

/// 64-byte-aligned storage chunk for non-CUDA / fallback host staging buffers.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct AlignedChunk([u8; 64]);

fn size_class_bytes(requested: usize) -> Result<usize> {
    let min_bytes = requested.max(MIN_PINNED_CLASS_BYTES);
    min_bytes.checked_next_power_of_two().ok_or_else(|| {
        DeviceError::Memcpy(format!(
            "pinned staging request too large for size class: {requested} bytes"
        ))
    })
}

enum PinnedBacking {
    #[cfg(feature = "cuda")]
    Cuda {
        ptr: std::ptr::NonNull<u8>,
        class_bytes: usize,
        ctx: Option<DeviceContext>,
    },
    #[cfg(feature = "hip")]
    Hip {
        ptr: std::ptr::NonNull<u8>,
        class_bytes: usize,
        ctx: Option<DeviceContext>,
    },
    HostFallback {
        buf: Vec<AlignedChunk>,
        class_bytes: usize,
    },
}

// SAFETY: `Cuda` holds a `cuMemAllocHost_v2` pointer owned uniquely by this
// `PinnedBacking`; `HostFallback` owns a `Vec`. Only one `PinnedHostLease` (or
// pool slot) owns a `PinnedBacking` at a time.
unsafe impl Send for PinnedBacking {}
// SAFETY: Shared references do not mutate the backing and DMA ordering is
// enforced by deferred release before reuse.
unsafe impl Sync for PinnedBacking {}

impl PinnedBacking {
    fn allocate(ctx: Option<&DeviceContext>, class_bytes: usize) -> Result<Self> {
        #[cfg(feature = "cuda")]
        {
            if let Some(ctx) = ctx {
                ctx.inner().bind_to_thread()?;
                let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
                // SAFETY: `raw` is a valid out-pointer and `ctx` was bound on
                // this thread immediately above.
                let status = unsafe {
                    cudarc::driver::sys::lib().cuMemAllocHost_v2(&mut raw, class_bytes.max(1))
                };
                if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                    return Err(DeviceError::Driver(format!(
                        "cuMemAllocHost_v2({class_bytes}): {status:?}"
                    )));
                }
                let ptr = std::ptr::NonNull::new(raw.cast::<u8>()).ok_or_else(|| {
                    DeviceError::Driver("cuMemAllocHost_v2 returned null pointer".to_string())
                })?;
                return Ok(Self::Cuda {
                    ptr,
                    class_bytes,
                    ctx: Some(ctx.clone()),
                });
            }

            // Contextless construction (for example tests compiled with `--features cuda`
            // on a host without a GPU) degrades cleanly to aligned heap memory so `Drop`
            // never depends on an ambient thread-local CUDA context.
        }

        #[cfg(feature = "hip")]
        {
            if let Some(ctx) = ctx.filter(|_| cubecl_hip_sys::is_available()) {
                ctx.inner().bind_to_thread()?;
                let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
                let alloc_bytes = class_bytes.max(1);
                // SAFETY: `raw` is a valid out-pointer and `ctx` was bound on
                // this thread immediately above. Mapped + portable host memory
                // backs zero-copy APU transfers cleanly across threads.
                let mut status = unsafe {
                    cubecl_hip_sys::hipHostMalloc(
                        &mut raw,
                        alloc_bytes,
                        cubecl_hip_sys::hipHostMallocMapped | cubecl_hip_sys::hipHostMallocPortable,
                    )
                };
                if status != cubecl_hip_sys::hipError_t_hipSuccess {
                    // SAFETY: fallback to default flags when a HIP runtime declines mapped|portable.
                    status = unsafe {
                        cubecl_hip_sys::hipHostMalloc(
                            &mut raw,
                            alloc_bytes,
                            cubecl_hip_sys::hipHostMallocDefault,
                        )
                    };
                }
                if status != cubecl_hip_sys::hipError_t_hipSuccess {
                    return Err(DeviceError::Driver(format!(
                        "hipHostMalloc({class_bytes}): {status:?}"
                    )));
                }
                let ptr = std::ptr::NonNull::new(raw.cast::<u8>()).ok_or_else(|| {
                    DeviceError::Driver("hipHostMalloc returned null pointer".to_string())
                })?;
                return Ok(Self::Hip {
                    ptr,
                    class_bytes,
                    ctx: Some(ctx.clone()),
                });
            }
        }

        let _ = ctx;
        let chunks = class_bytes.div_ceil(64);
        Ok(Self::HostFallback {
            buf: vec![AlignedChunk([0u8; 64]); chunks],
            class_bytes,
        })
    }

    fn class_bytes(&self) -> usize {
        match self {
            #[cfg(feature = "cuda")]
            Self::Cuda { class_bytes, .. } => *class_bytes,
            #[cfg(feature = "hip")]
            Self::Hip { class_bytes, .. } => *class_bytes,
            Self::HostFallback { class_bytes, .. } => *class_bytes,
        }
    }

    fn is_pinned(&self) -> bool {
        match self {
            #[cfg(feature = "cuda")]
            Self::Cuda { .. } => true,
            #[cfg(feature = "hip")]
            Self::Hip { .. } => true,
            Self::HostFallback { .. } => false,
        }
    }

    fn as_slice(&self, len: usize) -> &[u8] {
        debug_assert!(len <= self.class_bytes());
        match self {
            #[cfg(feature = "cuda")]
            Self::Cuda { ptr, .. } => {
                // SAFETY: `ptr` points to `class_bytes >= len` uniquely owned
                // host-pinned bytes for the lifetime of this borrow.
                unsafe { std::slice::from_raw_parts(ptr.as_ptr(), len) }
            }
            #[cfg(feature = "hip")]
            Self::Hip { ptr, .. } => {
                // SAFETY: `ptr` points to `class_bytes >= len` uniquely owned
                // `hipHostMalloc` bytes for the lifetime of this borrow.
                unsafe { std::slice::from_raw_parts(ptr.as_ptr(), len) }
            }
            Self::HostFallback { buf, .. } => {
                // SAFETY: `AlignedChunk` is `#[repr(C, align(64))]` over `[u8; 64]`
                // with no padding, and `buf.len() * 64 >= class_bytes >= len`.
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len) }
            }
        }
    }

    fn as_mut_slice(&mut self, len: usize) -> &mut [u8] {
        debug_assert!(len <= self.class_bytes());
        match self {
            #[cfg(feature = "cuda")]
            Self::Cuda { ptr, .. } => {
                // SAFETY: `ptr` points to `class_bytes >= len` uniquely owned
                // host-pinned bytes and `&mut self` guarantees exclusivity.
                unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), len) }
            }
            #[cfg(feature = "hip")]
            Self::Hip { ptr, .. } => {
                // SAFETY: `ptr` points to `class_bytes >= len` uniquely owned
                // `hipHostMalloc` bytes and `&mut self` guarantees exclusivity.
                unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), len) }
            }
            Self::HostFallback { buf, .. } => {
                // SAFETY: `AlignedChunk` is `#[repr(C, align(64))]` over `[u8; 64]`
                // with no padding, and `buf.len() * 64 >= class_bytes >= len`.
                unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), len) }
            }
        }
    }
}

impl Drop for PinnedBacking {
    fn drop(&mut self) {
        #[cfg(feature = "cuda")]
        if let Self::Cuda { ptr, ctx, .. } = self {
            if let Some(ctx) = ctx {
                if ctx.inner().bind_to_thread().is_err() {
                    return;
                }
            }
            // SAFETY: `ptr` came from `cuMemAllocHost_v2` and is owned solely
            // by this `PinnedBacking`, so freeing it once here cannot double-free.
            unsafe {
                let _ = cudarc::driver::sys::lib()
                    .cuMemFreeHost(ptr.as_ptr() as *mut std::ffi::c_void);
            }
        }
        #[cfg(feature = "hip")]
        if let Self::Hip { ptr, ctx, .. } = self {
            if !cubecl_hip_sys::is_available() {
                return;
            }
            if let Some(ctx) = ctx {
                if ctx.inner().bind_to_thread().is_err() {
                    return;
                }
            }
            // SAFETY: `ptr` came from `hipHostMalloc` and is owned solely by
            // this `PinnedBacking`, so freeing it once here cannot double-free.
            unsafe {
                let _ = cubecl_hip_sys::hipHostFree(ptr.as_ptr() as *mut std::ffi::c_void);
            }
        }
    }
}

/// Exclusive lease of a pooled page-locked host staging buffer.
///
/// Dropping a lease without returning it via [`PinnedHostPool::release`] or
/// [`PinnedHostPool::release_deferred`] frees its backing memory safely.
pub struct PinnedHostLease {
    backing: Option<PinnedBacking>,
    requested_bytes: usize,
    last_stream_id: u64,
}

impl PinnedHostLease {
    /// Requested byte length of this lease (may be smaller than [`Self::capacity_bytes`]).
    pub fn len(&self) -> usize {
        self.requested_bytes
    }

    /// Whether the requested byte length is zero.
    pub fn is_empty(&self) -> bool {
        self.requested_bytes == 0
    }

    /// Underlying size-class capacity in bytes.
    pub fn capacity_bytes(&self) -> usize {
        self.backing.as_ref().map_or(0, PinnedBacking::class_bytes)
    }

    /// `true` when backed by `cuMemAllocHost_v2`, `false` on stub/fallback memory.
    pub fn is_pinned(&self) -> bool {
        self.backing.as_ref().is_some_and(PinnedBacking::is_pinned)
    }

    /// Stream identifier associated with this lease at acquisition.
    pub fn stream_id(&self) -> u64 {
        self.last_stream_id
    }

    /// Read-only view of the requested byte range.
    pub fn as_slice(&self) -> &[u8] {
        self.backing
            .as_ref()
            .map_or(&[], |b| b.as_slice(self.requested_bytes))
    }

    /// Mutable view of the requested byte range.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let len = self.requested_bytes;
        self.backing
            .as_mut()
            .map_or(&mut [], |b| b.as_mut_slice(len))
    }

    /// View the requested byte range as `&[T]`.
    pub fn as_typed_slice<T: bytemuck::Pod>(&self) -> Result<&[T]> {
        bytemuck::try_cast_slice(self.as_slice())
            .map_err(|e| DeviceError::Memcpy(format!("pinned lease slice cast failed: {e}")))
    }

    /// View the requested byte range as `&mut [T]`.
    pub fn as_typed_mut_slice<T: bytemuck::Pod>(&mut self) -> Result<&mut [T]> {
        bytemuck::try_cast_slice_mut(self.as_mut_slice())
            .map_err(|e| DeviceError::Memcpy(format!("pinned lease mut slice cast failed: {e}")))
    }
}

/// Snapshot of [`PinnedHostPool`] occupancy and hit/eviction counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinnedHostPoolStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub ready_buffers: usize,
    pub ready_bytes: usize,
    pub deferred_buffers: usize,
    pub deferred_bytes: usize,
    pub cap_bytes: usize,
}

struct ReadyEntry {
    backing: PinnedBacking,
    last_stream_id: u64,
    seq: u64,
}

struct DeferredEntry {
    backing: PinnedBacking,
    stream_id: u64,
    class_bytes: usize,
    ready_fn: Box<dyn FnMut() -> bool + Send>,
}

struct PinnedPoolState {
    ready: BTreeMap<usize, VecDeque<ReadyEntry>>,
    deferred: VecDeque<DeferredEntry>,
    ready_bytes: usize,
    deferred_bytes: usize,
    next_seq: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

struct PinnedPoolInner {
    ctx: Option<DeviceContext>,
    cap_bytes: usize,
    state: Mutex<PinnedPoolState>,
}

impl Drop for PinnedPoolInner {
    fn drop(&mut self) {
        let mut deferred = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.deferred_bytes = 0;
            std::mem::take(&mut state.deferred)
        };
        if deferred.is_empty() {
            return;
        }
        let mut synced = false;
        while let Some(mut entry) = deferred.pop_front() {
            if !(entry.ready_fn)() {
                if !synced {
                    synced = self
                        .ctx
                        .as_ref()
                        .is_some_and(|ctx| ctx.synchronize().is_ok());
                }
                if !synced && !(entry.ready_fn)() {
                    // Never free host memory while DMA may still be in flight.
                    std::mem::forget(entry.backing);
                }
            }
        }
    }
}

/// Size-classed stream-ordered pool of reusable page-locked host staging buffers.
#[derive(Clone)]
pub struct PinnedHostPool {
    inner: Arc<PinnedPoolInner>,
}

impl Default for PinnedHostPool {
    fn default() -> Self {
        Self::new()
    }
}

impl PinnedHostPool {
    /// Create a pool with the default 64 MiB idle capacity cap.
    ///
    /// On non-`cuda` builds (or on `cuda` builds without a current context),
    /// allocations degrade cleanly to 64-byte-aligned heap buffers.
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_PINNED_POOL_CAP_BYTES)
    }

    /// Create a pool with a custom idle capacity cap in bytes.
    pub fn with_cap(cap_bytes: usize) -> Self {
        Self {
            inner: Arc::new(PinnedPoolInner {
                ctx: None,
                cap_bytes,
                state: Mutex::new(PinnedPoolState {
                    ready: BTreeMap::new(),
                    deferred: VecDeque::new(),
                    ready_bytes: 0,
                    deferred_bytes: 0,
                    next_seq: 0,
                    hits: 0,
                    misses: 0,
                    evictions: 0,
                }),
            }),
        }
    }

    /// Create a CUDA-context-bound pool with a custom idle capacity cap in bytes.
    pub fn with_context(ctx: DeviceContext, cap_bytes: usize) -> Self {
        Self {
            inner: Arc::new(PinnedPoolInner {
                ctx: Some(ctx),
                cap_bytes,
                state: Mutex::new(PinnedPoolState {
                    ready: BTreeMap::new(),
                    deferred: VecDeque::new(),
                    ready_bytes: 0,
                    deferred_bytes: 0,
                    next_seq: 0,
                    hits: 0,
                    misses: 0,
                    evictions: 0,
                }),
            }),
        }
    }

    /// Acquire a staging buffer of at least `bytes` capacity for `stream_id`.
    ///
    /// Checks ready buffers of the matching size class first (preferring entries
    /// previously used on `stream_id`), then polls deferred releases whose GPU
    /// completion fence has fired, and allocates fresh only on a miss.
    pub fn acquire(&self, bytes: usize, stream_id: u64) -> Result<PinnedHostLease> {
        let class_bytes = size_class_bytes(bytes)?;
        if let Some(backing) = self.take_ready(class_bytes, stream_id) {
            cratonvm_types::gpu_event_census::note_pinned_pool_hit();
            return Ok(PinnedHostLease {
                backing: Some(backing),
                requested_bytes: bytes,
                last_stream_id: stream_id,
            });
        }

        // Poll deferred entries outside the mutex so a user-provided query
        // closure cannot deadlock if it inspects pool state, then re-try.
        if self.reclaim_completed() > 0 {
            if let Some(backing) = self.take_ready(class_bytes, stream_id) {
                cratonvm_types::gpu_event_census::note_pinned_pool_hit();
                return Ok(PinnedHostLease {
                    backing: Some(backing),
                    requested_bytes: bytes,
                    last_stream_id: stream_id,
                });
            }
        }

        {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.misses = state.misses.saturating_add(1);
        }
        cratonvm_types::gpu_event_census::note_pinned_pool_miss();
        let backing = PinnedBacking::allocate(self.inner.ctx.as_ref(), class_bytes)?;
        Ok(PinnedHostLease {
            backing: Some(backing),
            requested_bytes: bytes,
            last_stream_id: stream_id,
        })
    }

    /// Return a lease immediately when the caller has already verified no GPU
    /// DMA is referencing its memory.
    pub fn release(&self, mut lease: PinnedHostLease) {
        let Some(backing) = lease.backing.take() else {
            return;
        };
        let stream_id = lease.last_stream_id;
        let evicted = self.park_ready(backing, stream_id);
        drop(evicted);
    }

    /// Return a lease whose DMA may still be in flight on `stream_id`.
    ///
    /// `event_query_fn` must return `true` once the GPU has finished reading or
    /// writing the buffer. Until then, the backing stays quarantined in the
    /// deferred queue so the CPU cannot race with an active DMA.
    pub fn release_deferred<F>(&self, mut lease: PinnedHostLease, stream_id: u64, mut event_query_fn: F)
    where
        F: FnMut() -> bool + Send + 'static,
    {
        let Some(backing) = lease.backing.take() else {
            return;
        };
        if event_query_fn() {
            let evicted = self.park_ready(backing, stream_id);
            drop(evicted);
            return;
        }
        let class_bytes = backing.class_bytes();
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        state.deferred_bytes = state.deferred_bytes.saturating_add(class_bytes);
        state.deferred.push_back(DeferredEntry {
            backing,
            stream_id,
            class_bytes,
            ready_fn: Box::new(event_query_fn),
        });
    }

    /// Poll all deferred buffers and move completed ones into the ready pool
    /// (evicting older ready entries if the idle cap is exceeded).
    ///
    /// Returns the number of deferred buffers whose fence fired.
    pub fn reclaim_completed(&self) -> usize {
        struct ReclaimGuard<'a> {
            inner: &'a PinnedPoolInner,
            pending: VecDeque<DeferredEntry>,
            still_pending: VecDeque<DeferredEntry>,
        }
        impl Drop for ReclaimGuard<'_> {
            fn drop(&mut self) {
                if self.pending.is_empty() && self.still_pending.is_empty() {
                    return;
                }
                let mut restore_bytes = 0usize;
                let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
                while let Some(item) = self.still_pending.pop_front() {
                    restore_bytes = restore_bytes.saturating_add(item.class_bytes);
                    state.deferred.push_back(item);
                }
                while let Some(item) = self.pending.pop_front() {
                    restore_bytes = restore_bytes.saturating_add(item.class_bytes);
                    state.deferred.push_back(item);
                }
                state.deferred_bytes = state.deferred_bytes.saturating_add(restore_bytes);
            }
        }

        let pending = {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.deferred.is_empty() {
                return 0;
            }
            state.deferred_bytes = 0;
            std::mem::take(&mut state.deferred)
        };

        let mut guard = ReclaimGuard {
            inner: &self.inner,
            pending,
            still_pending: VecDeque::new(),
        };
        let mut completed = Vec::new();
        while let Some(mut entry) = guard.pending.pop_front() {
            if (entry.ready_fn)() {
                completed.push((entry.backing, entry.stream_id));
            } else {
                guard.still_pending.push_back(entry);
            }
        }
        drop(guard);

        let reclaimed_count = completed.len();
        for (backing, stream_id) in completed {
            let evicted = self.park_ready(backing, stream_id);
            drop(evicted);
        }
        reclaimed_count
    }

    /// Drop all currently idle ready buffers (deferred buffers remain quarantined
    /// until their fence completes).
    pub fn clear_ready(&self) {
        let dropped = {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.ready_bytes = 0;
            std::mem::take(&mut state.ready)
        };
        drop(dropped);
    }

    /// Snapshot current pool statistics.
    pub fn stats(&self) -> PinnedHostPoolStats {
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        let ready_buffers = state.ready.values().map(VecDeque::len).sum();
        PinnedHostPoolStats {
            hits: state.hits,
            misses: state.misses,
            evictions: state.evictions,
            ready_buffers,
            ready_bytes: state.ready_bytes,
            deferred_buffers: state.deferred.len(),
            deferred_bytes: state.deferred_bytes,
            cap_bytes: self.inner.cap_bytes,
        }
    }

    fn take_ready(&self, class_bytes: usize, stream_id: u64) -> Option<PinnedBacking> {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        let popped = {
            let queue = state.ready.get_mut(&class_bytes)?;
            if let Some(pos) = queue.iter().rposition(|e| e.last_stream_id == stream_id) {
                queue.remove(pos)
            } else {
                queue.pop_back()
            }
        };
        let entry = popped?;
        if state
            .ready
            .get(&class_bytes)
            .is_some_and(VecDeque::is_empty)
        {
            state.ready.remove(&class_bytes);
        }
        state.ready_bytes = state.ready_bytes.saturating_sub(class_bytes);
        state.hits = state.hits.saturating_add(1);
        Some(entry.backing)
    }

    fn park_ready(&self, backing: PinnedBacking, stream_id: u64) -> Vec<PinnedBacking> {
        let class_bytes = backing.class_bytes();
        let cap = self.inner.cap_bytes;
        let mut evicted = Vec::new();
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        if class_bytes > cap {
            state.evictions = state.evictions.saturating_add(1);
            evicted.push(backing);
            return evicted;
        }
        while state.ready_bytes.saturating_add(class_bytes) > cap {
            let Some(victim_class) = state
                .ready
                .iter()
                .filter_map(|(&c, q)| q.front().map(|e| (e.seq, c)))
                .min_by_key(|&(seq, _)| seq)
                .map(|(_, c)| c)
            else {
                break;
            };
            let removed_empty = if let Some(queue) = state.ready.get_mut(&victim_class) {
                if let Some(victim) = queue.pop_front() {
                    evicted.push(victim.backing);                } else {
                    break;                }
                queue.is_empty()            } else {
                break;
            };
            state.ready_bytes = state.ready_bytes.saturating_sub(victim_class);
            state.evictions = state.evictions.saturating_add(1);
            if removed_empty {
                state.ready.remove(&victim_class);
            }
        }
        let seq = state.next_seq;
        state.next_seq = state.next_seq.saturating_add(1);
        state.ready_bytes = state.ready_bytes.saturating_add(class_bytes);
        state
            .ready
            .entry(class_bytes)
            .or_default()
            .push_back(ReadyEntry {
                backing,
                last_stream_id: stream_id,
                seq,
            });
        evicted
    }
}

/// Whether a [`PinnedSpanGuard`] is backed by an active `cuMemHostRegister_v2`
/// pinning or fell back to unpinned host memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinStatus {
    /// Span is page-locked via `cuMemHostRegister_v2`.
    Pinned,
    /// Non-`cuda` build or driver refused registration; caller should use
    /// staged transfers without failing.
    UnpinnedFallback,
}

/// Per-VM quota limits for [`HostRegionPinCache`].
///
/// Any field set to `0` means no per-VM limit on that dimension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostPinQuota {
    pub soft_pinned_bytes: u64,
    pub hard_pinned_bytes: u64,
    pub max_spans: usize,
}

/// Per-`vm_identity` diagnostic snapshot of [`HostRegionPinCache`] usage and
/// quota enforcement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostPinCacheStats {
    pub vm_identity: u64,
    pub pinned_bytes: u64,
    pub peak_pinned_bytes: u64,
    pub span_count: usize,
    pub soft_pinned_bytes: u64,
    pub hard_pinned_bytes: u64,
    pub max_spans: usize,
    pub hits: u64,
    pub misses: u64,
    pub registrations: u64,
    pub evictions: u64,
    pub soft_watermark_triggers: u64,
    pub quota_denials: u64,
}

#[derive(Debug)]
struct HostPinQuotaSlot {
    quota: Mutex<HostPinQuota>,
    pinned_bytes: AtomicU64,
    peak_pinned_bytes: AtomicU64,
    span_count: AtomicUsize,
    hits: AtomicU64,
    misses: AtomicU64,
    registrations: AtomicU64,
    evictions: AtomicU64,
    soft_watermark_triggers: AtomicU64,
    quota_denials: AtomicU64,
}

impl HostPinQuotaSlot {
    fn new() -> Self {
        Self {
            quota: Mutex::new(HostPinQuota::default()),
            pinned_bytes: AtomicU64::new(0),
            peak_pinned_bytes: AtomicU64::new(0),
            span_count: AtomicUsize::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            registrations: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            soft_watermark_triggers: AtomicU64::new(0),
            quota_denials: AtomicU64::new(0),
        }
    }

    fn record_peak(&self, used_after: u64) {
        let mut cur = self.peak_pinned_bytes.load(Ordering::Relaxed);
        while used_after > cur {
            match self.peak_pinned_bytes.compare_exchange_weak(
                cur,
                used_after,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    fn snapshot(&self, vm_identity: u64) -> HostPinCacheStats {
        let q = *self.quota.lock().unwrap_or_else(|p| p.into_inner());
        HostPinCacheStats {
            vm_identity,
            pinned_bytes: self.pinned_bytes.load(Ordering::Relaxed),
            peak_pinned_bytes: self.peak_pinned_bytes.load(Ordering::Relaxed),
            span_count: self.span_count.load(Ordering::Relaxed),
            soft_pinned_bytes: q.soft_pinned_bytes,
            hard_pinned_bytes: q.hard_pinned_bytes,
            max_spans: q.max_spans,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            registrations: self.registrations.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            soft_watermark_triggers: self.soft_watermark_triggers.load(Ordering::Relaxed),
            quota_denials: self.quota_denials.load(Ordering::Relaxed),
        }
    }
}

static HOST_PIN_QUOTA_SLOTS: OnceLock<Mutex<HashMap<u64, Arc<HostPinQuotaSlot>>>> = OnceLock::new();

fn host_pin_quota_slot_for(vm_identity: u64) -> Arc<HostPinQuotaSlot> {
    let map = HOST_PIN_QUOTA_SLOTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    Arc::clone(
        guard
            .entry(vm_identity)
            .or_insert_with(|| Arc::new(HostPinQuotaSlot::new())),
    )
}

/// Configure per-VM limits for [`HostRegionPinCache`] (`vm_identity == 0` = unscoped default).
pub fn set_vm_host_pin_quota(vm_identity: u64, quota: HostPinQuota) {
    let slot = host_pin_quota_slot_for(vm_identity);
    *slot.quota.lock().unwrap_or_else(|p| p.into_inner()) = quota;
}

/// Return the active [`HostPinQuota`] for `vm_identity`.
pub fn vm_host_pin_quota(vm_identity: u64) -> HostPinQuota {
    *host_pin_quota_slot_for(vm_identity)
        .quota
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Return a diagnostic snapshot of host-pin cache stats for `vm_identity`.
pub fn vm_host_pin_stats(vm_identity: u64) -> HostPinCacheStats {
    host_pin_quota_slot_for(vm_identity).snapshot(vm_identity)
}

/// Return sorted host-pin cache snapshots for all known VM identities.
pub fn all_vm_host_pin_stats() -> Vec<HostPinCacheStats> {
    let Some(map) = HOST_PIN_QUOTA_SLOTS.get() else {
        return Vec::new();
    };
    let snapshot: Vec<(u64, Arc<HostPinQuotaSlot>)> = {
        let guard = map.lock().unwrap_or_else(|p| p.into_inner());
        guard.iter().map(|(&k, v)| (k, Arc::clone(v))).collect()
    };
    let mut stats: Vec<HostPinCacheStats> = snapshot
        .into_iter()
        .map(|(vm_id, slot)| slot.snapshot(vm_id))
        .collect();
    stats.sort_by_key(|s| s.vm_identity);
    stats
}

/// Remove per-VM host-pin quota tracking on VM teardown.
pub fn clear_vm_host_pin_quota(vm_identity: u64) {
    if let Some(map) = HOST_PIN_QUOTA_SLOTS.get() {
        let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(&vm_identity);
    }
}

#[derive(Clone, Copy, Debug)]
struct SpanEntry {
    refcount: usize,
    registered: bool,
    dev_ptr_base: Option<u64>,
    last_used_seq: u64,
}

struct PinCacheState {
    spans: BTreeMap<(usize, usize), SpanEntry>,
    /// Disjoint index of currently registered spans (`base_addr -> len`) for O(log N) lookups.
    registered_spans: BTreeMap<usize, usize>,
    seq: u64,
}

#[cfg(test)]
pub(crate) trait HostPinTestOps: Send + Sync {
    fn register(&self, base_addr: usize, len: usize) -> bool;
    fn unregister(&self, base_addr: usize);
}

struct PinCacheInner {
    ctx: Option<DeviceContext>,
    vm_identity: u64,
    quota_slot: Arc<HostPinQuotaSlot>,
    state: Mutex<PinCacheState>,
    reg_lock: Mutex<()>,
    #[cfg(test)]
    test_ops: Option<Arc<dyn HostPinTestOps>>,
}

impl PinCacheInner {
    fn register_span(&self, base_addr: usize, len: usize) -> (bool, Option<u64>) {
        #[cfg(test)]
        if let Some(ops) = &self.test_ops {
            return (ops.register(base_addr, len), None);
        }
        try_register_host_span(self.ctx.as_ref(), base_addr, len)
    }

    fn unregister_span(&self, base_addr: usize) {
        #[cfg(test)]
        if let Some(ops) = &self.test_ops {
            ops.unregister(base_addr);
            return;
        }
        try_unregister_host_span(self.ctx.as_ref(), base_addr);
    }
}

impl Drop for PinCacheInner {
    fn drop(&mut self) {
        let spans = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.registered_spans.clear();
            std::mem::take(&mut state.spans)
        };
        let mut removed_pinned_bytes = 0u64;
        let removed_count = spans.len();
        for ((base_addr, len), entry) in spans {
            if entry.registered {
                removed_pinned_bytes = removed_pinned_bytes.saturating_add(len as u64);
                self.unregister_span(base_addr);
            }
        }
        if removed_count > 0 {
            let _ =
                self.quota_slot
                    .span_count
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                        Some(c.saturating_sub(removed_count))
                    });
        }
        if removed_pinned_bytes > 0 {
            let _ = self.quota_slot.pinned_bytes.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |c| Some(c.saturating_sub(removed_pinned_bytes)),
            );
        }
    }
}

/// RAII guard holding one reference count on a cached host span in [`HostRegionPinCache`].
///
/// Dropping the guard decrements the span's active refcount while leaving the
/// OS/driver registration cached for future dispatches until
/// [`HostRegionPinCache::unregister_span`] or [`HostRegionPinCache::clear`] is called.
pub struct PinnedSpanGuard {
    inner: Arc<PinCacheInner>,
    key: (usize, usize),
    status: PinStatus,
    dev_ptr_base: Option<u64>,
}

impl PinnedSpanGuard {
    /// `(base_addr, len)` of the cached span backing this guard.
    pub fn span(&self) -> (usize, usize) {
        self.key
    }

    /// Pinning status (`Pinned` vs `UnpinnedFallback`).
    pub fn status(&self) -> PinStatus {
        self.status
    }

    /// `true` when the span is actively registered with host-pinning (`cuMemHostRegister_v2` / `hipHostRegister`).
    pub fn is_pinned(&self) -> bool {
        self.status == PinStatus::Pinned
    }

    /// `true` when the cache degraded cleanly without driver registration.
    pub fn is_fallback(&self) -> bool {
        self.status == PinStatus::UnpinnedFallback
    }

    /// Device pointer mapped for `[addr, addr + byte_len)` inside this span,
    /// or `None` when device-mapped pointers were not obtained at registration.
    pub fn device_ptr_for(&self, addr: usize, byte_len: usize) -> Option<u64> {
        let dev_base = self.dev_ptr_base?;
        if addr < self.key.0 {
            return None;
        }
        let offset = addr - self.key.0;
        let end_offset = offset.checked_add(byte_len)?;
        if end_offset > self.key.1 {
            return None;
        }
        let delta = u64::try_from(offset).ok()?;
        dev_base.checked_add(delta)
    }
}

impl Drop for PinnedSpanGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = state.spans.get_mut(&self.key) {
            entry.refcount = entry.refcount.saturating_sub(1);
        }
    }
}

/// Cache of `cuMemHostRegister_v2` registrations over contiguous GC arena spans,
/// optionally partitioned per `vm_identity` so multi-tenant VMs cannot starve
/// each other's host pinning quota.
#[derive(Clone)]
pub struct HostRegionPinCache {
    inner: Arc<PinCacheInner>,
}

impl Default for HostRegionPinCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HostRegionPinCache {
    /// Create a pin cache with no bound [`DeviceContext`] (degrades cleanly on
    /// non-`cuda` builds or when no CUDA context is current).
    pub fn new() -> Self {
        Self::new_for_vm(0)
    }

    /// Create an unscoped pin cache bound to `vm_identity` with no [`DeviceContext`].
    pub fn new_for_vm(vm_identity: u64) -> Self {
        Self {
            inner: Arc::new(PinCacheInner {
                ctx: None,
                vm_identity,
                quota_slot: host_pin_quota_slot_for(vm_identity),
                state: Mutex::new(PinCacheState {
                    spans: BTreeMap::new(),
                    registered_spans: BTreeMap::new(),
                    seq: 0,
                }),
                reg_lock: Mutex::new(()),
                #[cfg(test)]
                test_ops: None,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_pin_ops_for_vm(
        vm_identity: u64,
        ops: Arc<dyn HostPinTestOps>,
    ) -> Self {
        Self {
            inner: Arc::new(PinCacheInner {
                ctx: None,
                vm_identity,
                quota_slot: host_pin_quota_slot_for(vm_identity),
                state: Mutex::new(PinCacheState {
                    spans: BTreeMap::new(),
                    registered_spans: BTreeMap::new(),
                    seq: 0,
                }),
                reg_lock: Mutex::new(()),
                test_ops: Some(ops),
            }),
        }
    }

    /// Create a pin cache bound to `ctx` for `cuMemHostRegister_v2` /
    /// `cuMemHostUnregister`.
    pub fn with_context(ctx: DeviceContext) -> Self {
        Self::with_context_for_vm(ctx, 0)
    }

    /// Create a pin cache bound to `ctx` and partitioned under `vm_identity`.
    pub fn with_context_for_vm(ctx: DeviceContext, vm_identity: u64) -> Self {
        Self {
            inner: Arc::new(PinCacheInner {
                ctx: Some(ctx),
                vm_identity,
                quota_slot: host_pin_quota_slot_for(vm_identity),
                state: Mutex::new(PinCacheState {
                    spans: BTreeMap::new(),
                    registered_spans: BTreeMap::new(),
                    seq: 0,
                }),
                reg_lock: Mutex::new(()),
                #[cfg(test)]
                test_ops: None,
            }),
        }
    }

    /// Owning VM identity (`0` when unscoped).
    pub fn vm_identity(&self) -> u64 {
        self.inner.vm_identity
    }

    /// Snapshot of host-pin cache stats and configured per-VM quota.
    pub fn stats(&self) -> HostPinCacheStats {
        self.inner.quota_slot.snapshot(self.inner.vm_identity)
    }

    fn evict_idle_for_quota(&self, incoming_len: usize, quota: HostPinQuota) {
        let incoming = incoming_len as u64;
        let mut to_unregister = Vec::new();
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                let cur_spans = self.inner.quota_slot.span_count.load(Ordering::Relaxed);
                let cur_bytes = self.inner.quota_slot.pinned_bytes.load(Ordering::Relaxed);
                let span_over = quota.max_spans > 0 && cur_spans >= quota.max_spans;
                let bytes_over = quota.hard_pinned_bytes > 0
                    && cur_bytes.saturating_add(incoming) > quota.hard_pinned_bytes;
                if !span_over && !bytes_over {
                    break;
                }
                let victim = state
                    .spans
                    .iter()
                    .filter(|(_, e)| e.refcount == 0 && (!bytes_over || e.registered))
                    .min_by_key(|(_, e)| (!bytes_over && e.registered, e.last_used_seq))
                    .or_else(|| {
                        if span_over {
                            state
                                .spans
                                .iter()
                                .filter(|(_, e)| e.refcount == 0)
                                .min_by_key(|(_, e)| e.last_used_seq)
                        } else {
                            None
                        }
                    })
                    .map(|(&k, _)| k);
                let Some(key) = victim else {
                    break;
                };
                if let Some(entry) = state.spans.remove(&key) {
                    if entry.registered {
                        state.registered_spans.remove(&key.0);
                    }
                    let _ = self.inner.quota_slot.span_count.try_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |c| Some(c.saturating_sub(1)),
                    );
                    self.inner
                        .quota_slot
                        .evictions
                        .fetch_add(1, Ordering::Relaxed);
                    if entry.registered {
                        let sz = key.1 as u64;
                        let _ = self.inner.quota_slot.pinned_bytes.try_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |c| Some(c.saturating_sub(sz)),
                        );
                        to_unregister.push(key.0);
                    }
                }
            }
        }
        for base_addr in to_unregister {
            self.inner.unregister_span(base_addr);
        }
    }

    fn try_acquire_hit(&self, base_addr: usize, end: usize) -> Option<PinnedSpanGuard> {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        let key = find_enclosing_key(&state, base_addr, end)?;
        state.seq = state.seq.wrapping_add(1);
        let seq = state.seq;
        let entry = state
            .spans
            .get_mut(&key)
            .expect("enclosing key must exist in map");
        entry.refcount = entry.refcount.saturating_add(1);
        entry.last_used_seq = seq;
        let status = status_from_registered(entry.registered);
        let dev_ptr_base = entry.dev_ptr_base;
        self.inner.quota_slot.hits.fetch_add(1, Ordering::Relaxed);
        cratonvm_types::gpu_event_census::note_host_pin_cache_hit();
        Some(PinnedSpanGuard {
            inner: Arc::clone(&self.inner),
            key,
            status,
            dev_ptr_base,
        })
    }

    fn try_reserve_bytes(&self, len: u64, hard_bytes: u64) -> Option<u64> {
        if hard_bytes == 0 {
            return Some(
                self.inner
                    .quota_slot
                    .pinned_bytes
                    .fetch_add(len, Ordering::Relaxed)
                    .saturating_add(len),
            );
        }
        self.inner
            .quota_slot
            .pinned_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                let next = cur.saturating_add(len);
                (next <= hard_bytes).then_some(next)
            })
            .ok()
            .map(|prev| prev.saturating_add(len))
    }

    fn release_reserved_bytes(&self, len: u64) {
        let _ = self.inner.quota_slot.pinned_bytes.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |cur| Some(cur.saturating_sub(len)),
        );
    }

    /// Acquire a refcounted [`PinnedSpanGuard`] covering `[base_addr, base_addr + len)`.
    ///
    /// Reuses an existing exact or enclosing cached span (incrementing its
    /// refcount and calling `note_host_pin_cache_hit`). On a miss, checks this
    /// VM's [`HostPinQuota`] (evicting idle spans if needed) and attempts
    /// `cuMemHostRegister_v2(..., CU_MEMHOSTREGISTER_PORTABLE)` on `cuda` builds,
    /// degrading cleanly to [`PinStatus::UnpinnedFallback`] when denied or unsupported.
    pub fn pin_span(&self, base_addr: usize, len: usize) -> Result<PinnedSpanGuard> {
        let end = validate_span(base_addr, len)?;

        if let Some(guard) = self.try_acquire_hit(base_addr, end) {
            return Ok(guard);
        }

        // Serialize cache misses so concurrent threads cannot double-register
        // or partially overlap `cuMemHostRegister_v2` on the same range.
        let _reg_guard = self
            .inner
            .reg_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(guard) = self.try_acquire_hit(base_addr, end) {
            return Ok(guard);
        }
        let overlaps_registered = {
            let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            has_overlapping_registered_span(&state, base_addr, end)
        };

        self.inner.quota_slot.misses.fetch_add(1, Ordering::Relaxed);

        let quota = *self
            .inner
            .quota_slot
            .quota
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if quota.max_spans > 0 || quota.hard_pinned_bytes > 0 {
            self.evict_idle_for_quota(len, quota);
        }

        let span_reserved = if quota.max_spans > 0 {
            self.inner
                .quota_slot
                .span_count
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    (cur < quota.max_spans).then_some(cur + 1)
                })
                .is_ok()
        } else {
            self.inner
                .quota_slot
                .span_count
                .fetch_add(1, Ordering::Relaxed);
            true
        };
        if !span_reserved {
            self.inner
                .quota_slot
                .quota_denials
                .fetch_add(1, Ordering::Relaxed);
            return Ok(PinnedSpanGuard {
                inner: Arc::clone(&self.inner),
                key: (base_addr, len),
                status: PinStatus::UnpinnedFallback,
                dev_ptr_base: None,
            });
        }

        let len_u64 = len as u64;
        let reserved_bytes_after = if overlaps_registered {
            None
        } else {
            match self.try_reserve_bytes(len_u64, quota.hard_pinned_bytes) {
                Some(after) => Some(after),
                None => {
                    self.inner
                        .quota_slot
                        .quota_denials
                        .fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        };

        let (registered, dev_ptr_base) = if let Some(after) = reserved_bytes_after {
            let (ok, dev_ptr_base) = self.inner.register_span(base_addr, len);
            if ok {
                self.inner
                    .quota_slot
                    .registrations
                    .fetch_add(1, Ordering::Relaxed);
                self.inner.quota_slot.record_peak(after);
                if quota.soft_pinned_bytes > 0 && after >= quota.soft_pinned_bytes {
                    self.inner
                        .quota_slot
                        .soft_watermark_triggers
                        .fetch_add(1, Ordering::Relaxed);
                }
                (true, dev_ptr_base)
            } else {
                self.release_reserved_bytes(len_u64);
                (false, None)
            }
        } else {
            (false, None)
        };

        {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.seq = state.seq.wrapping_add(1);
            let seq = state.seq;
            let prev = state.spans.insert(
                (base_addr, len),
                SpanEntry {
                    refcount: 1,
                    registered,
                    dev_ptr_base,
                    last_used_seq: seq,
                },
            );
            if registered {
                state.registered_spans.insert(base_addr, len);
            } else if prev.is_some_and(|e| e.registered) {
                state.registered_spans.remove(&base_addr);
            }
            if prev.is_some() {
                // Pre-reserved span_count only applies to newly inserted keys.
                let _ = self.inner.quota_slot.span_count.try_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |cur| Some(cur.saturating_sub(1)),
                );
            }
        }

        Ok(PinnedSpanGuard {
            inner: Arc::clone(&self.inner),
            key: (base_addr, len),
            status: status_from_registered(registered),
            dev_ptr_base,
        })
    }

    /// `true` if `[addr, addr + len)` lies inside a cached span whose
    /// `cuMemHostRegister_v2` succeeded.
    pub fn is_span_pinned(&self, addr: usize, len: usize) -> bool {
        let Ok(end) = validate_span(addr, len) else {
            return false;
        };
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        find_enclosing_key(&state, addr, end)
            .and_then(|k| state.spans.get(&k))
            .is_some_and(|e| e.registered)
    }

    /// `true` if `[addr, addr + len)` is tracked in the cache (either pinned or
    /// cached under [`PinStatus::UnpinnedFallback`]).
    pub fn is_span_cached(&self, addr: usize, len: usize) -> bool {
        let Ok(end) = validate_span(addr, len) else {
            return false;
        };
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        find_enclosing_key(&state, addr, end).is_some()
    }

    /// Current active guard refcount for the exact span `(base_addr, len)`.
    pub fn span_refcount(&self, base_addr: usize, len: usize) -> usize {
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .spans
            .get(&(base_addr, len))
            .map_or(0, |e| e.refcount)
    }

    /// Number of spans currently cached.
    pub fn cached_span_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .spans
            .len()
    }

    /// Unregister and remove `(base_addr, len)` when its refcount is zero.
    ///
    /// Returns `Ok(true)` if removed, `Ok(false)` if not present, or `Err` if
    /// one or more [`PinnedSpanGuard`]s are still active.
    pub fn unregister_span(&self, base_addr: usize, len: usize) -> Result<bool> {
        let removed = {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            let Some(entry) = state.spans.get(&(base_addr, len)) else {
                return Ok(false);
            };
            if entry.refcount > 0 {
                return Err(DeviceError::Driver(format!(
                    "cannot unregister host span ({base_addr:#x}, {len}) with active refcount {}",
                    entry.refcount
                )));
            }
            let removed = state.spans.remove(&(base_addr, len));
            if removed.is_some_and(|e| e.registered) {
                state.registered_spans.remove(&base_addr);
            }
            removed
        };
        if let Some(entry) = removed {
            let _ = self.inner.quota_slot.span_count.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |c| Some(c.saturating_sub(1)),
            );
            if entry.registered {
                let _ = self.inner.quota_slot.pinned_bytes.try_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |c| Some(c.saturating_sub(len as u64)),
                );
                self.inner.unregister_span(base_addr);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Unregister and clear all idle cached spans (entries with active
    /// [`PinnedSpanGuard`]s are retained so live DMA stays pinned).
    pub fn clear(&self) {
        let mut to_unregister = Vec::new();
        let mut removed_spans = 0usize;
        let mut removed_bytes = 0u64;
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            let state = &mut *state;
            let registered_spans = &mut state.registered_spans;
            state.spans.retain(|&(base_addr, len), entry| {
                if entry.refcount == 0 {
                    removed_spans += 1;
                    if entry.registered {
                        removed_bytes = removed_bytes.saturating_add(len as u64);
                        registered_spans.remove(&base_addr);
                        to_unregister.push(base_addr);
                    }
                    false
                } else {
                    true
                }
            });
        }
        if removed_spans > 0 {
            let _ = self.inner.quota_slot.span_count.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |c| Some(c.saturating_sub(removed_spans)),
            );
        }
        if removed_bytes > 0 {
            let _ = self.inner.quota_slot.pinned_bytes.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |c| Some(c.saturating_sub(removed_bytes)),
            );
        }
        for base_addr in to_unregister {
            self.inner.unregister_span(base_addr);
        }
    }
}

fn status_from_registered(registered: bool) -> PinStatus {
    if registered {
        PinStatus::Pinned
    } else {
        PinStatus::UnpinnedFallback
    }
}

fn validate_span(base_addr: usize, len: usize) -> Result<usize> {
    if base_addr == 0 || len == 0 {
        return Err(DeviceError::Memcpy(format!(
            "invalid host pin span: base_addr={base_addr:#x} len={len}"
        )));
    }
    base_addr.checked_add(len).ok_or_else(|| {
        DeviceError::Memcpy(format!(
            "host pin span overflows address space: base_addr={base_addr:#x} len={len}"
        ))
    })
}

fn find_enclosing_key(state: &PinCacheState, addr: usize, end: usize) -> Option<(usize, usize)> {
    // Prefer an actually pinned (`registered == true`) enclosing span in O(log N)
    // via the disjoint `registered_spans` index before scanning unpinned fallbacks.
    if let Some((&s_base, &s_len)) = state.registered_spans.range(..=addr).next_back() {
        if s_len > 0 && s_base.checked_add(s_len).is_some_and(|s_end| end <= s_end) {
            return Some((s_base, s_len));
        }
    }
    state
        .spans
        .range(..=(addr, usize::MAX))
        .rev()
        .find(|(&(s_base, s_len), _)| {
            s_base <= addr && s_base.checked_add(s_len).is_some_and(|s_end| end <= s_end)
        })
        .map(|(&key, _)| key)
}

fn has_overlapping_registered_span(state: &PinCacheState, addr: usize, end: usize) -> bool {
    state
        .registered_spans
        .range(..end)
        .next_back()
        .is_some_and(|(&s_base, &s_len)| {
            s_base.checked_add(s_len).is_some_and(|s_end| s_end > addr)
        })
}

fn try_register_host_span(
    ctx: Option<&DeviceContext>,
    base_addr: usize,
    len: usize,
) -> (bool, Option<u64>) {
    #[cfg(feature = "cuda")]
    {
        if let Some(ctx) = ctx {
            if ctx.inner().bind_to_thread().is_err() {
                return (false, None);
            }
        }
        let host_ptr = base_addr as *mut std::ffi::c_void;
        // SAFETY: `base_addr` and `len` were validated non-zero and non-overflowing;
        // `PORTABLE | DEVICEMAP` enables zero-copy device pointer mapping on APUs/unified
        // hosts, with clean fallback to `PORTABLE` or unpinned if declined.
        let (status, mapped) = unsafe {
            let sys = cudarc::driver::sys::lib();
            let s = sys.cuMemHostRegister_v2(
                host_ptr,
                len,
                cudarc::driver::sys::CU_MEMHOSTREGISTER_PORTABLE
                    | cudarc::driver::sys::CU_MEMHOSTREGISTER_DEVICEMAP,
            );
            if s == cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                (s, true)
            } else {
                (
                    sys.cuMemHostRegister_v2(
                        host_ptr,
                        len,
                        cudarc::driver::sys::CU_MEMHOSTREGISTER_PORTABLE,
                    ),
                    false,
                )
            }
        };
        if status == cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            cratonvm_types::gpu_event_census::note_host_pin_cache_register();
            let mut dev_ptr: cudarc::driver::sys::CUdeviceptr = 0;
            // SAFETY: `host_ptr` was just registered with `DEVICEMAP` and `dev_ptr` is a valid out-param.
            let dev_ptr_base = if mapped
                && unsafe {
                    cudarc::driver::sys::lib().cuMemHostGetDevicePointer_v2(&mut dev_ptr, host_ptr, 0)
                } == cudarc::driver::sys::CUresult::CUDA_SUCCESS
            {
                Some(dev_ptr as u64)
            } else {
                None
            };
            return (true, dev_ptr_base);
        }
        tracing::debug!(
            base_addr = format_args!("{base_addr:#x}"),
            len,
            status = ?status,
            "HostRegionPinCache: cuMemHostRegister_v2 declined, degrading to unpinned fallback"
        );
        (false, None)
    }
    #[cfg(feature = "hip")]
    {
        if !cubecl_hip_sys::is_available() {
            return (false, None);
        }
        if let Some(ctx) = ctx {
            if ctx.inner().bind_to_thread().is_err() {
                return (false, None);
            }
        }
        let host_ptr = base_addr as *mut std::ffi::c_void;
        // SAFETY: `base_addr` and `len` were validated non-zero and non-overflowing;
        // `hipHostRegisterPortable | hipHostRegisterMapped` exposes system RAM to RDNA/APU kernels.
        let mut status = unsafe {
            cubecl_hip_sys::hipHostRegister(
                host_ptr,
                len,
                cubecl_hip_sys::hipHostRegisterPortable | cubecl_hip_sys::hipHostRegisterMapped,
            )
        };
        if status != cubecl_hip_sys::hipError_t_hipSuccess {
            // SAFETY: fallback to default flags if portable|mapped is declined by runtime.
            status = unsafe { cubecl_hip_sys::hipHostRegister(host_ptr, len, 0) };
        }
        if status == cubecl_hip_sys::hipError_t_hipSuccess {
            cratonvm_types::gpu_event_census::note_host_pin_cache_register();
            let mut dev_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `host_ptr` was registered with `hipHostRegisterMapped` above and `dev_ptr` is valid.
            let dev_status =
                unsafe { cubecl_hip_sys::hipHostGetDevicePointer(&mut dev_ptr, host_ptr, 0) };
            let dev_ptr_base = if dev_status == cubecl_hip_sys::hipError_t_hipSuccess
                && !dev_ptr.is_null()
            {
                Some(dev_ptr as usize as u64)
            } else {
                None
            };
            return (true, dev_ptr_base);
        }
        tracing::debug!(
            base_addr = format_args!("{base_addr:#x}"),
            len,
            status = ?status,
            "HostRegionPinCache: hipHostRegister declined, degrading to unpinned fallback"
        );
        (false, None)
    }
    #[cfg(not(any(feature = "cuda", feature = "hip")))]
    {
        let _ = (ctx, base_addr, len);
        (false, None)
    }
}

fn try_unregister_host_span(ctx: Option<&DeviceContext>, base_addr: usize) {
    #[cfg(feature = "cuda")]
    {
        if let Some(ctx) = ctx {
            if ctx.inner().bind_to_thread().is_err() {
                return;
            }
        }
        // SAFETY: `base_addr` was registered by `cuMemHostRegister_v2` in this
        // cache and is unregistered at most once when removed or cleared.
        unsafe {
            let _ = cudarc::driver::sys::lib()
                .cuMemHostUnregister(base_addr as *mut std::ffi::c_void);
        }
    }
    #[cfg(feature = "hip")]
    {
        if !cubecl_hip_sys::is_available() {
            return;
        }
        if let Some(ctx) = ctx {
            if ctx.inner().bind_to_thread().is_err() {
                return;
            }
        }
        // SAFETY: `base_addr` was registered by `hipHostRegister` in this cache
        // and is unregistered at most once when removed or cleared.
        unsafe {
            let _ = cubecl_hip_sys::hipHostUnregister(base_addr as *mut std::ffi::c_void);
        }
    }
    #[cfg(not(any(feature = "cuda", feature = "hip")))]
    {
        let _ = (ctx, base_addr);
    }
}

#[cfg(test)]
mod tests;
