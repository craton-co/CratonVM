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

/// `loader heap address -> (owning VM, [loader-owned heap addresses])`.
///
/// The key is a heap address, unique across every live VM in the process, so
/// the marker's read path needs no VM identity. The owning VM is recorded so
/// [`forget_vm_metadata_pins`] can drop exactly one VM's rows at teardown, in
/// place of the blanket wipe that used to run at VM *creation* — where every
/// row present belongs to a concurrently-live VM.
fn store() -> &'static RwLock<FxHashMap<usize, (usize, Vec<usize>)>> {
    static STORE: OnceLock<RwLock<FxHashMap<usize, (usize, Vec<usize>)>>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

static WEAK_MODE: AtomicBool = AtomicBool::new(false);

/// Whether loader-owned metadata roots should be conditional this collection.
pub fn metadata_weak_mode() -> bool {
    WEAK_MODE.load(Ordering::Acquire)
}

/// `enabled == false` drops **`vm`'s** rows: with weak mode off this VM roots
/// its loader-owned metadata unconditionally and the registry is dead weight
/// for it, but another VM may still be mid-collection with weak mode on.
pub fn set_metadata_weak_mode(vm: usize, enabled: bool) {
    WEAK_MODE.store(enabled, Ordering::Release);
    if !enabled {
        forget_vm_metadata_pins(vm);
    }
}

/// Replace **`vm`'s** rows with the current loader-owned root snapshot.
pub fn replace_metadata_pins(vm: usize, entries: &[(usize, usize)]) {
    let mut pins = store().write();
    pins.retain(|_, (owner, _)| *owner != vm);
    for &(loader, object) in entries {
        let row = pins.entry(loader).or_insert_with(|| (vm, Vec::new()));
        row.0 = vm;
        if !row.1.contains(&object) {
            row.1.push(object);
        }
    }
}

/// Add one root discovered by a process-global native cache scanner.
pub fn add_metadata_pin(vm: usize, loader: usize, object: usize) {
    let mut pins = store().write();
    let row = pins.entry(loader).or_insert_with(|| (vm, Vec::new()));
    row.0 = vm;
    if !row.1.contains(&object) {
        row.1.push(object);
    }
}

/// Snapshot heap addresses owned by a loader that has just become marked.
pub fn roots_for_loader(loader: usize) -> Option<Vec<usize>> {
    store().read().get(&loader).map(|(_, v)| v.clone())
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
        Some(g.iter().map(|(&k, (_, v))| (k, v.clone())).collect())
    }
}

/// Drop every row `vm` owns, at that VM's teardown.
pub fn forget_vm_metadata_pins(vm: usize) {
    store().write().retain(|_, (owner, _)| *owner != vm);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `WEAK_MODE` is one process-wide flag and the store is one process-wide
    /// map, so these two run one at a time even though their VM ids differ.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn loader_owned_roots_are_replaced_and_deduplicated() {
        const VM: usize = 0xA00;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(VM);
        set_metadata_weak_mode(VM, true);
        replace_metadata_pins(VM, &[(10, 100), (10, 100), (10, 101)]);
        assert_eq!(roots_for_loader(10), Some(vec![100, 101]));
        add_metadata_pin(VM, 10, 102);
        assert_eq!(roots_for_loader(10), Some(vec![100, 101, 102]));
        replace_metadata_pins(VM, &[(20, 200)]);
        assert!(roots_for_loader(10).is_none());
        forget_vm_metadata_pins(VM);
    }

    #[test]
    fn one_vms_reset_leaves_a_concurrently_live_vms_roots_alone() {
        const VM_A: usize = 0xA01;
        const VM_B: usize = 0xB01;
        let _g = TEST_LOCK.lock();
        forget_vm_metadata_pins(VM_A);
        forget_vm_metadata_pins(VM_B);
        add_metadata_pin(VM_A, 0x1000, 0x1008);
        add_metadata_pin(VM_B, 0x2000, 0x2008);

        // Both of the wipes this replaced — VM creation, and weak mode going
        // off — took the whole table.
        replace_metadata_pins(VM_A, &[]);
        set_metadata_weak_mode(VM_A, false);
        assert_eq!(roots_for_loader(0x1000), None);
        assert_eq!(roots_for_loader(0x2000), Some(vec![0x2008]));

        forget_vm_metadata_pins(VM_B);
    }
}
