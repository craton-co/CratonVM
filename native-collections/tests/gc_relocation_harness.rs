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

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::MockCtx;
use cratonvm_native_api::NativeContext;
use cratonvm_native_collections::identity_hash::obj_key;
use cratonvm_native_collections::{
    __test_lhm_get, __test_lhm_set, __test_ll_get, __test_ll_set, __test_tm_fast_get_str,
    __test_tm_fast_put_str, __test_tm_get_slot, __test_tm_set_slot, __test_ts_get_slot,
    __test_ts_set_slot, gc_overlay_roots_for_collection, gc_overlay_roots_for_matching_owners,
    gc_scan_collection_overlay_roots, gc_update_collection_overlay_refs,
};
use cratonvm_types::{ObjectRef, Value};
use std::collections::HashMap;

// TreeMap / TreeSet slot indices — mirror the private consts in lib.rs.
// (DATA / SIZE / COMPARATOR live at 0/1/2 in both side-tables.)
const TM_FIELD_DATA: usize = 0;
const TM_FIELD_SIZE: usize = 1;
const TS_FIELD_DATA: usize = 0;
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
// Fast-mode TreeMap value overlay (GC roots + remap) — finding B1/V1
// ---------------------------------------------------------------------------
//
// A comparator-less `TreeMap<String,Object>` stores its entries in the
// `tm_fast_table` BTreeMap (the array slot is left empty), so an object
// VALUE is reachable only through that side-table. If the GC integration
// functions skip the fast table, a moving young-gen GC reclaims the value
// (use-after-free) or leaves a dangling pointer. These two tests pin the
// fix: the value must be reported as a root, and remapped after a move.

/// `gc_scan_collection_overlay_roots` must report a fast-mode TreeMap's
/// object value so the collector keeps it live.
#[test]
fn fast_treemap_value_is_a_gc_root() {
    let mut ctx = MockCtx::new();
    let tm = ctx.alloc_object_simple(0);
    let val = ctx.alloc_object_simple(0);

    __test_tm_fast_put_str(&ctx, tm, "k", Value::Object(Some(val)));

    let mut roots: Vec<ObjectRef> = Vec::new();
    gc_scan_collection_overlay_roots(&mut roots);
    assert!(
        roots.iter().any(|r| r.as_ptr() == val.as_ptr()),
        "fast-mode TreeMap object value must be reported as a GC root — \
         otherwise a moving collector reclaims it as garbage (B1/V1 use-after-free)"
    );
}

/// The non-moving Generational marker must follow a side-table edge only after
/// its owning collection was marked. A different collection's overlay must not
/// turn this value into a process-global root.
#[test]
fn overlay_roots_are_scoped_to_the_marked_collection_owner() {
    let mut ctx = MockCtx::new();
    let live_owner = ctx.alloc_object_simple(0);
    let dead_owner = ctx.alloc_object_simple(0);
    let live_value = ctx.alloc_object_simple(0);
    let dead_value = ctx.alloc_object_simple(0);

    __test_tm_fast_put_str(&ctx, live_owner, "live", Value::Object(Some(live_value)));
    __test_tm_fast_put_str(&ctx, dead_owner, "dead", Value::Object(Some(dead_value)));

    // `None`: this mock harness does not model class identity, so the
    // owner-recycling check is not what this case exercises.
    let roots = gc_overlay_roots_for_collection(live_owner.as_ptr() as usize, None);
    assert!(roots.iter().any(|r| r.as_ptr() == live_value.as_ptr()));
    assert!(
        roots.iter().all(|r| r.as_ptr() != dead_value.as_ptr()),
        "a different collection's overlay value must not be rooted"
    );
}

/// `gc_update_collection_overlay_refs` must repoint a fast-mode TreeMap's
/// object value to its relocated address after a moving GC.
#[test]
fn fast_treemap_value_survives_relocation() {
    let mut ctx = MockCtx::new();
    let tm = ctx.alloc_object_simple(0);
    let val = ctx.alloc_object_simple(0);

    __test_tm_fast_put_str(&ctx, tm, "k", Value::Object(Some(val)));
    assert_eq!(
        __test_tm_fast_get_str(&ctx, tm, "k"),
        Value::Object(Some(val)),
        "baseline: fast-mode value visible before relocation"
    );

    // Simulate a moving GC relocating the VALUE (the TreeMap itself stays
    // put, so its overlay key is unchanged). Build the pointer map the
    // collector would hand us: old value address -> new value address.
    let old_addr = val.as_ptr() as usize;
    let val_post = ctx.relocate_object(val);
    let new_addr = val_post.as_ptr() as usize;
    assert_ne!(
        old_addr, new_addr,
        "relocate must hand back a fresh address"
    );

    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(old_addr, new_addr);
    gc_update_collection_overlay_refs(&pm);

    let got = __test_tm_fast_get_str(&ctx, tm, "k");
    assert_eq!(
        got,
        Value::Object(Some(val_post)),
        "fast-mode TreeMap value must be repointed to its relocated address — \
         without the tm_fast_table remap it stays a stale dangling pointer (B1/V1)"
    );
    match got {
        Value::Object(Some(r)) => assert_eq!(
            r.as_ptr() as usize,
            new_addr,
            "remapped value must resolve to the new address"
        ),
        other => panic!("expected relocated object value, got {other:?}"),
    }
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

// ---------------------------------------------------------------------------
// Cross-table funnel coverage — `for_each_overlay_ref`
// ---------------------------------------------------------------------------
//
// Both GC hooks are now defined in terms of the single `for_each_overlay_ref`
// enumeration (the B1/V1 follow-up: the fast-table omission was possible only
// because the table list was hand-duplicated across the scan and remap
// functions). These two tests plant ONE object value in EACH of the five
// overlays and assert the funnel reaches all of them — for both rooting and
// remapping. If a future edit adds a 6th side-table but forgets to list it in
// `for_each_overlay_ref`, or drops one of the five, the relevant assert fires.
//   1. LinkedList overlay value      4. TreeMap fast-mode BTreeMap value
//   2. LinkedHashMap overlay value   5. TreeSet array-mode `data`
//   3. TreeMap array-mode `data`

/// Plant one object value in each of the five overlays. Returns the five
/// collection objects and their five (distinct) value objects, in funnel
/// order: (ll, lhm, tm, tmf, ts) and (v_ll, v_lhm, v_tm, v_tmf, v_ts).
#[allow(clippy::type_complexity)]
fn plant_one_value_per_overlay(ctx: &mut MockCtx) -> ([ObjectRef; 5], [ObjectRef; 5]) {
    let cols = [
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
    ];
    let vals = [
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
        ctx.alloc_object_simple(0),
    ];
    __test_ll_set(ctx, cols[0], "head", Value::Object(Some(vals[0])));
    __test_lhm_set(ctx, cols[1], "table", Value::Object(Some(vals[1])));
    __test_tm_set_slot(ctx, cols[2], TM_FIELD_DATA, Value::Object(Some(vals[2])));
    __test_tm_fast_put_str(ctx, cols[3], "k", Value::Object(Some(vals[3])));
    __test_ts_set_slot(ctx, cols[4], TS_FIELD_DATA, Value::Object(Some(vals[4])));
    (cols, vals)
}

const OVERLAY_LABELS: [&str; 5] = [
    "LinkedList overlay value",
    "LinkedHashMap overlay value",
    "TreeMap array-mode data",
    "TreeMap fast-mode value",
    "TreeSet array-mode data",
];

/// `for_each_overlay_ref` (via `gc_scan_collection_overlay_roots`) must reach
/// every overlay's object value, not just the collection objects' keys.
#[test]
fn all_overlay_object_values_are_roots() {
    let mut ctx = MockCtx::new();
    let (_cols, vals) = plant_one_value_per_overlay(&mut ctx);

    let mut roots: Vec<ObjectRef> = Vec::new();
    gc_scan_collection_overlay_roots(&mut roots);

    for (i, v) in vals.iter().enumerate() {
        assert!(
            roots.iter().any(|r| r.as_ptr() == v.as_ptr()),
            "{} missing from GC roots — for_each_overlay_ref skipped this \
             overlay (B1/V1 class of bug)",
            OVERLAY_LABELS[i]
        );
    }
}

/// The MOVING young collector cannot walk owners the way the non-moving
/// marker does, so it seeds from
/// `external_roots_for_matching_owners(&|_| true)` — every current owner's
/// refs, regardless of generation. Coverage here is therefore load-bearing:
/// an overlay this enumeration misses is a live backing array the moving
/// collector silently reclaims, which is exactly the 2026-07-31
/// `TreeSet.contains` -> `checkcast: not an object reference` abort.
#[test]
fn every_overlay_value_is_reachable_through_an_always_true_owner_predicate() {
    let mut ctx = MockCtx::new();
    let (_cols, vals) = plant_one_value_per_overlay(&mut ctx);

    let roots = gc_overlay_roots_for_matching_owners(&|_| true);

    for (i, v) in vals.iter().enumerate() {
        assert!(
            roots.iter().any(|r| r.as_ptr() == v.as_ptr()),
            "{} missing from the always-true owner-predicate seed the moving \
             young collector relies on",
            OVERLAY_LABELS[i]
        );
    }
}

/// The owner-index seed must survive the OWNER moving, not just the value.
///
/// Every root path that is not the unconditional table scan reaches an
/// overlay through `overlay_owner_keys`, an ADDRESS-keyed reverse index: the
/// moving young collector's `external_roots_for_matching_owners(&|_| true)`
/// seed, the non-moving young marker's per-owner walk, and `old_gen_gc`'s
/// `external_roots_for_owner(obj_ptr)` in its mark BFS. The side tables
/// themselves are keyed by the relocation-invariant identity hash, so the
/// collection keeps reading its own state correctly after a move whether or
/// not that index was re-keyed — which is precisely what makes a missed
/// re-key silent. The only thing it breaks is the GC's ability to answer
/// "which refs does this collection own?", and the first symptom is a
/// reclaimed backing array surfacing as `checkcast: not an object reference`
/// somewhere else entirely.
///
/// `gc_update_collection_overlay_refs` is what maintains that index across a
/// move. Assert both readers against the POST-move address.
#[test]
fn owner_seeded_roots_follow_the_owner_across_a_relocation() {
    let mut ctx = MockCtx::new();
    let (cols, vals) = plant_one_value_per_overlay(&mut ctx);

    // Move the COLLECTIONS and leave the values put — the mirror image of
    // `all_overlay_object_values_survive_relocation`, and the case the
    // address-keyed index actually depends on.
    let mut pm = cratonvm_types::PointerMap::default();
    let mut moved_cols = [cols[0]; 5];
    for (i, c) in cols.iter().enumerate() {
        let post = ctx.relocate_object(*c);
        pm.insert(c.as_ptr() as usize, post.as_ptr() as usize);
        moved_cols[i] = post;
    }

    gc_update_collection_overlay_refs(&pm);

    // 1. The always-true seed the moving young collector uses. It unions
    //    every indexed owner's refs, so it fails only if the re-key LOST an
    //    entry rather than merely leaving it at a stale address.
    let seeded = gc_overlay_roots_for_matching_owners(&|_| true);
    for (i, v) in vals.iter().enumerate() {
        assert!(
            seeded.iter().any(|r| r.as_ptr() == v.as_ptr()),
            "{} dropped out of the always-true owner seed after its owner \
             relocated — the moving young collector would reclaim it",
            OVERLAY_LABELS[i]
        );
    }

    // 2. The per-owner walk, queried at the POST-move address. This is the
    //    stricter of the two: it fails if the entry merely stayed at the
    //    owner's OLD address, which the union above cannot see.
    for (i, v) in vals.iter().enumerate() {
        // `None` for the owner-class discriminator: this harness relocates the
        // owners itself and never recycles an address under a different class,
        // which is the only case the filter exists to catch. `None` is the
        // behaviour the assertion below was written against, before
        // `gc_overlay_roots_for_collection` grew the parameter — the call site
        // was not updated then, so this test target has not compiled since.
        let owned = gc_overlay_roots_for_collection(moved_cols[i].as_ptr() as usize, None);
        assert!(
            owned.iter().any(|r| r.as_ptr() == v.as_ptr()),
            "{} is not reachable from its owner's POST-move address — the \
             owner index still names the pre-move address, so the non-moving \
             young marker and old_gen_gc's mark BFS both miss this edge",
            OVERLAY_LABELS[i]
        );
    }
}

/// `for_each_overlay_ref` (via `gc_update_collection_overlay_refs`) must
/// remap every overlay's object value after a moving GC.
#[test]
fn all_overlay_object_values_survive_relocation() {
    let mut ctx = MockCtx::new();
    let (cols, vals) = plant_one_value_per_overlay(&mut ctx);

    // Relocate the VALUE objects only — the collection objects (overlay keys)
    // stay put so their reads still resolve. Build the pointer map the
    // collector would hand us in one pass.
    let mut pm = cratonvm_types::PointerMap::default();
    let mut moved = [vals[0]; 5];
    for (i, v) in vals.iter().enumerate() {
        let post = ctx.relocate_object(*v);
        pm.insert(v.as_ptr() as usize, post.as_ptr() as usize);
        moved[i] = post;
    }

    gc_update_collection_overlay_refs(&pm);

    assert_eq!(
        __test_ll_get(&ctx, cols[0], "head"),
        Value::Object(Some(moved[0])),
        "{} not remapped",
        OVERLAY_LABELS[0]
    );
    assert_eq!(
        __test_lhm_get(&ctx, cols[1], "table"),
        Value::Object(Some(moved[1])),
        "{} not remapped",
        OVERLAY_LABELS[1]
    );
    assert_eq!(
        __test_tm_get_slot(&ctx, cols[2], TM_FIELD_DATA),
        Value::Object(Some(moved[2])),
        "{} not remapped",
        OVERLAY_LABELS[2]
    );
    assert_eq!(
        __test_tm_fast_get_str(&ctx, cols[3], "k"),
        Value::Object(Some(moved[3])),
        "{} not remapped",
        OVERLAY_LABELS[3]
    );
    assert_eq!(
        __test_ts_get_slot(&ctx, cols[4], TS_FIELD_DATA),
        Value::Object(Some(moved[4])),
        "{} not remapped",
        OVERLAY_LABELS[4]
    );
}
