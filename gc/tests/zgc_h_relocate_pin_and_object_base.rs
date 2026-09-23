// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ZRelocate` refuses an interior pointer and refuses a pinned object, and
//! does both **before it reads a byte of the object**.
//!
//! # Why this file exists
//!
//! Wave 1 of the 2026-09-20 ZGC round filed
//! `docs/internal/zgc-round-20260920/gap-b-relocate-moves-pinned-objects-and-interior-pointers.md`
//! as the most serious open finding in the round: `ZRelocate` had **no pin gate
//! and no object-base gate**, and both hazards were reachable through its two
//! public resolution entry points.
//!
//! Both failure modes are silent heap corruption, and they are silent in
//! different ways, which is why there is a test per mode rather than one
//! "relocation still works" test:
//!
//! * **Interior pointer.** `forward(base + 8)` used to find the page, miss in
//!   the forwarding table (no entry is keyed at a non-base offset), and hand
//!   the address to `ZRelocateContext::object_size`, which was being asked to
//!   decode an `ObjectHeader` at a byte that is not one. Mid-object bytes are
//!   **not detectably corrupt** — a reference field holding a small heap
//!   address reads as a plausible `class_id`/`shape` pair — so the
//!   `ImplausibleSize` guard cannot fire. On a plausible size the module copied
//!   from mid-object, published a forwarding entry under a **non-base key**,
//!   and returned a to-space address naming the middle of a synthesised object.
//!   This is not hypothetical for this VM: `gc/src/zgc.rs`'s
//!   `pinned_jit_roots_snapshot` exists precisely because a conservative
//!   JIT-frame scan is over-approximate, and its own comment says "an interior
//!   pointer or a plain `long` can present as a root".
//!
//! * **Pin.** `drain_page` relocated every object the context called live, with
//!   no consultation of a pin table, a JNI-critical count, a GPU
//!   `SafepointToken`, or the VM's conservative JIT-root snapshot. Large pages
//!   are excluded by `ZRelocationPolicy::relocate_large_pages`, which covers the
//!   common `GetPrimitiveArrayCritical` case *by accident but not on purpose* —
//!   and the production slide in `gc/src/zgc.rs` has real pin handling that
//!   `ZRelocate` inherits **none** of.
//!
//! # The assertion that matters is the counter, not the absence of a crash
//!
//! The gap page is explicit about this and these tests follow it: "it did not
//! crash" is satisfied by a context that happened to answer a plausible size.
//! So the interior-pointer tests assert that `object_size` was **never called**
//! for the offending address, using a context that records every address it was
//! asked about. A future refactor that moves the gate below the size call would
//! still return the right `Err`, and would still be the bug.

#![cfg(feature = "zgc")]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cratonvm_gc::collector::StopTheWorldToken;
use cratonvm_gc::zgc::forwarding::{ZForwardingRegistry, ZForwardingTable};
use cratonvm_gc::zgc::page::{ZPageAllocator, ZPageConfig, ZPageReal, ZPageSizeClass, ZPageState};
use cratonvm_gc::zgc::relocate::{
    ZRelocate, ZRelocateConfig, ZRelocateContext, ZRelocateError, ZRelocateLocal, ZRelocatePage,
    ZRelocateSafepoint, ZRemapState,
};

/// Test-only `StopTheWorldToken`: every test in this file is single-threaded
/// and no mutator exists, which is the precondition the token stands for.
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded test process; there is no other mutator to park.
    unsafe { StopTheWorldToken::new() }
}

// ===========================================================================
// Fixture
// ===========================================================================

/// A miniature heap geometry: a 256 KiB reservation instead of 256 MiB, the
/// same shape `page.rs`'s own tests use.
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

/// A synthetic object: `[u64 size][u64 tag][payload…]`. Deliberately **not** an
/// `ObjectHeader` — `relocate.rs` never decodes one, that is the context's job.
const TEST_TAG: u64 = 0x5A47_4352_4C43_0002;

fn write_test_object(addr: usize, size: usize, seed: u8) {
    assert!(size >= 16 && size % 8 == 0);
    // SAFETY: `addr` was handed out by `ZPageReal::alloc` for at least `size`
    // bytes inside the allocator's reservation, which outlives the test, and no
    // other thread has been given these bytes.
    unsafe {
        std::ptr::write(addr as *mut u64, size as u64);
        std::ptr::write((addr + 8) as *mut u64, TEST_TAG);
        for i in 16..size {
            std::ptr::write((addr + i) as *mut u8, seed.wrapping_add(i as u8));
        }
    }
}

/// A context that knows exactly which addresses are object bases and exactly
/// which are pinned, and **records every `object_size` call** so a test can
/// assert the gate ran first.
struct GateCtx {
    allocator: Arc<ZPageAllocator>,
    /// Registered object bases. This context is always exact: a test that wants
    /// "every address is a base" registers every address it will use.
    bases: BTreeSet<u64>,
    /// Addresses `is_pinned` answers `true` for.
    pins: Mutex<BTreeSet<u64>>,
    /// Every address `object_size` was asked about, in order.
    size_calls: Mutex<Vec<u64>>,
    /// How many times the base gate was consulted. Asserted non-zero so that a
    /// refactor which removes the gate entirely cannot pass by accident.
    base_calls: AtomicUsize,
}

impl GateCtx {
    fn new(allocator: Arc<ZPageAllocator>, bases: &[usize]) -> Self {
        Self {
            allocator,
            bases: bases.iter().map(|&a| a as u64).collect(),
            pins: Mutex::new(BTreeSet::new()),
            size_calls: Mutex::new(Vec::new()),
            base_calls: AtomicUsize::new(0),
        }
    }

    fn pin(&self, addr: usize) {
        self.pins.lock().expect("pin set").insert(addr as u64);
    }

    fn size_calls(&self) -> Vec<u64> {
        self.size_calls.lock().expect("size calls").clone()
    }

    fn asked_about(&self, addr: usize) -> bool {
        self.size_calls().contains(&(addr as u64))
    }

    fn base_calls(&self) -> usize {
        self.base_calls.load(Ordering::Relaxed)
    }
}

impl ZRelocateContext for GateCtx {
    fn object_size(&self, addr: u64) -> usize {
        self.size_calls.lock().expect("size calls").push(addr);
        // SAFETY: the tests only ever let a REGISTERED base reach this, which
        // is the property under test; `write_test_object` put the size word at
        // offset 0 of every such base.
        unsafe { std::ptr::read(addr as *const u64) as usize }
    }

    fn alloc_in(&self, _gen_hint: u8, bytes: usize, align: usize) -> Option<u64> {
        self.allocator
            .alloc_object(bytes, align)
            .ok()
            .map(|a| a as u64)
    }

    fn on_relocated(&self, _from: u64, _to: u64) {}

    fn is_object_base(&self, addr: u64) -> bool {
        self.base_calls.fetch_add(1, Ordering::Relaxed);
        self.bases.contains(&addr)
    }

    fn is_pinned(&self, addr: u64) -> bool {
        self.pins.lock().expect("pin set").contains(&addr)
    }
}

struct Fixture {
    ctx: Arc<GateCtx>,
    registry: Arc<ZForwardingRegistry>,
    relocate: Arc<ZRelocate>,
    remap: Arc<ZRemapState>,
    page: Arc<ZPageReal>,
    addrs: Vec<usize>,
}

/// A from-space page holding `count` objects of `size` bytes, claimed for the
/// relocation set with its forwarding table installed, and a context that knows
/// those `count` addresses and nothing else is a base.
///
/// The page comes from `alloc_page`, not `alloc_object`, so it is never the
/// allocator's *shared* page — which is what guarantees that to-space
/// allocations land elsewhere and can never alias the source.
fn fixture(count: usize, size: usize) -> Fixture {
    let allocator =
        Arc::new(ZPageAllocator::new(test_page_config()).expect("test geometry must validate"));
    let page = allocator
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

    let ctx = Arc::new(GateCtx::new(Arc::clone(&allocator), &addrs));
    let registry = Arc::new(ZForwardingRegistry::new());
    registry.install(
        page.id(),
        Arc::new(ZForwardingTable::for_page(page.id(), count.max(1))),
    );
    // Explicit binding: `Arc<GateCtx> -> Arc<dyn ZRelocateContext>` needs a
    // coercion site, and the method form of `clone` is the one that reaches it.
    let dyn_ctx: Arc<dyn ZRelocateContext> = ctx.clone();
    let relocate = Arc::new(ZRelocate::new(
        dyn_ctx,
        Arc::clone(&allocator),
        Arc::clone(&registry),
        ZRelocateConfig::default(),
    ));
    let remap = Arc::new(ZRemapState::new());
    remap.begin_cycle();
    assert!(remap.select_page(page.id(), count));

    Fixture {
        ctx,
        registry,
        relocate,
        remap,
        page,
        addrs,
    }
}

// ===========================================================================
// (a) interior pointers
// ===========================================================================

/// **The test the gap page asks for.** `forward(base + 8)` is refused, and
/// `object_size` is never asked about `base + 8`.
///
/// The second half is the whole point: a context that happened to answer a
/// plausible size for a mid-object byte would make a "did not crash" assertion
/// pass while the module copied 48 bytes from the middle of an object and
/// published the copy under a key no reader will ever look up.
#[test]
fn forward_refuses_an_interior_pointer_without_ever_sizing_it() {
    let f = fixture(1, 64);
    let base = f.addrs[0];
    let interior = base + 8;

    let err = f
        .relocate
        .forward(interior as u64)
        .expect_err("an interior pointer must be refused, not relocated");
    match err {
        ZRelocateError::NotAnObjectBase { from, page_id } => {
            assert_eq!(from, interior as u64);
            assert_eq!(page_id, f.page.id());
        }
        other => panic!("expected NotAnObjectBase, got {other:?}"),
    }

    assert!(
        !f.ctx.asked_about(interior),
        "object_size must NEVER be called for a non-base address — it would be \
         decoding a header at a byte that is not one, and a mid-object byte is \
         not detectably corrupt. Calls seen: {:?}",
        f.ctx.size_calls()
    );

    assert!(
        f.ctx.base_calls() > 0,
        "the base gate must actually have been consulted"
    );

    // Nothing was published, so nothing can later be adopted under the bad key.
    let table = f.registry.get(f.page.id()).expect("table installed");
    assert_eq!(
        table.entry_count(),
        0,
        "a refused interior pointer must leave the forwarding table untouched"
    );
}

/// Every interior offset of a real object is refused, not just the first —
/// including the last byte of the object and an unaligned one.
///
/// A gate implemented as "reject `base + 8`" (say, by comparing against the
/// header size) would pass the test above and fail here.
#[test]
fn every_interior_offset_is_refused() {
    const SIZE: usize = 64;
    let f = fixture(1, SIZE);
    let base = f.addrs[0];

    for off in [1usize, 7, 8, 16, 31, 32, SIZE - 8, SIZE - 1] {
        let addr = base + off;
        let err = f
            .relocate
            .forward(addr as u64)
            .expect_err("an interior offset must be refused");
        assert!(
            matches!(err, ZRelocateError::NotAnObjectBase { .. }),
            "offset {off} produced {err:?}"
        );
        assert!(!f.ctx.asked_about(addr), "offset {off} reached object_size");
    }

    // ...and the base itself still relocates, so the gate is not simply
    // refusing everything.
    let to = f
        .relocate
        .forward(base as u64)
        .expect("the base must still relocate")
        .expect("the base is inside a relocating page");
    assert_ne!(to, base as u64, "the object must have moved");
    assert!(
        f.ctx.asked_about(base),
        "the base's size IS read — the gate admits it"
    );
}

/// An interior pointer must be refused even after the real object has already
/// been relocated, i.e. when the forwarding table *does* hold an entry for the
/// page.
///
/// This is the arm where "the adopt path saves us" is most tempting and most
/// wrong: `find_payload` misses at a non-base offset, so without the gate the
/// adopt arm falls straight through to a fresh copy from mid-object.
#[test]
fn an_interior_pointer_is_refused_after_the_object_has_moved() {
    let f = fixture(1, 64);
    let base = f.addrs[0];
    let moved = f
        .relocate
        .forward(base as u64)
        .expect("base relocates")
        .expect("in a relocating page");

    let table = f.registry.get(f.page.id()).expect("table installed");
    assert_eq!(table.entry_count(), 1);

    let err = f
        .relocate
        .forward((base + 16) as u64)
        .expect_err("still refused with an entry present");
    assert!(matches!(err, ZRelocateError::NotAnObjectBase { .. }));
    assert_eq!(
        table.entry_count(),
        1,
        "no second entry may be published under an interior key"
    );

    // The real base still resolves to the same place, by adoption.
    assert_eq!(
        f.relocate.forward(base as u64).expect("adopt").unwrap(),
        moved
    );
}

// ===========================================================================
// (b) pinning
// ===========================================================================

/// A pinned object is refused by `forward`, before it is sized and before any
/// to-space is allocated.
///
/// `Pinned` is a **refusal**, not a self-forward: this module has no rung for a
/// half-evacuated page (see "Why there is no self-forwarding fallback"), so the
/// answer has to be reported rather than absorbed.
#[test]
fn forward_refuses_a_pinned_object_without_sizing_or_allocating() {
    let f = fixture(1, 64);
    let base = f.addrs[0];
    f.ctx.pin(base);

    let err = f
        .relocate
        .forward(base as u64)
        .expect_err("a pinned object must not be relocated");
    match err {
        ZRelocateError::Pinned { from, page_id } => {
            assert_eq!(from, base as u64);
            assert_eq!(page_id, f.page.id());
        }
        other => panic!("expected Pinned, got {other:?}"),
    }

    assert!(
        !f.ctx.asked_about(base),
        "a pinned object must not even be sized: {:?}",
        f.ctx.size_calls()
    );
    let table = f.registry.get(f.page.id()).expect("table installed");
    assert_eq!(table.entry_count(), 0, "nothing may be published for a pin");

    // Unpinning is not modelled (pins are per-cycle here); what matters is that
    // an UNPINNED sibling on the same page is unaffected.
    let f2 = fixture(2, 64);
    f2.ctx.pin(f2.addrs[0]);
    assert!(matches!(
        f2.relocate.forward(f2.addrs[0] as u64),
        Err(ZRelocateError::Pinned { .. })
    ));
    assert!(
        f2.relocate
            .forward(f2.addrs[1] as u64)
            .expect("the sibling is not pinned")
            .is_some(),
        "one pin must not refuse the rest of the page"
    );
}

/// `drain_page` **skips** a pinned object and evacuates the rest of the page —
/// one JNI-critical array must not abandon the other objects — but the page is
/// then reported as pinned and must not be completed.
///
/// That second half is INVARIANT ZR-1(a): a page may only be declared
/// `Relocated` once *every* live object has a published forwarding entry. A
/// skipped object has none, so its bytes are still read, and its address range
/// must stay backed.
#[test]
fn drain_page_skips_a_pin_evacuates_the_rest_and_withholds_the_page() {
    const COUNT: usize = 5;
    const SIZE: usize = 64;
    let f = fixture(COUNT, SIZE);
    let pinned = f.addrs[2];
    f.ctx.pin(pinned);

    let work = ZRelocatePage::new(Arc::clone(&f.page));
    let safepoint = ZRelocateSafepoint::new();
    let membership = safepoint.join();
    let mut local = ZRelocateLocal::new();
    let out = f
        .relocate
        .drain_page(&work, &mut local, &safepoint, &membership)
        .expect("a pin must not fail the page");
    drop(membership);

    assert_eq!(out.objects_seen, COUNT);
    assert_eq!(out.objects_pinned, 1, "exactly one pin was met");
    assert_eq!(
        out.objects_relocated,
        COUNT - 1,
        "every object but the pinned one moved"
    );
    assert!(!out.walk_aborted, "a pin is not a lost walk");

    let table = f.registry.get(f.page.id()).expect("table installed");
    assert_eq!(
        table.entry_count(),
        COUNT - 1,
        "the pinned object must have NO forwarding entry"
    );
    assert!(
        f.relocate.forward_lookup(pinned as u64).is_none(),
        "the pinned object did not move, so nothing resolves it"
    );
    for (i, &a) in f.addrs.iter().enumerate() {
        if a == pinned {
            continue;
        }
        assert!(
            f.relocate.forward_lookup(a as u64).is_some(),
            "object {i} at {a:#x} should have been evacuated around the pin"
        );
    }
}

/// `relocate_stw` reports the page as pinned rather than completed, and
/// `is_clean()` is false — so a driver that treats a clean run as licence to
/// recycle the page cannot do so.
#[test]
fn relocate_stw_reports_a_pinned_page_and_is_not_clean() {
    const COUNT: usize = 4;
    let f = fixture(COUNT, 64);
    f.ctx.pin(f.addrs[1]);

    let pages = [ZRelocatePage::new(Arc::clone(&f.page))];
    let result = f.relocate.relocate_stw(&stw(), &pages, &f.remap);

    assert_eq!(result.pages_attempted, 1);
    assert_eq!(
        result.pages_completed, 0,
        "a page with a pin is NOT completed"
    );
    assert_eq!(result.pinned_page_ids, vec![f.page.id()]);
    assert!(
        result.failed_page_ids.is_empty(),
        "a pin is not a failure: {:?}",
        result.failed_page_ids
    );
    assert!(result.aborted_walk_page_ids.is_empty());
    assert_eq!(result.worker_panics, 0);
    assert_eq!(
        result.outstanding_at_exit, 0,
        "the termination counter must still reach zero"
    );
    assert!(
        !result.is_clean(),
        "a run that left a page un-evacuated is not clean"
    );
    assert_eq!(
        result.counts.objects_relocated,
        COUNT - 1,
        "the other objects were still evacuated"
    );
}

/// With no pins and every address registered as a base, the run is clean and
/// the page completes — the control that says the two new gates are gates and
/// not a blanket refusal.
#[test]
fn an_unpinned_page_of_real_bases_still_relocates_cleanly() {
    const COUNT: usize = 6;
    let f = fixture(COUNT, 48);
    let pages = [ZRelocatePage::new(Arc::clone(&f.page))];
    let result = f.relocate.relocate_stw(&stw(), &pages, &f.remap);

    assert!(result.is_clean(), "clean run expected: {result:?}");
    assert_eq!(result.pages_completed, 1);
    assert!(result.pinned_page_ids.is_empty());
    assert_eq!(result.counts.objects_relocated, COUNT);
    assert_eq!(
        f.registry
            .get(f.page.id())
            .expect("table installed")
            .entry_count(),
        COUNT
    );
}
