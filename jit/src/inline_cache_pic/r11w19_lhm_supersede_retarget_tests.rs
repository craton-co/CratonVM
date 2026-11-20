//! Round 11 wave 19, lane lhm
//! (`r11w18-hashmap-inline-caches-keep-a-superseded-callee`): a publication
//! that supersedes a callee body retires the inline caches that still
//! publish the old one, so their next call re-resolves to the new body.
use super::*;
use crate::{ExecutableBuffer, JitCache};
use std::sync::atomic::Ordering;

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn holder_body() -> CompiledMethod {
    let mut cm = ret_body();
    cm._jit_mic_slots.push(Box::new(JitMICSlot::new_at(3)));
    cm._jit_mic_slots.push(Box::new(JitMICSlot::new_at(9)));
    cm._jit_pic_slots.push(Box::new(JitPICSlot::new_at(3)));
    cm
}

#[test]
fn a_supersede_retires_the_mic_and_pic_ways_that_publish_the_old_body() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(1);
    let holder_class: Arc<str> = Arc::from("R11w19LhmHolder");
    let callee_class: Arc<str> = Arc::from("R11w19LhmCallee");
    let h_name: Arc<str> = Arc::from("h");
    let m_name: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    cache.put(
        holder_class.clone(),
        h_name.clone(),
        desc.clone(),
        cid,
        holder_body(),
    );
    cache.put(
        callee_class.clone(),
        m_name.clone(),
        desc.clone(),
        cid,
        ret_body(),
    );
    let holder = cache
        .get(&holder_class, &h_name, &desc, cid)
        .expect("holder published");
    let first = cache
        .get(&callee_class, &m_name, &desc, cid)
        .expect("callee published");
    let mic = &holder._jit_mic_slots[0];
    let pic = &holder._jit_pic_slots[0];
    assert!(mic.holder.get().is_some(), "put binds the holder's MIC");
    assert!(pic.holder.get().is_some(), "put binds the holder's PIC");

    let first_entry = first.entry_ptr() as u64;
    mic.update(7, "Seven", first_entry, false, false);
    pic.install(7, "Seven", first_entry, false, false);
    assert_eq!(mic.cached_entry().0, first_entry);
    assert_eq!(pic.lookup(7), Some((first_entry, false)));
    assert_eq!(
        first.ic_holders.lock().len(),
        2,
        "one record for the MIC, one for the PIC"
    );

    // The supersede.
    cache.put(
        callee_class.clone(),
        m_name.clone(),
        desc.clone(),
        cid,
        ret_body(),
    );
    assert!(first.superseded.load(Ordering::Acquire));
    assert_eq!(
        mic.cached_entry().0,
        0,
        "the MIC still publishes the superseded body"
    );
    assert_eq!(
        mic.cached_class_id.load(Ordering::Acquire),
        7,
        "the MIC keeps its receiver, so its next miss re-resolves in place"
    );
    assert!(
        pic.lookup(7).is_none(),
        "a PIC way still publishes the superseded body"
    );
    assert!(
        pic.lookup_megamorphic(7).is_none(),
        "a hashed-table way still publishes the superseded body"
    );
    assert!(
        first.ic_holders.lock().is_empty(),
        "the walk consumed the record"
    );

    // The next resolution publishes the current body into the same MIC.
    let second = cache
        .get(&callee_class, &m_name, &desc, cid)
        .expect("replacement published");
    let second_entry = second.entry_ptr() as u64;
    assert_ne!(second_entry, first_entry);
    mic.update(7, "Seven", second_entry, false, false);
    assert_eq!(mic.cached_entry().0, second_entry);
    assert_eq!(second.ic_holders.lock().len(), 1);

    // A publication of the superseded body that lands AFTER the walk
    // withdraws itself.
    let late = &holder._jit_mic_slots[1];
    late.update(8, "Eight", first_entry, false, false);
    assert_eq!(
        late.cached_entry().0,
        0,
        "a publication of an already-superseded body must not stick"
    );
}

#[test]
fn an_unbound_slot_records_nothing() {
    let callee = ret_body();
    let mic = JitMICSlot::new();
    // No owner resolves for a body that was never registered, and an
    // unbound slot never records: nothing lands on the callee either way.
    mic.update(7, "Seven", callee.entry_ptr() as u64, false, false);
    assert!(callee.ic_holders.lock().is_empty());
    assert!(mic.holder.get().is_none());
}
