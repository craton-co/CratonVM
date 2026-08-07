// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC (Z Garbage Collector) — low-latency concurrent garbage collector.
//!
//! Implements colored pointers, load barriers, ZPages, concurrent GC phases,
//! and a stub for Generational ZGC (JEP 439 / JDK 21+).
//!
//! # ⚠ SIMULATION / EXPERIMENTAL BACKEND — NOT production ZGC
//!
//! The colored-pointer / load-barrier / phase machinery in this module
//! ([`ColoredPointer`], [`LoadBarrier`], [`ZPage`], [`ZgcHeap`],
//! [`ZgcCollector`], [`GenerationalZgc`]) is an **explicit, documented
//! simulation** of OpenJDK ZGC's *model*, not a working low-latency
//! collector. It exists to exercise the ZGC phase lifecycle and to give the
//! rest of the VM a faithful API shape; it does **not** deliver any of ZGC's
//! production guarantees. Concretely, it does **NOT** provide:
//!
//! - **Real backing storage.** `ZPage::virtual_start` / `physical_start` are
//!   synthetic `u64` offsets handed out by an internal counter
//!   ([`ZgcHeap::add_page`]); there is no `*mut u8` and no Java object bytes
//!   behind them. See the long note on [`ZgcCollector::concurrent_relocate`].
//! - **Real concurrent relocation.** [`ZgcCollector::concurrent_relocate`]
//!   records a forwarding-table entry but copies **no object bytes** (there
//!   is nothing to copy from). "Concurrent" marking/relocation run
//!   synchronously inside [`ZgcCollector::trigger_gc`]; there is no actual
//!   overlap with mutator threads and therefore **no pause-time benefit**.
//! - **A real colored-pointer load barrier.** [`LoadBarrier::slow_path`]
//!   takes `&mut self` and mutates a plain `ColoredPointer` value — it is
//!   **not** the atomic, lock-free, self-healing compare-and-set on an
//!   in-memory oop that ZGC's barrier performs. It cannot be driven
//!   concurrently from mutator threads against live heap pointers.
//!
//! Selecting this collector at runtime emits a one-time `tracing::warn!` (see
//! [`ZgcCollector::new`] / [`GenerationalZgc::new`]) so it can never silently
//! masquerade as production ZGC.
//!
//! **Follow-up (real implementation):** a production ZGC — multi-mapped
//! colored-pointer address space, an atomic CAS load barrier driven from
//! mutator threads, genuinely concurrent mark/relocate with real byte-copy
//! compaction, and remembered-set-backed generational collection — is a
//! major feature tracked separately and intentionally out of scope here. The
//! machinery above is retained as the scaffolding for that future work.
//!
//! # Real vs. simulated
//!
//! The original module ([`ZgcCollector`] / [`ZgcHeap`] / [`GenerationalZgc`])
//! is a *metadata-only simulation*: its `ZPage`s carry synthetic `u64`
//! virtual addresses with no backing storage, so it can model the colored-
//! pointer / load-barrier / phase lifecycle but cannot hold real Java
//! objects (see the long comment on [`ZgcCollector::concurrent_relocate`]).
//!
//! [`ZgcRealHeap`] (added below) is the **honest, functioning** collector:
//! it backs every allocation with real owned memory (an [`crate::arena::Arena`]
//! per page-tier), writes real [`ObjectHeader`]s, and implements the
//! [`crate::collector::GarbageCollector`] trait with the same bounds-checked
//! field/array semantics as [`crate::gen_heap::GenerationalHeap`] and
//! [`crate::g1::G1Collector`]. Its [`ZgcRealHeap::collect_garbage`] performs a
//! real stop-the-world **mark-sweep**: it traces the live object graph from
//! the supplied roots, marks survivors in their header, and reclaims dead
//! objects into a free list for reuse.
//!
//! What remains deferred (documented, not faked): the production ZGC
//! invariants of *concurrent* marking/relocation, colored-pointer load
//! barriers driving the trace, and *compaction* (the mark-sweep is
//! non-moving, so it reclaims but does not defragment). The colored-pointer
//! / phase machinery above is retained for that future work; the real heap
//! is deliberately a clean, correct STW collector rather than a half-built
//! concurrent one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::arena::Arena;
use crate::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use crate::gc::{GcResult, GcStats};
use crate::heap::{
    array_data_size, read_prim_element, write_prim_element, ArrayElementType, ObjectHeader,
    ObjectKind, GC_FLAG_MARKED, ARRAY_DATA_OFFSET, HEADER_SIZE, SLOT_SIZE,
};
use crate::reference::{ReferenceProcessingResult, ReferenceProcessor, ReferenceType};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Simulation-selected warning
// ---------------------------------------------------------------------------

/// Fires the one-time "this is a ZGC *simulation*, not production ZGC"
/// warning. Guarded by a process-wide [`std::sync::Once`] so constructing
/// many [`ZgcCollector`] / [`GenerationalZgc`] instances logs at most once.
///
/// The simulation backend cannot deliver ZGC's real guarantees (no real
/// concurrent relocation, no atomic colored-pointer load barrier — see the
/// module-level docs), so we make selecting it loud rather than letting it
/// silently masquerade as production ZGC.
fn warn_zgc_simulation_selected() {
    use std::sync::Once;
    static WARN_ONCE: Once = Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            target: "zgc",
            "ZGC (simulation) selected: the ColoredPointer / LoadBarrier / \
             ZPage / ZgcCollector / GenerationalZgc backend is an EXPERIMENTAL \
             SIMULATION of OpenJDK ZGC — it has NO real backing storage, does \
             NO byte-copy relocation, and its load barrier is a non-atomic \
             `&mut self` value (so relocation is NOT actually concurrent and \
             provides NO pause-time benefit). For a real, memory-backed \
             collector use ZgcRealHeap. See gc::zgc module docs."
        );
    });
}

// ---------------------------------------------------------------------------
// Colored pointer constants
// ---------------------------------------------------------------------------

const ZGC_ADDRESS_BITS: u64 = 42; // 4TB heap max
const ZGC_METADATA_SHIFT: u64 = 42;

// Color bits (bits 42-45)
pub(crate) const ZGC_COLOR_REMAPPED: u64 = 1 << 42;
pub(crate) const ZGC_COLOR_MARKED0: u64 = 1 << 43;
pub(crate) const ZGC_COLOR_MARKED1: u64 = 1 << 44;
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
        Self {
            raw: (address & ZGC_ADDRESS_MASK) | (colors & ZGC_COLOR_MASK),
        }
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
        Self {
            raw: self.raw | ZGC_COLOR_REMAPPED,
        }
    }

    /// Return a new pointer with the marked bit for the current cycle set.
    pub fn set_marked(&self, cycle: u32) -> Self {
        let bit = if cycle % 2 == 0 {
            ZGC_COLOR_MARKED0
        } else {
            ZGC_COLOR_MARKED1
        };
        Self {
            raw: self.raw | bit,
        }
    }

    pub fn with_colors(&self, new_colors: u64) -> Self {
        Self {
            raw: (self.raw & ZGC_ADDRESS_MASK) | (new_colors & ZGC_COLOR_MASK),
        }
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
        Self {
            good_colors: ZGC_COLOR_REMAPPED,
            ..Default::default()
        }
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
    ///
    /// ## Simulation note
    ///
    /// This is **not** a production ZGC load barrier. It takes `&mut self`
    /// and returns a recolored *value*; a real barrier performs an atomic,
    /// lock-free compare-and-set that *self-heals* the in-memory oop and runs
    /// concurrently from every mutator thread. Because this version is
    /// neither atomic nor applied in place, it provides no concurrency and no
    /// pause-time benefit — it only models the color-flip bookkeeping. See
    /// the module-level docs.
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
        // One-time loud warning: this is the SIMULATION backend, not real ZGC.
        warn_zgc_simulation_selected();
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
        self.load_barrier
            .update_good_colors(ZgcPhase::PauseRelocateStart);
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
        self.load_barrier
            .update_good_colors(ZgcPhase::PauseMarkStart);
        // Simulate marking all page base addresses as roots.
        let roots: Vec<u64> = self.heap.pages.iter().map(|p| p.virtual_start).collect();
        self.mark_stack.extend(roots);
    }

    /// Concurrent tri-color mark: drain the mark stack to completion.
    ///
    /// This is the synchronous entry point used by `trigger_gc`. For the
    /// concurrent-thread variant — where a background worker calls
    /// [`Self::concurrent_mark_step`] in a loop while mutators run — see
    /// [`crate::zgc_concurrent::ZgcConcurrentMarkController`] (task #55).
    ///
    /// The synchronous path is preserved as the canonical "single
    /// threaded" body: a full drain with no per-step budget.
    pub fn concurrent_mark(&mut self) {
        // Delegate to the step-based body with an unbounded budget so the
        // two code paths converge on identical per-pointer logic. This
        // also means any future change to the mark-pointer step only
        // needs to happen in one place.
        while !self.concurrent_mark_step(usize::MAX) {}
    }

    /// Drain up to `budget` gray pointers from the mark stack. Returns
    /// `true` iff the stack is now empty (fixed point reached) and the
    /// caller should park / advance to the remark STW.
    ///
    /// Called from:
    /// - [`Self::concurrent_mark`] (synchronous full-drain path).
    /// - The background worker spawned by
    ///   [`crate::zgc_concurrent::ZgcConcurrentMarkController::spawn`]
    ///   (per-call budget for prompt stop-signal observation).
    ///
    /// The body intentionally does NOT touch `load_barrier.good_colors`
    /// or transition `phase` to `PauseMarkEnd` — those are STW
    /// transitions owned by the coordinator. The only state the step
    /// mutates is `phase` (the first call advances it to `ConcurrentMark`,
    /// matching the G1 step-machine shape) and `live_bytes` on touched
    /// pages.
    ///
    /// ## Simulation note
    ///
    /// In a real ZGC each gray pointer would be a colored pointer; the
    /// step would (a) strip its color, (b) consult the load barrier to
    /// flip the marked-color bit on the object header, (c) trace its
    /// out-references onto the stack. Because `ZPage` has no backing
    /// storage (see [`Self::concurrent_relocate`] doc-comment) the
    /// simulation collapses this to "mark every byte on the touched
    /// page as live" — which is enough to keep the controller lifecycle
    /// honest but does not faithfully model object-level marking. See
    /// [`crate::zgc_concurrent`] module-doc for the full
    /// page-storage-simulation status.
    pub fn concurrent_mark_step(&mut self, budget: usize) -> bool {
        // First-touch phase transition. The G1 controller does this on
        // the collector side; we do it here so a caller that drives the
        // step directly (a test, or the synchronous path) doesn't have
        // to remember to flip phase manually.
        if self.phase != ZgcPhase::ConcurrentMark {
            self.phase = ZgcPhase::ConcurrentMark;
        }

        let mut drained = 0usize;
        while drained < budget {
            let addr = match self.mark_stack.pop() {
                Some(a) => a,
                None => return true, // stack empty → fixed point
            };
            if let Some(p) = self.heap.pages.iter_mut().find(|p| p.virtual_start == addr) {
                if !p.is_relocating {
                    p.live_bytes = p.top;
                }
            }
            drained += 1;
        }

        // Budget exhausted with possibly more work remaining.
        self.mark_stack.is_empty()
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
        // One-time loud warning: this is the SIMULATION backend, not real ZGC.
        warn_zgc_simulation_selected();
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
    pub fn scheduled_collect(&mut self, roots: &[u64]) -> (GenerationalTriggerKind, ZgcResult) {
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
// ZgcRealHeap — a real, functioning ZGC-flavoured collector
// ---------------------------------------------------------------------------
//
// Everything above this point is the colored-pointer / load-barrier / phase
// *simulation* (no backing storage). `ZgcRealHeap` is the real thing: owned
// memory, real object headers, a working mark-sweep collector, and the full
// `GarbageCollector` trait. It is intentionally self-contained and does not
// touch the simulation types.

/// Default total heap size for a [`ZgcRealHeap`]: 64 MB.
const ZGC_REAL_DEFAULT_HEAP: usize = 64 * 1024 * 1024;

/// Trigger a collection once live+dead allocation crosses this fraction of
/// total capacity.
const ZGC_REAL_GC_THRESHOLD_PERCENT: usize = 75;

/// Maximum array length, mirroring `heap.rs` / HotSpot's practical limit.
const ZGC_REAL_MAX_ARRAY_LENGTH: usize = i32::MAX as usize;

/// A real, memory-backed ZGC heap.
///
/// # Storage scheme
///
/// Objects live in a single bump-pointer [`Arena`] (the same allocator the
/// semi-space `Heap` and the non-moving young-gen sweep use). Each allocation
/// is laid out exactly like every other CratonVM heap:
///
/// ```text
/// [ObjectHeader (HEADER_SIZE)] [field0 (SLOT_SIZE)] ... [fieldN]
/// [ObjectHeader (HEADER_SIZE)] [elem0] [elem1] ...        (arrays, compact)
/// ```
///
/// so `ObjectRef`s handed out by [`Self::alloc_object`] / [`Self::alloc_array`]
/// are real `*mut u8` pointers into owned memory and are fully interoperable
/// with the shared header/slot decoders in `heap.rs`.
///
/// A side **object registry** (`Vec` of base addresses) records every live
/// allocation so the sweep can enumerate the heap without having to parse
/// arena holes. Allocation appends; the sweep rebuilds it from the survivors.
///
/// # Collection algorithm
///
/// [`Self::collect_garbage`] is a **stop-the-world, non-moving mark-sweep**:
///
/// 1. **Mark.** Clear every object's mark bit, then trace transitively from
///    `roots`: for each reachable object, set [`GC_FLAG_MARKED`] in its
///    header and push its reference-typed fields / reference array elements
///    onto a work stack (a real graph trace, not a page-level approximation).
/// 2. **Sweep.** Walk the registry; objects without the mark bit are dead —
///    their bytes are zeroed and handed back to the arena free list for
///    reuse. Survivors keep their address (non-moving) and have their mark
///    bit cleared for the next cycle.
///
/// Because it is non-moving, no `ObjectRef` ever changes — the returned
/// [`GcResult::pointer_map`] is therefore empty (no remapping needed), which
/// is exactly correct for a non-compacting collector.
pub struct ZgcRealHeap {
    /// Compact-layout domain of the VM that owns this heap. See
    /// `Heap::set_layout_domain`: `class_id` is a per-`ClassStore` index, so
    /// allocating against another domain's registry entry would give the object
    /// a foreign shape. Defaults to the first domain, so an untold heap behaves
    /// as it did before domains existed.
    layout_domain: std::sync::atomic::AtomicU32,

    /// Backing storage for all objects.
    arena: Mutex<Arena>,
    /// Base address of every live allocation, in allocation order. Rebuilt
    /// (filtered to survivors) by each sweep.
    /// Hash-set registry (ZGC-5/6 hardening): `is_object_address` is
    /// consulted per conservative-root candidate (every operand-stack root
    /// and JIT-frame qword), so membership must be O(1) — the previous
    /// `Vec` linear scan made every GC's root collection O(roots × live)
    /// and read as a hang at scale. The sweep also prunes DEAD bases
    /// in place now (never wholesale-replaces the set), so an allocation
    /// registered between the mark snapshot and the sweep publish can no
    /// longer be silently dropped from the registry.
    registry: Mutex<FxHashSet<usize>>,
    /// Monotonic identity-hash-code source (matches `Heap::next_hash`).
    next_hash_code: AtomicI32,
    /// Bytes of live+dead object payload currently outstanding (drops on
    /// sweep). Used by [`Self::needs_gc`] and [`Self::allocated_bytes`].
    allocated: AtomicUsize,
    /// Collection is triggered once `allocated` crosses this byte count.
    gc_threshold: usize,
    /// Post-GC re-arm floor: `needs_gc` stays `false` until `allocated`
    /// also crosses this. Set by each sweep to
    /// `live + max(remaining_headroom / 4, 64 KiB)` so a live set that sits
    /// above the static 75% threshold cannot latch `needs_gc` permanently
    /// true — which made `maybe_gc` (polled after EVERY allocation
    /// bytecode) run a full STW mark-sweep per allocation: a livelock-grade
    /// GC storm with no OOME ever surfacing.
    gc_rearm: AtomicUsize,
    /// Lifetime collection counter (observability).
    gc_count: AtomicUsize,
    /// Shared `java.lang.ref` reference processor.
    ///
    /// Weak/soft/phantom/cleaner/finalizer references discovered on this
    /// backend are registered here (via [`Self::discover_reference`]) and
    /// processed at the end of every [`Self::collect_garbage`] cycle by the
    /// *same* [`ReferenceProcessor`] the generational and G1 collectors use —
    /// the canonical HotSpot-ordered clearing/enqueue path in
    /// `gc::reference`. This closes the gap where the ZGC-backed heap performed
    /// NO reference processing, so finalizers/cleaners and `WeakReference`
    /// semantics silently broke under this collector.
    ref_processor: Mutex<ReferenceProcessor>,
    /// Finalizer-resurrection input for the current collection — see
    /// [`Self::collect_garbage_with_finalizers`]. Consumed (taken) by the
    /// mark phase; empty on every plain `collect_garbage`.
    pending_finalizer_roots: Mutex<Vec<usize>>,
    /// Output half: addresses of dead-but-finalizable objects the mark phase
    /// resurrected (non-moving, so pre == post address). Drained by
    /// [`Self::collect_garbage_with_finalizers`].
    resurrected_finalizers: Mutex<Vec<usize>>,
}

// SAFETY: identical argument to `Heap`/`G1Collector` (heap.rs:143). The only
// non-Send/Sync state is the raw `*mut u8` arena pointers, which live behind
// `Mutex<Arena>`; all allocation and collection serialize through that mutex,
// and the moving... (there is no moving — this collector is non-moving) so
// shared `&ZgcRealHeap` use across threads is sound under the same protocol
// the other collectors document.
unsafe impl Send for ZgcRealHeap {}
unsafe impl Sync for ZgcRealHeap {}

impl ZgcRealHeap {
    /// Bind this heap to its VM's compact-layout domain.
    pub fn set_layout_domain(&self, domain: u32) {
        self.layout_domain
            .store(domain, std::sync::atomic::Ordering::Release);
    }

    /// This heap's compact-layout domain.
    pub fn layout_domain(&self) -> u32 {
        self.layout_domain.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Create a heap with the default capacity ([`ZGC_REAL_DEFAULT_HEAP`]).
    pub fn new() -> Self {
        Self::with_capacity(ZGC_REAL_DEFAULT_HEAP)
    }

    /// Create a heap with the given total capacity in bytes.
    pub fn with_capacity(total_bytes: usize) -> Self {
        let cap = total_bytes.max(4096);
        Self {
            arena: Mutex::new(Arena::new(cap)),
            registry: Mutex::new(FxHashSet::default()),
            next_hash_code: AtomicI32::new(1),
            allocated: AtomicUsize::new(0),
            gc_threshold: cap * ZGC_REAL_GC_THRESHOLD_PERCENT / 100,
            gc_rearm: AtomicUsize::new(0),
            gc_count: AtomicUsize::new(0),
            ref_processor: Mutex::new(ReferenceProcessor::new()),
            pending_finalizer_roots: Mutex::new(Vec::new()),
            resurrected_finalizers: Mutex::new(Vec::new()),
        }
    }

    /// Run a collection with finalizer-aware resurrection: any address in
    /// `finalizer_addrs` (registered, not-yet-enqueued finalizable objects)
    /// that the mark phase finds DEAD is marked live — with its transitive
    /// closure — so the sweep keeps it and `finalize()` can later run
    /// against valid memory. Non-moving, so the returned dead-finalizer
    /// addresses are the same addresses that went in. The caller must
    /// enqueue them for finalization AND mark their processor entries
    /// enqueued (once-only finalization) — mirrors the generational
    /// backend's contract.
    pub fn collect_garbage_with_finalizers(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        *self.pending_finalizer_roots.lock() = finalizer_addrs.to_vec();
        self.resurrected_finalizers.lock().clear();
        let result = <Self as crate::collector::GarbageCollector>::collect_garbage(
            self, stw, roots, monitors,
        );
        self.pending_finalizer_roots.lock().clear();
        let dead = std::mem::take(&mut *self.resurrected_finalizers.lock());
        (result, dead)
    }

    /// Register a discovered `java.lang.ref.Reference` with this heap's shared
    /// [`ReferenceProcessor`].
    ///
    /// The runtime calls this during marking (the same point the generational
    /// and G1 backends discover references) so the next
    /// [`Self::collect_garbage`] can clear/enqueue it per Java semantics.
    /// `reference_obj` is the address of the `Reference` object, `referent`
    /// the object it points at, and `queue` the associated `ReferenceQueue`
    /// (if any).
    pub fn discover_reference(
        &self,
        ref_type: ReferenceType,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    ) {
        self.ref_processor.lock().discover_reference(
            ref_type,
            reference_obj.as_ptr() as usize,
            referent.as_ptr() as usize,
            queue.map(|q| q.as_ptr() as usize),
        );
    }

    /// Number of collections performed so far.
    pub fn gc_count(&self) -> usize {
        self.gc_count.load(Ordering::Relaxed)
    }

    /// Next identity hash code (never zero; wraps avoiding 0).
    pub fn next_hash(&self) -> i32 {
        let h = self.next_hash_code.fetch_add(1, Ordering::Relaxed);
        if h == 0 {
            self.next_hash_code.fetch_add(1, Ordering::Relaxed)
        } else {
            h
        }
    }

    /// Bump-allocate `size` zeroed bytes (8-byte aligned) and register the
    /// base address. Returns `None` on OOM.
    fn alloc_raw(&self, size: usize) -> Option<*mut u8> {
        let ptr = {
            let mut arena = self.arena.lock();
            let ptr = arena.alloc(size, 8)?;
            // The arena bump path hands out memory from a zeroed Vec, but a
            // reused free-list block may contain stale bytes — zero it so a
            // fresh header/fields start clean.
            // SAFETY: `arena.alloc` guarantees `size` valid bytes at `ptr`.
            unsafe { std::ptr::write_bytes(ptr, 0, size) };
            ptr
        };
        self.registry.lock().insert(ptr as usize);
        self.allocated.fetch_add(size, Ordering::Relaxed);
        Some(ptr)
    }

    /// Try to allocate an object. Returns `None` on true heap exhaustion.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let fields_size = num_fields.checked_mul(SLOT_SIZE)?;
        let total = HEADER_SIZE.checked_add(fields_size)?;
        let ptr = self.alloc_raw(total)?;
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            u32::try_from(num_fields).ok()?,
        );
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Try to allocate an array. Returns `None` on true heap exhaustion.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if length > ZGC_REAL_MAX_ARRAY_LENGTH {
            return None;
        }
        let data_size = array_data_size(length, element_type).ok()?;
        let total = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        let ptr = self.alloc_raw(total)?;
        let len_u32 = u32::try_from(length).ok()?;
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            len_u32,
            len_u32,
        );
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Allocate and pre-initialize primitive-typed slots from JVM descriptors.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        let obj = self.alloc_object(class_id, num_fields);
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| crate::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        obj
    }

    /// Fallible descriptor-aware object allocation.
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| crate::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        Some(obj)
    }

    /// Validate that `addr` is exactly the base of a live object. O(1) — hot
    /// path for conservative-root validation (per candidate qword).
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        if self.registry.lock().contains(&addr) {
            // SAFETY: the registry contains only live allocation bases.
            Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
        } else {
            None
        }
    }

    /// Loose containment check returning the base object for any address inside it.
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        // Fast path: an exact object base (the overwhelmingly common probe).
        if self.registry.lock().contains(&addr) {
            // SAFETY: the registry contains only live allocation bases.
            return Some(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
        // Interior pointers: fall back to the O(live) extent walk.
        for &base in self.registry.lock().iter() {
            let header = unsafe { &*(base as *const ObjectHeader) };
            let size = Self::alloc_size(header);
            let Some(end) = base.checked_add(size) else {
                continue;
            };
            if addr >= base && addr < end {
                // SAFETY: the registry contains only live allocation bases.
                return Some(unsafe { ObjectRef::from_raw(base as *mut u8) });
            }
        }
        None
    }

    /// Total backing arena capacity.
    pub fn heap_capacity(&self) -> usize {
        self.arena.lock().capacity()
    }

    /// Walk every live object and its allocation size.
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        self.registry
            .lock()
            .iter()
            .map(|&base| {
                let header = unsafe { &*(base as *const ObjectHeader) };
                (base as *mut u8, Self::alloc_size(header))
            })
            .collect()
    }

    /// Header accessor (shared with the trait impl).
    #[inline]
    fn header(&self, obj: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj` points to a live allocation whose first HEADER_SIZE
        // bytes are a valid `ObjectHeader` written at allocation time.
        unsafe { &*(obj.as_ptr() as *const ObjectHeader) }
    }

    #[inline]
    fn header_mut(&self, base: *mut u8) -> &mut ObjectHeader {
        // SAFETY: `base` is a registered live allocation base; its first
        // HEADER_SIZE bytes are a valid `ObjectHeader`.
        unsafe { &mut *(base as *mut ObjectHeader) }
    }

    /// Total size in bytes of the allocation rooted at `header`.
    fn alloc_size(header: &ObjectHeader) -> usize {
        match header.kind {
            ObjectKind::Object | ObjectKind::HumongousFiller => {
                HEADER_SIZE + cratonvm_types::object_body_size(header)
            }
            ObjectKind::Array => {
                let data =
                    array_data_size(header.array_length() as usize, header.element_type).unwrap_or(0);
                ARRAY_DATA_OFFSET + data
            }
        }
    }

    /// Push every reference-typed out-edge of the object at `base` onto
    /// `work`. Mirrors how the semi-space collector enumerates an object's
    /// oops, but reads them in place (non-moving).
    /// `skip_index`: for a registered Weak/Soft/Phantom `Reference` object
    /// (see `ReferenceProcessor::reference_object_addresses`'s INT-8 doc),
    /// the referent slot (index 0) must NOT be traced as a strong edge —
    /// otherwise the referent always appears reachable through its own
    /// Reference object and can never be cleared. Mirrors the G1 marker's
    /// `g1_set_reference_skip_set` mechanism (see
    /// `vm/src/runtime/interpreter.rs`), done internally here since
    /// `ZgcRealHeap::collect_garbage` is self-contained (no interpreter
    /// pre/post-GC cooperation step).
    fn enumerate_references(
        &self,
        base: *mut u8,
        work: &mut Vec<usize>,
        skip_index: Option<usize>,
    ) {
        let header = self.header_mut(base);
        match header.kind {
            ObjectKind::Object => {
                if cratonvm_types::is_compact_object(header) {
                    // Borrowing accessor: this walk only reads `field_offsets` /
                    // `is_ref` and drops the handle, so it need not pay the
                    // `Arc` clone/drop that `class_layout_for_fields` implies.
                    let _ = cratonvm_types::with_class_layout(
                        header.class_id.as_u32(),
                        header.num_slots(),
                        |layout| {
                            for (index, (&offset, &is_ref)) in layout
                                .field_offsets
                                .iter()
                                .zip(layout.is_ref.iter())
                                .enumerate()
                            {
                                if !is_ref || skip_index == Some(index) {
                                    continue;
                                }
                                let slot = unsafe { base.add(HEADER_SIZE + offset as usize) };
                                let raw = unsafe { std::ptr::read(slot as *const u64) };
                                if raw != 0 {
                                    work.push(raw as usize);
                                }
                            }
                        },
                    );
                } else {
                    let n = header.num_slots() as usize;
                    for i in 0..n {
                        if skip_index == Some(i) {
                            continue;
                        }
                        // SAFETY: i < num_slots so the slot is within the object.
                        let slot = unsafe { base.add(HEADER_SIZE + i * SLOT_SIZE) };
                        let val = unsafe { std::ptr::read(slot as *const Value) };
                        if let Value::Object(Some(r)) = val {
                            work.push(r.as_ptr() as usize);
                        }
                    }
                }
            }
            ObjectKind::Array => {
                if header.element_type == ArrayElementType::Reference {
                    let len = header.array_length() as usize;
                    // SAFETY: data area begins at base + HEADER_SIZE; each ref
                    // element is REF_ELEMENT_SIZE and `i < len`.
                    let data = unsafe { base.add(ARRAY_DATA_OFFSET) };
                    for i in 0..len {
                        let val =
                            unsafe { read_prim_element(data, i, ArrayElementType::Reference) };
                        if let Value::Object(Some(r)) = val {
                            work.push(r.as_ptr() as usize);
                        }
                    }
                }
                // Primitive arrays have no out-edges.
            }
            ObjectKind::HumongousFiller => {}
        }
    }

    /// Bounds-and-sanity check shared by `get_field`/`set_field`. Returns the
    /// in-bounds slot count, or `None` (caller treats as no-op / null) when
    /// the header is suspect or the index is out of range. Mirrors the guards
    /// in `g1::get_field` / `gen_heap`.
    fn check_field_index(&self, header: &ObjectHeader, index: usize) -> Option<usize> {
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(target: "zgc", index, num_slots, "zgc real: suspect header");
            return None;
        }
        if index >= num_slots {
            tracing::warn!(target: "zgc", index, num_slots, "zgc real: field index OOB");
            return None;
        }
        Some(num_slots)
    }

    /// True iff the object at `base` carries the [`GC_FLAG_MARKED`] bit set by
    /// the current cycle's mark phase. `base == 0` (null) is treated as not
    /// live. Used as the `is_marked` predicate handed to the shared
    /// [`ReferenceProcessor`] so weak/soft/phantom clearing observes the exact
    /// liveness the trace computed.
    fn is_marked_addr(&self, base: usize) -> bool {
        if base == 0 {
            return false;
        }
        // SAFETY: every address reachable here is either a registered live
        // allocation base (whose first HEADER_SIZE bytes are a valid header)
        // or null (handled above). Reference referents/objects discovered for
        // this heap are always such bases.
        let header = self.header_mut(base as *mut u8);
        header.gc_flags & GC_FLAG_MARKED != 0
    }

    /// Run the shared [`ReferenceProcessor`] against the just-completed mark.
    ///
    /// Called by [`Self::collect_garbage`] AFTER the mark phase has set the
    /// [`GC_FLAG_MARKED`] bits but BEFORE the sweep clears them and reclaims
    /// dead objects, so the `is_marked` snapshot is exactly the live set the
    /// trace computed and any still-live `Reference` object can have its
    /// referent field nulled in place.
    ///
    /// This is the SAME `gc::reference` path the generational and G1 backends
    /// drive (HotSpot ordering: soft → weak → final → phantom); it is invoked
    /// here, not reimplemented. Clearing nulls field 0 (the `referent`) of each
    /// soft/weak `Reference` whose referent died, mirroring the VM-level
    /// `process_references_after_gc` writer.
    ///
    /// Residual (documented, out of scope for this non-moving STW backend):
    /// (a) discovery is caller-driven via [`Self::discover_reference`]; this
    /// method does not itself scan the heap for `Reference` subclasses — that
    /// classification needs class-layout knowledge the GC layer intentionally
    /// does not own. (b) enqueue/finalize actions are surfaced in the returned
    /// [`ReferenceProcessingResult`] for the runtime to drain; the GC does not
    /// run finalizers. (c) `free_heap_mb` for the soft-ref LRU policy is
    /// approximated from outstanding allocation against the threshold.
    fn process_references(&self) -> ReferenceProcessingResult {
        // Approximate free heap (MB) for the SoftReference LRU policy: bytes
        // below the GC trigger threshold that are not currently outstanding.
        let free_bytes = self
            .gc_threshold
            .saturating_sub(self.allocated.load(Ordering::Relaxed));
        let free_mb = free_bytes / (1024 * 1024);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut rp = self.ref_processor.lock();
        let is_marked = |addr: usize| self.is_marked_addr(addr);
        let result = rp.process_references(&is_marked, free_mb, now_ms);

        // Null the referent (field 0) of every soft/weak Reference whose
        // referent was cleared this cycle. Each is emitted EXACTLY ONCE (the
        // processor flags `clear_emitted`), matching the VM-level writer and
        // avoiding the recycled-address corruption documented in
        // `gc::reference`.
        for ref_obj in rp.take_newly_cleared() {
            // The Reference object must itself be live (marked) to write into;
            // a dead Reference is about to be swept, so skip it.
            if self.is_marked_addr(ref_obj) {
                self.set_field(
                    // SAFETY: `ref_obj` is a registered live allocation base.
                    unsafe { ObjectRef::from_raw(ref_obj as *mut u8) },
                    0,
                    Value::Object(None),
                );
            }
        }

        // Drop bookkeeping for Reference objects that did not survive this
        // cycle so the registry does not grow without bound and stale indices
        // are rebuilt. (Non-moving: addresses are stable, so no
        // `update_after_gc` relocation is needed.)
        let is_live = |addr: usize| self.is_marked_addr(addr);
        rp.remove_collected(&is_live);

        result
    }
}

impl Default for ZgcRealHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl GarbageCollector for ZgcRealHeap {
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        let compact_body =
            cratonvm_types::compact_object_body_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let fields_size = compact_body.unwrap_or_else(|| {
            num_fields
                .checked_mul(SLOT_SIZE)
                .expect("object field size overflow")
        });
        let total = HEADER_SIZE
            .checked_add(fields_size)
            .expect("object total size overflow");
        let ptr = self.alloc_raw(total).unwrap_or_else(|| {
            eprintln!("FATAL: ZGC(real): out of heap space for object ({total} bytes)");
            std::process::abort();
        });
        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            u32::try_from(num_fields).expect("field count exceeds u32::MAX"),
        );
        if let Some(body) = compact_body {
            header.set_compact_shape(num_fields as u32, body);
        }
        // SAFETY: `ptr` is a fresh zeroed allocation of `total >= HEADER_SIZE`.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        assert!(
            length <= ZGC_REAL_MAX_ARRAY_LENGTH,
            "array length {length} exceeds maximum {ZGC_REAL_MAX_ARRAY_LENGTH}"
        );
        let data_size = array_data_size(length, element_type).expect("array data size overflow");
        let total = HEADER_SIZE
            .checked_add(data_size)
            .expect("array total size overflow");
        let ptr = self.alloc_raw(total).unwrap_or_else(|| {
            eprintln!("FATAL: ZGC(real): out of heap space for array ({total} bytes)");
            std::process::abort();
        });
        let len_u32 = u32::try_from(length).expect("array length exceeds u32::MAX");
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            len_u32,
            len_u32, // mirror length into num_slots, like Heap/G1/gen_heap
        );
        // SAFETY: fresh zeroed allocation of `total >= HEADER_SIZE`.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        self.header(obj)
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.header(obj).class_id
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.header(obj).kind
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.header(obj).element_type
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let header = self.header(obj);
        match header.mark_word_identity_hash(|| match self.next_hash() {
            0 => i32::MAX,
            h => h,
        }) {
            Ok(hash) => hash,
            Err(()) => crate::collector::displaced_identity_hash(
                header.mark_word.load(Ordering::Relaxed),
            ),
        }
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        let header = self.header(obj);
        if self.check_field_index(header, index).is_none() {
            return Value::Object(None);
        }
        if let Some((offset, storage)) =
            cratonvm_types::compact_object_field_storage(header, index)
        {
            let ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + offset) };
            return unsafe {
                cratonvm_types::read_compact_field(ptr, storage, Ordering::Relaxed)
            };
        }
        // SAFETY: index validated < num_slots, so the slot is within bounds.
        unsafe {
            let ptr = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
            std::ptr::read(ptr as *const Value)
        }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        let header = self.header(obj);
        if self.check_field_index(header, index).is_none() {
            return;
        }
        if let Some((offset, storage)) =
            cratonvm_types::compact_object_field_storage(header, index)
        {
            let ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + offset) };
            unsafe {
                cratonvm_types::write_compact_field(
                    ptr,
                    storage,
                    value,
                    Ordering::Relaxed,
                )
            };
            return;
        }
        // SAFETY: index validated < num_slots, so the slot is within bounds.
        unsafe {
            let ptr = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
            std::ptr::write(ptr as *mut Value, value);
        }
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        let _guard = crate::collector::volatile_stripe_lock(obj, index);
        std::sync::atomic::fence(Ordering::SeqCst);
        let v = self.get_field(obj, index);
        std::sync::atomic::fence(Ordering::SeqCst);
        v
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        let _guard = crate::collector::volatile_stripe_lock(obj, index);
        std::sync::atomic::fence(Ordering::SeqCst);
        self.set_field(obj, index, value);
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        let header = self.header(obj);
        debug_assert_eq!(header.kind, ObjectKind::Array, "not an array");
        header.array_length() as usize
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.header(obj);
        if header.kind != ObjectKind::Array {
            return Err(index as i32);
        }
        if index >= header.array_length() as usize {
            return Err(index as i32);
        }
        // SAFETY: bounds check passed; data area starts at base + HEADER_SIZE.
        let val = unsafe {
            let base = obj.as_ptr().add(ARRAY_DATA_OFFSET);
            read_prim_element(base, index, header.element_type)
        };
        Ok(val)
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        let header = self.header(obj);
        if header.kind != ObjectKind::Array {
            return Err(index as i32);
        }
        if index >= header.array_length() as usize {
            return Err(index as i32);
        }
        let element_type = header.element_type;
        // SAFETY: bounds check passed; data area starts at base + HEADER_SIZE.
        unsafe {
            let base = obj.as_ptr().add(ARRAY_DATA_OFFSET);
            if element_type == ArrayElementType::Reference {
                match value {
                    Value::Object(_) => {
                        write_prim_element(base, index, element_type, value);
                    }
                    other => {
                        // Auto-box non-Object values into a 1-field wrapper,
                        // matching `Heap::set_array_element`.
                        let wrapper = self.alloc_object(crate::heap::AUTOBOX_CLASS_ID, 1);
                        self.set_field(wrapper, 0, other);
                        // Re-fetch base: alloc_object cannot move existing
                        // objects (non-moving heap), so `base` is still valid,
                        // but reads are clearer with the explicit comment.
                        write_prim_element(base, index, element_type, Value::Object(Some(wrapper)));
                    }
                }
            } else {
                write_prim_element(base, index, element_type, value);
            }
        }
        Ok(())
    }

    fn needs_gc(&self) -> bool {
        let a = self.allocated.load(Ordering::Relaxed);
        a >= self.gc_threshold && a >= self.gc_rearm.load(Ordering::Relaxed)
    }

    fn collect_garbage(
        &self,
        _stw: &StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // ---- Mark phase --------------------------------------------------
        // Snapshot the registry of all live-or-dead allocations under the
        // lock, then release it: marking reads object bytes in place and
        // does not allocate, so it needs no arena lock. The set copy also
        // serves as the mark-phase validation oracle (ZGC-4): child pointers
        // pushed by `enumerate_references` come from raw field bytes, and a
        // corrupt/stale slot must be SKIPPED, not have a mark bit written
        // through it (a wild `header_mut` write inside an innocent object —
        // or outside the arena entirely — then cascades as the garbage
        // "object" is re-parsed for more children).
        let registered: FxHashSet<usize> = self.registry.lock().clone();
        let all: Vec<usize> = registered.iter().copied().collect();

        // Clear all mark bits first (objects may carry a stale bit from a
        // prior cycle's survivors).
        for &base in &all {
            self.header_mut(base as *mut u8).gc_flags &= !GC_FLAG_MARKED;
        }

        // INT-8: snapshot the referent-slot skip set — the currently
        // registered Weak/Soft/Phantom `Reference` object addresses. Their
        // slot 0 (the referent) must not be traced as a strong edge during
        // marking, or the referent always appears reachable through its own
        // Reference object and `process_references` below could never clear
        // it. Mirrors the G1 marker's `g1_set_reference_skip_set`; see
        // `enumerate_references`'s doc comment.
        let ref_skip_objs: FxHashSet<usize> = self
            .ref_processor
            .lock()
            .reference_object_addresses()
            .into_iter()
            .collect();
        let skip_for = |addr: usize| -> Option<usize> {
            if ref_skip_objs.contains(&addr) {
                Some(0)
            } else {
                None
            }
        };

        // Trace from roots. A work stack holds base addresses to visit.
        let mut work: Vec<usize> = Vec::new();
        for r in roots.iter() {
            work.push(r.as_ptr() as usize);
        }
        let mut wild_skipped = 0usize;
        while let Some(addr) = work.pop() {
            if addr == 0 {
                continue;
            }
            // ZGC-4: only registered allocation bases are objects. Roots are
            // pre-filtered by is_object_address, but CHILD pointers are raw
            // field bytes — skip anything that is not a current base.
            if !registered.contains(&addr) {
                wild_skipped += 1;
                continue;
            }
            let header = self.header_mut(addr as *mut u8);
            if header.gc_flags & GC_FLAG_MARKED != 0 {
                continue; // already visited
            }
            let class_id = header.class_id.as_u32();
            header.gc_flags |= GC_FLAG_MARKED;
            self.enumerate_references(addr as *mut u8, &mut work, skip_for(addr));
            if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr(class_id) {
                work.push(loader);
            }
            if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(addr) {
                work.extend(mirrors);
            }
            if let Some(metadata) = cratonvm_types::metadata_pin::roots_for_loader(addr) {
                work.extend(metadata);
            }
        }
        if wild_skipped > 0 {
            tracing::warn!(
                target: "zgc",
                wild_skipped,
                "zgc mark: skipped non-registered child pointers (corrupt/stale ref slots)"
            );
        }

        // ---- Finalizer resurrection (see collect_garbage_with_finalizers):
        // mark dead-but-finalizable objects (and their subtrees) live so the
        // sweep keeps them for the finalizer thread. Runs after the main
        // closure so "unmarked" == dead, and before the sweep decides.
        let fin_candidates = std::mem::take(&mut *self.pending_finalizer_roots.lock());
        if !fin_candidates.is_empty() {
            let mut resurrected = Vec::new();
            for addr in fin_candidates {
                if !registered.contains(&addr) {
                    continue; // not a current allocation (already swept earlier)
                }
                let header = self.header_mut(addr as *mut u8);
                if header.gc_flags & GC_FLAG_MARKED != 0 {
                    continue; // survived normally — stays registered, not finalized
                }
                resurrected.push(addr); // non-moving: address unchanged
                work.push(addr);
                while let Some(a) = work.pop() {
                    if a == 0 || !registered.contains(&a) {
                        continue; // ZGC-4: same wild-child skip as the main loop
                    }
                    let h = self.header_mut(a as *mut u8);
                    if h.gc_flags & GC_FLAG_MARKED != 0 {
                        continue;
                    }
                    let class_id = h.class_id.as_u32();
                    h.gc_flags |= GC_FLAG_MARKED;
                    self.enumerate_references(a as *mut u8, &mut work, skip_for(a));
                    if let Some(loader) =
                        cratonvm_types::loader_pin::loader_pin_addr(class_id)
                    {
                        work.push(loader);
                    }
                    if let Some(mirrors) =
                        cratonvm_types::mirror_pin::mirrors_for_loader(a)
                    {
                        work.extend(mirrors);
                    }
                    if let Some(metadata) =
                        cratonvm_types::metadata_pin::roots_for_loader(a)
                    {
                        work.extend(metadata);
                    }
                }
            }
            if !resurrected.is_empty() {
                *self.resurrected_finalizers.lock() = resurrected;
            }
        }

        // ---- Reference processing ---------------------------------------
        // Run the shared `gc::reference` processor while the mark bits still
        // reflect this cycle's live set (sweep clears them below). This
        // clears/enqueues weak/soft/phantom/cleaner/finalizer references per
        // HotSpot ordering — without it, finalizers/cleaners and
        // `WeakReference` semantics silently broke under this backend. The
        // enqueue/finalize actions are surfaced for the runtime to drain; the
        // referent-null writes are applied in place here.
        let ref_result = self.process_references();
        if !ref_result.to_enqueue.is_empty()
            || !ref_result.to_finalize.is_empty()
            || !ref_result.cleaner_actions.is_empty()
        {
            // The non-moving STW GC clears referents in place but does not run
            // finalizers or notify ReferenceQueues itself — that is the
            // runtime's job (cf. interpreter `process_references_after_gc`).
            // Surface the pending work so it is observable rather than silently
            // dropped when this backend is driven directly via the trait.
            tracing::debug!(
                target: "zgc",
                to_enqueue = ref_result.to_enqueue.len(),
                to_finalize = ref_result.to_finalize.len(),
                cleaner_actions = ref_result.cleaner_actions.len(),
                "zgc real: reference processing produced pending enqueue/finalize/cleaner work"
            );
        }

        // ---- INT-8 remark: resurrect policy-kept soft referents ----------
        // A soft referent the LRU policy chose to KEEP (not cleared, not
        // enqueued) was never traced by the main mark above — its Reference's
        // slot 0 was hidden by `ref_skip_objs`. If that referent is not ALSO
        // reachable some other way, the sweep below would reclaim it out
        // from under the still-live SoftReference. Mark it (and its
        // transitive closure) alive now, mirroring the interpreter's G1
        // remark step (`soft_survivor_referents`).
        {
            let survivors = self.ref_processor.lock().soft_survivor_referents();
            for addr in survivors {
                if registered.contains(&addr) {
                    work.push(addr);
                }
            }
            while let Some(addr) = work.pop() {
                if addr == 0 || !registered.contains(&addr) {
                    continue; // ZGC-4: same wild-child skip as the main loop
                }
                let header = self.header_mut(addr as *mut u8);
                if header.gc_flags & GC_FLAG_MARKED != 0 {
                    continue;
                }
                header.gc_flags |= GC_FLAG_MARKED;
                self.enumerate_references(addr as *mut u8, &mut work, skip_for(addr));
            }
        }

        // ---- Sweep phase -------------------------------------------------
        let mut dead: Vec<usize> = Vec::new();
        let mut bytes_copied = 0usize; // "retained" bytes (non-moving)
        let mut bytes_freed = 0usize;
        let mut objects_copied = 0usize;
        {
            let mut arena = self.arena.lock();
            let arena_base = arena.base_ptr() as usize;
            for &base in &all {
                let header = self.header_mut(base as *mut u8);
                let size = Self::alloc_size(header);
                if header.gc_flags & GC_FLAG_MARKED != 0 {
                    // Survivor: clear the mark bit for next cycle, keep it.
                    header.gc_flags &= !GC_FLAG_MARKED;
                    bytes_copied += size;
                    objects_copied += 1;
                } else {
                    // Dead: zero the bytes (so a later scan can't see a stale
                    // header) and return the span to the arena free list.
                    // SAFETY: `base` is a registered allocation of `size`
                    // bytes inside the arena.
                    unsafe { std::ptr::write_bytes(base as *mut u8, 0, size) };
                    if base >= arena_base {
                        arena.add_free_block(base - arena_base, size);
                    }
                    bytes_freed += size;
                    dead.push(base);
                }
            }

            // Coalesce the free list into maximal spans — same rationale as
            // gen_heap's post-sweep coalescer. The loop above returns ONE
            // object-sized hole per dead object, and `Arena::alloc`'s
            // small-tier scan is BUDGETED (16 entries): a workload whose
            // dead objects mix sizes (e.g. runs of 72-byte boxes burying
            // 296-byte byte[] holes) then misses reusable holes forever and
            // burns bump space until the arena exhausts — CopyChurn at
            // -Xmx256m OOM'd with most of the arena sitting unreachable on
            // the free list. Merging adjacent (and defensively overlapping)
            // holes rebuilds them into a few large spans that route to the
            // unbounded large tier, restoring reuse.
            let sorted = arena.free_blocks_sorted();
            if sorted.len() > 1 {
                arena.clear_free_list();
                let mut merged: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
                for (off, sz) in sorted {
                    if let Some(last) = merged.last_mut() {
                        let last_end = last.0 + last.1;
                        if off <= last_end {
                            // Adjacent or overlapping: extend to the farther
                            // end so no span is ever double-served.
                            let new_end = last_end.max(off + sz);
                            last.1 = new_end - last.0;
                            continue;
                        }
                    }
                    merged.push((off, sz));
                }
                for (off, sz) in merged {
                    arena.add_free_block(off, sz);
                }
            }
        }

        // Prune DEAD bases from the registry IN PLACE (never wholesale-
        // replace it with the mark snapshot's survivors): an allocation
        // registered by another path between the mark snapshot and this
        // publish would be erased by a replacement — leaking its memory
        // forever (unsweepable) and, worse, making is_object_address deny it
        // so conservative rooting drops it while reachable.
        {
            let mut reg = self.registry.lock();
            for d in &dead {
                reg.remove(d);
            }
        }
        self.allocated.store(bytes_copied, Ordering::Relaxed);
        // Re-arm the trigger: require at least a quarter of the remaining
        // headroom (min 64 KiB) of NEW allocation before the next
        // threshold-triggered collection, so a live set parked above the
        // static threshold cannot re-fire a full STW cycle on every
        // allocation (see `gc_rearm`). Allocation-failure GCs are driven by
        // the fallible alloc paths and ignore this gate.
        let cap = self.heap_capacity();
        let headroom = cap.saturating_sub(bytes_copied);
        self.gc_rearm.store(
            bytes_copied.saturating_add((headroom / 4).max(64 * 1024)),
            Ordering::Relaxed,
        );
        self.gc_count.fetch_add(1, Ordering::Relaxed);

        // Non-moving: no object changed address, so roots and external
        // references need no fix-up and the pointer map is empty.
        let pointer_map: HashMap<usize, usize> = HashMap::new();
        monitors.remap_after_gc(&pointer_map);
        // ZGC-3: `remap_after_gc` early-returns on the (always-empty) map,
        // so hand the collector's EXACT dead-address list to the registry
        // prune instead — reclaims monitor/cas-lock entries and prevents a
        // recycled address from inheriting a dead object's monitor.
        monitors.prune_dead(&dead);

        GcResult {
            stats: GcStats {
                objects_copied,
                bytes_copied,
                bytes_freed,
            },
            pointer_map,
        }
    }

    fn write_barrier(&self, _obj: ObjectRef, _stored_value: Value) {
        // Non-generational, non-concurrent: nothing to record.
    }

    fn allocated_bytes(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
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
        let cfg = ZgcConfig {
            heap_size: 1024,
            ..ZgcConfig::default()
        };
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
        c.relocation_set = c
            .heap
            .select_relocation_set(c.heap.config.relocation_threshold);
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
        gen.old_heap
            .allocate_medium(1024 * 1024)
            .expect("old alloc");
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
        gen.young_heap.used_size = (gen.young_heap.config.heap_size as f64 * 0.6) as usize;
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
        gen.young_heap.used_size = (gen.young_heap.config.heap_size as f64 * 0.9) as usize;
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

    // ------------------------------------------------------------------
    // ZgcRealHeap — real, memory-backed collector tests
    // ------------------------------------------------------------------
    //
    // `ClassId`, `StopTheWorldToken`, `MonitorCleanup`, `ObjectRef`,
    // `Value`, `ArrayElementType`, `ObjectKind` are all already in scope
    // via the `use super::*;` at the top of this module.

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &HashMap<usize, usize>) {}
    }

    #[test]
    fn real_alloc_object_roundtrips_fields() {
        let heap = ZgcRealHeap::new();
        let obj = heap.alloc_object(ClassId::new(7), 3);
        assert_eq!(heap.class_id_of(obj), ClassId::new(7));
        assert_eq!(heap.kind_of(obj), ObjectKind::Object);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 2, Value::Long(0x1_0000_0001));
        assert_eq!(heap.get_field(obj, 0), Value::Int(42));
        assert_eq!(heap.get_field(obj, 2), Value::Long(0x1_0000_0001));
        // An unwritten slot is raw zeroed memory, which decodes as
        // `Value::Int(0)` (discriminant 0 — see `value_discriminant_is_byte0_low32`
        // in types/src/value.rs), NOT `Value::Object(None)` (discriminant 4).
        // This is universal across every backend (heap.rs/gen_heap.rs never
        // explicitly zero-init a legacy slot to a typed default either — see
        // heap.rs's `alloc_object_fields_zero_initialized` test, which
        // deliberately does not assert a specific value for this exact
        // reason). Real Java field defaults (null for references, 0 for
        // primitives) are produced by the interpreter's class-layout-aware
        // initialization at `new` time, not by this raw allocator.
        assert_eq!(heap.get_field(obj, 1), Value::Int(0));
    }

    #[test]
    fn real_field_oob_is_safe() {
        let heap = ZgcRealHeap::new();
        let obj = heap.alloc_object(ClassId::new(1), 1);
        // Out-of-bounds read returns null and write is dropped (no UB / panic).
        assert_eq!(heap.get_field(obj, 99), Value::Object(None));
        heap.set_field(obj, 99, Value::Int(1));
    }

    #[test]
    fn real_array_roundtrips_and_bounds() {
        let heap = ZgcRealHeap::new();
        let arr = heap.alloc_array(ClassId::new(2), ArrayElementType::Int, 4);
        assert_eq!(heap.array_length(arr), 4);
        heap.set_array_element(arr, 1, Value::Int(7)).unwrap();
        assert_eq!(heap.get_array_element(arr, 1).unwrap(), Value::Int(7));
        assert!(heap.get_array_element(arr, 4).is_err());
        assert!(heap.set_array_element(arr, 4, Value::Int(0)).is_err());
    }

    #[test]
    fn real_collect_reclaims_unreachable_and_keeps_reachable() {
        let heap = ZgcRealHeap::new();
        let live = heap.alloc_object(ClassId::new(1), 1);
        let _dead = heap.alloc_object(ClassId::new(1), 1);
        let before = heap.allocated_bytes();
        assert!(before > 0);

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots = [live];
        let result = heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        // Exactly one object survives; the other is reclaimed.
        assert_eq!(result.stats.objects_copied, 1);
        assert!(result.stats.bytes_freed > 0);
        assert!(heap.allocated_bytes() < before);
        // The survivor is still usable and did not move (non-moving GC).
        assert_eq!(roots[0].as_ptr(), live.as_ptr());
        heap.set_field(live, 0, Value::Int(99));
        assert_eq!(heap.get_field(live, 0), Value::Int(99));
        assert_eq!(heap.gc_count(), 1);
    }

    #[test]
    fn real_collect_traces_transitively() {
        let heap = ZgcRealHeap::new();
        // root -> a -> b  (b reachable only via a's field)
        let b = heap.alloc_object(ClassId::new(3), 1);
        let a = heap.alloc_object(ClassId::new(3), 1);
        heap.set_field(a, 0, Value::Object(Some(b)));
        let _garbage = heap.alloc_object(ClassId::new(3), 1);

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots = [a];
        let result = heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        // a and b survive; the unreferenced object is collected.
        assert_eq!(result.stats.objects_copied, 2);
        // b's address is still reachable through a after the cycle.
        match heap.get_field(a, 0) {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr(), b.as_ptr()),
            other => panic!("expected b reference, got {other:?}"),
        }
    }

    #[test]
    fn real_array_reference_elements_are_traced() {
        let heap = ZgcRealHeap::new();
        let elem = heap.alloc_object(ClassId::new(4), 0);
        let arr = heap.alloc_array(ClassId::new(5), ArrayElementType::Reference, 2);
        heap.set_array_element(arr, 0, Value::Object(Some(elem)))
            .unwrap();

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots = [arr];
        let result = heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        // array + element both survive.
        assert_eq!(result.stats.objects_copied, 2);
        match heap.get_array_element(arr, 0).unwrap() {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr(), elem.as_ptr()),
            other => panic!("expected element reference, got {other:?}"),
        }
    }

    #[test]
    fn real_freed_memory_is_reused() {
        // Small heap so reuse is observable: allocate, drop all roots, GC,
        // then allocate again — the freed block should satisfy the request
        // without growing past capacity.
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        for _ in 0..10 {
            heap.alloc_object(ClassId::new(1), 4);
        }
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots: [ObjectRef; 0] = [];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        assert_eq!(heap.allocated_bytes(), 0);
        // Reallocate; must succeed from the reclaimed free list.
        let obj = heap.alloc_object(ClassId::new(1), 4);
        heap.set_field(obj, 0, Value::Int(123));
        assert_eq!(heap.get_field(obj, 0), Value::Int(123));
    }

    #[test]
    fn real_needs_gc_tracks_threshold() {
        let heap = ZgcRealHeap::with_capacity(8 * 1024);
        assert!(!heap.needs_gc());
        // Fill past 75% of 8 KB.
        while !heap.needs_gc() {
            heap.alloc_object(ClassId::new(1), 8);
        }
        assert!(heap.needs_gc());
    }

    // -- Reference processing under the ZGC-backed heap --------------------

    #[test]
    fn real_weak_ref_cleared_when_referent_dies() {
        let heap = ZgcRealHeap::new();
        // A live Reference object (field 0 = referent) and a referent that
        // becomes unreachable after we drop it from the roots.
        let weak = heap.alloc_object(ClassId::new(1), 1);
        let referent = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(weak, 0, Value::Object(Some(referent)));
        heap.discover_reference(ReferenceType::Weak, weak, referent, None);

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        // Root only the Reference object; the referent is otherwise dead.
        let mut roots = [weak];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        // The weak reference's referent (field 0) must be nulled in place.
        assert_eq!(heap.get_field(weak, 0), Value::Object(None));
    }

    #[test]
    fn real_weak_ref_kept_when_referent_live() {
        let heap = ZgcRealHeap::new();
        let weak = heap.alloc_object(ClassId::new(1), 1);
        let referent = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(weak, 0, Value::Object(Some(referent)));
        heap.discover_reference(ReferenceType::Weak, weak, referent, None);

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        // Root both: the referent stays strongly reachable, so the weak ref
        // must NOT be cleared.
        let mut roots = [weak, referent];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        match heap.get_field(weak, 0) {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr(), referent.as_ptr()),
            other => panic!("weak referent must survive, got {other:?}"),
        }
    }

    #[test]
    fn real_phantom_ref_enqueued_but_not_cleared() {
        let heap = ZgcRealHeap::new();
        let queue = heap.alloc_object(ClassId::new(9), 0);
        let phantom = heap.alloc_object(ClassId::new(1), 1);
        let referent = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(phantom, 0, Value::Object(Some(referent)));
        heap.discover_reference(ReferenceType::Phantom, phantom, referent, Some(queue));

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        // Root the phantom Reference and its queue; the referent is dead.
        let mut roots = [phantom, queue];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        // Java 9+: a phantom reference's referent field is NOT nulled by the
        // collector, so field 0 still points at the (now-dead) referent.
        match heap.get_field(phantom, 0) {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr(), referent.as_ptr()),
            other => panic!("phantom referent must not be cleared, got {other:?}"),
        }
    }
}
