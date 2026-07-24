// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Active compiled-frame ownership used to keep defining loaders alive.

use std::sync::OnceLock;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

#[derive(Default)]
struct Registry {
    owners: FxHashMap<usize, u32>,
    active: FxHashMap<u32, usize>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

pub fn register_executable_owner(entry: usize, class_id: u32) {
    registry().lock().owners.insert(entry, class_id);
}

pub fn unregister_executable_owner(entry: usize) {
    registry().lock().owners.remove(&entry);
}

/// Mark one compiled activation active, returning its owning ClassId.
pub fn enter(entry: usize) -> Option<u32> {
    let mut state = registry().lock();
    let class_id = *state.owners.get(&entry)?;
    *state.active.entry(class_id).or_insert(0) += 1;
    Some(class_id)
}

pub fn exit(class_id: u32) {
    let mut state = registry().lock();
    if let Some(count) = state.active.get_mut(&class_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            state.active.remove(&class_id);
        }
    }
}

pub fn active_class_ids() -> Vec<u32> {
    registry().lock().active.keys().copied().collect()
}

pub fn clear() {
    *registry().lock() = Registry::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_owner_counts_survive_nested_entries() {
        clear();
        register_executable_owner(0x1000, 7);
        assert_eq!(enter(0x1000), Some(7));
        assert_eq!(enter(0x1000), Some(7));
        assert_eq!(active_class_ids(), vec![7]);
        exit(7);
        assert_eq!(active_class_ids(), vec![7]);
        exit(7);
        assert!(active_class_ids().is_empty());
        unregister_executable_owner(0x1000);
        assert_eq!(enter(0x1000), None);
    }
}
