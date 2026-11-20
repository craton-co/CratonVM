use super::*;
use crate::heap::{ArrayElementType, Heap};
use cratonvm_types::ClassId;

/// Helper to create a test heap with small capacity for GC testing.
fn small_heap() -> Heap {
    // 8 KB total (4 KB per semi-space) — forces GC quickly
    Heap::with_capacity(8 * 1024)
}

/// gcj j1/cm: a live array above the old fixed 64 MiB "sanity" cap is
/// copied (the cap refused it, and `forward_object` aborted the process);
/// the bound is now the object's arena.
#[test]
fn gcj1cm_a_live_array_above_64_mib_is_copied() {
    let heap = Heap::with_capacity(2 * 72 * 1024 * 1024);
    let len = 65 * 1024 * 1024;
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, len);
    let mut roots = vec![arr];
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);
    assert_eq!(result.stats.objects_copied, 1);
    let header = unsafe { &*(roots[0].as_ptr() as *const ObjectHeader) };
    assert_eq!(header.array_length() as usize, len);
}

/// gcj j1/cm: a header whose size runs past the end of its arena is still
/// refused (the arena bound replaces the constant, it does not drop it).
#[test]
fn gcj1cm_a_size_past_the_arena_end_is_refused() {
    let heap = small_heap();
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);
    // Corrupt the length: u32::MAX ints cannot end inside a 4 KiB arena.
    unsafe {
        (arr.as_ptr().add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *mut u32)
            .write_unaligned(u32::MAX);
    }
    let (from, mut to) = heap.lock_spaces();
    let mut copied = 0usize;
    let mut map = cratonvm_types::PointerMap::default();
    let r = try_forward_object(&from, &mut to, arr.as_ptr(), &mut copied, &mut map);
    assert!(r.is_err());
    assert_eq!(copied, 0);
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
    let header = unsafe { &*(new_obj.as_ptr() as *const ObjectHeader) };
    assert_eq!(header.class_id, ClassId::new(1));
    assert_eq!(header.num_slots(), 2);

    // Read field values from to-space via raw pointers
    let field0_ptr = unsafe { new_obj.as_ptr().add(HEADER_SIZE) };
    let field0: Value = unsafe { std::ptr::read(field0_ptr as *const Value) };
    assert_eq!(field0.as_int(), Some(42));

    let field1_ptr = unsafe { new_obj.as_ptr().add(HEADER_SIZE + SLOT_SIZE) };
    let field1: Value = unsafe { std::ptr::read(field1_ptr as *const Value) };
    assert_eq!(field1.as_long(), Some(100));
}

#[test]
fn gc_unreachable_objects_freed() {
    let heap = small_heap();
    let _dead1 = heap.alloc_object(ClassId::new(0), 1);
    let _dead2 = heap.alloc_object(ClassId::new(0), 1);
    let live = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(live, 0, Value::Int(999));

    let bytes_before = heap.allocated_bytes();
    let mut roots = vec![live]; // only 'live' is a root
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);

    assert_eq!(result.stats.objects_copied, 1); // only the live one
    assert!(result.stats.bytes_freed > 0);
    assert!(result.stats.bytes_copied < bytes_before);

    // Verify the live object's data is intact
    let new_live = roots[0];
    let field0_ptr = unsafe { new_live.as_ptr().add(HEADER_SIZE) };
    let field0: Value = unsafe { std::ptr::read(field0_ptr as *const Value) };
    assert_eq!(field0.as_int(), Some(999));
}

#[test]
fn gc_updates_internal_references() {
    let heap = small_heap();
    let obj_a = heap.alloc_object(ClassId::new(1), 1);
    let obj_b = heap.alloc_object(ClassId::new(2), 1);

    // A points to B
    heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
    heap.set_field(obj_b, 0, Value::Int(77));

    let mut roots = vec![obj_a]; // only A is a root; B is reachable through A
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);

    assert_eq!(result.stats.objects_copied, 2); // both A and B copied

    // Read A's field — should now point to the new B
    let new_a = roots[0];
    let field0_ptr = unsafe { new_a.as_ptr().add(HEADER_SIZE) };
    let field0: Value = unsafe { std::ptr::read(field0_ptr as *const Value) };
    match field0 {
        Value::Object(Some(new_b)) => {
            // new_b should be different from old obj_b
            assert_ne!(new_b.as_ptr(), obj_b.as_ptr());
            // B's field should still be 77
            let b_field_ptr = unsafe { new_b.as_ptr().add(HEADER_SIZE) };
            let b_field: Value = unsafe { std::ptr::read(b_field_ptr as *const Value) };
            assert_eq!(b_field.as_int(), Some(77));
        }
        other => panic!("expected A's field to be Object(Some(...)), got {other:?}"),
    }
}

#[test]
fn gc_handles_cycles() {
    let heap = small_heap();
    let obj_a = heap.alloc_object(ClassId::new(0), 1);
    let obj_b = heap.alloc_object(ClassId::new(0), 1);

    // A -> B -> A (cycle)
    heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
    heap.set_field(obj_b, 0, Value::Object(Some(obj_a)));

    let mut roots = vec![obj_a];
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);

    assert_eq!(result.stats.objects_copied, 2); // both survive

    // Verify the cycle is intact: new_A -> new_B -> new_A
    let new_a = roots[0];
    let a_field_ptr = unsafe { new_a.as_ptr().add(HEADER_SIZE) };
    let a_field: Value = unsafe { std::ptr::read(a_field_ptr as *const Value) };
    match a_field {
        Value::Object(Some(new_b)) => {
            let b_field_ptr = unsafe { new_b.as_ptr().add(HEADER_SIZE) };
            let b_field: Value = unsafe { std::ptr::read(b_field_ptr as *const Value) };
            match b_field {
                Value::Object(Some(back_to_a)) => {
                    assert_eq!(back_to_a.as_ptr(), new_a.as_ptr());
                }
                other => unreachable!("expected B->A cycle, got {other:?}"),
            }
        }
        other => unreachable!("expected A->B reference, got {other:?}"),
    }
}

#[test]
fn gc_deep_chain() {
    let heap = small_heap();
    // Create chain: A -> B -> C -> D
    let d = heap.alloc_object(ClassId::new(3), 1);
    heap.set_field(d, 0, Value::Int(4));

    let c = heap.alloc_object(ClassId::new(2), 1);
    heap.set_field(c, 0, Value::Object(Some(d)));

    let b = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(b, 0, Value::Object(Some(c)));

    let a = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(a, 0, Value::Object(Some(b)));

    let mut roots = vec![a];
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);

    assert_eq!(result.stats.objects_copied, 4);

    // Walk the chain to verify D's value
    let new_a = roots[0];
    let b_val: Value =
        unsafe { std::ptr::read(new_a.as_ptr().add(HEADER_SIZE) as *const Value) };
    let new_b = b_val.as_object().unwrap();
    let c_val: Value =
        unsafe { std::ptr::read(new_b.as_ptr().add(HEADER_SIZE) as *const Value) };
    let new_c = c_val.as_object().unwrap();
    let d_val: Value =
        unsafe { std::ptr::read(new_c.as_ptr().add(HEADER_SIZE) as *const Value) };
    let new_d = d_val.as_object().unwrap();
    let d_field: Value =
        unsafe { std::ptr::read(new_d.as_ptr().add(HEADER_SIZE) as *const Value) };
    assert_eq!(d_field.as_int(), Some(4));
}

#[test]
fn gc_array_references() {
    let heap = small_heap();
    let elem = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(elem, 0, Value::Int(55));

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    heap.set_array_element(arr, 0, Value::Object(Some(elem)))
        .unwrap();
    heap.set_array_element(arr, 1, Value::Object(None)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(0)).unwrap(); // auto-boxed to wrapper

    let mut roots = vec![arr];
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);

    assert_eq!(result.stats.objects_copied, 3); // arr + elem + autobox wrapper

    // Check the array's first element points to the copied elem
    // Reference array elements are stored as compact 8-byte pointers (REF_ELEMENT_SIZE).
    let new_arr = roots[0];
    let slot0_ptr = unsafe { new_arr.as_ptr().add(ARRAY_DATA_OFFSET) };
    let raw: u64 = unsafe { std::ptr::read(slot0_ptr as *const u64) };
    assert_ne!(raw, 0, "Expected array[0] to be a non-null reference");
    let new_elem = unsafe { ObjectRef::from_raw(raw as usize as *mut u8) };
    assert_ne!(new_elem.as_ptr(), elem.as_ptr());
    let f: Value =
        unsafe { std::ptr::read(new_elem.as_ptr().add(HEADER_SIZE) as *const Value) };
    assert_eq!(f.as_int(), Some(55));
}

#[test]
fn gc_pointer_map() {
    let heap = small_heap();
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let old_addr = obj.as_ptr() as usize;

    let mut roots = vec![obj];
    let (mut from, mut to) = heap.lock_spaces();
    let result = collect(&mut from, &mut to, &mut roots);

    assert!(result.pointer_map.contains_key(&old_addr));
    let new_addr = result.pointer_map[&old_addr];
    assert_eq!(roots[0].as_ptr() as usize, new_addr);
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

#[test]
fn update_value_ref_leaves_unknown_ptr() {
    let map = cratonvm_types::PointerMap::default();
    let obj_ref = unsafe { ObjectRef::from_raw(0x9998 as *mut u8) }; // 8-byte aligned
    let mut val = Value::Object(Some(obj_ref));
    update_value_ref(&mut val, &map);

    match val {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr() as usize, 0x9998),
        _ => panic!("expected unchanged reference"),
    }
}

#[test]
fn update_value_ref_ignores_non_object() {
    let map = cratonvm_types::PointerMap::default();
    let mut val = Value::Int(42);
    update_value_ref(&mut val, &map);
    assert_eq!(val.as_int(), Some(42));
}

// ----------------------------------------------------------------
// T6.3.1 — JVMTI GC-hook tests. The installed hook is process-wide
// (`OnceLock`): once the first test installs it, EVERY `collect()` in
// the whole test binary fires the shared callback. So global counters
// would race — any other gc test that collects between a test's
// before-load and after-assert would bump the same counter and break
// the exact "+1" check under parallel execution.
//
// Fix: the callbacks bump a THREAD-LOCAL counter. `collect()` fires
// the hook synchronously on the calling thread, and each libtest case
// runs on its own thread, so a test observes only the GCs it itself
// triggered — making "fires exactly once per collect()" robust to
// collections happening concurrently on other test threads. (Same
// idiom as jit/src/lib.rs's per-thread counter.)
// ----------------------------------------------------------------

use std::cell::Cell;
thread_local! {
    static GC_HOOK_STARTS: Cell<u32> = const { Cell::new(0) };
    static GC_HOOK_FINISHES: Cell<u32> = const { Cell::new(0) };
}

fn gc_hook_start_cb() {
    GC_HOOK_STARTS.with(|c| c.set(c.get() + 1));
}
fn gc_hook_finish_cb() {
    GC_HOOK_FINISHES.with(|c| c.set(c.get() + 1));
}

#[test]
fn gc_hooks_fire_around_collect() {
    install_gc_start_hook(gc_hook_start_cb);
    install_gc_finish_hook(gc_hook_finish_cb);

    let before_start = GC_HOOK_STARTS.with(Cell::get);
    let before_finish = GC_HOOK_FINISHES.with(Cell::get);

    // Trigger a real GC cycle on a tiny heap.
    let heap = small_heap();
    let obj = heap.alloc_object(cratonvm_types::ClassId::new(1), 1);
    let mut roots = vec![obj];
    let (mut from, mut to) = heap.lock_spaces();
    let _ = collect(&mut from, &mut to, &mut roots);

    assert_eq!(
        GC_HOOK_STARTS.with(Cell::get),
        before_start + 1,
        "GarbageCollectionStart must fire exactly once per collect()"
    );
    assert_eq!(
        GC_HOOK_FINISHES.with(Cell::get),
        before_finish + 1,
        "GarbageCollectionFinish must fire exactly once per collect()"
    );
}

#[test]
fn gc_hooks_fire_around_collect_with_finalizers() {
    install_gc_start_hook(gc_hook_start_cb);
    install_gc_finish_hook(gc_hook_finish_cb);

    let before_start = GC_HOOK_STARTS.with(Cell::get);
    let before_finish = GC_HOOK_FINISHES.with(Cell::get);

    let heap = small_heap();
    let obj = heap.alloc_object(cratonvm_types::ClassId::new(1), 1);
    let mut roots = vec![obj];
    let (mut from, mut to) = heap.lock_spaces();
    let _ = collect_with_finalizers(&mut from, &mut to, &mut roots, &[]);

    assert_eq!(GC_HOOK_STARTS.with(Cell::get), before_start + 1);
    assert_eq!(GC_HOOK_FINISHES.with(Cell::get), before_finish + 1);
}

