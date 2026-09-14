// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Software watchpoint for the bc-math-ec `0x4` corruption (session
//! "continue investigating", 2026-06-05).
//!
//! Established facts (see `bc-math-ec-jit-miscompile-investigation.md`):
//! a mutator path raw-writes the small value `4` into a heap **reference
//! field's payload-low** half, bypassing `set_field`/`set_array_element` and
//! the GC. The victim object is essentially random (X9*/ECCurve, but also JDK
//! `HexFormat`/`Level`) — it is whatever object occupies the target address —
//! so we watch the few-instance EC holders and catch the write whichever run
//! lands on one.
//!
//!   1. When a *valid* reference is stored into a watched EC object's reference
//!      field (interpreter `Putfield`), [`record`] the `(ObjectRef, field_idx,
//!      expected_ptr)`.
//!   2. After every native dispatch (`safe_native_call`) and at GC entry,
//!      [`detect`] re-reads each watched field; any that flipped to a non-zero
//!      `< 0x1000` value is the corruption.
//!   3. On GC, [`remap`] rewrites each watched `ObjectRef` through the
//!      collector's `pointer_map` (a moving collector relocates survivors), so
//!      watches PERSIST across GCs — the corruption often hits an object that
//!      survived the GC in which its field was first written. (The earlier
//!      clear-on-GC version missed every such case.)
//!
//! Entirely gated behind `CRATONVM_DBG_ECWATCH`; zero cost when unset.

use cratonvm_types::ObjectRef;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Cached env gate. Off ⇒ every entry point is a single predicted branch.
#[inline]
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ECWATCH").is_some())
}

/// Secondary gate for the EXPENSIVE per-native detect (re-reads the whole list
/// after every `safe_native_call`). Off by default even when [`enabled`] is on;
/// the cheap GC-entry detect already confirms a corruption happened and names
/// the field. Flip this on to pin the exact native.
#[inline]
pub fn native_enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ECWATCH_NATIVE").is_some()
    })
}

/// table of `(holder, field_idx, expected_pointer, holder_class_id)`. The
/// `class_id` (header offset 0, `repr(C)`) lets [`detect`] re-validate the
/// remapped holder's identity, ruling out a remap-false-positive (holder
/// reclaimed + its address reused by a different object).
fn table() -> &'static Mutex<Vec<(ObjectRef, u32, usize, u32, usize)>> {
    static T: OnceLock<Mutex<Vec<(ObjectRef, u32, usize, u32, usize)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Vec::new()))
}

const CAP: usize = 1 << 15;

/// Current number of live watches (diagnostic).
pub fn size(vm: usize) -> usize {
    if !enabled() {
        return 0;
    }
    table().lock().iter().filter(|e| e.4 == vm).count()
}

/// Read the `class_id` (u32 at header offset 0) of the object at `addr`.
#[inline]
fn class_id_at(addr: usize) -> u32 {
    // SAFETY: `addr` is a watched holder (remapped on every GC); the managed
    // arenas stay mapped, so reading 4 bytes at offset 0 cannot fault.
    unsafe { std::ptr::read(addr as *const u32) }
}

/// Record a watched reference field. De-duplicated by `(holder, field_idx)`
/// (latest store wins). `class_id` is the holder's class for identity revalidation.
pub fn record(vm: usize, holder: ObjectRef, field_idx: usize, expected: usize, class_id: u32) {
    if !enabled() || expected < 0x1000 {
        return;
    }
    let idx = field_idx as u32;
    let key = holder.as_ptr() as usize;
    let mut t = table().lock();
    if let Some(e) = t
        .iter_mut()
        .find(|(o, i, _, _, v)| o.as_ptr() as usize == key && *i == idx && *v == vm)
    {
        e.2 = expected;
        e.3 = class_id;
        return;
    }
    if t.len() < CAP {
        t.push((holder, idx, expected, class_id, vm));
    }
}

/// Slot (16-byte `Value` cell) address for a holder + field index. Mirrors
/// `gen_heap::slot_ptr`: `obj + HEADER_SIZE + idx*SLOT_SIZE`. The `Value`
/// discriminant is byte 0 (low-32 == 4 for `Object`); the payload pointer is at
/// `+8`.
#[inline]
fn slot_addr(holder: ObjectRef, idx: u32) -> usize {
    use cratonvm_gc::heap::SLOT_SIZE;
    use cratonvm_gc::is_compact_object;
    use cratonvm_types::{class_layout_for_fields, ObjectHeader};

    let header = unsafe { &*(holder.as_ptr() as *const ObjectHeader) };
    if is_compact_object(header) {
        if let Some(layout) = class_layout_for_fields(header.class_id.as_u32(), header.num_slots())
        {
            if let Some(offset) = layout.field_offset(idx as usize) {
                return header_base(holder) + offset as usize;
            }
        }
    }

    header_base(holder) + (idx as usize) * SLOT_SIZE
}

#[inline]
fn header_base(holder: ObjectRef) -> usize {
    use cratonvm_gc::heap::HEADER_SIZE;
    holder.as_ptr() as usize + HEADER_SIZE
}

/// `Value` discriminant value for the `Object` variant (Int=0, Long=1, Float=2,
/// Double=3, Object=4) — see bc-math-ec-jit-miscompile-investigation.md.
const VALUE_DISC_OBJECT: u32 = 4;

/// Re-read every watched field; return `(holder_addr, field_idx, expected, now)`
/// for any whose `Value` is now `Object(Some(p))` with `p` non-zero and
/// `< 0x1000` (the `0x4` corruption signature) — DISCRIMINANT-CHECKED so a field
/// legitimately re-assigned to `Int(4)`/`Long(4)` (tag != Object, value 4 at
/// +8) is NOT a false positive. Entries that are no longer a non-null `Object`
/// (null, reclaimed-zeroed, or re-typed) are pruned to keep the list bounded;
/// reported entries are also dropped (once is enough).
pub fn detect(vm: usize) -> Vec<(usize, u32, usize, usize)> {
    if !enabled() {
        return Vec::new();
    }
    let mut t = table().lock();
    let mut hits = Vec::new();
    t.retain(|&(holder, idx, expected, class_id, entry_vm)| {
        // Another VM's entry: not ours to validate, and its addresses belong
        // to a different heap. Keep it, report nothing.
        if entry_vm != vm {
            return true;
        }
        // IDENTITY RE-VALIDATION: if the holder's current class_id no longer
        // matches the watched one, the holder was reclaimed and its address
        // reused by a different object (or relocated without our remap seeing
        // it) — a remap false-positive. Drop it rather than reporting a hit.
        if class_id_at(holder.as_ptr() as usize) != class_id {
            return false;
        }
        // SAFETY: `holder` is remapped on every GC (`remap`), so its address
        // is current managed memory; semispaces / old-gen stay mapped.
        let slot_ptr = slot_addr(holder, idx);
        let header = unsafe { &*(holder.as_ptr() as *const cratonvm_types::ObjectHeader) };
        let is_compact_slot = cratonvm_gc::is_compact_object(header)
            && cratonvm_types::class_layout_for_fields(
                header.class_id.as_u32(),
                header.num_slots(),
            )
            .and_then(|layout| layout.field_is_ref(idx as usize))
            .unwrap_or(false);

        if is_compact_slot {
            let now = unsafe { std::ptr::read(slot_ptr as *const usize) };
            if now == 0 {
                return false; // Object(None) — prune
            }
            if now != expected && now < 0x1000 {
                hits.push((holder.as_ptr() as usize, idx, expected, now));
                return false; // reported — drop
            }
            true
        } else {
            // SAFETY: legacy path reads a 16-byte field cell and validates the
            // Value::Object tag before using payload bytes.
            let bytes = unsafe { std::ptr::read(slot_ptr as *const [u8; 16]) };
            let tag = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if tag != VALUE_DISC_OBJECT {
                return false; // re-typed / reclaimed — prune
            }
            let now = usize::from_ne_bytes(bytes[8..16].try_into().unwrap());
            if now == 0 {
                return false; // Object(None) — prune
            }
            if now != expected && now < 0x1000 {
                hits.push((holder.as_ptr() as usize, idx, expected, now));
                return false; // reported — drop
            }
            true
        }
    });
    hits
}

/// Rewrite each watched holder through the collector's `pointer_map` after a
/// GC. MUST be called on every GC instead of clearing — a moving collector
/// relocates survivors, and the corruption frequently hits a survivor.
pub fn remap(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if !enabled() || pointer_map.is_empty() {
        return;
    }
    let mut t = table().lock();
    // Only THIS VM's entries: the table is process-global and a foreign VM's
    // relocation map names addresses in a different heap, so applying it here
    // would rewrite our holders to addresses that were never ours.
    for (holder, _, _, _, v) in t.iter_mut().filter(|e| e.4 == vm) {
        if let Some(&new) = pointer_map.get(&(holder.as_ptr() as usize)) {
            // SAFETY: `new` is a live post-GC heap address from the collector's
            // relocation map.
            *holder = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }
}
