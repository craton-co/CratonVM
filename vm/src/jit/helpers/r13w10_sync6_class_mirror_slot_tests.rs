use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

/// The per-class mirror slot: a mutator mints it from the mirror the class
/// has, the door's mint and the compile-time lookup agree on ONE word,
/// the word follows a relocation, the background worker finds but never
/// mints one, and a redefinition retires it with the class's `ldc` slots.
#[test]
fn one_collector_maintained_mirror_slot_per_class() {
    if ldc_slot_memo_latched_off() || !compiled_ldc_const_cache_enabled() {
        // `0` by design under the kill switches.
        return;
    }
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let (cid, other) = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(cid) = cm.try_ensure_synthetic_class("cratonvm/test/R13Sync6MirrorHolder", 0)
        else {
            panic!("Compatible mode fabricates this fixture");
        };
        let Ok(other) = cm.try_ensure_synthetic_class("cratonvm/test/R13Sync6MirrorOther", 0)
        else {
            panic!("Compatible mode fabricates this fixture");
        };
        (cid, other)        };
    let before = shared.mem.heap.alloc_object(cid, 0);
    let after = shared.mem.heap.alloc_object(cid, 0);
    let other_mirror = shared.mem.heap.alloc_object(other, 0);

    assert_eq!(
        class_mirror_slot_for(&shared, cid),
        0,
        "no mirror yet: no slot"
    );
    shared.classes.class_mirrors.write().insert(cid, before);
    shared
        .classes
        .class_mirrors
        .write()
        .insert(other, other_mirror);

    // The worker finds no slot and mints none.
    let worker = {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(BACKGROUND_COMPILER_THREAD_NAME.to_string())
            .spawn(move || class_mirror_slot_for(&shared, cid))
            .map(|h| h.join().unwrap_or(usize::MAX))
            .unwrap_or(usize::MAX)
    };
    assert_eq!(worker, 0, "the GC-neutral worker never mints");

    // A mutator mints it from the class's mirror.
    let slot = class_mirror_slot_for(&shared, cid);
    assert_ne!(slot, 0);
    // SAFETY: `slot` is a live `Box<ObjectRef>` owned by `shared`'s
    // global-ref table, which outlives this test body.
    let read = || unsafe { *(slot as *const ObjectRef) };
    assert_eq!(read().as_ptr(), before.as_ptr());
    assert_eq!(
        class_mirror_slot_for(&shared, cid),
        slot,
        "one slot per class"
    );
    assert_ne!(class_mirror_slot_for(&shared, other), slot, "per class");

    // The door's mint and the lookup name the same word; the worker now
    // finds it. Round 15 wave 4 (lane sync3): the mint answers `true` on
    // this thread's memo miss only, the door's once-per-thread cold path;
    // the compiled-caller door's form asks its switch there, and a refusal
    // fills nothing.
    let probe = || ldc_slot_memo_probe(shared.vm_identity, cid, CLASS_MIRROR_SLOT_CP_IDX);
    assert!(!mint_static_sync_mirror_slot_if(
        &shared,
        cid,
        before,
        || false
    ));
    assert_eq!(probe(), None, "a refused mint fills nothing");
    assert!(
        mint_static_sync_mirror_slot_if(&shared, cid, before, || true),
        "a miss, a slot"
    );
    let asked = std::cell::Cell::new(false);
    let admit = || {
        asked.set(true);
        true
    };
    assert!(
        !mint_static_sync_mirror_slot_if(&shared, cid, before, admit),
        "the memo's hit"
    );
    assert!(!asked.get(), "the switch is not asked on a hit");
    assert_eq!(
        ldc_slot_memo_probe(shared.vm_identity, cid, CLASS_MIRROR_SLOT_CP_IDX),
        Some(before.as_ptr() as i64)
    );
    assert!(
        !mint_static_sync_mirror_slot(&shared, cid, before),
        "the memo's hit"
    );
    let worker = {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(BACKGROUND_COMPILER_THREAD_NAME.to_string())
            .spawn(move || class_mirror_slot_for(&shared, cid))
            .map(|h| h.join().unwrap_or(usize::MAX))
            .unwrap_or(usize::MAX)
    };
    assert_eq!(worker, slot, "the worker uses a slot that exists");

    // A moving collection rewrites the word in place.
    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(before.as_ptr() as usize, after.as_ptr() as usize);
    shared
        .natives
        .jni_global_refs
        .lock()
        .update_after_gc(&moved);
    assert_eq!(read().as_ptr(), after.as_ptr());

    // A redefinition of the class retires its slot from the table (the
    // old word stays valid for code compiled before it).
    retire_ldc_slots_of_class(&shared, cid);
    if LDC_SLOT_MEMO_SURVIVES_REDEFINITION {
        let key = (cid.as_u32(), CLASS_MIRROR_SLOT_CP_IDX);
        assert_eq!(ldc_global_slots(&shared).lock().get(&key).copied(), None);
        assert_eq!(read().as_ptr(), after.as_ptr());
    }
}
