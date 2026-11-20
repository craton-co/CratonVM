use super::*;
use crate::gen_heap::GenerationalHeap;
use cratonvm_types::narrow_oop::write_ref_slot;
use cratonvm_types::ClassId;
use rustc_hash::FxHashSet;

/// A reference array's element: the value read back, and the card owed is
/// the element's on an element-precise table and the header's otherwise;
/// a null element reads 0.
#[test]
fn gci_i10gr_a_card_pair_reads_its_value_and_owes_the_right_card() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let holder = heap.alloc_array(ClassId::new(9), ArrayElementType::Reference, 4);
    let target = heap.alloc_object(ClassId::new(7), 4);
    let h = holder.as_ptr() as usize;
    let t = target.as_ptr() as usize;
    let off2 = ARRAY_DATA_OFFSET + 2 * ref_element_size();
    // SAFETY: element 2 of a freshly allocated 4-element reference array.
    unsafe { write_ref_slot(holder.as_ptr().add(off2), t as u64) };
    // SAFETY: `holder` is live and both slots are in bounds.
    unsafe {
        assert_eq!(card_pair_value_and_card(holder, 2, false), (t, h));
        assert_eq!(card_pair_value_and_card(holder, 2, true), (t, h + off2));
        assert_eq!(card_pair_value_and_card(holder, 1, false).0, 0, "a null element reads 0");
        assert_eq!(card_guard_redirty_addrs(&[(holder, 2, 0), (holder, 1, 0)], true), vec![
            h + off2,
            h + ARRAY_DATA_OFFSET + ref_element_size()
        ]);
    }
}

/// The candidates: a recorded start, a null and an out-of-space value are
/// not offenders; an interior card value and an interior overlay value
/// are, the first one named; a misaligned one is counted but not passed
/// on as an `ObjectRef`.
#[test]
fn gci_i10gr_only_unrecorded_from_space_values_are_candidates() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let holder = heap.alloc_array(ClassId::new(9), ArrayElementType::Reference, 4);
    let y = heap.alloc_object(ClassId::new(7), 8);
    let base = y.as_ptr() as usize;
    let interior = base + 16;
    let esz = ref_element_size();
    // SAFETY: elements 0..=2 of a freshly allocated 4-element array.
    unsafe {
        write_ref_slot(holder.as_ptr().add(ARRAY_DATA_OFFSET), base as u64);
        write_ref_slot(holder.as_ptr().add(ARRAY_DATA_OFFSET + esz), interior as u64);
    }
    let in_from = |a: usize| a >= base && a < base + 4096;
    let is_start = |a: usize| a == base;
    let pairs = [(holder, 0, 0), (holder, 1, 0), (holder, 3, 0)];
    // SAFETY: every pair names an in-bounds element of a live array.
    let found = unsafe {
        card_guard_candidates(&pairs, &[base + 24, base + 5, 0x10, base], in_from, is_start, false)
    };
    assert_eq!(found.card_offenders, 1, "only the interior element");
    assert_eq!(found.overlay_offenders, 2, "base+24 and the misaligned base+5");
    assert_eq!(found.misaligned, 1);
    let vals: Vec<usize> = found.values.iter().map(|r| r.as_ptr() as usize).collect();
    assert_eq!(vals, vec![interior, base + 24]);
    let first = found.first.expect("an offender");
    assert_eq!(
        (first.source, first.holder, first.ref_array, first.slot, first.value),
        ("dirty-card", holder.as_ptr() as usize, true, 1, interior),
    );
}

/// End to end on the decision: the candidates refuse a cycle through the
/// root guard's latch (census `[1, 0, 1]`), the same lowest value next
/// time fails open, an excluded value does not refuse, and a clean card
/// set refuses nothing.
#[test]
fn gci_i10gr_an_unrecorded_card_value_refuses_once_through_the_latch() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let holder = heap.alloc_array(ClassId::new(9), ArrayElementType::Reference, 2);
    let y = heap.alloc_object(ClassId::new(7), 8);
    let base = y.as_ptr() as usize;
    let interior = base + 16;
    // SAFETY: element 0 of a freshly allocated 2-element array.
    unsafe { write_ref_slot(holder.as_ptr().add(ARRAY_DATA_OFFSET), interior as u64) };
    let from = heap.lock_young_from();
    let starts = ObjectStartBits::new(from.base_ptr() as usize, from.used());
    assert!(starts.insert(base));
    assert!(starts.insert(holder.as_ptr() as usize));
    let in_from = |a: usize| from.contains(a as *const u8);
    let is_start = |a: usize| starts.contains(a);
    // SAFETY: the pair names an in-bounds element of a live array.
    let found =
        unsafe { card_guard_candidates(&[(holder, 0, 0)], &[], in_from, is_start, false) };
    assert_eq!(found.values.len(), 1);
    let none = FxHashSet::default();
    assert!(heap.unrecorded_young_root_refuses_cycle(&found.values, &from, &starts, &none));
    assert_eq!(heap.unrecorded_young_root_census(), [1, 0, 1]);
    assert!(
        !heap.unrecorded_young_root_refuses_cycle(&found.values, &from, &starts, &none),
        "the same lowest address again fails open (one refusal per address)",
    );
    assert_eq!(heap.unrecorded_young_root_census(), [1, 1, 2]);
    let exclude: FxHashSet<usize> = [interior].into_iter().collect();
    assert!(!heap.unrecorded_young_root_refuses_cycle(&found.values, &from, &starts, &exclude));
    // A recorded value: no candidate at all.
    // SAFETY: as above.
    unsafe { write_ref_slot(holder.as_ptr().add(ARRAY_DATA_OFFSET), base as u64) };
    // SAFETY: as above.
    let clean =
        unsafe { card_guard_candidates(&[(holder, 0, 0)], &[], in_from, is_start, false) };
    assert!(clean.values.is_empty() && clean.first.is_none());
}

/// The engagement slot (census slot 8) starts at zero and is read by its
/// own getter, apart from the root guard's census.
#[test]
fn gci_i10gr_the_cards_guarded_census_starts_at_zero() {
    let heap = GenerationalHeap::with_sizes(1024 * 1024, 1024 * 1024);
    assert_eq!(heap.unrecorded_young_root_cards_guarded_cycles(), 0);
    heap.unrecorded_root_census[8].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(heap.unrecorded_young_root_cards_guarded_cycles(), 1);
    assert_eq!(heap.unrecorded_young_root_census(), [0, 0, 0], "slot 8 is not in the refusal census");
    assert_eq!(heap.unrecorded_young_root_ledger_guarded_cycles(), 0);    }