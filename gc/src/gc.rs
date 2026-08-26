// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    HEADER_SIZE, REF_ELEMENT_SIZE,
    SLOT_SIZE,
};
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, write_ref_slot};
use cratonvm_types::{ObjectRef, Value};

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

// ---------------------------------------------------------------------------
// Class-info diagnostic hook (layout-mismatch triage)
// ---------------------------------------------------------------------------
//
// When `gen_heap::{get,set}_field` detects an out-of-bounds field access it
// can only log the raw numeric `ClassId`, which is useless for triage — the
// orchestrator has to reverse-map it by hand. The `gc` crate cannot depend on
// `classloading`, so (mirroring the JVMTI GC hooks above) the VM installs a
// resolver function at boot. Given a raw class-id `u32` it returns the class
// NAME and the class's REAL declared `num_total_fields`, converting an
// opaque `ClassId(275)` into an actionable `org/jboss/.../Foo (real layout
// has 7 fields)`.

/// Signature of the class-info resolver installed by the VM crate.
///
/// Argument: the raw `ClassId` value (`ClassId::as_u32`).
/// Returns: `Some((class_name, num_total_fields))` if the id is known,
/// `None` if it is not registered (e.g. a raw test-only id).
pub type ClassInfoHook = fn(u32) -> Option<(String, usize)>;

static CLASS_INFO_HOOK: OnceLock<ClassInfoHook> = OnceLock::new();

/// Install the class-info resolver used by heap layout-mismatch diagnostics.
/// Idempotent — only the first installation wins.
pub fn install_class_info_hook(hook: ClassInfoHook) {
    let _ = CLASS_INFO_HOOK.set(hook);
}

/// Resolve a raw class id to `(name, declared_field_count)` if a resolver
/// has been installed. Used only on cold diagnostic paths.
pub fn resolve_class_info(class_id: u32) -> Option<(String, usize)> {
    CLASS_INFO_HOOK.get().and_then(|hook| hook(class_id))
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
    pub pointer_map: cratonvm_types::PointerMap,
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
/// # Root remapping contract
///
/// This function updates **only** the `ObjectRef`s in the `roots` slice it is
/// handed: every root that points into `from_space` is rewritten in place to
/// its new to-space address (Phase 1). A `debug_assert` at the end of Phase 1
/// verifies this happened for every such root.
///
/// VM-external references — JVMTI/JNI handles, class statics, the interned
/// string pool, and the monitor table — are **not** part of the `roots` slice
/// and are therefore **not** remapped here. Remapping those is the caller's
/// responsibility: the caller must apply the returned [`GcResult::pointer_map`]
/// to every external reference table (see `Heap::collect_garbage`, which calls
/// `MonitorCleanup::remap_after_gc`, and the VM crate which remaps statics /
/// JNI / the string pool). If an external root is reachable from the heap it
/// must also be included in `roots` so the object survives the copy; passing
/// it only via an external table is a use-after-free.
///
/// # Safety
/// The arenas must contain valid heap objects. All roots must point into from_space.
pub fn collect(from_space: &mut Arena, to_space: &mut Arena, roots: &mut [ObjectRef]) -> GcResult {
    fire_gc_start();
    let bytes_before = from_space.used();
    let mut objects_copied: usize = 0;
    let mut pointer_map = cratonvm_types::PointerMap::default();

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

    // Contract check: after Phase 1 no root may still point into from-space.
    // Every from-space root was forwarded and rewritten above; a root that
    // remains in from-space here means a root was missed (would dangle once
    // from-space is reset). External references (statics/JNI/string pool/
    // monitors) are intentionally NOT in `roots` — see the function doc.
    debug_assert!(
        roots.iter().all(|r| !from_space.contains(r.as_ptr())),
        "gc::collect: a root still points into from-space after Phase 1 — \
         every passed root must be remapped before from-space is reset",
    );

    // Phase 2: Cheney scan — scan to-space linearly, forwarding any references found in copied objects
    let mut scan_cursor: usize = 0;
    while scan_cursor < to_space.used() {
        // SAFETY: scan_cursor is within [0, to_space.used()), and objects are
        // laid out contiguously in to-space by forward_object allocations.
        let obj_ptr = unsafe { to_space.base_ptr_mut().add(scan_cursor) };
        // SAFETY: obj_ptr points to a valid ObjectHeader copied by forward_object.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let total_size = object_total_size(header);

        // Defensive: a corrupt header in to-space (implausible / zero-size)
        // would desynchronise the linear Cheney walk. `object_total_size`
        // returns 0 for an overflowing array header, and a 0 stride would spin
        // this loop forever. Every object here was copied by `forward_object`,
        // which rejects corrupt headers before copying, so this should be
        // unreachable — but never advance the cursor by an implausible amount.
        // Stop the walk with a diagnostic rather than aborting the process.
        if total_size < HEADER_SIZE || scan_cursor + total_size > to_space.used() {
            tracing::warn!(
                "gc::collect: stopping Cheney scan at offset {} — implausible object \
                 size {} (kind=0x{:02x}, num_slots={}, array_len={}); to_space.used()={}",
                scan_cursor,
                total_size,
                ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                header.num_slots(),
                header.array_length(),
                to_space.used(),
            );
            break;
        }

        // Scan all reference-containing slots
        if header.kind() == ObjectKind::Array {
            // Reference arrays use compact 8-byte pointer storage (REF_ELEMENT_SIZE).
            if header.element_type() == ArrayElementType::Reference {
                for i in 0..header.array_length() as usize {
                    // SAFETY: i < array_length, so HEADER_SIZE + i * the reference
                    // element width is within the allocated object bounds.
                    let s_ptr = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                    // SAFETY: s_ptr points to a valid 8-byte reference slot in the array.
                    let raw: u64 = unsafe { read_ref_slot(s_ptr) };
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
                                write_ref_slot(s_ptr, new_ref_ptr as u64);
                            }
                        }
                    }
                }
            }
        } else {
            if cratonvm_types::is_compact_object(header) {
                // `with_class_layout` rather than `class_layout_for_fields`:
                // this loop only reads `ref_offsets` and would drop the handle
                // immediately, so borrowing the cached layout in place saves an
                // `Arc` clone/drop — two atomic RMWs — per scanned object.
                //
                // `forward_object` re-enters the layout cache (via
                // `object_total_size` -> `object_body_size`) for the *referent's*
                // class on every object it copies. That is supported: the
                // accessor holds only a shared borrow, so the nested lookup
                // still hits the cache. See `with_class_layout`'s doc.
                let _ = cratonvm_types::with_class_layout(
                    header.class_id.as_u32(),
                    header.num_slots(),
                    |layout| {
                        for &offset in &layout.ref_offsets {
                            let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + offset as usize) };
                            let raw = unsafe { read_ref_slot(slot_ptr) };
                            if raw != 0 && from_space.contains(raw as usize as *mut u8) {
                                let new_ref_ptr = forward_object(
                                    from_space,
                                    to_space,
                                    raw as usize as *mut u8,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                );
                                unsafe { write_ref_slot(slot_ptr, new_ref_ptr as u64) };
                            }
                        }
                    },
                );
            } else {
                // The slot count is DERIVED from the extent this scan already
                // validated, not re-read from the header. `total_size` came from
                // `object_total_size(header)`, and the guard above required
                // `scan_cursor + total_size <= to_space.used()` — so those bytes are
                // inside to-space, and inverting the same arithmetic leaves this walk
                // unable to outrun them by construction.
                //
                // It replaces `header.num_slots().min(1 << 24)` (HIB-DCAST-LATEPHASE.1),
                // which is a PLAUSIBILITY clamp and not an extent: 16 M slots is 256 MB
                // of stride, and it was a SECOND load of a header only the FIRST load
                // had been checked in. That is the identical defect
                // `concurrent_mark::scan_object` was fixed for on 2026-08-24, with the
                // identical fix — the semi-space collector is the reader that pass
                // missed. The `1 << 24` bound is unchanged in effect: it is enforced
                // inside `object_total_size`, which yields a size the guard above
                // rejects.
                let num_slots = total_size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;
                for slot_idx in 0..num_slots {
                    // SAFETY: slot_idx * SLOT_SIZE stays inside the validated extent.
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    // Discriminant-screened, like every other legacy-cell reader since the
                    // corrupt-`Value`-cell work: an unchecked transmute of a swept-and-
                    // reused cell yields a `Value` with an out-of-range tag, and the
                    // `if let` below compiles to a jump-table load `[table + disc*4]` with
                    // no bounds check, because Rust guarantees an in-range discriminant.
                    // A corrupt cell decodes to `Value::Object(None)` and is skipped.
                    let value = unsafe {
                        crate::heap::read_value_cell_checked(
                            slot_ptr as *const Value,
                            "gc::cheney_scan",
                        )
                    };
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
                            // SAFETY: slot_ptr names a Value cell inside the copied object.
                            unsafe { std::ptr::write(slot_ptr as *mut Value, new_value) };
                        }
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

/// Sanity bound on a single heap object's total size.
///
/// Mirrors the `MAX_SANE_OBJECT_SIZE` guard in `gen_heap.rs::forward_object`.
/// A corrupt header (e.g. a stale `num_slots`/`array_length`) can inflate the
/// computed `total_size` arbitrarily; without this check the subsequent
/// `to_space.alloc` fails and `forward_object` aborts the whole process. With
/// the check, the corruption is detected and surfaced as a recoverable GC
/// error instead.
const MAX_SANE_OBJECT_SIZE: usize = 64 * 1024 * 1024; // 64 MB

/// Error returned by [`try_forward_object`] when an object cannot be safely copied.
#[derive(Debug)]
pub struct GcError {
    /// Human-readable description of the failure.
    pub message: String,
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
    pointer_map: &mut cratonvm_types::PointerMap,
) -> *mut u8 {
    match try_forward_object(from_space, to_space, old_ptr, objects_copied, pointer_map) {
        Ok(new_ptr) => new_ptr,
        Err(e) => {
            // A genuine to-space OOM (or a corrupt header that slipped past the
            // sanity check) is unrecoverable for the semi-space collector: there
            // is no partial-copy state to unwind. Abort with a clear message.
            eprintln!("FATAL: gc: forward_object: {}", e.message);
            std::process::abort();
        }
    }
}

/// Fallible core of [`forward_object`].
///
/// Returns `Err(GcError)` if the object header looks corrupt (implausibly
/// large `total_size`) or if to-space is genuinely out of memory. Detecting a
/// corrupt header here turns a hard `process::abort()` into a surfaced GC
/// error.
fn try_forward_object(
    from_space: &Arena,
    to_space: &mut Arena,
    old_ptr: *mut u8,
    objects_copied: &mut usize,
    pointer_map: &mut cratonvm_types::PointerMap,
) -> Result<*mut u8, GcError> {
    // SAFETY: old_ptr is a valid heap object in from_space (verified by caller's
    // from_space.contains() check). The header is readable for the duration of GC.
    //
    // Aliasing-safety fix: access the old header exclusively through raw
    // pointers. Holding a shared `&ObjectHeader` here while later taking a
    // `&mut ObjectHeader` to the same address (to install the forwarding
    // pointer) would be two live references — one mutable — to overlapping
    // memory, which is undefined behavior under Rust's aliasing rules.
    let old_header_ptr = old_ptr as *const ObjectHeader;

    // Already forwarded?
    // SAFETY: `old_header_ptr` points at a valid, fully initialized header.
    if unsafe { (*old_header_ptr).is_forwarded() } {
        return Ok(unsafe { (*old_header_ptr).forwarding_address() });
    }

    // HIB-DCAST-LATEPHASE.1: `from_space.contains()` (the caller's only check
    // on `old_ptr`) is a bare bounds check, not proof `old_ptr` is a real
    // object base — a conservative/stale root can land mid-object. Below,
    // `header_copy`'s construction reads `kind`/`element_type` as TYPED
    // `#[repr(u8)]` enums via `ptr::read`; loading an out-of-range
    // discriminant is immediate UB the instant that happens, and optimized
    // code can lower it into a hardware trap rather than anything
    // recoverable — the same defect class already fixed in
    // `gen_heap.rs::forward_object_impl`, `OldGen::scan_region`, and
    // `scan_object_for_old_refs` (see
    // `fixed-suite-bugs/hibernate/defaultcatalogandschema-late-phase-instability-20260801-FIXED.md`).
    // Validate the raw tag bytes before ever reading either field as a typed
    // enum.
    // SAFETY: `old_ptr` is confirmed inside `from_space` by the caller, so
    // the two single-byte tag reads at the fixed header offsets are
    // in-bounds; reading a raw `u8` has no validity requirement beyond
    // in-bounds-and-readable.
    let kind_tag = unsafe { cratonvm_types::kind_tag_at(old_ptr) };
    let elem_tag = unsafe { cratonvm_types::element_type_tag_at(old_ptr) };
    if cratonvm_types::object_kind_from_tag(kind_tag).is_none()
        || cratonvm_types::array_element_type_from_tag(elem_tag).is_none()
    {
        return Err(GcError {
            message: format!(
                "invalid kind/element_type tag (kind_tag={kind_tag}, elem_tag={elem_tag}) at \
                 {old_ptr:p} — corrupt header or non-base address, refusing to copy"
            ),
        });
    }

    // Build an owned copy of the header so `object_total_size` operates on an
    // owned value rather than a borrow that would alias the later `&mut`.
    //
    // ATOMIC-UB fix: do NOT `std::ptr::read` the whole `ObjectHeader` by
    // value. `ObjectHeader` embeds `mark_word: AtomicU64`; a `ptr::read` of a
    // struct containing an atomic performs a *non-atomic* read of an atomic
    // location, which is undefined behavior. Read each scalar field
    // individually through field-projected raw pointers, and read `mark_word`
    // via an explicit `AtomicU64::load`, then reconstruct an owned header.
    // SAFETY: `old_header_ptr` points at a valid, fully initialized header;
    // the kind/element_type tag bytes were just validated above, so reading
    // them as typed enums here is sound.
    let header_copy: ObjectHeader = unsafe {
        let h = old_header_ptr;
        let mut owned = ObjectHeader::new(
            std::ptr::addr_of!((*h).class_id).read(),
            (*h).kind(),
            (*h).element_type(),
            0,
            0,
        );
        owned.shape = std::ptr::addr_of!((*h).shape).read();
        owned.set_gc_age((*h).gc_age());
        owned.set_gc_flags((*h).gc_flags());
        // `mark_word` is an `AtomicU64`: read it through an atomic load.
        owned.mark_word.store(
            (*h).mark_word.load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        owned
    };
    let total_size = object_total_size(&header_copy);

    // Sanity: a corrupt header (stale num_slots / array_length) can inflate
    // `total_size` arbitrarily. Detect that here — matching the
    // `MAX_SANE_OBJECT_SIZE` guard in `gen_heap.rs::forward_object` — so a
    // bad header is surfaced as a recoverable GC error instead of being
    // converted into a hard process abort by the to-space OOM path below.
    if !(HEADER_SIZE..=MAX_SANE_OBJECT_SIZE).contains(&total_size) {
        return Err(GcError {
            message: format!(
                "implausible object size {} bytes at {:p} (kind={:?}, \
                 num_slots={}, array_len={}) — corrupt header, refusing to copy",
                total_size,
                old_ptr,
                header_copy.kind(),
                header_copy.num_slots(),
                header_copy.array_length(),
            ),
        });
    }

    // Allocate in to-space
    let new_ptr = match to_space.alloc(total_size, 8) {
        Some(p) => p,
        None => {
            return Err(GcError {
                message: format!(
                    "to-space OOM allocating {} bytes (to-space {}/{} used)",
                    total_size,
                    to_space.used(),
                    to_space.capacity(),
                ),
            });
        }
    };

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
    // Clear the forwarding pointer in the NEW copy (it's a fresh object).
    // Write through a raw pointer so no `&mut ObjectHeader` is ever live
    // alongside any other reference to this header.
    // (The destination needs no forwarding clear: forwarding now lives in the
    // mark word, and the copy above carried the source's PRE-forward mark word,
    // which by construction is not in the `FORWARDED` state — the source is
    // clobbered only below, after this copy.)

    // SAFETY: old_ptr is still valid in from_space (not freed yet) and we have
    // exclusive access during STW GC. Install forwarding pointer for future
    // lookups. Written through a raw pointer to avoid aliasing the shared
    // header view used earlier in this function.
    unsafe {
        (*(old_ptr as *const ObjectHeader)).set_forwarding_address(new_ptr);
    }

    // Track the mapping
    pointer_map.insert(old_ptr as usize, new_ptr as usize);
    *objects_copied += 1;

    // Ensure the from_space check in the caller still works:
    // We just modified old_ptr's header (which is in from_space). The
    // from_space.contains() check uses the arena's data bounds, so this is fine.
    debug_assert!(from_space.contains(old_ptr));

    Ok(new_ptr)
}

/// Compute the total size of a heap object (header + data).
///
/// Objects use num_slots * SLOT_SIZE. Arrays use compact element sizes.
///
/// Defensive corruption handling (gc-gc fix): a corrupt / implausible array
/// header (e.g. a stale `array_length` so large that `header + length *
/// element_size` overflows `usize`) must NOT abort the whole VM. The moving
/// collector previously `.expect()`-panicked here, killing the process on a
/// bad header, whereas the non-moving sweep (`gen_heap.rs::gen_object_total_size`)
/// returns a `0` sentinel and lets its walker re-sync. Mirror that behavior:
/// on overflow, log a diagnostic and return `0`. `0 < HEADER_SIZE`, so every
/// caller's existing corruption guard treats it as a bad header:
///   - `try_forward_object` (the `total_size < HEADER_SIZE` check) surfaces a
///     recoverable `GcError` instead of copying garbage;
///   - the Cheney-scan loops (in `collect` / `collect_with_finalizers`) detect
///     the implausible stride and stop the walk defensively rather than
///     advancing the cursor by 0 and spinning forever.
///
/// This does not mask genuine bugs silently — the corruption is logged — but
/// it converts a hard process abort into a recoverable / fail-safe path.
pub fn object_total_size(header: &ObjectHeader) -> usize {
    if header.kind() == ObjectKind::Array {
        match array_data_size(header.array_length() as usize, header.element_type()) {
            Ok(data) => ARRAY_DATA_OFFSET + data,
            Err(_) => {
                // Implausible array header — treat as corrupt. Return 0 so the
                // caller's `total_size < HEADER_SIZE` guard fires (matching the
                // non-moving sweep's re-sync contract) instead of panicking.
                tracing::warn!(
                    "gc: implausible array_length {} (element_type={:?}) in moving-collector \
                     object header — treating as corrupt; caller will skip/stop the walk",
                    header.array_length(),
                    header.element_type(),
                );
                0
            }
        }
    } else {
        // object_body_size() honours the per-object GC_FLAG_COMPACT bit: a
        // compact-layout instance stores its true body size (ref fields = 8B,
        // primitive fields = 16B, packed by declared field, NOT a uniform
        // num_slots*SLOT_SIZE) in the header's array_length. Using the plain
        // num_slots*SLOT_SIZE legacy formula unconditionally here OVER-sizes
        // every compact object (up to 8 bytes wasted per reference field),
        // which corrupted evacuation: evacuate_object()/its equivalent below
        // used this inflated size both to reserve destination space AND as
        // the copy_nonoverlapping() length, desyncing every subsequent
        // object's stride through the region from its actual body size.
        // Root-caused via the JavaPoet LineWrapper NPE
        // (CRATONVM-SPRING-GENUINE-BUGLIST): LineWrapper
        // mixes ref/primitive fields with its LAST field (nextFlush, a ref)
        // landing at a compact byte offset the legacy formula never accounted
        // for. Mirrors the already-correct gen_heap.rs::gen_object_total_size.
        HEADER_SIZE + crate::object_body_size(header)
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
///
/// The same root-remapping contract as [`collect`] applies: only the `roots`
/// slice is remapped in place; VM-external references must be remapped by the
/// caller using the returned [`GcResult::pointer_map`].
pub fn collect_with_finalizers(
    from_space: &mut Arena,
    to_space: &mut Arena,
    roots: &mut [ObjectRef],
    finalizer_addrs: &[usize],
) -> (GcResult, Vec<usize>) {
    fire_gc_start();
    let bytes_before = from_space.used();
    let mut objects_copied: usize = 0;
    let mut pointer_map = cratonvm_types::PointerMap::default();

    // Phase 1: Forward all root objects (same as collect)
    for root in roots.iter_mut() {
        let old_ptr = root.as_ptr();
        if !from_space.contains(old_ptr) {
            continue;
        }
        let new_ptr = forward_object(
            from_space,
            to_space,
            old_ptr,
            &mut objects_copied,
            &mut pointer_map,
        );
        *root = unsafe { ObjectRef::from_raw(new_ptr) };
    }

    // Contract check: see `collect`. After Phase 1 no root may still point
    // into from-space; external references are remapped by the caller.
    debug_assert!(
        roots.iter().all(|r| !from_space.contains(r.as_ptr())),
        "gc::collect_with_finalizers: a root still points into from-space \
         after Phase 1 — every passed root must be remapped before reset",
    );

    // Phase 2: Cheney scan (same as collect)
    let mut scan_cursor: usize = 0;
    while scan_cursor < to_space.used() {
        let obj_ptr = unsafe { to_space.base_ptr_mut().add(scan_cursor) };
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let total_size = object_total_size(header);

        // Defensive: see the matching guard in `collect`. A corrupt header
        // yields total_size 0 (or an implausibly large stride); never advance
        // the cursor by it — stop the walk with a diagnostic instead of
        // spinning forever / aborting the VM.
        if total_size < HEADER_SIZE || scan_cursor + total_size > to_space.used() {
            tracing::warn!(
                "gc::collect_with_finalizers: stopping Cheney scan at offset {} — implausible \
                 object size {} (kind=0x{:02x}, num_slots={}, array_len={}); to_space.used()={}",
                scan_cursor,
                total_size,
                ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                header.num_slots(),
                header.array_length(),
                to_space.used(),
            );
            break;
        }

        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                for i in 0..header.array_length() as usize {
                    let s_ptr = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                    let raw: u64 = unsafe { read_ref_slot(s_ptr) };
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
                            unsafe {
                                write_ref_slot(s_ptr, new_ref_ptr as u64);
                            }
                        }
                    }
                }
            }
        } else {
            if cratonvm_types::is_compact_object(header) {
                // `with_class_layout` rather than `class_layout_for_fields`:
                // this loop only reads `ref_offsets` and would drop the handle
                // immediately, so borrowing the cached layout in place saves an
                // `Arc` clone/drop — two atomic RMWs — per scanned object.
                //
                // `forward_object` re-enters the layout cache (via
                // `object_total_size` -> `object_body_size`) for the *referent's*
                // class on every object it copies. That is supported: the
                // accessor holds only a shared borrow, so the nested lookup
                // still hits the cache. See `with_class_layout`'s doc.
                let _ = cratonvm_types::with_class_layout(
                    header.class_id.as_u32(),
                    header.num_slots(),
                    |layout| {
                        for &offset in &layout.ref_offsets {
                            let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + offset as usize) };
                            let raw = unsafe { read_ref_slot(slot_ptr) };
                            if raw != 0 && from_space.contains(raw as usize as *mut u8) {
                                let new_ref_ptr = forward_object(
                                    from_space,
                                    to_space,
                                    raw as usize as *mut u8,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                );
                                unsafe { write_ref_slot(slot_ptr, new_ref_ptr as u64) };
                            }
                        }
                    },
                );
            } else {
                // Extent-derived count and screened decode: see the matching
                // scan arm in `collect`.
                let num_slots = total_size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;
                for slot_idx in 0..num_slots {
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe {
                        crate::heap::read_value_cell_checked(
                            slot_ptr as *const Value,
                            "gc::cheney_scan",
                        )
                    };
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
                            let new_value =
                                Value::Object(Some(unsafe { ObjectRef::from_raw(new_ref_ptr) }));
                            unsafe {
                                std::ptr::write(slot_ptr as *mut Value, new_value);
                            }
                        }
                    }
                }
            }
        }
        scan_cursor += total_size;
    }

    // Phase 3 + 3b (fixpoint): Resurrect dead finalizable objects and Cheney-
    // scan their transitive references. The naive single-pass formulation
    // (Phase 3 once, Phase 3b once) misses the case where a resurrected
    // finalizable object references *another* finalizable object that is also
    // unreachable from normal roots. The Cheney scan would copy that second
    // object as a follow-up reference of the first (so the slot is fixed up),
    // but the second object's `dead_finalizers` entry would never be recorded
    // — its `finalize()` would silently not run. We loop:
    //   (a) for every finalizer_addr not yet forwarded, resurrect it and
    //       record it in `dead_finalizers`;
    //   (b) Cheney-continue from `scan_cursor` until to-space is fully
    //       scanned, which may forward additional finalizable objects
    //       through normal field references;
    // and we stop when (a) finds no new unforwarded finalizable object AND
    // the Cheney scan has nothing left to chew on.
    let mut dead_finalizers = Vec::new();
    let mut dead_finalizers_set: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    loop {
        let mut newly_resurrected = false;

        // Phase 3: resurrect any finalizer_addr that is still unreachable
        // (either it was never forwarded, or the prior Cheney pass copied it
        // through a transitive ref but we haven't yet recorded it as a dead
        // finalizer that needs `finalize()` invocation).
        for &old_addr in finalizer_addrs {
            let old_ptr = old_addr as *mut u8;
            if !from_space.contains(old_ptr) {
                continue;
            }
            if pointer_map.contains_key(&old_addr) {
                // Already forwarded — either it was reachable from a real
                // root (skip, no finalize) or it was reached transitively
                // from a previously-resurrected finalizer. We can't easily
                // distinguish the two here without a separate "root-
                // reachable" set; the safe over-approximation is to skip
                // (matches the original Phase 3 semantics, which checks
                // `pointer_map.contains_key`). Skipping a transitively-
                // forwarded finalizer means its `finalize()` does not run
                // when it would have under a hypothetical strictly-correct
                // implementation — but the object IS kept alive, which is
                // the more critical invariant.
                continue;
            }
            let new_ptr = forward_object(
                from_space,
                to_space,
                old_ptr,
                &mut objects_copied,
                &mut pointer_map,
            );
            let new_addr = new_ptr as usize;
            if dead_finalizers_set.insert(new_addr) {
                dead_finalizers.push(new_addr);
                newly_resurrected = true;
            }
        }

        // Phase 3b: Cheney-scan everything newly copied.
        let scan_made_progress = scan_cursor < to_space.used();
        while scan_cursor < to_space.used() {
            let obj_ptr = unsafe { to_space.base_ptr_mut().add(scan_cursor) };
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let total_size = object_total_size(header);

            // Defensive: see the matching guard in `collect`. Stop the inner
            // scan on an implausible / zero-size header rather than advancing
            // by a 0 stride (infinite loop) or aborting the VM. Force the
            // cursor to the end so the abandoned tail is not re-scanned and the
            // OUTER `loop`'s `scan_made_progress` (re-evaluated as
            // `scan_cursor < to_space.used()`) goes false on the next pass —
            // otherwise a stuck cursor below `used` spins the outer loop forever.
            if total_size < HEADER_SIZE || scan_cursor + total_size > to_space.used() {
                tracing::warn!(
                    "gc::collect_with_finalizers: stopping Phase-3b Cheney scan at offset {} — \
                     implausible object size {} (kind=0x{:02x}, num_slots={}, array_len={}); \
                     to_space.used()={}",
                    scan_cursor,
                    total_size,
                    ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                    header.num_slots(),
                    header.array_length(),
                    to_space.used(),
                );
                scan_cursor = to_space.used();
                break;
            }

            if header.kind() == ObjectKind::Array {
                if header.element_type() == ArrayElementType::Reference {
                    for i in 0..header.array_length() as usize {
                        let s_ptr = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                        let raw: u64 = unsafe { read_ref_slot(s_ptr) };
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
                                unsafe {
                                    write_ref_slot(s_ptr, new_ref_ptr as u64);
                                }
                            }
                        }
                    }
                }
            } else {
                if cratonvm_types::is_compact_object(header) {
                    // Borrowing accessor: see the identical scan arm above for
                    // why, and for why the nested `forward_object` re-entry into
                    // the layout cache is safe.
                    let _ = cratonvm_types::with_class_layout(
                        header.class_id.as_u32(),
                        header.num_slots(),
                        |layout| {
                            for &offset in &layout.ref_offsets {
                                let slot_ptr =
                                    unsafe { obj_ptr.add(HEADER_SIZE + offset as usize) };
                                let raw = unsafe { read_ref_slot(slot_ptr) };
                                if raw != 0 && from_space.contains(raw as usize as *mut u8) {
                                    let new_ref_ptr = forward_object(
                                        from_space,
                                        to_space,
                                        raw as usize as *mut u8,
                                        &mut objects_copied,
                                        &mut pointer_map,
                                    );
                                    unsafe { write_ref_slot(slot_ptr, new_ref_ptr as u64) };
                                }
                            }
                        },
                    );
                } else {
                    // Extent-derived count and screened decode: see the matching
                    // scan arm in `collect`.
                    let num_slots = total_size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;
                    for slot_idx in 0..num_slots {
                        let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                        let value = unsafe {
                            crate::heap::read_value_cell_checked(
                                slot_ptr as *const Value,
                                "gc::cheney_scan",
                            )
                        };
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
                                let new_value = Value::Object(Some(unsafe {
                                    ObjectRef::from_raw(new_ref_ptr)
                                }));
                                unsafe {
                                    std::ptr::write(slot_ptr as *mut Value, new_value);
                                }
                            }
                        }
                    }
                }
            }
            scan_cursor += total_size;
        }

        if !newly_resurrected && !scan_made_progress {
            break;
        }
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
pub fn update_value_ref(value: &mut Value, pointer_map: &cratonvm_types::PointerMap) {
    if let Value::Object(Some(ref mut obj_ref)) = value {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by the GC's pointer map and points
            // into to-space where a valid object was copied by forward_object.
            // The debug_assert verifies non-null during development.
            debug_assert!(
                new_addr != 0,
                "gc: update_value_ref: pointer map contains null address"
            );
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
    use cratonvm_types::ClassId;

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
}
