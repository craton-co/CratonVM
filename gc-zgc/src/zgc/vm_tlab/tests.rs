use super::*;
use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_gc_core::heap::{HEADER_SIZE, SLOT_SIZE};
use cratonvm_types::{ObjectRef, Value};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

/// zs s2/nd2 (zr4ab item 4): the share count's row is per heap -- a thread
/// alternating heaps A, B, A finds A's epoch again (no re-count); the
/// single-slot kill arm is the positive control (A's epoch is lost to B);
/// a dropped heap's row is pruned.
#[test]
fn the_share_count_row_is_kept_per_heap() {
    let (a, b) = (std::sync::Arc::new(11u64), std::sync::Arc::new(22u64));
    let mut rows = Vec::new();
    // A counts at epoch 7, then B at epoch 3.
    let mut seen = u64::MAX;
    vm_tlab_slot_count(&mut rows, &a, 11, true, |e| {
        seen = e;
        7
    });
    assert_eq!(seen, 0);
    vm_tlab_slot_count(&mut rows, &b, 22, true, |e| {
        seen = e;
        3
    });
    assert_eq!(seen, 0, "B takes its own row, A's survives");
    vm_tlab_slot_count(&mut rows, &a, 11, true, |e| {
        seen = e;
        e
    });
    assert_eq!(seen, 7);
    vm_tlab_slot_count(&mut rows, &b, 22, true, |e| {
        seen = e;
        e
    });
    assert_eq!(seen, 3);
    // Kill arm: one slot, B's count dropped A's row.
    vm_tlab_slot_count(&mut rows, &b, 22, false, |e| e);
    vm_tlab_slot_count(&mut rows, &a, 11, false, |e| {
        seen = e;
        e
    });
    assert_eq!(seen, 0);
    // A dropped heap's row is pruned.
    drop(b);
    vm_tlab_slot_count(&mut rows, &a, 11, true, |e| e);
    assert_eq!(rows.len(), 1);
}

/// Lay an object out in a VM-side `Tlab` the way `tlab_alloc_shaped_inner`
/// does: bump, write a legacy header, then `note_tlab_object`.
fn tlab_new_object(
    heap: &ZgcRealHeap,
    tlab: &mut cratonvm_gc_core::tlab::Tlab,
    class_id: ClassId,
    num_fields: usize,
) -> Option<ObjectRef> {
    use cratonvm_gc_core::heap::{ArrayElementType, ObjectHeader, ObjectKind};
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

/// zr z4/ab (`common-watched-referent-set-is-per-thread-not-per-heap-20261012.md` §1): one thread
/// refilling from heap A, then heap B, then A again gives A's first chunk
/// back, and retiring A's last chunk returns A's `reserved_bytes` to where
/// it started. Before the per-heap record B's refill overwrote A's, so
/// A's second refill released nothing and A kept the first chunk charged
/// for good. The kill-switch arm (`per_heap = false`, the single slot) is
/// the positive control: it must leak exactly one chunk.
#[test]
fn one_thread_refilling_from_two_heaps_gives_each_its_reservation_back() {
    fn run(per_heap: bool) -> (usize, usize) {
        let a = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        let b = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        a.set_vm_tlab_enabled(true);
        b.set_vm_tlab_enabled(true);
        let a_before = a.tlabs.reserved_bytes.load(Ordering::Relaxed);
        let refill = |h: &ZgcRealHeap| -> (*mut u8, usize) {
            // The production refill, with the record written under the arm
            // being tested (the cached switch cannot be flipped per test).
            let (ptr, size) = h.refill_tlab(64 * 1024).expect("chunk");
            take_vm_tlab_charge(h.tlabs.id());
            set_vm_tlab_charge(
                &h.tlabs,
                VmTlabCharge {
                    id: h.tlabs.id(),
                    lo: ptr as usize,
                    hi: ptr as usize + size,
                    charged_at: h.gc_count(),
                },
                per_heap,
            );
            (ptr, size)
        };
        let _a1 = refill(&a);
        let (b_ptr, b_size) = refill(&b);
        let (a_ptr, a_size) = refill(&a);
        let a_mid = a.tlabs.reserved_bytes.load(Ordering::Relaxed);
        let mut ta = unsafe { cratonvm_gc_core::tlab::Tlab::new(a_ptr, a_size) };
        ta.retire();
        let mut tb = unsafe { cratonvm_gc_core::tlab::Tlab::new(b_ptr, b_size) };
        tb.retire();
        let a_after = a.tlabs.reserved_bytes.load(Ordering::Relaxed);
        assert!(a_mid >= a_size);
        (a_after - a_before, a_size)
    }
    let (leak_on, _) = run(true);
    assert_eq!(
        leak_on, 0,
        "per-heap record: A's reservation comes back whole"
    );
    let (leak_off, a_chunk) = run(false);
    assert!(
        leak_off > 0 && leak_off <= a_chunk * 2,
        "single slot (kill switch): B's refill overwrote A's record, so A leaks a chunk \
         (leaked {leak_off})"
    );
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
    assert!(size >= cratonvm_gc_core::tlab::min_tlab_size() && size <= 64 * 1024);
    assert_eq!(size % ZGC_TLAB_ALIGN, 0);
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size) };
    assert!(bytes.iter().all(|b| *b == 0), "chunk must be zeroed");
    let (refills, refill_bytes, _, _) = heap.vm_tlab_engagement();
    assert_eq!((refills, refill_bytes), (1, size));
}

/// Wave 9, lane gcI (sizer9's ZGC half): a floored refill hands out at
/// least the floor even when the request is smaller, and a floor above
/// the heap's chunk ceiling is declined without carving (no refill
/// booked). A 16 MiB arena's ceiling is 16 KiB
/// (`chunk_bytes_for_capacity`: capacity / 1024, at least
/// `min_tlab_size()` = 8 KiB), so the served floor sits between the
/// 8 KiB request and that ceiling, and a 20 KiB floor (an object no chunk
/// of this heap could hold) is a decline case.
#[test]
fn gc21i_a_floored_refill_never_carves_less_than_the_floor() {
    let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
    heap.set_vm_tlab_enabled(true);
    let floor = 12 * 1024 + 8;
    let (_, size) = heap
        .refill_tlab_at_least(8 * 1024, floor)
        .expect("a fresh 16 MiB heap must serve a floor below its 16 KiB chunk ceiling");
    assert!(size >= floor, "chunk {size} below the floor {floor}");
    assert_eq!(size % ZGC_TLAB_ALIGN, 0);
    let (refills, _, _, _) = heap.vm_tlab_engagement();
    assert_eq!(refills, 1);
    for above in [20 * 1024 + 8, 64 * 1024 * 1024] {
        assert!(
            heap.refill_tlab_at_least(8 * 1024, above).is_none(),
            "a floor of {above} above the chunk ceiling must decline"
        );
    }
    let (refills, _, _, _) = heap.vm_tlab_engagement();
    assert_eq!(refills, 1, "the declined refills carved nothing");
}

/// zt t1/sp (`zgc-tlab-refill-ignores-the-object-floor-FIXED-20261019.md`, its
/// "How to verify"): the RECYCLED-block arm of the carve honours the
/// floor too. A spent arena whose only free block is 12 KiB answers a
/// floor no chunk of this heap can hold (20 KiB over a 16 KiB ceiling)
/// with `None` and books nothing, and a 4 KiB floor with that block.
#[test]
fn zt1sp_a_floored_refill_takes_a_recycled_block_only_above_the_floor() {
    let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
    heap.set_vm_tlab_enabled(true);
    let (base, hole) = {
        let mut arena = heap.arena.lock();
        let base = arena.base_ptr() as usize;
        let room = arena.capacity() - arena.used();
        let p = arena.alloc(room, 8).expect("the whole arena") as usize - base;
        arena.add_free_block(p + 4096, 12 * 1024);
        (base, p + 4096)
    };
    assert!(heap.refill_tlab_at_least(32 * 1024, 20 * 1024).is_none());
    let (refills, _, _, _) = heap.vm_tlab_engagement();
    assert_eq!(refills, 0, "the declined refill carved nothing");
    let (ptr, size) = heap
        .refill_tlab_at_least(32 * 1024, 4 * 1024)
        .expect("the 12 KiB block");
    assert_eq!((ptr as usize - base, size), (hole, 12 * 1024));
}

/// gci i48/zl3 (`zx48-collector-scope-audit.md` A8): a floored refill
/// below the 8 KiB chunk floor -- what the VM's refill site asks under a
/// typed `MinTLABSize` or `-XX:-UseTLAB` -- is carved as asked, never
/// below the object; a plain refill and an 8 KiB request keep the floor.
#[test]
fn gci_i48_zl3_a_floored_refill_below_the_chunk_floor_is_carved_as_asked() {
    let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
    heap.set_vm_tlab_enabled(true);
    // `-XX:MinTLABSize=4k`: the request is 4 KiB, the object 64 bytes.
    let (_, size) = heap.refill_tlab_at_least(4096, 64).expect("a 4 KiB chunk");
    assert_eq!(size, 4096, "a typed MinTLABSize below 8 KiB is honoured");
    // `-XX:-UseTLAB`: the request IS the object (40 bytes).
    let (_, size) = heap.refill_tlab_at_least(40, 40).expect("a 40 B chunk");
    assert_eq!(size, 40, "no buffer beyond the object");
    // Unfloored: the chunk floor stands, as before.
    let (_, size) = heap.refill_tlab(40).expect("a plain refill");
    assert_eq!(
        size,
        cratonvm_gc_core::tlab::min_tlab_size(),
        "plain refill keeps 8 KiB"
    );
    // The default sizing never asks below 8 KiB: unchanged.
    let (_, size) = heap
        .refill_tlab_at_least(cratonvm_gc_core::tlab::min_tlab_size(), 64)
        .expect("an 8 KiB chunk");
    assert_eq!(size, cratonvm_gc_core::tlab::min_tlab_size());
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

#[test]
fn retiring_the_vm_tlab_returns_its_tail_to_the_arena() {
    let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
    heap.set_vm_tlab_enabled(true);
    let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
    let mut tlab = unsafe { cratonvm_gc_core::tlab::Tlab::new(ptr, size) };
    let _ = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 4).expect("fits");
    let consumed = tlab.consumed_bytes();
    let free_before = heap.arena.lock().free_list_bytes();
    let allocated_before = heap.allocated.load(Ordering::Relaxed);
    tlab.retire();
    let tail = size - consumed;
    let free_after = heap.arena.lock().free_list_bytes();
    assert_eq!(
        free_after - free_before,
        tail,
        "the tail must be free-listed once"
    );
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
    let mut tlab = unsafe { cratonvm_gc_core::tlab::Tlab::new(ptr, 8192) };
    assert!(tlab.alloc(64, 8).is_some());
    tlab.retire();
    let header = unsafe { &*(ptr.add(64) as *const cratonvm_gc_core::heap::ObjectHeader) };
    assert_eq!(
        header.class_id,
        cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID,
        "a foreign tail must still get its filler"
    );
}

// The compaction-side obligation (a published tail is neither slid over
// nor reclaimed) is asserted in `zgc.rs`'s test module, beside the other
// relocation tests, because it needs that module's quiescence test lock:
// `a_published_vm_tlab_tail_is_neither_slid_over_nor_reclaimed`.

#[test]
fn owned_vm_tlab_chunk_epoch_hook_is_wired_and_invalidated_by_gc_and_retire() {
    use cratonvm_gc_core::tlab::{
        call_owned_vm_tlab_chunk_epoch, jit_zgc_announce_table_addr, Tlab, JIT_ZGC_ANNOUNCE,
    };

    let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
    heap.set_vm_tlab_enabled(true);

    let mut verified = false;
    for _ in 0..16 {
        let epoch_before = JIT_ZGC_ANNOUNCE.epoch.load(Ordering::Acquire);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let lo = ptr as usize;
        let hi = lo + size;
        let got = call_owned_vm_tlab_chunk_epoch(lo, hi);
        let epoch_after = JIT_ZGC_ANNOUNCE.epoch.load(Ordering::Acquire);
        if epoch_before != epoch_after {
            let mut tlab = unsafe { Tlab::new(ptr, size) };
            tlab.retire();
            continue;
        }
        assert_eq!(
            got,
            Some(epoch_before),
            "registered epoch hook must answer Some(epoch) for an active ZGC VM-TLAB chunk"
        );
        assert_eq!(
            call_owned_vm_tlab_chunk_epoch(lo, hi - 8),
            None,
            "mismatched chunk range must not match"
        );

        let mut tlab = unsafe { Tlab::new(ptr, size) };
        let epoch_tlab = JIT_ZGC_ANNOUNCE.epoch.load(Ordering::Acquire);
        if epoch_tlab == epoch_before
            && JIT_ZGC_ANNOUNCE.owner.load(Ordering::Acquire) == heap.arena_base
        {
            assert_eq!(
                tlab.zgc_inline_announce_state(),
                (epoch_before, jit_zgc_announce_table_addr())
            );
        }
        tlab.retire();
        assert_eq!(
            call_owned_vm_tlab_chunk_epoch(lo, hi),
            None,
            "retired chunk must clear its owned-chunk epoch record"
        );
        verified = true;
        break;
    }
    assert!(verified, "expected an uncontended epoch window");

    let (ptr2, size2) = heap.refill_tlab(64 * 1024).expect("chunk2");
    let lo2 = ptr2 as usize;
    let hi2 = lo2 + size2;
    heap.set_jit_tlab_skip_regions(&[(lo2, hi2)]);
    let stw = unsafe { StopTheWorldToken::new() };
    let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    heap.set_jit_tlab_skip_regions(&[]);
    assert_eq!(
        call_owned_vm_tlab_chunk_epoch(lo2, hi2),
        None,
        "ZGC GC cycle must bump epoch and invalidate owned-chunk hook"
    );
    let mut tlab2 = unsafe { Tlab::new(ptr2, size2) };
    tlab2.retire();
}
