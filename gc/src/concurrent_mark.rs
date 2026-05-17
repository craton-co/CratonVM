//! Concurrent marking for the garbage collector.
//!
//! Implements tri-color marking that runs concurrently with application
//! threads. The marking process has four phases:
//!
//! 1. **Initial Mark (STW, brief):** Mark objects directly reachable from
//!    thread stacks and static fields. This is a short STW pause.
//!
//! 2. **Concurrent Mark:** Traverse the object graph from the initial roots,
//!    marking all reachable objects. Application threads continue running;
//!    the SATB write barrier logs overwritten references.
//!
//! 3. **Remark (STW, brief):** Process SATB buffers and re-scan roots to
//!    catch any references modified during concurrent marking.
//!
//! 4. **Concurrent Sweep:** Walk the old generation, freeing unmarked objects
//!    back to the free list. Application threads continue running.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::Mutex;

use crate::heap::{
    ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, REF_ELEMENT_SIZE,
    SLOT_SIZE,
};
use crate::mark_bitmap::MarkBitmap;
use crate::old_gen::OldGen;
use crate::satb::SatbQueue;
use rustjvm_types::Value;

// ---------------------------------------------------------------------------
// Concurrent GC phase tracking
// ---------------------------------------------------------------------------

/// Current phase of the concurrent GC cycle.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrentGcPhase {
    /// No concurrent GC activity.
    Idle = 0,
    /// Initial mark — brief STW to mark root-reachable objects.
    InitialMark = 1,
    /// Concurrent mark — marker threads traverse the heap.
    ConcurrentMark = 2,
    /// Remark — brief STW to process SATB buffers and re-scan roots.
    Remark = 3,
    /// Concurrent sweep — reclaim unmarked old-gen objects.
    ConcurrentSweep = 4,
}

impl From<u8> for ConcurrentGcPhase {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::InitialMark,
            2 => Self::ConcurrentMark,
            3 => Self::Remark,
            4 => Self::ConcurrentSweep,
            _ => Self::Idle,
        }
    }
}

/// Atomic phase tracker visible to all threads.
pub struct ConcurrentGcState {
    phase: AtomicU8,
}

impl ConcurrentGcState {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(ConcurrentGcPhase::Idle as u8),
        }
    }

    /// Get the current GC phase.
    #[inline]
    pub fn phase(&self) -> ConcurrentGcPhase {
        ConcurrentGcPhase::from(self.phase.load(Ordering::Acquire))
    }

    /// Set the GC phase (called by the GC coordinator).
    pub fn set_phase(&self, phase: ConcurrentGcPhase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    /// Whether concurrent marking is active (SATB barrier should log).
    #[inline]
    pub fn is_marking_active(&self) -> bool {
        let p = self.phase.load(Ordering::Acquire);
        p == ConcurrentGcPhase::ConcurrentMark as u8
            || p == ConcurrentGcPhase::Remark as u8
    }
}

impl Default for ConcurrentGcState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ConcurrentGcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConcurrentGcState({:?})", self.phase())
    }
}

// ---------------------------------------------------------------------------
// Mark queue (work list for concurrent marking)
// ---------------------------------------------------------------------------

/// Thread-safe work queue for concurrent marking.
///
/// Uses a sharded approach: multiple `VecDeque`s behind separate `Mutex`es,
/// selected by hashing the pointer value. This reduces lock contention when
/// multiple marker threads push/pop concurrently, since threads operating on
/// different pointer ranges will typically hit different shards.
pub struct MarkQueue {
    shards: Vec<Mutex<VecDeque<*mut u8>>>,
}

/// Number of shards for the mark queue. Must be a power of two for fast modulo.
const MARK_QUEUE_SHARDS: usize = 8;

thread_local! {
    /// Per-thread round-robin cursor used by [`MarkQueue::pop`] to choose
    /// the shard probe order. Bumping this on every `pop` distributes
    /// marker threads across shards instead of stacking them all on
    /// shard 0 (which is what the original "start at 0" loop did,
    /// defeating the entire point of sharding under contention).
    static POP_CURSOR: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// SAFETY: The raw pointers in the queue are heap object addresses managed
// by the GC; they are valid for the duration of the marking phase. The
// sharded Mutex design ensures exclusive access to each shard.
unsafe impl Send for MarkQueue {}
unsafe impl Sync for MarkQueue {}

impl MarkQueue {
    pub fn new() -> Self {
        let shards = (0..MARK_QUEUE_SHARDS)
            .map(|_| Mutex::new(VecDeque::with_capacity(4096 / MARK_QUEUE_SHARDS)))
            .collect();
        Self { shards }
    }

    /// Select shard index from a pointer value.
    #[inline]
    fn shard_for(ptr: *mut u8) -> usize {
        // Use upper bits after shifting out alignment (objects are 8-byte aligned)
        ((ptr as usize) >> 3) & (MARK_QUEUE_SHARDS - 1)
    }

    /// Push an object onto the mark queue (it becomes gray).
    pub fn push(&self, obj_ptr: *mut u8) {
        let idx = Self::shard_for(obj_ptr);
        self.shards[idx].lock().push_back(obj_ptr);
    }

    /// Pop an object from the mark queue for scanning.
    /// Returns `None` if all shards are empty.
    ///
    /// Each calling thread maintains its own round-robin cursor so that
    /// concurrent markers spread contention evenly across shards instead
    /// of always hammering shard 0 first.
    pub fn pop(&self) -> Option<*mut u8> {
        let start = POP_CURSOR.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        }) & (MARK_QUEUE_SHARDS - 1);
        for offset in 0..MARK_QUEUE_SHARDS {
            let idx = (start + offset) & (MARK_QUEUE_SHARDS - 1);
            if let Some(ptr) = self.shards[idx].lock().pop_front() {
                return Some(ptr);
            }
        }
        None
    }

    /// Push multiple objects at once.
    pub fn push_batch(&self, ptrs: &[*mut u8]) {
        for &ptr in ptrs {
            self.push(ptr);
        }
    }

    /// Number of pending objects in the queue (sum across all shards).
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    /// Whether the queue is empty (all shards empty).
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.lock().is_empty())
    }

    /// Clear all entries from all shards.
    pub fn clear(&self) {
        for shard in &self.shards {
            shard.lock().clear();
        }
    }
}

impl Default for MarkQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Concurrent Marker
// ---------------------------------------------------------------------------

/// The concurrent marker traverses the object graph, marking live objects
/// in the mark bitmap.
pub struct ConcurrentMarker {
    /// Mark bitmap covering the old generation.
    pub bitmap: MarkBitmap,
    /// Work queue of gray objects to scan.
    pub queue: MarkQueue,
    /// Global SATB queue for write barrier entries.
    pub satb_queue: SatbQueue,
    /// Phase tracker.
    pub state: ConcurrentGcState,
}

impl ConcurrentMarker {
    /// Create a new concurrent marker for a heap region.
    pub fn new(old_gen_base: usize, old_gen_size: usize) -> Self {
        Self {
            bitmap: MarkBitmap::new(old_gen_base, old_gen_size),
            queue: MarkQueue::new(),
            satb_queue: SatbQueue::new(),
            state: ConcurrentGcState::new(),
        }
    }

    /// Phase 1: Initial Mark (called during brief STW pause).
    ///
    /// Marks objects directly reachable from roots. Only marks old-gen objects;
    /// young-gen objects are handled by the minor GC.
    ///
    /// Returns the number of root objects marked.
    pub fn initial_mark(&self, roots: &[*mut u8], old_gen: &OldGen) -> usize {
        self.state.set_phase(ConcurrentGcPhase::InitialMark);
        self.bitmap.clear();
        self.queue.clear();

        let mut count = 0;
        for &root_ptr in roots {
            if !root_ptr.is_null() && old_gen.contains(root_ptr) {
                if self.bitmap.try_mark(root_ptr as usize) {
                    self.queue.push(root_ptr);
                    count += 1;
                }
            }
        }

        // Activate SATB barrier for the concurrent phase.
        self.satb_queue.activate();
        self.state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        count
    }

    /// Phase 2: Concurrent Mark — process the mark queue until empty.
    ///
    /// Called by marker thread(s). This runs concurrently with application
    /// threads. The SATB barrier ensures correctness by logging overwritten
    /// references.
    ///
    /// Returns the number of objects scanned.
    pub fn concurrent_mark(&self, old_gen: &OldGen) -> usize {
        let mut scanned = 0;

        while let Some(obj_ptr) = self.queue.pop() {
            self.scan_object(obj_ptr, old_gen);
            scanned += 1;
        }

        scanned
    }

    /// Phase 3: Remark (called during brief STW pause).
    ///
    /// Processes all SATB buffer entries and re-scans roots to catch any
    /// references modified during the concurrent mark phase.
    ///
    /// Returns the number of additional objects discovered.
    pub fn remark(&self, roots: &[*mut u8], old_gen: &OldGen) -> usize {
        self.state.set_phase(ConcurrentGcPhase::Remark);
        let mut discovered = 0;

        // Process SATB entries: these are old reference values that were
        // overwritten during concurrent marking. We must mark them to
        // prevent live objects from being collected.
        let satb_entries = self.satb_queue.drain();
        for addr in satb_entries {
            if addr != 0 && old_gen.contains(addr as *const u8) {
                if self.bitmap.try_mark(addr) {
                    self.queue.push(addr as *mut u8);
                    discovered += 1;
                }
            }
        }

        // Re-scan roots (some may have changed during concurrent mark).
        for &root_ptr in roots {
            if !root_ptr.is_null() && old_gen.contains(root_ptr) {
                if self.bitmap.try_mark(root_ptr as usize) {
                    self.queue.push(root_ptr);
                    discovered += 1;
                }
            }
        }

        // Drain the queue fully (mark transitive closure from new roots).
        while let Some(obj_ptr) = self.queue.pop() {
            self.scan_object(obj_ptr, old_gen);
            discovered += 1;
        }

        // Deactivate SATB barrier — no more logging needed.
        self.satb_queue.deactivate();
        self.state.set_phase(ConcurrentGcPhase::ConcurrentSweep);

        discovered
    }

    /// Phase 4: Concurrent Sweep — reclaim unmarked old-gen objects.
    ///
    /// Walks all allocated objects in the old generation and frees those
    /// that are not marked in the bitmap.
    ///
    /// Returns the number of objects freed.
    pub fn concurrent_sweep(&self, old_gen: &mut OldGen) -> usize {
        let objects = old_gen.walk_objects();
        let mut freed = Vec::new();

        for (obj_ptr, total_size) in objects {
            if !self.bitmap.is_marked(obj_ptr as usize) {
                freed.push((obj_ptr, total_size));
            }
        }

        let freed_count = freed.len();
        for (ptr, size) in freed {
            // SAFETY: ptr and size come from old_gen.walk_objects() which yields
            // valid (pointer, total_size) pairs for allocated objects. The object
            // is unmarked (unreachable), so freeing it is correct.
            unsafe { old_gen.free(ptr, size) };
        }

        self.bitmap.clear();
        self.state.set_phase(ConcurrentGcPhase::Idle);

        freed_count
    }

    /// Scan an object's reference fields and mark any old-gen targets.
    fn scan_object(&self, obj_ptr: *mut u8, old_gen: &OldGen) {
        // SAFETY: obj_ptr was popped from the mark queue, which only contains
        // pointers to valid old-gen objects verified by old_gen.contains() before
        // being enqueued. The header is readable for the lifetime of the GC cycle.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                // Reference array: compact 8-byte pointer per element.
                for i in 0..header.array_length as usize {
                    // SAFETY: i < array_length, offset is within the allocated array object.
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    // SAFETY: slot_ptr points to a valid 8-byte reference element in the array.
                    let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if old_gen.contains(ref_ptr) && self.bitmap.try_mark(ref_ptr as usize) {
                            self.queue.push(ref_ptr);
                        }
                    }
                }
            }
            // Primitive arrays have no references to scan.
        } else {
            // Object: 16-byte Value slots.
            for slot_idx in 0..header.num_slots as usize {
                // SAFETY: slot_idx < num_slots, offset is within the allocated object.
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: slot_ptr points to a valid Value-sized region within the object.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if old_gen.contains(ref_ptr) && self.bitmap.try_mark(ref_ptr as usize) {
                        self.queue.push(ref_ptr);
                    }
                }
            }
        }
    }

    /// Run all four phases of a concurrent GC cycle.
    ///
    /// This is a convenience method for testing. In production, the phases
    /// are coordinated by the GC barrier with STW pauses at phase 1 and 3.
    ///
    /// Returns (objects_marked, objects_swept).
    pub fn full_cycle(
        &self,
        roots: &[*mut u8],
        old_gen: &mut OldGen,
    ) -> (usize, usize) {
        let initial = self.initial_mark(roots, old_gen);
        let concurrent = self.concurrent_mark(old_gen);
        let remark = self.remark(roots, old_gen);
        let swept = self.concurrent_sweep(old_gen);
        (initial + concurrent + remark, swept)
    }
}

impl std::fmt::Debug for ConcurrentMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrentMarker")
            .field("phase", &self.state.phase())
            .field("bitmap", &self.bitmap)
            .field("queue_len", &self.queue.len())
            .field("satb_queue", &self.satb_queue)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ObjectHeader, ObjectKind, ArrayElementType, HEADER_SIZE, SLOT_SIZE};
    use rustjvm_types::{ClassId, ObjectRef, Value};

    fn make_old_gen_with_object(num_slots: u32) -> (OldGen, *mut u8) {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + num_slots as usize * SLOT_SIZE;
        let ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let header = &mut *(ptr as *mut ObjectHeader);
            header.class_id = ClassId::new(1);
            header.kind = ObjectKind::Object;
            header.element_type = ArrayElementType::Byte;
            header.num_slots = num_slots;
            header.gc_age = 0;
            header.gc_flags = 0x01; // GC_FLAG_OLD_GEN
        }
        (og, ptr)
    }

    #[test]
    fn initial_mark_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(2);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let count = marker.initial_mark(&[obj_ptr], &og);
        assert_eq!(count, 1);
        assert!(marker.bitmap.is_marked(obj_ptr as usize));
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
    }

    #[test]
    fn concurrent_mark_follows_references() {
        let mut og = OldGen::new(65536);

        // Allocate object A (2 slots)
        let size_a = HEADER_SIZE + 2 * SLOT_SIZE;
        let ptr_a = og.alloc(size_a, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.kind = ObjectKind::Object;
            h.num_slots = 2;
            h.gc_flags = 0x01;
        }

        // Allocate object B (1 slot, no refs)
        let size_b = HEADER_SIZE + 1 * SLOT_SIZE;
        let ptr_b = og.alloc(size_b, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.kind = ObjectKind::Object;
            h.num_slots = 1;
            h.gc_flags = 0x01;
        }

        // A.field[0] = ref to B
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            let obj_b = ObjectRef::from_raw(ptr_b);
            std::ptr::write(slot, Value::Object(Some(obj_b)));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr_a], &og);
        let scanned = marker.concurrent_mark(&og);

        // Both A and B should be marked
        assert!(marker.bitmap.is_marked(ptr_a as usize));
        assert!(marker.bitmap.is_marked(ptr_b as usize));
        assert!(scanned >= 1); // At least B was scanned via A
    }

    #[test]
    fn sweep_frees_unmarked() {
        let mut og = OldGen::new(65536);

        // Allocate two objects
        let size = HEADER_SIZE + 1 * SLOT_SIZE;
        let live_ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(live_ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.kind = ObjectKind::Object;
            h.num_slots = 1;
            h.gc_flags = 0x01;
        }

        let dead_ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(dead_ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.kind = ObjectKind::Object;
            h.num_slots = 1;
            h.gc_flags = 0x01;
        }

        let used_before = og.used();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Only mark live_ptr as a root (dead_ptr is unreachable)
        let (_marked, swept) = marker.full_cycle(&[live_ptr], &mut og);
        assert_eq!(swept, 1); // dead_ptr should be freed
        assert!(og.used() < used_before);
    }

    #[test]
    fn satb_prevents_lost_object() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + 1 * SLOT_SIZE;

        // Object A (root)
        let ptr_a = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.kind = ObjectKind::Object;
            h.num_slots = 1;
            h.gc_flags = 0x01;
        }

        // Object B (initially referenced by A)
        let ptr_b = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.kind = ObjectKind::Object;
            h.num_slots = 1;
            h.gc_flags = 0x01;
        }

        // A.field[0] = B initially
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Phase 1: initial mark
        marker.initial_mark(&[ptr_a], &og);

        // Simulate concurrent mutation: A.field[0] = null
        // The SATB barrier should log the OLD value (ptr_b).
        marker.satb_queue.flush(vec![ptr_b as usize]);

        // Now break the reference
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(None));
        }

        // Phase 2: concurrent mark (won't find B via A anymore)
        marker.concurrent_mark(&og);

        // Phase 3: remark — should discover B from SATB
        let discovered = marker.remark(&[ptr_a], &og);
        assert!(discovered > 0); // B should be re-discovered from SATB

        // Phase 4: sweep — B should NOT be freed
        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(swept, 0); // Both A and B are live
    }

    #[test]
    fn phase_transitions() {
        let marker = ConcurrentMarker::new(0x0, 1024);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        let og = OldGen::new(1024);
        marker.initial_mark(&[], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
        assert!(marker.satb_queue.is_active());

        marker.concurrent_mark(&og);
        // Phase doesn't change after concurrent mark — stays ConcurrentMark

        marker.remark(&[], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn initial_mark_multiple_roots() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + 1 * SLOT_SIZE;

        let ptrs: Vec<*mut u8> = (0..5)
            .map(|i| {
                let p = og.alloc(size, 8).unwrap();
                unsafe {
                    let h = &mut *(p as *mut ObjectHeader);
                    h.class_id = ClassId::new(i + 1);
                    h.kind = ObjectKind::Object;
                    h.num_slots = 1;
                    h.gc_flags = 0x01;
                }
                p
            })
            .collect();

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let count = marker.initial_mark(&ptrs, &og);

        assert_eq!(count, 5);
        for &p in &ptrs {
            assert!(marker.bitmap.is_marked(p as usize));
        }
        assert_eq!(marker.queue.len(), 5);
    }

    #[test]
    fn initial_mark_skips_null_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let roots: Vec<*mut u8> = vec![std::ptr::null_mut(), obj_ptr, std::ptr::null_mut()];
        let count = marker.initial_mark(&roots, &og);

        assert_eq!(count, 1);
        assert!(marker.bitmap.is_marked(obj_ptr as usize));
    }

    #[test]
    fn initial_mark_deduplicates_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Same root listed twice — should only mark once
        let count = marker.initial_mark(&[obj_ptr, obj_ptr], &og);
        assert_eq!(count, 1);
        assert_eq!(marker.queue.len(), 1);
    }

    #[test]
    fn empty_heap_marking() {
        let og = OldGen::new(4096);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let count = marker.initial_mark(&[], &og);
        assert_eq!(count, 0);

        let scanned = marker.concurrent_mark(&og);
        assert_eq!(scanned, 0);

        let discovered = marker.remark(&[], &og);
        assert_eq!(discovered, 0);
    }

    #[test]
    fn single_object_full_cycle() {
        let (mut og, obj_ptr) = make_old_gen_with_object(0);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let (marked, swept) = marker.full_cycle(&[obj_ptr], &mut og);
        // The single object is a root, so it should survive
        assert!(marked >= 1);
        assert_eq!(swept, 0);
    }

    #[test]
    fn mark_queue_push_pop_ordering() {
        let q = MarkQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);

        q.push(0x100 as *mut u8);
        q.push(0x200 as *mut u8);
        q.push(0x300 as *mut u8);
        assert_eq!(q.len(), 3);

        // FIFO ordering
        assert_eq!(q.pop().unwrap() as usize, 0x100);
        assert_eq!(q.pop().unwrap() as usize, 0x200);
        assert_eq!(q.pop().unwrap() as usize, 0x300);
        assert!(q.pop().is_none());
        assert!(q.is_empty());
    }

    #[test]
    fn mark_queue_push_batch() {
        let q = MarkQueue::new();
        let ptrs: Vec<*mut u8> = (1..=4).map(|i| (i * 0x100) as *mut u8).collect();

        q.push_batch(&ptrs);
        assert_eq!(q.len(), 4);

        for expected in &ptrs {
            assert_eq!(q.pop().unwrap() as usize, *expected as usize);
        }
    }

    #[test]
    fn mark_queue_clear() {
        let q = MarkQueue::new();
        q.push(0x100 as *mut u8);
        q.push(0x200 as *mut u8);
        assert_eq!(q.len(), 2);

        q.clear();
        assert!(q.is_empty());
        assert!(q.pop().is_none());
    }

    #[test]
    fn mark_queue_concurrent_push_pop() {
        use std::sync::Arc;

        let q = Arc::new(MarkQueue::new());
        let mut handles = Vec::new();

        // 4 threads each push 100 items
        for t in 0..4u64 {
            let q = q.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..100u64 {
                    q.push((t * 1000 + i) as *mut u8);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(q.len(), 400);

        // Drain all
        let mut count = 0;
        while q.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 400);
    }

    #[test]
    fn concurrent_gc_state_is_marking_active() {
        let state = ConcurrentGcState::new();
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::InitialMark);
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        assert!(state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::Remark);
        assert!(state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::Idle);
        assert!(!state.is_marking_active());
    }

    #[test]
    fn phase_from_u8_invalid_defaults_to_idle() {
        assert_eq!(ConcurrentGcPhase::from(255), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(5), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(100), ConcurrentGcPhase::Idle);
    }

    #[test]
    fn phase_from_u8_all_valid() {
        assert_eq!(ConcurrentGcPhase::from(0), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(1), ConcurrentGcPhase::InitialMark);
        assert_eq!(ConcurrentGcPhase::from(2), ConcurrentGcPhase::ConcurrentMark);
        assert_eq!(ConcurrentGcPhase::from(3), ConcurrentGcPhase::Remark);
        assert_eq!(ConcurrentGcPhase::from(4), ConcurrentGcPhase::ConcurrentSweep);
    }

    #[test]
    fn large_object_graph_marking() {
        let mut og = OldGen::new(1 << 20); // 1 MB
        let size = HEADER_SIZE + 1 * SLOT_SIZE;

        // Build a chain: obj[0] -> obj[1] -> ... -> obj[N-1]
        let n = 50;
        let mut ptrs: Vec<*mut u8> = Vec::new();
        for i in 0..n {
            let p = og.alloc(size, 8).unwrap();
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(i as u32 + 1);
                h.kind = ObjectKind::Object;
                h.num_slots = 1;
                h.gc_flags = 0x01;
            }
            ptrs.push(p);
        }

        // Wire up chain references: ptrs[i].slot[0] = ptrs[i+1]
        for i in 0..n - 1 {
            unsafe {
                let slot = ptrs[i].add(HEADER_SIZE) as *mut Value;
                std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptrs[i + 1]))));
            }
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // Only root is the first object
        marker.initial_mark(&[ptrs[0]], &og);
        marker.concurrent_mark(&og);

        // All objects in the chain should be marked
        for &p in &ptrs {
            assert!(marker.bitmap.is_marked(p as usize));
        }
    }

    #[test]
    fn marking_graph_with_cycle() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + 1 * SLOT_SIZE;

        // Create A -> B -> C -> A (cycle)
        let ptr_a = og.alloc(size, 8).unwrap();
        let ptr_b = og.alloc(size, 8).unwrap();
        let ptr_c = og.alloc(size, 8).unwrap();

        for (p, id) in [(ptr_a, 1u32), (ptr_b, 2), (ptr_c, 3)] {
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(id);
                h.kind = ObjectKind::Object;
                h.num_slots = 1;
                h.gc_flags = 0x01;
            }
        }

        // A -> B
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
        }
        // B -> C
        unsafe {
            let slot = ptr_b.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_c))));
        }
        // C -> A (back edge creating cycle)
        unsafe {
            let slot = ptr_c.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_a))));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr_a], &og);
        let scanned = marker.concurrent_mark(&og);

        // All three should be marked despite the cycle
        assert!(marker.bitmap.is_marked(ptr_a as usize));
        assert!(marker.bitmap.is_marked(ptr_b as usize));
        assert!(marker.bitmap.is_marked(ptr_c as usize));
        assert!(scanned >= 2); // A scanned in initial_mark's queue, B and C via concurrent
    }

    #[test]
    fn full_cycle_phase_sequence() {
        let (mut og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Before cycle: Idle
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        marker.initial_mark(&[obj_ptr], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
        assert!(marker.satb_queue.is_active());

        marker.concurrent_mark(&og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);

        marker.remark(&[obj_ptr], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());

        marker.concurrent_sweep(&mut og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    }

    #[test]
    fn sweep_all_unreachable() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + 1 * SLOT_SIZE;

        // Allocate 3 objects, none rooted
        for i in 0..3 {
            let p = og.alloc(size, 8).unwrap();
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(i + 1);
                h.kind = ObjectKind::Object;
                h.num_slots = 1;
                h.gc_flags = 0x01;
            }
        }

        let used_before = og.used();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let (_marked, swept) = marker.full_cycle(&[], &mut og);

        assert_eq!(swept, 3);
        assert!(og.used() < used_before);
    }
}
