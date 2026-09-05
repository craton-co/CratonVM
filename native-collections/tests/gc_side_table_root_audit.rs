// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! Root/identity audit regressions for the collection side tables
//! (2026-08-01, `native-collections-root-audit.md`).
//!
//! Two distinct failure modes are covered here, and they want opposite fixes:
//!
//!  * **Not rooted / not remapped** — a raw `ObjectRef` held across a call
//!    that can run Java. `ConcurrentSkipListMap`'s binary search was the last
//!    one of these in the crate; `tm_binary_search`, `ts_binary_search` and
//!    `pbq_offer_locked` had all been converted, CSLM had not.
//!
//!  * **Not swept** — a side table keyed by `widened_obj_key` whose entries
//!    outlive their collection. `tm_force_array_set` was the only such table
//!    `gc_prune_dead_collection_overlays` did not clear, so it grew without
//!    bound and, once a dead map's 32-bit identity hash was recycled, handed
//!    the new map the dead one's sticky "array mode" flag.
//!
//! Rooting a dead key is a leak; failing to sweep it is stale state a
//! newcomer aliases onto. The two tests below pin down one each.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::MockCtx;
use cratonvm_native_api::NativeContext;
use cratonvm_native_collections::{
    __test_cslm_get, __test_cslm_init_comparator, __test_cslm_put, __test_cslm_size,
    __test_tm_force_array_len, __test_tm_force_array_mode, __test_tm_set_force_array,
    gc_prune_dead_collection_overlays,
};
use cratonvm_types::{ObjectRef, Value};

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

/// Serialises the tests that assert on process-global side-table *counts*.
/// `cargo test` runs a binary's tests on several threads and the overlay
/// tables are `static`, so two counting tests would see each other's entries.
fn side_table_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// 1. ConcurrentSkipListMap — stale `ObjectRef` across the comparator dispatch
// ---------------------------------------------------------------------------

/// `cslm_binary_search` calls `tree_compare`, which dispatches an arbitrary
/// user `Comparator.compare` — a full re-entry into the interpreter that can
/// allocate and complete a moving young collection. Before the fix the search
/// and its callers kept using the pre-dispatch `this` / keys array / values
/// array, so:
///
///   * `set_array_element(keys, pos, key)` wrote through a dead address and
///     was silently dropped, and
///   * `set_field(this, CSLM_FIELD_SIZE, size + 1)` was dropped too, leaving
///     the map claiming its old count.
///
/// `MockCtx::set_relocate_pins_on_invoke(true)` relocates every pinned root on
/// each `invoke_virtual`, which is what a comparator dispatch does. The test
/// pins the map and its backing arrays first — the real VM's
/// `safe_native_call` pins the receiver and arguments the same way, so the
/// objects genuinely do move under a native that is mid-flight.
///
/// NOTE: these natives are deliberately NOT registered (the real
/// `java.util.concurrent.ConcurrentSkipListMap` bytecode runs instead), so the
/// test drives them through the `__test_cslm_*` hooks rather than the
/// registry. The implementation is kept for re-enablement; this keeps its GC
/// discipline honest in the meantime.
#[test]
fn cslm_put_survives_a_comparator_triggered_relocation() {
    let mut ctx = MockCtx::new();
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/ConcurrentSkipListMap")
        .unwrap();
    let map = ctx.alloc_object(cid, 3);
    // A zero-field comparator: `comparator_compare` finds no factory tag and
    // no lambda interface, so it dispatches `compare(Object,Object)` through
    // `invoke_virtual` — the GC point this test is about.
    let cmp = ctx.alloc_object_simple(900);
    __test_cslm_init_comparator(&mut ctx, &[obj(map), obj(cmp)]).unwrap();

    let k1 = ctx.alloc_object_simple(901);
    let v1 = ctx.alloc_object_simple(902);
    let k2 = ctx.alloc_object_simple(903);
    let v2 = ctx.alloc_object_simple(904);
    let k3 = ctx.alloc_object_simple(905);
    let v3 = ctx.alloc_object_simple(906);

    // Seed two entries with no relocation. `put` on an empty map performs no
    // comparison at all; the second performs exactly one.
    __test_cslm_put(&mut ctx, &[obj(map), obj(k1), obj(v1)]).unwrap();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(-1)))]);
    __test_cslm_put(&mut ctx, &[obj(map), obj(k2), obj(v2)]).unwrap();
    assert_eq!(
        __test_cslm_size(&mut ctx, &[obj(map)]).unwrap(),
        Some(Value::Int(2)),
        "baseline: two entries before any relocation"
    );

    // Pin the receiver and its backing arrays, the way the VM pins a native's
    // receiver, so the mock's relocate-on-invoke actually moves them.
    let keys = match ctx.get_field(map, 0) {
        Value::Object(Some(a)) => a,
        other => panic!("expected a keys array, got {other:?}"),
    };
    let values = match ctx.get_field(map, 1) {
        Value::Object(Some(a)) => a,
        other => panic!("expected a values array, got {other:?}"),
    };
    let map_pin = ctx.pin_native_root(map);
    let _keys_pin = ctx.pin_native_root(keys);
    let _values_pin = ctx.pin_native_root(values);

    // Every comparison says "existing key sorts before the new one", so the
    // third entry appends. Supply enough results for the whole search.
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Int(-1))),
        Ok(Some(Value::Int(-1))),
        Ok(Some(Value::Int(-1))),
        Ok(Some(Value::Int(-1))),
    ]);
    ctx.set_relocate_pins_on_invoke(true);
    __test_cslm_put(&mut ctx, &[obj(map), obj(k3), obj(v3)]).unwrap();
    ctx.set_relocate_pins_on_invoke(false);

    // The map moved during the comparator dispatch; read it back through the
    // pin exactly as the fixed native does.
    let map = ctx.read_native_pin(map_pin, map);

    assert_eq!(
        __test_cslm_size(&mut ctx, &[obj(map)]).unwrap(),
        Some(Value::Int(3)),
        "the size write must land on the post-move receiver — before the fix \
         `set_field(this, CSLM_FIELD_SIZE, ..)` went to the pre-move address \
         and was silently dropped, leaving the map at 2"
    );

    // And the entry itself must be readable back. `get` runs its own search
    // over three entries: mid=1 says "sorts before" (walk right), mid=2 says
    // "equal" (found).
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(-1))), Ok(Some(Value::Int(0)))]);
    let got = __test_cslm_get(&mut ctx, &[obj(map), obj(k3)]).unwrap();
    assert!(
        !matches!(got, Some(Value::Object(None))),
        "the stored value must be reachable after the relocation; a dropped \
         `set_array_element` leaves a null hole here"
    );
}

/// The map's own state must survive the relocation intact: a stale `keys`
/// array means the shift/insert wrote nowhere, so the entries seeded *before*
/// the move must still read back.
#[test]
fn cslm_existing_entries_survive_a_relocating_put() {
    let mut ctx = MockCtx::new();
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/ConcurrentSkipListMap")
        .unwrap();
    let map = ctx.alloc_object(cid, 3);
    let cmp = ctx.alloc_object_simple(910);
    __test_cslm_init_comparator(&mut ctx, &[obj(map), obj(cmp)]).unwrap();

    let k1 = ctx.alloc_object_simple(911);
    let v1 = ctx.alloc_object_simple(912);
    __test_cslm_put(&mut ctx, &[obj(map), obj(k1), obj(v1)]).unwrap();

    let keys = match ctx.get_field(map, 0) {
        Value::Object(Some(a)) => a,
        other => panic!("expected a keys array, got {other:?}"),
    };
    let map_pin = ctx.pin_native_root(map);
    let _keys_pin = ctx.pin_native_root(keys);
    let _v1_pin = ctx.pin_native_root(v1);

    let k2 = ctx.alloc_object_simple(913);
    let v2 = ctx.alloc_object_simple(914);
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Int(-1))),
        Ok(Some(Value::Int(-1))),
        Ok(Some(Value::Int(-1))),
    ]);
    ctx.set_relocate_pins_on_invoke(true);
    __test_cslm_put(&mut ctx, &[obj(map), obj(k2), obj(v2)]).unwrap();
    ctx.set_relocate_pins_on_invoke(false);

    let map = ctx.read_native_pin(map_pin, map);
    // Slot 0 must still hold the first key/value pair — the insert appended at
    // slot 1 and must not have clobbered or abandoned the existing entry.
    let keys_now = match ctx.get_field(map, 0) {
        Value::Object(Some(a)) => a,
        other => panic!("keys array lost after relocation: {other:?}"),
    };
    assert!(
        !matches!(ctx.get_array_element(keys_now, 0), Value::Object(None)),
        "the pre-existing key must still be in the backing array; a stale \
         `keys` reference in the shift loop empties it"
    );
    assert_eq!(
        __test_cslm_size(&mut ctx, &[obj(map)]).unwrap(),
        Some(Value::Int(2)),
    );
}

// ---------------------------------------------------------------------------
// 2. `tm_force_array_set` — the side table the GC prune forgot
// ---------------------------------------------------------------------------

/// Every other `widened_obj_key`-keyed TreeMap/TreeSet table is cleared by
/// `gc_prune_dead_collection_overlays`; the sticky array-mode flag was not.
/// It holds no `ObjectRef`, so this is not a use-after-free — the damage is
/// that the table grows for the life of the process and that a recycled
/// identity hash lets a *new* TreeMap inherit a dead one's flag and start life
/// pinned to array mode.
///
/// The right fix here is a sweep, not a root: rooting the key would keep dead
/// maps alive forever, which is precisely the mistake `lhm_heap_backed`
/// documents on the LinkedHashMap side.
/// Runs serially with the CSLM tests above: `gc_prune_dead_collection_overlays`
/// walks the process-global key registry, so a concurrent test minting keys
/// would make the entry count non-deterministic.
#[test]
fn dead_treemap_force_array_flag_is_swept() {
    let _serial = side_table_test_lock();
    let mut ctx = MockCtx::new();
    let live = ctx.alloc_object_simple(0);
    let dead = ctx.alloc_object_simple(0);

    let before = __test_tm_force_array_len();
    __test_tm_set_force_array(&ctx, live);
    __test_tm_set_force_array(&ctx, dead);
    assert_eq!(
        __test_tm_force_array_len(),
        before + 2,
        "both flags must be recorded under distinct keys"
    );
    assert!(__test_tm_force_array_mode(&ctx, live));
    assert!(__test_tm_force_array_mode(&ctx, dead));

    // The collector reports `dead` as unreachable. `is_live` is keyed by raw
    // address, matching the `last_ptr` markers the object-key registry holds.
    let dead_addr = dead.as_ptr() as usize;
    gc_prune_dead_collection_overlays(&|addr: usize| addr != dead_addr);

    // Assert on the entry COUNT, not on `tm_force_array_mode(dead)`: the prune
    // also drops `dead`'s registry slot, so re-deriving its key afterwards
    // mints a fresh generation and a presence check would read `false` even
    // with the sweep missing. Before the fix this table was the one
    // `widened_obj_key`-keyed table the prune skipped and the count stayed at
    // `before + 2`.
    assert_eq!(
        __test_tm_force_array_len(),
        before + 1,
        "a reclaimed TreeMap's sticky array-mode flag must be swept — leaving \
         it both leaks the entry forever and, once its 32-bit identity hash is \
         recycled, silently forces the next map into array mode"
    );
    assert!(
        __test_tm_force_array_mode(&ctx, live),
        "the sweep must not touch a still-live map's flag"
    );
}
