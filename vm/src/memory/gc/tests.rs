use super::*;
use crate::classloading::ClassId;
use crate::memory::heap::Heap;

/// gen r5w3/unload7: the layout-retention scope is per thread, nests by
/// restoring (never by clearing), and outside it the transaction saves
/// nothing — so every stop-the-world caller unloads exactly as before.
#[test]
fn r5w3_layout_retention_is_scoped_to_the_caller() {
    let retaining = || RETAIN_UNLOADED_LAYOUTS.with(std::cell::Cell::get);
    assert!(!retaining());
    with_retained_unloaded_layouts(|| {
        assert!(retaining());
        with_retained_unloaded_layouts(|| assert!(retaining()));
        assert!(retaining(), "a nested scope restores the outer one");
    });
    assert!(!retaining());
    assert!(save_layouts_to_retain(&[ClassId::new(1), ClassId::new(2)]).is_empty());
}

/// Helper to create a test heap with small capacity for GC testing.
fn small_heap() -> Heap {
    // 8 KB total (4 KB per semi-space) — forces GC quickly
    Heap::with_capacity(8 * 1024)
}

/// Test-only `StopTheWorldToken`. Single-threaded test harness.
#[inline]
fn stw() -> cratonvm_gc::collector::StopTheWorldToken {
    unsafe { cratonvm_gc::collector::StopTheWorldToken::new() }
}

#[test]
fn moving_gc_rewrites_live_handle_slots_in_place() {
    // SAFETY: these aligned non-null addresses are never dereferenced.
    let old = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
    let unmoved = unsafe { ObjectRef::from_raw(0x3000usize as *mut u8) };
    let mut slots = vec![Some(old), None, Some(unmoved)];
    let pointer_map = cratonvm_types::PointerMap::from_iter([(0x1000usize, 0x2000usize)]);

    assert_eq!(remap_handle_slots(&mut slots, &pointer_map), 1);
    assert_eq!(slots[0].unwrap().as_ptr() as usize, 0x2000);
    assert!(slots[1].is_none());
    assert_eq!(slots[2].unwrap().as_ptr() as usize, 0x3000);
}

/// The remap half of `roots.rs` §6b. The scan half pushes every cached
/// proxy-dispatch `Method` as an unconditional root, so under a moving
/// young collection the object IS evacuated — and the cache kept serving
/// the from-space address to every later dispatch on the same key. The
/// object Java then received had a zeroed body, so `Method.getName()`
/// returned `null` and Spring's String-switch on it threw
/// `NullPointerException: Cannot invoke "String.hashCode()"`.
///
/// The third entry is the load-bearing one: an UNMOVED cache value must be
/// left exactly as it is. A remap that rewrote every value (rather than
/// only pointer-map keys) would corrupt entries the collector pinned.
#[test]
fn moving_gc_rewrites_the_proxy_method_cache_in_place() {
    // SAFETY: these aligned non-null addresses are never dereferenced —
    // the remap is pure address arithmetic against the pointer map.
    let moved = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
    let also_moved = unsafe { ObjectRef::from_raw(0x2000usize as *mut u8) };
    let pinned = unsafe { ObjectRef::from_raw(0x3000usize as *mut u8) };

    let mut cache: std::collections::HashMap<&'static str, ObjectRef> =
        std::collections::HashMap::new();
    cache.insert("Api.alpha()Ljava/lang/String;", moved);
    cache.insert(
        "Api.bravo(Ljava/lang/String;)Ljava/lang/String;",
        also_moved,
    );
    cache.insert("Api.charlie(II)I", pinned);

    let pointer_map = cratonvm_types::PointerMap::from_iter([
        (0x1000usize, 0x8000usize),
        (0x2000usize, 0x9000usize),
    ]);

    assert_eq!(remap_proxy_method_cache(&mut cache, &pointer_map), 2);
    assert_eq!(
        cache["Api.alpha()Ljava/lang/String;"].as_ptr() as usize,
        0x8000,
        "a relocated cached Method must follow the move, or the next dispatch serves Java a from-space object whose `name` reads null"
    );
    assert_eq!(
        cache["Api.bravo(Ljava/lang/String;)Ljava/lang/String;"].as_ptr() as usize,
        0x9000,
    );
    assert_eq!(
        cache["Api.charlie(II)I"].as_ptr() as usize,
        0x3000,
        "an entry absent from the pointer map did not move and must not be rewritten"
    );
}

/// §10 post-move fixup covers ALL FOUR per-thread single-slot references.
///
/// `jit_pending_exception` is here because it once lived in a
/// `thread_local!` this function could not name, so a moving collection
/// left the interpreter's drain reading a from-space address.
/// `uncaught_exception_pending` is here for the same shape one level up:
/// the launcher held the fatal throwable in a Rust LOCAL across
/// shutdown-hook execution, where neither half could reach it, and a
/// collection inside a hook reclaimed it — the render then read a zeroed
/// header as `ClassId(0)` and printed `Exception in thread "main"
/// java/lang/Object`. See
/// `bug-h2-testopenclose-throwable-is-java-lang-object-FIXED-20260830.md`.
///
/// The count below is the point of the test: a new slot added to
/// `remap_thread_object_slots` without being added here leaves the guard
/// asserting a stale total, which is how a half-wired slot gets in.
#[test]
fn moving_gc_rewrites_every_per_thread_object_slot() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    // SAFETY: these aligned non-null addresses are never dereferenced —
    // the remap is pure address arithmetic against the pointer map.
    let mirror = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
    let jit_exc = unsafe { ObjectRef::from_raw(0x3000usize as *mut u8) };
    let uncaught = unsafe { ObjectRef::from_raw(0x4000usize as *mut u8) };

    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.java_thread_obj = Some(mirror);
    thread.jit_pending_exception = Some(jit_exc);
    thread.uncaught_exception_pending = Some(uncaught);

    let pointer_map = cratonvm_types::PointerMap::from_iter([
        (0x1000usize, 0x8000usize),
        (0x3000usize, 0xA000usize),
        (0x4000usize, 0xB000usize),
    ]);

    assert_eq!(remap_thread_object_slots(&mut thread, &pointer_map), 3);
    assert_eq!(
        thread.uncaught_exception_pending.unwrap().as_ptr() as usize,
        0xB000,
        "the launcher's parked throwable must come back at its post-move address"
    );
    assert_eq!(
        thread.java_thread_obj.unwrap().as_ptr() as usize,
        0x8000,
        "java_thread_obj must follow the move"
    );
    assert_eq!(
        thread.jit_pending_exception.unwrap().as_ptr() as usize,
        0xA000,
        "the JIT's pending throwable must follow the move — a stale address \
         here is handed straight to the interpreter's post-JIT drain"
    );
}

/// A slot whose object did not move, and an empty slot, must both come
/// through untouched — the remap must not invent or drop references.
#[test]
fn per_thread_object_slot_remap_leaves_unmoved_and_empty_slots_alone() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    // SAFETY: never dereferenced; see the sibling test.
    let unmoved = unsafe { ObjectRef::from_raw(0x5000usize as *mut u8) };

    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.jit_pending_exception = Some(unmoved);

    // A non-empty map that simply does not mention our object.
    let pointer_map = cratonvm_types::PointerMap::from_iter([(0x1000usize, 0x8000usize)]);

    assert_eq!(remap_thread_object_slots(&mut thread, &pointer_map), 0);
    assert_eq!(
        thread.jit_pending_exception.unwrap().as_ptr() as usize,
        0x5000
    );
    assert!(thread.java_thread_obj.is_none());
}

/// End-to-end: stash a JIT pending exception on the thread, run a *real*
/// moving collection with that object as the only root, and check the
/// thread's slot now names the object's post-copy address — and that the
/// object is genuinely still there (its field survives the copy).
///
/// This is the shape the bug had: the reference is parked in a slot while a
/// collection relocates the object, and the drain that follows must not see
/// the pre-move address.
#[test]
fn jit_pending_exception_survives_and_follows_a_moving_collection() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let heap = small_heap();
    let monitor_table = crate::threading::monitor::MonitorTable::new();

    // Stand in for the throwable the JIT helper would have constructed.
    let exc = heap.alloc_object(ClassId::new(7), 1);
    heap.set_field(exc, 0, Value::Int(0x5EED));

    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.jit_pending_exception = Some(exc);

    // The scan half: §10 of `roots.rs` pushes this slot. Model it directly
    // (that file is the collector's root list, not this module's) so the
    // object is reachable for the copy.
    let mut roots = vec![exc];
    let result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

    let moved_to = roots[0];
    assert_ne!(
        moved_to.as_ptr(),
        exc.as_ptr(),
        "the collection must actually relocate the object, or this test \
         proves nothing about the remap"
    );

    // The remap half: what this module owns.
    remap_thread_object_slots(&mut thread, &result.pointer_map);

    let drained = thread
        .jit_pending_exception
        .take()
        .expect("the pending exception must survive the collection");
    assert_eq!(
        drained.as_ptr(),
        moved_to.as_ptr(),
        "the drain must see the post-move address"
    );
    assert_eq!(
        heap.get_field(drained, 0).as_int(),
        Some(0x5EED),
        "the relocated throwable must still be a live, readable object"
    );
}

/// Fake, never-dereferenced object address. Below 64 KiB, so it can never
/// coincide with a real heap address in this process — which matters for
/// the `update_all_roots` test below, whose remap also fans out over
/// process-global side tables shared with concurrently running tests.
fn fake(addr: usize) -> ObjectRef {
    // SAFETY: used only as an address; never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// **EVERY OFF-FRAME PER-THREAD FAMILY GOES THROUGH THE SHARED LIST.**
///
/// The count is the guard: a family added to `JvmThread` and to this
/// function's scan half but not here leaves the total wrong. The families
/// the four hand-maintained lists disagreed on are the load-bearing ones —
/// `native_alloc_pool` (missing from the initiator) and the two parked
/// throwables (missing from every peer path).
#[test]
fn the_off_frame_remap_covers_every_per_thread_family() {
    use crate::threading::jvm_thread::{
        JitHashMapStringNodeCacheEntry, JvmThread, StringCaseCacheEntry, ThreadId,
    };

    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.java_thread_obj = Some(fake(0x1000));
    thread.jit_pending_exception = Some(fake(0x1200));
    thread.uncaught_exception_pending = Some(fake(0x1300));
    thread.printed.push(Value::Object(Some(fake(0x1400))));
    thread.native_pin_roots.push(fake(0x1500));
    thread.native_alloc_pool.push(fake(0x1600));
    thread.handle_slots.push(Some(fake(0x1700)));
    thread.native_pending_return = Some(fake(0x1800));
    thread
        .jit_hashmap_string_node_cache
        .push(JitHashMapStringNodeCacheEntry {
            map: fake(0x1900),
            node: fake(0x1A00),
            key_object: Some(fake(0x1B00)),
            key: String::new(),
            mod_count_slot: 0,
            mod_count: 0,
            chm_generation: None,
            chm_segment_id: None,
        });
    thread.string_case_cache.push(StringCaseCacheEntry {
        source: fake(0x1C00),
        locale: Some(fake(0x1D00)),
        upper: false,
        first: fake(0x1E00),
        second: fake(0x1F00),
        next: false,
    });
    thread
        .scoped_values
        .push((1, Some(fake(0x2000)), Value::Object(Some(fake(0x2100)))));

    // Every address above moves by +0x8000.
    let pointer_map: cratonvm_types::PointerMap = (0x10..=0x21)
        .map(|k| (k * 0x100, k * 0x100 + 0x8000))
        .collect();

    assert_eq!(
        remap_thread_off_frame_refs(&mut thread, &pointer_map),
        17,
        "every off-frame reference must be forwarded exactly once"
    );
    let moved = |o: ObjectRef| o.as_ptr() as usize >= 0x8000;
    assert!(moved(thread.native_alloc_pool[0]), "native_alloc_pool");
    assert!(
        moved(thread.jit_pending_exception.unwrap()),
        "jit_pending_exception"
    );
    assert!(
        moved(thread.uncaught_exception_pending.unwrap()),
        "uncaught_exception_pending"
    );
    assert!(moved(thread.handle_slots[0].unwrap()), "handle_slots");
    assert!(
        moved(thread.string_case_cache[0].locale.unwrap()),
        "case cache locale"
    );
    assert!(
        moved(thread.jit_hashmap_string_node_cache[0].key_object.unwrap()),
        "hashmap cache key"
    );
    assert!(
        moved(thread.scoped_values[0].1.unwrap()),
        "scoped-value key"
    );
    match thread.scoped_values[0].2 {
        Value::Object(Some(o)) => assert!(moved(o), "scoped-value value"),
        ref other => panic!("scoped value changed kind: {other:?}"),
    }
}

/// **THE SCAN AND THE REMAP NAME EXACTLY THE SAME REFERENCES** (gc-common
/// w3-b, step 1 of `docs/feature-designs/real-jdk-mode.md`).
///
/// Every per-thread field family is populated with a distinct address; the
/// scan (`roots::push_off_frame_thread_roots`, what `collect_roots` §4 now
/// calls) must report exactly that set, and the remap
/// (`remap_thread_off_frame_refs`) must rewrite exactly that many. A family
/// in one list and not the other fails here: scan-only is a root the post-
/// move fix-up leaves at the vacated address, remap-only is an object the
/// collector frees while the thread still names it.
#[test]
fn thread_field_scan_and_remap_cover_the_same_references() {
    use crate::threading::jvm_thread::{
        JitHashMapStringNodeCacheEntry, JvmThread, StringCaseCacheEntry, ThreadId,
    };

    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.java_thread_obj = Some(fake(0x2200));
    thread.jit_pending_exception = Some(fake(0x2400));
    thread.uncaught_exception_pending = Some(fake(0x2500));
    thread.printed.push(Value::Object(Some(fake(0x2600))));
    thread.native_pin_roots.push(fake(0x2700));
    thread.native_alloc_pool.push(fake(0x2800));
    thread.handle_slots.push(Some(fake(0x2900)));
    thread.handle_slots.push(None);
    thread.native_pending_return = Some(fake(0x2A00));
    thread
        .jit_hashmap_string_node_cache
        .push(JitHashMapStringNodeCacheEntry {
            map: fake(0x2B00),
            node: fake(0x2C00),
            key_object: Some(fake(0x2D00)),
            key: String::new(),
            mod_count_slot: 0,
            mod_count: 0,
            chm_generation: None,
            chm_segment_id: None,
        });
    thread.string_case_cache.push(StringCaseCacheEntry {
        source: fake(0x2E00),
        locale: Some(fake(0x2F00)),
        upper: false,
        first: fake(0x3000),
        second: fake(0x3100),
        next: false,
    });
    thread
        .scoped_values
        .push((1, Some(fake(0x3200)), Value::Object(Some(fake(0x3300)))));
    // 0x2300 was `pending_async_exception`, removed with its dead channel.
    let populated: Vec<usize> = (0x22..=0x33)
        .filter(|&k| k != 0x23)
        .map(|k| k * 0x100)
        .collect();

    let mut scanned: Vec<ObjectRef> = Vec::new();
    crate::memory::roots::push_off_frame_thread_roots(&thread, &mut scanned);
    let mut scanned: Vec<usize> = scanned.iter().map(|o| o.as_ptr() as usize).collect();
    scanned.sort_unstable();
    assert_eq!(
        scanned, populated,
        "the scan must report every populated per-thread field reference, once"
    );

    let pointer_map: cratonvm_types::PointerMap =
        populated.iter().map(|&a| (a, a + 0x8000)).collect();
    assert_eq!(
        remap_thread_off_frame_refs(&mut thread, &pointer_map),
        populated.len(),
        "the remap must forward exactly the references the scan roots"
    );
}

/// **A CHAINED MAP IS APPLIED ONCE, NOT FOLLOWED.**
///
/// A sliding compactor routinely emits `A -> B` and `B -> C` in one map (a
/// survivor slides into the space a predecessor vacated). A reference that
/// named `A` must end at `B`; ending at `C` would name the OTHER object.
/// This is why `update_all_roots` must not run the shared off-frame list
/// and the old per-family blocks both.
#[test]
fn update_all_roots_applies_a_chained_map_to_off_frame_refs_exactly_once() {
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Deliberately odd sub-64-KiB addresses: `update_all_roots` also remaps
    // process-global side tables, and these must not collide with a fake
    // address some concurrently running test parked in one of them.
    const A: usize = 0xC3A0;
    const B: usize = 0xC3A8;
    const C: usize = 0xC3B0;
    thread.jit_pending_exception = Some(fake(A));
    thread.native_alloc_pool.push(fake(A));
    thread.printed.push(Value::Object(Some(fake(B))));
    thread.root_snapshot.lock().push(fake(A));

    let pointer_map = cratonvm_types::PointerMap::from_iter([
        (A, B), // A -> B
        (B, C), // B -> C (B's previous occupant)
    ]);
    update_all_roots(
        &shared,
        &mut thread,
        &pointer_map,
        &crate::memory::addr_keyed::RelocationTargets::for_pause(
            &shared.mem.heap,
            &pointer_map,
        ),
    );

    let at = |o: ObjectRef| o.as_ptr() as usize;
    assert_eq!(at(thread.jit_pending_exception.unwrap()), B);
    assert_eq!(at(thread.native_alloc_pool[0]), B);
    match thread.printed[0] {
        Value::Object(Some(o)) => assert_eq!(at(o), C),
        ref other => panic!("printed value changed kind: {other:?}"),
    }
    assert_eq!(at(thread.root_snapshot.lock()[0]), B);
}

/// **A PARKED PEER FORWARDS THE SAME LIST, AND ITS SNAPSHOT.**
///
/// `apply_pointer_map_to_thread` carried its own copy of the off-frame list
/// and missed both parked throwables; it also left `root_snapshot` at the
/// pre-move addresses, which the next pause's takeover reads if this thread
/// never polls again.
#[test]
fn the_safepoint_resume_remap_forwards_throwables_and_the_snapshot() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.jit_pending_exception = Some(fake(0x4000));
    thread.uncaught_exception_pending = Some(fake(0x4100));
    thread.native_alloc_pool.push(fake(0x4200));
    thread.root_snapshot.lock().push(fake(0x4300));

    let pointer_map = cratonvm_types::PointerMap::from_iter([
        (0x4000usize, 0x9000usize),
        (0x4100usize, 0x9100usize),
        (0x4200usize, 0x9200usize),
        (0x4300usize, 0x9300usize),
    ]);
    crate::runtime::interpreter::apply_pointer_map_to_thread(
        &mut thread,
        &pointer_map,
        &heap,
        &cratonvm_classloading::TypeMapStore::new(),
    );

    let at = |o: ObjectRef| o.as_ptr() as usize;
    assert_eq!(at(thread.jit_pending_exception.unwrap()), 0x9000);
    assert_eq!(at(thread.uncaught_exception_pending.unwrap()), 0x9100);
    assert_eq!(at(thread.native_alloc_pool[0]), 0x9200);
    assert_eq!(at(thread.root_snapshot.lock()[0]), 0x9300);
}

/// The reverse-mirror re-key removes EVERY old key before inserting ANY new
/// one, so a chained move (`A -> B`, `B -> C`) cannot have its second
/// removal delete the first insertion.
#[test]
fn reverse_mirror_rekey_survives_a_chained_map() {
    use crate::classloading::ClassId;
    let (c1, c2) = (ClassId::new(11), ClassId::new(12));
    let mut reverse: rustc_hash::FxHashMap<ObjectRef, ClassId> = Default::default();
    reverse.insert(fake(0x5000), c1); // A
    reverse.insert(fake(0x5100), c2); // B

    let moved = [
        (c1, 0x5000usize, 0x5100usize),
        (c2, 0x5100usize, 0x5200usize),
    ];
    assert!(rekey_reverse_mirrors(&mut reverse, &moved, 2));
    assert_eq!(reverse.get(&fake(0x5100)), Some(&c1));
    assert_eq!(reverse.get(&fake(0x5200)), Some(&c2));
    assert!(!reverse.contains_key(&fake(0x5000)));
}

/// A reverse map that had drifted from the forward one is reported, so the
/// caller falls back to the full rebuild.
#[test]
fn reverse_mirror_rekey_reports_drift() {
    use crate::classloading::ClassId;
    let mut reverse: rustc_hash::FxHashMap<ObjectRef, ClassId> = Default::default();
    reverse.insert(fake(0x6000), ClassId::new(21));
    reverse.insert(fake(0x6100), ClassId::new(22)); // stale extra row
    let moved = [(ClassId::new(21), 0x6000usize, 0x6800usize)];
    assert!(
        !rekey_reverse_mirrors(&mut reverse, &moved, 1),
        "a forward map of one entry cannot be mirrored by two reverse rows"
    );
}

/// gc-common w8-e: every loader-conditional table leaves with the classes.
///
/// Each table below defers its values to the defining loader's
/// `metadata_pin` row instead of rooting them. When the loader dies, those
/// values are not marked. So the unload transaction must drop the rows of
/// the classes it unloads. A row left behind names a reclaimed object, and
/// with the loader pin gone the next root scan roots that address. The
/// `ClassValue` memo cache was missing from this list: its forget function
/// existed but had no caller. This is a source check because the unit-test
/// VM cannot fill that cache (the `ClassValue.get` native calls a real
/// `computeValue`).
#[test]
fn the_unload_transaction_forgets_every_loader_conditional_table() {
    let src = include_str!("../gc.rs");
    let start = src
        .find("pub fn unload_dead_class_metadata(")
        .expect("the unload transaction");
    let len = src[start..].find("\n}\n").expect("its closing brace");
    let body = &src[start..start + len];
    for (table, needle) in [
        ("statics (roots.rs step 2)", "statics_index.unpublish("),
        ("class locks (step 3)", ".class_locks"),
        ("decoded SourceFile strings (step 6c)", ".stack_trace_source_files"),
        (
            "condy values (step 13)",
            ".forget_classes(|id| gone.contains(&id))",
        ),
        // Not a deferral: the reflective link cache, which the unload path
        // evicts itself since the per-class resolution hook stopped firing
        // for an unload (w8-e), in one batch pass (w9-e).
        (
            "LinkResolver records",
            "link_resolver.invalidate_classes(&gone)",
        ),
        // Not a deferral either: a ClassId-keyed JIT memo with no other
        // unload eviction (w9-e).
        ("method-entry gate memo", ".jit_gate_pass"),
        // Nor this: a ClassId-keyed retransform record (wave 123, L3).
        (
            "installed retransform files",
            ".installed_retransform_files",
        ),
        ("proxy Methods (step 6b)", ".proxy_method_cache"),
        ("class mirrors (step 6)", ".class_mirrors"),
        (
            "generic indy call sites",
            "forget_unloaded_generic_indy_sites(",
        ),
        ("ObjectStreamClass descriptors", "osc_cache.remove_classes("),
        ("annotation proxies", "forget_unloaded_annotation_proxies("),
        (
            "reflection-door outer-resolution failures and spoof verdicts",
            "forget_unloaded_reflect_door_rows(",
        ),
        (
            "Class$Atomic reflection slots",
            "prune_class_atomic_side_store(",
        ),
        (
            "ClassValue memo cache",
            "forget_unloaded_classvalue_entries(",
        ),
        ("bound MethodType class-keyed rows", ".bound_method_types"),
        // JIT round 22 wave 1, lane review: the init-error rows and the
        // global handle on their saved ExceptionInInitializerError.
        ("class init-error rows and saved causes", ".class_init_errors"),
    ] {
        assert!(
            body.contains(needle),
            "unload_dead_class_metadata no longer drops the {table} rows (`{needle}`)"
        );
    }
}

/// gc-common w9-e: the unload transaction drops the unloaded class's
/// method-entry gate memo rows (`JitRealm::jit_gate_pass`), which nothing
/// evicted before, and leaves a live class's rows alone. Skips cleanly when
/// the unit-test VM cannot define a class (no `java/lang/Object` to link
/// against).
#[test]
fn the_unload_transaction_drops_the_gate_memo_rows_of_unloaded_classes() {
    use crate::classloading::ClassLoaderId;
    use crate::config::VmConfig;
    use std::sync::Arc;

    fn class_bytes(this_name: &str) -> Vec<u8> {
        let super_name = "java/lang/Object";
        let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
        b.extend_from_slice(&[0x00, 0x05]);
        b.push(7);
        b.extend_from_slice(&[0x00, 0x02]);
        b.push(1);
        b.extend_from_slice(&u16::try_from(this_name.len()).unwrap().to_be_bytes());
        b.extend_from_slice(this_name.as_bytes());
        b.push(7);
        b.extend_from_slice(&[0x00, 0x04]);
        b.push(1);
        b.extend_from_slice(&u16::try_from(super_name.len()).unwrap().to_be_bytes());
        b.extend_from_slice(super_name.as_bytes());
        // public super, this=#1, super=#3, no interfaces/fields/methods/attrs
        b.extend_from_slice(&[0x00, 0x21, 0x00, 0x01, 0x00, 0x03]);
        b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        b
    }

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let define = |name: &str, loader: u32| {
        shared
            .classes
            .class_manager_write()
            .define_class_with_options(
                name,
                &class_bytes(name),
                ClassLoaderId::UserDefined(loader),
                cratonvm_classloading::DefineClassOptions {
                    skip_verification: true,
                    ..Default::default()
                },
            )
    };
    let (Ok(gone), Ok(kept)) = (define("w9e/GateGone", 9_901), define("w9e/GateKept", 9_902))
    else {
        eprintln!("skipped: this unit-test VM cannot define a user-loader class");
        return;
    };
    let row = |id| (id, Arc::<str>::from("run"), Arc::<str>::from("()V"));
    {
        let mut memo = shared.jit.jit_gate_pass.write();
        memo.insert(row(gone), (0, false));
        memo.insert(row(kept), (0, false));
    }

    shared.classes.stack_trace_source_file(gone, Some("Gone.java"));
    shared.classes.stack_trace_source_file(kept, Some("Kept.java"));
    let result = unload_dead_class_metadata(&shared, &[gone.as_u32()]);

    assert_eq!(result.classes_unloaded, 1);
    assert!(!shared.classes.stack_trace_source_files.read().contains_key(&gone));
    assert!(shared.classes.stack_trace_source_files.read().contains_key(&kept));
    let memo = shared.jit.jit_gate_pass.read();
    assert!(
        !memo.contains_key(&row(gone)),
        "the unloaded class's gate memo row must go with it"
    );
    assert!(
        memo.contains_key(&row(kept)),
        "a live class's gate memo row must be kept"
    );
}

/// A class file with no members: `this_name extends java/lang/Object`.
fn w11e_class_bytes(this_name: &str) -> Vec<u8> {
    let super_name = "java/lang/Object";
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
    b.extend_from_slice(&[0x00, 0x05]);
    b.push(7);
    b.extend_from_slice(&[0x00, 0x02]);
    b.push(1);
    b.extend_from_slice(&u16::try_from(this_name.len()).unwrap().to_be_bytes());
    b.extend_from_slice(this_name.as_bytes());
    b.push(7);
    b.extend_from_slice(&[0x00, 0x04]);
    b.push(1);
    b.extend_from_slice(&u16::try_from(super_name.len()).unwrap().to_be_bytes());
    b.extend_from_slice(super_name.as_bytes());
    b.extend_from_slice(&[0x00, 0x21, 0x00, 0x01, 0x00, 0x03]);
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    b
}

/// Define `name` under `loader` in `shared`, skipping verification.
fn w11e_define(
    shared: &crate::vm::SharedVm,
    name: &str,
    loader: crate::classloading::ClassLoaderId,
) -> Option<crate::classloading::ClassId> {
    shared
        .classes
        .class_manager_write()
        .define_class_with_options(
            name,
            &w11e_class_bytes(name),
            loader,
            cratonvm_classloading::DefineClassOptions {
                skip_verification: true,
                ..Default::default()
            },
        )
        .ok()
}

/// Give each `(class id, loader address)` a defining-loader row, then run
/// the reconcile with every one of those loaders dead, so each id gets an
/// orphan marker. Returns the reconcile's dead ids (the unload hints).
fn w11e_orphan(shared: &crate::vm::SharedVm, rows: &[(u32, usize)]) -> Vec<u32> {
    use cratonvm_native_builtins::classloader as cl;
    for &(id, loader) in rows {
        cl::register_defining_loader(&shared.loader_identity, id, fake(loader));
    }
    let dead: Vec<usize> = rows.iter().map(|&(_, loader)| loader).collect();
    let hints = cl::gc_reconcile_defining_loaders(
        &shared.loader_identity,
        &|addr: usize| !dead.contains(&addr),
        &cratonvm_types::PointerMap::default(),
        None,
    );
    for &(id, _) in rows {
        assert!(
            cl::is_defining_loader_orphaned(&shared.loader_identity, id),
            "setup: {id} marked"
        );
    }
    hints
}

/// gc-common w11-e
/// (`common-w10f-unload-early-exits-drop-orphan-markers-of-live-classes`),
/// first early exit: no hint is in a user namespace. A class still in the
/// store under a built-in namespace keeps its orphan marker (it used to
/// lose it here, and so became visible to every loader). A hint with no
/// class in the store loses its marker. Skips cleanly when the unit-test
/// VM cannot define a class.
#[test]
fn the_unload_early_exit_keeps_the_orphan_marker_of_a_class_still_loaded() {
    use crate::classloading::ClassLoaderId;
    use crate::config::VmConfig;
    use cratonvm_native_builtins::classloader::is_defining_loader_orphaned;
    // Above the defining-loader key-set cap, and never a class here.
    const ABSENT: u32 = 2_011_101;

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let vm = shared.vm_identity;
    let Some(kept) = w11e_define(&shared, "w11e/EarlyExitKept", ClassLoaderId::Application)
    else {
        eprintln!("skipped: this unit-test VM cannot define an application class");
        return;
    };
    let hints = w11e_orphan(
        &shared,
        &[(kept.as_u32(), 0x11E_5000), (ABSENT, 0x11E_5100)],
    );

    let result = unload_dead_class_metadata(&shared, &hints);

    assert_eq!(result, ClassMetadataUnloadResult::default());
    assert!(
        is_defining_loader_orphaned(&shared.loader_identity, kept.as_u32()),
        "a class still loaded under a built-in namespace keeps its orphan marker"
    );
    assert!(
        !is_defining_loader_orphaned(&shared.loader_identity, ABSENT),
        "a hint whose class is gone loses its orphan marker"
    );
    cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm);
}

/// gc-common w11-e, the main path, and the unchanged common case: the
/// unloaded user-namespace class loses its marker as before, a
/// built-in-namespace class of the same dead loader keeps it (it did
/// here before too), and a hint already gone from the store loses it.
#[test]
fn the_unload_main_path_forgets_only_the_markers_of_classes_gone_from_the_store() {
    use crate::classloading::ClassLoaderId;
    use crate::config::VmConfig;
    use cratonvm_native_builtins::classloader::is_defining_loader_orphaned;
    const ABSENT: u32 = 2_011_201;

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let vm = shared.vm_identity;
    let (dead_ns, live_ns) = (
        ClassLoaderId::UserDefined(11_901),
        ClassLoaderId::UserDefined(11_902),
    );
    let (Some(gone), Some(kept), Some(_live)) = (
        w11e_define(&shared, "w11e/MainGone", dead_ns),
        w11e_define(&shared, "w11e/MainKept", ClassLoaderId::Application),
        w11e_define(&shared, "w11e/MainLive", live_ns),
    ) else {
        eprintln!("skipped: this unit-test VM cannot define a class");
        return;
    };
    // Initiating-resolution memo rows naming a class that stays loaded:
    // one under the namespace this unload empties, one under a live one.
    {
        let mut cache = shared.classes.initiating_resolution_cache.write();
        for ns in [dead_ns, live_ns] {
            cache
                .entry(ns)
                .or_default()
                .insert(std::sync::Arc::<str>::from("w11e/MainKept"), kept);
        }
    }
    let hints = w11e_orphan(
        &shared,
        &[
            (gone.as_u32(), 0x11E_6000),
            (kept.as_u32(), 0x11E_6000),
            (ABSENT, 0x11E_6000),
        ],
    );

    let result = unload_dead_class_metadata(&shared, &hints);

    assert_eq!(result.classes_unloaded, 1);
    assert!(!is_defining_loader_orphaned(&shared.loader_identity, gone.as_u32()));
    assert!(is_defining_loader_orphaned(&shared.loader_identity, kept.as_u32()));
    assert!(!is_defining_loader_orphaned(&shared.loader_identity, ABSENT));
    let cache = shared.classes.initiating_resolution_cache.read();
    assert!(
        !cache.contains_key(&dead_ns),
        "the memo of a namespace left with no class goes whole"
    );
    assert_eq!(
        cache
            .get(&live_ns)
            .and_then(|m| m.get("w11e/MainKept"))
            .copied(),
        Some(kept),
        "a live namespace's row naming a loaded class stays"
    );
    drop(cache);
    cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm);
}

/// gc-common w18-d: a non-strong hidden class of the APPLICATION
/// namespace whose mirror died is unloaded by the transaction, with its
/// metaspace charge given back, while a plain application class stays.
#[test]
fn a_dead_non_strong_hidden_class_of_the_application_namespace_unloads() {
    use crate::classloading::ClassLoaderId;
    use crate::config::VmConfig;
    use cratonvm_native_builtins::classloader as cl;

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let vm = shared.vm_identity;
    let hidden = shared
        .classes
        .class_manager_write()
        .define_class_with_options(
            "w18d/GcPayload/0x1",
            &w11e_class_bytes("w18d/GcPayload"),
            ClassLoaderId::Application,
            cratonvm_classloading::DefineClassOptions {
                override_name: Some("w18d/GcPayload/0x1".to_string()),
                hidden: true,
                skip_verification: true,
                ..Default::default()
            },
        )
        .ok();
    let (Some(hidden), Some(plain)) = (
        hidden,
        w11e_define(&shared, "w18d/GcPlain", ClassLoaderId::Application),
    ) else {
        eprintln!("skipped: this unit-test VM cannot define an application class");
        return;
    };
    let charged = shared
        .classes
        .class_manager
        .read()
        .metaspace_charged_bytes();
    let mirror: usize = 0x18D_9000;
    cl::register_non_strong_hidden_mirror(&shared.loader_identity, hidden.as_u32(), mirror, None);
    let hints = cl::gc_reconcile_defining_loaders(
        &shared.loader_identity,
        &|addr: usize| addr != mirror,
        &cratonvm_types::PointerMap::default(),
        None,
    );
    assert_eq!(hints, vec![hidden.as_u32()]);

    let result = unload_dead_class_metadata(&shared, &hints);

    assert_eq!(result.classes_unloaded, 1);
    assert_eq!(result.loaders_unloaded, 0, "the application loader lives");
    let cm = shared.classes.class_manager.read();
    assert!(cm.get_class(hidden).is_none());
    assert!(cm.get_class(plain).is_some());
    assert!(
        cm.metaspace_charged_bytes() < charged,
        "the charge is given back"
    );
    drop(cm);
    assert_eq!(cl::non_strong_hidden_mirror(&shared.loader_identity, hidden.as_u32()), None);
    cl::forget_vm_loader_singletons(vm);
}

/// gc-common w11-e: the rule every exit uses, including the second early
/// exit (user-namespace hints, none removed), which a unit test cannot
/// reach through the transaction itself: only a hint whose class is not
/// loaded is forgotten, in hint order.
#[test]
fn only_hints_no_longer_loaded_lose_their_markers() {
    let loaded = [3u32, 7];
    assert_eq!(
        hints_no_longer_loaded(&[3, 5, 7, 9], |id| loaded.contains(&id)),
        vec![5, 9]
    );
    assert!(hints_no_longer_loaded(&[3, 7], |id| loaded.contains(&id)).is_empty());
    assert_eq!(hints_no_longer_loaded(&[4], |_| false), vec![4]);
}

/// **A DEAD MIRROR LEAVES THE REVERSE MAP WITH THE FORWARD ONE.**
///
/// On a non-moving cycle nothing else rebuilds the reverse map, so a row
/// left here keys the reclaimed address to its old `ClassId` and
/// `class_id_from_mirror` answers it for the next object allocated there.
#[test]
fn reconcile_class_mirrors_prunes_the_reverse_map_too() {
    use crate::classloading::ClassId;
    use crate::config::VmConfig;

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let dead = fake(0x7000);
    let live = fake(0x7100);
    let (dead_id, live_id) = (ClassId::new(900_001), ClassId::new(900_002));
    shared.classes.class_mirrors.write().insert(dead_id, dead);
    shared.classes.class_mirrors.write().insert(live_id, live);
    shared
        .classes
        .class_mirrors_reverse
        .write()
        .insert(dead, dead_id);
    shared
        .classes
        .class_mirrors_reverse
        .write()
        .insert(live, live_id);

    shared.classes.stack_trace_source_file(dead_id, Some("Dead.java"));
    shared.classes.stack_trace_source_file(live_id, Some("Live.java"));
    shared.classes.retain_stack_trace_source_file(dead_id, "Dead.java", fake(0x7300));
    shared.classes.retain_stack_trace_source_file(live_id, "Live.java", fake(0x7400));
    // Everything the VM itself created stays marked; only `dead` is not.
    reconcile_class_mirrors(&shared, &|addr| addr != 0x7000, None);
    assert!(!shared.classes.stack_trace_source_files.read().contains_key(&dead_id));
    assert_eq!(shared.classes.stack_trace_source_files.read()[&live_id].string, Some(fake(0x7400)));

    assert!(!shared.classes.class_mirrors.read().contains_key(&dead_id));
    assert!(
        !shared
            .classes
            .class_mirrors_reverse
            .read()
            .contains_key(&dead),
        "the reverse row of a pruned mirror must go with it"
    );
    assert_eq!(
        shared.classes.class_mirrors_reverse.read().get(&live),
        Some(&live_id),
        "a surviving mirror's reverse row must be untouched"
    );
}

/// nio20 L3 (NIO19-5): the compiled-`getClass()` words track
/// `class_mirrors` through both collector writers -- a moved mirror's word
/// follows it in `update_all_roots`, a word never published stays `0`, and
/// a dead mirror's word goes with it in `reconcile_class_mirrors`.
#[test]
fn getclass_mirror_words_follow_moves_and_die_with_their_mirror() {
    use crate::classloading::ClassId;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let words = &shared.classes.getclass_mirror_words;
    let _ = words.base_addr();
    // Odd sub-64-KiB addresses, as in the chained-map test above, and
    // class ids below the table's capacity no VM-created class reaches.
    let (moved_id, quiet_id, dead_id, live_id) = (
        ClassId::new(61_001),
        ClassId::new(61_002),
        ClassId::new(61_003),
        ClassId::new(61_004),
    );
    const MOVED: usize = 0xD4A0;
    const MOVED_TO: usize = 0xD4A8;
    const QUIET: usize = 0xD4B0;
    const QUIET_TO: usize = 0xD4B8;
    const DEAD: usize = 0xD4C0;
    const LIVE: usize = 0xD4C8;
    {
        let mut mirrors = shared.classes.class_mirrors.write();
        mirrors.insert(moved_id, fake(MOVED));
        mirrors.insert(quiet_id, fake(QUIET));
        mirrors.insert(dead_id, fake(DEAD));
        mirrors.insert(live_id, fake(LIVE));
    }
    words.publish(moved_id, fake(MOVED));
    words.publish(dead_id, fake(DEAD));
    words.publish(live_id, fake(LIVE));

    let pointer_map =
        cratonvm_types::PointerMap::from_iter([(MOVED, MOVED_TO), (QUIET, QUIET_TO)]);
    update_all_roots(
        &shared,
        &mut thread,
        &pointer_map,
        &crate::memory::addr_keyed::RelocationTargets::for_pause(
            &shared.mem.heap,
            &pointer_map,
        ),
    );
    assert_eq!(
        words.get(moved_id),
        MOVED_TO,
        "a published word follows its mirror"
    );
    assert_eq!(words.get(quiet_id), 0, "an unpublished word stays 0");
    assert_eq!(words.get(live_id), LIVE, "an unmoved word is untouched");

    reconcile_class_mirrors(&shared, &|addr| addr != DEAD, None);
    assert_eq!(words.get(dead_id), 0, "a dead mirror's word goes with it");
    assert_eq!(words.get(live_id), LIVE);
    assert_eq!(words.get(moved_id), MOVED_TO);
}

/// gc-common w2-b: a proxy-dispatch `Method` that did not survive the
/// cycle takes its `proxy_method_cache` row with it (roots.rs step 6b now
/// pins a user-loader proxy's `Method` to its loader instead of rooting it,
/// so it CAN die), and the reconciliation is where that happens.
#[test]
fn reconcile_class_mirrors_prunes_dead_proxy_methods() {
    use crate::classloading::ClassId;
    use crate::config::VmConfig;

    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let dead = fake(0x7200);
    let live = fake(0x7300);
    {
        let mut cache = shared.classes.proxy_method_cache.write();
        cache.insert((ClassId::new(900_011), "go".into(), "()V".into()), dead);
        cache.insert((ClassId::new(900_012), "go".into(), "()V".into()), live);
    }
    reconcile_class_mirrors(&shared, &|addr| addr != 0x7200, None);
    let cache = shared.classes.proxy_method_cache.read();
    assert_eq!(cache.len(), 1, "the dead Method's row must be dropped");
    assert!(
        cache.values().any(|m| *m == live),
        "the live row must be kept"
    );
}

#[test]
fn gc_basic_copy_single_object() {
    let heap = small_heap();
    let obj = heap.alloc_object(ClassId::new(1), 2);
    heap.set_field(obj, 0, Value::Int(42));
    heap.set_field(obj, 1, Value::Long(100));

    let mut roots = vec![obj];
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);

    assert_eq!(result.stats.objects_copied, 1);

    // Root should be updated
    let new_obj = roots[0];
    assert_ne!(new_obj.as_ptr(), obj.as_ptr());

    // Read fields from to-space
    let header = unsafe { &*(new_obj.as_ptr() as *const crate::memory::heap::ObjectHeader) };
    assert_eq!(header.class_id, ClassId::new(1));
    assert_eq!(header.num_slots(), 2);

    // Read field values from to-space via raw pointers
    let field0_ptr = unsafe { new_obj.as_ptr().add(crate::memory::heap::HEADER_SIZE) };
    let field0: Value = unsafe { std::ptr::read(field0_ptr as *const Value) };
    assert_eq!(field0.as_int(), Some(42));

    let field1_ptr = unsafe {
        new_obj
            .as_ptr()
            .add(crate::memory::heap::HEADER_SIZE + crate::memory::heap::SLOT_SIZE)
    };
    let field1: Value = unsafe { std::ptr::read(field1_ptr as *const Value) };
    assert_eq!(field1.as_long(), Some(100));
}

/// w2-e item 9 (applied by gc-common w18-a): only the Generational heap
/// has an "old gen" to name. A G1 object used to be reported as one,
/// because `is_in_young_addr` is false on G1 by design.
#[test]
fn heap_region_name_labels_a_g1_address_by_its_backend() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    let addr = heap
        .alloc_object(cratonvm_types::ClassId::new(7), 1)
        .as_ptr() as usize;
    assert_eq!(heap_region_name(&heap, 0), "null");
    if heap.is_heap_addr(addr).is_some() {
        assert_eq!(heap_region_name(&heap, addr), "G1 region");
    }
    assert_eq!(heap_region_name(&heap, 0x1_0000), "off-heap");
}

#[test]
fn gc_full_cycle_via_collect_garbage() {
    let heap = small_heap();
    let monitor_table = crate::threading::monitor::MonitorTable::new();

    let obj = heap.alloc_object(ClassId::new(1), 2);
    heap.set_field(obj, 0, Value::Int(42));
    heap.set_field(obj, 1, Value::Long(100));

    let mut roots = vec![obj];
    let result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

    assert_eq!(result.stats.objects_copied, 1);
    assert_eq!(roots.len(), 1);

    let new_obj = roots[0];
    assert_ne!(new_obj.as_ptr(), obj.as_ptr());

    assert_eq!(heap.get_field(new_obj, 0).as_int(), Some(42));
    assert_eq!(heap.get_field(new_obj, 1).as_long(), Some(100));
}

#[test]
fn gc_full_cycle_with_monitor_remap() {
    use crate::threading::jvm_thread::ThreadId;

    let heap = small_heap();
    let monitor_table = crate::threading::monitor::MonitorTable::new();
    let tid = ThreadId(1);

    let obj = heap.alloc_object(ClassId::new(0), 0);
    monitor_table.enter(obj, tid);

    let mut roots = vec![obj];
    let _result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

    let new_obj = roots[0];
    assert_ne!(new_obj.as_ptr(), obj.as_ptr());

    assert!(monitor_table.exit(new_obj, tid).is_ok());
    assert!(monitor_table.exit(new_obj, tid).is_err());
}

#[test]
fn update_value_ref_updates_known_ptr() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000usize, 0x2000usize);

    let obj_ref = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
    let mut val = Value::Object(Some(obj_ref));
    update_value_ref(&mut val, &map);

    match val {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr() as usize, 0x2000),
        _ => panic!("expected updated reference"),
    }
}

/// Recycled-destination false-alarm regression (SpinPollMark `-Xmx80m`
/// OOM pressure, 2026-07-11): under the G1 evacuation-failure retry a
/// drain pass can hand out a first-pass FROM-address as its to-space
/// destination, so the composed pointer map legitimately holds both
/// `K -> V` (drain move into the recycled region) and `V -> C` (the
/// first pass moving V's PREVIOUS occupant out). A slot correctly
/// rewritten to `V` must NOT be reported stale; a slot holding a key
/// that is NOT also a destination is a genuine missed remap and must be.
#[test]
fn verify_no_stale_refs_ignores_recycled_destinations() {
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
    map.insert(0x20013B00000, 0x20010200000); // drain: K -> V (V recycled)
    map.insert(0x20010200000, 0x20013DC02E8); // pass 1: V -> C (old occupant)
    map.insert(0x50000000, 0x60000000); // unrelated genuine move

    // local[0] = a correctly-rewritten recycled destination -> benign;
    // local[1] = a from-address nothing rewrote -> genuine stale.
    let recycled = unsafe { ObjectRef::from_raw(0x20010200000usize as *mut u8) };
    let missed = unsafe { ObjectRef::from_raw(0x50000000usize as *mut u8) };

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let frame = Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        10,
        2,
        &[Value::Object(Some(recycled)), Value::Object(Some(missed))],
    );
    thread.frames.push(frame);

    assert_eq!(
        verify_no_stale_refs(&thread, &map, None),
        1,
        "recycled destination must be benign; unrewritten key must report"
    );
}

#[test]
fn gc_update_all_roots_integration() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    // B10: `update_object_refs` requires a `&VmHeap` for the heap-
    // membership filter; construct one alongside the legacy semi-space
    // `Heap` used by the older standalone GC tests.
    let vm_heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let heap = small_heap();
    let monitor_table = crate::threading::monitor::MonitorTable::new();

    let obj = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(obj, 0, Value::Int(42));

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mut frame = Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        10,
        2,
        &[Value::Object(Some(obj)), Value::Int(99)],
    );
    frame.stack.push(Value::Object(Some(obj))).unwrap();
    thread.frames.push(frame);

    thread.printed.push(Value::Object(Some(obj)));

    let old_ptr = obj.as_ptr();

    let mut roots = vec![obj, obj, obj];
    let result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

    thread.frames[0].update_local_refs(&result.pointer_map, &vm_heap);
    thread.frames[0]
        .stack
        .update_object_refs(&result.pointer_map, &vm_heap);
    for val in &mut thread.printed {
        update_value_ref(val, &result.pointer_map);
    }

    match thread.frames[0].get_local(0) {
        Value::Object(Some(r)) => {
            assert_ne!(r.as_ptr(), old_ptr);
            assert_eq!(heap.get_field(r, 0).as_int(), Some(42));
        }
        other => unreachable!("expected updated object ref in locals, got {other:?}"),
    }
    assert_eq!(thread.frames[0].get_local(1).as_int(), Some(99));

    match thread.frames[0].stack.get_value(0) {
        Value::Object(Some(r)) => {
            assert_ne!(r.as_ptr(), old_ptr);
        }
        other => unreachable!("expected updated object ref in stack, got {other:?}"),
    }

    match &thread.printed[0] {
        Value::Object(Some(r)) => {
            assert_ne!(r.as_ptr(), old_ptr);
        }
        other => unreachable!("expected updated object ref in printed, got {other:?}"),
    }
}
