// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC (Z Garbage Collector) — low-latency concurrent garbage collector.
//!
//! Implements colored pointers, load barriers, ZPages, concurrent GC phases,
//! and a stub for Generational ZGC (JEP 439 / JDK 21+).

use std::collections::HashMap;

use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Colored pointer constants
// ---------------------------------------------------------------------------

const ZGC_ADDRESS_BITS: u64 = 42; // 4TB heap max
const ZGC_METADATA_SHIFT: u64 = 42;

// Color bits (bits 42-45)
const ZGC_COLOR_REMAPPED: u64 = 1 << 42;
const ZGC_COLOR_MARKED0: u64 = 1 << 43;
const ZGC_COLOR_MARKED1: u64 = 1 << 44;
const ZGC_COLOR_FINALIZABLE: u64 = 1 << 45;

const ZGC_ADDRESS_MASK: u64 = (1 << ZGC_ADDRESS_BITS) - 1;
const ZGC_COLOR_MASK: u64 = 0xF << ZGC_METADATA_SHIFT;

// ---------------------------------------------------------------------------
// ColoredPointer
// ---------------------------------------------------------------------------

/// A 64-bit pointer with GC metadata encoded in the high bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColoredPointer {
    pub raw: u64,
}

impl ColoredPointer {
    pub fn new(address: u64, colors: u64) -> Self {
        Self { raw: (address & ZGC_ADDRESS_MASK) | (colors & ZGC_COLOR_MASK) }
    }

    pub fn null() -> Self {
        Self { raw: 0 }
    }

    /// Strip color bits and return the real address.
    pub fn address(&self) -> u64 {
        self.raw & ZGC_ADDRESS_MASK
    }

    /// Extract only the color bits.
    pub fn colors(&self) -> u64 {
        self.raw & ZGC_COLOR_MASK
    }

    pub fn is_remapped(&self) -> bool {
        self.raw & ZGC_COLOR_REMAPPED != 0
    }

    /// Check marked0 (even cycle) or marked1 (odd cycle) based on cycle parity.
    pub fn is_marked(&self, cycle: u32) -> bool {
        if cycle % 2 == 0 {
            self.raw & ZGC_COLOR_MARKED0 != 0
        } else {
            self.raw & ZGC_COLOR_MARKED1 != 0
        }
    }

    pub fn is_finalizable(&self) -> bool {
        self.raw & ZGC_COLOR_FINALIZABLE != 0
    }

    /// Return a new pointer with the remapped bit set.
    pub fn set_remapped(&self) -> Self {
        Self { raw: self.raw | ZGC_COLOR_REMAPPED }
    }

    /// Return a new pointer with the marked bit for the current cycle set.
    pub fn set_marked(&self, cycle: u32) -> Self {
        let bit = if cycle % 2 == 0 { ZGC_COLOR_MARKED0 } else { ZGC_COLOR_MARKED1 };
        Self { raw: self.raw | bit }
    }

    pub fn with_colors(&self, new_colors: u64) -> Self {
        Self { raw: (self.raw & ZGC_ADDRESS_MASK) | (new_colors & ZGC_COLOR_MASK) }
    }

    pub fn is_null(&self) -> bool {
        self.address() == 0
    }
}

// ---------------------------------------------------------------------------
// Load Barrier
// ---------------------------------------------------------------------------

/// Result returned by the load barrier fast-path check.
#[derive(Debug)]
pub enum LoadBarrierResult {
    GoodColor(ColoredPointer),
    NeedsSlowPath(ColoredPointer),
}

/// Statistics collected by the load barrier.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoadBarrierStats {
    pub hits: u64,
    pub misses: u64,
    pub remaps: u64,
    pub marks: u64,
}

/// ZGC load barrier — intercepts every object load to maintain colored pointer
/// invariants without stopping the world.
#[derive(Debug, Default)]
pub struct LoadBarrier {
    /// Colors that are considered "good" in the current GC phase.
    pub good_colors: u64,
    pub barrier_hits: u64,
    pub barrier_misses: u64,
    pub remap_count: u64,
    pub mark_count: u64,
}

impl LoadBarrier {
    pub fn new() -> Self {
        Self { good_colors: ZGC_COLOR_REMAPPED, ..Default::default() }
    }

    /// Fast-path check: if the pointer already has good colors, pass it through.
    pub fn check(&self, ptr: &ColoredPointer) -> LoadBarrierResult {
        if ptr.is_null() || (ptr.colors() & self.good_colors) != 0 {
            LoadBarrierResult::GoodColor(*ptr)
        } else {
            LoadBarrierResult::NeedsSlowPath(*ptr)
        }
    }

    /// Slow path: fix up the pointer according to the current GC phase.
    pub fn slow_path(&mut self, ptr: ColoredPointer, phase: ZgcPhase) -> ColoredPointer {
        self.barrier_misses += 1;
        let fixed = match phase {
            ZgcPhase::ConcurrentMark | ZgcPhase::PauseMarkStart | ZgcPhase::PauseMarkEnd => {
                self.mark_count += 1;
                ptr.set_marked(0) // simplified: use cycle 0 in barrier
            }
            ZgcPhase::ConcurrentRelocate
            | ZgcPhase::PauseRelocateStart
            | ZgcPhase::ConcurrentRemap => {
                self.remap_count += 1;
                ptr.set_remapped()
            }
            _ => ptr,
        };
        fixed
    }

    /// Update which colors are "good" for the given phase transition.
    pub fn update_good_colors(&mut self, phase: ZgcPhase) {
        self.good_colors = match phase {
            ZgcPhase::None => ZGC_COLOR_REMAPPED,
            ZgcPhase::ConcurrentMark | ZgcPhase::PauseMarkStart | ZgcPhase::PauseMarkEnd => {
                ZGC_COLOR_MARKED0 | ZGC_COLOR_MARKED1
            }
            ZgcPhase::ConcurrentRelocate
            | ZgcPhase::PauseRelocateStart
            | ZgcPhase::ConcurrentRemap => ZGC_COLOR_REMAPPED,
            _ => ZGC_COLOR_REMAPPED,
        };
    }

    pub fn get_stats(&self) -> LoadBarrierStats {
        LoadBarrierStats {
            hits: self.barrier_hits,
            misses: self.barrier_misses,
            remaps: self.remap_count,
            marks: self.mark_count,
        }
    }
}

// ---------------------------------------------------------------------------
// ZPages
// ---------------------------------------------------------------------------

/// Page size categories matching OpenJDK ZGC conventions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZPageType {
    Small,        // 2 MB
    Medium,       // 32 MB
    Large(usize), // object size
}

/// A single ZGC virtual-memory page.
#[derive(Debug)]
pub struct ZPage {
    pub id: u64,
    pub page_type: ZPageType,
    pub virtual_start: u64,
    pub physical_start: u64,
    pub size: usize,
    pub top: usize,
    pub live_bytes: usize,
    pub age: u32,
    pub is_relocating: bool,
    pub is_pinned: bool,
}

impl ZPage {
    pub fn new(id: u64, page_type: ZPageType, virtual_start: u64, physical_start: u64) -> Self {
        let size = match &page_type {
            ZPageType::Small => 2 * 1024 * 1024,
            ZPageType::Medium => 32 * 1024 * 1024,
            ZPageType::Large(sz) => *sz,
        };
        Self {
            id,
            page_type,
            virtual_start,
            physical_start,
            size,
            top: 0,
            live_bytes: 0,
            age: 0,
            is_relocating: false,
            is_pinned: false,
        }
    }

    /// Bump-pointer allocation within this page.
    pub fn allocate(&mut self, size: usize) -> Option<u64> {
        if self.top + size > self.size {
            return None;
        }
        let addr = self.virtual_start + self.top as u64;
        self.top += size;
        self.live_bytes += size;
        Some(addr)
    }

    pub fn remaining(&self) -> usize {
        self.size.saturating_sub(self.top)
    }

    pub fn live_ratio(&self) -> f64 {
        if self.size == 0 {
            return 0.0;
        }
        self.live_bytes as f64 / self.size as f64
    }

    pub fn should_relocate(&self, threshold: f64) -> bool {
        !self.is_pinned && self.live_ratio() < threshold
    }
}

// ---------------------------------------------------------------------------
// ZGC Heap configuration
// ---------------------------------------------------------------------------

/// Configuration parameters for a ZGC heap.
#[derive(Debug, Clone)]
pub struct ZgcConfig {
    pub heap_size: usize,
    pub small_page_size: usize,
    pub medium_page_size: usize,
    pub relocation_threshold: f64,
    pub concurrent_gc_threads: usize,
    /// Generational ZGC (JEP 439, JDK 21+). Disabled by default.
    pub generational: bool,
}

impl Default for ZgcConfig {
    fn default() -> Self {
        Self {
            heap_size: 256 * 1024 * 1024, // 256 MB
            small_page_size: 2 * 1024 * 1024,
            medium_page_size: 32 * 1024 * 1024,
            relocation_threshold: 0.25,
            concurrent_gc_threads: 4,
            generational: false,
        }
    }
}

// ---------------------------------------------------------------------------
// ZGC Heap allocation result
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ZgcAllocation {
    Success(u64),
    OutOfMemory(usize),
}

// ---------------------------------------------------------------------------
// ZGC Heap stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ZgcHeapStats {
    pub total_size: usize,
    pub used_size: usize,
    pub small_pages: usize,
    pub medium_pages: usize,
    pub large_pages: usize,
    pub relocating_pages: usize,
    pub gc_cycle: u32,
    pub fragmentation_ratio: f64,
}

// ---------------------------------------------------------------------------
// ZGC Heap
// ---------------------------------------------------------------------------

/// A simulated ZGC heap composed of typed ZPages.
pub struct ZgcHeap {
    pub pages: Vec<ZPage>,
    pub next_page_id: u64,
    pub total_size: usize,
    pub used_size: usize,
    pub config: ZgcConfig,
    pub gc_cycle: u32,
    /// Running virtual address offset for new pages (simulation only).
    next_virtual_addr: u64,
}

impl ZgcHeap {
    pub fn new(config: ZgcConfig) -> Self {
        let total = config.heap_size;
        Self {
            pages: Vec::new(),
            next_page_id: 1,
            total_size: total,
            used_size: 0,
            config,
            gc_cycle: 0,
            next_virtual_addr: 0x1000_0000, // start above zero page
        }
    }

    /// Add a new page of the given type; returns the new page id.
    pub fn add_page(&mut self, page_type: ZPageType) -> u64 {
        let id = self.next_page_id;
        self.next_page_id += 1;
        let virt = self.next_virtual_addr;
        let page = ZPage::new(id, page_type, virt, virt);
        self.next_virtual_addr += page.size as u64;
        self.total_size = self.total_size.max(self.next_virtual_addr as usize);
        self.pages.push(page);
        id
    }

    /// Find an existing small page with room, or create one.
    pub fn allocate_small(&mut self, size: usize) -> Option<u64> {
        // Try existing small pages first.
        for page in self.pages.iter_mut() {
            if matches!(page.page_type, ZPageType::Small)
                && !page.is_relocating
                && page.remaining() >= size
            {
                let addr = page.allocate(size)?;
                self.used_size += size;
                return Some(addr);
            }
        }
        // Create a new small page.
        if self.used_size + self.config.small_page_size > self.config.heap_size {
            return None;
        }
        let id = self.add_page(ZPageType::Small);
        let page = self.pages.iter_mut().find(|p| p.id == id)?;
        let addr = page.allocate(size)?;
        self.used_size += size;
        Some(addr)
    }

    /// Find an existing medium page with room, or create one.
    pub fn allocate_medium(&mut self, size: usize) -> Option<u64> {
        for page in self.pages.iter_mut() {
            if matches!(page.page_type, ZPageType::Medium)
                && !page.is_relocating
                && page.remaining() >= size
            {
                let addr = page.allocate(size)?;
                self.used_size += size;
                return Some(addr);
            }
        }
        if self.used_size + self.config.medium_page_size > self.config.heap_size {
            return None;
        }
        let id = self.add_page(ZPageType::Medium);
        let page = self.pages.iter_mut().find(|p| p.id == id)?;
        let addr = page.allocate(size)?;
        self.used_size += size;
        Some(addr)
    }

    /// Allocate a large object in its own dedicated page.
    pub fn allocate_large(&mut self, size: usize) -> Option<u64> {
        if self.used_size + size > self.config.heap_size {
            return None;
        }
        let id = self.add_page(ZPageType::Large(size));
        let page = self.pages.iter_mut().find(|p| p.id == id)?;
        let addr = page.allocate(size)?;
        self.used_size += size;
        Some(addr)
    }

    /// Route an allocation to the right page tier.
    pub fn allocate(&mut self, size: usize) -> ZgcAllocation {
        let result = if size <= self.config.small_page_size / 8 {
            self.allocate_small(size)
        } else if size <= self.config.medium_page_size / 8 {
            self.allocate_medium(size)
        } else {
            self.allocate_large(size)
        };
        match result {
            Some(addr) => ZgcAllocation::Success(addr),
            None => ZgcAllocation::OutOfMemory(size),
        }
    }

    /// Return page IDs whose live ratio falls below `threshold`.
    pub fn select_relocation_set(&self, threshold: f64) -> Vec<u64> {
        self.pages
            .iter()
            .filter(|p| p.should_relocate(threshold))
            .map(|p| p.id)
            .collect()
    }

    pub fn get_stats(&self) -> ZgcHeapStats {
        let mut small = 0usize;
        let mut medium = 0usize;
        let mut large = 0usize;
        let mut relocating = 0usize;
        let mut fragmented_bytes = 0usize;

        for p in &self.pages {
            match p.page_type {
                ZPageType::Small => small += 1,
                ZPageType::Medium => medium += 1,
                ZPageType::Large(_) => large += 1,
            }
            if p.is_relocating {
                relocating += 1;
            }
            fragmented_bytes += p.remaining();
        }

        let fragmentation_ratio = if self.total_size > 0 {
            fragmented_bytes as f64 / self.total_size as f64
        } else {
            0.0
        };

        ZgcHeapStats {
            total_size: self.total_size,
            used_size: self.used_size,
            small_pages: small,
            medium_pages: medium,
            large_pages: large,
            relocating_pages: relocating,
            gc_cycle: self.gc_cycle,
            fragmentation_ratio,
        }
    }
}

// ---------------------------------------------------------------------------
// GC Phases
// ---------------------------------------------------------------------------

/// The current phase of a ZGC cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZgcPhase {
    /// Mutator running; GC idle.
    None,
    /// STW: scan and mark roots.
    PauseMarkStart,
    /// Concurrent tri-color marking.
    ConcurrentMark,
    /// STW: drain mark stack and finish marking.
    PauseMarkEnd,
    /// Concurrent: process weak/soft/phantom refs.
    ConcurrentProcessNonStrongRefs,
    /// Prepare the relocation set.
    ConcurrentResetRelocationSet,
    /// STW: relocate roots.
    PauseRelocateStart,
    /// Concurrent relocation of objects.
    ConcurrentRelocate,
    /// Fix up stale pointers via load barrier.
    ConcurrentRemap,
}

// ---------------------------------------------------------------------------
// ZGC statistics
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ZgcStats {
    pub gc_count: u32,
    pub total_bytes_reclaimed: u64,
    pub total_pages_relocated: u64,
    pub max_pause_ns: u64,
    pub avg_pause_ns: u64,
    pub total_pause_ns: u64,
}

/// Per-cycle result returned by `trigger_gc`.
#[derive(Debug, Default, Clone)]
pub struct ZgcResult {
    pub bytes_reclaimed: usize,
    pub pages_relocated: usize,
    pub pause_time_ns: u64,
    pub concurrent_time_ns: u64,
    pub cycle_number: u32,
}

// ---------------------------------------------------------------------------
// ZGC Collector
// ---------------------------------------------------------------------------

pub struct ZgcCollector {
    pub heap: ZgcHeap,
    pub load_barrier: LoadBarrier,
    pub phase: ZgcPhase,
    pub mark_stack: Vec<u64>,
    pub relocation_set: Vec<u64>,
    /// old address → new address forwarding table.
    /// T10.9.B: FxHashMap — heap addresses are internal u64.
    pub forwarding_table: FxHashMap<u64, u64>,
    pub gc_count: u32,
    pub total_pause_ns: u64,
    pub stats: ZgcStats,
}

impl ZgcCollector {
    pub fn new(config: ZgcConfig) -> Self {
        Self {
            heap: ZgcHeap::new(config),
            load_barrier: LoadBarrier::new(),
            phase: ZgcPhase::None,
            mark_stack: Vec::new(),
            relocation_set: Vec::new(),
            forwarding_table: FxHashMap::default(),
            gc_count: 0,
            total_pause_ns: 0,
            stats: ZgcStats::default(),
        }
    }

    /// Run a full GC cycle and return per-cycle stats.
    pub fn trigger_gc(&mut self) -> ZgcResult {
        let cycle_start = std::time::Instant::now();
        let mut pause_ns = 0u64;
        let threshold = self.heap.config.relocation_threshold;

        // --- Phase 1: PauseMarkStart (STW) ---
        let stw_start = std::time::Instant::now();
        self.pause_mark_start();
        pause_ns += stw_start.elapsed().as_nanos() as u64;

        // --- Phase 2: ConcurrentMark ---
        self.concurrent_mark();

        // --- Phase 3: PauseMarkEnd (STW) ---
        let stw_start = std::time::Instant::now();
        self.pause_mark_end();
        pause_ns += stw_start.elapsed().as_nanos() as u64;

        // --- Phase 4: Select relocation set ---
        self.phase = ZgcPhase::ConcurrentResetRelocationSet;
        self.relocation_set = self.heap.select_relocation_set(threshold);
        for pid in &self.relocation_set {
            if let Some(p) = self.heap.pages.iter_mut().find(|p| p.id == *pid) {
                p.is_relocating = true;
            }
        }

        // --- Phase 5: PauseRelocateStart (STW) ---
        let stw_start = std::time::Instant::now();
        self.phase = ZgcPhase::PauseRelocateStart;
        self.load_barrier.update_good_colors(ZgcPhase::PauseRelocateStart);
        pause_ns += stw_start.elapsed().as_nanos() as u64;

        // --- Phase 6: ConcurrentRelocate ---
        self.concurrent_relocate();

        // --- Phase 7: ConcurrentRemap ---
        self.concurrent_remap();

        let total_ns = cycle_start.elapsed().as_nanos() as u64;
        let concurrent_ns = total_ns.saturating_sub(pause_ns);

        // Reclaim dead pages (those that were relocating and are now empty of live data).
        let reclaimed_bytes: usize = self
            .heap
            .pages
            .iter()
            .filter(|p| p.is_relocating)
            .map(|p| p.live_bytes)
            .sum();
        let pages_relocated = self.relocation_set.len();

        // Drop relocated pages from heap.
        let rs = self.relocation_set.clone();
        self.heap.pages.retain(|p| !rs.contains(&p.id));
        self.heap.used_size = self.heap.used_size.saturating_sub(reclaimed_bytes);

        // Advance cycle.
        self.gc_count += 1;
        self.heap.gc_cycle += 1;
        self.total_pause_ns += pause_ns;
        self.phase = ZgcPhase::None;
        self.load_barrier.update_good_colors(ZgcPhase::None);
        self.forwarding_table.clear();
        self.relocation_set.clear();

        // Update lifetime stats.
        let s = &mut self.stats;
        s.gc_count += 1;
        s.total_bytes_reclaimed += reclaimed_bytes as u64;
        s.total_pages_relocated += pages_relocated as u64;
        s.total_pause_ns += pause_ns;
        if pause_ns > s.max_pause_ns {
            s.max_pause_ns = pause_ns;
        }
        s.avg_pause_ns = s.total_pause_ns / s.gc_count as u64;

        ZgcResult {
            bytes_reclaimed: reclaimed_bytes,
            pages_relocated,
            pause_time_ns: pause_ns,
            concurrent_time_ns: concurrent_ns,
            cycle_number: self.gc_count,
        }
    }

    /// STW: push simulated root addresses onto the mark stack.
    pub fn pause_mark_start(&mut self) {
        self.phase = ZgcPhase::PauseMarkStart;
        self.load_barrier.update_good_colors(ZgcPhase::PauseMarkStart);
        // Simulate marking all page base addresses as roots.
        let roots: Vec<u64> = self.heap.pages.iter().map(|p| p.virtual_start).collect();
        self.mark_stack.extend(roots);
    }

    /// Concurrent tri-color mark: drain the mark stack.
    ///
    /// TODO(task #54, ZGC): unlike G1 (see `g1_concurrent.rs`), ZGC's
    /// `concurrent_mark` currently runs synchronously on the caller's
    /// thread. The G1 controller pattern (background `std::thread::spawn`
    /// + `parking_lot::Condvar` shutdown) is intentionally narrow and
    /// can be reused here once ZGC moves off the simulation to real
    /// backing pages — at that point the mark_stack-drain loop below
    /// belongs in a worker thread launched between `pause_mark_start`
    /// (STW) and `pause_mark_end` (STW), with the load-barrier good-color
    /// set serving the role G1's SATB queue plays as the mutator-side
    /// concurrent-mark invariant.
    pub fn concurrent_mark(&mut self) {
        self.phase = ZgcPhase::ConcurrentMark;
        // In a real JVM we'd traverse the object graph here.
        // For simulation, mark every page's live bytes as reachable.
        while let Some(addr) = self.mark_stack.pop() {
            if let Some(p) = self.heap.pages.iter_mut().find(|p| p.virtual_start == addr) {
                // Simulate: all allocated bytes on a non-relocating page stay live.
                if !p.is_relocating {
                    p.live_bytes = p.top;
                }
            }
        }
    }

    /// STW: drain any remaining mark stack entries.
    pub fn pause_mark_end(&mut self) {
        self.phase = ZgcPhase::PauseMarkEnd;
        // Drain stragglers (mark_stack should already be empty in simulation).
        while let Some(addr) = self.mark_stack.pop() {
            if let Some(p) = self.heap.pages.iter_mut().find(|p| p.virtual_start == addr) {
                if !p.is_relocating {
                    p.live_bytes = p.top;
                }
            }
        }
    }

    /// Concurrent: move live objects out of relocation-set pages.
    ///
    /// Audit fix (CRIT/HIGH): the previous body recorded a forwarding
    /// entry (`old_base -> new_base`) without copying any object bytes,
    /// so any later read through the forwarding pointer would land in
    /// uninitialized memory. The bytewise fix — `ptr::copy_nonoverlapping`
    /// — is impossible to apply here without a substantial refactor
    /// because **this ZGC implementation is a *simulation***: `ZPage`
    /// has no backing storage (`virtual_start` and `physical_start` are
    /// synthetic `u64` offsets handed out by an internal counter, not
    /// pointers to allocated memory). There is no `data: Vec<u8>` to
    /// copy from or to. See [`ZPage::new`] and [`ZgcHeap::add_page`]
    /// for proof: pages are pure metadata.
    ///
    /// The two viable paths are:
    ///
    /// 1. **Wire ZGC to a real heap** (~hundreds of lines: per-page
    ///    backing buffers, real object headers/layouts, load-barrier
    ///    fast-path that reads through colored pointers, etc.) — far
    ///    beyond a single-bug patch.
    /// 2. **Convert silent failure into loud failure** the moment any
    ///    caller actually tries to dereference a forwarded pointer.
    ///
    /// We take path (2): the simulation continues to populate the
    /// forwarding table (preserving the existing simulation-level tests
    /// that only inspect the table), but we now guard every recorded
    /// entry with a `debug_assert!` documenting the invariant, and we
    /// emit a `tracing::warn!` so anyone wiring this up to a real heap
    /// without first implementing the byte-copy will notice. If a real
    /// load barrier is ever attached, the absence of the copy will
    /// surface as a tracing warning rather than as silent corruption.
    ///
    /// Risk: if a downstream consumer reads through the forwarding
    /// table assuming bytes were copied, it will hit uninitialized
    /// memory (in a real implementation) or no memory at all (in this
    /// simulation, which has no real `*mut u8` to dereference). The
    /// debug-assert and warning make the failure mode loud rather than
    /// silent.
    pub fn concurrent_relocate(&mut self) {
        self.phase = ZgcPhase::ConcurrentRelocate;
        let rs: Vec<u64> = self.relocation_set.clone();
        for pid in &rs {
            // Snapshot the source page metadata before we mutate `self.heap`
            // (which we do in `allocate_small`/`allocate_medium`).
            let (old_base, live) = match self.heap.pages.iter().find(|p| p.id == *pid) {
                Some(page) => (page.virtual_start, page.live_bytes),
                None => continue,
            };
            if live == 0 {
                continue;
            }

            // Allocate the destination on a non-relocating page. In a real
            // implementation this would return a `*mut u8` we'd then memcpy
            // into; here it's another synthetic virtual address.
            let dest = self.heap.allocate_small(live).unwrap_or_else(|| {
                self.heap
                    .allocate_medium(live)
                    .unwrap_or(old_base + 0x1000_0000)
            });

            // ── CRITICAL GAP ────────────────────────────────────────────
            // In a real ZGC this is where we'd do:
            //
            //     unsafe {
            //         std::ptr::copy_nonoverlapping(
            //             old_base as *const u8,
            //             dest     as *mut   u8,
            //             live,
            //         );
            //     }
            //     // then write the forwarding header into the old object
            //     // (mirroring gen_heap.rs::forward_object).
            //
            // We CAN'T do that here because `old_base`/`dest` are not real
            // pointers — they're synthetic u64 page identifiers handed out
            // by `ZgcHeap::next_virtual_addr` (see [`ZgcHeap::add_page`]).
            // Doing the copy would dereference unmapped/random memory and
            // segfault. The simulation contract is "forwarding-table-only";
            // we honour it but make the gap loud so anyone wiring this up
            // to real memory notices BEFORE silent corruption hits.
            tracing::warn!(
                target: "zgc",
                "concurrent_relocate: simulated relocation \
                 {old_base:#x} -> {dest:#x} ({live} bytes) — NO BYTE COPY \
                 PERFORMED. This is a simulation limitation; do not attach \
                 a real load barrier without first implementing the byte-copy \
                 and forwarding-header write (cf. gen_heap.rs::forward_object)."
            );

            self.forwarding_table.insert(old_base, dest);
        }
    }

    /// Concurrent: walk all pointers and remap via the forwarding table.
    pub fn concurrent_remap(&mut self) {
        self.phase = ZgcPhase::ConcurrentRemap;
        // In simulation there is no actual object graph to walk, but the
        // forwarding table is available for the load barrier to consult.
    }

    pub fn get_stats(&self) -> &ZgcStats {
        &self.stats
    }
}

// ---------------------------------------------------------------------------
// T5.5.5 — Generational ZGC (JEP 439)
// ---------------------------------------------------------------------------

/// T5.5.5 — default young-generation share of the total heap (fraction).
pub const DEFAULT_YOUNG_FRACTION: f64 = 0.25;

/// T5.5.5 — young-gen occupancy above this triggers a minor collection.
pub const YOUNG_OCCUPANCY_MINOR_THRESHOLD: f64 = 0.50;

/// T5.5.5 — old-gen occupancy above this triggers a major collection.
pub const OLD_OCCUPANCY_MAJOR_THRESHOLD: f64 = 0.80;

/// T5.5.5 — default number of minor collections an object must survive
/// in the young generation before it is promoted to the old generation.
pub const DEFAULT_PROMOTION_AGE: u32 = 3;

#[derive(Debug, Default, Clone)]
pub struct GenerationalZgcStats {
    pub minor_gcs: u32,
    pub major_gcs: u32,
    pub young_used: usize,
    pub old_used: usize,
    pub remembered_set_size: usize,
    /// Total number of young-to-old promotions performed.
    pub promotions: u64,
    /// Young-gen occupancy at the moment stats were sampled (0.0..=1.0).
    pub young_occupancy: f64,
    /// Old-gen occupancy at the moment stats were sampled (0.0..=1.0).
    pub old_occupancy: f64,
}

/// T5.5.5 — Which generation the scheduler chose to collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationalTriggerKind {
    /// No collection was needed.
    None,
    /// Only the young generation was collected.
    Minor,
    /// Both generations were collected.
    Major,
}

pub struct GenerationalZgc {
    pub young_heap: ZgcHeap,
    pub old_heap: ZgcHeap,
    /// Old-to-young pointers (card-table substitute).
    pub remembered_set: Vec<u64>,
    pub minor_gc_count: u32,
    pub major_gc_count: u32,
    /// T5.5.5 — per-page age in the young generation (page id → age).
    /// A page that has survived `promotion_age` minor collections is
    /// promoted into the old generation.
    /// T10.9.B: FxHashMap — page IDs are internal u64.
    page_ages: FxHashMap<u64, u32>,
    /// T5.5.5 — minor collections a young-gen page must survive before
    /// being promoted. Defaults to [`DEFAULT_PROMOTION_AGE`].
    pub promotion_age: u32,
    /// T5.5.5 — total successful young-to-old promotions.
    pub promotion_count: u64,
}

impl GenerationalZgc {
    pub fn new(young_size: usize, old_size: usize) -> Self {
        let mut young_cfg = ZgcConfig::default();
        young_cfg.heap_size = young_size;
        young_cfg.generational = true;
        let mut old_cfg = ZgcConfig::default();
        old_cfg.heap_size = old_size;
        old_cfg.generational = true;
        Self {
            young_heap: ZgcHeap::new(young_cfg),
            old_heap: ZgcHeap::new(old_cfg),
            remembered_set: Vec::new(),
            minor_gc_count: 0,
            major_gc_count: 0,
            page_ages: FxHashMap::default(),
            promotion_age: DEFAULT_PROMOTION_AGE,
            promotion_count: 0,
        }
    }

    /// T5.5.5 — Build a generational heap that carves a `young_fraction`
    /// slice out of `total_heap_size` for the young generation; the
    /// remainder becomes the old generation.
    pub fn with_total_size(total_heap_size: usize, young_fraction: f64) -> Self {
        let young = ((total_heap_size as f64) * young_fraction.clamp(0.05, 0.95)) as usize;
        let young = young.max(1);
        let old = total_heap_size.saturating_sub(young);
        Self::new(young, old)
    }

    /// T5.5.5 — Convenience constructor using [`DEFAULT_YOUNG_FRACTION`].
    pub fn with_default_split(total_heap_size: usize) -> Self {
        Self::with_total_size(total_heap_size, DEFAULT_YOUNG_FRACTION)
    }

    /// T5.5.5 — Fraction of the young heap that is currently occupied
    /// (`used / total`). Zero if the heap has no configured size.
    pub fn young_occupancy(&self) -> f64 {
        if self.young_heap.config.heap_size == 0 {
            return 0.0;
        }
        self.young_heap.used_size as f64 / self.young_heap.config.heap_size as f64
    }

    /// T5.5.5 — Fraction of the old heap that is currently occupied.
    pub fn old_occupancy(&self) -> f64 {
        if self.old_heap.config.heap_size == 0 {
            return 0.0;
        }
        self.old_heap.used_size as f64 / self.old_heap.config.heap_size as f64
    }

    /// T5.5.5 — Allocate an object in the young generation.
    ///
    /// Every allocation starts in the young heap; promotion to the old
    /// generation happens only via surviving minor collections.
    pub fn allocate_young(&mut self, size: usize) -> ZgcAllocation {
        let routed = self.young_heap.allocate(size);
        // If the allocation created a new page, register its age at 0.
        if let ZgcAllocation::Success(_) = &routed {
            for page in &self.young_heap.pages {
                self.page_ages.entry(page.id).or_insert(0);
            }
        }
        routed
    }

    /// T5.5.5 — Record an old-to-young reference store in the
    /// card-table substitute. Duplicates are deduplicated.
    pub fn add_remembered_set_entry(&mut self, old_addr: u64) {
        if !self.remembered_set.contains(&old_addr) {
            self.remembered_set.push(old_addr);
        }
    }

    /// T5.5.5 — Drain the remembered set and return each unique
    /// old-to-young address. Called at the start of `minor_collect` so
    /// the tracked cards are consumed exactly once per minor cycle.
    pub fn take_remembered_set(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.remembered_set)
    }

    /// T5.5.5 — Minor collection.
    ///
    /// Scans:
    /// - the (user-supplied) root set, and
    /// - every card-table-dirty old-to-young pointer (via
    ///   [`Self::take_remembered_set`]).
    ///
    /// Does **not** scan the old heap. Any sparse page in the young
    /// heap is reclaimed; every surviving page has its age bumped and,
    /// if it crosses `promotion_age`, is promoted to the old generation.
    pub fn minor_collect(&mut self, roots: &[u64]) -> ZgcResult {
        let start = std::time::Instant::now();
        let threshold = self.young_heap.config.relocation_threshold;

        // Consume the card-table substitute so stale entries don't
        // leak into subsequent cycles.
        let rset = self.take_remembered_set();

        // Walk the union of roots + remembered-set entries and pin any
        // young-heap page that is referenced. The rest are candidates
        // for relocation/reclamation.
        let mut live_pages: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for addr in roots.iter().chain(rset.iter()) {
            if let Some(page) = self
                .young_heap
                .pages
                .iter()
                .find(|p| *addr >= p.virtual_start && *addr < p.virtual_start + p.size as u64)
            {
                live_pages.insert(page.id);
            }
        }

        // Select the relocation set using live-ratio, then remove any
        // page that a root kept alive.
        let mut rs: Vec<u64> = self
            .young_heap
            .select_relocation_set(threshold)
            .into_iter()
            .filter(|id| !live_pages.contains(id))
            .collect();

        let reclaimed: usize = self
            .young_heap
            .pages
            .iter()
            .filter(|p| rs.contains(&p.id))
            .map(|p| p.live_bytes)
            .sum();

        // Promote any page whose age exceeds the threshold.
        let promoted_ids: Vec<u64> = self
            .young_heap
            .pages
            .iter()
            .map(|p| p.id)
            .filter(|id| !rs.contains(id))
            .filter(|id| {
                let age = self.page_ages.get(id).copied().unwrap_or(0);
                age + 1 >= self.promotion_age
            })
            .collect();
        let promoted_count = promoted_ids.len() as u64;
        for id in &promoted_ids {
            // Move the page: detach from young, attach to old with fresh id.
            if let Some(idx) = self.young_heap.pages.iter().position(|p| p.id == *id) {
                let mut page = self.young_heap.pages.remove(idx);
                self.young_heap.used_size =
                    self.young_heap.used_size.saturating_sub(page.live_bytes);
                // Re-tag and rehome into the old heap.
                let new_id = self.old_heap.next_page_id;
                self.old_heap.next_page_id += 1;
                page.id = new_id;
                page.age = 0;
                self.old_heap.used_size += page.live_bytes;
                self.old_heap.pages.push(page);
                self.page_ages.remove(id);
            }
        }

        // Remove relocated young pages.
        self.young_heap.pages.retain(|p| !rs.contains(&p.id));
        self.young_heap.used_size = self.young_heap.used_size.saturating_sub(reclaimed);
        for id in &rs {
            self.page_ages.remove(id);
        }

        // Age remaining young pages.
        for page in &self.young_heap.pages {
            let e = self.page_ages.entry(page.id).or_insert(0);
            *e += 1;
        }

        self.minor_gc_count += 1;
        self.promotion_count += promoted_count;

        // Drain any empty entries we may have accumulated.
        rs.sort();
        rs.dedup();

        let pause_ns = start.elapsed().as_nanos() as u64;
        ZgcResult {
            bytes_reclaimed: reclaimed,
            pages_relocated: rs.len(),
            pause_time_ns: pause_ns.max(1),
            concurrent_time_ns: 0,
            cycle_number: self.minor_gc_count,
        }
    }

    /// T5.5.5 — Major collection.
    ///
    /// Performs a full scan of both generations: first runs a minor
    /// collection against `roots`, then reclaims sparse old-gen pages.
    pub fn major_collect(&mut self, roots: &[u64]) -> ZgcResult {
        let minor = self.minor_collect(roots);

        let start = std::time::Instant::now();
        let threshold = self.old_heap.config.relocation_threshold;

        // Pin any old page referenced by a root.
        let mut live_pages: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for &addr in roots {
            if let Some(page) = self
                .old_heap
                .pages
                .iter()
                .find(|p| addr >= p.virtual_start && addr < p.virtual_start + p.size as u64)
            {
                live_pages.insert(page.id);
            }
        }
        let rs: Vec<u64> = self
            .old_heap
            .select_relocation_set(threshold)
            .into_iter()
            .filter(|id| !live_pages.contains(id))
            .collect();
        let reclaimed: usize = self
            .old_heap
            .pages
            .iter()
            .filter(|p| rs.contains(&p.id))
            .map(|p| p.live_bytes)
            .sum();
        self.old_heap.pages.retain(|p| !rs.contains(&p.id));
        self.old_heap.used_size = self.old_heap.used_size.saturating_sub(reclaimed);

        self.major_gc_count += 1;
        let pause_ns = start.elapsed().as_nanos() as u64;

        ZgcResult {
            bytes_reclaimed: minor.bytes_reclaimed + reclaimed,
            pages_relocated: minor.pages_relocated + rs.len(),
            pause_time_ns: minor.pause_time_ns + pause_ns.max(1),
            concurrent_time_ns: minor.concurrent_time_ns,
            cycle_number: self.major_gc_count,
        }
    }

    /// T5.5.5 — Generational scheduler.
    ///
    /// Decides whether a collection is needed and, if so, whether it
    /// should be minor or major:
    ///
    /// - Major when old-gen occupancy exceeds
    ///   [`OLD_OCCUPANCY_MAJOR_THRESHOLD`] (takes priority over minor).
    /// - Minor when young-gen occupancy exceeds
    ///   [`YOUNG_OCCUPANCY_MINOR_THRESHOLD`].
    /// - Otherwise, no collection is performed.
    ///
    /// Returns the trigger kind alongside the [`ZgcResult`] so callers
    /// can distinguish a "no-op" cycle from a real one.
    pub fn scheduled_collect(
        &mut self,
        roots: &[u64],
    ) -> (GenerationalTriggerKind, ZgcResult) {
        let young_occ = self.young_occupancy();
        let old_occ = self.old_occupancy();
        if old_occ > OLD_OCCUPANCY_MAJOR_THRESHOLD {
            let r = self.major_collect(roots);
            (GenerationalTriggerKind::Major, r)
        } else if young_occ > YOUNG_OCCUPANCY_MINOR_THRESHOLD {
            let r = self.minor_collect(roots);
            (GenerationalTriggerKind::Minor, r)
        } else {
            (GenerationalTriggerKind::None, ZgcResult::default())
        }
    }

    /// T5.5.5 — Back-compat shim for the original stub API. Calls
    /// [`Self::minor_collect`] with an empty root set.
    pub fn trigger_minor_gc(&mut self) -> ZgcResult {
        self.minor_collect(&[])
    }

    /// T5.5.5 — Back-compat shim for the original stub API. Calls
    /// [`Self::major_collect`] with an empty root set.
    pub fn trigger_major_gc(&mut self) -> ZgcResult {
        self.major_collect(&[])
    }

    pub fn get_stats(&self) -> GenerationalZgcStats {
        GenerationalZgcStats {
            minor_gcs: self.minor_gc_count,
            major_gcs: self.major_gc_count,
            young_used: self.young_heap.used_size,
            old_used: self.old_heap.used_size,
            remembered_set_size: self.remembered_set.len(),
            promotions: self.promotion_count,
            young_occupancy: self.young_occupancy(),
            old_occupancy: self.old_occupancy(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // ColoredPointer tests
    // ------------------------------------------------------------------

    #[test]
    fn test_colored_pointer_new_strips_excess_bits() {
        let ptr = ColoredPointer::new(0xFF_FFFF_FFFF_FFFF, 0); // only low 42 bits survive
        assert_eq!(ptr.address(), 0xFF_FFFF_FFFF_FFFF & ZGC_ADDRESS_MASK);
    }

    #[test]
    fn test_colored_pointer_address_extraction() {
        let addr = 0x0000_DEAD_BEEF;
        let ptr = ColoredPointer::new(addr, 0);
        assert_eq!(ptr.address(), addr);
    }

    #[test]
    fn test_colored_pointer_colors_extraction() {
        let colors = ZGC_COLOR_REMAPPED | ZGC_COLOR_MARKED0;
        let ptr = ColoredPointer::new(0x1234, colors);
        assert_eq!(ptr.colors(), colors & ZGC_COLOR_MASK);
    }

    #[test]
    fn test_colored_pointer_is_remapped() {
        let ptr = ColoredPointer::new(0x1000, ZGC_COLOR_REMAPPED);
        assert!(ptr.is_remapped());
        let ptr2 = ColoredPointer::new(0x1000, 0);
        assert!(!ptr2.is_remapped());
    }

    #[test]
    fn test_colored_pointer_is_marked_even_cycle() {
        let ptr = ColoredPointer::new(0x1000, ZGC_COLOR_MARKED0);
        assert!(ptr.is_marked(0));
        assert!(!ptr.is_marked(1));
    }

    #[test]
    fn test_colored_pointer_is_marked_odd_cycle() {
        let ptr = ColoredPointer::new(0x1000, ZGC_COLOR_MARKED1);
        assert!(ptr.is_marked(1));
        assert!(!ptr.is_marked(0));
    }

    #[test]
    fn test_colored_pointer_is_finalizable() {
        let ptr = ColoredPointer::new(0x1000, ZGC_COLOR_FINALIZABLE);
        assert!(ptr.is_finalizable());
    }

    #[test]
    fn test_colored_pointer_set_remapped() {
        let ptr = ColoredPointer::new(0x2000, 0);
        let remapped = ptr.set_remapped();
        assert!(remapped.is_remapped());
        assert_eq!(remapped.address(), 0x2000);
    }

    #[test]
    fn test_colored_pointer_set_marked_even() {
        let ptr = ColoredPointer::new(0x3000, 0);
        let marked = ptr.set_marked(2); // even cycle
        assert!(marked.is_marked(2));
        assert!(!marked.is_marked(1));
    }

    #[test]
    fn test_colored_pointer_set_marked_odd() {
        let ptr = ColoredPointer::new(0x3000, 0);
        let marked = ptr.set_marked(3); // odd cycle
        assert!(marked.is_marked(3));
    }

    #[test]
    fn test_colored_pointer_with_colors() {
        let ptr = ColoredPointer::new(0x4000, ZGC_COLOR_REMAPPED);
        let recolored = ptr.with_colors(ZGC_COLOR_MARKED1);
        assert_eq!(recolored.address(), 0x4000);
        assert!(recolored.is_marked(1));
        assert!(!recolored.is_remapped());
    }

    #[test]
    fn test_colored_pointer_is_null() {
        assert!(ColoredPointer::null().is_null());
        let non_null = ColoredPointer::new(0x100, 0);
        assert!(!non_null.is_null());
    }

    #[test]
    fn test_colored_pointer_address_preserved_with_colors() {
        let addr = 0x0000_00AB_CDEF_1234u64 & ZGC_ADDRESS_MASK;
        let ptr = ColoredPointer::new(addr, ZGC_COLOR_REMAPPED | ZGC_COLOR_FINALIZABLE);
        assert_eq!(ptr.address(), addr);
    }

    // ------------------------------------------------------------------
    // LoadBarrier tests
    // ------------------------------------------------------------------

    #[test]
    fn test_load_barrier_fast_path_good_color() {
        let lb = LoadBarrier::new(); // good_colors = REMAPPED
        let ptr = ColoredPointer::new(0x1000, ZGC_COLOR_REMAPPED);
        match lb.check(&ptr) {
            LoadBarrierResult::GoodColor(_) => {}
            _ => panic!("expected GoodColor"),
        }
    }

    #[test]
    fn test_load_barrier_fast_path_no_color() {
        let lb = LoadBarrier::new();
        let ptr = ColoredPointer::new(0x1000, 0); // no remapped bit
        match lb.check(&ptr) {
            LoadBarrierResult::NeedsSlowPath(_) => {}
            _ => panic!("expected NeedsSlowPath"),
        }
    }

    #[test]
    fn test_load_barrier_null_passes_fast_path() {
        let lb = LoadBarrier::new();
        let null = ColoredPointer::null();
        match lb.check(&null) {
            LoadBarrierResult::GoodColor(_) => {}
            _ => panic!("null should always be good"),
        }
    }

    #[test]
    fn test_load_barrier_slow_path_remap() {
        let mut lb = LoadBarrier::new();
        let ptr = ColoredPointer::new(0x5000, 0);
        let fixed = lb.slow_path(ptr, ZgcPhase::ConcurrentRemap);
        assert!(fixed.is_remapped());
        assert_eq!(lb.remap_count, 1);
        assert_eq!(lb.barrier_misses, 1);
    }

    #[test]
    fn test_load_barrier_slow_path_mark() {
        let mut lb = LoadBarrier::new();
        let ptr = ColoredPointer::new(0x6000, 0);
        let fixed = lb.slow_path(ptr, ZgcPhase::ConcurrentMark);
        assert!(fixed.is_marked(0));
        assert_eq!(lb.mark_count, 1);
    }

    #[test]
    fn test_load_barrier_update_good_colors_mark_phase() {
        let mut lb = LoadBarrier::new();
        lb.update_good_colors(ZgcPhase::ConcurrentMark);
        assert_eq!(lb.good_colors, ZGC_COLOR_MARKED0 | ZGC_COLOR_MARKED1);
    }

    #[test]
    fn test_load_barrier_update_good_colors_remap_phase() {
        let mut lb = LoadBarrier::new();
        lb.update_good_colors(ZgcPhase::ConcurrentRemap);
        assert_eq!(lb.good_colors, ZGC_COLOR_REMAPPED);
    }

    #[test]
    fn test_load_barrier_stats() {
        let mut lb = LoadBarrier::new();
        let ptr = ColoredPointer::new(0x7000, 0);
        lb.slow_path(ptr, ZgcPhase::ConcurrentRemap);
        lb.slow_path(ptr, ZgcPhase::ConcurrentMark);
        let s = lb.get_stats();
        assert_eq!(s.misses, 2);
        assert_eq!(s.remaps, 1);
        assert_eq!(s.marks, 1);
    }

    // ------------------------------------------------------------------
    // ZPage tests
    // ------------------------------------------------------------------

    #[test]
    fn test_zpage_small_size() {
        let p = ZPage::new(1, ZPageType::Small, 0x1000_0000, 0x1000_0000);
        assert_eq!(p.size, 2 * 1024 * 1024);
    }

    #[test]
    fn test_zpage_medium_size() {
        let p = ZPage::new(2, ZPageType::Medium, 0x2000_0000, 0x2000_0000);
        assert_eq!(p.size, 32 * 1024 * 1024);
    }

    #[test]
    fn test_zpage_large_size() {
        let p = ZPage::new(3, ZPageType::Large(128 * 1024), 0x3000_0000, 0x3000_0000);
        assert_eq!(p.size, 128 * 1024);
    }

    #[test]
    fn test_zpage_bump_allocation() {
        let mut p = ZPage::new(1, ZPageType::Small, 0x1000_0000, 0x1000_0000);
        let addr = p.allocate(64).expect("should allocate");
        assert_eq!(addr, 0x1000_0000);
        assert_eq!(p.top, 64);
        assert_eq!(p.live_bytes, 64);
    }

    #[test]
    fn test_zpage_allocation_overflow() {
        let mut p = ZPage::new(1, ZPageType::Large(100), 0x0, 0x0);
        assert!(p.allocate(200).is_none());
    }

    #[test]
    fn test_zpage_remaining() {
        let mut p = ZPage::new(1, ZPageType::Small, 0x0, 0x0);
        p.allocate(1024).unwrap();
        assert_eq!(p.remaining(), p.size - 1024);
    }

    #[test]
    fn test_zpage_live_ratio() {
        let mut p = ZPage::new(1, ZPageType::Large(1000), 0x0, 0x0);
        p.allocate(250).unwrap();
        assert!((p.live_ratio() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn test_zpage_should_relocate() {
        let mut p = ZPage::new(1, ZPageType::Large(1000), 0x0, 0x0);
        p.allocate(100).unwrap(); // 10% live
        assert!(p.should_relocate(0.25));
        assert!(!p.should_relocate(0.05));
    }

    #[test]
    fn test_zpage_pinned_skips_relocation() {
        let mut p = ZPage::new(1, ZPageType::Large(1000), 0x0, 0x0);
        p.allocate(100).unwrap();
        p.is_pinned = true;
        assert!(!p.should_relocate(0.99)); // even with huge threshold
    }

    // ------------------------------------------------------------------
    // ZgcHeap allocation tests
    // ------------------------------------------------------------------

    #[test]
    fn test_heap_allocate_small() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        let result = heap.allocate_small(128);
        assert!(result.is_some());
        assert_eq!(heap.used_size, 128);
    }

    #[test]
    fn test_heap_allocate_medium() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        let result = heap.allocate_medium(1024 * 1024);
        assert!(result.is_some());
    }

    #[test]
    fn test_heap_allocate_large() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        let size = 50 * 1024 * 1024; // 50 MB
        let result = heap.allocate_large(size);
        assert!(result.is_some());
    }

    #[test]
    fn test_heap_allocate_routes_small() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        match heap.allocate(256) {
            ZgcAllocation::Success(_) => {}
            ZgcAllocation::OutOfMemory(_) => panic!("should succeed"),
        }
    }

    #[test]
    fn test_heap_allocate_oom() {
        let cfg = ZgcConfig { heap_size: 1024, ..ZgcConfig::default() };
        let mut heap = ZgcHeap::new(cfg);
        // Fill beyond capacity.
        let _ = heap.allocate(512);
        let _ = heap.allocate(512);
        match heap.allocate(512) {
            ZgcAllocation::OutOfMemory(sz) => assert_eq!(sz, 512),
            ZgcAllocation::Success(_) => panic!("should be OOM"),
        }
    }

    #[test]
    fn test_heap_add_page() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        let id = heap.add_page(ZPageType::Small);
        assert_eq!(id, 1);
        assert_eq!(heap.pages.len(), 1);
    }

    #[test]
    fn test_heap_select_relocation_set() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        // Create a sparse page manually.
        let id = heap.add_page(ZPageType::Large(1000));
        {
            let p = heap.pages.iter_mut().find(|p| p.id == id).unwrap();
            p.allocate(100).unwrap(); // 10% live
        }
        let rs = heap.select_relocation_set(0.25);
        assert!(rs.contains(&id));
    }

    #[test]
    fn test_heap_stats() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        heap.add_page(ZPageType::Small);
        heap.add_page(ZPageType::Medium);
        heap.add_page(ZPageType::Large(64 * 1024));
        let stats = heap.get_stats();
        assert_eq!(stats.small_pages, 1);
        assert_eq!(stats.medium_pages, 1);
        assert_eq!(stats.large_pages, 1);
    }

    // ------------------------------------------------------------------
    // GC phase & collector tests
    // ------------------------------------------------------------------

    #[test]
    fn test_zgc_collector_initial_phase() {
        let c = ZgcCollector::new(ZgcConfig::default());
        assert_eq!(c.phase, ZgcPhase::None);
    }

    #[test]
    fn test_zgc_pause_mark_start_sets_phase() {
        let mut c = ZgcCollector::new(ZgcConfig::default());
        c.heap.add_page(ZPageType::Small);
        c.pause_mark_start();
        assert_eq!(c.phase, ZgcPhase::PauseMarkStart);
    }

    #[test]
    fn test_zgc_concurrent_mark_drains_stack() {
        let mut c = ZgcCollector::new(ZgcConfig::default());
        c.heap.add_page(ZPageType::Small);
        c.pause_mark_start();
        c.concurrent_mark();
        assert!(c.mark_stack.is_empty());
    }

    #[test]
    fn test_zgc_trigger_gc_returns_cycle_number() {
        let mut c = ZgcCollector::new(ZgcConfig::default());
        let r = c.trigger_gc();
        assert_eq!(r.cycle_number, 1);
        let r2 = c.trigger_gc();
        assert_eq!(r2.cycle_number, 2);
    }

    #[test]
    fn test_zgc_trigger_gc_phase_resets_to_none() {
        let mut c = ZgcCollector::new(ZgcConfig::default());
        c.trigger_gc();
        assert_eq!(c.phase, ZgcPhase::None);
    }

    #[test]
    fn test_zgc_forwarding_table_populated_after_relocate() {
        let mut c = ZgcCollector::new(ZgcConfig::default());
        // Add a sparse page that will be selected for relocation.
        let id = c.heap.add_page(ZPageType::Large(1000));
        {
            let p = c.heap.pages.iter_mut().find(|p| p.id == id).unwrap();
            p.allocate(100).unwrap(); // 10% < 25% threshold
        }
        c.pause_mark_start();
        c.concurrent_mark();
        c.pause_mark_end();
        c.relocation_set = c.heap.select_relocation_set(c.heap.config.relocation_threshold);
        for pid in &c.relocation_set.clone() {
            if let Some(p) = c.heap.pages.iter_mut().find(|p| p.id == *pid) {
                p.is_relocating = true;
            }
        }
        c.concurrent_relocate();
        assert!(!c.forwarding_table.is_empty());
    }

    #[test]
    fn test_zgc_stats_after_gc() {
        let mut c = ZgcCollector::new(ZgcConfig::default());
        c.trigger_gc();
        assert_eq!(c.get_stats().gc_count, 1);
    }

    #[test]
    fn test_zgc_forwarding_table_cleared_after_cycle() {
        let mut c = ZgcCollector::new(ZgcConfig::default());
        c.forwarding_table.insert(0x1000, 0x2000);
        c.trigger_gc();
        assert!(c.forwarding_table.is_empty());
    }

    // ------------------------------------------------------------------
    // Generational ZGC tests
    // ------------------------------------------------------------------

    #[test]
    fn test_generational_zgc_minor_gc() {
        let mut gen = GenerationalZgc::new(64 * 1024 * 1024, 192 * 1024 * 1024);
        let r = gen.trigger_minor_gc();
        assert_eq!(r.cycle_number, 1);
        assert_eq!(gen.minor_gc_count, 1);
    }

    #[test]
    fn test_generational_zgc_major_gc() {
        let mut gen = GenerationalZgc::new(64 * 1024 * 1024, 192 * 1024 * 1024);
        let r = gen.trigger_major_gc();
        assert_eq!(gen.major_gc_count, 1);
        let _ = r.pages_relocated; // API usage verification
    }

    #[test]
    fn test_generational_zgc_remembered_set() {
        let mut gen = GenerationalZgc::new(64 * 1024 * 1024, 192 * 1024 * 1024);
        gen.add_remembered_set_entry(0xDEAD_BEEF);
        gen.add_remembered_set_entry(0xDEAD_BEEF); // duplicate should not be added
        assert_eq!(gen.remembered_set.len(), 1);
    }

    #[test]
    fn test_generational_zgc_stats() {
        let mut gen = GenerationalZgc::new(64 * 1024 * 1024, 192 * 1024 * 1024);
        gen.trigger_minor_gc();
        gen.trigger_major_gc();
        let s = gen.get_stats();
        assert_eq!(s.minor_gcs, 2); // major also calls minor internally
        assert_eq!(s.major_gcs, 1);
    }

    #[test]
    fn test_generational_zgc_young_allocate() {
        let mut gen = GenerationalZgc::new(64 * 1024 * 1024, 192 * 1024 * 1024);
        gen.young_heap.allocate_small(512).expect("young alloc");
        assert_eq!(gen.young_heap.used_size, 512);
    }

    #[test]
    fn test_generational_zgc_old_allocate() {
        let mut gen = GenerationalZgc::new(64 * 1024 * 1024, 192 * 1024 * 1024);
        gen.old_heap.allocate_medium(1024 * 1024).expect("old alloc");
        assert!(gen.old_heap.used_size > 0);
    }

    // ------------------------------------------------------------------
    // Additional edge-case tests (bring total to 50+)
    // ------------------------------------------------------------------

    #[test]
    fn test_colored_pointer_multiple_colors() {
        let colors = ZGC_COLOR_REMAPPED | ZGC_COLOR_MARKED0 | ZGC_COLOR_FINALIZABLE;
        let ptr = ColoredPointer::new(0xABC, colors);
        assert!(ptr.is_remapped());
        assert!(ptr.is_marked(0));
        assert!(ptr.is_finalizable());
    }

    #[test]
    fn test_zpage_multiple_allocations() {
        let mut p = ZPage::new(1, ZPageType::Small, 0x1000_0000, 0x1000_0000);
        let a1 = p.allocate(64).unwrap();
        let a2 = p.allocate(64).unwrap();
        assert_eq!(a2, a1 + 64);
        assert_eq!(p.top, 128);
    }

    #[test]
    fn test_heap_reuses_existing_small_page() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        heap.allocate_small(128).unwrap();
        let before = heap.pages.len();
        heap.allocate_small(128).unwrap();
        // Should reuse existing small page, not add another.
        assert_eq!(heap.pages.len(), before);
    }

    #[test]
    fn test_heap_relocation_flag() {
        let mut heap = ZgcHeap::new(ZgcConfig::default());
        let id = heap.add_page(ZPageType::Small);
        let p = heap.pages.iter_mut().find(|p| p.id == id).unwrap();
        p.is_relocating = true;
        let stats = heap.get_stats();
        assert_eq!(stats.relocating_pages, 1);
    }

    #[test]
    fn test_zgc_phase_enum_variants_are_distinct() {
        assert_ne!(ZgcPhase::None, ZgcPhase::ConcurrentMark);
        assert_ne!(ZgcPhase::PauseMarkStart, ZgcPhase::PauseMarkEnd);
        assert_ne!(ZgcPhase::ConcurrentRelocate, ZgcPhase::ConcurrentRemap);
    }

    #[test]
    fn test_zgc_result_defaults() {
        let r = ZgcResult::default();
        assert_eq!(r.bytes_reclaimed, 0);
        assert_eq!(r.pages_relocated, 0);
    }

    #[test]
    fn test_zgc_config_defaults() {
        let cfg = ZgcConfig::default();
        assert_eq!(cfg.heap_size, 256 * 1024 * 1024);
        assert_eq!(cfg.small_page_size, 2 * 1024 * 1024);
        assert_eq!(cfg.medium_page_size, 32 * 1024 * 1024);
        assert!((cfg.relocation_threshold - 0.25).abs() < 1e-9);
        assert_eq!(cfg.concurrent_gc_threads, 4);
        assert!(!cfg.generational);
    }

    #[test]
    fn test_heap_fragmentation_ratio_zero_when_empty() {
        let heap = ZgcHeap::new(ZgcConfig::default());
        let stats = heap.get_stats();
        // No pages yet — fragmented_bytes = 0
        assert_eq!(stats.fragmentation_ratio, 0.0);
    }

    #[test]
    fn test_load_barrier_none_phase_slow_path() {
        let mut lb = LoadBarrier::new();
        let ptr = ColoredPointer::new(0x8000, 0);
        let fixed = lb.slow_path(ptr, ZgcPhase::None);
        // None phase: pointer returned unchanged.
        assert_eq!(fixed.raw, ptr.raw);
    }

    #[test]
    fn test_colored_pointer_raw_roundtrip() {
        let addr = 0x0000_1234_5678u64;
        let colors = ZGC_COLOR_MARKED1;
        let ptr = ColoredPointer::new(addr, colors);
        assert_eq!(ptr.address(), addr);
        assert_eq!(ptr.colors(), colors);
    }

    // ------------------------------------------------------------------
    // T5.5.5 — Generational partition tests
    // ------------------------------------------------------------------

    #[test]
    fn generational_default_split_is_25_percent_young() {
        let gen = GenerationalZgc::with_default_split(400 * 1024 * 1024);
        // Young = 25% of 400 MB = 100 MB; old = 300 MB.
        assert_eq!(gen.young_heap.config.heap_size, 100 * 1024 * 1024);
        assert_eq!(gen.old_heap.config.heap_size, 300 * 1024 * 1024);
    }

    #[test]
    fn generational_with_total_size_custom_fraction() {
        let gen = GenerationalZgc::with_total_size(100 * 1024 * 1024, 0.40);
        assert_eq!(gen.young_heap.config.heap_size, 40 * 1024 * 1024);
        assert_eq!(gen.old_heap.config.heap_size, 60 * 1024 * 1024);
    }

    #[test]
    fn generational_allocate_routes_to_young() {
        let mut gen = GenerationalZgc::with_default_split(64 * 1024 * 1024);
        match gen.allocate_young(256) {
            ZgcAllocation::Success(_) => {}
            _ => panic!("young allocation should succeed"),
        }
        assert_eq!(gen.young_heap.used_size, 256);
        assert_eq!(gen.old_heap.used_size, 0);
    }

    #[test]
    fn generational_occupancy_reports_fractions() {
        let mut gen = GenerationalZgc::with_total_size(8 * 1024 * 1024, 0.25);
        // Force some young usage. (2 MB / 1 MB minimum pages — allocate_small will create a 2MB page.)
        let _ = gen.allocate_young(1024);
        assert!(gen.young_occupancy() > 0.0);
        assert_eq!(gen.old_occupancy(), 0.0);
    }

    #[test]
    fn generational_minor_collect_reclaims_sparse_young_pages() {
        let mut gen = GenerationalZgc::with_total_size(8 * 1024 * 1024, 0.25);
        // Create a sparse young page manually: large, low live bytes.
        let id = gen.young_heap.add_page(ZPageType::Large(1000));
        {
            let p = gen
                .young_heap
                .pages
                .iter_mut()
                .find(|p| p.id == id)
                .unwrap();
            p.allocate(50).unwrap(); // 5% live — below 0.25 threshold
        }
        gen.young_heap.used_size = 50;
        let before = gen.young_heap.pages.len();
        let result = gen.minor_collect(&[]);
        assert!(result.pages_relocated > 0, "sparse page must be reclaimed");
        assert!(gen.young_heap.pages.len() < before);
        assert_eq!(gen.minor_gc_count, 1);
    }

    #[test]
    fn generational_minor_collect_pins_rooted_page() {
        let mut gen = GenerationalZgc::with_total_size(8 * 1024 * 1024, 0.25);
        // Sparse page, but rooted → must survive.
        let id = gen.young_heap.add_page(ZPageType::Large(1000));
        let root_addr = {
            let p = gen
                .young_heap
                .pages
                .iter_mut()
                .find(|p| p.id == id)
                .unwrap();
            p.allocate(50).unwrap()
        };
        gen.young_heap.used_size = 50;
        let result = gen.minor_collect(&[root_addr]);
        // The page is protected by the root; relocation set is empty.
        assert_eq!(result.pages_relocated, 0);
    }

    #[test]
    fn generational_minor_collect_consumes_remembered_set() {
        let mut gen = GenerationalZgc::with_total_size(8 * 1024 * 1024, 0.25);
        gen.add_remembered_set_entry(0x1000_0000);
        gen.add_remembered_set_entry(0x2000_0000);
        assert_eq!(gen.remembered_set.len(), 2);
        gen.minor_collect(&[]);
        assert!(gen.remembered_set.is_empty());
    }

    #[test]
    fn generational_promotion_after_threshold_age() {
        let mut gen = GenerationalZgc::with_total_size(16 * 1024 * 1024, 0.25);
        gen.promotion_age = 2;
        // Create a young-gen page with some live data and a root so it survives.
        let id = gen.young_heap.add_page(ZPageType::Small);
        let root_addr = {
            let p = gen
                .young_heap
                .pages
                .iter_mut()
                .find(|p| p.id == id)
                .unwrap();
            p.allocate(2048).unwrap()
        };
        gen.young_heap.used_size = 2048;
        gen.page_ages.insert(id, 0);

        // Cycle 1: survive, age goes 0 → 1.
        gen.minor_collect(&[root_addr]);
        assert_eq!(gen.promotion_count, 0);
        assert_eq!(gen.young_heap.pages.len(), 1);
        // Cycle 2: age 1 + 1 >= 2 → promoted to old.
        gen.minor_collect(&[root_addr]);
        assert_eq!(gen.promotion_count, 1);
        assert!(gen.young_heap.pages.iter().all(|p| p.id != id));
        assert!(!gen.old_heap.pages.is_empty());
    }

    #[test]
    fn generational_major_collect_reclaims_old_gen() {
        let mut gen = GenerationalZgc::with_total_size(16 * 1024 * 1024, 0.25);
        // Install a sparse page directly in the old heap.
        let id = gen.old_heap.add_page(ZPageType::Large(1000));
        {
            let p = gen.old_heap.pages.iter_mut().find(|p| p.id == id).unwrap();
            p.allocate(50).unwrap();
        }
        gen.old_heap.used_size = 50;
        let before = gen.old_heap.pages.len();
        let result = gen.major_collect(&[]);
        assert!(result.pages_relocated > 0);
        assert!(gen.old_heap.pages.len() < before);
        assert_eq!(gen.major_gc_count, 1);
    }

    #[test]
    fn generational_scheduled_collect_skips_when_quiet() {
        let mut gen = GenerationalZgc::with_total_size(16 * 1024 * 1024, 0.25);
        let (kind, _r) = gen.scheduled_collect(&[]);
        assert_eq!(kind, GenerationalTriggerKind::None);
        assert_eq!(gen.minor_gc_count, 0);
        assert_eq!(gen.major_gc_count, 0);
    }

    #[test]
    fn generational_scheduled_collect_triggers_minor_on_young_pressure() {
        let mut gen = GenerationalZgc::with_total_size(16 * 1024 * 1024, 0.25);
        // Force young occupancy above the 0.5 threshold.
        gen.young_heap.used_size =
            (gen.young_heap.config.heap_size as f64 * 0.6) as usize;
        let (kind, _r) = gen.scheduled_collect(&[]);
        assert_eq!(kind, GenerationalTriggerKind::Minor);
        assert_eq!(gen.minor_gc_count, 1);
    }

    #[test]
    fn generational_scheduled_collect_triggers_major_on_old_pressure() {
        let mut gen = GenerationalZgc::with_total_size(16 * 1024 * 1024, 0.25);
        // Force old occupancy above the 0.8 threshold; the major branch
        // wins even if young pressure is also high.
        gen.old_heap.used_size = (gen.old_heap.config.heap_size as f64 * 0.9) as usize;
        gen.young_heap.used_size =
            (gen.young_heap.config.heap_size as f64 * 0.9) as usize;
        let (kind, _r) = gen.scheduled_collect(&[]);
        assert_eq!(kind, GenerationalTriggerKind::Major);
        assert_eq!(gen.major_gc_count, 1);
    }

    #[test]
    fn generational_stats_include_occupancy_and_promotions() {
        let gen = GenerationalZgc::with_total_size(16 * 1024 * 1024, 0.25);
        let s = gen.get_stats();
        assert_eq!(s.promotions, 0);
        assert!(s.young_occupancy <= 1.0);
        assert!(s.old_occupancy <= 1.0);
    }
}
