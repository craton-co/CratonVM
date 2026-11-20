//! Round 12 wave 2, lane calls (proposal W2-1, step 1): the shared
//! megamorphic dispatch table's publication protocol.
use super::*;
use crate::{ExecutableBuffer, JitCache};
use std::sync::atomic::Ordering;

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

/// A published body (so its entry has an owner the table can retain).
fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
    let cid = cratonvm_types::ClassId::new(0x1202);
    let class: Arc<str> = Arc::from("R12w2Mega");
    let method: Arc<str> = Arc::from(name);
    let desc: Arc<str> = Arc::from("()V");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    cache.get(&class, &method, &desc, cid).expect("published")
}

#[test]
fn keys_never_collide_with_the_sentinels_and_sets_stay_in_range() {
    for class_id in [0u32, 1, 7, 12_345, u32::MAX - 2] {
        for selector in [1u32, 2, 65_536] {
            let key = MegaDispatchTable::key(class_id, selector);
            assert_ne!(key, MegaDispatchTable::EMPTY_KEY);
            assert_ne!(key, MegaDispatchTable::RETIRED_KEY);
            let base = MegaDispatchTable::set_base(class_id, selector);
            assert_eq!(base % MEGA_DISPATCH_WAYS, 0);
            assert!(base + MEGA_DISPATCH_WAYS <= MEGA_DISPATCH_ENTRIES);
        }
    }
}

#[test]
fn selectors_intern_per_resolution_record() {
    let table = MegaDispatchTable::new();
    let a = table.selector_id("p/A", "f", "()V", 0, 11, 0);
    assert_eq!(a, Some(1));
    assert_eq!(
        table.selector_id("p/A", "f", "()V", 0, 11, 0),
        a,
        "same record"
    );
    // Any field that the resolution reads makes another selector.
    assert_eq!(table.selector_id("p/A", "f", "()V", 2, 11, 0), Some(2));
    assert_eq!(table.selector_id("p/A", "f", "()V", 0, 12, 0), Some(3));
    assert_eq!(table.selector_id("p/A", "g", "()V", 0, 11, 0), Some(4));
}

#[test]
fn install_lookup_retire_and_invalidate() {
    let cache = JitCache::new();
    let body = published(&cache, "m");
    let other = published(&cache, "n");
    let table = MegaDispatchTable::new();
    let entry = body.entry_ptr() as u64;
    // An address with no owner is never published.
    table.install(5, 1, 0x7000, false, false);
    assert!(table.lookup(5, 1).is_none());
    table.install(5, 1, entry, true, false);
    let (got, ctx, owner) = table.lookup(5, 1).expect("hit");
    assert_eq!((got, ctx), (entry, true));
    assert!(owner.is_some_and(|o| Arc::ptr_eq(&o, &body)));
    // Another selector or class misses.
    assert!(table.lookup(5, 2).is_none());
    assert!(table.lookup(6, 1).is_none());
    // Re-installing the same word is a no-op; a different target for
    // the key retires the old way and publishes into another.
    table.install(5, 1, entry, true, false);
    assert_eq!(table.entries_used(), 1);
    table.install(5, 1, other.entry_ptr() as u64, false, false);
    assert_eq!(table.entries_used(), 1);
    assert_eq!(
        table.lookup(5, 1).map(|(e, c, _)| (e, c)),
        Some((other.entry_ptr() as u64, false))
    );
    // Supersede-style retirement by entry.
    table.retire_entry(other.entry_ptr() as usize);
    assert!(table.lookup(5, 1).is_none());
    // Invalidation by target set.
    table.install(9, 3, entry, false, false);
    let mut doomed = std::collections::HashSet::new();
    doomed.insert(entry as usize);
    table.invalidate_targets(&doomed);
    assert!(table.lookup(9, 3).is_none());
    // Redefinition-style retirement by owner, and the full clear.
    table.install(9, 4, entry, false, false);
    table.invalidate_owned_targets(&|o: &Arc<CompiledMethod>| Arc::ptr_eq(o, &body));
    assert!(table.lookup(9, 4).is_none());
    table.install(9, 5, entry, false, false);
    table.clear_entries();
    assert_eq!(table.entries_used(), 0);
    for b in [body, other] {
        crate::defer_jit_owner(Some(b));
    }
}

/// A retired body is never published, and a full set never evicts a live
/// way (the resolving helper stays the overflow path).
#[test]
fn retired_bodies_and_full_sets_publish_nothing() {
    let cache = JitCache::new();
    let body = published(&cache, "r");
    let table = MegaDispatchTable::new();
    body.retired.store(true, Ordering::SeqCst);
    table.install(5, 1, body.entry_ptr() as u64, false, false);
    assert!(table.lookup(5, 1).is_none());
    body.retired.store(false, Ordering::SeqCst);
    // Fill one set with WAYS keys of distinct classes that hash there.
    let selector = 7;
    let target = MegaDispatchTable::set_base(1, selector);
    let same_set: Vec<u32> = (1u32..200_000)
        .filter(|&c| MegaDispatchTable::set_base(c, selector) == target)
        .take(MEGA_DISPATCH_WAYS + 1)
        .collect();
    assert_eq!(same_set.len(), MEGA_DISPATCH_WAYS + 1);
    for &c in &same_set {
        table.install(c, selector, body.entry_ptr() as u64, false, false);
    }
    let hits = same_set
        .iter()
        .filter(|&&c| table.lookup(c, selector).is_some())
        .count();
    assert_eq!(
        hits, MEGA_DISPATCH_WAYS,
        "the fifth class of a full set is not published"
    );
    table.clear_entries();
    crate::defer_jit_owner(Some(body));
}

#[test]
fn a_site_binds_the_table_word_before_its_selector_at_fixed_offsets() {
    let table = MegaDispatchTable::new();
    let pic = JitPICSlot::new();
    assert_eq!(pic.mega_selector(), 0);
    pic.bind_mega_dispatch(&table, 0);
    assert_eq!(pic.mega_selector(), 0, "selector 0 binds nothing");
    pic.bind_mega_dispatch(&table, 3);
    assert_eq!(pic.mega_selector(), 3);
    let base = &pic as *const JitPICSlot as usize;
    assert_eq!(
        &pic.mega_dispatch_table as *const _ as usize - base,
        JitPICSlot::MEGA_DISPATCH_TABLE_OFFSET
    );
    assert_eq!(
        &pic.mega_selector as *const _ as usize - base,
        JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET
    );
    assert_eq!(
        pic.mega_dispatch_table.load(Ordering::Acquire),
        table.slots_base()
    );
}
