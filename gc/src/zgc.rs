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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

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
// Production-ZGC submodules
// ---------------------------------------------------------------------------
//
// The types above (`ColoredPointer`, `LoadBarrier`, `ZPage`, `ZgcCollector`,
// `GenerationalZgc`) are the metadata-only SIMULATION described in the module
// docs. The submodules below are the real, memory-backed replacements being
// built out under `docs/feature-designs/zgc-production-implementation-plan.md`.
//
// They are deliberately decoupled from one another: each depends on a trait it
// declares itself rather than on a sibling's concrete types, so they can land
// and be reviewed independently.
//
// ADOPTION STATUS — recount before quoting. This said "NONE of them is wired
// into `ZgcRealHeap` yet" from 2026-08-07 until 2026-08-10, and by the end of
// that window HALF of them were. `gc/Cargo.toml` quoted it, and
// `docs/known-issues/tomcat/gc-backend-3way-fullsuite-comparison-20260810.md`
// quoted that in turn to argue ZGC's suite result might be an artefact of an
// unwired allocator — which is exactly the inference a stale count corrupts.
// As of 2026-08-10, uses in THIS file outside `mod tests`:
//
//     census 24 · mark 15 · tlab 12 · vaddr 7 · page 2 · metrics 1   ADOPTED
//     barrier · forwarding · remembered · generation · relocate · adapters  NOT
//
// The TLAB in particular is default-ON and serves `alloc_object`/`alloc_array`
// through `alloc_raw_tlab`, so "not wired into the real allocator" is the one
// phrasing to avoid. `vaddr` additionally reaches `gc/src/heap.rs` and
// `vm/src/jit/helpers.rs`, as the tripwire that catches a colored word arriving
// where a plain pointer belongs.
//
// What HAS not changed is the collector: `ZgcRealHeap` is still the
// stop-the-world non-moving mark-sweep it has always been, and the six
// unadopted modules are precisely the moving/generational/concurrent
// machinery — `vm_init.rs` still hard-codes `RELOCATION_REQUESTED = false`.
// Say that, rather than that nothing is wired in.
//
// Recount with, from the repo root:
//
//     awk '/^mod tests \{/{exit} {print}' gc/src/zgc.rs \
//       | grep -c '\bcensus::'      # etc, per module

/// Real colored-pointer encoding and virtual address space.
pub mod vaddr;

/// Real memory-backed ZPages (small/medium/large tiers) and the O(1) page table.
pub mod page;

/// Real atomic, self-healing colored-pointer load barrier.
pub mod barrier;

/// Lock-free per-page forwarding table and relocation-set selection.
pub mod forwarding;

/// ZGC cycle phase/pause instrumentation and allocation-stall accounting.
pub mod metrics;

/// Generational remembered set (old -> young) and the store barrier.
pub mod remembered;

/// Concurrent marking engine: striped work-stealing stacks, the termination
/// handshake, and the mutator ingress the load barrier publishes into.
pub mod mark;

/// Generational young/old split (JEP 439) over the real page allocator.
pub mod generation;

/// Object relocation: the copy/CAS-publish protocol, the from-space page
/// lifetime state machine, and the stop-the-world compaction path.
pub mod relocate;

/// Thread-local allocation buffers for the ZGC page allocator.
pub mod tlab;

/// Reference-slot census: measures the compact/legacy/array/static split that
/// decides how much of the load barrier's slot-resolution work is critical path.
pub mod census;

/// The seam layer between the submodules above. These modules were authored
/// independently and each declares its own decoupling traits, so the glue that
/// joins them lives here rather than in any one of them. An adapter in this
/// module is a *symptom*: the long-term fix for several of them is for the two
/// modules to agree on a shared type.
pub mod adapters;

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

/// Collect once the arena can no longer serve a request this large, however
/// few LIVE bytes there are — see [`ZgcRealHeap::headroom_low`].
///
/// The margin has to cover the largest request a workload makes between two
/// native-call boundaries, because a native cannot collect where it stands: the
/// boundary hook in `vm/src/vm/vm_exec.rs` is the first place a collection can
/// run on its behalf. `capacity/128` with an 8 MiB floor is 16 MiB at
/// `-Xmx 2g` — under 1% of the heap, against a failure mode that costs the
/// whole run.
fn zgc_headroom_margin(capacity: usize) -> usize {
    (capacity / 128).max(8 * 1024 * 1024)
}

/// Maximum array length, mirroring `heap.rs` / HotSpot's practical limit.
const ZGC_REAL_MAX_ARRAY_LENGTH: usize = i32::MAX as usize;

/// Registered objects the sweep refused to size (and therefore refused to
/// reclaim) since process start. See [`ZgcRealHeap::alloc_size`]. Non-zero
/// means a class was unloaded while an instance was still registered, or a
/// header is corrupt; both leak the object rather than corrupt the arena.
pub static ZGC_UNSIZABLE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

/// One-shot latch for the [`ZGC_UNSIZABLE_OBJECTS`] warning — one line per
/// swept object would itself be the hang.
static ZGC_UNSIZABLE_WARNED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Object-start membership for `ZgcRealHeap` — the allocation-path bitmap
// ---------------------------------------------------------------------------
//
// # Why (a measurement with an in-tree precedent, not a hypothesis)
//
// [`ZgcRealHeap::registry`] holds the base address of every live allocation and
// is written on EVERY allocation, from every mutator thread. It used to be a
// `Mutex<FxHashSet<usize>>`. `bench/BinTreesClassic.java`, release build,
// ABBA-interleaved, checksums identical on both sides (so this backend is
// CORRECT, just slow), 2026-08-07:
//
// ```text
//   depth 12   generational   35 ms    zgc    231 ms     6.6x
//   depth 14   generational  138 ms    zgc   1566 ms    11.3x
//   depth 16   generational  621 ms    zgc  11417 ms    18.4x
//   depth 18   generational  ~7.9 s    zgc   ~116 s     14.6x
// ```
//
// The ratio is SUPERLINEAR: each depth step is ~4x the objects, and the
// generational collector scales ~4x while this one scales ~7x. And
// `--verbose:gc` shows the depth-16 run performed exactly ONE collection
// (1.1 ms, `bytes_freed=0`), finishing at 719 MB against a 4.2 GB heap. So the
// cost is not the collector, and it is not allocation locking — TLABs
// ([`ZgcRealHeap::tlabs`]) landed first and did not move the number. It is
// per-allocation work on the mutator path that grows with the LIVE-OBJECT
// COUNT. Tens of millions of entries in one global hash set, behind one global
// mutex, probed with cache-missing scatter reads over a multi-gigabyte table,
// is exactly that shape and nothing else on the path is.
//
// # The precedent
//
// `gen_heap` had this defect and it was diagnosed and fixed on 2026-07-26
// (landed `7307935bc`). There a `young_object_starts: FxHashSet<usize>` made a
// moving collection cost O(objects *allocated*) instead of O(objects
// *surviving*): `perf` over bt18 measured `HashMap::insert` at 40.7% of the
// whole process plus `reserve_rehash` at 8.3% — 49% together. The fix was
// [`crate::young_mark::ObjectStartBits`], one bit per 8 bytes of
// `[base, base + used)`, and bt18 went 5.0x -> 2.1x. That finding's own
// generalisation is why this section exists: *any membership-over-a-contiguous-
// arena set in this GC is a bitmap candidate.* This registry is precisely that,
// and worse than the one that was fixed — `gen_heap`'s set was built once per
// collection; this one is written on every allocation from every thread under a
// single lock.
//
// # Why the two existing bitmaps could not be reused as-is
//
// * [`crate::young_mark::ObjectStartBits`] has exactly the right *semantics*
//   (it is alignment-exact: an unaligned address is rejected rather than
//   aliased onto a neighbour's bit) but its accessors take `&mut self` over a
//   plain `Vec<u64>`, because its own doc says it is "single-threaded by
//   construction — the walk and the forwarding that reads it both run inside
//   the collection's stop-the-world region". This registry is written from
//   concurrent mutators, so it cannot serve. It also has no `remove`, which the
//   sweep's in-place prune needs.
// * [`crate::young_mark::YoungMarkBits`] IS atomic and IS the allocation model
//   copied below (`alloc_zeroed` + `*mut AtomicU64` + `fetch_or`), but its
//   `locate` deliberately does NOT check alignment — `addr` and `addr + 4` map
//   to the same bit — because its callers pre-screen. Here that would be
//   unsound: [`ZgcRealHeap::is_object_address`] would answer `Some` for an
//   INTERIOR conservative-root candidate and hand back an `ObjectRef` pointing
//   four bytes into an object, which is the `is_addr_live` unsoundness
//   [`ZgcRealHeap::conservative_addr_span`] documents. It also has no `remove`.
//
// Neither file is edited. [`ZObjectStartBits`] below is the atomic twin:
// `YoungMarkBits`'s storage and RMW discipline, `ObjectStartBits`'s exactness
// rule, plus the `remove` the sweep needs.
//
// # Why the bitmap is EXACT here (three legs, each verified against the code)
//
// 1. **One contiguous region with a stable base.** [`ZgcRealHeap::with_capacity`]
//    creates a single [`Arena`] and captures `arena_base`/`arena_end` from it;
//    there is no `Arena::grow` call anywhere in this file and `alloc_raw` is the
//    single arena chokepoint, so the envelope never moves. Those two fields
//    already back [`ZgcRealHeap::conservative_addr_span`] for exactly this
//    reason.
// 2. **Every footprint is a multiple of 8.** `Arena::alloc` opens with
//    `let size = size.checked_add(7)? & !7` (`arena.rs:579`) and warns on an
//    unrounded request, so consecutive starts sit on the 8-byte grid.
// 3. **Every start is 8-aligned relative to `arena_base`.** All three of this
//    heap's paths into the arena ask for `align == 8`: `alloc_raw` calls
//    `arena.alloc(size, 8)`, `tlab_refill` calls `arena.alloc(want,
//    ZGC_TLAB_ALIGN)` with [`ZGC_TLAB_ALIGN`]` == 8`, and the bump path aligns
//    the *offset* (`arena.rs:634`) so `addr - arena_base` is a multiple of 8 by
//    construction.
//
// Therefore `(addr - arena_base) / 8` is a total, collision-free encoding of
// "is this an object start", and a non-multiple-of-8 offset is *provably* not
// one — which is what lets [`ZObjectStartBits::locate`] reject it instead of
// aliasing, preserving the exact-base answer the `FxHashSet` gave.
//
// **TLAB-served allocations preserve all three.** A TLAB is not a second
// allocator: `tlab_refill` carves its chunk out of this same arena with
// `arena.alloc(want, ZGC_TLAB_ALIGN)`, so the chunk base is on the grid; every
// object inside is bumped at [`ZGC_TLAB_ALIGN`] (the constant's own doc pins it
// at 8 precisely so "the cursor can never leave the object grid"); and
// [`zgc_tlab_footprint`] rounds each request to that alignment. So a TLAB start
// is `chunk_base + 8k` and `chunk_base` is `arena_base + 8j`.
//
// # The one leg that is NOT a proof, and the fallback that covers it
//
// `Arena`'s backing store is a `Vec<u8>`, whose pointer Rust only guarantees to
// be 1-aligned; and `Arena::alloc`'s FREE-LIST tiers align the *absolute*
// address (`arena.rs:481`, `:548`) while the bump tier aligns the *offset*
// (`arena.rs:634`). The two agree iff `arena_base` is itself 8-aligned, which
// every real allocator delivers for a multi-megabyte block but no type in this
// tree asserts. Rather than bet on it, [`ZObjectStartBits`] keeps an
// [`overflow`](ZObjectStartBits::overflow) set for any address the grid cannot
// encode. It is empty on every real run (and a one-shot `tracing::warn!` says
// so if it is not), it costs one relaxed load of a never-written cache line on
// the query path, and it means a base can never be LOST — losing one would make
// `is_object_address` deny a reachable object and drop it from conservative
// rooting, which is a crash, not a slowdown.

/// Runtime kill switch: `CRATONVM_ZGC_STARTBITS`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) puts the registry back on
/// the `Mutex<FxHashSet<usize>>` it used before this change, byte for byte, so
/// the A/B is a re-run and not a rebuild.
///
/// Same idiom, and the same two reasons, as [`zgc_tlab_enabled_by_default`]:
/// read through [`cratonvm_types::flags::runtime_var_os`] so it layers with
/// `-XX:` like every other flag, and deliberately NOT declared as a
/// [`cratonvm_types::GcFlags`] field, because a declared flag latches on first
/// read and a mid-run `set_var` then becomes invisible to the very suite that
/// wants to A/B it. Read once per heap, in [`ZgcRealHeap::with_capacity`].
fn zgc_start_bits_enabled_by_default() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_STARTBITS") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// One bit per 8 bytes of the arena: "is this address an object start?"
///
/// The atomic, removable twin of [`crate::young_mark::ObjectStartBits`] — see
/// the section header above for why neither existing bitmap could be reused and
/// for the three-leg exactness argument.
///
/// # Memory
///
/// One bit per 8 bytes is **1/64th of the arena**, committed up front: ~1 MB for
/// the 64 MB default heap, ~66 MB for a 4.2 GB one. That is deliberate rather
/// than lazily committed. `alloc_zeroed` for a block this size goes straight to
/// the OS (`mmap`/`VirtualAlloc`) and the pages are demand-faulted anyway, so
/// the resident cost already tracks the part of the arena that has been
/// allocated into; a hand-rolled two-level commit would add a dependent load to
/// [`ZgcRealHeap::is_object_address`], which is on the mutator path via
/// `jit_checkcast`. It also compares favourably with what it replaces: the hash
/// set it removes cost ~8 bytes per LIVE OBJECT plus load factor — at bt16's
/// 719 MB of ~72-byte nodes that is well over 100 MB, and it grew without bound
/// because the collection that would prune it essentially never fires.
pub(crate) struct ZObjectStartBits {
    /// `alloc_zeroed` block of [`Self::nwords`] `AtomicU64`s, freed in `Drop`.
    /// Raw rather than `Box<[AtomicU64]>` for the same reason
    /// [`crate::young_mark::YoungMarkBits`] is: `AtomicU64` is not `Clone`, so
    /// `vec![]` cannot build one, and collecting an iterator would MEMSET tens
    /// of megabytes instead of taking a zero-page mapping.
    words: *mut AtomicU64,
    nwords: usize,
    /// The arena base. Bit `i` denotes `base + i * 8`.
    base: usize,
    /// Arena capacity in bytes; `[base, base + span)` is the covered range.
    span: usize,
    /// Bases the 8-byte grid cannot encode — see the section header's "one leg
    /// that is NOT a proof". Expected to stay empty forever.
    overflow: Mutex<FxHashSet<usize>>,
    /// `overflow.len()`, readable without taking the lock. The query path tests
    /// this before it will even consider locking, so the fallback costs a
    /// relaxed load of a line that is never written on a healthy run.
    overflow_len: AtomicUsize,
    /// One-shot latch for the "the grid could not encode a base" warning: one
    /// line per allocation would itself be the hang.
    overflow_warned: AtomicBool,
}

// SAFETY: identical argument to `crate::young_mark::YoungMarkBits`. Every
// access to `words` is an atomic operation on `AtomicU64`; the allocation is
// owned exclusively by this value and freed exactly once in `Drop`. The
// `overflow` set is behind a `Mutex`.
unsafe impl Send for ZObjectStartBits {}
// SAFETY: as above — all shared access is atomic or mutex-guarded.
unsafe impl Sync for ZObjectStartBits {}

impl ZObjectStartBits {
    /// Cover `[base, base + span)`.
    fn new(base: usize, span: usize) -> Self {
        let nwords = span.div_ceil(8).div_ceil(64);
        let words = if nwords == 0 {
            std::ptr::NonNull::<AtomicU64>::dangling().as_ptr()
        } else {
            let layout = std::alloc::Layout::array::<AtomicU64>(nwords)
                .expect("zgc object-start bitmap layout overflow");
            // SAFETY: `nwords > 0` so the layout is non-zero-sized, and an
            // all-zero bit pattern is a valid `AtomicU64` (value 0).
            let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut AtomicU64;
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            p
        };
        Self {
            words,
            nwords,
            base,
            span: if nwords == 0 { 0 } else { span },
            overflow: Mutex::new(FxHashSet::default()),
            overflow_len: AtomicUsize::new(0),
            overflow_warned: AtomicBool::new(false),
        }
    }

    /// Word index and bit mask for `addr`, or `None` when the grid cannot
    /// encode it.
    ///
    /// The `off & 7 != 0` rejection is the whole exactness argument in one
    /// line, and it is what [`crate::young_mark::YoungMarkBits`] omits: without
    /// it an interior address would alias its object's bit and
    /// [`ZgcRealHeap::is_object_address`] would promote a conservative-root
    /// candidate that is not a base.
    #[inline]
    fn locate(&self, addr: usize) -> Option<(usize, u64)> {
        let off = addr.checked_sub(self.base)?;
        if off >= self.span || off & 7 != 0 {
            return None;
        }
        let bit = off >> 3;
        Some((bit >> 6, 1u64 << (bit & 63)))
    }

    /// Record an object start.
    ///
    /// # Why `fetch_or` and not a CAS loop
    ///
    /// The word is shared with the 63 neighbouring 8-byte grid slots, so two
    /// threads allocating adjacent objects write the same word — a
    /// `load`/`or`/`store` would drop one of them. `fetch_or` is a single
    /// atomic read-modify-write, and because this bitmap is *monotone between
    /// collections* (inserts only ever set bits; the only clears happen in the
    /// stop-the-world sweep, with no mutator inserting) there is no value to
    /// re-check and therefore nothing for a retry loop to do. That is exactly
    /// [`crate::young_mark::YoungMarkBits::try_mark`]'s argument.
    ///
    /// `Release`: this is at least as strong as the `Mutex::unlock` it
    /// replaces. A thread that hands a fresh pointer to another thread does so
    /// through a plain field store, which supplies no edge of its own; keeping
    /// the release here means the reader's `Acquire` in [`Self::contains`]
    /// still cannot observe "not an object" for a pointer whose publication it
    /// has already seen. Relaxed would be sufficient under the memory model
    /// only if the publication path itself carried the edge, and it does not.
    #[inline]
    fn insert(&self, addr: usize) {
        match self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            Some((w, mask)) => unsafe {
                (*self.words.add(w)).fetch_or(mask, Ordering::Release);
            },
            None => self.spill(addr),
        }
    }

    /// The grid could not encode `addr`; keep it exactly, in the side set.
    #[cold]
    fn spill(&self, addr: usize) {
        {
            let mut overflow = self.overflow.lock();
            if overflow.insert(addr) {
                let n = overflow.len();
                self.overflow_len.store(n, Ordering::Release);
            }
        }
        if !self.overflow_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "zgc",
                addr = addr,
                base = self.base,
                span = self.span,
                "zgc object-start bitmap: an allocation base is off the 8-byte \
                 grid (or outside the arena) — falling back to the exact side \
                 set for it; membership stays correct, but every such base \
                 costs a lock and the bitmap is not carrying it"
            );
        }
    }

    /// Exact membership: is `addr` the base of a registered allocation?
    ///
    /// `Acquire` pairs with [`Self::insert`]'s `Release`; see there. The
    /// overflow probe is guarded by a relaxed-cost load that is zero on every
    /// healthy run, so the common answer is one aligned load and one mask.
    #[inline]
    fn contains(&self, addr: usize) -> bool {
        if let Some((w, mask)) = self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            if unsafe { (*self.words.add(w)).load(Ordering::Acquire) } & mask != 0 {
                return true;
            }
        }
        self.overflow_len.load(Ordering::Acquire) != 0 && self.overflow.lock().contains(&addr)
    }

    /// Clear one start. Called only from the sweep's in-place prune, inside the
    /// stop-the-world region.
    ///
    /// `fetch_and` rather than `load`/`store` for [`Self::insert`]'s reason —
    /// the word is shared with 63 neighbours, and although the sweep is the
    /// only writer *of this word's bits* while the world is stopped, using the
    /// RMW costs nothing and removes the assumption. `Release` keeps the clear
    /// no weaker than the `Mutex::unlock` it replaces, so the preceding
    /// zero-fill of the dead object cannot be observed after it.
    fn remove(&self, addr: usize) {
        if let Some((w, mask)) = self.locate(addr) {
            // SAFETY: `locate` bounds-checked `addr`, so `w < self.nwords`.
            let prev = unsafe { (*self.words.add(w)).fetch_and(!mask, Ordering::Release) };
            if prev & mask != 0 {
                return;
            }
        }
        if self.overflow_len.load(Ordering::Acquire) != 0 {
            let mut overflow = self.overflow.lock();
            if overflow.remove(&addr) {
                let n = overflow.len();
                self.overflow_len.store(n, Ordering::Release);
            }
        }
    }

    /// Is anything held outside the grid? See [`Self::overflow`].
    #[inline]
    fn has_spill(&self) -> bool {
        self.overflow_len.load(Ordering::Acquire) != 0
    }

    /// The greatest recorded start at or below `addr`, or `None`.
    ///
    /// This is the whole of interior-pointer resolution. Allocations do not
    /// overlap, so the ONLY base whose extent can contain `addr` is the
    /// greatest one `<= addr`: find it, deref one header, compare one extent.
    /// A forward walk reaches the same candidate — after visiting every base
    /// below it and dereferencing every one of their headers.
    ///
    /// WHY THAT MATTERED. `ZgcRealHeap::is_heap_addr` is not a GC-only path:
    /// `VmHeap::is_heap_addr`'s ZGC arm feeds it per-slot conservative root
    /// scanning over ambiguous JVM-long-vs-jobject operand words, so every
    /// interior pointer and every long bit pattern that happens to land inside
    /// the arena envelope bought a full walk of the object-start bitmap up to
    /// the arena's high-water mark. On a heap whose cursor has reached capacity
    /// that is the entire bitmap — ~3M word loads plus a header dereference per
    /// set bit, per probe. Measured with `perf record` on
    /// `type.temporal.InstantTests` (2026-08-11, Azure Linux, default
    /// collector): **80.7% of all CPU samples in `VmHeap::is_heap_addr`**, with
    /// another 11.5% in `object_body_size` — the header deref inside that walk.
    /// The class takes 6.7 s on real HotSpot and had not finished in 1800 s.
    ///
    /// Backwards, the scan stops at the first set bit below `addr`, which in a
    /// populated heap is a handful of words away. It is never worse in shape
    /// than the forward walk it replaces (both are bounded by the bitmap), and
    /// it dereferences exactly one header instead of one per live object.
    ///
    /// The `overflow` set is deliberately NOT consulted here — it carries no
    /// ordering, so "greatest at or below" is not a question it can answer. The
    /// caller keeps the full walk for that arm; see [`Self::has_spill`].
    fn nearest_base_at_or_below(&self, addr: usize) -> Option<usize> {
        if self.span == 0 || self.nwords == 0 {
            return None;
        }
        let off = addr.checked_sub(self.base)?;
        // Past the covered span: the last gridded slot is still the greatest
        // candidate at or below `addr`.
        let off = off.min(self.span - 1);
        let bit = off >> 3; // floor onto the 8-byte grid
        let mut w = bit >> 6;
        debug_assert!(w < self.nwords, "bit index is derived from a bounded offset");
        // Keep only bits at or below `bit` in the first word. `u64::MAX >> k`
        // has its low `64 - k` bits set, so `k = 63 - (bit & 63)` leaves
        // exactly bits `0..=(bit & 63)`.
        let k = 63 - (bit & 63);
        // SAFETY: `w < self.nwords`, checked above.
        let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) } & (u64::MAX >> k);
        loop {
            if word != 0 {
                let b = 63 - word.leading_zeros() as usize;
                return Some(self.base + (((w << 6) | b) << 3));
            }
            if w == 0 {
                return None;
            }
            w -= 1;
            // SAFETY: `w` only decreases from a value `< self.nwords`.
            word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
        }
    }

    /// Visit every set start, ASCENDING. `f` returns `false` to stop early.
    ///
    /// `end_hint` is a PERFORMANCE BOUND, not a filter: every base strictly
    /// below it is guaranteed to be visited, bases at or above it MAY be. Pass
    /// `usize::MAX` for a full walk. It exists because the walk cost of a
    /// bitmap and of a hash set have opposite shapes — the set was O(live), the
    /// bitmap is O(arena span), so on a mostly-empty multi-gigabyte heap an
    /// unbounded walk would be a *regression* (~8.2M word loads for a 4.2 GB
    /// arena against a few thousand hash steps). The one caller that walks from
    /// the mutator path, [`ZgcRealHeap::is_heap_addr`], has the arena's
    /// high-water mark available and passes it, which restores the O(bytes
    /// actually allocated) shape.
    ///
    /// `Acquire` on each word load, matching [`Self::contains`]. These walks
    /// are off the allocation path, so the per-word ordering is not worth
    /// trading for a fence.
    fn for_each_base(&self, end_hint: usize, f: &mut dyn FnMut(usize) -> bool) {
        // Bytes of the covered span that can hold a base below `end_hint`,
        // rounded UP to a whole word: over-approximating is what makes the
        // parameter a hint rather than a filter.
        let reach = end_hint.saturating_sub(self.base).min(self.span);
        let limit = reach.div_ceil(8).div_ceil(64).min(self.nwords);
        for w in 0..limit {
            // SAFETY: `w < limit <= self.nwords`.
            let mut word = unsafe { (*self.words.add(w)).load(Ordering::Acquire) };
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                if !f(self.base + ((w * 64 + b) << 3)) {
                    return;
                }
            }
        }
        // The spill is always walked in full: its members are precisely the
        // ones whose address the grid could not reason about, so no bound
        // derived from the grid may exclude them.
        if self.has_spill() {
            for &addr in self.overflow.lock().iter() {
                if !f(addr) {
                    return;
                }
            }
        }
    }
}

impl Drop for ZObjectStartBits {
    fn drop(&mut self) {
        if self.nwords == 0 {
            return;
        }
        let layout = std::alloc::Layout::array::<AtomicU64>(self.nwords)
            .expect("zgc object-start bitmap layout overflow");
        // SAFETY: `words` came from `alloc_zeroed` with this exact layout and
        // is freed exactly once.
        unsafe { std::alloc::dealloc(self.words as *mut u8, layout) };
    }
}

/// The membership structure behind [`ZgcRealHeap::registry`], plus its kill
/// switch.
///
/// Both arms answer the same questions with the same semantics; `Hash` is the
/// pre-2026-08-07 structure kept verbatim so `CRATONVM_ZGC_STARTBITS=0` is a
/// true A/B and not an approximation of one.
enum ZObjectStartsKind {
    Bits(ZObjectStartBits),
    Hash(Mutex<FxHashSet<usize>>),
}

/// Base address of every live allocation. See the section header for the
/// measurement, the precedent and the exactness argument.
pub(crate) struct ZObjectStarts {
    kind: ZObjectStartsKind,
}

impl ZObjectStarts {
    /// Cover the arena `[base, base + span)`, honouring
    /// [`zgc_start_bits_enabled_by_default`].
    ///
    /// A zero span cannot be gridded at all, so it falls back unconditionally —
    /// which also keeps `ZgcRealHeap`'s constructor total if the envelope is
    /// ever degenerate.
    fn new(base: usize, span: usize) -> Self {
        Self::with_bitmap(base, span, zgc_start_bits_enabled_by_default())
    }

    /// [`Self::new`] with the kill switch supplied rather than read.
    ///
    /// The seam the arm-equivalence tests drive: they must exercise BOTH arms
    /// in one process, and doing that through `CRATONVM_ZGC_STARTBITS` would
    /// mean a `set_var` race against every other test in the binary.
    fn with_bitmap(base: usize, span: usize, bitmap: bool) -> Self {
        let kind = if bitmap && span > 0 {
            ZObjectStartsKind::Bits(ZObjectStartBits::new(base, span))
        } else {
            ZObjectStartsKind::Hash(Mutex::new(FxHashSet::default()))
        };
        Self { kind }
    }

    /// Is the bitmap arm in force? Diagnostics for the kill-switch tests.
    #[cfg(test)]
    fn is_bitmap(&self) -> bool {
        matches!(self.kind, ZObjectStartsKind::Bits(_))
    }

    /// Record one allocation base. The whole point of this file's change: on
    /// the bitmap arm this is one `fetch_or` and no lock at all.
    #[inline]
    fn insert(&self, addr: usize) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.insert(addr),
            ZObjectStartsKind::Hash(set) => {
                set.lock().insert(addr);
            }
        }
    }

    /// Record a batch. Shaped for [`ZTlabHeapHooks::register_allocations`], and
    /// the reason the `Hash` arm keeps its single `reserve` + single lock: the
    /// A/B has to compare against what was actually there.
    #[inline]
    fn insert_all(&self, addrs: &[usize]) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                for &addr in addrs {
                    bits.insert(addr);
                }
            }
            ZObjectStartsKind::Hash(set) => {
                let mut set = set.lock();
                set.reserve(addrs.len());
                for &addr in addrs {
                    set.insert(addr);
                }
            }
        }
    }

    /// Exact-base membership.
    #[inline]
    fn contains(&self, addr: usize) -> bool {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.contains(addr),
            ZObjectStartsKind::Hash(set) => set.lock().contains(&addr),
        }
    }

    /// Drop one base (the sweep's in-place prune).
    fn remove(&self, addr: usize) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.remove(addr),
            ZObjectStartsKind::Hash(set) => {
                set.lock().remove(&addr);
            }
        }
    }

    /// Does this structure hold anything the arena envelope does not bound?
    ///
    /// `true` on the `Hash` arm unconditionally — a hash set carries no
    /// geometry, so nothing may be inferred from an address range about what it
    /// contains. On the bitmap arm it is `true` only if
    /// [`ZObjectStartBits::overflow`] took something, which means: *every base
    /// this structure holds is inside `[base, base + span)`*, because an address
    /// outside it could not have been encoded and would have spilled. That
    /// makes the envelope screen in [`ZgcRealHeap::is_heap_addr`] a proof rather
    /// than an assumption.
    #[inline]
    fn has_spill(&self) -> bool {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.has_spill(),
            ZObjectStartsKind::Hash(_) => true,
        }
    }

    /// The greatest base at or below `addr` — see
    /// [`ZObjectStartBits::nearest_base_at_or_below`] for why interior-pointer
    /// resolution is one backwards bit scan and not a walk.
    ///
    /// `None` on the `Hash` arm, and `None` whenever the bitmap has a spill:
    /// neither can order what it holds, so neither can answer "greatest at or
    /// below". A `None` means "ask the walk", never "no such base".
    #[inline]
    fn nearest_base_at_or_below(&self, addr: usize) -> Option<usize> {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) if !bits.has_spill() => {
                bits.nearest_base_at_or_below(addr)
            }
            _ => None,
        }
    }

    /// Visit every base; `f` returns `false` to stop early.
    ///
    /// `end_hint` is a performance bound and never a filter — see
    /// [`ZObjectStartBits::for_each_base`]. The `Hash` arm ignores it, because
    /// its walk cost is already O(live) and it has no ordering to exploit.
    ///
    /// On the bitmap arm this holds NO lock (the words are read atomically),
    /// which is strictly better than the `Hash` arm, where the guard is held
    /// across the callback exactly as the old code held it.
    fn for_each_base(&self, end_hint: usize, f: &mut dyn FnMut(usize) -> bool) {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => bits.for_each_base(end_hint, f),
            ZObjectStartsKind::Hash(set) => {
                for &addr in set.lock().iter() {
                    if !f(addr) {
                        return;
                    }
                }
            }
        }
    }

    /// Every base, as a materialised list.
    fn bases(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        self.for_each_base(usize::MAX, &mut |addr| {
            out.push(addr);
            true
        });
        out
    }

    /// A frozen copy, for the collection cycle.
    ///
    /// This is the direct replacement for `self.registry.lock().clone()`. On
    /// the bitmap arm it is a `Vec<u64>` of one bit per 8 arena bytes — 1/64th
    /// of the heap — which is *cheaper* than the `FxHashSet` clone it replaces
    /// (8 bytes per live object plus load factor) for any occupancy above ~1.5%,
    /// and it keeps the O(1) membership the mark phase's wild-child screen
    /// needs.
    fn snapshot(&self) -> ZObjectStartsSnapshot {
        match &self.kind {
            ZObjectStartsKind::Bits(bits) => {
                let mut words: Vec<u64> = Vec::with_capacity(bits.nwords);
                for w in 0..bits.nwords {
                    // SAFETY: `w < bits.nwords`.
                    words.push(unsafe { (*bits.words.add(w)).load(Ordering::Acquire) });
                }
                let extra = if bits.overflow_len.load(Ordering::Acquire) != 0 {
                    bits.overflow.lock().clone()
                } else {
                    FxHashSet::default()
                };
                ZObjectStartsSnapshot {
                    words,
                    base: bits.base,
                    extra,
                }
            }
            ZObjectStartsKind::Hash(set) => ZObjectStartsSnapshot {
                words: Vec::new(),
                base: 0,
                extra: set.lock().clone(),
            },
        }
    }
}

/// A point-in-time copy of [`ZObjectStarts`], with the same two operations the
/// mark phase used to get from its `FxHashSet` clone: O(1) membership and a
/// full enumeration.
pub(crate) struct ZObjectStartsSnapshot {
    /// Copied bitmap words; empty on the `Hash` arm.
    words: Vec<u64>,
    /// The arena base the bits are relative to; meaningless when `words` is
    /// empty.
    base: usize,
    /// The `Hash` arm's whole set, or the bitmap arm's (normally empty)
    /// overflow spill.
    extra: FxHashSet<usize>,
}

impl ZObjectStartsSnapshot {
    /// Exact-base membership, identical in answer to `FxHashSet::contains`.
    ///
    /// No `span` field is needed: the trailing bits of the last word cover
    /// addresses past `base + span`, and `ZObjectStartBits::locate` refuses to
    /// set those, so they are always clear.
    #[inline]
    fn contains(&self, addr: usize) -> bool {
        if !self.words.is_empty() {
            if let Some(off) = addr.checked_sub(self.base) {
                if off & 7 == 0 {
                    let bit = off >> 3;
                    if let Some(word) = self.words.get(bit >> 6) {
                        if word & (1u64 << (bit & 63)) != 0 {
                            return true;
                        }
                    }
                }
            }
        }
        !self.extra.is_empty() && self.extra.contains(&addr)
    }

    /// Every base in the snapshot, bitmap portion ASCENDING.
    ///
    /// Ascending order is free here (it was not, from a hash set) and it is the
    /// order the sweep wants: adjacent dead objects hand adjacent spans to
    /// `Arena::add_free_block`, which is what the post-sweep coalescer merges.
    fn bases(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(self.extra.len());
        for (w, &word) in self.words.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                out.push(self.base + ((w * 64 + b) << 3));
            }
        }
        out.extend(self.extra.iter().copied());
        out
    }
}

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
    /// Immutable arena envelope, captured at construction — see
    /// [`Self::conservative_addr_span`]. Plain `usize`, deliberately outside
    /// the `Mutex`: the conservative-root filter must answer without locking.
    arena_base: usize,
    /// Exclusive upper bound of the arena envelope.
    arena_end: usize,
    /// Base address of every live allocation. Pruned (dead bases removed in
    /// place) by each sweep.
    ///
    /// Membership must be O(1): `is_object_address` is consulted per
    /// conservative-root candidate (every operand-stack root and JIT-frame
    /// qword), and the original `Vec` linear scan made every GC's root
    /// collection O(roots × live) and read as a hang at scale (ZGC-5/6
    /// hardening). The sweep prunes DEAD bases IN PLACE and never
    /// wholesale-replaces the structure, so an allocation registered between
    /// the mark snapshot and the sweep publish cannot be silently dropped.
    ///
    /// It was an `FxHashSet<usize>` behind a `Mutex` until 2026-08-07, when
    /// `bench/BinTreesClassic.java` measured this backend at 6.6x/11.3x/18.4x
    /// the generational collector at bt12/14/16 — superlinear, with exactly ONE
    /// collection in the whole bt16 run, so the cost was neither the collector
    /// nor the arena lock but a per-allocation global-mutex hash insert whose
    /// table grows with the live set. It is now an object-start BITMAP; see the
    /// "Object-start membership" section header above this struct for the
    /// measurement, the 2026-07-26 `gen_heap` precedent it copies, the exactness
    /// argument, and the `CRATONVM_ZGC_STARTBITS` kill switch.
    registry: ZObjectStarts,
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
    /// "`needs_gc` went true while allocating" latch, read at the native-call
    /// boundary via `VmHeap::young_spill_pressure`.
    ///
    /// Why it exists: this collector's `GarbageCollector::alloc_object` /
    /// `alloc_array` are INFALLIBLE — they end in
    /// `eprintln!("FATAL: ZGC(real): out of heap space …"); std::process::abort()`.
    /// A workload that allocates only from inside native wrappers reaches no
    /// safepoint of its own, so without a signal the one hook that can collect
    /// on a native's behalf (`vm/src/vm/vm_exec.rs`) never fires here and the
    /// process dies by `abort()` on a heap full of garbage, with no Java-visible
    /// `OutOfMemoryError` ever thrown. G1 closed exactly this defect with
    /// `G1Collector::native_alloc_pressure`, armed by
    /// `note_region_consumed_locked` (`g1.rs:1765-1774`); this field is the ZGC
    /// analogue, armed from [`Self::alloc_raw`].
    ///
    /// It CANNOT recreate the `gc_rearm` GC-storm livelock documented on the
    /// field above, because the arming predicate is `needs_gc`'s predicate
    /// verbatim — `allocated >= gc_threshold && allocated >= gc_rearm` — so it
    /// inherits the re-arm floor. A live set parked above the static threshold
    /// leaves `gc_rearm` above `allocated` after every sweep, so the latch
    /// simply stays down until genuinely new allocation clears the floor. It
    /// adds a signal below no gate that `needs_gc` does not already have.
    ///
    /// `Relaxed` throughout, matching G1: this is an advisory diagnostic
    /// feeding a policy decision ("should this native boundary collect?"), not
    /// a correctness handshake. Nothing is published *through* the bit — the
    /// consumer re-reads the heap's own counters and re-checks its own gates
    /// before acting — so no acquire/release pairing is needed, and a read that
    /// observes the store one boundary late merely defers a collection to the
    /// next boundary.
    native_alloc_pressure: AtomicBool,
    /// A request that the arena **actually refused**, as distinct from the
    /// advisory pressure above.
    ///
    /// # Why the two cannot be one bit
    ///
    /// `native_alloc_pressure` is consumed through
    /// `VmHeap::young_spill_pressure`, whose boundary consumer re-checks
    /// `needs_gc()` before it collects — deliberately, so that an advisory note
    /// buys one gate evaluation and cannot storm. That is right for a *soft*
    /// signal and wrong for a hard one, and [`Self::alloc_raw`] latches the
    /// same bit for both. Its own comment says why the re-check is wrong there:
    /// a request that just failed "is stronger evidence that a cycle is due
    /// than the `allocated >= gc_threshold` predicate, which counts LIVE bytes
    /// and therefore cannot see the bump space this heap never rewinds."
    ///
    /// So the arming site and the consuming site disagreed, and the consuming
    /// site won: on the exact shape this collector fails in — an arena full of
    /// TLAB *reservations* with `allocated` far below the threshold, i.e.
    /// Tomcat's `TestNonBlockingAPI` on 2026-08-13 — `needs_gc()` answered
    /// **no**, the latch was cleared without collecting, and the one signal
    /// that knew better was discarded. This bit is that signal, kept separate
    /// so the boundary can honour it without loosening the soft path.
    ///
    /// Cannot storm: it is set only where an allocation genuinely failed, the
    /// consumer clears it after acting, and `gc_overhead_limit_exceeded` still
    /// gates it — the same bound the soft path relies on.
    hard_alloc_failure: AtomicBool,
    /// Fragmentation ratchet — Phase 2.2. See [`ZFragGauge`].
    ///
    /// Held as three plain atomics rather than a `Mutex<ZFragGauge>` because
    /// they are written once per collection, under the arena lock, and read at
    /// shutdown; there is no invariant across them that a torn read could
    /// break, only three numbers describing one sample.
    frag_samples: AtomicUsize,
    /// Worst `largest_free_block * 1000 / capacity` seen, or `usize::MAX` for
    /// "never sampled" — deliberately a sentinel rather than 1000, so that a
    /// run which never met the sampling condition cannot be mistaken for one
    /// that scored perfectly.
    frag_worst_permille: AtomicUsize,
    /// Free share of capacity at the worst sample, permille.
    frag_worst_free_permille: AtomicUsize,
    /// Collection number of the worst sample.
    frag_worst_cycle: AtomicUsize,
    /// One-shot latch for the floor warning.
    frag_floor_warned: AtomicBool,
    /// Cycles in which the concurrent-mark driver refused to certify a
    /// complete mark set and the single-threaded marker ran instead. Non-zero
    /// is not a crash — it is the fail-closed path doing its job — but it is
    /// the number that says the driver is not trustworthy on this workload.
    parallel_mark_fallbacks: AtomicUsize,
    /// `allocated` at the last stress-triggered collection — see `needs_gc`.
    gc_stress_mark: AtomicUsize,
    /// Addresses that must NOT be relocated, with a use count.
    ///
    /// Filled by `VmHeap::pin_critical_region` at
    /// `GetPrimitiveArrayCritical` and drained by `unpin_critical_regions` at
    /// `Release`. Refcounted because the same array can be inside two nested
    /// critical sections, and the inner `Release` must not unpin the outer.
    ///
    /// # Why this exists (2026-08-14)
    ///
    /// `pin_critical_region`'s ZGC arm returned `Vec::new()`, i.e. pinned
    /// nothing, and that was correct for as long as this collector never moved
    /// an object. It moves now. The hazard is not the native pointer — this VM
    /// hands native code a *copy* — it is the **copy-back at Release**, which
    /// re-resolves the Get-time array address. If the slide moved that array,
    /// the copy-back writes a whole array's worth of bytes over whatever
    /// object now occupies the old address. The JNI site's own comment names
    /// the outcome: "data loss / write to a recycled object".
    ///
    /// A `Mutex<FxHashMap>` rather than something clever: it is touched twice
    /// per critical section and once per collection, and it is empty in every
    /// workload that makes no critical calls at all.
    critical_pins: Mutex<FxHashMap<usize, usize>>,
    /// Concurrent phases the DRIVER reported, summed over all cycles.
    ///
    /// This is the counter that distinguishes "the worker pool marked" from
    /// "`zgc_concurrent`'s controller drove the cycle" — the pool-only path
    /// this replaced produced identical mark bits, identical stats and an
    /// identical `parallel_mark_cycles`, so nothing else here can tell the two
    /// apart. `passes` exists only inside `ZgcMarkCycleOutcome`.
    driver_passes: AtomicUsize,
    /// Whether a concurrent mark cycle is in progress — Phase 3.
    ///
    /// This is the **only** thing on the mutator store path while no cycle is
    /// running: [`Self::satb_pre_barrier`] loads it and returns. Everything
    /// else behind the barrier is reachable only when it is `true`.
    mark_active: AtomicBool,
    /// Mutator ingress for the concurrent marker — Phase 3.
    ///
    /// Overwritten references arrive here from the VM's existing pre-write
    /// barrier and are drained by the marker. Allocated once with the heap and
    /// left empty while [`Self::mark_active`] is false, so a non-concurrent
    /// run pays for the buckets and nothing else.
    mark_ingress: mark::ZMarkIngress,
    /// Phase 4: the barrier's good mask, and the phase machine behind it.
    ///
    /// `Z_REMAPPED` — "no mark parity is good; addresses are plain" — until a
    /// cycle arms it. The barrier gates on this and needs no separate
    /// activation flag; see `zgc::barrier::ZBarrierContext::good_mask`.
    barrier_good_mask: AtomicU64,
    /// Phase 4: whether a relocating cycle is in progress. Distinct from
    /// [`Self::mark_active`] because the barrier's mark and relocate slow
    /// paths are separately armed.
    relocate_active: AtomicBool,
    /// Whether reference slots currently hold COLOURED words, and so whether
    /// the read-path load barrier may run at all.
    ///
    /// Deliberately not inferred from the good mask. `Z_REMAPPED` is both the
    /// quiescent "addresses are plain" state AND a real ZGC colour, so
    /// `good_mask() != Z_REMAPPED` answers "is a mark parity good", which is a
    /// different question and is false during the remap phase — exactly when
    /// the barrier is most needed.
    barrier_armed: AtomicBool,
    /// Phase 4: `from_offset -> to_offset` for objects this cycle has moved.
    ///
    /// A plain map rather than `zgc::forwarding::ZForwardingTable` on purpose:
    /// that table is per-page and this collector has one arena and no pages,
    /// so its page-id keying would carry no information here. The table
    /// becomes the right structure when `zgc::page` is adopted, which is the
    /// step after this one.
    forwarding: Mutex<FxHashMap<u64, u64>>,
    /// Barrier counters — slow-path entries, heals, forward lookups.
    barrier_stats: barrier::ZBarrierStats,
    /// Per-logical-page age, indexed by page id. A page's age is the number of
    /// cycles it has survived; `generation::ZPromotionPolicy` turns that into
    /// young-or-old. Grown on demand, never shrunk -- a page id is an index
    /// into the arena's logical grid and the arena does not shrink either.
    page_ages: Mutex<Vec<u32>>,
    /// Old-to-young edges, per old page -- `zgc::remembered`.
    ///
    /// Fed by [`Self::satb_pre_barrier`], which every reference store in the
    /// VM already reaches. Consumed as extra roots by a young-scoped cycle,
    /// which is the whole reason a generational collector can look at less
    /// than the whole heap.
    remembered: remembered::ZRememberedSetTable,
    /// Page ids the last cycle classified as old. Read by the store barrier to
    /// decide whether a store is an old-to-young edge worth remembering.
    old_page_ids: Mutex<Vec<u64>>,
    /// Cycles in which the parallel marker ran, and in which compaction moved
    /// at least one object, plus the objects it moved.
    ///
    /// Reported on the `[GC] zgc-real:` shutdown line. These exist because the
    /// first smoke test of the 2026-08-13 default flip could not tell whether
    /// either feature had actually engaged: the per-cycle `tracing::debug!`
    /// needs a subscriber the suite harness does not install, so a run that
    /// silently took the serial path looked exactly like a run that did not.
    /// **An opt-in feature turned on by default needs a counter, or its first
    /// measurement is a vacuous green.**
    parallel_mark_cycles: AtomicUsize,
    compaction_cycles: AtomicUsize,
    objects_relocated: AtomicUsize,
    /// Addresses this barrier has published since the cycle began. Telemetry
    /// for the adoption work — it is how you tell "the barrier is wired" from
    /// "the barrier is wired and the workload actually overwrites references",
    /// which are the two states an inert-looking instrument confuses.
    mark_ingress_pushes: AtomicUsize,
    /// "The arena can no longer serve a request of [`headroom_margin`] bytes."
    ///
    /// # Why the live-bytes trigger is not enough on THIS backend
    ///
    /// [`needs_gc`](GarbageCollector::needs_gc) asks `allocated >= gc_threshold`,
    /// and the sweep stores *retained* bytes back into `allocated` — so it is a
    /// LIVE-BYTES question. On a compacting heap that is the right question,
    /// because live bytes and allocatable space move together.
    ///
    /// This heap does not compact. The arena's bump cursor never rewinds, and
    /// reclaimed space comes back only as free-list holes. Allocatable space is
    /// therefore `max(capacity - cursor, largest_free_block)`, and it falls as
    /// the *garbage* grows — a quantity `allocated` cannot see, because the
    /// sweep subtracts exactly that garbage from it.
    ///
    /// The two diverge by however much garbage there is, and the divergence is
    /// not academic: on `ZipContentTests` at `-Xmx 2g`, ten collections ran and
    /// an 8 KB array allocation still failed with live at **1,434,932,032 of
    /// 2,147,483,648 bytes** — 66.8%, well under the 75% threshold. `needs_gc`
    /// answered "no collection needed" while the allocation that raised
    /// `OutOfMemoryError` was failing, because it was answering about live
    /// bytes and the wall the workload hit was allocatable space.
    ///
    /// Armed from [`Self::alloc_raw`] under the arena lock, consulted by
    /// `needs_gc`, cleared by the sweep. It is gated by the same `gc_rearm`
    /// floor as the threshold term, so it cannot re-create the GC storm that
    /// field exists to prevent: right after a sweep `gc_rearm` exceeds
    /// `allocated`, so a still-low headroom simply waits for genuinely new
    /// allocation instead of firing a cycle per allocation.
    headroom_low: AtomicBool,
    /// Lifetime collection counter (observability).
    gc_count: AtomicUsize,
    /// `--verbose:gc` per-collection logging gate — see
    /// [`Self::enable_gc_logging`]. Off by default; flipped by
    /// `VmHeap::enable_gc_logging`. Mirrors G1's `gc_log_enabled`
    /// (`g1.rs:6770-6777`). `Relaxed`: a logging toggle orders nothing.
    gc_log_enabled: AtomicBool,
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

    /// Reference-slot census — the instrument for the one unmeasured number in
    /// `docs/feature-designs/zgc-reference-slot-representation.md`.
    ///
    /// Why it lives on the heap rather than in a `static`: it is the same
    /// no-process-global rule [`census`]'s module header states, and the same
    /// rule this tree learned the hard way from process-global GC caches
    /// outliving the VM that filled them. One census per heap, reached only
    /// through `&self`.
    ///
    /// **Off by default**, so an untouched heap pays exactly one relaxed
    /// `AtomicBool` load per collection (the gate on the [`Self::collect_garbage`]
    /// call site) and nothing at all on any mutator path. Turn it on through
    /// [`Self::slot_census`].
    slot_census: census::ZSlotCensus,

    /// The weak-referent skip set for a **concurrent** mark cycle: the
    /// heap-owned twin of the `ref_skip_objs` local that
    /// [`Self::collect_garbage`] builds on its own stack.
    ///
    /// # Why this is a field and not a parameter
    ///
    /// [`mark::ZMarkContext::visit_refs`] takes `&self` and an address and
    /// nothing else, and it is called from every mark worker and from
    /// arbitrary mutator threads. The skip set therefore cannot travel on the
    /// stack the way `collect_garbage`'s does; it has to live somewhere every
    /// caller can reach. It is also why the set is an [`std::sync::Arc`]: a
    /// worker clones the handle under a read lock, drops the guard, and walks
    /// the object with no lock held, which is the same shape the census's
    /// two-phase contract uses and the same reason — a registry-style guard
    /// held across a call back into the heap is a lock cycle.
    ///
    /// # Why it must be a SNAPSHOT
    ///
    /// [`ReferenceProcessor::reference_object_addresses`] is a live view: a
    /// mutator that constructs a `WeakReference` mid-cycle adds to it. If
    /// workers read that live view, an object could be traced as a strong
    /// edge before the `Reference` was discovered and skipped after, so
    /// whether the referent is immortal would depend on thread timing. Taken
    /// once at [`Self::begin_concurrent_mark_cycle`] and held for the whole
    /// cycle, the answer is the same for every worker.
    ///
    /// `None` means "no concurrent mark cycle is open" — see
    /// [`Self::concurrent_mark_skip_set`] for what `visit_refs` then does and
    /// why that direction was chosen.
    mark_ref_skip: parking_lot::RwLock<Option<std::sync::Arc<FxHashSet<usize>>>>,

    /// One-shot latch for the "`visit_refs` ran with no skip-set snapshot"
    /// warning. Without it the warning is one line per object visited, which
    /// on a real heap is millions of lines and is itself a hang. `Relaxed`: it
    /// orders nothing and publishes nothing, it only de-duplicates a log line.
    mark_ref_skip_warned: AtomicBool,

    /// Per-thread allocation buffers carved out of [`Self::arena`] — the
    /// adoption of `gc::zgc::tlab`'s hook/statistics contract onto this heap.
    ///
    /// # Why (the measurement, not a guess)
    ///
    /// Every allocation on this backend used to serialise on
    /// [`Self::arena`]'s single `Mutex<Arena>`, and that mutex is not held for
    /// a pointer bump: it covers `Arena::alloc`'s free-list tier scans (the
    /// large tier is a linear scan of every reclaimed span) *and* the
    /// per-object `write_bytes` zeroing. The generational and G1 backends have
    /// TLABs; this one had none. The full Spring Boot suite under
    /// `-XX:+UseZGC` measured 1860 PASS / 49 HANG / 22 FAIL against
    /// 1902 / 18 / 11 on the default collector — **35 classes PASS -> HANG**,
    /// concentrated on `*AutoConfigurationTests`, i.e. `ApplicationContext`
    /// boot/teardown, the most allocation-heavy workload in the suite
    /// (`fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260807.md`).
    ///
    /// That record does not attribute the hangs to this lock and neither does
    /// this field: a global lock on every allocation is the wrong answer at
    /// any heap size, and it is the cheaper of the two suspects to remove.
    ///
    /// See [`ZArenaTlabRegistry`] for why the buffers are carved from the
    /// arena rather than from `zgc::tlab`'s [`crate::zgc::page::ZPageAllocator`].
    tlabs: ZArenaTlabRegistry,

    /// Kill switch for [`Self::tlabs`], seeded from `CRATONVM_ZGC_TLAB` at
    /// construction and flippable at runtime through
    /// [`Self::set_tlab_enabled`]. **Default on.**
    ///
    /// Seeded per heap rather than latched in a `OnceLock` (or declared in
    /// `cratonvm_types::GcFlags`) deliberately: a latched process-global gate
    /// makes `set_var` invisible to a test that runs after the first read, and
    /// the point of a kill switch is that a suite run can A/B it. `Relaxed`:
    /// nothing is published through the bit — a mutator that observes the flip
    /// one allocation late merely takes the other path for one object.
    ///
    /// Turning it OFF does **not** disable [`Self::retire_all_tlabs`]: chunks
    /// already handed out must still be closed and returned, or their tails
    /// leak.
    tlab_enabled: AtomicBool,
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

    /// The arena envelope, for conservative root scanning.
    ///
    /// `VmHeap::conservative_addr_span`'s ZGC arm answered `None`, justified by
    /// "ZGC keeps live bases in a registry, not a contiguous arena". That
    /// premise is false — this heap is one `Arena`, created in
    /// [`Self::with_capacity`] and never grown — and the consequence was real:
    /// the sole consumer (`vm/src/jit/conservative_roots.rs`) hoists this span
    /// out of its scan loop and rejects a word with an inline
    /// `w < lo || w >= hi`. With `None` it cannot, so EVERY 8-byte stack word
    /// went through `is_object_address`, which at the time opened with
    /// `registry.lock()` — one global mutex acquire per candidate qword, per
    /// frame, per thread. (That lock is gone as of the object-start bitmap; the
    /// span filter still earns its keep, because rejecting a word with a range
    /// compare beats even an atomic load and a mask.)
    ///
    /// The same two numbers grid the bitmap — see [`ZObjectStarts`] and the
    /// "Object-start membership" section header. That is not a coincidence but
    /// the same fact used twice: this arena is created once and never grown.
    ///
    /// **This is a filter, not an answer.** A word inside the span must still
    /// go through the exact-base registry test. Widening it into an acceptance
    /// path would readmit interior pointers as object bases — the `is_addr_live`
    /// unsoundness that was fixed separately, where the sweep's coalescing of
    /// free spans lets a dead object's base become interior to a live one.
    ///
    /// Answers without taking the arena lock; see the fields' note.
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        if self.arena_end > self.arena_base {
            Some((self.arena_base, self.arena_end))
        } else {
            None
        }
    }

    /// Bytes committed for the Java heap — this collector's arena envelope.
    /// See [`crate::vm_heap::VmHeap::committed_bytes`] for what the quantity is
    /// for and why it must not track live bytes.
    ///
    /// Fixed for the collector's lifetime: the arena is allocated once in
    /// [`Self::with_capacity`] and never grown, which is the same property that
    /// makes `arena_base`/`arena_end` safe to read without the lock.
    pub fn committed_bytes(&self) -> usize {
        self.arena_end.saturating_sub(self.arena_base)
    }

    /// Create a heap with the default capacity ([`ZGC_REAL_DEFAULT_HEAP`]).
    pub fn new() -> Self {
        Self::with_capacity(ZGC_REAL_DEFAULT_HEAP)
    }

    /// Create a heap with the given total capacity in bytes.
    pub fn with_capacity(total_bytes: usize) -> Self {
        let cap = total_bytes.max(4096);
        let mut arena = Arena::new(cap);
        // Put a floor under the large-object end. Without it the split between
        // the two ends exists but carries no weight: TLAB chunks are carved at
        // 512 KiB apiece and a thread-heavy workload creates thousands of them,
        // so the low end reaches the high end's cursor while the large-object
        // region still holds almost nothing (measured: 3,829 chunk-sized spans,
        // 1.96 GB of a 2.15 GB arena, at the failing allocation). See
        // `Arena::high_reserve`, and note that the reserve is a preference the
        // low end may still overrun rather than fail.
        //
        // `capacity / 8` — 256 MiB at `-Xmx 2g`, ~120 live 2 MB buffers, which
        // is an order of magnitude more than any workload measured here holds
        // at once. The floor of 8 MiB keeps small heaps (and the gc unit tests,
        // which build arenas of a few KiB) from reserving a share that
        // `set_high_reserve`'s quarter-of-capacity clamp would then have to
        // take back.
        arena.set_high_reserve((cap / 8).max(8 * 1024 * 1024));
        // Capture the arena envelope ONCE, as plain `usize`. This backs
        // `conservative_addr_span`, and it is deliberately not read through
        // `self.arena.lock()`: the span's whole purpose is to let a caller
        // reject a stack word with a range compare and no lock, so taking a
        // lock to answer it would reintroduce exactly the contention it
        // exists to delete. Sound because this arena is created here and
        // NEVER grown — there is no `Arena::grow` call anywhere in this file,
        // and `alloc_raw` is the single allocation chokepoint.
        let arena_base = arena.base_ptr() as usize;
        let arena_end = arena_base.saturating_add(arena.capacity());
        Self {
            layout_domain: std::sync::atomic::AtomicU32::new(
                cratonvm_types::FIRST_LAYOUT_DOMAIN,
            ),
            arena_base,
            arena_end,
            arena: Mutex::new(arena),
            // The bitmap is gridded over the arena envelope captured above —
            // the SAME two numbers `conservative_addr_span` answers with, and
            // sound for the same reason (this arena is created here and never
            // grown). `arena_end` is derived from the arena's post-rounding
            // capacity, so the span covers every address `Arena::alloc` can
            // ever return.
            registry: ZObjectStarts::new(arena_base, arena_end.saturating_sub(arena_base)),
            next_hash_code: AtomicI32::new(1),
            allocated: AtomicUsize::new(0),
            gc_threshold: cap * ZGC_REAL_GC_THRESHOLD_PERCENT / 100,
            gc_rearm: AtomicUsize::new(0),
            native_alloc_pressure: AtomicBool::new(false),
            hard_alloc_failure: AtomicBool::new(false),
            frag_samples: AtomicUsize::new(0),
            frag_worst_permille: AtomicUsize::new(usize::MAX),
            frag_worst_free_permille: AtomicUsize::new(0),
            frag_worst_cycle: AtomicUsize::new(0),
            frag_floor_warned: AtomicBool::new(false),
            parallel_mark_fallbacks: AtomicUsize::new(0),
            gc_stress_mark: AtomicUsize::new(0),
            critical_pins: Mutex::new(FxHashMap::default()),
            driver_passes: AtomicUsize::new(0),
            mark_active: AtomicBool::new(false),
            mark_ingress: mark::ZMarkIngress::new(),
            mark_ingress_pushes: AtomicUsize::new(0),
            barrier_good_mask: AtomicU64::new(vaddr::Z_REMAPPED),
            relocate_active: AtomicBool::new(false),
            barrier_armed: AtomicBool::new(false),
            forwarding: Mutex::new(FxHashMap::default()),
            barrier_stats: barrier::ZBarrierStats::default(),
            page_ages: Mutex::new(Vec::new()),
            remembered: remembered::ZRememberedSetTable::new(),
            old_page_ids: Mutex::new(Vec::new()),
            parallel_mark_cycles: AtomicUsize::new(0),
            compaction_cycles: AtomicUsize::new(0),
            objects_relocated: AtomicUsize::new(0),
            headroom_low: AtomicBool::new(false),
            gc_count: AtomicUsize::new(0),
            gc_log_enabled: AtomicBool::new(false),
            ref_processor: Mutex::new(ReferenceProcessor::new()),
            pending_finalizer_roots: Mutex::new(Vec::new()),
            resurrected_finalizers: Mutex::new(Vec::new()),
            slot_census: census::ZSlotCensus::new(),
            mark_ref_skip: parking_lot::RwLock::new(None),
            mark_ref_skip_warned: AtomicBool::new(false),
            // NOTE (E0063 class of break): this is the ONE struct literal for
            // `ZgcRealHeap` — `new()` and `Default` both delegate here — so a
            // new field must be initialised here and nowhere else. Verified by
            // grepping `ZgcRealHeap {` across the workspace.
            tlabs: ZArenaTlabRegistry::for_capacity(cap),
            tlab_enabled: AtomicBool::new(zgc_tlab_enabled_by_default()),
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

    /// Native-allocation pressure signal — see the
    /// [`native_alloc_pressure`](Self::native_alloc_pressure) field doc.
    /// Consumed at the `safe_native_call` boundary via
    /// `VmHeap::young_spill_pressure`. Mirrors `G1Collector::native_alloc_pressure`
    /// (`g1.rs:1779-1781`), including its `Relaxed` ordering: advisory
    /// diagnostic feeding a policy decision, not a correctness handshake.
    #[inline]
    pub fn native_alloc_pressure(&self) -> bool {
        self.native_alloc_pressure.load(Ordering::Relaxed)
    }

    /// Clear the native-allocation pressure latch. The consumer clears
    /// unconditionally after acting (including when its own gates said no), so
    /// this must be idempotent and cheap. `Relaxed` for the reason on the
    /// field: nothing is published through the bit. Mirrors `g1.rs:1785-1787`.
    #[inline]
    pub fn clear_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
    }

    /// Latch the native-allocation pressure signal from OUTSIDE the collector —
    /// the `VmHeap::note_young_spill_pressure` funnel used by allocation
    /// wrappers that spilled and cannot collect themselves. Mirrors
    /// `g1.rs:1793-1795`.
    ///
    /// Note this externally-noted edge deliberately bypasses the `gc_threshold`
    /// / `gc_rearm` predicate that [`Self::alloc_raw`] applies: the caller is
    /// asserting pressure the heap's own counters cannot see. It still cannot
    /// storm, because the *consumer* re-checks `needs_gc` and its own overhead
    /// gate before running a cycle, and each call latches once for one clear.
    #[inline]
    pub fn note_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(true, Ordering::Relaxed);
    }

    /// Whether an allocation has been **refused** since the last collection —
    /// see the [`hard_alloc_failure`](Self::hard_alloc_failure) field doc.
    ///
    /// Consumed at the `safe_native_call` boundary, which is the one point on
    /// the native dispatch path where a collection is safe (every Java
    /// argument is pinned and remapped around it). It is deliberately NOT
    /// consumed inside the allocation wrappers themselves: those "must stay
    /// GC-free mid-callback, since their callers hold unrooted local
    /// `ObjectRef`s" (`vm_exec.rs`, the native-alloc young-pressure relief
    /// comment). Collecting there would be a use-after-free, not a fix.
    #[inline]
    pub fn hard_alloc_failure(&self) -> bool {
        self.hard_alloc_failure.load(Ordering::Relaxed)
    }

    /// Clear the hard-failure latch. Idempotent; the consumer clears after
    /// acting whether or not its overhead gate let the cycle run.
    #[inline]
    pub fn clear_hard_alloc_failure(&self) {
        self.hard_alloc_failure.store(false, Ordering::Relaxed);
    }

    /// `(parallel_mark_cycles, compaction_cycles, objects_relocated)` — did
    /// the 2026-08-13 default-on features actually engage this run?
    ///
    /// Reported at shutdown. A zero here is not a failure — the relocation-set
    /// selector legitimately declines a heap with no garbage worth moving, and
    /// a small-core box legitimately marks serially — but it IS the difference
    /// between "the feature ran and behaved" and "the feature never ran and
    /// the run proves nothing about it".
    pub fn feature_engagement(&self) -> (usize, usize, usize) {
        (
            self.parallel_mark_cycles.load(Ordering::Relaxed),
            self.compaction_cycles.load(Ordering::Relaxed),
            self.objects_relocated.load(Ordering::Relaxed),
        )
    }

    /// `(driver_passes, fallbacks)` — did `zgc_concurrent`'s controller drive
    /// the marking, and did it ever refuse to certify the result?
    ///
    /// `driver_passes == 0` with a non-zero `parallel_mark_cycles` would mean
    /// the pool marked without the driver, which is the state this adoption
    /// exists to leave behind.
    pub fn driver_engagement(&self) -> (usize, usize) {
        (
            self.driver_passes.load(Ordering::Relaxed),
            self.parallel_mark_fallbacks.load(Ordering::Relaxed),
        )
    }

    /// The fragmentation ratchet's current reading — Phase 2.2 of the ZGC
    /// maturity plan, which asked to turn "ZGC fragments" from an anecdote per
    /// suite run into a tracked number.
    ///
    /// Reported at shutdown by `VmHeap::print_gc_summary` on the `[GC]
    /// zgc-real:` line, so a suite runner can extract it per class with a grep
    /// and a CI job can ratchet on it. See [`ZFragGauge`] for what the two
    /// numbers mean and why one of them alone means nothing.
    pub fn frag_gauge(&self) -> ZFragGauge {
        let worst = self.frag_worst_permille.load(Ordering::Relaxed);
        ZFragGauge {
            samples: self.frag_samples.load(Ordering::Relaxed),
            worst_permille: (worst != usize::MAX).then_some(worst),
            free_permille: self.frag_worst_free_permille.load(Ordering::Relaxed),
            worst_cycle: self.frag_worst_cycle.load(Ordering::Relaxed),
        }
    }

    /// Take one post-sweep fragmentation reading.
    ///
    /// Called at the end of every collection with the arena guard still held.
    /// Cost is one `largest_free_block()` (O(1) — a `BTreeMap` last key and a
    /// field read), two divisions and, on the rare improving edge, three
    /// relaxed stores.
    ///
    /// The sampling condition is the instrument: a collection that leaves less
    /// than [`ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE`] of the heap free is not
    /// evidence about fragmentation at all, and counting it would turn this
    /// gauge into a second, worse occupancy trigger.
    fn sample_frag_gauge(&self, arena: &Arena, cycle: usize) {
        let capacity = arena.capacity();
        if capacity == 0 {
            return;
        }
        // Free = what the arena could still hand out at all: the un-bumped
        // middle plus both free lists. `remaining()` is exactly that sum.
        let free_permille = arena.remaining().saturating_mul(1000) / capacity;
        if free_permille < ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE {
            return;
        }
        // ...against the biggest single thing it could hand out.
        //
        // NOT `largest_free_block()` alone. That is the largest FREE-LIST
        // block, and on a heap that has not yet bumped its way to capacity the
        // biggest servable run is the un-bumped middle between the two
        // cursors, which is on no free list at all. Scoring the free list by
        // itself reads a pristine 1 MiB arena as **0 permille fragmented** —
        // caught by `a_collection_with_room_to_spare_takes_a_reading`, which
        // is what that test is for. `remaining()` is middle + both free lists,
        // so subtracting the lists leaves the middle exactly.
        let middle = arena.remaining().saturating_sub(arena.free_list_bytes());
        let servable = middle.max(arena.largest_free_block());
        let largest_permille = servable.saturating_mul(1000) / capacity;
        self.frag_samples.fetch_add(1, Ordering::Relaxed);
        if largest_permille < self.frag_worst_permille.load(Ordering::Relaxed) {
            self.frag_worst_permille
                .store(largest_permille, Ordering::Relaxed);
            self.frag_worst_free_permille
                .store(free_permille, Ordering::Relaxed);
            self.frag_worst_cycle.store(cycle, Ordering::Relaxed);
        }
        if largest_permille < ZGC_FRAG_FLOOR_PERMILLE
            && !self.frag_floor_warned.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                cycle,
                largest_free_permille = largest_permille,
                free_permille,
                capacity,
                largest_servable_block = servable,
                "zgc frag gauge: the arena is broken up — {}.{}% of the heap is                  free but the largest single block is only {}.{}% of capacity,                  so a request above that size cannot be served however much is                  free. This collector does not compact, so the shape does not                  recover on its own.",
                free_permille / 10,
                free_permille % 10,
                largest_permille / 10,
                largest_permille % 10,
            );
        }
    }

    /// The SATB pre-write barrier, for the concurrent marker — Phase 3.
    ///
    /// # What calls this, and why that is the whole point
    ///
    /// Nothing new. `VmHeap::satb_barrier` is already called before **every**
    /// reference store in this VM — the interpreter's `putfield`/`aastore`,
    /// the JIT's `aastore` and `putfield` helpers, `deopt_materialize`, and
    /// `vm_init` — because G1 needs it. Its ZGC arm was `{}`. So the mutator
    /// ingress that `zgc_concurrent.rs` describes as the missing piece
    /// ("nothing calls `ZMarkHandle::mark_live_offset` from a `getfield`, so
    /// the mutator ingress is empty in practice") did not need a new call
    /// site threaded through three code generators. It needed this arm to stop
    /// being empty.
    ///
    /// # Cost while nothing is marking
    ///
    /// One relaxed load of a never-written cache line, then return. That is
    /// the reason `mark_active` is a separate flag rather than, say, an
    /// `Option` probe or a lock: this sits on the store path of every Java
    /// program the VM runs, including every program that will never see a
    /// concurrent cycle.
    ///
    /// # Why SATB and not the load barrier ZGC actually uses
    ///
    /// Real ZGC marks on **read**, in the load barrier, which is why
    /// `zgc_concurrent.rs`'s mark-end is a *decision point* rather than a
    /// conclusion: with a read barrier every mutator stays a producer until it
    /// is stopped, hence the restart loop. A pre-write barrier is the other
    /// discipline — snapshot-at-the-beginning — and it is what this VM already
    /// has plumbed everywhere, at zero additional emission cost.
    ///
    /// The two are **not interchangeable**, and adopting this one has a
    /// consequence that must be written down before anybody relies on it: SATB
    /// keeps everything live at the snapshot, so it is *conservative* (an
    /// object that dies during the cycle is collected in the next one), while
    /// ZGC's load barrier is precise. Conservative is a throughput cost, not a
    /// correctness one, which is the right side to be wrong on for a first
    /// adoption. The restart loop stays correct under it — it simply reaches
    /// `Complete` sooner, because a snapshot's producer set really is bounded.
    ///
    /// # What is NOT done
    ///
    /// This publishes into the ingress; it does not yet run a cycle. There is
    /// no mark-start safepoint, no per-thread [`mark::ZMarkMutatorBuffer`]
    /// (every push takes an uncontended bucket mutex, which is the batching
    /// this barrier will want before it is on by default), and no coordinator
    /// pointed at this heap. `mark_active` is therefore never set to `true` by
    /// production code today — only by tests. Nothing here is reachable in a
    /// real run, and it must not be described as if it were.
    #[inline]
    pub fn satb_pre_barrier(&self, old_addr: usize) {
        // The entire cost of this barrier on a non-concurrent run.
        if !self.mark_active.load(Ordering::Relaxed) {
            return;
        }
        self.satb_pre_barrier_slow(old_addr);
    }

    /// Out-of-line remainder of [`Self::satb_pre_barrier`], so the fast path
    /// is a load and a branch and nothing else is inlined into every store
    /// site in the VM.
    #[cold]
    fn satb_pre_barrier_slow(&self, old_addr: usize) {
        if old_addr == 0 || !self.registry.contains(old_addr) {
            // A null overwrite carries no edge, and an address this heap never
            // handed out is not ours to mark — the same gate
            // `ZMarkContext::is_in_heap` applies to every child pointer.
            return;
        }
        // Bucket by address so concurrent mutators spread across the ingress
        // rather than contending on one mutex. `ZMarkIngress::push` masks this
        // into its bucket count, so any well-distributed key works; the
        // address shifted past the object-alignment zeros is the cheapest one
        // available here.
        self.mark_ingress.push(old_addr >> 3, old_addr as u64);
        self.mark_ingress_pushes.fetch_add(1, Ordering::Relaxed);
    }

    /// Card an OLD object whose fields have just been written --
    /// `zgc::remembered`.
    ///
    /// # A card names an OBJECT to re-scan, not a slot
    ///
    /// The first version of this recorded the slot address and
    /// `remembered_roots` read a reference word straight out of it. That is
    /// the *precise* remembered set, and this VM cannot feed it: the barrier
    /// hook every reference store already reaches is
    /// `GarbageCollector::write_barrier(obj, stored_value)`, which is handed
    /// the **object**. Recording an object base and re-enumerating its
    /// reference slots at cycle time is the classic card design, it is what
    /// the available hook can actually supply, and it is robust to the four
    /// different slot shapes an object can have -- a hand-computed slot offset
    /// is not, which is how the first version carded the tag word of a
    /// 16-byte cell instead of its reference word.
    ///
    /// # Cost while there is no old generation
    ///
    /// One `is_empty` check on a lock that is uncontended and, until a cycle
    /// has aged a page past the promotion age, always empty.
    #[inline]
    pub fn note_ref_store(&self, obj_addr: usize) {
        if self.old_page_ids.lock().is_empty() {
            return;
        }
        self.note_ref_store_slow(obj_addr);
    }

    #[cold]
    fn note_ref_store_slow(&self, obj_addr: usize) {
        let base = self.arena.lock().base_ptr() as usize;
        if obj_addr < base {
            return;
        }
        let page = ((obj_addr - base) / Self::Z_LOGICAL_PAGE_BYTES) as u64;
        if !self.old_page_ids.lock().contains(&page) {
            // A store into a young page needs no card: a young cycle scans
            // every young page anyway.
            return;
        }
        let offset = (obj_addr - base) % Self::Z_LOGICAL_PAGE_BYTES;
        self.remember_old_to_young(page, offset);
    }

    /// How many old-to-young edges are currently remembered. Diagnostic, and
    /// the discriminator a test needs between "the barrier is wired" and "the
    /// workload made no such store".
    pub fn remembered_edge_count(&self) -> usize {
        self.remembered
            .snapshot()
            .iter()
            .map(|s| s.bits_set())
            .sum()
    }

    /// Arm or disarm the concurrent-mark barrier — Phase 3 wiring and tests.
    ///
    /// Disarming clears the ingress, because a leftover address from a
    /// finished cycle would be republished into the next one as mark work
    /// against an arena that has since been swept and coalesced.
    pub fn set_mark_active(&self, active: bool) {
        if !active {
            self.mark_active.store(false, Ordering::Relaxed);
            self.mark_ingress.clear();
            self.mark_ingress_pushes.store(0, Ordering::Relaxed);
        } else {
            self.mark_ingress.clear();
            self.mark_ingress_pushes.store(0, Ordering::Relaxed);
            self.mark_active.store(true, Ordering::Relaxed);
        }
    }

    /// Is the concurrent-mark barrier armed?
    #[inline]
    pub fn mark_active(&self) -> bool {
        self.mark_active.load(Ordering::Relaxed)
    }

    /// How many overwritten references this barrier has published since the
    /// cycle began. See the field doc: zero with the barrier armed means the
    /// workload overwrote no references, which is a different fact from the
    /// barrier not being wired.
    pub fn mark_ingress_pushes(&self) -> usize {
        self.mark_ingress_pushes.load(Ordering::Relaxed)
    }

    /// Drain the mutator ingress — what a coordinator's mark-end flush calls.
    pub fn drain_mark_ingress(&self, out: &mut Vec<u64>) -> usize {
        self.mark_ingress.drain_into(out)
    }

    /// How many mark workers a parallel stop-the-world mark should use.
    ///
    /// `0` and `1` both mean "do not go parallel" — the caller falls back to
    /// the serial loop, which has no pool to spawn and no join to pay for.
    fn parallel_mark_workers(&self) -> usize {
        // DEFAULT-ON since 2026-08-13, for the gauntlet. Unset means "pick a
        // count"; `0` is the kill switch and still means serial.
        //
        // The default is deliberately not `cores`: this is a stop-the-world
        // phase on a machine that is also running the suite harness and, on
        // the Windows box, several sibling worktrees' builds. Half the cores
        // capped at 4 leaves the box usable and still gets most of the
        // available parallelism on a mark, which is memory-bound long before
        // it is core-bound.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let requested = match cratonvm_types::flags::runtime_var("CRATONVM_ZGC_PARMARK") {
            Ok(v) => v.trim().parse::<usize>().unwrap_or(0),
            Err(_) => Z_PARMARK_DEFAULT_WORKERS,
        };
        if requested == 0 {
            return 0;
        }
        // Never more workers than the machine has cores to run them on: this
        // is a stop-the-world phase, so oversubscription buys nothing and
        // costs context switches inside the pause it is meant to shorten.
        requested.min(cores).min(Z_PARMARK_MAX_WORKERS)
    }

    /// Mark the whole strong closure from `roots` using the parallel engine,
    /// at a stop-the-world — Phase 3 of the ZGC maturity plan.
    ///
    /// # This is PARALLEL, not CONCURRENT, and the difference is the point
    ///
    /// Every mutator is stopped for the whole of this call. That is what makes
    /// it adoptable today: with no mutator running there is no producer racing
    /// the marker, so it needs **no barrier of any kind** — not the load
    /// barrier real ZGC uses, and not the SATB pre-write barrier
    /// [`Self::satb_pre_barrier`] now feeds. `try_end_mark` is expected to
    /// answer `Complete` on the first pass for exactly that reason, and a
    /// `Restart` here would mean the engine found buffered work at a
    /// safepoint where by construction there can be none.
    ///
    /// It is therefore the honest intermediate step between the single-
    /// threaded sweep this collector has always run and the concurrent cycle
    /// the plan ends at: it exercises the coordinator, the striped queues, the
    /// work stealing and the termination handshake against a REAL heap and a
    /// real object graph, where the only prior driver was `TestMarkContext`.
    ///
    /// # What the caller must have done first
    ///
    /// `begin_concurrent_mark_cycle` must be open, or `visit_refs` traces
    /// weak/soft/phantom referents as strong edges and no reference can ever
    /// be cleared. It warns once if not, and this function does not rely on
    /// that warning — it is the caller's contract.
    ///
    /// Returns the engine's stats for the cycle, or `None` when the driver
    /// could not certify a complete mark set — see
    /// [`Self::mark_with_controller_stw`], whose refusal this forwards.
    fn mark_parallel_stw(&self, roots: &[u64], workers: usize) -> Option<mark::ZMarkStatsSnapshot> {
        self.mark_with_controller_stw(roots, workers)
    }

    /// Drive one whole marking cycle through
    /// [`crate::zgc_concurrent::ZgcConcurrentMarkController`] against this
    /// heap — the adoption Phase 3's exit criterion asks for.
    ///
    /// # What this closes
    ///
    /// The plan's Phase 3 exits on "`zgc_concurrent`'s coordinator drives a
    /// real collection". Until this function that was false in a way no
    /// measurement would have caught: the driver existed, it was complete, it
    /// was unit-tested, and its only context was `TestMarkContext`. The heap
    /// marked with its own single-threaded loop and later with a bespoke
    /// `mark_to_completion` call that used the worker pool but **not the
    /// driver** — so the restart loop, the mark-end handshake and the
    /// reference re-drain were all still bypassed on the real heap.
    ///
    /// # Why `ZgcNoMutatorSafepoint` is the CORRECT safepoint here
    ///
    /// Its doc says it is legal "only when the thread driving the heap is the
    /// sole mutator", and that is exactly the state a `StopTheWorldToken`
    /// proves: every other mutator is parked (or was forcibly stopped and
    /// conservatively scanned) before `collect_garbage` is entered. There is
    /// nothing to stop and no per-thread buffer to flush, so a no-op
    /// implementation is not a stub standing in for the real thing — it is the
    /// right answer to the question the trait asks. A **concurrent** cycle
    /// needs the real one (plan item C1); this one does not.
    ///
    /// # Why `refs: None` is safe here, against the parameter's own warning
    ///
    /// `ZgcConcurrentMarkParams`'s doc says `None` "skips the phase entirely,
    /// which is only correct for a heap with no registered `Reference`
    /// objects (i.e. a test)". That warning is about a **concurrent** cycle,
    /// where the mark-end safepoint is the only place a complete mark set
    /// exists and therefore the only place the reference phase can run.
    ///
    /// Here the phase is not skipped — it runs immediately after this returns,
    /// in `collect_garbage`, exactly where it always has, including its
    /// existing INT-8 remark that re-drains whatever `keep_alive` resurrects.
    /// Moving it into the hook is plan item C3 and belongs with C1, not before
    /// it: doing it now would duplicate a working reference phase for no
    /// change in behaviour.
    ///
    /// # Fail-closed
    ///
    /// `mark_set_complete` is the driver's load-bearing verdict and a sweep
    /// against an incomplete mark set is a use-after-free. This returns `None`
    /// rather than stats when the driver cannot certify one, and the caller
    /// falls back to the single-threaded marker. That fallback is not
    /// belt-and-braces: it is the only reason this adoption can be default-on.
    fn mark_with_controller_stw(
        &self,
        roots: &[u64],
        workers: usize,
    ) -> Option<mark::ZMarkStatsSnapshot> {
        use crate::zgc_concurrent::{
            ZgcConcurrentMarkController, ZgcConcurrentMarkParams, ZgcNoMutatorSafepoint,
        };
        let bridge: std::sync::Arc<dyn mark::ZMarkContext> =
            std::sync::Arc::new(ZHeapMarkBridge { heap: self });
        let coordinator = std::sync::Arc::new(mark::ZMarkCoordinator::new(bridge, workers));
        coordinator.begin_cycle();
        coordinator.push_roots(roots);

        // One restart is budgeted rather than zero, for the reason the old
        // bespoke path gave: no mutator can race us, but budgeting zero would
        // turn any flush that legitimately produced work into an "incomplete
        // mark set" verdict on a set the sweep is about to trust.
        let params = ZgcConcurrentMarkParams::new(
            std::sync::Arc::clone(&coordinator),
            std::sync::Arc::new(ZgcNoMutatorSafepoint),
        )
        .with_max_mark_end_restarts(Z_PARMARK_RESTART_BUDGET);

        let outcome = match ZgcConcurrentMarkController::spawn(params).join_cycle() {
            Ok(o) => o,
            Err(_) => {
                tracing::error!(
                    target: "zgc",
                    "zgc mark: the concurrent-mark driver PANICKED; falling back to                      the single-threaded marker for this cycle"
                );
                coordinator.end_cycle();
                return None;
            }
        };
        let stats = coordinator.stats().snapshot();
        coordinator.end_cycle();
        self.driver_passes
            .fetch_add(outcome.passes, Ordering::Relaxed);

        if !outcome.mark_set_complete {
            tracing::error!(
                target: "zgc",
                passes = outcome.passes,
                restarts = outcome.restarts,
                redrains = outcome.redrains,
                "zgc mark: the driver could not certify a complete mark set AT A                  SAFEPOINT — no mutator is running, so this cannot be a mutator                  race. Falling back to the single-threaded marker rather than                  sweeping against it"
            );
            return None;
        }
        tracing::debug!(
            target: "zgc",
            workers,
            passes = outcome.passes,
            restarts = outcome.restarts,
            marked = stats.objects_marked,
            "zgc mark: driven by zgc_concurrent's coordinator"
        );
        Some(stats)
        // `coordinator` drops here. The controller thread was joined by
        // `join_cycle` above, and `ZMarkCoordinator::drop` stops and JOINS
        // every worker — so no thread holding a clone of `bridge` outlives
        // this borrow of `self`.
    }

    /// The pool-only marking path this heap used between 2026-08-13 and
    /// 2026-08-14. Kept as a private helper so the driver-based path above has
    /// something to be compared against in a bench, and referenced by name in
    /// the plan's Phase 3 note; not on any live path.
    #[cfg(test)]
    fn mark_pool_only_stw(&self, roots: &[u64], workers: usize) -> mark::ZMarkStatsSnapshot {
        let bridge: std::sync::Arc<dyn mark::ZMarkContext> =
            std::sync::Arc::new(ZHeapMarkBridge { heap: self });
        let coordinator = mark::ZMarkCoordinator::new(bridge, workers);
        coordinator.begin_cycle();
        coordinator.push_roots(roots);
        // One restart is budgeted rather than zero. Not because a mutator can
        // race us — none is running — but because budgeting zero would turn
        // any future flush that legitimately produces work into an
        // "INCOMPLETE mark set" verdict, and an incomplete mark set is what
        // the sweep is about to act on.
        let report = coordinator.mark_to_completion(Z_PARMARK_RESTART_BUDGET);
        if report.budget_exhausted {
            // Cannot happen with the world stopped, which is exactly why it is
            // worth saying loudly if it ever does: it would mean the mark set
            // the sweep is about to trust is incomplete.
            tracing::error!(
                target: "zgc",
                passes = report.passes,
                restarts = report.restarts,
                "zgc parallel mark: restart budget exhausted AT A SAFEPOINT — no \
                 mutator is running, so this cannot be a mutator race; the mark \
                 set may be incomplete"
            );
        }
        coordinator.end_cycle();
        report.stats
        // `coordinator` drops here; its Drop stops and JOINS every worker, so
        // no thread holding a clone of `bridge` outlives this borrow of `self`.
    }

    /// Logical page size for the relocation-set view over the arena.
    ///
    /// The arena is one flat span with no pages, and `zgc::page`'s allocator
    /// is not adopted -- replacing `Arena` with it is a rewrite of the
    /// allocation path, not an increment. But `forwarding`'s relocation-set
    /// SELECTOR only needs pages as an accounting unit: a page id, the live
    /// bytes on it, and its allocated extent. Imposing a logical grid over the
    /// arena gives it all three, which is what turns compaction from "slide
    /// the whole region" into "evacuate the pages whose garbage pays for the
    /// copy".
    ///
    /// 2 MiB matches `ZPageConfig`'s Small page size, so the accounting unit
    /// is the one the rest of the ZGC modules already reason in, and it is
    /// four times `ZGC_TLAB_MAX_CHUNK` -- big enough that a page is not a
    /// single thread's chunk, small enough to be a useful granularity on the
    /// 64 MiB default heap.
    const Z_LOGICAL_PAGE_BYTES: usize = 2 * 1024 * 1024;

    /// The logical grid as real [`page::ZPageReal`] views, with per-page
    /// `used` and `live_bytes` filled in from the live set.
    ///
    /// Views, not pages: they allocate nothing and free nothing -- see
    /// `ZPageReal::view`. They exist so the page-keyed consumers in this crate
    /// (`forwarding`'s selector, `generation`'s scope, `remembered`'s table)
    /// can be driven against this arena-backed heap without the arena being
    /// replaced by the page allocator first.
    fn logical_pages(
        &self,
        live: &[usize],
        base: usize,
        low_end: usize,
    ) -> Vec<std::sync::Arc<page::ZPageReal>> {
        let pages = (low_end - base).div_ceil(Self::Z_LOGICAL_PAGE_BYTES);
        let mut live_bytes = vec![0usize; pages.max(1)];
        for addr in live {
            if *addr < base || *addr >= low_end {
                continue;
            }
            let idx = (*addr - base) / Self::Z_LOGICAL_PAGE_BYTES;
            live_bytes[idx] += Self::alloc_size(self.header_ref(*addr as *mut u8)).unwrap_or(0);
        }
        let ages = self.page_ages.lock();
        (0..pages)
            .map(|i| {
                let page_base = base + i * Self::Z_LOGICAL_PAGE_BYTES;
                let span = Self::Z_LOGICAL_PAGE_BYTES;
                // `used` is the arena's bump extent inside this cell, which is
                // what `walk_bounds` must report and what the selector's
                // "capacity" means -- bytes never allocated cost nothing to
                // reclaim.
                let used = low_end.saturating_sub(page_base).min(span);
                let p = page::ZPageReal::view(
                    i as u64,
                    page::ZPageSizeClass::Small,
                    page_base,
                    span,
                    used,
                    live_bytes[i],
                );
                p.set_age(ages.get(i).copied().unwrap_or(0));
                std::sync::Arc::new(p)
            })
            .collect()
    }

    /// Age every logical page by one cycle and return the young/old split
    /// under `policy`.
    ///
    /// This is `zgc::generation`'s promotion rule applied to the grid: a page
    /// whose age after this cycle reaches the policy's promotion age is old,
    /// and everything else is young.
    fn age_pages_and_split(
        &self,
        page_count: usize,
        policy: &generation::ZPromotionPolicy,
    ) -> (Vec<u64>, Vec<u64>) {
        let mut ages = self.page_ages.lock();
        if ages.len() < page_count {
            ages.resize(page_count, 0);
        }
        let mut young = Vec::new();
        let mut old = Vec::new();
        for (i, age) in ages.iter_mut().enumerate().take(page_count) {
            *age = age.saturating_add(1);
            if policy.should_promote(*age) {
                old.push(i as u64);
            } else {
                young.push(i as u64);
            }
        }
        (young, old)
    }

    /// Record an old-to-young edge for a young-scoped cycle -- `zgc::remembered`.
    ///
    /// Called from the store barrier. `slot_page` must already be known old
    /// and the value it now holds young; this only writes the bit.
    fn remember_old_to_young(&self, slot_page: u64, page_relative_offset: usize) {
        let _ = self
            .remembered
            .remember(slot_page, page_relative_offset);
    }

    /// The remembered set's extra roots for a young cycle: every old-page slot
    /// recorded as pointing into young.
    ///
    /// Without these a young collection is simply wrong -- an object reachable
    /// only from an old-generation field has no path from the thread roots and
    /// would be swept while live. That is the entire justification for the
    /// store barrier's existence, so it is asserted rather than assumed by
    /// `a_young_scope_treats_remembered_old_slots_as_roots`.
    fn remembered_roots(&self, base: usize) -> Vec<usize> {
        let mut roots = Vec::new();
        for set in self.remembered.snapshot() {
            let page_base = base + set.page_id() as usize * Self::Z_LOGICAL_PAGE_BYTES;
            let mut carded: Vec<usize> = Vec::new();
            set.iterate(|offset| carded.push(page_base + offset));
            for obj in carded {
                // A card names an OBJECT to re-scan. Re-check the registry:
                // the carded object may have died since the card was written,
                // and a card is allowed to be stale -- that is the price of
                // recording eagerly on the store path.
                if !self.registry.contains(obj) {
                    continue;
                }
                use census::ZCensusHeapView;
                self.reference_slots(obj as u64, &mut |slot| {
                    let target = slot.raw_word as usize;
                    if target != 0 && self.registry.contains(target) {
                        roots.push(target);
                    }
                });
            }
        }
        roots
    }

    /// Apply the ZGC **load barrier** to one reference slot, in place.
    ///
    /// This is the read path Phase 4 is about. Given the address of an 8-byte
    /// reference word, it runs `barrier::load_barrier_fast_bad`; on the fast
    /// path (the overwhelmingly common case, and the only one reachable while
    /// no cycle is armed) it returns the word unchanged. On the slow path it
    /// calls `barrier::load_barrier_slow`, which forwards the offset through
    /// [`Self::forward`], publishes it to the marker if a mark is running, and
    /// **self-heals** the slot by CAS-ing the corrected colored word back.
    ///
    /// # Colored slots, and why the word returned here is not a pointer
    ///
    /// A colored word is `Z_COLORED_TAG | color | 42-bit OFFSET`. The barrier
    /// hands back a bare offset, so a caller wanting a machine address must
    /// add the heap base. That conversion is the reason
    /// `ZMarkContext::heap_base` had to stop being `None`.
    ///
    /// # Cost while no cycle is armed
    ///
    /// One relaxed load, one AND, one compare. `bad_mask` is derived from a
    /// good mask that stays at `Z_REMAPPED`, under which a plain pointer
    /// classifies as good — so an unarmed run takes the fast path on every
    /// reference read and touches nothing else.
    ///
    /// Returns the **machine address** the slot should be read as, or `None`
    /// for null.
    #[inline]
    fn load_barrier_slot(&self, slot_addr: usize) -> Option<usize> {
        use barrier::ZBarrierContext;
        // SAFETY: the caller supplies the address of an 8-byte-aligned
        // reference word inside a live object.
        let slot = unsafe { &*(slot_addr as *const std::sync::atomic::AtomicU64) };

        // ---- THE GATE, and it is not an optimisation ---------------------
        //
        // The barrier cannot be run over an UNCOLORED slot. `ZFastPath::Good`
        // carries a bare 42-bit OFFSET, and the classifier decides good-vs-bad
        // by testing the metadata bits — but a plain machine pointer into this
        // arena is well above 2^42, so its bits 42-46 are address bits that the
        // classifier would read as colors and `address_mask` would truncate.
        // Every reference read would take the slow path and every one of them
        // would resolve to the wrong object.
        //
        // So the barrier runs only once slots actually hold colored words,
        // which is what flipping the good mask off `Z_REMAPPED` declares.
        // Nothing in a default run flips it. The cost of the gate on an
        // unarmed run is one relaxed load and a compare.
        if !self.load_barrier_armed() {
            let raw = slot.load(Ordering::Relaxed) as usize;
            return (raw != 0).then_some(raw);
        }

        let bad = self.bad_mask();
        let mask = self.address_mask();
        match barrier::load_barrier_fast_bad(slot, bad, mask) {
            barrier::ZFastPath::Good(offset) => {
                if slot.load(Ordering::Relaxed) == vaddr::Z_NULL {
                    return None;
                }
                let base = <Self as mark::ZMarkContext>::heap_base(self).unwrap_or(0);
                Some(base.wrapping_add(offset) as usize)
            }
            barrier::ZFastPath::Bad(observed) => {
                let offset = barrier::load_barrier_slow(
                    slot,
                    observed,
                    self,
                    barrier::ZBarrierKind::Load,
                );
                if offset == 0 {
                    return None;
                }
                let base = <Self as mark::ZMarkContext>::heap_base(self).unwrap_or(0);
                Some(base.wrapping_add(offset) as usize)
            }
        }
    }

    /// Is the read-path load barrier armed?
    ///
    /// Only ever true when the good mask has been flipped off `Z_REMAPPED`,
    /// which nothing in a default run does. Exposed so a test can assert the
    /// unarmed cost claim rather than take it on trust.
    pub fn load_barrier_armed(&self) -> bool {
        self.barrier_armed.load(Ordering::Acquire)
    }

    /// Arm or disarm the read-path barrier by flipping the good mask.
    ///
    /// `Some(color)` arms it for that mark parity; `None` returns to
    /// `Z_REMAPPED`, the quiescent "addresses are plain" state.
    pub fn set_barrier_color(&self, color: Option<vaddr::ZColor>) {
        let mask = match color {
            Some(vaddr::ZColor::Marked0) => vaddr::Z_MARKED0,
            Some(vaddr::ZColor::Marked1) => vaddr::Z_MARKED1,
            // `Finalizable` is not a phase: it marks a pointer reached ONLY
            // through a finalizable object, and is never the cycle's good
            // color. Treating it as one would make every ordinary reference
            // bad for a whole cycle.
            Some(vaddr::ZColor::Finalizable)
            | Some(vaddr::ZColor::Remapped)
            | None => vaddr::Z_REMAPPED,
        };
        self.barrier_good_mask.store(mask, Ordering::Release);
        self.barrier_armed.store(color.is_some(), Ordering::Release);
        // ...and the process-wide codegen gate. The JIT decides whether to
        // emit a raw inline reference load while holding no heap handle, so
        // this one fact has to be reachable without one -- see
        // `crate::zgc_read_barrier_armed` for why that exception is safe.
        cratonvm_types::set_zgc_read_barrier_armed(color.is_some());
    }

    /// Is the default-off stop-the-world compaction sub-flag set?
    ///
    /// `CRATONVM_ZGC_RELOCATE=1`. This is the sub-flag the production plan's
    /// R5 row insists on, and which that row's own wording made stale: it said
    /// "default-off within the already-default-off `zgc` feature", and `zgc`
    /// has been default-ON since 2026-08-10. This switch is no longer
    /// belt-and-braces; it IS the belt.
    ///
    /// Deliberately NOT `zgc_relocation_permitted`. That gate answers a
    /// *safety* question (is the JIT off?) and lives in `vm_init`; this one
    /// answers an *intent* question and lives here. A caller needs both.
    fn relocation_requested(&self) -> bool {
        // DEFAULT-ON since 2026-08-13, for the gauntlet.
        //
        // `CRATONVM_ZGC_RELOCATE=0` (or `off`/`false`/`no`) is the kill switch
        // and restores the non-moving behaviour byte for byte -- the sweep
        // simply returns an empty `PointerMap` as it always did, so the A/B is
        // a re-run and not a rebuild.
        //
        // **What to watch on the first gauntlet.** This is the first
        // configuration in which a ZGC cycle returns a NON-EMPTY pointer map,
        // so every consumer of one now runs for this collector: JIT frame
        // maps, monitor tables, external root providers and native side
        // tables. Those consumers are collector-agnostic and already run for
        // the generational moving-young path, and the two `VmHeap::Zgc`
        // predicates that take a pre-GC address were audited and tested (R6) --
        // but "already runs for another collector" is not "has run for this
        // one". A crash or a stale-reference warning that appears only with
        // this flag on is this change, and `=0` is the bisect.
        match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_RELOCATE") {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    }

    /// Compact the low end of the arena at a stop-the-world, after the mark.
    /// Phase 4 of the ZGC maturity plan.
    ///
    /// # What it does
    ///
    /// Slides every marked survivor in the small-object region down into the
    /// lowest free space, in address order; then rewrites **every reference
    /// slot in every survivor** through the resulting `from -> to` map; then
    /// drops the bump cursor to the end of the compacted region. That last
    /// step is the entire point -- a non-compacting arena's cursor is a
    /// one-way ratchet, and this is the only operation that can return the
    /// middle of the heap to one contiguous run.
    ///
    /// Returns `(objects_moved, bytes_reclaimed, pointer_map)`.
    ///
    /// # Three gates, all of which must be open
    ///
    /// 1. `CRATONVM_ZGC_RELOCATE=1` -- *intent* ([`Self::relocation_requested`]).
    /// 2. `zgc_relocation_permitted` -- *safety*. **Refuses whenever the JIT is
    ///    enabled**: JIT-compiled code loads reference fields with no ZGC load
    ///    barrier, so a moving cycle would hand it stale pointers into
    ///    evacuated objects. That gate lives in `vm_init` and is now tested.
    /// 3. A stop-the-world token, held by the caller for the whole call.
    ///
    /// # What it deliberately is not
    ///
    /// Not *concurrent* relocation. Every mutator is stopped, so no load
    /// barrier is needed to observe the move -- which is exactly why this step
    /// is reachable before barrier emission lands, and why it is an honest
    /// first cut rather than a stand-in for `zgc::relocate`.
    ///
    /// **The high end is not compacted.** Large objects bump down from
    /// capacity with their own free list; moving them needs the same slide in
    /// the opposite direction against a different free structure. Left out on
    /// purpose -- the low end is where TLAB chunks fragment, which is the
    /// measured problem.
    ///
    /// # The caller's remaining obligation
    ///
    /// The returned `PointerMap` is **non-empty**, and `VmHeap::Zgc`'s arms
    /// assert non-moving throughout. Every consumer of a raw heap address
    /// outside this heap -- JIT frame maps, monitor tables, external root
    /// providers, native side tables -- must be remapped through it, exactly as
    /// the generational and G1 paths already do. Auditing those arms is the
    /// reason this stays behind a default-off flag instead of being wired into
    /// `collect_garbage`'s normal path.
    /// # Why the live set is a PARAMETER
    ///
    /// It reads mark bits in no version of this function, and that is
    /// deliberate. The first draft filtered `registered` by `GC_FLAG_MARKED`
    /// and moved **nothing**, because the sweep clears every survivor's mark
    /// bit before returning -- so by the time a post-sweep compaction runs,
    /// the marks it wanted are gone. Taking the set explicitly makes the
    /// caller say which objects are live instead of inferring it from state
    /// another phase owns: `collect_garbage` passes the post-sweep registry
    /// (everything the sweep did not reclaim), and a test passes whatever it
    /// built.
    #[cfg(test)]
    pub(crate) fn relocate_stw_for_test(
        &self,
        live: &[usize],
    ) -> (usize, usize, cratonvm_types::PointerMap) {
        self.relocate_stw(live)
    }

    fn relocate_stw(&self, live: &[usize]) -> (usize, usize, cratonvm_types::PointerMap) {
        // `relocate::ZRelocationRecord` rather than a local map: it is the
        // module's from->to ledger, it builds the `PointerMap` this function
        // must return, and it carries the reserve so a large evacuation does
        // not rehash mid-slide. Hand-rolling a `FxHashMap` here -- which this
        // did first -- is a second implementation of the thing the module
        // exists to be, and the two would drift on exactly the question that
        // matters: whether an identity entry is recorded for an object that
        // did not move. It is not; only real moves are recorded.
        let record = relocate::ZRelocationRecord::new(true, live.len());
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        let mut moved = 0usize;
        let reclaimed;
        // Captured out of the arena scope for the post-slide verifier, which
        // runs after the guard is dropped.
        let mut arena_lo = 0usize;
        let mut arena_hi = 0usize;

        {
            let mut arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            let low_end = base + arena.used_low_for_compaction();
            arena_lo = base;
            arena_hi = base + arena.capacity();

            // Survivors in ADDRESS order. The slide requires it: an object may
            // only be copied into space a lower-addressed survivor has already
            // vacated. Out-of-order copying overwrites a survivor that has not
            // moved yet -- silent heap corruption, not a failed assert.
            // ---- Which pages are worth evacuating -------------------------
            //
            // `forwarding::ZRelocationSet::select` is the real ZGC selector
            // and this is its first production caller. It ranks the logical
            // pages by profitability -- copy cost is live bytes, benefit is
            // `extent - live` -- refuses pages above `max_live_occupancy`
            // (0.25 by default: copy one byte to reclaim at least three), and
            // stops at `max_evacuation_bytes`.
            //
            // Without it this function slid the WHOLE low region every cycle,
            // which copies a dense, wholly-live page for no reclaim at all.
            // With it, a page that is 90% live is left to decay.
            // ---- The generational scope -----------------------------------
            //
            // `generation::ZPromotionPolicy` ages every logical page one cycle
            // and splits young from old; `ZGenerationScope::from_pages` then
            // names the byte ranges a young cycle may touch. Both are driven
            // from real `page::ZPageReal` VIEWS over the arena grid -- see
            // `logical_pages` for why a view is not a page.
            //
            // The scope is advisory for now: this cycle still evacuates from
            // whichever pages the relocation-set selector picks, young or old.
            // What it buys today is the accounting and the ages; scoping the
            // MARK to young is what needs the remembered set to be complete,
            // and completeness is a property of every store site, not of this
            // function.
            let views = self.logical_pages(live, base, low_end);
            let promo = generation::ZPromotionPolicy::default();
            let (young_ids, old_ids) = self.age_pages_and_split(views.len(), &promo);
            let young_views: Vec<_> = views
                .iter()
                .filter(|p| young_ids.contains(&p.id()))
                .cloned()
                .collect();
            let scope = generation::ZGenerationScope::from_pages(
                generation::ZGeneration::Young,
                self.gc_count.load(Ordering::Relaxed) as u64,
                &young_views,
                young_ids.len() == views.len(),
            );
            // Every old page gets a remembered set, so the store barrier has
            // somewhere to record an old-to-young edge before the next cycle.
            for id in &old_ids {
                self.remembered
                    .register_old_page(*id, Self::Z_LOGICAL_PAGE_BYTES);
            }
            self.old_page_ids.lock().clone_from(&old_ids);
            tracing::debug!(
                target: "zgc",
                young = young_ids.len(),
                old = old_ids.len(),
                scope_pages = scope.page_count(),
                "zgc generational split over the logical grid"
            );

            // `adapters::page_candidates` rather than a local map. That module
            // exists so there is EXACTLY ONE conversion between a
            // `page::ZPageReal` and a `forwarding::PageCandidate`; hand-rolling
            // a second one here is the thing its header calls a symptom, and it
            // is how `capacity_bytes` comes to mean the page SPAN in one place
            // and the bump EXTENT in another -- an ambiguity that inverts the
            // selector's profitability ranking.
            let candidates = adapters::page_candidates(&views);
            let policy = forwarding::ZRelocationPolicy::default();
            let reloc_set = forwarding::ZRelocationSet::select(&candidates, &policy);
            let mut selected: std::collections::HashSet<u64> =
                reloc_set.pages().iter().map(|p| p.page_id).collect();
            // A pinned object makes its whole PAGE immovable.
            //
            // Page granularity, deliberately, and it is what G1 does with
            // `pin_region_for_addr`: the slide's placement probe reasons about
            // pages, so an object-granular pin would need the probe to route
            // around individual survivors inside a page it is otherwise
            // compacting — more machinery, and every extra rule in that probe
            // is a chance to overwrite something live. Dropping the page costs
            // one page's worth of reclaim for the duration of a critical
            // section, which is bounded by the section itself.
            let pins = self.critical_pin_addrs();
            if !pins.is_empty() {
                let page_span = Self::Z_LOGICAL_PAGE_BYTES;
                let mut dropped = 0usize;
                for addr in pins {
                    if addr >= base && addr < low_end {
                        if selected.remove(&(((addr - base) / page_span) as u64)) {
                            dropped += 1;
                        }
                    }
                }
                if dropped > 0 {
                    tracing::debug!(
                        target: "zgc",
                        dropped,
                        "zgc relocate: pages withheld from the relocation set because \
                         a JNI critical section pins an object on them"
                    );
                }
            }
            if selected.is_empty() {
                // Nothing profitable to move. Not a failure -- it is the
                // selector doing its job on a heap whose pages are all dense.
                let reclaimed = arena.retract_cursor_into_free_tail();
                return (0, reclaimed, cratonvm_types::PointerMap::default());
            }

            let page_of = |addr: usize| ((addr - base) / Self::Z_LOGICAL_PAGE_BYTES) as u64;
            // Survivors ON THE SELECTED PAGES ONLY. An object on an unselected
            // page must not move, so the slide's destination cursor has to
            // start above the highest unselected survivor -- see below.
            let mut survivors: Vec<usize> = live
                .iter()
                .copied()
                .filter(|b| *b >= base && *b < low_end)
                .filter(|b| selected.contains(&page_of(*b)))
                .collect();
            survivors.sort_unstable();

            // The slide may only use space below the first page it is allowed
            // to disturb. Sliding into an unselected page would overwrite
            // survivors this cycle promised not to touch.
            // From the FILTERED set, not `reloc_set`: a pinned page dropped
            // above may have been the lowest, and starting the slide at a page
            // that is no longer selected would place survivors on top of the
            // very object the pin exists to hold still.
            let first_selected_page = *selected
                .iter()
                .min()
                .expect("non-empty, checked above");
            let slide_floor = base + first_selected_page as usize * Self::Z_LOGICAL_PAGE_BYTES;

            let mut dest = slide_floor;
            for from in survivors {
                let Some(size) = Self::alloc_size(self.header_ref(from as *mut u8)) else {
                    // A header this collector cannot size cannot be moved, and
                    // nothing above it may move either or the slide would run
                    // over it. Stop here rather than guess.
                    tracing::warn!(
                        target: "cratonvm::gc::guard",
                        addr = from,
                        "zgc relocate: unsizable survivor stops the slide"
                    );
                    dest = from;
                    break;
                };
                // THE DESTINATION MUST LIE ENTIRELY INSIDE SELECTED PAGES.
                //
                // `ZRelocationSet::select` ranks by descending garbage ratio
                // and takes a prefix, so the selected ids are an arbitrary,
                // NON-CONTIGUOUS set -- every page at or above
                // `max_live_occupancy` is skipped. Until 2026-08-14 this loop
                // marched one cursor up from `slide_floor` and placed every
                // survivor consecutively, so as soon as the selected pages'
                // live bytes exceeded the gap below the first dense page, this
                // memmove copied survivors straight over the live objects ON
                // it. Silent heap corruption; it reached the outside world as
                // `compaction must not raise the cursor: 15348137664 > ...`
                // (a clobbered header read back as a 1.9-billion-element
                // array) and, on one run, as a bare SIGSEGV.
                //
                // The comment eight lines above the old code said the cursor
                // "has to start above the highest unselected survivor". That
                // was the right rule and the code implemented something else.
                //
                // Skipping rather than clamping keeps the selector's choice:
                // pages above a dense one are still compacted, into the next
                // selected page, which is what makes a non-contiguous
                // selection worth having at all.
                let span = size.max(1);
                let mut probe = dest;
                let mut chosen: Option<usize> = None;
                while probe < from {
                    let cand = (probe + 7) & !7;
                    // Only a strictly-downward move is worth anything, and an
                    // upward one would overwrite a survivor not yet visited.
                    if cand >= from || cand + span > low_end {
                        break;
                    }
                    let first_page = page_of(cand);
                    let last_page = page_of(cand + span - 1);
                    match (first_page..=last_page).find(|pg| !selected.contains(pg)) {
                        // The span would touch an unselected page: restart the
                        // probe at the page after it. Strictly increasing, so
                        // this terminates in at most one pass over the grid.
                        Some(blocked) => {
                            probe = base + (blocked as usize + 1) * Self::Z_LOGICAL_PAGE_BYTES;
                        }
                        None => {
                            chosen = Some(cand);
                            break;
                        }
                    }
                }
                match chosen {
                    Some(to) => {
                        debug_assert!(to < from, "the slide must never move an object UP");
                        // SAFETY: `size` bytes are live at `from`, `to` is
                        // inside the arena and strictly below `from`, and the
                        // regions may overlap -- `copy` is memmove, correct in
                        // that direction.
                        unsafe { std::ptr::copy(from as *const u8, to as *mut u8, size) };
                        pairs.push((from, to));
                        moved += 1;
                        dest = to + size;
                    }
                    // Nowhere below it inside a selected page: it stays put,
                    // and the cursor continues above it so a later survivor
                    // cannot be placed on top of it.
                    None => dest = from + size,
                }
            }
            // The cursor may only drop to the compacted end if nothing that
            // STAYED PUT lives above it.
            //
            // Only UNSELECTED survivors count here. An object on a selected
            // page has already been slid down, so its entry in `live` names an
            // address it no longer occupies -- taking the maximum over the
            // whole live set pins the cursor at the pre-compaction top and
            // reclaims exactly nothing, which is what the first draft did.
            let highest_pinned_end = live
                .iter()
                .copied()
                .filter(|b| *b >= base && *b < low_end)
                .filter(|b| !selected.contains(&page_of(*b)))
                .map(|b| {
                    let size = Self::alloc_size(self.header_ref(b as *mut u8)).unwrap_or(0);
                    // An extent that runs past the bump cursor is impossible
                    // for a live object and means the header is not one --
                    // which is how the 2026-08-14 corruption presented: a
                    // clobbered header sized at 15,348,137,664 bytes on a
                    // 168 MB arena, carried into `compact_low_to` as a cursor
                    // 91x the heap.
                    //
                    // Refuse it here rather than let it set the cursor. Pinning
                    // at `low_end` reclaims nothing this cycle, which is the
                    // conservative answer and strictly better than either
                    // trusting the number or panicking: `alloc_size` has no
                    // arena to check against, and this is the caller that does.
                    if size > low_end.saturating_sub(b) {
                        tracing::warn!(
                            target: "cratonvm::gc::guard",
                            addr = b,
                            size,
                            low_end,
                            "zgc relocate: a pinned survivor's extent runs past the \
                             bump cursor -- refusing to reclaim this cycle rather than \
                             trust the header"
                        );
                        low_end
                    } else {
                        b + size
                    }
                })
                .max()
                .unwrap_or(base);
            let new_cursor = dest.max(highest_pinned_end) - base;
            reclaimed = arena.compact_low_to(new_cursor);
            // One batched publish after the slide, not one per object: the
            // record is read by the rewrite pass below, which must see the
            // WHOLE map or it resolves half the graph against a half-built one.
            record.record_many(&pairs);
        }

        if moved == 0 {
            return (0, reclaimed, cratonvm_types::PointerMap::default());
        }

        // ---- Rewrite every reference slot in every surviving object -------
        //
        // AFTER the whole slide, not during it: a slot in an already-moved
        // object may point at an object that has not moved yet, so rewriting
        // as we go resolves half the graph against a half-built map.
        let live_now: Vec<usize> = live
            .iter()
            .map(|b| record.get(*b).unwrap_or(*b))
            .collect();
        for obj in &live_now {
            let mut rewrites: Vec<(u64, u64)> = Vec::new();
            {
                use census::ZCensusHeapView;
                self.reference_slots(*obj as u64, &mut |slot| {
                    let raw = slot.raw_word as usize;
                    if raw == 0 {
                        return;
                    }
                    if let Some(to) = record.get(raw) {
                        rewrites.push((slot.slot_addr, to as u64));
                    }
                });
            }
            for (slot_addr, to) in rewrites {
                // SAFETY: `slot_addr` is an 8-byte-aligned reference word
                // inside a live object, as reported by `reference_slots`, and
                // the world is stopped.
                unsafe { std::ptr::write(slot_addr as *mut u64, to) };
            }
        }

        // ---- Rebuild the object-start registry ----------------------------
        for (from, to) in &pairs {
            self.registry.remove(*from);
            self.registry.insert(*to);
        }

        self.verify_no_dangling_slots_after_slide(&live_now, arena_lo, arena_hi);

        let pointer_map: cratonvm_types::PointerMap =
            record.into_pointer_map().into_iter().collect();
        (moved, reclaimed, pointer_map)
    }

    /// After a slide: every reference slot in every survivor must point at a
    /// **registered live base**, or the rewrite pass missed it.
    ///
    /// # Why this exists rather than a test
    ///
    /// A missed slot is a dangling pointer, and a dangling pointer surfaces as
    /// a SIGSEGV somewhere else entirely, later, in whichever subsystem
    /// happens to dereference it first. That is the 2026-08-14 netty/hibernate
    /// cluster: 22 crashes across two unrelated applications, ZGC-only,
    /// spanning WebSocket handshaking, HTTP/2, LZMA, HQL queries and
    /// multitenancy — i.e. no subsystem in common except the collector. No
    /// stack trace from such a crash names the collector, so the crash site is
    /// worthless as evidence and the only useful place to look is here, at the
    /// moment the invariant breaks.
    ///
    /// Off unless `CRATONVM_DBG_ZGC_VERIFY_SLIDE` is set: it is O(live slots)
    /// with a registry probe each, which is far too expensive for every cycle
    /// but trivial next to a debugging session.
    ///
    /// Reports and continues rather than panicking. A panic here would abort
    /// inside a stop-the-world with the heap half-described, and the whole
    /// point is to get the *list* — the first offender is rarely the only one,
    /// and the shape of the set is what names the missing rewrite.
    fn verify_no_dangling_slots_after_slide(
        &self,
        live_now: &[usize],
        arena_lo: usize,
        arena_hi: usize,
    ) {
        if !zgc_verify_slide_enabled() {
            return;
        }
        use census::ZCensusHeapView;
        let mut dangling = 0usize;
        let mut reported = 0usize;
        for obj in live_now {
            self.reference_slots(*obj as u64, &mut |slot| {
                let raw = slot.raw_word as usize;
                if raw == 0 || raw < arena_lo || raw >= arena_hi {
                    // Off-arena words are not this collector's to judge: a
                    // reference slot can legitimately hold an address from
                    // another space, and `is_in_heap` refuses them everywhere
                    // else too.
                    return;
                }
                if self.registry.contains(raw) {
                    return;
                }
                dangling += 1;
                if reported < 16 {
                    reported += 1;
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        holder = *obj,
                        holder_class = self.header_ref(*obj as *mut u8).class_id.as_u32(),
                        slot_addr = slot.slot_addr,
                        points_to = raw,
                        "zgc slide verify: a reference slot points at an address that                          is NOT a registered live base — the rewrite pass missed it,                          and this word is a dangling pointer"
                    );
                }
            });
        }
        if dangling > 0 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                dangling,
                survivors = live_now.len(),
                "zgc slide verify: {dangling} dangling reference slot(s) after                  compaction",
            );
        } else {
            tracing::debug!(
                target: "zgc",
                survivors = live_now.len(),
                "zgc slide verify: every reference slot resolves to a live base"
            );
        }
    }

    /// Pin `addr` against relocation for a JNI critical section — see
    /// [`Self::critical_pins`]. Idempotent per call; balanced by
    /// [`Self::unpin_critical`].
    pub fn pin_critical(&self, addr: usize) {
        if addr == 0 {
            return;
        }
        *self.critical_pins.lock().entry(addr).or_insert(0) += 1;
    }

    /// Release one pin taken by [`Self::pin_critical`].
    pub fn unpin_critical(&self, addr: usize) {
        let mut pins = self.critical_pins.lock();
        if let Some(count) = pins.get_mut(&addr) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&addr);
            }
        }
    }

    /// Snapshot of the pinned addresses, for the relocation-set filter.
    fn critical_pin_addrs(&self) -> Vec<usize> {
        let pins = self.critical_pins.lock();
        if pins.is_empty() {
            return Vec::new();
        }
        pins.keys().copied().collect()
    }

    /// Number of live critical pins — diagnostics and tests.
    pub fn critical_pin_count(&self) -> usize {
        self.critical_pins.lock().len()
    }

    /// Free share of the arena in permille — test support for the
    /// fragmentation-gauge fixtures, which have to assert the state they claim
    /// to have built rather than assume it.
    #[cfg(test)]
    fn arena_free_permille_for_test(&self) -> usize {
        let arena = self.arena.lock();
        let capacity = arena.capacity();
        if capacity == 0 {
            return 0;
        }
        arena.remaining().saturating_mul(1000) / capacity
    }

    /// Enable GC event logging (`--verbose:gc`).
    ///
    /// Closes the gap recorded on `VmHeap::enable_gc_logging`'s ZGC arm: that
    /// arm had no toggle to flip and no gated statement to flip it for, so a
    /// user running `--verbose:gc -XX:+UseZGC` was told logging was on and then
    /// got nothing for the entire run. Same name/shape as
    /// `G1Collector::enable_gc_logging` (`g1.rs:6770-6777`) so the `VmHeap` arm
    /// is a plain delegation.
    pub fn enable_gc_logging(&self) {
        self.gc_log_enabled.store(true, Ordering::Relaxed);
    }

    /// Disable GC event logging. Mirrors `g1.rs:6775-6777`.
    pub fn disable_gc_logging(&self) {
        self.gc_log_enabled.store(false, Ordering::Relaxed);
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

    /// Cap on how much the one-shot fragmentation report walks and prints:
    /// walls visited in the winning window, and class rows emitted. A report
    /// that scrolls the log away at the moment the log is least readable is
    /// the failure mode the one-shot guard exists to avoid.
    const ZGC_FRAG_REPORT_WALLS: usize = 32;

    /// One-shot report of an arena allocation failure, with the occupancy that
    /// says whether the heap was full or merely fragmented.
    ///
    /// One-shot on purpose: the failure repeats for every subsequent request
    /// once the arena is out, and a per-failure line would bury the run in
    /// stderr exactly when it is least readable.
    fn warn_alloc_failed_once(
        size: usize,
        used: usize,
        capacity: usize,
        free_list_bytes: usize,
        largest_free_block: usize,
        // (spans, distinct sizes) -- the SHAPE of the fragmentation. Without it
        // "1.13 GiB free, biggest hole 65528" leaves open whether that is two
        // dozen holes or twenty thousand, and those want different fixes.
        span_shape: (usize, usize),
    ) {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if WARNED.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            target: "cratonvm::gc::guard",
            request = size,
            used,
            capacity,
            free_list_bytes,
            largest_free_block,
            free_spans = span_shape.0,
            free_span_sizes = span_shape.1,
            "zgc: arena allocation failed — this heap does not compact, so the \
             bump cursor never rewinds and reclaimed space returns only as \
             free-list holes. `largest_free_block < request` with a large \
             `free_list_bytes` means fragmentation, not exhaustion.",
        );
    }

    /// Gather what is standing between the holes, once, on the allocation
    /// failure that is about to become an `OutOfMemoryError`.
    ///
    /// The one-line guard above says the heap is fragmented rather than full.
    /// That is where every previous investigation of this failure stopped, and
    /// it is one question short: *fragmented by what?* A free list of 3892
    /// half-megabyte spans holding 93% of the heap can mean the spans are
    /// walled by a live/dead mosaic (nothing short of relocation serves the
    /// request) or that a handful of small survivors is holding gigabytes
    /// hostage (a targeted fix does). [`Arena::frag_profile`] separates those,
    /// and this walks the winning window's walls so the occupants can be named.
    ///
    /// Returns `None` on every call after the first: the failure repeats for
    /// every subsequent request once the arena is out, and this report is far
    /// too long to emit per failure.
    ///
    /// Bounded on purpose: at most [`Self::ZGC_FRAG_REPORT_WALLS`] walls are
    /// walked and that many class rows returned.
    fn frag_report_once(&self, request: usize, arena: &Arena) -> Option<ZFragReport> {
        static REPORTED: AtomicBool = AtomicBool::new(false);
        if REPORTED.swap(true, Ordering::Relaxed) {
            return None;
        }
        let profile = arena.frag_profile(request);
        let (high_blocks, high_bytes, high_max) = arena.high_free_shape();
        let mut report = ZFragReport {
            profile,
            high_cursor: arena.high_cursor(),
            high_blocks,
            high_bytes,
            high_max,
            high_reserve_unclaimed: arena.unclaimed_high_reserve(),
            occupants: Vec::new(),
            unregistered_bytes: 0,
        };
        let Some(window) = report.profile.cheapest else {
            return Some(report);
        };
        // Walk the walls inside that window and tally their occupants. Each
        // wall is a run of live objects laid end to end; a byte that is not a
        // registered base is counted rather than skipped, because unregistered
        // bytes below the cursor are themselves a finding — an un-retired TLAB
        // tail is exactly that, and it is invisible to both the allocator and
        // the sweep.
        let base = arena.base_ptr() as usize;
        let blocks = arena.free_blocks_sorted();
        let mut by_class: FxHashMap<u32, (usize, usize)> = FxHashMap::default();
        let mut walls_walked = 0usize;
        for w in blocks.windows(2) {
            let wall_start = w[0].0 + w[0].1;
            let wall_end = w[1].0;
            if wall_end <= wall_start || wall_start < window.start || wall_end > window.end {
                continue;
            }
            walls_walked += 1;
            if walls_walked > Self::ZGC_FRAG_REPORT_WALLS {
                break;
            }
            let mut off = wall_start;
            while off < wall_end {
                let addr = base + off;
                if self.registry.contains(addr) {
                    // SAFETY: the registry holds only live allocation bases.
                    let header = unsafe { &*(addr as *const ObjectHeader) };
                    match Self::alloc_size(header) {
                        Some(size) => {
                            let e = by_class.entry(header.class_id.as_u32()).or_insert((0, 0));
                            e.0 += 1;
                            e.1 += size;
                            off += size.max(8);
                        }
                        None => {
                            // Unsizable header: the sweep refuses these too, so
                            // the rest of this run cannot be strided.
                            report.unregistered_bytes += wall_end - off;
                            break;
                        }
                    }
                } else {
                    report.unregistered_bytes += 8;
                    off += 8;
                }
            }
        }
        let mut rows: Vec<(u32, usize, usize)> =
            by_class.into_iter().map(|(c, (n, b))| (c, n, b)).collect();
        rows.sort_unstable_by_key(|&(c, n, b)| (std::cmp::Reverse(b), std::cmp::Reverse(n), c));
        rows.truncate(Self::ZGC_FRAG_REPORT_WALLS);
        report.occupants = rows;
        Some(report)
    }

    /// Bump-allocate `size` zeroed bytes (8-byte aligned) and register the
    /// base address. Returns `None` on OOM.
    fn alloc_raw(&self, size: usize) -> Option<*mut u8> {
        // Which end of the arena. See `Arena::high_cursor` for the measurement
        // this exists for; the short version is that one long-lived object
        // inside a thread's private TLAB chunk caps every hole in the heap at
        // one chunk, so an object too big for any TLAB must not be allocated
        // among TLAB chunks.
        let large = size >= ZGC_LARGE_OBJECT_MIN;
        let ptr = {
            let mut arena = self.arena.lock();
            // The high end first for a large object, the low end as the
            // fallback: the two ends share one middle, so a high-end refusal
            // does not mean the arena is out — and serving a large object from
            // the small-object end is merely bad for later fragmentation,
            // while failing is an `OutOfMemoryError`.
            let mut got = if large {
                arena.alloc_high(size, 8).or_else(|| arena.alloc(size, 8))
            } else {
                arena.alloc(size, 8)
            };
            if got.is_none() {
                // Before this can be called a failure: every live TLAB is
                // holding a chunk whose unused tail is arena space no allocator
                // can see. On a non-compacting heap that tail is also the piece
                // most likely to be ADJACENT to a free hole (a chunk is carved
                // from one), so handing it back can both supply the bytes and
                // let `Arena::alloc`'s merge re-form a bigger span.
                //
                // Lock order is cell -> arena, never the reverse, so the arena
                // guard has to go first. Dropping it is safe here: the first
                // attempt already failed, and a peer that allocates in the
                // window can only make the retry fail again — which is exactly
                // the pre-existing answer.
                drop(arena);
                self.retire_all_tlabs();
                arena = self.arena.lock();
                got = if large {
                    arena.alloc_high(size, 8).or_else(|| arena.alloc(size, 8))
                } else {
                    arena.alloc(size, 8)
                };
            }
            let ptr = match got {
                Some(p) => p,
                None => {
                    // Allocation failure on a NON-COMPACTING heap is not the
                    // same event as "full of live data", and the two want
                    // different fixes. Say which, once, with the numbers that
                    // separate them:
                    //
                    //   used ~ capacity, free list small  -> genuinely full
                    //   used ~ capacity, free list LARGE  -> fragmented: the
                    //                                        bytes are there,
                    //                                        no hole this big
                    //   largest_free_block < size         -> why THIS request
                    //                                        failed
                    //
                    // Without it an `OutOfMemoryError` raised from here is
                    // undiagnosable after the fact, which is what it was when
                    // `ZipContentTests` began failing at the Spring Boot
                    // suite's default `-Xmx 2g` on the day ZGC became the
                    // default collector.
                    let used = arena.used();
                    let capacity = arena.capacity();
                    let free_list_bytes = arena.free_list_bytes();
                    let largest = arena.largest_free_block();
                    let shape = arena.free_span_shape();
                    // ...and, once, WHAT is standing between the holes. The
                    // guard line says "fragmented, not exhausted"; this says
                    // by what, which is the question every previous
                    // investigation of this failure stopped one step short of.
                    // Gathered under the lock, LOGGED without it — see
                    // `ZFragReport`.
                    let frag = self.frag_report_once(size, &arena);
                    drop(arena);
                    Self::warn_alloc_failed_once(
                        size,
                        used,
                        capacity,
                        free_list_bytes,
                        largest,
                        shape,
                    );
                    if let Some(frag) = frag {
                        frag.log(size);
                    }
                    // Ask for a collection at the next safepoint. A native
                    // cannot collect where it stands, but `vm_exec`'s
                    // native-boundary hook acts on this latch — and a request
                    // that just failed is stronger evidence that a cycle is due
                    // than the `allocated >= gc_threshold` predicate, which
                    // counts LIVE bytes and therefore cannot see the bump space
                    // this heap never rewinds.
                    self.native_alloc_pressure.store(true, Ordering::Relaxed);
                    // ...and the HARD latch, which the boundary honours without
                    // re-asking `needs_gc()`. The line above has been here since
                    // the latch existed and was, on its own, inert in exactly
                    // the case it was written for: see the `hard_alloc_failure`
                    // field doc.
                    self.hard_alloc_failure.store(true, Ordering::Relaxed);
                    return None;
                }
            };
            // Arm the ALLOCATABLE-SPACE trigger (see `headroom_low`). Two
            // reasons this is here rather than in `needs_gc`: the arena lock is
            // already held, and `needs_gc` is polled far more often than
            // allocation happens.
            //
            // Cheap by construction. The un-bumped tail is two field reads, and
            // while it is above the margin — which is the whole of a normal run
            // — nothing else is consulted. Only once the tail is genuinely low
            // does the free-list probe run, and `has_free_block_at_least` is
            // O(1) for exactly the "no" answer that matters here.
            //
            // WIDENING THE MARGIN BY THE UNCLAIMED LARGE-OBJECT RESERVE WAS
            // TRIED HERE, MEASURED, AND REMOVED. The reasoning was sound: a
            // TLAB chunk is reserved space, not allocated space, so `allocated`
            // cannot see the arena filling with chunks, and on
            // `TestNonBlockingAPI` (2026-08-13) this was the ONLY trigger that
            // ever fired — at a bare `margin`, when the un-bumped middle was
            // already down to 16 MB. Collecting earlier does keep the reserve
            // intact.
            //
            // It is still not here, because the arm was priced against what it
            // changed. Once `ZGC_TLAB_RESERVATION_SHARE` bounds what chunks may
            // claim at all, the class passes with the bare margin (`OK (44
            // tests)`, zero OOM warnings) — and the widened margin cost **4.6%
            // on `TestTomcat`**, a class with nowhere near enough threads for
            // the chunk to shrink, i.e. a class that paid for the trigger and
            // got nothing back. Collecting earlier is the wrong lever for a
            // reservation that should not have been that large.
            let margin = zgc_headroom_margin(arena.capacity());
            let tail = arena.capacity().saturating_sub(arena.used());
            if tail < margin && !arena.has_free_block_at_least(margin) {
                self.headroom_low.store(true, Ordering::Relaxed);
            }
            // The arena bump path hands out memory from a zeroed Vec, but a
            // reused free-list block may contain stale bytes — zero it so a
            // fresh header/fields start clean.
            // SAFETY: `arena.alloc` guarantees `size` valid bytes at `ptr`.
            unsafe { std::ptr::write_bytes(ptr, 0, size) };
            ptr
        };
        // One `fetch_or` into the object-start bitmap — no lock, no hash, no
        // table that grows with the live set. See the "Object-start membership"
        // section header for the measurement this replaced.
        self.registry.insert(ptr as usize);
        let after = self.allocated.fetch_add(size, Ordering::Relaxed) + size;
        // Arm the native-allocation-pressure latch on the crossing edge. This
        // is the ZGC analogue of G1's `note_region_consumed_locked`
        // (`g1.rs:1765-1774`): "`needs_gc` went true during allocation".
        //
        // `alloc_raw` is the single chokepoint for EVERY object and array on
        // this backend — `try_alloc_object`, `try_alloc_array` and the
        // infallible `GarbageCollector::alloc_object` / `alloc_array` all route
        // here — so one test covers both shapes.
        //
        // The predicate is `needs_gc`'s predicate verbatim (inlined rather than
        // called, to keep this off the trait and out of any import ambiguity;
        // see `<Self as GarbageCollector>::needs_gc`). Reusing it — rather than
        // testing `gc_threshold` alone — is what keeps the latch out of the
        // GC-storm shape the `gc_rearm` field exists to prevent: a live set
        // parked above the static threshold leaves `gc_rearm` above `allocated`
        // after every sweep, so the latch stays DOWN until genuinely new
        // allocation clears the re-arm floor. The latch can therefore never
        // request a collection that `needs_gc` would refuse.
        if after >= self.gc_threshold && after >= self.gc_rearm.load(Ordering::Relaxed) {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
        Some(ptr)
    }

    /// Try to allocate an object. Returns `None` on true heap exhaustion.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let fields_size = num_fields.checked_mul(SLOT_SIZE)?;
        let total = HEADER_SIZE.checked_add(fields_size)?;
        let ptr = self.alloc_raw_tlab(total)?;
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
        let ptr = self.alloc_raw_tlab(total)?;
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
    ///
    /// **TLAB audit.** This deliberately does *not* retire TLABs, and does not
    /// need to: a TLAB-served object is inserted into [`Self::registry`] the
    /// instant it is handed out, inside the owning cell's lock. This predicate
    /// is not GC-only — the JIT calls it on the mutator path (`jit_checkcast`
    /// resolves its receiver through it), and the conservative-root scanner
    /// calls it *before* `collect_garbage` is entered — which is exactly why
    /// `gc::zgc::tlab`'s batched registration could not be adopted here. See
    /// [`ZArenaTlabRegistry`]'s `registry_batch` note.
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        if self.registry.contains(addr) {
            // SAFETY: the registry contains only live allocation bases.
            Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
        } else {
            None
        }
    }

    /// The primitive inside an auto-box wrapper, or `None` when `candidate` is
    /// an ordinary object.
    ///
    /// The read half of the auto-boxing [`Collector::set_array_element`]
    /// performs for a non-`Object` value stored into a reference array. Without
    /// it the caller gets the *wrapper* back — an object of the synthetic
    /// `AUTOBOX_CLASS_ID`, which has no class name, no methods and no
    /// relationship to the primitive it carries. `GenerationalHeap` and
    /// `G1Collector` both un-box on the read side; this collector did not, so
    /// `Stream.mapToLong(...)`/`mapToDouble(...)` — whose Rust bridge collects
    /// `Value::Long`/`Value::Double` into a reference array — handed the
    /// pipeline a wrapper where a primitive belonged, and every consumer
    /// downstream read either zero or the wrapper's own address.
    ///
    /// Validates the address before dereferencing its header: a reference array
    /// element is a raw word and a stale or garbage one could point anywhere,
    /// so an unchecked read here would be wild. Mirrors
    /// `G1Collector::autobox_payload`.
    fn autobox_payload(&self, candidate: ObjectRef) -> Option<Value> {
        self.is_object_address(candidate.as_ptr() as usize)?;
        // SAFETY: `is_object_address` confirmed `candidate` is a registered
        // live allocation base, so its first HEADER_SIZE bytes are a header.
        let header = unsafe { &*(candidate.as_ptr() as *const ObjectHeader) };
        if header.class_id != crate::heap::AUTOBOX_CLASS_ID {
            return None;
        }
        Some(<Self as GarbageCollector>::get_field(self, candidate, 0))
    }

    /// Loose containment check returning the base object for any address inside it.
    ///
    /// **TLAB audit.** No retire here either. The interior-pointer fallback
    /// below iterates [`Self::registry`] and decodes a header at each
    /// *registered base* — it never strides raw arena bytes — so an
    /// un-retired chunk tail is invisible to it rather than fatal. Retiring
    /// from a mutator query would also be actively wrong: it would close every
    /// thread's buffer on a conservative-root probe.
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        // Fast path: an exact object base (the overwhelmingly common probe).
        if self.registry.contains(addr) {
            // SAFETY: the registry contains only live allocation bases.
            return Some(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
        // ---- Two screens BEFORE the extent walk -------------------------
        //
        // Neither changes the answer; both exist because the fallback's cost
        // shape changed when the registry became a bitmap. The `FxHashSet`
        // iteration was O(live); a bitmap walk is O(arena span), so on a
        // mostly-empty multi-gigabyte heap an unbounded walk here would be a
        // REGRESSION (~8.2M word loads for a 4.2 GB arena against a few
        // thousand hash steps). That matters because this is not a GC-only
        // path: `VmHeap::is_heap_addr`'s ZGC arm (`vm_heap.rs:694-700`) feeds
        // it per-slot conservative root scanning over ambiguous
        // JVM-long-vs-jobject operand words, whose dominant population is
        // zeros, small integers and long bit patterns — every one of which
        // misses the exact-base probe above and lands here.
        //
        // Screen 1 — the arena envelope, which `vm_heap.rs`'s own `TODO(zgc)`
        // on that arm asks for by name ("once `ZgcRealHeap` publishes its arena
        // envelope, the range compare belongs here too"). It cannot lose a
        // base: `has_spill()` is false exactly when every base this registry
        // holds was encodable on the arena grid and is therefore inside the
        // envelope, and no in-arena object's extent can reach outside it
        // either. When it is true (the `Hash` kill-switch arm, or a spill) the
        // screen stands down and the behaviour is the pre-change one, byte for
        // byte.
        if !self.registry.has_spill() && (addr < self.arena_base || addr >= self.arena_end) {
            return None;
        }
        // Screen 3 — the one that removes the walk instead of bounding it.
        //
        // Allocations do not overlap, so the only base whose extent can contain
        // `addr` is the greatest base `<= addr`. On the bitmap arm (no spill)
        // that is one backwards bit scan and ONE header dereference — see
        // `ZObjectStartBits::nearest_base_at_or_below` for the `perf record`
        // that made this the top of the profile (80.7% of all CPU samples on
        // `type.temporal.InstantTests`, a class HotSpot finishes in 6.7 s).
        //
        // Screen 2 below bounded the same walk by the arena high-water mark,
        // which helps only while the cursor is low; once a long-running heap
        // has bumped to capacity the bound is the whole bitmap again. This
        // does not depend on occupancy at all.
        if let Some(base) = self.registry.nearest_base_at_or_below(addr) {
            let header = unsafe { &*(base as *const ObjectHeader) };
            // Same refusal as the walk below: an unsizable header cannot be
            // said to CONTAIN anything.
            if let Some(size) = Self::alloc_size(header) {
                if let Some(end) = base.checked_add(size) {
                    if addr >= base && addr < end {
                        // SAFETY: the registry contains only live bases.
                        return Some(unsafe { ObjectRef::from_raw(base as *mut u8) });
                    }
                }
            }
            // The nearest base does not cover `addr`, and no LOWER base can
            // (its extent would have to span across this one). Definitive.
            return None;
        }
        // Screen 2 — bound the walk by the arena's high-water mark. Every base
        // ever handed out sits below `arena_base + used` (the cursor only
        // advances, and free-list blocks are carved from below it), so this
        // restores the O(bytes actually allocated) shape the hash iteration
        // had. One uncontended arena acquire, released immediately, on a path
        // that previously took the registry mutex TWICE.
        //
        // Reached only on the `Hash` kill-switch arm or after a grid spill —
        // neither can order what it holds, so neither can answer "greatest at
        // or below" and both keep the walk.
        let end_hint = self.arena_base.saturating_add(self.arena.lock().used());
        // Interior pointers: fall back to the O(live) extent walk. Same answer
        // as the `FxHashSet` iteration this replaced, and on the bitmap arm it
        // holds no lock at all while the callback dereferences headers.
        let mut hit: Option<usize> = None;
        self.registry.for_each_base(end_hint, &mut |base| {
            let header = unsafe { &*(base as *const ObjectHeader) };
            // An object this collector cannot size cannot be said to CONTAIN
            // `addr` either — claiming a 1 TiB extent for it would swallow
            // every higher arena address and map unrelated conservative-root
            // words onto the wrong `ObjectRef`. Its own base is still answered
            // by the exact-base fast path above.
            let Some(size) = Self::alloc_size(header) else {
                return true;
            };
            let Some(end) = base.checked_add(size) else {
                return true;
            };
            if addr >= base && addr < end {
                hit = Some(base);
                return false;
            }
            true
        });
        // SAFETY: the registry contains only live allocation bases.
        hit.map(|base| unsafe { ObjectRef::from_raw(base as *mut u8) })
    }

    /// Total backing arena capacity.
    pub fn heap_capacity(&self) -> usize {
        self.arena.lock().capacity()
    }

    /// Walk every live object and its allocation size.
    ///
    /// Retires every TLAB first — this is a heap-walking entry point and the
    /// invariant on [`Self::retire_all_tlabs`] applies to it. Idempotent, so
    /// calling it from inside a collection that has already retired costs a
    /// pass over an empty set.
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        self.retire_all_tlabs();
        self.registry
            .bases()
            .into_iter()
            .filter_map(|base| {
                let header = unsafe { &*(base as *const ObjectHeader) };
                // Drop rather than report a sentinel extent: this feeds heap
                // dumps and referrer queries, and `(ptr, 1 TiB)` is worse for
                // every one of them than an omitted row.
                Self::alloc_size(header).map(|size| (base as *mut u8, size))
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

    /// Shared (`&`) header view — the concurrent twin of [`Self::header_mut`].
    ///
    /// Every mutating operation an [`ObjectHeader`] offers goes through the
    /// atomic mark word, so a `&ObjectHeader` is enough to claim a mark bit.
    /// [`Self::header_mut`] is nonetheless unusable from the concurrent marker:
    /// N workers each minting a `&mut ObjectHeader` for the same object are
    /// aliasing `&mut`s, which is undefined behaviour regardless of what the
    /// fields underneath happen to be — the compiler is entitled to assume the
    /// reference is unique. Every method on [`mark::ZMarkContext`] below uses
    /// this accessor, and none uses `header_mut`.
    #[inline]
    fn header_ref(&self, base: *mut u8) -> &ObjectHeader {
        // SAFETY: `base` is a registered live allocation base (the caller has
        // passed it through the registry gate); its first HEADER_SIZE bytes
        // are a valid `ObjectHeader` written at allocation time, and this
        // reference is shared, so concurrent readers are sound.
        unsafe { &*(base as *const ObjectHeader) }
    }

    /// Largest body this collector will believe. Mirrors the `1 << 24` slot cap
    /// `gen_object_total_size` (`gen_heap.rs`) and `object_body_size`'s own
    /// legacy screen already apply — "no real class has 16M fields" — expressed
    /// in bytes so it also bounds a compact body.
    ///
    /// It exists to reject [`cratonvm_types::object_body_size`]'s corrupt-header
    /// sentinel, which is `1 << 40` (1 TiB). That value is deliberately
    /// "impossibly large" rather than `0` so that a caller deriving an extent
    /// from it fails its own arena bounds check and re-syncs — see the constant's
    /// doc in `types/src/field_layout.rs`. This collector had no such check.
    const MAX_PLAUSIBLE_BODY: usize = (1usize << 24) * SLOT_SIZE;

    /// Total size in bytes of the allocation rooted at `header`, or `None` when
    /// the header cannot be sized.
    ///
    /// # Why this is fallible
    ///
    /// [`cratonvm_types::object_body_size`] answers `IMPLAUSIBLE_BODY_SIZE`
    /// (1 TiB) — not `0` — for two states: a `GC_FLAG_COMPACT` object whose
    /// `(class_id, num_slots)` no longer resolves to a registered layout (a
    /// class unloaded by `ClassStore::remove` -> `unregister_class_layout`
    /// while an instance is still in this heap's registry), and a legacy header
    /// whose `num_slots` is past the plausibility cap (a desynced walk, a
    /// dangling base). The sentinel is a REFUSAL, and every sibling collector
    /// treats it as one: `gen_heap`'s non-moving sweep stops the walk on
    /// `cursor + total > used`, `g1` refuses to evacuate a sub-header object,
    /// and `gen_object_total_size` returns `0` rather than passing the sentinel
    /// on.
    ///
    /// This function used to return it verbatim, and
    /// [`Self::collect_garbage`]'s sweep fed it straight to
    /// `std::ptr::write_bytes(base, 0, size)` and `Arena::add_free_block`
    /// (whose only bound is a `debug_assert!`, compiled out in release). One
    /// unresolvable compact object therefore memset a terabyte from its base
    /// and handed the allocator a free block far outside the arena. Five other
    /// sites in this same file already guard the identical "compact-flagged,
    /// layout does not resolve" state (`get_field`, `set_field`,
    /// `visit_strong_refs_at`, `census::reference_slots`,
    /// `effectively_compact_header`); the one that decides how many bytes to
    /// ZERO was the only consumer without a screen.
    ///
    /// The array arm is fallible for the matching reason: `array_data_size`
    /// answers `None` on an overflowing length, and the previous
    /// `.unwrap_or(0)` under-reported the extent — the direction that leaves
    /// live bytes on the free list.
    fn alloc_size(header: &ObjectHeader) -> Option<usize> {
        match header.kind() {
            ObjectKind::Object | ObjectKind::HumongousFiller => {
                let body = cratonvm_types::object_body_size(header);
                if body > Self::MAX_PLAUSIBLE_BODY {
                    return None;
                }
                HEADER_SIZE.checked_add(body)
            }
            ObjectKind::Array => {
                let data =
                    array_data_size(header.array_length() as usize, header.element_type()).ok()?;
                ARRAY_DATA_OFFSET.checked_add(data)
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
        match header.kind() {
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
                                // SAFETY: `offset` comes from this object's own
                                // registered layout — the layout its body was
                                // sized with at allocation — so the field lies
                                // inside the allocation, and `read_ref_slot`
                                // reads exactly `ref_field_size()` bytes of it.
                                //
                                // WHY NOT `std::ptr::read(slot as *const u64)`:
                                // that is what stood here, and it was wrong
                                // under compressed oops. `ref_field_size()` is
                                // 4 when narrow oops are on, so an
                                // unconditional 8-byte load reads one field
                                // plus half of the next and calls the result a
                                // pointer. Every compact reference field then
                                // traces to garbage — silently refused by the
                                // registry/`is_in_heap` screen, so nothing
                                // looks wrong — and the real referent is never
                                // traced at all: a live object collected.
                                //
                                // It was LATENT, never live, and only because
                                // of a gate in another crate:
                                // `vm/src/vm/vm_init.rs` (the
                                // `gc_backend != GcBackend::Generational`
                                // branch, ~line 1397) refuses compressed oops
                                // for every backend except `Generational`, so
                                // ZGC has never seen a narrow slot. THAT GATE
                                // MUST NOT BE RELAXED PER-BACKEND WITHOUT
                                // AUDITING EVERY REFERENCE-SLOT READ IN THE
                                // COLLECTOR BEING ENABLED. `gc/src/g1.rs` was
                                // audited on 2026-08-07 and still carries the
                                // same wide-read pattern, held off by the same
                                // gate; `gen_heap.rs`, the one collector the
                                // gate admits, is clean.
                                let slot = unsafe { base.add(HEADER_SIZE + offset as usize) };
                                let raw =
                                    unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                                if raw != 0 {
                                    work.push(raw as usize);
                                }
                            }
                        },
                    );
                } else {
                    // The legacy arm is NOT narrow-oop territory and must not be
                    // "fixed" to match the compact arm above. A legacy field is a
                    // whole 16-byte `Value` cell (`SLOT_SIZE`, heap_types.rs:171)
                    // whose object payload is an 8-byte raw pointer at
                    // `FIELD_CELL_PAYLOAD64_OFFSET` (heap_types.rs:230). Both are
                    // plain constants: neither narrows when compressed oops are
                    // on, so the widths here are already right in both configs.
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
                if header.element_type() == ArrayElementType::Reference {
                    let len = header.array_length() as usize;
                    // This arm was ALREADY narrow-correct and is deliberately
                    // left calling `read_prim_element`: its `Reference` case
                    // (gc/src/heap.rs, `ArrayElementType::Reference`) strides by
                    // `cratonvm_types::ref_element_size()` and loads through
                    // `narrow_oop::read_ref_slot`, exactly like the two sibling
                    // walkers in this file. The SAFETY note here used to cite
                    // `REF_ELEMENT_SIZE` (heap_types.rs:176) as the stride; that
                    // is the WIDE 8-byte constant and is the wrong stride under
                    // narrow oops, so do not reintroduce it — the code was
                    // right, only the comment was.
                    //
                    // SAFETY: data area begins at base + ARRAY_DATA_OFFSET; each
                    // ref element is `ref_element_size()` bytes and `i < len`.
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
        header.gc_flags() & GC_FLAG_MARKED != 0
    }

    /// This heap's [`census::ZSlotCensus`] — the reference-slot instrument.
    ///
    /// The handle a driver uses to arm the diagnostic
    /// (`heap.slot_census().enable()`), label the run, and read the result back
    /// after a collection. It is disabled until something calls `enable()`, so
    /// exposing it costs a build nothing.
    pub fn slot_census(&self) -> &census::ZSlotCensus {
        &self.slot_census
    }

    /// Whether the VM's own read path will take its **compact** arm for this
    /// object — the census's "effectively compact" predicate.
    ///
    /// Why this is not `is_compact_object(header)`: `compact_object_field_storage`
    /// (`types/src/field_layout.rs:953-965`) refuses in *two* steps —
    /// `is_compact_object`, and then `class_layout_for_fields(class_id,
    /// num_slots)?`. An object carrying [`cratonvm_types::GC_FLAG_COMPACT`] whose
    /// class has no layout registered for its exact field count (a redefine that
    /// changed the count, a foreign layout domain) falls through that `?`, and
    /// [`Self::get_field`] then reads it as a **16-byte tagged cell**. For an
    /// instrument whose entire purpose is barrier cost, what the read path does
    /// is the truth and the header bit is not.
    ///
    /// `with_class_layout(.., |_| ())`, not `class_layout_for_fields(..)`: the
    /// two resolve through the same `registry_layout_for_fields`, and the
    /// borrowing form does not pay the `Arc` clone/drop for a handle that is
    /// discarded immediately — the same reason `enumerate_references` uses it.
    fn effectively_compact_header(header: &ObjectHeader) -> bool {
        cratonvm_types::is_compact_object(header)
            && cratonvm_types::with_class_layout(
                header.class_id.as_u32(),
                header.num_slots(),
                |_layout| (),
            )
            .is_some()
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

    // -- Concurrent-mark seam ---------------------------------------------
    //
    // The three methods below exist only to serve the
    // [`mark::ZMarkContext`] implementation further down this file. Nothing
    // in `collect_garbage` calls them and nothing in `collect_garbage`
    // changed to accommodate them; see that impl's header for why adopting
    // the concurrent engine is a separate step.

    /// Open a concurrent mark cycle: snapshot the weak-referent skip set and
    /// publish it for [`mark::ZMarkContext::visit_refs`].
    ///
    /// Returns the snapshot so a caller can log or assert on its size.
    ///
    /// # When to call it
    ///
    /// In the mark-start safepoint, **after** every survivor's stale
    /// [`GC_FLAG_MARKED`] has been cleared and **before** any worker thread is
    /// started. Both halves of that ordering are load-bearing:
    ///
    /// * Clearing after a worker has started loses that worker's claim — the
    ///   object reads as unmarked again and can be pushed a second time, or
    ///   worse, the clear lands between another worker's winning
    ///   [`mark::ZMarkContext::try_mark`] and its scan, so the object is
    ///   scanned once and then re-claimed by a third worker as if it were
    ///   fresh. Exactly-once is a property of the whole cycle, not of one CAS.
    /// * Snapshotting after workers start means some edges were traced under
    ///   an empty skip set — see the `mark_ref_skip` field docs for why a
    ///   live view makes referent immortality depend on thread timing.
    ///
    /// This method deliberately does **not** clear the mark bits itself. The
    /// clear is a full walk of the registry that must happen inside the
    /// safepoint, and folding it in here would hide a stop-the-world pass
    /// behind a name that reads like bookkeeping.
    pub fn begin_concurrent_mark_cycle(&self) -> std::sync::Arc<FxHashSet<usize>> {
        // Take the `ref_processor` guard, build the set, and DROP the guard
        // before touching `mark_ref_skip`. Holding two collector locks at once
        // is how a lock cycle is built; there is no reason to here.
        let snapshot: FxHashSet<usize> = {
            let rp = self.ref_processor.lock();
            rp.reference_object_addresses().into_iter().collect()
        };
        let shared = std::sync::Arc::new(snapshot);
        *self.mark_ref_skip.write() = Some(std::sync::Arc::clone(&shared));
        shared
    }

    /// Close the concurrent mark cycle opened by
    /// [`Self::begin_concurrent_mark_cycle`], dropping the skip-set snapshot.
    ///
    /// Call it after the last worker has joined and after the non-strong
    /// reference phase has run — the reference phase asks
    /// [`mark::ZMarkContext::is_marked`] about referents, which does not
    /// consult the skip set, but a `keep_alive` re-drain does re-enter
    /// `visit_refs` and must still see the cycle's snapshot rather than an
    /// empty set.
    pub fn end_concurrent_mark_cycle(&self) {
        *self.mark_ref_skip.write() = None;
    }

    /// The current cycle's skip-set snapshot, or `None` outside a cycle.
    ///
    /// # What `visit_refs` does when this is `None`
    ///
    /// It reports **every** reference slot, including a `Reference`'s
    /// referent, and logs one warning per heap. That is the wrong answer, and
    /// it is the wrong answer in the survivable direction: an over-reported
    /// edge makes a weak referent immortal (a leak, and `Cleaner` stops
    /// firing), while an under-reported edge drops a live object's out-edges
    /// and is a use-after-free. Faced with a caller that skipped
    /// [`Self::begin_concurrent_mark_cycle`], leak.
    ///
    /// The alternative — take the snapshot lazily on the first `visit_refs` —
    /// was rejected twice over: it would block a mark worker on
    /// `ref_processor`'s mutex, which
    /// [`mark::ZMarkContext`]'s threading contract forbids (a blocking
    /// implementation turns `pause_for_safepoint` into a hang), and a snapshot
    /// taken at the first *visit* is not a snapshot taken at mark *start*.
    pub fn concurrent_mark_skip_set(&self) -> Option<std::sync::Arc<FxHashSet<usize>>> {
        // Clone the handle under the read lock and return; the guard is gone
        // by the time the caller walks anything.
        self.mark_ref_skip.read().as_ref().map(std::sync::Arc::clone)
    }

    /// Report every **strong** reference out-edge of the object at `base`.
    ///
    /// # This is a FORK of [`Self::enumerate_references`], and why
    ///
    /// It is not a wrapper for two reasons, one of shape and one historical:
    ///
    /// * **Shape.** `enumerate_references` pushes into a `&mut Vec<usize>`;
    ///   [`mark::ZMarkContext::visit_refs`] hands each child to a
    ///   `&mut dyn FnMut(u64)` so the engine can push into the *striped*
    ///   work-stealing stack it owns. Funnelling through a `Vec` first would
    ///   allocate per object on every mark worker. This reason still stands.
    /// * **Defect (since FIXED at the source).** `enumerate_references`'s
    ///   compact arm used to read its field with
    ///   `std::ptr::read(slot as *const u64)`, an unconditional 8-byte load.
    ///   The real read path (`read_compact_field`,
    ///   `types/src/field_layout.rs:978-993`) goes through
    ///   [`cratonvm_types::narrow_oop::read_ref_slot`], which is a 4-byte
    ///   narrow load plus base+shift when compressed oops are on. Under narrow
    ///   oops the raw 8-byte load reads one field and half of the next and
    ///   calls the result a pointer: every compact reference field is then
    ///   traced to garbage (refused by `is_in_heap`, so it looks like nothing
    ///   is wrong) and the real referent is never traced at all — a live
    ///   object collected, silently, only under compressed oops. It was latent
    ///   rather than live only because `vm/src/vm/vm_init.rs` refuses
    ///   compressed oops for any backend but `Generational`.
    ///   `enumerate_references` now uses the same
    ///   `narrow_oop::read_ref_slot` load this fork does, so the two arms agree
    ///   and this bullet records why the fork exists, not a live divergence.
    ///
    /// The legacy arm also differs deliberately: it reads the cell's `u32` tag
    /// and `u64` payload directly instead of `std::ptr::read`ing a whole
    /// [`Value`]. A concurrent marker races a mutator's field store, and
    /// materialising a `Value` out of a half-written cell can produce an
    /// invalid enum discriminant, which is instant undefined behaviour rather
    /// than a wrong answer. The raw tag/payload read cannot be poisoned that
    /// way — a torn cell yields an implausible address, which
    /// [`mark::ZMarkContext::is_in_heap`] then refuses like any other wild
    /// child. This is the same reason the census reads slots raw.
    ///
    /// `skip_index` is the referent-slot hole: `Some(0)` for a registered
    /// `Weak`/`Soft`/`Phantom`/`Cleaner` `Reference` object, exactly as
    /// `collect_garbage`'s `skip_for` computes it.
    fn visit_strong_refs_at(
        &self,
        base: *mut u8,
        skip_index: Option<usize>,
        f: &mut dyn FnMut(u64),
    ) {
        let header = self.header_ref(base);
        match header.kind() {
            ObjectKind::Object => {
                let class_id = header.class_id.as_u32();
                let num_slots = header.num_slots();
                // The RAW header bit, following the precedent set by this
                // file's census impl: the bit decides the object's BODY
                // LAYOUT (`alloc_object` sizes a flagged object with
                // `layout.body_size`, not `num_slots * SLOT_SIZE`), while a
                // successful layout lookup only decides how that body is
                // *interpreted*. Deciding with `effectively_compact_header`
                // would send a flagged object whose layout cannot be resolved
                // down the legacy arm, which strides `num_slots` 16-byte cells
                // through a compact-sized body and reads past the allocation.
                if cratonvm_types::is_compact_object(header) {
                    let laid_out = cratonvm_types::with_class_layout(
                        class_id,
                        num_slots,
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
                                // SAFETY: `offset` comes from this object's own
                                // registered layout — the layout its body was
                                // sized with at allocation — so the field lies
                                // inside the allocation, and `read_ref_slot`
                                // reads exactly `ref_field_size()` bytes of it.
                                let slot = unsafe { base.add(HEADER_SIZE + offset as usize) };
                                let raw =
                                    unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                                if raw != 0 {
                                    f(raw);
                                }
                            }
                        },
                    )
                    .is_some();
                    if !laid_out {
                        // Compact-flagged with no layout for
                        // `(class_id, num_slots)`: its cells cannot be walked
                        // without reading out of bounds, so report NO edges and
                        // say so. Under-reporting edges is the dangerous
                        // direction, which is exactly why this is a loud
                        // warning and not a silent `return`.
                        tracing::warn!(
                            target: "zgc",
                            class_id,
                            num_slots,
                            "zgc concurrent mark: compact-flagged object has no registered \
                             layout; its out-edges cannot be enumerated and are NOT traced"
                        );
                    }
                } else {
                    let n = num_slots as usize;
                    // The same "no real class has 1<<24 fields" screen
                    // `check_field_index` applies: a walk that decoded payload
                    // bytes as a header would otherwise stride millions of
                    // cells straight out of the arena.
                    if n > (1 << 24) {
                        tracing::debug!(
                            target: "zgc",
                            num_slots = n,
                            "zgc concurrent mark: suspect header, out-edges omitted"
                        );
                        return;
                    }
                    for i in 0..n {
                        if skip_index == Some(i) {
                            continue;
                        }
                        // SAFETY: a non-compact object's body is exactly
                        // `num_slots * SLOT_SIZE` bytes (`alloc_object`'s
                        // `fields_size` fallback), so cell `i` and both words
                        // read out of it lie inside the allocation. Both reads
                        // are naturally aligned because `SLOT_SIZE` is 16 and
                        // `HEADER_SIZE` is 8-byte aligned.
                        let cell = unsafe { base.add(HEADER_SIZE + i * SLOT_SIZE) };
                        let tag = unsafe {
                            std::ptr::read(
                                cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32
                            )
                        };
                        if tag != cratonvm_types::VTAG_OBJECT as u32 {
                            continue;
                        }
                        let raw = unsafe {
                            std::ptr::read(
                                cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET)
                                    as *const u64,
                            )
                        };
                        if raw != 0 {
                            f(raw);
                        }
                    }
                }
            }
            ObjectKind::Array => {
                if header.element_type() == ArrayElementType::Reference {
                    let len = header.array_length() as usize;
                    // `ref_element_size()`, not the wide `REF_ELEMENT_SIZE`
                    // constant: the stride is 4 under narrow oops. Same reason
                    // the compact arm above avoids the raw 8-byte load.
                    let stride = cratonvm_types::ref_element_size();
                    // SAFETY: the data area begins at `base + ARRAY_DATA_OFFSET`
                    // and holds `array_length()` elements of `stride` bytes;
                    // `i < len`.
                    let data = unsafe { base.add(ARRAY_DATA_OFFSET) };
                    for i in 0..len {
                        let slot = unsafe { data.add(i * stride) };
                        let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                        if raw != 0 {
                            f(raw);
                        }
                    }
                }
                // Primitive arrays have no out-edges.
            }
            ObjectKind::HumongousFiller => {}
        }
    }
}

impl Default for ZgcRealHeap {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Thread-local allocation buffers for `ZgcRealHeap`
// ---------------------------------------------------------------------------
//
// This section adopts `gc::zgc::tlab`'s contract onto `ZgcRealHeap`. Read the
// header on [`ZArenaTlabRegistry`] first: it states which parts of that module
// are reused verbatim, which one part could not be, and why.

use std::sync::Arc;

use crate::tlab::Tlab;
use crate::zgc::tlab::{current_thread_key, ZTlabConfig, ZTlabHeapHooks, ZTlabStats};
use crate::zgc::vaddr::ZColor;

/// Alignment of every TLAB-served allocation.
///
/// Fixed at 8 rather than taken from the caller: every production allocation
/// path on this heap already asks [`Arena::alloc`] for 8, and
/// [`Tlab::reserved_tail`]'s own note records that a cursor which is not
/// 8-aligned is silently dropped by the consumer of the tail — which turns the
/// tripwire into a no-op instead of a warning. Pinning the alignment here means
/// the cursor can never leave the object grid in the first place.
const ZGC_TLAB_ALIGN: usize = 8;

/// Ceiling on a TLAB chunk carved from the arena.
///
/// Two forces pull on this number in opposite directions.
///
/// DOWN: every byte sitting in a live chunk but not yet handed to an object is
/// a byte `ZgcRealHeap::allocated` does not know about (see
/// [`ZgcRealHeap::alloc_tlab`] on accounting). That blind spot is bounded by
/// `live_threads * chunk`, so the chunk is the knob that bounds it. This was
/// 64 KiB for that reason alone — deliberately below
/// `crate::tlab::default_tlab_size()` (256 KiB), which is sized for the
/// generational young arena and its disjoint accounting.
///
/// UP, and this is the force that was missing: **on a non-compacting heap the
/// chunk size sets the GRANULARITY OF FREE-LIST HOLES.** A chunk is one
/// `arena.alloc`; the objects inside it are freed individually by the sweep and
/// merged back by the coalescer, but a single survivor anywhere in a chunk
/// walls it off from its neighbours. The steady state is therefore a free list
/// whose largest block is about one chunk — and an allocation LARGER than a
/// chunk can then never be served again, no matter how much of the heap is
/// free.
///
/// 64 KiB was the worst possible value for that, because a 64 KiB hole is
/// exactly too small for one of the most common large Java allocations there
/// is: `new T[8192]` is `8192 * 8 + 16 = 65552` bytes. Measured on the
/// 2026-08-11 Azure Linux Hibernate run, `sql.exec.SmokeTests` died with
/// `OutOfMemoryError` on precisely that array — ANTLR's
/// `ParserATNSimulator.computeTargetState` allocating `DFAState[8192]` — with
/// the guard line reading `free_list_bytes=1211993376 largest_free_block=65528`:
/// 1.13 GiB free, short by 24 bytes. The same class passes on the same heap
/// with `CRATONVM_ZGC_TLAB=0` (no chunks, no walls) and on G1 (which
/// evacuates).
///
/// 512 KiB keeps the hole granularity an order of magnitude above the array
/// shapes real workloads repeat, and raises `max_tlab_alloc` (`chunk / 8`) from
/// 8 KiB to 64 KiB so mid-sized arrays stop needing an arena hole at all.
///
/// **Do not reach for this knob again.** Raising it was the answer to the
/// 64 KiB failure and it did not survive the next one: on 2026-08-13 a 2 MB
/// `char[]` hit the same wall at 512 KiB. Two changes have since removed the
/// coupling this constant was being used to paper over — allocations no TLAB
/// can serve are placed at the arena's other end (`ZGC_LARGE_OBJECT_MIN`,
/// `Arena::high_cursor`), and a refill now accepts a shorter RECYCLED chunk so
/// retired chunks stop being one survivor short of reusable
/// (`ZgcRealHeap::tlab_refill`). What is left for this number to decide is the
/// accounting blind spot named above, and that is what it should be derived
/// from. The
/// accounting blind spot it costs is `live_threads * 512 KiB` — a few MiB on
/// the thread counts this VM runs, against a heap sized in gigabytes — and the
/// unused tail is returned to the free list at every retire
/// ([`ZgcRealHeap::tlab_retire_locked`]), so it is a reservation, not waste.
///
/// Small heaps are unaffected: [`ZArenaTlabRegistry::chunk_bytes_for_capacity`]
/// takes `capacity / 1024` first, so this ceiling only binds above ~512 MiB,
/// which is exactly where the fragmentation it exists to prevent appears.
const ZGC_TLAB_MAX_CHUNK: usize = 512 * 1024;

/// Size at or above which an allocation is served from the arena's HIGH end
/// (`Arena::alloc_high`) instead of being bump-allocated among TLAB chunks.
///
/// 64 KiB, and the number is derived rather than picked: it is
/// `ZGC_TLAB_MAX_CHUNK / 8`, which is exactly `ZTlabConfig::max_tlab_alloc` —
/// the size above which no TLAB will ever serve an object. So the rule is not
/// "big objects go up there", it is **"an object that can only come from the
/// shared arena must not be placed among the thread-private chunks"**, and the
/// threshold is the definition of that set rather than a tuning knob.
///
/// # Why the two populations must not share space
///
/// This heap does not compact, so the largest servable request is the largest
/// gap between two survivors. Measured on `TestNonBlockingAPI` under ZGC
/// (2026-08-13, `-Xmx 2g`): a 2,101,264-byte `char[]` raised
/// `OutOfMemoryError` with **1.99 GB of the 2 GB heap free**, because the
/// largest hole was 524,192 bytes — one `ZGC_TLAB_MAX_CHUNK` minus 96. The
/// fragmentation report named the walls exactly: **544 live bytes in four
/// runs — four `AbstractQueuedSynchronizer$ConditionNode`s and two
/// `ExclusiveNode`s — standing inside 2,621,264 bytes of otherwise contiguous
/// arena**. A thread that parks leaves one small, long-lived AQS node inside
/// its own 512 KiB private chunk; one survivor per chunk caps every hole in
/// the heap at one chunk, for the rest of the process.
///
/// Raising the chunk size only relocates that ceiling — it had already been
/// raised once (64 KiB -> 512 KiB) when a 65,552-byte `DFAState[8192]` hit the
/// same wall in Hibernate's `sql.exec.SmokeTests`. Splitting the populations
/// removes it: at the high end a large object's only possible neighbours are
/// other large objects, which are rarer by orders of magnitude, so the holes
/// they leave stay large.
const ZGC_LARGE_OBJECT_MIN: usize = ZGC_TLAB_MAX_CHUNK / 8;


/// The size of a RECYCLED TLAB chunk worth taking, given the full chunk size
/// `want`, the request `need` that forced this refill, and the largest block on
/// the low end's free list.
///
/// `None` means "ask for a full chunk" — either the free list has nothing worth
/// having, or it already has a full-size block, in which case the ordinary
/// `alloc(want)` will find it and there is nothing to decide.
///
/// Split out of [`ZgcRealHeap::tlab_refill`] so the decision can be tested
/// against the exact numbers that produced the defect, which is not reachable
/// through the heap's public API: it takes ~4,000 threads and a full 2 GB arena
/// to reproduce the shape, and the shape is one comparison.
///
/// The floor is `want / 8`, which is `ZTlabConfig::max_tlab_alloc` — the bound
/// that decides what a TLAB will serve at all. A chunk at the floor therefore
/// still holds at least eight of the largest object it can ever be asked for.
/// Below that a buffer is churning rather than buffering.
#[inline]
fn recycled_chunk_size(want: usize, need: usize, largest_low_free: usize) -> Option<usize> {
    let size = largest_low_free & !(ZGC_TLAB_ALIGN - 1);
    let floor = (want / 8).max(need);
    (size >= floor && size < want).then_some(size)
}

/// Share of the arena that TLAB chunks may hold in RESERVATION at one time.
///
/// A chunk is not allocated memory, it is *claimed* memory: the part of it not
/// yet handed to an object belongs to no object and to no free list, and no
/// collection can reclaim it while its owning thread is alive. The chunk size
/// therefore has to be a function of how many threads are claiming one, and
/// until this constant existed it was not — it was `capacity / 1024` clamped to
/// [`ZGC_TLAB_MAX_CHUNK`], i.e. a flat 512 KiB on any heap above 512 MiB
/// however many threads the workload ran.
///
/// Measured on Tomcat's `TestNonBlockingAPI` (2026-08-13, `-Xmx 2g`): the class
/// runs ~4,000 threads, and `4,000 x 512 KiB` is **2 GB — the entire heap**.
/// The arena filled with chunk reservations, the un-bumped middle closed, and
/// a 2 MB `char[]` had nowhere left to go while 1.99 GB sat on the free list.
/// No collection could help: the chunks were legitimately claimed by live
/// threads.
///
/// `capacity / 16` (134 MiB at `-Xmx 2g`) divided by the live buffer count is
/// the budget each thread gets, clamped into
/// `[min_tlab_size(), ZGC_TLAB_MAX_CHUNK]`. The clamp is what keeps ordinary
/// workloads unchanged: below 256 threads the per-thread share still exceeds
/// 512 KiB at `-Xmx 2g`, so the chunk is the same 512 KiB it was.
const ZGC_TLAB_RESERVATION_SHARE: usize = 16;

/// Workers the marking cycle uses when `CRATONVM_ZGC_PARMARK` is unset.
///
/// **Zero — the bespoke serial loop — and that is a measurement.**
///
/// Phase 3's exit criterion is "pause time falls measurably on a heap with a
/// large live set". Measured on `probes/BigLive.java` (1,000,088 live objects,
/// `-Xmx1500m`, relocation off, three interleaved reps, mean of the last five
/// cycles each):
///
/// | workers | mean pause | vs serial |
/// |---:|---:|---:|
/// | 0 — this hand-written loop | **95.2 ms** | — |
/// | 1 — driven, one worker | 124.9 ms | +31% |
/// | 4 — driven | 240.6 ms | +153% |
/// | 8 — driven | ~304 ms | +219% |
///
/// The criterion is therefore **measured and failed**, in both directions:
/// adding workers makes the pause worse monotonically (contention, not
/// start-up cost — a fixed overhead would flatten and then improve), and even
/// a single-worker driven cycle costs a third of the pause over the loop it
/// replaces.
///
/// **A note on how nearly this shipped wrong.** A single un-interleaved run of
/// the one-worker arm measured 95.7 ms, i.e. free, and a default of `1` was
/// briefly justified on it. Three interleaved reps put it at 124.9 ms with a
/// spread of 2.7 ms — the original number was machine noise on a box that had
/// just finished a build. Interleave, or do not compare.
///
/// The driver is not deleted and Phase 3's other clause still holds: it is
/// reachable, correct, and exercised end-to-end through `collect_garbage` by
/// `the_concurrent_mark_driver_drives_a_real_collection`. It is simply not
/// what a user's pauses pay for until C5 of the concurrent+generational plan
/// makes the marker scale.
const Z_PARMARK_DEFAULT_WORKERS: usize = 0;

/// `CRATONVM_DBG_ZGC_VERIFY_SLIDE` — walk every survivor's reference slots
/// after a compaction and report any that do not resolve to a registered live
/// base. See [`ZgcRealHeap::verify_no_dangling_slots_after_slide`].
fn zgc_verify_slide_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ZGC_VERIFY_SLIDE").is_some()
    })
}

/// Below this share of capacity free, a small largest-block means the heap is
/// **full**, not fragmented — so the fragmentation gauge does not sample.
///
/// This condition is the difference between an instrument and a number. The
/// quantity Phase 2.2 asks to track is `largest_free_block / capacity`, and on
/// its own that quantity falls to nearly zero in two completely different
/// states: an arena broken into crumbs (which is the problem) and an arena
/// genuinely full of live objects (which is not — it is what a heap is for).
/// A gauge that cannot separate them would fire on every workload that uses
/// its heap, get muted, and then be worth nothing on the day it was right.
/// 250 permille — a quarter of the heap free — is where "there are plenty of
/// bytes, they are just not contiguous" becomes the only reading available.
const ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE: usize = 250;

/// The ratchet's floor: one warning per process when the largest block a
/// sampled collection could hand out falls below 1% of capacity.
///
/// Sized against what it is protecting rather than picked: `ZGC_TLAB_MAX_CHUNK`
/// is 512 KiB, so on any heap up to 50 MiB a 1% largest block cannot serve even
/// one full TLAB chunk, and above that it cannot serve the large-object end's
/// first request. It is a floor, not a target — a healthy run does not
/// approach it, and the number to watch is the reported worst, not this.
const ZGC_FRAG_FLOOR_PERMILLE: usize = 10;

/// Hard ceiling on parallel-mark workers, whatever `CRATONVM_ZGC_PARMARK` and
/// the core count say.
///
/// Bounds a thread count, and the thing it bounds is a user-supplied integer —
/// i.e. unbounded — which is the shape the Phase 2.3 constant audit exists to
/// catch. 64 is far above any plausible collection-time parallelism and far
/// below a number that would exhaust the OS thread limit inside a safepoint.
const Z_PARMARK_MAX_WORKERS: usize = 64;

/// Restart budget for a stop-the-world parallel mark. See
/// [`ZgcRealHeap::mark_parallel_stw`] for why this is 1 and not 0.
const Z_PARMARK_RESTART_BUDGET: usize = 1;

/// A fragmentation reading, taken post-sweep — the "steady state" of Phase 2.2.
///
/// `worst_permille` is `largest_free_block * 1000 / capacity` at its lowest
/// across every SAMPLED collection (see [`ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE`]
/// for which collections those are), and `free_permille` is how much of the
/// heap was free at that same moment — the two have to be read together or
/// neither means anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZFragGauge {
    /// Collections that met the sampling condition.
    pub samples: usize,
    /// Worst (lowest) `largest_free_block / capacity`, in permille. `None`
    /// when nothing was sampled — which is the normal state for a short or
    /// heap-light run and must not be reported as a perfect score.
    pub worst_permille: Option<usize>,
    /// Free share of capacity at the worst sample, in permille.
    pub free_permille: usize,
    /// The collection number the worst sample came from.
    pub worst_cycle: usize,
}


/// Runtime kill switch: `CRATONVM_ZGC_TLAB`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) disable the TLAB fast path;
/// anything else — including an unset variable — enables it. Read through
/// [`cratonvm_types::flags::runtime_var_os`] so it participates in the same
/// `-XX:` layering every other flag does, but deliberately **not** declared as
/// a [`cratonvm_types::GcFlags`] field: a declared flag latches on first read,
/// which makes a mid-run `set_var` invisible, and the whole point of this
/// switch is that a suite can A/B it. (Declaring it would also mean editing
/// `types/`, which this change does not touch.)
///
/// Read once per heap, in [`ZgcRealHeap::with_capacity`], into
/// [`ZgcRealHeap::tlab_enabled`].
fn zgc_tlab_enabled_by_default() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_TLAB") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// The reserved footprint of a `bytes` request at [`ZGC_TLAB_ALIGN`].
///
/// Mirrors [`Tlab::alloc_initialized`]'s own `footprint` computation, so the
/// bytes charged to [`ZgcRealHeap::allocated`] are the bytes the buffer
/// actually consumed. (There is no *leading* padding to account for: the
/// cursor never leaves the 8-byte grid because every request uses
/// [`ZGC_TLAB_ALIGN`].)
#[inline]
fn zgc_tlab_footprint(bytes: usize) -> usize {
    bytes.saturating_add(ZGC_TLAB_ALIGN - 1) & !(ZGC_TLAB_ALIGN - 1)
}

/// What one [`ZgcRealHeap::retire_all_tlabs`] did.
///
/// A distinct type rather than `zgc::tlab::ZTlabRetireSummary` for one honest
/// reason: that type's `waste_bytes` is documented as "tail bytes made
/// parseable by a filler" — i.e. *abandoned*. On this arena-backed adapter the
/// tail is handed straight back to the arena free list, so it is recovered,
/// not wasted, and reporting it under a field named `waste_bytes` would be a
/// lie a capacity investigation could act on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZArenaTlabRetireSummary {
    /// Buffers visited.
    pub tlabs: usize,
    /// Buffers that actually held a chunk (the rest were already retired).
    pub live_chunks: usize,
    /// Chunk-tail bytes returned to the arena free list by this retirement.
    pub tail_bytes_returned: u64,
    /// Registered slots dropped because their owning thread has exited.
    pub slots_pruned: usize,
    /// Buffers left alone because their cell was locked — see
    /// [`ZgcRealHeap::retire_all_tlabs`] on why this is a safe skip and not a
    /// missed obligation. Expected to be zero; a non-zero value means a peer
    /// was mid-allocation (or forcibly stopped inside one) when the collection
    /// started.
    pub skipped_locked: usize,
}

/// A thread-private bump buffer carved out of [`ZgcRealHeap`]'s arena.
///
/// Wraps [`crate::tlab::Tlab`] by value — the same reuse decision
/// `gc::zgc::tlab::ZTlab` makes, and for the same reason: the bump path, the
/// "reserve the aligned FOOTPRINT" rule, the two-case tail filler and the
/// idempotent retire each carry a scar from a real defect, and a second copy
/// would have to be fixed twice.
struct ZArenaTlab {
    /// The shared TLAB doing the bump.
    inner: Tlab,
    /// `[start, end)` of the current chunk, or `None` when retired. Recorded
    /// separately because [`Tlab::retire`] nulls its own pointers and the
    /// retire path still needs the extent.
    chunk: Option<(usize, usize)>,
    /// Accounting, in `zgc::tlab`'s shape so the two allocators' numbers are
    /// comparable.
    ///
    /// [`ZTlabStats::waste_bytes`] stays **zero** here and that is deliberate,
    /// not an omission: nothing is abandoned on this adapter — see
    /// [`ZArenaTlabRetireSummary`]. Recovered tail bytes are counted in
    /// [`Self::tail_returned_bytes`] instead. `pages_taken` likewise stays zero
    /// (there are no pages), and `direct_*` stays zero (there is no
    /// keep-the-buffer direct path — see [`ZgcRealHeap::tlab_refill`]).
    stats: ZTlabStats,
    /// Cumulative chunk-tail bytes handed back to the arena free list.
    tail_returned_bytes: u64,
}

impl ZArenaTlab {
    fn new() -> Self {
        Self {
            inner: Tlab::empty(),
            chunk: None,
            stats: ZTlabStats::default(),
            tail_returned_bytes: 0,
        }
    }

    /// **Fast path.** A plain load / add / compare / store on thread-private
    /// memory: no CAS, no arena lock, no free-list scan, no `write_bytes`.
    ///
    /// The `Release` fence inside [`Tlab::alloc_initialized`] stays (it orders
    /// the caller's header stores ahead of the cursor commit for a cross-thread
    /// STW reader, and emits no instruction on x86-64).
    #[inline]
    fn alloc(&mut self, bytes: usize) -> Option<usize> {
        let ptr = self.inner.alloc(bytes, ZGC_TLAB_ALIGN)?;
        let footprint = zgc_tlab_footprint(bytes);
        self.stats.fast_allocations += 1;
        self.stats.fast_bytes += footprint as u64;
        Some(ptr as usize)
    }
}

/// Every live [`ZArenaTlab`] on one heap, so a safepoint can retire all of them.
///
/// # What is reused from `gc::zgc::tlab`, and the one thing that is not
///
/// Reused verbatim: [`ZTlabHeapHooks`] (implemented for [`ZgcRealHeap`] below),
/// [`ZTlabConfig`] and its `normalized()` clamping, [`ZTlabStats`], and
/// [`current_thread_key`]. The bump itself is [`crate::tlab::Tlab`], which is
/// what `ZTlab` wraps too.
///
/// **Not reused: `ZTlab` / `ZTlabCell` / `ZTlabRegistry` themselves**, because
/// their refill source is a [`crate::zgc::page::ZPageAllocator`] and
/// `ZgcRealHeap` has no pages. That is not a stylistic difference:
///
/// * `ZTlab::refill` is private and takes `&ZPageAllocator`; there is no
///   public entry point that installs a chunk from arbitrary memory.
/// * `ZgcRealHeap::collect_garbage`'s sweep returns every dead object to the
///   **arena** free list (`arena.add_free_block(base - arena_base, size)`)
///   under nothing but a `base >= arena_base` screen. Objects carved from page
///   storage that happens to sit above the arena would be turned into
///   free-list offsets far past the arena's `Vec`, and the arena would then
///   hand out pointers outside its own allocation. Adopting page-backed
///   chunks therefore *requires* changing the sweep, which this change is
///   explicitly not doing.
///
/// So the chunk source — item 1 of the five ZGC-specific things `zgc::tlab`'s
/// header lists — is the one part re-derived here, over the arena. Items 2
/// (page handle) and 3 (allocation colour) do not exist on this heap; items 4
/// (registry/accounting hooks) and 5 (a registry of live TLABs) are what this
/// type and the [`ZTlabHeapHooks`] impl provide.
///
/// # Scoping
///
/// Instance-owned: no `static`, no `OnceLock`. Two heaps in one process (which
/// the gc unit tests routinely create) get two independent sets of buffers.
/// The only process-global is a monotonic *id* counter, which is an identity,
/// not a cache — it exists so a thread-local handle cache cannot alias a
/// recycled heap address.
///
/// # Lock order
///
/// `registry.slots -> ZArenaTlab cell -> ZgcRealHeap::arena` and
/// `ZArenaTlab cell -> ZgcRealHeap::registry`. Every method here snapshots
/// under `slots` and releases it before locking a cell, and no path takes the
/// arena or the object registry before a cell, so the reverse edge does not
/// exist.
pub struct ZArenaTlabRegistry {
    /// Process-unique identity, used only to key the thread-local handle cache.
    id: u64,
    /// Registered buffers, as an atomic so the refill path can read it without
    /// taking [`Self::slots`].
    ///
    /// Taking that lock at refill time would be a lock-order INVERSION: the
    /// documented order is `slots -> cell -> arena`, and a refill runs with the
    /// cell already held. Maintained at the two places `slots` changes size
    /// (insert in `attach`, prune in `retire_all_tlabs`), so it is exact
    /// between them and never more than one insert stale.
    live_slots: AtomicUsize,
    /// Normalised policy. `initial_chunk == 0` means "this heap is too small
    /// for a TLAB" — see [`Self::chunk_bytes_for_capacity`].
    config: ZTlabConfig,
    slots: Mutex<FxHashMap<u64, Arc<Mutex<ZArenaTlab>>>>,
}

/// Monotonic registry ids. Never reused, so a stale thread-local entry for a
/// dropped heap can never be mistaken for a live one that happens to have been
/// allocated at the same address.
static NEXT_ZTLAB_REGISTRY_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

thread_local! {
    /// This thread's [`ZArenaTlab`] handle per registry id.
    ///
    /// A *cache*, not a registry: the authoritative map is
    /// [`ZArenaTlabRegistry::slots`], which is per-heap. Without it every
    /// allocation would pay a mutex plus a hash lookup to find its own buffer,
    /// which is most of the cost the TLAB exists to remove.
    ///
    /// Keyed by [`ZArenaTlabRegistry::id`] and never by address, so an entry
    /// left behind by a dropped heap is unreachable rather than aliasable. Such
    /// an entry keeps an `Arc` alive whose chunk points into freed arena
    /// memory — that is inert: nothing reads it (the id will never match again)
    /// and dropping it runs no destructor that touches the chunk.
    static ZGC_TLAB_HANDLES: std::cell::RefCell<Vec<(u64, Arc<Mutex<ZArenaTlab>>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

impl ZArenaTlabRegistry {
    /// Chunk size for an arena of `capacity` bytes, or `0` to disable.
    ///
    /// `capacity / 1024`, clamped into `[crate::tlab::min_tlab_size(),
    /// ZGC_TLAB_MAX_CHUNK]`, and refused outright when one chunk would be more
    /// than a quarter of the whole heap — at that point the buffer is a second
    /// heap rather than a buffer, and the honest answer is to allocate the way
    /// this backend always has. (The 8 KiB unit tests take that branch.)
    fn chunk_bytes_for_capacity(capacity: usize) -> usize {
        let want = (capacity / 1024).clamp(crate::tlab::min_tlab_size(), ZGC_TLAB_MAX_CHUNK);
        if want.saturating_mul(4) > capacity {
            return 0;
        }
        want & !(ZGC_TLAB_ALIGN - 1)
    }

    /// The chunk size to carve RIGHT NOW, for an arena of `capacity` bytes.
    ///
    /// [`Self::chunk_bytes_for_capacity`] fixes a ceiling once, at
    /// construction. This divides the reservation budget
    /// ([`ZGC_TLAB_RESERVATION_SHARE`]) by the buffers actually registered, so
    /// the chunk shrinks as the thread count rises and the total claimed by
    /// TLABs stays bounded whatever the workload does.
    ///
    /// `0` (the too-small-heap answer) stays `0`: the ceiling decides whether
    /// this heap has TLABs at all, and this only decides how big they are.
    fn chunk_bytes_now(&self, capacity: usize) -> usize {
        let ceiling = self.config.initial_chunk;
        if ceiling == 0 {
            return 0;
        }
        let live = self.live_slots.load(Ordering::Relaxed).max(1);
        let budget = capacity / ZGC_TLAB_RESERVATION_SHARE;
        let want = (budget / live).clamp(crate::tlab::min_tlab_size(), ceiling);
        want & !(ZGC_TLAB_ALIGN - 1)
    }

    /// A registry sized for an arena of `capacity` bytes.
    fn for_capacity(capacity: usize) -> Self {
        let chunk = Self::chunk_bytes_for_capacity(capacity);
        let config = ZTlabConfig {
            initial_chunk: chunk,
            min_chunk: chunk,
            max_chunk: chunk,
            // An object big enough to eat an eighth of a chunk is not worth
            // carving one for; it goes to `alloc_raw`, which is exactly where
            // it went before this change.
            max_tlab_alloc: (chunk / 8).min(crate::tlab::tlab_max_alloc()),
            // HotSpot's refill-waste policy exists to avoid ABANDONING a large
            // chunk remainder. This adapter abandons nothing — the remainder
            // goes back to the arena free list at retire — so the policy has no
            // work to do and the ratchet that guards its pathological middle is
            // likewise unnecessary. Stated as 1/0 rather than left at the
            // defaults so that "there is no waste policy" is visible here.
            refill_waste_fraction: 1,
            waste_increment: 0,
            // BATCHING IS REVERTED, and this is where that is declared.
            //
            // `ZTlabHeapHooks`'s doc argues for per-chunk batched registry
            // inserts, and states the precondition: "a collection only ever
            // observes the registry at a safepoint, and `retire_all` — which
            // every collection must call first — flushes every pending batch
            // before it returns."
            //
            // That precondition does not hold for `ZgcRealHeap`.
            // `is_object_address` is not a GC-only predicate here: the JIT
            // calls it on the *mutator* path as its "is this a valid object
            // pointer" oracle (`vm/src/jit/helpers.rs` — `jit_checkcast`
            // resolves its receiver through it and turns a miss into a silent
            // null, and the conservative-root scanner in
            // `vm/src/jit/conservative_roots.rs` validates every candidate
            // qword through it *before* `collect_garbage` is entered). An
            // object sitting in an unflushed batch would answer `false` to both
            // — a mutator-visible miscompare and a use-after-free respectively,
            // neither of which any safepoint ordering can fix.
            //
            // So a TLAB-served object is inserted into the registry the moment
            // it is handed out, and `registry_batch: 1` says so. The arena
            // mutex — which covers free-list tier scans and a per-object
            // `write_bytes`, not a pointer bump — is what this change removes;
            // the object registry's single hash insert stays. Sharding
            // `ZgcRealHeap::registry` is the obvious follow-up and is a
            // separate change.
            registry_batch: 1,
        }
        .normalized();
        Self {
            id: NEXT_ZTLAB_REGISTRY_ID.fetch_add(1, Ordering::Relaxed),
            // `normalized()` clamps a zero chunk up to the object grid, which
            // would re-enable a TLAB this heap is too small for. Restore the
            // sentinel.
            config: if chunk == 0 {
                ZTlabConfig {
                    initial_chunk: 0,
                    max_tlab_alloc: 0,
                    ..config
                }
            } else {
                config
            },
            slots: Mutex::new(FxHashMap::default()),
            live_slots: AtomicUsize::new(0),
        }
    }

    /// The policy in force.
    fn config(&self) -> &ZTlabConfig {
        &self.config
    }

    /// Is a TLAB available on this heap at all?
    fn is_active(&self) -> bool {
        self.config.initial_chunk > 0
    }

    /// This thread's buffer, creating and registering it on first use.
    ///
    /// `try_with` rather than `with`: a thread-local is inaccessible while its
    /// own destructors run, and an allocation from a TLS destructor must fall
    /// back to the map rather than panic.
    fn attach(&self) -> Arc<Mutex<ZArenaTlab>> {
        let cached = ZGC_TLAB_HANDLES
            .try_with(|c| {
                c.borrow()
                    .iter()
                    .find(|(id, _)| *id == self.id)
                    .map(|(_, cell)| Arc::clone(cell))
            })
            .ok()
            .flatten();
        if let Some(cell) = cached {
            return cell;
        }
        let key = current_thread_key();
        let cell = {
            let mut slots = self.slots.lock();
            match slots.get(&key) {
                Some(existing) => Arc::clone(existing),
                None => {
                    let cell = Arc::new(Mutex::new(ZArenaTlab::new()));
                    slots.insert(key, Arc::clone(&cell));
                    self.live_slots.store(slots.len(), Ordering::Relaxed);
                    cell
                }
            }
        };
        let _ = ZGC_TLAB_HANDLES.try_with(|c| c.borrow_mut().push((self.id, Arc::clone(&cell))));
        cell
    }

    /// Snapshot of every registered buffer. Taken under `slots` and returned
    /// with the guard released — the caller locks cells, and the reverse edge
    /// must not exist.
    fn cells(&self) -> Vec<Arc<Mutex<ZArenaTlab>>> {
        self.slots.lock().values().map(Arc::clone).collect()
    }
}

impl std::fmt::Debug for ZArenaTlabRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZArenaTlabRegistry")
            .field("id", &self.id)
            .field("chunk_bytes", &self.config.initial_chunk)
            .field("max_tlab_alloc", &self.config.max_tlab_alloc)
            .field("live_tlabs", &self.slots.lock().len())
            .finish()
    }
}

/// The bookkeeping a TLAB-served object still needs, because it never touched
/// [`ZgcRealHeap::alloc_raw`].
///
/// This is `zgc::tlab`'s trait, implemented against the heap it was written
/// for. It is the whole of the contract: registry membership, the `allocated`
/// counter, the native-allocation-pressure latch, an identity-hash source and
/// an allocation colour.
impl ZTlabHeapHooks for ZgcRealHeap {
    /// Register freshly allocated object bases and charge their footprint.
    ///
    /// `bytes` is the **reserved footprint**, which is what `allocated` has to
    /// count for `needs_gc` to be right.
    ///
    /// The threshold test below is [`GarbageCollector::needs_gc`]'s predicate
    /// verbatim — `allocated >= gc_threshold && allocated >= gc_rearm` — copied
    /// from [`ZgcRealHeap::alloc_raw`] rather than re-derived, and that is the
    /// whole reason this path cannot reopen the GC storm the
    /// [`gc_rearm`](ZgcRealHeap::gc_rearm) field exists to prevent: a live set
    /// parked above the static threshold leaves `gc_rearm` above `allocated`
    /// after every sweep, so the latch stays DOWN until genuinely new
    /// allocation clears the re-arm floor. Nothing here writes `gc_rearm`;
    /// only the sweep does.
    fn register_allocations(&self, addrs: &[usize], bytes: usize) {
        self.registry.insert_all(addrs);
        if bytes == 0 {
            return;
        }
        let after = self.allocated.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if after >= self.gc_threshold && after >= self.gc_rearm.load(Ordering::Relaxed) {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
    }

    /// [`ZColor::Remapped`] — the quiescent good colour, unconditionally.
    ///
    /// The same answer, for the same reason, as this file's
    /// [`mark::ZMarkContext::good_mask`] impl: **`ZgcRealHeap` stores plain
    /// machine pointers and colours nothing.** Liveness lives in the object
    /// header's [`GC_FLAG_MARKED`] bit, not in a pointer's metadata bits, and
    /// no load barrier runs on this backend. Any answer is therefore inert and
    /// the only question is which one is honest: `Z_REMAPPED` is what
    /// [`vaddr::ZGoodMask::new`] installs and what `mark::mark_color_for` maps
    /// to `None`, so a metrics line reads "this cycle has no mark colour"
    /// rather than claiming a `Marked0`/`Marked1` parity nothing maintains.
    /// Returning a mark colour would be a lie a future barrier could act on.
    ///
    /// When colored pointers land this becomes `self.good_mask.allocation_color()`.
    fn allocation_color(&self) -> ZColor {
        ZColor::Remapped
    }

    /// Identity-hash source.
    ///
    /// Provided because the trait requires it, but **nothing on this heap calls
    /// it**: contrary to the shape the trait's doc assumes,
    /// [`ZgcRealHeap::alloc_array`] does not stamp a hash at allocation — it
    /// leaves the mark word zero and lets
    /// [`GarbageCollector::identity_hash_code`] install one lazily through
    /// `ObjectHeader::mark_word_identity_hash`. A TLAB-served array therefore
    /// needs nothing extra, which is why [`ZgcRealHeap::alloc_tlab`] does not
    /// consult this.
    ///
    /// `ZgcRealHeap::next_hash` is the *inherent* method (inherent candidates
    /// are resolved before trait ones, so this is not a self-call).
    fn next_hash(&self) -> i32 {
        ZgcRealHeap::next_hash(self)
    }

    /// Charge bytes consumed from the arena that will never hold a live object.
    ///
    /// Implemented to the contract — and **not called by this adapter**, which
    /// is the load-bearing half of the note. `zgc::tlab` assumes a retired
    /// chunk tail is abandoned inside a page whose reclamation is a whole-page
    /// event; an arena has a free list, so
    /// [`ZgcRealHeap::tlab_retire_locked`] hands the tail straight back and the
    /// bytes are *recovered*, not wasted. Charging them here as well would
    /// double-count the same span and re-arm the collection trigger early.
    fn note_waste(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.allocated.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl ZgcRealHeap {
    /// Is the TLAB fast path armed on this heap?
    ///
    /// False either because `CRATONVM_ZGC_TLAB` (or
    /// [`Self::set_tlab_enabled`]) turned it off, or because the heap is too
    /// small to carve a chunk from — see
    /// [`ZArenaTlabRegistry::chunk_bytes_for_capacity`].
    pub fn tlab_enabled(&self) -> bool {
        self.tlab_enabled.load(Ordering::Relaxed) && self.tlabs.is_active()
    }

    /// Flip the kill switch at runtime. See [`Self::tlab_enabled`].
    ///
    /// Turning it off leaves already-issued chunks in place; they are closed
    /// and returned by the next [`Self::retire_all_tlabs`], which is *not*
    /// gated on this flag.
    pub fn set_tlab_enabled(&self, enabled: bool) {
        self.tlab_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Aggregate TLAB accounting across every registered buffer.
    ///
    /// [`ZTlabStats::waste_bytes`] is always zero here and that is the honest
    /// answer — this adapter abandons nothing. Recovered tail bytes are
    /// [`Self::tlab_tail_returned_bytes`].
    pub fn tlab_stats(&self) -> ZTlabStats {
        let mut total = ZTlabStats::default();
        for cell in self.tlabs.cells() {
            total.add(&cell.lock().stats);
        }
        total
    }

    /// Cumulative chunk-tail bytes handed back to the arena free list by
    /// retirement, across every registered buffer.
    ///
    /// The counterpart of [`ZTlabStats::waste_bytes`] on a page-backed TLAB:
    /// there the tail is abandoned, here it is recovered. Buffers reaped by
    /// [`Self::retire_all_tlabs`]'s dead-thread prune drop their contribution,
    /// so this is a live-buffer figure, not a lifetime total.
    pub fn tlab_tail_returned_bytes(&self) -> u64 {
        self.tlabs
            .cells()
            .iter()
            .map(|cell| cell.lock().tail_returned_bytes)
            .sum()
    }

    /// **The tripwire.** Reserved `[cursor, end)` tails of every registered
    /// buffer that still holds a live chunk.
    ///
    /// Empty immediately after [`Self::retire_all_tlabs`]. A non-empty result
    /// at a walk point names exactly the spans that would have been walked as
    /// objects — far more useful than the SIGSEGV three phases later.
    pub fn tlab_reserved_tails(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for cell in self.tlabs.cells() {
            if let Some(span) = cell.lock().inner.reserved_tail() {
                out.push(span);
            }
        }
        out.sort_unstable();
        out
    }

    /// **Retire every registered TLAB.** The safepoint entry point, and the
    /// precondition every heap-walking path in this file states.
    ///
    /// When this returns: no registered buffer holds a live chunk, every
    /// chunk tail has been closed with `zgc::tlab`'s filler sentinels and
    /// returned to the arena free list, and every object a TLAB served is in
    /// [`Self::registry`] (it was inserted at hand-out time — see
    /// [`ZArenaTlabRegistry`]'s `registry_batch` note).
    ///
    /// Cells are snapshotted under the `slots` lock, which is released before
    /// any cell is taken, so a thread that is mid-allocation (holding its cell,
    /// waiting on the arena) can never be the far side of a deadlock with a
    /// thread that is attaching.
    ///
    /// # `try_lock`, and why blocking here would be a hang, not a handshake
    ///
    /// `zgc::tlab::ZTlabRegistry::retire_all` blocks on a peer's cell
    /// deliberately: over a *page*-backed TLAB, retiring is a correctness
    /// precondition (an un-retired chunk desyncs a `top`-bounded linear page
    /// walk), so waiting is the only safe answer and blocking substitutes for
    /// an OS-level suspension handshake.
    ///
    /// **On this heap it is not a correctness precondition.** Every walker —
    /// the mark snapshot, the sweep, the census walk, `walk_objects`,
    /// `is_heap_addr` — is driven by [`Self::registry`], so a live chunk's
    /// un-handed-out tail is *invisible* rather than misparsed, and every
    /// object inside a chunk is already registered. Blocking would therefore
    /// buy nothing and risk everything: BUG-03 says this VM does forcibly stop
    /// peers (a frozen in-JIT thread), and one stopped inside its own bump
    /// would hold its cell for the rest of the process — the collector would
    /// wait on it forever. That is the exact failure this change exists to
    /// remove, reintroduced one layer down.
    ///
    /// So an uncontended cell is retired and a contended one is **skipped and
    /// counted** ([`ZArenaTlabRetireSummary::skipped_locked`]). The cost of a
    /// skip is that one chunk tail is not returned to the arena this cycle; the
    /// next collection gets it. This is the same "skip list rather than block"
    /// resolution `zgc::tlab::ZTlabRegistry::reserved_tails` offers for BUG-03.
    ///
    /// Not gated on [`Self::tlab_enabled`]: a chunk issued before the kill
    /// switch was flipped still has to be closed and returned.
    pub fn retire_all_tlabs(&self) -> ZArenaTlabRetireSummary {
        let cells = self.tlabs.cells();
        let mut summary = ZArenaTlabRetireSummary {
            tlabs: cells.len(),
            ..ZArenaTlabRetireSummary::default()
        };
        for cell in cells.iter() {
            let Some(mut tlab) = cell.try_lock() else {
                summary.skipped_locked += 1;
                continue;
            };
            if tlab.chunk.is_some() {
                summary.live_chunks += 1;
            }
            summary.tail_bytes_returned += self.tlab_retire_locked(&mut tlab) as u64;
        }
        // Drop our own handles BEFORE the prune below reads `Arc::strong_count`.
        drop(cells);
        // Reap buffers whose owning thread has exited. A live thread always
        // holds a second `Arc` — either in `ZGC_TLAB_HANDLES` for its whole
        // life, or on its stack while it is inside `attach`/`alloc_tlab` — so
        // `strong_count == 1` means "only the map still refers to this", i.e.
        // the owner is gone. Every such buffer was retired one loop above, so
        // dropping it releases nothing the arena still needs. Without this the
        // map grows by one entry per thread ever created, and `ThreadId`s are
        // never reused, so nothing would ever remove them.
        {
            let mut slots = self.tlabs.slots.lock();
            let before = slots.len();
            slots.retain(|_, cell| Arc::strong_count(cell) > 1);
            summary.slots_pruned = before - slots.len();
            self.tlabs.live_slots.store(slots.len(), Ordering::Relaxed);
        }
        // The per-buffer tripwire is asserted inside `tlab_retire_locked`,
        // while that buffer's cell lock is still held — race-free, and strictly
        // stronger than a sweep of `tlab_reserved_tails()` after the fact. A
        // global assert here would be a false alarm waiting to happen: this
        // entry point is reachable from `walk_objects` and from the census
        // driver, neither of which stops the world, so a peer may legitimately
        // refill between the loop and the check. `tlab_reserved_tails()` stays
        // available as the tripwire for a caller that IS at a safepoint.
        if summary.live_chunks > 0 || summary.slots_pruned > 0 || summary.skipped_locked > 0 {
            tracing::debug!(
                target: "zgc",
                tlabs = summary.tlabs,
                live_chunks = summary.live_chunks,
                tail_bytes_returned = summary.tail_bytes_returned,
                slots_pruned = summary.slots_pruned,
                skipped_locked = summary.skipped_locked,
                "zgc real: retired TLABs",
            );
        }
        summary
    }

    /// Close one buffer's chunk and hand its tail back to the arena.
    ///
    /// Returns the tail bytes returned. Idempotent: a second call finds no
    /// chunk and does nothing, which matters because several paths can retire
    /// the same buffer with no synchronisation between them (a collection, then
    /// a census walk, then a thread teardown) — exactly as [`Tlab::retire`]'s
    /// own idempotence note describes.
    ///
    /// The caller must hold the cell lock. The arena lock is taken *inside*
    /// (cell -> arena, never the reverse) and is not held across anything else.
    fn tlab_retire_locked(&self, tlab: &mut ZArenaTlab) -> usize {
        let Some((chunk_start, chunk_end)) = tlab.chunk.take() else {
            return 0;
        };
        // Read the tail BEFORE `retire()`, which nulls the cursor.
        let tail = tlab.inner.reserved_tail();
        // Installs the `int[]` filler over `[cursor, end)`, or the
        // `GAP_FILLER_CLASS_ID` sentinel for a sub-header tail. Strictly
        // speaking this heap has no linear walker to protect — its sweep,
        // its census walk and `walk_objects` are all driven by
        // `ZgcRealHeap::registry`, so an un-parsed span is invisible rather
        // than fatal. It is installed anyway: the span is about to enter the
        // arena free list shared with every other allocator in this crate, the
        // write is one header, and "walkable" is the state the rest of the tree
        // assumes of arena bytes below the cursor.
        tlab.inner.retire();
        tlab.stats.retires += 1;
        debug_assert!(tlab.inner.reserved_tail().is_none());
        let Some((tail_start, tail_end)) = tail else {
            return 0;
        };
        debug_assert!(tail_start >= chunk_start && tail_end <= chunk_end);
        let bytes = tail_end - tail_start;
        if bytes == 0 {
            return 0;
        }
        // Return the tail to the free list rather than abandoning it. This is
        // the difference between an arena and a page, and it is not an
        // optimisation: `retire_all_tlabs` runs at EVERY collection, so every
        // thread abandons a partly-used chunk on every cycle. At 64 KiB and 50
        // threads that is up to 3 MiB per collection of memory no walker can
        // ever see again (the filler is not in `registry`, so the sweep never
        // visits it and never frees it). A few dozen cycles would exhaust the
        // heap. `note_waste` is therefore deliberately NOT called for these
        // bytes — they come back.
        {
            let mut arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            if tail_start >= base {
                arena.add_free_block(tail_start - base, bytes);
            }
        }
        // `stats.waste_bytes` stays zero on purpose — nothing was wasted. The
        // recovered bytes are counted separately.
        tlab.tail_returned_bytes += bytes as u64;
        bytes
    }

    /// Carve a fresh chunk out of the arena for `tlab`.
    ///
    /// `Some(())` means the caller's retry is guaranteed to fit. The chunk is
    /// no longer always `initial_chunk` — a recycled one may be shorter, see
    /// the note in the body — but it is never shorter than `need`, which is
    /// what the guarantee actually rests on. `None` means the arena could not
    /// serve a chunk; the caller falls back to [`Self::alloc_raw`], which is
    /// the pre-TLAB path unchanged.
    ///
    /// There is no keep-the-buffer "direct" path and no HotSpot refill-waste
    /// ratchet, because the remainder is recovered rather than abandoned — see
    /// [`ZArenaTlabRegistry::for_capacity`].
    fn tlab_refill(&self, tlab: &mut ZArenaTlab, need: usize) -> Option<()> {
        // Sized against the LIVE buffer count, not once at construction — see
        // `ZGC_TLAB_RESERVATION_SHARE`. A request too big for the current chunk
        // takes `alloc_raw`, which is where it went before TLABs existed.
        let want = self
            .tlabs
            .chunk_bytes_now(self.arena_end.saturating_sub(self.arena_base));
        if want == 0 || need > want {
            return None;
        }
        self.tlab_retire_locked(tlab);
        // Take the arena lock for the bump ONLY. The zeroing below is the
        // expensive half and must not be inside it — that is the very
        // serialisation this whole section exists to remove.
        let carved = {
            let mut arena = self.arena.lock();
            // ACCEPT A SHORTER CHUNK WHEN THE FREE LIST HAS ONE.
            //
            // This is the difference between an allocator that recycles and one
            // that only consumes. A retired chunk gives back the span BELOW its
            // survivors, so a chunk that held even one live object comes back
            // SHORTER than a chunk — and a fixed-size request for `want` can
            // then never be served by the remains of any chunk that ever
            // contained a survivor. The free list fills with near-chunk-sized
            // spans that only a bump can be an alternative to, and the bump is
            // finite.
            //
            // Measured on `TestNonBlockingAPI` (2026-08-13, `-Xmx 2g`) with the
            // fixed-size request: **3,828 free spans of 524,192 bytes** — one
            // per thread that had ever parked — against a 524,288-byte chunk
            // request. Every one of them was **96 bytes short**: exactly one
            // `AbstractQueuedSynchronizer$ConditionNode`. 1.96 GB of a 2.15 GB
            // heap sat on the free list in pieces that were each one small
            // object short of reusable, so every refill in the process bumped,
            // the arena reached capacity, and a 2 MB `char[]` had nowhere to go.
            //
            // The floor is `want / 8` — the same `max_tlab_alloc` bound that
            // decides what a TLAB will serve at all, so a chunk at the floor
            // still holds at least eight of the largest object it can ever be
            // asked for. Below that a buffer is churning rather than buffering
            // and the honest answer is a full-size chunk (or the failure that
            // follows it).
            let largest = arena.largest_low_free_block();
            let sized = recycled_chunk_size(want, need, largest)
                .and_then(|size| arena.alloc(size, ZGC_TLAB_ALIGN).map(|p| (p, size)));
            // `alloc(want)` covers both the "the free list has a full-size
            // block" case and the bump.
            sized.or_else(|| arena.alloc(want, ZGC_TLAB_ALIGN).map(|p| (p, want)))
        };
        let (ptr, want) = carved?;
        // An arena block can carry stale bytes: a split remainder, or the tail
        // filler header a previous retire left behind. `alloc_raw` zeroes per
        // object for exactly this reason; the TLAB pays it once per chunk
        // instead, so `Tlab::alloc` can hand out already-zero memory the way
        // `zgc::tlab` does over a zeroed page reservation.
        // SAFETY: `arena.alloc` guarantees `want` valid bytes at `ptr`, and the
        // span is exclusively ours until retire.
        unsafe { std::ptr::write_bytes(ptr, 0, want) };
        // SAFETY: `[ptr, ptr + want)` was just reserved from the arena and is
        // owned exclusively by this thread until retire; it is zeroed above;
        // `want` is a multiple of `ZGC_TLAB_ALIGN`, which is what `Tlab::new`'s
        // tail-filler contract requires of `ptr + want`.
        tlab.inner = unsafe { Tlab::new(ptr, want) };
        tlab.chunk = Some((ptr as usize, ptr as usize + want));
        tlab.stats.refills += 1;
        tlab.stats.refill_bytes += want as u64;
        Some(())
    }

    /// Bump-allocate `size` zeroed bytes from this thread's TLAB.
    ///
    /// Returns `None` when the caller must take [`Self::alloc_raw`]: the kill
    /// switch is off, the heap is too small for a chunk, the object is larger
    /// than `max_tlab_alloc`, or the arena could not serve a refill.
    ///
    /// # Accounting
    ///
    /// Each object charges its own **footprint** to `allocated` through
    /// [`ZTlabHeapHooks::register_allocations`], in the same instant it enters
    /// [`Self::registry`] — the chunk itself is never charged, so nothing is
    /// counted twice. The residual is that the not-yet-handed-out part of a
    /// live chunk is invisible to `allocated`, bounded by
    /// `live_threads * chunk` and squeezed by [`ZGC_TLAB_MAX_CHUNK`]. That
    /// errs on the *late* side of the collection trigger, which is the same
    /// direction `alloc_raw` already errs (it charges the unrounded `size`
    /// while the arena consumes the rounded footprint plus alignment padding),
    /// and it collapses to zero at every collection because
    /// [`Self::retire_all_tlabs`] runs first and hands every remainder back.
    ///
    /// Charging the whole chunk at refill instead would make `allocated`
    /// exact between collections and wrong across one: a sweep republishes
    /// `allocated` as the surviving *object* bytes, so a threshold crossed by
    /// chunk reservations would not still be crossed afterwards, and the
    /// parked-live-set re-arm reasoning (`gc_rearm`) would be reading two
    /// different quantities on either side of the sweep.
    fn alloc_tlab(&self, size: usize) -> Option<*mut u8> {
        if !self.tlab_enabled.load(Ordering::Relaxed) {
            return None;
        }
        let config = self.tlabs.config();
        if config.initial_chunk == 0 || size == 0 || size > config.max_tlab_alloc {
            return None;
        }
        let cell = self.tlabs.attach();
        let mut tlab = cell.lock();
        let addr = match tlab.alloc(size) {
            Some(addr) => addr,
            None => {
                self.tlab_refill(&mut tlab, size)?;
                tlab.alloc(size)?
            }
        };
        // Registered INSIDE the cell lock, and before the pointer is returned
        // to anyone: `is_object_address` is a mutator-path oracle on this heap,
        // not just a GC one, so the window in which a live object is absent
        // from the registry has to be as narrow as `alloc_raw`'s.
        <Self as ZTlabHeapHooks>::register_allocations(
            self,
            std::slice::from_ref(&addr),
            zgc_tlab_footprint(size),
        );
        Some(addr as *mut u8)
    }

    /// [`Self::alloc_tlab`] with [`Self::alloc_raw`] as the fallback.
    ///
    /// The single funnel every object and array allocation on this backend now
    /// takes. The fallback is the pre-TLAB path byte-for-byte, so a heap with
    /// the kill switch off behaves exactly as it did before this change.
    #[inline]
    fn alloc_raw_tlab(&self, size: usize) -> Option<*mut u8> {
        match self.alloc_tlab(size) {
            Some(ptr) => Some(ptr),
            None => self.alloc_raw(size),
        }
    }
}

/// Everything the one-shot fragmentation report needs, gathered while the
/// arena lock is held so the logging — which resolves class NAMES through the
/// VM's class manager — can happen after it is released.
///
/// Splitting it is not tidiness. [`ZgcRealHeap::alloc_raw`] reaches its failure
/// arm holding the arena mutex, and [`crate::collector::class_name_for_diagnostics`]
/// calls back into the VM to take the class-manager lock; naming under the
/// arena lock would invert `arena -> class_manager` against every ordinary
/// allocation path (`class_manager -> arena`) — i.e. turn a diagnostic into a
/// deadlock reachable only when the heap is already failing.
struct ZFragReport {
    profile: crate::arena::FragProfile,
    /// The large-object end's downward bump cursor. `capacity` means the end
    /// was never used at all, which is a different diagnosis from "used and
    /// full".
    high_cursor: usize,
    /// `free_high` blocks, bytes and largest — the split's OWN free list. "The
    /// split is in place" and "the split has room in it" are different claims,
    /// and only the second predicts whether a large request can be served.
    high_blocks: usize,
    high_bytes: usize,
    high_max: usize,
    /// Bytes of the large-object floor the high end has not claimed yet. Large
    /// here and a failing large allocation together mean the reserve was
    /// respected but the region never grew into it; small means the region is
    /// as big as it is allowed to get and the floor is the thing to raise.
    high_reserve_unclaimed: usize,
    /// `(class_id, objects, bytes)` for the occupants of the winning window's
    /// walls, largest byte total first.
    occupants: Vec<(u32, usize, usize)>,
    /// Bytes inside the window that are below the cursor but are neither
    /// free-listed nor a registered object base.
    unregistered_bytes: usize,
}

impl ZFragReport {
    /// Emit the report. Called with **no** heap lock held — see the type doc.
    fn log(&self, request: usize) {
        let p = &self.profile;
        tracing::warn!(
            target: "cratonvm::gc::guard",
            request,
            spans = p.spans,
            largest_span = p.largest_span,
            free_bytes = p.free_bytes,
            walls = p.walls,
            wall_bytes = p.wall_bytes,
            span_hist = %crate::arena::format_log2_hist(&p.span_hist),
            wall_hist = %crate::arena::format_log2_hist(&p.wall_hist),
            high_cursor = self.high_cursor,
            high_blocks = self.high_blocks,
            high_bytes = self.high_bytes,
            high_max = self.high_max,
            high_reserve_unclaimed = self.high_reserve_unclaimed,
            "zgc frag: the shape of the free list at the failing request \
             (histograms are log2 lower bounds: `512K:3892` = 3892 spans of \
             512 KiB..1 MiB)",
        );
        let Some(w) = p.cheapest else {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                request,
                "zgc frag: no contiguous stretch of the swept region is even {request} bytes \
                 wide — the walls are not the problem, the request exceeds the whole span \
                 from the lowest free block to the highest.",
            );
            return;
        };
        tracing::warn!(
            target: "cratonvm::gc::guard",
            request,
            window_start = w.start,
            window_end = w.end,
            window_bytes = w.end - w.start,
            window_free = w.free_bytes,
            wall_bytes = w.wall_bytes,
            walls = w.walls,
            "zgc frag: the CHEAPEST window that could serve this request — {} live bytes \
             in {} run(s) are all that stand between {} free bytes spread over {} bytes of \
             contiguous arena. A small number here means a handful of survivors is holding \
             the window hostage; a number near the request means the region is a genuine \
             live/dead mosaic.",
            w.wall_bytes,
            w.walls,
            w.free_bytes,
            w.end - w.start,
        );
        for &(class_id, count, bytes) in &self.occupants {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                class = %crate::collector::class_name_for_diagnostics(class_id),
                count,
                bytes,
                "zgc frag: wall occupant",
            );
        }
        if self.unregistered_bytes != 0 {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                bytes = self.unregistered_bytes,
                "zgc frag: bytes inside the window that are below the cursor but neither \
                 free-listed nor a registered object base — memory no allocator and no \
                 sweep can see.",
            );
        }
    }
}


/// Heap-side seam for the reference-slot census.
///
/// # Why this exists at all
///
/// `docs/feature-designs/zgc-reference-slot-representation.md` ends with exactly
/// one unresolved number — *what fraction of live reference slots are legacy
/// 16-byte cells?* — and says in as many words that it is the only guess in the
/// document and the one that sizes the work. [`census`] is the instrument; this
/// impl is the only thing that makes it non-inert. Without it the census module
/// compiles, unit-tests, and measures nothing.
///
/// # This is a FORK of [`ZgcRealHeap::enumerate_references`], not a reuse of it
///
/// The marker and the census want different things out of the same three arms,
/// and every difference is load-bearing rather than incidental:
///
/// * **Nulls are kept.** The marker discards `raw == 0` because null has no
///   out-edge. The census must count it: a null slot is a barrier fast-path hit,
///   so a heap of null legacy fields is *not* a heap of barrier work, and the
///   two are indistinguishable once the nulls are dropped.
/// * **Every legacy slot is reported, tagged, whatever its tag.** The marker
///   only wants `Value::Object(Some(_))`. Filtering here would merge the
///   never-written population (`tag == 0` from the zero-fill) into "this class
///   has no reference fields", which is the difference the census's
///   `legacy_unwritten_slots` column exists to keep.
/// * **No `skip_index`.** A `java.lang.ref.Reference`'s referent is not a strong
///   edge, so the marker hides it; but it *is* a reference slot, and the census
///   counts slots.
/// * **Raw reads, never `read_prim_element`.** That helper's plausibility
///   degrade (`gc/src/heap.rs:1687-1714`) turns an implausible word into
///   `Value::Object(None)`. For a colored pointer that degrade is exactly the
///   corruption the study names as hazard 2, and a census that inherits it
///   reports the corrupted population as null instead of as a finding.
///
/// # The two-phase contract
///
/// [`census::ZSlotCensus::run_walk`] drains
/// [`census::ZCensusHeapView::for_each_live_object`] into a local `Vec` **before**
/// it calls [`census::ZCensusHeapView::reference_slots`] for anything, so the
/// registry lock taken below is never held across a call back into this heap.
/// That is the whole reason the trait is shaped this way; see this tree's
/// standing note that a registry guard held across a call-back is a lock cycle.
impl census::ZCensusHeapView for ZgcRealHeap {
    /// Visit every **marked** registered allocation.
    ///
    /// "Live" here means [`GC_FLAG_MARKED`], so this is only meaningful between
    /// the end of the mark phase and the sweep that clears those bits — which is
    /// exactly where [`ZgcRealHeap::collect_garbage`] calls the census from. Run
    /// outside that window every bit is clear and the walk reports
    /// `ZCensusVerdict::NoData`, which the census prints as "nothing was
    /// decided" rather than as a row of zeroes.
    fn for_each_live_object(
        &self,
        f: &mut dyn FnMut(u64, ClassId, census::ZCensusObjectKind),
    ) {
        // Heap-walking entry point: retire every TLAB first
        // ([`ZgcRealHeap::retire_all_tlabs`]). Inside `collect_garbage` — the
        // only caller today — this is a no-op second retire, because the cycle
        // retires before its mark snapshot and the world is stopped in between.
        // It is here for the census driver that calls `run_walk` directly,
        // which reaches no safepoint of its own.
        self.retire_all_tlabs();

        // Materialise the base list BEFORE reading a single header. The trait
        // permits holding the registry for the whole callback, but this heap's
        // `header_mut` reads arena bytes and `effectively_compact_header`
        // reaches the layout cache, so the shorter window is free to take —
        // and on the `Hash` arm of `ZObjectStarts` there is still a real guard
        // to keep out of that window.
        let bases: Vec<usize> = self.registry.bases();

        for base in bases {
            let header = self.header_mut(base as *mut u8);
            if header.gc_flags() & GC_FLAG_MARKED == 0 {
                continue;
            }
            let kind = match header.kind() {
                ObjectKind::Object => {
                    // The EFFECTIVE predicate, not the header bit — see
                    // `effectively_compact_header`. An object the read path
                    // treats as a 16-byte cell is a legacy instance for census
                    // purposes no matter what its flags say.
                    if Self::effectively_compact_header(header) {
                        census::ZCensusObjectKind::CompactInstance
                    } else {
                        census::ZCensusObjectKind::LegacyInstance
                    }
                }
                ObjectKind::Array => {
                    if header.element_type() == ArrayElementType::Reference {
                        census::ZCensusObjectKind::ReferenceArray
                    } else {
                        census::ZCensusObjectKind::NoReferenceSlots
                    }
                }
                ObjectKind::HumongousFiller => census::ZCensusObjectKind::NoReferenceSlots,
            };
            let class_id = header.class_id;
            f(base as u64, class_id, kind);
        }
    }

    /// The second, independent derivation of compactness the census cross-checks
    /// against the kind reported above.
    ///
    /// It must agree with [`census::ZCensusObjectKind::CompactInstance`] for
    /// every object or the walk is void
    /// ([`census::ZWalkResult::kind_disagreements`]). Both sides route through
    /// [`ZgcRealHeap::effectively_compact_header`]; the `kind() == Object` screen
    /// is what makes them agree for arrays and fillers, which are never compact
    /// instances regardless of what a stale flag byte might read.
    fn is_compact(&self, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        let header = self.header_mut(addr as usize as *mut u8);
        header.kind() == ObjectKind::Object && Self::effectively_compact_header(header)
    }

    /// Report every slot of the object at `addr` that could hold a reference.
    ///
    /// See the impl header for how each arm differs from
    /// [`ZgcRealHeap::enumerate_references`].
    fn reference_slots(&self, addr: u64, f: &mut dyn FnMut(census::ZSlotObservation)) {
        if addr == 0 {
            return;
        }
        let base = addr as usize as *mut u8;
        let header = self.header_mut(base);
        match header.kind() {
            ObjectKind::Object => {
                let class_id = header.class_id.as_u32();
                let num_slots = header.num_slots();
                // The RAW header bit here, deliberately, and it is the one place
                // in this impl where the effective predicate is the wrong tool:
                // the bit decides the object's BODY LAYOUT (`alloc_object` sizes
                // a flagged object with `layout.body_size`, not
                // `num_slots * SLOT_SIZE`), while the effective predicate decides
                // how the read path INTERPRETS that body. Striding 16-byte cells
                // through a compact-sized body because the layout lookup failed
                // would read past the allocation — `get_field`'s fall-through
                // does exactly that, and it is a latent bug there that this
                // instrument must not reproduce.
                let header_bit_compact = cratonvm_types::is_compact_object(header);
                if header_bit_compact {
                    let laid_out = cratonvm_types::with_class_layout(
                        class_id,
                        num_slots,
                        |layout| {
                            for (&offset, &is_ref) in
                                layout.field_offsets.iter().zip(layout.is_ref.iter())
                            {
                                // No `skip_index` and no `raw == 0` filter; see
                                // the impl header.
                                if !is_ref {
                                    continue;
                                }
                                // SAFETY: `offset` comes from this object's own
                                // registered layout, which is the layout its body
                                // was sized with at allocation, so the field lies
                                // inside the allocation. `read_ref_slot` reads
                                // `ref_field_size()` bytes — the width the layout
                                // reserved for a reference field — and is used
                                // rather than a bare `read::<u64>` so the
                                // observation matches what `read_compact_field`
                                // (`types/src/field_layout.rs:978-993`) would
                                // actually load under narrow oops.
                                let slot = unsafe { base.add(HEADER_SIZE + offset as usize) };
                                let raw =
                                    unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                                f(census::ZSlotObservation::bare(
                                    census::ZSlotShape::CompactField,
                                    slot as u64,
                                    raw,
                                ));
                            }
                        },
                    )
                    .is_some();
                    if !laid_out {
                        // Flagged compact, no layout for `(class_id, num_slots)`:
                        // `for_each_live_object` reported this object as a LEGACY
                        // instance (the read path will treat it as one), but its
                        // body is compact-sized, so its cells cannot be walked
                        // without reading out of bounds. Report no slots and say
                        // so. The object is still counted; only its slots are
                        // missing, which biases the legacy share DOWN — the
                        // conservative direction for a decision rule whose
                        // failure mode is deferring work that is not deferrable.
                        tracing::warn!(
                            target: "zgc",
                            class_id,
                            num_slots,
                            "zgc census: compact-flagged object has no registered layout; \
                             its slots are omitted from the census (cells cannot be walked \
                             through a compact-sized body)"
                        );
                    }
                } else {
                    let n = num_slots as usize;
                    // The same "no real class has 1<<24 fields" screen
                    // `check_field_index` applies. A desynced walk that decoded
                    // payload bytes as a header would otherwise stride millions
                    // of cells straight out of the arena.
                    if n > (1 << 24) {
                        tracing::debug!(
                            target: "zgc",
                            num_slots = n,
                            "zgc census: suspect header, slots omitted"
                        );
                        return;
                    }
                    for i in 0..n {
                        // SAFETY: a non-compact object's body is exactly
                        // `num_slots * SLOT_SIZE` bytes (`alloc_object`'s
                        // `fields_size` fallback), so cell `i` and both words
                        // read out of it lie inside the allocation. The two
                        // reads are 4- and 8-byte aligned because `SLOT_SIZE` is
                        // 16 and `HEADER_SIZE` is 8-byte aligned.
                        let cell = unsafe { base.add(HEADER_SIZE + i * SLOT_SIZE) };
                        let tag = unsafe {
                            std::ptr::read(
                                cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32
                            )
                        };
                        let word_ptr =
                            unsafe { cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) };
                        let word = unsafe { std::ptr::read(word_ptr as *const u64) };
                        // EVERY slot, tag and all — the census does the tag
                        // arithmetic itself. Filtering to `Value::Object` here is
                        // what would merge the never-written population into the
                        // "no reference fields" one and make the answer wrong.
                        f(census::ZSlotObservation::tagged(
                            census::ZSlotShape::LegacyField,
                            word_ptr as u64,
                            word,
                            tag,
                        ));
                    }
                }
            }
            ObjectKind::Array => {
                if header.element_type() == ArrayElementType::Reference {
                    let len = header.array_length() as usize;
                    // `ref_element_size()`, not the `REF_ELEMENT_SIZE` constant:
                    // the constant is the wide (8-byte) width, and the stride is
                    // 4 under narrow oops. This is the stride
                    // `read_prim_element`'s own reference arm uses, so the census
                    // reads the same words the VM does.
                    let stride = cratonvm_types::ref_element_size();
                    // SAFETY: the data area begins at `base + ARRAY_DATA_OFFSET`
                    // and holds `array_length()` elements of `stride` bytes;
                    // `i < len`.
                    let data = unsafe { base.add(ARRAY_DATA_OFFSET) };
                    for i in 0..len {
                        let slot = unsafe { data.add(i * stride) };
                        // A RAW read. `read_prim_element` would apply
                        // `plausible_heap_pointer` and hand back
                        // `Value::Object(None)` for a colored or corrupt word —
                        // the study's hazard 2 — silently reclassifying the exact
                        // population this census exists to find as null.
                        let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                        f(census::ZSlotObservation::bare(
                            census::ZSlotShape::ArrayElement,
                            slot as u64,
                            raw,
                        ));
                    }
                }
                // Primitive arrays have no reference slots.
            }
            ObjectKind::HumongousFiller => {}
        }
    }

    // `static_reference_slots` is left at its default no-op ON PURPOSE. A
    // `StaticsBlock` is `Box::leak`ed memory owned by
    // `vm/src/vm/realms/class_realm.rs:43-46`, outside this crate and outside
    // this collector's registry, so nothing here can reach it. The census
    // therefore reports the `StaticField` column as NOT WIRED rather than 0 —
    // see `ZSlotCensus::statics_wired`, which this heap never sets.
}

/// The concurrent marking engine's view of this heap.
///
/// # This makes the engine POSSIBLE; it does not adopt it
///
/// `gc/src/zgc/mark.rs` is a complete concurrent marker (striped
/// work-stealing stacks, a termination handshake, a mutator ingress) and
/// `gc/src/zgc_concurrent.rs` is a complete driver for it, but until this impl
/// existed nothing implemented [`mark::ZMarkContext`] for a real heap, so both
/// were unreachable code that compiled and marked nothing.
///
/// [`ZgcRealHeap::collect_garbage`] is deliberately unchanged: it still runs
/// its own single-threaded stop-the-world mark loop, and this impl is not
/// wired into it. Switching the production path from that loop to the
/// concurrent engine is a separate and much riskier change — it needs a
/// mark-start safepoint, a mutator write barrier feeding
/// [`mark::ZMarkIngress`], and a decision about what happens to
/// [`Self::collect_garbage_with_finalizers`]'s resurrection pass — and it is
/// not this change's call to make. Two consequences follow, and both are
/// intentional:
///
/// * The two markers now enumerate references through *different* code
///   ([`ZgcRealHeap::visit_strong_refs_at`] here,
///   [`ZgcRealHeap::enumerate_references`] there). They now agree on the words
///   they load — the narrow-oop defect that once separated them has been fixed
///   at the source; see [`ZgcRealHeap::visit_strong_refs_at`]'s doc comment for
///   what remains a deliberate difference (shape, and the legacy arm's
///   tear-safe raw tag/payload read).
/// * Nothing calls [`ZgcRealHeap::begin_concurrent_mark_cycle`] yet, so on
///   today's execution path this impl is inert.
///
/// # Address domain
///
/// Every `u64` crossing this trait is an unmasked machine address of an object
/// base, never a [`vaddr::ZColoredWord`]. This heap stores raw pointers in its
/// slots and never colors them, so there is nothing to uncolor on the way in;
/// a colored word that somehow reached a slot would be refused by
/// [`Self::is_in_heap`] like any other wild child, which is the outcome
/// `vaddr`'s bit-63 tag exists to produce.
/// Lends a `&ZgcRealHeap` to the mark engine for the duration of ONE
/// stop-the-world collection.
///
/// # Why this exists
///
/// [`mark::ZMarkCoordinator::new`] takes an `Arc<dyn ZMarkContext>`, and
/// `ZgcRealHeap` is held **by value** inside `VmHeap` — there is no `Arc` to
/// hand it and no safe way to mint one. That is the whole of the reason the
/// marking engine sat unadopted after its `ZMarkContext` impl landed; it is not
/// a missing feature, it is an ownership mismatch.
///
/// # Why it is sound
///
/// Three facts, and all three are needed:
///
/// 1. **The bridge is created, used and destroyed inside a single
///    [`ZgcRealHeap::mark_parallel_stw`] call**, which takes `&self`. It is
///    never stored on the heap, never returned, and never handed to anything
///    that outlives that call.
/// 2. **[`mark::ZMarkCoordinator`]'s `Drop` joins every worker.** Unusually
///    for this crate it does not detach — its own doc says so — so when the
///    coordinator goes out of scope, no thread holding a clone of this `Arc`
///    is still running. That covers the panic path as well as the normal one,
///    which is why `mark_parallel_stw` needs no explicit guard.
/// 3. **The heap cannot move while `&self` is live**, and `&self` outlives the
///    coordinator by construction of (1).
///
/// The cost of that safety is a worker-pool spawn and join per collection.
/// That is real and it is why this path is opt-in; caching the pool across
/// collections would require the heap to be `Arc`-owned, which is the larger
/// change this deliberately does not make.
struct ZHeapMarkBridge {
    heap: *const ZgcRealHeap,
}

// SAFETY: the pointer is only ever dereferenced through `ZMarkContext`, whose
// methods all take `&self` on a heap that is Sync; and the bridge cannot
// outlive the borrow it was built from — see the type doc's three facts. The
// `!Send`ness being overridden here is `*const T`'s blanket one, not a
// property of `ZgcRealHeap`.
unsafe impl Send for ZHeapMarkBridge {}
// SAFETY: as above.
unsafe impl Sync for ZHeapMarkBridge {}

impl ZHeapMarkBridge {
    #[inline]
    fn heap(&self) -> &ZgcRealHeap {
        // SAFETY: see the type doc. The referent outlives every call.
        unsafe { &*self.heap }
    }
}

impl mark::ZMarkContext for ZHeapMarkBridge {
    fn good_mask(&self) -> u64 {
        self.heap().good_mask()
    }
    fn try_mark(&self, addr: u64) -> bool {
        self.heap().try_mark(addr)
    }
    fn is_marked(&self, addr: u64) -> bool {
        self.heap().is_marked(addr)
    }
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        self.heap().visit_refs(addr, f)
    }
    fn is_in_heap(&self, addr: u64) -> bool {
        self.heap().is_in_heap(addr)
    }
    fn object_size(&self, addr: u64) -> usize {
        self.heap().object_size(addr)
    }
}

/// Phase 4 of the ZGC maturity plan: the load-barrier seam.
///
/// # What this is, and what it is emphatically not
///
/// This is the `ZBarrierContext` the built-and-unadopted `zgc::barrier` module
/// has been waiting for. Implementing it makes the barrier *drivable* against
/// this heap. It does **not** put a barrier on any read path: no interpreter
/// `getfield`, no JIT-emitted load and no native accessor calls
/// `load_barrier_fast` today, so in a normal run every method below is
/// unreachable and `good_mask` never leaves `Z_REMAPPED`.
///
/// That distinction is the whole discipline of this phase and it is why the
/// relocation refusal gate (`zgc_relocation_permitted`) stays shut with the
/// JIT on: a colored slot that a JIT-compiled load reads without a barrier is
/// a use-after-free, and no amount of correctness *here* changes that.
impl barrier::ZBarrierContext for ZgcRealHeap {
    fn good_mask(&self) -> u64 {
        self.barrier_good_mask.load(Ordering::Acquire)
    }

    fn is_marking(&self) -> bool {
        self.mark_active.load(Ordering::Relaxed)
    }

    fn is_relocating(&self) -> bool {
        self.relocate_active.load(Ordering::Relaxed)
    }

    /// Map a heap **offset** to where that object lives now.
    ///
    /// Identity for anything this cycle has not moved, which is every object
    /// while `relocate_active` is false — so a caller that reaches the slow
    /// path outside a relocating cycle gets its own offset back rather than a
    /// `None` the barrier would turn into
    /// [`on_forward_failure`](barrier::ZBarrierContext::on_forward_failure)'s
    /// panic.
    ///
    /// Returns a **bare offset**, never an address: `is_bare_offset` on the
    /// barrier's slow path checks exactly that, in release builds too, because
    /// returning an address here was the shape that made this method's return
    /// type `Option<u64>` in the first place.
    fn forward(&self, addr: u64) -> Option<u64> {
        if !self.relocate_active.load(Ordering::Relaxed) {
            return Some(addr);
        }
        Some(
            self.forwarding
                .lock()
                .get(&addr)
                .copied()
                .unwrap_or(addr),
        )
    }

    /// Publish `addr` (an offset) to the concurrent marker.
    ///
    /// Shares the ingress `satb_pre_barrier` feeds — one queue per heap, so a
    /// cycle drains mutator-published work from the store barrier and the load
    /// barrier together rather than needing two drains that could disagree
    /// about when they are empty.
    fn mark_live(&self, addr: u64) {
        if addr == 0 {
            return;
        }
        let Some(base) = <Self as mark::ZMarkContext>::heap_base(self) else {
            return;
        };
        let absolute = base.wrapping_add(addr) as usize;
        if !self.registry.contains(absolute) {
            return;
        }
        self.mark_ingress.push(absolute >> 3, absolute as u64);
        self.mark_ingress_pushes.fetch_add(1, Ordering::Relaxed);
    }

    fn stats(&self) -> &barrier::ZBarrierStats {
        &self.barrier_stats
    }
}

impl mark::ZMarkContext for ZgcRealHeap {
    /// [`vaddr::Z_REMAPPED`] — the quiescent good mask, unconditionally.
    ///
    /// # Why a constant and not a cycle-flipped mask
    ///
    /// Colored pointers are not on this heap's live path. `ZgcRealHeap` keeps
    /// liveness in the object header's [`GC_FLAG_MARKED`] bit, not in a
    /// pointer's metadata bits, and the engine states that it does not branch
    /// on this value: it is surfaced for logging and for the load barrier,
    /// neither of which this heap drives. Any answer is therefore inert, and
    /// the only question is which one is *honest*.
    ///
    /// `Z_REMAPPED` is what [`vaddr::ZGoodMask::new`] installs and what
    /// `flip_to_remap` returns — the "no mark parity is good; addresses are
    /// plain" state. It is also what [`mark::mark_color_for`] maps to `None`,
    /// so a metrics line reads "this cycle has no mark color" rather than
    /// claiming a `Marked0`/`Marked1` parity that nothing in this heap
    /// maintains. Returning a mark color instead would be a lie that a future
    /// barrier could act on.
    ///
    /// When colored pointers do land, this becomes a [`vaddr::ZGoodMask`]
    /// field on the heap and this method becomes `self.good_mask.good()`; the
    /// flip belongs to whoever owns the phase machine, not here.
    fn good_mask(&self) -> u64 {
        vaddr::Z_REMAPPED
    }

    /// Atomically claim the object at `addr` for this cycle.
    ///
    /// # Why `try_add_gc_flags` and not the STW marker's read-then-set
    ///
    /// `collect_garbage`'s loop tests `gc_flags() & GC_FLAG_MARKED` and then
    /// calls `add_gc_flags`, which is correct only because it is the sole
    /// thread running. With N workers plus mutators, two threads can both read
    /// "unmarked" and both proceed: the harmless outcome is a double scan, but
    /// the caller's contract here is *"`true` means you now own the obligation
    /// to scan it"*, so two `true`s for one object is only wasteful while a
    /// lost update is fatal — the object is pushed once, popped once, and its
    /// out-edges are never traced. Its children are then swept while it is
    /// alive and pointing at them.
    ///
    /// [`ObjectHeader::try_add_gc_flags`] is the exactly-once primitive that
    /// closes it: a CAS loop with `AcqRel`/`Acquire`, returning `true` iff
    /// *this* call transitioned the bits. Its sibling `add_gc_flags` returns
    /// nothing and cannot be used here.
    ///
    /// **Only [`GC_FLAG_MARKED`] is claimed**, never a set that also carries
    /// [`cratonvm_types::GC_FLAG_OLD_GEN`] or
    /// [`cratonvm_types::GC_FLAG_COMPACT`]. The claim is all-or-nothing
    /// by design, so a set containing a sticky flag that is already set loses
    /// forever and the object would never be marked at all.
    ///
    /// The engine calls [`Self::is_in_heap`] immediately before every
    /// `try_mark`; the null screen below is the only extra gate, since a
    /// header write through an unregistered address is the wild-write cascade
    /// this collector's `wild_skipped` counter exists to prevent.
    fn try_mark(&self, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        debug_assert!(
            self.registry.contains(addr as usize),
            "try_mark on an address the engine did not gate through is_in_heap"
        );
        self.header_ref(addr as usize as *mut u8)
            .try_add_gc_flags(GC_FLAG_MARKED)
    }

    /// Is the object at `addr` marked live for this cycle? **Query only.**
    ///
    /// The non-strong reference phase asks this about every referent to decide
    /// whether the `Reference` must be cleared. An implementation that marked
    /// as a side effect would answer "live, because I just made it live" for
    /// every referent, and no weak, soft, phantom or cleaner reference would
    /// ever be cleared again — a leak that no assertion in the reference tests
    /// would catch, because every one of them is satisfied by "the referent
    /// survived".
    ///
    /// This is [`Self::is_marked_addr`]'s predicate, read through
    /// [`Self::header_ref`] rather than `header_mut` so that concurrent
    /// callers do not mint aliasing `&mut`s.
    fn is_marked(&self, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        self.header_ref(addr as usize as *mut u8).gc_flags() & GC_FLAG_MARKED != 0
    }

    /// Report the object's **strong** out-edges, plus the three pin edges.
    ///
    /// # The referent hole
    ///
    /// Slot 0 of a registered `Weak`/`Soft`/`Phantom`/`Cleaner` `Reference` is
    /// its referent and must not be reported. Report it and the referent is
    /// reachable through its own `Reference`, so it is always marked, so
    /// `process_references` never clears it: `WeakReference.get()` never
    /// returns null and `Cleaner` never runs. Nothing throws, nothing logs —
    /// the program just leaks. The skip set is the cycle-long snapshot taken
    /// by [`Self::begin_concurrent_mark_cycle`]; see
    /// [`Self::concurrent_mark_skip_set`] for the deliberately-leaky fallback
    /// when no cycle is open.
    ///
    /// # The three pin edges
    ///
    /// `collect_garbage`'s loop pushes them by hand after every
    /// `enumerate_references`, and they are edges, not decoration: the class's
    /// loader ([`cratonvm_types::loader_pin`]), that loader's class mirrors
    /// ([`cratonvm_types::mirror_pin`]) and its metadata roots
    /// ([`cratonvm_types::metadata_pin`]). Dropping them lets a loader whose
    /// only remaining reference is one of its own loaded classes be swept,
    /// taking every mirror and method structure with it — a class-loader
    /// unloading bug that presents as a crash in unrelated reflection, not as
    /// a GC bug. They are reported here for the same reason and in the same
    /// order.
    ///
    /// The Arc handle is cloned and the read guard dropped **before** the walk
    /// begins, so no lock is held across the `f` callback — the engine calls
    /// back into its own stripe pusher from inside it, and a lock held across
    /// that is a cycle.
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        if addr == 0 {
            return;
        }
        let base = addr as usize;

        let skip_index = match self.concurrent_mark_skip_set() {
            Some(skip) => {
                if skip.contains(&base) {
                    Some(0usize)
                } else {
                    None
                }
            }
            None => {
                // No cycle open. Trace everything (including referents) and
                // say so ONCE — see `concurrent_mark_skip_set` for why leaking
                // beats dropping an edge, and why the latch is not optional.
                if !self.mark_ref_skip_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        target: "zgc",
                        "zgc concurrent mark: visit_refs ran with no skip-set snapshot; \
                         begin_concurrent_mark_cycle was not called, so weak/soft/phantom \
                         referents are being traced as STRONG edges and cannot be cleared"
                    );
                }
                None
            }
        };

        self.visit_strong_refs_at(base as *mut u8, skip_index, f);

        // The pin edges, in `collect_garbage`'s order. `class_id` is read
        // through the shared header view, like every other read here.
        let class_id = self.header_ref(base as *mut u8).class_id.as_u32();
        if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr(class_id) {
            f(loader as u64);
        }
        if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(base) {
            for m in mirrors {
                f(m as u64);
            }
        }
        if let Some(metadata) = cratonvm_types::metadata_pin::roots_for_loader(base) {
            for m in metadata {
                f(m as u64);
            }
        }
        // The native collection-overlay edges. `collect_garbage`'s serial loop
        // pushes these and this method did not, which would have been a
        // use-after-free the moment a coordinator drove a real collection
        // through here: an overlay is reachable ONLY through the Java
        // collection object that owns it, so a marker that skips this edge
        // sweeps live native-backed contents while the owner survives.
        //
        // The propagation must stay owner-based rather than unconditional —
        // `native_roots.rs::scan_collection_overlays` explicitly defers to this
        // loop having run, and rooting every overlay regardless of reachability
        // is what that deferral exists to avoid.
        for overlay_ref in crate::external_roots::external_roots_for_owner(base, Some(class_id)) {
            f(overlay_ref.as_ptr() as u64);
        }
    }

    /// The wild-pointer gate: is `addr` the base of a live allocation?
    ///
    /// A reference slot is raw bytes that a racing mutator may be mid-store
    /// into, so a child address read out of one can be anything at all. This
    /// is `collect_garbage`'s `registered.contains(&addr)` screen; the engine
    /// counts refusals in `ZMarkStats::off_heap_children`, which is that
    /// loop's `wild_skipped`. Interior pointers are refused too — an object
    /// base is the only thing whose first bytes are a header.
    fn is_in_heap(&self, addr: u64) -> bool {
        addr != 0 && self.registry.contains(addr as usize)
    }

    /// Bytes to charge to live-set accounting — [`ZgcRealHeap::alloc_size`],
    /// the same figure the sweep and [`ZgcRealHeap::walk_objects`] use.
    ///
    /// Returning the default `0` would compile and disable liveness
    /// accounting silently, which is how a relocation-set chooser comes to
    /// believe every page is empty.
    fn object_size(&self, addr: u64) -> usize {
        if addr == 0 {
            return 0;
        }
        // A header this collector cannot size contributes no accounted bytes.
        // `alloc_size`'s `None` is a corrupt/unresolvable header (see its doc),
        // and charging a 1 TiB sentinel to the live set would make every
        // occupancy figure derived from it meaningless.
        Self::alloc_size(self.header_ref(addr as usize as *mut u8)).unwrap_or(0)
    }

    /// The arena's base address, so a colored word's 42-bit **offset** can be
    /// turned back into the address `try_mark` and `is_in_heap` expect.
    ///
    /// This defaults to `None` in the trait, and the default's own doc explains
    /// why: `ZgcRealHeap`'s slots hold raw pointers and are never coloured, so
    /// `None` was "the *honest* answer for it today". That stopped being true
    /// when this heap grew a `ZBarrierContext` (Phase 4), because the barrier
    /// speaks offsets and `mark_live` must convert — with `None` it would have
    /// returned early on every call, i.e. a barrier wired to a marker that
    /// silently discards everything it is handed.
    ///
    /// The promise the trait extracts for `Some(base)` is that `base + offset`
    /// is an address this context's own `try_mark`/`is_in_heap` accept, and it
    /// holds here by construction: both go through `self.registry`, which is
    /// keyed by the same arena addresses `base_ptr` is the start of.
    fn heap_base(&self) -> Option<u64> {
        Some(self.arena.lock().base_ptr() as u64)
    }

    // `on_worker_start` / `on_worker_end` are left at their defaults on
    // purpose. They exist for per-thread context state (a TLAB-like scratch
    // buffer, a JFR thread binding); this heap has none — allocation is
    // serialized through one `Mutex<Arena>` and the marker allocates nothing
    // — so an override would be an empty function that later reads as a hook
    // somebody forgot to fill in.
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
        let ptr = self.alloc_raw_tlab(total).unwrap_or_else(|| {
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
        let ptr = self.alloc_raw_tlab(total).unwrap_or_else(|| {
            eprintln!("FATAL: ZGC(real): out of heap space for array ({total} bytes)");
            std::process::abort();
        });
        let len_u32 = u32::try_from(length).expect("array length exceeds u32::MAX");
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
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
        self.header(obj).kind()
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.header(obj).element_type()
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
            let v = unsafe { cratonvm_types::read_compact_field(ptr, storage, Ordering::Relaxed) };
            if !storage.is_reference() {
                return v;
            }
            // Un-box the wrapper `set_field` installs for a non-reference value
            // stored into a declared-REFERENCE slot. This heap's `set_field`
            // used to hand such a value to `write_compact_field`, whose
            // `FieldStorageKind::Reference` arm maps every non-`Object` value
            // to raw 0 — so the write was silently dropped to null while
            // `gen_heap` boxed it and the legacy 16-byte cell kept it verbatim.
            // ZGC has been the default since 2026-08-10, so the discarding
            // answer was the one most code actually got
            // (W7-84-primitive-in-reference-store.md).
            //
            // `autobox_payload` is the same validated read the reference-ARRAY
            // path here has always used; `crate::autobox` puts it behind the
            // process-wide latch so a run that never boxes never reaches it.
            return crate::autobox::unbox_reference_slot(
                v,
                |r| {
                    self.is_object_address(r.as_ptr() as usize)?;
                    // SAFETY: `is_object_address` confirmed `r` is a registered
                    // live allocation base, so its first HEADER_SIZE bytes are
                    // a header.
                    Some(unsafe { (*(r.as_ptr() as *const ObjectHeader)).class_id })
                },
                |r| <Self as GarbageCollector>::get_field(self, r, 0),
            );
        }
        // HIB-DCAST-LATEPHASE.1 (mutator side). `compact_object_field_storage`
        // answers `None` for TWO reasons and only the first licenses the
        // fall-through: (1) "this is a legacy object" — its contract, and the
        // 16-byte `Value` cell read below is correct; (2) "this IS a compact
        // object (`GC_FLAG_COMPACT`, set at allocation) but its
        // `(class_id, num_slots)` no longer resolves to a registered layout" —
        // a redefinition that changed the field count, a foreign layout
        // domain, or a desynced walk.
        //
        // Case (2) must NOT fall through. `alloc_object` sized this body with
        // `compact_object_body_size`, and `num_slots()` on a compact object is
        // the FIELD COUNT — so the `index < num_slots` screen above does not
        // bound `HEADER_SIZE + index * SLOT_SIZE`, and an entirely in-range
        // index still strides past the allocation. `object_field_size` already
        // treats this state as "this is not that object at all" and answers
        // `IMPLAUSIBLE_BODY_SIZE`; the hazard was recognised one layer down but
        // not at this call site.
        //
        // `is_compact_object(header)` — the per-object header bit, independent
        // of the registry — is what tells the two cases apart. It is the same
        // discriminator the GC walkers and this file's own
        // `ZCensusHeapView::reference_slots` already use. The walkers SKIP such
        // an object; an accessor cannot skip, so it serves nothing and says so.
        // Deliberately not a panic: the sibling accessors in `gen_heap.rs` /
        // `g1.rs` document why (a racing redefinition must not abort the JVM).
        if cratonvm_types::is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                class_id = ?header.class_id,
                "zgc::get_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — returning null rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return Value::Object(None);
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
            // A non-reference value into a declared-REFERENCE slot: box it,
            // rather than let `write_compact_field`'s `Reference` arm map it to
            // raw 0 and drop the write to null. Boxing is what `gen_heap` has
            // always done for fields, what all four heaps (this one included)
            // have always done for reference ARRAY elements, and what the
            // legacy 16-byte cell does by construction
            // (W7-84-primitive-in-reference-store.md).
            //
            // The allocation happens BEFORE `ptr` is taken. It does not have to
            // on this heap — ZGC does not move an object under its own mutator
            // — but the four implementations are kept in the same order so a
            // reader diffing them sees no difference to explain.
            let value = if storage.is_reference() {
                crate::autobox::box_for_reference_slot(value, header.class_id, index, |v| {
                    let wrapper = self.alloc_object(crate::heap::AUTOBOX_CLASS_ID, 1);
                    <Self as GarbageCollector>::set_field(self, wrapper, 0, v);
                    wrapper
                })
            } else {
                value
            };
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
        // HIB-DCAST-LATEPHASE.1, write half — see the matching note in
        // `get_field` above. This one is strictly worse than the read: striding
        // a 16-byte `Value` cell through a compact-sized body WRITES past the
        // allocation, corrupting whatever object the allocator placed next.
        // Drop the store rather than commit it somewhere unrelated.
        if cratonvm_types::is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                class_id = ?header.class_id,
                "zgc::set_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — dropping the store rather than \
                 writing a legacy 16-byte cell past its packed compact body \
                 (HIB-DCAST-LATEPHASE.1)",
            );
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
        debug_assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        header.array_length() as usize
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.header(obj);
        if header.kind() != ObjectKind::Array {
            return Err(index as i32);
        }
        if index >= header.array_length() as usize {
            return Err(index as i32);
        }
        let element_type = header.element_type();
        // ---- THE LOAD BARRIER, on a real read path (Phase 4) -------------
        //
        // A reference element goes through `load_barrier_slot`, which forwards
        // a relocated offset, publishes to the marker, and self-heals the slot
        // — but ONLY once the good mask says slots are coloured, which nothing
        // in a default run does. Until then it is one relaxed load and a
        // compare, and the element is read exactly as before.
        //
        // This arm exists because `read_prim_element`'s own Reference branch
        // applies `plausible_heap_pointer` and degrades an implausible word to
        // `Object(None)` — sites 6 and 7 of
        // `gc/tests/zgc_colored_word_degradation.rs`. A coloured word IS
        // deliberately implausible, so a coloured reference array read through
        // that branch silently nulls every live element. Barriering first is
        // how this backend goes through the barrier rather than around it;
        // that arm is shared with Generational and G1 and must not be edited.
        if element_type == ArrayElementType::Reference && self.load_barrier_armed() {
            let slot_addr =
                unsafe { obj.as_ptr().add(ARRAY_DATA_OFFSET) as usize + index * SLOT_SIZE };
            let val = match self.load_barrier_slot(slot_addr) {
                None => Value::Object(None),
                // SAFETY: the barrier returned a machine address it resolved
                // from a live slot; `from_raw` is the same construction the
                // unbarriered path performs.
                Some(addr) => {
                    Value::Object(Some(unsafe { ObjectRef::from_raw(addr as *mut u8) }))
                }
            };
            if let Value::Object(Some(boxed)) = val {
                if let Some(inner) = self.autobox_payload(boxed) {
                    return Ok(inner);
                }
            }
            return Ok(val);
        }
        // SAFETY: bounds check passed; data area starts at base + HEADER_SIZE.
        let val = unsafe {
            let base = obj.as_ptr().add(ARRAY_DATA_OFFSET);
            read_prim_element(base, index, element_type)
        };
        // Un-box the wrapper `set_array_element` installs for a non-Object
        // value stored into a reference array — see `autobox_payload`, and
        // `G1Collector::get_array_element` / `GenerationalHeap::
        // get_array_element_unboxing` for the same read. A Java-level
        // `Object[]` never holds an `AUTOBOX_CLASS_ID` object, so this cannot
        // change what an `aaload` observes.
        if element_type == ArrayElementType::Reference {
            if let Value::Object(Some(boxed)) = val {
                if let Some(inner) = self.autobox_payload(boxed) {
                    return Ok(inner);
                }
            }
        }
        Ok(val)
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        let header = self.header(obj);
        if header.kind() != ObjectKind::Array {
            return Err(index as i32);
        }
        if index >= header.array_length() as usize {
            return Err(index as i32);
        }
        let element_type = header.element_type();
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
                        // Arm the process-wide wrapper latch — see the matching
                        // note in `GenerationalHeap::set_array_element`.
                        crate::autobox::note_wrapper_created();
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
        // DBG: `CRATONVM_DBG_GC_STRESS=<bytes>` — collect every `<bytes>` of
        // allocation, bypassing every occupancy predicate below.
        //
        // **This backend ignored the flag entirely until 2026-08-14**, which
        // made the tree's standard GC-stress lever inert on its DEFAULT
        // collector: `gen_heap` honours it, ZGC did not, so a repro command
        // copied from any handoff page silently ran an ordinary workload. It
        // is how a moving-collector defect that needs many cycles to surface
        // gets one cycle in a suite class and hides.
        //
        // Bypassing `gc_rearm` is the point and is safe because this is a
        // diagnostic: the floor exists to stop a cycle-per-allocation storm
        // against a live set parked above the threshold, and storming is
        // exactly what the operator asked for.
        if let Some(step) = crate::gc_flags().gc_stress_bytes {
            if step > 0 {
                let last = self.gc_stress_mark.load(Ordering::Relaxed);
                if a.saturating_sub(last) >= step {
                    return true;
                }
            }
        }
        // Two independent reasons to collect, behind one shared anti-storm
        // floor:
        //
        //   * `a >= gc_threshold` — the classic LIVE-BYTES trigger.
        //   * `headroom_low`      — the arena can no longer serve a
        //                           `zgc_headroom_margin` request. On a heap
        //                           that never compacts this is the constraint
        //                           that actually binds, and it can be reached
        //                           with live bytes far below the threshold:
        //                           measured at 66.8% on `ZipContentTests`
        //                           while an 8 KB allocation was failing.
        //
        // Both keep the `gc_rearm` floor, so neither can fire a cycle per
        // allocation against a live set parked above the threshold.
        a >= self.gc_rearm.load(Ordering::Relaxed)
            && (a >= self.gc_threshold || self.headroom_low.load(Ordering::Relaxed))
    }

    fn collect_garbage(
        &self,
        _stw: &StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Pause clock for the `--verbose:gc` line at the end of this function.
        // Taken ONLY when logging is armed, so a quiet run pays one relaxed
        // load per collection and no clock read at all.
        let gc_started = self
            .gc_log_enabled
            .load(Ordering::Relaxed)
            .then(std::time::Instant::now);

        // ---- RETIRE EVERY TLAB -------------------------------------------
        // FIRST statement of the cycle, before the registry snapshot below and
        // therefore before anything reads or walks a heap byte. This is the
        // module invariant `gc::zgc::tlab` states, honoured here for the three
        // things it actually buys on THIS heap:
        //
        //  1. Chunk tails come back. `retire_all_tlabs` hands every
        //     `[cursor, chunk_end)` remainder to the arena free list, so the
        //     coalescer at the end of the sweep sees them and the bytes are
        //     reusable. Skipping this leaks up to `threads * chunk` per cycle
        //     into spans the sweep can never visit (a filler is not in
        //     `registry`), which exhausts the heap in a few dozen cycles.
        //  2. Every un-handed-out span becomes parseable, so the arena bytes
        //     below `cursor` satisfy the walkability contract the rest of this
        //     crate assumes of them.
        //  3. It is the single point at which "no mutator owns a private slice
        //     of this heap" is true, which is what makes the mark snapshot on
        //     the next line a complete picture.
        //
        // Registry completeness is NOT one of the three: a TLAB-served object
        // enters `registry` at hand-out (`ZArenaTlabRegistry`'s
        // `registry_batch` note explains why batching was reverted), so it is
        // already there before this call.
        self.retire_all_tlabs();

        // ---- Mark phase --------------------------------------------------
        // Snapshot the registry of all live-or-dead allocations: marking reads
        // object bytes in place and does not allocate, so it needs no arena
        // lock and must not pin the registry either. The copy also
        // serves as the mark-phase validation oracle (ZGC-4): child pointers
        // pushed by `enumerate_references` come from raw field bytes, and a
        // corrupt/stale slot must be SKIPPED, not have a mark bit written
        // through it (a wild `header_mut` write inside an innocent object —
        // or outside the arena entirely — then cascades as the garbage
        // "object" is re-parsed for more children).
        //
        // `snapshot()` is the direct replacement for the old
        // `self.registry.lock().clone()` and keeps its two properties exactly:
        // it is FROZEN (so the oracle cannot drift under the mark loop) and its
        // membership is O(1). On the bitmap arm it copies one bit per 8 arena
        // bytes rather than 8+ bytes per live object, so it is also strictly
        // cheaper than the set clone at any occupancy above ~1.5%.
        let registered: ZObjectStartsSnapshot = self.registry.snapshot();
        let all: Vec<usize> = registered.bases();

        // Clear all mark bits first (objects may carry a stale bit from a
        // prior cycle's survivors).
        for &base in &all {
            self.header_mut(base as *mut u8)
                .clear_gc_flags(GC_FLAG_MARKED);
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

        // ---- PARALLEL MARK (opt-in, Phase 3) -----------------------------
        //
        // `CRATONVM_ZGC_PARMARK=<n>` runs the strong closure on the real mark
        // engine instead of the serial loop below, at this same safepoint. The
        // two must produce the IDENTICAL mark set — that is what
        // `parallel_mark_marks_the_same_objects_as_the_serial_loop` asserts,
        // and it is the only claim that matters here, because the sweep that
        // follows cannot tell which loop set the bits.
        //
        // Left off by default: it is a worker-pool spawn and join per
        // collection (see `ZHeapMarkBridge` for why the pool cannot yet be
        // cached), so whether it is a net win is a measurement on a real
        // workload with a large live set, which is what the plan's Phase 3
        // exit criterion asks for and this switch exists to make possible.
        let parallel_workers = self.parallel_mark_workers();
        let mut work: Vec<usize> = Vec::new();
        let mut wild_skipped = 0usize;
        // Cleared when the driver refuses to certify a complete mark set, which
        // routes this cycle through the single-threaded marker below.
        let mut parallel_ok = true;
        // `>= 1`, not `> 1`: ONE worker still means the cycle is driven by
        // `zgc_concurrent`'s controller, which is Phase 3's first exit clause,
        // and it measures within 1.4% of the bespoke serial loop (95.7 ms vs
        // 94.4 ms on a 1M-object live set). Only `0` takes the hand-written
        // path, and that is the kill switch.
        if parallel_workers >= 1 {
            let root_addrs: Vec<u64> = roots.iter().map(|r| r.as_ptr() as u64).collect();
            // OPEN THE CYCLE FIRST. `mark_parallel_stw`'s doc calls this "the
            // caller's contract" and this caller violated it until 2026-08-13.
            //
            // Without the snapshot, `visit_refs` has no skip set and traces
            // slot 0 of every `Reference` as a STRONG edge — so every weak,
            // soft, phantom and final referent is reachable through its own
            // `Reference` and can never be cleared. `WeakReference` and
            // `Cleaner` silently stop working; it is a leak, not a crash, and
            // the only signal is a one-shot warning nobody reads.
            //
            // It stayed hidden because parallel marking was opt-in and no test
            // drove `collect_garbage` with it on. Turning it on by default is
            // what surfaced it, via `real_weak_ref_cleared_when_referent_dies`
            // — the serial path builds its own `ref_skip_objs` a few lines
            // below, so the two marking paths disagreed about the one thing
            // that must not differ between them.
            let _skip = self.begin_concurrent_mark_cycle();
            let driven = self.mark_parallel_stw(&root_addrs, parallel_workers);
            self.end_concurrent_mark_cycle();
            match driven {
                Some(stats) => {
                    self.parallel_mark_cycles.fetch_add(1, Ordering::Relaxed);
                    // `off_head_children` is this loop's `wild_skipped` under
                    // another name — the engine's own doc says so.
                    wild_skipped = stats.off_heap_children as usize;
                    tracing::debug!(
                        target: "zgc",
                        workers = parallel_workers,
                        marked = stats.objects_marked,
                        scanned = stats.objects_scanned,
                        off_heap_children = stats.off_heap_children,
                        "zgc STW mark complete, driven by zgc_concurrent"
                    );
                }
                // FAIL CLOSED. The driver could not certify a complete mark
                // set, and a sweep against one is a use-after-free. Fall
                // through to the single-threaded marker below, which starts
                // from the same roots and re-marks from scratch — the mark
                // bits already set are idempotent, so the fallback is a
                // superset of whatever the driver managed.
                None => {
                    self.parallel_mark_fallbacks.fetch_add(1, Ordering::Relaxed);
                    parallel_ok = false;
                }
            }
        }
        if !parallel_ok || parallel_workers == 0 {
        // Trace from roots. A work stack holds base addresses to visit.
        for r in roots.iter() {
            work.push(r.as_ptr() as usize);
        }
        while let Some(addr) = work.pop() {
            if addr == 0 {
                continue;
            }
            // ZGC-4: only registered allocation bases are objects. Roots are
            // pre-filtered by is_object_address, but CHILD pointers are raw
            // field bytes — skip anything that is not a current base.
            if !registered.contains(addr) {
                wild_skipped += 1;
                continue;
            }
            let header = self.header_mut(addr as *mut u8);
            if header.gc_flags() & GC_FLAG_MARKED != 0 {
                continue; // already visited
            }
            let class_id = header.class_id.as_u32();
            header.add_gc_flags(GC_FLAG_MARKED);
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
            // Same owner-based propagation Generational's non-moving young
            // marker and old-gen BFS already do (`gen_heap.rs`): a native
            // side-table entry is only reachable through the Java collection
            // object that owns it, so it must be traced from a CONFIRMED-live
            // owner, not rooted unconditionally for every overlay regardless
            // of reachability. `native_roots.rs`'s `scan_collection_overlays`
            // relies on this loop running before it defers to us.
            for overlay_ref in
                crate::external_roots::external_roots_for_owner(addr, Some(class_id))
            {
                work.push(overlay_ref.as_ptr() as usize);
            }
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
                if !registered.contains(addr) {
                    continue; // not a current allocation (already swept earlier)
                }
                let header = self.header_mut(addr as *mut u8);
                if header.gc_flags() & GC_FLAG_MARKED != 0 {
                    continue; // survived normally — stays registered, not finalized
                }
                resurrected.push(addr); // non-moving: address unchanged
                work.push(addr);
                while let Some(a) = work.pop() {
                    if a == 0 || !registered.contains(a) {
                        continue; // ZGC-4: same wild-child skip as the main loop
                    }
                    let h = self.header_mut(a as *mut u8);
                    if h.gc_flags() & GC_FLAG_MARKED != 0 {
                        continue;
                    }
                    let class_id = h.class_id.as_u32();
                    h.add_gc_flags(GC_FLAG_MARKED);
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
                    // Same owner-based overlay propagation as the main mark
                    // loop above — a resurrected finalizable object's own
                    // side-table entries must survive with it.
                    for overlay_ref in
                        crate::external_roots::external_roots_for_owner(a, Some(class_id))
                    {
                        work.push(overlay_ref.as_ptr() as usize);
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
                if registered.contains(addr) {
                    work.push(addr);
                }
            }
            while let Some(addr) = work.pop() {
                if addr == 0 || !registered.contains(addr) {
                    continue; // ZGC-4: same wild-child skip as the main loop
                }
                let header = self.header_mut(addr as *mut u8);
                if header.gc_flags() & GC_FLAG_MARKED != 0 {
                    continue;
                }
                header.add_gc_flags(GC_FLAG_MARKED);
                self.enumerate_references(addr as *mut u8, &mut work, skip_for(addr));
            }
        }

        // ---- Reference-slot census (diagnostic, default OFF) -------------
        // Placed HERE and nowhere else: this is the last point at which the
        // mark bits are final AND no object has been touched by the sweep. One
        // statement later the sweep clears every survivor's `GC_FLAG_MARKED`
        // and zero-fills every dead object, so a census run from inside or
        // after that loop would see an empty live set and a heap of zeroed
        // headers. It also holds no lock here — the registry guard was released
        // at the mark snapshot and the arena guard is not taken until the sweep
        // block below — which is what lets `for_each_live_object` take the
        // registry lock itself.
        //
        // Cost when disabled: one relaxed `AtomicBool` load and a not-taken
        // branch. `run_walk` re-checks the same gate and returns `None` before
        // touching this heap, so the guard is belt-and-braces; it is written
        // out anyway so the cost is visible at the callsite rather than one
        // module away.
        if self.slot_census.is_enabled() {
            // The result is also logged by `run_walk` itself (target `zgc`,
            // with `legacy_share` and `verdict`), and folded into the census's
            // cumulative counters. Callers that want the per-walk gauge read it
            // back through `slot_census()`.
            let _ = self.slot_census.run_walk(self);
        }

        // ---- Sweep phase -------------------------------------------------
        let mut dead: Vec<usize> = Vec::new();
        let mut bytes_copied = 0usize; // "retained" bytes (non-moving)
        let mut bytes_freed = 0usize;
        let mut objects_copied = 0usize;
        // Registered bases whose header could not be sized this cycle — see
        // the refusal in the sweep loop below.
        let mut unsizable = 0usize;
        {
            let mut arena = self.arena.lock();
            let arena_base = arena.base_ptr() as usize;
            for &base in &all {
                let header = self.header_mut(base as *mut u8);
                // A header this collector cannot size must not be swept. The
                // dead arm below `write_bytes`es `size` bytes and hands the
                // same span to `Arena::add_free_block`, whose only bound is a
                // `debug_assert!` — so passing `object_body_size`'s 1 TiB
                // corrupt-header sentinel through here memsets a terabyte from
                // `base` and free-lists memory that is not in the arena.
                //
                // Retaining it instead leaks one object until its layout
                // resolves again (or forever, if its class really is gone),
                // which is the fail-safe direction: it stays registered, stays
                // rooted conservatively, and is never handed out twice. The
                // one-shot `tracing::warn!` makes the leak visible rather than
                // silent.
                let Some(size) = Self::alloc_size(header) else {
                    unsizable += 1;
                    header.clear_gc_flags(GC_FLAG_MARKED);
                    continue;
                };
                if header.gc_flags() & GC_FLAG_MARKED != 0 {
                    // Survivor: clear the mark bit for next cycle, keep it.
                    header.clear_gc_flags(GC_FLAG_MARKED);
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
            //
            // The merge itself now lives on `Arena` (`coalesce_free_list`), so
            // this sweep and the last-resort merge `Arena::alloc` runs before
            // it returns `None` cannot drift apart.
            arena.coalesce_free_list();
            // The same two steps for the LARGE-OBJECT end. They are separate
            // calls, not a wider version of the two above, because a merge
            // that straddled the point where the two cursors meet would be
            // re-routed into one region by its offset alone — see
            // `Arena::coalesce_high`. Cheap: this list holds large objects, so
            // it is three orders of magnitude shorter than the low one.
            arena.coalesce_high();
            let reclaimed_high = arena.retract_high_cursor_into_free_head();
            if reclaimed_high != 0 {
                tracing::debug!(
                    target: "cratonvm::gc",
                    bytes = reclaimed_high,
                    high_cursor = arena.high_cursor(),
                    "zgc sweep: retracted the large-object cursor into a free head",
                );
            }
            // Un-bump a wholly-free tail. Coalescing above has made the topmost
            // span maximal, so this is one comparison — and it is the only
            // thing on a non-compacting heap that can restore a large
            // CONTIGUOUS region. Objects die young, so the top of the arena is
            // usually all garbage; without this the cursor is a one-way ratchet
            // and a 16 MB array becomes unservable forever once the process has
            // allocated its capacity, with 1.8 GB free and 15% live. See
            // `Arena::retract_cursor_into_free_tail`.
            let reclaimed_tail = arena.retract_cursor_into_free_tail();
            if reclaimed_tail != 0 {
                tracing::debug!(
                    target: "cratonvm::gc",
                    bytes = reclaimed_tail,
                    cursor = arena.used(),
                    "zgc sweep: retracted the bump cursor into a free tail",
                );
            }
        }

        if unsizable != 0 {
            ZGC_UNSIZABLE_OBJECTS.fetch_add(unsizable, Ordering::Relaxed);
            if !ZGC_UNSIZABLE_WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    count = unsizable,
                    "zgc sweep: registered object(s) whose header could not be \
                     sized (compact layout unresolvable, or an implausible \
                     legacy num_slots) — retained rather than zeroed and \
                     free-listed. See ZgcRealHeap::alloc_size.",
                );
            }
        }
        // Prune DEAD bases from the registry IN PLACE (never wholesale-
        // replace it with the mark snapshot's survivors): an allocation
        // registered by another path between the mark snapshot and this
        // publish would be erased by a replacement — leaking its memory
        // forever (unsweepable) and, worse, making is_object_address deny it
        // so conservative rooting drops it while reachable.
        for d in &dead {
            self.registry.remove(*d);
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
        // Lower the allocatable-space latch: the sweep above has just
        // free-listed every dead object and coalesced the result into maximal
        // spans, so whatever headroom this heap can have, it has now.
        //
        // Cleared unconditionally rather than recomputed. If headroom is STILL
        // below the margin the very next allocation re-arms it — and that path
        // is correct where a recompute would be fragile, because it re-measures
        // the arena the caller actually failed against instead of a snapshot
        // taken here. Re-arming cannot storm: `gc_rearm` was just set above
        // `allocated`, so the trigger waits for genuinely new allocation.
        self.headroom_low.store(false, Ordering::Relaxed);
        // Disarm the native-allocation-pressure latch here, at the same point
        // `gc_rearm` is recomputed: the collection the latch asked for has now
        // happened, and the re-arm floor just published above is the only thing
        // that decides whether the next allocation may raise it again. Mirrors
        // G1's clear at the end of its cycle (`g1.rs:8392`).
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
        // Same point, same reasoning, for the hard-refusal latch. Note this is
        // NOT done in `clear_native_alloc_pressure`: that runs on the boundary's
        // "gates said no" path too, and lowering the hard bit there would throw
        // away the one signal this whole mechanism exists to carry.
        self.hard_alloc_failure.store(false, Ordering::Relaxed);
        // Re-arm the stress trigger against the post-sweep live figure.
        self.gc_stress_mark
            .store(self.allocated.load(Ordering::Relaxed), Ordering::Relaxed);
        let cycle = self.gc_count.fetch_add(1, Ordering::Relaxed) + 1;

        // Phase 2.2: one post-sweep fragmentation reading per collection.
        //
        // Here rather than inside the sweep's own arena scope above, and the
        // second `lock()` is deliberate: this reads the arena AFTER the sweep,
        // the high-end coalesce, and both cursor retractions, which is the
        // state a workload actually allocates against and therefore the only
        // one worth ratcheting on. The world is still stopped, so the two
        // scopes see identical bytes and the extra acquire is uncontended.
        {
            let arena = self.arena.lock();
            self.sample_frag_gauge(&arena, cycle);
        }

        // Per-collection `--verbose:gc` line. One `eprintln!` and no
        // `tracing` twin on purpose: the defect this closes is that a run with
        // JUST `--verbose:gc` (no `RUST_LOG`) produced zero output for the
        // whole run, and stderr is the only sink that is on in that case.
        //
        // Format: the `[GC]` prefix and the `zgc-real:` tag match this
        // backend's existing shutdown line in `vm_heap.rs::print_gc_summary`
        // (`[GC] zgc-real: collections=… occupancy=…/… bytes`), and the
        // key names are G1's `[GC-STAT]` keys verbatim (`pause_us=`,
        // `objects_copied=`, `bytes_copied=`, `bytes_freed=` — `g1.rs:6652`)
        // so the two backends' per-collection lines extract identically when
        // compared by hand. `objects_copied`/`bytes_copied` are survivors
        // retained in place on this non-moving collector, not relocations.
        //
        // TODO(zgc): `gc::zgc::metrics::ZgcMetrics::format_cycle_line` already
        // renders the richer OpenJDK-shaped line (before(%)->after(%),
        // reclaimed, STW/concurrent split, allocation stalls) for exactly this
        // callsite. Adopting the metrics module is a larger step — it has to be
        // fed from every phase, not just here — so this line stays standalone
        // until then; replace it wholesale at that point.
        if let Some(started) = gc_started {
            let pause_us = started.elapsed().as_micros();
            eprintln!(
                "[GC] zgc-real: cycle={cycle} pause_us={pause_us} \
                 objects_copied={objects_copied} bytes_copied={bytes_copied} \
                 bytes_freed={bytes_freed} occupancy={bytes_copied}/{cap} bytes",
            );
        }

        // ---- COMPACTION (opt-in, Phase 4) --------------------------------
        //
        // Normally non-moving: no object changes address, roots and external
        // references need no fix-up, and the pointer map is empty. That is
        // still what every default run does.
        //
        // With `CRATONVM_ZGC_RELOCATE=1` the sweep is followed by a
        // stop-the-world slide that compacts the small-object end and returns
        // a NON-EMPTY pointer map. Three things make that safe to have here:
        //
        //  * the sub-flag, which is the only thing keeping it off a user's
        //    machine now that the `zgc` Cargo feature is default-ON;
        //  * `zgc_relocation_permitted` in `vm_init`, which refuses whenever
        //    the JIT is enabled -- compiled code loads reference fields with
        //    no load barrier and would be handed stale pointers;
        //  * the caller's `StopTheWorldToken`, already proven above.
        //
        // The `roots` slice is rewritten in place here, because a root that
        // still names a pre-slide address is a dangling pointer the moment
        // this function returns -- and unlike a field slot, nothing downstream
        // would rewrite it.
        let mut pointer_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        if self.relocation_requested() {
            // The POST-sweep registry: the sweep has already removed every
            // dead base from it, so what remains is exactly the live set --
            // and unlike the mark bits, it is still there. `registered` (the
            // pre-sweep snapshot) would include the objects just reclaimed.
            let live_now: Vec<usize> = self.registry.snapshot().bases();
            let (moved, reclaimed, map) = self.relocate_stw(&live_now);
            if moved > 0 {
                self.compaction_cycles.fetch_add(1, Ordering::Relaxed);
                self.objects_relocated.fetch_add(moved, Ordering::Relaxed);
                for r in roots.iter_mut() {
                    if let Some(to) = map.get(&(r.as_ptr() as usize)) {
                        // SAFETY: `to` is an object base this slide just wrote,
                        // inside the arena, 8-byte aligned by construction.
                        *r = unsafe { ObjectRef::from_raw(*to as *mut u8) };
                    }
                }
                tracing::debug!(
                    target: "zgc",
                    moved,
                    reclaimed,
                    "zgc STW compaction complete"
                );
            }
            pointer_map = map;
        }
        monitors.remap_after_gc(&pointer_map);
        // ZGC-3: hand the collector's dead-address list to the registry prune —
        // reclaims monitor/cas-lock entries and prevents a recycled address
        // from inheriting a dead object's monitor.
        //
        // **RE-SCREENED AGAINST THE POST-SLIDE REGISTRY SINCE 2026-08-14, and
        // without this the compacting configuration frees a LIVE object's
        // monitor.** `MonitorCleanup::prune_dead` documents `dead` as EXACT —
        // "this address was a live allocation base before this collection and
        // its memory is now freed, so no thread can read its mark word" — and
        // on that licence it calls `Monitor::release_mark_ref`, dropping the
        // strong reference the mark word itself owns.
        //
        // Compaction breaks that precondition in the commonest way there is:
        // survivors slide DOWN into the space vacated by dead objects, so a
        // dead base is very likely to be a live object's new base by the time
        // this runs. `remap_after_gc` has already moved that object's monitor
        // to its new address, and the unfiltered prune then removes it again
        // and releases the mark-word reference — leaving the live object's
        // mark word pointing at a possibly-freed `Monitor`. That is a
        // use-after-free reachable from any `synchronized` block, on an object
        // chosen by where the slide happened to put it, which is why it
        // presents as scattered SIGSEGVs in unrelated subsystems rather than
        // as a locking bug.
        //
        // The registry was rebuilt by the slide (old bases removed, new bases
        // inserted), so `contains` is exactly "this address is a live base
        // NOW" — which is the predicate `prune_dead`'s doc assumes and the
        // pre-slide list can no longer supply.
        let dead: Vec<usize> = if pointer_map.is_empty() {
            dead
        } else {
            let before = dead.len();
            let screened: Vec<usize> = dead
                .into_iter()
                .filter(|d| !self.registry.contains(*d))
                .collect();
            let resurrected = before - screened.len();
            if resurrected > 0 {
                tracing::debug!(
                    target: "zgc",
                    resurrected,
                    "zgc compaction: dead addresses now occupied by slid survivors,                      withheld from the monitor prune"
                );
            }
            screened
        };
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

    /// Card the written object when it lives in an old page --
    /// `zgc::remembered`, Phase 4.
    ///
    /// This arm said "Non-generational, non-concurrent: nothing to record"
    /// and was empty. It is the hook every reference store in the VM already
    /// reaches, so it is where a generational collector's card barrier
    /// belongs; see [`ZgcRealHeap::note_ref_store`] for why a card names an
    /// object rather than a slot, and for the cost while no page is old (one
    /// `is_empty` check).
    ///
    /// `stored_value` is deliberately not consulted. Filtering to
    /// "the value is a young reference" would card fewer objects, but it
    /// needs the value's page, which is another lock on the store path to
    /// avoid a re-scan that only costs anything at cycle time. Over-carding
    /// is safe -- a stale or unnecessary card makes a young cycle scan an
    /// object it did not need to -- while under-carding is a use-after-free.
    fn write_barrier(&self, obj: ObjectRef, _stored_value: Value) {
        self.note_ref_store(obj.as_ptr() as usize);
    }

    fn allocated_bytes(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // TLAB chunk recycling
    // ------------------------------------------------------------------

    /// The chunk must be a function of how many threads are claiming one, and
    /// the total claimed must stay bounded. The numbers are the ones from the
    /// `TestNonBlockingAPI` failure: `-Xmx 2g`, ~4,000 threads, a flat 512 KiB
    /// chunk — `4,000 x 512 KiB` is the whole heap.
    #[test]
    fn the_chunk_shrinks_so_total_reservation_stays_bounded() {
        const CAP: usize = 2 * 1024 * 1024 * 1024;
        let reg = ZArenaTlabRegistry::for_capacity(CAP);
        let budget = CAP / ZGC_TLAB_RESERVATION_SHARE;

        // A handful of threads: unchanged, the ceiling binds.
        reg.live_slots.store(8, Ordering::Relaxed);
        assert_eq!(
            reg.chunk_bytes_now(CAP),
            ZGC_TLAB_MAX_CHUNK,
            "an ordinary thread count must see exactly the chunk it saw before",
        );

        // The workload that produced the defect.
        reg.live_slots.store(4000, Ordering::Relaxed);
        let chunk = reg.chunk_bytes_now(CAP);
        assert!(
            chunk < ZGC_TLAB_MAX_CHUNK,
            "4000 threads must not each get a full chunk (got {chunk})",
        );
        assert!(
            chunk * 4000 <= budget,
            "total reservation {} must stay inside the budget {budget}",
            chunk * 4000,
        );
        // The old behaviour, stated so the regression is unmistakable: a flat
        // 512 KiB chunk at this thread count claimed 97.7% of a 2 GiB heap.
        assert!(
            ZGC_TLAB_MAX_CHUNK * 4000 * 10 >= CAP * 9,
            "precondition: a flat chunk at this thread count claimed the heap",
        );

        // Never below the floor, however many threads there are.
        reg.live_slots.store(10_000_000, Ordering::Relaxed);
        assert_eq!(
            reg.chunk_bytes_now(CAP),
            crate::tlab::min_tlab_size(),
            "the floor holds",
        );
    }

    /// A heap too small for TLABs stays too small for TLABs: the adaptive
    /// sizer decides how big a chunk is, never whether there is one.
    #[test]
    fn a_heap_with_no_tlabs_gets_no_chunk_from_the_adaptive_sizer() {
        let reg = ZArenaTlabRegistry::for_capacity(8 * 1024);
        assert_eq!(reg.config.initial_chunk, 0, "precondition: TLABs are off here");
        reg.live_slots.store(1, Ordering::Relaxed);
        assert_eq!(reg.chunk_bytes_now(8 * 1024), 0);
    }

    /// The exact numbers from the `TestNonBlockingAPI` failure, 2026-08-13.
    ///
    /// A 512 KiB chunk that held one 96-byte `AQS$ConditionNode` comes back as
    /// a 524,192-byte span. Under the fixed-size request that span was
    /// unusable for a refill **forever**, so every refill in the process had to
    /// bump; the arena reached capacity with 1.96 GB sitting on the free list
    /// in 3,828 pieces that were each one small object short of reusable.
    ///
    /// Ninety-six bytes is the whole defect, so that is the number this test
    /// asserts on.
    #[test]
    fn a_chunk_remnant_one_object_short_is_still_worth_taking() {
        const CHUNK: usize = 512 * 1024;
        const NODE: usize = 96;
        assert_eq!(
            recycled_chunk_size(CHUNK, 64, CHUNK - NODE),
            Some(CHUNK - NODE),
            "a chunk short by one AQS node must still be recycled",
        );
    }

    /// The other three answers the decision has to give, so a change that
    /// makes the case above pass by always saying yes fails here.
    #[test]
    fn the_recycled_chunk_decision_refuses_the_three_cases_it_must() {
        const CHUNK: usize = 512 * 1024;
        // 1. Nothing on the free list: ask for a full chunk.
        assert_eq!(recycled_chunk_size(CHUNK, 64, 0), None);
        // 2. Below the floor (`want / 8` = `max_tlab_alloc`): a buffer that
        //    small is churn, not a buffer.
        assert_eq!(recycled_chunk_size(CHUNK, 64, CHUNK / 8 - 8), None);
        assert_eq!(
            recycled_chunk_size(CHUNK, 64, CHUNK / 8),
            Some(CHUNK / 8),
            "the floor itself is acceptable",
        );
        // 3. At or above a full chunk: there is nothing to decide, the ordinary
        //    `alloc(want)` finds it.
        assert_eq!(recycled_chunk_size(CHUNK, 64, CHUNK), None);
        assert_eq!(recycled_chunk_size(CHUNK, 64, CHUNK * 4), None);
    }

    /// The guarantee `tlab_refill`'s contract rests on: whatever size comes
    /// back, the request that forced the refill still fits in it. A recycled
    /// chunk shorter than `need` would hand the caller a buffer its own
    /// allocation cannot use, and the caller's `tlab.alloc(size)?` would then
    /// fail with a live chunk installed.
    #[test]
    fn a_recycled_chunk_is_never_shorter_than_the_request_that_forced_it() {
        const CHUNK: usize = 512 * 1024;
        for need in [8usize, 1024, CHUNK / 8, CHUNK / 8 + 8, CHUNK / 2] {
            for largest in [0usize, 4096, CHUNK / 8, CHUNK / 2, CHUNK - 96, CHUNK, CHUNK * 2] {
                if let Some(size) = recycled_chunk_size(CHUNK, need, largest) {
                    assert!(
                        size >= need,
                        "need={need} largest={largest} produced a {size}-byte chunk",
                    );
                    assert!(size <= largest, "cannot carve more than the block holds");
                    assert_eq!(size % ZGC_TLAB_ALIGN, 0, "chunks stay on the object grid");
                }
            }
        }
    }

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
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
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

    // ------------------------------------------------------------------
    // Native-allocation-pressure latch + `--verbose:gc` gate
    // ------------------------------------------------------------------

    /// Fill `heap` with fresh objects until `allocated` crosses `gc_threshold`,
    /// returning them so the caller decides whether they stay live. Bounded so
    /// a mis-sized heap fails the test rather than hanging (or, worse, driving
    /// the infallible allocator into its `abort()`).
    fn alloc_up_to_threshold(heap: &ZgcRealHeap) -> Vec<ObjectRef> {
        let mut held = Vec::new();
        let mut guard = 0;
        while heap.allocated_bytes() < heap.gc_threshold {
            held.push(heap.alloc_object(ClassId::new(11), 4));
            guard += 1;
            assert!(guard < 100_000, "threshold never crossed — heap mis-sized");
        }
        // A small deliberate overshoot so a caller that keeps these live is
        // unambiguously PARKED above the threshold, not sitting on the exact
        // boundary where a byte of accounting slop decides the test.
        for _ in 0..8 {
            held.push(heap.alloc_object(ClassId::new(11), 4));
        }
        held
    }

    #[test]
    fn real_native_alloc_pressure_arms_on_the_threshold_crossing() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        assert!(
            !heap.native_alloc_pressure(),
            "latch must start down on a fresh heap"
        );

        // A single small object is nowhere near the 75% trigger.
        let _ = heap.alloc_object(ClassId::new(11), 1);
        assert!(
            !heap.native_alloc_pressure(),
            "latch armed below the gc_threshold"
        );

        let _held = alloc_up_to_threshold(&heap);
        assert!(
            heap.native_alloc_pressure(),
            "latch must arm once allocation crosses gc_threshold — this is the \
             signal that lets a native call collect on the mutator's behalf \
             instead of the infallible allocator reaching abort()"
        );
    }

    #[test]
    fn real_native_alloc_pressure_is_disarmed_by_a_collection() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let held = alloc_up_to_threshold(&heap);
        assert!(heap.native_alloc_pressure());

        // Drop every root: the whole set is garbage, so the cycle reclaims it.
        drop(held);
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots: [ObjectRef; 0] = [];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        assert!(
            !heap.native_alloc_pressure(),
            "the collection the latch asked for has happened; it must disarm"
        );
        assert_eq!(heap.gc_count(), 1);
    }

    #[test]
    fn real_native_alloc_pressure_manual_note_and_clear_round_trip() {
        // The externally-noted edge (`VmHeap::note_young_spill_pressure`):
        // a wrapper that spilled below the trigger and cannot collect itself.
        let heap = ZgcRealHeap::new();
        assert!(!heap.native_alloc_pressure());
        heap.note_native_alloc_pressure();
        assert!(heap.native_alloc_pressure());
        heap.clear_native_alloc_pressure();
        assert!(!heap.native_alloc_pressure());
        // Idempotent: the consumer clears unconditionally, including when its
        // own gates declined to act.
        heap.clear_native_alloc_pressure();
        assert!(!heap.native_alloc_pressure());
    }

    #[test]
    fn real_gc_logging_is_off_by_default_and_toggles() {
        let heap = ZgcRealHeap::new();
        assert!(
            !heap.gc_log_enabled.load(Ordering::Relaxed),
            "per-collection logging must stay off unless --verbose:gc asked"
        );
        heap.enable_gc_logging();
        assert!(heap.gc_log_enabled.load(Ordering::Relaxed));
        heap.disable_gc_logging();
        assert!(!heap.gc_log_enabled.load(Ordering::Relaxed));
    }

    /// REGRESSION (GC storm): a live set parked ABOVE the static 75% threshold
    /// must not make repeated `needs_gc` polling produce back-to-back
    /// collections. This is the livelock the `gc_rearm` field exists to
    /// prevent — `needs_gc` once latched permanently true and `maybe_gc`
    /// (polled after every allocation bytecode) ran a full STW mark-sweep per
    /// allocation, with no OOME ever surfacing. The `native_alloc_pressure`
    /// latch reuses the SAME predicate, so it must not reopen it.
    ///
    /// Counts only — no wall-clock assertions.
    #[test]
    fn real_parked_live_set_does_not_produce_back_to_back_collections() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        // Every object stays reachable through `held`, so the sweep reclaims
        // nothing and post-GC `allocated` remains above `gc_threshold`.
        let mut held = alloc_up_to_threshold(&heap);
        assert!(heap.needs_gc());
        assert!(heap.native_alloc_pressure());

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut collections = 0usize;
        for _ in 0..64 {
            if heap.needs_gc() {
                heap.collect_garbage(&stw, &mut held, &NoMonitors);
                collections += 1;
            }
        }

        assert_eq!(
            collections, 1,
            "64 polls over a parked live set ran {collections} collections; \
             one is the storm-free answer (the first poll collects, the re-arm \
             floor published by that sweep silences the other 63)"
        );
        assert_eq!(heap.gc_count(), 1);
        assert!(
            heap.allocated_bytes() >= heap.gc_threshold,
            "the live set must still be parked above the static threshold, or \
             this test is not exercising the storm shape at all"
        );
        assert!(
            !heap.needs_gc(),
            "needs_gc must stay false above the threshold until NEW allocation \
             clears gc_rearm"
        );
        assert!(
            !heap.native_alloc_pressure(),
            "the latch must stay down too — it shares needs_gc's predicate, so \
             a latch that could re-arm here would reopen the storm through the \
             native-call boundary instead of through maybe_gc"
        );
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

    /// `object_body_size` answers `IMPLAUSIBLE_BODY_SIZE` (1 TiB) — not `0` —
    /// for a `GC_FLAG_COMPACT` object whose `(class_id, num_slots)` no longer
    /// resolves to a registered layout (a class unloaded by
    /// `unregister_class_layout` while an instance is still registered here).
    /// The sentinel is a REFUSAL that every caller is required to bounds-check.
    ///
    /// `ZgcRealHeap::alloc_size` returned it verbatim, and the sweep fed it to
    /// `std::ptr::write_bytes(base, 0, size)` and to `Arena::add_free_block`
    /// (whose only bound is a `debug_assert!`, compiled out in release). One
    /// such object therefore memset a terabyte from its own base.
    #[test]
    fn alloc_size_refuses_a_compact_object_with_no_registered_layout() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let obj = heap.alloc_object(ClassId::new(999_996), 4);

        // Sizable while it is an ordinary legacy object.
        let header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
        assert_eq!(
            ZgcRealHeap::alloc_size(header),
            Some(HEADER_SIZE + 4 * SLOT_SIZE)
        );

        // No layout is registered for this class id, so flipping the per-object
        // compact bit reproduces the racing state (header says compact, the
        // registry cannot serve it) without a live class unload.
        header.add_gc_flags(cratonvm_types::GC_FLAG_COMPACT);
        assert_eq!(
            ZgcRealHeap::alloc_size(header),
            None,
            "an unresolvable compact header must be refused, not sized at \
             HEADER_SIZE + 1 TiB"
        );
    }

    /// The sweep must RETAIN an object it cannot size rather than zero it and
    /// hand its span to the arena. Retaining leaks one object; the alternative
    /// is a 1 TiB `write_bytes` and a free block outside the arena.
    #[test]
    fn the_sweep_retains_an_object_it_cannot_size() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let live = heap.alloc_object(ClassId::new(1), 2);
        let doomed = heap.alloc_object(ClassId::new(999_995), 4);
        let doomed_addr = doomed.as_ptr() as usize;
        // Sentinel bytes in the body: if the sweep zeroed this object we would
        // see it, and a 1 TiB memset would have taken the whole process with it.
        heap.set_field(doomed, 0, Value::Int(0x5A5A_5A5A));

        let header = unsafe { &*(doomed.as_ptr() as *const ObjectHeader) };
        header.add_gc_flags(cratonvm_types::GC_FLAG_COMPACT);

        let before = ZGC_UNSIZABLE_OBJECTS.load(Ordering::Relaxed);
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        // `doomed` is unreachable from the root set, so an unguarded sweep
        // would take the dead arm.
        let mut roots = [live];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        assert!(
            ZGC_UNSIZABLE_OBJECTS.load(Ordering::Relaxed) > before,
            "the refusal must be counted, not silent"
        );
        assert!(
            heap.registry.contains(doomed_addr),
            "an unsizable object stays registered — dropping it from the \
             registry would make is_object_address deny it while conservative \
             rooting still reaches it"
        );
        header.clear_gc_flags(cratonvm_types::GC_FLAG_COMPACT);
        assert_eq!(
            heap.get_field(doomed, 0),
            Value::Int(0x5A5A_5A5A),
            "the body must not have been zeroed"
        );
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

    /// A refusal and the occupancy predicate are **different questions**, and
    /// this is the test that says so.
    ///
    /// `needs_gc()` asks "have enough live bytes accumulated". A non-compacting
    /// arena refuses a request when no single hole is big enough, which it can
    /// do at any occupancy at all — including zero. Until 2026-08-13 the
    /// `safe_native_call` boundary consumed the refusal latch through a gate
    /// that re-asked `needs_gc()`, so in exactly this state the signal was
    /// cleared without a collection ever running.
    ///
    /// The assertions are deliberately paired: `!needs_gc()` is what makes
    /// `hard_alloc_failure()` worth having, and a version of this test that
    /// dropped it would still pass against a latch wired to the occupancy
    /// trigger — i.e. against no fix at all.
    #[test]
    fn a_refused_request_latches_a_signal_the_occupancy_trigger_cannot_see() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        assert!(!heap.needs_gc());
        assert!(!heap.hard_alloc_failure());

        // No arena of this size can ever serve this, at any occupancy.
        let refused = heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 1024 * 1024);
        assert!(refused.is_none(), "a 1 MB array must not fit a 64 KB arena");

        assert!(
            !heap.needs_gc(),
            "the occupancy trigger must still say no — that is the whole point"
        );
        assert!(
            heap.hard_alloc_failure(),
            "a refused request must latch the hard signal"
        );
    }

    /// The hard latch is lowered by the collection it asked for, so one
    /// refusal buys one cycle and a healthy heap does not carry the bit.
    #[test]
    fn a_collection_lowers_the_hard_allocation_failure_latch() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        assert!(heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 1024 * 1024)
            .is_none());
        assert!(heap.hard_alloc_failure());

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots: [ObjectRef; 0] = [];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        assert!(
            !heap.hard_alloc_failure(),
            "the cycle the latch asked for has run; the bit must not persist"
        );
    }

    // -- Phase 2.2: the fragmentation ratchet -----------------------------

    /// A run that never met the sampling condition reports **no reading**, not
    /// a perfect one.
    ///
    /// This is the vacuous-green guard for the whole gauge. `worst_permille`
    /// starts at a `usize::MAX` sentinel precisely so that "never sampled" and
    /// "sampled and scored 1000" cannot be confused, and this test is what
    /// stops someone simplifying that sentinel into a plain `1000`.
    #[test]
    fn the_frag_gauge_reports_no_reading_rather_than_a_perfect_one() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let g = heap.frag_gauge();
        assert_eq!(g.samples, 0);
        assert_eq!(
            g.worst_permille, None,
            "an unsampled gauge must not read as a perfect score"
        );
    }

    /// A collection on a mostly-empty heap samples, and scores well.
    #[test]
    fn a_collection_with_room_to_spare_takes_a_reading() {
        let heap = ZgcRealHeap::with_capacity(1024 * 1024);
        // A handful of objects, all dead by the time we collect.
        for _ in 0..16 {
            heap.alloc_object(ClassId::new(1), 4);
        }
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots: [ObjectRef; 0] = [];
        heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        let g = heap.frag_gauge();
        assert_eq!(g.samples, 1, "an empty-ish heap must be sampled");
        let worst = g.worst_permille.expect("sampled, so there is a reading");
        assert!(
            worst > ZGC_FRAG_FLOOR_PERMILLE,
            "a heap with one contiguous run of free space is not fragmented; \
             got {worst} permille"
        );
        assert!(
            g.free_permille >= ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE,
            "the recorded sample must satisfy the condition it was taken under"
        );
    }

    /// **A full heap is not a fragmented heap**, and the gauge must not say it
    /// is.
    ///
    /// This is the test that makes [`ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE`] load-
    /// bearing rather than decorative. Fill a heap with LIVE objects, collect,
    /// and the largest free block is legitimately tiny — a gauge without the
    /// condition would ratchet to nearly zero here and fire its floor warning
    /// on the most ordinary workload there is.
    ///
    /// The exact edit that trips it: delete the `free_permille <` early return
    /// in `sample_frag_gauge`.
    #[test]
    fn a_heap_that_is_merely_full_is_not_recorded_as_fragmented() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        // Keep everything alive, so the sweep frees nothing — and allocate
        // until the arena genuinely refuses, so the post-sweep free share is
        // really below the sampling condition. A fixed object count would
        // leave room and quietly test nothing.
        // Pin the TLAB off for this fixture. Not because the TLAB is the
        // subject — it is not — but because `zgc_tlab_enabled_by_default`
        // reads a process-wide flag on every heap construction, so a peer test
        // holding a `FlagOverride` decides how much of this arena a refill
        // claims and therefore where the fill loop below stops. Without this
        // the test passed alone and failed in the full suite, which is a
        // FIXTURE defect masquerading as a gauge defect.
        heap.set_tlab_enabled(false);
        let mut live: Vec<ObjectRef> = Vec::new();
        while let Some(o) = heap.try_alloc_object(ClassId::new(1), 8) {
            live.push(o);
        }
        assert!(
            live.len() > 16,
            "the fixture must actually fill the arena to test anything; got {}",
            live.len()
        );
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        heap.collect_garbage(&stw, &mut live, &NoMonitors);

        // Precondition, asserted rather than assumed: this test says nothing
        // unless the heap really did end up full. If a future change makes the
        // fill loop stop early, the assertion below would pass for the wrong
        // reason and the test would become a vacuous green.
        let free_permille = heap.arena_free_permille_for_test();
        assert!(
            free_permille < ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE,
            "fixture did not fill the heap: {free_permille} permille free"
        );

        let g = heap.frag_gauge();
        assert_eq!(
            g.samples, 0,
            "a collection that left <25% of the heap free says nothing about \
             fragmentation and must not be counted as a reading"
        );
        assert_eq!(g.worst_permille, None);
    }

    // -- Phase 3: the mutator ingress -------------------------------------

    /// Disarmed, the barrier publishes **nothing** — including for a live,
    /// registered address it would otherwise capture.
    ///
    /// This is the test that pins the cost argument. The barrier sits on the
    /// store path of every Java program this VM runs, and the claim that a
    /// non-concurrent run pays "one relaxed load" is only true while the
    /// disarmed path reaches no registry lookup, no mutex and no counter.
    #[test]
    fn the_satb_barrier_is_inert_while_no_cycle_is_marking() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let obj = heap.alloc_object(ClassId::new(1), 4);
        assert!(!heap.mark_active());

        heap.satb_pre_barrier(obj.as_ptr() as usize);

        assert_eq!(
            heap.mark_ingress_pushes(),
            0,
            "a disarmed barrier must publish nothing"
        );
        let mut drained = Vec::new();
        assert_eq!(heap.drain_mark_ingress(&mut drained), 0);
        assert!(drained.is_empty());
    }

    /// Armed, the barrier publishes the overwritten reference — which is the
    /// whole point of a snapshot-at-the-beginning barrier.
    #[test]
    fn an_armed_satb_barrier_publishes_the_overwritten_reference() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let obj = heap.alloc_object(ClassId::new(1), 4);
        heap.set_mark_active(true);

        heap.satb_pre_barrier(obj.as_ptr() as usize);

        assert_eq!(heap.mark_ingress_pushes(), 1);
        let mut drained = Vec::new();
        assert_eq!(heap.drain_mark_ingress(&mut drained), 1);
        assert_eq!(drained, vec![obj.as_ptr() as usize as u64]);
    }

    /// Two things the armed barrier must still refuse: a null overwrite (no
    /// edge to preserve) and an address this heap never handed out.
    ///
    /// The second is the same gate `ZMarkContext::is_in_heap` applies to every
    /// child pointer. Publishing a foreign address would hand the marker an
    /// address to `try_mark`, i.e. a write through a pointer into memory this
    /// collector does not own.
    #[test]
    fn an_armed_satb_barrier_refuses_null_and_foreign_addresses() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        heap.set_mark_active(true);

        heap.satb_pre_barrier(0);
        assert_eq!(heap.mark_ingress_pushes(), 0, "null carries no edge");

        // An address the arena never handed out. Well clear of the arena and
        // aligned, so only the registry gate can reject it.
        heap.satb_pre_barrier(0xDEAD_BEE0);
        assert_eq!(
            heap.mark_ingress_pushes(),
            0,
            "an address this heap never allocated must not reach the marker"
        );
    }

    /// Disarming clears the ingress, so a leftover address cannot be
    /// republished into the next cycle.
    ///
    /// This matters more than it looks: the addresses in the ingress are raw
    /// arena offsets, and between cycles the sweep coalesces the free list and
    /// retracts the bump cursor. An address held across that is not merely
    /// stale, it may name reclaimed space — `reclaim_guard`'s whole subject.
    #[test]
    fn disarming_the_satb_barrier_clears_the_ingress() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let obj = heap.alloc_object(ClassId::new(1), 4);
        heap.set_mark_active(true);
        heap.satb_pre_barrier(obj.as_ptr() as usize);
        assert_eq!(heap.mark_ingress_pushes(), 1);

        heap.set_mark_active(false);

        assert!(!heap.mark_active());
        let mut drained = Vec::new();
        assert_eq!(
            heap.drain_mark_ingress(&mut drained),
            0,
            "an address from a finished cycle must not survive into the next"
        );
    }

    // -- Phase 3: the collection-overlay edge ------------------------------

    /// Arming slot for [`overlay_provider_roots`]. `(owner_addr, root)`.
    ///
    /// A `static` because [`crate::external_roots::ExternalRootProvider`] holds
    /// plain `fn` pointers, which cannot capture — and because providers are
    /// process-global and **cannot be unregistered**, so the provider itself
    /// must be inert unless a test has armed it for one specific address it
    /// owns. Guarded by [`OVERLAY_TEST_LOCK`] so the two tests below cannot
    /// arm it concurrently.
    static OVERLAY_ARMED: parking_lot::Mutex<Option<(usize, ObjectRef)>> =
        parking_lot::Mutex::new(None);
    static OVERLAY_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Shared with `vm_heap`'s tests: the flag-sensitive ZGC fixtures set a
    /// process-visible override and must not run concurrently.
    pub(crate) fn tests_overlay_lock() -> parking_lot::MutexGuard<'static, ()> {
        OVERLAY_TEST_LOCK.lock()
    }

    fn overlay_provider_roots(owner_addr: usize, _class_id: Option<u32>) -> Vec<ObjectRef> {
        match *OVERLAY_ARMED.lock() {
            Some((armed_owner, root)) if armed_owner == owner_addr => vec![root],
            _ => Vec::new(),
        }
    }

    fn register_overlay_provider() {
        crate::external_roots::register_external_root_provider(
            crate::external_roots::ExternalRootProvider {
                name: "zgc-test-collection-overlay",
                scan: |_out| {},
                owner_addrs: || None,
                roots_for_owner: overlay_provider_roots,
                roots_for_matching_owners: |_p| Vec::new(),
                remap: |_m| {},
                prune: |_p| {},
            },
        );
    }

    /// **`ZMarkContext::visit_refs` must report the collection-overlay edge.**
    ///
    /// The serial loop in `collect_garbage` pushes
    /// `external_roots_for_owner(addr, class_id)`; `visit_refs` did not, until
    /// 2026-08-13. That difference is invisible while the serial loop is the
    /// only marker and becomes a use-after-free the moment a coordinator drives
    /// a real collection: a native collection overlay is reachable ONLY through
    /// the Java object that owns it, so a marker that skips the edge sweeps
    /// live contents out from under a surviving owner.
    ///
    /// The exact edit that trips it: delete the `external_roots_for_owner` loop
    /// at the end of `visit_refs`.
    #[test]
    fn visit_refs_reports_the_collection_overlay_edge() {
        let _guard = OVERLAY_TEST_LOCK.lock();
        register_overlay_provider();
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        let owner = heap.alloc_object(ClassId::new(7), 0);
        let overlay = heap.alloc_object(ClassId::new(8), 0);
        *OVERLAY_ARMED.lock() = Some((owner.as_ptr() as usize, overlay));

        let mut seen: Vec<u64> = Vec::new();
        {
            use super::mark::ZMarkContext;
            heap.visit_refs(owner.as_ptr() as u64, &mut |a| seen.push(a));
        }
        *OVERLAY_ARMED.lock() = None;

        assert!(
            seen.contains(&(overlay.as_ptr() as u64)),
            "visit_refs must report the overlay owned by this object; saw {seen:?}"
        );
    }

    /// ...and the parallel marker therefore keeps the overlay alive.
    ///
    /// The end-to-end statement of the test above: this is the failure the
    /// missing edge would actually have produced.
    #[test]
    fn the_parallel_mark_keeps_a_collection_overlay_alive() {
        let _guard = OVERLAY_TEST_LOCK.lock();
        register_overlay_provider();
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        let owner = heap.alloc_object(ClassId::new(7), 0);
        let overlay = heap.alloc_object(ClassId::new(8), 0);
        *OVERLAY_ARMED.lock() = Some((owner.as_ptr() as usize, overlay));

        let _skip = heap.begin_concurrent_mark_cycle();
        heap.mark_parallel_stw(&[owner.as_ptr() as u64], 2)
            .expect("the driver must certify a complete mark set at a safepoint");
        heap.end_concurrent_mark_cycle();
        *OVERLAY_ARMED.lock() = None;

        assert!(
            heap.header_ref(overlay.as_ptr()).gc_flags() & GC_FLAG_MARKED != 0,
            "an overlay reachable only through a live owner must survive the mark"
        );
    }

    // -- Phase 3: stop-the-world PARALLEL marking --------------------------

    /// Build a small object graph: a root chain plus a side branch and one
    /// unreachable object. Returns `(roots, all_allocated, expected_live)`.
    fn parallel_mark_fixture(heap: &ZgcRealHeap) -> (Vec<ObjectRef>, Vec<ObjectRef>, usize) {
        // root -> a -> b, root -> c, and `dead` reachable from nothing.
        let root = heap.alloc_object(ClassId::new(1), 2);
        let a = heap.alloc_object(ClassId::new(1), 1);
        let b = heap.alloc_object(ClassId::new(1), 0);
        let c = heap.alloc_object(ClassId::new(1), 0);
        let dead = heap.alloc_object(ClassId::new(1), 0);
        heap.set_field(root, 0, Value::Object(Some(a)));
        heap.set_field(root, 1, Value::Object(Some(c)));
        heap.set_field(a, 0, Value::Object(Some(b)));
        (vec![root], vec![root, a, b, c, dead], 4)
    }

    /// **The parallel mark must produce the identical mark set to the serial
    /// loop**, because the sweep that follows cannot tell which one set the
    /// bits.
    ///
    /// Two heaps, same fixture, same roots: one collected with the serial loop
    /// and one with `mark_parallel_stw`. Comparing survivor COUNTS after the
    /// sweep is the end-to-end statement — a parallel mark that missed a live
    /// object would sweep it, and one that over-marked would retain garbage.
    #[test]
    fn parallel_mark_marks_the_same_objects_as_the_serial_loop() {
        // Serial arm.
        let serial = ZgcRealHeap::with_capacity(256 * 1024);
        let (mut serial_roots, _all, expected_live) = parallel_mark_fixture(&serial);
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        serial.collect_garbage(&stw, &mut serial_roots, &NoMonitors);
        let serial_live = serial.registry.snapshot().bases().len();

        // Parallel arm, driven directly so the test does not depend on the
        // environment variable being visible to this process.
        let par = ZgcRealHeap::with_capacity(256 * 1024);
        let (par_roots, all, _) = parallel_mark_fixture(&par);
        let skip = par.begin_concurrent_mark_cycle();
        let _ = skip;
        let root_addrs: Vec<u64> = par_roots.iter().map(|r| r.as_ptr() as u64).collect();
        let stats = par
            .mark_parallel_stw(&root_addrs, 4)
            .expect("the driver must certify a complete mark set at a safepoint");
        par.end_concurrent_mark_cycle();

        // Count what the parallel engine marked, directly off the headers.
        let par_marked = all
            .iter()
            .filter(|o| {
                par.header_ref(o.as_ptr()).gc_flags() & GC_FLAG_MARKED != 0
            })
            .count();

        assert_eq!(
            par_marked, expected_live,
            "the parallel mark must reach every reachable object and no more; \
             engine reported objects_marked={} scanned={}",
            stats.objects_marked, stats.objects_scanned
        );
        assert_eq!(
            serial_live, expected_live,
            "the serial arm is the control and must agree with the fixture"
        );
    }

    /// The engine must reach a transitively-reachable object — i.e. the test
    /// above is not passing because everything happens to be a root.
    ///
    /// Without this, a `mark_parallel_stw` that marked only its root set would
    /// satisfy a survivor count on a fixture whose objects were all roots.
    #[test]
    fn parallel_mark_reaches_a_grandchild_not_just_the_roots() {
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        let root = heap.alloc_object(ClassId::new(1), 1);
        let child = heap.alloc_object(ClassId::new(1), 1);
        let grandchild = heap.alloc_object(ClassId::new(1), 0);
        heap.set_field(root, 0, Value::Object(Some(child)));
        heap.set_field(child, 0, Value::Object(Some(grandchild)));

        let _skip = heap.begin_concurrent_mark_cycle();
        heap.mark_parallel_stw(&[root.as_ptr() as u64], 4)
            .expect("the driver must certify a complete mark set at a safepoint");
        heap.end_concurrent_mark_cycle();

        for (name, obj) in [
            ("root", root),
            ("child", child),
            ("grandchild", grandchild),
        ] {
            assert!(
                heap.header_ref(obj.as_ptr()).gc_flags() & GC_FLAG_MARKED != 0,
                "{name} must be marked by the parallel engine"
            );
        }
    }

    /// An object reachable from nothing must NOT be marked.
    ///
    /// The counterpart to the test above: an engine that marked every
    /// registered address would pass both survivor counts and every
    /// reachability assertion, and would retain the whole heap forever.
    #[test]
    fn parallel_mark_leaves_an_unreachable_object_unmarked() {
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        let root = heap.alloc_object(ClassId::new(1), 0);
        let orphan = heap.alloc_object(ClassId::new(1), 0);

        let _skip = heap.begin_concurrent_mark_cycle();
        heap.mark_parallel_stw(&[root.as_ptr() as u64], 4)
            .expect("the driver must certify a complete mark set at a safepoint");
        heap.end_concurrent_mark_cycle();

        assert!(heap.header_ref(root.as_ptr()).gc_flags() & GC_FLAG_MARKED != 0);
        assert!(
            heap.header_ref(orphan.as_ptr()).gc_flags() & GC_FLAG_MARKED == 0,
            "an unreachable object must not be marked, or nothing is ever collected"
        );
    }

    /// Marking is **serial by default**, and the driven path is opt-in.
    ///
    /// It was default-on for one day (2026-08-13 to 2026-08-14) and was turned
    /// back off by the measurement Phase 3's exit criterion asks for: on a
    /// 1M-object live set a driven cycle costs +31% pause at one worker and
    /// +153% at four. See `Z_PARMARK_DEFAULT_WORKERS` for the table.
    ///
    /// The knob still resolves and still caps, because the opt-in path is how
    /// C5 will be measured.
    #[test]
    fn marking_is_serial_by_default_and_parallelism_is_opt_in() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let n = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_PARMARK", None)],
            || heap.parallel_mark_workers(),
        );
        assert_eq!(
            n, 0,
            "unset must mean the serial loop: a driven cycle is a measured              pause regression until the marker scales"
        );
        let n = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_PARMARK", Some("4"))],
            || heap.parallel_mark_workers(),
        );
        let cores = std::thread::available_parallelism()
            .map(|c| c.get())
            .unwrap_or(1);
        assert_eq!(
            n,
            4.min(cores).min(Z_PARMARK_MAX_WORKERS),
            "an explicit request must still resolve, and still cap"
        );
    }

    /// **The parallel mark path must open a mark cycle, or no weak reference
    /// can ever be cleared.**
    ///
    /// `visit_refs` without a skip-set snapshot traces slot 0 of every
    /// `Reference` as a STRONG edge, so the referent is reachable through its
    /// own `Reference` — a leak, not a crash, whose only signal is a one-shot
    /// warning. `collect_garbage`'s parallel branch did exactly that until
    /// 2026-08-13, and it stayed hidden because the path was opt-in and no
    /// test drove `collect_garbage` with it on.
    ///
    /// The serial branch builds its own `ref_skip_objs`. This asserts the two
    /// paths agree about the one thing that must not differ between them.
    ///
    /// The exact edit that trips it: drop the `begin_concurrent_mark_cycle`
    /// call from the parallel branch.
    #[test]
    fn the_parallel_mark_path_clears_a_weak_reference_like_the_serial_one() {
        for workers in ["0", "4"] {
            let heap = ZgcRealHeap::new();
            let weak = heap.alloc_object(ClassId::new(1), 1);
            let referent = heap.alloc_object(ClassId::new(2), 0);
            heap.set_field(weak, 0, Value::Object(Some(referent)));
            heap.discover_reference(ReferenceType::Weak, weak, referent, None);

            let mut roots = [weak];
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_ZGC_PARMARK", Some(workers)),
                    // Isolate the marking question from the moving one.
                    ("CRATONVM_ZGC_RELOCATE", Some("0")),
                ],
                || {
                    // SAFETY: these unit tests run the heap single-threaded.
                    let stw = unsafe { StopTheWorldToken::new() };
                    heap.collect_garbage(&stw, &mut roots, &NoMonitors);
                },
            );

            assert_eq!(
                heap.get_field(roots[0], 0),
                Value::Object(None),
                "CRATONVM_ZGC_PARMARK={workers}: the referent must be cleared \
                 on BOTH marking paths"
            );
        }
    }

    // -- Phase 4: stop-the-world compaction --------------------------------

    /// **The object graph must survive a compaction.**
    ///
    /// A slide that moves objects but does not rewrite the references between
    /// them leaves every survivor pointing at where its neighbour used to be
    /// -- addresses that `compact_low_to` has just zeroed. This is the test
    /// that says the rewrite happened, and it checks the FIELD, not just that
    /// something moved.
    #[test]
    fn compaction_moves_survivors_and_rewrites_the_references_between_them() {
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        heap.set_tlab_enabled(false);

        // Garbage first, so the survivors above it have somewhere to slide to.
        for _ in 0..8 {
            heap.alloc_object(ClassId::new(1), 4);
        }
        let parent = heap.alloc_object(ClassId::new(1), 1);
        let child = heap.alloc_object(ClassId::new(1), 0);
        heap.set_field(parent, 0, Value::Object(Some(child)));
        assert_eq!(heap.get_field(parent, 0), Value::Object(Some(child)));

        let live = [parent.as_ptr() as usize, child.as_ptr() as usize];
        let (moved, reclaimed, map) = heap.relocate_stw(&live);

        assert!(moved >= 2, "both survivors should have slid down; moved={moved}");
        assert!(reclaimed > 0, "the cursor must come back down");
        assert!(!map.is_empty(), "a moving cycle must publish a pointer map");

        let new_parent = map
            .get(&(parent.as_ptr() as usize))
            .copied()
            .expect("parent moved, so it must be in the map");
        let new_child = map
            .get(&(child.as_ptr() as usize))
            .copied()
            .expect("child moved, so it must be in the map");
        assert!(new_parent < parent.as_ptr() as usize, "objects slide DOWN");

        // THE ASSERTION THIS TEST EXISTS FOR: the parent's reference field
        // must name the child's NEW address, not its old one.
        let moved_parent = unsafe { ObjectRef::from_raw(new_parent as *mut u8) };
        assert_eq!(
            heap.get_field(moved_parent, 0),
            Value::Object(Some(unsafe { ObjectRef::from_raw(new_child as *mut u8) })),
            "the reference between two moved survivors was not rewritten"
        );
    }

    /// A compaction with nothing dead below the survivors moves nothing, and
    /// says so with an empty map rather than a map of identity entries.
    ///
    /// An identity entry is worse than no entry: every consumer of the map
    /// would rewrite a slot to the value it already held, and `is_empty()` --
    /// which is how a caller decides whether to run the remap at all --
    /// would answer `false` for a cycle that moved nothing.
    #[test]
    fn a_compaction_with_no_garbage_below_the_survivors_moves_nothing() {
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        heap.set_tlab_enabled(false);
        let a = heap.alloc_object(ClassId::new(1), 0);
        let b = heap.alloc_object(ClassId::new(1), 0);
        let live = [a.as_ptr() as usize, b.as_ptr() as usize];
        let (moved, _reclaimed, map) = heap.relocate_stw(&live);

        assert_eq!(moved, 0, "nothing below them died, so nothing can slide");
        assert!(
            map.is_empty(),
            "a cycle that moved nothing must publish an EMPTY map, not identity entries"
        );
        assert_eq!(heap.get_field(a, 0), Value::Object(None));
    }

    /// Compaction actually reclaims: the largest servable block after a
    /// compaction must exceed what the same heap could serve before it.
    ///
    /// This is the property the whole phase exists for -- Gap B in the
    /// maturity assessment -- so it is asserted directly rather than inferred
    /// from a byte count.
    #[test]
    fn compaction_restores_a_contiguous_run_the_sweep_alone_cannot() {
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        heap.set_tlab_enabled(false);
        // Alternate garbage and survivors so the free space is genuinely
        // interleaved -- the shape a non-moving sweep cannot repair.
        let mut survivors = Vec::new();
        for _ in 0..16 {
            heap.alloc_object(ClassId::new(1), 8); // garbage
            survivors.push(heap.alloc_object(ClassId::new(1), 0));
        }
        let live: Vec<usize> = survivors.iter().map(|o| o.as_ptr() as usize).collect();

        let before = heap.arena.lock().largest_free_block();
        let (moved, reclaimed, _map) = heap.relocate_stw(&live);
        let after = {
            let a = heap.arena.lock();
            a.remaining().saturating_sub(a.free_list_bytes())
        };

        assert!(moved > 0, "interleaved garbage must let survivors slide");
        assert!(reclaimed > 0);
        assert!(
            after > before,
            "compaction must leave a bigger contiguous run than the free list \
             held before it: after={after} before={before}"
        );
    }

    /// **End to end: a full `collect_garbage` with compaction on keeps the
    /// graph intact and rewrites the caller's roots.**
    ///
    /// The unit tests above drive `relocate_stw` directly. This one goes
    /// through the real entry point with `CRATONVM_ZGC_RELOCATE=1`, which is
    /// the only way to cover the two things the wiring adds and the unit
    /// tests cannot see: that the `roots` slice is rewritten in place -- a
    /// root still naming a pre-slide address is a dangling pointer the instant
    /// `collect_garbage` returns, and nothing downstream would fix it -- and
    /// that the returned `PointerMap` is non-empty so callers actually run
    /// their remap.
    ///
    /// Serialised against the other flag-sensitive fixtures, since the
    /// override is process-wide.
    #[test]
    fn collect_garbage_with_compaction_on_rewrites_roots_and_keeps_the_graph() {
        let _guard = OVERLAY_TEST_LOCK.lock();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("1"))],
            || {
                let heap = ZgcRealHeap::with_capacity(256 * 1024);
                heap.set_tlab_enabled(false);
                assert!(
                    heap.relocation_requested(),
                    "the override must reach the heap or this test proves nothing"
                );

                // Garbage below, so the survivors have somewhere to slide.
                for _ in 0..8 {
                    heap.alloc_object(ClassId::new(1), 4);
                }
                let parent = heap.alloc_object(ClassId::new(1), 1);
                let child = heap.alloc_object(ClassId::new(1), 0);
                heap.set_field(parent, 0, Value::Object(Some(child)));

                let old_parent = parent.as_ptr() as usize;
                let mut roots = [parent];
                // SAFETY: these unit tests run the heap single-threaded.
                let stw = unsafe { StopTheWorldToken::new() };
                let result = heap.collect_garbage(&stw, &mut roots, &NoMonitors);

                assert!(
                    !result.pointer_map.is_empty(),
                    "a compacting cycle must publish a non-empty pointer map"
                );
                assert!(
                    (roots[0].as_ptr() as usize) < old_parent,
                    "collect_garbage must rewrite the caller's root to the new address"
                );
                // ...and the graph the root leads to is still intact.
                match heap.get_field(roots[0], 0) {
                    Value::Object(Some(c)) => {
                        assert!(
                            heap.registry.contains(c.as_ptr() as usize),
                            "the child reference must name a live, registered object"
                        );
                    }
                    other => {
                        panic!("the parent's reference was lost by compaction: {other:?}")
                    }
                }
            },
        );
    }

    /// **The relocation-set selector actually refuses a dense page.**
    ///
    /// Adopting `forwarding::ZRelocationSet::select` only means something if
    /// its policy changes an outcome. A page whose live occupancy is at or
    /// above `max_live_occupancy` (0.25 by default -- copy one byte to reclaim
    /// at least three) must be left to decay rather than copied for nothing,
    /// and this is the test that says so: every object is live, so there is no
    /// garbage to reclaim and the profitable move is not to move.
    ///
    /// Without the selector this function slid the whole low region every
    /// cycle, which on this fixture copies every byte and reclaims none. The
    /// exact edit that trips it: drop the `select` call and slide everything.
    #[test]
    fn the_selector_refuses_to_evacuate_a_page_with_no_garbage_to_reclaim() {
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        heap.set_tlab_enabled(false);
        let mut all = Vec::new();
        for _ in 0..24 {
            all.push(heap.alloc_object(ClassId::new(1), 4));
        }
        // EVERY object is live: occupancy 1.0, garbage 0.
        let live: Vec<usize> = all.iter().map(|o| o.as_ptr() as usize).collect();

        let (moved, _reclaimed, map) = heap.relocate_stw(&live);

        assert_eq!(
            moved, 0,
            "a wholly-live page is pure copy cost and zero reclaim; the policy \
             must refuse it"
        );
        assert!(map.is_empty(), "nothing moved, so nothing to remap");
        // ...and every object is still readable where it was.
        for o in &all {
            assert!(heap.registry.contains(o.as_ptr() as usize));
        }
    }

    /// ...and it still evacuates a page that IS mostly garbage.
    ///
    /// The pair to the test above: a policy that refused everything would
    /// satisfy that one and would have turned compaction off entirely.
    #[test]
    fn the_selector_still_evacuates_a_page_that_is_mostly_garbage() {
        let heap = ZgcRealHeap::with_capacity(256 * 1024);
        heap.set_tlab_enabled(false);
        let mut survivors = Vec::new();
        for i in 0..40 {
            let o = heap.alloc_object(ClassId::new(1), 4);
            // One in ten survives: occupancy 0.1, well under the 0.25 cutoff.
            if i % 10 == 0 {
                survivors.push(o);
            }
        }
        let live: Vec<usize> = survivors.iter().map(|o| o.as_ptr() as usize).collect();

        let (moved, reclaimed, map) = heap.relocate_stw(&live);

        assert!(
            moved > 0,
            "a page that is 90% garbage is exactly what the policy exists to \
             evacuate"
        );
        assert!(reclaimed > 0, "and the cursor must come back down");
        assert!(!map.is_empty());
    }

    // -- Phase 4: generation + remembered over the logical grid ------------

    /// The address of an object's first reference WORD, as the census reports
    /// it.
    ///
    /// Not `base + HEADER_SIZE`: for the two 16-byte-cell shapes the reference
    /// word is at `cell + 8`, so a hand-computed offset lands on the tag and
    /// the card is recorded against the wrong 8 bytes. `reference_slots` is
    /// the only thing that knows which of the four slot shapes an object has.
    fn first_ref_slot_addr(heap: &ZgcRealHeap, obj: ObjectRef) -> usize {
        use super::census::ZCensusHeapView;
        let mut found = None;
        heap.reference_slots(obj.as_ptr() as u64, &mut |slot| {
            if found.is_none() {
                found = Some(slot.slot_addr as usize);
            }
        });
        found.expect("the fixture object must have a reference slot")
    }

    /// Pages age one cycle at a time and cross into old at the policy's
    /// promotion age -- `zgc::generation`'s rule, applied to the grid.
    #[test]
    fn a_logical_page_is_promoted_when_its_age_reaches_the_policy_age() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let policy = generation::ZPromotionPolicy::with_age(3);

        // Cycles 1 and 2: still young.
        for cycle in 1..=2 {
            let (young, old) = heap.age_pages_and_split(1, &policy);
            assert_eq!(young.len(), 1, "cycle {cycle}: page must still be young");
            assert!(old.is_empty(), "cycle {cycle}: nothing promoted yet");
        }
        // Cycle 3 reaches the promotion age.
        let (young, old) = heap.age_pages_and_split(1, &policy);
        assert!(young.is_empty(), "the page has reached the promotion age");
        assert_eq!(old, vec![0], "and must now be old");
    }

    /// **The store barrier records an old-to-young edge, and is inert while
    /// there is no old generation.**
    ///
    /// The inert half is the cost argument: `note_ref_store` sits on every
    /// reference store, and a program short enough never to promote a page
    /// must not pay more than one length check for it.
    #[test]
    fn the_card_barrier_is_inert_until_a_page_is_old_then_records_the_edge() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        heap.set_tlab_enabled(false);
        let holder = heap.alloc_object(ClassId::new(1), 1);
        let _slot = first_ref_slot_addr(&heap, holder);

        // No page is old yet.
        heap.note_ref_store(holder.as_ptr() as usize);
        assert_eq!(
            heap.remembered_edge_count(),
            0,
            "with no old generation there is nothing to remember"
        );

        // Age page 0 into old, and register its remembered set the way a
        // cycle does.
        let policy = generation::ZPromotionPolicy::with_age(1);
        let (_young, old) = heap.age_pages_and_split(1, &policy);
        assert_eq!(old, vec![0]);
        heap.remembered
            .register_old_page(0, ZgcRealHeap::Z_LOGICAL_PAGE_BYTES);
        heap.old_page_ids.lock().clone_from(&old);

        heap.note_ref_store(holder.as_ptr() as usize);
        assert_eq!(
            heap.remembered_edge_count(),
            1,
            "a store into an old page must be remembered"
        );
    }

    /// **A remembered old slot is a ROOT: the object it names is reachable
    /// from the old generation and nowhere else.**
    ///
    /// This is the assertion that makes the whole generational apparatus worth
    /// having. Without it a young cycle sweeps an object whose only reference
    /// lives in an old-generation field -- the classic missing-card
    /// use-after-free, and the entire justification for the store barrier.
    #[test]
    fn a_young_scope_treats_remembered_old_slots_as_roots() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        heap.set_tlab_enabled(false);
        let old_holder = heap.alloc_object(ClassId::new(1), 1);
        let young_target = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(old_holder, 0, Value::Object(Some(young_target)));

        // Page 0 becomes old and the store is carded.
        let policy = generation::ZPromotionPolicy::with_age(1);
        let (_y, old) = heap.age_pages_and_split(1, &policy);
        heap.remembered
            .register_old_page(0, ZgcRealHeap::Z_LOGICAL_PAGE_BYTES);
        heap.old_page_ids.lock().clone_from(&old);
        heap.note_ref_store(old_holder.as_ptr() as usize);
        assert_eq!(heap.remembered_edge_count(), 1);

        let base = heap.arena.lock().base_ptr() as usize;
        let roots = heap.remembered_roots(base);

        assert!(
            roots.contains(&(young_target.as_ptr() as usize)),
            "the object an old field points at must come back as a root; \
             without it a young cycle collects a live object. roots={roots:?}"
        );
    }

    /// **`GarbageCollector::write_barrier` actually reaches the card
    /// barrier.**
    ///
    /// The test above calls `note_ref_store` directly, so it would pass with
    /// the `write_barrier` arm back to the `{}` it was until 2026-08-13. This
    /// one drives the trait method every reference store in the VM already
    /// goes through, which is the only way to tell a wired barrier from an
    /// inert one.
    #[test]
    fn the_write_barrier_trait_arm_reaches_the_card_barrier() {
        use crate::collector::GarbageCollector;
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        heap.set_tlab_enabled(false);
        let holder = heap.alloc_object(ClassId::new(1), 1);

        let policy = generation::ZPromotionPolicy::with_age(1);
        let (_y, old) = heap.age_pages_and_split(1, &policy);
        heap.remembered
            .register_old_page(0, ZgcRealHeap::Z_LOGICAL_PAGE_BYTES);
        heap.old_page_ids.lock().clone_from(&old);

        heap.write_barrier(holder, Value::Object(None));

        assert_eq!(
            heap.remembered_edge_count(),
            1,
            "write_barrier must card the written object"
        );
    }

    // -- Phase 4: the load barrier on a real read path ---------------------

    /// Disarmed, the read path is byte-for-byte what it was: an uncoloured
    /// slot is read as a plain pointer and no barrier machinery runs.
    ///
    /// This is the cost claim, asserted. The gate is not an optimisation --
    /// the barrier CANNOT run over an uncoloured slot, because a plain arena
    /// pointer is above 2^42 and the classifier would read its address bits as
    /// colours. If this ever passes with the barrier armed by default, every
    /// reference read in the VM is resolving to the wrong object.
    #[test]
    fn the_read_barrier_is_disarmed_by_default_and_reads_a_plain_pointer() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        assert!(
            !heap.load_barrier_armed(),
            "nothing in a default run may colour slots"
        );
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
        let target = heap.alloc_object(ClassId::new(1), 0);
        heap.set_array_element(arr, 0, Value::Object(Some(target)))
            .expect("in bounds");

        assert_eq!(
            heap.get_array_element(arr, 0),
            Ok(Value::Object(Some(target)))
        );
        assert_eq!(heap.get_array_element(arr, 1), Ok(Value::Object(None)));
    }

    /// **Armed, the barrier resolves a COLOURED slot back to its object** --
    /// which the unbarriered path cannot do, because a coloured word is
    /// deliberately implausible and `read_prim_element` degrades it to null.
    ///
    /// This is sites 6 and 7 of `zgc_colored_word_degradation.rs` seen from
    /// the other side: the tripwire says the shared array arm nulls a coloured
    /// word, and this says the ZGC arm goes through the barrier instead of
    /// through that arm.
    #[test]
    fn an_armed_barrier_resolves_a_coloured_reference_element() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        let target = heap.alloc_object(ClassId::new(1), 0);

        let base = heap.arena.lock().base_ptr() as u64;
        let offset = target.as_ptr() as u64 - base;
        let coloured: u64 = vaddr::color(offset, vaddr::ZColor::Remapped).into();

        // Write the coloured word straight into the element, the way a
        // relocation-aware store would.
        let slot = unsafe { arr.as_ptr().add(ARRAY_DATA_OFFSET) as usize };
        // SAFETY: an 8-byte-aligned reference word in a live array.
        unsafe { std::ptr::write(slot as *mut u64, coloured) };

        // Disarmed, that word is nonsense: the plain read hands back the raw
        // bits, which is exactly why the barrier must precede the shared arm.
        assert!(!heap.load_barrier_armed());

        heap.set_barrier_color(Some(vaddr::ZColor::Remapped));
        assert!(heap.load_barrier_armed());

        let got = heap.get_array_element(arr, 0).expect("in bounds");
        heap.set_barrier_color(None);

        assert_eq!(
            got,
            Value::Object(Some(target)),
            "the barrier must resolve a coloured word back to its object; \
             a null here is the silent-degradation defect the tripwire names"
        );
    }

    /// An armed barrier **forwards** a reference whose object relocation
    /// moved, and heals the slot so the next read takes the fast path.
    ///
    /// This is the property that makes relocation possible at all: the mutator
    /// keeps reading through a stale slot and the barrier repairs it on the
    /// way past.
    #[test]
    fn an_armed_barrier_forwards_a_relocated_reference() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        let from_obj = heap.alloc_object(ClassId::new(1), 0);
        let to_obj = heap.alloc_object(ClassId::new(1), 0);

        let base = heap.arena.lock().base_ptr() as u64;
        let from_off = from_obj.as_ptr() as u64 - base;
        let to_off = to_obj.as_ptr() as u64 - base;

        // A slot still naming the OLD location, coloured with the mark parity
        // that is bad while `Remapped` is good -- i.e. the state a mutator
        // finds after a relocating cycle it has not yet caught up with.
        let stale: u64 = vaddr::color(from_off, vaddr::ZColor::Marked0).into();
        let slot = unsafe { arr.as_ptr().add(ARRAY_DATA_OFFSET) as usize };
        // SAFETY: an 8-byte-aligned reference word in a live array.
        unsafe { std::ptr::write(slot as *mut u64, stale) };

        // Publish the move and arm the barrier.
        heap.forwarding.lock().insert(from_off, to_off);
        heap.relocate_active.store(true, Ordering::Relaxed);
        heap.set_barrier_color(Some(vaddr::ZColor::Remapped));

        let got = heap.get_array_element(arr, 0).expect("in bounds");

        // SAFETY: reading back the word the barrier healed.
        let healed = unsafe { std::ptr::read(slot as *const u64) };
        heap.set_barrier_color(None);
        heap.relocate_active.store(false, Ordering::Relaxed);
        heap.forwarding.lock().clear();

        assert_eq!(
            got,
            Value::Object(Some(to_obj)),
            "the barrier must forward a stale reference to where the object went"
        );
        assert_ne!(
            healed, stale,
            "and it must SELF-HEAL the slot, or every later read pays the slow \
             path again"
        );
    }

    /// **Arming the read barrier reaches the JIT's codegen gate.**
    ///
    /// The barrier tests above all sit inside this crate, and every one of
    /// them would pass while JIT-compiled code went on emitting raw inline
    /// reference loads over coloured slots -- which is a use-after-free, not a
    /// missed optimisation. This asserts the one fact that connects the two:
    /// `set_barrier_color` publishes to `cratonvm_types`, which is what
    /// `x64::zgc_read_barrier_blocks_inline_fields` reads.
    ///
    /// Serialised, because the flag is process-wide by design (see its doc for
    /// why the JIT cannot read a per-heap one).
    #[test]
    fn arming_the_read_barrier_sets_the_process_wide_codegen_gate() {
        let _serialise = OVERLAY_TEST_LOCK.lock();
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        assert!(
            !cratonvm_types::zgc_read_barrier_armed(),
            "the codegen gate must start closed"
        );

        heap.set_barrier_color(Some(vaddr::ZColor::Marked0));
        let armed = cratonvm_types::zgc_read_barrier_armed();
        heap.set_barrier_color(None);
        let disarmed = cratonvm_types::zgc_read_barrier_armed();

        assert!(
            armed,
            "arming must reach the codegen gate, or the JIT keeps emitting raw              inline loads over coloured slots"
        );
        assert!(!disarmed, "and disarming must let the inline arms back on");
    }

    /// Compaction is **on by default** as of 2026-08-13, with
    /// `CRATONVM_ZGC_RELOCATE=0` as the kill switch.
    ///
    /// The kill switch restores the non-moving behaviour byte for byte — the
    /// sweep returns an empty `PointerMap` exactly as it always did — so the
    /// A/B is a re-run and not a rebuild. That property is what makes this
    /// flag usable as a bisect when something only breaks with a moving
    /// collector.
    #[test]
    fn compaction_is_on_by_default_and_zero_is_the_kill_switch() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", None)],
            || heap.relocation_requested(),
        );
        assert!(on, "compaction must be on by default");
        for off in ["0", "off", "false", "no"] {
            let v = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_RELOCATE", Some(off))],
                || heap.relocation_requested(),
            );
            assert!(!v, "CRATONVM_ZGC_RELOCATE={off} must be a kill switch");
        }
    }

    /// **A dense page BETWEEN two selected pages must not be slid over.**
    ///
    /// `ZRelocationSet::select` ranks by descending garbage ratio and takes a
    /// prefix, so the selected page ids are an arbitrary, **non-contiguous**
    /// set — any page at or above `max_live_occupancy` (25%) is skipped. The
    /// slide, however, marched one `dest` cursor upward from the first
    /// selected page and placed every survivor consecutively, with no regard
    /// for the unselected pages in between. Once the selected pages' live
    /// bytes exceeded the gap below the first dense page, survivors from the
    /// pages ABOVE it were copied straight over the live objects ON it.
    ///
    /// That is `zgc-relocate-cursor-panic-on-netty-tls-20260814.md`. The panic
    /// it was reported as is two steps downstream: the clobbered header is
    /// read back by `highest_pinned_end`, `alloc_size` believes its garbage
    /// array length, and `compact_low_to` refuses a `new_cursor` of
    /// 15,348,137,664 on a 168 MB arena — which is exactly 1,918,517,208 * 8,
    /// an 8-byte element type times a length read out of clobbered memory.
    ///
    /// This test asserts the corruption directly rather than the panic,
    /// because the panic is incidental: the same overwrite presented once as a
    /// bare SIGSEGV, and on a run where the clobbered bytes happened to decode
    /// plausibly it would present as neither.
    #[test]
    fn compaction_must_not_slide_survivors_over_an_unselected_dense_page() {
        // Six 2 MiB logical pages of bump, with page 1 dense. The selected
        // pages' live bytes then total more than the 2 MiB below page 1, which
        // is the condition that makes the slide reach it.
        const PAGE: usize = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        const PAGES: usize = 6;
        const DENSE: usize = 1;
        const FIELDS: usize = 500; // 16 + 4000 = 4016 bytes per object
        let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
        // Deterministic layout: a TLAB carves chunks whose size depends on a
        // process-wide flag, which would make which object lands on which page
        // depend on a peer test.
        heap.set_tlab_enabled(false);

        let per_page = PAGE / (HEADER_SIZE + FIELDS * SLOT_SIZE);
        let mut roots: Vec<ObjectRef> = Vec::new();
        // (object, sentinel) for the objects on the dense page.
        let mut victims: Vec<(ObjectRef, i32)> = Vec::new();
        let mut sentinel = 1i32;

        for page in 0..PAGES {
            // Dense page: keep 3 in 4 (75% > 25%, so never selected).
            // Sparse pages: keep 1 in 5 (20% < 25%, so selected).
            let keep_every = if page == DENSE { 4 } else { 5 };
            let keep_of_four = if page == DENSE { 3 } else { 1 };
            for i in 0..per_page {
                let o = heap.alloc_object(ClassId::new(1), FIELDS);
                let keep = (i % keep_every) < keep_of_four;
                if keep {
                    heap.set_field(o, 0, Value::Int(sentinel));
                    roots.push(o);
                    if page == DENSE {
                        victims.push((o, sentinel));
                    }
                    sentinel += 1;
                }
            }
        }
        assert!(
            victims.len() > 100,
            "the dense page must hold enough live objects to be worth checking; got {}",
            victims.len()
        );

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("1"))],
            || {
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            },
        );

        // An object on an unselected page must not move...
        for (i, (obj, _)) in victims.iter().enumerate() {
            let now = roots[roots.len() - victims.len() + i - 0];
            let _ = now; // addresses are checked via the sentinel below
            let _ = obj;
        }
        // ...and, the part that actually fails, its contents must survive.
        // Read through the ORIGINAL reference: an unselected object does not
        // move, so a stale read here is the correct read.
        let mut clobbered = 0usize;
        for (obj, want) in &victims {
            if heap.get_field(*obj, 0) != Value::Int(*want) {
                clobbered += 1;
            }
        }
        assert_eq!(
            clobbered, 0,
            "{clobbered} of {} live objects on the unselected dense page were \
             overwritten by survivors slid down from the selected pages above it",
            victims.len()
        );

        // ...and the fix must not have achieved that by refusing to compact.
        // Skipping an unselected page is the correct repair; clamping the
        // slide to the pages below it would also pass the assertion above and
        // would quietly turn every non-contiguous selection into a no-op.
        let (_par, compactions, relocated) = heap.feature_engagement();
        assert!(
            compactions > 0 && relocated > 0,
            "compaction must still MOVE things across a non-contiguous              selection: compaction_cycles={compactions} objects_relocated={relocated}"
        );
    }

    /// **`zgc_concurrent`'s coordinator drives a real collection** — Phase 3's
    /// exit criterion, clause 1, asserted through `collect_garbage`.
    ///
    /// That clause is a CODE fact rather than a measurement, and it was false
    /// for a whole session while the summary table said the phase was done:
    /// the driver was complete, unit-tested, and its only context was
    /// `TestMarkContext`. The heap marked with a bespoke `mark_to_completion`
    /// call that used the worker pool but bypassed the driver — same mark
    /// bits, same stats, same `parallel_mark_cycles`. `driver_passes` is the
    /// only observable that separates them, because `passes` exists nowhere
    /// but inside `ZgcMarkCycleOutcome`.
    ///
    /// The exact edit that trips it: point `mark_parallel_stw` back at
    /// `mark_pool_only_stw`.
    #[test]
    fn the_concurrent_mark_driver_drives_a_real_collection() {
        let heap = ZgcRealHeap::with_capacity(4 * 1024 * 1024);
        heap.set_tlab_enabled(false);

        // A small graph with reachable and unreachable halves, so the cycle
        // has real work and a real verdict to reach.
        let root = heap.alloc_object(ClassId::new(1), 4);
        let child = heap.alloc_object(ClassId::new(2), 2);
        heap.set_field(root, 0, Value::Object(Some(child)));
        for _ in 0..64 {
            heap.alloc_object(ClassId::new(3), 8);
        }

        let mut roots = [root];
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_ZGC_PARMARK", Some("4")),
                // Isolate marking from the moving question.
                ("CRATONVM_ZGC_RELOCATE", Some("0")),
            ],
            || {
                // SAFETY: these unit tests run the heap single-threaded, which
                // is also exactly the condition `ZgcNoMutatorSafepoint` needs.
                let stw = unsafe { StopTheWorldToken::new() };
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            },
        );

        let (passes, fallbacks) = heap.driver_engagement();
        assert!(
            passes > 0,
            "the coordinator must have DRIVEN the cycle; `passes` comes only              from ZgcMarkCycleOutcome, so 0 means the pool marked without it"
        );
        assert_eq!(
            fallbacks, 0,
            "the driver must certify a complete mark set at a safepoint, where              by construction no mutator can race it"
        );

        // ...and it must have marked correctly, not merely run.
        assert_ne!(
            heap.get_field(roots[0], 0),
            Value::Object(None),
            "the reachable child must have survived the driven cycle"
        );
    }

    /// **A dead address that a survivor slid into must NOT reach the monitor
    /// prune.**
    ///
    /// `MonitorCleanup::prune_dead` documents its input as EXACT — the address
    /// "was a live allocation base before this collection and its memory is
    /// now freed, so no thread can read its mark word" — and on that licence
    /// it calls `Monitor::release_mark_ref`, dropping the strong reference the
    /// object's own mark word owns. Compaction breaks the precondition in the
    /// commonest way possible: survivors slide DOWN into space vacated by dead
    /// objects, so a dead base is very likely a LIVE object's base afterwards.
    ///
    /// The consequence is a use-after-free on a live object's monitor, chosen
    /// by wherever the slide happened to land — which is why it presents as
    /// scattered SIGSEGVs rather than as a locking bug.
    ///
    /// This records exactly what reached `prune_dead` and asserts that nothing
    /// in it is a live base afterwards. The exact edit that trips it: drop the
    /// `registry.contains` filter before the `monitors.prune_dead(&dead)` call.
    #[test]
    fn a_dead_address_a_survivor_slid_into_is_withheld_from_the_monitor_prune() {
        /// Captures the `dead` slice the collector hands the monitor table.
        #[derive(Default)]
        struct CapturingMonitors {
            pruned: std::sync::Mutex<Vec<usize>>,
        }
        impl crate::collector::MonitorCleanup for CapturingMonitors {
            fn remap_after_gc(&self, _map: &cratonvm_types::PointerMap) {}
            fn prune_dead(&self, dead: &[usize]) {
                self.pruned.lock().unwrap().extend_from_slice(dead);
            }
        }

        const PAGE: usize = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        const FIELDS: usize = 500; // 4016 bytes per object
        let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
        heap.set_tlab_enabled(false);

        // Two sparse pages so the selector takes both and page 1's survivors
        // slide down into page 0's freed space — the exact overlap this is
        // about.
        let per_page = PAGE / (HEADER_SIZE + FIELDS * SLOT_SIZE);
        let mut roots: Vec<ObjectRef> = Vec::new();
        for _page in 0..4 {
            for i in 0..per_page {
                let o = heap.alloc_object(ClassId::new(1), FIELDS);
                if i % 5 == 0 {
                    roots.push(o);
                }
            }
        }
        let pre: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

        let monitors = CapturingMonitors::default();
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("1"))],
            || {
                heap.collect_garbage(&stw, &mut roots, &monitors);
            },
        );

        let moved = roots
            .iter()
            .zip(pre.iter())
            .filter(|(now, was)| now.as_ptr() as usize != **was)
            .count();
        assert!(
            moved > 0,
            "the fixture must actually relocate something, or this proves nothing"
        );

        // THE ASSERTION: nothing handed to the prune may be a live base.
        let pruned = monitors.pruned.lock().unwrap().clone();
        let live_now: std::collections::HashSet<usize> =
            roots.iter().map(|r| r.as_ptr() as usize).collect();
        let overlap: Vec<usize> = pruned
            .iter()
            .copied()
            .filter(|d| live_now.contains(d))
            .collect();
        assert!(
            overlap.is_empty(),
            "{} address(es) handed to prune_dead are LIVE object bases after the \
             slide, e.g. {:#x} — prune_dead releases the mark-word reference on \
             the strength of them being freed, so this is a use-after-free on a \
             live object's monitor",
            overlap.len(),
            overlap[0]
        );
    }

    /// **A JNI-critical-pinned object must not be relocated.**
    ///
    /// `VmHeap::pin_critical_region`'s ZGC arm returned `Vec::new()` until
    /// 2026-08-14 — correct while this collector never moved an object, and
    /// wrong the day compaction shipped. The hazard is not the native pointer
    /// (this VM hands native code a copy); it is the copy-back at
    /// `ReleasePrimitiveArrayCritical`, which re-resolves the Get-time
    /// address. Move the array and that copy-back writes a whole array of
    /// bytes over whatever now occupies the old address — "data loss / write
    /// to a recycled object", in the JNI site's own words.
    ///
    /// The exact edit that trips it: delete the `critical_pin_addrs` filter
    /// that drops a pinned object's page from the relocation set.
    #[test]
    fn a_critically_pinned_object_is_never_relocated() {
        const PAGE: usize = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        const FIELDS: usize = 500;
        let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
        heap.set_tlab_enabled(false);

        let per_page = PAGE / (HEADER_SIZE + FIELDS * SLOT_SIZE);
        let mut roots: Vec<ObjectRef> = Vec::new();
        for _page in 0..4 {
            for i in 0..per_page {
                let o = heap.alloc_object(ClassId::new(1), FIELDS);
                if i % 5 == 0 {
                    roots.push(o);
                }
            }
        }
        // Pin one survivor from a page the selector would otherwise compact.
        // Index 1 rather than 0: object 0 sits at the very bottom and would
        // not move anyway, which would make this pass for the wrong reason.
        let pinned_idx = 1usize;
        let pinned_addr = roots[pinned_idx].as_ptr() as usize;
        heap.pin_critical(pinned_addr);
        assert_eq!(heap.critical_pin_count(), 1);

        let pre: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("1"))],
            || {
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            },
        );

        let moved_total = roots
            .iter()
            .zip(pre.iter())
            .filter(|(now, was)| now.as_ptr() as usize != **was)
            .count();
        assert!(
            moved_total > 0,
            "the fixture must relocate SOMETHING, or a pin that holds is meaningless"
        );
        assert_eq!(
            roots[pinned_idx].as_ptr() as usize, pinned_addr,
            "the pinned object moved: the copy-back at Release would write over \
             whatever now occupies {pinned_addr:#x}"
        );

        // ...and the pin is releasable, or one critical section immobilises a
        // page for the life of the process.
        heap.unpin_critical(pinned_addr);
        assert_eq!(heap.critical_pin_count(), 0);
    }

    /// Nested critical sections on one array: the inner release must not
    /// unpin the outer. Refcounting, asserted rather than assumed.
    #[test]
    fn critical_pins_are_refcounted_so_a_nested_release_does_not_unpin() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let o = heap.alloc_object(ClassId::new(1), 4);
        let addr = o.as_ptr() as usize;
        heap.pin_critical(addr);
        heap.pin_critical(addr);
        assert_eq!(heap.critical_pin_count(), 1, "one address, two pins");
        heap.unpin_critical(addr);
        assert_eq!(
            heap.critical_pin_count(),
            1,
            "the inner release must leave the outer pin standing"
        );
        heap.unpin_critical(addr);
        assert_eq!(heap.critical_pin_count(), 0);
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

    // -- ZMarkContext: the concurrent-mark seam ----------------------------
    //
    // These drive the trait methods DIRECTLY, never a worker pool. The engine's
    // own threading is tested in `gc/src/zgc/mark.rs` against its in-memory
    // `TestMarkContext`; what is untested until here is whether *this heap*
    // answers the four contract points correctly, and each of those is a
    // single-threaded question. Nothing below sleeps, spawns, or times
    // anything.

    use super::mark::ZMarkContext;

    /// The exactly-once property `try_mark`'s whole contract rests on: the
    /// first claim wins, every later one loses, and the bit is set either way.
    /// A second `true` would hand two workers the same scan obligation; the
    /// dangerous inverse (a lost update, so nobody scans it) is what the
    /// non-atomic read-then-set in `collect_garbage` would produce under
    /// concurrency.
    #[test]
    fn zmark_try_mark_is_exactly_once() {
        let heap = ZgcRealHeap::new();
        let obj = heap.alloc_object(ClassId::new(1), 0);
        let addr = obj.as_ptr() as u64;

        assert!(heap.try_mark(addr), "the first claim must win");
        assert!(!heap.try_mark(addr), "the second claim must lose");
        assert!(!heap.try_mark(addr), "and so must every one after it");
        assert!(heap.is_marked(addr), "the bit is set whoever won");
    }

    /// `is_marked` must be a pure query. If it set the bit, the `try_mark`
    /// below would lose — and in production every weak referent would be
    /// immortal, because the reference phase asks `is_marked` about each one.
    #[test]
    fn zmark_is_marked_does_not_mark() {
        let heap = ZgcRealHeap::new();
        let obj = heap.alloc_object(ClassId::new(1), 0);
        let addr = obj.as_ptr() as u64;

        assert!(!heap.is_marked(addr));
        assert!(!heap.is_marked(addr));
        assert!(
            heap.try_mark(addr),
            "is_marked set the bit; every weak referent would now be immortal"
        );
    }

    /// Null and off-heap addresses are refused rather than dereferenced —
    /// this is the gate that keeps a torn reference slot from becoming a
    /// header write into arbitrary memory.
    #[test]
    fn zmark_is_in_heap_gates_wild_children() {
        let heap = ZgcRealHeap::new();
        let obj = heap.alloc_object(ClassId::new(1), 0);

        assert!(heap.is_in_heap(obj.as_ptr() as u64));
        assert!(!heap.is_in_heap(0), "null is not an object base");
        assert!(
            !heap.is_in_heap(0xDEAD_BEEF),
            "an unregistered address is not an object base"
        );
        // An interior pointer is not a base either: only a base has a header.
        assert!(!heap.is_in_heap(obj.as_ptr() as u64 + 8));
    }

    /// `object_size` must report real bytes, not the trait's `0` default —
    /// which compiles and disables live-set accounting silently.
    #[test]
    fn zmark_object_size_is_the_allocation_size() {
        let heap = ZgcRealHeap::new();
        let obj = heap.alloc_object(ClassId::new(1), 3);
        let size = heap.object_size(obj.as_ptr() as u64);

        assert!(size >= HEADER_SIZE, "size must include the header");
        assert_eq!(size, heap.walk_objects()[0].1, "must agree with the sweep");
        assert_eq!(heap.object_size(0), 0, "null has no size");
    }

    /// The one that breaks `WeakReference` and `Cleaner` silently when it is
    /// wrong: slot 0 of a registered `Reference` is the referent and is NOT a
    /// strong edge. The control object proves the hole is specific to the
    /// registered `Reference` rather than "slot 0 is never reported".
    #[test]
    fn zmark_visit_refs_skips_a_registered_references_referent() {
        let heap = ZgcRealHeap::new();
        let weak = heap.alloc_object(ClassId::new(1), 1);
        let referent = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(weak, 0, Value::Object(Some(referent)));
        heap.discover_reference(ReferenceType::Weak, weak, referent, None);

        // A plain object holding the SAME referent in the same slot.
        let plain = heap.alloc_object(ClassId::new(3), 1);
        heap.set_field(plain, 0, Value::Object(Some(referent)));

        heap.begin_concurrent_mark_cycle();

        let referent_addr = referent.as_ptr() as u64;

        let mut from_weak: Vec<u64> = Vec::new();
        heap.visit_refs(weak.as_ptr() as u64, &mut |c| from_weak.push(c));
        assert!(
            !from_weak.contains(&referent_addr),
            "a Weak reference's referent was reported as a STRONG edge; it could \
             never be cleared"
        );

        let mut from_plain: Vec<u64> = Vec::new();
        heap.visit_refs(plain.as_ptr() as u64, &mut |c| from_plain.push(c));
        assert!(
            from_plain.contains(&referent_addr),
            "an ordinary field holding the same referent must still be traced"
        );

        heap.end_concurrent_mark_cycle();
    }

    /// The snapshot is taken ONCE, at cycle start, and held: a `Reference`
    /// discovered after the snapshot does not retroactively acquire a hole.
    /// This is what makes referent immortality independent of thread timing
    /// rather than a race between a worker and a mutator allocating a
    /// `WeakReference`.
    #[test]
    fn zmark_skip_set_is_a_cycle_long_snapshot() {
        let heap = ZgcRealHeap::new();
        let weak = heap.alloc_object(ClassId::new(1), 1);
        let referent = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(weak, 0, Value::Object(Some(referent)));

        // Cycle opens BEFORE discovery, so the snapshot is empty.
        let snapshot = heap.begin_concurrent_mark_cycle();
        assert!(snapshot.is_empty());
        heap.discover_reference(ReferenceType::Weak, weak, referent, None);

        let mut children: Vec<u64> = Vec::new();
        heap.visit_refs(weak.as_ptr() as u64, &mut |c| children.push(c));
        assert!(
            children.contains(&(referent.as_ptr() as u64)),
            "the held snapshot must not observe a mid-cycle discovery"
        );

        // Re-open: the next cycle's snapshot does see it.
        heap.end_concurrent_mark_cycle();
        let snapshot = heap.begin_concurrent_mark_cycle();
        assert!(snapshot.contains(&(weak.as_ptr() as usize)));

        let mut children: Vec<u64> = Vec::new();
        heap.visit_refs(weak.as_ptr() as u64, &mut |c| children.push(c));
        assert!(!children.contains(&(referent.as_ptr() as u64)));
        heap.end_concurrent_mark_cycle();
    }

    /// With no cycle open the fallback leaks (reports the referent) rather
    /// than dropping edges. Pinned as a test because it is a deliberate
    /// choice, not an oversight: over-reporting an edge makes a referent
    /// immortal, under-reporting one frees a live object.
    #[test]
    fn zmark_visit_refs_without_a_cycle_leaks_rather_than_drops() {
        let heap = ZgcRealHeap::new();
        let weak = heap.alloc_object(ClassId::new(1), 1);
        let referent = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(weak, 0, Value::Object(Some(referent)));
        heap.discover_reference(ReferenceType::Weak, weak, referent, None);

        assert!(heap.concurrent_mark_skip_set().is_none());

        let mut children: Vec<u64> = Vec::new();
        heap.visit_refs(weak.as_ptr() as u64, &mut |c| children.push(c));
        assert!(
            children.contains(&(referent.as_ptr() as u64)),
            "no snapshot must mean 'trace everything', never 'skip everything'"
        );
    }

    /// A reference array's elements are strong edges, read at the current
    /// reference width. Reading them at a fixed 8 bytes under narrow oops would
    /// be a defect — but note this was never the state of
    /// `enumerate_references`'s ARRAY arm, which has always strided through
    /// `read_prim_element` (and so through `ref_element_size()`). The fixed
    /// 8-byte read was in its COMPACT FIELD arm, and is now fixed there too.
    #[test]
    fn zmark_visit_refs_walks_a_reference_array() {
        let heap = ZgcRealHeap::new();
        let a = heap.alloc_object(ClassId::new(2), 0);
        let b = heap.alloc_object(ClassId::new(2), 0);
        let arr = heap.alloc_array(ClassId::new(4), ArrayElementType::Reference, 2);
        heap.set_array_element(arr, 0, Value::Object(Some(a))).unwrap();
        heap.set_array_element(arr, 1, Value::Object(Some(b))).unwrap();

        heap.begin_concurrent_mark_cycle();
        let mut children: Vec<u64> = Vec::new();
        heap.visit_refs(arr.as_ptr() as u64, &mut |c| children.push(c));
        heap.end_concurrent_mark_cycle();

        assert!(children.contains(&(a.as_ptr() as u64)));
        assert!(children.contains(&(b.as_ptr() as u64)));
    }

    // ------------------------------------------------------------------
    // TLAB adoption — `ZArenaTlabRegistry` / `ZTlabHeapHooks`
    // ------------------------------------------------------------------
    //
    // Counts and addresses only. No wall-clock assertions: this change is a
    // contention fix, and a shared build host makes a multi-thread wall-time
    // measurement worthless as evidence.

    /// Every object's `[base, base + alloc_size)` extent, from the registry.
    fn extents(heap: &ZgcRealHeap) -> Vec<(usize, usize)> {
        let mut spans: Vec<(usize, usize)> = heap
            .registry
            .bases()
            .into_iter()
            .map(|base| {
                let header = unsafe { &*(base as *const ObjectHeader) };
                let size =
                    ZgcRealHeap::alloc_size(header).expect("test objects are always sizable");
                (base, base + size)
            })
            .collect();
        spans.sort_unstable();
        spans
    }

    #[test]
    fn tlab_is_on_by_default_and_serves_the_fast_path() {
        let heap = ZgcRealHeap::new();
        assert!(
            heap.tlab_enabled(),
            "CRATONVM_ZGC_TLAB defaults ON — a 64 MB heap is far above the \
             geometry floor"
        );
        for _ in 0..64 {
            heap.alloc_object(ClassId::new(11), 4);
        }
        let stats = heap.tlab_stats();
        assert_eq!(
            stats.refills, 1,
            "64 small objects must fit one chunk — more refills means the \
             chunk geometry regressed"
        );
        assert_eq!(stats.fast_allocations, 64);
        assert!(stats.fast_bytes > 0);
    }

    /// A heap too small to carve a chunk from disables the TLAB by geometry,
    /// with no flag involved — and still allocates.
    #[test]
    fn tlab_disabled_by_geometry_on_a_tiny_heap() {
        let heap = ZgcRealHeap::with_capacity(8 * 1024);
        assert!(
            !heap.tlab_enabled(),
            "one chunk would be the whole heap; the buffer must stand down"
        );
        let obj = heap.alloc_object(ClassId::new(1), 1);
        assert!(heap.is_object_address(obj.as_ptr() as usize).is_some());
        assert_eq!(heap.tlab_stats().fast_allocations, 0);
    }

    /// N threads bumping their own buffers must never hand out overlapping
    /// memory. This is the property the whole change rests on: the arena mutex
    /// is gone from the fast path, so nothing but per-thread chunk ownership
    /// keeps two threads apart.
    #[test]
    fn tlab_concurrent_allocation_never_overlaps() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 300;

        let heap = std::sync::Arc::new(ZgcRealHeap::new());
        assert!(heap.tlab_enabled());

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let heap = std::sync::Arc::clone(&heap);
            handles.push(std::thread::spawn(move || {
                let mut mine = Vec::with_capacity(PER_THREAD);
                for i in 0..PER_THREAD {
                    // Mixed shapes so the threads do not march in lockstep.
                    let obj = heap.alloc_object(ClassId::new(20 + t as u32), 1 + (i % 5));
                    mine.push(obj.as_ptr() as usize);
                }
                mine
            }));
        }
        let mut handed_out: Vec<usize> = Vec::new();
        for h in handles {
            handed_out.extend(h.join().expect("allocating thread panicked"));
        }
        assert_eq!(handed_out.len(), THREADS * PER_THREAD);

        // No address handed out twice...
        let mut sorted = handed_out.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            handed_out.len(),
            "two threads were handed the same base address"
        );

        // ...and no two allocations' FOOTPRINTS overlap either, which is the
        // stronger statement: distinct bases inside one another's bodies would
        // still be corruption.
        let spans = extents(&heap);
        for w in spans.windows(2) {
            assert!(
                w[0].1 <= w[1].0,
                "overlapping allocations: [{:#x},{:#x}) and [{:#x},{:#x})",
                w[0].0,
                w[0].1,
                w[1].0,
                w[1].1,
            );
        }
    }

    /// Every TLAB-served object is a registered object base, and after a
    /// retire no buffer publishes a reserved tail.
    #[test]
    fn tlab_objects_are_registered_and_retire_clears_every_tail() {
        let heap = ZgcRealHeap::new();
        let mut objs = Vec::new();
        for i in 0..200 {
            objs.push(heap.alloc_object(ClassId::new(12), 1 + (i % 3)));
        }
        // Registration is at hand-out, not at retire — batching was reverted
        // because `is_object_address` is a mutator-path oracle here.
        for o in &objs {
            assert!(
                heap.is_object_address(o.as_ptr() as usize).is_some(),
                "a TLAB-served object was invisible to is_object_address \
                 BEFORE any retire"
            );
        }
        assert!(
            !heap.tlab_reserved_tails().is_empty(),
            "a partly-used chunk must publish a reserved tail, or the tripwire \
             is measuring nothing"
        );

        let summary = heap.retire_all_tlabs();
        assert_eq!(summary.tlabs, 1);
        assert_eq!(summary.live_chunks, 1);
        assert!(summary.tail_bytes_returned > 0);
        assert!(
            heap.tlab_reserved_tails().is_empty(),
            "retire_all_tlabs must leave no reserved tail"
        );
        for o in &objs {
            assert!(heap.is_object_address(o.as_ptr() as usize).is_some());
        }
        // Idempotent.
        let again = heap.retire_all_tlabs();
        assert_eq!(again.live_chunks, 0);
        assert_eq!(again.tail_bytes_returned, 0);
    }

    /// A retired chunk's tail goes back to the arena free list rather than
    /// being abandoned. Without this, `retire_all_tlabs` (which runs at EVERY
    /// collection) would leak up to one chunk per thread per cycle into spans
    /// the registry-driven sweep can never visit.
    #[test]
    fn tlab_retire_returns_the_chunk_tail_to_the_arena() {
        let heap = ZgcRealHeap::new();
        heap.alloc_object(ClassId::new(13), 1);
        let free_before = heap.arena.lock().free_list_bytes();
        let summary = heap.retire_all_tlabs();
        let free_after = heap.arena.lock().free_list_bytes();
        assert!(summary.tail_bytes_returned > 0);
        assert_eq!(
            free_after - free_before,
            summary.tail_bytes_returned as usize,
            "every reported tail byte must actually be on the free list",
        );
        assert_eq!(
            heap.tlab_tail_returned_bytes(),
            summary.tail_bytes_returned,
            "the per-buffer counter and the summary must agree",
        );
        // Nothing was abandoned, so the shared `waste_bytes` field must stay 0.
        assert_eq!(heap.tlab_stats().waste_bytes, 0);
    }

    /// A collection after TLAB allocation reclaims exactly the unreachable set
    /// and leaves the survivor usable — the ordinary contract, unchanged by the
    /// buffers.
    #[test]
    fn tlab_collection_reclaims_correctly() {
        let heap = ZgcRealHeap::new();
        let live = heap.alloc_object(ClassId::new(14), 2);
        let reachable = heap.alloc_object(ClassId::new(14), 1);
        heap.set_field(live, 0, Value::Object(Some(reachable)));
        for _ in 0..300 {
            heap.alloc_object(ClassId::new(14), 2);
        }
        let before = heap.allocated_bytes();
        assert!(before > 0);

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots = [live];
        let result = heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        assert_eq!(
            result.stats.objects_copied, 2,
            "only the root and the object it points at may survive"
        );
        assert!(result.stats.bytes_freed > 0);
        assert!(heap.allocated_bytes() < before);
        // Non-moving: the survivor did not move and is still readable.
        assert_eq!(roots[0].as_ptr(), live.as_ptr());
        heap.set_field(live, 1, Value::Int(99));
        assert_eq!(heap.get_field(live, 1), Value::Int(99));
        match heap.get_field(live, 0) {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr(), reachable.as_ptr()),
            other => panic!("expected the reachable child, got {other:?}"),
        }
        // The cycle retired every buffer on its way in.
        assert!(heap.tlab_reserved_tails().is_empty());
        // And the heap is still usable afterwards.
        let fresh = heap.alloc_object(ClassId::new(14), 1);
        assert!(heap.is_object_address(fresh.as_ptr() as usize).is_some());
    }

    /// The kill switch disables the fast path cleanly: allocation keeps
    /// working (it falls back to `alloc_raw`, the pre-TLAB path), objects stay
    /// registered, and no new chunk is taken.
    #[test]
    fn tlab_kill_switch_disables_cleanly() {
        let heap = ZgcRealHeap::new();
        heap.alloc_object(ClassId::new(15), 1);
        let refills_before = heap.tlab_stats().refills;
        let fast_before = heap.tlab_stats().fast_allocations;
        assert_eq!(refills_before, 1);

        heap.set_tlab_enabled(false);
        assert!(!heap.tlab_enabled());

        let mut objs = Vec::new();
        for _ in 0..64 {
            objs.push(heap.alloc_object(ClassId::new(15), 2));
        }
        let stats = heap.tlab_stats();
        assert_eq!(stats.refills, refills_before, "no chunk may be taken");
        assert_eq!(
            stats.fast_allocations, fast_before,
            "no allocation may take the bump path"
        );
        for o in &objs {
            assert!(heap.is_object_address(o.as_ptr() as usize).is_some());
        }
        assert!(heap.allocated_bytes() > 0);

        // Retiring is NOT gated on the switch: the chunk issued before the
        // flip still has to be closed and returned.
        let summary = heap.retire_all_tlabs();
        assert_eq!(summary.live_chunks, 1);
        assert!(summary.tail_bytes_returned > 0);

        // Turning it back on resumes the fast path.
        heap.set_tlab_enabled(true);
        heap.alloc_object(ClassId::new(15), 2);
        assert_eq!(heap.tlab_stats().refills, refills_before + 1);
    }

    /// `ZTlabHeapHooks` is the contract, so it is exercised directly rather
    /// than only through the allocator.
    #[test]
    fn tlab_hooks_register_charge_and_colour() {
        // `ZTlabHeapHooks` reaches this module through the parent's `use`, via
        // the `use super::*` at the top of this test module.
        let heap = ZgcRealHeap::new();
        // Real allocations, so the addresses are genuine bases (the hook is not
        // allowed to be told about memory that is not one).
        let a = heap.alloc_object(ClassId::new(16), 1).as_ptr() as usize;
        let b = heap.alloc_object(ClassId::new(16), 1).as_ptr() as usize;
        let charged_before = heap.allocated_bytes();

        // Re-registering is idempotent for the set and additive for the
        // counter, which is exactly what the trait promises.
        <ZgcRealHeap as ZTlabHeapHooks>::register_allocations(&heap, &[a, b], 128);
        assert_eq!(heap.allocated_bytes(), charged_before + 128);
        assert!(heap.is_object_address(a).is_some());
        assert!(heap.is_object_address(b).is_some());

        // Zero bytes registers without charging.
        let charged = heap.allocated_bytes();
        <ZgcRealHeap as ZTlabHeapHooks>::register_allocations(&heap, &[a], 0);
        assert_eq!(heap.allocated_bytes(), charged);

        // `note_waste` charges without registering anything.
        <ZgcRealHeap as ZTlabHeapHooks>::note_waste(&heap, 64);
        assert_eq!(heap.allocated_bytes(), charged + 64);

        // The honest colour: this heap stores plain machine pointers.
        assert_eq!(
            <ZgcRealHeap as ZTlabHeapHooks>::allocation_color(&heap),
            crate::zgc::vaddr::ZColor::Remapped
        );
        assert_eq!(
            <ZgcRealHeap as ZTlabHeapHooks>::allocation_color_bit(&heap),
            vaddr::Z_REMAPPED
        );
        // The hash source is live (never zero) even though no allocation path
        // on this heap consults it — arrays get theirs lazily from the mark
        // word.
        assert_ne!(<ZgcRealHeap as ZTlabHeapHooks>::next_hash(&heap), 0);
    }

    /// An object larger than `max_tlab_alloc` bypasses the buffer entirely and
    /// is served (and registered, and charged) by `alloc_raw`.
    #[test]
    fn tlab_large_object_bypasses_the_buffer() {
        let heap = ZgcRealHeap::new();
        heap.alloc_object(ClassId::new(17), 1); // install a chunk
        let fast_before = heap.tlab_stats().fast_allocations;
        let refills_before = heap.tlab_stats().refills;

        // `max_tlab_alloc` is chunk/8 = 8 KiB on a 64 MB heap; a 64 KiB byte[]
        // is comfortably past it.
        let big = heap.alloc_array(ClassId::new(18), ArrayElementType::Byte, 64 * 1024);
        assert!(heap.is_object_address(big.as_ptr() as usize).is_some());
        let stats = heap.tlab_stats();
        assert_eq!(stats.fast_allocations, fast_before, "must not use the bump");
        assert_eq!(stats.refills, refills_before, "must not take a new chunk");
    }

    // ------------------------------------------------------------------
    // Object-start bitmap — `ZObjectStarts` / `ZObjectStartBits`
    // ------------------------------------------------------------------
    //
    // Counts, addresses and answers only. NO wall-clock assertions: this is a
    // throughput fix measured with an ABBA-interleaved benchmark on a quiet
    // host, and a timed assertion inside the unit suite would be a latent CI
    // flake that measures the build host's load instead of this change.

    /// A synthetic 8-aligned arena envelope for the structure-level tests.
    /// 1 MiB of grid is 128 KiB of bitmap — big enough to span many words,
    /// small enough to run anywhere.
    const TEST_SPAN: usize = 1024 * 1024;
    const TEST_BASE: usize = 0x1000_0000;

    /// Every allocated base tests positive, and NOTHING else does.
    ///
    /// The negative half is the load-bearing one: `is_object_address` is the
    /// conservative-root predicate, so a bitmap that aliased an interior or
    /// unaligned address onto its object's bit would hand `ObjectRef`s that are
    /// not object bases to the JIT's `jit_checkcast` and to the root scanner.
    /// It is exactly the check `young_mark::YoungMarkBits` omits (its callers
    /// pre-screen) and the reason that type could not simply be reused.
    #[test]
    fn object_starts_are_exact_over_a_dense_address_window() {
        let heap = ZgcRealHeap::new();
        let mut bases: Vec<usize> = Vec::new();
        for i in 0..64u32 {
            bases.push(
                heap.alloc_object(ClassId::new(21), (i % 5) as usize)
                    .as_ptr() as usize,
            );
            bases.push(
                heap.alloc_array(
                    ClassId::new(22),
                    ArrayElementType::Byte,
                    (i % 7) as usize + 1,
                )
                .as_ptr() as usize,
            );
        }
        let registered: FxHashSet<usize> = bases.iter().copied().collect();
        assert_eq!(registered.len(), bases.len(), "no address served twice");

        // Positive: every base.
        for &b in &bases {
            assert!(
                heap.is_object_address(b).is_some(),
                "allocated base {b:#x} must test positive",
            );
        }

        // Exhaustive over a dense window: EVERY 8-aligned address from a little
        // below the first base to a little past the last must agree with the
        // set, and every +4 offset must be refused outright.
        let lo = bases.iter().copied().min().unwrap() - 64;
        let hi = bases.iter().copied().max().unwrap() + 4096;
        let mut addr = lo;
        while addr < hi {
            assert_eq!(
                heap.is_object_address(addr).is_some(),
                registered.contains(&addr),
                "membership disagreed at {addr:#x}",
            );
            assert!(
                heap.is_object_address(addr + 4).is_none(),
                "an unaligned address is never an object start ({:#x})",
                addr + 4,
            );
            addr += 8;
        }

        // Outside the arena entirely.
        let (arena_lo, arena_hi) = heap.conservative_addr_span().expect("one arena");
        assert!(heap.is_object_address(0).is_none());
        assert!(heap.is_object_address(arena_lo - 8).is_none());
        assert!(heap.is_object_address(arena_hi).is_none());
        assert!(
            heap.is_object_address(arena_hi - 8).is_none(),
            "the unallocated bump tail holds no object starts",
        );
    }

    /// Concurrent inserts from N threads are ALL observed — the property the
    /// `&mut self` `young_mark::ObjectStartBits` cannot provide and the reason
    /// the twin below it exists. `fetch_or` is what makes it hold: adjacent
    /// grid slots share a `u64`, so a load/or/store would drop a neighbour's
    /// bit.
    #[test]
    fn object_starts_observe_every_concurrent_insert() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 4096;
        let starts = ZObjectStarts::with_bitmap(TEST_BASE, TEST_SPAN, true);
        assert!(starts.is_bitmap());

        // Interleave the threads across the SAME words rather than giving each
        // a private region: thread `t` writes slots `t, t+8, t+16, ...`, so
        // every 64-bit word is contended by all eight.
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let starts = &starts;
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        starts.insert(TEST_BASE + ((i * THREADS + t) << 3));
                    }
                });
            }
        });

        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                let addr = TEST_BASE + ((i * THREADS + t) << 3);
                assert!(
                    starts.contains(addr),
                    "lost a concurrent insert at {addr:#x}"
                );
            }
        }
        assert_eq!(
            starts.bases().len(),
            THREADS * PER_THREAD,
            "no extra bits were set",
        );
    }

    /// The sweep's in-place prune clears EXACTLY the dead set: the survivors
    /// stay registered and every reclaimed base stops answering.
    #[test]
    fn sweep_prunes_exactly_the_dead_bases() {
        let heap = ZgcRealHeap::new();
        let live = heap.alloc_object(ClassId::new(23), 2);
        let reachable = heap.alloc_object(ClassId::new(23), 1);
        heap.set_field(live, 0, Value::Object(Some(reachable)));
        let mut garbage: Vec<usize> = Vec::new();
        for _ in 0..300 {
            garbage.push(heap.alloc_object(ClassId::new(23), 2).as_ptr() as usize);
        }
        let before: FxHashSet<usize> = heap.registry.bases().into_iter().collect();
        assert_eq!(before.len(), 302);

        // SAFETY: these unit tests run the heap single-threaded.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots = [live];
        let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);

        let after: FxHashSet<usize> = heap.registry.bases().into_iter().collect();
        let expected: FxHashSet<usize> = [live.as_ptr() as usize, reachable.as_ptr() as usize]
            .into_iter()
            .collect();
        assert_eq!(after, expected, "exactly the two survivors stay registered");
        for g in &garbage {
            assert!(
                heap.is_object_address(*g).is_none(),
                "reclaimed base {g:#x} must stop answering",
            );
        }
    }

    /// The kill switch round-trips: both arms answer every question
    /// identically, so `CRATONVM_ZGC_STARTBITS=0` is a true A/B and not an
    /// approximation of one.
    #[test]
    fn both_object_start_arms_answer_identically() {
        let bits = ZObjectStarts::with_bitmap(TEST_BASE, TEST_SPAN, true);
        let hash = ZObjectStarts::with_bitmap(TEST_BASE, TEST_SPAN, false);
        assert!(bits.is_bitmap());
        assert!(
            !hash.is_bitmap(),
            "the switch selects the pre-change structure"
        );

        let inserted: Vec<usize> = (0..1000).map(|i| TEST_BASE + (i * 24)).collect();
        for &a in &inserted {
            bits.insert(a);
            hash.insert(a);
        }
        let batch: Vec<usize> = (0..16).map(|i| TEST_BASE + 700_000 + (i * 8)).collect();
        bits.insert_all(&batch);
        hash.insert_all(&batch);

        let probes: Vec<usize> = (0..3000)
            .map(|i| TEST_BASE + (i * 8))
            .chain([0, TEST_BASE - 8, TEST_BASE + TEST_SPAN, TEST_BASE + 4])
            .collect();
        for p in &probes {
            assert_eq!(
                bits.contains(*p),
                hash.contains(*p),
                "arms disagreed on {p:#x}",
            );
        }

        let mut a = bits.bases();
        let mut b = hash.bases();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "arms enumerate the same set");

        // Snapshots agree too — this is the mark phase's oracle.
        let (sa, sb) = (bits.snapshot(), hash.snapshot());
        for p in &probes {
            assert_eq!(sa.contains(*p), sb.contains(*p), "snapshots disagreed");
        }
        let mut a = sa.bases();
        let mut b = sb.bases();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);

        // Removal, on both.
        for &r in inserted.iter().take(500) {
            bits.remove(r);
            hash.remove(r);
        }
        for p in &probes {
            assert_eq!(
                bits.contains(*p),
                hash.contains(*p),
                "arms disagreed after prune on {p:#x}",
            );
        }
        assert_eq!(bits.bases().len(), hash.bases().len());
    }

    /// The bitmap is ON by default (the switch is opt-OUT), and the snapshot a
    /// collection takes is frozen: it does not see an allocation made after it.
    #[test]
    fn start_bits_default_on_and_snapshot_is_frozen() {
        let heap = ZgcRealHeap::new();
        assert!(
            heap.registry.is_bitmap(),
            "CRATONVM_ZGC_STARTBITS defaults ON",
        );
        let a = heap.alloc_object(ClassId::new(24), 1);
        let snap = heap.registry.snapshot();
        let b = heap.alloc_object(ClassId::new(24), 1);
        assert!(snap.contains(a.as_ptr() as usize));
        assert!(
            !snap.contains(b.as_ptr() as usize),
            "a snapshot must not drift under the mark loop",
        );
        assert!(heap.registry.contains(b.as_ptr() as usize));
    }

    /// A base the 8-byte grid cannot encode is kept EXACTLY in the overflow
    /// set rather than dropped. Losing one would make `is_object_address` deny
    /// a reachable object and drop it from conservative rooting — a crash, not
    /// a slowdown — which is why the fallback exists even though no real arena
    /// is expected to need it.
    #[test]
    fn object_starts_keep_bases_the_grid_cannot_encode() {
        let starts = ZObjectStarts::with_bitmap(TEST_BASE, TEST_SPAN, true);
        let on_grid = TEST_BASE + 8;
        let off_grid = TEST_BASE + 12; // not 8-aligned relative to the base
        let outside = TEST_BASE + TEST_SPAN + 8; // past the covered span
        starts.insert(on_grid);
        starts.insert(off_grid);
        starts.insert(outside);

        assert!(starts.contains(on_grid));
        assert!(starts.contains(off_grid));
        assert!(starts.contains(outside));
        // The spill must not alias anything onto the grid.
        assert!(!starts.contains(TEST_BASE));
        assert!(!starts.contains(TEST_BASE + 16));

        let mut all = starts.bases();
        all.sort_unstable();
        assert_eq!(all, vec![on_grid, off_grid, outside]);

        let snap = starts.snapshot();
        assert!(snap.contains(off_grid) && snap.contains(outside) && snap.contains(on_grid));

        for a in [on_grid, off_grid, outside] {
            starts.remove(a);
            assert!(!starts.contains(a), "remove must clear {a:#x}");
        }
        assert!(starts.bases().is_empty());
    }

    /// The backwards bit scan must answer EXACTLY what a forward walk answers.
    ///
    /// `nearest_base_at_or_below` replaces an O(arena-span) forward walk with a
    /// backwards scan from `addr`, and `is_heap_addr` treats its answer as
    /// definitive (a lower base cannot cover `addr` without spanning across the
    /// nearest one). If the scan ever disagreed with the walk, conservative root
    /// scanning would silently stop rooting an object — a crash, not a
    /// slowdown. Cross-check it against a brute-force maximum over `bases()`,
    /// probing every 8-byte slot in the covered span so word boundaries, the
    /// low bit of a word, the high bit of a word and the empty prefix below the
    /// first base are all hit.
    #[test]
    fn nearest_base_at_or_below_matches_a_brute_force_walk() {
        let starts = ZObjectStarts::with_bitmap(TEST_BASE, TEST_SPAN, true);
        // Deliberately awkward spacing: one at the very first slot, a pair
        // inside one word, a pair straddling a 64-bit word boundary, and a gap
        // wide enough to force a multi-word backwards scan.
        let inserted: Vec<usize> = [0usize, 8, 24, 504, 512, 520, 4096]
            .iter()
            .map(|d| TEST_BASE + d)
            .filter(|a| *a < TEST_BASE + TEST_SPAN)
            .collect();
        for &a in &inserted {
            starts.insert(a);
        }
        assert!(!starts.has_spill(), "the grid must encode all of these");

        let mut all = starts.bases();
        all.sort_unstable();
        assert_eq!(all, inserted, "the fixture itself must be what we think");

        let probe_end = (TEST_SPAN).min(8192);
        for off in (0..probe_end).step_by(8) {
            let addr = TEST_BASE + off;
            let brute = all.iter().copied().filter(|&b| b <= addr).max();
            assert_eq!(
                starts.nearest_base_at_or_below(addr),
                brute,
                "disagreement at offset {off}",
            );
        }
        // Interior (non-grid) addresses floor onto their slot, which is what
        // extent containment needs.
        assert_eq!(
            starts.nearest_base_at_or_below(TEST_BASE + 519),
            Some(TEST_BASE + 512),
        );
        // A spill removes the ordering guarantee, so the scan must decline and
        // send the caller back to the walk.
        starts.insert(TEST_BASE + 12); // off-grid -> overflow
        assert!(starts.has_spill());
        assert_eq!(starts.nearest_base_at_or_below(TEST_BASE + 4096), None);
    }

    /// `is_heap_addr` still resolves an INTERIOR pointer to its base, and still
    /// refuses everything else — the two screens added in front of its extent
    /// walk (the arena envelope, and the high-water-mark walk bound) are cost
    /// controls and must not change a single answer.
    #[test]
    fn is_heap_addr_still_resolves_interiors_after_the_screens() {
        let heap = ZgcRealHeap::new();
        let arr = heap.alloc_array(ClassId::new(25), ArrayElementType::Byte, 512);
        let base = arr.as_ptr() as usize;
        let obj = heap.alloc_object(ClassId::new(25), 3);

        // Exact bases.
        assert_eq!(
            heap.is_heap_addr(base).map(|o| o.as_ptr() as usize),
            Some(base)
        );
        assert_eq!(
            heap.is_heap_addr(obj.as_ptr() as usize)
                .map(|o| o.as_ptr() as usize),
            Some(obj.as_ptr() as usize),
        );
        // Interior of the array resolves to the array.
        for delta in [8usize, 64, 256, 500] {
            assert_eq!(
                heap.is_heap_addr(base + delta).map(|o| o.as_ptr() as usize),
                Some(base),
                "interior +{delta} must resolve to its base",
            );
        }
        // Off-heap and past-the-cursor words are refused.
        let (lo, hi) = heap.conservative_addr_span().expect("one arena");
        assert!(heap.is_heap_addr(0).is_none());
        assert!(
            heap.is_heap_addr(8).is_none(),
            "a small integer is not a heap word"
        );
        assert!(heap.is_heap_addr(lo - 8).is_none());
        assert!(heap.is_heap_addr(hi).is_none());
        assert!(
            heap.is_heap_addr(hi - 4096).is_none(),
            "the unallocated bump tail is not inside any object",
        );
    }

    // -- Reference arrays as a generic Value store --

    /// A REFERENCE array element is a raw 8-byte pointer, but every backend
    /// must accept an arbitrary `Value` in one: natives across the tree use a
    /// reference array as a generic `Value` store, and the generational heap
    /// has always honoured that by auto-boxing into an `AUTOBOX_CLASS_ID`
    /// wrapper.
    ///
    /// ZGC-real had the WRITE half and no read half, so a `Value::Long(-42)`
    /// came back as the *wrapper object* — an instance of a synthetic class
    /// with no name, no methods and no relation to the primitive. In Java that
    /// made `stream.mapToLong(Long::longValue).toArray()` return all zeros
    /// under `-XX:+UseZGC`, and `boxed()` yield objects printing as `?@2`. No
    /// collection was involved: deterministic, identical with `--nojit` and at
    /// a heap large enough that no GC runs. `mapToInt` survived, because a
    /// 4-byte element never reached the auto-box arm.
    #[test]
    fn reference_array_round_trips_every_value_kind() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, 6);
        let obj = heap.alloc_object(ClassId::new(2), 1);

        let cases = [
            Value::Long(-42),
            Value::Double(2.5),
            Value::Int(7),
            Value::Float(1.5),
            Value::Object(Some(obj)),
            Value::Object(None),
        ];
        for (i, v) in cases.iter().enumerate() {
            heap.set_array_element(arr, i, *v).expect("in bounds");
        }
        for (i, want) in cases.iter().enumerate() {
            let got = heap.get_array_element(arr, i).expect("in bounds");
            match (want, got) {
                (Value::Object(Some(w)), Value::Object(Some(g))) => {
                    assert_eq!(w.as_ptr(), g.as_ptr(), "element {i}: reference identity")
                }
                (w, g) => assert_eq!(*w, g, "element {i} did not round-trip"),
            }
        }
    }

    /// The boxing must not leak into a PRIMITIVE array: those store the value
    /// directly and a wrapper there would be a wrong-width write.
    #[test]
    fn primitive_arrays_are_not_auto_boxed() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Double, 2);
        heap.set_array_element(arr, 0, Value::Double(3.25))
            .expect("in bounds");
        assert_eq!(heap.get_array_element(arr, 0), Ok(Value::Double(3.25)));
    }

    /// An ordinary object stored in a reference array must come back as
    /// itself, not be mistaken for a wrapper. The un-box keys on
    /// `AUTOBOX_CLASS_ID`, so this pins that a real object's class id cannot
    /// collide with it.
    #[test]
    fn an_ordinary_object_is_not_mistaken_for_an_auto_box() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let obj = heap.alloc_object(ClassId::new(3), 1);
        heap.set_field(obj, 0, Value::Int(99));
        heap.set_array_element(arr, 0, Value::Object(Some(obj)))
            .expect("in bounds");
        match heap.get_array_element(arr, 0) {
            Ok(Value::Object(Some(got))) => assert_eq!(got.as_ptr(), obj.as_ptr()),
            other => panic!("expected the object back, got {other:?}"),
        }
    }

    /// A reference-array word that is NOT a live object base must be returned
    /// as-is rather than dereferenced. `autobox_payload` screens through
    /// `is_object_address` for exactly this reason; without the screen the
    /// un-box would read an `ObjectHeader` out of arbitrary memory.
    #[test]
    fn a_non_object_word_in_a_reference_array_is_not_dereferenced() {
        let heap = ZgcRealHeap::with_capacity(64 * 1024);
        let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        // SAFETY: never dereferenced — the point of the test is that the
        // un-box path refuses to dereference an unregistered address.
        let bogus = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
        heap.set_array_element(arr, 0, Value::Object(Some(bogus)))
            .expect("in bounds");
        match heap.get_array_element(arr, 0) {
            Ok(Value::Object(Some(got))) => assert_eq!(got.as_ptr() as usize, 0x1000),
            other => panic!("expected the raw word back, got {other:?}"),
        }
    }
}
