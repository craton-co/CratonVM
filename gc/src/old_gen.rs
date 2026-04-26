//! Old generation free-list allocator with mark-compact collection.
//!
//! The old generation uses a free-list allocator for allocation and a sliding
//! mark-compact collector for major GC. Objects are allocated from a best-fit
//! free list. During major GC, live objects are compacted toward the start of
//! the heap, eliminating fragmentation entirely.
//!
//! Allocation uses best-fit from a sorted free list. Adjacent free blocks
//! are coalesced on free to reduce fragmentation.

use std::collections::HashMap;

use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, GC_FLAG_MARKED, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use rustjvm_types::{ObjectRef, Value};

/// A contiguous free block in the old generation.
#[derive(Debug, Clone, Copy)]
struct FreeBlock {
    /// Byte offset from the start of the data buffer.
    offset: usize,
    /// Size in bytes.
    size: usize,
}

/// Non-moving free-list allocator for the old generation.
///
/// Objects are allocated via first-fit from a sorted free list. Freed blocks
/// are coalesced with adjacent free blocks to reduce fragmentation.
pub struct OldGen {
    /// Backing storage (pre-allocated, zero-initialized).
    data: Vec<u8>,
    /// Free list, sorted by offset (ascending).
    free_list: Vec<FreeBlock>,
    /// Total bytes currently allocated (excluding free space).
    used_bytes: usize,
}

impl OldGen {
    /// Create a new old generation with the given capacity.
    pub fn new(capacity: usize) -> Self {
        let data = vec![0u8; capacity];
        let free_list = vec![FreeBlock {
            offset: 0,
            size: capacity,
        }];
        Self {
            data,
            free_list,
            used_bytes: 0,
        }
    }

    /// Allocate `size` bytes with the given alignment from the free list.
    ///
    /// Uses best-fit: scans the entire free list and selects the smallest
    /// block that can satisfy the request (after alignment). This reduces
    /// fragmentation compared to first-fit by preserving larger blocks for
    /// bigger allocations. Returns `None` if no block is large enough.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        if size == 0 {
            return Some(self.data.as_mut_ptr());
        }

        let base = self.data.as_ptr() as usize;

        // Find the best-fit block: smallest block that can satisfy the request.
        let mut best_idx: Option<usize> = None;
        let mut best_waste: usize = usize::MAX;

        for i in 0..self.free_list.len() {
            let block = self.free_list[i];
            let block_addr = base + block.offset;
            let aligned_addr = (block_addr + align - 1) & !(align - 1);
            let padding = aligned_addr - block_addr;
            let total_needed = padding + size;

            if total_needed <= block.size {
                let waste = block.size - total_needed;
                if waste < best_waste {
                    best_waste = waste;
                    best_idx = Some(i);
                    // Perfect fit -- no need to keep searching.
                    if waste == 0 {
                        break;
                    }
                }
            }
        }

        let i = best_idx?;
        let block = self.free_list[i];
        let block_addr = base + block.offset;
        let aligned_addr = (block_addr + align - 1) & !(align - 1);
        let padding = aligned_addr - block_addr;
        let alloc_offset = block.offset + padding;

        if padding > 0 {
            // Keep a free block for the padding bytes before alignment
            self.free_list[i] = FreeBlock {
                offset: block.offset,
                size: padding,
            };
            // Remaining space after allocation
            let remaining = block.size - padding - size;
            if remaining > 0 {
                self.free_list.insert(
                    i + 1,
                    FreeBlock {
                        offset: alloc_offset + size,
                        size: remaining,
                    },
                );
            }
        } else {
            // No padding needed
            let remaining = block.size - size;
            if remaining > 0 {
                self.free_list[i] = FreeBlock {
                    offset: block.offset + size,
                    size: remaining,
                };
            } else {
                self.free_list.remove(i);
            }
        }

        self.used_bytes += size;
        // SAFETY: alloc_offset is within [0, self.data.len()) and size bytes fit
        // within the selected free block bounds.
        let ptr = unsafe { self.data.as_mut_ptr().add(alloc_offset) };
        // SAFETY: ptr points to alloc_offset within the data buffer with at
        // least size bytes available. Zero-initializing the allocated region.
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }
        Some(ptr)
    }

    /// Free a previously allocated block, returning it to the free list.
    ///
    /// Coalesces with adjacent free blocks to reduce fragmentation.
    ///
    /// # Safety
    /// `ptr` must point to a block previously allocated from this OldGen,
    /// and `size` must be the exact size of that allocation.
    pub unsafe fn free(&mut self, ptr: *mut u8, size: usize) {
        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr + size <= base + self.data.len());
        let offset = addr - base;

        // Zero the freed region for safety
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }

        self.used_bytes = self.used_bytes.saturating_sub(size);

        // Insert into sorted position
        let insert_pos = self.free_list.partition_point(|b| b.offset < offset);

        self.free_list
            .insert(insert_pos, FreeBlock { offset, size });

        // Coalesce with the next block
        if insert_pos + 1 < self.free_list.len() {
            let curr = self.free_list[insert_pos];
            let next = self.free_list[insert_pos + 1];
            if curr.offset + curr.size == next.offset {
                self.free_list[insert_pos].size += next.size;
                self.free_list.remove(insert_pos + 1);
            }
        }

        // Coalesce with the previous block
        if insert_pos > 0 {
            let prev = self.free_list[insert_pos - 1];
            let curr = self.free_list[insert_pos];
            if prev.offset + prev.size == curr.offset {
                self.free_list[insert_pos - 1].size += curr.size;
                self.free_list.remove(insert_pos);
            }
        }
    }

    /// Returns true if the given pointer falls within this old generation's storage.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr();
        let end = unsafe { base.add(self.data.len()) };
        ptr >= base && ptr < end
    }

    /// Get the base pointer of the backing storage.
    pub fn base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable base pointer of the backing storage.
    pub fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Total bytes currently allocated.
    pub fn used(&self) -> usize {
        self.used_bytes
    }

    /// Total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Walk all allocated objects in the old generation.
    ///
    /// This iterates through allocated regions (gaps between free blocks)
    /// and yields each object's pointer and header. Used by the mark-sweep
    /// collector to iterate old-gen objects.
    ///
    /// Returns a Vec of (object pointer, total object size) pairs.
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut objects = Vec::new();
        let base = self.data.as_ptr() as usize;

        // Build sorted list of allocated regions from free list gaps
        let mut cursor: usize = 0;
        for block in &self.free_list {
            // Allocated region from cursor to block.offset
            if block.offset > cursor {
                self.scan_region(base, cursor, block.offset, &mut objects);
            }
            cursor = block.offset + block.size;
        }
        // Region after last free block
        if cursor < self.data.len() {
            self.scan_region(base, cursor, self.data.len(), &mut objects);
        }

        objects
    }

    /// Scan a contiguous allocated region for objects.
    fn scan_region(
        &self,
        base: usize,
        start_offset: usize,
        end_offset: usize,
        objects: &mut Vec<(*mut u8, usize)>,
    ) {
        let mut offset = start_offset;
        while offset < end_offset {
            let ptr = (base + offset) as *mut u8;
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            let total_size = if header.kind == ObjectKind::Array {
                HEADER_SIZE + array_data_size(header.array_length as usize, header.element_type)
                    .expect("array_data_size overflow in old_gen scan")
            } else {
                HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
            };
            // Sanity check: if total_size is 0 or too large, stop scanning
            if total_size < HEADER_SIZE || offset + total_size > end_offset {
                break;
            }
            objects.push((ptr, total_size));
            offset += total_size;
        }
    }

    // -----------------------------------------------------------------------
    // Mark-Compact Collection (Session 26)
    // -----------------------------------------------------------------------

    /// Compact all marked (live) objects toward the start of the heap using
    /// sliding compaction. This eliminates all fragmentation — after compaction,
    /// the free list is a single contiguous block at the end.
    ///
    /// **Algorithm (4 phases):**
    /// 1. **Compute forwarding addresses** — walk objects in address order,
    ///    assign each live object a destination at the next free position.
    /// 2. **Update internal references** — for each live object, rewrite
    ///    reference slots to use forwarding addresses.
    /// 3. **Slide objects** — copy each object to its forwarding address
    ///    (low-to-high order guarantees no data corruption since dest ≤ src).
    /// 4. **Rebuild free list** — single free block from compacted end to
    ///    capacity.
    ///
    /// **Precondition:** live objects have `GC_FLAG_MARKED` set in their headers.
    /// **Postcondition:** `GC_FLAG_MARKED` and `forwarding_ptr` are cleared on
    /// all surviving objects. Dead objects are reclaimed.
    ///
    /// Returns a pointer map (old_addr → new_addr) for objects that moved.
    /// Objects that stay in place are NOT included in the map.
    pub fn compact(&mut self) -> HashMap<usize, usize> {
        let base = self.data.as_mut_ptr();
        let objects = self.walk_objects();

        // Phase 1: Compute forwarding addresses for live objects.
        // `write_cursor` tracks the next available byte offset (8-byte aligned).
        let mut write_cursor: usize = 0;
        let mut pointer_map: HashMap<usize, usize> = HashMap::new();
        // (src_ptr, total_size, dest_ptr) for each live object
        let mut live_objects: Vec<(*mut u8, usize, *mut u8)> = Vec::new();

        for &(obj_ptr, total_size) in &objects {
            let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
            if header.gc_flags & GC_FLAG_MARKED == 0 {
                continue; // Dead object — skip
            }
            let aligned = (write_cursor + 7) & !7;
            let dest = unsafe { base.add(aligned) };

            // Store forwarding address in header for Phase 2 reference updates
            header.forwarding_ptr = dest;

            if obj_ptr != dest {
                pointer_map.insert(obj_ptr as usize, dest as usize);
            }
            live_objects.push((obj_ptr, total_size, dest));
            write_cursor = aligned + total_size;
        }

        // Phase 2: Update references within live old-gen objects.
        // Each reference slot that points to a moved old-gen object is rewritten
        // to the object's forwarding address.
        for &(obj_ptr, _, _) in &live_objects {
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            Self::update_refs_in_object(obj_ptr, header, &self.data);
        }

        // Phase 3: Slide objects to their forwarding addresses.
        // Processing order is low-to-high by source address, and dest ≤ src
        // for sliding compaction, so `std::ptr::copy` (memmove) is safe even
        // for overlapping regions.
        for &(src, size, dest) in &live_objects {
            if src != dest {
                unsafe { std::ptr::copy(src, dest, size) };
            }
            // Clear GC metadata on the (possibly moved) object
            let final_header = unsafe { &mut *(dest as *mut ObjectHeader) };
            final_header.forwarding_ptr = std::ptr::null_mut();
            final_header.gc_flags &= !GC_FLAG_MARKED;
        }

        // Phase 4: Rebuild free list — one contiguous block at the end.
        let compacted_end = (write_cursor + 7) & !7;
        self.free_list.clear();
        if compacted_end < self.data.len() {
            // Zero the freed region for safety
            unsafe {
                std::ptr::write_bytes(base.add(compacted_end), 0, self.data.len() - compacted_end)
            };
            self.free_list.push(FreeBlock {
                offset: compacted_end,
                size: self.data.len() - compacted_end,
            });
        }
        self.used_bytes = compacted_end;

        pointer_map
    }

    /// Update reference slots within a single live old-gen object so they point
    /// to forwarding addresses of moved objects.
    ///
    /// Handles both regular object fields (16-byte `Value` slots) and reference
    /// arrays (8-byte compact pointers).
    fn update_refs_in_object(obj_ptr: *mut u8, header: &ObjectHeader, data: &[u8]) {
        let data_start = data.as_ptr() as usize;
        let data_end = data_start + data.len();

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    let slot = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize;
                        // Only update references within old gen bounds
                        if ref_ptr >= data_start && ref_ptr < data_end {
                            let ref_header =
                                unsafe { &*(ref_ptr as *const ObjectHeader) };
                            if !ref_header.forwarding_ptr.is_null() {
                                unsafe {
                                    std::ptr::write(
                                        slot as *mut u64,
                                        ref_header.forwarding_ptr as u64,
                                    )
                                };
                            }
                        }
                    }
                }
            }
        } else {
            for slot_idx in 0..header.num_slots as usize {
                let slot = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(slot as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr() as usize;
                    if ref_ptr >= data_start && ref_ptr < data_end {
                        let ref_header =
                            unsafe { &*(ref_ptr as *const ObjectHeader) };
                        if !ref_header.forwarding_ptr.is_null() {
                            let new_value = Value::Object(Some(unsafe {
                                ObjectRef::from_raw(ref_header.forwarding_ptr)
                            }));
                            unsafe { std::ptr::write(slot as *mut Value, new_value) };
                        }
                    }
                }
            }
        }
    }

    /// Returns the number of contiguous free blocks (fragmentation metric).
    pub fn free_block_count(&self) -> usize {
        self.free_list.len()
    }

    /// Returns the size of the largest contiguous free block.
    pub fn largest_free_block(&self) -> usize {
        self.free_list.iter().map(|b| b.size).max().unwrap_or(0)
    }
}

impl std::fmt::Debug for OldGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OldGen")
            .field("used", &self.used_bytes)
            .field("capacity", &self.data.len())
            .field("free_blocks", &self.free_list.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_old_gen() {
        let og = OldGen::new(4096);
        assert_eq!(og.used(), 0);
        assert_eq!(og.capacity(), 4096);
        assert_eq!(og.free_list.len(), 1);
    }

    #[test]
    fn alloc_basic() {
        let mut og = OldGen::new(4096);
        let ptr = og.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(og.used(), 64);
        assert!(og.contains(ptr));
    }

    #[test]
    fn alloc_multiple() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(128, 8).unwrap();
        assert_ne!(p1, p2);
        assert_eq!(og.used(), 192);
    }

    #[test]
    fn alloc_alignment() {
        let mut og = OldGen::new(4096);
        // Allocate 3 bytes, then 64 with align 8
        let _p1 = og.alloc(3, 1).unwrap();
        let p2 = og.alloc(64, 8).unwrap();
        // p2 should be 8-byte aligned
        assert_eq!(p2 as usize % 8, 0);
    }

    #[test]
    fn alloc_full_returns_none() {
        let mut og = OldGen::new(128);
        let _p1 = og.alloc(128, 1).unwrap();
        assert!(og.alloc(1, 1).is_none());
    }

    #[test]
    fn free_basic() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        assert_eq!(og.used(), 64);

        unsafe { og.free(p1, 64) };
        assert_eq!(og.used(), 0);
        // Should be able to allocate again
        let p2 = og.alloc(64, 8).unwrap();
        assert!(!p2.is_null());
    }

    #[test]
    fn free_coalesce_forward() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(64, 8).unwrap();
        let _p3 = og.alloc(64, 8).unwrap();

        // Free p2, then p1 — should coalesce with the gap that p2 created
        unsafe { og.free(p2, 64) };
        unsafe { og.free(p1, 64) };

        // Now we should have a contiguous 128-byte free block at the start
        assert_eq!(og.used(), 64); // only p3 remains
        let big = og.alloc(128, 8).unwrap();
        assert!(!big.is_null());
    }

    #[test]
    fn free_coalesce_backward() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(64, 8).unwrap();

        // Free p1, then p2 — should coalesce
        unsafe { og.free(p1, 64) };
        unsafe { og.free(p2, 64) };
        assert_eq!(og.used(), 0);

        // Free list should be back to a single block
        assert_eq!(og.free_list.len(), 1);
        assert_eq!(og.free_list[0].size, 4096);
    }

    #[test]
    fn contains() {
        let og = OldGen::new(256);
        let base = og.base_ptr();
        assert!(og.contains(base));
        assert!(og.contains(unsafe { base.add(128) }));
        assert!(!og.contains(std::ptr::null()));
        assert!(!og.contains(unsafe { base.add(256) })); // exclusive end
    }

    #[test]
    fn walk_objects_empty() {
        let og = OldGen::new(4096);
        let objects = og.walk_objects();
        assert!(objects.is_empty());
    }

    #[test]
    fn walk_objects_with_allocations() {
        let mut og = OldGen::new(4096);

        // Allocate two objects manually with proper headers
        let obj_size1 = HEADER_SIZE + 2 * SLOT_SIZE; // 2 fields
        let p1 = og.alloc(obj_size1, 8).unwrap();
        unsafe {
            let header = &mut *(p1 as *mut ObjectHeader);
            header.num_slots = 2;
        }

        let obj_size2 = HEADER_SIZE + SLOT_SIZE; // 1 field
        let p2 = og.alloc(obj_size2, 8).unwrap();
        unsafe {
            let header = &mut *(p2 as *mut ObjectHeader);
            header.num_slots = 1;
        }

        let objects = og.walk_objects();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].0, p1);
        assert_eq!(objects[0].1, obj_size1);
        assert_eq!(objects[1].0, p2);
        assert_eq!(objects[1].1, obj_size2);
    }
}
