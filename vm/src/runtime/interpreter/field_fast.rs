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
//!
//! **Which one predominates changed on 2026-09-03, and the reasoning printed
//! here did not.** The paragraph this replaces said that
//! `ZgcRealHeap::try_alloc_object` — the TLAB path
//! `interpreter::alloc_object_shared` takes — "builds *only*" legacy bodies,
//! so "essentially every object the interpreter allocates is legacy", and
//! cited `fast-field: get hit=0 miss=1801267` on `probes/FieldShape.java` as
//! the measurement. That was true when it was written and is false now:
//! `gc_and_alloc::compact_tlab_alloc_enabled` has been **default-ON since
//! 2026-09-03**, so the TLAB path plans a compact body for every class that
//! has a registered layout, and it is the COMPACT arm that serves the common
//! case.
//!
//! Both arms are still needed — a class with no registered layout, and every
//! object allocated before its layout was registered, is still legacy — so
//! this is a correction to the *reasoning*, not to the code. It matters
//! because that reasoning is what a reader uses to decide which arm to touch:
//! optimising the legacy arm on the strength of "essentially every object" is
//! now optimising the cold one.
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
//! Kill switches: `CRATONVM_JIT_NO_FIELD_FAST_PATH=1` (`CRATONVM_JIT=
//! -field-fast-path`) turns the arms off entirely;
//! `CRATONVM_JIT_NO_FIELD_ADDR_ELIDE=1` (`CRATONVM_JIT=-field-addr-elide`)
//! keeps them but restores the per-receiver object-start registry probe they
//! used to open with — see [`registry_probe_restored`] for why that probe is
//! not what makes the header read sound.
//! Engagement: `CRATONVM_DBG_FIELD_SITE=1` prints
//! `fast-field: get hit/miss/fill put hit/miss/fill unusable`, and names the
//! first few reasons a site could not be quickened.

use super::site_cache::{
    site_stats, FastFieldSite, FastFieldSiteCache, FieldSiteCache, InterpSiteTables,
    PrimitiveConstantSiteCache,
};
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

/// Is the object-start registry probe restored on the quickened **array**
/// arms? `CRATONVM_JIT_NO_FIELD_ADDR_ELIDE=1`
/// (`CRATONVM_JIT=-field-addr-elide`); default `false`, i.e. elided.
///
/// # The field half of this was REVERTED, and the reason is the interesting part
///
/// This gate originally covered [`field_ptr_for`] too, on the argument that
/// the handler those arms replace does not make the test either:
/// `ZgcRealHeap::get_field` is `self.header(obj)`, a bare pointer cast, with no
/// membership check. That argument was **wrong, and it was wrong by looking one
/// level too low.**
///
/// The handler a quickened `getfield` actually stands in for is `op_getfield`,
/// not `get_field`. Its second act, right after popping the receiver, is
/// `shared.mem.heap.load_and_forward(obj_ref)` — and `load_and_forward_inner`
/// opens with `if !pre_validated && self.is_object_address(..).is_none()`.
/// `op_putfield` does the same. So the slow path **does** probe the receiver;
/// it just does it a level up from where the comparison was made. Eliding it on
/// the field arms was a robustness regression, not a parity restoration, and
/// the measured benefit was weak (1-4 ns, 8/10 pairwise). It is reverted.
///
/// Two comments on this file said so at the time and were overruled by that
/// bad argument — `field_ptr_for`'s own doc ("it keeps a stale operand-stack
/// reference to an uncommitted page from faulting where the slow handler would
/// have answered `class 0`") and the `# Safety` contract on the raw accessors,
/// which names the probe as its first discharging leg. **Both were right.**
///
/// # Why the ARRAY arms keep the elide
///
/// The array slow paths genuinely do not validate. The `0x2e..=0x35` arm reaches
/// `VmHeap::get_array_element` and the `0xbe` arm reaches `VmHeap::array_length`
/// with **no** `load_and_forward` and no probe anywhere between the pop and the
/// header read — grep says zero call sites in either. So for
/// [`prim_elem_ptr`] and [`array_load_ref`] the parity argument holds as
/// originally stated, and the probe stays elided there.
///
/// That asymmetry is the finding: "does the slow path check this?" has a
/// different answer for fields than for arrays, and one gate covered both.
#[inline]
fn registry_probe_restored() -> bool {
    crate::runtime::env_cache::no_field_addr_elide()
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
    // SAFETY: `ptr` was just confirmed to be a registered object start on this
    // heap, so its first `HEADER_SIZE` bytes are a live `ObjectHeader`.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    let compact = cratonvm_types::is_compact_object(header);
    // A compact instance's layout is its class's (its 8-byte header has no
    // count word), so class id plus the compact bit settle it; only a legacy
    // receiver also needs its self-described field count to match.
    if header.class_id != site.receiver_class_id
        || compact != site.storage.is_some()
        || (!compact && header.num_slots() != site.num_slots)
    {
        return None;
    }
    // SAFETY: `site.offset` is an ABSOLUTE displacement: for a compact
    // receiver its class layout's, for a legacy one `HEADER_SIZE + field_index
    // * SLOT_SIZE` with `field_index < num_slots` checked at fill. Either way
    // it lies inside the object.
    Some(unsafe { (ptr as *mut u8).add(site.offset as usize) })
}

/// Quickened `arraylength`.
///
/// Replaces the array reference on top of the operand stack with its length
/// and returns `true`, or leaves the stack untouched and returns `false` so the
/// caller keeps the general path (which owns the null-receiver NPE).
///
/// # Why this arm exists
///
/// `arraylength` has no resolution, no site cache, no barrier and no
/// allocation: it reads one word out of a header and pushes an int. Measured
/// 2026-09-05 it cost **24.4 ns against HotSpot's 0.39** — 62x, the worst
/// ratio of any bytecode in the interpreter's operation table, and about three
/// and a half straight-line bytecodes' worth of time to perform one load.
///
/// What it was spending it on, all of it avoidable:
///
/// * `frame.stack.pop_unchecked()` decodes the 8-byte `CompactValue` into the
///   16-byte `Value` enum, and the arm's own fall-through pushes it back — so
///   the wide value stays live across the arm and needs a stack slot.
/// * `shared.mem.heap.array_length(arr)` goes through the `VmHeap` enum
///   `dispatch!` to reach a collector method whose entire body is
///   `self.header(obj).array_length()`.
///
/// This reads the header at the address already in the slot and pushes the
/// length. `ObjectHeader::array_length` is a pure header read on every
/// collector (`shape`, guarded by `kind()`), but this arm is admitted only
/// under the same `fast_field_zgc` gate as the field and array-element arms so
/// it inherits their contract: any per-access diagnostic that turns those off
/// turns this off too, and only ZGC is served.
///
/// A non-array receiver declines rather than pushing `0`: `array_length()`
/// answers `0` for a non-array, and the general path's own
/// `Value::Object(Some(_))` arm would have asked the collector, whose
/// `debug_assert` documents that a non-array here is a bug. Declining keeps
/// that question with the code that owns it.
///
/// The caller owns the admission: `fast_field_zgc` and the
/// `CRATONVM_JIT_NO_ARRAYLENGTH_FAST` kill switch, both hoisted in the dispatch
/// loop (`arraylength_fusion_on`; the switch was re-read here on every call
/// until interpreter round i2 wave 60, lane L7). `census` is
/// `VmConfig::site_census`, hoisted there too.
#[inline]
pub(super) fn arraylength_fast(stack: &mut ValueStack, census: bool) -> bool {
    if stack.len() == 0 {
        site_stats::bump(site_stats::ARRLEN_MISS);
        return false;
    }
    // `None` for a null receiver (`SUB_NULL` is not `SUB_OBJECT`), which is
    // what routes the NPE to the general path.
    let Some(ptr) = stack.peek_compact().as_object_ptr() else {
        site_stats::bump(site_stats::ARRLEN_MISS);
        return false;
    };
    if ptr == 0 {
        site_stats::bump(site_stats::ARRLEN_MISS);
        return false;
    }
    // SAFETY: `ptr` is the receiver of a verified `arraylength`, so it is a
    // live object reference and its first `HEADER_SIZE` bytes are an
    // `ObjectHeader` — the same premise `ZgcRealHeap::array_length` reads on,
    // from the same operand-stack slot, in the same safepoint-free window.
    // See `registry_probe_restored` for the full argument.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if header.kind() != ObjectKind::Array {
        site_stats::bump(site_stats::ARRLEN_MISS);
        return false;
    }
    // JVMS: `arraylength` pushes an int, and an array length is bounded by
    // `Integer.MAX_VALUE`. A header claiming more is corrupt; decline to the
    // path that owns that diagnosis rather than wrapping it negative.
    let Ok(len) = i32::try_from(header.array_length()) else {
        site_stats::bump(site_stats::ARRLEN_MISS);
        return false;
    };
    stack.pop_compact();
    stack.push_int_unchecked(len);
    site_stats::bump_if(census, site_stats::ARRLEN_HIT);
    true
}

/// [`arraylength_fast`] for a reference still in a LOCAL, for the dispatch
/// loop's fused `aload{_N, n}; arraylength` shapes (interpreter round i1 wave
/// 5, lane L1): the length of the array a plain `Object`-tagged local slot
/// addresses, with nothing pushed or popped. `None` for every shape
/// `arraylength_fast` would decline once `aload` had pushed the slot verbatim
/// (null, a non-array, a corrupt header) and for every non-`Object` slot,
/// which `aload` decodes through the validated coerce this does not
/// duplicate. On `None` the caller runs the unfused `aload`, so the
/// `arraylength` raises its NPE at its own pc, and a miss is counted there
/// rather than here. The caller also owns the admission `arraylength_fast`
/// reads per call (`fast_field_zgc` and `CRATONVM_JIT_NO_ARRAYLENGTH_FAST`,
/// hoisted in the dispatch loop), and `census` (`VmConfig::site_census`).
#[inline(always)]
pub(super) fn arraylength_of_local(cv: CompactValue, census: bool) -> Option<i32> {
    let ptr = cv.as_object_ptr().filter(|&p| p != 0)?;
    // SAFETY: as in `arraylength_fast` — `ptr` is the non-null `Object`
    // payload of the reference local a verified `aload; arraylength` reads,
    // the very bits `aload` would push, in the same safepoint-free window.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if header.kind() != ObjectKind::Array {
        return None;
    }
    let len = i32::try_from(header.array_length()).ok()?;
    site_stats::bump_if(census, site_stats::ARRLEN_HIT);
    Some(len)
}

/// Quickened `getfield`. Replaces the receiver on top of the operand stack
/// with the field value and returns `true`, or leaves the stack untouched and
/// returns `false` so the caller runs `op_getfield`. `census` is
/// `VmConfig::site_census`, hoisted by the dispatch loop; `phases_on` is
/// `field_phases::on()` (`CRATONVM_DBG_FIELD_PHASES`), hoisted by the loop
/// with its other per-entry gates (interpreter round i2 wave 61, lane L7: a
/// static byte load per quickened `getfield` before). `watch` is the loop's
/// hoisted field-watch gate ([`field_watch_gate`]).
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn getfield_fast(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    cp_index: u16,
    census: bool,
    phases_on: bool,
    watch: Option<bool>,
) -> bool {
    let class_id = frame.class_id;
    getfield_fast_keyed_gated(
        shared,
        zgc,
        sites,
        &mut frame.stack,
        class_id,
        cp_index,
        0,
        census,
        phases_on,
        watch,
    )
}

/// The dispatch loop's `aload n; getfield` fusion
/// (`CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD`,
/// `VmConfig::interp_fused_wide_local_getfield`; interpreter round i3 wave
/// 83, lane L7; `i72-L7-proposal-wide-local-forms-of-the-fused-groups`): the
/// `aload_N; getfield` pair's quickened read for a receiver past local 3,
/// which the `aload n` arm (at `pc`) has just pushed. The `getfield` is at
/// `pc + 2`, its pool index at `pc + 3..=pc + 4`, all below `code_len` (the
/// unpadded length) or `None`. Exactly [`getfield_fast`]: the same site,
/// admission, push, and a miss that leaves the receiver on the stack for the
/// unfused `getfield`. Returns the pc past the `getfield` on a hit.
///
/// Everything the read needs beyond the arm's own values is derived here,
/// not passed (host rework of wave 83: the first, seven-argument version
/// grew the loop frame by 32 bytes): the ZGC admission
/// ([`fast_field_zgc`], which the loop folded into the arm's gate bit),
/// the site census (`VmConfig::site_census`), the field-phase instrument
/// (`field_phases::on()`, the loop's `DG_FIELD_PHASES`), and the field-watch
/// union, loaded here (`watch = None`), as every caller outside the loop's
/// hoisted gate does.
#[inline(never)]
pub(super) fn wide_local_getfield(
    shared: &SharedVm,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
) -> Option<usize> {
    if pc + 4 >= code_len {
        return None;
    }
    let code: &[u8] = &frame.code;
    let (op, hi, lo) = (*code.get(pc + 2)?, *code.get(pc + 3)?, *code.get(pc + 4)?);
    if op != 0xb4 {
        return None;
    }
    let zgc = fast_field_zgc(shared)?;
    getfield_fast(
        shared,
        zgc,
        sites,
        frame,
        (u16::from(hi) << 8) | u16::from(lo),
        shared.config.site_census,
        crate::runtime::interpreter::field_phases::on(),
        None,
    )
    .then_some(pc + 5)
}

/// The dispatch loop's `aload_N; {aload_M | iload_M}; putfield | invoke*`
/// pair (`CRATONVM_INTERP_FUSED_LOAD_PAIR`, `VmConfig::interp_fused_load_pair`;
/// interpreter round i4 wave 89, lane L7;
/// `interpreter-i85-L7-proposal-opcode-pair-census-of-the-battery-DONE-20261122`). The `aload_N` arm
/// has pushed its local at `pc`; this pushes the second load's local exactly
/// as its own arm would (`iload_M`: the raw slot; `aload_M`:
/// `load_ref_local`, the validated coerce included) and answers the pc to
/// run next:
///
/// * the bytecode after the `putfield` when the third bytecode is a
///   `putfield` the quickened arm ([`putfield_fast`], under its own
///   admission [`fast_field_zgc`] and the field-watch union) stores;
/// * otherwise `pc + 2`, the third bytecode, with both values pushed: a
///   `putfield` miss then runs the full handler at its own pc (NPE, final
///   rule, resolution, the FieldModification event), and an invoke runs its
///   own arm.
///
/// The caller has tested the switch, `fusion_off`, the second opcode
/// (`iload_0..3` / `aload_0..3`) and the third (`putfield` or an invoke);
/// both loads read locals 0..3 of verified code. Out of line, so the loop's
/// frame keeps its size.
#[inline(never)]
pub(super) fn fused_load_pair(
    shared: &SharedVm,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
) -> usize {
    let (second, third, hi, lo) = {
        let code: &[u8] = &frame.code;
        let byte = |i: usize| if i < code_len { code.get(i).copied() } else { None };
        (byte(pc + 1), byte(pc + 2), byte(pc + 3), byte(pc + 4))
    };
    match second {
        Some(op @ 0x1a..=0x1d) => {
            let cv = frame.get_local_compact_unchecked(usize::from(op - 0x1a));
            frame.stack.push_compact(cv);
        }
        Some(op @ 0x2a..=0x2d) => super::load_ref_local(shared, frame, usize::from(op - 0x2a)),
        // Not a shape the caller admits: run the second bytecode unfused.
        _ => return pc + 1,
    }
    if let (Some(0xb5), Some(hi), Some(lo)) = (third, hi, lo) {
        if let Some(zgc) = fast_field_zgc(shared) {
            if putfield_fast(
                zgc,
                sites,
                frame,
                (u16::from(hi) << 8) | u16::from(lo),
                shared.config.site_census,
                None,
            ) {
                return pc + 5;
            }
        }
    }
    pc + 2
}

/// Is a field watch armed, as the quickened field arms must ask before they
/// answer? `Some(armed)` is the dispatch loop's hoisted answer
/// (`VmConfig::interp_hoisted_observers`: refreshed in the loop top's slow
/// path, which every writer of the union sends the loop to); `None` loads the
/// process union ([`crate::runtime::jvmti::any_field_watchpoint_active`]) here,
/// for every caller outside the loop and with the switch off (interpreter
/// round i2 wave 62, lane L7).
#[inline(always)]
pub(super) fn field_watch_gate(watch: Option<bool>) -> bool {
    match watch {
        Some(armed) => armed,
        None => crate::runtime::jvmti::any_field_watchpoint_active(),
    }
}

/// `getfield_fast` with the site key spelled out, for the invoke fast door's
/// trivial-getter shortcut: the site belongs to the getter's declaring class
/// and constant pool, while the operand stack is the caller's (the receiver
/// on top is the getter's `this`). `ret_opcode` is the getter's `xreturn`
/// opcode, checked against the field's type the way
/// `try_execute_cached_trivial_instance_getter` checks it against the
/// descriptor; `0` skips the check for a plain `getfield`. `census` is
/// `VmConfig::site_census`.
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn getfield_fast_keyed(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    stack: &mut ValueStack,
    class_id: ClassId,
    cp_index: u16,
    ret_opcode: u8,
    census: bool,
) -> bool {
    getfield_fast_keyed_gated(
        shared,
        zgc,
        sites,
        stack,
        class_id,
        cp_index,
        ret_opcode,
        census,
        crate::runtime::interpreter::field_phases::on(),
        None,
    )
}

/// [`getfield_fast_keyed`] with the field-phase instrument's gate supplied
/// by the caller (`armed`: `field_phases::on()`), so the dispatch loop's
/// `getfield` arms test the value it hoisted (wave 61, lane L7).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn getfield_fast_keyed_gated(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    stack: &mut ValueStack,
    class_id: ClassId,
    cp_index: u16,
    ret_opcode: u8,
    census: bool,
    armed: bool,
    watch: Option<bool>,
) -> bool {
    // Phase boundaries for `CRATONVM_DBG_FIELD_PHASES=1`. `now_if(false)`
    // returns 0 and `charge_if(false, ..)` is a no-op, so an unarmed run pays
    // one predicted branch on a register per boundary. See `field_phases` for
    // what it can and cannot claim, and for the four refuted hypotheses that
    // made it necessary.
    use crate::runtime::interpreter::field_phases as ph;
    // The instrument's gate (`armed`), read ONCE per access since wave 22
    // (lane L7): every boundary below used to re-read it through `now()` /
    // `charge()`, a dozen loads of the same static byte per unarmed field
    // read; since wave 61 the dispatch loop hoists it per entry.
    let t_entry = ph::now_if(armed);
    if field_watch_gate(watch) {
        return false;
    }
    if stack.len() == 0 {
        return false;
    }
    // Boundary BEFORE the peek: the prologue and the `Acquire` load are the two
    // candidates for this phase being the largest, and both land above here.
    let t_wp = ph::now_if(armed);
    ph::charge_if(armed, ph::P_ENTRY, t_entry, t_wp);
    let Some(ptr) = stack.peek_compact().as_object_ptr() else {
        return false;
    };
    let t_gates = ph::now_if(armed);
    ph::charge_if(armed, ph::P_PEEK, t_wp, t_gates);
    let site = match sites.get(class_id, cp_index) {
        Some(s) => *s,
        None => {
            site_stats::bump(site_stats::FAST_GET_MISS);
            return false;
        }
    };
    let t_site = ph::now_if(armed);
    ph::charge_if(armed, ph::P_SITE, t_gates, t_site);
    if ret_opcode != 0 && !return_opcode_agrees(&site, ret_opcode) {
        return false;
    }
    let Some(fp) = field_ptr_for(zgc, ptr, &site) else {
        return false;
    };
    let t_ptr = ph::now_if(armed);
    ph::charge_if(armed, ph::P_PTR, t_site, t_ptr);
    // Charged at EVERY success exit, not just the compact tail.
    //
    // The first version of this instrument put the count on the compact path
    // only, and `FieldBurn`'s receiver is legacy-layout — so it returned
    // through `getfield_legacy`, the phases accumulated over millions of
    // accesses, and the denominator counted ONE. It printed
    // `accesses=1 total_cycles=1215177994`. That the count is printed at all
    // is what made it obvious; a per-access figure alone would have looked
    // merely surprising.
    macro_rules! charged_hit {
        () => {{
            if armed {
                ph::charge_if(armed, ph::P_READ, t_ptr, ph::now_if(armed));
                // CALIB last, under the conditions the phases above actually
                // ran in.
                let c0 = ph::now_if(armed);
                ph::charge_if(armed, ph::P_CALIB, c0, ph::now_if(armed));
                ph::count_access();
            }
        }};
    }
    let Some(storage) = site.storage else {
        let hit = getfield_legacy(shared, stack, fp, &site, census);
        if hit {
            charged_hit!();
        }
        return hit;
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
            charged_hit!();
            site_stats::bump_if(census, site_stats::FAST_GET_HIT);
            return true;
        }
        FieldStorageKind::Double => {
            // SAFETY: as the group above — `fp` is this site's validated
            // field address, and `Double` is an 8-byte slot.
            let v = f64::from_bits(unsafe { load_u64(fp) });
            stack.pop_compact();
            stack.push_double_unchecked(v);
            charged_hit!();
            site_stats::bump_if(census, site_stats::FAST_GET_HIT);
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
                // it and this arm does not — so it must decline when the value
                // it loaded IS one.
                //
                // This used to ask `autobox::wrapper_exists()`, the
                // process-wide latch, which is armed at bootstrap by the
                // class-mirror populator and is therefore true in every
                // process. The screen was permanently closed and this arm never
                // served a non-null reference field at all. Measured on the
                // sibling `aaload` arm, which carried the identical screen:
                // `hit=0 miss_barrier=0 miss_wrapper=2000146`.
                //
                // The precise question costs one header compare on the object
                // just loaded. See `autobox::header_is_wrapper`.
                //
                // SAFETY: a non-zero reference slot of a live compact object
                // holds an object address the collector maintains.
                let obj = shared
                    .mem
                    .heap
                    .load_and_forward(unsafe { ObjectRef::from_raw(raw as *mut u8) });
                // SAFETY: `load_and_forward` returned a live object address;
                // its first `HEADER_SIZE` bytes are a header.
                let obj_header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
                if cratonvm_gc::autobox::header_is_wrapper(obj_header) {
                    return false;
                }
                match CompactValue::try_from_pointer(obj.as_ptr() as u64) {
                    Some(cv) => cv,
                    None => return false,
                }
            }
        }
    };
    stack.pop_compact();
    stack.push_compact(pushed);
    charged_hit!();
    site_stats::bump_if(census, site_stats::FAST_GET_HIT);
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
    census: bool,
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
    site_stats::bump_if(census, site_stats::FAST_GET_HIT);
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
/// `op_putfield`. `census` is `VmConfig::site_census`, hoisted by the
/// dispatch loop; `watch` its hoisted field-watch gate ([`field_watch_gate`]).
#[inline]
pub(super) fn putfield_fast(
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    cp_index: u16,
    census: bool,
    watch: Option<bool>,
) -> bool {
    if field_watch_gate(watch) {
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
    // JVMS §6.5: a final-field store the full handler would reject must reach
    // it, whichever method filled the site.
    if !fast_put_admitted(&site, frame) {
        return false;
    }
    let Some(fp) = field_ptr_for(zgc, ptr, &site) else {
        return false;
    };
    if !put_field_value::<true>(zgc, fp, ptr, &site, val, kind) {
        return false;
    }
    frame.stack.pop_compact();
    frame.stack.pop_compact();
    site_stats::bump_if(census, site_stats::FAST_PUT_HIT);
    true
}

/// [`fused_field_increment`]'s `mode` bits, packed once per loop entry by the
/// dispatch loop (interpreter round i2 wave 69, lane L7) so the call keeps
/// seven arguments: `VmConfig::site_census`.
pub(super) const FIELD_INC_CENSUS: u8 = 1;
/// `mode`: the field-phase instrument is armed (`field_phases::on()`).
pub(super) const FIELD_INC_PHASES: u8 = 2;
/// `mode`: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE`
/// (`VmConfig::interp_fused_field_increment_wide`): narrow and long fields.
pub(super) const FIELD_INC_WIDE: u8 = 4;
/// `mode`: `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`
/// (`VmConfig::interp_fused_increment_early_decline`): the byte-only screen
/// runs before the body.
pub(super) const FIELD_INC_EARLY_DECLINE: u8 = 8;
/// `mode`: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP`
/// (`VmConfig::interp_fused_field_increment_no_dup`; interpreter round i2
/// wave 70, lane L7): the group may also open `aload_N; aload_N` (the same
/// local twice) where the `dup` form opens `aload_N; dup`. The loop tests
/// this bit before it calls for that prefix, so with it clear the prefix
/// never reaches the helper.
pub(super) const FIELD_INC_NO_DUP: u8 = 16;
/// `mode`: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE`
/// (`VmConfig::interp_fused_field_increment_value`; interpreter round i3
/// wave 71, lane L7): the `dup` form whose updated value is used, with a
/// `dup_x1` (`dup2_x1` for a long) right after the `getfield` (the old value
/// stays on the stack: `last = count++`, `a[size++] = x`) or right before
/// the `putfield` (the new one: `return ++count`, `id = (seq += k)`). A hit
/// pushes that one value, as the unfused group leaves it.
pub(super) const FIELD_INC_VALUE: u8 = 32;
/// `mode`: `CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS`
/// (`VmConfig::interp_fused_field_update_two_fields`; interpreter round i3
/// wave 72, lane L7; proposal
/// `i71-L7-proposal-fuse-the-two-field-update-20261104`): the `aload_N;
/// aload_N` group may store to ANOTHER field of the same receiver than the
/// one it read, `this.a = this.b + k` ([`fused_two_field_update`]). Set only
/// with [`FIELD_INC_NO_DUP`] ([`with_two_fields`]); the loop's screen
/// ([`no_dup_group_may_fuse`]) then passes a `putfield` of any index.
pub(super) const FIELD_INC_TWO_FIELDS: u8 = 64;
/// `mode`: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT` itself; a zero `mode` is
/// the switch off, which the loop tests.
pub(super) const FIELD_INC_ON: u8 = 0x80;
/// [`fused_field_increment`]'s `mode`, ABOVE the hoisted byte:
/// `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W` admitted (interpreter round i3
/// wave 83, lane L7; `VmConfig::interp_fused_increment_ldc2w` with the
/// `ldc2_w` inline hit). The byte above is full, so this is the loop's own
/// gate bit (`SLG_FUSED_INCREMENT_LDC2W` of its hoisted `static_ldc_gates`
/// word), which the `aload_N` arm ORs into the call's `u16` mode: no new
/// hoisted local. Read only with [`FIELD_INC_WIDE`]: the long operand may
/// then also be an `ldc2_w` of a `CONSTANT_Long` the thread's
/// numeric-constant table holds (`super::ldc2w_long_operand`).
pub(super) const FIELD_INC_LDC2W: u16 = super::SLG_FUSED_INCREMENT_LDC2W;
const _: () = assert!(FIELD_INC_LDC2W > 0xff && FIELD_INC_LDC2W.count_ones() == 1);

/// `mode` with [`FIELD_INC_TWO_FIELDS`] added when `on` and the mode already
/// carries [`FIELD_INC_NO_DUP`] (so never to a zero, switched-off mode).
#[inline]
pub(super) fn with_two_fields(mode: u8, on: bool) -> u8 {
    if on && mode & FIELD_INC_NO_DUP != 0 {
        mode | FIELD_INC_TWO_FIELDS
    } else {
        mode
    }
}

/// The `mode` the dispatch loop hoists for [`fused_field_increment`]: `0`
/// with `CRATONVM_INTERP_FUSED_FIELD_INCREMENT` off, else [`FIELD_INC_ON`]
/// with the other bits as configured.
#[inline]
pub(super) fn field_increment_mode(
    on: bool,
    census: bool,
    phases_on: bool,
    wide: bool,
    early_decline: bool,
    no_dup: bool,
    value: bool,
) -> u8 {
    if !on {
        return 0;
    }
    let bit = |set: bool, b: u8| if set { b } else { 0 };
    FIELD_INC_ON
        | bit(census, FIELD_INC_CENSUS)
        | bit(phases_on, FIELD_INC_PHASES)
        | bit(wide, FIELD_INC_WIDE)
        | bit(early_decline, FIELD_INC_EARLY_DECLINE)
        | bit(no_dup, FIELD_INC_NO_DUP)
        | bit(value, FIELD_INC_VALUE)
}

/// The byte length of an operand the fused increments accept, by opcode
/// (`iconst_m1` .. `iconst_5`, `lconst_0/1`, `iload_0..3`, `lload_0..3`: 1;
/// `bipush`, `iload`, `lload`: 2; `sipush`: 3); `0` for every other opcode.
/// [`no_dup_group_may_fuse`]'s one lookup (round i3 wave 71, lane L7).
const INCREMENT_OPERAND_LEN: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut op = 0x02;
    while op <= 0x0a {
        t[op] = 1;
        op += 1;
    }
    let mut op = 0x1a;
    while op <= 0x21 {
        t[op] = 1;
        op += 1;
    }
    t[0x10] = 2;
    t[0x15] = 2;
    t[0x16] = 2;
    t[0x11] = 3;
    t
};

/// The dispatch loop's screen for the `aload_N; aload_N; getfield` prefix
/// under [`FIELD_INC_NO_DUP`] (interpreter round i3 wave 71, lane L7;
/// proposal `i69-L7-proposal-fuse-the-field-increment-without-dup`): `false`
/// when the `putfield` the group would end with does not carry the
/// `getfield`'s two index bytes (`this.a = this.b + k`, which shares the
/// prefix), or no operand the fused increments accept follows the
/// `getfield` (`a = b + c`). `code` is the UNPADDED code; every byte is
/// read through the slice.
///
/// Made as cheap as the question allows after the wave-71 host run (the
/// first version reused the early-decline screen `increment_group_screen`,
/// whose per-byte `Option` closure and operand `match` cost the declined
/// two-field rows 5-12% per execution): one table lookup gives the
/// operand's length and so the position of the arithmetic, one byte test
/// skips an `i2b` / `i2c` / `i2s`, and one three-byte compare checks the
/// `putfield`. The arithmetic opcode itself is not tested: the helper
/// decodes the whole group again and decides under its own mode, so `true`
/// is not a promise (only the declines must be exact, and every group the
/// decoder takes passes here).
///
/// `any_put` ([`FIELD_INC_TWO_FIELDS`]; round i3 wave 72, lane L7): the
/// `putfield` may carry any index (`this.a = this.b + k`); a `false` keeps
/// the wave-71 answer exactly.
#[inline(always)]
pub(super) fn no_dup_group_may_fuse(code: &[u8], pc: usize, any_put: bool) -> bool {
    let (Some(&hi), Some(&lo), Some(&op)) = (code.get(pc + 3), code.get(pc + 4), code.get(pc + 5))
    else {
        return false;
    };
    let len = usize::from(INCREMENT_OPERAND_LEN[usize::from(op)]);
    if len == 0 {
        return false;
    }
    // The arithmetic sits at `pc + 5 + len`; the `putfield` after it, or
    // after the conversion that follows it.
    let mut put = pc + 6 + len;
    if matches!(code.get(put), Some(0x91..=0x93)) {
        put += 1;
    }
    match code.get(put..put + 3) {
        Some(&[0xb5, h, l]) => any_put || (h == hi && l == lo),
        _ => false,
    }
}

/// What a fused field increment leaves on the operand stack
/// ([`FIELD_INC_VALUE`]; interpreter round i3 wave 71, lane L7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FieldIncrementUse {
    /// The statement form: nothing (net stack effect zero).
    Statement,
    /// `dup_x1` / `dup2_x1` before the `putfield`: the stored (narrowed)
    /// value, as the pre-increment leaves it.
    New,
    /// `dup_x1` / `dup2_x1` after the `getfield`: the value the `getfield`
    /// read, as the post-increment leaves it.
    Old,
}

/// The fused instance-field increment
/// (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT`; interpreter round i2 wave 68,
/// lane L7): at `pc` of `frame` an `aload_N` (`0x2a..=0x2d`), a `dup`, a
/// `getfield #F`, an int operand (`super::static_increment_operand`: the
/// static twin's decoder, `iconst_*`, `bipush`, `sipush`, `iload`), an
/// `iadd` or `isub`, and a `putfield` whose 16-bit pool index is byte for
/// byte the `getfield`'s (two Fieldrefs naming one field stay unfused), all
/// below `code_len` (the unpadded length). `this.count++`, `modCount++`,
/// `size -= k`.
///
/// Under [`FIELD_INC_WIDE`] (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE`;
/// wave 69, lane L7) two more shapes are served, with the same site, final
/// rule and declines: a narrow field, `... <int operand>; iadd | isub; i2b |
/// i2c | i2s; putfield #F (B / C / S)`, where the conversion must be the
/// descriptor's own and the narrowed int is stored; and a `long` field,
/// `... getfield #F (J); lconst_0 | lconst_1 | lload; ladd | lsub; putfield
/// #F` (`super::static_increment_long_operand`), read as the quickened
/// `getfield` reads a `J` and stored with the long stack kind. Without it
/// every one of those declines, and the `I` group is decided as in wave 68.
///
/// Under [`FIELD_INC_EARLY_DECLINE`]
/// (`CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`; wave 69, lane L7) the
/// group's bytes are screened here first ([`field_increment_screen`]) and a
/// shape that cannot be fused returns before the body is called; a group the
/// screen passes is decided by the body exactly as without it.
///
/// Under [`FIELD_INC_NO_DUP`] (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP`;
/// wave 70, lane L7) the group may open `aload_N; aload_N` (both the same
/// opcode byte, so the same local) instead of `aload_N; dup`: `count = count
/// + 1`, `other.size = other.size - k`. Both prefixes are two bytes before
/// the `getfield`, both push the same receiver twice (the local cannot change
/// between the loads, and no operand the decoder accepts stores a local), so
/// the rest of the group, the site, the rules and every decline are the `dup`
/// form's unchanged.
///
/// Under [`FIELD_INC_VALUE`] (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE`;
/// interpreter round i3 wave 71, lane L7) the `dup` form may also carry the
/// `dup_x1` (an int group) or `dup2_x1` (a long group) javac emits when the
/// updated value is used: right after the `getfield` (post-increment, the
/// old value is left) or right before the `putfield` (pre-increment, the new
/// narrowed value is left). The site, the rules and every decline are the
/// statement form's; a hit then pushes that one value with the stack kind the
/// unfused group leaves (an int slot, or a `KIND_LONG` one), after checking
/// the operand stack has room for it BEFORE the field is stored, so a
/// decline still writes nothing. Value forms are taken after `dup` only (not
/// with the `aload_N; aload_N` prefix).
///
/// The caller has tested `fusion_off`, the switch and `fast_field_zgc`
/// (which is `zgc`). `mode` packs [`FIELD_INC_CENSUS`], [`FIELD_INC_PHASES`],
/// [`FIELD_INC_WIDE`], [`FIELD_INC_EARLY_DECLINE`], [`FIELD_INC_NO_DUP`] and
/// [`FIELD_INC_VALUE`].
/// Returns the pc past
/// the `putfield`; `None`, having written nothing, for any other shape.
///
/// Out of line and small: this entry keeps no state of its own across a
/// call, so a screened decline costs a call and a few byte loads. The body is
/// `#[inline(always)]` into this entry: as a second `#[inline(never)]`
/// function reached by a tail call (wave 69) it cost every fused hit ~12% with
/// the screen off (`fieldInc` 26.3 -> 29.9 ns/iter, wave-69 triage).
///
/// `tables` is the thread's `JvmThread::site_tables` (interpreter round i3
/// wave 83, lane L7, `i70-L7-proposal-one-argument-for-the-fused-helpers-site-tables`):
/// one argument, in the slot the field-site table alone took before, from
/// which the body reads `fast_field_sites` exactly as it read that table.
/// `mode` is a `u16` since the same wave: its low byte is the hoisted mode
/// (every bit above), and [`FIELD_INC_LDC2W`] sits above it; under that bit
/// (with [`FIELD_INC_WIDE`]) the decoder reads an `ldc2_w` operand's
/// constant from `tables.prim_const_sites`. Same argument count, so the
/// loop's outgoing-argument area does not grow.
#[inline(never)]
pub(super) fn fused_field_increment(
    zgc: &ZgcRealHeap,
    tables: &mut InterpSiteTables,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
    mode: u16,
    watch: Option<bool>,
) -> Option<usize> {
    let ldc2w = mode & FIELD_INC_LDC2W != 0;
    let [mode, _] = mode.to_le_bytes();
    if mode & FIELD_INC_EARLY_DECLINE != 0
        && !field_increment_screen(
            &frame.code,
            code_len,
            pc,
            mode & FIELD_INC_WIDE != 0,
            mode & FIELD_INC_NO_DUP != 0,
            mode & FIELD_INC_VALUE != 0,
            ldc2w,
        )
    {
        return None;
    }
    let InterpSiteTables {
        fast_field_sites,
        prim_const_sites,
        ..
    } = tables;
    fused_field_increment_body(
        zgc,
        fast_field_sites,
        frame,
        code_len,
        pc,
        mode,
        watch,
        ldc2w.then_some(prim_const_sites),
    )
}

/// [`fused_field_increment`] over a bare field-site table, for the tests
/// written before wave 83 grouped the tables (`sites` is moved into an
/// otherwise empty [`InterpSiteTables`] for the call and back after it, so
/// what the call filled or read is the caller's table).
#[cfg(test)]
fn fused_field_increment_on(
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
    mode: u8,
    watch: Option<bool>,
) -> Option<usize> {
    let mut tables = InterpSiteTables::new();
    std::mem::swap(&mut tables.fast_field_sites, sites);
    let next = fused_field_increment(
        zgc,
        &mut tables,
        frame,
        code_len,
        pc,
        u16::from(mode),
        watch,
    );
    std::mem::swap(&mut tables.fast_field_sites, sites);
    next
}

/// The body of [`fused_field_increment`]. The field-watch gate (`watch`, the
/// hoisted union of `FieldAccess` and `FieldModification` watches, as
/// `getfield_fast` and `putfield_fast` test it) and the field-phase
/// instrument ([`FIELD_INC_PHASES`]; an armed run counts every access through
/// `getfield_fast`) decline. The receiver is local `N` as `aload_N` would
/// push it: an `Object`-tagged, non-null slot (a null, a coerced or any other
/// shape declines, so the unfused `getfield` raises the NPE at its own pc).
/// The one entry of `sites` keyed by the index serves both accesses, as it
/// serves the two quickened arms; its descriptor must be the one the group
/// stores to (`I` unnarrowed; `B` / `C` / `S` behind their own conversion;
/// `J` from a long group), it must be a field the quickened `putfield` would
/// store to (`fast_put_admitted`: a final declines outside the frames the
/// rule allows), and the receiver must pass `field_ptr_for` (its probe and
/// its class / slot-count / compact-flag check). A volatile field never has
/// an entry (`fill_site` refuses it), so no fence is needed here. The value
/// is read as `getfield_fast` reads the field (compact: the slot of the
/// descriptor's own storage kind, sign- or zero-extended as the quickened
/// arm extends it; legacy: the checked cell, an `Int` narrowed to the
/// descriptor or a zero-initialized `Object(None)` for an int-typed field, a
/// `Long`, a widened `Int` or `Object(None)` for a `J`, any other shape
/// declines), added to / subtracted from with the wrapping arithmetic,
/// narrowed by the group's own conversion, and stored by
/// `put_field_value::<true>`, the quickened `putfield`'s own store. One get
/// hit and one put hit are counted, as the two quickened arms count them.
/// Nothing is popped; nothing is pushed for the statement form (the group's
/// net stack effect is zero), and one value for a [`FIELD_INC_VALUE`] form.
///
/// `consts` (wave 83, lane L7) is the thread's numeric-constant table when
/// [`FIELD_INC_LDC2W`] is admitted, else `None`: the decoder's `ldc2_w`
/// operand (under `wide`) reads it, and every other shape is decided as
/// before.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn fused_field_increment_body(
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
    mode: u8,
    watch: Option<bool>,
    consts: Option<&mut PrimitiveConstantSiteCache>,
) -> Option<usize> {
    // `CRATONVM_DBG_FIELD_SITE` (the census bit): every decline below is
    // counted by its reason, every hit by its form (round i3 wave 71, lane
    // L7), so a host run can tell "the group never fused" from "it fused and
    // gained nothing" without a clock.
    let census = mode & FIELD_INC_CENSUS != 0;
    macro_rules! decline {
        ($why:expr) => {{
            site_stats::bump_if(census, $why);
            return None;
        }};
    }
    if mode & FIELD_INC_PHASES != 0 || field_watch_gate(watch) {
        decline!(site_stats::FIELD_INC_DECLINE_GATE);
    }
    let wide = mode & FIELD_INC_WIDE != 0;
    let no_dup = mode & FIELD_INC_NO_DUP != 0;
    let value = mode & FIELD_INC_VALUE != 0;
    let Some((index, local, operand, subtract, narrow, next_pc, used)) =
        decode_field_increment_used(
            frame,
            &frame.code,
            code_len,
            pc,
            wide,
            no_dup,
            value,
            consts,
        )
    else {
        // Round i3 wave 72 (lane L7): a two-field update is tried only once
        // the one-field decoder has refused, so no one-field hit pays for it.
        if mode & FIELD_INC_TWO_FIELDS != 0 {
            return fused_two_field_update(zgc, sites, frame, code_len, pc, wide, census);
        }
        decline!(site_stats::FIELD_INC_DECLINE_SHAPE);
    };
    if local >= usize::from(frame.max_locals) {
        decline!(site_stats::FIELD_INC_DECLINE_SHAPE);
    }
    // A value form pushes one slot: decided before anything is stored, so a
    // full stack (never under verified code: the unfused group peaks two
    // slots higher) declines having written nothing.
    if used != FieldIncrementUse::Statement && !frame.stack.has_room() {
        decline!(site_stats::FIELD_INC_DECLINE_STORE);
    }
    let ptr = match frame.get_local_compact_unchecked(local).as_object_ptr() {
        Some(p) if p != 0 => p,
        _ => decline!(site_stats::FIELD_INC_DECLINE_RECEIVER),
    };
    let Some(&site) = sites.get(frame.class_id, index) else {
        decline!(site_stats::FIELD_INC_DECLINE_SITE);
    };
    // The field each shape stores to: `I` unnarrowed, `B` / `C` / `S` behind
    // their own conversion only (never `Z`), `J` from a long group.
    match (operand, narrow, site.desc_byte) {
        (super::IncrementOperand::Int(_), None, b'I')
        | (super::IncrementOperand::Int(_), Some(0x91), b'B')
        | (super::IncrementOperand::Int(_), Some(0x92), b'C')
        | (super::IncrementOperand::Int(_), Some(0x93), b'S')
        | (super::IncrementOperand::Long(_), None, b'J') => {}
        _ => decline!(site_stats::FIELD_INC_DECLINE_FIELD),
    }
    if !fast_put_admitted(&site, frame) {
        decline!(site_stats::FIELD_INC_DECLINE_FIELD);
    }
    let Some(fp) = field_ptr_for(zgc, ptr, &site) else {
        decline!(site_stats::FIELD_INC_DECLINE_RECEIVER);
    };
    // `(stored, the value the group leaves)`: the old value as the
    // `getfield` read it, or the new one as the `putfield` stored it.
    let (stored, left) = match operand {
        super::IncrementOperand::Int(k) => {
            let Some(current) = field_increment_read_int(fp, &site) else {
                decline!(site_stats::FIELD_INC_DECLINE_STORE);
            };
            let next = if subtract {
                current.wrapping_sub(k)
            } else {
                current.wrapping_add(k)
            };
            let narrowed = super::narrow_increment_result(next, narrow);
            // `kind` is read only for a `J` / `D` store; an int store ignores it.
            let stored =
                put_field_value::<true>(zgc, fp, ptr, &site, CompactValue::int(narrowed), 0);
            let left = match used {
                FieldIncrementUse::Old => super::IncrementOperand::Int(current),
                _ => super::IncrementOperand::Int(narrowed),
            };
            (stored, left)
        }
        super::IncrementOperand::Long(k) => {
            let Some(current) = field_increment_read_long(fp, &site) else {
                decline!(site_stats::FIELD_INC_DECLINE_STORE);
            };
            let next = if subtract {
                current.wrapping_sub(k)
            } else {
                current.wrapping_add(k)
            };
            let stored = put_field_value::<true>(
                zgc,
                fp,
                ptr,
                &site,
                CompactValue::long(next),
                ValueStack::KIND_MARK_LONG,
            );
            let left = match used {
                FieldIncrementUse::Old => super::IncrementOperand::Long(current),
                _ => super::IncrementOperand::Long(next),
            };
            (stored, left)
        }
    };
    if !stored {
        decline!(site_stats::FIELD_INC_DECLINE_STORE);
    }
    // What `dup_x1` / `dup2_x1` leaves under the receiver the `putfield`
    // pops: an int slot (`iadd` / `i2b` / the quickened int `getfield` push
    // the same `KIND_UNKNOWN` int), or a `KIND_LONG` one (`ladd` and the
    // quickened `J` `getfield` push that). Room was checked above.
    if used != FieldIncrementUse::Statement {
        match left {
            super::IncrementOperand::Int(v) => frame.stack.push_int_unchecked(v),
            super::IncrementOperand::Long(v) => frame.stack.push_long_unchecked(v),
        }
    }
    site_stats::bump_if(census, site_stats::FAST_GET_HIT);
    site_stats::bump_if(census, site_stats::FAST_PUT_HIT);
    site_stats::bump_if(
        census,
        if used == FieldIncrementUse::Statement {
            site_stats::FIELD_INC_HIT
        } else {
            site_stats::FIELD_INC_VALUE_HIT
        },
    );
    Some(next_pc)
}

/// The two-field update `aload_N; aload_N; getfield #G; <operand>; iadd |
/// isub; [i2b | i2c | i2s]; putfield #F` with `#F != #G`, both on the
/// receiver in local `N`: `this.a = this.b + k`, `hi = lo - 1`,
/// `limit = pos + n` (`CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS`,
/// [`FIELD_INC_TWO_FIELDS`];
/// interpreter round i3 wave 72, lane L7; proposal
/// `i71-L7-proposal-fuse-the-two-field-update-20261104`). Called by
/// [`fused_field_increment_body`] after its own gates (the watch union, the
/// field-phase instrument) and only once the one-field decoder refused the
/// group. Each pool index has its own site in `sites`: the read site's
/// descriptor must be an int-typed one (`I`, `B`, `C`, `S`) for an int group
/// or `J` for a long group (read as the quickened `getfield` reads it), the
/// store site's must be the one the group stores to (as for the one-field
/// increment: `I` unnarrowed, `B` / `C` / `S` behind their own conversion,
/// `J` from a long group) and pass `fast_put_admitted`; both must pass
/// `field_ptr_for` against the one receiver. The narrow and long shapes need
/// [`FIELD_INC_WIDE`] (`wide`), as the one-field ones do. Statement form
/// only (javac puts no `dup` in this shape). Returns the pc past the
/// `putfield`; `None`, having written nothing and pushed nothing, for every
/// other shape, so the plain `aload_N` runs and the `aload_N; getfield`
/// fusion serves the read.
///
/// Out of line: the one-field body it is reached from stays as it was on
/// every one-field hit.
#[inline(never)]
fn fused_two_field_update(
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    frame: &Frame,
    code_len: usize,
    pc: usize,
    wide: bool,
    census: bool,
) -> Option<usize> {
    macro_rules! decline {
        ($why:expr) => {{
            site_stats::bump_if(census, $why);
            return None;
        }};
    }
    let Some((get_index, put_index, local, operand, subtract, narrow, next_pc)) =
        decode_two_field_update(frame, &frame.code, code_len, pc, wide)
    else {
        decline!(site_stats::FIELD_INC_DECLINE_SHAPE);
    };
    if local >= usize::from(frame.max_locals) {
        decline!(site_stats::FIELD_INC_DECLINE_SHAPE);
    }
    let ptr = match frame.get_local_compact_unchecked(local).as_object_ptr() {
        Some(p) if p != 0 => p,
        _ => decline!(site_stats::FIELD_INC_DECLINE_RECEIVER),
    };
    let Some(&get_site) = sites.get(frame.class_id, get_index) else {
        decline!(site_stats::FIELD_INC_DECLINE_SITE);
    };
    let Some(&put_site) = sites.get(frame.class_id, put_index) else {
        decline!(site_stats::FIELD_INC_DECLINE_SITE);
    };
    match (operand, get_site.desc_byte) {
        (super::IncrementOperand::Int(_), b'I' | b'B' | b'C' | b'S')
        | (super::IncrementOperand::Long(_), b'J') => {}
        _ => decline!(site_stats::FIELD_INC_DECLINE_FIELD),
    }
    match (operand, narrow, put_site.desc_byte) {
        (super::IncrementOperand::Int(_), None, b'I')
        | (super::IncrementOperand::Int(_), Some(0x91), b'B')
        | (super::IncrementOperand::Int(_), Some(0x92), b'C')
        | (super::IncrementOperand::Int(_), Some(0x93), b'S')
        | (super::IncrementOperand::Long(_), None, b'J') => {}
        _ => decline!(site_stats::FIELD_INC_DECLINE_FIELD),
    }
    if !fast_put_admitted(&put_site, frame) {
        decline!(site_stats::FIELD_INC_DECLINE_FIELD);
    }
    let (Some(get_fp), Some(put_fp)) = (
        field_ptr_for(zgc, ptr, &get_site),
        field_ptr_for(zgc, ptr, &put_site),
    ) else {
        decline!(site_stats::FIELD_INC_DECLINE_RECEIVER);
    };
    let stored = match operand {
        super::IncrementOperand::Int(k) => {
            let Some(current) = field_increment_read_int(get_fp, &get_site) else {
                decline!(site_stats::FIELD_INC_DECLINE_STORE);
            };
            let next = if subtract {
                current.wrapping_sub(k)
            } else {
                current.wrapping_add(k)
            };
            let narrowed = super::narrow_increment_result(next, narrow);
            put_field_value::<true>(zgc, put_fp, ptr, &put_site, CompactValue::int(narrowed), 0)
        }
        super::IncrementOperand::Long(k) => {
            let Some(current) = field_increment_read_long(get_fp, &get_site) else {
                decline!(site_stats::FIELD_INC_DECLINE_STORE);
            };
            let next = if subtract {
                current.wrapping_sub(k)
            } else {
                current.wrapping_add(k)
            };
            put_field_value::<true>(
                zgc,
                put_fp,
                ptr,
                &put_site,
                CompactValue::long(next),
                ValueStack::KIND_MARK_LONG,
            )
        }
    };
    if !stored {
        decline!(site_stats::FIELD_INC_DECLINE_STORE);
    }
    site_stats::bump_if(census, site_stats::FAST_GET_HIT);
    site_stats::bump_if(census, site_stats::FAST_PUT_HIT);
    site_stats::bump_if(census, site_stats::FIELD_INC_TWO_FIELDS_HIT);
    Some(next_pc)
}

/// The decoder of [`fused_two_field_update`] at `pc` of `code` (unpadded
/// length `code_len`; every byte read through the slice, below it): `(read
/// index, store index, receiver local, operand, subtract, conversion, pc
/// past the putfield)`, or `None` unless the group opens with the same
/// `aload_N` twice and its two pool indexes differ (the same index is the
/// one-field increment's). The operand, the arithmetic and the conversion
/// are decoded as [`decode_field_increment_used`] decodes them (the long
/// operand and the conversion only under `wide`).
#[inline(always)]
#[allow(clippy::type_complexity)]
fn decode_two_field_update(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    pc: usize,
    wide: bool,
) -> Option<(
    u16,
    u16,
    usize,
    super::IncrementOperand,
    bool,
    Option<u8>,
    usize,
)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let load = byte(pc)?;
    if !(0x2a..=0x2d).contains(&load) || byte(pc + 1)? != load || byte(pc + 2)? != 0xb4 {
        return None;
    }
    let get_index = (u16::from(byte(pc + 3)?) << 8) | u16::from(byte(pc + 4)?);
    let (operand, op_pc) = match super::static_increment_operand(frame, code, code_len, pc + 5) {
        Some((k, p)) => (super::IncrementOperand::Int(k), p),
        None if wide => {
            let (k, p) = super::static_increment_long_operand(frame, code, code_len, pc + 5)?;
            (super::IncrementOperand::Long(k), p)
        }
        None => return None,
    };
    let subtract = match (operand, byte(op_pc)?) {
        (super::IncrementOperand::Int(_), 0x60) | (super::IncrementOperand::Long(_), 0x61) => false,
        (super::IncrementOperand::Int(_), 0x64) | (super::IncrementOperand::Long(_), 0x65) => true,
        _ => return None,
    };
    let mut put_pc = op_pc + 1;
    let narrow = match (operand, byte(put_pc)?) {
        (super::IncrementOperand::Int(_), conv @ 0x91..=0x93) if wide => {
            put_pc += 1;
            Some(conv)
        }
        _ => None,
    };
    if byte(put_pc)? != 0xb5 {
        return None;
    }
    let put_index = (u16::from(byte(put_pc + 1)?) << 8) | u16::from(byte(put_pc + 2)?);
    if put_index == get_index {
        return None;
    }
    Some((
        get_index,
        put_index,
        usize::from(load - 0x2a),
        operand,
        subtract,
        narrow,
        put_pc + 3,
    ))
}

/// The int the quickened `getfield` would push for the int-typed field
/// (`I`, `B`, `C`, `S`; the caller has matched the descriptor) at `fp`, or
/// `None` for a storage or a cell shape it would not answer this way.
#[inline(always)]
fn field_increment_read_int(fp: *mut u8, site: &FastFieldSite) -> Option<i32> {
    // SAFETY (each raw load): `fp` addresses this site's field inside a live,
    // header-validated compact object (`field_ptr_for`), and each width is
    // the layout's own for the storage kind matched (`fill_site` checked the
    // kind against the descriptor; the pairs are spelled out regardless).
    match (site.desc_byte, site.storage) {
        (b'I', Some(FieldStorageKind::Int)) => Some((unsafe { load_u32(fp) }) as i32),
        // Cast: sign-extend the 1-byte slot, as `getfield_fast` does.
        (b'B', Some(FieldStorageKind::Byte)) => Some(i32::from((unsafe { load_u8(fp) }) as i8)),
        (b'C', Some(FieldStorageKind::Char)) => Some(i32::from(unsafe { load_u16(fp) })),
        // Cast: sign-extend the 2-byte slot, as `getfield_fast` does.
        (b'S', Some(FieldStorageKind::Short)) => Some(i32::from((unsafe { load_u16(fp) }) as i16)),
        (_, Some(_)) => None,
        // SAFETY: `fp` is a 16-byte field cell of a live legacy object
        // (`field_ptr_for` checked the receiver against the site).
        (desc, None) => {
            match unsafe { cratonvm_types::read_value_checked_atomic(fp as *const Value) }? {
                Value::Int(x) => match super::narrow_int_to_field_type(Value::Int(x), desc) {
                    Value::Int(v) => Some(v),
                    _ => None,
                },
                Value::Object(None) => Some(0),
                _ => None,
            }
        }
    }
}

/// The long the quickened `getfield` would push for the `J` field at `fp`
/// (the caller has matched the descriptor), or `None` for a storage or a
/// cell shape it would not answer this way.
#[inline(always)]
fn field_increment_read_long(fp: *mut u8, site: &FastFieldSite) -> Option<i64> {
    match site.storage {
        // SAFETY: as in `field_increment_read_int`; `Long` is an 8-byte slot.
        Some(FieldStorageKind::Long) => Some((unsafe { load_u64(fp) }) as i64),
        Some(_) => None,
        // SAFETY: `fp` is a 16-byte field cell of a live legacy object.
        None => match unsafe { cratonvm_types::read_value_checked_atomic(fp as *const Value) }? {
            Value::Long(x) => Some(x),
            Value::Int(x) => Some(i64::from(x)),
            Value::Object(None) => Some(0),
            _ => None,
        },
    }
}

/// The shape [`fused_field_increment`] fuses, decoded at `pc` of `code`
/// (unpadded length `code_len`; every byte read through the slice, below
/// it): `(pool index, receiver local, operand, subtract, conversion, pc past
/// the putfield)`, or `None` for any other shape. The operand's local, if
/// any, is read from `frame` by `super::static_increment_operand` (or, under
/// `wide`, `super::static_increment_long_operand`); a conversion (`i2b` /
/// `i2c` / `i2s` after an int operation) is accepted only under `wide`. The
/// second byte is a `dup`, or, under `no_dup`, the first byte again (the
/// same `aload_N`). The statement form only: [`decode_field_increment_used`]
/// with `value` clear, which the tests pin.
#[cfg(test)]
#[allow(clippy::type_complexity)]
fn decode_field_increment(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    pc: usize,
    wide: bool,
    no_dup: bool,
) -> Option<(u16, usize, super::IncrementOperand, bool, Option<u8>, usize)> {
    let (index, local, operand, subtract, narrow, next_pc, _) =
        decode_field_increment_used(frame, code, code_len, pc, wide, no_dup, false, None)?;
    Some((index, local, operand, subtract, narrow, next_pc))
}

/// The decoder of [`fused_field_increment`], with the value forms
/// (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE`; interpreter round i3 wave
/// 71, lane L7): under `value`, and only after a `dup` (never the `no_dup`
/// prefix), a `dup_x1` / `dup2_x1` right after the `getfield` (then the
/// operand follows it: [`FieldIncrementUse::Old`]) or right before the
/// `putfield` (after the arithmetic and any conversion:
/// [`FieldIncrementUse::New`]). The duplicate's width must be the group's
/// (`dup_x1` over an int operation, `dup2_x1` over a long one); one
/// duplicate at most. Without `value` every such group is refused, as
/// before.
///
/// `consts` (`CRATONVM_INTERP_FUSED_INCREMENT_LDC2W`; interpreter round i3
/// wave 83, lane L7): when `Some` (the switch admitted), the long operand
/// under `wide` may also be an `ldc2_w` whose `CONSTANT_Long` the table holds
/// (`super::ldc2w_long_operand`: a miss or a `Double` refuses the group).
/// `None` decides every group exactly as before.
#[inline(always)]
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn decode_field_increment_used(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    pc: usize,
    wide: bool,
    no_dup: bool,
    value: bool,
    consts: Option<&mut PrimitiveConstantSiteCache>,
) -> Option<(
    u16,
    usize,
    super::IncrementOperand,
    bool,
    Option<u8>,
    usize,
    FieldIncrementUse,
)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let load = byte(pc)?;
    let second = byte(pc + 1)?;
    if !(0x2a..=0x2d).contains(&load)
        || !(second == 0x59 || (no_dup && second == load))
        || byte(pc + 2)? != 0xb4
    {
        return None;
    }
    let (hi, lo) = (byte(pc + 3)?, byte(pc + 4)?);
    // The value forms follow a `dup` only.
    let value = value && second == 0x59;
    // Post-increment: the duplicate of the value just read, its width
    // (`true`: `dup2_x1`, a long) checked against the operand below.
    let post_long = if value {
        match byte(pc + 5)? {
            0x5a => Some(false),
            0x5d => Some(true),
            _ => None,
        }
    } else {
        None
    };
    let operand_pc = if post_long.is_some() { pc + 6 } else { pc + 5 };
    let (operand, op_pc) = match super::static_increment_operand(frame, code, code_len, operand_pc)
    {
        Some((k, p)) => (super::IncrementOperand::Int(k), p),
        None if wide => {
            let (k, p) =
                match super::static_increment_long_operand(frame, code, code_len, operand_pc) {
                    Some(found) => found,
                    None => super::ldc2w_long_operand(
                        code,
                        code_len,
                        operand_pc,
                        frame.class_id,
                        consts?,
                    )?,
                };
            (super::IncrementOperand::Long(k), p)
        }
        None => return None,
    };
    let long_group = matches!(operand, super::IncrementOperand::Long(_));
    if matches!(post_long, Some(l) if l != long_group) {
        return None;
    }
    let subtract = match (operand, byte(op_pc)?) {
        (super::IncrementOperand::Int(_), 0x60) | (super::IncrementOperand::Long(_), 0x61) => false,
        (super::IncrementOperand::Int(_), 0x64) | (super::IncrementOperand::Long(_), 0x65) => true,
        _ => return None,
    };
    let mut put_pc = op_pc + 1;
    let narrow = match (operand, byte(put_pc)?) {
        (super::IncrementOperand::Int(_), conv @ 0x91..=0x93) if wide => {
            put_pc += 1;
            Some(conv)
        }
        _ => None,
    };
    // Pre-increment: the duplicate of the result, before the `putfield`.
    let dup_op = if long_group { 0x5d } else { 0x5a };
    let used = if post_long.is_some() {
        FieldIncrementUse::Old
    } else if value && byte(put_pc)? == dup_op {
        put_pc += 1;
        FieldIncrementUse::New
    } else {
        FieldIncrementUse::Statement
    };
    if byte(put_pc)? != 0xb5 || byte(put_pc + 1)? != hi || byte(put_pc + 2)? != lo {
        return None;
    }
    let index = (u16::from(hi) << 8) | u16::from(lo);
    Some((
        index,
        usize::from(load - 0x2a),
        operand,
        subtract,
        narrow,
        put_pc + 3,
        used,
    ))
}

/// The byte-only screen of [`fused_field_increment`]
/// (`CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`; wave 69, lane L7):
/// `false` when the group at `pc` of `code` (unpadded length `code_len`)
/// cannot be a shape [`decode_field_increment_used`] accepts under `wide`,
/// decided from opcode bytes and the two pool-index pairs alone
/// (`super::increment_group_screen`: no local read, no site probe). `true`
/// is not a promise: the body decodes again and decides. Under `no_dup` the
/// second byte may be the first again, as [`decode_field_increment_used`]
/// takes it. Under `value` (wave 71, lane L7) a `dup`-prefixed group may also
/// carry its `dup_x1` / `dup2_x1` after the `getfield` or before the
/// `putfield`, as the decoder takes it. Under `ldc2w` (wave 83, lane L7,
/// [`FIELD_INC_LDC2W`]) the operand may also be an `ldc2_w` under `wide`.
#[inline(always)]
fn field_increment_screen(
    code: &[u8],
    code_len: usize,
    pc: usize,
    wide: bool,
    no_dup: bool,
    value: bool,
    ldc2w: bool,
) -> bool {
    // The `aload_N` (the arm's own opcode) and the `dup; getfield` (or
    // `aload_N; getfield`) the loop tested are re-read through the slice,
    // below `code_len`, as is everything else.
    let at = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let load = at(pc);
    if !matches!(load, Some(0x2a..=0x2d)) || at(pc + 2) != Some(0xb4) {
        return false;
    }
    if at(pc + 1) == Some(0x59) {
        // The statement form, or (under `value`) the pre-increment's
        // duplicate before the `putfield`; then the post-increment's right
        // after the `getfield`, the operand one byte later.
        return super::increment_group_screen(
            code,
            code_len,
            pc + 3,
            pc + 5,
            0xb5,
            wide,
            value,
            ldc2w,
        ) || (value
            && matches!(at(pc + 5), Some(0x5a | 0x5d))
            && super::increment_group_screen(
                code,
                code_len,
                pc + 3,
                pc + 6,
                0xb5,
                wide,
                false,
                ldc2w,
            ));
    }
    // The `aload_N; aload_N` prefix takes no `ldc2_w` operand (the decoder
    // reads it, but the loop's no-dup screen never offers one).
    no_dup
        && at(pc + 1) == load
        && super::increment_group_screen(code, code_len, pc + 3, pc + 5, 0xb5, wide, false, ldc2w)
}

/// The store half of [`putfield_fast`], shared since interpreter round i1
/// wave 28 (lane L7) with [`ctor_field_stores_fast`]: write `val` (operand
/// stack kind `kind`) into the field at `fp` of the object at `recv`, with the
/// barriers `ZgcRealHeap::set_field` puts around the raw write, or return
/// `false` having written NOTHING when `val` is not already in the
/// representation the field's storage takes (the shapes `op_putfield`
/// coerces, which the slow handler owns).
///
/// `STORE = false` runs every one of those checks and writes nothing: a
/// caller that must make several stores all-or-nothing validates each first.
/// `putfield_fast` is the `STORE = true` instantiation, unchanged in what it
/// checks and writes and in what order (the checks of each arm still precede
/// its store); the operand-stack pops and the hit count stay with the caller.
///
/// `fp` must come from [`field_ptr_for`] for `recv` and `site`.
#[inline(always)]
fn put_field_value<const STORE: bool>(
    zgc: &ZgcRealHeap,
    fp: *mut u8,
    recv: u64,
    site: &FastFieldSite,
    val: CompactValue,
    kind: u8,
) -> bool {
    let Some(storage) = site.storage else {
        return put_legacy_value::<STORE>(zgc, fp, recv, site, val, kind);
    };
    // SAFETY (every raw store below): see `field_ptr_for`.
    match storage {
        FieldStorageKind::Int => {
            let Some(v) = val.as_int() else {
                return false;
            };
            if STORE {
                unsafe { store_u32(fp, v as u32) };
            }
        }
        FieldStorageKind::Boolean => {
            let Some(v) = val.as_int() else {
                return false;
            };
            if STORE {
                // SAFETY: `fp` came from `field_ptr_for`, and `Boolean` is
                // the layout's own 1-byte slot for this field.
                unsafe { store_u8(fp, (v & 1) as u8) };
            }
        }
        FieldStorageKind::Byte => {
            let Some(v) = val.as_int() else {
                return false;
            };
            if STORE {
                // SAFETY: as above; `Byte` is a 1-byte slot.
                unsafe { store_u8(fp, v as u8) };
            }
        }
        FieldStorageKind::Char | FieldStorageKind::Short => {
            let Some(v) = val.as_int() else {
                return false;
            };
            if STORE {
                // SAFETY: as above; both `Char` and `Short` are 2-byte slots.
                unsafe { store_u16(fp, v as u16) };
            }
        }
        FieldStorageKind::Float => {
            let Some(f) = val.as_float() else {
                return false;
            };
            if STORE {
                // SAFETY: as above; `Float` is a 4-byte slot, written as bits.
                unsafe { store_u32(fp, f.to_bits()) };
            }
        }
        FieldStorageKind::Long => {
            if kind != ValueStack::KIND_MARK_LONG {
                return false;
            }
            if STORE {
                // SAFETY: as above; `Long` is an 8-byte slot, and the stack
                // kind was just checked so `as_long_unchecked` is entitled.
                unsafe { store_u64(fp, val.as_long_unchecked() as u64) };
            }
        }
        FieldStorageKind::Double => {
            if kind != ValueStack::KIND_MARK_DOUBLE {
                return false;
            }
            if STORE {
                // SAFETY: as above; `Double` is an 8-byte slot, and the stack
                // kind was just checked.
                unsafe { store_u64(fp, val.raw_bits()) };
            }
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
            if STORE {
                if zgc.mark_active() {
                    // SAFETY: as above, on the `Reference` arm, so the slot is
                    // a reference slot. The pre-barrier needs the OLD value
                    // before the store overwrites it.
                    let old = unsafe { load_ref(fp) };
                    if old != 0 {
                        zgc.satb_pre_barrier(old as usize);
                    }
                }
                // SAFETY: as above, on the `Reference` arm; `raw` is either 0
                // or an object pointer taken from `as_object_ptr`, which is
                // what `store_ref`'s encode step requires.
                unsafe { store_ref(fp, raw) };
                // The generational card, after the store (`set_field`'s order).
                //
                // Only on this arm. A compact primitive slot can never hold a
                // reference, so a store into one cannot create the
                // old-to-young edge a card exists to name; carding it anyway
                // made every primitive `putfield` on an old object dirty that
                // object's card and sent the next young cycle to re-scan it
                // for nothing. The accessor `set_field` cards unconditionally
                // because it is handed a `Value` it does not type-check; this
                // arm knows the layout.
                zgc.note_ref_store(recv as usize);
            }
        }
    }
    true
}

/// The legacy half of [`put_field_value`]: decode the operand-stack slot the
/// way `op_putfield` does for this descriptor, then (with `STORE`) commit the
/// whole 16-byte cell with the same barrier order `ZgcRealHeap::set_field`
/// uses. Was `putfield_legacy`, which also popped the stack and counted the
/// hit; those moved to [`putfield_fast`] in wave 28.
#[inline]
fn put_legacy_value<const STORE: bool>(
    zgc: &ZgcRealHeap,
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
    if !STORE {
        return true;
    }
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
    // Card only a reference store -- see the compact `Reference` arm of
    // `put_field_value`. The descriptor decided `value`'s shape above, so a
    // `J`/`D`/sub-int store here cannot be a reference.
    if matches!(site.desc_byte, b'L' | b'[') {
        zgc.note_ref_store(recv as usize);
    }
    true
}

/// Most stores [`ctor_field_stores_fast`] makes for one constructor.
pub(super) const CTOR_FIELD_STORES_MAX: usize = 4;

/// The field stores of a constructor answered without a frame
/// (`invoke_fast::FIELD_CTOR_ELISION`, interpreter round i1 wave 28, lane
/// L7). Each `(cp index, value, kind)` of `stores` (`kind`: the operand-stack
/// kind mark, wave 115) is a `putfield` of the
/// constructor's own body -- `cp index` in the constant pool of `class_id`,
/// the constructor's declaring class -- storing `value` into the object at
/// `recv`: exactly the store the quickened `putfield` arm would make in the
/// constructor's frame, through the same site (`fast_field_sites`, keyed by
/// that class and index, so only a field the slow handler already resolved
/// for this thread qualifies) and the same [`put_field_value`].
///
/// All or nothing: every store is checked first -- site present, admitted as
/// `fast_put_admitted` admits a store from an `<init>` frame, receiver of the
/// site's layout, value in the field's representation -- and only then are
/// they made, in order. `false` means nothing was written, and the caller runs
/// the constructor with its frame. No JVMTI field watchpoint may be armed (the
/// full handler fires it). `census` is `VmConfig::site_census`.
pub(super) fn ctor_field_stores_fast(
    zgc: &ZgcRealHeap,
    sites: &mut FastFieldSiteCache,
    class_id: ClassId,
    recv: u64,
    stores: &[(u16, CompactValue, u8)],
    census: bool,
) -> bool {
    if stores.len() > CTOR_FIELD_STORES_MAX || crate::runtime::jvmti::any_field_watchpoint_active()
    {
        return false;
    }
    let mut planned: [Option<(*mut u8, FastFieldSite)>; CTOR_FIELD_STORES_MAX] =
        [None; CTOR_FIELD_STORES_MAX];
    for (slot, &(cp_index, val, kind)) in planned.iter_mut().zip(stores) {
        let site = match sites.get(class_id, cp_index) {
            Some(s) => *s,
            None => {
                site_stats::bump(site_stats::FAST_PUT_MISS);
                return false;
            }
        };
        // `fast_put_admitted` for a store made from an `<init>` frame: a final
        // field of this class may be stored, another class's never.
        if !matches!(
            site.final_put,
            FastFieldSite::PUT_ANY | FastFieldSite::PUT_INIT_ONLY
        ) {
            return false;
        }
        let Some(fp) = field_ptr_for(zgc, recv, &site) else {
            return false;
        };
        // `kind` is the operand-stack kind mark the value would carry in the
        // constructor's frame: 0 for a category-1 value, so a `long` /
        // `double` field declines here unless the caller marked the value
        // (interpreter round i6 wave 115, lane L7:
        // `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`; without it the caller
        // admits no `long` / `double` parameter or constant).
        if !put_field_value::<false>(zgc, fp, recv, &site, val, kind) {
            return false;
        }
        *slot = Some((fp, site));
    }
    for (slot, &(_, val, kind)) in planned.iter().zip(stores) {
        if let Some((fp, site)) = slot {
            let _ = put_field_value::<true>(zgc, *fp, recv, site, val, kind);
            site_stats::bump_if(census, site_stats::FAST_PUT_HIT);
        }
    }
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
    // SBR4 satb: never quicken `Reference.referent`. A site is keyed by
    // (class, constant-pool index), and `get()`'s `getfield` and `<init>`'s
    // `putfield` share one `Fieldref`, so a site filled by either would be hit
    // by the other; the quickened read has no SATB keep-alive, and the checked
    // arm (`op_getfield`) is the one that applies it.
    if field.field_index == 0
        && crate::memory::ref_discovery::is_referent_field(
            shared,
            field.declaring_class_id,
            field.field_index,
        )
    {
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
        match cratonvm_types::compact_object_field_disp(header, field.field_index) {
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
                (HEADER_SIZE + field.field_index * SLOT_SIZE, None)
            }
        };
    let (Ok(offset), Ok(field_index)) = (u32::try_from(offset), u32::try_from(field.field_index))
    else {
        site_stats::bump(site_stats::FAST_FIELD_UNUSABLE);
        report_unusable_once(header, field, "offset or index out of u32 range");
        return;
    };
    thread.site_tables.fast_field_sites.put(
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
            final_put: final_put_rule(field, current_class_id),
        },
    );
    site_stats::bump(if is_put {
        site_stats::FAST_PUT_FILL
    } else {
        site_stats::FAST_GET_FILL
    });
}

/// The [`FastFieldSite::final_put`] rule for a site of `current_class_id`
/// resolving to `field`: the fast-arm image of
/// `field_access::final_field_put_violation`, decided without the class-file
/// version (a same-class final is `PUT_INIT_ONLY` even below version 53, where
/// its other stores are legal and just take the full handler).
#[inline]
fn final_put_rule(field: &ResolvedField, current_class_id: ClassId) -> u8 {
    if !field.is_final {
        FastFieldSite::PUT_ANY
    } else if field.declaring_class_id == current_class_id {
        FastFieldSite::PUT_INIT_ONLY
    } else {
        FastFieldSite::PUT_NEVER
    }
}

/// Whether a quickened `putfield` from `frame` may use `site`. One byte
/// compare for a non-final field; the method-name compare only for a final one.
#[inline(always)]
fn fast_put_admitted(site: &FastFieldSite, frame: &Frame) -> bool {
    match site.final_put {
        FastFieldSite::PUT_ANY => true,
        FastFieldSite::PUT_INIT_ONLY => frame.method_name() == "<init>",
        _ => false,
    }
}

// ── Quickened getstatic ─────────────────────────────────────────────────────
//
// `docs/internal/fixed-bugs/interpreter-L4-proposal-static-field-quickening-FIXED-20260930.md`,
// stage 2. A `getstatic` site whose field has already been resolved AND whose
// declaring class is initialized is answered from `JvmThread::site_tables.static_field_sites`
// with the site lookup, one statics read and the push: no staticness check, no
// `java/lang/System` screen, no class-initialization probe (a thread-local
// hash-set probe per access), no `Boolean` repair screen.
//
// Admission (all decided at FILL, all immutable for the entry's life):
//
// * the site is admissible to `field_sites` and holds a valid entry for the
//   same key — so loader-sensitive sites are excluded exactly as there, and
//   the entry is epoch-validated and redefinition-latched by the same
//   `SiteCache` (class unloading bumps the definition epoch, so it cannot
//   strand an entry either);
// * the declaring class is `Initialized` (`is_class_initialized_via_manager`),
//   not merely "`ensure_class_initialized_shared` returned Ok", which it also
//   does for a class the current thread is still initializing;
// * the field is static, not a `java/lang/System` field, and not a reference
//   while `op_getstatic`'s `Boolean` repair could apply (the caller decides
//   that last one, with the predicate it owns).
//
// The hit reads through `get_static_shared`, so `CRATONVM_NO_STATICS_INDEX`,
// block republication and an unpublished (unloaded) class all keep their
// meaning, and pushes through `push_static_field_value`, so the value path is
// byte-identical to the slow handler's. Any JVMTI field watchpoint sends every
// access to the full handler, which fires it.
//
// Wave 5 (i1, lane L4): volatile sites are admitted — the hit issues the same
// `SeqCst` fence after the read that `op_getstatic` does — and `putstatic`
// shares the table (`putstatic_site_hit`). One entry per `(class, cp index)`
// serves both opcodes, which is sound because the entry is the resolved
// field, identical for both, and both fill under the same admission rule; the
// one `putstatic`-only check that the entry does not imply, the final-field
// rule, is made at the `putstatic` hit (`is_final` declines; a final static
// is legally stored only from `<clinit>`, which never fills).
//
// Engagement counters: `CRATONVM_DBG_GETSTATIC_PROF=1`
// (`crate::vm::note_interp_static_site`, printed by `gs_prof::dump`).
//
// Kill switches: `CRATONVM_JIT_NO_FIELD_FAST_PATH=1` (the quickened field arms'
// switch) and `CRATONVM_JIT_FIELD_SITE_CACHE=0` (no `field_sites` entry, so no
// fill) — no flag of its own.

/// Quickened `getstatic`: push the field value and return `Ok(true)`, or
/// return `Ok(false)` with nothing done so the caller runs `op_getstatic`.
#[inline]
pub(super) fn getstatic_site_hit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
) -> Result<bool, crate::error::MethodCallFailed> {
    if crate::runtime::env_cache::no_field_fast_path()
        || crate::runtime::jvmti::any_field_watchpoint_active()
    {
        return Ok(false);
    }
    let current_class_id = thread.frames[frame_idx].class_id;
    let Some(field) = thread
        .site_tables
        .static_field_sites
        .get(current_class_id, cp_index)
        .cloned()
    else {
        return Ok(false);
    };
    let value = crate::vm::get_static_shared(shared, field.declaring_class_id, field.field_index);
    if field.is_volatile {
        std::sync::atomic::fence(Ordering::SeqCst);
    }
    super::field_access::push_static_field_value(
        &mut thread.frames[frame_idx].stack,
        value,
        field.is_reference,
        Some(field.desc_byte),
    )?;
    crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_GET_HIT);
    Ok(true)
}

/// Quickened `putstatic`: pop and store the value and return `Ok(true)`, or
/// return `Ok(false)` with nothing done so the caller runs `op_putstatic`.
/// An `Err` is the pop's own, raised before it consumed anything (i66-L7):
/// the one `op_putstatic` would raise over the same stack.
///
/// Same table and admission as [`getstatic_site_hit`]; a filled site proves
/// the field resolved, is static and its class is `Initialized`. What the hit
/// still does is exactly `op_putstatic`'s tail: the descriptor-aware pop,
/// the fences around a volatile store, and `set_static_shared` (which takes
/// the SATB pre-barrier itself).
#[inline]
pub(super) fn putstatic_site_hit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
) -> Result<bool, crate::error::MethodCallFailed> {
    if crate::runtime::env_cache::no_field_fast_path()
        || crate::runtime::jvmti::any_field_watchpoint_active()
    {
        return Ok(false);
    }
    let current_class_id = thread.frames[frame_idx].class_id;
    let Some(field) = thread
        .site_tables
        .static_field_sites
        .get(current_class_id, cp_index)
        .cloned()
    else {
        return Ok(false);
    };
    // JVMS §6.5 putstatic: a final field only from its own `<clinit>`, which
    // `check_final_field_put` decides on the full path.
    if field.is_final {
        return Ok(false);
    }
    let value = super::field_access::pop_static_field_value(
        &mut thread.frames[frame_idx].stack,
        Some(field.desc_byte),
    )?;
    if field.is_volatile {
        std::sync::atomic::fence(Ordering::SeqCst);
    }
    crate::vm::set_static_shared(shared, field.declaring_class_id, field.field_index, value);
    if field.is_volatile {
        std::sync::atomic::fence(Ordering::SeqCst);
    }
    crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_PUT_HIT);
    Ok(true)
}

/// Offer a `getstatic` / `putstatic` site the slow handler just served to
/// `JvmThread::site_tables.static_field_sites`. `op_getstatic` calls it on its ordinary
/// (non-`System`) arm after the push, `op_putstatic` after the store, each
/// only when `op_getstatic`'s `Boolean` repair cannot apply to `field` (the
/// entry serves both opcodes); see the section comment above for the rest of
/// the admission rule.
pub(super) fn fill_static_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    field: &ResolvedField,
) {
    if !field.is_static
        || crate::runtime::env_cache::no_field_fast_path()
        || super::opcodes::static_may_be_system(shared, field.declaring_class_id)
    {
        return;
    }
    let frame = &thread.frames[frame_idx];
    // A `<clinit>` reading statics is almost always its own class's, which is
    // not `Initialized` yet: the initialization probe below would miss its
    // memo and take the `class_manager` lock on every such access (a table
    // built in a static-initializer loop). Those sites fill after `<clinit>`.
    if frame.method_name() == "<clinit>" {
        return;
    }
    let current_class_id = frame.class_id;
    // Read BEFORE the checks below; see `SiteCache::put`.
    let epochs_at_entry = FastFieldSiteCache::epochs_for(shared);
    // Same key, same field, valid under the current epochs: the site is one
    // `field_sites` admits, i.e. not loader-sensitive.
    let admitted = thread
        .field_sites
        .get(current_class_id, cp_index)
        .is_some_and(|f| {
            f.declaring_class_id == field.declaring_class_id && f.field_index == field.field_index
        });
    if !admitted {
        return;
    }
    if !crate::vm::is_class_initialized_via_manager(shared, field.declaring_class_id) {
        return;
    }
    thread.site_tables.static_field_sites.put(
        current_class_id,
        cp_index,
        epochs_at_entry,
        field.clone(),
    );
    crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_FILL);
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
/// handler entry, before any resolution. `shared`'s own counters (wave 20).
#[inline]
pub(super) fn fill_epochs(shared: &SharedVm) -> (u64, u64) {
    FieldSiteCache::epochs_for(shared)
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
    if registry_probe_restored() && zgc.is_object_address(base).is_none() {
        return None;
    }
    // SAFETY: `arr` is the array reference of a verified `*aload` / `*astore`,
    // popped from the operand stack in this same safepoint-free window; its
    // first `HEADER_SIZE` bytes are a live header. The `kind()`,
    // `element_type()` and `array_length()` tests below are what establish the
    // element address is in bounds. See `registry_probe_restored`.
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

/// Quickened `aaload`.
///
/// Pushes the reference element and returns `true`, or leaves the stack
/// untouched and returns `false` so the caller keeps `VmHeap::get_array_element`
/// (which owns every exception and every cold decode).
///
/// # Why this exists
///
/// `aaload` is in the dispatch loop's `0x2e..=0x35` arm, but that arm's
/// quickened half declines it: `prim_elem_for_opcode` has no reference entry,
/// so until now every `aaload` fell through to the full path. Measured
/// 2026-09-05 on `probes/ArrBurn.java`, 30 M iterations: `aaload` 99.9 ns per
/// iteration against `iaload`'s 79.0 in the identical loop shape — **21 ns**,
/// where HotSpot has the two identical to within noise.
///
/// The first attempt at this row blamed the autobox screen
/// (`get_array_element` called `autobox_payload`, whose first act is
/// `is_object_address`, unlatched). Latching it was correct and is kept, but
/// it measured **nothing** — 3/8 pairwise, 20 ms in 2960. The cost is not one
/// probe, it is the whole slow path: a `VmHeap` enum `dispatch!`, the
/// collector's own header and bounds re-reads, an `Acquire` load for the
/// barrier arm, `read_prim_element`'s match, and a 16-byte `Value` returned to
/// be re-encoded into an 8-byte stack slot.
///
/// # What it declines, and why each one has to
///
/// * **`load_barrier_armed()`** — the barrier arm of `get_array_element`
///   self-heals the slot through `load_barrier_slot` and has different
///   semantics from a plain read. Nothing in a default run arms it, but when
///   something does, this arm must not be the one answering.
/// * **`autobox::wrapper_exists()`** — a wrapper in a reference slot must be
///   un-boxed, which is the work the latch beside this screens for. Same
///   screen, same reason as the compact reference-field arm above.
/// * **A word that is neither zero nor a plausible heap pointer** — that is
///   `ref_element_word_implausible`'s population, and it is not one answer but
///   three: a well-formed ZGC colored word (a hard failure naming the missing
///   barrier), stale garbage (a counted degrade to null), and the counter that
///   separates them. Reproducing any of that here would be a second copy of a
///   diagnosis; declining keeps it in one place.
///
/// `raw == 0` is handled rather than declined, because an ordinary null
/// element is the common case and `decode_ref_element_word`'s cold arm returns
/// `Object(None)` for it without counting anything.
///
/// `ref_fast` is the dispatch loop's hoisted `ref_array_fast_on`
/// (`CRATONVM_JIT_NO_REF_ARRAY_FAST` unset; re-read here on every call until
/// interpreter round i2 wave 60, lane L7) and `census` its `site_census`.
#[inline]
pub(super) fn array_load_ref(
    zgc: &ZgcRealHeap,
    stack: &mut ValueStack,
    arr: ObjectRef,
    index: i32,
    ref_fast: bool,
    census: bool,
) -> bool {
    if !ref_fast {
        return false;
    }
    if index < 0 {
        site_stats::bump(site_stats::REFARR_MISS_SHAPE);
        return false;
    }
    // Named separately: both are process-wide latches, but one is armed by a
    // collection cycle and the other by anything that ever boxed a primitive
    // into a reference slot. A census that says only "screen" cannot tell a
    // reader which, and the fixes have nothing in common.
    if zgc.load_barrier_armed() {
        site_stats::bump(site_stats::REFARR_MISS_BARRIER);
        return false;
    }
    // NOT `autobox::wrapper_exists()`. That latch is armed at bootstrap by the
    // class-mirror populator, so it is true in every process and a screen keyed
    // on it is permanently closed — measured `aaload: hit=0 miss_barrier=0
    // miss_wrapper=2000146` before this. The precise question is asked below,
    // on the element this access actually loaded, for one header compare.
    let base = arr.as_ptr() as usize;
    // SAFETY: `arr` is the array reference of a verified `aaload`, popped from
    // the operand stack in this same safepoint-free window; its first
    // `HEADER_SIZE` bytes are a live header. See `registry_probe_restored`.
    let header = unsafe { &*(base as *const ObjectHeader) };
    if header.kind() != ObjectKind::Array || header.element_type() != ArrayElementType::Reference {
        site_stats::bump(site_stats::REFARR_MISS_SHAPE);
        return false;
    }
    let index = index as usize;
    if index >= header.array_length() as usize {
        site_stats::bump(site_stats::REFARR_MISS_SHAPE);
        return false;
    }
    let Some(offset) = index.checked_mul(cratonvm_types::narrow_oop::ref_element_size()) else {
        site_stats::bump(site_stats::REFARR_MISS_SHAPE);
        return false;
    };
    // SAFETY: `index < length` and the stride is the one `read_prim_element`
    // uses for a reference element, so the slot lies inside the array body.
    let raw = unsafe {
        cratonvm_types::narrow_oop::read_ref_slot(
            (base as *const u8).add(cratonvm_types::ARRAY_DATA_OFFSET + offset),
        )
    };
    let cv = if raw == 0 {
        CompactValue::null()
    } else if cratonvm_types::plausible_heap_pointer(raw) {
        // SAFETY: a plausible, non-zero element word of a live reference array
        // addresses an object whose first `HEADER_SIZE` bytes are a header —
        // the same premise `autobox_payload` reads on.
        let elem_header = unsafe { &*(raw as usize as *const ObjectHeader) };
        if cratonvm_gc::autobox::header_is_wrapper(elem_header) {
            // A wrapper must be un-boxed, which is `get_array_element`'s job.
            site_stats::bump(site_stats::REFARR_MISS_WRAPPER);
            return false;
        }
        match CompactValue::try_from_pointer(raw) {
            Some(cv) => cv,
            None => {
                site_stats::bump(site_stats::REFARR_MISS_WORD);
                return false;
            }
        }
    } else {
        // The three-way cold decode above; not reproduced here.
        site_stats::bump(site_stats::REFARR_MISS_WORD);
        return false;
    };
    stack.push_compact(cv);
    site_stats::bump_if(census, site_stats::REFARR_HIT);
    true
}

/// The element read of the fused `aload_N; iload_M; <x>aload` (interpreter
/// round i1 wave 24, lane L7): [`array_load_prim`], then [`array_load_ref`]
/// for an `aaload` -- the unfused `*aload` arm's readers, so its admission --
/// pushing the element and answering `true`, or leaving the stack untouched.
///
/// Out of line (wave 26, lane L7): the two readers are large (the reference
/// one carries its screens and census bumps), and inlined at every fusion
/// site they multiplied inside `execute_frame_from_index`, whose code size and
/// register pressure every arm pays for. The unfused `*aload` arm keeps its
/// own inlined copy; a fused group, which saves two dispatches, pays one
/// call.
#[inline(never)]
pub(super) fn array_load_fused(
    zgc: &ZgcRealHeap,
    stack: &mut ValueStack,
    arr: ObjectRef,
    index: i32,
    opcode: u8,
    ref_fast: bool,
    census: bool,
) -> bool {
    array_load_prim(zgc, stack, arr, index, opcode)
        || (opcode == 0x32 && array_load_ref(zgc, stack, arr, index, ref_fast, census))
}

/// The `*aload` of the fused `iload{_N, n}; <x>aload` (interpreter round i1
/// wave 25, lane L7): the array is already on top of the operand stack (an
/// `aaload` of the outer dimension, a `getfield`, an `aload`), the index is the
/// int `index` read straight out of the `iload`'s local. Replaces the array
/// slot with the element and returns `true`, or leaves the stack exactly as it
/// was -- the array slot restored bit- and kind-exact, as the `*aload` arm's
/// own decline does -- and returns `false`, so the caller runs the plain
/// `iload` and the `*aload` raises its NPE / AIOOBE at its own pc. The
/// readers are the `*aload` arm's (`array_load_prim`, `array_load_ref`), so
/// the admission is the unfused arm's.
///
/// Out of line (interpreter round i1 wave 26, lane L7), like
/// [`array_load_fused`]: both readers inline into it once, instead of into
/// both `iload` fusion sites of the dispatch loop.
#[inline(never)]
pub(super) fn array_load_top_with_index(
    zgc: &ZgcRealHeap,
    stack: &mut ValueStack,
    index: i32,
    opcode: u8,
    ref_fast: bool,
    census: bool,
) -> bool {
    if index < 0 || stack.is_empty() {
        return false;
    }
    let (arr_cv, arr_kind) = stack.peek_with_kind_at(0);
    let Some(aptr) = arr_cv.as_object_ptr().filter(|&p| p != 0) else {
        return false;
    };
    // SAFETY: a non-null `Object`-tagged operand-stack slot holds a heap
    // address; both readers re-validate the header before reading.
    let arr = unsafe { ObjectRef::from_raw(aptr as *mut u8) };
    stack.pop_compact();
    if array_load_prim(zgc, stack, arr, index, opcode)
        || (opcode == 0x32 && array_load_ref(zgc, stack, arr, index, ref_fast, census))
    {
        return true;
    }
    stack.push_with_kind_unchecked(arr_cv, arr_kind);
    false
}

/// The element read `a[i]` with a local past 3
/// (`CRATONVM_INTERP_FUSED_WIDE_LOCALS`, `VmConfig::interp_fused_wide_locals`;
/// interpreter round i3 wave 75, lane L7; proposal
/// `interpreter-i72-L7-proposal-wide-local-forms-of-the-fused-groups-KEPT-OFF-20261019`): the
/// array is local `array_local` (the caller's `aload_N` or `aload n`), and
/// at `iload_pc` of `frame`'s code (unpadded length `code_len`; every byte
/// read through the slice, below it) an `iload n` or `iload_M`, then an
/// `*aload`. Both locals are read straight out of their slots (an
/// `Object`-tagged non-null array, an int index, both below `max_locals`),
/// the element through the `*aload` arm's own readers ([`array_load_prim`],
/// [`array_load_ref`] for `aaload`), as [`array_load_fused`] reads it for
/// the short-local group. Pushes the element and returns the pc past the
/// `*aload`; `None` having pushed nothing for any other shape, a null /
/// non-array / other-type array, a non-int index and an index out of range,
/// so the caller runs the plain load and the `*aload` raises its NPE /
/// AIOOBE at its own pc.
///
/// The shapes javac writes for them: the for-each loop's `v = arr$[i$]`
/// (`aload_N; iload n; <x>aload`: the hidden copy, length and index take
/// three locals after the method's own, so the index is at 4 or above in any
/// method with a receiver and one parameter, or two parameters), and `s +=
/// b[i]` in a method with four or more locals before the loop (`aload n;
/// iload n; <x>aload`). Without it each was two dispatches (the `aload`, then
/// the wave-25 `iload n; <x>aload` pair). The caller has tested
/// `fusion_off`, the switch and the byte after its own load. Out of line
/// through [`array_load_wide_locals_at`], the dispatch loop's one entry.
#[inline(always)]
pub(super) fn array_load_wide_locals(
    zgc: &ZgcRealHeap,
    frame: &mut Frame,
    code_len: usize,
    iload_pc: usize,
    array_local: usize,
    ref_fast: bool,
    census: bool,
) -> Option<usize> {
    let at = |i: usize| {
        if i < code_len {
            frame.code.get(i).copied()
        } else {
            None
        }
    };
    let (index_local, load_pc) = match at(iload_pc)? {
        0x15 => (usize::from(at(iload_pc + 1)?), iload_pc + 2),
        op @ 0x1a..=0x1d => (usize::from(op - 0x1a), iload_pc + 1),
        _ => return None,
    };
    let load = at(load_pc)?;
    if !matches!(load, 0x2e..=0x35) {
        return None;
    }
    let max_locals = usize::from(frame.max_locals);
    if array_local >= max_locals || index_local >= max_locals {
        return None;
    }
    let aptr = frame
        .get_local_compact_unchecked(array_local)
        .as_object_ptr()?;
    let index = frame.get_local_compact_unchecked(index_local).as_int()?;
    if aptr == 0 || index < 0 {
        return None;
    }
    // SAFETY: a non-null `Object`-tagged local slot holds a heap address, the
    // bits `aload` would push; both readers re-validate the header before
    // reading.
    let arr = unsafe { ObjectRef::from_raw(aptr as *mut u8) };
    let pushed = array_load_prim(zgc, &mut frame.stack, arr, index, load)
        || (load == 0x32 && array_load_ref(zgc, &mut frame.stack, arr, index, ref_fast, census));
    pushed.then_some(load_pc + 1)
}

/// The dispatch loop's entry to [`array_load_wide_locals`] (round i3 wave 76,
/// lane L7; both call sites, the `aload_N` and `aload n` arms): six
/// arguments, all in registers, where the wave-75 sites passed seven (the
/// census bool went on the stack) and kept the hoisted `ref_array_fast_on`
/// and `site_census` live at the arm. The heap option is tested here, not at
/// the arm. `CRATONVM_JIT_NO_REF_ARRAY_FAST`'s memo is read per call (the
/// hoisted bool's own source; only an `aaload` hit uses it) and the census
/// gate from `shared.config`, so the answer is the wave-75 one.
#[inline(never)]
pub(super) fn array_load_wide_locals_at(
    shared: &SharedVm,
    zgc: Option<&ZgcRealHeap>,
    frame: &mut Frame,
    code_len: usize,
    iload_pc: usize,
    array_local: usize,
) -> Option<usize> {
    array_load_wide_locals(
        zgc?,
        frame,
        code_len,
        iload_pc,
        array_local,
        !crate::runtime::env_cache::no_ref_array_fast(),
        shared.config.site_census,
    )
}

// ── Fused array element increment (round i3 wave 72, lane L7) ────────────

/// The fused array element increment's group, decoded at `p` (the `dup2`) of
/// `code` (unpadded length `code_len`; every byte read through the slice,
/// below it) for [`fused_array_increment_local`] and
/// [`fused_array_increment_top`]:
///
/// ```text
/// dup2
/// <x>aload                   iaload | laload | baload | caload | saload
/// [dup_x2 | dup2_x2]         post-increment value form (under `value`)
/// <operand>                  iconst_* | bipush | sipush | iload[_N]
///                            (lconst_0/1 | lload[_N] for laload)
/// iadd | isub | imul         (ladd | lsub | lmul | land | lor | lxor)
///   | iand | ior | ixor
/// [i2b | i2c | i2s]          exactly baload's / caload's / saload's own
/// [dup_x2 | dup2_x2]         pre-increment value form (under `value`)
/// <x>astore                  the load's own store (load + 0x21)
/// ```
///
/// `(the *aload opcode, operand, the arithmetic opcode, pc past the
/// *astore, use)`, or
/// `None` for any other shape. The duplicate's width is the group's (`dup_x2`
/// over an int element, `dup2_x2` over a long); one duplicate at most;
/// without `value` a group carrying one is refused. The operand's local, if
/// any, is read from `frame` by `super::static_increment_operand` /
/// `super::static_increment_long_operand` (an int-tagged / long-marked slot
/// below `max_locals`); no accepted operand stores a local, so the index
/// read before the group is the one every `*aload` / `*astore` of it uses.
/// `faload` / `daload` (no float arithmetic is fused), `aaload` and a
/// conversion that is not the element's own (or one under `iaload`) refuse.
// The production callers take a `mode` since wave 74 (`decode_array_increment_mode`).
#[cfg_attr(not(test), allow(dead_code))]
#[inline(always)]
#[allow(clippy::type_complexity)]
fn decode_array_increment(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    p: usize,
    value: bool,
) -> Option<(u8, super::IncrementOperand, u8, usize, FieldIncrementUse)> {
    decode_array_increment_mode(frame, code, code_len, p, array_increment_mode(value, false))
}

/// [`decode_array_increment_mode`]'s `mode` bit: the value forms
/// (`VmConfig::interp_fused_array_increment_value`).
pub(super) const ARRAY_INC_VALUE: u8 = 1;
/// [`decode_array_increment_mode`]'s `mode` bit: the shifts and divisions
/// (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SHIFT_DIV`,
/// `VmConfig::interp_fused_array_increment_shift_div`; round i3 wave 74,
/// lane L7).
pub(super) const ARRAY_INC_SHIFT_DIV: u8 = 2;
/// [`decode_array_increment_mode`]'s `mode` bit: `p` is the `*aload` of the
/// group written without a `dup2` (`a[j] = a[j] + k`,
/// `CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP`; round i3 wave 74, lane L7),
/// not a `dup2`. Set by the local entry only, after it has matched the
/// repeated `aload_N; iload_M` pair.
pub(super) const ARRAY_INC_NO_DUP: u8 = 4;

/// The `mode` the array increment's decoder and body take, from the two
/// per-VM switches every entry reads ([`ARRAY_INC_NO_DUP`] is the local
/// entry's own).
#[inline(always)]
pub(super) fn array_increment_mode(value: bool, shift_div: bool) -> u8 {
    (if value { ARRAY_INC_VALUE } else { 0 }) | (if shift_div { ARRAY_INC_SHIFT_DIV } else { 0 })
}

/// [`decode_array_increment_group`]'s `mode` bit: the operand may be another
/// array element, `aload_X; iload_Y; <x>aload` with X and Y in 0..3 and the
/// load of the group's width (`a[j] += b[k]`, `a[j] = a[j] + b[j]`;
/// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND`,
/// `VmConfig::interp_fused_array_increment_array_operand`; round i3 wave 76,
/// lane L7).
pub(super) const ARRAY_INC_ARRAY_OPERAND: u8 = 8;
/// [`decode_array_increment_group`]'s `mode` bit: the operand may be a
/// `getstatic #F` of the group's width (`a[j] += SF`;
/// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND`,
/// `VmConfig::interp_fused_array_increment_static_operand`; round i3 wave 76,
/// lane L7).
pub(super) const ARRAY_INC_STATIC_OPERAND: u8 = 16;
/// [`decode_array_increment_group`]'s `mode` bit, set only together with
/// [`ARRAY_INC_ARRAY_OPERAND`]: the element operand's array may also be
/// `aload n` and its index `iload n` (locals past 3: a method with a
/// receiver and parameters, a for-each body), and the index may be
/// `iload{_Y, n}; <int constant>; iadd | isub` (`p[i] += p[i - 1]`,
/// `h[k] += w[k + 2]`), computed with `iadd`'s wrapping
/// (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND_WIDE`,
/// `VmConfig::interp_fused_array_increment_array_operand_wide`; round i3
/// wave 77, lane L7).
pub(super) const ARRAY_INC_ARRAY_OPERAND_WIDE: u8 = 32;
/// [`decode_array_increment_group`]'s `mode` bit, meaningful only together
/// with [`ARRAY_INC_STATIC_OPERAND`]: an OWNED frame running `<clinit>`
/// refuses the static operand on its bytes
/// (`CRATONVM_INTERP_FUSED_ARRAY_STATIC_OPERAND_NO_CLINIT`,
/// `VmConfig::interp_fused_array_static_operand_no_clinit`; interpreter round
/// i4 wave 94, lane L7;
/// `i77-L7-proposal-a-clinit-frame-skips-the-array-increment-static-operand`).
/// `fill_static_site` never fills a site from a `<clinit>` frame, so there the
/// static operand misses on every execution after the whole decode and the
/// element validation, and an owned frame has no decline memo to retire the
/// pc.
pub(super) const ARRAY_INC_STATIC_NO_CLINIT: u8 = 64;

/// The `mode` every production entry hands its body: the two wave-74 bits
/// of [`array_increment_mode`] plus the wave-76 operand bits, all read from
/// `shared.config` (fixed for the VM's life).
#[inline(always)]
fn array_increment_mode_of(shared: &SharedVm) -> u8 {
    let c = &shared.config;
    let array_operand = if c.interp_fused_array_increment_array_operand {
        ARRAY_INC_ARRAY_OPERAND
    } else {
        0
    };
    let static_operand = if c.interp_fused_array_increment_static_operand {
        ARRAY_INC_STATIC_OPERAND
    } else {
        0
    };
    // Wave 77 (lane L7): the wide / index-expression element forms ride on
    // the element operand's own switch.
    let array_operand_wide = if c.interp_fused_array_increment_array_operand
        && c.interp_fused_array_increment_array_operand_wide
    {
        ARRAY_INC_ARRAY_OPERAND_WIDE
    } else {
        0
    };
    // Wave 94 (lane L7): the `<clinit>` refusal rides on the static operand.
    let static_no_clinit = if c.interp_fused_array_increment_static_operand
        && c.interp_fused_array_static_operand_no_clinit
    {
        ARRAY_INC_STATIC_NO_CLINIT
    } else {
        0
    };
    array_increment_mode(
        c.interp_fused_array_increment_value,
        c.interp_fused_array_increment_shift_div,
    ) | array_operand
        | static_operand
        | array_operand_wide
        | static_no_clinit
}

/// The operand of a fused array increment as its decoder leaves it (round i3
/// wave 76, lane L7): a value already read from the bytes or a local
/// (`Ready`, every shape before wave 76), or one the body reads after it has
/// validated the group's own element: another array element (`Element`,
/// under [`ARRAY_INC_ARRAY_OPERAND`]) or a quickened static (`Static`, under
/// [`ARRAY_INC_STATIC_OPERAND`]). The decoder still reads no heap and no
/// site cache, so a refusal of its own stays a function of the bytes (the
/// decline memo's rule); a failure to read a deferred operand is a
/// per-execution decline the memo never records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArrayIncOperand {
    Ready(super::IncrementOperand),
    Element {
        array_local: usize,
        index_local: usize,
        load: u8,
    },
    /// `local[array_local][local[index_local] + delta]` (round i3 wave 77,
    /// lane L7, under [`ARRAY_INC_ARRAY_OPERAND_WIDE`]): the index
    /// expression `iload; <int constant>; iadd | isub`, `isub` decoded as
    /// the constant's wrapping negation (the same 32-bit result).
    ElementAt {
        array_local: usize,
        index_local: usize,
        delta: i32,
        load: u8,
    },
    Static {
        cp_index: u16,
    },
}

/// The deferred operand forms at `q` (round i3 wave 76, lane L7): `aload_X;
/// iload_Y; <x>aload` (X, Y below `max_locals`; the load `iaload` /
/// `baload` / `caload` / `saload` for an int group, `laload` for a long one)
/// under [`ARRAY_INC_ARRAY_OPERAND`], `getstatic #F` under
/// [`ARRAY_INC_STATIC_OPERAND`]; with the pc after it. `None` for anything
/// else, and for both forms when their bit is clear.
#[inline(always)]
fn decode_array_increment_deferred_operand(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    q: usize,
    long_group: bool,
    mode: u8,
) -> Option<(ArrayIncOperand, usize)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    match byte(q)? {
        a @ 0x2a..=0x2d if mode & ARRAY_INC_ARRAY_OPERAND != 0 => {
            let (i, load) = (byte(q + 1)?, byte(q + 2)?);
            let load_ok = if long_group {
                load == 0x2f
            } else {
                matches!(load, 0x2e | 0x33..=0x35)
            };
            if !matches!(i, 0x1a..=0x1d) || !load_ok {
                // Round i3 wave 77 (lane L7): a wide index or an index
                // expression after a short array local.
                return if mode & ARRAY_INC_ARRAY_OPERAND_WIDE != 0 {
                    decode_array_increment_wide_element(
                        frame,
                        code,
                        code_len,
                        usize::from(a - 0x2a),
                        q + 1,
                        long_group,
                    )
                } else {
                    None
                };
            }
            let (array_local, index_local) = (usize::from(a - 0x2a), usize::from(i - 0x1a));
            let max_locals = usize::from(frame.max_locals);
            if array_local >= max_locals || index_local >= max_locals {
                return None;
            }
            Some((
                ArrayIncOperand::Element {
                    array_local,
                    index_local,
                    load,
                },
                q + 3,
            ))
        }
        // Round i3 wave 77 (lane L7): `aload n` as the operand's array.
        0x19 if mode & (ARRAY_INC_ARRAY_OPERAND | ARRAY_INC_ARRAY_OPERAND_WIDE)
            == ARRAY_INC_ARRAY_OPERAND | ARRAY_INC_ARRAY_OPERAND_WIDE =>
        {
            decode_array_increment_wide_element(
                frame,
                code,
                code_len,
                usize::from(byte(q + 1)?),
                q + 2,
                long_group,
            )
        }
        0xb2 if mode & ARRAY_INC_STATIC_OPERAND != 0 => {
            // Wave 94 (lane L7, [`ARRAY_INC_STATIC_NO_CLINIT`]): one pointer
            // test for a cached frame (every frame an invoke pushes), the name
            // compare only for an owned one. A function of the method, so a
            // shape decline the memo may record.
            if mode & ARRAY_INC_STATIC_NO_CLINIT != 0
                && frame.cached_method().is_none()
                && frame.method_name() == "<clinit>"
            {
                return None;
            }
            let (hi, lo) = (byte(q + 1)?, byte(q + 2)?);
            Some((
                ArrayIncOperand::Static {
                    cp_index: (u16::from(hi) << 8) | u16::from(lo),
                },
                q + 3,
            ))
        }
        _ => None,
    }
}

/// The element operand's index and load after its array local (round i3
/// wave 77, lane L7, [`ARRAY_INC_ARRAY_OPERAND_WIDE`]): at `r` an `iload_Y`
/// or `iload n`, optionally `<int constant>; iadd | isub` (the constant
/// `iconst_m1..5`, `bipush` or `sipush`), then the `*aload` of the group's
/// width (`long_group`: `laload`; else `iaload` / `baload` / `caload` /
/// `saload`). Both locals must be below `max_locals`. A function of the bytes
/// alone (and `max_locals`), so a refusal here is a shape decline the memo
/// may record. `Element` without an index expression (the same operand the
/// short form decodes), `ElementAt` with one; with the pc after the load.
#[inline(always)]
fn decode_array_increment_wide_element(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    array_local: usize,
    r: usize,
    long_group: bool,
) -> Option<(ArrayIncOperand, usize)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let (index_local, mut r) = match byte(r)? {
        i @ 0x1a..=0x1d => (usize::from(i - 0x1a), r + 1),
        0x15 => (usize::from(byte(r + 1)?), r + 2),
        _ => return None,
    };
    // `<int constant>; iadd | isub`: `isub` of `k` is `iadd` of `-k`
    // (wrapping, the same 32 bits for every `k`, `MIN_VALUE` included).
    let constant = match byte(r)? {
        c @ 0x02..=0x08 => Some((i32::from(c) - 3, r + 1)),
        // Cast: bytecode operand decoding (signed byte)
        0x10 => Some((i32::from(byte(r + 1)? as i8), r + 2)),
        0x11 => {
            let (hi, lo) = (byte(r + 1)?, byte(r + 2)?);
            // Cast: bytecode operand decoding (signed short)
            Some((
                i32::from(((u16::from(hi) << 8) | u16::from(lo)) as i16),
                r + 3,
            ))
        }
        _ => None,
    };
    let delta = match constant {
        Some((k, n)) => {
            let delta = match byte(n)? {
                0x60 => k,
                0x64 => k.wrapping_neg(),
                _ => return None,
            };
            r = n + 1;
            Some(delta)
        }
        None => None,
    };
    let load = byte(r)?;
    let load_ok = if long_group {
        load == 0x2f
    } else {
        matches!(load, 0x2e | 0x33..=0x35)
    };
    let max_locals = usize::from(frame.max_locals);
    if !load_ok || array_local >= max_locals || index_local >= max_locals {
        return None;
    }
    let operand = match delta {
        None => ArrayIncOperand::Element {
            array_local,
            index_local,
            load,
        },
        Some(delta) => ArrayIncOperand::ElementAt {
            array_local,
            index_local,
            delta,
            load,
        },
    };
    Some((operand, r + 1))
}

/// [`decode_array_increment`] under `mode` (round i3 wave 74, lane L7):
/// [`ARRAY_INC_VALUE`] is its `value`; under [`ARRAY_INC_SHIFT_DIV`] the
/// arithmetic may also be `idiv` / `irem` / `ishl` / `ishr` / `iushr` (an
/// int group) or `ldiv` / `lrem` (a long group, long operand) or `lshl` /
/// `lshr` / `lushr` (a long group whose operand is an int: the shift
/// count); under [`ARRAY_INC_NO_DUP`] `p` is the group's `*aload` (the
/// `dup2`-less update, whose operand stack from the `*aload` on is the
/// `dup2` form's, so the same tail is decoded, value forms included). The
/// result's operand of a long shift is its int count widened to `Long`;
/// the arithmetic opcode tells the body which it is.
///
/// Since wave 76 (lane L7) a view of [`decode_array_increment_group`] that
/// answers `None` for a deferred operand; the body decodes through the group
/// decoder, the tests through this one.
#[cfg_attr(not(test), allow(dead_code))]
#[inline(always)]
#[allow(clippy::type_complexity)]
fn decode_array_increment_mode(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    p: usize,
    mode: u8,
) -> Option<(u8, super::IncrementOperand, u8, usize, FieldIncrementUse)> {
    match decode_array_increment_group(frame, code, code_len, p, mode)? {
        (load, ArrayIncOperand::Ready(operand), arith, next_pc, used) => {
            Some((load, operand, arith, next_pc, used))
        }
        _ => None,
    }
}

/// [`decode_array_increment_mode`] with the deferred operands (round i3 wave
/// 76, lane L7): under [`ARRAY_INC_ARRAY_OPERAND`] /
/// [`ARRAY_INC_STATIC_OPERAND`] an operand none of the byte / local forms
/// takes may be another element or a static
/// ([`decode_array_increment_deferred_operand`]); it is tried only after
/// those forms refused, so a group the wave-75 decoder took decodes exactly
/// as before. A deferred operand's arithmetic is the group's own width's (no
/// int shift count under a long group). The body's decoder.
#[inline(always)]
#[allow(clippy::type_complexity)]
fn decode_array_increment_group(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    p: usize,
    mode: u8,
) -> Option<(u8, ArrayIncOperand, u8, usize, FieldIncrementUse)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let value = mode & ARRAY_INC_VALUE != 0;
    let shift_div = mode & ARRAY_INC_SHIFT_DIV != 0;
    let load_pc = if mode & ARRAY_INC_NO_DUP != 0 {
        p
    } else {
        if byte(p)? != 0x5c {
            return None;
        }
        p + 1
    };
    let load = byte(load_pc)?;
    let long_group = match load {
        0x2e | 0x33..=0x35 => false,
        0x2f => true,
        _ => return None,
    };
    let dup_op = if long_group { 0x5e } else { 0x5b };
    let mut q = load_pc + 1;
    let post = value && byte(q)? == dup_op;
    if post {
        q += 1;
    }
    // `int_count`: a long group's int operand, which only a long shift takes.
    let ready = if long_group {
        match super::static_increment_long_operand(frame, code, code_len, q) {
            Some((k, n)) => Some((super::IncrementOperand::Long(k), n, false)),
            None if shift_div => super::static_increment_operand(frame, code, code_len, q)
                .map(|(k, n)| (super::IncrementOperand::Long(i64::from(k)), n, true)),
            None => None,
        }
    } else {
        super::static_increment_operand(frame, code, code_len, q)
            .map(|(k, n)| (super::IncrementOperand::Int(k), n, false))
    };
    let (operand, op_pc, int_count) = match ready {
        Some((k, n, int_count)) => (ArrayIncOperand::Ready(k), n, int_count),
        // Round i3 wave 76 (lane L7): another element or a static, read by
        // the body; with both mode bits clear this refuses as before.
        None if mode & (ARRAY_INC_ARRAY_OPERAND | ARRAY_INC_STATIC_OPERAND) != 0 => {
            let (o, n) = decode_array_increment_deferred_operand(
                frame, code, code_len, q, long_group, mode,
            )?;
            (o, n, false)
        }
        None => return None,
    };
    // The int arithmetic opcodes that cannot throw (no `idiv` / `irem`)
    // and whose second operand is an int; their long twins are one more.
    // Under `shift_div` also the divisions (the body declines a zero
    // divisor, so the unfused one throws) and the shifts.
    let arith = byte(op_pc)?;
    let arith_ok = if int_count {
        matches!(arith, 0x79 | 0x7b | 0x7d)
    } else if long_group {
        matches!(arith, 0x61 | 0x65 | 0x69 | 0x7f | 0x81 | 0x83)
            || (shift_div && matches!(arith, 0x6d | 0x71))
    } else {
        matches!(arith, 0x60 | 0x64 | 0x68 | 0x7e | 0x80 | 0x82)
            || (shift_div && matches!(arith, 0x6c | 0x70 | 0x78 | 0x7a | 0x7c))
    };
    if !arith_ok {
        return None;
    }
    let mut store_pc = op_pc + 1;
    if let Some(conv) = array_increment_conversion(load) {
        if byte(store_pc)? != conv {
            return None;
        }
        store_pc += 1;
    }
    let used = if post {
        FieldIncrementUse::Old
    } else if value && byte(store_pc)? == dup_op {
        store_pc += 1;
        FieldIncrementUse::New
    } else {
        FieldIncrementUse::Statement
    };
    // `*aload` + 0x21 is its `*astore` (0x2e -> 0x4f, ..., 0x35 -> 0x56).
    if byte(store_pc)? != load + 0x21 {
        return None;
    }
    Some((load, operand, arith, store_pc + 1, used))
}

/// The conversion javac puts before the store of a narrow element's
/// compound assignment: `i2b` for `baload`, `i2c` for `caload`, `i2s` for
/// `saload`; `None` for `iaload` / `laload`.
#[inline(always)]
fn array_increment_conversion(load: u8) -> Option<u8> {
    match load {
        0x33 => Some(0x91),
        0x34 => Some(0x92),
        0x35 => Some(0x93),
        _ => None,
    }
}

/// The body both fused array increment entries share: decode the group at
/// `p` (the `dup2`), validate `arr` / `index` as the quickened `*aload`
/// would (`prim_elem_ptr`: a live array of the opcode's element type, the
/// index in range; additionally a `boolean[]` under `baload` declines, since
/// javac never emits that group and `bastore`'s bit-0 rule is not
/// reproduced here; javac's `boolean[]` compound `|=` has no `i2b` and is
/// refused by the decoder anyway), read the element as `array_load_prim`
/// reads it, apply the group's arithmetic as the unfused opcode computes it
/// (wrapping add / sub / mul, and / or / xor), narrow by the group's own
/// conversion and store through the same element address with the width
/// `array_store_prim` stores (a primitive store takes no reference barrier
/// and cannot raise `ArrayStoreException`). Every decline precedes the
/// store, so a decline has written nothing. `need_room`: a value form must
/// push one slot (the local entry; the stack entry replaces the array slot).
///
/// Returns the pc past the `*astore` and, for a value form, the value to
/// leave (the old element as the `*aload` pushed it, or the new narrowed one
/// as the `*astore` popped it).
///
/// `mode` is the decoder's ([`decode_array_increment_mode`]; `p` is the
/// `*aload`'s pc under [`ARRAY_INC_NO_DUP`]). Under [`ARRAY_INC_SHIFT_DIV`]
/// (round i3 wave 74, lane L7) a division by zero declines after the element
/// is read and before anything is stored, so the unfused `idiv` / `ldiv`
/// raises the `ArithmeticException` at its own pc; `MIN_VALUE / -1` and
/// `MIN_VALUE % -1` wrap as the JVM's do (`wrapping_div` / `wrapping_rem`);
/// a shift uses the count's low five (int) or six (long) bits, as
/// `wrapping_shl` / `wrapping_shr` mask them.
///
/// `memo_pc` (round i3 wave 75, lane L7,
/// `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`): the calling entry's own pc
/// when that per-VM switch is on, [`NO_DECLINE_MEMO`] otherwise. A group the
/// decoder refuses then sets that pc's bit in the method's decline memo
/// ([`note_array_increment_decline`]), so the dispatch loop stops calling
/// the entry there. No other decline is remembered: a null or short array,
/// a negative index, a zero divisor and a full stack are per execution.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn fused_array_increment_body(
    zgc: &ZgcRealHeap,
    frame: &Frame,
    code_len: usize,
    p: usize,
    arr_ptr: u64,
    index: i32,
    mode: u8,
    census: bool,
    need_room: bool,
    memo_pc: usize,
    shared: &SharedVm,
    static_sites: &mut FieldSiteCache,
) -> Option<(usize, Option<super::IncrementOperand>)> {
    macro_rules! decline {
        ($why:expr) => {{
            site_stats::bump_if(census, $why);
            return None;
        }};
    }
    let Some((load, deferred, arith, next_pc, used)) =
        decode_array_increment_group(frame, &frame.code, code_len, p, mode)
    else {
        if memo_pc != NO_DECLINE_MEMO {
            note_array_increment_decline(frame, memo_pc);
        }
        decline!(site_stats::ARRAY_INC_DECLINE_SHAPE);
    };
    // A value form pushes one slot: decided before anything is stored (never
    // under verified code: the unfused group peaks three slots higher).
    if need_room && used != FieldIncrementUse::Statement && !frame.stack.has_room() {
        decline!(site_stats::ARRAY_INC_DECLINE_SHAPE);
    }
    if arr_ptr == 0 || index < 0 {
        decline!(site_stats::ARRAY_INC_DECLINE_ARRAY);
    }
    // SAFETY: a non-null `Object`-tagged local or operand-stack slot holds a
    // heap address, the bits `aload_N` would push; `prim_elem_ptr`
    // re-validates the header before anything is read.
    let arr = unsafe { ObjectRef::from_raw(arr_ptr as *mut u8) };
    let Some(ep) = prim_elem_ptr(zgc, arr, index, load) else {
        decline!(site_stats::ARRAY_INC_DECLINE_ARRAY);
    };
    if load == 0x33 {
        // SAFETY: `prim_elem_ptr` validated `arr` as a live array.
        let header = unsafe { &*(arr.as_ptr() as *const ObjectHeader) };
        if header.element_type() != ArrayElementType::Byte {
            decline!(site_stats::ARRAY_INC_DECLINE_ARRAY);
        }
    }
    // The operand (round i3 wave 76, lane L7): a deferred one is read now,
    // after the group's own element was validated and before anything is
    // stored, so a decline here has written nothing and the unfused group
    // throws where it would (the group's `*aload` first, then the operand's).
    // Both reads precede the one store, so `a[j] += a[j]` (one element twice)
    // reads the old value twice, as the unfused group does.
    let operand = match deferred {
        ArrayIncOperand::Ready(k) => k,
        ArrayIncOperand::Element {
            array_local,
            index_local,
            load: operand_load,
        }
        | ArrayIncOperand::ElementAt {
            array_local,
            index_local,
            load: operand_load,
            ..
        } => match array_increment_element_operand(
            zgc,
            frame,
            array_local,
            index_local,
            match deferred {
                ArrayIncOperand::ElementAt { delta, .. } => delta,
                _ => 0,
            },
            operand_load,
        ) {
            Some(k) => k,
            None => {
                // Round i3 wave 77 (lane L7,
                // `CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET`): a pc
                // that keeps missing is retired into the decline memo.
                note_operand_miss_if_budgeted(shared, frame, memo_pc);
                decline!(site_stats::ARRAY_INC_DECLINE_ARRAY)
            }
        },
        ArrayIncOperand::Static { cp_index } => {
            match array_increment_static_operand(shared, frame, static_sites, cp_index, load) {
                Some(k) => k,
                None => {
                    // As the element operand's miss (wave 77): a static site
                    // that never fills (`<clinit>`, `java/lang/System`)
                    // stops costing the call after its budget.
                    note_operand_miss_if_budgeted(shared, frame, memo_pc);
                    decline!(site_stats::ARRAY_INC_DECLINE_ARRAY)
                }
            }
        }
    };
    // SAFETY (every raw load and store): `ep` is an in-bounds element of a
    // live array of exactly the width each arm reads and writes
    // (`prim_elem_for_opcode`); no safepoint can intervene in this call.
    let left = match operand {
        super::IncrementOperand::Int(k) => {
            // As `array_load_prim` pushes it: sign-extended for byte / short,
            // zero-extended for char.
            let current = match load {
                0x2e => (unsafe { load_u32(ep) }) as i32,
                0x33 => i32::from((unsafe { load_u8(ep) }) as i8),
                0x34 => i32::from(unsafe { load_u16(ep) }),
                0x35 => i32::from((unsafe { load_u16(ep) }) as i16),
                _ => decline!(site_stats::ARRAY_INC_DECLINE_SHAPE),
            };
            let next = match arith {
                0x60 => current.wrapping_add(k),
                0x64 => current.wrapping_sub(k),
                0x68 => current.wrapping_mul(k),
                0x7e => current & k,
                0x80 => current | k,
                0x82 => current ^ k,
                // `idiv` / `irem` by zero: the unfused one throws.
                0x6c | 0x70 if k == 0 => decline!(site_stats::ARRAY_INC_DECLINE_ARRAY),
                0x6c => current.wrapping_div(k),
                0x70 => current.wrapping_rem(k),
                // Cast: the JVM shift count's low five bits (`wrapping_*`
                // masks the `u32` count by 31).
                0x78 => current.wrapping_shl(k as u32),
                0x7a => current.wrapping_shr(k as u32),
                // Cast: `iushr` is the logical shift of the same 32 bits.
                0x7c => (current as u32).wrapping_shr(k as u32) as i32,
                _ => decline!(site_stats::ARRAY_INC_DECLINE_SHAPE),
            };
            let narrowed = super::narrow_increment_result(next, array_increment_conversion(load));
            // Cast: the element's own width, as `array_store_prim` truncates.
            match load {
                0x2e => unsafe { store_u32(ep, narrowed as u32) },
                0x33 => unsafe { store_u8(ep, narrowed as u8) },
                _ => unsafe { store_u16(ep, narrowed as u16) },
            }
            super::IncrementOperand::Int(if used == FieldIncrementUse::Old {
                current
            } else {
                narrowed
            })
        }
        super::IncrementOperand::Long(k) => {
            if load != 0x2f {
                decline!(site_stats::ARRAY_INC_DECLINE_SHAPE);
            }
            let current = (unsafe { load_u64(ep) }) as i64;
            let next = match arith {
                0x61 => current.wrapping_add(k),
                0x65 => current.wrapping_sub(k),
                0x69 => current.wrapping_mul(k),
                0x7f => current & k,
                0x81 => current | k,
                0x83 => current ^ k,
                // `ldiv` / `lrem` by zero: the unfused one throws.
                0x6d | 0x71 if k == 0 => decline!(site_stats::ARRAY_INC_DECLINE_ARRAY),
                0x6d => current.wrapping_div(k),
                0x71 => current.wrapping_rem(k),
                // Cast: `k` is the int shift count widened by the decoder;
                // its low six bits are the JVM's (`wrapping_*` masks the
                // `u32` count by 63, and truncating keeps those bits).
                0x79 => current.wrapping_shl(k as u32),
                0x7b => current.wrapping_shr(k as u32),
                // Cast: `lushr` is the logical shift of the same 64 bits.
                0x7d => (current as u64).wrapping_shr(k as u32) as i64,
                _ => decline!(site_stats::ARRAY_INC_DECLINE_SHAPE),
            };
            unsafe { store_u64(ep, next as u64) };
            super::IncrementOperand::Long(if used == FieldIncrementUse::Old {
                current
            } else {
                next
            })
        }
    };
    // As `array_store_prim`: the host just wrote this array, so a device
    // buffer mirroring it is stale.
    #[cfg(feature = "gpu-offload")]
    crate::runtime::offload::input_cache::invalidate(arr);
    if used == FieldIncrementUse::Statement {
        site_stats::bump_if(census, site_stats::ARRAY_INC_HIT);
        Some((next_pc, None))
    } else {
        site_stats::bump_if(census, site_stats::ARRAY_INC_VALUE_HIT);
        Some((next_pc, Some(left)))
    }
}

/// The array-element operand of a fused array increment (round i3 wave 76,
/// lane L7, `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND`):
/// `local[array_local][local[index_local]]` read as the quickened `*aload`
/// `operand_load` pushes it (`prim_elem_ptr`'s validation: a live array of the
/// opcode's element type, `boolean[]` under `baload` included, the index in
/// range; sign-extended for `baload` / `saload`, zero-extended for `caload`,
/// a `Long` for `laload`). `None` for a null or non-`Object` array local, a
/// non-int index local and every array the quickened `*aload` would refuse,
/// so the unfused operand `*aload` throws its NPE / AIOOBE at its own pc. The
/// decoder checked both locals are below `max_locals`.
///
/// `delta` (round i3 wave 77, lane L7, the `ElementAt` index expression; `0`
/// otherwise) is added to the index local as `iadd` adds it (wrapping), and
/// the sum is what the range check sees, as the unfused `*aload`'s index.
#[inline(always)]
fn array_increment_element_operand(
    zgc: &ZgcRealHeap,
    frame: &Frame,
    array_local: usize,
    index_local: usize,
    delta: i32,
    operand_load: u8,
) -> Option<super::IncrementOperand> {
    let aptr = frame
        .get_local_compact_unchecked(array_local)
        .as_object_ptr()?;
    let index = frame
        .get_local_compact_unchecked(index_local)
        .as_int()?
        .wrapping_add(delta);
    if aptr == 0 || index < 0 {
        return None;
    }
    // SAFETY: a non-null `Object`-tagged local slot holds a heap address, the
    // bits `aload_N` would push; `prim_elem_ptr` re-validates the header.
    let arr = unsafe { ObjectRef::from_raw(aptr as *mut u8) };
    let ep = prim_elem_ptr(zgc, arr, index, operand_load)?;
    // SAFETY: `ep` is an in-bounds element of a live array of exactly the
    // width each arm reads (`prim_elem_for_opcode`).
    Some(match operand_load {
        0x2e => super::IncrementOperand::Int((unsafe { load_u32(ep) }) as i32),
        0x33 => super::IncrementOperand::Int(i32::from((unsafe { load_u8(ep) }) as i8)),
        0x34 => super::IncrementOperand::Int(i32::from(unsafe { load_u16(ep) })),
        0x35 => super::IncrementOperand::Int(i32::from((unsafe { load_u16(ep) }) as i16)),
        0x2f => super::IncrementOperand::Long((unsafe { load_u64(ep) }) as i64),
        _ => return None,
    })
}

/// The `getstatic` operand of a fused array increment (round i3 wave 76,
/// lane L7, `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND`): the
/// static at pool index `cp_index` of `frame`'s class, answered only from a
/// filled quickened site (`static_sites`, the probe the `getstatic` inline
/// hit makes: a filled site proves the field resolved, static, not a
/// `java/lang/System` field, its class `Initialized`), read through
/// `get_static_shared` with the `SeqCst` fence after a volatile read, as the
/// quickened `getstatic`. The descriptor must be the group's width: `I` /
/// `B` / `C` / `S` / `Z` (an int) for an int group, `J` for a long one
/// (`load` is the group's `*aload`); the value is what
/// `push_static_field_value` pushes for it (`Object(None)`, a zero-initialised
/// slot, reads as 0; a `J` also takes a widened `Int`; anything else
/// refuses). `None` while any JVMTI field watchpoint is armed (the
/// `getstatic` must post its access), on a cold site (the unfused
/// `getstatic` resolves, initialises and fills it; the next execution hits)
/// and for any other descriptor or value shape: nothing is read into the
/// stack and nothing written.
#[inline(always)]
fn array_increment_static_operand(
    shared: &SharedVm,
    frame: &Frame,
    static_sites: &mut FieldSiteCache,
    cp_index: u16,
    load: u8,
) -> Option<super::IncrementOperand> {
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        return None;
    }
    let field = static_sites.get(frame.class_id, cp_index)?;
    let long_group = load == 0x2f;
    let desc_ok = if long_group {
        field.desc_byte == b'J'
    } else {
        matches!(field.desc_byte, b'I' | b'B' | b'C' | b'S' | b'Z')
    };
    if !desc_ok || field.is_reference {
        return None;
    }
    let (holder, slot, volatile) = (
        field.declaring_class_id,
        field.field_index,
        field.is_volatile,
    );
    let value = crate::vm::get_static_shared(shared, holder, slot);
    if volatile {
        std::sync::atomic::fence(Ordering::SeqCst);
    }
    match (long_group, value) {
        (false, Value::Int(x)) => Some(super::IncrementOperand::Int(x)),
        (false, Value::Object(None)) => Some(super::IncrementOperand::Int(0)),
        (true, Value::Long(x)) => Some(super::IncrementOperand::Long(x)),
        (true, Value::Int(x)) => Some(super::IncrementOperand::Long(i64::from(x))),
        (true, Value::Object(None)) => Some(super::IncrementOperand::Long(0)),
        _ => None,
    }
}

/// Push what a value form of the fused array increment leaves: an int slot
/// (`iadd`, `i2b`, the quickened `*aload` push the same `KIND_UNKNOWN` int)
/// or a `KIND_LONG` one (`ladd` and the quickened `laload`).
#[inline(always)]
fn push_increment_value(stack: &mut ValueStack, left: super::IncrementOperand) {
    match left {
        super::IncrementOperand::Int(v) => stack.push_int_unchecked(v),
        super::IncrementOperand::Long(v) => stack.push_long_unchecked(v),
    }
}

/// The fused array element increment
/// (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT`, `VmConfig::interp_fused_array_increment`;
/// interpreter round i3 wave 72, lane L7; proposal
/// `i71-L7-proposal-fuse-the-array-element-increment-20261104`): at `pc` of
/// `frame` an `aload_N` (the array), an `iload_M` (the index), then the
/// group [`decode_array_increment`] takes at `pc + 2`. `a[i]++`,
/// `hist[b] += k`, `sum[j] -= x`, and (under
/// `VmConfig::interp_fused_array_increment_value`) `x = a[i]++` /
/// `x = ++a[i]`. The array and the index are read straight out of their
/// locals; a hit stores the element and pushes nothing (the statement form)
/// or the one value a value form leaves, and returns the pc past the
/// `*astore`. `None`, having written nothing and pushed nothing, for any
/// other shape and for every array / index the quickened `*aload` would not
/// serve, so the plain `aload_N` runs and the `*aload` raises the NPE /
/// AIOOBE at its own pc.
///
/// Under `VmConfig::interp_fused_array_update_no_dup` (round i3 wave 74,
/// lane L7) the `dup2` at `pc + 2` may instead be the same `aload_N;
/// iload_M` pair again (`a[j] = a[j] + k`): the group is then decoded from
/// its `*aload` at `pc + 4` ([`ARRAY_INC_NO_DUP`]) and served the same way;
/// the first unfused `*aload` of a decline throws, as the `dup2` form's.
/// Under `interp_fused_array_increment_shift_div` the shifts and divisions
/// are taken too ([`decode_array_increment_mode`]).
///
/// The caller has tested `fusion_off` (single-step, an armed breakpoint:
/// the group's inner pcs must each be seen), the switch, the two opcode
/// bytes it dispatched on, and `fast_field_zgc` (which is `zgc`). No JVMTI
/// field watch concerns an array element. Out of line, so the loop's frame
/// keeps its size; the per-VM value switch and census gate are read here,
/// so the loop hoists nothing for it.
#[inline(never)]
pub(super) fn fused_array_increment_local(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
    static_sites: &mut FieldSiteCache,
) -> Option<usize> {
    if memo_says_declined(shared, frame, pc) {
        return None;
    }
    let census = shared.config.site_census;
    let at = |i: usize| frame.code.get(i).copied().filter(|_| i < code_len);
    let (Some(load), Some(iload)) = (at(pc), at(pc + 1)) else {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    };
    if !(0x2a..=0x2d).contains(&load) || !(0x1a..=0x1d).contains(&iload) {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    }
    let (a, i) = (usize::from(load - 0x2a), usize::from(iload - 0x1a));
    let max_locals = usize::from(frame.max_locals);
    if a >= max_locals || i >= max_locals {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    }
    let (Some(arr_ptr), Some(index)) = (
        frame.get_local_compact_unchecked(a).as_object_ptr(),
        frame.get_local_compact_unchecked(i).as_int(),
    ) else {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_ARRAY);
        return None;
    };
    let mode = array_increment_mode_of(shared);
    let memo_pc = decline_memo_pc(shared, pc);
    // The `dup2` form, or (round i3 wave 74, lane L7,
    // `CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP`) the same `aload_N;
    // iload_M` pair again, `a[j] = a[j] + k`: the group's tail is then
    // decoded from its `*aload` at `pc + 4`.
    let (p, mode) = match at(pc + 2) {
        Some(0x5c) => (pc + 2, mode),
        Some(b)
            if b == load
                && shared.config.interp_fused_array_update_no_dup
                && at(pc + 3) == Some(iload) =>
        {
            (pc + 4, mode | ARRAY_INC_NO_DUP)
        }
        _ => {
            // A prefix that is no update of the same element (`a[j] = a[j]
            // + ...` with another index local, `x = a[j] ... a[k]`): bytes
            // alone, so remembered (wave 75).
            if memo_pc != NO_DECLINE_MEMO {
                note_array_increment_decline(frame, memo_pc);
            }
            site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
            return None;
        }
    };
    let (next_pc, left) = fused_array_increment_body(
        zgc,
        frame,
        code_len,
        p,
        arr_ptr,
        index,
        mode,
        census,
        true,
        memo_pc,
        shared,
        static_sites,
    )?;
    if let Some(v) = left {
        push_increment_value(&mut frame.stack, v);
    }
    Some(next_pc)
}

/// The fused array element increment whose array is already on top of the
/// operand stack (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_TOP`,
/// `VmConfig::interp_fused_array_increment_top`; interpreter round i3 wave
/// 72, lane L7): at `pc` of `frame` an `iload_M` (the index), then the group
/// [`decode_array_increment`] takes at `pc + 1`, below an array pushed by a
/// `getfield` (`this.counts[i]++`), an `aaload` (`m[r][c] += k`), an
/// `aload` or anything else. Admitted only with
/// `VmConfig::interp_fused_array_increment` (tested here, so the loop tests
/// one switch); the value forms with `interp_fused_array_increment_value`.
/// A hit pops the array slot and pushes the one value a value form leaves
/// (net: the slot is replaced), and returns the pc past the `*astore`;
/// `None` leaves the stack exactly as it was and has written nothing, so the
/// plain `iload_M` runs and the `*aload` throws at its own pc. The caller has
/// tested `fusion_off`, the switch and the `dup2` byte, and passes
/// `fast_field_zgc` as `zgc`.
#[inline(never)]
pub(super) fn fused_array_increment_top(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
    static_sites: &mut FieldSiteCache,
) -> Option<usize> {
    if !shared.config.interp_fused_array_increment || memo_says_declined(shared, frame, pc) {
        return None;
    }
    let census = shared.config.site_census;
    let Some(iload) = frame.code.get(pc).copied().filter(|_| pc < code_len) else {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    };
    let i = usize::from(iload.wrapping_sub(0x1a));
    if !(0x1a..=0x1d).contains(&iload) || i >= usize::from(frame.max_locals) {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    }
    if frame.stack.is_empty() {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    }
    let (arr_cv, _) = frame.stack.peek_with_kind_at(0);
    let index_cv = frame.get_local_compact_unchecked(i);
    let (Some(arr_ptr), Some(index)) = (arr_cv.as_object_ptr(), index_cv.as_int()) else {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_ARRAY);
        return None;
    };
    let (next_pc, left) = fused_array_increment_body(
        zgc,
        frame,
        code_len,
        pc + 1,
        arr_ptr,
        index,
        array_increment_mode_of(shared),
        census,
        false,
        decline_memo_pc(shared, pc),
        shared,
        static_sites,
    )?;
    frame.stack.pop_compact();
    if let Some(v) = left {
        push_increment_value(&mut frame.stack, v);
    }
    site_stats::bump_if(census, site_stats::ARRAY_INC_TOP_HIT);
    Some(next_pc)
}

/// The fused array element increment entered at its `dup2`
/// (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_DUP2`,
/// `VmConfig::interp_fused_array_increment_dup2`; interpreter round i3 wave
/// 72, lane L7): at `pc` of `frame` the `dup2` the group
/// [`decode_array_increment`] takes opens with, the array and the index
/// both already on the operand stack, however they were computed:
/// `hist[x & 255]++`, `count[s.charAt(i)]++`, `this.a[i + 1] += k`, and
/// every shape the local and stack-top entries also serve (two dispatches
/// later). Admitted only with `VmConfig::interp_fused_array_increment`
/// (tested here); the value forms with
/// `interp_fused_array_increment_value`. A hit pops the two slots and pushes
/// the one value a value form leaves, and returns the pc past the
/// `*astore`; `None` leaves the stack exactly as it was and has written
/// nothing, so the plain `dup2` runs and the `*aload` throws at its own pc.
/// The caller (the `dup2` arm) has tested `fusion_off`, the switch and that
/// the next byte is an `*aload` the group can open with, and passes
/// `fast_field_zgc` as `zgc`.
#[inline(never)]
pub(super) fn fused_array_increment_dup2(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
    static_sites: &mut FieldSiteCache,
) -> Option<usize> {
    if !shared.config.interp_fused_array_increment || memo_says_declined(shared, frame, pc) {
        return None;
    }
    let census = shared.config.site_census;
    if frame.stack.len() < 2 {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    }
    let (index_cv, index_kind) = frame.stack.peek_with_kind_at(0);
    let (arr_cv, _) = frame.stack.peek_with_kind_at(1);
    // A category-2 slot on top is a `long` / `double` `dup2` duplicates,
    // never the `(arrayref, int)` pair an `*aload` takes.
    if ValueStack::is_cat2_kind(index_kind, index_cv) {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_SHAPE);
        return None;
    }
    let (Some(arr_ptr), Some(index)) = (arr_cv.as_object_ptr(), index_cv.as_int()) else {
        site_stats::bump_if(census, site_stats::ARRAY_INC_DECLINE_ARRAY);
        return None;
    };
    let (next_pc, left) = fused_array_increment_body(
        zgc,
        frame,
        code_len,
        pc,
        arr_ptr,
        index,
        array_increment_mode_of(shared),
        census,
        false,
        decline_memo_pc(shared, pc),
        shared,
        static_sites,
    )?;
    frame.stack.pop_compact();
    frame.stack.pop_compact();
    if let Some(v) = left {
        push_increment_value(&mut frame.stack, v);
    }
    site_stats::bump_if(census, site_stats::ARRAY_INC_DUP2_HIT);
    Some(next_pc)
}

/// An empty static-site table for the array increment entries' tests that
/// use no `getstatic` operand (round i3 wave 76, lane L7).
#[cfg(test)]
fn no_sites() -> FieldSiteCache {
    FieldSiteCache::new()
}

/// A default-config VM for the body tests that predate the wave-76 operand
/// arguments (they read no static operand, so any VM serves).
#[cfg(test)]
fn body_test_vm() -> &'static SharedVm {
    static VM: std::sync::OnceLock<SharedVm> = std::sync::OnceLock::new();
    VM.get_or_init(|| SharedVm::new(crate::config::VmConfig::default()))
}

// ── Fused array increment decline memo (round i3 wave 75, lane L7)
//
// (Wave 73's byte-only screen, `array_increment_group_may_fuse` under
// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SCREEN`, stood here. It was kept off
// after measuring (a dozen inline byte compares cost every hit ~10%), it
// predated the wave-74 shifts / divisions and no-dup form, so it refused
// shapes the decoder now takes, and it was retired in wave 75:
// `docs/internal/fixed-bugs/interpreter-i73-L7-array-increment-decline-screen-KEPT-OFF-20261106.md`.
// The memo below answers its question exactly, per pc, after one decline.)

/// The memo test each array increment entry makes first (round i3 wave 76,
/// lane L7): `true` when `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO` is on and
/// the entry's `pc` already declined on its bytes
/// ([`array_increment_declined`]), so the entry returns `None` before any
/// decode. Wave 75 made this test inline in the three dispatch-loop arms,
/// ahead of the call; each inlined copy kept the frame's method entry, the
/// bitmap's slice and the word index live beside the arm's own values, and
/// with the two wide-local call sites they took the loop's frame from 1784
/// (wave 74) to its 1816-byte ratchet. Inside the entry a hit pays the same
/// test (it ran before every hit's call anyway); a remembered decline pays
/// the call and return, not the decode.
#[inline(always)]
fn memo_says_declined(shared: &SharedVm, frame: &Frame, pc: usize) -> bool {
    shared.config.interp_fused_array_decline_memo && array_increment_declined(frame, pc)
}

/// [`fused_array_increment_body`]'s `memo_pc` when nothing is to be
/// remembered (the switch is off).
pub(super) const NO_DECLINE_MEMO: usize = usize::MAX;

/// The `memo_pc` an array increment entry at `pc` hands its body: `pc`
/// under `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`
/// (`VmConfig::interp_fused_array_decline_memo`), [`NO_DECLINE_MEMO`]
/// otherwise.
#[inline(always)]
fn decline_memo_pc(shared: &SharedVm, pc: usize) -> usize {
    if shared.config.interp_fused_array_decline_memo {
        pc
    } else {
        NO_DECLINE_MEMO
    }
}

/// Whether a fused array increment entry at `pc` of `frame`'s method has
/// already seen its group refused on the group's bytes
/// (`CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`; interpreter round i3 wave 75,
/// lane L7; proposal
/// `i74-L7-proposal-a-per-method-array-increment-decline-memo-20261107`), so
/// the entry declines there without decoding. Each entry tests it first
/// ([`memo_says_declined`]; since wave 76 the dispatch loop no longer tests
/// it inline), so it runs only on an array increment prefix. `false` for a frame not pushed from a
/// `CachedBytecodeMethod` (no memo: those keep the call), for a method with no decline yet (the
/// bitmap is allocated at its first), and for a pc past the bitmap. A
/// `match`, an `Arc` deref, the cell's state and one word: no allocation, no
/// lock, no store.
///
/// What a `true` costs is never an answer: the plain opcode then runs the
/// group unfused, which is what every decline did anyway. A bit is set only
/// for a decline that is a function of the method's bytes, the per-VM
/// switches and the operand local's verifier-fixed type
/// ([`note_array_increment_decline`]), so it would decline again.
#[inline(always)]
pub(super) fn array_increment_declined(frame: &Frame, pc: usize) -> bool {
    let Some(cm) = frame.cached_method() else {
        return false;
    };
    let Some(memo) = cm.array_inc_declined.get() else {
        return false;
    };
    memo.declined
        .get(pc >> 6)
        .is_some_and(|w| (w.load(Ordering::Relaxed) >> (pc & 63)) & 1 != 0)
}

/// Remember that the fused array increment entry at `pc` of `frame`'s
/// method declined its group on the group's bytes (see
/// [`array_increment_declined`]): allocate the method entry's bitmap (one
/// bit per byte of its code) at the first such decline, then set the bit
/// (relaxed `fetch_or`; two threads racing on one word both land). Nothing
/// for a frame without a cached method entry, or one running other code than
/// its entry's (a body moved under a JVMTI redefinition), whose pcs the
/// bitmap does not index. Cold: once per declining site per entry, at most
/// once per execution while racing.
#[cold]
#[inline(never)]
fn note_array_increment_decline(frame: &Frame, pc: usize) {
    let Some(cm) = frame.cached_method() else {
        return;
    };
    if !std::sync::Arc::ptr_eq(&cm.code, &frame.code) {
        return;
    }
    let memo = cm
        .array_inc_declined
        .get_or_init(|| cratonvm_jit_api::ArrayIncMemo::new(cm.code.len()));
    set_array_increment_decline_bit(memo, pc);
}

/// Set `pc`'s bit in `memo`'s decline bitmap (relaxed `fetch_or`; a pc past
/// the bitmap is ignored).
#[inline(always)]
fn set_array_increment_decline_bit(memo: &cratonvm_jit_api::ArrayIncMemo, pc: usize) {
    if let Some(w) = memo.declined.get(pc >> 6) {
        w.fetch_or(1u64 << (pc & 63), Ordering::Relaxed);
    }
}

/// How many deferred-operand misses an array increment entry's pc may take
/// before [`note_array_increment_operand_miss`] retires it into the decline
/// memo (`CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET`, round i3 wave 77,
/// lane L7). Counted over the method entry's life (no reset: a hit never
/// touches the counter).
pub(super) const ARRAY_OPERAND_MISS_BUDGET: u8 = 64;

/// [`fused_array_increment_body`]'s note of a deferred-operand miss at
/// `memo_pc` (a cold static site, a null or short operand array): under
/// `CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET` (and with the decline
/// memo on, which `memo_pc` says) count it, and at the budget retire the pc.
/// Off: one per-VM bool load on a decline path.
#[inline(always)]
fn note_operand_miss_if_budgeted(shared: &SharedVm, frame: &Frame, memo_pc: usize) {
    if memo_pc != NO_DECLINE_MEMO && shared.config.interp_fused_array_operand_miss_budget {
        note_array_increment_operand_miss(frame, memo_pc);
    }
}

/// Count one deferred-operand miss of the fused array increment entry at
/// `pc` of `frame`'s method (round i3 wave 77, lane L7): the method entry's
/// memo (allocated here if no decline came first) gets a per-pc saturating
/// byte counter at its first miss, and the miss that brings `pc`'s counter
/// to [`ARRAY_OPERAND_MISS_BUDGET`] sets `pc`'s decline bit, so the loop's
/// entry then returns before decoding, as after a shape decline. Returns
/// whether `pc` is now retired. Nothing for a frame without a cached method
/// entry or one running other code than its entry's (the memo's own rule).
///
/// The counter's compare-exchange loop saturates at the budget, so racing
/// threads never wrap it; two threads both seeing the budget reached both
/// set the same bit. Cold: only a deferred-operand decline reaches it.
#[cold]
#[inline(never)]
fn note_array_increment_operand_miss(frame: &Frame, pc: usize) -> bool {
    let Some(cm) = frame.cached_method() else {
        return false;
    };
    if !std::sync::Arc::ptr_eq(&cm.code, &frame.code) {
        return false;
    }
    let memo = cm
        .array_inc_declined
        .get_or_init(|| cratonvm_jit_api::ArrayIncMemo::new(cm.code.len()));
    let misses = memo
        .operand_misses
        .get_or_init(|| (0..cm.code.len()).map(|_| AtomicU8::new(0)).collect());
    let Some(counter) = misses.get(pc) else {
        return false;
    };
    let mut seen = counter.load(Ordering::Relaxed);
    loop {
        if seen >= ARRAY_OPERAND_MISS_BUDGET {
            break;
        }
        match counter.compare_exchange_weak(seen, seen + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                seen += 1;
                break;
            }
            Err(now) => seen = now,
        }
    }
    if seen >= ARRAY_OPERAND_MISS_BUDGET {
        set_array_increment_decline_bit(memo, pc);
        return true;
    }
    false
}

// ── Fused array accumulate (round i3 wave 73, lane L7)

/// The fused array element accumulate's group at `pc` of `code` (unpadded
/// length `code_len`), from opcode bytes alone:
///
/// ```text
/// iload_S                     the accumulator (0x1a..=0x1d)
/// aload_N                     the array       (0x2a..=0x2d)
/// iload_M                     the index       (0x1a..=0x1d)
/// iaload | baload | caload | saload
/// iadd | isub | imul | iand | ior | ixor
/// istore_Z                    (0x3b..=0x3e; any low local, as the
///                              `iload_X; iload_Y; iadd; istore_Z` fusion)
/// ```
///
/// `(S, N, M, the *aload opcode, the arithmetic opcode, Z)`, or `None` for
/// any other shape (a `laload` / `faload` / `daload` / `aaload`, an
/// arithmetic that can throw or is not an int one, a shift, a store that is
/// not `istore_0..3`, a group cut off by the end of the code). The next pc
/// of a hit is `pc + 6`.
#[inline(always)]
fn decode_array_accumulate(
    code: &[u8],
    code_len: usize,
    pc: usize,
) -> Option<(usize, usize, usize, u8, u8, usize)> {
    if pc.checked_add(5)? >= code_len {
        return None;
    }
    let g = code.get(pc..pc + 6)?;
    let (s, a, i, load, arith, st) = (g[0], g[1], g[2], g[3], g[4], g[5]);
    let shape_ok = matches!(s, 0x1a..=0x1d)
        && matches!(a, 0x2a..=0x2d)
        && matches!(i, 0x1a..=0x1d)
        && matches!(load, 0x2e | 0x33..=0x35)
        && matches!(arith, 0x60 | 0x64 | 0x68 | 0x7e | 0x80 | 0x82)
        && matches!(st, 0x3b..=0x3e);
    if !shape_ok {
        return None;
    }
    Some((
        usize::from(s - 0x1a),
        usize::from(a - 0x2a),
        usize::from(i - 0x1a),
        load,
        arith,
        usize::from(st - 0x3b),
    ))
}

/// The fused array element accumulate
/// (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE`,
/// `VmConfig::interp_fused_array_accumulate`; interpreter round i3 wave 73,
/// lane L7; proposal `i72-L7-proposal-fuse-the-array-element-accumulate-20261105`):
/// at `pc` of `frame` the group [`decode_array_accumulate`] takes,
/// `s += a[i]`, `s -= a[i]`, `h ^= b[k]`, `t = s + c[j]`. The accumulator,
/// the array and the index are read straight out of their locals (an int,
/// a non-null `Object`, an int; every local below `max_locals`); the
/// element is validated as the quickened `*aload` validates it
/// (`prim_elem_ptr`: a live array of the opcode's element type, a
/// `boolean[]` under `baload` as that arm admits it, the index in range)
/// and read as `array_load_prim` pushes it (sign-extended for byte / short,
/// zero-extended for char); the result is the unfused arithmetic's
/// (wrapping add / sub / mul, and / or / xor, accumulator first), stored
/// into local Z with the `istore_N` arm's own slot write
/// (`set_local_compact_unchecked` of `CompactValue::int`). Nothing is pushed
/// (the unfused group's net stack effect is zero). Returns `pc + 6`.
///
/// `None`, having written nothing and pushed nothing, for any other shape
/// and for every accumulator / array / index the unfused group would not
/// run the quickened way, so the plain `iload_S` runs and the `*aload`
/// raises the NPE / AIOOBE at its own pc.
///
/// The caller (the `iload_N` arm) has tested `fusion_off` (single-step, an
/// armed breakpoint: the group's inner pcs must each be seen), the `aload_N`
/// and `iload_M` bytes, the switch, and `fast_field_zgc` (which is `zgc`).
/// Out of line, so the loop's frame keeps its size.
#[inline(never)]
pub(super) fn fused_array_accumulate(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
) -> Option<usize> {
    let census = shared.config.site_census;
    macro_rules! decline {
        () => {{
            site_stats::bump_if(census, site_stats::ARRAY_ACC_DECLINE);
            return None;
        }};
    }
    let Some((s, a, i, load, arith, z)) = decode_array_accumulate(&frame.code, code_len, pc) else {
        decline!();
    };
    let max_locals = usize::from(frame.max_locals);
    if s.max(a).max(i).max(z) >= max_locals {
        decline!();
    }
    let (Some(acc), Some(arr_ptr), Some(index)) = (
        frame.get_local_compact_unchecked(s).as_int(),
        frame.get_local_compact_unchecked(a).as_object_ptr(),
        frame.get_local_compact_unchecked(i).as_int(),
    ) else {
        decline!();
    };
    if arr_ptr == 0 || index < 0 {
        decline!();
    }
    // SAFETY: a non-null `Object`-tagged local slot holds a heap address, the
    // bits `aload_N` would push; `prim_elem_ptr` re-validates the header
    // before anything is read.
    let arr = unsafe { ObjectRef::from_raw(arr_ptr as *mut u8) };
    let Some(ep) = prim_elem_ptr(zgc, arr, index, load) else {
        decline!();
    };
    // SAFETY (every raw load): `ep` is an in-bounds element of a live array
    // of exactly the width each arm reads (`prim_elem_for_opcode`); no
    // safepoint can intervene in this call.
    let elem = match load {
        0x2e => (unsafe { load_u32(ep) }) as i32,
        0x33 => i32::from((unsafe { load_u8(ep) }) as i8),
        0x34 => i32::from(unsafe { load_u16(ep) }),
        _ => i32::from((unsafe { load_u16(ep) }) as i16),
    };
    let result = match arith {
        0x60 => acc.wrapping_add(elem),
        0x64 => acc.wrapping_sub(elem),
        0x68 => acc.wrapping_mul(elem),
        0x7e => acc & elem,
        0x80 => acc | elem,
        _ => acc ^ elem,
    };
    frame.set_local_compact_unchecked(z, CompactValue::int(result));
    site_stats::bump_if(census, site_stats::ARRAY_ACC_HIT);
    Some(pc + 6)
}

/// Decode the long form of the fused array element accumulate at `pc`
/// (interpreter round i3 wave 82, lane L7;
/// `i73-L7-proposal-long-and-wide-local-forms-of-the-array-accumulate`,
/// item 1):
///
/// * `lload_S; aload_N; iload_M; laload; <op>; lstore_Z` (6 bytes), and
/// * `lload_S; aload_N; iload_M; <x>aload; i2l; <op>; lstore_Z` (7 bytes)
///   for `iaload`, `baload`, `caload`, `saload` (the widening `long` sum
///   over an `int[]`, `byte[]`, `char[]` or `short[]`),
///
/// with `<op>` one of `ladd`, `lsub`, `lmul`, `land`, `lor`, `lxor` (none of
/// which throws) and every local in 0..3. Answers `(S, N, M, load, op, Z,
/// group length)`, or `None` for any other shape and for a group that does
/// not lie wholly below `code_len`.
#[allow(clippy::type_complexity)]
fn decode_array_accumulate_long(
    code: &[u8],
    code_len: usize,
    pc: usize,
) -> Option<(usize, usize, usize, u8, u8, usize, usize)> {
    if pc.checked_add(5)? >= code_len {
        return None;
    }
    let g = code.get(pc..pc + 6)?;
    let (s, a, i, load) = (g[0], g[1], g[2], g[3]);
    if !(matches!(s, 0x1e..=0x21) && matches!(a, 0x2a..=0x2d) && matches!(i, 0x1a..=0x1d)) {
        return None;
    }
    let (arith, st, len) = match load {
        0x2f => (g[4], g[5], 6),
        0x2e | 0x33..=0x35 => {
            if g[4] != 0x85 || pc + 6 >= code_len {
                return None;
            }
            (g[5], *code.get(pc + 6)?, 7)
        }
        _ => return None,
    };
    if !(matches!(arith, 0x61 | 0x65 | 0x69 | 0x7f | 0x81 | 0x83) && matches!(st, 0x3f..=0x42)) {
        return None;
    }
    Some((
        usize::from(s - 0x1e),
        usize::from(a - 0x2a),
        usize::from(i - 0x1a),
        load,
        arith,
        usize::from(st - 0x3f),
        len,
    ))
}

/// The long form of the fused array element accumulate
/// (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG`,
/// `VmConfig::interp_fused_array_accumulate_long`; interpreter round i3 wave
/// 82, lane L7): at `pc` of `frame` the group [`decode_array_accumulate_long`]
/// takes, `long s; s += la[i]` and `long s; s += a[i]` over an `int[]` /
/// `byte[]` / `char[]` / `short[]`. The accumulator is read straight out of
/// its local only when that slot carries the `long` kind mark (as the fused
/// static increment's `lload` operand reads one,
/// `static_increment_long_operand`), the array and the index as the int form
/// reads them; the element is validated as the quickened `*aload` validates
/// it (`prim_elem_ptr`) and read as `array_load_prim` pushes it, an int
/// element then sign-extended as `i2l` extends it; the result is the unfused
/// long arithmetic's (wrapping add / sub / mul, and / or / xor, accumulator
/// first), stored into local Z exactly as the `lstore_N` arm stores it
/// (`set_local_unchecked` of `Value::Long`, which writes the kind mark and
/// invalidates the upper half). Nothing is pushed. Returns the pc past the
/// group.
///
/// `None`, having written nothing and pushed nothing, for any other shape,
/// an unmarked accumulator, a local pair past `max_locals`, and every array
/// / index the unfused group would not run the quickened way, so the plain
/// `lload_S` runs and the `*aload` raises the NPE / AIOOBE at its own pc.
///
/// The caller (the `lload_N` arms) has tested the `aload_N` and `iload_M`
/// bytes, `fusion_off`, the switch and `fast_field_zgc` (which is `zgc`).
/// Out of line, so the loop's frame keeps its size.
#[inline(never)]
pub(super) fn fused_array_accumulate_long(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
) -> Option<usize> {
    let census = shared.config.site_census;
    macro_rules! decline {
        () => {{
            site_stats::bump_if(census, site_stats::ARRAY_ACC_DECLINE);
            return None;
        }};
    }
    let Some((s, a, i, load, arith, z, len)) =
        decode_array_accumulate_long(&frame.code, code_len, pc)
    else {
        decline!();
    };
    let max_locals = usize::from(frame.max_locals);
    // A long occupies its slot and the next one.
    if (s + 1).max(z + 1).max(a).max(i) >= max_locals || !frame.local_is_long_marked(s) {
        decline!();
    }
    let acc = frame.get_local_compact_unchecked(s).as_long_unchecked();
    let (Some(arr_ptr), Some(index)) = (
        frame.get_local_compact_unchecked(a).as_object_ptr(),
        frame.get_local_compact_unchecked(i).as_int(),
    ) else {
        decline!();
    };
    if arr_ptr == 0 || index < 0 {
        decline!();
    }
    // SAFETY: as in `fused_array_accumulate`: a non-null `Object`-tagged
    // local slot holds a heap address; `prim_elem_ptr` re-validates the
    // header before anything is read.
    let arr = unsafe { ObjectRef::from_raw(arr_ptr as *mut u8) };
    let Some(ep) = prim_elem_ptr(zgc, arr, index, load) else {
        decline!();
    };
    // SAFETY (every raw load): `ep` is an in-bounds element of a live array
    // of exactly the width each arm reads (`prim_elem_for_opcode`); no
    // safepoint can intervene in this call.
    let elem: i64 = match load {
        0x2f => (unsafe { load_u64(ep) }) as i64,
        0x2e => i64::from((unsafe { load_u32(ep) }) as i32),
        0x33 => i64::from((unsafe { load_u8(ep) }) as i8),
        0x34 => i64::from(unsafe { load_u16(ep) }),
        _ => i64::from((unsafe { load_u16(ep) }) as i16),
    };
    let result = match arith {
        0x61 => acc.wrapping_add(elem),
        0x65 => acc.wrapping_sub(elem),
        0x69 => acc.wrapping_mul(elem),
        0x7f => acc & elem,
        0x81 => acc | elem,
        _ => acc ^ elem,
    };
    frame.set_local_unchecked(z, Value::Long(result));
    site_stats::bump_if(census, site_stats::ARRAY_ACC_HIT);
    Some(pc + len)
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

/// Quickened `aastore` (interpreter round i1, wave 22, lane L7).
///
/// Stores `val` (a null or a plain reference, already peeked with its kind
/// mark) into element `index` of the reference array `arr` and returns
/// `true`, or returns `false` with nothing written so the caller keeps the
/// dispatch loop's general `aastore` arm, which owns every exception
/// (`NullPointerException`, `ArrayIndexOutOfBoundsException`,
/// `ArrayStoreException`), the primitive-boxing recovery
/// (`normalize_aastore_value`) and every covariance rule that needs a lock.
///
/// # Why
///
/// `aastore` was the one array opcode with no quickened half. Every element
/// store (`ArrayList.add`'s `elementData[s] = e`, every varargs pack, every
/// `HashMap` resize) paid two `ref_operand_value` decodes, four `VmHeap` enum
/// dispatches (`array_length`, `kind_of`, `element_type_of`, and the
/// `get_array_element` the SATB pre-barrier read the old element through,
/// decoding it to a `Value`), and `set_array_element`, which re-reads the
/// header, re-checks the bounds, runs the corpse audit and matches on the
/// `Value` it was handed.
///
/// # What it keeps, in `ZgcRealHeap::set_array_element`'s order
///
/// * The SATB pre-barrier on the overwritten reference while marking is
///   active, read as the raw slot word (the general arm read it through
///   `get_array_element`, whose unboxing would have hidden a wrapper from the
///   barrier; the raw word is the reference the collector must see).
/// * The store through `write_ref_slot`, which honours compressed oops.
/// * The generational card note on the array.
///
/// # What it declines
///
/// * A value slot that is not null or a plain, plausible reference, or that
///   carries a `long` / `double` kind mark: the general arm's coercion and
///   boxing own those shapes.
/// * A receiver that is not a reference array, or an index out of range
///   (either sign): the general arm raises the exception.
/// * An armed load barrier (`zgc.load_barrier_armed()`): the old slot word
///   may be a colored pointer only the barrier-aware read heals, the same
///   screen `array_load_ref` applies.
/// * A non-null element whose assignability the two lock-free verdicts the
///   general arm consults first cannot confirm — the `Object[]` component
///   memo (`typecheck::reference_array_component_is_object`) and, for a
///   non-array element, the published supers closure
///   (`typecheck::published_subtype_verdict`). Only a confirmed `true` is
///   served, so this arm never decides an `ArrayStoreException`.
///
/// Kill switch: `CRATONVM_JIT_NO_REF_ARRAY_FAST=1`, shared with the quickened
/// `aaload`. Engagement: `CRATONVM_DBG_FIELD_SITE=1` prints
/// `aastore: hit=… miss=…`; `census` is `VmConfig::site_census`, hoisted by
/// the dispatch loop.
#[inline]
pub(super) fn array_store_ref(
    shared: &SharedVm,
    zgc: &ZgcRealHeap,
    arr: ObjectRef,
    index: i32,
    val: CompactValue,
    kind: u8,
    census: bool,
) -> bool {
    // The kill switch (`CRATONVM_JIT_NO_REF_ARRAY_FAST`) is the caller's: the
    // dispatch loop's hoisted `ref_array_fast_on` guards the call (interpreter
    // round i2 wave 60, lane L7; it was a memo read here on every `aastore`).
    if index < 0 || kind == ValueStack::KIND_MARK_LONG || kind == ValueStack::KIND_MARK_DOUBLE {
        site_stats::bump(site_stats::AASTORE_MISS);
        return false;
    }
    let raw: u64 = if val.is_null() {
        0
    } else {
        match val.as_object_ptr() {
            Some(p) if cratonvm_types::plausible_heap_pointer(p) => p,
            // `Int(0)`, a smuggled `long` handle, an implausible payload: the
            // general arm's validated coerce owns them.
            _ => {
                site_stats::bump(site_stats::AASTORE_MISS);
                return false;
            }
        }
    };
    if zgc.load_barrier_armed() {
        site_stats::bump(site_stats::AASTORE_MISS);
        return false;
    }
    let base = arr.as_ptr() as usize;
    // SAFETY: `arr` is the array operand of a verified `aastore`, read from
    // the operand stack in this same safepoint-free window; its first
    // `HEADER_SIZE` bytes are a live header (the premise `array_load_ref`
    // reads on — see `registry_probe_restored`). The kind, element-type and
    // length tests below are what establish the slot address is in bounds.
    let header = unsafe { &*(base as *const ObjectHeader) };
    if header.kind() != ObjectKind::Array || header.element_type() != ArrayElementType::Reference {
        site_stats::bump(site_stats::AASTORE_MISS);
        return false;
    }
    let idx = index as usize;
    if idx >= header.array_length() as usize {
        site_stats::bump(site_stats::AASTORE_MISS);
        return false;
    }
    if raw != 0 {
        // The general arm's first two answers, in its order; anything they
        // cannot confirm goes back to it. On a reference array the header's
        // class id is the COMPONENT class.
        let assignable = super::typecheck::reference_array_component_is_object(shared, arr) || {
            // SAFETY: `raw` is a plausible, non-null reference taken from a
            // reference-typed operand slot of a verified `aastore`, so its
            // first `HEADER_SIZE` bytes are a live header — the premise
            // `array_load_ref` reads an element header on.
            let elem_header = unsafe { &*(raw as usize as *const ObjectHeader) };
            elem_header.kind() != ObjectKind::Array
                && super::typecheck::published_subtype_verdict(
                    shared,
                    elem_header.class_id,
                    header.class_id,
                ) == Some(true)
        };
        if !assignable {
            site_stats::bump(site_stats::AASTORE_MISS);
            return false;
        }
    }
    let Some(offset) = idx.checked_mul(cratonvm_types::narrow_oop::ref_element_size()) else {
        site_stats::bump(site_stats::AASTORE_MISS);
        return false;
    };
    // SAFETY: `idx < length` and the stride is the one `write_prim_element`
    // uses for a reference element, so the slot lies inside the array body.
    let slot = unsafe { (base as *mut u8).add(cratonvm_types::ARRAY_DATA_OFFSET + offset) };
    if zgc.mark_active() {
        // SAFETY: as above; a reference slot of a live reference array.
        let old = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
        if old != 0 {
            zgc.satb_pre_barrier(old as usize);
        }
    }
    // SAFETY: as above; `raw` is 0 or a plausible heap reference, which is
    // what the compressed-oop encode inside `write_ref_slot` requires.
    unsafe { cratonvm_types::narrow_oop::write_ref_slot(slot, raw) };
    // The generational card, after the store (`set_array_element`'s order).
    // zt t3/gn (zr z2/g2's cross-lane hunk X1): per element, so a large old
    // array is carded per granule under `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`;
    // with that switch off it cards the array exactly as `note_ref_store`
    // did, and its first statement is the same `has_old_objects` load.
    zgc.note_ref_store_element(base, slot as usize, header.array_length() as usize);
    site_stats::bump_if(census, site_stats::AASTORE_HIT);
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
            (0x2e, 0x4f, Int, 4),    // iaload  / iastore
            (0x2f, 0x50, Long, 8),   // laload  / lastore
            (0x30, 0x51, Float, 4),  // faload  / fastore
            (0x31, 0x52, Double, 8), // daload  / dastore
            (0x33, 0x54, Byte, 1),   // baload  / bastore
            (0x34, 0x55, Char, 2),   // caload  / castore
            (0x35, 0x56, Short, 2),  // saload  / sastore
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
            final_put: FastFieldSite::PUT_ANY,
        }
    }

    fn resolved(declaring: u32, is_final: bool) -> ResolvedField {
        ResolvedField {
            declaring_class_id: ClassId::new(declaring),
            field_index: 0,
            is_static: false,
            is_volatile: false,
            is_final,
            is_reference: false,
            desc_byte: b'I',
        }
    }

    /// The fast arm's final-field rule (JVMS §6.5 `putfield`): a non-final
    /// field is open to every store, a final field of the site's own class
    /// only to `<init>`, and a final field of another class to none — even
    /// when a legal `getfield` through the same constant-pool entry filled
    /// the site.
    #[test]
    fn final_put_rule_keeps_illegal_final_stores_on_the_full_handler() {
        let own = ClassId::new(7);
        assert_eq!(
            final_put_rule(&resolved(7, false), own),
            FastFieldSite::PUT_ANY
        );
        assert_eq!(
            final_put_rule(&resolved(9, false), own),
            FastFieldSite::PUT_ANY
        );
        assert_eq!(
            final_put_rule(&resolved(7, true), own),
            FastFieldSite::PUT_INIT_ONLY
        );
        assert_eq!(
            final_put_rule(&resolved(9, true), own),
            FastFieldSite::PUT_NEVER
        );
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

#[cfg(test)]
mod registry_probe_tests {
    use super::*;
    use cratonvm_types::{ArrayElementType, ClassId, ObjectKind};

    /// A `FastFieldSite` for a legacy-body receiver of `(class_id, num_slots)`.
    fn legacy_site(class_id: u32, num_slots: u32) -> FastFieldSite {
        FastFieldSite {
            receiver_class_id: ClassId::new(class_id),
            num_slots,
            offset: 0,
            storage: None,
            field_index: 0,
            desc_byte: b'I',
            is_reference: false,
            final_put: FastFieldSite::PUT_ANY,
        }
    }

    /// **The test the registry elide needed and did not have.**
    ///
    /// `field_ptr_for` must refuse an address the heap does not know, even when
    /// the bytes at that address are a *perfectly matching header*. That is the
    /// only case that distinguishes "the probe ran" from "the probe was
    /// skipped": every other guard in the function — class id, slot count,
    /// compact flag — passes by construction here.
    ///
    /// # Why the header is forged rather than wild
    ///
    /// A wild pointer would make the elided build SEGFAULT, which aborts the
    /// whole test binary and takes every other result with it. A forged header
    /// in a Rust-owned allocation is readable, satisfies all three comparisons,
    /// and is not in the heap's registry — so the probe is the *only* thing
    /// that can refuse it. With the probe: `None`, and this passes. Without it:
    /// `Some(ptr)`, and this fails as an assertion rather than a crash.
    ///
    /// This is the shape any future "the fast path may skip check X" change
    /// needs: construct the input that only X rejects.
    #[test]
    fn field_ptr_for_refuses_a_matching_header_the_heap_does_not_know() {
        let zgc = ZgcRealHeap::new();
        let (class_id, num_slots) = (4321u32, 3u32);

        // A header the site would accept, in memory the heap never handed out.
        let forged = Box::new(ObjectHeader::new(
            ClassId::new(class_id),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            num_slots,
        ));
        let ptr = Box::into_raw(forged);
        let site = legacy_site(class_id, num_slots);

        // SAFETY: `ptr` is a live `Box` allocation for the length of this test.
        let header = unsafe { &*ptr };
        assert_eq!(header.class_id, site.receiver_class_id, "forge is wrong");
        assert_eq!(header.num_slots(), site.num_slots, "forge is wrong");
        assert!(
            !cratonvm_types::is_compact_object(header),
            "forge is wrong: a legacy site must meet a legacy header"
        );

        let got = field_ptr_for(&zgc, ptr as u64, &site);

        // SAFETY: reclaim the forged header; nothing retained it.
        unsafe { drop(Box::from_raw(ptr)) };

        assert!(
            got.is_none(),
            "field_ptr_for accepted an address the heap does not know. Every \
             other guard in it passes for this input by construction, so this \
             can only mean the `is_object_address` probe was removed. It is \
             not redundant with the slow path: `op_getfield` and `op_putfield` \
             both reach `load_and_forward`, whose first act is that same probe."
        );
    }

    /// The array arms deliberately do **not** probe, and that asymmetry is
    /// load-bearing rather than an oversight — so it is pinned here too.
    ///
    /// `prim_elem_ptr` is at parity with the handler it replaces:
    /// `VmHeap::get_array_element` and `VmHeap::array_length` are both reached
    /// with **no** `load_and_forward` and no membership test between the
    /// operand-stack pop and the header read. The field path is not at parity,
    /// because its handlers do reach one. A single switch used to cover both;
    /// this test records which side is which, so that flipping either one has
    /// to come here and say why.
    #[test]
    fn the_array_arm_is_at_parity_with_its_handler_and_the_field_arm_is_not() {
        // Not an execution test — a statement of the contract the two arms are
        // held to, in a place a change to either has to walk past.
        let field_handler_probes = true; // op_getfield/op_putfield -> load_and_forward
        let array_handler_probes = false; // get_array_element / array_length
        assert!(
            field_handler_probes,
            "if op_getfield stops calling load_and_forward, field_ptr_for's \
             probe may be reconsidered — until then it stays"
        );
        assert!(
            !array_handler_probes,
            "if the array handlers start validating, prim_elem_ptr must too"
        );
    }
}

#[cfg(test)]
mod static_site_tests {
    use super::*;
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    fn vm() -> Arc<SharedVm> {
        Arc::new(SharedVm::new(crate::config::VmConfig::default()))
    }

    fn thread_with_frame() -> JvmThread {
        let mut thread = JvmThread::new(ThreadId(4343), "static-site");
        thread.frames.push(Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        thread
    }

    fn static_int(declaring: ClassId, is_volatile: bool) -> ResolvedField {
        ResolvedField {
            declaring_class_id: declaring,
            field_index: 0,
            is_static: true,
            is_volatile,
            is_final: false,
            is_reference: false,
            desc_byte: b'I',
        }
    }

    /// A filled site answers `getstatic` with the current value of the static,
    /// read through `get_static_shared`, and a later store is seen.
    #[test]
    fn a_filled_site_serves_the_current_static_value() {
        if cratonvm_classloading::any_class_redefined() {
            return; // the site tables are latched off for this process
        }
        let shared = vm();
        let decl = ClassId::new(90_001);
        crate::vm::set_static_shared(&shared, decl, 0, Value::Int(42));
        let mut thread = thread_with_frame();
        // Other tests load classes concurrently and move the epochs, which
        // drops a fill; retry until one sticks.
        let mut served = false;
        for _ in 0..1000 {
            let epochs = FieldSiteCache::epochs_now();
            thread.site_tables.static_field_sites.put(
                ClassId::new(0),
                7,
                epochs,
                static_int(decl, false),
            );
            if matches!(getstatic_site_hit(&shared, &mut thread, 0, 7), Ok(true)) {
                served = true;
                break;
            }
        }
        assert!(served, "a filled static site must be served");
        assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(42)));

        crate::vm::set_static_shared(&shared, decl, 0, Value::Int(43));
        if matches!(getstatic_site_hit(&shared, &mut thread, 0, 7), Ok(true)) {
            assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(43)));
        }
    }

    /// An empty site declines without touching the stack.
    #[test]
    fn an_unfilled_site_declines() {
        let shared = vm();
        let mut thread = thread_with_frame();
        assert!(matches!(
            getstatic_site_hit(&shared, &mut thread, 0, 9),
            Ok(false)
        ));
        assert_eq!(thread.frames[0].stack.len(), 0);
    }

    /// Wave 5: the `System` screen is per VM. Two VMs that recorded different
    /// ids for `java/lang/System` each answer for their own id only (the
    /// deleted process-global latch answered every VM with the last one).
    #[test]
    fn the_system_screen_answers_per_vm() {
        use crate::runtime::interpreter::opcodes::static_may_be_system;
        use std::sync::atomic::Ordering as O;
        let a = vm();
        let b = vm();
        a.classes.system_class_id.store(70_001, O::Relaxed);
        b.classes.system_class_id.store(70_002, O::Relaxed);
        assert!(static_may_be_system(&a, ClassId::new(70_001)));
        assert!(!static_may_be_system(&a, ClassId::new(70_002)));
        assert!(static_may_be_system(&b, ClassId::new(70_002)));
        assert!(!static_may_be_system(&b, ClassId::new(70_001)));
    }

    /// Wave 5: one entry serves `putstatic` (the store lands and pops its
    /// operand) and a volatile `getstatic`; a final field declines the
    /// `putstatic` hit and leaves the operand for the full handler.
    #[test]
    fn a_filled_site_serves_putstatic_and_volatile_reads_but_no_final_store() {
        if cratonvm_classloading::any_class_redefined() {
            return; // the site tables are latched off for this process
        }
        let shared = vm();
        let decl = ClassId::new(90_003);
        crate::vm::set_static_shared(&shared, decl, 0, Value::Int(1));
        let mut thread = thread_with_frame();
        let mut served = false;
        for _ in 0..1000 {
            let epochs = FieldSiteCache::epochs_now();
            thread.site_tables.static_field_sites.put(
                ClassId::new(0),
                13,
                epochs,
                static_int(decl, true),
            );
            let _ = thread.frames[0].stack.push(Value::Int(77));
            if matches!(putstatic_site_hit(&shared, &mut thread, 0, 13), Ok(true)) {
                served = true;
                break;
            }
            let _ = thread.frames[0].stack.pop();
        }
        assert!(served, "a filled static site must serve putstatic");
        assert_eq!(
            thread.frames[0].stack.len(),
            0,
            "the store pops its operand"
        );
        assert_eq!(
            crate::vm::get_static_shared(&shared, decl, 0),
            Value::Int(77)
        );
        if matches!(getstatic_site_hit(&shared, &mut thread, 0, 13), Ok(true)) {
            assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(77)));
        }

        let mut final_field = static_int(decl, false);
        final_field.is_final = true;
        let epochs = FieldSiteCache::epochs_now();
        thread
            .site_tables
            .static_field_sites
            .put(ClassId::new(0), 14, epochs, final_field);
        let _ = thread.frames[0].stack.push(Value::Int(5));
        assert!(matches!(
            putstatic_site_hit(&shared, &mut thread, 0, 14),
            Ok(false)
        ));
        assert_eq!(
            thread.frames[0].stack.len(),
            1,
            "a declined store pops nothing"
        );
        assert_eq!(
            crate::vm::get_static_shared(&shared, decl, 0),
            Value::Int(77)
        );
    }

    /// The fill refuses a class that is not `Initialized` (here: unknown to the
    /// class manager) — for a volatile field too, which wave 5 otherwise
    /// admits — even when `field_sites` admits the site.
    #[test]
    fn fill_requires_an_initialized_class_and_a_plain_field() {
        let shared = vm();
        let mut thread = thread_with_frame();
        let decl = ClassId::new(90_002);
        for field in [static_int(decl, false), static_int(decl, true)] {
            let epochs = FieldSiteCache::epochs_now();
            thread
                .field_sites
                .put(ClassId::new(0), 11, epochs, field.clone());
            fill_static_site(&shared, &mut thread, 0, 11, &field);
            assert!(thread
                .site_tables
                .static_field_sites
                .get(ClassId::new(0), 11)
                .is_none());
        }
        // Not admitted by `field_sites` at all: never filled.
        let other = static_int(decl, false);
        fill_static_site(&shared, &mut thread, 0, 12, &other);
        assert!(thread
            .site_tables
            .static_field_sites
            .get(ClassId::new(0), 12)
            .is_none());
    }
}

#[cfg(test)]
mod i68_l7_fused_field_increment_tests {
    //! Interpreter round i2 wave 68, lane L7: the fused instance-field int
    //! increment (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT`,
    //! `fused_field_increment`). A hit stores what the unfused `aload_N; dup;
    //! getfield; <operand>; iadd | isub; putfield` stores (the wrapping int
    //! result, in the receiver's field) and leaves the operand stack as it
    //! found it; every decline writes nothing.
    use super::*;
    use crate::runtime::interpreter::site_cache::PinnedSites;

    const CP: u16 = 0x0102;

    /// The site table every test here runs against: entries a test fills
    /// are pinned, so another test thread moving the process epochs between
    /// the fill and the helper's lookup cannot turn an expected hit into a
    /// cold-site decline (wave 73, lane L7; page
    /// `i70-orch-fused-increment-unit-tests-fail-intermittently-in-the-full-suite-20261103`).
    type PinnedFast = PinnedSites<FastFieldSite>;

    /// `<load>; dup; getfield #0x0102; <operand>; <arith>; putfield #0x0102;
    /// return`.
    fn group(load: u8, operand: &[u8], arith: u8) -> Vec<u8> {
        let mut code = vec![load, 0x59, 0xb4, 0x01, 0x02];
        code.extend_from_slice(operand);
        code.extend_from_slice(&[arith, 0xb5, 0x01, 0x02, 0xb1]);
        code
    }

    /// An instance `(I)V` frame named `method` over `code`: local 0 =
    /// `recv`, local 1 = `5`, `max_locals` 3.
    fn frame(class_id: ClassId, method: &str, code: Vec<u8>, recv: Value) -> Frame {
        Frame::new(
            class_id,
            "cratonvm/test/i68l7/FieldIncrement".to_string(),
            method.to_string(),
            "(I)V".to_string(),
            None,
            code,
            vec![],
            4,
            3,
            &[recv, Value::Int(5)],
        )
    }

    /// Fill `CP` of `class_id` with `site` and pin it (re-filled by `run` /
    /// `run_mode` if the process epochs retire it); `false` when the epochs
    /// kept moving (other tests defining classes) and every fill was dropped.
    fn fill(sites: &mut PinnedFast, class_id: ClassId, site: FastFieldSite) -> bool {
        sites.pin(class_id, CP, site)
    }

    /// A legacy one-field receiver of `class` on `zgc` holding `Int(value)`,
    /// with the site `fill_site` would record for it; `None` when the
    /// allocation failed or came out compact (a layout another test
    /// registered for the id).
    fn legacy_receiver(zgc: &ZgcRealHeap, class: u32, value: i32) -> Option<(u64, FastFieldSite)> {
        let obj = zgc.try_alloc_object(ClassId::new(class), 1)?;
        let ptr = obj.as_ptr() as u64;
        // SAFETY: a fresh allocation starts with its header.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        if cratonvm_types::is_compact_object(header) {
            return None;
        }
        // SAFETY: field 0's 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                Value::Int(value),
            )
        };
        Some((
            ptr,
            FastFieldSite {
                receiver_class_id: header.class_id,
                num_slots: header.num_slots(),
                offset: u32::try_from(HEADER_SIZE).ok()?,
                storage: None,
                field_index: 0,
                desc_byte: b'I',
                is_reference: false,
                final_put: FastFieldSite::PUT_ANY,
            },
        ))
    }

    /// The legacy field's current value.
    fn legacy_value(ptr: u64) -> Option<Value> {
        // SAFETY: as in `legacy_receiver`.
        unsafe {
            cratonvm_types::read_value_checked_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *const Value
            )
        }
    }

    fn run(zgc: &ZgcRealHeap, sites: &mut PinnedFast, f: &mut Frame) -> Option<usize> {
        run_mode(zgc, sites, f, FIELD_INC_ON)
    }

    #[test]
    fn the_decoder_reads_every_operand_and_both_arithmetic_opcodes() {
        let class_id = ClassId::new(0x00ab_0681);
        // (operand bytes, the int it pushes)
        let operands: [(&[u8], i32); 8] = [
            (&[0x02], -1),
            (&[0x04], 1),
            (&[0x08], 5),
            (&[0x10, 0x80], -128),
            (&[0x11, 0x80, 0x00], -32768),
            (&[0x11, 0x03, 0xe8], 1000),
            (&[0x15, 0x01], 5),
            (&[0x1b], 5),
        ];
        for load in 0x2a..=0x2d_u8 {
            for (operand, k) in operands {
                for (arith, subtract) in [(0x60_u8, false), (0x64_u8, true)] {
                    let code = group(load, operand, arith);
                    let end = code.len() - 1;
                    let f = frame(class_id, "m", code, Value::Object(None));
                    let code_len = f.code.len() - 2;
                    assert_eq!(
                        decode_field_increment(&f, &f.code, code_len, 0, false, false),
                        Some((
                            CP,
                            usize::from(load - 0x2a),
                            crate::runtime::interpreter::IncrementOperand::Int(k),
                            subtract,
                            None,
                            end
                        )),
                        "{load:#x} {operand:?} {arith:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_decoder_refuses_every_other_shape() {
        let class_id = ClassId::new(0x00ab_0682);
        let refuses = |code: Vec<u8>| {
            let f = frame(class_id, "m", code, Value::Object(None));
            let code_len = f.code.len() - 2;
            assert_eq!(
                decode_field_increment(&f, &f.code, code_len, 0, false, false),
                None
            );
        };
        // `aload n` (0x19), `iload_0`: not the short reference load.
        refuses(group(0x19, &[0x04], 0x60));
        refuses(group(0x1a, &[0x04], 0x60));
        // No `dup`, or `dup_x1`.
        refuses(vec![
            0x2a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        refuses(vec![
            0x2a, 0x5a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        // Another arithmetic opcode (`imul`, `ladd`), another operand (`ldc`,
        // `aload_0`), an int local past `max_locals`, a non-int local.
        refuses(group(0x2a, &[0x04], 0x68));
        refuses(group(0x2a, &[0x04], 0x61));
        refuses(group(0x2a, &[0x12, 0x01], 0x60));
        refuses(group(0x2a, &[0x2a], 0x60));
        refuses(group(0x2a, &[0x15, 0xff], 0x60));
        refuses(group(0x2a, &[0x1a], 0x60));
        // A `putfield` of another pool index, a `putstatic`, a `getfield`.
        refuses(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
        ]);
        refuses(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x02, 0xb1,
        ]);
        refuses(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb4, 0x01, 0x02, 0xb1,
        ]);
        // Cut by the end of the code (the padding is never read as the tail).
        refuses(vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01]);
        refuses(vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60]);
    }

    #[test]
    fn a_legacy_receiver_is_incremented_in_place() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0683);
        let cases: [(&[u8], u8, i32, i32); 3] = [
            (&[0x04], 0x60, 41, 42),
            (&[0x10, 0x07], 0x64, 10, 3),
            (&[0x1b], 0x60, i32::MAX, i32::MAX.wrapping_add(5)),
        ];
        for (operand, arith, start, want) in cases {
            let Some((ptr, site)) = legacy_receiver(&zgc, 6881, start) else {
                return;
            };
            let mut sites = PinnedFast::new();
            if !fill(&mut sites, class_id, site) {
                return;
            }
            // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
            let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
            let code = group(0x2a, operand, arith);
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code, recv);
            f.stack.push_int(42).unwrap();
            assert_eq!(
                run(&zgc, &mut sites, &mut f),
                Some(end),
                "lands on the `return`"
            );
            assert_eq!(legacy_value(ptr), Some(Value::Int(want)));
            assert_eq!(f.stack.len(), 1, "nothing pushed or popped");
        }
    }

    #[test]
    fn a_compact_int_field_is_incremented_in_place() {
        const CLASS: u32 = 6882;
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        cratonvm_types::register_class_layout(
            zgc.layout_domain(),
            CLASS,
            std::sync::Arc::new(cratonvm_types::CompactLayout {
                field_disps: vec![8],
                is_ref: vec![false],
                field_kinds: vec![FieldStorageKind::Int],
                ref_disps: vec![],
                total_size: 16,
            }),
        );
        let Some(obj) = zgc.try_alloc_object(ClassId::new(CLASS), 1) else {
            return;
        };
        let ptr = obj.as_ptr() as u64;
        // SAFETY: a fresh allocation starts with its header.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        if !cratonvm_types::is_compact_object(header) {
            return;
        }
        // SAFETY: displacement 8 is the layout's 4-byte `Int` slot.
        let fp = unsafe { (ptr as *mut u8).add(8) };
        // SAFETY: as above.
        unsafe { store_u32(fp, 99) };
        let site = FastFieldSite {
            receiver_class_id: header.class_id,
            num_slots: header.num_slots(),
            offset: 8,
            storage: Some(FieldStorageKind::Int),
            field_index: 0,
            desc_byte: b'I',
            is_reference: false,
            final_put: FastFieldSite::PUT_ANY,
        };
        let class_id = ClassId::new(0x00ab_0684);
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            return;
        }
        let mut f = frame(
            class_id,
            "m",
            group(0x2a, &[0x04], 0x64),
            Value::Object(Some(obj)),
        );
        assert!(run(&zgc, &mut sites, &mut f).is_some());
        // SAFETY: as above.
        assert_eq!(unsafe { load_u32(fp) } as i32, 98);
    }

    #[test]
    fn every_decline_writes_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0685);
        let Some((ptr, site)) = legacy_receiver(&zgc, 6883, 7) else {
            return;
        };
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let code = || group(0x2a, &[0x04], 0x60);
        let unchanged = || assert_eq!(legacy_value(ptr), Some(Value::Int(7)));

        // A cold site: the unfused `getfield` resolves and fills.
        let mut empty = PinnedFast::new();
        assert_eq!(
            run(&zgc, &mut empty, &mut frame(class_id, "m", code(), recv)),
            None
        );
        unchanged();

        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            return;
        }
        // A null receiver: the unfused `getfield` throws the NPE.
        let mut null = frame(class_id, "m", code(), Value::Object(None));
        assert_eq!(run(&zgc, &mut sites, &mut null), None);
        // A non-reference local 0.
        let mut int0 = frame(class_id, "m", code(), Value::Int(3));
        assert_eq!(run(&zgc, &mut sites, &mut int0), None);
        // A watch armed, or the field-phase instrument.
        let mut f = frame(class_id, "m", code(), recv);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_field_increment_on(
                &zgc,
                &mut sites,
                &mut f,
                code_len,
                0,
                FIELD_INC_ON,
                Some(true)
            ),
            None
        );
        assert_eq!(
            fused_field_increment_on(
                &zgc,
                &mut sites,
                &mut f,
                code_len,
                0,
                FIELD_INC_ON | FIELD_INC_PHASES,
                Some(false)
            ),
            None
        );
        unchanged();

        // Not an `I` field (a narrow field needs its narrowing store).
        for desc in [b'S', b'B', b'C', b'Z', b'J', b'F', b'L'] {
            let mut other = PinnedFast::new();
            if fill(
                &mut other,
                class_id,
                FastFieldSite {
                    desc_byte: desc,
                    ..site
                },
            ) {
                assert_eq!(run(&zgc, &mut other, &mut f), None, "{}", desc as char);
            }
        }
        unchanged();
        // A final field: never from another class, only from `<init>` of
        // its own.
        let mut never = PinnedFast::new();
        let never_site = FastFieldSite {
            final_put: FastFieldSite::PUT_NEVER,
            ..site
        };
        if fill(&mut never, class_id, never_site) {
            assert_eq!(run(&zgc, &mut never, &mut f), None);
        }
        unchanged();
        let mut init_only = PinnedFast::new();
        let init_site = FastFieldSite {
            final_put: FastFieldSite::PUT_INIT_ONLY,
            ..site
        };
        if fill(&mut init_only, class_id, init_site) {
            assert_eq!(run(&zgc, &mut init_only, &mut f), None);
            unchanged();
            let mut ctor = frame(class_id, "<init>", code(), recv);
            assert!(run(&zgc, &mut init_only, &mut ctor).is_some());
            assert_eq!(legacy_value(ptr), Some(Value::Int(8)));
        }
    }

    #[test]
    fn a_receiver_of_another_shape_declines() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0686);
        let Some((ptr, site)) = legacy_receiver(&zgc, 6884, 7) else {
            return;
        };
        let mut sites = PinnedFast::new();
        // The site names another receiver class: `field_ptr_for` refuses.
        let other = FastFieldSite {
            receiver_class_id: ClassId::new(6885),
            ..site
        };
        if !fill(&mut sites, class_id, other) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let mut f = frame(class_id, "m", group(0x2a, &[0x04], 0x60), recv);
        assert_eq!(run(&zgc, &mut sites, &mut f), None);
        assert_eq!(legacy_value(ptr), Some(Value::Int(7)));
    }

    // ── Wave 69, lane L7: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE` and
    // `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE` ──

    /// The wave-69 widened mode.
    const WIDE: u8 = FIELD_INC_ON | FIELD_INC_WIDE;

    /// `<load>; dup; getfield #0x0102; <operand>; <arith>; <tail...>;
    /// putfield #0x0102; return`.
    fn wide_group(load: u8, operand: &[u8], arith: u8, tail: &[u8]) -> Vec<u8> {
        let mut code = vec![load, 0x59, 0xb4, 0x01, 0x02];
        code.extend_from_slice(operand);
        code.push(arith);
        code.extend_from_slice(tail);
        code.extend_from_slice(&[0xb5, 0x01, 0x02, 0xb1]);
        code
    }

    fn run_mode(
        zgc: &ZgcRealHeap,
        sites: &mut PinnedFast,
        f: &mut Frame,
        mode: u8,
    ) -> Option<usize> {
        let code_len = f.code.len() - 2;
        sites.run(|cache| fused_field_increment_on(zgc, cache, f, code_len, 0, mode, Some(false)))
    }

    /// A compact one-field receiver of `class` whose only field has storage
    /// `kind` at displacement 8, with the site `fill_site` would record for
    /// it under descriptor `desc`; `None` when the allocation failed or came
    /// out legacy.
    fn compact_receiver(
        zgc: &ZgcRealHeap,
        class: u32,
        kind: FieldStorageKind,
        desc: u8,
    ) -> Option<(ObjectRef, *mut u8, FastFieldSite)> {
        cratonvm_types::register_class_layout(
            zgc.layout_domain(),
            class,
            std::sync::Arc::new(cratonvm_types::CompactLayout {
                field_disps: vec![8],
                is_ref: vec![false],
                field_kinds: vec![kind],
                ref_disps: vec![],
                total_size: 16,
            }),
        );
        let obj = zgc.try_alloc_object(ClassId::new(class), 1)?;
        let ptr = obj.as_ptr() as u64;
        // SAFETY: a fresh allocation starts with its header.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        if !cratonvm_types::is_compact_object(header) {
            return None;
        }
        // SAFETY: displacement 8 is the layout's slot for the one field.
        let fp = unsafe { (ptr as *mut u8).add(8) };
        let site = FastFieldSite {
            receiver_class_id: header.class_id,
            num_slots: header.num_slots(),
            offset: 8,
            storage: Some(kind),
            field_index: 0,
            desc_byte: desc,
            is_reference: false,
            final_put: FastFieldSite::PUT_ANY,
        };
        Some((obj, fp, site))
    }

    #[test]
    fn a_compact_narrow_field_wraps_as_the_unfused_group() {
        // (class, storage, descriptor, conversion, stored bits, operand,
        //  arith, the field's value after, as the quickened getfield reads it)
        let cases: [(u32, FieldStorageKind, u8, u8, u16, &[u8], u8, i32); 6] = [
            (
                6901,
                FieldStorageKind::Byte,
                b'B',
                0x91,
                0x7f,
                &[0x04],
                0x60,
                -128,
            ),
            (
                6902,
                FieldStorageKind::Byte,
                b'B',
                0x91,
                0x80,
                &[0x04],
                0x64,
                127,
            ),
            (
                6903,
                FieldStorageKind::Char,
                b'C',
                0x92,
                0,
                &[0x04],
                0x64,
                65535,
            ),
            (
                6904,
                FieldStorageKind::Char,
                b'C',
                0x92,
                0xffff,
                &[0x04],
                0x60,
                0,
            ),
            (
                6905,
                FieldStorageKind::Short,
                b'S',
                0x93,
                0x7fff,
                &[0x04],
                0x60,
                -32768,
            ),
            (
                6906,
                FieldStorageKind::Short,
                b'S',
                0x93,
                5,
                &[0x11, 0x03, 0xe8],
                0x64,
                -995,
            ),
        ];
        for (class, kind, desc, conv, bits, operand, arith, want) in cases {
            let zgc = ZgcRealHeap::with_capacity(1 << 20);
            let Some((obj, fp, site)) = compact_receiver(&zgc, class, kind, desc) else {
                continue;
            };
            let class_id = ClassId::new(0x00ab_0690 + class);
            let mut sites = PinnedFast::new();
            if !fill(&mut sites, class_id, site) {
                continue;
            }
            let read = || -> i32 {
                // SAFETY: `fp` is the layout's slot of the live object, of
                // the width its storage kind names.
                match kind {
                    FieldStorageKind::Byte => i32::from((unsafe { load_u8(fp) }) as i8),
                    FieldStorageKind::Char => i32::from(unsafe { load_u16(fp) }),
                    _ => i32::from((unsafe { load_u16(fp) }) as i16),
                }
            };
            // SAFETY: as above.
            unsafe {
                if kind == FieldStorageKind::Byte {
                    store_u8(fp, bits as u8);
                } else {
                    store_u16(fp, bits);
                }
            }
            let before = read();
            let code = wide_group(0x2a, operand, arith, &[conv]);
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code, Value::Object(Some(obj)));
            // Without the wide switch the wave-68 rule (`I` only) declines.
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
            assert_eq!(read(), before, "a decline writes nothing");
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), Some(end));
            assert_eq!(
                read(),
                want,
                "{} {conv:#x} {bits:#x} {arith:#x}",
                desc as char
            );
            assert_eq!(f.stack.len(), 0, "nothing pushed or popped");
        }
    }

    #[test]
    fn a_legacy_narrow_field_is_stored_narrowed() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0697);
        // (descriptor, conversion, current cell, arith, want)
        let cases: [(u8, u8, Value, u8, i32); 4] = [
            (b'B', 0x91, Value::Int(127), 0x60, -128),
            (b'C', 0x92, Value::Int(0), 0x64, 65535),
            (b'S', 0x93, Value::Int(-32768), 0x64, 32767),
            (b'B', 0x91, Value::Object(None), 0x64, -1),
        ];
        for (desc, conv, current, arith, want) in cases {
            let Some((ptr, site)) = legacy_receiver(&zgc, 6907, 0) else {
                return;
            };
            // SAFETY: field 0's 16-byte cell of the live legacy object.
            unsafe {
                cratonvm_types::write_value_atomic(
                    (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                    current,
                )
            };
            let mut sites = PinnedFast::new();
            if !fill(
                &mut sites,
                class_id,
                FastFieldSite {
                    desc_byte: desc,
                    ..site
                },
            ) {
                return;
            }
            // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
            let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
            let code = wide_group(0x2a, &[0x04], arith, &[conv]);
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code, recv);
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), Some(end));
            assert_eq!(
                legacy_value(ptr),
                Some(Value::Int(want)),
                "{}",
                desc as char
            );
        }
    }

    #[test]
    fn a_long_field_carries_and_borrows() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        // Compact: `lconst_1; ladd` over the 32-bit carry and the top.
        if let Some((obj, fp, site)) = compact_receiver(&zgc, 6908, FieldStorageKind::Long, b'J') {
            let class_id = ClassId::new(0x00ab_0698);
            let mut sites = PinnedFast::new();
            if fill(&mut sites, class_id, site) {
                // (stored, operand, arith, want)
                let cases: [(i64, u8, u8, i64); 4] = [
                    (0xffff_ffff, 0x0a, 0x61, 0x1_0000_0000),
                    (i64::MAX, 0x0a, 0x61, i64::MIN),
                    (i64::MIN, 0x0a, 0x65, i64::MAX),
                    (42, 0x09, 0x65, 42),
                ];
                for (stored, operand, arith, want) in cases {
                    // SAFETY: displacement 8 is the layout's 8-byte `Long` slot.
                    unsafe { store_u64(fp, stored as u64) };
                    let code = wide_group(0x2a, &[operand], arith, &[]);
                    let end = code.len() - 1;
                    let mut f = frame(class_id, "m", code, Value::Object(Some(obj)));
                    assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
                    // SAFETY: as above.
                    assert_eq!(unsafe { load_u64(fp) } as i64, stored);
                    assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), Some(end));
                    // SAFETY: as above.
                    assert_eq!(unsafe { load_u64(fp) } as i64, want, "{stored:#x}");
                }
                // An int operation on a `J` field, a long one on an `I` field.
                let mut f = frame(
                    class_id,
                    "m",
                    group(0x2a, &[0x04], 0x60),
                    Value::Object(Some(obj)),
                );
                assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), None);
            }
        }
        // Legacy: a `Long` cell, and an `Int` the quickened read widens.
        let class_id = ClassId::new(0x00ab_0699);
        for (current, want) in [
            (Value::Long(0x7fff_ffff), 0x8000_0000_i64),
            (Value::Int(-1), 0),
            (Value::Object(None), 1),
        ] {
            let Some((ptr, site)) = legacy_receiver(&zgc, 6909, 0) else {
                return;
            };
            // SAFETY: field 0's 16-byte cell of the live legacy object.
            unsafe {
                cratonvm_types::write_value_atomic(
                    (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                    current,
                )
            };
            let mut sites = PinnedFast::new();
            if !fill(
                &mut sites,
                class_id,
                FastFieldSite {
                    desc_byte: b'J',
                    ..site
                },
            ) {
                return;
            }
            // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
            let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
            let mut f = frame(class_id, "m", wide_group(0x2a, &[0x0a], 0x61, &[]), recv);
            assert!(run_mode(&zgc, &mut sites, &mut f, WIDE).is_some());
            assert_eq!(legacy_value(ptr), Some(Value::Long(want)));
            // The same long group on an `I` field declines.
            let mut int_sites = PinnedFast::new();
            if fill(&mut int_sites, class_id, site) {
                assert_eq!(run_mode(&zgc, &mut int_sites, &mut f, WIDE), None);
                assert_eq!(legacy_value(ptr), Some(Value::Long(want)));
            }
        }
    }

    #[test]
    fn a_long_local_operand_is_read_only_when_marked() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_069a);
        let Some((ptr, site)) = legacy_receiver(&zgc, 6910, 0) else {
            return;
        };
        // SAFETY: field 0's 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                Value::Long(10),
            )
        };
        let mut sites = PinnedFast::new();
        if !fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: b'J',
                ..site
            },
        ) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        // An instance `(J)V` frame: local 0 the receiver, locals 1-2 a long.
        let mut f = Frame::new(
            class_id,
            "cratonvm/test/i68l7/FieldIncrement".to_string(),
            "m".to_string(),
            "(J)V".to_string(),
            None,
            wide_group(0x2a, &[0x1f], 0x65, &[]),
            vec![],
            6,
            3,
            &[recv, Value::Long(3)],
        );
        let hit = run_mode(&zgc, &mut sites, &mut f, WIDE);
        if f.local_is_long_marked(1) {
            assert!(hit.is_some());
            assert_eq!(legacy_value(ptr), Some(Value::Long(7)));
        } else {
            assert_eq!(hit, None);
            assert_eq!(legacy_value(ptr), Some(Value::Long(10)));
        }
        // `lload_1` over the frame whose local 1 is an int declines.
        let mut g = frame(class_id, "m", wide_group(0x2a, &[0x1f], 0x65, &[]), recv);
        let before = legacy_value(ptr);
        assert_eq!(run_mode(&zgc, &mut sites, &mut g, WIDE), None);
        assert_eq!(legacy_value(ptr), before);
    }

    #[test]
    fn a_conversion_not_the_fields_own_declines() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_069b);
        let Some((ptr, site)) = legacy_receiver(&zgc, 6911, 7) else {
            return;
        };
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        for (desc, conv) in [
            (b'B', 0x92_u8),
            (b'B', 0x93),
            (b'C', 0x91),
            (b'S', 0x92),
            (b'I', 0x91),
            (b'Z', 0x91),
            (b'J', 0x91),
        ] {
            let mut sites = PinnedFast::new();
            if !fill(
                &mut sites,
                class_id,
                FastFieldSite {
                    desc_byte: desc,
                    ..site
                },
            ) {
                continue;
            }
            let mut f = frame(
                class_id,
                "m",
                wide_group(0x2a, &[0x04], 0x60, &[conv]),
                recv,
            );
            assert_eq!(
                run_mode(&zgc, &mut sites, &mut f, WIDE),
                None,
                "{} {conv:#x}",
                desc as char
            );
        }
        // A narrow field without its conversion (what javac never emits).
        for desc in [b'B', b'C', b'S', b'Z'] {
            let mut sites = PinnedFast::new();
            if fill(
                &mut sites,
                class_id,
                FastFieldSite {
                    desc_byte: desc,
                    ..site
                },
            ) {
                let mut f = frame(class_id, "m", group(0x2a, &[0x04], 0x60), recv);
                assert_eq!(
                    run_mode(&zgc, &mut sites, &mut f, WIDE),
                    None,
                    "{}",
                    desc as char
                );
            }
        }
        assert_eq!(legacy_value(ptr), Some(Value::Int(7)));
    }

    /// Groups the early-decline screen must agree with the body on.
    fn screen_corpus() -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let operands: [&[u8]; 12] = [
            &[0x02],
            &[0x04],
            &[0x10, 0x80],
            &[0x11, 0x03, 0xe8],
            &[0x15, 0x01],
            &[0x15, 0x09],
            &[0x1b],
            &[0x09],
            &[0x0a],
            &[0x16, 0x01],
            &[0x1f],
            &[0x12, 0x01],
        ];
        for operand in operands {
            for arith in [0x60_u8, 0x64, 0x61, 0x65, 0x68] {
                for tail in [&[][..], &[0x91][..], &[0x92][..], &[0x93][..], &[0x88][..]] {
                    out.push(wide_group(0x2a, operand, arith, tail));
                }
            }
        }
        // A `dup_x2` where the operand would be (never fused).
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5b, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        // The value forms (wave 71, `FIELD_INC_VALUE`): post / pre over an
        // int, a long and a byte group.
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5a, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0x5a, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5d, 0x0a, 0x61, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x0a, 0x61, 0x5d, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5a, 0x04, 0x60, 0x91, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0x91, 0x5a, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        // A value form whose duplicate is the other width.
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0x5d, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        // Another `putfield` index, a `putstatic`, cut short.
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
        ]);
        out.push(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x02, 0xb1,
        ]);
        out.push(vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01]);
        out
    }

    #[test]
    fn the_early_decline_screen_changes_no_answer() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_069c);
        let Some((ptr, site)) = legacy_receiver(&zgc, 6912, 41) else {
            return;
        };
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let reset = || {
            // SAFETY: field 0's 16-byte cell of the live legacy object.
            unsafe {
                cratonvm_types::write_value_atomic(
                    (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                    Value::Int(41),
                )
            }
        };
        for desc in [b'I', b'B', b'C', b'S', b'J'] {
            let mut sites = PinnedFast::new();
            if !fill(
                &mut sites,
                class_id,
                FastFieldSite {
                    desc_byte: desc,
                    ..site
                },
            ) {
                continue;
            }
            for code in screen_corpus() {
                let mut f = frame(class_id, "m", code.clone(), recv);
                for base in [FIELD_INC_ON, WIDE, WIDE | FIELD_INC_VALUE] {
                    reset();
                    let plain = run_mode(&zgc, &mut sites, &mut f, base);
                    let plain_value = legacy_value(ptr);
                    reset();
                    let screened =
                        run_mode(&zgc, &mut sites, &mut f, base | FIELD_INC_EARLY_DECLINE);
                    assert_eq!(
                        (plain, plain_value),
                        (screened, legacy_value(ptr)),
                        "{} {base:#x} {code:x?}",
                        desc as char
                    );
                    if plain.is_some() {
                        let code_len = f.code.len() - 2;
                        let wide = base & FIELD_INC_WIDE != 0;
                        let value = base & FIELD_INC_VALUE != 0;
                        assert!(field_increment_screen(
                            &f.code, code_len, 0, wide, false, value, false
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn the_screen_refuses_on_bytes_alone() {
        let class_id = ClassId::new(0x00ab_069d);
        let screen = |code: Vec<u8>, wide: bool| {
            let f = frame(class_id, "m", code, Value::Object(None));
            let code_len = f.code.len() - 2;
            field_increment_screen(&f.code, code_len, 0, wide, false, false, false)
        };
        assert!(screen(group(0x2a, &[0x04], 0x60), false));
        assert!(
            screen(group(0x2d, &[0x15, 0x09], 0x64), false),
            "the local is not read"
        );
        // A `dup_x2` after the getfield, `b++` without the wide switch, `l +=
        // 1` likewise.
        assert!(!screen(
            vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5b, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1],
            true
        ));
        assert!(!screen(wide_group(0x2a, &[0x04], 0x60, &[0x91]), false));
        assert!(screen(wide_group(0x2a, &[0x04], 0x60, &[0x91]), true));
        assert!(!screen(wide_group(0x2a, &[0x0a], 0x61, &[]), false));
        assert!(screen(wide_group(0x2a, &[0x0a], 0x61, &[]), true));
        // `aload n`, another index, cut short.
        assert!(!screen(group(0x19, &[0x04], 0x60), true));
        assert!(!screen(
            vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1],
            true
        ));
        assert!(!screen(
            vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01],
            true
        ));
    }

    #[test]
    fn the_mode_byte_packs_the_switches() {
        assert_eq!(
            field_increment_mode(false, true, true, true, true, true, true),
            0
        );
        assert_eq!(
            field_increment_mode(true, false, false, false, false, false, false),
            FIELD_INC_ON
        );
        assert_eq!(
            field_increment_mode(true, true, true, true, true, false, false),
            FIELD_INC_ON
                | FIELD_INC_CENSUS
                | FIELD_INC_PHASES
                | FIELD_INC_WIDE
                | FIELD_INC_EARLY_DECLINE
        );
    }

    // ── Wave 70, lane L7: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP` ──

    /// `<load>; <load>; getfield #0x0102; <operand>; <arith>; <tail...>;
    /// putfield #<put_lo>; return` (`count = count + k`).
    fn no_dup_group(load: u8, operand: &[u8], arith: u8, tail: &[u8], put_lo: u8) -> Vec<u8> {
        let mut code = vec![load, load, 0xb4, 0x01, 0x02];
        code.extend_from_slice(operand);
        code.push(arith);
        code.extend_from_slice(tail);
        code.extend_from_slice(&[0xb5, 0x01, put_lo, 0xb1]);
        code
    }

    #[test]
    fn the_mode_byte_carries_the_no_dup_bit() {
        assert_eq!(
            field_increment_mode(false, false, false, false, false, true, false),
            0
        );
        assert_eq!(
            field_increment_mode(true, false, false, false, false, true, false),
            FIELD_INC_ON | FIELD_INC_NO_DUP
        );
        // The bit is its own: no other switch's bit is shared with it.
        for other in [
            FIELD_INC_CENSUS,
            FIELD_INC_PHASES,
            FIELD_INC_WIDE,
            FIELD_INC_EARLY_DECLINE,
        ] {
            assert_eq!(other & FIELD_INC_NO_DUP, 0);
        }
        assert_eq!(FIELD_INC_ON & FIELD_INC_NO_DUP, 0);
    }

    #[test]
    fn the_decoder_takes_the_repeated_load_only_under_no_dup() {
        let class_id = ClassId::new(0x00ab_0701);
        for load in 0x2a..=0x2d_u8 {
            for (arith, subtract) in [(0x60_u8, false), (0x64_u8, true)] {
                let code = no_dup_group(load, &[0x04], arith, &[], 0x02);
                let end = code.len() - 1;
                let f = frame(class_id, "m", code, Value::Object(None));
                let code_len = f.code.len() - 2;
                assert_eq!(
                    decode_field_increment(&f, &f.code, code_len, 0, false, false),
                    None
                );
                assert_eq!(
                    decode_field_increment(&f, &f.code, code_len, 0, false, true),
                    Some((
                        CP,
                        usize::from(load - 0x2a),
                        crate::runtime::interpreter::IncrementOperand::Int(1),
                        subtract,
                        None,
                        end
                    )),
                    "{load:#x} {arith:#x}"
                );
            }
        }
        let refuses = |code: Vec<u8>| {
            let f = frame(class_id, "m", code, Value::Object(None));
            let code_len = f.code.len() - 2;
            assert_eq!(
                decode_field_increment(&f, &f.code, code_len, 0, true, true),
                None
            );
        };
        // Two different locals (`this.a = other.a + 1` is not one receiver).
        refuses(vec![
            0x2a, 0x2b, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        // `this.a = this.b + 1`: two different pool indexes.
        refuses(no_dup_group(0x2a, &[0x04], 0x60, &[], 0x03));
        // `aload n` twice: not the short load.
        refuses(vec![
            0x19, 0x19, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
        ]);
        // The dup form is still taken with the bit set.
        let f = frame(
            class_id,
            "m",
            group(0x2a, &[0x04], 0x60),
            Value::Object(None),
        );
        let code_len = f.code.len() - 2;
        assert!(decode_field_increment(&f, &f.code, code_len, 0, false, true).is_some());
    }

    #[test]
    fn the_no_dup_group_stores_what_the_unfused_group_stores() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0702);
        // (current, operand, arith, want)
        let cases: [(i32, &[u8], u8, i32); 4] = [
            (7, &[0x04], 0x60, 8),
            (i32::MAX, &[0x04], 0x60, i32::MIN),
            (i32::MIN, &[0x04], 0x64, i32::MAX),
            (10, &[0x1b], 0x64, 5),
        ];
        for (current, operand, arith, want) in cases {
            let Some((ptr, site)) = legacy_receiver(&zgc, 7002, current) else {
                return;
            };
            let mut sites = PinnedFast::new();
            if !fill(&mut sites, class_id, site) {
                return;
            }
            // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
            let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
            let code = no_dup_group(0x2a, operand, arith, &[], 0x02);
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code, recv);
            // Without the bit the prefix is not the helper's: a decline.
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
            assert_eq!(
                legacy_value(ptr),
                Some(Value::Int(current)),
                "a decline writes nothing"
            );
            for extra in [0, FIELD_INC_EARLY_DECLINE] {
                // SAFETY: field 0's 16-byte cell of the live legacy object.
                unsafe {
                    cratonvm_types::write_value_atomic(
                        (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                        Value::Int(current),
                    )
                };
                let mode = FIELD_INC_ON | FIELD_INC_NO_DUP | extra;
                assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), Some(end));
                assert_eq!(
                    legacy_value(ptr),
                    Some(Value::Int(want)),
                    "{current} {arith:#x}"
                );
                assert_eq!(f.stack.len(), 0, "nothing pushed or popped");
            }
        }
    }

    #[test]
    fn the_no_dup_group_declines_as_the_dup_group_declines() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0703);
        let mode = FIELD_INC_ON | FIELD_INC_NO_DUP;
        // A null receiver: nothing written, the unfused `getfield` throws.
        let mut sites = PinnedFast::new();
        let code = no_dup_group(0x2a, &[0x04], 0x60, &[], 0x02);
        let mut f = frame(class_id, "m", code, Value::Object(None));
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), None);
        let Some((ptr, site)) = legacy_receiver(&zgc, 7003, 41) else {
            return;
        };
        if !fill(&mut sites, class_id, site) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        // Two fields (`a = b + 1`), a watch armed, the field-phase instrument,
        // a narrow conversion without the wide switch.
        let mut two = frame(
            class_id,
            "m",
            no_dup_group(0x2a, &[0x04], 0x60, &[], 0x03),
            recv,
        );
        assert_eq!(run_mode(&zgc, &mut sites, &mut two, mode), None);
        assert_eq!(
            run_mode(&zgc, &mut sites, &mut two, mode | FIELD_INC_EARLY_DECLINE),
            None
        );
        let mut f = frame(
            class_id,
            "m",
            no_dup_group(0x2a, &[0x04], 0x60, &[], 0x02),
            recv,
        );
        let code_len = f.code.len() - 2;
        let watched =
            fused_field_increment_on(&zgc, &mut sites, &mut f, code_len, 0, mode, Some(true));
        assert_eq!(watched, None);
        assert_eq!(
            run_mode(&zgc, &mut sites, &mut f, mode | FIELD_INC_PHASES),
            None
        );
        let code = no_dup_group(0x2a, &[0x04], 0x60, &[0x91], 0x02);
        let mut narrow = frame(class_id, "m", code, recv);
        assert_eq!(run_mode(&zgc, &mut sites, &mut narrow, mode), None);
        assert_eq!(legacy_value(ptr), Some(Value::Int(41)));
    }

    #[test]
    fn the_screen_takes_the_repeated_load_only_under_no_dup() {
        let class_id = ClassId::new(0x00ab_0704);
        let screen = |code: Vec<u8>, no_dup: bool| {
            let f = frame(class_id, "m", code, Value::Object(None));
            let code_len = f.code.len() - 2;
            field_increment_screen(&f.code, code_len, 0, false, no_dup, false, false)
        };
        assert!(!screen(no_dup_group(0x2b, &[0x04], 0x60, &[], 0x02), false));
        assert!(screen(no_dup_group(0x2b, &[0x04], 0x60, &[], 0x02), true));
        assert!(!screen(no_dup_group(0x2b, &[0x04], 0x60, &[], 0x03), true));
        assert!(!screen(
            vec![0x2a, 0x2b, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1],
            true
        ));
        assert!(screen(group(0x2a, &[0x04], 0x60), true));
    }

    // ── Round i3 wave 71, lane L7: the loop's no-dup screen and
    // `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE` ──

    #[test]
    fn the_no_dup_loop_screen_passes_one_field_and_refuses_two() {
        let screen = |code: Vec<u8>| no_dup_group_may_fuse(&code[..], 0, false);
        // One field: every operand form, both operations, the narrowing and
        // long shapes (the screen takes the widest rule).
        let operands: [&[u8]; 5] = [
            &[0x04],
            &[0x10, 0x80],
            &[0x11, 0x03, 0xe8],
            &[0x15, 0x01],
            &[0x1b],
        ];
        for operand in operands {
            for arith in [0x60_u8, 0x64] {
                assert!(screen(no_dup_group(0x2a, operand, arith, &[], 0x02)));
                assert!(screen(no_dup_group(0x2a, operand, arith, &[0x91], 0x02)));
                // `this.a = this.b + k`: the `putfield` names another index.
                assert!(!screen(no_dup_group(0x2a, operand, arith, &[], 0x03)));
                assert!(!screen(no_dup_group(0x2a, operand, arith, &[0x91], 0x03)));
            }
        }
        assert!(screen(no_dup_group(0x2b, &[0x0a], 0x61, &[], 0x02)));
        assert!(screen(no_dup_group(0x2b, &[0x16, 0x01], 0x65, &[], 0x02)));
        assert!(!screen(no_dup_group(0x2b, &[0x0a], 0x61, &[], 0x03)));
        // `this.a = this.b + this.c` (the operand is a load), a `putstatic`,
        // and a group cut by the end of the code.
        assert!(!screen(no_dup_group(0x2a, &[0x2a], 0x60, &[], 0x02)));
        // The arithmetic opcode is the helper's to check (`imul` passes the
        // loop screen and the decoder refuses it).
        assert!(screen(no_dup_group(0x2a, &[0x04], 0x68, &[], 0x02)));
        assert!(!screen(vec![
            0x2a, 0x2a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x02, 0xb1
        ]));
        assert!(!screen(vec![
            0x2a, 0x2a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01
        ]));
        assert!(!screen(vec![0x2a, 0x2a, 0xb4, 0x01, 0x02]));
    }

    #[test]
    fn the_no_dup_loop_screen_never_refuses_what_the_helper_fuses() {
        let class_id = ClassId::new(0x00ab_0711);
        for code in screen_corpus() {
            // The corpus is `dup`-prefixed; the same groups opened
            // `aload_0; aload_0`.
            let mut code = code;
            code[1] = code[0];
            let f = frame(class_id, "m", code.clone(), Value::Object(None));
            let code_len = f.code.len() - 2;
            for wide in [false, true] {
                if decode_field_increment(&f, &f.code, code_len, 0, wide, true).is_some() {
                    assert!(no_dup_group_may_fuse(&code[..], 0, false), "{code:x?}");
                }
            }
        }
    }

    #[test]
    fn the_mode_byte_carries_the_value_bit() {
        assert_eq!(
            field_increment_mode(false, false, false, false, false, false, true),
            0
        );
        assert_eq!(
            field_increment_mode(true, false, false, false, false, false, true),
            FIELD_INC_ON | FIELD_INC_VALUE
        );
        for other in [
            FIELD_INC_CENSUS,
            FIELD_INC_PHASES,
            FIELD_INC_WIDE,
            FIELD_INC_EARLY_DECLINE,
            FIELD_INC_NO_DUP,
            FIELD_INC_ON,
        ] {
            assert_eq!(other & FIELD_INC_VALUE, 0);
        }
    }

    /// `aload_0; dup; getfield #0x0102; [dup_x1 | dup2_x1]; <operand>;
    /// <arith>; <tail...>; putfield #0x0102; return`: the post-increment
    /// whose old value is used (`post` is the duplicate opcode).
    fn post_group(post: u8, operand: &[u8], arith: u8, tail: &[u8]) -> Vec<u8> {
        let mut code = vec![0x2a, 0x59, 0xb4, 0x01, 0x02, post];
        code.extend_from_slice(operand);
        code.push(arith);
        code.extend_from_slice(tail);
        code.extend_from_slice(&[0xb5, 0x01, 0x02, 0xb1]);
        code
    }

    /// The pre-increment whose new value is used: `tail` ends with the
    /// duplicate (`dup_x1` / `dup2_x1`).
    fn pre_group(operand: &[u8], arith: u8, tail: &[u8]) -> Vec<u8> {
        wide_group(0x2a, operand, arith, tail)
    }

    #[test]
    fn the_decoder_takes_the_value_forms_only_under_the_bit() {
        let class_id = ClassId::new(0x00ab_0712);
        type Use = FieldIncrementUse;
        let int1 = crate::runtime::interpreter::IncrementOperand::Int(1);
        let long1 = crate::runtime::interpreter::IncrementOperand::Long(1);
        // (code, wide needed, operand, conversion, use)
        let cases: Vec<(Vec<u8>, bool, _, Option<u8>, Use)> = vec![
            (
                post_group(0x5a, &[0x04], 0x60, &[]),
                false,
                int1,
                None,
                Use::Old,
            ),
            (
                pre_group(&[0x04], 0x60, &[0x5a]),
                false,
                int1,
                None,
                Use::New,
            ),
            (
                post_group(0x5a, &[0x04], 0x60, &[0x91]),
                true,
                int1,
                Some(0x91),
                Use::Old,
            ),
            (
                pre_group(&[0x04], 0x60, &[0x93, 0x5a]),
                true,
                int1,
                Some(0x93),
                Use::New,
            ),
            (
                post_group(0x5d, &[0x0a], 0x61, &[]),
                true,
                long1,
                None,
                Use::Old,
            ),
            (
                pre_group(&[0x0a], 0x61, &[0x5d]),
                true,
                long1,
                None,
                Use::New,
            ),
        ];
        for (code, needs_wide, operand, narrow, used) in cases {
            let end = code.len() - 1;
            let f = frame(class_id, "m", code.clone(), Value::Object(None));
            let code_len = f.code.len() - 2;
            let decode = |wide: bool, value: bool| {
                decode_field_increment_used(&f, &f.code, code_len, 0, wide, false, value, None)
            };
            assert_eq!(decode(true, false), None, "no bit: {code:x?}");
            if needs_wide {
                assert_eq!(decode(false, true), None, "no wide: {code:x?}");
            }
            assert_eq!(
                decode(true, true),
                Some((CP, 0, operand, false, narrow, end, used)),
                "{code:x?}"
            );
            // The early-decline screen passes what the decoder takes.
            assert!(field_increment_screen(
                &f.code, code_len, 0, true, false, true, false
            ));
            assert!(!field_increment_screen(
                &f.code, code_len, 0, true, false, false, false
            ));
        }
        // The statement form is still a statement under the bit.
        let f = frame(
            class_id,
            "m",
            group(0x2a, &[0x04], 0x60),
            Value::Object(None),
        );
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_field_increment_used(&f, &f.code, code_len, 0, true, false, true, None)
                .map(|d| d.6),
            Some(Use::Statement)
        );
        let refuses = |code: Vec<u8>| {
            let f = frame(class_id, "m", code.clone(), Value::Object(None));
            let code_len = f.code.len() - 2;
            assert_eq!(
                decode_field_increment_used(&f, &f.code, code_len, 0, true, true, true, None),
                None,
                "{code:x?}"
            );
        };
        // A duplicate of the other width, both duplicates, `dup_x2`, `dup`.
        refuses(post_group(0x5d, &[0x04], 0x60, &[]));
        refuses(post_group(0x5a, &[0x0a], 0x61, &[]));
        refuses(pre_group(&[0x04], 0x60, &[0x5d]));
        refuses(pre_group(&[0x0a], 0x61, &[0x5a]));
        refuses(post_group(0x5a, &[0x04], 0x60, &[0x5a]));
        refuses(post_group(0x5b, &[0x04], 0x60, &[]));
        refuses(pre_group(&[0x04], 0x60, &[0x59]));
        // The value forms follow `dup` only, never the `aload_N; aload_N`
        // prefix (not measured; `x = count = count + 1` is rare).
        let mut no_dup_pre = pre_group(&[0x04], 0x60, &[0x5a]);
        no_dup_pre[1] = 0x2a;
        refuses(no_dup_pre);
        // Another index after the duplicate.
        refuses(vec![
            0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5a, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
        ]);
    }

    #[test]
    fn a_value_form_hit_leaves_what_the_unfused_group_leaves() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0713);
        let value = FIELD_INC_ON | FIELD_INC_VALUE;
        // (code, field before, field after, the int left on the stack)
        let cases: [(Vec<u8>, i32, i32, i32); 4] = [
            (post_group(0x5a, &[0x04], 0x60, &[]), 41, 42, 41),
            (pre_group(&[0x04], 0x60, &[0x5a]), 41, 42, 42),
            (
                post_group(0x5a, &[0x04], 0x64, &[]),
                i32::MIN,
                i32::MAX,
                i32::MIN,
            ),
            (
                pre_group(&[0x1b], 0x60, &[0x5a]),
                i32::MAX,
                i32::MIN.wrapping_add(4),
                i32::MIN + 4,
            ),
        ];
        for (code, before, after, left) in cases {
            let Some((ptr, site)) = legacy_receiver(&zgc, 7101, before) else {
                return;
            };
            let mut sites = PinnedFast::new();
            if !fill(&mut sites, class_id, site) {
                return;
            }
            // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
            let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code.clone(), recv);
            f.stack.push_int(7).unwrap();
            // Without the bit: a decline, nothing written, nothing pushed.
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
            assert_eq!(legacy_value(ptr), Some(Value::Int(before)));
            assert_eq!(f.stack.len(), 1);
            for extra in [0, FIELD_INC_EARLY_DECLINE] {
                // SAFETY: field 0's 16-byte cell of the live legacy object.
                unsafe {
                    cratonvm_types::write_value_atomic(
                        (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                        Value::Int(before),
                    )
                };
                assert_eq!(run_mode(&zgc, &mut sites, &mut f, value | extra), Some(end));
                assert_eq!(legacy_value(ptr), Some(Value::Int(after)), "{code:x?}");
                assert_eq!(f.stack.len(), 2, "one value pushed");
                assert_eq!(f.stack.pop_int_unchecked(), left, "{code:x?}");
                assert_eq!(f.stack.pop_int_unchecked(), 7, "the slot below untouched");
                f.stack.push_int(7).unwrap();
            }
        }
    }

    #[test]
    fn a_narrow_value_form_leaves_the_extended_old_or_the_narrowed_new() {
        // (class, storage, descriptor, conversion, stored bits, post?, the
        //  field after as the quickened getfield reads it, the int left)
        let cases: [(u32, FieldStorageKind, u8, u8, u16, bool, i32, i32); 4] = [
            (
                7102,
                FieldStorageKind::Byte,
                b'B',
                0x91,
                0x7f,
                false,
                -128,
                -128,
            ),
            (
                7103,
                FieldStorageKind::Byte,
                b'B',
                0x91,
                0x7f,
                true,
                -128,
                127,
            ),
            (
                7104,
                FieldStorageKind::Char,
                b'C',
                0x92,
                0xffff,
                true,
                0,
                65535,
            ),
            (
                7105,
                FieldStorageKind::Short,
                b'S',
                0x93,
                0x7fff,
                false,
                -32768,
                -32768,
            ),
        ];
        for (class, kind, desc, conv, bits, post, want, left) in cases {
            let zgc = ZgcRealHeap::with_capacity(1 << 20);
            let Some((obj, fp, site)) = compact_receiver(&zgc, class, kind, desc) else {
                continue;
            };
            let class_id = ClassId::new(0x00ab_0720 + class);
            let mut sites = PinnedFast::new();
            if !fill(&mut sites, class_id, site) {
                continue;
            }
            // SAFETY: `fp` is the layout's slot of the live object, of the
            // width its storage kind names.
            unsafe {
                if kind == FieldStorageKind::Byte {
                    store_u8(fp, bits as u8);
                } else {
                    store_u16(fp, bits);
                }
            }
            let code = if post {
                post_group(0x5a, &[0x04], 0x60, &[conv])
            } else {
                pre_group(&[0x04], 0x60, &[conv, 0x5a])
            };
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code, Value::Object(Some(obj)));
            let mode = FIELD_INC_ON | FIELD_INC_WIDE | FIELD_INC_VALUE;
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), Some(end));
            // SAFETY: as above.
            let read = match kind {
                FieldStorageKind::Byte => i32::from((unsafe { load_u8(fp) }) as i8),
                FieldStorageKind::Char => i32::from(unsafe { load_u16(fp) }),
                _ => i32::from((unsafe { load_u16(fp) }) as i16),
            };
            assert_eq!(read, want, "{} post={post}", desc as char);
            assert_eq!(f.stack.len(), 1);
            assert_eq!(
                f.stack.pop_int_unchecked(),
                left,
                "{} post={post}",
                desc as char
            );
        }
    }

    #[test]
    fn a_long_value_form_leaves_one_long_slot() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0714);
        let mode = FIELD_INC_ON | FIELD_INC_WIDE | FIELD_INC_VALUE;
        // (code, before, after, left)
        let cases: [(Vec<u8>, i64, i64, i64); 2] = [
            (
                post_group(0x5d, &[0x0a], 0x61, &[]),
                0xffff_ffff,
                0x1_0000_0000,
                0xffff_ffff,
            ),
            (
                pre_group(&[0x0a], 0x65, &[0x5d]),
                i64::MIN,
                i64::MAX,
                i64::MAX,
            ),
        ];
        for (code, before, after, left) in cases {
            let Some((ptr, site)) = legacy_receiver(&zgc, 7106, 0) else {
                return;
            };
            // SAFETY: field 0's 16-byte cell of the live legacy object.
            unsafe {
                cratonvm_types::write_value_atomic(
                    (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                    Value::Long(before),
                )
            };
            let mut sites = PinnedFast::new();
            if !fill(
                &mut sites,
                class_id,
                FastFieldSite {
                    desc_byte: b'J',
                    ..site
                },
            ) {
                return;
            }
            // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
            let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code, recv);
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), Some(end));
            assert_eq!(legacy_value(ptr), Some(Value::Long(after)));
            assert_eq!(
                f.stack.len(),
                1,
                "one slot, as the quickened `J` getfield pushes"
            );
            let (_, kind) = f.stack.peek_with_kind_unchecked();
            assert_eq!(kind, ValueStack::KIND_MARK_LONG);
            assert_eq!(f.stack.pop_long_unchecked(), left);
        }
    }

    #[test]
    fn a_value_form_declines_on_a_full_stack_having_written_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0715);
        let Some((ptr, site)) = legacy_receiver(&zgc, 7107, 41) else {
            return;
        };
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let mode = FIELD_INC_ON | FIELD_INC_VALUE;
        let mut f = frame(class_id, "m", pre_group(&[0x04], 0x60, &[0x5a]), recv);
        while f.stack.has_room() {
            f.stack.push_int(0).unwrap();
        }
        let full = f.stack.len();
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), None);
        assert_eq!(legacy_value(ptr), Some(Value::Int(41)), "nothing stored");
        assert_eq!(f.stack.len(), full);
        // The statement form pushes nothing, so a full stack does not matter.
        let mut g = frame(class_id, "m", group(0x2a, &[0x04], 0x60), recv);
        while g.stack.has_room() {
            g.stack.push_int(0).unwrap();
        }
        assert!(run_mode(&zgc, &mut sites, &mut g, mode).is_some());
        assert_eq!(legacy_value(ptr), Some(Value::Int(42)));
    }

    #[test]
    fn a_value_form_declines_as_the_statement_form_declines() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0716);
        let mode = FIELD_INC_ON | FIELD_INC_VALUE;
        // A null receiver: nothing pushed, the unfused `getfield` throws.
        let mut sites = PinnedFast::new();
        let mut null = frame(
            class_id,
            "m",
            post_group(0x5a, &[0x04], 0x60, &[]),
            Value::Object(None),
        );
        assert_eq!(run_mode(&zgc, &mut sites, &mut null, mode), None);
        assert_eq!(null.stack.len(), 0);
        let Some((ptr, site)) = legacy_receiver(&zgc, 7108, 41) else {
            return;
        };
        if !fill(&mut sites, class_id, site) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let mut f = frame(class_id, "m", post_group(0x5a, &[0x04], 0x60, &[]), recv);
        let code_len = f.code.len() - 2;
        // A watch armed, the field-phase instrument, a final field.
        let watched =
            fused_field_increment_on(&zgc, &mut sites, &mut f, code_len, 0, mode, Some(true));
        assert_eq!(watched, None);
        assert_eq!(
            run_mode(&zgc, &mut sites, &mut f, mode | FIELD_INC_PHASES),
            None
        );
        let mut never = PinnedFast::new();
        let never_site = FastFieldSite {
            final_put: FastFieldSite::PUT_NEVER,
            ..site
        };
        if fill(&mut never, class_id, never_site) {
            assert_eq!(run_mode(&zgc, &mut never, &mut f, mode), None);
        }
        assert_eq!(f.stack.len(), 0, "nothing pushed by a decline");
        assert_eq!(legacy_value(ptr), Some(Value::Int(41)));
    }

    // ── Interpreter round i3 wave 83, lane L7:
    // `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W` (`FIELD_INC_LDC2W`) ──

    /// The thread's numeric-constant table, pinned as `PinnedFast` is.
    type PinnedConsts = PinnedSites<
        crate::runtime::interpreter::site_cache::PrimitiveConstant,
        crate::runtime::interpreter::site_cache::NameEpochs,
    >;

    /// The pool index of the `ldc2_w` the wave-83 groups load.
    const K: u16 = 0x0007;

    /// [`run_mode`] over both tables, with [`FIELD_INC_LDC2W`] as `ldc2w`:
    /// the two pinned tables are moved into one `InterpSiteTables` for the
    /// call and back after it.
    fn run_ldc2w(
        zgc: &ZgcRealHeap,
        sites: &mut PinnedFast,
        consts: &mut PinnedConsts,
        f: &mut Frame,
        mode: u8,
        ldc2w: bool,
    ) -> Option<usize> {
        let code_len = f.code.len() - 2;
        let mode = u16::from(mode) | (if ldc2w { FIELD_INC_LDC2W } else { 0 });
        consts.run(|ccache| {
            sites.run(|scache| {
                let mut tables = InterpSiteTables::new();
                std::mem::swap(&mut tables.fast_field_sites, scache);
                std::mem::swap(&mut tables.prim_const_sites, ccache);
                let next =
                    fused_field_increment(zgc, &mut tables, f, code_len, 0, mode, Some(false));
                std::mem::swap(&mut tables.fast_field_sites, scache);
                std::mem::swap(&mut tables.prim_const_sites, ccache);
                next
            })
        })
    }

    /// A legacy `J` receiver holding `Long(current)`, its site pinned in a
    /// fresh field table, and a fresh constant table holding `constant` at
    /// [`K`]; `None` when an allocation or a pin did not take.
    fn ldc2w_fixture(
        zgc: &ZgcRealHeap,
        class_id: ClassId,
        current: i64,
        constant: crate::runtime::interpreter::site_cache::PrimitiveConstant,
    ) -> Option<(u64, Value, PinnedFast, PinnedConsts)> {
        let (ptr, site) = legacy_receiver(zgc, 6983, 0)?;
        // SAFETY: field 0's 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                Value::Long(current),
            )
        };
        let mut sites = PinnedFast::new();
        if !fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: b'J',
                ..site
            },
        ) {
            return None;
        }
        let mut consts = PinnedConsts::new();
        if !consts.pin(class_id, K, constant) {
            return None;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        Some((ptr, recv, sites, consts))
    }

    #[test]
    fn an_ldc2w_long_operand_is_added_only_under_its_bit_and_wide() {
        use crate::runtime::interpreter::site_cache::PrimitiveConstant;
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0883);
        // (arith, constant, current, want): `ladd` / `lsub` wrap as the
        // unfused pair does.
        let cases: [(u8, i64, i64, i64); 4] = [
            (0x61, 1_000_000, 5, 1_000_005),
            (0x65, 1024, 0, -1024),
            (0x61, 1, i64::MAX, i64::MIN),
            (0x65, i64::MIN, 0, i64::MIN),
        ];
        for (arith, k, current, want) in cases {
            let Some((ptr, recv, mut sites, mut consts)) =
                ldc2w_fixture(&zgc, class_id, current, PrimitiveConstant::Long(k))
            else {
                return;
            };
            let code = wide_group(0x2a, &[0x14, 0x00, 0x07], arith, &[]);
            let end = code.len() - 1;
            let mut f = frame(class_id, "m", code, recv);
            // Off (the bit clear), or the bit without the wide switch, or
            // the screen without the bit: declined, nothing written.
            assert_eq!(
                run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, WIDE, false),
                None
            );
            assert_eq!(
                run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, FIELD_INC_ON, true),
                None
            );
            let early = WIDE | FIELD_INC_EARLY_DECLINE;
            assert_eq!(
                run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, early, false),
                None
            );
            assert_eq!(legacy_value(ptr), Some(Value::Long(current)));
            assert_eq!(f.stack.len(), 0, "nothing pushed by a decline");
            // On: the constant is added (the screen passes it too).
            assert_eq!(
                run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, early, true),
                Some(end)
            );
            assert_eq!(legacy_value(ptr), Some(Value::Long(want)), "{k}");
            assert_eq!(f.stack.len(), 0, "the statement form pushes nothing");
        }
    }

    #[test]
    fn an_ldc2w_miss_a_double_or_an_int_entry_declines() {
        use crate::runtime::interpreter::site_cache::PrimitiveConstant;
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0884);
        for constant in [PrimitiveConstant::Double(2.0), PrimitiveConstant::Int(3)] {
            let Some((ptr, recv, mut sites, mut consts)) =
                ldc2w_fixture(&zgc, class_id, 7, constant)
            else {
                return;
            };
            let code = wide_group(0x2a, &[0x14, 0x00, 0x07], 0x61, &[]);
            let mut f = frame(class_id, "m", code, recv);
            assert_eq!(
                run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, WIDE, true),
                None
            );
            assert_eq!(legacy_value(ptr), Some(Value::Long(7)));
            // Another pool index than the one the table holds: a miss.
            let mut other = frame(
                class_id,
                "m",
                wide_group(0x2a, &[0x14, 0x00, 0x08], 0x61, &[]),
                recv,
            );
            assert_eq!(
                run_ldc2w(&zgc, &mut sites, &mut consts, &mut other, WIDE, true),
                None
            );
            assert_eq!(legacy_value(ptr), Some(Value::Long(7)));
        }
    }

    #[test]
    fn an_ldc2w_pre_increment_leaves_the_new_value() {
        use crate::runtime::interpreter::site_cache::PrimitiveConstant;
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0885);
        let Some((ptr, recv, mut sites, mut consts)) =
            ldc2w_fixture(&zgc, class_id, 40, PrimitiveConstant::Long(2))
        else {
            return;
        };
        // `return this.n += 2L`: `... ldc2_w; ladd; dup2_x1; putfield`.
        let code = wide_group(0x2a, &[0x14, 0x00, 0x07], 0x61, &[0x5d]);
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, recv);
        let mode = WIDE | FIELD_INC_VALUE;
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, mode, false),
            None
        );
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, mode, true),
            Some(end)
        );
        assert_eq!(legacy_value(ptr), Some(Value::Long(42)));
        assert_eq!(f.stack.len(), 1, "the new value, one slot");
    }

    #[test]
    fn the_screen_takes_an_ldc2w_operand_only_under_its_flag_and_wide() {
        let class_id = ClassId::new(0x00ab_0886);
        let f = frame(
            class_id,
            "m",
            wide_group(0x2a, &[0x14, 0x00, 0x07], 0x61, &[]),
            Value::Object(None),
        );
        let code_len = f.code.len() - 2;
        assert!(field_increment_screen(
            &f.code, code_len, 0, true, false, false, true
        ));
        assert!(!field_increment_screen(
            &f.code, code_len, 0, true, false, false, false
        ));
        assert!(!field_increment_screen(
            &f.code, code_len, 0, false, false, false, true
        ));
        // Cut off inside the index: refused.
        assert!(!field_increment_screen(
            &f.code, 5, 0, true, false, false, true
        ));
        // The decoder agrees: without a table the operand is refused.
        assert!(
            decode_field_increment_used(&f, &f.code, code_len, 0, true, false, false, None)
                .is_none()
        );
    }
}

#[cfg(test)]
mod i72_l7_fused_array_increment_tests {
    //! Interpreter round i3 wave 72, lane L7: the fused array element
    //! increment (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT`, `_VALUE`, `_TOP`;
    //! `fused_array_increment_local` / `_top`). A hit stores what the unfused
    //! `aload_N; iload_M; dup2; <x>aload; <operand>; <op>; [conv]; <x>astore`
    //! stores (the wrapping result, narrowed by the element's own conversion)
    //! and leaves the operand stack as the unfused group leaves it; every
    //! decline writes nothing and pushes nothing. Each test sets the switches
    //! it needs explicitly.
    use super::*;
    use crate::runtime::interpreter::IncrementOperand;
    use std::sync::Arc;

    type Decoded = (u8, IncrementOperand, u8, usize, FieldIncrementUse);

    /// `aload_0; iload_1; dup2; <load>; [post]; <operand>; <arith>; [conv];
    /// [pre]; <store>; return`.
    #[allow(clippy::too_many_arguments)]
    fn group_with_store(
        load: u8,
        post: Option<u8>,
        operand: &[u8],
        arith: u8,
        conv: Option<u8>,
        pre: Option<u8>,
        store: u8,
    ) -> Vec<u8> {
        let mut code = vec![0x2a, 0x1b, 0x5c, load];
        code.extend(post);
        code.extend_from_slice(operand);
        code.push(arith);
        code.extend(conv);
        code.extend(pre);
        code.push(store);
        code.push(0xb1);
        code
    }

    /// [`group_with_store`] with the load's own store (`load + 0x21`).
    fn group(
        load: u8,
        post: Option<u8>,
        operand: &[u8],
        arith: u8,
        conv: Option<u8>,
        pre: Option<u8>,
    ) -> Vec<u8> {
        group_with_store(load, post, operand, arith, conv, pre, load + 0x21)
    }

    /// A static frame over `code`: local 0 = `arr`, local 1 = `index`, local
    /// 2 = `3`; `max_locals` 4.
    fn frame(code: Vec<u8>, arr: Value, index: i32) -> Frame {
        Frame::new(
            ClassId::new(0x00ab_0721),
            "cratonvm/test/i72l7/ArrayIncrement".to_string(),
            "m".to_string(),
            "([III)V".to_string(),
            None,
            code,
            vec![],
            6,
            4,
            &[arr, Value::Int(index), Value::Int(3)],
        )
    }

    fn decode(code: Vec<u8>, value: bool) -> Option<Decoded> {
        let f = frame(code, Value::Object(None), 0);
        let code_len = f.code.len() - 2;
        decode_array_increment(&f, &f.code, code_len, 2, value)
    }

    /// A fresh array of `et`, `len` elements, or `None` when the test heap is
    /// exhausted.
    fn array(zgc: &ZgcRealHeap, et: ArrayElementType, len: usize) -> Option<ObjectRef> {
        zgc.try_alloc_array(ClassId::new(0), et, len)
    }

    /// Element `index` of `arr` as the `*aload` `load` pushes it (an int, or
    /// a long for `laload`), widened to `i64`.
    fn get(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, load: u8) -> Option<i64> {
        let mut s = ValueStack::new(4);
        if !array_load_prim(zgc, &mut s, arr, index, load) {
            return None;
        }
        let cv = s.pop_compact();
        if load == 0x2f {
            Some(cv.as_long_unchecked())
        } else {
            cv.as_int().map(i64::from)
        }
    }

    /// Store `v` (an int, or a long for `lastore`) into element `index`.
    fn set(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, store: u8, v: i64) {
        let (cv, kind) = if store == 0x50 {
            (CompactValue::long(v), ValueStack::KIND_MARK_LONG)
        } else {
            // Cast: test values are in the element's range.
            (CompactValue::int(v as i32), 0)
        };
        assert!(array_store_prim(zgc, arr, index, store, cv, kind));
    }

    fn body(
        zgc: &ZgcRealHeap,
        f: &Frame,
        arr: ObjectRef,
        index: i32,
        value: bool,
    ) -> Option<(usize, Option<IncrementOperand>)> {
        let code_len = f.code.len() - 2;
        let ptr = arr.as_ptr() as u64;
        fused_array_increment_body(
            zgc,
            f,
            code_len,
            2,
            ptr,
            index,
            array_increment_mode(value, false),
            false,
            true,
            NO_DECLINE_MEMO,
            body_test_vm(),
            &mut no_sites(),
        )
    }

    #[test]
    fn the_decoder_takes_every_element_kind_with_its_own_conversion() {
        let iconst_1: &[u8] = &[0x04];
        for (load, conv) in [
            (0x2e_u8, None),
            (0x33, Some(0x91_u8)),
            (0x34, Some(0x92)),
            (0x35, Some(0x93)),
        ] {
            for arith in [0x60_u8, 0x64, 0x68, 0x7e, 0x80, 0x82] {
                let code = group(load, None, iconst_1, arith, conv, None);
                let end = code.len() - 1;
                assert_eq!(
                    decode(code, false),
                    Some((
                        load,
                        IncrementOperand::Int(1),
                        arith,
                        end,
                        FieldIncrementUse::Statement
                    )),
                    "{load:#x} {arith:#x}"
                );
            }
        }
        // `bipush -3`, `sipush 300`, `iload_2` (3), `iload 2`.
        let operands: [(&[u8], i32); 4] = [
            (&[0x10, 0xfd], -3),
            (&[0x11, 0x01, 0x2c], 300),
            (&[0x1c], 3),
            (&[0x15, 0x02], 3),
        ];
        for (operand, k) in operands {
            let code = group(0x2e, None, operand, 0x60, None, None);
            assert_eq!(
                decode(code, false).map(|d| d.1),
                Some(IncrementOperand::Int(k))
            );
        }
        for arith in [0x61_u8, 0x65, 0x69, 0x7f, 0x81, 0x83] {
            let code = group(0x2f, None, &[0x0a], arith, None, None);
            let end = code.len() - 1;
            assert_eq!(
                decode(code, false),
                Some((
                    0x2f,
                    IncrementOperand::Long(1),
                    arith,
                    end,
                    FieldIncrementUse::Statement
                ))
            );
        }
    }

    #[test]
    fn the_decoder_refuses_every_other_shape() {
        let refused = |code: Vec<u8>| assert_eq!(decode(code.clone(), true), None, "{code:x?}");
        // A narrow element without its conversion, or with another's.
        refused(group(0x33, None, &[0x04], 0x60, None, None));
        refused(group(0x33, None, &[0x04], 0x60, Some(0x92), None));
        refused(group(0x34, None, &[0x04], 0x60, Some(0x93), None));
        // An int element with a conversion (not a compound assignment).
        refused(group(0x2e, None, &[0x04], 0x60, Some(0x91), None));
        // `idiv` can throw; the shifts are not taken.
        refused(group(0x2e, None, &[0x04], 0x6c, None, None));
        refused(group(0x2e, None, &[0x04], 0x70, None, None));
        refused(group(0x2e, None, &[0x04], 0x78, None, None));
        // Float / double / reference elements.
        refused(group(0x30, None, &[0x0b], 0x62, None, None));
        refused(group(0x31, None, &[0x0e], 0x63, None, None));
        refused(group(0x32, None, &[0x01], 0x60, None, None));
        // Width mismatches between the element, the operand and the op.
        refused(group(0x2f, None, &[0x04], 0x61, None, None));
        refused(group(0x2f, None, &[0x0a], 0x60, None, None));
        refused(group(0x2e, None, &[0x0a], 0x60, None, None));
        refused(group(0x2e, None, &[0x04], 0x61, None, None));
        // Another array kind's store.
        refused(group_with_store(
            0x33,
            None,
            &[0x04],
            0x60,
            Some(0x91),
            None,
            0x4f,
        ));
        refused(group_with_store(
            0x2e,
            None,
            &[0x04],
            0x60,
            None,
            None,
            0x54,
        ));
        // A non-int operand local (local 0 is the array).
        refused(group(0x2e, None, &[0x1a], 0x60, None, None));
        // A duplicate of the wrong width, or two of them.
        refused(group(0x2e, Some(0x5e), &[0x04], 0x60, None, None));
        refused(group(0x2f, Some(0x5b), &[0x0a], 0x61, None, None));
        refused(group(0x2e, Some(0x5b), &[0x04], 0x60, None, Some(0x5b)));
        // Not a `dup2` at the group's pc.
        let mut code = group(0x2e, None, &[0x04], 0x60, None, None);
        code[2] = 0x59;
        refused(code);
    }

    #[test]
    fn the_decoder_takes_the_value_forms_only_under_the_bit() {
        let post = group(0x2e, Some(0x5b), &[0x04], 0x60, None, None);
        let pre = group(0x33, None, &[0x04], 0x60, Some(0x91), Some(0x5b));
        let post_long = group(0x2f, Some(0x5e), &[0x0a], 0x61, None, None);
        let pre_long = group(0x2f, None, &[0x0a], 0x65, None, Some(0x5e));
        for code in [&post, &pre, &post_long, &pre_long] {
            assert_eq!(decode(code.clone(), false), None, "{code:x?}");
        }
        assert_eq!(
            decode(post.clone(), true).map(|d| d.4),
            Some(FieldIncrementUse::Old)
        );
        assert_eq!(
            decode(pre.clone(), true).map(|d| d.4),
            Some(FieldIncrementUse::New)
        );
        assert_eq!(
            decode(post_long.clone(), true).map(|d| d.4),
            Some(FieldIncrementUse::Old)
        );
        assert_eq!(
            decode(pre_long.clone(), true).map(|d| d.4),
            Some(FieldIncrementUse::New)
        );
        for code in [post, pre, post_long, pre_long] {
            let end = code.len() - 1;
            assert_eq!(decode(code, true).map(|d| d.3), Some(end));
        }
    }

    #[test]
    fn a_hit_stores_what_the_unfused_group_stores() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (imin, imax) = (i64::from(i32::MIN), i64::from(i32::MAX));
        // (element type, load, conversion, operand, arith, before, after)
        #[allow(clippy::type_complexity)]
        let cases: [(ArrayElementType, u8, Option<u8>, &[u8], u8, i64, i64); 14] = [
            (ArrayElementType::Int, 0x2e, None, &[0x04], 0x60, 41, 42),
            (ArrayElementType::Int, 0x2e, None, &[0x04], 0x60, imax, imin),
            (ArrayElementType::Int, 0x2e, None, &[0x04], 0x64, imin, imax),
            (ArrayElementType::Int, 0x2e, None, &[0x1c], 0x68, 7, 21),
            (
                ArrayElementType::Int,
                0x2e,
                None,
                &[0x10, 0x0c],
                0x7e,
                0x3a,
                0x08,
            ),
            (
                ArrayElementType::Int,
                0x2e,
                None,
                &[0x10, 0x0c],
                0x80,
                0x31,
                0x3d,
            ),
            (
                ArrayElementType::Int,
                0x2e,
                None,
                &[0x10, 0x0c],
                0x82,
                0x0f,
                0x03,
            ),
            (
                ArrayElementType::Byte,
                0x33,
                Some(0x91),
                &[0x04],
                0x60,
                127,
                -128,
            ),
            (
                ArrayElementType::Byte,
                0x33,
                Some(0x91),
                &[0x04],
                0x64,
                -128,
                127,
            ),
            (
                ArrayElementType::Char,
                0x34,
                Some(0x92),
                &[0x04],
                0x60,
                0xffff,
                0,
            ),
            (
                ArrayElementType::Char,
                0x34,
                Some(0x92),
                &[0x04],
                0x64,
                0,
                0xffff,
            ),
            (
                ArrayElementType::Short,
                0x35,
                Some(0x93),
                &[0x04],
                0x60,
                32767,
                -32768,
            ),
            (
                ArrayElementType::Long,
                0x2f,
                None,
                &[0x0a],
                0x61,
                i64::MAX,
                i64::MIN,
            ),
            (ArrayElementType::Long, 0x2f, None, &[0x0a], 0x65, 0, -1),
        ];
        for (et, load, conv, operand, arith, before, after) in cases {
            let Some(arr) = array(&zgc, et, 3) else {
                return;
            };
            set(&zgc, arr, 0, load + 0x21, 0);
            set(&zgc, arr, 1, load + 0x21, before);
            set(&zgc, arr, 2, load + 0x21, 0);
            let code = group(load, None, operand, arith, conv, None);
            let end = code.len() - 1;
            let f = frame(code, Value::Object(Some(arr)), 1);
            assert_eq!(
                body(&zgc, &f, arr, 1, false),
                Some((end, None)),
                "{load:#x} {arith:#x}"
            );
            assert_eq!(
                get(&zgc, arr, 1, load),
                Some(after),
                "{load:#x} {arith:#x} {before}"
            );
            // The neighbours are untouched.
            assert_eq!(get(&zgc, arr, 0, load), Some(0), "{load:#x}");
            assert_eq!(get(&zgc, arr, 2, load), Some(0), "{load:#x}");
        }
    }

    #[test]
    fn every_decline_writes_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(ints), Some(bools)) = (
            array(&zgc, ArrayElementType::Int, 3),
            array(&zgc, ArrayElementType::Boolean, 3),
        ) else {
            return;
        };
        for i in 0..3 {
            set(&zgc, ints, i, 0x4f, 10 + i64::from(i));
        }
        let code = group(0x2e, None, &[0x04], 0x60, None, None);
        let f = frame(code, Value::Object(Some(ints)), 3);
        // Out of range either side (the unfused `iaload` raises the AIOOBE).
        assert_eq!(body(&zgc, &f, ints, 3, false), None);
        assert_eq!(body(&zgc, &f, ints, -1, false), None);
        assert_eq!(body(&zgc, &f, ints, i32::MIN, false), None);
        // A null array (the unfused `iaload` raises the NPE).
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_body(
                &zgc,
                &f,
                code_len,
                2,
                0,
                0,
                0,
                false,
                true,
                NO_DECLINE_MEMO,
                body_test_vm(),
                &mut no_sites()
            ),
            None
        );
        // An element type the opcode does not name.
        let bytes = group(0x33, None, &[0x04], 0x60, Some(0x91), None);
        let fb = frame(bytes.clone(), Value::Object(Some(ints)), 0);
        assert_eq!(body(&zgc, &fb, ints, 0, false), None);
        let longs = group(0x2f, None, &[0x0a], 0x61, None, None);
        let fl = frame(longs, Value::Object(Some(ints)), 0);
        assert_eq!(body(&zgc, &fl, ints, 0, false), None);
        for i in 0..3 {
            assert_eq!(
                get(&zgc, ints, i, 0x2e),
                Some(10 + i64::from(i)),
                "element {i}"
            );
        }
        // A `boolean[]` under `baload`: declined, not reproduced.
        set(&zgc, bools, 0, 0x54, 1);
        let fz = frame(bytes, Value::Object(Some(bools)), 0);
        assert_eq!(body(&zgc, &fz, bools, 0, false), None);
        assert_eq!(get(&zgc, bools, 0, 0x33), Some(1));
    }

    #[test]
    fn a_value_form_leaves_the_old_or_the_new_element() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let int = |n: i32| IncrementOperand::Int(n);
        // (element type, code, before, after, left)
        let cases: [(ArrayElementType, Vec<u8>, i64, i64, IncrementOperand); 5] = [
            (
                ArrayElementType::Int,
                group(0x2e, Some(0x5b), &[0x04], 0x60, None, None),
                41,
                42,
                int(41),
            ),
            (
                ArrayElementType::Int,
                group(0x2e, None, &[0x04], 0x60, None, Some(0x5b)),
                41,
                42,
                int(42),
            ),
            (
                ArrayElementType::Byte,
                group(0x33, Some(0x5b), &[0x04], 0x60, Some(0x91), None),
                127,
                -128,
                int(127),
            ),
            (
                ArrayElementType::Char,
                group(0x34, None, &[0x04], 0x64, Some(0x92), Some(0x5b)),
                0,
                0xffff,
                int(0xffff),
            ),
            (
                ArrayElementType::Long,
                group(0x2f, Some(0x5e), &[0x0a], 0x61, None, None),
                9,
                10,
                IncrementOperand::Long(9),
            ),
        ];
        for (et, code, before, after, left) in cases {
            let Some(arr) = array(&zgc, et, 2) else {
                return;
            };
            let load = code[3];
            set(&zgc, arr, 0, load + 0x21, before);
            let end = code.len() - 1;
            let f = frame(code.clone(), Value::Object(Some(arr)), 0);
            // Without the bit: a decline, nothing written.
            assert_eq!(body(&zgc, &f, arr, 0, false), None, "{code:x?}");
            assert_eq!(get(&zgc, arr, 0, load), Some(before), "{code:x?}");
            assert_eq!(
                body(&zgc, &f, arr, 0, true),
                Some((end, Some(left))),
                "{code:x?}"
            );
            assert_eq!(get(&zgc, arr, 0, load), Some(after), "{code:x?}");
        }
    }

    fn vm(on: bool, value: bool, top: bool) -> Arc<SharedVm> {
        let mut config = crate::config::VmConfig::default();
        config.interp_fused_array_increment = on;
        config.interp_fused_array_increment_value = value;
        config.interp_fused_array_increment_top = top;
        // Pinned too (wave 73), though no helper under test reads them: a
        // config built from the environment must not leak a default.
        config.interp_fused_array_increment_dup2 = false;
        config.interp_fused_array_accumulate = false;
        // Wave 74: the widened arithmetic and the no-dup entry, pinned off.
        config.interp_fused_array_increment_shift_div = false;
        config.interp_fused_array_update_no_dup = false;
        // Wave 75: the decline memo, pinned off.
        config.interp_fused_array_decline_memo = false;
        config.interp_fused_array_increment_array_operand = false;
        config.interp_fused_array_increment_static_operand = false;
        config.site_census = false;
        Arc::new(SharedVm::new(config))
    }

    #[test]
    fn the_local_entry_reads_both_locals_and_pushes_only_a_value_form() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = array(&zgc, ArrayElementType::Int, 2) else {
            return;
        };
        let statement = group(0x2e, None, &[0x04], 0x60, None, None);
        let post = group(0x2e, Some(0x5b), &[0x04], 0x60, None, None);
        for value in [false, true] {
            let shared = vm(true, value, false);
            set(&zgc, arr, 1, 0x4f, 5);
            let mut f = frame(statement.clone(), Value::Object(Some(arr)), 1);
            let code_len = f.code.len() - 2;
            assert_eq!(
                fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
                Some(statement.len() - 1)
            );
            assert_eq!(f.stack.len(), 0, "the statement form pushes nothing");
            assert_eq!(get(&zgc, arr, 1, 0x2e), Some(6));
            let mut f = frame(post.clone(), Value::Object(Some(arr)), 1);
            let code_len = f.code.len() - 2;
            let got =
                fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
            if value {
                assert_eq!(got, Some(post.len() - 1));
                assert_eq!(f.stack.len(), 1);
                assert_eq!(f.stack.pop_int_unchecked(), 6, "the old element");
                assert_eq!(get(&zgc, arr, 1, 0x2e), Some(7));
            } else {
                assert_eq!(got, None);
                assert_eq!(f.stack.len(), 0);
                assert_eq!(get(&zgc, arr, 1, 0x2e), Some(6));
            }
        }
        // A null array local: a decline (the unfused `iaload` throws).
        let shared = vm(true, true, false);
        let mut f = frame(statement, Value::Object(None), 1);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 0);
    }

    #[test]
    fn the_top_entry_replaces_the_array_slot_and_needs_the_base_switch() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = array(&zgc, ArrayElementType::Int, 2) else {
            return;
        };
        // `iload_1; dup2; iaload; iconst_1; iadd; [dup_x2]; iastore; return`,
        // the array pushed before it.
        let statement = group(0x2e, None, &[0x04], 0x60, None, None)[1..].to_vec();
        let pre = group(0x2e, None, &[0x04], 0x60, None, Some(0x5b))[1..].to_vec();
        for (code, value, want_left) in [(statement, false, None), (pre, true, Some(4))] {
            for on in [false, true] {
                set(&zgc, arr, 0, 0x4f, 3);
                let shared = vm(on, value, true);
                let mut f = frame(code.clone(), Value::Object(Some(arr)), 0);
                f.stack.push_int(9).unwrap();
                f.stack.push(Value::Object(Some(arr))).unwrap();
                let code_len = f.code.len() - 2;
                let got =
                    fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
                if !on {
                    assert_eq!(got, None, "admitted only with the base switch");
                    assert_eq!(f.stack.len(), 2, "the stack as it was");
                    assert_eq!(get(&zgc, arr, 0, 0x2e), Some(3));
                    continue;
                }
                assert_eq!(got, Some(code.len() - 1));
                assert_eq!(get(&zgc, arr, 0, 0x2e), Some(4));
                match want_left {
                    None => assert_eq!(f.stack.len(), 1, "the array slot popped"),
                    Some(v) => {
                        assert_eq!(f.stack.len(), 2, "the array slot replaced");
                        assert_eq!(f.stack.pop_int_unchecked(), v);
                    }
                }
                assert_eq!(f.stack.pop_int_unchecked(), 9, "the slot below untouched");
            }
        }
        // Out of range: the stack as it was, nothing written.
        let shared = vm(true, false, true);
        let code = group(0x2e, None, &[0x04], 0x60, None, None)[1..].to_vec();
        let mut f = frame(code, Value::Object(Some(arr)), 2);
        f.stack.push(Value::Object(Some(arr))).unwrap();
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 1);
    }

    #[test]
    fn the_dup2_entry_pops_both_slots_and_declines_a_category_two_top() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = array(&zgc, ArrayElementType::Int, 4) else {
            return;
        };
        // `dup2; iaload; iconst_1; iadd; [dup_x2]; iastore; return`, the
        // array and the index pushed before it.
        let statement = group(0x2e, None, &[0x04], 0x60, None, None)[2..].to_vec();
        let post = group(0x2e, Some(0x5b), &[0x04], 0x60, None, None)[2..].to_vec();
        for (code, value, want_left) in [(statement.clone(), false, None), (post, true, Some(7))] {
            for on in [false, true] {
                set(&zgc, arr, 3, 0x4f, 7);
                let shared = vm(on, value, false);
                let mut f = frame(code.clone(), Value::Object(Some(arr)), 0);
                f.stack.push_int(9).unwrap();
                f.stack.push(Value::Object(Some(arr))).unwrap();
                f.stack.push_int(3).unwrap();
                let code_len = f.code.len() - 2;
                let got =
                    fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
                if !on {
                    assert_eq!(got, None, "admitted only with the base switch");
                    assert_eq!(f.stack.len(), 3, "the stack as it was");
                    assert_eq!(get(&zgc, arr, 3, 0x2e), Some(7));
                    continue;
                }
                assert_eq!(got, Some(code.len() - 1));
                assert_eq!(get(&zgc, arr, 3, 0x2e), Some(8));
                match want_left {
                    None => assert_eq!(f.stack.len(), 1, "both slots popped"),
                    Some(v) => {
                        assert_eq!(f.stack.len(), 2, "one value left");
                        assert_eq!(f.stack.pop_int_unchecked(), v);
                    }
                }
                assert_eq!(f.stack.pop_int_unchecked(), 9, "the slot below untouched");
            }
        }
        // A long on top (a `dup2` of a category-2 value): declined. The loop
        // above left the element at 8 (its last hit); reset it so the
        // decline's "nothing written" is what the last assertion checks
        // (wave 73: the reset was missing, so the assertion read the 8 the
        // loop's last hit stored: `left: Some(8), right: Some(7)` on every
        // run that got this far).
        set(&zgc, arr, 3, 0x4f, 7);
        let shared = vm(true, false, false);
        let mut f = frame(statement, Value::Object(Some(arr)), 0);
        f.stack.push(Value::Object(Some(arr))).unwrap();
        f.stack.push_long(3).unwrap();
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 2);
        assert_eq!(get(&zgc, arr, 3, 0x2e), Some(7));
    }
}

#[cfg(test)]
mod i72_l7_two_field_update_tests {
    //! Interpreter round i3 wave 72, lane L7: the two-field update
    //! `this.a = this.b + k` (`CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS`,
    //! `FIELD_INC_TWO_FIELDS`, `fused_two_field_update`). A hit stores what
    //! the unfused `aload_0; aload_0; getfield #G; <operand>; iadd | isub;
    //! putfield #F` stores into `#F` and leaves `#G` and the stack alone; a
    //! decline writes nothing. Each test passes its mode bits explicitly.
    use super::*;
    use crate::runtime::interpreter::site_cache::PinnedSites;

    /// Pinned sites (wave 73, lane L7): see `i68_l7_fused_field_increment_tests`.
    type PinnedFast = PinnedSites<FastFieldSite>;

    const GET: u16 = 0x0102;
    const PUT: u16 = 0x0103;

    /// `aload_0; aload_0; getfield #GET; <operand>; <arith>; <tail..>;
    /// putfield #<put>; return`.
    fn group(operand: &[u8], arith: u8, tail: &[u8], put: u16) -> Vec<u8> {
        let mut code = vec![0x2a, 0x2a, 0xb4, 0x01, 0x02];
        code.extend_from_slice(operand);
        code.push(arith);
        code.extend_from_slice(tail);
        // Cast: the test's two-byte pool index.
        code.extend_from_slice(&[0xb5, (put >> 8) as u8, put as u8, 0xb1]);
        code
    }

    /// An instance `(I)V` frame: local 0 = `recv`, local 1 = `5`.
    fn frame(class_id: ClassId, code: Vec<u8>, recv: Value) -> Frame {
        Frame::new(
            class_id,
            "cratonvm/test/i72l7/TwoFields".to_string(),
            "m".to_string(),
            "(I)V".to_string(),
            None,
            code,
            vec![],
            4,
            3,
            &[recv, Value::Int(5)],
        )
    }

    fn fill(sites: &mut PinnedFast, class_id: ClassId, cp: u16, site: FastFieldSite) -> bool {
        sites.pin(class_id, cp, site)
    }

    /// A legacy two-field receiver (`b` in field 0, `a` in field 1, both
    /// `Int`) and the sites `fill_site` would record for them under the
    /// descriptors given; `None` when the allocation failed or came out
    /// compact.
    fn receiver(
        zgc: &ZgcRealHeap,
        class: u32,
        b: Value,
        a: Value,
        b_desc: u8,
        a_desc: u8,
    ) -> Option<(u64, FastFieldSite, FastFieldSite)> {
        let obj = zgc.try_alloc_object(ClassId::new(class), 2)?;
        let ptr = obj.as_ptr() as u64;
        // SAFETY: a fresh allocation starts with its header.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        if cratonvm_types::is_compact_object(header) {
            return None;
        }
        let site = |field_index: u32, desc_byte: u8| FastFieldSite {
            receiver_class_id: header.class_id,
            num_slots: header.num_slots(),
            offset: u32::try_from(HEADER_SIZE + field_index as usize * SLOT_SIZE).unwrap_or(0),
            storage: None,
            field_index,
            desc_byte,
            is_reference: false,
            final_put: FastFieldSite::PUT_ANY,
        };
        set(ptr, 0, b);
        set(ptr, 1, a);
        Some((ptr, site(0, b_desc), site(1, a_desc)))
    }

    fn set(ptr: u64, field: usize, v: Value) {
        // SAFETY: field `field`'s 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE + field * SLOT_SIZE) as *mut Value,
                v,
            )
        };
    }

    fn get(ptr: u64, field: usize) -> Option<Value> {
        // SAFETY: as in `set`.
        unsafe {
            cratonvm_types::read_value_checked_atomic(
                (ptr as *mut u8).add(HEADER_SIZE + field * SLOT_SIZE) as *const Value,
            )
        }
    }

    const MODE: u8 = FIELD_INC_ON | FIELD_INC_WIDE | FIELD_INC_NO_DUP | FIELD_INC_TWO_FIELDS;

    fn run(zgc: &ZgcRealHeap, sites: &mut PinnedFast, f: &mut Frame, mode: u8) -> Option<usize> {
        let code_len = f.code.len() - 2;
        sites.run(|cache| fused_field_increment_on(zgc, cache, f, code_len, 0, mode, Some(false)))
    }

    #[test]
    fn the_bit_is_set_only_on_top_of_no_dup() {
        assert_eq!(with_two_fields(0, true), 0, "a switched-off mode stays off");
        assert_eq!(with_two_fields(FIELD_INC_ON, true), FIELD_INC_ON);
        assert_eq!(
            with_two_fields(FIELD_INC_ON | FIELD_INC_NO_DUP, true),
            FIELD_INC_ON | FIELD_INC_NO_DUP | FIELD_INC_TWO_FIELDS
        );
        assert_eq!(
            with_two_fields(FIELD_INC_ON | FIELD_INC_NO_DUP, false),
            FIELD_INC_ON | FIELD_INC_NO_DUP
        );
        for other in [
            FIELD_INC_CENSUS,
            FIELD_INC_PHASES,
            FIELD_INC_WIDE,
            FIELD_INC_EARLY_DECLINE,
            FIELD_INC_NO_DUP,
            FIELD_INC_VALUE,
            FIELD_INC_ON,
        ] {
            assert_eq!(other & FIELD_INC_TWO_FIELDS, 0);
        }
    }

    #[test]
    fn the_loop_screen_passes_another_index_only_under_the_bit() {
        let two = group(&[0x04], 0x60, &[], PUT);
        let one = group(&[0x04], 0x60, &[], GET);
        assert!(!no_dup_group_may_fuse(&two, 0, false));
        assert!(no_dup_group_may_fuse(&two, 0, true));
        assert!(no_dup_group_may_fuse(&one, 0, false));
        assert!(no_dup_group_may_fuse(&one, 0, true));
        // A load operand (`a = b + c`) is still refused at the operand byte.
        let three = vec![
            0x2a, 0x2a, 0xb4, 0x01, 0x02, 0x2a, 0xb4, 0x01, 0x04, 0x60, 0xb5, 0x01, 0x03,
        ];
        assert!(!no_dup_group_may_fuse(&three, 0, true));
    }

    #[test]
    fn the_decoder_takes_two_indexes_of_one_receiver() {
        let class_id = ClassId::new(0x00ab_0722);
        let f = frame(
            class_id,
            group(&[0x1b], 0x64, &[], PUT),
            Value::Object(None),
        );
        let code_len = f.code.len() - 2;
        let end = code_len - 1;
        assert_eq!(
            decode_two_field_update(&f, &f.code, code_len, 0, false),
            Some((
                GET,
                PUT,
                0,
                crate::runtime::interpreter::IncrementOperand::Int(5),
                true,
                None,
                end
            ))
        );
        let refuses = |code: Vec<u8>, wide: bool| {
            let f = frame(class_id, code, Value::Object(None));
            let code_len = f.code.len() - 2;
            assert_eq!(
                decode_two_field_update(&f, &f.code, code_len, 0, wide),
                None
            );
        };
        // One index: the one-field increment's.
        refuses(group(&[0x04], 0x60, &[], GET), true);
        // Two receivers.
        refuses(
            vec![
                0x2a, 0x2b, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
            ],
            true,
        );
        // A conversion without `wide`.
        refuses(group(&[0x04], 0x60, &[0x91], PUT), false);
        // `imul` is not an increment.
        refuses(group(&[0x04], 0x68, &[], PUT), true);
        let f = frame(
            class_id,
            group(&[0x04], 0x60, &[0x91], PUT),
            Value::Object(None),
        );
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_two_field_update(&f, &f.code, code_len, 0, true).map(|d| d.5),
            Some(Some(0x91))
        );
    }

    #[test]
    fn a_hit_stores_the_other_field_and_leaves_the_read_one() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0723);
        let Some((ptr, b_site, a_site)) =
            receiver(&zgc, 7231, Value::Int(41), Value::Int(0), b'I', b'I')
        else {
            return;
        };
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, GET, b_site) || !fill(&mut sites, class_id, PUT, a_site) {
            return;
        }
        // SAFETY: `ptr` is the live object `receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let code = group(&[0x04], 0x60, &[], PUT);
        let end = code.len() - 1;
        let mut f = frame(class_id, code, recv);
        // Without the bit: a decline, nothing written.
        assert_eq!(
            run(&zgc, &mut sites, &mut f, MODE & !FIELD_INC_TWO_FIELDS),
            None
        );
        assert_eq!(get(ptr, 1), Some(Value::Int(0)));
        assert_eq!(run(&zgc, &mut sites, &mut f, MODE), Some(end));
        assert_eq!(get(ptr, 1), Some(Value::Int(42)), "a = b + 1");
        assert_eq!(get(ptr, 0), Some(Value::Int(41)), "b untouched");
        assert_eq!(f.stack.len(), 0, "nothing pushed");
        // Wrapping, and an `iload` operand (local 1 = 5) with `isub`.
        set(ptr, 0, Value::Int(i32::MIN));
        let mut f = frame(class_id, group(&[0x1b], 0x64, &[], PUT), recv);
        assert!(run(&zgc, &mut sites, &mut f, MODE).is_some());
        assert_eq!(get(ptr, 1), Some(Value::Int(i32::MIN.wrapping_sub(5))));
    }

    #[test]
    fn a_store_site_of_another_width_declines_having_written_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0724);
        // `a` is a `byte` field but the group has no `i2b`; then an `I` read
        // into a `J` store; then a cold store site.
        for (a_desc, fill_put) in [(b'B', true), (b'J', true), (b'I', false)] {
            let Some((ptr, b_site, a_site)) =
                receiver(&zgc, 7241, Value::Int(7), Value::Int(3), b'I', a_desc)
            else {
                return;
            };
            let mut sites = PinnedFast::new();
            if !fill(&mut sites, class_id, GET, b_site)
                || (fill_put && !fill(&mut sites, class_id, PUT, a_site))
            {
                return;
            }
            // SAFETY: `ptr` is the live object `receiver` allocated.
            let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
            let mut f = frame(class_id, group(&[0x04], 0x60, &[], PUT), recv);
            assert_eq!(
                run(&zgc, &mut sites, &mut f, MODE),
                None,
                "{}",
                a_desc as char
            );
            assert_eq!(get(ptr, 1), Some(Value::Int(3)), "{}", a_desc as char);
            assert_eq!(get(ptr, 0), Some(Value::Int(7)));
        }
        // A null receiver: a decline (the unfused `getfield` throws).
        let mut sites = PinnedFast::new();
        let mut f = frame(
            class_id,
            group(&[0x04], 0x60, &[], PUT),
            Value::Object(None),
        );
        assert_eq!(run(&zgc, &mut sites, &mut f, MODE), None);
    }

    #[test]
    fn a_narrow_store_is_stored_narrowed() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let class_id = ClassId::new(0x00ab_0725);
        let Some((ptr, b_site, a_site)) =
            receiver(&zgc, 7251, Value::Int(127), Value::Int(0), b'I', b'B')
        else {
            return;
        };
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, GET, b_site) || !fill(&mut sites, class_id, PUT, a_site) {
            return;
        }
        // SAFETY: `ptr` is the live object `receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let mut f = frame(class_id, group(&[0x04], 0x60, &[0x91], PUT), recv);
        assert!(run(&zgc, &mut sites, &mut f, MODE).is_some());
        assert_eq!(get(ptr, 1), Some(Value::Int(-128)), "(byte) (127 + 1)");
        // Without `wide` the conversion is refused.
        set(ptr, 1, Value::Int(0));
        let mut f = frame(class_id, group(&[0x04], 0x60, &[0x91], PUT), recv);
        assert_eq!(run(&zgc, &mut sites, &mut f, MODE & !FIELD_INC_WIDE), None);
        assert_eq!(get(ptr, 1), Some(Value::Int(0)));
    }
}

#[cfg(test)]
mod i73_l7_array_screen_and_accumulate_tests {
    //! Interpreter round i3 wave 73, lane L7: the fused array element
    //! accumulate (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE`,
    //! `fused_array_accumulate`). (The wave-73 byte screen's two tests left
    //! with the screen in wave 75.) An accumulate hit stores what the
    //! unfused `iload_S; aload_N; iload_M; <x>aload; <op>; istore_Z` stores,
    //! and a decline writes nothing. Each test sets the switches it needs
    //! explicitly.
    use super::*;
    use std::sync::Arc;

    /// A static frame over `code` with `args` in its first locals,
    /// `max_locals` 4.
    fn frame_with(code: Vec<u8>, args: &[Value]) -> Frame {
        Frame::new(
            ClassId::new(0x00ab_0731),
            "cratonvm/test/i73l7/ArrayScreen".to_string(),
            "m".to_string(),
            "([III)V".to_string(),
            None,
            code,
            vec![],
            6,
            4,
            args,
        )
    }

    /// `code` followed by the two padding bytes a frame's code carries.
    fn padded(code: &[u8]) -> Vec<u8> {
        let mut v = code.to_vec();
        v.extend_from_slice(&[0, 0]);
        v
    }

    fn vm(accumulate: bool) -> Arc<SharedVm> {
        let mut config = crate::config::VmConfig::default();
        config.interp_fused_array_accumulate = accumulate;
        config.site_census = false;
        Arc::new(SharedVm::new(config))
    }

    /// `iload_3; aload_0; iload_2; <load>; <arith>; <store>; return` over
    /// locals 0 = `arr`, 1 = 0, 2 = `index`, 3 = `acc`.
    fn accumulate_frame(
        load: u8,
        arith: u8,
        store: u8,
        arr: Value,
        index: Value,
        acc: i32,
    ) -> Frame {
        frame_with(
            vec![0x1d, 0x2a, 0x1c, load, arith, store, 0xb1],
            &[arr, Value::Int(0), index, Value::Int(acc)],
        )
    }

    #[test]
    fn the_accumulate_decoder_takes_only_its_group() {
        #[allow(clippy::type_complexity)]
        fn d(code: &[u8]) -> Option<(usize, usize, usize, u8, u8, usize)> {
            decode_array_accumulate(&padded(code), code.len(), 0)
        }
        assert_eq!(
            d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x3e]),
            Some((3, 0, 2, 0x2e, 0x60, 3))
        );
        assert_eq!(
            d(&[0x1a, 0x2d, 0x1b, 0x34, 0x82, 0x3c]),
            Some((0, 3, 1, 0x34, 0x82, 1))
        );
        // laload / faload / aaload, a throwing or a shift arithmetic, a
        // long arithmetic, a store that is not `istore_0..3`, a cut group.
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2f, 0x61, 0x3e]), None);
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x30, 0x62, 0x3e]), None);
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x32, 0x60, 0x3e]), None);
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x6c, 0x3e]), None);
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x78, 0x3e]), None);
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x61, 0x3e]), None);
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x36, 0x05]), None);
        assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60]), None);
        assert_eq!(
            decode_array_accumulate(&padded(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x3e]), 5, 0),
            None
        );
    }

    #[test]
    fn an_accumulate_hit_stores_the_unfused_result() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let shared = vm(true);
        // (element type, load, element bits as stored, element as loaded)
        let kinds: [(ArrayElementType, u8, i64, i32); 5] = [
            (ArrayElementType::Int, 0x2e, -7, -7),
            (ArrayElementType::Byte, 0x33, -2, -2),
            (ArrayElementType::Boolean, 0x33, 1, 1),
            (ArrayElementType::Char, 0x34, 0xfffe, 0xfffe),
            (ArrayElementType::Short, 0x35, -3, -3),
        ];
        for (et, load, stored, elem) in kinds {
            let Some(arr) = zgc.try_alloc_array(ClassId::new(0), et, 3) else {
                return;
            };
            let store_op = load + 0x21;
            let (cv, kind) = (CompactValue::int(stored as i32), 0);
            assert!(array_store_prim(&zgc, arr, 1, store_op, cv, kind), "{et:?}");
            for (arith, want) in [
                (0x60_u8, 100i32.wrapping_add(elem)),
                (0x64, 100i32.wrapping_sub(elem)),
                (0x68, 100i32.wrapping_mul(elem)),
                (0x7e, 100 & elem),
                (0x80, 100 | elem),
                (0x82, 100 ^ elem),
            ] {
                // Into the accumulator (`s += a[i]`) and into another local
                // (`t = s + a[i]`, local 1).
                for (store, z) in [(0x3e_u8, 3usize), (0x3c, 1)] {
                    let mut f = accumulate_frame(
                        load,
                        arith,
                        store,
                        Value::Object(Some(arr)),
                        Value::Int(1),
                        100,
                    );
                    let code_len = f.code.len() - 2;
                    assert_eq!(
                        fused_array_accumulate(&shared, &zgc, &mut f, code_len, 0),
                        Some(6),
                        "{et:?} {arith:#x}"
                    );
                    assert_eq!(f.stack.len(), 0, "nothing pushed");
                    assert_eq!(
                        f.get_local_compact_unchecked(z).as_int(),
                        Some(want),
                        "{et:?} {arith:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_accumulate_decline_writes_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let shared = vm(true);
        let Some(ints) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 2) else {
            return;
        };
        let cases: [(u8, Value, Value); 6] = [
            // Out of range either side (the unfused `iaload` throws).
            (0x2e, Value::Object(Some(ints)), Value::Int(2)),
            (0x2e, Value::Object(Some(ints)), Value::Int(-1)),
            // A null array (the unfused `iaload` throws the NPE).
            (0x2e, Value::Object(None), Value::Int(0)),
            // An element type the opcode does not name.
            (0x33, Value::Object(Some(ints)), Value::Int(0)),
            (0x34, Value::Object(Some(ints)), Value::Int(0)),
            // An index local that is not an int.
            (0x2e, Value::Object(Some(ints)), Value::Float(0.0)),
        ];
        for (load, arr, index) in cases {
            let mut f = accumulate_frame(load, 0x60, 0x3e, arr, index, 5);
            let code_len = f.code.len() - 2;
            assert_eq!(
                fused_array_accumulate(&shared, &zgc, &mut f, code_len, 0),
                None,
                "{load:#x}"
            );
            assert_eq!(f.stack.len(), 0);
            assert_eq!(
                f.get_local_compact_unchecked(3).as_int(),
                Some(5),
                "{load:#x}"
            );
        }
        // An accumulator local that is not an int.
        let mut f = frame_with(
            vec![0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x3e, 0xb1],
            &[
                Value::Object(Some(ints)),
                Value::Int(0),
                Value::Int(0),
                Value::Float(1.0),
            ],
        );
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_accumulate(&shared, &zgc, &mut f, code_len, 0),
            None
        );
        assert_eq!(f.stack.len(), 0);
    }
}

/// nio21 wave 8, lane L3 (`r21-reference-array-unsafe-cas-is-lock-based-FIXED-20261008.md`):
/// the host's `Nio21CasRace` rows `ref-array` / `ara` lost markers on the w6
/// binary in both modes and with the JIT off. The cause was the collector, not
/// the interpreter: ZGC is the DEFAULT backend, and the w6 hardware
/// reference-element CAS declined ZGC, so every default-build reference CAS was
/// still the `with_cas_lock` read-compare-write that a plain `aastore` (this
/// file's `array_store_ref`) or a native `set_array_element` never takes. Wave
/// 7 admitted an unarmed ZGC heap. These tests pin the PAIR the probe races, on
/// every backend: the interpreter's quickened plain `aastore` (ZGC) or the
/// heap's plain element store, against `compare_and_swap_field_shared`.
#[cfg(test)]
mod nio21_w8_ref_array_store_cas_race_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::sync::Arc;

    /// A component class id no fresh VM defines, so the quickened `aastore`'s
    /// covariance memo answers "`Object[]`" and serves the store.
    const COMPONENT: u32 = 77_777;
    const MARKERS: usize = 300;
    const SPINS: usize = 128;

    fn vm(gc_algorithm: crate::config::GcAlgorithm) -> Arc<SharedVm> {
        Arc::new(SharedVm::new(crate::config::VmConfig {
            gc_algorithm,
            ..crate::config::VmConfig::default()
        }))
    }

    fn raw_of(v: Value) -> u64 {
        match v {
            Value::Object(Some(r)) => r.as_ptr() as u64,
            _ => 0,
        }
    }

    /// Every backend, the default one first, takes the HARDWARE
    /// reference-element CAS. The w6 tests would have failed this on the
    /// default (ZGC) backend; it fails again if any backend is re-declined.
    #[test]
    fn nio21_w8_every_backend_serves_a_reference_element_cas_in_hardware() {
        if cratonvm_types::zgc_read_barrier_armed() {
            return; // an armed ZGC keeps the lock path by design (ZGC page)
        }
        let mut backends = vec![
            crate::config::VmConfig::default().gc_algorithm,
            crate::config::GcAlgorithm::Generational,
            crate::config::GcAlgorithm::G1,
        ];
        backends.dedup();
        for gc in backends {
            let shared = vm(gc);
            let heap = &shared.mem.heap;
            let arr = heap.alloc_array(ClassId::new(COMPONENT), ArrayElementType::Reference, 2);
            let a = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
            let served = crate::vm::vm_exec::hw_cas_reference_array_element(
                &shared,
                arr,
                1,
                Value::Object(None),
                Value::Object(Some(a)),
            );
            assert_eq!(
                served,
                Some((0, true)),
                "{gc:?}: the element CAS must be one LOCK CMPXCHG"
            );
            assert_eq!(
                raw_of(heap.get_array_element(arr, 1).unwrap()),
                a.as_ptr() as u64,
                "{gc:?}"
            );
        }
    }

    /// One writer stores marker `j` (an object of group `j`) with a PLAIN
    /// store; two CAS threads toggle the element between the two objects of
    /// whichever group they read. Once the writer has stored group `j`, no read
    /// may see a lower group: a CAS that expected an older object must fail. A
    /// lost marker is a CAS that landed over a plain store. No ABA is possible:
    /// group `j - 1`'s objects never return to the slot once `j` is stored.
    fn race_on(gc: crate::config::GcAlgorithm) {
        let shared = vm(gc);
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(ClassId::new(COMPONENT), ArrayElementType::Reference, 1);
        let mut w = Vec::with_capacity(MARKERS + 1);
        let mut d = Vec::with_capacity(MARKERS + 1);
        let mut group: HashMap<u64, usize> = HashMap::new();
        for j in 0..=MARKERS {
            let wj = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
            let dj = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
            group.insert(wj.as_ptr() as u64, j);
            group.insert(dj.as_ptr() as u64, j);
            w.push(wj.as_ptr() as u64);
            d.push(dj.as_ptr() as u64);
        }
        // SAFETY: `w[0]` is the live marker just allocated.
        let w0 = unsafe { ObjectRef::from_raw(w[0] as *mut u8) };
        heap.set_array_element(arr, 0, Value::Object(Some(w0)))
            .expect("in range");
        let arr_addr = arr.as_ptr() as u64;
        let lost = AtomicU64::new(0);
        let fast_hits = AtomicUsize::new(0);
        let done = AtomicBool::new(false);
        let (shared_ref, group, w, d): (&SharedVm, &HashMap<u64, usize>, &Vec<u64>, &Vec<u64>) =
            (&*shared, &group, &w, &d);
        let (lost, fast_hits, done) = (&lost, &fast_hits, &done);
        // SAFETY (every use): the array and the markers are live for the whole
        // test, and nothing allocates or collects while the threads run.
        let obj = |raw: u64| unsafe { ObjectRef::from_raw(raw as *mut u8) };
        let read = move || {
            let raw = raw_of(
                shared_ref
                    .mem
                    .heap
                    .get_array_element(obj(arr_addr), 0)
                    .unwrap(),
            );
            *group.get(&raw).expect("the element holds a marker")
        };
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(move || {
                    let arr = obj(arr_addr);
                    let mut top = 0usize;
                    while !done.load(Ordering::Acquire) {
                        let cur = raw_of(shared_ref.mem.heap.get_array_element(arr, 0).unwrap());
                        let g = group[&cur];
                        if g < top {
                            lost.fetch_add(1, Ordering::Relaxed);
                        }
                        top = top.max(g);
                        let next = if cur == w[g] { d[g] } else { w[g] };
                        let _ = crate::vm::vm_exec::compare_and_swap_field_shared(
                            shared_ref,
                            arr,
                            0,
                            Value::Object(Some(obj(cur))),
                            Value::Object(Some(obj(next))),
                        );
                    }
                });
            }
            s.spawn(move || {
                let arr = obj(arr_addr);
                let zgc: Option<&ZgcRealHeap> = match &shared_ref.mem.heap {
                    crate::memory::vm_heap::VmHeap::Zgc(h) => Some(&**h),
                    _ => None,
                };
                for j in 1..=MARKERS {
                    // Odd markers through the interpreter's quickened plain
                    // `aastore` where the backend has one; the rest (and any
                    // decline) through the heap's plain element store, the one
                    // `putReferenceRelease` / `AtomicReferenceArray.lazySet`
                    // reach.
                    let served = j % 2 == 1
                        && zgc.is_some_and(|z| {
                            array_store_ref(
                                shared_ref,
                                z,
                                arr,
                                0,
                                CompactValue::object(w[j]),
                                0,
                                false,
                            )
                        });
                    if served {
                        fast_hits.fetch_add(1, Ordering::Relaxed);
                    } else {
                        shared_ref
                            .mem
                            .heap
                            .set_array_element(arr, 0, Value::Object(Some(obj(w[j]))))
                            .expect("in range");
                    }
                    for _ in 0..SPINS {
                        if read() < j {
                            lost.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    std::thread::yield_now();
                }
                done.store(true, Ordering::Release);
            });
        });
        assert_eq!(
            lost.load(Ordering::Relaxed),
            0,
            "{gc:?}: a CAS overwrote a plain store"
        );
        assert_eq!(read(), MARKERS, "{gc:?}: the last marker survives");
        if matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Zgc(_)) {
            assert!(
                fast_hits.load(Ordering::Relaxed) > 0,
                "the quickened aastore arm must take part in the race"
            );
        }
    }

    #[test]
    fn nio21_w8_plain_store_never_lost_to_a_reference_cas_on_the_default_backend() {
        if cratonvm_types::zgc_read_barrier_armed() {
            return;
        }
        race_on(crate::config::VmConfig::default().gc_algorithm);
    }

    #[test]
    fn nio21_w8_plain_store_never_lost_to_a_reference_cas_on_generational() {
        race_on(crate::config::GcAlgorithm::Generational);
    }

    #[test]
    fn nio21_w8_plain_store_never_lost_to_a_reference_cas_on_g1() {
        race_on(crate::config::GcAlgorithm::G1);
    }
}

#[cfg(test)]
mod i74_l7_array_shift_div_and_no_dup_tests {
    //! Interpreter round i3 wave 74, lane L7: the fused array element
    //! increment's widened arithmetic
    //! (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SHIFT_DIV`,
    //! [`ARRAY_INC_SHIFT_DIV`]) and its `dup2`-less local entry
    //! (`CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP`, [`ARRAY_INC_NO_DUP`]).
    //! A shift / division hit stores what the unfused `ishl` / `idiv` /
    //! ... stores; a zero divisor declines having written nothing (the
    //! unfused `idiv` throws); without the bit the decoder answers exactly as
    //! before; the no-dup group is decoded as its `dup2` twin. Each test sets
    //! the switches it needs explicitly.
    use super::*;
    use crate::runtime::interpreter::IncrementOperand;
    use std::sync::Arc;

    type Decoded = (u8, IncrementOperand, u8, usize, FieldIncrementUse);

    /// A static frame over `code`: local 0 = `arr`, local 1 = `index`, local
    /// 2 = `3`; `max_locals` 4.
    fn frame(code: Vec<u8>, arr: Value, index: i32) -> Frame {
        Frame::new(
            ClassId::new(0x00ab_0741),
            "cratonvm/test/i74l7/ArrayShiftDiv".to_string(),
            "m".to_string(),
            "([III)V".to_string(),
            None,
            code,
            vec![],
            6,
            4,
            &[arr, Value::Int(index), Value::Int(3)],
        )
    }

    /// `aload_0; iload_1; dup2; <load>; <operand>; <arith>; [conv];
    /// <load's store>; return`.
    fn group(load: u8, operand: &[u8], arith: u8, conv: Option<u8>) -> Vec<u8> {
        let mut code = vec![0x2a, 0x1b, 0x5c, load];
        code.extend_from_slice(operand);
        code.push(arith);
        code.extend(conv);
        code.push(load + 0x21);
        code.push(0xb1);
        code
    }

    fn decode(code: &[u8], p: usize, mode: u8) -> Option<Decoded> {
        let f = frame(code.to_vec(), Value::Object(None), 0);
        let code_len = f.code.len() - 2;
        decode_array_increment_mode(&f, &f.code, code_len, p, mode)
    }

    fn array(zgc: &ZgcRealHeap, et: ArrayElementType, len: usize) -> Option<ObjectRef> {
        zgc.try_alloc_array(ClassId::new(0), et, len)
    }

    fn get(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, load: u8) -> Option<i64> {
        let mut s = ValueStack::new(4);
        if !array_load_prim(zgc, &mut s, arr, index, load) {
            return None;
        }
        let cv = s.pop_compact();
        if load == 0x2f {
            Some(cv.as_long_unchecked())
        } else {
            cv.as_int().map(i64::from)
        }
    }

    fn set(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, store: u8, v: i64) {
        let (cv, kind) = if store == 0x50 {
            (CompactValue::long(v), ValueStack::KIND_MARK_LONG)
        } else {
            // Cast: test values are in the element's range.
            (CompactValue::int(v as i32), 0)
        };
        assert!(array_store_prim(zgc, arr, index, store, cv, kind));
    }

    fn body(zgc: &ZgcRealHeap, f: &Frame, arr: ObjectRef, index: i32, mode: u8) -> Option<usize> {
        let code_len = f.code.len() - 2;
        let ptr = arr.as_ptr() as u64;
        fused_array_increment_body(
            zgc,
            f,
            code_len,
            2,
            ptr,
            index,
            mode,
            false,
            true,
            NO_DECLINE_MEMO,
            body_test_vm(),
            &mut no_sites(),
        )
        .map(|r| r.0)
    }

    #[test]
    fn the_decoder_takes_the_shifts_and_divisions_only_under_the_bit() {
        for arith in [0x6c_u8, 0x70, 0x78, 0x7a, 0x7c] {
            let code = group(0x2e, &[0x04], arith, None);
            let end = code.len() - 1;
            assert_eq!(decode(&code, 2, 0), None, "{arith:#x}");
            assert_eq!(decode(&code, 2, ARRAY_INC_VALUE), None, "{arith:#x}");
            assert_eq!(
                decode(&code, 2, ARRAY_INC_SHIFT_DIV),
                Some((
                    0x2e,
                    IncrementOperand::Int(1),
                    arith,
                    end,
                    FieldIncrementUse::Statement
                )),
                "{arith:#x}"
            );
        }
        // A narrow element keeps its own conversion.
        let code = group(0x33, &[0x04], 0x78, Some(0x91));
        assert!(decode(&code, 2, ARRAY_INC_SHIFT_DIV).is_some());
        assert_eq!(
            decode(&group(0x33, &[0x04], 0x78, None), 2, ARRAY_INC_SHIFT_DIV),
            None
        );
        // Long divisions take a long operand; long shifts an int count.
        for arith in [0x6d_u8, 0x71] {
            let code = group(0x2f, &[0x0a], arith, None);
            let end = code.len() - 1;
            assert_eq!(decode(&code, 2, 0), None);
            assert_eq!(
                decode(&code, 2, ARRAY_INC_SHIFT_DIV),
                Some((
                    0x2f,
                    IncrementOperand::Long(1),
                    arith,
                    end,
                    FieldIncrementUse::Statement
                ))
            );
            assert_eq!(
                decode(&group(0x2f, &[0x04], arith, None), 2, ARRAY_INC_SHIFT_DIV),
                None
            );
        }
        for arith in [0x79_u8, 0x7b, 0x7d] {
            let code = group(0x2f, &[0x10, 0x41], arith, None);
            let end = code.len() - 1;
            assert_eq!(decode(&code, 2, 0), None);
            assert_eq!(
                decode(&code, 2, ARRAY_INC_SHIFT_DIV),
                Some((
                    0x2f,
                    IncrementOperand::Long(65),
                    arith,
                    end,
                    FieldIncrementUse::Statement
                ))
            );
            // A long operand under a long shift is not a verified shape.
            assert_eq!(
                decode(&group(0x2f, &[0x0a], arith, None), 2, ARRAY_INC_SHIFT_DIV),
                None
            );
        }
        // An int count does not open the other long arithmetic.
        assert_eq!(
            decode(&group(0x2f, &[0x04], 0x61, None), 2, ARRAY_INC_SHIFT_DIV),
            None
        );
        // A long shift under an int element, an int shift under a long one.
        assert_eq!(
            decode(&group(0x2e, &[0x04], 0x79, None), 2, ARRAY_INC_SHIFT_DIV),
            None
        );
        assert_eq!(
            decode(&group(0x2f, &[0x04], 0x78, None), 2, ARRAY_INC_SHIFT_DIV),
            None
        );
        // Float arithmetic stays refused.
        assert_eq!(
            decode(&group(0x2e, &[0x04], 0x6e, None), 2, ARRAY_INC_SHIFT_DIV),
            None
        );
    }

    #[test]
    fn the_widened_decoder_answers_every_old_hit_alike() {
        let loads: [u8; 6] = [0x2e, 0x2f, 0x33, 0x34, 0x35, 0x30];
        let operands: [&[u8]; 7] = [
            &[0x02],
            &[0x04],
            &[0x10, 0xfe],
            &[0x1c],
            &[0x0a],
            &[0x09],
            &[0x15, 0x02],
        ];
        let convs: [Option<u8>; 4] = [None, Some(0x91), Some(0x92), Some(0x93)];
        let mut hits = 0usize;
        for load in loads {
            for operand in operands {
                for arith in 0x60_u8..=0x83 {
                    for conv in convs {
                        let code = group(load, operand, arith, conv);
                        for value in [0, ARRAY_INC_VALUE] {
                            if let Some(old) = decode(&code, 2, value) {
                                hits += 1;
                                assert_eq!(
                                    decode(&code, 2, value | ARRAY_INC_SHIFT_DIV),
                                    Some(old),
                                    "{code:x?}"
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(hits > 0);
    }

    #[test]
    fn the_no_dup_group_decodes_as_its_dup2_twin() {
        let tails: [Vec<u8>; 5] = [
            vec![0x2e, 0x04, 0x60, 0x4f],
            vec![0x2e, 0x1c, 0x68, 0x4f],
            vec![0x33, 0x04, 0x64, 0x91, 0x54],
            vec![0x2e, 0x04, 0x60, 0x5b, 0x4f],
            vec![0x2e, 0x04, 0x78, 0x4f],
        ];
        let modes = [
            0,
            ARRAY_INC_VALUE,
            ARRAY_INC_SHIFT_DIV,
            ARRAY_INC_VALUE | ARRAY_INC_SHIFT_DIV,
        ];
        for tail in tails {
            for mode in modes {
                // `aload_0; iload_1; dup2; <tail>; return` against
                // `aload_0; iload_1; aload_0; iload_1; <tail>; return`.
                let mut dup = vec![0x2a, 0x1b, 0x5c];
                dup.extend_from_slice(&tail);
                dup.push(0xb1);
                let mut no_dup = vec![0x2a, 0x1b, 0x2a, 0x1b];
                no_dup.extend_from_slice(&tail);
                no_dup.push(0xb1);
                let want = decode(&dup, 2, mode).map(|d| (d.0, d.1, d.2, d.3 + 1, d.4));
                assert_eq!(
                    decode(&no_dup, 4, mode | ARRAY_INC_NO_DUP),
                    want,
                    "{tail:x?} {mode}"
                );
                // Without the bit the `*aload` is not a `dup2`.
                assert_eq!(decode(&no_dup, 4, mode), None);
            }
        }
    }

    #[test]
    fn a_shift_or_division_hit_stores_what_the_unfused_group_stores() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (imin, imax) = (i64::from(i32::MIN), i64::from(i32::MAX));
        // (element type, load, conversion, operand, arith, before, after)
        #[allow(clippy::type_complexity)]
        let cases: [(ArrayElementType, u8, Option<u8>, &[u8], u8, i64, i64); 19] = [
            (ArrayElementType::Int, 0x2e, None, &[0x04], 0x78, 3, 6),
            // The count's low five bits: 33 -> 1, -1 -> 31.
            (ArrayElementType::Int, 0x2e, None, &[0x10, 0x21], 0x78, 3, 6),
            (
                ArrayElementType::Int,
                0x2e,
                None,
                &[0x10, 0xff],
                0x78,
                1,
                imin,
            ),
            (ArrayElementType::Int, 0x2e, None, &[0x04], 0x7a, -7, -4),
            (ArrayElementType::Int, 0x2e, None, &[0x04], 0x7c, -2, imax),
            (ArrayElementType::Int, 0x2e, None, &[0x06], 0x6c, -7, -2),
            (ArrayElementType::Int, 0x2e, None, &[0x06], 0x70, -7, -1),
            (ArrayElementType::Int, 0x2e, None, &[0x1c], 0x6c, 10, 3),
            (ArrayElementType::Int, 0x2e, None, &[0x02], 0x6c, imin, imin),
            (ArrayElementType::Int, 0x2e, None, &[0x02], 0x70, imin, 0),
            (
                ArrayElementType::Byte,
                0x33,
                Some(0x91),
                &[0x04],
                0x78,
                64,
                -128,
            ),
            (
                ArrayElementType::Char,
                0x34,
                Some(0x92),
                &[0x04],
                0x7a,
                0xffff,
                0x7fff,
            ),
            (
                ArrayElementType::Short,
                0x35,
                Some(0x93),
                &[0x05],
                0x6c,
                -32768,
                -16384,
            ),
            (ArrayElementType::Long, 0x2f, None, &[0x04], 0x79, 1, 2),
            // The count's low six bits: 65 -> 1.
            (
                ArrayElementType::Long,
                0x2f,
                None,
                &[0x10, 0x41],
                0x79,
                1,
                2,
            ),
            (ArrayElementType::Long, 0x2f, None, &[0x04], 0x7b, -3, -2),
            (
                ArrayElementType::Long,
                0x2f,
                None,
                &[0x04],
                0x7d,
                -2,
                i64::MAX,
            ),
            (
                ArrayElementType::Long,
                0x2f,
                None,
                &[0x0a],
                0x6d,
                i64::MIN,
                i64::MIN,
            ),
            (ArrayElementType::Long, 0x2f, None, &[0x0a], 0x71, -9, 0),
        ];
        for (et, load, conv, operand, arith, before, after) in cases {
            let Some(arr) = array(&zgc, et, 3) else {
                return;
            };
            for i in 0..3 {
                set(&zgc, arr, i, load + 0x21, if i == 1 { before } else { 0 });
            }
            let code = group(load, operand, arith, conv);
            let end = code.len() - 1;
            let f = frame(code, Value::Object(Some(arr)), 1);
            // Without the bit: a decline, nothing written.
            assert_eq!(body(&zgc, &f, arr, 1, 0), None, "{load:#x} {arith:#x}");
            assert_eq!(
                get(&zgc, arr, 1, load),
                Some(before),
                "{load:#x} {arith:#x}"
            );
            assert_eq!(
                body(&zgc, &f, arr, 1, ARRAY_INC_SHIFT_DIV),
                Some(end),
                "{load:#x} {arith:#x}"
            );
            assert_eq!(
                get(&zgc, arr, 1, load),
                Some(after),
                "{load:#x} {arith:#x} {before}"
            );
            assert_eq!(get(&zgc, arr, 0, load), Some(0));
            assert_eq!(get(&zgc, arr, 2, load), Some(0));
        }
    }

    #[test]
    fn a_zero_divisor_declines_having_written_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(ints), Some(longs)) = (
            array(&zgc, ArrayElementType::Int, 2),
            array(&zgc, ArrayElementType::Long, 2),
        ) else {
            return;
        };
        set(&zgc, ints, 0, 0x4f, 7);
        set(&zgc, longs, 0, 0x50, 7);
        for arith in [0x6c_u8, 0x70] {
            let f = frame(
                group(0x2e, &[0x03], arith, None),
                Value::Object(Some(ints)),
                0,
            );
            assert_eq!(body(&zgc, &f, ints, 0, ARRAY_INC_SHIFT_DIV), None);
            assert_eq!(get(&zgc, ints, 0, 0x2e), Some(7));
        }
        for arith in [0x6d_u8, 0x71] {
            let f = frame(
                group(0x2f, &[0x09], arith, None),
                Value::Object(Some(longs)),
                0,
            );
            assert_eq!(body(&zgc, &f, longs, 0, ARRAY_INC_SHIFT_DIV), None);
            assert_eq!(get(&zgc, longs, 0, 0x2f), Some(7));
        }
    }

    fn vm(no_dup: bool, shift_div: bool) -> Arc<SharedVm> {
        let mut config = crate::config::VmConfig::default();
        config.interp_fused_array_increment = true;
        config.interp_fused_array_increment_value = true;
        config.interp_fused_array_increment_top = false;
        config.interp_fused_array_increment_dup2 = false;
        config.interp_fused_array_accumulate = false;
        config.interp_fused_array_decline_memo = false;
        config.interp_fused_array_increment_array_operand = false;
        config.interp_fused_array_increment_static_operand = false;
        config.interp_fused_array_increment_shift_div = shift_div;
        config.interp_fused_array_update_no_dup = no_dup;
        config.site_census = false;
        Arc::new(SharedVm::new(config))
    }

    #[test]
    fn the_local_entry_serves_the_no_dup_update_only_under_its_switch() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = array(&zgc, ArrayElementType::Int, 2) else {
            return;
        };
        // `a[i] = a[i] + 1` over locals 0 / 1.
        let plus = vec![0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x04, 0x60, 0x4f, 0xb1];
        for on in [false, true] {
            set(&zgc, arr, 1, 0x4f, 5);
            let shared = vm(on, false);
            let mut f = frame(plus.clone(), Value::Object(Some(arr)), 1);
            let code_len = f.code.len() - 2;
            let got =
                fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
            assert_eq!(f.stack.len(), 0);
            if on {
                assert_eq!(got, Some(8));
                assert_eq!(get(&zgc, arr, 1, 0x2e), Some(6));
            } else {
                assert_eq!(got, None);
                assert_eq!(get(&zgc, arr, 1, 0x2e), Some(5));
            }
        }
        let shared = vm(true, false);
        // Another index local, another array local: not the same element.
        for code in [
            vec![0x2a, 0x1b, 0x2a, 0x1c, 0x2e, 0x04, 0x60, 0x4f, 0xb1],
            vec![0x2a, 0x1b, 0x2b, 0x1b, 0x2e, 0x04, 0x60, 0x4f, 0xb1],
        ] {
            set(&zgc, arr, 1, 0x4f, 5);
            let mut f = frame(code, Value::Object(Some(arr)), 1);
            let code_len = f.code.len() - 2;
            assert_eq!(
                fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
                None
            );
            assert_eq!(get(&zgc, arr, 1, 0x2e), Some(5));
        }
        // `x = a[i] = a[i] + 1` (the value form): the new element is left.
        let pre = vec![0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x04, 0x60, 0x5b, 0x4f, 0xb1];
        set(&zgc, arr, 1, 0x4f, 5);
        let mut f = frame(pre, Value::Object(Some(arr)), 1);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            Some(9)
        );
        assert_eq!(f.stack.len(), 1);
        assert_eq!(f.stack.pop_int_unchecked(), 6);
        // `a[i] = a[i] << 1`: needs the shift switch too.
        let shl = vec![0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x04, 0x78, 0x4f, 0xb1];
        for shift_div in [false, true] {
            set(&zgc, arr, 1, 0x4f, 5);
            let shared = vm(true, shift_div);
            let mut f = frame(shl.clone(), Value::Object(Some(arr)), 1);
            let code_len = f.code.len() - 2;
            let got =
                fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
            assert_eq!(got, shift_div.then_some(8));
            assert_eq!(
                get(&zgc, arr, 1, 0x2e),
                Some(if shift_div { 10 } else { 5 })
            );
        }
        // Out of range and a null array: declines (the unfused `iaload`
        // throws), nothing written, nothing pushed.
        set(&zgc, arr, 1, 0x4f, 5);
        let mut f = frame(plus.clone(), Value::Object(Some(arr)), 2);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 0);
        let mut f = frame(plus, Value::Object(None), 1);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 0);
        assert_eq!(get(&zgc, arr, 1, 0x2e), Some(5));
    }
}

#[cfg(test)]
mod i75_l7_array_decline_memo_tests {
    //! Interpreter round i3 wave 75, lane L7: the fused array increment's
    //! per-method decline memo (`CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`,
    //! `array_increment_declined` / `note_array_increment_decline`). A group
    //! the decoder refuses on its bytes sets its entry pc's bit, under the
    //! switch only; a per-execution decline (null array, index out of range,
    //! zero divisor) sets nothing; the bit is per pc, so a hit elsewhere in
    //! the method still fuses; a frame without a cached method entry keeps no
    //! memo. Each test sets the switches it needs explicitly.
    use super::*;
    use crate::classloading::resolution::CachedBytecodeMethod;
    use std::sync::Arc;

    fn vm(memo: bool) -> Arc<SharedVm> {
        let mut config = crate::config::VmConfig::default();
        config.interp_fused_array_increment = true;
        config.interp_fused_array_increment_value = true;
        config.interp_fused_array_increment_top = true;
        config.interp_fused_array_increment_dup2 = true;
        config.interp_fused_array_accumulate = false;
        config.interp_fused_array_increment_shift_div = true;
        config.interp_fused_array_update_no_dup = true;
        config.interp_fused_array_decline_memo = memo;
        config.interp_fused_array_increment_array_operand = false;
        config.interp_fused_array_increment_static_operand = false;
        config.site_census = false;
        Arc::new(SharedVm::new(config))
    }

    /// A static frame pushed from a fresh `CachedBytecodeMethod` over `code`
    /// (padded as the cached-invoke path pads it), `args` in its first
    /// locals, `max_locals` 4.
    fn cached_frame(code: &[u8], args: &[Value]) -> Frame {
        let cm = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0x00ab_0751),
                class_name: Arc::from("cratonvm/test/i75l7/DeclineMemo"),
                method_name: Arc::from("m"),
                method_descriptor: Arc::from("([III)V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(code),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 6,
                max_locals: 4,
                num_params: 3,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let (mut locals_pool, mut stacks_pool) = (Vec::new(), Vec::new());
        Frame::new_pooled_cached(cm, args, &mut locals_pool, &mut stacks_pool)
    }

    /// The same frame built the owned way (no cached method entry).
    fn owned_frame(code: &[u8], args: &[Value]) -> Frame {
        Frame::new(
            ClassId::new(0x00ab_0752),
            "cratonvm/test/i75l7/DeclineMemoOwned".to_string(),
            "m".to_string(),
            "([III)V".to_string(),
            None,
            code.to_vec(),
            vec![],
            6,
            4,
            args,
        )
    }

    fn ints(zgc: &ZgcRealHeap, len: usize) -> Option<ObjectRef> {
        zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, len)
    }

    fn get(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32) -> Option<i32> {
        let mut s = ValueStack::new(4);
        if !array_load_prim(zgc, &mut s, arr, index, 0x2e) {
            return None;
        }
        s.pop_compact().as_int()
    }

    /// `a[j] += SF` (`aload_0; iload_1; dup2; iaload; getstatic #1; iadd;
    /// iastore`), pc 0..=8: a group the decoder refuses on its bytes.
    const STATIC_OPERAND: [u8; 9] = [0x2a, 0x1b, 0x5c, 0x2e, 0xb2, 0x00, 0x01, 0x60, 0x4f];
    /// `a[j]++` (`aload_0; iload_1; dup2; iaload; iconst_1; iadd; iastore`).
    const INCREMENT: [u8; 7] = [0x2a, 0x1b, 0x5c, 0x2e, 0x04, 0x60, 0x4f];

    /// `bytes` then `return`.
    fn method(bytes: &[u8]) -> Vec<u8> {
        let mut code = bytes.to_vec();
        code.push(0xb1);
        code
    }

    fn args(arr: Option<ObjectRef>, index: i32) -> [Value; 3] {
        [Value::Object(arr), Value::Int(index), Value::Int(3)]
    }

    #[test]
    fn a_shape_decline_is_remembered_only_under_the_switch() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = ints(&zgc, 4) else {
            return;
        };
        let code = method(&STATIC_OPERAND);
        for memo in [false, true] {
            let shared = vm(memo);
            let mut f = cached_frame(&code, &args(Some(arr), 1));
            let code_len = f.code.len() - 2;
            assert!(!array_increment_declined(&f, 0));
            assert_eq!(
                fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
                None
            );
            assert_eq!(f.stack.len(), 0, "a decline pushes nothing");
            assert_eq!(array_increment_declined(&f, 0), memo, "memo {memo}");
            // Only the entry's own pc.
            for pc in 1..code.len() {
                assert!(!array_increment_declined(&f, pc), "pc {pc}");
            }
            assert_eq!(get(&zgc, arr, 1), Some(0), "nothing written");
        }
    }

    #[test]
    fn each_entry_remembers_its_own_pc() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = ints(&zgc, 4) else {
            return;
        };
        let shared = vm(true);
        // The stack-top entry at its `iload_1`.
        let mut f = cached_frame(&method(&STATIC_OPERAND[1..]), &args(Some(arr), 1));
        f.stack.push(Value::Object(Some(arr))).unwrap();
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 1, "the array slot is left");
        assert!(array_increment_declined(&f, 0));
        // The `dup2` entry at its `dup2`.
        let mut f = cached_frame(&method(&STATIC_OPERAND[2..]), &args(Some(arr), 1));
        f.stack.push(Value::Object(Some(arr))).unwrap();
        f.stack.push_int(1).unwrap();
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 2, "both slots are left");
        assert!(array_increment_declined(&f, 0));
        // The no-dup prefix that is not an update of the same element
        // (`a[j] = a[k] + 1`): refused on its bytes, remembered.
        let code = method(&[0x2a, 0x1b, 0x2a, 0x1c, 0x2e, 0x04, 0x60, 0x4f]);
        let mut f = cached_frame(&code, &args(Some(arr), 1));
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert!(array_increment_declined(&f, 0));
        assert_eq!(get(&zgc, arr, 1), Some(0));
    }

    #[test]
    fn a_per_execution_decline_is_not_remembered() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = ints(&zgc, 4) else {
            return;
        };
        let shared = vm(true);
        let code = method(&INCREMENT);
        // A null array, an index out of range either side.
        for (a, index) in [(None, 1), (Some(arr), 4), (Some(arr), -1)] {
            let mut f = cached_frame(&code, &args(a, index));
            let code_len = f.code.len() - 2;
            assert_eq!(
                fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
                None
            );
            assert!(!array_increment_declined(&f, 0), "{index}");
        }
        // A zero divisor (`a[j] /= 0`): the unfused `idiv` throws, every time.
        let div = method(&[0x2a, 0x1b, 0x5c, 0x2e, 0x03, 0x6c, 0x4f]);
        let mut f = cached_frame(&div, &args(Some(arr), 1));
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert!(!array_increment_declined(&f, 0));
        // The increment hits once its array and index are good.
        let mut f = cached_frame(&code, &args(Some(arr), 1));
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            Some(7)
        );
        assert!(!array_increment_declined(&f, 0));
        assert_eq!(get(&zgc, arr, 1), Some(1));
    }

    #[test]
    fn a_remembered_site_leaves_the_other_sites_of_its_method_fused() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = ints(&zgc, 4) else {
            return;
        };
        let shared = vm(true);
        // `a[j] += SF; a[j]++; return`: the second group starts at pc 9.
        let mut bytes = STATIC_OPERAND.to_vec();
        bytes.extend_from_slice(&INCREMENT);
        let mut f = cached_frame(&method(&bytes), &args(Some(arr), 2));
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert!(array_increment_declined(&f, 0));
        assert!(!array_increment_declined(&f, 9));
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 9, &mut no_sites()),
            Some(16)
        );
        assert!(!array_increment_declined(&f, 9));
        assert_eq!(get(&zgc, arr, 2), Some(1));
        // A pc past the bitmap answers "not declined".
        assert!(!array_increment_declined(&f, 1 << 20));
    }

    #[test]
    fn a_frame_without_a_cached_method_keeps_no_memo() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(arr) = ints(&zgc, 4) else {
            return;
        };
        let shared = vm(true);
        let mut f = owned_frame(&method(&STATIC_OPERAND), &args(Some(arr), 1));
        assert!(f.cached_method().is_none());
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert!(!array_increment_declined(&f, 0));
    }
}

#[cfg(test)]
mod i75_l7_wide_local_element_read_tests {
    //! Interpreter round i3 wave 75, lane L7: the element read with a local
    //! past 3 (`CRATONVM_INTERP_FUSED_WIDE_LOCALS`,
    //! `array_load_wide_locals`). A hit pushes what the unfused `aload;
    //! iload; <x>aload` pushes and returns the pc past the `*aload`; every
    //! decline pushes nothing.
    use super::*;

    /// A static frame over `code`, `max_locals` 6, `args` in its first
    /// locals.
    fn frame(code: Vec<u8>, args: &[Value]) -> Frame {
        Frame::new(
            ClassId::new(0x00ab_0753),
            "cratonvm/test/i75l7/WideLocals".to_string(),
            "m".to_string(),
            "([IIIIIF)V".to_string(),
            None,
            code,
            vec![],
            4,
            6,
            args,
        )
    }

    fn run(zgc: &ZgcRealHeap, f: &mut Frame, iload_pc: usize, array_local: usize) -> Option<usize> {
        let code_len = f.code.len() - 2;
        array_load_wide_locals(zgc, f, code_len, iload_pc, array_local, true, false)
    }

    /// Locals: 0 = `arr`, 1 = 2, 2..=3 = 0, 4 = `index`, 5 = a float.
    fn locals(arr: Option<ObjectRef>, index: Value) -> [Value; 6] {
        [
            Value::Object(arr),
            Value::Int(2),
            Value::Int(0),
            Value::Int(0),
            index,
            Value::Float(0.5),
        ]
    }

    #[test]
    fn the_wide_index_and_the_wide_array_forms_read_the_element() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(ints), Some(longs)) = (
            zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 3),
            zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 3),
        ) else {
            return;
        };
        assert!(array_store_prim(
            &zgc,
            ints,
            1,
            0x4f,
            CompactValue::int(-7),
            0
        ));
        assert!(array_store_prim(
            &zgc,
            ints,
            2,
            0x4f,
            CompactValue::int(9),
            0
        ));
        assert!(array_store_prim(
            &zgc,
            longs,
            1,
            0x50,
            CompactValue::long(i64::MIN),
            ValueStack::KIND_MARK_LONG
        ));
        // `aload_0; iload 4; iaload` (the for-each read).
        let mut f = frame(
            vec![0x2a, 0x15, 0x04, 0x2e, 0xb1],
            &locals(Some(ints), Value::Int(1)),
        );
        assert_eq!(run(&zgc, &mut f, 1, 0), Some(4));
        assert_eq!(f.stack.len(), 1);
        assert_eq!(f.stack.pop_compact().as_int(), Some(-7));
        // `aload_0; iload 4; laload`.
        let mut f = frame(
            vec![0x2a, 0x15, 0x04, 0x2f, 0xb1],
            &locals(Some(longs), Value::Int(1)),
        );
        assert_eq!(run(&zgc, &mut f, 1, 0), Some(4));
        assert_eq!(f.stack.pop_compact().as_long_unchecked(), i64::MIN);
        // `aload 0; iload_1; iaload` (the wide-array arm, short index:
        // local 1 holds 2).
        let mut f = frame(
            vec![0x19, 0x00, 0x1b, 0x2e, 0xb1],
            &locals(Some(ints), Value::Int(1)),
        );
        assert_eq!(run(&zgc, &mut f, 2, 0), Some(4));
        assert_eq!(f.stack.pop_compact().as_int(), Some(9));
        // `aload 0; iload 4; iaload`.
        let mut f = frame(
            vec![0x19, 0x00, 0x15, 0x04, 0x2e, 0xb1],
            &locals(Some(ints), Value::Int(1)),
        );
        assert_eq!(run(&zgc, &mut f, 2, 0), Some(5));
        assert_eq!(f.stack.pop_compact().as_int(), Some(-7));
    }

    #[test]
    fn every_decline_pushes_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(ints) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 3) else {
            return;
        };
        let read = vec![0x2a, 0x15, 0x04, 0x2e, 0xb1];
        let cases: [(Vec<u8>, Option<ObjectRef>, Value); 8] = [
            // Out of range either side (the unfused `iaload` throws).
            (read.clone(), Some(ints), Value::Int(3)),
            (read.clone(), Some(ints), Value::Int(-1)),
            // A null array (the unfused `iaload` throws the NPE).
            (read.clone(), None, Value::Int(0)),
            // An element type the opcode does not name.
            (
                vec![0x2a, 0x15, 0x04, 0x33, 0xb1],
                Some(ints),
                Value::Int(0),
            ),
            // An index local that is not an int.
            (read.clone(), Some(ints), Value::Float(1.0)),
            // A local past `max_locals`.
            (
                vec![0x2a, 0x15, 0x09, 0x2e, 0xb1],
                Some(ints),
                Value::Int(0),
            ),
            // Not an `*aload` after the index.
            (
                vec![0x2a, 0x15, 0x04, 0x36, 0x05, 0xb1],
                Some(ints),
                Value::Int(0),
            ),
            // Not an `iload` at all.
            (
                vec![0x2a, 0x16, 0x04, 0x2e, 0xb1],
                Some(ints),
                Value::Int(0),
            ),
        ];
        for (code, arr, index) in cases {
            let mut f = frame(code.clone(), &locals(arr, index));
            assert_eq!(run(&zgc, &mut f, 1, 0), None, "{code:x?}");
            assert_eq!(f.stack.len(), 0, "{code:x?}");
        }
        // A group cut off by the end of the code.
        let mut f = frame(read, &locals(Some(ints), Value::Int(0)));
        assert_eq!(
            array_load_wide_locals(&zgc, &mut f, 3, 1, 0, true, false),
            None
        );
        assert_eq!(f.stack.len(), 0);
        // An array local past `max_locals`.
        let mut f = frame(
            vec![0x19, 0x07, 0x15, 0x04, 0x2e, 0xb1],
            &locals(Some(ints), Value::Int(0)),
        );
        assert_eq!(run(&zgc, &mut f, 2, 7), None);
        assert_eq!(f.stack.len(), 0);
    }
}

#[cfg(test)]
mod i76_l7_array_increment_operand_tests {
    //! Interpreter round i3 wave 76, lane L7: the decline memo tested inside
    //! the three array increment entries (`memo_says_declined`), the wide-local
    //! element read's one entry (`array_load_wide_locals_at`), and the two
    //! deferred operands of the fused array increment: another array element
    //! (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND`) and a quickened
    //! static (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND`). Each
    //! test sets every switch it depends on explicitly.
    use super::*;
    use crate::classloading::resolution::CachedBytecodeMethod;
    use std::sync::Arc;

    const CLASS: u32 = 0x00ab_0761;

    fn vm(memo: bool, array_operand: bool, static_operand: bool) -> Arc<SharedVm> {
        let mut config = crate::config::VmConfig::default();
        config.interp_fused_array_increment = true;
        config.interp_fused_array_increment_value = true;
        config.interp_fused_array_increment_top = true;
        config.interp_fused_array_increment_dup2 = true;
        config.interp_fused_array_accumulate = false;
        config.interp_fused_array_increment_shift_div = true;
        config.interp_fused_array_update_no_dup = true;
        config.interp_fused_array_decline_memo = memo;
        config.interp_fused_array_increment_array_operand = array_operand;
        config.interp_fused_array_increment_static_operand = static_operand;
        config.interp_fused_wide_locals = true;
        config.site_census = false;
        Arc::new(SharedVm::new(config))
    }

    /// An owned static frame over `code` (class `CLASS`), `max_locals` 4,
    /// `args` in its first locals.
    fn frame(code: &[u8], args: &[Value]) -> Frame {
        Frame::new(
            ClassId::new(CLASS),
            "cratonvm/test/i76l7/ArrayOperand".to_string(),
            "m".to_string(),
            "([II[II)V".to_string(),
            None,
            code.to_vec(),
            vec![],
            6,
            4,
            args,
        )
    }

    /// The same frame pushed from a fresh `CachedBytecodeMethod` (so it keeps
    /// a decline memo).
    fn cached_frame(code: &[u8], args: &[Value]) -> Frame {
        let cm = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(CLASS),
                class_name: Arc::from("cratonvm/test/i76l7/ArrayOperandCached"),
                method_name: Arc::from("m"),
                method_descriptor: Arc::from("([II[II)V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(code),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 6,
                max_locals: 4,
                num_params: 4,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let (mut locals_pool, mut stacks_pool) = (Vec::new(), Vec::new());
        Frame::new_pooled_cached(cm, args, &mut locals_pool, &mut stacks_pool)
    }

    fn array(zgc: &ZgcRealHeap, et: ArrayElementType, len: usize) -> Option<ObjectRef> {
        zgc.try_alloc_array(ClassId::new(0), et, len)
    }

    /// Element `index` of `arr` as `load` pushes it, widened to `i64`.
    fn get(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, load: u8) -> Option<i64> {
        let mut s = ValueStack::new(4);
        if !array_load_prim(zgc, &mut s, arr, index, load) {
            return None;
        }
        if load == 0x2f {
            Some(s.pop_compact().as_long_unchecked())
        } else {
            s.pop_compact().as_int().map(i64::from)
        }
    }

    /// Store `v` at `index` of `arr` with the `*astore` matching `load`.
    fn set(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, load: u8, v: i64) {
        let (cv, kind) = if load == 0x2f {
            (CompactValue::long(v), ValueStack::KIND_MARK_LONG)
        } else {
            // Cast: test values fit the element width.
            (CompactValue::int(v as i32), 0)
        };
        assert!(array_store_prim(zgc, arr, index, load + 0x21, cv, kind));
    }

    /// `a` in local 0, `j` in 1, `b` in 2, `k` in 3.
    fn args(a: Option<ObjectRef>, j: i32, b: Option<ObjectRef>, k: i32) -> [Value; 4] {
        [
            Value::Object(a),
            Value::Int(j),
            Value::Object(b),
            Value::Int(k),
        ]
    }

    fn local(shared: &SharedVm, zgc: &ZgcRealHeap, f: &mut Frame, pc: usize) -> Option<usize> {
        let code_len = f.code.len() - 2;
        fused_array_increment_local(shared, zgc, f, code_len, pc, &mut no_sites())
    }

    /// `a[j] += b[k]`: `aload_0; iload_1; dup2; iaload; aload_2; iload_3;
    /// iaload; iadd; iastore; return`.
    const ADD_ELEMENT: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x2e, 0x60, 0x4f, 0xb1];
    /// `a[j] += SF`: `aload_0; iload_1; dup2; iaload; getstatic #1; iadd;
    /// iastore; return`.
    const ADD_STATIC: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0xb2, 0x00, 0x01, 0x60, 0x4f, 0xb1];
    /// `a[j]++`.
    const INCREMENT: [u8; 8] = [0x2a, 0x1b, 0x5c, 0x2e, 0x04, 0x60, 0x4f, 0xb1];

    #[test]
    fn the_deferred_operands_decode_only_under_their_bits() {
        let f = frame(&ADD_ELEMENT, &args(None, 0, None, 0));
        let code_len = f.code.len() - 2;
        let base = array_increment_mode(false, true);
        assert_eq!(
            decode_array_increment_group(&f, &f.code, code_len, 2, base),
            None
        );
        assert_eq!(
            decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_ARRAY_OPERAND),
            Some((
                0x2e,
                ArrayIncOperand::Element {
                    array_local: 2,
                    index_local: 3,
                    load: 0x2e
                },
                0x60,
                9,
                FieldIncrementUse::Statement
            ))
        );
        // The static bit does not open the element form, nor the reverse.
        assert_eq!(
            decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_STATIC_OPERAND),
            None
        );
        let f = frame(&ADD_STATIC, &args(None, 0, None, 0));
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_ARRAY_OPERAND),
            None
        );
        assert_eq!(
            decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_STATIC_OPERAND),
            Some((
                0x2e,
                ArrayIncOperand::Static { cp_index: 1 },
                0x60,
                9,
                FieldIncrementUse::Statement
            ))
        );
        // The tests' view never answers a deferred operand.
        assert_eq!(
            decode_array_increment_mode(&f, &f.code, code_len, 2, base | ARRAY_INC_STATIC_OPERAND),
            None
        );
        // An operand load of another width (`laload` under an int group), a
        // wide operand local (`aload 2`), an `iinc`-free index expression:
        // refused on the bytes.
        let all = base | ARRAY_INC_ARRAY_OPERAND | ARRAY_INC_STATIC_OPERAND;
        for code in [
            vec![0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x2f, 0x60, 0x4f, 0xb1],
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x19, 0x02, 0x1d, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x04, 0x64, 0x2e, 0x60, 0x4f, 0xb1,
            ],
        ] {
            let f = frame(&code, &args(None, 0, None, 0));
            let code_len = f.code.len() - 2;
            assert_eq!(
                decode_array_increment_group(&f, &f.code, code_len, 2, all),
                None,
                "{code:x?}"
            );
        }
        // A wave-75 operand decodes exactly as before under the new bits.
        let f = frame(&INCREMENT, &args(None, 0, None, 0));
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_array_increment_group(&f, &f.code, code_len, 2, all),
            Some((
                0x2e,
                ArrayIncOperand::Ready(super::super::IncrementOperand::Int(1)),
                0x60,
                7,
                FieldIncrementUse::Statement
            ))
        );
    }

    #[test]
    fn an_element_operand_is_added_only_under_the_switch() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(a), Some(b)) = (
            array(&zgc, ArrayElementType::Int, 4),
            array(&zgc, ArrayElementType::Int, 4),
        ) else {
            return;
        };
        set(&zgc, a, 1, 0x2e, 5);
        set(&zgc, b, 3, 0x2e, 7);
        for on in [false, true] {
            let shared = vm(false, on, false);
            let mut f = frame(&ADD_ELEMENT, &args(Some(a), 1, Some(b), 3));
            let before = get(&zgc, a, 1, 0x2e);
            let got = local(&shared, &zgc, &mut f, 0);
            assert_eq!(f.stack.len(), 0);
            if on {
                assert_eq!(got, Some(9));
                assert_eq!(get(&zgc, a, 1, 0x2e), before.map(|v| v + 7));
            } else {
                assert_eq!(got, None);
                assert_eq!(get(&zgc, a, 1, 0x2e), before);
            }
        }
        // The stack-top and `dup2` entries take it too.
        let shared = vm(false, true, false);
        let mut f = frame(&ADD_ELEMENT[1..], &args(Some(a), 1, Some(b), 3));
        f.stack.push(Value::Object(Some(a))).unwrap();
        let code_len = f.code.len() - 2;
        let top = fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
        assert_eq!(top, Some(8));
        assert_eq!(f.stack.len(), 0, "the array slot is consumed");
        assert_eq!(get(&zgc, a, 1, 0x2e), Some(19));
        let mut f = frame(&ADD_ELEMENT[2..], &args(Some(a), 1, Some(b), 3));
        f.stack.push(Value::Object(Some(a))).unwrap();
        f.stack.push_int(1).unwrap();
        let code_len = f.code.len() - 2;
        let dup2 = fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
        assert_eq!(dup2, Some(7));
        assert_eq!(f.stack.len(), 0);
        assert_eq!(get(&zgc, a, 1, 0x2e), Some(26));
    }

    #[test]
    fn element_operands_alias_widen_and_decline_as_the_unfused_group() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(a), Some(bytes), Some(la), Some(lb)) = (
            array(&zgc, ArrayElementType::Int, 4),
            array(&zgc, ArrayElementType::Byte, 4),
            array(&zgc, ArrayElementType::Long, 4),
            array(&zgc, ArrayElementType::Long, 4),
        ) else {
            return;
        };
        let shared = vm(false, true, false);
        // `a[j] += a[j]`: both reads precede the store.
        set(&zgc, a, 2, 0x2e, 5);
        let alias = [0x2a, 0x1b, 0x5c, 0x2e, 0x2a, 0x1b, 0x2e, 0x60, 0x4f, 0xb1];
        let mut f = frame(&alias, &args(Some(a), 2, None, 0));
        assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
        assert_eq!(get(&zgc, a, 2, 0x2e), Some(10));
        // A `byte[]` operand into an `int[]` element: sign-extended.
        set(&zgc, bytes, 0, 0x33, -3);
        let widen = [0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x33, 0x60, 0x4f, 0xb1];
        let mut f = frame(&widen, &args(Some(a), 2, Some(bytes), 0));
        assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
        assert_eq!(get(&zgc, a, 2, 0x2e), Some(7));
        // `la[j] += lb[k]` over `long[]`s, and `la[j] -= lb[k]`.
        set(&zgc, la, 1, 0x2f, i64::MAX);
        set(&zgc, lb, 3, 0x2f, 2);
        let ladd = [0x2a, 0x1b, 0x5c, 0x2f, 0x2c, 0x1d, 0x2f, 0x61, 0x50, 0xb1];
        let mut f = frame(&ladd, &args(Some(la), 1, Some(lb), 3));
        assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
        assert_eq!(
            get(&zgc, la, 1, 0x2f),
            Some(i64::MIN + 1),
            "wrapping, as ladd"
        );
        // `a[j] = a[j] + b[j]` (the no-dup form, `twoLoadsMix`).
        set(&zgc, bytes, 2, 0x33, 4);
        let no_dup = [
            0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x2c, 0x1b, 0x33, 0x60, 0x4f, 0xb1,
        ];
        let mut f = frame(&no_dup, &args(Some(a), 2, Some(bytes), 0));
        assert_eq!(local(&shared, &zgc, &mut f, 0), Some(10));
        assert_eq!(get(&zgc, a, 2, 0x2e), Some(11));
        // Every operand refusal declines having written nothing: out of
        // range either side, a null operand array, another element type.
        for (b, k) in [
            (Some(bytes), 4),
            (Some(bytes), -1),
            (None, 0),
            (Some(la), 0),
        ] {
            let mut f = frame(&widen, &args(Some(a), 2, b, k));
            assert_eq!(local(&shared, &zgc, &mut f, 0), None, "{k}");
            assert_eq!(f.stack.len(), 0);
            assert_eq!(get(&zgc, a, 2, 0x2e), Some(11));
        }
    }

    #[test]
    fn an_operand_decline_is_not_remembered() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(a), Some(b)) = (
            array(&zgc, ArrayElementType::Int, 4),
            array(&zgc, ArrayElementType::Int, 2),
        ) else {
            return;
        };
        let shared = vm(true, true, true);
        // The operand index out of range: per execution.
        let mut f = cached_frame(&ADD_ELEMENT, &args(Some(a), 1, Some(b), 3));
        assert_eq!(local(&shared, &zgc, &mut f, 0), None);
        assert!(!array_increment_declined(&f, 0));
        // A cold static site: the unfused `getstatic` fills it.
        let mut f = cached_frame(&ADD_STATIC, &args(Some(a), 1, None, 0));
        assert_eq!(local(&shared, &zgc, &mut f, 0), None);
        assert!(!array_increment_declined(&f, 0));
        // With both switches off the same bytes are a shape decline, and
        // remembered (wave 75).
        let shared = vm(true, false, false);
        let mut f = cached_frame(&ADD_STATIC, &args(Some(a), 1, None, 0));
        assert_eq!(local(&shared, &zgc, &mut f, 0), None);
        assert!(array_increment_declined(&f, 0));
    }

    fn static_field(declaring: ClassId, desc_byte: u8, is_volatile: bool) -> ResolvedField {
        ResolvedField {
            declaring_class_id: declaring,
            field_index: 0,
            is_static: true,
            is_volatile,
            is_final: false,
            is_reference: false,
            desc_byte,
        }
    }

    /// Fill `#1`'s static site with `field`, then run the local entry over
    /// `code` until a fill survives the epochs other tests move (as the
    /// wave-5 `getstatic` site tests do); the entry's answer when it served,
    /// `None` when no fill stuck or it declined.
    fn run_with_site(
        shared: &SharedVm,
        zgc: &ZgcRealHeap,
        code: &[u8],
        a: ObjectRef,
        field: &ResolvedField,
    ) -> Option<usize> {
        for _ in 0..1000 {
            let mut sites = FieldSiteCache::new();
            sites.put(
                ClassId::new(CLASS),
                1,
                FieldSiteCache::epochs_now(),
                field.clone(),
            );
            if sites.get(ClassId::new(CLASS), 1).is_none() {
                continue;
            }
            let mut f = frame(code, &args(Some(a), 1, None, 0));
            let code_len = f.code.len() - 2;
            return fused_array_increment_local(shared, zgc, &mut f, code_len, 0, &mut sites);
        }
        None
    }

    #[test]
    fn a_static_operand_is_read_from_a_filled_site_of_the_groups_width() {
        if cratonvm_classloading::any_class_redefined() {
            return; // the site tables are latched off for this process
        }
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(a), Some(bytes), Some(la)) = (
            array(&zgc, ArrayElementType::Int, 4),
            array(&zgc, ArrayElementType::Byte, 4),
            array(&zgc, ArrayElementType::Long, 4),
        ) else {
            return;
        };
        let shared = vm(false, false, true);
        let decl = ClassId::new(90_761);
        crate::vm::set_static_shared(&shared, decl, 0, Value::Int(6));
        // `a[j] += SF` (an `I` static, volatile or not).
        for volatile in [false, true] {
            let before = get(&zgc, a, 1, 0x2e);
            let got = run_with_site(
                &shared,
                &zgc,
                &ADD_STATIC,
                a,
                &static_field(decl, b'I', volatile),
            );
            if got.is_some() {
                assert_eq!(got, Some(9));
                assert_eq!(get(&zgc, a, 1, 0x2e), before.map(|v| v + 6));
            }
        }
        // A `B` static into a `byte[]` element, narrowed by the group's `i2b`.
        set(&zgc, bytes, 1, 0x33, 125);
        let byte_group = [
            0x2a, 0x1b, 0x5c, 0x33, 0xb2, 0x00, 0x01, 0x60, 0x91, 0x54, 0xb1,
        ];
        let byte_field = static_field(decl, b'B', false);
        if run_with_site(&shared, &zgc, &byte_group, bytes, &byte_field).is_some() {
            assert_eq!(get(&zgc, bytes, 1, 0x33), Some(-125), "125 + 6 as i2b");
        }
        // A `J` static into a `long[]` element (an `Int` slot widens).
        let long_group = [0x2a, 0x1b, 0x5c, 0x2f, 0xb2, 0x00, 0x01, 0x61, 0x50, 0xb1];
        let long_field = static_field(decl, b'J', false);
        if run_with_site(&shared, &zgc, &long_group, la, &long_field).is_some() {
            assert_eq!(get(&zgc, la, 1, 0x2f), Some(6));
        }
        // Another width declines, nothing written: a `J` static under an int
        // group, an `I` static under a long one, a reference.
        let before = get(&zgc, a, 1, 0x2e);
        assert_eq!(
            run_with_site(
                &shared,
                &zgc,
                &ADD_STATIC,
                a,
                &static_field(decl, b'J', false)
            ),
            None
        );
        assert_eq!(get(&zgc, a, 1, 0x2e), before);
        assert_eq!(
            run_with_site(
                &shared,
                &zgc,
                &long_group,
                la,
                &static_field(decl, b'I', false)
            ),
            None
        );
        let mut reference = static_field(decl, b'L', false);
        reference.is_reference = true;
        assert_eq!(
            run_with_site(&shared, &zgc, &ADD_STATIC, a, &reference),
            None
        );
        assert_eq!(get(&zgc, a, 1, 0x2e), before);
        // Without the switch a filled site is not consulted.
        let off = vm(false, false, false);
        assert_eq!(
            run_with_site(&off, &zgc, &ADD_STATIC, a, &static_field(decl, b'I', false)),
            None
        );
        assert_eq!(get(&zgc, a, 1, 0x2e), before);
    }

    #[test]
    fn a_remembered_pc_is_declined_inside_each_entry() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(a) = array(&zgc, ArrayElementType::Int, 4) else {
            return;
        };
        for memo in [false, true] {
            let shared = vm(memo, false, false);
            let mut f = cached_frame(&INCREMENT, &args(Some(a), 1, None, 0));
            // A bit set by an earlier decline (here by hand).
            note_array_increment_decline(&f, 0);
            assert!(array_increment_declined(&f, 0));
            let before = get(&zgc, a, 1, 0x2e);
            let got = local(&shared, &zgc, &mut f, 0);
            if memo {
                assert_eq!(got, None, "the entry tests the memo first");
                assert_eq!(get(&zgc, a, 1, 0x2e), before);
            } else {
                assert_eq!(got, Some(7), "without the switch the bit is not read");
                assert_eq!(get(&zgc, a, 1, 0x2e), before.map(|v| v + 1));
            }
            // The stack-top entry at its `iload_1`.
            let mut f = cached_frame(&INCREMENT[1..], &args(Some(a), 1, None, 0));
            f.stack.push(Value::Object(Some(a))).unwrap();
            note_array_increment_decline(&f, 0);
            let code_len = f.code.len() - 2;
            let got =
                fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
            assert_eq!(got.is_some(), !memo);
            // The `dup2` entry at its `dup2`.
            let mut f = cached_frame(&INCREMENT[2..], &args(Some(a), 1, None, 0));
            f.stack.push(Value::Object(Some(a))).unwrap();
            f.stack.push_int(1).unwrap();
            note_array_increment_decline(&f, 0);
            let code_len = f.code.len() - 2;
            let got =
                fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
            assert_eq!(got.is_some(), !memo);
            assert_eq!(f.stack.len(), if memo { 2 } else { 0 });
        }
    }

    #[test]
    fn the_wide_local_entry_tests_the_heap_and_reads_as_the_body() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(ints) = array(&zgc, ArrayElementType::Int, 3) else {
            return;
        };
        set(&zgc, ints, 1, 0x2e, -7);
        let shared = vm(false, false, false);
        // `aload_0; iload_3; iaload` through the wide entry's short-index arm.
        let code = [0x2a, 0x1d, 0x2e, 0xb1];
        let mut f = frame(&code, &args(Some(ints), 0, None, 1));
        let code_len = f.code.len() - 2;
        assert_eq!(
            array_load_wide_locals_at(&shared, None, &mut f, code_len, 1, 0),
            None
        );
        assert_eq!(f.stack.len(), 0);
        assert_eq!(
            array_load_wide_locals_at(&shared, Some(&zgc), &mut f, code_len, 1, 0),
            Some(3)
        );
        assert_eq!(f.stack.pop_compact().as_int(), Some(-7));
    }
}

#[cfg(test)]
mod i77_l7_array_operand_budget_and_wide_tests {
    //! Interpreter round i3 wave 77, lane L7: the deferred operands' miss
    //! budget (`CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET`) and the
    //! element operand's wide-local and index-expression forms
    //! (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND_WIDE`). Each test
    //! sets every switch it depends on explicitly.
    use super::*;
    use crate::classloading::resolution::CachedBytecodeMethod;
    use std::sync::Arc;

    const CLASS: u32 = 0x00ab_0771;

    fn vm(memo: bool, budget: bool, wide: bool) -> Arc<SharedVm> {
        let mut config = crate::config::VmConfig::default();
        config.interp_fused_array_increment = true;
        config.interp_fused_array_increment_value = true;
        config.interp_fused_array_increment_top = true;
        config.interp_fused_array_increment_dup2 = true;
        config.interp_fused_array_accumulate = false;
        config.interp_fused_array_increment_shift_div = true;
        config.interp_fused_array_update_no_dup = true;
        config.interp_fused_array_decline_memo = memo;
        config.interp_fused_array_increment_array_operand = true;
        config.interp_fused_array_increment_static_operand = true;
        config.interp_fused_array_operand_miss_budget = budget;
        config.interp_fused_array_increment_array_operand_wide = wide;
        config.interp_fused_wide_locals = true;
        config.site_census = false;
        Arc::new(SharedVm::new(config))
    }

    /// An owned static frame over `code`, `max_locals` 8, `args` in its
    /// first locals.
    fn frame(code: &[u8], args: &[Value]) -> Frame {
        Frame::new(
            ClassId::new(CLASS),
            "cratonvm/test/i77l7/ArrayOperand".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            code.to_vec(),
            vec![],
            6,
            8,
            args,
        )
    }

    /// The same frame pushed from a fresh `CachedBytecodeMethod` (so it keeps
    /// a decline memo and a miss counter).
    fn cached_frame(code: &[u8], args: &[Value]) -> Frame {
        let cm = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(CLASS),
                class_name: Arc::from("cratonvm/test/i77l7/ArrayOperandCached"),
                method_name: Arc::from("m"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(code),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 6,
                max_locals: 8,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let (mut locals_pool, mut stacks_pool) = (Vec::new(), Vec::new());
        Frame::new_pooled_cached(cm, args, &mut locals_pool, &mut stacks_pool)
    }

    fn ints(zgc: &ZgcRealHeap, len: usize) -> Option<ObjectRef> {
        zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, len)
    }

    fn get(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32) -> Option<i32> {
        let mut s = ValueStack::new(4);
        if !array_load_prim(zgc, &mut s, arr, index, 0x2e) {
            return None;
        }
        s.pop_compact().as_int()
    }

    fn set(zgc: &ZgcRealHeap, arr: ObjectRef, index: i32, v: i32) {
        assert!(array_store_prim(
            zgc,
            arr,
            index,
            0x4f,
            CompactValue::int(v),
            0
        ));
    }

    fn local(shared: &SharedVm, zgc: &ZgcRealHeap, f: &mut Frame, pc: usize) -> Option<usize> {
        let code_len = f.code.len() - 2;
        fused_array_increment_local(shared, zgc, f, code_len, pc, &mut FieldSiteCache::new())
    }

    fn misses_allocated(f: &Frame) -> bool {
        f.cached_method()
            .and_then(|cm| cm.array_inc_declined.get())
            .is_some_and(|memo| memo.operand_misses.get().is_some())
    }

    /// `a[j] += SF`: `aload_0; iload_1; dup2; iaload; getstatic #1; iadd;
    /// iastore; return`.
    const ADD_STATIC: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0xb2, 0x00, 0x01, 0x60, 0x4f, 0xb1];
    /// `a[j] += b[k]`: `aload_0; iload_1; dup2; iaload; aload_2; iload_3;
    /// iaload; iadd; iastore; return`.
    const ADD_ELEMENT: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x2e, 0x60, 0x4f, 0xb1];

    #[test]
    fn a_cold_static_operand_retires_its_pc_only_under_the_budget() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(a) = ints(&zgc, 4) else {
            return;
        };
        for budget in [false, true] {
            let shared = vm(true, budget, false);
            // A site that never fills (an empty table every time, as a
            // `<clinit>` access leaves it).
            let mut f = cached_frame(&ADD_STATIC, &[Value::Object(Some(a)), Value::Int(1)]);
            for _ in 1..ARRAY_OPERAND_MISS_BUDGET {
                assert_eq!(local(&shared, &zgc, &mut f, 0), None);
            }
            assert!(!array_increment_declined(&f, 0), "below the budget");
            assert_eq!(misses_allocated(&f), budget);
            assert_eq!(local(&shared, &zgc, &mut f, 0), None);
            assert_eq!(
                array_increment_declined(&f, 0),
                budget,
                "the budget-th miss"
            );
            assert_eq!(f.stack.len(), 0);
        }
    }

    #[test]
    fn a_missing_element_operand_counts_too_and_saturates() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(a), Some(b)) = (ints(&zgc, 4), ints(&zgc, 2)) else {
            return;
        };
        let shared = vm(true, true, false);
        // `b[3]` is out of range: a per-execution decline, counted.
        let args = [
            Value::Object(Some(a)),
            Value::Int(1),
            Value::Object(Some(b)),
            Value::Int(3),
        ];
        let mut f = cached_frame(&ADD_ELEMENT, &args);
        for _ in 0..ARRAY_OPERAND_MISS_BUDGET {
            assert_eq!(local(&shared, &zgc, &mut f, 0), None);
        }
        assert!(array_increment_declined(&f, 0));
        // Further misses (here by hand) neither wrap the counter nor clear
        // the bit.
        for _ in 0..300 {
            assert!(note_array_increment_operand_miss(&f, 0));
        }
        let counter = f
            .cached_method()
            .and_then(|cm| cm.array_inc_declined.get())
            .and_then(|memo| memo.operand_misses.get())
            .map(|m| m[0].load(Ordering::Relaxed));
        assert_eq!(counter, Some(ARRAY_OPERAND_MISS_BUDGET));
        assert!(array_increment_declined(&f, 0));
    }

    #[test]
    fn the_budget_needs_the_memo_and_a_hit_never_counts() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let (Some(a), Some(b)) = (ints(&zgc, 4), ints(&zgc, 4)) else {
            return;
        };
        // Memo off: nothing is counted or retired.
        let shared = vm(false, true, false);
        let mut f = cached_frame(&ADD_STATIC, &[Value::Object(Some(a)), Value::Int(1)]);
        for _ in 0..u32::from(ARRAY_OPERAND_MISS_BUDGET) + 4 {
            assert_eq!(local(&shared, &zgc, &mut f, 0), None);
        }
        assert!(!array_increment_declined(&f, 0));
        assert!(!misses_allocated(&f));
        // Hits leave no counter behind.
        let shared = vm(true, true, false);
        let args = [
            Value::Object(Some(a)),
            Value::Int(1),
            Value::Object(Some(b)),
            Value::Int(3),
        ];
        let mut f = cached_frame(&ADD_ELEMENT, &args);
        set(&zgc, b, 3, 2);
        for _ in 0..u32::from(ARRAY_OPERAND_MISS_BUDGET) + 4 {
            assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
        }
        assert!(!misses_allocated(&f));
        assert!(!array_increment_declined(&f, 0));
    }

    #[test]
    fn wide_locals_and_index_expressions_decode_only_under_their_bit() {
        let f = frame(&[0xb1], &[]);
        let base = array_increment_mode(false, true) | ARRAY_INC_ARRAY_OPERAND;
        let wide = base | ARRAY_INC_ARRAY_OPERAND_WIDE;
        // (code, the operand it decodes to under `wide`, the pc past the
        // group)
        let cases: [(Vec<u8>, ArrayIncOperand, usize); 5] = [
            // acc[i] += x[i] with acc, i, x in 4, 5, 6: `aload 4; iload 5;
            // dup2; iaload; aload 6; iload 5; iaload; iadd; iastore`.
            (
                vec![
                    0x19, 4, 0x15, 5, 0x5c, 0x2e, 0x19, 6, 0x15, 5, 0x2e, 0x60, 0x4f, 0xb1,
                ],
                ArrayIncOperand::Element {
                    array_local: 6,
                    index_local: 5,
                    load: 0x2e,
                },
                13,
            ),
            // p[i] += p[i - 1]: `aload_0; iload_1; dup2; iaload; aload_0;
            // iload_1; iconst_1; isub; iaload; iadd; iastore`.
            (
                vec![
                    0x2a, 0x1b, 0x5c, 0x2e, 0x2a, 0x1b, 0x04, 0x64, 0x2e, 0x60, 0x4f, 0xb1,
                ],
                ArrayIncOperand::ElementAt {
                    array_local: 0,
                    index_local: 1,
                    delta: -1,
                    load: 0x2e,
                },
                11,
            ),
            // h[k] += w[k + 200] (`sipush`), w in local 7.
            (
                vec![
                    0x2a, 0x1b, 0x5c, 0x2e, 0x19, 7, 0x1b, 0x11, 0x00, 0xc8, 0x60, 0x2e, 0x60,
                    0x4f, 0xb1,
                ],
                ArrayIncOperand::ElementAt {
                    array_local: 7,
                    index_local: 1,
                    delta: 200,
                    load: 0x2e,
                },
                14,
            ),
            // a[j] += b[k + 3] with k wide (`bipush`): `aload_2; iload 5;
            // bipush 3; iadd; iaload`.
            (
                vec![
                    0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 5, 0x10, 3, 0x60, 0x2e, 0x60, 0x4f, 0xb1,
                ],
                ArrayIncOperand::ElementAt {
                    array_local: 2,
                    index_local: 5,
                    delta: 3,
                    load: 0x2e,
                },
                13,
            ),
            // A `byte[]` operand with a short array and a wide index.
            (
                vec![
                    0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 6, 0x33, 0x60, 0x4f, 0xb1,
                ],
                ArrayIncOperand::Element {
                    array_local: 2,
                    index_local: 6,
                    load: 0x33,
                },
                10,
            ),
        ];
        for (code, operand, next) in cases {
            let p = if code[0] == 0x19 { 4 } else { 2 };
            let code_len = code.len();
            assert_eq!(
                decode_array_increment_group(&f, &code, code_len, p, base),
                None,
                "{code:x?} without the bit"
            );
            assert_eq!(
                decode_array_increment_group(&f, &code, code_len, p, wide),
                Some((0x2e, operand, 0x60, next, FieldIncrementUse::Statement)),
                "{code:x?}"
            );
        }
        // Still refused under the bit: a local at or past `max_locals` (8),
        // an index expression of another arithmetic (`imul`) or with a local
        // addend, an operand load of another width.
        for code in [
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x19, 8, 0x1b, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 9, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1b, 0x05, 0x68, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1b, 0x1c, 0x60, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 5, 0x2f, 0x60, 0x4f, 0xb1,
            ],
        ] {
            let code_len = code.len();
            assert_eq!(
                decode_array_increment_group(&f, &code, code_len, 2, wide),
                None,
                "{code:x?}"
            );
        }
        // The wide bit alone (without the element operand's) opens nothing.
        let code = vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x19, 6, 0x15, 5, 0x2e, 0x60, 0x4f, 0xb1,
        ];
        let only_wide = array_increment_mode(false, true) | ARRAY_INC_ARRAY_OPERAND_WIDE;
        assert_eq!(
            decode_array_increment_group(&f, &code, code.len(), 2, only_wide),
            None
        );
    }

    #[test]
    fn an_index_expression_operand_reads_and_declines_as_the_unfused_group() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let Some(p) = ints(&zgc, 4) else {
            return;
        };
        for (i, v) in [(0, 1), (1, 2), (2, 3), (3, 4)] {
            set(&zgc, p, i, v);
        }
        // p[i] += p[i - 1]
        let prefix = [
            0x2a, 0x1b, 0x5c, 0x2e, 0x2a, 0x1b, 0x04, 0x64, 0x2e, 0x60, 0x4f, 0xb1,
        ];
        let off = vm(false, false, false);
        let mut f = frame(&prefix, &[Value::Object(Some(p)), Value::Int(2)]);
        assert_eq!(local(&off, &zgc, &mut f, 0), None, "switch off");
        assert_eq!(get(&zgc, p, 2), Some(3));
        let on = vm(false, false, true);
        let mut f = frame(&prefix, &[Value::Object(Some(p)), Value::Int(2)]);
        assert_eq!(local(&on, &zgc, &mut f, 0), Some(11));
        assert_eq!(get(&zgc, p, 2), Some(5), "3 + p[1]");
        assert_eq!(f.stack.len(), 0);
        // i = 0: the unfused `p[-1]` throws; the fused group declines having
        // written nothing.
        let mut f = frame(&prefix, &[Value::Object(Some(p)), Value::Int(0)]);
        assert_eq!(local(&on, &zgc, &mut f, 0), None);
        assert_eq!(get(&zgc, p, 0), Some(1));
        // A wide operand array: `aload_0; iload_1; dup2; iaload; aload 5;
        // iload 6; iaload; iadd; iastore`.
        let Some(w) = ints(&zgc, 8) else {
            return;
        };
        set(&zgc, w, 7, 40);
        let wide = [
            0x2a, 0x1b, 0x5c, 0x2e, 0x19, 5, 0x15, 6, 0x2e, 0x60, 0x4f, 0xb1,
        ];
        let args = [
            Value::Object(Some(p)),
            Value::Int(3),
            Value::Int(0),
            Value::Int(0),
            Value::Int(0),
            Value::Object(Some(w)),
            Value::Int(7),
        ];
        let mut f = frame(&wide, &args);
        assert_eq!(local(&on, &zgc, &mut f, 0), Some(11));
        assert_eq!(get(&zgc, p, 3), Some(44));
    }
}

#[cfg(test)]
mod i82_l7_long_array_accumulate_tests {
    //! Interpreter round i3 wave 82, lane L7: the long and widening forms of
    //! the fused array element accumulate
    //! (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG`,
    //! `fused_array_accumulate_long`). A hit stores what the unfused `lload_S;
    //! aload_N; iload_M; <x>aload; [i2l;] <op>; lstore_Z` stores, kind mark
    //! included, and a decline writes nothing. The helper does not read the
    //! switch (the loop does), so these tests need none.
    use super::*;
    use std::sync::Arc;

    /// A static frame over `code` with locals 0 = `arr`, 1 = `index`, and the
    /// long accumulator `acc` in 2..3 (`max_locals` 4).
    fn long_frame(code: Vec<u8>, arr: Value, index: Value, acc: Option<i64>) -> Frame {
        let mut f = Frame::new(
            ClassId::new(0x00ab_0882),
            "cratonvm/test/i82l7/LongAccumulate".to_string(),
            "m".to_string(),
            "([JI)V".to_string(),
            None,
            code,
            vec![],
            6,
            4,
            &[arr, index, Value::Int(0), Value::Int(0)],
        );
        if let Some(acc) = acc {
            f.set_local_unchecked(2, Value::Long(acc));
        }
        f
    }

    /// `code` followed by the two padding bytes a frame's code carries.
    fn padded(code: &[u8]) -> Vec<u8> {
        let mut v = code.to_vec();
        v.extend_from_slice(&[0, 0]);
        v
    }

    fn vm() -> Arc<SharedVm> {
        let mut config = crate::config::VmConfig::default();
        config.site_census = false;
        Arc::new(SharedVm::new(config))
    }

    #[test]
    fn the_long_decoder_takes_only_its_groups() {
        #[allow(clippy::type_complexity)]
        fn d(code: &[u8]) -> Option<(usize, usize, usize, u8, u8, usize, usize)> {
            decode_array_accumulate_long(&padded(code), code.len(), 0)
        }
        // `lload_2; aload_0; iload_1; laload; ladd; lstore_2`.
        assert_eq!(
            d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x41]),
            Some((2, 0, 1, 0x2f, 0x61, 2, 6))
        );
        // `lload_0; aload_2; iload_3; iaload; i2l; lxor; lstore_1` (a
        // different target), and `caload` widened.
        assert_eq!(
            d(&[0x1e, 0x2c, 0x1d, 0x2e, 0x85, 0x83, 0x40]),
            Some((0, 2, 3, 0x2e, 0x83, 1, 7))
        );
        assert_eq!(
            d(&[0x1e, 0x2c, 0x1d, 0x34, 0x85, 0x61, 0x3f]),
            Some((0, 2, 3, 0x34, 0x61, 0, 7))
        );
        // An int arithmetic, `laload` with an `i2l`, an int load without
        // one, `faload` / `daload` / `aaload`, a throwing `ldiv` / `lrem`, a
        // shift, `istore_N` / `lstore n` as the store, an `iload_S` head.
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x60, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x85, 0x61, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2e, 0x61, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x30, 0x61, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x31, 0x61, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x32, 0x61, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x6d, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x71, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x79, 0x41]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x3e]), None);
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x37, 0x02]), None);
        assert_eq!(d(&[0x1c, 0x2a, 0x1b, 0x2f, 0x61, 0x41]), None);
        // Cut groups: the long form one byte short, the widening form's
        // store past `code_len`.
        assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61]), None);
        let widen = padded(&[0x1e, 0x2c, 0x1d, 0x2e, 0x85, 0x61, 0x3f]);
        assert_eq!(decode_array_accumulate_long(&widen, 6, 0), None);
    }

    #[test]
    fn a_long_accumulate_hit_stores_the_unfused_result() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let shared = vm();
        // (element type, load, element as stored, element as `i2l` /
        // `laload` leaves it)
        let big: i64 = 0x1234_5678_9abc_def0;
        let kinds: [(ArrayElementType, u8, i64, i64); 6] = [
            (ArrayElementType::Long, 0x2f, big, big),
            (ArrayElementType::Int, 0x2e, -7, -7),
            (ArrayElementType::Byte, 0x33, -2, -2),
            (ArrayElementType::Boolean, 0x33, 1, 1),
            (ArrayElementType::Char, 0x34, 0xfffe, 0xfffe),
            (ArrayElementType::Short, 0x35, -3, -3),
        ];
        let acc: i64 = 1 << 40;
        for (et, load, stored, elem) in kinds {
            let Some(arr) = zgc.try_alloc_array(ClassId::new(0), et, 3) else {
                return;
            };
            let (cv, kind) = if load == 0x2f {
                (CompactValue::long(stored), ValueStack::KIND_MARK_LONG)
            } else {
                (CompactValue::int(stored as i32), 0)
            };
            assert!(
                array_store_prim(&zgc, arr, 1, load + 0x21, cv, kind),
                "{et:?}"
            );
            for (arith, want) in [
                (0x61_u8, acc.wrapping_add(elem)),
                (0x65, acc.wrapping_sub(elem)),
                (0x69, acc.wrapping_mul(elem)),
                (0x7f, acc & elem),
                (0x81, acc | elem),
                (0x83, acc ^ elem),
            ] {
                let mut code = vec![0x20, 0x2a, 0x1b, load];
                if load != 0x2f {
                    code.push(0x85);
                }
                code.extend_from_slice(&[arith, 0x41, 0xb1]);
                let group = code.len() - 1;
                let mut f = long_frame(code, Value::Object(Some(arr)), Value::Int(1), Some(acc));
                let code_len = f.code.len() - 2;
                assert_eq!(
                    fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
                    Some(group),
                    "{et:?} {arith:#x}"
                );
                assert_eq!(f.stack.len(), 0, "nothing pushed");
                assert!(f.local_is_long_marked(2), "{et:?} {arith:#x}");
                assert_eq!(
                    f.get_local_compact_unchecked(2).as_long_unchecked(),
                    want,
                    "{et:?} {arith:#x}"
                );
            }
        }
    }

    #[test]
    fn every_long_accumulate_decline_writes_nothing() {
        let zgc = ZgcRealHeap::with_capacity(1 << 20);
        let shared = vm();
        let Some(longs) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 2) else {
            return;
        };
        let code = || vec![0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x41, 0xb1];
        let cases: [(Value, Value); 4] = [
            // Out of range either side (the unfused `laload` throws).
            (Value::Object(Some(longs)), Value::Int(2)),
            (Value::Object(Some(longs)), Value::Int(-1)),
            // A null array (the unfused `laload` throws the NPE).
            (Value::Object(None), Value::Int(0)),
            // An index local that is not an int.
            (Value::Object(Some(longs)), Value::Float(0.0)),
        ];
        for (arr, index) in cases {
            let mut f = long_frame(code(), arr, index, Some(5));
            let code_len = f.code.len() - 2;
            assert_eq!(
                fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
                None
            );
            assert_eq!(f.stack.len(), 0);
            assert_eq!(f.get_local_compact_unchecked(2).as_long_unchecked(), 5);
        }
        // An accumulator slot without the `long` kind mark.
        let mut f = long_frame(code(), Value::Object(Some(longs)), Value::Int(0), None);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
            None
        );
        assert_eq!(f.get_local_compact_unchecked(2).as_int(), Some(0));
        // An `int[]` under `laload` (another element type).
        let Some(ints) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 2) else {
            return;
        };
        let mut f = long_frame(code(), Value::Object(Some(ints)), Value::Int(0), Some(5));
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
            None
        );
        assert_eq!(f.get_local_compact_unchecked(2).as_long_unchecked(), 5);
    }
}
