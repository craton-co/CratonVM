// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! gc-common w12-c: the collection-overlay rows are scanned, remapped, pruned
//! and forgotten PER VM.
//!
//! The overlay tables and the object-key registry are process-wide, and a
//! multi-VM process (`libcratonvm` embedders, every unit-test binary) holds
//! several heaps. Before w12-c the provider's GC hooks took no VM, so:
//!
//!   * VM A's root scan handed A's collector VM B's overlay elements;
//!   * VM A's pointer map was applied to VM B's rows, slots and owner index;
//!   * VM A's post-collection prune asked A's liveness -- "dead" for every
//!     address outside A's heap -- about B's owners, and deleted B's live
//!     collections (on Generational, where no span screen applied:
//!     `common-w10g-generational-overlay-prune-has-no-ownership-screen`);
//!   * nothing dropped a torn-down VM's rows.
//!
//! Each test stands two `MockCtx`s up as two VMs (`set_vm_identity`), with
//! identities no other test in this binary uses. Each context allocates in its
//! own 4 GiB address window, which is how an address is attributed to a VM.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::MockCtx;
use cratonvm_native_collections::{
    __test_ll_get, __test_ll_set, __test_tm_fast_get_str, __test_tm_fast_put_str,
    forget_vm_collection_overlays, gc_overlay_roots_for_collection,
    gc_prune_dead_collection_overlays_for_vm, gc_scan_collection_overlay_roots_for_vm,
    gc_update_collection_overlay_refs_for_vm,
};
use cratonvm_types::{ObjectRef, Value};

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn vm_ctx(vm: usize) -> MockCtx {
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(vm);
    ctx
}

/// One VM's overlay-backed collections: a LinkedList whose `head` and a
/// fast-mode TreeMap whose one value live only in the overlay tables.
struct Planted {
    list: ObjectRef,
    list_head: ObjectRef,
    map: ObjectRef,
    map_value: ObjectRef,
}

fn plant(ctx: &mut MockCtx) -> Planted {
    let list = ctx.alloc_object_simple(0);
    let list_head = ctx.alloc_object_simple(0);
    let map = ctx.alloc_object_simple(0);
    let map_value = ctx.alloc_object_simple(0);
    __test_ll_set(ctx, list, "head", obj(list_head));
    __test_tm_fast_put_str(ctx, map, "k", obj(map_value));
    Planted {
        list,
        list_head,
        map,
        map_value,
    }
}

fn in_window(window: (usize, usize), a: usize) -> bool {
    a >= window.0 && a < window.1
}

/// Scan: each VM's scan returns its own rows and none of the other VM's.
/// Remap: VM A's pointer map moves A's refs, slots and owner-index entries
/// and leaves B's alone even when it names B's addresses. Prune: A's prune
/// with an all-dead predicate deletes A's rows only. Forget: B's teardown
/// drops B's rows.
#[test]
fn w12c_overlay_rows_are_scanned_remapped_pruned_and_forgotten_per_vm() {
    const VM_A: usize = 0xC12A_0001;
    const VM_B: usize = 0xC12A_0002;
    let mut ctx_a = vm_ctx(VM_A);
    let mut ctx_b = vm_ctx(VM_B);
    let win_a = ctx_a.address_window();
    let win_b = ctx_b.address_window();
    assert_ne!(win_a, win_b, "the two mock VMs must not share addresses");
    let a = plant(&mut ctx_a);
    let b = plant(&mut ctx_b);

    // --- Scan -------------------------------------------------------------
    let mut roots_a = Vec::new();
    gc_scan_collection_overlay_roots_for_vm(VM_A, &mut roots_a);
    assert!(roots_a.contains(&a.list_head) && roots_a.contains(&a.map_value));
    assert!(
        !roots_a.iter().any(|r| in_window(win_b, addr(*r))),
        "VM A's scan handed A's collector one of VM B's overlay elements"
    );
    let mut roots_b = Vec::new();
    gc_scan_collection_overlay_roots_for_vm(VM_B, &mut roots_b);
    assert!(roots_b.contains(&b.list_head) && roots_b.contains(&b.map_value));
    assert!(
        !roots_b.iter().any(|r| in_window(win_a, addr(*r))),
        "VM B's scan handed B's collector one of VM A's overlay elements"
    );

    // --- Remap ------------------------------------------------------------
    // A's collection moves A's list head and A's map owner. Its pointer map
    // also (impossibly, but it is the defect's shape) names B's head and B's
    // list owner, to bogus addresses: a VM-less remap would rewrite them.
    let moved_head_a = ctx_a.relocate_object(a.list_head);
    let moved_map_a = ctx_a.relocate_object(a.map);
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(addr(a.list_head), addr(moved_head_a));
    pm.insert(addr(a.map), addr(moved_map_a));
    pm.insert(addr(b.list_head), 0x1000);
    pm.insert(addr(b.list), 0x2000);
    gc_update_collection_overlay_refs_for_vm(VM_A, &pm);

    assert_eq!(
        __test_ll_get(&ctx_a, a.list, "head"),
        obj(moved_head_a),
        "A's own row was not remapped"
    );
    assert!(
        gc_overlay_roots_for_collection(addr(moved_map_a), None).contains(&a.map_value),
        "A's owner-index entry did not follow A's map to its new address"
    );
    assert_eq!(
        __test_ll_get(&ctx_b, b.list, "head"),
        obj(b.list_head),
        "VM A's pointer map rewrote VM B's overlay row"
    );
    assert!(
        gc_overlay_roots_for_collection(addr(b.list), None).contains(&b.list_head),
        "VM A's pointer map moved VM B's owner-index entry"
    );

    // B's slot kept its address too: B's own prune, told only B's list and
    // map are live, keeps B's rows. Had A's remap moved B's slot to 0x2000,
    // this would condemn it.
    let (b_list, b_map) = (addr(b.list), addr(b.map));
    gc_prune_dead_collection_overlays_for_vm(VM_B, &|x: usize| x == b_list || x == b_map);
    assert_eq!(__test_ll_get(&ctx_b, b.list, "head"), obj(b.list_head));

    // --- Prune ------------------------------------------------------------
    // A's collection finds every owner of ITS heap dead, and -- as a real
    // VM's liveness does -- every address outside its heap dead too.
    gc_prune_dead_collection_overlays_for_vm(VM_A, &|_: usize| false);
    assert_eq!(
        __test_ll_get(&ctx_b, b.list, "head"),
        obj(b.list_head),
        "VM A's prune deleted VM B's live LinkedList (the w10-g scenario)"
    );
    assert_eq!(
        __test_tm_fast_get_str(&ctx_b, b.map, "k"),
        obj(b.map_value),
        "VM A's prune deleted VM B's live TreeMap (the w10-g scenario)"
    );
    let mut roots_a = Vec::new();
    gc_scan_collection_overlay_roots_for_vm(VM_A, &mut roots_a);
    assert!(
        !roots_a.contains(&moved_head_a) && !roots_a.contains(&a.map_value),
        "A's prune must still delete A's own dead rows"
    );

    // --- Forget -----------------------------------------------------------
    forget_vm_collection_overlays(VM_B);
    let mut roots_b = Vec::new();
    gc_scan_collection_overlay_roots_for_vm(VM_B, &mut roots_b);
    assert!(
        !roots_b.contains(&b.list_head) && !roots_b.contains(&b.map_value),
        "a torn-down VM's rows must be dropped"
    );
    assert!(
        gc_overlay_roots_for_collection(addr(b.list), None).is_empty(),
        "a torn-down VM's owner-index entries must be dropped"
    );
    let mut roots_other = Vec::new();
    gc_scan_collection_overlay_roots_for_vm(0xC12A_00FF, &mut roots_other);
    assert!(
        !roots_other.contains(&b.list_head),
        "no VM may root a torn-down VM's rows"
    );
}

/// The same contract through the collector-facing registry: registering the
/// natives registers the VM-scoped callbacks, and the `_for_vm` fan-outs in
/// `cratonvm_gc::external_roots` reach them (the calls `native_roots.rs`,
/// `process_references_after_gc` and `release_vm_native_state` make).
#[test]
fn w12c_the_registry_fan_outs_reach_the_vm_scoped_overlay_hooks() {
    use cratonvm_gc::external_roots as er;
    const VM_C: usize = 0xC12C_0001;
    const VM_D: usize = 0xC12C_0002;
    let _registry = common::build_registry();
    let mut ctx_c = vm_ctx(VM_C);
    let mut ctx_d = vm_ctx(VM_D);
    let c = plant(&mut ctx_c);
    let d = plant(&mut ctx_d);

    let mut roots_c = Vec::new();
    er::scan_external_roots_for_vm(VM_C, &mut roots_c);
    assert!(roots_c.contains(&c.list_head));
    assert!(
        !roots_c.contains(&d.list_head),
        "the registry scan is not VM-scoped"
    );

    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(addr(d.list_head), 0x3000);
    er::remap_external_roots_for_vm(VM_C, &pm);
    assert_eq!(__test_ll_get(&ctx_d, d.list, "head"), obj(d.list_head));

    // `None`: the Generational arm, which has no span to screen with.
    er::prune_external_roots_for_vm(VM_C, None, &|_: usize| false);
    assert_eq!(
        __test_ll_get(&ctx_d, d.list, "head"),
        obj(d.list_head),
        "the registry prune condemned another VM's row"
    );

    er::forget_vm_external_roots(VM_D);
    let mut roots_d = Vec::new();
    er::scan_external_roots_for_vm(VM_D, &mut roots_d);
    assert!(
        !roots_d.contains(&d.list_head),
        "teardown did not reach the provider"
    );
}
