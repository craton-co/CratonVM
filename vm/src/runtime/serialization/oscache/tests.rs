use super::*;
use std::sync::Arc;
use std::thread;

/// Build a fake `ObjectRef` with a distinct pointer value. Because
/// the cache only stores and returns the ref (never dereferences
/// it), we can fabricate one pointing at an integer for unit-test
/// purposes. 8-byte alignment is required by `ObjectRef::from_raw`.
fn fake_ref(n: usize) -> ObjectRef {
    // SAFETY: we never dereference this pointer in this test; the
    // cache only stores and returns it by copy.
    unsafe { ObjectRef::from_raw((n * 8 + 8) as *mut u8) }
}

#[test]
fn empty_cache_returns_none() {
    let cache = OscCache::new();
    assert!(cache.is_empty());
    assert!(cache.get(ClassId::new(1)).is_none());
}

#[test]
fn insert_and_retrieve() {
    let cache = OscCache::new();
    let r = fake_ref(1);
    cache.insert_if_absent(ClassId::new(7), r);
    assert_eq!(cache.get(ClassId::new(7)), Some(r));
    assert_eq!(cache.len(), 1);
}

#[test]
fn get_or_insert_with_builds_once() {
    let cache = OscCache::new();
    let cid = ClassId::new(42);
    let r1 = fake_ref(1);
    let r2 = fake_ref(2);

    let a = cache.get_or_insert_with(cid, || r1);
    let b = cache.get_or_insert_with(cid, || r2); // should NOT rebuild
    assert_eq!(a, r1);
    assert_eq!(b, r1, "second lookup must return the first insert");
}

#[test]
fn different_classes_have_different_entries() {
    let cache = OscCache::new();
    let r1 = fake_ref(1);
    let r2 = fake_ref(2);
    cache.insert_if_absent(ClassId::new(1), r1);
    cache.insert_if_absent(ClassId::new(2), r2);
    assert_eq!(cache.get(ClassId::new(1)), Some(r1));
    assert_eq!(cache.get(ClassId::new(2)), Some(r2));
    assert_ne!(
        cache.get(ClassId::new(1)),
        cache.get(ClassId::new(2)),
        "distinct classes must yield distinct descriptors"
    );
}

#[test]
fn concurrent_inserts_are_idempotent() {
    // Two threads racing on the same class must land on the same
    // descriptor. The winner is whichever thread's insert runs
    // first under the write lock; whichever loses drops its
    // ObjectRef on the floor but both observers see the same value.
    let cache = Arc::new(OscCache::new());
    let cid = ClassId::new(99);

    let cache1 = Arc::clone(&cache);
    let cache2 = Arc::clone(&cache);
    let t1 = thread::spawn(move || cache1.get_or_insert_with(cid, || fake_ref(1)));
    let t2 = thread::spawn(move || cache2.get_or_insert_with(cid, || fake_ref(2)));
    let v1 = t1.join().unwrap();
    let v2 = t2.join().unwrap();
    assert_eq!(v1, v2, "concurrent inserts must agree on the winner");
}

#[test]
fn clear_resets_the_cache() {
    let cache = OscCache::new();
    cache.insert_if_absent(ClassId::new(1), fake_ref(1));
    assert_eq!(cache.len(), 1);
    cache.clear();
    assert!(cache.is_empty());
    assert!(cache.get(ClassId::new(1)).is_none());
}

#[test]
fn scan_yields_cached_descriptor() {
    let seeded = fake_ref(0x5_0001);
    let cache = OscCache::new();
    cache.insert_if_absent(ClassId::new(5001), seeded);

    let mut roots = Vec::new();
    cache.scan_roots(&mut |_, desc| roots.push(desc));
    assert!(
        roots.contains(&seeded),
        "scan must surface the cached descriptor as a GC root"
    );
}

/// The isolation property the process-global registry did not have:
/// scanning ONE cache must not report another cache's descriptors.
///
/// Under the old `Mutex<Vec<*const OscMap>>` fan-out, every live
/// cache in the process contributed to every root scan, so a second
/// VM's collector received addresses belonging to a heap it does not
/// own. This asserts the scan is bounded by the receiver.
#[test]
fn scan_does_not_leak_another_caches_descriptors() {
    let mine = fake_ref(0x5_1001);
    let theirs = fake_ref(0x5_1002);
    let vm_a = OscCache::new();
    let vm_b = OscCache::new();
    vm_a.insert_if_absent(ClassId::new(5101), mine);
    vm_b.insert_if_absent(ClassId::new(5101), theirs);

    let mut roots = Vec::new();
    vm_a.scan_roots(&mut |_, desc| roots.push(desc));
    assert!(roots.contains(&mine), "own descriptor must be rooted");
    assert!(
        !roots.contains(&theirs),
        "scanning VM A's cache must not surface VM B's descriptor \
         (that is a pointer into a heap A's collector does not own)"
    );
}

#[test]
fn remap_rewrites_cached_descriptor() {
    let old_ref = fake_ref(0x6_0001);
    let new_ref = fake_ref(0x6_0002);
    let cache = OscCache::new();
    let cid = ClassId::new(6001);
    cache.insert_if_absent(cid, old_ref);

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(old_ref.as_ptr() as usize, new_ref.as_ptr() as usize);
    cache.remap_roots(&map);

    assert_eq!(
        cache.get(cid),
        Some(new_ref),
        "remap must rewrite the cached ref to its relocated address"
    );
}

/// The remap counterpart of the scan-isolation test above: VM B's
/// relocation map must not rewrite VM A's entries. Two heaps can
/// hand out the same address, so a shared fan-out silently repointed
/// the other VM's descriptor at an unrelated object.
#[test]
fn remap_does_not_rewrite_another_caches_descriptors() {
    let shared_addr = fake_ref(0x6_1001);
    let relocated = fake_ref(0x6_1002);
    let vm_a = OscCache::new();
    let vm_b = OscCache::new();
    let cid = ClassId::new(6101);
    vm_a.insert_if_absent(cid, shared_addr);
    vm_b.insert_if_absent(cid, shared_addr);

    // VM B collects and moves the object at `shared_addr`.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(shared_addr.as_ptr() as usize, relocated.as_ptr() as usize);
    vm_b.remap_roots(&map);

    assert_eq!(vm_b.get(cid), Some(relocated), "B's own entry moves");
    assert_eq!(
        vm_a.get(cid),
        Some(shared_addr),
        "B's relocation map must not rewrite A's entry"
    );
}

#[test]
fn remap_leaves_unmoved_refs_untouched() {
    let kept = fake_ref(0x7_0001);
    let cache = OscCache::new();
    let cid = ClassId::new(7001);
    cache.insert_if_absent(cid, kept);

    // A remap table that mentions a *different* address must not
    // disturb our entry.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xDEAD_0000usize, 0xBEEF_0000usize);
    cache.remap_roots(&map);

    assert_eq!(cache.get(cid), Some(kept));
}
