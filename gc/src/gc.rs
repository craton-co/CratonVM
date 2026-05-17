//! Semi-space copying garbage collector (Cheney algorithm).
//!
//! The collector copies all live objects from from-space to to-space,
//! updating all references in the process. After collection, from-space
//! is reset and the two spaces are swapped.
//!
//! Key properties:
//! - O(live data) time complexity — dead objects are never touched
//! - Compacting — no fragmentation after collection
//! - Handles cycles — forwarding pointers prevent infinite loops

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::arena::Arena;
use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, REF_ELEMENT_SIZE,
    SLOT_SIZE,
};
use rustjvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// T6.3.1 — JVMTI GC hook registry
// ---------------------------------------------------------------------------
//
// The GC fires `GarbageCollectionStart` and `GarbageCollectionFinish` JVMTI
// events at the boundaries of every `collect()` / `collect_with_finalizers()`
// invocation. `gc` has no dependency on the VM crate, so the VM installs
// two function pointers at boot time. When no hook is installed the GC hot
// path pays a single relaxed atomic load (`Acquire` on the flag) before
// branching past the call.

/// Signature of the JVMTI GC lifecycle hook installed by the VM crate.
pub type JvmtiGcHook = fn();

static GC_START_HOOK: OnceLock<JvmtiGcHook> = OnceLock::new();
static GC_FINISH_HOOK: OnceLock<JvmtiGcHook> = OnceLock::new();
static GC_HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the JVMTI `GarbageCollectionStart` hook. Idempotent.
pub fn install_gc_start_hook(hook: JvmtiGcHook) {
    if GC_START_HOOK.set(hook).is_ok() {
        GC_HOOKS_ACTIVE.store(true, Ordering::Release);
    }
}

/// Install the JVMTI `GarbageCollectionFinish` hook. Idempotent.
pub fn install_gc_finish_hook(hook: JvmtiGcHook) {
    if GC_FINISH_HOOK.set(hook).is_ok() {
        GC_HOOKS_ACTIVE.store(true, Ordering::Release);
    }
}

/// Fire the GC-start hook if one is installed.
#[inline]
pub fn fire_gc_start() {
    if !GC_HOOKS_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = GC_START_HOOK.get() {
        hook();
    }
}

/// Fire the GC-finish hook if one is installed.
#[inline]
pub fn fire_gc_finish() {
    if !GC_HOOKS_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = GC_FINISH_HOOK.get() {
        hook();
    }
}

/// Statistics returned after a GC collection.
#[derive(Debug, Clone)]
pub struct GcStats {
    /// Number of live objects copied to to-space.
    pub objects_copied: usize,
    /// Total bytes copied (headers + slot data).
    pub bytes_copied: usize,
    /// Bytes freed (from-space used before - bytes copied).
    pub bytes_freed: usize,
}

/// Result of a GC collection: stats + pointer remapping table.
#[derive(Debug)]
pub struct GcResult {
    /// Collection statistics.
    pub stats: GcStats,
    /// Mapping from old pointer addresses to new pointer addresses.
    /// Used to remap monitor table keys and other external references.
    pub pointer_map: HashMap<usize, usize>,
}

/// Perform a semi-space garbage collection.
///
/// Copies all objects reachable from `roots` from `from_space` to `to_space`.
/// Updates all root ObjectRefs in-place to point to the new locations.
/// After this call, from_space contains only dead data (and should be reset),
/// and to_space contains all live objects.
///
/// Fires JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish` around
/// the copy phase when the VM has installed hooks. Both hooks are zero-cost
/// (single atomic load + branch) when no agent is attached.
///
/// Returns GC statistics and a pointer remapping table.
///
/// # Safety
/// The arenas must contain valid heap objects. All roots must point into from_space.
pub fn collect(from_space: &mut Arena, to_space: &mut Arena, roots: &mut [ObjectRef]) -> GcResult {
    fire_gc_start();
    let bytes_before = from_space.used();
    let mut objects_copied: usize = 0;
    let mut pointer_map = HashMap::new();

    // Phase 1: Forward all root objects
    for root in roots.iter_mut() {
        let old_ptr = root.as_ptr();
        if !from_space.contains(old_ptr) {
            continue; // Skip roots that aren't in from-space (shouldn't happen, but defensive)
        }
        let new_ptr = forward_object(
            from_space,
            to_space,
            old_ptr,
            &mut objects_copied,
            &mut pointer_map,
        );
        // SAFETY: new_ptr was just allocated in to_space via forward_object and
        // points to a valid, fully-copied object with a proper ObjectHeader.
        *root = unsafe { ObjectRef::from_raw(new_ptr) };
    }

    // Phase 2: Cheney scan — scan to-space linearly, forwarding any references found in copied objects
    let mut scan_cursor: usize = 0;
    while scan_cursor < to_space.used() {
        // SAFETY: scan_cursor is within [0, to_space.used()), and objects are
        // laid out contiguously in to-space by forward_object allocations.
        let obj_ptr = unsafe { to_space.base_ptr_mut().add(scan_cursor) };
        // SAFETY: obj_ptr points to a valid ObjectHeader copied by forward_object.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let total_size = object_total_size(header);

        // Scan all reference-containing slots
        if header.kind == ObjectKind::Array {
            // Reference arrays use compact 8-byte pointer storage (REF_ELEMENT_SIZE).
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    // SAFETY: i < array_length, so HEADER_SIZE + i * REF_ELEMENT_SIZE
                    // is within the allocated object bounds.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    // SAFETY: s_ptr points to a valid 8-byte reference slot in the array.
                    let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if from_space.contains(ref_ptr) {
                            let new_ref_ptr = forward_object(
                                from_space,
                                to_space,
                                ref_ptr,
                                &mut objects_copied,
                                &mut pointer_map,
                            );
                            // SAFETY: s_ptr is a valid slot within the copied array;
                            // writing the forwarded pointer back.
                            unsafe {
                                std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64);
                            }
                        }
                    }
                }
            }
        } else {
            let num_slots = header.num_slots as usize;
            for slot_idx in 0..num_slots {
                // SAFETY: slot_idx < num_slots, so the offset is within the object.
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: slot_ptr points to a valid Value-sized region.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if from_space.contains(ref_ptr) {
                        let new_ref_ptr = forward_object(
                            from_space,
                            to_space,
                            ref_ptr,
                            &mut objects_copied,
                            &mut pointer_map,
                        );
                        // SAFETY: new_ref_ptr is a valid object pointer in to-space.
                        let new_value =
                            Value::Object(Some(unsafe { ObjectRef::from_raw(new_ref_ptr) }));
                        // SAFETY: slot_ptr is a valid Value slot within the copied object.
                        unsafe { std::ptr::write(slot_ptr as *mut Value, new_value) };
                    }
                }
            }
        }

        scan_cursor += total_size;
    }

    let bytes_copied = to_space.used();

    // Phase 3: Reset from-space (all live data has been copied to to-space)
    from_space.reset();

    let result = GcResult {
        stats: GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed: bytes_before.saturating_sub(bytes_copied),
        },
        pointer_map,
    };
    fire_gc_finish();
    result
}

/// Forward a single object from from-space to to-space.
///
/// If the object has already been forwarded (forwarding_ptr is set),
/// returns the existing forwarding address.
/// Otherwise, copies the object to to-space and installs a forwarding pointer.
fn forward_object(
    from_space: &Arena,
    to_space: &mut Arena,
    old_ptr: *mut u8,
    objects_copied: &mut usize,
    pointer_map: &mut HashMap<usize, usize>,
) -> *mut u8 {
    // SAFETY: old_ptr is a valid heap object in from_space (verified by caller's
    // from_space.contains() check). The header is readable for the duration of GC.
    let header = unsafe { &*(old_ptr as *const ObjectHeader) };

    // Already forwarded?
    if header.is_forwarded() {
        return header.forwarding_address();
    }

    let total_size = object_total_size(header);

    // Allocate in to-space
    let new_ptr = to_space.alloc(total_size, 8).unwrap_or_else(|| {
        eprintln!(
            "FATAL: gc: forward_object: to-space OOM allocating {} bytes (to-space {}/{} used)",
            total_size,
            to_space.used(),
            to_space.capacity(),
        );
        std::process::abort();
    });

    // SAFETY: old_ptr and new_ptr are non-overlapping (from-space vs to-space),
    // both regions are at least total_size bytes. copy_nonoverlapping is valid.
    unsafe {
        std::ptr::copy_nonoverlapping(old_ptr, new_ptr, total_size);
    }

    // Round-2 fix (T2-4): explicit atomic load+store for the mark_word field.
    // The bulk memcpy above is technically UB for `AtomicU64`: even under STW
    // (no mutator is racing), the C++/Rust memory model requires that any read
    // or write of an atomic location go through an atomic operation. The
    // memcpy reads/writes the bytes but does not produce a happens-before
    // edge with respect to any concurrent observer (e.g. a future
    // concurrent-GC marker thread or an inflate-monitor CAS racing the
    // copy). Replicating the mark word as a real atomic load+store
    // materializes the correct ordering.
    //
    // NOTE for future concurrent-GC support: this STW-only protocol won't
    // suffice. A concurrent collector must instead use a CAS-based forwarding
    // protocol that stalls or retries when the mutator inflates the monitor
    // concurrently with the copy.
    // SAFETY: both `old_ptr` and `new_ptr` point at a fully written
    // ObjectHeader whose `mark_word` field lives at MARK_WORD_OFFSET (32).
    unsafe {
        let old_header_ptr = old_ptr as *const ObjectHeader;
        let new_header_ptr = new_ptr as *mut ObjectHeader;
        let mark = (*old_header_ptr)
            .mark_word
            .load(std::sync::atomic::Ordering::Relaxed);
        (*new_header_ptr)
            .mark_word
            .store(mark, std::sync::atomic::Ordering::Relaxed);
    }

    // SAFETY: new_ptr was just allocated in to_space with at least HEADER_SIZE bytes.
    // Clear the forwarding pointer in the NEW copy (it's a fresh object)
    let new_header = unsafe { &mut *(new_ptr as *mut ObjectHeader) };
    new_header.forwarding_ptr = std::ptr::null_mut();

    // SAFETY: old_ptr is still valid in from_space (not freed yet) and we have
    // exclusive access during STW GC. Install forwarding pointer for future lookups.
    let old_header = unsafe { &mut *(old_ptr as *mut ObjectHeader) };
    old_header.forwarding_ptr = new_ptr;

    // Track the mapping
    pointer_map.insert(old_ptr as usize, new_ptr as usize);
    *objects_copied += 1;

    // Ensure the from_space check in the caller still works:
    // We just modified old_ptr's header (which is in from_space). The
    // from_space.contains() check uses the arena's data bounds, so this is fine.
    debug_assert!(from_space.contains(old_ptr));

    new_ptr
}

/// Compute the total size of a heap object (header + data).
///
/// Objects use num_slots * SLOT_SIZE. Arrays use compact element sizes.
pub fn object_total_size(header: &ObjectHeader) -> usize {
    if header.kind == ObjectKind::Array {
        HEADER_SIZE + array_data_size(header.array_length as usize, header.element_type)
            .expect("array_data_size overflow in gc object_total_size")
    } else {
        HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
    }
}

/// Perform a semi-space GC, then resurrect dead finalizable objects.
///
/// Works like [`collect`] but takes an additional list of finalizer addresses.
/// After the normal Cheney scan, any finalizer address that was NOT copied
/// (i.e. unreachable from roots) is forwarded from from-space to to-space
/// so that `finalize()` can still access the object. The returned
/// `dead_finalizers` vector contains the NEW addresses of these resurrected
/// objects.
pub fn collect_with_finalizers(
    from_space: &mut Arena,
    to_space: &mut Arena,
    roots: &mut [ObjectRef],
    finalizer_addrs: &[usize],
) -> (GcResult, Vec<usize>) {
    fire_gc_start();
    let bytes_before = from_space.used();
    let mut objects_copied: usize = 0;
    let mut pointer_map = HashMap::new();

    // Phase 1: Forward all root objects (same as collect)
    for root in roots.iter_mut() {
        let old_ptr = root.as_ptr();
        if !from_space.contains(old_ptr) {
            continue;
        }
        let new_ptr = forward_object(
            from_space, to_space, old_ptr, &mut objects_copied, &mut pointer_map,
        );
        *root = unsafe { ObjectRef::from_raw(new_ptr) };
    }

    // Phase 2: Cheney scan (same as collect)
    let mut scan_cursor: usize = 0;
    while scan_cursor < to_space.used() {
        let obj_ptr = unsafe { to_space.base_ptr_mut().add(scan_cursor) };
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let total_size = object_total_size(header);

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if from_space.contains(ref_ptr) {
                            let new_ref_ptr = forward_object(
                                from_space, to_space, ref_ptr,
                                &mut objects_copied, &mut pointer_map,
                            );
                            unsafe { std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64); }
                        }
                    }
                }
            }
        } else {
            let num_slots = header.num_slots as usize;
            for slot_idx in 0..num_slots {
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if from_space.contains(ref_ptr) {
                        let new_ref_ptr = forward_object(
                            from_space, to_space, ref_ptr,
                            &mut objects_copied, &mut pointer_map,
                        );
                        let new_value =
                            Value::Object(Some(unsafe { ObjectRef::from_raw(new_ref_ptr) }));
                        unsafe { std::ptr::write(slot_ptr as *mut Value, new_value); }
                    }
                }
            }
        }
        scan_cursor += total_size;
    }

    // Phase 3: Resurrect dead finalizable objects.
    // Objects whose old address is NOT in pointer_map were unreachable from
    // normal roots. Copy them to to-space so finalize() can run on them.
    let mut dead_finalizers = Vec::new();
    for &old_addr in finalizer_addrs {
        if pointer_map.contains_key(&old_addr) {
            continue; // already reachable — skip
        }
        let old_ptr = old_addr as *mut u8;
        if !from_space.contains(old_ptr) {
            continue;
        }
        let new_ptr = forward_object(
            from_space, to_space, old_ptr, &mut objects_copied, &mut pointer_map,
        );
        dead_finalizers.push(new_ptr as usize);

        // Cheney-scan the resurrected object's references too, so any objects
        // reachable from it also survive (e.g. this.id field pointing to another obj).
    }

    // Phase 3b: Continue Cheney scan for newly added objects from resurrection
    while scan_cursor < to_space.used() {
        let obj_ptr = unsafe { to_space.base_ptr_mut().add(scan_cursor) };
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let total_size = object_total_size(header);

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if from_space.contains(ref_ptr) {
                            let new_ref_ptr = forward_object(
                                from_space, to_space, ref_ptr,
                                &mut objects_copied, &mut pointer_map,
                            );
                            unsafe { std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64); }
                        }
                    }
                }
            }
        } else {
            let num_slots = header.num_slots as usize;
            for slot_idx in 0..num_slots {
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if from_space.contains(ref_ptr) {
                        let new_ref_ptr = forward_object(
                            from_space, to_space, ref_ptr,
                            &mut objects_copied, &mut pointer_map,
                        );
                        let new_value =
                            Value::Object(Some(unsafe { ObjectRef::from_raw(new_ref_ptr) }));
                        unsafe { std::ptr::write(slot_ptr as *mut Value, new_value); }
                    }
                }
            }
        }
        scan_cursor += total_size;
    }

    let bytes_copied = to_space.used();
    from_space.reset();

    let out = (
        GcResult {
            stats: GcStats {
                objects_copied,
                bytes_copied,
                bytes_freed: bytes_before.saturating_sub(bytes_copied),
            },
            pointer_map,
        },
        dead_finalizers,
    );
    fire_gc_finish();
    out
}

/// Update a Value's ObjectRef using the pointer map.
/// If the value is `Object(Some(ref))` and the ref's address is in the map,
/// update it to the new address.
pub fn update_value_ref(value: &mut Value, pointer_map: &HashMap<usize, usize>) {
    if let Value::Object(Some(ref mut obj_ref)) = value {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by the GC's pointer map and points
            // into to-space where a valid object was copied by forward_object.
            // The debug_assert verifies non-null during development.
            debug_assert!(new_addr != 0, "gc: update_value_ref: pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ArrayElementType, Heap};
    use rustjvm_types::ClassId;

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
        let header = unsafe { &*(new_obj.as_ptr() as *const ObjectHeader) };
        assert_eq!(header.class_id, ClassId::new(1));
        assert_eq!(header.num_slots, 2);

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
        let slot0_ptr = unsafe { new_arr.as_ptr().add(HEADER_SIZE) };
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
    fn update_value_ref_leaves_unknown_ptr() {
        let map = HashMap::new();
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
        let map = HashMap::new();
        let mut val = Value::Int(42);
        update_value_ref(&mut val, &map);
        assert_eq!(val.as_int(), Some(42));
    }

    // ----------------------------------------------------------------
    // T6.3.1 — JVMTI GC-hook tests. `OnceLock` is process-wide so these
    // tests share one installed hook; bodies tolerate repeat invocations.
    // A shared static mutex serializes the two tests since they both
    // observe the single pair of static counters.
    // ----------------------------------------------------------------

    use std::sync::atomic::{AtomicU32, Ordering as CounterOrd};
    static GC_HOOK_STARTS: AtomicU32 = AtomicU32::new(0);
    static GC_HOOK_FINISHES: AtomicU32 = AtomicU32::new(0);

    fn gc_hook_start_cb() { GC_HOOK_STARTS.fetch_add(1, CounterOrd::SeqCst); }
    fn gc_hook_finish_cb() { GC_HOOK_FINISHES.fetch_add(1, CounterOrd::SeqCst); }

    fn gc_hook_test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock as StdOnceLock};
        static L: StdOnceLock<Mutex<()>> = StdOnceLock::new();
        L.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn gc_hooks_fire_around_collect() {
        let _guard = gc_hook_test_lock();
        install_gc_start_hook(gc_hook_start_cb);
        install_gc_finish_hook(gc_hook_finish_cb);

        let before_start = GC_HOOK_STARTS.load(CounterOrd::SeqCst);
        let before_finish = GC_HOOK_FINISHES.load(CounterOrd::SeqCst);

        // Trigger a real GC cycle on a tiny heap.
        let heap = small_heap();
        let obj = heap.alloc_object(rustjvm_types::ClassId::new(1), 1);
        let mut roots = vec![obj];
        let (mut from, mut to) = heap.lock_spaces();
        let _ = collect(&mut from, &mut to, &mut roots);

        assert_eq!(GC_HOOK_STARTS.load(CounterOrd::SeqCst), before_start + 1, "GarbageCollectionStart must fire exactly once per collect()");
        assert_eq!(GC_HOOK_FINISHES.load(CounterOrd::SeqCst), before_finish + 1, "GarbageCollectionFinish must fire exactly once per collect()");
    }

    #[test]
    fn gc_hooks_fire_around_collect_with_finalizers() {
        let _guard = gc_hook_test_lock();
        install_gc_start_hook(gc_hook_start_cb);
        install_gc_finish_hook(gc_hook_finish_cb);

        let before_start = GC_HOOK_STARTS.load(CounterOrd::SeqCst);
        let before_finish = GC_HOOK_FINISHES.load(CounterOrd::SeqCst);

        let heap = small_heap();
        let obj = heap.alloc_object(rustjvm_types::ClassId::new(1), 1);
        let mut roots = vec![obj];
        let (mut from, mut to) = heap.lock_spaces();
        let _ = collect_with_finalizers(&mut from, &mut to, &mut roots, &[]);

        assert_eq!(GC_HOOK_STARTS.load(CounterOrd::SeqCst), before_start + 1);
        assert_eq!(GC_HOOK_FINISHES.load(CounterOrd::SeqCst), before_finish + 1);
    }
}
