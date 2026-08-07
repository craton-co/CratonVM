// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-global GC object **keep-alive pin set** for JNI array access.
//!
//! # Why this exists
//!
//! `GetPrimitiveArrayCritical` / `Get<Type>ArrayElements` check out the body of
//! a Java array for native C code until the matching `Release`. The VM ships
//! *moving* collectors (the semi-space [`crate::heap::Heap`], the generational
//! [`crate::gen_heap::GenerationalHeap`] young-gen copy, the
//! [`crate::g1::G1Collector`] evacuator), so an array handed out as a raw heap
//! pointer could be relocated out from under the native code mid-call.
//!
//! The JNI layer closes the *data-movement* half of that hazard by handing
//! native code a detached **copy** of the array body (`is_copy = JNI_TRUE`),
//! never a direct heap pointer — so a relocation of the source array is
//! harmless. What remains is the *liveness* half: the source array must not be
//! **reclaimed** before the copy-back at `Release` (it may be reachable only
//! through native code that the GC's ordinary root scan does not see). This
//! module is the cross-collector primitive for that: a process-global,
//! **reference-counted** set of pinned heap addresses spliced into the root set.
//!
//! # Contract
//!
//! * [`pin`] is called on every array-critical / array-element handout, keyed by
//!   the source array object's base address.
//! * [`unpin`] is called by the matching `Release`. Pin/unpin are refcounted so
//!   nested or overlapping checkouts of the same array are safe — the address
//!   only leaves the set when its count returns to zero.
//! * A collector splices [`pinned_addrs`] into its root set so a pinned array is
//!   kept **alive** (and remapped if relocated). It does NOT need to keep the
//!   object **in place**: because native code holds a copy, not a heap pointer,
//!   relocating a pinned array is safe. [`is_pinned`] is exposed for any
//!   collector that wants to *additionally* avoid relocating pinned objects (a
//!   pure optimisation), but correctness does not depend on it.
//!
//! # Why global (not per-`Heap`)
//!
//! The JNI layer only sees `shared.heap` and a raw address; the address space
//! is process-global, and the existing per-`Heap` `gpu_pinned_refs` set is
//! feature-gated behind `gpu-offload`. A single global set keeps the JNI
//! call sites collector-agnostic and always compiled in.
//!
//! # Wiring status
//!
//! The pin set, the refcounting, the JNI `pin`/`unpin` call sites, and the
//! keep-alive root splice in [`crate::heap::Heap::collect_garbage`] (and its
//! finalizer variant) are fully implemented. The generational and G1 collectors
//! keep checked-out arrays alive through their existing root machinery
//! (`native_pin_roots` for a method-argument array, the implicit JNI local frame
//! for one the native created), so the splice is belt-and-suspenders there. No
//! per-object no-relocation enforcement is required anywhere — data-movement
//! safety is provided by the JNI copy, not by pinning the object in place.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

/// Fast gate: total number of *distinct* addresses currently pinned. Checked
/// with a single relaxed load on the hot GC per-object path so the common
/// "nothing pinned" case never touches the mutex.
static PINNED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The pin table: heap address -> pin reference count. An address is present
/// iff its count is > 0. Behind a `parking_lot::Mutex` to match the rest of the
/// crate. A `HashMap` (not a `HashSet`) so overlapping critical sections on the
/// same array refcount correctly instead of a second `unpin` prematurely
/// dropping a still-live pin.
fn table() -> &'static Mutex<cratonvm_types::PointerMap> {
    static TABLE: std::sync::OnceLock<Mutex<cratonvm_types::PointerMap>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(cratonvm_types::PointerMap::default()))
}

/// Pin the heap object at `addr` so a moving collector will neither relocate
/// nor reclaim it. Refcounted: each `pin` must be balanced by exactly one
/// [`unpin`]. A zero address is ignored (null handle / non-heap pointer).
pub fn pin(addr: usize) {
    if addr == 0 {
        return;
    }
    let mut t = table().lock();
    let entry = t.entry(addr).or_insert(0);
    if *entry == 0 {
        // First pin for this address — it becomes a distinct member of the set.
        PINNED_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    *entry += 1;
}

/// Decrement the pin count for `addr`, removing it from the set when the count
/// reaches zero. Unbalanced `unpin`s (address absent) are tolerated as no-ops
/// so a double-`Release` from native code cannot underflow the count.
pub fn unpin(addr: usize) {
    if addr == 0 {
        return;
    }
    let mut t = table().lock();
    if let Some(count) = t.get_mut(&addr) {
        *count -= 1;
        if *count == 0 {
            t.remove(&addr);
            PINNED_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// True iff the heap object at `addr` is currently pinned by at least one JNI
/// critical section. Hot-path safe: a single relaxed load short-circuits the
/// overwhelmingly common "nothing pinned" case without locking.
#[inline]
pub fn is_pinned(addr: usize) -> bool {
    if PINNED_COUNT.load(Ordering::Relaxed) == 0 {
        return false;
    }
    table().lock().contains_key(&addr)
}

/// True iff *any* address is pinned. A cheap gate a collector can check once,
/// before bothering to snapshot the set, to keep the no-pins path allocation-free.
#[inline]
pub fn any_pinned() -> bool {
    PINNED_COUNT.load(Ordering::Relaxed) != 0
}

/// Snapshot of every currently-pinned address. A moving collector splices these
/// into its root set so a pinned array reachable ONLY through native code is not
/// reclaimed (and is remapped if the collector relocates it — which is safe,
/// because native code holds a copy of the body, not a heap pointer).
pub fn pinned_addrs() -> Vec<usize> {
    if PINNED_COUNT.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    table().lock().keys().copied().collect()
}

/// Re-key pins whose object a moving collection relocated (INT-10 pairing
/// for the [`pinned_addrs`] root splice). The pin table is keyed by object
/// base address; after a move, the matching `Release`-side `unpin` (which
/// looks the object up by its CURRENT base) and any `is_pinned` query must
/// find the entry under the NEW address. Refcounts are merged if both old
/// and new keys exist (defensive — cannot happen for a bijective map).
pub fn update_after_gc(pointer_map: &cratonvm_types::PointerMap) {
    if PINNED_COUNT.load(Ordering::Relaxed) == 0 || pointer_map.is_empty() {
        return;
    }
    let mut t = table().lock();
    // Collect first: mutating while iterating is UB-adjacent for HashMap.
    let moved: Vec<(usize, usize, usize)> = t
        .iter()
        .filter_map(|(&old, &count)| {
            pointer_map
                .get(&old)
                .filter(|&&new| new != old)
                .map(|&new| (old, new, count))
        })
        .collect();
    for (old, new, count) in moved {
        t.remove(&old);
        let entry = t.entry(new).or_insert(0);
        if *entry == 0 && count > 0 {
            // net distinct-count unchanged: one removed, one added
        } else if count > 0 {
            // merged into an existing key — one distinct member fewer
            PINNED_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
        *entry += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test pins distinct addresses and fully unpins them so the global
    // table is left empty for the next test (tests in a module run on the same
    // process; keep them order-independent by balancing pin/unpin).

    #[test]
    fn pin_then_unpin_roundtrips() {
        let a = 0x1_0000_usize;
        assert!(!is_pinned(a));
        pin(a);
        assert!(is_pinned(a));
        unpin(a);
        assert!(!is_pinned(a));
    }

    #[test]
    fn refcount_is_balanced() {
        let a = 0x2_0000_usize;
        pin(a);
        pin(a); // overlapping critical section on the same array
        assert!(is_pinned(a));
        unpin(a);
        // Still pinned: only one of two pins released.
        assert!(is_pinned(a));
        unpin(a);
        assert!(!is_pinned(a));
    }

    #[test]
    fn zero_addr_is_ignored() {
        pin(0);
        assert!(!is_pinned(0));
        unpin(0); // no-op, must not panic
    }

    #[test]
    fn unbalanced_unpin_is_noop() {
        let a = 0x3_0000_usize;
        unpin(a); // never pinned — tolerated
        assert!(!is_pinned(a));
    }

    #[test]
    fn pinned_addrs_snapshots_set() {
        let a = 0x4_0000_usize;
        let b = 0x4_1000_usize;
        pin(a);
        pin(b);
        let snap = pinned_addrs();
        assert!(snap.contains(&a));
        assert!(snap.contains(&b));
        unpin(a);
        unpin(b);
    }
}
