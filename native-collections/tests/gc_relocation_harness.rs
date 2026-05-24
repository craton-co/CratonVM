// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! GC-relocation reachability harness for the four collection overlays.
//!
//! Today the LinkedList / LinkedHashMap / TreeMap / TreeSet side-tables
//! are keyed by `ctx.identity_hash_code(this)` (was `this.as_ptr()`).
//! A moving GC preserves the identity-hash word during compaction, so
//! the same overlay entry must remain reachable through the relocated
//! `ObjectRef`.
//!
//! Each test:
//!   1. Writes a marker value into the overlay via the pre-relocation
//!      `ObjectRef`.
//!   2. Simulates a GC move via `MockCtx::relocate_object`, which hands
//!      back a fresh `ObjectRef` at a brand-new pointer address but
//!      carries the original identity-hash word forward (the exact
//!      contract a real moving GC's `forward_object` enforces).
//!   3. Reads the marker back through the *new* `ObjectRef` and asserts
//!      it matches.
//!
//! Pre-rekey (when overlay tables were keyed by `as_ptr() as usize`),
//! step 3 returned the default-empty value because the new pointer
//! didn't hash to the existing entry. Post-rekey, the identity-hash
//! key is invariant across the move and the read hits.

mod common;

use common::MockCtx;
use cratonvm_native_api::NativeContext;
use cratonvm_native_collections::{
    __test_ll_get, __test_ll_set, __test_lhm_get, __test_lhm_set,
    __test_tm_get_slot, __test_tm_set_slot, __test_ts_get_slot, __test_ts_set_slot,
};
use cratonvm_native_collections::identity_hash::obj_key;
use cratonvm_types::Value;

// TreeMap / TreeSet slot indices — mirror the private consts in lib.rs.
// (DATA / SIZE / COMPARATOR live at 0/1/2 in both side-tables.)
const TM_FIELD_DATA: usize = 0;
const TM_FIELD_SIZE: usize = 1;
const TS_FIELD_SIZE: usize = 1;

/// Sanity check on the mock itself: `identity_hash_code` must be
/// stable across `relocate_object`. If this fails the rest of the
/// suite is meaningless — every other test below leans on this
/// invariant to model the moving-GC contract.
#[test]
fn mock_preserves_identity_hash_across_relocation() {
    let mut ctx = MockCtx::new();
    let pre = ctx.alloc_object_simple(0);
    let pre_hash = ctx.identity_hash_code(pre);
    let post = ctx.relocate_object(pre);
    let post_hash = ctx.identity_hash_code(post);
    assert_ne!(
        pre.as_ptr() as usize,
        post.as_ptr() as usize,
        "relocate_object must hand back a fresh pointer"
    );
    assert_eq!(
        pre_hash, post_hash,
        "identity_hash_code must survive relocation — this is the \
         GC invariant every overlay-rekey test below relies on"
    );
}

/// `identity_hash::obj_key` is the single funnel every overlay key
/// site calls into. Verify it returns the same `usize` pre- and
/// post-move for the same logical object.
#[test]
fn obj_key_is_stable_across_relocation() {
    let mut ctx = MockCtx::new();
    let pre = ctx.alloc_object_simple(0);
    let pre_key = obj_key(&ctx, pre);
    let post = ctx.relocate_object(pre);
    let post_key = obj_key(&ctx, post);
    assert_eq!(
        pre_key, post_key,
        "obj_key must be identity-hash-derived, not address-derived"
    );
    // Sanity: distinct objects must have distinct keys (probabilistic
    // — the mock's hash derivation guarantees no collision for the
    // two pointers minted in this test).
    let other = ctx.alloc_object_simple(0);
    let other_key = obj_key(&ctx, other);
    assert_ne!(
        pre_key, other_key,
        "distinct objects must hash to distinct overlay keys"
    );
}

// ---------------------------------------------------------------------------
// LinkedList overlay
// ---------------------------------------------------------------------------

#[test]
fn linked_list_overlay_survives_relocation() {
    let mut ctx = MockCtx::new();
    let ll = ctx.alloc_object_simple(0);

    __test_ll_set(&mut ctx, ll, "size", Value::Int(42));
    __test_ll_set(&mut ctx, ll, "head", Value::Object(None));
    assert_eq!(
        __test_ll_get(&ctx, ll, "size"),
        Value::Int(42),
        "baseline: overlay write visible before relocation"
    );

    let ll_post = ctx.relocate_object(ll);
    assert_eq!(
        __test_ll_get(&ctx, ll_post, "size"),
        Value::Int(42),
        "C13 contract: LinkedList overlay 'size' must still resolve \
         through the relocated ObjectRef — overlay key did not survive \
         the GC move, which is the regression this rekey prevents"
    );
}

// ---------------------------------------------------------------------------
// LinkedHashMap overlay
// ---------------------------------------------------------------------------

#[test]
fn linked_hashmap_overlay_survives_relocation() {
    let mut ctx = MockCtx::new();
    let lhm = ctx.alloc_object_simple(0);

    __test_lhm_set(&mut ctx, lhm, "size", Value::Int(7));
    __test_lhm_set(&mut ctx, lhm, "table", Value::Object(None));
    __test_lhm_set(&mut ctx, lhm, "accessOrder", Value::Int(1));
    assert_eq!(__test_lhm_get(&ctx, lhm, "size"), Value::Int(7));

    let lhm_post = ctx.relocate_object(lhm);
    assert_eq!(
        __test_lhm_get(&ctx, lhm_post, "size"),
        Value::Int(7),
        "LHM overlay 'size' must remain reachable after relocation"
    );
    assert_eq!(
        __test_lhm_get(&ctx, lhm_post, "accessOrder"),
        Value::Int(1),
        "LHM overlay 'accessOrder' (the C25 LRU flag) must also survive — \
         losing it would silently revert an access-order LHM to insertion-order"
    );
}

/// Two distinct LinkedHashMaps must NOT share overlay entries — the
/// rekey contract is per-object, not per-process.
#[test]
fn linked_hashmap_distinct_objects_have_distinct_overlays() {
    let mut ctx = MockCtx::new();
    let lhm_a = ctx.alloc_object_simple(0);
    let lhm_b = ctx.alloc_object_simple(0);

    __test_lhm_set(&mut ctx, lhm_a, "size", Value::Int(10));
    __test_lhm_set(&mut ctx, lhm_b, "size", Value::Int(99));
    assert_eq!(__test_lhm_get(&ctx, lhm_a, "size"), Value::Int(10));
    assert_eq!(__test_lhm_get(&ctx, lhm_b, "size"), Value::Int(99));

    // Relocate just one of the two — the other's overlay must be
    // unaffected.
    let lhm_a_post = ctx.relocate_object(lhm_a);
    assert_eq!(__test_lhm_get(&ctx, lhm_a_post, "size"), Value::Int(10));
    assert_eq!(__test_lhm_get(&ctx, lhm_b, "size"), Value::Int(99));
}

// ---------------------------------------------------------------------------
// TreeMap overlay
// ---------------------------------------------------------------------------

#[test]
fn treemap_overlay_survives_relocation() {
    let mut ctx = MockCtx::new();
    let tm = ctx.alloc_object_simple(0);

    __test_tm_set_slot(&mut ctx, tm, TM_FIELD_SIZE, Value::Int(5));
    __test_tm_set_slot(&mut ctx, tm, TM_FIELD_DATA, Value::Object(None));
    assert_eq!(__test_tm_get_slot(&ctx, tm, TM_FIELD_SIZE), Value::Int(5));

    let tm_post = ctx.relocate_object(tm);
    assert_eq!(
        __test_tm_get_slot(&ctx, tm_post, TM_FIELD_SIZE),
        Value::Int(5),
        "TreeMap array-mode size must remain reachable after relocation — \
         the side-table is the sole authoritative store for subclass-layout \
         compatibility, so losing it on GC silently zeroes the map"
    );
}

// ---------------------------------------------------------------------------
// TreeSet overlay
// ---------------------------------------------------------------------------

#[test]
fn treeset_overlay_survives_relocation() {
    let mut ctx = MockCtx::new();
    let ts = ctx.alloc_object_simple(0);

    __test_ts_set_slot(&mut ctx, ts, TS_FIELD_SIZE, Value::Int(3));
    assert_eq!(__test_ts_get_slot(&ctx, ts, TS_FIELD_SIZE), Value::Int(3));

    let ts_post = ctx.relocate_object(ts);
    assert_eq!(
        __test_ts_get_slot(&ctx, ts_post, TS_FIELD_SIZE),
        Value::Int(3),
        "TreeSet overlay size must remain reachable after relocation"
    );
}

/// Final cross-cutting check: a single relocation must keep *all four*
/// overlays consistent at the same time. This catches a partial rekey
/// (e.g. someone re-broke only one of the four key sites).
#[test]
fn all_four_overlays_survive_concurrent_relocation() {
    let mut ctx = MockCtx::new();
    let ll = ctx.alloc_object_simple(0);
    let lhm = ctx.alloc_object_simple(0);
    let tm = ctx.alloc_object_simple(0);
    let ts = ctx.alloc_object_simple(0);

    __test_ll_set(&mut ctx, ll, "size", Value::Int(11));
    __test_lhm_set(&mut ctx, lhm, "size", Value::Int(22));
    __test_tm_set_slot(&mut ctx, tm, TM_FIELD_SIZE, Value::Int(33));
    __test_ts_set_slot(&mut ctx, ts, TS_FIELD_SIZE, Value::Int(44));

    let ll2 = ctx.relocate_object(ll);
    let lhm2 = ctx.relocate_object(lhm);
    let tm2 = ctx.relocate_object(tm);
    let ts2 = ctx.relocate_object(ts);

    assert_eq!(__test_ll_get(&ctx, ll2, "size"), Value::Int(11));
    assert_eq!(__test_lhm_get(&ctx, lhm2, "size"), Value::Int(22));
    assert_eq!(__test_tm_get_slot(&ctx, tm2, TM_FIELD_SIZE), Value::Int(33));
    assert_eq!(__test_ts_get_slot(&ctx, ts2, TS_FIELD_SIZE), Value::Int(44));
}
