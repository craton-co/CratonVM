// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
mod test_hooks;

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
mod tests;
