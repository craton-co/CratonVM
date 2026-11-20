//! Round 12 wave 4, lane mega3: the per-class columns (M3-1), the loader
//! resolution context (M3-2) and the owner index (M3-3) of the shared
//! megamorphic table. The machine reader is exercised by
//! `runtime_lowering::r12w4_mega3_class_slot_probe_tests`.
use super::*;
use crate::{ExecutableBuffer, JitCache};
use std::sync::atomic::{AtomicUsize, Ordering};

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
    let cid = cratonvm_types::ClassId::new(0x1204);
    let class: Arc<str> = Arc::from("R12w4Mega3");
    let method: Arc<str> = Arc::from(name);
    let desc: Arc<str> = Arc::from("()V");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    cache.get(&class, &method, &desc, cid).expect("published")
}

/// The current directory's address.
fn current_dir(table: &MegaDispatchTable) -> usize {
    // SAFETY: `class_root_addr` is the address of the table's own boxed
    // root word, alive as long as `table`.
    let root = unsafe { &*(table.class_root_addr() as *const AtomicUsize) };
    root.load(Ordering::Acquire)
}

#[test]
fn a_site_binds_its_column_after_the_root_at_fixed_offsets() {
    let table = MegaDispatchTable::new();
    let pic = JitPICSlot::new();
    let base = &pic as *const JitPICSlot as usize;
    assert_eq!(
        &pic.mega_class_slot as *const _ as usize - base,
        JitPICSlot::MEGA_CLASS_SLOT_OFFSET
    );
    assert_eq!(
        &pic.mega_class_root as *const _ as usize - base,
        JitPICSlot::MEGA_CLASS_ROOT_OFFSET
    );
    // The hashed-stub contract's earlier offsets did not move.
    assert_eq!(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET, 296);
    assert_eq!(JitPICSlot::MEGA_CLASS_SLOT_OFFSET, 300);
    assert_eq!(JitPICSlot::MEGA_CLASS_ROOT_OFFSET, 304);
    pic.bind_mega_class_slot(&table, 0);
    pic.bind_mega_class_slot(&table, MEGA_CLASS_SLOT_MAX + 1);
    assert_eq!(pic.mega_class_slot(), 0, "0 and past the cap bind nothing");
    assert_eq!(pic.mega_class_root.load(Ordering::Acquire), 0);
    pic.bind_mega_class_slot(&table, 7);
    assert_eq!(pic.mega_class_slot(), 7);
    assert_eq!(
        pic.mega_class_root.load(Ordering::Acquire),
        table.class_root_addr()
    );
    pic.bind_mega_class_slot(&table, 9);
    assert_eq!(pic.mega_class_slot(), 7, "a bound column never changes");
    // The root names a published (empty) directory from birth.
    assert_ne!(current_dir(&table), 0);
}

/// Round 17 wave 2, lane mega2: the bind answers the column the site is
/// bound to, the winner's, so a second binder publishes its cell where
/// the site reads (`r17-mega-class-slot-bind-is-check-then-store`).
#[test]
fn a_second_column_bind_answers_the_first_column() {
    let table = MegaDispatchTable::new();
    let pic = JitPICSlot::new();
    assert_eq!(pic.bind_mega_class_slot(&table, 0), 0);
    assert_eq!(pic.bind_mega_class_slot(&table, MEGA_CLASS_SLOT_MAX + 1), 0);
    assert_eq!(pic.bind_mega_class_slot(&table, 5), 5);
    assert_eq!(
        pic.bind_mega_class_slot(&table, 11),
        5,
        "the first column wins"
    );
    assert_eq!(pic.bind_mega_class_slot(&table, 0), 5);
    assert_eq!(pic.mega_class_slot(), 5);
}

#[test]
fn class_cells_publish_share_by_key_only_and_retire_with_every_pass() {
    let cache = JitCache::new();
    let body = published(&cache, "c1");
    let other = published(&cache, "c2");
    let table = MegaDispatchTable::new();
    let entry = body.entry_ptr() as u64;
    let other_entry = other.entry_ptr() as u64;
    table.install_with_class_slot(5, 1, Some((3, 8)), entry, true, false);
    assert_eq!(table.lookup_class_cell(5, 1, 3), Some((entry, true)));
    assert!(
        table.lookup(5, 1).is_some(),
        "the hashed ways are offered it too"
    );
    // A column is an index, not a target: another selector at the same
    // column of the same class is refused (the first publication holds
    // the cell) and never reads the first one's body.
    table.install_with_class_slot(5, 2, Some((3, 8)), other_entry, false, false);
    assert_eq!(table.lookup_class_cell(5, 2, 3), None);
    assert_eq!(table.lookup_class_cell(5, 1, 3), Some((entry, true)));
    // Another class has its own array.
    table.install_with_class_slot(6, 2, Some((3, 8)), other_entry, false, false);
    assert_eq!(table.lookup_class_cell(6, 2, 3), Some((other_entry, false)));
    // Past the array the first publication sized: refused.
    table.install_with_class_slot(5, 1, Some((9, 16)), entry, true, false);
    assert_eq!(table.lookup_class_cell(5, 1, 9), None);
    // Supersede-style retirement by entry reaches the cell.
    table.retire_entry(entry as usize);
    assert_eq!(table.lookup_class_cell(5, 1, 3), None);
    assert!(table.lookup(5, 1).is_none());
    // Invalidation by target set.
    let mut doomed = std::collections::HashSet::new();
    doomed.insert(other_entry as usize);
    table.invalidate_targets(&doomed);
    assert_eq!(table.lookup_class_cell(6, 2, 3), None);
    // Redefinition-style retirement by owner, and the full clear.
    table.install_with_class_slot(7, 4, Some((2, 4)), entry, false, false);
    assert!(table.lookup_class_cell(7, 4, 2).is_some());
    table.invalidate_owned_targets(&|o: &Arc<CompiledMethod>| Arc::ptr_eq(o, &body));
    assert_eq!(table.lookup_class_cell(7, 4, 2), None);
    table.install_with_class_slot(8, 4, Some((2, 4)), entry, false, false);
    assert!(table.lookup_class_cell(8, 4, 2).is_some());
    table.clear_entries();
    assert_eq!(table.lookup_class_cell(8, 4, 2), None);
    assert_eq!(table.class_cells_and_index_rows(), (0, 0));
    for b in [body, other] {
        crate::defer_jit_owner(Some(b));
    }
}

#[test]
fn a_retired_body_is_never_published_into_a_cell() {
    let cache = JitCache::new();
    let body = published(&cache, "c3");
    let table = MegaDispatchTable::new();
    body.retired.store(true, Ordering::SeqCst);
    table.install_with_class_slot(5, 1, Some((1, 4)), body.entry_ptr() as u64, false, false);
    assert_eq!(table.lookup_class_cell(5, 1, 1), None);
    assert_eq!(table.class_cells_and_index_rows(), (0, 0));
    body.retired.store(false, Ordering::SeqCst);
    crate::defer_jit_owner(Some(body));
}

/// The owner index names every live way once per entry, and a retirement
/// through it leaves no row behind.
#[test]
fn the_owner_index_tracks_hashed_ways_and_cells() {
    let cache = JitCache::new();
    let a = published(&cache, "i1");
    let b = published(&cache, "i2");
    let table = MegaDispatchTable::new();
    for class_id in 10..40u32 {
        let body = if class_id % 2 == 0 { &a } else { &b };
        let entry = body.entry_ptr() as u64;
        table.install_with_class_slot(class_id, 3, Some((1, 2)), entry, false, false);
    }
    let (cells, rows) = table.class_cells_and_index_rows();
    assert_eq!(cells, 30);
    assert_eq!(rows, 2, "one row per distinct entry");
    table.retire_entry(a.entry_ptr() as usize);
    assert_eq!(table.class_cells_and_index_rows(), (15, 1));
    for class_id in (10..40u32).filter(|c| c % 2 == 1) {
        assert!(table.lookup_class_cell(class_id, 3, 1).is_some());
    }
    table.retire_entry(b.entry_ptr() as usize);
    assert_eq!(table.class_cells_and_index_rows(), (0, 0));
    assert_eq!(table.entries_used(), 0);
    for body in [a, b] {
        crate::defer_jit_owner(Some(body));
    }
}

/// A class id past the current directory grows it; the arrays published
/// before the growth stay reachable through the new directory.
#[test]
fn the_directory_grows_and_keeps_every_array() {
    let cache = JitCache::new();
    let body = published(&cache, "d1");
    let table = MegaDispatchTable::new();
    let first = current_dir(&table);
    let entry = body.entry_ptr() as u64;
    table.install_with_class_slot(3, 1, Some((1, 1)), entry, false, false);
    table.install_with_class_slot(5000, 1, Some((1, 1)), entry, false, false);
    let dir = current_dir(&table);
    assert_ne!(dir, first, "the directory was replaced");
    // SAFETY: a published directory is `[len, array..]` and lives as long
    // as the table.
    let len = unsafe { &*(dir as *const AtomicUsize) }.load(Ordering::Acquire);
    assert!(len > 5000);
    assert!(table.lookup_class_cell(3, 1, 1).is_some());
    assert!(table.lookup_class_cell(5000, 1, 1).is_some());
    table.clear_entries();
    crate::defer_jit_owner(Some(body));
}

/// An unloaded RECEIVER's ways and cells go even when their target (an
/// inherited body) survives; other receivers' stay
/// (`r12w4-hunter2-mega-table-keeps-unloaded-receiver-ways-patch`).
#[test]
fn an_unloaded_receiver_class_loses_its_ways_and_cells() {
    let cache = JitCache::new();
    let body = published(&cache, "u1");
    let table = MegaDispatchTable::new();
    let entry = body.entry_ptr() as u64;
    table.install_with_class_slot(7, 1, Some((2, 4)), entry, false, false);
    table.install_with_class_slot(8, 1, Some((2, 4)), entry, false, false);
    assert!(table.lookup(7, 1).is_some() && table.lookup_class_cell(7, 1, 2).is_some());
    table.retire_receiver_classes(&|id: u32| id == 7);
    assert!(
        table.lookup(7, 1).is_none(),
        "the dead receiver's hashed way"
    );
    assert_eq!(
        table.lookup_class_cell(7, 1, 2),
        None,
        "the dead receiver's cell"
    );
    assert!(
        table.lookup(8, 1).is_some(),
        "another receiver's way survives"
    );
    assert_eq!(table.lookup_class_cell(8, 1, 2), Some((entry, false)));
    assert_eq!(table.class_cells_and_index_rows(), (1, 1));
    table.clear_entries();
    crate::defer_jit_owner(Some(body));
}

#[test]
fn a_loader_context_never_shares_a_selector_with_a_caller_class() {
    let table = MegaDispatchTable::new();
    let loader = |id| mega_selector_loader_context(id);
    let by_class = table.selector_id("p/A", "f", "()V", 0, 3, 0);
    let by_loader = table.selector_id_in_context("p/A", "f", "()V", 0, loader(3), 0);
    assert!(by_class.is_some() && by_loader.is_some());
    assert_ne!(by_class, by_loader);
    // Two callers of one loader share it; another loader does not.
    assert_eq!(
        table.selector_id_in_context("p/A", "f", "()V", 0, loader(3), 0),
        by_loader
    );
    assert_ne!(
        table.selector_id_in_context("p/A", "f", "()V", 0, loader(4), 0),
        by_loader
    );
}
