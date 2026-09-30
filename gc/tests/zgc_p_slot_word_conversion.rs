// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane-P (wave 4): `vaddr::root_address_for_slot_word`, pinned away from the
//! adapter that used to be its only caller.
//!
//! # Why this file exists
//!
//! Wave 3 added the mandatory colour/offset conversion to
//! `adapters::ZTableRememberedSetView::iterate_old_to_young`, and the round
//! summary then recorded "do not delete the unused adapters" on the grounds
//! that **deleting the type would delete that conversion**. That was the right
//! objection to the wrong thing. The conversion is a `vaddr` function; what the
//! adapter held was one call site that no production path ever reached, and
//! `remembered::ZRememberedSetTable::iterate_slot_addresses` has since become
//! the seam the adapter was standing in for (`ZgcRealHeap::young_extra_roots`
//! consumes it as of wave 4).
//!
//! So the adapter was retired and this file took over the obligation. Every
//! assertion below was reachable only through
//! `adapters::ZTableRememberedSetView` before; none of them is about the
//! adapter.
//!
//! # What makes this worth pinning rather than reading
//!
//! Omitting the conversion **does not fail**. A bare 42-bit offset handed to
//! the generation marker is not rejected — `ZGenerationScope::admits` answers
//! `false` for it, so every cross-generation edge is silently *filtered*, the
//! remembered set reads empty, and live young objects are freed with no error
//! anywhere. And it is platform-dependent in the worst direction: on a Windows
//! dev host the heap base is small enough that an unconverted offset can land
//! inside the heap and the bug hides, while on Linux the base is around
//! `0x7f…` and every edge drops. A test that runs on one host and passes for
//! the wrong reason is exactly what `to_a_machine_address_is_not_the_identity`
//! below refuses to be.

#![cfg(feature = "zgc")]

use cratonvm_gc::zgc::vaddr::{root_address_for_slot_word, ZColor, ZVirtualAddressSpace, Z_NULL};

/// A Linux-shaped base: high enough that a 42-bit offset can never be mistaken
/// for the machine address it encodes. Using a small base here would make the
/// pass-through and the conversion produce indistinguishable answers, which is
/// the precise way this bug hides on a Windows host.
const LINUX_SHAPED_BASE: u64 = 0x0000_7f00_0000_0000;
const SPACE_SIZE: u64 = 64 * 1024 * 1024;

fn space() -> ZVirtualAddressSpace {
    ZVirtualAddressSpace::new(LINUX_SHAPED_BASE, SPACE_SIZE)
        .expect("a 64 MiB space at a Linux-shaped base must be expressible in 42 bits")
}

/// PINS: case 3 of the documented table — an **untagged** word is already a
/// machine address and is returned unchanged, with or without an address
/// space.
///
/// This is the case every heap in this tree hits today (`ZgcRealHeap` stores
/// plain machine pointers), and it is the case a hand-written call site gets
/// right by accident. It has to keep working in both arms, because the point of
/// the function is that a caller need not know which kind of heap it is on.
#[test]
fn an_untagged_word_is_passed_through_with_and_without_a_space() {
    let plain = LINUX_SHAPED_BASE + 0x1234;
    assert_eq!(root_address_for_slot_word(plain, None), Some(plain));
    let s = space();
    assert_eq!(root_address_for_slot_word(plain, Some(&s)), Some(plain));

    // An untagged ZERO answers `Some(0)`, deliberately: a null slot is not this
    // function's business to filter, and returning `None` for two different
    // reasons would let a caller that treats 0 as a root hide behind it.
    assert_eq!(root_address_for_slot_word(0, None), Some(0));
    assert_eq!(root_address_for_slot_word(0, Some(&s)), Some(0));
}

/// PINS: case 1 — a tagged colored word plus its address space becomes the
/// machine address it encodes, **and that address is not the word**.
///
/// The second half is the assertion that survives a move to a different host.
/// Without it this test passes on a machine whose heap base happens to be small
/// even when the conversion has been removed.
#[test]
fn to_a_machine_address_is_not_the_identity() {
    let s = space();
    let addr = LINUX_SHAPED_BASE + 4096;
    let colored = s
        .color_address(addr, ZColor::Remapped)
        .expect("an address inside the space must be encodable");

    assert_ne!(
        colored, addr,
        "the fixture must produce a word that DIFFERS from the address it \
         encodes, or this test cannot tell the conversion from a pass-through"
    );
    assert_eq!(root_address_for_slot_word(colored, Some(&s)), Some(addr));
}

/// PINS: case 2, first half — a tagged word with **no** address space is
/// refused rather than passed through.
///
/// Passing it through is the bug: the marker would receive a 42-bit offset,
/// answer "not mine", and drop a live edge. `None` is what makes the caller
/// count it.
#[test]
fn a_colored_word_without_a_space_is_refused_not_passed_through() {
    let s = space();
    let addr = LINUX_SHAPED_BASE + 8192;
    let colored = s
        .color_address(addr, ZColor::Marked0)
        .expect("encodable address");

    assert_eq!(
        root_address_for_slot_word(colored, None),
        None,
        "a colored word with nothing to convert against was passed through; \
         the marker would have been handed a 42-bit offset"
    );
}

/// PINS: case 2, second half — a tagged word whose offset lands **outside**
/// the space is refused.
///
/// This is the corrupt-word / wrong-space arm. It is a separate test from the
/// one above because the two reach `None` by different routes and a single
/// test would pass with either route broken.
#[test]
fn a_colored_word_whose_offset_is_out_of_range_is_refused() {
    let small = ZVirtualAddressSpace::new(LINUX_SHAPED_BASE, 4096).expect("4 KiB space");
    let wide = space();
    let far = LINUX_SHAPED_BASE + SPACE_SIZE - 8;
    let colored = wide
        .color_address(far, ZColor::Remapped)
        .expect("encodable in the wide space");

    assert_eq!(
        root_address_for_slot_word(colored, Some(&small)),
        None,
        "an offset past the end of the space was converted anyway; the result \
         would be an address outside the heap presented as a root"
    );
    // ...and the same word IS convertible against the space it was made for,
    // so the refusal above is about the range and not about the word.
    assert_eq!(root_address_for_slot_word(colored, Some(&wide)), Some(far));
}

/// PINS: `Z_NULL` answers `Some(0)`, **not** `None` — and records that
/// `root_address_for_slot_word`'s own doc table says otherwise.
///
/// The table in `vaddr.rs` reads "tagged colored, offset out of range, or
/// `Z_NULL` | `None`". `Z_NULL` is `0`, and `is_colored_word(0)` is `false`
/// (the tag bit is clear), so a null word never reaches the colored arm at all:
/// it takes the untagged pass-through on the function's first line and returns
/// `Some(0)`. The `filter(|addr| *addr != 0)` at the end — the thing that would
/// produce the documented `None` — is therefore unreachable for null, and can
/// only fire for a tagged word that `uncolor` maps to zero.
///
/// The behaviour is right; the row is wrong. Pinned here so that whichever of
/// the two is changed, the other has to be looked at — a caller that read the
/// table and wrote `if conv.is_none() { /* null, skip */ }` would treat every
/// null slot as a converted root at address 0. Filed for `vaddr.rs`'s owner in
/// `docs/internal/zgc-round-20260920/gap-p-vaddr-null-row-is-wrong.md`.
#[test]
fn the_null_word_passes_through_as_zero_despite_the_doc_table() {
    let s = space();
    assert_eq!(root_address_for_slot_word(Z_NULL, Some(&s)), Some(0));
    assert_eq!(root_address_for_slot_word(Z_NULL, None), Some(0));
}
