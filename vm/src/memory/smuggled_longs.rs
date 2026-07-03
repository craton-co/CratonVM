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

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::memory::vm_heap::VmHeap;

/// Fast-path emptiness flag: workloads that never mint a smuggle (the vast
/// majority) pay one relaxed load per rewrite-candidate check and nothing on
/// GC cycles.
static NONEMPTY: AtomicBool = AtomicBool::new(false);

static SET: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// Record `bits` as a deliberately-minted Java-visible object handle.
///
/// Call from mint chokepoints only, after the caller has verified the value
/// is (or was derived from) a real object address. Cold: mints happen on
/// FFI/JVMTI slow paths.
pub fn record_minted_long(bits: u64) {
    if bits == 0 {
        return;
    }
    let mut g = SET.lock().unwrap_or_else(|p| p.into_inner());
    g.get_or_insert_with(HashSet::new).insert(bits);
    NONEMPTY.store(true, Ordering::Release);
}

/// Was `bits` ever minted as a Java-visible object handle (and still live)?
///
/// Called on the GC rewrite path only after a pointer-map hit, inside a
/// stop-the-world pause — the lock is uncontended there.
pub fn is_minted(bits: u64) -> bool {
    if !NONEMPTY.load(Ordering::Acquire) {
        return false;
    }
    let g = SET.lock().unwrap_or_else(|p| p.into_inner());
    g.as_ref().is_some_and(|s| s.contains(&bits))
}

/// Once per GC cycle: relocate registered handles through `pointer_map` and
/// drop entries whose referent is no longer a live heap object (the sweep
/// zeroes dead objects, so a stale entry's header no longer parses).
pub fn remap_and_sweep(pointer_map: &HashMap<usize, usize>, heap: &VmHeap) {
    if !NONEMPTY.load(Ordering::Acquire) {
        return;
    }
    let mut g = SET.lock().unwrap_or_else(|p| p.into_inner());
    let Some(set) = g.as_mut() else { return };
    if set.is_empty() {
        return;
    }
    let mut next: HashSet<u64> = HashSet::with_capacity(set.len());
    for &bits in set.iter() {
        if let Some(&new_addr) = pointer_map.get(&(bits as usize)) {
            next.insert(new_addr as u64);
        } else if heap.is_object_address(bits as usize).is_some() {
            next.insert(bits);
        }
        // else: referent dead — drop the entry so a future primitive
        // collision with the reused address cannot resurrect it.
    }
    let now_empty = next.is_empty();
    *set = next;
    if now_empty {
        NONEMPTY.store(false, Ordering::Release);
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    let mut g = SET.lock().unwrap_or_else(|p| p.into_inner());
    *g = None;
    NONEMPTY.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_rejects() {
        // NOTE: registry is process-global; this test only asserts the
        // fast-path contract for a value never minted anywhere.
        assert!(!is_minted(0xDEAD_BEE8));
    }

    #[test]
    fn record_then_hit() {
        record_minted_long(0x0000_7000_0000_1230);
        assert!(is_minted(0x0000_7000_0000_1230));
        assert!(!is_minted(0x0000_7000_0000_1238));
    }
}
