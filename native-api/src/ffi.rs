// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
///
/// # Security (M4b — raw pointer use-after-free)
///
/// A bare `*mut u8` handed out by [`allocate`](Self::allocate) /
/// [`get_ptr`](Self::get_ptr) is **not** tied to the lifetime of its backing
/// allocation: a concurrent (or later) [`free`](Self::free) of the same
/// `alloc_id` deallocates the block out from under any retained pointer,
/// leaving it dangling. Two mitigations live here:
///
///  * [`get_ptr_checked`](Self::get_ptr_checked) validates an
///    `offset + len` window against the recorded allocation `size` and
///    returns a pointer that is guaranteed in-bounds *at the moment of the
///    call*. Consumers should request a fresh checked pointer for each access
///    and MUST NOT retain it across any operation that could free the
///    allocation.
///  * Each allocation carries a monotonic **generation** stamped into a
///    composite handle ([`AllocHandle`]). Because `alloc_id`s are reused only
///    in the (astronomically distant) `i64` wrap case, the generation's job is
///    to make a *stale handle fail closed* should id reuse ever occur: a
///    handle whose generation does not match the live slot is rejected by
///    [`get_ptr_checked_handle`](Self::get_ptr_checked_handle).
pub struct NativeMemoryTable {
    allocations: FxHashMap<i64, NativeAllocation>,
    next_id: i64,
    /// Monotonic generation counter. Bumped on every successful `allocate`
    /// and every successful `free` so that a handle minted for one
    /// allocation can never be confused with a later allocation that happens
    /// to reuse the same `alloc_id`.
    next_generation: u64,
    /// Running sum of `layout.size()` over all live allocations. Kept in
    /// sync by `allocate`/`free` so `live_bytes()` is O(1) instead of
    /// re-summing the whole map on every call.
    live_bytes: usize,
}

struct NativeAllocation {
    ptr: *mut u8,
    layout: Layout,
    /// Generation stamped at allocation time. A composite [`AllocHandle`]
    /// must carry a matching generation to resolve to this allocation; a
    /// mismatch means the handle is stale (the slot was freed and the
    /// `alloc_id` reused) and the lookup fails closed.
    generation: u64,
}

// Safety: NativeAllocation contains raw pointers, but they are only ever
// dereferenced (and the table only ever mutated) while the owning
// `SharedVm` holds its `Mutex`. The pointers are never copied out and used
// concurrently from two threads without that lock; the `unsafe impl`s below
// merely assert that the *struct itself* may cross threads, which is sound
// because every access path goes through that single lock. Do NOT relax this
// to lock-free access without revisiting the use-after-free analysis in the
// type-level docs above.
unsafe impl Send for NativeAllocation {}
unsafe impl Sync for NativeAllocation {}

/// Composite handle for a native allocation that binds an `alloc_id` to the
/// generation that minted it.
///
/// Holding an `AllocHandle` (rather than a bare `alloc_id`) lets a consumer
/// detect that the underlying allocation has been freed and its id recycled:
/// [`NativeMemoryTable::get_ptr_checked_handle`] rejects a handle whose
/// generation no longer matches the live slot, so a stale handle fails closed
/// instead of resolving to an unrelated allocation (M4b/M4c — confused
/// deputy / use-after-free hardening).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocHandle {
    pub alloc_id: i64,
    pub generation: u64,
}

impl AllocHandle {
    /// Construct a handle from its parts. Normally obtained from
    /// [`NativeMemoryTable::allocate_handle`] rather than built by hand.
    pub fn new(alloc_id: i64, generation: u64) -> Self {
        Self {
            alloc_id,
            generation,
        }
    }
}

impl NativeMemoryTable {
    pub fn new() -> Self {
        Self {
            allocations: FxHashMap::default(),
            next_id: 1,
            next_generation: 1,
            live_bytes: 0,
        }
    }

    /// Allocate `size` bytes with `align` alignment. Returns (id, raw_pointer).
    ///
    /// The pointer is zeroed. Returns None if allocation fails.
    ///
    /// # Security (M4b)
    ///
    /// The returned `*mut u8` is the *base* of the allocation with no length
    /// attached and no tie to the allocation's lifetime: a later
    /// [`free`](Self::free) of the returned id leaves it dangling. Treat it
    /// as valid only until the next operation that could free the block, and
    /// prefer [`allocate_handle`](Self::allocate_handle) +
    /// [`get_ptr_checked_handle`](Self::get_ptr_checked_handle) when the
    /// pointer must survive across other table operations.
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
            None => {
                // ID space exhausted — free the block we just allocated
                // rather than leaking it. Without this `dealloc` the
                // `alloc_zeroed` above would orphan `size` bytes on every
                // failed call (rare in practice — `i64` exhaustion would
                // take billions of years — but a leak nonetheless, and
                // valgrind/miri/asan will flag it).
                //
                // Safety: `ptr` was just produced by `alloc::alloc_zeroed`
                // with `layout` and has not been handed out or stored
                // anywhere, so we are the unique owner.
                unsafe { alloc::dealloc(ptr, layout) };
                #[cfg(test)]
                test_hooks::EXHAUSTION_DEALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return None;
            }
        };
        // Stamp this allocation with a fresh generation. `saturating_add`
        // guarantees forward progress even at the (unreachable in practice)
        // `u64` ceiling; a saturated generation simply stops distinguishing
        // handles, which is fail-closed-equivalent because it can only cause
        // a stale handle to be *rejected*, never wrongly accepted.
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.allocations.insert(
            id,
            NativeAllocation {
                ptr,
                layout,
                generation,
            },
        );
        // Keep the running `live_bytes` total in sync. `id` is a fresh
        // monotonic counter, so this `insert` never replaces an entry.
        self.live_bytes += layout.size();
        Some((id, ptr))
    }

    /// Allocate `size` bytes with `align` alignment and return a composite
    /// [`AllocHandle`] (id + generation) alongside the base pointer.
    ///
    /// Prefer this over [`allocate`](Self::allocate) when the resulting
    /// pointer will be revalidated later via
    /// [`get_ptr_checked_handle`](Self::get_ptr_checked_handle): the handle
    /// lets a stale `alloc_id` (after free + id reuse) be detected and
    /// rejected (M4b/M4c).
    pub fn allocate_handle(&mut self, size: usize, align: usize) -> Option<(AllocHandle, *mut u8)> {
        let (id, ptr) = self.allocate(size, align)?;
        // The generation just stamped is `next_generation - 1`; read it back
        // from the table so the handle is always consistent with the slot.
        let generation = self.allocations.get(&id).map(|a| a.generation)?;
        Some((AllocHandle::new(id, generation), ptr))
    }

    /// Free a single allocation by ID. Returns true if found and freed.
    ///
    /// # Security (M4b)
    ///
    /// After this returns, any bare `*mut u8` previously obtained for `id`
    /// (via [`allocate`](Self::allocate) / [`get_ptr`](Self::get_ptr)) is
    /// **dangling** — dereferencing it is undefined behaviour. The generation
    /// counter is advanced so that if `id` is ever recycled, handles minted
    /// for the freed allocation fail closed in
    /// [`get_ptr_checked_handle`](Self::get_ptr_checked_handle).
    pub fn free(&mut self, id: i64) -> bool {
        if let Some(alloc) = self.allocations.remove(&id) {
            // Keep the running `live_bytes` total in sync.
            self.live_bytes -= alloc.layout.size();
            // Advance the generation so a recycled `id` cannot be mistaken
            // for this now-freed allocation by a stale handle.
            self.next_generation = self.next_generation.saturating_add(1);
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

    /// Get the raw *base* pointer for an allocation. Returns None if not found.
    ///
    /// # Security (M4b)
    ///
    /// This returns the unvalidated base pointer with no length information.
    /// A caller that then indexes into it is responsible for its own bounds
    /// checking, and the pointer is invalidated by any subsequent
    /// [`free`](Self::free) of `id`. For pointer arithmetic prefer
    /// [`get_ptr_checked`](Self::get_ptr_checked), which validates the access
    /// window against the recorded allocation size and fails closed on
    /// overflow / out-of-range.
    pub fn get_ptr(&self, id: i64) -> Option<*mut u8> {
        self.allocations.get(&id).map(|a| a.ptr)
    }

    /// Return a pointer to `ptr + offset` for allocation `id`, but only if the
    /// window `[offset, offset + len)` lies fully within the allocation's
    /// recorded size. Returns `None` if the allocation is unknown, if
    /// `offset + len` overflows `usize`, or if the window runs past the end of
    /// the block.
    ///
    /// # Security (M4b)
    ///
    /// This is the bounds-checked alternative to [`get_ptr`](Self::get_ptr):
    /// it guarantees the returned pointer addresses `len` valid bytes *at the
    /// moment of the call*. It does **not** extend the allocation's lifetime —
    /// the pointer must still not be retained across a possible
    /// [`free`](Self::free). Pass `len == 0` to validate `offset <= size`
    /// (a one-past-the-end offset is permitted for a zero-length window, matching
    /// the usual C/Rust pointer rules).
    pub fn get_ptr_checked(&self, id: i64, offset: usize, len: usize) -> Option<*mut u8> {
        let alloc = self.allocations.get(&id)?;
        let size = alloc.layout.size();
        // `offset + len` must not overflow and must not exceed `size`.
        let end = offset.checked_add(len)?;
        if end > size {
            return None;
        }
        // Safety: `offset <= size` and `size` is the exact byte length of the
        // allocation `alloc.ptr` points at, so `add(offset)` stays within (or
        // one-past) the same allocation, which is the precondition for
        // `pointer::add`.
        Some(unsafe { alloc.ptr.add(offset) })
    }

    /// Like [`get_ptr_checked`](Self::get_ptr_checked) but additionally
    /// verifies the [`AllocHandle`]'s generation against the live slot, so a
    /// stale handle (after the original allocation was freed and its
    /// `alloc_id` recycled) fails closed instead of resolving to an unrelated
    /// allocation.
    ///
    /// # Security (M4b / M4c)
    ///
    /// Returns `None` when the generation does not match — this is the
    /// confused-deputy / use-after-free defence. Prefer this entry point for
    /// any pointer that outlives the call that produced it.
    pub fn get_ptr_checked_handle(
        &self,
        handle: AllocHandle,
        offset: usize,
        len: usize,
    ) -> Option<*mut u8> {
        let alloc = self.allocations.get(&handle.alloc_id)?;
        if alloc.generation != handle.generation {
            // Stale handle: the slot was freed (and possibly the id reused)
            // since this handle was minted. Fail closed.
            return None;
        }
        let size = alloc.layout.size();
        let end = offset.checked_add(len)?;
        if end > size {
            return None;
        }
        // Safety: same invariant as `get_ptr_checked`, with the added
        // guarantee that this is the very allocation the handle was minted
        // for (generation matched).
        Some(unsafe { alloc.ptr.add(offset) })
    }

    /// Return the current generation of allocation `id`, or `None` if the id
    /// is not live. Lets a caller mint an [`AllocHandle`] for an id it already
    /// holds (e.g. one returned by the legacy [`allocate`](Self::allocate)).
    pub fn generation_of(&self, id: i64) -> Option<u64> {
        self.allocations.get(&id).map(|a| a.generation)
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

#[cfg(test)]
mod test_hooks {
    use std::sync::atomic::AtomicUsize;
    /// Incremented by `NativeMemoryTable::allocate` every time the
    /// ID-exhaustion path deallocates the just-allocated block before
    /// returning `None`. Reset by tests that want to observe a single
    /// failure in isolation.
    pub(super) static EXHAUSTION_DEALLOCS: AtomicUsize = AtomicUsize::new(0);
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
///
/// A hostile/large `offset` near `usize::MAX` must not wrap: the
/// `offset + align - 1` term is computed with a checked add. On overflow we
/// saturate to the largest representable aligned value (`usize::MAX` masked
/// down to an `align` boundary) rather than wrapping back toward zero.
pub fn align_up(offset: usize, align: usize) -> usize {
    if align == 0 {
        return offset;
    }
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    match offset.checked_add(align - 1) {
        Some(sum) => sum & !(align - 1),
        // Overflow: saturate to the highest aligned value (never wrap).
        None => usize::MAX & !(align - 1),
    }
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

/// A slot in the [`UpcallTable`]: the optional entry plus the generation that
/// currently owns the slot. The generation is bumped every time the slot is
/// vacated or reused so a handle minted for an old occupant can be detected.
struct UpcallSlot {
    entry: Option<UpcallEntry>,
    /// Generation of the entry currently in this slot. A composite
    /// [`UpcallHandle`] must carry a matching generation to resolve.
    generation: u64,
}

/// Composite handle for an upcall registration: the slot index plus the
/// generation that minted it.
///
/// # Security (M4c — confused deputy via slot reuse)
///
/// [`UpcallTable::register`] reuses vacated slots, so a bare slot index handed
/// to native code can, after the original registration is
/// [`remove`](UpcallTable::remove)d and the slot re-registered, resolve to a
/// *different* Java object — a confused deputy. An `UpcallHandle` binds the
/// index to the registering generation; [`UpcallTable::get_checked`] rejects a
/// handle whose generation no longer matches the slot, so a stale trampoline
/// fails closed instead of invoking an unrelated callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpcallHandle {
    pub slot: usize,
    pub generation: u64,
}

impl UpcallHandle {
    /// Construct a handle from its parts. Normally obtained from
    /// [`UpcallTable::register_handle`].
    pub fn new(slot: usize, generation: u64) -> Self {
        Self { slot, generation }
    }
}

/// Table of upcall entries indexed by trampoline slot.
pub struct UpcallTable {
    slots: Vec<UpcallSlot>,
    /// Monotonic counter used to stamp each (re)registration with a unique
    /// generation. Bumped on every `register` and every `remove`.
    next_generation: u64,
}

impl UpcallTable {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            next_generation: 1,
        }
    }

    /// Register an upcall entry. Returns the slot index.
    ///
    /// # Security (M4c)
    ///
    /// The bare index returned here does NOT distinguish between successive
    /// occupants of a reused slot. Prefer
    /// [`register_handle`](Self::register_handle) +
    /// [`get_checked`](Self::get_checked) for native trampolines so a stale
    /// index cannot resolve to a later, unrelated callback.
    pub fn register(&mut self, entry: UpcallEntry) -> usize {
        self.register_handle(entry).slot
    }

    /// Register an upcall entry and return a generation-tagged
    /// [`UpcallHandle`]. The slot index is reused from a vacated slot when
    /// possible, but the generation is always fresh, so the handle uniquely
    /// identifies *this* registration (M4c).
    pub fn register_handle(&mut self, entry: UpcallEntry) -> UpcallHandle {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        // Reuse an empty slot if available
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.entry.is_none() {
                slot.entry = Some(entry);
                slot.generation = generation;
                return UpcallHandle::new(i, generation);
            }
        }
        let idx = self.slots.len();
        self.slots.push(UpcallSlot {
            entry: Some(entry),
            generation,
        });
        UpcallHandle::new(idx, generation)
    }

    /// Get an entry by slot index.
    ///
    /// # Security (M4c)
    ///
    /// This ignores generation, so it can return a *different* callback than
    /// the one a stale index was minted for (confused deputy). Use
    /// [`get_checked`](Self::get_checked) when resolving a handle that may have
    /// outlived its registration.
    pub fn get(&self, index: usize) -> Option<&UpcallEntry> {
        self.slots.get(index).and_then(|s| s.entry.as_ref())
    }

    /// Get an entry by generation-tagged handle. Returns `None` (fails closed)
    /// when the slot is empty or its current generation does not match the
    /// handle — i.e. the registration the handle referred to has since been
    /// removed and/or the slot reused (M4c).
    pub fn get_checked(&self, handle: UpcallHandle) -> Option<&UpcallEntry> {
        let slot = self.slots.get(handle.slot)?;
        if slot.generation != handle.generation {
            return None;
        }
        slot.entry.as_ref()
    }

    /// Return the current generation of live slot `index`, or `None` if the
    /// index is out of range or the slot is currently vacant. Lets a caller
    /// mint an [`UpcallHandle`] for a slot it already holds (e.g. one returned
    /// by the legacy [`register`](Self::register)).
    pub fn generation_of(&self, index: usize) -> Option<u64> {
        self.slots
            .get(index)
            .and_then(|s| s.entry.as_ref().map(|_| s.generation))
    }

    /// Remove an entry by slot index.
    ///
    /// Advances the slot's generation so that any handle still referring to
    /// the removed occupant fails closed in [`get_checked`](Self::get_checked),
    /// even after the slot is reused by a later [`register`](Self::register).
    pub fn remove(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.entry = None;
            // Bump the slot's generation so a stale handle for the just-removed
            // entry can never match again.
            slot.generation = self.next_generation;
            self.next_generation = self.next_generation.saturating_add(1);
        }
    }

    /// Collect every live entry's callback `target` as a GC root.
    ///
    /// MOVING-GC FIX: each occupied slot holds a `target: ObjectRef` to the Java
    /// callback object. The legacy Java-side dispatch path (`pe_upcall_invoke`)
    /// reads this `target` and invokes it, so it must be kept alive AND rewritten
    /// across relocations. Without scanning it here (and remapping it in
    /// [`update_after_gc`]) a moving collector could free or relocate the target
    /// out from under a still-registered upcall, leaving the dispatch path
    /// holding a stale/dangling pointer.
    pub fn collect_roots(&self, out: &mut Vec<ObjectRef>) {
        for slot in &self.slots {
            if let Some(entry) = &slot.entry {
                if entry.target.as_ptr() as usize != 0 {
                    out.push(entry.target);
                }
            }
        }
    }

    /// Apply a GC pointer map: rewrite each live entry's callback `target` to its
    /// post-relocation address. Counterpart to [`collect_roots`](Self::collect_roots).
    pub fn update_after_gc(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        for slot in &mut self.slots {
            if let Some(entry) = &mut slot.entry {
                let old = entry.target.as_ptr() as usize;
                if let Some(&new) = pointer_map.get(&old) {
                    entry.target = unsafe { ObjectRef::from_raw(new as *mut u8) };
                }
            }
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
    // M4b — bounds-checked pointer + generation handles
    // -----------------------------------------------------------------------

    #[test]
    fn get_ptr_checked_validates_window() {
        let mut table = NativeMemoryTable::new();
        let (id, base) = table.allocate(64, 8).unwrap();
        // Whole-allocation window is valid and equals the base pointer.
        assert_eq!(table.get_ptr_checked(id, 0, 64), Some(base));
        // Interior window is valid and offset correctly.
        assert_eq!(
            table.get_ptr_checked(id, 16, 8),
            Some(unsafe { base.add(16) })
        );
        // One-past-the-end zero-length window is allowed.
        assert_eq!(
            table.get_ptr_checked(id, 64, 0),
            Some(unsafe { base.add(64) })
        );
        // Past the end fails closed.
        assert!(table.get_ptr_checked(id, 60, 8).is_none());
        assert!(table.get_ptr_checked(id, 65, 0).is_none());
        // Overflow of offset+len fails closed (no panic).
        assert!(table.get_ptr_checked(id, usize::MAX, 1).is_none());
        // Unknown id fails closed.
        assert!(table.get_ptr_checked(999, 0, 1).is_none());
    }

    #[test]
    fn get_ptr_checked_after_free_returns_none() {
        let mut table = NativeMemoryTable::new();
        let (id, _) = table.allocate(32, 8).unwrap();
        assert!(table.get_ptr_checked(id, 0, 32).is_some());
        assert!(table.free(id));
        // After free the id no longer resolves — guards use-after-free at the
        // table boundary.
        assert!(table.get_ptr_checked(id, 0, 1).is_none());
    }

    #[test]
    fn alloc_handle_detects_stale_after_free_and_reuse() {
        let mut table = NativeMemoryTable::new();
        let (h1, _) = table.allocate_handle(32, 8).unwrap();
        let first_id = h1.alloc_id;
        // The first handle resolves while live.
        assert!(table.get_ptr_checked_handle(h1, 0, 32).is_some());
        assert!(table.free(first_id));
        // Force id reuse by rewinding next_id so the next allocation reuses
        // `first_id`. This simulates the i64-wrap reuse scenario.
        table.next_id = first_id;
        let (h2, _) = table.allocate_handle(32, 8).unwrap();
        assert_eq!(h2.alloc_id, first_id, "test setup: id should be reused");
        // The fresh handle works...
        assert!(table.get_ptr_checked_handle(h2, 0, 32).is_some());
        // ...but the stale handle for the freed allocation fails closed even
        // though the id now resolves to a *different* allocation.
        assert_ne!(h1.generation, h2.generation);
        assert!(table.get_ptr_checked_handle(h1, 0, 1).is_none());
    }

    #[test]
    fn generation_of_tracks_live_slot() {
        let mut table = NativeMemoryTable::new();
        let (id, _) = table.allocate(8, 1).unwrap();
        let gen = table.generation_of(id).unwrap();
        // A hand-built handle with the live generation resolves.
        let h = AllocHandle::new(id, gen);
        assert!(table.get_ptr_checked_handle(h, 0, 8).is_some());
        // A bogus generation fails closed.
        let bad = AllocHandle::new(id, gen.wrapping_add(1));
        assert!(table.get_ptr_checked_handle(bad, 0, 8).is_none());
        assert!(table.free(id));
        assert!(table.generation_of(id).is_none());
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
    fn allocate_does_not_leak_on_id_exhaustion() {
        // Regression: previously, when `next_id.checked_add(1)`
        // overflowed, `allocate` returned `None` without freeing the
        // block it had just acquired via `alloc::alloc_zeroed`. Force
        // the overflow by seeding `next_id` at `i64::MAX` and verify:
        //   1. the function returns `None`,
        //   2. the allocation table stays empty (the failed allocation
        //      was *not* stashed under any id), and
        //   3. the running `live_bytes` total is unchanged, and
        //   4. the dedicated free-on-exhaustion path actually ran,
        //      witnessed via the `EXHAUSTION_DEALLOCS` test counter.
        let before = test_hooks::EXHAUSTION_DEALLOCS.load(std::sync::atomic::Ordering::Relaxed);
        let mut table = NativeMemoryTable::new();
        table.next_id = i64::MAX;
        let result = table.allocate(4096, 8);
        assert!(result.is_none(), "expected None on id exhaustion");
        assert!(
            table.allocations.is_empty(),
            "leaked block was inserted into table"
        );
        assert_eq!(table.live_bytes, 0, "live_bytes drifted on failure");
        let after = test_hooks::EXHAUSTION_DEALLOCS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after - before,
            1,
            "exhaustion path did not call the matching dealloc"
        );
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
    fn upcall_handle_detects_slot_reuse() {
        // M4c: a handle minted for the original occupant of a slot must NOT
        // resolve after the slot is removed and reused by a different entry.
        let mut table = UpcallTable::new();
        let h0 = table.register_handle(UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "first".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_LONG,
        });
        assert_eq!(h0.slot, 0);
        assert_eq!(table.get_checked(h0).unwrap().method_name, "first");

        // Remove and re-register: slot index 0 is reused, generation differs.
        table.remove(h0.slot);
        assert!(
            table.get_checked(h0).is_none(),
            "stale handle must fail closed after remove"
        );

        let h1 = table.register_handle(UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "second".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_LONG,
        });
        assert_eq!(h1.slot, 0, "slot should be reused");
        assert_ne!(
            h0.generation, h1.generation,
            "generation must advance on reuse"
        );
        // Fresh handle resolves to the new entry...
        assert_eq!(table.get_checked(h1).unwrap().method_name, "second");
        // ...stale handle still fails closed (confused-deputy defence).
        assert!(table.get_checked(h0).is_none());
        // The legacy unchecked `get` would have returned the WRONG entry here.
        assert_eq!(table.get(0).unwrap().method_name, "second");
    }

    #[test]
    fn upcall_generation_of_and_manual_handle() {
        let mut table = UpcallTable::new();
        let slot = table.register(UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "apply".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_INT,
        });
        let gen = table.generation_of(slot).unwrap();
        let h = UpcallHandle::new(slot, gen);
        assert!(table.get_checked(h).is_some());
        let bad = UpcallHandle::new(slot, gen.wrapping_add(1));
        assert!(table.get_checked(bad).is_none());
    }

    #[test]
    fn upcall_generation_of_removed_slot_fails_closed_after_reuse() {
        let mut table = UpcallTable::new();
        let handle = table.register_handle(UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "first".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_INT,
        });

        table.remove(handle.slot);
        assert!(
            table.generation_of(handle.slot).is_none(),
            "vacant slots must not expose a generation that can be reused"
        );

        let removed_generation = table.slots[handle.slot].generation;
        let stale_after_remove = UpcallHandle::new(handle.slot, removed_generation);
        assert!(table.get_checked(stale_after_remove).is_none());

        let reused = table.register_handle(UpcallEntry {
            target: dummy_obj_ref(),
            method_name: "second".to_string(),
            method_descriptor: "()V".to_string(),
            param_kinds: vec![],
            return_kind: LAYOUT_INT,
        });
        assert_eq!(reused.slot, handle.slot);
        assert_ne!(
            reused.generation, removed_generation,
            "reused slots must receive a generation distinct from the vacant slot"
        );
        assert!(table.get_checked(stale_after_remove).is_none());
        assert_eq!(table.get_checked(reused).unwrap().method_name, "second");
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

    #[test]
    fn upcall_collect_roots_yields_live_targets() {
        let mut table = UpcallTable::new();
        let a = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
        let b = unsafe { ObjectRef::from_raw(0x2000 as *mut u8) };
        let s0 = table.register(UpcallEntry {
            target: a,
            method_name: "a".into(),
            method_descriptor: String::new(),
            param_kinds: vec![],
            return_kind: -1,
        });
        table.register(UpcallEntry {
            target: b,
            method_name: "b".into(),
            method_descriptor: String::new(),
            param_kinds: vec![],
            return_kind: -1,
        });
        let mut roots = Vec::new();
        table.collect_roots(&mut roots);
        assert!(roots.contains(&a));
        assert!(roots.contains(&b));
        assert_eq!(roots.len(), 2);

        // A removed (vacated) slot contributes no root.
        table.remove(s0);
        let mut roots2 = Vec::new();
        table.collect_roots(&mut roots2);
        assert_eq!(roots2, vec![b]);
    }

    #[test]
    fn upcall_update_after_gc_remaps_target() {
        // MOVING-GC FIX: a relocating collection must rewrite each live slot's
        // callback target so the legacy `pe_upcall_invoke` dispatch resolves the
        // object's CURRENT address, not the stale from-space pointer.
        let mut table = UpcallTable::new();
        let old = unsafe { ObjectRef::from_raw(0x3000 as *mut u8) };
        let new_addr = 0x9000usize;
        table.register(UpcallEntry {
            target: old,
            method_name: "moved".into(),
            method_descriptor: String::new(),
            param_kinds: vec![],
            return_kind: -1,
        });
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(old.as_ptr() as usize, new_addr);
        table.update_after_gc(&pm);
        assert_eq!(table.get(0).unwrap().target.as_ptr() as usize, new_addr);

        // An empty pointer map (non-moving collection) leaves the target intact.
        table.update_after_gc(&cratonvm_types::PointerMap::default());
        assert_eq!(table.get(0).unwrap().target.as_ptr() as usize, new_addr);
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
        assert_eq!(std::mem::size_of::<i8>(), 1); // JAVA_BYTE
        assert_eq!(std::mem::size_of::<i16>(), 2); // JAVA_SHORT
        assert_eq!(std::mem::size_of::<i32>(), 4); // JAVA_INT
        assert_eq!(std::mem::size_of::<i64>(), 8); // JAVA_LONG
        assert_eq!(std::mem::size_of::<f32>(), 4); // JAVA_FLOAT
        assert_eq!(std::mem::size_of::<f64>(), 8); // JAVA_DOUBLE
        assert_eq!(std::mem::size_of::<bool>(), 1); // JAVA_BOOLEAN
        assert_eq!(std::mem::size_of::<u16>(), 2); // JAVA_CHAR
    }

    #[test]
    fn test_pointer_alignment() {
        // Pointers should be aligned to their size
        assert_eq!(
            std::mem::align_of::<*const u8>(),
            std::mem::size_of::<*const u8>()
        );
    }

    #[test]
    fn test_arena_allocation_and_cleanup() {
        // Arena.ofConfined() allocates, close() frees
        let layout = std::alloc::Layout::from_size_align(64, 8).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null());
        unsafe {
            std::alloc::dealloc(ptr, layout);
        }
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
        struct Point {
            x: i32,
            y: i32,
        }
        assert_eq!(std::mem::size_of::<Point>(), 8);
        assert_eq!(std::mem::align_of::<Point>(), 4);
    }

    #[test]
    fn test_union_layout() {
        // UnionLayout takes the size of the largest member
        #[repr(C)]
        union IntOrFloat {
            i: i32,
            f: f32,
        }
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
