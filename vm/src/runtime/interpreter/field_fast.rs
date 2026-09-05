// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//! Quickened instance-field access and primitive array access for the
//! interpreter's fast-path arms.
//!
//! # Why
//!
//! `op_getfield` / `op_putfield` are the spec-complete handlers: they resolve
//! the field reference through the loader-aware resolver, retarget it for the
//! loader-split case, forward the receiver twice, run every field diagnostic,
//! hash the method name for JVMTI, and read or write through
//! `VmHeap::get_field` / `set_field`, which re-validates the receiver, checks
//! the field index, resolves the compact layout through a thread-local cache
//! and clones an `Arc<CompactLayout>` before touching memory. Measured
//! against HotSpot's template interpreter on 2026-09-02 that was 160-200 ns
//! per instance field access against ~1 ns — the worst ratio of any
//! interpreter operation.
//!
//! Almost all of that is constant per call site and per receiver class. This
//! module memoizes it on the site: once the slow handler has resolved a field
//! for a receiver, it records the receiver's `(class id, num_slots)` shape and
//! the byte offset of the field inside it, and the next access whose receiver
//! header matches loads or stores at that offset directly.
//!
//! # The two body layouts, and why both are here
//!
//! A ZGC object body is one of two things, and which one it is depends on how
//! it was allocated, not on the class:
//!
//! * a **compact** body — fields packed at the offsets of the class's
//!   registered `CompactLayout`, each in its natural width, marked
//!   `GC_FLAG_COMPACT` in the header. `GarbageCollector::alloc_object` builds
//!   these when the class has a layout.
//! * a **legacy** body — one 16-byte tagged `Value` cell per field, no flag.
//!   `ZgcRealHeap::try_alloc_object`, the TLAB path
//!   `interpreter::alloc_object_shared` takes, builds *only* these: it sizes
//!   the allocation `num_fields * SLOT_SIZE` and never calls
//!   `set_compact_shape`. So on this collector essentially every object the
//!   interpreter allocates is legacy, and a fast path that handled compact
//!   bodies alone measured `fast-field: get hit=0 miss=1801267` on
//!   `probes/FieldShape.java` — it never fired once.
//!
//! Both are handled here, chosen per site by what the receiver's header says
//! and re-checked on every access. The compact arm reads and writes the field
//! at its packed width; the legacy arm reads and writes the whole cell with
//! `read_value_checked_atomic` / `write_value_atomic`, which is what
//! `ZgcRealHeap::get_field` / `set_field` do for it — the same checked decode,
//! the same padding-free store.
//!
//! # Correctness contract
//!
//! * A site is only filled for a **non-static, non-volatile** field on a
//!   **ZGC** heap, and for a compact receiver only when the storage kind the
//!   layout assigned agrees with the field descriptor. Anything else keeps the
//!   full handler.
//! * The fast arm re-checks the receiver's `class_id`, `num_slots` and compact
//!   flag against the site (the compact layout registry is keyed by exactly
//!   `(class_id, field_count)`, and the flag says which body shape the object
//!   actually has). A mismatch falls back to the full handler, which refills
//!   the site for the new receiver. Polymorphic sites therefore thrash the
//!   memo and run mostly slow, which is the same shape as the monomorphic
//!   inline cache for invokes.
//! * Sites are epoch-validated by `SiteCache` (class definition epoch and
//!   resolution epoch) and dropped wholesale on class redefinition, exactly
//!   like `field_sites`.
//! * Every diagnostic the slow handlers honour per access
//!   (`CRATONVM_DBG_FIELD*`, the corrupt-cell watch, the punned-store watch,
//!   the vacated-frames ledger, the remap trace, the stray-stack probe, the
//!   EC watch) and every JVMTI field watchpoint turns the fast arms off, so an
//!   armed run sees exactly what it saw before.
//! * A `null` or non-object receiver falls back so the helpful-NPE message is
//!   built by the code that owns it.
//! * Reference loads keep the receiver-side `load_and_forward` on the loaded
//!   value, and a heap that has ever minted an autobox wrapper
//!   (`cratonvm_gc::autobox::wrapper_exists`) keeps compact reference loads
//!   slow, so the unboxing `get_field` performs is never skipped.
//! * Reference stores run the SATB pre-barrier while marking is active and the
//!   generational card note afterwards — the two things `ZgcRealHeap::
//!   set_field` does around the raw write, in that order. ZGC leaves
//!   `GarbageCollector::write_barrier_pre` at its empty default, so the
//!   explicit call `op_putfield` makes before the store is a no-op here and
//!   has no fast-path counterpart.
//! * Category-2 stores only take the fast path when the operand-stack kind
//!   mark says the slot was pushed by a genuine `long` / `double` producer;
//!   the slow handler owns every decode heuristic for the unmarked shapes.
//! * A legacy cell that fails `read_value_checked_atomic` (a corrupt cell)
//!   falls back, so the corrupt-cell census and its report stay with the
//!   handler that owns them.
//!
//! Kill switch: `CRATONVM_JIT_NO_FIELD_FAST_PATH=1` (`CRATONVM_JIT=
//! -field-fast-path`). Engagement: `CRATONVM_DBG_FIELD_SITE=1` prints
//! `fast-field: get hit/miss/fill put hit/miss/fill unusable`, and names the
//! first few reasons a site could not be quickened.

use super::site_cache::{site_stats, FastFieldSite, FastFieldSiteCache, FieldSiteCache};
use crate::runtime::frame::Frame;
use crate::runtime::value_stack::ValueStack;
use crate::threading::JvmThread;
use crate::vm::SharedVm;
use cratonvm_classloading::resolution::ResolvedField;
use cratonvm_gc::zgc::ZgcRealHeap;
use cratonvm_gc::{ArrayElementType, ObjectHeader, ObjectKind};
use cratonvm_types::{
    ClassId, CompactValue, FieldStorageKind, ObjectRef, Value, HEADER_SIZE, SLOT_SIZE,
};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Per-`execute_frame` admission for the quickened field and array arms.
///
/// Returns the ZGC heap the arms address memory through, or `None` when the
/// arms must stay off for this frame: another collector, the kill switch, or
/// any per-access diagnostic the slow handlers would have honoured.
#[inline]
pub(super) fn fast_field_zgc(shared: &SharedVm) -> Option<&ZgcRealHeap> {
    if crate::runtime::env_cache::no_field_fast_path() {
        return None;
    }
    let zgc = match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Zgc(h) => &**h,
        _ => return None,
    };
    if crate::runtime::env_cache::any_field_diag()
        || crate::runtime::env_cache::dbg_field_watch()
        || crate::runtime::env_cache::corrupt_cell_dbg()
        || crate::runtime::env_cache::corrupt_cell_selftest()
        || super::straystack_enabled()
        || super::remap_trace_on()
        || crate::runtime::ec_watch::enabled()
        || cratonvm_gc::zgc::punned_store_watch_armed()
        || cratonvm_gc::gc_quiescence::vacated_frames_enabled()
    {
        return None;
    }
    Some(zgc)
}

/// The raw field accessors below all carry the same obligation, stated once
/// here and referenced by each.
///
/// # Safety
///
/// `p` must be a field address produced by [`field_ptr_for`] for a receiver it
/// accepted, and the width must be the one that receiver's layout records for
/// that field. `field_ptr_for` is what discharges it: it refuses an address the
/// heap does not know (`is_object_address`), then refuses a header whose class
/// id, slot count or compact flag disagrees with the site — so the offset it
/// adds belongs to the layout the site was filled against, and the bytes are
/// inside a live object body on a committed page.
///
/// Every access is `Relaxed` through an atomic of the field's own width, which
/// is what makes a torn read impossible when another thread writes the same
/// slot; ordering is the interpreter's business, not this helper's.
///
/// Passing a pointer from anywhere else — a stale operand-stack value, an
/// address into a decommitted span, a width that disagrees with the layout —
/// is undefined behaviour, and reads as silent field corruption rather than a
/// fault.
#[inline(always)]
unsafe fn load_u8(p: *mut u8) -> u8 {
    (*(p as *const AtomicU8)).load(Ordering::Relaxed)
}
/// # Safety
///
/// As [`load_u8`]: `p` must come from [`field_ptr_for`], at this width.
#[inline(always)]
unsafe fn load_u16(p: *mut u8) -> u16 {
    (*(p as *const AtomicU16)).load(Ordering::Relaxed)
}
/// # Safety
///
/// As [`load_u8`]: `p` must come from [`field_ptr_for`], at this width.
#[inline(always)]
unsafe fn load_u32(p: *mut u8) -> u32 {
    (*(p as *const AtomicU32)).load(Ordering::Relaxed)
}
/// # Safety
///
/// As [`load_u8`]: `p` must come from [`field_ptr_for`], at this width.
#[inline(always)]
unsafe fn load_u64(p: *mut u8) -> u64 {
    (*(p as *const AtomicU64)).load(Ordering::Relaxed)
}
/// # Safety
///
/// As [`load_u8`]: `p` must come from [`field_ptr_for`], at this width.
#[inline(always)]
unsafe fn store_u8(p: *mut u8, v: u8) {
    (*(p as *const AtomicU8)).store(v, Ordering::Relaxed)
}
/// # Safety
///
/// As [`load_u8`]: `p` must come from [`field_ptr_for`], at this width.
#[inline(always)]
unsafe fn store_u16(p: *mut u8, v: u16) {
    (*(p as *const AtomicU16)).store(v, Ordering::Relaxed)
}
/// # Safety
///
/// As [`load_u8`]: `p` must come from [`field_ptr_for`], at this width.
#[inline(always)]
unsafe fn store_u32(p: *mut u8, v: u32) {
    (*(p as *const AtomicU32)).store(v, Ordering::Relaxed)
}
/// # Safety
///
/// As [`load_u8`]: `p` must come from [`field_ptr_for`], at this width.
#[inline(always)]
unsafe fn store_u64(p: *mut u8, v: u64) {
    (*(p as *const AtomicU64)).store(v, Ordering::Relaxed)
}

/// Raw reference slot read, honouring compressed oops the way
/// `read_compact_field` does.
///
/// # Safety
///
/// As [`load_u8`], and the slot must be a REFERENCE slot: the width read is
/// decided by `narrow_oops_enabled()` rather than by the argument, so reading a
/// primitive slot through this would use the wrong width whenever compressed
/// oops are on.
#[inline(always)]
unsafe fn load_ref(p: *mut u8) -> u64 {
    if cratonvm_types::narrow_oop::narrow_oops_enabled() {
        cratonvm_types::narrow_oop::decode(load_u32(p))
    } else {
        load_u64(p)
    }
}

/// Raw reference slot write, honouring compressed oops the way
/// `write_compact_field` does (probe included).
///
/// # Safety
///
/// As [`load_ref`]. `raw` must be a heap reference or 0 — `narrow_oop::probe`
/// asserts that in debug builds, and in release an unencodable value would be
/// truncated by `encode` into a pointer to somewhere else.
#[inline(always)]
unsafe fn store_ref(p: *mut u8, raw: u64) {
    cratonvm_types::narrow_oop::probe(raw);
    if cratonvm_types::narrow_oop::narrow_oops_enabled() {
        store_u32(p, cratonvm_types::narrow_oop::encode(raw));
    } else {
        store_u64(p, raw);
    }
}

/// Check the receiver against the site and return the field's address.
///
/// The `is_object_address` probe is what `class_id_of` does before reading a
/// header; it keeps a stale operand-stack reference to an uncommitted page
/// from faulting where the slow handler would have answered `class 0`. The
/// compact-flag comparison is what makes the two body layouts safe to mix in
/// one cache: a site filled against a legacy receiver refuses a compact one
/// and vice versa, even at the same class id.
#[inline(always)]
fn field_ptr_for(zgc: &ZgcRealHeap, ptr: u64, site: &FastFieldSite) -> Option<*mut u8> {
    if zgc.is_object_address(ptr as usize).is_none() {
        return None;
    }
    // SAFETY: `ptr` was just confirmed to be a registered object start on
    // this heap, so its first `HEADER_SIZE` bytes are a live `ObjectHeader`.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if header.class_id != site.receiver_class_id
        || header.num_slots() != site.num_slots
        || cratonvm_types::is_compact_object(header) != site.storage.is_some()
    {
        return None;
    }
    // SAFETY: for a compact receiver the offset came from the layout
    // registered for exactly this `(class_id, num_slots)`; for a legacy one it
    // is `field_index * SLOT_SIZE` with `field_index < num_slots` checked at
    // fill. Either way it lies inside the object body.
    Some(unsafe { (ptr as *mut u8).add(HEADER_SIZE + site.offset as usize) })
}

/// Quickened `getfield`. Replaces the receiver on top of the operand stack
/// with the field value and returns `true`, or leaves the stack untouched and
/// returns `false` so the caller runs `op_getfield`.
#[inline]
pub(super) fn getfield_fast(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    cp_index: u16,
) -> bool {
    let class_id = frame.class_id;
    getfield_fast_keyed(shared, zgc, sites, &mut frame.stack, class_id, cp_index, 0)
}

/// `getfield_fast` with the site key spelled out, for the invoke fast door's
/// trivial-getter shortcut: the site belongs to the getter's declaring class
/// and constant pool, while the operand stack is the caller's (the receiver
/// on top is the getter's `this`). `ret_opcode` is the getter's `xreturn`
/// opcode, checked against the field's type the way
/// `try_execute_cached_trivial_instance_getter` checks it against the
/// descriptor; `0` skips the check for a plain `getfield`.
#[inline]
pub(super) fn getfield_fast_keyed(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    stack: &mut ValueStack,
    class_id: ClassId,
    cp_index: u16,
    ret_opcode: u8,
) -> bool {
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        return false;
    }
    if stack.len() == 0 {
        return false;
    }
    let Some(ptr) = stack.peek_compact().as_object_ptr() else {
        return false;
    };
    let site = match sites.get(class_id, cp_index) {
        Some(s) => *s,
        None => {
            site_stats::bump(site_stats::FAST_GET_MISS);
            return false;
        }
    };
    if ret_opcode != 0 && !return_opcode_agrees(&site, ret_opcode) {
        return false;
    }
    let Some(fp) = field_ptr_for(zgc, ptr, &site) else {
        return false;
    };
    let Some(storage) = site.storage else {
        return getfield_legacy(shared, stack, fp, &site);
    };
    // SAFETY (every raw load below): `fp` addresses the field inside a live,
    // header-validated compact object; the width is the layout's own.
    let pushed = match storage {
        FieldStorageKind::Int => CompactValue::int(unsafe { load_u32(fp) } as i32),
        FieldStorageKind::Boolean => CompactValue::int((unsafe { load_u8(fp) } != 0) as i32),
        FieldStorageKind::Byte => CompactValue::int(unsafe { load_u8(fp) } as i8 as i32),
        FieldStorageKind::Char => CompactValue::int(unsafe { load_u16(fp) } as i32),
        FieldStorageKind::Short => CompactValue::int(unsafe { load_u16(fp) } as i16 as i32),
        FieldStorageKind::Float => CompactValue::float(f32::from_bits(unsafe { load_u32(fp) })),
        FieldStorageKind::Long => {
            let v = unsafe { load_u64(fp) } as i64;
            stack.pop_compact();
            stack.push_long_unchecked(v);
            site_stats::bump(site_stats::FAST_GET_HIT);
            return true;
        }
        FieldStorageKind::Double => {
            // SAFETY: as the group above — `fp` is this site's validated
            // field address, and `Double` is an 8-byte slot.
            let v = f64::from_bits(unsafe { load_u64(fp) });
            stack.pop_compact();
            stack.push_double_unchecked(v);
            site_stats::bump(site_stats::FAST_GET_HIT);
            return true;
        }
        FieldStorageKind::Reference => {
            // SAFETY: as the group above, and the arm is `Reference`, so the
            // slot is the reference slot `load_ref`'s width rule expects.
            let raw = unsafe { load_ref(fp) };
            if raw == 0 {
                CompactValue::null()
            } else {
                // A wrapper may sit in a reference slot; `get_field` unboxes
                // it and this arm does not.
                if cratonvm_gc::autobox::wrapper_exists() {
                    return false;
                }
                // SAFETY: a non-zero reference slot of a live compact object
                // holds an object address the collector maintains.
                let obj = shared
                    .mem
                    .heap
                    .load_and_forward(unsafe { ObjectRef::from_raw(raw as *mut u8) });
                match CompactValue::try_from_pointer(obj.as_ptr() as u64) {
                    Some(cv) => cv,
                    None => return false,
                }
            }
        }
    };
    stack.pop_compact();
    stack.push_compact(pushed);
    site_stats::bump(site_stats::FAST_GET_HIT);
    true
}

/// The legacy half of [`getfield_fast_keyed`]: one 16-byte tagged `Value`
/// cell, decoded and converted exactly as `op_getfield`'s tail does.
#[inline]
fn getfield_legacy(
    shared: &SharedVm,
    stack: &mut ValueStack,
    cell: *mut u8,
    site: &FastFieldSite,
) -> bool {
    // SAFETY: `cell` is a 16-byte field cell of a live legacy object.
    let Some(raw) = (unsafe { cratonvm_types::read_value_checked_atomic(cell as *const Value) })
    else {
        // A corrupt cell: leave it to the handler that owns the census.
        return false;
    };
    match site.desc_byte {
        b'J' => {
            let bits: i64 = match raw {
                Value::Long(x) => x,
                Value::Double(x) => x.to_bits() as i64,
                Value::Int(x) => x as i64,
                Value::Object(None) | Value::Uninitialized => 0,
                Value::Object(Some(r)) => r.as_ptr() as usize as i64,
                Value::Float(x) => x.to_bits() as i64,
                Value::ReturnAddress(pc) => pc as i64,
            };
            stack.pop_compact();
            stack.push_compact_long(CompactValue::long(bits));
        }
        b'D' => {
            let d: f64 = match raw {
                Value::Double(x) => x,
                Value::Long(x) => f64::from_bits(x as u64),
                Value::Int(x) => x as f64,
                Value::Object(None) | Value::Uninitialized => 0.0,
                Value::Object(Some(r)) => f64::from_bits(r.as_ptr() as usize as u64),
                Value::Float(x) => x as f64,
                Value::ReturnAddress(pc) => pc as f64,
            };
            stack.pop_compact();
            stack.push_compact_double(CompactValue::double_raw(d));
        }
        desc => {
            let mut value = raw;
            if site.is_reference {
                match value {
                    Value::Int(0) | Value::Long(0) => value = Value::Object(None),
                    _ => {}
                }
            } else {
                match value {
                    Value::Object(None) => value = Value::Int(0),
                    Value::Object(Some(r)) => {
                        value = Value::Int(r.as_ptr() as usize as u64 as i32);
                    }
                    _ => {}
                }
                value = super::narrow_int_to_field_type(value, desc);
            }
            if let Value::Object(Some(inner)) = value {
                value = Value::Object(Some(shared.mem.heap.load_and_forward(inner)));
            }
            stack.pop_compact();
            stack.push_unchecked(value);
        }
    }
    site_stats::bump(site_stats::FAST_GET_HIT);
    true
}

/// Whether a trivial getter's `xreturn` opcode agrees with the field's type,
/// the check `try_execute_cached_trivial_instance_getter` makes against the
/// descriptor before it answers a getter without a frame.
#[inline(always)]
fn return_opcode_agrees(site: &FastFieldSite, ret_opcode: u8) -> bool {
    matches!(
        (site.desc_byte, ret_opcode),
        (b'J', 0xad)
            | (b'F', 0xae)
            | (b'D', 0xaf)
            | (b'L' | b'[', 0xb0)
            | (b'Z' | b'B' | b'C' | b'S' | b'I', 0xac)
    )
}

/// Quickened `putfield`. Pops the value and the receiver and stores, or
/// leaves the stack untouched and returns `false` so the caller runs
/// `op_putfield`.
#[inline]
pub(super) fn putfield_fast(
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    cp_index: u16,
) -> bool {
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        return false;
    }
    if frame.stack.len() < 2 {
        return false;
    }
    let (val, kind) = frame.stack.peek_with_kind_unchecked();
    let Some(ptr) = frame.stack.peek_compact_at(1).as_object_ptr() else {
        return false;
    };
    let site = match sites.get(frame.class_id, cp_index) {
        Some(s) => *s,
        None => {
            site_stats::bump(site_stats::FAST_PUT_MISS);
            return false;
        }
    };
    let Some(fp) = field_ptr_for(zgc, ptr, &site) else {
        return false;
    };
    let Some(storage) = site.storage else {
        return putfield_legacy(zgc, &mut frame.stack, fp, ptr, &site, val, kind);
    };
    // SAFETY (every raw store below): see `field_ptr_for`.
    match storage {
        FieldStorageKind::Int => {
            let Some(v) = val.as_int() else {
                return false;
            };
            unsafe { store_u32(fp, v as u32) };
        }
        FieldStorageKind::Boolean => {
            let Some(v) = val.as_int() else {
                return false;
            };
            // SAFETY: `fp` came from `field_ptr_for`, and `Boolean` is the
            // layout's own 1-byte slot for this field.
            unsafe { store_u8(fp, (v & 1) as u8) };
        }
        FieldStorageKind::Byte => {
            let Some(v) = val.as_int() else {
                return false;
            };
            // SAFETY: as above; `Byte` is a 1-byte slot.
            unsafe { store_u8(fp, v as u8) };
        }
        FieldStorageKind::Char | FieldStorageKind::Short => {
            let Some(v) = val.as_int() else {
                return false;
            };
            // SAFETY: as above; both `Char` and `Short` are 2-byte slots.
            unsafe { store_u16(fp, v as u16) };
        }
        FieldStorageKind::Float => {
            let Some(f) = val.as_float() else {
                return false;
            };
            // SAFETY: as above; `Float` is a 4-byte slot, written as bits.
            unsafe { store_u32(fp, f.to_bits()) };
        }
        FieldStorageKind::Long => {
            if kind != ValueStack::KIND_MARK_LONG {
                return false;
            }
            // SAFETY: as above; `Long` is an 8-byte slot, and the stack kind
            // was just checked so `as_long_unchecked` is entitled.
            unsafe { store_u64(fp, val.as_long_unchecked() as u64) };
        }
        FieldStorageKind::Double => {
            if kind != ValueStack::KIND_MARK_DOUBLE {
                return false;
            }
            // SAFETY: as above; `Double` is an 8-byte slot, and the stack kind
            // was just checked.
            unsafe { store_u64(fp, val.raw_bits()) };
        }
        FieldStorageKind::Reference => {
            let raw = if val.is_null() {
                0
            } else if let Some(p) = val.as_object_ptr() {
                p
            } else {
                // `Int(0)` / smuggled-long coercions belong to the slow path.
                return false;
            };
            if zgc.mark_active() {
                // SAFETY: as above, on the `Reference` arm, so the slot is a
                // reference slot. The pre-barrier needs the OLD value before
                // the store overwrites it.
                let old = unsafe { load_ref(fp) };
                if old != 0 {
                    zgc.satb_pre_barrier(old as usize);
                }
            }
            // SAFETY: as above, on the `Reference` arm; `raw` is either 0 or
            // an object pointer taken from `as_object_ptr`, which is what
            // `store_ref`'s encode step requires.
            unsafe { store_ref(fp, raw) };
        }
    }
    zgc.note_ref_store(ptr as usize);
    frame.stack.pop_compact();
    frame.stack.pop_compact();
    site_stats::bump(site_stats::FAST_PUT_HIT);
    true
}

/// The legacy half of [`putfield_fast`]: decode the operand-stack slot the way
/// `op_putfield` does for this descriptor, then commit the whole 16-byte cell
/// with the same barrier order `ZgcRealHeap::set_field` uses.
#[inline]
#[allow(clippy::too_many_arguments)]
fn putfield_legacy(
    zgc: &ZgcRealHeap,
    stack: &mut ValueStack,
    cell: *mut u8,
    recv: u64,
    site: &FastFieldSite,
    val: CompactValue,
    kind: u8,
) -> bool {
    let value = match site.desc_byte {
        b'J' => {
            if kind != ValueStack::KIND_MARK_LONG {
                return false;
            }
            Value::Long(val.as_long_unchecked())
        }
        b'D' => {
            if kind != ValueStack::KIND_MARK_DOUBLE {
                return false;
            }
            Value::Double(f64::from_bits(val.raw_bits()))
        }
        b'L' | b'[' => {
            // `coerce_value_for_return_validated` is the identity on a slot
            // that is already a reference or null; every other shape (an
            // `Int(0)`, a smuggled long) keeps the slow path that owns it.
            if val.is_null() {
                Value::Object(None)
            } else if let Some(p) = val.as_object_ptr() {
                // SAFETY: an `Object`-tagged slot holds a heap address.
                Value::Object(Some(unsafe { ObjectRef::from_raw(p as *mut u8) }))
            } else {
                return false;
            }
        }
        desc => {
            let Some(v) = val.as_int() else {
                return false;
            };
            super::narrow_int_to_field_type(Value::Int(v), desc)
        }
    };
    // The two things `ZgcRealHeap::set_field` does around the raw store, in
    // its order: the SATB pre-barrier on the overwritten reference while
    // marking is active, then the write, then the card note.
    if zgc.mark_active() {
        // SAFETY: `cell` is a live 16-byte field cell.
        if let Some(Value::Object(Some(old))) =
            unsafe { cratonvm_types::read_value_checked_atomic(cell as *const Value) }
        {
            zgc.satb_pre_barrier(old.as_ptr() as usize);
        }
    }
    // SAFETY: `cell` is a live 16-byte field cell; `write_value_atomic` is the
    // padding-free, marker-safe store `set_field` uses for this layout.
    unsafe { cratonvm_types::write_value_atomic(cell as *mut Value, value) };
    zgc.note_ref_store(recv as usize);
    stack.pop_compact();
    stack.pop_compact();
    site_stats::bump(site_stats::FAST_PUT_HIT);
    true
}

/// Record the site the slow handler just resolved, so the next access with a
/// receiver of the same shape takes the fast arm. Called by `op_getfield` and
/// `op_putfield` after the access succeeded.
pub(super) fn fill_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    current_class_id: ClassId,
    cp_index: u16,
    obj: ObjectRef,
    field: &ResolvedField,
    epochs_at_entry: (u64, u64),
    is_put: bool,
) {
    if field.is_static || field.is_volatile {
        return;
    }
    if crate::runtime::env_cache::no_field_fast_path() {
        return;
    }
    if !matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Zgc(_)) {
        return;
    }
    if shared
        .mem
        .heap
        .is_object_address(obj.as_ptr() as usize)
        .is_none()
    {
        return;
    }
    // SAFETY: `obj` is a registered object start (checked above).
    let header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
    let (offset, storage) =
        match cratonvm_types::compact_object_field_storage(header, field.field_index) {
            Some((offset, storage)) => {
                if FieldStorageKind::from_descriptor_byte(field.desc_byte) != Some(storage) {
                    site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
                    report_unusable_once(header, field, "descriptor / storage disagree");
                    return;
                }
                (offset, Some(storage))
            }
            None => {
                if cratonvm_types::is_compact_object(header) {
                    // A compact object whose `(class_id, field_count)` has no
                    // registered layout: `get_field` itself refuses this one.
                    site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
                    report_unusable_once(header, field, "compact receiver, no layout");
                    return;
                }
                if field.field_index >= header.num_slots() as usize {
                    site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
                    report_unusable_once(header, field, "field index beyond the legacy body");
                    return;
                }
                (field.field_index * SLOT_SIZE, None)
            }
        };
    let (Ok(offset), Ok(field_index)) = (u32::try_from(offset), u32::try_from(field.field_index))
    else {
        site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
        report_unusable_once(header, field, "offset or index out of u32 range");
        return;
    };
    thread.fast_field_sites.put(
        current_class_id,
        cp_index,
        epochs_at_entry,
        FastFieldSite {
            receiver_class_id: header.class_id,
            num_slots: header.num_slots(),
            offset,
            storage,
            field_index,
            desc_byte: field.desc_byte,
            is_reference: field.is_reference,
        },
    );
    site_stats::bump(if is_put {
        site_stats::FAST_PUT_FILL
    } else {
        site_stats::FAST_GET_FILL
    });
}

/// Under `CRATONVM_DBG_FIELD_SITE=1`, say once per reason why a site could not
/// be quickened — the engagement census counts `unusable` and this names it.
#[cold]
#[inline(never)]
fn report_unusable_once(header: &ObjectHeader, field: &ResolvedField, why: &'static str) {
    static REPORTED: AtomicU32 = AtomicU32::new(0);
    if !site_stats::on() || REPORTED.fetch_add(1, Ordering::Relaxed) >= 8 {
        return;
    }
    eprintln!(
        "[fast-field] unusable: {why}: receiver class_id={} num_slots={} gc_flags={:#x} compact={} \
         field_index={} desc_byte={:?} is_ref={} volatile={}",
        header.class_id.as_u32(),
        header.num_slots(),
        header.gc_flags(),
        cratonvm_types::is_compact_object(header),
        field.field_index,
        field.desc_byte as char,
        field.is_reference,
        field.is_volatile,
    );
}

/// Snapshot the epochs a fill must be validated against; taken at slow
/// handler entry, before any resolution.
#[inline]
pub(super) fn fill_epochs() -> (u64, u64) {
    FieldSiteCache::epochs_now()
}

// ── Primitive array element access ───────────────────────────────────────

/// Element type the `*aload` / `*astore` opcode expects, with the byte width
/// the header must agree on. `None` for the reference opcodes, which keep the
/// barrier-aware `get_array_element` / `set_array_element` path.
#[inline(always)]
fn prim_elem_for_opcode(opcode: u8) -> Option<(ArrayElementType, usize)> {
    Some(match opcode {
        0x2e | 0x4f => (ArrayElementType::Int, 4),
        0x2f | 0x50 => (ArrayElementType::Long, 8),
        0x30 | 0x51 => (ArrayElementType::Float, 4),
        0x31 | 0x52 => (ArrayElementType::Double, 8),
        0x33 | 0x54 => (ArrayElementType::Byte, 1),
        0x34 | 0x55 => (ArrayElementType::Char, 2),
        0x35 | 0x56 => (ArrayElementType::Short, 2),
        _ => return None,
    })
}

/// Validate `arr` as a live primitive array of the opcode's element type with
/// `index` in range, and return the element address.
///
/// `baload` / `bastore` serve both `byte[]` and `boolean[]`; the header says
/// which, and both are one byte wide, so either is accepted for them.
#[inline(always)]
fn prim_elem_ptr(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, opcode: u8) -> Option<*mut u8> {
    let (et, width) = prim_elem_for_opcode(opcode)?;
    if index < 0 {
        return None;
    }
    let base = arr.as_ptr() as usize;
    if zgc.is_object_address(base).is_none() {
        return None;
    }
    // SAFETY: registered object start, live header.
    let header = unsafe { &*(base as *const ObjectHeader) };
    if header.kind() != ObjectKind::Array {
        return None;
    }
    let actual = header.element_type();
    let type_ok =
        actual == et || (et == ArrayElementType::Byte && actual == ArrayElementType::Boolean);
    if !type_ok {
        return None;
    }
    let index = index as usize;
    if index >= header.array_length() as usize {
        return None;
    }
    // SAFETY: `index < length` and the width matches the element type, so the
    // element lies inside the array body.
    Some(unsafe { (base as *mut u8).add(HEADER_SIZE + index * width) })
}

/// Quickened primitive `*aload`: pushes the element and returns `true`, or
/// returns `false` with the stack untouched so the caller keeps the general
/// path (which also owns every exception).
#[inline]
pub(super) fn array_load_prim(
    zgc: &ZgcRealHeap,
    stack: &mut ValueStack,
    arr: ObjectRef,
    index: i32,
    opcode: u8,
) -> bool {
    let Some(p) = prim_elem_ptr(zgc, arr, index, opcode) else {
        return false;
    };
    // SAFETY (every read): `p` is an in-bounds element of a live array.
    match opcode {
        0x2e => stack.push_int_unchecked(unsafe { load_u32(p) } as i32),
        0x2f => stack.push_long_unchecked(unsafe { load_u64(p) } as i64),
        0x30 => stack.push_float_unchecked(f32::from_bits(unsafe { load_u32(p) })),
        0x31 => stack.push_double_unchecked(f64::from_bits(unsafe { load_u64(p) })),
        0x33 => stack.push_int_unchecked(unsafe { load_u8(p) } as i8 as i32),
        0x34 => stack.push_int_unchecked(unsafe { load_u16(p) } as i32),
        0x35 => stack.push_int_unchecked(unsafe { load_u16(p) } as i16 as i32),
        _ => return false,
    }
    true
}

/// Quickened primitive `*astore`: stores `val` (already peeked with its kind
/// mark) and returns `true`, or returns `false` so the caller keeps the
/// general path.
#[inline]
pub(super) fn array_store_prim(
    zgc: &ZgcRealHeap,
    arr: ObjectRef,
    index: i32,
    opcode: u8,
    val: CompactValue,
    kind: u8,
) -> bool {
    let Some(p) = prim_elem_ptr(zgc, arr, index, opcode) else {
        return false;
    };
    // SAFETY (every write): `p` is an in-bounds element of a live array.
    match opcode {
        0x4f => {
            let Some(v) = val.as_int() else {
                return false;
            };
            unsafe { store_u32(p, v as u32) };
        }
        0x50 => {
            if kind != ValueStack::KIND_MARK_LONG {
                return false;
            }
            unsafe { store_u64(p, val.as_long_unchecked() as u64) };
        }
        0x51 => {
            let Some(f) = val.as_float() else {
                return false;
            };
            // SAFETY: `p` is this static's slot address, resolved above, and
            // `Float` is its 4-byte slot.
            unsafe { store_u32(p, f.to_bits()) };
        }
        0x52 => {
            if kind != ValueStack::KIND_MARK_DOUBLE {
                return false;
            }
            // SAFETY: as above; `Double` is an 8-byte slot.
            unsafe { store_u64(p, val.raw_bits()) };
        }
        0x54 => {
            let Some(v) = val.as_int() else {
                return false;
            };
            // `bastore` into a `boolean[]` stores only bit 0 (JVMS §6.5).
            // SAFETY: `prim_elem_ptr` validated `arr` as a live array.
            let header = unsafe { &*(arr.as_ptr() as *const ObjectHeader) };
            let raw = if header.element_type() == ArrayElementType::Boolean {
                (v & 1) as u8
            } else {
                v as u8
            };
            unsafe { store_u8(p, raw) };
        }
        0x55 | 0x56 => {
            let Some(v) = val.as_int() else {
                return false;
            };
            // SAFETY: as above; a 2-byte slot.
            unsafe { store_u16(p, v as u16) };
        }
        _ => return false,
    }
    // The host just wrote this array, so any device buffer mirroring
    // it in the GPU input-residency cache is stale and must go.
    //
    // AUDIT 2026-09-02: this is the THIRD `*astore` implementation in
    // the VM. `interpreter/opcodes.rs`'s `Instruction::Iastore` arm,
    // `interpreter.rs`'s fast dispatch loop and
    // `jit::helpers::jit_iastore` all invalidate -- but this quickened
    // arm runs BEFORE every one of them and writes the element through
    // a raw pointer, so without this the next submit computes from a
    // stale device copy. This is the same hole
    // `bench-gpu/runtime-stress.sh` was written for after the defect in
    // the fast dispatch loop; four of its seven scenarios fail without
    // this line.
    #[cfg(feature = "gpu-offload")]
    crate::runtime::offload::input_cache::invalidate(arr);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `*aload` / `*astore` table is hand-written from JVMS §6.5, and a
    /// transcription error in it would read or write an element at the wrong
    /// width — silently, on a live array. Pin every entry against the opcode
    /// numbers and the `ArrayElementType` widths the header carries.
    #[test]
    fn the_primitive_array_opcode_table_matches_the_jvms_element_types() {
        use ArrayElementType::*;
        // (load opcode, store opcode, element type, byte width)
        let expected: &[(u8, u8, ArrayElementType, usize)] = &[
            (0x2e, 0x4f, Int, 4),      // iaload  / iastore
            (0x2f, 0x50, Long, 8),     // laload  / lastore
            (0x30, 0x51, Float, 4),    // faload  / fastore
            (0x31, 0x52, Double, 8),   // daload  / dastore
            (0x33, 0x54, Byte, 1),     // baload  / bastore
            (0x34, 0x55, Char, 2),     // caload  / castore
            (0x35, 0x56, Short, 2),    // saload  / sastore
        ];
        for &(load, store, et, width) in expected {
            assert_eq!(
                prim_elem_for_opcode(load),
                Some((et, width)),
                "load opcode {load:#x}"
            );
            assert_eq!(
                prim_elem_for_opcode(store),
                Some((et, width)),
                "store opcode {store:#x}"
            );
        }
    }

    /// `aaload` (0x32) and `aastore` (0x53) must NOT be in the table: a
    /// reference element needs the load barrier and the array-store check,
    /// which only the general path performs.
    #[test]
    fn the_reference_array_opcodes_are_not_quickened() {
        assert_eq!(prim_elem_for_opcode(0x32), None, "aaload");
        assert_eq!(prim_elem_for_opcode(0x53), None, "aastore");
        // And nothing outside the two ranges answers either.
        for op in [0x00u8, 0x2d, 0x36, 0x4e, 0x57, 0xb4, 0xff] {
            assert_eq!(prim_elem_for_opcode(op), None, "opcode {op:#x}");
        }
    }

    fn site_with(desc_byte: u8) -> FastFieldSite {
        FastFieldSite {
            receiver_class_id: ClassId::new(1),
            num_slots: 1,
            offset: 0,
            storage: None,
            field_index: 0,
            desc_byte,
            is_reference: matches!(desc_byte, b'L' | b'['),
        }
    }

    /// The trivial-getter shortcut answers a getter without building its
    /// frame, so its return opcode must agree with the field's type exactly as
    /// `try_execute_cached_trivial_instance_getter` requires — otherwise a
    /// `long` field could be returned through `ireturn`.
    #[test]
    fn the_trivial_getter_return_opcode_agrees_only_with_its_own_field_type() {
        const IRETURN: u8 = 0xac;
        const LRETURN: u8 = 0xad;
        const FRETURN: u8 = 0xae;
        const DRETURN: u8 = 0xaf;
        const ARETURN: u8 = 0xb0;
        let accepted: &[(u8, u8)] = &[
            (b'J', LRETURN),
            (b'F', FRETURN),
            (b'D', DRETURN),
            (b'L', ARETURN),
            (b'[', ARETURN),
            (b'Z', IRETURN),
            (b'B', IRETURN),
            (b'C', IRETURN),
            (b'S', IRETURN),
            (b'I', IRETURN),
        ];
        for &(desc, ret) in accepted {
            assert!(
                return_opcode_agrees(&site_with(desc), ret),
                "{} should be returnable by {ret:#x}",
                desc as char
            );
        }
        // Every other pairing is refused.
        let all_returns = [IRETURN, LRETURN, FRETURN, DRETURN, ARETURN];
        for &(desc, ret) in accepted {
            for other in all_returns {
                if other == ret {
                    continue;
                }
                assert!(
                    !return_opcode_agrees(&site_with(desc), other),
                    "{} must NOT be returnable by {other:#x}",
                    desc as char
                );
            }
        }
    }
}
