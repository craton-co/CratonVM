// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loader-owned heap metadata edges used by the non-moving generational marker.
//!
//! Static reference fields, synthetic class locks, condy values and reflective
//! descriptor caches must live while their defining loader is live, but must
//! not independently root that loader forever. This registry models the
//! missing `ClassLoaderData -> metadata oops` edge in CratonVM's side-table
//! class-loader representation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

fn store() -> &'static RwLock<FxHashMap<usize, Vec<usize>>> {
    static STORE: OnceLock<RwLock<FxHashMap<usize, Vec<usize>>>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

static WEAK_MODE: AtomicBool = AtomicBool::new(false);

/// Whether loader-owned metadata roots should be conditional this collection.
pub fn metadata_weak_mode() -> bool {
    WEAK_MODE.load(Ordering::Acquire)
}

pub fn set_metadata_weak_mode(enabled: bool) {
    WEAK_MODE.store(enabled, Ordering::Release);
    if !enabled {
        store().write().clear();
    }
}

/// Replace the registry with the current loader-owned root snapshot.
pub fn replace_metadata_pins(entries: &[(usize, usize)]) {
    let mut pins = store().write();
    pins.clear();
    for &(loader, object) in entries {
        let values = pins.entry(loader).or_default();
        if !values.contains(&object) {
            values.push(object);
        }
    }
}

/// Add one root discovered by a process-global native cache scanner.
pub fn add_metadata_pin(loader: usize, object: usize) {
    let mut pins = store().write();
    let values = pins.entry(loader).or_default();
    if !values.contains(&object) {
        values.push(object);
    }
}

/// Snapshot heap addresses owned by a loader that has just become marked.
pub fn roots_for_loader(loader: usize) -> Option<Vec<usize>> {
    store().read().get(&loader).cloned()
}

/// Whole-registry snapshot for a stop-the-world marker.
///
/// `roots_for_loader` takes the registry `RwLock` and clones a `Vec` for
/// EVERY marked object, which on a multi-million-object young collection is
/// both a measurable cost and (once the marker runs on several threads) a
/// shared-cache-line hot spot -- for a registry that is empty in essentially
/// every run. The marker is at a safepoint, so one snapshot taken before the
/// closure starts is exactly equivalent, and `None` (the common case) lets it
/// skip the lookup entirely.
pub fn snapshot() -> Option<FxHashMap<usize, Vec<usize>>> {
    let g = store().read();
    if g.is_empty() {
        None
    } else {
        Some(g.clone())
    }
}

pub fn clear_metadata_pins() {
    store().write().clear();
    WEAK_MODE.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_owned_roots_are_replaced_and_deduplicated() {
        clear_metadata_pins();
        set_metadata_weak_mode(true);
        replace_metadata_pins(&[(10, 100), (10, 100), (10, 101)]);
        assert_eq!(roots_for_loader(10), Some(vec![100, 101]));
        add_metadata_pin(10, 102);
        assert_eq!(roots_for_loader(10), Some(vec![100, 101, 102]));
        replace_metadata_pins(&[(20, 200)]);
        assert!(roots_for_loader(10).is_none());
        clear_metadata_pins();
    }
}
