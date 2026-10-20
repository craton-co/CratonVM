// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i10/gr (2026-10-13) — residual (a) of
//! `docs/internal/gc/gengc-r5w6-sizer10-refused-young-root-dangles-RETIRED-RULING-20261018.md`:
//! the unrecorded-young-root guard extended to the DIRTY-CARD slot values and
//! the OVERLAY row values (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`, opt-in).
//!
//! # What was open
//!
//! `GenerationalHeap::collect_garbage_inner_with_pins` refuses a moving young
//! cycle when a ROOT names from-space at an address the cycle's object-start
//! walk did not record (gcd d2/h; the ledger arm gcf f4/y). Two other kinds of
//! value reach `forward_object_impl` the same way and were never judged:
//!
//! * the young values of the old→young slots the dirty-card scan reports
//!   (`seed_dirty_card_roots` forwards them), and
//! * the young values of the external overlay rows (`seed_overlay_roots`).
//!
//! An unrecorded one is refused there ("not-an-object-start"), the slot or row
//! keeps naming from-space, and from-space is reset: the holder dangles. The
//! root guard runs BEFORE the card take because a refusal after it loses the
//! consumed cards' edges; that was the stated reason (a) was left open.
//!
//! # What this adds
//!
//! The values are read right AFTER the take (the take is what produces the
//! `(holder, slot)` pairs), on exactly the cycles the root guard ran on, with
//! the same exclusion set and the same one-refusal latch
//! (`unrecorded_young_root_refuses_cycle`). A refusal first RE-DIRTIES every
//! card the take consumed that held a young value -- the non-moving sweep's own
//! re-dirty rule (`seed_one_card_root`: the element's card on an
//! element-precise table, the holder's header card otherwise) -- plus the
//! walk-gap cards, and then takes the refused-walk exit. A consumed card that
//! held no young value is not restored: no old→young edge lives behind it, and
//! the moving cycle it replaces would have wiped it with `clear_all()` anyway.
//! So a refused cycle leaves the next one exactly the young edges this one
//! would have seen.
//!
//! The producer hunt ((1) of the page): each offender is printed once per
//! occurrence (rate-limited on the guard's own census) as
//! `[unrecorded-young-card-root] source=<dirty-card|overlay> holder=0x..
//! holder_class_id=.. ref_array=.. slot=.. value=0x.. nearest_recorded_start=..`,
//! so a run that meets one names the holder whose store produced the value.
//! Printed through `tracing::warn!` like the root guard's line (the launcher's
//! floor is ERROR: rows read it with `RUST_LOG=warn`).

use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size};
use cratonvm_types::{ObjectRef, Value};

use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE, SLOT_SIZE};
use crate::young_mark::ObjectStartBits;

/// `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS` -- opt-in (default OFF), read once
/// per moving cycle on which the unrecorded-root guard ran. Opt-in because it
/// adds one slot read per dirty-card pair (the copy reads the same slots again)
/// and one overlay registry pass per guarded cycle, to catch a case no run has
/// shown yet.
pub(super) fn unrecorded_root_guard_cards_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS")
}

/// The first offender a guarded cycle met, for the producer line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CardGuardOffender {
    /// `"dirty-card"` or `"overlay"`.
    pub(super) source: &'static str,
    /// The old holder (0 for an overlay row: the registry does not name it).
    pub(super) holder: usize,
    /// The holder's class id (0 for an overlay row).
    pub(super) holder_class_id: u32,
    /// Whether the holder is a reference array (then `slot` is an element index).
    pub(super) ref_array: bool,
    /// The pair's slot as the card scan encodes it (element index, compact byte
    /// offset, or `Value` cell index).
    pub(super) slot: usize,
    /// The unrecorded from-space value.
    pub(super) value: usize,
}

/// What [`card_guard_candidates`] found: the from-space values that are NOT
/// recorded object starts, as roots for `unrecorded_young_root_refuses_cycle`.
#[derive(Default)]
pub(super) struct CardGuardFound {
    pub(super) values: Vec<ObjectRef>,
    pub(super) card_offenders: usize,
    pub(super) overlay_offenders: usize,
    /// From-space values off the 8-byte grid. Not passed on (an `ObjectRef`
    /// must be aligned); counted and named in the producer line.
    pub(super) misaligned: usize,
    pub(super) first: Option<CardGuardOffender>,
}

/// One dirty-card pair's slot value (0 = null or not a reference) and the
/// address whose card a re-dirty owes, read-only. The three encodings are
/// `seed_dirty_card_roots`' / `seed_one_card_root`'s exactly.
///
/// # Safety
/// `(old_obj, slot_idx)` is a pair the dirty-card scan produced this pause: a
/// live old-generation object and an in-bounds reference slot of it; the world
/// is stopped.
pub(super) unsafe fn card_pair_value_and_card(
    old_obj: ObjectRef,
    slot_idx: usize,
    precise_array_cards: bool,
) -> (usize, usize) {
    let base = old_obj.as_ptr() as usize;
    // SAFETY: the caller's contract -- a live old-gen object's header.
    let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
    let is_ref_array =
        header.kind() == ObjectKind::Array && header.element_type() == ArrayElementType::Reference;
    if is_ref_array {
        let offset = ARRAY_DATA_OFFSET + slot_idx * ref_element_size();
        // SAFETY: `slot_idx` is within the array's bounds (card scan).
        let raw = unsafe { read_ref_slot(old_obj.as_ptr().add(offset)) };
        let card = if precise_array_cards { base.wrapping_add(offset) } else { base };
        (raw as usize, card)
    } else if crate::is_compact_object(header) {
        // SAFETY: `slot_idx` is the byte offset of an in-bounds 8-byte slot.
        let raw = unsafe { read_ref_slot(old_obj.as_ptr().add(slot_idx)) };
        (raw as usize, base)
    } else {
        // SAFETY: `slot_idx` is within `num_slots` (card scan): a `Value` cell.
        let value =
            unsafe { std::ptr::read(old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) as *const Value) };
        match value {
            Value::Object(Some(r)) => (r.as_ptr() as usize, base),
            _ => (0, base),
        }
    }
}

/// The guard's input: every dirty-card pair value and every overlay value
/// that `in_from` places in from-space and `is_start` does not record.
///
/// # Safety
/// As [`card_pair_value_and_card`], for every pair of `extra_roots`.
pub(super) unsafe fn card_guard_candidates(
    extra_roots: &[(ObjectRef, usize, usize)],
    overlay_values: &[usize],
    in_from: impl Fn(usize) -> bool,
    is_start: impl Fn(usize) -> bool,
    precise_array_cards: bool,
) -> CardGuardFound {
    let mut found = CardGuardFound::default();
    let take = |found: &mut CardGuardFound, offender: CardGuardOffender| {
        let v = offender.value;
        if v == 0 || !in_from(v) || is_start(v) {
            return;
        }
        if offender.source == "overlay" {
            found.overlay_offenders += 1;
        } else {
            found.card_offenders += 1;
        }
        if found.first.is_none() {
            found.first = Some(offender);
        }
        if v & 7 != 0 {
            found.misaligned += 1;
            return;
        }
        // SAFETY: non-null and 8-aligned (checked above); never dereferenced
        // as an object by the guard's decision.
        found.values.push(unsafe { ObjectRef::from_raw(v as *mut u8) });
    };
    for &(old_obj, slot_idx, _) in extra_roots {
        // SAFETY: the caller's contract.
        let (value, _) = unsafe { card_pair_value_and_card(old_obj, slot_idx, precise_array_cards) };
        if value == 0 {
            continue;
        }
        // SAFETY: the caller's contract -- a live old-gen object's header.
        let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
        take(
            &mut found,
            CardGuardOffender {
                source: "dirty-card",
                holder: old_obj.as_ptr() as usize,
                holder_class_id: header.class_id.as_u32(),
                ref_array: header.kind() == ObjectKind::Array
                    && header.element_type() == ArrayElementType::Reference,
                slot: slot_idx,
                value,
            },
        );
    }
    for &value in overlay_values {
        take(
            &mut found,
            CardGuardOffender {
                source: "overlay",
                holder: 0,
                holder_class_id: 0,
                ref_array: false,
                slot: 0,
                value,
            },
        );
    }
    found
}

/// The card addresses a refused cycle must re-dirty: one per dirty-card pair,
/// by the non-moving sweep's rule (see the module doc).
///
/// # Safety
/// As [`card_pair_value_and_card`], for every pair of `extra_roots`.
pub(super) unsafe fn card_guard_redirty_addrs(
    extra_roots: &[(ObjectRef, usize, usize)],
    precise_array_cards: bool,
) -> Vec<usize> {
    extra_roots
        .iter()
        // SAFETY: the caller's contract.
        .map(|&(o, s, _)| unsafe { card_pair_value_and_card(o, s, precise_array_cards) }.1)
        .collect()
}

/// The producer line ((1) of the page), rate-limited on the guard's own
/// census: `seen_before` is `cycles_refused + failed_open` read BEFORE this
/// cycle's decision; the first eight and every power of two print.
pub(super) fn note_card_guard_offenders(
    found: &CardGuardFound,
    refused: bool,
    seen_before: u64,
    starts: &ObjectStartBits,
) {
    let seen = seen_before.saturating_add(1);
    if !(seen <= 8 || seen.is_power_of_two()) {
        return;
    }
    let Some(o) = found.first else {
        return;
    };
    let near = starts
        .nearest_start_at_or_below(o.value)
        .map(|a| format!("0x{a:x}"))
        .unwrap_or_else(|| "none".into());
    tracing::warn!(
        "[unrecorded-young-card-root] source={} holder=0x{:x} holder_class_id={} ref_array={} \
         slot={} value=0x{:x} nearest_recorded_start={near} card_offenders={} \
         overlay_offenders={} misaligned={} refused={refused} (occurrence #{seen}; see \
         gengc-r5w6-sizer10-refused-young-root-dangles (a)/(1))",
        o.source,
        o.holder,
        o.holder_class_id,
        o.ref_array,
        o.slot,
        o.value,
        found.card_offenders,
        found.overlay_offenders,
        found.misaligned,
    );
}

#[cfg(test)]
mod tests {
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
        assert_eq!(heap.unrecorded_young_root_ledger_guarded_cycles(), 0);
    }

    /// The switch is opt-in: off unless set.
    #[test]
    fn gci_i10gr_the_cards_guard_is_opt_in() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS", None)],
            || assert!(!unrecorded_root_guard_cards_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS", Some("1"))],
            || assert!(unrecorded_root_guard_cards_enabled()),
        );
    }
}
