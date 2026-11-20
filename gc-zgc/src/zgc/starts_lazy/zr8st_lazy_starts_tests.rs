use super::*;
use cratonvm_gc_core::heap::{ArrayElementType, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE};
use cratonvm_types::ClassId;

#[test]
fn the_mode_parses_like_its_doc() {
    assert_eq!(parse_lazy_tlab_starts_mode(None), ZLazyStartsMode::Off);
    for off in ["", "0", "off", "FALSE", " no "] {
        assert_eq!(
            parse_lazy_tlab_starts_mode(Some(off)),
            ZLazyStartsMode::Off,
            "{off:?}"
        );
    }
    for on in ["census", "1", "on", "materialize", "yes"] {
        assert_eq!(
            parse_lazy_tlab_starts_mode(Some(on)),
            ZLazyStartsMode::Census,
            "{on:?}"
        );
    }
}

/// A tiny deterministic generator (no `rand` dependency in this crate).
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Fill a VM-TLAB chunk the way the VM does (header, then
/// `note_tlab_object`, then the commit) with a random mix of legacy
/// instances and primitive / reference arrays, some at 16-byte alignment.
/// Returns the buffer and every base handed out.
struct Filled {
    tlab: cratonvm_gc_core::tlab::Tlab,
    bases: Vec<usize>,
    lo: usize,
    end: usize,
}

impl Filled {
    fn cursor(&self) -> usize {
        self.lo + self.tlab.consumed_bytes()
    }
}

fn fill_chunk(heap: &ZgcRealHeap, seed: u64) -> Filled {
    let (ptr, size) = heap.refill_tlab(64 * 1024).expect("a VM-TLAB chunk");
    // SAFETY: the chunk was just carved, zeroed, for this buffer.
    let mut tlab = unsafe { cratonvm_gc_core::tlab::Tlab::new(ptr, size) };
    let mut rng = XorShift(seed | 1);
    let mut bases = Vec::new();
    loop {
        let pick = rng.next() % 4;
        let (header, total) = match pick {
            0 | 1 => {
                let slots = (rng.next() % 6) as u32;
                let h = ObjectHeader::new(
                    ClassId::new(7),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    slots,
                );
                (h, HEADER_SIZE + slots as usize * cratonvm_gc_core::heap::SLOT_SIZE)
            }
            2 => {
                let len = (rng.next() % 40) as u32;
                let h = ObjectHeader::new(
                    ClassId::new(0),
                    ObjectKind::Array,
                    ArrayElementType::Byte,
                    len,
                    len,
                );
                let data =
                    cratonvm_gc_core::heap::array_data_size_checked(len as usize, ArrayElementType::Byte)
                        .unwrap();
                (h, ARRAY_DATA_OFFSET + data)
            }
            _ => {
                let len = (rng.next() % 12) as u32;
                let h = ObjectHeader::new(
                    ClassId::new(0),
                    ObjectKind::Array,
                    ArrayElementType::Int,
                    len,
                    len,
                );
                let data =
                    cratonvm_gc_core::heap::array_data_size_checked(len as usize, ArrayElementType::Int)
                        .unwrap();
                (h, ARRAY_DATA_OFFSET + data)
            }
        };
        let align = if rng.next() % 5 == 0 { 16 } else { 8 };
        let header = std::cell::Cell::new(Some(header));
        let got = tlab.alloc_initialized(total, align, |p| {
            // SAFETY: `p` is this buffer's fresh, unpublished reservation.
            unsafe { std::ptr::write(p as *mut ObjectHeader, header.take().unwrap()) };
            heap.note_tlab_object(p, total);
        });
        match got {
            Some(p) => bases.push(p as usize),
            None => break,
        }
    }
    Filled {
        tlab,
        bases,
        lo: ptr as usize,
        end: (ptr as usize + size) & !7,
    }
}

fn bases_in(heap: &ZgcRealHeap, lo: usize, hi: usize) -> Vec<usize> {
    (lo..hi)
        .step_by(8)
        .filter(|&a| heap.registry.contains(a))
        .collect()
}

#[test]
fn the_walk_finds_exactly_the_registered_bases_over_random_chunks() {
    for seed in [1u64, 2, 3, 0xDEAD_BEEF, 0x1234_5678_9ABC] {
        let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
        let f = fill_chunk(&heap, seed);
        let (lo, top, bases) = (f.lo, f.cursor(), f.bases.clone());
        assert!(bases.len() > 100, "seed {seed}: the chunk holds objects");
        assert_eq!(
            bases_in(&heap, lo, top),
            bases,
            "seed {seed}: the bit regime"
        );
        let mut walked = Vec::new();
        // SAFETY: the buffer is this thread's, quiescent, below its cursor.
        let complete = unsafe { ZgcRealHeap::walk_tlab_span(lo, top, |b| walked.push(b)) };
        assert!(complete, "seed {seed}: the walk parses the whole span");
        assert_eq!(
            walked, bases,
            "seed {seed}: the walk agrees with the registry"
        );
        // Every registered base is found from the floor, and no address
        // between two bases is.
        for (i, &b) in bases.iter().enumerate() {
            // SAFETY: as above.
            let (found, _) =
                unsafe { ZgcRealHeap::tlab_walk_finds(lo, b, usize::MAX) }.unwrap();
            assert!(found, "seed {seed}: base {i}");
            if b + 8 < top && !bases.contains(&(b + 8)) {
                // SAFETY: as above (a `false` is accepted).
                let (found, _) =
                    unsafe { ZgcRealHeap::tlab_walk_finds(lo, b + 8, usize::MAX) }.unwrap();
                assert!(!found, "seed {seed}: an interior word is not a base");
            }
        }
    }
}

/// `census` walks and counts but registers nothing; `off` never walks.
#[test]
fn census_counts_without_registering_and_off_is_inert() {
    cratonvm_gc_core::tlab::register_retired_span_listener();
    for mode in [ZLazyStartsMode::Census, ZLazyStartsMode::Off] {
        let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
        heap.counters.lazy_starts.set_mode_for_test(mode);
        let mut f = fill_chunk(&heap, 99);
        let bases = f.bases.clone();
        let dropped = bases[bases.len() / 2];
        heap.registry.remove(dropped);
        f.tlab.retire();
        let r = heap.lazy_tlab_starts_report();
        assert!(
            !heap.registry.contains(dropped),
            "{mode:?}: nothing registered"
        );
        if mode == ZLazyStartsMode::Census {
            assert_eq!(r.objects_walked, bases.len());
            assert_eq!(r.already_registered, bases.len() - 1);
            assert_eq!(r.unregistered_found, 1, "the positive control");
        } else {
            assert_eq!(
                r,
                ZLazyStartsReport {
                    mode: "off",
                    ..Default::default()
                }
            );
        }
    }
}

/// A published tail walks the span below its cursor once and moves the
/// floor, so the retire after it walks only what came later.
#[test]
fn a_published_tail_moves_the_floor() {
    let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
    heap.counters
        .lazy_starts
        .set_mode_for_test(ZLazyStartsMode::Census);
    let f = fill_chunk(&heap, 5);
    let bases = f.bases.clone();
    let s = f.cursor();
    let e = f.end;
    heap.lazy_note_published_tails(&[(s, e)]);
    let r = heap.lazy_tlab_starts_report();
    assert_eq!((r.stw_walks, r.objects_walked), (1, bases.len()));
    heap.lazy_note_published_tails(&[(s, e)]);
    assert_eq!(
        heap.lazy_tlab_starts_report().objects_walked,
        bases.len(),
        "nothing new"
    );
    heap.lazy_note_retired_span(f.lo, s, e);
    let r = heap.lazy_tlab_starts_report();
    assert_eq!((r.retire_walks, r.objects_walked), (1, bases.len()));
    assert_eq!(r.chunks_live, 0);
}


/// The oracle census: a hit inside this thread's live chunk counts as
/// `q_own`, its check walk agrees with the registry, and a hit outside
/// every live chunk is not counted.
#[test]
fn the_query_census_classifies_and_checks_hits() {
    let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
    heap.counters
        .lazy_starts
        .set_mode_for_test(ZLazyStartsMode::Census);
    let f = fill_chunk(&heap, 21);
    let bases = f.bases.clone();
    for &b in &bases {
        assert!(heap.is_object_address(b).is_some());
    }
    let r = heap.lazy_tlab_starts_report();
    assert_eq!(r.q_own + r.q_peer, bases.len());
    assert_eq!(r.q_mismatch, 0);
    assert!(
        r.q_checked > 0,
        "one hit in {Z_LAZY_CHECK_EVERY} is checked"
    );
    let outside = cratonvm_gc_core::collector::GarbageCollector::alloc_object(&*heap, ClassId::new(7), 1);
    assert!(heap.is_object_address(outside.as_ptr() as usize).is_some());
    let r2 = heap.lazy_tlab_starts_report();
    assert_eq!(
        r2.q_own + r2.q_peer,
        bases.len(),
        "a non-TLAB object is not counted"
    );
}

/// The cross-thread window: a peer queries bases the owner publishes
/// (header, start bit, commit, then a Release store of the base) while the
/// owner keeps bumping. Every read-only walk from the floor finds the
/// published base, whatever the owner is writing above it.
#[test]
fn a_peer_walk_finds_every_published_base_while_the_owner_bumps() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
    let published = Arc::new(AtomicUsize::new(0));
    let floor = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let owner = {
        let heap = Arc::clone(&heap);
        let published = Arc::clone(&published);
        let floor = Arc::clone(&floor);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let (ptr, size) = heap.refill_tlab(256 * 1024).expect("a VM-TLAB chunk");
            // SAFETY: freshly carved and zeroed for this buffer.
            let mut tlab = unsafe { cratonvm_gc_core::tlab::Tlab::new(ptr, size) };
            floor.store(ptr as usize, Ordering::Release);
            let mut rng = XorShift(77);
            let mut n = 0usize;
            loop {
                let len = (rng.next() % 6) as u32;
                let data =
                    cratonvm_gc_core::heap::array_data_size_checked(len as usize, ArrayElementType::Long)
                        .unwrap();
                let total = ARRAY_DATA_OFFSET + data;
                let got = tlab.alloc_initialized(total, 8, |p| {
                    let h = ObjectHeader::new(
                        ClassId::new(0),
                        ObjectKind::Array,
                        ArrayElementType::Long,
                        len,
                        len,
                    );
                    // SAFETY: fresh, unpublished reservation.
                    unsafe { std::ptr::write(p as *mut ObjectHeader, h) };
                    heap.note_tlab_object(p, total);
                });
                let Some(p) = got else { break };
                n += 1;
                if n % 3 == 0 {
                    published.store(p as usize, Ordering::Release);
                }
            }
            done.store(true, Ordering::Release);
            n
        })
    };
    let mut checked = 0usize;
    while !done.load(Ordering::Acquire) || checked == 0 {
        let p = published.load(Ordering::Acquire);
        let lo = floor.load(Ordering::Acquire);
        if p == 0 || lo == 0 {
            std::hint::spin_loop();
            // `p` may have been read before the owner's last publish and
            // `done`: re-read it after seeing `done`, or a loaded host
            // ends the loop having checked nothing.
            if done.load(Ordering::Acquire) && published.load(Ordering::Acquire) == 0 {
                break;
            }
            continue;
        }
        assert!(heap.registry.contains(p), "a published base is registered");
        // SAFETY: `p` is a published, registered base of the owner's
        // live chunk starting at `lo`.
        let (found, _) = unsafe { ZgcRealHeap::tlab_walk_finds(lo, p, usize::MAX) }.unwrap();
        assert!(found, "the peer walk finds the published base 0x{p:x}");
        checked += 1;
    }
    let n = owner.join().unwrap();
    assert!(n > 50, "the owner allocated ({n})");
    assert!(checked > 0, "the peer checked at least one base");
}
