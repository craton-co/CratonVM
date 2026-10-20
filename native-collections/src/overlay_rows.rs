// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The collection-overlay table type: `packed object key -> row`, recording
//! which keys a mutator WROTE since a collector last asked.
//!
//! # Why a collector needs to know
//!
//! An overlay row is not a heap slot, so no write barrier sees a reference
//! stored into one. A young collection must still find every young object an
//! OLD owner's row holds; without a record of which rows changed, the only way
//! to find them is to read every row of every owner in the process on every
//! pause — the cost `docs/internal/gc/common-b-g1-roots-every-overlay-element-unconditionally-FIXED-20261005.md`
//! names. G1 instead reads the rows written since its last pause plus the rows
//! it remembered as holding young references (`gc/src/g1_overlay.rs`).
//!
//! # Why recording here is complete
//!
//! Every overlay table is an [`OverlayRows`], and the type exposes no `&mut`
//! access to its map except through the recording methods ([`OverlayRows::get_mut`],
//! [`OverlayRows::entry`], [`OverlayRows::insert`]); the unrecorded mutators
//! are named `gc_*` and exist for the collector's own remap and prune, which
//! never introduce a reference. A new mutator path that tries to write a row
//! any other way does not compile. The record and the write happen under the
//! same table lock, and a collection reads the record under that lock too, so
//! no write can fall between a collector's read and its own record.
//!
//! Recording costs one hash-set insert per mutable access, and only once
//! some collector has asked for it ([`enable_dirty_tracking`]); until then it
//! is one relaxed load.

use std::collections::hash_map::{Entry, HashMap, Iter, IterMut, Keys, Values};
use std::collections::HashSet;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once a collector that consumes the record exists in the process. A
/// latch, not per-VM state: it only decides whether a write is recorded, and a
/// record nobody reads is bounded by the live rows (a removed row leaves it).
static DIRTY_TRACKING: AtomicBool = AtomicBool::new(false);

/// Start recording written keys. Idempotent. A collector must call this
/// before the collection that first relies on the record, and must read every
/// row once (a full pass) after the call — writes before it were not recorded.
pub(crate) fn enable_dirty_tracking() {
    DIRTY_TRACKING.store(true, Ordering::Release);
}

#[inline]
fn tracking() -> bool {
    DIRTY_TRACKING.load(Ordering::Relaxed)
}

/// One overlay table. See the module doc.
pub(crate) struct OverlayRows<V, S = std::collections::hash_map::RandomState> {
    map: HashMap<usize, V, S>,
    /// Keyed by the PACKED object key (`identity_hash << 32 | generation`),
    /// so NOT an `FxHashSet`: Fx multiplies the key, and keys that differ
    /// only in their high half then share buckets — measured quadratic
    /// (seconds per pause) at 60k collections.
    dirty: HashSet<usize>,
}

impl<V, S: Default> Default for OverlayRows<V, S> {
    fn default() -> Self {
        Self {
            map: HashMap::with_hasher(S::default()),
            dirty: HashSet::new(),
        }
    }
}

impl<V> OverlayRows<V> {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

impl<V, S: BuildHasher> OverlayRows<V, S> {
    #[inline]
    fn note(&mut self, key: usize) {
        if tracking() {
            self.dirty.insert(key);
        }
    }

    // -- reads -------------------------------------------------------------

    #[inline]
    pub(crate) fn get(&self, key: &usize) -> Option<&V> {
        self.map.get(key)
    }
    #[inline]
    pub(crate) fn contains_key(&self, key: &usize) -> bool {
        self.map.contains_key(key)
    }
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
    #[inline]
    pub(crate) fn iter(&self) -> Iter<'_, usize, V> {
        self.map.iter()
    }
    #[inline]
    pub(crate) fn keys(&self) -> Keys<'_, usize, V> {
        self.map.keys()
    }
    #[inline]
    pub(crate) fn values(&self) -> Values<'_, usize, V> {
        self.map.values()
    }

    // -- recorded writes ----------------------------------------------------

    #[inline]
    pub(crate) fn get_mut(&mut self, key: &usize) -> Option<&mut V> {
        self.note(*key);
        self.map.get_mut(key)
    }
    #[inline]
    pub(crate) fn entry(&mut self, key: usize) -> Entry<'_, usize, V> {
        self.note(key);
        self.map.entry(key)
    }
    #[inline]
    pub(crate) fn insert(&mut self, key: usize, value: V) -> Option<V> {
        self.note(key);
        self.map.insert(key, value)
    }
    /// Removing a row stores no reference, so it is not recorded; the key
    /// leaves the record with its row.
    #[inline]
    pub(crate) fn remove(&mut self, key: &usize) -> Option<V> {
        self.dirty.remove(key);
        self.map.remove(key)
    }

    // -- the collector's own, unrecorded --------------------------------------

    /// Mutable iteration for the post-collection REMAP, which rewrites a
    /// reference to its relocated address and never introduces one.
    #[inline]
    pub(crate) fn gc_iter_mut(&mut self) -> IterMut<'_, usize, V> {
        self.map.iter_mut()
    }
    /// Retain for the PRUNE and the VM teardown; the record follows.
    pub(crate) fn gc_retain(&mut self, mut keep: impl FnMut(&usize, &mut V) -> bool) {
        let dirty = &mut self.dirty;
        self.map.retain(|k, v| {
            let kept = keep(k, v);
            if !kept {
                dirty.remove(k);
            }
            kept
        });
    }
    /// Take the recorded keys. The caller puts back what it does not consume
    /// with [`Self::restore_dirty`].
    pub(crate) fn take_dirty(&mut self) -> HashSet<usize> {
        std::mem::take(&mut self.dirty)
    }
    /// Re-record keys another VM's collector still has to read.
    pub(crate) fn restore_dirty(&mut self, keys: impl IntoIterator<Item = usize>) {
        for key in keys {
            if self.map.contains_key(&key) {
                self.dirty.insert(key);
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn is_dirty(&self, key: usize) -> bool {
        self.dirty.contains(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_recorded_once_tracking_is_on_and_removal_forgets() {
        enable_dirty_tracking();
        let mut rows: OverlayRows<u32> = OverlayRows::new();
        rows.insert(1, 10);
        *rows.entry(2).or_default() += 1;
        if let Some(v) = rows.get_mut(&1) {
            *v += 1;
        }
        assert!(rows.is_dirty(1) && rows.is_dirty(2));
        let _ = rows.get(&1);
        let taken = rows.take_dirty();
        assert_eq!(taken.len(), 2);
        assert!(!rows.is_dirty(1), "taking clears the record");
        let _ = rows.get(&1);
        assert!(!rows.is_dirty(1), "a read is not a write");
        rows.insert(3, 0);
        rows.remove(&3);
        assert!(!rows.is_dirty(3), "a removed row leaves the record");
        rows.restore_dirty([1, 99]);
        assert!(rows.is_dirty(1));
        assert!(
            !rows.is_dirty(99),
            "only keys that still have a row come back"
        );
        for (_, v) in rows.gc_iter_mut() {
            *v += 1;
        }
        rows.take_dirty();
        rows.gc_retain(|_, _| true);
        assert!(
            !rows.is_dirty(1),
            "the collector's own writes are not recorded"
        );
    }
}
