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
//! for a receiver, it records `(receiver class id, num_slots, byte offset,
//! storage kind)` in `JvmThread::fast_field_sites`, and the next access whose
//! receiver header matches loads or stores at that offset directly.
//!
//! # Correctness contract
//!
//! * A site is only filled for a **non-static, non-volatile** field of a
//!   **compact-layout** receiver, on a **ZGC** heap, when the storage kind the
//!   layout assigned agrees with the field descriptor. Anything else keeps the
//!   full handler.
//! * The fast arm re-checks the receiver's `class_id` and `num_slots` against
//!   the site (the compact layout registry is keyed by exactly that pair) and
//!   that the object carries the compact flag. A mismatch falls back to the
//!   full handler, which refills the site for the new receiver. Polymorphic
//!   sites therefore thrash the memo and run mostly slow, which is the same
//!   shape as the monomorphic invoke cache.
//! * Sites are epoch-validated by `SiteCache` (class definition epoch and
//!   resolution epoch) and dropped wholesale on class redefinition, exactly
//!   like `field_sites`.
//! * Every diagnostic the slow handlers honour per access
//!   (`CRATONVM_DBG_FIELD*`, the corrupt-cell watch, the punned-store watch,
//!   the vacated-frames ledger, the remap trace, the stray-stack probe) and
//!   every JVMTI field watchpoint turns the fast arms off, so an armed run
//!   sees exactly what it saw before.
//! * A `null` or non-object receiver falls back so the helpful-NPE message is
//!   built by the code that owns it.
//! * Reference loads keep the receiver-side `load_and_forward` on the loaded
//!   value and mint the operand-stack slot through
//!   `CompactValue::try_from_pointer`, i.e. the same provenance record the
//!   `Value::Object` push made. A heap that has ever minted an autobox wrapper
//!   (`cratonvm_gc::autobox::wrapper_exists`) keeps reference loads slow, so
//!   the unboxing `get_field` performs is never skipped.
//! * Reference stores run the SATB pre-barrier while marking is active and
//!   the generational card note afterwards, the two things `ZgcRealHeap::
//!   set_field` does around the raw write. Primitive stores never carded an
//!   old→young edge, so the note is skipped for them.
//! * Category-2 stores only take the fast path when the operand-stack kind
//!   mark says the slot was pushed by a genuine `long` / `double` producer;
//!   the slow handler owns every decode heuristic for the unmarked shapes.
//!
//! Kill switch: `CRATONVM_JIT_NO_FIELD_FAST_PATH=1` (`CRATONVM_JIT=
//! -field-fast-path`). Engagement: `CRATONVM_DBG_FIELD_SITE=1` prints
//! `fast-field: get hit/miss/fill put hit/miss/fill unusable`.

use super::site_cache::{site_stats, FastFieldSite, FastFieldSiteCache, FieldSiteCache};
use crate::runtime::frame::Frame;
use crate::runtime::value_stack::ValueStack;
use crate::threading::JvmThread;
use crate::vm::SharedVm;
use cratonvm_classloading::resolution::ResolvedField;
use cratonvm_gc::zgc::ZgcRealHeap;
use cratonvm_gc::{ArrayElementType, ObjectHeader, ObjectKind};
use cratonvm_types::{ClassId, CompactValue, FieldStorageKind, ObjectRef, HEADER_SIZE};
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

#[inline(always)]
unsafe fn load_u8(p: *mut u8) -> u8 {
    (*(p as *const AtomicU8)).load(Ordering::Relaxed)
}
#[inline(always)]
unsafe fn load_u16(p: *mut u8) -> u16 {
    (*(p as *const AtomicU16)).load(Ordering::Relaxed)
}
#[inline(always)]
unsafe fn load_u32(p: *mut u8) -> u32 {
    (*(p as *const AtomicU32)).load(Ordering::Relaxed)
}
#[inline(always)]
unsafe fn load_u64(p: *mut u8) -> u64 {
    (*(p as *const AtomicU64)).load(Ordering::Relaxed)
}
#[inline(always)]
unsafe fn store_u8(p: *mut u8, v: u8) {
    (*(p as *const AtomicU8)).store(v, Ordering::Relaxed)
}
#[inline(always)]
unsafe fn store_u16(p: *mut u8, v: u16) {
    (*(p as *const AtomicU16)).store(v, Ordering::Relaxed)
}
#[inline(always)]
unsafe fn store_u32(p: *mut u8, v: u32) {
    (*(p as *const AtomicU32)).store(v, Ordering::Relaxed)
}
#[inline(always)]
unsafe fn store_u64(p: *mut u8, v: u64) {
    (*(p as *const AtomicU64)).store(v, Ordering::Relaxed)
}

/// Raw reference slot read, honouring compressed oops the way
/// `read_compact_field` does.
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
/// from faulting where the slow handler would have answered `class 0`.
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
        || !cratonvm_types::is_compact_object(header)
    {
        return None;
    }
    // SAFETY: the site's offset was produced by the compact layout registered
    // for exactly this `(class_id, num_slots)`, so it lies inside the object.
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
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        return false;
    }
    let Some(ptr) = frame.stack.peek_compact().as_object_ptr() else {
        return false;
    };
    let site = match sites.get(frame.class_id, cp_index) {
        Some(s) => *s,
        None => {
            site_stats::bump(site_stats::FAST_GET_MISS);
            return false;
        }
    };
    let Some(fp) = field_ptr_for(zgc, ptr, &site) else {
        return false;
    };
    // SAFETY (every raw load below): `fp` addresses the field inside a live,
    // header-validated compact object; the width is the layout's own.
    let pushed = match site.storage {
        FieldStorageKind::Int => CompactValue::int(unsafe { load_u32(fp) } as i32),
        FieldStorageKind::Boolean => CompactValue::int((unsafe { load_u8(fp) } != 0) as i32),
        FieldStorageKind::Byte => CompactValue::int(unsafe { load_u8(fp) } as i8 as i32),
        FieldStorageKind::Char => CompactValue::int(unsafe { load_u16(fp) } as i32),
        FieldStorageKind::Short => CompactValue::int(unsafe { load_u16(fp) } as i16 as i32),
        FieldStorageKind::Float => CompactValue::float(f32::from_bits(unsafe { load_u32(fp) })),
        FieldStorageKind::Long => {
            let v = unsafe { load_u64(fp) } as i64;
            frame.stack.pop_compact();
            frame.stack.push_long_unchecked(v);
            site_stats::bump(site_stats::FAST_GET_HIT);
            return true;
        }
        FieldStorageKind::Double => {
            let v = f64::from_bits(unsafe { load_u64(fp) });
            frame.stack.pop_compact();
            frame.stack.push_double_unchecked(v);
            site_stats::bump(site_stats::FAST_GET_HIT);
            return true;
        }
        FieldStorageKind::Reference => {
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
    frame.stack.pop_compact();
    frame.stack.push_compact(pushed);
    site_stats::bump(site_stats::FAST_GET_HIT);
    true
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
    // SAFETY (every raw store below): see `field_ptr_for`.
    match site.storage {
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
            unsafe { store_u8(fp, (v & 1) as u8) };
        }
        FieldStorageKind::Byte => {
            let Some(v) = val.as_int() else {
                return false;
            };
            unsafe { store_u8(fp, v as u8) };
        }
        FieldStorageKind::Char | FieldStorageKind::Short => {
            let Some(v) = val.as_int() else {
                return false;
            };
            unsafe { store_u16(fp, v as u16) };
        }
        FieldStorageKind::Float => {
            let Some(f) = val.as_float() else {
                return false;
            };
            unsafe { store_u32(fp, f.to_bits()) };
        }
        FieldStorageKind::Long => {
            if kind != ValueStack::KIND_MARK_LONG {
                return false;
            }
            unsafe { store_u64(fp, val.as_long_unchecked() as u64) };
        }
        FieldStorageKind::Double => {
            if kind != ValueStack::KIND_MARK_DOUBLE {
                return false;
            }
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
                let old = unsafe { load_ref(fp) };
                if old != 0 {
                    zgc.satb_pre_barrier(old as usize);
                }
            }
            unsafe { store_ref(fp, raw) };
            zgc.note_ref_store(ptr as usize);
        }
    }
    frame.stack.pop_compact();
    frame.stack.pop_compact();
    site_stats::bump(site_stats::FAST_PUT_HIT);
    true
}

/// Record the site the slow handler just resolved, so the next access with a
/// receiver of the same class takes the fast arm. Called by `op_getfield` and
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
    let Some((offset, storage)) =
        cratonvm_types::compact_object_field_storage(header, field.field_index)
    else {
        site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
        return;
    };
    if FieldStorageKind::from_descriptor_byte(field.desc_byte) != Some(storage) {
        site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
        return;
    }
    let (Ok(offset), Ok(field_index)) = (u32::try_from(offset), u32::try_from(field.field_index))
    else {
        site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
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
        },
    );
    site_stats::bump(if is_put {
        site_stats::FAST_PUT_FILL
    } else {
        site_stats::FAST_GET_FILL
    });
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
    let type_ok = actual == et
        || (et == ArrayElementType::Byte && actual == ArrayElementType::Boolean);
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

/// Quickened primitive `*astore`: stores `val` (already popped with its kind
/// mark) and returns `true`, or returns `false` so the caller keeps the general
/// path. The caller must not have popped the index and array yet unless it
/// re-pushes them on `false`.
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
            unsafe { store_u32(p, f.to_bits()) };
        }
        0x52 => {
            if kind != ValueStack::KIND_MARK_DOUBLE {
                return false;
            }
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
            unsafe { store_u16(p, v as u16) };
        }
        _ => return false,
    }
    true
}
