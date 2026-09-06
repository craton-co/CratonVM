// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The VM thread's TLAB on the ZGC backend.
//!
//! # What this is
//!
//! Every mutator thread owns a [`crate::tlab::Tlab`] (`JvmThread::tlab`) that
//! the interpreter's `new` and the JIT's inline allocator bump into without a
//! call. `VmHeap::refill_tlab` is how that buffer gets a chunk, and until
//! 2026-09-02 its `Zgc` arm returned `None`: the thread's buffer stayed empty
//! for the life of the process, the inline bump missed every time, and every
//! compiled allocation took the `jit_new_object` helper into
//! [`super::ZgcRealHeap::alloc_tlab`] -- an `Arc<Mutex<_>>` buffer with about
//! six atomic operations per object. `tlab_hit_count` was zero on this backend
//! by construction.
//!
//! This module is the `Zgc` arm. It carves chunks from the same low-arena
//! source the heap's own buffers use ([`super::ZgcRealHeap::carve_tlab_chunk`]),
//! registers each object the VM allocates into one, takes the unused tail back
//! when the thread retires the buffer, and keeps a published un-retired tail
//! out of the compactor's way.
//!
//! # The four obligations, and where each is met
//!
//! 1. **Every object is in the start registry before anyone else can see
//!    it.** The registry is a mutator-path oracle on this backend
//!    (`is_object_address`, the SATB barrier, the conservative scans), not
//!    only a GC one. So registration is per object, at the moment the header
//!    is complete: the VM calls [`super::ZgcRealHeap::note_tlab_object`] from
//!    the header-initialising closure of its TLAB path and from
//!    `jit_post_tlab_init`, the helper the inline allocator always calls. The
//!    bytes an inline bump has reserved but not yet handed to that helper are
//!    invisible to the sweep (unregistered) and cannot be handed to anyone
//!    else (below the thread's cursor), which is the same window
//!    `alloc_tlab` has between its bump and its `register_allocations`.
//!
//! 2. **Allocate-black.** A chunk carved while a concurrent mark is running
//!    is blackened whole by the carve; an object allocated into a chunk that
//!    predates the mark start is blackened per object by `note_tlab_object`
//!    (`allocate_black_if_marking` is one bitmap test when marking and one
//!    flag load when not).
//!
//! 3. **The tail goes back.** `Tlab::retire` has no heap in scope, so this
//!    heap registers as a [`crate::tlab::TlabTailSink`] (in `new_shared`) and
//!    takes `[cursor, end)` straight into the arena free list, exactly as
//!    [`super::ZgcRealHeap::tlab_retire_locked`] does for the heap's own
//!    buffers. A sink declines a span outside its arena, so several heaps in
//!    one process (every unit test) cannot take each other's tails.
//!
//! 4. **An un-retired tail is never slid over.** A thread parked at a
//!    safepoint has retired (`safepoint_check`), and so has the collector's
//!    own thread (`maybe_gc`); a thread blocked in native or frozen in
//!    compiled code has not, and the STW protocol publishes those tails via
//!    `set_jit_tlab_skip_regions`. The sweep needs nothing (it walks the
//!    registry, and a tail holds no registered base), but the slide does:
//!    `relocate_stw` withholds every page a published tail touches from the
//!    relocation set and never lowers the bump cursor below a published
//!    tail's end, so `compact_low_to` cannot zero it or hand it out.
//!
//! # Accounting
//!
//! The chunk is charged to `allocated` in full at refill and the tail is
//! credited back at retire. That is the opposite of `alloc_tlab`'s per-object
//! charge (see its doc for why that path chose so), and it is chosen here
//! because the per-object charge is a `fetch_add` on one cache line shared by
//! every allocating thread -- the cost this module exists to remove from the
//! fast path. `allocated` is republished as the live byte count by every
//! collection, so the reservation error is bounded by `threads * chunk` and
//! never survives a cycle.
//!
//! # Switch
//!
//! OPT-IN: `CRATONVM_ZGC_JIT_TLAB=1` turns it on, and unset means
//! `refill_tlab -> None` exactly as before -- nothing else in this module then
//! runs, because no chunk is ever handed out. See
//! [`zgc_vm_tlab_enabled_by_default`] for the measurement that set that
//! default.

use std::sync::atomic::Ordering;

use cratonvm_types::ClassId;

use super::{zgc_corpse_enabled, ZgcRealHeap, ZGC_TLAB_ALIGN};

/// `CRATONVM_ZGC_JIT_TLAB`: hand the VM thread's TLAB a chunk on this backend.
///
/// **Default OFF, and the measurement is why.** The arm is correct — the
/// probes and both suites are green with it on — but on BinTreesClassic 16 at
/// `-Xmx512m`, release build, interleaved on a quiet host, it is *slower*
/// every round: 2841/3049/2746/2564 ms with it on against 2218/2188/2172/2007
/// ms with it off, identical checksums, and FEWER collections on the slow arm
/// (1 vs 2), so it is the allocation path itself and not collection frequency.
///
/// The cost is structural and is worth stating precisely, because it is what
/// the next attempt has to remove. This collector finds objects through an
/// allocation-base registry, so every inline allocation must be ANNOUNCED, and
/// the only existing announcement point is `jit_post_tlab_init` — a helper
/// that also mints an identity hash, looks up the class's compact layout and
/// dispatches primitive-field initialisation. The other two backends skip that
/// call entirely for a class that needs none of it (`skip_post_init_helper`),
/// so ZGC pays a call per allocation that they do not, and it costs more than
/// the `jit_new_object` preamble the inline bump saves.
///
/// The win needs a register-only helper in the JIT's ABI — one call that does
/// nothing but set the start bit — at which point this becomes a candidate for
/// default-on again, against this same measurement.
///
/// # It IS suite-clean, as of 2026-09-06 — 92/92, and the reason is one bug
///
/// This section used to read "it is not suite-clean either, and that is
/// UNTRIAGED", over five extra failures against the default arm
/// (`RJitMultiArrayClass`, `RArrayStoreLibrary`, `ROverlaySystemGcStress`,
/// `RSyncMethodJit`, `RVarHandleAccess`) and a note that five failures need
/// not be five defects. They were not. Three were closed by unrelated work
/// between 2026-09-02 and 2026-09-06; the last two were ONE defect, and it was
/// not in this module.
///
/// `jit_post_tlab_init` stamped a freshly minted identity hash at raw offset 8
/// of the object header, which was an `identity_hash_code: i32` field when
/// that store was written and has been the MARK WORD since 2026-08-07. The
/// hash went in unshifted, over the two-bit lock-state tag, so three objects
/// in four came back not-`MARK_NEUTRAL` — read afterwards as thin-locked, as
/// INFLATED (a monitor pointer synthesised from hash bits: the SIGSEGV) or as
/// FORWARDED (a relocation target synthesised the same way: the hang).
///
/// **The arm is what made that reachable, and nothing else can.** The inline
/// bump calls that helper only when `skip_helper` is false, and `skip_helper`
/// is `helper_is_noop && !jit_tlab_registration_required()` — registration is
/// required only here. Every other configuration either skips the helper or
/// never inlines, so the store was dead code everywhere else in the process.
/// It is the third defect this arm has exposed rather than caused; see the
/// two in the module doc's obligation 1.
///
/// `regression-suite/run.sh`, release binary, one run each, 2026-09-06:
/// unset **92/92**, `=1` **92/92**. The remaining reason this is opt-in is the
/// throughput measurement above, which is unchanged.
pub(crate) fn zgc_vm_tlab_enabled_by_default() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_JIT_TLAB") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "on" | "true" | "yes")
        }
        None => false,
    }
}

impl ZgcRealHeap {
    /// Whether [`Self::refill_tlab`] hands out chunks. Off by the kill switch,
    /// by `set_tlab_enabled(false)`, or on a heap with no `Arc` identity (no
    /// sink registered, so a retired tail would have nowhere to go).
    pub fn vm_tlab_enabled(&self) -> bool {
        self.counters.vm_tlab_enabled.load(Ordering::Relaxed)
            && self.tlab_enabled.load(Ordering::Relaxed)
            && self.self_weak.get().is_some()
    }

    /// Test and diagnostic override of the kill switch.
    pub fn set_vm_tlab_enabled(&self, on: bool) {
        self.counters.vm_tlab_enabled.store(on, Ordering::Relaxed);
    }

    /// The `Zgc` arm of `VmHeap::refill_tlab`: a zeroed chunk of at most
    /// `requested` bytes (never below the VM's minimum TLAB, never above the
    /// per-thread ceiling the heap's own buffers observe), or `None` when the
    /// arena cannot serve one -- the caller then allocates per object through
    /// `try_alloc_object_full`, which is the path every allocation took before.
    pub fn refill_tlab(&self, requested: usize) -> Option<(*mut u8, usize)> {
        if !self.vm_tlab_enabled() {
            return None;
        }
        let capacity = self.arena_end.saturating_sub(self.arena_base);
        let ceiling = self.tlabs.chunk_bytes_now(capacity);
        if ceiling == 0 {
            return None;
        }
        let floor = crate::tlab::min_tlab_size().min(ceiling);
        let want = requested.clamp(floor, ceiling) & !(ZGC_TLAB_ALIGN - 1);
        if want == 0 {
            return None;
        }
        // A recycled block shorter than `want` is accepted down to the VM's
        // minimum buffer: the VM retries the object that missed against the
        // new buffer and falls to the per-object path if it does not fit, so
        // a short chunk costs one miss, not a failure.
        let need = floor;
        let (ptr, size) = self.carve_tlab_chunk(want, need)?;
        let after = self.allocated.fetch_add(size, Ordering::Relaxed) + size;
        if after >= self.gc_threshold && after >= self.gc_rearm.load(Ordering::Relaxed) {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
        self.counters.vm_tlab_refills.fetch_add(1, Ordering::Relaxed);
        self.counters.vm_tlab_refill_bytes.fetch_add(size, Ordering::Relaxed);
        Some((ptr, size))
    }

    /// An object the VM just wrote a complete header for at `ptr`, inside a
    /// chunk this heap handed out through [`Self::refill_tlab`]. `footprint`
    /// is the bytes the buffer's cursor advanced by.
    ///
    /// Registers the base (obligation 1 in the module doc), notes the young
    /// grain when generational, and blackens the object when a mark is
    /// running. Does NOT charge `allocated`: the chunk was charged at refill.
    #[inline]
    pub fn note_tlab_object(&self, ptr: *mut u8, footprint: usize) {
        let addr = ptr as usize;
        if zgc_corpse_enabled() {
            self.audit_registry_insert(addr, footprint, "vm_tlab");
        }
        self.registry.insert(addr);
        self.note_young_page(addr);
        crate::gc_quiescence::note_allocated(std::slice::from_ref(&addr));
        self.allocate_black_if_marking(ptr);
    }


    /// The reserved tails of TLABs whose owners could not retire before this
    /// collection (blocked in native, or frozen in compiled code). Consumed
    /// by `relocate_stw`: see obligation 4 in the module doc. Replaces any
    /// previous list; spans outside the arena are dropped.
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        let mut kept = self.counters.jit_tlab_skip.lock();
        kept.clear();
        kept.extend(
            regions
                .iter()
                .copied()
                .filter(|(s, e)| e > s && *s >= self.arena_base && *e <= self.arena_end),
        );
    }

    /// Clear the list set by [`Self::set_jit_tlab_skip_regions`].
    pub fn clear_jit_tlab_skip_regions(&self) {
        self.counters.jit_tlab_skip.lock().clear();
    }

    /// Snapshot of the published tails, for the compactor.
    pub(crate) fn jit_tlab_skip_regions(&self) -> Vec<(usize, usize)> {
        self.counters.jit_tlab_skip.lock().clone()
    }

    /// `(refills, refill_bytes, tails_returned, tail_bytes_returned)` for the
    /// VM thread's TLAB on this backend. The first is the engagement counter:
    /// zero on a run that allocated means the switch is off or the arm is
    /// unreachable, and no throughput claim about it can stand.
    pub fn vm_tlab_engagement(&self) -> (usize, usize, usize, usize) {
        (
            self.counters.vm_tlab_refills.load(Ordering::Relaxed),
            self.counters.vm_tlab_refill_bytes.load(Ordering::Relaxed),
            self.counters.vm_tlab_tails_returned.load(Ordering::Relaxed),
            self.counters.vm_tlab_tail_bytes_returned.load(Ordering::Relaxed),
        )
    }

    /// The bump-cursor floor the published tails impose on a slide: the end
    /// of the highest published tail inside `[base, low_end)`, or `base`.
    pub(crate) fn jit_tlab_skip_floor(&self, base: usize, low_end: usize) -> usize {
        self.counters
            .jit_tlab_skip
            .lock()
            .iter()
            .filter(|(s, _)| *s >= base && *s < low_end)
            .map(|(_, e)| (*e).min(low_end))
            .max()
            .unwrap_or(base)
    }
}

/// `CRATONVM_ZGC_TLAB_TAIL_SINK`: take retired VM TLAB tails back into the
/// arena free list. `0`/`off`/`false`/`no` declines every tail, which leaves
/// the VM's filler object in place -- an A/B lever for the reclamation, at
/// the cost of the tail bytes until the next slide.
pub(crate) fn zgc_vm_tlab_tail_sink_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_TLAB_TAIL_SINK") {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    })
}

impl crate::tlab::TlabTailSink for ZgcRealHeap {
    fn reclaim_tlab_tail(&self, start: usize, end: usize) -> bool {
        if end <= start || start < self.arena_base || end > self.arena_end {
            return false;
        }
        if !zgc_vm_tlab_tail_sink_enabled() {
            return false;
        }
        if (start | end) & (ZGC_TLAB_ALIGN - 1) != 0 {
            return false;
        }
        let bytes = end - start;
        {
            let mut arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            arena.add_free_block(start - base, bytes);
        }
        // The chunk was charged whole at refill; the part that was never
        // handed out goes back. Saturating: a collection between the refill
        // and this retire has already republished `allocated` as live bytes.
        let _ = self
            .allocated
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |a| {
                Some(a.saturating_sub(bytes))
            });
        self.counters.vm_tlab_tails_returned.fetch_add(1, Ordering::Relaxed);
        self.counters.vm_tlab_tail_bytes_returned
            .fetch_add(bytes, Ordering::Relaxed);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
    use crate::heap::{HEADER_SIZE, SLOT_SIZE};
    use cratonvm_types::{ObjectRef, Value};

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
    }

    /// Lay an object out in a VM-side `Tlab` the way `tlab_alloc_shaped_inner`
    /// does: bump, write a legacy header, then `note_tlab_object`.
    fn tlab_new_object(
        heap: &ZgcRealHeap,
        tlab: &mut crate::tlab::Tlab,
        class_id: ClassId,
        num_fields: usize,
    ) -> Option<ObjectRef> {
        use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind};
        let total = HEADER_SIZE + num_fields * SLOT_SIZE;
        let ptr = tlab.alloc_initialized(total, 8, |ptr| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                num_fields as u32,
            );
            unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
            heap.note_tlab_object(ptr, total);
        })?;
        Some(unsafe { ObjectRef::from_raw(ptr) })
    }

    #[test]
    fn a_shared_heap_hands_the_vm_a_zeroed_chunk_inside_its_arena() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (ptr, size) = heap
            .refill_tlab(64 * 1024)
            .expect("a fresh 16 MiB heap must serve a 64 KiB chunk");
        let (lo, hi) = heap.conservative_addr_span().expect("arena span");
        let start = ptr as usize;
        assert!(start >= lo && start + size <= hi, "chunk outside the arena");
        assert!(size >= crate::tlab::min_tlab_size() && size <= 64 * 1024);
        assert_eq!(size % ZGC_TLAB_ALIGN, 0);
        let bytes = unsafe { std::slice::from_raw_parts(ptr, size) };
        assert!(bytes.iter().all(|b| *b == 0), "chunk must be zeroed");
        let (refills, refill_bytes, _, _) = heap.vm_tlab_engagement();
        assert_eq!((refills, refill_bytes), (1, size));
    }

    #[test]
    fn the_switch_and_a_heap_without_an_arc_identity_both_refuse() {
        let plain = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        plain.set_vm_tlab_enabled(true);
        assert!(
            plain.refill_tlab(64 * 1024).is_none(),
            "no sink can be registered for a heap that is not shared, so no chunk"
        );
        let shared = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        shared.set_vm_tlab_enabled(false);
        assert!(shared.refill_tlab(64 * 1024).is_none());
        shared.set_vm_tlab_enabled(true);
        assert!(shared.refill_tlab(64 * 1024).is_some());
    }

    /// OPT-IN, and the default is the measured one — see
    /// [`zgc_vm_tlab_enabled_by_default`] for the numbers. A future change
    /// that flips this has to move that comment too, which is the point.
    #[test]
    fn the_vm_tlab_is_opt_in_and_the_switch_reads_both_ways() {
        assert!(
            !zgc_vm_tlab_enabled_by_default(),
            "unset must mean OFF: the arm measured slower than the helper path"
        );
        for on in ["1", "on", "true", "yes"] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_JIT_TLAB", Some(on))],
                || assert!(zgc_vm_tlab_enabled_by_default(), "`{on}` must enable it"),
            );
        }
        for off in ["0", "off", "false", "no", ""] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_JIT_TLAB", Some(off))],
                || assert!(!zgc_vm_tlab_enabled_by_default(), "`{off}` must not"),
            );
        }
    }

    /// The seam that keeps the JIT's inline allocator announcing what it
    /// allocates. Without it every inline allocation is absent from the start
    /// registry, and `is_object_address` — a mutator-path oracle here, not
    /// just the sweep's — answers `None` for a perfectly live object.
    #[test]
    fn a_shared_heap_tells_the_jit_that_tlab_objects_must_be_announced() {
        cratonvm_types::set_jit_tlab_registration_required(false);
        let heap = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_JIT_TLAB", Some("1"))],
            || ZgcRealHeap::new_shared(16 * 1024 * 1024),
        );
        assert!(
            cratonvm_types::jit_tlab_registration_required(),
            "a ZGC heap that hands out VM TLABs must require the announcing call"
        );
        drop(heap);
        cratonvm_types::set_jit_tlab_registration_required(false);
    }

    #[test]
    fn objects_laid_out_in_the_vm_tlab_are_registered_and_collected_like_any_other() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        let kept = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 2).expect("fits");
        let dropped = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 2).expect("fits");
        heap.set_field(kept, 0, Value::Int(41));
        assert!(heap.is_object_address(kept.as_ptr() as usize).is_some());
        assert!(heap.is_object_address(dropped.as_ptr() as usize).is_some());
        let dropped_addr = dropped.as_ptr() as usize;
        tlab.retire();
        let stw = unsafe { StopTheWorldToken::new_unchecked() };
        let mut roots = vec![kept];
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("0"))],
            || {
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            },
        );
        assert_eq!(heap.get_field(roots[0], 0), Value::Int(41));
        assert!(
            heap.is_object_address(dropped_addr).is_none(),
            "an unrooted TLAB object must be swept"
        );
    }

    #[test]
    fn retiring_the_vm_tlab_returns_its_tail_to_the_arena() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        let _ = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 4).expect("fits");
        let consumed = tlab.consumed_bytes();
        let free_before = heap.arena.lock().free_list_bytes();
        let allocated_before = heap.allocated.load(Ordering::Relaxed);
        tlab.retire();
        let tail = size - consumed;
        let free_after = heap.arena.lock().free_list_bytes();
        assert_eq!(free_after - free_before, tail, "the tail must be free-listed once");
        assert_eq!(
            allocated_before - heap.allocated.load(Ordering::Relaxed),
            tail,
            "the tail's reservation must be credited back"
        );
        let (_, _, tails, tail_bytes) = heap.vm_tlab_engagement();
        assert_eq!((tails, tail_bytes), (1, tail));
        assert!(tlab.is_retired());
    }

    #[test]
    fn a_tail_outside_this_arena_is_declined_and_takes_the_filler_path() {
        let _heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        let mut backing = vec![0u64; 1024];
        let ptr = backing.as_mut_ptr() as *mut u8;
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, 8192) };
        assert!(tlab.alloc(64, 8).is_some());
        tlab.retire();
        let header = unsafe { &*(ptr.add(64) as *const crate::heap::ObjectHeader) };
        assert_eq!(
            header.class_id,
            crate::tlab::TLAB_FILLER_CLASS_ID,
            "a foreign tail must still get its filler"
        );
    }

    // The compaction-side obligation (a published tail is neither slid over
    // nor reclaimed) is asserted in `zgc.rs`'s test module, beside the other
    // relocation tests, because it needs that module's quiescence test lock:
    // `a_published_vm_tlab_tail_is_neither_slid_over_nor_reclaimed`.
}
