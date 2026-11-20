use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

/// The redefinition count a fill read before its record.
pub fn redefinitions() -> u64 {
    crate::classloading::class_redefinition_count()
}

/// The memo answers through a slot the collector rewrites: after a
/// simulated relocation (the exact call every moving collection makes on
/// the JNI global-ref table) the SAME memo entry answers the NEW address.
/// A different VM identity, holder or index never matches.
#[test]
pub fn a_memoized_literal_follows_its_object_across_a_relocation() {
    if ldc_slot_memo_latched_off() {
        // Only under the kill switch, and sticky: bypassed by design.
        return;
    }
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        cm.try_ensure_synthetic_class("cratonvm/test/R9w3LdcHolder", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --real-jdk")
    };
    let before = shared.mem.heap.alloc_object(cid, 0);
    let after = shared.mem.heap.alloc_object(cid, 0);
    let holder = ClassId::new(0x00AB_CDEF);
    assert_eq!(ldc_slot_memo_probe(shared.vm_identity, holder, 7), None);

    ldc_slot_memo_fill(&shared, holder, 7, before, redefinitions());
    assert_eq!(
        ldc_slot_memo_probe(shared.vm_identity, holder, 7),
        Some(before.as_ptr() as i64)
    );
    assert_eq!(ldc_slot_memo_probe(shared.vm_identity, holder, 8), None);
    assert_eq!(ldc_slot_memo_probe(shared.vm_identity + 1, holder, 7), None);

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(before.as_ptr() as usize, after.as_ptr() as usize);
    shared
        .natives
        .jni_global_refs
        .lock()
        .update_after_gc(&moved);
    assert_eq!(
        ldc_slot_memo_probe(shared.vm_identity, holder, 7),
        Some(after.as_ptr() as i64),
        "the memo must read the collector-maintained slot, not a copied pointer"
    );

    // A second fill of the same site reuses the one global ref.
    let refs = shared.natives.jni_global_refs.lock().count();
    ldc_slot_memo_fill(&shared, holder, 7, after, redefinitions());
    assert_eq!(shared.natives.jni_global_refs.lock().count(), refs);
}

/// Round 9 wave 7 (`ldc7`): the compile-time slot for a RECORDED site is
/// the word the compiled probe reads. It holds the recorded constant, it
/// follows a relocation in place, the helper memo shares it, and an
/// unrecorded site gets none (the helper call stays).
#[test]
pub fn the_compile_time_ldc_slot_is_the_collector_maintained_word() {
    if ldc_slot_memo_latched_off() || !compiled_ldc_const_cache_enabled() {
        // Latched (kill switch) / switched off by the environment: `0` by
        // design.
        return;
    }
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        cm.try_ensure_synthetic_class("cratonvm/test/R9w7LdcSlotHolder", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --real-jdk")
    };
    let before = shared.mem.heap.alloc_object(cid, 0);
    let after = shared.mem.heap.alloc_object(cid, 0);

    assert_eq!(
        compiled_ldc_slot_for(&shared, cid, 11),
        0,
        "nothing recorded: no slot, the site keeps its helper call"
    );

    crate::runtime::interpreter::constants::store_recorded_cp_constant(
        &shared,
        cid,
        11,
        Value::Object(Some(before)),
    );
    let slot = compiled_ldc_slot_for(&shared, cid, 11);
    assert_ne!(slot, 0, "a recorded site gets a slot");
    // SAFETY: `slot` is a live `Box<ObjectRef>` owned by `shared`'s
    // global-ref table, which outlives this test body.
    let read = || unsafe { *(slot as *const ObjectRef) };
    assert_eq!(read().as_ptr(), before.as_ptr());
    assert_eq!(
        compiled_ldc_slot_for(&shared, cid, 11),
        slot,
        "one slot per site"
    );

    // The helper memo answers through the SAME word.
    ldc_slot_memo_fill(&shared, cid, 11, before, redefinitions());
    assert_eq!(
        ldc_slot_memo_probe(shared.vm_identity, cid, 11),
        Some(before.as_ptr() as i64)
    );

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(before.as_ptr() as usize, after.as_ptr() as usize);
    shared
        .natives
        .jni_global_refs
        .lock()
        .update_after_gc(&moved);
    assert_eq!(
        read().as_ptr(),
        after.as_ptr(),
        "the baked address must name the relocated object after a moving collection"
    );
    assert_eq!(
        ldc_slot_memo_probe(shared.vm_identity, cid, 11),
        Some(after.as_ptr() as i64)
    );
}

/// Interpreter round i1 wave 18, lane L4: a redefinition retires exactly
/// the redefined class's slots, and a fill whose read of the record began
/// before a redefinition is neither published nor memoized. Another
/// class's slot keeps serving. The process count is not moved here (other
/// tests memoize in this process); the stamps are set around it instead.
#[test]
pub fn a_redefinition_retires_only_the_redefined_classs_ldc_slots() {
    if ldc_slot_memo_latched_off() || !LDC_SLOT_MEMO_SURVIVES_REDEFINITION {
        return;
    }
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        cm.try_ensure_synthetic_class("cratonvm/test/I18L4LdcHolder", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --real-jdk")
    };
    let literal = shared.mem.heap.alloc_object(cid, 0);
    let redefined = ClassId::new(0x00AB_1801);
    let untouched = ClassId::new(0x00AB_1802);
    let slot_of = |holder: ClassId| {
        ldc_global_slots(&shared)
            .lock()
            .get(&(holder.as_u32(), 3))
            .copied()
    };

    // A read that began before a redefinition: nothing published or kept.
    let refs = shared.natives.jni_global_refs.lock().count();
    ldc_slot_memo_fill(
        &shared,
        redefined,
        3,
        literal,
        redefinitions().wrapping_sub(1),
    );
    assert_eq!(slot_of(redefined), None);
    assert_eq!(ldc_slot_memo_probe(shared.vm_identity, redefined, 3), None);
    assert_eq!(
        shared.natives.jni_global_refs.lock().count(),
        refs,
        "a refused publication releases the global ref it made"
    );

    let now = redefinitions();
    ldc_slot_memo_fill(&shared, redefined, 3, literal, now);
    ldc_slot_memo_fill(&shared, untouched, 3, literal, now);
    if redefinitions() != now {
        return; // another test redefined a class meanwhile
    }
    assert!(slot_of(redefined).is_some() && slot_of(untouched).is_some());

    retire_ldc_slots_of_class(&shared, redefined);
    assert_eq!(
        slot_of(redefined),
        None,
        "the redefined class's slot is retired"
    );
    assert!(slot_of(untouched).is_some(), "another class's slot is kept");
    assert_eq!(
        ldc_slot_memo_probe(shared.vm_identity, untouched, 3),
        Some(literal.as_ptr() as i64),
        "and still answers"
    );
}
