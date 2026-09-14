// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Exact-value registry of heap addresses that were deliberately handed to
//! Java as `long` bits ("smuggled" jobject handles).
//!
//! ## Why this exists
//!
//! The conservative long-smuggle machinery in
//! [`crate::runtime::value_stack::ValueStack`] roots and — on a moving GC —
//! REWRITES any Long/Double-tagged slot whose bits look like a heap object
//! address. Rooting a false positive merely over-retains, but rewriting one
//! silently corrupts a genuine primitive `long` whose value happens to equal
//! a moved object's from-space address. Provenance of `ObjectRef`
//! construction (`object_ref_payload_is_known`) cannot discriminate that
//! case: the collision target is a real, once-constructed object by
//! definition of being in the GC pointer map (and the 64-byte-granule bitmap
//! that check now uses is looser still).
//!
//! The only signal that CAN discriminate is *mint provenance*: a genuine
//! smuggle's 64-bit value was, at some point, deliberately converted from an
//! object reference to Java-visible `long` bits at one of a handful of
//! CratonVM-controlled chokepoints (generic-JNI `J` returns,
//! `SetLongField`/`SetLongArrayRegion`, JVMTI `GetLocal*`). Those sites
//! register the exact value here; the value-stack rewrite arm then only
//! rewrites slots whose bits (or relocation target) are registered. A
//! colliding primitive is preserved — the residual hazard shrinks from
//! "primitive equals ANY moved object's address" to "primitive equals a
//! LIVE, Java-minted smuggle handle", a set that is typically empty.
//!
//! Entries are remapped through every GC pointer map and swept (dropped when
//! the referent is no longer a live object) once per collection from
//! [`crate::memory::gc::update_all_roots`], the same per-cycle contract as
//! `JniGlobalRefs::update_after_gc`.
//!
//! ## Why the registry is keyed on the heap
//!
//! Every value in here is a raw heap address, and a raw heap address is only
//! meaningful against the heap that produced it. One process can own several
//! heaps at once (`Vm::new` per embedded VM; the inline test modules build a
//! `VmHeap` per test), so a single flat set was wrong in both directions:
//!
//!   * **Mints were silently unregistered by an unrelated VM.**
//!     [`remap_and_sweep`] drops any entry that neither appears in the
//!     collector's pointer map nor still parses as a live object *in the
//!     heap being collected*. VM B's collection therefore swept VM A's
//!     handles out of the shared set — `is_object_address` on a foreign
//!     address always misses. When VM A later moved that object, the
//!     value-stack rewrite arm asked [`is_minted`], got `false`, and left
//!     the Java-visible `long` pointing into vacated from-space. That is
//!     precisely the use-after-move this module exists to prevent, and it
//!     needed no concurrency — a second VM created *after* the first was
//!     torn down is enough.
//!   * **Foreign mints were rewritten by the wrong pointer map.** A value
//!     minted against heap A that collides with an address in heap B's
//!     relocation map was rewritten to B's new address, corrupting A's
//!     handle.
//!
//! The key is the address of the owning [`VmHeap`] (see [`heap_id`]). A
//! `VmHeap` lives inside `Arc<SharedVm>` (`shared.mem.heap`), so its address
//! is stable for the VM's whole life and distinct from every other LIVE
//! heap's — which is exactly the scope over which the addresses it stores
//! are meaningful. `vm_identity` would name the same thing, but it is not
//! reachable from `ValueStack::update_object_refs` (which takes a `&VmHeap`
//! and nothing else) without threading a new parameter through every
//! interpreter and GC caller.
//!
//! Residual, documented: an allocator that reuses a dropped `VmHeap`'s
//! address for a new one inherits the dead heap's table. The new heap's
//! first [`remap_and_sweep`] clears it (no entry parses as live there), and
//! until then a stale entry can only matter for a value that ALSO appears in
//! the new heap's pointer map. That is strictly narrower than the previous
//! behaviour, where every heap shared one set unconditionally.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::memory::vm_heap::VmHeap;

/// Identifies the heap a smuggled address belongs to. See the module docs
/// for why this is the key.
pub type HeapId = usize;

/// The key for `heap`. The address of the `VmHeap` itself: stable for the
/// heap's lifetime and unique among live heaps.
#[inline]
pub fn heap_id(heap: &VmHeap) -> HeapId {
    heap as *const VmHeap as usize
}

/// Fast-path emptiness flag: workloads that never mint a smuggle (the vast
/// majority) pay one relaxed load per rewrite-candidate check and nothing on
/// GC cycles.
///
/// Deliberately process-wide even though the tables are per-heap: it is a
/// conservative "no heap in this process has any mint", so a `false` read is
/// a sound early-out for every heap, and the common case (no mints anywhere)
/// still costs one load.
static NONEMPTY: AtomicBool = AtomicBool::new(false);

/// Per-heap mint registries, keyed by [`heap_id`].
static SETS: Mutex<Option<HashMap<HeapId, HashSet<u64>>>> = Mutex::new(None);

fn with_sets<R>(f: impl FnOnce(&mut HashMap<HeapId, HashSet<u64>>) -> R) -> R {
    let mut g = SETS.lock().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// Record `bits` as a deliberately-minted Java-visible object handle
/// belonging to `heap`.
///
/// Call from mint chokepoints only, after the caller has verified the value
/// is (or was derived from) a real object address in `heap`. Cold: mints
/// happen on FFI/JVMTI slow paths.
pub fn record_minted_long(heap: &VmHeap, bits: u64) {
    if bits == 0 {
        return;
    }
    let key = heap_id(heap);
    with_sets(|tables| {
        tables.entry(key).or_default().insert(bits);
    });
    NONEMPTY.store(true, Ordering::Release);
}

/// Was `bits` ever minted as a Java-visible object handle **in `heap`** (and
/// still live)?
///
/// Called on the GC rewrite path only after a pointer-map hit, inside a
/// stop-the-world pause — the lock is uncontended there.
pub fn is_minted(heap: &VmHeap, bits: u64) -> bool {
    if !NONEMPTY.load(Ordering::Acquire) {
        return false;
    }
    let key = heap_id(heap);
    with_sets(|tables| tables.get(&key).is_some_and(|s| s.contains(&bits)))
}

/// Once per GC cycle: relocate `heap`'s registered handles through
/// `pointer_map` and drop entries whose referent is no longer a live heap
/// object (the sweep zeroes dead objects, so a stale entry's header no
/// longer parses).
///
/// Only `heap`'s own table is touched. Another VM's handles are neither
/// rewritten by this collector's map nor swept against this collector's
/// heap — the two bugs described in the module docs.
pub fn remap_and_sweep(pointer_map: &cratonvm_types::PointerMap, heap: &VmHeap) {
    if !NONEMPTY.load(Ordering::Acquire) {
        return;
    }
    let key = heap_id(heap);
    let any_left = with_sets(|tables| {
        let mut now_empty = false;
        if let Some(set) = tables.get_mut(&key) {
            if !set.is_empty() {
                let mut next: HashSet<u64> = HashSet::with_capacity(set.len());
                for &bits in set.iter() {
                    if let Some(&new_addr) = pointer_map.get(&(bits as usize)) {
                        next.insert(new_addr as u64);
                    } else if heap.is_object_address(bits as usize).is_some() {
                        next.insert(bits);
                    }
                    // else: referent dead — drop the entry so a future
                    // primitive collision with the reused address cannot
                    // resurrect it.
                }
                *set = next;
            }
            now_empty = set.is_empty();
        }
        if now_empty {
            tables.remove(&key);
        }
        // NONEMPTY may only be cleared when NO heap holds a mint; another
        // heap's table is still authoritative for its own rewrites.
        tables.values().any(|s| !s.is_empty())
    });
    if !any_left {
        NONEMPTY.store(false, Ordering::Release);
    }
}

/// Drop `heap`'s table outright. For a VM teardown path that wants to reclaim
/// the entries eagerly rather than wait for the next collection.
pub fn forget_heap(heap: &VmHeap) {
    let key = heap_id(heap);
    let any_left = with_sets(|tables| {
        tables.remove(&key);
        tables.values().any(|s| !s.is_empty())
    });
    if !any_left {
        NONEMPTY.store(false, Ordering::Release);
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    let mut g = SETS.lock().unwrap_or_else(|p| p.into_inner());
    *g = None;
    NONEMPTY.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::vm_heap::GcBackend;

    fn heap() -> VmHeap {
        VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024)
    }

    #[test]
    fn empty_registry_rejects() {
        // NOTE: `NONEMPTY` is process-global; this test only asserts the
        // contract for a value never minted anywhere in this heap.
        let h = heap();
        assert!(!is_minted(&h, 0xDEAD_BEE8));
    }

    #[test]
    fn record_then_hit() {
        let h = heap();
        record_minted_long(&h, 0x0000_7000_0000_1230);
        assert!(is_minted(&h, 0x0000_7000_0000_1230));
        assert!(!is_minted(&h, 0x0000_7000_0000_1238));
    }

    /// A mint belongs to the heap it was minted against. Before the tables
    /// were keyed, `is_minted` answered from one shared set, so any heap in
    /// the process vouched for any other heap's handle.
    #[test]
    fn mint_is_not_visible_to_another_heap() {
        let a = heap();
        let b = heap();
        let bits = 0x0000_7000_0000_2230u64;
        record_minted_long(&a, bits);
        assert!(is_minted(&a, bits));
        assert!(
            !is_minted(&b, bits),
            "a handle minted against heap A must not be vouched for by heap B"
        );
    }

    /// The regression this fix exists for: a collection in heap B must not
    /// unregister heap A's mints.
    ///
    /// Under the old single set, B's `remap_and_sweep` asked
    /// `B.is_object_address(a_handle)` — always `None` for a foreign
    /// address — and dropped the entry. A's later moving collection then saw
    /// `is_minted == false` and refused to rewrite the Java-visible `long`,
    /// leaving it pointing into vacated from-space.
    #[test]
    fn foreign_collection_does_not_sweep_away_our_mint() {
        use cratonvm_types::ClassId;

        let a = heap();
        let b = heap();
        // A real, live object in A — so the entry is legitimately retained
        // by A's own sweep and only a foreign sweep could remove it.
        let obj = a.alloc_object(ClassId::new(0), 0);
        let bits = obj.as_ptr() as u64;
        record_minted_long(&a, bits);

        // B collects. Nothing of A's may be consulted or discarded.
        remap_and_sweep(&cratonvm_types::PointerMap::default(), &b);

        assert!(
            is_minted(&a, bits),
            "heap B's collection must not unregister heap A's mint"
        );
    }

    /// A value minted against heap A must not be rewritten by heap B's
    /// relocation map.
    #[test]
    fn foreign_pointer_map_does_not_rewrite_our_mint() {
        use cratonvm_types::ClassId;

        let a = heap();
        let b = heap();
        let obj = a.alloc_object(ClassId::new(0), 0);
        let bits = obj.as_ptr() as u64;
        let elsewhere = 0x0000_7000_0000_3330u64;
        record_minted_long(&a, bits);

        // B moves an object that happens to sit at the same address.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(bits as usize, elsewhere as usize);
        remap_and_sweep(&map, &b);

        assert!(
            is_minted(&a, bits),
            "A's entry must keep its own address after B's collection"
        );
        assert!(
            !is_minted(&a, elsewhere),
            "B's relocation must not have rewritten A's entry"
        );
    }

    /// The owning heap's own sweep still works: a moved handle is relocated,
    /// and a handle whose referent no longer parses as an object is dropped.
    #[test]
    fn own_sweep_relocates_and_drops() {
        use cratonvm_types::ClassId;

        let h = heap();
        let live = h.alloc_object(ClassId::new(0), 0);
        let moved = h.alloc_object(ClassId::new(0), 0);
        let live_bits = live.as_ptr() as u64;
        let moved_bits = moved.as_ptr() as u64;
        let dead_bits = 0x1_0000u64; // not an object address in this heap
        assert!(h.is_object_address(dead_bits as usize).is_none());

        record_minted_long(&h, live_bits);
        record_minted_long(&h, dead_bits);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(live_bits as usize, moved_bits as usize);
        remap_and_sweep(&map, &h);

        assert!(
            is_minted(&h, moved_bits),
            "a relocated handle must be registered at its new address"
        );
        assert!(
            !is_minted(&h, live_bits),
            "the pre-move address must no longer be registered"
        );
        assert!(
            !is_minted(&h, dead_bits),
            "an entry whose referent is not a live object must be dropped"
        );
    }
}
