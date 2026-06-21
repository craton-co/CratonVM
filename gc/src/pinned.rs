// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-global GC object **pin set** for JNI critical sections.
//!
//! # Why this exists
//!
//! `GetPrimitiveArrayCritical` / no-copy `Get<Type>ArrayElements` hand a raw,
//! direct pointer into a Java array's heap body back to native C code and
//! promise it stays valid until the matching `Release`. The VM ships *moving*
//! collectors (the semi-space [`crate::heap::Heap`], the generational
//! [`crate::gen_heap::GenerationalHeap`] young-gen copy, the
//! [`crate::g1::G1Collector`] evacuator). A GC that fires while native code
//! holds that pointer would relocate the array and leave the C side writing
//! through a dangling address.
//!
//! HotSpot solves this by *pinning* the object's region/page for the duration
//! of the critical section so the collector neither relocates nor reclaims it.
//! This module is the cross-collector primitive for that: a process-global,
//! **reference-counted** set of pinned heap addresses.
//!
//! # Contract
//!
//! * [`pin`] is called on every direct (no-copy) array-critical / array-element
//!   handout, keyed by the array object's base address.
//! * [`unpin`] is called by the matching `Release`. Pin/unpin are refcounted so
//!   nested or overlapping critical sections on the same array are safe — the
//!   address only leaves the set when its count returns to zero.
//! * A moving collector MUST consult [`is_pinned`] in its per-object relocation
//!   decision and, when true, keep the object **in place** (do not evacuate /
//!   forward) and **alive** (treat it as a root). See
//!   [`pinned_addrs`] for the snapshot a collector splices into its root set.
//!
//! # Why global (not per-`Heap`)
//!
//! The JNI layer only sees `shared.heap` and a raw address; the address space
//! is process-global, and the existing per-`Heap` `gpu_pinned_refs` set is
//! feature-gated behind `gpu-offload`. A single global set keeps the JNI
//! call sites collector-agnostic and always compiled in.
//!
//! # Wiring status (be honest)
//!
//! The pin set, the refcounting, and the JNI `pin`/`unpin` call sites are fully
//! implemented. The *consult* in the moving collectors' evacuation predicates is
//! only partially wired here: the semi-space [`crate::heap::Heap::collect_garbage`]
//! splices [`pinned_addrs`] into its root set (so pinned arrays are never
//! *reclaimed* and are remapped after a copy), but the per-object
//! *no-relocation* check still has to be added to the actual forwarding sites,
//! which live outside the files this change is scoped to:
//!   * `gc::try_forward_object` (semi-space copy),
//!   * `g1::G1Collector::evacuate_object` (G1 evacuation),
//!   * the young-gen copy loop in `gen_heap`.
//! Each needs the same one-liner at the top of its copy decision:
//! ```ignore
//! if crate::pinned::is_pinned(old_ptr as usize) {
//!     return old_ptr; // pinned by a JNI critical section — keep in place
//! }
//! ```
//! Those sites are marked with `TODO(jni-critical-pin)` so the follow-up is a
//! mechanical one-line edit per collector.

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
fn table() -> &'static Mutex<HashMap<usize, usize>> {
    static TABLE: std::sync::OnceLock<Mutex<HashMap<usize, usize>>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
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
/// into its root set so a pinned array reachable ONLY through the native
/// pointer is not reclaimed (and is remapped if the collector still relocates
/// it — though a correctly-wired collector keeps pinned objects in place via
/// [`is_pinned`]).
pub fn pinned_addrs() -> Vec<usize> {
    if PINNED_COUNT.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    table().lock().keys().copied().collect()
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
