// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Post-collection fixup for **address-keyed side-tables**.
//!
//! # The hazard
//!
//! `ObjectRef`'s `Hash`/`Eq` are address-based, so a
//! `HashMap<ObjectRef, _>` silently breaks across a moving collection.
//! Two distinct failures follow, and they need opposite remedies:
//!
//! * **A survivor that moved** leaves its entry stranded under the old
//!   address, and every later lookup misses. Harmless in isolation — the
//!   table merely stops working — so this half is a *performance* bug.
//! * **An entry whose object died** is the dangerous half. The collector
//!   hands the reclaimed address back to the allocator, so a later object
//!   can land on it and collide with the stale entry. The table then
//!   answers a lookup for the *new* object with the *old* object's value:
//!   a silent wrong answer, not a miss.
//!
//! Clearing the whole table on every collection closes both and is
//! sometimes the right trade, but it discards every live entry too.
//! [`remap_and_sweep`] keeps the live ones.
//!
//! # Why this is not an `external_roots` / `native_roots` provider
//!
//! Both registries pair a `scan` half (keep the referent alive) with a
//! `remap` half (keep the address valid), and both are driven from
//! `gc::update_all_roots` **after** its `pointer_map.is_empty()` early
//! return. That is the wrong shape for a *cache*:
//!
//! * A cache must not root its keys. An entry whose object is otherwise
//!   unreachable can never be looked up again — nothing is left to name
//!   it — so rooting it would convert the table into an immortality set
//!   and leak every object it ever saw.
//! * The sweep half has to run on a **non-moving** collection too, where
//!   `pointer_map` is empty and objects still die. A remap callback
//!   registered in either registry never runs on that path.
//!
//! So this runs before the early return, alongside
//! [`crate::memory::smuggled_longs::remap_and_sweep`], which sweeps its
//! own registry for exactly the same address-reuse reason.
//!
//! # Ordering within a cycle
//!
//! Callers pass the collector's old→new `pointer_map` and a liveness
//! predicate over **pre-remap** addresses. Both are evaluated against the
//! state the collector publishes at the end of the cycle, while the world
//! is still stopped, so no mutator can allocate onto a just-reclaimed
//! address before the sweep observes it as dead.

use crate::types::ObjectRef;
use rustc_hash::FxHashMap;
use std::collections::HashMap;

/// What one [`remap_and_sweep`] pass did, for tests and diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    /// Entries whose key appeared in the pointer map and were re-keyed.
    pub moved: usize,
    /// Entries whose object survived without moving; key left alone.
    pub retained: usize,
    /// Entries whose object did not survive; dropped.
    pub dropped: usize,
}

/// Re-key `table` through the collector's relocation map and drop entries
/// whose object did not survive.
///
/// `is_live` answers "is this **pre-remap** address still a live object?"
/// — `VmHeap::is_object_address(addr).is_some()` is the usual
/// implementation. It is only consulted for keys absent from
/// `pointer_map`, since a key present there has already been proven to be
/// a relocated survivor.
///
/// Values are moved, never cloned, so `V` needs no bounds.
///
/// # Destination collisions
///
/// A key that moved is authoritative: it is inserted first, and an
/// unmoved entry is only kept if nothing already claimed its address.
/// This matters when object `A` is evacuated onto the address that dead
/// object `B` used to occupy. `is_live(B_addr)` is then true — the
/// address *is* live, but it belongs to `A` now — so a single-pass
/// implementation would keep `B`'s stale entry and let insertion order
/// decide which value survives. Today's collectors evacuate into regions
/// disjoint from the ones they free, so the collision is not reachable;
/// the two-pass order means correctness does not rest on that staying
/// true.
pub fn remap_and_sweep<V>(
    table: &mut FxHashMap<ObjectRef, V>,
    pointer_map: &HashMap<usize, usize>,
    is_live: &dyn Fn(usize) -> bool,
) -> SweepStats {
    if table.is_empty() {
        return SweepStats::default();
    }

    let mut stats = SweepStats::default();
    let mut moved: Vec<(usize, V)> = Vec::new();
    let mut stayed: Vec<(usize, V)> = Vec::new();

    for (obj, value) in table.drain() {
        let addr = obj.as_ptr() as usize;
        match pointer_map.get(&addr) {
            Some(&new_addr) => moved.push((new_addr, value)),
            None if is_live(addr) => stayed.push((addr, value)),
            // Neither relocated nor still live: the object is gone. Drop
            // the entry so a future object allocated onto the reclaimed
            // address cannot collide with it.
            None => stats.dropped += 1,
        }
    }

    for (addr, value) in moved {
        table.insert(object_ref_at(addr), value);
        stats.moved += 1;
    }
    for (addr, value) in stayed {
        let key = object_ref_at(addr);
        if table.contains_key(&key) {
            // A relocated survivor already claimed this address, so this
            // entry's object is dead after all — see "Destination
            // collisions" above.
            stats.dropped += 1;
            continue;
        }
        table.insert(key, value);
        stats.retained += 1;
    }

    stats
}

/// Rebuild an `ObjectRef` from an address the collector just published.
///
/// # Panics
///
/// Debug builds assert the address is non-null and 8-byte aligned, the
/// same precondition `ObjectRef::from_raw` documents.
#[inline]
fn object_ref_at(addr: usize) -> ObjectRef {
    debug_assert!(addr != 0 && addr % 8 == 0, "bad object address {addr:#x}");
    // SAFETY: `addr` comes from the collector's own pointer map or from a
    // key it just confirmed live, so it denotes a real object. The ref is
    // stored as a key and is not dereferenced here.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: usize = 0x1_0000;
    const B: usize = 0x2_0000;
    const C: usize = 0x3_0000;

    fn key(addr: usize) -> ObjectRef {
        object_ref_at(addr)
    }

    fn table(entries: &[(usize, u32)]) -> FxHashMap<ObjectRef, u32> {
        entries.iter().map(|&(a, v)| (key(a), v)).collect()
    }

    fn nothing_moved() -> HashMap<usize, usize> {
        HashMap::new()
    }

    #[test]
    fn moved_entry_is_rekeyed_and_keeps_its_value() {
        let mut t = table(&[(A, 7)]);
        let map = HashMap::from([(A, B)]);

        let stats = remap_and_sweep(&mut t, &map, &|_| true);

        assert_eq!(stats.moved, 1);
        assert_eq!(t.get(&key(B)), Some(&7), "value must follow the object");
        assert!(!t.contains_key(&key(A)), "old address must not linger");
    }

    #[test]
    fn live_unmoved_entry_survives_a_nonmoving_sweep() {
        let mut t = table(&[(A, 7)]);

        let stats = remap_and_sweep(&mut t, &nothing_moved(), &|addr| addr == A);

        assert_eq!(
            stats,
            SweepStats {
                moved: 0,
                retained: 1,
                dropped: 0
            }
        );
        assert_eq!(t.get(&key(A)), Some(&7));
    }

    /// The correctness-critical case: a non-moving collection publishes an
    /// empty pointer map, so a remap-only fixup would be a no-op and the
    /// dead entry would stay to collide with whatever is allocated onto
    /// its reclaimed address next.
    #[test]
    fn dead_entry_is_dropped_even_when_nothing_moved() {
        let mut t = table(&[(A, 7)]);

        let stats = remap_and_sweep(&mut t, &nothing_moved(), &|_| false);

        assert_eq!(stats.dropped, 1);
        assert!(t.is_empty(), "reclaimed address must not stay keyed");
    }

    #[test]
    fn mixed_cycle_sorts_each_entry_into_the_right_bucket() {
        let mut t = table(&[(A, 1), (B, 2), (C, 3)]);
        // A relocated to 0x40000; B survived in place; C died.
        let map = HashMap::from([(A, 0x4_0000)]);

        let stats = remap_and_sweep(&mut t, &map, &|addr| addr == B);

        assert_eq!(
            stats,
            SweepStats {
                moved: 1,
                retained: 1,
                dropped: 1
            }
        );
        assert_eq!(t.get(&key(0x4_0000)), Some(&1));
        assert_eq!(t.get(&key(B)), Some(&2));
        assert_eq!(t.len(), 2);
    }

    /// A relocated survivor evacuated onto a dead entry's old address must
    /// win, whatever order the two entries come out of the table in.
    #[test]
    fn relocated_survivor_wins_a_destination_collision() {
        let mut t = table(&[(A, 1), (B, 2)]);
        // A moves onto B's address; B is dead, but `is_live(B)` now reports
        // true because A occupies that address.
        let map = HashMap::from([(A, B)]);

        let stats = remap_and_sweep(&mut t, &map, &|_| true);

        assert_eq!(t.len(), 1);
        assert_eq!(t.get(&key(B)), Some(&1), "A's value, not B's stale one");
        assert_eq!(
            stats,
            SweepStats {
                moved: 1,
                retained: 0,
                dropped: 1
            }
        );
    }

    #[test]
    fn empty_table_is_a_no_op() {
        let mut t: FxHashMap<ObjectRef, u32> = FxHashMap::default();

        let stats = remap_and_sweep(&mut t, &HashMap::from([(A, B)]), &|_| true);

        assert_eq!(stats, SweepStats::default());
        assert!(t.is_empty());
    }
}
