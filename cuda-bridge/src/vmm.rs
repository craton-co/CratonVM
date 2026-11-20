//! CUDA Driver Virtual Memory Management (VMM) slab backend.
//!
//! Manages 2 MiB-aligned physical allocations (`CUmemGenericAllocationHandle`)
//! mapped into contiguous virtual address reservations (`cuMemAddressReserve` +
//! `cuMemMap` + `cuMemSetAccess`). Unused trailing or freed chunks can be
//! unmapped and released independently without relocating active device
//! pointers, and fragmented free physical chunks can be unmapped and remapped
//! contiguously into a new VA range via [`VmmPool::defragment_or_remap`].
//!
//! When the active CUDA device does not report
//! `CU_DEVICE_ATTRIBUTE_VIRTUAL_MEMORY_MANAGEMENT_SUPPORTED` or when VMM is
//! disabled via [`set_vmm_enabled(false)`], callers fall back cleanly to
//! standard `cuMemAlloc` / `cuMemFree`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::{DeviceError, Result};

/// Minimum VMM chunk alignment (2 MiB), matching CUDA large-page VMM granularity.
pub const DEFAULT_VMM_CHUNK_BYTES: usize = 2 * 1024 * 1024;

/// Default VA reservation window size per VMM region (64 MiB).
pub const DEFAULT_VMM_RESERVE_BYTES: usize = 64 * 1024 * 1024;

/// Default per-stream scattered idle-chunk threshold before automatic VMM
/// defragmentation/park compacts fragmented VA chunks on that stream.
pub const DEFAULT_STREAM_DEFRAG_WATERMARK_CHUNKS: usize = 8;

/// Default minimum number of 2 MiB physical handles asynchronously pre-warmed
/// into `parked_handles` at context boot (`4` = 8 MiB).
pub const DEFAULT_VMM_PREWARM_CHUNKS: usize = 4;

static VMM_ENABLED: AtomicBool = AtomicBool::new(true);
static VMM_PREWARM_MIN_CHUNKS: AtomicUsize = AtomicUsize::new(DEFAULT_VMM_PREWARM_CHUNKS);

/// Enable or disable CUDA Driver VMM globally for newly created allocators.
///
/// Even when enabled, devices that do not support VMM fall back to
/// `cuMemAlloc` / `cuMemFree`.
pub fn set_vmm_enabled(enabled: bool) {
    VMM_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Returns whether CUDA Driver VMM is currently enabled.
pub fn vmm_enabled() -> bool {
    VMM_ENABLED.load(Ordering::Relaxed)
}

/// Configure the minimum number of VMM physical chunks asynchronously
/// pre-warmed when initializing new CUDA contexts (`0` = disabled).
pub fn set_vmm_prewarm_min_chunks(min_chunks: usize) {
    VMM_PREWARM_MIN_CHUNKS.store(min_chunks, Ordering::Relaxed);
}

/// Configured minimum number of VMM physical chunks asynchronously pre-warmed
/// at context boot.
pub fn vmm_prewarm_min_chunks() -> usize {
    VMM_PREWARM_MIN_CHUNKS.load(Ordering::Relaxed)
}

/// Round `value` up to a multiple of `granularity` (which must be > 0).
#[inline]
pub fn align_up(value: usize, granularity: usize) -> Option<usize> {
    if granularity == 0 {
        return None;
    }
    if value == 0 {
        return Some(0);
    }
    let rem = value % granularity;
    if rem == 0 {
        Some(value)
    } else {
        value.checked_add(granularity - rem)
    }
}

/// Hardware VMM capabilities probed for a CUDA device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmmSupport {
    /// Whether the device supports CUDA virtual memory management.
    pub supported: bool,
    /// Minimum physical allocation granularity in bytes (`0` if unsupported).
    pub min_granularity: usize,
    /// Recommended physical allocation granularity in bytes (`0` if unsupported).
    pub recommended_granularity: usize,
    /// Effective 2 MiB-aligned chunk size used for VMM slabs.
    pub chunk_bytes: usize,
}

impl Default for VmmSupport {
    fn default() -> Self {
        Self {
            supported: false,
            min_granularity: 0,
            recommended_granularity: 0,
            chunk_bytes: DEFAULT_VMM_CHUNK_BYTES,
        }
    }
}

/// Low-level driver operations backing [`VmmPool`].
///
/// Abstracted over a trait so both the real CUDA 12 Driver VMM implementation
/// and pure-CPU test drivers share the exact same chunk mapping, partial
/// unmapping, and defragmentation logic.
pub trait VmmDriver: Send + Sync {
    /// Opaque handle to a physical allocation chunk.
    type Handle: Copy + Clone + Eq + std::fmt::Debug + Send + Sync;

    /// Chunk granularity in bytes (guaranteed to be a multiple of 2 MiB on CUDA).
    fn granularity(&self) -> usize;

    /// Reserve a contiguous virtual address range of `size` bytes aligned to `alignment`.
    fn reserve_va(&self, size: usize, alignment: usize) -> Result<u64>;

    /// Release a previously reserved virtual address range `[va, va + size)`.
    fn free_va(&self, va: u64, size: usize) -> Result<()>;

    /// Create a physical allocation handle of `size` bytes.
    fn create_physical(&self, size: usize) -> Result<Self::Handle>;

    /// Release a physical allocation handle.
    fn release_physical(&self, handle: Self::Handle) -> Result<()>;

    /// Map `[va, va + size)` to physical `handle` (offset 0).
    fn map(&self, va: u64, size: usize, handle: Self::Handle) -> Result<()>;

    /// Grant read/write access on the active device for `[va, va + size)`.
    fn set_access(&self, va: u64, size: usize) -> Result<()>;

    /// Unmap `[va, va + size)` from its backing physical handle.
    fn unmap(&self, va: u64, size: usize) -> Result<()>;

    /// Ensure any thread-local driver context is bound on the calling worker thread.
    fn bind_context(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState<H> {
    /// VA slot has never been mapped or was unmapped and returned.
    Unmapped,
    /// VA slot is mapped to `handle` and currently backing live suballocator ranges.
    Active(H),
    /// VA slot is mapped to `handle`, but its entire chunk is idle and may be
    /// unmapped/released or remapped during defragmentation without moving live pointers.
    IdleMapped(H),
}

#[derive(Debug)]
struct VmmRegion<H> {
    base_va: u64,
    reserved_bytes: usize,
    chunks: Vec<ChunkState<H>>,
    /// Owning stream id (`0` = default/unscoped) for each chunk slot so
    /// per-stream fragmentation watermarks operate independently across streams.
    chunk_streams: Vec<u64>,
}

impl<H: Copy + Eq> VmmRegion<H> {
    fn all_unmapped(&self) -> bool {
        self.chunks
            .iter()
            .all(|c| matches!(c, ChunkState::Unmapped))
    }

    fn find_contiguous_unmapped(&self, count: usize) -> Option<usize> {
        if count == 0 || count > self.chunks.len() {
            return None;
        }
        let mut run = 0usize;
        for (idx, state) in self.chunks.iter().enumerate() {
            if matches!(state, ChunkState::Unmapped) {
                run += 1;
                if run == count {
                    return Some(idx + 1 - count);
                }
            } else {
                run = 0;
            }
        }
        None
    }

    fn find_contiguous_idle_mapped(&self, count: usize) -> Option<usize> {
        if count == 0 || count > self.chunks.len() {
            return None;
        }
        let mut run = 0usize;
        for (idx, state) in self.chunks.iter().enumerate() {
            if matches!(state, ChunkState::IdleMapped(_)) {
                run += 1;
                if run == count {
                    return Some(idx + 1 - count);
                }
            } else {
                run = 0;
            }
        }
        None
    }
}

/// Description of a contiguous mapped VMM slab returned to the suballocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmmSlab {
    /// Base device virtual address of the mapped slab.
    pub base_ptr: u64,
    /// Total mapped byte length (multiple of `chunk_bytes`).
    pub size_bytes: usize,
    /// Number of backing chunks in `[base_ptr, base_ptr + size_bytes)`.
    pub chunk_count: usize,
}

/// Summary snapshot of [`VmmPool`] state for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmmStats {
    /// Chunk size in bytes.
    pub chunk_bytes: usize,
    /// Total VA bytes currently reserved across all regions.
    pub reserved_va_bytes: usize,
    /// Physical bytes currently mapped into VA space (active + idle).
    pub mapped_physical_bytes: usize,
    /// Physical bytes currently parked unmapped in the handle cache.
    pub parked_physical_bytes: usize,
    /// Number of reserved VA regions.
    pub region_count: usize,
    /// Total remap operations performed.
    pub remap_count: u64,
    /// Currently mapped active chunks.
    pub active_chunks: usize,
    /// Currently mapped idle chunks awaiting reuse or compaction.
    pub idle_mapped_chunks: usize,
    /// Parked physical handles ready for zero-`cuMemCreate` mapping.
    pub parked_chunks: usize,
    /// Unmapped VA chunk slots inside reserved regions.
    pub unmapped_chunks: usize,
    /// Number of per-stream VMM defragmentation watermark triggers.
    pub defrag_watermark_hits: u64,
    /// Total physical handles asynchronously pre-warmed into the parked handle cache.
    pub prewarmed_chunks: u64,
}

struct VmmState<H> {
    regions: Vec<VmmRegion<H>>,
    /// Map `base_va -> region index` for fast lookup.
    va_index: BTreeMap<u64, usize>,
    /// Unmapped physical handles retained for zero-`cuMemCreate` remapping.
    parked_handles: Vec<H>,
    remap_count: u64,
    defrag_watermark_hits: u64,
    prewarmed_chunks: u64,
    default_defrag_watermark: usize,
    stream_watermarks: BTreeMap<u64, usize>,
    reported_census: (i64, i64, i64, i64),
}

impl<H> VmmState<H> {
    fn new() -> Self {
        Self {
            regions: Vec::new(),
            va_index: BTreeMap::new(),
            parked_handles: Vec::new(),
            remap_count: 0,
            defrag_watermark_hits: 0,
            prewarmed_chunks: 0,
            default_defrag_watermark: DEFAULT_STREAM_DEFRAG_WATERMARK_CHUNKS,
            stream_watermarks: BTreeMap::new(),
            reported_census: (0, 0, 0, 0),
        }
    }

    fn count_chunks(&self) -> (usize, usize, usize, usize) {
        let mut active = 0usize;
        let mut idle_mapped = 0usize;
        let mut unmapped = 0usize;
        for r in &self.regions {
            for c in &r.chunks {
                match c {
                    ChunkState::Active(_) => active += 1,
                    ChunkState::IdleMapped(_) => idle_mapped += 1,
                    ChunkState::Unmapped => unmapped += 1,
                }
            }
        }
        (active, idle_mapped, self.parked_handles.len(), unmapped)
    }

    fn sync_census(&mut self) {
        let (a, i, p, u) = self.count_chunks();
        let next = (a as i64, i as i64, p as i64, u as i64);
        let da = next.0 - self.reported_census.0;
        let di = next.1 - self.reported_census.1;
        let dp = next.2 - self.reported_census.2;
        let du = next.3 - self.reported_census.3;
        if da != 0 || di != 0 || dp != 0 || du != 0 {
            cratonvm_types::gpu_event_census::apply_vmm_chunk_deltas(da, di, dp, du);
            self.reported_census = next;
        }
    }

    fn clear_census(&mut self) {
        let (ra, ri, rp, ru) = self.reported_census;
        if ra != 0 || ri != 0 || rp != 0 || ru != 0 {
            cratonvm_types::gpu_event_census::apply_vmm_chunk_deltas(-ra, -ri, -rp, -ru);
            self.reported_census = (0, 0, 0, 0);
        }
    }

    fn watermark_for_stream(&self, stream_key: u64) -> usize {
        self.stream_watermarks
            .get(&stream_key)
            .copied()
            .unwrap_or(self.default_defrag_watermark)
    }

    fn region_for_va_mut(&mut self, va: u64) -> Option<(&mut VmmRegion<H>, usize)> {
        let (&base, &idx) = self.va_index.range(..=va).next_back()?;
        let region = self.regions.get_mut(idx)?;
        let end = base.saturating_add(region.reserved_bytes as u64);
        if va >= base && va < end {
            Some((region, idx))
        } else {
            None
        }
    }

    fn rebuild_index(&mut self) {
        self.va_index.clear();
        for (idx, r) in self.regions.iter().enumerate() {
            self.va_index.insert(r.base_va, idx);
        }
    }
}

/// Manages VMM VA reservations, physical chunk allocation/mapping, partial
/// chunk release, and non-moving VA defragmentation/remapping.
pub struct VmmPool<D: VmmDriver> {
    driver: Arc<D>,
    chunk_bytes: usize,
    default_reserve_bytes: usize,
    vm_identity: u64,
    device_ordinal: u32,
    state: Arc<Mutex<VmmState<D::Handle>>>,
    prewarm_shutdown: Arc<AtomicBool>,
    prewarm_threads: Mutex<Vec<JoinHandle<()>>>,
}

impl<D: VmmDriver> VmmPool<D> {
    /// Create a new VMM pool around `driver`.
    pub fn new(driver: D, default_reserve_bytes: usize) -> Self {
        Self::new_for_vm(driver, default_reserve_bytes, 0, 0)
    }

    /// Create a new VMM pool around `driver` scoped to `(vm_identity, device_ordinal)`
    /// for shared VRAM budget accounting on graph arenas.
    pub fn new_for_vm(
        driver: D,
        default_reserve_bytes: usize,
        vm_identity: u64,
        device_ordinal: u32,
    ) -> Self {
        let chunk_bytes = align_up(
            driver.granularity().max(DEFAULT_VMM_CHUNK_BYTES),
            driver.granularity().max(1),
        )
        .unwrap_or(DEFAULT_VMM_CHUNK_BYTES);
        let reserve = align_up(
            default_reserve_bytes.max(chunk_bytes),
            chunk_bytes,
        )
        .unwrap_or(chunk_bytes);
        Self {
            driver: Arc::new(driver),
            chunk_bytes,
            default_reserve_bytes: reserve,
            vm_identity,
            device_ordinal,
            state: Arc::new(Mutex::new(VmmState::new())),
            prewarm_shutdown: Arc::new(AtomicBool::new(false)),
            prewarm_threads: Mutex::new(Vec::new()),
        }
    }

    /// Reserve a dedicated graph-scoped VMM virtual range isolated from the
    /// general [`VmmPool`] slabs so repeated graph executions reuse fixed
    /// device addresses without pinning general pool slabs.
    pub fn reserve_graph_arena(&self, capacity_bytes: usize) -> Result<Arc<GraphVmmArena<D>>> {
        GraphVmmArena::new_for_vm(
            Arc::clone(&self.driver),
            self.chunk_bytes,
            capacity_bytes,
            self.vm_identity,
            self.device_ordinal,
        )
    }

    /// Effective chunk granularity in bytes.
    pub fn chunk_bytes(&self) -> usize {
        self.chunk_bytes
    }

    /// Underlying [`VmmDriver`] instance.
    pub fn driver(&self) -> &D {
        &self.driver
    }

    /// Configure the default per-stream idle-chunk fragmentation watermark (`0` = disabled).
    pub fn set_default_defrag_watermark(&self, max_idle_chunks: usize) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.default_defrag_watermark = max_idle_chunks;
    }

    /// Configure a stream-specific idle-chunk fragmentation watermark (`0` = disabled).
    pub fn set_stream_defrag_watermark(&self, stream_id: Option<u64>, max_idle_chunks: usize) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.stream_watermarks
            .insert(stream_id.unwrap_or(0), max_idle_chunks);
    }

    /// Acquire a contiguous mapped slab of at least `min_bytes` bytes.
    ///
    /// Reuses contiguous idle mapped chunks if present, otherwise maps parked
    /// or freshly created physical handles into existing unmapped VA slots (or
    /// a newly reserved VA region).
    pub fn acquire_slab(&self, min_bytes: usize) -> Result<VmmSlab> {
        self.acquire_slab_for_stream(min_bytes, None)
    }

    /// Acquire a contiguous mapped slab tagged with `stream_id` affinity for
    /// per-stream VMM defragmentation watermark accounting.
    pub fn acquire_slab_for_stream(
        &self,
        min_bytes: usize,
        stream_id: Option<u64>,
    ) -> Result<VmmSlab> {
        let stream_key = stream_id.unwrap_or(0);
        let aligned = align_up(min_bytes.max(self.chunk_bytes), self.chunk_bytes)
            .ok_or_else(|| DeviceError::Driver("VMM slab size overflow".into()))?;
        let needed_chunks = aligned / self.chunk_bytes;

        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());

        // Fast path 1: contiguous idle-mapped chunks already backing VA space.
        for region in &mut st.regions {
            if let Some(start) = region.find_contiguous_idle_mapped(needed_chunks) {
                for slot_idx in start..start + needed_chunks {
                    if let ChunkState::IdleMapped(h) = region.chunks[slot_idx] {
                        region.chunks[slot_idx] = ChunkState::Active(h);
                        region.chunk_streams[slot_idx] = stream_key;
                    }
                }
                let base_ptr = region.base_va + (start * self.chunk_bytes) as u64;
                st.sync_census();
                return Ok(VmmSlab {
                    base_ptr,
                    size_bytes: aligned,
                    chunk_count: needed_chunks,
                });
            }
        }

        // Fast path 2: existing region with enough contiguous unmapped VA slots.
        if let Some((reg_idx, start)) = st
            .regions
            .iter()
            .enumerate()
            .find_map(|(i, r)| r.find_contiguous_unmapped(needed_chunks).map(|s| (i, s)))
        {
            let res = self.map_into_region(&mut st, reg_idx, start, needed_chunks, stream_key);
            st.sync_census();
            return res;
        }

        // Path 3: reserve a new VA region and map `needed_chunks` at slot 0.
        let reserve_bytes = self
            .default_reserve_bytes
            .max(aligned)
            .checked_next_multiple_of_checked(self.chunk_bytes)
            .unwrap_or(aligned);
        let region_chunks = reserve_bytes / self.chunk_bytes;
        let base_va = self.driver.reserve_va(reserve_bytes, self.chunk_bytes)?;
        let reg_idx = st.regions.len();
        st.regions.push(VmmRegion {
            base_va,
            reserved_bytes: reserve_bytes,
            chunks: vec![ChunkState::Unmapped; region_chunks],
            chunk_streams: vec![0; region_chunks],
        });
        st.va_index.insert(base_va, reg_idx);

        let res = match self.map_into_region(&mut st, reg_idx, 0, needed_chunks, stream_key) {
            Ok(slab) => Ok(slab),
            Err(err) => {
                // Clean up empty newly reserved VA region on failure.
                if st.regions.get(reg_idx).is_some_and(VmmRegion::all_unmapped) {
                    let removed = st.regions.remove(reg_idx);
                    st.rebuild_index();
                    let _ = self.driver.free_va(removed.base_va, removed.reserved_bytes);
                }
                Err(err)
            }
        };
        st.sync_census();
        res
    }

    fn map_into_region(
        &self,
        st: &mut VmmState<D::Handle>,
        reg_idx: usize,
        start_chunk: usize,
        needed_chunks: usize,
        stream_key: u64,
    ) -> Result<VmmSlab> {
        let base_va = st.regions[reg_idx].base_va + (start_chunk * self.chunk_bytes) as u64;
        let mut acquired: Vec<(usize, D::Handle, bool)> = Vec::with_capacity(needed_chunks);

        for offset in 0..needed_chunks {
            let from_parked = st.parked_handles.pop();
            let (handle, is_parked) = match from_parked {
                Some(h) => (h, true),
                None => match self.driver.create_physical(self.chunk_bytes) {
                    Ok(h) => (h, false),
                    Err(e) => {
                        self.rollback_mapped(st, reg_idx, &acquired);
                        return Err(e);
                    }
                },
            };

            let chunk_va = base_va + (offset * self.chunk_bytes) as u64;
            if let Err(e) = self.driver.map(chunk_va, self.chunk_bytes, handle) {
                if is_parked {
                    st.parked_handles.push(handle);
                } else {
                    let _ = self.driver.release_physical(handle);
                }
                self.rollback_mapped(st, reg_idx, &acquired);
                return Err(e);
            }
            if let Err(e) = self.driver.set_access(chunk_va, self.chunk_bytes) {
                let _ = self.driver.unmap(chunk_va, self.chunk_bytes);
                if is_parked {
                    st.parked_handles.push(handle);
                } else {
                    let _ = self.driver.release_physical(handle);
                }
                self.rollback_mapped(st, reg_idx, &acquired);
                return Err(e);
            }

            let slot_idx = start_chunk + offset;
            st.regions[reg_idx].chunks[slot_idx] = ChunkState::Active(handle);
            st.regions[reg_idx].chunk_streams[slot_idx] = stream_key;
            acquired.push((slot_idx, handle, is_parked));
        }

        Ok(VmmSlab {
            base_ptr: base_va,
            size_bytes: needed_chunks * self.chunk_bytes,
            chunk_count: needed_chunks,
        })
    }

    fn rollback_mapped(
        &self,
        st: &mut VmmState<D::Handle>,
        reg_idx: usize,
        acquired: &[(usize, D::Handle, bool)],
    ) {
        let base = st.regions[reg_idx].base_va;
        for &(slot_idx, handle, is_parked) in acquired.iter().rev() {
            let chunk_va = base + (slot_idx * self.chunk_bytes) as u64;
            let _ = self.driver.unmap(chunk_va, self.chunk_bytes);
            st.regions[reg_idx].chunks[slot_idx] = ChunkState::Unmapped;
            st.regions[reg_idx].chunk_streams[slot_idx] = 0;
            if is_parked {
                st.parked_handles.push(handle);
            } else {
                let _ = self.driver.release_physical(handle);
            }
        }
    }

    /// Mark chunks covering `[va, va + bytes)` as idle-mapped (can be reused
    /// in place, unmapped, or remapped later without moving live chunks).
    pub fn mark_chunks_idle(&self, va: u64, bytes: usize) {
        self.mark_chunks_idle_for_stream(va, bytes, None);
    }

    /// Mark chunks covering `[va, va + bytes)` as idle-mapped under `stream_id`
    /// and check that stream's per-stream VMM defragmentation watermark.
    ///
    /// Returns `true` when the stream's idle chunk count crossed the configured
    /// watermark and scattered idle chunks on that stream were compacted into
    /// `parked_handles`.
    pub fn mark_chunks_idle_for_stream(
        &self,
        va: u64,
        bytes: usize,
        stream_id: Option<u64>,
    ) -> bool {
        if bytes == 0 {
            return false;
        }
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let chunk_bytes = self.chunk_bytes;
        let Some((region, _)) = st.region_for_va_mut(va) else {
            return false;
        };
        let rel = (va - region.base_va) as usize;
        if rel % chunk_bytes != 0 || bytes % chunk_bytes != 0 {
            return false;
        }
        let start = rel / chunk_bytes;
        let count = bytes / chunk_bytes;
        let end = (start + count).min(region.chunks.len());
        let mut touched_stream = stream_id;
        for idx in start..end {
            if let ChunkState::Active(h) = region.chunks[idx] {
                region.chunks[idx] = ChunkState::IdleMapped(h);
                if let Some(s) = stream_id {
                    region.chunk_streams[idx] = s;
                } else if touched_stream.is_none() {
                    touched_stream = Some(region.chunk_streams[idx]);
                }
            }
        }
        let stream_key = touched_stream.unwrap_or(0);
        let triggered = self.compact_stream_if_over_watermark(&mut st, stream_key);
        st.sync_census();
        triggered
    }

    fn compact_stream_if_over_watermark(
        &self,
        st: &mut VmmState<D::Handle>,
        stream_key: u64,
    ) -> bool {
        let watermark = st.watermark_for_stream(stream_key);
        if watermark == 0 {
            return false;
        }
        let idle_for_stream: usize = st
            .regions
            .iter()
            .map(|r| {
                r.chunks
                    .iter()
                    .zip(r.chunk_streams.iter())
                    .filter(|(c, s)| matches!(c, ChunkState::IdleMapped(_)) && **s == stream_key)
                    .count()
            })
            .sum();
        if idle_for_stream < watermark {
            return false;
        }

        let mut parked_any = false;
        for reg_idx in 0..st.regions.len() {
            let region_base = st.regions[reg_idx].base_va;
            for idx in 0..st.regions[reg_idx].chunks.len() {
                if st.regions[reg_idx].chunk_streams[idx] == stream_key {
                    if let ChunkState::IdleMapped(h) = st.regions[reg_idx].chunks[idx] {
                        let chunk_va = region_base + (idx * self.chunk_bytes) as u64;
                        if self.driver.unmap(chunk_va, self.chunk_bytes).is_ok() {
                            st.regions[reg_idx].chunks[idx] = ChunkState::Unmapped;
                            st.regions[reg_idx].chunk_streams[idx] = 0;
                            st.parked_handles.push(h);
                            parked_any = true;
                        }
                    }
                }
            }
        }
        if parked_any {
            let mut reg_idx = st.regions.len();
            while reg_idx > 0 {
                reg_idx -= 1;
                if st.regions[reg_idx].all_unmapped() {
                    let removed = st.regions.remove(reg_idx);
                    let _ = self.driver.free_va(removed.base_va, removed.reserved_bytes);
                }
            }
            st.rebuild_index();
            st.defrag_watermark_hits += 1;
            cratonvm_types::gpu_event_census::note_vmm_defrag_watermark();
        }
        parked_any
    }

    /// Unmap chunks covering `[va, va + bytes)`.
    ///
    /// When `release_physical` is `true`, calls `cuMemRelease` to return
    /// physical VRAM back to the driver; otherwise parks the physical handle
    /// for cheap remapping via [`Self::defragment_or_remap`] or subsequent
    /// [`Self::acquire_slab`] calls.
    pub fn unmap_chunks(&self, va: u64, bytes: usize, release_physical: bool) -> usize {
        if bytes == 0 {
            return 0;
        }
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let chunk_bytes = self.chunk_bytes;
        let Some((region, reg_idx)) = st.region_for_va_mut(va) else {
            return 0;
        };
        let rel = (va - region.base_va) as usize;
        if rel % chunk_bytes != 0 || bytes % chunk_bytes != 0 {
            return 0;
        }
        let start = rel / chunk_bytes;
        let count = bytes / chunk_bytes;
        let end = (start + count).min(region.chunks.len());
        let region_base = region.base_va;
        let mut unmapped_bytes = 0usize;
        let mut to_park = Vec::new();

        for idx in start..end {
            let handle = match st.regions[reg_idx].chunks[idx] {
                ChunkState::Active(h) | ChunkState::IdleMapped(h) => h,
                ChunkState::Unmapped => continue,
            };
            let chunk_va = region_base + (idx * chunk_bytes) as u64;
            if self.driver.unmap(chunk_va, chunk_bytes).is_ok() {
                st.regions[reg_idx].chunks[idx] = ChunkState::Unmapped;
                st.regions[reg_idx].chunk_streams[idx] = 0;
                unmapped_bytes += chunk_bytes;
                if release_physical {
                    let _ = self.driver.release_physical(handle);
                } else {
                    to_park.push(handle);
                }
            }
        }

        st.parked_handles.extend(to_park);

        // If an entire VA region became unmapped and we have > 1 region, free
        // the trailing empty VA reservation too so VA address space stays tidy.
        if st.regions.len() > 1 && st.regions[reg_idx].all_unmapped() {
            let removed = st.regions.remove(reg_idx);
            st.rebuild_index();
            let _ = self.driver.free_va(removed.base_va, removed.reserved_bytes);
        }

        st.sync_census();
        unmapped_bytes
    }

    /// Release all parked unmapped physical handles back to the driver and trim
    /// any completely unmapped VA regions (keeping at most one empty VA region
    /// when `keep_one_empty_va` is true).
    pub fn release_unused_chunks(&self, keep_one_empty_va: bool) -> usize {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut released_bytes = 0usize;

        // First unmap & release all IdleMapped chunks without moving Active chunks.
        let mut empty_regions = Vec::new();
        for reg_idx in 0..st.regions.len() {
            let region_base = st.regions[reg_idx].base_va;
            for idx in 0..st.regions[reg_idx].chunks.len() {
                if let ChunkState::IdleMapped(h) = st.regions[reg_idx].chunks[idx] {
                    let chunk_va = region_base + (idx * self.chunk_bytes) as u64;
                    if self.driver.unmap(chunk_va, self.chunk_bytes).is_ok() {
                        st.regions[reg_idx].chunks[idx] = ChunkState::Unmapped;
                        st.regions[reg_idx].chunk_streams[idx] = 0;
                        let _ = self.driver.release_physical(h);
                        released_bytes += self.chunk_bytes;
                    }
                }
            }
            if st.regions[reg_idx].all_unmapped() {
                empty_regions.push(reg_idx);
            }
        }

        while let Some(h) = st.parked_handles.pop() {
            let _ = self.driver.release_physical(h);
            released_bytes += self.chunk_bytes;
        }

        // Remove empty regions from back to front.
        let keep = usize::from(keep_one_empty_va);
        while empty_regions.len() > keep {
            if let Some(reg_idx) = empty_regions.pop() {
                let removed = st.regions.remove(reg_idx);
                let _ = self.driver.free_va(removed.base_va, removed.reserved_bytes);
            }
        }
        st.rebuild_index();

        st.sync_census();
        released_bytes
    }

    /// Gather scattered idle-mapped and parked physical chunks, unmap them
    /// without touching any active chunks or moving active pointers, and remap
    /// at least `min_chunks` (or all gatherable chunks) contiguously into a
    /// VA range (`note_vmm_remap()`).
    ///
    /// Returns `Ok(Some(slab))` containing the newly remapped contiguous VA
    /// slab marked `Active` when enough idle/parked chunks were gathered, or
    /// `Ok(None)` if fewer than `min_chunks` idle/parked chunks exist.
    pub fn defragment_or_remap(&self, min_chunks: usize) -> Result<Option<VmmSlab>> {
        let min_chunks = min_chunks.max(1);
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());

        // Fast check: how many idle-mapped + parked handles are available?
        let idle_count: usize = st
            .regions
            .iter()
            .flat_map(|r| r.chunks.iter())
            .filter(|c| matches!(c, ChunkState::IdleMapped(_)))
            .count();
        let total_avail = idle_count + st.parked_handles.len();
        if total_avail < min_chunks {
            return Ok(None);
        }

        // Unmap only as many scattered IdleMapped chunks as needed to reach
        // `min_chunks`, leaving excess mapped idle chunks ready for fast reuse.
        'unmap: for reg_idx in 0..st.regions.len() {
            if st.parked_handles.len() >= min_chunks {
                break;
            }
            let region_base = st.regions[reg_idx].base_va;
            for idx in 0..st.regions[reg_idx].chunks.len() {
                if st.parked_handles.len() >= min_chunks {
                    break 'unmap;
                }
                if let ChunkState::IdleMapped(h) = st.regions[reg_idx].chunks[idx] {
                    let chunk_va = region_base + (idx * self.chunk_bytes) as u64;
                    if let Err(e) = self.driver.unmap(chunk_va, self.chunk_bytes) {
                        st.sync_census();
                        return Err(e);
                    }
                    st.regions[reg_idx].chunks[idx] = ChunkState::Unmapped;
                    st.regions[reg_idx].chunk_streams[idx] = 0;
                    st.parked_handles.push(h);
                }
            }
        }

        let target_chunks = st.parked_handles.len().min(min_chunks);
        if target_chunks < min_chunks {
            st.sync_census();
            return Ok(None);
        }

        // Clean out any regions that became wholly unmapped before reserving/finding a contiguous window.
        let mut idx = st.regions.len();
        while idx > 0 {
            idx -= 1;
            if st.regions[idx].all_unmapped() {
                let removed = st.regions.remove(idx);
                let _ = self.driver.free_va(removed.base_va, removed.reserved_bytes);
            }
        }
        st.rebuild_index();

        // Find existing unmapped contiguous VA run or reserve a fresh VA range.
        let (reg_idx, start_chunk) = if let Some(found) = st
            .regions
            .iter()
            .enumerate()
            .find_map(|(i, r)| r.find_contiguous_unmapped(target_chunks).map(|s| (i, s)))
        {
            found
        } else {
            let needed_bytes = target_chunks * self.chunk_bytes;
            let reserve_bytes = self
                .default_reserve_bytes
                .max(needed_bytes)
                .checked_next_multiple_of_checked(self.chunk_bytes)
                .unwrap_or(needed_bytes);
            let region_chunks = reserve_bytes / self.chunk_bytes;
            let base_va = match self.driver.reserve_va(reserve_bytes, self.chunk_bytes) {
                Ok(va) => va,
                Err(e) => {
                    st.sync_census();
                    return Err(e);
                }
            };
            let new_idx = st.regions.len();
            st.regions.push(VmmRegion {
                base_va,
                reserved_bytes: reserve_bytes,
                chunks: vec![ChunkState::Unmapped; region_chunks],
                chunk_streams: vec![0; region_chunks],
            });
            st.va_index.insert(base_va, new_idx);
            (new_idx, 0)
        };

        let base_va = st.regions[reg_idx].base_va + (start_chunk * self.chunk_bytes) as u64;
        let mut mapped: Vec<(usize, D::Handle)> = Vec::with_capacity(target_chunks);

        for offset in 0..target_chunks {
            let Some(handle) = st.parked_handles.pop() else {
                break;
            };
            let chunk_va = base_va + (offset * self.chunk_bytes) as u64;
            if let Err(e) = self.driver.map(chunk_va, self.chunk_bytes, handle) {
                st.parked_handles.push(handle);
                for &(slot_idx, h) in mapped.iter().rev() {
                    let va = st.regions[reg_idx].base_va + (slot_idx * self.chunk_bytes) as u64;
                    let _ = self.driver.unmap(va, self.chunk_bytes);
                    st.regions[reg_idx].chunks[slot_idx] = ChunkState::Unmapped;
                    st.regions[reg_idx].chunk_streams[slot_idx] = 0;
                    st.parked_handles.push(h);
                }
                st.sync_census();
                return Err(e);
            }
            if let Err(e) = self.driver.set_access(chunk_va, self.chunk_bytes) {
                let _ = self.driver.unmap(chunk_va, self.chunk_bytes);
                st.parked_handles.push(handle);
                for &(slot_idx, h) in mapped.iter().rev() {
                    let va = st.regions[reg_idx].base_va + (slot_idx * self.chunk_bytes) as u64;
                    let _ = self.driver.unmap(va, self.chunk_bytes);
                    st.regions[reg_idx].chunks[slot_idx] = ChunkState::Unmapped;
                    st.regions[reg_idx].chunk_streams[slot_idx] = 0;
                    st.parked_handles.push(h);
                }
                st.sync_census();
                return Err(e);
            }
            let slot_idx = start_chunk + offset;
            st.regions[reg_idx].chunks[slot_idx] = ChunkState::Active(handle);
            st.regions[reg_idx].chunk_streams[slot_idx] = 0;
            mapped.push((slot_idx, handle));
        }

        st.remap_count += 1;
        cratonvm_types::gpu_event_census::note_vmm_remap();
        st.sync_census();

        Ok(Some(VmmSlab {
            base_ptr: base_va,
            size_bytes: mapped.len() * self.chunk_bytes,
            chunk_count: mapped.len(),
        }))
    }

    /// Snapshot current VMM counters for telemetry and tests.
    pub fn stats(&self) -> VmmStats {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let reserved_va_bytes: usize = st.regions.iter().map(|r| r.reserved_bytes).sum();
        let (active_chunks, idle_mapped_chunks, parked_chunks, unmapped_chunks) = st.count_chunks();
        let mapped_chunks = active_chunks + idle_mapped_chunks;
        VmmStats {
            chunk_bytes: self.chunk_bytes,
            reserved_va_bytes,
            mapped_physical_bytes: mapped_chunks * self.chunk_bytes,
            parked_physical_bytes: parked_chunks * self.chunk_bytes,
            region_count: st.regions.len(),
            remap_count: st.remap_count,
            active_chunks,
            idle_mapped_chunks,
            parked_chunks,
            unmapped_chunks,
            defrag_watermark_hits: st.defrag_watermark_hits,
            prewarmed_chunks: st.prewarmed_chunks,
        }
    }
}

impl<D: VmmDriver + 'static> VmmPool<D> {
    /// Asynchronously pre-create up to `min_chunks` 2 MiB physical handles on a
    /// background worker thread (`CratonVmmPrewarm`) and park them in
    /// `parked_handles` so first kernel bursts avoid `cuMemCreate` page-fault stalls.
    pub fn prewarm_async(&self, min_chunks: usize) {
        if min_chunks == 0 || self.prewarm_shutdown.load(Ordering::Acquire) {
            return;
        }
        {
            let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if st.parked_handles.len() >= min_chunks {
                return;
            }
        }
        let driver = Arc::clone(&self.driver);
        let state = Arc::clone(&self.state);
        let shutdown = Arc::clone(&self.prewarm_shutdown);
        let chunk_bytes = self.chunk_bytes;
        let Ok(handle) = std::thread::Builder::new()
            .name("CratonVmmPrewarm".to_string())
            .spawn(move || {
                if driver.bind_context().is_err() {
                    return;
                }
                loop {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    {
                        let st = state.lock().unwrap_or_else(|p| p.into_inner());
                        if st.parked_handles.len() >= min_chunks {
                            break;
                        }
                    }
                    let Ok(phys) = driver.create_physical(chunk_bytes) else {
                        break;
                    };
                    let keep = if shutdown.load(Ordering::Acquire) {
                        false
                    } else {
                        let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
                        if st.parked_handles.len() >= min_chunks {
                            false
                        } else {
                            st.parked_handles.push(phys);
                            st.prewarmed_chunks += 1;
                            st.sync_census();
                            true
                        }
                    };
                    if keep {
                        cratonvm_types::gpu_event_census::note_vmm_prewarm_created(1);
                    } else {
                        let _ = driver.release_physical(phys);
                        break;
                    }
                }
            })
        else {
            return;
        };

        let mut threads = self
            .prewarm_threads
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        threads.retain(|h| !h.is_finished());
        threads.push(handle);
    }

    /// Block until any in-flight asynchronous VMM pre-warm workers finish.
    pub fn wait_for_prewarm(&self) {
        let handles = {
            let mut threads = self
                .prewarm_threads
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *threads)
        };
        for h in handles {
            let _ = h.join();
        }
    }
}

impl<D: VmmDriver> Drop for VmmPool<D> {
    fn drop(&mut self) {
        self.prewarm_shutdown.store(true, Ordering::Release);
        let handles = {
            let threads = self
                .prewarm_threads
                .get_mut()
                .unwrap_or_else(|p| p.into_inner());
            std::mem::take(threads)
        };
        for h in handles {
            let _ = h.join();
        }
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for region in st.regions.drain(..) {
            for (idx, chunk) in region.chunks.into_iter().enumerate() {
                match chunk {
                    ChunkState::Active(h) | ChunkState::IdleMapped(h) => {
                        let va = region.base_va + (idx * self.chunk_bytes) as u64;
                        let _ = self.driver.unmap(va, self.chunk_bytes);
                        let _ = self.driver.release_physical(h);
                    }
                    ChunkState::Unmapped => {}
                }
            }
            let _ = self.driver.free_va(region.base_va, region.reserved_bytes);
        }
        while let Some(h) = st.parked_handles.pop() {
            let _ = self.driver.release_physical(h);
        }
        st.clear_census();
    }
}

/// Type-erased handle to a graph-scoped VMM arena bound to a [`crate::graph::GraphExec`].
pub trait GraphArenaHandle: Send + Sync {
    /// Fixed base virtual address of the graph-scoped reservation.
    fn base_va(&self) -> u64;
    /// Total reserved virtual address span in bytes.
    fn reserved_bytes(&self) -> usize;
    /// High-water allocated bytes inside the graph arena.
    fn used_bytes(&self) -> usize;
    /// Currently mapped physical bytes backing the graph arena.
    fn mapped_bytes(&self) -> usize;
    /// Ensure physical backing is mapped for `[base_va, base_va + used_bytes)`
    /// before launching the graph.
    fn ensure_mapped(&self) -> Result<()>;
    /// Unmap and release physical backing between graph executions while
    /// preserving the fixed virtual address range.
    fn release_physical_keeping_va(&self) -> Result<()>;
}

struct GraphArenaState<H> {
    used_bytes: usize,
    chunks: Vec<Option<H>>,
}

/// Dedicated graph-scoped VMM virtual address arena for intermediate/ephemeral
/// graph buffers. Keeps fixed virtual addresses across repeated graph executions
/// without pinning general [`VmmPool`] slabs.
pub struct GraphVmmArena<D: VmmDriver> {
    driver: Arc<D>,
    base_va: u64,
    reserved_bytes: usize,
    chunk_bytes: usize,
    vm_identity: u64,
    device_ordinal: u32,
    state: Mutex<GraphArenaState<D::Handle>>,
}

impl<D: VmmDriver> GraphVmmArena<D> {
    /// Reserve a dedicated VMM virtual address range of at least `capacity_bytes`.
    pub fn new(driver: Arc<D>, chunk_bytes: usize, capacity_bytes: usize) -> Result<Arc<Self>> {
        Self::new_for_vm(driver, chunk_bytes, capacity_bytes, 0, 0)
    }

    /// Reserve a dedicated VMM virtual address range scoped to `(vm_identity, device_ordinal)`.
    pub fn new_for_vm(
        driver: Arc<D>,
        chunk_bytes: usize,
        capacity_bytes: usize,
        vm_identity: u64,
        device_ordinal: u32,
    ) -> Result<Arc<Self>> {
        let chunk_bytes = chunk_bytes.max(driver.granularity()).max(DEFAULT_VMM_CHUNK_BYTES);
        let reserved_bytes = align_up(capacity_bytes.max(chunk_bytes), chunk_bytes)
            .ok_or_else(|| DeviceError::Driver("graph VMM arena size overflow".into()))?;
        let chunk_count = reserved_bytes / chunk_bytes;
        let base_va = driver.reserve_va(reserved_bytes, chunk_bytes)?;
        Ok(Arc::new(Self {
            driver,
            base_va,
            reserved_bytes,
            chunk_bytes,
            vm_identity,
            device_ordinal,
            state: Mutex::new(GraphArenaState {
                used_bytes: 0,
                chunks: vec![None; chunk_count],
            }),
        }))
    }

    /// Suballocate `bytes` (256-byte aligned) at a fixed virtual address inside
    /// the graph-scoped VMM range, mapping physical chunks on demand.
    pub fn alloc(&self, bytes: usize) -> Result<u64> {
        let aligned = align_up(bytes.max(256), 256)
            .ok_or_else(|| DeviceError::Driver("graph VMM alloc overflow".into()))?;
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let end = st
            .used_bytes
            .checked_add(aligned)
            .ok_or_else(|| DeviceError::Driver("graph VMM arena cursor overflow".into()))?;
        if end > self.reserved_bytes {
            return Err(DeviceError::Driver(format!(
                "graph VMM arena exhausted: requested {aligned} bytes, used {} / {}",
                st.used_bytes, self.reserved_bytes
            )));
        }
        let ptr = self.base_va + st.used_bytes as u64;
        self.map_up_to(&mut st, end)?;
        st.used_bytes = end;
        Ok(ptr)
    }

    /// Reset the bump cursor to `0` so a re-captured graph can reuse the same
    /// fixed virtual address range from its base.
    pub fn reset_cursor(&self) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.used_bytes = 0;
    }

    fn map_up_to(&self, st: &mut GraphArenaState<D::Handle>, byte_end: usize) -> Result<()> {
        let needed_chunks = if byte_end == 0 {
            0
        } else {
            byte_end.div_ceil(self.chunk_bytes)
        };
        for idx in 0..needed_chunks {
            if st.chunks[idx].is_some() {
                continue;
            }
            if !crate::allocator::try_reserve_vram_partition_with_reclaim(
                self.vm_identity,
                self.device_ordinal,
                self.chunk_bytes as u64,
            ) {
                return Err(DeviceError::Driver(format!(
                    "VRAM reservation limit exceeded mapping graph VMM chunk ({} bytes)",
                    self.chunk_bytes
                )));
            }
            let chunk_va = self.base_va + (idx * self.chunk_bytes) as u64;
            let handle = match self.driver.create_physical(self.chunk_bytes) {
                Ok(h) => h,
                Err(e) => {
                    crate::allocator::release_vram_partition(
                        self.vm_identity,
                        self.device_ordinal,
                        self.chunk_bytes as u64,
                    );
                    return Err(e);
                }
            };
            if let Err(e) = self.driver.map(chunk_va, self.chunk_bytes, handle) {
                let _ = self.driver.release_physical(handle);
                crate::allocator::release_vram_partition(
                    self.vm_identity,
                    self.device_ordinal,
                    self.chunk_bytes as u64,
                );
                return Err(e);
            }
            if let Err(e) = self.driver.set_access(chunk_va, self.chunk_bytes) {
                let _ = self.driver.unmap(chunk_va, self.chunk_bytes);
                let _ = self.driver.release_physical(handle);
                crate::allocator::release_vram_partition(
                    self.vm_identity,
                    self.device_ordinal,
                    self.chunk_bytes as u64,
                );
                return Err(e);
            }
            st.chunks[idx] = Some(handle);
        }
        Ok(())
    }
}

impl<D: VmmDriver> GraphArenaHandle for GraphVmmArena<D> {
    fn base_va(&self) -> u64 {
        self.base_va
    }

    fn reserved_bytes(&self) -> usize {
        self.reserved_bytes
    }

    fn used_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .used_bytes
    }

    fn mapped_bytes(&self) -> usize {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.chunks.iter().filter(|c| c.is_some()).count() * self.chunk_bytes
    }

    fn ensure_mapped(&self) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let end = st.used_bytes;
        self.map_up_to(&mut st, end)
    }

    fn release_physical_keeping_va(&self) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut released_bytes = 0u64;
        let mut first_err = None;
        for (idx, slot) in st.chunks.iter_mut().enumerate() {
            if let Some(handle) = slot.take() {
                let chunk_va = self.base_va + (idx * self.chunk_bytes) as u64;
                if let Err(e) = self
                    .driver
                    .unmap(chunk_va, self.chunk_bytes)
                    .and_then(|()| self.driver.release_physical(handle))
                {
                    first_err = Some(e);
                    break;
                }
                released_bytes = released_bytes.saturating_add(self.chunk_bytes as u64);
            }
        }
        drop(st);
        if released_bytes > 0 {
            crate::allocator::release_vram_partition(
                self.vm_identity,
                self.device_ordinal,
                released_bytes,
            );
            crate::allocator::notify_vram_freed();
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl<D: VmmDriver> Drop for GraphVmmArena<D> {
    fn drop(&mut self) {
        let _ = self.release_physical_keeping_va();
        let _ = self.driver.free_va(self.base_va, self.reserved_bytes);
    }
}

trait CheckedNextMultiple {
    fn checked_next_multiple_of_checked(self, rhs: usize) -> Option<usize>;
}

impl CheckedNextMultiple for usize {
    #[inline]
    fn checked_next_multiple_of_checked(self, rhs: usize) -> Option<usize> {
        align_up(self, rhs)
    }
}

// ---------------------------------------------------------------------------
// CUDA 12 Driver VMM implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "cuda")]
mod cuda_vmm {
    use std::sync::Arc;

    use cudarc::driver::sys;
    use cudarc::driver::CudaDevice;

    use super::{
        align_up, vmm_enabled, VmmDriver, VmmSupport, DEFAULT_VMM_CHUNK_BYTES,
    };
    use crate::{DeviceError, Result};

    #[inline]
    fn check_cu(res: sys::CUresult, stage: &str) -> Result<()> {
        if res == sys::CUresult::CUDA_SUCCESS {
            Ok(())
        } else {
            Err(DeviceError::Driver(format!("{stage}: {res:?}")))
        }
    }

    #[inline]
    fn make_alloc_prop(cu_device: sys::CUdevice) -> sys::CUmemAllocationProp {
        let mut prop = sys::CUmemAllocationProp::default();
        prop.type_ = sys::CUmemAllocationType::CU_MEM_ALLOCATION_TYPE_PINNED;
        prop.location = sys::CUmemLocation {
            type_: sys::CUmemLocationType::CU_MEM_LOCATION_TYPE_DEVICE,
            id: cu_device,
        };
        prop.requestedHandleTypes = sys::CUmemAllocationHandleType::CU_MEM_HANDLE_TYPE_NONE;
        prop
    }

    /// Probe CUDA Driver VMM capability and allocation granularity for a
    /// [`DeviceContext`](crate::DeviceContext).
    pub fn probe_vmm_support_for_context(ctx: &crate::DeviceContext) -> VmmSupport {
        probe_vmm_support(ctx.inner().device())
    }

    /// Probe CUDA Driver VMM capability and allocation granularity on `dev`.
    pub fn probe_vmm_support(dev: &Arc<CudaDevice>) -> VmmSupport {
        if !vmm_enabled() {
            return VmmSupport::default();
        }
        let supported = dev
            .attribute(sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_VIRTUAL_MEMORY_MANAGEMENT_SUPPORTED)
            .map(|v| v > 0)
            .unwrap_or(false);
        if !supported {
            return VmmSupport::default();
        }
        let cu_device = *dev.cu_device();
        let prop = make_alloc_prop(cu_device);
        let mut min_gran: usize = 0;
        let mut rec_gran: usize = 0;
        // SAFETY: `prop` is initialized on stack and points to a valid
        // `CUmemAllocationProp`; `min_gran` and `rec_gran` are valid out-pointers.
        let (r_min, r_rec) = unsafe {
            let lib = sys::lib();
            (
                lib.cuMemGetAllocationGranularity(
                    &mut min_gran,
                    &prop,
                    sys::CUmemAllocationGranularity_flags::CU_MEM_ALLOC_GRANULARITY_MINIMUM,
                ),
                lib.cuMemGetAllocationGranularity(
                    &mut rec_gran,
                    &prop,
                    sys::CUmemAllocationGranularity_flags::CU_MEM_ALLOC_GRANULARITY_RECOMMENDED,
                ),
            )
        };
        if r_min != sys::CUresult::CUDA_SUCCESS || r_rec != sys::CUresult::CUDA_SUCCESS {
            return VmmSupport::default();
        }
        let base_gran = rec_gran.max(min_gran).max(1);
        let chunk_bytes = align_up(DEFAULT_VMM_CHUNK_BYTES.max(base_gran), base_gran)
            .unwrap_or(DEFAULT_VMM_CHUNK_BYTES);
        VmmSupport {
            supported: true,
            min_granularity: min_gran,
            recommended_granularity: rec_gran,
            chunk_bytes,
        }
    }

    /// CUDA 12 Driver VMM driver backed by a live `CudaDevice`.
    pub struct CudaVmmDriver {
        dev: Arc<CudaDevice>,
        cu_device: sys::CUdevice,
        chunk_bytes: usize,
    }

    impl CudaVmmDriver {
        /// Construct a CUDA VMM driver for `dev` using probed `support.chunk_bytes`.
        pub fn new(dev: Arc<CudaDevice>, support: VmmSupport) -> Self {
            let cu_device = *dev.cu_device();
            Self {
                dev,
                cu_device,
                chunk_bytes: support.chunk_bytes.max(DEFAULT_VMM_CHUNK_BYTES),
            }
        }

        /// Construct a CUDA VMM driver for `ctx` using probed `support.chunk_bytes`.
        pub fn from_context(ctx: &crate::DeviceContext, support: VmmSupport) -> Self {
            Self::new(Arc::clone(ctx.inner().device()), support)
        }

        fn bind(&self) -> Result<()> {
            self.dev
                .bind_to_thread()
                .map_err(|e| DeviceError::Driver(format!("vmm bind_to_thread: {e:?}")))
        }
    }

    impl VmmDriver for CudaVmmDriver {
        type Handle = sys::CUmemGenericAllocationHandle;

        fn granularity(&self) -> usize {
            self.chunk_bytes
        }

        fn reserve_va(&self, size: usize, alignment: usize) -> Result<u64> {
            self.bind()?;
            let mut ptr: sys::CUdeviceptr = 0;
            // SAFETY: context is bound; `ptr` is a valid out-pointer; `size`
            // and `alignment` are multiples of driver VMM granularity.
            let res = unsafe { sys::lib().cuMemAddressReserve(&mut ptr, size, alignment, 0, 0) };
            check_cu(res, "cuMemAddressReserve")?;
            Ok(ptr)
        }

        fn free_va(&self, va: u64, size: usize) -> Result<()> {
            let _ = self.bind();
            // SAFETY: `[va, va + size)` was reserved with `cuMemAddressReserve`
            // and all chunks inside it have been unmapped.
            let res = unsafe { sys::lib().cuMemAddressFree(va, size) };
            check_cu(res, "cuMemAddressFree")
        }

        fn create_physical(&self, size: usize) -> Result<Self::Handle> {
            self.bind()?;
            let prop = make_alloc_prop(self.cu_device);
            let mut handle: sys::CUmemGenericAllocationHandle = 0;
            // SAFETY: context is bound; `prop` is valid and `handle` is a
            // valid out-pointer; `size` is granularity-aligned.
            let res = unsafe { sys::lib().cuMemCreate(&mut handle, size, &prop, 0) };
            check_cu(res, "cuMemCreate")?;
            Ok(handle)
        }

        fn release_physical(&self, handle: Self::Handle) -> Result<()> {
            let _ = self.bind();
            // SAFETY: `handle` was returned by `cuMemCreate` and is no longer
            // owned elsewhere (remaining mappings keep internal driver refcounts).
            let res = unsafe { sys::lib().cuMemRelease(handle) };
            check_cu(res, "cuMemRelease")
        }

        fn map(&self, va: u64, size: usize, handle: Self::Handle) -> Result<()> {
            self.bind()?;
            // SAFETY: `[va, va + size)` is an unmapped reserved range and
            // `handle` is a live physical allocation of at least `size` bytes.
            let res = unsafe { sys::lib().cuMemMap(va, size, 0, handle, 0) };
            check_cu(res, "cuMemMap")
        }

        fn set_access(&self, va: u64, size: usize) -> Result<()> {
            self.bind()?;
            let desc = sys::CUmemAccessDesc {
                location: sys::CUmemLocation {
                    type_: sys::CUmemLocationType::CU_MEM_LOCATION_TYPE_DEVICE,
                    id: self.cu_device,
                },
                flags: sys::CUmemAccess_flags::CU_MEM_ACCESS_FLAGS_PROT_READWRITE,
            };
            // SAFETY: `[va, va + size)` is mapped via `cuMemMap` and `desc`
            // points to a stack-initialized `CUmemAccessDesc` array of length 1.
            let res = unsafe { sys::lib().cuMemSetAccess(va, size, &desc, 1) };
            check_cu(res, "cuMemSetAccess")
        }

        fn unmap(&self, va: u64, size: usize) -> Result<()> {
            let _ = self.bind();
            // SAFETY: `[va, va + size)` is currently mapped via `cuMemMap`.
            let res = unsafe { sys::lib().cuMemUnmap(va, size) };
            check_cu(res, "cuMemUnmap")
        }

        fn bind_context(&self) -> Result<()> {
            self.bind()
        }
    }
}

#[cfg(feature = "cuda")]
pub use cuda_vmm::{probe_vmm_support, probe_vmm_support_for_context, CudaVmmDriver};
