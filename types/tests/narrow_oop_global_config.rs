// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The narrow-oop tests that must publish the PROCESS-GLOBAL geometry, kept in
//! a test binary of their own.
//!
//! # Why these are not unit tests
//!
//! `narrow_oop::ENABLED` is process-global and is read on the hot path of every
//! reference access: `element_byte_size(ArrayElementType::Reference)` resolves
//! through [`cratonvm_types::narrow_oop::ref_element_size`], so publishing a
//! geometry retypes every reference array in the process from an 8-byte element
//! stride to a 4-byte one, mid-life. `cargo test` runs one binary's tests on
//! parallel threads, so a test that enables narrow oops around its own
//! assertions is reinterpreting the heaps — and the constants — of every test
//! running beside it.
//!
//! In `cratonvm-gc` that cost real debugging time: it was the root cause of the
//! `g1::tests::parallel_matches_serial_no_loss_or_dup` flake, which read as a
//! race in G1's parallel evacuator and is not one. The same shape was latent
//! here: `heap_types::tests::element_byte_size_reference` asserts
//! `element_byte_size(Reference) == REF_ELEMENT_SIZE` (8) in the very binary
//! these tests were flipping to 4.
//!
//! A `Mutex` that serialises the global-config tests against EACH OTHER does
//! not fix that — the blast radius is every other test in the binary, and none
//! of them takes the lock. Isolation is the fix: in this binary these tests are
//! the only tests, so the mutex below now bounds the whole population and the
//! serialisation is real.
//!
//! Anything that does NOT need the published geometry belongs back in
//! `types/src/narrow_oop.rs`, written against
//! [`cratonvm_types::narrow_oop::encode_with`] /
//! [`cratonvm_types::narrow_oop::decode_with`], which take the geometry as
//! arguments and touch no global.

use cratonvm_types::narrow_oop::{
    disable_for_test, enable, encode, is_encodable, narrow_limit, narrow_oops_enabled,
    ref_element_size, ref_field_size,
};
use std::sync::Mutex;

/// Serialises the tests in THIS binary. Sound here precisely because this
/// binary contains nothing else — see the module note.
static LOCK: Mutex<()> = Mutex::new(());

/// Take the lock and leave the global config disabled on the way in, so a test
/// that panicked mid-body cannot hand its geometry to the next one.
fn global_config_guard() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    disable_for_test();
    g
}

#[test]
fn disabled_by_default_reports_wide_slots() {
    let _g = global_config_guard();
    assert!(!narrow_oops_enabled());
    assert_eq!(ref_field_size(), 8);
    assert_eq!(ref_element_size(), 8);
}

#[test]
fn enabling_narrows_both_slot_widths() {
    let _g = global_config_guard();
    assert!(enable(0x7f00_0000_0000, 3));
    // The whole reason the global is dangerous: these two are what
    // `element_byte_size(Reference)` and the compact field accessors resolve
    // through, for every object in the process.
    assert_eq!(ref_field_size(), 4);
    assert_eq!(ref_element_size(), 4);
    disable_for_test();
}

#[test]
fn encodability_window() {
    let _g = global_config_guard();
    let base = 0x7f00_0000_0000u64;
    assert!(enable(base, 3));
    assert!(is_encodable(0));
    assert!(!is_encodable(base), "base itself must be reserved for null");
    assert!(is_encodable(base + 8));
    assert!(
        !is_encodable(base + 4),
        "misaligned address is not encodable"
    );
    assert!(!is_encodable(base - 8));
    assert!(!is_encodable(narrow_limit()));
    disable_for_test();
}

#[test]
fn rejects_oversized_shift() {
    let _g = global_config_guard();
    assert!(!enable(0x1000, 4));
    assert!(!narrow_oops_enabled());
}

#[test]
fn the_published_geometry_is_what_encode_reads() {
    // `encode` is `encode_with` against the globals; that delegation is the
    // only thing the published config still decides, so pin it here rather
    // than duplicating the arithmetic tests that live in the lib.
    let _g = global_config_guard();
    let base = 0x7f00_0000_0000u64;
    assert!(enable(base, 3));
    assert_eq!(encode(base + 8), 1);
    assert_eq!(encode(0), 0);
    disable_for_test();
}
