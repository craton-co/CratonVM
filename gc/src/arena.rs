//! Linear arena allocator for the semi-space copying GC.
//!
//! Each arena is a contiguous block of memory (`Vec<u8>`) with a cursor
//! that advances on each allocation. Objects cannot be individually freed;
//! instead, the entire arena is reset when GC swaps the semi-spaces.

/// A linear (bump-pointer) arena allocator.
///
/// Allocates from a pre-sized `Vec<u8>` buffer. The cursor advances
/// monotonically. Individual deallocations are not supported — only
/// full-arena reset.
pub struct Arena {
    /// Backing storage. Pre-allocated to `capacity` bytes.
    data: Vec<u8>,
    /// Next free byte offset within `data`.
    cursor: usize,
}

impl Arena {
    /// Create a new arena with the given capacity in bytes.
    pub fn new(capacity: usize) -> Self {
        // We need the Vec to have length == capacity so we can
        // hand out pointers into it. We zero-initialize for safety.
        let data = vec![0u8; capacity];
        Self { data, cursor: 0 }
    }

    /// Bump-allocate `size` bytes with the given alignment.
    ///
    /// Returns a pointer to the allocated region, or `None` if there
    /// isn't enough space.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        // Align the cursor up (checked to prevent overflow near usize::MAX)
        let aligned = self.cursor.checked_add(align - 1).map(|v| v & !(align - 1));
        let end = aligned.and_then(|a| a.checked_add(size));
        let aligned = aligned?;
        let end = end?;
        if end > self.data.len() {
            return None;
        }
        // SAFETY: `aligned` is within `[0, self.data.len())` because `end > aligned`
        // was checked above and `end <= self.data.len()`.
        let ptr = unsafe { self.data.as_mut_ptr().add(aligned) };
        self.cursor = end;
        Some(ptr)
    }

    /// Reset the arena, logically freeing all allocations.
    /// The backing memory is zeroed for safety.
    ///
    /// # When is it safe to use [`Self::reset_no_zero`] instead?
    ///
    /// The unsafe `reset_no_zero` variant is only sound when **both** of the
    /// following hold:
    ///
    /// 1. The allocator path zero-initialises every byte before handing the
    ///    pointer to the caller (e.g. `try_alloc_young` calls
    ///    `ptr::write_bytes(ptr, 0, size)`), OR the immediate caller fully
    ///    overwrites the region (e.g. Cheney `copy_nonoverlapping` writing
    ///    `total_size` bytes into the to-space slot). This guarantees no
    ///    legitimate read ever observes a stale byte.
    /// 2. No external code can read bytes past the cursor between the reset
    ///    and the next allocation. In CratonVM this is **not** true for
    ///    young-gen arenas: `GenerationalHeap::is_object_address`
    ///    (gen_heap.rs:465) performs conservative pointer validation that
    ///    accepts any 8-byte-aligned address within `[base, base+capacity)`
    ///    of either young space and then dereferences it as an
    ///    `ObjectHeader`. The VM's conservative root scanner
    ///    (vm/src/vm/vm_exec.rs:679) feeds ambiguous JVM long values through
    ///    this check, so leftover bytes in a reset young arena can be
    ///    misidentified as live objects and forwarded as garbage.
    ///
    /// Therefore: do **not** swap `reset` for `reset_no_zero` on the young
    /// from-space hot path until `is_object_address` is bounded by the live
    /// cursor (or the conservative root scanner is replaced with a precise
    /// stack map). The audit-flagged "perf bug" is a real cost, but the
    /// correctness hazard outweighs it.
    pub fn reset(&mut self) {
        // Zero out used region for safety (prevents stale data reads)
        self.data[..self.cursor].fill(0);
        self.cursor = 0;
    }

    /// Reset the arena without zeroing memory.
    ///
    /// # Safety
    /// Callers must ensure all subsequent allocations are fully
    /// initialized before any reads, AND that no external scanner can
    /// observe bytes past the (now-zero) cursor before they are
    /// re-allocated. See the doc comment on [`Self::reset`] for the
    /// full safety contract.
    ///
    /// Currently unused: the young-gen reset path in `gen_heap.rs` cannot
    /// satisfy condition (2) because conservative root scanning may
    /// dereference any 8-byte-aligned address in an arena's capacity
    /// range. Retained for future use when the GC switches to precise
    /// stack maps or when `is_object_address` is tightened to honour the
    /// cursor bound.
    #[allow(dead_code)]
    pub unsafe fn reset_no_zero(&mut self) {
        self.cursor = 0;
    }

    /// Returns true if the given pointer falls within this arena's storage.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr();
        let end = unsafe { base.add(self.data.len()) };
        ptr >= base && ptr < end
    }

    /// The number of bytes currently allocated (cursor position).
    pub fn used(&self) -> usize {
        self.cursor
    }

    /// The total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Get the base pointer of the arena's backing storage.
    pub fn base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable base pointer of the arena's backing storage.
    pub fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Grow the arena to at least `new_capacity` bytes, preserving existing data.
    ///
    /// If `new_capacity` <= current capacity, this is a no-op.
    ///
    /// # Panics
    ///
    /// `Vec::resize` may reallocate the backing buffer, which **invalidates
    /// every raw pointer** previously handed out by [`Self::alloc`]. Nothing
    /// in the type system enforces that callers fix up those pointers, so
    /// growing a non-empty arena is almost always a silent heap-corruption
    /// bug. To make the "safe by accident" usage explicit, this method
    /// **panics** if the arena has any live allocations (`cursor != 0`).
    ///
    /// Only an empty arena (cursor at 0, e.g. a freshly-reset to-space) may
    /// be grown. If a future caller genuinely needs to grow a populated
    /// arena it must first relocate every object and reset the cursor.
    ///
    /// Returns the old base pointer so callers can compute relocation offsets.
    pub fn grow(&mut self, new_capacity: usize) -> *const u8 {
        if new_capacity <= self.data.len() {
            return self.data.as_ptr();
        }
        assert_eq!(
            self.cursor, 0,
            "Arena::grow called on a non-empty arena ({} bytes live): \
             Vec::resize may reallocate and invalidate every pointer into \
             the arena. Only an empty (reset) arena may be grown.",
            self.cursor,
        );
        let old_base = self.data.as_ptr();
        self.data.resize(new_capacity, 0);
        old_base
    }

    /// The remaining free bytes in this arena.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("used", &self.cursor)
            .field("capacity", &self.data.len())
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
    fn arena_basic_alloc() {
        let mut arena = Arena::new(1024);
        assert_eq!(arena.used(), 0);
        assert_eq!(arena.capacity(), 1024);

        let ptr = arena.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(arena.used(), 64);
    }

    #[test]
    fn arena_multiple_allocs() {
        let mut arena = Arena::new(256);

        let p1 = arena.alloc(32, 8).unwrap();
        let p2 = arena.alloc(32, 8).unwrap();
        assert_ne!(p1, p2);
        assert_eq!(arena.used(), 64);
    }

    #[test]
    fn arena_alignment() {
        let mut arena = Arena::new(256);

        // Allocate 3 bytes with alignment 1
        arena.alloc(3, 1).unwrap();
        assert_eq!(arena.used(), 3);

        // Next allocation with alignment 8 should align up to offset 8
        let p2 = arena.alloc(8, 8).unwrap();
        let offset = unsafe { p2.offset_from(arena.base_ptr_mut()) } as usize;
        assert_eq!(offset, 8); // aligned to 8
        assert_eq!(arena.used(), 16);
    }

    #[test]
    fn arena_full_returns_none() {
        let mut arena = Arena::new(64);
        assert!(arena.alloc(64, 8).is_some());
        assert!(arena.alloc(1, 1).is_none());
    }

    #[test]
    fn arena_reset() {
        let mut arena = Arena::new(128);
        arena.alloc(64, 8).unwrap();
        assert_eq!(arena.used(), 64);

        arena.reset();
        assert_eq!(arena.used(), 0);

        // Can allocate again
        assert!(arena.alloc(128, 8).is_some());
    }

    #[test]
    fn arena_contains() {
        let mut arena = Arena::new(256);
        let ptr = arena.alloc(32, 8).unwrap();

        assert!(arena.contains(ptr));
        assert!(arena.contains(unsafe { ptr.add(16) }));
        // Just outside the arena
        assert!(!arena.contains(std::ptr::null()));
    }

    #[test]
    fn arena_zero_size_alloc() {
        let mut arena = Arena::new(64);
        let ptr = arena.alloc(0, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(arena.used(), 0);
    }

    #[test]
    fn arena_alignment_overflow_returns_none() {
        let mut arena = Arena::new(64);
        // Push cursor near usize::MAX so alignment arithmetic would overflow
        arena.cursor = usize::MAX - 2;
        // align=8 means cursor + 7 would overflow usize
        assert!(arena.alloc(1, 8).is_none());
    }
}
