use super::*;

fn bitmap() -> Option<ZObjectStartBits> {
    Some(ZObjectStartBits::labelled(0x1000_0000, 1 << 20, "skip"))
}


#[test]
fn zt31zc11_members_answer_until_the_clear() {
    let s = ZSkipBits::new(true);
    let (a, b, c) = (0x1000_0010, 0x1000_0040, 0x1000_0080);
    assert_eq!(s.answer(a), None, "no cycle: the filters decide");
    assert_eq!(s.fill([a, b, a, 0], false, &[], bitmap), Some(vec![a, b]));
    assert_eq!(s.answer(a), Some(true));
    assert_eq!(s.answer(b), Some(true));
    assert_eq!(s.answer(c), Some(false));
    // A refresh while armed only adds.
    assert_eq!(s.fill([c], true, &[], bitmap), Some(vec![a, b, c]));
    assert_eq!(s.answer(a), Some(true));
    assert_eq!(s.answer(c), Some(true));
    let r = s.report();
    assert_eq!((r.fills, r.members, r.last_members), (2, 3, 3), "deduplicated");
    s.clear();
    assert_eq!(s.answer(a), None, "the filters decide again");
    let bits = s.bitmap().expect("built");
    assert!(!bits.contains(a) && !bits.contains(b) && !bits.contains(c));
    assert_eq!(s.report().cleared, 3);
    // A fresh cycle starts empty.
    assert_eq!(s.fill([b], false, &[], bitmap), Some(vec![b]));
    assert_eq!(s.answer(a), Some(false));
    assert_eq!(s.answer(b), Some(true));
}

/// zt t31/zc11: a refresh withdraws exactly the listed members it does not
/// name again (the soft rows at the mark end); every other member keeps its
/// bit, a withdrawn one named again stays, and a fresh cycle ignores the
/// list (it starts from the new set).
#[test]
fn zt31zc11_a_refresh_withdraws_the_listed_members_only() {
    let s = ZSkipBits::new(true);
    let (weak, soft, soft_again, late) = (0x1000_0010, 0x1000_0040, 0x1000_0080, 0x1000_00c0);
    assert_eq!(
        s.fill([weak, soft, soft_again], false, &[], bitmap),
        Some(vec![weak, soft, soft_again])
    );
    let members = s.fill([weak, soft_again, late], true, &[soft, soft_again], bitmap);
    assert_eq!(members, Some(vec![weak, soft_again, late]));
    assert_eq!(s.answer(soft), Some(false), "withdrawn: its referent is traced now");
    assert_eq!(s.answer(soft_again), Some(true), "named again: still hidden");
    assert_eq!(s.answer(weak), Some(true), "not withdrawn: a refresh only adds");
    assert_eq!(s.answer(late), Some(true));
    assert_eq!(s.report().withdrawn, 1);
    s.clear();
    assert_eq!(s.report().cleared, 3, "the clear walks the members left");
    assert_eq!(s.fill([weak], false, &[weak], bitmap), Some(vec![weak]));
    assert_eq!(s.answer(weak), Some(true), "a fresh cycle withdraws nothing");
}

/// On a real heap: the mark's member answer is the same through the
/// bitmap and through the hash-set path (its kill), the bitmap builds no
/// hash set, and the cycle's end clears every member bit.
#[test]
fn zt31zc11_the_mark_answers_the_same_with_and_without_the_bitmap() {
    use cratonvm_gc_core::collector::GarbageCollector;
    use crate::zgc::ZgcRealHeap;
    use cratonvm_types::ClassId;
    for on in [true, false] {
        let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        heap.counters.skipbits.set_enabled_for_test(on);
        let member = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
        let same_class = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
        let other = heap.alloc_object(ClassId::new(63), 2).as_ptr() as usize;
        heap.set_stw_vm_reference_skip(vec![member, member]);
        let snap = heap.begin_concurrent_mark_cycle();
        assert_eq!(snap.len(), 1, "on={on}: one member");
        assert!(snap.contains(&member));
        assert_eq!(heap.counters.skipbits.answer(member), on.then_some(true));
        assert_eq!(heap.mark_skip_index(member), Some(0), "on={on}: the member");
        assert_eq!(heap.mark_skip_index(same_class), None, "on={on}");
        assert_eq!(heap.mark_skip_index(other), None, "on={on}");
        assert_eq!(heap.mark_skip_index_classed(other, Some(63)), None);
        heap.end_concurrent_mark_cycle();
        heap.clear_stw_vm_reference_skip();
        assert_eq!(heap.counters.skipbits.answer(member), None);
        if on {
            let bits = heap.counters.skipbits.bitmap().expect("built");
            assert!(!bits.contains(member), "cleared with the arm");
            assert_eq!(heap.skip_bits_report().cleared, 1);
        }
    }
}

/// zt t31/zc11: a soft row is hidden for the concurrent phase only. The
/// mark end takes it out of the set (`finish_concurrent_mark`), so a soft
/// `Reference` the closing drain first reaches shows its referent -- with
/// the bitmap as with the hash set. The bitmap kept the bit (a refresh
/// only added), so the referent stayed hidden, was never marked, and was
/// swept under a live `SoftReference`.
#[test]
fn zt31zc11_the_mark_end_refresh_shows_a_soft_referent() {
    use super::super::mark::ZMarkContext;
    use cratonvm_gc_core::collector::GarbageCollector;
    use crate::zgc::ZgcRealHeap;
    use cratonvm_types::{ClassId, Value};
    for on in [true, false] {
        let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        heap.counters.skipbits.set_enabled_for_test(on);
        let soft = heap.alloc_object(ClassId::new(1), 2);
        let referent = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(soft, 0, Value::Object(Some(referent)));
        let shows_referent = |h: &ZgcRealHeap| {
            let mut v: Vec<u64> = Vec::new();
            h.visit_refs(soft.as_ptr() as u64, &mut |c| v.push(c));
            v.contains(&(referent.as_ptr() as u64))
        };
        heap.set_concurrent_vm_soft_reference_skip(vec![soft.as_ptr() as usize]);
        heap.begin_concurrent_mark_cycle();
        assert!(!shows_referent(&heap), "on={on}: hidden in the phase");
        // The mark end: the soft rows leave, the set is refreshed.
        let rows = std::mem::take(&mut *heap.counters.vm_conc_soft_skip.lock());
        heap.begin_concurrent_mark_cycle_withdrawing(&rows);
        assert!(shows_referent(&heap), "on={on}: traced from the mark end on");
        heap.end_concurrent_mark_cycle();
        assert_eq!(heap.skip_bits_report().withdrawn, usize::from(on));
    }
}

#[test]
fn zt31zc11_off_or_no_bitmap_keeps_the_hash_set_path() {
    let off = ZSkipBits::new(false);
    assert_eq!(off.fill([0x1000_0010], false, &[], bitmap), None);
    assert_eq!(off.answer(0x1000_0010), None);
    let none = ZSkipBits::new(true);
    assert_eq!(none.fill([0x1000_0010], false, &[], || None), None);
    assert_eq!(none.answer(0x1000_0010), None);
    // An armed cycle that answers from the hash set is not switched over by
    // a refresh.
    let s = ZSkipBits::new(true);
    assert_eq!(s.fill([0x1000_0010], true, &[], bitmap), None);
    assert_eq!(s.answer(0x1000_0010), None);
}
