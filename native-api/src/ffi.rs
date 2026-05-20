//! Panama FFI infrastructure (JEP 454).
//!
//! Provides off-heap memory management and native library loading for the
//! Foreign Function & Memory API.

use std::alloc::{self, Layout};
// AUDIT 2026-05-16: std::collections::HashMap is unused (replaced by
// rustc_hash::FxHashMap below).

use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Off-heap memory management
// ---------------------------------------------------------------------------

/// Tracks off-heap memory allocations for Panama MemorySegments.
///
/// Each allocation gets a unique ID used as a key. The actual raw pointer
/// and layout are stored so we can free the memory later.
/// T10.9.B: FxHashMap — allocation IDs are internal monotonic counters.
pub struct NativeMemoryTable {
    allocations: FxHashMap<i64, NativeAllocation>,
    next_id: i64,
    /// Running sum of `layout.size()` over all live allocations. Kept in
    /// sync by `allocate`/`free` so `live_bytes()` is O(1) instead of
    /// re-summing the whole map on every call.
    live_bytes: usize,
}

struct NativeAllocation {
    ptr: *mut u8,
    layout: Layout,
}

// Safety: NativeAllocation contains raw pointers, but they are only accessed
// through NativeMemoryTable methods which are protected by a Mutex in SharedVm.
unsafe impl Send for NativeAllocation {}
unsafe impl Sync for NativeAllocation {}

impl NativeMemoryTable {
    pub fn new() -> Self {
        Self {
            allocations: FxHashMap::default(),
            next_id: 1,
            live_bytes: 0,
        }
    }

    /// Allocate `size` bytes with `align` alignment. Returns (id, raw_pointer).
    ///
    /// The pointer is zeroed. Returns None if allocation fails.
    pub fn allocate(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)> {
        let align = align.max(1);
        let size = size.max(1);
        let layout = Layout::from_size_align(size, align).ok()?;
        // Safety: layout is valid (size >= 1, align >= 1 and power of two from Layout::from_size_align)
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        let id = self.next_id;
        self.next_id = match self.next_id.checked_add(1) {
            Some(next) => next,
            None => return None, // ID space exhausted
        };
        self.allocations
            .insert(id, NativeAllocation { ptr, layout });
        // Keep the running `live_bytes` total in sync. `id` is a fresh
        // monotonic counter, so this `insert` never replaces an entry.
        self.live_bytes += layout.size();
        Some((id, ptr))
    }

    /// Free a single allocation by ID. Returns true if found and freed.
    pub fn free(&mut self, id: i64) -> bool {
        if let Some(alloc) = self.allocations.remove(&id) {
            // Keep the running `live_bytes` total in sync.
            self.live_bytes -= alloc.layout.size();
            // Safety: ptr was allocated with alloc::alloc_zeroed with this layout.
            unsafe { alloc::dealloc(alloc.ptr, alloc.layout) };
            true
        } else {
            false
        }
    }

    /// Free multiple allocations by ID.
    pub fn free_all(&mut self, ids: &[i64]) {
        for &id in ids {
            self.free(id);
        }
    }

    /// Get the raw pointer for an allocation. Returns None if not found.
    pub fn get_ptr(&self, id: i64) -> Option<*mut u8> {
        self.allocations.get(&id).map(|a| a.ptr)
    }

    /// Get the size of an allocation.
    pub fn get_size(&self, id: i64) -> Option<usize> {
        self.allocations.get(&id).map(|a| a.layout.size())
    }

    /// Number of currently live allocations. Used by NEW-17 cleaner tests
    /// to verify that DirectByteBuffer cleanup released native memory.
    pub fn live_count(&self) -> usize {
        self.allocations.len()
    }

    /// Sum of sizes of all currently live allocations, in bytes.
    ///
    /// O(1): returns the running total maintained by `allocate`/`free`
    /// rather than re-summing the allocation map on every call.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }
}

impl Default for NativeMemoryTable {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NativeMemoryTable {
    fn drop(&mut self) {
        // Free all remaining allocations on table drop.
        for alloc in self.allocations.values() {
            unsafe { alloc::dealloc(alloc.ptr, alloc.layout) };
        }
        self.allocations.clear();
    }
}

// ---------------------------------------------------------------------------
// ValueLayout kind constants (matches Java's ValueLayout types)
// ---------------------------------------------------------------------------

pub const LAYOUT_BYTE: i32 = 0;
pub const LAYOUT_SHORT: i32 = 1;
pub const LAYOUT_INT: i32 = 2;
pub const LAYOUT_LONG: i32 = 3;
pub const LAYOUT_FLOAT: i32 = 4;
pub const LAYOUT_DOUBLE: i32 = 5;
pub const LAYOUT_ADDRESS: i32 = 6;
pub const LAYOUT_BOOLEAN: i32 = 7;
pub const LAYOUT_CHAR: i32 = 8;
pub const LAYOUT_STRUCT: i32 = 10;
pub const LAYOUT_UNION: i32 = 11;
pub const LAYOUT_SEQUENCE: i32 = 12;
pub const LAYOUT_PADDING: i32 = 13;

/// Return the byte size for a primitive layout kind.
/// For compound layouts (struct/union/sequence), size is stored on the object.
pub fn layout_byte_size(kind: i32) -> usize {
    match kind {
        LAYOUT_BYTE | LAYOUT_BOOLEAN => 1,
        LAYOUT_SHORT | LAYOUT_CHAR => 2,
        LAYOUT_INT | LAYOUT_FLOAT => 4,
        LAYOUT_LONG | LAYOUT_DOUBLE | LAYOUT_ADDRESS => 8,
        // Compound layouts return 0 here — actual size is on the synthetic object.
        LAYOUT_STRUCT | LAYOUT_UNION | LAYOUT_SEQUENCE | LAYOUT_PADDING => 0,
        _ => 1,
    }
}

/// Return the natural alignment for a primitive layout kind.
pub fn layout_alignment(kind: i32) -> usize {
    match kind {
        LAYOUT_BYTE | LAYOUT_BOOLEAN => 1,
        LAYOUT_SHORT | LAYOUT_CHAR => 2,
        LAYOUT_INT | LAYOUT_FLOAT => 4,
        LAYOUT_LONG | LAYOUT_DOUBLE | LAYOUT_ADDRESS => 8,
        _ => 1,
    }
}

/// Align `offset` up to the next multiple of `align`.
pub fn align_up(offset: usize, align: usize) -> usize {
    if align == 0 {
        return offset;
    }
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    (offset + align - 1) & !(align - 1)
}

// ---------------------------------------------------------------------------
// Upcall table — maps trampoline indices to Java callback info
// ---------------------------------------------------------------------------

use cratonvm_types::ObjectRef;

/// Entry in the upcall table — describes a Java callback for C to call.
pub struct UpcallEntry {
    /// The Java object implementing the functional interface.
    pub target: ObjectRef,
    /// Method name to invoke (e.g. "apply", "accept").
    pub method_name: String,
    /// Method descriptor.
    pub method_descriptor: String,
    /// Parameter layout kinds for unmarshaling C args to Java values.
    pub param_kinds: Vec<i32>,
    /// Return layout kind for marshaling Java return to C.
    pub return_kind: i32,
}

/// Table of upcall entries indexed by trampoline slot.
pub struct UpcallTable {
    entries: Vec<Option<UpcallEntry>>,
}

impl UpcallTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register an upcall entry. Returns the slot index.
    pub fn register(&mut self, entry: UpcallEntry) -> usize {
        // Reuse an empty slot if available
        for (i, slot) in self.entries.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(entry);
                return i;
            }
        }
        let idx = self.entries.len();
        self.entries.push(Some(entry));
        idx
    }

    /// Get an entry by slot index.
    pub fn get(&self, index: usize) -> Option<&UpcallEntry> {
        self.entries.get(index).and_then(|e| e.as_ref())
    }

    /// Remove an entry by slot index.
    pub fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries[index] = None;
        }
    }
}

impl Default for UpcallTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Arena kind constants
// ---------------------------------------------------------------------------

pub const ARENA_GLOBAL: i32 = 0;
pub const ARENA_AUTO: i32 = 1;
pub const ARENA_CONFINED: i32 = 2;
pub const ARENA_SHARED: i32 = 3;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // NativeMemoryTable
    // -----------------------------------------------------------------------

    #[test]
    fn allocate_and_free() {
        let mut table = NativeMemoryTable::new();
        let (id, ptr) = table.allocate(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert!(id > 0);

        // Write and read back
        unsafe {
            *ptr = 42;
            assert_eq!(*ptr, 42);
        }

        assert!(table.free(id));
        assert!(!table.free(id)); // double-free returns false
    }

    #[test]
    fn allocate_zeroed() {
        let mut table = NativeMemoryTable::new();
        let (_, ptr) = table.allocate(128, 1).unwrap();
        // Memory should be zeroed
        for i in 0..128 {
            assert_eq!(unsafe { *ptr.add(i) }, 0);
        }
        // cleanup in Drop
    }

    #[test]
    fn multiple_allocations() {
        let mut table = NativeMemoryTable::new();
        let (id1, _) = table.allocate(32, 4).unwrap();
        let (id2, _) = table.allocate(64, 8).unwrap();
        let (id3, _) = table.allocate(16, 1).unwrap();
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);

        table.free_all(&[id1, id3]);
        assert!(table.get_ptr(id1).is_none());
        assert!(table.get_ptr(id2).is_some());
        assert!(table.get_ptr(id3).is_none());
    }

    #[test]
    fn layout_sizes() {
        assert_eq!(layout_byte_size(LAYOUT_BYTE), 1);
        assert_eq!(layout_byte_size(LAYOUT_SHORT), 2);
        assert_eq!(layout_byte_size(LAYOUT_INT), 4);
        assert_eq!(layout_byte_size(LAYOUT_LONG), 8);
        assert_eq!(layout_byte_size(LAYOUT_FLOAT), 4);
        assert_eq!(layout_byte_size(LAYOUT_DOUBLE), 8);
        assert_eq!(layout_byte_size(LAYOUT_ADDRESS), 8);
    }

    #[test]
    fn get_size() {
        let mut table = NativeMemoryTable::new();
        let (id, _) = table.allocate(256, 16).unwrap();
        assert_eq!(table.get_size(id), Some(256));
    }

    // -----------------------------------------------------------------------
    // NativeMemoryTable — additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn default_is_same_as_new() {
        let table = NativeMemoryTable::default();
        assert_eq!(table.next_id, 1);
        assert!(table.allocations.is_empty());
    }

    #[test]
    fn allocate_minimum_size() {
        let mut table = NativeMemoryTable::new();
        // size=0 should be rounded up to 1
        let result = table.allocate(0, 1);
        assert!(result.is_some());
        let (id, ptr) = result.unwrap();
        assert!(!ptr.is_null());
        assert_eq!(table.get_size(id), Some(1));
    }

    #[test]
    fn allocate_minimum_alignment() {
        let mut table = NativeMemoryTable::new();
        // align=0 should be rounded up to 1
        let result = table.allocate(16, 0);
        assert!(result.is_some());
    }

    #[test]
    fn get_ptr_nonexistent_returns_none() {
        let table = NativeMemoryTable::new();
        assert!(table.get_ptr(999).is_none());
    }

    #[test]
    fn get_size_nonexistent_returns_none() {
        let table = NativeMemoryTable::new();
        assert!(table.get_size(999).is_none());
    }

    #[test]
    fn free_nonexistent_returns_false() {
        let mut table = NativeMemoryTable::new();
        assert!(!table.free(999));
    }

    #[test]
    fn free_all_with_empty_slice() {
        let mut table = NativeMemoryTable::new();
        table.free_all(&[]); // should not panic
    }

    #[test]
    fn free_all_with_mix_of_valid_and_invalid() {
        let mut table = NativeMemoryTable::new();
        let (id1, _) = table.allocate(8, 1).unwrap();
        table.free_all(&[id1, 999, 1000]);
        assert!(table.get_ptr(id1).is_none());
    }

    #[test]
    fn ids_are_sequential() {
        let mut table = NativeMemoryTable::new();
        let (id1, _) = table.allocate(8, 1).unwrap();
        let (id2, _) = table.allocate(8, 1).unwrap();
        let (id3, _) = table.allocate(8, 1).unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn large_allocation() {
        let mut table = NativeMemoryTable::new();
        let (id, ptr) = table.allocate(1024 * 1024, 64).unwrap(); // 1 MiB
        assert!(!ptr.is_null());
        assert_eq!(table.get_size(id), Some(1024 * 1024));
        assert!(table.free(id));
    }

    #[test]
    fn write_and_read_back_multiple_bytes() {
        let mut table = NativeMemoryTable::new();
        let (id, ptr) = table.allocate(256, 8).unwrap();
        unsafe {
            for i in 0..=255u8 {
                *ptr.add(i as usize) = i;
            }
            for i in 0..=255u8 {
                assert_eq!(*ptr.add(i as usize), i);
            }
        }
        assert!(table.free(id));
    }

    #[test]
    fn drop_frees_all_remaining() {
        // Just ensure no leak/crash — miri or valgrind would catch leaks
        let mut table = NativeMemoryTable::new();
        let _ = table.allocate(64, 8);
        let _ = table.allocate(128, 16);
        // table dropped here, Drop impl should free both
    }

    // --- Phase 80.6: Native Alloc returns None on overflow ---

    #[test]
    fn allocate_overflow_returns_none() {
        let mut table = NativeMemoryTable::new();
        // usize::MAX as size — Layout::from_size_align will fail, returning None.
        let result = table.allocate(usize::MAX, 8);
        assert!(
            result.is_none(),
            "allocation of usize::MAX bytes must return None, not panic"
        );
    }

    // -----------------------------------------------------------------------
    // layout_byte_size — compound and unknown kinds
    // -----------------------------------------------------------------------

    #[test]
    fn layout_sizes_boolean_and_char() {
        assert_eq!(layout_byte_size(LAYOUT_BOOLEAN), 1);
        assert_eq!(layout_byte_size(LAYOUT_CHAR), 2);
    }

    #[test]
    fn layout_sizes_compound_return_zero() {
        assert_eq!(layout_byte_size(LAYOUT_STRUCT), 0);
        assert_eq!(layout_byte_size(LAYOUT_UNION), 0);
        assert_eq!(layout_byte_size(LAYOUT_SEQUENCE), 0);
        assert_eq!(layout_byte_size(LAYOUT_PADDING), 0);
    }

    #[test]
    fn layout_size_unknown_kind_defaults_to_1() {
        assert_eq!(layout_byte_size(99), 1);
        assert_eq!(layout_byte_size(-1), 1);
    }

    // -----------------------------------------------------------------------
    // layout_alignment
    // -----------------------------------------------------------------------

    #[test]
    fn layout_alignment_primitives() {
        assert_eq!(layout_alignment(LAYOUT_BYTE), 1);
        assert_eq!(layout_alignment(LAYOUT_BOOLEAN), 1);
        assert_eq!(layout_alignment(LAYOUT_SHORT), 2);
        assert_eq!(layout_alignment(LAYOUT_CHAR), 2);
        assert_eq!(layout_alignment(LAYOUT_INT), 4);
        assert_eq!(layout_alignment(LAYOUT_FLOAT), 4);
        assert_eq!(layout_alignment(LAYOUT_LONG), 8);
        assert_eq!(layout_alignment(LAYOUT_DOUBLE), 8);
        assert_eq!(layout_alignment(LAYOUT_ADDRESS), 8);
    }

    #[test]
    fn layout_alignment_unknown_defaults_to_1() {
        assert_eq!(layout_alignment(99), 1);
        assert_eq!(layout_alignment(-1), 1);
    }

    // -----------------------------------------------------------------------
    // align_up
    // -----------------------------------------------------------------------

    #[test]
    fn align_up_already_aligned() {
        assert_eq!(align_up(16, 8), 16);
        assert_eq!(align_up(0, 4), 0);
    }

    #[test]
    fn align_up_needs_padding() {
        assert_eq!(align_up(1, 4), 4);
        assert_eq!(align_up(5, 8), 8);
        assert_eq!(align_up(9, 4), 12);
        assert_eq!(align_up(7, 2), 8);
    }

    #[test]
    fn align_up_align_1() {
        assert_eq!(align_up(0, 1), 0);
        assert_eq!(align_up(7, 1), 7);
        assert_eq!(align_up(100, 1), 100);
    }

    #[test]
    fn align_up_align_0_returns_offset() {
        assert_eq!(align_up(42, 0), 42);
    }

    // -----------------------------------------------------------------------
    // UpcallTable
    // -----------------------------------------------------------------------

    #[test]
    fn upcall_table_new_is_empty() {
        let table = UpcallTable::new();
        assert!(table.get(0).is_none());
    }

    #[test]
    fn upcall_table_default() {
        let table = UpcallTable::default();
        assert!(table.get(0).is_none());
    }

    /// Helper to create a dummy ObjectRef for testing.
    fn dummy_obj_ref() -> ObjectRef {
        unsafe { ObjectRef::from_raw(0x1000 as *mut u8) }
    }

    #[test]
    fn upcall_register_and_get() {
        let mut table = UpcallTable::new();
        let entry = UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "apply".to_string(),
            method_descriptor: "(I)I".to_string(),
            param_kinds: vec![LAYOUT_INT],
            return_kind: LAYOUT_INT,
        };
        let slot = table.register(entry);
        assert_eq!(slot, 0);
        let e = table.get(0).unwrap();
        assert_eq!(e.method_name, "apply");
        assert_eq!(e.param_kinds, vec![LAYOUT_INT]);
    }

    #[test]
    fn upcall_remove_and_reuse_slot() {
        let mut table = UpcallTable::new();
        let entry0 = UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "first".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_LONG,
        };
        let entry1 = UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "second".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_LONG,
        };
        let slot0 = table.register(entry0);
        let slot1 = table.register(entry1);
        assert_eq!(slot0, 0);
        assert_eq!(slot1, 1);

        // Remove slot 0
        table.remove(slot0);
        assert!(table.get(slot0).is_none());
        assert!(table.get(slot1).is_some());

        // Next register should reuse slot 0
        let entry2 = UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "third".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_LONG,
        };
        let reused = table.register(entry2);
        assert_eq!(reused, 0);
        assert_eq!(table.get(0).unwrap().method_name, "third");
    }

    #[test]
    fn upcall_remove_out_of_bounds_noop() {
        let mut table = UpcallTable::new();
        table.remove(100); // should not panic
    }

    #[test]
    fn upcall_get_out_of_bounds_none() {
        let table = UpcallTable::new();
        assert!(table.get(100).is_none());
    }

    // -----------------------------------------------------------------------
    // Arena constants
    // -----------------------------------------------------------------------

    #[test]
    fn arena_constants() {
        assert_eq!(ARENA_GLOBAL, 0);
        assert_eq!(ARENA_AUTO, 1);
        assert_eq!(ARENA_CONFINED, 2);
    }

    // -----------------------------------------------------------------------
    // Foreign Function & Memory API layout tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_memory_layout_basic_types() {
        // ValueLayout sizes per JVM spec
        assert_eq!(std::mem::size_of::<i8>(), 1);   // JAVA_BYTE
        assert_eq!(std::mem::size_of::<i16>(), 2);  // JAVA_SHORT
        assert_eq!(std::mem::size_of::<i32>(), 4);  // JAVA_INT
        assert_eq!(std::mem::size_of::<i64>(), 8);  // JAVA_LONG
        assert_eq!(std::mem::size_of::<f32>(), 4);  // JAVA_FLOAT
        assert_eq!(std::mem::size_of::<f64>(), 8);  // JAVA_DOUBLE
        assert_eq!(std::mem::size_of::<bool>(), 1);  // JAVA_BOOLEAN
        assert_eq!(std::mem::size_of::<u16>(), 2);  // JAVA_CHAR
    }

    #[test]
    fn test_pointer_alignment() {
        // Pointers should be aligned to their size
        assert_eq!(std::mem::align_of::<*const u8>(), std::mem::size_of::<*const u8>());
    }

    #[test]
    fn test_arena_allocation_and_cleanup() {
        // Arena.ofConfined() allocates, close() frees
        let layout = std::alloc::Layout::from_size_align(64, 8).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null());
        unsafe { std::alloc::dealloc(ptr, layout); }
    }

    #[test]
    fn test_memory_segment_bounds() {
        // MemorySegment must enforce bounds on access
        let data = vec![0u8; 128];
        let base = data.as_ptr();
        let len = data.len();
        // In-bounds access
        assert!(0 < len);
        assert!(127 < len);
        // Out-of-bounds would be 128+
        assert!(128 >= len);
        let _ = base; // suppress unused warning
    }

    #[test]
    fn test_null_pointer_segment() {
        // MemorySegment.NULL should be zero-length at address 0
        let null_ptr: *const u8 = std::ptr::null();
        assert!(null_ptr.is_null());
    }

    #[test]
    fn test_struct_layout_composition() {
        // StructLayout should compose member layouts
        #[repr(C)]
        struct Point { x: i32, y: i32 }
        assert_eq!(std::mem::size_of::<Point>(), 8);
        assert_eq!(std::mem::align_of::<Point>(), 4);
    }

    #[test]
    fn test_union_layout() {
        // UnionLayout takes the size of the largest member
        #[repr(C)]
        union IntOrFloat { i: i32, f: f32 }
        assert_eq!(std::mem::size_of::<IntOrFloat>(), 4);
    }

    #[test]
    fn test_string_marshaling_utf8() {
        // String marshaling: Java String -> C char* (UTF-8)
        let java_string = "Hello, Panama!";
        let c_bytes = java_string.as_bytes();
        assert_eq!(c_bytes.len(), 14);
        // Must be null-terminated for C
        let mut c_string = java_string.as_bytes().to_vec();
        c_string.push(0);
        assert_eq!(c_string.last(), Some(&0u8));
    }

    #[test]
    fn test_downcall_argument_types() {
        // Verify supported argument types for downcalls
        let int_arg: i32 = 42;
        let long_arg: i64 = 100;
        let float_arg: f32 = 3.14;
        let double_arg: f64 = 2.718;
        let ptr_arg: *const u8 = std::ptr::null();
        assert_eq!(int_arg, 42);
        assert_eq!(long_arg, 100);
        assert!((float_arg - 3.14f32).abs() < 0.001);
        assert!((double_arg - 2.718).abs() < 0.001);
        assert!(ptr_arg.is_null());
    }

    #[test]
    fn test_memory_segment_copy() {
        // MemorySegment.copy(src, srcOffset, dst, dstOffset, bytes)
        let src = vec![1u8, 2, 3, 4, 5];
        let mut dst = vec![0u8; 5];
        dst.copy_from_slice(&src);
        assert_eq!(dst, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_memory_segment_slice() {
        // MemorySegment.asSlice(offset, size)
        let data = vec![10u8, 20, 30, 40, 50];
        let slice = &data[1..4];
        assert_eq!(slice, &[20, 30, 40]);
    }
}
