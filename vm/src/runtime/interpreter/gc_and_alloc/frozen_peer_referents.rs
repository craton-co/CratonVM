#[allow(unused_imports)]
use super::*;

// ---------------------------------------------------------------------------
// G1 concurrent marking cycle
// ---------------------------------------------------------------------------

/// Phase 1 of a ZGC concurrent cycle: **mark start**, at a brief STW pause.
///
/// Opens the cycle and returns. The transitive closure is then traced by
/// `ZMarkCoordinator`'s worker threads while every mutator in this VM runs;
/// the cycle is closed inside the next `collect_garbage`, which replays the
/// SATB ingress, re-scans the roots, and only then sweeps.
///
/// # Why this is shaped exactly like `g1_concurrent_mark_cycle`
///
/// Because the constraint is the VM's, not the collector's: a stop-the-world
/// pause in this VM can only be initiated by a thread that is in the thread
/// registry, holds a `JvmThread`, and can drive `stw_take_over_and_wait` for
/// in-JIT peers. A GC background thread is none of those. So the two phase
/// boundaries a concurrent collector needs — mark start and mark end — are
/// both taken by mutators, and the collector's own threads do only the part
/// that needs no safepoint: the tracing.
///
/// The open-coded `request → takeover-wait → work → complete` (rather than a
/// request plus a plain `wait_for_all` -- the deleted `brief_stw*` helpers) is
/// INT-3's residual fix, copied deliberately: a plain `wait_for_all()` stalls forever
/// on a peer spinning in compiled code, and its root set covers such a peer
/// only through a STALE deposit snapshot. A missed root here is an object the
/// concurrent phase never traces.
///
/// Mark-only pause: nothing moves, so there is no pointer map, no pin set and
/// no root rewrite — the frozen peers' conservative roots are simply extra
/// mark roots.
/// JIT round 20 wave 8 (lane huntGC; page
/// `docs/feature-designs/jit-barrier-lowering.md`):
/// the references held RIGHT NOW by every object a frozen in-JIT peer names,
/// as extra mark roots for a mark-start pause -- the G1 and ZGC twin of the
/// generational `ConcurrentMarker::initial_mark_with_frozen`
/// (`gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928.md`).
///
/// # The window
///
/// Compiled reference stores test the JIT SATB pre-gate once and skip the
/// pre-barrier when it reads clear (`emit_g1_satb_gate_skip`,
/// `emit_gated_compact_ref_putfield`, the gated `aastore`, and the IR tier's
/// `emit_gated_ir_ref_putfield` / `emit_gated_ir_aastore` /
/// `emit_g1_ir_ref_putfield`). Nothing between that test and the store is a
/// safepoint, so a thread that ARRIVES at a pause is never between them; a
/// thread the takeover FREEZES can be. It read the gate before this pause
/// armed it, and once resumed completes `O.f = B` without logging `A`, the
/// value `O.f` held at the snapshot. If another mutator has meanwhile copied
/// `A` into an object the marker has already scanned, `A` ends the mark
/// unmarked and is freed while referenced.
///
/// # Why greying `O`'s referents here closes it
///
/// SATB must mark everything reachable AT THE SNAPSHOT, and the one value the
/// frozen store can hide is `O.f`'s value at the snapshot, which is still in
/// `O` while the world is stopped. Every gated emitter holds its receiver (or
/// array) in RAX, loaded from its frame home before the gate test, for the
/// whole window, so `O` is among the frozen peer's conservative roots
/// (`xt_roots`). Marking every reference `O` holds now marks `A` before the
/// frozen thread can overwrite anything. Over-marking is SATB-safe (floating
/// garbage for one cycle at most).
///
/// Bounded by what the takeover already found: one level, over the frozen
/// peers' own roots only, and nothing when no peer was frozen (`xt_roots`
/// empty) -- the common case pays one empty-slice test. Unconditional: a
/// correctness fix.
///
/// # Which objects are walked, and how (wave 8 host regression, G1)
///
/// `xt_roots` are CONSERVATIVE words. On G1 the plain `is_object_address` is
/// a header-tag screen, so a header-shaped word inside a `byte[]` payload
/// passes it; the first version walked such a word's "fields" through
/// `for_each_object_reference`, whose legacy arm read string bytes as `Value`
/// cells and logged 32 `oracle-walk: corrupt Value cell` errors per run on
/// `R20LoopClassWordForward` under `-XX:+UseG1GC`. So:
///
/// * a root must pass `is_object_address_for_root_scan` -- the screen the
///   conservative band scans use for object STARTS (on G1 it also rejects a
///   decoded object that would cross its region end);
/// * a primitive array is skipped; a reference array is read through the
///   heap's own `get_array_element` (region- and width-aware), never flat;
/// * a compact object is read at its class layout's `ref_disps` only;
/// * a legacy object's cells are read SILENTLY: only a cell whose tag word is
///   `FIELD_CELL_TAG_OBJECT` yields its payload word, every other cell is
///   skipped without decoding a `Value`;
/// * every candidate child must itself pass `is_object_address_for_root_scan`
///   before it becomes a mark root.
///
/// A false-positive root therefore costs at most a few over-marked roots,
/// which conservative roots already are, and nothing reads it as a `Value`.
///
/// # Never a `Reference`'s referent (JIT round 20 wave 21, lane g1ref)
///
/// `reference_objs` is the cycle's referent-slot skip set (the registered
/// Weak/Soft/Phantom `Reference` objects, `reference_object_addresses`), SORTED.
/// Slot 0 of such an object is `Reference.referent`, which the marker hides for
/// the whole cycle; greying it here made the referent STRONGLY reachable for
/// the cycle whenever a frozen compiled thread merely held the `Reference`
/// (the receiver of a `while (r.get() != null)` spin is held in a register for
/// the whole loop), so remark-time reference processing could never clear it
/// -- `docs/feature-designs/concurrent-gc-maturation.md`.
/// Leaving it out is sound for the window above: compiled code stores a
/// referent only in `Reference.<init>` (the old value is null), and the clear
/// is HotSpot's no-keep-alive `clear0`; a mutator that READ the referent got it
/// through `get()`'s SATB keep-alive, which marks it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn frozen_peer_referents(
    shared: &SharedVm,
    frozen: &[ObjectRef],
    reference_objs: &[usize],
) -> Vec<ObjectRef> {
    let heap = &shared.mem.heap;
    let mut out: Vec<ObjectRef> = Vec::new();
    let mut children: Vec<usize> = Vec::new();
    for r in frozen {
        let Some(obj) = heap.is_object_address_for_root_scan(r.as_ptr() as usize) else {
            continue;
        };
        let addr = obj.as_ptr() as usize;
        let header = heap.get_header(obj);
        children.clear();
        // The hidden referent slot, when `obj` is a registered `Reference`.
        let hides_referent = reference_objs.binary_search(&addr).is_ok();
        if header.kind() == cratonvm_types::ObjectKind::Array {
            if header.element_type() != ArrayElementType::Reference {
                continue;
            }
            for i in 0..heap.array_length(obj) {
                if let Ok(Value::Object(Some(c))) = heap.get_array_element(obj, i) {
                    children.push(c.as_ptr() as usize);
                }
            }
        } else if cratonvm_types::is_compact_object(header) {
            let _ = cratonvm_types::with_class_layout(
                header.class_id.as_u32(),
                header.num_slots(),
                |layout| {
                    let referent_disp = if hides_referent {
                        layout.field_disp(0)
                    } else {
                        None
                    };
                    for &disp in &layout.ref_disps {
                        if Some(disp) == referent_disp {
                            continue;
                        }
                        // SAFETY: the world is stopped and `addr` passed the
                        // root-scan screen; `disp` is a reference field's
                        // absolute displacement in this class's layout.
                        let raw = unsafe { std::ptr::read((addr + disp as usize) as *const u64) };
                        children.push(raw as usize);                    }                },
            );
        } else {
            for i in 0..header.num_slots() as usize {
                if hides_referent && i == 0 {
                    continue;
                }
                let cell = addr + cratonvm_types::HEADER_SIZE + i * cratonvm_types::SLOT_SIZE;
                // SAFETY: as above; the root-scan screen bounded the object
                // (and so its `num_slots` cells) inside its region.
                let tag = unsafe {
                    std::ptr::read((cell + cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32)
                };
                if tag != cratonvm_types::FIELD_CELL_TAG_OBJECT {
                    continue;
                }
                // SAFETY: as above.
                let raw = unsafe {
                    std::ptr::read(
                        (cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const u64,
                    )
                };
                children.push(raw as usize);
            }
        }
        for &c in &children {
            if c == 0 {
                continue;
            }
            if let Some(child) = heap.is_object_address_for_root_scan(c) {
                out.push(child);
            }
        }
    }
    out
}

/// zt t22/zc3b — `CRATONVM_ZGC_MARK_START_BINDS_REFERENCES` (default ON, `=0`
/// kill): does a ZGC concurrent mark start bind the `Reference`s registered at
/// allocation before it hands the marker its skip set? Read once per mark
/// start (a flag-snapshot lookup, never per allocation).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_mark_start_binds_references_enabled() -> bool {
    true
}



pub(in crate::runtime::interpreter) fn zgc_concurrent_mark_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    // zr z1/br: a previous mark start of this collection epoch was refused by
    // the take-over cover; do not pay another take-over pause for the same
    // answer. The next collection lifts the latch.
    if zgc_satb_window_latched(shared) {
        // zr z3/cs: a requested every-trigger cycle lapses at once, so the
        // next poll collects stop-the-world instead of waiting out its grace.
        shared.mem.heap.zgc_note_conc_start_outcome(false);
        return;
    }
    // zr zs s1/tp (`zgc-zr11rv9-z8-z9-review-residuals-FIXED-20261019.md` item 2):
    // re-test the start before requesting the pause. Every caller tested
    // `zgc_should_start_concurrent_mark` (or, under every-trigger, its
    // `zgc_conc_start_requested` half) before calling, but two threads can
    // both pass that test; the one that requests second, after the first
    // one's pause has opened the cycle and released the world, stopped every
    // mutator for a mark start that `start_concurrent_mark` then refuses at
    // its first line. The re-test closes all but the window between it and
    // the request below (`ct_lost_races` counts what is left). It is the
    // same predicate the callers use, so a start that is still due is never
    // dropped. `CRATONVM_ZGC_CONC_START_RECHECK=0` restores the old path.
    if !shared.mem.heap.zgc_should_start_concurrent_mark() {
        shared.mem.heap.zgc_note_conc_start_skipped();
        return;
    }
    // Pre-bind newly allocated references before entering STW so large
    // allocations do not inflate the Pause Mark Start safepoint.
    if zgc_mark_start_binds_references_enabled() {
        crate::runtime::interpreter::bind_allocated_references_pre_gc(shared);
    }
    // zt t31/zc11: the start of the cycle this mark start may open, taken
    // right before requesting the pause (after pre-STW reference binding).
    let mark_start_requested_at = std::time::Instant::now();
    // gc-common w4-a: the identity census, initiator slot included.
    let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) else {
        // Another STW is in progress. Nothing has been done, so there is
        // nothing to unwind: the next allocation re-tests the threshold and
        // re-opens the cycle. If that other STW is a collection, the threshold
        // will have dropped and the cycle correctly does not open.
        return;
    };
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        // JIT round 19 residual (`r19-an-unwind-out-of-a-pause-leaks-its-lazy-native-claims-FIXED-20261007`):
        // armed BEFORE the take-over, so an unwind anywhere in this pause
        // resumes the frozen peers and reopens the world (`GenConcPauseGuard`).
        let mut pause_guard = GenConcPauseGuard::arm(shared);
        // zt t31/zc11 (`CRATONVM_ZGC_MARK_START_GRACE_US`): a longer cooperative
        // grace, so the take-over cover below has fewer frozen peers to refuse.
        pause_guard.hold(with_takeover_first_pass_grace(zgc_mark_start_grace_us(), || {
            stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids)
        }));
        // gc-common w5-a: `--verbose:gc` pause line (`door=zgc-mark-start`).
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::ZgcMarkStart);

        // SAFETY: `stw_take_over_and_wait` above has parked every other mutator
        // at a safepoint (or forcibly stopped and conservatively scanned it),
        // and `taken` is still held, so this thread is the only mutator for the
        // whole block below.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };

        // zt t22/zc3b (`CRATONVM_ZGC_MARK_START_BINDS_REFERENCES`, default ON,
        // `=0` kill): the `Reference`s registered at allocation since the last
        // collection become rows HERE, before the skip candidates and the
        // frozen-peer referents read the rows. Under discovery at allocation
        // (every `--real-jdk` VM, and `--compatible` over a real JDK) they were
        // bound only by the next collection's pre-collection pass, so this
        // cycle's skip set missed them and the concurrent marker traced their
        // referents strongly: a `System.gc()` that owns the cycle
        // (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`) returned with every weak
        // referent made since the last collection still set
        // (`zc1_S1_J`: alive_after_gc=80000, HotSpot 0). The world is stopped,
        // as at the collection's own bind.
        if zgc_mark_start_binds_references_enabled() {
            crate::runtime::interpreter::bind_allocated_references_pre_gc(shared);
        }

        // zt t32 (successor page section 10,
        // `CRATONVM_ZGC_MARK_START_REFRESHES_INITIATOR_SNAPSHOT`, default ON,
        // `=0` kill): publish this thread's roots NOW, as the collection door
        // does at its entry (`run_collection_pause`). `collect_roots` step 11
        // pushes the initiator's own `root_snapshot` and the registry gather
        // below pushes it again, and the snapshot is whatever this thread
        // deposited the last time it PARKED -- for a thread that has only
        // initiated pauses since, frames that have long returned.
        // `GciI31GqSystemGcFinalizerChainProbe` under the profile: main parked
        // for a peer's mark start inside `makeAndDrop`, then initiated the
        // `System.gc()` cycle's mark start, whose roots kept the dead round's
        // `kids` array through that deposit -- 12 weak references set after
        // the call, in about 6 % of loaded runs. Inside the pause, the world
        // stopped; the coverage verdicts of the JIT scan are this pause's.
        if zgc_mark_start_refreshes_initiator_snapshot_enabled() {
            update_root_snapshot(shared, thread);
        }
        let roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        // zt t23/hunt5 (`CRATONVM_ZGC_MARK_START_ROOTS_PENDING_FINALIZERS`,
        // default ON, `=0` kill): the objects queued for `finalize()` and not
        // yet finalized are roots of the cycle, as G1's initial mark roots
        // them (`append_pending_finalizer_mark_roots`). `collect_roots` leaves
        // the queue out (it is a collection's RESURRECTION input), and a
        // collection sees no window; a concurrent cycle does. The delivery
        // thread can take a queued `x` and run its `finalize()` during the
        // concurrent phase; when that stores `x` (or an object only `x`
        // reached) into an object the marker has already scanned -- a static
        // list, a node allocated black -- the SATB barrier logs the overwritten
        // value, not `x`, and `x` is gone from the queue by the mark end, so
        // nothing marks it and the sweep frees a reachable object. HotSpot's
        // `x` is strongly reachable from `Finalizer`'s queue for the whole
        // cycle. Inside this pause, with the world stopped; `[]` with the
        // switch off.
        let mut pending_finalizers: Vec<ObjectRef> = Vec::new();
        {
            pending_finalizers.extend(g1_pending_finalizer_mark_roots(
                shared.mem.finalizer_thread.pending_addresses(),
            ));
        }
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        // JIT round 20 wave 8 (lane huntGC): what the frozen peers' objects
        // hold NOW, before this pause arms the SATB gate (`frozen_peer_referents`).
        // Wave 21 (lane g1ref): never a registered `Reference`'s referent --
        // the marker hides that slot (`ref_skip_objs`). Read only when a peer
        // was frozen; the common pause takes no extra lock.
        let reference_objs = if xt_roots.is_empty() {
            Vec::new()
        } else {
            let mut objs = shared.mem.ref_processor.lock().reference_object_addresses();
            objs.sort_unstable();
            objs
        };
        let frozen_referents = frozen_peer_referents(shared, &xt_roots, &reference_objs);
        // JIT r23 w1b L7 (R23W1L7-4): where each part of `all_roots` ends, for
        // the census (the finalizer queue, past the frozen referents, is
        // labelled with them).
        let census_ends = (
            roots.len(),
            roots.len() + snapshot_roots.len(),
            roots.len() + snapshot_roots.len() + xt_roots.len(),
        );
        let all_roots: Vec<ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .chain(frozen_referents.into_iter())
            // zt t23/hunt5: the finalizer queue (empty with the switch off).
            .chain(pending_finalizers.into_iter())
            .collect();
        // JIT r23 w1b L7 (`CRATONVM_DBG=root-kind-census`, off by default).
        root_kind_census_for_g1_mark(shared, "zgc-mark-start", &all_roots, census_ends);

        // zr z1/br: the take-over cover. A peer frozen in compiled code may be
        // inside a SATB gate window (the pre gate read "idle", the store has
        // not run) or an inline allocation's announce window (the `blocked`
        // word read "open", the start bit is not set); it resumes after
        // `set_mark_active(true)` below and overwrites a reference unlogged,
        // or publishes an object that is neither black nor rooted. Either is
        // a live object the concurrent cycle can free. Refuse to open: the
        // collection stays stop-the-world, which needs no barrier.
        let frozen = pause_guard.frozen_count();
        // zt pl4 item 1: the cycle's cause, read before the cycle opens (its
        // trigger latch is the pending one here), for `sun.gc.cause`.
        let open_cause = shared.mem.heap.zgc_hotspot_gc_cause(false);
        let opened = if zgc_satb_window_refuses(frozen, zgc_satb_takeover_cover_enabled) {
            note_zgc_satb_window_refusal(shared, frozen);
            false
        } else {
            // `CRATONVM_ZGC_CONC_REF_SKIP` (default ON): hide the referent
            // slot of every active Weak / Phantom `Reference` from the concurrent
            // marker, as G1's initial mark does (`g1_set_reference_skip_set`).
            // Without it the concurrent phase traced those referents strongly, so a
            // cycle ending in a certified concurrent mark cleared none whose
            // referent died before mark start. Inside this pause, before the cycle
            // opens; the heap empties the set at the cycle's end.
            if zgc_conc_ref_skip_enabled() {
                let candidates = zgc_conc_reference_skip_candidates(shared);
                shared
                    .mem
                    .heap
                    .zgc_set_concurrent_reference_skip(candidates);
                // zr z2/cc: the soft rows, for the concurrent phase only (the
                // heap re-roots the policy-kept referents at the mark end).
                if zgc_conc_ref_skip_soft_enabled() {
                    let soft = zgc_conc_soft_reference_skip_candidates(shared);
                    shared.mem.heap.zgc_set_concurrent_soft_reference_skip(soft);
                }
            }
            let opened = shared.mem.heap.zgc_start_concurrent_mark(&stw, &all_roots);
            // zt t31/zc11: the cycle's start, for its closing collection's
            // `... Cycles` record and bean time (taken before this pause was
            // requested, so its own pause record lies inside the cycle).
            if opened {
                if let Some(cycle) = super::gc_events::zgc_concurrent_cycle_jfr(shared) {
                    cycle.note_cycle_open_at(mark_start_requested_at);
                    // zt t32: the cycle's cause, as its mark-start pause
                    // books it (the requester's door, else the rule's).
                    cycle.note_cycle_cause(
                        super::gc_events::zgc_requested_mark_start_cause(shared)
                            .or(open_cause)
                            .unwrap_or(GcDoor::AllocationThreshold.cause()),
                    );
                }
            }
            if !opened {
                // No cycle: nothing may keep the rows for a later one.
                shared
                    .mem
                    .heap
                    .zgc_set_concurrent_reference_skip(Vec::new());
                shared
                    .mem
                    .heap
                    .zgc_set_concurrent_soft_reference_skip(Vec::new());
            }
            opened
        };
        // zr z3/cs: the outcome, for `CRATONVM_ZGC_CONC_EVERY_TRIGGER`'s
        // request (a refusal lapses it). A no-op with the switch off.
        shared.mem.heap.zgc_note_conc_start_outcome(opened);
        // zr z4/jf: the JFR row opens the cycle's `Concurrent Mark` only if
        // a concurrent mark really runs.
        super::gc_events::non_collection_pause_note_opened(&mut pause_timer, opened);

        // Clear TLAB skip regions + resume frozen peers BEFORE reopening the
        // world — same race rationale as `maybe_gc`'s epilogue.
        non_collection_pause_seal(&mut pause_timer);
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
        // zt pl4 item 1: `sun.gc.cause` names the open cycle until its
        // closing collection's pause refresh.
        if let (true, Some(cause)) = (opened, open_cause) {
            crate::runtime::perf_data_file::note_concurrent_cycle_opened(shared, cause);
        }

        if opened {
            tracing::debug!(
                "[ZGC] concurrent mark started: {} roots seeded",
                all_roots.len()
            );
        }
    }
}



/// gci i19/fz: the objects queued for `finalize()` and not yet finalized
/// (`FinalizerThread::pending_addresses`) are MARK roots of a G1 cycle, at its
/// initial mark and at its remark.
///
/// # The defect this closes (default path)
///
/// A collection that finds a finalizable object `x` dead resurrects it and
/// queues it; from then on only the finalizer queue names `x`, and it holds a
/// raw address. `collect_roots` deliberately does not root that queue (it is
/// the evacuation pauses' RESURRECTION input, `finalizable_roots`), and the
/// generational concurrent cycle counts it as extra live (`gen_conc_y2o_seeds`),
/// but G1's two marking doors seeded nothing for it. So on G1 `System.gc()`
/// (young pause -> initial mark -> remark -> cleanup -> compaction pause) the
/// young pause queued `x` into a Survivor region below TAMS, the mark never
/// reached it, and `cleanup`'s dead-object scrub (`scrub_dead_refs_below_tams`,
/// `gc/src/g1.rs`) nulled every reference slot of the live `x`. Its
/// `finalize()` resurrected it with `x.weak == null` (the
/// `GciI16FrStwUndiscoveredProbe nested|twin` NPE). In HotSpot `x` is strongly
/// reachable from `Finalizer`'s queue for the whole cycle and its keep-alive
/// traces its fields, so the `WeakReference` only `x` holds is an ordinary
/// live object. The same gap could let `cleanup` free a wholly-unmarked Old
/// region in place while the queue still names an object in it.
///
/// Only the QUEUED objects: a registered finalizable that has not died yet is
/// not a root (the remark's reference round decides it and resurrects it
/// itself). A queued object that the finalizer thread takes during the
/// concurrent phase is in the snapshot, because the initial mark rooted it.
/// The addresses are screened as every mark root is (`mark_root_seed`); the
/// queue is relocated by every collection (`FinalizerThread::update_after_gc`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn append_pending_finalizer_mark_roots(shared: &SharedVm, roots: &mut Vec<ObjectRef>) {
    roots.extend(
        g1_pending_finalizer_mark_roots(shared.mem.finalizer_thread.pending_addresses()),
    );
}

/// gci i19/fz: the root words of [`append_pending_finalizer_mark_roots`]
/// (null entries dropped). Split out so the selection is tested without a heap.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_pending_finalizer_mark_roots(pending: Vec<usize>) -> impl Iterator<Item = ObjectRef> {
    pending
        .into_iter()
        .filter(|&addr| addr != 0)
        // SAFETY: an address the finalizer queue holds, relocated by every
        // collection; a mark root is only carried and screened by the marker
        // (`mark_root_seed`), never dereferenced here.
        .map(|addr| unsafe { ObjectRef::from_raw(addr as *mut u8) })
}

// ---------------------------------------------------------------------------
// gci i31/gq: G-2 of `gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`
// ---------------------------------------------------------------------------
//
// HotSpot's G1 `System.gc()` is ONE full collection: a dead finalizable `X` is
// finalizer-reachable, so a soft / weak reference to an object only `X`
// reaches is cleared before `finalize()`. This VM's is the door's young pause
// (which resurrects and QUEUES `X`) followed by the stop-the-world full mark
// (`g1_stw_full_mark_cycle`), which roots the finalizer queue
// (`append_pending_finalizer_mark_roots`, gci i19/fz): an Old child `C` only
// `X` reaches read marked there and kept its weak reference.
//
// `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES` (default ON since the
// 2026-10-17 user-approved WARNING flip, `=0` kills; marker
// `WARNING gci-i48-flipB-20261017`; a pause-shape change, and a missed
// mark of a held-out object would be a use-after-free, so its soundness rows
// came first):
//
// 1. `run_collection_pause` on a full-collection door carries the pause's
//    `dead_finalizers` (queued, post-move) with the collection count
//    (`g1_carry_door_queued_finalizables`).
// 2. `g1_stw_full_mark_cycle` takes the carry when no collection ran since,
//    keeps the entries the queue still holds, and leaves exactly those out of
//    the queue's mark roots (`g1_hold_out_mark_roots`: only the section the
//    queue appended -- a thread or static root naming `X` still roots it).
// 3. Its remark callback, FIRST: the unmarked closure of the held-out objects
//    (`g1_remark_reference_closure` on the completed bitmap), then marks the
//    held-out objects and drains their closure (`g1_remark_mark_kept_closure`)
//    BEFORE anything reads the bitmap for a verdict that frees (the JNI weak
//    clear, class unloading, cleanup), then stages the closure as the remark
//    round's `resurrected` set (`remark_reference_rows` takes it): soft and
//    weak references into it are cleared, final and phantom keep it, as the
//    Generational remark treats its retention closure (gen r5w5/conc9). The
//    mark happens before the round, not after it (the page's step 4): the
//    round reads the noted set, not the bitmap, for those objects, so the
//    order costs no precision and the memory-safety step cannot be skipped by
//    an early return of the round.





/// zt t31c: releases the door's hold (`g1_carry_door_queued_finalizables`)
/// when the door ends, however it ends, and wakes the delivery thread if a
/// hold was released anywhere in between (it may have found only held entries
/// and gone back to sleep). The stop-the-world full mark releases it first, inside
/// its pause, as it takes the carry. Armed only once this door collected (only
/// its own pause can have set the hold), so a door that coalesced never ends
/// another door's hold.
pub(in crate::runtime::interpreter::gc_and_alloc) struct G1DoorHeldFinalizables<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) shared: &'a SharedVm,
    pub(in crate::runtime::interpreter::gc_and_alloc) armed: bool,
}

impl Drop for G1DoorHeldFinalizables<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let ft = &self.shared.mem.finalizer_thread;
        ft.release_held();
        if ft.take_hold_released() && ft.pending_count() > 0 {
            let _ = request_finalizer_thread(self.shared, false);
        }
    }
}

/// gci i31/gq (G-2): the doors whose collection is a full collection
/// (`force_full_collection`: the young pause, then the marking cycle). The
/// same set `run_collection_pause` counts as `System.gc()`-door collections.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_collection_door(door: GcDoor) -> bool {
    matches!(
        door,
        GcDoor::SystemGc
            | GcDoor::MetadataThreshold
            | GcDoor::DiagnosticCommand
            | GcDoor::HeapInspection
            | GcDoor::HeapDump
            | GcDoor::JvmtiForceGc
    )
}

/// gci i31/gq (G-2), step 1: `run_collection_pause`'s carry, after
/// `enqueue_resurrected_finalizers` (the addresses are post-move and queued).
/// Replaces any earlier carry, an empty one included. No-op off G1, on any
/// other door, or with the switch off.
///
/// gci i32/ec (`i31-gq-report.md` review item 2): on the
/// `-XX:+ExplicitGCInvokesConcurrent` arm the carry is written and never
/// read (only `g1_stw_full_mark_cycle` takes it; the next collection's stamp
/// voids it), and that is HotSpot's answer, not a gap: HotSpot's
/// concurrent `System.gc()` is a concurrent-start young pause that enqueues
/// the dead finalizable `X` (its `FinalReference` made inactive), then a
/// concurrent mark in which `X` is strongly reachable from `Finalizer`'s
/// queue / unfinalized list, so an Old child `C` only `X` reaches is marked
/// and a weak reference to it survives that cycle's remark. This VM's
/// concurrent initial mark roots the queue the same way
/// (`append_pending_finalizer_mark_roots`). G-2 is a property of the ONE
/// stop-the-world full collection only.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_carry_door_queued_finalizables(shared: &SharedVm, door: GcDoor, queued: &[usize]) {
    if !shared.mem.heap.is_g1()
        || !g1_full_collection_door(door)
    {
        return;
    }
    let stamp = shared.mem.heap.collection_count();
    shared
        .mem
        .ref_processor
        .lock()
        .carry_door_queued_finalizables(queued, stamp);
    // zt t31c: inside the pause that queued them, so the delivery thread never
    // sees them unheld. Only when a stop-the-world full mark follows to take
    // them (the concurrent arm's mark roots the queue; HotSpot's too).
    if !queued.is_empty()
        && (g1_door_forces_stw_full_cycle(door)
            || g1_force_full_cycle_is_stw(shared.config.explicit_gc))
    {
        shared.mem.finalizer_thread.hold_until_judged(queued);
    }
}

/// gci i31/gq (G-2), step 2: inside the stop-the-world full mark, the carried
/// finalizables that are still valid (no collection since the carry, so
/// nothing moved) and still in the finalizer queue (the delivery thread may
/// have taken one meanwhile); ascending, each once. Empty off G1 or with the
/// switch off.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_take_door_queued_held_out(shared: &SharedVm) -> Vec<usize> {
    // zt t31c: inside this pause the delivery thread is parked; the carry is
    // judged here, so the door's hold ends here (whatever the switches say).
    shared.mem.finalizer_thread.release_held();
    if !shared.mem.heap.is_g1() {
        return Vec::new();
    }
    let stamp = shared.mem.heap.collection_count();
    let carried = shared
        .mem
        .ref_processor
        .lock()
        .take_carried_door_queued_for(stamp);
    if carried.is_empty() {
        return carried;
    }
    let carried_len = carried.len();
    let held_out =
        g1_door_queued_still_queued(carried, shared.mem.finalizer_thread.pending_addresses());
    // zt t31c (`CRATONVM_DBG_REFPROC_REMARK`): a carry the delivery thread
    // emptied before this mark shows as `still_queued` below `carried`.
    if None::<std::ffi::OsString>.is_some() {
        eprintln!(
            "[refproc-remark] door-carry carried={carried_len} still_queued={}",
            held_out.len()
        );
    }
    held_out
}

/// gci i31/gq (G-2): `carried` that `pending` also holds, null dropped;
/// ascending, each once. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_door_queued_still_queued(carried: Vec<usize>, mut pending: Vec<usize>) -> Vec<usize> {
    pending.sort_unstable();
    let mut out: Vec<usize> = carried
        .into_iter()
        .filter(|&a| a != 0 && pending.binary_search(&a).is_ok())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// gci i31/gq (G-2), step 2: drop from `roots[from..]` (the section
/// `append_pending_finalizer_mark_roots` appended) every word in `held_out`
/// (ascending). Earlier sections are untouched: a thread, static or native
/// root naming a held-out object still roots it. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_hold_out_mark_roots(roots: &mut Vec<ObjectRef>, from: usize, held_out: &[usize]) {
    if held_out.is_empty() || from >= roots.len() {
        return;
    }
    let tail: Vec<ObjectRef> = roots
        .drain(from..)
        .filter(|r| held_out.binary_search(&(r.as_ptr() as usize)).is_err())
        .collect();
    roots.extend(tail);
}

/// gci i31/gq (G-2), step 3, at the head of the stop-the-world full mark's
/// remark callback (marker joined, remark drained, nothing freed): the
/// unmarked closure of `held_out` is computed on the completed bitmap, the
/// held-out objects are marked and their closure drained
/// ([`g1_remark_mark_kept_closure`]: the heap's own resurrection marker, which
/// follows the side edges and hides referent slots as the mark does), and the
/// closure is staged as the remark round's `resurrected` set. Nothing to do
/// with an empty `held_out`. Lock order: the class manager is read and
/// released before the marker runs; the reference processor is taken alone.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_hold_out_door_queued(
    shared: &SharedVm,
    is_live: &dyn Fn(usize) -> bool,
    held_out: &[usize],
) {
    if held_out.is_empty() {
        return;
    }
    let mut reached = 0usize;
    let closure: Vec<usize> = {
        let class_manager = shared.classes.class_manager.read();
        let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
        // As `remark_reference_rows`' shape test: an array is never a
        // `Reference` (a `Reference[]` carries its component's class id).
        let is_reference_shaped = |obj: ObjectRef| -> bool {
            if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
                return false;
            }
            match reference_cid {
                Some(cid) => class_manager.is_subclass_of(shared.mem.heap.class_id_of(obj), cid),
                None => true,
            }
        };
        let full = g1_remark_reference_closure(shared, is_live, &is_reference_shaped, held_out);
        reached = full.len();
        full.into_iter().filter(|&a| !is_live(a)).collect()
    };
    // Memory safety first: everything the held-out objects reach is marked
    // before any verdict of this callback can free it.
    g1_remark_mark_kept_closure(shared, held_out);
    if None::<std::ffi::OsString>.is_some() {
        eprintln!(
            "[refproc-remark] door-queued held_out={} closure={} reached={reached}",
            held_out.len(),
            closure.len()
        );
    }
    if !closure.is_empty() && !no_refproc() {
        shared
            .mem
            .ref_processor
            .lock()
            .stage_remark_resurrected(closure);
    }
}

/// gci i31/gq (G-2): after the remark callback's reference pass, forget a
/// staged set the round did not take (`CRATONVM_DBG_NO_REFPROC` returns before
/// it); `finish_pre_gc_cycle` empties it too. No lock with an empty `held_out`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_unstage_door_queued(shared: &SharedVm, held_out: &[usize]) {
    if held_out.is_empty() {
        return;
    }
    let _ = shared
        .mem
        .ref_processor
        .lock()
        .take_remark_staged_resurrected();
}


// ---------------------------------------------------------------------------
// zr z1/br: the take-over cover of the ZGC concurrent mark start
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jit-barrier-lowering.md`.
//
// Every compiled reference store tests the heap's SATB pre gate
// (`JitRefStoreGates::pre_active`) and then stores, with no safepoint poll in
// between; every inline ZGC allocation tests the `blocked` word and then sets
// the start bit. The gates' own docs argue soundness from "armed only inside a
// stop-the-world pause, and nothing between the test and the store is a
// safepoint". That holds for a cooperative pause and NOT for the cross-thread
// take-over (`stw_take_over_and_wait`), which OS-suspends a peer at ANY
// instruction of JIT code. The mark-start pause takes over, arms the barrier
// (`zgc_start_concurrent_mark` -> `set_mark_active(true)`) and resumes the
// peer, which then:
//
// * stores over a reference without logging it. Another mutator that read
//   the old value after the snapshot and stored it into an already-scanned
//   object hides it from the marker: freed while live.
// * or registers an object (start bit) that is not black and was not a
//   registered root at the snapshot. Stored into a black object it is never
//   traced: freed while live.
//
// Interpreted code and Rust helpers are not exposed: a take-over resumes any
// peer whose PC is outside JIT code, and such a peer finishes its store before
// its next poll. So the cover is: a mark start that froze any peer does not
// open the concurrent cycle. A per-PC window rule would be tighter; see
// `zgc-zr1br-proposal-takeover-resumes-gate-windows-20261012.md`.



/// `CRATONVM_ZGC_SATB_TAKEOVER_COVER` (zr z1/br): default ON, `=0` restores the
/// unconditional open. Acts only when concurrent marking is configured
/// (`CRATONVM_ZGC_CONC_START`) and a mark start's take-over froze a peer.
/// Read per mark start (rare), not cached.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_satb_takeover_cover_enabled() -> bool {
    true
}

/// Does the cover refuse a mark start that froze `frozen` peers? `enabled` is
/// the switch, asked only when a peer was frozen.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_satb_window_refuses(frozen: usize, enabled: impl FnOnce() -> bool) -> bool {
    frozen != 0 && enabled()
}

/// Is this collection epoch's mark start already refused? The latch is
/// `collection_count() + 1` at the refusal; any later collection lifts it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_satb_window_latched(shared: &SharedVm) -> bool {
    let at = shared
        .mem
        .zgc_satb_window
        .refused_at
        .load(std::sync::atomic::Ordering::Relaxed);
    zgc_satb_window_latch_holds(at, shared.mem.heap.collection_count())
}

/// The latch rule, pure: `refused_at` holds `count + 1` from the refusal.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_satb_window_latch_holds(refused_at: u64, collection_count: u64) -> bool {
    refused_at != 0 && refused_at == collection_count.wrapping_add(1)
}

/// Count a refusal, latch it for this collection epoch, and name it under
/// `CRATONVM_DBG_ZGC_SATB_WINDOW` (the probe rows' positive control).
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_zgc_satb_window_refusal(shared: &SharedVm, frozen: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let state = &shared.mem.zgc_satb_window;
    let n = state.refusals.fetch_add(1, Relaxed) + 1;
    let gc = shared.mem.heap.collection_count();
    state.refused_at.store(gc.wrapping_add(1), Relaxed);
    if false {
        eprintln!("[zgc-satb-window] mark start refused: frozen={frozen} refusals={n} gc={gc}");    }}


/// Execute a full G1 concurrent marking cycle:
/// 1. Initial Mark (brief STW) — mark roots, activate SATB
/// 2. Concurrent Mark (background thread) — traverse heap regions
/// 3. Remark (brief STW) — drain SATB buffers, re-mark roots
/// 4. Cleanup — compute per-region liveness, free empty regions
pub(in crate::runtime::interpreter) fn g1_concurrent_mark_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    // Phase 1: Initial Mark — brief STW pause.
    // Activates SATB write barrier and marks root-reachable objects.
    //
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) instead of a request plus a plain `wait_for_all` (the deleted
    // `brief_stw*` helpers), which stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot (a missed mark root here = cleanup frees a
    // live object). Mark-only pause: the frozen peers' fresh conservative
    // roots are extra MARK roots; nothing moves, so no pin/pointer-map
    // concerns. gc-common w4-a: the request passes the identity census,
    // initiator slot included (`request_non_collection_pause`).
    let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) else {
        return; // Another STW in progress
    };
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        // JIT round 19 residual (`r19-an-unwind-out-of-a-pause-leaks-its-lazy-native-claims-FIXED-20261007`):
        // armed BEFORE the take-over, so an unwind anywhere in this pause
        // resumes the frozen peers and reopens the world (`GenConcPauseGuard`).
        let mut pause_guard = GenConcPauseGuard::arm(shared);
        pause_guard.hold(stw_take_over_and_wait(
            shared,
            &mut xt_roots,
            &counted_os_tids,
        ));
        // gc-common w5-a: `--verbose:gc` pause line (`door=g1-initial-mark`).
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::G1InitialMark);
        // Round-5 fix (CRIT — UAF): drain the initiator's per-thread
        // SATB buffer on the way into initial-mark. Other mutators
        // already flushed on their `safepoint_check` arrival; the
        // initiator hasn't, and any buffered overwrites from before
        // SATB activation must reach the global queue before the
        // marker starts consuming it.
        shared.mem.heap.flush_thread_satb();
        // SAFETY (I-17): `stw_take_over_and_wait` above has parked every other
        // mutator at a safepoint (or forcibly stopped and conservatively
        // scanned it), and `taken` is still held, so this thread is the only
        // mutator for the whole initial-mark block below. G1's mark-cycle entry
        // points now require this witness for the same reason `collect_garbage`
        // does: they reclassify and free regions.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        // INT-8: the referent-slot skip set for this cycle -- the
        // Weak/Soft/Phantom Reference OBJECT addresses currently registered.
        // Inside this STW the snapshot is consistent (no mutator can
        // construct, move, or free a Reference), and it must be in force
        // before anything is grayed so no Reference is ever scanned without
        // the skip. G1 carries the set across every mid-cycle evacuation
        // pause internally (remap survivors, prune CSet casualties).
        // gci i14/gq G-4: published INSIDE the cycle's start, before the
        // overlay snapshot grays an incomplete provider's rows and before the
        // marker is spawned (it used to be published after both).
        // Sorted (JIT round 20 wave 21): `frozen_peer_referents` below looks
        // its holders up in it (the skip set itself is a hash set).
        let mut ref_objs = shared.mem.ref_processor.lock().reference_object_addresses();
        ref_objs.sort_unstable();
        // g1ep7: the concurrent cycle's young regions are root regions.
        shared
            .mem
            .heap
            .g1_start_concurrent_mark_with_root_regions(&stw, &ref_objs);
        // gcd d10/r: this cycle's `metadata_pin` rows start empty and are the
        // initial mark's deferrals; every scan from here to the remark keeps
        // them (`roots::g1_cycle_keeps_metadata_rows`), so whatever a previous
        // cycle's remark left must not leak into this one.
        cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
        // Mark roots into the G1 mark bitmap
        let mut roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        // gci i15/vh (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`, default ON since 2026-10-17): this gather
        // keys the parked, all-old JDK continuations by their mirrors (their
        // frames become `metadata_pin` rows the cycle's marker follows from a
        // marked mirror; neither is a root), and step 10b's copy of each keyed
        // mirror leaves `roots`. No verdict here: only the remark judges.
        let g1_keyed = g1_key_parked_for_mark(shared);
        let mut snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        g1_keyed.strip_registry_mirrors(&mut roots);
        g1_keyed.forget_without_verdict();
        drop(g1_keyed);
        // gci i5g/g1r: a G1 in-native band (opt-in
        // `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1`) is a mark root here too.
        append_in_native_band_roots(shared, &mut snapshot_roots);
        // gci i19/fz: the objects queued for `finalize()` are in the snapshot
        // (`append_pending_finalizer_mark_roots`).
        append_pending_finalizer_mark_roots(shared, &mut snapshot_roots);
        // gci i12/wb: the frozen peers' objects again, separately (see below).
        let frozen_held: Vec<ObjectRef> = xt_roots.clone();
        // JIT round 20 wave 8 (lane huntGC): what the frozen peers' objects
        // hold NOW; the cycle's start above armed the SATB gate while
        // they were frozen (`frozen_peer_referents`). Wave 21 (lane g1ref):
        // never the hidden referent slot of a `Reference` in `ref_objs`.
        // Merge of origin/dev (2026-10-15): gci i12/wb's eager scan below
        // covers the same window; both kept (they compose: marks only).
        let frozen_referents = frozen_peer_referents(shared, &xt_roots, &ref_objs);
        // JIT r23 w1b L7 (R23W1L7-4): where each part of `all_roots` ends, for
        // the census.
        let census_ends = (
            roots.len(),
            roots.len() + snapshot_roots.len(),
            roots.len() + snapshot_roots.len() + xt_roots.len(),
        );
        let all_roots: Vec<cratonvm_types::ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .chain(frozen_referents.into_iter())
            .collect();
        // JIT r23 w1b L7 (`CRATONVM_DBG=root-kind-census`, off by default).
        root_kind_census_for_g1_mark(shared, "g1-initial-mark", &all_roots, census_ends);
        shared.mem.heap.g1_mark_roots(&stw, &all_roots);
        tracing::debug!("[G1] Initial mark: {} roots marked", all_roots.len());
        // gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 1):
        // a peer the takeover froze between a compiled store's SATB decision
        // (the g1put pre-gate test, or the single-pass `aastore`'s helper
        // answering "inactive") and the store resumes after this pause and
        // overwrites without logging. Scan every object it holds NOW, so what
        // those objects name at the snapshot is gray before it resumes: G1's
        // twin of the generational `initial_mark_with_frozen`. Empty unless the
        // takeover froze someone. `CRATONVM_G1_CONC_FROZEN_EAGER_SCAN=0` kills it.
        if !frozen_held.is_empty()
        {
            let scanned = shared.mem.heap.g1_scan_frozen_held_objects(&stw, &frozen_held);
            tracing::info!(
                target: "cratonvm::g1_frozen_scan",
                "[G1] initial mark: frozen_roots={} frozen_objects_scanned={}",
                frozen_held.len(),
                scanned
            );
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        // gci i27/ob (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
        // this pause opened the cycle, so the young pause that asked for it
        // reports now as `Pause Young (Concurrent Start)`, before this pause
        // prints `GC(c) Concurrent Mark Cycle` -- HotSpot's order.
        super::gc_events::g1_emit_deferred_young_report(shared, thread.thread_id, true);
        non_collection_pause_finish(pause_timer);
    }

    // Phase 2: Concurrent Mark — the background worker that drains
    // the mark worklist is owned by the VmHeap-level
    // `ConcurrentMarkController` (task #56). `g1_start_concurrent_mark`
    // above spawned it during the initial-mark STW, so by the time we
    // get here the marker is already running concurrently with mutators.
    //
    // Phases 3+4 (STW final remark + cleanup) are driven by
    // `maybe_concurrent_gc`: after each subsequent young collection the
    // GC-initiating mutator checks `g1_concurrent_mark_finished()` and,
    // once the worker has drained to a fixed point, runs
    // `g1_final_remark_cleanup` under its own brief STW. The previous
    // design — a detached watcher thread polling quiescence and calling
    // `g1_signal_marking_complete` (i.e. cleanup) directly — never ran a
    // final remark at all: the SATB log was discarded wholesale and the
    // roots were never re-scanned, so the sweep verdicts raced every
    // reference the mutators rewrote during the concurrent phase. A
    // watcher thread also cannot run the remark itself: it has no
    // JvmThread/barrier context to initiate an STW.
    //
    // Deferral note: if allocation stops entirely after IHOP fired, no
    // young GC follows and the cycle stays open (SATB active, cleanup
    // pending). That is benign — a heap nobody allocates into needs no
    // reclamation — and the next allocation-triggered GC closes it.
}

/// Phases 3+4 of the G1 cycle: STW final remark, then cleanup.
///
/// Called by `maybe_concurrent_gc` on the GC-initiating mutator once the
/// background marker has quiesced. Collects the full root set (all
/// threads) under a brief STW and hands it to
/// [`VmHeap::g1_final_remark_and_cleanup`], which re-marks roots, drains
/// the SATB log, completes the transitive closure, and runs cleanup —
/// all while the world is stopped.
///
/// fork6-pattern race handling: if another thread's STW wins
/// (the request returns false), nothing ran — the cycle simply
/// stays open (SATB active, worklist quiescent) and the next
/// `maybe_concurrent_gc` retries. Unlike the generational remark there is
/// nothing to abort: no sweep decision has been made yet.
///
/// LANE W7-D: returns whether the pause actually RAN. A lost STW race and a
/// completed cycle used to be the same `()`, so the mark-cycle door census
/// could not tell "this door finished the cycle" from "this door tried and was
/// beaten", and those have different meanings for a default decision — the
/// first says the door works, the second says nothing at all.
pub(in crate::runtime::interpreter) fn g1_final_remark_cleanup(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) so a never-polling in-JIT peer neither stalls the pause
    // forever nor is covered only by its stale deposit snapshot (a missed
    // mark root here = cleanup frees a live object). Unlike the young/mixed
    // pauses nothing moves, so no pins — but `cleanup` DOES linearly walk
    // every non-Free region computing live bytes, so the takeover's
    // frozen-TLAB-tail publication (consumed by the region walkers' skip
    // checks) is load-bearing here too.
    //
    // Finding 1(a): remark pauses use the identity census too, so blocked
    // threads are excluded BY IDENTITY and their wake-time arrivals cannot
    // satisfy this pause's quota (`arrive_and_wait_auto`). gc-common w4-a: the
    // census also says whether the initiator owns a counted slot
    // (`request_non_collection_pause`).
    if let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        // JIT round 19 residual (`r19-an-unwind-out-of-a-pause-leaks-its-lazy-native-claims-FIXED-20261007`):
        // armed BEFORE the take-over, so an unwind anywhere in this pause
        // resumes the frozen peers and reopens the world (`GenConcPauseGuard`).
        let mut pause_guard = GenConcPauseGuard::arm(shared);
        pause_guard.hold(stw_take_over_and_wait(
            shared,
            &mut xt_roots,
            &counted_os_tids,
        ));
        // gc-common w5-a: `--verbose:gc` pause line (`door=g1-remark`) — the
        // pause `common-w4a-non-collection-pauses-have-no-pause-line` was
        // filed for: a fixed-point drain, remark-time reference processing
        // and a walk of every non-Free region, previously timed nowhere.
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::G1Remark);
        // Drain the initiator's per-thread SATB buffer; the other
        // mutators' buffers are pulled by `remark` itself
        // (`flush_all_thread_satb_buffers`) now that they are parked.
        shared.mem.heap.flush_thread_satb();
        let mut roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        // gci i15/vh (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`, default ON since 2026-10-17): the
        // remark keys the parked, all-old JDK continuations as the initial mark
        // did (rows owned by the mirror, no root, step 10b's mirror copy out of
        // `roots`); the callback below judges them on the completed mark and
        // the pause retires the unmarked ones once cleanup trusted that mark.
        let g1_keyed = g1_key_parked_for_mark(shared);
        let mut snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        g1_keyed.strip_registry_mirrors(&mut roots);
        // gci i5g/g1r: a G1 in-native band (opt-in
        // `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1`) is a mark root here too:
        // the remark's cleanup frees what this mark leaves unmarked.
        append_in_native_band_roots(shared, &mut snapshot_roots);
        // gci i19/fz: and so are the objects queued for `finalize()`, so the
        // cleanup neither scrubs nor frees one before it runs
        // (`append_pending_finalizer_mark_roots`).
        append_pending_finalizer_mark_roots(shared, &mut snapshot_roots);
        // JIT r23 w1 L7: where each part of `all_roots` ends, for the census.
        let census_ends = (
            roots.len(),
            roots.len() + snapshot_roots.len(),
            roots.len() + snapshot_roots.len() + xt_roots.len(),
        );
        let all_roots: Vec<cratonvm_types::ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .collect();
        // JIT r23 w1 L7 (`CRATONVM_DBG=root-kind-census`, off by default).
        root_kind_census_for_g1_mark(shared, "g1-remark", &all_roots, census_ends);
        // INT-8: run VM reference processing against the completed mark
        // bitmap between the remark drain and cleanup — the only point
        // in the cycle where a weak/soft ref to a dead OLD-region
        // referent can be observed dead (evacuation pauses only ever
        // see CSet deaths). The callback returns the addresses the
        // heap must resurrect before cleanup's in-place frees.
        // gci i8/jf: the callback's end is where the remark's work ends and
        // the cleanup begins (a recording splits the pause there into HotSpot
        // G1's `Pause Remark` and `Pause Cleanup` rows); `None` if it never ran.
        let cleanup_at: std::cell::Cell<Option<std::time::Instant>> = std::cell::Cell::new(None);
        let mut process = |is_live: &dyn Fn(usize) -> bool| -> Vec<usize> {
            // gci i16/mr (`gci-i15vh-g1-marking-scan-rows-filed-while-the-marker-runs-FIXED-20261017`,
            // default path): both marking scans file `metadata_pin` rows while
            // the concurrent marker runs, so an owner it scanned before its row
            // existed is black without that row's values. Close every row
            // over the completed mark FIRST, before any reference, class,
            // loader or weak verdict below reads the bitmap.
            let _ = g1_remark_close_metadata_rows(shared, is_live);
            // Merge of origin/dev (2026-10-15): this callback is the
            // concurrent twin of `g1_remark_reference_pass` (the STW full
            // cycle's), kept inline for the gci steps around it; dev's gc
            // wave 9 lane F step comes first there and here: the
            // inherited-`ThreadLocal` ephemeron, before reference processing
            // reads the bitmap.
            if crate::memory::native_roots::g1_itl_pending_ephemeron_enabled() {
                g1_remark_mark_inherited_tl_values(shared, is_live);
            }
            let keep = g1_remark_process_references(shared, is_live);
            // gci i14/gq G-8: on the `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP=0`
            // arm the keep closure is not marked yet (the default arm marked it
            // inside `g1_remark_process_references`), so the JNI clear below
            // read a finalizable's private child as dead while the heap
            // resurrects it right after this callback. Mark it first; it is
            // the heap's own resurrection, made earlier (idempotent).
            if !g1_remark_unload_after_keep_enabled() {
                g1_remark_mark_kept_closure(shared, &keep);
            }
            // gci i15/vh: the mirror-keyed parked threads, on the mark-exact
            // verdict, after the keep closure is marked on both arms (a mirror
            // reachable only through a kept soft referent or a finalizable
            // reads live) and BEFORE the weak clears below: when this cycle
            // keyed anything, the rows of every live owner are first marked
            // to a fixed point (`resurrect_after_remark`), which can only add
            // marks the verdicts below must see. Judged only: retired after
            // cleanup.
            let _ = shared
                .threads
                .thread_registry
                .judge_mirror_keyed_parked_after_mark(is_live, &mut |values: &[usize]| {
                    g1_remark_mark_kept_closure(shared, values)
                });
            // gc-common w8-c: JNI weak globals whose referent the completed
            // mark judged dead read NULL from here on, before cleanup frees a
            // region. A referent the reference pass is resurrecting (`keep`)
            // keeps its weak global.
            let _ = crate::native::jni::clear_weak_global_refs_unmarked(shared, is_live, &keep);
            // gci i21/sw (opt-in `CRATONVM_G1_WEAK_STRING_TABLE`): the pool's
            // verdict on the same bitmap; settled after cleanup below.
            let _ = crate::memory::string_table::g1_remark_judge_interned_strings(
                shared, is_live, &keep,
            );
            // gch h4/g1: the never-started threads' inherited-`ThreadLocal`
            // buckets, on the same mark-exact verdict -- only when the keep
            // closure is already marked (`g1_remark_mark_kept_closure`), so a
            // child the reference pass resurrects keeps its bucket.
            if g1_remark_unload_after_keep_enabled() && g1_remark_itl_sweep_enabled() {
                g1_remark_sweep_inherited_tl_buckets(shared, is_live);
            }
            cleanup_at.set(Some(std::time::Instant::now()));
            keep
        };
        // SAFETY (I-17): same pause as above — `stw_take_over_and_wait` parked
        // every other mutator and `taken` is still held. The remark drain and
        // cleanup that follow are STW phases; the token is their witness.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let completed =
            shared
                .mem
                .heap
                .g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process));
        tracing::debug!(
            "[G1] Final remark: {} roots, cycle_completed={}",
            all_roots.len(),
            completed
        );
        root_kind_census_after_g1_cleanup(shared, "g1-remark", completed);
        // gci i21/sw: the entries the callback judged dead leave the pool now,
        // inside the pause, only if this cleanup trusted its closure.
        let _ = crate::memory::string_table::g1_remark_settle_interned_strings(shared, completed);
        // gci i15/vh: retire the judged parked threads only when this cleanup
        // trusted its closure (an untrusted one freed nothing and proved
        // nothing); still inside the pause, before anything reads their words.
        let keyed_trusted = completed && g1_parked_keyed_mark_trusted(shared);
        let _ = shared
            .threads
            .thread_registry
            .settle_mirror_keyed_parked_after_mark(keyed_trusted);
        drop(g1_keyed);
        // Cleanup freed every region whose objects are all unmarked, and no
        // `update_all_roots` follows this pause, so the address-keyed side
        // tables are swept here (gc-common w3-g; `common-b-address-keyed-
        // sweeps-miss-concurrent-reclamation`). Still inside the STW, so no
        // allocation can reuse a freed region before the sweep sees it
        // (`is_object_address` declines a Free region outright). The GPU input
        // cache is swept here too, and not after the Generational concurrent
        // sweep: its drain-then-rekey protocol requires stopped mutators.
        // Only when the cycle completed: an incomplete remark freed nothing.
        if completed {
            // gci i22/g1rk (opt-in): first on the trusted mark, then as before.
            g1_remark_sweep_address_keyed_mark_exact(shared);
            crate::memory::addr_keyed::sweep_address_keyed_tables(shared);
            #[cfg(feature = "gpu-offload")]
            crate::runtime::offload::input_cache::remap_and_sweep(
                shared.vm_identity,
                &cratonvm_types::PointerMap::default(),
                &shared.mem.heap,
            );
        }
        super::gc_events::non_collection_pause_cleanup_point(&mut pause_timer, cleanup_at.get());
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
        // gci i11/go (R3): the remark's deferred `gc+ref` block.
        flush_remark_reference_lines(shared);
        // gci i15/vh: the execution states of the parked threads this remark
        // retired, dropped now the world is released (as the collection door
        // does). Empty unless `CRATONVM_GC_VT_PARKED_MIRROR_KEYED`.
        let collected = shared.threads.thread_registry.take_collected_parked();
        if !collected.is_empty() {
            crate::vm::jdk_continuation::retire_collected_continuations(shared, collected);
        }
        true
    } else {
        tracing::debug!("[G1] Final remark lost the STW race — retrying at next GC");
        false
    }
}

/// JIT round 20 wave 21 (lane g1ref) -- G1's whole marking cycle in ONE
/// stop-the-world pause: initial mark, the closure drained to a fixed point,
/// remark-time reference processing and cleanup, with no mutator running in
/// between. [`g1_force_full_cycle`]'s fresh cycle, i.e. `System.gc()` (and the
/// pre-OOM ladder), under every `VmConfig::explicit_gc` policy but
/// `-XX:+ExplicitGCInvokesConcurrent`.
///
/// # Why not the concurrent cycle
///
/// HotSpot's G1 answers `System.gc()` with a stop-the-world FULL collection:
/// no mutator runs between the snapshot and reference processing, so nothing a
/// mutator does can keep a weakly-reachable referent alive. The concurrent
/// cycle this VM ran instead opens a window between the initial mark and the
/// remark in which every `Reference.get()` -- interpreted, native or compiled
/// (`jit_getfield`'s referent arm) -- logs its referent through the SATB
/// keep-alive, as it must while marking is active. A thread spinning on
/// `while (r.get() != null)` therefore marked the referent in EVERY cycle and
/// never saw it cleared, while HotSpot prints the clear
/// (`docs/feature-designs/concurrent-gc-maturation.md`).
/// In the interpreter the young pause before the cycle happened to clear it:
/// a G1 young pause judges a referent by region (`is_addr_in_live_region`),
/// and only a compiled spinner's conservative register words JIT-pin the
/// region holding the `Reference` and, beside it, its referent.
///
/// Here the bitmap verdict is the snapshot's: `g1_final_remark_and_cleanup`
/// joins the marker the initial mark spawned and drains the closure inside
/// this same pause, and the referent slots stay hidden (`ref_objs`), so a
/// referent no root reaches is cleared by `g1_remark_reference_pass`. The pause
/// line is `G1Remark`'s: that pause already carries the fixed-point drain.
///
/// Returns whether the pause ran (`false`: another thread's STW won the race
/// and nothing was done).
pub(in crate::runtime::interpreter) fn g1_stw_full_mark_cycle(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) else {
        return false; // Another STW in progress
    };
    let mut xt_roots: Vec<ObjectRef> = Vec::new();
    // Armed BEFORE the take-over, so an unwind anywhere in this pause resumes
    // the frozen peers and reopens the world (`GenConcPauseGuard`).
    let mut pause_guard = GenConcPauseGuard::arm(shared);
    pause_guard.hold(stw_take_over_and_wait(
        shared,
        &mut xt_roots,
        &counted_os_tids,
    ));
    let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::G1Remark);
    // The initiator's per-thread SATB buffer, as the initial mark drains it;
    // the parked mutators flushed theirs on arrival.
    shared.mem.heap.flush_thread_satb();
    // SAFETY (I-17): `stw_take_over_and_wait` above has parked every other
    // mutator at a safepoint (or forcibly stopped and conservatively scanned
    // it), and `taken` is still held, so this thread is the only mutator for
    // the whole block below, the initial mark, the drain and the cleanup.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    // INT-8: the referent-slot skip set, before any root seeds the gray set
    // (see `g1_concurrent_mark_cycle`). Sorted for `frozen_peer_referents`.
    // gci i14/gq G-4 (merge of origin/dev, 2026-10-15): published inside the
    // cycle's start, before the spawned marker can scan a `Reference`.
    let mut ref_objs = shared.mem.ref_processor.lock().reference_object_addresses();
    ref_objs.sort_unstable();
    shared
        .mem
        .heap
        .g1_start_concurrent_mark_with_reference_skip(&stw, &ref_objs);
    // gcd d10/r: this cycle's `metadata_pin` rows start empty.
    cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
    // gci i31/gq (G-2, default-ON `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES`):
    // the finalizables this full collection's own young pause queued, still
    // queued, with no collection since. Empty otherwise (and under `=0`).
    let door_queued = g1_take_door_queued_held_out(shared);
    let roots =
        cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
    let mut snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
    // gci i5g/g1r and i19/fz (merge of origin/dev, 2026-10-15): the G1
    // in-native band and the objects queued for `finalize()` are mark roots
    // of this cycle too, as of the concurrent one; without the second the
    // cleanup scrubs a queued finalizable's fields (`System.gc()` on G1
    // takes this door by default since dev's STW full cycle).
    append_in_native_band_roots(shared, &mut snapshot_roots);
    let fin_from = snapshot_roots.len();
    append_pending_finalizer_mark_roots(shared, &mut snapshot_roots);
    // gci i31/gq (G-2): the door's own queued finalizables are not roots of
    // this mark (finalizer-reachable only, as in HotSpot's one full
    // collection); the remark callback marks them and their closure first.
    g1_hold_out_mark_roots(&mut snapshot_roots, fin_from, &door_queued);
    // What the frozen peers' objects hold now, never a hidden referent. No
    // frozen peer can resume inside this cycle, so this only over-marks; kept
    // so the root set is the concurrent initial mark's, exactly.
    let frozen_referents = frozen_peer_referents(shared, &xt_roots, &ref_objs);
    // JIT r23 w1 L7: where each part of `all_roots` ends, for the census.
    let census_ends = (
        roots.len(),
        roots.len() + snapshot_roots.len(),
        roots.len() + snapshot_roots.len() + xt_roots.len(),
    );
    let all_roots: Vec<ObjectRef> = roots
        .into_iter()
        .chain(snapshot_roots.into_iter())
        // INT-3 — frozen in-JIT peers' conservative register/stack roots.
        .chain(xt_roots.into_iter())
        .chain(frozen_referents.into_iter())
        .collect();
    // JIT r23 w1 L7 (`CRATONVM_DBG=root-kind-census`, off by default).
    root_kind_census_for_g1_mark(shared, "g1-stw-full-mark", &all_roots, census_ends);
    shared.mem.heap.g1_mark_roots(&stw, &all_roots);
    let mut process = |is_live: &dyn Fn(usize) -> bool| -> Vec<usize> {
        // gci i31/gq (G-2): mark the held-out objects' closure FIRST (before
        // any verdict of this callback can free it) and stage it as the
        // round's `resurrected` set; no-ops with nothing held out.
        g1_remark_hold_out_door_queued(shared, is_live, &door_queued);
        let keep = g1_remark_reference_pass(shared, is_live);
        g1_remark_unstage_door_queued(shared, &door_queued);
        keep
    };
    let completed =
        shared
            .mem
            .heap
            .g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process));
    // gci i31/gq (G-2): a cycle that did not complete returned before its
    // remark and callback (no controller) and freed nothing; the next cycle
    // marks from its own roots, the queue included (the carry was taken).
    tracing::debug!(
        "[G1] stop-the-world full mark: {} roots, cycle_completed={}",
        all_roots.len(),
        completed
    );
    root_kind_census_after_g1_cleanup(shared, "g1-stw-full-mark", completed);
    // gci i21/sw: as `g1_final_remark_cleanup` -- the judged pool entries go
    // now, inside the pause, only if this cleanup trusted its closure.
    let _ = crate::memory::string_table::g1_remark_settle_interned_strings(shared, completed);
    // As `g1_final_remark_cleanup`: cleanup freed regions in place and no
    // `update_all_roots` follows, so the address-keyed tables are swept here,
    // inside the pause.
    if completed {
        // gci i22/g1rk (opt-in): first on the trusted mark, then as before.
        g1_remark_sweep_address_keyed_mark_exact(shared);
        crate::memory::addr_keyed::sweep_address_keyed_tables(shared);
        #[cfg(feature = "gpu-offload")]
        crate::runtime::offload::input_cache::remap_and_sweep(
            shared.vm_identity,
            &cratonvm_types::PointerMap::default(),
            &shared.mem.heap,
        );
    }
    // Clear TLAB skip regions + resume frozen peers BEFORE reopening the world
    // (same race rationale as maybe_gc's epilogue).
    non_collection_pause_seal(&mut pause_timer);
    let taken = pause_guard.release();
    retire_skip_spans_and_resume(shared, taken);
    shared
        .mem
        .gc_barrier
        .complete_gc(cratonvm_types::PointerMap::default());
    non_collection_pause_finish(pause_timer);
    // gci i20/mr finding 2: the reference pass deferred its `-Xlog:gc+ref`
    // block (`remark_reference_rows`) exactly as the concurrent remark's does;
    // print it now the world is released, as `g1_final_remark_cleanup` does
    // (gci i11/go R3). Without it `System.gc()` on G1, which takes this door
    // by default since the merge, printed no `gc+ref` block (the next flush
    // drops a block whose id moved).
    flush_remark_reference_lines(shared);
    true
}

/// Does [`g1_force_full_cycle`] start its fresh cycle as the stop-the-world
/// [`g1_stw_full_mark_cycle`]? Every policy but `-XX:+ExplicitGCInvokesConcurrent`,
/// under which HotSpot's G1 also answers `System.gc()` with a concurrent cycle.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_force_full_cycle_is_stw(policy: crate::config::ExplicitGcPolicy) -> bool {
    policy != crate::config::ExplicitGcPolicy::Concurrent
}

/// The remark's reference pass (the callback `g1_final_remark_and_cleanup`
/// runs between the remark drain and cleanup): the inherited-`ThreadLocal`
/// ephemeron, the reference rows, the JNI weak globals and the never-started
/// threads' inherited buckets, all on the completed bitmap. Returns what the
/// heap must resurrect before cleanup. Shared by [`g1_final_remark_cleanup`]
/// and the stop-the-world full cycle [`g1_stw_full_mark_cycle`] (JIT round 20
/// wave 21, lane g1ref: one body, so the two remarks cannot drift).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_reference_pass(shared: &SharedVm, is_live: &dyn Fn(usize) -> bool) -> Vec<usize> {
    // gci i16/mr (merge of origin/dev, 2026-10-15): the cycle's
    // `metadata_pin` rows closed over the completed mark first, as the
    // concurrent remark does (only marks values of live owners).
    // gci i20/mr finding 3: FIRST, before the ephemeron below, in the
    // concurrent remark's order (`both_g1_remarks_share_one_reference_pass`
    // pins the order now): every later step reads the closed bitmap.
    let _ = g1_remark_close_metadata_rows(shared, is_live);
    // gc wave 9 lane F: the inherited-`ThreadLocal` ephemeron (the
    // initial mark and this remark pinned the values to their child
    // `Thread`): a live child's values are marked BEFORE reference
    // processing reads the bitmap (and once more after the keep
    // closure, inside `g1_remark_process_references`).
    if crate::memory::native_roots::g1_itl_pending_ephemeron_enabled() {
        g1_remark_mark_inherited_tl_values(shared, is_live);
    }
    let keep = g1_remark_process_references(shared, is_live);
    // gci i14/gq G-8 (merge of origin/dev, 2026-10-15): on the
    // `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP=0` arm the keep closure is marked
    // before the JNI clear, as the concurrent remark does.
    if !g1_remark_unload_after_keep_enabled() {
        g1_remark_mark_kept_closure(shared, &keep);
    }
    // gc-common w8-c: JNI weak globals whose referent the completed
    // mark judged dead read NULL from here on, before cleanup frees a
    // region. A referent the reference pass is resurrecting (`keep`)
    // keeps its weak global.
    let _ = crate::native::jni::clear_weak_global_refs_unmarked(shared, is_live, &keep);
    // gci i21/sw (opt-in `CRATONVM_G1_WEAK_STRING_TABLE`): the pool's verdict
    // on the same bitmap, as the concurrent remark takes it; the caller
    // settles it after cleanup.
    let _ = crate::memory::string_table::g1_remark_judge_interned_strings(shared, is_live, &keep);
    // gch h4/g1: the never-started threads' inherited-`ThreadLocal`
    // buckets, on the same mark-exact verdict -- only when the keep
    // closure is already marked (`g1_remark_mark_kept_closure`), so a
    // child the reference pass resurrects keeps its bucket.
    if g1_remark_unload_after_keep_enabled() && g1_remark_itl_sweep_enabled() {
        g1_remark_sweep_inherited_tl_buckets(shared, is_live);
    }
    keep
}

/// INT-8 — remark-time reference processing (G1 only). Runs INSIDE the
/// final-remark STW, after the gray set drained to a fixed point and BEFORE
/// `cleanup()` frees anything, with `is_marked` = the collector's
/// bitmap+TAMS verdict. This is the HotSpot-shaped point where weak/soft
/// references to dead OLD-region referents finally clear: with referent-slot
/// hiding, the bitmap holds an untainted verdict for every referent, and the
/// young-pause processing path (whose `is_marked` treats every non-CSet
/// region as live) can never see these deaths.
///
/// Mirrors `process_references_after_gc`'s consumer protocol with two
/// deliberate differences:
/// - no pointer map (nothing moved in this pause) — the staleness guard is
///   dead-BY-MARK instead: a Reference/queue that is itself unmarked is
///   skipped (writing through it would be resurrection-by-side-effect right
///   before its region is freed);
/// - referent clears use the SATB-suppressed store: the clear is the
///   processor's decided verdict, and SATB-logging the old referent would
///   feed it straight back into the resurrection drain that follows.
///
/// Returns every address that must stay live through this cycle's cleanup:
/// dead finalizables about to run `finalize()` (this closes the
/// finalize-never-runs gap for in-place-freed regions), submitted cleaner
/// actions, pending cleaner chains, and policy-retained soft referents.
///
/// # gch h4/g1 -- reference rows first, class unloading on the final bitmap
///
/// `gch-h3os-g1-remark-unloads-classes-before-it-resurrects-soft-retained-referents-FIXED-20261012`:
/// this used to reconcile the class mirrors and the defining loaders, and
/// unload, on the raw bitmap FIRST, and run the reference rows LAST. The
/// marker hides every soft referent slot, so an object reachable only through
/// a soft reference the LRU policy KEEPS read unmarked when the mirrors were
/// judged, and was resurrected a moment later by the heap
/// (`resurrect_after_remark`). The JDK's `LambdaForm` caches are soft: a
/// `LambdaForm$MH` holder class was unloaded while the `MethodHandle ->
/// form -> vmentry -> MemberName.clazz` chain naming its mirror survived, and
/// every later GC notification on G1 failed (`class not found`). HotSpot's
/// remark runs reference processing (its keep-alive closure marks what the
/// policy keeps) and unloads classes afterwards, on the final marking; the
/// generational remark does the same (`gen_remark_unload_classes`).
///
/// Under `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP` (default ON, `=0` restores the
/// old order) the rows run first, the keep list is marked and its closure
/// drained HERE ([`g1_remark_mark_kept_closure`], the marker the heap runs
/// after this callback anyway, so the side edges -- `loader_pin`,
/// `mirror_pin`, `metadata_pin` -- are followed exactly as a mark follows
/// them), and every class / loader verdict below reads the final bitmap
/// through `is_marked` (a live read of the bitmap, not a snapshot). The heap's
/// own resurrection of the returned list is then a no-op. The caller's JNI
/// weak-global clear reads the same final bitmap, so a weak global to an object
/// behind a kept soft referent is no longer cleared while the object survives.
pub(in crate::runtime::interpreter) fn g1_remark_process_references(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
) -> Vec<usize> {
    // gch h4/g1: the reference rows FIRST and their keep closure marked (see
    // the doc). `None` = the old order, the rows run last.
    let early_keep = if g1_remark_unload_after_keep_enabled() {
        // gci i14/gq (G-1): see `g1_remark_note_round`.
        g1_remark_note_round(shared);
        let keep = remark_reference_rows(shared, is_marked, &[]).keep;
        g1_remark_mark_kept_closure(shared, &keep);
        // gc wave 9 lane F: a never-started child only the keep closure
        // reaches gets its pinned inherited values marked too, before the
        // class verdicts below and before cleanup.
        if !keep.is_empty() && crate::memory::native_roots::g1_itl_pending_ephemeron_enabled() {
            g1_remark_mark_inherited_tl_values(shared, is_marked);
        }
        Some(keep)
    } else {
        None
    };
    // G1's marker follows loader/mirror/metadata side edges during a full mark.
    // Reconcile those weak ownership tables against the completed bitmap before
    // cleanup frees dead regions and before the optional reference-processor
    // short-circuit.
    // G1's own mark bitmap has no separate roots Vec by this point (unlike
    // the Generational path this diagnostic was built for) -- `None` keeps
    // `CRATONVM_DBG_MIRRORPIN_WHY`'s verified-root check off for G1 rather
    // than feeding it a stale/empty vector that would misreport every real
    // root as unverified.
    crate::memory::gc::reconcile_class_mirrors(shared, is_marked, None);
    let no_moves = cratonvm_types::PointerMap::default();
    let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
        &shared.loader_identity,
        is_marked,
        &no_moves,
        None,
    );
    // Lambda-proxy loader pins: see `process_references_after_gc`.
    crate::runtime::invokedynamic::repin_lambda_proxy_loaders(shared.vm_identity);
    // The unloaded classes' compact layouts are RETAINED, as the generational
    // concurrent remark does (`remark_unload_dead_classes`): `cleanup` frees
    // only the wholly dead regions, so a dead compact instance of an unloaded
    // class in a region that also holds live objects stays in place until a
    // mixed collection evacuates that region, and every linear walk of the
    // region in between (an evacuation-failure scrub, a heap walk, the
    // class-id census itself) sizes it from that layout
    // (`object_instance_size` answers `IMPLAUSIBLE_BODY_SIZE` without one).
    // `g1_stw_layout_census` releases each layout once no instance is left.
    // `docs/feature-designs/concurrent-gc-maturation.md`.
    let unloaded = crate::memory::gc::with_retained_unloaded_layouts(|| {
        crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints)
    });
    if unloaded.classes_unloaded != 0 {
        shared.mem.concurrent_gc_state.note_class_unload(
            unloaded.loaders_unloaded,
            unloaded.classes_unloaded,
            unloaded.layouts_retained,
        );
        tracing::debug!(
            loaders = unloaded.loaders_unloaded,
            classes = unloaded.classes_unloaded,
            jit_entries = unloaded.jit_entries_retired,
            layouts_retained = unloaded.layouts_retained,
            "G1 class-loader metadata unloaded at final remark"
        );
    }
    // gcd d10/r: rebuild the `mirror_pin` registry from the tables the two
    // reconciles just pruned, as the post-pause path does after its own
    // reconcile. Without it the rows of a loader this remark judged dead
    // stayed keyed at the loader's address, naming its dead mirrors, until
    // the next stop-the-world epilogue -- after `cleanup` below had freed both
    // -- and the next concurrent mark (`MarkSideTables`) followed them from
    // whatever object was allocated at that address in between.
    crate::memory::gc::rebuild_mirror_pins(shared, &no_moves);
    match early_keep {
        Some(keep) => keep,
        // `=0`: the old order. G1's remark resurrects nothing before this
        // pass: no retention closure.
        None => {
            g1_remark_note_round(shared);
            remark_reference_rows(shared, is_marked, &[]).keep
        }
    }
}

/// gci i14/gq (`gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`
/// G-1): `CRATONVM_G1_REMARK_WEAK_BEFORE_FINAL` -- default ON since
/// 2026-10-17 (`runtime_flag_default_on`, `=0` kills). Read once per G1
/// remark; no `static`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_weak_before_final_enabled() -> bool {
    true
}

/// gci i14/gq G-1: under [`g1_remark_weak_before_final_enabled`], tell the
/// reference processor that the round [`remark_reference_rows`] is about to run
/// clears soft and weak references before finalization.
///
/// G1's remark is the one processing round that runs BEFORE the dead
/// finalizable objects are resurrected: `is_marked` is the completed bitmap, a
/// finalizable object in an Old region that died during the cycle is unmarked,
/// and `resurrected` is empty (`&[]` above). The processor's Phase 0 then makes
/// it and everything only it reaches "finalizer-reachable", and that closure
/// was folded into the soft and weak predicates, so a `WeakReference` to it (or
/// to its private child) survived the remark and `finalize()` could hand the
/// object back with the weak reference still naming it. HotSpot G1's remark
/// (`G1ConcurrentMark::weak_refs_work`, SoftWeakFinalRefsPhase before
/// KeepAliveFinalRefsPhase) clears them first; the Generational remark gets the
/// same answer by marking its dead finalizables first (gen r5w5/conc9,
/// `take_finalizer_retention_closure`). Finalization, the phantom / `Cleaner`
/// rule (`phantom_is_live` keeps the closure) and the keep list are unchanged:
/// the object and its closure are still resurrected by the keep list, only the
/// weak and soft references to them read null. Off: nothing is noted, the
/// remark as before.
///
/// gci i14/gq G-3: also, under [`g1_remark_soft_clock_now_enabled`], the round
/// ends the soft clock at "now" (`ReferenceProcessor::note_round_clock_now`),
/// HotSpot's `update_soft_ref_master_clock` after the concurrent-mark round;
/// off, the remark commits only the last `SoftReference.get()` time.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_note_round(shared: &SharedVm) {
    // `no_refproc`: `remark_reference_rows` returns before any round, and a
    // note nobody consumes must not reach the next collection's round.
    if no_refproc() {
        return;
    }
    let weak_before_final = g1_remark_weak_before_final_enabled();
    let clock_now = g1_remark_soft_clock_now_enabled();
    if weak_before_final || clock_now {
        let mut rp = shared.mem.ref_processor.lock();
        if weak_before_final {
            rp.note_weak_before_final_round();
        }
        if clock_now {
            rp.note_round_clock_now();
        }
    }
}

/// gci i14/gq G-3: `CRATONVM_G1_REMARK_SOFT_CLOCK_NOW` -- default OFF
/// (`runtime_flag_on`; soft retention timing). Read once per remark; no
/// `static`. See [`g1_remark_note_round`].
///
/// gci i48/zl2: G1's final remark AND the Generational concurrent remark's
/// reference round ([`gen_remark_note_soft_clock`]), despite the name (kept):
/// HotSpot's `update_soft_ref_master_clock` ends every
/// `process_discovered_references`. ZGC is outside this switch (audit zx48
/// A6 scopes it to these two remarks). Same default (OFF) and `=1` on both.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_soft_clock_now_enabled() -> bool {
    false
}

/// gci i48/zl2: the Generational concurrent remark's half of
/// [`g1_remark_soft_clock_now_enabled`]: under the switch, the remark's
/// reference round ([`gen_remark_process_references`]) ends the soft clock at
/// "now" (`ReferenceProcessor::note_round_clock_now`), as G1's remark does in
/// [`g1_remark_note_round`]. Off (the default): nothing is noted, the remark as
/// before. `no_refproc`: no round runs, so nothing is noted either.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_remark_note_soft_clock(shared: &SharedVm) {
    if no_refproc() || !g1_remark_soft_clock_now_enabled() {
        return;
    }
    shared.mem.ref_processor.lock().note_round_clock_now();
}

/// gch h4/g1: `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP` -- default ON
/// (`runtime_flag_default_on`), `=0` restores the remark's old order (class
/// unloading on the raw bitmap, reference rows last). See
/// [`g1_remark_process_references`]. Read once per G1 remark; no `static`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_unload_after_keep_enabled() -> bool {
    true
}

/// gch h4/g1: mark the remark's keep list (`remark_reference_rows`' `keep`:
/// dead finalizables, cleaner actions and chains, policy-kept soft referents)
/// and drain its closure to a fixed point, INSIDE the remark callback, so the
/// class and loader verdicts that follow and the JNI weak-global clear read the
/// final bitmap. It is the call the heap makes on the returned list right after
/// the callback (`VmHeap::g1_final_remark_and_cleanup`), made earlier; marking
/// twice is a no-op. Nothing is held across it: the reference processor and the
/// class manager were released by `remark_reference_rows`, and the heap holds
/// no lock while the callback runs. A no-op on an empty list or another heap.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_mark_kept_closure(shared: &SharedVm, keep: &[usize]) {
    if keep.is_empty() {
        return;
    }
    shared.mem.heap.g1_resurrect_after_remark(keep);
}

/// gci i16/mr (`gci-i15vh-g1-marking-scan-rows-filed-while-the-marker-runs-FIXED-20261017`):
/// close the cycle's `metadata_pin` rows over the completed mark, inside the
/// remark callback (marker joined, remark drained, nothing freed).
///
/// The initial mark spawns the concurrent marker
/// (`g1_start_concurrent_mark_with_reference_skip`) BEFORE its `collect_roots`
/// files the cycle's rows (loader statics, class locks, condy values,
/// inherited-`ThreadLocal` values of never-started threads), and the remark's
/// `collect_roots` adds rows while the same worker still runs. A batch that
/// scanned an owner before its row existed (or with side tables captured
/// before it, `MarkSideTables::is_current`) left the owner black without the
/// row's edges, and nothing rescans a black owner: a value reachable only
/// through the row read dead to the reference, class-unload and weak verdicts
/// and to cleanup. Here every row whose owner `is_live` hands its values to
/// [`g1_remark_mark_kept_closure`] (mark gray, drain to a fixed point; the
/// drain follows rows of anything it newly marks), repeated until no further
/// owner turns live (`mark_live_row_values_to_fixpoint`). This is the edge the
/// marker follows, applied to the final bitmap: every value marked is
/// reachable because its owner is, and every value is a heap object the
/// cycle's pauses kept and remapped (`metadata_pin::vm_row_addresses`).
/// Only THIS VM's rows are taken (gci i18/nr, gci i17/rv finding 9:
/// `metadata_pin::vm_snapshot`; the whole-process snapshot cloned every
/// VM's rows per remark and handed the others' values to a heap that skips
/// them, no region). Returns how many values were handed over (0 without
/// rows or off G1).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_close_metadata_rows(shared: &SharedVm, is_live: &dyn Fn(usize) -> bool) -> usize {
    if !shared.mem.heap.is_g1() {
        return 0;
    }
    let Some(rows) = cratonvm_types::metadata_pin::vm_snapshot(shared.vm_identity) else {
        return 0;
    };
    crate::threading::thread_registry::mark_live_row_values_to_fixpoint(
        &rows,
        is_live,
        &mut |values: &[usize]| g1_remark_mark_kept_closure(shared, values),
    )
}

/// gch h4/g1: `CRATONVM_G1_REMARK_ITL_SWEEP` -- default ON
/// (`runtime_flag_default_on`), `=0` restores the region-granular verdict
/// alone. See [`g1_remark_sweep_inherited_tl_buckets`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_itl_sweep_enabled() -> bool {
    true
}



/// gci i22/g1rk (`gch-h4g1-proposal-g1-remark-sweeps-every-address-keyed-table-mark-exact-20261009`,
/// P1/g1): after a completed remark + cleanup, sweep every address-keyed side
/// table on the completed MARK, not on `addr_keyed::survived_in_place`'s
/// region-granular G1 verdict (`is_addr_in_live_region`, no reclaimed-hole
/// test), under which a dead key in a region the cleanup kept -- any region
/// with one live object -- reads "survived" at every pause until a mixed pause
/// evacuates the region. TLS owner rows, JUL / JULI rows, lock-key slots,
/// socket and zip rows, Throwable back-traces and smuggled `long`s then
/// outlive their key by that long; HotSpot G1 drops the equivalent weak rows
/// at its remark (`WeakProcessor` / `OopStorage` on the completed mark).
///
/// Runs in both G1 remark doors ([`g1_final_remark_cleanup`],
/// [`g1_stw_full_mark_cycle`]) inside the pause, after `cleanup` and before
/// the region-granular `addr_keyed::sweep_address_keyed_tables` (which then
/// finds only the rows this left). Deliberately AFTER `cleanup`, not in the
/// remark callback as the proposal sketched: dropping a row is irreversible
/// (unlike the string pool's judge / settle split), and whether `cleanup`
/// trusted the closure (`last_mark_trusted`) is known only after it; the
/// verdict is `G1Collector::trusted_mark_liveness`, `None` (sweep nothing)
/// for an untrusted, aborted or still-marking cycle or a mark that does not
/// cover the JIT pins. After `cleanup` the verdict is still the mark's: a
/// freed region reads Free (dead, right for a key inside it), and the kept
/// regions' bitmaps and incarnations are untouched; the remark callback has
/// already marked the reference pass's keep closure, so a kept referent, a
/// finalizable about to run and their closures read live.
///
/// JNI weak globals are not swept here (the callback's
/// `clear_weak_global_refs_unmarked` already cleared them on this mark, with
/// the keep list). Returns whether the mark-exact sweep ran.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_sweep_address_keyed_mark_exact(shared: &SharedVm) -> bool {
    if !shared.mem.heap.is_g1() {
        return false;
    }
    let Some(view) = shared.mem.heap.g1_trusted_mark_liveness() else {
        return false;
    };
    // `CRATONVM_DBG_G1_REMARK_ADDR_KEYED=1`: one stderr line per sweep with
    // the number of keys asked about and judged dead (the deciding rows'
    // engagement counter; a table may ask once per row).
    let asked = std::cell::Cell::new(0usize);
    let dead = std::cell::Cell::new(0usize);
    crate::memory::addr_keyed::sweep_address_keyed_tables_with(shared, &|addr| {
        let live = view.is_live(addr);
        asked.set(asked.get() + 1);
        if !live {
            dead.set(dead.get() + 1);        }
        live    });
    if false {
        eprintln!(
            "[g1-addr-keyed] mark-exact asked={} dead={}",
            asked.get(),
            dead.get()
        );
    }
    true
}

/// gci i22/g1rk: the opt-in switch, the no-op arms of the mark-exact sweep,
/// the predicate-driven table sweep on a G1 VM, and both doors' order.
/// Filter: `cargo test -j 5 -p cratonvm-vm --lib gci_i22_g1rk_`.


/// gch h4/g1 (`gch-h3jx-g1-compat-unstarted-thread-inherited-values-never-released-FIXED-20261012`):
/// sweep the pending inherited-`ThreadLocal` buckets of never-started threads
/// with the remark's MARK-EXACT verdict, inside the remark callback.
///
/// Their only other G1 verdict is `addr_keyed::survived_in_place`, which is
/// region-granular on G1 (`is_addr_in_live_region`; no reclaimed-hole test): a
/// dead child `Thread` in a region no pause evacuates keeps reading
/// "survived", and its bucket keeps the values' JNI global roots for as long
/// as the region stays (measured: `live=25` after 483 full cycles, or 5 calls
/// where HotSpot needs 1). `is_live` here is the completed mark's
/// (`G1Collector::is_live_after_mark`: bitmap below TAMS, live above it or
/// outside the heap), so an unmarked child is dead whatever its region, the
/// verdict `jni::clear_weak_global_refs_unmarked` already takes at this point.
///
/// Only after [`g1_remark_mark_kept_closure`] ran: a child reachable only
/// through a policy-kept soft referent or a finalizable must read live, or its
/// bucket would be dropped while the child is resurrected. So the caller asks
/// both switches. Nothing moved, so the map is empty. The values themselves
/// were rooted by this cycle's root scan (as plain pending-table references
/// since `CRATONVM_TL_INHERITED_PENDING_EPHEMERON` went default ON, JNI globals
/// under its `=0`) and die at the next collection that can reach them --
/// unless the cycle's licensed scans pinned them to their child instead
/// (gci i6/g1t's `CRATONVM_G1_ITL_DEFER_AT_MARK`,
/// [`g1_itl_defer_at_mark_enabled`], AND gc wave 9 lane F's
/// `CRATONVM_G1_REMARK_ITL_EPHEMERON`, both default ON; see
/// `native_roots::scan_inherited_tl_pending`): then a live child's values are
/// marked by [`g1_remark_mark_inherited_tl_values`], a dead child's are
/// unmarked here with it, and this sweep is what makes that deferral sound.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_sweep_inherited_tl_buckets(shared: &SharedVm, is_live: &dyn Fn(usize) -> bool) {
    crate::memory::gc::sweep_inherited_thread_local_buckets(
        shared,
        &cratonvm_types::PointerMap::default(),
        is_live,
    );
}

/// gci i6/g1t (`gch-h3jx-g1-compat-unstarted-thread-inherited-values-never-released-FIXED-20261012`,
/// the residual `gcCalls=2` against HotSpot's 1): may a G1 MARK root scan (the
/// initial mark and the remark, the two scans `roots::conditional_loader_metadata`
/// licenses on G1) defer the never-started threads' queued inherited values to
/// their child `Thread` (`metadata_pin`) instead of rooting them?
///
/// Default ON, `CRATONVM_G1_ITL_DEFER_AT_MARK` (`runtime_flag_default_on`, `=0` kills it), and only while
/// the remark's mark-exact bucket sweep is on
/// ([`g1_remark_sweep_inherited_tl_buckets`], both of its switches): a deferred
/// value of a dead child is left unmarked, so cleanup and the following mixed
/// or full-compaction pauses may free it, and the bucket naming it must leave
/// the table in the remark, before cleanup, on the exact verdict. The
/// region-granular `addr_keyed::survived_in_place` alone would keep the bucket
/// of a dead child in a region nothing evacuates, and the next root scan would
/// hand the freed value back as a root. Read by
/// `memory::native_roots::scan_inherited_tl_pending` on a licensed G1 scan
/// inside a marking cycle only; no `static`.
pub(crate) fn g1_itl_defer_at_mark_enabled() -> bool {
    // Default ON since gci orchestrator 2026-10-13 (vi6-g1t S + C + G PASS); `=0` kills it.
    true
        && g1_remark_unload_after_keep_enabled()
        && g1_remark_itl_sweep_enabled()
}


/// gc wave 9 lane F (`gch-h3jx-g1-compat-unstarted-thread-inherited-values-never-released-20261008`,
/// the residual): the ephemeron half of the G1 inherited-`ThreadLocal` pin.
///
/// Under `CRATONVM_G1_REMARK_ITL_EPHEMERON` the cycle's two licensed scans
/// (initial mark, remark) pin each never-started child's queued values to the
/// child (`native_roots::scan_inherited_tl_pending`) instead of rooting them.
/// The concurrent marker follows a pin row when it scans the child, but a row
/// filed by the remark scan for a child that was already marked, or for a child
/// allocated above TAMS (never scanned), is followed by no one. So, inside the
/// remark callback, every value of a child the completed mark finds live
/// (`is_live`, `G1Collector::is_live_after_mark`) that reads unmarked is marked
/// and its closure drained (`resurrect_after_remark`, the marker itself), to a
/// fixed point: a value can reach another queued child. The buckets of the
/// children that stay unmarked are dropped by
/// [`g1_remark_sweep_inherited_tl_buckets`] in the same callback, before
/// `cleanup` frees their values, so no bucket ever names a freed value.
/// `pushed` bounds the loop (an address `resurrect_after_remark` skips, a Free
/// region, is offered once). A no-op off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_mark_inherited_tl_values(shared: &SharedVm, is_live: &dyn Fn(usize) -> bool) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let g1 = &shared.mem.heap;
    // addr-keyed: local to this remark callback (one pause, nothing moves); dropped before cleanup.
    let mut pushed: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    loop {
        let mut values: Vec<usize> = Vec::new();
        cratonvm_native_builtins::phases_early::gc_inherited_tl_pending_values_of_live_children(
            shared.vm_identity,
            is_live,
            &mut values,
        );
        values.retain(|&v| pushed.insert(v));
        if values.is_empty() {
            return;
        }
        g1.g1_resurrect_after_remark(&values);
    }
}


/// gen r5w3/unload7 — what [`remark_reference_rows`] did to the reference
/// processor's rows.
#[derive(Default)]
pub(in crate::runtime::interpreter::gc_and_alloc) struct RemarkReferenceRows {
    /// Everything that must survive this cycle's reclamation (see
    /// [`g1_remark_process_references`]).
    pub(in crate::runtime::interpreter::gc_and_alloc) keep: Vec<usize>,
    /// The soft / weak / phantom rows the processing round retired
    /// (`ReferenceProcessingResult::retired`).
    pub(in crate::runtime::interpreter::gc_and_alloc) retired: Vec<usize>,
    /// ACTIVE rows the dead-`Reference` pre-pass dropped
    /// (`remove_collected_reference_objects`).
    pub(in crate::runtime::interpreter::gc_and_alloc) pruned: usize,
    /// gcj j4/rf: the referents of the soft rows the round kept
    /// (`soft_survivor_referents`, also in `keep`), for the Generational
    /// remark's soft-before-final retraction
    /// ([`gen_remark_retract_softly_reachable_finalizers`]).
    pub(in crate::runtime::interpreter::gc_and_alloc) soft_kept: Vec<usize>,
}

/// gci i11/go (`gci-i11rp-generational-reference-processing-review-findings-FIXED-20261016`
/// R3): one remark round's `-Xlog:gc+ref` census, in the post-GC path's
/// spelling (`process_references_after_gc`): this round's cleared / enqueued
/// counts as discovered and processed per kind (soft, weak, final incl. the
/// resurrected finalizables, phantom incl. cleaners), nothing dropped, the
/// whole round as phase 1. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn remark_ref_census(
    s: &cratonvm_gc::reference::ReferenceProcessingStats,
    total_ns: u64,
) -> crate::runtime::unified_logging::RefProcCensus {
    let w = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let this_round = [
        w(s.soft_refs_cleared),
        w(s.weak_refs_cleared),
        w(s.finalizer_refs_enqueued
            .saturating_add(s.finalizer_refs_resurrected)),
        w(s.phantom_refs_enqueued
            .saturating_add(s.cleaner_refs_processed)),
    ];
    crate::runtime::unified_logging::RefProcCensus {
        discovered: this_round,
        dropped: [0; 4],
        processed: this_round,
        total_ns,
        phase_ns: [total_ns, 0, 0],
        phase1_kind_ns: [0, 0, 0],
    }
}

/// gci i11/go (R3): the `GC(n)` a remark's reference block carries. gci
/// i12/ge (`gci-i11go-g1-concurrent-cycle-xlog-and-jfr-gc-event-review-findings-FIXED-20261017`
/// item 1): on G1 the remark's concurrent cycle's id, as HotSpot files it
/// (the cycle takes its id at its initial mark on every run since then,
/// `gc_events::g1_open_cycle_id`); elsewhere (the Generational remark, whose
/// `-Xlog` cycle line has its own `GC(c<n>)` counter) the VM's collection
/// ordinal as it stands (the id the NEXT collection will take). Read inside
/// the pause and again by [`flush_remark_reference_lines`] after it; only a
/// collection (Generational) or another cycle's initial mark (G1) between the
/// two moves it, and then the block is dropped by the next flush (lost lines,
/// never misfiled ones).
pub(in crate::runtime::interpreter::gc_and_alloc) fn remark_reference_xlog_id(shared: &SharedVm) -> u64 {
    // gci i24/rv (item 16 of `gci-i24rv-...-review-findings-20261015`): a
    // remark with no open cycle (the `System.gc()` stop-the-world cycle) inside
    // a G1 one-report full collection (`CRATONVM_G1_FULL_GC_ONE_REPORT`) files
    // under that report's id, not the next collection's ordinal. `None`, and
    // so the old answer, whenever no such scope is open (the default).
    super::gc_events::g1_open_cycle_id(shared)
        .or_else(|| super::gc_events::g1_full_scope_marking_id(shared))
        .unwrap_or_else(|| {
            shared
                .mem
                .gc_cycle_count
                .load(std::sync::atomic::Ordering::Relaxed)
        })
}

/// gci i11/go (R3): print the reference block a remark deferred
/// ([`remark_reference_rows`]). Call after the remark pause has released (no
/// I/O inside it), on both remark doors (G1 `g1_final_remark_cleanup`, the
/// Generational concurrent remark). A no-op without `-Xlog` or with nothing
/// deferred.
pub(in crate::runtime::interpreter::gc_and_alloc) fn flush_remark_reference_lines(shared: &SharedVm) {
    if crate::runtime::unified_logging::xlog_reference_lines_wanted() {
        crate::runtime::unified_logging::flush_deferred_gc_lines(remark_reference_xlog_id(shared));
    }
}

/// The reference-processing half of [`g1_remark_process_references`]: every
/// step after the class-metadata reconcile, unchanged, plus what the processor
/// retired. Split out (gen r5w3/unload7) so the generational remark can run the
/// reconcile AFTER its keep closure when it unloads classes
/// (`gen_remark_unload_classes`), and read the retired rows without diffing
/// the registry.
///
/// gen r5w5/conc9: `resurrected` — objects `is_marked` calls live only because
/// the remark retained them for `finalize()` (the generational retention
/// closure; G1 passes none). They are noted to the processor before its round
/// (`note_resurrected_finalizables`, the post-collection contract), so soft and
/// weak references to them are processed as for unmarked objects, and the
/// soft / finalizer tracer descends into them. Empty is the pass as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn remark_reference_rows(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    resurrected: &[usize],
) -> RemarkReferenceRows {
    // gcj j1yg-pause round 3: a remark sees every row.
    shared.mem.ref_processor.lock().restore_settled_old_rows();
    // Same subsystem-level exclusion switch as the post-GC path.
    if no_refproc() {
        return RemarkReferenceRows::default();
    }
    let no_moves = cratonvm_types::PointerMap::default();
    // Same pressure input as the post-GC path — see the long comment there for
    // why this is allocatable headroom rather than the former hardcoded `64`,
    // and why the `0` clock argument is correct rather than a second hardcode.
    // gch h2/jr: the same helper as the post-GC path.
    let free_mb = crate::memory::ref_discovery::soft_lru_free_mb(shared);
    // See `process_references_after_gc`: ClassManager must be consulted
    // before taking the lower-ranked reference-processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
    // gc-common w2-f: see `gc_enqueued_sentinel`. The remark moves nothing.
    let enqueued_sentinel = gc_enqueued_sentinel(shared, &no_moves);
    // See `process_references_after_gc`: same L10-before-L7 acquisition, same
    // shape guard, for the same reason — `is_marked` says the ADDRESS survived,
    // not that the object at it is still the Reference the processor recorded.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    let queue_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue");
    //
    // Both guards screen the KIND first, and the `num_fields >= 2` test above
    // them is why: an array MIRRORS ITS LENGTH into `num_slots`, so a
    // `Reference[2]` reports two "fields" and — because a reference array
    // carries its COMPONENT's class id — also answers `is_subclass_of(
    // java/lang/ref/Reference)`. It would pass both tests and reach the
    // positional `get_field(obj, 0)` / `get_field(obj, 1)` reads below, which
    // stride packed 8-byte elements as 16-byte `Value` cells. Same species as
    // `corrupt-value-cell-producer-was-a-string-array-FIXED-20260822`; the heap
    // accessors refuse it now, but a door that can answer "not a Reference"
    // for free should not make the heap say it.
    let is_reference_shaped = |obj: ObjectRef| -> bool {
        if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
            return false;
        }
        match reference_cid {
            Some(cid) => class_manager.is_subclass_of(shared.mem.heap.class_id_of(obj), cid),
            None => true,
        }
    };
    let is_queue_shaped = |obj: ObjectRef| -> bool {
        if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
            return false;
        }
        match queue_cid {
            Some(cid) => class_manager.is_subclass_of(shared.mem.heap.class_id_of(obj), cid),
            None => true,
        }
    };
    let enqueued_sentinel =
        enqueued_sentinel.filter(|&s| queue_cid.is_some() && is_queue_shaped(s));
    let mut ref_proc = shared.mem.ref_processor.lock();
    // gci i31/gq (G-2, default-ON `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES`):
    // the G1 stop-the-world full mark held the finalizables its door's young
    // pause queued out of its roots and marked their closure just before this
    // round (`g1_remark_hold_out_door_queued`); that closure is this round's
    // `resurrected` set, as the Generational remark passes its retention
    // closure. Empty -- `resurrected` as given -- on every other remark.
    let staged = ref_proc.take_remark_staged_resurrected();
    let staged_resurrected: Vec<usize>;
    let resurrected: &[usize] = if staged.is_empty() {
        resurrected
    } else {
        staged_resurrected = resurrected.iter().copied().chain(staged).collect();
        &staged_resurrected
    };
    // gc-common w1-d (2026-09-23): the last-ditch soft-clear rule, applied
    // where on G1 it can actually free something.
    //
    // `last_ditch_reclaim` used to run the G1 full cycle with the LRU policy
    // and only THEN the soft-clear collection — and that collection is a
    // young pause, whose post-GC `is_marked` treats every non-CSet region as
    // live. A condemned soft referent in an Old region therefore "survived",
    // was restored, and the allocation went on to `OutOfMemoryError` with
    // softly-reachable Old data in hand: the `java.lang.ref` guarantee held
    // only for young referents. The remark is the one G1 point that sees Old
    // deaths, and its marker has HIDDEN every soft referent slot, so
    // condemning every active soft row here clears each one nothing else
    // marked; `soft_survivor_referents` below then resurrects none of them.
    // `last_ditch_reclaim` arms the rule around its full cycle; a remark run
    // on any other thread, or on any other path, sees it unarmed.
    let last_ditch = weakref_clear_enabled() && last_ditch_soft_clear_armed();
    if last_ditch {
        let _ = ref_proc.condemn_all_soft_refs();
        // gci i13/oa item 2: the evidence `g1_soft_rung_skippable` reads.
        shared
            .mem
            .alloc_ladder
            .armed_soft_remarks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // gc-common w1-d: retire the soft/weak/phantom rows whose `Reference`
    // OBJECT is dead by the completed bitmap, before the phases — the same
    // pre-pass the post-GC path runs, for the same reason (a dead
    // `SoftReference` must not seed `soft_live`). It matters more here than
    // it used to: those rows were synthetic roots until this round, so a
    // `Reference` could not die at remark; a queue-less one now can, and
    // `cleanup` is about to free its region in place. Left in the registry,
    // the row would name freed memory until the next post-GC prune, and the
    // next young pause's pre-collection pass reads the header at that
    // address. `is_marked` is exact for a `Reference` object: only its slot 0
    // is hidden from the marker, never the object itself.
    // i61 residual: LambdaFormEditor caches nest SoftReferences. Pruning a
    // softly reachable inner Reference on the strong bitmap discarded its
    // retention row, leaving its unmarked LambdaForm graph dangling after G1.
    let soft_trace = |roots: &[usize]| {
        g1_remark_reference_closure(shared, is_marked, &is_reference_shaped, roots)
    };
    let soft_closure = ref_proc.soft_retention_closure(is_marked, &soft_trace, free_mb, 0);
    // gci i15/rf (G-9, opt-in `CRATONVM_G1_REMARK_UNDISCOVERED_REFS_STRONG`):
    // the `Reference` objects only a dead finalizable reaches (unmarked, so
    // the marker never "discovered" them) and their referents. Their rows
    // survive the pre-prune below, the round leaves them alone, and the
    // referents are kept (`resurrect` below): HotSpot's keep-alive phase
    // traces them strongly. Empty off G1, with the switch off, or with no
    // unmarked finalizable.
    //
    // With no `resurrected` set the round's finalizer phase traces from these
    // same roots under this same `is_marked` (`tracer_is_marked` below), so
    // the two share one walk ([`RemarkClosureMemo`]).
    let closure_memo = resurrected.is_empty().then(RemarkClosureMemo::default);
    let (mut undiscovered_refs, mut undiscovered_referents) =
        if shared.mem.heap.is_g1() {
            let fin_roots: Vec<usize> = ref_proc
                .finalizer_referent_addresses()
                .into_iter()
                .filter(|&a| !is_marked(a))
                .collect();
            if resurrected.is_empty() {
                g1_remark_undiscovered_references(
                    shared,
                    is_marked,
                    &is_reference_shaped,
                    &fin_roots,
                    closure_memo.as_ref(),
                )
            } else {
                // gci i31/gq (G-2): the held-out closure is marked already but
                // reachable only for `finalize()`: a `Reference` in it is one
                // HotSpot's keep-alive phase reaches with discovery disabled,
                // exactly as one only an unmarked finalizable reaches.
                g1_remark_undiscovered_references_beside(
                    shared,
                    is_marked,
                    &is_reference_shaped,
                    &fin_roots,
                    resurrected,
                )
            }
        } else {
            (rustc_hash::FxHashSet::default(), Vec::new())
        };
    // gci i27/gr (`gci-i15rf-...-20261014` residual, opt-in
    // `CRATONVM_G1_FULL_GC_CARRIES_UNDISCOVERED_REFS`): the rows the
    // stop-the-world pause of this same full collection left alone as
    // undiscovered (carried with its collection count) stay undiscovered at
    // this remark, and so do the `Reference`s behind their referents. Empty
    // off G1, with the switch off, or when a collection ran since the carry.
    // gci i48/rf48: also read under the opt-in
    // `CRATONVM_G1_FULL_GC_KEEPS_UNDISCOVERED_REFS`: a G1 full-collection
    // door's young pause carries (`g1_carry_door_undiscovered_refs`); every
    // other pause drops the carry.
    if shared.mem.heap.is_g1()
        && (g1_full_gc_carries_undiscovered_refs_enabled()
            || g1_full_gc_keeps_undiscovered_refs_enabled())
    {
        let carried = ref_proc.carried_undiscovered_for(shared.mem.heap.collection_count());
        if !carried.is_empty() {
            let (refs, referents) = g1_remark_carried_undiscovered_references(
                shared,
                is_marked,
                &is_reference_shaped,
                &carried,
            );
            undiscovered_refs.extend(refs);
            for r in referents {
                if !undiscovered_referents.contains(&r) {
                    undiscovered_referents.push(r);
                }
            }
        }
    }
    // gci i48/rf48 (`gci-i15rf-...-20261014` residual, opt-in
    // `CRATONVM_G1_FULL_GC_KEEPS_UNDISCOVERED_REFS`): what this remark leaves
    // undiscovered stays undiscovered through the compaction pause of the
    // same full collection -- staged here, armed by the door
    // (`g1_full_gc_arm_undiscovered`), taken by that pause's pre-collection
    // pass. Every G1 remark restages (an empty set included); only a
    // full-collection door arms it.
    if shared.mem.heap.is_g1() && g1_full_gc_keeps_undiscovered_refs_enabled() {
        let refs: Vec<usize> = undiscovered_refs.iter().copied().collect();
        ref_proc.stage_full_gc_undiscovered(&refs, shared.mem.heap.collection_count());
    }
    let reference_is_live = |addr: usize| {
        is_marked(addr) || soft_closure.contains(&addr) || undiscovered_refs.contains(&addr)
    };
    let pruned = if !no_refproc_prepass() {
        ref_proc.remove_collected_reference_objects(&reference_is_live)    } else {
        0
    };
    // gc-common w3-d (`docs/feature-designs/real-jdk-mode.md` §5): the
    // phases get the TRANSITIVE soft- and finalizer-reachable closures, not
    // the single hop `process_references` falls back to. The marker hid every
    // soft referent slot, so an object reachable only at depth >= 2 through a
    // policy-retained soft referent is unmarked here: without a tracer a weak
    // reference to it was cleared, and a `Cleaner` / `PhantomReference` for it
    // FIRED — a `DirectByteBuffer` held by a soft cache through one holder
    // object had its native memory freed — while `soft_survivor_referents`
    // below then resurrected the whole subtree. The post-collection paths never
    // had the gap: their policy-kept soft slots are not hidden, so the marker
    // traces them.
    //
    // gen r5w5/conc9: the retained finalizables' closure is "marked only for
    // `finalize()`" — noted for the processor's soft and weak phases, and not
    // a stop for the tracer (a policy-kept soft referent reaching into it
    // keeps what lies behind it softly reachable). Empty: nothing changes.
    let resurrected_set: rustc_hash::FxHashSet<usize> = resurrected.iter().copied().collect();
    if !resurrected.is_empty() {
        ref_proc.note_resurrected_finalizables(resurrected);
    }
    let tracer_is_marked =
        |addr: usize| -> bool { is_marked(addr) && !resurrected_set.contains(&addr) };
    let remark_closure = |roots: &[usize]| -> Vec<usize> {
        match &closure_memo {
            Some(memo) => memo
                .walk_of(roots, |roots| {
                    g1_remark_reference_walk(shared, &tracer_is_marked, &is_reference_shaped, roots)
                })
                .closure
                .clone(),
            None => {
                g1_remark_reference_closure(shared, &tracer_is_marked, &is_reference_shaped, roots)
            }
        }
    };
    let remark_closure: &dyn Fn(&[usize]) -> Vec<usize> = &remark_closure;
    // gci i11/go (`gci-i11rp-...-review-findings-20261013` R3): the remark's
    // `-Xlog:gc+ref=debug` / `gc+phases+ref=debug` block, as the post-GC path
    // renders it; printed after the pause by `flush_remark_reference_lines`.
    let xlog_ref_t0 = crate::runtime::unified_logging::xlog_reference_lines_wanted()
        .then(std::time::Instant::now);
    // gci i15/rf (G-9): see `undiscovered_refs` above.
    if !undiscovered_refs.is_empty() {
        let refs: Vec<usize> = undiscovered_refs.iter().copied().collect();
        ref_proc.note_undiscovered_references(&refs);
    }
    // zt t31c (`CRATONVM_DBG_REFPROC_REMARK`, diagnostic, reads only): every
    // active row's inputs to this round, and after it whether the round
    // cleared the row -- the evidence that names a weak reference the remark
    // keeps although nothing strong reaches its referent.
    let row_trace = None::<std::ffi::OsString>
        .is_some()
        .then(|| ref_proc.reference_rows_for_trace());
    let result = ref_proc.process_references_with_finalizer_trace(
        is_marked,
        Some(remark_closure),
        free_mb,
        0,
    );
    if let Some(before) = row_trace {
        // addr-keyed: local to one remark's diagnostic trace (dropped before the pause ends).
        let after: rustc_hash::FxHashMap<usize, bool> = ref_proc
            .reference_rows_for_trace()
            .into_iter()
            .map(|(_, obj, _, cleared, _)| (obj, cleared))
            .collect();
        let class_of = |addr: usize| -> String {
            if addr == 0 {
                return "-".to_string();
            }
            // SAFETY: inside the remark pause, before cleanup: a row's referent
            // is still an object (the marker hid the slot, nothing is freed yet).
            let o = unsafe { ObjectRef::from_raw(addr as *mut u8) };
            class_manager
                .get_class(shared.mem.heap.class_id_of(o))
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "?".to_string())
        };
        for (kind, obj, referent, cleared, enqueued) in before {
            if cleared || enqueued {
                continue;
            }
            eprintln!(
                "[refproc-remark] row {kind:?} ref=0x{obj:x} ref_marked={} referent=0x{referent:x} \
                 class={} referent_marked={} resurrected={} soft_closure={} undiscovered_ref={} \
                 undiscovered_referent={} cleared_after={}",
                is_marked(obj),
                class_of(referent),
                referent != 0 && is_marked(referent),
                resurrected_set.contains(&referent),
                soft_closure.contains(&referent),
                undiscovered_refs.contains(&obj),
                undiscovered_referents.contains(&referent),
                after.get(&obj).copied().unwrap_or(false),
            );
        }
    }
    if let Some(t0) = xlog_ref_t0 {
        let total_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let census = remark_ref_census(&result.stats, total_ns);
        // gci i25/sg: the remark of a G1 one-report full collection's own
        // cycle hands its census to the report (one block, after its
        // `gc,start`); every other remark defers its block as before.
        if !super::gc_events::g1_full_scope_absorb_ref_census(shared, true, &census) {
            crate::runtime::unified_logging::defer_xlog_reference_processing(
                remark_reference_xlog_id(shared),
                &census,
            );
        }
    }
    // The remark has no pre-collection pass of its own, so nothing it did
    // must survive into the next collection's bookkeeping — least of all a
    // last-ditch condemnation. (Also retires a stale set left by a pass whose
    // post-GC half never ran.) See `ReferenceProcessor::finish_pre_gc_cycle`.
    ref_proc.finish_pre_gc_cycle();

    // Null the referent slot of newly-cleared references (once-only per
    // entry, same contract as the post-GC path).
    let cleared = ref_proc.take_newly_cleared();
    for ref_addr in cleared {
        if !reference_is_live(ref_addr) {
            // The Reference itself is dead this cycle — no mutator can ever
            // observe its slot again and cleanup may free it momentarily.
            continue;
        }
        // SAFETY: `ref_addr` is a registry address kept current by the
        // per-pause `update_after_gc`; nothing has been freed since.
        let obj_ref = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if shared.mem.heap.num_fields(obj_ref) < 2 || !is_reference_shaped(obj_ref) {
            continue; // belt-and-suspenders, mirrors the post-GC path
        }
        // SATB-suppressed: the clear is a decided verdict, not a semantic
        // overwrite — logging the old referent would resurrect it in the
        // re-drain below and retain the memory a full extra cycle.
        shared
            .mem
            .heap
            .set_field_suppress_satb(obj_ref, 0, Value::Object(None));
    }
    // ... and of every row this round RETIRED, which adds the ENQUEUED
    // phantoms `take_newly_cleared` (soft and weak only) does not return.
    // JDK 9+ clears a phantom as it is enqueued, and the post-collection paths
    // get that from their pre-null pass, which never restores an enqueued
    // phantom. Here nothing else would: `cleanup` then frees the referent's
    // region in place while the live `PhantomReference` (held by its queue, a
    // `Cleaner`'s list) keeps naming it, and a later full trace, `refersTo`
    // or card scan of the `Reference` reads freed memory. A soft / weak row
    // was nulled just above (the `Some` test skips it). Both remarks, G1's and
    // the generational one, run this.
    // `docs/feature-designs/concurrent-gc-maturation.md`.
    for &ref_addr in &result.retired {
        if !reference_is_live(ref_addr) {
            continue; // dead by mark: cleanup frees the `Reference` itself
        }
        // SAFETY: as above -- a registry address, marked live, nothing freed.
        let obj_ref = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if shared.mem.heap.num_fields(obj_ref) < 2 || !is_reference_shaped(obj_ref) {
            continue;
        }
        if let Value::Object(Some(_)) = shared.mem.heap.get_field(obj_ref, 0) {
            // SATB-suppressed, as above.
            shared
                .mem
                .heap
                .set_field_suppress_satb(obj_ref, 0, Value::Object(None));
        }
    }

    // gc-common w3-d: queues this remark links into, for the delivery thread's
    // wake-up — see the same list in `process_references_after_gc`. The remark
    // moves nothing, so the addresses are final. gc-common w5-d: also under
    // `--real-jdk` without the flag (`queue_wake_ups_recorded_for`).
    let record_enqueued_queues = queue_wake_ups_recorded(shared);
    let mut gc_enqueued_queues: Vec<usize> = Vec::new();
    let java_queue_lock_slot = class_manager
        .find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue")
        .and_then(|cid| crate::vm::vm_exec::resolve_field_index_in_hierarchy(
            cid, "lock", class_manager.class_store(),
        ));
    let mut deferred_reference_enqueues = Vec::new();

    // Queue links for cleared/phantom references. Skip the whole enqueue
    // when the Reference or its queue is dead-by-mark: linking a dead
    // Reference into a live queue would resurrect it into a region cleanup
    // is about to free (dangling queue head), and a dead queue has no
    // consumer to poll it.
    for (ref_addr, queue_addr) in &result.to_enqueue {
        if !reference_is_live(*ref_addr) || !reference_is_live(*queue_addr) {
            continue;
        }
        // SAFETY: registry addresses, current as above; both marked live.
        let ref_obj = unsafe { ObjectRef::from_raw(*ref_addr as *mut u8) };
        let q_obj = unsafe { ObjectRef::from_raw(*queue_addr as *mut u8) };
        if shared.mem.heap.num_fields(q_obj) < 2
            || shared.mem.heap.num_fields(ref_obj) < 2
            || !is_reference_shaped(ref_obj)
            || !is_queue_shaped(q_obj)
        {
            continue;
        }
        if java_queue_lock_slot.is_some_and(|slot| slot < shared.mem.heap.num_fields(q_obj)) {
            deferred_reference_enqueues.push((*ref_addr, *queue_addr));
            continue;
        }
        // Same linked-list protocol as the post-GC path: head/size on the
        // queue, linkage through the Reference's `next` slot (slot 2 on the
        // real-JDK layout; legacy 2-field shape falls back to slot 0).
        let old_head = shared.mem.heap.get_field(q_obj, 0);
        shared
            .mem
            .heap
            .set_field(q_obj, 0, Value::Object(Some(ref_obj)));
        let next_slot = if shared.mem.heap.num_fields(ref_obj) <= 2 {
            0 // legacy synthetic 2-field shape: referent, queue only
        } else {
            reference_next_slot
        };
        shared.mem.heap.set_field(ref_obj, next_slot, old_head);
        // G56-1 NOMINATION 7: this path used to read only `Int` (discarding a
        // `Long` and defaulting to 0) and always write back an `Int`, where
        // the sibling post-GC path above preserves the stored width because
        // `queueLength` is `long` on a real JDK `ReferenceQueue` whose own
        // `enqueue0`/`poll0` bytecode reads it back. Copy that arm so both
        // enqueue paths agree.
        let new_size = match shared.mem.heap.get_field(q_obj, 1) {
            Value::Long(v) => Value::Long(v + 1),
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::Int(1),
        };
        shared.mem.heap.set_field(q_obj, 1, new_size);
        // Enqueued mark — see `gc_enqueued_sentinel`.
        shared
            .mem
            .heap
            .set_field(ref_obj, 1, gc_enqueued_mark(enqueued_sentinel, next_slot));
        if record_enqueued_queues {
            gc_enqueued_queues.push(*queue_addr);
        }
    }
    // Shape guards done; see the post-GC path.
    drop(class_manager);

    // Everything handed out below must survive this cycle's cleanup — the
    // caller marks these and re-drains the closure before any region is
    // freed.
    let mut resurrect: Vec<usize> = Vec::new();

    // Dead finalizables: submit for finalize() AND resurrect. This is the
    // half of INT-8 that closes the finalize-never-runs gap: previously a
    // finalizable object in a wholly-dead Old region was freed in place by
    // cleanup and the post-GC staleness guard then (correctly) dropped its
    // stale submission — finalize() silently never ran.
    // `enqueue_unfinalized`: reported from never-enqueued rows — see
    // `process_references_after_gc`'s finalize loop.
    for obj_addr in &result.to_finalize {
        shared.mem.finalizer_thread.enqueue_unfinalized(*obj_addr);
        resurrect.push(*obj_addr);
    }
    while let Some(obj_addr) = ref_proc.dequeue_for_finalization() {
        shared.mem.finalizer_thread.enqueue_unfinalized(obj_addr);
        resurrect.push(obj_addr);
    }

    // Cleaner actions fired by this round: submit + resurrect (the action
    // object is dereferenced later by run_cleaner_actions).
    for action_addr in &result.cleaner_actions {
        shared.mem.cleaner_thread.submit_action(*action_addr);
        resurrect.push(*action_addr);
    }

    // Pending (not-yet-fired) cleaner chains: the registry will hand these
    // out on a later cycle, so cleanup must not free them — the HotSpot
    // equivalent is the Cleaner's internal strong list. Self-referent
    // (finalizer-style) registrations are excluded inside the accessor.
    resurrect.extend(ref_proc.cleaner_pending_object_addresses());

    // Policy-retained soft referents: the marker never traced them
    // (referent-slot hiding), so a softly-only-reachable referent is
    // unmarked even though the LRU policy kept it — exactly like HotSpot,
    // reference processing itself keeps them alive.
    let soft_kept = ref_proc.soft_survivor_referents();
    resurrect.extend(soft_kept.iter().copied());
    resurrect.extend(soft_closure);
    // gci i15/rf (G-9): the referents of the `Reference` objects the round
    // left undiscovered. The marker hid their slot 0, so resurrecting the
    // finalizable that holds them would NOT reach these: kept here, as
    // HotSpot's keep-alive phase traces them (discovery disabled).
    resurrect.extend(undiscovered_referents);

    // DBG (CRATONVM_DBG_REFPROC_REMARK): per-remark mechanism evidence —
    // distinguishes clears that happened HERE (against the mark bitmap)
    // from clears the evacuation-pause path produced, which black-box
    // probes cannot tell apart.
    if None::<std::ffi::OsString>.is_some() {
        eprintln!(
            "[refproc-remark] soft_cleared={} weak_cleared={} enqueued={} finalize={} \
             cleaner_actions={} resurrect={}",
            result.stats.soft_refs_cleared,
            result.stats.weak_refs_cleared,
            result.to_enqueue.len(),
            result.to_finalize.len(),
            result.cleaner_actions.len(),
            resurrect.len(),
        );
    }

    // gc-common w3-d: queue wake-ups, and the finalizers / cleaner actions this
    // remark queued, go to the delivery thread (flag-gated). It cannot run
    // before this remark pause completes, by which time the caller has marked
    // everything in `resurrect`. A remark queue is marked, so cleanup does not
    // free it; the next pause roots it through `CleanerThread::pending_addresses`.
    drop(ref_proc);
    gc_enqueued_queues_hand_off(shared, &gc_enqueued_queues);
    gc_reference_enqueues_hand_off(shared, &deferred_reference_enqueues);
    // gci i42/rp: and tell the doors that can run Java that this remark queued
    // finalizers / cleaner actions, as every collection pause does
    // (`remark_note_ref_work_queued`).
    let _ = remark_note_ref_work_queued(shared);

    RemarkReferenceRows {
        keep: resurrect,
        retired: result.retired,
        pruned,
        soft_kept,
    }
}



/// gci i42/rp (`docs/feature-designs/real-jdk-mode.md`):
/// raise the "reference work queued elsewhere" hint
/// (`GcBarrier::note_ref_work_queued`) when the finalizer or cleaner queue is
/// non-empty after a remark's reference round, exactly as
/// [`run_collection_pause`] does after every collection. `true` when raised.
///
/// The defect: the remark rounds (G1's `g1_final_remark_cleanup`, the
/// Generational `gen_concurrent_remark_pause`, both through
/// [`remark_reference_rows`]) QUEUE `finalize()` for the dead finalizables and
/// cleaner actions for the dead `jdk.internal.ref.Cleaner`s only they can see
/// (an Old object no young pause judges), but raised no hint. Under the
/// default delivery policy those queues are drained by a door that runs Java
/// after its own pause or by `maybe_gc`'s no-collection path, which acts only
/// on the hint, and the remark doors that are not followed by a drain (the
/// allocation-failure door, the JIT driver, the Generational concurrent
/// service thread) left the work waiting for the NEXT collection's pause to
/// notice the queue. HotSpot hands a remark's discovered references to the
/// Reference Handler at once. The full hand-off is unaffected (the remark's
/// `gc_enqueued_queues_hand_off` already wakes the delivery thread). Inside
/// the remark pause: two queue-lock reads and at most one atomic swap.
pub(in crate::runtime::interpreter::gc_and_alloc) fn remark_note_ref_work_queued(shared: &SharedVm) -> bool {
    if shared.mem.finalizer_thread.pending_count() == 0
        && shared.mem.cleaner_thread.pending_count() == 0
    {
        return false;
    }
    shared.mem.gc_barrier.note_ref_work_queued();
    true
}


/// gci i46/ts: the missing tests of
/// `docs/feature-designs/real-jdk-mode.md`
/// -- item 1 (the delivery thread's registry unwind guard, applied in
/// `069555c4b7`), M2 (a remark round that queues a finalizer raises the
/// ref-work hint) and M4 (the i41/rs `=0` arm turns the in-pause Survivor
/// stamp off, driven through the real pauses).
/// `cargo test -j 5 -p cratonvm-vm --lib ts46_`.


/// The `trace_from` primitive `process_references_with_finalizer_trace` asks
/// for, built for G1's final remark from the heap's accessors: every UNMARKED
/// object transitively reachable from `roots` (soft survivors' referents, or
/// the referents of about-to-be-finalized objects).
///
/// * A MARKED object ends the walk: everything it reaches through a strong edge
///   is marked too, and the phases already treat `is_marked` as live.
/// * A `java.lang.ref.Reference`'s slot 0 (`referent`) is not an edge — the
///   object behind it is only as reachable as that reference's own strength,
///   exactly as the marker (which hides that slot) and HotSpot treat it.
/// * Primitive arrays have no edges; reference arrays are walked element-wise.
///
/// This must be a complete closure: its Reference owners are used by the
/// mark-exact pre-prune. Truncating it can retire a live nested soft cache.
/// Runs inside the remark STW, before cleanup frees anything, so every object
/// reachable from a live root is intact. gc-common w3-d.
///
/// The walk is [`g1_remark_reference_walk`]; this is its closure alone.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_reference_closure(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    is_reference_shaped: &dyn Fn(ObjectRef) -> bool,
    roots: &[usize],
) -> Vec<usize> {
    g1_remark_reference_walk(shared, is_marked, is_reference_shaped, roots).closure
}

/// What one [`g1_remark_reference_walk`] found.
pub(in crate::runtime::interpreter::gc_and_alloc) struct RemarkWalk {
    /// Every object the walk reached, in visiting order: the closure
    /// [`g1_remark_reference_closure`] answers.
    pub(in crate::runtime::interpreter::gc_and_alloc) closure: Vec<usize>,
    /// The members of `closure` that are unmarked, `Reference`-shaped
    /// instances of at least two fields, in the same order -- the candidates
    /// [`g1_remark_undiscovered_references`] screens the closure for. The walk
    /// already asks each unmarked instance whether it is `Reference`-shaped
    /// (to leave its referent slot out), so the screen costs nothing here and
    /// a second pass over millions of entries nothing at all.
    pub(in crate::runtime::interpreter::gc_and_alloc) references: Vec<usize>,
}

/// The walk behind [`g1_remark_reference_closure`].
///
/// The visited set is a [`RemarkVisited`]: a hash set while the walk is small,
/// a heap bitmap once it is not. An instance's edges come from its class's
/// oop-map (`VmHeap::push_compact_reference_words`), not from a `get_field` of
/// every field; a legacy body is still read field by field. A dead
/// `jdk.nio.zipfs.ZipFileSystem` (it has a `finalize()`) keeps its whole entry
/// index reachable, and Spring's in-process `javac` tests leave thousands of
/// them per cycle: one remark's finalizer closure was 9.5 million objects, and
/// the walk took ~1.8 us per object
/// (`docs/feature-designs/jit-ir-optimizations.md`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_reference_walk(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    is_reference_shaped: &dyn Fn(ObjectRef) -> bool,
    roots: &[usize],
) -> RemarkWalk {
    let heap = &shared.mem.heap;
    let mut seen = RemarkVisited::new(heap.reserved_address_envelope());
    let mut closure: Vec<usize> = Vec::new();
    let mut references: Vec<usize> = Vec::new();
    let mut work: Vec<usize> = roots.to_vec();
    while let Some(addr) = work.pop() {
        if addr == 0 || !seen.insert(addr) {
            continue;
        }
        // The root itself is always reported (the tracer-less fallback reports
        // it too), marked or not.
        closure.push(addr);
        if is_marked(addr) {
            continue;
        }
        let Some(obj) = heap.is_heap_addr(addr) else {
            continue;
        };
        match heap.kind_of(obj) {
            crate::memory::heap::ObjectKind::Array => {
                if heap.array_element_type(obj) != Some(ArrayElementType::Reference) {
                    continue;
                }
                for i in 0..heap.array_length(obj) {
                    if let Ok(Value::Object(Some(child))) = heap.get_array_element(obj, i) {
                        work.push(child.as_ptr() as usize);
                    }
                }
            }
            crate::memory::heap::ObjectKind::Object => {
                let shaped = is_reference_shaped(obj);
                let fields = heap.num_fields(obj);
                if shaped && fields >= 2 {
                    references.push(addr);
                }
                let first = usize::from(shaped);
                // SAFETY: `obj` was validated just above, and the remark is a
                // pause: nothing moves it before its words are pushed.
                if !unsafe { heap.push_compact_reference_words(obj, first, &mut work) } {
                    for i in first..fields {
                        if let Value::Object(Some(child)) = heap.get_field(obj, i) {
                            work.push(child.as_ptr() as usize);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    RemarkWalk {
        closure,
        references,
    }
}

/// The visited set of one [`g1_remark_reference_closure`] walk.
///
/// Most walks are a soft cache's few thousand objects, where a hash set is
/// cheapest: no mapping, nothing to fault in. Past [`Self::PROMOTE_AT`]
/// entries the set moves into a [`cratonvm_gc::heap_bitmap::HeapBitmap`] over
/// the heap's reserved span -- one bit per 8 bytes on a fresh zero-page
/// mapping, so only the words the walk touches are ever faulted in -- and an
/// insert becomes a shift and a test-and-set instead of a hash, a probe and,
/// every doubling, a rehash of millions of entries. Without a span (a heap
/// that publishes none) the hash set serves the whole walk.
pub(in crate::runtime::interpreter::gc_and_alloc) enum RemarkVisited {
    Small {
        set: rustc_hash::FxHashSet<usize>,
        span: Option<(usize, usize)>,
    },
    Big(cratonvm_gc::heap_bitmap::HeapBitmap),
}

impl RemarkVisited {
    /// Entries at which a walk leaves the hash set (about 1 MiB of it).
    pub(in crate::runtime::interpreter::gc_and_alloc) const PROMOTE_AT: usize = 1 << 16;

    pub(in crate::runtime::interpreter::gc_and_alloc) fn new(span: Option<(usize, usize)>) -> Self {
        RemarkVisited::Small {
            set: rustc_hash::FxHashSet::default(),
            span,
        }
    }

    /// Record `addr`; `true` when it was not already recorded.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn insert(&mut self, addr: usize) -> bool {
        match self {
            RemarkVisited::Big(bits) => bits.claim(addr),
            RemarkVisited::Small { set, span } => {
                if !set.insert(addr) {
                    return false;
                }
                if set.len() >= Self::PROMOTE_AT {
                    if let Some((lo, hi)) = span.filter(|(lo, hi)| hi > lo) {
                        let bits = cratonvm_gc::heap_bitmap::HeapBitmap::labelled_side_table(
                            lo,
                            hi - lo,
                            "g1-remark-trace",
                            true,
                        );
                        for &a in set.iter() {
                            bits.claim(a);
                        }
                        *self = RemarkVisited::Big(bits);
                    }
                }
                true
            }
        }
    }
}

/// The closures one G1 remark has traced, by their exact root list.
///
/// Two passes of the same remark ask for the closure of the same roots under
/// the same predicate: [`g1_remark_undiscovered_references`] walks from the
/// unmarked finalizable objects, and the finalizer phase of
/// `process_references_with_finalizer_trace` walks from the same list. Nothing
/// between them writes the heap or the mark bitmap, so the second answer is
/// the first. Only a caller whose two predicates are the same function may
/// share one memo (`remark_reference_rows` builds it only with no
/// `resurrected` set, which is the only input that makes them differ); a root
/// list that differs by one address, or by its order, is a miss.
#[derive(Default)]
pub(in crate::runtime::interpreter::gc_and_alloc) struct RemarkClosureMemo {
    pub(in crate::runtime::interpreter::gc_and_alloc) traced: std::cell::RefCell<Vec<(Vec<usize>, std::rc::Rc<RemarkWalk>)>>,
}

impl RemarkClosureMemo {
    /// The walk from `roots`: remembered, or walked by `walk` and remembered.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn walk_of(
        &self,
        roots: &[usize],
        walk: impl FnOnce(&[usize]) -> RemarkWalk,
    ) -> std::rc::Rc<RemarkWalk> {
        if let Some((_, found)) = self.traced.borrow().iter().find(|(r, _)| r[..] == roots[..]) {
            return std::rc::Rc::clone(found);
        }
        let found = std::rc::Rc::new(walk(roots));
        self.traced
            .borrow_mut()
            .push((roots.to_vec(), std::rc::Rc::clone(&found)));
        found
    }
}




/// gci i48/rf48 (`gci-i15rf-...-20261014` residual):
/// `CRATONVM_G1_FULL_GC_KEEPS_UNDISCOVERED_REFS` (default ON, `=0` kills) -- a G1 full
/// collection's young and compaction pauses keep a `Reference` only a dead
/// finalizable reaches undiscovered (probe, carry into the remark, staged
/// set held through the compaction pause's pre-pass). The sets are consumed
/// and forgotten at the door's end; the next collection discovers the rows.
/// The pre-GC retirer must leave held rows active (gcj j4/rf), so their
/// referents can be cleared and enqueued by that next collection.
/// Keeping this off loses live referents before the finalizer can read them.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_gc_keeps_undiscovered_refs_enabled() -> bool {
    true
}

/// gci i15/rf (G-9): the `Reference` objects HotSpot would never have
/// DISCOVERED at this remark -- unmarked, yet reachable from the unmarked
/// finalizable objects `finalizable_roots` the remark is about to keep alive
/// -- and their referents, transitively (a referent can hold another such
/// `Reference`). Returns `(reference objects, referents)`; the referents are
/// the unmarked, non-null slot-0 values, each once.
///
/// HotSpot's `process_discovered_references` disables discovery before its
/// keep-alive phase marks the finalizable objects, so a `Reference` reached
/// only then is an ordinary object: its referent is traced strongly and it is
/// processed by a LATER cycle. Here the marker hid that `Reference`'s slot 0
/// for the whole cycle (INT-8), so the remark's resurrection of the
/// finalizable marks the `Reference` but not its referent, and the pre-prune
/// retired its row as dead-by-mark: the referent was left unmarked behind a
/// live `Reference` (freed in place by cleanup when its region held nothing
/// else) and the row never came back. The caller keeps the rows, tells the
/// round to leave them alone and resurrects the referents.
///
/// Runs inside the remark STW, before cleanup frees anything, with the
/// reference processor and the class manager held by the caller (the walk
/// only reads the heap, as [`g1_remark_reference_closure`] does).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_undiscovered_references(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    is_reference_shaped: &dyn Fn(ObjectRef) -> bool,
    finalizable_roots: &[usize],
    // `Some` only when the caller's later finalizer phase traces the same
    // roots under this same `is_marked` ([`RemarkClosureMemo`]).
    memo: Option<&RemarkClosureMemo>,
    // addr-keyed: local to this remark (one pause; the caller consumes it before cleanup).
) -> (rustc_hash::FxHashSet<usize>, Vec<usize>) {
    let heap = &shared.mem.heap;
    // addr-keyed: local to this remark (one pause).
    let mut refs: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut referents: Vec<usize> = Vec::new();
    // addr-keyed: local to this remark (one pause).
    let mut seen_referents: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut frontier: Vec<usize> = finalizable_roots.to_vec();
    let walk =
        |roots: &[usize]| g1_remark_reference_walk(shared, is_marked, is_reference_shaped, roots);
    while !frontier.is_empty() {
        let found = match memo {
            Some(memo) => memo.walk_of(&frontier, &walk),
            None => std::rc::Rc::new(walk(&frontier)),
        };
        frontier.clear();
        // The closure's unmarked, `Reference`-shaped instances of at least two
        // fields, in closure order (`RemarkWalk::references`).
        for &addr in &found.references {
            if refs.contains(&addr) {
                continue;
            }
            let Some(obj) = heap.is_heap_addr(addr) else {
                continue;
            };
            refs.insert(addr);
            if let Value::Object(Some(referent)) = heap.get_field(obj, 0) {
                let r = referent.as_ptr() as usize;
                if !is_marked(r) && seen_referents.insert(r) {
                    referents.push(r);
                    frontier.push(r);
                }
            }
        }
    }
    (refs, referents)
}

/// gci i31/gq (G-2, default-ON `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES`):
/// [`g1_remark_undiscovered_references`] for a round whose `resurrected` set
/// (the stop-the-world full mark's held-out closure, marked just before the
/// round) is finalizer-reachable only: those objects read unmarked to the walk
/// and are roots of it beside the unmarked registered finalizables, so a
/// `Reference` only they reach is left undiscovered and its referent kept, as
/// HotSpot's keep-alive phase traces it with discovery disabled.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_undiscovered_references_beside(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    is_reference_shaped: &dyn Fn(ObjectRef) -> bool,
    finalizable_roots: &[usize],
    resurrected: &[usize],
    // addr-keyed: local to this remark (one pause; the caller consumes it before cleanup).
) -> (rustc_hash::FxHashSet<usize>, Vec<usize>) {
    let mut held_out: Vec<usize> = resurrected.to_vec();
    held_out.sort_unstable();
    held_out.dedup();
    let is_marked_beside =
        |addr: usize| -> bool { is_marked(addr) && held_out.binary_search(&addr).is_err() };
    let mut roots: Vec<usize> = finalizable_roots.to_vec();
    roots.extend(held_out.iter().copied());
    g1_remark_undiscovered_references(shared, &is_marked_beside, is_reference_shaped, &roots, None)
}

/// gci i27/gr (`gci-i15rf-g1-remark-loses-a-reference-only-a-dead-finalizable-reaches-RETIRED-UNFIXED-20261018`
/// residual): `CRATONVM_G1_FULL_GC_CARRIES_UNDISCOVERED_REFS` -- default OFF
/// (`runtime_flag_on`): it changes what a G1 remark keeps. Read once per G1
/// stop-the-world round and once per G1 remark; no `static`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_gc_carries_undiscovered_refs_enabled() -> bool {
    false
}

/// gcj j4/rf (`gcj-j1rf-reference-processor-review-residuals` item 2):
/// `CRATONVM_REF_CENSUS_COUNTS_LATE_CLEARS` -- default ON, `=0` kill. The
/// `-Xlog:gc+ref` census of a collection round counts the weak / soft rows
/// its restore pass cleared after the round
/// (`ReferenceProcessor::take_late_cleared`): HotSpot's census counts every
/// reference its collection cleared. Read only when the census is logged.
pub(in crate::runtime::interpreter::gc_and_alloc) fn census_counts_late_clears_enabled() -> bool {
    true
}

/// gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals` item 3):
/// `CRATONVM_GEN_SKIP_SHAPE_ROW_PRUNE` -- default ON, `=0` kill. Memoised (read
/// once per pause otherwise).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_skip_shape_row_prune_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// gci i48/rf48 (`gci-i15rf-...-20261014` residual, default path): the carry
/// of `ReferenceProcessor::carry_probe_undiscovered` from a G1
/// full-collection door's pause ([`g1_full_gc_keeps_undiscovered`]), after its
/// round; the opt-in `CRATONVM_G1_FULL_GC_CARRIES_UNDISCOVERED_REFS` carries
/// from every G1 round inside `process_references_after_gc` already (no-op
/// then). `process_references_after_gc` dropped the carry on this pause, so
/// a pause on any other door leaves none. Needed when the finalizer thread
/// ran `finalize()` between the young pause and the mark: the object is then
/// strongly reachable, the remark marks its `Reference` and only the carry
/// says it was never discovered. This also applies to a user-requested
/// concurrent cycle: its remark must not rediscover the same references.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_carry_door_undiscovered_refs(
    shared: &SharedVm,
    door: GcDoor,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if no_refproc()
        || g1_full_gc_carries_undiscovered_refs_enabled()
        || !g1_full_gc_keeps_undiscovered(shared, door)
    {
        return;
    }
    let stamp = shared.mem.heap.collection_count();
    shared
        .mem
        .ref_processor
        .lock()
        .carry_probe_undiscovered(pointer_map, stamp);
}

/// gci i48/rf48: arm the set the full collection's remark staged
/// (`ReferenceProcessor::stage_full_gc_undiscovered`) for the door's
/// compaction pause, if no collection ran since. No-op off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_gc_arm_undiscovered(shared: &SharedVm) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let stamp = shared.mem.heap.collection_count();
    let _ = shared
        .mem
        .ref_processor
        .lock()
        .arm_full_gc_undiscovered(stamp);
}

/// gci i48/rf48: the door is over -- drop the armed set if no pause took it
/// (an untrusted mark ran no compaction pause) and the young pause's carry,
/// so neither reaches a later, separate collection. No-op off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_gc_forget_undiscovered(shared: &SharedVm) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let mut rp = shared.mem.ref_processor.lock();
    rp.drop_full_gc_undiscovered();
    rp.drop_carried_undiscovered();
}


/// gci i27/gr: the remark half of the carry
/// (`ReferenceProcessor::carry_probe_undiscovered`). G1's `System.gc()` is a
/// young pause and then a marking cycle; HotSpot runs one full collection.
/// The young pause's stop-the-world probe (`CRATONVM_STW_UNDISCOVERED_REFS_STRONG`)
/// left alone the `Reference`s only a dead finalizable reached, as HotSpot
/// does; the cycle's remark then marks them through the now-queued
/// finalizable (a mark root since gci i19/fz) and, their slot 0 hidden,
/// cleared them -- a second discovery inside what HotSpot treats as one
/// collection (`fz19_f_on_*` / `both_*`: `outer-kept` / `inner-kept` /
/// `undiscovered-kept: false`).
///
/// Returns `(Reference objects, referents)`: every carried `Reference` the
/// remark MARKED (an unmarked one is not kept by anything this remark keeps,
/// so cleanup may free it: it is left to the round as before) with its
/// unmarked slot-0 referent, plus what
/// [`g1_remark_undiscovered_references`] finds behind those referents (a
/// referent holding another such `Reference`, the probe's nested shape). The
/// caller keeps the rows through the pre-prune, notes them undiscovered and
/// resurrects the referents, exactly as for the finalizer-reached rows.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_carried_undiscovered_references(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    is_reference_shaped: &dyn Fn(ObjectRef) -> bool,
    carried: &[usize],
    // addr-keyed: local to this remark (one pause; the caller consumes it before cleanup).
) -> (rustc_hash::FxHashSet<usize>, Vec<usize>) {
    let heap = &shared.mem.heap;
    // addr-keyed: local to this remark (one pause).
    let mut refs: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut referents: Vec<usize> = Vec::new();
    for &addr in carried {
        if !is_marked(addr) || refs.contains(&addr) {
            continue;
        }
        let Some(obj) = heap.is_heap_addr(addr) else {
            continue;
        };
        if heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array
            || heap.num_fields(obj) < 2
            || !is_reference_shaped(obj)
        {
            continue;
        }
        refs.insert(addr);
        if let Value::Object(Some(referent)) = heap.get_field(obj, 0) {
            let r = referent.as_ptr() as usize;
            if !is_marked(r) && !referents.contains(&r) {
                referents.push(r);
            }
        }
    }
    if !referents.is_empty() {
        let (behind_refs, behind_referents) =
            g1_remark_undiscovered_references(shared, is_marked, is_reference_shaped, &referents, None);
        refs.extend(behind_refs);
        for r in behind_referents {
            if !referents.contains(&r) {
                referents.push(r);
            }
        }
    }
    (refs, referents)
}


/// The whole final rung of the allocation-failure ladder: everything the VM
/// can still do to satisfy an allocation that has already failed once, before
/// the caller is entitled to throw `OutOfMemoryError`.
///
/// Three steps, in order, because they reclaim disjoint things:
///
/// 1. [`g1_force_full_cycle`] — dead Old and humongous regions, which only a
///    completed mark cycle's cleanup reclaims. G1 only.
/// 2. G1 mixed evacuation after that mark — compacts partially-live Old
///    regions, which is the only way to coalesce a free run for a humongous
///    retry. G1 only.
/// 3. [`last_ditch_clear_soft_refs`] — every softly-reachable object, on every
///    collector. `java.lang.ref`'s guarantee is unconditional: all soft
///    references to softly-reachable objects are cleared before the VM throws
///    `OutOfMemoryError`. CratonVM honoured only the LRU half of the soft-ref
///    policy, which by construction never fires for the reference the failing
///    program is itself reading in a loop.
///
/// gc-common w1-d (2026-09-23): under G1 the full cycle in step 1 now runs
/// with the last-ditch soft rule ARMED, so its final remark condemns every
/// soft reference (`g1_remark_process_references`). Step 2 alone cannot clear
/// a soft referent in an Old region on G1: it is a young pause, and a young
/// pause treats every region outside its collection set as live.
///
/// **Not for `System.gc()`.** `force_gc_from_native` used to call this for G1
/// to finish the concurrent cycle synchronously, which cleared EVERY soft
/// reference on every `System.gc()` — HotSpot's explicit full GC applies the
/// LRU policy and clears none it would keep. That caller now runs
/// [`g1_force_full_cycle`] (`force_full_collection`); see
/// `docs/feature-designs/real-jdk-mode.md`.
pub(crate) fn last_ditch_reclaim(shared: &SharedVm, thread: &mut JvmThread) {
    // gci i14/ox: steps 1 + 2 and step 3 (+ 4) are HotSpot G1's two `Pause
    // Full (G1 Compaction Pause)` collections; each half reports as one under
    // the default-ON `CRATONVM_G1_OOME_PAUSE_FULL_REPORT` (no-op otherwise).
    // The G1 near-full proxy-define page (`CRATONVM_G1_LAST_DITCH_TENURES_ALL`,
    // default ON, `=0` kills): the two full collections tenure every live
    // young object, as `System.gc()`'s do (`request_system_gc_tenure`) and as
    // HotSpot's Full GC compacts young into old regions. On a heap whose live
    // set leaves 2 Free regions, a Survivor region holding 28 KB of in-flight
    // survivors was the region the retry needed
    // (`Zs3Nd3UpcallMints proxy 4096 -Xmx64m`).
    with_last_ditch_tenure(shared, thread, |thread| {
        let (armed_before, _) = last_ditch_first_full(shared, thread, None);
        last_ditch_second_full(shared, thread, armed_before);
    });
    // The G1 near-full proxy-define page: the caller's final attempt may
    // take one region of G1's allocation reserve
    // (`G1Collector::arm_reserve_last_chance`).
    shared.mem.heap.g1_arm_reserve_last_chance();
}



/// Run `body` (the last-ditch ladder's full collections, split or not) under
/// the tenure-all request on G1 (`CRATONVM_G1_LAST_DITCH_TENURES_ALL`), and
/// withdraw it after, whatever `body` returns.
pub(in crate::runtime::interpreter::gc_and_alloc) fn with_last_ditch_tenure<R>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    body: impl FnOnce(&mut JvmThread) -> R,
) -> R {
    let tenure = shared.mem.heap.is_g1();
    if tenure {
        cratonvm_gc::gc_quiescence::request_system_gc_tenure();
    }
    let out = body(thread);
    if tenure {
        let _ = cratonvm_gc::gc_quiescence::take_system_gc_tenure_request();
    }
    out
}

/// The G1 near-full proxy row: [`g1_last_ditch_reclaim_attempting`] (the two
/// full collections with `attempt` between them) under the tenure-all request,
/// as [`last_ditch_reclaim`] runs its two. The interpreter's split ladder and
/// the JIT allocation helpers' (`CRATONVM_G1_JIT_LADDER_ATTEMPTS`). `Some` = an
/// attempt allocated; `None` = both ran, and the caller attempts once more.
pub(crate) fn g1_last_ditch_split<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    attempt: &mut impl FnMut(&SharedVm, &mut JvmThread) -> Option<T>,
    between: bool,
) -> Option<T> {
    with_last_ditch_tenure(shared, thread, |thread| {
        g1_last_ditch_reclaim_attempting(shared, thread, attempt, between)
    })
}

/// The G1 near-full proxy row: `CRATONVM_G1_JIT_LADDER_ATTEMPTS` (default ON,
/// `=0` kills): the JIT allocation helpers' last-ditch rung on G1 is the split
/// ladder ([`g1_last_ditch_split`]), HotSpot's full, attempt, full, as the
/// interpreter's is; `=0` runs both full collections before the attempt.
/// Read once per JIT last-ditch rung.
pub(crate) fn g1_jit_ladder_attempts_enabled() -> bool {
    true
}

/// gci i14/ox: [`last_ditch_reclaim`]'s first half -- G1's complete marking
/// cycle ([`g1_last_ditch_full_cycle_with`]) and the recovery compaction
/// ([`g1_last_ditch_recovery_pause`]) -- inside one `Pause Full (G1
/// Compaction Pause)` report under `CRATONVM_G1_OOME_PAUSE_FULL_REPORT`
/// (`gc_events::g1_oome_full_report_begin`; `None`, so no report, off G1 or
/// with the switch off). `between` is the interpreter ladder's attempt, asked
/// between an open cycle's cleanup and the fresh cycle under
/// `CRATONVM_G1_OOME_RETRY_BEFORE_FRESH_CYCLE`; when it allocated, the fresh
/// cycle and the recovery pause do not run. Returns the armed-remark count
/// before the cycle (gci i13/oa item 2) and whether `between` allocated. A
/// no-op (but the count) off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn last_ditch_first_full(
    shared: &SharedVm,
    thread: &mut JvmThread,
    between: Option<&mut dyn FnMut(&SharedVm, &mut JvmThread) -> bool>,
) -> (u64, bool) {
    // gci i13/oa item 2: an armed remark inside the cycle below is counted.
    let armed_before = armed_soft_remarks(shared);
    let report = super::gc_events::g1_oome_full_report_begin(shared);
    // zt t31/x10: each reported half is one of HotSpot G1's two pre-OOME
    // full collections, which `full_gc_dump` brackets like any other.
    if report.is_some() {
        full_gc_heap_dump(shared, thread, true, full_gc_dump_gc_id(shared, true));
        owe_full_gc_heap_dump_after(shared, thread);
    }
    let satisfied = g1_last_ditch_full_cycle_with(shared, thread, between);
    oome_ladder_step(shared, thread, &format!("last-ditch-full-cycle satisfied={satisfied}"));
    // As in `force_full_collection`: a cycle the recovery pause opens is an
    // ordinary one, not part of this report.
    super::gc_events::g1_full_report_marking_done(report.as_ref());
    if !satisfied {
        g1_last_ditch_recovery_pause(shared, thread);
    }
    if let Some(report) = report {
        report.finish();
        settle_full_gc_heap_dump_after(shared, thread, true, full_gc_dump_gc_id(shared, false));
    }
    (armed_before, satisfied)
}

/// gci i14/ox: [`last_ditch_reclaim`]'s second half -- the soft-reference
/// rung (step 3) and the Generational major (step 4) -- with the rung inside
/// the second `Pause Full (G1 Compaction Pause)` report under
/// `CRATONVM_G1_OOME_PAUSE_FULL_REPORT`. HotSpot's second full collection
/// always runs once the first did not satisfy the request; when the rung ran
/// no collection on this thread (no active soft reference, gci i13/oa's
/// futile-rung skip, or four lost races) that report is given one
/// maximal-compaction pause of its own ([`g1_oome_second_full_pause`]), so the
/// `G1 Old Generation` count moves by HotSpot's two. Without the switch:
/// byte for byte the old steps 3 and 4.
pub(in crate::runtime::interpreter::gc_and_alloc) fn last_ditch_second_full(shared: &SharedVm, thread: &mut JvmThread, armed_before: u64) {
    let report = super::gc_events::g1_oome_full_report_begin(shared);
    if report.is_some() {
        full_gc_heap_dump(shared, thread, true, full_gc_dump_gc_id(shared, true));
        owe_full_gc_heap_dump_after(shared, thread);
    }
    let collected = if g1_soft_rung_skippable(shared, armed_before) {
        // G1 only: `collected` feeds `last_ditch_generational_major`, a
        // no-op off Generational. The engagement line `vi13-oa` E counts.
        if gc_overhead_dbg_on() {
            eprintln!(
                "[GC_OVERHEAD] g1-soft-rung: thread={} skipped=futile",
                thread.thread_id.0
            );
        }
        false
    } else {
        last_ditch_clear_soft_refs(shared, thread)
    };
    if let Some(report) = report {
        if g1_oome_second_full_needs_own_pause(collected) {
            g1_oome_second_full_pause(shared, thread);
        }
        report.finish();
        settle_full_gc_heap_dump_after(shared, thread, true, full_gc_dump_gc_id(shared, false));
    }
    oome_ladder_step(shared, thread, &format!("second-full soft-rung-collected={collected}"));
    last_ditch_generational_major(shared, thread, collected);
}

/// gci i14/ox: does the second `Pause Full (G1 Compaction Pause)` report
/// (open) need a pause of its own? Only when the soft-reference rung ran no
/// collection on this thread. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_oome_second_full_needs_own_pause(soft_rung_collected: bool) -> bool {
    !soft_rung_collected
}

/// gci i14/ox: the second full collection's own pause when the soft rung ran
/// none ([`last_ditch_second_full`], under
/// `CRATONVM_G1_OOME_PAUSE_FULL_REPORT` only): one recovery-compaction pause
/// on the same site as [`g1_last_ditch_recovery_pause`]'s (door
/// `G1CompactionPause`), with every garbage-bearing Old region the Free pool
/// can absorb armed (`g1_arm_full_compaction`, HotSpot's second full
/// collection is its maximal compaction) and the same root-snapshot handoff.
/// A lost STW race takes part in the other thread's pause and folds nothing
/// (the report then books only what it folded). G1 only.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_oome_second_full_pause(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    shared.mem.heap.note_young_spill_pressure();
    // As the recovery pause: the in-place pass alone on an untrusted mark.
    if !shared.mem.heap.g1_arm_full_compaction() {
        let _ = shared.mem.heap.g1_arm_in_place_compaction();
    }
    maybe_gc_forced_at(shared, thread, G1_LAST_DITCH_COMPACTION_SITE);
}

/// gci i14/ox: `CRATONVM_G1_OOME_RETRY_BEFORE_FRESH_CYCLE` (default OFF,
/// `runtime_flag_on`). Read once per interpreter-door G1 allocation-failure
/// ladder ([`last_ditch_then_second_major`]); no static.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_oome_retry_before_fresh_cycle_enabled() -> bool {
    false
}

/// gci i14/ox: which shape [`last_ditch_then_second_major`] runs. `split` =
/// the two full collections with the request attempted between them (G1 and
/// `CRATONVM_G1_OOME_PAUSE_FULL_REPORT`: HotSpot's `satisfy_failed_allocation`
/// attempts after its first `Pause Full` and runs the second only when that
/// attempt failed); `between` = the attempt is also asked between an open
/// cycle's cleanup and the fresh cycle (G1 and
/// `CRATONVM_G1_OOME_RETRY_BEFORE_FRESH_CYCLE`). Either one routes through
/// the split shape. Off G1 neither. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_oome_ladder_shape(g1: bool, report_on: bool, retry_on: bool) -> (bool, bool) {
    let between = g1 && retry_on;
    (g1 && (report_on || between), between)
}

/// gci i14/ox: the interpreter door's G1 last-ditch rung with attempts inside
/// it ([`last_ditch_then_second_major`] under one of the two opt-ins, see
/// [`g1_oome_ladder_shape`]): the first full collection (with `attempt` asked
/// between an open cycle's cleanup and the fresh cycle when `between`), an
/// attempt, then the second full collection. `Some` = an attempt allocated
/// (the rest did not run); `None` = both halves ran and the caller makes its
/// own attempt after them, as after [`last_ditch_reclaim`].
///
/// Soundness: an attempt here is what the caller makes right after
/// [`last_ditch_reclaim`] anyway -- a collection-free allocation, outside
/// every pause, re-deriving any address it needs. The one between the
/// cycles runs inside [`g1_force_full_cycle_with`]'s loop after a remark +
/// cleanup returned (or after it saw another thread close the cycle) with no
/// pause in flight, and with the thread-local last-ditch soft arming set when
/// the cycle runs under it (only a collection reads that, and an attempt
/// collects nothing). No `OutOfMemoryError` is thrown from here, so the soft
/// guarantee is unaffected: the error still comes only after the second half
/// (the soft rung).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_last_ditch_reclaim_attempting<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    attempt: &mut impl FnMut(&SharedVm, &mut JvmThread) -> Option<T>,
    between: bool,
) -> Option<T> {
    let mut got: Option<T> = None;
    let (armed_before, satisfied) = {
        let mut hook = |s: &SharedVm, t: &mut JvmThread| -> bool {
            got = attempt(s, t);
            got.is_some()
        };
        let hook_ref: Option<&mut dyn FnMut(&SharedVm, &mut JvmThread) -> bool> = if between {
            Some(&mut hook as &mut dyn FnMut(&SharedVm, &mut JvmThread) -> bool)
        } else {
            None
        };
        last_ditch_first_full(shared, thread, hook_ref)
    };
    if satisfied {
        if gc_overhead_dbg_on() {
            eprintln!(
                "[GC_OVERHEAD] g1-oome-retry: thread={} allocated=before-fresh-cycle",
                thread.thread_id.0
            );
        }
        return got;
    }
    if let Some(v) = attempt(shared, thread) {
        return Some(v);
    }
    last_ditch_second_full(shared, thread, armed_before);
    // The G1 near-full proxy-define page: the caller's final attempt may take
    // one reserve region, as after `last_ditch_reclaim` (this split shape is
    // G1's default ladder, so without it the arming reached no G1 door: the
    // gc-safe string door threw at 2 Free regions on
    // `Zs3Nd3UpcallMints proxy 4096 -Xmx64m`).
    shared.mem.heap.g1_arm_reserve_last_chance();
    None
}

/// gci i13/oa item 2: `AllocLadderState::armed_soft_remarks` now -- remarks
/// that ran the last-ditch soft-clear rule.
pub(in crate::runtime::interpreter::gc_and_alloc) fn armed_soft_remarks(shared: &SharedVm) -> u64 {
    shared
        .mem
        .alloc_ladder
        .armed_soft_remarks
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// gci i13/oa item 2 (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`):
/// may [`last_ditch_reclaim`] skip its soft-reference rung on G1? That rung is
/// a forced YOUNG pause: it can clear only a soft referent in its collection
/// set (Eden, Survivor) or in an eagerly reclaimed humongous span. Skipped
/// only under `CRATONVM_G1_OOME_SKIP_FUTILE_SOFT_RUNG` (default ON since
/// 2026-10-17) and only when
/// 1. a remark that ran the last-ditch soft-clear rule (it condemns every
///    active soft reference; `remark_reference_rows`) ran since `armed_before`
///    -- this thread's own, or another thread's armed one: the arming is
///    thread-local, so a cycle another thread closed WITHOUT it is no evidence
///    and the rung runs, as before;
/// 2. every soft row is bound (`ReferenceProcessor::active_soft_referents`),
///    and no active soft referent lies in a young or humongous region now
///    (`G1Collector::any_young_or_humongous_addr`) -- i.e. what is left is Old,
///    where the rung's young pause treats it as live.
/// Every other backend, and any doubt: the rung runs.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_soft_rung_skippable(shared: &SharedVm, armed_before: u64) -> bool {
    if !shared.mem.heap.is_g1() {
        return false;
    }
    let h = &shared.mem.heap;
    soft_rung_skippable(
        true,
        armed_soft_remarks(shared) > armed_before,
        || {
            let referents = shared.mem.ref_processor.lock().active_soft_referents();
            match referents {
                Some(referents) => !h.g1_any_young_or_humongous_addr(&referents),
                None => false,
            }
        },
    )
}

/// The pure decision of [`g1_soft_rung_skippable`]: the switch, an armed
/// remark, and (asked only then) nothing left the young pause could clear.
pub(in crate::runtime::interpreter::gc_and_alloc) fn soft_rung_skippable(
    switch_on: bool,
    armed_remark_ran: bool,
    nothing_young_left: impl FnOnce() -> bool,
) -> bool {
    switch_on && armed_remark_ran && nothing_young_left()
}



/// gcg g3o residual (`gcg-g3o-g1-latched-overhead-exit-differs-across-the-three-doors`,
/// 2026-10-04): the ONE rung every allocation door takes on G1 when the
/// GC-overhead streak is latched -- the interpreter's
/// (`collect_and_retry_with_thread`), the native factories'
/// (`native_reclaim_ladder_tail`), the JIT helpers'
/// (`jit_latched_overhead_limit_throws`) and the exception door's
/// (`create_exception_object_for_class_inner`). Each door then makes exactly
/// one attempt when [`g1_latched_overhead_exit`] allows it and throws
/// `OutOfMemoryError` otherwise.
///
/// Until then the doors disagreed: the interpreter ran [`last_ditch_reclaim`]
/// and attempted; the native factory ran the soft rung and then
/// [`g1_overhead_limit_full_cycle`]; the JIT helpers ran the soft rung, the
/// cycle only when the soft rung collected nothing, and threw at once on a
/// futile cycle in one helper and attempted after it, unbounded, in the
/// other four; the exception door attempted, then cycled only on a
/// productive verdict.
///
/// The rung is [`last_ditch_reclaim`]'s G1 steps -- the complete marking
/// cycle (soft rule armed when soft references are live), the one compacting
/// recovery pause when the failure proved fragmentation, the soft-reference
/// rung -- with the cycle judged by [`g1_overhead_limit_full_cycle`]
/// (`CRATONVM_DBG_GC_OVERHEAD=1` prints its `g1-overhead-cycle` line; a
/// productive cycle resets the streak). `before_live` is the
/// `live_bytes_estimate` the door sampled before its own collections.
/// Under `CRATONVM_GC_OVERHEAD_PROGRESS=0` the cycle runs unjudged (the
/// default verdict: not productive, nothing measured).
pub(crate) fn g1_latched_overhead_rung(
    shared: &SharedVm,
    thread: &mut JvmThread,
    before_live: usize,
) -> G1OverheadCycle {
    if !shared.mem.heap.is_g1() {
        return G1OverheadCycle::default();
    }
    let cycle = if gc_overhead_progress_enabled() {
        g1_overhead_limit_full_cycle(shared, thread, before_live)
    } else {
        g1_last_ditch_full_cycle(shared, thread);
        G1OverheadCycle::default()
    };
    g1_last_ditch_recovery_pause(shared, thread);
    let _ = last_ditch_clear_soft_refs(shared, thread);
    cycle
}

/// gcg g3o residual: G1's whole latched GC-overhead exit, the same at every
/// door -- [`g1_latched_overhead_rung`], then whether the door may make its
/// ONE attempt: after a cycle that freed anything (a productive one has also
/// reset the streak; a sliver may be exactly what this request needs, the
/// residual page's point), or, after a cycle that freed NOTHING, on the
/// latched exit's bounded grace ([`latched_overhead_grace`], the
/// Generational exit's since gce e2/o). A cycle that freed nothing with the
/// grace spent is the error, so a G1 heap wedged on live data throws instead
/// of running one full marking cycle per allocation -- the crawl the gen
/// r4w5/thrash5 rule ended on Generational, measured here as 284 cycles in
/// one `Gcd10oG1LatchProbe` run once the streak could latch on G1 (280 of
/// them freed 0 bytes; the attempt lived on what the forced young pause
/// freed). Not the Generational rule's 2 % cut: a G1 cycle that frees a
/// sliver is often reclaiming what compiled frames' stale slots held
/// (`GenR4W4NativeStringOomProbe` under an explicit limit: 0.5 MB, then the
/// `println` it needs), and refusing it threw where the unlatched path
/// allocates. Under `CRATONVM_GC_OVERHEAD_PROGRESS=0` the cycle is unjudged
/// and the attempt always made (the historical one attempt).
pub(crate) fn g1_latched_overhead_exit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    before_live: usize,
) -> bool {
    let cycle = g1_latched_overhead_rung(shared, thread, before_live);
    g1_latched_exit_attempts(gc_overhead_progress_enabled(), cycle, || {
        latched_overhead_grace(shared)
    })
}

/// The decision of [`g1_latched_overhead_exit`], pure: attempt when the cycle
/// is unjudged (`CRATONVM_GC_OVERHEAD_PROGRESS=0`), was productive or freed
/// anything, else only on the grace (taken -- `grace` called -- only then).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_latched_exit_attempts(
    judged: bool,
    cycle: G1OverheadCycle,
    grace: impl FnOnce() -> bool,
) -> bool {
    !judged || cycle.productive || cycle.cycle_freed > 0 || grace()
}

/// G1's step 1b of [`last_ditch_reclaim`] (split out by the gcg g3o residual
/// so [`g1_latched_overhead_rung`] runs the very same steps): the one
/// compacting recovery pause after the marking cycle. A no-op off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_last_ditch_recovery_pause(shared: &SharedVm, thread: &mut JvmThread) {
    if oome_ladder_dbg_all(shared) {
        let tid = thread.thread_id.0;
        let steps: Vec<String> = shared
            .mem
            .alloc_ladder
            .oome_ladder_trace
            .lock()
            .iter()
            .filter(|(t, _)| *t == tid)
            .map(|(_, s)| s.clone())
            .collect();
        eprintln!(
            "[cratonvm] oome-ladder: recovery-pause after: {} | now [{}]",
            steps.join(" -> "),
            oome_ladder_marks(shared)
        );
    }
    // Preserve the latest failed humongous span even when its initial failure
    // was exhaustion: marking may recover enough capacity in separate holes.
    // Arm compaction only when the fresh region census proves fragmentation
    // after this completed mark; ordinary mixed-policy caps remain untouched.
    if shared.mem.heap.is_g1() {
        shared.mem.heap.g1_arm_fragmented_humongous_compaction();
    }
    // Cleanup can return plenty of Free regions without one run large enough
    // for the allocation that brought us here: live objects in partially-full
    // Old regions still separate the holes.  The armed failure plan is exactly
    // ONE compacting mixed pause.  Do not drain the normal eight-pause mixed
    // cadence here: after the first pause has made the target window Free,
    // later forced young pauses are allowed to use it as to-space and can
    // fragment the very run the failed allocation is about to retry.
    if shared.mem.heap.is_g1() {
        // `g1_force_full_cycle` has just cleared the ordinary pressure latch.
        // Re-arm it for this one recovery pause: every Java frame and every
        // native argument is rooted at this allocation-failure boundary, so
        // G1 may use that exact root snapshot to discard stale RSet sources.
        // Without this handoff the recovery pause falls back to region-level
        // RSet scanning and can keep an unreachable old reference array (and
        // its target) alive long enough for the retry to fail despite ample
        // reclaimable space.  This is deliberately not a new global policy:
        // normal young pauses still retain their historical RSet contract.
        shared.mem.heap.note_young_spill_pressure();
        // gcd d10/o: unless a humongous window plan was armed above (it wins
        // the pause), the recovery pause also takes every garbage-bearing Old
        // region the Free pool can absorb -- see `g1_full_compaction_pause`.
        // The G1 near-full proxy row: on an untrusted mark, which the copying
        // pass refuses, the in-place pass alone (it marks from the roots).
        if !shared.mem.heap.g1_arm_full_compaction() {
            let _ = shared.mem.heap.g1_arm_in_place_compaction();
        }
        maybe_gc_forced_at(shared, thread, G1_LAST_DITCH_COMPACTION_SITE);
        oome_ladder_step(shared, thread, "recovery-pause");
        // r16 G1 page: when no window had a destination run for its live
        // humongous span, that pause only cleared one (a staging pause) and
        // re-armed the request. The G1 planner bounds the stages; this bound
        // only guards against a request that keeps being re-armed.
        for _ in 0..G1_COMPACTION_STAGE_PAUSES {
            if !shared.mem.heap.g1_humongous_compaction_pending() {
                break;
            }
            shared.mem.heap.note_young_spill_pressure();
            maybe_gc_forced_at(shared, thread, G1_LAST_DITCH_COMPACTION_SITE);
        }
        // gci i16/oo (opt-in `CRATONVM_G1_OOME_STAGED_FULL_COMPACTION`): the
        // pause above copies only what the Free pool can hold, so on a heap
        // that is nearly all Old it reclaims nothing; compact in stages.
        // gcki-ngr: not once the failed humongous request fits -- a stage
        // copies Old data into Free regions, and cut the 53-region run the
        // pause above had left for `NativeGrowthReclaimProbe`'s 50-region
        // grow to 43.
        if !(shared.mem.heap.g1_failed_humongous_request_fits())
        {
            let _ = g1_staged_full_compaction(shared, thread);
        }
        // The R22 aastore-humongous page (`CRATONVM_G1_FRAGMENTED_HUMONGOUS_SLIDE`):
        // the copies above keep the Free count but not its shape; a humongous
        // request still refused on shape gets one in-place slide that packs
        // Old into the lowest regions, HotSpot's `Pause Full`.
        if let Some((requested, free, longest)) =
            shared.mem.heap.g1_arm_fragmented_humongous_slide()
        {
            shared.mem.heap.note_young_spill_pressure();
            maybe_gc_forced_at(shared, thread, G1_LAST_DITCH_COMPACTION_SITE);
            oome_ladder_step(
                shared,
                thread,
                &format!("fragmented-slide requested={requested} free={free} longest={longest}"),
            );
        }
    }
}

/// Extra recovery pauses [`g1_last_ditch_recovery_pause`] runs while a staged
/// allocation-failure compaction keeps its request armed.
pub(in crate::runtime::interpreter::gc_and_alloc) const G1_COMPACTION_STAGE_PAUSES: usize = 3;





/// The most full-compaction stages [`g1_staged_full_compaction`] runs after
/// the recovery pause. Each stage at least grows the Free pool by one region
/// (else the loop stops), and a stage that has `f` Free regions to copy into
/// can take candidates holding up to `f - 1` regions of live data, so on a
/// heap whose Old regions are half garbage the pool roughly doubles per stage:
/// 16 stages cover any grid this VM builds from a few Free regions.
pub(in crate::runtime::interpreter::gc_and_alloc) const G1_STAGED_FULL_COMPACTION_PAUSES: usize = 16;

/// gci i16/oo (`gci-i15orch-g1-ooms-on-the-mixed-names-probe-at-96m-where-hotspot-does-not-FIXED-20261017`,
/// defect 1's last-ditch half): HotSpot's `Pause Full (G1 Compaction Pause)`
/// slides every live object down IN PLACE, so it needs no free space and
/// leaves a heap of `L` live bytes about `L` full. This collector's full
/// collection is an armed mixed pause (`G1Collector::arm_full_compaction`)
/// that COPIES its old candidates into Free regions, and
/// `full_compaction_candidates` takes only as much live data as the Free pool
/// (less one region) can hold. With 4 Free regions of 96 and the Old regions
/// about 30 % garbage, that is two or three candidates: `GciGo11G1NamesProbe
/// mixed -Xmx96m` (i15a) logged `Pause Young (Mixed) (G1 Compaction Pause)
/// 92M->92M(96M)` and then threw `OutOfMemoryError` with ~40 MB of garbage in
/// place, where HotSpot G1 completes.
///
/// The stages: re-arm the full compaction on the SAME completed mark (its
/// `live_bytes` stay the liveness verdict across pauses, as across an ordinary
/// mixed phase; the arm refuses once a new cycle has started or the last
/// cleanup was not trusted) and run one more forced pause, while the previous
/// stage grew the Free pool, up to [`G1_STAGED_FULL_COMPACTION_PAUSES`]. Each
/// stage frees the candidates it emptied and spends at most their live bytes
/// (plus an eighth) on destinations, so the pool grows while the cheapest
/// remaining candidates are less than ~89 % live, and the next stage can take
/// more. A humongous window plan still wins every pause it is armed for; the
/// loop stops while one is pending. Returns the stages run. A no-op off G1.
///
/// G1 only, OOME path only (the last-ditch rung and the latched overhead
/// rung). `-XX:+UseZGC` / Generational never reach it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_staged_full_compaction(shared: &SharedVm, thread: &mut JvmThread) -> usize {
    if !shared.mem.heap.is_g1() {
        return 0;
    }
    let h = &shared.mem.heap;
    let mut stages = 0usize;
    let mut free_before = h.g1_free_region_count().unwrap_or(0);
    // JIT round 22 wave 8 (lane memory, `CRATONVM_G1_STAGED_COMPACTION_BYTE_PROGRESS`):
    // the Old regions' bump extent, the second progress measure.
    let byte_progress = g1_staged_compaction_byte_progress_enabled();
    let mut old_before = if byte_progress { shared.mem.heap.region_census().unwrap().old.0 } else { 0 };
    // JIT round 22 wave 11 (lane memory, `CRATONVM_G1_OOME_MAXIMAL_COMPACTION`):
    // once the ordinary stages stop, the same loop again in MAXIMAL mode
    // (every garbage-bearing Old region is a candidate), with its own bound.
    let maximal_enabled = true;
    let mut maximal = false;
    let mut mode_start = 0usize;
    let mut stop: &str;
    loop {
        if stages - mode_start >= G1_STAGED_FULL_COMPACTION_PAUSES {
            stop = "bound";
        } else if shared.mem.heap.g1_humongous_compaction_pending() {
            stop = "humongous-plan-pending";
        } else {
            // The same root-snapshot handoff as the recovery pause.
            shared.mem.heap.note_young_spill_pressure();
            let armed = if maximal {
                shared.mem.heap.g1_arm_full_compaction_maximal()
            } else {
                shared.mem.heap.g1_arm_full_compaction()
            };
            if !armed {
                stop = "untrusted-mark";
            } else {
                maybe_gc_forced_at(shared, thread, G1_LAST_DITCH_COMPACTION_SITE);
                stages += 1;
                let free_after = h.g1_free_region_count().unwrap_or(0);
                let old_after = if byte_progress || maximal_enabled {
                    shared.mem.heap.region_census().unwrap().old.0
                } else {
                    0
                };
                if gc_overhead_dbg_on() {
                    eprintln!(
                        "[GC_OVERHEAD] g1-staged-compaction: thread={} stage={stages} maximal={maximal} free_before={free_before} free_after={free_after} old_before={old_before} old_after={old_after}",
                        thread.thread_id.0,
                    );
                }
                oome_ladder_step(
                    shared,
                    thread,
                    &format!(
                        "staged#{stages}{} free {free_before}->{free_after} old {old_before}->{old_after}",
                        if maximal { "(maximal)" } else { "" }
                    ),
                );
                let run = stages - mode_start;
                let go_on = if byte_progress || maximal {
                    staged_full_compaction_continues_by_bytes(
                        run,
                        free_before,
                        free_after,
                        old_before,
                        old_after,
                        shared.mem.heap.region_census().unwrap().region_size,
                    )
                } else {
                    staged_full_compaction_continues(run, free_before, free_after)
                };
                free_before = free_after;
                old_before = old_after;
                if go_on {
                    continue;
                }
                stop = "no-progress";
            }
        }
        // The ordinary stages stopped: the maximal pass, once (not after an
        // untrusted mark or a pending humongous plan, which it cannot help).
        if !maximal && maximal_enabled && stop != "untrusted-mark" && stop != "humongous-plan-pending" {
            maximal = true;
            mode_start = stages;
            if old_before == 0 {
                old_before = shared.mem.heap.region_census().unwrap().old.0;
            }
            continue;
        }
        break;
    }
    oome_ladder_step(shared, thread, &format!("staged-stop reason={stop} stages={stages}"));
    stages
}



/// JIT round 22 wave 12, lane memory (the FlipGcRows page;
/// `CRATONVM_JIT_ALLOC_LATCH_AFTER_FAILURE`, HotSpot parity, default ON,
/// `=0` restores the early exit): the compiled `new` / `anewarray` slow
/// paths (`jit_new_object_body`, `jit_anewarray_object_body`) consult a
/// latched GC-overhead streak only after an allocation attempt FAILED, as
/// the interpreter's ladder (`collect_and_retry_with_thread`) and
/// `jit_newarray_body` do. They used to consult it on entry, before any
/// attempt: every TLAB refill under a latched streak ran G1's latched exit (a
/// full marking cycle) and, when that cycle freed nothing, threw
/// `OutOfMemoryError` for an object the heap still had room for -- a throw
/// the interpreter never makes, and HotSpot's G1 has no overhead limit at
/// all. Read only while the streak is latched; no static.
pub(crate) fn jit_alloc_latch_after_failure_enabled() -> bool {
    true
}

/// JIT round 22 wave 8, lane memory (page
/// `docs/feature-designs/jit-ir-optimizations.md`;
/// `CRATONVM_G1_STAGED_COMPACTION_BYTE_PROGRESS`, HotSpot parity, default ON,
/// `=0` restores the region-count rule): [`g1_staged_full_compaction`] also
/// goes on after a stage that freed no WHOLE region but shrank the Old
/// regions' bump extent by at least an eighth of a region. On a nearly full
/// heap whose Old regions carry their garbage thinly (10-25 % each), a stage
/// may take only two or three candidates (the Free pool, less one region, is
/// the room), and copying them into a destination region nets a fraction of a
/// region: the old rule stopped there and the ladder threw with megabytes of
/// garbage in place, where HotSpot's sliding full collection compacts it all.
/// The next stage resumes the same destination region, so the fractions add
/// up to Free regions. Bounded by [`G1_STAGED_FULL_COMPACTION_PAUSES`], and
/// only on the way to `OutOfMemoryError`. Read once per staged compaction.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_staged_compaction_byte_progress_enabled() -> bool {
    true
}

/// The decision of [`g1_staged_full_compaction`] under
/// [`g1_staged_compaction_byte_progress_enabled`], pure: as
/// [`staged_full_compaction_continues`], or a stage that kept the Free pool
/// and shrank the Old extent by at least `region_size / 8`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn staged_full_compaction_continues_by_bytes(
    stages_run: usize,
    free_before: usize,
    free_after: usize,
    old_before: usize,
    old_after: usize,
    region_size: usize,
) -> bool {
    if staged_full_compaction_continues(stages_run, free_before, free_after) {
        return true;
    }
    stages_run < G1_STAGED_FULL_COMPACTION_PAUSES
        && free_after == free_before
        && old_after.saturating_add((region_size / 8).max(1)) <= old_before
}

/// The decision of [`g1_staged_full_compaction`], pure: one more stage only
/// after a stage that grew the Free pool, and never past the bound.
pub(in crate::runtime::interpreter::gc_and_alloc) fn staged_full_compaction_continues(
    stages_run: usize,
    free_before: usize,
    free_after: usize,
) -> bool {
    stages_run < G1_STAGED_FULL_COMPACTION_PAUSES && free_after > free_before
}

/// Step 4 of [`last_ditch_reclaim`]: the Generational major when the soft
/// rung (`collected`) ran none. A no-op off Generational.
pub(in crate::runtime::interpreter::gc_and_alloc) fn last_ditch_generational_major(shared: &SharedVm, thread: &mut JvmThread, collected: bool) {
    // gcd d2/j: the major below is the soft step's discipline now — bounded
    // retries of a collection THIS thread runs, and the thread-local request
    // withdrawn when every attempt lost the STW race
    // ([`generational_major_on_this_thread`]). It used to be one
    // `maybe_gc_forced_at` whose lost race left the request pending: the
    // ladder then threw `OutOfMemoryError` after a peer's YOUNG pause with the
    // old generation's garbage still in place, and the stale request turned
    // this thread's next unrelated collection into a major (and chose the
    // requested-major root overlay for it).
    //
    // gen r4w3/hunter (2026-09-23): the Generational backend's allocation
    // ladder never reached the OLD generation on its own. `maybe_gc_forced`
    // runs a young cycle, and that cycle runs a major one only when old gen
    // is at least 75% full or a major was requested
    // (`OldGen::major_trigger_verdict`); the only request on this ladder was
    // the soft-reference step above, which returns early when the program
    // holds no live soft references. So an allocation that needs space the
    // old generation holds as GARBAGE below the 75% floor — a large array,
    // which goes straight to old gen, or a young survivor that cannot be
    // promoted — threw `OutOfMemoryError` with most of the heap reclaimable.
    // HotSpot's Serial collector runs a full collection before giving up.
    // Same thread-local request and the same STW-race caveat as the soft
    // step's own major (see its comment). A soft step that did collect has
    // already requested a major, so this runs at most one extra collection,
    // and only on the path to `OutOfMemoryError`.
    // See `docs/feature-designs/generative-gc-architecture.md`.
    if !collected && shared.mem.heap.is_generational() {
        // `maybe_gc_forced_collected` retires this thread's TLAB itself.
        let _ = generational_major_on_this_thread(
            shared,
            thread,
            "last-ditch-major",
            LAST_DITCH_MAJOR_ATTEMPTS,
        );
    }
}

/// Step 1 of [`last_ditch_reclaim`]: G1's complete marking cycle, with the
/// last-ditch soft rule armed when the program holds live soft references
/// (gc-common w1-d, see there). A no-op on the other backends. Split out by
/// gcd d10/o so the overhead-limit exit ([`g1_overhead_limit_full_cycle`])
/// runs the very same cycle.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_last_ditch_full_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    let _ = g1_last_ditch_full_cycle_with(shared, thread, None);
}

/// [`g1_last_ditch_full_cycle`] with gci i14/ox's `between` hook, handed to
/// [`g1_force_full_cycle_with`]: `true` = the hook allocated after an open
/// cycle's cleanup and the fresh cycle did not run. `None`: the old cycle,
/// byte for byte.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_last_ditch_full_cycle_with(
    shared: &SharedVm,
    thread: &mut JvmThread,
    between: Option<&mut dyn FnMut(&SharedVm, &mut JvmThread) -> bool>,
) -> bool {
    // gci i13/gp (`gci-i13oa-...-review-findings-20261013` item 7, opt-in
    // `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`): HotSpot's allocation-failure
    // full collection aborts a running cycle; report it so. No-op otherwise.
    let _ = super::gc_events::g1_last_ditch_abort_open_cycle(shared);
    let arm_soft_clear = shared.mem.heap.is_g1()
        && weakref_clear_enabled()
        && shared.mem.ref_processor.lock().has_active_soft_refs();
    // gci i30/ob (`i28-dg-report.md` review item 1): HotSpot's
    // allocation-failure full collection is `VM_G1CollectFull`, stop-the-world
    // whatever `-XX:+ExplicitGCInvokesConcurrent` says (only `System.gc()` and
    // `jcmd GC.run` turn concurrent under it), so the fresh cycle is the
    // stop-the-world one. It followed the flag: a concurrent cycle, whose
    // marking window lets every `Reference.get()` keep its referent alive,
    // on the path to `OutOfMemoryError`.
    let door_forces_stw = true;
    // gci i30/ob hand-back 1: the rows' witness of the decision, at INFO (a
    // release build compiles `debug!` out: vi30-ob L1 could not see
    // `g1_stw_full_mark_cycle`'s line). Off unless `RUST_LOG` selects the
    // target; once per last-ditch cycle, on the path to `OutOfMemoryError`.
    if shared.mem.heap.is_g1() {
        tracing::info!(
            target: "cratonvm::g1_last_ditch",
            "[G1] last-ditch full cycle: fresh cycle stop-the-world={} (explicit-gc concurrent={})",
            door_forces_stw || g1_force_full_cycle_is_stw(shared.config.explicit_gc),
            !g1_force_full_cycle_is_stw(shared.config.explicit_gc)
        );
    }
    if arm_soft_clear {
        crate::runtime::interpreter::with_last_ditch_soft_clear(|| {
            g1_force_full_cycle_with(shared, thread, between, door_forces_stw)
        })
    } else {
        g1_force_full_cycle_with(shared, thread, between, door_forces_stw)
    }
}


/// gcd d10/o (2026-09-28): G1's full collection before a GC-overhead-limit
/// `OutOfMemoryError`. `true` = the cycle freed at least 2 % of the heap
/// since `before_live` (a `VmHeap::live_bytes_estimate` the caller sampled
/// before its own collections), so the streak is reset and the caller retries
/// once; `false` = no cycle (not G1, or `CRATONVM_GC_OVERHEAD_PROGRESS=0`), or
/// a futile one, and the verdict stands.
///
/// # The gap
///
/// With the overhead streak latched, every allocation ladder honoured the
/// verdict after the forced collection and the soft-reference rung: the
/// interpreter's (`collect_and_retry_with_thread`, whose comment said "G1's
/// last-ditch cycle stays off this fast-fail exit"), the native door's
/// (`NativeContextImpl::reclaim_before_alloc_retry`) and the JIT helpers'
/// (`jit_latched_overhead_limit_throws`, unless a soft-reference collection
/// ran). On Generational that exit runs a major first (gen r4w4, gcd d4/j),
/// and a ZGC forced cycle is itself whole-heap. On G1 the forced collection
/// is a YOUNG pause, and dead Old and humongous regions are reclaimed only by
/// a completed marking cycle's cleanup (see [`g1_force_full_cycle`]), so a
/// G1 program that caught an `OutOfMemoryError` and dropped its data met the
/// latched verdict on its next allocation with the dropped data still in
/// place: an `OutOfMemoryError` no full collection had decided. HotSpot's G1
/// runs a full collection before every heap `OutOfMemoryError`.
///
/// # The rule
///
/// The Generational exit's (gen r4w5/thrash5): a full collection this caller
/// ran decides. A productive one resets the streak (as `note_gc_productivity`
/// does for a Generational major) and earns one retry; a futile one keeps the
/// verdict, so a G1 heap wedged on live data still throws instead of limping
/// on one full cycle per object. Measured from the caller's `before_live`:
/// the interpreter's ladder samples it just before this call (its attempt
/// after the forced collection has already failed), the native door before
/// its forced collection (it cannot attempt in between, so what that
/// collection and the soft rung freed counts too). The cycle is
/// [`last_ditch_reclaim`]'s own G1 step (soft rule armed when soft references
/// are live).
///
/// `CRATONVM_DBG_GC_OVERHEAD=1` prints one `[GC_OVERHEAD] g1-overhead-cycle:`
/// line per call.
pub(crate) fn g1_overhead_limit_full_cycle(
    shared: &SharedVm,
    thread: &mut JvmThread,
    before_live: usize,
) -> G1OverheadCycle {
    if !shared.mem.heap.is_g1() || !gc_overhead_progress_enabled() {
        return G1OverheadCycle::default();
    }
    let cap = shared.mem.heap.heap_capacity();
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    let cycle_before = shared.mem.heap.live_bytes_estimate();
    g1_last_ditch_full_cycle(shared, thread);
    let after = shared.mem.heap.live_bytes_estimate();
    let cycle_freed = cycle_before.saturating_sub(after);
    let productive = g1_overhead_cycle_productive(before_live, after, cap);
    if productive {
        shared
            .mem
            .gc_unproductive_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        // gcg g3o residual: the latched episode is over, as when
        // `note_gc_productivity` resets the streak.
        reset_latched_grace(shared);
    }
    if gc_overhead_dbg_on() {
        eprintln!(
            "[GC_OVERHEAD] g1-overhead-cycle: thread={} before={before_live} after={after} \
             cap={cap} productive={productive} cycle_freed={cycle_freed}",
            thread.thread_id.0,
        );
    }
    G1OverheadCycle {
        productive,
        cycle_freed,
    }
}

/// What [`g1_overhead_limit_full_cycle`] found: `productive` -- at least 2 %
/// of the heap freed since the door's `before_live` (the streak was reset);
/// `cycle_freed` -- the bytes the marking cycle ITSELF freed (live before it
/// minus live after it). Default: no cycle ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct G1OverheadCycle {
    pub productive: bool,
    pub cycle_freed: usize,
}

/// The pure verdict of [`g1_overhead_limit_full_cycle`]: freed at least 2 %
/// of `cap` (the overhead limit's one yardstick, [`two_percent_sliver`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_overhead_cycle_productive(before_live: usize, after_live: usize, cap: usize) -> bool {
    // Widening: usize -> u64 is loss-free on every supported target.
    cap != 0 && !two_percent_sliver(before_live.saturating_sub(after_live) as u64, cap)
}


/// Attempts [`last_ditch_reclaim`]'s Generational major makes before it gives
/// up on winning the STW race: the soft step's `LAST_DITCH_ATTEMPTS`, for the
/// same reason (each lost attempt lets a peer's pause finish, so this cannot
/// livelock).
pub(in crate::runtime::interpreter::gc_and_alloc) const LAST_DITCH_MAJOR_ATTEMPTS: u32 = 4;

/// A Generational major collection run by THIS thread for a rung of the
/// allocation-failure ladder: the thread-local major request
/// (`gc_quiescence::request_major_gc`) is honoured only by a collection this
/// thread initiates, so request and collect up to `attempts` times until this
/// thread wins the STW race. Returns whether it did. A request still pending
/// afterwards -- every attempt lost, or a won pause that did not consume it --
/// is withdrawn, so it cannot turn this thread's next unrelated collection
/// into a major. gcd d2/j (2026-09-27). `pub(crate)` for the collecting door
/// of `runtime::exceptions::create_exception_object_for_class_inner`.
pub(crate) fn generational_major_on_this_thread(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    attempts: u32,
) -> bool {
    let mut collected = false;
    let t0 = std::time::Instant::now();
    for _attempt in 0..attempts {
        cratonvm_gc::gc_quiescence::request_major_gc();
        collected = maybe_gc_forced_collected(shared, thread, site);
        if collected {
            break;
        }
    }
    if cratonvm_gc::gc_quiescence::major_gc_requested() {
        let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
    }
    // gcd d9/b: `[GC] oome_ladder: ladder_majors*`. The time includes any
    // lost attempts' participation in a peer's pause.
    {
        use cratonvm_gc::gen_heap::oome_ladder as l;
        if collected {
            ladder_census(shared, l::MAJORS, 1);
            ladder_census(shared, l::MAJORS_US, elapsed_us(t0));
        } else {
            ladder_census(shared, l::MAJORS_LOST, 1);
        }
    }
    collected
}

/// Collect once with every SoftReference condemned — see
/// `ReferenceProcessor::condemn_all_soft_refs` for the rule and
/// `with_last_ditch_soft_clear` for why the arming is thread-local.
///
/// Skipped outright when the application holds no live soft references, which
/// is the common case; a heap that is genuinely full then pays no extra GC on
/// its way to `OutOfMemoryError`.
///
/// Returns whether it initiated a collection (gen r4w3/hunter: the caller runs
/// the Generational last-ditch major itself when this step did not).
pub(crate) fn last_ditch_clear_soft_refs(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    // zr z5/cr: a refusal whose forced collection the futile rule skipped is
    // owed ONE collection here, soft references or not, so a refusal never
    // reaches its `OutOfMemoryError` without a collection after it (ZGC only;
    // never armed elsewhere).
    let owed = zgc_refusal_take_owed(shared);
    if !weakref_clear_enabled() {
        // The opt-out restores the legacy never-clearing behaviour wholesale;
        // that has to include this rule, or `CRATONVM_WEAKREF_CLEAR=0` would
        // no longer be the byte-identical safety net it is documented to be.
        return owed && zgc_refusal_pay_owed(shared, thread);
    }
    if !shared.mem.ref_processor.lock().has_active_soft_refs() {
        // zt t24/hunt7 (`CRATONVM_ZGC_OOM_LADDER_FRESH_MARK`): with nothing to
        // clear, the ladder may still rest on a mark that started before the
        // failure (a concurrent cycle open now, or closed by the ladder's
        // forced collection with its SATB mark set); one fresh-mark
        // collection first, which also pays an owed one.
        if zgc_oom_ladder_fresh_mark_due(shared) {
            return zgc_oom_ladder_fresh_mark(shared, thread, owed);
        }
        return owed && zgc_refusal_pay_owed(shared, thread);
    }
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    // gc-common w1-d (2026-09-23): on Generational this collection must reach
    // the OLD generation, or a condemned soft referent that was tenured —
    // which is where a long-lived soft cache lives — is judged to have
    // survived (`watched_pre_gc_addr_survived` answers an old-gen address
    // "survived" unless old-gen storage was reclaimed this cycle), gets its
    // slot restored, and the caller throws `OutOfMemoryError` holding it.
    // `maybe_gc_forced` never asks for a major cycle on its own; the same
    // thread-local request `System.gc()` makes (`force_gc_from_native`) is
    // what makes the collector run one. Thread-local, and consumed by the very
    // next Generational collection on this thread — this one, unless it loses
    // the STW race, in which case it rides this thread's next collection,
    // which is also an allocation-failure retry. G1 got the equivalent above
    // (`last_ditch_reclaim` arms the remark); ZGC's collection is whole-heap.
    //
    // LOSING THE RACE LOST THE RULE (gc-common w2-a, 2026-09-23). Both halves
    // of this rung are thread-local — the soft-clear arming
    // (`with_last_ditch_soft_clear`) and the major request — so only a
    // collection THIS thread initiates honours them. On the path to
    // `OutOfMemoryError` every allocating thread is racing for the same pause,
    // and a thread that lost it took part in someone else's ordinary
    // collection: no soft reference was condemned, the caller threw OOM with
    // its soft cache intact, and the unconsumed major request leaked into the
    // thread's next unrelated collection. Retry a bounded number of times,
    // exactly as `System.gc()` does (every lost attempt lets another thread's
    // pause finish, so this cannot livelock), and withdraw the request if every
    // attempt lost.
    //
    // gc-common w5-a (`handoff-w4d-last-ditch-soft-clear-per-vm-request`,
    // residuals §4): the soft-clear half is now ALSO recorded per VM, so the
    // next collection applies it whichever thread initiates it -- including
    // another thread's pause that wins every attempt below. Consumed exactly
    // once by `weakref_null_referents_pre_gc` (which also consumes it when
    // this thread's own attempt wins, so no later ordinary collection is
    // affected). The major-GC request stays thread-local: it is withdrawn
    // below when every attempt lost.
    shared
        .mem
        .ref_processor
        .lock()
        .request_clear_all_soft_refs();
    const LAST_DITCH_ATTEMPTS: u32 = 4;
    let generational = shared.mem.heap.is_generational();
    // zs s2/rf (`zgc-zr1rf-concurrent-mark-traces-reference-slots-strongly`,
    // "the last-ditch soft clear under an open cycle"): read BEFORE the
    // collection, which closes the cycle.
    let zgc_conc_open = shared.mem.heap.zgc_concurrent_mark_active();
    // zt t23/hunt5 (`CRATONVM_ZGC_LAST_DITCH_FRESH_MARK`, default ON, `=0`
    // kill): with a ZGC concurrent cycle open, the clear-all collection marks
    // from scratch instead of closing that cycle with its mark set. The full
    // request is the collecting thread's (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`
    // abandons the open cycle for it). HotSpot's ZGC fails a stalled allocation
    // only after a cycle that STARTED after the stall (`Allocation Stall`,
    // which clears every soft reference). A concurrently marked cycle also
    // stands the soft-precise probe down (`CRATONVM_ZGC_SOFT_CLEAR_PRECISE_FIRST`
    // needs the serial two-drain mark), so `zgc_soft_clear_all_conservative_referents`
    // cleared a soft reference whose referent a static still holds whenever a
    // stale compiled-frame word named it too (`Zr7SpStaticHolderStaleWord`:
    // `soft_cleared true`, HotSpot `false`). Read only with a cycle open.
    let zgc_fresh = zgc_conc_open;
    let mut collected = false;
    for _attempt in 0..LAST_DITCH_ATTEMPTS {
        if generational || zgc_fresh {
            cratonvm_gc::gc_quiescence::request_major_gc();
        }
        collected = crate::runtime::interpreter::with_last_ditch_soft_clear(|| {
            maybe_gc_forced_collected(shared, thread, "last-ditch-soft-refs")
        });
        if collected {
            break;
        }
    }
    // zt t23/hunt5: on ZGC the request is withdrawn whatever the outcome (a
    // won collection consumed it unless `CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK=0`,
    // which leaves it to the next collection on this thread).
    if (!collected && generational) || zgc_fresh {
        let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
    }
    // zs s2/rf: on ZGC a clear-all collection that ENDS an open concurrent
    // cycle uses that cycle's mark set, in which every soft referent the
    // phase could see through an unhidden slot is already marked (no
    // `CRATONVM_ZGC_CONC_REF_SKIP`, or a soft `Reference` registered during
    // the phase): the pre-pass nulls the slot, the referent "survives", and
    // the slot is restored -- "clear every softly reachable object before
    // OutOfMemoryError" did not hold for that collection. One more clear-all
    // collection, now stop-the-world, does it. Only when a concurrent cycle
    // was open on entry (opt-in `CRATONVM_ZGC_CONC_START`); kill
    // `CRATONVM_ZGC_LAST_DITCH_CONC_SOFT_REPEAT=0`.
    if collected && zgc_conc_open {
        let still_active = shared.mem.ref_processor.lock().has_active_soft_refs();
        let repeat = still_active;
        // The engagement witness for host rows: the residual is REACHED when
        // a concurrent cycle was open and soft references are still active
        // after the clear-all collection (printed on both arms; the kill arm
        // is the positive control).
        if crate::runtime::interpreter::dbg_weakref_on() {
            eprintln!(
                "[weakref] last-ditch: zgc concurrent cycle was open; soft refs active after the clear-all collection: {still_active}; repeat: {repeat}"
            );
        }
        if repeat {
            shared
                .mem
                .ref_processor
                .lock()
                .request_clear_all_soft_refs();
            for _attempt in 0..LAST_DITCH_ATTEMPTS {
                // zt t23/hunt5: a cycle opened since by another thread's poll
                // is not closed with its mark set either.
                if zgc_fresh {
                    cratonvm_gc::gc_quiescence::request_major_gc();
                }
                if crate::runtime::interpreter::with_last_ditch_soft_clear(|| {
                    maybe_gc_forced_collected(shared, thread, "last-ditch-soft-refs")
                }) {
                    break;
                }
            }
            if zgc_fresh {
                let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
            }
            if crate::runtime::interpreter::dbg_weakref_on() {
                eprintln!(
                    "[weakref] last-ditch: after the repeat soft refs active: {}",
                    shared.mem.ref_processor.lock().has_active_soft_refs()
                );
            }
        }
    }
    collected
}



/// zt t24/hunt7 (`zgc-zt23hunt5-oom-ladder-can-rest-on-a-mark-that-started-before-the-stall-*`):
/// does the clear-soft rung, reached with no active soft reference, owe the
/// ladder a collection that marks from scratch? On ZGC, when a concurrent
/// cycle is open (the rung's own collection would close it with its mark set)
/// or the last collection closed one with its mark set
/// (`ZgcRealHeap::last_collection_marked_concurrently`: the ladder's forced
/// collection, or a peer's, reused a snapshot taken before the failure). That
/// mark keeps every object live at its start, so an array the program dropped
/// after it is still there when `OutOfMemoryError` is thrown; HotSpot fails a
/// stalled allocation only after a GC cycle that STARTED after the stall.
/// `false` off ZGC and on the default stop-the-world ZGC, which never marks
/// concurrently (the switch is not even read there).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_oom_ladder_fresh_mark_due(
    shared: &SharedVm,
) -> bool {
    let heap = &shared.mem.heap;
    heap.zgc_concurrent_mark_active() || heap.zgc_last_collection_marked_concurrently()
}

/// zt t24/hunt7: the fresh-mark collection of [`zgc_oom_ladder_fresh_mark_due`]:
/// the collecting thread's full request (`gc_quiescence::request_major_gc`),
/// so an open cycle is abandoned (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`) and
/// the mark starts now, at the rung's site (a refusal collection, never
/// skipped), retried as the soft clear is when this thread loses the
/// stop-the-world race, the request withdrawn after. `owed` (a skipped
/// refusal's collection, already taken) is paid by it and counted as before.
/// Returns whether this thread ran the collection.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_oom_ladder_fresh_mark(shared: &SharedVm, thread: &mut JvmThread, owed: bool) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    if owed {
        census.owed_paid.fetch_add(1, Relaxed);
    }
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    const ATTEMPTS: u32 = 4;
    let mut collected = false;
    for _attempt in 0..ATTEMPTS {
        cratonvm_gc::gc_quiescence::request_major_gc();
        collected = maybe_gc_forced_collected(shared, thread, ZGC_REFUSAL_SOFT_RUNG_SITE);
        if collected {
            break;
        }
    }
    let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
    // zt t24/x20 (item 30i): counted only when the collection really marked
    // from scratch. Under `CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK=0`, or when
    // the open cycle is a `System.gc()`-owned one (never abandoned), it closes
    // the open cycle with that cycle's mark set; that run is not a fresh mark.
    if collected && !shared.mem.heap.zgc_last_collection_marked_concurrently() {
        census.oom_fresh_marks.fetch_add(1, Relaxed);
    }
    if crate::runtime::interpreter::dbg_weakref_on() {
        eprintln!(
            "[weakref] last-ditch: no active soft refs; zgc fresh-mark collection ran: {collected}"
        );
    }
    collected
}



/// Last-ditch G1 full marking cycle before declaring OutOfMemoryError.
///
/// Young/mixed pauses reclaim only collection-set regions; dead Old regions
/// and dead humongous spans are reclaimed exclusively by a completed mark
/// cycle's cleanup phase (`reclaim_dead_humongous_spans_locked`). When an
/// allocation still fails after the forced young GC, the heap may simply be
/// full of *unmarked dead* Old/humongous data — run one complete cycle
/// synchronously (start → drain → final remark → cleanup) and let the caller
/// retry the allocation once more before throwing OOM. Mirrors HotSpot's
/// last-ditch full GC on allocation failure. Also `System.gc()`'s G1 cycle
/// (`force_gc_from_native`).
///
/// No-op on non-G1 backends.
///
/// # Bounded, parked, and it finishes the cycle (gc-common w3-a, 2026-09-23)
///
/// This used to YIELD-SPIN until the background marker quiesced, for up to two
/// seconds, and then give up silently with the cycle still open. The
/// orchestrator's wave-2 battery caught a failing G1 `RefProbe` run in which
/// every allocation attempt on the exhausted heap was 8-16 s apart, all of it
/// CPU: one failed allocation reaches this function more than once (the
/// ladder's `last_ditch_reclaim`, then the OOM path's own ladder), and each
/// visit spun its full two seconds on a cycle that never quiesced. Three
/// changes:
///
/// 1. **The quiescence wait is short and bounded** —
///    [`FORCE_FULL_QUIESCE_WAIT`] — and after it the final remark runs anyway.
///    Quiescence is a latency preference, not a correctness gate:
///    `VmHeap::g1_final_remark_and_cleanup` stops and joins the marker, then
///    drains the gray set to a fixed point itself, inside the pause. So a
///    marker that cannot converge (more work published by every young pause
///    of a thrashing heap) no longer costs the caller its cycle; it costs a
///    longer remark, which on the path to `OutOfMemoryError` (or in an
///    explicit `System.gc()`) is exactly HotSpot's full-GC trade.
/// 2. **It parks instead of spinning** ([`LastDitchBackoff`]): a few yields,
///    then sleeps growing to 1 ms, while still taking part in every pause
///    another thread requests (`safepoint_check` each turn).
/// 3. **A cycle it could not complete is reported** — only possible now when
///    every attempt for [`FORCE_FULL_CYCLE_BUDGET`] lost its STW request to
///    another initiator — with the reason, instead of a `debug!` nobody sees.
///
/// Returns as soon as THIS call ran the remark + cleanup of a cycle that
/// STARTED after the call, or observed another thread close such a cycle.
///
/// # A cycle already open on entry is finished, then a fresh one runs
///
/// A cycle's SATB snapshot keeps everything that was live when IT started. A
/// cycle opened by the IHOP trigger while the program was still filling the
/// heap therefore keeps the very data the program has since dropped, and its
/// cleanup reclaims none of it: `NativeFactoryReclaimProbe -Xmx128m` saw the
/// last-ditch cleanup end at 33 Free / 87 Old in about half its rounds (the
/// rounds whose fill had crossed IHOP) and threw `OutOfMemoryError`, against
/// 105-121 Free in the others. So the open cycle is finished (it must be,
/// before another can start) and one more is driven from a snapshot taken
/// after the allocation failed -- HotSpot's full collection on this path marks
/// from scratch too. `CRATONVM_G1_FORCE_FULL_FRESH_CYCLE=0` restores the
/// finish-the-open-cycle-only behaviour.
///
/// gci i15/ca: under `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE` (default OFF) an
/// open cycle the caller's full collection marked aborted is not finished but
/// aborted ([`g1_abort_open_cycle_pause`]), so only the fresh cycle marks.
pub(crate) fn g1_force_full_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    let _ = g1_force_full_cycle_with(shared, thread, None, false);
}

/// gci i28/dg (i27/ob review item 4): [`g1_force_full_cycle`] for a full
/// collection whose cause HotSpot never turns concurrent: its fresh cycle is
/// the stop-the-world [`g1_stw_full_mark_cycle`] whatever
/// `-XX:+ExplicitGCInvokesConcurrent` says.
pub(crate) fn g1_force_full_cycle_stw(shared: &SharedVm, thread: &mut JvmThread) {
    let _ = g1_force_full_cycle_with(shared, thread, None, true);
}

/// gci i28/dg (i27/ob review item 4): does a G1 full collection on `door` run
/// its fresh cycle stop-the-world even under `-XX:+ExplicitGCInvokesConcurrent`?
/// HotSpot 25 `G1CollectedHeap::should_do_concurrent_full_gc` turns only
/// `_java_lang_system_gc` and `_dcmd_gc_run` concurrent under the flag
/// (`is_user_requested_concurrent_full_gc`); `_jvmti_force_gc`, the heap
/// inspection / dump and the metadata full collection stay `VM_G1CollectFull`
/// (`Pause Full`). It used to read the policy alone, so a JVMTI
/// `ForceGarbageCollection` under the flag ran a concurrent cycle. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_door_forces_stw_full_cycle(door: GcDoor) -> bool {
    !matches!(door, GcDoor::SystemGc | GcDoor::DiagnosticCommand)
}

/// gci cs48 (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`,
/// vi28-dg E4): is a full collection on `door` HotSpot 25's user-requested
/// concurrent cycle -- G1, `System.gc()` / `jcmd GC.run` under
/// `-XX:+ExplicitGCInvokesConcurrent` -- whose own young pause is the forced
/// `Concurrent Start` and which runs no compaction after its cleanup? `false`
/// under `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE=0` (the IHOP tick and the
/// compaction pause, as before). The switch is read only for such a door.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_door_is_user_requested_concurrent(shared: &SharedVm, door: GcDoor) -> bool {
    shared.mem.heap.is_g1()
        && !g1_door_forces_stw_full_cycle(door)
        && !g1_force_full_cycle_is_stw(shared.config.explicit_gc)
        && super::gc_events::g1_concurrent_start_at_young_pause_enabled()
}

