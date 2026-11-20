use super::*;
use cratonvm_gc_core::heap::HEADER_SIZE;
use crate::zgc::generation::{ZPromotionPolicy, Z_DEFAULT_PROMOTION_AGE};
use crate::zgc::page::ZPageConfig;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// A miniature page geometry — the same *shape* as OpenJDK's at 1/32 the
/// scale, so a test heap is 4 MiB and a Small page holds several chunks.
fn page_config() -> ZPageConfig {
    ZPageConfig {
        granule_size: 4096,
        small_page_size: 64 * 1024,
        medium_page_size: 256 * 1024,
        small_object_limit: 8 * 1024,
        medium_object_limit: 32 * 1024,
        max_capacity: 4 * 1024 * 1024,
    }
}

fn pages() -> Arc<ZPageAllocator> {
    Arc::new(ZPageAllocator::new(page_config()).expect("test geometry must validate"))
}

/// A TLAB policy scaled to the miniature pages. Note `min_chunk` is
/// `cratonvm_gc_core::tlab::min_tlab_size()` (8 KiB) because the shared adaptive sizer
/// clamps to that internally regardless of what we ask for — see
/// [`ZTlabConfig::min_chunk`].
fn tlab_config() -> ZTlabConfig {
    ZTlabConfig {
        initial_chunk: 16 * 1024,
        min_chunk: 8 * 1024,
        max_chunk: 32 * 1024,
        max_tlab_alloc: 1024,
        refill_waste_fraction: 64, // waste limit = 256 B on a 16 KiB chunk
        waste_increment: 0,        // isolate the threshold in the boundary test
        registry_batch: 512,
    }
}

/// Records everything the heap would have done, so a test can assert on it.
#[derive(Debug, Default)]
struct TestHooks {
    batches: Mutex<Vec<Vec<usize>>>,
    registered: Mutex<Vec<usize>>,
    registered_bytes: AtomicUsize,
    waste_bytes: AtomicUsize,
    hash: AtomicI32,
}

impl TestHooks {
    fn new() -> Self {
        Self {
            hash: AtomicI32::new(1),
            ..Self::default()
        }
    }

    fn batch_count(&self) -> usize {
        self.batches.lock().len()
    }

    fn registered_count(&self) -> usize {
        self.registered.lock().len()
    }

    fn bytes(&self) -> usize {
        self.registered_bytes.load(Ordering::Relaxed)
    }

    fn contains(&self, addr: usize) -> bool {
        self.registered.lock().iter().any(|a| *a == addr)
    }
}

impl ZTlabHeapHooks for TestHooks {
    fn register_allocations(&self, addrs: &[usize], bytes: usize) {
        self.batches.lock().push(addrs.to_vec());
        self.registered.lock().extend_from_slice(addrs);
        self.registered_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn allocation_color(&self) -> ZColor {
        // The quiescent good color; a real heap wires this to
        // `ZGoodMask::allocation_color()`.
        ZColor::Remapped
    }

    fn next_hash(&self) -> i32 {
        self.hash.fetch_add(1, Ordering::Relaxed)
    }

    fn note_waste(&self, bytes: usize) {
        self.waste_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// A distinctive class id for test objects. Not zero (which is a legal
/// class id and therefore indistinguishable from zeroed memory) and not
/// either filler sentinel.
const TEST_CID: u32 = 0x0BAD_0001;
/// Test object size: a multiple of the object grid and at least a header,
/// so a linear walk over them is well-formed whatever `HEADER_SIZE` is.
const OBJ: usize = 64;

/// Stamp a recognisable class id at offset 0, the one header-layout fact
/// `install_tail_filler` itself relies on.
fn stamp(addr: usize) {
    assert_eq!(addr % 8, 0, "object base must be on the 8-byte grid");
    unsafe { std::ptr::write(addr as *mut u32, TEST_CID) };
}

fn read_cid(addr: usize) -> u32 {
    unsafe { std::ptr::read(addr as *const u32) }
}

// -- fast path ----------------------------------------------------------

#[test]
fn bump_within_a_chunk_never_touches_shared_state() {
    let pages = pages();
    let hooks = TestHooks::new();
    let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Young);

    // One slow-path allocation to install a chunk.
    let first = tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    let page_used_after_refill = tlab.page().unwrap().used();
    let refills_after_first = tlab.stats().refills;

    // Everything from here is the pure bump.
    let mut last = first;
    for _ in 0..64 {
        let addr = tlab.alloc(OBJ, 8).expect("chunk has room");
        assert_eq!(addr % 8, 0, "bump must stay on the object grid");
        assert_eq!(addr, last + OBJ, "bump must be dense and increasing");
        last = addr;
    }

    // The proxy for "took no lock": no shared structure moved. The page's
    // CAS cursor is untouched (only the chunk reservation moves it), no
    // refill happened, and the registry was never handed anything.
    assert_eq!(tlab.page().unwrap().used(), page_used_after_refill);
    assert_eq!(tlab.stats().refills, refills_after_first);
    assert_eq!(hooks.batch_count(), 0, "the batch must not have flushed");
    assert_eq!(tlab.pending_registrations(), 65);
}

#[test]
fn inner_tlab_sits_at_offset_zero() {
    // The JIT's inline `new` reads `cursor` at +0 and `end` at +8 from a
    // TLAB pointer. Wrapping must not move them.
    let tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Young);
    let base = &tlab as *const ZTlab as usize;
    let inner = &tlab.inner as *const Tlab as usize;
    assert_eq!(inner - base, ZTlab::INNER_TLAB_OFFSET);
    assert_eq!(ZTlab::INNER_TLAB_OFFSET, 0);
    assert_eq!(Tlab::CURSOR_OFFSET, 0);
    assert_eq!(Tlab::END_OFFSET, 8);
}

// -- refill -------------------------------------------------------------

#[test]
fn refill_installs_a_fresh_chunk_on_exhaustion() {
    let pages = pages();
    let hooks = TestHooks::new();
    let cfg = tlab_config();
    let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);

    let per_chunk = cfg.initial_chunk / OBJ; // 256
    for _ in 0..per_chunk {
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    }
    assert_eq!(tlab.stats().refills, 1, "one chunk should have sufficed");
    assert_eq!(tlab.remaining(), 0, "the chunk should be exactly drained");
    let first_chunk = tlab.chunk().unwrap();

    // The next object cannot fit -> retire + refill.
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    assert_eq!(tlab.stats().refills, 2);
    let second_chunk = tlab.chunk().unwrap();
    assert_ne!(first_chunk, second_chunk);
    assert_eq!(
        second_chunk.0, first_chunk.1,
        "chunks must be contiguous within a page"
    );
    // Retiring the drained chunk flushed its batch.
    assert!(hooks.registered_count() >= per_chunk);
}

#[test]
fn first_refill_uses_the_configured_initial_chunk_not_the_sizer() {
    // A fresh `Tlab` reports zero elapsed time and zero consumption, which
    // the shared adaptive sizer reads as "filled instantly" and doubles.
    // The first refill must therefore bypass it.
    let pages = pages();
    let hooks = TestHooks::new();
    let cfg = tlab_config();
    let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    let (start, end) = tlab.chunk().unwrap();
    assert_eq!(end - start, cfg.initial_chunk);
}

// -- waste policy -------------------------------------------------------

#[test]
fn waste_policy_keeps_a_fat_remainder_and_discards_a_thin_one() {
    let pages = pages();
    let hooks = TestHooks::new();
    let cfg = tlab_config();
    // waste limit = 16384 / 64 = 256 bytes, ratchet disabled.
    let waste_limit = cfg.initial_chunk / cfg.refill_waste_fraction;
    assert_eq!(waste_limit, 256);

    let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();

    // Drain to a remainder of 320 bytes — ABOVE the limit, so the buffer is
    // worth keeping.
    while tlab.remaining() > 320 {
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    }
    assert_eq!(tlab.remaining(), 320);
    let refills_before = tlab.stats().refills;

    // A 512-byte object does not fit in 320 bytes.
    let addr = tlab.allocate(512, 8, &pages, &hooks).unwrap();
    assert_eq!(
        tlab.stats().refills,
        refills_before,
        "a remainder above the waste limit must NOT be thrown away"
    );
    assert_eq!(tlab.stats().direct_allocations, 1);
    assert_eq!(tlab.remaining(), 320, "the buffer must be untouched");
    assert!(
        hooks.contains(addr),
        "a direct allocation is registered immediately, not batched"
    );

    // Now drain to 64 bytes — BELOW the limit, so it is cheap to discard.
    while tlab.remaining() > 64 {
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    }
    assert_eq!(tlab.remaining(), 64);
    let refills_before = tlab.stats().refills;
    let direct_before = tlab.stats().direct_allocations;
    tlab.allocate(512, 8, &pages, &hooks).unwrap();
    assert_eq!(
        tlab.stats().refills,
        refills_before + 1,
        "a remainder at or below the waste limit must be retired"
    );
    assert_eq!(tlab.stats().direct_allocations, direct_before);
    assert_eq!(tlab.stats().waste_bytes, 64);
}

#[test]
fn waste_increment_stops_the_direct_path_ratcheting_forever() {
    let pages = pages();
    let hooks = TestHooks::new();
    let mut cfg = tlab_config();
    cfg.waste_increment = 512;
    // Raise the bypass threshold so a 2 KiB request stays a TLAB request
    // rather than becoming a large-object bypass.
    cfg.max_tlab_alloc = 4096;
    let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    while tlab.remaining() > 1024 {
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    }
    assert_eq!(tlab.remaining(), 1024);
    // First oversized request: remainder 1024 > limit 256 -> direct, and
    // the limit ratchets to 768.
    tlab.allocate(2048, 8, &pages, &hooks).unwrap();
    assert_eq!(tlab.stats().direct_allocations, 1);
    assert_eq!(tlab.stats().refills, 1);
    // Second: remainder is still 1024 > 768 -> direct again, limit -> 1280.
    // Third: 1024 <= 1280, so the buffer finally loses and is retired.
    tlab.allocate(2048, 8, &pages, &hooks).unwrap();
    tlab.allocate(2048, 8, &pages, &hooks).unwrap();
    assert!(
        tlab.stats().refills >= 2,
        "the ratchet must eventually retire a buffer the policy kept re-skipping \
         (refills={}, direct={})",
        tlab.stats().refills,
        tlab.stats().direct_allocations,
    );
}

// -- large bypass -------------------------------------------------------

#[test]
fn large_objects_bypass_the_tlab_entirely() {
    let pages = pages();
    let hooks = TestHooks::new();
    let cfg = tlab_config();
    let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    let remaining_before = tlab.remaining();
    let refills_before = tlab.stats().refills;

    // Above max_tlab_alloc (1 KiB) but still a Small object by page.rs's
    // routing (small_object_limit is 8 KiB) — the bypass decides whether to
    // carve a chunk, not which tier the object lands in.
    let small = tlab.allocate(4096, 8, &pages, &hooks).unwrap();
    // Above medium_object_limit (32 KiB) — its own Large page.
    let large = tlab.allocate(40_000, 8, &pages, &hooks).unwrap();

    assert_eq!(tlab.stats().large_allocations, 2);
    assert_eq!(
        tlab.remaining(),
        remaining_before,
        "the buffer is untouched"
    );
    assert_eq!(tlab.stats().refills, refills_before);
    assert!(pages.table().contains(small));
    assert!(pages.table().contains(large));
    assert!(hooks.contains(small) && hooks.contains(large));
    assert_eq!(
        pages.page_for(large).unwrap().size_class(),
        ZPageSizeClass::Large,
    );
}

// -- THE retirement test ------------------------------------------------

/// The most important test in the file: after `retire_all`, every page's
/// `[base, top)` decodes cleanly end to end with no zeroed hole.
///
/// The walk strides test objects by their known size and jumps a retired
/// chunk tail to the chunk's recorded end. It deliberately does **not**
/// decode the filler's own length: the only header-layout fact this file
/// may rely on is "class_id is a `u32` at offset 0", which is exactly what
/// `Tlab::install_tail_filler` relies on for its `GAP_FILLER` sentinel.
/// The header's field packing is mid-refactor; the sentinel is not.
#[test]
fn retire_all_leaves_every_page_top_consistent_and_walkable() {
    let pages = pages();
    let hooks = TestHooks::new();
    let cfg = tlab_config();
    let registry = ZTlabRegistry::new(cfg.clone(), ZTlabGeneration::Young);
    let cell = registry.attach();

    let per_chunk = cfg.initial_chunk / OBJ; // 256, an exact fit
    let mut chunks: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, objects)
    let mut current: Option<(usize, usize)> = None;
    let mut in_chunk = 0usize;

    for _ in 0..(per_chunk + 100) {
        let mut tlab = cell.lock();
        let addr = tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        stamp(addr);
        let chunk = tlab.chunk().expect("a bumped object lives in a chunk");
        if current != Some(chunk) {
            if let Some(prev) = current {
                chunks.push((prev.0, prev.1, in_chunk));
            }
            current = Some(chunk);
            in_chunk = 0;
        }
        in_chunk += 1;
    }
    if let Some(prev) = current {
        chunks.push((prev.0, prev.1, in_chunk));
    }
    assert_eq!(chunks.len(), 2, "expected exactly two chunks: {chunks:?}");

    // BEFORE retirement the hazard is real and observable.
    let tails = registry.reserved_tails();
    assert_eq!(tails.len(), 1, "the partially-filled chunk has a live tail");
    let (tail_lo, tail_hi) = tails[0];
    assert_eq!(read_cid(tail_lo), 0, "an un-retired tail is ZEROED memory");
    assert!(
        tail_lo < pages.page_for(tail_lo).unwrap().top_addr(),
        "and it sits BELOW the page's top, i.e. inside walk_bounds()"
    );
    assert!(tail_hi > tail_lo);

    let summary = registry.retire_all(&hooks);
    assert_eq!(summary.tlabs, 1);
    assert_eq!(summary.live_chunks, 1);
    assert!(summary.waste_bytes > 0);

    // Clause (a): no reserved tails remain.
    assert!(registry.reserved_tails().is_empty());
    // Clause (b): every object reached the registry.
    assert_eq!(hooks.registered_count(), per_chunk + 100);
    // Clause (c): no page is still Allocating for this thread.
    for page in pages.pages() {
        assert_ne!(
            page.state(),
            ZPageState::Allocating,
            "page {} was left Allocating after retire_all",
            page.id(),
        );
    }

    // The walk itself. One thread, one private page, no bypass, so the
    // whole heap is one page and the recorded chunks tile it exactly —
    // which is what lets the walk below carry `p` across chunk boundaries.
    assert_eq!(pages.pages().len(), 1, "this test assumes a single page");
    let mut walked_objects = 0usize;
    for page in pages.pages() {
        let (lo, hi) = page.walk_bounds();
        let mut p = lo;
        for &(cstart, cend, n) in chunks.iter() {
            if cstart < lo || cend > hi {
                continue;
            }
            assert_eq!(p, cstart, "walk must arrive exactly at the chunk base");
            for _ in 0..n {
                assert_eq!(
                    read_cid(p),
                    TEST_CID,
                    "walk desynced: {p:#x} is not a test object",
                );
                p += OBJ;
                walked_objects += 1;
            }
            if p < cend {
                let cid = read_cid(p);
                assert_ne!(
                    cid, 0,
                    "the chunk tail at {p:#x} is still ZERO — a top-bounded walk \
                     would decode it as a live `new Object()` and stride off grid",
                );
                assert!(
                    is_tlab_filler_class(cid),
                    "the chunk tail at {p:#x} must carry a filler sentinel, got {cid:#x}",
                );
                let tail = cend - p;
                if tail < HEADER_SIZE {
                    assert_eq!(cid, GAP_FILLER_CLASS_ID.as_u32());
                } else {
                    assert_eq!(cid, TLAB_FILLER_CLASS_ID.as_u32());
                }
                p = cend;
            }
        }
        assert_eq!(p, hi, "the walk must land exactly on the page's top");
    }
    assert_eq!(walked_objects, per_chunk + 100);
}

#[test]
fn retire_is_idempotent() {
    let pages = pages();
    let hooks = TestHooks::new();
    let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
    let cell = registry.attach();
    cell.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();

    registry.retire_all(&hooks);
    let after_first = cell.stats();
    registry.retire_all(&hooks);
    let after_second = cell.stats();
    assert_eq!(after_first.retires, after_second.retires);
    assert_eq!(after_first.waste_bytes, after_second.waste_bytes);
    assert_eq!(
        after_first.registered_objects,
        after_second.registered_objects
    );
    assert!(cell.lock().is_retired());
}

// -- registry batching --------------------------------------------------

#[test]
fn registry_inserts_are_batched_not_per_object() {
    let pages = pages();
    let hooks = TestHooks::new();
    let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
    let cell = registry.attach();

    for _ in 0..200 {
        let addr = cell.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
        stamp(addr);
    }
    // The whole point: 200 objects, zero registry mutex acquisitions so far.
    assert_eq!(hooks.batch_count(), 0);
    assert_eq!(hooks.registered_count(), 0);

    registry.retire_all(&hooks);

    assert_eq!(hooks.batch_count(), 1, "one hand-off for 200 objects");
    assert_eq!(hooks.registered_count(), 200);
    assert_eq!(hooks.bytes(), 200 * OBJ);
}

#[test]
fn a_full_batch_flushes_without_disturbing_the_buffer() {
    let pages = pages();
    let hooks = TestHooks::new();
    let mut cfg = tlab_config();
    cfg.registry_batch = 4;
    let mut tlab = ZTlab::new(cfg, ZTlabGeneration::Young);

    for _ in 0..20 {
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    }
    assert_eq!(hooks.registered_count(), 16, "4 full batches flushed");
    assert_eq!(tlab.pending_registrations(), 4);
    assert_eq!(tlab.stats().refills, 1, "flushing must not refill");
    assert_eq!(tlab.stats().fast_allocations, 20);
}

// -- adaptive sizing ----------------------------------------------------

#[test]
fn adaptive_sizing_grows_a_hot_thread_and_shrinks_a_cold_one() {
    let pages = pages();
    let hooks = TestHooks::new();
    let cfg = tlab_config();
    let mut tlab = ZTlab::new(cfg.clone(), ZTlabGeneration::Young);

    // GROW: drain the chunk completely. The shared sizer's "well used"
    // arm (>= 75% of the chunk consumed) fires, so this does not depend on
    // the sub-millisecond fill-time arm and survives a loaded host.
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    let first = tlab.chunk().unwrap();
    assert_eq!(first.1 - first.0, cfg.initial_chunk);
    while tlab.remaining() >= OBJ {
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    }
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap(); // forces the refill
    let second = tlab.chunk().unwrap();
    assert!(
        second.1 - second.0 > first.1 - first.0,
        "a drained buffer must grow: {} -> {}",
        first.1 - first.0,
        second.1 - second.0,
    );

    // SHRINK: barely touch the fresh chunk, then ask the sizer what it
    // would request next.
    //
    // Three conditions have to hold for the shrink arm and nothing else to
    // fire, and all three are properties of the ALLOCATION PATTERN, not of
    // the clock: (1) the elapsed window must exceed the sizer's 1 ms
    // "filled instantly" threshold — `sleep` never undersleeps, so 3 ms is
    // a lower bound, and this is a bound on the SIZE decision rather than a
    // wall-clock assertion; (2) the object must be at or below the sizer's
    // 256-byte "large allocation" mark, or its few-but-large grow arm
    // fires; (3) the chunk must stay far from drained, or its well-used
    // grow arm fires.
    std::thread::sleep(std::time::Duration::from_millis(3));
    let big = second.1 - second.0;
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    assert!(tlab.remaining() > big / 2, "the chunk must stay cold");
    let sized = tlab.next_refill_size_for_test();
    assert!(
        sized < big,
        "a barely-touched buffer must shrink: {big} -> {sized}",
    );
}

impl ZTlab {
    /// Test-only view of what the shared adaptive sizer would ask for next.
    /// Must be called before a retire — see `Tlab::next_refill_size`.
    fn next_refill_size_for_test(&mut self) -> usize {
        self.inner
            .next_refill_size()
            .clamp(self.config.min_chunk, self.config.max_chunk)
    }
}

// -- accounting ---------------------------------------------------------

#[test]
fn accounting_totals_match_the_sum_of_allocations() {
    let pages = pages();
    let hooks = TestHooks::new();
    let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
    let cell = registry.attach();

    let sizes: [usize; 6] = [OBJ, 128, 40, 512, 4096, 40_000];
    let mut expected = 0usize;
    for round in 0..40 {
        let size = sizes[round % sizes.len()];
        cell.lock().allocate(size, 8, &pages, &hooks).unwrap();
        expected += footprint_of(size, 8);
    }
    registry.retire_all(&hooks);

    let stats = registry.stats();
    assert_eq!(stats.total_allocations(), 40);
    assert_eq!(stats.total_bytes(), expected as u64);
    // Every byte the hooks were told about is an object byte; the tail
    // waste goes through `note_waste`, never through the registry.
    assert_eq!(hooks.bytes(), expected);
    assert_eq!(hooks.registered_count(), 40);
    assert_eq!(
        hooks.waste_bytes.load(Ordering::Relaxed) as u64,
        stats.waste_bytes,
    );
}

#[test]
fn detach_folds_a_dead_threads_stats_into_the_aggregate() {
    let pages = pages();
    let hooks = TestHooks::new();
    let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
    let cell = registry.attach_key(7);
    for _ in 0..10 {
        cell.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
    }
    assert_eq!(registry.len(), 1);
    registry.detach_key(7, &hooks);
    assert_eq!(registry.len(), 0);
    assert!(registry.is_empty());
    let stats = registry.stats();
    assert_eq!(stats.fast_allocations, 10);
    assert_eq!(hooks.registered_count(), 10);
}

// -- concurrency --------------------------------------------------------

/// The correctness test that matters: N threads bumping in parallel must
/// never hand out overlapping memory.
#[test]
fn concurrent_threads_produce_disjoint_address_ranges() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 150;

    let pages = pages();
    let hooks = Arc::new(TestHooks::new());
    let registry = Arc::new(ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let pages = Arc::clone(&pages);
        let hooks = Arc::clone(&hooks);
        let registry = Arc::clone(&registry);
        handles.push(std::thread::spawn(move || {
            let cell = registry.attach();
            let mut mine: Vec<usize> = Vec::with_capacity(PER_THREAD);
            for _ in 0..PER_THREAD {
                let addr = cell
                    .lock()
                    .allocate(OBJ, 8, &pages, &*hooks)
                    .expect("test heap is large enough");
                // Each thread stamps its own objects: if two threads ever
                // shared a span this would be a data race AND the range
                // check below would fail.
                stamp(addr);
                mine.push(addr);
            }
            mine
        }));
    }

    let mut all: Vec<usize> = Vec::new();
    for h in handles {
        all.extend(h.join().expect("worker must not panic"));
    }
    assert_eq!(all.len(), THREADS * PER_THREAD);
    assert_eq!(registry.len(), THREADS);

    // Set-disjointness, not timing: sort the ranges and prove no two touch.
    all.sort_unstable();
    for w in all.windows(2) {
        assert!(
            w[0] + OBJ <= w[1],
            "overlapping allocations: [{:#x}, {:#x}) and [{:#x}, ...)",
            w[0],
            w[0] + OBJ,
            w[1],
        );
    }
    for &addr in all.iter() {
        assert_eq!(
            read_cid(addr),
            TEST_CID,
            "object at {addr:#x} was clobbered"
        );
        assert!(pages.table().contains(addr));
    }

    registry.retire_all(&*hooks);
    assert!(registry.reserved_tails().is_empty());
    assert_eq!(hooks.registered_count(), THREADS * PER_THREAD);
    // One hand-off per thread's chunk, not one per object.
    assert!(
        hooks.batch_count() <= THREADS * 2,
        "registry batching regressed: {} hand-offs for {} objects",
        hooks.batch_count(),
        THREADS * PER_THREAD,
    );
}

#[test]
fn every_thread_gets_its_own_buffer_and_its_own_page() {
    let pages = pages();
    let hooks = TestHooks::new();
    let registry = ZTlabRegistry::new(tlab_config(), ZTlabGeneration::Young);
    let a = registry.attach_key(1);
    let b = registry.attach_key(2);
    assert!(!Arc::ptr_eq(&a, &b));
    a.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
    b.lock().allocate(OBJ, 8, &pages, &hooks).unwrap();
    let page_a = a.lock().page().map(|p| p.id());
    let page_b = b.lock().page().map(|p| p.id());
    assert!(page_a.is_some() && page_b.is_some());
    assert_ne!(page_a, page_b, "private pages must not be shared");
    // Re-attaching the same key returns the SAME buffer.
    assert!(Arc::ptr_eq(&a, &registry.attach_key(1)));
}

// -- the generation tag vs. ZPageReal::age (2026-08-07) ------------------

#[test]
fn tlab_refill_never_writes_the_pages_generational_age() {
    let pages = pages();
    let hooks = TestHooks::new();
    // `Old` is the arm that used to stamp `age = 1`. (`Young` stamped `0`,
    // which is what a fresh page already carries — the young arm was a
    // no-op and that is exactly why the collision went unnoticed.)
    let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Old);
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    let page = Arc::clone(tlab.page().expect("the first allocation refills"));
    assert_eq!(
        page.age(),
        0,
        "a page a TLAB just took has survived zero minor cycles — \
         ZPageReal::age is generation.rs's survival count, not a generation tag",
    );
    assert_eq!(
        tlab.page_generation(),
        Some(ZTlabGeneration::Old),
        "the generation tag belongs to the TLAB, which is the only thing that reads it",
    );

    // Drive the buffer until it abandons this page for a fresh one: neither
    // carving more chunks out of it nor releasing it may touch the age.
    let mut took_a_second_page = false;
    for _ in 0..4096 {
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let still_the_same_page = tlab.page().is_some_and(|held| Arc::ptr_eq(held, &page));
        if !still_the_same_page {
            took_a_second_page = true;
            break;
        }
    }
    assert!(
        took_a_second_page,
        "4096 x {} B must outlast a {} B page",
        OBJ,
        page_config().small_page_size,
    );
    assert_eq!(
        page.age(),
        0,
        "releasing a private page must leave its survival count alone",
    );
    assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Old));
}

/// The regression the audit's N6 describes, end to end: a page that is a
/// TLAB refill target **and** a young-generation survivor must promote on
/// the cycle `ZPromotionPolicy` says, not one early.
///
/// The aging step is `generation::sweep_young`'s, replayed verbatim
/// (`let age = page.age() + 1; page.set_age(age)`), because that is the
/// writer whose meaning `page.rs` endorses. Counted in cycles, not seconds.
#[test]
fn a_tlab_refill_target_still_promotes_at_the_right_age() {
    let pages = pages();
    let hooks = TestHooks::new();
    let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Old);
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    let page = Arc::clone(tlab.page().expect("the first allocation refills"));

    let policy = ZPromotionPolicy::default();
    let mut promoted_after: Option<u32> = None;
    for cycle in 1..=Z_DEFAULT_PROMOTION_AGE {
        // The buffer keeps bumping into the same page across the cycles.
        tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
        let age = page.age().saturating_add(1);
        page.set_age(age);
        if promoted_after.is_none() && policy.should_promote(age) {
            promoted_after = Some(cycle);
        }
    }
    assert_eq!(
        promoted_after,
        Some(Z_DEFAULT_PROMOTION_AGE),
        "promoted after {:?} minor cycles instead of {} — somebody other than \
         sweep_young wrote ZPageReal::age",
        promoted_after,
        Z_DEFAULT_PROMOTION_AGE,
    );
    assert_eq!(
        page.age(),
        Z_DEFAULT_PROMOTION_AGE,
        "the age must be exactly the number of cycles the page survived",
    );
}

#[test]
fn the_generation_tag_tracks_the_private_page_not_the_page_word() {
    let pages = pages();
    let hooks = TestHooks::new();
    let mut tlab = ZTlab::new(tlab_config(), ZTlabGeneration::Young);
    assert_eq!(tlab.page_generation(), None, "no page, no tag");
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    let page = Arc::clone(tlab.page().expect("the first allocation refills"));
    assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Young));

    tlab.retire(&hooks);
    assert!(tlab.page().is_none());
    assert_eq!(
        tlab.page_generation(),
        None,
        "the tag is released with the page, so it can never go stale",
    );
    assert_eq!(page.age(), 0, "retirement is not a minor cycle");

    // Retargeting a retired buffer takes effect on the next refill, and the
    // new page carries the new tag.
    assert!(tlab.set_generation(ZTlabGeneration::Old));
    assert_eq!(tlab.generation(), ZTlabGeneration::Old);
    tlab.allocate(OBJ, 8, &pages, &hooks).unwrap();
    assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Old));
    assert_eq!(
        tlab.page().expect("refilled").age(),
        0,
        "an old-generation TLAB may not hand its page any survival credit",
    );
}

// -- configuration ------------------------------------------------------

#[test]
fn normalized_config_keeps_a_tlab_eligible_object_fitting_a_chunk() {
    let cfg = ZTlabConfig {
        initial_chunk: 8 * 1024,
        min_chunk: 8 * 1024,
        max_chunk: 16 * 1024,
        // Absurd: bigger than any chunk we will ever hand out.
        max_tlab_alloc: 1024 * 1024,
        refill_waste_fraction: 0,
        waste_increment: 0,
        registry_batch: 0,
    }
    .normalized();
    assert_eq!(cfg.max_tlab_alloc, cfg.max_chunk);
    assert_eq!(cfg.refill_waste_fraction, 1, "a zero divisor is clamped");
    assert_eq!(cfg.registry_batch, 1, "a zero batch is clamped");
    assert_eq!(cfg.initial_chunk % ZPAGE_OBJECT_GRID, 0);
}

#[test]
fn default_config_tracks_the_shared_tlab_constants() {
    let cfg = ZTlabConfig::default().normalized();
    assert_eq!(cfg.initial_chunk, tlab::initial_refill_size());
    assert_eq!(cfg.min_chunk, tlab::min_tlab_size());
    assert_eq!(cfg.max_chunk, tlab::max_tlab_size());
    assert_eq!(cfg.max_tlab_alloc, tlab::tlab_max_alloc());
    // The bypass threshold must sit well below page.rs's Small-object
    // limit, or the TLAB would be deciding an object's page TIER.
    assert!(cfg.max_tlab_alloc < ZPageConfig::default().small_object_limit);
}

#[test]
fn filler_sentinels_are_the_shared_ones_not_new_ones() {
    assert!(is_tlab_filler_class(TLAB_FILLER_CLASS_ID.as_u32()));
    assert!(is_tlab_filler_class(GAP_FILLER_CLASS_ID.as_u32()));
    assert!(!is_tlab_filler_class(TEST_CID));
    assert!(!is_tlab_filler_class(0));
    assert_eq!(ZTLAB_TAIL_FILLER_CLASS_IDS.len(), 2);
}
