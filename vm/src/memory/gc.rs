// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VM-specific GC helpers.
//!
//! The core GC algorithm lives in the `cratonvm-gc` crate. This module
//! provides the VM-specific `update_all_roots` function and re-exports
//! the gc crate's types for backward compatibility.

use std::collections::HashMap;

// Re-export everything from the gc crate's gc module.
pub use cratonvm_gc::gc::*;

use crate::types::ObjectRef;
#[cfg(test)]
use crate::types::Value;

/// Update all root locations in the VM state after a GC collection.
///
/// Scans thread frames (locals + operand stacks), static fields, class locks,
/// and printed values, updating any ObjectRef whose old address appears in
/// the pointer map.
pub fn update_all_roots(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    pointer_map: &HashMap<usize, usize>,
) {
    if pointer_map.is_empty() {
        return;
    }

    // 1. Thread frames — locals and operand stacks (SoA layout)
    for frame in &mut thread.frames {
        frame.update_local_refs(pointer_map);
        frame.stack.update_object_refs(pointer_map);
    }

    for obj_ref in &mut thread.native_pin_roots {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }

    if let Some(ref mut obj_ref) = thread.native_pending_return {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }

    // 2. Static fields
    {
        let mut statics = shared.statics.write();
        for fields in statics.values_mut() {
            for val in fields.iter_mut() {
                update_value_ref(val, pointer_map);
            }
        }
    }

    // 3. Class lock objects
    {
        let mut class_locks = shared.class_locks.write();
        for obj_ref in class_locks.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 4. Thread printed values
    for val in &mut thread.printed {
        update_value_ref(val, pointer_map);
    }

    // 5. Interned string pool
    {
        let mut string_pool = shared.string_pool.write();
        for obj_ref in string_pool.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 6. Class mirror cache
    {
        let mut class_mirrors = shared.class_mirrors.write();
        for obj_ref in class_mirrors.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 7. System streams (System.out, System.err)
    {
        let mut out = shared.system_out.write();
        if let Some(ref mut obj_ref) = *out {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        let mut err = shared.system_err.write();
        if let Some(ref mut obj_ref) = *err {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 8. Primitive type Class mirrors
    {
        let mut prim_mirrors = shared.primitive_mirrors.write();
        for obj_ref in prim_mirrors.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 9. JNI global references — update stored ObjectRefs inside each Box<ObjectRef>.
    {
        shared
            .jni_global_refs
            .lock()
            .update_after_gc(pointer_map);
    }

    // 10. Thread-local ObjectRefs — java_thread_obj, pending_async_exception
    if let Some(ref mut obj_ref) = thread.java_thread_obj {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    if let Some(ref mut obj_ref) = thread.pending_async_exception {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }

    // 11. Root snapshot
    {
        let mut snapshot = thread.root_snapshot.lock();
        for obj_ref in snapshot.iter_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 12. Scoped value bindings (JEP 446)
    //
    // Round-9 GC fix: also remap the ScopedValue KEY ObjectRef when present
    // (previous code only remapped values). Without this, a moving GC
    // would leave the key pointing at a stale post-compaction address —
    // a use-after-free on the next `Carrier.get` traversal.
    for (_key_id, key_ref, val) in &mut thread.scoped_values {
        if let Some(obj_ref) = key_ref {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        update_value_ref(val, pointer_map);
    }

    // 13. Resolution cache — CONSTANT_Dynamic values may hold ObjectRefs
    {
        let mut cache = shared.resolution_cache.write();
        cache.update_condy_refs(pointer_map);
    }

    // 14. Class mirrors reverse map — rebuild keys from updated forward map
    {
        let class_mirrors = shared.class_mirrors.read();
        let mut reverse = shared.class_mirrors_reverse.write();
        reverse.clear();
        for (&class_id, obj_ref) in class_mirrors.iter() {
            reverse.insert(*obj_ref, class_id);
        }
    }

    // 15. Round-9 CRIT GC-correctness fix: re-point the process-global
    //     Integer.valueOf / Boolean.TRUE/FALSE caches living in
    //     `native-builtins/src/lang_math.rs`. They are reported as roots
    //     by `roots.rs` step 15, so the cached objects survive GC — but
    //     under a moving collector their addresses change and we must
    //     remap them here, otherwise the next cache lookup returns a
    //     stale pointer.
    cratonvm_native_builtins::lang_math::gc_update_value_of_cache_refs(pointer_map);

    // 16. Round-9 perf + GC fix: re-point the process-global LambdaMetafactory
    //     CallSite cache living in `native-builtins/src/lang_invoke.rs`.
    //     Same scan/update contract as the Integer.valueOf cache.
    cratonvm_native_builtins::lang_invoke::gc_update_lambda_callsite_cache_refs(pointer_map);

    // Post-GC verification: check that no frame refs still point to relocated addresses.
    verify_no_stale_refs(thread, pointer_map);
}

/// Post-GC verification: warns if any thread frame local or operand stack value
/// still holds an ObjectRef whose address appears in the pointer_map (i.e. was
/// supposed to be relocated), OR points to zeroed-out memory (i.e. was garbage
/// collected because it wasn't in the root set).
fn verify_no_stale_refs(
    thread: &crate::threading::jvm_thread::JvmThread,
    pointer_map: &HashMap<usize, usize>,
) {
    use crate::types::Value;
    use cratonvm_types::ObjectHeader;

    // Allow opt-in heavy diagnostic that walks every Object slot and checks
    // for a zeroed header (class_id=0 && identity_hash_code=0 && num_slots=0).
    // Such a slot is the in-memory signature of the heavy-trees bug: a
    // pointer at an address inside the just-reset young-from semispace.
    let heavy = std::env::var("CRATONVM_GC_VERIFY_STALE").ok().as_deref() == Some("1");

    for (fi, frame) in thread.frames.iter().enumerate() {
        let cname = frame.class_name();
        let mname = frame.method_name();
        // Check locals
        for li in 0..frame.locals_len() {
            let val = frame.get_local(li as u16);
            if let Value::Object(Some(obj_ref)) = val {
                let addr = obj_ref.as_ptr() as usize;
                if pointer_map.contains_key(&addr) {
                    tracing::error!(
                        "POST-GC STALE LOCAL: frame[{}] {}.{} local[{}] still points to \
                         relocated addr 0x{:x} (should be 0x{:x})",
                        fi, cname, mname, li,
                        addr, pointer_map[&addr],
                    );
                }
                if heavy && addr != 0 {
                    // SAFETY: read-only probe of an aligned address; if the
                    // slot is corrupt we'll see it in the diagnostic. This is
                    // an opt-in debug path.
                    let h = unsafe { &*(addr as *const ObjectHeader) };
                    if h.class_id.as_u32() == 0
                        && h.identity_hash_code == 0
                        && h.num_slots == 0
                        && h.array_length == 0
                    {
                        tracing::error!(
                            "POST-GC ZERO-HEADER LOCAL: frame[{}] {}.{} local[{}] pc={} \
                             points to ZEROED header at 0x{:x} (kind={:?}, gc_flags=0x{:x})",
                            fi, cname, mname, li, frame.pc,
                            addr, h.kind, h.gc_flags,
                        );
                    }
                }
            }
        }
        // Check stack
        for si in 0..frame.stack.len() {
            let val = frame.stack.get_value(si);
            if let Value::Object(Some(obj_ref)) = val {
                let addr = obj_ref.as_ptr() as usize;
                if pointer_map.contains_key(&addr) {
                    tracing::error!(
                        "POST-GC STALE STACK: frame[{}] {}.{} stack[{}] still points to \
                         relocated addr 0x{:x} (should be 0x{:x})",
                        fi, cname, mname, si,
                        addr, pointer_map[&addr],
                    );
                }
                if heavy && addr != 0 {
                    // SAFETY: see locals comment above.
                    let h = unsafe { &*(addr as *const ObjectHeader) };
                    if h.class_id.as_u32() == 0
                        && h.identity_hash_code == 0
                        && h.num_slots == 0
                        && h.array_length == 0
                    {
                        tracing::error!(
                            "POST-GC ZERO-HEADER STACK: frame[{}] {}.{} stack[{}] pc={} \
                             points to ZEROED header at 0x{:x} (kind={:?}, gc_flags=0x{:x})",
                            fi, cname, mname, si, frame.pc,
                            addr, h.kind, h.gc_flags,
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::memory::heap::Heap;

    /// Helper to create a test heap with small capacity for GC testing.
    fn small_heap() -> Heap {
        // 8 KB total (4 KB per semi-space) — forces GC quickly
        Heap::with_capacity(8 * 1024)
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
        assert_eq!(header.num_slots, 2);

        // Read field values from to-space via raw pointers
        let field0_ptr = unsafe { new_obj.as_ptr().add(crate::memory::heap::HEADER_SIZE) };
        let field0: Value = unsafe { std::ptr::read(field0_ptr as *const Value) };
        assert_eq!(field0.as_int(), Some(42));

        let field1_ptr = unsafe { new_obj.as_ptr().add(crate::memory::heap::HEADER_SIZE + crate::memory::heap::SLOT_SIZE) };
        let field1: Value = unsafe { std::ptr::read(field1_ptr as *const Value) };
        assert_eq!(field1.as_long(), Some(100));
    }

    #[test]
    fn gc_full_cycle_via_collect_garbage() {
        let heap = small_heap();
        let monitor_table = crate::threading::monitor::MonitorTable::new();

        let obj = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(100));

        let mut roots = vec![obj];
        let result = heap.collect_garbage(&mut roots, &monitor_table);

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
        let _result = heap.collect_garbage(&mut roots, &monitor_table);

        let new_obj = roots[0];
        assert_ne!(new_obj.as_ptr(), obj.as_ptr());

        assert!(monitor_table.exit(new_obj, tid).is_ok());
        assert!(monitor_table.exit(new_obj, tid).is_err());
    }

    #[test]
    fn update_value_ref_updates_known_ptr() {
        let mut map = HashMap::new();
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
    fn gc_update_all_roots_integration() {
        use crate::runtime::frame::Frame;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

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
        let result = heap.collect_garbage(&mut roots, &monitor_table);

        thread.frames[0].update_local_refs(&result.pointer_map);
        thread.frames[0]
            .stack
            .update_object_refs(&result.pointer_map);
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
}
