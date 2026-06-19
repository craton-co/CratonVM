// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GC-Interpreter Integration
//!
//! Provides thread-local allocation buffers (TLABs), write barriers,
//! safepoint coordination, and GC root scanning infrastructure that
//! wires the garbage collector into the interpreter and runtime.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Card table constants
// ---------------------------------------------------------------------------

const CARD_CLEAN: u8 = 0;
const CARD_DIRTY: u8 = 1;
#[allow(dead_code)]
const CARD_YOUNG_GEN: u8 = 2;

// ---------------------------------------------------------------------------
// TLAB — Thread-Local Allocation Buffer
// ---------------------------------------------------------------------------

/// Configuration for TLAB sizing.
#[derive(Debug, Clone)]
pub struct TlabConfig {
    pub initial_size: usize,
    pub max_size: usize,
    pub refill_waste_fraction: usize,
    pub min_size: usize,
}

impl Default for TlabConfig {
    fn default() -> Self {
        Self {
            initial_size: 512 * 1024,  // 512 KB
            max_size: 4 * 1024 * 1024, // 4 MB
            refill_waste_fraction: 64, // allow 1/64 waste before refill
            min_size: 2 * 1024,        // 2 KB
        }
    }
}

/// A bump-pointer allocation result from a TLAB.
#[derive(Debug, Clone, PartialEq)]
pub struct TlabAllocation {
    pub address: usize,
    pub size: usize,
}

/// Thread-local allocation buffer — fast bump-pointer allocator.
#[derive(Debug)]
pub struct Tlab {
    pub start: usize,
    pub top: usize,
    pub end: usize,
    pub thread_id: u64,
    pub allocations: u64,
    pub wasted_bytes: usize,
    pub refill_count: u32,
}

impl Tlab {
    /// Create a new TLAB spanning `[start, start + size)` for the given thread.
    pub fn new(start: usize, size: usize, thread_id: u64) -> Self {
        Self {
            start,
            top: start,
            end: start + size,
            thread_id,
            allocations: 0,
            wasted_bytes: 0,
            refill_count: 0,
        }
    }

    /// Fast-path bump-pointer allocation. Returns `None` if the TLAB does not
    /// have enough room.
    pub fn try_allocate(&mut self, size: usize) -> Option<TlabAllocation> {
        if size == 0 {
            return None;
        }
        // Align to 8 bytes
        let aligned = (size + 7) & !7;
        let new_top = self.top + aligned;
        if new_top > self.end {
            return None;
        }
        let addr = self.top;
        self.top = new_top;
        self.allocations += 1;
        Some(TlabAllocation {
            address: addr,
            size: aligned,
        })
    }

    /// Bytes remaining in this TLAB.
    pub fn remaining(&self) -> usize {
        self.end.saturating_sub(self.top)
    }

    /// Whether the TLAB is full (no remaining space).
    pub fn is_full(&self) -> bool {
        self.top >= self.end
    }

    /// Refill the TLAB with a new region from the heap.
    pub fn reset(&mut self, start: usize, size: usize) {
        self.wasted_bytes += self.remaining();
        self.start = start;
        self.top = start;
        self.end = start + size;
        self.refill_count += 1;
    }
}

// ---------------------------------------------------------------------------
// Allocation path
// ---------------------------------------------------------------------------

/// Result of an object/array allocation attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum AllocationResult {
    TlabFastPath {
        address: usize,
    },
    TlabRefill {
        address: usize,
        new_tlab_size: usize,
    },
    SlowPath {
        address: usize,
    },
    OutOfMemory {
        requested: usize,
    },
}

/// Tracks allocation-path statistics and drives the TLAB/slow-path decision.
#[derive(Debug, Default)]
pub struct AllocationPath {
    pub tlab_hit_count: u64,
    pub tlab_refill_count: u64,
    pub slow_path_count: u64,
}

/// Simulated heap address counter for slow-path / refill allocations.
static NEXT_HEAP_ADDR: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0x1000_0000);

fn next_heap_addr(size: usize) -> usize {
    NEXT_HEAP_ADDR.fetch_add(size, Ordering::Relaxed)
}

impl AllocationPath {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate an object of `size` bytes (header included) via the given TLAB.
    /// `class_id` is recorded for debugging but does not affect allocation.
    pub fn allocate_object(
        &mut self,
        tlab: &mut Tlab,
        size: usize,
        _class_id: u32,
    ) -> AllocationResult {
        self.allocate(tlab, size)
    }

    /// Allocate an array: `element_size * length` plus an 16-byte header.
    pub fn allocate_array(
        &mut self,
        tlab: &mut Tlab,
        element_size: usize,
        length: usize,
    ) -> AllocationResult {
        let total = 16 + element_size * length; // 16-byte array header
        self.allocate(tlab, total)
    }

    // Internal allocation logic shared by object and array paths.
    fn allocate(&mut self, tlab: &mut Tlab, size: usize) -> AllocationResult {
        if size == 0 {
            return AllocationResult::OutOfMemory { requested: 0 };
        }

        // 1. TLAB fast path
        if let Some(alloc) = tlab.try_allocate(size) {
            self.tlab_hit_count += 1;
            return AllocationResult::TlabFastPath {
                address: alloc.address,
            };
        }

        // 2. TLAB refill — if remaining waste is acceptable
        let config = TlabConfig::default();
        let waste_limit = config.initial_size / config.refill_waste_fraction;
        if tlab.remaining() <= waste_limit {
            let new_size = config.initial_size;
            let new_start = next_heap_addr(new_size);
            tlab.reset(new_start, new_size);
            self.tlab_refill_count += 1;

            if let Some(alloc) = tlab.try_allocate(size) {
                return AllocationResult::TlabRefill {
                    address: alloc.address,
                    new_tlab_size: new_size,
                };
            }
        }

        // 3. Slow path — object too large for a TLAB or refill failed
        let aligned = (size + 7) & !7;
        let addr = next_heap_addr(aligned);
        self.slow_path_count += 1;
        AllocationResult::SlowPath { address: addr }
    }
}

// ---------------------------------------------------------------------------
// Write barriers
// ---------------------------------------------------------------------------

/// Entry in the SATB (snapshot-at-the-beginning) pre-write barrier queue.
#[derive(Debug, Clone, PartialEq)]
pub struct SatbEntry {
    pub old_ref: usize,
    pub field_addr: usize,
    pub timestamp: u64,
}

/// Statistics about write-barrier activity.
#[derive(Debug, Clone, Default)]
pub struct WriteBarrierStats {
    pub dirty_card_count: u64,
    pub satb_entries: u64,
    pub cards_cleared: u64,
}

/// Card-table + SATB write-barrier implementation.
#[derive(Debug)]
pub struct WriteBarrier {
    pub card_table: Vec<u8>,
    pub card_table_base: usize,
    pub card_shift: usize,
    pub satb_queue: Vec<SatbEntry>,
    pub satb_queue_capacity: usize,
    pub dirty_card_count: u64,
    pub satb_entries_count: u64,
    cards_cleared: u64,
}

impl WriteBarrier {
    /// Create a write barrier covering `heap_size` bytes starting at `base`.
    pub fn new(base: usize, heap_size: usize) -> Self {
        let card_shift = 9; // 512-byte cards
        let card_count = (heap_size >> card_shift) + 1;
        Self {
            card_table: vec![CARD_CLEAN; card_count],
            card_table_base: base,
            card_shift,
            satb_queue: Vec::new(),
            satb_queue_capacity: 256,
            dirty_card_count: 0,
            satb_entries_count: 0,
            cards_cleared: 0,
        }
    }

    fn card_index(&self, addr: usize) -> usize {
        (addr.saturating_sub(self.card_table_base)) >> self.card_shift
    }

    /// Post-write barrier: mark the card containing `object_addr` as dirty.
    pub fn post_write_barrier(&mut self, object_addr: usize) {
        let idx = self.card_index(object_addr);
        if idx < self.card_table.len() {
            if self.card_table[idx] != CARD_DIRTY {
                self.card_table[idx] = CARD_DIRTY;
                self.dirty_card_count += 1;
            }
        }
    }

    /// Pre-write (SATB) barrier: enqueue the old reference before overwrite.
    pub fn pre_write_barrier(&mut self, old_ref: usize) {
        if old_ref == 0 {
            return;
        }
        let entry = SatbEntry {
            old_ref,
            field_addr: 0,
            timestamp: self.satb_entries_count,
        };
        self.satb_queue.push(entry);
        self.satb_entries_count += 1;

        // Auto-flush when queue is at capacity (callers should drain periodically).
    }

    /// Check whether the card for `addr` is dirty.
    pub fn is_card_dirty(&self, addr: usize) -> bool {
        let idx = self.card_index(addr);
        idx < self.card_table.len() && self.card_table[idx] == CARD_DIRTY
    }

    /// Clear (mark clean) the card for `addr`.
    pub fn clear_card(&mut self, addr: usize) {
        let idx = self.card_index(addr);
        if idx < self.card_table.len() {
            self.card_table[idx] = CARD_CLEAN;
            self.cards_cleared += 1;
        }
    }

    /// Clear all cards in the table.
    pub fn clear_all_cards(&mut self) {
        for card in self.card_table.iter_mut() {
            if *card == CARD_DIRTY {
                *card = CARD_CLEAN;
                self.cards_cleared += 1;
            }
        }
    }

    /// Return the base addresses of all dirty cards.
    pub fn dirty_cards(&self) -> Vec<usize> {
        self.card_table
            .iter()
            .enumerate()
            .filter(|(_, &c)| c == CARD_DIRTY)
            .map(|(i, _)| self.card_table_base + (i << self.card_shift))
            .collect()
    }

    /// Drain and return the SATB queue.
    pub fn flush_satb_queue(&mut self) -> Vec<SatbEntry> {
        std::mem::take(&mut self.satb_queue)
    }

    /// Snapshot of write-barrier statistics.
    pub fn get_stats(&self) -> WriteBarrierStats {
        WriteBarrierStats {
            dirty_card_count: self.dirty_card_count,
            satb_entries: self.satb_entries_count,
            cards_cleared: self.cards_cleared,
        }
    }
}

// ---------------------------------------------------------------------------
// Safepoints
// ---------------------------------------------------------------------------

/// Reason for requesting a safepoint.
#[derive(Debug, Clone, PartialEq)]
pub enum SafepointReason {
    GC,
    Deoptimization,
    ThreadDump,
    ClassRedefinition,
    BiasedLockRevocation,
}

/// Returned when a safepoint is requested.
#[derive(Debug)]
pub struct SafepointRequest {
    pub id: u64,
    pub reason: SafepointReason,
}

/// Safepoint statistics.
#[derive(Debug, Clone, Default)]
pub struct SafepointStats {
    pub count: u64,
    pub total_pause_ns: u64,
    pub max_pause_ns: u64,
    pub avg_pause_ns: u64,
}

/// Coordinates global safepoints for stop-the-world operations.
pub struct SafepointManager {
    pub safepoint_requested: AtomicBool,
    pub threads_at_safepoint: AtomicU32,
    pub total_threads: AtomicU32,
    safepoint_count: u64,
    total_pause_ns: u64,
    max_pause_ns: u64,
}

impl SafepointManager {
    pub fn new() -> Self {
        Self {
            safepoint_requested: AtomicBool::new(false),
            threads_at_safepoint: AtomicU32::new(0),
            total_threads: AtomicU32::new(0),
            safepoint_count: 0,
            total_pause_ns: 0,
            max_pause_ns: 0,
        }
    }

    /// Request a global safepoint for the given `reason`.
    pub fn request_safepoint(&mut self, reason: SafepointReason) -> SafepointRequest {
        self.safepoint_requested.store(true, Ordering::SeqCst);
        self.safepoint_count += 1;
        SafepointRequest {
            id: self.safepoint_count,
            reason,
        }
    }

    /// Poll whether a safepoint has been requested (called at method entry,
    /// back-edges, etc.).
    pub fn poll_safepoint(&self) -> bool {
        self.safepoint_requested.load(Ordering::SeqCst)
    }

    /// Signal that the current thread has reached the safepoint.
    pub fn thread_at_safepoint(&self) {
        self.threads_at_safepoint.fetch_add(1, Ordering::SeqCst);
    }

    /// Signal that the current thread is leaving the safepoint.
    pub fn thread_left_safepoint(&self) {
        self.threads_at_safepoint.fetch_sub(1, Ordering::SeqCst);
    }

    /// Check whether all registered threads are at the safepoint.
    pub fn all_threads_at_safepoint(&self) -> bool {
        let at = self.threads_at_safepoint.load(Ordering::SeqCst);
        let total = self.total_threads.load(Ordering::SeqCst);
        total > 0 && at >= total
    }

    /// Clear the safepoint request and record pause time.
    pub fn clear_safepoint(&mut self, pause_ns: u64) {
        self.safepoint_requested.store(false, Ordering::SeqCst);
        self.threads_at_safepoint.store(0, Ordering::SeqCst);
        self.total_pause_ns += pause_ns;
        if pause_ns > self.max_pause_ns {
            self.max_pause_ns = pause_ns;
        }
    }

    /// Return safepoint statistics.
    pub fn get_stats(&self) -> SafepointStats {
        let count = self.safepoint_count;
        SafepointStats {
            count,
            total_pause_ns: self.total_pause_ns,
            max_pause_ns: self.max_pause_ns,
            avg_pause_ns: if count > 0 {
                self.total_pause_ns / count
            } else {
                0
            },
        }
    }
}

impl Default for SafepointManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// GC Root Scanning
// ---------------------------------------------------------------------------

/// Classification of a GC root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GcRootType {
    ThreadStack,
    JniLocal,
    JniGlobal,
    ClassStatic,
    InternedString,
    MonitorLock,
    CodeCache,
}

/// A single GC root discovered during scanning.
#[derive(Debug, Clone)]
pub struct GcRoot {
    pub address: usize,
    pub root_type: GcRootType,
    pub description: String,
}

/// A slot in a thread's stack frame.
#[derive(Debug, Clone)]
pub struct StackSlot {
    pub address: usize,
    pub slot_type: SlotType,
}

/// Whether a stack slot holds a reference or a primitive.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotType {
    Reference,
    Primitive,
}

/// Representation of a thread's stack for root scanning.
#[derive(Debug, Clone)]
pub struct ThreadStack {
    pub thread_id: u64,
    pub frames: Vec<StackSlot>,
}

/// A JNI handle (local or global).
#[derive(Debug, Clone)]
pub struct JniHandle {
    pub address: usize,
    pub is_global: bool,
}

/// A static field in a loaded class.
#[derive(Debug, Clone)]
pub struct StaticField {
    pub class_name: String,
    pub field_name: String,
    pub address: usize,
}

/// An interned string entry.
#[derive(Debug, Clone)]
pub struct InternedString {
    pub hash: u64,
    pub address: usize,
}

/// The complete set of roots to scan.
#[derive(Debug, Clone, Default)]
pub struct RootSet {
    pub stacks: Vec<ThreadStack>,
    pub jni_handles: Vec<JniHandle>,
    pub class_statics: Vec<StaticField>,
    pub interned_strings: Vec<InternedString>,
}

/// Result of a full root scan.
#[derive(Debug)]
pub struct GcRootScanResult {
    pub total_roots: usize,
    pub by_type: HashMap<GcRootType, usize>,
    pub scan_time_ns: u64,
}

/// Scans all GC root categories.
pub struct GcRootScanner;

impl GcRootScanner {
    /// Scan thread stacks for references.
    pub fn scan_thread_stacks(stacks: &[ThreadStack]) -> Vec<GcRoot> {
        let mut roots = Vec::new();
        for stack in stacks {
            for slot in &stack.frames {
                if slot.slot_type == SlotType::Reference && slot.address != 0 {
                    roots.push(GcRoot {
                        address: slot.address,
                        root_type: GcRootType::ThreadStack,
                        description: format!("thread-{} stack ref", stack.thread_id),
                    });
                }
            }
        }
        roots
    }

    /// Scan JNI handles (local and global).
    pub fn scan_jni_handles(handles: &[JniHandle]) -> Vec<GcRoot> {
        handles
            .iter()
            .filter(|h| h.address != 0)
            .map(|h| GcRoot {
                address: h.address,
                root_type: if h.is_global {
                    GcRootType::JniGlobal
                } else {
                    GcRootType::JniLocal
                },
                description: format!(
                    "JNI {} handle",
                    if h.is_global { "global" } else { "local" }
                ),
            })
            .collect()
    }

    /// Scan class static fields for references.
    pub fn scan_class_statics(statics: &[StaticField]) -> Vec<GcRoot> {
        statics
            .iter()
            .filter(|s| s.address != 0)
            .map(|s| GcRoot {
                address: s.address,
                root_type: GcRootType::ClassStatic,
                description: format!("{}.{}", s.class_name, s.field_name),
            })
            .collect()
    }

    /// Scan interned string table.
    pub fn scan_interned_strings(strings: &[InternedString]) -> Vec<GcRoot> {
        strings
            .iter()
            .filter(|s| s.address != 0)
            .map(|s| GcRoot {
                address: s.address,
                root_type: GcRootType::InternedString,
                description: format!("interned string hash={}", s.hash),
            })
            .collect()
    }

    /// Perform a full root scan across all categories.
    pub fn scan_all(roots: &RootSet) -> GcRootScanResult {
        let start = Instant::now();

        let mut all_roots: Vec<GcRoot> = Vec::new();
        all_roots.extend(Self::scan_thread_stacks(&roots.stacks));
        all_roots.extend(Self::scan_jni_handles(&roots.jni_handles));
        all_roots.extend(Self::scan_class_statics(&roots.class_statics));
        all_roots.extend(Self::scan_interned_strings(&roots.interned_strings));

        let mut by_type: HashMap<GcRootType, usize> = HashMap::new();
        for root in &all_roots {
            *by_type.entry(root.root_type.clone()).or_insert(0) += 1;
        }

        let elapsed = start.elapsed().as_nanos() as u64;

        GcRootScanResult {
            total_roots: all_roots.len(),
            by_type,
            scan_time_ns: elapsed,
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // TLAB tests
    // -----------------------------------------------------------------------

    #[test]
    fn tlab_new_has_correct_bounds() {
        let tlab = Tlab::new(0x1000, 1024, 1);
        assert_eq!(tlab.start, 0x1000);
        assert_eq!(tlab.top, 0x1000);
        assert_eq!(tlab.end, 0x1000 + 1024);
        assert_eq!(tlab.thread_id, 1);
        assert_eq!(tlab.remaining(), 1024);
    }

    #[test]
    fn tlab_fast_path_allocation() {
        let mut tlab = Tlab::new(0x1000, 1024, 1);
        let alloc = tlab.try_allocate(64).unwrap();
        assert_eq!(alloc.address, 0x1000);
        assert_eq!(alloc.size, 64);
        assert_eq!(tlab.allocations, 1);
        assert_eq!(tlab.remaining(), 1024 - 64);
    }

    #[test]
    fn tlab_allocations_are_8byte_aligned() {
        let mut tlab = Tlab::new(0x1000, 1024, 1);
        let a1 = tlab.try_allocate(13).unwrap();
        assert_eq!(a1.size, 16); // 13 rounded up to 16
        let a2 = tlab.try_allocate(1).unwrap();
        assert_eq!(a2.address, 0x1000 + 16);
        assert_eq!(a2.size, 8); // 1 rounded up to 8
    }

    #[test]
    fn tlab_multiple_allocations() {
        let mut tlab = Tlab::new(0x1000, 256, 1);
        let a1 = tlab.try_allocate(64).unwrap();
        let a2 = tlab.try_allocate(64).unwrap();
        let a3 = tlab.try_allocate(64).unwrap();
        assert_eq!(a1.address, 0x1000);
        assert_eq!(a2.address, 0x1000 + 64);
        assert_eq!(a3.address, 0x1000 + 128);
        assert_eq!(tlab.allocations, 3);
    }

    #[test]
    fn tlab_exhaustion_returns_none() {
        let mut tlab = Tlab::new(0x1000, 64, 1);
        assert!(tlab.try_allocate(32).is_some());
        assert!(tlab.try_allocate(32).is_some());
        assert!(tlab.try_allocate(8).is_none());
        assert!(tlab.is_full());
    }

    #[test]
    fn tlab_zero_size_returns_none() {
        let mut tlab = Tlab::new(0x1000, 1024, 1);
        assert!(tlab.try_allocate(0).is_none());
    }

    #[test]
    fn tlab_is_full_initially_false() {
        let tlab = Tlab::new(0x1000, 64, 1);
        assert!(!tlab.is_full());
    }

    #[test]
    fn tlab_reset_refills() {
        let mut tlab = Tlab::new(0x1000, 64, 1);
        tlab.try_allocate(48).unwrap();
        let remaining_before = tlab.remaining();
        tlab.reset(0x2000, 128);
        assert_eq!(tlab.start, 0x2000);
        assert_eq!(tlab.top, 0x2000);
        assert_eq!(tlab.end, 0x2000 + 128);
        assert_eq!(tlab.remaining(), 128);
        assert_eq!(tlab.wasted_bytes, remaining_before);
        assert_eq!(tlab.refill_count, 1);
    }

    #[test]
    fn tlab_remaining_decreases() {
        let mut tlab = Tlab::new(0x1000, 256, 1);
        assert_eq!(tlab.remaining(), 256);
        tlab.try_allocate(64).unwrap();
        assert_eq!(tlab.remaining(), 192);
    }

    #[test]
    fn tlab_config_defaults() {
        let cfg = TlabConfig::default();
        assert_eq!(cfg.initial_size, 512 * 1024);
        assert_eq!(cfg.max_size, 4 * 1024 * 1024);
        assert_eq!(cfg.refill_waste_fraction, 64);
        assert_eq!(cfg.min_size, 2 * 1024);
    }

    // -----------------------------------------------------------------------
    // AllocationPath tests
    // -----------------------------------------------------------------------

    #[test]
    fn alloc_path_fast_path() {
        let mut path = AllocationPath::new();
        let mut tlab = Tlab::new(0x1000, 4096, 1);
        let result = path.allocate_object(&mut tlab, 64, 1);
        assert!(matches!(result, AllocationResult::TlabFastPath { .. }));
        assert_eq!(path.tlab_hit_count, 1);
    }

    #[test]
    fn alloc_path_refill() {
        let mut path = AllocationPath::new();
        // Create a tiny TLAB that will be exhausted
        let mut tlab = Tlab::new(0x1000, 32, 1);
        tlab.try_allocate(32).unwrap(); // exhaust it
        let result = path.allocate_object(&mut tlab, 64, 1);
        assert!(matches!(result, AllocationResult::TlabRefill { .. }));
        assert_eq!(path.tlab_refill_count, 1);
    }

    #[test]
    fn alloc_path_slow_path_large_object() {
        let mut path = AllocationPath::new();
        // TLAB is small, object is huge — even after refill it won't fit
        let mut tlab = Tlab::new(0x1000, 32, 1);
        tlab.try_allocate(32).unwrap();
        // Request something larger than default TLAB initial_size
        let result = path.allocate_object(&mut tlab, 1024 * 1024, 1);
        assert!(matches!(result, AllocationResult::SlowPath { .. }));
        assert_eq!(path.slow_path_count, 1);
    }

    #[test]
    fn alloc_path_array_allocation() {
        let mut path = AllocationPath::new();
        let mut tlab = Tlab::new(0x1000, 4096, 1);
        let result = path.allocate_array(&mut tlab, 4, 10); // 16 + 40 = 56 bytes
        assert!(matches!(result, AllocationResult::TlabFastPath { .. }));
    }

    #[test]
    fn alloc_path_zero_size_returns_oom() {
        let mut path = AllocationPath::new();
        let mut tlab = Tlab::new(0x1000, 4096, 1);
        let result = path.allocate_object(&mut tlab, 0, 1);
        assert!(matches!(result, AllocationResult::OutOfMemory { .. }));
    }

    #[test]
    fn alloc_path_counters_accumulate() {
        let mut path = AllocationPath::new();
        let mut tlab = Tlab::new(0x1000, 4096, 1);
        path.allocate_object(&mut tlab, 64, 1);
        path.allocate_object(&mut tlab, 64, 2);
        path.allocate_object(&mut tlab, 64, 3);
        assert_eq!(path.tlab_hit_count, 3);
    }

    // -----------------------------------------------------------------------
    // Write barrier tests
    // -----------------------------------------------------------------------

    #[test]
    fn write_barrier_card_dirty() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.post_write_barrier(0x200); // card index 1
        assert!(wb.is_card_dirty(0x200));
        assert!(!wb.is_card_dirty(0x0)); // card 0 still clean
    }

    #[test]
    fn write_barrier_card_clean_initially() {
        let wb = WriteBarrier::new(0x0, 64 * 1024);
        assert!(!wb.is_card_dirty(0x0));
        assert!(!wb.is_card_dirty(0x1000));
    }

    #[test]
    fn write_barrier_clear_card() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.post_write_barrier(0x200);
        assert!(wb.is_card_dirty(0x200));
        wb.clear_card(0x200);
        assert!(!wb.is_card_dirty(0x200));
    }

    #[test]
    fn write_barrier_clear_all_cards() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.post_write_barrier(0x0);
        wb.post_write_barrier(0x200);
        wb.post_write_barrier(0x400);
        assert_eq!(wb.dirty_cards().len(), 3);
        wb.clear_all_cards();
        assert_eq!(wb.dirty_cards().len(), 0);
    }

    #[test]
    fn write_barrier_dirty_cards_list() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.post_write_barrier(0x0);
        wb.post_write_barrier(0x200);
        let dirty = wb.dirty_cards();
        assert_eq!(dirty.len(), 2);
        assert!(dirty.contains(&0x0));
        assert!(dirty.contains(&0x200));
    }

    #[test]
    fn write_barrier_duplicate_dirty_no_double_count() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.post_write_barrier(0x200);
        wb.post_write_barrier(0x200);
        assert_eq!(wb.dirty_card_count, 1);
    }

    #[test]
    fn write_barrier_satb_enqueue() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.pre_write_barrier(0xABC);
        assert_eq!(wb.satb_queue.len(), 1);
        assert_eq!(wb.satb_queue[0].old_ref, 0xABC);
    }

    #[test]
    fn write_barrier_satb_null_ignored() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.pre_write_barrier(0);
        assert!(wb.satb_queue.is_empty());
    }

    #[test]
    fn write_barrier_flush_satb() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.pre_write_barrier(0x100);
        wb.pre_write_barrier(0x200);
        let flushed = wb.flush_satb_queue();
        assert_eq!(flushed.len(), 2);
        assert!(wb.satb_queue.is_empty());
    }

    #[test]
    fn write_barrier_stats() {
        let mut wb = WriteBarrier::new(0x0, 64 * 1024);
        wb.post_write_barrier(0x0);
        wb.post_write_barrier(0x200);
        wb.pre_write_barrier(0x100);
        wb.clear_card(0x0);
        let stats = wb.get_stats();
        assert_eq!(stats.dirty_card_count, 2);
        assert_eq!(stats.satb_entries, 1);
        assert_eq!(stats.cards_cleared, 1);
    }

    #[test]
    fn write_barrier_card_young_gen_constant() {
        // Verify constant is available for future use
        assert_eq!(CARD_YOUNG_GEN, 2);
    }

    // -----------------------------------------------------------------------
    // Safepoint tests
    // -----------------------------------------------------------------------

    #[test]
    fn safepoint_not_requested_initially() {
        let mgr = SafepointManager::new();
        assert!(!mgr.poll_safepoint());
    }

    #[test]
    fn safepoint_request_and_poll() {
        let mut mgr = SafepointManager::new();
        let req = mgr.request_safepoint(SafepointReason::GC);
        assert_eq!(req.id, 1);
        assert_eq!(req.reason, SafepointReason::GC);
        assert!(mgr.poll_safepoint());
    }

    #[test]
    fn safepoint_thread_arrival() {
        let mut mgr = SafepointManager::new();
        mgr.total_threads.store(2, Ordering::SeqCst);
        mgr.request_safepoint(SafepointReason::GC);
        assert!(!mgr.all_threads_at_safepoint());
        mgr.thread_at_safepoint();
        assert!(!mgr.all_threads_at_safepoint());
        mgr.thread_at_safepoint();
        assert!(mgr.all_threads_at_safepoint());
    }

    #[test]
    fn safepoint_thread_leave() {
        let mgr = SafepointManager::new();
        mgr.total_threads.store(1, Ordering::SeqCst);
        mgr.thread_at_safepoint();
        assert!(mgr.all_threads_at_safepoint());
        mgr.thread_left_safepoint();
        assert!(!mgr.all_threads_at_safepoint());
    }

    #[test]
    fn safepoint_clear() {
        let mut mgr = SafepointManager::new();
        mgr.request_safepoint(SafepointReason::ThreadDump);
        assert!(mgr.poll_safepoint());
        mgr.clear_safepoint(1000);
        assert!(!mgr.poll_safepoint());
    }

    #[test]
    fn safepoint_stats() {
        let mut mgr = SafepointManager::new();
        mgr.request_safepoint(SafepointReason::GC);
        mgr.clear_safepoint(500);
        mgr.request_safepoint(SafepointReason::Deoptimization);
        mgr.clear_safepoint(1500);
        let stats = mgr.get_stats();
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_pause_ns, 2000);
        assert_eq!(stats.max_pause_ns, 1500);
        assert_eq!(stats.avg_pause_ns, 1000);
    }

    #[test]
    fn safepoint_all_reasons() {
        let mut mgr = SafepointManager::new();
        let reasons = vec![
            SafepointReason::GC,
            SafepointReason::Deoptimization,
            SafepointReason::ThreadDump,
            SafepointReason::ClassRedefinition,
            SafepointReason::BiasedLockRevocation,
        ];
        for reason in reasons {
            let req = mgr.request_safepoint(reason.clone());
            assert_eq!(req.reason, reason);
            mgr.clear_safepoint(0);
        }
        assert_eq!(mgr.get_stats().count, 5);
    }

    #[test]
    fn safepoint_no_threads_means_not_all_at() {
        let mgr = SafepointManager::new();
        // total_threads is 0 — all_threads_at_safepoint should be false
        assert!(!mgr.all_threads_at_safepoint());
    }

    // -----------------------------------------------------------------------
    // GC Root scanning tests
    // -----------------------------------------------------------------------

    #[test]
    fn scan_thread_stacks_finds_references() {
        let stacks = vec![ThreadStack {
            thread_id: 1,
            frames: vec![
                StackSlot {
                    address: 0x100,
                    slot_type: SlotType::Reference,
                },
                StackSlot {
                    address: 0x200,
                    slot_type: SlotType::Primitive,
                },
                StackSlot {
                    address: 0x300,
                    slot_type: SlotType::Reference,
                },
            ],
        }];
        let roots = GcRootScanner::scan_thread_stacks(&stacks);
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|r| r.root_type == GcRootType::ThreadStack));
    }

    #[test]
    fn scan_thread_stacks_skips_null() {
        let stacks = vec![ThreadStack {
            thread_id: 1,
            frames: vec![StackSlot {
                address: 0,
                slot_type: SlotType::Reference,
            }],
        }];
        let roots = GcRootScanner::scan_thread_stacks(&stacks);
        assert_eq!(roots.len(), 0);
    }

    #[test]
    fn scan_jni_handles_local_and_global() {
        let handles = vec![
            JniHandle {
                address: 0x100,
                is_global: false,
            },
            JniHandle {
                address: 0x200,
                is_global: true,
            },
        ];
        let roots = GcRootScanner::scan_jni_handles(&handles);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].root_type, GcRootType::JniLocal);
        assert_eq!(roots[1].root_type, GcRootType::JniGlobal);
    }

    #[test]
    fn scan_jni_handles_skips_null() {
        let handles = vec![JniHandle {
            address: 0,
            is_global: false,
        }];
        let roots = GcRootScanner::scan_jni_handles(&handles);
        assert_eq!(roots.len(), 0);
    }

    #[test]
    fn scan_class_statics() {
        let statics = vec![StaticField {
            class_name: "java/lang/System".into(),
            field_name: "out".into(),
            address: 0x500,
        }];
        let roots = GcRootScanner::scan_class_statics(&statics);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].root_type, GcRootType::ClassStatic);
        assert!(roots[0].description.contains("System"));
    }

    #[test]
    fn scan_class_statics_skips_null() {
        let statics = vec![StaticField {
            class_name: "Foo".into(),
            field_name: "bar".into(),
            address: 0,
        }];
        let roots = GcRootScanner::scan_class_statics(&statics);
        assert_eq!(roots.len(), 0);
    }

    #[test]
    fn scan_interned_strings() {
        let strings = vec![
            InternedString {
                hash: 42,
                address: 0x600,
            },
            InternedString {
                hash: 99,
                address: 0x700,
            },
        ];
        let roots = GcRootScanner::scan_interned_strings(&strings);
        assert_eq!(roots.len(), 2);
        assert!(roots
            .iter()
            .all(|r| r.root_type == GcRootType::InternedString));
    }

    #[test]
    fn scan_interned_strings_skips_null() {
        let strings = vec![InternedString {
            hash: 1,
            address: 0,
        }];
        let roots = GcRootScanner::scan_interned_strings(&strings);
        assert_eq!(roots.len(), 0);
    }

    #[test]
    fn scan_all_aggregates() {
        let root_set = RootSet {
            stacks: vec![ThreadStack {
                thread_id: 1,
                frames: vec![StackSlot {
                    address: 0x100,
                    slot_type: SlotType::Reference,
                }],
            }],
            jni_handles: vec![JniHandle {
                address: 0x200,
                is_global: false,
            }],
            class_statics: vec![StaticField {
                class_name: "A".into(),
                field_name: "x".into(),
                address: 0x300,
            }],
            interned_strings: vec![InternedString {
                hash: 1,
                address: 0x400,
            }],
        };
        let result = GcRootScanner::scan_all(&root_set);
        assert_eq!(result.total_roots, 4);
        assert_eq!(*result.by_type.get(&GcRootType::ThreadStack).unwrap(), 1);
        assert_eq!(*result.by_type.get(&GcRootType::JniLocal).unwrap(), 1);
        assert_eq!(*result.by_type.get(&GcRootType::ClassStatic).unwrap(), 1);
        assert_eq!(*result.by_type.get(&GcRootType::InternedString).unwrap(), 1);
    }

    #[test]
    fn scan_all_empty_root_set() {
        let root_set = RootSet::default();
        let result = GcRootScanner::scan_all(&root_set);
        assert_eq!(result.total_roots, 0);
        assert!(result.by_type.is_empty());
    }

    #[test]
    fn scan_multiple_threads() {
        let stacks = vec![
            ThreadStack {
                thread_id: 1,
                frames: vec![StackSlot {
                    address: 0x100,
                    slot_type: SlotType::Reference,
                }],
            },
            ThreadStack {
                thread_id: 2,
                frames: vec![
                    StackSlot {
                        address: 0x200,
                        slot_type: SlotType::Reference,
                    },
                    StackSlot {
                        address: 0x300,
                        slot_type: SlotType::Reference,
                    },
                ],
            },
        ];
        let roots = GcRootScanner::scan_thread_stacks(&stacks);
        assert_eq!(roots.len(), 3);
    }

    #[test]
    fn gc_root_type_hash_eq() {
        let mut map: HashMap<GcRootType, usize> = HashMap::new();
        map.insert(GcRootType::ThreadStack, 10);
        map.insert(GcRootType::JniLocal, 5);
        assert_eq!(*map.get(&GcRootType::ThreadStack).unwrap(), 10);
        assert_eq!(*map.get(&GcRootType::JniLocal).unwrap(), 5);
    }

    #[test]
    fn tlab_allocation_struct_fields() {
        let alloc = TlabAllocation {
            address: 0x1000,
            size: 64,
        };
        assert_eq!(alloc.address, 0x1000);
        assert_eq!(alloc.size, 64);
    }

    #[test]
    fn allocation_result_variants() {
        let fast = AllocationResult::TlabFastPath { address: 0x1000 };
        let refill = AllocationResult::TlabRefill {
            address: 0x2000,
            new_tlab_size: 512,
        };
        let slow = AllocationResult::SlowPath { address: 0x3000 };
        let oom = AllocationResult::OutOfMemory { requested: 999 };
        assert!(matches!(fast, AllocationResult::TlabFastPath { .. }));
        assert!(matches!(refill, AllocationResult::TlabRefill { .. }));
        assert!(matches!(slow, AllocationResult::SlowPath { .. }));
        assert!(matches!(
            oom,
            AllocationResult::OutOfMemory { requested: 999 }
        ));
    }

    #[test]
    fn satb_entry_fields() {
        let entry = SatbEntry {
            old_ref: 0x100,
            field_addr: 0x200,
            timestamp: 42,
        };
        assert_eq!(entry.old_ref, 0x100);
        assert_eq!(entry.field_addr, 0x200);
        assert_eq!(entry.timestamp, 42);
    }

    #[test]
    fn safepoint_default_impl() {
        let mgr = SafepointManager::default();
        assert!(!mgr.poll_safepoint());
    }
}
