use super::*;
use crate::zgc::page::{ZPageConfig, ZPageSizeClass};
use std::sync::atomic::AtomicU64;
use std::sync::Barrier;

// -- test fixture -------------------------------------------------------

/// A miniature heap geometry, the same shape page.rs's own tests use: a
/// 256 KiB reservation instead of 256 MiB.
fn test_page_config() -> ZPageConfig {
    ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,
        medium_page_size: 65536,
        small_object_limit: 8192 / 8,
        medium_object_limit: 65536 / 8,
        max_capacity: 4096 * 64,
    }
}

/// A synthetic test object: `[u64 size][u64 tag][payload...]`.
///
/// Deliberately **not** an `ObjectHeader` — this module never decodes one
/// (that is `ZRelocateContext::object_size`'s job), so the tests do not
/// need one and pretending otherwise would couple them to a layout that has
/// changed three times this year.
const TEST_TAG: u64 = 0x5A47_4352_4C43_0001;

/// Fill `[addr, addr + size)` with a size word, a tag and a seeded pattern.
fn write_test_object(addr: usize, size: usize, seed: u8) {
    assert!(size >= 16 && size % 8 == 0);
    // SAFETY: `addr` was handed out by `ZPageReal::alloc` for at least
    // `size` bytes inside the allocator's reservation, which outlives the
    // test, and no other thread has been given these bytes.
    unsafe {
        std::ptr::write(addr as *mut u64, size as u64);
        std::ptr::write((addr + 8) as *mut u64, TEST_TAG);
        for i in 16..size {
            std::ptr::write((addr + i) as *mut u8, seed.wrapping_add(i as u8));
        }
    }
}

/// Read `[addr, addr + size)` back as bytes.
fn read_bytes(addr: usize, size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    // SAFETY: same provenance as `write_test_object`; the range is inside a
    // live page of the allocator's reservation.
    unsafe {
        std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), size);
    }
    out
}

/// Test context over a real [`ZPageAllocator`].
struct TestCtx {
    allocator: Arc<ZPageAllocator>,
    relocated: Mutex<Vec<(u64, u64)>>,
    abandoned: AtomicUsize,
    alloc_calls: AtomicUsize,
    /// When non-zero, `object_size` panics for this address (panic test).
    panic_at: AtomicU64,
    /// When true, `alloc_in` always fails (to-space exhaustion test).
    starve: AtomicBool,
    /// Registered object bases. Empty means "every address is a base",
    /// which is how the pre-2026-09-20 tests behave; non-empty makes
    /// `is_object_base` exact, which is what an interior-pointer test needs.
    bases: Mutex<std::collections::BTreeSet<u64>>,
    /// Addresses [`ZRelocateContext::is_pinned`] answers `true` for.
    pins: Mutex<std::collections::BTreeSet<u64>>,
    /// zs s2/sl2: when non-zero, `alloc_in` answers this address instead
    /// of allocating (the unencodable-destination test).
    alloc_override: AtomicU64,
}

impl TestCtx {
    fn new(allocator: Arc<ZPageAllocator>) -> Self {
        Self {
            allocator,
            relocated: Mutex::new(Vec::new()),
            abandoned: AtomicUsize::new(0),
            alloc_calls: AtomicUsize::new(0),
            panic_at: AtomicU64::new(0),
            starve: AtomicBool::new(false),
            bases: Mutex::new(std::collections::BTreeSet::new()),
            pins: Mutex::new(std::collections::BTreeSet::new()),
            alloc_override: AtomicU64::new(0),
        }
    }

    /// Make `is_object_base` exact over `addrs`.
    fn register_bases(&self, addrs: &[usize]) {
        let mut g = self.bases.lock();
        for &a in addrs {
            g.insert(a as u64);
        }
        drop(g);
    }

    fn pin(&self, addr: u64) {
        let mut g = self.pins.lock();
        g.insert(addr);
        drop(g);
    }

    fn relocated_pairs(&self) -> Vec<(u64, u64)> {
        let g = self.relocated.lock();
        let v = g.clone();
        drop(g);
        v
    }
}

impl ZRelocateContext for TestCtx {
    fn object_size(&self, addr: u64) -> usize {
        let boom = self.panic_at.load(Ordering::Relaxed);
        if boom != 0 && boom == addr {
            panic!("TestCtx: deliberate panic decoding the object at {addr:#x}");
        }
        // SAFETY: every address the tests pass here is an object base
        // inside a live page, written by `write_test_object`.
        unsafe { std::ptr::read(addr as *const u64) as usize }
    }

    fn alloc_in(&self, _gen_hint: u8, bytes: usize, align: usize) -> Option<u64> {
        self.alloc_calls.fetch_add(1, Ordering::Relaxed);
        if self.starve.load(Ordering::Relaxed) {
            return None;
        }
        let forced = self.alloc_override.load(Ordering::Relaxed);
        if forced != 0 {
            return Some(forced);
        }
        self.allocator
            .alloc_object(bytes, align)
            .ok()
            .map(|a| a as u64)
    }

    fn on_relocated(&self, from: u64, to: u64) {
        let mut g = self.relocated.lock();
        g.push((from, to));
        drop(g);
    }

    fn on_abandoned(&self, _to: u64, _bytes: usize) {
        self.abandoned.fetch_add(1, Ordering::Relaxed);
    }

    fn is_object_base(&self, addr: u64) -> bool {
        let g = self.bases.lock();
        // An empty set means the test never registered anything and wants
        // the old "everything is a base" behaviour. A test that registers
        // even one base opts the whole context into exactness.
        let answer = g.is_empty() || g.contains(&addr);
        drop(g);
        answer
    }

    fn is_pinned(&self, addr: u64) -> bool {
        let g = self.pins.lock();
        let answer = g.contains(&addr);
        drop(g);
        answer
    }
}

/// Everything a test needs, wired together.
struct Fixture {
    allocator: Arc<ZPageAllocator>,
    ctx: Arc<TestCtx>,
    registry: Arc<ZForwardingRegistry>,
    relocate: Arc<ZRelocate>,
    remap: Arc<ZRemapState>,
}

fn fixture() -> Fixture {
    let allocator =
        Arc::new(ZPageAllocator::new(test_page_config()).expect("test geometry must validate"));
    let ctx = Arc::new(TestCtx::new(Arc::clone(&allocator)));
    let registry = Arc::new(ZForwardingRegistry::new());
    // Explicit binding rather than an `as` cast: the unsizing coercion
    // `Arc<TestCtx> -> Arc<dyn ZRelocateContext>` needs a coercion site.
    // `ctx.clone()` (method form), NOT `Arc::clone(&ctx)`: the associated-fn
    // form infers its type parameter from the *expected* type, so it wants a
    // `&Arc<dyn ZRelocateContext>` argument and never reaches the unsizing
    // coercion. The method form clones `Arc<TestCtx>` and then coerces.
    let dyn_ctx: Arc<dyn ZRelocateContext> = ctx.clone();
    let relocate = Arc::new(ZRelocate::new(
        dyn_ctx,
        Arc::clone(&allocator),
        Arc::clone(&registry),
        ZRelocateConfig::default(),
    ));
    Fixture {
        allocator,
        ctx,
        registry,
        relocate,
        remap: Arc::new(ZRemapState::new()),
    }
}

/// Build a from-space page holding `count` objects of `size` bytes, claim
/// it for the relocation set, and install its forwarding table.
///
/// The page comes from `alloc_page`, not from `alloc_object`, so it is
/// never the allocator's *shared* page — which is what guarantees that
/// to-space allocations (which go through `alloc_object`) land in a
/// different page and can never alias the source.
fn make_from_page(f: &Fixture, count: usize, size: usize) -> (Arc<ZPageReal>, Vec<usize>) {
    let page = f
        .allocator
        .alloc_page(ZPageSizeClass::Small, 0)
        .expect("small page");
    let mut addrs = Vec::with_capacity(count);
    for i in 0..count {
        let a = page
            .alloc(size, 8)
            .expect("from-page must hold the objects");
        write_test_object(a, size, i as u8);
        addrs.push(a);
    }
    page.set_live_bytes(count * size);
    page.set_state(ZPageState::Relocatable);
    assert!(page.try_transition(ZPageState::Relocatable, ZPageState::InRelocationSet));

    f.registry.install(
        page.id(),
        Arc::new(ZForwardingTable::for_page(page.id(), count.max(1))),
    );
    f.remap.begin_cycle();
    assert!(f.remap.select_page(page.id(), count));
    (page, addrs)
}

// -- 1. single-object round trip ---------------------------------------

#[test]
fn single_object_relocate_round_trip() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 1, 64);
    let table = f.registry.get(page.id()).unwrap();
    let mut local = ZRelocateLocal::new();

    let before = read_bytes(addrs[0], 64);
    let out = f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
        .expect("relocation must succeed");

    assert!(out.fresh, "the first relocation of an object is fresh");
    assert_eq!(out.bytes, 64);
    assert_ne!(out.to, addrs[0] as u64, "the object must have moved");
    assert_eq!(read_bytes(out.to as usize, 64), before);

    // A second resolution adopts and copies nothing.
    let again = f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
        .expect("second resolution must succeed");
    assert_eq!(again.to, out.to);
    assert!(!again.fresh);
    assert_eq!(again.bytes, 0);

    assert_eq!(table.entry_count(), 1, "exactly one entry published");
    f.relocate.flush_local(&mut local);
    assert_eq!(f.relocate.stats().snapshot().objects_relocated, 1);
    assert_eq!(f.relocate.stats().snapshot().objects_adopted, 1);
}

// -- 2. THE important test: N threads race the SAME object -------------

#[test]
fn racing_threads_converge_on_one_winner_and_one_copy() {
    const THREADS: usize = 8;
    const SIZE: usize = 96;

    let f = fixture();
    let (page, addrs) = make_from_page(&f, 1, SIZE);
    let from = addrs[0] as u64;
    let expected = read_bytes(addrs[0], SIZE);
    let table = f.registry.get(page.id()).unwrap();

    let barrier = Arc::new(Barrier::new(THREADS));
    let mut results: Vec<u64> = Vec::new();
    let mut fresh_count = 0usize;

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let relocate = Arc::clone(&f.relocate);
            let table = Arc::clone(&table);
            let page = Arc::clone(&page);
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                let mut local = ZRelocateLocal::new();
                // Release every thread into `relocate_object` at once — the
                // narrower the window, the likelier a real CAS collision.
                barrier.wait();
                let out = relocate
                    .relocate_object(&page, &table, from, 0, &mut local)
                    .expect("every racer must succeed");
                relocate.flush_local(&mut local);
                out
            }));
        }
        for h in handles {
            let out = h.join().expect("no racer may panic");
            if out.fresh {
                fresh_count += 1;
            }
            results.push(out.to);
        }
    });

    // (a) Every thread observed the SAME winner.
    let winner = results[0];
    assert!(winner != 0);
    for (i, r) in results.iter().enumerate() {
        assert_eq!(
            *r, winner,
            "thread {i} disagreed about the winner — the object's identity split, which \
             is exactly the G1 loser-used-its-own-value defect"
        );
    }

    // (b) Exactly ONE copy is published.
    assert_eq!(fresh_count, 1, "exactly one thread may report `fresh`");
    assert_eq!(table.entry_count(), 1, "exactly one forwarding entry");
    assert_eq!(
        f.ctx.relocated_pairs().len(),
        1,
        "`on_relocated` must fire once per distinct object"
    );
    assert_eq!(f.ctx.relocated_pairs()[0], (from, winner));

    // (c) The published copy is byte-identical to the source.
    assert_eq!(read_bytes(winner as usize, SIZE), expected);

    // (d) Every racer that copied and lost is accounted for, and every
    //     racer is accounted for exactly once.
    let counts = f.relocate.stats().snapshot();
    assert_eq!(counts.objects_relocated, 1);
    assert_eq!(counts.resolutions(), THREADS);
    assert_eq!(
        counts.objects_abandoned,
        f.ctx.abandoned.load(Ordering::Relaxed)
    );

    // (e) The pointer map has the move, once.
    assert_eq!(f.relocate.record().len(), 1);
    assert_eq!(
        f.relocate.record().get(from as usize),
        Some(winner as usize)
    );
}

// -- 3. byte identity for a range of sizes ------------------------------

#[test]
fn copied_object_bytes_are_identical() {
    let f = fixture();
    for &size in &[16usize, 24, 64, 200, 512] {
        let (page, addrs) = make_from_page(&f, 1, size);
        let table = f.registry.get(page.id()).unwrap();
        let mut local = ZRelocateLocal::new();
        let before = read_bytes(addrs[0], size);
        let out = f
            .relocate
            .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
            .expect("relocation must succeed");
        assert_eq!(
            read_bytes(out.to as usize, size),
            before,
            "size {size}: the copy must be byte-identical"
        );
        f.relocate.flush_local(&mut local);
    }
}

// -- 4. a fully drained page can be recycled ---------------------------

#[test]
fn full_page_relocation_empties_the_page_and_permits_recycling() {
    const COUNT: usize = 24;
    const SIZE: usize = 64;

    let f = fixture();
    let (page, addrs) = make_from_page(&f, COUNT, SIZE);
    let page_id = page.id();
    let cycle = f.remap.current_cycle();

    let work = vec![ZRelocatePage::new(Arc::clone(&page))];
    assert!(work[0].is_claimed());

    // SAFETY: this test is single-threaded and has no mutator to park.
    let stw = unsafe { StopTheWorldToken::new() };
    let result = f.relocate.relocate_stw(&stw, &work, &f.remap);

    assert!(result.is_clean(), "clean drain: {result:?}");
    assert_eq!(result.pages_completed, 1);
    assert_eq!(result.counts.objects_relocated, COUNT);
    assert_eq!(result.outstanding_at_exit, 0);

    let table = f.registry.get(page_id).unwrap();
    assert_eq!(table.entry_count(), COUNT);
    for a in addrs.iter() {
        assert!(table.find_payload((*a - page.base()) as u64).is_some());
    }

    // ZR-1: `Relocated` is NOT recyclable. The page is quarantined.
    assert_eq!(f.remap.state_of(page_id), Some(ZPageRemapState::Relocated));
    assert!(
        !f.remap.may_recycle(page_id),
        "a Relocated page is quarantined until its table is uninstalled (ZR-1)"
    );
    assert_eq!(f.remap.quarantined_pages(), vec![page_id]);

    // The concurrent rule needs a STRICTLY later mark; the same cycle's
    // mark does not qualify.
    assert!(f.remap.note_mark_complete(cycle).is_empty());
    assert!(!f.remap.may_recycle(page_id));
    assert_eq!(f.remap.note_mark_complete(cycle + 1), vec![page_id]);
    assert!(f.remap.may_recycle(page_id));

    // Now the page may go back.
    f.registry.clear();
    f.allocator.free_page(&page);
    assert!(f.remap.note_recycled(page_id));
    assert_eq!(page.state(), ZPageState::Free);
    assert_eq!(page.used(), 0);
    assert!(f.remap.quarantined_pages().is_empty());
}

#[test]
fn stw_remap_completes_within_the_pause() {
    let f = fixture();
    let (page, _addrs) = make_from_page(&f, 4, 64);
    let page_id = page.id();
    let cycle = f.remap.current_cycle();
    let work = vec![ZRelocatePage::new(Arc::clone(&page))];
    // SAFETY: single-threaded test, no mutator to park.
    let stw = unsafe { StopTheWorldToken::new() };
    let _ = f.relocate.relocate_stw(&stw, &work, &f.remap);

    // The STW driver rewrites every reference inside the pause, so the
    // SAME cycle qualifies — the `<=` that distinguishes this path.
    assert!(!f.remap.may_recycle(page_id));
    assert_eq!(f.remap.note_stw_remap_complete(cycle), vec![page_id]);
    assert!(f.remap.may_recycle(page_id));
}

#[test]
fn a_quarantined_page_cannot_be_reselected() {
    let f = fixture();
    let (page, _addrs) = make_from_page(&f, 2, 64);
    let page_id = page.id();
    let work = vec![ZRelocatePage::new(Arc::clone(&page))];
    // SAFETY: single-threaded test.
    let stw = unsafe { StopTheWorldToken::new() };
    let _ = f.relocate.relocate_stw(&stw, &work, &f.remap);

    f.remap.begin_cycle();
    assert!(
        !f.remap.select_page(page_id, 2),
        "re-selecting a quarantined page would replace a table stale references \
         still resolve through (ZR-1)"
    );
}

// -- 5. the abandoned copy must not corrupt the target cursor -----------

#[test]
fn abandoned_copies_do_not_corrupt_the_target_page_cursor() {
    const THREADS: usize = 6;
    const SIZE: usize = 64;

    let f = fixture();
    let (page, addrs) = make_from_page(&f, 1, SIZE);
    let from = addrs[0] as u64;
    let expected = read_bytes(addrs[0], SIZE);
    let table = f.registry.get(page.id()).unwrap();
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut winner = 0u64;

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let relocate = Arc::clone(&f.relocate);
            let table = Arc::clone(&table);
            let page = Arc::clone(&page);
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                let mut local = ZRelocateLocal::new();
                barrier.wait();
                let out = relocate
                    .relocate_object(&page, &table, from, 0, &mut local)
                    .expect("racer");
                relocate.flush_local(&mut local);
                out.to
            }));
        }
        for h in handles {
            let to = h.join().expect("racer");
            if winner == 0 {
                winner = to;
            }
            assert_eq!(to, winner);
        }
    });

    // A fresh allocation must not be handed any byte the winner owns: an
    // abandoned copy leaves the cursor *ahead*, never behind. (Rewinding it
    // is the tempting "reclaim the waste" optimisation, and this assertion
    // is what would catch it.)
    let fresh = f
        .ctx
        .alloc_in(0, SIZE, ZRELOCATE_ALIGN)
        .expect("to-space must still have room");
    let w = winner;
    assert!(
        fresh >= w + SIZE as u64 || fresh + SIZE as u64 <= w,
        "a post-race allocation at {fresh:#x} overlaps the winning copy at {w:#x}"
    );

    // ...and the winner's bytes are untouched by everything that followed.
    assert_eq!(read_bytes(winner as usize, SIZE), expected);
    assert_eq!(table.entry_count(), 1);
}

// -- 6. a panicking worker must not hang the cycle ---------------------

#[test]
fn outstanding_guard_retires_on_unwind() {
    let outstanding = AtomicUsize::new(1);
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _g = ZOutstandingGuard::new(&outstanding);
        panic!("deliberate: a GC worker exploding mid-page");
    }));
    assert!(r.is_err(), "the panic must be observed, not swallowed");
    assert_eq!(
        outstanding.load(Ordering::Acquire),
        0,
        "the RAII guard must retire the count on the unwind path — a bare fetch_sub \
         after the fallible call is what turned the G1 crash into a 3h08m hang"
    );
}

#[test]
fn a_panicking_worker_fails_the_cycle_without_hanging() {
    const COUNT: usize = 8;
    const SIZE: usize = 64;

    let f = fixture();
    // Two pages: one healthy, one whose third object explodes on decode.
    let (good, _ga) = make_from_page(&f, COUNT, SIZE);
    let (bad, ba) = make_from_page(&f, COUNT, SIZE);
    f.ctx.panic_at.store(ba[2] as u64, Ordering::Relaxed);

    let work = vec![
        ZRelocatePage::new(Arc::clone(&good)),
        ZRelocatePage::new(Arc::clone(&bad)),
    ];
    let workers = ZRelocateWorkers::new(4);
    let result = workers.relocate_set(&f.relocate, &work, &f.remap);

    // The whole point: the run RETURNS.
    assert_eq!(
        result.outstanding_at_exit, 0,
        "every claimed page must retire its outstanding count, panic or not"
    );
    assert_eq!(result.worker_panics, 1);
    assert!(!result.is_clean());
    assert_eq!(result.failed_page_ids, vec![bad.id()]);

    // The healthy page still finished — one bad header must not abandon the
    // rest of the relocation set.
    assert_eq!(result.pages_completed, 1);
    assert_eq!(
        f.remap.state_of(good.id()),
        Some(ZPageRemapState::Relocated)
    );

    // The failed page is NOT marked relocated and therefore can never be
    // recycled by ZR-1's rule — which is the correct outcome for a page
    // whose survivors were not all copied out. zs s2/sl2: the two copies
    // published before the panic ARE credited (the page is `Relocating`,
    // not `Selected`): their entries are live and the page's state says
    // so. Before the fix the credit died with the drain's stack frame.
    assert_eq!(
        f.remap.state_of(bad.id()),
        Some(ZPageRemapState::Relocating)
    );
    assert_eq!(
        f.remap.entry(bad.id()).map(|e| e.relocated_objects),
        Some(2)
    );
    assert!(!f.remap.may_recycle(bad.id()));
    // Only the healthy page can ever leave quarantine.
    assert_eq!(
        f.remap.note_mark_complete(f.remap.current_cycle() + 1),
        vec![good.id()]
    );
}

/// The same guarantee for the STW path, which is the one that lands and the
/// only one that runs with every mutator parked.
///
/// What breaks if this fails: `relocate_stw` unwinds, and in the VM the
/// stack it unwinds through is the stack holding the stop-the-world. Every
/// mutator stays parked behind a pause that never ends — the 3h08m G1
/// evacuation wedge reached from a different direction, and a *worse*
/// outcome than the pool's because there is no peer left running to notice.
/// The assertion that matters is simply that this function RETURNS.
#[test]
fn a_panicking_stw_drain_fails_the_cycle_instead_of_unwinding_out_of_the_pause() {
    const COUNT: usize = 8;
    const SIZE: usize = 64;

    let f = fixture();
    // Two pages: one healthy, one whose third object explodes on decode.
    let (good, _ga) = make_from_page(&f, COUNT, SIZE);
    let (bad, ba) = make_from_page(&f, COUNT, SIZE);
    f.ctx.panic_at.store(ba[2] as u64, Ordering::Relaxed);

    let work = vec![
        ZRelocatePage::new(Arc::clone(&good)),
        ZRelocatePage::new(Arc::clone(&bad)),
    ];
    // SAFETY: single-threaded test, no mutator to park.
    let stw = unsafe { StopTheWorldToken::new() };
    let result = f.relocate.relocate_stw(&stw, &work, &f.remap);

    assert_eq!(
        result.outstanding_at_exit, 0,
        "the RAII guard must retire the count on the unwind path here too"
    );
    assert_eq!(result.worker_panics, 1, "the cycle must fail LOUDLY");
    assert!(!result.is_clean());
    assert_eq!(result.failed_page_ids, vec![bad.id()]);

    // One bad header must not abandon the rest of the set, on this path
    // either: the healthy page is fully evacuated.
    assert_eq!(result.pages_completed, 1);
    assert_eq!(
        f.remap.state_of(good.id()),
        Some(ZPageRemapState::Relocated)
    );
    // And the failed page keeps its quarantine: its drain never returned,
    // so its survivors were not all copied out and ZR-1 must not let it go.
    // zs s2/sl2: the two copies published before the panic are credited.
    assert_eq!(
        f.remap.state_of(bad.id()),
        Some(ZPageRemapState::Relocating)
    );
    assert_eq!(
        f.remap.entry(bad.id()).map(|e| e.relocated_objects),
        Some(2)
    );
    assert!(!f.remap.may_recycle(bad.id()));
}

#[test]
fn to_space_exhaustion_fails_the_page_loudly() {
    let f = fixture();
    let (page, _addrs) = make_from_page(&f, 4, 64);
    f.ctx.starve.store(true, Ordering::Relaxed);

    let work = vec![ZRelocatePage::new(Arc::clone(&page))];
    // SAFETY: single-threaded test.
    let stw = unsafe { StopTheWorldToken::new() };
    let result = f.relocate.relocate_stw(&stw, &work, &f.remap);

    assert!(!result.is_clean());
    assert_eq!(result.failed_page_ids, vec![page.id()]);
    assert_eq!(result.outstanding_at_exit, 0);
    assert!(f.relocate.stats().alloc_failures() >= 1);
    assert!(!f.remap.may_recycle(page.id()));
}

#[test]
fn an_implausible_object_size_aborts_the_walk_instead_of_spinning() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 4, 64);
    // Corrupt the second object's size word to 0 — `gc::object_total_size`'s
    // corruption sentinel. A stride of 0 would spin forever.
    // SAFETY: `addrs[1]` is a live object base written by the fixture.
    unsafe {
        std::ptr::write(addrs[1] as *mut u64, 0u64);
    }
    let work = vec![ZRelocatePage::new(Arc::clone(&page))];
    // SAFETY: single-threaded test.
    let stw = unsafe { StopTheWorldToken::new() };
    let result = f.relocate.relocate_stw(&stw, &work, &f.remap);

    assert_eq!(result.aborted_walk_page_ids, vec![page.id()]);
    assert_eq!(result.pages_completed, 0);
    assert_eq!(result.outstanding_at_exit, 0);
    assert_ne!(
        f.remap.state_of(page.id()),
        Some(ZPageRemapState::Relocated)
    );
}

// -- 7. the pointer map records every move -----------------------------

#[test]
fn the_pointer_map_records_every_move() {
    const COUNT: usize = 32;
    const SIZE: usize = 48;

    let f = fixture();
    let (page, addrs) = make_from_page(&f, COUNT, SIZE);
    let work = vec![ZRelocatePage::new(Arc::clone(&page))];
    // SAFETY: single-threaded test.
    let stw = unsafe { StopTheWorldToken::new() };
    let result = f.relocate.relocate_stw(&stw, &work, &f.remap);
    assert!(result.is_clean());

    let map = f.relocate.record().snapshot_pointer_map();
    assert_eq!(map.len(), COUNT, "one entry per relocated object");

    for a in addrs.iter() {
        let to = *map.get(a).expect("every from-address must be in the map");
        assert_ne!(to, *a, "a recorded move must actually move");
        // Read the table through the sanctioned decoder, never raw: a bare
        // `find` returns an ENCODED payload (see `ZRelocate::decode_to`).
        assert_eq!(
            f.relocate.forward_lookup(*a as u64),
            Some(to as u64),
            "the map and the forwarding table must agree"
        );
        // And the bytes really are there.
        let size = f.ctx.object_size(to as u64);
        assert_eq!(size, SIZE);
    }

    // `into_pointer_map` hands the map over and leaves the record empty —
    // this is the value `GcResult::pointer_map` must carry.
    let taken: HashMap<usize, usize> = f.relocate.record().into_pointer_map();
    assert_eq!(taken.len(), COUNT);
    assert!(f.relocate.record().is_empty());
}

#[test]
fn a_disabled_record_accumulates_nothing() {
    let allocator = Arc::new(ZPageAllocator::new(test_page_config()).expect("test geometry"));
    let ctx = Arc::new(TestCtx::new(Arc::clone(&allocator)));
    let registry = Arc::new(ZForwardingRegistry::new());
    // `ctx.clone()` (method form), NOT `Arc::clone(&ctx)`: the associated-fn
    // form infers its type parameter from the *expected* type, so it wants a
    // `&Arc<dyn ZRelocateContext>` argument and never reaches the unsizing
    // coercion. The method form clones `Arc<TestCtx>` and then coerces.
    let dyn_ctx: Arc<dyn ZRelocateContext> = ctx.clone();
    let relocate = ZRelocate::new(
        dyn_ctx,
        Arc::clone(&allocator),
        Arc::clone(&registry),
        ZRelocateConfig {
            record_pointer_map: false,
            ..ZRelocateConfig::default()
        },
    );
    assert!(!relocate.record().is_enabled());
    relocate.record().record_many(&[(0x1000, 0x2000)]);
    assert!(relocate.record().is_empty());
}

#[test]
fn destination_addresses_round_trip_through_the_encoding() {
    let f = fixture();
    let base = f.allocator.base() as u64;
    assert_eq!(f.relocate.to_encoding_base(), base);

    for &off in &[0u64, 8, 4096, 1 << 18] {
        let abs = base + off;
        let enc = f
            .relocate
            .encode_to(abs)
            .expect("an in-heap address must encode");
        // The three things `ZgcForwardingEntry::pack` demands.
        assert_ne!(enc.encoded(), 0, "pack rejects a zero destination");
        assert_eq!(enc.encoded() % 8, 0, "pack requires 8-alignment");
        assert!(
            enc.encoded() <= (1u64 << 44) - 8,
            "a raw process address would blow the 41-bit `to` field on Linux; the \
             heap-relative encoding is what keeps it in range"
        );
        assert_eq!(f.relocate.decode_to(enc), abs, "round trip");
    }

    // Below the base is a modelling error, reported rather than wrapped.
    assert!(matches!(
        f.relocate.encode_to(base - 8),
        Err(ZRelocateError::UnencodableDestination { .. })
    ));

    // The absolute mode is still expressible, for the day ZGC owns a low
    // reserved virtual range — but only over a heap that actually fits
    // below the 16 TiB the `to` field reaches. WHICH ARM RUNS IS THE POINT:
    // `Some(0)` is precisely the configuration that works on the Windows dev
    // host (heap placed low) and cannot work on the Linux build host.
    let cfg_absolute = ZRelocateConfig {
        to_encoding_base: Some(0),
        ..ZRelocateConfig::default()
    };
    let dyn_ctx: Arc<dyn ZRelocateContext> = f.ctx.clone();
    let built = ZRelocate::try_new(
        dyn_ctx,
        Arc::clone(&f.allocator),
        Arc::clone(&f.registry),
        cfg_absolute,
    );
    if f.allocator.end() as u64 <= ZFWD_MAX_PAYLOAD {
        let absolute = built.expect("a heap entirely below 16 TiB encodes absolutely");
        assert_eq!(absolute.to_encoding_base(), 0);
        assert_eq!(
            absolute.encode_to(0x1234_5678),
            Ok(ZForwardingPayload::from_encoded(0x1234_5678))
        );
        assert_eq!(
            absolute.decode_to(ZForwardingPayload::from_encoded(0x1234_5678)),
            0x1234_5678
        );
    } else {
        assert!(
            matches!(
                built,
                Err(ZRelocateError::EncodingBaseUnusable { requested: 0, .. })
            ),
            "a heap above 16 TiB must refuse absolute encoding, not discover it \
             one Unencodable insert at a time: {built:?}"
        );
    }
}

// -- 7b. the address-domain boundary (added 2026-08-07) ----------------

/// The offset domain the load barrier traffics in, round-tripped through
/// the same relocation the absolute entry points perform.
#[test]
fn the_offset_domain_round_trips_and_agrees_with_the_absolute_one() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 2, 64);
    let heap_base = f.relocate.heap_base();
    assert_eq!(heap_base, f.allocator.base() as u64);

    let from_abs = addrs[0] as u64;
    let from_off = f
        .relocate
        .offset_of_address(from_abs)
        .expect("an object in the reservation has an offset");
    assert_eq!(from_off, from_abs - heap_base);
    assert_eq!(f.relocate.address_of_offset(from_off), Some(from_abs));
    assert!(
        is_bare_offset(from_off, Z_OFFSET_MASK),
        "an in-heap offset is a bare 42-bit value by construction"
    );

    // Nothing has moved yet.
    assert_eq!(f.relocate.forward_lookup_offset(from_off), None);

    let to_off = f
        .relocate
        .forward_offset(from_off)
        .expect("forward_offset must succeed")
        .expect("the offset is in a relocating page");
    assert!(
        is_bare_offset(to_off, Z_OFFSET_MASK),
        "the value handed to the load barrier must be a bare offset"
    );
    assert_ne!(to_off, from_off, "the object must have moved");

    // The two domains describe the SAME move, differing by exactly the heap
    // base. A discrepancy here is the fixed G1 `address + 3` shape.
    let to_abs = f
        .relocate
        .forward_lookup(from_abs)
        .expect("the absolute lookup sees the same entry");
    assert_eq!(to_off, to_abs - heap_base);
    assert_eq!(f.relocate.address_of_offset(to_off), Some(to_abs));
    assert_eq!(f.relocate.forward_lookup_offset(from_off), Some(to_off));

    // An offset in a page with no installed table is the identity case.
    assert_eq!(f.relocate.forward_offset(to_off).unwrap(), None);
    let _ = page;
}

/// A machine address handed to an offset-domain entry point must be
/// **caught**, never silently accepted — in release builds as well as
/// debug, because on the Windows dev host the mistake is invisible.
#[test]
fn a_machine_address_where_an_offset_belongs_is_caught() {
    let f = fixture();

    // A Linux-shaped heap address: ~2^47, five whole bits past the 42-bit
    // offset field. This is what `page.rs` hands out on the build host.
    const LINUX_SHAPED: u64 = 0x7F00_0000_1000;
    // A still-coloured word, which is the other way to be in the wrong
    // domain: `vaddr::Z_COLORED_TAG` is bit 63.
    const STILL_COLOURED: u64 = (1u64 << 63) | (1u64 << 44) | 0x1000;

    for bad in [LINUX_SHAPED, STILL_COLOURED] {
        assert!(
            !is_bare_offset(bad, Z_OFFSET_MASK),
            "{bad:#x} must not pass the barrier's own domain predicate"
        );

        let caught =
            std::panic::catch_unwind(AssertUnwindSafe(|| f.relocate.forward_offset(bad)));
        match caught {
            // Debug builds: the `debug_assert!` in `check_offset_domain`
            // turns it into a test failure at the boundary.
            Err(_) => {}
            // Release builds: asserts are off, so what must survive is the
            // REFUSAL (plus the `tracing::error!`). Silently answering
            // `Ok(_)` here would be the bug this file exists to prevent.
            Ok(v) => assert!(
                matches!(v, Err(ZRelocateError::NotInHeap { .. })),
                "a machine address must be refused, not translated: {v:?}"
            ),
        }

        let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
            f.relocate.forward_lookup_offset(bad)
        }));
        match caught {
            Err(_) => {}
            Ok(v) => assert_eq!(v, None, "a machine address must not resolve"),
        }
    }
}

/// `to_encoding_base: Some(0)` over a Linux-shaped reservation must be
/// refused up front, not discovered one `Unencodable` insert at a time.
///
/// Driven through [`ZRelocate::check_encoding_base`] with synthetic bases so
/// the verdict does not depend on where *this* host's allocator landed —
/// which is the entire hazard being pinned.
#[test]
fn an_unusable_encoding_base_is_refused_before_the_first_object_moves() {
    // Where Linux `mmap`s a quarter-gigabyte `Vec<u8>`.
    const LINUX_BASE: u64 = 0x7F00_0000_0000;
    // Where a Windows heap of the same size tends to land.
    const WINDOWS_BASE: u64 = 0x0000_0200_0000;
    const HEAP: u64 = 256 << 20;

    // (a) The footgun itself.
    assert!(
        matches!(
            ZRelocate::check_encoding_base(0, LINUX_BASE, LINUX_BASE + HEAP),
            Err(ZRelocateError::EncodingBaseUnusable { requested: 0, .. })
        ),
        "absolute encoding over a 0x7f… heap cannot fit the 41-bit `to` field"
    );

    // (b) ...and why it ships green: the same configuration is genuinely
    //     fine on a low heap, so no dev-host test would ever notice.
    assert!(ZRelocate::check_encoding_base(0, WINDOWS_BASE, WINDOWS_BASE + HEAP).is_ok());

    // (c) The default — the allocator base — works at either placement,
    //     because the encoded value is bounded by the heap SIZE.
    assert!(ZRelocate::check_encoding_base(LINUX_BASE, LINUX_BASE, LINUX_BASE + HEAP).is_ok());
    assert!(
        ZRelocate::check_encoding_base(WINDOWS_BASE, WINDOWS_BASE, WINDOWS_BASE + HEAP).is_ok()
    );

    // (d) A base above the heap base cannot express the bottom of the heap
    //     at all — every destination there fails `checked_sub`.
    assert!(
        ZRelocate::check_encoding_base(LINUX_BASE + 8, LINUX_BASE, LINUX_BASE + HEAP).is_err()
    );

    // (e) The boundary: a heap ending exactly at ZFWD_MAX_PAYLOAD encodes
    //     absolutely; one byte-grid past it does not.
    assert!(ZRelocate::check_encoding_base(0, 8, ZFWD_MAX_PAYLOAD).is_ok());
    assert!(ZRelocate::check_encoding_base(0, 8, ZFWD_MAX_PAYLOAD + 8).is_err());
}

/// `encode_to` must bound the destination at BOTH ends. Before 2026-08-07 it
/// checked only the lower one, so a destination above the modelled heap
/// surfaced as `Forwarding { cause: Unencodable }` — blaming the forwarding
/// table for the relocator's mistake.
#[test]
fn encode_to_rejects_a_destination_above_the_encoding_range() {
    let f = fixture();
    let base = f.relocate.to_encoding_base();
    assert_ne!(base, 0, "the default configuration is heap-relative");

    // The largest destination the `to` field can hold, and the next grid
    // slot up.
    let highest_ok = base + ZFWD_MAX_PAYLOAD - ZRELOCATE_ENCODING_BIAS;
    let too_high = highest_ok + ZRELOCATE_ENCODING_BIAS;

    let enc = f
        .relocate
        .encode_to(highest_ok)
        .expect("the top of the encodable range must encode");
    assert_eq!(enc.encoded(), ZFWD_MAX_PAYLOAD);

    match f.relocate.encode_to(too_high) {
        Err(ZRelocateError::DestinationAboveEncodingRange {
            to,
            encoded,
            max_payload,
            ..
        }) => {
            assert_eq!(to, too_high);
            assert!(encoded > max_payload);
            assert_eq!(max_payload, ZFWD_MAX_PAYLOAD);
        }
        other => panic!(
            "a destination above the encoding range must name ITSELF as the cause, \
             not reach the forwarding table and come back as Unencodable: {other:?}"
        ),
    }

    // And the message must not point at the table.
    let msg = ZRelocateError::DestinationAboveEncodingRange {
        to: too_high,
        base,
        encoded: ZFWD_MAX_PAYLOAD + 8,
        max_payload: ZFWD_MAX_PAYLOAD,
    }
    .to_string();
    assert!(msg.contains("NOT a forwarding-table defect"), "{msg}");
}

// -- 8. the barrier entry point ----------------------------------------

#[test]
fn forward_relocates_on_demand_and_passes_through_unrelocating_pages() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 2, 64);
    let from = addrs[0] as u64;
    let before = read_bytes(addrs[0], 64);

    assert_eq!(f.relocate.forward_lookup(from), None, "nothing copied yet");
    let to = f
        .relocate
        .forward(from)
        .expect("forward must succeed")
        .expect("the address is in a relocating page");
    assert_ne!(to, from);
    assert_eq!(read_bytes(to as usize, 64), before);
    assert_eq!(f.relocate.forward_lookup(from), Some(to));
    // Idempotent.
    assert_eq!(f.relocate.forward(from).unwrap(), Some(to));

    // An address in a page with no installed table did not move.
    assert_eq!(f.relocate.forward(to).unwrap(), None);
    // An address outside the heap entirely.
    assert_eq!(f.relocate.forward(8).unwrap(), None);
    let _ = page;
}

// -- 9. the safepoint --------------------------------------------------

#[test]
fn a_worker_parks_at_a_requested_safepoint_and_resumes() {
    let sp = Arc::new(ZRelocateSafepoint::new());
    let entered = Arc::new(Barrier::new(2));
    let observed = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        let sp_w = Arc::clone(&sp);
        let entered_w = Arc::clone(&entered);
        let observed_w = Arc::clone(&observed);
        let h = scope.spawn(move || {
            let m = sp_w.join();
            entered_w.wait();
            // Poll until the request has been seen and released. Counts
            // only, never a wall-clock bound — fixed timing bounds are a
            // documented source of CI flakes in this tree.
            loop {
                sp_w.poll(&m);
                observed_w.fetch_add(1, Ordering::Relaxed);
                if !sp_w.is_requested() && observed_w.load(Ordering::Relaxed) > 1 {
                    break;
                }
                std::thread::yield_now();
            }
            drop(m);
        });

        entered.wait();
        sp.request();
        sp.wait_until_all_parked();
        // While parked, the worker is provably not touching the heap.
        assert!(sp.parked() >= 1 || sp.active() == 0);
        sp.release();
        h.join().expect("worker must finish");
    });

    assert_eq!(sp.parked(), 0, "no member may stay parked after release");
    assert_eq!(sp.active(), 0, "membership must be retired on drop");
}

#[test]
fn a_dead_member_cannot_be_waited_for() {
    // `wait_until_all_parked` must not block on a member that has gone
    // away — the membership guard's Drop is what makes that true.
    let sp = ZRelocateSafepoint::new();
    {
        let _m = sp.join();
        assert_eq!(sp.active(), 1);
    }
    assert_eq!(sp.active(), 0);
    sp.request();
    sp.wait_until_all_parked(); // returns immediately: nobody is active
    sp.release();
}

// -- 10. parallel drain over several pages -----------------------------

#[test]
fn parallel_workers_drain_every_page_exactly_once() {
    const PAGES: usize = 6;
    const COUNT: usize = 16;
    const SIZE: usize = 48;

    let f = fixture();
    let mut work = Vec::with_capacity(PAGES);
    let mut all_addrs: Vec<(u64, Arc<ZPageReal>)> = Vec::new();
    for _ in 0..PAGES {
        let (page, addrs) = make_from_page(&f, COUNT, SIZE);
        for a in addrs {
            all_addrs.push((a as u64, Arc::clone(&page)));
        }
        work.push(ZRelocatePage::new(page));
    }

    let workers = ZRelocateWorkers::new(4);
    let result = workers.relocate_set(&f.relocate, &work, &f.remap);

    assert!(result.is_clean(), "{result:?}");
    assert_eq!(result.pages_completed, PAGES);
    assert_eq!(result.counts.objects_relocated, PAGES * COUNT);
    assert_eq!(
        result.counts.objects_abandoned, 0,
        "no two workers share a page"
    );
    assert_eq!(result.outstanding_at_exit, 0);
    assert_eq!(f.relocate.record().len(), PAGES * COUNT);

    for (from, page) in all_addrs.iter() {
        let to = f
            .relocate
            .forward_lookup(*from)
            .expect("every object must be forwarded");
        assert!(
            !page.contains(to as usize),
            "to-space must not be from-space"
        );
        assert_eq!(f.relocate.record().get(*from as usize), Some(to as usize));
    }
}

// -- 11. types are shareable across workers ----------------------------

#[test]
fn relocation_types_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ZRelocate>();
    assert_send_sync::<ZRemapState>();
    assert_send_sync::<ZRelocateSafepoint>();
    assert_send_sync::<ZRelocateWorkers>();
    assert_send_sync::<ZRelocationRecord>();
    assert_send_sync::<ZRelocateStats>();
    assert_send_sync::<ZRelocatePage>();
}

/// zt t7/cc (zs3ac item 5): every participant's drain-loop CPU is summed
/// into the run result, saturating, panicked participants included; a
/// fresh result reads 0.
#[test]
fn zt7cc_the_pool_result_sums_every_participants_cpu() {
    let mut r = ZRelocateRunResult::new(3);
    assert_eq!(r.worker_cpu_ns, 0);
    r.merge_worker(ZWorkerTotals {
        cpu_ns: 5,
        ..Default::default()
    });
    r.merge_worker(ZWorkerTotals {
        panics: 1,
        cpu_ns: 7,
        ..Default::default()
    });
    assert_eq!(r.worker_cpu_ns, 12);
    assert_eq!(r.worker_panics, 1);
    r.merge_worker(ZWorkerTotals {
        cpu_ns: u64::MAX,
        ..Default::default()
    });
    assert_eq!(r.worker_cpu_ns, u64::MAX, "saturates");    }

/// The index must answer exactly what the map it replaced answered, for
/// every address in and around the moved set — including the two ends,
/// which the range check special-cases.
#[test]
fn the_forward_index_answers_what_a_map_of_the_same_pairs_would() {
    let pairs = [(0x9000, 0x1000), (0x3000, 0x2000), (0x7000, 0x4000)];
    let index = ZForwardIndex::from_pairs(&pairs);
    let reference: HashMap<usize, usize> = pairs.iter().copied().collect();
    for addr in (0..0xC000).step_by(8) {
        assert_eq!(
            index.get(addr),
            reference.get(&addr).copied(),
            "disagreement at {addr:#x}"
        );
        assert_eq!(index.contains_from(addr), reference.contains_key(&addr));
        assert_eq!(
            index.resolve(addr),
            reference.get(&addr).copied().unwrap_or(addr)
        );
    }
    assert_eq!(index.len(), 3);
    assert_eq!(
        index.entries(),
        &[(0x3000, 0x2000), (0x7000, 0x4000), (0x9000, 0x1000)],
        "entries must come back ascending by `from`"
    );
}

/// An identity pair is not a move. The high pack emits one for a pinned
/// survivor it left in place, and recording it would make the rewrite pass
/// store a slot's own value back over itself — harmless, but it would also
/// make `contains_from` claim the address was vacated, which the
/// post-slide verifier reads as a MISSED REWRITE rather than an untouched
/// object.
#[test]
fn an_identity_pair_is_not_a_move() {
    let index = ZForwardIndex::from_pairs(&[(0x4000, 0x4000), (0x8000, 0x2000)]);
    assert_eq!(index.get(0x4000), None);
    assert!(!index.contains_from(0x4000));
    assert_eq!(index.resolve(0x4000), 0x4000);
    assert_eq!(index.get(0x8000), Some(0x2000));
    assert_eq!(index.len(), 1);
}

/// The empty index must not claim the whole address space. `lo`/`hi` are
/// seeded `usize::MAX`/`0` so the range check rejects everything before
/// the search can look at an empty slice.
#[test]
fn an_empty_index_forwards_nothing() {
    let index = ZForwardIndex::from_pairs(&[]);
    assert!(index.is_empty());
    for addr in [0usize, 8, 0x1000, usize::MAX / 2, usize::MAX] {
        assert_eq!(index.get(addr), None);
        assert_eq!(index.resolve(addr), addr);
    }
}

// -- the two gates: object base, and pinning ---------------------------
//
// `gc/tests/zgc_h_relocate_pin_and_object_base.rs` is the thorough version
// (it counts `object_size` calls to prove the gate runs BEFORE the header
// read). These are the unit-level twins, kept here so a change to
// `relocate_object` fails in this file rather than only in an integration
// target.

/// An interior pointer is refused, and refused *before* the adopt probe and
/// the size call. Without the gate, `find_payload` misses at a non-base
/// offset, `object_size` is handed a mid-object byte, and a plausible
/// answer gets a copy published under a key no reader will look up.
#[test]
fn an_interior_pointer_is_refused_by_relocate_object() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 1, 64);
    f.ctx.register_bases(&addrs);
    let table = f.registry.get(page.id()).unwrap();
    let mut local = ZRelocateLocal::new();

    for off in [1usize, 8, 32, 63] {
        let err = f
            .relocate
            .relocate_object(&page, &table, (addrs[0] + off) as u64, 0, &mut local)
            .expect_err("an interior pointer must be refused");
        assert!(
            matches!(err, ZRelocateError::NotAnObjectBase { .. }),
            "offset {off} produced {err:?}"
        );
    }
    assert_eq!(table.entry_count(), 0, "nothing may be published");

    // The base itself is unaffected.
    let out = f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
        .expect("the base must still relocate");
    assert!(out.fresh);
    assert_eq!(table.entry_count(), 1);
    f.relocate.flush_local(&mut local);
}

/// A pinned object is refused with [`ZRelocateError::Pinned`] — a refusal,
/// never a self-forward, because this module has no rung for a
/// half-evacuated page.
#[test]
fn a_pinned_object_is_refused_by_relocate_object() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 2, 64);
    f.ctx.register_bases(&addrs);
    f.ctx.pin(addrs[0] as u64);
    let table = f.registry.get(page.id()).unwrap();
    let mut local = ZRelocateLocal::new();

    let err = f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
        .expect_err("a pinned object must not move");
    match err {
        ZRelocateError::Pinned { from, page_id } => {
            assert_eq!(from, addrs[0] as u64);
            assert_eq!(page_id, page.id());
        }
        other => panic!("expected Pinned, got {other:?}"),
    }
    assert_eq!(table.entry_count(), 0);

    // The sibling still moves: one pin does not refuse the page.
    assert!(
        f.relocate
            .relocate_object(&page, &table, addrs[1] as u64, 0, &mut local)
            .expect("the sibling is not pinned")
            .fresh
    );
    assert_eq!(table.entry_count(), 1);
    f.relocate.flush_local(&mut local);
}

/// **A pinned object that has ALREADY been forwarded resolves to its
/// destination — it does not come back `Err(Pinned)`.**
///
/// The gate order this asserts was wrong until 2026-09-21 (wave 4, lane Q;
/// observed by wave 3 and left unfixed because `alloc_in` answered `None`,
/// so nothing could reach it). `is_pinned` sat above the lookup, so a
/// published entry — which is *proof the object has already moved* — was
/// discarded in favour of a refusal.
///
/// Refusing does not un-move the object. It tells [`ZRelocate::forward`]'s
/// callers "this reference cannot be resolved", and `forward`'s contract
/// then forbids falling back to `from`, so the reference is stranded on a
/// from-space address whose page ZR-1 may already have quarantined. A pin
/// gate producing a use-after-free by *refusing* to move something is
/// exactly the failure this module's invariants exist to make impossible.
#[test]
fn a_pinned_object_with_a_published_entry_adopts_instead_of_refusing() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 2, 64);
    f.ctx.register_bases(&addrs);
    let table = f.registry.get(page.id()).unwrap();
    let mut local = ZRelocateLocal::new();

    // Move it while nothing is pinned...
    let out = f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
        .expect("an unpinned object moves");
    assert!(out.fresh);
    let to = out.to;

    // ...and pin it afterwards. The entry stays published, so the object
    // HAS moved and the only correct answer is where it moved to.
    f.ctx.pin(addrs[0] as u64);
    let again = f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
        .expect(
            "a published forwarding entry is proof the object already moved; \
             refusing now strands every reference to it on a from-space address",
        );
    assert_eq!(again.to, to, "the adopt must hand back the winner");
    assert!(!again.fresh, "nothing was copied");
    assert_eq!(again.bytes, 0);

    // Through the entry point the driver and the external address-keyed
    // tables actually use, which is where the stranding would happen.
    assert_eq!(
        f.relocate
            .forward(addrs[0] as u64)
            .expect("forward must resolve a moved object, pinned or not"),
        Some(to),
    );

    // The gate is MOVED, not removed: a pinned object with no entry is
    // still refused, before a byte of it is read.
    f.ctx.pin(addrs[1] as u64);
    match f
        .relocate
        .relocate_object(&page, &table, addrs[1] as u64, 0, &mut local)
    {
        Err(ZRelocateError::Pinned { from, .. }) => assert_eq!(from, addrs[1] as u64),
        other => panic!("an unmoved pinned object must still be refused: {other:?}"),
    }
    assert_eq!(table.entry_count(), 1, "the pin must not have published");
    f.relocate.flush_local(&mut local);
}

/// `drain_page` skips the pin, evacuates the rest, and reports the page as
/// pinned so that ZR-1(a) is not violated by marking it `Relocated`.
#[test]
fn drain_page_counts_a_pin_and_withholds_the_page() {
    const COUNT: usize = 4;
    let f = fixture();
    let (page, addrs) = make_from_page(&f, COUNT, 64);
    f.ctx.register_bases(&addrs);
    f.ctx.pin(addrs[1] as u64);

    let work = ZRelocatePage::new(Arc::clone(&page));
    let safepoint = ZRelocateSafepoint::new();
    let membership = safepoint.join();
    let mut local = ZRelocateLocal::new();
    let out = f
        .relocate
        .drain_page(&work, &mut local, &safepoint, &membership)
        .expect("a pin must not fail the page");
    drop(membership);

    assert_eq!(out.objects_seen, COUNT);
    assert_eq!(out.objects_pinned, 1);
    assert_eq!(out.objects_relocated, COUNT - 1);
    assert!(!out.walk_aborted, "a pin is not a lost walk");
    f.relocate.flush_local(&mut local);

    // SAFETY: single-threaded test; there is no other mutator to park.
    let stw = unsafe { StopTheWorldToken::new() };
    let result = f.relocate.relocate_stw(&stw, &[work], &f.remap);
    assert_eq!(result.pinned_page_ids, vec![page.id()]);
    assert_eq!(result.pages_completed, 0);
    assert!(!result.is_clean());
}

// -- zs s2/sl2: the page-evac latent defects ------------------------------

/// Defect 1: a size that runs past the page's walk top is refused BEFORE
/// any to-space is taken or any byte is read past the page. Positive
/// control: the object below it, with an honest size, relocates.
#[test]
fn zs2sl2_a_size_past_the_walk_top_is_refused_before_the_copy() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 4, 64);
    let table = f.registry.get(page.id()).unwrap();
    let top = page.walk_bounds().1;
    let last = addrs[3];
    // One grid step past the walk top: the old lower-bound-only check
    // admitted it and copied past the page.
    let lying = top - last + 8;
    // SAFETY: `last` is a live object base written by the fixture.
    unsafe { std::ptr::write(last as *mut u64, lying as u64) };
    let mut local = ZRelocateLocal::new();
    let calls_before = f.ctx.alloc_calls.load(Ordering::Relaxed);
    match f
        .relocate
        .relocate_object(&page, &table, last as u64, 0, &mut local)
    {
        Err(ZRelocateError::ImplausibleSize { from, size }) => {
            assert_eq!(from, last as u64);
            assert_eq!(size, lying);
        }
        other => panic!("an overrunning size must be refused: {other:?}"),
    }
    assert_eq!(f.relocate.stats().size_overruns(), 1);
    assert_eq!(
        f.ctx.alloc_calls.load(Ordering::Relaxed),
        calls_before,
        "no to-space may be taken for a refused object"
    );
    assert_eq!(table.entry_count(), 0);
    // Positive control: an honest object on the same page still moves.
    let ok = f
        .relocate
        .relocate_object(&page, &table, addrs[2] as u64, 0, &mut local)
        .expect("the honest object relocates");
    assert!(ok.fresh);
    assert_eq!(f.relocate.stats().size_overruns(), 1);
    f.relocate.flush_local(&mut local);
}

/// Defect 2: a destination the payload cannot encode is refused before the
/// copy, and the allocation is handed back (`on_abandoned`) and counted.
#[test]
fn zs2sl2_an_unencodable_destination_is_abandoned_before_the_copy() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 1, 64);
    let table = f.registry.get(page.id()).unwrap();
    assert_ne!(f.relocate.to_encoding_base(), 0);
    // Below the encoding base: `encode_to` answers UnencodableDestination.
    // Nothing may be written there (it is not memory the test owns), which
    // is itself the check that the copy no longer precedes the encode.
    f.ctx.alloc_override.store(8, Ordering::Relaxed);
    let mut local = ZRelocateLocal::new();
    match f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
    {
        Err(ZRelocateError::UnencodableDestination { to, .. }) => assert_eq!(to, 8),
        other => panic!("an unencodable destination must be refused: {other:?}"),
    }
    assert_eq!(f.relocate.stats().encode_failures(), 1);
    assert_eq!(f.ctx.abandoned.load(Ordering::Relaxed), 1);
    assert_eq!(table.entry_count(), 0);
    // Kill the override: the same object then relocates normally.
    f.ctx.alloc_override.store(0, Ordering::Relaxed);
    let ok = f
        .relocate
        .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
        .expect("relocates once the destination is encodable");
    assert!(ok.fresh);
    assert_eq!(f.ctx.abandoned.load(Ordering::Relaxed), 1);
    f.relocate.flush_local(&mut local);
}

/// Defect 4: an announced member that has not joined holds
/// `wait_until_all_parked`; withdrawing the expectation releases it.
/// Positive control: without the announcement the wait returns at once
/// with nobody active (the window the fix closes).
#[test]
fn zs2sl2_an_announced_member_holds_the_park_wait_until_it_joins() {
    let sp = Arc::new(ZRelocateSafepoint::new());
    // Control: nobody announced, nobody active -> immediate return.
    sp.wait_until_all_parked();

    let expected = sp.expect_members(1);
    assert_eq!(sp.pending(), 1);
    sp.request();
    let done = Arc::new(AtomicBool::new(false));
    let (sp_d, done_d) = (Arc::clone(&sp), Arc::clone(&done));
    let driver = std::thread::spawn(move || {
        sp_d.wait_until_all_parked();
        done_d.store(true, Ordering::Release);
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !done.load(Ordering::Acquire),
        "the driver must wait for a member that was announced but has not joined"
    );
    // The member joins and parks at its first poll.
    let sp_w = Arc::clone(&sp);
    let worker = std::thread::spawn(move || {
        let m = sp_w.join();
        sp_w.poll(&m);
    });
    driver
        .join()
        .expect("driver returns once the member parked");
    assert!(done.load(Ordering::Acquire));
    assert_eq!(sp.pending(), 0, "join consumed the expectation");
    sp.release();
    worker.join().expect("worker resumes");
    drop(expected);
    assert_eq!(sp.pending(), 0);

    // A member that never joins: dropping the guard withdraws it.
    let never = sp.expect_members(2);
    assert_eq!(sp.pending(), 2);
    drop(never);
    assert_eq!(sp.pending(), 0);
    sp.wait_until_all_parked();
}

/// Defect 5: a drain that fails mid-page credits the copies it published.
#[test]
fn zs2sl2_a_drain_that_fails_mid_page_credits_its_published_copies() {
    let f = fixture();
    let (page, addrs) = make_from_page(&f, 4, 64);
    // Object 2 overruns the walk top -> `ImplausibleSize` from
    // `relocate_object`, an `Ok(Err(_))` drain after two copies.
    let lying = page.walk_bounds().1 - addrs[2] + 8;
    // SAFETY: `addrs[2]` is a live object base written by the fixture.
    unsafe { std::ptr::write(addrs[2] as *mut u64, lying as u64) };
    let work = vec![ZRelocatePage::new(Arc::clone(&page))];
    // SAFETY: single-threaded test.
    let stw = unsafe { StopTheWorldToken::new() };
    let result = f.relocate.relocate_stw(&stw, &work, &f.remap);
    assert_eq!(result.failed_page_ids, vec![page.id()]);
    assert_eq!(
        f.remap.state_of(page.id()),
        Some(ZPageRemapState::Relocating),
        "two entries are published: the page is no longer merely Selected"
    );
    assert_eq!(
        f.remap.entry(page.id()).map(|e| e.relocated_objects),
        Some(2)
    );
    assert!(!f.remap.may_recycle(page.id()));
}
