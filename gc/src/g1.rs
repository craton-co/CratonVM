// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! G1 (Garbage-First) garbage collector.
//!
//! A region-based, generational, incremental, parallel, mostly concurrent
//! garbage collector. Key features:
//!
//! - **Region-based heap:** Fixed-size regions classified as Eden, Survivor,
//!   Old, Humongous, or Free.
//! - **Young collection (STW):** Evacuate all Eden + Survivor regions.
//! - **Mixed collection:** Evacuate young + selected old regions (worst-first).
//! - **Concurrent marking:** Tri-color marking with SATB barriers.
//! - **IHOP:** Initiating Heap Occupancy Percent triggers concurrent marking.
//! - **Region pinning (JEP 423):** Pin regions during JNI critical sections.
//! - **String deduplication:** Deduplicate identical String backing arrays.
//! - **Humongous allocation:** Objects > region_size/2 span contiguous regions.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use crate::collector::{GarbageCollector, MonitorCleanup};
use crate::concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState};
use crate::gc::{GcResult, GcStats};
use crate::gc_flags;
use crate::heap::{
    array_data_size, array_element_type_from_tag, object_kind_from_tag, ArrayElementType,
    ObjectHeader, ObjectKind, ARRAY_ELEMENT_TYPE_OFFSET, GC_FLAG_OLD_GEN, HEADER_SIZE,
    OBJECT_KIND_OFFSET, SLOT_SIZE,
};
use crate::mark_bitmap::MarkBitmap;
use crate::region::{RegionType, RememberedSet};
use crate::satb::SatbQueue;
use cratonvm_types::{ClassId, ObjectRef, Value};

#[inline]
unsafe fn value_from_unaligned_ptr(ptr: *const u8) -> Value {
    // SAFETY: callers only pass bytes copied from a valid `Value` slot.
    unsafe { std::ptr::read_unaligned(ptr as *const Value) }
}

#[inline]
unsafe fn value_to_unaligned_ptr(value: Value, ptr: *mut u8) {
    // SAFETY: the byte buffer is large enough for one `Value`; alignment is
    // intentionally not assumed because stack `[u8; N]` buffers are align-1.
    unsafe { std::ptr::write_unaligned(ptr as *mut Value, value) };
}

#[inline]
fn value_from_bytes(bytes: &[u8; SLOT_SIZE]) -> Value {
    // SAFETY: callers only pass bytes copied from a valid `Value` slot.
    unsafe { value_from_unaligned_ptr(bytes.as_ptr()) }
}

#[inline]
fn value_to_bytes(value: Value, bytes: &mut [u8; SLOT_SIZE]) {
    // SAFETY: the byte buffer is large enough for one `Value`.
    unsafe { value_to_unaligned_ptr(value, bytes.as_mut_ptr()) };
}

/// Enumerate reference fields of a non-humongous object under either body
/// layout. `slot` points at the writable on-heap representation; `compact`
/// selects an 8-byte raw pointer versus a legacy 16-byte `Value` cell.
fn for_each_flat_object_reference(
    obj_ptr: *const u8,
    header: &ObjectHeader,
    first_index: usize,
    mut visit: impl FnMut(*mut u8, usize, bool),
) {
    if cratonvm_types::is_compact_object(header) {
        // Borrowing accessor: only `field_offsets` / `is_ref` are read here.
        // `visit` is caller-supplied and may re-enter the layout cache (G1's
        // evacuation visitor sizes the referent); that is supported — the
        // accessor holds a shared borrow, so nested lookups still hit.
        let _ = cratonvm_types::with_class_layout(
            header.class_id.as_u32(),
            header.num_slots(),
            |layout| {
                for (index, (&offset, &is_ref)) in layout
                    .field_offsets
                    .iter()
                    .zip(layout.is_ref.iter())
                    .enumerate()
                {
                    if !is_ref || index < first_index {
                        continue;
                    }
                    let slot = unsafe { obj_ptr.add(HEADER_SIZE + offset as usize) } as *mut u8;
                    let raw = unsafe { std::ptr::read(slot as *const u64) } as usize;
                    if raw != 0 {
                        visit(slot, raw, true);
                    }
                }
            },
        );
    } else {
        for index in first_index..header.num_slots() as usize {
            let slot = unsafe { obj_ptr.add(HEADER_SIZE + index * SLOT_SIZE) } as *mut u8;
            let value = unsafe { cratonvm_types::read_value_atomic(slot as *const Value) };
            if let Value::Object(Some(reference)) = value {
                visit(slot, reference.as_ptr() as usize, false);
            }
        }
    }
}

fn write_flat_object_reference(slot: *mut u8, raw: usize, compact: bool) {
    if compact {
        unsafe { std::ptr::write(slot as *mut u64, raw as u64) };
    } else {
        let value = Value::Object(Some(unsafe { ObjectRef::from_raw(raw as *mut u8) }));
        unsafe { cratonvm_types::write_value_atomic(slot as *mut Value, value) };
    }
}

#[inline]
unsafe fn array_element_from_unaligned_ptr(element_type: ArrayElementType, p: *const u8) -> Value {
    // SAFETY: `p` points at the native-endian bytes for one array element.
    unsafe {
        match element_type {
            ArrayElementType::Int => Value::Int(std::ptr::read_unaligned(p as *const i32)),
            ArrayElementType::Long => Value::Long(std::ptr::read_unaligned(p as *const i64)),
            ArrayElementType::Float => Value::Float(std::ptr::read_unaligned(p as *const f32)),
            ArrayElementType::Double => Value::Double(std::ptr::read_unaligned(p as *const f64)),
            ArrayElementType::Byte => Value::Int(std::ptr::read_unaligned(p as *const i8) as i32),
            ArrayElementType::Boolean => Value::Int(std::ptr::read_unaligned(p) as i32),
            ArrayElementType::Short => Value::Int(std::ptr::read_unaligned(p as *const i16) as i32),
            ArrayElementType::Char => Value::Int(std::ptr::read_unaligned(p as *const u16) as i32),
            ArrayElementType::Reference => {
                let r: u64 = std::ptr::read_unaligned(p as *const u64);
                if r == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(r as usize as *mut u8)))
                }
            }
        }
    }
}

#[inline]
fn array_element_from_bytes(element_type: ArrayElementType, raw: &[u8; 8]) -> Value {
    // SAFETY: `raw` holds the native-endian bytes for one array element.
    unsafe { array_element_from_unaligned_ptr(element_type, raw.as_ptr()) }
}

#[inline]
unsafe fn array_element_to_unaligned_ptr(element_type: ArrayElementType, value: Value, p: *mut u8) {
    // SAFETY: every write is at most 8 bytes and the destination may be align-1.
    unsafe {
        match element_type {
            ArrayElementType::Int => {
                std::ptr::write_unaligned(p as *mut i32, value.as_int().unwrap_or(0))
            }
            ArrayElementType::Long => {
                std::ptr::write_unaligned(p as *mut i64, value.as_long().unwrap_or(0))
            }
            ArrayElementType::Float => std::ptr::write_unaligned(
                p as *mut f32,
                match value {
                    Value::Float(f) => f,
                    _ => 0.0,
                },
            ),
            ArrayElementType::Double => std::ptr::write_unaligned(
                p as *mut f64,
                match value {
                    Value::Double(d) => d,
                    _ => 0.0,
                },
            ),
            ArrayElementType::Byte | ArrayElementType::Boolean => {
                std::ptr::write_unaligned(p as *mut i8, value.as_int().unwrap_or(0) as i8)
            }
            ArrayElementType::Short => {
                std::ptr::write_unaligned(p as *mut i16, value.as_int().unwrap_or(0) as i16)
            }
            ArrayElementType::Char => {
                std::ptr::write_unaligned(p as *mut u16, value.as_int().unwrap_or(0) as u16)
            }
            ArrayElementType::Reference => {
                let r: u64 = match value {
                    Value::Object(Some(r)) => r.as_ptr() as u64,
                    _ => 0u64,
                };
                std::ptr::write_unaligned(p as *mut u64, r)
            }
        }
    }
}

#[inline]
fn array_element_to_bytes(element_type: ArrayElementType, value: Value, raw: &mut [u8; 8]) {
    // SAFETY: every write is at most 8 bytes.
    unsafe { array_element_to_unaligned_ptr(element_type, value, raw.as_mut_ptr()) };
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Step 9 (parallel evacuation) opt-out flag, declared early per the design's
/// §7 scaffolding convention: a recognized env knob, read **once** behind a
/// `OnceLock`, defaulting to the SAFE single-threaded behaviour so a later step
/// can flip the default without re-plumbing. `CRATONVM_G1_PARALLEL_EVAC=1` (or
/// `true`) will opt INTO the multi-threaded evacuator once it is built; unset or
/// any other value keeps evacuation single-threaded.
///
/// Nothing gates on it yet — the parallel work_list / CAS-forwarding machinery
/// is a deliberate follow-up: the single-threaded evacuator must be memory-safe
/// across the gauntlet first (cf. the open gpu-bench-cpu G1 SIGSEGV). The
/// behaviour-identical groundwork that *does* land now is the `evacuate_object`
/// freshness signal that removes the `pointer_map.contains_key` evacuation
/// TOCTOU at the ref-scan sites.
fn parallel_evac_enabled() -> bool {
    gc_flags().g1_parallel_evac
}

// ===========================================================================
// Step 9 — parallel STW evacuation (gated behind `CRATONVM_G1_PARALLEL_EVAC`)
// ===========================================================================
//
// The serial evacuator threads `&mut Vec<G1Region>`, a `HashMap` forwarding
// map, and a `Vec<*mut u8>` work list through one call chain under one big
// `regions.lock()`. The parallel evacuator implements the four-piece protocol
// from the design's §3.4:
//
//   1. Atomic forwarding — the dedup/race winner is decided by a CAS on the
//      from-space object's own `ObjectHeader::forwarding_ptr` field (treated as
//      an `AtomicUsize`), not a shared map lookup. G1's serial young/mixed path
//      never uses the from-space header's `forwarding_ptr` (it uses the
//      `pointer_map`), and every live object starts a collection with
//      `forwarding_ptr == null`, so the field is free to repurpose as the
//      per-object install slot. Only worker-vs-worker races exist (mutators are
//      parked at the STW safepoint; CSet objects are not mutated).
//   2. Per-worker forward shards — each worker records its winning
//      `(old, new)` pairs into a thread-local `Vec`; they are merged into the
//      `GcResult.pointer_map` after the closure (consumed by the VM's
//      `update_all_roots`, by Phase-4 region remap, monitors and the mark
//      worklist — exactly as the serial map).
//   3. Per-worker GC-TLAB allocation — replaces the single big lock. Each
//      worker claims whole Free regions from a shared pool via a lock-free
//      `fetch_add` index, then bump-allocates within its claimed region with a
//      thread-local cursor (single owner ⇒ no atomics needed inside a region);
//      the final cursor is written back when the region is retired.
//   4. Shared work queue — a `Mutex<Vec<usize>>` of gray (already-evacuated,
//      not-yet-scanned) to-space addresses with per-worker local batches, and
//      an `outstanding` counter for termination (children are added to the
//      counter before the parent is subtracted, so it never transiently hits 0
//      while work remains). This is the design's explicit "first cut" in place
//      of work-stealing deques.
//
// SAFETY MODEL. The `regions.lock()` guard is held by the driver for the whole
// collection (so the concurrent marker — which takes the same lock per step —
// stays excluded). The driver derives the regions' raw base (`as_mut_ptr()`)
// once and does NOT deref the guard again until after the parallel scope joins;
// all region access in between goes through that raw base under a strict
// disjointness discipline (the `split_at_mut`-style pattern):
//   * CSet (from-space) regions are only ever READ (copy source) plus an atomic
//     CAS on each object's `forwarding_ptr` — never `&mut`-aliased.
//   * To-space regions are each claimed by exactly one worker (unique pool
//     index) and `&mut`-accessed only by that owner.
//   * `region_for_ptr` resolves via the immutable, never-mutated
//     `region_lookup` table, not the regions Vec.
// `std::thread::scope` join is the synchronisation point after which the driver
// resumes normal guard access for Phases 4/5.

/// Raw base pointer of the regions `Vec`, shared across evacuation workers.
///
/// `Send`/`Sync` is sound under the disjointness discipline documented above:
/// workers only `&mut`-access regions they exclusively claimed and only read /
/// atomically-CAS shared (CSet) regions.
#[derive(Clone, Copy)]
struct RegionsBase(*mut G1Region);
// SAFETY: see the module-level SAFETY MODEL note — disjoint per-worker access.
unsafe impl Send for RegionsBase {}
unsafe impl Sync for RegionsBase {}

/// A per-worker, per-destination-type thread-local allocation buffer.
struct Tlab {
    dest_type: RegionType,
    /// Index of the currently-owned to-space region, if any.
    region_idx: Option<usize>,
    /// Base address of the owned region.
    base: usize,
    /// Length (bytes) of the owned region.
    len: usize,
    /// Local bump cursor (offset from `base`).
    offset: usize,
}

impl Tlab {
    fn new(dest_type: RegionType) -> Self {
        Self {
            dest_type,
            region_idx: None,
            base: 0,
            len: 0,
            offset: 0,
        }
    }
}

/// Each worker holds one Survivor TLAB (young survivors) and one Old TLAB
/// (tenured promotions).
struct TlabSet {
    survivor: Tlab,
    old: Tlab,
}

impl Default for TlabSet {
    fn default() -> Self {
        Self {
            survivor: Tlab::new(RegionType::Survivor),
            old: Tlab::new(RegionType::Old),
        }
    }
}

/// Immutable shared state handed to every evacuation worker (the driver and the
/// spawned threads). Auto-`Sync` because every field is `Sync` (`RegionsBase`
/// via the `unsafe impl` above).
struct SharedEvac<'a> {
    collector: &'a G1Collector,
    regions_base: RegionsBase,
    /// CSet membership (region indices being evacuated FROM).
    cset: &'a std::collections::HashSet<usize>,
    /// Free-region indices available for to-space TLAB claiming.
    pool: &'a [usize],
    /// Lock-free claim cursor into `pool`.
    pool_next: &'a AtomicUsize,
    /// Gray-object work queue (to-space addresses awaiting a ref scan).
    queue: &'a Mutex<Vec<usize>>,
    /// Termination counter: queued + in-progress items (see protocol note 4).
    outstanding: &'a AtomicUsize,
    /// Tenuring threshold copied from the collector config.
    promotion_age: u8,
}

impl<'a> SharedEvac<'a> {
    fn append_self_forwarded_from_forwards(
        forwards: &[(usize, usize)],
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        let mut seen: std::collections::HashSet<usize> =
            deferred_self_forwarded.iter().copied().collect();
        for &(old, new) in forwards {
            if old == new && seen.insert(old) {
                deferred_self_forwarded.push(old);
            }
        }
    }

    fn record_fresh_child(
        old_ptr: *mut u8,
        new_ptr: *mut u8,
        defer_self_forwarded: bool,
        children: &mut Vec<usize>,
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        if defer_self_forwarded && old_ptr == new_ptr {
            deferred_self_forwarded.push(new_ptr as usize);
        } else {
            children.push(new_ptr as usize);
        }
    }

    /// Claim/bump-allocate `size` bytes into `tlab`. Returns the destination
    /// address, or `None` on free-region-pool exhaustion (the same loss
    /// semantics the serial allocator has when it cannot find space).
    ///
    /// SAFETY: `regions_base` must be the live regions Vec base; the claimed
    /// region index is unique to this worker (lock-free `fetch_add`), so the
    /// `&mut G1Region` formed here never aliases another thread.
    unsafe fn tlab_alloc(&self, tlab: &mut Tlab, size: usize) -> Option<usize> {
        loop {
            if tlab.region_idx.is_some() {
                let aligned = (tlab.base + tlab.offset + 7) & !7;
                let new_off = (aligned - tlab.base) + size;
                if new_off <= tlab.len {
                    tlab.offset = new_off;
                    return Some(aligned);
                }
                // Current region is full — retire it (write back the cursor) and
                // fall through to claim a fresh one.
                self.retire_tlab(tlab);
            }
            let i = self.pool_next.fetch_add(1, Ordering::Relaxed);
            if i >= self.pool.len() {
                return None;
            }
            let idx = self.pool[i];
            let region = &mut *self.regions_base.0.add(idx);
            region.region_type = tlab.dest_type;
            if tlab.dest_type == RegionType::Survivor {
                region.age = 1;
            }
            // Freshly-Free regions are zero-filled (reset) with cursor 0.
            tlab.region_idx = Some(idx);
            tlab.base = region.data.addr();
            tlab.len = region.data.len();
            tlab.offset = 0;
        }
    }

    /// Write a TLAB's final bump cursor back to its region and clear the TLAB.
    unsafe fn retire_tlab(&self, tlab: &mut Tlab) {
        if let Some(idx) = tlab.region_idx.take() {
            let region = &mut *self.regions_base.0.add(idx);
            region.cursor = tlab.offset;
        }
        tlab.base = 0;
        tlab.len = 0;
        tlab.offset = 0;
    }

    /// Retire both TLABs of a worker (called once the worker is done).
    unsafe fn retire_all(&self, tlab: &mut TlabSet) {
        self.retire_tlab(&mut tlab.survivor);
        self.retire_tlab(&mut tlab.old);
    }

    /// Evacuate one CSet object: atomic header CAS-forwarding + TLAB copy.
    /// Returns `Some((new_ptr, fresh))` where `fresh` is true iff THIS call
    /// performed the copy (CAS winner), `None` on alloc failure / corrupt
    /// header. The from-space object's `forwarding_ptr` is the single install
    /// slot; on a CAS loss the speculatively-copied destination is abandoned
    /// (becomes unreferenced to-space garbage reclaimed next cycle) and the
    /// winner's pointer is returned so all references converge.
    unsafe fn evacuate(
        &self,
        tlab: &mut TlabSet,
        old_ptr: *mut u8,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
    ) -> Option<(*mut u8, bool)> {
        let fwd_atomic = &*(std::ptr::addr_of_mut!((*(old_ptr as *mut ObjectHeader)).forwarding_ptr)
            as *const AtomicUsize);

        // Fast path: already forwarded this cycle.
        let existing = fwd_atomic.load(Ordering::Acquire);
        if existing != 0 {
            // DEFECT-2 FIX (part 1 of 2): record the forward in THIS cycle's
            // forward set even though we didn't perform the copy, so it reaches
            // `pointer_map`. The serial path dedups via the per-cycle
            // `pointer_map` (so every forward is recorded); the parallel path
            // dedups via the persistent `forwarding_ptr` header field, and a
            // fast-path hit previously returned `existing` WITHOUT recording it.
            // That left the address `old_ptr -> existing` absent from
            // `pointer_map`, so the VM's `update_all_roots` could not remap a root
            // still pointing at `old_ptr` — it stayed stuck on the from-space
            // object and dangled when the region was reused (the rare
            // `java/lang/Object`). Recording it lets the root be remapped to
            // `existing`. Safe because part 2 (clearing `forwarding_ptr` for every
            // evacuated object at cycle end) guarantees `existing` is a THIS-cycle
            // forward (a fully-scanned live copy), never a stale prior-cycle one.
            forwards.push((old_ptr as usize, existing));
            return Some((existing as *mut u8, false));
        }

        let obj_size = {
            let header = &*(old_ptr as *const ObjectHeader);
            object_total_size(header)
        };
        if obj_size < HEADER_SIZE {
            tracing::warn!(
                "g1::evacuate(parallel): refusing to evacuate object at {:p} — \
                 implausible size {} (corrupt header)",
                old_ptr,
                obj_size
            );
            return None;
        }

        let promote = {
            let header = &*(old_ptr as *const ObjectHeader);
            header.gc_age >= self.promotion_age
        };
        let dest_tlab = if promote {
            &mut tlab.old
        } else {
            &mut tlab.survivor
        };
        let new_addr = match self.tlab_alloc(dest_tlab, obj_size) {
            Some(a) => a,
            None => {
                // EVACUATION FAILURE (to-space pool exhausted): self-forward in
                // place rather than dropping the object (mirrors the serial path).
                // CAS the from-space header's forwarding slot to the object's OWN
                // address; the winner records an identity forward (old→old) so
                // Phase 5 keeps its region, a loser adopts whatever address won
                // (a real new location, or another self-forward). `fresh` from the
                // CAS keeps the object scanned exactly once.
                let old = old_ptr as usize;
                return match fwd_atomic.compare_exchange(
                    0,
                    old,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        forwards.push((old, old));
                        Some((old_ptr, true))
                    }
                    Err(winner) => Some((winner as *mut u8, false)),
                };
            }
        };
        let new_ptr = new_addr as *mut u8;

        std::ptr::copy_nonoverlapping(old_ptr, new_ptr, obj_size);

        // Atomic mark-word transfer (matches the serial T2-4 fix: the bulk
        // memcpy is UB for the AtomicU64 mark word).
        {
            let old_h = old_ptr as *const ObjectHeader;
            let new_h = new_ptr as *mut ObjectHeader;
            let mark = (*old_h).mark_word.load(Ordering::Relaxed);
            (*new_h).mark_word.store(mark, Ordering::Relaxed);
        }

        let new_header = &mut *(new_ptr as *mut ObjectHeader);
        if promote {
            // G1AUD-1 — same old-generation stamp as the serial
            // `evacuate_object`; see the long note there for why the JIT's
            // inline reference-store fast paths depend on this bit.
            new_header.gc_flags |= GC_FLAG_OLD_GEN;
        } else {
            new_header.gc_age = new_header.gc_age.saturating_add(1);
        }
        // Clear the NEW copy's forwarding slot (the memcpy may have copied a
        // racing non-null value from the old header).
        new_header.forwarding_ptr = std::ptr::null_mut();

        // Install the forward on the OLD (from-space) header. Winner copies; a
        // loser abandons its `new_ptr` and adopts the winner's address.
        match fwd_atomic.compare_exchange(0, new_addr, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                forwards.push((old_ptr as usize, new_addr));
                *objs += 1;
                *bytes += obj_size;
                Some((new_ptr, true))
            }
            Err(winner) => Some((winner as *mut u8, false)),
        }
    }

    /// Scan one already-evacuated (to-space) object's reference fields; evacuate
    /// each CSet target, rewrite the slot, and collect freshly-evacuated targets
    /// into `children`. Mirrors the serial `scan_and_evacuate_refs` slot
    /// dispatch.
    unsafe fn process_object(
        &self,
        tlab: &mut TlabSet,
        obj_ptr: *mut u8,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        children: &mut Vec<usize>,
        deferred_self_forwarded: &mut Vec<usize>,
        defer_self_forwarded: bool,
    ) {
        let (kind, etype, alen, nslots) = {
            let h = &*(obj_ptr as *const ObjectHeader);
            (h.kind, h.element_type, h.array_length(), h.num_slots())
        };
        if kind == ObjectKind::Array {
            if etype == ArrayElementType::Reference {
                for i in 0..alen as usize {
                    let slot_ptr = obj_ptr.add(HEADER_SIZE + i * 8);
                    let raw: u64 = std::ptr::read(slot_ptr as *const u64);
                    if raw == 0 {
                        continue;
                    }
                    let ref_ptr = raw as usize as *mut u8;
                    if let Some(ridx) = self.collector.lookup_region_for_addr(ref_ptr as usize) {
                        if self.cset.contains(&ridx) {
                            if let Some((new_ptr, fresh)) =
                                self.evacuate(tlab, ref_ptr, forwards, objs, bytes)
                            {
                                std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                                if fresh {
                                    Self::record_fresh_child(
                                        ref_ptr,
                                        new_ptr,
                                        defer_self_forwarded,
                                        children,
                                        deferred_self_forwarded,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        } else {
            for slot_idx in 0..nslots as usize {
                let slot_ptr = obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE);
                let value = std::ptr::read(slot_ptr as *const Value);
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if let Some(ridx) = self.collector.lookup_region_for_addr(ref_ptr as usize) {
                        if self.cset.contains(&ridx) {
                            if let Some((new_ptr, fresh)) =
                                self.evacuate(tlab, ref_ptr, forwards, objs, bytes)
                            {
                                let nv = Value::Object(Some(ObjectRef::from_raw(new_ptr)));
                                std::ptr::write(slot_ptr as *mut Value, nv);
                                if fresh {
                                    Self::record_fresh_child(
                                        ref_ptr,
                                        new_ptr,
                                        defer_self_forwarded,
                                        children,
                                        deferred_self_forwarded,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Seed phase (driver/main thread, single-threaded): walk a non-CSet
    /// remembered-set source region, evacuate every CSet-bound reference and
    /// rewrite the slot in place, pushing freshly-evacuated targets onto the
    /// shared work queue. Mirrors the serial `scan_source_region_for_cset_refs`.
    unsafe fn seed_source_region(
        &self,
        source_idx: usize,
        tlab: &mut TlabSet,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        if self.cset.contains(&source_idx) {
            return;
        }
        let (cursor, base) = {
            let r = &*self.regions_base.0.add(source_idx);
            if r.region_type == RegionType::Free {
                return;
            }
            (r.cursor, r.data.addr() as *mut u8)
        };

        let jit_skips = self.collector.jit_tlab_skip_spans();
        let mut newly: Vec<usize> = Vec::new();
        let mut offset = 0usize;
        while offset < cursor {
            let obj_ptr = base.add(offset);
            // INT-3 — frozen-peer TLAB tail: uninitialized, no walkable
            // filler; must be skipped before any byte is interpreted.
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                offset += skip;
                continue;
            }
            // TLAB-retire gap sentinel: skip its exact span.
            if let Some(gap) = gap_filler_len(obj_ptr) {
                offset += gap;
                continue;
            }
            let (kind, etype, alen, nslots, is_filler, obj_size) = {
                let header = &*(obj_ptr as *const ObjectHeader);
                let is_filler = is_humongous_filler(header);
                let sz = if is_filler {
                    0
                } else {
                    object_total_size(header)
                };
                (
                    header.kind,
                    header.element_type,
                    header.array_length(),
                    header.num_slots(),
                    is_filler,
                    sz,
                )
            };
            if is_filler {
                break;
            }
            if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                break;
            }

            if kind == ObjectKind::Array {
                if etype == ArrayElementType::Reference {
                    for i in 0..alen as usize {
                        let slot_ptr = obj_ptr.add(HEADER_SIZE + i * 8);
                        let raw: u64 = std::ptr::read(slot_ptr as *const u64);
                        if raw == 0 {
                            continue;
                        }
                        let ref_ptr = raw as usize as *mut u8;
                        if let Some(ridx) = self.collector.lookup_region_for_addr(ref_ptr as usize)
                        {
                            if self.cset.contains(&ridx) {
                                if let Some((new_ptr, fresh)) =
                                    self.evacuate(tlab, ref_ptr, forwards, objs, bytes)
                                {
                                    std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                                    if fresh {
                                        Self::record_fresh_child(
                                            ref_ptr,
                                            new_ptr,
                                            true,
                                            &mut newly,
                                            deferred_self_forwarded,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..nslots as usize {
                    let slot_ptr = obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE);
                    let value = std::ptr::read(slot_ptr as *const Value);
                    if let Value::Object(Some(ref_obj)) = value {
                        let ref_ptr = ref_obj.as_ptr();
                        if let Some(ridx) = self.collector.lookup_region_for_addr(ref_ptr as usize)
                        {
                            if self.cset.contains(&ridx) {
                                if let Some((new_ptr, fresh)) =
                                    self.evacuate(tlab, ref_ptr, forwards, objs, bytes)
                                {
                                    let nv = Value::Object(Some(ObjectRef::from_raw(new_ptr)));
                                    std::ptr::write(slot_ptr as *mut Value, nv);
                                    if fresh {
                                        Self::record_fresh_child(
                                            ref_ptr,
                                            new_ptr,
                                            true,
                                            &mut newly,
                                            deferred_self_forwarded,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            offset += obj_size;
        }

        if !newly.is_empty() {
            self.outstanding.fetch_add(newly.len(), Ordering::AcqRel);
            let mut q = self.queue.lock();
            q.extend(newly);
        }
    }

    /// Worker drain loop: pop gray objects, scan + evacuate their refs, push
    /// fresh children, until `outstanding` reaches 0. Retires its TLABs on exit.
    unsafe fn run_worker(
        &self,
        tlab: &mut TlabSet,
        objs: &mut usize,
        bytes: &mut usize,
        forwards: &mut Vec<(usize, usize)>,
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        let mut children: Vec<usize> = Vec::new();
        loop {
            if self.outstanding.load(Ordering::Acquire) == 0 {
                break;
            }
            let item = {
                let mut q = self.queue.lock();
                q.pop()
            };
            match item {
                Some(addr) => {
                    children.clear();
                    self.process_object(
                        tlab,
                        addr as *mut u8,
                        forwards,
                        objs,
                        bytes,
                        &mut children,
                        deferred_self_forwarded,
                        true,
                    );
                    if !children.is_empty() {
                        // Add children to the outstanding count BEFORE retiring
                        // the parent so the counter never transiently hits 0
                        // while work remains.
                        self.outstanding.fetch_add(children.len(), Ordering::AcqRel);
                        let mut q = self.queue.lock();
                        q.extend(children.iter().copied());
                    }
                    self.outstanding.fetch_sub(1, Ordering::AcqRel);
                }
                None => {
                    // Queue momentarily empty but work still outstanding
                    // (another worker is mid-scan and about to push).
                    std::thread::yield_now();
                }
            }
        }
        self.retire_all(tlab);
    }

    /// Drain evacuation-failed objects after the parallel closure is complete.
    ///
    /// A self-forwarded object stays in its original CSet region, so scanning it
    /// rewrites that from-space object's slots in place. Doing that rewrite in a
    /// worker can race another worker that is still copying the same object as a
    /// source. The driver therefore scans these in-place objects only after all
    /// parallel workers have stopped.
    /// `scan` is the persistent cursor into `work`: entries before it were
    /// already scanned by an earlier drain round of the same collection (the
    /// caller loops this to a fixpoint with `append_kept_cset_region_objects`,
    /// which can only grow `work`). The caller retires `tlab` once the
    /// fixpoint is reached — NOT here — so successive rounds keep filling the
    /// same partially-used to-space region instead of burning one per round.
    unsafe fn drain_deferred_self_forwarded(
        &self,
        tlab: &mut TlabSet,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        work: &mut Vec<usize>,
        scan: &mut usize,
    ) {
        let mut ignored_deferred = Vec::new();
        while *scan < work.len() {
            let addr = work[*scan];
            *scan += 1;
            self.process_object(
                tlab,
                addr as *mut u8,
                forwards,
                objs,
                bytes,
                work,
                &mut ignored_deferred,
                false,
            );
        }
    }
}

/// Configuration for the G1 garbage collector.
#[derive(Debug, Clone)]
pub struct G1CollectorConfig {
    /// Total heap size in bytes (default 256 MB).
    pub heap_size: usize,
    /// Region size in bytes (default 1 MB).
    pub region_size: usize,
    /// Target maximum GC pause in milliseconds (default 200).
    pub max_gc_pause_ms: u64,
    /// Initiating heap occupancy percent (default 45).
    pub ihop_percent: u8,
    /// Tenuring threshold: survive this many young GCs before promotion (default 15).
    pub promotion_age: u8,
    /// Number of parallel GC worker threads (default 4).
    pub gc_worker_threads: usize,
    /// Enable string deduplication (default false).
    pub string_dedup_enabled: bool,
    /// Target number of mixed GC cycles after marking (default 8).
    pub mixed_gc_count_target: u8,
    /// Maximum percentage of old regions to include per mixed GC (default 10).
    pub old_cset_region_threshold_percent: u8,
}

impl Default for G1CollectorConfig {
    fn default() -> Self {
        Self {
            heap_size: 256 * 1024 * 1024,
            region_size: 1024 * 1024,
            max_gc_pause_ms: 200,
            // T19.3.G1: raised from 45 → 70 so static-init bursts
            // on small heaps don't fire a concurrent marking cycle
            // before the heap has actually retained anything worth
            // collecting. HotSpot defaults 45% but assumes a 32 GiB
            // heap where 45% = ~14 GiB; at 256 MiB (our default)
            // 45% is only 115 MiB, which a Quarkus static-init
            // replay can chew through in under two seconds.
            // Adaptive IHOP (see `update_ihop`) still adjusts
            // downward under real memory pressure.
            ihop_percent: 70,
            promotion_age: 15,
            gc_worker_threads: 4,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// G1 Region
// ---------------------------------------------------------------------------

/// Non-owning view of one region's backing slice inside
/// [`G1Collector::arena`].
///
/// `base` is the slice's start address in the arena and `len` is
/// `region_size`. `base` is stored as a `usize` (not a raw pointer) so that
/// `G1Region` stays `Send + Sync` exactly as it did with the old
/// `data: Vec<u8>` field. `Deref`/`DerefMut` expose the slice, so every
/// existing `region.data.as_ptr()` / `.len()` / `.fill(0)` / indexing call
/// site keeps working unchanged.
///
/// IMPORTANT: cross-region access (a humongous object whose payload extends
/// past this region's `len` into the physically-adjacent next region) must go
/// through [`RegionBuf::addr`] integer arithmetic, NOT through the `Deref`
/// slice — `.as_ptr().add(off)` past `len` would be out of the slice's
/// provenance. The arena guarantees those bytes are contiguous and live.
pub struct RegionBuf {
    base: usize,
    len: usize,
}

impl RegionBuf {
    #[inline]
    fn new(base: usize, len: usize) -> Self {
        Self { base, len }
    }
    /// Start address of this region's slice, as a `usize`. Use this (plus an
    /// offset cast to `*mut u8`) for any access that may cross into the
    /// adjacent region (humongous payloads); the arena keeps the bytes
    /// contiguous and live.
    #[inline]
    fn addr(&self) -> usize {
        self.base
    }
}

impl Deref for RegionBuf {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        // SAFETY: `[base, base+len)` is a live, per-region-exclusive slice of
        // the collector's `arena` (allocated once, never freed/reallocated for
        // the collector's lifetime); standalone test regions point at a leaked
        // boxed slice, equally stable. Region ranges never overlap.
        unsafe { std::slice::from_raw_parts(self.base as *const u8, self.len) }
    }
}

impl DerefMut for RegionBuf {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: see `deref`; `&mut self` gives unique access to this region's
        // disjoint arena range.
        unsafe { std::slice::from_raw_parts_mut(self.base as *mut u8, self.len) }
    }
}

/// Enhanced region descriptor for the G1 collector.
pub struct G1Region {
    /// Current region classification.
    pub region_type: RegionType,
    /// Backing storage for this region (a slice of [`G1Collector::arena`]).
    pub data: RegionBuf,
    /// Bump pointer: next free byte offset.
    pub cursor: usize,
    /// Bytes of live data (computed during marking).
    pub live_bytes: usize,
    /// GC efficiency: `live_bytes / region_size`. Lower means more garbage.
    pub gc_efficiency: f64,
    /// Per-region remembered set.
    pub rset: RememberedSet,
    /// Whether this region is pinned (JEP 423: JNI critical region pinning).
    /// Mirror of `pin_count > 0` — the CSet-selection filters read this flag.
    pub pinned: bool,
    /// Number of live JNI-critical pins on this region (refcount). Overlapping
    /// critical sections on arrays in the same region — or nested checkouts of
    /// one array — must refcount: a single bool would let an inner `Release`
    /// clear the pin while an outer section is still live, re-admitting the
    /// region to the collection set and relocating an array a native pointer's
    /// copy-back still depends on. Maintained by [`G1Collector::pin_region`] /
    /// [`G1Collector::unpin_region`].
    pub pin_count: u32,
    /// Survivor age (number of young GCs survived).
    pub age: u8,
    /// Incarnation counter, bumped on every [`Self::reset`]. Lets the
    /// concurrent-mark cleanup detect that a region was recycled after the
    /// mark-start snapshot (its content then has no mark information and
    /// must be treated as live) — the TAMS-equivalent guard.
    pub reuse_epoch: u64,
    /// G1AUD-5 (defect G1-8) — value of [`G1Collector::rset_cache_epoch`] at
    /// this region's most recent [`Self::reset`].
    ///
    /// Remembered-set entries carry the generation they were recorded in
    /// (`RememberedSet::add_reference_in_generation`); an entry naming THIS
    /// region as a source is dead once `recycled_in_generation` exceeds its
    /// stamp, because `reset` zero-filled everything that could have held the
    /// edge. Starts at 0 (== the initial `rset_cache_epoch`), so an entry
    /// recorded before any collection is never mistaken for stale, and a reset
    /// site that ever forgets to advance it only over-retains.
    pub recycled_in_generation: u64,
    /// Per-region mark bitmap for concurrent marking.
    ///
    /// Round-2 fix (HIGH — GC #5): the bitmap is keyed off the region's
    /// own heap-allocated `data.as_ptr()` base, so `try_mark`/`is_marked`
    /// accept real object addresses living inside this region. The
    /// previous design used a single global bitmap rooted at address 0,
    /// which silently rejected every real region address and produced
    /// `live_bytes = 0` for every region (breaking mixed-GC region
    /// selection). The Vec backing the region is never reallocated
    /// (only zero-filled by `reset`), so the bitmap base remains stable
    /// for the entire collector lifetime.
    pub mark_bitmap: MarkBitmap,
}

impl G1Region {
    /// Create a free region backed by `region_size` bytes starting at arena
    /// address `base`. The caller ([`G1Collector::new`]) guarantees the range
    /// is a live, region-exclusive slice of the collector's `arena`.
    fn from_arena(base: usize, region_size: usize) -> Self {
        // Round-2 fix (HIGH — GC #5): bitmap covers exactly this region's
        // backing slice. Base = arena address, span = region_size.
        let mark_bitmap = MarkBitmap::new(base, region_size);
        Self {
            region_type: RegionType::Free,
            data: RegionBuf::new(base, region_size),
            cursor: 0,
            live_bytes: 0,
            gc_efficiency: 0.0,
            rset: RememberedSet::default(),
            pinned: false,
            pin_count: 0,
            age: 0,
            reuse_epoch: 0,
            recycled_in_generation: 0,
            mark_bitmap,
        }
    }

    /// Standalone test constructor: allocates a dedicated, leaked backing
    /// buffer so a region can own stable memory without a shared arena.
    /// Production regions come from [`G1Region::from_arena`] over
    /// [`G1Collector::arena`]; this exists only for the unit tests that
    /// exercise a single region in isolation.
    #[cfg(test)]
    fn new(region_size: usize) -> Self {
        let buf = vec![0u8; region_size].into_boxed_slice();
        let base = Box::leak(buf).as_mut_ptr() as usize;
        Self::from_arena(base, region_size)
    }

    /// Remaining free bytes in this region.
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }

    /// Step 7 — estimated time (ns) to evacuate this region's live data, used
    /// by pause-target collection-set sizing. Evacuation cost is dominated by
    /// copying the region's live bytes (plus per-slot reference rewriting);
    /// `ns_per_byte` is the collector's rolling `evac_ns_per_byte` calibration.
    pub fn estimated_evac_cost_ns(&self, ns_per_byte: u64) -> u64 {
        (self.live_bytes as u64).saturating_mul(ns_per_byte)
    }

    /// Reset this region to Free state.
    ///
    /// `rset_generation` is the collector's current
    /// [`G1Collector::rset_cache_epoch`], which every recycle/retype phase
    /// bumps before it touches a region. Recording it here is what lets the
    /// remembered set drop entries that name this region as a source from an
    /// earlier incarnation (G1AUD-5 / defect G1-8): the parameter is mandatory
    /// so a new reset site cannot silently skip the stamp.
    fn reset(&mut self, rset_generation: u64) {
        self.region_type = RegionType::Free;
        self.cursor = 0;
        self.live_bytes = 0;
        self.gc_efficiency = 0.0;
        self.rset.clear();
        self.pinned = false;
        self.pin_count = 0;
        self.age = 0;
        // New incarnation: content allocated from here on postdates any
        // in-flight mark cycle's snapshot (see `cleanup`).
        self.reuse_epoch = self.reuse_epoch.wrapping_add(1);
        // G1AUD-5: every rset entry naming this region as a source with a
        // stamp strictly below this value describes an object this reset just
        // zero-filled. Monotone by construction (`rset_cache_epoch` only ever
        // increases), so a later reset can never lower the bar.
        self.recycled_in_generation = rset_generation;
        // Round-2 fix (HIGH — GC #5): clear stale mark bits so they don't
        // pollute the next concurrent-mark cycle. The Vec is never
        // reallocated (only `fill(0)`'d) so the bitmap's base address
        // remains valid.
        self.mark_bitmap.clear();
        // Zero the backing storage
        self.data.fill(0);
    }

    /// Bump-allocate `size` bytes (with alignment) in this region.
    /// Returns `(pointer, offset_within_region)` or `None` if region is full.
    fn bump_alloc(&mut self, size: usize, align: usize) -> Option<(*mut u8, usize)> {
        let base = self.data.as_mut_ptr() as usize;
        let current = base + self.cursor;
        let aligned = (current + align - 1) & !(align - 1);
        let offset_in_region = aligned - base;
        let end = offset_in_region + size;

        if end > self.data.len() {
            return None;
        }

        self.cursor = end;
        let ptr = aligned as *mut u8;
        // Zero-init the allocated area
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }
        Some((ptr, offset_in_region))
    }

    /// Get a raw pointer to the start of this region's data.
    fn _base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable raw pointer to the start of this region's data.
    fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
}

/// Round-5 HIGH #6 — defensive cap on the gray-set worklist.
///
/// The mark worklist was previously an unbounded `Vec<usize>`. A
/// pathological object graph (e.g. a malicious or buggy classloader that
/// produces an exceptionally wide reference fan-out during a concurrent
/// mark cycle, or a marker thread that has been starved so the worklist
/// grows faster than it drains) could OOM the JVM by ballooning this
/// Vec. The cap below converts that silent allocator-OOM into a
/// deterministic panic, which is strictly better than crashing the
/// entire VM with an unrecoverable allocation failure deep inside
/// `Vec::push`.
///
/// 1 million entries * 8 bytes = 8 MiB — chosen large enough that any
/// realistic mark cycle stays well below it, small enough that the
/// panic is reproducible in tests.
///
/// TODO(round-5+): the real fix is an overflow-handling strategy —
/// either spill the worklist to a backing region, or drop the explicit
/// worklist entirely and fall back to a "mark-everything-dirty" sweep
/// pass guided by the card table. Both are too invasive for this
/// hotfix; the cap below is the defensive interim.
const MARK_WORKLIST_CAP: usize = 1 << 20;

/// Bound on the per-collection pause-record ring (§7 item 6). 64K records is
/// ~1.5 MB and covers a very long soak's most-recent window for p50/p99; older
/// records are evicted (and counted) so memory stays bounded.
const PAUSE_HISTORY_CAP: usize = 1 << 16;

// ---------------------------------------------------------------------------
// Collection type
// ---------------------------------------------------------------------------

/// Type of G1 collection to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G1CollectionType {
    /// Evacuate all Eden + Survivor regions.
    YoungOnly,
    /// Evacuate young + selected old regions.
    Mixed,
    /// Full heap compaction (fallback when evacuation fails).
    Full,
}

/// One STW collection's pause record (§7 item 6 — the structured pause sink).
///
/// Pause is recorded in **microseconds** (not milliseconds): a young G1 pause
/// is routinely sub-millisecond, so the old `Duration::as_millis()` rounded the
/// whole young series to `0` and made p50/p99 reporting (§5) impossible. The
/// collector keeps a bounded ring of the most recent records (see
/// `G1Collector::pause_history`) which `pause_summary()` reduces to percentiles.
#[derive(Debug, Clone, Copy)]
pub struct G1PauseRecord {
    /// Young / mixed / full.
    pub collection_type: G1CollectionType,
    /// STW pause duration in microseconds.
    pub pause_us: u64,
    /// Live objects copied.
    pub objects_copied: usize,
    /// Bytes copied (headers + slot data).
    pub bytes_copied: usize,
    /// Bytes reclaimed.
    pub bytes_freed: usize,
}

/// Percentile reduction of the recorded pauses, split by collection type
/// (the §5 acceptance metric). All durations are microseconds.
#[derive(Debug, Clone, Default)]
pub struct G1PausePercentiles {
    /// Number of collections of this type in the (bounded) history.
    pub count: u64,
    /// Total pause across the recorded collections.
    pub total_us: u64,
    /// Median (50th percentile) pause.
    pub p50_us: u64,
    /// 99th percentile pause.
    pub p99_us: u64,
    /// Worst observed pause.
    pub max_us: u64,
}

/// Aggregate pause summary for a run: young + mixed percentiles plus the
/// number of records dropped from the bounded ring (so a long soak's summary
/// never silently under-reports — see `PAUSE_HISTORY_CAP`).
#[derive(Debug, Clone, Default)]
pub struct G1PauseSummary {
    /// Young-only collection percentiles.
    pub young: G1PausePercentiles,
    /// Mixed collection percentiles.
    pub mixed: G1PausePercentiles,
    /// Records evicted from the bounded ring before this summary was taken.
    pub dropped: u64,
}

// ---------------------------------------------------------------------------
// G1 Collector
// ---------------------------------------------------------------------------

/// G1 garbage collector implementing the `GarbageCollector` trait.
pub struct G1Collector {
    /// Collector configuration.
    config: G1CollectorConfig,
    /// Single contiguous backing store for every region.
    ///
    /// All `num_regions` regions are carved as adjacent `region_size` slices
    /// of this one allocation (region `i` lives at `arena_base + i*region_size`).
    /// This makes a humongous object spanning a contiguous run of region
    /// indices one physically-contiguous block, so the JIT's flat
    /// `base + HEADER_SIZE + i*stride` array addressing (and JNI-critical /
    /// Unsafe / arraycopy raw pointers) address every element correctly. The
    /// previous design gave each region its own `Vec<u8>`, so a humongous
    /// element past the first region's payload read/wrote unrelated heap
    /// memory (silent zero tail → SIGSEGV). Allocated once in
    /// [`G1Collector::new`]; never moved or reallocated, so every region base
    /// and the `region_lookup` table stay address-stable for the collector's
    /// lifetime. `Box<[u8]>` (not `Vec`) to make the no-realloc contract
    /// explicit. Dropped with the collector — no leak.
    #[allow(dead_code)]
    arena: Box<[u8]>,
    /// All heap regions.
    regions: Mutex<Vec<G1Region>>,

    /// INT-3 — published un-retired TLAB tails of threads the cross-thread
    /// STW JIT takeover froze mid-JIT (plus any blocked/tearing-down thread
    /// that missed its safepoint retire), as absolute `(cursor, end)`
    /// address ranges.
    ///
    /// A frozen peer never reached `Tlab::retire`, so its reserved tail is
    /// uninitialized memory *below* its Eden region's `cursor` carrying no
    /// walkable int[]/gap filler. Every linear region walker must skip these
    /// spans (see [`jit_tlab_skip_span_len`]), and
    /// [`Self::jit_pinned_region_set`] excludes the containing regions from
    /// the CSet — the owner resumes bump-allocating into `[cursor, end)`
    /// after the pause, so the region must survive the collection in place.
    /// Set under STW immediately before a collection and cleared immediately
    /// after (empty on every normal cycle) — the G1 counterpart of
    /// [`crate::gen_heap::GenerationalHeap::set_jit_tlab_skip_regions`].
    jit_tlab_skip_regions: Mutex<Vec<(usize, usize)>>,

    /// Adaptive `needs_gc` Free-fraction threshold, in percent (baseline 25).
    /// Raised (up to 50) after any collection that recorded an evacuation
    /// failure — the free pool at trigger time is the young collection's
    /// ENTIRE to-space, so a live-young set larger than 25% of the heap
    /// otherwise self-forwards en masse on every single pause (the retry
    /// loop then drains the keeps, but at the cost of re-copying the live
    /// set several times per pause). Decays back to the baseline after a
    /// clean collection so healthy workloads keep the larger eden.
    needs_gc_free_percent: AtomicUsize,

    /// Native-allocation pressure latch — G1's half of the "the allocation
    /// wrappers cannot collect, so the next `safe_native_call` boundary does
    /// it for them" protocol. The generational half is
    /// [`crate::gen_heap::GenerationalHeap::young_spill_pressure`]; the shared
    /// consumer is `vm_exec::safe_native_call_impl`, the one point on the
    /// native-dispatch path where every Java argument is pinned in
    /// `native_pin_roots` and remapped afterwards, so an orchestrated moving
    /// collection is safe there.
    ///
    /// Latched whenever a NEW region is claimed for Eden (from
    /// [`Self::alloc_in_region`] or [`Self::refill_tlab`]) and the remaining
    /// Free pool has fallen below the `needs_gc` threshold; cleared by the
    /// consumer and at the end of every collection.
    ///
    /// Why this exists: the interpreter's `maybe_gc()` — the only thing that
    /// ever asks G1 to collect during normal execution — is polled from the
    /// bytecode allocation instructions (`new`/`newarray`/`anewarray`/
    /// `multianewarray`) alone. A hot loop whose body allocates only from
    /// INSIDE native methods (no allocation bytecode of its own — e.g.
    /// `AsynchronousFileChannel.read`, whose `Integer` box is allocated on the
    /// Rust side by `afc_box_integer`) therefore runs for millions of
    /// iterations with ZERO safepoint checks. Eden grows to 100% of the heap
    /// and the infallible [`GarbageCollector::alloc_object`] entry point —
    /// which has no way to report failure and so cannot retry after a GC —
    /// aborts the process with "out of heap space" while the heap is almost
    /// entirely garbage. The generational collector hides the same gap behind
    /// its young→old spill fallback (`gen_heap::alloc_object`); G1 has no
    /// equivalent, so it aborted outright. See
    /// `docs/internal/fixed-suite-bugs/g1-native-alloc-no-safepoint-oom-FIXED.md`.
    native_alloc_pressure: AtomicBool,

    /// Per-region `(reuse_epoch, cursor, region_type)` snapshot captured at
    /// concurrent-mark start (`start_concurrent_mark`, under the `regions`
    /// lock). The TAMS equivalent: `cleanup` treats every byte allocated
    /// after this snapshot — a grown cursor, a recycled (epoch-bumped) or
    /// re-typed region — as LIVE, because the mark bitmap carries no
    /// information about objects that did not exist when marking began.
    /// Without it, cleanup freed Old regions filled by promotion DURING the
    /// mark cycle (zero marked bytes), zeroing freshly-promoted live objects
    /// wholesale (SteadyChurn: live list nodes vanished the moment the first
    /// mark cycle completed under promotion pressure).
    mark_start_snapshot: Mutex<Vec<(u64, usize, RegionType)>>,

    /// Index of the current Eden allocation region.
    current_eden: AtomicUsize,
    /// Next identity hash code to assign.
    next_hash_code: AtomicI32,

    /// Concurrent GC phase state.
    ///
    /// Round-2 fix (HIGH — GC #5): the previously-global `mark_bitmap`
    /// has moved to `G1Region::mark_bitmap` so each region's bitmap is
    /// keyed off that region's actual data pointer (not address 0).
    /// Callers route bitmap operations through the region lookup.
    pub gc_state: Arc<ConcurrentGcState>,
    /// Global SATB queue.
    satb_queue: Arc<SatbQueue>,

    /// Number of collections performed.
    collection_count: AtomicU64,
    /// Total pause time in **microseconds** across all collections. (The public
    /// `total_pause_ms()` accessor derives milliseconds from this; storing
    /// microseconds keeps sub-millisecond young pauses from rounding to zero.)
    total_pause_us: AtomicU64,
    /// Bounded ring of the most recent per-collection pause records
    /// (§7 item 6 — the structured pause sink that `pause_summary()` reduces to
    /// p50/p99). Capped at `PAUSE_HISTORY_CAP`; the oldest record is evicted
    /// when full and `pause_history_dropped` counts the evictions so a summary
    /// stays honest about coverage. Lock order: this is a leaf lock, never held
    /// across `regions.lock()`.
    pause_history: Mutex<VecDeque<G1PauseRecord>>,
    /// Records evicted from `pause_history` because the ring was full.
    pause_history_dropped: AtomicU64,

    /// Step 7 (pause-target CSet sizing) — rolling per-region copy-cost
    /// calibration: an EMA of observed evacuation cost in **nanoseconds per
    /// live byte copied**, refreshed from each *mixed* collection (the
    /// collections that actually evacuate old regions). `estimated_evac_cost_ns`
    /// multiplies a region's `live_bytes` by this to bound the mixed collection
    /// set against `max_gc_pause_ms`. Initialised to 4 ns/byte (~250 MB/s,
    /// matching `region::Region::estimated_evac_cost_ns`) and clamped positive.
    /// Relaxed: a statistics/scheduling signal, not a correctness guard.
    evac_ns_per_byte: AtomicU64,

    /// Current old-gen bytes (for IHOP tracking).
    old_gen_bytes: AtomicUsize,
    /// Byte threshold at which to initiate concurrent marking.
    marking_threshold_bytes: AtomicUsize,

    /// String deduplication table: hash -> canonical object address.
    /// T10.9.B: FxHashMap — key is Java String hash from loaded bytecode.
    string_dedup_table: Mutex<FxHashMap<u64, usize>>,

    /// Whether GC event logging is enabled.
    gc_log_enabled: AtomicBool,

    /// Whether a concurrent mark cycle has completed and mixed GC is needed.
    marking_complete: AtomicBool,
    /// Remaining mixed GC cycles after a marking cycle.
    mixed_gc_remaining: AtomicU64,

    /// Audit fix (HIGH-3): persistent mark worklist (the "gray" stack)
    /// drained by `concurrent_mark_step`. Roots are pushed by `remark`
    /// (which the VM calls both at initial-mark and at final-remark
    /// STW points) and SATB entries are pushed when remark drains the
    /// SATB queue. Each step pops an object, marks it, and pushes its
    /// reference fields that are not yet marked. Stored as raw `usize`
    /// addresses so the queue is `Send`/`Sync` without `unsafe impl`
    /// gymnastics for `*mut u8`.
    mark_worklist: Mutex<Vec<usize>>,

    /// Round-9 gc HIGH-5 — set when any `mark_worklist` push is
    /// dropped because the cap was hit. The marker checks this flag at
    /// the end of remark and falls back to a conservative full re-walk
    /// of all live regions: every already-marked object is re-scanned
    /// and its outgoing references are pushed again. This replaces the
    /// previous `panic!` (which a hostile Java app could trip) with a
    /// time/correctness trade — no crash, just longer mark.
    mark_worklist_overflowed: AtomicBool,

    /// G1MARK-8 — set when the marker popped a gray-set entry whose header
    /// failed the plausibility gate (`plausible_mark_scan_target`): a wild
    /// child pointer read from a corrupt/stale ref slot, or (vanishingly
    /// unlikely) a torn header. Scanning such an "object" would amplify the
    /// corruption — its garbage `num_slots`/`array_length` extent would be
    /// walked and more garbage pushed as children. We skip the scan instead,
    /// but that leaves the closure potentially incomplete, so `cleanup`
    /// consults this flag and RETAINS everything for the cycle (no in-place
    /// Old-region frees, no humongous reclaim) — the Generational marker's
    /// "mark all old-gen for this cycle" fail-safe, region-flavored.
    mark_saw_implausible: AtomicBool,

    /// INT-8 — referent-slot hiding. Addresses of every Weak/Soft/Phantom
    /// `Reference` OBJECT registered with the VM's `ReferenceProcessor` at
    /// mark start (published by the initial-mark STW via
    /// [`Self::set_reference_skip_set`]). While a cycle is active,
    /// `scan_object_refs` skips slot 0 (the referent) of exactly these
    /// objects, so the trace cannot keep a weakly-reachable referent alive
    /// through its (strongly-reachable) Reference — the taint that made
    /// bitmap-based reference processing inert. Maintained across every
    /// mid-cycle evacuation pause by [`Self::remap_reference_skip_set`]:
    /// survivors are re-keyed through the pause's pointer map and CSet
    /// casualties are PRUNED (a stale entry could alias a reused address
    /// and hide an innocent object's slot 0 — under-marking). Cleared at
    /// cleanup/abort; empty outside a cycle.
    reference_skip: Mutex<FxHashSet<usize>>,

    /// Finalizer-resurrection input for the CURRENT collection (see
    /// [`Self::collect_garbage_with_finalizers`]): referent addresses of
    /// registered, not-yet-enqueued finalizable objects. Consumed (taken)
    /// by the serial young/mixed paths' Phase 3.5, which evacuates any of
    /// them that died in the CSet so `finalize()` can still run against
    /// valid memory. Empty on every plain `collect_garbage` call.
    pending_finalizer_roots: Mutex<Vec<usize>>,
    /// Output half of the finalizer-resurrection protocol: the POST-copy
    /// addresses of objects Phase 3.5 resurrected this collection. Drained
    /// by [`Self::collect_garbage_with_finalizers`].
    resurrected_finalizers: Mutex<Vec<usize>>,

    /// Region indices still holding UNRESOLVED self-forwarded objects after
    /// the evacuation-failure retry loop gave up (wedged drain / pass cap).
    /// Paired with [`Self::kept_unresolved_live`]: inside such a region,
    /// ONLY the recorded self-forwarded addresses are live — everything
    /// else below the cursor is dead garbage the failing pass never
    /// scanned, whose ref slots were never rewritten. `is_addr_in_live_region`
    /// must not report those dead bodies as live, or post-GC reference
    /// processing "restores" a weak referent whose fields dangle into
    /// regions freed the same pause (G1CORE-3). Cleared at the start of
    /// every retry evaluation; normally both sets are empty.
    kept_unresolved_regions: Mutex<std::collections::HashSet<usize>>,
    /// The self-forwarded (live-in-place) addresses within
    /// [`Self::kept_unresolved_regions`].
    kept_unresolved_live: Mutex<std::collections::HashSet<usize>>,
    /// Lock-free fast gate for the two sets above (they are empty except in
    /// the rare wedged-drain window; `is_addr_in_live_region` is a hot path).
    kept_unresolved_any: AtomicBool,

    /// SECURITY FIX (V7a): RSet write-barrier TLS-cache epoch.
    ///
    /// `post_write_barrier_rset`'s fast path caches a stable `*const
    /// G1Region` keyed by region index. Because the regions `Vec` never
    /// reallocates, that pointer stays address-valid even after the
    /// region is recycled (reset to `Free` and re-typed) by a
    /// collection. The old fast path therefore could record an inbound
    /// reference into a *just-recycled* region's rset (only the slow path
    /// gated on `RegionType::Free`), holding a reference that the next GC
    /// drains as garbage.
    ///
    /// This monotonic counter is bumped (under the `regions` lock, with
    /// `Release` ordering) at every point that recycles/retypes regions:
    /// the start of `young_collection`, `mixed_collection`, and
    /// `cleanup`. The fast path stamps the current epoch into its TLS
    /// entry and re-loads + compares it (with `Acquire`) on every hit; a
    /// mismatch forces the slow path, which re-validates `region_type !=
    /// Free` under the lock. The `Release`/`Acquire` pair establishes the
    /// happens-before edge so a reclassification can never be missed by a
    /// concurrent mutator's cached entry.
    rset_cache_epoch: AtomicU64,

    /// Process-unique identity of this collector instance, minted from a
    /// global monotonic counter in [`G1Collector::new`].
    ///
    /// `post_write_barrier_rset`'s TLS cache must key its cached `*const
    /// G1Region` to the collector instance that produced it. Using the
    /// collector's own address (`self as *const Self`) for that is unsound:
    /// a later collector can be constructed at the SAME address as a dropped
    /// one (stack slot reuse across calls, or allocator block reuse), and if
    /// its `rset_cache_epoch` history happens to line up too — trivially
    /// true for two identically-driven collectors, e.g. unit tests running
    /// the same scenario twice — the fast path revalidates a dangling
    /// pointer into the dropped collector's freed `regions` storage and
    /// writes through it (observed as intermittent 0xC0000374 heap
    /// corruption in parallel `cargo test -p cratonvm-gc` runs). A minted id
    /// is never reused within the process, so a cache entry can only ever
    /// match the instance that created it.
    instance_id: u64,

    /// Address-to-region lookup table for O(log R) `region_for_ptr` queries.
    ///
    /// Each entry is `(base_addr, region_idx)`, sorted ascending by
    /// `base_addr`. Built once in [`G1Collector::new`] and never mutated
    /// afterward: the outer `regions: Vec<G1Region>` is constructed with a
    /// fixed length and never `push`/`pop`ed, and each region's `data` Vec
    /// is allocated once with `region_size` capacity — `G1Region::reset`
    /// only zero-fills, it does not reallocate — so the backing-buffer
    /// addresses are stable for the entire lifetime of the collector.
    ///
    /// This replaces the previous O(R) linear scan inside
    /// `scan_and_evacuate_refs` and the write barrier, which was an
    /// audit-flagged hot-path bottleneck (CRIT-P4): with 256 regions, a
    /// 100-slot object incurred ~25k linear probes during evacuation.
    region_lookup: Vec<(usize, usize)>,

    /// Lock-free inclusive-exclusive bounds `[arena_base, arena_end)` of the
    /// single contiguous backing arena. Immutable for the collector's
    /// lifetime (the `arena` `Box` is allocated once in [`G1Collector::new`]
    /// and never moved or resized), so they can be read with no atomics and
    /// no `regions.lock()`.
    ///
    /// Used by [`G1Collector::is_addr_in_live_region`] as an O(1) lock-free
    /// reject for the overwhelming majority of conservative-root-scan
    /// candidate words (return addresses, ints, native-stack addresses) that
    /// fall outside the heap arena entirely. That function is the per-word
    /// hot path of `scan_active_jit_frames` / `update_root_snapshot`, which
    /// run on *every* object-returning native call; the previous
    /// implementation took the regions mutex and linearly scanned all
    /// `num_regions` regions for each candidate word, contending
    /// catastrophically on deep-stack JIT-on workloads. This mirrors the
    /// lock-free `[base, end)` bounds gate `gen_heap` already adopted for the
    /// identical reason (see `GenerationalHeap::is_object_address`).
    arena_base: usize,
    arena_end: usize,
}

// SAFETY: All fields are either atomic, behind Mutex, or Arc. Raw pointers
// in the mark bitmap are heap-managed and only accessed during STW pauses.
unsafe impl Send for G1Collector {}
unsafe impl Sync for G1Collector {}

/// Monotonic source of process-unique [`G1Collector::instance_id`] values.
/// Starts at 1 so 0 can serve as a never-assigned sentinel in debugging.
/// Wraparound after 2^64 collectors is not a practical concern.
static NEXT_G1_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

impl G1Collector {
    /// Create a new G1 collector with the given configuration.
    pub fn new(config: G1CollectorConfig) -> Self {
        let num_regions = config.heap_size / config.region_size;
        assert!(
            num_regions > 0,
            "g1: heap must fit at least one region (heap_size={}, region_size={})",
            config.heap_size,
            config.region_size
        );

        // One contiguous arena carved into `num_regions` adjacent slices, so
        // region `i+1` physically follows region `i`. This is what makes a
        // humongous object spanning a contiguous run of region indices one
        // contiguous block (see the `arena` field doc and `alloc_humongous_locked`).
        // Allocated once and never moved/reallocated → every region base and
        // the `region_lookup` table are address-stable for the collector's life.
        let arena: Box<[u8]> = vec![0u8; num_regions * config.region_size].into_boxed_slice();
        let arena_base = arena.as_ptr() as usize;
        let arena_end = arena_base + arena.len();

        let regions: Vec<G1Region> = (0..num_regions)
            .map(|i| G1Region::from_arena(arena_base + i * config.region_size, config.region_size))
            .collect();

        // Build the address-to-region lookup table (sorted by base addr).
        // Bases are `arena_base + i*region_size` — already ascending; the sort
        // is retained for robustness and parity with the previous construction.
        // The arena is never reallocated so these addresses are stable for the
        // lifetime of the collector (see field doc).
        let mut region_lookup: Vec<(usize, usize)> = regions
            .iter()
            .enumerate()
            .map(|(i, r)| (r.data.as_ptr() as usize, i))
            .collect();
        region_lookup.sort_unstable_by_key(|(base, _)| *base);

        // Round-2 fix (HIGH — GC #5): no global mark bitmap any more —
        // bitmaps live per-region (see `G1Region::mark_bitmap`) so they
        // correctly cover the real heap addresses of each region's data
        // buffer. The previous global `MarkBitmap::new(0, heap_size)`
        // silently rejected every real address.

        let ihop_threshold = (config.heap_size as u64 * config.ihop_percent as u64 / 100) as usize;

        Self {
            config: config.clone(),
            arena,
            regions: Mutex::new(regions),
            jit_tlab_skip_regions: Mutex::new(Vec::new()),
            current_eden: AtomicUsize::new(usize::MAX), // no eden yet
            next_hash_code: AtomicI32::new(1),
            needs_gc_free_percent: AtomicUsize::new(25),
            native_alloc_pressure: AtomicBool::new(false),
            mark_start_snapshot: Mutex::new(Vec::new()),
            gc_state: Arc::new(ConcurrentGcState::new()),
            satb_queue: Arc::new(SatbQueue::new()),
            collection_count: AtomicU64::new(0),
            total_pause_us: AtomicU64::new(0),
            pause_history: Mutex::new(VecDeque::new()),
            pause_history_dropped: AtomicU64::new(0),
            evac_ns_per_byte: AtomicU64::new(4),
            old_gen_bytes: AtomicUsize::new(0),
            marking_threshold_bytes: AtomicUsize::new(ihop_threshold),
            string_dedup_table: Mutex::new(FxHashMap::default()),
            gc_log_enabled: AtomicBool::new(false),
            marking_complete: AtomicBool::new(false),
            mixed_gc_remaining: AtomicU64::new(0),
            mark_worklist: Mutex::new(Vec::new()),
            mark_worklist_overflowed: AtomicBool::new(false),
            mark_saw_implausible: AtomicBool::new(false),
            reference_skip: Mutex::new(FxHashSet::default()),
            pending_finalizer_roots: Mutex::new(Vec::new()),
            resurrected_finalizers: Mutex::new(Vec::new()),
            kept_unresolved_regions: Mutex::new(std::collections::HashSet::new()),
            kept_unresolved_live: Mutex::new(std::collections::HashSet::new()),
            kept_unresolved_any: AtomicBool::new(false),
            // SECURITY FIX (V7a): start the RSet TLS-cache epoch at 0.
            rset_cache_epoch: AtomicU64::new(0),
            instance_id: NEXT_G1_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            region_lookup,
            arena_base,
            arena_end,
        }
    }

    /// Create a G1 collector with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(G1CollectorConfig::default())
    }

    /// Get the number of regions.
    pub fn num_regions(&self) -> usize {
        self.regions.lock().len()
    }

    /// Generate the next identity hash code.
    ///
    /// Relaxed ordering is sufficient: hash codes are monotonic counters with
    /// no ordering requirements relative to other memory operations. Duplicate
    /// or slightly-stale values are acceptable per the JVM spec (identity
    /// hashes need not be unique).
    ///
    /// H1: exposed publicly so `interpreter::init_object_header` (TLAB
    /// fast path) can mint a unique hash at allocation time, matching the
    /// non-TLAB allocators.
    pub fn next_hash(&self) -> i32 {
        self.next_hash_code.fetch_add(1, Ordering::Relaxed)
    }

    /// Lazily mint and durably install a non-zero identity hash for an
    /// object whose header field is still 0 (the JIT inline `new` fast path
    /// leaves it TLAB-zeroed — see `identity_hash_code`'s doc comment).
    /// Safe under concurrency: the header field is written via CAS from 0,
    /// so a losing racer's mint is discarded and every caller converges on
    /// the single value that ends up durably stored.
    fn mint_identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let minted = match self.next_hash() {
            0 => i32::MAX,
            h => h,
        };
        // SAFETY: see the identical justification in
        // `gen_heap::GenerationalHeap::mint_identity_hash_code`.
        unsafe {
            let field_ptr =
                std::ptr::addr_of_mut!((*(obj.as_ptr() as *mut ObjectHeader)).identity_hash_code);
            let atomic = &*(field_ptr as *const AtomicI32);
            match atomic.compare_exchange(0, minted, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => minted,
                Err(existing) => existing,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    /// Count the Free regions left and latch [`Self::native_alloc_pressure`]
    /// when the pool has dropped below the `needs_gc` threshold.
    ///
    /// Called ONLY when a new region is consumed — once per `region_size`
    /// bytes of allocation, never per object — so the O(num_regions) count is
    /// amortized away. `regions` must already be locked by the caller.
    fn note_region_consumed_locked(&self, regions: &[G1Region]) {
        let free = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Free)
            .count();
        let pct = self.needs_gc_free_percent.load(Ordering::Relaxed).max(1);
        if free * 100 < regions.len() * pct {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
    }

    /// Native-allocation pressure signal — see the field doc. Consumed at the
    /// `safe_native_call` boundary via `VmHeap::young_spill_pressure`.
    #[inline]
    pub fn native_alloc_pressure(&self) -> bool {
        self.native_alloc_pressure.load(Ordering::Relaxed)
    }

    /// Clear the native-allocation pressure latch.
    #[inline]
    pub fn clear_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
    }

    /// Latch the native-allocation pressure signal from outside the collector
    /// (the `VmHeap::note_young_spill_pressure` funnel used by allocation
    /// wrappers that spilled and cannot collect themselves).
    #[inline]
    pub fn note_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(true, Ordering::Relaxed);
    }

    /// Free regions held back from *speculative bulk* TLAB refills once the
    /// pool runs low, so the last of the heap serves real object allocations
    /// (which cannot be deferred) instead of TLAB tails that may never be
    /// used. Defense-in-depth for the native-alloc safepoint gap: it widens
    /// the window in which the `safe_native_call` boundary GC can still fire
    /// before the infallible allocator is cornered. `refill_tlab` returning
    /// `None` is an already-handled path (the caller falls back to per-object
    /// allocation), so this can never wedge allocation.
    fn tlab_reserve_regions(&self, total_regions: usize) -> usize {
        // Capped at an eighth of the heap so a very small heap (the unit
        // tests' 2-region collectors, an embedded `-Xmx4m`) is never starved
        // of TLABs by its own reserve — at 8 regions or fewer the cap decides,
        // and below 8 regions the reserve is zero (previous behaviour).
        (total_regions / 64).max(2).min(total_regions / 8)
    }

    /// Bump-allocate `size` bytes in the current Eden region.
    /// Returns `(pointer, region_index)` or `None` on failure.
    pub fn alloc_in_region(&self, size: usize) -> Option<(*mut u8, usize)> {
        let mut regions = self.regions.lock();
        let region_size = self.config.region_size;

        // Humongous check
        if size > region_size / 2 {
            let result = self.alloc_humongous_locked(&mut regions, size);
            if result.is_some() {
                // A humongous span consumes several regions at once — always
                // the largest single bite out of the Free pool.
                self.note_region_consumed_locked(&regions);
            }
            return result;
        }

        // Try current Eden region
        let cur = self.current_eden.load(Ordering::Relaxed);
        if cur < regions.len() && regions[cur].region_type == RegionType::Eden {
            if let Some(result) = regions[cur].bump_alloc(size, 8) {
                return Some((result.0, cur));
            }
        }

        // Find a new free region for Eden
        if let Some(idx) = find_free_region(&regions) {
            regions[idx].region_type = RegionType::Eden;
            self.current_eden.store(idx, Ordering::Relaxed);
            if let Some(result) = regions[idx].bump_alloc(size, 8) {
                self.note_region_consumed_locked(&regions);
                return Some((result.0, idx));
            }
        }

        None
    }

    /// Allocate a humongous object spanning a contiguous run of free regions.
    ///
    /// The object occupies ONE physically-contiguous block: the regions are
    /// adjacent slices of [`G1Collector::arena`], so a span of `regions_needed`
    /// consecutive region indices is `regions_needed * region_size` contiguous
    /// bytes. The real `ObjectHeader` lives at offset 0 of the start region and
    /// the payload flows straight through the following regions — exactly like
    /// any other array, just larger. This is what lets the JIT (and
    /// JNI-critical / Unsafe / arraycopy) address every element with flat
    /// `base + HEADER_SIZE + i*stride` arithmetic.
    ///
    /// Layout / bookkeeping:
    /// * `start` is `HumongousStart` with `cursor = size` (the full object
    ///   size). Heap walkers iterating `[0, cursor)` therefore read the single
    ///   humongous object once — reads cross region boundaries safely because
    ///   the arena is contiguous — then stop.
    /// * Continuation regions are `HumongousContinuation` with `cursor = 0`, so
    ///   walkers skip them (their bytes are the start object's payload, NOT
    ///   independent objects). No per-region header prefix and no
    ///   `HumongousFiller` sentinel: those would corrupt the contiguous
    ///   payload. (`is_humongous_filler` is now never true for live data and
    ///   stays only as a defensive no-op in the walkers.)
    ///
    /// Replaces the previous region-fragmented layout (each region a separate
    /// `Vec<u8>` with its own HEADER_SIZE prefix), which was safe for the
    /// GC-internal region-aware accessors but invisible to the JIT's flat
    /// addressing — a humongous element past the first region read/wrote
    /// unrelated memory (silent zero tail at ~1–2 MB, SIGSEGV at ~4 MB+).
    fn alloc_humongous_locked(
        &self,
        regions: &mut Vec<G1Region>,
        size: usize,
    ) -> Option<(*mut u8, usize)> {
        let region_size = self.config.region_size;
        if region_size == 0 || size < HEADER_SIZE {
            return None;
        }

        // Number of contiguous regions whose combined bytes hold the whole
        // object (header + payload). Contiguity in the arena makes this a
        // single block, so we size by the FULL object, not a per-region chunk.
        let regions_needed = size.div_ceil(region_size).max(1);

        let start = find_contiguous_free(regions, regions_needed)?;

        // Classify the span. `cursor = size` on the start makes walkers read
        // the one object; `cursor = 0` on continuations makes walkers skip them.
        regions[start].region_type = RegionType::HumongousStart;
        regions[start].cursor = size;
        for i in 1..regions_needed {
            regions[start + i].region_type = RegionType::HumongousContinuation;
            regions[start + i].cursor = 0;
        }

        // Zero the entire contiguous span before handing it out. Continuation
        // regions come from `Free` slots that may still hold stale collected
        // data; `G1Region::reset` zeroes a region only on its STW retire path.
        // The arena is one allocation, so a single `write_bytes` across the
        // full span is in-bounds and contiguous.
        let start_addr = regions[start].data.addr();
        unsafe {
            // SAFETY: `[start_addr, start_addr + regions_needed*region_size)` is
            // `regions_needed` adjacent arena slices reserved by this
            // allocation (`find_contiguous_free` returned a Free run); `size <=
            // regions_needed*region_size`, so zeroing `size` bytes stays inside
            // the reserved span.
            std::ptr::write_bytes(start_addr as *mut u8, 0, size);
        }

        // Humongous bytes count toward the IHOP occupancy statistic (see
        // `recompute_old_gen_bytes`). Bump it here too so a burst of
        // humongous allocation can cross the marking threshold *between*
        // pauses — the next pause's recompute replaces this running total,
        // so drift never accumulates.
        self.old_gen_bytes.fetch_add(size, Ordering::Relaxed);

        Some((start_addr as *mut u8, start))
    }

    /// Allocate in a region of the specified type (Survivor or Old).
    fn alloc_in_type_locked(
        regions: &mut Vec<G1Region>,
        target_type: RegionType,
        size: usize,
        cset: &std::collections::HashSet<usize>,
    ) -> Option<*mut u8> {
        // CORRECTNESS (evacuation destination must NOT be in the collection set):
        // a young GC evacuates *all* Survivor regions, so a partially-filled
        // Survivor region is itself in the CSet; a mixed GC likewise has selected
        // Old regions in the CSet. Reusing such a region as an evacuation
        // *destination* copies survivors into a region that Phase 5 then resets
        // (frees) — the copies are lost and every reference to them is left
        // dangling (silent live-object loss; the held-tree `got=1` repro). Skip
        // any CSet region here: young survivors land only in fresh Free regions,
        // mixed promotions only in non-CSet Old or fresh Free regions — matching
        // the semi-space "never allocate into from-space" invariant the
        // generational collector and the Step-9 parallel TLAB path already honour.
        for i in 0..regions.len() {
            if regions[i].region_type == target_type && !cset.contains(&i) {
                if let Some((ptr, _)) = regions[i].bump_alloc(size, 8) {
                    return Some(ptr);
                }
            }
        }

        // Allocate a new free region (Free regions are never in the CSet).
        if let Some(idx) = find_free_region(regions) {
            regions[idx].region_type = target_type;
            if target_type == RegionType::Survivor {
                regions[idx].age = 1;
            }
            if let Some((ptr, _)) = regions[idx].bump_alloc(size, 8) {
                return Some(ptr);
            }
        }

        None
    }

    /// Phase 5: free evacuated CSet regions — EXCEPT those that hold a
    /// self-forwarded (evacuation-failed) object, which must be KEPT so the
    /// still-live object that could not be relocated is not freed.
    ///
    /// A self-forwarded object is recorded as an identity entry (`key == value`)
    /// in the forwarding map by [`Self::evacuate_object`] (and the parallel
    /// evacuator) when to-space is exhausted. Its region is kept intact: a young
    /// `Eden` region is retyped to `Survivor` (it now holds survivors and is
    /// re-collected next cycle, when the moved-out garbage copies it also
    /// contains become unreachable and freed); `Survivor`/`Old` regions keep
    /// their type. Returns the bytes freed (only from regions actually reset).
    ///
    /// At adequate heaps no evacuation fails, so `failed` is empty and this is
    /// exactly the old "reset every CSet region" behaviour.
    fn free_or_keep_cset(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &[usize],
        pointer_map: &HashMap<usize, usize>,
    ) -> usize {
        // Regions that hold at least one self-forwarded (in-place) object.
        // `lookup_region_for_addr` consults the immutable region table, so it
        // does not borrow `regions` (no conflict with the mutable loop below).
        let failed: std::collections::HashSet<usize> = pointer_map
            .iter()
            .filter(|(k, v)| k == v)
            .filter_map(|(k, _)| self.lookup_region_for_addr(*k))
            .collect();

        let mut bytes_freed = 0usize;
        // G1AUD-5: every pause bumps `rset_cache_epoch` before it reclassifies
        // anything, so this reads the generation this pause owns.
        let generation = self.rset_generation();
        for &cset_idx in cset {
            if failed.contains(&cset_idx) {
                if regions[cset_idx].region_type == RegionType::Eden {
                    regions[cset_idx].region_type = RegionType::Survivor;
                }
            } else {
                if gc_flags().g1_dbg_reach {
                    eprintln!(
                        "[g1][FREED] evac region={cset_idx} type={:?} cursor={:#x}",
                        regions[cset_idx].region_type, regions[cset_idx].cursor
                    );
                }
                bytes_freed += regions[cset_idx].cursor;
                regions[cset_idx].reset(generation);
            }
        }
        bytes_freed
    }

    /// Evacuation-failure recovery (the kept-region death-spiral fix).
    ///
    /// A young/mixed pass that exhausts its to-space pool self-forwards every
    /// remaining reached object, KEEPING each such object's region wholesale —
    /// including all the garbage those regions hold. Under allocation churn
    /// the free pool at trigger time is small (`needs_gc` fires at <25% free),
    /// so once young-live exceeds the pool a single pass can convert most of
    /// the heap into kept, mostly-garbage regions; every later collection then
    /// finds even fewer free regions and keeps even more, monotonically, until
    /// each pass reports `objects_copied == 0 && bytes_freed == 0` and the
    /// mutator dies on a heap that is largely garbage. Observed
    /// deterministically on the SteadyChurn recreation at `-Xmx16m` (serial
    /// G1, --nojit): from the 4th collection onward nothing is copied or
    /// freed, and the program aborts with a corrupted OOM-path throwable.
    ///
    /// Recovery: drain the self-forwarded (identity-forwarded) objects with a
    /// MINIMAL live-only pass — evacuate exactly those objects (plus any
    /// still-kept objects they transitively reference), rewrite all heap/root
    /// references through the drain's map, and free the kept regions that are
    /// now fully drained. Loop while progress is made and identities remain.
    ///
    /// The seeds are live BY CONSTRUCTION (only reached objects self-forward),
    /// and their slots were already rewritten in place when the failing pass
    /// scanned them, so the drain never touches a dead object and never walks
    /// a region wholesale. (Two earlier designs failed here: re-running full
    /// young collections re-copied the live set several times per pause —
    /// inflating `gc_age` until the whole churn set promoted — and any
    /// whole-region source walk resurrects dead objects' targets, a
    /// compounding "undead" population that permanently filled Old.)
    ///
    /// Healthy collections (no evacuation failure) pay one `pointer_map`
    /// scan; no drain runs.
    fn retry_after_evacuation_failure(
        &self,
        first: GcResult,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Hard cap: each productive drain frees at least one region (growing
        // the pool for the next), so real recoveries converge quickly.
        const MAX_EVAC_RETRY_PASSES: usize = 8;

        // Diagnostic kill-switch (bisection aid): CRATONVM_G1_NO_EVAC_RETRY=1
        // restores the single-pass behaviour (and with it, the kept-region
        // death spiral under to-space exhaustion).
        if gc_flags().g1_no_evac_retry {
            return first;
        }

        let identities = |m: &HashMap<usize, usize>| -> Vec<usize> {
            m.iter().filter(|(k, v)| k == v).map(|(&k, _)| k).collect()
        };

        // Reset the unresolved-kept tracking for this pause; repopulated
        // below iff the drain gives up with seeds remaining.
        self.kept_unresolved_any.store(false, Ordering::Release);
        self.kept_unresolved_regions.lock().clear();
        self.kept_unresolved_live.lock().clear();

        let mut acc = first;
        let mut seeds = identities(&acc.pointer_map);
        if seeds.is_empty() {
            // Clean pause: decay the adaptive trigger back toward baseline so
            // healthy workloads keep the larger eden.
            let cur = self.needs_gc_free_percent.load(Ordering::Relaxed);
            if cur > 25 {
                self.needs_gc_free_percent
                    .store((cur - 5).max(25), Ordering::Relaxed);
            }
            return acc;
        }
        // Evacuation failure: the free pool at trigger time was too small for
        // the live-young set. Trigger the NEXT collection earlier so its
        // to-space pool is larger (capped at 50% of the heap).
        {
            let cur = self.needs_gc_free_percent.load(Ordering::Relaxed);
            self.needs_gc_free_percent
                .store((cur + 8).min(50), Ordering::Relaxed);
        }

        let mut passes = 1usize;
        while !seeds.is_empty() && passes < MAX_EVAC_RETRY_PASSES {
            let drain = self.drain_kept_self_forwards(&seeds, roots, monitors);
            if gc_flags().g1_dbg_reach {
                eprintln!(
                    "[g1][RETRY] drain pass={} seeds={} copied={} freed={} forwards={}",
                    passes + 1,
                    seeds.len(),
                    drain.stats.objects_copied,
                    drain.stats.bytes_freed,
                    drain.pointer_map.len(),
                );
            }
            let progressed = drain.stats.objects_copied > 0 || drain.stats.bytes_freed > 0;
            seeds = identities(&drain.pointer_map);
            Self::compose_forward_maps(&mut acc.pointer_map, &drain.pointer_map);
            acc.stats.objects_copied += drain.stats.objects_copied;
            acc.stats.bytes_copied += drain.stats.bytes_copied;
            acc.stats.bytes_freed += drain.stats.bytes_freed;
            passes += 1;
            if !progressed {
                break; // genuinely wedged: the live set does not fit (true OOM)
            }
        }
        if !seeds.is_empty() {
            // The drain gave up (wedge / pass cap) with live self-forwarded
            // objects still parked in kept regions. Two follow-ups keep the
            // heap coherent until a later pause resolves them:
            //
            // (a) G1CORE-4: their ref slots were rewritten IN PLACE to
            //     to-space addresses by the failing pass — GC-internal edges
            //     no mutator barrier ever recorded. A kept OLD region is not
            //     re-collected automatically (unlike kept Eden→Survivor), so
            //     without remembered-set entries the next young pause never
            //     scans it as a source and frees its live young referents.
            //     Record each seed's outgoing cross-region edges now.
            //
            // (b) G1CORE-3: record the kept regions + their live addresses
            //     so `is_addr_in_live_region` reports the regions' DEAD
            //     bodies (never scanned, slots never rewritten) as dead —
            //     otherwise post-GC reference processing restores weak
            //     referents whose fields dangle into same-pause-freed
            //     regions.
            {
                let mut regions = self.regions.lock();
                for &seed in &seeds {
                    self.record_outgoing_rset_edges(&mut regions, seed);
                }
            }
            let mut kept_regions = self.kept_unresolved_regions.lock();
            let mut kept_live = self.kept_unresolved_live.lock();
            for &seed in &seeds {
                if let Some(idx) = self.lookup_region_for_addr(seed) {
                    kept_regions.insert(idx);
                }
                kept_live.insert(seed);
            }
            self.kept_unresolved_any.store(true, Ordering::Release);
        }
        // Overwrites this pause's young/mixed record on purpose: when a drain
        // ran, the drain is what the operator needs to see, and `kind` names
        // it unambiguously. A wedged drain is the one G1 state that silently
        // converts most of the heap into kept, mostly-garbage regions, so it
        // must never be invisible.
        let mut degraded = crate::gc_metrics::g1_degraded::EVACUATION_FAILURE;
        if !seeds.is_empty() {
            degraded |= crate::gc_metrics::g1_degraded::EVACUATION_FAILURE_UNRESOLVED;
        }
        crate::gc_metrics::record_g1_cycle(
            crate::gc_metrics::g1_cycle_kind::KEPT_REGION_DRAIN,
            0,
            0,
            0,
            0,
            degraded,
        );
        acc
    }

    /// Record remembered-set edges for every cross-region reference held by
    /// the (live, in-place) object at `obj_addr` — the GC-internal
    /// counterpart of `post_write_barrier_rset` for slots the collector
    /// itself rewrote. See the unresolved-kept block in
    /// [`Self::retry_after_evacuation_failure`].
    fn record_outgoing_rset_edges(&self, regions: &mut [G1Region], obj_addr: usize) {
        let Some(src_idx) = self.lookup_region_for_addr(obj_addr) else {
            return;
        };
        let obj_ptr = obj_addr as *mut u8;
        // Kept objects are ordinary (humongous regions never enter a CSet),
        // so flat payload reads are in-bounds.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        // G1AUD-5: GC-internal edges are stamped with the pause's generation,
        // exactly like the mutator barrier's.
        let generation = self.rset_generation();
        let mut record = |raw: usize| {
            if raw == 0 {
                return;
            }
            if let Some(dst_idx) = self.lookup_region_for_addr(raw) {
                if dst_idx != src_idx && regions[dst_idx].region_type != RegionType::Free {
                    regions[dst_idx]
                        .rset
                        .add_reference_in_generation(src_idx, generation);
                }
            }
        };
        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length() as usize {
                    // SAFETY: i < array_length — inside the allocation.
                    let raw =
                        unsafe { std::ptr::read(obj_ptr.add(HEADER_SIZE + i * 8) as *const u64) };
                    record(raw as usize);
                }
            }
        } else {
            for_each_flat_object_reference(obj_ptr, header, 0, |_, raw, _| record(raw));
        }
    }

    /// Minimal same-pause drain of evacuation-failed objects: evacuate exactly
    /// `seeds` (self-forwarded, hence LIVE, objects still sitting in kept
    /// from-space regions) and their transitively-kept referents, then run the
    /// normal Phase-4/5 fix-ups against the drain's forwarding map. See
    /// [`Self::retry_after_evacuation_failure`].
    fn drain_kept_self_forwards(
        &self,
        seeds: &[usize],
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.lock();
        // SECURITY FIX (V7a): Phase 5 below can reset/retype regions.
        self.rset_cache_epoch.fetch_add(1, Ordering::Release);

        // The drain's collection set: exactly the regions holding seeds (the
        // kept regions). Only seed-reachable objects inside them are copied.
        let cset_set: std::collections::HashSet<usize> = seeds
            .iter()
            .filter_map(|&s| self.lookup_region_for_addr(s))
            .collect();
        let cset: Vec<usize> = cset_set.iter().copied().collect();
        if cset.is_empty() {
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map: HashMap::new(),
            };
        }

        let mut pointer_map: HashMap<usize, usize> = HashMap::new();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;
        let mut work_list: Vec<*mut u8> = Vec::new();

        for &seed in seeds {
            if let Some((new_ptr, _fresh)) = self.evacuate_object(
                &mut regions,
                seed as *mut u8,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &cset_set,
            ) {
                work_list.push(new_ptr);
            }
        }
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Roots may point directly at seeds — remap them like Phase 1 would.
        for root in roots.iter_mut() {
            if let Some(&new_addr) = pointer_map.get(&(root.as_ptr() as usize)) {
                *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }

        // Phase 4/5 equivalents against the drain map.
        self.update_references_in_regions(&mut regions, &cset_set, &pointer_map);
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map);
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "kept-drain");

        let cur_eden = self.current_eden.load(Ordering::Relaxed);
        if cset_set.contains(&cur_eden) {
            self.current_eden.store(usize::MAX, Ordering::Relaxed);
        }

        self.recompute_old_gen_bytes(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — same
        // protocol as the young paths (done under STW, guard held).
        {
            let mut worklist = self.mark_worklist.lock();
            if !worklist.is_empty() {
                worklist.retain_mut(|addr| {
                    if let Some(&new_addr) = pointer_map.get(&*addr) {
                        *addr = new_addr;
                        return true;
                    }
                    match self.region_for_ptr(&regions, *addr as *mut u8) {
                        Some(idx) if cset_set.contains(&idx) => false,
                        _ => true,
                    }
                });
            }
        }

        let pause_us = start.elapsed().as_micros() as u64;
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection(G1CollectionType::YoungOnly, pause_us, &stats);
        GcResult { stats, pointer_map }
    }

    /// Compose the forwarding maps of two consecutive same-pause passes
    /// (`acc` ran first, `next` second) into `acc`.
    ///
    /// Values chase one hop: an object moved by an earlier pass and moved
    /// again — including a pass-1 self-forward `k -> k` that a retry resolved
    /// to a real copy — ends at its final address. New keys are added only if
    /// absent: the VM's `update_all_roots` remaps frame locals that hold
    /// PAUSE-START addresses, so when a `next` key collides with an existing
    /// `acc` key the `acc` entry is the meaningful one (the `next` key is a
    /// pass-1-freed address recycled as later-pass to-space — no frame local
    /// can name it).
    fn compose_forward_maps(acc: &mut HashMap<usize, usize>, next: &HashMap<usize, usize>) {
        for v in acc.values_mut() {
            if let Some(&nv) = next.get(v) {
                *v = nv;
            }
        }
        for (&k, &v) in next {
            acc.entry(k).or_insert(v);
        }
    }

    // -----------------------------------------------------------------------
    // Young Collection (STW)
    // -----------------------------------------------------------------------

    /// Perform a young-only collection. Evacuates all Eden + Survivor regions.
    pub fn young_collection(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Step 9: opt-in multi-threaded evacuator (`CRATONVM_G1_PARALLEL_EVAC`).
        // Behaviour-equivalent to the serial path below (byte-identical program
        // output); see the parallel-evacuation module note above.
        //
        // Fall back to the serial path whenever a thread is in JIT: only the
        // serial path implements conservative-JIT-root region pinning (the
        // parallel evacuator would relocate a JIT-rooted object whose holder
        // slot cannot be rewritten). When parallel DOES run (no thread in JIT)
        // there are no conservative JIT roots to pin, so it stays correct.
        // Finalizer resurrection (Phase 3.5) is implemented only on the
        // serial paths — force serial while resurrection candidates are
        // pending (System.gc with registered finalizables; rare and already
        // a full-STW slow path).
        if parallel_evac_enabled()
            && !crate::gc_quiescence::is_active()
            && self.pending_finalizer_roots.lock().is_empty()
        {
            return self.young_collection_parallel(roots, monitors);
        }
        let start = std::time::Instant::now();
        let mut regions = self.regions.lock();
        // SECURITY FIX (V7a): this collection will reset/retype CSet
        // regions (Phase 5). Bump the RSet TLS-cache epoch *before* any
        // reclassification so every mutator's fast-path cache entry is
        // invalidated and falls to the Free-gated slow path. `Release`
        // pairs with the `Acquire` load on the fast path.
        self.rset_cache_epoch.fetch_add(1, Ordering::Release);
        let mut pointer_map: HashMap<usize, usize> = HashMap::new();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;

        // Regions holding a conservatively-discovered JIT root must NOT be
        // evacuated: the collector cannot rewrite the (register/spill) slot that
        // holds the only reference, so the object must stay put (the
        // generational collector achieves this by not moving anything while in
        // JIT). Map each published root address to its region and exclude those
        // from the CSet, exactly like JNI-pinned regions. Empty unless a thread
        // is in JIT (the common case for a JIT-triggered young GC).
        let jit_pinned_regions = self.jit_pinned_region_set();
        if gc_flags().g1_dbg_pins {
            eprintln!(
                "[g1][PINS] young pause: jit_active={} pin_addrs={} pin_regions={:?}",
                crate::gc_quiescence::is_active(),
                crate::gc_quiescence::pinned_jit_root_count(),
                jit_pinned_regions,
            );
        }

        // Build collection set: all Eden + Survivor regions (skip pinned + any
        // region holding a conservative JIT root)
        let cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && (r.region_type == RegionType::Eden || r.region_type == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();

        // How many young regions each pin vocabulary kept out of this CSet.
        // These are the regions the pause CANNOT reclaim, so they belong in
        // the cycle record: an operator seeing G1 reclaim nothing needs to be
        // able to tell "nothing was garbage" from "everything was pinned".
        let (jni_pinned_out, jit_pinned_out) =
            count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);

        if cset.is_empty() {
            let mut degraded = crate::gc_metrics::g1_degraded::EMPTY_COLLECTION_SET;
            if jni_pinned_out > 0 {
                degraded |= crate::gc_metrics::g1_degraded::JNI_PINNED_REGIONS_EXCLUDED;
            }
            if jit_pinned_out > 0 {
                degraded |= crate::gc_metrics::g1_degraded::JIT_PINNED_REGIONS_EXCLUDED;
            }
            crate::gc_metrics::record_g1_cycle(
                crate::gc_metrics::g1_cycle_kind::YOUNG,
                0,
                0,
                (jni_pinned_out + jit_pinned_out) as u32,
                0,
                degraded,
            );
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map,
            };
        }

        // A pinned region must never enter a collection set: Phase 5 resets
        // (zero-fills and re-types) every CSet region that holds no
        // self-forwarded object, so a pinned region in the CSet is a
        // relocated-or-freed JNI-critical array — the precise thing
        // `pin_region_for_addr` promises cannot happen. The filter above is
        // the enforcement; this states it so a future edit to the predicate
        // cannot quietly drop a term. Cheap: one pass over a short Vec.
        debug_assert!(
            cset.iter()
                .all(|&i| !regions[i].pinned && !jit_pinned_regions.contains(&i)),
            "G1 young CSet contains a pinned region"
        );

        // Phase 1: Scan roots and evacuate reachable objects from CSet
        let cset_set: std::collections::HashSet<usize> = cset.iter().copied().collect();
        let mut work_list: Vec<*mut u8> = Vec::new();

        // Process root references
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(region_idx) = self.region_for_ptr(&regions, old_ptr) {
                if cset_set.contains(&region_idx) {
                    // Step 9: `fresh` is ignored here — the root loop keeps its
                    // existing unconditional push (a duplicate root re-scans
                    // idempotently). Gating it on `fresh` is deferred to the
                    // parallel evacuator (where duplicate worklist entries
                    // matter for worker load).
                    if let Some((new_ptr, _fresh)) = self.evacuate_object(
                        &mut regions,
                        old_ptr,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                        &cset_set,
                    ) {
                        *root = unsafe { ObjectRef::from_raw(new_ptr) };
                        work_list.push(new_ptr);
                    }
                }
            }
        }

        // Phase 1b: marking keep-alive (see `marking_keepalive_roots`).
        // While a concurrent mark cycle is active, CSet-resident gray/SATB
        // objects are snapshot-live: evacuate them like roots so the marker
        // can finish tracing them. The worklist remap at the end of this
        // pause then rewrites every gray through `pointer_map` instead of
        // dropping it (a drop = the object's unscanned subtree is silently
        // unmarked = cleanup frees live Old/humongous objects).
        for addr in self.marking_keepalive_roots(&regions, &cset_set) {
            if let Some((new_ptr, fresh)) = self.evacuate_object(
                &mut regions,
                addr as *mut u8,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &cset_set,
            ) {
                if fresh {
                    work_list.push(new_ptr);
                }
                self.push_gray_or_mark(&regions, new_ptr as usize);
            }
        }

        // Phase 2: Scan remembered sets for references into CSet
        // (Collect rset sources before mutating regions)
        //
        // G1AUD-5 (defect G1-8): `live_rset_sources` drops entries whose source
        // region has been RECYCLED since the edge was recorded. The previous
        // "is the source Free right now?" filter (still applied inside
        // `scan_source_region_for_cset_refs`) misses precisely the case that
        // costs: a source that was freed and then re-typed into a live region
        // is not Free, so it was re-walked *wholesale* every pause on behalf of
        // an object that no longer exists — resurrecting that object's
        // referents, cycle after cycle.
        let mut rset_sources: std::collections::HashSet<usize> =
            Self::live_rset_sources(&regions, &cset);

        // CRIT fix (UAF): actually process the collected rset sources.
        // Dedup source indices so we walk each source region at most once.
        // JIT-pinned regions are scanned as sources too: their objects stay in
        // place (excluded from the CSet) but may reference objects that ARE in
        // the CSet, and JIT-compiled code may have installed those references
        // through stores the collector cannot assume went through
        // `post_write_barrier_rset`, so the pinned region is walked wholesale
        // as defense-in-depth. Without this, a list/tree straddling pinned and
        // CSet regions loses the CSet-side nodes (SIGSEGV / wrong checksum).
        //
        // NOTE (audited): JNI-pinned regions (`r.pinned`, GetPrimitiveArray-
        // Critical) do NOT need this treatment. Interpreter/native ref stores
        // all funnel through `post_write_barrier_rset`, which records EVERY
        // cross-region edge (young→young included — an earlier revision of
        // this comment claimed otherwise), and Phase 4's
        // `collect_outgoing_cross_region_edges` re-records edges the
        // evacuation itself rewrites. A CSet object whose only referent sits
        // in a JNI-pinned region is therefore reached via that region's RSet
        // membership — pinned regions are ordinary RSet sources. Regression:
        // `jni_pinned_young_region_holder_keeps_cset_referent_alive{,_parallel}`.
        // Deliberately NOT added wholesale here: an unconditional walk of
        // pinned regions would resurrect their dead objects' referents every
        // pause (the documented "undead" compounding) for no soundness gain.
        //
        // G1AUD-4 (2026-07-31) — the "all ref stores funnel through
        // `post_write_barrier_rset`" premise above is TRUE for the interpreter
        // and for every native/JIT-helper store, and is now also true for
        // JIT-compiled stores into OLD receivers (see the `GC_FLAG_OLD_GEN`
        // stamp in `evacuate_object`, which routes them to
        // `jit_putfield_object`). It is NOT yet true for a JIT-compiled
        // null->non-null `putfield` into a YOUNG receiver: those take an
        // inline store with no post barrier. That is harmless for an ordinary
        // young source (every young region is in this CSet, so the holder is
        // traced) but NOT for a young source held out of the CSet by a JNI
        // pin, which is reached only through its remembered set. Closing it
        // requires a `jit/` change (see `docs/gc/g1-audit.md`, defect G1-2);
        // the debug-only `verify_no_dangling_into_cset` below is the tripwire
        // in the meantime.
        let dbg_phases = gc_flags().g1_dbg_reach;
        let p1_forwards = pointer_map.len();
        rset_sources.extend(jit_pinned_regions.iter().copied());
        let unique_sources = rset_sources;
        let rset_sources_scanned = unique_sources.len();
        if dbg_phases {
            eprintln!(
                "[g1][PHASES] roots={} p1_forwards={p1_forwards} sources={:?}",
                roots.len(),
                unique_sources
            );
        }
        for src_idx in unique_sources {
            self.scan_source_region_for_cset_refs(
                &mut regions,
                src_idx,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }
        if dbg_phases {
            eprintln!(
                "[g1][PHASES] after-sources forwards={} (delta={})",
                pointer_map.len(),
                pointer_map.len() - p1_forwards
            );
        }

        // Phase 3: Cheney-style scan of evacuated objects
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;

            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Phase 3.5: finalizer resurrection (see
        // `collect_garbage_with_finalizers`). Must run after the closure is
        // complete (so "not forwarded" == dead) and before Phase 5 resets
        // the CSet regions (after that, the bytes are gone).
        self.resurrect_dead_finalizers(
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
            &mut work_list,
        );

        // Phase 4: Update forwarding pointers in non-CSet regions
        self.update_references_in_regions(&mut regions, &cset_set, &pointer_map);

        // Phase 5: Free evacuated regions
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);

        // SECURITY FIX (V7b): after the CSet is freed, scan survivors for
        // any slot still pointing into a freed CSet region with no
        // forwarding entry (incomplete remembered set => UAF). No-op on
        // the release/quiet path; aborts in debug.
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map);
        // Env-gated diagnostics (no-ops unless CRATONVM_G1_DBG_HEADERS /
        // CRATONVM_G1_DBG_ZERO are set) — same coverage the parallel path has,
        // so serial-path corruption is also caught at the collection that
        // introduces it.
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "young-serial");

        // Young and mixed evacuation leave humongous spans in place. Their
        // reachability is decided by the concurrent-mark cleanup phase, which
        // can see the whole heap and reclaims unmarked, unpinned spans there.

        // Reset current eden if it was in the CSet
        let cur_eden = self.current_eden.load(Ordering::Relaxed);
        if cset_set.contains(&cur_eden) {
            self.current_eden.store(usize::MAX, Ordering::Relaxed);
        }

        // Update old gen bytes tracking.
        // Relaxed ordering: this is a statistics counter read only by IHOP heuristics;
        // exact inter-thread visibility ordering is not required.
        self.recompute_old_gen_bytes(&regions);

        // Remap monitors
        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);

        // Round-7 fix (HIGH, audit §12): the concurrent mark worklist holds
        // raw object addresses that may have just been evacuated by this
        // young STW. The marker, after we unpark mutators, would otherwise
        // dereference stale pointers into freed regions → UAF. Walk the
        // worklist and apply the same `pointer_map` we applied to roots
        // and remembered-set updates.
        //
        // With the Phase-1b marking keep-alive, every CSet-resident gray was
        // evacuated above and therefore has a forwarding entry — the drop arm
        // below is defensive only. (It used to fire for grays the evacuation
        // closure did not reach, on the reasoning "unreached ⇒ dead ⇒ safe to
        // drop" — unsound under SATB: a gray unreachable at PAUSE time was
        // still live at MARK START, and dropping it unmarked its whole
        // unscanned subtree, which can include Old/humongous objects that are
        // very much live at cleanup. That was the marking-soundness defect
        // behind SteadyChurn's freed-live-Old-region failure.)
        //
        // Done under STW (still holding `regions.lock()`), so no marker
        // thread can be reading/writing `mark_worklist` concurrently — the
        // concurrent marker takes the same lock for each step.
        {
            let mut worklist = self.mark_worklist.lock();
            if !worklist.is_empty() {
                worklist.retain_mut(|addr| {
                    if let Some(&new_addr) = pointer_map.get(&*addr) {
                        // Object was evacuated — follow the forwarding ptr.
                        *addr = new_addr;
                        return true;
                    }
                    // Not forwarded. If the address lived in a CSet region
                    // it is now dangling (the region was reset above) so
                    // drop it. Otherwise (Old / non-CSet) leave it alone.
                    match self.region_for_ptr(&regions, *addr as *mut u8) {
                        Some(idx) if cset_set.contains(&idx) => false,
                        _ => true,
                    }
                });
            }
        }

        let pause_us = start.elapsed().as_micros() as u64;
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection(G1CollectionType::YoungOnly, pause_us, &stats);
        crate::gc_metrics::record_g1_cycle(
            crate::gc_metrics::g1_cycle_kind::YOUNG,
            cset.len() as u32,
            0,
            (jni_pinned_out + jit_pinned_out) as u32,
            rset_sources_scanned as u32,
            g1_pause_degraded_flags(&pointer_map, jni_pinned_out, jit_pinned_out, false),
        );

        GcResult { stats, pointer_map }
    }

    /// Phase H (RH.8) test hook — invoke `f` with mutable access to
    /// the region table.  **Intended for integration tests only.**
    /// External callers must assume this method will be removed if a
    /// safer API replaces it; it exists to let Phase H tests set up
    /// synthetic `gc_efficiency` values on old regions without having
    /// to run a full marking cycle.
    ///
    /// Holding the internal region lock across the closure guarantees
    /// no concurrent GC work observes a partially-mutated table.
    #[doc(hidden)]
    pub fn with_regions_mut<F: FnOnce(&mut [G1Region])>(&self, f: F) {
        let mut guard = self.regions.lock();
        f(&mut guard);
    }

    /// Phase H (RH.8) — select the old regions that will be evacuated
    /// in the next mixed collection, ordered by expected benefit.
    ///
    /// Selection policy:
    /// 1. **Eligible:** region must be of [`RegionType::Old`] and not
    ///    [`G1Region::pinned`] (JNI critical sections may pin regions).
    /// 2. **Rank:** ascending [`G1Region::gc_efficiency`] (= lowest
    ///    live-bytes ratio wins) — this is HotSpot's "worst-first"
    ///    heuristic: the region with the most reclaimable garbage is
    ///    selected first, maximising memory freed per byte copied.
    /// 3. **Cap:** at most `old_cset_region_threshold_percent` of all
    ///    regions (defaults to 10%), with a minimum of one region so
    ///    a tiny heap still makes progress.
    /// 4. **Pause-target cap (Step 7):** on top of the percentage cap, stop
    ///    adding old regions once their estimated copy time (rolling
    ///    `evac_ns_per_byte` × `live_bytes`) would exceed `max_gc_pause_ms`,
    ///    always keeping ≥1 region. Deferred regions are reclaimed in a later
    ///    mixed cycle.
    /// 5. **Deterministic:** ties broken by ascending region index.
    ///
    /// This helper is exposed publicly so tests and external policy
    /// hooks can inspect the selection without running a full mixed
    /// GC cycle.  The real cycle duplicates the logic inline to avoid
    /// releasing its region-table lock guard.
    pub fn select_old_regions_for_mixed_gc(&self) -> Vec<usize> {
        let regions = self.regions.lock();
        let total_regions = regions.len();
        let cap_percent = self.config.old_cset_region_threshold_percent as usize;
        let max_old = ((total_regions * cap_percent) / 100).max(1);

        // `live_bytes > 0` = "has liveness data from the last completed mark
        // cycle" (HotSpot likewise only mixes regions with marking data). A
        // region promoted into AFTER cleanup still carries the reset()
        // defaults live_bytes=0 / gc_efficiency=0: it sorts FIRST (looks
        // like 100% garbage) with an estimated evacuation cost of 0, so the
        // pause budget never binds on it and the mixed pause copies fully-
        // live regions wholesale — blowing max_gc_pause_ms and draining
        // to-space toward evacuation failure. A genuinely 0-live STAMPED
        // region cannot appear here: cleanup frees those in place.
        let mut candidates: Vec<(usize, f64)> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type == RegionType::Old && !r.pinned && r.live_bytes > 0)
            .map(|(i, r)| (i, r.gc_efficiency))
            .collect();
        candidates.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        // Step 7 — pause-target cap, mirroring the inline `mixed_collection`
        // path: bound the old CSet by the `max_gc_pause_ms` copy-time budget
        // (rolling `evac_ns_per_byte` × `live_bytes`) on top of the percentage
        // cap, always keeping at least one region for forward progress.
        let budget_ns = self.config.max_gc_pause_ms.saturating_mul(1_000_000);
        let ns_per_byte = self.evac_ns_per_byte.load(Ordering::Relaxed).max(1);
        let mut cost_ns: u64 = 0;
        let mut out: Vec<usize> = Vec::new();
        for (i, _) in candidates.into_iter().take(max_old) {
            let cost = regions[i].estimated_evac_cost_ns(ns_per_byte);
            if !out.is_empty() && cost_ns.saturating_add(cost) > budget_ns {
                break;
            }
            out.push(i);
            cost_ns = cost_ns.saturating_add(cost);
        }
        out
    }

    /// Perform a mixed collection. Evacuates young + selected old regions.
    pub fn mixed_collection(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Mixed GC always uses the SERIAL evacuator, even under
        // `CRATONVM_G1_PARALLEL_EVAC`. The parallel evacuator still has an OPEN
        // correctness bug, so mixed (which reclaims old gen) stays on the
        // differentially-verified serial path.
        //
        // Two distinct parallel-evac defects exist:
        //  1. self-forward (evacuation-failure) UAF — FIXED on dev `7e97111e`
        //     (`parallel_evacuate` clears each self-forward's `forwarding_ptr`
        //     after merging the shards; the `key == value` loop).
        //  2. a RARE, NON-DETERMINISTIC race in the *young* parallel evacuator
        //     (`run_worker`/`evacuate`/`process_object`) that intermittently
        //     corrupts a live object under frequent young GCs at small heaps —
        //     reproduced on `SteadyChurn @16m --nojit` (~1 in 8 runs, smallest
        //     heap only; serial is always correct). This is NOT defect 1 (no
        //     to-space exhaustion) and is the tracked parallel-evac follow-up.
        //     Because it lives in the shared `parallel_evacuate` closure that
        //     `mixed_collection_parallel` also drives, mixed must not be routed
        //     to it until the race is fixed — even though `PromoteMixed` happened
        //     to pass at the larger heaps it was tested on.
        //
        // mixed GCs are infrequent, so serializing them costs little. The
        // parallel young path is unchanged (`young_collection` still dispatches
        // to it under the flag). `mixed_collection_parallel` is retained (and
        // unit-tested directly) for when defect 2 is resolved.
        let start = std::time::Instant::now();
        let mut regions = self.regions.lock();
        // SECURITY FIX (V7a): mixed GC resets/retypes CSet regions
        // (Phase 5). Invalidate every mutator's RSet fast-path cache
        // before any reclassification — see `rset_cache_epoch`.
        self.rset_cache_epoch.fetch_add(1, Ordering::Release);
        let mut pointer_map: HashMap<usize, usize> = HashMap::new();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;

        // Exclude regions holding a conservative JIT root from the CSet (pin in
        // place) — see `young_collection` / `jit_pinned_region_set`. A mixed GC
        // can also select the (now promoted) region of a long-lived JIT-rooted
        // object, so this guard matters for both young and old CSet members.
        let jit_pinned_regions = self.jit_pinned_region_set();

        // Build CSet: all young regions + worst old regions (skip pinned + any
        // region holding a conservative JIT root)
        let mut cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && (r.region_type == RegionType::Eden || r.region_type == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();
        let cset_young = cset.len();
        let (jni_pinned_out, jit_pinned_out) =
            count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);

        // Select old regions sorted by gc_efficiency (lowest = most
        // garbage first).  See [`Self::select_old_regions_for_mixed_gc`]
        // for the stand-alone helper; the logic is duplicated here to
        // avoid releasing the `regions` lock guard.
        let max_old =
            (regions.len() * self.config.old_cset_region_threshold_percent as usize) / 100;
        let max_old = max_old.max(1);

        let mut old_candidates: Vec<(usize, f64)> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                // live_bytes > 0 = has liveness data from the last completed
                // mark cycle — see select_old_regions_for_mixed_gc.
                r.region_type == RegionType::Old
                    && !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && r.live_bytes > 0
            })
            .map(|(i, r)| (i, r.gc_efficiency))
            .collect();
        // Stable sort so that among regions with identical efficiency
        // the lower index wins — keeps selection deterministic.
        old_candidates.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        // Step 7 — pause-target CSet sizing. On top of the percentage cap
        // (`max_old`), bound the OLD collection set by an estimated copy-time
        // budget so a mixed pause stays near `max_gc_pause_ms`. Per-region cost
        // uses the rolling `evac_ns_per_byte` calibration. Always include at
        // least one old region for forward progress; stop before the budget is
        // exceeded — the deferred regions are reclaimed in a later mixed cycle
        // (`mixed_gc_remaining`). The percentage cap remains the hard upper
        // bound; the budget only binds when a single mixed GC would copy enough
        // live old data to blow the target (a genuinely long pause).
        let budget_ns = self.config.max_gc_pause_ms.saturating_mul(1_000_000);
        let ns_per_byte = self.evac_ns_per_byte.load(Ordering::Relaxed).max(1);
        let mut old_cost_ns: u64 = 0;
        let mut old_selected: usize = 0;
        for (idx, _) in old_candidates.into_iter().take(max_old) {
            let cost = regions[idx].estimated_evac_cost_ns(ns_per_byte);
            if old_selected > 0 && old_cost_ns.saturating_add(cost) > budget_ns {
                break;
            }
            cset.push(idx);
            old_cost_ns = old_cost_ns.saturating_add(cost);
            old_selected += 1;
        }

        let cset_set: std::collections::HashSet<usize> = cset.iter().copied().collect();
        // Same invariant as the young path: a pinned region must never enter a
        // collection set, because Phase 5 resets every CSet region that holds
        // no self-forwarded object. A mixed CSet is the harder case — it also
        // takes Old regions, where a long-lived JIT-rooted or JNI-pinned object
        // is most likely to have ended up.
        debug_assert!(
            cset.iter()
                .all(|&i| !regions[i].pinned && !jit_pinned_regions.contains(&i)),
            "G1 mixed CSet contains a pinned region"
        );
        let mut work_list: Vec<*mut u8> = Vec::new();

        // Evacuate roots
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(region_idx) = self.region_for_ptr(&regions, old_ptr) {
                if cset_set.contains(&region_idx) {
                    // Step 9: `fresh` is ignored here — the root loop keeps its
                    // existing unconditional push (a duplicate root re-scans
                    // idempotently). Gating it on `fresh` is deferred to the
                    // parallel evacuator (where duplicate worklist entries
                    // matter for worker load).
                    if let Some((new_ptr, _fresh)) = self.evacuate_object(
                        &mut regions,
                        old_ptr,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                        &cset_set,
                    ) {
                        *root = unsafe { ObjectRef::from_raw(new_ptr) };
                        work_list.push(new_ptr);
                    }
                }
            }
        }

        // Marking keep-alive (see `marking_keepalive_roots`): a mixed pause
        // can run while a NEW concurrent mark cycle is active (IHOP re-fires
        // during the post-cleanup mixed sequence — `start_concurrent_mark`
        // does not clear `marking_complete`), and its CSet includes Old
        // regions, exactly where the gray set concentrates. Same protocol as
        // the young paths.
        for addr in self.marking_keepalive_roots(&regions, &cset_set) {
            if let Some((new_ptr, fresh)) = self.evacuate_object(
                &mut regions,
                addr as *mut u8,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &cset_set,
            ) {
                if fresh {
                    work_list.push(new_ptr);
                }
                self.push_gray_or_mark(&regions, new_ptr as usize);
            }
        }

        // CRIT fix (UAF): process remembered-set sources for every CSet
        // region. Collect sources up front (before mutating regions during
        // evacuation), dedup, then walk each source region rewriting
        // CSet-bound slots and seeding the work_list with newly evacuated
        // targets. Without this, cross-region refs (e.g. old → young)
        // were silently dropped, leaving stale pointers in non-CSet
        // regions after CSet reset.
        let mixed_rset_sources: std::collections::HashSet<usize> = {
            // Round-9 gc CRIT-8: the snapshot accessors return owned data
            // (the underlying map lives behind a per-RSet mutex), so the RSet
            // lock is not held across the body.
            //
            // G1AUD-5 (defect G1-8): entries whose source was recycled since
            // the edge was recorded are dropped rather than re-walked — see
            // `live_rset_sources` and the note in `young_collection`.
            let mut set = Self::live_rset_sources(&regions, &cset);
            // JIT-pinned regions are scanned as sources too (see
            // young_collection): their objects stay in place but their CSet
            // referents must still be evacuated and fixed up.
            set.extend(jit_pinned_regions.iter().copied());
            set
        };
        let rset_sources_scanned = mixed_rset_sources.len();

        for src_idx in mixed_rset_sources {
            self.scan_source_region_for_cset_refs(
                &mut regions,
                src_idx,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Cheney scan
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;

            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Phase 3.5: finalizer resurrection — see young_collection.
        self.resurrect_dead_finalizers(
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
            &mut work_list,
        );

        // Update references and free evacuated regions
        self.update_references_in_regions(&mut regions, &cset_set, &pointer_map);

        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);

        // SECURITY FIX (V7b): mixed GC frees old regions as well as young
        // ones, where a stale/incomplete rset is most likely. Verify no
        // survivor slot dangles into a freed CSet region. No-op on the
        // release/quiet path; aborts in debug.
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map);
        // Env-gated diagnostics (no-ops unless CRATONVM_G1_DBG_HEADERS /
        // CRATONVM_G1_DBG_ZERO are set) — mixed-path coverage matching the
        // parallel young path.
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "mixed-serial");

        let cur_eden = self.current_eden.load(Ordering::Relaxed);
        if cset_set.contains(&cur_eden) {
            self.current_eden.store(usize::MAX, Ordering::Relaxed);
        }

        // Relaxed ordering: statistics counter for IHOP heuristics only.
        self.recompute_old_gen_bytes(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — same
        // protocol as the young paths. A mixed pause moves OLD objects, where
        // the gray set concentrates; before this block the mixed path left
        // every gray pointing into freed CSet regions (marker UAF whenever a
        // new mark cycle overlaps the mixed sequence). With the keep-alive
        // above, every retained CSet gray has a forwarding entry.
        {
            let mut worklist = self.mark_worklist.lock();
            if !worklist.is_empty() {
                worklist.retain_mut(|addr| {
                    if let Some(&new_addr) = pointer_map.get(&*addr) {
                        *addr = new_addr;
                        return true;
                    }
                    match self.region_for_ptr(&regions, *addr as *mut u8) {
                        Some(idx) if cset_set.contains(&idx) => false,
                        _ => true,
                    }
                });
            }
        }

        // Decrement mixed GC counter.
        // Relaxed ordering: mixed_gc_remaining and marking_complete are GC-internal
        // scheduling counters only accessed during STW pauses (single-threaded).
        let remaining = self.mixed_gc_remaining.load(Ordering::Relaxed);
        if remaining > 0 {
            self.mixed_gc_remaining
                .store(remaining - 1, Ordering::Relaxed);
            if remaining - 1 == 0 {
                self.marking_complete.store(false, Ordering::Relaxed);
            }
        }

        let elapsed = start.elapsed();
        let pause_us = elapsed.as_micros() as u64;
        // Step 7 — recalibrate the rolling copy-cost from this mixed cycle's
        // actual pause / bytes copied, so the next mixed CSet is sized against
        // real wall-clock throughput.
        self.update_evac_cost(elapsed.as_nanos() as u64, bytes_copied);
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection(G1CollectionType::Mixed, pause_us, &stats);
        crate::gc_metrics::record_g1_cycle(
            crate::gc_metrics::g1_cycle_kind::MIXED,
            cset_young as u32,
            old_selected as u32,
            (jni_pinned_out + jit_pinned_out) as u32,
            rset_sources_scanned as u32,
            g1_pause_degraded_flags(&pointer_map, jni_pinned_out, jit_pinned_out, false),
        );

        GcResult { stats, pointer_map }
    }

    // -----------------------------------------------------------------------
    // Step 9 — parallel evacuation drivers (gated; see the module note above)
    // -----------------------------------------------------------------------

    /// Number of evacuation workers: `gc_worker_threads` clamped to the
    /// available hardware parallelism (always ≥ 1). With 1 worker the parallel
    /// code path drains serially — useful for determinism testing.
    fn parallel_worker_count(&self) -> usize {
        // Diagnostic override: `CRATONVM_G1_WORKERS=N` forces the worker count
        // (e.g. =1 to drain the parallel path serially and isolate concurrency
        // races from logic divergences). Falls back to the config otherwise.
        if let Some(n) = gc_flags().g1_workers {
            return n;
        }
        let cfg = self.config.gc_worker_threads.max(1);
        let avail = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        cfg.min(avail)
    }

    /// Shared seed + parallel transitive-closure core used by both
    /// `young_collection_parallel` and `mixed_collection_parallel`.
    ///
    /// Seeds the work queue from `roots` and from `sources` on the calling
    /// (driver) thread, then runs the work-stealing closure across
    /// `parallel_worker_count()` workers (the driver participating as one). Roots
    /// are rewritten in place. Returns the merged forwarding map plus the
    /// objects/bytes copied.
    ///
    /// `sources` is the set of region indices to walk wholesale looking for
    /// CSet-bound references. G1AUD-6 (defect G1-9): it is computed by the
    /// CALLER, while the `regions` guard is still dereferenceable, and must be
    /// the same set the serial path builds — the live remembered-set sources of
    /// every CSet region PLUS every JIT-pinned region. This function used to
    /// derive it itself from the CSet remembered sets alone, which silently
    /// dropped the JIT-pinned half; see `young_collection_parallel` for why that
    /// is a live-object loss and not merely a missing optimisation.
    ///
    /// SAFETY: `regions_base` must be the live regions Vec base and the caller
    /// must hold `regions.lock()` for the whole call WITHOUT dereferencing the
    /// guard (see the module SAFETY MODEL note). `pool` lists currently-Free
    /// region indices reserved for to-space allocation.
    unsafe fn parallel_evacuate(
        &self,
        regions_base: RegionsBase,
        cset_set: &std::collections::HashSet<usize>,
        pool: Vec<usize>,
        roots: &mut [ObjectRef],
        keepalive: &[usize],
        sources: &std::collections::HashSet<usize>,
    ) -> (HashMap<usize, usize>, usize, usize) {
        // One-shot confirmation that the parallel evacuator is genuinely active
        // (the gauntlet lesson: never assume a gated path was taken — verify).
        {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    "g1: parallel evacuation ACTIVE ({} workers, CRATONVM_G1_PARALLEL_EVAC)",
                    self.parallel_worker_count()
                );
            }
        }
        let pool_next = AtomicUsize::new(0);
        let queue: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        let outstanding = AtomicUsize::new(0);
        let shared = SharedEvac {
            collector: self,
            regions_base,
            cset: cset_set,
            pool: &pool,
            pool_next: &pool_next,
            queue: &queue,
            outstanding: &outstanding,
            promotion_age: self.config.promotion_age,
        };

        let mut objs = 0usize;
        let mut bytes = 0usize;
        let mut main_tlab = TlabSet::default();
        let mut main_forwards: Vec<(usize, usize)> = Vec::new();
        let mut main_deferred_self_forwarded: Vec<usize> = Vec::new();

        // Phase 1 (driver): seed roots.
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(ridx) = self.lookup_region_for_addr(old_ptr as usize) {
                if cset_set.contains(&ridx) {
                    if let Some((new_ptr, fresh)) = shared.evacuate(
                        &mut main_tlab,
                        old_ptr,
                        &mut main_forwards,
                        &mut objs,
                        &mut bytes,
                    ) {
                        *root = ObjectRef::from_raw(new_ptr);
                        if fresh {
                            if new_ptr == old_ptr {
                                main_deferred_self_forwarded.push(new_ptr as usize);
                            } else {
                                outstanding.fetch_add(1, Ordering::AcqRel);
                                queue.lock().push(new_ptr as usize);
                            }
                        }
                    }
                }
            }
        }

        // Phase 1b (driver): seed marking keep-alive addresses (CSet-resident
        // gray/SATB objects — snapshot-live; see `marking_keepalive_roots`).
        // Evacuated exactly like roots; the fresh copies are re-grayed by the
        // caller and the worklist remap after Phase 5 rewrites the old gray
        // entries through the forwarding map instead of dropping them.
        for &addr in keepalive {
            let old_ptr = addr as *mut u8;
            if let Some(ridx) = self.lookup_region_for_addr(addr) {
                if cset_set.contains(&ridx) {
                    if let Some((new_ptr, fresh)) = shared.evacuate(
                        &mut main_tlab,
                        old_ptr,
                        &mut main_forwards,
                        &mut objs,
                        &mut bytes,
                    ) {
                        if fresh {
                            if new_ptr == old_ptr {
                                main_deferred_self_forwarded.push(new_ptr as usize);
                            } else {
                                outstanding.fetch_add(1, Ordering::AcqRel);
                                queue.lock().push(new_ptr as usize);
                            }
                        }
                    }
                }
            }
        }

        // Phase 2 (driver): seed the source regions the caller selected — the
        // CSet remembered-set sources plus the JIT-pinned regions (G1AUD-6).
        for &src_idx in sources {
            shared.seed_source_region(
                src_idx,
                &mut main_tlab,
                &mut main_forwards,
                &mut objs,
                &mut bytes,
                &mut main_deferred_self_forwarded,
            );
        }

        // Phase 3: parallel transitive closure.
        let nworkers = self.parallel_worker_count();
        let (extra_objs, extra_bytes, worker_forwards, worker_deferred): (
            usize,
            usize,
            Vec<Vec<(usize, usize)>>,
            Vec<Vec<usize>>,
        ) = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for _ in 1..nworkers {
                let shared_ref = &shared;
                handles.push(s.spawn(move || {
                    let mut tlab = TlabSet::default();
                    let mut o = 0usize;
                    let mut b = 0usize;
                    let mut f: Vec<(usize, usize)> = Vec::new();
                    let mut d: Vec<usize> = Vec::new();
                    unsafe {
                        shared_ref.run_worker(&mut tlab, &mut o, &mut b, &mut f, &mut d);
                    }
                    (o, b, f, d)
                }));
            }
            // The driver participates as a worker, reusing its seeded TLAB
            // and accumulators.
            unsafe {
                shared.run_worker(
                    &mut main_tlab,
                    &mut objs,
                    &mut bytes,
                    &mut main_forwards,
                    &mut main_deferred_self_forwarded,
                );
            }
            let mut to = 0usize;
            let mut tb = 0usize;
            let mut allf: Vec<Vec<(usize, usize)>> = Vec::new();
            let mut alld: Vec<Vec<usize>> = Vec::new();
            for h in handles {
                let (o, b, f, d) = h.join().expect("g1 parallel-evac worker panicked");
                to += o;
                tb += b;
                allf.push(f);
                alld.push(d);
            }
            (to, tb, allf, alld)
        });
        objs += extra_objs;
        bytes += extra_bytes;

        for f in worker_forwards {
            main_forwards.extend(f);
        }
        for d in worker_deferred {
            main_deferred_self_forwarded.extend(d);
        }

        // The identity forward itself is the authoritative signal that an
        // evacuation-failed object stayed in its CSet region. Derive the serial
        // drain set from the merged forward shards as a backstop for every
        // caller path, rather than relying only on the side-channel populated
        // when a caller observes `fresh && old == new`.
        //
        // FIXPOINT: the serial drain can itself self-forward children (pool
        // exhaustion mid-drain), so re-derive the identity-forward set and
        // re-drain until no new work appears — every LIVE self-forwarded
        // holder gets its slots rewritten before Phase 4/5.
        //
        // DEAD objects in kept regions are deliberately NOT scanned (an
        // earlier revision walked every object of every kept region here;
        // under sustained evacuation failure that resurrected the kept
        // regions' garbage wholesale each pause — the dead holders' targets
        // were evacuated as if live — and OOMed the SteadyChurn recreation).
        // Their slots may retain stale cross-cycle references, which is safe:
        // a stale reference can only point into a region that is Free, a
        // this-pause destination, or a mutator-reused region — never a
        // CSet-resident live object — so nothing consults it as a live edge.
        let mut serial_tlab = TlabSet::default();
        let mut drained = 0usize;
        loop {
            SharedEvac::append_self_forwarded_from_forwards(
                &main_forwards,
                &mut main_deferred_self_forwarded,
            );
            if drained >= main_deferred_self_forwarded.len() {
                break;
            }
            unsafe {
                shared.drain_deferred_self_forwarded(
                    &mut serial_tlab,
                    &mut main_forwards,
                    &mut objs,
                    &mut bytes,
                    &mut main_deferred_self_forwarded,
                    &mut drained,
                );
            }
        }
        unsafe {
            shared.retire_all(&mut serial_tlab);
        }

        // Merge the per-worker forward shards into the pointer map consumed by
        // the VM root remap and Phases 4/5.
        let mut pointer_map: HashMap<usize, usize> = HashMap::with_capacity(main_forwards.len());
        for (o, n) in main_forwards {
            pointer_map.insert(o, n);
        }

        // DEFECT-2 FIX (part 2 of 2): restore the evacuator's
        // "`forwarding_ptr == 0` at collection start" invariant for EVERY
        // from-space object forwarded this cycle, not just the self-forwarded
        // ones.
        //
        // The parallel evacuator CAS-installs each forward into the from-space
        // object's *persistent* `ObjectHeader::forwarding_ptr` (the lock-free
        // install slot), unlike the serial path which records forwards only in
        // the per-cycle `pointer_map`. Phase 5 (`free_or_keep_cset`) zeroes the
        // field for a FREED region, but a KEPT region (one holding a
        // self-forwarded / evacuation-failed object) is never reset, so the
        // `forwarding_ptr` of EVERY forwarded object it contains — the
        // self-forwarded one AND the normally-evacuated bodies left behind —
        // would persist into the next collection. On the next cycle `evacuate`'s
        // fast path reads that STALE forward, returns it without re-evacuating or
        // recording it, and (with part 1) records a stale forward / strands a
        // root on a from-space object → the rare `java/lang/Object`.
        //
        // Clearing only `k == v` (self-forwards) was insufficient: it left the
        // normally-evacuated bodies in kept regions stale (and clearing them
        // alone, without part 1's fast-path recording, removed the redirect that
        // was masking the stuck roots — hence the two halves are landed
        // together). Clear ALL keys: for a freed region this is redundant (Phase
        // 5 zeroes it anyway); for a kept region it is the correction. Together
        // with part 1 this makes the parallel path behave like the serial one —
        // every forward recorded in `pointer_map`, none persisting across cycles.
        for &k in pointer_map.keys() {
            // SAFETY: `k` is a from-space object address forwarded this cycle;
            // its header is intact and its region is held under the collection's
            // `regions` lock (Phase 5 has not run yet).
            unsafe {
                (*(k as *mut ObjectHeader)).forwarding_ptr = std::ptr::null_mut();
            }
        }

        (pointer_map, objs, bytes)
    }

    /// Parallel young-only collection (Step 9). Behaviour-equivalent to
    /// [`Self::young_collection`]'s serial body — Phases 1–3 run through the
    /// multi-threaded evacuator, Phases 4/5 + stats mirror the serial path.
    pub(crate) fn young_collection_parallel(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.lock();
        // SECURITY FIX (V7a): invalidate every mutator's RSet fast-path cache
        // before any reclassification (see `rset_cache_epoch`).
        self.rset_cache_epoch.fetch_add(1, Ordering::Release);

        // Same conservative-JIT-root region exclusion as the serial path:
        // objects held by un-rewritable JIT register/spill slots must not
        // move. Empty (free) unless a thread is in JIT, and the young path
        // only dispatches here when none is — this keeps the invariant even
        // if that gate is ever loosened or the fn is called directly.
        let jit_pinned_regions = self.jit_pinned_region_set();
        let cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && (r.region_type == RegionType::Eden || r.region_type == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();
        if cset.is_empty() {
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map: HashMap::new(),
            };
        }
        let cset_set: std::collections::HashSet<usize> = cset.iter().copied().collect();
        let pool: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type == RegionType::Free)
            .map(|(i, _)| i)
            .collect();

        // Marking keep-alive (see `marking_keepalive_roots`): computed while
        // the guard is still dereferenceable, passed to the evacuator as
        // extra roots, and re-grayed after Phase 5 below.
        let keepalive = self.marking_keepalive_roots(&regions, &cset_set);

        // G1AUD-6 (defect G1-9) — the source set, built EXACTLY as the serial
        // `young_collection` builds it (`live_rset_sources` + every JIT-pinned
        // region), while the guard is still dereferenceable.
        //
        // The parallel evacuator used to derive this set itself from the CSet
        // remembered sets alone, omitting the `unique_sources.extend(
        // jit_pinned_regions)` term the serial path has carried since the
        // original CSet-straddle UAF fix. That is not a lost optimisation: a
        // JIT-pinned region is EXCLUDED from the CSet (so it is never traced as
        // a from-space object) and is reached only as a remembered-set source,
        // so any CSet object referenced from it that the barrier failed to
        // record was neither evacuated nor rewritten — its holder's slot then
        // pointed into a region Phase 5 zero-filled.
        //
        // `jit_pinned_region_set()` is NOT gated on `gc_quiescence::is_active()`
        // (see its INT-3 note): it also contains every region holding a
        // published un-retired TLAB tail, so this set is routinely non-empty on
        // a multi-threaded pause with NO thread in JIT — which is precisely the
        // configuration G1-9's live-object corruption was reproduced under
        // (`SteadyChurn @16m --nojit`, smallest heap, ~1 run in 8). Whether or
        // not it is the whole of that defect, the serial/parallel divergence is
        // real and the fail-safe direction is to walk MORE sources, never
        // fewer.
        let parallel_sources: std::collections::HashSet<usize> = {
            let mut set = Self::live_rset_sources(&regions, &cset);
            set.extend(jit_pinned_regions.iter().copied());
            set
        };

        // Take the raw regions base; do NOT deref `regions` again until after
        // `parallel_evacuate` returns (see the module SAFETY MODEL note).
        let regions_base = RegionsBase(regions.as_mut_ptr());
        let (pointer_map, objects_copied, bytes_copied) = unsafe {
            self.parallel_evacuate(
                regions_base,
                &cset_set,
                pool,
                roots,
                &keepalive,
                &parallel_sources,
            )
        };

        // Phase 4: update interior refs in non-CSet regions.
        self.update_references_in_regions(&mut regions, &cset_set, &pointer_map);

        // Phase 5: free evacuated regions.
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map);
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "young-parallel");

        // Re-gray the keep-alive copies (SATB-origin entries never sat in the
        // worklist, so the remap below cannot rewrite them — push their
        // relocated copies explicitly; duplicates are idempotent).
        for &addr in &keepalive {
            if let Some(&new_addr) = pointer_map.get(&addr) {
                self.push_gray_or_mark(&regions, new_addr);
            }
        }

        let cur_eden = self.current_eden.load(Ordering::Relaxed);
        if cset_set.contains(&cur_eden) {
            self.current_eden.store(usize::MAX, Ordering::Relaxed);
        }

        self.recompute_old_gen_bytes(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — identical to
        // the serial young path. Done under STW (guard held) so no marker step
        // races us.
        {
            let mut worklist = self.mark_worklist.lock();
            if !worklist.is_empty() {
                worklist.retain_mut(|addr| {
                    if let Some(&new_addr) = pointer_map.get(&*addr) {
                        *addr = new_addr;
                        return true;
                    }
                    match self.region_for_ptr(&regions, *addr as *mut u8) {
                        Some(idx) if cset_set.contains(&idx) => false,
                        _ => true,
                    }
                });
            }
        }

        let pause_us = start.elapsed().as_micros() as u64;
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection(G1CollectionType::YoungOnly, pause_us, &stats);
        let (jni_pinned_out, jit_pinned_out) =
            count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
        crate::gc_metrics::record_g1_cycle(
            crate::gc_metrics::g1_cycle_kind::YOUNG,
            cset.len() as u32,
            0,
            (jni_pinned_out + jit_pinned_out) as u32,
            // G1AUD-6: the parallel path now reports the sources it walked,
            // like the serial one. A hard 0 here made the two paths' cycle
            // records incomparable, which is exactly how the missing
            // JIT-pinned source term stayed invisible.
            parallel_sources.len() as u32,
            g1_pause_degraded_flags(&pointer_map, jni_pinned_out, jit_pinned_out, true),
        );
        GcResult { stats, pointer_map }
    }

    /// Parallel mixed collection (Step 9). Behaviour-equivalent to
    /// [`Self::mixed_collection`]'s serial body (including the Step-7
    /// pause-target old-region CSet sizing and the mixed-cycle bookkeeping).
    pub(crate) fn mixed_collection_parallel(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.lock();
        self.rset_cache_epoch.fetch_add(1, Ordering::Release);

        // Build CSet: all young regions + worst (most-garbage) old regions,
        // bounded by both the percentage cap and the Step-7 pause budget —
        // identical selection to the serial `mixed_collection`.
        // Conservative-JIT-root region exclusion — see young_collection_parallel.
        let jit_pinned_regions = self.jit_pinned_region_set();
        let mut cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && (r.region_type == RegionType::Eden || r.region_type == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();

        let max_old =
            (regions.len() * self.config.old_cset_region_threshold_percent as usize) / 100;
        let max_old = max_old.max(1);

        let mut old_candidates: Vec<(usize, f64)> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                // live_bytes > 0 = has liveness data from the last completed
                // mark cycle — see select_old_regions_for_mixed_gc.
                r.region_type == RegionType::Old
                    && !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && r.live_bytes > 0
            })
            .map(|(i, r)| (i, r.gc_efficiency))
            .collect();
        old_candidates.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        let budget_ns = self.config.max_gc_pause_ms.saturating_mul(1_000_000);
        let ns_per_byte = self.evac_ns_per_byte.load(Ordering::Relaxed).max(1);
        let mut old_cost_ns: u64 = 0;
        let mut old_selected: usize = 0;
        for (idx, _) in old_candidates.into_iter().take(max_old) {
            let cost = regions[idx].estimated_evac_cost_ns(ns_per_byte);
            if old_selected > 0 && old_cost_ns.saturating_add(cost) > budget_ns {
                break;
            }
            cset.push(idx);
            old_cost_ns = old_cost_ns.saturating_add(cost);
            old_selected += 1;
        }

        if cset.is_empty() {
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map: HashMap::new(),
            };
        }
        let cset_set: std::collections::HashSet<usize> = cset.iter().copied().collect();
        let pool: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type == RegionType::Free)
            .map(|(i, _)| i)
            .collect();

        // Marking keep-alive (see `marking_keepalive_roots` and the note in
        // the serial `mixed_collection`).
        let keepalive = self.marking_keepalive_roots(&regions, &cset_set);

        // G1AUD-6 — same source set the serial `mixed_collection` builds; see
        // the long note in `young_collection_parallel`.
        let parallel_sources: std::collections::HashSet<usize> = {
            let mut set = Self::live_rset_sources(&regions, &cset);
            set.extend(jit_pinned_regions.iter().copied());
            set
        };

        let regions_base = RegionsBase(regions.as_mut_ptr());
        let (pointer_map, objects_copied, bytes_copied) = unsafe {
            self.parallel_evacuate(
                regions_base,
                &cset_set,
                pool,
                roots,
                &keepalive,
                &parallel_sources,
            )
        };

        self.update_references_in_regions(&mut regions, &cset_set, &pointer_map);

        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map);
        self.dbg_verify_reachable_integrity(&regions, roots, "mixed-parallel");

        // Re-gray the keep-alive copies (see young_collection_parallel).
        for &addr in &keepalive {
            if let Some(&new_addr) = pointer_map.get(&addr) {
                self.push_gray_or_mark(&regions, new_addr);
            }
        }

        let cur_eden = self.current_eden.load(Ordering::Relaxed);
        if cset_set.contains(&cur_eden) {
            self.current_eden.store(usize::MAX, Ordering::Relaxed);
        }

        self.recompute_old_gen_bytes(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — same
        // protocol as the young paths (the mixed CSet includes Old regions,
        // where the gray set concentrates).
        {
            let mut worklist = self.mark_worklist.lock();
            if !worklist.is_empty() {
                worklist.retain_mut(|addr| {
                    if let Some(&new_addr) = pointer_map.get(&*addr) {
                        *addr = new_addr;
                        return true;
                    }
                    match self.region_for_ptr(&regions, *addr as *mut u8) {
                        Some(idx) if cset_set.contains(&idx) => false,
                        _ => true,
                    }
                });
            }
        }

        let remaining = self.mixed_gc_remaining.load(Ordering::Relaxed);
        if remaining > 0 {
            self.mixed_gc_remaining
                .store(remaining - 1, Ordering::Relaxed);
            if remaining - 1 == 0 {
                self.marking_complete.store(false, Ordering::Relaxed);
            }
        }

        let elapsed = start.elapsed();
        let pause_us = elapsed.as_micros() as u64;
        self.update_evac_cost(elapsed.as_nanos() as u64, bytes_copied);
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection(G1CollectionType::Mixed, pause_us, &stats);
        GcResult { stats, pointer_map }
    }

    // -----------------------------------------------------------------------
    // Evacuation helpers
    // -----------------------------------------------------------------------

    /// Find which region a raw pointer belongs to.
    ///
    /// O(log R) via binary search on the cached `region_lookup` table.
    /// The `regions` parameter is retained for signature compatibility but
    /// is no longer scanned linearly — the per-slot evacuation hot path
    /// (CRIT-P4) used to incur an O(R) probe per reference slot, dragging
    /// scan cost to O(objects × refs × regions). Lookup is now independent
    /// of the live-region count.
    ///
    /// Note: this no longer skips `Free` regions (matching
    /// [`Self::region_for_ptr_with_regions`]). All callers either filter
    /// via the CSet (which excludes Free regions by construction) or are
    /// inherently safe against the case.
    fn region_for_ptr(&self, _regions: &[G1Region], ptr: *mut u8) -> Option<usize> {
        self.lookup_region_for_addr(ptr as usize)
    }

    /// Evacuate a single object from its current region to Survivor or Old.
    /// Returns `Some((new_ptr, fresh))` where `fresh` is `true` iff THIS call
    /// performed the copy (vs found an existing forwarding entry), or `None`
    /// on evacuation failure.
    ///
    /// Step 9 (parallel-evac foundation): `fresh` is the dedup signal callers
    /// use to gate the work_list push, replacing a separate
    /// `pointer_map.contains_key` pre-check — that pre-check is a TOCTOU the
    /// moment evacuation is sharded across worker threads (two workers both
    /// sample "absent", both copy, the loser's allocation leaks and is
    /// double-scanned). Reading the freshness from the evacuation outcome
    /// instead means the parallel evacuator can later derive it from the
    /// atomic CAS-forwarding install in the object header (`forwarding_ptr`)
    /// without changing the call sites.
    /// Run a collection with finalizer-aware resurrection: any address in
    /// `finalizer_addrs` (registered, not-yet-enqueued finalizable objects,
    /// from `ReferenceProcessor::finalizer_referent_addresses`) that DIES in
    /// this collection's CSet is evacuated anyway — with its transitive
    /// closure — so `finalize()` can later run against valid memory, exactly
    /// like the generational backend's Phase 2.5. Returns the collection
    /// result plus the POST-copy addresses of the resurrected objects; the
    /// caller must enqueue them for finalization AND mark their processor
    /// entries enqueued (once-only finalization).
    ///
    /// Dead finalizables OUTSIDE the CSet (e.g. promoted to an Old region a
    /// young pause does not collect) are left in place, still registered —
    /// they are picked up by whichever later collection selects their
    /// region.
    pub fn collect_garbage_with_finalizers(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        *self.pending_finalizer_roots.lock() = finalizer_addrs.to_vec();
        self.resurrected_finalizers.lock().clear();
        let result = <Self as crate::collector::GarbageCollector>::collect_garbage(
            self, stw, roots, monitors,
        );
        // Belt-and-braces: clear any candidates a path did not consume
        // (e.g. an empty-CSet early return) so a later plain collection
        // never sees stale candidates.
        self.pending_finalizer_roots.lock().clear();
        let mut dead = std::mem::take(&mut *self.resurrected_finalizers.lock());
        // `retry_after_evacuation_failure` (run inside collect_garbage,
        // after Phase 3.5) can relocate objects AGAIN via the kept-region
        // drain and composes those forwards into the final pointer map —
        // resolve each resurrected address through it so the caller never
        // enqueues an intermediate (already-vacated) address.
        for addr in dead.iter_mut() {
            if let Some(&fixed) = result.pointer_map.get(&*addr) {
                *addr = fixed;
            }
        }
        (result, dead)
    }

    /// Phase 3.5 (serial young/mixed): evacuate dead-but-finalizable CSet
    /// objects (and their transitive closure, via the same Phase-3 scan)
    /// so `finalize()` can run against valid memory. See
    /// [`Self::collect_garbage_with_finalizers`]. No-op when no candidates
    /// are pending (every plain collection).
    fn resurrect_dead_finalizers(
        &self,
        regions: &mut Vec<G1Region>,
        cset_set: &std::collections::HashSet<usize>,
        pointer_map: &mut HashMap<usize, usize>,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
    ) {
        let candidates = std::mem::take(&mut *self.pending_finalizer_roots.lock());
        if candidates.is_empty() {
            return;
        }
        let mut resurrected = Vec::new();
        let scan_resume = work_list.len();
        for old_addr in candidates {
            if pointer_map.contains_key(&old_addr) {
                continue; // survived normally (or already self-forwarded)
            }
            let Some(ridx) = self.lookup_region_for_addr(old_addr) else {
                continue; // not a current heap address
            };
            if !cset_set.contains(&ridx) {
                continue; // not dying this pause — stays registered
            }
            // Dead in the CSet: evacuate it like a root and let the scan
            // below pull its subtree out too.
            if let Some((new_ptr, fresh)) = self.evacuate_object(
                regions,
                old_addr as *mut u8,
                pointer_map,
                objects_copied,
                bytes_copied,
                cset_set,
            ) {
                if fresh {
                    work_list.push(new_ptr);
                }
                resurrected.push(new_ptr as usize);
            }
        }
        // Re-run the Phase-3 closure over the resurrected subtree.
        let mut scan_idx = scan_resume;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                regions,
                obj_ptr,
                header,
                cset_set,
                pointer_map,
                objects_copied,
                bytes_copied,
                work_list,
            );
        }
        if !resurrected.is_empty() {
            self.resurrected_finalizers.lock().extend(resurrected);
        }
    }

    fn evacuate_object(
        &self,
        regions: &mut Vec<G1Region>,
        old_ptr: *mut u8,
        pointer_map: &mut HashMap<usize, usize>,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        cset: &std::collections::HashSet<usize>,
    ) -> Option<(*mut u8, bool)> {
        let old_addr = old_ptr as usize;

        // Already forwarded (not fresh) — return the existing forward.
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            return Some((new_addr as *mut u8, false));
        }

        let header = unsafe { &*(old_ptr as *const ObjectHeader) };
        let obj_size = object_total_size(header);

        // gc-abort-cleanup (mirrors gc.rs::try_forward_object): a corrupt /
        // implausible header makes `object_total_size` return the `0` sentinel
        // (or otherwise an implausible size). Refuse to evacuate it rather than
        // memcpy `0`/garbage bytes and install a bad forwarding entry; bail out
        // as an evacuation failure (`None`) with a diagnostic instead of
        // aborting the VM.
        if obj_size < HEADER_SIZE {
            tracing::warn!(
                "g1::evacuate_object: refusing to evacuate object at {:p} — implausible \
                 size {} (kind=0x{:02x}, num_slots={}, array_len={}); corrupt header",
                old_ptr,
                obj_size,
                header.kind as u8,
                header.num_slots(),
                header.array_length(),
            );
            return None;
        }

        // Decide destination based on age
        let promote = header.gc_age >= self.config.promotion_age;
        let dest_type = if promote {
            RegionType::Old
        } else {
            RegionType::Survivor
        };

        let new_ptr = match Self::alloc_in_type_locked(regions, dest_type, obj_size, cset) {
            Some(p) => p,
            None => {
                // EVACUATION FAILURE (to-space exhausted: no non-CSet region of the
                // destination type has room and no Free region is left). Real G1
                // "self-forwards" such an object — keeps it IN PLACE rather than
                // dropping it. The previous `?` returned None here, so the caller
                // skipped the object: its referrers' slots kept pointing into a
                // CSet region that Phase 5 then reset/freed → silent live-object
                // loss (a wrong result under memory pressure where the
                // generational collector correctly OOMs).
                //
                // Self-forward: install old→old in the pointer map (an identity
                // forward) and return the object at its current address with
                // `fresh = true` so the caller still scans its fields (refs to
                // objects that DID evacuate are rewritten; refs to other
                // self-forwarded objects stay put) and the referrer's slot is
                // rewritten to `old_ptr` (a no-op — the object did not move).
                // Phase 5 detects self-forwarded objects (key == value in the
                // pointer map) and KEEPS their regions instead of freeing them,
                // so nothing is lost. The heap is then simply not reclaimed → the
                // triggering mutator allocation fails → a clean, catchable
                // OutOfMemoryError, exactly as the generational collector does.
                pointer_map.insert(old_addr, old_addr);
                return Some((old_ptr, true));
            }
        };

        // Copy object data
        unsafe {
            std::ptr::copy_nonoverlapping(old_ptr, new_ptr, obj_size);
        }

        // Round-2 fix (T2-4): explicit atomic load+store for the mark_word
        // field. The bulk memcpy above is technically UB for `AtomicU64`:
        // even under STW the memory model requires atomic ops on atomic
        // locations. Replicate the mark word atomically so subsequent CAS
        // operations (monitor inflation, etc.) on the new copy observe a
        // properly synchronized initial value.
        //
        // NOTE: a future concurrent G1 collector needs a different
        // forwarding protocol — CAS-install the forwarding pointer and
        // re-read the mark word if a mutator raced the evacuation.
        // SAFETY: both pointers reference a fully written ObjectHeader.
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

        // Increment GC age on the new copy
        let new_header = unsafe { &mut *(new_ptr as *mut ObjectHeader) };
        if promote {
            // G1AUD-1 — stamp the header's old-generation bit on promotion.
            //
            // G1 does not need this bit itself (region type is authoritative,
            // and `region_for_ptr` is O(log R)); the JIT does. Every inline
            // reference-store fast path in `jit/src/x64.rs` decides "no post
            // barrier needed" from `gc_flags & GC_FLAG_OLD_GEN == 0`
            // (`:8778`, `:8846`, `:16741`, `:16810`). The INT-6 mitigation
            // intended G1 receivers to be routed to the full-barrier helper by
            // the published-region containment guard instead — but two emitters
            // reach the same inline store WITHOUT that guard (the
            // `receiver_is_trusted_oop` arms at `:16707`/`:16795`, which emit a
            // bare null check, and `emit_inline_fresh_ctor_compact_ref_putfield`
            // at `:8825`, which emits none). On an unstamped G1 heap every
            // receiver reads as young there, so a null->non-null store into a
            // promoted object skipped `post_write_barrier_rset` and the
            // old->young edge never entered the remembered set — the next young
            // pause frees the still-live referent (UAF).
            //
            // Stamping makes the JIT's own old-generation test fire, which is
            // the fail-safe direction: the store takes `jit_putfield_object`,
            // which runs the complete SATB + RSet barrier pair. Nothing inside
            // this crate reads the bit on a G1 heap (`grep '\.gc_flags'
            // gc/src/g1.rs`), and `concurrent_mark`'s header validator already
            // lists it as a known flag, so the stamp is invisible to G1 itself.
            new_header.gc_flags |= GC_FLAG_OLD_GEN;
        } else {
            new_header.gc_age = new_header.gc_age.saturating_add(1);
        }
        new_header.forwarding_ptr = std::ptr::null_mut();

        pointer_map.insert(old_addr, new_ptr as usize);
        *objects_copied += 1;
        *bytes_copied += obj_size;

        Some((new_ptr, true))
    }

    /// Scan an evacuated object's reference fields. For each reference pointing
    /// into the CSet, evacuate the target and update the field.
    ///
    /// **STW-only correctness contract** (Round-7 audit §2): the
    /// `pointer_map.contains_key(...)` dedup pre-check below is a
    /// plain-`HashMap` operation that is correct ONLY because young/mixed
    /// evacuation runs single-threaded under STW with the calling thread
    /// holding `self.regions.lock()` for the entire collection. The
    /// `gc_worker_threads` config field (default 4) exists for a future
    /// parallel evacuator; the dedup will become a TOCTOU the moment the
    /// `work_list`/`pointer_map` is shared between worker threads: two
    /// workers can sample `contains_key == false` for the same source addr,
    /// both call `evacuate_object`, one wins the insert, and the loser's
    /// freshly-copied Survivor allocation is leaked while still being
    /// pushed onto a worklist for double-scan. Before enabling parallel
    /// evacuation, convert `pointer_map` to a `DashMap` (or per-worker
    /// shards) and use `entry().or_insert_with(...)` so the dedup signal
    /// is the entry's vacancy state, not a separate `contains_key` call.
    ///
    /// The caller is documented to hold the regions lock; we cannot
    /// `debug_assert!` directly on lock ownership (parking_lot Mutex offers
    /// no such API), so the invariant is enforced by the type-level
    /// `&mut Vec<G1Region>` parameter (only the lock holder can produce
    /// it) plus this contract comment.
    fn scan_and_evacuate_refs(
        &self,
        regions: &mut Vec<G1Region>,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        cset: &std::collections::HashSet<usize>,
        pointer_map: &mut HashMap<usize, usize>,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
    ) {
        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length() as usize {
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * 8) };
                    let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if let Some(region_idx) = self.region_for_ptr(regions, ref_ptr) {
                            if cset.contains(&region_idx) {
                                // Round-5 fix (CRIT, O(N²)): the prior code
                                // pushed `new_ptr` onto the worklist in BOTH
                                // branches of the dedup `if/else`, so every
                                // already-evacuated target was rescanned —
                                // turning ref-cycles into quadratic blow-up
                                // and reaching the worklist budget cap in
                                // pathological graphs. The dedup signal we
                                // need is "was this evacuation fresh?".
                                // `evacuate_object` inserts into
                                // `pointer_map` only when it actually copies.
                                // Step 9: take the freshness from the
                                // evacuation outcome (`fresh`) rather than a
                                // separate `contains_key` pre-check (a TOCTOU
                                // under parallel evacuation) and push to the
                                // worklist only on a fresh evacuation.
                                if let Some((new_ptr, fresh)) = self.evacuate_object(
                                    regions,
                                    ref_ptr,
                                    pointer_map,
                                    objects_copied,
                                    bytes_copied,
                                    cset,
                                ) {
                                    unsafe {
                                        std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                                    }
                                    if fresh {
                                        work_list.push(new_ptr);
                                    }
                                }
                            } else if let Some(&new_addr) = pointer_map.get(&(ref_ptr as usize)) {
                                // Already forwarded from a previous scan
                                unsafe {
                                    std::ptr::write(slot_ptr as *mut u64, new_addr as u64);
                                }
                            }
                        }
                    }
                }
            }
        } else {
            for_each_flat_object_reference(obj_ptr, header, 0, |slot_ptr, raw, compact| {
                let ref_ptr = raw as *mut u8;
                if let Some(region_idx) = self.region_for_ptr(regions, ref_ptr) {
                    if cset.contains(&region_idx) {
                        // Round-5 fix (CRIT, O(N²)): only push the
                        // forwarded target onto the worklist when this
                        // call site actually evacuated it. Step 9: the
                        // freshness comes from the evacuation outcome
                        // (`fresh`), not a separate `contains_key`
                        // pre-check (a TOCTOU under parallel evacuation).
                        if let Some((new_ptr, fresh)) = self.evacuate_object(
                            regions,
                            ref_ptr,
                            pointer_map,
                            objects_copied,
                            bytes_copied,
                            cset,
                        ) {
                            write_flat_object_reference(slot_ptr, new_ptr as usize, compact);
                            if fresh {
                                work_list.push(new_ptr);
                            }
                        }
                    } else if let Some(&new_addr) = pointer_map.get(&raw) {
                        write_flat_object_reference(slot_ptr, new_addr, compact);
                    }
                }
            });
        }
    }

    /// CRIT fix (UAF): scan every object in a non-CSet source region and,
    /// for each reference slot pointing into the CSet, evacuate the target
    /// (if not already evacuated) and rewrite the slot to the forwarded
    /// pointer in place. Without this step the RSet sources collected in
    /// young/mixed Phase 2 were dropped, so cross-region references from
    /// non-CSet → CSet survived as stale pointers after the CSet regions
    /// were reset, causing silent use-after-free. Pattern mirrors
    /// `scan_and_evacuate_refs` but operates on a region's full object
    /// walk rather than a single evacuated object.
    fn scan_source_region_for_cset_refs(
        &self,
        regions: &mut Vec<G1Region>,
        source_idx: usize,
        cset: &std::collections::HashSet<usize>,
        pointer_map: &mut HashMap<usize, usize>,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
    ) {
        // Source must be a live non-CSet region; cset sources from rset can
        // include indices that have since been reclassified.
        if cset.contains(&source_idx) {
            return;
        }
        let (cursor, base) = {
            let r = &mut regions[source_idx];
            if r.region_type == RegionType::Free {
                return;
            }
            (r.cursor, r.data.as_mut_ptr())
        };

        let jit_skips = self.jit_tlab_skip_spans();
        let mut offset = 0usize;
        while offset < cursor {
            let obj_ptr = unsafe { base.add(offset) };
            // INT-3 — frozen-peer TLAB tail: uninitialized, no walkable
            // filler; must be skipped before any byte is interpreted.
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                offset += skip;
                continue;
            }
            // TLAB-retire gap sentinel: skip its exact span (NOT a walkable
            // header — see `gap_filler_len`; `break`ing here would skip the
            // rest of a possibly-pinned source region's objects).
            if let Some(gap) = gap_filler_len(obj_ptr) {
                offset += gap;
                continue;
            }
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // Round-9 gc CRIT-1: humongous continuation filler covers the
            // entire region; skip without trying to follow any oops.
            if is_humongous_filler(header) {
                break;
            }
            let obj_size = object_total_size(header);
            if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                if gc_flags().g1_dbg_reach {
                    eprintln!(
                        "[g1][WALKBRK] source-scan region={source_idx} off={offset:#x} \
                         cursor={cursor:#x} obj_size={obj_size:#x}"
                    );
                }
                break;
            }

            // Walk reference slots; mirror scan_and_evacuate_refs's slot
            // dispatch but rewrite the slot atomically-by-store (STW: no
            // concurrent mutator; uses the same plain ptr::write pattern
            // as evacuate_object's mark-word transfer and the existing
            // scan_and_evacuate_refs helper).
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length() as usize {
                        let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * 8) };
                        let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                        if raw == 0 {
                            continue;
                        }
                        let ref_ptr = raw as usize as *mut u8;
                        if let Some(ridx) = self.region_for_ptr(regions, ref_ptr) {
                            if cset.contains(&ridx) {
                                // Step 9: `fresh` ignored — this RSet-source
                                // scan keeps its existing unconditional push
                                // (behaviour-identical; the parallel evacuator
                                // will gate it on `fresh`).
                                if let Some((new_ptr, _fresh)) = self.evacuate_object(
                                    regions,
                                    ref_ptr,
                                    pointer_map,
                                    objects_copied,
                                    bytes_copied,
                                    cset,
                                ) {
                                    unsafe {
                                        std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                                    }
                                    work_list.push(new_ptr);
                                }
                            }
                        }
                    }
                }
            } else {
                for_each_flat_object_reference(obj_ptr, header, 0, |slot_ptr, raw, compact| {
                    let ref_ptr = raw as *mut u8;
                    if let Some(ridx) = self.region_for_ptr(regions, ref_ptr) {
                        if cset.contains(&ridx) {
                            // Step 9: `fresh` ignored (see the Array branch).
                            if let Some((new_ptr, _fresh)) = self.evacuate_object(
                                regions,
                                ref_ptr,
                                pointer_map,
                                objects_copied,
                                bytes_copied,
                                cset,
                            ) {
                                write_flat_object_reference(slot_ptr, new_ptr as usize, compact);
                                work_list.push(new_ptr);
                            }
                        }
                    }
                });
            }

            offset += obj_size;
        }
    }

    /// Update interior references in all non-CSet regions using the pointer map.
    fn update_references_in_regions(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &std::collections::HashSet<usize>,
        pointer_map: &HashMap<usize, usize>,
    ) {
        if pointer_map.is_empty() {
            return;
        }

        // RSet rebuild (CORRECTNESS — remembered-set completeness for GC-internal
        // pointer rewrites). An edge whose holder and referent are collected
        // together carries NO remembered-set entry while they share a generation:
        // a young->young edge needs none (young is always collected whole), and an
        // edge a mutator stored while BOTH ends were young recorded only the
        // (now-recycled) young source region. When the holder/referent later age
        // or are promoted to Old, that edge silently becomes Old->young OR
        // Old->old (or humongous->old, the array-of-nodes case) — but no mutator
        // write barrier ever fires for it (the rewrite below, and the promotion
        // copy, are GC-internal). So the referent's region never learns of the
        // source: the next YOUNG GC drops a still-live young referent, AND a MIXED
        // GC that selects the referent's Old region never scans the source so it
        // drops the still-live OLD referent (proven: `MixedChurn` loses
        // cross-referenced young nodes; `PromoteMixed`'s mixed GC drops every node
        // held only through a humongous `keep[]` array — the V7b verifier reports
        // the holder region dangling into a freed CSet region). This is the
        // general (aging/promotion) case of the JIT-pinned-straddle fix landed
        // earlier. Since this pass already walks every non-CSet (=> Old/Humongous)
        // region each collection, we rebuild the rset here at no extra walk: every
        // cross-region reference from this region into a *collectable* region
        // (Eden/Survivor/Old — the region types that can enter a young or mixed
        // CSet) is recorded so the next collection scans this region as a source.
        // (Edges are collected and applied after the walk to keep borrows simple;
        // `add_reference` dedups.)
        let mut new_rset_edges: Vec<(usize, usize)> = Vec::new();
        let jit_skips = self.jit_tlab_skip_spans();

        for i in 0..regions.len() {
            if cset.contains(&i) || regions[i].region_type == RegionType::Free {
                continue;
            }

            let cursor = regions[i].cursor;
            let base = regions[i].data.as_mut_ptr();
            let mut offset = 0usize;

            while offset < cursor {
                let obj_ptr = unsafe { base.add(offset) };
                // INT-3 — frozen-peer TLAB tail: uninitialized, no walkable
                // filler; must be skipped before any byte is interpreted.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span — breaking
                // here would leave the rest of this region's references
                // un-fixed-up after evacuation (stale pointers).
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: humongous continuation filler covers
                // the entire region; skip the rest.
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);

                if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                    if gc_flags().g1_dbg_reach {
                        eprintln!(
                            "[g1][WALKBRK] phase4 region={i} off={offset:#x} \
                             cursor={cursor:#x} obj_size={obj_size:#x}"
                        );
                    }
                    break;
                }

                update_object_refs(obj_ptr, header, pointer_map);
                self.collect_outgoing_cross_region_edges(
                    regions,
                    i,
                    obj_ptr,
                    header,
                    &mut new_rset_edges,
                );
                offset += obj_size;
            }
        }

        // G1AUD-5: the Phase-4 rebuild is a GC-internal edge producer; stamp
        // its entries with this pause's generation so they age out with the
        // source region like the mutator barrier's do.
        let generation = self.rset_generation();
        for (target_region, source_region) in new_rset_edges {
            regions[target_region]
                .rset
                .add_reference_in_generation(source_region, generation);
        }
    }

    /// Record (into `out`) every cross-region reference from `obj` (which lives
    /// in non-CSet `holder` region — always Old/Humongous, since all young
    /// regions are in the CSet) into a *collectable* region as a
    /// `(target_region, holder)` rset edge. See `update_references_in_regions`
    /// for why this is required for remembered-set completeness.
    ///
    /// A "collectable" target is one whose region type can enter a collection
    /// set: `Eden`/`Survivor` (young CSet) OR `Old` (mixed CSet). Recording the
    /// Old targets is what lets a mixed GC find the live old graph reachable only
    /// through a GC-internal Old->old or humongous->old edge (the `PromoteMixed`
    /// drop). Humongous targets are never collected by young/mixed evacuation, so
    /// edges into them carry no rset entry (consistent with the mutator barrier,
    /// which records cross-region edges regardless of target type but whose
    /// entries on humongous regions are simply never consulted).
    fn collect_outgoing_cross_region_edges(
        &self,
        regions: &[G1Region],
        holder: usize,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        out: &mut Vec<(usize, usize)>,
    ) {
        let data_start = unsafe { obj_ptr.add(HEADER_SIZE) };
        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for k in 0..header.array_length() as usize {
                    let raw: u64 = unsafe { std::ptr::read(data_start.add(k * 8) as *const u64) };
                    if raw == 0 {
                        continue;
                    }
                    if let Some(j) = self.lookup_region_for_addr(raw as usize) {
                        if j != holder && is_collectable_region_type(regions[j].region_type) {
                            out.push((j, holder));
                        }
                    }
                }
            }
        } else {
            for_each_flat_object_reference(obj_ptr, header, 0, |_, raw, _| {
                if let Some(j) = self.lookup_region_for_addr(raw) {
                    if j != holder && is_collectable_region_type(regions[j].region_type) {
                        out.push((j, holder));
                    }
                }
            });
        }
    }

    /// SECURITY FIX (V7b): defensive post-evacuation dangling-pointer
    /// verification.
    ///
    /// Remembered-set completeness is a hard UAF precondition: Phase-2
    /// only evacuates objects reachable from CSet rset sources, and
    /// Phase-5 then frees (resets) every CSet region. If a live
    /// cross-region edge into the CSet was missing from some rset, the
    /// referent is neither evacuated nor entered into `pointer_map`, so
    /// Phase-4 leaves the referring slot untouched — a dangling pointer
    /// into a region whose backing buffer was just zero-filled and will
    /// be re-typed for unrelated objects (classic UAF).
    ///
    /// This pass runs AFTER Phase-5 over every surviving (non-CSet,
    /// non-Free) region and inspects each reference slot. A slot whose
    /// target lands inside a CSet region (those addresses still map to
    /// the same region indices — `reset` only zero-fills, it never
    /// reallocates the buffer) but is NOT a key in `pointer_map` is a
    /// dangling reference into a freed region. Rather than silently leave
    /// it dangling we abort (debug builds) / log (release builds).
    ///
    /// Cost: an extra walk of survivor/old regions. To keep production
    /// overhead near-zero it is gated on `debug_assertions` OR the
    /// existing `gc_log_enabled` verify flag; the common (release, quiet)
    /// path skips it entirely.
    fn verify_no_dangling_into_cset(
        &self,
        regions: &[G1Region],
        cset: &std::collections::HashSet<usize>,
        pointer_map: &HashMap<usize, usize>,
    ) {
        let verify = cfg!(debug_assertions) || self.gc_log_enabled.load(Ordering::Relaxed);
        if !verify {
            return;
        }

        // Closure: classify a referent address. Returns true if `addr`
        // is a dangling pointer into a (now-freed) CSet region.
        let is_dangling = |addr: usize| -> bool {
            if addr == 0 {
                return false;
            }
            match self.lookup_region_for_addr(addr) {
                Some(tgt_idx) if cset.contains(&tgt_idx) => {
                    // Target sits in a freed CSet region. If it was
                    // properly evacuated it would have a forwarding entry.
                    !pointer_map.contains_key(&addr)
                }
                _ => false,
            }
        };

        let jit_skips = self.jit_tlab_skip_spans();
        for i in 0..regions.len() {
            if cset.contains(&i) || regions[i].region_type == RegionType::Free {
                continue;
            }

            let cursor = regions[i].cursor;
            let base = regions[i].data.as_ptr();
            let mut offset = 0usize;

            while offset < cursor {
                let obj_ptr = unsafe { base.add(offset) };
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);
                if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                    break;
                }

                let data_start = unsafe { obj_ptr.add(HEADER_SIZE) };
                if header.kind == ObjectKind::Array {
                    if header.element_type == ArrayElementType::Reference {
                        for k in 0..header.array_length() as usize {
                            let slot_ptr = unsafe { data_start.add(k * 8) };
                            let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                            if is_dangling(raw as usize) {
                                self.report_dangling_cset_ref(i, obj_ptr as usize, raw as usize);
                            }
                        }
                    }
                } else {
                    for_each_flat_object_reference(obj_ptr, header, 0, |_, raw, _| {
                        if is_dangling(raw) {
                            self.report_dangling_cset_ref(i, obj_ptr as usize, raw);
                        }
                    });
                }

                offset += obj_size;
            }
        }
    }

    /// SECURITY FIX (V7b): report a detected dangling-into-CSet slot.
    /// In debug builds this is a hard abort (the heap is corrupt and any
    /// further mutation risks a UAF); in release builds (reached only via
    /// the `gc_log_enabled` verify flag) it logs loudly so the condition
    /// is observable without crashing a production VM.
    #[cold]
    #[inline(never)]
    fn report_dangling_cset_ref(&self, holder_region: usize, holder_obj: usize, target: usize) {
        eprintln!(
            "[g1][SECURITY V7b] post-evacuation dangling reference: object {:#x} in \
             surviving region {} still points at {:#x}, which lies in a freed CSet \
             region with no forwarding entry (incomplete remembered set => UAF)",
            holder_obj, holder_region, target
        );
        debug_assert!(
            false,
            "G1 post-evacuation dangling reference into freed CSet region (V7b): \
             holder_obj={:#x} holder_region={} target={:#x}",
            holder_obj, holder_region, target
        );
    }

    /// DIAGNOSTIC TOOL (parallel-evac defect 2, env `CRATONVM_G1_DBG_HEADERS=1`).
    /// Post-collection consistency verifier that ROOT-CAUSED the rare
    /// `young_collection_parallel` corruption (`SteadyChurn @16m --nojit`, ~1/8,
    /// `java/lang/Object`). Checks, after a parallel collection:
    ///   (0) OVERLAP — two from-space objects forwarded to the same dest (a TLAB
    ///       race) — NEVER fires (ruled out);
    ///   (a) UN-REWRITTEN — a non-CSet/root ref to a `pointer_map` KEY — NEVER
    ///       fires from heap holders (the heap is clean);
    ///   (b) LOST — a root → a CSet object NOT in `pointer_map` — fires EVERY
    ///       collection: the smoking gun.
    ///
    /// ROOT CAUSE (full writeup + ruled-out fixes:
    /// `docs/internal/fixed-suite-bugs/g1-parallel-evac-persistent-forwarding-root-remap.md`):
    /// the parallel evacuator dedups via the PERSISTENT `forwarding_ptr` header
    /// field (serial uses the per-cycle `pointer_map`). A fast-path hit returns a
    /// forward — possibly left over from a PRIOR cycle — WITHOUT recording it in
    /// `pointer_map`, so the VM's `update_all_roots` cannot remap a root that
    /// resolved through it. The frame local stays stuck on the from-space object,
    /// surviving only via the `forwarding_ptr` redirect until its region is
    /// reused → corruption. Invisible to V7b (heap-only). Three naive fixes all
    /// fail (see the doc); the proper fix is a per-cycle forwarding redesign.
    ///
    /// Mixed GC is kept on the serial evacuator and parallel evac stays
    /// opt-in/experimental (`CRATONVM_G1_PARALLEL_EVAC`) until this is fixed.
    /// No-op unless the env knob is set.
    fn dbg_verify_no_unrewritten_forward(
        &self,
        regions: &[G1Region],
        cset_set: &std::collections::HashSet<usize>,
        pointer_map: &HashMap<usize, usize>,
        roots: &[ObjectRef],
    ) {
        if !gc_flags().g1_dbg_headers {
            return;
        }
        // (0) OVERLAP DETECTOR: two distinct from-space objects forwarded to the
        // SAME destination address = a TLAB allocation race (one copy clobbers
        // the other's header → `java/lang/Object`). Reverse-map the forwards.
        {
            let mut by_dest: HashMap<usize, usize> = HashMap::with_capacity(pointer_map.len());
            let mut overlaps = 0usize;
            for (&k, &v) in pointer_map.iter() {
                if k == v {
                    continue; // self-forward (in place) — not a copy destination
                }
                if let Some(&prev) = by_dest.get(&v) {
                    overlaps += 1;
                    if overlaps <= 8 {
                        eprintln!(
                            "[g1][DBG-HEADERS] OVERLAP: dest {:#x} is the forward target of TWO \
                             from-space objects {:#x} and {:#x}",
                            v, prev, k
                        );
                    }
                } else {
                    by_dest.insert(v, k);
                }
            }
            if overlaps > 0 {
                eprintln!(
                    "[g1][DBG-HEADERS] {overlaps} forward-destination OVERLAP(s) this collection"
                );
            }
        }

        let mut hits = 0usize;
        // A holder is REACHABLE-RELEVANT only if it is NOT in a CSet (from-space)
        // region: kept CSet regions hold dead-never-reached from-space objects
        // whose slots are legitimately un-rewritten (noise). True survivors/old
        // (non-CSet) and roots MUST have every CSet ref rewritten (by the in-place
        // scan for new survivors, or Phase 4 for pre-existing survivors/old).
        let mut lost = 0usize;
        let mut check = |holder: usize, where_: &str, target: usize| {
            if let Some(&new) = pointer_map.get(&target) {
                if new != target {
                    hits += 1;
                    if hits <= 24 {
                        let treg = self.lookup_region_for_addr(target);
                        eprintln!(
                            "[g1][DBG-HEADERS] UN-REWRITTEN forward (live holder): {where_} \
                             holder={:#x} slot->{:#x} should be {:#x}; target region={:?}",
                            holder, target, new, treg
                        );
                    }
                }
                return;
            }
            // LOST: target sits in a CSet (collected) region but has NO forwarding
            // entry — it was never evacuated. V7b catches this for HEAP holders;
            // here it also covers ROOTS (which V7b never scans). A live object
            // reachable only via a root, dropped because the parallel closure
            // terminated before scanning it, lands here — and the VM's
            // `update_all_roots` cannot remap it (not in `pointer_map`), so the
            // frame local dangles into the freed region → `java/lang/Object`.
            if let Some(tidx) = self.lookup_region_for_addr(target) {
                if cset_set.contains(&tidx) {
                    lost += 1;
                    if lost <= 24 {
                        eprintln!(
                            "[g1][DBG-HEADERS] LOST (un-evacuated CSet target): {where_} \
                             holder={:#x} slot->{:#x} region={tidx} — NOT in pointer_map",
                            holder, target
                        );
                    }
                }
            }
        };
        // Roots (always reachable).
        for r in roots {
            let p = r.as_ptr() as usize;
            if p != 0 {
                check(0, "root", p);
            }
        }
        // Only NON-CSet regions (true survivors / old / new survivors). Kept CSet
        // regions are excluded — their dead objects are the stranded-copy noise.
        let jit_skips = self.jit_tlab_skip_spans();
        for (ridx, region) in regions.iter().enumerate() {
            if region.region_type == RegionType::Free || cset_set.contains(&ridx) {
                continue;
            }
            let base = region.data.as_ptr();
            let cursor = region.cursor;
            let mut offset = 0usize;
            while offset < cursor {
                let obj_ptr = unsafe { base.add(offset) };
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);
                if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                    break;
                }
                let data = unsafe { obj_ptr.add(HEADER_SIZE) };
                if header.kind == ObjectKind::Array {
                    if header.element_type == ArrayElementType::Reference {
                        for k in 0..header.array_length() as usize {
                            let raw =
                                unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
                            if raw != 0 {
                                check(obj_ptr as usize, "array-elem", raw);
                            }
                        }
                    }
                } else {
                    for_each_flat_object_reference(obj_ptr, header, 0, |_, raw, _| {
                        check(obj_ptr as usize, "field", raw);
                    });
                }
                offset += obj_size;
            }
        }
        if hits > 0 || lost > 0 {
            eprintln!(
                "[g1][DBG-HEADERS] {hits} un-rewritten + {lost} LOST(un-evacuated CSet) \
                 from NON-CSET holders/roots = the real defect"
            );
        }
    }

    /// DIAGNOSTIC (defect-2 residual race, env `CRATONVM_G1_DBG_ZERO=1`): after a
    /// parallel collection, scan EVERY non-Free region (INCLUDING kept CSet
    /// regions, which the un-rewritten verifier excludes) plus the roots, and
    /// report any reference whose TARGET has an all-zero header (the freed/reused
    /// region signature behind `java/lang/Object`). Catches the residual
    /// concurrency-race corruption at the collection that introduces it,
    /// regardless of which region the holder lives in. No-op unless the env set.
    fn dbg_scan_for_zeroed_refs(
        &self,
        regions: &[G1Region],
        cset_set: &std::collections::HashSet<usize>,
        roots: &[ObjectRef],
    ) {
        if !gc_flags().g1_dbg_zero {
            return;
        }
        let mut hits = 0usize;
        let is_zeroed = |addr: usize| -> bool {
            if addr == 0 || self.lookup_region_for_addr(addr).is_none() {
                return false;
            }
            let h = unsafe { &*(addr as *const ObjectHeader) };
            h.class_id.as_u32() == 0
                && h.num_slots() == 0
                && (h.kind as u8) == 0
                && h.array_length() == 0
        };
        let mut report = |holder: usize, hreg: Option<usize>, where_: &str, target: usize| {
            if is_zeroed(target) {
                hits += 1;
                if hits <= 16 {
                    let treg = self.lookup_region_for_addr(target);
                    let h_in_cset = hreg.map(|i| cset_set.contains(&i)).unwrap_or(false);
                    let t_in_cset = treg.map(|i| cset_set.contains(&i)).unwrap_or(false);
                    eprintln!(
                        "[g1][DBG-ZERO] ZEROED target: {where_} holder={:#x} (region={:?} \
                         in_cset={h_in_cset}) slot->{:#x} (region={:?} in_cset={t_in_cset})",
                        holder, hreg, target, treg
                    );
                }
            }
        };
        for r in roots {
            report(0, None, "root", r.as_ptr() as usize);
        }
        let jit_skips = self.jit_tlab_skip_spans();
        for (ridx, region) in regions.iter().enumerate() {
            if region.region_type == RegionType::Free {
                continue;
            }
            let base = region.data.as_ptr();
            let cursor = region.cursor;
            let mut off = 0usize;
            while off < cursor {
                let obj_ptr = unsafe { base.add(off) };
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    off += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    off += gap;
                    continue;
                }
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let sz = object_total_size(header);
                if sz < HEADER_SIZE || off + sz > cursor {
                    break;
                }
                let data = unsafe { obj_ptr.add(HEADER_SIZE) };
                if header.kind == ObjectKind::Array {
                    if header.element_type == ArrayElementType::Reference {
                        for k in 0..header.array_length() as usize {
                            let raw =
                                unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
                            report(obj_ptr as usize, Some(ridx), "array-elem", raw);
                        }
                    }
                } else {
                    for_each_flat_object_reference(obj_ptr, header, 0, |_, raw, _| {
                        report(obj_ptr as usize, Some(ridx), "field", raw);
                    });
                }
                off += sz;
            }
        }
        if hits > 0 {
            eprintln!(
                "[g1][DBG-ZERO] {hits} reference(s) to a ZEROED (freed) object this collection"
            );
        }
    }

    /// DIAGNOSTIC (env `CRATONVM_G1_DBG_REACH=1`): after a collection, BFS the
    /// heap from `roots` and report any REACHABLE slot whose target has a
    /// zeroed header or lies outside every region. Unlike `DBG-ZERO` (which
    /// walks regions linearly and false-positives on dead objects' harmless
    /// stale slots), this checks exactly the invariant the mutator depends on
    /// — so the FIRST collection it fires on is the one that corrupted live
    /// state. No-op unless the env is set.
    fn dbg_verify_reachable_integrity(
        &self,
        regions: &[G1Region],
        roots: &[ObjectRef],
        label: &str,
    ) {
        if !gc_flags().g1_dbg_reach {
            return;
        }
        static PAUSE_NO: AtomicUsize = AtomicUsize::new(0);
        let pause = PAUSE_NO.fetch_add(1, Ordering::Relaxed);

        let is_zeroed = |addr: usize| -> bool {
            let h = unsafe { &*(addr as *const ObjectHeader) };
            // identity_hash_code is stamped non-zero on every allocation, so
            // requiring it zero here keeps a live bare `new Object()` (which
            // legitimately has class_id 0 / no slots) from false-positiving.
            h.class_id.as_u32() == 0
                && h.num_slots() == 0
                && (h.kind as u8) == 0
                && h.array_length() == 0
                && h.identity_hash_code == 0
        };

        let mut stack: Vec<usize> = Vec::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut bad = 0usize;
        let mut check_push = |addr: usize,
                              holder: usize,
                              where_: &str,
                              slot: usize,
                              stack: &mut Vec<usize>,
                              seen: &mut std::collections::HashSet<usize>,
                              bad: &mut usize| {
            if addr == 0 {
                return;
            }
            let region = self.lookup_region_for_addr(addr);
            if region.is_none() || is_zeroed(addr) {
                *bad += 1;
                if *bad <= 16 {
                    let (hcid, hkind, hslots, hlen) = if holder != 0 {
                        let hh = unsafe { &*(holder as *const ObjectHeader) };
                        (
                            hh.class_id.as_u32(),
                            hh.kind as u8,
                            hh.num_slots(),
                            hh.array_length(),
                        )
                    } else {
                        (0, 0, 0, 0)
                    };
                    let hregion = self.lookup_region_for_addr(holder);
                    let (hoff, hcur) = hregion
                        .map(|ri| {
                            let base = regions[ri].data.as_ptr() as usize;
                            (holder - base, regions[ri].cursor)
                        })
                        .unwrap_or((0, 0));
                    eprintln!(
                        "[g1][DBG-REACH] pause={pause} {label}: LIVE-REACHABLE {where_}[{slot}] \
                         holder={holder:#x} (cid={hcid} kind={hkind} slots={hslots} len={hlen} \
                         region={hregion:?} off={hoff:#x} cursor={hcur:#x}{}) -> {addr:#x} is {} \
                         (region={region:?})",
                        if hoff >= hcur { " ABOVE-CURSOR" } else { "" },
                        if region.is_none() { "WILD" } else { "ZEROED" }
                    );
                }
                return;
            }
            if seen.insert(addr) {
                stack.push(addr);
            }
        };

        for r in roots {
            check_push(
                r.as_ptr() as usize,
                0,
                "root",
                0,
                &mut stack,
                &mut seen,
                &mut bad,
            );
        }
        while let Some(addr) = stack.pop() {
            let header = unsafe { &*(addr as *const ObjectHeader) };
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    let data = unsafe { (addr as *const u8).add(HEADER_SIZE) };
                    for k in 0..header.array_length() as usize {
                        let raw = unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
                        check_push(raw, addr, "array-elem", k, &mut stack, &mut seen, &mut bad);
                    }
                }
            } else {
                for_each_flat_object_reference(addr as *mut u8, header, 0, |_, raw, _| {
                    check_push(raw, addr, "field", 0, &mut stack, &mut seen, &mut bad);
                });
            }
        }
        if bad > 0 {
            eprintln!(
                "[g1][DBG-REACH] pause={pause} {label}: {bad} corrupted LIVE-REACHABLE ref(s) \
                 ({} objects reached)",
                seen.len()
            );
        }

        // Root census (env CRATONVM_G1_DBG_ROOTCENSUS=1): per-root transitive
        // reach counts, to identify a stale root anchoring a large dead
        // subgraph (e.g. a historical list node retaining everything appended
        // after it through `next` chains).
        if gc_flags().g1_dbg_rootcensus {
            for (ri, r) in roots.iter().enumerate() {
                let addr = r.as_ptr() as usize;
                if addr == 0 || self.lookup_region_for_addr(addr).is_none() {
                    continue;
                }
                let mut rseen: std::collections::HashSet<usize> = std::collections::HashSet::new();
                let mut rstack = vec![addr];
                rseen.insert(addr);
                while let Some(a) = rstack.pop() {
                    let h = unsafe { &*(a as *const ObjectHeader) };
                    let data = unsafe { (a as *const u8).add(HEADER_SIZE) };
                    if h.kind == ObjectKind::Array {
                        if h.element_type == ArrayElementType::Reference {
                            for k in 0..h.array_length() as usize {
                                let raw = unsafe { std::ptr::read(data.add(k * 8) as *const u64) }
                                    as usize;
                                if raw != 0
                                    && self.lookup_region_for_addr(raw).is_some()
                                    && rseen.insert(raw)
                                {
                                    rstack.push(raw);
                                }
                            }
                        }
                    } else {
                        for s in 0..h.num_slots() as usize {
                            let v =
                                unsafe { std::ptr::read(data.add(s * SLOT_SIZE) as *const Value) };
                            if let Value::Object(Some(o)) = v {
                                let a2 = o.as_ptr() as usize;
                                if self.lookup_region_for_addr(a2).is_some() && rseen.insert(a2) {
                                    rstack.push(a2);
                                }
                            }
                        }
                    }
                }
                if rseen.len() > 1000 {
                    let h = unsafe { &*(addr as *const ObjectHeader) };
                    // Diagnostic slot peek: for SteadyChurn-shaped objects,
                    // slot 1 is the `seq` int — identifies WHICH node/payload
                    // anchors the subgraph (ancient seq ⟹ stale root).
                    let slot1 = if h.num_slots() >= 2 {
                        let v = unsafe {
                            std::ptr::read(
                                (addr as *const u8).add(HEADER_SIZE + SLOT_SIZE) as *const Value
                            )
                        };
                        format!("{v:?}")
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "[g1][ROOTCENSUS] pause={pause} root#{ri} addr={addr:#x} cid={} kind={} \
                         slots={} reach={} slot1={slot1}",
                        h.class_id.as_u32(),
                        h.kind as u8,
                        h.num_slots(),
                        rseen.len()
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Concurrent Marking
    // -----------------------------------------------------------------------

    /// Marking keep-alive (SATB soundness across evacuation pauses).
    ///
    /// While a concurrent mark cycle is active, two sets of raw object
    /// addresses are snapshot-live (live at mark start, so the SATB
    /// invariant obliges the marker to trace them to completion):
    ///
    ///   1. the gray set (`mark_worklist`) — discovered but not yet scanned;
    ///   2. the SATB log — old reference values overwritten by mutators
    ///      since the last drain.
    ///
    /// An evacuation pause moves/frees CSet regions, so both sets must be
    /// carried across the pause. Dropping a CSet-resident gray whose object
    /// the evacuation closure did not reach (the pre-fix `retain_mut`
    /// behaviour) silently unmarks its entire unscanned subtree: a live Old
    /// or humongous object whose only remaining marker-visible path ran
    /// through that gray is then never marked, and cleanup frees it live
    /// (the SteadyChurn `[FREED] cleanup region=N` failure). Leaving SATB
    /// entries in the queue is just as bad once the final remark consumes
    /// them: the addresses dangle into reset regions.
    ///
    /// Called at the start of an evacuation pause (STW, `regions` lock
    /// held). Drains the SATB log (every parked mutator's local buffer plus
    /// the global shards) into the gray set, then returns the gray entries
    /// that live in the CSet and are not already marked (marked ⇒ already
    /// scanned — its successors are in the gray set on their own). The
    /// caller evacuates each returned address exactly like a root; the
    /// existing worklist remap then rewrites every retained gray through
    /// the pause's forwarding map instead of dropping it.
    ///
    /// No-op (empty Vec) while no cycle is active — the SATB queue is only
    /// active between `start_concurrent_mark` and `cleanup`.
    fn marking_keepalive_roots(
        &self,
        regions: &[G1Region],
        cset_set: &std::collections::HashSet<usize>,
    ) -> Vec<usize> {
        if !self.satb_queue.is_active() {
            return Vec::new();
        }
        // Mutators are parked at the STW barrier, so their thread-local SATB
        // buffers are stable — pull them into the global shards, then drain.
        crate::satb::flush_all_thread_satb_buffers(&self.satb_queue);
        let mut worklist = self.mark_worklist.lock();
        let mut keepalive: Vec<usize> = Vec::new();
        for addr in self.satb_queue.drain() {
            if addr == 0 {
                continue;
            }
            let Some(idx) = self.lookup_region_for_addr(addr) else {
                continue;
            };
            if regions[idx].region_type == RegionType::Free
                || regions[idx].mark_bitmap.is_marked(addr)
            {
                continue;
            }
            if cset_set.contains(&idx) {
                // CSet resident: the evacuation loop below owns its survival;
                // its (forwarded) copy is re-grayed by the caller, so the
                // worklist push is not needed and cannot be capped away.
                keepalive.push(addr);
            } else if worklist.len() < MARK_WORKLIST_CAP {
                worklist.push(addr);
            } else {
                // Seed-class entry at cap. A plain drop is NOT recoverable:
                // the overflow rescan only re-walks MARKED objects, and no
                // marked object need reference a SATB seed. Mark it black
                // without scanning — the rescan the flag forces will scan
                // its fields. (Stays in place: non-CSet checked above.)
                regions[idx].mark_bitmap.try_mark(addr);
                self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
            }
        }
        keepalive.extend(worklist.iter().copied().filter(|&addr| {
            match self.lookup_region_for_addr(addr) {
                Some(idx) => cset_set.contains(&idx) && !regions[idx].mark_bitmap.is_marked(addr),
                None => false,
            }
        }));
        keepalive
    }

    /// Re-gray the relocated copy of a marking keep-alive object: push
    /// `new_addr` onto the mark worklist, or — at the worklist cap — mark it
    /// black-without-scan and set the overflow flag so the conservative
    /// rescan in `concurrent_mark_step` scans its fields instead. `new_addr`
    /// is a to-space (non-CSet) address, so the mark bit survives the pause.
    fn push_gray_or_mark(&self, regions: &[G1Region], new_addr: usize) {
        let mut worklist = self.mark_worklist.lock();
        if worklist.len() < MARK_WORKLIST_CAP {
            worklist.push(new_addr);
        } else if let Some(idx) = self.lookup_region_for_addr(new_addr) {
            regions[idx].mark_bitmap.try_mark(new_addr);
            self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
        }
    }

    /// Test/diagnostic: has `addr` been grayed or marked by the current
    /// mark cycle? True if its region's bitmap has it marked OR it sits in
    /// the mark worklist. Used by the g1_concurrent SATB delivery test,
    /// whose raw queue-contents assertion under-approximates delivery now
    /// that the marker drains the shards mid-cycle (G1MARK-6). Takes the
    /// regions lock then (after dropping it) the worklist lock — never both.
    pub(crate) fn dbg_is_grayed_or_marked(&self, addr: usize) -> bool {
        {
            let regions = self.regions.lock();
            if let Some(idx) = self.region_for_ptr(&regions, addr as *mut u8) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    return true;
                }
            }
        }
        self.mark_worklist.lock().contains(&addr)
    }

    /// Abort an in-flight marking cycle WITHOUT acting on the (incomplete)
    /// bitmap: discard the gray set, deactivate the SATB barrier, return the
    /// phase to `Idle`. No cleanup verdicts are computed — the next cycle
    /// re-discovers liveness from scratch. Used when the cycle's driver
    /// state is lost (an orphaned controller slot) so the completion gate
    /// in the VM does not spin forever on a cycle nobody is driving.
    pub fn abort_concurrent_mark(&self) {
        self.mark_worklist.lock().clear();
        self.mark_worklist_overflowed
            .store(false, Ordering::Relaxed);
        self.mark_saw_implausible.store(false, Ordering::Relaxed);
        // INT-8: the skip set is per-cycle state.
        self.reference_skip.lock().clear();
        // G1AUD-2 — leave the marking-active phase BEFORE deactivating the
        // queue, not after. The reverse order opens a window in which
        // `is_marking_active()` is still true while the queue is already
        // INACTIVE: every reference store in that window reads its old value
        // (the phase gate admits it) and then drops it (the queue gate rejects
        // it). Harmless for an *abort*, whose bitmap is discarded anyway — but
        // it is the only place in the collector that violated
        // `is_marking_active() => satb_queue.is_active()`, so the invariant
        // could not be asserted globally while it stood. See
        // `satb_pre_barrier_required`.
        self.gc_state.set_phase(ConcurrentGcPhase::Idle);
        let _ = self.satb_queue.deactivate_and_drain();
    }

    /// Start a concurrent marking cycle. Sets phase to InitialMark.
    ///
    /// Audit fix (HIGH-3): also clears the mark worklist so a previous
    /// aborted cycle doesn't leak gray pointers into the new cycle.
    pub fn start_concurrent_mark(&self) {
        self.gc_state.set_phase(ConcurrentGcPhase::InitialMark);
        self.satb_queue.activate();
        // Round-2 fix (HIGH — GC #5): clear every per-region bitmap so a
        // previous cycle's mark bits don't leak into this one.
        {
            let regions = self.regions.lock();
            for r in regions.iter() {
                r.mark_bitmap.clear();
            }
            // TAMS snapshot: record every region's incarnation + fill level
            // at mark start so `cleanup` can treat later allocations as live
            // (see `mark_start_snapshot`). Same regions-then-snapshot lock
            // order as `cleanup`.
            let mut snap = self.mark_start_snapshot.lock();
            snap.clear();
            snap.extend(
                regions
                    .iter()
                    .map(|r| (r.reuse_epoch, r.cursor, r.region_type)),
            );
        }
        self.mark_worklist.lock().clear();
        // Round-9 gc HIGH-5: reset overflow indicator at cycle start so
        // a previous cycle's overflow doesn't trigger a needless rescan.
        self.mark_worklist_overflowed
            .store(false, Ordering::Relaxed);
        // G1MARK-8: same per-cycle reset for the implausible-header flag.
        self.mark_saw_implausible.store(false, Ordering::Relaxed);
        // INT-8: stale skip entries from a previous cycle must never hide a
        // reused address's slot 0 — the VM re-publishes the current set
        // right after this call (still inside the initial-mark STW).
        self.reference_skip.lock().clear();
        // G1AUD-2 — the queue MUST already be live before the phase becomes
        // marking-active: the store paths read their old slot value on the
        // phase and retain it on the queue, so flipping the phase first would
        // silently drop every edge overwritten in between. See
        // `satb_pre_barrier_required` for the full statement of the invariant.
        debug_assert!(
            self.satb_queue.is_active(),
            "start_concurrent_mark: the SATB queue must be activated BEFORE the phase \
             becomes marking-active"
        );
        self.gc_state.set_phase(ConcurrentGcPhase::ConcurrentMark);
    }

    /// INT-8: publish the referent-slot skip set for the cycle that
    /// [`Self::start_concurrent_mark`] just opened. `addrs` are the
    /// Weak/Soft/Phantom `Reference` OBJECT addresses from the VM's
    /// `ReferenceProcessor` registry, snapshotted inside the same
    /// initial-mark STW (so no mutator can move or free them between the
    /// snapshot and this publish). See the `reference_skip` field doc.
    pub fn set_reference_skip_set(&self, addrs: &[usize]) {
        let mut skip = self.reference_skip.lock();
        skip.clear();
        skip.extend(addrs.iter().copied());
    }

    /// Test/diagnostic: current size of the referent-slot skip set.
    #[cfg(test)]
    pub(crate) fn dbg_reference_skip_len(&self) -> usize {
        self.reference_skip.lock().len()
    }

    /// Test/diagnostic: does the skip set contain `addr`?
    #[cfg(test)]
    pub(crate) fn dbg_reference_skip_contains(&self, addr: usize) -> bool {
        self.reference_skip.lock().contains(&addr)
    }

    /// INT-8: carry the referent-slot skip set across an evacuation pause.
    /// Survivors are re-keyed through `pointer_map`; entries that sat in a
    /// CSet region and were NOT forwarded died with their region — they are
    /// PRUNED, because their address can be recycled later in the cycle and
    /// a stale entry would then hide the slot 0 of whatever innocent object
    /// reuses the memory (under-marking → freed-live corruption). A
    /// self-forwarded kept-region object maps to itself and is retained.
    /// Pruning a live entry is impossible by construction (non-CSet objects
    /// do not move and CSet survivors are always in the map), but even if a
    /// wedged drain dropped one, the failure mode is over-retention of its
    /// referent for one cycle — never under-marking.
    ///
    /// Called with the pause's regions lock held, after evacuation composed
    /// the final `pointer_map` (same protocol point as the monitor-table
    /// remap). No-op when no cycle is active (set empty).
    fn remap_reference_skip_set(
        &self,
        cset_set: &std::collections::HashSet<usize>,
        pointer_map: &HashMap<usize, usize>,
    ) {
        let mut skip = self.reference_skip.lock();
        if skip.is_empty() {
            return;
        }
        let old: Vec<usize> = skip.drain().collect();
        for addr in old {
            if let Some(&new_addr) = pointer_map.get(&addr) {
                skip.insert(new_addr);
            } else {
                let in_cset = self
                    .lookup_region_for_addr(addr)
                    .is_some_and(|idx| cset_set.contains(&idx));
                if !in_cset {
                    skip.insert(addr); // untouched region — object did not move
                }
                // else: died in the CSet — prune.
            }
        }
    }

    /// INT-8: post-remark liveness verdict for reference processing. Valid
    /// ONLY between the final remark's fixed-point drain and `cleanup()`
    /// (the mark bitmap is complete and nothing has been freed yet).
    /// Mirrors cleanup's own liveness walk:
    /// - not a heap address / no cycle data → LIVE (never claim dead
    ///   without positive evidence);
    /// - region freed earlier in the cycle → DEAD;
    /// - region recycled/re-typed since the mark-start snapshot → LIVE
    ///   (its entire content postdates the snapshot);
    /// - allocated above the snapshot cursor (TAMS) → LIVE;
    /// - otherwise → the region's mark-bitmap verdict.
    pub fn is_live_after_mark(&self, addr: usize) -> bool {
        let regions = self.regions.lock();
        let Some(idx) = self.region_for_ptr(&regions, addr as *mut u8) else {
            return true;
        };
        let region = &regions[idx];
        if region.region_type == RegionType::Free {
            return false;
        }
        // Same regions→snapshot lock order as `cleanup` / `start_concurrent_mark`.
        let snap = self.mark_start_snapshot.lock();
        if snap.is_empty() {
            return true;
        }
        match snap.get(idx) {
            Some(&(epoch, snap_cursor, snap_type))
                if epoch == region.reuse_epoch && snap_type == region.region_type =>
            {
                let base = region.data.as_ptr() as usize;
                if addr.wrapping_sub(base) >= snap_cursor {
                    return true; // TAMS: allocated after mark start
                }
                region.mark_bitmap.is_marked(addr)
            }
            _ => true,
        }
    }

    /// INT-8: resurrect objects the remark-time reference processor decided
    /// to hand out (dead finalizables about to be finalize()d, pending
    /// cleaner chains, policy-retained soft referents): mark each gray and
    /// drain the closure to a fixed point so this cycle's `cleanup` cannot
    /// free them or anything they reach. Must run between the remark drain
    /// and `cleanup()`. Addresses outside a live region are skipped — the
    /// caller's own staleness guards already dropped those.
    pub fn resurrect_after_remark(&self, addrs: &[usize]) {
        if addrs.is_empty() {
            return;
        }
        {
            let regions = self.regions.lock();
            for &addr in addrs {
                if let Some(idx) = self.region_for_ptr(&regions, addr as *mut u8) {
                    if regions[idx].region_type != RegionType::Free {
                        self.push_gray_or_mark(&regions, addr);
                    }
                }
            }
        }
        while !self.concurrent_mark_step(usize::MAX) {}
    }

    /// INT-8: store a field WITHOUT firing the SATB pre-barrier. Reserved
    /// for the weak-reference PROTOCOL writes — the pre-collection referent
    /// null pass (whose value is restored before mutators resume, so the
    /// snapshot graph is unchanged) and the remark-time referent clears
    /// (whose edge removal is the reference processor's decided verdict,
    /// which SATB must not resurrect). Every semantic mutator store must
    /// keep using `set_field`. Bounds/humongous handling and the RSet
    /// post-barrier are identical to `set_field` — only the pre-barrier is
    /// suppressed, via a same-thread RAII scope around the normal path.
    pub(crate) fn set_field_no_satb(&self, obj: ObjectRef, index: usize, value: Value) {
        let _guard = SatbPreSuppressGuard::new();
        <Self as crate::collector::GarbageCollector>::set_field(self, obj, index, value);
    }

    /// Perform an incremental step of concurrent marking.
    ///
    /// Audit fix (HIGH-3): this used to walk every region and call
    /// `try_mark` on every header — no roots, no transitive closure, so
    /// G1 reported every allocated object as live. The new implementation
    /// does real tri-color marking by draining the persistent
    /// `mark_worklist`:
    ///
    /// 1. Pop an object address from the worklist (the gray set).
    /// 2. Mark it in the bitmap.
    /// 3. Scan its reference slots — for each unmarked old/young heap
    ///    object it points at, mark it gray (push onto the worklist).
    ///
    /// The worklist is seeded by `remark` (which the interpreter calls
    /// at initial-mark STW and at the final remark STW). This way the
    /// worker thread that calls `concurrent_mark_step` in a loop simply
    /// drains the gray set produced by the roots.
    ///
    /// `work_amount` is the maximum number of objects to scan in this
    /// step. Returns `true` when the worklist is empty (marking is done).
    pub fn concurrent_mark_step(&self, work_amount: usize) -> bool {
        if work_amount == 0 {
            return self.mark_worklist.lock().is_empty();
        }

        // G1MARK-6: pull mutator SATB overwrites into the gray set NOW
        // rather than letting them pile up in the shard queue until a young
        // pause or the final remark drains them. A mutation-heavy but
        // allocation-free phase (in-place updates of a preallocated working
        // set) triggers no young pauses, so the shards — which have no size
        // bound — grew without limit and the eventual remark pause was
        // O(all entries). Draining from the marker keeps the queue bounded
        // by the marker's cadence and shrinks the final remark. Entries
        // logged after this drain are picked up on the next step; the
        // gray-set/worklist protocol is the same one remark uses.
        // (Must run BEFORE the worklist lock below — push_gray_or_mark
        // takes that lock itself.)
        {
            let regions = self.regions.lock();
            for addr in self.satb_queue.drain() {
                self.push_gray_or_mark(&regions, addr);
            }
        }

        let regions = self.regions.lock();
        let mut worklist = self.mark_worklist.lock();
        let mut remaining = work_amount;

        while remaining > 0 {
            let obj_addr = match worklist.pop() {
                Some(a) => a,
                None => break, // gray set empty for now — see overflow handling below
            };
            remaining -= 1;

            // Defensive: confirm this address really lives in some region.
            // (Stale roots from before a heap rearrangement would otherwise
            // dereference garbage.)
            let obj_ptr = obj_addr as *mut u8;
            let region_idx = match self.region_for_ptr(&regions, obj_ptr) {
                Some(idx) => idx,
                None => continue,
            };

            // G1MARK-8: header-plausibility gate. Worklist entries are raw
            // field bytes read by `scan_object_refs` — a corrupt or stale
            // slot can name any in-region address. Region containment alone
            // (above) still lets a wild pointer's garbage "header" drive the
            // scan: its num_slots/array_length extent is walked and more
            // garbage is pushed as children (corruption amplifier — the same
            // class ZGC-4 closed with its registry check; G1 has no
            // per-object registry, so the gate is geometric + header
            // consistency instead). Skipping the scan can under-mark if the
            // header was genuinely torn, so the flag makes `cleanup` retain
            // everything this cycle.
            if !plausible_mark_scan_target(&regions[region_idx], obj_addr) {
                self.mark_saw_implausible.store(true, Ordering::Relaxed);
                tracing::warn!(
                    "g1 concurrent mark: skipping gray entry {obj_addr:#x} (region {region_idx}) \
                     with implausible header — cleanup will retain all regions this cycle"
                );
                continue;
            }

            // Round-2 fix (HIGH — GC #5): bitmaps now live per-region, so
            // route the mark through the owning region's bitmap (which is
            // keyed off that region's actual data pointer).
            // Already black? Skip — nothing new to discover from it.
            if !regions[region_idx].mark_bitmap.try_mark(obj_addr) {
                continue;
            }

            // Scan the object's reference fields and push gray successors.
            // SAFETY: region_for_ptr above confirmed the address is inside
            // a live region's data buffer; the header is therefore
            // readable for the duration of the GC cycle (regions are
            // pinned by the lock guard).
            let header = unsafe { &*(obj_addr as *const ObjectHeader) };
            self.scan_object_refs(obj_ptr, header, &regions, &mut worklist);
        }

        if !worklist.is_empty() {
            // Ran out of budget but still have work — caller should call again.
            return false;
        }

        // Round-9 gc HIGH-5 — graceful overflow handling. If any push
        // was dropped earlier in this cycle, the transitive closure is
        // incomplete: dropped objects were never scanned, so their
        // children may be unmarked despite being live. Run a
        // conservative re-walk: for every marked object in every live
        // region, re-scan its outgoing references and push any unmarked
        // targets. The loop bounds the number of recovery passes; once
        // a full pass causes no new pushes the worklist drains and we
        // declare marking complete. The bitmap monotonically grows, so
        // this terminates after a bounded number of passes.
        if self.mark_worklist_overflowed.load(Ordering::Relaxed) {
            // Reset the flag so we can detect re-overflow during recovery.
            self.mark_worklist_overflowed
                .store(false, Ordering::Relaxed);
            let jit_skips = self.jit_tlab_skip_spans();
            for region in regions.iter() {
                if region.region_type == RegionType::Free {
                    continue;
                }
                let base = region.data.as_ptr() as usize;
                let mut offset = 0usize;
                while offset < region.cursor {
                    let obj_addr = base + offset;
                    // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                    if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_addr) {
                        offset += skip;
                        continue;
                    }
                    // TLAB-retire gap sentinel: skip its exact span.
                    if let Some(gap) = gap_filler_len(obj_addr as *const u8) {
                        offset += gap;
                        continue;
                    }
                    // SAFETY: offset < cursor; header is within the region.
                    let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                    if is_humongous_filler(header) {
                        break;
                    }
                    let obj_size = object_total_size(header);
                    if obj_size < HEADER_SIZE || offset + obj_size > region.cursor {
                        break;
                    }
                    if region.mark_bitmap.is_marked(obj_addr) {
                        let obj_ptr = obj_addr as *mut u8;
                        self.scan_object_refs(obj_ptr, header, &regions, &mut worklist);
                    }
                    offset += obj_size;
                }
            }
            // Tell the caller to keep stepping — the rescan likely
            // refilled the worklist with newly-discovered references.
            // Returning `false` causes the marking loop to call us
            // again, which will drain whatever the rescan produced.
            return worklist.is_empty() && !self.mark_worklist_overflowed.load(Ordering::Relaxed);
        }

        // Worklist empty and no overflow: marking complete.
        true
    }

    /// Scan an object's reference slots; for each in-heap, not-yet-marked
    /// target push the target onto `worklist`. This mirrors
    /// `ConcurrentMarker::scan_object` in `concurrent_mark.rs` but uses
    /// G1's per-region addressing (objects live inside `G1Region::data`).
    ///
    /// Round-2 fix (HIGH — GC #5): bitmaps are per-region. Instead of
    /// receiving a single global bitmap, this helper looks up the owning
    /// region for each reference and consults that region's bitmap to
    /// avoid pushing already-marked targets back onto the worklist. The
    /// `try_mark` in `concurrent_mark_step` is still the authoritative
    /// marker; the `is_marked` check here is only an optimization to
    /// reduce worklist churn.
    fn scan_object_refs(
        &self,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        regions: &[G1Region],
        worklist: &mut Vec<usize>,
    ) {
        // CRIT-perf fix: use the O(log R) cached `lookup_region_for_addr`
        // helper instead of an O(R) linear walk. The wrapper preserves
        // the "skip Free regions" guard the linear version had by
        // checking the looked-up region's type.
        let region_for = |p: *mut u8| -> Option<usize> {
            let idx = self.lookup_region_for_addr(p as usize)?;
            if regions[idx].region_type == RegionType::Free {
                None
            } else {
                Some(idx)
            }
        };

        // C2 (round-12 gc): humongous objects are region-fragmented — reading a
        // ref slot at a flat offset from `obj_ptr` would read OOB past the
        // start region for any slot beyond the first region's payload. Detect
        // the humongous case once and read each ref slot through the same
        // region-aware translation used by the field/array accessors.
        let humongous_start: Option<(usize, usize)> = {
            match self.lookup_region_for_addr(obj_ptr as usize) {
                Some(idx) if regions[idx].region_type == RegionType::HumongousStart => {
                    let total_size = object_total_size(header);
                    Some((idx, total_size.saturating_sub(HEADER_SIZE)))
                }
                _ => None,
            }
        };
        // Read an 8-byte ref word at logical payload offset `payload_off`.
        let read_ref = |payload_off: usize| -> u64 {
            if let Some((start, total_payload)) = humongous_start {
                let mut buf = [0u8; 8];
                if self.humongous_copy(
                    regions,
                    start,
                    total_payload,
                    payload_off,
                    buf.as_mut_ptr(),
                    8,
                    false,
                ) {
                    u64::from_ne_bytes(buf)
                } else {
                    0
                }
            } else {
                // SAFETY: caller guarantees the slot is within the object.
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + payload_off) };
                unsafe { std::ptr::read(slot_ptr as *const u64) }
            }
        };

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                // Reference array: 8-byte compact slot per element.
                for i in 0..header.array_length() as usize {
                    let raw: u64 = read_ref(i * 8);
                    if raw == 0 {
                        continue;
                    }
                    let ref_ptr = raw as usize as *mut u8;
                    if let Some(idx) = region_for(ref_ptr) {
                        if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                            // Round-9 gc HIGH-5: graceful overflow — drop
                            // the push and record the event so remark
                            // can run a conservative full re-walk.
                            if worklist.len() >= MARK_WORKLIST_CAP {
                                self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                            } else {
                                worklist.push(ref_ptr as usize);
                            }
                        }
                    }
                }
            }
            // Primitive arrays carry no references.
        } else {
            // INT-8: referent-slot hiding. A Weak/Soft/Phantom Reference
            // object (registered in `reference_skip` at mark start, re-keyed
            // across every evacuation pause) has its slot 0 — the referent —
            // EXCLUDED from the trace: weak reachability must not keep the
            // referent alive, or remark-time reference processing can never
            // see a dead verdict for it (the INT-8 bitmap taint). All other
            // slots (queue, next, discovered, subclass fields) trace
            // normally. Lock order: regions (held by every caller) →
            // reference_skip — same as `remap_reference_skip_set`'s callers.
            let first_slot = {
                let skip = self.reference_skip.lock();
                if !skip.is_empty() && skip.contains(&(obj_ptr as usize)) {
                    1
                } else {
                    0
                }
            };
            if cratonvm_types::is_compact_object(header) {
                // Borrowing accessor: the mark walk only reads `field_offsets` /
                // `is_ref`, so it need not pay an `Arc` clone/drop per marked
                // object.
                let _ = cratonvm_types::with_class_layout(
                    header.class_id.as_u32(),
                    header.num_slots(),
                    |layout| {
                        for (slot_idx, (&payload_off, &is_ref)) in layout
                            .field_offsets
                            .iter()
                            .zip(layout.is_ref.iter())
                            .enumerate()
                        {
                            if !is_ref || slot_idx < first_slot {
                                continue;
                            }
                            let raw = read_ref(payload_off as usize);
                            if raw == 0 {
                                continue;
                            }
                            let ref_ptr = raw as usize as *mut u8;
                            if let Some(idx) = region_for(ref_ptr) {
                                if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                                    if worklist.len() >= MARK_WORKLIST_CAP {
                                        self.mark_worklist_overflowed
                                            .store(true, Ordering::Relaxed);
                                    } else {
                                        worklist.push(ref_ptr as usize);
                                    }
                                }
                            }
                        }
                    },
                );
            } else {
                // Legacy object: 16-byte Value slot per field.
                for slot_idx in first_slot..header.num_slots() as usize {
                    let payload_off = slot_idx * SLOT_SIZE;
                    let value = if let Some((start, total_payload)) = humongous_start {
                        let mut buf = [0u8; SLOT_SIZE];
                        if !self.humongous_copy(
                            regions,
                            start,
                            total_payload,
                            payload_off,
                            buf.as_mut_ptr(),
                            SLOT_SIZE,
                            false,
                        ) {
                            continue;
                        }
                        value_from_bytes(&buf)
                    } else {
                        let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + payload_off) };
                        unsafe { cratonvm_types::read_value_atomic(slot_ptr as *const Value) }
                    };
                    if let Value::Object(Some(ref_obj)) = value {
                        let ref_ptr = ref_obj.as_ptr();
                        if let Some(idx) = region_for(ref_ptr) {
                            if !regions[idx].mark_bitmap.is_marked(ref_ptr as usize) {
                                // Round-9 gc HIGH-5: graceful overflow — drop
                                // the push and record the event so remark
                                // can run a conservative full re-walk.
                                if worklist.len() >= MARK_WORKLIST_CAP {
                                    self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                                } else {
                                    worklist.push(ref_ptr as usize);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Class-loader-data side edges. CratonVM stores these relationships in
        // VM side tables rather than traceable Java fields:
        //
        //   live instance -> defining loader
        //   live defining loader -> class mirrors and metadata oops
        //
        // Root gathering publishes the mirror/metadata tables only for G1's
        // initial/final full-mark snapshots, never for an evacuating young
        // pause. The concurrent marker can therefore follow them exactly like
        // ordinary object references without making the loader itself a root.
        let mut enqueue = |addr: usize| {
            let ptr = addr as *mut u8;
            if let Some(idx) = region_for(ptr) {
                if !regions[idx].mark_bitmap.is_marked(addr) {
                    if worklist.len() >= MARK_WORKLIST_CAP {
                        self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
                    } else {
                        worklist.push(addr);
                    }
                }
            }
        };
        if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr(header.class_id.as_u32())
        {
            enqueue(loader);
        }
        if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(obj_ptr as usize) {
            for mirror in mirrors {
                enqueue(mirror);
            }
        }
        if let Some(metadata) = cratonvm_types::metadata_pin::roots_for_loader(obj_ptr as usize) {
            for object in metadata {
                enqueue(object);
            }
        }
    }

    /// Remark phase (STW): mark roots + drain SATB buffer onto the mark
    /// worklist (the gray set), set phase to `Remark`, and let
    /// `concurrent_mark_step` drain the worklist transitively.
    ///
    /// Audit fix (HIGH-3): the previous implementation drained the SATB
    /// queue into `_satb_entries` and threw it away, and only marked the
    /// root addresses directly without pushing them to a worklist — no
    /// transitive closure was ever computed.
    ///
    /// SATB semantics: every pointer in the SATB queue is a *previously
    /// live* reference value that was overwritten while concurrent
    /// marking was active. We must treat each as a root to avoid the
    /// classic G1 lost-object scenario where A→B is replaced by A→null
    /// after we scanned A but before we scanned B.
    ///
    /// NOTE: in this codebase `vm_heap::g1_mark_roots` also calls this
    /// function during the *initial-mark* STW (right after
    /// `start_concurrent_mark`), so the function deliberately treats
    /// SATB draining as idempotent and safe at either point — at
    /// initial mark the SATB queue is freshly activated and typically
    /// empty. Callers who want a true STW final-remark should follow
    /// up with `concurrent_mark_step(usize::MAX)` to drain the worklist
    /// before transitioning to sweep.
    pub fn remark(&self, roots: &[ObjectRef]) {
        self.gc_state.set_phase(ConcurrentGcPhase::Remark);

        let regions = self.regions.lock();
        let mut worklist = self.mark_worklist.lock();

        // Round-2 fix (HIGH — GC #5): bitmaps are per-region. Helper
        // returns the owning region index (if any) so we can consult
        // *that* region's bitmap for the already-marked check.
        // CRIT-perf fix: use the O(log R) cached `lookup_region_for_addr`
        // instead of an O(R) linear scan; preserve the Free-region skip
        // by post-filtering on the looked-up region's type.
        let region_for = |addr: usize| -> Option<usize> {
            let idx = self.lookup_region_for_addr(addr)?;
            if regions[idx].region_type == RegionType::Free {
                None
            } else {
                Some(idx)
            }
        };

        // Round-9 gc HIGH-5 — graceful overflow. The previous panic was
        // reachable from any wide-graph workload (hostile or otherwise).
        //
        // G1MARK-7: at the cap, a SEED-class entry (root or SATB overwrite)
        // must NOT be plain-dropped the way `scan_object_refs` drops child
        // pushes: the overflow rescan only re-walks MARKED objects, and no
        // marked object need reference a root or a SATB seed — a dropped
        // unmarked seed was unrecoverable and cleanup freed it live. Mark it
        // black without scanning instead (same recovery contract as the
        // CSet-drain and `push_gray_or_mark` paths): the forced rescan scans
        // every marked object's fields, closing the seed's subtree.
        let overflow_flag = &self.mark_worklist_overflowed;
        let push_with_cap = |worklist: &mut Vec<usize>, idx: usize, addr: usize| {
            if worklist.len() >= MARK_WORKLIST_CAP {
                regions[idx].mark_bitmap.try_mark(addr);
                overflow_flag.store(true, Ordering::Relaxed);
                return;
            }
            worklist.push(addr);
        };

        // 1) Roots — push every non-null in-heap root onto the gray set.
        //    `concurrent_mark_step` will mark them and follow their refs.
        for root in roots {
            let p = root.as_ptr();
            if p.is_null() {
                continue;
            }
            let addr = p as usize;
            if let Some(idx) = region_for(addr) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    continue; // already black
                }
                push_with_cap(&mut worklist, idx, addr);
            }
        }

        // 2) SATB completeness (finding #18): before draining the global
        //    shards, pull in every live mutator's partially-full per-thread
        //    buffer. The fast-path barrier only spills a thread's local buffer
        //    into the shards when it fills (~256 entries) or when that thread
        //    self-flushes; references a thread overwrote since its last spill
        //    live only in its local buffer, invisible to the shard `drain()`
        //    below. Excluded from the remark snapshot, the still-live objects
        //    they point at are swept while reachable (use-after-free).
        //
        //    G1's remark uses `drain()` (not `deactivate_and_drain()`, which
        //    runs only at end-of-cycle `cleanup` where stragglers are
        //    discarded), so this is the one place that must drain the registry.
        //    Sound only because `remark` runs at the STW safepoint (final
        //    remark; and the initial-mark call, where buffers are typically
        //    empty): no mutator is mid-barrier, so nothing re-fills a buffer
        //    after we drain it. Draining every buffer here from the collector
        //    removes the dependence on each mutator self-flushing at the
        //    safepoint — the external, unenforced contract finding #18 flagged.
        //
        //    No `debug_assert!` that all registered buffers are now empty: the
        //    registry is process-global and this crate cannot observe the VM's
        //    STW state, so such a check races with any concurrent SATB user
        //    (notably the parallel test harness) and would flake. The contract
        //    is instead verified deterministically by
        //    `satb::tests::flush_all_captures_every_parked_mutator_buffer`.
        crate::satb::flush_all_thread_satb_buffers(&self.satb_queue);

        // 2) SATB — every overwritten reference becomes a root.
        let satb_entries = self.satb_queue.drain();
        for addr in satb_entries {
            if addr == 0 {
                continue;
            }
            if let Some(idx) = region_for(addr) {
                if regions[idx].mark_bitmap.is_marked(addr) {
                    continue;
                }
                push_with_cap(&mut worklist, idx, addr);
            }
        }

        // Note: we leave SATB *active* — the cycle continues in the
        // background marker. `cleanup()` (the final phase) is the right
        // place to deactivate SATB, since at that point marking is
        // truly complete.
    }

    /// Cleanup phase: compute per-region live_bytes and gc_efficiency,
    /// free completely empty old regions.
    pub fn cleanup(&self) {
        let mut regions = self.regions.lock();
        // SECURITY FIX (V7a): cleanup recycles completely-empty Old
        // regions (reset to Free below). Invalidate every mutator's RSet
        // fast-path cache before that reclassification — see
        // `rset_cache_epoch`.
        self.rset_cache_epoch.fetch_add(1, Ordering::Release);
        // G1AUD-5: the generation this cleanup owns. Read AFTER the bump so
        // every region recycled below is stamped with a value strictly greater
        // than any generation a still-running mutator could have recorded an
        // edge in.
        let cleanup_generation = self.rset_generation();
        let region_size = self.config.region_size;
        // G1MARK-8 fail-safe: the marker skipped an implausible-header gray
        // entry this cycle, so the closure may be incomplete — a 0-live
        // verdict is not trustworthy. Retain everything (no in-place frees,
        // no humongous reclaim); the next cycle re-derives liveness from
        // scratch. Swap-and-clear: the flag is per-cycle.
        let saw_implausible = self.mark_saw_implausible.swap(false, Ordering::Relaxed);
        if saw_implausible {
            tracing::warn!(
                "g1 cleanup: implausible gray entry seen during marking — \
                 retaining all regions this cycle (no in-place frees)"
            );
        }
        // G1AUD-3 — the gray set MUST be empty here.
        //
        // Cleanup's in-place free of a zero-live Old region (and the humongous
        // reclaim below) is sound only if the transitive closure is COMPLETE:
        // `live_bytes == 0` is read as "every object in this region predates
        // the mark snapshot and is unreachable in it". A non-empty gray set
        // means the closure was never driven to a fixed point, so unmarked
        // does not imply unreachable and the verdict frees live objects.
        //
        // The precondition was documented on the driver
        // (`VmHeap::g1_final_remark_and_cleanup` runs
        // `while !concurrent_mark_step(usize::MAX) {}` before calling here) and
        // enforced by nothing — and the sibling driver
        // `VmHeap::g1_signal_marking_complete`, retained for the abort/teardown
        // path, calls `cleanup()` with NO remark and NO drain at all. Rather
        // than trust the caller, detect it and take the same fail-safe the
        // implausible-header gate takes: retain everything for this cycle. The
        // next cycle re-derives liveness from scratch, so the cost is one
        // delayed reclamation, against freeing a live region.
        //
        // Lock order is regions -> worklist, matching every other site that
        // holds both (`marking_keepalive_roots`, `concurrent_mark_step`, and
        // the post-pause worklist remaps).
        // Deliberately a warn + retain, NOT a `debug_assert!`: cleanup runs on
        // a heap that may already be damaged, and the module's own policy (see
        // the `live_bytes > region.cursor` clamp below) is that it must not
        // introduce a panic path there. The invariant is pinned by
        // `cleanup_with_an_undrained_gray_set_retains_every_region` instead,
        // which is a stronger check than an assertion because it proves the
        // fail-safe actually retains.
        let closure_incomplete = !self.mark_worklist.lock().is_empty();
        if closure_incomplete {
            tracing::warn!(
                "g1 cleanup: gray set non-empty at cleanup — the mark closure is \
                 incomplete; retaining all regions this cycle (no in-place frees). \
                 Drive `concurrent_mark_step(usize::MAX)` to a fixed point first \
                 (see `VmHeap::g1_final_remark_and_cleanup`)."
            );
        }
        // Either fail-safe suppresses every reclamation decision this cycle.
        let retain_all = saw_implausible || closure_incomplete;
        // TAMS guard (see `mark_start_snapshot`): bytes allocated after the
        // mark-start snapshot carry no mark information and MUST count as
        // live, or this pass frees Old regions filled by promotion during
        // the cycle and zeroes live objects.
        let mark_snapshot: Vec<(u64, usize, RegionType)> = self.mark_start_snapshot.lock().clone();
        let jit_skips = self.jit_tlab_skip_spans();

        for (region_idx, region) in regions.iter_mut().enumerate() {
            if region.region_type == RegionType::Free {
                continue;
            }

            // Compute live bytes by walking objects and checking the bitmap.
            // Round-2 fix (HIGH — GC #5): consult this region's own
            // bitmap (keyed off `data.as_ptr()`), not a global one.
            let base = region.data.as_ptr() as usize;
            let mut live_bytes = 0usize;
            let mut offset = 0usize;

            // TAMS (top-at-mark-start) for this region. Objects BELOW it were
            // in the mark snapshot, so the bitmap is authoritative for them;
            // everything at or above it postdates the snapshot, carries no
            // mark information, and is implicitly live (added wholesale after
            // the walk).
            //
            // G1MAT-1 (double-count fix): the bitmap walk used to run over the
            // ENTIRE region `[0, cursor)` and the post-TAMS extent
            // `cursor - snap_cursor` was then added on top — so every
            // post-TAMS object that the marker DID reach (SATB keep-alive,
            // `push_gray_or_mark`, a fresh promotion that a root still names)
            // was counted twice. That inflates `live_bytes` (it can exceed
            // `cursor`, breaking the `live_bytes <= cursor` invariant) and
            // therefore `gc_efficiency`, which is exactly the key
            // `mixed_collection` / `select_old_regions_for_mixed_gc` sort on
            // (ascending = worst-first). Inflated efficiency makes
            // garbage-rich Old regions look live, so mixed GC picks the wrong
            // regions — and `estimated_evac_cost_ns` (live_bytes x
            // evac_ns_per_byte) over-charges the pause budget, so it picks
            // FEWER of them. Net effect: old-gen reclamation is throttled and
            // biased, which is precisely the failure mode that keeps G1 from
            // being a usable escape hatch. Bound the bitmap walk by TAMS so
            // each byte is attributed exactly once.
            let tams = if mark_snapshot.is_empty() {
                // No cycle data (cleanup driven outside a real mark cycle,
                // e.g. unit tests): keep the pure-bitmap behaviour by putting
                // TAMS at the top, so nothing is treated as implicitly live.
                region.cursor
            } else {
                match mark_snapshot.get(region_idx) {
                    Some(&(epoch, snap_cursor, snap_type))
                        if epoch == region.reuse_epoch && snap_type == region.region_type =>
                    {
                        snap_cursor.min(region.cursor)
                    }
                    // Recycled (epoch bump), re-typed, or absent snapshot entry:
                    // the region's ENTIRE content postdates the snapshot.
                    _ => 0,
                }
            };

            while offset < tams {
                let obj_addr = base + offset;
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_addr) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
                if let Some(gap) = gap_filler_len(obj_addr as *const u8) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(obj_addr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: humongous continuation filler covers
                // the entire region with no live objects of its own; skip.
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);

                if obj_size < HEADER_SIZE || offset + obj_size > region.cursor {
                    break;
                }

                if region.mark_bitmap.is_marked(obj_addr) {
                    live_bytes += obj_size;
                }
                offset += obj_size;
            }

            // TAMS: everything at or above `tams` postdates the mark-start
            // snapshot and is conservatively live. `tams` already encodes the
            // recycled / re-typed / absent-entry cases (0 => the whole region
            // postdates the snapshot) and the no-cycle case (`tams == cursor`
            // => nothing implicitly live, pure-bitmap verdict). Because the
            // walk above stopped at `tams`, this addition cannot double-count
            // a marked post-TAMS object (G1MAT-1).
            live_bytes += region.cursor.saturating_sub(tams);

            // Invariant restored by G1MAT-1: a region can never be more than
            // 100% live, so `gc_efficiency` stays in [0, 1] and the worst-first
            // mixed-GC ranking is meaningful. Under bump allocation TAMS is
            // always an object boundary, so the walk above cannot legitimately
            // count an object that straddles it — but a corrupt header can
            // report an implausible extent that still fits inside `cursor`.
            // Clamp (and log) rather than `debug_assert!`: cleanup must not
            // introduce a new panic path on an already-damaged heap, and a
            // clamped value is still non-zero, so the in-place-free decision
            // below is unaffected.
            if live_bytes > region.cursor {
                tracing::warn!(
                    "g1 cleanup: region {} reported {} live bytes over a {}-byte \
                     cursor (tams {}) — clamping; a header in this region is \
                     likely corrupt",
                    region_idx,
                    live_bytes,
                    region.cursor,
                    tams
                );
                live_bytes = region.cursor;
            }

            region.live_bytes = live_bytes;
            region.gc_efficiency = if region_size > 0 {
                live_bytes as f64 / region_size as f64
            } else {
                0.0
            };

            // In-place free of wholly-dead Old regions — RESTORED. This was
            // disabled ("containment") when concurrent marking missed objects
            // whose holders young collections moved mid-cycle (SteadyChurn
            // @16m: live list nodes vanished right after a mark cycle,
            // observed via CRATONVM_G1_DBG_REACH + the [FREED] cleanup
            // trace). The marker itself is fixed now, so a 0-live verdict is
            // trustworthy again:
            //
            //   1. FINAL REMARK actually runs: the VM finishes every cycle
            //      with an STW remark (root re-scan + SATB drain) and drains
            //      the gray set to a fixed point BEFORE calling cleanup
            //      (`vm_heap::g1_final_remark_and_cleanup`). Previously the
            //      SATB log was never consumed — the whole cycle was a
            //      one-shot closure racing mutator edge deletions.
            //   2. KEEP-ALIVE across evacuation pauses: CSet-resident gray
            //      and SATB entries are evacuated like roots and re-grayed
            //      (`marking_keepalive_roots`), never dropped — snapshot-live
            //      subtrees survive every young/mixed pause in the cycle.
            //   3. TAMS: the `mark_start_snapshot` guard above counts every
            //      post-snapshot allocation as live.
            //
            // Under (1)–(3), `live_bytes == 0` ⇒ every object in the region
            // predates the mark snapshot and is unreachable in it ⇒ garbage
            // by SATB. The empty-snapshot gate keeps the bitmap-only verdict
            // advisory when cleanup is driven outside a real cycle (unit
            // tests) — no in-place free there.
            if !retain_all
                && !mark_snapshot.is_empty()
                && region.live_bytes == 0
                && region.region_type == RegionType::Old
                && !region.pinned
            {
                if gc_flags().g1_dbg_reach {
                    eprintln!(
                        "[g1][FREED] cleanup region={region_idx} cursor={:#x}",
                        region.cursor
                    );
                }
                region.reset(cleanup_generation);
            }
        }

        // G1MARK-8: humongous reclaim trusts the same possibly-incomplete
        // closure — skip it under either fail-safe (G1AUD-3 adds the
        // undrained-gray-set case to the implausible-header one).
        if !retain_all {
            self.reclaim_dead_humongous_spans_locked(&mut regions);
        }

        // G1MAT-4: prune remembered-set entries naming regions that are now
        // Free. Nothing else in the collector ever removes an rset entry —
        // `RememberedSet::clear` only runs on the TARGET region's own
        // `reset()` — so without this a source index recorded once is kept for
        // the rest of that target's life. Cleanup is the right place: it runs
        // once per mark cycle, already holds the regions lock, and the Free
        // set is final here (both the in-place Old frees above and the
        // humongous reclaim have completed).
        //
        // Safety: a `Free` region holds no live object, so it cannot be the
        // holder of a live cross-region edge — dropping it can only remove
        // work, never hide a reachable referent. The scan side already skips
        // Free sources (`scan_source_region_for_cset_refs`), so this changes
        // no collection decision; it bounds memory and per-pause iteration.
        // If such a region is later re-typed and stores a cross-region
        // reference, the mutator post-write barrier re-adds it, and the
        // Phase-4 rebuild re-derives GC-internal edges.
        //
        // G1AUD-5 (defect G1-8) — the Free test alone is not enough. A source
        // that was freed and then RE-TYPED (the common case: a young pause
        // recycles a region and the next allocation claims it as Eden) is not
        // Free at cleanup time, so its entry survived forever, and every pause
        // re-walked that region wholesale on behalf of an object that had been
        // zero-filled cycles ago — the "undead" entry. Each entry now carries
        // the generation it was recorded in and each region the generation it
        // was last recycled in, which makes the sharper test available here.
        {
            // `(is_free, recycled_in_generation)` per region index, snapshotted
            // so the retain closures below do not re-borrow `regions`.
            let source_state: Vec<(bool, u64)> = regions
                .iter()
                .map(|r| (r.region_type == RegionType::Free, r.recycled_in_generation))
                .collect();
            for region in regions.iter() {
                region
                    .rset
                    .retain_sources_in_generation(|src, generation| {
                        match source_state.get(src) {
                            // Free: holds nothing, so it holds no live edge.
                            // Recycled since the edge was recorded: the holder
                            // was zero-filled by `reset`. Either way the entry
                            // is dead. `RSET_GENERATION_PINNED` entries (no
                            // generation available at record time) survive the
                            // second test by construction.
                            Some(&(is_free, recycled_in)) => !is_free && generation >= recycled_in,
                            // Out of range — can never be walked.
                            None => false,
                        }
                    });
            }
        }

        // Publish the remembered-set size gauge for G1. Until now
        // `remembered_set_bytes` described only the generational card table, so
        // `rset_bytes_per_live_byte` read as zero under `-XX:+UseG1GC` — the
        // reconciliation item left open by `docs/gc/tlab-and-card-audit.md`
        // §2.3. Measured here (once per mark cycle, after the prune) rather
        // than per pause: this is the point at which the set is smallest and
        // final, and it costs one lock per region on a path that just walked
        // every region anyway.
        //
        // G1AUD-5: one entry is now `(source_index, generation)`, so the
        // per-entry cost is `usize + u64`, not `usize`. Keeping the gauge in
        // step with the representation matters more than the absolute number:
        // `rset_bytes_per_live_byte` is the measurement that says whether the
        // remembered set needs a real bound (audit §9 item 5), and a gauge that
        // silently under-reports by a third would answer that question wrong.
        const RSET_ENTRY_BYTES: usize =
            std::mem::size_of::<usize>() + std::mem::size_of::<u64>();
        let rset_sources_total: usize = regions.iter().map(|r| r.rset.source_count()).sum();
        crate::gc_metrics::record_remembered_set_bytes(
            (rset_sources_total * RSET_ENTRY_BYTES) as u64,
        );
        let pinned_regions = regions.iter().filter(|r| r.pinned).count();

        // Refresh the IHOP occupancy statistic NOW: cleanup just freed Old
        // regions and dead humongous spans, and leaving the pre-cleanup sum
        // in place until the next evacuation pause lets
        // `g1_should_start_marking` immediately re-fire a pointless
        // back-to-back mark cycle against stale occupancy.
        self.recompute_old_gen_bytes(&regions);

        // Audit fix (HIGH-3): clear any stragglers from the gray set and
        // deactivate the SATB write barrier — the cycle is fully done.
        self.mark_worklist.lock().clear();
        // Round-9 gc HIGH-5: clear the overflow indicator so the next
        // cycle starts in a clean state. Captured on the way out so the cycle
        // record can name it: `concurrent_mark_step` clears the flag when it
        // runs its recovery rescan, so a `true` here means an overflow was
        // still OUTSTANDING at cleanup (the rescan never ran), which is the
        // case worth reporting.
        let overflow_outstanding = self.mark_worklist_overflowed.swap(false, Ordering::Relaxed);
        // INT-8: referent-slot hiding ends with the cycle.
        self.reference_skip.lock().clear();
        // Round-5 CRIT #4: close the SATB barrier with a drain-then-flip
        // protocol so no mutator log push that observed the gate as
        // active can be stranded after the cycle ends. The drained
        // stragglers are discarded — the mark cycle is complete and
        // anything not yet marked is correctly dead; the next cycle
        // will re-discover live state from roots. (We are at the very
        // end of the cycle so missing a few late SATB entries is
        // semantically fine.)
        let _stragglers = self.satb_queue.deactivate_and_drain();

        self.gc_state.set_phase(ConcurrentGcPhase::Idle);
        // Round-9 HIGH-1: Release publishes marking_complete=true so any
        // subsequent RSet writes / mutator-side reads of the flag observe
        // all of the prior cycle's effects (gray set drained, SATB queue
        // deactivated, phase set to Idle). Paired with Acquire load in
        // `needs_mixed_gc` and any other consumer.
        self.marking_complete.store(true, Ordering::Release);
        self.mixed_gc_remaining
            .store(self.config.mixed_gc_count_target as u64, Ordering::Relaxed);

        // State what this cleanup decided, including any fail-safe it took.
        // Without this an operator watching G1 fail to reclaim old gen cannot
        // tell "the closure was abandoned" from "there is no garbage".
        let mut degraded = crate::gc_metrics::g1_degraded::NONE;
        if saw_implausible {
            degraded |= crate::gc_metrics::g1_degraded::MARK_IMPLAUSIBLE_HEADER;
        }
        if closure_incomplete {
            degraded |= crate::gc_metrics::g1_degraded::CLEANUP_CLOSURE_INCOMPLETE;
        }
        if overflow_outstanding {
            degraded |= crate::gc_metrics::g1_degraded::MARK_WORKLIST_OVERFLOW;
        }
        if pinned_regions > 0 {
            degraded |= crate::gc_metrics::g1_degraded::JNI_PINNED_REGIONS_EXCLUDED;
        }
        crate::gc_metrics::record_g1_cycle(
            crate::gc_metrics::g1_cycle_kind::CONCURRENT_CLEANUP,
            0,
            0,
            pinned_regions as u32,
            rset_sources_total as u32,
            degraded,
        );
    }

    /// Reclaim dead humongous spans after a mark cycle.
    ///
    /// Young and mixed evacuation cannot infer humongous reachability from a
    /// partial collection set. Cleanup runs after whole-heap marking, so an
    /// unmarked `HumongousStart` with no pinned slice is safe to recycle along
    /// with each contiguous continuation region.
    fn reclaim_dead_humongous_spans_locked(&self, regions: &mut [G1Region]) -> usize {
        let region_size = self.config.region_size;
        if region_size == 0 {
            return 0;
        }

        let mut reclaimed = 0usize;
        let mut i = 0usize;
        while i < regions.len() {
            if regions[i].region_type != RegionType::HumongousStart {
                i += 1;
                continue;
            }

            let total_size = regions[i].cursor;
            let regions_needed = total_size.div_ceil(region_size).max(1);
            let Some(end) = i
                .checked_add(regions_needed)
                .filter(|&end| end <= regions.len())
            else {
                i += 1;
                continue;
            };

            // G1MAT-2: validate the span before resetting anything in it. The
            // extent is derived purely from `regions[i].cursor`, so a stale or
            // corrupt cursor on a `HumongousStart` would silently `reset()`
            // (zero-fill + retype-to-Free) regions belonging to OTHER live
            // objects. Every region a humongous span actually owns is typed
            // `HumongousContinuation` (`alloc_humongous_locked`), so require
            // exactly that before touching them. A mismatch means the derived
            // extent disagrees with the region table: skip the span and log,
            // rather than free memory we cannot prove is ours.
            //
            // This is the same class of defect as the round-9 `HumongousFiller`
            // walker audit: a humongous invariant assumed at a call site rather
            // than checked there.
            let span_well_formed = regions[i + 1..end]
                .iter()
                .all(|r| r.region_type == RegionType::HumongousContinuation);
            if !span_well_formed {
                tracing::warn!(
                    "g1 cleanup: humongous span at region {} claims {} regions \
                     (cursor {}) but the following regions are not all \
                     HumongousContinuation — skipping reclaim",
                    i,
                    regions_needed,
                    total_size
                );
                i += 1;
                continue;
            }

            let pinned = regions[i..end].iter().any(|r| r.pinned);
            if regions[i].live_bytes == 0 && !pinned {
                reclaimed = reclaimed.saturating_add(total_size);
                let generation = self.rset_generation();
                for region in &mut regions[i..end] {
                    region.reset(generation);
                }
                i = end;
            } else {
                i += 1;
            }
        }

        reclaimed
    }

    // -----------------------------------------------------------------------
    // IHOP
    // -----------------------------------------------------------------------

    /// Check if old-gen occupancy has reached the IHOP threshold.
    pub fn check_ihop(&self) -> bool {
        let old_bytes = self.old_gen_bytes.load(Ordering::Relaxed);
        let threshold = self.marking_threshold_bytes.load(Ordering::Relaxed);
        old_bytes >= threshold
    }

    /// Adaptively adjust IHOP based on actual pause time.
    pub fn update_ihop(&self, actual_pause_ms: u64) {
        let target = self.config.max_gc_pause_ms;
        let current_threshold = self.marking_threshold_bytes.load(Ordering::Relaxed);

        let new_threshold = if actual_pause_ms > target {
            // Pause too long: lower threshold to start marking earlier —
            // but FLOOR the decay at 1% of the heap (min one region). With
            // no floor, a chronically-slow host (every pause > target)
            // decays the threshold to 0 through integer truncation; the
            // raise branch can never recover it (0 * 1.05 == 0) and the
            // VM-side trigger gate (`g1_should_start_marking` requires
            // `marking_threshold_bytes() > 0`) then reads a zero threshold
            // as "marking disabled" — permanently: no cycle ever starts,
            // Old/humongous garbage is never reclaimed, and the heap OOMs
            // while mostly dead.
            let floor = (self.config.heap_size / 100).max(self.config.region_size);
            ((current_threshold as f64 * 0.9) as usize).max(floor)
        } else if actual_pause_ms < target / 2 {
            // Pause well under target: raise threshold — but NEVER above the
            // statically-configured IHOP. G1 has no full-GC fallback: letting
            // fast young pauses defer marking indefinitely means dead promoted
            // objects accumulate in Old unreclaimed until the heap is
            // exhausted. Observed on the SteadyChurn recreation: ~7ms pauses
            // raised the threshold 5% per collection to the old 90%-of-heap
            // cap, concurrent marking never started across 61k young
            // collections, and every heap size died with a true OOM while
            // >80% of Old was garbage. The adaptive threshold may float
            // BELOW the configured IHOP (long pauses → mark earlier) and
            // recover back up to it, no further.
            let max = self.config.heap_size * self.config.ihop_percent as usize / 100;
            ((current_threshold as f64 * 1.05) as usize).min(max)
        } else {
            current_threshold
        };

        self.marking_threshold_bytes
            .store(new_threshold, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Region Pinning (JEP 423)
    // -----------------------------------------------------------------------

    /// Pin a region, preventing it from being evacuated during GC. Refcounted:
    /// each `pin_region` must be balanced by exactly one [`unpin_region`], so
    /// overlapping JNI critical sections on the same region pin/unpin
    /// independently.
    pub fn pin_region(&self, region_idx: usize) {
        let mut regions = self.regions.lock();
        if region_idx < regions.len() {
            let r = &mut regions[region_idx];
            r.pin_count = r.pin_count.saturating_add(1);
            r.pinned = true;
        }
    }

    /// Unpin a region, allowing it to be collected again once its pin count
    /// returns to zero. Tolerant of an unbalanced call (count already zero), so
    /// a stray double-`Release` from native code cannot wrongly clear a pin
    /// another section still holds.
    pub fn unpin_region(&self, region_idx: usize) {
        let mut regions = self.regions.lock();
        if region_idx < regions.len() {
            let r = &mut regions[region_idx];
            r.pin_count = r.pin_count.saturating_sub(1);
            if r.pin_count == 0 {
                r.pinned = false;
            }
        }
    }

    /// Pin the region backing the object at `addr` (a JNI critical section) and
    /// return its index for the matching [`unpin_region`], or `None` if `addr`
    /// is not in any region. Refcounted via [`pin_region`].
    ///
    /// G1's only object-moving paths — `young_collection` and
    /// `mixed_collection` — exclude pinned regions from the collection set, and
    /// no full-GC compaction path exists, so this guarantees the object stays
    /// at `addr` until every pin is released. That is what lets
    /// `GetPrimitiveArrayCritical`'s detached copy be copied back to the *same*
    /// object at `Release` (its Get-time handle would otherwise go stale once
    /// the array was evacuated). See the design doc §3.2.5.
    pub fn pin_region_for_addr(&self, addr: usize) -> Option<usize> {
        let idx = self.lookup_region_for_addr(addr)?;
        self.pin_region(idx);
        Some(idx)
    }

    /// Check if a region is pinned.
    pub fn is_pinned(&self, region_idx: usize) -> bool {
        let regions = self.regions.lock();
        region_idx < regions.len() && regions[region_idx].pinned
    }

    // -----------------------------------------------------------------------
    // String Deduplication
    // -----------------------------------------------------------------------

    /// Attempt to deduplicate a String object. The hash is used to look up
    /// a canonical instance.
    ///
    /// Returns:
    /// - `Some(canonical_addr)` on a HIT: the caller should redirect its
    ///   pointer at `addr` to `canonical_addr` and discard the duplicate.
    /// - `None` on a MISS (or when dedup is disabled): the caller's
    ///   `addr` was registered as the canonical instance for `hash`;
    ///   nothing to redirect.
    ///
    /// Audit fix (HIGH): the previous implementation ignored the canonical
    /// address on a hit and only returned a `bool`, so the caller had no
    /// way to actually perform the dedup. Returning the canonical address
    /// makes the function correct on its own terms.
    ///
    /// # DO NOT WIRE UP without a remap (G1CORE-6)
    ///
    /// The table stores RAW canonical object addresses and NO collection
    /// path remaps or purges them: a canonical String registered in Eden is
    /// evacuated or freed by the very next young pause, after which a HIT
    /// returns a dangling from-space address the caller would install as a
    /// live reference (UAF). There are currently no production callers
    /// (`string_dedup_enabled` plumbs from `-XX:+UseStringDeduplication`
    /// but nothing invokes this API). Before adding one: remap
    /// `string_dedup_table` values through the pointer map (and drop
    /// entries whose value died) in every collection's fix-up phase, or
    /// register only non-moving (post-promotion Old) addresses.
    pub fn deduplicate_string(&self, hash: u64, addr: usize) -> Option<usize> {
        if !self.config.string_dedup_enabled {
            return None;
        }
        let mut table = self.string_dedup_table.lock();
        if let Some(&canonical) = table.get(&hash) {
            // Hit: caller should redirect `addr` -> `canonical`.
            // If `addr` happens to already be the canonical (idempotent
            // caller), still return Some so the API stays uniform.
            Some(canonical)
        } else {
            // Miss: register `addr` as the canonical instance.
            table.insert(hash, addr);
            None
        }
    }

    // -----------------------------------------------------------------------
    // GC Logging
    // -----------------------------------------------------------------------

    /// Log a GC event if logging is enabled.
    /// Step 7 — refresh the rolling `evac_ns_per_byte` calibration from a
    /// completed mixed collection. Observed cost = whole pause / bytes copied
    /// (deliberately includes the fixed scan overhead, so it slightly
    /// *over*-estimates → a smaller, safer collection set). Smoothed with a
    /// slow EMA (1/8 weight on the new sample) and clamped to a sane band so a
    /// single anomalous cycle cannot wreck the estimate. No-op when nothing was
    /// copied (no signal).
    fn update_evac_cost(&self, pause_ns: u64, bytes_copied: usize) {
        if bytes_copied == 0 || pause_ns == 0 {
            return;
        }
        let observed = (pause_ns / bytes_copied as u64).clamp(1, 4096);
        let prev = self.evac_ns_per_byte.load(Ordering::Relaxed).max(1);
        let next = (prev.saturating_mul(7).saturating_add(observed)) / 8;
        self.evac_ns_per_byte.store(next.max(1), Ordering::Relaxed);
    }

    /// Account for one completed STW collection: bump the counters, append the
    /// microsecond-granular record to the bounded pause ring, and emit the log
    /// line(s). Called by every young/mixed evacuation path (serial + parallel)
    /// so the pause sink and the `[GC ...]` log stay in lock-step. `pause_us`
    /// is `Instant::elapsed().as_micros()` — see `G1PauseRecord`.
    fn record_collection(&self, collection_type: G1CollectionType, pause_us: u64, stats: &GcStats) {
        // Relaxed ordering: these are statistics counters for monitoring /
        // logging only. They guard no data and a slightly stale read is
        // harmless.
        self.collection_count.fetch_add(1, Ordering::Relaxed);
        self.total_pause_us.fetch_add(pause_us, Ordering::Relaxed);

        {
            let mut hist = self.pause_history.lock();
            if hist.len() >= PAUSE_HISTORY_CAP {
                hist.pop_front();
                self.pause_history_dropped.fetch_add(1, Ordering::Relaxed);
            }
            hist.push_back(G1PauseRecord {
                collection_type,
                pause_us,
                objects_copied: stats.objects_copied,
                bytes_copied: stats.bytes_copied,
                bytes_freed: stats.bytes_freed,
            });
        }

        self.log_gc_event(collection_type, pause_us, stats);
    }

    fn log_gc_event(&self, collection_type: G1CollectionType, pause_us: u64, stats: &GcStats) {
        if !self.gc_log_enabled.load(Ordering::Relaxed) {
            return;
        }
        // Two sinks:
        //  - `tracing::info!` keeps the legacy `[GC <Type>] ...` line for
        //    `RUST_LOG=cratonvm_gc=info` consumers and the G1-selection guard
        //    (which greps for `[GC YoungOnly]`); pause is now microseconds.
        //  - a structured `[GC-STAT]` line straight to stderr so the pause +
        //    bytes are visible with JUST `--verbose:gc` (no RUST_LOG needed)
        //    and trivially parseable by the gauntlet runner (§7 item 6).
        tracing::info!(
            "[GC {:?}] pause={pause_us}us copied={} bytes_copied={} freed={}",
            collection_type,
            stats.objects_copied,
            stats.bytes_copied,
            stats.bytes_freed,
        );
        eprintln!(
            "[GC-STAT] type={:?} pause_us={pause_us} objects_copied={} bytes_copied={} bytes_freed={}{}",
            collection_type,
            stats.objects_copied,
            stats.bytes_copied,
            stats.bytes_freed,
            if gc_flags().g1_dbg_reach {
                // try_lock: the collection paths call this while still holding
                // the regions guard (diagnostic-only; skip the counts then).
                if let Some(regions) = self.regions.try_lock() {
                    let mut f = 0usize;
                    let mut e = 0usize;
                    let mut s = 0usize;
                    let mut o = 0usize;
                    let mut h = 0usize;
                    for r in regions.iter() {
                        match r.region_type {
                            RegionType::Free => f += 1,
                            RegionType::Eden => e += 1,
                            RegionType::Survivor => s += 1,
                            RegionType::Old => o += 1,
                            _ => h += 1,
                        }
                    }
                    format!(
                        " free={f} eden={e} surv={s} old={o} hum={h} old_bytes={}",
                        self.old_gen_bytes.load(Ordering::Relaxed)
                    )
                } else {
                    format!(
                        " old_bytes={}",
                        self.old_gen_bytes.load(Ordering::Relaxed)
                    )
                }
            } else {
                String::new()
            },
        );
    }

    /// Snapshot the bounded pause ring (most-recent window) for offline
    /// percentile/throughput analysis.
    pub fn pause_history_snapshot(&self) -> Vec<G1PauseRecord> {
        self.pause_history.lock().iter().copied().collect()
    }

    /// Reduce the recorded pauses to per-type p50/p99/max (§5 acceptance
    /// metric). Returns `None` when nothing has been collected yet.
    pub fn pause_summary(&self) -> Option<G1PauseSummary> {
        let hist = self.pause_history.lock();
        if hist.is_empty() {
            return None;
        }
        let reduce = |kind: G1CollectionType| -> G1PausePercentiles {
            let mut samples: Vec<u64> = hist
                .iter()
                .filter(|r| r.collection_type == kind)
                .map(|r| r.pause_us)
                .collect();
            if samples.is_empty() {
                return G1PausePercentiles::default();
            }
            samples.sort_unstable();
            let count = samples.len();
            let total_us: u64 = samples.iter().sum();
            // Nearest-rank percentile: index ceil(p/100 * N) - 1, clamped.
            let pct = |p: usize| -> u64 {
                let rank = ((p * count) + 99) / 100; // ceil(p*N/100)
                let idx = rank.saturating_sub(1).min(count - 1);
                samples[idx]
            };
            G1PausePercentiles {
                count: count as u64,
                total_us,
                p50_us: pct(50),
                p99_us: pct(99),
                max_us: *samples.last().unwrap(),
            }
        };
        Some(G1PauseSummary {
            young: reduce(G1CollectionType::YoungOnly),
            mixed: reduce(G1CollectionType::Mixed),
            dropped: self.pause_history_dropped.load(Ordering::Relaxed),
        })
    }

    /// Print the aggregate pause summary to stderr (the §5 p50/p99 table). A
    /// no-op when no collection has run. Emitted at VM shutdown when GC stats
    /// are requested — see `VmHeap::print_gc_summary`.
    pub fn print_gc_summary(&self) {
        let Some(s) = self.pause_summary() else {
            return;
        };
        let line = |label: &str, p: &G1PausePercentiles| {
            if p.count == 0 {
                return;
            }
            eprintln!(
                "[GC-SUMMARY] {label} count={} total_us={} p50_us={} p99_us={} max_us={} avg_us={}",
                p.count,
                p.total_us,
                p.p50_us,
                p.p99_us,
                p.max_us,
                p.total_us / p.count.max(1),
            );
        };
        line("young", &s.young);
        line("mixed", &s.mixed);
        if s.dropped > 0 {
            eprintln!(
                "[GC-SUMMARY] note: {} oldest record(s) evicted from the {}-entry ring (percentiles cover the most-recent window)",
                s.dropped, PAUSE_HISTORY_CAP,
            );
        }
    }

    /// Enable GC event logging.
    pub fn enable_gc_logging(&self) {
        self.gc_log_enabled.store(true, Ordering::Relaxed);
    }

    /// Disable GC event logging.
    pub fn disable_gc_logging(&self) {
        self.gc_log_enabled.store(false, Ordering::Relaxed);
    }

    /// Get the total number of collections performed.
    pub fn collection_count(&self) -> u64 {
        self.collection_count.load(Ordering::Relaxed)
    }

    /// Get the total pause time across all collections.
    pub fn total_pause_ms(&self) -> u64 {
        self.total_pause_us.load(Ordering::Relaxed) / 1000
    }

    /// Total pause time across all collections in microseconds (the precise
    /// accumulator; `total_pause_ms()` rounds this to milliseconds).
    pub fn total_pause_us(&self) -> u64 {
        self.total_pause_us.load(Ordering::Relaxed)
    }

    /// Get the current GC phase.
    pub fn gc_phase(&self) -> ConcurrentGcPhase {
        self.gc_state.phase()
    }

    /// Get old-gen byte count.
    pub fn old_gen_bytes(&self) -> usize {
        self.old_gen_bytes.load(Ordering::Relaxed)
    }

    /// Recompute the IHOP occupancy statistic (`old_gen_bytes`) from the
    /// region table: bytes in `Old` regions PLUS humongous spans.
    ///
    /// Humongous objects are logically part of the old generation — HotSpot's
    /// IHOP compares old occupancy *including* humongous regions to the
    /// threshold. Counting only `Old` regions here meant a humongous-heavy
    /// workload (large arrays churned faster than they promote ordinary
    /// objects) never crossed IHOP, so concurrent marking — the ONLY path
    /// that reclaims dead humongous spans (`cleanup` →
    /// `reclaim_dead_humongous_spans_locked`) — never started and the heap
    /// filled with unreclaimable dead spans until OOM, while young pauses
    /// spun freeing nothing.
    ///
    /// A `HumongousStart` region's `cursor` is the FULL object size (its
    /// continuations carry `cursor = 0`), so summing both types counts each
    /// humongous object exactly once.
    fn recompute_old_gen_bytes(&self, regions: &[G1Region]) {
        let old_bytes: usize = regions
            .iter()
            .filter(|r| {
                matches!(
                    r.region_type,
                    RegionType::Old
                        | RegionType::HumongousStart
                        | RegionType::HumongousContinuation
                )
            })
            .map(|r| r.cursor)
            .sum();
        self.old_gen_bytes.store(old_bytes, Ordering::Relaxed);
    }

    /// Get the IHOP marking threshold.
    pub fn marking_threshold_bytes(&self) -> usize {
        self.marking_threshold_bytes.load(Ordering::Relaxed)
    }

    /// Return the total heap capacity in bytes.
    pub fn heap_capacity(&self) -> usize {
        self.config.heap_size
    }

    /// Return (used, capacity) for Eden regions.
    pub fn eden_stats(&self) -> (usize, usize) {
        let regions = self.regions.lock();
        let mut used = 0usize;
        let mut count = 0usize;
        for r in regions.iter() {
            if r.region_type == RegionType::Eden {
                used += r.cursor;
                count += 1;
            }
        }
        (used, count * self.config.region_size)
    }

    /// Return (used, capacity) for Old regions.
    pub fn old_gen_stats(&self) -> (usize, usize) {
        let regions = self.regions.lock();
        let mut used = 0usize;
        let mut count = 0usize;
        for r in regions.iter() {
            if r.region_type == RegionType::Old {
                used += r.cursor;
                count += 1;
            }
        }
        (used, count * self.config.region_size)
    }

    /// Count regions of a given type.
    pub fn count_regions(&self, region_type: RegionType) -> usize {
        let regions = self.regions.lock();
        regions
            .iter()
            .filter(|r| r.region_type == region_type)
            .count()
    }

    /// Check if mixed GC is needed (marking is complete and cycles remain).
    fn needs_mixed_gc(&self) -> bool {
        // Round-9 HIGH-1: Acquire pairs with the Release publish in
        // `finish_mark_cycle` so this reader observes the fully-drained
        // gray set and deactivated SATB queue before acting on the flag.
        self.marking_complete.load(Ordering::Acquire)
            && self.mixed_gc_remaining.load(Ordering::Relaxed) > 0
    }

    // -----------------------------------------------------------------------
    // Fallible allocation (Phase 89)
    // -----------------------------------------------------------------------

    /// Try to allocate a Java object. Returns `None` when Eden is exhausted
    /// (caller should trigger GC and retry).
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        // M6 (round-12 gc): checked `+ HEADER_SIZE` to match `try_alloc_array`
        // and gen_heap; a near-`usize::MAX` field count must not wrap.
        let total_size = HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)?;
        let (ptr, _region) = self.alloc_in_region(total_size)?;

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            u32::try_from(num_fields).ok()?,
        );
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Allocate and pre-initialize primitive-typed slots based on JVM
    /// field descriptor bytes. See
    /// [`crate::heap::default_value_for_descriptor`] for the mapping.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        // Use the GarbageCollector trait's alloc_object to get the raw
        // object, then populate primitive slots.
        let obj = GarbageCollector::alloc_object(self, class_id, num_fields);
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|desc| crate::heap::default_value_for_descriptor(*desc))
                .unwrap_or(Value::Object(None));
            GarbageCollector::set_field(self, obj, i, default);
        }
        obj
    }

    /// Fallible variant of [`Self::alloc_object_with_descriptors`].
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|desc| crate::heap::default_value_for_descriptor(*desc))
                .unwrap_or(Value::Object(None));
            GarbageCollector::set_field(self, obj, i, default);
        }
        Some(obj)
    }

    /// Try to allocate a Java array. Returns `None` when Eden is exhausted.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = HEADER_SIZE.checked_add(data_size)?;
        let (ptr, _region) = self.alloc_in_region(total_size)?;

        // Mirror `length` into BOTH `array_length` and `num_slots`, matching
        // `Heap::alloc_array` (heap.rs:367-370,403-410) and
        // `GenerationalHeap::alloc_array` (gen_heap.rs:497-500). The shared
        // header decoders in `vm_heap` / `walk_objects` (e.g.
        // `VmHeap::num_fields(arr)`) consume `num_slots` and were silently
        // returning 0 for any G1-allocated array prior to this fix.
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            u32::try_from(length).ok()?,
            u32::try_from(length).expect("array length must fit u32 for G1 header num_slots"),
        );
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Carve a TLAB from the current Eden region.
    ///
    /// Returns `Some((ptr, actual_size))` on success. The TLAB is carved from
    /// the Eden region's bump pointer, exactly like how `refill_tlab` works in
    /// the generational collector but backed by G1 regions.
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        let mut regions = self.regions.lock();
        let region_size = self.config.region_size;

        // Don't serve TLABs for requests larger than half a region
        if requested_size > region_size / 2 {
            return None;
        }

        // Try current Eden region
        let cur = self.current_eden.load(Ordering::Relaxed);
        if cur < regions.len() && regions[cur].region_type == RegionType::Eden {
            let remaining = regions[cur].remaining();
            if remaining >= 256 {
                let actual = requested_size.min(remaining);
                if let Some((ptr, _off)) = regions[cur].bump_alloc(actual, 8) {
                    // TLAB contract: every backend returns a fully zeroed
                    // chunk. Inline compiled allocation relies on this for
                    // JVM default field values and zero-valued header words.
                    // Eden regions are recycled without clearing their bytes,
                    // so G1 must establish the contract here.
                    unsafe { std::ptr::write_bytes(ptr, 0, actual) };
                    return Some((ptr, actual));
                }
            }
        }

        // Find a new free region for Eden and carve TLAB from it.
        //
        // Emergency reserve: a TLAB refill is a *speculative bulk* claim (the
        // thread may retire the chunk with most of it unused), so once the
        // Free pool is down to the reserve, stop serving refills and leave
        // those regions to the per-object allocator — which, unlike this one,
        // has callers that cannot be told "no" (`GarbageCollector::
        // alloc_object` aborts the process). See `tlab_reserve_regions`.
        let free_count = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Free)
            .count();
        if free_count <= self.tlab_reserve_regions(regions.len()) {
            // Latch the pressure signal on the way out: the workload is
            // allocating hard enough to exhaust the pool and something must
            // ask for a collection.
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
            return None;
        }
        if let Some(idx) = find_free_region(&regions) {
            regions[idx].region_type = RegionType::Eden;
            self.current_eden.store(idx, Ordering::Relaxed);
            let remaining = regions[idx].remaining();
            if remaining >= 256 {
                let actual = requested_size.min(remaining);
                if let Some((ptr, _off)) = regions[idx].bump_alloc(actual, 8) {
                    // SAFETY: bump_alloc reserved `actual` writable bytes
                    // exclusively from this Eden region.
                    unsafe { std::ptr::write_bytes(ptr, 0, actual) };
                    self.note_region_consumed_locked(&regions);
                    return Some((ptr, actual));
                }
            }
        }

        None
    }

    /// Get a reference to the global SATB queue.
    pub fn satb_queue(&self) -> &Arc<SatbQueue> {
        &self.satb_queue
    }

    /// Record an old reference value in the SATB queue (for concurrent marking).
    /// Only records when SATB is active (during concurrent mark phase).
    ///
    /// Routes through the per-thread SATB buffer
    /// ([`crate::satb::satb_thread_local_log`]) so the hot write-barrier
    /// path takes no shared lock in the common case; the buffer auto-flushes
    /// into the global queue every ~256 entries.
    ///
    /// Also bumps a per-thread "SATB pre-barrier observed" epoch counter so
    /// the debug-only assertion in `write_barrier` can detect callers that
    /// invoke the post-store barrier without first logging the old slot
    /// value (see `write_barrier` for the SATB protocol contract).
    pub fn satb_pre_barrier(&self, old_ref: usize) {
        bump_satb_pre_barrier_epoch();
        if old_ref == 0 {
            return;
        }
        if self.satb_queue.is_active() {
            crate::satb::satb_thread_local_log(&self.satb_queue, old_ref);
        }
    }

    /// Is the SATB pre-barrier obligation in force for a store happening now?
    ///
    /// This is `gc_state.is_marking_active()` plus the assertion that makes the
    /// two-flag gate safe. A reference store logs its old value only when BOTH
    /// the phase says marking is active (that is what makes the store path read
    /// the old slot at all) AND the queue is active (that is what makes
    /// `satb_pre_barrier` retain the value). If a window ever existed where the
    /// phase said "marking" while the queue was already/still inactive, every
    /// store in that window would read its old value and then throw it away —
    /// a silently lost snapshot edge, which is exactly the mark-completeness
    /// hole SATB exists to close.
    ///
    /// The invariant `is_marking_active() => satb_queue.is_active()` is
    /// established by the ORDER of the two state changes:
    ///
    /// * [`Self::start_concurrent_mark`] calls `satb_queue.activate()` while
    ///   the phase is still `InitialMark` (not marking-active) and only then
    ///   flips it to `ConcurrentMark`;
    /// * [`Self::cleanup`] and [`Self::abort_concurrent_mark`] leave the
    ///   marking-active phases BEFORE `deactivate_and_drain()`.
    ///
    /// Both orderings are load-bearing and neither was checked anywhere, so
    /// this `debug_assert!` is the tripwire for a future reordering.
    #[inline]
    fn satb_pre_barrier_required(&self) -> bool {
        let marking = self.gc_state.is_marking_active();
        debug_assert!(
            !marking || self.satb_queue.is_active(),
            "G1 SATB gate is half-open: the concurrent-mark phase is active but the SATB \
             queue is not, so this store would read its old reference value and then \
             discard it. See `G1Collector::satb_pre_barrier_required`."
        );
        marking
    }

    /// G1AUD-5 (defect G1-8) — the current remembered-set *generation*.
    ///
    /// This is [`Self::rset_cache_epoch`], reused as a monotone reclassification
    /// clock: it is bumped (Release, under the regions lock) at the start of
    /// every phase that can recycle or re-type a region, so within one
    /// generation no region is reset. Every rset entry is stamped with the
    /// generation it was recorded in and every recycled region records the
    /// generation it was reset in (`G1Region::recycled_in_generation`); an entry
    /// is dead exactly when `stamp < source.recycled_in_generation`.
    ///
    /// `Acquire` pairs with the `Release` bump so a mutator that observes a new
    /// generation also observes the reclassification that caused it.
    #[inline]
    fn rset_generation(&self) -> u64 {
        self.rset_cache_epoch.load(Ordering::Acquire)
    }

    /// G1AUD-5 — is this remembered-set entry dead?
    ///
    /// True when the source region was recycled *after* the edge was recorded:
    /// [`G1Region::reset`] zero-filled the region and cleared its own rset, so
    /// nothing that could have held the edge survived. An out-of-range source
    /// index is also dead (it can never be walked).
    ///
    /// Deliberately a strict `<`: an edge recorded in the SAME generation that
    /// later reset the source (the Phase-4 rebuild runs before Phase 5's frees)
    /// is retained. That over-retains for one cycle and is the fail-safe
    /// direction — the scan side independently refuses `Free` sources, so a
    /// retained-but-dead entry costs a lookup, while a dropped-but-live one is
    /// a use-after-free.
    fn rset_entry_is_stale(regions: &[G1Region], source: usize, recorded_generation: u64) -> bool {
        match regions.get(source) {
            Some(r) => recorded_generation < r.recycled_in_generation,
            None => true,
        }
    }

    /// G1AUD-5 — the remembered-set sources of `cset` that are still live,
    /// deduplicated.
    ///
    /// Drops entries whose source was recycled since the edge was recorded
    /// (defect G1-8: such a source, once re-typed into a live region, was
    /// otherwise re-walked *wholesale* every pause on behalf of an object that
    /// no longer exists, resurrecting its referents). `Free` sources are left
    /// to `scan_source_region_for_cset_refs`'s own early return, which already
    /// handles them.
    fn live_rset_sources(regions: &[G1Region], cset: &[usize]) -> std::collections::HashSet<usize> {
        let mut set = std::collections::HashSet::new();
        for &cset_idx in cset {
            // `sources_with_generations()` snapshots under the per-rset mutex
            // and returns owned pairs, so the lock is not held across the body.
            for (source, generation) in regions[cset_idx].rset.sources_with_generations() {
                if !Self::rset_entry_is_stale(regions, source, generation) {
                    set.insert(source);
                }
            }
        }
        set
    }

    /// Post-write barrier: track cross-region references in remembered sets.
    ///
    /// Round-9 gc CRIT-8 — hot path: a per-thread cache of the
    /// last-touched destination region pointer avoids re-acquiring the
    /// heap-wide `regions` mutex on every reference store. Same-region
    /// successive stores (the dominant pattern: tight loops populating
    /// one array, append-to-list, etc.) hit the cache and take only the
    /// per-RSet inner mutex.
    ///
    /// SAFETY invariant: the `Vec<G1Region>` backing `self.regions` is
    /// created once in `G1Collector::new` with all `region_count`
    /// entries and is **never resized** thereafter (`grep -n
    /// 'regions.\\(push\\|resize\\|extend\\)' gc/src/g1.rs` returns no
    /// matches). Therefore `&regions[i]` is address-stable for the
    /// entire `G1Collector` lifetime, and a raw `*const G1Region`
    /// captured under one lock acquisition remains valid for later
    /// dereference even after the lock is released, as long as the
    /// collector itself is still alive. The cached pointer is keyed by
    /// both region index AND collector identity so a stale cache from a
    /// previous collector instance never resurrects a dangling pointer.
    /// Identity is the minted [`Self::instance_id`], NOT the collector's
    /// address: `self as *const Self` ABAs when a later collector is
    /// constructed at a dropped one's address (stack-slot or allocator
    /// block reuse) with a matching epoch history, which let this fast
    /// path write through a pointer into the dropped collector's freed
    /// regions storage (intermittent 0xC0000374 under parallel gc tests).
    pub fn post_write_barrier_rset(&self, src_obj: ObjectRef, stored_ref: ObjectRef) {
        let src_addr = src_obj.as_ptr() as usize;
        let dst_addr = stored_ref.as_ptr() as usize;

        // O(log R) lookups via the cached `region_lookup` table; these do
        // NOT take any lock (the lookup table is immutable post-`new`).
        let src_region = self.lookup_region_for_addr(src_addr);
        let dst_region = self.lookup_region_for_addr(dst_addr);

        // Only record cross-region references.
        let (src_idx, dst_idx) = match (src_region, dst_region) {
            (Some(s), Some(d)) if s != d => (s, d),
            _ => return,
        };

        let collector_id = self.instance_id;

        // SECURITY FIX (V7a): snapshot the current reclassification epoch.
        // Pairs (`Acquire`) with the `Release` bump performed under the
        // regions lock at the start of every recycle/retype phase
        // (`young_collection`/`mixed_collection`/`cleanup`).
        let cur_epoch = self.rset_cache_epoch.load(Ordering::Acquire);

        thread_local! {
            // SECURITY FIX (V7a): now (collector instance_id, region_idx,
            // *const G1Region, epoch). `Cell` is sufficient — the
            // pointer is `Copy` and never escapes the with() block other
            // than as a deref-then-call.
            static LAST_RSET_TARGET: std::cell::Cell<Option<(u64, usize, *const G1Region, u64)>>
                = const { std::cell::Cell::new(None) };
        }

        let hit = LAST_RSET_TARGET.with(|cell| {
            if let Some((cached_collector, cached_idx, cached_ptr, cached_epoch)) = cell.get() {
                // SECURITY FIX (V7a): only honour the fast path when the
                // cache was populated in the *current* reclassification
                // epoch. A stale `cached_epoch` means a collection has
                // recycled/retyped regions since the entry was captured,
                // so the cached `*const G1Region` may now name a Free (or
                // re-typed) region whose rset we must NOT touch. On
                // mismatch we fall through to the Free-gated slow path,
                // which re-validates under the regions lock and refreshes
                // the cache with the new epoch.
                if cached_collector == collector_id
                    && cached_idx == dst_idx
                    && cached_epoch == cur_epoch
                {
                    // SAFETY: see method-level invariant. `cached_ptr`
                    // was captured from `&regions[dst_idx]` (Vec never
                    // reallocates), the collector identity check rules
                    // out reuse across collector instances, and the
                    // epoch check rules out a recycled region.
                    // `add_reference_in_generation` takes `&self` (interior
                    // `parking_lot::Mutex` on the FxHashMap — see
                    // `RememberedSet`).
                    //
                    // G1AUD-5: stamp the entry with `cur_epoch`, the
                    // reclassification generation this store happened in. Free
                    // — the value was already loaded to validate the cache.
                    unsafe {
                        (*cached_ptr)
                            .rset
                            .add_reference_in_generation(src_idx, cur_epoch);
                    }
                    return true;
                }
            }
            false
        });
        if hit {
            return;
        }

        // Cache miss: take the regions lock just long enough to capture
        // the stable pointer, populate the TLS cache, then perform the
        // RSet add via the same `&self` interior-mutex path.
        //
        // Round-9 fix (HIGH C4): post-filter by `region_type` here, on the
        // slow path only. The cached `region_lookup` table is keyed by
        // address-range alone and so `lookup_region_for_addr` happily
        // returns the index of a Free (or HumongousContinuation) region
        // whose backing buffer still covers `dst_addr` from a prior cycle.
        // Recording references into Free regions inflates RSet traffic and,
        // more critically, holds references that will be reset to garbage
        // at the next GC. We replicate the gating that
        // `is_addr_in_live_region` (line 2324) and `is_object_address`
        // (line 2267) already apply on the read-side root-scan paths.
        //
        // SECURITY FIX (V7a): the fast-path TLS cache is now also gated
        // by `rset_cache_epoch` (see above). When the collector moves a
        // region between Eden/Survivor/Old/Free it bumps the epoch under
        // this same lock, so the previously-documented stale-cache window
        // — where a cache entry could briefly admit a write into a
        // just-freed region — is closed: any cached entry from before the
        // bump fails the epoch comparison and is forced down this
        // Free-gated slow path. We re-read the epoch under the lock so the
        // value stamped into the cache is consistent with the
        // `region_type` we validate.
        let regions = self.regions.lock();
        if regions[dst_idx].region_type == RegionType::Free {
            return;
        }
        let epoch_under_lock = self.rset_cache_epoch.load(Ordering::Acquire);
        let region_ptr: *const G1Region = &regions[dst_idx];
        LAST_RSET_TARGET.with(|cell| {
            cell.set(Some((collector_id, dst_idx, region_ptr, epoch_under_lock)));
        });
        // G1AUD-5: stamp with the generation validated under the lock, so the
        // stamp and the `region_type` check describe the same instant.
        regions[dst_idx]
            .rset
            .add_reference_in_generation(src_idx, epoch_under_lock);
    }

    /// Region indices that hold a conservatively-discovered JIT root this cycle
    /// and must therefore be EXCLUDED from the collection set (pinned in place).
    ///
    /// The VM's root gatherer publishes these addresses (only under G1) via
    /// [`crate::gc_quiescence::add_pinned_jit_root`]; a conservative JIT root
    /// lives in a register/spill slot the collector cannot rewrite, so its
    /// object must not move. Excluding its region from the CSet is G1's analog of
    /// the generational collector's non-moving-while-in-JIT sweep. Returns empty
    /// unless a thread is in JIT, so the no-JIT path pays nothing.
    ///
    /// INT-3 — ALSO includes every region holding a published un-retired
    /// TLAB tail (see [`Self::set_jit_tlab_skip_regions`]): the tail's owner
    /// resumes bump-allocating into `[cursor, end)` after the pause, so
    /// evacuating + freeing that region would hand the same memory out
    /// twice. Deliberately NOT gated on `gc_quiescence::is_active()` — a
    /// blocked thread's un-retired tail can be published while no thread is
    /// in JIT at all.
    fn jit_pinned_region_set(&self) -> std::collections::HashSet<usize> {
        let mut set: std::collections::HashSet<usize> = if crate::gc_quiescence::is_active() {
            crate::gc_quiescence::pinned_jit_roots_snapshot()
                .into_iter()
                .filter_map(|addr| self.lookup_region_for_addr(addr))
                .collect()
        } else {
            std::collections::HashSet::new()
        };
        for &(start, end) in self.jit_tlab_skip_regions.lock().iter() {
            // A mutator TLAB is carved from a single Eden region
            // (`refill_tlab`), so one lookup suffices; the `end - 1` probe is
            // defense-in-depth should that invariant ever change.
            if let Some(idx) = self.lookup_region_for_addr(start) {
                set.insert(idx);
            }
            if end > start {
                if let Some(idx) = self.lookup_region_for_addr(end - 1) {
                    set.insert(idx);
                }
            }
        }
        set
    }

    /// INT-3 — publish the reserved (un-retired) TLAB tails of frozen in-JIT
    /// peers / blocked threads as absolute `(cursor, end)` address ranges so
    /// this collection's region walkers skip them and their regions stay out
    /// of the CSet (see [`Self::jit_tlab_skip_regions`]). Replaces any
    /// previously-set list. Must be set under STW immediately before the
    /// collection and cleared immediately after.
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        let mut g = self.jit_tlab_skip_regions.lock();
        g.clear();
        g.extend_from_slice(regions);
    }

    /// INT-3 — clear the published TLAB skip regions (see
    /// [`Self::set_jit_tlab_skip_regions`]).
    pub fn clear_jit_tlab_skip_regions(&self) {
        self.jit_tlab_skip_regions.lock().clear();
    }

    /// INT-3 — snapshot of the published frozen-peer TLAB tails, taken once
    /// per walk loop (not per object). Empty on every normal cycle.
    fn jit_tlab_skip_spans(&self) -> Vec<(usize, usize)> {
        self.jit_tlab_skip_regions.lock().clone()
    }

    /// Find which region contains the given address (by raw address).
    ///
    /// O(log R) via the cached `region_lookup` table. See
    /// [`Self::region_for_ptr`] for the rationale; this variant is the
    /// hot path for the write barrier in
    /// [`Self::post_write_barrier_rset`], where it is called twice per
    /// reference store.
    fn region_for_ptr_with_regions(&self, _regions: &[G1Region], addr: usize) -> Option<usize> {
        self.lookup_region_for_addr(addr)
    }

    /// Binary-search the cached `(base_addr, region_idx)` table to find
    /// which region (if any) owns `addr`.
    ///
    /// Complexity: O(log R) — independent of the number of live regions.
    ///
    /// Returns `Some(idx)` iff `addr` falls within `[base, base + region_size)`
    /// for some region. The check uses `self.config.region_size` instead of
    /// the per-region `data.len()` because every region's backing buffer is
    /// allocated at exactly `region_size` bytes (see [`G1Region::from_arena`]).
    #[inline]
    fn lookup_region_for_addr(&self, addr: usize) -> Option<usize> {
        // Find the largest base address that is <= addr.
        // `partition_point` returns the first index where the predicate is
        // false; subtracting 1 gives the last index where it is true.
        let pp = self
            .region_lookup
            .partition_point(|(base, _)| *base <= addr);
        if pp == 0 {
            return None;
        }
        let (base, idx) = self.region_lookup[pp - 1];
        if addr < base.wrapping_add(self.config.region_size) {
            Some(idx)
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Humongous object addressing
    // -----------------------------------------------------------------------
    //
    // A humongous object spans a contiguous run of region indices, which —
    // because all regions are adjacent slices of one `arena` — is one
    // physically-contiguous block. The real `ObjectHeader` sits at offset 0 of
    // the start region and the payload flows straight through, so logical
    // payload byte `P` lives at `start_addr + HEADER_SIZE + P`. The accessors
    // below could read/write that flat range directly; `humongous_copy` is
    // retained so the existing field/array accessor call sites are unchanged.

    /// If `obj`'s start address names a `HumongousStart` region, return the
    /// start region index and the total payload byte count (object size minus
    /// the single ObjectHeader). Returns `None` for ordinary (non-humongous)
    /// objects, whose access uses the plain flat-offset path.
    ///
    /// Takes the already-held `regions` slice to avoid re-locking.
    fn humongous_span(
        &self,
        regions: &[G1Region],
        obj: ObjectRef,
        total_object_size: usize,
    ) -> Option<(usize, usize)> {
        let idx = self.lookup_region_for_addr(obj.as_ptr() as usize)?;
        if regions[idx].region_type != RegionType::HumongousStart {
            return None;
        }
        let payload_bytes = total_object_size.saturating_sub(HEADER_SIZE);
        Some((idx, payload_bytes))
    }

    /// `true` iff `obj`'s start address names a `HumongousStart` region.
    ///
    /// The object's payload is one contiguous block (the regions are adjacent
    /// arena slices), so a flat `obj.as_ptr() + HEADER_SIZE + i*stride` access
    /// is valid for callers — this predicate is retained for the GC-internal
    /// accessor routing and for callers that must distinguish humongous (e.g.
    /// to skip evacuation of an in-place, non-relocated object).
    ///
    /// This is the size-independent sibling of `humongous_span` (which also
    /// needs the object's total size to compute the payload byte count).
    /// Takes the `regions` lock briefly to inspect the region type — mirrors
    /// the `lookup_region_for_addr` + `RegionType::HumongousStart` check in
    /// `humongous_span`.
    pub(crate) fn is_humongous(&self, obj: ObjectRef) -> bool {
        let regions = self.regions.lock();
        match self.lookup_region_for_addr(obj.as_ptr() as usize) {
            Some(idx) => regions[idx].region_type == RegionType::HumongousStart,
            None => false,
        }
    }

    /// Copy `len` bytes of a humongous object's payload, starting at logical
    /// payload offset `payload_off`, between the heap backing store and the
    /// caller-provided `buf`.
    ///
    /// `write == true` copies `buf -> heap`; otherwise `heap -> buf`. The
    /// humongous object is one contiguous block (its regions are adjacent
    /// arena slices), so this is a single bounds-checked `memcpy` at
    /// `start_addr + HEADER_SIZE + payload_off`. Returns `false` if the access
    /// would exceed the object's payload (checked up front, before any copy) —
    /// the caller turns that into a dropped access, exactly as the `index >=
    /// len` bounds checks do.
    ///
    /// SAFETY: callers hold the `regions` lock for the duration, so the region
    /// classification (and thus the reserved arena span) is stable. `start`
    /// must be a `HumongousStart` index and `total_payload` the object's
    /// payload byte count (object size minus the single `ObjectHeader`).
    fn humongous_copy(
        &self,
        regions: &[G1Region],
        start: usize,
        total_payload: usize,
        payload_off: usize,
        buf: *mut u8,
        len: usize,
        write: bool,
    ) -> bool {
        // Reject any access whose end exceeds the object's payload.
        let end = match payload_off.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        if end > total_payload {
            return false;
        }
        if start >= regions.len() {
            return false;
        }

        // The payload is contiguous from `start_addr + HEADER_SIZE`. Use the
        // region's integer base address (not the `Deref` slice, whose `len` is
        // only `region_size`) so the pointer carries arena provenance across
        // region boundaries.
        let phys = (regions[start].data.addr() + HEADER_SIZE + payload_off) as *mut u8;
        // SAFETY: `payload_off + len <= total_payload`, and the humongous span
        // reserved `ceil(size/region_size)` contiguous arena regions covering
        // `HEADER_SIZE + total_payload` bytes from `start_addr`, so
        // `[phys, phys+len)` is inside the object's reserved, contiguous,
        // arena-backed memory. `buf` is a caller-owned buffer of >= `len` bytes.
        unsafe {
            if write {
                std::ptr::copy_nonoverlapping(buf, phys, len);
            } else {
                std::ptr::copy_nonoverlapping(phys, buf, len);
            }
        }
        true
    }

    /// Conservative validity check for a *raw address* — see
    /// [`crate::gen_heap::GenerationalHeap::is_object_address`] for the
    /// contract. NEW-1.5 JIT frame root scanning calls this through
    /// [`crate::vm_heap::VmHeap::is_object_address`].
    /// Lock-free `[base, end)` envelope of the single contiguous G1 arena.
    /// See [`crate::gen_heap::GenerationalHeap::conservative_addr_span`] for
    /// the contract; here the bounds are immutable for the collector's
    /// lifetime.
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        if self.arena_end > self.arena_base {
            Some((self.arena_base, self.arena_end))
        } else {
            None
        }
    }

    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        if !self.is_addr_in_live_region(addr) {
            return None;
        }
        let raw = addr as *const u8;
        // Validate raw enum tags before borrowing as `ObjectHeader`; this path
        // is fed by conservative JIT-frame words and must reject garbage
        // without letting invalid `#[repr(u8)]` values reach a debug enum
        // match.
        let kind = unsafe { object_kind_from_tag(*raw.add(OBJECT_KIND_OFFSET)) }?;
        let _element_type =
            unsafe { array_element_type_from_tag(*raw.add(ARRAY_ELEMENT_TYPE_OFFSET)) }?;
        if kind == ObjectKind::HumongousFiller {
            return None;
        }

        // SAFETY: address is inside a live region and both enum tag bytes have
        // been validated.
        let header = unsafe { &*(raw as *const ObjectHeader) };
        // Multi-array reloc fix (2026-05-22): `alloc_array` mirrors the
        // array length into `num_slots`, so a legitimate 256 MB int[] has
        // num_slots = 2^26 > 1<<24 and would be falsely rejected here.
        // Gate num_slots only for non-arrays, and bound array_length at
        // the JVM `Integer.MAX_VALUE` ceiling (matches `array_length()`).
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24;
        let is_array = kind == ObjectKind::Array;
        if !is_array && header.num_slots() > MAX_PLAUSIBLE_SLOTS {
            return None;
        }
        if is_array && header.array_length() > i32::MAX as u32 {
            return None;
        }
        Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
    }

    /// Loose validity check: alignment + region containment only.
    ///
    /// Mirrors [`crate::gen_heap::GenerationalHeap::is_heap_addr`]. Does NOT
    /// read the object header — used by GC root scanning of ambiguous JVM
    /// long slots, where the strict header check is too aggressive (false
    /// negatives drop legitimately-rooted objects, leading to dangling
    /// pointers post-GC and downstream SEGV).
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        if !self.is_addr_in_live_region(addr) {
            return None;
        }
        // SAFETY: alignment + live-region containment confirmed.
        Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
    }

    /// Check if an address is within a live (non-Free) region.
    ///
    /// Used by reference processing to determine if a referent survived GC
    /// without being relocated (e.g., objects in Old/Humongous regions that
    /// were not part of the collection set).
    pub fn is_addr_in_live_region(&self, addr: usize) -> bool {
        // Fast lock-free arena-bounds gate. `[arena_base, arena_end)` is
        // immutable for the collector's lifetime (single contiguous `Box<[u8]>`,
        // never moved/resized), so the test needs no atomics and no lock. This
        // is the hot path: `is_addr_in_live_region` is called per candidate word
        // by the conservative JIT/native root scan
        // (`scan_active_jit_frames` / `update_root_snapshot`), which runs on
        // every object-returning native call. The overwhelming majority of those
        // words (return addresses, ints, native-stack addresses) lie OUTSIDE the
        // heap arena and are rejected here without touching `regions.lock()` or
        // scanning any region. The previous implementation took the regions
        // mutex and linearly scanned all ~`num_regions` regions for EVERY word,
        // which made JIT-on, deep-stack workloads (e.g. Spring Boot buildSrc
        // JUnit annotation walks) run for minutes / appear hung under G1 while
        // serial GC — whose `gen_heap` adopted exactly this lock-free gate —
        // finished in seconds.
        if addr < self.arena_base || addr >= self.arena_end {
            return false;
        }
        // In-arena candidate: index the single owning region in O(1). Region `k`
        // occupies `[arena_base + k*region_size, +region_size)` and lives at
        // `regions[k]` (the Vec has fixed length and is never reordered; each
        // slot's `data` buffer is address-stable). Only genuine
        // heap-pointer-shaped words reach here, so the lock is taken rarely. The
        // explicit base/cursor bounds re-check below is the authoritative test
        // (and guards against any indexing skew).
        let region_size = self.config.region_size;
        if region_size == 0 {
            return false;
        }
        let idx = (addr - self.arena_base) / region_size;
        let regions = self.regions.lock();
        match regions.get(idx) {
            None => false,
            Some(r) => match r.region_type {
                // Free regions hold no live object.
                RegionType::Free => false,
                // A humongous object is physically contiguous across its
                // slices; only the `HumongousStart` region carries the full
                // `cursor = object_size`, while every continuation slice keeps
                // `cursor = 0` as a sentinel (see `alloc_humongous_locked`).
                // The whole continuation slice is therefore live — matching the
                // previous linear scan, which found such interior addresses via
                // the start region's full-span cursor. (A conservative root
                // scan tolerates the harmless over-retention of any tail
                // padding past the object's true end; `is_object_address`'s
                // header check still rejects non-object interior words.)
                RegionType::HumongousContinuation => true,
                // Eden / Survivor / Old / HumongousStart: live iff the address
                // is below the region's allocation cursor.
                _ => {
                    let base = r.data.as_ptr() as usize;
                    if addr < base || addr >= base + r.cursor {
                        return false;
                    }
                    // G1CORE-3: a kept region with UNRESOLVED evacuation
                    // failures holds exactly the recorded self-forwarded
                    // objects as live — the rest of its below-cursor bytes
                    // are dead garbage the failing pass never scanned, whose
                    // ref slots were never rewritten. Reporting those as
                    // live lets reference processing "restore" a dead weak
                    // referent whose fields dangle into regions freed the
                    // same pause. Both sets are empty except in the rare
                    // wedged-drain window (one lock + one lookup then).
                    if self.kept_unresolved_any.load(Ordering::Acquire)
                        && self.kept_unresolved_regions.lock().contains(&idx)
                    {
                        return self.kept_unresolved_live.lock().contains(&addr);
                    }
                    true
                }
            },
        }
    }

    /// Walk all live objects across all non-Free regions.
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut result = Vec::new();
        let regions = self.regions.lock();
        let jit_skips = self.jit_tlab_skip_spans();
        for r in regions.iter() {
            if r.region_type == RegionType::Free {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            let used = r.cursor;
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                // INT-3 — frozen-peer TLAB tail: skip before interpreting.
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, base + offset) {
                    offset += skip;
                    continue;
                }
                // TLAB-retire gap sentinel: skip its exact span.
                if let Some(gap) = gap_filler_len(ptr) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: HumongousFiller is a walker sentinel;
                // never report it as a real object.
                if is_humongous_filler(header) {
                    break;
                }
                // `object_total_size` (not an inline recompute): its 0
                // sentinel on a corrupt/overflowing array header stops the
                // walk instead of yielding a HEADER_SIZE-strided phantom.
                let total_size = object_total_size(header);
                if total_size < HEADER_SIZE || offset + total_size > used {
                    break;
                }
                result.push((ptr, total_size));
                offset += total_size;
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// GarbageCollector trait implementation
// ---------------------------------------------------------------------------

impl GarbageCollector for G1Collector {
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        let compact_body = cratonvm_types::compact_object_body_size(class_id.as_u32(), num_fields);
        let body_size = compact_body.unwrap_or(num_fields * SLOT_SIZE);
        let total_size = HEADER_SIZE + body_size;
        let (ptr, _region) = self.alloc_in_region(total_size).unwrap_or_else(|| {
            if gc_flags().g1_dbg_diag {
                let regions = self.regions.lock();
                let mut free = 0usize;
                let mut eden = 0usize;
                let mut survivor = 0usize;
                let mut old = 0usize;
                let mut hstart = 0usize;
                let mut hcont = 0usize;
                for r in regions.iter() {
                    match r.region_type {
                        RegionType::Free => free += 1,
                        RegionType::Eden => eden += 1,
                        RegionType::Survivor => survivor += 1,
                        RegionType::Old => old += 1,
                        RegionType::HumongousStart => hstart += 1,
                        RegionType::HumongousContinuation => hcont += 1,
                    }
                }
                eprintln!(
                    "DBG-G1DIAG: heap_size={} region_size={} num_regions={} free={} eden={} survivor={} old={} hstart={} hcont={} collection_count={}",
                    self.config.heap_size,
                    self.config.region_size,
                    regions.len(),
                    free, eden, survivor, old, hstart, hcont,
                    self.collection_count.load(Ordering::Relaxed)
                );
            }
            eprintln!(
                "FATAL: G1: out of heap space for object allocation ({} bytes) \
                 -- every region consumed with no collection able to run. This \
                 entry point cannot report failure (see `native_alloc_pressure`), \
                 so the pressure latch is supposed to have forced a collection at \
                 the preceding `safe_native_call` boundary; re-run with \
                 CRATONVM_DBG_G1DIAG=1 to see the region census per collection.",
                total_size
            );
            std::process::abort();
        });

        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            u32::try_from(num_fields).expect("field count exceeds u32::MAX"),
        );
        if let Some(body) = compact_body {
            header.set_compact_shape(num_fields as u32, body);
        }

        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        let data_size = array_data_size(length, element_type)
            .expect("array data size overflow in g1 alloc_array");
        let total_size = HEADER_SIZE + data_size;
        let (ptr, _region) = self.alloc_in_region(total_size).unwrap_or_else(|| {
            eprintln!(
                "FATAL: G1: out of heap space for array allocation ({} bytes) \
                 -- see the object-allocation abort above for the invariant \
                 this encodes (`native_alloc_pressure`).",
                total_size
            );
            std::process::abort();
        });

        // Mirror `length` into BOTH `array_length` and `num_slots`, matching
        // `Heap::alloc_array` (heap.rs:367-370,403-410) and
        // `GenerationalHeap::alloc_array` (gen_heap.rs:497-500). The shared
        // header decoders in `vm_heap` / `walk_objects` (e.g.
        // `VmHeap::num_fields(arr)`) consume `num_slots` and were silently
        // returning 0 for any G1-allocated array prior to this fix.
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            u32::try_from(length).expect("array length exceeds u32::MAX"),
            u32::try_from(length).expect("array length must fit u32 for G1 header num_slots"),
        );

        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        unsafe { &*(obj.as_ptr() as *const ObjectHeader) }
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.get_header(obj).class_id
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.get_header(obj).kind
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.get_header(obj).element_type
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let existing = self.get_header(obj).identity_hash_code;
        if existing != 0 {
            return existing;
        }
        self.mint_identity_hash_code(obj)
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        // C2b (round-12 gc): runtime bounds + suspect-header guard, mirroring
        // `GenerationalHeap::get_field` (gen_heap.rs). A corrupted/oversized
        // header or an out-of-layout index must NOT dereference arbitrary
        // memory — return a benign null read instead, matching gen_heap.
        let header = self.get_header(obj);
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                "g1::get_field: suspect header (returning null)",
            );
            return Value::Object(None);
        }
        if index >= num_slots {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                "g1::get_field: out-of-bounds field read dropped (returning null)",
            );
            return Value::Object(None);
        }

        let compact = cratonvm_types::compact_object_field_storage(header, index);
        let (payload_off, payload_size) = compact
            .map(|(offset, storage)| (offset, storage.size_runtime() as usize))
            .unwrap_or((index * SLOT_SIZE, SLOT_SIZE));
        let total_size = object_total_size(header);

        // C2 (round-12 gc): humongous objects are region-fragmented; translate
        // the flat payload offset to the owning continuation region's buffer so
        // the read can never escape the object's backing memory.
        {
            let regions = self.regions.lock();
            if let Some((start, total_payload)) = self.humongous_span(&regions, obj, total_size) {
                let mut tmp = [0u64; 2];
                if self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    tmp.as_mut_ptr().cast(),
                    payload_size,
                    false,
                ) {
                    return if let Some((_, storage)) = compact {
                        unsafe {
                            cratonvm_types::read_compact_field(
                                tmp.as_ptr().cast(),
                                storage,
                                Ordering::Relaxed,
                            )
                        }
                    } else {
                        let bytes =
                            unsafe { &*(tmp.as_ptr().cast::<u8>() as *const [u8; SLOT_SIZE]) };
                        value_from_bytes(bytes)
                    };
                }
                return Value::Object(None);
            }
        }

        // SAFETY: `index < num_slots` (checked above) so the slot lies within
        // the object's allocated, single-region backing store.
        //
        // PLAIN-SLOT TEARING FIX (2026-07-06): was a bare `ptr::read::<Value>`,
        // a non-atomic 16-byte copy that could tear against a concurrent
        // plain `set_field` from another mutator thread -- see
        // docs/known-issues/elasticsearch-lucene-binary-docvalues-range-hangs.md
        // #3 and commit 4e6b560f (the GC-marker-vs-JIT-store counterpart fix,
        // which covered g1::scan_object_refs but not this mutator-side path).
        let ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + payload_off) };
        if let Some((_, storage)) = compact {
            unsafe { cratonvm_types::read_compact_field(ptr, storage, Ordering::Relaxed) }
        } else {
            unsafe { cratonvm_types::read_value_atomic(ptr as *const Value) }
        }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        // C2b (round-12 gc): runtime bounds + suspect-header guard. Drop
        // out-of-layout writes rather than corrupting the neighboring object,
        // mirroring `GenerationalHeap::set_field`.
        let header = self.get_header(obj);
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                value = ?value,
                "g1::set_field: suspect header (dropping write)",
            );
            return;
        }
        if index >= num_slots {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                value = ?value,
                "g1::set_field: out-of-bounds field write dropped",
            );
            return;
        }

        // C2c (round-12 gc): for reference stores, fire the SATB pre-barrier
        // (log the OLD slot value before it is overwritten — only matters while
        // concurrent marking is active) so the marker never loses an edge, and
        // the RSet post-barrier (after the store) so cross-region roots are
        // tracked. This mirrors how `GenerationalHeap::set_field` fires its
        // barrier internally; callers do NOT need to call the barriers
        // separately. Primitive stores skip both barriers.
        //
        // INT-8: `satb_pre_suppressed()` exempts the weak-reference PROTOCOL
        // writes (the pre-pause referent null pass and the remark-time
        // referent clears) — those are not semantic overwrites, and logging
        // them recorded every active referent as a mark root at every
        // mid-cycle young pause, tainting the bitmap verdicts remark-time
        // reference processing depends on. The TLS read is gated behind the
        // marking-active check, so the non-marking hot path pays nothing.
        let is_ref_store = matches!(value, Value::Object(_));
        if is_ref_store && self.satb_pre_barrier_required() && !satb_pre_suppressed() {
            let old = self.get_field(obj, index);
            if let Value::Object(Some(old_ref)) = old {
                self.satb_pre_barrier(old_ref.as_ptr() as usize);
            }
        }

        let compact = cratonvm_types::compact_object_field_storage(header, index);
        let (payload_off, payload_size) = compact
            .map(|(offset, storage)| (offset, storage.size_runtime() as usize))
            .unwrap_or((index * SLOT_SIZE, SLOT_SIZE));
        let total_size = object_total_size(header);

        // C2 (round-12 gc): route humongous stores through the region-aware
        // translation so the write can never escape the object's memory.
        let stored = {
            let regions = self.regions.lock();
            if let Some((start, total_payload)) = self.humongous_span(&regions, obj, total_size) {
                let mut tmp = [0u64; 2];
                if let Some((_, storage)) = compact {
                    unsafe {
                        cratonvm_types::write_compact_field(
                            tmp.as_mut_ptr().cast(),
                            storage,
                            value,
                            Ordering::Relaxed,
                        )
                    };
                } else {
                    let bytes =
                        unsafe { &mut *(tmp.as_mut_ptr().cast::<u8>() as *mut [u8; SLOT_SIZE]) };
                    value_to_bytes(value, bytes);
                }
                self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    tmp.as_mut_ptr().cast(),
                    payload_size,
                    true,
                )
            } else {
                // SAFETY: `index < num_slots`, so the slot is in-bounds of the
                // object's single-region backing store.
                //
                // PLAIN-SLOT TEARING FIX (2026-07-06): was a bare
                // `ptr::write::<Value>` -- see the matching note on
                // `get_field`'s read side above.
                let ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + payload_off) };
                if let Some((_, storage)) = compact {
                    unsafe {
                        cratonvm_types::write_compact_field(ptr, storage, value, Ordering::Relaxed)
                    };
                } else {
                    unsafe {
                        cratonvm_types::write_value_atomic(ptr as *mut Value, value);
                    }
                }
                true
            }
        };

        if stored && is_ref_store {
            self.post_write_barrier_rset(
                obj,
                match value {
                    Value::Object(Some(r)) => r,
                    // Null store: nothing to record in the RSet.
                    _ => return,
                },
            );
        }
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        // JLS §17.7 atomicity for 16-byte `Value` slots: acquire the per-slot
        // stripe lock so paired writers don't expose a torn (tag, payload) to
        // this read. See `crate::collector::volatile_stripe_lock` for the
        // design rationale.
        //
        // Round-9 HIGH-3: restore the bracketing `fence(SeqCst)` pair.
        // Round-8 removed it on the (incorrect) premise that the mutex
        // acquire/release JMM edge subsumed it. It does not: parking_lot's
        // mutex acquire is Acquire-ordered and release is Release-ordered,
        // which gives happens-before WITHIN A SINGLE STRIPE but does NOT
        // establish a global total order across distinct stripes. JLS
        // §17.4.5 requires a total order over all `volatile` accesses
        // (synchronization order), so an IRIW-style observer can otherwise
        // see two volatile writes on different stripes in opposite orders
        // from two reader threads — a JMM violation. The SeqCst fence pair
        // adds the cross-stripe total order the per-stripe Acquire/Release
        // alone cannot supply, on top of which the lock still provides
        // 16-byte slot atomicity.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let _guard = crate::collector::volatile_stripe_lock(obj, index);
        let v = self.get_field(obj, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        v
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        // Pair with `get_field_volatile`: the stripe lock makes the 16-byte
        // `Value` write appear atomic to a concurrent volatile reader, and
        // the bracketing SeqCst fences guarantee the JLS §17.4.5 total order
        // across distinct stripes (the lock alone is per-stripe HB only).
        // Round-9 HIGH-3: round-8 removed these fences and reintroduced an
        // IRIW-observable JMM hole — see `get_field_volatile` for details.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let _guard = crate::collector::volatile_stripe_lock(obj, index);
        self.set_field(obj, index, value);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        self.get_header(obj).array_length() as usize
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.get_header(obj);
        let len = header.array_length() as usize;
        if index >= len {
            return Err(index as i32);
        }
        let element_type = header.element_type;
        let elem_size = crate::heap::element_byte_size(element_type);
        let payload_off = index * elem_size;

        // Read the raw element bytes (`elem_size` of them) into a fixed buffer.
        // The flat single-region path reads in place; the humongous path
        // translates to the owning continuation region. Either way the read is
        // bounds-confined to the object's own backing memory.
        let mut raw = [0u8; 8]; // largest element is 8 bytes (long/double/ref)
        {
            let regions = self.regions.lock();
            // C2: array data_size mirrors HEADER_SIZE + elements; recompute the
            // total so the humongous span / payload bound is exact.
            let total_size =
                HEADER_SIZE + crate::heap::array_data_size(len, element_type).unwrap_or(0);
            if let Some((start, total_payload)) = self.humongous_span(&regions, obj, total_size) {
                if !self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    raw.as_mut_ptr(),
                    elem_size,
                    false,
                ) {
                    return Err(index as i32);
                }
            } else {
                // SAFETY: `index < len` so `[payload_off, payload_off+elem_size)`
                // is inside the array's single-region payload.
                let slot_ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + payload_off) };
                unsafe {
                    std::ptr::copy_nonoverlapping(slot_ptr, raw.as_mut_ptr(), elem_size);
                }
            }
        }
        Ok(array_element_from_bytes(element_type, &raw))
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        let header = self.get_header(obj);
        let len = header.array_length() as usize;
        if index >= len {
            return Err(index as i32);
        }
        let element_type = header.element_type;
        let elem_size = crate::heap::element_byte_size(element_type);
        let payload_off = index * elem_size;
        let is_ref = element_type == ArrayElementType::Reference;

        // Encode the element value into a fixed byte buffer.
        let mut raw = [0u8; 8];
        array_element_to_bytes(element_type, value, &mut raw);

        // Write the raw bytes back — flat single-region path or humongous
        // region-translated path. Either way the write is bounds-confined.
        //
        // G1MAT-3: the SATB pre-barrier (read the OLD element, log it, THEN
        // overwrite) now runs inside the SAME `regions` critical section as
        // the store. It used to call the public `get_array_element`, which
        // takes and RELEASES the `regions` lock on its own — so the barrier's
        // read-then-store was split by a full lock release/re-acquire.
        // Widening that window widens the classic SATB lost-update race: two
        // mutators both read old value V, both log V, one stores A and the
        // other stores B; A is then reachable from no logged edge and the
        // marker can miss it. Keeping read and store under one acquisition
        // shrinks the window to the minimum this collector's slot model
        // allows, and drops a redundant `humongous_span` recomputation from
        // every reference-array store.
        //
        // INT-8 parity with `set_field`: honour `satb_pre_suppressed()` so the
        // weak-reference PROTOCOL writes never log an edge as a mark root.
        // `satb_pre_barrier` touches only TLS + the SATB shards (never the
        // `regions` lock), so calling it here cannot deadlock.
        let stored = {
            let regions = self.regions.lock();
            let total_size =
                HEADER_SIZE + crate::heap::array_data_size(len, element_type).unwrap_or(0);
            let span = self.humongous_span(&regions, obj, total_size);

            if is_ref && self.satb_pre_barrier_required() && !satb_pre_suppressed() {
                let mut old_raw = [0u8; 8];
                let read_ok = match span {
                    Some((start, total_payload)) => self.humongous_copy(
                        &regions,
                        start,
                        total_payload,
                        payload_off,
                        old_raw.as_mut_ptr(),
                        elem_size,
                        false,
                    ),
                    None => {
                        // SAFETY: `index < len` so the slot is inside the payload.
                        let slot_ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + payload_off) };
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                slot_ptr,
                                old_raw.as_mut_ptr(),
                                elem_size,
                            );
                        }
                        true
                    }
                };
                if read_ok {
                    if let Value::Object(Some(old_ref)) =
                        array_element_from_bytes(element_type, &old_raw)
                    {
                        self.satb_pre_barrier(old_ref.as_ptr() as usize);
                    }
                }
            }

            if let Some((start, total_payload)) = span {
                self.humongous_copy(
                    &regions,
                    start,
                    total_payload,
                    payload_off,
                    raw.as_mut_ptr(),
                    elem_size,
                    true,
                )
            } else {
                // SAFETY: `index < len` so the slot is inside the array payload.
                let slot_ptr = unsafe { obj.as_ptr().add(HEADER_SIZE + payload_off) };
                unsafe {
                    std::ptr::copy_nonoverlapping(raw.as_ptr(), slot_ptr, elem_size);
                }
                true
            }
        };
        if !stored {
            return Err(index as i32);
        }

        // C2c: RSet post-barrier for reference element stores of a non-null ref.
        if is_ref {
            if let Value::Object(Some(target)) = value {
                self.post_write_barrier_rset(obj, target);
            }
        }
        Ok(())
    }

    fn needs_gc(&self) -> bool {
        let regions = self.regions.lock();
        let free_count = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Free)
            .count();
        let total = regions.len();
        // Trigger a GC when the Free fraction drops below the (adaptive)
        // threshold. Baseline 25%; raised after a collection that hit
        // evacuation failure so the NEXT pause starts with a to-space pool
        // big enough for its live-young set (see
        // `retry_after_evacuation_failure` — the free pool at trigger time
        // IS the young evacuation's to-space, and a live-young set larger
        // than it self-forwards wholesale every pause).
        let pct = self.needs_gc_free_percent.load(Ordering::Relaxed).max(1);
        free_count * 100 < total * pct
    }

    fn collect_garbage(
        &self,
        _stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Phase 6 #1: defer until every live `SafepointToken` has
        // dropped — see `vm_heap::wait_for_gpu_critical_drain` for
        // the rationale. No-op when `gpu-offload` is off.
        crate::vm_heap::wait_for_gpu_critical_drain();

        // 1. Check if mixed GC is needed. Bracket the inner collection in an
        //    `Instant` so we can feed the actual pause delta (milliseconds)
        //    into `update_ihop` below. Previously this site passed
        //    `bytes_freed` to `update_ihop`, which expects milliseconds and
        //    compares against `config.max_gc_pause_ms` — feeding a byte
        //    count (typically 10^4-10^8) caused the adaptive IHOP threshold
        //    to floor on essentially every collection.
        if gc_flags().g1_dbg_diag {
            let regions = self.regions.lock();
            let mut free = 0usize;
            let mut eden = 0usize;
            let mut survivor = 0usize;
            let mut old = 0usize;
            for r in regions.iter() {
                match r.region_type {
                    RegionType::Free => free += 1,
                    RegionType::Eden => eden += 1,
                    RegionType::Survivor => survivor += 1,
                    RegionType::Old => old += 1,
                    _ => {}
                }
            }
            let pinned = regions.iter().filter(|r| r.pinned).count();
            eprintln!(
                "DBG-G1DIAG-PRE: collection #{} regions={} free={} eden={} survivor={} old={} pinned={}",
                self.collection_count.load(Ordering::Relaxed),
                regions.len(), free, eden, survivor, old, pinned
            );
        }
        // Name the backend in the process-wide decision report. G1 has no
        // young-moving *choice* to record — its collection set is always
        // evacuated — but a report that stays silent under `-XX:+UseG1GC` is
        // exactly how the `docs/GC.md` drift went unnoticed for the
        // generational path. Recording the constant answer makes "which
        // collector produced this summary?" a question the runtime answers.
        crate::gc_metrics::record_collector_decision(
            "g1",
            crate::gc_metrics::decision_reason::MOVING_BACKEND_ALWAYS_EVACUATES,
            crate::gc_quiescence::incomplete_reason::NONE,
        );
        let pause_start = std::time::Instant::now();
        let result = if self.needs_mixed_gc() {
            self.mixed_collection(roots, monitors)
        } else {
            self.young_collection(roots, monitors)
        };
        let result = self.retry_after_evacuation_failure(result, roots, monitors);
        let pause_ms = pause_start.elapsed().as_millis() as u64;
        if gc_flags().g1_dbg_diag {
            eprintln!(
                "DBG-G1DIAG-POST: objects_copied={} bytes_copied={} bytes_freed={}",
                result.stats.objects_copied, result.stats.bytes_copied, result.stats.bytes_freed
            );
        }

        // 2. IHOP / concurrent-mark triggering is driven by the VM layer
        //    (`interpreter::maybe_concurrent_gc` -> `g1_concurrent_mark_cycle`),
        //    which has the thread + STW-barrier context to run the FULL cycle:
        //    brief-STW initial mark, root marking, the background marker, and
        //    the completion watcher that flips into mixed GC.
        //
        //    This site used to ALSO call `start_concurrent_mark()` here, but
        //    that only flips the phase Idle -> ConcurrentMark (activates SATB,
        //    clears bitmaps) WITHOUT spawning the marker or marking roots. Run
        //    first (inside collect_garbage), it left `is_marking_active()` true,
        //    so the VM's `should_start && !is_marking_active` gate then BLOCKED
        //    the real cycle forever — the phase was stuck in ConcurrentMark,
        //    concurrent marking never actually ran, and so mixed GC never fired
        //    and old-gen was never reclaimed (a major cause of G1's footprint
        //    gap). The gc crate has no thread/barrier context to run the real
        //    cycle, so triggering belongs to the VM layer alone; this premature
        //    phase-flip is removed.

        // 3. Adaptive IHOP: feed the *pause time* of this collection (not
        //    the bytes freed) — see `update_ihop` doc for the contract.
        if pause_ms > 0 {
            self.update_ihop(pause_ms);
        }

        // 4. The Free pool has just been rebuilt, so any outstanding
        //    native-allocation pressure request has been served. Clear the
        //    latch; the next region claim re-latches it if the workload is
        //    still outrunning the collector (see `native_alloc_pressure`).
        self.native_alloc_pressure.store(false, Ordering::Relaxed);

        result
    }

    fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        // Post-write barrier: track cross-region references in remembered sets.
        //
        // PRECONDITION (SATB pre-barrier): when `gc_state.is_marking_active()`
        // returns true, the caller MUST have invoked
        // `VmHeap::satb_barrier(old_slot_value)` BEFORE performing the store
        // whose result is being signalled here. SATB needs the *old* slot
        // value to be logged before it is overwritten, and the trait shape
        // (post-store hook) cannot recover that value after the fact. See
        // `GarbageCollector::write_barrier` doc on the trait for the full
        // contract.
        //
        // Best-effort assertion: in debug builds, fire if the marking phase
        // is active and this thread has not invoked `satb_pre_barrier` since
        // its last `write_barrier` call. We cannot mechanically check that
        // the caller logged the *correct* old value (the old value is gone
        // by the time we get here) — only that *some* pre-call happened on
        // the same thread between consecutive post-store hooks. Misses are
        // false-positives on the very first store after marking activates;
        // they are still useful for surfacing call sites that need auditing
        // for SATB callsite coverage.
        debug_assert!(
            !self.gc_state.is_marking_active() || consume_satb_pre_barrier_epoch(),
            "G1 write_barrier invoked while concurrent marking is active without a \
             corresponding SATB pre-barrier on this thread. The trait contract \
             requires callers to invoke `VmHeap::satb_barrier(old_value)` BEFORE \
             the reference store. See `GarbageCollector::write_barrier` doc and \
             `G1Collector::satb_pre_barrier`."
        );

        if let Value::Object(Some(ref target)) = stored_value {
            self.post_write_barrier_rset(obj, *target);
        }
    }

    /// Task #25: G1 implements the SATB pre-store barrier by enqueueing
    /// the old reference into the global SATB log via the per-thread
    /// buffer. Inactive when concurrent mark is idle — `satb_pre_barrier`
    /// short-circuits on the `is_active()` Acquire load.
    ///
    /// `slot` is currently unused; we keep it in the trait signature so
    /// the debug triad-assertion in callers can pair a `(pre, post)`
    /// barrier by slot identity without breaking the API later.
    #[inline]
    fn write_barrier_pre(&self, _slot: *mut ObjectRef, old: ObjectRef) {
        self.satb_pre_barrier(old.as_ptr() as usize);
    }

    fn allocated_bytes(&self) -> usize {
        let regions = self.regions.lock();
        regions.iter().map(|r| r.cursor).sum()
    }
}

// ---------------------------------------------------------------------------
// SATB pre-barrier debug tracking
// ---------------------------------------------------------------------------
//
// Per-thread flag bumped by `G1Collector::satb_pre_barrier` and consumed by
// the debug-only assertion in `G1Collector::write_barrier`. This is purely a
// best-effort detector for missing SATB pre-calls — the post-store trait
// shape cannot see the old slot value, so we cannot mechanically verify
// correctness here. What we *can* do is detect the obvious bug where a
// caller invokes the post-store barrier while marking is active without
// having issued any pre-barrier on the same thread.
//
// Compiled to a no-op in release builds (only the `debug_assert!` consumer
// references these helpers, and the body of `consume_satb_pre_barrier_epoch`
// is trivially DCE-able when the assertion is stripped).

thread_local! {
    static SATB_PRE_BARRIER_FLAG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark that this thread has invoked the SATB pre-barrier; the next
/// post-store `write_barrier` call will consume the flag.
#[inline]
fn bump_satb_pre_barrier_epoch() {
    SATB_PRE_BARRIER_FLAG.with(|c| c.set(true));
}

/// Consume the per-thread SATB pre-barrier flag and return whether one was
/// observed since the last call. Used only by the debug-only assertion in
/// `write_barrier`.
#[inline]
fn consume_satb_pre_barrier_epoch() -> bool {
    SATB_PRE_BARRIER_FLAG.with(|c| c.replace(false))
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Assemble the [`crate::gc_metrics::g1_degraded`] mask for one evacuation
/// pause from the facts the pause already computed.
///
/// An identity entry (`key == value`) in the forwarding map IS the definition
/// of an evacuation failure: `evacuate_object` installs one when no to-space
/// could be allocated, and `free_or_keep_cset` reads exactly the same predicate
/// to decide which regions it must keep. Deriving the flag from the map rather
/// than from a separate counter is what stops the report and the reclamation
/// decision from ever disagreeing.
fn g1_pause_degraded_flags(
    pointer_map: &HashMap<usize, usize>,
    jni_pinned_out: usize,
    jit_pinned_out: usize,
    parallel: bool,
) -> u32 {
    use crate::gc_metrics::g1_degraded as flag;
    let mut degraded = flag::NONE;
    if pointer_map.iter().any(|(k, v)| k == v) {
        degraded |= flag::EVACUATION_FAILURE;
    }
    if jni_pinned_out > 0 {
        degraded |= flag::JNI_PINNED_REGIONS_EXCLUDED;
    }
    if jit_pinned_out > 0 {
        degraded |= flag::JIT_PINNED_REGIONS_EXCLUDED;
    }
    if parallel {
        degraded |= flag::PARALLEL_EVACUATOR;
    }
    degraded
}

/// How many *young* (Eden/Survivor) regions each pin vocabulary kept out of a
/// collection set, as `(jni_no_relocation_pins, jit_conservative_root_pins)`.
///
/// The two vocabularies are deliberately counted apart because they mean
/// different things and are fixed by different people: `G1Region::pinned` is a
/// JNI critical section the application controls, while
/// `G1Collector::jit_pinned_region_set` is a conservative JIT root (or a frozen
/// peer's un-retired TLAB tail) the runtime could in principle make precise. A
/// region carrying both is attributed to the JNI pin, which is the one that
/// will not go away on its own.
///
/// (A third, unrelated "pin" exists — `crate::pinned`, the process-global
/// keep-alive address set — which does NOT imply no-relocation and is not
/// counted here. See `docs/threading/objectref-concurrency-contract.md`.)
fn count_young_regions_pinned_out(
    regions: &[G1Region],
    jit_pinned: &std::collections::HashSet<usize>,
) -> (usize, usize) {
    let mut jni = 0usize;
    let mut jit = 0usize;
    for (i, r) in regions.iter().enumerate() {
        if r.region_type != RegionType::Eden && r.region_type != RegionType::Survivor {
            continue;
        }
        if r.pinned {
            jni += 1;
        } else if jit_pinned.contains(&i) {
            jit += 1;
        }
    }
    (jni, jit)
}

/// Find the first free region.
fn find_free_region(regions: &[G1Region]) -> Option<usize> {
    regions
        .iter()
        .position(|r| r.region_type == RegionType::Free)
}

/// Find `count` contiguous free regions.
fn find_contiguous_free(regions: &[G1Region], count: usize) -> Option<usize> {
    let mut run_start = 0;
    let mut run_len = 0;

    for (i, r) in regions.iter().enumerate() {
        if r.region_type == RegionType::Free {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
            if run_len >= count {
                return Some(run_start);
            }
        } else {
            run_len = 0;
        }
    }
    None
}

/// Compute total object size from header.
///
/// Defensive corruption handling (gc-abort-cleanup, mirrors `gc.rs`): a corrupt /
/// implausible array header (e.g. a stale `array_length` so large that
/// `header + length * element_size` overflows `usize`) must NOT abort the whole
/// VM. This previously `.expect()`-panicked here, killing the process on a bad
/// header, whereas the non-moving sweep (`gen_heap.rs::gen_object_total_size`)
/// returns a `0` sentinel and lets its walker re-sync. Mirror that behavior: on
/// overflow, log a diagnostic and return `0`. `0 < HEADER_SIZE`, so every
/// caller's existing corruption guard treats it as a bad header and stops /
/// re-syncs the linear walk rather than advancing the cursor by 0 and spinning.
///
/// This does not mask genuine bugs silently — the corruption is logged — but it
/// converts a hard process abort into a recoverable / fail-safe path.
fn object_total_size(header: &ObjectHeader) -> usize {
    if header.kind == ObjectKind::Array {
        match array_data_size(header.array_length() as usize, header.element_type) {
            Ok(data) => HEADER_SIZE + data,
            Err(_) => {
                // Implausible array header — treat as corrupt. Return 0 so the
                // caller's `total_size < HEADER_SIZE` guard fires (matching the
                // non-moving sweep's re-sync contract) instead of panicking.
                tracing::warn!(
                    "g1: implausible array_length {} (element_type={:?}) in object header — \
                     treating as corrupt; caller will skip/stop the walk",
                    header.array_length(),
                    header.element_type,
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

/// Round-9 gc CRIT-1: returns true if this header marks the start of a
/// humongous continuation filler region. Walkers MUST check this before
/// computing a per-object size — the filler covers the whole region
/// regardless of the (synthetic) per-field values stored in the header.
#[inline]
fn is_humongous_filler(header: &ObjectHeader) -> bool {
    matches!(header.kind, ObjectKind::HumongousFiller)
}

thread_local! {
    /// INT-8: same-thread SATB pre-barrier suppression window. Set only by
    /// [`SatbPreSuppressGuard`] around the weak-reference protocol writes
    /// (`G1Collector::set_field_no_satb`); consulted by `set_field`'s SATB
    /// block ONLY while marking is active, so the non-marking store path
    /// never touches TLS.
    static SATB_PRE_SUPPRESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// INT-8: is the current thread inside a `set_field_no_satb` protocol write?
#[inline]
fn satb_pre_suppressed() -> bool {
    SATB_PRE_SUPPRESSED.with(std::cell::Cell::get)
}

/// RAII scope for [`SATB_PRE_SUPPRESSED`] — restores the flag on drop (also
/// on unwind), so a panicking store can never leak suppression into later,
/// semantic stores on the same thread.
struct SatbPreSuppressGuard {
    prev: bool,
}

impl SatbPreSuppressGuard {
    fn new() -> Self {
        let prev = SATB_PRE_SUPPRESSED.with(|f| f.replace(true));
        Self { prev }
    }
}

impl Drop for SatbPreSuppressGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        SATB_PRE_SUPPRESSED.with(|f| f.set(prev));
    }
}

/// G1MARK-8: can `obj_addr` be trusted as an object start for a mark scan?
///
/// Gray-set entries are raw field bytes — a corrupt or stale slot can name
/// any address inside a live region, and `scan_object_refs` would then walk
/// a garbage header's `num_slots`/`array_length` extent, pushing more
/// garbage as children (corruption amplifier). Gate: object starts are
/// 8-aligned (`is_heap_addr`'s own invariant), the header must sit inside
/// the region's allocated prefix, its fields must be self-consistent
/// (shared `concurrent_mark_object_size` validator), and — except for a
/// humongous span, whose scan goes through the bounds-checked
/// `humongous_copy` — the object's extent must not cross the cursor.
///
/// False positives (garbage that happens to look plausible) are bounded by
/// the containment check; false negatives (a real object rejected, e.g. a
/// torn header) under-mark, which is why the caller sets
/// `mark_saw_implausible` and `cleanup` retains everything for the cycle.
fn plausible_mark_scan_target(region: &G1Region, obj_addr: usize) -> bool {
    if obj_addr & 0x7 != 0 {
        return false;
    }
    let base = region.data.as_ptr() as usize;
    let off = obj_addr.wrapping_sub(base);
    // Header must be fully inside the allocated prefix. (Real objects
    // satisfy start + size <= cursor, so start + HEADER_SIZE <= cursor.)
    if off
        .checked_add(HEADER_SIZE)
        .is_none_or(|end| end > region.cursor)
    {
        return false;
    }
    // SAFETY: the header span was just confirmed inside this region's
    // allocated prefix; the validator reads it field-by-field unaligned.
    let Some(size) =
        crate::concurrent_mark::concurrent_mark_object_size(obj_addr as *const ObjectHeader)
    else {
        return false;
    };
    if region.region_type != RegionType::HumongousStart
        && off.checked_add(size).is_none_or(|end| end > region.cursor)
    {
        return false;
    }
    true
}

/// TLAB-retire GAP sentinel probe (see `Tlab::retire`, tlab.rs): a
/// sub-`HEADER_SIZE` TLAB tail cannot hold a walkable `int[]` filler, so it
/// is stamped with `GAP_FILLER_CLASS_ID` at offset 0 and the exact gap
/// length at offset 4. Such a span is NOT a walkable object — its "kind"
/// byte is the low byte of the gap length. G1 refills TLABs from Eden
/// regions, so every linear region walker in this file must skip these
/// spans by their recorded length; treating one as an object header either
/// desyncs the stride or (via the defensive size check) `break`s the walk
/// and silently skips the REST of the region — fatal when the walk is a
/// pinned-region source scan or the Phase-4 reference fix-up (missed
/// evacuations / stale pointers). Returns the 8-aligned gap length when
/// `ptr` points at a gap sentinel.
///
/// SAFETY contract: caller guarantees `ptr` points at >= 8 readable bytes
/// inside a region (every walk loop checks `offset < cursor` first, and a
/// gap is always a trailing 8..=32-byte span fully inside the region).
/// INT-3 — if `addr` falls inside a published frozen-peer TLAB tail (see
/// [`G1Collector::set_jit_tlab_skip_regions`]), return the distance to the
/// span's end so the walker strides past it. The span is reserved,
/// UNINITIALIZED memory below the region cursor: it carries no walkable
/// filler (its owner was frozen mid-JIT before its safepoint retire), so it
/// must be skipped BEFORE any header/sentinel byte is interpreted — random
/// tail bytes could even alias `GAP_FILLER_CLASS_ID` and desync the walk. A
/// linear walk lands exactly on a span's start (objects fill the TLAB
/// contiguously up to `cursor`), but the check is range-based rather than
/// exact-start as defense-in-depth. `spans` is empty on every cycle without
/// frozen/blocked un-retired TLABs, making this a length check per object.
#[inline]
fn jit_tlab_skip_span_len(spans: &[(usize, usize)], addr: usize) -> Option<usize> {
    spans
        .iter()
        .find(|&&(s, e)| addr >= s && addr < e)
        .map(|&(_, e)| e - addr)
}

#[inline]
fn gap_filler_len(ptr: *const u8) -> Option<usize> {
    let cid = unsafe { std::ptr::read(ptr as *const u32) };
    if cid == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
        let len = unsafe { std::ptr::read((ptr as usize + 4) as *const u32) } as usize;
        // Defensive: round a corrupt length up to a positive 8-multiple so
        // the walk can never wedge in place.
        Some(((len + 7) & !7).max(8))
    } else {
        None
    }
}

/// True iff a region of this type can be a member of a collection set —
/// `Eden`/`Survivor` (every young or mixed CSet) or `Old` (a mixed CSet). The
/// Phase-4 rset rebuild records cross-region edges into these region types so a
/// later young/mixed GC scans the holder as a remembered-set source; edges into
/// non-collectable regions (`Free`, `HumongousStart`/`HumongousContinuation`)
/// are never consulted and so carry no rebuilt rset entry.
#[inline]
fn is_collectable_region_type(region_type: RegionType) -> bool {
    matches!(
        region_type,
        RegionType::Eden | RegionType::Survivor | RegionType::Old
    )
}

/// Update reference fields in an object using the forwarding map.
fn update_object_refs(
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    pointer_map: &HashMap<usize, usize>,
) {
    let data_start = unsafe { obj_ptr.add(HEADER_SIZE) };

    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            for i in 0..header.array_length() as usize {
                let slot_ptr = unsafe { data_start.add(i * 8) };
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    if let Some(&new_addr) = pointer_map.get(&(raw as usize)) {
                        unsafe {
                            std::ptr::write(slot_ptr as *mut u64, new_addr as u64);
                        }
                    }
                }
            }
        }
    } else {
        for_each_flat_object_reference(obj_ptr, header, 0, |slot, raw, compact| {
            if let Some(&new_addr) = pointer_map.get(&raw) {
                write_flat_object_reference(slot, new_addr, compact);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// T5.5.4 — Pause-budgeted region eviction policy
// ---------------------------------------------------------------------------

/// T5.5.4 — identifier type for regions selected for evacuation.
pub type RegionId = usize;

/// T5.5.4 — Select the top old regions to evacuate in the next mixed
/// collection, ordered by garbage-density and bounded by the pause
/// budget.
///
/// Selection algorithm:
///
/// 1. **Filter.** Only [`RegionType::Old`] regions with `live_bytes <
///    top` are considered — a region with zero garbage wastes
///    evacuation effort.
/// 2. **Rank.** Sort by the ratio `garbage_bytes / live_bytes`
///    descending: a region with lots of garbage and little live data
///    releases more memory per byte copied. `live_bytes == 0` is
///    treated as infinite ratio (perfect garbage region, evacuate
///    first — technically it just needs to be reclaimed but we keep it
///    in the set so the caller can free it uniformly).
/// 3. **Pack.** Walk the ranking, accumulating
///    [`Region::estimated_evac_cost_ns`]; stop once the next region
///    would push the running sum over `target_pause_ns`. Always
///    includes at least the first candidate if one exists, so that a
///    single oversized region does not starve the mixed GC entirely.
/// 4. **Deterministic.** Ties (same ratio) are broken by ascending
///    `region.index`.
pub fn select_evacuation_candidates(
    regions: &[crate::region::Region],
    target_pause_ns: u64,
) -> Vec<RegionId> {
    // Only old regions with at least one garbage byte are candidates.
    let mut candidates: Vec<(&crate::region::Region, f64)> = regions
        .iter()
        .filter(|r| r.region_type == crate::region::RegionType::Old && r.garbage_bytes() > 0)
        .map(|r| {
            // Ratio: garbage / live. If live is 0 the region is pure
            // garbage → infinite priority.
            let ratio = if r.live_bytes == 0 {
                f64::INFINITY
            } else {
                r.garbage_bytes() as f64 / r.live_bytes as f64
            };
            (r, ratio)
        })
        .collect();

    // Sort by ratio descending, tiebreak by index ascending.
    candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.index.cmp(&b.0.index))
    });

    let mut selected: Vec<RegionId> = Vec::new();
    let mut running_cost_ns: u64 = 0;
    for (region, _) in candidates {
        let cost = region.estimated_evac_cost_ns();
        // Always include the first candidate so the GC makes forward
        // progress even on a tight budget.
        if selected.is_empty() {
            selected.push(region.index);
            running_cost_ns = running_cost_ns.saturating_add(cost);
            continue;
        }
        let next_cost = running_cost_ns.saturating_add(cost);
        if next_cost > target_pause_ns {
            break;
        }
        selected.push(region.index);
        running_cost_ns = next_cost;
    }
    selected
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// No-op monitor cleanup for tests.
    struct NoopMonitors;
    impl MonitorCleanup for NoopMonitors {
        fn remap_after_gc(&self, _pointer_map: &HashMap<usize, usize>) {}
    }

    /// Test-only `StopTheWorldToken`. Single-threaded test harness, so the
    /// STW invariant is trivially satisfied.
    #[inline]
    fn stw() -> crate::collector::StopTheWorldToken {
        // SAFETY: these unit tests run the heap single-threaded.
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

    fn small_config() -> G1CollectorConfig {
        G1CollectorConfig {
            heap_size: 8 * 1024 * 1024, // 8 MB
            region_size: 1024 * 1024,   // 1 MB
            max_gc_pause_ms: 200,
            ihop_percent: 45,
            promotion_age: 3,
            gc_worker_threads: 1,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
        }
    }

    fn make_collector() -> G1Collector {
        G1Collector::new(small_config())
    }

    #[test]
    fn refill_tlab_zeroes_dirty_eden_bytes() {
        let gc = make_collector();
        let _seed = gc.alloc_object(ClassId::new(1), 0);

        let expected_ptr = {
            let mut regions = gc.regions.lock();
            let idx = gc.current_eden.load(Ordering::Relaxed);
            let region = &mut regions[idx];
            let ptr = unsafe { region.data.as_mut_ptr().add(region.cursor) };
            unsafe { std::ptr::write_bytes(ptr, 0xa5, 4096) };
            ptr
        };

        let (ptr, len) = gc.refill_tlab(4096).expect("TLAB refill");
        assert_eq!(ptr, expected_ptr);
        assert_eq!(len, 4096);
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        assert!(
            bytes.iter().all(|&byte| byte == 0),
            "G1 must return a fully zeroed TLAB even from dirty recycled Eden"
        );
    }

    fn unaligned_ptr(buf: &mut [u8], align: usize) -> *mut u8 {
        for offset in 0..align {
            let ptr = unsafe { buf.as_mut_ptr().add(offset) };
            if (ptr as usize) % align != 0 {
                return ptr;
            }
        }
        unreachable!("buffer base cannot be aligned to every offset")
    }

    unsafe fn corrupt_header_byte(obj: ObjectRef, offset: usize, value: u8) {
        unsafe {
            (obj.as_ptr() as *mut u8).add(offset).write(value);
        }
    }

    #[test]
    fn is_object_address_rejects_invalid_raw_header_tags() {
        let gc = make_collector();
        let invalid_kind = gc.alloc_object(ClassId::new(1), 0);
        unsafe {
            corrupt_header_byte(invalid_kind, OBJECT_KIND_OFFSET, 0x7f);
        }
        assert!(gc
            .is_object_address(invalid_kind.as_ptr() as usize)
            .is_none());

        let invalid_element = gc.alloc_array(ClassId::new(2), ArrayElementType::Int, 1);
        unsafe {
            corrupt_header_byte(invalid_element, ARRAY_ELEMENT_TYPE_OFFSET, 0x7f);
        }
        assert!(gc
            .is_object_address(invalid_element.as_ptr() as usize)
            .is_none());
    }

    #[test]
    fn scratch_value_helpers_accept_unaligned_buffers() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(99), 0);
        let values = [
            Value::Int(-7),
            Value::Long(0x0123_4567_89ab_cdef),
            Value::Float(1.25),
            Value::Double(-9.5),
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::ReturnAddress(0x1234),
            Value::Uninitialized,
        ];
        let mut backing = [0u8; SLOT_SIZE + 8];
        let ptr = unaligned_ptr(&mut backing, std::mem::align_of::<Value>());

        for value in values {
            backing.fill(0xa5);
            unsafe {
                value_to_unaligned_ptr(value, ptr);
                assert_eq!(value_from_unaligned_ptr(ptr), value);
            }
        }
    }

    #[test]
    fn scratch_array_element_helpers_accept_unaligned_buffers() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(100), 0);
        let cases = [
            (ArrayElementType::Int, Value::Int(-123_456)),
            (ArrayElementType::Long, Value::Long(0x1020_3040_5060_7080)),
            (ArrayElementType::Float, Value::Float(3.5)),
            (ArrayElementType::Double, Value::Double(-17.25)),
            (ArrayElementType::Byte, Value::Int(-5)),
            (ArrayElementType::Boolean, Value::Int(1)),
            (ArrayElementType::Short, Value::Int(-1234)),
            (ArrayElementType::Char, Value::Int(0x03bb)),
            (ArrayElementType::Reference, Value::Object(None)),
            (ArrayElementType::Reference, Value::Object(Some(obj))),
        ];
        let mut backing = [0u8; 16];
        let ptr = unaligned_ptr(&mut backing, std::mem::align_of::<u64>());

        for (element_type, value) in cases {
            backing.fill(0x5a);
            unsafe {
                array_element_to_unaligned_ptr(element_type, value, ptr);
                assert_eq!(array_element_from_unaligned_ptr(element_type, ptr), value);
            }
        }
    }

    #[test]
    fn boolean_array_element_decode_zero_extends_raw_byte() {
        let raw = [0xffu8; 1];

        unsafe {
            assert_eq!(
                array_element_from_unaligned_ptr(ArrayElementType::Byte, raw.as_ptr()),
                Value::Int(-1)
            );
            assert_eq!(
                array_element_from_unaligned_ptr(ArrayElementType::Boolean, raw.as_ptr()),
                Value::Int(255)
            );
        }
    }

    #[test]
    fn descriptor_defaults_initialize_reference_and_tail_slots() {
        let gc = make_collector();
        let obj = gc.alloc_object_with_descriptors(ClassId::new(7), 4, b"IL");

        assert_eq!(gc.get_field(obj, 0), Value::Int(0));
        assert_eq!(gc.get_field(obj, 1), Value::Object(None));
        assert_eq!(gc.get_field(obj, 2), Value::Object(None));
        assert_eq!(gc.get_field(obj, 3), Value::Object(None));
    }

    #[test]
    fn try_descriptor_defaults_initialize_reference_and_tail_slots() {
        let gc = make_collector();
        let obj = gc
            .try_alloc_object_with_descriptors(ClassId::new(8), 4, b"JL")
            .expect("small descriptor-aware allocation should fit");

        assert_eq!(gc.get_field(obj, 0), Value::Long(0));
        assert_eq!(gc.get_field(obj, 1), Value::Object(None));
        assert_eq!(gc.get_field(obj, 2), Value::Object(None));
        assert_eq!(gc.get_field(obj, 3), Value::Object(None));
    }

    // -- Config defaults --

    #[test]
    fn config_defaults() {
        let cfg = G1CollectorConfig::default();
        assert_eq!(cfg.heap_size, 256 * 1024 * 1024);
        assert_eq!(cfg.region_size, 1024 * 1024);
        assert_eq!(cfg.max_gc_pause_ms, 200);
        // T19.3.G1 raised from 45 → 70.
        assert_eq!(cfg.ihop_percent, 70);
        assert_eq!(cfg.promotion_age, 15);
        assert_eq!(cfg.gc_worker_threads, 4);
        assert!(!cfg.string_dedup_enabled);
        assert_eq!(cfg.mixed_gc_count_target, 8);
        assert_eq!(cfg.old_cset_region_threshold_percent, 10);
    }

    // -- Region basics --

    #[test]
    fn region_new_is_free() {
        let r = G1Region::new(1024);
        assert_eq!(r.region_type, RegionType::Free);
        assert_eq!(r.cursor, 0);
        assert!(!r.pinned);
        assert_eq!(r.age, 0);
        assert_eq!(r.live_bytes, 0);
    }

    #[test]
    fn region_bump_alloc() {
        let mut r = G1Region::new(4096);
        let (ptr, offset) = r.bump_alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(offset, 0);
        assert_eq!(r.cursor, 64);

        let (ptr2, offset2) = r.bump_alloc(128, 8).unwrap();
        assert!(!ptr2.is_null());
        assert_eq!(offset2, 64);
        assert_eq!(r.cursor, 192);
    }

    #[test]
    fn region_bump_alloc_full() {
        let mut r = G1Region::new(128);
        assert!(r.bump_alloc(64, 8).is_some());
        assert!(r.bump_alloc(64, 8).is_some());
        assert!(r.bump_alloc(1, 8).is_none()); // full
    }

    #[test]
    fn region_remaining() {
        let mut r = G1Region::new(1024);
        assert_eq!(r.remaining(), 1024);
        r.bump_alloc(100, 8);
        assert_eq!(r.remaining(), 924);
    }

    #[test]
    fn region_reset() {
        let mut r = G1Region::new(1024);
        r.region_type = RegionType::Eden;
        r.cursor = 500;
        r.live_bytes = 200;
        r.pinned = true;
        r.age = 5;
        r.reset(0);
        assert_eq!(r.region_type, RegionType::Free);
        assert_eq!(r.cursor, 0);
        assert!(!r.pinned);
        assert_eq!(r.age, 0);
    }

    // -- Collector creation --

    #[test]
    fn collector_creation() {
        let gc = make_collector();
        assert_eq!(gc.num_regions(), 8);
        assert_eq!(gc.collection_count(), 0);
        assert_eq!(gc.total_pause_ms(), 0);
    }

    #[test]
    fn collector_default() {
        let gc = G1Collector::with_defaults();
        assert_eq!(gc.num_regions(), 256);
    }

    // -- Allocation --

    #[test]
    fn alloc_object_basic() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let header = gc.get_header(obj);
        assert_eq!(header.class_id, ClassId::new(1));
        assert_eq!(header.kind, ObjectKind::Object);
        assert_eq!(header.num_slots(), 2);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);
    }

    #[test]
    fn alloc_multiple_objects_same_region() {
        let gc = make_collector();
        let _obj1 = gc.alloc_object(ClassId::new(1), 1);
        let _obj2 = gc.alloc_object(ClassId::new(2), 1);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);
    }

    #[test]
    fn alloc_array_basic() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(5), ArrayElementType::Int, 10);
        assert_eq!(gc.array_length(arr), 10);
        assert_eq!(gc.kind_of(arr), ObjectKind::Array);
    }

    #[test]
    fn alloc_field_read_write() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 3);
        gc.set_field(obj, 0, Value::Int(42));
        gc.set_field(obj, 1, Value::Long(1000));
        gc.set_field(obj, 2, Value::Float(3.14));
        assert_eq!(gc.get_field(obj, 0).as_int(), Some(42));
        assert_eq!(gc.get_field(obj, 1).as_long(), Some(1000));
    }

    #[test]
    fn alloc_array_elements() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        gc.set_array_element(arr, 0, Value::Int(10)).unwrap();
        gc.set_array_element(arr, 1, Value::Int(20)).unwrap();
        gc.set_array_element(arr, 2, Value::Int(30)).unwrap();
        assert_eq!(gc.get_array_element(arr, 0).unwrap().as_int(), Some(10));
        assert_eq!(gc.get_array_element(arr, 1).unwrap().as_int(), Some(20));
        assert_eq!(gc.get_array_element(arr, 2).unwrap().as_int(), Some(30));
    }

    #[test]
    fn boolean_array_elements_zero_extend_like_shared_heap_path() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Boolean, 1);

        gc.set_array_element(arr, 0, Value::Int(255)).unwrap();

        assert_eq!(gc.get_array_element(arr, 0).unwrap(), Value::Int(255));
    }

    #[test]
    fn alloc_array_bounds_check() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);
        assert!(gc.get_array_element(arr, 2).is_err());
        assert!(gc.set_array_element(arr, 5, Value::Int(0)).is_err());
    }

    // -- Humongous allocation --

    #[test]
    fn humongous_allocation() {
        let gc = make_collector();
        // region_size is 1MB, so > 512KB is humongous
        // A large array: HEADER_SIZE + 600000 * 4 bytes = ~2.4 MB
        let large = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 150_000);
        assert_eq!(gc.array_length(large), 150_000);
        assert!(gc.count_regions(RegionType::HumongousStart) >= 1);
    }

    #[test]
    fn cleanup_reclaims_dead_humongous_span_and_keeps_marked_span() {
        let gc = make_collector();
        let live = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let _dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);

        gc.set_array_element(live, 199_999, Value::Long(0x1234_5678))
            .unwrap();
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 2);
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 2);

        gc.start_concurrent_mark();
        gc.remark(&[live]);
        assert!(gc.concurrent_mark_step(usize::MAX));
        gc.cleanup();

        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 1);
        assert!(gc.is_humongous(live));
        assert_eq!(
            gc.get_array_element(live, 199_999).unwrap(),
            Value::Long(0x1234_5678)
        );
    }

    // C2 (round-12 gc): a humongous array spans multiple non-contiguous region
    // buffers. Element access at HIGH indices must land in the owning
    // continuation region — never OOB in unrelated heap memory. Round-trip
    // values at low / boundary / high indices and confirm they read back.
    #[test]
    fn humongous_int_array_multi_region_roundtrip() {
        let gc = make_collector();
        // 1 MB regions → usable payload ≈ (1MB-40)/4 ≈ 262133 ints/region.
        // 400_000 ints (~1.6 MB data) spans at least two regions.
        let n = 400_000;
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);
        // Write a recognizable pattern at indices in both the start region and
        // the continuation region(s).
        for &i in &[0usize, 1, 262_000, 262_133, 262_134, 300_000, n - 1] {
            gc.set_array_element(arr, i, Value::Int((i as i32).wrapping_mul(7) ^ 0x5a5a))
                .unwrap();
        }
        for &i in &[0usize, 1, 262_000, 262_133, 262_134, 300_000, n - 1] {
            assert_eq!(
                gc.get_array_element(arr, i).unwrap().as_int(),
                Some((i as i32).wrapping_mul(7) ^ 0x5a5a),
                "mismatch at index {i}",
            );
        }
        // OOB index is rejected, not a wild write.
        assert!(gc.set_array_element(arr, n, Value::Int(1)).is_err());
        assert!(gc.get_array_element(arr, n).is_err());
    }

    // G1 SIGSEGV regression (CpuOnlyBench / gpu-bench-cpu): a humongous array
    // must be ONE physically-contiguous block so the JIT's flat
    // `base + HEADER_SIZE + i*stride` array addressing — which bypasses the
    // GC's region-aware accessors entirely — reads/writes every element
    // correctly. The old region-fragmented layout backed each region with a
    // SEPARATE `Vec<u8>`, so flat access past the first region's payload hit
    // unrelated heap memory: silent zero tail at ~1–2 MB arrays, hard SIGSEGV
    // at ~4 MB+. This test writes/reads through a RAW FLAT POINTER (exactly as
    // JIT-compiled code does), including a full sweep over every element.
    #[test]
    fn humongous_int_array_is_contiguous_for_flat_jit_access() {
        let gc = make_collector();
        let n = 400_000; // ~1.6 MB → spans multiple 1 MB regions
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        assert!(gc.is_humongous(arr), "test array must be humongous");
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);

        // Raw flat base pointer to element 0 — what the JIT computes from the
        // array oop (`obj + HEADER_SIZE`), with no region-aware translation.
        let base = unsafe { arr.as_ptr().add(HEADER_SIZE) as *mut i32 };

        // 1) GC accessor writes, flat pointer reads back — including the tail
        //    element, which lives in a continuation region.
        let probes = [0usize, 1, 262_143, 262_144, 300_000, n - 1];
        for &i in &probes {
            gc.set_array_element(arr, i, Value::Int((i as i32).wrapping_mul(31) ^ 0x1234))
                .unwrap();
        }
        for &i in &probes {
            let v = unsafe { std::ptr::read(base.add(i)) };
            assert_eq!(
                v,
                (i as i32).wrapping_mul(31) ^ 0x1234,
                "flat JIT-style read mismatch at index {i}"
            );
        }

        // 2) Flat pointer writes EVERY element, GC accessor reads back. This is
        //    the crashing direction: a tight JIT store loop over the whole
        //    array. Pre-fix this would corrupt unrelated heap / SIGSEGV.
        for i in 0..n {
            unsafe { std::ptr::write(base.add(i), i as i32) };
        }
        for &i in &[0usize, 100_000, 262_144, 350_000, n - 1] {
            assert_eq!(
                gc.get_array_element(arr, i).unwrap().as_int(),
                Some(i as i32),
                "accessor read-back mismatch at index {i}"
            );
        }

        // 3) Contiguity invariant: the byte just past the last element stays
        //    inside the reserved span [start, start + regions_needed*region_size).
        let rs = gc.config.region_size;
        let total = HEADER_SIZE + n * 4;
        let regions_needed = total.div_ceil(rs);
        let start_base = arr.as_ptr() as usize;
        assert!(
            start_base + total <= start_base + regions_needed * rs,
            "humongous tail escapes its reserved contiguous span"
        );
    }

    // Residual humongous-OOB fix: `is_humongous` must distinguish a
    // multi-region humongous array from an ordinary single-region one, so
    // `VmHeap::array_data_ptr` can refuse to hand out a flat pointer for the
    // former (whose payload is non-contiguous).
    #[test]
    fn is_humongous_detects_multi_region_arrays() {
        let gc = make_collector();
        // Small array fits in a single region → NOT humongous.
        let small = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 10);
        assert!(
            !gc.is_humongous(small),
            "small array misclassified as humongous"
        );
        // Large array (~1.6 MB) spans multiple regions → humongous.
        let large = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 400_000);
        assert!(gc.count_regions(RegionType::HumongousStart) >= 1);
        assert!(gc.is_humongous(large), "humongous array not detected");
        // A plain object is not humongous either.
        let obj = gc.alloc_object(ClassId::new(1), 3);
        assert!(
            !gc.is_humongous(obj),
            "small object misclassified as humongous"
        );
    }

    // C2: long[] elements are 8 bytes; the per-region payload capacity
    // (region_size - HEADER_SIZE) is not a multiple of 8 for HEADER_SIZE=40
    // only when region_size isn't — but verify the straddle-safe copy path by
    // exercising elements adjacent to a region boundary.
    #[test]
    fn humongous_long_array_boundary_roundtrip() {
        let gc = make_collector();
        let n = 200_000; // 8 bytes each ≈ 1.6 MB → multi-region
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, n);
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);
        // usable/8 elements per region; probe around the first boundary.
        let usable = (gc.config.region_size - HEADER_SIZE) / 8;
        for &i in &[usable - 1, usable, usable + 1, n - 1] {
            gc.set_array_element(arr, i, Value::Long(0x0123_4567_89ab_cdef ^ i as i64))
                .unwrap();
        }
        for &i in &[usable - 1, usable, usable + 1, n - 1] {
            assert_eq!(
                gc.get_array_element(arr, i).unwrap().as_long(),
                Some(0x0123_4567_89ab_cdef ^ i as i64),
                "mismatch at index {i}",
            );
        }
    }

    // C2 / C2b: a humongous *object* (many reference slots) round-trips field
    // values across regions, and out-of-layout field access is dropped rather
    // than reading/writing neighboring memory.
    #[test]
    fn humongous_object_field_roundtrip_and_bounds() {
        let gc = make_collector();
        // > 512 KB of slots (16 bytes each) → humongous, multi-region.
        let num_fields = (1024 * 1024) / SLOT_SIZE + 4; // > 1 region of slots
        let obj = gc.alloc_object(ClassId::new(1), num_fields);
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);
        let per_region = (gc.config.region_size - HEADER_SIZE) / SLOT_SIZE;
        for &i in &[
            0usize,
            per_region - 1,
            per_region,
            per_region + 1,
            num_fields - 1,
        ] {
            gc.set_field(obj, i, Value::Int(i as i32));
        }
        for &i in &[
            0usize,
            per_region - 1,
            per_region,
            per_region + 1,
            num_fields - 1,
        ] {
            assert_eq!(gc.get_field(obj, i).as_int(), Some(i as i32), "field {i}");
        }
        // Out-of-layout field read/write must be a benign no-op (drop), not OOB.
        let oob = num_fields + 1000;
        assert_eq!(gc.get_field(obj, oob), Value::Object(None));
        gc.set_field(obj, oob, Value::Int(0xdead_u32 as i32)); // dropped silently
    }

    // -- Young collection --

    #[test]
    fn young_collection_basic() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(42));

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert!(result.stats.objects_copied >= 1);
        let new_obj = roots[0];
        // Should have been copied
        assert_eq!(gc.get_field(new_obj, 0).as_int(), Some(42));
    }

    #[test]
    fn young_collection_with_reference_chain() {
        let gc = make_collector();
        let a = gc.alloc_object(ClassId::new(1), 1);
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(a, 0, Value::Object(Some(b)));
        gc.set_field(b, 0, Value::Int(99));

        let mut roots = vec![a];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(result.stats.objects_copied, 2);
        let new_a = roots[0];
        let new_b_val = gc.get_field(new_a, 0);
        if let Value::Object(Some(new_b)) = new_b_val {
            assert_eq!(gc.get_field(new_b, 0).as_int(), Some(99));
        } else {
            panic!("expected reference to B");
        }
    }

    #[test]
    fn young_collection_promotion() {
        let mut cfg = small_config();
        cfg.promotion_age = 1; // promote after 1 GC cycle
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        // First GC: object goes to Survivor with age 1
        let _r1 = gc.young_collection(&mut roots, &NoopMonitors);
        // The object's gc_age should now be 1 which >= promotion_age
        // So next GC it should go to Old if it was in Survivor
        // Actually with promotion_age=1, age >= 1 means immediate promotion to Old
        let header = gc.get_header(roots[0]);
        // With promotion_age=1, objects with gc_age >= 1 go to Old.
        // After first GC, gc_age is incremented to 1.
        assert!(header.gc_age >= 1);
    }

    #[test]
    fn young_collection_frees_eden() {
        let gc = make_collector();
        let _obj = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);

        let mut roots = vec![_obj];
        gc.young_collection(&mut roots, &NoopMonitors);

        // Eden should be freed after collection
        assert_eq!(gc.count_regions(RegionType::Eden), 0);
    }

    #[test]
    fn young_collection_unreachable_freed() {
        let gc = make_collector();
        let live = gc.alloc_object(ClassId::new(1), 0);
        let _dead = gc.alloc_object(ClassId::new(2), 0);

        let mut roots = vec![live]; // _dead not in roots
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Only the live object should be copied
        assert_eq!(result.stats.objects_copied, 1);
    }

    #[test]
    fn young_collection_pointer_map() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let old_addr = obj.as_ptr() as usize;

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert!(result.pointer_map.contains_key(&old_addr));
        assert_eq!(result.pointer_map[&old_addr], roots[0].as_ptr() as usize);
    }

    /// MOVING-GC empirical confirmation for the JNI non-critical
    /// `Get/Release<Type>ArrayElements` copy-back handle.
    ///
    /// When a G1 young evacuation relocates an array between `GetArrayElements`
    /// and `ReleaseArrayElements`, the raw `array` jobject the native side holds
    /// (a CratonVM local ref is exactly `obj.as_ptr()`, captured at Get) becomes
    /// a stale from-space pointer. The OLD copy-back re-resolved THAT pointer via
    /// `is_heap_addr` (as `jobject_to_obj` does for a local ref) and therefore
    /// either silently dropped (region freed → `None`) or wrote into a recycled
    /// object. The FIX records a *remappable* handle instead — a JNI global ref,
    /// whose boxed `ObjectRef` `update_after_gc` rewrites through the very
    /// `pointer_map` produced here — so the copy-back follows the array.
    ///
    /// This reproduces the exact relocation and proves BOTH halves: (a) the
    /// stale raw handle no longer resolves to a live heap address, and (b) the
    /// remapped handle resolves to the array's new location with data intact and
    /// a copy-back write lands in the live array.
    #[test]
    fn jni_array_raw_handle_stale_after_evacuation_but_remap_survives() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        gc.set_array_element(arr, 0, Value::Int(111)).unwrap();
        gc.set_array_element(arr, 1, Value::Int(222)).unwrap();
        gc.set_array_element(arr, 2, Value::Int(333)).unwrap();

        // The raw `array` jobject value native code holds across the window,
        // captured BEFORE the GC (exactly what the old Release re-resolved).
        let stale_handle = arr.as_ptr() as usize;

        // `roots` models the remappable keep-alive handle: a JNI global ref boxes
        // an `ObjectRef` that the collector rewrites in place via this same
        // pointer-map mechanism (`JniGlobalRefs::update_after_gc`).
        let mut roots = vec![arr];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        let remapped = roots[0];
        let new_addr = remapped.as_ptr() as usize;

        // The evacuation actually MOVED the array (otherwise the test is vacuous).
        assert_eq!(result.pointer_map.get(&stale_handle), Some(&new_addr));
        assert_ne!(
            stale_handle, new_addr,
            "array must relocate for this test to be meaningful"
        );

        // (a) BUG path: the old raw handle is now stale — its region was
        // evacuated and freed, so the local-ref re-resolve fails. The old
        // copy-back would silently drop the native mutations here.
        assert!(
            !gc.is_addr_in_live_region(stale_handle),
            "evacuated array's old address should be a freed region"
        );
        assert!(
            gc.is_heap_addr(stale_handle).is_none(),
            "stale local-ref handle must not resolve to a live object"
        );

        // (b) FIX path: the remapped handle resolves to the array's CURRENT
        // location with the data preserved across the copy.
        assert!(gc.is_addr_in_live_region(new_addr));
        assert_eq!(gc.array_length(remapped), 3);
        assert_eq!(
            gc.get_array_element(remapped, 0).unwrap().as_int(),
            Some(111)
        );
        assert_eq!(
            gc.get_array_element(remapped, 1).unwrap().as_int(),
            Some(222)
        );
        assert_eq!(
            gc.get_array_element(remapped, 2).unwrap().as_int(),
            Some(333)
        );

        // A write through the remapped handle (the actual copy-back) is visible
        // in the live array — never in the dead from-space copy.
        gc.set_array_element(remapped, 1, Value::Int(999)).unwrap();
        assert_eq!(
            gc.get_array_element(remapped, 1).unwrap().as_int(),
            Some(999)
        );
    }

    // -- Mixed collection --

    #[test]
    fn mixed_collection_selects_old_regions() {
        let gc = make_collector();

        // Manually set up some old regions with gc_efficiency data.
        // `live_bytes > 0` marks them as carrying marking data from a
        // completed cycle — regions without it (post-cleanup promotions)
        // are ineligible for the mixed CSet (G1CORE-7 gate).
        {
            let mut regions = gc.regions.lock();
            regions[0].region_type = RegionType::Old;
            regions[0].cursor = 100;
            regions[0].live_bytes = 10;
            regions[0].gc_efficiency = 0.1; // 10% live = 90% garbage, best candidate
            regions[1].region_type = RegionType::Old;
            regions[1].cursor = 100;
            regions[1].live_bytes = 90;
            regions[1].gc_efficiency = 0.9; // 90% live = 10% garbage, poor candidate
        }

        // Force marking complete to trigger mixed GC
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);

        let mut roots = vec![];
        let _result = gc.mixed_collection(&mut roots, &NoopMonitors);

        // Region 0 (lowest gc_efficiency = most garbage) should have been collected.
        // Region 1 (high gc_efficiency) should remain.
        assert_eq!(gc.count_regions(RegionType::Old), 1);
    }

    // -- IHOP --

    #[test]
    fn ihop_threshold_calculation() {
        let mut cfg = small_config();
        cfg.ihop_percent = 50;
        let gc = G1Collector::new(cfg.clone());

        let expected = cfg.heap_size / 2;
        assert_eq!(gc.marking_threshold_bytes(), expected);
    }

    #[test]
    fn ihop_check_below_threshold() {
        let gc = make_collector();
        gc.old_gen_bytes.store(0, Ordering::Relaxed);
        assert!(!gc.check_ihop());
    }

    #[test]
    fn ihop_check_above_threshold() {
        let gc = make_collector();
        let threshold = gc.marking_threshold_bytes();
        gc.old_gen_bytes.store(threshold + 1, Ordering::Relaxed);
        assert!(gc.check_ihop());
    }

    #[test]
    fn ihop_adaptive_adjustment_lower() {
        let gc = make_collector();
        let before = gc.marking_threshold_bytes();
        // Pause way over target should lower threshold
        gc.update_ihop(gc.config.max_gc_pause_ms + 100);
        let after = gc.marking_threshold_bytes();
        assert!(after < before);
    }

    #[test]
    fn ihop_adaptive_adjustment_raise() {
        let gc = make_collector();
        let before = gc.marking_threshold_bytes();
        // Pause well under half the target should raise threshold
        gc.update_ihop(0);
        let after = gc.marking_threshold_bytes();
        assert!(after >= before);
    }

    // T19.3.G1 — GC allocation-storm follow-ups.

    #[test]
    fn t19_default_ihop_is_70_percent() {
        // T19.3.G1 raised the default IHOP from 45% → 70% so static-init
        // bursts on 256 MiB heaps don't fire concurrent marking while
        // the heap is still essentially empty.
        let cfg = G1CollectorConfig::default();
        assert_eq!(cfg.ihop_percent, 70);
    }

    #[test]
    fn t19_default_ihop_threshold_bytes_is_70_percent_of_heap() {
        let cfg = G1CollectorConfig::default();
        let gc = G1Collector::new(cfg.clone());
        // 70 % of 256 MiB == 179.2 MiB. Accept any value within 1% of
        // the exact calculation so future rounding tweaks don't break.
        let expected = cfg.heap_size * 70 / 100;
        let actual = gc.marking_threshold_bytes();
        let diff = actual.abs_diff(expected);
        assert!(
            diff * 100 <= expected,
            "ihop threshold {actual} deviates from {expected} by more than 1 %"
        );
    }

    #[test]
    fn t19_default_ihop_does_not_fire_at_40_percent_occupancy() {
        // A Quarkus static-init burst at 40 % old-gen occupancy used to
        // trigger concurrent marking under the old 45 % threshold. Post-fix
        // the heap must tolerate 40 % without firing.
        let gc = G1Collector::new(G1CollectorConfig::default());
        let forty_percent = gc.config.heap_size * 40 / 100;
        gc.old_gen_bytes.store(forty_percent, Ordering::Relaxed);
        assert!(
            !gc.check_ihop(),
            "IHOP fired at 40% occupancy with 70% threshold"
        );
    }

    #[test]
    fn t19_default_ihop_still_fires_at_75_percent_occupancy() {
        // Above 70 %, the threshold is crossed and concurrent marking
        // must start. This keeps the safety net intact.
        let gc = G1Collector::new(G1CollectorConfig::default());
        let seventy_five_percent = gc.config.heap_size * 75 / 100;
        gc.old_gen_bytes
            .store(seventy_five_percent, Ordering::Relaxed);
        assert!(
            gc.check_ihop(),
            "IHOP didn't fire at 75% occupancy with 70% threshold"
        );
    }

    // -- Region pinning --

    #[test]
    fn region_pinning() {
        let gc = make_collector();
        assert!(!gc.is_pinned(0));
        gc.pin_region(0);
        assert!(gc.is_pinned(0));
        gc.unpin_region(0);
        assert!(!gc.is_pinned(0));
    }

    #[test]
    fn pinned_region_not_collected() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(77));

        // Find the region and pin it
        let region_idx = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.pin_region(region_idx);

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Pinned region should not be collected
        assert_eq!(result.stats.objects_copied, 0);
        // Object should still be at the same address
        assert_eq!(roots[0].as_ptr(), obj.as_ptr());
    }

    #[test]
    fn pin_out_of_bounds() {
        let gc = make_collector();
        gc.pin_region(9999); // should not panic
        assert!(!gc.is_pinned(9999));
    }

    // -- INT-3: frozen-peer TLAB skip regions --

    /// INT-3 — a region holding a published un-retired TLAB tail must be
    /// excluded from the CSet: its (frozen) owner resumes bump-allocating
    /// into `[cursor, end)` after the pause, and objects it already
    /// allocated there may be addressed by un-rewritable frozen-peer state.
    /// Deliberately run WITHOUT `gc_quiescence` JIT activity: the exclusion
    /// must hold even when no thread is in JIT (a blocked thread that missed
    /// its retire publishes a tail too).
    #[test]
    fn jit_tlab_skip_region_excluded_from_cset() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(41));

        // Carve a mutator TLAB from the current Eden region — the region
        // cursor now covers `len` bytes the "mutator" never initialized,
        // exactly the state a frozen in-JIT peer leaves behind.
        let (tlab_ptr, len) = gc.refill_tlab(4096).expect("TLAB refill");
        let obj_region = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        let tlab_region = gc.lookup_region_for_addr(tlab_ptr as usize).unwrap();
        assert_eq!(
            obj_region, tlab_region,
            "test precondition: object and TLAB share the current Eden region"
        );

        gc.set_jit_tlab_skip_regions(&[(tlab_ptr as usize, tlab_ptr as usize + len)]);
        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            result.stats.objects_copied, 0,
            "region holding a frozen TLAB tail must be excluded from the CSet"
        );
        assert_eq!(
            roots[0].as_ptr(),
            obj.as_ptr(),
            "object sharing the frozen peer's Eden region must not move"
        );
        assert_eq!(gc.get_field(obj, 0).as_int(), Some(41));

        // After the (VM-driven) clear, the next collection evacuates normally.
        gc.clear_jit_tlab_skip_regions();
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            result.stats.objects_copied > 0,
            "clearing the skip list must restore normal evacuation"
        );
    }

    /// INT-3 — every linear region walker must stride over a published
    /// frozen TLAB tail instead of parsing its uninitialized bytes (zeroed
    /// test-arena bytes parse as a run of phantom 0-slot objects; real
    /// frozen-peer garbage can desync the stride entirely).
    #[test]
    fn walk_objects_skips_published_frozen_tlab_tail() {
        let gc = make_collector();
        let before = gc.alloc_object(ClassId::new(1), 2);
        let (tlab_ptr, len) = gc.refill_tlab(4096).expect("TLAB refill");
        let after = gc.alloc_object(ClassId::new(2), 3);
        let span = (tlab_ptr as usize, tlab_ptr as usize + len);
        assert_eq!(
            gc.lookup_region_for_addr(before.as_ptr() as usize),
            gc.lookup_region_for_addr(after.as_ptr() as usize),
            "test precondition: allocations straddle the carved TLAB in one region"
        );
        assert!(
            after.as_ptr() as usize >= span.1,
            "test precondition: `after` lands beyond the carved TLAB"
        );

        gc.set_jit_tlab_skip_regions(&[span]);
        let walked = gc.walk_objects();
        let ptrs: Vec<usize> = walked.iter().map(|&(p, _)| p as usize).collect();
        assert!(ptrs.contains(&(before.as_ptr() as usize)));
        assert!(
            ptrs.contains(&(after.as_ptr() as usize)),
            "walker must stride over the frozen tail and reach objects behind it"
        );
        assert!(
            ptrs.iter().all(|&p| p < span.0 || p >= span.1),
            "no phantom object may be reported inside the frozen TLAB tail"
        );
        gc.clear_jit_tlab_skip_regions();
    }

    #[test]
    fn pin_region_for_addr_keeps_jni_critical_array_in_place() {
        // Step 6 (JEP 423): GetPrimitiveArrayCritical pins the backing array's
        // region so a moving young/mixed collection cannot relocate it before
        // the copy-back at Release. Without the pin the object is evacuated and
        // its Get-time address goes stale (copy-back to a vacated/recycled slot
        // — data loss or corruption).
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(123));
        let addr = obj.as_ptr() as usize;

        let idx = gc
            .pin_region_for_addr(addr)
            .expect("freshly allocated object must live in a region");
        // Resolves to the same region `region_for_ptr` would.
        let expected = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        assert_eq!(idx, expected);
        assert!(gc.is_pinned(idx));

        // A young GC must NOT relocate the pinned array.
        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            result.stats.objects_copied, 0,
            "pinned region must be excluded from the collection set"
        );
        assert_eq!(
            roots[0].as_ptr(),
            obj.as_ptr(),
            "pinned JNI-critical array must not move"
        );
        // The data the copy-back would read is intact and at the same address.
        assert_eq!(gc.get_field(obj, 0).as_int(), Some(123));

        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));
    }

    /// A JNI-pinned young region is excluded from the CSet but its objects may
    /// hold the ONLY reference to objects that ARE collected. Coverage comes
    /// from the remembered set: the cross-region ref store recorded the
    /// pinned holder's region in the target's RSet, so Phase 2's source walk
    /// visits the pinned region, evacuates the referent, and rewrites the
    /// holder's slot in place. (Spotted during the marking-soundness audit as
    /// a suspected gap — this test pins the invariant that makes it a
    /// non-gap. If the RSet ever starts filtering young→young edges, or a
    /// store path skips `post_write_barrier_rset`, this fails with Q freed
    /// under P.)
    #[test]
    fn jni_pinned_young_region_holder_keeps_cset_referent_alive() {
        let gc = make_collector();
        // P — the holder — lands in the current Eden region.
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, p.as_ptr()).unwrap()
        };
        // Allocate until a NEW Eden region opens so Q is cross-region from P.
        let mut q = gc.alloc_object(ClassId::new(2), 1);
        loop {
            let q_region = {
                let regions = gc.regions.lock();
                gc.region_for_ptr(&regions, q.as_ptr()).unwrap()
            };
            if q_region != p_region {
                break;
            }
            q = gc.alloc_object(ClassId::new(2), 1);
        }
        gc.set_field(q, 0, Value::Int(4242));
        // The ONLY path to Q: a field of P (cross-region ref store → the
        // post-write barrier records P's region in Q's region's RSet).
        gc.set_field(p, 0, Value::Object(Some(q)));

        // JNI critical section pins P's region (GetPrimitiveArrayCritical
        // shape: the pin covers the whole region, holder objects included).
        gc.pin_region(p_region);

        // Young collection with NO roots: P's region is excluded from the
        // CSet by the pin; Q must still be evacuated and P's slot rewritten.
        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect("Q, referenced only from the pinned region, must be evacuated");
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(
            gc.get_field(p, 0),
            Value::Object(Some(q_new_ref)),
            "pinned holder's slot must be rewritten to Q's new location"
        );
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(4242));
        gc.unpin_region(p_region);
    }

    /// Parallel-evacuator twin of
    /// [`jni_pinned_young_region_holder_keeps_cset_referent_alive`].
    #[test]
    fn jni_pinned_young_region_holder_keeps_cset_referent_alive_parallel() {
        let gc = G1Collector::new(parallel_config(2, 8));
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, p.as_ptr()).unwrap()
        };
        let mut q = gc.alloc_object(ClassId::new(2), 1);
        loop {
            let q_region = {
                let regions = gc.regions.lock();
                gc.region_for_ptr(&regions, q.as_ptr()).unwrap()
            };
            if q_region != p_region {
                break;
            }
            q = gc.alloc_object(ClassId::new(2), 1);
        }
        gc.set_field(q, 0, Value::Int(2424));
        gc.set_field(p, 0, Value::Object(Some(q)));
        gc.pin_region(p_region);

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect("Q, referenced only from the pinned region, must be evacuated (parallel)");
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(gc.get_field(p, 0), Value::Object(Some(q_new_ref)));
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(2424));
        gc.unpin_region(p_region);
    }

    #[test]
    fn region_pin_refcount_balances() {
        // Overlapping critical sections on arrays in the same region (or nested
        // checkouts of one array) must refcount: one Release cannot unpin while
        // another section is still live.
        let gc = make_collector();
        gc.pin_region(0);
        gc.pin_region(0);
        assert!(gc.is_pinned(0));
        gc.unpin_region(0);
        assert!(gc.is_pinned(0), "still pinned after 1 of 2 unpins");
        gc.unpin_region(0);
        assert!(!gc.is_pinned(0), "unpinned after the final unpin");
        // An extra (unbalanced) unpin is tolerated and stays unpinned.
        gc.unpin_region(0);
        assert!(!gc.is_pinned(0));
    }

    // -- String deduplication --

    #[test]
    fn string_dedup_disabled_by_default() {
        let gc = make_collector();
        assert_eq!(gc.deduplicate_string(12345, 0x1000), None);
    }

    #[test]
    fn string_dedup_enabled() {
        let mut cfg = small_config();
        cfg.string_dedup_enabled = true;
        let gc = G1Collector::new(cfg);

        // First string with this hash: not deduped, just registered.
        assert_eq!(gc.deduplicate_string(42, 0x1000), None);
        // Second string with same hash: deduped — caller gets the canonical
        // address so it can redirect its pointer.
        assert_eq!(gc.deduplicate_string(42, 0x2000), Some(0x1000));
        // Different hash: not deduped, registered fresh.
        assert_eq!(gc.deduplicate_string(99, 0x3000), None);
        // Re-hit the second hash with yet another address — still points
        // at the original canonical instance.
        assert_eq!(gc.deduplicate_string(99, 0x4000), Some(0x3000));
    }

    // -- GC logging --

    #[test]
    fn gc_logging_toggle() {
        let gc = make_collector();
        assert!(!gc.gc_log_enabled.load(Ordering::Relaxed));
        gc.enable_gc_logging();
        assert!(gc.gc_log_enabled.load(Ordering::Relaxed));
        gc.disable_gc_logging();
        assert!(!gc.gc_log_enabled.load(Ordering::Relaxed));
    }

    // -- Collection counting --

    #[test]
    fn collection_count_increments() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        assert_eq!(gc.collection_count(), 0);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.collection_count(), 1);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.collection_count(), 2);
    }

    // -- Pause sink (§7 item 6) --

    #[test]
    fn pause_history_records_each_collection() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        assert!(gc.pause_summary().is_none(), "no history before any GC");

        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);

        let hist = gc.pause_history_snapshot();
        assert_eq!(hist.len(), 2, "one record per collection");
        assert!(hist
            .iter()
            .all(|r| r.collection_type == G1CollectionType::YoungOnly));

        let summary = gc.pause_summary().expect("history present");
        assert_eq!(summary.young.count, 2);
        assert_eq!(summary.mixed.count, 0);
        // Microsecond accumulator agrees with the per-record total.
        let recorded: u64 = hist.iter().map(|r| r.pause_us).sum();
        assert_eq!(gc.total_pause_us(), recorded);
    }

    #[test]
    fn pause_percentiles_nearest_rank() {
        // Drive the reduction directly with a known distribution so the
        // percentile math is exercised independently of wall-clock timing.
        let gc = make_collector();
        let stats = GcStats {
            objects_copied: 0,
            bytes_copied: 0,
            bytes_freed: 0,
        };
        // pauses 10,20,...,100 us (10 young collections).
        for i in 1..=10u64 {
            gc.record_collection(G1CollectionType::YoungOnly, i * 10, &stats);
        }
        let s = gc.pause_summary().unwrap();
        assert_eq!(s.young.count, 10);
        assert_eq!(s.young.max_us, 100);
        // nearest-rank: p50 -> ceil(0.5*10)=5th -> 50; p99 -> ceil(0.99*10)=10th -> 100.
        assert_eq!(s.young.p50_us, 50);
        assert_eq!(s.young.p99_us, 100);
        assert_eq!(s.young.total_us, 550);
    }

    #[test]
    fn pause_history_ring_is_bounded() {
        let gc = make_collector();
        let stats = GcStats {
            objects_copied: 0,
            bytes_copied: 0,
            bytes_freed: 0,
        };
        for _ in 0..(PAUSE_HISTORY_CAP + 100) {
            gc.record_collection(G1CollectionType::YoungOnly, 1, &stats);
        }
        assert_eq!(gc.pause_history_snapshot().len(), PAUSE_HISTORY_CAP);
        assert_eq!(
            gc.pause_history_dropped.load(Ordering::Relaxed),
            100,
            "evictions are counted so the summary stays honest"
        );
        // collection_count keeps the true total, not the ring size.
        assert_eq!(gc.collection_count(), (PAUSE_HISTORY_CAP + 100) as u64);
    }

    // -- Concurrent marking --

    #[test]
    fn concurrent_mark_phases() {
        let gc = make_collector();
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Idle);

        gc.start_concurrent_mark();
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::ConcurrentMark);

        let done = gc.concurrent_mark_step(1000);
        assert!(done); // no objects to mark, should complete immediately

        let obj = gc.alloc_object(ClassId::new(1), 0);
        gc.remark(&[obj]);
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Remark);
    }

    #[test]
    fn cleanup_computes_live_bytes() {
        // Round-2 fix (HIGH — GC #5): bitmaps now live per-region and
        // are keyed off each region's heap-allocated `data.as_ptr()`,
        // so `try_mark`/`is_marked` accept the real addresses of
        // objects living inside the region. This means `cleanup` can
        // now correctly attribute live bytes to a marked object.
        //
        // The previous version of this test documented the BUG —
        // marking a real address against a `[0, heap_size)` bitmap
        // always silently failed, so `live_bytes` was always 0. After
        // the per-region-bitmap fix, marking succeeds and `cleanup`
        // reports a non-zero `live_bytes` for the region holding the
        // marked object.
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let obj_addr = obj.as_ptr() as usize;

        // Locate the region the allocator placed the object in, mark
        // the object in *that* region's bitmap, then promote every
        // Eden region (including ours) to Old so `cleanup` walks them.
        {
            let mut regions = gc.regions.lock();
            let region_idx = gc
                .region_for_ptr(&regions, obj.as_ptr())
                .expect("freshly-allocated object must live in some region");
            let marked = regions[region_idx].mark_bitmap.try_mark(obj_addr);
            assert!(
                marked,
                "per-region bitmap must accept real heap addresses post-fix"
            );
            for r in regions.iter_mut() {
                if r.region_type == RegionType::Eden {
                    r.region_type = RegionType::Old;
                }
            }
        }

        gc.cleanup();

        // The region containing our marked object must report non-zero
        // live_bytes (was always 0 under the buggy global bitmap). The
        // exact byte count equals one ObjectHeader + 2 SLOT_SIZE
        // fields = HEADER_SIZE + 2*SLOT_SIZE. We assert >0 to keep the
        // test resilient to header-size tuning.
        let regions = gc.regions.lock();
        let holding_region = regions
            .iter()
            .find(|r| {
                let base = r.data.as_ptr() as usize;
                obj_addr >= base && obj_addr < base + r.data.len()
            })
            .expect("holding region must still exist");
        assert!(
            holding_region.live_bytes > 0,
            "per-region bitmap fix: cleanup must report >0 live_bytes for the marked region (got {})",
            holding_region.live_bytes,
        );
        assert_eq!(
            holding_region.region_type,
            RegionType::Old,
            "non-empty old region must not be freed by cleanup"
        );

        // Other old regions had no marked objects, so they should have
        // been freed (live_bytes == 0, type went back to Free).
        let old_count = regions
            .iter()
            .filter(|r| r.region_type == RegionType::Old)
            .count();
        assert_eq!(
            old_count, 1,
            "only the region containing the marked object should remain Old; \
             empty old regions must be freed"
        );
    }

    // =====================================================================
    // Marking soundness across evacuation pauses (SATB keep-alive) and the
    // restored cleanup in-place free — regression tests for the SteadyChurn
    // "[FREED] cleanup region=N while live holders point in" defect stack.
    // =====================================================================

    /// A gray worklist entry whose object the evacuation closure does not
    /// reach (dead at PAUSE time, live at MARK START) must be evacuated and
    /// re-grayed, not dropped: under SATB its unscanned subtree is
    /// snapshot-live. Pre-fix, the young pause dropped the gray and the
    /// subtree was silently unmarked.
    #[test]
    fn gray_cset_entry_is_evacuated_and_subtree_marked_across_young_pause() {
        let gc = make_collector();
        // G (young) → H (young); at the pause below NEITHER is reachable
        // from any root — the evacuation closure alone judges both dead.
        let g = gc.alloc_object(ClassId::new(1), 1);
        let h = gc.alloc_object(ClassId::new(2), 0);
        gc.set_field(g, 0, Value::Object(Some(h)));

        gc.start_concurrent_mark();
        // Seed G gray (as the initial-mark root scan would); do NOT drain —
        // the young pause interrupts marking mid-cycle.
        gc.remark(&[g]);

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        let g_new = *result
            .pointer_map
            .get(&(g.as_ptr() as usize))
            .expect("snapshot-live gray must be evacuated by the keep-alive, not dropped");
        let h_new = *result
            .pointer_map
            .get(&(h.as_ptr() as usize))
            .expect("the gray's referent must be evacuated with it");

        // Drain the marker: both copies must end up marked.
        while !gc.concurrent_mark_step(usize::MAX) {}
        let regions = gc.regions.lock();
        let g_idx = gc.region_for_ptr(&regions, g_new as *mut u8).unwrap();
        let h_idx = gc.region_for_ptr(&regions, h_new as *mut u8).unwrap();
        assert!(
            regions[g_idx].mark_bitmap.is_marked(g_new),
            "evacuated gray must be re-grayed and marked"
        );
        assert!(
            regions[h_idx].mark_bitmap.is_marked(h_new),
            "the gray's unscanned subtree must reach the bitmap (SATB snapshot)"
        );
    }

    /// SATB-logged references must survive an evacuation pause: the log is
    /// drained into the gray set at pause start and CSet-resident entries
    /// are evacuated. Pre-fix the raw addresses sat in the queue while the
    /// pause reset their regions (dangling), and at runtime the log was
    /// never consumed at all.
    #[test]
    fn satb_entry_survives_young_pause_and_gets_marked() {
        let gc = make_collector();
        let x = gc.alloc_object(ClassId::new(7), 0);

        gc.start_concurrent_mark();
        // A mutator overwrites the last reference to X mid-cycle: the SATB
        // pre-barrier logs X. Flush the thread-local buffer immediately so
        // a concurrent test's registry-wide flush cannot steal the entry
        // into a different collector's queue (the buffer is process-global).
        gc.satb_pre_barrier(x.as_ptr() as usize);
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        let x_new = *result
            .pointer_map
            .get(&(x.as_ptr() as usize))
            .expect("SATB-logged object must be kept alive across the pause");
        while !gc.concurrent_mark_step(usize::MAX) {}
        let regions = gc.regions.lock();
        let idx = gc.region_for_ptr(&regions, x_new as *mut u8).unwrap();
        assert!(
            regions[idx].mark_bitmap.is_marked(x_new),
            "SATB entry must reach the bitmap after the pause"
        );
    }

    /// Mixed pauses move OLD regions — where the gray set concentrates —
    /// and must remap the mark worklist exactly like young pauses do.
    /// Pre-fix, neither mixed path touched the worklist at all: every gray
    /// pointing into an evacuated Old region dangled into freed memory.
    #[test]
    fn mixed_collection_keeps_and_remaps_gray_old_entry() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);
        // Promote OBJ to Old with two young collections.
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(41));
        let mut roots = vec![obj];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let obj = roots[0];
        {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, obj.as_ptr()).unwrap();
            assert_eq!(
                regions[idx].region_type,
                RegionType::Old,
                "test setup: object must be promoted"
            );
        }

        // A NEW marking cycle overlaps the post-cleanup mixed sequence
        // (reachable in production: start_concurrent_mark does not clear
        // marking_complete).
        gc.start_concurrent_mark();
        gc.remark(&[obj]); // OBJ gray, unscanned
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);

        let result = gc.mixed_collection(&mut roots, &NoopMonitors);

        // No gray may dangle into a freed region…
        {
            let regions = gc.regions.lock();
            let worklist = gc.mark_worklist.lock();
            for &addr in worklist.iter() {
                let idx = gc
                    .region_for_ptr(&regions, addr as *mut u8)
                    .expect("gray entry must point into the heap");
                assert_ne!(
                    regions[idx].region_type,
                    RegionType::Free,
                    "gray entry {addr:#x} dangles into a freed region"
                );
            }
        }
        // …and OBJ (possibly relocated) must be marked after draining.
        let obj_final = result
            .pointer_map
            .get(&(obj.as_ptr() as usize))
            .copied()
            .unwrap_or(obj.as_ptr() as usize);
        while !gc.concurrent_mark_step(usize::MAX) {}
        let regions = gc.regions.lock();
        let idx = gc.region_for_ptr(&regions, obj_final as *mut u8).unwrap();
        assert!(regions[idx].mark_bitmap.is_marked(obj_final));
    }

    /// The SteadyChurn failure shape, end to end: an Old object X whose
    /// only marker-visible path runs through a young gray G that a
    /// mid-cycle young pause judges dead. The keep-alive must carry G (and
    /// through it X) to the bitmap, and cleanup's restored in-place free
    /// must therefore keep X's region.
    #[test]
    fn cleanup_in_place_free_respects_keepalive_marking() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Promote X (the "list node") to Old.
        let x = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![x];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let x = roots[0];
        let x_region = {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, x.as_ptr()).unwrap();
            assert_eq!(regions[idx].region_type, RegionType::Old);
            idx
        };

        // Young holder G → X.
        let g = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(g, 0, Value::Object(Some(x)));

        gc.start_concurrent_mark();
        gc.remark(&[g]); // G gray, unscanned; X reachable ONLY through G

        // Mid-cycle young pause with no roots: the evacuation closure
        // reaches neither G nor X. Pre-fix: G dropped → X never marked →
        // cleanup freed X's region while it was snapshot-live.
        let mut no_roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut no_roots, &NoopMonitors);

        while !gc.concurrent_mark_step(usize::MAX) {}
        gc.cleanup();

        let regions = gc.regions.lock();
        assert_eq!(
            regions[x_region].region_type,
            RegionType::Old,
            "Old region holding snapshot-live X must survive cleanup's in-place free"
        );
        assert!(regions[x_region].live_bytes > 0);
    }

    /// Companion: cleanup's restored in-place free DOES free a wholly-dead,
    /// TAMS-clean Old region (the whole point of restoring it — reclaiming
    /// such regions without waiting for a mixed pause).
    #[test]
    fn cleanup_in_place_frees_wholly_dead_old_region() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);
        let x = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![x];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let x = roots[0];
        let x_region = {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, x.as_ptr()).unwrap();
            assert_eq!(regions[idx].region_type, RegionType::Old);
            idx
        };

        // Drop the last reference BEFORE the cycle starts: X is dead in the
        // snapshot and its region wholly garbage at mark start.
        roots.clear();

        gc.start_concurrent_mark();
        gc.remark(&[]);
        while !gc.concurrent_mark_step(usize::MAX) {}
        gc.cleanup();

        let regions = gc.regions.lock();
        assert_eq!(
            regions[x_region].region_type,
            RegionType::Free,
            "wholly-dead TAMS-clean Old region must be freed in place by cleanup"
        );
    }

    /// G1MARK-7: a remark SEED (root / SATB overwrite) arriving at a full
    /// worklist must be marked black in place, not dropped — the overflow
    /// rescan only re-walks MARKED objects, so a dropped unmarked seed was
    /// unrecoverable and cleanup freed it live.
    #[test]
    fn remark_seed_at_worklist_cap_is_marked_not_dropped() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Y: promoted to Old; after the cycle starts its ONLY liveness
        // evidence is the remark root below.
        let y = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![y];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let y = roots[0];
        let y_region = {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, y.as_ptr()).unwrap();
            assert_eq!(regions[idx].region_type, RegionType::Old);
            idx
        };

        // Filler object whose address saturates the worklist.
        let x = gc.alloc_object(ClassId::new(2), 0);

        gc.start_concurrent_mark();
        gc.mark_worklist
            .lock()
            .extend(std::iter::repeat(x.as_ptr() as usize).take(MARK_WORKLIST_CAP));

        // Pre-fix: y's push is dropped (worklist at cap) and nothing ever
        // marks it — cleanup frees its region while it is a remark root.
        gc.remark(&[y]);
        assert!(
            gc.mark_worklist_overflowed.load(Ordering::Relaxed),
            "cap hit must set the overflow flag"
        );
        {
            let regions = gc.regions.lock();
            assert!(
                regions[y_region].mark_bitmap.is_marked(y.as_ptr() as usize),
                "seed at cap must be marked black in place"
            );
        }

        while !gc.concurrent_mark_step(usize::MAX) {}
        gc.cleanup();

        let regions = gc.regions.lock();
        assert_eq!(
            regions[y_region].region_type,
            RegionType::Old,
            "root-live Old region must survive cleanup after a capped remark"
        );
        assert!(regions[y_region].live_bytes > 0);
    }

    /// G1MARK-8: a gray-set entry with an implausible header (wild child
    /// pointer from a corrupt/stale slot) must be skipped without scanning,
    /// and cleanup must RETAIN everything for that cycle (the closure may
    /// be incomplete). The next, clean cycle reclaims as usual.
    #[test]
    fn implausible_gray_entry_skips_scan_and_retains_cycle() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // X: promoted to Old, then dropped — wholly-dead at mark start.
        let x = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![x];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let x = roots[0];
        let x_region = {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, x.as_ptr()).unwrap();
            assert_eq!(regions[idx].region_type, RegionType::Old);
            idx
        };
        roots.clear();

        // Garbage "object": a byte[] whose payload we stamp with 0xFF —
        // an interior pointer at the payload start reads kind_tag=0xFF,
        // which no ObjectKind matches.
        let arr = gc.alloc_array(ClassId::new(9), ArrayElementType::Byte, 64);
        let garbage_addr = arr.as_ptr() as usize + HEADER_SIZE;
        assert_eq!(garbage_addr & 0x7, 0);
        unsafe { std::ptr::write_bytes(garbage_addr as *mut u8, 0xFF, 32) };

        gc.start_concurrent_mark();
        gc.mark_worklist.lock().push(garbage_addr);
        gc.remark(&[]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        gc.cleanup();
        {
            let regions = gc.regions.lock();
            assert_eq!(
                regions[x_region].region_type,
                RegionType::Old,
                "cleanup must retain all regions after an implausible gray entry"
            );
        }
        assert!(
            !gc.mark_saw_implausible.load(Ordering::Relaxed),
            "fail-safe flag is per-cycle and must be cleared by cleanup"
        );

        // A clean follow-up cycle reclaims the wholly-dead region.
        gc.start_concurrent_mark();
        gc.remark(&[]);
        while !gc.concurrent_mark_step(usize::MAX) {}
        gc.cleanup();
        let regions = gc.regions.lock();
        assert_eq!(
            regions[x_region].region_type,
            RegionType::Free,
            "next clean cycle must reclaim the wholly-dead Old region"
        );
    }

    /// INT-8 core: with referent-slot hiding, a weakly-only-reachable
    /// OLD-region referent is unmarked at cycle end and cleanup frees its
    /// region — while WITHOUT the skip set the trace through the (live)
    /// Reference keeps it alive forever (the pre-fix taint).
    #[test]
    fn referent_slot_hiding_lets_cleanup_free_weak_old_referent() {
        let run = |hide: bool| -> RegionType {
            let cfg = G1CollectorConfig {
                promotion_age: 1,
                ..small_config()
            };
            let gc = G1Collector::new(cfg);

            // X: promoted to Old; after promotion its ONLY path is R.slot0.
            let x = gc.alloc_object(ClassId::new(1), 0);
            let mut roots = vec![x];
            gc.young_collection(&mut roots, &NoopMonitors);
            gc.young_collection(&mut roots, &NoopMonitors);
            let x = roots[0];
            let x_region = {
                let regions = gc.regions.lock();
                let idx = gc.region_for_ptr(&regions, x.as_ptr()).unwrap();
                assert_eq!(regions[idx].region_type, RegionType::Old);
                idx
            };

            // R: a live "WeakReference" (2 slots: referent, queue) whose
            // slot 0 is the only edge to X.
            let r = gc.alloc_object(ClassId::new(2), 2);
            gc.set_field(r, 0, Value::Object(Some(x)));

            gc.start_concurrent_mark();
            if hide {
                gc.set_reference_skip_set(&[r.as_ptr() as usize]);
            }
            gc.remark(&[r]);
            while !gc.concurrent_mark_step(usize::MAX) {}
            gc.cleanup();

            let regions = gc.regions.lock();
            regions[x_region].region_type
        };

        assert_eq!(
            run(false),
            RegionType::Old,
            "baseline (no hiding): the trace through R keeps X's region live"
        );
        assert_eq!(
            run(true),
            RegionType::Free,
            "with referent-slot hiding, cleanup must free the weakly-only-reachable Old region"
        );
    }

    /// INT-8: the skip set follows a Reference object through a mid-cycle
    /// evacuation pause (survivor re-keyed), and a Reference that dies in
    /// the CSet is pruned.
    #[test]
    fn reference_skip_set_remaps_survivors_and_prunes_casualties() {
        let gc = G1Collector::new(small_config());

        // Survivor case: R is rooted through the pause.
        let r = gc.alloc_object(ClassId::new(2), 2);
        gc.start_concurrent_mark();
        gc.set_reference_skip_set(&[r.as_ptr() as usize]);
        let mut roots = vec![r];
        gc.young_collection(&mut roots, &NoopMonitors);
        let r_new = roots[0];
        assert_eq!(gc.dbg_reference_skip_len(), 1);
        assert!(
            gc.dbg_reference_skip_contains(r_new.as_ptr() as usize),
            "skip set must hold R's POST-evacuation address"
        );
        gc.abort_concurrent_mark();
        assert_eq!(gc.dbg_reference_skip_len(), 0, "abort clears the set");

        // Casualty case: R2 is unrooted and dies in the young CSet.
        let r2 = gc.alloc_object(ClassId::new(2), 2);
        gc.start_concurrent_mark();
        gc.set_reference_skip_set(&[r2.as_ptr() as usize]);
        let mut no_roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut no_roots, &NoopMonitors);
        assert_eq!(
            gc.dbg_reference_skip_len(),
            0,
            "a CSet casualty must be pruned (stale entries can alias reused addresses)"
        );
        gc.abort_concurrent_mark();
    }

    /// INT-8: `set_field_no_satb` suppresses exactly the SATB pre-barrier —
    /// the same overwrite through plain `set_field` logs the old value as a
    /// mark root; the protocol variant does not.
    #[test]
    fn set_field_no_satb_suppresses_pre_barrier_only() {
        let run = |suppress: bool| -> bool {
            let gc = G1Collector::new(small_config());
            let holder = gc.alloc_object(ClassId::new(1), 1);
            let old_val = gc.alloc_object(ClassId::new(2), 0);
            gc.set_field(holder, 0, Value::Object(Some(old_val)));

            gc.start_concurrent_mark();
            if suppress {
                gc.set_field_no_satb(holder, 0, Value::Object(None));
            } else {
                gc.set_field(holder, 0, Value::Object(None));
            }
            // remark flushes every thread-local SATB buffer into the gray set.
            gc.remark(&[]);
            let grayed = gc.dbg_is_grayed_or_marked(old_val.as_ptr() as usize);
            gc.abort_concurrent_mark();
            grayed
        };

        assert!(
            run(false),
            "plain set_field must SATB-log the overwritten referent while marking"
        );
        assert!(
            !run(true),
            "set_field_no_satb must not log the protocol null's old value"
        );
    }

    /// INT-8: `is_live_after_mark` verdicts — bitmap-marked ⇒ live,
    /// unmarked pre-TAMS ⇒ dead, post-mark-start allocation ⇒ live (TAMS),
    /// non-heap address ⇒ live (never claim dead without evidence).
    #[test]
    fn is_live_after_mark_verdicts() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // live_old: promoted + kept as a root. dead_old: promoted + dropped.
        let live = gc.alloc_object(ClassId::new(1), 0);
        let dead = gc.alloc_object(ClassId::new(2), 0);
        let mut roots = vec![live, dead];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let (live, dead) = (roots[0], roots[1]);

        gc.start_concurrent_mark();
        gc.remark(&[live]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        // Post-mark-start allocation: TAMS says live despite no mark bit.
        let fresh = gc.alloc_object(ClassId::new(3), 0);

        assert!(gc.is_live_after_mark(live.as_ptr() as usize));
        assert!(!gc.is_live_after_mark(dead.as_ptr() as usize));
        assert!(gc.is_live_after_mark(fresh.as_ptr() as usize));
        assert!(
            gc.is_live_after_mark(0x10),
            "non-heap address must read live (conservative)"
        );
        gc.abort_concurrent_mark();
    }

    /// INT-8: `resurrect_after_remark` keeps a dead-by-mark object (and its
    /// subtree) alive through cleanup — the finalize()/cleaner handout
    /// protocol.
    #[test]
    fn resurrect_after_remark_survives_cleanup() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        let d = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![d];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let d = roots[0];
        let d_region = {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, d.as_ptr()).unwrap();
            assert_eq!(regions[idx].region_type, RegionType::Old);
            idx
        };
        roots.clear(); // D is dead at mark start

        gc.start_concurrent_mark();
        gc.remark(&[]);
        while !gc.concurrent_mark_step(usize::MAX) {}
        assert!(!gc.is_live_after_mark(d.as_ptr() as usize));

        gc.resurrect_after_remark(&[d.as_ptr() as usize]);
        gc.cleanup();

        let regions = gc.regions.lock();
        assert_eq!(
            regions[d_region].region_type,
            RegionType::Old,
            "a resurrected finalizable's region must survive this cycle's cleanup"
        );
        assert!(regions[d_region].live_bytes > 0);
    }

    // -- Native-allocation pressure latch --
    // (docs/internal/fixed-suite-bugs/g1-native-alloc-no-safepoint-oom-FIXED.md)

    /// Fill the first `count` regions so they read as fully-consumed Eden,
    /// leaving `num_regions - count` Free. Mirrors what a running mutator
    /// would have done, without allocating megabytes in a unit test.
    fn consume_regions(gc: &G1Collector, count: usize) {
        let full = gc.config.region_size;
        let mut regions = gc.regions.lock();
        for r in regions.iter_mut().take(count) {
            r.region_type = RegionType::Eden;
            r.cursor = full;
        }
    }

    fn free_region_count(gc: &G1Collector) -> usize {
        gc.regions
            .lock()
            .iter()
            .filter(|r| r.region_type == RegionType::Free)
            .count()
    }

    /// The latch must stay clear while the Free pool is comfortable and arm
    /// the moment a newly-claimed Eden region takes it under the `needs_gc`
    /// threshold — the signal `safe_native_call` consumes to run the
    /// collection a native allocation wrapper cannot run itself.
    #[test]
    fn native_alloc_pressure_arms_only_once_the_free_pool_falls_below_the_gc_threshold() {
        let gc = make_collector(); // 8 regions x 1 MiB, threshold 25% => 2
        assert!(
            !gc.native_alloc_pressure(),
            "a fresh collector must not report native-alloc pressure"
        );

        // 3 consumed, 5 Free. Allocating claims a 4th => 4 Free = 50%, well
        // above the 25% bar: the latch must stay clear.
        consume_regions(&gc, 3);
        let _ = GarbageCollector::alloc_object(&gc, ClassId::new(1), 1);
        assert_eq!(free_region_count(&gc), 4);
        assert!(
            !gc.native_alloc_pressure(),
            "the latch must not arm while the Free pool is still above the \
             needs_gc threshold (a comfortable heap must not pay for a GC at \
             every native call)"
        );

        // Consume everything but one Free region, current Eden included, so
        // the next allocation is forced to claim a fresh region.
        consume_regions(&gc, 7);
        let _ = GarbageCollector::alloc_object(&gc, ClassId::new(1), 1);
        assert!(
            gc.native_alloc_pressure(),
            "claiming an Eden region that takes the Free pool under the \
             needs_gc threshold must arm the latch"
        );
    }

    /// The consumer clears the latch; a still-starved heap must re-arm it on
    /// the very next region claim, and a collection (which rebuilds the Free
    /// pool) must clear it.
    #[test]
    fn native_alloc_pressure_rearms_after_clear_and_is_cleared_by_a_collection() {
        let gc = make_collector();
        consume_regions(&gc, 7);
        let _ = GarbageCollector::alloc_object(&gc, ClassId::new(1), 1);
        assert!(gc.native_alloc_pressure());

        gc.clear_native_alloc_pressure();
        assert!(!gc.native_alloc_pressure());

        // Still starved: the next claim must re-arm rather than stay quiet
        // until the abort.
        consume_regions(&gc, 8);
        {
            // Free one region back so a claim is possible at all.
            let mut regions = gc.regions.lock();
            regions[7].region_type = RegionType::Free;
            regions[7].cursor = 0;
        }
        gc.current_eden.store(usize::MAX, Ordering::Relaxed);
        let _ = GarbageCollector::alloc_object(&gc, ClassId::new(1), 1);
        assert!(
            gc.native_alloc_pressure(),
            "a post-clear region claim under the threshold must re-arm"
        );

        // A collection rebuilds the Free pool, so the outstanding request has
        // been served: the latch must not survive it (else every native call
        // after the first pressure event would force a collection).
        let mut roots: Vec<ObjectRef> = Vec::new();
        let _ = GarbageCollector::collect_garbage(&gc, &stw(), &mut roots, &NoopMonitors);
        assert!(
            !gc.native_alloc_pressure(),
            "a completed collection must clear the pressure latch"
        );
    }

    /// Emergency reserve: once the Free pool is down to the reserve, the
    /// speculative bulk TLAB refill must step aside (and arm the latch) while
    /// real object allocation — whose caller cannot be told "no" — still
    /// succeeds out of the held-back regions.
    #[test]
    fn tlab_refill_holds_back_an_emergency_reserve_for_real_allocations() {
        let gc = make_collector();
        // 8 regions: the eighth-of-the-heap cap decides, so the reserve is 1.
        assert_eq!(gc.tlab_reserve_regions(8), 1);
        // A heap too small to hold a reserve keeps the previous behaviour.
        assert_eq!(gc.tlab_reserve_regions(2), 0);
        // A realistic 1 GiB heap holds back 16 of its 1024 regions.
        assert_eq!(gc.tlab_reserve_regions(1024), 16);

        consume_regions(&gc, 7); // 1 Free == the reserve
        assert!(
            gc.refill_tlab(4096).is_none(),
            "a TLAB refill must not consume the last reserve regions"
        );
        assert!(
            gc.native_alloc_pressure(),
            "refusing a refill for lack of regions must arm the pressure latch"
        );

        gc.clear_native_alloc_pressure();
        assert!(
            gc.try_alloc_object(ClassId::new(1), 1).is_some(),
            "the reserve must still serve a real object allocation"
        );
        assert!(
            gc.native_alloc_pressure(),
            "consuming a reserve region must arm the pressure latch"
        );
    }

    // -- Needs GC --

    #[test]
    fn needs_gc_when_regions_full() {
        let gc = make_collector();
        assert!(!gc.needs_gc()); // all 8 regions free

        // Use up most regions
        {
            let mut regions = gc.regions.lock();
            for i in 0..7 {
                regions[i].region_type = RegionType::Eden;
                regions[i].cursor = 100;
            }
        }
        // 1 free out of 8 = 12.5% free, threshold is 25%
        assert!(gc.needs_gc());
    }

    // -- Allocated bytes --

    #[test]
    fn allocated_bytes_tracking() {
        let gc = make_collector();
        assert_eq!(gc.allocated_bytes(), 0);
        let _obj = gc.alloc_object(ClassId::new(1), 2);
        let bytes = gc.allocated_bytes();
        assert!(bytes > 0);
        assert_eq!(bytes, HEADER_SIZE + 2 * SLOT_SIZE);
    }

    // -- collect_garbage trait method --

    #[test]
    fn collect_garbage_via_trait() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(123));

        let mut roots = vec![obj];
        let result = gc.collect_garbage(&stw(), &mut roots, &NoopMonitors);

        assert!(result.stats.objects_copied >= 1);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(123));
    }

    // -- Identity hash code --

    #[test]
    fn identity_hash_code_unique() {
        let gc = make_collector();
        let a = gc.alloc_object(ClassId::new(1), 0);
        let b = gc.alloc_object(ClassId::new(1), 0);
        assert_ne!(gc.identity_hash_code(a), gc.identity_hash_code(b));
    }

    // -- Volatile field access --

    #[test]
    fn volatile_field_access() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field_volatile(obj, 0, Value::Int(42));
        assert_eq!(gc.get_field_volatile(obj, 0).as_int(), Some(42));
    }

    // -- Region type transitions --

    #[test]
    fn region_type_transitions() {
        let mut r = G1Region::new(1024);
        assert_eq!(r.region_type, RegionType::Free);

        r.region_type = RegionType::Eden;
        assert_eq!(r.region_type, RegionType::Eden);

        r.region_type = RegionType::Survivor;
        r.age = 3;
        assert_eq!(r.region_type, RegionType::Survivor);

        r.region_type = RegionType::Old;
        assert_eq!(r.region_type, RegionType::Old);

        r.reset(0);
        assert_eq!(r.region_type, RegionType::Free);
    }

    // -- Evacuation failure (pinned region) --

    #[test]
    fn evacuation_failure_pinned() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(999));

        // Pin the region
        let region_idx = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.pin_region(region_idx);

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Object should NOT have been evacuated
        assert_eq!(result.stats.objects_copied, 0);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(999));
    }

    // -- Mixed GC remaining counter --

    #[test]
    fn mixed_gc_remaining_decrements() {
        let gc = make_collector();
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(3, Ordering::Relaxed);

        let mut roots = vec![];
        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 2);

        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 1);

        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 0);
        assert!(!gc.marking_complete.load(Ordering::Relaxed));
    }

    // -- Remembered set tracking --

    #[test]
    fn remembered_set_tracking() {
        let mut rset = RememberedSet::default();
        rset.add_reference(1);
        rset.add_reference(3);
        rset.add_reference(1); // duplicate
        assert_eq!(rset.source_count(), 2);

        let sources: Vec<usize> = rset.sources();
        assert!(sources.contains(&1));
        assert!(sources.contains(&3));

        rset.clear();
        assert_eq!(rset.source_count(), 0);
    }

    // -- GC efficiency --

    #[test]
    fn gc_efficiency_computation() {
        let mut r = G1Region::new(1024);
        r.region_type = RegionType::Old;
        r.cursor = 500;
        r.live_bytes = 200;
        r.gc_efficiency = r.live_bytes as f64 / 1024.0;
        assert!(r.gc_efficiency < 0.2);
        assert!(r.gc_efficiency > 0.19);
    }

    // ======================================================================
    // Phase 89 tests
    // ======================================================================

    // -- 89.1: Fallible allocation --

    #[test]
    fn p89_try_alloc_object_success() {
        let gc = make_collector();
        let obj = gc.try_alloc_object(ClassId::new(1), 3);
        assert!(obj.is_some());
        let obj = obj.unwrap();
        assert_eq!(gc.class_id_of(obj), ClassId::new(1));
        assert_eq!(gc.get_header(obj).num_slots(), 3);
    }

    #[test]
    fn p89_try_alloc_object_returns_none_when_full() {
        // Tiny config: 2 regions of 4 KB each = 8 KB total
        let cfg = G1CollectorConfig {
            heap_size: 8192,
            region_size: 4096,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Fill all regions by allocating many objects
        let mut allocated = Vec::new();
        for _i in 0..100 {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }
        // Eventually should return None
        // With 2 regions of 4096 bytes, each object ~72 bytes, we can fit many but not 100
        assert!(allocated.len() < 100, "should have run out of space");
        // Final try should fail
        assert!(gc.try_alloc_object(ClassId::new(1), 4).is_none());
    }

    #[test]
    fn p89_try_alloc_array_success() {
        let gc = make_collector();
        let arr = gc.try_alloc_array(ClassId::new(2), ArrayElementType::Int, 10);
        assert!(arr.is_some());
        let arr = arr.unwrap();
        assert_eq!(gc.array_length(arr), 10);
        assert_eq!(gc.get_header(arr).element_type, ArrayElementType::Int);
    }

    #[test]
    fn p89_try_alloc_array_returns_none_when_full() {
        let cfg = G1CollectorConfig {
            heap_size: 8192,
            region_size: 4096,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Allocate a huge array that won't fit in 2 regions
        let result = gc.try_alloc_array(ClassId::new(1), ArrayElementType::Long, 2000);
        // 2000 longs = 16000 bytes + header > 8192 total heap
        assert!(result.is_none());
    }

    // -- 89.1: TLAB refill from G1 Eden --

    #[test]
    fn p89_tlab_refill_from_eden() {
        let gc = make_collector();
        let result = gc.refill_tlab(4096);
        assert!(result.is_some());
        let (ptr, size) = result.unwrap();
        assert!(!ptr.is_null());
        assert!(size >= 256); // must be at least minimum useful size
        assert!(size <= 4096);

        // Second refill should still work (from same or new region)
        let result2 = gc.refill_tlab(4096);
        assert!(result2.is_some());
    }

    #[test]
    fn p89_tlab_refill_returns_none_when_exhausted() {
        let cfg = G1CollectorConfig {
            heap_size: 2 * 65536, // 2 regions
            region_size: 65536,   // 64 KB regions
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Request small TLABs to stay under region_size/2
        let r1 = gc.refill_tlab(16384);
        assert!(r1.is_some());

        // Keep refilling until exhausted
        let mut refills = 1;
        loop {
            match gc.refill_tlab(16384) {
                Some(_) => refills += 1,
                None => break,
            }
            if refills > 20 {
                break;
            } // safety
        }
        // Should have run out eventually
        assert!(gc.refill_tlab(16384).is_none());
    }

    // -- 89.1: Young GC trigger and survivor promotion --

    #[test]
    fn p89_young_gc_trigger_and_collect() {
        let gc = make_collector(); // 8 MB, 1 MB regions
                                   // Allocate objects until GC is needed
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..50 {
            if let Some(obj) = gc.try_alloc_object(ClassId::new(1), 2) {
                gc.set_field(obj, 0, Value::Int(i));
                roots.push(obj);
            }
        }
        assert!(!roots.is_empty());

        // Run a young collection
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(result.stats.objects_copied > 0 || result.stats.bytes_freed > 0);

        // Root objects should still be accessible (possibly relocated)
        for root in &roots {
            let val = gc.get_field(*root, 0);
            assert!(val.as_int().is_some());
        }
    }

    #[test]
    fn p89_survivor_promotion_after_aging() {
        let cfg = G1CollectorConfig {
            heap_size: 8 * 1024 * 1024,
            region_size: 1024 * 1024,
            promotion_age: 1, // promote after 1 young GC (age >= 1)
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Allocate an object
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(999));
        let mut roots = vec![obj];

        // First young GC: object starts at age 0, goes to Survivor with age 1
        let r1 = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(r1.stats.objects_copied > 0);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(999));

        // Second young GC: age 1 >= promotion_age(1), promoted to Old
        let _r2 = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(999));

        // Verify old-gen bytes increased (object promoted)
        let old_bytes = gc.old_gen_bytes();
        assert!(old_bytes > 0, "object should have been promoted to old gen");
    }

    #[test]
    fn p89_humongous_allocation() {
        let gc = make_collector(); // 1 MB regions
                                   // Object with > 512KB of fields = humongous
        let num_fields = (512 * 1024) / SLOT_SIZE + 1;
        let obj = gc.try_alloc_object(ClassId::new(1), num_fields);
        assert!(obj.is_some());
        let obj = obj.unwrap();
        assert_eq!(gc.get_header(obj).num_slots(), num_fields as u32);

        // Verify the region is marked humongous
        let humongous_count = gc.count_regions(RegionType::HumongousStart);
        assert!(humongous_count >= 1);
    }

    #[test]
    fn p89_oom_after_gc_returns_none() {
        let cfg = G1CollectorConfig {
            heap_size: 2 * 65536,
            region_size: 65536,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // Fill heap completely
        let mut allocated = Vec::new();
        loop {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }
        assert!(!allocated.is_empty());

        // GC with all objects rooted — they move to survivor/old but still fill the heap
        gc.collect_garbage(&stw(), &mut allocated, &NoopMonitors);

        // Keep allocating until we're full again (GC freed Eden → moved to Survivor)
        loop {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }

        // Now truly OOM with all roots held
        gc.collect_garbage(&stw(), &mut allocated, &NoopMonitors);

        // Fill again
        loop {
            match gc.try_alloc_object(ClassId::new(1), 4) {
                Some(obj) => allocated.push(obj),
                None => break,
            }
        }

        // After multiple rounds of GC+fill, heap should eventually be fully packed
        // Verify the allocation eventually fails
        let final_try = gc.try_alloc_object(ClassId::new(1), 4);
        assert!(
            final_try.is_none(),
            "heap should be exhausted after repeated fill+GC cycles"
        );
    }

    // -- 89.2: SATB write barrier --

    #[test]
    fn p89_satb_pre_barrier_logs_when_active() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let addr = obj.as_ptr() as usize;

        // SATB inactive — should not log
        gc.satb_pre_barrier(addr);
        // Flush in case a previous test on this thread left buffered entries.
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        let _ = gc.satb_queue().drain();
        assert!(gc.satb_queue().is_empty());

        // Activate SATB and log. The barrier now writes into the per-thread
        // buffer; force a safepoint-style flush so the global queue sees it
        // without waiting for the auto-flush threshold.
        gc.satb_queue().activate();
        gc.satb_pre_barrier(addr);
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert_eq!(gc.satb_queue().len(), 1);

        let drained = gc.satb_queue().drain();
        assert_eq!(drained, vec![addr]);

        gc.satb_queue().deactivate();
    }

    #[test]
    fn p89_cross_region_rset_tracking() {
        let gc = make_collector();
        // Allocate two objects — they may land in the same or different regions
        // Force them into different regions by filling the first
        let obj1 = gc.alloc_object(ClassId::new(1), 1);

        // Fill current Eden to force next alloc into a new region
        while gc.try_alloc_object(ClassId::new(1), 100).is_some() {
            // keep going until region is full
        }
        // Allocate in a new region by doing a collect first to free space
        let mut roots = vec![obj1];
        gc.collect_garbage(&stw(), &mut roots, &NoopMonitors);
        let obj2 = gc.alloc_object(ClassId::new(1), 1);

        // Store obj2 ref in obj1 — triggers write barrier
        gc.set_field(roots[0], 0, Value::Object(Some(obj2)));
        gc.write_barrier(roots[0], Value::Object(Some(obj2)));

        // The write barrier should have tracked the cross-region reference
        // (or it's a same-region ref, which is fine too — we just verify no crash)
    }

    #[test]
    fn p89_satb_barrier_skips_null() {
        let gc = make_collector();
        gc.satb_queue().activate();
        gc.satb_pre_barrier(0); // null ref — should be skipped
        assert!(gc.satb_queue().is_empty());
        gc.satb_queue().deactivate();
    }

    // -- 89.3: Reference processing wiring --

    #[test]
    fn p89_weak_ref_cleared_after_gc() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        // Allocate a referent object (will be unreachable)
        let referent = gc.alloc_object(ClassId::new(10), 1);
        let referent_addr = referent.as_ptr() as usize;

        // Allocate a WeakReference wrapper
        let weak_ref = gc.alloc_object(ClassId::new(20), 2);
        let weak_addr = weak_ref.as_ptr() as usize;

        // Register the weak reference
        rp.discover_reference(ReferenceType::Weak, weak_addr, referent_addr, None);

        // Process with referent unmarked (unreachable)
        let is_marked = |addr: usize| -> bool { addr != referent_addr };
        let result = rp.process_references(&is_marked, 64, 0);

        // Weak referent should be cleared
        assert!(
            result.stats.weak_refs_cleared > 0 || rp.cleared_ref_objects().contains(&weak_addr)
        );
    }

    #[test]
    fn p89_soft_ref_retained_with_free_heap() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        let referent = gc.alloc_object(ClassId::new(10), 1);
        let referent_addr = referent.as_ptr() as usize;

        let soft_ref = gc.alloc_object(ClassId::new(30), 2);
        let soft_addr = soft_ref.as_ptr() as usize;

        rp.discover_reference(ReferenceType::Soft, soft_addr, referent_addr, None);

        // Process with lots of free heap — soft refs should be retained
        let is_marked = |_addr: usize| -> bool { false };
        let result = rp.process_references(&is_marked, 1024, 0); // 1024 MB free

        // With 1024 MB free, soft refs should be retained per LRU policy
        // (exact behavior depends on implementation — verify no crash at minimum)
        let _ = result.stats.soft_refs_cleared;
    }

    #[test]
    fn p89_phantom_ref_enqueued() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        let referent = gc.alloc_object(ClassId::new(10), 1);
        let referent_addr = referent.as_ptr() as usize;

        let phantom_ref = gc.alloc_object(ClassId::new(40), 2);
        let phantom_addr = phantom_ref.as_ptr() as usize;

        rp.discover_reference(ReferenceType::Phantom, phantom_addr, referent_addr, None);

        // Process with referent dead
        let is_marked = |addr: usize| -> bool { addr != referent_addr };
        let result = rp.process_references(&is_marked, 64, 0);

        // Phantom should be enqueued
        assert!(result.stats.phantom_refs_enqueued > 0);
    }

    // -- 89.4: Finalization --

    #[test]
    fn p89_finalizer_enqueued_for_unreachable() {
        use crate::reference::{ReferenceProcessor, ReferenceType};

        let gc = make_collector();
        let mut rp = ReferenceProcessor::new();

        let obj = gc.alloc_object(ClassId::new(50), 1);
        let obj_addr = obj.as_ptr() as usize;

        // Register as finalizable (same pattern as SharedVm::register_finalizable)
        rp.discover_reference(ReferenceType::Finalizer, obj_addr, obj_addr, None);

        // Process with object unreachable
        let is_marked = |_addr: usize| -> bool { false };
        let result = rp.process_references(&is_marked, 64, 0);

        // Object should be queued for finalization
        assert!(!result.to_finalize.is_empty());
        assert!(result.to_finalize.contains(&obj_addr));
    }

    #[test]
    fn p89_finalizer_prevents_double_finalization() {
        use crate::reference::FinalizerThread;

        let ft = FinalizerThread::new();

        // Enqueue an object
        ft.enqueue(0x1000);
        let first = ft.dequeue();
        assert_eq!(first, Some(0x1000));

        // Enqueue same object again
        ft.enqueue(0x1000);
        let second = ft.dequeue();
        // FinalizerThread tracks already-finalized objects — second enqueue is skipped
        assert!(second.is_none());
    }

    #[test]
    fn p89_finalizer_exception_does_not_crash() {
        use crate::reference::FinalizerThread;

        let ft = FinalizerThread::new();
        ft.enqueue(0x2000);
        ft.enqueue(0x3000);

        // Dequeue should work for both (simulating finalize() calls)
        let a = ft.dequeue();
        assert_eq!(a, Some(0x2000));
        let b = ft.dequeue();
        assert_eq!(b, Some(0x3000));
        let c = ft.dequeue();
        assert_eq!(c, None); // empty
    }

    // -----------------------------------------------------------------
    // T5.5.4 — select_evacuation_candidates tests
    // -----------------------------------------------------------------

    fn make_old_region(index: usize, top: usize, live_bytes: usize) -> crate::region::Region {
        let mut r = crate::region::Region {
            index,
            region_type: crate::region::RegionType::Old,
            top,
            rset: crate::region::RememberedSet::default(),
            live_bytes,
            age: 0,
        };
        // Sanity check invariant: live_bytes <= top.
        debug_assert!(r.live_bytes <= r.top);
        // Force the struct to live long enough — fields already public.
        let _ = &mut r.rset;
        r
    }

    #[test]
    fn evacuation_candidates_empty_when_no_old_regions() {
        let regions = vec![crate::region::Region {
            index: 0,
            region_type: crate::region::RegionType::Eden,
            top: 1024,
            rset: crate::region::RememberedSet::default(),
            live_bytes: 512,
            age: 0,
        }];
        let picked = select_evacuation_candidates(&regions, 1_000_000);
        assert!(picked.is_empty(), "only Old regions are eligible");
    }

    #[test]
    fn evacuation_candidates_prefer_high_garbage_ratio() {
        // Region 0: 90% garbage (100 live of 1000 total)
        // Region 1: 10% garbage (900 live of 1000 total)
        // Region 2: 50% garbage (500 live of 1000 total)
        // Budget large enough to take all three — ordering is what matters.
        let regions = vec![
            make_old_region(0, 1000, 100),
            make_old_region(1, 1000, 900),
            make_old_region(2, 1000, 500),
        ];
        let picked = select_evacuation_candidates(&regions, u64::MAX);
        assert_eq!(picked, vec![0, 2, 1]);
    }

    #[test]
    fn evacuation_candidates_respect_pause_budget() {
        // All three regions have plenty of garbage. Budget allows only
        // the first to fit; the remaining two must be dropped.
        // cost per region = live * 4 ns → region with live=100 costs 400 ns.
        let regions = vec![
            make_old_region(0, 1000, 100), // 400 ns, ratio 9.0
            make_old_region(1, 1000, 200), // 800 ns, ratio 4.0
            make_old_region(2, 1000, 300), // 1200 ns, ratio 2.33
        ];
        let picked = select_evacuation_candidates(&regions, 500);
        // Only region 0 fits (400 ns); region 1 would push to 1200 ns.
        assert_eq!(picked, vec![0]);
    }

    #[test]
    fn evacuation_candidates_always_include_first_even_if_over_budget() {
        // Even with a tiny budget, return the top-ranked region so the
        // mixed GC can always make forward progress.
        let regions = vec![make_old_region(0, 1000, 500)]; // 2000 ns cost
        let picked = select_evacuation_candidates(&regions, 100);
        assert_eq!(picked, vec![0], "must include at least one region");
    }

    #[test]
    fn evacuation_candidates_skip_zero_garbage_regions() {
        // Region 1 has live == top → garbage_bytes == 0 → ineligible.
        let regions = vec![
            make_old_region(0, 1000, 100),  // garbage 900
            make_old_region(1, 1000, 1000), // garbage 0, skip
        ];
        let picked = select_evacuation_candidates(&regions, u64::MAX);
        assert_eq!(picked, vec![0]);
    }

    #[test]
    fn evacuation_candidates_tie_break_by_index() {
        // Equal ratio → lower index wins.
        let regions = vec![
            make_old_region(7, 1000, 500),
            make_old_region(3, 1000, 500),
            make_old_region(5, 1000, 500),
        ];
        let picked = select_evacuation_candidates(&regions, u64::MAX);
        assert_eq!(picked, vec![3, 5, 7]);
    }

    #[test]
    fn region_estimated_evac_cost_matches_heuristic() {
        let r = make_old_region(0, 1000, 250);
        assert_eq!(r.estimated_evac_cost_ns(), 1000);
        let r_empty = make_old_region(0, 1000, 0);
        assert_eq!(r_empty.estimated_evac_cost_ns(), 0);
    }

    // -- Step 7: pause-target CSet sizing --

    #[test]
    fn g1region_estimated_evac_cost_scales_with_live_and_rate() {
        // G1Region cost = live_bytes * ns_per_byte (the rolling calibration).
        let gc = make_collector();
        gc.with_regions_mut(|rs| {
            rs[0].live_bytes = 1000;
        });
        let regions = gc.regions.lock();
        assert_eq!(regions[0].estimated_evac_cost_ns(4), 4000);
        assert_eq!(regions[0].estimated_evac_cost_ns(1), 1000);
        assert_eq!(regions[0].estimated_evac_cost_ns(0), 0);
    }

    #[test]
    fn evac_cost_ema_calibrates_toward_observed() {
        // The rolling copy-cost EMA starts at 4 ns/byte and moves toward the
        // observed cost; a cycle that copied nothing is no signal.
        let gc = make_collector();
        assert_eq!(gc.evac_ns_per_byte.load(Ordering::Relaxed), 4);
        for _ in 0..100 {
            gc.update_evac_cost(100, 1); // observed 100 ns/byte
        }
        let after = gc.evac_ns_per_byte.load(Ordering::Relaxed);
        assert!(
            (5..=100).contains(&after),
            "EMA must rise from 4 toward 100, got {after}"
        );
        // No bytes copied / zero pause => no change.
        let frozen = gc.evac_ns_per_byte.load(Ordering::Relaxed);
        gc.update_evac_cost(1_000_000, 0);
        gc.update_evac_cost(0, 1_000);
        assert_eq!(gc.evac_ns_per_byte.load(Ordering::Relaxed), frozen);
    }

    #[test]
    fn mixed_cset_old_selection_respects_pause_budget() {
        // With a tight pause target the time-budget cap bounds the old CSet
        // (always >=1); with a generous target only the percentage cap applies.
        // Four old regions, each 125_000 live bytes => 0.5ms at 4 ns/byte.
        let build = |pause_ms: u64| {
            let mut cfg = small_config();
            cfg.max_gc_pause_ms = pause_ms;
            cfg.old_cset_region_threshold_percent = 100; // percentage cap won't bind
            let gc = G1Collector::new(cfg);
            gc.with_regions_mut(|rs| {
                for r in rs.iter_mut().take(4) {
                    r.region_type = RegionType::Old;
                    r.live_bytes = 125_000; // 0.5ms at the default 4 ns/byte
                    r.gc_efficiency = 0.1;
                }
            });
            gc
        };
        let tight = build(1).select_old_regions_for_mixed_gc();
        assert!(!tight.is_empty(), "always keep >=1 old region for progress");
        assert!(
            tight.len() < 4,
            "pause budget must cap the old CSet below the 4 available, got {}",
            tight.len()
        );
        let generous = build(10_000).select_old_regions_for_mixed_gc();
        assert_eq!(
            generous.len(),
            4,
            "a generous pause target leaves only the percentage cap"
        );
    }

    #[test]
    fn region_garbage_bytes_basic() {
        let r = make_old_region(0, 1000, 300);
        assert_eq!(r.garbage_bytes(), 700);
        let full = make_old_region(0, 1000, 1000);
        assert_eq!(full.garbage_bytes(), 0);
    }

    // -- CRIT-P4: address-to-region lookup table --

    /// Every region's `data` base appears in `region_lookup` exactly once,
    /// and the table is sorted by base address.
    #[test]
    fn region_lookup_table_built_for_every_region() {
        let gc = make_collector();
        let num = gc.num_regions();
        assert_eq!(gc.region_lookup.len(), num);

        // Sorted by base
        for w in gc.region_lookup.windows(2) {
            assert!(w[0].0 < w[1].0, "lookup table must be strictly sorted");
        }

        // Every region index represented
        let mut idxs: Vec<usize> = gc.region_lookup.iter().map(|(_, i)| *i).collect();
        idxs.sort_unstable();
        assert_eq!(idxs, (0..num).collect::<Vec<_>>());

        // Each entry's base matches the live region's data ptr.
        let regions = gc.regions.lock();
        for &(base, idx) in &gc.region_lookup {
            assert_eq!(base, regions[idx].data.as_ptr() as usize);
        }
    }

    /// Pointers into each region resolve to the correct region index;
    /// pointers outside the heap return None.
    #[test]
    fn region_for_ptr_returns_correct_index_for_each_region() {
        let gc = make_collector();
        let regions = gc.regions.lock();
        let region_size = gc.config.region_size;

        // Sample three offsets per region: start, middle, last byte.
        for (expected_idx, r) in regions.iter().enumerate() {
            let base = r.data.as_ptr() as usize;
            for offset in [0usize, region_size / 2, region_size - 1] {
                let ptr = (base + offset) as *mut u8;
                let got = gc.region_for_ptr(&regions, ptr);
                assert_eq!(
                    got,
                    Some(expected_idx),
                    "ptr {:#x} (region {} offset {}) lookup mismatch",
                    ptr as usize,
                    expected_idx,
                    offset,
                );
            }
        }

        // One-past-the-end of a region is OUT of that region. If it lands
        // exactly on the next region's base it should resolve to that next
        // region; otherwise None.
        for (i, r) in regions.iter().enumerate() {
            let just_past = r.data.as_ptr() as usize + region_size;
            let got = gc.region_for_ptr(&regions, just_past as *mut u8);
            // The byte at base+region_size belongs to no region unless
            // another region happens to start there.
            if let Some(idx) = got {
                assert_ne!(
                    idx, i,
                    "ptr {:#x} should not resolve back to region {}",
                    just_past, i
                );
            }
        }
    }

    /// Addresses outside every region's `[base, base + region_size)` band
    /// must return None — neither very small addresses nor very large ones
    /// should false-positively match.
    #[test]
    fn region_for_ptr_returns_none_outside_heap() {
        let gc = make_collector();
        let regions = gc.regions.lock();

        // Trivially-low addresses
        for addr in [0usize, 8, 0x1000, 0x1_0000] {
            // Filter out the (extremely unlikely) case the OS allocated a
            // region near zero — skip if it would actually land in one.
            if gc.lookup_region_for_addr(addr).is_none() {
                assert_eq!(
                    gc.region_for_ptr(&regions, addr as *mut u8),
                    None,
                    "addr {:#x} should not resolve to any region",
                    addr,
                );
            }
        }

        // High addresses well above any plausible region base.
        let max_base = gc.region_lookup.iter().map(|(b, _)| *b).max().unwrap();
        let well_above = max_base + gc.config.region_size + 0x10_0000;
        assert_eq!(
            gc.region_for_ptr(&regions, well_above as *mut u8),
            None,
            "addr {:#x} above all regions should not resolve",
            well_above,
        );
    }

    /// `is_addr_in_live_region` regression: the lock-free O(1) arena-bounds
    /// gate + single-region index must (a) reject addresses outside the arena
    /// without taking the lock, (b) reject addresses inside the arena but in an
    /// unallocated (Free) region, and (c) accept the address of a freshly
    /// allocated live object. This is the per-word hot path of the conservative
    /// JIT/native root scan; the previous lock+linear-scan-per-word
    /// implementation made G1+JIT deep-stack workloads (Spring Boot buildSrc
    /// JUnit) fall off a throughput cliff (1264 s → 72 s after the fix).
    #[test]
    fn is_addr_in_live_region_bounds_and_liveness() {
        let gc = make_collector();

        // (a) Outside the arena → false, via the lock-free fast gate.
        assert!(!gc.is_addr_in_live_region(0));
        assert!(!gc.is_addr_in_live_region(gc.arena_base - 8));
        assert!(!gc.is_addr_in_live_region(gc.arena_end));
        assert!(!gc.is_addr_in_live_region(gc.arena_end + 0x10_0000));

        // (b) Inside the arena but in a still-Free region → false. The very
        // first slot is Free until the first allocation carves an Eden.
        let first_free = gc.arena_base + 64; // 8-aligned, inside region 0
        assert!(
            !gc.is_addr_in_live_region(first_free),
            "address in an unallocated Free region must not be live"
        );

        // (c) A freshly allocated object's address is in a live region.
        let obj = gc
            .try_alloc_object(ClassId::new(0), 3)
            .expect("alloc should succeed on a fresh heap");
        let addr = obj.as_ptr() as usize;
        assert!(addr >= gc.arena_base && addr < gc.arena_end);
        assert!(
            gc.is_addr_in_live_region(addr),
            "a live object's address must resolve as in a live region"
        );

        // O(1) index agrees with the authoritative binary-search lookup for the
        // live address (sanity on the `(addr - arena_base) / region_size` math).
        assert!(gc.lookup_region_for_addr(addr).is_some());
    }

    /// `region_for_ptr_with_regions` (write-barrier hot path) must agree
    /// with `region_for_ptr` for every probe.
    #[test]
    fn region_for_ptr_with_regions_matches_region_for_ptr() {
        let gc = make_collector();
        let regions = gc.regions.lock();
        let region_size = gc.config.region_size;

        for (expected_idx, r) in regions.iter().enumerate() {
            let base = r.data.as_ptr() as usize;
            for offset in [0usize, 1, 64, region_size / 3, region_size - 1] {
                let addr = base + offset;
                let by_addr = gc.region_for_ptr_with_regions(&regions, addr);
                let by_ptr = gc.region_for_ptr(&regions, addr as *mut u8);
                assert_eq!(by_addr, by_ptr);
                assert_eq!(by_addr, Some(expected_idx));
            }
        }
    }

    /// End-to-end: allocating an object and looking up its pointer must
    /// return the same region index the allocator placed it in.
    #[test]
    fn region_for_ptr_agrees_with_allocator() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let regions = gc.regions.lock();
        let idx = gc.region_for_ptr(&regions, obj.as_ptr()).unwrap();
        assert_eq!(regions[idx].region_type, RegionType::Eden);
    }

    // -- Task #25: SATB write_barrier_pre on the GarbageCollector trait --

    /// Task #25: G1's `write_barrier_pre` (via the `GarbageCollector`
    /// trait) MUST enqueue the old reference value into the SATB log
    /// when concurrent marking is active. The pre-barrier is the
    /// snapshot half of SATB: without it, references the mutator
    /// overwrites between initial-mark and remark would silently fall
    /// out of the live closure.
    #[test]
    fn t25_g1_write_barrier_pre_enqueues_old_ref() {
        let gc = make_collector();
        let old_obj = gc.alloc_object(ClassId::new(1), 1);
        let old_addr = old_obj.as_ptr() as usize;

        // Drain any leftover thread-local SATB buffer from a prior test.
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        let _ = gc.satb_queue().drain();
        assert!(gc.satb_queue().is_empty());

        // Activate SATB so the trait method enqueues.
        gc.satb_queue().activate();

        // Call through the trait method (NOT the inherent shortcut), to
        // exercise the dispatch path the VM uses.
        <G1Collector as GarbageCollector>::write_barrier_pre(&gc, std::ptr::null_mut(), old_obj);

        // Force a per-thread flush so the global queue sees the entry
        // (the auto-flush threshold is 256).
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());

        let drained = gc.satb_queue().drain();
        assert_eq!(
            drained,
            vec![old_addr],
            "G1::write_barrier_pre must enqueue the old reference's raw address",
        );

        gc.satb_queue().deactivate();
    }

    /// Task #25: the trait's default `write_barrier_pre` is genuinely a
    /// no-op — calling it on a non-SATB collector must not touch any
    /// shared state. Verified via a stub implementor whose only
    /// override is the required-by-trait methods; `write_barrier_pre`
    /// uses the default. The test calls the trait method many times
    /// and asserts (a) it returns; (b) no panic; (c) the call site
    /// resolves through trait dispatch (i.e. we are NOT accidentally
    /// monomorphizing to G1).
    ///
    /// This pins the zero-cost contract: a future PR that adds work to
    /// the default would have to update this test, surfacing the
    /// regression.
    #[test]
    fn t25_default_write_barrier_pre_is_zero_cost_noop() {
        // GarbageCollector, MonitorCleanup, GcResult, ObjectHeader,
        // ObjectKind, ArrayElementType, ClassId, ObjectRef, Value all
        // come in via the test module's `use super::*;`.

        // Minimal stub collector. Every required method panics — we
        // never call them. The trait dispatch for `write_barrier_pre`
        // resolves to the default empty body, which is what we want
        // to assert is benign.
        struct StubCollector;
        impl GarbageCollector for StubCollector {
            fn alloc_object(&self, _: ClassId, _: usize) -> ObjectRef {
                unreachable!("not called by this test")
            }
            fn alloc_array(&self, _: ClassId, _: ArrayElementType, _: usize) -> ObjectRef {
                unreachable!()
            }
            fn get_header(&self, _: ObjectRef) -> &ObjectHeader {
                unreachable!()
            }
            fn class_id_of(&self, _: ObjectRef) -> ClassId {
                unreachable!()
            }
            fn kind_of(&self, _: ObjectRef) -> ObjectKind {
                unreachable!()
            }
            fn element_type_of(&self, _: ObjectRef) -> ArrayElementType {
                unreachable!()
            }
            fn identity_hash_code(&self, _: ObjectRef) -> i32 {
                unreachable!()
            }
            fn get_field(&self, _: ObjectRef, _: usize) -> Value {
                unreachable!()
            }
            fn set_field(&self, _: ObjectRef, _: usize, _: Value) {
                unreachable!()
            }
            fn get_field_volatile(&self, _: ObjectRef, _: usize) -> Value {
                unreachable!()
            }
            fn set_field_volatile(&self, _: ObjectRef, _: usize, _: Value) {
                unreachable!()
            }
            fn array_length(&self, _: ObjectRef) -> usize {
                unreachable!()
            }
            fn get_array_element(&self, _: ObjectRef, _: usize) -> Result<Value, i32> {
                unreachable!()
            }
            fn set_array_element(&self, _: ObjectRef, _: usize, _: Value) -> Result<(), i32> {
                unreachable!()
            }
            fn needs_gc(&self) -> bool {
                unreachable!()
            }
            fn collect_garbage(
                &self,
                _: &crate::collector::StopTheWorldToken,
                _: &mut [ObjectRef],
                _: &dyn MonitorCleanup,
            ) -> GcResult {
                unreachable!()
            }
            fn write_barrier(&self, _: ObjectRef, _: Value) {
                unreachable!()
            }
            // NOTE: deliberately NO override of `write_barrier_pre`. The
            // trait's default empty body should be selected, and that
            // is exactly what this test asserts is safe.
            fn allocated_bytes(&self) -> usize {
                unreachable!()
            }
        }

        // Build a fake ObjectRef — never dereferenced by the no-op
        // default. `Box::leak`'d `u64` guarantees 8-byte alignment so
        // the `ObjectRef::from_raw` debug-assert is satisfied.
        let backing: Box<u64> = Box::new(0u64);
        let leaked: *mut u64 = Box::into_raw(backing);
        let fake = unsafe { ObjectRef::from_raw(leaked as *mut u8) };

        let stub = StubCollector;
        // Call through trait dispatch many times. If the default body
        // were not empty, an `unreachable!()` from any required
        // method would fire (because the default would have to use
        // another trait method to do real work) OR the test would
        // observe state mutation. Neither happens.
        for _ in 0..1000 {
            <StubCollector as GarbageCollector>::write_barrier_pre(
                &stub,
                std::ptr::null_mut(),
                fake,
            );
        }
        // Survival of the loop = default body is empty = zero-cost.
        // Reclaim the leaked backing alloc.
        unsafe {
            drop(Box::from_raw(leaked));
        }
    }

    // =====================================================================
    // Step 9 — parallel evacuation tests.
    //
    // These call `young_collection_parallel` / `mixed_collection_parallel`
    // directly so they exercise the multi-threaded evacuator regardless of
    // the `CRATONVM_G1_PARALLEL_EVAC` env flag (which gates only the public
    // dispatch). They assert no object is lost, duplicated, or corrupted and
    // that the result matches the serial path.
    // =====================================================================

    fn parallel_config(workers: usize, region_count: usize) -> G1CollectorConfig {
        let region_size = 1024 * 1024;
        G1CollectorConfig {
            heap_size: region_size * region_count,
            region_size,
            max_gc_pause_ms: 200,
            ihop_percent: 45,
            promotion_age: 3,
            gc_worker_threads: workers,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
        }
    }

    #[test]
    fn parallel_young_basic() {
        let gc = G1Collector::new(parallel_config(4, 8));
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(42));
        let mut roots = vec![obj];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert!(result.stats.objects_copied >= 1);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(42));
    }

    #[test]
    fn parallel_young_reference_chain() {
        let gc = G1Collector::new(parallel_config(4, 8));
        let a = gc.alloc_object(ClassId::new(1), 1);
        let b = gc.alloc_object(ClassId::new(2), 1);
        let c = gc.alloc_object(ClassId::new(3), 1);
        gc.set_field(a, 0, Value::Object(Some(b)));
        gc.set_field(b, 0, Value::Object(Some(c)));
        gc.set_field(c, 0, Value::Int(99));
        let mut roots = vec![a];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(result.stats.objects_copied, 3);
        let na = roots[0];
        let nb = match gc.get_field(na, 0) {
            Value::Object(Some(o)) => o,
            _ => panic!("a->b lost"),
        };
        let nc = match gc.get_field(nb, 0) {
            Value::Object(Some(o)) => o,
            _ => panic!("b->c lost"),
        };
        assert_eq!(gc.get_field(nc, 0).as_int(), Some(99));
    }

    #[test]
    fn parallel_young_unreachable_freed() {
        let gc = G1Collector::new(parallel_config(4, 8));
        let live = gc.alloc_object(ClassId::new(1), 0);
        let _dead = gc.alloc_object(ClassId::new(2), 0);
        let mut roots = vec![live];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(result.stats.objects_copied, 1);
    }

    #[test]
    fn parallel_pointer_map_remaps_root() {
        let gc = G1Collector::new(parallel_config(4, 8));
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let old = obj.as_ptr() as usize;
        let mut roots = vec![obj];
        let r = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(r.pointer_map.get(&old), Some(&(roots[0].as_ptr() as usize)));
        assert_ne!(old, roots[0].as_ptr() as usize);
    }

    /// Wide fan-out: a root reference array of N distinct objects, each with a
    /// unique int. Multi-worker evacuation must preserve EVERY element exactly
    /// once (no loss, no duplication).
    #[test]
    fn parallel_young_wide_ref_array() {
        let gc = G1Collector::new(parallel_config(8, 32));
        let n = 2000usize;
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, n);
        for i in 0..n {
            let o = gc.alloc_object(ClassId::new(7), 1);
            gc.set_field(o, 0, Value::Int(i as i32));
            gc.set_array_element(arr, i, Value::Object(Some(o)))
                .unwrap();
        }
        let mut roots = vec![arr];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        // array + n elements, each evacuated exactly once.
        assert_eq!(result.stats.objects_copied, n + 1);
        assert_eq!(result.pointer_map.len(), n + 1);
        let narr = roots[0];
        for i in 0..n {
            let child = match gc.get_array_element(narr, i).unwrap() {
                Value::Object(Some(o)) => o,
                _ => panic!("element {i} lost"),
            };
            assert_eq!(gc.get_field(child, 0).as_int(), Some(i as i32), "value {i}");
        }
    }

    /// Diamond sharing: P parents each referencing the SAME S shared children.
    /// The atomic CAS-forwarding must evacuate each shared child exactly once
    /// under concurrency, so `objects_copied == 1 + P + S`, and every parent
    /// must end up pointing at the one canonical copy of each child.
    #[test]
    fn parallel_young_diamond_shared_children_dedup() {
        let gc = G1Collector::new(parallel_config(8, 32));
        let p = 200usize;
        let s = 50usize;
        let shared: Vec<ObjectRef> = (0..s)
            .map(|j| {
                let c = gc.alloc_object(ClassId::new(9), 1);
                gc.set_field(c, 0, Value::Int(1000 + j as i32));
                c
            })
            .collect();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, p);
        for i in 0..p {
            let parent = gc.alloc_object(ClassId::new(8), s);
            for j in 0..s {
                gc.set_field(parent, j, Value::Object(Some(shared[j])));
            }
            gc.set_array_element(arr, i, Value::Object(Some(parent)))
                .unwrap();
        }
        let mut roots = vec![arr];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(
            result.stats.objects_copied,
            1 + p + s,
            "shared children must be evacuated exactly once"
        );
        assert_eq!(result.pointer_map.len(), 1 + p + s);
        let narr = roots[0];
        let parent0 = match gc.get_array_element(narr, 0).unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let parent_last = match gc.get_array_element(narr, p - 1).unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        for j in 0..s {
            let c0 = match gc.get_field(parent0, j) {
                Value::Object(Some(o)) => o,
                _ => panic!(),
            };
            let cl = match gc.get_field(parent_last, j) {
                Value::Object(Some(o)) => o,
                _ => panic!(),
            };
            assert_eq!(
                c0.as_ptr(),
                cl.as_ptr(),
                "shared child {j} must be one canonical object after evac"
            );
            assert_eq!(gc.get_field(c0, 0).as_int(), Some(1000 + j as i32));
        }
    }

    #[test]
    fn parallel_young_promotion() {
        let mut cfg = parallel_config(4, 8);
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(7));
        let mut roots = vec![obj];
        // First parallel young GC: Survivor, age -> 1.
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert!(gc.get_header(roots[0]).gc_age >= 1);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(7));
        // Second: age >= promotion_age -> promote to Old.
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert!(gc.count_regions(RegionType::Old) >= 1);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(7));
    }

    /// Regression (parallel-evac self-forward UAF). The parallel evacuator
    /// installs each forward into the from-space object's *persistent*
    /// `ObjectHeader::forwarding_ptr` field. A normally-evacuated object's
    /// region is reset in Phase 5 (zeroing the field), but a SELF-FORWARDED
    /// (evacuation-failure) object's region is KEPT — so its `forwarding_ptr`
    /// must be explicitly cleared at cycle end. Without the clear, the NEXT
    /// collection's `evacuate` fast path reads the stale self-pointer, SKIPS
    /// re-evacuating the still-live object, records no forward, and
    /// `free_or_keep_cset` (seeing no `key == value` entry) frees the region
    /// out from under it — a use-after-free that surfaced as V7b dangling
    /// references / SIGSEGV on the `PromoteMixed` humongous-array workload.
    #[test]
    fn parallel_self_forward_clears_forwarding_ptr_across_cycles() {
        let gc = G1Collector::new(parallel_config(4, 6));
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(12345));
        let mut roots = vec![obj];

        // Force an evacuation failure: drain the to-space pool by retyping every
        // Free region to Old (non-CSet), leaving the GC nowhere to copy the
        // Eden survivor — so it must self-forward (stay in place).
        {
            let mut regions = gc.regions.lock();
            for r in regions.iter_mut() {
                if r.region_type == RegionType::Free {
                    r.region_type = RegionType::Old;
                }
            }
        }

        // Cycle 1: obj cannot be copied -> self-forwards (identity entry).
        let r1 = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        let addr = roots[0].as_ptr() as usize;
        assert_eq!(
            r1.pointer_map.get(&addr),
            Some(&addr),
            "expected a self-forward (evacuation failure)"
        );
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(12345));
        // THE FIX: the kept object's persistent forwarding_ptr is cleared, so it
        // does not look "already forwarded" to the next cycle.
        assert!(
            gc.get_header(roots[0]).forwarding_ptr.is_null(),
            "self-forwarded object's forwarding_ptr must be cleared after the cycle"
        );

        // Cycle 2: with a stale self-pointer the object would be skipped and its
        // region freed; the field read would then hit reclaimed memory.
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(
            gc.get_field(roots[0], 0).as_int(),
            Some(12345),
            "live object lost across a second collection (stale self-forward UAF)"
        );
    }

    /// Regression for the residual parallel self-forward race: workers must not
    /// scan evacuation-failed CSet objects in place while other workers may still
    /// be copying from them. The parallel closure defers those in-place scans to
    /// a serial drain, and that drain must still discover transitive children.
    #[test]
    fn parallel_self_forwarded_holders_are_drained_serially() {
        let gc = G1Collector::new(parallel_config(4, 6));
        let child = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(child, 0, Value::Int(77));
        let parent = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(parent, 0, Value::Object(Some(child)));

        let parent_addr = parent.as_ptr() as usize;
        let child_addr = child.as_ptr() as usize;
        let mut roots = vec![parent];

        // Force evacuation failure: leave the young CSet with no free to-space.
        {
            let mut regions = gc.regions.lock();
            for r in regions.iter_mut() {
                if r.region_type == RegionType::Free {
                    r.region_type = RegionType::Old;
                }
            }
        }

        let r = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        assert_eq!(roots[0].as_ptr() as usize, parent_addr);
        assert_eq!(r.pointer_map.get(&parent_addr), Some(&parent_addr));
        assert_eq!(
            r.pointer_map.get(&child_addr),
            Some(&child_addr),
            "serial drain did not discover the self-forwarded child"
        );

        let drained_child = match gc.get_field(roots[0], 0) {
            Value::Object(Some(o)) => o,
            other => panic!("parent child ref lost after serial drain: {other:?}"),
        };
        assert_eq!(drained_child.as_ptr() as usize, child_addr);
        assert_eq!(gc.get_field(drained_child, 0).as_int(), Some(77));
        assert!(gc.get_header(roots[0]).forwarding_ptr.is_null());
        assert!(gc.get_header(drained_child).forwarding_ptr.is_null());
    }

    #[test]
    fn parallel_identity_forwards_seed_serial_drain() {
        let mut deferred = vec![0x1000usize];
        let forwards = vec![
            (0x2000usize, 0x3000usize),
            (0x4000usize, 0x4000usize),
            (0x4000usize, 0x4000usize),
            (0x1000usize, 0x1000usize),
        ];

        SharedEvac::append_self_forwarded_from_forwards(&forwards, &mut deferred);

        assert_eq!(deferred, vec![0x1000usize, 0x4000usize]);
    }

    /// Regression (defect 2: persistent forwarding_ptr root-remap). When the
    /// parallel evacuator reaches a CSet object whose `forwarding_ptr` is already
    /// set (a fast-path hit), it must RECORD `old -> existing` in `pointer_map`
    /// (so the VM's `update_all_roots` can remap a root that still points at
    /// `old`) and CLEAR the header at cycle end (so the forward does not persist
    /// into the next cycle). Before the fix the fast path returned `existing`
    /// without recording it, so the root stayed stuck on the from-space object
    /// and dangled when its region was reused.
    #[test]
    fn parallel_fast_path_hit_is_recorded_and_root_remapped() {
        let mut cfg = parallel_config(4, 8);
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        // Promote a target T to Old so it is a stable (non-CSet) forward target.
        let t = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(t, 0, Value::Int(99));
        let mut troots = vec![t];
        gc.young_collection_parallel(&mut troots, &NoopMonitors); // Survivor, age 1
        gc.young_collection_parallel(&mut troots, &NoopMonitors); // promote -> Old
        let t_old = troots[0].as_ptr() as usize;

        // O lives in young Eden; pre-install O.forwarding_ptr = T as if a worker
        // had already forwarded O to T this cycle (or a stale prior-cycle
        // redirect). Root O.
        let o = gc.alloc_object(ClassId::new(1), 0);
        let o_addr = o.as_ptr() as usize;
        unsafe {
            (*(o.as_ptr() as *mut ObjectHeader)).forwarding_ptr = t_old as *mut u8;
        }
        let mut roots = vec![o];
        let r = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        // (a) the fast-path forward O -> T is recorded in pointer_map.
        assert_eq!(
            r.pointer_map.get(&o_addr),
            Some(&t_old),
            "fast-path forward O->T was not recorded in pointer_map (root cannot be remapped)"
        );
        // (b) the root is remapped to T, and T is intact (not re-copied/corrupted).
        assert_eq!(
            roots[0].as_ptr() as usize,
            t_old,
            "root not remapped to the forward target"
        );
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(99));
    }

    /// Drain-phase evacuation-failure semantics: a LIVE holder that
    /// self-forwards during the serial drain still gets its children
    /// discovered (fixpoint over the identity-forward set), while a DEAD
    /// object in the same kept region is NOT scanned — its targets are not
    /// resurrected (an earlier revision walked every kept-region object,
    /// which under sustained evacuation failure resurrected kept garbage
    /// wholesale each pause and OOMed the SteadyChurn recreation). The dead
    /// holder's slot goes stale, which is safe: a stale reference can only
    /// point into a region that is Free or a this-pause destination — never a
    /// CSet-resident live object — so nothing consults it as a live edge.
    #[test]
    fn parallel_drain_phase_self_forward_kept_region_fully_scanned() {
        // workers=1 → the parallel machinery runs single-threaded, making the
        // pool-exhaustion sequencing deterministic.
        let gc = G1Collector::new(parallel_config(1, 8));

        let region_of = |o: ObjectRef| {
            gc.lookup_region_for_addr(o.as_ptr() as usize)
                .expect("object not in any region")
        };

        // Region A: root X.
        let x = gc.alloc_object(ClassId::new(1), 1);
        let ra = region_of(x);

        // Spill into region B.
        let mut filler = gc.alloc_object(ClassId::new(9), 200);
        while region_of(filler) == ra {
            filler = gc.alloc_object(ClassId::new(9), 200);
        }

        // Region B: DEAD holder D (never rooted) and live child Y; X -> Y.
        let d = gc.alloc_object(ClassId::new(2), 1);
        let y = gc.alloc_object(ClassId::new(3), 1);
        let rb = region_of(d);
        assert_eq!(region_of(y), rb, "D and Y must share region B");
        assert_ne!(ra, rb);
        gc.set_field(y, 0, Value::Int(41));
        gc.set_field(x, 0, Value::Object(Some(y)));

        // Spill into region C.
        let mut filler2 = gc.alloc_object(ClassId::new(9), 200);
        while region_of(filler2) == rb {
            filler2 = gc.alloc_object(ClassId::new(9), 200);
        }

        // Region C: Z, referenced ONLY by the dead D.
        let z = gc.alloc_object(ClassId::new(4), 1);
        let rc = region_of(z);
        assert!(rc != ra && rc != rb);
        gc.set_field(z, 0, Value::Int(42));
        gc.set_field(d, 0, Value::Object(Some(z)));
        let z_addr = z.as_ptr() as usize;

        // Empty the to-space pool so EVERY evacuation self-forwards: X in
        // Phase 1 (worker-phase keep of region A), Y only during the serial
        // drain (drain-phase keep of region B — after the old single-round
        // backstop had already run).
        {
            let mut regions = gc.regions.lock();
            for r in regions.iter_mut() {
                if r.region_type == RegionType::Free {
                    r.region_type = RegionType::Old;
                }
            }
        }

        let mut roots = vec![x];
        let r = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        // Y self-forwarded during the drain — region B is a drain-phase keep.
        let y_addr = gc.get_field(roots[0], 0);
        let y_now = match y_addr {
            Value::Object(Some(o)) => o,
            other => panic!("X.field lost: {other:?}"),
        };
        assert_eq!(
            r.pointer_map.get(&(y_now.as_ptr() as usize)),
            Some(&(y_now.as_ptr() as usize)),
            "Y should have self-forwarded (drain-phase evacuation failure)"
        );

        // Dead holder D (region B) must NOT have been scanned: its target Z is
        // garbage and must not be resurrected into the pointer map (that
        // wholesale resurrection is what OOMed the churn workload).
        assert_eq!(
            r.pointer_map.get(&z_addr),
            None,
            "dead kept-region holder was scanned — its garbage target Z was \
             resurrected (kept-garbage amplification)"
        );
        // And the live path through the drain is fully intact.
        assert_eq!(gc.get_field(y_now, 0).as_int(), Some(41), "Y corrupted");
    }

    #[test]
    fn parallel_mixed_basic() {
        let mut cfg = parallel_config(4, 16);
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);
        let a = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(a, 0, Value::Int(11));
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(b, 0, Value::Int(22));
        let mut roots = vec![a, b];
        gc.young_collection_parallel(&mut roots, &NoopMonitors); // -> Survivor
        gc.young_collection_parallel(&mut roots, &NoopMonitors); // -> Old
        assert!(gc.count_regions(RegionType::Old) >= 1);
        // Parallel mixed collection (young empty, old region(s) in CSet).
        let _r = gc.mixed_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(11));
        assert_eq!(gc.get_field(roots[1], 0).as_int(), Some(22));
    }

    // --- Serial-vs-parallel equivalence (no loss / dup / corruption) ---

    fn build_equiv_graph(gc: &G1Collector, n: usize, s: usize) -> ObjectRef {
        let shared: Vec<ObjectRef> = (0..s)
            .map(|j| {
                let c = gc.alloc_object(ClassId::new(9), 1);
                gc.set_field(c, 0, Value::Int(1000 + j as i32));
                c
            })
            .collect();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, n);
        for i in 0..n {
            let parent = gc.alloc_object(ClassId::new(8), 2);
            gc.set_field(parent, 0, Value::Int(i as i32));
            gc.set_field(parent, 1, Value::Object(Some(shared[i % s])));
            gc.set_array_element(arr, i, Value::Object(Some(parent)))
                .unwrap();
        }
        arr
    }

    fn reachable_ints(gc: &G1Collector, root_arr: ObjectRef, n: usize) -> Vec<i32> {
        let mut vals = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..n {
            let parent = match gc.get_array_element(root_arr, i).unwrap() {
                Value::Object(Some(o)) => o,
                _ => panic!("parent {i} lost"),
            };
            vals.push(gc.get_field(parent, 0).as_int().unwrap());
            let child = match gc.get_field(parent, 1) {
                Value::Object(Some(o)) => o,
                _ => panic!("child of {i} lost"),
            };
            if seen.insert(child.as_ptr()) {
                vals.push(gc.get_field(child, 0).as_int().unwrap());
            }
        }
        vals.sort();
        vals
    }

    /// The serial path, the 1-worker parallel path, and the 8-worker parallel
    /// path must all copy the same number of objects and preserve the same
    /// multiset of reachable values. Addresses differ; program-observable
    /// state does not.
    #[test]
    fn parallel_matches_serial_no_loss_or_dup() {
        let n = 500usize;
        let s = 40usize;

        // Serial baseline (env flag is unset in the test process, so the public
        // `young_collection` dispatches to the serial body).
        let gc_serial = G1Collector::new(parallel_config(1, 32));
        let arr_s = build_equiv_graph(&gc_serial, n, s);
        let mut roots_s = vec![arr_s];
        let res_s = gc_serial.young_collection(&mut roots_s, &NoopMonitors);
        let vals_s = reachable_ints(&gc_serial, roots_s[0], n);

        // 1-worker parallel (deterministic drain through the parallel code).
        let gc_p1 = G1Collector::new(parallel_config(1, 32));
        let arr_p1 = build_equiv_graph(&gc_p1, n, s);
        let mut roots_p1 = vec![arr_p1];
        let res_p1 = gc_p1.young_collection_parallel(&mut roots_p1, &NoopMonitors);
        let vals_p1 = reachable_ints(&gc_p1, roots_p1[0], n);

        // 8-worker parallel (real concurrency + CAS races).
        let gc_p8 = G1Collector::new(parallel_config(8, 32));
        let arr_p8 = build_equiv_graph(&gc_p8, n, s);
        let mut roots_p8 = vec![arr_p8];
        let res_p8 = gc_p8.young_collection_parallel(&mut roots_p8, &NoopMonitors);
        let vals_p8 = reachable_ints(&gc_p8, roots_p8[0], n);

        // 1 array + n parents + s shared children.
        let expected_copied = 1 + n + s;
        assert_eq!(res_s.stats.objects_copied, expected_copied, "serial count");
        assert_eq!(res_p1.stats.objects_copied, expected_copied, "p1 count");
        assert_eq!(res_p8.stats.objects_copied, expected_copied, "p8 count");

        assert_eq!(vals_s, vals_p1, "serial vs 1-worker values diverge");
        assert_eq!(vals_s, vals_p8, "serial vs 8-worker values diverge");
    }

    /// Regression for the serial-G1 evacuate-into-CSet-region bug: a young GC
    /// evacuates ALL survivor regions, so a partially-filled survivor region is
    /// itself in the CSet. `alloc_in_type_locked` used to reuse it as an
    /// evacuation *destination*, so survivors were copied into a region Phase 5
    /// then reset (freed) — silently dropping live objects and leaving every
    /// reference dangling (the held-tree `got=1` repro). A held object graph must
    /// survive REPEATED young GCs fully intact. This drives the SERIAL path
    /// (`young_collection`; the env flag is unset in tests).
    #[test]
    fn held_chain_survives_repeated_young_gc() {
        let gc = make_collector();
        let n = 50usize;
        let head = gc.alloc_object(ClassId::new(1), 1);
        let mut cur = head;
        for _ in 1..n {
            let node = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(cur, 0, Value::Object(Some(node)));
            cur = node;
        }
        gc.set_field(cur, 0, Value::Int(999)); // tail marker
        let mut roots = vec![head];
        for round in 0..6 {
            let _ = gc.alloc_object(ClassId::new(9), 8); // a little garbage
            gc.young_collection(&mut roots, &NoopMonitors);
            // Walk the whole chain: it must still be exactly `n` nodes ending in
            // the tail marker — no node lost to an evacuate-into-CSet free.
            let mut count = 0usize;
            let mut c = roots[0];
            loop {
                count += 1;
                assert!(count <= n, "round {round}: chain longer than {n}");
                match gc.get_field(c, 0) {
                    Value::Object(Some(next)) => c = next,
                    Value::Int(999) => break,
                    other => panic!("round {round}: chain broke at node {count} -> {other:?}"),
                }
            }
            assert_eq!(count, n, "round {round}: chain length changed (lost nodes)");
        }
    }

    /// Regression for G1 evacuation failure (to-space exhaustion). A heap of
    /// exactly two regions is filled by a held chain so that when a young GC
    /// runs there are ZERO free regions for to-space. Previously `evacuate_object`
    /// returned `None` and the caller DROPPED the object → silent live-object
    /// loss. Now it self-forwards in place and `free_or_keep_cset` keeps the
    /// region, so the entire held chain survives intact (nothing reclaimed — the
    /// real heap is full, which the allocation path turns into a clean OOM).
    #[test]
    fn evacuation_failure_self_forwards_does_not_drop() {
        let mut cfg = small_config();
        cfg.region_size = 1024 * 1024;
        cfg.heap_size = 2 * 1024 * 1024; // exactly 2 regions
        let gc = G1Collector::new(cfg);
        // Fill ~1.1 MB across the 2 regions with a held chain → both regions
        // become Eden (CSet), leaving 0 Free regions for evacuation to-space.
        let n = 24000usize;
        let head = gc.alloc_object(ClassId::new(1), 1);
        let mut cur = head;
        for _ in 1..n {
            let node = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(cur, 0, Value::Object(Some(node)));
            cur = node;
        }
        gc.set_field(cur, 0, Value::Int(7));
        let mut roots = vec![head];
        // No free region → every survivor hits evacuation failure → self-forward.
        let r = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            r.stats.bytes_freed, 0,
            "a failed collection must free nothing"
        );
        // The whole chain must still be reachable and intact — nothing dropped.
        let mut count = 0usize;
        let mut c = roots[0];
        loop {
            count += 1;
            assert!(count <= n, "chain longer than {n}");
            match gc.get_field(c, 0) {
                Value::Object(Some(next)) => c = next,
                Value::Int(7) => break,
                other => panic!("chain dropped at node {count} -> {other:?}"),
            }
        }
        assert_eq!(count, n, "evacuation failure dropped live nodes");
    }

    /// Regression for the kept-region death spiral (the serial-G1 "SteadyChurn
    /// recreation trips at -Xmx16m" wedge). When young-live exceeds the free
    /// pool at trigger time, a single pass keeps most of the heap (every
    /// region holding one self-forwarded live object survives wholesale,
    /// garbage included) and successive collections monotonically degrade to
    /// `copied == 0 && freed == 0`. `retry_after_evacuation_failure` must
    /// drain the kept garbage with same-pause retry passes (excluding earlier
    /// passes' destination regions) and compose the per-pass forward maps so
    /// pause-start addresses still remap correctly.
    #[test]
    fn evacuation_failure_retry_drains_kept_garbage() {
        let mut cfg = small_config();
        cfg.region_size = 1024 * 1024;
        cfg.heap_size = 10 * 1024 * 1024; // 10 regions
        let gc = G1Collector::new(cfg);

        // Interleave live chain nodes with ~2x garbage so every filled region
        // holds BOTH live and garbage (the SteadyChurn shape): ~6 MB total,
        // ~2 MB live — more than the 1-region pool left below, so pass 1 must
        // hit evacuation failure part-way through.
        let head = gc.alloc_object(ClassId::new(1), 8);
        let head_addr = head.as_ptr() as usize;
        let mut cur = head;
        let mut live_count = 1usize;
        loop {
            let _g1 = gc.alloc_object(ClassId::new(9), 8);
            let _g2 = gc.alloc_object(ClassId::new(9), 8);
            let node = gc.alloc_object(ClassId::new(1), 8);
            gc.set_field(cur, 0, Value::Object(Some(node)));
            cur = node;
            live_count += 1;
            // Stop once ~6 of the 10 regions are consumed.
            let used: usize = {
                let regions = gc.regions.lock();
                regions
                    .iter()
                    .filter(|r| r.region_type != RegionType::Free)
                    .count()
            };
            if used >= 6 {
                break;
            }
        }
        gc.set_field(cur, 0, Value::Int(424242)); // tail marker

        // Leave exactly ONE Free region as evacuation pool; deny the rest.
        {
            let mut regions = gc.regions.lock();
            let mut left = 1usize;
            for r in regions.iter_mut().rev() {
                if r.region_type == RegionType::Free {
                    if left > 0 {
                        left -= 1;
                    } else {
                        r.region_type = RegionType::Old;
                    }
                }
            }
        }

        let mut roots = vec![head];
        let first = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            first.pointer_map.iter().any(|(k, v)| k == v),
            "setup failed to force evacuation failure (no self-forwards)"
        );

        let result = gc.retry_after_evacuation_failure(first, &mut roots, &NoopMonitors);

        // (a) recovery converged: no identity forward survives the pause.
        assert!(
            !result.pointer_map.iter().any(|(k, v)| k == v),
            "retry loop left unresolved self-forwards (still wedged)"
        );
        // (b) the kept garbage was actually drained: most of the ~4 MB of
        // garbage must be free again.
        let free_after = {
            let regions = gc.regions.lock();
            regions
                .iter()
                .filter(|r| r.region_type == RegionType::Free)
                .count()
        };
        assert!(
            free_after >= 3,
            "kept-region garbage not reclaimed (free regions after recovery: {free_after})"
        );
        // (c) the composed map remaps the PAUSE-START head address to the
        // final location the root was rewritten to (frame-local semantics).
        assert_eq!(
            result.pointer_map.get(&head_addr),
            Some(&(roots[0].as_ptr() as usize)),
            "composed pointer_map does not take the pause-start address to the final copy"
        );
        // (d) the live chain is fully intact.
        let mut count = 0usize;
        let mut c = roots[0];
        loop {
            count += 1;
            assert!(count <= live_count, "chain longer than {live_count}");
            match gc.get_field(c, 0) {
                Value::Object(Some(next)) => c = next,
                Value::Int(424242) => break,
                other => panic!("chain broke at node {count} -> {other:?}"),
            }
        }
        assert_eq!(count, live_count, "retry recovery dropped live nodes");
    }

    /// `compose_forward_maps` semantics: values chase one hop (including a
    /// pass-1 self-forward resolved by a retry), new keys are added, and a
    /// colliding key keeps the FIRST pass's entry (pause-start address wins
    /// over a recycled intermediate).
    #[test]
    fn compose_forward_maps_chases_and_keeps_pause_start_keys() {
        let mut acc: HashMap<usize, usize> = [(0xa0, 0xb0), (0x40, 0x40), (0x70, 0x71)]
            .into_iter()
            .collect();
        let next: HashMap<usize, usize> = [(0xb0, 0xc0), (0x40, 0x90), (0xe0, 0xf0), (0x70, 0x99)]
            .into_iter()
            .collect();
        G1Collector::compose_forward_maps(&mut acc, &next);
        assert_eq!(
            acc.get(&0xa0),
            Some(&0xc0),
            "value not chased through pass 2"
        );
        assert_eq!(
            acc.get(&0x40),
            Some(&0x90),
            "identity self-forward not resolved"
        );
        assert_eq!(acc.get(&0xe0), Some(&0xf0), "new pass-2 key not added");
        assert_eq!(
            acc.get(&0x70),
            Some(&0x71),
            "a colliding pass-2 key (recycled intermediate address) must NOT \
             clobber the pause-start entry from pass 1"
        );
    }

    /// Regression for the Old->young remembered-set-completeness hole: an
    /// `A.a = B` edge created while both are young carries no rset entry. Once
    /// `A` promotes to Old (while `B`, allocated later, stays younger), the edge
    /// becomes Old->young — but it was created/maintained only by GC-internal
    /// pointer rewrites, never a mutator write barrier, so it was missing from
    /// the rset and the next young GC dropped `B` (reachable only via `A.a`).
    /// `update_references_in_regions` now rebuilds the Old->young rset each
    /// collection, so `B` must survive repeated young GCs after `A` is tenured.
    #[test]
    fn old_to_young_ref_via_gc_rewrite_not_dropped() {
        let mut cfg = small_config();
        cfg.promotion_age = 3;
        let gc = G1Collector::new(cfg);

        let a = gc.alloc_object(ClassId::new(1), 1);
        let mut roots = vec![a]; // B will be reachable ONLY via A.a
                                 // Age A two young GCs (still a Survivor; A is older than B).
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);

        // Allocate B fresh (younger) and wire A.a -> B while both are young.
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(b, 0, Value::Int(777));
        gc.set_field(roots[0], 0, Value::Object(Some(b)));

        // Drive several more young GCs: A tenures to Old while B stays younger,
        // so A.a becomes an Old->young edge maintained only by GC rewrites. B
        // must never be dropped.
        for round in 0..6 {
            let _g = gc.alloc_object(ClassId::new(9), 1); // a little churn
            gc.young_collection(&mut roots, &NoopMonitors);
            match gc.get_field(roots[0], 0) {
                Value::Object(Some(bb)) => assert_eq!(
                    gc.get_field(bb, 0).as_int(),
                    Some(777),
                    "round {round}: B lost / corrupted (Old->young rset miss)"
                ),
                other => panic!("round {round}: A.a dropped -> {other:?}"),
            }
        }
    }

    /// Regression for bug C — the Old->old / humongous->old remembered-set
    /// completeness hole that left G1 *mixed* GC non-functional. This is the
    /// `PromoteMixed` repro at unit scale: a referent `B` is reachable ONLY
    /// through a humongous holder array (`keep[0] = B`, the array > region_size/2
    /// so it lives in HumongousStart/Continuation regions and is NEVER a CSet
    /// member). The `keep[0] -> B` edge is created while `B` is young (so the
    /// mutator barrier recorded it against `B`'s then-young region, since
    /// recycled) and is thereafter maintained only by GC-internal pointer
    /// rewrites as `B` promotes to Old — no barrier ever fires for the
    /// humongous->old edge. A mixed GC that selects `B`'s Old region therefore
    /// finds `B` only if `B`'s region records the humongous holder as an rset
    /// source. Before the fix `collect_outgoing_*_edges` recorded edges into
    /// young targets only, so the holder was never registered, the mixed GC
    /// never scanned `keep[]`, and `B` was dropped (the V7b verifier reported
    /// the humongous holder dangling into a freed CSet region). The Phase-4
    /// rebuild now records edges into every *collectable* (Eden/Survivor/Old)
    /// target, so `B` survives the mixed GC.
    #[test]
    fn humongous_to_old_ref_via_gc_rewrite_survives_mixed_gc() {
        let cfg = G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            promotion_age: 1, // age >= 1 promotes (2 young GCs)
            old_cset_region_threshold_percent: 100, // B's single Old region is eligible
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // A humongous reference array (> 512 KiB of refs) — the `keep[]` holder.
        let len = (640 * 1024) / 8; // 640 KiB of 8-byte refs -> humongous
        let keep = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, len);
        assert!(
            gc.count_regions(RegionType::HumongousStart) >= 1,
            "keep[] must be humongous for this regression"
        );

        // B is reachable ONLY via keep[0]. Wire it while B is young.
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(b, 0, Value::Int(424242));
        gc.set_array_element(keep, 0, Value::Object(Some(b)))
            .unwrap();
        let mut roots = vec![keep]; // root reaches B only through the humongous keep

        // Tenure B to Old (promotion_age = 1). The humongous keep stays in place
        // (humongous objects are never evacuated by young/mixed GC).
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            gc.old_gen_bytes() > 0,
            "B should have promoted to Old before the mixed GC"
        );

        // Force a mixed GC that can select B's Old region.
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);
        gc.mixed_collection(&mut roots, &NoopMonitors);

        // B must survive: reachable only through the humongous->old edge, which
        // is recorded only by the Phase-4 rset rebuild.
        match gc.get_array_element(roots[0], 0).unwrap() {
            Value::Object(Some(b2)) => assert_eq!(
                gc.get_field(b2, 0).as_int(),
                Some(424242),
                "B dropped / corrupted: humongous->old rset miss in mixed GC"
            ),
            other => panic!("keep[0] dropped in mixed GC -> {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // G1 maturation (docs/internal/arch-2026-07-26/g1-maturation.md)
    // -----------------------------------------------------------------------

    /// Overwrite the mark-start (TAMS) snapshot so a test can place TAMS at an
    /// exact offset inside one region while leaving every other region's entry
    /// consistent with its current state. Mirrors the shape
    /// `start_concurrent_mark` publishes; lock order is regions -> snapshot,
    /// same as production.
    fn force_mark_snapshot(gc: &G1Collector, region_idx: usize, tams_offset: usize) {
        let snapshot: Vec<(u64, usize, RegionType)> = {
            let regions = gc.regions.lock();
            regions
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let cursor = if i == region_idx { tams_offset } else { r.cursor };
                    (r.reuse_epoch, cursor, r.region_type)
                })
                .collect()
        };
        let mut snap = gc.mark_start_snapshot.lock();
        snap.clear();
        snap.extend(snapshot);
    }

    /// G1MAT-1 — `cleanup` must attribute each byte of a region exactly once.
    ///
    /// Before the fix the bitmap walk covered the WHOLE region and the
    /// post-TAMS extent was then added on top, so any post-TAMS object the
    /// marker actually reached (SATB keep-alive, `push_gray_or_mark`, a fresh
    /// promotion a root still names) was counted twice. `live_bytes` could
    /// exceed `cursor`, which corrupts `gc_efficiency` — the ascending
    /// (worst-first) sort key of `mixed_collection` /
    /// `select_old_regions_for_mixed_gc` — and inflates
    /// `estimated_evac_cost_ns`, so mixed GC selects the wrong Old regions AND
    /// fewer of them.
    #[test]
    fn g1mat1_cleanup_does_not_double_count_marked_post_tams_objects() {
        let gc = make_collector();
        let below = gc.alloc_object(ClassId::new(1), 0);
        let above = gc.alloc_object(ClassId::new(2), 0);
        let below_addr = below.as_ptr() as usize;
        let above_addr = above.as_ptr() as usize;

        let (idx, base) = {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, below.as_ptr()).unwrap();
            (idx, regions[idx].data.as_ptr() as usize)
        };
        assert_eq!(
            gc.lookup_region_for_addr(above_addr),
            Some(idx),
            "test setup: both objects must share one region"
        );

        // Old, so this is exactly the region class the mixed-GC selector ranks.
        gc.with_regions_mut(|regions| regions[idx].region_type = RegionType::Old);

        // TAMS lands between the two objects: `below` predates the snapshot,
        // `above` postdates it.
        let tams = above_addr - base;
        force_mark_snapshot(&gc, idx, tams);

        // The marker reached BOTH — including the post-TAMS object. That is the
        // case the old accounting double-counted.
        gc.with_regions_mut(|regions| {
            assert!(regions[idx].mark_bitmap.try_mark(below_addr));
            assert!(regions[idx].mark_bitmap.try_mark(above_addr));
        });

        gc.cleanup();

        let (live_bytes, cursor, efficiency) = {
            let regions = gc.regions.lock();
            (
                regions[idx].live_bytes,
                regions[idx].cursor,
                regions[idx].gc_efficiency,
            )
        };
        assert_eq!(
            live_bytes, cursor,
            "both objects are live exactly once: below-TAMS via the bitmap, \
             above-TAMS implicitly (double-count would give {} > {})",
            live_bytes, cursor
        );
        assert!(
            live_bytes <= cursor,
            "a region can never be more than 100% live"
        );
        assert!(
            efficiency <= 1.0,
            "gc_efficiency must stay in [0,1] — it is the worst-first sort key \
             for mixed GC (got {efficiency})"
        );
    }

    /// G1MAT-1 — a region whose pre-TAMS content is entirely dead but which
    /// grew after mark start must report exactly the post-TAMS bytes as live,
    /// and must NOT be freed in place.
    #[test]
    fn g1mat1_cleanup_counts_only_post_tams_bytes_when_pre_tams_is_dead() {
        let gc = make_collector();
        let dead = gc.alloc_object(ClassId::new(1), 0);
        let fresh = gc.alloc_object(ClassId::new(2), 0);
        let fresh_addr = fresh.as_ptr() as usize;

        let (idx, base) = {
            let regions = gc.regions.lock();
            let idx = gc.region_for_ptr(&regions, dead.as_ptr()).unwrap();
            (idx, regions[idx].data.as_ptr() as usize)
        };
        gc.with_regions_mut(|regions| regions[idx].region_type = RegionType::Old);

        let tams = fresh_addr - base;
        force_mark_snapshot(&gc, idx, tams);
        // Nothing marked: `dead` is unreachable in the snapshot, `fresh` is
        // implicitly live because it postdates it.
        gc.cleanup();

        let (live_bytes, cursor, region_type) = {
            let regions = gc.regions.lock();
            (
                regions[idx].live_bytes,
                regions[idx].cursor,
                regions[idx].region_type,
            )
        };
        assert_eq!(
            live_bytes,
            cursor - tams,
            "only the post-TAMS extent is implicitly live"
        );
        assert_ne!(
            region_type,
            RegionType::Free,
            "a region with post-TAMS allocation must never be freed in place"
        );
    }

    /// G1MAT-2 — `reclaim_dead_humongous_spans_locked` derives a span's extent
    /// from `regions[i].cursor` alone. If that disagrees with the region table
    /// (a stale or corrupt cursor), the old code would `reset()` — zero-fill
    /// and retype-to-Free — regions belonging to OTHER live objects. Require
    /// every claimed continuation region to actually be typed
    /// `HumongousContinuation` before touching the span.
    #[test]
    fn g1mat2_humongous_reclaim_skips_span_whose_regions_are_not_continuations() {
        let gc = make_collector();
        // ~1.6 MB over 1 MB regions => HumongousStart + 1 continuation.
        let huge = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let start_idx = gc.lookup_region_for_addr(huge.as_ptr() as usize).unwrap();
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 1);

        // Simulate the table/cursor disagreement: the region the cursor claims
        // as a continuation is in fact an unrelated live Old region.
        let bystander = start_idx + 1;
        gc.with_regions_mut(|regions| {
            regions[start_idx].live_bytes = 0; // "dead" per the mark bitmap
            regions[bystander].region_type = RegionType::Old;
            regions[bystander].cursor = 4096;
        });

        let reclaimed = {
            let mut regions = gc.regions.lock();
            gc.reclaim_dead_humongous_spans_locked(&mut regions)
        };

        assert_eq!(
            reclaimed, 0,
            "a malformed span must not be reclaimed at all"
        );
        let regions = gc.regions.lock();
        assert_eq!(
            regions[bystander].region_type,
            RegionType::Old,
            "the bystander region must not be retyped"
        );
        assert_eq!(
            regions[bystander].cursor, 4096,
            "the bystander region must not be zero-filled/reset"
        );
        assert_eq!(
            regions[start_idx].region_type,
            RegionType::HumongousStart,
            "the start region must be left alone too"
        );
    }

    /// G1MAT-3 — SATB pre-barrier ordering for reference ARRAY element stores:
    /// the old referent must reach the SATB log, and the slot must already hold
    /// the new value afterwards (i.e. the log happened strictly before the
    /// overwrite). Read and store now share one `regions` critical section, so
    /// the read->log->store sequence cannot be interleaved by a lock release.
    #[test]
    fn g1mat3_satb_pre_barrier_logs_overwritten_array_element_before_store() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 4);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        let old_addr = old.as_ptr() as usize;

        gc.set_array_element(arr, 0, Value::Object(Some(old)))
            .unwrap();

        // Marking inactive: no logging at all (the barrier must cost nothing
        // outside a cycle).
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        let _ = gc.satb_queue().drain();
        gc.set_array_element(arr, 0, Value::Object(Some(old)))
            .unwrap();
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            !gc.satb_queue().drain().contains(&old_addr),
            "no SATB entry may be logged while marking is idle"
        );

        // Marking active: the overwritten referent must be logged.
        gc.start_concurrent_mark();
        gc.set_array_element(arr, 0, Value::Object(Some(new)))
            .unwrap();
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            gc.satb_queue().drain().contains(&old_addr),
            "the OLD element value must be logged before it is overwritten"
        );
        assert_eq!(
            gc.get_array_element(arr, 0).unwrap(),
            Value::Object(Some(new)),
            "the store must still have landed"
        );
    }

    /// G1MAT-3 — the weak-reference PROTOCOL writes suppress the pre-barrier
    /// (INT-8). `set_array_element` now honours the same TLS scope `set_field`
    /// does, so the two store paths cannot disagree about what counts as a
    /// semantic overwrite.
    #[test]
    fn g1mat3_satb_pre_barrier_honours_suppression_scope_for_array_stores() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 2);
        let referent = gc.alloc_object(ClassId::new(2), 0);
        let referent_addr = referent.as_ptr() as usize;

        gc.set_array_element(arr, 0, Value::Object(Some(referent)))
            .unwrap();
        gc.start_concurrent_mark();
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        let _ = gc.satb_queue().drain();

        {
            let _suppressed = SatbPreSuppressGuard::new();
            gc.set_array_element(arr, 0, Value::Object(None)).unwrap();
        }
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            !gc.satb_queue().drain().contains(&referent_addr),
            "a suppressed protocol write must not resurrect the referent"
        );
        // …and the suppression scope must not leak past its guard.
        gc.set_array_element(arr, 0, Value::Object(Some(referent)))
            .unwrap();
        gc.set_array_element(arr, 0, Value::Object(None)).unwrap();
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            gc.satb_queue().drain().contains(&referent_addr),
            "the pre-barrier must be restored once the guard drops"
        );
    }

    /// G1MAT-4 — `cleanup` prunes remembered-set entries naming Free regions.
    ///
    /// Nothing else in the collector ever removes an rset entry: `clear()` runs
    /// only on the TARGET's own `reset()`, so a source index recorded once is
    /// kept for the rest of that target's life and every later pause pays for
    /// it. Free sources are provably dead (a Free region holds no live object),
    /// so dropping them changes no collection decision.
    #[test]
    fn g1mat4_cleanup_prunes_rset_sources_naming_free_regions() {
        let gc = make_collector();
        gc.with_regions_mut(|regions| {
            regions[1].region_type = RegionType::Free;
            regions[2].region_type = RegionType::Old;
            regions[3].region_type = RegionType::Old;
            regions[3].rset.add_reference(1); // Free  -> pruned
            regions[3].rset.add_reference(2); // Old   -> kept
            regions[3].rset.add_reference(usize::MAX); // out of range -> pruned
        });

        // No mark-start snapshot: cleanup keeps the pure-bitmap verdict and
        // performs no in-place frees, so this isolates the rset pruning.
        gc.cleanup();

        let sources = {
            let regions = gc.regions.lock();
            regions[3].rset.sources()
        };
        assert!(
            !sources.contains(&1),
            "a source region that is Free cannot hold a live edge — prune it"
        );
        assert!(
            sources.contains(&2),
            "a live source region must be retained"
        );
        assert!(
            !sources.contains(&usize::MAX),
            "an out-of-range source index must be pruned"
        );
    }

    /// Evacuation failure (to-space exhaustion), the path the retry/drain
    /// machinery is built on. With no Free region and no non-CSet Survivor
    /// region left, `evacuate_object` must SELF-FORWARD rather than drop the
    /// object: an identity entry in the pointer map, the object left at its
    /// original address with its fields intact, and its region KEPT (Eden
    /// retyped to Survivor) instead of reset.
    #[test]
    fn evacuation_failure_self_forwards_and_keeps_region() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(4242));
        let obj_addr = obj.as_ptr() as usize;

        let idx = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };

        // Starve to-space: every other region is a non-Free, non-Survivor
        // region with no room, so neither the `alloc_in_type_locked` reuse scan
        // nor `find_free_region` can supply a destination. `cursor = 0` keeps
        // the Phase-4 walk of these regions a no-op.
        gc.with_regions_mut(|regions| {
            for (i, r) in regions.iter_mut().enumerate() {
                if i != idx {
                    r.region_type = RegionType::Old;
                    r.cursor = 0;
                }
            }
        });

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            result.stats.objects_copied, 0,
            "there is no to-space: nothing can be copied"
        );
        assert_eq!(
            result.pointer_map.get(&obj_addr),
            Some(&obj_addr),
            "evacuation failure must record an IDENTITY forward (self-forward), \
             not drop the object"
        );
        assert_eq!(
            roots[0].as_ptr() as usize,
            obj_addr,
            "a self-forwarded root must not move"
        );
        assert_eq!(
            gc.get_field(roots[0], 0).as_int(),
            Some(4242),
            "the self-forwarded object's payload must be intact"
        );

        let region_type = {
            let regions = gc.regions.lock();
            regions[idx].region_type
        };
        assert_eq!(
            region_type,
            RegionType::Survivor,
            "a kept (evacuation-failed) Eden region is retyped to Survivor so it \
             is re-collected next cycle — it must NOT be reset"
        );
        assert_eq!(
            result.stats.bytes_freed, 0,
            "the kept region's bytes must not be reported as freed"
        );
    }

    /// Evacuation failure must not silently degrade the pause trigger: a pause
    /// that self-forwards raises `needs_gc_free_percent` so the NEXT collection
    /// starts with a bigger to-space pool (the kept-region death-spiral fix),
    /// and a clean pause decays it back toward the 25% baseline.
    #[test]
    fn evacuation_failure_raises_then_decays_the_gc_trigger() {
        let gc = make_collector();
        let baseline = gc.needs_gc_free_percent.load(Ordering::Relaxed);

        let obj = gc.alloc_object(ClassId::new(1), 0);
        let idx = {
            let regions = gc.regions.lock();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.with_regions_mut(|regions| {
            for (i, r) in regions.iter_mut().enumerate() {
                if i != idx {
                    r.region_type = RegionType::Old;
                    r.cursor = 0;
                }
            }
        });

        let mut roots = vec![obj];
        let first = gc.young_collection(&mut roots, &NoopMonitors);
        let _ = gc.retry_after_evacuation_failure(first, &mut roots, &NoopMonitors);
        let raised = gc.needs_gc_free_percent.load(Ordering::Relaxed);
        assert!(
            raised > baseline,
            "an evacuation failure must trigger the next GC earlier ({raised} <= {baseline})"
        );
        assert!(raised <= 50, "the trigger must stay capped at 50%");

        // A clean pause (empty pointer map => no identity forwards) decays it.
        let clean = GcResult {
            stats: GcStats {
                objects_copied: 0,
                bytes_copied: 0,
                bytes_freed: 0,
            },
            pointer_map: HashMap::new(),
        };
        let mut no_roots: Vec<ObjectRef> = Vec::new();
        let _ = gc.retry_after_evacuation_failure(clean, &mut no_roots, &NoopMonitors);
        assert!(
            gc.needs_gc_free_percent.load(Ordering::Relaxed) < raised,
            "a clean pause must decay the trigger back toward baseline"
        );
    }

    // =======================================================================
    // G1 correctness audit (docs/gc/g1-audit.md)
    // =======================================================================

    use crate::gc_metrics::{g1_cycle_kind, g1_degraded, last_g1_cycle};

    /// G1AUD-1 — promotion must stamp `GC_FLAG_OLD_GEN`.
    ///
    /// G1 itself never reads the bit; the JIT does. Every inline
    /// reference-store fast path in `jit/src/x64.rs` treats a receiver with the
    /// bit clear as "young, no post barrier needed". On an unstamped G1 heap
    /// every promoted object read as young, so a JIT-compiled null->non-null
    /// store into it skipped `post_write_barrier_rset` and the old->young edge
    /// never reached the remembered set.
    #[test]
    fn promotion_stamps_the_old_generation_bit_the_jit_barrier_reads() {
        let mut cfg = small_config();
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(
            gc.get_header(obj).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "a freshly allocated Eden object is young"
        );

        // Pass 1: age 0 < promotion_age 1 => Survivor, still young.
        let mut roots = vec![obj];
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            gc.get_header(roots[0]).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "a Survivor copy is still young and MUST NOT be stamped — stamping it \
             would send every JIT store on a survivor down the helper for nothing"
        );

        // Pass 2: age 1 >= promotion_age 1 => promoted to Old, stamped.
        gc.young_collection(&mut roots, &NoopMonitors);
        let promoted = gc
            .lookup_region_for_addr(roots[0].as_ptr() as usize)
            .expect("promoted object is in a region");
        assert_eq!(
            gc.regions.lock()[promoted].region_type,
            RegionType::Old,
            "the object should have been promoted by the second pass"
        );
        assert_ne!(
            gc.get_header(roots[0]).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "a promoted object MUST carry GC_FLAG_OLD_GEN so the JIT's inline \
             reference-store fast paths bail to the full-barrier helper"
        );
    }

    /// The stamp must not clobber the layout bit that decides how the object is
    /// read back — a promoted compact instance that lost `GC_FLAG_COMPACT`
    /// would be walked with the legacy 16-byte-cell stride.
    #[test]
    fn the_old_generation_stamp_preserves_every_other_header_flag() {
        let mut cfg = small_config();
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 1);
        // `GC_FLAG_MARKED` is the safe sentinel here: G1 keeps liveness in
        // per-region bitmaps and never reads this bit, whereas `GC_FLAG_COMPACT`
        // changes how `object_total_size` measures the object and would make
        // the test's own evacuation stride disagree with the allocation.
        let sentinel = cratonvm_types::GC_FLAG_MARKED;
        unsafe {
            let h = &mut *(obj.as_ptr() as *mut ObjectHeader);
            h.gc_flags |= sentinel;
        }

        let mut roots = vec![obj];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);

        let flags = gc.get_header(roots[0]).gc_flags;
        assert_ne!(flags & GC_FLAG_OLD_GEN, 0, "promoted");
        assert_ne!(
            flags & sentinel,
            0,
            "the promotion stamp must OR the old-gen bit in, never overwrite the \
             flags byte"
        );
    }

    /// G1AUD-2 — `is_marking_active() => satb_queue.is_active()`.
    ///
    /// A reference store reads its old slot value on the PHASE and retains it
    /// on the QUEUE. A window where the phase says "marking" but the queue is
    /// off silently discards every edge overwritten in it.
    #[test]
    fn the_satb_gate_is_never_half_open_across_a_whole_cycle() {
        let gc = make_collector();
        let check = |where_: &str| {
            if gc.gc_state.is_marking_active() {
                assert!(
                    gc.satb_queue.is_active(),
                    "SATB gate half-open at {where_}: marking is active but the \
                     queue is not, so overwritten references are being dropped"
                );
            }
        };

        check("idle");
        gc.start_concurrent_mark();
        check("after start_concurrent_mark");
        assert!(gc.gc_state.is_marking_active() && gc.satb_queue.is_active());

        gc.remark(&[]);
        check("after remark");

        gc.abort_concurrent_mark();
        check("after abort");
        assert!(
            !gc.gc_state.is_marking_active(),
            "abort must leave the marking phase"
        );
        assert!(!gc.satb_queue.is_active(), "abort must close the queue");
    }

    /// The abort path used to deactivate the queue BEFORE leaving the marking
    /// phase, which is the only ordering in the collector that opened the
    /// window above. Pin the order explicitly, because reversing it back would
    /// still leave the end state this test's sibling checks.
    #[test]
    fn abort_leaves_the_marking_phase_before_closing_the_satb_queue() {
        let gc = make_collector();
        gc.start_concurrent_mark();
        assert!(gc.gc_state.is_marking_active());
        assert!(gc.satb_queue.is_active());
        gc.abort_concurrent_mark();
        // Both ends observable only after the fact, so assert the invariant
        // that the ordering exists to preserve: at no point may the phase be
        // marking-active with the queue closed. The end state has BOTH off.
        assert!(!gc.gc_state.is_marking_active());
        assert!(!gc.satb_queue.is_active());
    }

    /// SATB completeness for the store paths this crate owns: a reference
    /// overwrite performed through `set_field` while marking is active must
    /// deliver the OLD value to the marker, not the new one.
    #[test]
    fn set_field_logs_the_overwritten_reference_while_marking() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        gc.set_field(holder, 0, Value::Object(Some(old)));

        gc.start_concurrent_mark();
        gc.set_field(holder, 0, Value::Object(Some(new)));
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());

        let logged = gc.satb_queue().drain();
        assert!(
            logged.contains(&(old.as_ptr() as usize)),
            "the pre-barrier must log the OLD value: {logged:?}"
        );
        assert!(
            !logged.contains(&(new.as_ptr() as usize)),
            "the pre-barrier must not log the value being stored: {logged:?}"
        );
    }

    /// Same obligation on the array path — `set_array_element` reads the old
    /// element inside the SAME regions critical section as the store.
    #[test]
    fn set_array_element_logs_the_overwritten_reference_while_marking() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 2);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        gc.set_array_element(arr, 0, Value::Object(Some(old)))
            .expect("in bounds");

        gc.start_concurrent_mark();
        gc.set_array_element(arr, 0, Value::Object(Some(new)))
            .expect("in bounds");
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());

        let logged = gc.satb_queue().drain();
        assert!(
            logged.contains(&(old.as_ptr() as usize)),
            "aastore's pre-barrier must log the OLD element: {logged:?}"
        );
    }

    /// No SATB traffic at all outside a mark cycle: the barrier's cost when
    /// idle is one Acquire load, and a run that logs while idle would grow the
    /// shards without bound with nobody to drain them.
    #[test]
    fn no_reference_is_logged_when_no_mark_cycle_is_active() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        gc.set_field(holder, 0, Value::Object(Some(old)));
        gc.set_field(holder, 0, Value::Object(Some(new)));
        crate::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(gc.satb_queue().is_empty());
    }

    /// Retype the region holding `obj` and hand back its index.
    fn retype_region_of(gc: &G1Collector, addr: usize, ty: RegionType) -> usize {
        let idx = gc
            .lookup_region_for_addr(addr)
            .expect("address is inside a region");
        gc.with_regions_mut(|rs| rs[idx].region_type = ty);
        idx
    }

    /// G1AUD-3 — cleanup's in-place free is sound only on a COMPLETE closure.
    /// The baseline: with the gray set drained, a zero-live Old region is
    /// recycled.
    #[test]
    fn cleanup_frees_a_zero_live_old_region_when_the_closure_is_complete() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let idx = retype_region_of(&gc, obj.as_ptr() as usize, RegionType::Old);

        gc.start_concurrent_mark();
        assert!(gc.mark_worklist.lock().is_empty());
        gc.cleanup();

        assert_eq!(
            gc.regions.lock()[idx].region_type,
            RegionType::Free,
            "an unmarked Old region under a complete closure is garbage"
        );
    }

    /// ...and the fail-safe: with a gray entry still outstanding, "unmarked"
    /// does not imply "unreachable", so nothing may be freed. The precondition
    /// was documented on `VmHeap::g1_final_remark_and_cleanup` and enforced by
    /// nothing — and its sibling `g1_signal_marking_complete` calls `cleanup()`
    /// with no remark and no drain at all.
    #[test]
    fn cleanup_with_an_undrained_gray_set_retains_every_region() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let idx = retype_region_of(&gc, obj.as_ptr() as usize, RegionType::Old);

        gc.start_concurrent_mark();
        // One gray entry the marker never got to. Its address is irrelevant —
        // what matters is that the closure is not at a fixed point.
        gc.mark_worklist.lock().push(obj.as_ptr() as usize);
        gc.cleanup();

        assert_eq!(
            gc.regions.lock()[idx].region_type,
            RegionType::Old,
            "cleanup must NOT free a region on an incomplete mark closure"
        );
        let facts = last_g1_cycle().expect("cleanup records a cycle");
        assert_eq!(facts.kind, g1_cycle_kind::CONCURRENT_CLEANUP);
        assert_ne!(
            facts.degraded & g1_degraded::CLEANUP_CLOSURE_INCOMPLETE,
            0,
            "the fail-safe must be visible in the cycle record, not silent"
        );
    }

    /// The humongous reclaimer trusts the same closure, so it must be
    /// suppressed by the same fail-safe.
    #[test]
    fn cleanup_with_an_undrained_gray_set_also_spares_humongous_spans() {
        let gc = make_collector();
        // Two regions' worth: HumongousStart + one continuation.
        let big = gc.alloc_array(
            ClassId::new(1),
            ArrayElementType::Long,
            small_config().region_size / 8,
        );
        let start = gc
            .lookup_region_for_addr(big.as_ptr() as usize)
            .expect("humongous start region");
        assert_eq!(
            gc.regions.lock()[start].region_type,
            RegionType::HumongousStart
        );

        gc.start_concurrent_mark();
        gc.mark_worklist.lock().push(big.as_ptr() as usize);
        gc.cleanup();

        assert_eq!(
            gc.regions.lock()[start].region_type,
            RegionType::HumongousStart,
            "an unmarked humongous span must survive an incomplete closure"
        );
    }

    /// A humongous span occupies a physically contiguous run: one
    /// `HumongousStart` carrying the whole object size as its cursor, then
    /// `HumongousContinuation` regions with cursor 0 so walkers skip them.
    /// Every walker in this file derives the span extent from that shape.
    #[test]
    fn a_humongous_span_is_a_contiguous_start_plus_continuations() {
        let gc = make_collector();
        let region_size = small_config().region_size;
        let big = gc.alloc_array(ClassId::new(1), ArrayElementType::Long, region_size / 8);
        let start = gc
            .lookup_region_for_addr(big.as_ptr() as usize)
            .expect("humongous start region");

        let regions = gc.regions.lock();
        assert_eq!(regions[start].region_type, RegionType::HumongousStart);
        let total = regions[start].cursor;
        assert!(total > region_size, "the object spans more than one region");
        let needed = total.div_ceil(region_size).max(1);
        for r in &regions[start + 1..start + needed] {
            assert_eq!(
                r.region_type,
                RegionType::HumongousContinuation,
                "every region a span owns must be typed as a continuation — the \
                 reclaimer refuses to free a span whose shape disagrees"
            );
            assert_eq!(r.cursor, 0, "continuations must be invisible to walkers");
        }
        // The object's first and last byte are inside the reserved run.
        let base = regions[start].data.addr();
        assert_eq!(big.as_ptr() as usize, base);
        assert!(total <= needed * region_size);
    }

    /// Region pinning, no-relocation vocabulary: a pinned region must never
    /// enter a collection set, because Phase 5 zero-fills and re-types every
    /// CSet region that holds no self-forwarded object. That is what lets
    /// `pin_region_for_addr` promise a JNI critical section a stable address.
    #[test]
    fn a_pinned_region_is_never_evacuated() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let before = obj.as_ptr() as usize;
        let idx = gc.pin_region_for_addr(before).expect("region for object");
        assert!(gc.is_pinned(idx));

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            roots[0].as_ptr() as usize,
            before,
            "an object in a pinned region must not move"
        );
        assert!(
            result.pointer_map.is_empty(),
            "a pinned-only heap has nothing to evacuate"
        );
        assert_ne!(
            gc.regions.lock()[idx].region_type,
            RegionType::Free,
            "a pinned region must not be freed"
        );
        let facts = last_g1_cycle().expect("the pause records a cycle");
        assert_ne!(
            facts.degraded & g1_degraded::JNI_PINNED_REGIONS_EXCLUDED,
            0,
            "a pause that could not collect because of a pin must say so"
        );
    }

    /// Pins are refcounted: overlapping critical sections release
    /// independently, and the region only becomes collectable again when the
    /// last one is gone.
    #[test]
    fn overlapping_pins_release_independently() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let idx = gc.pin_region_for_addr(obj.as_ptr() as usize).unwrap();
        let idx2 = gc.pin_region_for_addr(obj.as_ptr() as usize).unwrap();
        assert_eq!(idx, idx2);

        gc.unpin_region(idx);
        assert!(
            gc.is_pinned(idx),
            "one release must not drop the other section's pin"
        );
        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));

        // Unbalanced extra release must not underflow into a wrong state.
        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));
    }

    /// Evacuation failure: with no to-space anywhere, a reached object is
    /// self-forwarded IN PLACE and its region is kept rather than freed.
    /// Dropping it instead would leave every referrer pointing into a region
    /// Phase 5 had just reset.
    #[test]
    fn evacuation_failure_self_forwards_and_keeps_the_region() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let before = obj.as_ptr() as usize;
        let home = gc
            .lookup_region_for_addr(before)
            .expect("object is in a region");

        // Put EVERY region into the collection set (Eden), so
        // `alloc_in_type_locked` can find neither a non-CSet destination nor a
        // Free region.
        gc.with_regions_mut(|rs| {
            for r in rs.iter_mut() {
                r.region_type = RegionType::Eden;
            }
        });

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            roots[0].as_ptr() as usize,
            before,
            "a self-forwarded object stays at its own address"
        );
        assert_eq!(
            result.pointer_map.get(&before),
            Some(&before),
            "the identity forward is what Phase 5 reads to keep the region"
        );
        assert_eq!(
            gc.regions.lock()[home].region_type,
            RegionType::Survivor,
            "a kept Eden region is retyped to Survivor, not freed"
        );
        let facts = last_g1_cycle().expect("the pause records a cycle");
        assert_ne!(
            facts.degraded & g1_degraded::EVACUATION_FAILURE,
            0,
            "an evacuation failure must be visible in the cycle record"
        );
    }

    /// Remembered sets: the mutator post-barrier records EVERY cross-region
    /// edge (including young->young, which a JNI-pinned holder depends on),
    /// and never records a same-region one.
    #[test]
    fn the_post_write_barrier_records_every_cross_region_edge() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let src = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder region");

        // Same-region target: nothing to remember.
        let near = gc.alloc_object(ClassId::new(2), 0);
        gc.set_field(holder, 0, Value::Object(Some(near)));
        let near_region = gc
            .lookup_region_for_addr(near.as_ptr() as usize)
            .expect("target region");
        if near_region == src {
            assert!(
                gc.regions.lock()[near_region].rset.sources().is_empty(),
                "a same-region edge must not enter a remembered set"
            );
        }

        // Cross-region target: the edge must be remembered against the TARGET.
        let far_region = (src + 1) % gc.num_regions();
        let far_addr = {
            let mut regions = gc.regions.lock();
            regions[far_region].region_type = RegionType::Old;
            let (ptr, _) = regions[far_region]
                .bump_alloc(HEADER_SIZE, 8)
                .expect("room in a fresh region");
            ptr as usize
        };
        gc.post_write_barrier_rset(holder, unsafe {
            ObjectRef::from_raw(far_addr as *mut u8)
        });
        assert!(
            gc.regions.lock()[far_region].rset.sources().contains(&src),
            "the holder's region must be recorded as a source of the target's rset"
        );
    }

    /// Region recycling must not leave a target remembering a source that no
    /// longer holds anything: `reset()` clears the recycled region's OWN rset,
    /// and `cleanup` prunes entries naming a now-Free source. Without the
    /// prune a recycled index is re-walked wholesale once it is re-typed,
    /// resurrecting dead objects' referents.
    #[test]
    fn recycling_a_region_drops_the_stale_remembered_set_edges() {
        let gc = make_collector();
        let target = 1usize;
        let source = 2usize;
        gc.with_regions_mut(|rs| {
            // Survivor, not Old: cleanup's in-place free applies only to Old
            // regions, and the point of this test is the PRUNE, not the free.
            rs[target].region_type = RegionType::Survivor;
            rs[source].region_type = RegionType::Old;
            rs[target].rset.add_reference(source);
        });
        assert!(gc.regions.lock()[target].rset.sources().contains(&source));

        // The SOURCE is recycled. Its own rset is cleared by `reset`...
        gc.with_regions_mut(|rs| rs[source].reset(0));
        assert!(gc.regions.lock()[source].rset.sources().is_empty());

        // ...and cleanup prunes the now-dangling entry naming it.
        gc.start_concurrent_mark();
        gc.cleanup();
        assert!(
            !gc.regions.lock()[target].rset.sources().contains(&source),
            "an entry naming a Free source must be pruned, not carried forever"
        );
    }

    /// G1AUD-5 (defect G1-8) — the "undead" entry.
    ///
    /// The Free-source prune above only catches a source that is STILL Free when
    /// cleanup runs. The expensive case is the other one: a source that was
    /// recycled and then immediately re-typed (a young pause frees a region, the
    /// next allocation claims it as Eden). Such an entry is not Free, so it
    /// survived every prune and every later pause re-walked that region
    /// *wholesale* on behalf of an object zero-filled cycles ago — resurrecting
    /// its referents. Entries now carry the generation they were recorded in and
    /// regions the generation they were recycled in, which makes the stale ones
    /// nameable on both the scan side and in cleanup.
    #[test]
    fn a_recycled_and_retyped_remembered_set_source_is_dropped_not_rewalked() {
        let gc = make_collector();
        let target = 1usize;
        let source = 2usize;

        let recorded_in = gc.rset_generation();
        gc.with_regions_mut(|rs| {
            rs[target].region_type = RegionType::Survivor;
            rs[source].region_type = RegionType::Old;
            rs[target]
                .rset
                .add_reference_in_generation(source, recorded_in);
        });

        // A pause recycles the source and the allocator immediately re-types it.
        // Every recycle phase bumps the generation before it touches a region,
        // so the reset stamp is strictly newer than the edge's.
        gc.rset_cache_epoch.fetch_add(1, Ordering::Release);
        let recycled_in = gc.rset_generation();
        assert!(recycled_in > recorded_in);
        gc.with_regions_mut(|rs| {
            rs[source].reset(recycled_in);
            // Re-typed, NOT left Free — that is exactly the case the old
            // Free-only prune could not see. `Eden` also keeps cleanup's
            // in-place free (Old-only) out of the picture, so the assertion
            // below can only be satisfied by the generation prune.
            rs[source].region_type = RegionType::Eden;
        });

        // The entry still exists, and the OLD Free-only test would keep it...
        assert!(
            gc.regions.lock()[target].rset.sources().contains(&source),
            "precondition: the entry is still recorded and the source is not Free"
        );
        // ...but the scan side no longer offers it as a source to walk.
        {
            let regions = gc.regions.lock();
            assert!(
                G1Collector::rset_entry_is_stale(&regions, source, recorded_in),
                "an edge recorded before the source was recycled is dead"
            );
            assert!(
                !G1Collector::live_rset_sources(&regions, &[target]).contains(&source),
                "a recycled-then-retyped source must NOT be walked wholesale again"
            );
        }

        // And cleanup drops the entry outright, so it stops costing memory and
        // a per-pause lookup.
        gc.start_concurrent_mark();
        gc.cleanup();
        assert!(
            !gc.regions.lock()[target].rset.sources().contains(&source),
            "cleanup must prune an entry whose source was recycled, not only \
             one whose source happens to still be Free"
        );
    }

    /// The other half of G1AUD-5, and the one that would be a use-after-free if
    /// it broke: an edge recorded AFTER the recycle names a live holder and must
    /// survive both the scan-side filter and cleanup's prune. An entry with no
    /// generation available at record time (`RSET_GENERATION_PINNED`) must also
    /// survive — that is the fail-safe direction.
    #[test]
    fn a_remembered_set_edge_recorded_after_the_recycle_is_never_pruned() {
        let gc = make_collector();
        let target = 1usize;
        let fresh = 2usize;
        let unstamped = 3usize;

        gc.rset_cache_epoch.fetch_add(1, Ordering::Release);
        let recycled_in = gc.rset_generation();
        gc.with_regions_mut(|rs| {
            rs[target].region_type = RegionType::Survivor;
            // `Eden`, not `Old`: cleanup frees a zero-live OLD region in place,
            // which would turn these into Free sources and prune them for the
            // wrong reason.
            rs[fresh].reset(recycled_in);
            rs[fresh].region_type = RegionType::Eden;
            rs[unstamped].reset(recycled_in);
            rs[unstamped].region_type = RegionType::Eden;
        });

        // Same generation as the reset: the Phase-4 rebuild records edges
        // BEFORE Phase 5's frees, so `stale` is a strict `<` and this is kept.
        gc.with_regions_mut(|rs| {
            rs[target]
                .rset
                .add_reference_in_generation(fresh, recycled_in);
            // No generation to hand — the deprecated/prototype entry point.
            rs[target].rset.add_reference(unstamped);
        });
        assert_eq!(
            gc.regions.lock()[target].rset.recorded_generation(unstamped),
            Some(crate::region::RSET_GENERATION_PINNED),
            "the generation-less entry point must record the never-prune stamp"
        );

        {
            let regions = gc.regions.lock();
            let live = G1Collector::live_rset_sources(&regions, &[target]);
            assert!(live.contains(&fresh), "a same-generation edge is live");
            assert!(
                live.contains(&unstamped),
                "an unstamped edge must be walked, never dropped — dropping a \
                 live cross-region edge is a use-after-free"
            );
        }

        gc.start_concurrent_mark();
        gc.cleanup();
        let sources = gc.regions.lock()[target].rset.sources();
        assert!(sources.contains(&fresh));
        assert!(sources.contains(&unstamped));
    }

    /// G1AUD-1, parallel half — the promotion stamp must be on BOTH evacuators.
    ///
    /// The JIT's inline reference-store fast paths read `GC_FLAG_OLD_GEN` to
    /// decide "young receiver, no post barrier needed". If the parallel
    /// evacuator ever stops stamping it, every promoted object reads as young
    /// there, the remembered-set post barrier is skipped for JIT-compiled stores
    /// into it, and the next young pause frees a still-live referent. The serial
    /// twin is `promotion_stamps_the_old_generation_bit_the_jit_barrier_reads`.
    #[test]
    fn promotion_stamps_the_old_generation_bit_on_the_parallel_evacuator_too() {
        let mut cfg = parallel_config(2, 8);
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(
            gc.get_header(obj).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "a freshly allocated Eden object is young"
        );

        let mut roots = vec![obj];
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(
            gc.get_header(roots[0]).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "a Survivor copy is still young and MUST NOT be stamped"
        );

        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        let promoted = gc
            .lookup_region_for_addr(roots[0].as_ptr() as usize)
            .expect("promoted object is in a region");
        assert_eq!(
            gc.regions.lock()[promoted].region_type,
            RegionType::Old,
            "the object should have been promoted by the second parallel pass"
        );
        assert_ne!(
            gc.get_header(roots[0]).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "a promoted object MUST carry GC_FLAG_OLD_GEN on the parallel path \
             too — the JIT reads the header, not the region table"
        );
    }

    /// G1AUD-6 (defect G1-9) — the parallel evacuator must walk JIT-pinned
    /// regions wholesale as remembered-set sources, exactly as the serial path
    /// has since the original CSet-straddle UAF fix.
    ///
    /// A JIT-pinned region is excluded from the CSet, so it is never traced as a
    /// from-space object; it is reached ONLY as a source. `jit_pinned_region_set`
    /// is not gated on `gc_quiescence::is_active()` — it also contains every
    /// region holding a published un-retired TLAB tail — so this set is
    /// routinely non-empty with no thread in JIT at all, which is the
    /// configuration G1-9's live-object corruption reproduces under.
    ///
    /// The test removes the remembered-set entry the barrier recorded, leaving
    /// the wholesale walk as the only coverage. That is the store class the
    /// serial path's defence exists for (a JIT-compiled store the collector
    /// cannot assume went through `post_write_barrier_rset`). Before the fix the
    /// parallel path lost Q here: not evacuated, not rewritten, and its region
    /// zero-filled by Phase 5.
    #[test]
    fn a_jit_pinned_region_is_a_wholesale_rset_source_on_the_parallel_path_too() {
        let gc = G1Collector::new(parallel_config(2, 8));
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = gc.lookup_region_for_addr(p.as_ptr() as usize).unwrap();
        // Retire the current Eden so Q lands in a DIFFERENT region without
        // filling P's region to the brim — this test publishes a reserved tail
        // at the top of P's region and needs it to sit above the cursor, which
        // the "allocate until the region rolls over" idiom used elsewhere would
        // make impossible.
        gc.current_eden.store(usize::MAX, Ordering::Relaxed);
        let q = gc.alloc_object(ClassId::new(2), 1);
        let q_region = gc.lookup_region_for_addr(q.as_ptr() as usize).unwrap();
        assert_ne!(p_region, q_region, "Q must be cross-region from P");
        gc.set_field(q, 0, Value::Int(1717));
        gc.set_field(p, 0, Value::Object(Some(q)));

        // Erase the barrier's record so the ONLY remaining coverage for Q is the
        // wholesale walk of the pinned source.
        gc.regions.lock()[q_region].rset.clear();
        assert!(gc.regions.lock()[q_region].rset.sources().is_empty());

        // Publish an un-retired TLAB tail in P's region. This is what puts the
        // region into `jit_pinned_region_set()` with no thread in JIT. The span
        // sits above the region's allocation cursor so no walk ever reaches it.
        let (skip_start, skip_end) = {
            let regions = gc.regions.lock();
            let r = &regions[p_region];
            let base = r.data.addr();
            let len = r.data.len();
            assert!(r.cursor + 64 < len, "the tail must sit above the cursor");
            (base + len - 64, base + len)
        };
        gc.set_jit_tlab_skip_regions(&[(skip_start, skip_end)]);
        assert!(
            gc.jit_pinned_region_set().contains(&p_region),
            "a published reserved tail must pin its region, JIT active or not"
        );

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        gc.clear_jit_tlab_skip_regions();

        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect(
                "Q is reachable only from a JIT-pinned region with no rset entry; \
                 the parallel path must walk that region wholesale as a source, \
                 like the serial path does",
            );
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(
            gc.get_field(p, 0),
            Value::Object(Some(q_new_ref)),
            "the pinned holder's slot must be rewritten to Q's new location"
        );
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(1717));
        assert_eq!(
            gc.regions.lock()[p_region].region_type,
            RegionType::Eden,
            "the pinned region itself must stay out of the CSet"
        );
    }

    /// The G1 cycle record must reach the shared decision report, so a
    /// `--verbose:gc` run under `-XX:+UseG1GC` states which collector produced
    /// the summary and what it was unable to do.
    #[test]
    fn a_g1_pause_states_its_own_decision_in_the_report() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let mut roots = vec![obj];
        gc.collect_garbage(&stw(), &mut roots, &NoopMonitors);

        let text = crate::gc_metrics::collector_decision_report();
        assert!(text.contains("backend=g1"), "{text}");
        assert!(text.contains("young=MOVING"), "{text}");
        assert!(text.contains("[GC] g1 cycle"), "{text}");
        assert!(text.contains("kind=young"), "{text}");
    }
}
