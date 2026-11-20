// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
use cratonvm_gc::VmHeap;
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
pub(super) fn fast_field_zgc(shared: &SharedVm) -> Option<&crate::memory::vm_heap::VmHeap> {
    if crate::runtime::env_cache::no_field_fast_path() {
        return None;
    }
    if !shared.mem.heap.is_zgc() {
        return None;
    }
    let zgc = &shared.mem.heap;
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
    // zt t18/zb1 (zr1br A2a): the quickened arms read reference slots raw
    // (compact fields, legacy cells, `aaload`), so an ARMED read barrier keeps
    // them off for the frame and the full handlers -- whose heap accessors
    // barrier -- serve. Arming happens at heap construction, before the first
    // frame (`ZgcRealHeap::arm_load_barrier_plain_stores`); a mid-run arming
    // would also have to reach frames admitted before it.
    if zgc.load_barrier_armed() {
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
fn field_ptr_for(zgc: &crate::memory::vm_heap::VmHeap, ptr: u64, site: &FastFieldSite) -> Option<*mut u8> {
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
        // JIT round 22 wave 5c, lane sync: the raw byte, as `read_compact_field`
        // and the JIT's `MOVZX` read it (HotSpot's `getfield`).
        FieldStorageKind::Boolean => CompactValue::int(i32::from(unsafe { load_u8(fp) })),
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
                // JIT round 22 wave 6, lane sync: the read side (`Z` raw).
                value = super::narrow_int_read_from_field(value, desc);
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
                // JIT round 22 wave 6, lane sync: the read side (`Z` raw).
                Value::Int(x) => match super::narrow_int_read_from_field(Value::Int(x), desc) {
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
                    )?,                };
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
                        zgc.zgc_satb_pre_barrier(old as usize);
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
                zgc.zgc_note_ref_store(recv as usize);
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
            super::narrow_int_to_field_type(Value::Int(v), desc)        }    };
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
            zgc.zgc_satb_pre_barrier(old.as_ptr() as usize);
        }
    }
    // SAFETY: `cell` is a live 16-byte field cell; `write_value_atomic` is the
    // padding-free, marker-safe store `set_field` uses for this layout.
    unsafe { cratonvm_types::write_value_atomic(cell as *mut Value, value) };
    // Card only a reference store -- see the compact `Reference` arm of
    // `put_field_value`. The descriptor decided `value`'s shape above, so a
    // `J`/`D`/sub-int store here cannot be a reference.
    if matches!(site.desc_byte, b'L' | b'[') {
        zgc.zgc_note_ref_store(recv as usize);
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
    zgc: &crate::memory::vm_heap::VmHeap,
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

/// Should [`fill_site`] record a site on a heap WITHOUT the quickened arms
/// (Generational, G1)? Only under `CRATONVM_INTERP_FIELD_CTOR_GEN_G1`, only for
/// a `putfield`, and only from an `<init>` frame: the one reader of such a site
/// is [`ctor_field_stores_accessor`], which looks up the stores of a
/// constructor's own body. Tier-1 round wave 2, lane K.
#[inline]
fn accessor_site_fill_wanted(shared: &SharedVm, thread: &JvmThread, is_put: bool) -> bool {
    is_put
        && shared.config.interp_field_ctor_gen_g1
        && (shared.mem.heap.is_generational() || shared.mem.heap.is_g1())
        && thread
            .frames
            .last()
            .is_some_and(|f| f.method_name() == "<init>")
}

/// May [`ctor_field_stores_accessor`] serve on this VM's heap? The Generational
/// and G1 heaps only (ZGC has [`ctor_field_stores_fast`]), and none of the
/// per-store diagnostics `op_putfield` honours may be armed -- the same list
/// [`fast_field_zgc`] refuses the quickened arms for, plus the dynamic cell
/// watch `op_putfield` reports (`[WATCHFIELD]`). Tier-1 round wave 2, lane K.
pub(super) fn ctor_accessor_heap_admitted(shared: &SharedVm) -> bool {
    (shared.mem.heap.is_generational() || shared.mem.heap.is_g1()) && !crate::runtime::env_cache::no_field_fast_path()
        && !crate::runtime::env_cache::any_field_diag()
        && !crate::runtime::env_cache::dbg_field_watch()
        && !crate::runtime::env_cache::corrupt_cell_dbg()
        && !crate::runtime::env_cache::corrupt_cell_selftest()
        && !super::straystack_enabled()
        && !super::remap_trace_on()
        && !crate::runtime::ec_watch::enabled()
        && !cratonvm_gc::gc_quiescence::vacated_frames_enabled()
        && cratonvm_gc::heap::dynamic_watch_addr() == 0
}

/// The `Value` `op_putfield` would store for an operand-stack slot `val` of
/// kind `kind` into a field of descriptor byte `desc`, or `None` for every
/// shape `op_putfield` decodes by a heuristic (an unmarked category-2 slot, an
/// `Int(0)` or smuggled long in a reference field): those keep the frame.
#[inline]
fn accessor_store_value(desc: u8, val: CompactValue, kind: u8) -> Option<Value> {
    Some(match desc {
        b'J' => {
            if kind != ValueStack::KIND_MARK_LONG {
                return None;
            }
            Value::Long(val.as_long_unchecked())
        }
        b'D' => {
            if kind != ValueStack::KIND_MARK_DOUBLE {
                return None;
            }
            Value::Double(f64::from_bits(val.raw_bits()))
        }
        b'L' | b'[' => {
            if val.is_null() {
                Value::Object(None)
            } else {
                let p = val.as_object_ptr()?;
                // SAFETY: an `Object`-tagged slot holds a heap address.
                Value::Object(Some(unsafe { ObjectRef::from_raw(p as *mut u8) }))
            }
        }
        b'F' => Value::Float(val.as_float()?),
        b'I' | b'Z' | b'B' | b'C' | b'S' => {
            super::narrow_int_to_field_type(Value::Int(val.as_int()?), desc)
        }
        _ => return None,
    })
}

/// [`ctor_field_stores_fast`] for the Generational and G1 heaps (tier-1 round
/// wave 2, lane K; `CRATONVM_INTERP_FIELD_CTOR_GEN_G1`): the same all-or-nothing
/// checks against the same sites, and then each store made the way
/// `op_putfield` makes it on those heaps -- the SATB pre-barrier on the
/// overwritten reference while the Generational queue is active (G1's accessor
/// publishes it itself, so it is skipped there, as `op_putfield` skips it),
/// then `VmHeap::set_field`, whose own post-barrier is the collector's card
/// mark / remembered-set entry. So the barrier is by construction the one a
/// framed constructor's `putfield` gets, whatever region or generation the
/// new object was allocated in (TLAB, pinned in-place young, humongous, old).
///
/// The caller has asked [`ctor_accessor_heap_admitted`]. Nothing here
/// allocates or reaches a safepoint (a reference value is stored as is; the
/// autobox arm of `set_field` is reached only by a primitive in a reference
/// slot, which [`accessor_store_value`] never produces), so `recv` stays valid
/// across the stores.
pub(super) fn ctor_field_stores_accessor(
    shared: &SharedVm,
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
    let heap = &shared.mem.heap;
    if !heap.is_object_start(recv as usize) {
        return false;
    }
    // SAFETY: `recv` was just confirmed to be a registered object start on
    // this heap, so its first `HEADER_SIZE` bytes are a live `ObjectHeader`.
    let header = unsafe { &*(recv as *const ObjectHeader) };
    if header.kind() == ObjectKind::Array {
        return false;
    }
    let compact = cratonvm_types::is_compact_object(header);
    let mut planned: [Option<(usize, bool, Value)>; CTOR_FIELD_STORES_MAX] =
        [None; CTOR_FIELD_STORES_MAX];
    for (slot, &(cp_index, val, kind)) in planned.iter_mut().zip(stores) {
        let site = match sites.get(class_id, cp_index) {
            Some(s) => *s,
            None => {
                site_stats::bump(site_stats::FAST_PUT_MISS);
                return false;
            }
        };
        if !matches!(
            site.final_put,
            FastFieldSite::PUT_ANY | FastFieldSite::PUT_INIT_ONLY
        ) {
            return false;
        }
        // `field_ptr_for`'s receiver checks: the layout the site was filled
        // against is this object's.
        if header.class_id != site.receiver_class_id
            || compact != site.storage.is_some()
            || (!compact && header.num_slots() != site.num_slots)
        {
            return false;
        }
        let Some(value) = accessor_store_value(site.desc_byte, val, kind) else {
            return false;
        };
        let is_ref = matches!(site.desc_byte, b'L' | b'[');
        *slot = Some((site.field_index as usize, is_ref, value));
    }
    // SAFETY: `recv` is a registered object start (checked above).
    let obj = unsafe { ObjectRef::from_raw(recv as *mut u8) };
    let satb_pre = heap.satb_armed() && !heap.is_g1();
    for (index, is_ref, value) in planned.iter().flatten().copied() {
        if is_ref && satb_pre {
            if let Value::Object(Some(old)) = heap.get_field(obj, index) {
                heap.write_barrier_pre(std::ptr::null_mut(), old);
            }
        }
        heap.set_field(obj, index, value);
        site_stats::bump_if(census, site_stats::FAST_PUT_HIT);
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
    // Tier-1 round wave 2, lane K (`CRATONVM_INTERP_FIELD_CTOR_GEN_G1`): on the
    // Generational and G1 heaps the only reader of a site is the frameless
    // field-store constructor (`ctor_field_stores_accessor`), whose sites are
    // filled by the `putfield`s of an `<init>` frame; every quickened arm asks
    // `fast_field_zgc` and so never reads one there.
    let zgc_heap = shared.mem.heap.is_zgc();
    if !zgc_heap && !accessor_site_fill_wanted(shared, thread, is_put) {
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
    if !zgc_heap
        && thread
            .site_tables
            .fast_field_sites
            .get(current_class_id, cp_index)
            .is_some_and(|s| {
                s.receiver_class_id == header.class_id
                    && s.storage.is_some() == cratonvm_types::is_compact_object(header)
            })
    {
        // Already filled for this receiver class: a constructor that keeps its
        // frame (one the door declines) refills nothing per store.
        return;
    }
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
// `docs/feature-designs/real-jdk-mode.md`,
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
fn prim_elem_ptr(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32, opcode: u8) -> Option<*mut u8> {
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: Option<&cratonvm_gc::VmHeap>,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
// `docs/feature-designs/real-jdk-mode.md`.
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
            (g[5], *code.get(pc + 6)?, 7)        }
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
            unsafe { store_u32(p, v as u32) };        }
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
            unsafe { store_u16(p, v as u16) };        }
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
    zgc: &crate::memory::vm_heap::VmHeap,
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
            zgc.zgc_satb_pre_barrier(old as usize);
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
    zgc.zgc_note_ref_store_element(base, slot as usize, header.array_length() as usize);
    site_stats::bump_if(census, site_stats::AASTORE_HIT);
    true
}

#[cfg(test)]
mod tests;

/// Tier-1 round wave 2, lane K: the frameless field-store constructor's
/// accessor arm for the Generational and G1 heaps
/// (`CRATONVM_INTERP_FIELD_CTOR_GEN_G1`).
#[cfg(test)]
mod t1k_ctor_accessor_tests;

#[cfg(test)]
mod registry_probe_tests;

#[cfg(test)]
mod static_site_tests;

#[cfg(test)]
mod i68_l7_fused_field_increment_tests;

#[cfg(test)]
mod i72_l7_fused_array_increment_tests;

#[cfg(test)]
mod i72_l7_two_field_update_tests;

#[cfg(test)]
mod i73_l7_array_screen_and_accumulate_tests;

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
mod nio21_w8_ref_array_store_cas_race_tests;

#[cfg(test)]
mod i74_l7_array_shift_div_and_no_dup_tests;

#[cfg(test)]
mod i75_l7_array_decline_memo_tests;

#[cfg(test)]
mod i75_l7_wide_local_element_read_tests;

#[cfg(test)]
mod i76_l7_array_increment_operand_tests;

#[cfg(test)]
mod i77_l7_array_operand_budget_and_wide_tests;

#[cfg(test)]
mod i82_l7_long_array_accumulate_tests;
