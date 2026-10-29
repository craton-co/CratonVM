// Array allocation helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer
// passed through from the interpreter. atype encodes a JVM array element type (T_BOOLEAN..T_LONG).
// length is the requested array size. The returned i64 is a raw heap pointer to the new array.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// Open a ZGC concurrent mark cycle from a JIT allocation helper, if the
/// occupancy threshold has been crossed.
///
/// # Why the JIT needs its own call to this
///
/// `maybe_gc` is the interpreter's allocation hook, and it is where the
/// concurrent-start check naturally lives. **A JIT-compiled allocation loop
/// never reaches it.** `jit_newarray` calls `heap.try_alloc_array_full`, which
/// succeeds until the heap is genuinely full, so on this backend a fully
/// compiled `new byte[128]` loop consults no occupancy predicate at all: its
/// collections arrive by allocation FAILURE. Measured 2026-08-16 --
/// `ZgcConcMarkProbe` single-threaded reported `cycles_started=0` with six
/// collections, i.e. the feature could not engage on the workload it was
/// written for, while the multi-threaded probe (whose peers do run
/// interpreted code) engaged on every cycle.
///
/// The JIT safepoint poll is not an alternative: `emit_safepoint_poll` fires
/// only when `stw_requested` is already set, which is a consequence of a
/// collection rather than a cause of one.
///
/// Cost on the other two backends and on a ZGC run with the feature off: one
/// `match` and one load of a plain `usize` field that is `0`.
/// G1's concurrent-mark lifecycle, on the path a compiled workload takes.
///
/// The G1 twin of the ZGC hook below, and for a defect one step worse than the
/// one that hook was written for: ZGC's trigger merely engaged rarely on a JIT
/// workload, while G1's is unreachable in BOTH directions -- nothing starts a
/// cycle and, more importantly, nothing finishes one. See
/// `g1_drive_concurrent_mark_pub` for the measurements.
///
/// Cost when G1 is not the collector, or has nothing to do: an `is_g1()` match
/// and two relaxed loads, the same budget the ZGC arm spends.
#[inline]
// SAFETY: Must be called from a registered JVM thread context. Drives G1
// STW pauses and mutator TLAB retirement.
unsafe fn jit_drive_g1_concurrent_mark(vm: &SharedVm) {
    if !g1_jit_mark_driver_enabled() || !vm.mem.heap.is_g1() {
        return;
    }
    // DECIDE BEFORE TOUCHING THE THREAD.
    //
    // `thread.tlab.retire()` costs the allocation fast path, so it must happen
    // only when this call is really going to take a pause. The first version
    // asked only "is a cycle active?", which is true for the WHOLE of a mark
    // cycle -- so every JIT allocation in that window retired its TLAB and the
    // H2 workload stopped finishing at all (rc=124 at 600 s against ~60 s).
    // The mixed collections it unblocked were real (9 pauses, 516 MB freed);
    // the cost of getting there was not.
    let work = if vm.mem.heap.g1_is_marking_active() {
        // Only a DRAINED cycle has anything to do here.
        vm.mem.heap.g1_concurrent_mark_finished()
    } else {
        // gci cs48: no cycle opens between a `Mixed` pause and the next
        // young-only one (`g1_start_waits_for_young_only_pause`).
        !crate::runtime::interpreter::g1_start_waits_for_young_only_pause(vm)
            && vm.mem.heap.g1_should_start_marking()
    };
    if !work {
        return;
    }
    // Needs a `JvmThread`: both halves take a real STW pause, and only a
    // registered thread can request one.
    if let Some((thread, _guard)) = jit_thread_mut() {
        thread.tlab.retire();
        crate::runtime::interpreter::g1_drive_concurrent_mark_pub(vm, thread);
    }
}

/// OPT-IN, and the reason is measured, not cautious.
///
/// Driving the lifecycle works exactly as designed. On
/// `org.h2.test.store.TestMVStoreTool` at `-Xmx256m`, one binary, driver on
/// against driver off:
///
///   on   16-47 mixed pauses, 680-1382 MB reclaimed by them
///   off  0-8 mixed pauses, `OutOfMemoryError: Java heap space` 3/3 in 46-102 s
///
/// Without it the collector never gets that memory back at all: this hook is
/// the only caller of either half that a compiled workload reaches.
///
/// # Why it is still not the default
///
/// Two separate defects were measured behind this switch. The first is FIXED
/// and is no longer a reason:
///
/// 1. CORRUPT CELLS -- NOT GONE, and the "gone" reading was a SAMPLING FLUKE.
///    Runs on 2026-09-05 reported >=32 `corrupt Value cell` errors each, and
///    the parallel-evacuator screen fix
///    (`g1-parallel-evacuator-had-none-of-the-serial-arms-header-screens`,
///    `CRATONVM_G1_PARALLEL_EVAC_SCREEN`) plainly relates to them: the holder
///    report this branch added prints `class_id` 163587104 = 0x09C02470, the
///    low half of the heap pointer 0x1f509c02470 beside it, which is that
///    page's fabrication pattern verbatim.
///
///    But three clean runs right after that merge did NOT mean the family was
///    closed. A four-rep interleaved A/B of the two tips on the same workload
///    (2026-09-06, `c67f6bbc3` against `8d83c7585`, driver on in both) gives
///    the SAME rate on each: 2 of the 3 runs that reached mixed GC report 32
///    corrupt cells, on BOTH tips. It is not a regression from the GC commits
///    in between, and it is not depth-dependent -- the old tip stayed clean
///    through 24 mixed pauses and corrupted at 4, the new one corrupted at 31
///    and stayed clean at 16. It is stochastic, pre-existing, and roughly 2/3.
///
/// 2. A SIGSEGV THAT SURVIVED IT -- still open, and the actual blocker. On the
///    fixed tip, 2 of 3 driver-on runs still die with SIGSEGV, and the count of
///    `IMPLAUSIBLE legacy header` refusals tracks it exactly: 13 and 8 in the
///    two that crashed, 0 in the one that did not. The screens now REFUSE the
///    bad candidates instead of fabricating headers from them, so the crash has
///    moved rather than gone. The refusals split
///    `evacuate-src` / `evacuate-dest` / `ref-slot-candidate`, i.e. the source
///    header, the copy, and a reference slot all reach the screen implausible.
///
/// The driver-off arm is NOT a clean negative for (2): it never segfaults, but
/// it also dies of OOM in 46-182 s and so never reaches the depth where the
/// fault happens.
///
/// One ablation worth not repeating: `CRATONVM_GC_RESERVE=0` changes nothing
/// here (32 corrupt cells in 3/3, SIGSEGV still 1/3). On the Generational
/// give-back page
/// (`a-compiled-frame-keeps-a-young-reference-the-collector-moved-20260906`)
/// that switch converts the SIGSEGV into a silent stale read, which is what
/// named the fault there as reporting rather than cause. The two have a very
/// similar signature and are NOT the same defect; this one is not a
/// decommit-window fault.
///
/// So this switch is how you REACH the remaining defect, not a tuning knob.
/// Default it on once a driver-on run survives without implausible-header
/// refusals.
///
/// # LANE W7-D (2026-09-21) — this is now an ACCELERATOR, not a prerequisite
///
/// The claim above that G1's lifecycle "is unreachable in BOTH directions"
/// was true of the build it was written against and is no longer. The actual
/// gap was a missing caller, not a missing flag: `maybe_gc_forced_at` — the
/// allocation-failure collection that every JIT allocation helper,
/// `alloc_object_shared`, `gc_alloc_array` and the TLAB refill wedge funnel
/// into — ran a full collection and never touched the cycle machinery.
/// `CRATONVM_G1_ALLOC_MARK_DRIVE` (default ON) puts the same finish-then-start
/// pair on that pause's epilogue, so a default build closes its cycles with no
/// JIT involvement at all.
///
/// What remains for this flag is LATENCY — closing a cycle sooner than the
/// next allocation-failure pause would. That is a performance argument with no
/// number behind it yet, weighed against the 2-of-3 SIGSEGV above and W6-B's
/// measured 19 862x declined-poll cost at allocation rate. It stays opt-in.
/// See `docs/feature-designs/concurrent-gc-maturation.md` §5.
fn g1_jit_mark_driver_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_JIT_MARK_DRIVER"))
}

#[inline]
// SAFETY: Must be called from a registered JVM thread context. Drives ZGC
// concurrent mark start and mutator TLAB retirement.
unsafe fn jit_maybe_start_zgc_concurrent_mark(vm: &SharedVm) {
    if !vm.mem.heap.zgc_should_start_concurrent_mark() {
        return;
    }
    // Needs a `JvmThread`: the mark-start pause is a real STW and only a
    // registered thread can request one.
    if let Some((thread, _guard)) = jit_thread_mut() {
        thread.tlab.retire();
        crate::runtime::interpreter::zgc_concurrent_mark_cycle_pub(vm, thread);
    }
}

/// gcf f1/c — the Generational inline concurrent start
/// (`CRATONVM_GEN_CONC_INLINE_START`), taken at a JIT allocation helper's
/// ENTRY. The latch is set by a direct old-gen allocation (a humongous array,
/// a young spill); its only other pickup is `maybe_gc`, which a compiled
/// allocation loop never reaches -- the gap `jit_maybe_start_zgc_concurrent_mark`
/// closes for ZGC. At the entry the helper has allocated nothing yet, so no
/// unrooted old block exists while the cycle marks and sweeps. One relaxed
/// load when nothing is latched (always, with the flag off, and on G1 / ZGC).
#[inline]
// SAFETY: Must be called from a registered JVM thread context. Drives Generational
// inline concurrent start pauses and mutator TLAB retirement.
unsafe fn jit_take_gen_inline_concurrent_start(vm: &SharedVm) {
    if !crate::runtime::interpreter::gen_inline_concurrent_start_pending(vm) {
        return;
    }
    // gcg g4/oo hunk 5.2 (b) 3, applied by g4/jm: under the owed-cycle switch
    // the compiled poll takes the latch (its second test, armed by
    // `direct_helper_table_for`), outside the allocation this helper serves.
    // x86-64 only, as the poll arm is: an aarch64 poll does not test the latch.
    if cfg!(target_arch = "x86_64")
        && crate::runtime::interpreter::gc_and_alloc::gen_conc_owed_poll_enabled()
    {
        return;
    }
    // Needs a `JvmThread`: the cycle's two pauses are real STW pauses.
    if let Some((thread, _guard)) = jit_thread_mut() {
        thread.tlab.retire();
        crate::runtime::interpreter::gen_inline_concurrent_start_pub(vm, thread);
    }
}

pub unsafe extern "C" fn jit_newarray(vm_ptr: i64, atype: i64, length: i64) -> i64 {
    let f = || jit_newarray_body(vm_ptr, atype, length);
    contain("jit_newarray", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_newarray`], run under its panic guard.
// SAFETY: Called under `contain` panic guard with valid JIT caller arguments.
// Dereferences `vm_ptr` after plausibility checks and allocates Java array.
unsafe fn jit_newarray_body(vm_ptr: i64, atype: i64, length: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): drain THIS thread's per-thread SATB
    // buffer before any path that may park at the GC barrier or trigger
    // collection. Mirrors interpreter::safepoint_check (line 899).
    jit_safepoint_flush_satb(vm_ptr);
    // ZGC: the concurrent-start check the interpreter does in `maybe_gc`.
    // See `jit_maybe_start_zgc_concurrent_mark` for why this call site exists.
    if vm_ptr != 0 {
        // SAFETY: same provenance as the `&*(vm_ptr as *const SharedVm)` below.
        jit_maybe_start_zgc_concurrent_mark(&*(vm_ptr as *const SharedVm));
        jit_drive_g1_concurrent_mark(&*(vm_ptr as *const SharedVm));
        jit_take_gen_inline_concurrent_start(&*(vm_ptr as *const SharedVm));
    }
    let elem_type = match atype as u8 {
        4 => ArrayElementType::Boolean,
        5 => ArrayElementType::Char,
        6 => ArrayElementType::Float,
        7 => ArrayElementType::Double,
        8 => ArrayElementType::Byte,
        9 => ArrayElementType::Short,
        10 => ArrayElementType::Int,
        11 => ArrayElementType::Long,
        _ => return 0,
    };
    // BUGFIX: The JIT may pass length as a NaN-boxed CompactValue raw bit pattern
    // (e.g. 0xFFFC_0000_0000_000B for int 11) when reading from operand-stack slots
    // that were populated via mechanisms that store CompactValue raw bits rather
    // than untagged primitive bits. Defensive: narrow `length` to the int payload,
    // then sign-extend to i64. JLS only allows `int` array lengths, so the upper
    // 32 bits of a valid length are always 0 (or all-1 for negative, which becomes
    // a NegativeArraySizeException — JIT codegen ensures bounds-checked path).
    let length = length as i32 as i64;
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if length < 0 {
        // Negative length → NegativeArraySizeException (JLS). Stash it in the
        // pending-exception channel and return the 0/null sentinel; the
        // `newarray` codegen's `emit_post_alloc_oom_check` bail then routes it
        // through the method's exception table (catchable), matching the
        // interpreter / HotSpot. The cast-to-usize below would otherwise turn
        // a negative length into a huge allocation request.
        return jit_negative_array_size(vm, length);
    }
    // HotSpot refuses a length past the VM limit before touching the heap, with
    // its own message and no collection -- as `gc_alloc_array`,
    // `multianewarray` and the native allocators already do. gen r4w4/oom,
    // item 3 of
    // `docs/feature-designs/jit-ir-optimizations.md`.
    if crate::runtime::interpreter::array_length_exceeds_vm_limit(length as usize) {
        return jit_alloc_oom(vm, crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT);
    }
    let heap = &vm.mem.heap;
    // Try allocation; if young gen exhausted, run GC and retry.
    //
    // Task #43 (HIGH soundness — deferred from #25/#26): route the
    // allocation-failure GC through the real STW handshake instead of
    // calling `heap.collect_garbage` directly. The direct call was a
    // long-standing FIXME because in a multi-threaded VM it bypasses
    // `gc_barrier.request_stw()` / `wait_for_all()` / `complete_gc()` and
    // every mutator's `safepoint_check` — meaning a JIT thread could
    // start rewriting object addresses while another thread is still
    // running, producing the classic mid-flight pointer-tearing UAF.
    // `maybe_gc_forced` (the interpreter's allocation-failure GC entry
    // point — `runtime/interpreter.rs:225`) is the model: it drains
    // per-thread SATB, requests STW through `gc_barrier`, waits for all
    // mutators to park, runs collection, signals completion, and updates
    // roots from the pointer map. Reusing it here keeps the JIT helper
    // on the orchestrated STW path with zero JIT-specific divergence.
    // W1-vm (MED, audit §3): the probe size MUST match the real allocation
    // size. `array_data_size` returns `None` when `HEADER_SIZE + length *
    // elem_size` overflows `usize` — an allocation that can never succeed.
    // The previous `.unwrap_or(0)` silently probed with size 0, which always
    // passes `try_alloc_young_probe` and then hands the impossible length to
    // the non-fallible `alloc_array` (abort / UB on the cast-to-usize). Treat
    // the overflow case as an immediate, catchable OutOfMemoryError instead —
    // the same outcome the interpreter reaches via `gc_alloc_array`'s
    // `try_alloc_array` returning `None`.
    let Ok(data_size) = cratonvm_types::array_data_size(length as usize, elem_type) else {
        return jit_newarray_oom(vm, length as usize);
    };
    let total_size = cratonvm_types::ARRAY_DATA_OFFSET + data_size;
    // JIT round 21 wave 1 (lane alloc): the per-VM engagement census
    // (`CRATONVM_DBG_JIT_METHOD_STATS`; `jit_newarray_calls=` /
    // `jit_newarray_small_calls=` on the compiled-new exit line).
    vm.jit
        .jit_alloc_class_cache
        .new_paths
        .note_newarray(total_size <= cratonvm_gc::tlab::tlab_max_alloc());
    // Fastest path: bump this thread's TLAB, taking no lock at all. Both tiers
    // now bump small primitive arrays inline (the single-pass
    // `emit_inline_tlab_newarray`, the optimizing tier's
    // `runtime_lowering::emit_inline_tlab_newarray_ir`), so a small array
    // reaches this helper on a TLAB miss or from a site that declined the
    // bump. (This comment used to say there was no inline array bump in
    // codegen and this helper was the ONLY path; that stopped being true when
    // the inline array bumps landed -- the optimizing tier's and both tiers'
    // ZGC arms in round 16 wave 7.) The arm still matters for
    // those entries: every probe/alloc pair below takes the global
    // `young_from` mutex twice, the second time across the zeroing, which made
    // JIT array allocation a process-wide serialisation point (`new long[16]`
    // throughput flat across threads while `new Object()` scaled,
    // `AllocScaleProbe`). The interpreter's `gc_alloc_array` has had this arm
    // since `7888b80b6`; this is the JIT's.
    if let Some((thread, _guard)) = jit_thread_mut() {
        if let Some(obj_ref) = crate::runtime::interpreter::tlab_alloc_array_guarded_refill(
            thread,
            vm,
            ClassId::new(0),
            elem_type,
            length as usize,
        ) {
            vm.jit.jit_alloc_class_cache.new_paths.note_newarray_tlab();
            return jit_newarray_finish(obj_ref, atype, length);
        }
    }
    // gci i20/ys: as the interpreter ladder (hunk YS-H1, opt-in
    // `CRATONVM_G1_HUMONGOUS_IHOP_START`, read inside the predicate).
    // gci i26/hu: the thread first, then the question -- the predicate records
    // a pending concurrent-start request when it answers yes, and a request
    // no pause answers here would label the next ordinary pause.
    if let crate::memory::vm_heap::VmHeap::G1(h) = heap {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if h.humongous_alloc_wants_concurrent_start(total_size) {
                thread.tlab.retire();
                crate::runtime::interpreter::maybe_gc_forced_pub_at(
                    vm,
                    thread,
                    crate::runtime::interpreter::gc_and_alloc::jit_array_gc_site(vm, total_size),
                );
            }
        }
    }
    // JIT round 21, lane alloc2: no TLAB array (refill refused, over the cap,
    // or no thread) -- the spill census.
    vm.jit.jit_alloc_class_cache.new_paths.note_newarray_spill();
    // Fast path: probe the young gen with the REAL allocation size and, on
    // success, allocate without the fallible retry dance. This preserves the
    // common-case cost of the original helper.
    //
    // gen r4w4/alloc4: lock-free bump-tail headroom first (see the same probe
    // in `jit_new_object`): every array above the TLAB cap reaches this line,
    // and the locked probe followed by the locked allocation was two
    // `young_from` acquisitions per array where one does.
    if heap.young_bump_headroom(total_size) || heap.try_alloc_young_probe(total_size).is_some() {
        if let Some(obj_ref) = heap.try_alloc_array(ClassId::new(0), elem_type, length as usize) {
            jit_note_external_alloc(vm, total_size);
            return jit_newarray_finish(obj_ref, atype, length);
        }
    }
    // G1's probe above is a collection request while evacuation to-space
    // still exists. An array larger than the TLAB cap reaches this path on
    // every allocation; spilling despite that request lets Eden consume the
    // reserve and turns the next pause into an evacuation failure. Collect
    // at this published JIT allocation boundary before retrying the array.
    if heap.is_g1() && heap.needs_gc_for_jit_allocation() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            // gci i12/ge: a humongous request's guard collects as `G1
            // Humongous Allocation` (`jit_array_headroom_site`).
            crate::runtime::interpreter::maybe_gc_forced_pub_at(
                vm,
                thread,
                crate::runtime::interpreter::gc_and_alloc::jit_array_headroom_site(
                    vm,
                    total_size,
                    "jit-newarray-headroom",
                ),
            );
        }
    }
    // zr z3/ja (opt-in `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM=1`): on ZGC the
    // occupancy verdict `jit_new_object` and `jit_anewarray_object` act on
    // (their "young has no headroom" arm: `!needs_gc()` on ZGC) is acted on
    // here too. Without it a compiled `newarray` whose refill the gate refused
    // spills each array through `try_alloc_array_full` below until the wedge
    // breaker's 16,384 consecutive gate failures (or the refill trigger's
    // 65,536 entries) force the collection -- the overshoot
    // `zgc-zr3ja-compiled-newarray-overshoots-the-occupancy-trigger-SUPERSEDED-20261019.md`
    // describes. Stamped like its siblings.
    //
    // zr z11/rv9 (zr4rv4 stale-stamp page, items 2-3): the siblings' gates in
    // their order -- the cheap heap probes first, the futile-young backoff,
    // and the uncached flag read LAST, so the default-off arm costs a spilled
    // `newarray` no environment probe unless the heap already said "no room"
    // -- and the outcome note after a cycle this thread ran. (On ZGC the
    // backoff and the note are no-ops today -- `futile_young_backoff_applies`
    // is Generational-only -- so the arm's real limiter stays what its
    // siblings' is: the stamp and the refusal door's futile rule.)
    if matches!(heap, crate::memory::vm_heap::VmHeap::Zgc(_))
        && !heap.young_bump_headroom(total_size)
        && heap.try_alloc_young_probe(total_size).is_none()
        && !crate::runtime::interpreter::futile_young_backoff_skips(vm)
        && crate::runtime::interpreter::gc_and_alloc::zgc_jit_newarray_headroom_enabled()
        // zr s1/ca: ZGC's own rate limit (the backoff above is
        // Generational-only), read after the opt-in flag so the default arm
        // pays nothing for it.
        && !crate::runtime::interpreter::gc_and_alloc::zgc_jit_newarray_headroom_rate_limited(vm)
    {
        let stamp = crate::runtime::interpreter::gc_and_alloc::zgc_gc_request_stamp(vm);
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            if crate::runtime::interpreter::gc_and_alloc::maybe_gc_forced_stamped_pub_at(
                vm,
                thread,
                "jit-newarray-zgc-headroom",
                stamp,
            ) {
                crate::runtime::interpreter::note_forced_young_cycle_outcome(vm, total_size);
            }
        }
    }
    // Second attempt, BEFORE forcing a GC — the step `gc_alloc_array` takes and
    // this helper did not (SB-LOADER-ZIPCONTENT, 2026-08-04). The interpreter
    // runs `try_alloc_array_full` here, so a young generation that cannot serve
    // the request spills into old gen and the mutator continues; the spill
    // itself arms `note_young_spill_pressure`, which schedules the collection at
    // the next native-call boundary where roots are pinned and remappable.
    //
    // Without it the JIT path forced a full STW GC for EVERY array the young
    // free list could not fit. On `ZipContentTests` that was ~750 forced
    // collections, one per 8 KB `byte[]`, each freeing ~10 KB — the difference
    // between "the class is slow" and "the class does not finish". The young
    // probe above is unchanged, so a healthy heap never reaches this line and
    // pays nothing.
    if let Some(obj_ref) = heap.try_alloc_array_full(ClassId::new(0), elem_type, length as usize) {
        jit_note_external_alloc(vm, total_size);
        return jit_newarray_finish(obj_ref, atype, length);
    }
    // gcf f4/o: the `[GC] oome_ladder_doors:` census (`jit_*`): the ladder
    // starts here; its allocation is counted at the tail.
    crate::runtime::interpreter::gc_and_alloc::note_ladder_door_entered(
        vm,
        crate::runtime::interpreter::gc_and_alloc::LadderDoor::Jit,
    );
    jit_oome_ladder_begin(vm, "door=jit-newarray");
    // Slow path: young gen full (or the probe-then-alloc race lost the slot).
    // Mirror the interpreter's `gc_alloc_array` (runtime/interpreter.rs:840):
    // retire the TLAB, run an orchestrated STW GC, then retry the fallible
    // `try_alloc_array`. CRIT (jit/gc audit, 2026-05): MUST retire the
    // calling thread's TLAB before kicking off GC. The retire installs a
    // synthetic `int[]` filler at the cursor so the heap walker can stride
    // over the unused TLAB tail in O(1); without it, the walker mis-decodes
    // the tail's zeroed bytes (or a half-init JIT object) and aborts with
    // "implausible object size" / corrupts old gen when promote-on-pressure
    // copies stale pointers. Mirrors `alloc_object_shared` in the interpreter
    // (runtime/interpreter.rs line ~782).
    if let Some((thread, _guard)) = jit_thread_mut() {
        thread.tlab.retire();
        // Route allocation-failure GC through the interpreter's orchestrated
        // STW path (`maybe_gc_forced` -> `gc_barrier.request_stw()` +
        // `wait_for_all()`), so other mutator threads are parked before the
        // moving collector rewrites object addresses. (Resolves the prior
        // FIXME that called `heap.collect_garbage` with an unchecked
        // StopTheWorldToken.)
        // gci i12/ge: a G1 humongous request's collection is `G1 Humongous
        // Allocation`, as the interpreter's array ladder's
        // (`jit_array_gc_site`; `jit-helpers` otherwise).
        // zt t20/fl1: the zt t4/ca2 stamped failure door
        // (`CRATONVM_ZGC_JIT_FAILURE_DOOR_STAMPED`) is retired: it never
        // dropped a request on any row (`failure_door_dropped=0`).
        crate::runtime::interpreter::maybe_gc_forced_pub_at(
            vm,
            thread,
            crate::runtime::interpreter::gc_and_alloc::jit_array_gc_site(vm, total_size),
        );
    }
    // GC-overhead limit: if forced GCs keep freeing almost nothing, the heap is
    // full of live objects — surface OOM now instead of limping on slivers
    // (death-spiral). Mirrors the interpreter's alloc paths.
    // The soft-reference rung first: a collection that cleared them earns
    // the ordinary retry below, unless it left the streak latched
    // (`jit_latched_overhead_limit_throws`, gen r5w2/oomjit6).
    // gcf f2/o: `graced` = this attempt runs on the latched exit's grace; its
    // success is counted (`ladder_latched_grace_ok=`) where it allocates.
    jit_oome_ladder_step(vm, "forced-collection");
    let mut graced = false;
    if crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
        && jit_latched_overhead_limit_throws(vm)
    {
        graced = jit_latched_grace(vm);
        if !graced {
            jit_oome_ladder_step(vm, "jit-newarray latched-exit throws");
            return jit_newarray_oom(vm, length as usize);
        }
    }
    // Retry after GC. On a second failure, try a G1 last-ditch full mark
    // cycle (dead humongous spans need cleanup, not a young pause) and retry
    // once more; then the heap is genuinely exhausted — surface a catchable
    // `java/lang/OutOfMemoryError` exactly as the interpreter's
    // `gc_alloc_array` does, instead of the old non-fallible `alloc_array`
    // (which would abort the process on a real OOM).
    //
    // SB-LOADER-ZIPCONTENT (2026-08-04): these two retries MUST use the
    // old-gen-spilling `try_alloc_array_full`, not the young-only
    // `try_alloc_array`. `gc_alloc_array` states the reason for the
    // interpreter's identical arm — "once a non-moving JIT-safe sweep has left
    // the young generation fragmented it can spill the request into old space
    // ... retrying young-only here used to report OOM for a tiny array while
    // most of the heap was available as old-generation headroom" — and this
    // helper was the one primitive-array path that never got the same
    // treatment (`jit_anewarray_object` already calls `try_alloc_array_full`
    // and `jit_new_object` already calls `try_alloc_object_full`; see the
    // `jit_alloc_oom` doc comment, which lists the asymmetry as fact).
    //
    // That gap is exactly how `ZipContentTests.nestedZip64CanBeRead` died:
    // moving-young had fallen back to the non-moving sweep
    // (`reason=unregistered-jit-frame-on-stack`), the young free list could no
    // longer serve the compiled `byte[8192]` that assertj's `assertHasContent`
    // allocates once per ZIP entry, and this arm reported
    // `OutOfMemoryError: Java heap space (alloc_array length 8192)` with a
    // 134 MB live set, a 1.5 GiB heap, and **1042 MB of old-generation
    // headroom** (`CRATONVM_DBG=gc-overhead`: `old_gen_wedged=false`, so the
    // overhead limit correctly never fired — the heap was not full, the
    // allocator just refused to look at the free gigabyte). The same class
    // passed 29/29 under `--nojit`, where every array runs `gc_alloc_array`.
    let obj_ref = match heap.try_alloc_array_full(ClassId::new(0), elem_type, length as usize) {
        Some(o) => o,
        None => {
            let between = jit_g1_last_ditch_attempting(vm, || {
                heap.try_alloc_array_full(ClassId::new(0), elem_type, length as usize)
            });
            match between {
                Err(()) => return jit_newarray_oom(vm, length as usize),
                Ok(Some(o)) => o,
                Ok(None) => {
                    match heap.try_alloc_array_full(ClassId::new(0), elem_type, length as usize) {
                        Some(o) => o,
                        None => return jit_newarray_oom(vm, length as usize),
                    }
                }
            }
        }
    };
    if graced {
        crate::runtime::interpreter::gc_and_alloc::note_latched_grace_outcome(vm, true);
    }
    crate::runtime::interpreter::gc_and_alloc::note_ladder_door_allocated(
        vm,
        crate::runtime::interpreter::gc_and_alloc::LadderDoor::Jit,
    );
    jit_note_external_alloc(vm, total_size);
    jit_newarray_finish(obj_ref, atype, length)
}

/// Charge `bytes` to the calling thread's allocation counter — the one behind
/// `com.sun.management.ThreadMXBean.getThreadAllocatedBytes`.
///
/// Only for allocations that **bypassed the TLAB**. TLAB-served allocations,
/// including the ones compiled code performs with its own inline bump, are
/// already accounted for by [`cratonvm_gc::Tlab::thread_allocated_bytes`],
/// which reads the live cursor; charging them here as well would double-count.
///
/// The JIT keeps its own copies of the interpreter's slow allocation paths
/// (`jit_newarray`, `jit_new_object`, `jit_anewarray_object`), so the
/// accounting `alloc_object_shared` / `gc_alloc_array` do has to be repeated
/// here — otherwise a compiled thread's counter would silently omit every
/// humongous object it allocated, which is the direction that reads as good
/// news to a caller asserting "we allocated less than N".
///
/// Best-effort: no `JvmThread` (a foreign/unattached caller) means no counter
/// to charge, and monitoring must never be the thing that fails an allocation.
///
/// gen r4w4/alloc4: also charges the per-VM total (`bytes_allocated_total`),
/// exactly as `alloc_object_shared` does for the interpreter's slow objects —
/// the per-VM counter never saw a JIT allocation outside a TLAB. (The TLAB
/// half, the JIT's inline bump, is topped up at retire by
/// `Tlab::top_up_vm_allocation_total`.)
///
/// # Safety
/// Same contract as [`jit_thread_mut`]: call only from a JIT helper running on
/// the thread that installed `JIT_THREAD`, with no other `&mut JvmThread` (and
/// no live `JitThreadGuard`) outstanding. Every call site below sits after the
/// allocation's own guards have been dropped.
#[inline]
unsafe fn jit_note_external_alloc(vm: &SharedVm, bytes: usize) {
    vm.mem
        .bytes_allocated_total
        .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    if let Some((thread, _guard)) = jit_thread_mut() {
        thread.tlab.note_external_allocation(bytes);
    }
}

/// W1-vm: surface an allocation failure from `jit_newarray` as a catchable
/// `java/lang/OutOfMemoryError`, mirroring the interpreter's `gc_alloc_array`
/// (runtime/interpreter.rs:853) OOM arm.
///
/// The `newarray` codegen site (`emit_post_alloc_oom_check` in `jit/src/x64.rs`)
/// null-checks RAX and, on the `0`/null OOM sentinel, bails to the shared
/// exception stub (returning the `i64::MIN` deopt sentinel + running the
/// epilogue). We stash the throwable in `JIT_PENDING_EXCEPTION` (the same
/// channel the void-return store helpers use for null-array NPEs) and the
/// interpreter's post-JIT general-exception drain
/// (`take_jit_pending_exception()` on the dispatch-aware return path) routes the
/// OOME through the JIT'd method's own exception table, giving a JIT'd
/// `newarray` identical catchable-OOM semantics to the interpreter. The bail
/// also forces the method `has_dispatch` (`emitted_alloc_oom_check` in `x64.rs`)
/// so that drain runs AND so `jit_thread_mut()` below is non-null.
///
/// If the OOME object itself cannot be constructed (e.g. the heap is too
/// exhausted to even allocate the throwable), we fall back to leaving the
/// flag unset and returning `0` — the legacy behaviour — so this change is
/// purely additive and never makes a previously-handled case worse.
#[cold]
fn jit_newarray_oom(vm: &SharedVm, length: usize) -> i64 {
    jit_alloc_oom(
        vm,
        &format!("Java heap space (alloc_array length {})", length),
    )
}

/// The last rung of the compiled `ldc` doors (a class mirror, or a pooled
/// String constant) after their one forced collection has not made room: the
/// same full reclaim [`jit_newarray`]'s ladder runs
/// ([`jit_g1_last_ditch_full_cycle`]), then one more attempt.
///
/// These doors used to throw after a single young pause. On G1 a young pause
/// never reclaims dead Old regions, so a program that caught an
/// `OutOfMemoryError`, dropped its data and then printed got a second,
/// uncaught error from the `ldc` of its message string:
/// `FullHeapResolveProbe -XX:+UseG1GC -Xmx64m` died on `ldc "round "` right
/// after `junk = null`. The interpreter's `ldc` goes through
/// `collect_and_retry`, which already has this rung.
#[cold]
fn jit_ldc_last_ditch<T>(vm: &SharedVm, attempt: impl FnOnce() -> Option<T>) -> Option<T> {
    jit_oome_ladder_begin(vm, "door=jit-ldc (after its forced collection)");
    if jit_g1_last_ditch_full_cycle(vm) {
        attempt()
    } else {
        None
    }
}

/// Last-ditch reclamation on allocation failure — see the interpreter's
/// `last_ditch_reclaim`: a completed G1 mark cycle for dead Old/humongous
/// spans, then a collection with every SoftReference condemned, which
/// `java.lang.ref` requires before the VM may throw `OutOfMemoryError`.
/// Returns `true` when it was attempted (caller should retry the allocation
/// once before surfacing OOM).
///
/// The G1 gate this used to open with is gone: it belonged to step 1 alone,
/// and applying it to the whole function made the soft-reference rule
/// G1-only — so a ZGC or Generational program OOME'd with a heap of softly
/// reachable garbage, which is exactly the failure the interpreter-side ladder
/// was fixed for. `last_ditch_reclaim` re-applies it to the step that owns it.
/// JIT round 22 wave 12, lane memory: `CRATONVM_DBG_OOME_LADDER`'s door
/// record for a compiled allocation helper (`gc_and_alloc::oome_ladder_begin`).
/// One cached load when the diagnostic is off. Call only where the helpers
/// already take `jit_thread_mut` (no `JitThreadGuard` of this thread live).
#[cold]
fn jit_oome_ladder_begin(vm: &SharedVm, door: &str) {
    if !crate::runtime::interpreter::gc_and_alloc::oome_ladder_dbg(vm) {
        return;
    }
    // SAFETY: the `jit_g1_last_ditch_full_cycle` contract -- a JIT allocation
    // slow-path helper on the mutator thread that entered compiled code, with
    // no other `JitThreadGuard` of it live; the guard drops at once.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        crate::runtime::interpreter::gc_and_alloc::oome_ladder_begin(vm, thread, door);
    }
}

/// [`jit_oome_ladder_begin`]'s step record (`gc_and_alloc::oome_ladder_step`).
#[cold]
fn jit_oome_ladder_step(vm: &SharedVm, step: &str) {
    if !crate::runtime::interpreter::gc_and_alloc::oome_ladder_dbg(vm) {
        return;
    }
    // SAFETY: as in `jit_oome_ladder_begin`.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        crate::runtime::interpreter::gc_and_alloc::oome_ladder_step(vm, thread, step);
    }
}

/// The JIT helpers' last-ditch rung with the helper's own `attempt` between
/// G1's two full collections (`CRATONVM_G1_JIT_LADDER_ATTEMPTS`, default ON;
/// the near-full proxy row ran both on every refusal, and the second freed
/// nothing). `Err(())` = no attached thread (the caller throws); `Ok(Some)` =
/// the attempt between them allocated; `Ok(None)` = the whole rung ran, and
/// the caller attempts once more. Off G1, or `=0`:
/// [`jit_g1_last_ditch_full_cycle`], then `Ok(None)`. `attempt` must hold no
/// object reference across a collection (each helper's attempt re-reads only
/// class ids and lengths).
#[cold]
fn jit_g1_last_ditch_attempting<T>(
    vm: &SharedVm,
    mut attempt: impl FnMut() -> Option<T>,
) -> Result<Option<T>, ()> {
    if !vm.mem.heap.is_g1() || !crate::runtime::interpreter::gc_and_alloc::g1_jit_ladder_attempts_enabled()
    {
        return if jit_g1_last_ditch_full_cycle(vm) {
            Ok(None)
        } else {
            Err(())
        };
    }
    // SAFETY: as in `jit_g1_last_ditch_full_cycle`.
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return Err(());
    };
    let mut hook = |_: &SharedVm, _: &mut JvmThread| attempt();
    Ok(crate::runtime::interpreter::gc_and_alloc::g1_last_ditch_split(
        vm, thread, &mut hook, false,
    ))
}

#[cold]
fn jit_g1_last_ditch_full_cycle(vm: &SharedVm) -> bool {
    // SAFETY: called only from the JIT allocation slow-path helpers, on a
    // mutator thread that entered compiled code through the JIT entry
    // trampoline — the same contract as the surrounding `jit_thread_mut`
    // calls in those helpers.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        crate::runtime::interpreter::last_ditch_reclaim(vm, thread);
        // gcd d4/j (cross-lane, applied by d4/o): the ladder's second major
        // before an OOME (every caller retries once after this and then
        // throws).
        let _ = crate::runtime::interpreter::gc_and_alloc::second_major_before_oome(
            vm,
            thread,
            "jit-oome-second-major",
        );
        true
    } else {
        false
    }
}

/// The soft-reference rung of the GC-overhead early exit: every
/// SoftReference is cleared before a JIT allocation helper may throw
/// `OutOfMemoryError` for a latched overhead limit, exactly as the
/// interpreter's `collect_and_retry_with_thread` does
/// (`java.lang.ref.SoftReference` guarantees it before ANY OOME). Returns
/// whether a collection ran. No collection (the application holds no live
/// soft references, or no attached thread) keeps the immediate OOME; a
/// collection earns the caller one retry through its ordinary path only if it
/// also reset the streak -- the decision is
/// [`jit_latched_overhead_limit_throws`]'s, the helpers' only caller of this
/// (gen r5w2/oomjit6; it used to be "whenever a collection ran", which let a
/// latched heap limp on one object per major collection).
///
/// Without it a latched overhead limit threw from a COMPILED allocation with no
/// soft-clearing collection at all. It is one of the two halves of
/// `tools/probes/interp/L5/SoftRefBeforeOome` (`cleared false` with the JIT
/// on, `true` with `--nojit`): the other was that the collection it runs could
/// not clear a referent the loop's own `soft.get()` had left in
/// `native_pending_return` (`drain_native_return_at_compiled_helper_entry`).
/// See
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// gci i2/oo: since SBR4 (2026-10-05) `SoftReference.get` runs as bytecode, so
/// no native return holds the referent any more; the probe's `cleared false`
/// came back with a different holder (a compiled-frame word, by reading:
/// `docs/feature-designs/jit-ir-optimizations.md`).
/// This rung still runs on every latched exit.
#[cold]
fn jit_overhead_limit_clear_soft_refs(vm: &SharedVm) -> bool {
    // SAFETY: called only from the JIT allocation slow-path helpers, on the
    // mutator thread that entered compiled code (the `jit_thread_mut`
    // contract), with no other `JitThreadGuard` of this thread live.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        crate::runtime::interpreter::last_ditch_clear_soft_refs(vm, thread)
    } else {
        false
    }
}

/// The JIT allocation helpers' answer to a LATCHED GC-overhead limit
/// (`gc_overhead_limit_exceeded` already true): `true` = throw
/// `OutOfMemoryError` now, `false` = the soft-reference rung earned one
/// ordinary retry.
///
/// gen r5w2/oomjit6, item 2 of
/// `docs/feature-designs/jit-ir-optimizations.md`
/// (`chain-hot` hangs past 600 s under `CRATONVM_DBG=gc-stress=250000`). The
/// rung used to earn the retry whenever it RAN a collection, and it runs one
/// whenever any SoftReference is still active -- which a JDK cache whose
/// referent is strongly reachable keeps true for the life of the VM. With the
/// streak latched and the old generation wedged but not yet empty (under 2 %
/// headroom is still megabytes), every helper entry then paid the forced
/// young cycle plus a soft-clearing MAJOR and allocated ONE object out of that
/// headroom: tens of thousands of majors for a `Node` chain, never an
/// exception. Under `gc-stress` the TLAB carve cap sends every `new` to the
/// helper, which is why only that setting hung.
///
/// The interpreter's ladder (`collect_and_retry_with_thread`) already decides
/// the other way: a collection this thread ran, judged by
/// `note_gc_productivity`, that leaves the streak latched IS the error. This
/// applies the same rule after the rung, on the same gate (Generational, and
/// `CRATONVM_GC_OVERHEAD_PROGRESS` on, its kill switch restoring the retry).
/// A rung that freed a soft cache is productive, resets the streak, and still
/// earns the retry, so `SoftRefBeforeOome` keeps its `cleared true`. G1 and
/// ZGC are unchanged.
///
/// gcd d1/b (2026-09-27): the rung that ran NO collection (the program holds
/// no live soft reference -- the common case) used to throw at once, on a
/// verdict the forced YOUNG cycles alone had built. On Generational those do
/// not reach the old generation below its 75 % floor, which is exactly where a
/// program's dropped data sits after an `OutOfMemoryError`; so the first
/// compiled allocation after `made = null` threw with most of the heap
/// garbage (`GenR4W4NativeStringOomProbe`'s `println`, the `println` after
/// `GenR4W6JitOomRootProbe`'s `oome-osr-loop`, `NativeGrowthReclaimProbe`'s
/// compiled `fillAndDrop`). The interpreter's ladder runs a major first and
/// throws only when a major THIS thread ran leaves the streak latched
/// (`collect_and_retry_with_thread`, gen r4w4 + r4w5/thrash5 + r5w5/sizer9);
/// this is the same rule ([`jit_overhead_limit_major`],
/// [`jit_overhead_limit_verdict`]), on the same gate.
#[cold]
fn jit_latched_overhead_limit_throws(vm: &SharedVm) -> bool {
    // gcd d10/o, then the gcg g3o residual (2026-10-04): on G1 no collection
    // so far reached a dead Old region (the forced collection is a young
    // pause), and G1's latched exit runs (`g1_latched_overhead_exit`: the
    // marking cycle, judged, the recovery pause, the soft rung; then the
    // attempt only after a productive cycle or on the bounded latched grace)
    // -- the exit the interpreter, native-factory and exception doors take.
    // `false` = the helper's ordinary attempt; `true` = the error. The grace
    // is decided there, so the callers' own grace (`jit_latched_grace`)
    // stands down on G1. Until then a soft rung that collected skipped the
    // cycle, and four of the five callers attempted after a futile cycle,
    // with no bound: a wedged G1 heap ran one full marking cycle per
    // compiled allocation.
    if vm.mem.heap.is_g1() {
        let before_live = vm.mem.heap.live_bytes_estimate();
        return !jit_g1_latched_overhead_exit(vm, before_live);
    }
    let soft_collected = jit_overhead_limit_clear_soft_refs(vm);
    // zr z2/cp: on ZGC the helper's forced cycle was whole-heap, so the streak
    // re-read after the soft rung is the verdict, as at the interpreter's and
    // the native door's exit (`zgc_overhead_judged`).
    if crate::runtime::interpreter::gc_and_alloc::zgc_overhead_judged(vm) {
        return crate::runtime::interpreter::gc_overhead_limit_exceeded(vm);
    }
    let judge = vm.mem.heap.is_generational() && cratonvm_types::flags().gc.gc_overhead_progress;
    // gcd d5/u: also when the soft-reference rung DID collect but left the
    // streak latched -- the interpreter ladder's rule
    // (`collect_and_retry_with_thread`: `!collected || (judge_major &&
    // gc_overhead_limit_exceeded(shared))`). The rung's collection is ONE
    // major, and one major is no verdict on Generational (gcd d4/j,
    // `majors_to_decide_oome`: dead old data a young survivor keeps needs a
    // second mark); so a compiled allocation threw after one major wherever
    // the program still held a live `SoftReference` (every JDK boot does),
    // where the interpreter ran up to two more. The verdict below re-reads
    // the streak after it, so a productive major earns the retry and a futile
    // one throws, as in the interpreter.
    let major_ran = judge
        && (!soft_collected || crate::runtime::interpreter::gc_overhead_limit_exceeded(vm))
        && jit_overhead_limit_major(vm);
    let latched_now = crate::runtime::interpreter::gc_overhead_limit_exceeded(vm);
    // gcg g3/o (opt-in): the interpreter's and the native door's decision
    // (`gcd-d10o-proposal-oome-ladder-census-on-every-collector` step 1).
    if crate::runtime::interpreter::gc_and_alloc::jit_ladder_unified_enabled() {
        return crate::runtime::interpreter::gc_and_alloc::jit_overhead_limit_throws_unified(
            judge,
            soft_collected,
            major_ran,
            latched_now,
        );
    }
    jit_overhead_limit_verdict(judge, soft_collected, major_ran, latched_now)
}

/// gcd d1/b: the Generational major of [`jit_latched_overhead_limit_throws`]
/// -- the interpreter ladder's `overhead-limit-major` for a compiled
/// allocation. Returns whether THIS thread ran it, i.e. whether
/// `note_gc_productivity` judged a full collection whose verdict the caller
/// may honour.
///
/// The major request is thread-local and is consumed by the Phase-5 check of
/// the next collection this thread runs; still pending afterwards means the
/// thread lost the STW race and only took part in another thread's pause.
/// The request is then withdrawn, as `last_ditch_clear_soft_refs` withdraws
/// its own, so it cannot turn an unrelated later collection into a major.
///
/// Called only from the allocation helpers' slow paths, at a point where each
/// of them already runs `maybe_gc_forced_pub_at` (no object reference is held
/// across it) and with no `JitThreadGuard` of this thread live.
#[cold]
fn jit_overhead_limit_major(vm: &SharedVm) -> bool {
    // SAFETY: called only from the JIT allocation slow-path helpers, on the
    // mutator thread that entered compiled code (the `jit_thread_mut`
    // contract), with no other `JitThreadGuard` of this thread live -- the
    // same contract as `jit_overhead_limit_clear_soft_refs`, whose guard has
    // dropped before this runs.
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return false;
    };
    // gcd d4/j (cross-lane, applied by d4/o): the interpreter ladder's rule --
    // a second major when the first freed under 2 % of the heap (young->old
    // nepotism); withdraws a request a lost race left.
    crate::runtime::interpreter::gc_and_alloc::majors_to_decide_oome(
        vm,
        thread,
        "jit-overhead-limit-major",
    )
}

/// gcd d10/o, gcg g3o residual: the G1 arm of
/// [`jit_latched_overhead_limit_throws`] -- G1's latched exit
/// (`g1_latched_overhead_exit`); `true` = make the one attempt. `false` with
/// no attached thread (no cycle could run).
#[cold]
fn jit_g1_latched_overhead_exit(vm: &SharedVm, before_live: usize) -> bool {
    // SAFETY: the `jit_overhead_limit_major` contract -- called only from the
    // JIT allocation slow-path helpers, on the mutator thread that entered
    // compiled code, with no other `JitThreadGuard` of it live.
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return false;
    };
    crate::runtime::interpreter::gc_and_alloc::g1_latched_overhead_exit(vm, thread, before_live)
}

/// gcg g3o residual: the latched exit's grace on the JIT helpers -- none on
/// G1, whose exit (`g1_latched_overhead_exit`) has already decided its own;
/// `latched_overhead_grace` elsewhere.
#[inline]
fn jit_latched_grace(vm: &SharedVm) -> bool {
    !vm.mem.heap.is_g1() && crate::runtime::interpreter::gc_and_alloc::latched_overhead_grace(vm)
}

/// gcd d1/b: the pure decision of [`jit_latched_overhead_limit_throws`];
/// `true` = throw `OutOfMemoryError` now.
///
/// * `judge` -- Generational with `CRATONVM_GC_OVERHEAD_PROGRESS` on. Off, the
///   historical answer: throw unless the soft-reference rung collected.
/// * `soft_collected` -- the rung ran a collection on this thread (a major on
///   Generational). Judged as before: the streak decides, read AFTER the
///   overhead-limit major that follows the rung when the rung left it
///   latched (gcd d5/u; `latched_now` is read after both).
/// * `major_ran` -- the rung did not collect, and this thread then ran the
///   overhead-limit major. Its verdict decides. When it did NOT run (the STW
///   race was lost), no full collection judged the heap: no throw here -- the
///   helper's ordinary retry and last-ditch rung decide, as the interpreter's
///   `OverheadLimitExit::AttemptThenLastDitch` does.
fn jit_overhead_limit_verdict(
    judge: bool,
    soft_collected: bool,
    major_ran: bool,
    latched_now: bool,
) -> bool {
    if !judge {
        return !soft_collected;
    }
    if soft_collected {
        return latched_now;
    }
    major_ran && latched_now
}

#[cfg(test)]
mod gcd_d2j_jit_alloc_oom_tests {
    //! gcd d2/j: `jit_alloc_oom` with no JIT thread is counted per VM.
    use super::*;
    use crate::config::VmConfig;

    #[test]
    fn a_jit_oom_without_a_jit_thread_is_counted_per_vm() {
        // SAFETY: only asks whether this test thread installed a JIT thread
        // (it did not); the guard, if any, drops at once.
        if unsafe { jit_thread_mut() }.is_some() {
            return;
        }
        let shared = SharedVm::new(VmConfig::default());
        let count = || {
            shared
                .mem
                .alloc_ladder
                .jit_alloc_oom_without_thread
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        assert_eq!(count(), 0);
        assert_eq!(
            jit_alloc_oom(&shared, "Java heap space (new_object class_id 1 fields 2)"),
            0
        );
        assert_eq!(jit_alloc_oom(&shared, "Java heap space"), 0);
        assert_eq!(count(), 2);
        let other = SharedVm::new(VmConfig::default());
        assert_eq!(
            other
                .mem
                .alloc_ladder
                .jit_alloc_oom_without_thread
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}

#[cfg(test)]
mod gcd_d1b_overhead_verdict_tests {
    //! gcd d1/b: the latched overhead limit of a compiled allocation throws
    //! only after a full collection this thread ran.
    use super::jit_overhead_limit_verdict as verdict;

    /// Off the gate (G1, ZGC, `CRATONVM_GC_OVERHEAD_PROGRESS=0`): the
    /// historical answer, whatever the streak says.
    #[test]
    fn the_unjudged_answer_is_unchanged() {
        for latched in [false, true] {
            assert!(verdict(false, false, false, latched));
            assert!(!verdict(false, true, false, latched));
        }
    }

    /// A soft-reference collection is judged as before (gen r5w2/oomjit6).
    #[test]
    fn a_soft_collection_still_decides_by_the_streak() {
        assert!(verdict(true, true, false, true));
        assert!(!verdict(true, true, false, false));
    }

    /// The fix: no soft collection used to be an immediate throw. Now the
    /// major decides -- and a major this thread did not run decides nothing.
    #[test]
    fn no_soft_collection_needs_a_major_before_the_error() {
        assert!(
            verdict(true, false, true, true),
            "a wedged heap still throws"
        );
        assert!(
            !verdict(true, false, true, false),
            "a productive major earns the retry"
        );
        assert!(
            !verdict(true, false, false, true),
            "a lost STW race is no verdict"
        );
    }
}

/// Shared OOM signal for the fallible JIT allocation helpers — `jit_newarray`,
/// `jit_anewarray_object` (both via `try_alloc_array_full`), and
/// `jit_new_object` (via `try_alloc_object_full`). On heap exhaustion the
/// helper stashes a `java/lang/OutOfMemoryError` in `JIT_PENDING_EXCEPTION` and
/// returns the `0`/null sentinel; the alloc codegen site null-checks the result
/// and bails to the shared exception stub (`emit_post_alloc_oom_check` in
/// `x64.rs`, which also forces the method `has_dispatch`), after which the
/// interpreter's general-exception drain on the dispatch-aware return path routes
/// the OOME through the method's exception table — giving a JIT'd allocation the
/// same catchable-OOM semantics as the interpreter's `gc_alloc_array` /
/// `gc_alloc_object`, instead of the old SIGSEGV (null deref of the result) or
/// hard `alloc_young` abort. The object/array helpers go through the
/// `*_full` fallible paths so the old-generation spill of the non-fallible
/// `alloc_object` / `alloc_array` is preserved before OOM is reported.
///
/// If a fresh OOME object cannot be constructed (heap too exhausted to even
/// allocate the throwable / its message), fall back to the pre-allocated
/// singleton `OutOfMemoryError` (`SharedVm::singleton_oom`) so the OOM stays
/// catchable on a 100%-full heap instead of the materialization hard-aborting.
/// If the singleton is also absent (pre-allocation hasn't run yet), the flag is
/// left unset and `0` returned — the legacy behaviour, never worse.
#[cold]
fn jit_alloc_oom(vm: &SharedVm, msg: &str) -> i64 {
    // SAFETY: called only from a JIT alloc helper on the thread that installed the
    // JIT thread pointer; no other `&mut JvmThread` borrow is live here.
    // SAFETY (both acquisitions): see the comment above — this runs on the JIT
    // thread and no other `&mut JvmThread` borrow is live. The two are separate
    // because the first `_guard` must drop before the singleton fallback can
    // re-borrow.
    //
    // gen r4w4/oom (2026-09-24), items 1 and 2 of
    // `docs/feature-designs/jit-ir-optimizations.md`:
    // * the NO-COLLECT constructor. Every caller reaches here after its own
    //   ladder gave up (or for a refusal that must not collect, the VM limit);
    //   the collecting constructor ran the whole ladder a second time to
    //   allocate the error object, which every other tier stopped doing in
    //   gc-common w4-c (`ExceptionAlloc::NoCollect`);
    // * HotSpot's text. The site detail (`Java heap space (alloc_array length
    //   8192)`) is diagnostics only; every other tier strips it at
    //   `RuntimeError::as_java_throwable` through the same function.
    // * `-XX:+HeapDumpOnOutOfMemoryError`: the interpreter ladders dump before
    //   they throw, and this door did not, so a compiled program's first OOME
    //   wrote no dump. Once per VM and a no-op without the flag. The dump is a
    //   NON-moving pause, requested from the same helper frames that already
    //   request collection pauses (`maybe_gc_forced_pub_at`), so it adds no
    //   new hazard for the compiled caller.
    // * gci i10/al (`gci-i10al-oome-dump-gc-safe-slice-re-derives-mut-jvmthread-FIXED-20261016`):
    //   the dump's GC-safe retry (i9/hj) opens blocked regions on this
    //   thread. It gets the guard's own `&mut` (reborrowed here), and no longer
    //   re-derives a second one through `jni::current_jvm_thread_of` while the
    //   guard is live.
    let mut had_thread = false;
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        had_thread = true;
        crate::runtime::interpreter::maybe_dump_heap_on_oom_for(
            vm,
            thread,
            cratonvm_types::error::hotspot_oome_detail(msg),
        );
        // gen r4w5/oomjit5: `CRATONVM_DBG=oldmark-root-census` — the
        // compiled-code OOME is the event the census exists for; arm the VM
        // labeller and say what this thread holds outside its frames now.
        if oldmark_census_on() {
            oldmark_census_thread_line(thread, "compiled-code OOME");
        }
        // gen r5w1/oom5: `create_vm_oome_object` is the no-collect door,
        // plus one collecting retry for the VM-limit refusal only (it ran no
        // ladder, so on a heap full of garbage its message used to be lost to
        // the `Java heap space` singleton). Both callers that pass that
        // message (`jit_newarray`, `jit_anewarray_object`) collect on their
        // own ordinary path, so a collection here is one they already allow.
        if let Ok(exc) = crate::runtime::exceptions::create_vm_oome_object(
            vm,
            thread,
            "java/lang/OutOfMemoryError",
            Some(cratonvm_types::error::hotspot_oome_detail(msg)),
        ) {
            set_jit_pending_exception(thread, exc);
            return 0;
        }
    }
    // Fresh creation failed (or no JIT thread) — use the pre-allocated
    // singleton. This needs the thread too now that the stash is a GC-rooted
    // `JvmThread` field: the singleton OOME is exactly the reference most
    // likely to be live across a collection (we are here *because* the heap is
    // full), so parking it anywhere the collector cannot see would be the
    // worst possible place for it.
    //
    // gcd d2/j: the preallocated error of this message when there is one
    // (`CRATONVM_GC_PREALLOCATED_OOME_KINDS`: the VM-limit refusal keeps its
    // `Requested array size exceeds VM limit`), else the singleton as before.
    let preallocated = crate::runtime::exceptions::preallocated_oome_for(
        vm,
        cratonvm_types::error::hotspot_oome_detail(msg),
    );
    if let Some(oom) = preallocated {
        if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
            set_jit_pending_exception(thread, oom);
        }
    }
    if !had_thread {
        jit_alloc_oom_without_thread(vm, msg);
    }
    0
}

/// gcd d2/j, item 2 of `gengc-r5w1-oom5-oom-path-review-residuals-FIXED-20260928`:
/// [`jit_alloc_oom`] reached with no JIT thread, so neither a fresh
/// `OutOfMemoryError` nor a preallocated one was published and the compiled
/// caller takes its exception exit with nothing pending. Every caller runs on
/// the JIT thread by construction, so this is the fail-closed witness a
/// `debug_assert!` cannot be (unit tests drive the helpers without a JIT
/// thread): a per-VM count (`HeapRealm::alloc_ladder`), and the first
/// occurrence of each VM on stderr.
#[cold]
fn jit_alloc_oom_without_thread(vm: &SharedVm, msg: &str) {
    let before = vm
        .mem
        .alloc_ladder
        .jit_alloc_oom_without_thread
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if before == 0 {
        eprintln!(
            "[GC] jit_alloc_oom: no JIT thread to publish `OutOfMemoryError: {msg}` on; \
             the compiled caller exits with nothing pending (first of this VM)"
        );
    }
}

// ---------------------------------------------------------------------------
// gen r4w5/oomjit5 (2026-09-24): a leftover native return must not outlive a
// throw that compiled code CATCHES, and the VM half of the old-gen root census
// ---------------------------------------------------------------------------

/// Drop `native_pending_return` at a door where compiled code (or the
/// interpreter on its behalf) ENTERS a handler for an exception that came out
/// of compiled code. Returns whether a value was dropped.
///
/// # The leak
///
/// `native_pending_return` roots a native call's returned object (or thrown
/// exception) until the caller has it somewhere the collector scans. The
/// interpreter consumes it at `native_return_pushed_to_stack`, and every throw
/// the interpreter unwinds drains it first — `settle_thrown_exception`
/// (`runtime/interpreter.rs`): "a leftover must not outlive this throw". The
/// JIT's fast-native paths in this file set it as their object-return handoff
/// root (`jit_integer_value_of_direct` and ~20 siblings) and nothing on the
/// compiled side ever clears it, and the compiled catch doors never pass
/// through `settle_thrown_exception`. So after an exception compiled code
/// catches, the last object a compiled fast-native call returned stayed a GC
/// root — with everything it reaches — until the next interpreted native
/// funnel happened to overwrite it. After an `OutOfMemoryError` that is
/// exactly the data the handler is trying to let go of
/// (`docs/feature-designs/jit-ir-optimizations.md`).
///
/// # Why dropping it here is safe
///
/// At a catch door the throwing frame's operand stack is being discarded and
/// the throwable itself is already held elsewhere (the compiled handler's
/// out-slot, or the handler frame's operand stack) before this runs. A value a
/// native returned earlier that the compiled code still needs is in its frame
/// (every fallible call flushes the operand stack to frame slots first), and
/// the frame is a root. The slot is a HANDOFF root, not an owner: the
/// interpreter already drops it on every throw.
///
/// Also the census hook for the compiled catch doors: under
/// `CRATONVM_DBG=oldmark-root-census` a handler entered with objects still in
/// the deopt / exceptional stashes is reported, because every consumer that
/// owned them has run by the time a handler is entered — see
/// [`oldmark_census_thread_line`].
pub(crate) fn drain_native_return_at_compiled_catch(
    thread: &mut JvmThread,
    door: &'static str,
) -> bool {
    let dropped = thread.native_pending_return.take().is_some();
    if oldmark_census_on() && cratonvm_jit::deopt::stashed_deopt_object_count() > 0 {
        oldmark_census_thread_line(thread, door);
    }
    dropped
}

// ---------------------------------------------------------------------------
// gc-common w36-f (handoff-w36b): a native's object return handed to compiled
// code stops being a GC root at the hand-off
// ---------------------------------------------------------------------------

/// Drop `native_pending_return` as a JIT helper hands its result back to
/// compiled code. Called by the extern wrapper of every helper that can return
/// a native's object (see [`drop_native_return_handed_to_compiled_code`]),
/// AFTER the body under its panic guard has finished, so after the helper's
/// last GC point by construction.
///
/// # Safety
///
/// The same contract as [`jit_thread_mut`]: called on the thread that ran the
/// helper, with no `JitThreadGuard` of that helper still live.
#[inline]
pub(crate) unsafe fn native_return_handed_to_compiled_code() {
    if let Some((thread, _guard)) = jit_thread_mut() {
        drop_native_return_handed_to_compiled_code(thread);
    }
}

/// The body of [`native_return_handed_to_compiled_code`]: forget the handoff
/// root.
///
/// # The leak (handoff-w36b, cause 1 of `common-w4o`)
///
/// `native_pending_return` roots a native's returned object (or thrown
/// exception) until its caller holds it somewhere the collector scans. The
/// interpreter consumes it when it pushes the value
/// (`native_return_pushed_to_stack`); compiled code never did, so after a
/// compiled `WeakReference.get()` (a native bridge) the referent stayed a GC
/// root until the NEXT native call replaced it. A loop that calls `get()` every
/// iteration re-rooted the referent before every collection and no allocation-
/// driven collection could ever clear it (`WeakClearYoungProbe`).
///
/// # Why dropping it at the hand-off is safe
///
/// Once the helper returns, the value is in `RAX` and compiled code treats it
/// exactly like every other reference a helper returns — a compiled or
/// interpreted Java callee's result through the same `jit_invoke_dispatch`, a
/// `jit_new_object` allocation — none of which ever had a handoff root. The
/// caller moves it to its operand stack / a register before its next
/// safepoint, where the oop map, the shadow push or the conservative frame
/// scan publishes it if it is still live; a GC requested by another thread
/// waits for this thread's next poll (the thread never left the running-Java
/// state), and a cross-thread take-over of a thread stopped in the window reads
/// `RAX` from the captured context conservatively (`xt_root_scan.rs`). The
/// thrown-exception half is parked in `jit_pending_exception` (its own root)
/// before a helper returns its sentinel. So the slot is dead weight past the
/// return, and the only thing it still did was keep a dropped value alive.
///
/// A Rust caller that calls one of these extern helpers and keeps the result
/// across a further GC point must root it itself (`varhandle_read_direct_reference`,
/// `lambda_int_to_double_generic` do).
#[inline]
pub(crate) fn drop_native_return_handed_to_compiled_code(thread: &mut JvmThread) {
    thread.native_pending_return = None;
}

/// Drop `native_pending_return` where compiled code ENTERS a GC-capable
/// helper: the top of every helper that calls [`jit_safepoint_flush_satb`]
/// (allocation, invoke dispatch, the virtual MIC, monitor enter, the direct
/// natives, ...) and the cooperative safepoint poll
/// ([`jit_safepoint_slow_path`]).
///
/// # The leak (r11, 2026-09-25)
///
/// The catch doors above were the only place the compiled side ever cleared
/// the handoff root. So in a compiled loop the LAST object a native call
/// returned stayed a GC root until the next native call replaced it — through
/// every collection in between. `tools/probes/interp/L5/SoftRefBeforeOome`:
///
/// ```text
/// while (true) { hold.add(new long[1024]); if (soft.get() != null) touched++; }
/// ```
///
/// Once `fill`'s loop is OSR-compiled, `soft.get()` (a registered native,
/// reached through the JIT's native dispatch) leaves the 16 MiB referent
/// in `native_pending_return`, and nothing else in the loop calls a native.
/// Every allocation-failure collection — the last-ditch one that condemns
/// every soft reference included — then found the referent strongly
/// reachable through the thread's field roots (and the root snapshot
/// republished from them at GC entry), restored it, and the helper threw
/// `OutOfMemoryError` with the soft reference uncleared. The interpreter never
/// did this: it consumes the slot at `native_return_pushed_to_stack`.
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// # Why dropping it here is safe
///
/// The slot bridges a native's return from the funnel to the caller's
/// collector-visible storage. When compiled code is entering a NEW helper, the
/// helper that produced the value has already returned it: compiled code
/// holds it where the frame scan covers this call site (a flushed frame slot,
/// a mapped spill, a callee-saved image -- the same obligation every object
/// that came from a `getfield` already relies on), or has dropped it. This is the compiled twin
/// of `native_return_pushed_to_stack`, one door later.
///
/// The window it still covers is untouched: from the funnel's return to the
/// helper's return, and in compiled code up to its next helper call (a frozen
/// peer scanned without a poll).
///
/// A helper that holds a native's return in a Rust local and then runs Java
/// before handing it on would lose that root here — but it already loses it
/// to the first native call that Java makes (the funnel overwrites the slot),
/// so the contract "no Java between a native return and its consumer" is not
/// narrowed by this door.
///
/// # Safety
///
/// Same contract as [`jit_thread_mut`]: call only on the thread that entered
/// compiled code, with no `JitThreadGuard` of this thread live — i.e. at the
/// very top of a helper body, before its own borrows.
#[inline]
unsafe fn drain_native_return_at_compiled_helper_entry() {
    if let Some((thread, _guard)) = jit_thread_mut() {
        if thread.native_pending_return.is_some() {
            thread.native_pending_return = None;
        }
    }
}

/// Is `CRATONVM_DBG=oldmark-root-census` on? The first call also installs the
/// VM-side labeller the generational old-gen census asks for
/// (`cratonvm_gc::gen_heap::install_oldmark_census_vm_labeller`). Called only
/// from exception doors, so the arming costs nothing on a normal run.
pub(crate) fn oldmark_census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OLDMARK_ROOT_CENSUS");
        if on {
            cratonvm_gc::gen_heap::install_oldmark_census_vm_labeller(oldmark_census_vm_labels);
        }
        on
    })
}

/// One line naming what `thread` holds OUTSIDE its frames at `door`: the
/// deopt / exceptional stashes (with their top identities), native pins,
/// handle slots, the native allocation pool and the three parked references.
fn oldmark_census_thread_line(thread: &JvmThread, door: &str) {
    eprintln!(
        "[oldmark-root-census] {door}: tid={} stash_refs={} top_exceptional={:?} \
         top_deopt={:?} native_pins={} handle_slots={} native_alloc_pool={} \
         jit_pending_exception={} native_pending_return={} uncaught_pending={}",
        thread.thread_id.0,
        cratonvm_jit::deopt::stashed_deopt_object_count(),
        cratonvm_jit::deopt::peek_exceptional_identity(),
        cratonvm_jit::deopt::peek_last_deopt_identity(),
        thread.native_pin_roots.len(),
        thread.handle_slots.iter().flatten().count(),
        thread.native_alloc_pool.len(),
        thread.jit_pending_exception.is_some(),
        thread.native_pending_return.is_some(),
        thread.uncaught_exception_pending.is_some(),
    );
    oldmark_census_frames_line(door);
}

/// gcf f1/o (2026-10-01): the most compiled activations
/// [`oldmark_census_frames_line`] prints; a deeper stack ends in `...`.
const OLDMARK_CENSUS_MAX_FRAMES: usize = 12;

/// gcf f1/o: which body an activation runs, for the census: `ir` (the
/// optimizing tier), `sp` (single pass), with `-osr` for an OSR body.
fn oldmark_census_tier(used_ir_backend: bool, osr: bool) -> &'static str {
    match (used_ir_backend, osr) {
        (true, false) => "ir",
        (true, true) => "ir-osr",
        (false, false) => "sp",
        (false, true) => "sp-osr",
    }
}

/// gcf f1/o: one activation of the census frames line,
/// `<label>@<tier>:<bci>`.
fn oldmark_census_frame_token(label: &str, tier: &str, bci: i32) -> String {
    format!("{label}@{tier}:{bci}")
}

/// gcf f1/o (2026-10-01), `gcd-d6u-pinned-callee-oome-heap-stays-full-after-the-catch`:
/// one line naming the compiled activations live on THIS thread at `door`,
/// innermost first, each with the TIER of the body it runs.
///
/// Why: the pinned-callee family (`Gcd1PinnedCalleeOomeProbe`,
/// `GenR4W6JitOomRootProbe`'s `oome-compiled-callee`) warms its compiled pair
/// for exactly 20 000 calls, which is the default C2 bar
/// (`CRATONVM_TIER_C2_THRESHOLD`): the optimizing compile of `chainCatch` and
/// `grow` is requested on the LAST warm-up call and finishes in the background,
/// so whether the measured call enters the single-pass or the optimizing body
/// is a race the host's load decides -- the load dependence the pages record.
/// The two tiers keep different conservative words alive across the catch (the
/// optimizing tier clears dead homes, `value_use_ranges`; the single-pass claim
/// covers java locals only, `sp_local_map_claim_enabled`). This line, printed
/// at the compiled-code OOME and again at the native funnel's first
/// post-OOME major (`native_call_owes_oome_major`), says which race a run lost,
/// and marks where the post-catch `[holder-census]` blocks begin.
pub(crate) fn oldmark_census_frames_line(door: &str) {
    use std::fmt::Write as _;
    let frames = crate::jit::conservative_roots::active_compiled_frames();
    let mut tokens = String::new();
    for f in frames.iter().take(OLDMARK_CENSUS_MAX_FRAMES) {
        let tier = if f.cm_ptr == 0 {
            "?"
        } else {
            // SAFETY: `cm_ptr` names the artifact a LIVE activation of this
            // thread is running (`ActiveCompiledFrame::cm_ptr`: valid for as
            // long as the frame is live), and this thread is below it.
            let cm = unsafe { &*(f.cm_ptr as *const cratonvm_jit::CompiledMethod) };
            oldmark_census_tier(cm.used_ir_backend, cm.osr_pc_to_native.is_some())
        };
        let _ = write!(
            tokens,
            " {}",
            oldmark_census_frame_token(&f.label, tier, f.bci)
        );
    }
    if frames.len() > OLDMARK_CENSUS_MAX_FRAMES {
        tokens.push_str(" ...");
    }
    eprintln!(
        "[oldmark-root-census] {door} frames: n={}{tokens}",
        frames.len()
    );
}

/// gch h1/fr (2026-10-08, d9d): the INTERPRETER frames of `thread` at `door`,
/// innermost first, each `Class.method@pc` (at most
/// [`OLDMARK_CENSUS_MAX_FRAMES`]; a deeper stack ends in `...`). The twin of
/// [`oldmark_census_frames_line`]: together they say which side of each call
/// ran compiled, which names the door that pinned a `native-pin` holder.
fn oldmark_census_interp_frames_line(thread: &JvmThread, door: &str) {
    use std::fmt::Write as _;
    let mut tokens = String::new();
    for f in thread.frames.iter().rev().take(OLDMARK_CENSUS_MAX_FRAMES) {
        let _ = write!(
            tokens,
            " {}",
            oldmark_census_interp_frame_token(f.class_name(), f.method_name(), f.pc)
        );
    }
    if thread.frames.len() > OLDMARK_CENSUS_MAX_FRAMES {
        tokens.push_str(" ...");
    }
    eprintln!(
        "[oldmark-root-census] {door} interp: n={}{tokens}",
        thread.frames.len()
    );
    // gci i4/il: each interpreter frame's kind and live-local mask (low word),
    // so an `interp-local-live` holder can be pinned to one frame.
    let mut kinds = String::new();
    for f in thread.frames.iter().rev().take(OLDMARK_CENSUS_MAX_FRAMES) {
        let _ = write!(
            kinds,
            " {}.{}:{}{}:live={:#x}",
            f.class_name(),
            f.method_name(),
            if f.owned_meta().is_some() {
                "owned"
            } else {
                "cached"
            },
            if f.liveness_keeps_slot0() { "+s0" } else { "" },
            f.live_locals_mask_here().low_word(),
        );
    }
    eprintln!("[oldmark-root-census] {door} interp-kinds:{kinds}");
}

/// gch h1/fr: one interpreter frame of [`oldmark_census_interp_frames_line`].
fn oldmark_census_interp_frame_token(class: &str, method: &str, pc: usize) -> String {
    format!("{class}.{method}@{pc}")
}

/// gch h1/fr (d9d): ` <k>/<n>` for every slot `k` of the `n`-slot pin stack
/// `pins` that holds one of `markers`' addresses, in pin-stack order, or
/// ` none`. A low `k` was pushed by an OUTER door: the pin stack is LIFO, so
/// the slot's depth orders the doors on the Rust stack.
fn oldmark_census_pinned_marker_slots(pins: &[usize], markers: &[usize]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (k, addr) in pins.iter().enumerate() {
        if markers.contains(addr) {
            let _ = write!(out, " {k}/{}", pins.len());
        }
    }
    if out.is_empty() {
        out.push_str(" none");
    }
    out
}

#[cfg(test)]
mod gch_h1fr_census_door_tests {
    //! gch h1/fr: the holder-census door's attribution lines, pure parts.
    use super::{oldmark_census_interp_frame_token, oldmark_census_pinned_marker_slots};

    #[test]
    fn gch_h1fr_pinned_marker_slots_name_each_pin_of_a_marker() {
        assert_eq!(
            oldmark_census_pinned_marker_slots(&[0x10, 0x20, 0x30, 0x20], &[0x20]),
            " 1/4 3/4"
        );
        assert_eq!(
            oldmark_census_pinned_marker_slots(&[0x10, 0x30], &[0x20, 0x30]),
            " 1/2"
        );
        assert_eq!(
            oldmark_census_pinned_marker_slots(&[0x10], &[0x20]),
            " none"
        );
        assert_eq!(oldmark_census_pinned_marker_slots(&[], &[0x20]), " none");
    }

    #[test]
    fn gch_h1fr_interp_frame_token_shape() {
        assert_eq!(
            oldmark_census_interp_frame_token("Gcd1ArgPinSpecialProbe", "virtualCaller", 7),
            "Gcd1ArgPinSpecialProbe.virtualCaller@7"
        );
    }
}

#[cfg(test)]
mod gcf_f1o_census_frames_tests {
    //! gcf f1/o: the census frames line's pure parts.
    use super::{oldmark_census_frame_token, oldmark_census_tier};

    #[test]
    fn gcf_f1o_census_tier_names_every_body() {
        assert_eq!(oldmark_census_tier(true, false), "ir");
        assert_eq!(oldmark_census_tier(true, true), "ir-osr");
        assert_eq!(oldmark_census_tier(false, false), "sp");
        assert_eq!(oldmark_census_tier(false, true), "sp-osr");
    }

    #[test]
    fn gcf_f1o_census_frame_token_shape() {
        assert_eq!(
            oldmark_census_frame_token(
                "Probe.chainCatch:(Ljava/lang/ref/WeakReference;I)V",
                "sp",
                12
            ),
            "Probe.chainCatch:(Ljava/lang/ref/WeakReference;I)V@sp:12"
        );
        assert_eq!(
            oldmark_census_frame_token("A.b:()V", "ir", -1),
            "A.b:()V@ir:-1"
        );
    }
}

// ---------------------------------------------------------------------------
// gen r4w6/oomjit6 (2026-09-24): the census labeller outside the JIT scope
// ---------------------------------------------------------------------------
//
// Wave 5's labeller could see the collecting thread's `JvmThread` only through
// `current_jit_thread_ptr`, i.e. only while the collection was nested in a JIT
// scope, and it knew nothing of the thread's INTERPRETER frames, the statics,
// the string pool or any other VM table. Everything those hold printed as
// `vm/unattributed` -- which is exactly the verdict the wave-5 run gave the
// JIT OOME retention, and exactly the verdict that names nothing. The
// collection door (`gc_and_alloc::run_collection_pause`) now publishes the VM
// and the collecting thread here for the length of the collection, and stages
// the index SECTIONS of the root vector it built with the census itself
// (`cratonvm_gc::gen_heap::stage_oldmark_census_root_sections`), so every
// root is named by the part of the VM that pushed it, and the labeller below
// refines that with the structure that holds the address.

thread_local! {
    /// `(vm, thread)` of the collection door currently running on this OS
    /// thread, or nulls. Written only by [`OldmarkCensusDoor`], and only while
    /// the census token is set.
    static OLDMARK_CENSUS_DOOR: std::cell::Cell<(*const SharedVm, *const JvmThread)> =
        const { std::cell::Cell::new((std::ptr::null(), std::ptr::null())) };
    /// The frame and VM-table labels the door computed BEFORE the collection
    /// (gen r4w6 orchestrator). They cannot be computed by the labeller: it
    /// runs inside the collector, which holds the heap's locks, and the frame
    /// scanners and `is_object_address` take those same locks -- the wave-6
    /// merge deadlocked every `CRATONVM_DBG=oldmark-root-census` run at its
    /// first collection that way. Replayed by [`oldmark_census_vm_labels`].
    static OLDMARK_CENSUS_STAGED_LABELS: std::cell::RefCell<Vec<(usize, &'static str)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// RAII scope, held by a collection door across its `collect_garbage` call:
/// the census labeller may read `vm` and `thread` for its duration. Restores
/// the previous value on drop, so a nested collection cannot leave its own
/// door behind.
pub(crate) struct OldmarkCensusDoor {
    prev: (*const SharedVm, *const JvmThread),
    prev_labels: Vec<(usize, &'static str)>,
    /// gen r5w4/jit8: the holder census's markers for this collection,
    /// restored to the outer staging when the door closes.
    _markers: cratonvm_gc::gen_heap::OldmarkCensusStagedHolderMarkers,
}

impl OldmarkCensusDoor {
    /// `None` (nothing published) unless `CRATONVM_DBG=oldmark-root-census`
    /// is set, so a normal run pays one cached flag load per collection.
    ///
    /// The caller must keep `vm` and `thread` alive, and must not write
    /// through any other reference to `thread`, until the returned guard is
    /// dropped: the labeller forms a shared reference to it from the stored
    /// pointer. A collection door satisfies both by construction -- it holds
    /// the guard only across `collect_garbage_with_finalizers`, which takes
    /// neither.
    pub(crate) fn enter(vm: &SharedVm, thread: &JvmThread) -> Option<Self> {
        if !oldmark_census_on() {
            return None;
        }
        let now = (vm as *const SharedVm, thread as *const JvmThread);
        // The frame and table labels, computed here, outside the collector's
        // heap locks (see `OLDMARK_CENSUS_STAGED_LABELS`).
        let mut labels: Vec<(usize, &'static str)> = Vec::new();
        {
            let mut push = |addr: usize, label: &'static str| labels.push((addr, label));
            oldmark_census_label_frames(vm, thread, &mut push);
            oldmark_census_label_vm_tables(vm, thread, &mut push);
        }
        // gen r5w4/jit8: the holder census's markers, and one line naming the
        // collecting thread (the `initiator` section of every holder line) and
        // the layout-free sweep counters as they stand after this pause's root
        // scan (cumulative; compare with the previous door line). Silent when
        // no marker is live, so a run's minor collections stay quiet.
        let markers = oldmark_census_holder_markers(vm);
        if !markers.is_empty() {
            let (bands, fallback, foreign, a5_sweeps, a5_roots, a5_frames) =
                crate::jit::conservative_roots::band_path::snapshot();
            eprintln!(
                "[holder-census] door tid={} markers={} bandpath=(bands={bands} \
                 fallback={fallback} foreign_innermost={foreign} a5_sweeps={a5_sweeps} \
                 a5_roots={a5_roots} a5_frames={a5_frames}) native_pins={} \
                 native_pending_return={} stash_refs={}",
                thread.thread_id.0,
                markers.len(),
                thread.native_pin_roots.len(),
                thread.native_pending_return.is_some(),
                cratonvm_jit::deopt::stashed_deopt_object_count(),
            );
            // gch h1/fr (2026-10-08, `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`):
            // a `native-pin` holder does not say WHICH Rust door pinned it.
            // These three lines do, read together: where in the pin stack each
            // marker sits, and which activations (compiled and interpreted)
            // this thread runs at the pause. A compiled callee under an
            // INTERPRETED caller is the interpreter's argument window
            // (`jit_bridge.rs` `JitArgPinGuard`); a compiled callee under a
            // compiled caller with no `[DISP_TRACE]` / `[MIC_TRACE]` line for
            // it is a by-name door (`interpreter::execute`'s own argument
            // guard); an interpreted callee entered by name is
            // `invoke_on_class_shared_inner`'s `PinnedDispatchArgs`.
            let pin_addrs: Vec<usize> = thread
                .native_pin_roots
                .iter()
                .map(|o| o.as_ptr() as usize)
                .collect();
            let marker_addrs: Vec<usize> = markers.iter().map(|m| m.addr).collect();
            eprintln!(
                "[holder-census] door pinned-markers:{}",
                oldmark_census_pinned_marker_slots(&pin_addrs, &marker_addrs)
            );
            oldmark_census_frames_line("holder-census door");
            oldmark_census_interp_frames_line(thread, "holder-census door");
        }
        let staged_markers = cratonvm_gc::gen_heap::stage_oldmark_census_holder_markers(markers);
        let prev_labels = OLDMARK_CENSUS_STAGED_LABELS
            .try_with(|c| std::mem::replace(&mut *c.borrow_mut(), labels))
            .ok()?;
        let prev = OLDMARK_CENSUS_DOOR.try_with(|c| c.replace(now)).ok()?;
        Some(Self {
            prev,
            prev_labels,
            _markers: staged_markers,
        })
    }
}

/// gen r5w4/jit8: the holder census's MARKERS for one collection -- the
/// referent of every ACTIVE plain `java.lang.ref.WeakReference` (subclasses
/// are left out: `WeakHashMap`, `ThreadLocal` and the JDK's own caches extend
/// it, and a retention probe watches its data with the plain class), at most
/// 16. The census (`gc/src/gen_heap_oldmark_census.rs`, "the HOLDER census")
/// then names every root that still reaches one.
///
/// Read after `weakref_null_referents_pre_gc` (the door opens after it), so
/// the processor's pairs are this pause's active set. `try_*` locks, taken one
/// at a time: a diagnostic must never block a pause, and a lock it cannot
/// take costs the markers, not the collection.
fn oldmark_census_holder_markers(
    vm: &SharedVm,
) -> Vec<cratonvm_gc::gen_heap::OldmarkCensusHolderMarker> {
    const MAX_MARKERS: usize = 16;
    let weak_class = match vm.classes.class_manager.try_read() {
        Some(cm) => cm.find_bootstrap_class_by_name("java/lang/ref/WeakReference"),
        None => None,
    };
    let Some(weak_class) = weak_class else {
        return Vec::new();
    };
    let pairs = match vm.mem.ref_processor.try_lock() {
        Some(rp) => rp.weak_phantom_active_pairs(),
        None => return Vec::new(),
    };
    let mut markers = Vec::new();
    for (reference, referent) in pairs {
        if markers.len() >= MAX_MARKERS {
            break;
        }
        let Some(reference_obj) = vm.mem.heap.is_object_address(reference) else {
            continue;
        };
        if vm.mem.heap.class_id_of(reference_obj) != weak_class {
            continue;
        }
        markers.push(cratonvm_gc::gen_heap::OldmarkCensusHolderMarker {
            addr: referent,
            why: format!("referent of java/lang/ref/WeakReference@0x{reference:x}"),
        });
    }
    markers
}

impl Drop for OldmarkCensusDoor {
    fn drop(&mut self) {
        let prev = self.prev;
        let _ = OLDMARK_CENSUS_DOOR.try_with(|c| c.set(prev));
        let labels = std::mem::take(&mut self.prev_labels);
        let _ = OLDMARK_CENSUS_STAGED_LABELS.try_with(|c| *c.borrow_mut() = labels);
    }
}

/// The VM-side labeller of the generational old-gen root census: every heap
/// address a VM-owned structure of the COLLECTING thread (or of the VM) holds,
/// with the structure's name. Installed by [`oldmark_census_on`]; called by
/// the collector inside the pause, on the collecting thread, before the mark.
/// The census keeps the FIRST label an address receives, so the emission
/// order below is most-specific first.
///
/// * The deopt / exceptional stashes are thread-locals of this OS thread, so
///   they are always reachable.
/// * The `JvmThread` is the collection door's ([`OldmarkCensusDoor`]) when one
///   is published, else the JIT scope's (`current_jit_thread_ptr`), else
///   nothing (the wave-5 behaviour, kept for a collection no door announced).
/// * The interpreter frames and the VM tables need the `SharedVm`, which only
///   the door provides.
fn oldmark_census_vm_labels(out: &mut dyn FnMut(usize, &'static str)) {
    // Diagnostic-only side effect, stated: this counts as a scan for the
    // debug-build pairing check in `remap_stashed_deopt_objects`, which only
    // makes that check more permissive while the census token is set.
    cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| out(addr as usize, "deopt-stash"));
    let (door_vm, door_thread) = OLDMARK_CENSUS_DOOR
        .try_with(std::cell::Cell::get)
        .unwrap_or((std::ptr::null(), std::ptr::null()));
    let thread_ptr: *const JvmThread = if door_thread.is_null() {
        current_jit_thread_ptr() as *const JvmThread
    } else {
        door_thread
    };
    if thread_ptr.is_null() {
        return;
    }
    // SAFETY: either the collection door's thread, published by
    // `OldmarkCensusDoor::enter` for exactly the collection this runs inside
    // (whose contract keeps it alive and unwritten until the guard drops), or
    // `current_jit_thread_ptr`, this OS thread's own `JvmThread` installed by
    // `set_jit_thread` for the JIT scope the collection is nested in. The
    // collector runs on this thread inside a stop-the-world pause, so nothing
    // mutates these fields while they are read; every read below is shared and
    // copies values out, the same shared-reference-shaped access
    // `jit_local_handler_lookup` makes to `jit_pending_exception`.
    let thread = unsafe { &*thread_ptr };
    oldmark_census_label_thread_fields(thread, out);
    if door_vm.is_null() {
        return;
    }
    // The door's frame and VM-table labels, computed before the collection
    // took the heap locks. `try_borrow`: a labeller re-entered from inside
    // `out` (it is not) would find the vector borrowed and skip it rather than
    // panic in a pause.
    let _ = OLDMARK_CENSUS_STAGED_LABELS.try_with(|c| {
        if let Ok(labels) = c.try_borrow() {
            for &(addr, label) in labels.iter() {
                out(addr, label);
            }
        }
    });
}

/// Every `JvmThread` field that holds a heap reference, by field. The list is
/// `roots::push_off_frame_thread_roots`'s; its catch-all at the end keeps a
/// field added there later from printing as unlabelled.
fn oldmark_census_label_thread_fields(
    thread: &JvmThread,
    out: &mut dyn FnMut(usize, &'static str),
) {
    let singles = [
        (thread.jit_pending_exception, "jit-pending-exception"),
        (thread.native_pending_return, "native-pending-return"),
        (
            thread.uncaught_exception_pending,
            "uncaught-exception-pending",
        ),
        (thread.java_thread_obj, "own-thread-mirror"),
    ];
    for (obj, label) in singles {
        if let Some(obj) = obj {
            out(obj.as_ptr() as usize, label);
        }
    }
    for obj in &thread.native_pin_roots {
        out(obj.as_ptr() as usize, "native-pin");
    }
    for obj in thread.handle_slots.iter().flatten() {
        out(obj.as_ptr() as usize, "handle-slot");
    }
    for obj in &thread.native_alloc_pool {
        out(obj.as_ptr() as usize, "native-alloc-pool");
    }
    for entry in &thread.jit_hashmap_string_node_cache {
        out(entry.map.as_ptr() as usize, "jit-memo-cache");
        out(entry.node.as_ptr() as usize, "jit-memo-cache");
    }
    for entry in &thread.string_case_cache {
        out(entry.source.as_ptr() as usize, "jit-memo-cache");
    }
    for (_key_id, key, value) in &thread.scoped_values {
        if let Some(key) = key {
            out(key.as_ptr() as usize, "scoped-value");
        }
        if let crate::types::Value::Object(Some(obj)) = value {
            out(obj.as_ptr() as usize, "scoped-value");
        }
    }
    for value in &thread.printed {
        if let crate::types::Value::Object(Some(obj)) = value {
            out(obj.as_ptr() as usize, "print-buffer");
        }
    }
    let mut rest: Vec<crate::types::ObjectRef> = Vec::new();
    crate::memory::roots::push_off_frame_thread_roots(thread, &mut rest);
    for obj in rest {
        out(obj.as_ptr() as usize, "thread-field");
    }
}

/// The collecting thread's INTERPRETER frames, split by WHY a slot is a root.
///
/// The split is the point. With any thread in compiled code the root scans
/// add the tag-blind conservative probes (`roots::conservative_locals_enabled`);
/// `collect_roots` and `update_root_snapshot` keep the bytecode-liveness proof
/// for them, but the JNI deposit (`NativeContextImpl::deposit_root_snapshot_inner`)
/// switches to `scan_local_objects_all_live` under the same flag, and every
/// scan does while a debugger can read locals (gch h1/jp) -- so a reference
/// local that bytecode liveness says is DEAD can still be a root, a family that
/// exists only with the JIT on (or a debugger), and so a suspect for any
/// retention that `--nojit` does not reproduce. Emitted live-first, so an
/// address a live slot also holds keeps the live label.
fn oldmark_census_label_frames(
    vm: &SharedVm,
    thread: &JvmThread,
    out: &mut dyn FnMut(usize, &'static str),
) {
    let heap = &vm.mem.heap;
    let mut v: Vec<crate::types::ObjectRef> = Vec::new();
    for frame in thread.frames.iter() {
        v.clear();
        frame.scan_local_objects(&mut v, heap);
        for o in &v {
            out(o.as_ptr() as usize, "interp-local-live");
        }
        v.clear();
        frame.stack.scan_object_refs(&mut v, heap);
        for o in &v {
            out(o.as_ptr() as usize, "interp-stack");
        }
        if let Some(m) = frame.monitor_on_exit {
            out(m.as_ptr() as usize, "interp-monitor-on-exit");
        }
    }
    for frame in thread.frames.iter() {
        v.clear();
        frame.scan_local_objects_all_live(&mut v, heap);
        for o in &v {
            out(o.as_ptr() as usize, "interp-local-dead-by-liveness");
        }
    }
    for frame in thread.frames.iter() {
        v.clear();
        frame.scan_locals_conservative(&mut v, heap);
        for o in &v {
            out(o.as_ptr() as usize, "interp-local-lost-tag");
        }
        v.clear();
        frame.stack.scan_object_refs_conservative(&mut v, heap);
        for o in &v {
            out(o.as_ptr() as usize, "interp-stack-lost-tag");
        }
    }
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        // gcd d2/f: say WHICH kind of shadow entry names the object, labels
        // being first-wins. An indirect entry is a frame block's (IR tier)
        // pointer to a live frame's home; one naming a slot BELOW this
        // function's own frame names stack no live frame owns any more -- a
        // block its frame failed to retract, which a moving cycle would
        // write through. A value entry is a safepoint's pushed copy
        // (`jit-shadow-stack-value`; before this split every shadow root
        // was labelled `jit-shadow-stack`).
        let here = 0usize;
        let own_sp = std::ptr::addr_of!(here) as usize;
        for slot in thread.shadow_stack.indirect_slots() {
            // SAFETY: `indirect_slots` returns only aligned words inside this
            // thread's own stack band named by an entry below `top`, the same
            // words `for_each_value` dereferences just below; this runs on the
            // collecting thread for its own `JvmThread`.
            let w = unsafe { (slot as *const usize).read() };
            if let Some(o) = heap.is_object_address(w) {
                let label = if slot < own_sp {
                    "jit-shadow-stack-indirect-below-sp"
                } else {
                    "jit-shadow-stack-indirect"
                };
                out(o.as_ptr() as usize, label);
            }
        }
        thread.shadow_stack.for_each_value(|w| {
            if let Some(o) = heap.is_object_address(w) {
                out(o.as_ptr() as usize, "jit-shadow-stack-value");
            }
        });
    }
    v.clear();
    crate::native::jni::collect_local_ref_roots(&mut v);
    for o in &v {
        out(o.as_ptr() as usize, "jni-local");
    }
}

/// The VM-wide tables `roots::collect_roots` roots directly. `try_*` locks:
/// the census is a diagnostic and must never be the reason a pause blocks, so
/// a table whose lock is unavailable is simply left unlabelled.
fn oldmark_census_label_vm_tables(
    vm: &SharedVm,
    _thread: &JvmThread,
    out: &mut dyn FnMut(usize, &'static str),
) {
    if let Some(statics) = vm.classes.statics.try_read() {
        for fields in statics.values() {
            for value in fields.iter() {
                if let crate::types::Value::Object(Some(obj)) = *value {
                    out(obj.as_ptr() as usize, "static-field");
                }
            }
        }
    }
    if let Some(locks) = vm.classes.class_locks.try_read() {
        for obj in locks.values() {
            out(obj.as_ptr() as usize, "class-lock");
        }
    }
    if let Some(mirrors) = vm.classes.class_mirrors.try_read() {
        for obj in mirrors.values() {
            out(obj.as_ptr() as usize, "class-mirror");
        }
    }
    // gcg g4/rs H12, applied by g4/jm: under the weak String table this pause
    // did not root the pool.
    if !crate::memory::string_table::pool_left_out_now(vm) {
        if let Some(pool) = vm.mem.string_pool.try_read() {
            for obj in pool.values() {
                out(obj.as_ptr() as usize, "interned-string");
            }
        }
    }
    if let Some(oom) = vm.mem.singleton_oom.try_read() {
        if let Some(obj) = *oom {
            out(obj.as_ptr() as usize, "singleton-oom");
        }
    }
    if let Some(globals) = vm.natives.jni_global_refs.try_lock() {
        let mut v: Vec<crate::types::ObjectRef> = Vec::new();
        globals.collect_roots(&mut v);
        for o in v {
            out(o.as_ptr() as usize, "jni-global");
        }
    }
}

/// gen r4w6/oomjit6 (2026-09-24): a compiled local handler has just committed
/// to catching the exception in flight, so every exceptional frame its own
/// callees published on the way out is an orphan -- drop them, down to the
/// innermost JIT door's entry floor. Enabled by default;
/// `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=0` disables it. It remains a
/// no-op without a door. Returns how many frames were dropped.
///
/// Called ONLY from the two commit points of `jit_local_handler_lookup`, where
/// the catching frame is compiled code inside the innermost door's span. The
/// interpreter-side catch doors (`exception_dispatch`, `jit_bridge`) do not
/// call it: they run in Rust frames whose innermost door may be an outer one,
/// and the floor would then describe a different span. See
/// `docs/feature-designs/jit-ir-optimizations.md`
/// and `cratonvm_jit::deopt::truncate_exceptional_stash_to`.
///
/// JIT round 17 wave 3, lane syncdoor3
/// (`r17-syncdoor2-dropped-exceptional-frames-keep-a-handed-method-monitor-FIXED-20261005.md`):
/// each orphan's publisher has unwound into this catch, so the locks its
/// stub kept for a sink (a self-locking body's handed method monitor, a block
/// lock) would be released by nobody. Under the exceptional hand-over's
/// switches
/// ([`crate::runtime::interpreter::CompiledLocksOfAStash::dropped_frame_holds_released`])
/// the orphans are TAKEN one at a time from the top, innermost publisher
/// first, and what each names is released before the truncation; a frame
/// taken here is not counted in `STASH_DROPPED_UNCLAIMED` (it was claimed, by
/// this release). Off, exactly the old truncation. The stash depth is asked
/// first, so a catch with no orphan reads no switch.
fn drop_orphaned_exceptional_frames_at_compiled_catch(
    vm: &SharedVm,
    thread: &mut JvmThread,
    door: &'static str,
) -> usize {
    use crate::runtime::interpreter::CompiledLocksOfAStash;
    let Some(floor) = crate::jit::conservative_roots::exceptional_stash_floor() else {
        return 0;
    };
    let mut released_frames = 0usize;
    // JIT round 22 wave 8, lane sync: links 3 and 4 of the self-lock census's
    // hand-over chain (`self_lock_census.rs` `CatchStashAboveFloor`,
    // `OrphanDropFrames`). Reporting only.
    let census = vm.jit.tiered_manager.verdicts().self_lock_census();
    let above_floor = cratonvm_jit::deopt::exceptional_stash_depth() > floor;
    if above_floor {
        census.note(cratonvm_jit::self_lock_census::SelfLockRow::CatchStashAboveFloor);
    }
    if above_floor && CompiledLocksOfAStash::dropped_frame_holds_released() {
        while let Some((rframe, _cause, _point)) =
            cratonvm_jit::deopt::take_exceptional_frame_above_if(floor, |_frame, _point| true)
        {
            census.note(cratonvm_jit::self_lock_census::SelfLockRow::OrphanDropFrames);
            // Right after the take, before anything that can allocate.
            let _ = CompiledLocksOfAStash::release_for_a_dropped_frame(vm, thread, &rframe, door);
            released_frames += 1;
        }
    }
    let dropped = released_frames + cratonvm_jit::deopt::truncate_exceptional_stash_to(floor);
    if dropped > 0 && oldmark_census_on() {
        eprintln!(
            "[oldmark-root-census] {door}: dropped {dropped} orphaned exceptional frame(s) \
             above the entry floor {floor}"
        );
    }
    dropped
}

/// Negative-array-length signal for the fallible JIT array helpers
/// (`jit_newarray` / `jit_anewarray_object`). JLS requires
/// `java.lang.NegativeArraySizeException` (message = the offending length) for
/// `new T[n]` with `n < 0`. Mirroring `jit_alloc_oom`, this stashes the
/// exception in `JIT_PENDING_EXCEPTION` and returns the `0`/null sentinel; the
/// shared `emit_post_alloc_oom_check` bail then routes it through the method's
/// exception table (catchable), matching HotSpot and the interpreter. Reusing
/// the same channel + bail means no extra codegen is needed for the negative
/// case. As with OOM, if the exception object cannot be built the flag is left
/// unset and `0` returned.
///
/// JIT round 22 (lane throws; `CRATONVM_JIT_NEGATIVE_ARRAY_SIZE_OOME_FALLBACK`,
/// correctness, default ON, `=0` restores the old exit): when the
/// `NegativeArraySizeException` itself cannot be built (the heap is full),
/// the compiled caller used to take its exception exit with NOTHING pending,
/// which every drain reads as a plain deopt (a re-run of the `newarray`, or a
/// replay of the method from entry). HotSpot throws the `OutOfMemoryError`
/// its failed exception allocation raised; [`jit_alloc_oom`] is that door
/// (no-collect OOME, then the preallocated one), exactly as the allocation
/// failure of the array itself already takes.
#[cold]
fn jit_negative_array_size(vm: &SharedVm, length: i64) -> i64 {
    let mut heap_full = false;
    // SAFETY: called only from a JIT array helper on the thread that installed the
    // JIT thread pointer; no other `&mut JvmThread` borrow is live here. The
    // guard drops at the end of this block, before `jit_alloc_oom` re-borrows.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        match crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/NegativeArraySizeException",
            Some(&length.to_string()),
        ) {
            Ok(exc) => set_jit_pending_exception(thread, exc),
            // JIT round 22 wave 2 (lane throws; item 1 of
            // `docs/feature-designs/jdk-only-mode.md`): the
            // construction itself THREW (its own OutOfMemoryError with its own
            // message, a StackOverflowError at the stack limit, a class-load /
            // initialization error). HotSpot propagates that pending throwable;
            // so does this exit. Same switch: `=0` keeps the old silent exit.
            Err(crate::error::MethodCallFailed::ExceptionThrown(thrown))
                if jit_negative_array_size_oome_fallback_enabled() =>
            {
                set_jit_pending_exception(thread, thrown);
            }
            // JIT round 22 wave 3 (lane throws; item 1 of
            // `docs/feature-designs/jdk-only-mode.md`):
            // only the allocator's own out-of-memory answer means "the heap is
            // full". Any other internal failure (the class cannot be resolved, a
            // linkage error, a contained panic) keeps the pre-fallback exit: an
            // OOME there would hand a catchable `OutOfMemoryError: Java heap
            // space` to a program HotSpot gives a NegativeArraySizeException.
            Err(crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(
                crate::error::RuntimeError::OutOfMemoryError { .. },
            ))) => heap_full = true,
            Err(other) => {
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
                    eprintln!(
                        "[cratonvm] jit_negative_array_size: NegativeArraySizeException({length}) \
                         not built ({other}); compiled caller exits with nothing pending"
                    );
                }
            }
        }
    }
    if heap_full && jit_negative_array_size_oome_fallback_enabled() {
        return jit_alloc_oom(vm, "Java heap space");
    }
    0
}

/// `CRATONVM_JIT_NEGATIVE_ARRAY_SIZE_OOME_FALLBACK` (JIT round 22, lane
/// throws; correctness, default ON, `=0` off): see [`jit_negative_array_size`].
/// Read only on that cold path, never per allocation.
fn jit_negative_array_size_oome_fallback_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_NEGATIVE_ARRAY_SIZE_OOME_FALLBACK")
}

/// W1-vm: shared tail for the `jit_newarray` success paths — runs the optional
/// allocation trace and converts the `ObjectRef` into the raw `i64` pointer the
/// JIT caller expects. Factored out so the fast and slow paths stay identical.
#[inline]
// SAFETY: `obj_ref` is a freshly-allocated, non-null array object whose header
// the allocator initialized; the function only writes that array's own
// length/element slots within bounds.
unsafe fn jit_newarray_finish(obj_ref: ObjectRef, atype: i64, length: i64) -> i64 {
    let raw = obj_ref.as_ptr();
    if crate::runtime::env_cache::jit_newarray_trace() {
        let class_id_raw = std::ptr::read(raw as *const u32);
        let kind_byte = cratonvm_types::kind_tag_at(raw);
        let elem_byte = cratonvm_types::element_type_tag_at(raw);
        let stored_len = std::ptr::read(raw.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *const u32);
        let num_slots = (*(raw as *const cratonvm_types::ObjectHeader)).num_slots();
        eprintln!(
            "[JIT-NA] ptr={:p} atype={} len={} cid={} kind={} elem={} arrlen={} num_slots={}",
            raw, atype, length, class_id_raw, kind_byte, elem_byte, stored_len, num_slots
        );
    }
    raw as i64
}

/// JIT inline-TLAB completion helper.
///
/// Called from JIT-emitted code AFTER the inline TLAB bump has already
/// claimed the object's footprint at `obj_ptr` -- a compact layout's
/// `total_size` for a compact reservation, `HEADER_SIZE + num_fields *
/// SLOT_SIZE` for a legacy one -- and written the class id at offset 0 and
/// the whole 32-bit mark word at `MARK_WORD_OFFSET` (neutral, no hash, with
/// `GC_FLAG_HEADER` and, for a compact reservation, `GC_FLAG_COMPACT` in its
/// flags byte). This helper finishes the header (a LEGACY instance's shape
/// and hash words; the compact flag byte of a compact one -- see the layout
/// comment in the body), installs primitive-field typed-zero defaults, and
/// registers the object with the finalizer queue when its class overrides
/// `finalize()`.
/// It does NOT mint an identity hash: that is installed lazily on first
/// request, into the mark word, and the store that used to do it here was
/// writing to a header field deleted on 2026-08-07. See the layout comment
/// in the body.
///
/// Separating this from `jit_new_object` lets the JIT emit the cheap
/// bump-pointer prologue inline (~5-7 instructions) and pay a single
/// call only for the header-completion + primitive-defaults work that
/// touches the class-metadata `RwLock`.
///
/// # Safety
/// `vm_ptr` must be a valid `SharedVm` pointer; `obj_ptr` must be a
/// freshly-bumped TLAB allocation of the footprint described above, zeroed
/// past the header, with the class id and the mark word already written.
/// `num_fields` must match the class metadata.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_post_tlab_init(
    vm_ptr: i64,
    obj_ptr: i64,
    class_id_raw: i64,
    num_fields: i64,
) -> i64 {
    let f = || jit_post_tlab_init_body(vm_ptr, obj_ptr, class_id_raw, num_fields);
    contain("jit_post_tlab_init", OnPanic::Deopt, 0, f)
}

/// zt t10/ca2 / t11/ca3: split [`jit_post_tlab_init`]'s field-count argument
/// into the field count and the register-first flag
/// (`cratonvm_types::JIT_POST_TLAB_INIT_REGISTERED`, bit 30), which a compiled
/// `new` sets when it registered the object itself before its cursor commit
/// (`CRATONVM_ZGC_JIT_POST_INIT_REGISTER_FIRST`). Pure, so the split is
/// unit-tested without a `SharedVm`.
pub(crate) fn split_post_init_fields(num_fields: i64) -> (i64, bool) {
    let registered = (num_fields & cratonvm_types::JIT_POST_TLAB_INIT_REGISTERED) != 0;
    (num_fields & !cratonvm_types::JIT_POST_TLAB_INIT_REGISTERED, registered)
}

#[cfg(test)]
mod zt11ca3_split_post_init_fields_tests {
    use super::split_post_init_fields;
    use cratonvm_types::JIT_POST_TLAB_INIT_REGISTERED as REGISTERED;

    /// A bare count passes through unflagged; a flagged one loses only bit 30.
    /// The count includes inherited slots, so the cases go past the u16 range,
    /// all far below bit 30.
    #[test]
    fn the_register_first_bit_splits_off_and_leaves_the_count() {
        for n in [0i64, 1, 3, 255, 65_535, 65_536, (1 << 29) - 1] {
            assert_eq!(split_post_init_fields(n), (n, false), "bare {n}");
            assert_eq!(split_post_init_fields(n | REGISTERED), (n, true), "flagged {n}");
        }
        assert_eq!(REGISTERED, 1 << 30, "the bit the emitters set");
    }
}

/// Body of [`jit_post_tlab_init`], run under its panic guard.
unsafe fn jit_post_tlab_init_body(
    vm_ptr: i64,
    obj_ptr: i64,
    class_id_raw: i64,
    num_fields: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    let vm = &*(vm_ptr as *const SharedVm);
    // zt t10/ca2 (`CRATONVM_ZGC_JIT_POST_INIT_REGISTER_FIRST`): a compiled
    // `new` that registered the object itself, before it committed the
    // cursor, says so in bit 30 of the field count, and the registration below
    // is skipped (`cratonvm_types::JIT_POST_TLAB_INIT_REGISTERED`).
    let (num_fields, already_registered) = split_post_init_fields(num_fields);
    // gcg g1/p: compiled-`new` engagement (per VM; `CRATONVM_DBG_JIT_METHOD_STATS`).
    vm.jit.jit_alloc_class_cache.new_paths.note_post_tlab_init();
    if already_registered {
        vm.jit
            .jit_alloc_class_cache
            .new_paths
            .note_post_tlab_init_registered();
    }
    let class_id = ClassId::new(class_id_raw as u32);
    let raw_ptr = obj_ptr as *mut u8;

    // Finish header: the shape word of a LEGACY instance and the compact flag
    // of a compact one. Nothing else here is this helper's.
    //
    //   off 0..4   class_id    -- written inline by the JIT
    //   off 4..8   mark word   -- `MARK_WORD_OFFSET`; the flags byte is its
    //                             top byte (`GC_FLAGS_BYTE_OFFSET` == 7)
    //   off 8..16  LEGACY: field count (`NUM_SLOTS_OFFSET`) + identity hash
    //              COMPACT: the first field(s) -- the header is 8 bytes
    //
    // The compact flag store is a whole-byte store, sound only because the
    // object is FRESH (`gc_age` shares the byte and is zero exactly here), and
    // it carries `GC_FLAG_HEADER` too, which the emitter published in the same
    // byte: dropping it would make the header indistinguishable from zeroed
    // arena space (`SWEEP_NO_HEADER_FLAG`, `gc/src/gen_heap.rs`).
    //
    // The shape is the one the EMITTER reserved, read back from the flags
    // byte it stamped (both x64 inline allocators write `GC_FLAG_COMPACT`
    // there for a compact reservation), never re-decided from the class's
    // current layout. A method compiled while its class was a compatibility
    // stub reserves a legacy body; if the stub has since been upgraded and
    // given a compact layout, a fresh lookup here would flag that legacy-sized
    // body compact, and every walker would then size it from the (smaller)
    // layout total.
    let emitted_flags = *(raw_ptr.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *const u8);
    let emitted_compact = emitted_flags & cratonvm_types::GC_FLAG_COMPACT != 0;
    // The reserved footprint, for the accounting below. r9w2-vm2: through the
    // generation-validated per-thread layout cache, not `class_layout` -- this
    // runs once per inline allocation wherever the post-init call is not
    // elided (always under ZGC). A compact class's layout does not change
    // under live instances, so it is the total the emitter reserved.
    let compact_total = if emitted_compact {
        cratonvm_types::field_layout::with_current_class_layout(class_id_raw as u32, |l| {
            (l.field_count() == num_fields as usize).then_some(l.total_size)
        })
        .flatten()
    } else {
        None
    };
    //
    // zt t10/ca2: NOT on a register-first entry (`already_registered`). The
    // object is in the collector's registry already, so a mark or a
    // promotion may have set a bit in the flags byte since (`GC_FLAG_MARKED`
    // when the mark bits live in the header, `CRATONVM_ZGC_MARKBITS=0`;
    // `GC_FLAG_OLD_GEN`), and the whole-byte store would clear it. Nothing is
    // lost by skipping: both inline emitters wrote these exact values before
    // their commit (the flags byte with the mark word, a legacy instance's
    // shape and hash words), which is what makes the store idempotent here.
    if !already_registered {
        if emitted_compact {
            *(raw_ptr.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *mut u8) =
                cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER;
        } else {
            // CRIT (#23, BinTrees-18): the shape word is WRITTEN, never assumed
            // zero -- a TLAB slot was observed carrying prior `byte[]` data here.
            *(raw_ptr.add(cratonvm_types::NUM_SLOTS_OFFSET) as *mut u32) = num_fields as u32;
            *(raw_ptr.add(cratonvm_types::IDENTITY_HASH_OFFSET) as *mut u32) = 0;
        }
    }
    // NO IDENTITY-HASH STAMP HERE, and its absence is the fix.
    //
    // Until 2026-09-06 this read
    //
    //     let hash = vm.mem.heap.next_identity_hash();
    //     *(raw_ptr.add(8) as *mut i32) = hash;
    //
    // written against a 24-byte header in which offset 8 was a dedicated
    // `identity_hash_code: i32`. On 2026-08-07 that field was deleted and the
    // header shrank to 16 bytes: offset 8 became the MARK WORD (since the
    // 8-byte header it is at `cratonvm_types::MARK_WORD_OFFSET`, 4), whose low
    // two bits are the lock
    // STATE TAG and whose bits 2..32 are the hash -- `MARK_HASH_SHIFT`. The
    // store was never moved with the field, so it published a raw 31-bit hash
    // over the mark word's lock STATE TAG: three values in four left
    // `mark_state != MARK_NEUTRAL`, and the object was then read as
    // thin-locked, INFLATED (a monitor pointer synthesised out of hash bits --
    // the SIGSEGV in `displaced_hash_from_mark`) or FORWARDED. Under today's
    // 8-byte header the mark word is at `MARK_WORD_OFFSET` (4) and offset 8 is
    // a legacy instance's shape word or a compact instance's first field.
    //
    // The hash is installed LAZILY on first request, into the mark word and
    // through the mark word's own accessors (`identity_hash_code` ->
    // `java_identity_hash`), exactly as the interpreter's TLAB path leaves it:
    // `init_object_header` writes no hash either. There is nothing for this
    // helper to do about the hash, and the emitter agrees -- it writes the
    // whole mark word, neutral and hash-free, before the call.
    //
    // The bisection lever that restored the old store
    // (`CRATONVM_JIT_POST_TLAB_HASH_STAMP`) was deleted in round 11 wave 16
    // (lane tier; page `r11w15-hdr-helpers-header-diagnostics-patch`, item 3).
    // Under the 8-byte header, offset 8 is no longer the mark word: it is a
    // LEGACY instance's shape word (the store turned the field count into a
    // hash and every walker then mis-sized the object) and a COMPACT
    // instance's first field. It no longer reproduced the defect it was kept
    // for, and what it did produce was worse.

    // Family-A forensics (CRATONVM_DBG_A2, default-inert): record the
    // JIT-inline allocation into the a2dbg breadcrumb ring, exactly like the
    // interpreter TLAB fast path (`init_object_header`) and the gen_heap
    // allocators already do. Without this, the sweep's desync forensics
    // report "NO young alloc record" for every JIT-allocated object, which
    // makes the corrupt-header attribution ambiguous (allocated-then-clobbered
    // vs never-allocated-here).
    //
    // zt t11/ca3 (x8 C11): on the register-first HELPER arm (the start-bit
    // store refused) `jit_zgc_note_tlab_object` has recorded this object
    // already, so the ring holds it twice; on the inline arm this is its only
    // record. Kept on purpose: the two records are identical, a lookup returns
    // one per address, the cost is one ring slot under the default-inert
    // switch, and dropping this record on flagged entries would leave the
    // inline arm's objects with none.
    {
        let total = if let Some(total) = compact_total {
            total as usize
        } else {
            HEADER_SIZE + num_fields as usize * SLOT_SIZE
        };
        cratonvm_gc::a2dbg::record(
            raw_ptr as usize,
            class_id_raw as u32,
            0, // kind = Object
            0, // element_type = Reference
            compact_total.unwrap_or(0),
            num_fields as u32,
            total,
        );
    }

    // The header is complete from here on. ZGC needs every TLAB object in its
    // start registry before anything else can observe the address (the
    // registry is a mutator-path oracle there, not only the sweep's), and
    // this helper is the one call the inline allocator always makes -- so
    // this is where an inline-allocated object is registered. A no-op on the
    // backends whose sweeps parse the chunk linearly. `zgc/vm_tlab.rs`.
    // Unless the emitter registered it already, before its cursor commit
    // (zt t10/ca2, `already_registered` above).
    if !already_registered {
        let footprint = compact_total
            .map_or(HEADER_SIZE + num_fields as usize * SLOT_SIZE, |total| {
                total as usize
            });
        vm.mem.heap.note_tlab_object(raw_ptr, footprint);
    }

    // Primitive-typed default values + JLS §12.6 finalizer registration.
    // Kept here (rather than inlined) because the JIT cannot synthesise
    // per-field descriptor reads without pre-resolving the full layout at
    // compile time; the per-class recipe cache makes the steady state
    // lock-free (see `jit_post_alloc_init`).
    //
    // r9w3-vmhelpers3: the recipe is read HERE first and the out-of-line call
    // is skipped when it is empty — no primitive field, no finalizer, which is
    // every node/pair/wrapper class (bintrees' `Node`). Under ZGC this helper
    // runs once per inline allocation (`perf-zgc-compiled-new-always-takes-
    // the-rust-helper-20260918.md`), so that call was paid per object for
    // nothing. A class not yet in the cache still takes the call, which fills it.
    //
    // r9w7-ldc7: the typed handle is built only on that arm. `from_raw` records
    // the payload's provenance (`record_object_ref_payload_slow`, 5.2% of the
    // same profile), and a no-op class never uses the handle. The IR tier's
    // inline bump still calls this helper for every class, so this is its
    // share of the page's step 2 until it learns the announce mode.
    //
    // zki (page `zgc-zt25hunt7b-zr2cp-grow-cpu-*` item A): a COMPACT body
    // (the flag the emitter stamped) owes no default store at all -- its
    // long/float/double and reference cells read zero bytes as the typed
    // default -- which the single-pass emitter already knows at compile time
    // (`CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM`, `jit_new_site_flags`); the IR
    // tier's inline bump calls here for every class, and a `Double` paid two
    // generic `set_field` stores per `Double.valueOf` (~120 ns).
    let compact_noop = emitted_compact && jit_post_alloc_init_is_noop_compact(vm, class_id);
    if !compact_noop && !jit_post_alloc_init_is_noop(vm, class_id) {
        let obj_ref = cratonvm_types::ObjectRef::from_raw(raw_ptr);
        jit_post_alloc_init(vm, obj_ref, class_id);
    }

    if dbg_jit_alloc_filter() == Some(class_id_raw as u32) {
        eprintln!(
            "[JIT_ALLOC] post_tlab_init class_id={} obj=0x{:x}",
            class_id_raw, obj_ptr
        );
    }
    obj_ptr
}

/// ZGC register-only announce for an inline-TLAB object whose class needs no
/// post-init work — the thin sibling of [`jit_post_tlab_init`].
///
/// Round 9 wave 3 (vmhelpers3), for
/// `perf-zgc-compiled-new-always-takes-the-rust-helper-20260918.md` step 2 and
/// `NOTES-w3-x64obj3.md` cross-lane request 3 (whose contract this matches:
/// `(vm_ptr, obj_ptr, size_bytes) -> obj_ptr`, `0` = pending exception).
///
/// Under ZGC the emitter cannot skip the post-init call, because every TLAB
/// object must be entered in the collector's start registry before anything
/// can observe its address. For a class with no primitive default and no
/// finalizer (`helper_is_noop` in `op_object.rs`) that registration is ALL the
/// call has to do, and [`jit_post_tlab_init`] made it pay for everything else
/// too: a layout lookup to re-derive the shape the emitter already wrote, the
/// header re-stores, `ObjectRef` provenance recording, the init-recipe probe
/// and the debug gates — about 23% of `CratonBench bintrees` busy samples with
/// `CRATONVM_ZGC_JIT_TLAB=1` (`note_tlab_object` itself was another 24%).
///
/// # Contract with the emitter
///
/// * The header is COMPLETE when this is called: class id, shape word and the
///   mark word with its `GC_FLAG_HEADER`/`GC_FLAG_COMPACT` byte, exactly as the
///   non-ZGC skip path leaves it. Nothing here writes the object.
/// * `size_bytes` is what the inline bump advanced the TLAB cursor by — the
///   footprint `note_tlab_object` registers — never a re-derived size.
/// * Called only for a class the compile-time resolver proved needs no
///   primitive defaults and no finalizer registration. Anything else keeps
///   calling [`jit_post_tlab_init`].
///
/// Wired into `JitRuntimeHelpers::zgc_note_tlab_object` (ABI v13) by
/// `build_helpers`; the x64 emitter calls it through that slot.
///
/// # Safety
/// `vm_ptr` must be a valid `SharedVm` pointer and `obj_ptr` a freshly bumped,
/// fully headed TLAB object of `size_bytes` bytes from this thread's TLAB.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_zgc_note_tlab_object(
    vm_ptr: i64,
    obj_ptr: i64,
    size_bytes: i64,
) -> i64 {
    let f = || jit_zgc_note_tlab_object_body(vm_ptr, obj_ptr, size_bytes);
    contain("jit_zgc_note_tlab_object", OnPanic::Deopt, 0, f)
}

/// Body of [`jit_zgc_note_tlab_object`], run under its panic guard.
unsafe fn jit_zgc_note_tlab_object_body(vm_ptr: i64, obj_ptr: i64, size_bytes: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 || obj_ptr == 0 || size_bytes <= 0 {
        return obj_ptr;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // JIT round 21 wave 1 (lane alloc): the per-VM census
    // (`CRATONVM_DBG_JIT_METHOD_STATS`, `jit_zgc_note_calls=`): one entry per
    // inline ZGC allocation whose inline start-bit store refused. zt t12/ca4
    // counted the same entries (`jit_zgc_announce_calls=`); both keys read
    // this one counter (one cached-flag load per entry otherwise, as at
    // `jit_new_object`).
    vm.jit.jit_alloc_class_cache.new_paths.note_zgc_note();
    let raw_ptr = obj_ptr as *mut u8;
    let footprint = size_bytes as usize;
    // Same forensics ring `jit_post_tlab_init` feeds (default-inert gate).
    if cratonvm_gc::a2dbg::enabled() {
        let class_id_raw = std::ptr::read(raw_ptr as *const u32);
        let header = &*(raw_ptr as *const cratonvm_types::ObjectHeader);
        // zr z3/ja: the inline ARRAY bumps (both families, both tiers) announce
        // through this helper too; record an array as an array -- kind,
        // element type and length -- instead of the object row's `kind = 0`.
        if header.kind() == cratonvm_types::ObjectKind::Array {
            cratonvm_gc::a2dbg::record(
                raw_ptr as usize,
                class_id_raw,
                cratonvm_types::ObjectKind::Array as u8,
                header.element_type() as u8,
                header.array_length(),
                0,
                footprint,
            );
            vm.mem.heap.note_tlab_object(raw_ptr, footprint);
            return obj_ptr;
        }
        let num_slots = header.num_slots();
        let compact = *raw_ptr.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET)
            & cratonvm_types::GC_FLAG_COMPACT
            != 0;
        // The compact layout's TOTAL, header included, as `jit_post_tlab_init`
        // and the interpreter's `init_object_header` record it in this slot.
        // `footprint` is exactly that total (the emitter's bump); subtracting
        // the long `HEADER_SIZE` left this breadcrumb 16 bytes short of both
        // (round 11 wave 16, lane tier; page
        // `r11w15-hdr-helpers-header-diagnostics-patch`, item 1).
        let body = if compact {
            u32::try_from(footprint).unwrap_or(0)
        } else {
            0
        };
        cratonvm_gc::a2dbg::record(
            raw_ptr as usize,
            class_id_raw,
            0, // kind = Object
            0, // element_type = Reference
            body,
            num_slots,
            footprint,
        );
    }
    vm.mem.heap.note_tlab_object(raw_ptr, footprint);
    obj_ptr
}

/// Cached `CRATONVM_DBG_JIT_ALLOC` class-id filter (`None` = unset or
/// unparseable). PERF: the previous per-call `cratonvm_types::flags::runtime_var` in the two
/// allocation helpers was ~13% of binarytrees-18 wall time — getenv does a
/// linear scan of `environ` on every call.
fn dbg_jit_alloc_filter() -> Option<u32> {
    use std::sync::OnceLock;
    static F: OnceLock<Option<u32>> = OnceLock::new();
    *F.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_JIT_ALLOC")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
    })
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and num_fields must match the class metadata resolved at compile time.
// The returned i64 is a raw heap pointer to the newly allocated object.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// DBG (CRATONVM_DBG_TLABMISS): count why `jit_new_object` fell past the
/// TLAB arm — the bimodal-bt18 discriminator. reason: 0=oversized,
/// 1=guarded-refill returned None, 2=JIT_THREAD TLS null. Prints a running
/// breakdown every 2^20 misses.
#[inline]
fn dbg_tlabmiss(reason: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    if !cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TLABMISS")) {
        return;
    }
    static COUNTS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
    let n = COUNTS[reason].fetch_add(1, Ordering::Relaxed) + 1;
    let total = COUNTS[0].load(Ordering::Relaxed)
        + COUNTS[1].load(Ordering::Relaxed)
        + COUNTS[2].load(Ordering::Relaxed);
    if total & 0xFFFFF == 0 || (n == 1 && reason == 2) {
        eprintln!(
            "[tlabmiss] oversized={} refill_none={} jit_thread_null={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            COUNTS[2].load(Ordering::Relaxed),
        );
    }
}

/// Stash the pending exception for a `<clinit>` that failed under a JIT `new`.
///
/// Extracted so the memoized and un-memoized arms of the check cannot drift in
/// what they report -- the difference between them is meant to be *when* the
/// authoritative check runs, and nothing else. The caller then returns the
/// `0`/null sentinel, which the codegen's existing `emit_post_alloc_oom_check()`
/// guard already routes through the method's exception table.
fn stash_new_class_init_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    class_id_raw: i64,
) {
    let convert = helper_failure_conversion_on();
    stash_helper_failure(
        vm,
        thread,
        err,
        &format_args!("JIT new class_id {class_id_raw} failed to initialize"),
        convert,
        convert,
    );
}

/// A/B lever for the `new`-path class-init memo.
///
/// `CRATONVM_JIT=-new-class-init-memo` takes every allocation back through
/// `ensure_class_initialized_shared`. The OFF position is the pre-2026-08-25
/// behaviour: correct, and slower. It exists because what this change buys is a
/// RATE, and a rate measured across two binaries built minutes apart on a shared
/// host is not a measurement -- the arms have to interleave inside one
/// executable.
fn new_class_init_memo_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_NEW_CLASS_INIT_MEMO")
    })
}

pub unsafe extern "C" fn jit_new_object(vm_ptr: i64, class_id_raw: i64, num_fields: i64) -> i64 {
    let f = || jit_new_object_body(vm_ptr, class_id_raw, num_fields);
    contain("jit_new_object", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_new_object`], run under its panic guard.
unsafe fn jit_new_object_body(vm_ptr: i64, class_id_raw: i64, num_fields: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // gcg g1/p: compiled-`new` engagement (per VM; `CRATONVM_DBG_JIT_METHOD_STATS`).
    vm.jit.jit_alloc_class_cache.new_paths.note_new_object();
    let heap = &vm.mem.heap;
    let class_id = ClassId::new(class_id_raw as u32);
    // ZGC: the concurrent-start check the interpreter does in `maybe_gc`.
    jit_maybe_start_zgc_concurrent_mark(vm);
    // G1: the concurrent-mark lifecycle the interpreter drives from the same
    // place. See `jit_drive_g1_concurrent_mark`.
    jit_drive_g1_concurrent_mark(vm);
    // gcf f1/c: the Generational inline concurrent start (see the fn).
    jit_take_gen_inline_concurrent_start(vm);

    // JVMS §5.5 / §new: `new` must initialize its class before the object
    // is allocated -- same missing check, same shape of bug as the
    // `jit_getstatic`/`jit_putstatic_*` fixes above (see `jit_getstatic`'s
    // comment for the full hazard writeup). The interpreter's own `new`
    // handler (`runtime/interpreter.rs`, opcode `0xbb`) already calls
    // `ensure_class_initialized_shared` before it ever looks at
    // `num_total_fields`; this JIT slow-path helper never did, so a
    // JIT-compiled `new` that happens to be the first-ever touch of its
    // class (e.g. a rarely-taken `new`, reached only after the surrounding
    // method already tiered up to JIT via other branches) could allocate
    // and construct an instance of a class whose `<clinit>` — including any
    // static state the constructor itself reads — had not yet run.
    //
    // Run the check BEFORE any allocation-capacity probing below: a
    // `<clinit>` failure must be surfaced as the ordinary
    // `ExceptionInInitializerError` through the caller's exception table,
    // not after this helper has already committed heap state for an object
    // that will never be returned. Mirrors the existing OOM/negative-length
    // convention on this same helper family (`jit_alloc_oom`,
    // `jit_negative_array_size`): stash the exception via
    // `set_jit_pending_exception` and return the `0`/null sentinel, which
    // the codegen's EXISTING `emit_post_alloc_oom_check()` guard (already
    // emitted after every call site of this helper in `jit/src/x64.rs`,
    // 0xbb arm) already recognizes and routes through the exception table —
    // no codegen change needed for this fix, unlike `getstatic`/`putstatic`,
    // whose fallible-call convention had no existing post-call check at all.
    //
    // NOTE (scope): this only covers the ordinary allocation path. The inline
    // TLAB bump (both tiers) and a scalar-replaced `new` never call this
    // helper, so they are covered at COMPILE time instead (interpreter round
    // i1 wave 4, L7): a `new` whose class still owes initialization gets no
    // resolved row, and compiles to `jit_new_object_cp`, which ends here. See
    // `jit_bridge::jit_new_target_init_pending`.
    //
    // Behind `class_init_memo`, exactly as `jit_getstatic` and
    // `jit_putstatic_object` already are. The check is required on the FIRST
    // allocation of a class and is pure overhead on every one after it:
    // initialization is monotonic (JVMS §5.5 -- a class that has completed
    // initialization never returns to an uninitialized state, and redefinition
    // does not re-run `<clinit>`), so a confirmed "yes" needs no invalidation.
    //
    // What the memo saves is larger here than at `getstatic`, because this call
    // site pays for the answer twice over: `ensure_class_initialized_shared`
    // needs a `class_manager.read()` to reach the per-class atomic, and this
    // site needs a `jit_thread_mut()` acquire merely to have a thread to hand
    // it. The memo skips BOTH. `is_class_initialized_via_manager` +
    // `ensure_class_initialized_shared` were 12.2% of a whole-process profile
    // of an allocation loop -- see `osr-door-binds-ctor-and-the-inline-new-lever-is-inert-FIXED-20260817.md`.
    if new_class_init_memo_enabled() {
        if !class_init_memo::is_initialized(vm, class_id_raw as u32) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Err(err) = crate::vm::ensure_class_initialized_shared(vm, thread, class_id) {
                    stash_new_class_init_failure(vm, thread, err, class_id_raw);
                    return 0;
                }
                // Only a CONFIRMED success is memoized. A failed `<clinit>`
                // leaves the class in the erroneous state, and every later
                // attempt must throw from the real check rather than be waved
                // through by a stale bit. `Ok` alone is not confirmation: it
                // is also the answer for a class this thread is still
                // initializing, and the memo is VM-wide (see `jit_getstatic`).
                let generation = class_init_memo::generation(vm);
                if crate::vm::is_class_initialized_via_manager(vm, class_id) {
                    class_init_memo::mark_initialized(vm, class_id_raw as u32, generation);
                }
            }
        }
    } else if let Some((thread, _guard)) = jit_thread_mut() {
        if let Err(err) = crate::vm::ensure_class_initialized_shared(vm, thread, class_id) {
            stash_new_class_init_failure(vm, thread, err, class_id_raw);
            return 0;
        }
    }

    // CRIT (jit/gc audit, 2026-05): probe young-gen capacity BEFORE
    // allocating. If young gen would overflow, retire the calling
    // thread's TLAB and trigger an orchestrated STW GC so the next
    // allocation has room — mirrors `alloc_object_shared` in
    // `runtime/interpreter.rs:782`. The TLAB retire is critical: it
    // installs a synthetic `int[]` filler at the cursor so the heap
    // walker can stride over the unused tail in O(1) without
    // mis-decoding it.
    //
    // This is the slow-path entry — we're here because the inline-TLAB
    // bump in `emit_inline_tlab_new` failed (TLAB full / null thread)
    // OR because the caller went straight to the helper for an
    // over-sized object. In all cases the inline bump did not commit a
    // half-initialized object: the TLAB cursor in memory is the
    // last-allocated-object's end, so `retire()` here is safe.
    // The shape planner, not a bare legacy size. This site reserves the region
    // that `tlab_alloc_object_guarded_refill` stamps a header onto, so a legacy
    // reservation here with a compact header there is an object claiming a
    // smaller size than it was given -- which every header-strided heap walk
    // then misparses.
    let (total_size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
        vm,
        class_id,
        num_fields as usize,
        crate::runtime::interpreter::tlab_site::JIT_NEW,
    );
    // gen r4w4/alloc4: the lock-free bump-tail headroom first. This probe runs
    // on EVERY entry to this helper — i.e. once per TLAB refill of every
    // JIT-compiled `new`, the inline bump's only miss — and
    // `try_alloc_young_probe` takes the `young_from` mutex to read the same
    // two words `young_bump_headroom` reads from the published triple (gen
    // r4/alloc made that one lock-free for exactly this path's gate). Only a
    // bump tail too short for the object falls through to the locked probe,
    // which also consults the free list, so the verdict is the locked one's
    // whenever the tail does not decide it. G1 and ZGC answer both calls with
    // the same `!needs_gc()`.
    //
    // gcd d2/j: unless the futile-young backoff's window is open -- the last
    // forced young cycle at an object door left young unable to serve the
    // object (a young generation full of live objects under a non-moving
    // cycle), so this one would free nothing either and the object spills to
    // the old generation below regardless. One futile cycle per object was the
    // `oome-thread-exit` livelock
    // (`gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles-FIXED-20260928`).
    // `CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0` restores the cycle per entry.
    if !heap.young_bump_headroom(total_size)
        && heap.try_alloc_young_probe(total_size).is_none()
        && !crate::runtime::interpreter::futile_young_backoff_skips(vm)
    {
        // zr z3/ja: the decision's stamp (every collector since gci i48/zl2;
        // `CRATONVM_ZGC_DROP_STALE_GC_REQUESTS=0` stamps nothing). A dropped request -- a
        // collection completed since the verdict -- goes straight to the
        // allocation below, whose failure arm still collects.
        let stamp = crate::runtime::interpreter::gc_and_alloc::zgc_gc_request_stamp(vm);
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            // zr z3/t2: its own site name. On ZGC both young probes above are
            // `!needs_gc()`, so this door is an OCCUPANCY verdict, not a
            // failed allocation (`gc_and_alloc::ZGC_OCCUPANCY_FORCED_SITES`).
            if crate::runtime::interpreter::gc_and_alloc::maybe_gc_forced_stamped_pub_at(
                vm,
                thread,
                "jit-object-young-probe",
                stamp,
            ) {
                crate::runtime::interpreter::note_forced_young_cycle_outcome(vm, total_size);
            }
        }
    }

    // GC-overhead limit (see jit_newarray): bail to catchable OOM if forced GCs
    // keep freeing almost nothing, instead of death-spiralling on slivers --
    // after the soft-reference rung, which earns one ordinary retry only when
    // it reset the streak (`jit_latched_overhead_limit_throws`).
    // gcf f2/o: `graced` -- see `jit_newarray_body`; counted on success below.
    // JIT round 22 wave 12 (lane memory, `CRATONVM_JIT_ALLOC_LATCH_AFTER_FAILURE`,
    // default ON): not before an attempt -- the streak is consulted in the
    // failure arm below, after the fallible allocation failed, as in the
    // interpreter's ladder. `=0` restores this early exit.
    let mut graced = false;
    if crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
        && !crate::runtime::interpreter::gc_and_alloc::jit_alloc_latch_after_failure_enabled()
        && jit_latched_overhead_limit_throws(vm)
    {
        graced = jit_latched_grace(vm);
        if !graced {
            jit_oome_ladder_step(vm, "jit-new latched-exit-before-attempt throws");
            return jit_alloc_oom(
                vm,
                &format!(
                    "Java heap space (new_object class_id {class_id_raw} fields {num_fields})"
                ),
            );
        }
    }
    // JIT TLAB refill, round 2 (round 1 was reverted the same day: the
    // unconditional/naively-gated refill collapsed bt18 10.6s→40-460s from
    // young churn + per-allocation O(free-list) probes). This round routes
    // the slow path through `tlab_alloc_object_guarded_refill`, whose refill
    // arm only fires when young can supply the chunk WITHOUT a GC — an O(1)
    // bump-tail check plus the amortized-O(1) cached-bound early-exit
    // reclaimed-span probe (`young_has_free_block`) that round 1 lacked.
    // Post-sweep+coalesce young is a handful of big spans, so refills flow
    // at bump speed and the JIT's INLINE bump fast path comes back to life
    // for the following allocations; a genuinely-full young answers `false`
    // in O(1) and falls through to the historical old-gen spill below.
    if total_size <= cratonvm_gc::tlab::tlab_max_alloc() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(obj_ref) = crate::runtime::interpreter::tlab_alloc_object_guarded_refill(
                thread,
                vm,
                class_id,
                num_fields as usize,
                total_size,
            ) {
                if graced {
                    crate::runtime::interpreter::gc_and_alloc::note_latched_grace_outcome(vm, true);
                }
                jit_post_alloc_init(vm, obj_ref, class_id);
                if dbg_jit_alloc_filter() == Some(class_id_raw as u32) {
                    eprintln!(
                        "[JIT_ALLOC] new_object(tlab) class_id={} obj=0x{:x}",
                        class_id_raw,
                        obj_ref.as_ptr() as usize
                    );
                }
                return obj_ref.as_ptr() as i64;
            }
            dbg_tlabmiss(1); // guarded refill returned None
        } else {
            dbg_tlabmiss(2); // JIT_THREAD TLS pointer is null
        }
    } else {
        dbg_tlabmiss(0); // oversized for TLAB
    }
    // JIT round 21, lane alloc2: no TLAB object -- the census of the
    // "every allocation takes the helper" regime (`CRATONVM_DBG_JIT_METHOD_STATS`).
    vm.jit
        .jit_alloc_class_cache
        .new_paths
        .note_new_object_spill();
    // Fallible young → old-gen alloc (preserves alloc_object's old-gen spill).
    // A successful young-space probe above does not guarantee this allocation
    // will succeed: the TLAB refill may need a larger contiguous span than the
    // object itself, so it can fall through to an already-full old generation.
    // In that case force one orchestrated collection and retry before reporting
    // OOM.  This is essential for the JIT-active non-moving collector: its
    // in-place old sweep runs during that collection and can reclaim dead
    // promoted objects without relocating conservative JIT roots.
    // gcf f4/o: set when the fallible attempt below failed and the ladder ran
    // (`[GC] oome_ladder_doors:` `jit_*`).
    let mut laddered = false;
    let obj_ref = match heap.try_alloc_object_full(class_id, num_fields as usize) {
        Some(o) => o,
        None => {
            laddered = true;
            crate::runtime::interpreter::gc_and_alloc::note_ladder_door_entered(
                vm,
                crate::runtime::interpreter::gc_and_alloc::LadderDoor::Jit,
            );
            jit_oome_ladder_begin(vm, "door=jit-new");
            if let Some((thread, _guard)) = jit_thread_mut() {
                thread.tlab.retire();
                crate::runtime::interpreter::maybe_gc_forced_pub_at(vm, thread, "jit-helpers");
            }
            jit_oome_ladder_step(vm, "forced-collection");
            // A latched overhead limit still earns the retry once the
            // soft-reference rung has collected -- and reset the streak
            // (`jit_latched_overhead_limit_throws`, gen r5w2/oomjit6).
            // JIT round 22 wave 12 (`CRATONVM_JIT_ALLOC_LATCH_AFTER_FAILURE`):
            // the entry check stood down, so the latched exit's grace is
            // taken here instead (off G1; G1's exit decides its own).
            let mut may_attempt = !crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
                || !jit_latched_overhead_limit_throws(vm);
            if !may_attempt
                && crate::runtime::interpreter::gc_and_alloc::jit_alloc_latch_after_failure_enabled()
            {
                graced = jit_latched_grace(vm);
                may_attempt = graced;
            }
            jit_oome_ladder_step(
                vm,
                if may_attempt {
                    "jit-new attempt"
                } else {
                    "jit-new latched-exit throws"
                },
            );
            if may_attempt {
                if let Some(obj_ref) = heap.try_alloc_object_full(class_id, num_fields as usize) {
                    obj_ref
                } else {
                    let oom = || {
                        jit_alloc_oom(
                            vm,
                            &format!(
                                "Java heap space (new_object class_id {class_id_raw} fields {num_fields})"
                            ),
                        )
                    };
                    let between = jit_g1_last_ditch_attempting(vm, || {
                        heap.try_alloc_object_full(class_id, num_fields as usize)
                    });
                    match between {
                        Err(()) => return oom(),
                        Ok(Some(o)) => o,
                        Ok(None) => match heap.try_alloc_object_full(class_id, num_fields as usize) {
                            Some(o) => o,
                            None => return oom(),
                        },
                    }
                }
            } else {
                return jit_alloc_oom(
                    vm,
                    &format!(
                        "Java heap space (new_object class_id {class_id_raw} fields {num_fields})"
                    ),
                );
            }
        }
    };
    if graced {
        crate::runtime::interpreter::gc_and_alloc::note_latched_grace_outcome(vm, true);
    }
    if laddered {
        crate::runtime::interpreter::gc_and_alloc::note_ladder_door_allocated(
            vm,
            crate::runtime::interpreter::gc_and_alloc::LadderDoor::Jit,
        );
    }
    // Every arm reaching here allocated outside the TLAB (the TLAB arm above
    // returns early), so the thread's allocation counter has to be told.
    jit_note_external_alloc(vm, total_size);
    // Initialize primitive-typed fields to proper JVM default values (zero
    // memory reads as Object(None) which is wrong for int/long/float/double
    // fields) + JLS §12.6 finalizer registration.
    jit_post_alloc_init(vm, obj_ref, class_id);
    if dbg_jit_alloc_filter() == Some(class_id_raw as u32) {
        eprintln!(
            "[JIT_ALLOC] new_object class_id={} obj=0x{:x}",
            class_id_raw,
            obj_ref.as_ptr() as usize
        );
    }
    obj_ref.as_ptr() as i64
}

/// Stash `msg` as a `java/lang/InternalError` on the JIT pending-exception
/// channel and return the `0`/null allocation sentinel.
///
/// Shared by the two CP-indexed allocation helpers below for the failures that
/// are not a Java-level `MethodCallFailed` (a missing referencing class, a CP
/// entry that is not a class reference). Same convention as `jit_alloc_oom`:
/// the caller's `emit_post_alloc_oom_check()` sees the 0 and routes the stashed
/// exception through the method's exception table.
///
/// # Safety
/// Calls [`jit_thread_mut`]: same contract — invoke only from a JIT helper on
/// the thread that installed `JIT_THREAD`, with no other `&mut JvmThread` (and
/// no live `JitThreadGuard`) outstanding, and with NO class-manager lock held
/// (constructing the exception re-enters the class manager).
unsafe fn jit_cp_alloc_internal_error(vm: &SharedVm, msg: &str) -> i64 {
    if let Some((thread, _guard)) = jit_thread_mut() {
        if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/InternalError",
            Some(msg),
        ) {
            set_jit_pending_exception(thread, exc);
        }
    }
    0
}

/// Stash a `MethodCallFailed` on the JIT pending-exception channel and return
/// the `0`/null allocation sentinel. A thrown Java exception passes through
/// unchanged. Round 9 wave 6 (`vmhelp6`): a `VmError::Runtime` (for example
/// the `OutOfMemoryError` `alloc_multi_array` returns) and a `VmError::Linkage`
/// are built into THEIR Java throwable, as the interpreter's opcode-boundary
/// conversion and `handle_jit_dispatch_error` do, so `catch (OutOfMemoryError)`
/// / `catch (LinkageError)` behave the same in both tiers. Round 12 wave 4: the
/// stack-overflow shapes too (`CRATONVM_JIT_DISPATCH_SOE_THROWS`), through the
/// shared [`stash_helper_failure`]; a `NotImplemented` and every other internal
/// error are still reported as `java/lang/InternalError`.
fn jit_cp_alloc_stash_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    context: &str,
) -> i64 {
    stash_helper_failure(vm, thread, err, &context, true, true);
    0
}

/// The recorded resolution of `holder`'s `CONSTANT_Class` entry `cp_index`
/// (`constants::recorded_class_entry`), for the compiled class sites
/// (`jit_resolve_cp_class`, `jit_typecheck_holder_site_target`), which must
/// answer what the interpreter's class opcodes answer for the entry. A record
/// is written only for a class a user loader defined
/// (`constants::class_entry_record_applies`), so every other holder pays the
/// defining-loader side-table probe (a bit test) rather than a flag read and
/// a resolution-cache lock on each execution. Call with no class-manager
/// guard held. Round 15 wave 5, lane classsite2
/// (`r15w5-classsite-class-entry-shared-by-every-instruction-patch-FIXED-20260930.md`,
/// part C).
fn jit_recorded_class_entry(vm: &SharedVm, holder: ClassId, cp_index: u16) -> Option<ClassId> {
    cratonvm_native_builtins::classloader::defining_loader_for(
        &vm.loader_identity,
        holder.as_u32(),
    )?;
    crate::runtime::interpreter::constants::recorded_class_entry(vm, holder, cp_index)
}

/// Resolve a `new`/`anewarray` constant-pool site at RUN TIME and return the
/// resolved class id, or `None` after stashing the appropriate pending
/// exception (the caller then returns the `0`/null allocation sentinel).
///
/// This is the run-time half of the cold-`new` fix. The compile-time resolver
/// (`resolve_jit_new_site`) deliberately never loads a class — running a user
/// `ClassLoader.loadClass` from inside the compile path is a
/// deadlock/reentrancy hazard and would load classes the program never would.
/// So a `new` whose target class had not been loaded yet used to bail the whole
/// compile, permanently, leaving hot methods carrying a cold
/// `throw new SomeException(...)` in the interpreter forever
/// (`jit-compile-bail-unresolved-new-cold-class.md`).
///
/// Doing the same resolution HERE is sound for the reason the doc gives: it is
/// exactly what the interpreter's own `0xbb`/`0xbd` handler does — same thread,
/// same program point, same loader-faithful resolver, same access check. And it
/// costs nothing on the hot path: a site whose class IS loaded at compile time
/// never reaches this helper at all, and even here the second and later
/// executions take the `find_class_by_name_for_class` fast path with no Java on
/// the stack.
///
/// # Safety
/// Calls [`jit_thread_mut`]: invoke only from a JIT helper on the thread that
/// installed `JIT_THREAD`, with no other `&mut JvmThread` live.
unsafe fn jit_resolve_cp_class(
    vm: &SharedVm,
    site: CpSite,
    check_access: bool,
) -> Result<(ClassId, u16), i64> {
    let holder_cid = site.holder;
    // (1) CP text → binary name, plus the already-loaded fast path under one
    //     read lock. After the first execution of this site the class is loaded,
    //     so this is all that runs.
    //
    //     Every exit from this block DROPS the read lock first and only then
    //     builds an exception: `create_exception_object` re-enters the class
    //     manager, and a nested read while a writer is queued deadlocks (see
    //     the ClassManager nested-read reentrancy pattern). Hence the
    //     "compute a reason, report after the lock" shape rather than an early
    //     `return` from inside the block.
    //
    //     `fill_as_of` is read BEFORE that pool read: the failure records below
    //     are published only while no redefinition of the holder may have
    //     replaced the pool (`record_resolution_failure_as_of`; i22-L5,
    //     interpreter round i1 wave 24). One `Acquire` load.
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    // The entry's index in the pool as this guard holds it (`CpSite::index_in`,
    // interpreter round i1 wave 43, lane L2): every record below is keyed by it.
    let (cp_index, resolved_name): (u16, Result<(String, Option<ClassId>), String>) = {
        let cm = vm.classes.class_manager.read();
        let cp_index = site.index_in(&cm);
        let resolved = match cm.get_class(holder_cid) {
            None => Err(format!(
                "JIT CP allocation: referencing class id {} not found",
                holder_cid.as_u32()
            )),
            Some(class) => match class.constant_pool.get_class_name(cp_index) {
                None => Err(format!(
                    "JIT CP allocation: invalid class ref at cp#{cp_index} of class id {}",
                    holder_cid.as_u32()
                )),
                Some(name) => {
                    let name = name.to_string();
                    // Only what the holder's loader has defined or initiated:
                    // `find_class_by_name_for_class` would answer an
                    // uninitiated name with an ancestor's copy that a
                    // child-first loader is about to shadow with its own.
                    // A JDK-global name its child-first loader has not
                    // answered yet is resolved through the loader below
                    // (interpreter round i1 wave 37, lane L2).
                    let already = crate::runtime::interpreter::class_resolved_without_loading(
                        vm, &cm, holder_cid, &name,
                    )
                    .and_then(|id| {
                        crate::runtime::interpreter::jit_bridge::loader_faithful_global_name_answer(
                            vm, &cm, holder_cid, &name, id,
                        )
                    });
                    Ok((name, already))
                }
            },
        };
        (cp_index, resolved)
    };
    let (class_name, already_loaded) = match resolved_name {
        Ok(v) => v,
        Err(msg) => return Err(jit_cp_alloc_internal_error(vm, &msg)),
    };
    // The entry's recorded resolution answers before the memo: it is what
    // the interpreter's `new` of this entry answers, and the memo follows a
    // loader whose answer changed since (round 15 wave 5, lane classsite2;
    // `r15w5-classsite-class-entry-shared-by-every-instruction-patch-FIXED-20260930.md`,
    // part C). Read after the class-manager guard is dropped.
    let already_loaded = jit_recorded_class_entry(vm, holder_cid, cp_index).or(already_loaded);

    // (1b) JVMS §5.4.3: an entry whose resolution already failed — in either
    //      tier — rethrows the recorded error, even if the class has since
    //      become loadable. Same record and position as the interpreter's
    //      `ldc`/`new`/`anewarray` arms; one relaxed load when nothing failed.
    if crate::runtime::interpreter::constants::any_resolution_failure_recorded(vm) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(recorded) = crate::runtime::interpreter::recorded_resolution_failure(
                vm, thread, holder_cid, cp_index,
            ) {
                return Err(jit_cp_alloc_stash_failure(
                    vm,
                    thread,
                    recorded,
                    &format!("JIT CP allocation: {class_name} failed to resolve earlier"),
                ));
            }
        }
    }

    // (2) Not loaded yet — drive the same loader-faithful resolution the
    //     interpreter's `Instruction::New` does. `emit_post_alloc_oom_check`
    //     forces `has_dispatch` on every site that can reach here, so
    //     `JIT_THREAD` is set; the `None` arm is purely defensive.
    let target_id = match already_loaded {
        Some(id) => id,
        None => {
            let Some((thread, _guard)) = jit_thread_mut() else {
                return Err(jit_cp_alloc_internal_error(
                    vm,
                    &format!("JIT CP allocation: no live JIT thread to resolve {class_name}"),
                ));
            };
            // Through the entry's record, as the interpreter's `new` (round 15
            // wave 5, lane classsite2): an isolated URL loader is asked once
            // per entry, in either tier.
            match crate::runtime::interpreter::constants::resolve_class_entry_loader_aware(
                vm,
                thread,
                (holder_cid, cp_index),
                &class_name,
                fill_as_of,
            ) {
                Ok(id) => {
                    // JVMS §5.4.3: a failure another thread recorded for the
                    // entry meanwhile is its outcome, as in the interpreter's
                    // `new` (interpreter round i1 wave 38, lane L5;
                    // `--jdk-only`).
                    if let Some(recorded) =
                        crate::runtime::interpreter::recorded_resolution_failure_after_success(
                            vm, thread, holder_cid, cp_index,
                        )
                    {
                        return Err(jit_cp_alloc_stash_failure(
                            vm,
                            thread,
                            recorded,
                            &format!("JIT CP allocation: {class_name} failed to resolve meanwhile"),
                        ));
                    }
                    id
                }
                Err(e) => {
                    // The holder, not the top interpreter frame, is the
                    // initiating class (it decides the CNFE cause).
                    let converted = crate::runtime::exceptions::convert_class_not_found_for(
                        vm,
                        thread,
                        Some(holder_cid),
                        &class_name,
                        e,
                    );
                    // Record it (a `LinkageError` only) so every later
                    // execution, compiled or interpreted, rethrows it; a
                    // racing success recorded first is the entry's (t1 W14).
                    match crate::runtime::interpreter::constants::record_class_entry_failure_as_of(
                        vm, holder_cid, cp_index, converted, fill_as_of,
                    ) {
                        Ok(winner) => winner,
                        Err(converted) => {
                            return Err(jit_cp_alloc_stash_failure(
                                vm,
                                thread,
                                converted,
                                &format!("JIT CP allocation: cannot resolve {class_name}"),
                            ))
                        }
                    }
                }
            }
        }
    };

    // (2b) The class-path package step of the interpreter's class-constant
    //      doors (`ldc` / `anewarray`; `vm_util::check_class_path_package_unlinked_at_door`,
    //      interpreter round i4 wave 94): an unlinked application class of a
    //      package sealed by another JAR is HotSpot's `SecurityException` at its
    //      load, and a compiled site that resolves it here must not hand it out
    //      either. Neither recorded nor cached, as at the interpreter's doors.
    //      One `bool` load when `CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE` is off.
    //      Interpreter round i4 wave 95, lane L5 (the i92 page's second "Still
    //      open" item). A site whose class was loaded at COMPILE time never
    //      calls this helper; see that page's wave-95 note.
    if vm.classes.unlinked_doors_check_package {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Err(e) =
                crate::vm::check_class_path_package_unlinked_at_door(vm, thread, target_id)
            {
                return Err(jit_cp_alloc_stash_failure(
                    vm,
                    thread,
                    e,
                    &format!("JIT CP allocation: {class_name} refused by its package"),
                ));
            }
        }
    }

    // (3) JVMS 6.5 `new` / 5.4.4 access check — the interpreter's `new` handler
    //     runs it in every mode (`check_access`), and its `anewarray` / `ldc`
    //     handlers in every mode (`class_constant_access_enforced`, i9/i10-L2),
    //     so a site compiled through this helper must too. Same verdict and
    //     HotSpot message as the interpreter (`class_constant_access_denial_in`;
    //     the old `LinkageError::to_string()` added an `illegal access: `
    //     prefix the interpreter's `new` never printed). A non-`new` refusal
    //     is recorded against the entry, as the interpreter's is.
    let enforced = crate::runtime::interpreter::constants::class_constant_access_enforced(vm);
    if check_access || enforced {
        let denied = {
            let cm = vm.classes.class_manager.read();
            // Defensive inside: no denial is invented when either class is missing.
            crate::runtime::interpreter::constants::class_constant_access_denial_in(
                vm,
                &cm,
                holder_cid,
                &class_name,
                Some(target_id),
            )
        };
        if let Some(message) = denied {
            // HotSpot's `unnamed module @0x<hash>` (lane L4's shared helper;
            // no class-manager guard held here). Interpreter round i5 wave 100.
            let message =
                crate::vm::access_denial_with_unnamed_module_hash(vm, holder_cid, message);
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                    vm,
                    thread,
                    "java/lang/IllegalAccessError",
                    Some(&message),
                ) {
                    // `new`'s refusal too since t1 wave 8 (lane I1), as the
                    // interpreter's `op_new` (`record_new_class_access_denial`).
                    if !check_access
                        || crate::runtime::interpreter::constants::resolution_error_record_hotspot_enabled()
                    {
                        let _ =
                            crate::runtime::interpreter::constants::record_resolution_failure_as_of(
                                vm,
                                holder_cid,
                                cp_index,
                                crate::error::MethodCallFailed::ExceptionThrown(exc),
                                fill_as_of,
                            );
                    }
                    set_jit_pending_exception(thread, exc);
                }
            }
            return Err(0);
        }
    }

    Ok((target_id, cp_index))
}

/// Ways in [`CP_STAMP_CHECKED`]. A power of two.
const CP_STAMP_CHECKED_WAYS: usize = 16;

thread_local! {
    /// `(vm_identity, holder, stamp, count)` per way: at class-redefinition
    /// count `count`, code of `holder` compiled at constant-pool stamp
    /// `stamp` was found to index the holder's current pool (the holder had
    /// no redefinition after `stamp`). A zero `vm_identity` is an empty way.
    static CP_STAMP_CHECKED: [Cell<(usize, u32, u64, u64)>; CP_STAMP_CHECKED_WAYS] =
        const { [const { Cell::new((0, 0, 0, 0)) }; CP_STAMP_CHECKED_WAYS] };
}

/// The `(holder, cp index)` a compiled `(vm, holder, cp index)` helper site
/// names in its class's CURRENT constant pool (interpreter round i1 wave 20,
/// lane L1; `i19-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool`).
///
/// `holder_word` is `cratonvm_jit::cp_holder_word`: the holder class id,
/// and in its high half the class-redefinition count the code was compiled
/// at. A redefinition installs the class's new pool with the old constants
/// appended (`cratonvm_classloading::obsolete_code`), so a compiled body
/// still running after it (an OSR loop, a long method-entry body) indexes a
/// pool that now holds other constants at those indices. Its index is
/// translated through the class's `RedefinitionHistory`, as an interpreter
/// frame's code is (`interpreter::obsolete_frames`), BEFORE the recorded
/// resolution is probed: a record at the untranslated index may be the new
/// pool's constant.
///
/// One `Acquire` load and a compare for code compiled after every
/// redefinition in the process (and for a word with no stamp); otherwise
/// [`stale_cp_site_index`].
///
/// The answer is taken WITHOUT the class-manager guard, so it holds only for
/// the pool as it was when the count was read: a redefinition that begins
/// after that read and installs before the caller reads the pool leaves the
/// caller reading the new pool at the old index (interpreter round i1 wave
/// 43, lane L2: `JIT ldc: cp#19 ... holds Utf8("length")` on the host, under
/// load). A caller that reads the pool reads it through [`CpSite::index_in`]
/// under its guard instead; one that reads a record keyed by the answer
/// confirms it with [`CpSite::verdict_still_holds`] ([`CpSite::index_lock_free`]).
/// Since wave 44 (lane L2) no helper reads through it: the last one, the
/// holder-keyed type-check sites, judges under its guard
/// (`typecheck_site_index_in`); the tests keep it as the lock-free reading.
#[cfg(test)]
fn cp_site_index(vm: &SharedVm, holder_word: i64, cp_idx: i64) -> (ClassId, u16) {
    let site = CpSite::of(holder_word, cp_idx);
    (site.holder, site.index_lock_free(vm).0)
}

/// A compiled `(vm, holder, cp index)` helper site: the holder, the index the
/// compiled code baked, and the constant-pool stamp it was compiled at
/// (`cratonvm_jit::cp_holder_word`). Interpreter round i1 wave 43, lane L2
/// (`interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007`).
///
/// A compiled body keeps running after its class's redefinition (an OSR loop,
/// a body with no exit: `i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled`),
/// and its sites translate their indices through the class's history. The
/// translation must be judged against the pool the site then READS: under the
/// class-manager guard that read holds ([`Self::index_in`]), or lock-free and
/// confirmed after the read ([`Self::index_lock_free`] and
/// [`Self::verdict_still_holds`], a seqlock read of the redefinition count,
/// which a redefinition advances under the class-manager writer before it
/// replaces the pool).
#[derive(Clone, Copy)]
struct CpSite {
    holder: ClassId,
    /// The index the compiled code carries: into the pool of `stamp`.
    baked: u16,
    /// The class-redefinition count the code was compiled at; `None` when the
    /// word carries none.
    stamp: Option<u64>,
}

impl CpSite {
    /// The site a helper was handed.
    #[inline]
    fn of(holder_word: i64, cp_idx: i64) -> Self {
        // Cast: the helper ABI carries the packed word as an i64 bit pattern.
        let (holder, stamp) = cratonvm_jit::cp_holder_word_parts(holder_word as u64);
        Self {
            holder: ClassId::new(holder),
            baked: cp_idx as u16, // Cast: a constant-pool index is a u16 by construction
            stamp,
        }
    }

    /// The index in the holder's current pool, without the class-manager
    /// guard, and the class-redefinition count it was judged at. A caller
    /// that reads through it without the guard (a recorded-resolution probe)
    /// keeps the answer only when [`Self::verdict_still_holds`] afterwards.
    /// One `Acquire` load and a compare for code compiled after every
    /// redefinition in the process; otherwise [`stale_cp_site_index`].
    #[inline]
    fn index_lock_free(&self, vm: &SharedVm) -> (u16, u64) {
        let now = crate::classloading::class_redefinition_count();
        let idx = match self.stamp {
            Some(stamp) if stamp < now => stale_cp_site_index(vm, self.holder, stamp, self.baked),
            _ => self.baked,
        };
        (idx, now)
    }

    /// Did no redefinition begin since [`Self::index_lock_free`] judged at
    /// `judged_at`? The second read of a seqlock: the fence keeps the reads
    /// made through the answer before it.
    #[inline]
    fn verdict_still_holds(judged_at: u64) -> bool {
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        crate::classloading::class_redefinition_count() == judged_at
    }

    /// The index in the pool `cm` holds, judged under `cm`: a redefinition
    /// replaces the pool and records its history under one hold of the
    /// writer, so the answer and every read through the same guard agree.
    /// A history that no longer reaches the stamp leaves the index as it is,
    /// as [`stale_cp_site_index`] does.
    fn index_in(&self, cm: &crate::classloading::ClassManager) -> u16 {
        match self.stamp {
            Some(stamp) => match cm.redefinition_history(self.holder) {
                Some(history) if history.is_stale(stamp) => {
                    history.translate(stamp, self.baked).unwrap_or(self.baked)
                }
                _ => self.baked,
            },
            None => self.baked,
        }
    }
}

/// `cp_site_index` for code compiled before some redefinition in the
/// process: `idx` unchanged when `holder` itself was not redefined after
/// `stamp` (remembered per thread until the count moves again), translated
/// through the holder's history when it was. A history that no longer
/// reaches `stamp` leaves `idx` as it is, which is what an interpreter frame
/// in that position does too.
#[cold]
#[inline(never)]
fn stale_cp_site_index(vm: &SharedVm, holder: ClassId, stamp: u64, idx: u16) -> u16 {
    let now = crate::classloading::class_redefinition_count();
    let key = (vm.vm_identity, holder.as_u32(), stamp);
    let way = ((u64::from(holder.as_u32()) ^ stamp).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60)
        as usize
        & (CP_STAMP_CHECKED_WAYS - 1);
    let checked = CP_STAMP_CHECKED
        .try_with(|ways| {
            let (vm_id, h, s, at) = ways[way].get();
            (vm_id, h, s) == key && at == now
        })
        .unwrap_or(false);
    if checked {
        return idx;
    }
    let cm = vm.classes.class_manager.read();
    match cm.redefinition_history(holder) {
        Some(history) if history.is_stale(stamp) => history.translate(stamp, idx).unwrap_or(idx),
        _ => {
            let _ = CP_STAMP_CHECKED.try_with(|ways| ways[way].set((key.0, key.1, key.2, now)));
            idx
        }
    }
}

/// `cp_site_index` for a `multianewarray` site word
/// (`cratonvm_jit::pack_multianewarray_site`), whose stamp is truncated to 16
/// bits because the word also carries the cp index
/// (`cratonvm_jit::multianewarray_site_stamp` rebuilds it; interpreter round
/// i1 wave 21, lane L1,
/// `interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`). One `Acquire`
/// load and a compare for code compiled after every redefinition in the
/// process, and for a word with no stamp.
///
/// Answers the site (its stamp rebuilt against the count read here), the
/// index lock-free and the count it was judged at ([`CpSite::index_lock_free`]).
/// The index keys only a probe the caller confirms with
/// [`CpSite::verdict_still_holds`]; the pool is read at [`CpSite::index_in`]
/// under its guard (`interpreter::multianewarray_alloc_at_site`; interpreter
/// round i1 wave 44, lane L2).
#[inline]
fn multianewarray_site(vm: &SharedVm, site: i64) -> (CpSite, u16, u64) {
    let (holder, baked) = cratonvm_jit::unpack_multianewarray_site(site);
    let now = crate::classloading::class_redefinition_count();
    let cp_site = CpSite {
        holder: ClassId::new(holder),
        baked,
        stamp: cratonvm_jit::multianewarray_site_stamp(site, now),
    };
    let (idx, judged_at) = cp_site.index_lock_free(vm);
    (cp_site, idx, judged_at)
}

/// The `(holder, cp index)` a holder-keyed type-check site
/// (`cratonvm_jit::typecheck_holder_word_for_site`) keys its JVMS §5.4.3
/// resolution record by: `cp_site_index` of the interned holder word
/// (interpreter round i1 wave 21, lane L1,
/// `interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`), taken only when
/// the translated entry names the site's class. The stamp is the one the
/// pointer was first interned with, and the pointer is shared by every compile
/// of the same `(name, holder, index)` (and, holder ids being per VM, by
/// another VM's); the check keeps a translation that does not fit this code
/// from keying another constant's record. Otherwise the index as it is.
///
/// Judged under `cm`, the guard the caller reads the pool under
/// ([`CpSite::index_in`]; interpreter round i1 wave 44, lane L2): judged
/// lock-free, a redefinition beginning after the count read and installing
/// before the check keyed the record by the untranslated index, i.e. by
/// another entry of the new pool
/// (`interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008`).
fn typecheck_site_index_in(
    cm: &crate::classloading::ClassManager,
    holder_word: u64,
    idx: u16,
    class_name: &str,
) -> (ClassId, u16) {
    // Cast: the helper ABI's bit pattern, as compiled code passes it.
    let site = CpSite::of(holder_word as i64, i64::from(idx));
    let (holder, translated) = (site.holder, site.index_in(cm));
    if translated == idx {
        return (holder, idx);
    }
    let names_the_site_class = cm
        .get_class(holder)
        .and_then(|class| class.constant_pool.get_class_name(translated))
        .is_some_and(|name| name == class_name);
    (
        holder,
        if names_the_site_class {
            translated
        } else {
            idx
        },
    )
}

/// CP-indexed `new` (0xbb) slow path — see [`jit_resolve_cp_class`] for why
/// this exists and `jit_api::JitRuntimeHelpers::new_object_cp` for the ABI.
///
/// Resolves + access-checks the target class, then falls into
/// [`jit_new_object`], which owns `<clinit>`, the TLAB/GC retry ladder and the
/// `0`-on-failure convention. Splitting it this way means the deferred site
/// behaves identically to a resolved one from the allocation onwards.
///
/// SAFETY: called from JIT-compiled code; `vm_ptr` must be a valid `SharedVm`
/// pointer and `holder_class_id`/`cp_idx` must be the compile-time-baked
/// referencing class and constant-pool index of this `new` site.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_new_object_cp(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64 {
    let f = || jit_new_object_cp_body(vm_ptr, holder_class_id, cp_idx);
    contain("jit_new_object_cp", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_new_object_cp`], run under its panic guard.
unsafe fn jit_new_object_cp_body(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64 {
    if vm_ptr == 0 {
        return 0;
    }
    // Cross the Rust<->JIT boundary BEFORE resolving, not just before
    // allocating: resolution can run a user `ClassLoader.loadClass`, i.e.
    // arbitrary Java that can itself GC. `jit_new_object` does both of these
    // too, but it only runs after resolution — leaving the per-thread JIT-scan
    // cache stale, and the SATB buffer unflushed, for the whole classloader
    // call.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code that received it from the
    // interpreter's SharedVm reference (same contract as `jit_new_object`).
    let vm = &*(vm_ptr as *const SharedVm);
    // The resolution below judges the site's index under the guard it reads
    // the pool under (`CpSite::index_in`, interpreter round i1 wave 43, lane
    // L2). The memo is consulted only while no class was ever redefined, when
    // the baked index is the current one.
    let site = CpSite::of(holder_class_id, cp_idx);
    let (holder_cid, idx) = (site.holder, site.baked);
    // Round 11 wave 18 (lane hashmap): JVMS §5.4.3 -- the entry resolves ONCE.
    // A site compiled `Deferred` is not always cold: a method first compiled
    // before any instance of its `new` target existed (a callee compiled at a
    // compiled caller's inline-cache miss, before the callee ever ran -- the
    // `LinkedHashMap.newNode` -> `new LinkedHashMap$Entry` shape) allocates
    // through this helper on EVERY execution, and the resolution below is a
    // CP name copy, three class-manager read locks and an access check each
    // time. The epoch is read BEFORE the resolution, so a definition racing it
    // stamps the entry stale rather than current; see `NewCpSiteMemo`. The
    // memo is off once any class was redefined, so `idx` is the site's own
    // index whenever it is consulted. Only a site that allocates is filled:
    // the `InstantiationError` arm below returns before the fill.
    let memo_on = new_cp_site_memo_enabled() && !crate::classloading::any_class_redefined();
    let epoch = crate::classloading::class_definition_epoch();
    let holder_raw = holder_cid.as_u32();
    if memo_on {
        if let Some((target, num_fields)) =
            new_cp_site_memo_probe(vm.vm_identity, epoch, holder_raw, idx)
        {
            return jit_new_object(vm_ptr, i64::from(target), i64::from(num_fields));
        }
    }
    let target_id = match jit_resolve_cp_class(vm, site, true) {
        Ok((id, _)) => id,
        Err(sentinel) => return sentinel,
    };
    let (num_fields, refusal) = {
        let cm = vm.classes.class_manager.read();
        let class = cm.get_class(target_id);
        (
            class.map(|c| c.num_total_fields).unwrap_or(0),
            // JVMS 6.5: `InstantiationError` for an interface or abstract
            // class, after resolution and before initialization, as the
            // interpreter's `op_new` raises it (wave 16). The compile-time
            // `new` row defers such a site to this helper.
            class.and_then(crate::runtime::interpreter::gc_and_alloc::new_instantiation_refusal),
        )
    };
    if let Some((error_class, message)) = refusal {
        if let Some((thread, _guard)) = jit_thread_mut() {
            let err = match crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                error_class,
                Some(&message),
            ) {
                Ok(exc) => crate::error::MethodCallFailed::ExceptionThrown(exc),
                Err(e) => e,
            };
            return jit_cp_alloc_stash_failure(vm, thread, err, "JIT CP allocation");
        }
        return 0;
    }
    if memo_on {
        if let Ok(fields) = u32::try_from(num_fields) {
            new_cp_site_memo_fill(NewCpSiteMemo {
                vm_identity: vm.vm_identity,
                epoch,
                holder: holder_raw,
                cp_idx: idx,
                target: target_id.as_u32(),
                num_fields: fields,
            });
        }
    }
    jit_new_object(vm_ptr, i64::from(target_id.as_u32()), num_fields as i64)
}

/// The DEFERRED compiled field site (interpreter round i4 wave 92, lane L2;
/// `cratonvm_jit::DirectHelperTable::field_site_cp`, switch
/// `CRATONVM_JIT_DEFERRED_FIELD_SITES`, default OFF): a `getstatic` /
/// `putstatic` / `getfield` / `putfield` whose field the compile door could
/// not resolve (typically a cold branch touching a class not loaded when the
/// method compiled), resolved at its execution instead of refusing the method.
///
/// `(vm_ptr, cp_holder_word(holder), packed, operands)`; `packed` is
/// `cp_index | opcode << 16 | descriptor tag << 24 | NPE trap key << 32`, and
/// `operands` the frame block the emitter homes (`op_field.rs`
/// `emit_deferred_field_site`): `[writer name C string | 0, objectref or
/// putstatic value, putfield value]`, read only AFTER resolution and
/// initialisation, which can run Java and move the objects (the words are
/// published GC homes the collector rewrites).
///
/// What runs, in the interpreter's order: the loader-aware resolution and the
/// §6.5 linking checks ([`crate::runtime::interpreter::link_deferred_field_site`]),
/// then the access through the resolved helpers -- `jit_getstatic` (which
/// initialises the owner and serves the `System` streams), the owner's
/// initialisation then `jit_putstatic_*`, `jit_getfield` (its null receiver is
/// the NPE with the trap site's JEP 358 message), or the null check then
/// `jit_putfield_*` -- so the value encodings are the resolved arms'. A
/// failure is stashed and answered `i64::MIN`. The per-execution cost is the
/// resolution's per-thread site cache hit plus the access helper.
///
/// # Safety
/// Called from JIT-compiled code on the thread that installed `JIT_THREAD`;
/// `operands` must be the block the emitter laid out for this opcode.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_field_site_cp(
    vm_ptr: i64,
    holder_word: i64,
    packed: i64,
    operands: i64,
) -> i64 {
    let f = || jit_field_site_cp_body(vm_ptr, holder_word, packed, operands);
    contain("jit_field_site_cp", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of [`jit_field_site_cp`], run under its panic guard.
unsafe fn jit_field_site_cp_body(vm_ptr: i64, holder_word: i64, packed: i64, operands: i64) -> i64 {
    if vm_ptr == 0 || operands == 0 {
        // JIT round 22, lane sync (r10 help2's sentinel rule, `helper_guard`'s
        // "Two `i64::MIN` sentinel checks"): an unreachable arm's sentinel
        // carries the deopt flag, or the outer gate returns it as the value.
        set_jit_deopt_pending();
        return i64::MIN;
    }
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr is the SharedVm the compiled code was handed.
    let vm = &*(vm_ptr as *const SharedVm);
    // Cast: the packed word's fields, by construction (`emit_deferred_field_site`).
    let raw = packed as u64;
    let cp_raw = (raw & 0xFFFF) as u16;
    let opcode = ((raw >> 16) & 0xFF) as u8;
    // Bits 24..32 carry the site's descriptor tag; the resolved field's own
    // descriptor byte decides the access below.
    let npe_key = (raw >> 32) as u32;
    // Cast: the operand block's frame address, carried as a word.
    let ops = operands as usize as *const i64;
    let site = CpSite::of(holder_word, i64::from(cp_raw));
    let holder = site.holder;
    // The index in the holder's CURRENT pool (a body still running after its
    // class was redefined; `CpSite::index_in`).
    let cp_index = {
        let cm = vm.classes.class_manager.read();
        site.index_in(&cm)
    };
    // ops[0]: the writing method's name, for a store's final-field message.
    let writer_ptr = *ops;
    let writer: &str = if writer_ptr == 0 {
        ""
    } else {
        // SAFETY: an interned, process-lived NUL-terminated string
        // (`cratonvm_jit::intern_catch_type_name`).
        std::ffi::CStr::from_ptr(writer_ptr as usize as *const std::ffi::c_char)
            .to_str()
            .unwrap_or("")
    };
    let field = {
        let Some((thread, _guard)) = jit_thread_mut() else {
            // As the null-`vm_ptr` arm above (JIT round 22, lane sync).
            set_jit_deopt_pending();
            return i64::MIN;
        };
        match crate::runtime::interpreter::link_deferred_field_site(
            vm, thread, holder, cp_index, opcode, writer,
        ) {
            Ok(field) => field,
            Err(err) => {
                stash_helper_failure(
                    vm,
                    thread,
                    err,
                    &format_args!(
                        "JIT deferred field site class_id {} cp#{cp_index} failed to link",
                        holder.as_u32()
                    ),
                    true,
                    true,
                );
                return i64::MIN;
            }
        }
    };
    // Interpreter round i5 wave 100, lane L2
    // (`CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER`, default OFF): the site
    // linked, so an optimizing recompile armed for this body now finds the row.
    deferred_field_site_retier_take(vm);
    // JIT round 21, lane tcshape (`CRATONVM_JIT_DEFERRED_FIELD_RECOMPILE`):
    // the site linked, so a recompile of the bodies deferring it binds the
    // access; ask it once per site. A compile that reads the owner before
    // the access below finished initialising it builds the helper read and
    // waits on the class's `<clinit>` end instead (static-init waiters).
    crate::runtime::interpreter::jit_bridge::deferred_field_site_linked(
        vm,
        holder.as_u32(),
        cp_raw,
    );
    let owner = i64::from(field.declaring_class_id.as_u32());
    // Cast: a field slot index.
    let index = field.field_index as i64;
    let narrow = |v: i64| -> i64 {
        // JVMS §6.5: a sub-int field stores only its own width (as
        // `emit_narrow_to_field_tag` does for a resolved site).
        // Cast: the narrowing conversions themselves.
        match field.desc_byte {
            b'B' => i64::from(v as i8),
            b'S' => i64::from(v as i16),
            b'C' => i64::from(v as u16),
            b'Z' => v & 1,
            _ => v,
        }
    };
    match opcode {
        0xb2 => jit_getstatic(vm_ptr, owner, index),
        0xb3 => {
            // Initialise before the value is read: `<clinit>` can move it.
            if let Some(sentinel) = jit_putstatic_class_init_guard(vm, owner) {
                return sentinel;
            }
            let value = *ops.add(1);
            // JMM: a volatile store is a release too -- the leading fence
            // `op_putstatic` takes (and the aarch64 backend's `DMB ISH`), so
            // no earlier access is reordered past the store.
            if field.is_volatile {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            }
            let answer = match field.desc_byte {
                b'J' => jit_putstatic_long(vm_ptr, owner, index, value),
                b'F' => jit_putstatic_float(vm_ptr, owner, index, value),
                b'D' => jit_putstatic_double(vm_ptr, owner, index, value),
                b'L' | b'[' => jit_putstatic_object(vm_ptr, owner, index, value),
                _ => jit_putstatic_int(vm_ptr, owner, index, narrow(value)),
            };
            if field.is_volatile {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            }
            answer
        }
        0xb4 => {
            let receiver = *ops.add(1);
            let is_ref = matches!(field.desc_byte, b'L' | b'[');
            // Cast: `field_index` fits the argument's index field by construction
            // of a class's slot count.
            let arg = cratonvm_jit_api::getfield_index_arg(index as u32, is_ref, false, npe_key);
            // Cast: the flag bits sit at 61/62, so the value stays positive.
            jit_getfield(vm_ptr, receiver, arg as i64)
        }
        0xb5 => {
            let receiver = *ops.add(1);
            let value = *ops.add(2);
            if !cratonvm_types::plausible_heap_pointer(receiver as u64) {
                // JVMS §putfield: a null objectref, after resolution.
                set_jit_pending_npe_action_at(
                    crate::runtime::exceptions::helpful_npe::jit_action::NONE,
                    npe_key,
                );
                return i64::MIN;
            }
            // Release half of a volatile store (see the `putstatic` arm).
            if field.is_volatile {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            }
            match field.desc_byte {
                b'J' => jit_putfield_long(receiver, index, value),
                b'F' => jit_putfield_float(receiver, index, value),
                b'D' => jit_putfield_double(receiver, index, value),
                b'L' | b'[' => jit_putfield_object(vm_ptr, receiver, index, value),
                _ => jit_putfield_int(receiver, index, narrow(value)),
            }
            if field.is_volatile {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            }
            0
        }
        _ => i64::MIN,
    }
}

/// Proposal A of `i94-L2-proposal-deferred-field-site-retier-and-self-patch`
/// (interpreter round i5 wave 100, lane L2;
/// `CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER`, default OFF): a deferred field
/// site just linked in the compiled body below this helper. When that body is
/// one an optimizing task fell through to
/// (`jit_bridge::arm_deferred_field_site_retier`), take its row and ask ONE
/// more optimizing compile of the method through the late-bind C2 retry door:
/// the site's owner is loaded now, so the recompile's field peek finds the row
/// the first build bailed on. One shot per armed body (the row is removed);
/// a body that falls through again is armed again under its new compile id,
/// and the retry door's own per-method budget bounds the chain.
///
/// Cost while nothing is armed (always, with the switch off): four relaxed
/// loads. A body that is not armed but shares a filter bit pays the TLS read
/// of the innermost compile id and one lock.
#[cold]
fn deferred_field_site_retier_take(vm: &SharedVm) {
    use std::sync::atomic::Ordering;
    let bridge = &vm.jit.bridge;
    if bridge
        .deferred_field_retier_filter
        .iter()
        .all(|w| w.load(Ordering::Relaxed) == 0)
    {
        return;
    }
    let Some((compile_id, _, _)) = crate::jit::conservative_roots::innermost_compiled_site_key()
    else {
        return;
    };
    let (word, mask) =
        crate::runtime::interpreter::jit_bridge::deferred_field_retier_filter_bit(compile_id);
    if bridge.deferred_field_retier_filter[word].load(Ordering::Acquire) & mask == 0 {
        return;
    }
    let key = {
        let Ok(mut map) = bridge.deferred_field_retier.lock() else {
            return;
        };
        let key = map.remove(&compile_id);
        if key.is_some() {
            crate::runtime::interpreter::jit_bridge::publish_deferred_field_retier_filter(
                bridge, &map,
            );
        }
        key
    };
    let Some((key, osr_entry_bci)) = key else {
        return;
    };
    // Interpreter round i5 wave 102, lane L2
    // (`CRATONVM_JIT_OSR_DEFERRED_FIELD_SITE_RETIER`): an armed OSR body asks
    // one OSR compile at its entry bci instead.
    if let Some(entry_bci) = osr_entry_bci {
        let answer =
            crate::runtime::interpreter::jit_bridge::request_osr_deferred_field_site_retier(
                vm, &key, entry_bci,
            );
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr deferred-field-site retier {}.{}{} entry_bci={entry_bci} compile_id={compile_id}: {}",
                key.class_name,
                key.method_name,
                key.descriptor,
                match answer {
                    Some(crate::jit::tiered::OsrRequest::Queued(_)) => "queued",
                    Some(crate::jit::tiered::OsrRequest::Pending) => "pending",
                    Some(crate::jit::tiered::OsrRequest::Refused) => "refused",
                    None => "not asked (no background compiler)",
                }
            );
        }
        return;
    }
    // As the inline-cache profile seed's recompile: only a background compiler
    // with the C2 supersede publishes over the single-pass body.
    let answer =
        if crate::runtime::env_cache::bg_compile() && crate::runtime::env_cache::c2_supersede() {
        Some(vm.jit.tiered_manager.request_late_bind_recompile(&key))
    } else {
        None
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] deferred-field-site retier {}.{}{} compile_id={compile_id}: {}",
            key.class_name,
            key.method_name,
            key.descriptor,
            match answer {
                Some(a) => format!("{a:?}"),
                None => "not asked (no background compiler or no C2 supersede)".to_string(),
            }
        );
    }
}

/// `CRATONVM_JIT_NEW_CP_SITE_MEMO=0` -- resolve every deferred compiled `new`
/// site from its constant pool on every execution again, as before round 11
/// wave 18. Default ON.
fn new_cp_site_memo_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_NEW_CP_SITE_MEMO").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// One remembered resolution of a deferred compiled `new` site
/// ([`jit_new_object_cp`]): `(holder class, CP index)` of this VM resolved to
/// `target`, a class with `num_fields` fields, at class-definition epoch
/// `epoch`.
///
/// # Why it is sound
///
/// * JVMS §5.4.3: a symbolic class reference resolves once, and a successful
///   resolution (the §5.4.4 access check included) is the answer every later
///   execution of the entry gets. Only a SUCCESS is remembered; a failure
///   keeps its own record (`record_resolution_failure`) and its own path.
/// * The key names the holder by class id and the entry by index. Both stop
///   meaning the same thing only if the class-id -> class mapping changes (an
///   unload and a reuse of the id, a second definition), which moves
///   `class_definition_epoch`, or if the holder is redefined in place, which a
///   redefinition-free run never does -- the memo is off once
///   `any_class_redefined` is set.
/// * `<clinit>`, the abstract/interface refusal and the allocation itself
///   still run on every execution: they are [`jit_new_object`]'s, which a hit
///   calls exactly as a miss does.
#[derive(Clone, Copy)]
struct NewCpSiteMemo {
    vm_identity: usize,
    epoch: u64,
    holder: u32,
    cp_idx: u16,
    target: u32,
    num_fields: u32,
}

/// Direct-mapped, per thread. A collision only costs the resolution.
const NEW_CP_SITE_MEMO_SLOTS: usize = 64;

thread_local! {
    static NEW_CP_SITE_MEMO: std::cell::RefCell<[Option<NewCpSiteMemo>; NEW_CP_SITE_MEMO_SLOTS]> =
        const { std::cell::RefCell::new([None; NEW_CP_SITE_MEMO_SLOTS]) };
}

fn new_cp_site_memo_index(holder: u32, cp_idx: u16) -> usize {
    let mixed = ((u64::from(holder) << 16) | u64::from(cp_idx)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // Cast: the high half of a u64 fits a usize on every supported target.
    ((mixed >> 32) as usize) % NEW_CP_SITE_MEMO_SLOTS
}

fn new_cp_site_memo_probe(
    vm_identity: usize,
    epoch: u64,
    holder: u32,
    cp_idx: u16,
) -> Option<(u32, u32)> {
    let slot = new_cp_site_memo_index(holder, cp_idx);
    NEW_CP_SITE_MEMO
        .try_with(|memo| {
            memo.borrow()[slot]
                .filter(|m| {
                    m.vm_identity == vm_identity
                        && m.epoch == epoch
                        && m.holder == holder
                        && m.cp_idx == cp_idx
                })
                .map(|m| (m.target, m.num_fields))
        })
        .ok()
        .flatten()
}

fn new_cp_site_memo_fill(entry: NewCpSiteMemo) {
    let slot = new_cp_site_memo_index(entry.holder, entry.cp_idx);
    let _ = NEW_CP_SITE_MEMO.try_with(|memo| {
        memo.borrow_mut()[slot] = Some(entry);
    });
}

#[cfg(test)]
mod r11w18_hashmap_new_cp_site_memo_tests {
    use super::*;

    fn entry(holder: u32, cp_idx: u16, epoch: u64) -> NewCpSiteMemo {
        NewCpSiteMemo {
            vm_identity: 0x51,
            epoch,
            holder,
            cp_idx,
            target: holder + 1000,
            num_fields: 5,
        }
    }

    #[test]
    fn a_filled_site_answers_its_own_key_only() {
        new_cp_site_memo_fill(entry(4242, 17, 9));
        assert_eq!(new_cp_site_memo_probe(0x51, 9, 4242, 17), Some((5242, 5)));
        assert_eq!(
            new_cp_site_memo_probe(0x52, 9, 4242, 17),
            None,
            "another VM"
        );
        assert_eq!(
            new_cp_site_memo_probe(0x51, 10, 4242, 17),
            None,
            "a class was defined"
        );
        // Another site of the same holder: a miss even if it shares the slot.
        assert_ne!(new_cp_site_memo_probe(0x51, 9, 4242, 18), Some((5242, 5)));
    }

    #[test]
    fn a_colliding_fill_replaces_rather_than_aliases() {
        // Find a second key that maps to the same slot as (7, 3).
        let slot = new_cp_site_memo_index(7, 3);
        let other = (0u16..u16::MAX)
            .map(|cp| (8u32, cp))
            .find(|&(h, cp)| new_cp_site_memo_index(h, cp) == slot)
            .expect("64 slots: some index of holder 8 collides");
        new_cp_site_memo_fill(entry(7, 3, 1));
        new_cp_site_memo_fill(entry(other.0, other.1, 1));
        assert_eq!(new_cp_site_memo_probe(0x51, 1, 7, 3), None);
        assert_eq!(
            new_cp_site_memo_probe(0x51, 1, other.0, other.1),
            Some((other.0 + 1000, 5))
        );
    }

    #[test]
    fn every_index_is_in_range() {
        for holder in [0u32, 1, 4096, u32::MAX] {
            for cp in [0u16, 1, 255, u16::MAX] {
                assert!(new_cp_site_memo_index(holder, cp) < NEW_CP_SITE_MEMO_SLOTS);
            }
        }
    }

    /// A text pin (the helper cannot be driven without compiled code): the
    /// virtual-MIC helper's miss arm throttles a declined probe and hands a
    /// body the PIC had no room for to the per-thread entry cache, and its
    /// megamorphic arm refuses a superseded or retired way. Round 11 wave 18.
    #[test]
    fn the_mic_miss_arm_does_not_repeat_its_resolution_per_call() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = src
            .find("\nunsafe fn jit_invoke_virtual_mic_body(")
            .expect("the MIC body exists");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
        let miss = &body[body
            .find("// --- Cache miss: full resolution + update cache ---")
            .expect("the miss arm exists")..];
        assert!(
            miss.contains("MIC_COMPILE_DECLINED.with("),
            "the miss arm re-probes a declined callee on every call"
        );
        assert!(
            miss.contains(".lookup_megamorphic(receiver_cid)")
                && miss.contains("mic.callee_rerun_pins(),"),
            "a body the PIC has no room for is not handed to the entry cache"
        );
        assert!(
            body.contains("cm.superseded.load(std::sync::atomic::Ordering::Acquire)"),
            "the megamorphic arm calls a superseded way"
        );
    }
}

/// CP-indexed `ldc <Class>` (0x12/0x13) slow path — see
/// `jit_api::JitRuntimeHelpers::ldc_class_cp` for the ABI.
///
/// Resolves the target class through the same loader-faithful
/// [`jit_resolve_cp_class`] the deferred-`new` helper uses, then returns its
/// mirror. `0` means resolution failed and a pending exception is published.
/// The JVMS §5.4.4 class-access check runs exactly when the interpreter's
/// `ldc` runs it (every mode; `class_constant_access_enforced`, i9/i10-L2).
///
/// # Why the mirror is fetched per execution
///
/// It is an ordinary heap object, so a relocating collector can move it
/// between two invocations of the same compiled body — a baked immediate would
/// name freed or reused memory. Same reason [`jit_ldc_string`] re-interns its
/// literal rather than baking an `ObjectRef`. `get_or_create_class_mirror` is
/// a cached lookup, so the repeat cost is a map hit, not a re-creation.
///
/// Before this existed, an `ldc <Class>` had no representation anywhere in the
/// JIT: the compile-time resolver returned `None`, which is the
/// "permanently unrepresentable" answer, so the enclosing method was
/// bail-listed and never compiled at any tier. See
/// `docs/known-issues/jit-bans/dateformatsymbols-getproviderinstance-compile-bail-20260731.md`.
///
/// SAFETY: called from JIT-compiled code; `vm_ptr` must be a valid `SharedVm`
/// pointer and `holder_class_id`/`cp_idx` must be the compile-time-baked
/// referencing class and constant-pool index of this `ldc` site.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_ldc_class_cp(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64 {
    let f = || jit_ldc_class_cp_body(vm_ptr, holder_class_id, cp_idx);
    contain("jit_ldc_class_cp", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_ldc_class_cp`], run under its panic guard.
unsafe fn jit_ldc_class_cp_body(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64 {
    if vm_ptr == 0 {
        return 0;
    }
    // Resolution can run a user `ClassLoader.loadClass`, i.e. arbitrary Java
    // that can itself GC — so the cold arm crosses the boundary the way
    // `jit_new_object_cp` does. The SATB flush waits for that arm: a recorded
    // hit is a map lookup and has nothing to flush.
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: see `jit_new_object_cp`.
    let vm = &*(vm_ptr as *const SharedVm);
    // JVMS §5.4.3: the entry resolves ONCE. Before the resolution, before the
    // class-manager lock, and before the mirror map — the same position the
    // interpreter's `execute_ldc` probe was hoisted to, and for the same
    // reason: a recorded hit IS the whole instruction. The index is the
    // site's in the holder's CURRENT pool (`cp_site_index`). `fill_as_of`,
    // read before that translation, guards the record below (i22-L5, wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let site = CpSite::of(holder_class_id, cp_idx);
    let holder_cid = site.holder;
    let (fast_idx, judged_at) = site.index_lock_free(vm);
    if let Some(bits) = recorded_ldc_reference(vm, holder_cid, fast_idx) {
        // Kept only when no redefinition began since the index was judged
        // (interpreter round i1 wave 43, lane L2; `CpSite`); otherwise
        // resolved again below, under the guard.
        if CpSite::verdict_still_holds(judged_at) {
            crate::runtime::interpreter::site_cache::site_stats::bump(
                crate::runtime::interpreter::site_cache::site_stats::JIT_LDC_HIT,
            );
            return bits;
        }
    }
    crate::runtime::interpreter::site_cache::site_stats::bump(
        crate::runtime::interpreter::site_cache::site_stats::JIT_LDC_MISS,
    );
    jit_safepoint_flush_satb(vm_ptr);
    // The index the entry was read at, judged under the guard it was read
    // under: the record below is keyed by it.
    let (target_id, idx) = match jit_resolve_cp_class(vm, site, false) {
        Ok(resolved) => resolved,
        // A FAILED resolution is not recorded HERE (this store holds values):
        // `jit_resolve_cp_class` records it in the separate failure table, and
        // JVMS §5.4.3 rethrows that same error on every later attempt.
        Err(sentinel) => return sentinel,
    };
    // gen r4w6 (review6, class-mirror page item 1): the interpreter's
    // `execute_ldc` arm (fd1c23e51), compiled. The infallible mirror
    // allocation aborted the VM on a full heap (`FATAL: OutOfMemoryError:
    // young gen exhausted`, GenR4W6MirrorOomProbe's `jit:` case); decline,
    // collect once through the helpers' forced door, retry, then throw a
    // catchable `OutOfMemoryError`. Nothing is recorded or cached on the
    // declining path, so the retry re-derives everything.
    let mirror = match crate::vm::try_get_or_create_class_mirror(vm, target_id) {
        Some(m) => m,
        None => {
            // SAFETY: this helper runs on the JIT thread that installed the
            // thread pointer, and no other `&mut JvmThread` borrow is live
            // here (the same contract as the allocation slow paths above).
            if let Some((thread, _guard)) = jit_thread_mut() {
                thread.tlab.retire();
                crate::runtime::interpreter::maybe_gc_forced_pub_at(vm, thread, "jit-ldc-class");
            }
            match crate::vm::try_get_or_create_class_mirror(vm, target_id).or_else(|| {
                jit_ldc_last_ditch(vm, || {
                    crate::vm::try_get_or_create_class_mirror(vm, target_id)
                })
            }) {
                Some(m) => m,
                None => return jit_alloc_oom(vm, "Java heap space (ldc Class)"),
            }
        }
    };
    record_ldc_reference(vm, holder_cid, idx, mirror, fill_as_of);
    mirror.as_ptr() as i64
}

// ---------------------------------------------------------------------------
// JVMS §5.4.3 in compiled code: a constant-pool entry resolves ONCE.
//
// The interpreter learned this on 2026-08-18. Compiled code did not: both JIT
// `ldc` helpers re-derived their constant on every execution — `jit_ldc_string`
// through the string pool's `RwLock`, a hash of the whole literal and a
// `memcmp`; `jit_ldc_class_cp` through a full loader-faithful resolution and a
// mirror lookup. Measured at 18.4 ns and 62.7 ns against HotSpot's 0.2
// (`probes/LdcConstCostProbe.java`, compiled arm, one binary).
//
// The store is the one the interpreter already uses — `MemberResolver`'s
// `probe_constant` / `record_constant`, keyed `(class, cp index)` — reached
// through the SAME two functions in `interpreter::constants`, so the two
// routes cannot drift apart in what they consider recordable. That store is
// already collector-remapped (`for_each_condy_root` / `update_condy_refs`),
// redefinition-invalidated and bounded, which is exactly why adopting it is
// sound where baking an `ObjectRef` into the code would not be.
// ---------------------------------------------------------------------------

/// `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0` — stop answering compiled `ldc`
/// sites from the recorded resolution, and stop writing it. Default ON.
///
/// Deliberately separate from the interpreter's
/// `CRATONVM_JIT_NO_LDC_CONST_CACHE`: an A/B that moved both arms would change
/// what the warm-up costs as well as what the compiled body costs, and the
/// compiled body is what this switch exists to measure.
///
/// **It gates the WRITE as well as the read**, which is the lesson the
/// interpreter's own switch records: an ungated record would make the OFF arm
/// resolve AND take the store's write lock — work the pre-change compiled code
/// never did — so the A/B would measure the instrument instead of the change.
/// The `fill` counter reading non-zero with `hit`/`miss` at zero is the shape
/// that catches it.
fn compiled_ldc_const_cache_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        if crate::runtime::interpreter::constants::ldc_const_cache_forced_off_by_diagnostics() {
            // A trace that must observe every resolution cannot be answered
            // from a record. Same three switches the interpreter's own gate
            // reads, through the same predicate.
            return false;
        }
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_COMPILED_LDC_CONST_CACHE")
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
    })
}

/// The recorded resolution of this `ldc` site as a raw JIT return value, or
/// `None` when nothing is recorded (or the switch is off).
///
/// Only an OBJECT record can answer here: both callers push a reference, and a
/// constant-pool index has exactly one tag, so a record holding anything else
/// at this key would be a store bug rather than a shape this should coerce.
unsafe fn recorded_ldc_reference(vm: &SharedVm, holder: ClassId, cp_idx: u16) -> Option<i64> {
    if !compiled_ldc_const_cache_enabled() {
        return None;
    }
    // O(1) first: a per-thread direct-mapped memo naming a GC-maintained slot.
    if let Some(bits) = ldc_slot_memo_probe(vm.vm_identity, holder, cp_idx) {
        return Some(bits);
    }
    // Read BEFORE the record: a fill whose read straddles a redefinition is
    // not memoized (see `LDC_SLOT_MEMO_SURVIVES_REDEFINITION`).
    let as_of = crate::classloading::class_redefinition_count();
    match crate::runtime::interpreter::constants::probe_recorded_cp_constant(vm, holder, cp_idx) {
        Some(Value::Object(Some(obj))) => {
            ldc_slot_memo_fill(vm, holder, cp_idx, obj, as_of);
            Some(obj.as_ptr() as i64)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The compiled-`ldc` slot memo: an O(1), lock-free, hash-free recorded hit.
//
// `recorded_ldc_reference` answered every execution through
// `MemberResolver::probe_constant`: the resolution cache's `RwLock` read (a
// locked RMW on a line every thread shares) plus a hash-map probe of
// `(class, cp index)` — 6.5% + 2.2% of `CratonBenchC2 bind`, 3.9% + 1.0% of
// `dispatch` (round 9 wave 2 `perfdiag`,
// `perf-compiled-ldc-string-is-a-helper-call-per-execution-20260918.md`).
//
// The answer cannot be memoized as a raw pointer: the literal is a heap
// object and a relocating collection moves it. It CAN be memoized as the
// address of a slot the collector keeps current. A JNI global reference is
// exactly that — a `Box<ObjectRef>` whose address never changes, reported as
// a root and rewritten in place by `JniGlobalRefs::update_after_gc` on every
// moving collection, while every mutator is stopped. So:
//
// * per `(vm, holder, cp index)` there is ONE global ref, created on the first
//   recorded hit and never released (the VM's own `NativeRealm::ldc_slots`,
//   beside the global-ref table the slots live in; the table is bounded);
// * each thread keeps a 64-way direct-mapped memo `key -> slot address`
//   (`LDC_SLOT_MEMO`). A hit is one TLS access, a multiply, two compares and
//   one load through the slot.
//
// Soundness of each input:
//
// * The VALUE is permanent: JVMS §5.4.3 resolves the entry once, and the
//   record this mirrors never changes once written. `ClassId`s are never
//   reused (`ClassStore::next_id`).
// * Redefinition is the one event that invalidates the record: it replaces
//   the holder's constant pool under the same id. The redefined class's
//   slots leave the table (`retire_ldc_slots_of_class`) and every way carries
//   the `class_redefinition_count` it was filled under, so the memo keeps
//   serving every other class (`LDC_SLOT_MEMO_SURVIVES_REDEFINITION`; until
//   interpreter round i1 wave 18 it was bypassed for the process once
//   `any_class_redefined` was set).
// * A slot of a DROPPED VM is never read: the slot table dies with its VM,
//   and `vm_identity` (never issued twice) is part of every per-thread memo
//   way, so no live VM's probe can match a dropped VM's way.
// * The read of `*slot` races nothing: the collector rewrites slots only
//   while this mutator is parked, which is the same guarantee every other
//   heap read in a JIT helper relies on.
//
// Cost: one `Box<ObjectRef>` and one entry per distinct compiled `ldc` site,
// and the literal stays reachable for the VM's life — which an interned
// literal already is (JVMS §5.1). Past `LDC_GLOBAL_SLOTS_CAP` sites nothing
// new is memoized and the store answers as before.
// ---------------------------------------------------------------------------

/// Ways in the per-thread [`LDC_SLOT_MEMO`]. A power of two.
const LDC_SLOT_MEMO_WAYS: usize = 64;

/// Distinct `(holder, cp index)` sites of one VM that may get a memo slot.
const LDC_GLOBAL_SLOTS_CAP: usize = 1 << 16;

thread_local! {
    /// `(vm_identity, (holder << 16) | cp_idx, slot address, redefinition
    /// count)` per way; a zero slot address is an empty way. The count is the
    /// `class_redefinition_count` the way was filled under.
    static LDC_SLOT_MEMO: [Cell<(usize, u64, usize, u64)>; LDC_SLOT_MEMO_WAYS] =
        const { [const { Cell::new((0, 0, 0, 0)) }; LDC_SLOT_MEMO_WAYS] };
}

/// Kill switch for the `ldc` slot memo's redefinition handling (interpreter
/// round i1 wave 18, lane L4). `true`: a redefinition retires exactly what it
/// can change. A redefined class keeps its `ClassId` and gets a new constant
/// pool, so its `(holder, cp index)` slots may name the OLD pool's
/// constants: `redefine_class_with` drops them from [`ldc_global_slots`]
/// (`retire_ldc_slots_of_class`, before the code-cache flush), every
/// per-thread way carries the `class_redefinition_count` it was filled under
/// and misses once the count moves, and a fill or a publication whose read of
/// the record straddled a redefinition is dropped. `false`: the historical
/// latch — no memo and no compile-time slot for the rest of the process after
/// the first redefinition of any class.
const LDC_SLOT_MEMO_SURVIVES_REDEFINITION: bool = true;

/// Whether the `ldc` slot memo is latched off by a redefinition: never,
/// unless the kill switch restores the latch.
#[inline(always)]
fn ldc_slot_memo_latched_off() -> bool {
    !LDC_SLOT_MEMO_SURVIVES_REDEFINITION && crate::classloading::any_class_redefined()
}

/// Whether a way, publication or fill stamped `as_of` is still current.
#[inline(always)]
fn ldc_redefinitions_unchanged_since(as_of: u64) -> bool {
    !LDC_SLOT_MEMO_SURVIVES_REDEFINITION || crate::classloading::class_redefinition_count() == as_of
}

/// Drop `holder`'s slots from this VM's [`ldc_global_slots`]: a redefinition
/// replaced `holder`'s constant pool, so the constants they hold are the old
/// pool's. Called by `redefine_class_with` under the class-manager writer,
/// after the shared resolution memo was swept and BEFORE the code-cache
/// flush, so no compile that starts after the flush finds one.
///
/// The JNI global references behind them are NOT released: a compiled body
/// of the old class may still be running and reading its baked slot, and the
/// per-thread memo ways naming them retire only on their next probe. One
/// word per retired site per redefinition of its class stays rooted for the
/// VM's life (the old constant; an interned literal already is).
pub(crate) fn retire_ldc_slots_of_class(vm: &SharedVm, holder: ClassId) {
    if !LDC_SLOT_MEMO_SURVIVES_REDEFINITION {
        return;
    }
    let holder = holder.as_u32();
    let mut slots = ldc_global_slots(vm).lock();
    slots.retain(|&(h, _), _| h != holder);
}

/// This VM's `(holder, cp_idx) -> slot address` (the untagged
/// `Box<ObjectRef>` behind one of its JNI global refs). Consulted only on a
/// per-thread memo miss.
///
/// Per-VM (`NativeRealm::ldc_slots`) since round 14 wave 2 (MISC11-1): it was
/// a process static keyed by `vm_identity`, whose dead rows a teardown hook
/// had to purge (`r13w10-sync6-ldc-global-slots-outlive-their-vm`) so that an
/// embedding that creates and drops VMs did not fill [`LDC_GLOBAL_SLOTS_CAP`];
/// now the rows die with the VM and the cap is per VM.
#[inline]
fn ldc_global_slots(
    vm: &SharedVm,
) -> &parking_lot::Mutex<rustc_hash::FxHashMap<(u32, u16), usize>> {
    &vm.natives.ldc_slots
}

/// Pack a site key and pick its way: Fibonacci hash, HIGH bits, so the
/// clustered cp indices of one class do not alias (the `SiteCache` recipe).
#[inline(always)]
fn ldc_slot_memo_key(holder: ClassId, cp_idx: u16) -> (u64, usize) {
    let key = ((holder.as_u32() as u64) << 16) | cp_idx as u64;
    let way = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize & (LDC_SLOT_MEMO_WAYS - 1);
    (key, way)
}

/// The memoized literal for this site, if this thread has seen it.
#[inline]
fn ldc_slot_memo_probe(vm_identity: usize, holder: ClassId, cp_idx: u16) -> Option<i64> {
    if ldc_slot_memo_latched_off() {
        return None;
    }
    let (key, way) = ldc_slot_memo_key(holder, cp_idx);
    let (vm_id, k, slot, as_of) = LDC_SLOT_MEMO.try_with(|m| m[way].get()).ok()?;
    if slot == 0 || vm_id != vm_identity || k != key || !ldc_redefinitions_unchanged_since(as_of) {
        return None;
    }
    // SAFETY: `slot` is the address of a live `Box<ObjectRef>` owned by this
    // VM's `JniGlobalRefs` (never released, including after
    // `retire_ldc_slots_of_class` drops it from the table; see the section
    // comment) — the VM is
    // live because the caller holds `&SharedVm` for `vm_identity`. The
    // collector rewrites it only while this thread is parked.
    let obj: ObjectRef = unsafe { *(slot as *const ObjectRef) };
    Some(obj.as_ptr() as i64)
}

/// Give this site a GC-maintained slot (creating the global ref on first use,
/// process-wide) and remember it in this thread's memo.
///
/// `as_of` is the `class_redefinition_count` read before `obj` was read from
/// the record; nothing is published or memoized once it has moved.
#[cold]
fn ldc_slot_memo_fill(vm: &SharedVm, holder: ClassId, cp_idx: u16, obj: ObjectRef, as_of: u64) {
    if ldc_slot_memo_latched_off() || !ldc_redefinitions_unchanged_since(as_of) {
        return;
    }
    let Some(slot) = ldc_global_slot_publish(vm, holder, cp_idx, obj, as_of) else {
        return;
    };
    let (key, way) = ldc_slot_memo_key(holder, cp_idx);
    let _ = LDC_SLOT_MEMO.try_with(|m| m[way].set((vm.vm_identity, key, slot, as_of)));
}

/// The site's slot address in [`ldc_global_slots`], creating the JNI global
/// ref that backs it from `obj` when the site has none. `None` past
/// [`LDC_GLOBAL_SLOTS_CAP`].
///
/// `obj` must be the site's recorded resolution, read by a thread the
/// collector cannot relocate under (a mutator between safepoints): the new
/// global ref is created holding `obj` and is only remapped by collections
/// that START after it is in the table.
fn ldc_global_slot_publish(
    vm: &SharedVm,
    holder: ClassId,
    cp_idx: u16,
    obj: ObjectRef,
    as_of: u64,
) -> Option<usize> {
    let map_key = (holder.as_u32(), cp_idx);
    let existing = ldc_global_slots(vm).lock().get(&map_key).copied();
    if existing.is_some() {
        return existing;
    }
    if ldc_global_slots(vm).lock().len() >= LDC_GLOBAL_SLOTS_CAP {
        return None;
    }
    // Not under the slots lock: `jni_global_refs` has its own, and the
    // two are never held together.
    let handle = vm.natives.jni_global_refs.lock().add(obj) as usize;
    let mine = handle & !1;
    let winner = {
        let mut slots = ldc_global_slots(vm).lock();
        // Under the slots lock, which `retire_ldc_slots_of_class` also takes:
        // `obj` read before a redefinition may be the old pool's constant, and
        // once the count has moved the retirement may already have run.
        if ldc_redefinitions_unchanged_since(as_of) {
            Some(*slots.entry(map_key).or_insert(mine))
        } else {
            None
        }
    };
    if winner != Some(mine) {
        // Another thread published first, or a redefinition overtook the
        // read: release ours, which was never handed out.
        vm.natives
            .jni_global_refs
            .lock()
            .remove(handle as crate::native::jni::JObject);
    }
    winner
}

/// Name of the GC-neutral background compile worker
/// (`jit::tiered::start_background_compiler`).
const BACKGROUND_COMPILER_THREAD_NAME: &str = "cratonvm-jit-compiler";

/// COMPILE TIME: the address of a GC-maintained word holding the resolved
/// `ldc <String>` / `ldc <Class>` constant at `(holder, cp_idx)`, or `0` when
/// the site must keep calling `jit_ldc_string_cp` / `jit_ldc_class_cp`.
///
/// This is the VM half of
/// `perf-compiled-ldc-string-is-a-helper-call-per-execution-20260918.md`.
/// Compiled code bakes the address and does `mov rax,[slot]; test; jnz done`
/// (`jit/src/x64/objects.rs::emit_ldc_cp_site`) in place of the helper CALL.
///
/// # What the slot is, and why compiled code may read it raw
///
/// It is the same slot the per-thread memo already reads
/// ([`ldc_slot_memo_probe`]): the untagged `Box<ObjectRef>` behind a JNI global
/// reference of THIS VM, created once per site and never released while the VM
/// lives. So it is
///
/// * **stable**: a `Box` never moves, and nothing removes it
///   ([`ldc_global_slot_publish`] releases only a ref that lost the race and
///   was never handed out);
/// * **a root**: `JniGlobalRefs::collect_roots` reports it, so the constant
///   stays alive;
/// * **remapped in place**: every moving collection runs
///   `JniGlobalRefs::update_after_gc` (`memory::gc::update_all_roots`), which
///   rewrites the word, and does so while every mutator is parked at a
///   safepoint. A thread executing the three-instruction probe is not parked,
///   so it reads either the pre-move or the post-move address, never a torn
///   or stale one: exactly the guarantee a raw reference in a compiled frame
///   already relies on;
/// * **not a ZGC colored slot**: roots hold plain machine pointers. ZGC's load
///   barrier (`ZgcRealHeap::load_barrier_slot`) is for colored heap slots and
///   is never armed on a default run; the memo reads this word with no barrier
///   for the same reason.
/// * **VM-specific**: the slot table is this VM's own ([`ldc_global_slots`]),
///   and the compiled body that bakes the address also bakes this VM's class
///   id, so it can run only against this VM, and the VM outlives its code.
///
/// # When the answer is `0`
///
/// * `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0` (or a diagnostic that must see
///   every resolution): the kill switch covers the slot too.
/// * Only under the `LDC_SLOT_MEMO_SURVIVES_REDEFINITION = false` kill
///   switch: any class has been redefined. Otherwise a redefined class (same
///   id, new constant pool) has its slots dropped from the table by
///   `retire_ldc_slots_of_class` before the code-cache flush, so a compile
///   after it mints fresh ones from the new pool, and a publication whose
///   read straddled a redefinition is refused. Code compiled BEFORE the
///   redefinition keeps its slot; it runs the old bytecode, whose constant
///   is the old one.
/// * The site has not resolved yet (nothing recorded). A failed resolution is
///   never recorded (JVMS §5.4.3), so an error site never gets a slot.
/// * The slot table is full ([`LDC_GLOBAL_SLOTS_CAP`]).
/// * No slot exists yet and this is the background compile worker. Creating a
///   slot reads the recorded `ObjectRef` and stores it into a new root. The
///   worker is GC-neutral (never parked, not counted by the STW barrier), so a
///   collection can remap the roots between the read and the insert, and the
///   new root would then hold a pre-move address that no one remaps. A mutator
///   compiling in-line cannot be collected under (the collection waits for it
///   to park), so it may create the slot. The worker may still use a slot that
///   already exists: that path reads only the table, never the heap.
///
/// Must not be called with the class-manager lock held: it takes the
/// resolution cache, the slot table and the JNI global-ref table locks, which
/// the helper path takes with no class-manager lock.
pub(crate) fn compiled_ldc_slot_for(vm: &SharedVm, holder: ClassId, cp_idx: u16) -> usize {
    if !compiled_ldc_const_cache_enabled() || ldc_slot_memo_latched_off() {
        return 0;
    }
    let map_key = (holder.as_u32(), cp_idx);
    if let Some(slot) = ldc_global_slots(vm).lock().get(&map_key).copied() {
        return slot;
    }
    if std::thread::current().name() == Some(BACKGROUND_COMPILER_THREAD_NAME) {
        return 0;
    }
    let as_of = crate::classloading::class_redefinition_count();
    match crate::runtime::interpreter::constants::probe_recorded_cp_constant(vm, holder, cp_idx) {
        Some(Value::Object(Some(obj))) => {
            ldc_global_slot_publish(vm, holder, cp_idx, obj, as_of).unwrap_or(0)
        }
        _ => 0,
    }
}

/// Round 13 wave 10 (lane sync6,
/// `r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md` part A):
/// the constant-pool index under which [`ldc_global_slots`] keeps a class's
/// own MIRROR slot, the monitor of its `static synchronized` methods (JVMS
/// 2.11.10). No `ldc` site can collide with it: `constant_pool_count` is a
/// `u16`, so the last valid index is 65534.
const CLASS_MIRROR_SLOT_CP_IDX: u16 = u16::MAX;

/// COMPILE TIME: the address of the GC-maintained word holding `class_id`'s
/// `Class` mirror, or `0`. A `static synchronized` method's self-locking body
/// bakes it (`CompileRequest::self_lock_mirror_slot`) and reads its monitor
/// from it at every use.
///
/// The slot is an ordinary [`ldc_global_slots`] entry under
/// [`CLASS_MIRROR_SLOT_CP_IDX`], so everything [`compiled_ldc_slot_for`] says
/// about why compiled code may read it raw holds unchanged (a stable box, a
/// JNI global root, remapped in place while every mutator is parked), and so
/// does its minting rule: a MUTATOR may create the slot, from the mirror the
/// class already has (read from `class_mirrors`, never created here: creation
/// allocates); the GC-neutral background compile worker only finds one that
/// exists. The door mints it on a thread that holds the mirror anyway
/// ([`mint_static_sync_mirror_slot`]), so a compile that follows first-tier
/// calls through the door finds it.
///
/// `0` under `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0` (and the diagnostics
/// that force it off), under the redefinition latch kill switch, past
/// [`LDC_GLOBAL_SLOTS_CAP`], on the worker with no slot yet, and for a class
/// with no mirror yet. A redefinition drops the class's slot with its `ldc`
/// slots (`retire_ldc_slots_of_class`); the mirror itself survives a
/// redefinition, so a body compiled before it keeps reading the right object.
/// Like every `ldc` slot the global reference is never released, which keeps
/// the mirror (and so its class) reachable for the VM's life, as an
/// `ldc <Class>` slot of that class already does.
///
/// Must not be called with the class-manager lock held (the slot table and
/// JNI global-ref locks, as [`compiled_ldc_slot_for`]).
pub(crate) fn class_mirror_slot_for(vm: &SharedVm, class_id: ClassId) -> usize {
    if !compiled_ldc_const_cache_enabled() || ldc_slot_memo_latched_off() {
        return 0;
    }
    let map_key = (class_id.as_u32(), CLASS_MIRROR_SLOT_CP_IDX);
    if let Some(slot) = ldc_global_slots(vm).lock().get(&map_key).copied() {
        return slot;
    }
    if std::thread::current().name() == Some(BACKGROUND_COMPILER_THREAD_NAME) {
        return 0;
    }
    let as_of = crate::classloading::class_redefinition_count();
    let Some(mirror) = vm.classes.class_mirrors.read().get(&class_id).copied() else {
        return 0;
    };
    ldc_global_slot_publish(vm, class_id, CLASS_MIRROR_SLOT_CP_IDX, mirror, as_of).unwrap_or(0)
}

/// JIT round 19 proposal wave 5, lane concat5 (STRGAP4-2): the constant-pool
/// index under which [`ldc_global_slots`] keeps a string-literal slot that no
/// `ldc` site names (`"null"`, `""`). Index 0 is never a valid constant-pool
/// entry (JVMS 4.4: the table starts at 1) and the mirror slot is
/// [`CLASS_MIRROR_SLOT_CP_IDX`], so no other row can collide with
/// `(literal, 0)` whatever the "holder" word is.
const STRING_LITERAL_SLOT_CP_IDX: u16 = 0;

/// COMPILE TIME (STRGAP4-2, `CRATONVM_JIT_STRING_LITERAL_SLOTS`): the address
/// of a GC-maintained word holding THIS VM's interned `"null"` (`which == 0`)
/// or `""` (`which == 1`) literal, or `0`. Compiled code bakes the address and
/// reads the word (`mov rax,[slot]`) where JDK 25 would `ldc` the literal
/// inside a callee it does not run: `String.valueOf(Object)` of `null`, whose
/// body is `obj == null ? "null" : obj.toString()`.
///
/// # Why a rooted slot, and why this one
///
/// The literal is a movable heap object with no address of its own that
/// compiled code could bake. The slot is an ordinary [`ldc_global_slots`]
/// entry (the untagged `Box<ObjectRef>` of one of this VM's JNI global
/// references), so everything [`compiled_ldc_slot_for`] says about reading it
/// raw holds: a stable box, a root (`JniGlobalRefs::collect_roots`),
/// remapped in place by every moving collection while every mutator is
/// parked, never released while the VM lives, and per VM (the table is
/// `NativeRealm::ldc_slots`; no process global). Rooting is also what keeps
/// the IDENTITY right: the slot holds the very object the string pool maps
/// `"null"` to, which is what `ldc "null"` answers (JVMS 5.1), and while the
/// root holds it the weak pool (`CRATONVM_GC_WEAK_STRING_TABLE`, Generational
/// only) cannot drop that entry and later pool a different `"null"`. An
/// immortal (non-moving) copy was the alternative and is wrong for exactly
/// that reason: it would be a second `"null"` whenever the pool already holds
/// one, and `String.valueOf(null) == "null"` is `true` on HotSpot.
///
/// # When the answer is `0`
///
/// The [`compiled_ldc_slot_for`] cases (kill switch, latch, table cap, the
/// background worker with no slot yet), plus: the literal is not pooled yet.
/// This never interns: a compile does not allocate. JDK 25's own
/// `String.valueOf` interns `"null"` the first time it runs its `ldc`, and a
/// later compile mints the slot from the pooled object. Reading the pool here
/// is sound for the reason the `ldc` helper's pool hit is: an entry present
/// between pauses names a live String (the weak sweep runs only inside a
/// pause, and a concurrent cycle keeps the pool strong), and the new root is
/// in the table before the next pause scans it.
///
/// Must not be called with the class-manager lock held (the slot table, the
/// string pool and the JNI global-ref locks).
pub(crate) fn string_literal_slot_for(vm: &SharedVm, which: u32) -> usize {
    let text = match which {
        0 => "null",
        1 => "",
        _ => return 0,
    };
    if !compiled_ldc_const_cache_enabled() || ldc_slot_memo_latched_off() {
        return 0;
    }
    let map_key = (which, STRING_LITERAL_SLOT_CP_IDX);
    if let Some(slot) = ldc_global_slots(vm).lock().get(&map_key).copied() {
        return slot;
    }
    if std::thread::current().name() == Some(BACKGROUND_COMPILER_THREAD_NAME) {
        return 0;
    }
    let as_of = crate::classloading::class_redefinition_count();
    // Never blocks on the pool (the resolver's lock discipline, see
    // `jit_well_known_class_id`): a busy pool costs this compile the slot.
    let Some(obj) = vm
        .mem
        .string_pool
        .try_read()
        .and_then(|pool| pool.get(text).copied())
    else {
        return 0;
    };
    ldc_global_slot_publish(
        vm,
        ClassId::new(which),
        STRING_LITERAL_SLOT_CP_IDX,
        obj,
        as_of,
    )
    .unwrap_or(0)
}

/// Round 13 wave 10 (lane sync6): give `class_id` its mirror slot
/// ([`class_mirror_slot_for`]) from `mirror`, which the calling MUTATOR holds
/// for a `static synchronized` call and read with no safepoint since (the JIT
/// door's monitor acquire, `jit_bridge` `JitSynchronizedMonitorGuard::acquire`).
/// Once minted, one probe of this thread's `ldc` memo: the per-call cost of a
/// door that is already ~400 ns, and that a self-locking body, once published,
/// no longer takes.
///
/// Round 15 wave 4 (lane sync3, SY2-2): `true` when this call missed this
/// thread's memo and the slot exists now (minted here or found in the table)
/// -- the once-per-thread cold path on which the door asks whether the
/// method's published body is a wrapped one compiled before the slot existed
/// (`jit_bridge` `request_static_self_lock_recompile`). `false` on the memo's
/// hit and whenever no slot could be had.
pub(crate) fn mint_static_sync_mirror_slot(
    vm: &SharedVm,
    class_id: ClassId,
    mirror: ObjectRef,
) -> bool {
    if !compiled_ldc_const_cache_enabled()
        || ldc_slot_memo_probe(vm.vm_identity, class_id, CLASS_MIRROR_SLOT_CP_IDX).is_some()
    {
        return false;
    }
    ldc_slot_memo_fill(
        vm,
        class_id,
        CLASS_MIRROR_SLOT_CP_IDX,
        mirror,
        crate::classloading::class_redefinition_count(),
    );
    // The fill records nothing past the table's cap or across a redefinition.
    ldc_slot_memo_probe(vm.vm_identity, class_id, CLASS_MIRROR_SLOT_CP_IDX).is_some()
}

/// Round 15 wave 4 (lane sync3, SY2-2): [`mint_static_sync_mirror_slot`] for
/// the door compiled callers take (`jit_bridge`
/// `execute_wrapped_body_door_locked`), which never minted before this wave.
/// `admit` (that change's kill switch) is asked only on this thread's memo
/// miss, so the door's per-call cost stays one memo probe.
pub(crate) fn mint_static_sync_mirror_slot_if(
    vm: &SharedVm,
    class_id: ClassId,
    mirror: ObjectRef,
    admit: impl FnOnce() -> bool,
) -> bool {
    if !compiled_ldc_const_cache_enabled()
        || ldc_slot_memo_probe(vm.vm_identity, class_id, CLASS_MIRROR_SLOT_CP_IDX).is_some()
        || !admit()
    {
        return false;
    }
    mint_static_sync_mirror_slot(vm, class_id, mirror)
}

#[cfg(test)]
mod r13w10_sync6_class_mirror_slot_tests {
    use super::*;
    use crate::config::VmConfig;
    use std::sync::Arc;

    /// The per-class mirror slot: a mutator mints it from the mirror the class
    /// has, the door's mint and the compile-time lookup agree on ONE word,
    /// the word follows a relocation, the background worker finds but never
    /// mints one, and a redefinition retires it with the class's `ldc` slots.
    #[test]
    fn one_collector_maintained_mirror_slot_per_class() {
        if ldc_slot_memo_latched_off() || !compiled_ldc_const_cache_enabled() {
            // `0` by design under the kill switches.
            return;
        }
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let (cid, other) = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(cid) = cm.try_ensure_synthetic_class("cratonvm/test/R13Sync6MirrorHolder", 0)
            else {
                panic!("Compatible mode fabricates this fixture");
            };
            let Ok(other) = cm.try_ensure_synthetic_class("cratonvm/test/R13Sync6MirrorOther", 0)
            else {
                panic!("Compatible mode fabricates this fixture");
            };
            (cid, other)
        };
        let before = shared.mem.heap.alloc_object(cid, 0);
        let after = shared.mem.heap.alloc_object(cid, 0);
        let other_mirror = shared.mem.heap.alloc_object(other, 0);

        assert_eq!(
            class_mirror_slot_for(&shared, cid),
            0,
            "no mirror yet: no slot"
        );
        shared.classes.class_mirrors.write().insert(cid, before);
        shared
            .classes
            .class_mirrors
            .write()
            .insert(other, other_mirror);

        // The worker finds no slot and mints none.
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(BACKGROUND_COMPILER_THREAD_NAME.to_string())
                .spawn(move || class_mirror_slot_for(&shared, cid))
                .map(|h| h.join().unwrap_or(usize::MAX))
                .unwrap_or(usize::MAX)
        };
        assert_eq!(worker, 0, "the GC-neutral worker never mints");

        // A mutator mints it from the class's mirror.
        let slot = class_mirror_slot_for(&shared, cid);
        assert_ne!(slot, 0);
        // SAFETY: `slot` is a live `Box<ObjectRef>` owned by `shared`'s
        // global-ref table, which outlives this test body.
        let read = || unsafe { *(slot as *const ObjectRef) };
        assert_eq!(read().as_ptr(), before.as_ptr());
        assert_eq!(
            class_mirror_slot_for(&shared, cid),
            slot,
            "one slot per class"
        );
        assert_ne!(class_mirror_slot_for(&shared, other), slot, "per class");

        // The door's mint and the lookup name the same word; the worker now
        // finds it. Round 15 wave 4 (lane sync3): the mint answers `true` on
        // this thread's memo miss only, the door's once-per-thread cold path;
        // the compiled-caller door's form asks its switch there, and a refusal
        // fills nothing.
        let probe = || ldc_slot_memo_probe(shared.vm_identity, cid, CLASS_MIRROR_SLOT_CP_IDX);
        assert!(!mint_static_sync_mirror_slot_if(
            &shared,
            cid,
            before,
            || false
        ));
        assert_eq!(probe(), None, "a refused mint fills nothing");
        assert!(
            mint_static_sync_mirror_slot_if(&shared, cid, before, || true),
            "a miss, a slot"
        );
        let asked = std::cell::Cell::new(false);
        let admit = || {
            asked.set(true);
            true
        };
        assert!(
            !mint_static_sync_mirror_slot_if(&shared, cid, before, admit),
            "the memo's hit"
        );
        assert!(!asked.get(), "the switch is not asked on a hit");
        assert_eq!(
            ldc_slot_memo_probe(shared.vm_identity, cid, CLASS_MIRROR_SLOT_CP_IDX),
            Some(before.as_ptr() as i64)
        );
        assert!(
            !mint_static_sync_mirror_slot(&shared, cid, before),
            "the memo's hit"
        );
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(BACKGROUND_COMPILER_THREAD_NAME.to_string())
                .spawn(move || class_mirror_slot_for(&shared, cid))
                .map(|h| h.join().unwrap_or(usize::MAX))
                .unwrap_or(usize::MAX)
        };
        assert_eq!(worker, slot, "the worker uses a slot that exists");

        // A moving collection rewrites the word in place.
        let mut moved = cratonvm_types::PointerMap::new();
        moved.insert(before.as_ptr() as usize, after.as_ptr() as usize);
        shared
            .natives
            .jni_global_refs
            .lock()
            .update_after_gc(&moved);
        assert_eq!(read().as_ptr(), after.as_ptr());

        // A redefinition of the class retires its slot from the table (the
        // old word stays valid for code compiled before it).
        retire_ldc_slots_of_class(&shared, cid);
        if LDC_SLOT_MEMO_SURVIVES_REDEFINITION {
            let key = (cid.as_u32(), CLASS_MIRROR_SLOT_CP_IDX);
            assert_eq!(ldc_global_slots(&shared).lock().get(&key).copied(), None);
            assert_eq!(read().as_ptr(), after.as_ptr());
        }
    }
}

#[cfg(test)]
mod r13w11_misc11_ldc_slot_teardown_tests {
    use super::*;
    use crate::config::VmConfig;
    use std::sync::Arc;

    fn vm_with_a_mirror_slot(name: &str) -> (Arc<SharedVm>, ClassId, usize) {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(cid) = cm.try_ensure_synthetic_class(name, 0) else {
                panic!("Compatible mode fabricates this fixture");
            };
            cid
        };
        let mirror = shared.mem.heap.alloc_object(cid, 0);
        shared.classes.class_mirrors.write().insert(cid, mirror);
        let slot = class_mirror_slot_for(&shared, cid);
        assert_ne!(slot, 0);
        (shared, cid, slot)
    }

    /// `r13w10-sync6-ldc-global-slots-outlive-their-vm`, and MISC11-1 (round
    /// 14 wave 2): the slot table is per VM, so a VM's rows die with it and
    /// one VM's rows, even under the same `(class id, cp index)` key, are
    /// neither seen nor retired by another.
    #[test]
    fn each_vm_has_its_own_ldc_slot_table() {
        if ldc_slot_memo_latched_off() || !compiled_ldc_const_cache_enabled() {
            // No slot is ever minted under the kill switches.
            return;
        }
        let name = "cratonvm/test/R13Misc11LdcPerVm";
        let (a, a_cid, a_slot) = vm_with_a_mirror_slot(name);
        let (b, b_cid, b_slot) = vm_with_a_mirror_slot(name);
        assert_ne!(a_slot, b_slot, "each VM's slot is its own global ref");
        assert_eq!(ldc_global_slots(&a).lock().len(), 1);
        assert_eq!(ldc_global_slots(&b).lock().len(), 1);

        retire_ldc_slots_of_class(&a, a_cid);
        if LDC_SLOT_MEMO_SURVIVES_REDEFINITION {
            assert_eq!(ldc_global_slots(&a).lock().len(), 0);
        }
        assert_eq!(
            ldc_global_slots(&b).lock().len(),
            1,
            "another VM's retirement"
        );
        assert_eq!(class_mirror_slot_for(&b, b_cid), b_slot);
        drop(a);
        assert_eq!(
            class_mirror_slot_for(&b, b_cid),
            b_slot,
            "a dropped VM's teardown"
        );
    }
}

/// Record this `ldc` site's resolution, so the next execution is a probe.
///
/// Bumps the fill counter ITSELF, and only when the write actually happened.
/// Counting the attempt instead reported `fill=4 796 969` on a run with the
/// switch OFF, which had recorded nothing — a counter whose name implies the
/// wrong fact, and the exact shape the site-cache stats exist to make visible.
///
/// `fill_as_of` is the `ResolutionCache::fill_snapshot` the helper read before
/// its site's key and pool read: nothing is recorded when a redefinition of
/// `holder` may have replaced the pool since (i22-L5, interpreter round i1
/// wave 24). The fill counter still counts the attempt in that rare case.
///
/// Only the `CONSTANT_String` and `CONSTANT_Class` helpers call this (nio21
/// wave 12 L1 audit): compiled code has no `ldc <MethodType>` helper, so a
/// method type is always recorded by the interpreter or a bootstrap, which
/// route it to the permanent store
/// (`constants::method_type_record_is_permanent`). A future MethodType (or
/// MethodHandle / Dynamic) caller must NOT come here — it must record through
/// `constants::record_cp_constant_permanent_as_of` and push the winner it
/// answers, or an evicted FIFO record would let the weakly interned object
/// change identity while its class lives.
unsafe fn record_ldc_reference(
    vm: &SharedVm,
    holder: ClassId,
    cp_idx: u16,
    obj: ObjectRef,
    fill_as_of: u64,
) {
    if !compiled_ldc_const_cache_enabled() {
        // gcg g4/rs H2 (string-table review item 3, compiled half), applied by
        // g4/jm: under the weak String table the record is what keeps a
        // literal alive (HotSpot's resolved references), whatever the cache
        // switch says. Nothing reads it with the cache off. The interpreter's
        // twin is `constants::record_cp_constant_if_enabled`.
        let value = Value::Object(Some(obj));
        if crate::runtime::interpreter::constants::weak_table_needs_the_record(
            vm, holder, cp_idx, &value,
        ) {
            crate::runtime::interpreter::constants::store_recorded_cp_constant_as_of(
                vm, holder, cp_idx, value, fill_as_of,
            );
        }
        return;
    }
    crate::runtime::interpreter::constants::store_recorded_cp_constant_as_of(
        vm,
        holder,
        cp_idx,
        Value::Object(Some(obj)),
        fill_as_of,
    );
    crate::runtime::interpreter::site_cache::site_stats::bump(
        crate::runtime::interpreter::site_cache::site_stats::JIT_LDC_FILL,
    );
}

/// CP-indexed `ldc <String>` — the superseder of [`jit_ldc_string`].
///
/// Returns the interned literal named at `cp_idx` in `holder_class_id`'s
/// constant pool, or `0` with a pending exception published if that entry
/// cannot be re-read (which the compile-time resolver has already proved it
/// can, so this arm is defensive).
///
/// # Why the object is still fetched at run time
///
/// The same reason [`jit_ldc_class_cp`] fetches its mirror: an `ObjectRef`
/// baked into generated code names freed or reused memory after a relocating
/// collector moves it. What changed is not that the fetch happens — it is what
/// the fetch COSTS. A recorded resolution is a hash of `(class id, cp index)`;
/// the string pool is a hash of the literal's entire content plus a `memcmp`
/// against the stored key, and netty puts literals on its hottest paths by
/// construction (`ObjectUtil.checkPositive(increment, "increment")` inside
/// `RefCnt.retain0`).
///
/// # Why no handoff root
///
/// Unlike the native fast paths, neither the recorded value nor an interned
/// literal needs `native_pending_return`: both stores ARE GC roots (the
/// resolution record is scanned by `for_each_condy_root`, the pool by
/// `roots.rs`), so the reference cannot be collected in the window between
/// this return and the caller's push. (gcg g4/rs: under the opt-in weak String
/// table the pool is not a root of a stop-the-world pause; the record is, and
/// with the `ldc` cache off `record_ldc_reference` still records under the
/// weak table. The returned value is a compiled-frame value from here on.)
///
/// SAFETY: called from JIT-compiled code; `vm_ptr` must be a valid `SharedVm`
/// pointer and `holder_class_id`/`cp_idx` the compile-time-baked referencing
/// class and constant-pool index of this `ldc` site.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_ldc_string_cp(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64 {
    let f = || jit_ldc_string_cp_body(vm_ptr, holder_class_id, cp_idx);
    contain("jit_ldc_string_cp", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_ldc_string_cp`], run under its panic guard.
unsafe fn jit_ldc_string_cp_body(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64 {
    if vm_ptr == 0 {
        return 0;
    }
    // The Rust<->JIT boundary bookkeeping is owed on BOTH arms; the SATB flush
    // is owed only on the COLD one. A recorded hit allocates nothing, reaches
    // no safepoint and performs no reference store, so it has nothing to
    // flush — the same split the `VarHandle` read helper's fast path takes.
    // The cold arm interns, and flushes immediately before it does.
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: see `jit_ldc_class_cp`.
    let vm = &*(vm_ptr as *const SharedVm);
    // Before the translation and the pool read below (i22-L5, wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let site = CpSite::of(holder_class_id, cp_idx);
    let holder = site.holder;
    let (fast_idx, judged_at) = site.index_lock_free(vm);
    if let Some(bits) = recorded_ldc_reference(vm, holder, fast_idx) {
        // A record found through an index judged before a redefinition that
        // began meanwhile may be the new pool's: re-read under the guard
        // below (interpreter round i1 wave 43, lane L2).
        if CpSite::verdict_still_holds(judged_at) {
            crate::runtime::interpreter::site_cache::site_stats::bump(
                crate::runtime::interpreter::site_cache::site_stats::JIT_LDC_HIT,
            );
            return bits;
        }
    }
    crate::runtime::interpreter::site_cache::site_stats::bump(
        crate::runtime::interpreter::site_cache::site_stats::JIT_LDC_MISS,
    );
    jit_safepoint_flush_satb(vm_ptr);
    // Cold, once per site: re-read the literal. Owned before the lock is
    // dropped, because `create_java_string` allocates and must not run under
    // the class-manager read lock (`create_exception_object` re-enters it).
    // Three different failures used to share one message, and the message named
    // only the one that cannot happen. The compile-time resolver has already
    // proved this entry IS a readable string literal in this class, so a
    // failure here is never "the constant pool says otherwise" — it is the
    // class id no longer naming the class it named at compile time, or naming
    // nothing at all. Reporting them apart is the difference between "the
    // constant pool is wrong" (it is not) and "this artifact is being run
    // against a class store that does not hold its holder".
    //
    // The index is judged under the guard the literal is read under
    // (`CpSite::index_in`, interpreter round i1 wave 43, lane L2): judged
    // before it, a redefinition that installed in between made a body of the
    // old bytecode read the new pool at the old index.
    let (idx, text, why) = {
        let cm = vm.classes.class_manager.read();
        let idx = site.index_in(&cm);
        let (text, why) = match cm.get_class(holder) {
            None => (None, "no class is loaded at that class id".to_string()),
            Some(class) => match class.constant_pool.get(idx) {
                Some(cratonvm_reader::constant_pool::ConstantPoolEntry::StringReference {
                    string_index,
                }) => match class.constant_pool.get_utf8(*string_index) {
                    Some(t) => (Some(t.to_owned()), String::new()),
                    None => (
                        None,
                        format!(
                            "class {} holds a StringReference at that index whose utf8 entry #{string_index} is unreadable",
                            class.name
                        ),
                    ),
                },
                other => (
                    None,
                    format!(
                        "class {} holds {} at that index, not a StringReference",
                        class.name,
                        match other {
                            Some(e) => format!("{e:?}"),
                            None => "no entry".to_string(),
                        },
                    ),
                ),
            },
        };
        (idx, text, why)
    };
    let Some(text) = text else {
        return jit_cp_alloc_internal_error(
            vm,
            &format!(
                "JIT ldc: cp#{idx} (compiled as cp#{} at stamp {:?}) of class id {}: {why} (this site was resolved at compile time, and the index is translated through the class's redefinitions under the guard the entry is read under)",
                site.baked,
                site.stamp,
                holder.as_u32()
            ),
        );
    };
    // gen r4w6 (orchestrator): the compiled twin of gc-common w8's
    // `intern_string_literal_or_oom` (af081c505). The infallible pooled
    // constructor aborted the VM on a full heap (`FATAL: heap exhausted
    // allocating java/lang/String`, found by GenR4W6MirrorOomProbe under
    // `CRATONVM_DBG=oom-bt`); decline, collect once through the helpers'
    // forced door, retry, then throw a catchable `OutOfMemoryError`. Nothing
    // is pooled or recorded on the declining path.
    let obj = match crate::vm::try_create_java_string_reporting(vm, &text) {
        Some((o, _)) => o,
        None => {
            // SAFETY: this helper runs on the JIT thread that installed the
            // thread pointer, and no other `&mut JvmThread` borrow is live
            // here (the same contract as the allocation slow paths).
            if let Some((thread, _guard)) = jit_thread_mut() {
                thread.tlab.retire();
                crate::runtime::interpreter::maybe_gc_forced_pub_at(vm, thread, "jit-ldc-string");
            }
            match crate::vm::try_create_java_string_reporting(vm, &text).or_else(|| {
                jit_ldc_last_ditch(vm, || {
                    crate::vm::try_create_java_string_reporting(vm, &text)
                })
            }) {
                Some((o, _)) => o,
                None => return jit_alloc_oom(vm, "Java heap space (ldc String)"),
            }
        }
    };
    record_ldc_reference(vm, holder, idx, obj, fill_as_of);
    obj.as_ptr() as i64
}

/// CP-indexed `anewarray` (0xbd) slow path — the `anewarray` sibling of
/// [`jit_new_object_cp`]. Resolves the COMPONENT class at run time and then
/// falls into [`jit_anewarray_object`], which owns the negative-length and
/// OOM conventions.
///
/// SAFETY: as [`jit_new_object_cp`], plus `length` is the JVM `int` array
/// length already on the operand stack.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_anewarray_object_cp(
    vm_ptr: i64,
    holder_class_id: i64,
    cp_idx: i64,
    length: i64,
) -> i64 {
    let f = || jit_anewarray_object_cp_body(vm_ptr, holder_class_id, cp_idx, length);
    contain("jit_anewarray_object_cp", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_anewarray_object_cp`], run under its panic guard.
unsafe fn jit_anewarray_object_cp_body(
    vm_ptr: i64,
    holder_class_id: i64,
    cp_idx: i64,
    length: i64,
) -> i64 {
    if vm_ptr == 0 {
        return 0;
    }
    // See `jit_new_object_cp`: resolution can run Java, so cross the boundary
    // before it, not just before the allocation.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: see `jit_new_object_cp`.
    let vm = &*(vm_ptr as *const SharedVm);
    // The index is judged under the guard the pool is read under
    // (`CpSite::index_in`, interpreter round i1 wave 43, lane L2).
    let site = CpSite::of(holder_class_id, cp_idx);
    // JVMS §anewarray resolves the component class but performs no `new`-style
    // access check (that belongs to `new`), matching `Instruction::Anewarray`.
    let component_id = match jit_resolve_cp_class(vm, site, false) {
        Ok((id, _)) => id,
        Err(sentinel) => return sentinel,
    };
    jit_anewarray_object(vm_ptr, i64::from(component_id.as_u32()), length)
}

/// Post-allocation init shared by the three JIT slow-path allocation arms:
/// primitive-field default values + JLS §12.6 finalizer registration.
///
/// Fast path: a lock-free per-class recipe from `SharedVm::jit_alloc_class_cache`
/// — no `class_manager.read()`, no hierarchy walk. First allocation of a class
/// computes the recipe under the read lock and publishes it; the legacy
/// two-lookup path remains as the fallback (cache opt-out via
/// `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE`, dense-range overflow, or a class not
/// yet in the store).
pub(crate) fn jit_post_alloc_init(vm: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    use crate::jit::alloc_class_cache::{ClassAllocInfo, PrimKind};

    // gcg g6/jm: the VM's own reading, not a process static.
    let cached = if vm.jit.jit_alloc_class_cache.enabled() {
        match vm.jit.jit_alloc_class_cache.get(class_id.as_u32()) {
            Some(info) => Some(info),
            None => {
                // First slow-path allocation of this class: build the recipe
                // with the SAME walk as `jit_init_primitive_fields` so cached
                // and uncached behavior are bit-identical. Publish only a
                // complete recipe — bail to the legacy path if any class in
                // the hierarchy is missing from the store. The builder is
                // shared with the interpreter's `gc_alloc_object`, which reads
                // the same recipe.
                let recipe = {
                    let cm = vm.classes.class_manager.read();
                    ClassAllocInfo::build(&cm.class_store, class_id)
                };
                recipe.and_then(|r| vm.jit.jit_alloc_class_cache.insert(class_id.as_u32(), r))
            }
        }
    } else {
        None
    };

    if let Some(info) = cached {
        // An int-family default needs no store: a zeroed body decodes as
        // Int(0) in both legacy and compact layouts.
        for &(inst_idx, kind) in info.prim_inits.iter() {
            if kind == PrimKind::Int {
                continue;
            }
            vm.mem
                .heap
                .set_field(obj, inst_idx as usize, kind.default_value());
        }
        // Legacy Value cells decode an all-zero reference slot as Int(0), so
        // reference-only classes must pass through this helper and get the
        // typed JVM default. Compact cells also accept Object(None) as zero.
        for &inst_idx in info.ref_inits.iter() {
            vm.mem
                .heap
                .set_field(obj, inst_idx as usize, cratonvm_types::Value::Object(None));
        }
        if info.has_finalizer {
            vm.register_finalizable(obj.as_ptr() as usize);
        }
        // gcg g4/rs H6, applied by g4/jm: opt-in discovery at allocation (one
        // relaxed load with it off).
        crate::memory::ref_discovery::note_allocation(vm, obj, class_id);
        return;
    }

    // Legacy fallback: per-allocation class-manager lookups.
    jit_init_primitive_fields(vm, obj, class_id);
    let has_fin = vm
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_fin {
        vm.register_finalizable(obj.as_ptr() as usize);
    }
    crate::memory::ref_discovery::note_allocation(vm, obj, class_id);
}

/// Whether the cached recipe has no reference defaults, no nonzero primitive
/// defaults, and no finalizer registration. A cache miss takes the helper so
/// it can populate the recipe. Int-only classes still skip the call because a
/// zeroed body already decodes as Int(0).
#[inline]
fn jit_post_alloc_init_is_noop(vm: &SharedVm, class_id: ClassId) -> bool {
    use crate::jit::alloc_class_cache::PrimKind;
    vm.jit.jit_alloc_class_cache.enabled()
        && vm
            .jit
            .jit_alloc_class_cache
            .get(class_id.as_u32())
            .is_some_and(|info| {
                !info.has_finalizer
                    // gcg g4/rs H6, SBR3 `ref`: a reference class owes the helper
                    // its allocation-time registration, which the recipe records
                    // as its kind (every other class owes nothing, engaged or not).
                    && info.reference_kind == 0
                    && info.ref_inits.is_empty()
                    && info
                        .prim_inits
                        .iter()
                        .all(|&(_, kind)| kind == PrimKind::Int)
            })
}

/// zki: [`jit_post_alloc_init_is_noop`] for an object the emitter reserved
/// COMPACT: every JVM default -- int-family, long, float, double and
/// reference -- is the zero bytes the TLAB already holds, so only a
/// finalizer or a `java.lang.ref` registration is owed. A cache miss takes
/// the helper (which fills the recipe). Per allocation, so no switch is read
/// here; the compile-time gate keeps its own (`CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM`).
#[inline]
fn jit_post_alloc_init_is_noop_compact(vm: &SharedVm, class_id: ClassId) -> bool {
    vm.jit.jit_alloc_class_cache.enabled()
        && vm
            .jit
            .jit_alloc_class_cache
            .get(class_id.as_u32())
            .is_some_and(|info| !info.has_finalizer && info.reference_kind == 0)
}

/// Initialize the JVM default values of a newly allocated object (JIT fallback).
///
/// The cached helper is preferred. This hierarchy walk remains the fallback
/// when class metadata cannot be cached; it writes typed reference defaults as
/// well as nonzero primitive defaults. Reference-only classes now reach the
/// helper through needs_post_alloc_init, closing the raw-zero mismatch for
/// compiled TLAB allocations too.
fn jit_init_primitive_fields(vm: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    use crate::runtime::interpreter::gc_and_alloc::jvm_default_for_descriptor;

    let cm = vm.classes.class_manager.read();
    let store = &cm.class_store;
    let mut cid = Some(class_id);
    while let Some(current_id) = cid {
        if let Some(class) = store.get(current_id) {
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() {
                    continue;
                }
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                vm.mem
                    .heap
                    .set_field(obj, inst_idx, jvm_default_for_descriptor(desc_first));
                inst_idx += 1;
            }
            cid = class.superclass;
        } else {
            break;
        }
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// component_class_id_raw is the ClassId of the array's component type. length is non-negative.
// Returns a raw heap pointer to a newly allocated reference array.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// JIT monitorenter helper. The common path is the mark-word thin-lock CAS in
/// `MonitorTable::enter_or_contend`; only actual contention enters the
/// GC-blocked parking protocol. Returning the possibly remapped receiver gives
/// generated code an unambiguous non-sentinel success value.
pub unsafe extern "C" fn jit_monitor_enter(vm_ptr: i64, obj_ptr: i64) -> i64 {
    let f = || jit_monitor_enter_body(vm_ptr, obj_ptr);
    contain("jit_monitor_enter", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of [`jit_monitor_enter`], run under its panic guard.
unsafe fn jit_monitor_enter_body(vm_ptr: i64, obj_ptr: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_monitor_screen_vm_ptr("jit_monitor_enter", vm_ptr, obj_ptr);
    if obj_ptr == 0 {
        // JIT round 20 (lane throw; fuzz `R20FuzzExcMsg1` site 6): name the
        // opcode family, so the drain's JEP 358 rebuild accepts the bci the
        // monitor call's published safepoint left in the frame and prints
        // HotSpot's `Cannot enter synchronized block because "..." is null`.
        // With no action the corroboration refused it and the compiled NPE
        // had `getMessage() == null`, where the interpreter's had the text.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::MONITOR_ENTER,
        );
        return i64::MIN;
    }
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    let Some((thread, _guard)) = jit_thread_mut() else {
        // r10 help2: same idiom as `native_integer_value_of`/`native_long_value_of`'s
        // "no live JIT thread" arm — without this, `jit_bridge`'s post-JIT path
        // takes its `result == i64::MIN` arm only `&& deopt_signaled`. With every
        // signal clear it pushed this `i64::MIN` as a real value, and this helper
        // answers a REFERENCE (the possibly-remapped receiver): a wild pointer in
        // an oop slot. Belt and braces: `jit_thread_mut` failing here should be
        // unreachable (a compiled frame always installs `JIT_THREAD` first), but
        // so was the callee-deopt-body case this pattern was copied from.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    let obj = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    // gcd d2/i: the compiled door arms the opt-in blocked-monitor proof
    // (`CRATONVM_XT_BLOCKED_MONITOR_PROOF`); off, it is `monitor_enter_blocking`.
    crate::vm::vm_exec::monitor_enter_blocking_from_compiled(vm, thread, obj).as_ptr() as i64
}

/// Whether `vm_ptr` cannot be a `SharedVm`: null or in the first page,
/// misaligned, or inside the calling thread's own stack. A `SharedVm` lives
/// in an `Arc` on the Rust heap, never on a thread stack, so the stack test
/// is exact, not a heuristic. Cost: one memoized TLS read and three compares.
#[inline]
fn jit_monitor_vm_ptr_is_wild(vm_ptr: i64) -> bool {
    let p = vm_ptr as usize;
    p < 4096
        || p % 8 != 0
        || cratonvm_gc::shadow_stack::current_thread_stack()
            .is_some_and(|band| (band.lo..band.hi).contains(&p))
}

/// Entry screen of the two compiled monitor helpers (gcd d1/mon,
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// The optimizing tier handed these helpers the SAVED CALLER RBP as their VM
/// pointer: `ir_lower::scan_frame_needs` reserved no context slot for a
/// method whose only context-taking nodes were its monitors, so
/// `emit_monitor_stub` loaded `[rbp - 0]`. The helpers then read a "heap" and
/// a "lock-slot table" out of stack memory and faulted far from the cause
/// (`flush_thread_satb`, `LockSlots::lease`). JIT round 13 fixes the
/// reservation; this screen turns any recurrence -- from either tier -- into
/// one attributable report instead of a wild dereference.
///
/// It must not unwind: both helpers run under `contain(.., OnPanic::Throw {
/// vm_ptr })`, which would build the throwable through the very pointer that
/// is wrong. So it reports and aborts.
#[inline]
fn jit_monitor_screen_vm_ptr(helper: &'static str, vm_ptr: i64, obj_ptr: i64) {
    if jit_monitor_vm_ptr_is_wild(vm_ptr) {
        jit_monitor_wild_vm_ptr(helper, vm_ptr, obj_ptr);
    }
}

/// The report half of [`jit_monitor_screen_vm_ptr`]. Names the compiled
/// caller through the inline frame record (the RBP and compile-id mirrors the
/// caller's prologue published; the monitor stub republishes only AFTER the
/// call, so at helper entry they still name the caller), says whether that
/// artifact takes the context ABI at all, and dumps the top of its frame:
/// `[rbp]` is the saved caller RBP, so `vm_ptr == [rbp]` is the unreserved
/// context slot's signature, and `[rbp-8*k]` are the locals and the context
/// slot (`(num_locals + 1) * 8` when reserved).
#[cold]
#[inline(never)]
fn jit_monitor_wild_vm_ptr(helper: &'static str, vm_ptr: i64, obj_ptr: i64) -> ! {
    let band = cratonvm_gc::shadow_stack::current_thread_stack();
    let rbp = crate::jit::conservative_roots::top_rbp_mirror_read();
    let id = crate::jit::conservative_roots::top_cm_id_mirror_read();
    let caller = cratonvm_jit::lookup_compile_id(id).map(|p| {
        // SAFETY: a published compile id names a live artifact: its frame is
        // the one calling this helper, so the body cannot have been reclaimed.
        let cm = unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
        (cm.method_label_shared(), cm.needs_context())
    });
    let mut words = String::new();
    let rbp_in_stack =
        rbp % 8 == 0 && band.is_some_and(|b| rbp >= b.lo + 64 && rbp.saturating_add(8) <= b.hi);
    if rbp_in_stack {
        for k in 0..=8usize {
            // SAFETY: `[rbp - 64, rbp + 8)` lies inside this thread's own
            // stack band (checked above) and is 8-aligned.
            let w = unsafe { *((rbp - k * 8) as *const usize) };
            let off = k * 8;
            words.push_str(&format!(" [rbp-{off}]={w:#x}"));
        }
    }
    let saved_rbp_matches = rbp_in_stack
        // SAFETY: as above.
        && unsafe { *(rbp as *const usize) } == vm_ptr as usize;
    let caller = caller
        .as_ref()
        .map(|(label, needs_context)| format!("{label} needs_context={needs_context}"));
    let band = band.map(|b| (b.lo as *const u8, b.hi as *const u8));
    // `concat!` keeps the literal on one logical line; a macro-built format
    // string cannot capture identifiers, so every argument is named.
    eprintln!(
        concat!(
            "[cratonvm] FATAL {helper}: vm_ptr={vm:#x} is not a SharedVm (obj={obj:#x}, ",
            "thread stack={band:?}); compiled caller compile_id={id} {caller:?} rbp={rbp:#x} ",
            "vm_ptr==[rbp](saved caller rbp: unreserved context slot)={matches};{words}",
        ),
        helper = helper,
        vm = vm_ptr as usize,
        obj = obj_ptr as usize,
        band = band,
        id = id,
        caller = caller,
        rbp = rbp,
        matches = saved_rbp_matches,
        words = words,
    );
    std::process::abort()
}

#[cold]
fn stash_jit_monitor_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
) {
    // Round 12 wave 4: a runtime error whose conversion itself failed (a
    // `NotImplemented`, or an allocation failure building the throwable) used
    // to stash NOTHING while the caller still returned the `i64::MIN`
    // sentinel; the shared helper wraps it in an `InternalError` instead.
    // With nothing constructible at all, raise the out-of-band signal (the
    // no-thread idiom of `jit_monitor_enter_body`) so the bare sentinel is not
    // taken as a value.
    let convert = helper_failure_conversion_on();
    if !stash_helper_failure(
        vm,
        thread,
        err,
        &"JIT monitor operation failed",
        true,
        convert,
    ) && convert
    {
        set_jit_deopt_pending();
    }
}

/// JIT monitorexit helper. A successful thin unlock is one release CAS. An
/// ownership failure becomes the ordinary catchable
/// `IllegalMonitorStateException` and is reported with the common JIT sentinel.
///
/// Releases through [`crate::vm::vm_exec::monitor_exit_and_retract_jmx`], NOT
/// through a bare `monitors.exit`. `jit_monitor_enter` reaches
/// `complete_jmx_monitor_enter` by way of `monitor_enter_blocking`, so a
/// compiled `monitorenter` publishes into the thread's `jmx_locked_monitors`
/// exactly as the interpreter's does; a bare release here left every one of
/// those publishes standing. The list is membership-scanned on each
/// acquisition, so it grew to every distinct object the thread had ever locked
/// from compiled code and made `monitorenter` quadratic — on `ZipContentTests`,
/// `complete_jmx_monitor_enter` + `remove_jmx_locked_monitor` were **54% of the
/// run**, which is the bulk of the JIT's CPU deficit against `--nojit` on
/// lock-dense code. The entries also pinned their objects as GC roots forever.
pub unsafe extern "C" fn jit_monitor_exit(vm_ptr: i64, obj_ptr: i64) -> i64 {
    let f = || jit_monitor_exit_body(vm_ptr, obj_ptr);
    contain("jit_monitor_exit", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of [`jit_monitor_exit`], run under its panic guard.
unsafe fn jit_monitor_exit_body(vm_ptr: i64, obj_ptr: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_monitor_screen_vm_ptr("jit_monitor_exit", vm_ptr, obj_ptr);
    if obj_ptr == 0 {
        // JIT round 20 (lane throw): as `jit_monitor_enter`'s null arm.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::MONITOR_EXIT,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let Some((thread, _guard)) = jit_thread_mut() else {
        // r10 help2: same "no live JIT thread" idiom as `jit_monitor_enter_body`
        // just above (and `native_integer_value_of`'s twin below) — without the
        // flag, `jit_bridge`'s post-JIT gate (`result == i64::MIN && deopt_signaled`,
        // applied uniformly regardless of the Java return type — see
        // `runtime/interpreter/jit_bridge.rs` around the "MEDIUM fix" comment)
        // pushes this bare `i64::MIN` as a real return value instead of routing
        // through the exception/deopt path.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    let obj = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    match crate::vm::vm_exec::monitor_exit_and_retract_jmx(vm, obj, thread.thread_id) {
        Ok(()) => 1,
        Err(err) => {
            stash_jit_monitor_error(vm, thread, jit_monitor_exit_java_error(err));
            i64::MIN
        }
    }
}

/// `CRATONVM_JIT_MONITOR_EXIT_NULL_IMSE_TEXT` -- default ON (JIT round 21
/// wave 4, lane monitors): a compiled `monitorexit` the monitor table refuses
/// raises HotSpot's null-message `IllegalMonitorStateException`, as the
/// interpreter's `monitorexit` does (`held_monitors::release_or_imse`); the
/// table's text ("thread N does not own the monitor for object at 0x...")
/// names a thread id and a heap address, so it goes to the log. `0|false|off|
/// no`: that text is the exception's message, as before. Cold (read only on
/// a refused release).
#[cold]
fn jit_monitor_exit_java_error(
    err: crate::error::MethodCallFailed,
) -> crate::error::MethodCallFailed {
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalMonitorStateException { message },
        )) if cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_MONITOR_EXIT_NULL_IMSE_TEXT",
        ) =>
        {
            tracing::warn!(
                detail = %message,
                "compiled monitorexit: the monitor table refused the release"
            );
            crate::runtime::interpreter::held_monitors::imse_no_message()
        }
        other => other,
    }
}

pub unsafe extern "C" fn jit_anewarray_object(
    vm_ptr: i64,
    component_class_id_raw: i64,
    length: i64,
) -> i64 {
    let f = || jit_anewarray_object_body(vm_ptr, component_class_id_raw, length);
    contain("jit_anewarray_object", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_anewarray_object`], run under its panic guard.
unsafe fn jit_anewarray_object_body(vm_ptr: i64, component_class_id_raw: i64, length: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    // ZGC / G1 concurrent-mark start, exactly as `jit_newarray` and
    // `jit_new_object` do. This allocator was the one path that skipped it, so
    // an `Object[]`-heavy loop never started a concurrent cycle.
    if vm_ptr != 0 {
        // SAFETY: same provenance as the `&*(vm_ptr as *const SharedVm)` below.
        jit_maybe_start_zgc_concurrent_mark(&*(vm_ptr as *const SharedVm));
        jit_drive_g1_concurrent_mark(&*(vm_ptr as *const SharedVm));
        jit_take_gen_inline_concurrent_start(&*(vm_ptr as *const SharedVm));
    }
    // BUGFIX: see `jit_newarray` — narrow length to int payload and sign-extend.
    // JLS only allows `int` array lengths; defensive against JIT slot patterns
    // that carry stale upper bits (e.g. NaN-boxed CompactValue raw bits).
    let length = length as i32 as i64;
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr is a valid SharedVm pointer per the caller contract.
    let vm = &*(vm_ptr as *const SharedVm);
    // zr z3/ja: the ZGC inline reference-array bump's positive control (per
    // VM; `CRATONVM_DBG_JIT_METHOD_STATS`). `small` = an array the inline arm
    // would have served had its TLAB had room.
    vm.jit.jit_alloc_class_cache.new_paths.note_anewarray(
        length >= 0
            && cratonvm_types::array_data_size_checked(
                length as usize,
                ArrayElementType::Reference,
            )
            .is_some_and(|d| {
                cratonvm_types::ARRAY_DATA_OFFSET + d <= cratonvm_gc::tlab::tlab_max_alloc()
            }),
    );
    if length < 0 {
        // Negative length → NegativeArraySizeException (JLS). See `jit_newarray`:
        // stash it in the pending-exception channel + return the 0/null sentinel
        // so the `anewarray` codegen's emit_post_alloc_oom_check bail routes it
        // through the method's exception table (catchable).
        return jit_negative_array_size(vm, length);
    }
    // The VM-limit refusal, before the heap is touched -- see `jit_newarray`.
    if crate::runtime::interpreter::array_length_exceeds_vm_limit(length as usize) {
        return jit_alloc_oom(vm, crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT);
    }
    let heap = &vm.mem.heap;
    let class_id = ClassId::new(component_class_id_raw as u32);

    // CRIT (jit/gc audit, 2026-05): probe young-gen capacity and retire
    // the calling thread's TLAB before triggering GC. See
    // `jit_new_object` / `jit_newarray` for the full rationale —
    // without the retire, the heap walker steps into TLAB tail bytes
    // and mis-decodes them as object headers when GC fires from this
    // slow path.
    // W1-vm, as in `jit_newarray`: an overflowing size is an allocation that
    // can never succeed. `unwrap_or(0)` probed with size 0, which always
    // passes, and handed the impossible length onward.
    let Ok(data_size) =
        cratonvm_types::array_data_size(length as usize, ArrayElementType::Reference)
    else {
        return jit_newarray_oom(vm, length as usize);
    };
    let total_size = cratonvm_types::ARRAY_DATA_OFFSET + data_size;
    // Lock-free TLAB bump first — see the same arm in `jit_newarray` for why
    // this is the only path a JIT-compiled array allocation has, and what the
    // global `young_from` mutex below cost when it was the only option.
    if let Some((thread, _guard)) = jit_thread_mut() {
        if let Some(arr) = crate::runtime::interpreter::tlab_alloc_array_guarded_refill(
            thread,
            vm,
            class_id,
            ArrayElementType::Reference,
            length as usize,
        ) {
            return arr.as_ptr() as i64;
        }
    }
    // gen r4w4/alloc4: lock-free bump-tail headroom first — see `jit_new_object`.
    // gcd d2/j: and the futile-young backoff, as in `jit_new_object` (an
    // `Object[]` under the TLAB cap spills like an object when young is full
    // of live data; a spill that fails still collects below).
    let mut collected_here = false;
    if !heap.young_bump_headroom(total_size)
        && heap.try_alloc_young_probe(total_size).is_none()
        && !crate::runtime::interpreter::futile_young_backoff_skips(vm)
    {
        // zr z3/ja: stamped as in `jit_new_object` (every collector since
        // gci i48/zl2). A dropped request leaves
        // `collected_here` false, so a failing allocation below still forces
        // its own collection instead of reading the drop as one.
        let stamp = crate::runtime::interpreter::gc_and_alloc::zgc_gc_request_stamp(vm);
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            // zr z3/t2: its own site name, as in `jit_new_object` (an
            // occupancy verdict on ZGC).
            if crate::runtime::interpreter::gc_and_alloc::maybe_gc_forced_stamped_pub_at(
                vm,
                thread,
                "jit-anewarray-young-probe",
                stamp,
            ) {
                crate::runtime::interpreter::note_forced_young_cycle_outcome(vm, total_size);
                collected_here = true;
            }
        }
    }

    // `anewarray` has the same allocation-boundary headroom contract as
    // primitive `newarray`: a failed probe is not the only reason to collect.
    // Large live reference arrays can otherwise consume the remaining Eden
    // budget immediately before a heap-filling phase, leaving no evacuation
    // destination for the next pause. This is the reference-array counterpart
    // to the G1 guard in `jit_newarray_body` above.
    if heap.is_g1() && heap.needs_gc_for_jit_allocation() && !collected_here {
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            // gci i12/ge: as `jit_newarray_body`'s guard.
            crate::runtime::interpreter::maybe_gc_forced_pub_at(
                vm,
                thread,
                crate::runtime::interpreter::gc_and_alloc::jit_array_headroom_site(
                    vm,
                    total_size,
                    "jit-anewarray-headroom",
                ),
            );
            collected_here = true;
        }
    }

    // GC-overhead limit (see jit_newarray): bail to catchable OOM if forced GCs
    // keep freeing almost nothing, instead of death-spiralling on slivers --
    // after the soft-reference rung, which earns one ordinary retry only when
    // it reset the streak (`jit_latched_overhead_limit_throws`).
    // gcf f2/o: `graced` -- see `jit_newarray_body`; counted on success below.
    // JIT round 22 wave 12 (lane memory, `CRATONVM_JIT_ALLOC_LATCH_AFTER_FAILURE`,
    // default ON): not before an attempt -- the failure arm below consults
    // the streak once the fallible allocation failed, as the interpreter's
    // ladder does. `=0` restores this early exit.
    let mut graced = false;
    if crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
        && !crate::runtime::interpreter::gc_and_alloc::jit_alloc_latch_after_failure_enabled()
        && jit_latched_overhead_limit_throws(vm)
    {
        graced = jit_latched_grace(vm);
        if !graced {
            jit_oome_ladder_step(vm, "jit-anewarray latched-exit-before-attempt throws");
            return jit_alloc_oom(
                vm,
                &format!(
                    "Java heap space (anewarray component {component_class_id_raw} length {length})"
                ),
            );
        }
    }
    // Fallible young → humongous/old-gen alloc (preserves alloc_array's spill);
    // on exhaustion surface a catchable OutOfMemoryError instead of the hard
    // abort in alloc_young. The `anewarray` codegen's emit_post_alloc_oom_check
    // bails on the 0/null sentinel and routes the OOME through the method's
    // exception table (matching the interpreter's gc_alloc_array).
    let mut arr = heap.try_alloc_array_full(class_id, ArrayElementType::Reference, length as usize);
    // gcf f4/o: the ladder starts at this attempt's failure
    // (`[GC] oome_ladder_doors:` `jit_*`).
    let laddered = arr.is_none();
    if laddered {
        crate::runtime::interpreter::gc_and_alloc::note_ladder_door_entered(
            vm,
            crate::runtime::interpreter::gc_and_alloc::LadderDoor::Jit,
        );
        jit_oome_ladder_begin(vm, "door=jit-anewarray");
    }
    // gc-common w36-e (`common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications`):
    // the probe above is no promise. G1 and ZGC answer it with `!needs_gc()`,
    // a LIVE-bytes threshold that cannot see a bump space ZGC never rewinds, so
    // on a heap full of dropped garbage it said "room" and nothing collected;
    // the last-ditch rung below is a no-op on ZGC; and the helper threw
    // `OutOfMemoryError` with the whole heap reclaimable. Force one collection
    // and retry before the last-ditch rung, as `jit_new_object` and
    // `jit_newarray` do and as the interpreter's `gc_alloc_array` always has.
    // The helper holds no object reference across the collection.
    // JIT round 22 wave 12 (`CRATONVM_JIT_ALLOC_LATCH_AFTER_FAILURE`): with
    // the entry check stood down, a failed attempt after the young-probe
    // collection (`collected_here`) still meets the latched exit below, with
    // no second collection.
    let forced_here = arr.is_none() && !collected_here;
    let latch_after_failure = arr.is_none()
        && collected_here
        && crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
        && crate::runtime::interpreter::gc_and_alloc::jit_alloc_latch_after_failure_enabled();
    if forced_here || latch_after_failure {
        if forced_here {
            if let Some((thread, _guard)) = jit_thread_mut() {
                thread.tlab.retire();
                // gci i12/ge: `G1 Humongous Allocation` for a G1 humongous
                // request (`jit_array_gc_site`; `jit-helpers` otherwise).
                // zt t20/fl1: the unstamped door (the zt t4/ca2 stamped failure
                // door is retired with `CRATONVM_ZGC_JIT_FAILURE_DOOR_STAMPED`).
                crate::runtime::interpreter::maybe_gc_forced_pub_at(
                    vm,
                    thread,
                    crate::runtime::interpreter::gc_and_alloc::jit_array_gc_site(vm, total_size),
                );
            }
            jit_oome_ladder_step(vm, "forced-collection");
        }
        // gcd d1/b: the same latched-limit rule as the check above (and as
        // every sibling helper), not an immediate throw on a verdict only
        // this young cycle built -- see `jit_latched_overhead_limit_throws`.
        if crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
            && jit_latched_overhead_limit_throws(vm)
        {
            let graced_here = jit_latched_grace(vm);
            if !graced_here {
                jit_oome_ladder_step(vm, "jit-anewarray latched-exit throws");
                return jit_alloc_oom(
                    vm,
                    &format!(
                        "Java heap space (anewarray component {component_class_id_raw} length {length})"
                    ),
                );
            }
            graced = true;
        }
        arr = heap.try_alloc_array_full(class_id, ArrayElementType::Reference, length as usize);
    }
    let arr = match arr {
        Some(a) => a,
        None => {
            // G1 last-ditch full cycle + one retry (see jit_newarray), with an
            // attempt between its two full collections.
            let between = jit_g1_last_ditch_attempting(vm, || {
                heap.try_alloc_array_full(class_id, ArrayElementType::Reference, length as usize)
            });
            let between = match between {
                Err(()) => {
                    return jit_alloc_oom(
                        vm,
                        &format!(
                            "Java heap space (anewarray component {component_class_id_raw} length {length})"
                        ),
                    );
                }
                Ok(between) => between,
            };
            match between.or_else(|| {
                heap.try_alloc_array_full(class_id, ArrayElementType::Reference, length as usize)
            }) {
                Some(a) => a,
                None => {
                    return jit_alloc_oom(
                        vm,
                        &format!(
                            "Java heap space (anewarray component {component_class_id_raw} length {length})"
                        ),
                    )
                }
            }
        }
    };
    if graced {
        crate::runtime::interpreter::gc_and_alloc::note_latched_grace_outcome(vm, true);
    }
    if laddered {
        crate::runtime::interpreter::gc_and_alloc::note_ladder_door_allocated(
            vm,
            crate::runtime::interpreter::gc_and_alloc::LadderDoor::Jit,
        );
    }
    // Non-TLAB arm (the guarded-refill TLAB arm above returns early).
    jit_note_external_alloc(vm, total_size);
    arr.as_ptr() as i64
}

// ---------------------------------------------------------------------------
// Array element access helpers
