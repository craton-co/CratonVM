// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! gc-common w13-c: `Object.clone()` of a LinkedHashMap, and the
//! re-entrancy guard keys, across a moving collection that hands a vacated
//! address back to the allocator.
//!
//! `common-w12c-lhm-clone-writes-through-a-stale-address-cache`: the ctx-less
//! `clone_lhm_overlay` resolved both maps through `lhm_ptr_cache`, a raw
//! address → overlay key map that was never remapped. A clone allocated at the
//! address a still-live map had left was resolved to THAT map's key, and the
//! source's overlay replaced the moved map's contents. A clone at a fresh
//! address got a row under its raw address, which no reader used and no prune
//! could drop.
//!
//! Each test uses its own `MockCtx` (its own address window and identity-hash
//! range) and its own VM identity, so the process-wide overlay tables cannot
//! mix them up with another test's state.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::MockCtx;
use cratonvm_native_collections::{
    __test_lhm_get, __test_lhm_set, __test_reentry_guard_key,
    clone_lhm_overlay_ctx, gc_overlay_roots_for_collection,
    gc_prune_dead_collection_overlays_for_vm, gc_update_collection_overlay_refs_for_vm,
};
use cratonvm_types::{ObjectRef, PointerMap, Value};

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// Allocate the next object at `at`, then resume the context's own bump
/// pointer.
fn alloc_at(ctx: &mut MockCtx, at: usize) -> ObjectRef {
    let resume = ctx.peek_next_ptr();
    ctx.set_next_ptr(at);
    let o = ctx.alloc_object_simple(0);
    ctx.set_next_ptr(resume);
    assert_eq!(addr(o), at, "the mock must reuse the vacated address");
    o
}

/// The page's retirement test: a live map moves, a clone of ANOTHER map is
/// allocated at the address it left, and the moved map's contents are
/// unchanged. Then the clone's row behaves like any slot-keyed row: it has
/// the source's state, follows the clone when it moves, and is pruned when
/// the clone dies.
#[test]
fn w13c_clone_onto_a_vacated_address_leaves_the_moved_map_intact() {
    const VM: usize = 0xC13C_0001;
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(VM);

    // A live LinkedHashMap written at D, then moved to D' by a collection.
    let moved = ctx.alloc_object_simple(0);
    let moved_head = ctx.alloc_object_simple(0);
    __test_lhm_set(&mut ctx, moved, "head", obj(moved_head));
    let vacated = addr(moved);
    let moved = ctx.relocate_object(moved);
    let mut pm = PointerMap::default();
    pm.insert(vacated, addr(moved));
    gc_update_collection_overlay_refs_for_vm(VM, &pm);
    assert_eq!(__test_lhm_get(&ctx, moved, "head"), obj(moved_head));

    // The source map, and its clone allocated at D. `marker` stands for an
    // overlay-only reference: the copy carries every name except the four
    // structural ones, and a reference is what the owner index can show.
    let src = ctx.alloc_object_simple(0);
    let src_head = ctx.alloc_object_simple(0);
    let marker = ctx.alloc_object_simple(0);
    __test_lhm_set(&mut ctx, src, "head", obj(src_head));
    __test_lhm_set(&mut ctx, src, "accessOrder", Value::Int(1));
    __test_lhm_set(&mut ctx, src, "__w13c_marker", obj(marker));
    let clone = alloc_at(&mut ctx, vacated);

    // The slot-keyed form: the clone gets the source's overlay-only state,
    // and not its structural state (`HashMap.clone()` rebuilds that from the
    // source in bytecode; sharing it would share the source's nodes).
    clone_lhm_overlay_ctx(&ctx, src, clone);
    assert_eq!(
        __test_lhm_get(&ctx, moved, "head"),
        obj(moved_head),
        "the slot-keyed copy must not touch the moved map either"
    );
    assert_eq!(__test_lhm_get(&ctx, clone, "accessOrder"), Value::Int(1));
    assert_eq!(__test_lhm_get(&ctx, clone, "__w13c_marker"), obj(marker));
    assert_eq!(
        __test_lhm_get(&ctx, clone, "head"),
        Value::Object(None),
        "the clone must not share the source's list"
    );
    assert_eq!(__test_lhm_get(&ctx, src, "head"), obj(src_head));
    let clone_roots = gc_overlay_roots_for_collection(vacated, None);
    assert!(
        clone_roots.contains(&marker),
        "the clone's row must be owner-indexed under the clone's address"
    );
    assert!(!clone_roots.contains(&src_head));

    // The clone moves: its row follows it.
    let clone = ctx.relocate_object(clone);
    let mut pm = PointerMap::default();
    pm.insert(vacated, addr(clone));
    gc_update_collection_overlay_refs_for_vm(VM, &pm);
    assert_eq!(__test_lhm_get(&ctx, clone, "__w13c_marker"), obj(marker));
    assert_eq!(__test_lhm_get(&ctx, moved, "head"), obj(moved_head));

    // The clone dies: its row goes with it, and nothing else does.
    let live = [addr(moved), addr(src)];
    gc_prune_dead_collection_overlays_for_vm(VM, &|a: usize| live.contains(&a));
    assert!(
        gc_overlay_roots_for_collection(addr(clone), None).is_empty(),
        "a dead clone's row must be pruned"
    );
    assert_eq!(__test_lhm_get(&ctx, src, "head"), obj(src_head));
    assert_eq!(__test_lhm_get(&ctx, moved, "head"), obj(moved_head));
}

/// A source with nothing to copy leaves the clone without a row: neither a
/// map with no overlay row at all (populated by real bytecode) nor one whose
/// row holds only structural state.
#[test]
fn w13c_clone_of_a_map_with_nothing_to_copy_adds_nothing() {
    const VM: usize = 0xC13C_0002;
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(VM);
    let src = ctx.alloc_object_simple(0);
    let clone = ctx.alloc_object_simple(0);
    clone_lhm_overlay_ctx(&ctx, src, clone);
    assert!(gc_overlay_roots_for_collection(addr(clone), None).is_empty());

    let head = ctx.alloc_object_simple(0);
    __test_lhm_set(&mut ctx, src, "head", obj(head));
    let clone2 = ctx.alloc_object_simple(0);
    clone_lhm_overlay_ctx(&ctx, src, clone2);
    assert!(gc_overlay_roots_for_collection(addr(clone2), None).is_empty());
    assert_eq!(__test_lhm_get(&ctx, clone2, "head"), Value::Object(None));
    assert_eq!(__test_lhm_get(&ctx, src, "head"), obj(head));
}

/// The re-entrancy guards held across a call into Java (`DELEGATE_GUARD`,
/// `COW_SET_GUARD`, `SNAPITR_FALLBACK`) key a receiver by a value that
/// survives its move, and that a newcomer at its vacated address does not
/// share. A raw address had neither property.
#[test]
fn w13c_reentry_guard_key_follows_the_object_not_the_address() {
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(0xC13C_0003);
    let receiver = ctx.alloc_object_simple(0);
    let key = __test_reentry_guard_key(&ctx, receiver);
    let vacated = addr(receiver);
    assert_ne!(key, vacated, "the guard key must not be the raw address");

    let receiver = ctx.relocate_object(receiver);
    assert_eq!(
        __test_reentry_guard_key(&ctx, receiver),
        key,
        "a moved receiver must keep its guard key, or a re-entry after a \
         collection would go undetected"
    );

    let newcomer = alloc_at(&mut ctx, vacated);
    assert_ne!(
        __test_reentry_guard_key(&ctx, newcomer),
        key,
        "an object at the vacated address must not read as the receiver \
         re-entering"
    );
}
