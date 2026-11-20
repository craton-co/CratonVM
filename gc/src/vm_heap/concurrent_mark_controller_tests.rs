use super::*;
use cratonvm_gc_g1::g1::G1CollectorConfig;

/// Test-only `StopTheWorldToken` (I-17). Single-threaded test harness, so
/// the STW invariant the token witnesses is trivially satisfied.
#[inline]
fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: single-threaded test harness; no mutator is running.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn make_g1_heap() -> VmHeap {
    // Small heap — fast to construct, big enough for the few
    // allocations these tests perform.
    let mut cfg = G1CollectorConfig::default();
    cfg.heap_size = 4 * 1024 * 1024;
    cfg.region_size = 1024 * 1024;
    crate::vm_heap::VmHeap::from_inner(VmHeapInner::G1(G1State::new(cfg)))
}

/// Helper: extract the G1State for white-box assertions on the
/// controller slot. Returns `None` for a generational heap.
fn g1_state(heap: &VmHeap) -> Option<&G1State> {
    match &heap.inner {
        VmHeapInner::G1(s) => Some(s),
        _ => None,
    }
}

/// **R6, the `VmHeapInner::Zgc` arm audit, verified against a cycle that
/// actually moves an object.**
///
/// The production plan's R6 row says: *"58 arms plus a macro; the
/// affirmative ones (`true`, `(0,0)`) assert facts that are only true for
/// a non-moving collector, and none of them will fail loudly when they
/// become wrong."* Compaction (`CRATONVM_ZGC_RELOCATE=1`, 2026-08-13) is
/// the change that can make them wrong, so the audit is no longer
/// theoretical.
///
/// The audit's finding is that the two arms which genuinely take a
/// **pre-GC address** — `watched_pre_gc_addr_survived` and
/// `pre_gc_addr_did_not_survive` — are already correct, because both
/// consult the `pointer_map` *before* they reach their ZGC arm. That is a
/// property worth a test rather than a reading: the ZGC arms themselves
/// (`is_addr_live`, a registry lookup) answer about the address as it is
/// *now*, and after a slide the old address holds zeroed bytes. Without
/// the map check, a survivor that moved would be reported dead at its old
/// address, and `process_references_after_gc` would drop every reference
/// to it — the exact H2/HIB-CV-32 shape those predicates were written for.
///
/// The exact edit that trips this: delete either
/// `pointer_map.contains_key(&addr)` early return.
/// The `Zgc` arm of [`VmHeap::refill_tlab`] returned `None` until
/// 2026-09-02, which left the JIT's inline allocator dead on the default
/// collector: `thread.tlab` stayed empty, the inline bump missed every
/// time, and every compiled `new` took the helper. It carves a chunk now,
/// and retiring the buffer hands the unused tail back rather than leaving
/// a filler object this collector would never reclaim (it sweeps a
/// registry, not memory).
#[cfg(feature = "zgc")]
#[test]
fn the_zgc_arm_of_refill_tlab_carves_a_chunk_and_takes_its_tail_back() {
    let heap = VmHeap::from_inner(crate::vm_heap::VmHeapInner::Zgc(cratonvm_gc_zgc::zgc::ZgcRealHeap::new_shared(16 * 1024 * 1024)));
    if let VmHeapInner::Zgc(z) = &heap.inner {
        z.set_vm_tlab_enabled(true);
    }
    let (ptr, size) = heap
        .refill_tlab(64 * 1024)
        .expect("ZGC must hand the VM thread's TLAB a chunk");
    assert!(!ptr.is_null());
    assert!(
        size >= cratonvm_gc_core::tlab::min_tlab_size() && size <= 64 * 1024,
        "chunk of {size} bytes is outside the requested bounds"
    );
    let mut tlab = unsafe { crate::Tlab::new(ptr, size) };
    assert!(tlab.alloc(128, 8).is_some());
    // The chunk is charged whole at refill; the retire credits the unused
    // tail back, so `allocated` never counts bytes nobody can reach.
    let allocated_before = heap.allocated_bytes();
    tlab.retire();
    assert!(tlab.is_retired());
    assert_eq!(
        allocated_before - heap.allocated_bytes(),
        size - 128,
        "the retired tail must be credited back to the heap"
    );
    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    assert_eq!(
        z.vm_tlab_engagement(),
        (1, size, 1, size - 128),
        "(refills, refill_bytes, tails_returned, tail_bytes_returned)"
    );
}

#[cfg(feature = "zgc")]
#[test]
fn the_pre_gc_address_predicates_are_correct_for_an_object_compaction_moved() {
    let heap = VmHeap::from_inner(crate::vm_heap::VmHeapInner::Zgc(cratonvm_gc_zgc::zgc::ZgcRealHeap::new_shared(256 * 1024)));
    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    z.set_tlab_enabled(false);
    // Garbage below, so the survivor has somewhere to slide to.
    for _ in 0..8 {
        z.alloc_object(ClassId::new(1), 4);
    }
    let survivor = z.alloc_object(ClassId::new(1), 0);
    let old_addr = survivor.as_ptr() as usize;

    let live = [old_addr];
    let (moved, _reclaimed, map) = z.relocate_stw_for_test(&live);
    assert_eq!(moved, 1, "the fixture must actually move the survivor");
    assert!(map.contains_key(&old_addr));

    // The address as it is NOW: nothing live is there any more.
    assert!(
        !heap.is_addr_live(old_addr),
        "the old address must not read as live once the object has left it"
    );

    // ...and yet both pre-GC predicates must get the answer RIGHT, because
    // each consults the pointer map before its ZGC arm.
    assert!(
        heap.watched_pre_gc_addr_survived(old_addr, &map),
        "a survivor that MOVED must still count as having survived; \
         without the pointer-map check this reads as death and every \
         reference to it is dropped"
    );
    assert!(
        !heap.pre_gc_addr_did_not_survive(old_addr, &map),
        "and its negation must agree — these two must never disagree \
         about the same address"
    );
}

/// The same two predicates must still report a genuinely dead address as
/// dead when a compacting cycle ran.
///
/// Without this, the test above is satisfied by a predicate that answers
/// "survived" for everything -- which would disable the stale-pointer
/// protection entirely rather than fix it.
///
/// This one goes through the real `collect_garbage` rather than calling
/// the relocator directly, and it has to: an object is only *dead* once
/// the sweep has removed it from the registry, and `is_addr_live` is a
/// registry lookup. Driving the relocator alone leaves every allocation
/// still registered, so a "dead" address reads as live and the test fails
/// for a fixture reason that says nothing about the predicate. That was
/// this test's first draft.
/// Monitor-cleanup stub for the compaction fixtures.
#[cfg(feature = "zgc")]
struct R6NoMonitors;
#[cfg(feature = "zgc")]
impl MonitorCleanup for R6NoMonitors {
    fn remap_after_gc(&self, _map: &cratonvm_types::PointerMap) {}
    fn prune_dead(&self, _dead: &[usize]) {}
}


/// **The `VmHeap::satb_barrier` ZGC arm actually reaches the collector.**
///
/// This is the test that separates a wired barrier from an inert one, and
/// it is the only one that can: `ZgcRealHeap`'s own barrier tests call
/// `satb_pre_barrier` directly, so every one of them would still pass with
/// this arm back to the `{}` it was until 2026-08-13. The dispatch is the
/// subject here, not the barrier.
///
/// The exact edit that trips it: empty the `VmHeapInner::Zgc` arm of
/// `satb_barrier`.
#[cfg(feature = "zgc")]
#[test]
fn the_vm_heap_satb_arm_reaches_the_zgc_barrier() {
    let heap = VmHeap::from_inner(crate::vm_heap::VmHeapInner::Zgc(cratonvm_gc_zgc::zgc::ZgcRealHeap::new_shared(64 * 1024)));
    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    let obj = z.alloc_object(ClassId::new(1), 4);
    z.set_mark_active(true);

    // Through the VM-facing funnel every reference store already calls.
    heap.satb_barrier(Value::Object(Some(obj)));

    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    assert_eq!(
        z.mark_ingress_pushes(),
        1,
        "VmHeap::satb_barrier must reach ZgcRealHeap::satb_pre_barrier"
    );
}

/// `set_field_suppress_satb` really suppresses on ZGC, and plain
/// `set_field` really does not.
///
/// # The bug this exists to have caught
///
/// That method was a plain `set_field` on this backend, and correctly so
/// for as long as ZGC had no armed pre-write barrier. Concurrent marking
/// gave it one. `weakref_null_referents_pre_gc` nulls EVERY registered
/// referent immediately before `collect_garbage` -- i.e. while the cycle is
/// still armed -- so the unsuppressed arm would have published every active
/// referent into the ingress, `finish_concurrent_mark` would have replayed
/// them as mark roots, and no weak, soft, phantom or cleaner reference
/// could ever have been cleared again.
///
/// It would have been invisible. Every reference test in this tree is
/// satisfied by "the referent survived", which is exactly what the bug
/// produces; the only symptom is a leak.
///
/// Both directions are asserted. A suppression that suppressed everything
/// -- including the ordinary store path -- would pass the half of this test
/// that matters most and disable the barrier wholesale.
#[cfg(feature = "zgc")]
#[test]
fn zgc_set_field_suppress_satb_suppresses_and_plain_set_field_does_not() {
    let heap = VmHeap::new(GcBackend::Zgc, 1024 * 1024);
    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    let holder = heap.alloc_object(ClassId::new(1), 2);
    let a = heap.alloc_object(ClassId::new(1), 0);
    let b = heap.alloc_object(ClassId::new(1), 0);
    heap.set_field(holder, 0, Value::Object(Some(a)));
    heap.set_field(holder, 1, Value::Object(Some(b)));
    z.set_mark_active(true);

    let before = z.mark_ingress_pushes();
    heap.set_field_suppress_satb(holder, 0, Value::Object(None));
    assert_eq!(
        z.mark_ingress_pushes(),
        before,
        "the referent-protocol write must NOT reach the concurrent marker"
    );

    let before = z.mark_ingress_pushes();
    heap.set_field(holder, 1, Value::Object(None));
    assert_eq!(
        z.mark_ingress_pushes(),
        before + 1,
        "...while an ordinary store still must, or the suppression has \
         disabled the barrier rather than exempted one caller"
    );

    z.set_mark_active(false);
}

/// **The card barrier is reached through `VmHeap`, by BOTH store channels.**
///
/// # Why through the enum and not through `ZgcRealHeap`
///
/// The card barrier's whole history is of being wired to something nothing
/// calls. Until 2026-08-17 it hung off `GarbageCollector::write_barrier`,
/// which this backend's `set_field` never invokes -- so `remembered_roots`
/// had no non-test caller and the remembered set was empty on every real
/// workload, while the collector-level tests were green. The tests in
/// `zgc.rs` cannot see that: they call the accessor directly. This one goes
/// through the dispatch the interpreter goes through.
///
/// Both channels, because `set_field_suppress_satb` exists to skip the OTHER
/// barrier and must not skip this one: SATB is about a reference being LOST,
/// a card is about one now being HELD. A single implementation change
/// (routing the suppression channel around `set_field_no_satb`) would break
/// exactly one of the two assertions below.
#[cfg(feature = "zgc")]
#[test]
fn the_vm_heap_store_channels_both_reach_the_zgc_card_barrier() {
    let heap = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    z.set_generational_enabled(true);
    z.set_gen_promotion_age(1);
    z.set_relocation_enabled(false);

    let plain = heap.alloc_object(ClassId::new(1), 2);
    let suppressed = heap.alloc_object(ClassId::new(1), 2);
    let target = heap.alloc_object(ClassId::new(1), 0);

    // Promote all three, then let a young cycle clean the promotion cards --
    // otherwise the assertions read a set that is dirty for a reason they
    // did not cause.
    let mut roots = [plain, suppressed, target];
    {
        // SAFETY: single-threaded test.
        let stw = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
        let _ = z.collect_garbage(&stw, &mut roots, &R6NoMonitors);
        let _ = z.collect_garbage(&stw, &mut roots, &R6NoMonitors);
    }
    let [plain, suppressed, target] = roots;
    assert!(
        !z.is_carded_for_test(plain.as_ptr() as usize)
            && !z.is_carded_for_test(suppressed.as_ptr() as usize),
        "the young cycle must have cleaned the promotion cards first"
    );

    heap.set_field(plain, 0, Value::Object(Some(target)));
    assert!(
        z.is_carded_for_test(plain.as_ptr() as usize),
        "an ordinary store through VmHeap must card its receiver"
    );

    heap.set_field_suppress_satb(suppressed, 0, Value::Object(Some(target)));
    assert!(
        z.is_carded_for_test(suppressed.as_ptr() as usize),
        "...and so must the SATB-suppressed channel: suppressing the \\
         snapshot barrier must not suppress the card, or every referent \\
         write silently drops an old-to-young edge"
    );
}

/// Every ZGC collection through `VmHeap` records a collector decision, and
/// the decision agrees with what the cycle actually did.
///
/// Before 2026-09-23 ZGC recorded nothing, so the default backend's
/// `collector_decision_report()` was empty however much it collected.
/// Asserted as AGREEMENT with the slide counter rather than as a fixed
/// verdict: whether a given cycle compacts depends on the cost gate and the
/// relocation defaults, and a test pinned to one of those would be testing
/// the gate, not the record.
#[cfg(feature = "zgc")]
#[test]
fn every_zgc_collection_records_a_decision_that_matches_the_slide() {
    use cratonvm_gc_core::gc_metrics::{decision_reason, last_collector_decision};
    let heap = VmHeap::new(GcBackend::Zgc, 8 * 1024 * 1024);
    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    let keep = heap.alloc_object(ClassId::new(1), 1);
    for _ in 0..256 {
        let _ = heap.alloc_object(ClassId::new(1), 4);
    }
    let seq_before = last_collector_decision().map_or(0, |d| d.sequence);
    let compactions_before = z.feature_engagement().1;
    // SAFETY: this test is the only mutator.
    let stw = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
    let mut roots = [keep];
    let _ = heap.collect_garbage(&stw, &mut roots, &R6NoMonitors);

    let d = last_collector_decision().expect("a ZGC collection must record a decision");
    assert_eq!(
        d.sequence,
        seq_before + 1,
        "exactly one record per collection"
    );
    assert_eq!(d.backend, "zgc");
    let compacted = z.feature_engagement().1 > compactions_before;
    assert_eq!(
        d.young_moving,
        compacted,
        "the record must say MOVING iff the slide ran (reason={})",
        decision_reason::label(d.reason),
    );
    if compacted {
        assert_eq!(d.reason, decision_reason::MOVING_BACKEND_COMPACTED);
    }

    // ...and the finalizer-aware door records too: production reaches the
    // collector through it, not through `collect_garbage`.
    let _ = heap.collect_garbage_with_finalizers(&stw, &mut roots, &[], &R6NoMonitors);
    let d2 = last_collector_decision().expect("recorded");
    assert_eq!(d2.sequence, seq_before + 2);
    assert_eq!(d2.backend, "zgc");
}

/// Every `zgc_*_concurrent_mark` arm of `VmHeap` reaches the collector.
///
/// # Why this is a separate test from the ones in `zgc.rs`
///
/// Those exercise `ZgcRealHeap` directly. This exercises the **dispatch**,
/// and in this enum the dispatch is where a feature goes quietly missing:
/// every one of these methods has two arms that are a literal `false` /
/// `{}` / `(0, 0, 0, 0, 0)`, and a fifth arm that reads
/// `VmHeapInner::Zgc(_) => false` compiles, passes every collector-level test,
/// and turns the whole feature off. This tree has shipped exactly that
/// shape before — an inert registration is indistinguishable from a
/// missing feature from anywhere except the call site.
///
/// The exact edit that trips it: change any `VmHeapInner::Zgc(h) => h.…` arm in
/// the ZGC concurrent-marking block to the neutral value its siblings use.
#[cfg(feature = "zgc")]
#[test]
fn the_vm_heap_zgc_concurrent_arms_reach_the_collector() {
    let heap = VmHeap::new(GcBackend::Zgc, 8 * 1024 * 1024);
    assert!(!heap.zgc_concurrent_mark_active());
    assert_eq!(heap.zgc_concurrent_mark_stats(), (0, 0, 0, 0, 0));

    let holder = heap.alloc_object(ClassId::new(1), 2);
    let child = heap.alloc_object(ClassId::new(1), 0);
    heap.set_field(holder, 0, Value::Object(Some(child)));
    let garbage = heap.alloc_object(ClassId::new(1), 0);
    let garbage_addr = garbage.as_ptr() as usize;

    // SAFETY: this test is the only mutator.
    let stw = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
    assert!(
        heap.zgc_start_concurrent_mark(&stw, &[holder]),
        "the VmHeap arm must open a cycle, not return a neutral false"
    );
    assert!(heap.zgc_concurrent_mark_active());

    // The mutator ingress, through the funnel every reference store in the
    // VM already reaches.
    let extra = heap.alloc_object(ClassId::new(1), 0);
    heap.set_field(holder, 1, Value::Object(Some(extra)));
    heap.satb_barrier(Value::Object(Some(child)));

    let mut roots = [holder];
    let _ = heap.collect_garbage(&stw, &mut roots, &R6NoMonitors);

    let (started, completed, black, _replayed, _ns) = heap.zgc_concurrent_mark_stats();
    assert_eq!(
        (started, completed),
        (1, 1),
        "the cycle must have been opened AND certified through the VmHeap arms"
    );
    assert!(black >= 1, "the object allocated mid-cycle was born marked");
    assert!(!heap.zgc_concurrent_mark_active());

    assert!(
        heap.is_object_address(child.as_ptr() as usize).is_some(),
        "the live child survived a concurrently-marked collection"
    );
    assert!(
        heap.is_object_address(garbage_addr).is_none(),
        "...and the garbage did not, so this is not just 'nothing was freed'"
    );
}

/// ...and the same funnel is inert on ZGC while no cycle is marking, which
/// is what makes it free to leave wired in every build.
#[cfg(feature = "zgc")]
#[test]
fn the_vm_heap_satb_arm_is_inert_on_zgc_while_not_marking() {
    let heap = VmHeap::from_inner(crate::vm_heap::VmHeapInner::Zgc(cratonvm_gc_zgc::zgc::ZgcRealHeap::new_shared(64 * 1024)));
    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    let obj = z.alloc_object(ClassId::new(1), 4);

    heap.satb_barrier(Value::Object(Some(obj)));

    let VmHeapInner::Zgc(z) = &heap.inner else {
        unreachable!("constructed as Zgc")
    };
    assert_eq!(z.mark_ingress_pushes(), 0);    }

/// The `-XX:` G1 knobs flow config → `G1ConfigOverrides` →
/// `new_with_overrides` → `G1CollectorConfig`. region_size is observable via
/// the region count; the other overrides take the identical match-arm path.
#[test]
fn g1_config_overrides_apply() {
    // Default (1 MiB regions): 8 MiB / 1 MiB = 8 regions.
    let h = VmHeap::new(GcBackend::G1, 8 * 1024 * 1024);
    assert_eq!(g1_state(&h).unwrap().num_regions(), 8);
    // -XX:G1HeapRegionSize=2m override: 8 MiB / 2 MiB = 4 regions.
    let ov = G1ConfigOverrides {
        region_size: Some(2 * 1024 * 1024),
        ..Default::default()
    };
    let h2 = VmHeap::new_with_overrides(GcBackend::G1, 8 * 1024 * 1024, ov);
    assert_eq!(
        g1_state(&h2).unwrap().num_regions(),
        4,
        "G1HeapRegionSize override must change the region count"
    );
    // Generational backend ignores G1 overrides (no panic / no effect).
    let _ = VmHeap::new_with_overrides(GcBackend::Generational, 8 * 1024 * 1024, ov);
}

/// gci i11/gm finding 13 — an explicit `-XX:G1HeapRegionSize` follows
/// HotSpot 25 (measured on 25.0.3 with `-Xmx2g`): below 1 MiB rounds up to
/// 1 MiB, a non-power of two rounds up, 64 MiB stays 64 MiB (the ergonomic
/// 32 MiB cap does not apply), 512 MiB is the ceiling.
#[test]
fn gci_i11gm_explicit_region_size_follows_hotspot_bounds() {
    const MIB: usize = 1024 * 1024;
    let heap = 2048 * MIB;
    assert_eq!(explicit_region_size(512 * 1024, heap), MIB);
    assert_eq!(explicit_region_size(1, heap), MIB);
    assert_eq!(explicit_region_size(MIB, heap), MIB);
    assert_eq!(explicit_region_size(3 * MIB, heap), 4 * MIB);
    assert_eq!(explicit_region_size(64 * MIB, heap), 64 * MIB);
    assert_eq!(explicit_region_size(512 * MIB, heap), 512 * MIB);
    assert_eq!(explicit_region_size(300 * MIB, heap), 512 * MIB);
    // Above the ceiling (the launcher refuses first; the clamp is a backstop).
    assert_eq!(explicit_region_size(1024 * MIB, heap), 512 * MIB);
    // Never larger than the heap's largest power of two (one region at least).
    assert_eq!(explicit_region_size(64 * MIB, 48 * MIB), 32 * MIB);
    assert_eq!(explicit_region_size(4 * MIB, 8 * MIB), 4 * MIB);
    // The ergonomic arm keeps HotSpot's 32 MiB ergonomic ceiling.
    assert_eq!(g1_ergonomic_region_size(1usize << 40), G1_MAX_REGION_SIZE);
}

/// gci i11/gm finding 13 — end to end through the constructor: `512k` gives
/// 1 MiB regions, as on HotSpot.
#[test]
fn gci_i11gm_a_sub_mib_explicit_region_size_builds_one_mib_regions() {
    let ov = G1ConfigOverrides {
        region_size: Some(512 * 1024),
        ..Default::default()
    };
    let h = VmHeap::new_with_overrides(GcBackend::G1, 8 * 1024 * 1024, ov);
    assert_eq!(g1_state(&h).unwrap().num_regions(), 8);
}

/// `-Xms` reaches every backend arm of the constructor, each arm says what
/// it did with it, and as of 2026-09-21 every arm COMMITS.
///
/// Two `Dropped` answers were the finding this test was written to state
/// (`docs/feature-designs/generative-gc-architecture.md`);
/// both are gone. The shape is what made that edit a one-line change — the
/// previous spelling, a field on `G1ConfigOverrides` that two arms could
/// not read, had nothing to edit — so the assertions stay here rather than
/// being retired with the page.
#[test]
fn every_backend_reports_what_it_did_with_xms() {
    let xmx = 8 * 1024 * 1024;
    let xms = 4 * 1024 * 1024;
    let ov = G1ConfigOverrides::default();

    for backend in backends_under_test() {
        // Absent and zero are both "nothing was asked for", never a commit
        // of nothing.
        for none_ish in [None, Some(0usize)] {
            let (_, d) = VmHeap::new_with_heap_sizing(backend, xmx, none_ish, ov);
            assert_eq!(
                d,
                XmsDisposition::NotSupplied,
                "{backend:?}: an absent -Xms must not read as a commit",
            );
        }

        let (_, d) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xms), ov);
        assert_eq!(
            d,
            XmsDisposition::Committed(xms),
            "{backend:?}: -Xms must be honoured, not dropped",
        );

        // `-Xms` above `-Xmx` is clamped, not refused.
        let (_, d) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xmx * 4), ov);
        assert_eq!(
            d,
            XmsDisposition::Committed(xmx),
            "{backend:?}: an oversized -Xms is clamped to -Xmx",
        );
    }
}

/// The backends this build has. ZGC is feature-gated, and a loop over a
/// hardcoded pair would silently stop covering it.
fn backends_under_test() -> Vec<GcBackend> {
    let mut v = vec![GcBackend::Generational, GcBackend::G1];
    #[cfg(feature = "zgc")]
    v.push(GcBackend::Zgc);
    v
}

/// `-Xms` moves COMMITTED BYTES, on every backend, and does not move the
/// heap's capacity.
///
/// The disposition test above asserts what each arm *says*. This one
/// asserts that the saying is backed: a `Committed(n)` that committed
/// nothing would pass that test and would be exactly the silent drop the
/// known-issues page was opened about.
///
/// # Why the assertion is a comparison and not a threshold
///
/// How many bytes a fresh heap has committed depends on the store
/// (`CRATONVM_GC_RESERVE=0` gives a wholly-committed one, where every arena
/// starts at its capacity and nothing can rise), on the granule size, and
/// on what the constructor itself touches. So the test builds two heaps of
/// the SAME `-Xmx` and requires the `-Xms` one to be committed at least as
/// much — never less — and requires `-Xms` to leave `heap_capacity`
/// untouched, which is the half that would break if a backend "honoured"
/// the flag by sizing its reservation from it.
#[test]
fn xms_commits_bytes_without_resizing_the_heap() {
    let xmx = 64 * 1024 * 1024;
    let xms = 32 * 1024 * 1024;
    let ov = G1ConfigOverrides::default();

    for backend in backends_under_test() {
        let (cold, _) = VmHeap::new_with_heap_sizing(backend, xmx, None, ov);
        let (warm, d) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xms), ov);
        assert_eq!(d, XmsDisposition::Committed(xms));

        // g1sz: G1's `heap_capacity` is the sizing policy's soft capacity
        // (default ON since 2026-10-26), which starts at `-Xms` exactly as
        // HotSpot's committed heap does; the heap -Xms must not size is
        // its `-Xmx` grid.
        let max_capacity = |h: &VmHeap| match &h.inner {
            VmHeapInner::G1(g) => g.collector.heap_target_bytes_within(false),
            _ => h.heap_capacity(),
        };
        assert_eq!(
            max_capacity(&warm),
            max_capacity(&cold),
            "{backend:?}: -Xms must size the startup COMMIT, never the heap. A capacity that moved with -Xms is the \"alias for -Xmx\" mistake: -Xms512m -Xmx8g would allocate 512m and then fail.",
        );
        assert!(
            warm.os_committed_bytes() >= cold.os_committed_bytes(),
            "{backend:?}: -Xms committed {} bytes, no more than the {} a heap with no -Xms at all starts with — the flag is being accepted and dropped",
            warm.os_committed_bytes(),
            cold.os_committed_bytes(),
        );

        // On a RESERVING store the `>=` above is not enough: a backend that
        // dropped the flag would satisfy it with equality, which is the
        // defect. The precondition is the store's own: a heap that already
        // has its whole capacity committed cannot commit more, and the
        // wholly-committed fallback (`CRATONVM_GC_RESERVE=0`, and any
        // platform without a reservation) is exactly that heap.
        if cold.os_committed_bytes() < cold.heap_capacity() {
            assert!(
                warm.os_committed_bytes() > cold.os_committed_bytes(),
                "{backend:?}: the store reserves ({} of {} committed with no -Xms), so an -Xms of {xms} must raise the committed bytes; it left them at {}",
                cold.os_committed_bytes(),
                cold.heap_capacity(),
                warm.os_committed_bytes(),
            );
        }
    }
}

#[test]
fn start_then_signal_complete_spawns_and_joins_controller() {
    let heap = make_g1_heap();

    // Before start: no controller parked.
    assert!(
        !g1_state(&heap).unwrap().has_active_controller(),
        "fresh heap should have no controller",
    );

    // Start: phase flips to ConcurrentMark and a controller is
    // installed.
    heap.g1_start_concurrent_mark(&stw());
    assert!(
        heap.g1_is_marking_active(),
        "phase must be ConcurrentMark after start"
    );
    assert!(
        g1_state(&heap).unwrap().has_active_controller(),
        "controller must be parked after g1_start_concurrent_mark",
    );

    // Complete: drains the slot, joins the worker, runs cleanup,
    // phase returns to Idle.
    heap.g1_signal_marking_complete(&stw());
    assert!(
        !g1_state(&heap).unwrap().has_active_controller(),
        "controller slot must be drained after g1_signal_marking_complete",
    );
    assert!(
        !heap.g1_is_marking_active(),
        "phase must return to Idle after cleanup",
    );
}

#[test]
fn repeated_cycles_do_not_leak_threads() {
    // Each cycle spawns and joins a worker; running many cycles
    // back-to-back must not accumulate join handles or Arc clones.
    let heap = make_g1_heap();
    let collector_arc_before = Arc::strong_count(&g1_state(&heap).unwrap().collector);

    for _ in 0..10 {
        heap.g1_start_concurrent_mark(&stw());
        heap.g1_signal_marking_complete(&stw());
    }

    // After every cycle has joined, the only strong reference to
    // the collector must be the one VmHeap holds. A leaked worker
    // thread would still be holding a clone and bump this count.
    let collector_arc_after = Arc::strong_count(&g1_state(&heap).unwrap().collector);
    assert_eq!(
        collector_arc_before, collector_arc_after,
        "Arc<G1Collector> ref count leaked across cycles \
         (before={collector_arc_before}, after={collector_arc_after})",
    );
    assert!(
        !g1_state(&heap).unwrap().has_active_controller(),
        "no controller should remain after the final complete",
    );
}

#[test]
fn double_start_joins_stale_controller_before_spawning_new_one() {
    // Edge case: calling g1_start_concurrent_mark twice without a
    // complete in between. The implementation joins the stale
    // controller (logging a warning) and replaces it. The phase
    // remains ConcurrentMark; the new controller is parked.
    let heap = make_g1_heap();

    heap.g1_start_concurrent_mark(&stw());
    let first_arc_count = Arc::strong_count(&g1_state(&heap).unwrap().collector);

    // Second start without complete in between — must not leak.
    heap.g1_start_concurrent_mark(&stw());
    assert!(
        g1_state(&heap).unwrap().has_active_controller(),
        "a controller must still be parked after the second start",
    );

    // Single signal-complete drains everything.
    heap.g1_signal_marking_complete(&stw());
    let final_arc_count = Arc::strong_count(&g1_state(&heap).unwrap().collector);
    assert!(
        final_arc_count <= first_arc_count,
        "double start should not strand additional Arc clones \
         (first_active={first_arc_count}, final={final_arc_count})",
    );
    assert!(
        !g1_state(&heap).unwrap().has_active_controller(),
        "controller slot must be empty after complete",
    );
}

#[test]
fn signal_complete_without_active_controller_is_noop() {
    // Defensive: calling g1_signal_marking_complete before any
    // g1_start_concurrent_mark must not panic, must leave the
    // collector in Idle, and must not install a controller.
    let heap = make_g1_heap();

    assert!(!heap.g1_is_marking_active());
    heap.g1_signal_marking_complete(&stw()); // no-op path
    assert!(!heap.g1_is_marking_active());
    assert!(!g1_state(&heap).unwrap().has_active_controller());

    // Calling it twice (after a real cycle then a stray call) must
    // also be a no-op.
    heap.g1_start_concurrent_mark(&stw());
    heap.g1_signal_marking_complete(&stw());
    heap.g1_signal_marking_complete(&stw()); // stray second call
    assert!(!heap.g1_is_marking_active());
}

/// gci i15/ca: the full collection's abort joins the marker, leaves the
/// marking phase, closes the SATB queue and strands no worker; a second
/// abort (and one with no cycle) does nothing; a fresh cycle then starts
/// and completes normally. Filter: `-p cratonvm-gc gci_i15ca`.
#[test]
fn gci_i15ca_abort_joins_the_marker_and_a_fresh_cycle_follows() {
    let heap = make_g1_heap();
    assert!(!heap.g1_abort_concurrent_mark(&stw()), "no cycle: nothing to abort");
    let arc_before = Arc::strong_count(&g1_state(&heap).unwrap().collector);

    heap.g1_start_concurrent_mark(&stw());
    assert!(heap.g1_is_marking_active());
    assert!(g1_state(&heap).unwrap().has_active_controller());

    assert!(heap.g1_abort_concurrent_mark(&stw()), "the open cycle is aborted");
    assert!(!heap.g1_is_marking_active(), "abort leaves the marking phase");
    assert!(
        !g1_state(&heap).unwrap().has_active_controller(),
        "abort joins and drops the marker"
    );
    assert!(
        !g1_state(&heap).unwrap().collector.satb_queue().is_active(),
        "abort closes the SATB queue"
    );
    assert_eq!(
        Arc::strong_count(&g1_state(&heap).unwrap().collector),
        arc_before,
        "the joined worker holds no collector clone"
    );
    assert!(!heap.g1_abort_concurrent_mark(&stw()), "a second abort does nothing");

    // The full collection's own cycle: starts, marks, completes.
    heap.g1_start_concurrent_mark(&stw());
    assert!(heap.g1_is_marking_active());
    assert!(g1_state(&heap).unwrap().collector.satb_queue().is_active());
    heap.g1_signal_marking_complete(&stw());
    assert!(!heap.g1_is_marking_active());
    assert!(!g1_state(&heap).unwrap().has_active_controller());
}

/// gci i15/ca: nothing the abort leaves behind gives a verdict from the
/// half-built cycle. A logged SATB entry is discarded (the queue is empty
/// and closed), the referent-slot skip set is dropped, and
/// `is_live_after_mark` answers LIVE for an object the aborted cycle never
/// marked (the mark-start snapshot is gone, so no bitmap bit is read).
#[test]
fn gci_i15ca_abort_leaves_no_half_built_mark_state() {
    let heap = make_g1_heap();
    let holder = heap.alloc_object(cratonvm_types::ClassId::new(1), 0);
    let unmarked = heap.alloc_object(cratonvm_types::ClassId::new(2), 0);
    let holder_addr = holder.as_ptr() as usize;
    let unmarked_addr = unmarked.as_ptr() as usize;
    heap.g1_start_concurrent_mark_with_reference_skip(&stw(), &[holder_addr]);
    let state = g1_state(&heap).unwrap();
    assert_eq!(state.collector.dbg_reference_skip_len(), 1);
    heap.satb_barrier(Value::Object(Some(unmarked)));
    heap.flush_thread_satb();

    assert!(heap.g1_abort_concurrent_mark(&stw()));
    let queue = state.collector.satb_queue();
    assert!(!queue.is_active());
    assert_eq!(queue.len(), 0, "the aborted cycle's SATB entries are discarded");
    assert_eq!(state.collector.dbg_reference_skip_len(), 0, "the skip set is per cycle");
    assert!(
        state.collector.is_live_after_mark(unmarked_addr),
        "no dead verdict from an aborted cycle's bitmap"
    );
    assert!(state.collector.is_live_after_mark(holder_addr));
}

#[test]
fn satb_pre_barrier_captured_during_concurrent_phase() {
    // SATB sanity: while the concurrent phase is active (between
    // g1_start_concurrent_mark and g1_signal_marking_complete) the
    // satb_barrier path must enqueue overwritten references. This
    // is the same property the #54-era marker tests assert at the
    // ConcurrentMarker level — here we exercise it through the
    // VmHeap public API to prove the wiring actually drives SATB.
    let heap = make_g1_heap();

    // Allocate an object inside G1 to use as a "previously live"
    // reference that mutators overwrite during marking.
    let obj = heap.alloc_object(cratonvm_types::ClassId::new(1), 0);

    heap.g1_start_concurrent_mark(&stw());

    // Before the overwrite, the SATB queue is empty (initial-mark
    // activated it but nothing was logged yet).
    let satb_queue = g1_state(&heap).unwrap().collector.satb_queue().clone();
    assert!(
        satb_queue.is_active(),
        "SATB must be active during concurrent mark"
    );
    let before = satb_queue.len();

    // Simulate a mutator overwrite: barrier sees the old value.
    heap.satb_barrier(Value::Object(Some(obj)));
    // The pre-barrier writes through the thread-local SATB buffer, so the
    // global queue may not see it yet. Flush this thread's buffer.
    heap.flush_thread_satb();

    // FLAKE FIX: `after > before` alone is a race, and it is the same
    // under-approximation `g1_concurrent::satb_captures_mutator_writes_during_concurrent_mark`
    // already documents. `g1_start_concurrent_mark()` above spawns a real
    // background marker, and since G1MARK-6 `concurrent_mark_step` drains
    // the queue shards on every step — so the worker can consume the entry
    // between the flush and this read, leaving `after == before == 0` on a
    // queue that did exactly what it was supposed to. Measured at roughly
    // one failure in twenty full-suite runs on a loaded host; it needs the
    // worker scheduled inside a sub-millisecond window.
    //
    // Assert the SATB *guarantee* instead of the transient: the overwritten
    // reference reached the marker either by still being queued, or by
    // already having been pulled into the gray set / marked. Both routes
    // are the barrier working; only neither is a bug.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    let mut condition = false;
    let mut final_after = 0;
    let mut final_grayed = false;
    while std::time::Instant::now() < deadline {
        let after = satb_queue.len();
        let grayed_or_marked = g1_state(&heap)
            .unwrap()
            .collector
            .dbg_is_grayed_or_marked(obj.as_ptr() as usize);
        final_after = after;
        final_grayed = grayed_or_marked;
        if after > before || grayed_or_marked {
            condition = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        condition,
        "satb_barrier during concurrent mark must deliver the old reference \
         to the marker (before={before}, after={final_after}, \
         grayed_or_marked={final_grayed})",
    );

    // Cleanup so we don't strand the worker.
    heap.g1_signal_marking_complete(&stw());
}

/// Residual humongous OOB regression: `read_char_array_bulk` on a G1
/// humongous char[] must read every code unit correctly through the
/// region-aware path, never off the end of the start region's buffer.
///
/// With `region_size = 1 MiB` the per-region usable payload is just under
/// 1 MiB, so a char[] of 600_000 elements (1.2 MB of payload) is humongous
/// (total > region_size/2) AND its payload spans TWO regions. The old flat
/// `copy_nonoverlapping(obj+HEADER_SIZE, .., len*2)` would have walked past
/// the first region's buffer; the per-element path must not.
#[test]
fn read_char_array_bulk_humongous_crosses_region_boundary() {
    let heap = make_g1_heap();
    let len = 600_000usize; // 1.2 MB payload > 1 MiB region => spans 2 regions
    let arr = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Char, len);

    // Write a position-dependent pattern so a stale/short read is detected.
    // Sample a sparse set of indices (writing all 600k is wasteful), making
    // sure to cover the first region, the boundary, and the tail.
    let probe = |i: usize| -> u16 { ((i.wrapping_mul(2654435761)) & 0xFFFF) as u16 };
    let region_usable_chars = (1024 * 1024 - HEADER_SIZE) / 2;
    let mut indices = vec![
        0,
        1,
        region_usable_chars - 1,
        region_usable_chars,
        region_usable_chars + 1,
        len - 1,
    ];
    indices.dedup();
    for &i in &indices {
        heap.set_array_element(arr, i, Value::Int(probe(i) as i32))
            .unwrap();
    }

    let out = heap.read_char_array_bulk(arr);
    assert_eq!(out.len(), len, "bulk read must return all elements");
    for &i in &indices {
        assert_eq!(
            out[i],
            probe(i),
            "code unit at index {i} (region-crossing read) corrupted"
        );
    }
    // Indices we never wrote default to 0 (fresh zeroed payload).
    assert_eq!(
        out[region_usable_chars / 2],
        0,
        "untouched element must read as 0, not garbage"
    );
}

/// Sanity: the ordinary (single-region) G1 char[] path still works through
/// the same per-element bulk reader.
#[test]
fn read_char_array_bulk_small_g1_array_roundtrips() {
    let heap = make_g1_heap();
    let len = 8usize;
    let arr = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Char, len);
    for i in 0..len {
        heap.set_array_element(arr, i, Value::Int((0x4100 + i) as i32))
            .unwrap();
    }
    let out = heap.read_char_array_bulk(arr);
    assert_eq!(out.len(), len);
    for i in 0..len {
        assert_eq!(out[i], (0x4100 + i) as u16);
    }
}

/// After the single-arena change a G1 humongous array is one contiguous
/// block, so `array_data_ptr` hands out a flat pointer valid for the whole
/// payload (not just the first region). Verify ordinary AND humongous
/// arrays both return `obj + HEADER_SIZE`, and that the humongous flat
/// pointer round-trips a tail element living far past region 0 — exactly
/// the bulk-consumer (arraycopy/Unsafe/GPU) access pattern.
#[test]
fn array_data_ptr_is_flat_and_contiguous_for_g1_humongous() {
    let heap = make_g1_heap();

    // Ordinary single-region int[]: contiguous pointer just past the header.
    let small = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Int, 8);
    let sptr = heap
        .array_data_ptr(small)
        .expect("ordinary array must have a flat pointer");
    assert_eq!(
        sptr,
        unsafe { small.as_ptr().add(ARRAY_DATA_OFFSET) },
        "flat pointer must be obj + HEADER_SIZE",
    );

    // Humongous int[] (~1.6 MB > 1 MiB region) is now also contiguous.
    let n = 400_000usize;
    let large = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Int, n);
    let lptr = heap
        .array_data_ptr(large)
        .expect("humongous array now exposes a contiguous flat pointer")
        as *mut i32;
    assert_eq!(
        lptr as *mut u8,
        unsafe { large.as_ptr().add(ARRAY_DATA_OFFSET) },
        "humongous flat pointer must be obj + HEADER_SIZE",
    );

    // The flat pointer must reach the tail element (far past region 0)
    // without escaping the object: write via the raw pointer (as a bulk
    // consumer would), read back via the GC accessor.
    unsafe { std::ptr::write(lptr.add(n - 1), 0x1234_5678) };
    assert_eq!(
        heap.get_array_element(large, n - 1).unwrap().as_int(),
        Some(0x1234_5678),
        "flat write to the humongous tail must round-trip",
    );
}

// ======================================================================
// soft_ref_policy_free_mb — the pressure input to the SoftReference LRU.
// See the accessor's doc comment for why it is keyed on allocatable
// rather than merely unused bytes, and for the cross-owner handoff.
// ======================================================================

#[test]
fn soft_ref_policy_free_mb_is_bounded_by_whole_heap_headroom() {
    let heap = VmHeap::new(GcBackend::Generational, 32 * 1024 * 1024);
    let (young_used, young_cap) = heap.young_gen_stats();
    let (old_used, old_cap) = heap.old_gen_stats();
    let total_free_mb =
        (young_cap + old_cap).saturating_sub(young_used + old_used) / (1024 * 1024);

    let reported = heap.soft_ref_policy_free_mb();
    assert!(
        reported <= total_free_mb,
        "reported {reported} MB exceeds whole-heap headroom {total_free_mb} MB"
    );
    // It must also never exceed the young generation's own headroom, which
    // is what an allocation actually has to fit into.
    let young_free_mb = young_cap.saturating_sub(young_used) / (1024 * 1024);
    if young_cap != 0 {
        assert!(
            reported <= young_free_mb,
            "reported {reported} MB exceeds young headroom {young_free_mb} MB"
        );
    }
}

/// G1's soft-ref headroom is the whole heap's, not the unused tails of the
/// regions that currently happen to be Eden or Old.
///
/// The pre-2026-09-23 arm fed G1's `eden_stats` / `old_gen_stats` into the
/// generational formula. Those capacities count only regions OF THAT TYPE,
/// so Free regions — G1's actual headroom — were counted nowhere, and on a
/// nearly empty heap the answer was `0` MB: maximum soft-reference
/// pressure, every `SoftReference` cleared at every pause.
#[test]
fn soft_ref_policy_free_mb_on_g1_counts_free_regions() {
    const MB: usize = 1024 * 1024;
    let heap = VmHeap::new(GcBackend::G1, 64 * MB);
    // A few objects, so at least one region is claimed and the old formula
    // had something (a part-full region) to report the tail of.
    for _ in 0..64 {
        let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
    }
    let reported = heap.soft_ref_policy_free_mb();
    // g1sz: against the `-Xmx` grid (HotSpot's `MaxHeapSize`), not the
    // sizing policy's soft capacity, which starts at `-Xms`.
    let max = match &heap.inner {
        VmHeapInner::G1(h) => h.collector.heap_target_bytes_within(false),
        _ => unreachable!("built as G1"),
    };
    let whole_heap_free_mb = max.saturating_sub(heap.allocated_bytes()) / MB;
    assert_eq!(
        reported, whole_heap_free_mb,
        "G1 has one allocation pool: its soft-ref headroom is max capacity - allocated"
    );
    assert!(
        reported >= 32,
        "a 64 MiB G1 heap holding a few KiB must not report maximum soft-ref \
         pressure (got {reported} MB) — that clears every SoftReference at every pause"
    );
}

/// ZGC's figure did not change with the G1 repair: it was already the
/// whole-heap headroom, reached through `young_gen_stats() == (0, 0)`.
#[cfg(feature = "zgc")]
#[test]
fn soft_ref_policy_free_mb_on_zgc_is_whole_heap_headroom() {
    const MB: usize = 1024 * 1024;
    let heap = VmHeap::new(GcBackend::Zgc, 64 * MB);
    let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
    assert_eq!(
        heap.soft_ref_policy_free_mb(),
        heap.heap_capacity().saturating_sub(heap.allocated_bytes()) / MB,
    );
}

#[test]
fn soft_ref_policy_free_mb_rounds_down_to_max_pressure() {
    // A heap far smaller than 1 MiB of usable headroom must report 0 MB
    // (maximum pressure), never a rounded-up 1 MB that would keep the LRU
    // threshold non-zero.
    let heap = VmHeap::new(GcBackend::Generational, 512 * 1024);
    assert_eq!(
        heap.soft_ref_policy_free_mb(),
        0,
        "sub-megabyte headroom must read as zero free MB"
    );
}

/// `committed_bytes` is a count of committed granules/regions, not an
/// occupancy, and `Runtime.totalMemory()` rests on that: allocating inside
/// the committed prefix must move `allocated_bytes` and leave
/// `committed_bytes` alone. A version that tracked occupancy would report a
/// `totalMemory()` that changes on every collection, which callers written
/// as "gc until totalMemory settles" read as "the heap is still resizing".
///
/// All three backends since 2026-09-23 (the arms were Generational and G1
/// only while ZGC's answer was a constant envelope). `-Xms` 16 MiB so the
/// few hundred KiB the fixture allocates land inside what is already
/// committed on every backend.
#[test]
fn committed_bytes_does_not_track_occupancy() {
    const MB: usize = 1024 * 1024;
    let ov = G1ConfigOverrides::default();
    for backend in backends_under_test() {
        let (heap, _) = VmHeap::new_with_heap_sizing(backend, 64 * MB, Some(16 * MB), ov);
        let committed_before = heap.committed_bytes();
        let allocated_before = heap.allocated_bytes();
        assert!(
            committed_before > 0,
            "{backend:?}: committed heap must be positive"
        );
        for _ in 0..4000 {
            let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
        }
        // Non-vacuity: if the allocations did not register, the equality
        // below would hold for the wrong reason.
        assert!(
            heap.allocated_bytes() > allocated_before,
            "{backend:?}: the fixture must actually allocate"
        );
        assert_eq!(
            heap.committed_bytes(),
            committed_before,
            "{backend:?}: committed heap moved while only occupancy changed"
        );
        assert!(
            heap.committed_bytes() >= heap.allocated_bytes(),
            "{backend:?}: committed heap is below what is allocated in it"
        );
    }
}

/// `Runtime.totalMemory()` is the COMMITTED heap, not the reservation
/// (`XmsProbe -Xms64m -Xmx512m`: HotSpot `total=64m`; CratonVM printed
/// `total=512m` on every backend until 2026-09-23). A fresh heap with a
/// small `-Xms` under a large `-Xmx` must report roughly the `-Xms`, on
/// every backend.
///
/// Precondition, as in `xms_commits_bytes_without_resizing_the_heap`: the
/// store must be a RESERVING one. The wholly-committed fallback
/// (`CRATONVM_GC_RESERVE=0`, or a platform that refused the reservation)
/// commits every arena at its capacity, and there the reservation IS the
/// committed heap.
#[test]
fn committed_bytes_is_the_committed_heap_not_the_reservation() {
    const MB: usize = 1024 * 1024;
    let xmx = 256 * MB;
    let xms = 16 * MB;
    let ov = G1ConfigOverrides::default();
    for backend in backends_under_test() {
        let (heap, _) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xms), ov);
        if heap.os_committed_bytes() >= heap.heap_capacity() {
            continue; // wholly-committed store: nothing to distinguish
        }
        let committed = heap.committed_bytes();
        // gen r5w2/obs6: Generational's committed heap excludes the copy
        // reserve (HotSpot Serial's `capacity()` leaves its to-space out
        // too). Under the even split (`CRATONVM_GEN_XMS_USABLE_FIRST=0`)
        // `commit_initial_heap` puts half of a 16 MiB `-Xms` (all of it
        // young here) in the to-space: committed, not usable — hence the
        // `xms / 2` floor. The default since gen r5w4/defaults8 is the
        // usable-first split (one survivor in the to-space, ~14 MiB usable
        // here), which clears the same floor with room.
        let floor = if backend == GcBackend::Generational {
            xms / 2
        } else {
            xms
        };
        assert!(
            committed >= floor,
            "{backend:?}: -Xms {xms} was committed at startup, but \
             committed_bytes reports only {committed} (floor {floor})"
        );
        assert!(
            committed < xmx / 2,
            "{backend:?}: committed_bytes reports {committed} of a {xmx} \
             reservation with a {xms} -Xms — that is the reservation, \
             which is what Runtime.totalMemory() must not report"
        );
        assert!(
            committed <= heap.os_committed_bytes(),
            "{backend:?}: committed_bytes ({committed}) above what the \
             stores have asked the OS to back ({})",
            heap.os_committed_bytes()
        );
    }
}

/// `docs/feature-designs/concurrent-gc-maturation.md`:
/// a humongous object must be counted in the G1 old-generation figure, and
/// the young + old capacities must cover the whole heap (Free regions
/// included) rather than only the regions currently typed Eden or Old.
#[test]
fn g1_old_gen_stats_count_humongous_and_free_regions() {
    const MB: usize = 1024 * 1024;
    let heap = VmHeap::new(GcBackend::G1, 64 * MB);
    // 4 MiB of int — larger than half of any region size the 64 MiB
    // ergonomic picks, so it is a humongous span.
    let len = MB;
    let arr = heap.try_alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Int, len);
    assert!(
        arr.is_some(),
        "fixture: the humongous allocation must succeed"
    );
    let (young_used, young_cap) = heap.young_gen_stats();
    let (old_used, old_cap) = heap.old_gen_stats();
    assert!(
        old_used >= 4 * len,
        "G1 old_gen_stats reports {old_used} bytes used with a {}-byte \
         humongous array live — humongous regions are not being counted",
        4 * len
    );
    assert!(
        young_cap + old_cap >= heap.heap_capacity(),
        "G1 young+old capacity ({young_cap} + {old_cap}) does not cover the \
         heap ({}) — Free regions are not being counted",
        heap.heap_capacity()
    );
    assert!(
        young_used + old_used >= heap.allocated_bytes(),
        "G1 young+old used ({young_used} + {old_used}) below the heap's \
         allocated bytes ({})",
        heap.allocated_bytes()
    );
}

/// gc-common w4-f (`docs/feature-designs/concurrent-gc-maturation.md`): Survivor
/// regions are HotSpot's "G1 Survivor Space", a YOUNG pool — they count in
/// `young_gen_stats`, not `old_gen_stats` (where the wave-2 complement of
/// Eden had put them). And `region_census` partitions the region table.
#[test]
fn g1_survivor_regions_are_on_the_young_side() {
    use cratonvm_gc_core::region::RegionType;
    const MB: usize = 1024 * 1024;
    const SURVIVOR_BYTES: usize = 4096;
    let heap = VmHeap::new(GcBackend::G1, 64 * MB);
    let VmHeapInner::G1(h) = &heap.inner else {
        unreachable!("constructed as G1")
    };
    // A few ordinary objects (Eden) ...
    for _ in 0..64 {
        let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
    }
    let (young_before, _) = heap.young_gen_stats();
    let (old_before, _) = heap.old_gen_stats();
    // ... and one Free region staged as a part-full Survivor, the state a
    // young pause with survivors leaves. Only the cursor is read by the
    // census, so no byte of the region is touched.
    let mut staged = false;
    h.with_regions_mut(|regions| {
        if let Some(r) = regions
            .iter_mut()
            .find(|r| r.region_type() == RegionType::Free)
        {
            r.set_age(1);
            r.set_region_type(RegionType::Survivor);
            r.set_cursor(SURVIVOR_BYTES);
            staged = true;
        }
    });
    assert!(
        staged,
        "fixture: a 64 MiB G1 heap has a Free region to stage"
    );

    let c = h.region_census();
    assert_eq!(c.survivor, (SURVIVOR_BYTES, 1));
    assert_eq!(
        c.eden.1 + c.survivor.1 + c.old.1 + c.humongous.1 + c.free,
        h.num_regions(),
        "the census must partition the region table"
    );
    let (young_used, young_cap) = heap.young_gen_stats();
    let (old_used, old_cap) = heap.old_gen_stats();
    assert_eq!(
        young_used,
        young_before + SURVIVOR_BYTES,
        "Survivor bytes must be on the young side"
    );
    assert_eq!(old_used, old_before, "... and not on the old side");
    assert!(
        young_cap >= (c.eden.1 + 1) * c.region_size,
        "young capacity must include the Survivor region"
    );
    assert!(
        young_cap + old_cap >= heap.heap_capacity(),
        "G1 young+old capacity ({young_cap} + {old_cap}) must still cover \
         the heap ({})",
        heap.heap_capacity()
    );
    assert_eq!(
        young_used + old_used,
        heap.allocated_bytes(),
        "young + old used must be every region's cursor, exactly once"
    );
}

#[test]
fn soft_ref_policy_free_mb_shrinks_as_the_heap_fills() {
    let heap = VmHeap::new(GcBackend::Generational, 64 * 1024 * 1024);
    let before = heap.soft_ref_policy_free_mb();
    // Allocate a few hundred KB of objects; the figure must not grow.
    for _ in 0..2000 {
        let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
    }
    let after = heap.soft_ref_policy_free_mb();
    assert!(
        after <= before,
        "free-MB estimate rose from {before} to {after} while allocating"
    );
}

/// KINDOF-SENTINEL (2026-08-04): `kind_of`/`element_type_of`/
/// `identity_hash_code`/`class_id_of`/`load_and_forward` all used to
/// dereference `obj` unconditionally, trusting the caller. Reproduced in
/// the wild as the all-ones sentinel `0xFFFFFFFFFFFFFFFF` reaching each
/// of these from a JIT bail-to-interpreter transition — a hard
/// `EXCEPTION_ACCESS_VIOLATION`, not a wrong answer. Every one of these
/// accessors must now fall back to a safe default instead of faulting.
#[test]
fn heap_accessors_reject_an_invalid_object_pointer_instead_of_faulting() {
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    // 8-byte aligned so only the region-bounds check (not the alignment
    // check) is exercised — the observed all-ones sentinel fails both,
    // and either failure mode must be rejected the same way.
    let sentinel = unsafe {
        ObjectRef::from_raw_nonnull(
            std::ptr::NonNull::new(0xFFFF_FFFF_FFFF_FFF8u64 as *mut u8).unwrap(),
        )
    };

    assert_eq!(heap.kind_of(sentinel), ObjectKind::Object);
    assert_eq!(heap.element_type_of(sentinel), ArrayElementType::Reference);
    assert_eq!(heap.identity_hash_code(sentinel), 0);
    assert_eq!(heap.class_id_of(sentinel), cratonvm_types::ClassId::new(0));
    assert_eq!(
        heap.load_and_forward(sentinel).as_ptr(),
        sentinel.as_ptr(),
        "an invalid obj has nothing valid to forward to; must return unchanged, not fault"
    );
}

/// The forwarding-target half of the same fix: a live, validly-addressed
/// object whose header bytes are corrupted can have its `forwarded` bit
/// spuriously set with a garbage `forwarding_ptr` — `load_and_forward`
/// must not hand that garbage address back to the caller.
#[test]
fn load_and_forward_rejects_a_forwarding_target_outside_every_region() {
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let obj = heap
        .try_alloc_object(cratonvm_types::ClassId::new(0), 8)
        .unwrap();
    // SAFETY: test-only corruption of a live header to simulate the
    // observed implausible-header family (see `old_gen::scan_region`'s
    // `validate_header_tags_or_desync`) reaching the forwarding word.
    unsafe {
        let header = &mut *(obj.as_ptr() as *mut ObjectHeader);
        // Raw bits: `set_forwarding_address` asserts a plausible target,
        // and an implausible one is exactly what this test installs.
        // The target lives in the second word; the mark word says FORWARDED.
        std::ptr::write(
            obj.as_ptr().add(cratonvm_types::FORWARDING_TARGET_OFFSET) as *mut u64,
            0xFFFF_FFFF_FFFF_FFF8u64,
        );
        header.mark_word.store(
            cratonvm_types::MARK_FORWARDED,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    assert_eq!(
        heap.load_and_forward(obj).as_ptr(),
        obj.as_ptr(),
        "an implausible forwarding target must fall back to the original pointer"
    );
}

/// gci i1/wr (`r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`, step
/// 2): through `VmHeap`, a G1 `cleanup` (which can free whole Old regions
/// in place) moves `address_epoch()` while `collection_count()` stays, so
/// the native address memos that key on it miss after a cleanup.
#[test]
fn gci_wr_g1_cleanup_moves_the_vm_heap_address_epoch_not_the_count() {
    let heap = make_g1_heap();
    let collections = heap.collection_count();
    let epoch = heap.address_epoch();
    let g1 = g1_state(&heap).expect("a G1 heap");
    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &[]);
    while !g1.concurrent_mark_step(usize::MAX) {}
    g1.cleanup(&stw());
    assert_eq!(heap.collection_count(), collections, "cleanup books no collection");
    assert_ne!(heap.address_epoch(), epoch, "cleanup must move the address epoch");
}
