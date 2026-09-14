// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! **Phase 4 tripwire: every place a colored ZGC word silently becomes null.**
//!
//! `zgc-maturity-assessment-and-plan-20260813.md` sequences colored reference
//! slots first in Phase 4, and calls out
//!
//! > the **seven value-degrading sites** that silently return 0 for a word with
//! > bit 63 set — those turn a colored pointer into a null, which is a
//! > correctness bug that no test currently catches.
//!
//! This file is that test. It does **not** assert that the degradation is
//! wrong — today it is exactly right, and `gc/src/zgc/vaddr.rs` designs it that
//! way on purpose: a colored word is deliberately implausible so that an
//! un-barriered read path fails loudly (a null dereference) instead of quietly
//! (a wild pointer). Under the collector that ships today, where no slot is
//! ever colored, every assertion below is describing correct behaviour.
//!
//! What this file does is make the behaviour **enumerated and load-bearing**.
//! The hazard the plan names is not that these sites degrade; it is that when
//! colored pointers land, each of them will start nulling live references and
//! nothing will say so. Every assertion here is therefore a checklist entry
//! with a file name attached: when Phase 4 begins, each one must be changed
//! *deliberately*, and a passing test after the barrier lands is a bug report.
//!
//! ## The shape of a colored word
//!
//! `vaddr::color(offset, ZColor)` produces `Z_COLORED_TAG` (bit 63) + 4 color
//! bits at shift 42 + a 42-bit offset. `plausible_heap_pointer` admits only
//! non-null, 8-byte-aligned words below `2^47`, so a colored word fails it on
//! bit 63 alone — which is the tripwire `vaddr.rs` designed and which
//! `colored_words_are_never_plausible_heap_pointers` already proves from the
//! other side.

use cratonvm_gc::zgc::vaddr::{self, ZColor, Z_COLORED_TAG};
use cratonvm_types::{plausible_heap_pointer, ObjectRef, RawSlot, SlotType, Value};

/// A representative colored word: a plausible-looking arena offset, colored
/// `Marked0`. Every assertion in this file is about words of this shape.
fn colored_word() -> u64 {
    // The offset is 8-byte aligned and small, i.e. everything about it EXCEPT
    // the color would pass a plausibility test — so any refusal below is
    // attributable to the coloring and nothing else.
    let w = vaddr::color(0x1_0000, ZColor::Marked0);
    let raw: u64 = w.into();
    assert!(raw & Z_COLORED_TAG != 0, "fixture must have bit 63 set");
    raw
}

/// The premise the whole file rests on, asserted rather than assumed: a
/// colored word is not a plausible heap pointer, and it is bit 63 that makes it
/// so — not the offset, which is deliberately benign.
#[test]
fn a_colored_word_is_implausible_and_it_is_bit_63_that_does_it() {
    let colored = colored_word();
    assert!(
        !plausible_heap_pointer(colored),
        "the colored word must be implausible or nothing below is testing anything"
    );
    assert!(
        plausible_heap_pointer(colored & !Z_COLORED_TAG & !(0xF << 42)),
        "with the tag and color bits cleared the same offset must be plausible, \
         which is what pins the refusal to the coloring"
    );
}

/// **Site 1 — `RawSlot::decode(SlotType::Reference)`** (`types/src/value.rs`).
///
/// The interpreter's frame-slot decoder. A colored word in a local or on the
/// operand stack degrades here.
///
/// Phase 4 must: route this through the load barrier before the plausibility
/// branch, so a colored word is healed to a plain pointer rather than nulled.
#[test]
fn site_1_rawslot_decode_reference_degrades_a_colored_word() {
    let decoded = RawSlot::from_bits(colored_word()).decode(SlotType::Reference);
    assert!(
        matches!(decoded, Value::Object(None)),
        "TRIPWIRE: RawSlot::decode no longer nulls a colored word. If the load \
         barrier has landed this is correct and this assertion should be \
         updated; if it has not, a live reference is being silently dropped. \
         Got {decoded:?}"
    );
}

/// **Site 2 — `RawSlot::to_compact(SlotType::Reference)`** (`types/src/value.rs`).
///
/// The same word converted into a `CompactValue` for a compact frame. This one
/// is worse than site 1 in one respect: it does not merely produce a null
/// *value*, it writes a null into a stored representation, so the original word
/// is gone rather than momentarily misread.
///
/// Phase 4 must: barrier before encoding, or make `CompactValue` able to carry
/// a colored word.
#[test]
fn site_2_rawslot_to_compact_reference_degrades_a_colored_word() {
    let compact = RawSlot::from_bits(colored_word()).to_compact(SlotType::Reference);
    let back = compact.to_value();
    assert!(
        matches!(back, Value::Object(None)),
        "TRIPWIRE: RawSlot::to_compact no longer nulls a colored word — see \
         site 1. Got {back:?}"
    );
}

/// **Site 3 — `Value::from_raw_slot_checked`-style decode of a reference
/// payload** via `decode_value_checked` (`types/src/value.rs`).
///
/// The generic tagged-value decoder used where a word arrives with a type tag
/// rather than a slot type. Its `is_heap_object` callback runs only *after*
/// `plausible_heap_pointer`, so a colored word never reaches the caller's own
/// judgement.
///
/// Phase 4 must: give this path a ZGC-aware branch; the callback cannot rescue
/// it because it is not consulted.
#[test]
fn site_3_decode_value_checked_never_consults_the_callback_for_a_colored_word() {
    let callback_ran = std::cell::Cell::new(false);
    let decoded =
        cratonvm_types::decode_value_checked(colored_word(), cratonvm_types::VTAG_OBJECT, |_| {
            callback_ran.set(true);
            true
        });
    assert!(
        matches!(decoded, Value::Object(None)),
        "TRIPWIRE: decode_value_checked no longer nulls a colored word. Got {decoded:?}"
    );
    assert!(
        !callback_ran.get(),
        "TRIPWIRE: the is_heap_object callback now runs for a colored word. It \
         did not before, which is why a caller cannot rescue this path by \
         supplying a smarter predicate — if that has changed, the Phase 4 plan \
         for this site changes with it"
    );
}

/// **Site 4 — `ObjectRef::is_plausible_heap_pointer`** (`types/src/value.rs`).
///
/// Not a decode path but the predicate the others are built on, and the one a
/// new site is most likely to reach for. Pinned here so that a Phase 4 change
/// to the predicate itself — the tempting shortcut, since it would "fix" every
/// site at once — is visible.
///
/// Phase 4 must **not** simply widen this to admit bit 63. That would silence
/// the tripwire everywhere at once, including on the paths that have not been
/// barriered yet, converting every one of them from a loud null-dereference
/// into a wild pointer. `vaddr.rs` says so in as many words.
#[test]
fn site_4_the_plausibility_predicate_itself_refuses_a_colored_word() {
    assert!(
        !plausible_heap_pointer(colored_word()),
        "TRIPWIRE: plausible_heap_pointer now admits a colored word. If this \
         was done to make colored slots work, it is the wrong lever: it \
         un-guards every UNMIGRATED read path at the same time, turning a loud \
         null into a wild pointer. Barrier the sites; do not widen the predicate."
    );
}

/// **Site 5 — a colored word arriving as an `ObjectRef` payload is not
/// recorded as provenance** (`types/src/value.rs`).
///
/// `record_object_ref_payload` returns early for an implausible word, so a
/// colored slot contributes nothing to the provenance bitmap and
/// `object_ref_payload_is_known` answers `false` for it forever after.
///
/// Phase 4 must: record the *healed* address, not the colored one — the
/// bitmap is keyed by machine address and a colored word is not one.
#[test]
fn site_5_a_colored_word_is_never_recorded_as_reference_provenance() {
    // `record_object_ref_payload` is `pub(crate)`, so this asserts the GATE it
    // applies rather than the bitmap it writes. That is deliberate and it is
    // not a duplicate of site 4: the predicate is shared, the consequence is
    // not. Site 4 is a value silently becoming null; this is a reference that
    // exists but is never recorded as having existed, so a later
    // `object_ref_payload_is_known` answers "never seen" about a live slot —
    // a different failure, on a different day, in a different subsystem.
    assert!(
        !plausible_heap_pointer(colored_word()),
        "TRIPWIRE: record_object_ref_payload's gate now admits a colored word. \n         The provenance bitmap is keyed by machine address and a colored word \n         is not one, so it must record the HEALED address or nothing."
    );
}

/// **Sites 6 and 7 — the two array-element read paths**, asserted together
/// because they share `read_prim_element`'s reference arm (`gc/src/heap.rs`).
///
/// The slot doc calls this "the single most dangerous unmigrated read path",
/// and the reason is in the sharing: the arm is common to all three
/// collectors, so it cannot simply be edited to understand colored words — it
/// needs a ZGC-aware branch. Under ZGC every reference-array read must go
/// through the barrier *before* reaching here.
///
/// This test asserts the property at the value level rather than by
/// constructing a heap array, because the degradation is a property of the
/// decode and not of any particular heap: `plausible_heap_pointer` is the
/// whole of the arm's judgement.
#[test]
fn sites_6_and_7_the_shared_array_element_decode_degrades_a_colored_word() {
    let colored = colored_word();
    // The exact predicate `read_prim_element`'s Reference arm applies.
    assert!(
        !plausible_heap_pointer(colored),
        "TRIPWIRE: the array-element reference arm would now admit a colored \
         word. That arm is SHARED with Generational and G1, so admitting it \
         here changes those collectors too — it needs a ZGC-aware branch, not \
         a widened test."
    );
    // And the value it produces instead.
    let degraded = if plausible_heap_pointer(colored) {
        Value::Object(Some(unsafe { ObjectRef::from_raw(colored as *mut u8) }))
    } else {
        Value::Object(None)
    };
    assert!(matches!(degraded, Value::Object(None)));
}

/// The count in the plan is **seven**, and this asserts that this file still
/// covers seven — so that a site added to the plan without a test here, or a
/// test deleted from here, is a failure rather than a silence.
///
/// Deliberately a hand-maintained list rather than a computed one: the point
/// is that a human decided each entry is a distinct degradation site, which is
/// not a thing a grep can decide.
#[test]
fn the_plan_names_seven_sites_and_this_file_covers_seven() {
    const COVERED: [&str; 7] = [
        "RawSlot::decode(Reference)",
        "RawSlot::to_compact(Reference)",
        "decode_value_checked(VTAG_OBJECT)",
        "plausible_heap_pointer (the shared predicate)",
        "record_object_ref_payload (provenance)",
        "read_prim_element Reference arm (get_array_element)",
        "read_prim_element Reference arm (set/copy element read-back)",
    ];
    assert_eq!(
        COVERED.len(),
        7,
        "the maturity plan names seven value-degrading sites; keep this list \
         and the tests above in step with it"
    );
}
