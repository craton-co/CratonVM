#[allow(unused_imports)]
use super::*;

/// JIT round 19 proposal wave 6, lane sync6 (REVIEW6-2; unconditional,
/// fail-closed): is the hold a `static synchronized` frame hands over (index
/// `i` of `rframe.monitors`) this VM's class mirror of `declaring_class_id`?
/// The JIT's [`cratonvm_jit::deopt::handed_method_monitor_index`] cannot know
/// the mirror and answers the static arm from the frame's shape (or its
/// `method_monitor_first` mark) alone; this is the one place with the map in
/// scope. A class with no mirror, or any other object, answers `false`, so the
/// frame is treated as handing nothing over (its monitor stays a block monitor
/// or the frame re-runs), exactly as for a foreign object on the instance arm.
pub(in crate::runtime::interpreter::deopt_resume) fn static_handed_hold_is_the_mirror(
    shared: &SharedVm,
    declaring_class_id: crate::classloading::ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    i: usize,
) -> bool {
    let held = match rframe.monitors.get(i).map(|m| &m.object) {
        Some(cratonvm_jit::deopt::FrameValue::Object(held)) if *held != 0 => *held,
        _ => return false,
    };
    shared
        .classes
        .class_mirrors
        .read()
        .get(&declaring_class_id)
        // Cast: object pointer to the raw word a stash carries.
        .is_some_and(|mirror| mirror.as_ptr() as usize as u64 == held)
}

/// The index in `rframe.monitors` of the handed-over method monitor
/// ([`frame_hands_over_method_monitor`]); cheap `None` for every frame that
/// is not a synchronized method's naming exactly one monitor.
///
/// Round 13 wave 11 (lane sync7,
/// `r13w10-sync6-static-self-lock-deopt-hand-over-patch-FIXED-20260929.md`): a
/// `static synchronized` method's frame too, whose handed hold is its class
/// mirror (a static self-locking body's guard,
/// `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER`). Every consumer reads it
/// the same way as an instance one: the builders make it `monitor_on_exit`,
/// the doors and `try_resume_trapped_callee` resume the frame although they
/// hold no monitor of their own, and every abandoning sink releases it as a
/// compiled lock (`CompiledLocksOfAStash`).
///
/// JIT round 19 proposal wave 6, lane sync6 (REVIEW6-2): the static arm is
/// also checked against the declaring class's mirror
/// ([`static_handed_hold_is_the_mirror`]), here, so every consumer answers the
/// same.
pub(in crate::runtime::interpreter::deopt_resume) fn handed_method_monitor_at(
    shared: &SharedVm,
    declaring_class_id: crate::classloading::ClassId,
    is_synchronized: bool,
    is_static: bool,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<usize> {
    // JIT round 18 wave 9 (lane syncdoor6, SD17-3): a frame MARKED
    // `method_monitor_first` (a self-locking body with `synchronized` blocks)
    // names the method monitor first and its open blocks after it.
    if !is_synchronized
        || rframe.monitors.is_empty()
        || (rframe.monitors.len() != 1 && !rframe.semantics.method_monitor_first)
    {
        return None;
    }
    let i = cratonvm_jit::deopt::handed_method_monitor_index(
        cratonvm_jit::deopt::SinkMethodMonitor::of(is_synchronized, is_static),
        cratonvm_jit::bytecode_holds_monitor(code, code.len()),
        rframe,
    )?;
    if is_static && !static_handed_hold_is_the_mirror(shared, declaring_class_id, rframe, i) {
        return None;
    }
    Some(i)
}

/// Round 13 wave 9 (lane chain4): a door that does NOT resume a stashed frame
/// and re-runs its method from entry instead abandons the activation the frame
/// describes, so what that activation's compiled code locked is released first,
/// as `resume_or_despeculate_stash` does on its own re-run arm. For the arm of
/// a synchronized method's door that holds no monitor of its own
/// (`CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP`), which re-ran without asking and so
/// left a nested self-locking callee's handed monitor (or any lock a foreign
/// stash's compiled code took) held for good. Kill switch
/// `CRATONVM_DEOPT_UNRESUMED_STASH_RELEASES=0`. Returns how many single releases
/// succeeded.
pub(crate) fn release_locks_of_an_unresumed_stash(
    shared: &SharedVm,
    thread: &mut JvmThread,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> u32 {
    if rframe.monitors.is_empty()
        && rframe
            .caller_frames
            .iter()
            .all(|scope| scope.monitors.is_empty())
    {
        return 0;
    }
    CompiledLocksOfAStash::pin_compiled_locks(thread, rframe).release_for_a_rerun(shared, thread)
}

/// JIT round 19 proposal wave 6, lane sync6 (SELFLOCK19-3): the thread-exit
/// monitor census. Folds the inflated monitors terminating Java threads still
/// owned (`MonitorTable::take_thread_exit_releases`) into this VM's self-lock
/// census row `monitors-held-at-thread-exit`, and prints
/// `[cratonvm] monitors-held-at-thread-exit=N` under
/// `CRATONVM_DBG=jit-method-stats` or `CRATONVM_DBG_DEOPT` (the row may be
/// folded after the census line printed). Java code cannot leave a monitor
/// held at thread exit, so a non-zero `N` is a leak from some path, compiled
/// or not: the end-to-end check every drop-site argument is trying to prove.
/// Not covered: thin locks, and the threads that end without the termination
/// sweep (the main thread at VM exit). Reporting only.
pub(in crate::runtime::interpreter::deopt_resume) fn report_thread_exit_monitor_census(shared: &SharedVm) {
    let n = shared.threads.monitors.take_thread_exit_releases();
    note_drop_census_row(
        shared,
        cratonvm_jit::self_lock_census::SelfLockRow::MonitorsHeldAtThreadExit,
        n,
    );
    if cratonvm_types::flags().jit.method_stats || dbg_deopt_enabled() {
        eprintln!("[cratonvm] monitors-held-at-thread-exit={n}");
    }
}

/// This VM's self-lock census, `row` by `n` (see
/// `CompiledLocksOfAStash::note_drop_census`).
pub(in crate::runtime::interpreter::deopt_resume) fn note_drop_census_row(
    shared: &SharedVm,
    row: cratonvm_jit::self_lock_census::SelfLockRow,
    n: u64,
) {
    shared
        .jit
        .tiered_manager
        .verdicts()
        .self_lock_census()
        .note_n(row, n);
}

/// [`build_deopt_frame_inner`], answering WHY it declined (round 12 wave 5,
/// lane replay3; `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`
/// cause 1).
///
/// `release_held_on_heap_failure` is the sink's promise that it will THROW on
/// [`DeoptFrameBail::VirtualMaterialiseHeap`]
/// ([`rematerialisation_out_of_memory`]) rather than re-run or restash: with
/// it (and `CRATONVM_DEOPT_REMATERIALISE_OOM` on), a heap failure first
/// releases the monitors the compiled code holds for the abandoned activation.
/// Without it the answer is the same `None` as before, only counted under its
/// own name.
pub(crate) fn build_deopt_frame_or_refusal(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    stress: bool,
    release_held_on_heap_failure: bool,
) -> Result<crate::runtime::frame::Frame, DeoptFrameBail> {
    // Single non-inlined frame (inlined-caller chains stay out of scope). Held
    // monitors are NO LONGER a blanket bail. Each `MonitorInfo` says whether the
    // resume must acquire it:
    //   * `relock == true` — an ELIDED lock. The single-pass backend's Phase C
    //     records `synchronized(scalarObj)` blocks this way, and the optimizing
    //     tier marks every monitor whose ops lock elision deleted. The object is
    //     materialized (if virtual) and locked `lock_depth` times below.
    //   * `relock == false` — a lock the compiled code took through the monitor
    //     helper (the optimizing tier; the single-pass tier since wave 26,
    //     where its analysis can name the lock). The thread still holds it; the resumed
    //     frame's `monitorexit` releases it. It is pinned and checked like any
    //     reference and otherwise left alone.
    if !rframe.caller_frames.is_empty() {
        note_deopt_frame_bail(DeoptFrameBail::InlinedChain, rframe);
        return Err(DeoptFrameBail::InlinedChain);
    }

    // Identity gate (jit-invokedynamic-groovy-regression), belt-and-suspenders
    // for every caller: the frame's baked `method_key` must name the method
    // whose bytecode/frame-shape (`cached`) we are about to materialize it
    // into. A mismatched (nested-callee) or key-less (legacy producer /
    // superseded-guard sentinel) frame bails to the safe re-run. Also bounds-
    // check the resume bci against this method's code — the superseded-guard
    // sentinel stashes `bci == u32::MAX` precisely so this fails.
    if !deopt_frame_matches_method(
        rframe,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        note_deopt_frame_bail(DeoptFrameBail::IdentityMismatch, rframe);
        return Err(DeoptFrameBail::IdentityMismatch);
    }
    if rframe.bci == u32::MAX {
        note_deopt_frame_bail(DeoptFrameBail::SupersededSentinel, rframe);
        return Err(DeoptFrameBail::SupersededSentinel);
    }
    // The frame says the interpreter may be parked at its bci, and this asks it
    // rather than assuming it. Everything below — `frame.pc = rframe.bci` at the
    // end of this function above all — is only correct for a `REEXECUTE` point.
    // See [`DeoptFrameBail::NotAResumePoint`] for why this is expected to stay
    // at zero and what a non-zero count would mean.
    if !rframe.semantics.reexecute {
        note_deopt_frame_bail(DeoptFrameBail::NotAResumePoint, rframe);
        return Err(DeoptFrameBail::NotAResumePoint);
    }

    // CRATONVM_DEOPT_VERIFY: structural-invariant check before resuming. On a
    // violation (slot count past the method maxima — e.g. a shifted snapshot — or
    // a malformed/dangling virtual descriptor) report loudly and force the safe
    // re-run. Runs on the ORIGINAL frame (virtuals intact) so the descriptor
    // checks see them. Gate is read-once; default-off ⇒ skipped entirely.
    if cratonvm_jit::deopt_verify_enabled() {
        let verdict = verify_reconstructed_frame(rframe, cached.max_locals, cached.max_stack)
            .and_then(|()| verify_reconstructed_oops(rframe, shared));
        if let Err(why) = verdict {
            eprintln!(
                "[DEOPT-VERIFY] {} bci={}: reconstructed-frame invariant violated: {why} \
                 — forcing safe re-run",
                rframe.method_key, rframe.bci
            );
            note_deopt_frame_bail(DeoptFrameBail::VerifyFailed, rframe);
            return Err(DeoptFrameBail::VerifyFailed);
        }
    }

    // Workstream A: re-materialize scalar-replaced (virtual) objects into real
    // heap shells before mapping. The reconstructed frame may carry
    // `VirtualObject`/`VirtualObjectRef` slots (escape analysis elided the
    // allocation on the fast path); the mapper below has no representation for
    // them and would bail to re-run. Materialize them into a heap object graph
    // and rewrite the slots to real `Object` refs first.
    //
    // GC-rooting: `keep_pins = true` leaves each shell pinned in
    // `native_pin_roots`; those pins ride the SAME `pin_base..truncate` window
    // `resume_real_ir_deopt` holds across `push_resumed_frame`, so the
    // shells are rooted continuously from allocation until the resumed frame
    // roots them. On any materialization failure (unsupported field / unknown
    // id) the pins are released and we fall back to re-run (`?` → `None`).
    use cratonvm_jit::deopt::FrameValue;
    // Monitors included: an elided lock on an object no local or stack slot
    // still names carries the object's only definition in its monitor entry.
    let has_virtual = rframe
        .locals
        .iter()
        .chain(rframe.stack.iter())
        .chain(rframe.monitors.iter().map(|m| &m.object))
        .any(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
            )        });
    if has_virtual && false {
        eprintln!(
            "[DBG_SCALAR_DEOPT] build_deopt_frame_inner: materializing virtual object(s) at bci={}",
            rframe.bci
        );
    }
    // The operand-stack fit, asked before anything that can allocate or GC
    // (the materialiser, the pool refill) rather than at the push, for the
    // reason `refusal_before_materialisation` gives. The interpreter stack is
    // compact (one entry per `long`/`double`), so a verifiable frame never
    // holds more entries than the JVM-slot `max_stack`.
    // Widening: u16 -> usize.
    if rframe.stack.len() > cached.max_stack as usize {
        note_deopt_frame_bail(DeoptFrameBail::StackPush, rframe);
        return Err(DeoptFrameBail::StackPush);
    }
    let materialized_frame;
    let rframe: &cratonvm_jit::deopt::ReconstructedFrame = if has_virtual {
        // Elided-monitor gate. Escape analysis performs lock elision over
        // non-escaping objects (`jit::escape_analysis::find_lock_elisions`): a
        // scalar-replaced object may have had its `monitorenter`/`monitorexit`
        // elided, so a resumed frame that later runs `monitorexit` would hit an
        // un-entered monitor.
        //
        // A `synchronized(obj)` BLOCK over such an object is described: both
        // producers record the elided lock as a `relock` `MonitorInfo` (the
        // optimizing tier from the builder's monitor stack since 2026-09-12), and
        // the relock below re-acquires it on the materialized object.
        //
        // What stays refused is an `ACC_SYNCHRONIZED` method whose receiver
        // slot is itself virtual: its method monitor is not a frame-state entry
        // at all (the door takes it, not a `monitorenter`), so an elision of it
        // under scalar replacement of the receiver would leave no trace here.
        // Round 12 wave 7 (lane replay4): ONLY that shape. No tier compiles the
        // method monitor (a synchronized body is entered through a door that
        // holds it and hands it to the resumed frame), and the monitor is the
        // class mirror or an argument this compile did not allocate, so every
        // other synchronized frame materialises and resumes. It used to refuse
        // them all and the doors re-ran the method from entry, committing its
        // prefix twice, while the compile-time replay check called the frame
        // resumable. Both ends now read one predicate
        // (`cratonvm_jit::deopt::synchronized_frame_refuses_virtuals`; kill
        // switch `CRATONVM_DEOPT_SYNC_VIRTUALS_RESUME=0` refuses every one again,
        // at both ends).
        if cratonvm_jit::deopt::synchronized_frame_refuses_virtuals(
            cratonvm_jit::deopt::SinkMethodMonitor::of(cached.is_synchronized, cached.is_static),
            &rframe.locals,
            has_virtual,
        ) {
            note_deopt_frame_bail(DeoptFrameBail::SynchronizedWithVirtuals, rframe);
            return Err(DeoptFrameBail::SynchronizedWithVirtuals);
        }
        // Every refusal that can follow the materialisation is asked FIRST.
        // Materialising allocates, and an allocation can run a moving GC; the
        // caller took `rframe` out of the stash, so its `Object(addr)` words are
        // rooted by nothing during that window, and a sink that restashes it on
        // a refusal (`try_resume_trapped_callee`) would hand the next consumer
        // from-space addresses. See r11-tier-deopt-resume-gc-windows-FIXED-20260924.md.
        if let Some(why) = refusal_before_materialisation(rframe) {
            note_deopt_frame_bail(why, rframe);
            return Err(why);
        }
        let mut copy = rframe.clone();
        // A sink that will throw on a heap failure needs the compiled code's
        // held monitors at their post-collection addresses to release them, so
        // they are pinned across the materialiser. On success these pins sit
        // below the shells' and go with the caller's watermark truncation.
        let answer_heap_failure = release_held_on_heap_failure;
        let held_base = if answer_heap_failure {
            pin_compiled_held_monitors(thread, rframe)
        } else {
            thread.native_pin_roots.len()
        };
        if let Err(failed) = crate::runtime::deopt_materialize::materialize_virtual_objects(
            shared, thread, &mut copy, /* stress_gc */ false, /* keep_pins */ true,
        ) {
            // Round 12 wave 5 (lane replay3): a full heap is named apart from a
            // malformed graph; see `DeoptFrameBail::VirtualMaterialiseHeap`.
            let why = if materialisation_failed_for_heap(&failed) {
                DeoptFrameBail::VirtualMaterialiseHeap
            } else {
                DeoptFrameBail::VirtualMaterialise
            };
            if answer_heap_failure && why == DeoptFrameBail::VirtualMaterialiseHeap {
                release_compiled_held_monitors(shared, thread, rframe, held_base);
            }
            thread.native_pin_roots.truncate(held_base);
            note_deopt_frame_bail(why, rframe);
            return Err(why);
        }
        materialized_frame = copy;
        &materialized_frame
    } else {
        rframe
    };
    // Round 13 wave 9 (lane chain4): the method monitor a self-locking body
    // handed over with this frame, which the frame owns as its
    // `monitor_on_exit` (seeded below), never as a block monitor.
    let handed_monitor = handed_method_monitor_at(
        shared,
        cached.declaring_class_id,
        cached.is_synchronized,
        cached.is_static,
        &cached.code,
        rframe,
    );

    // deopt-osr P2 — map via the cat-2-aware mappers (both route through
    // `fv_to_value`, so Int/Long/Float/Double/Object/Undefined all map): LOCALS
    // collapse the JVM-two-slot snapshot (a `long`/`double` reserves its upper
    // half, skipped) into the compact arg list `Frame::new_pooled` re-expands; the
    // operand STACK is already compact (one entry per value). Any unresolvable
    // slot (`Unsupported`/virtual/unresolved machine form) returns `None` → safe
    // re-run.
    //
    // `CRATONVM_DBG_DEOPTSLOT` is read once: this runs on every frame rebuild.
    static DBG_DEOPTSLOT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let dbg_deoptslot = *DBG_DEOPTSLOT
        .get_or_init(|| None::<std::ffi::OsString>.is_some());
    if dbg_deoptslot
        && (ir_deopt_locals(&rframe.locals).is_none()
            || ir_deopt_frame_values(&rframe.stack).is_none())
    {
        eprintln!(
            "[DBG_DEOPTSLOT] {} bci={} locals={:?} stack={:?}",
            rframe.method_key, rframe.bci, rframe.locals, rframe.stack
        );
    }
    let locals = match ir_deopt_locals(&rframe.locals) {
        Some(l) => l,
        None => {
            note_deopt_frame_bail(DeoptFrameBail::UnmappableLocal, rframe);
            return Err(DeoptFrameBail::UnmappableLocal);
        }
    };
    let stack_vals = match ir_deopt_frame_values(&rframe.stack) {
        Some(v) => v,
        None => {
            note_deopt_frame_bail(DeoptFrameBail::UnmappableStack, rframe);
            return Err(DeoptFrameBail::UnmappableStack);
        }
    };

    // ROOT the reconstructed oops BEFORE the GC-capable refill (locals then
    // stack — the order the re-read below relies on).
    let pin_base = thread.native_pin_roots.len();
    for v in locals.iter().chain(stack_vals.iter()) {
        if let Value::Object(Some(obj)) = v {
            thread.native_pin_roots.push(*obj);
        }
    }
    // Phase C: pin the held-monitor objects too (materialized shells, or the
    // real object a compiled lock holds), so the refill GC forwards them in
    // place and the relock below uses live addresses. `monitor_plan` keeps
    // (depth, relock) in push order; a non-Object monitor (an unresolved
    // virtual ref — shouldn't occur, the materializer rewrote them) or a null
    // one bails to safe re-run.
    let mut monitor_plan: Vec<(u32, bool)> = Vec::with_capacity(rframe.monitors.len());
    for m in &rframe.monitors {
        match &m.object {
            FrameValue::Object(addr) if *addr != 0 => {
                // SAFETY: `addr` is a non-null reference the deopt resolved from
                // the trapped frame (or materialization produced), and it is
                // pinned here before any allocation or GC can make it stale.
                let obj = unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) };
                thread.native_pin_roots.push(obj);
                monitor_plan.push((m.lock_depth, m.relock));
            }
            // Null monitor or an unresolved form — refuse rather than relock a
            // bogus object (would corrupt the monitor table). Release nothing
            // extra; the caller truncates `native_pin_roots` to its watermark.
            _ => {
                note_deopt_frame_bail(DeoptFrameBail::BadMonitor, rframe);
                return Err(DeoptFrameBail::BadMonitor);
            }
        }
    }

    // Stress hook: the ONLY sanctioned GC injection point — before refill, while
    // every reconstructed oop is pinned (a GC after the re-read would stale the
    // unrooted `*_fwd` vecs / the un-pushed frame; there is none, by construction).
    if stress {
        maybe_gc_forced_pub_at(shared, thread, "deopt-resume");
    }

    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        cached.max_locals as usize,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        (cached.max_stack as usize).max(16) + 8,
    );

    // Re-read each oop from its (possibly forwarded) pin slot, in push order, so
    // the frame carries the current address, never the stale `Object(u64)`.
    let mut k = pin_base;
    let mut locals_fwd = Vec::with_capacity(locals.len());
    for v in &locals {
        match v {
            Value::Object(Some(_)) => {
                let fwd = thread.native_pin_roots[k];
                k += 1;
                locals_fwd.push(Value::Object(Some(fwd)));
            }
            other => locals_fwd.push(*other),
        }
    }
    let mut stack_fwd = Vec::with_capacity(stack_vals.len());
    for v in &stack_vals {
        match v {
            Value::Object(Some(_)) => {
                let fwd = thread.native_pin_roots[k];
                k += 1;
                stack_fwd.push(Value::Object(Some(fwd)));
            }
            other => stack_fwd.push(*other),
        }
    }
    // Phase C: re-read the forwarded monitor objects (pushed after locals+stack),
    // pairing each with its lock depth and relock marker for the relock below.
    let mut monitors_fwd: Vec<(ObjectRef, u32, bool)> = Vec::with_capacity(monitor_plan.len());
    for &(depth, relock) in &monitor_plan {
        let fwd = thread.native_pin_roots[k];
        k += 1;
        monitors_fwd.push((fwd, depth, relock));
    }

    // Build the Frame as a LOCAL (never pushed onto thread.frames), so the
    // subsequent re-run sees identical interpreter state after it is discarded.
    let mut frame = crate::runtime::frame::Frame::new_pooled(
        cached.declaring_class_id,
        cached.class_name.clone(),
        cached.method_name.clone(),
        cached.method_descriptor.clone(),
        cached.source_file.clone(),
        cached.code.clone(),
        cached.exception_table.clone(),
        cached.max_stack,
        cached.max_locals,
        &locals_fwd,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    frame.note_method_is_static(cached.is_static);
    for v in &stack_fwd {
        // A clean pilot never overflows the padded stack. If it somehow did,
        // recycle the frame's pooled buffers before bailing — `Frame` has no
        // `Drop` impl (recycling is explicit), so a plain `?`-return would leak
        // them. The caller then releases the pins and re-runs.
        if frame.stack.push(*v).is_err() {
            frame.recycle(&mut thread.locals_pool, &mut thread.stacks_pool);
            note_deopt_frame_bail(DeoptFrameBail::StackPush, rframe);
            return Err(DeoptFrameBail::StackPush);
        }
    }
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    frame.pc = rframe.bci as usize;
    // Phase C: re-acquire each ELIDED monitor (`relock`) on its object,
    // `lock_depth` times, so the resumed frame's eventual `monitorexit` (and any
    // nested exits) balance via the MonitorTable. Done after every fallible step
    // so a bail never leaves a stray lock. `monitors.enter` is the recursive
    // uncontended enter: an elided lock's object never escaped this thread, so
    // there is no contention and no GC.
    //
    // A monitor with `relock == false` is one the compiled code acquired through
    // the monitor helper. This thread still holds it in the monitor table, so
    // the resumed frame already holds it and entering again would leave it one
    // level too deep after the frame's own `monitorexit`.
    for &(obj, depth, relock) in &monitors_fwd {
        if !relock {
            continue;
        }
        for _ in 0..depth {
            shared.threads.monitors.enter(obj, thread.thread_id);
        }
        // Publish the relocked monitor as every other acquisition does, so
        // `getLockedMonitors()` sees it while the resumed frame holds it. One
        // publish per object: the book dedupes, and the frame's outermost
        // `monitorexit` retracts it (r11w5-sync-deopt-relock-publish-patch).
        if depth > 0 {
            shared
                .threads
                .thread_registry
                .complete_jmx_monitor_enter(thread.thread_id, obj);
        }
    }
    // Seed the frame's record of its block monitors (`Frame::held_monitors`,
    // JVMS §2.11.10; interpreter round i1 wave 24, lane L2): every lock the
    // compiled frame held, the ones its code took and the elided levels just
    // re-taken, `lock_depth` copies each, outermost first. HotSpot's unpacked
    // interpreter frame likewise gets a `BasicObjectLock` for every monitor the
    // compiled frame's debug info names (`vframeArrayElement::fill_in`).
    //
    // Without it the frame's own `monitorexit`s still balance — they fall to
    // `held_monitors::monitorexit_unrecorded`, which releases an acquisition
    // no record accounts for — but the rules that read the record would not
    // see these locks: a return or an unwind out of the frame while one is
    // still held (hand-written bytecode; javac releases first) would keep it
    // silently instead of throwing `IllegalMonitorStateException`, and an
    // interpreted callee's `monitorexit` of this frame's lock would release it
    // instead of throwing. An entry the monitor table does not back is dropped
    // by `held_monitors::prune_stale` before any report, so a description that
    // over-states a hold degrades to the old behaviour.
    //
    // GC: the entries are the forwarded addresses read back from the pins
    // above, and nothing allocates between here and the caller's push (the
    // same window the frame's locals rely on); once pushed, the record is a
    // frame root like `monitor_on_exit`.
    //
    // Round 13 wave 9 (lane chain4): except the METHOD monitor a self-locking
    // body handed over (`handed_monitor`): the frame owns it as its
    // `monitor_on_exit`, the implicit release of an `ACC_SYNCHRONIZED` return
    // or unwind (`pop_and_recycle_frame_with_reason`). Recorded as a block
    // monitor, the frame's return would throw `IllegalMonitorStateException`.
    // A door that holds a hold of its own for the method releases that one
    // (`transfer_to_resumed_frame`), so the thread ends the call holding what
    // it held before it.
    for (i, &(obj, depth, _)) in monitors_fwd.iter().enumerate() {
        if handed_monitor == Some(i) {
            frame.monitor_on_exit = Some(obj);
            continue;
        }
        for _ in 0..depth {
            frame.held_monitors.push(obj);
        }
    }
    // Interpreter round i6 wave 118, lane L6 (page
    // `i116-L9-strict-monitorexit-throws-on-an-understated-record`, the L9
    // wave-117 residual): a compiled frame that names NO lock may be a
    // single-pass body's at a pc where its monitor analysis has no answer,
    // so a method with monitor bytecode gets an understated record, and the
    // strict `monitorexit` arm stays lenient for this thread while the frame
    // lives. A frame with no monitor bytecode is left unmarked.
    // Wave 121 (lane L6): the producer in the mark, trusted or not.
    if monitors_fwd.is_empty() {
        crate::runtime::interpreter::held_monitors::clear_untrusted_record(
            &mut frame,
            crate::runtime::interpreter::held_monitors::RecordProducer::DeoptBuild,
        );
    } else {
        crate::runtime::interpreter::held_monitors::note_trusted_record(
            &mut frame,
            crate::runtime::interpreter::held_monitors::RecordProducer::DeoptBuild,
        );
    }
    // Interpreter round i6 wave 123, lane L2 (stage 3b of i43-L1,
    // `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`): a debugger's `SetValues` to
    // the compiled activation this exit rebuilds is stored into the frame
    // before it runs a bytecode. One load while no such write waits; nothing
    // allocates, so the caller's no-collection window up to the push holds.
    #[cfg(feature = "experimental-debug")]
    {
        if shared.debug.debugger_gates.compiled_home_writes_pending() {
            crate::debug::inspect::apply_compiled_home_writes(shared, thread, &mut frame);
        }
    }
    Ok(frame)
}

/// The refusals [`build_deopt_frame_inner`] makes AFTER materialising virtual
/// objects, asked of the un-materialised frame: a `VirtualObject` /
/// `VirtualObjectRef` counts as a mappable, non-null reference (the
/// materialiser turns each into an `Object` shell). `None` means none of them
/// can fire once materialisation succeeds, so no refusal follows an allocation.
/// (The operand-stack fit is asked separately, before either path.)
pub(in crate::runtime::interpreter::deopt_resume) fn refusal_before_materialisation(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<DeoptFrameBail> {
    use cratonvm_jit::deopt::FrameValue;
    fn mappable(v: &FrameValue) -> bool {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        ) || fv_to_value(v).is_some()
    }
    // The same walk as `ir_deopt_locals`: a cat-2 value's upper half is skipped.
    let mut i = 0;
    while i < rframe.locals.len() {
        let v = &rframe.locals[i];
        if !mappable(v) {
            return Some(DeoptFrameBail::UnmappableLocal);
        }
        // JIT round 21 wave 2 (lane keepset): `ir_deopt_locals`' TOP rule.
        if cat2_pair_split_in_locals(&rframe.locals, i) {
            i += 1;
            continue;
        }
        i += if matches!(v, FrameValue::Long(_) | FrameValue::Double(_)) {
            2
        } else {
            1
        };
    }
    if !rframe.stack.iter().all(mappable) {
        return Some(DeoptFrameBail::UnmappableStack);
    }
    let monitors_ok = rframe.monitors.iter().all(|m| match &m.object {
        FrameValue::Object(addr) => *addr != 0,
        FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_) => true,
        _ => false,
    });
    if !monitors_ok {
        return Some(DeoptFrameBail::BadMonitor);
    }
    None
}

/// The locks a stashed compiled frame's code TOOK (`MonitorInfo::relock ==
/// false`), pinned from the moment a sink takes the stash until the sink knows
/// whether the frame resumes (interpreter round i1 wave 23, lane L2;
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// An optimizing body inside a `synchronized` block holds the block's monitor
/// through `jit_monitor_enter`, and a frame it stashes there (a guard trap, a
/// back-edge mode exit) describes that hold with `relock == false`. When the
/// frame RESUMES, the rebuilt interpreter frame inherits the hold and its own
/// `monitorexit` releases it (`build_deopt_frame_inner`). When a sink REFUSES
/// the frame and re-runs the method from entry, nothing else ever releases it:
/// the re-run enters the monitor again and leaves it once, so the thread owns
/// the lock for good. HotSpot never refuses a deoptimization; its interpreter
/// frame owns exactly what the compiled frame held, so the hold is released
/// once whichever way the activation ends. A re-run from entry must start from
/// the state it assumes — no lock held by the abandoned activation — so every
/// refusal that re-runs releases what the frame took, innermost first.
///
/// The objects are pinned (`native_pin_roots`) because the sinks' refusal
/// arms can run after a collection: the stash rooted the frame's `Object`
/// words, and once taken out of it nothing does. A moving collector forwards
/// the pins in place, so [`Self::release_for_a_rerun`] releases the live
/// address. Elided locks (`relock == true`) were never taken and are left
/// alone; a frame with no compiled lock pins nothing and costs nothing.
///
/// Every scope of an inlined caller chain is walked (outermost first, so the
/// release, which runs backwards, is innermost first): the re-run abandons
/// all of them. A frame the sink RESUMES keeps its locks
/// ([`Self::unpin_keeping_the_locks`]); a frame put back into the stash for an
/// outer sink keeps them too, since that sink decides.
///
/// # The exceptional channel (wave 24, lane L2)
///
/// A reason-9 frame (`LAST_EXCEPTIONAL`) names the same holds, and its sinks
/// make the same decision one way or the other: a sink that builds the
/// handler frame hands the trapping scope's locks to it
/// ([`Self::seed_frame_and_unpin`] / [`Self::seed_pushed_frame_and_unpin`] —
/// javac's catch-all handler then releases them through the frame's own
/// record), and a sink that propagates the exception past the activation
/// without its handler releases them ([`Self::release_for_a_propagation`]),
/// which is what that handler would have done before rethrowing the same
/// exception.
#[must_use = "a pinned hold must be released or unpinned, or its pins leak"]
pub(crate) struct CompiledLocksOfAStash {
    /// `native_pin_roots` watermark below the first pin.
    pub(in crate::runtime::interpreter::deopt_resume) pin_base: usize,
    /// How many times each pinned object was entered, in pin order.
    pub(in crate::runtime::interpreter::deopt_resume) depths: Vec<u32>,
    /// Index in `depths` (and the pins) of the TRAPPING scope's first lock;
    /// the ones before it belong to inlined caller scopes, whose locks a
    /// frame rebuilt for the trapping scope alone must not record.
    pub(in crate::runtime::interpreter::deopt_resume) own_from: usize,
    /// Index in `depths` (and the pins) where the TAKEN locks end: the pins
    /// from here on are the trapping scope's elided levels (`relock`), which
    /// the compiled code never took. No release or seeding reads them; only
    /// [`Self::replace_live_record_with_retaken_levels_and_unpin`] does, for
    /// the one transfer that re-takes them (interpreter round i1 wave 27,
    /// lane L2).
    pub(in crate::runtime::interpreter::deopt_resume) retake_from: usize,
    /// Index in `depths` (and the pins) where the OUTERMOST scope's taken
    /// locks end (they start at 0: that scope is walked first). The live
    /// frame an OSR-exit transfer rewrites is that scope's frame, so its
    /// record is these ([`Self::live_frame_locks`]). Equal to `retake_from`
    /// for a single-scope frame, where the outermost scope IS the trapping
    /// one. Round 13 wave 8 (lane sync5; part (c) of
    /// `r13w6-chain2-osr-guard-exit-chain-lock-record-patch-FIXED-20260928.md`).
    pub(in crate::runtime::interpreter::deopt_resume) outer_to: usize,
    /// Index in `depths` (and the pins) of the METHOD monitor a self-locking
    /// body handed over with this frame ([`handed_method_monitor_at`]), set
    /// only by [`Self::pin_compiled_locks_of_method`]. A frame that resumes
    /// owns it as its `monitor_on_exit`, never as a block monitor; every
    /// release treats it as any taken lock. JIT round 17 wave 2, lane
    /// syncdoor2 (SD17-1): the precise exceptional frames of a self-locking
    /// body with an exception table carry it
    /// (`CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER`).
    pub(in crate::runtime::interpreter::deopt_resume) method_monitor: Option<usize>,
}

impl CompiledLocksOfAStash {
    /// [`Self::pin_compiled_locks`] for a frame of the method with these
    /// facts, naming the handed-over METHOD monitor when the frame carries
    /// one ([`handed_method_monitor_at`]): [`Self::seed_frame_and_unpin`] /
    /// [`Self::seed_pushed_frame_and_unpin`] then make it the frame's
    /// `monitor_on_exit` (or leave it to the sink that sets it), and
    /// [`Self::hands_over_method_monitor`] tells the handler sinks not to
    /// acquire the method monitor again. JIT round 17 wave 2, lane syncdoor2
    /// (SD17-1). `code` is the method's bytecode, padded or not.
    ///
    /// Inert for every frame that names no such hold: a wrapped body's frame
    /// names no method monitor, a method with monitor bytecode never answers,
    /// and before this wave no exceptional frame of a method WITH a handler
    /// named one.
    pub(crate) fn pin_compiled_locks_of_method(
        shared: &SharedVm,
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
        declaring_class_id: crate::classloading::ClassId,
        is_synchronized: bool,
        is_static: bool,
        code: &[u8],
    ) -> Self {
        // JIT round 22 wave 8, lane sync: link 2 of the self-lock census's
        // hand-over chain -- a frame naming a compiled hold reached a sink.
        if Self::frame_names_compiled_hold(rframe) {
            Self::note_drop_census(
                shared,
                cratonvm_jit::self_lock_census::SelfLockRow::HoldFramesTaken,
                1,
            );
        }
        let mut locks = Self::pin_scopes(thread, rframe, false);
        // `handed_method_monitor_at` answers only for a single-scope frame
        // whose FIRST monitor is taken (`relock == false`), one level, on a
        // non-null object: the first pin of the trapping scope. Unmarked, it
        // is the only monitor, so the only pin; MARKED
        // (`method_monitor_first`, JIT round 18 wave 9, lane syncdoor6,
        // SD17-3) the block levels the frame names after it are the pins
        // after it, which the seeding records as `held_monitors`.
        if handed_method_monitor_at(
            shared,
            declaring_class_id,
            is_synchronized,
            is_static,
            code,
            rframe,
        ) == Some(0)
            && locks.retake_from > locks.own_from
            && (locks.retake_from == locks.own_from + 1 || rframe.semantics.method_monitor_first)
        {
            locks.method_monitor = Some(locks.own_from);
        }
        locks
    }

    /// Does this frame hand its method's monitor over
    /// ([`Self::pin_compiled_locks_of_method`])? A handler sink that resumes
    /// the frame then adopts that hold and must not acquire the monitor
    /// again; one that abandons it releases it once, as any taken lock.
    pub(crate) fn hands_over_method_monitor(&self) -> bool {
        self.method_monitor.is_some()
    }

    /// The handed-over method monitor at its current (pinned) address, for a
    /// sink that builds the handler frame itself and makes it the frame's
    /// `monitor_on_exit` before the push; `None` when the frame hands none
    /// over or the pins were truncated underneath.
    pub(crate) fn handed_method_monitor(&self, thread: &JvmThread) -> Option<ObjectRef> {
        let i = self.method_monitor?;
        if thread.native_pin_roots.len() < self.pin_base + self.depths.len() {
            return None;
        }
        thread.native_pin_roots.get(self.pin_base + i).copied()
    }

    /// Pin the locks `rframe`'s compiled code took. Call right after the stash
    /// is taken, before anything that can allocate.
    pub(crate) fn pin_compiled_locks(
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ) -> Self {
        Self::pin_scopes(thread, rframe, false)
    }

    /// [`Self::pin_compiled_locks`], plus the trapping scope's elided levels
    /// (the OUTERMOST scope's for an inlined chain: the scope the live frame
    /// becomes; round 13 wave 8, lane sync5)
    /// (`relock`) after every taken lock, for the OSR door, whose planless
    /// transfer re-takes them and records them
    /// ([`Self::replace_live_record_with_retaken_levels_and_unpin`]). Nothing
    /// releases or seeds an elided level: every other method reads the taken
    /// pins only. Interpreter round i1 wave 27, lane L2.
    pub(crate) fn pin_compiled_locks_and_elided_levels(
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ) -> Self {
        Self::pin_scopes(thread, rframe, true)
    }

    pub(in crate::runtime::interpreter::deopt_resume) fn pin_scopes(
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
        elided_levels: bool,
    ) -> Self {
        use cratonvm_jit::deopt::FrameValue;
        let pin_base = thread.native_pin_roots.len();
        let mut depths = Vec::new();
        let mut own_from = 0;
        let mut outer_to = 0;
        // The scope whose frame an OSR-exit transfer writes into the live
        // frame (the first one walked below).
        let outermost = stash_identity_scope(rframe);
        let scopes = rframe
            .caller_frames
            .iter()
            .rev()
            .chain(std::iter::once(rframe));
        for scope in scopes {
            // The trapping scope is the last one walked.
            if std::ptr::eq(scope, rframe) {
                own_from = depths.len();
            }
            for m in &scope.monitors {
                if m.relock || m.lock_depth == 0 {
                    continue;
                }
                if let FrameValue::Object(addr) = &m.object {
                    let addr = *addr;
                    if addr != 0 {
                        // SAFETY: a non-null reference the stash resolved from
                        // the trapped frame; the stash rooted it until the
                        // take, and nothing has run since.
                        let obj = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
                        thread.native_pin_roots.push(obj);
                        depths.push(m.lock_depth);
                    }
                }
            }
            if std::ptr::eq(scope, outermost) {
                outer_to = depths.len();
            }
        }
        // The elided levels of the scope the live frame becomes, after every
        // taken lock, when asked: kept current for the record of a transfer
        // that re-takes them, and read by nothing else (wave 27, lane L2).
        // That is the trapping scope for a single-scope frame, and the
        // OUTERMOST one for an inlined chain, whose inner scopes become pushed
        // frames (round 13 wave 8, lane sync5).
        let retake_from = depths.len();
        let elided: &[cratonvm_jit::deopt::MonitorInfo] = if elided_levels {
            &outermost.monitors
        } else {
            &[]
        };
        for m in elided {
            if !m.relock || m.lock_depth == 0 {
                continue;
            }
            if let FrameValue::Object(addr) = &m.object {
                let addr = *addr;
                if addr != 0 {
                    // SAFETY: a non-null reference the stash resolved from
                    // the trapped frame, rooted by the stash until the take;
                    // nothing has run since.
                    let obj = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
                    thread.native_pin_roots.push(obj);
                    depths.push(m.lock_depth);
                }
            }
        }
        Self {
            pin_base,
            depths,
            own_from,
            retake_from,
            outer_to,
            method_monitor: None,
        }
    }

    /// The frame resumed, or went back into the stash: its locks stay held
    /// (by the resumed frame, or for the next sink). Drop the pins.
    pub(crate) fn unpin_keeping_the_locks(self, thread: &mut JvmThread) {
        if !self.depths.is_empty() {
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }

    /// The trapping scope's pinned locks, `(object at its current address,
    /// lock_depth)`, outermost first; empty when the pins were truncated
    /// underneath (every sink below this one truncates to its own, higher
    /// watermark, so that is a caller defect, reported under
    /// `CRATONVM_DBG_DEOPT`).
    pub(in crate::runtime::interpreter::deopt_resume) fn own_locks(&self, thread: &JvmThread) -> Vec<(ObjectRef, u32)> {
        let n = self.depths.len();
        if thread.native_pin_roots.len() < self.pin_base + n {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] a resumed frame's lock pins were truncated underneath; \
                     its record names none of its {n} compiled lock(s)"
                );
            }
            return Vec::new();
        }
        (self.own_from..self.retake_from)
            .map(|i| (thread.native_pin_roots[self.pin_base + i], self.depths[i]))
            .collect()
    }

    /// The OUTERMOST scope's pinned locks, `(object at its current address,
    /// lock_depth)`, outermost first: the record of the live frame an OSR-exit
    /// transfer rewrites, which is that scope's frame. For a single-scope
    /// frame exactly [`Self::own_locks`]; for an inlined chain the trapping
    /// scope's locks belong to the frames the chain transfer PUSHES, not to
    /// the live one (round 13 wave 8, lane sync5; part (c) of
    /// `r13w6-chain2-osr-guard-exit-chain-lock-record-patch-FIXED-20260928.md`).
    /// Empty when the pins were truncated underneath, as [`Self::own_locks`].
    pub(in crate::runtime::interpreter::deopt_resume) fn live_frame_locks(&self, thread: &JvmThread) -> Vec<(ObjectRef, u32)> {
        if self.outer_to == self.retake_from && self.own_from == 0 {
            return self.own_locks(thread);
        }
        let n = self.depths.len();
        if thread.native_pin_roots.len() < self.pin_base + n {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] a transferred chain's lock pins were truncated underneath; \
                     the live record names none of its {n} compiled lock(s)"
                );
            }
            return Vec::new();
        }
        (0..self.outer_to.min(self.retake_from))
            .map(|i| (thread.native_pin_roots[self.pin_base + i], self.depths[i]))
            .collect()
    }

    /// A frame not yet pushed resumes the trapping scope (a compiled callee's
    /// handler frame): record the locks its compiled code took in the frame's
    /// `held_monitors`, `lock_depth` copies each, outermost first — the seeding
    /// [`build_deopt_frame_inner`] does for a guard trap's frame — then drop
    /// the pins. Nothing is entered or released. Interpreter round i1 wave 24,
    /// lane L2.
    ///
    /// The frame must be pushed before anything that can collect: from here the
    /// record is the frame's own reference to each object, like its locals.
    ///
    /// JIT round 17 wave 2, lane syncdoor2 (SD17-1): a handed-over METHOD
    /// monitor ([`Self::pin_compiled_locks_of_method`]) becomes the frame's
    /// `monitor_on_exit` instead, as `build_deopt_frame_inner` seeds a guard
    /// frame's; the sink acquired nothing of its own for it.
    pub(crate) fn seed_frame_and_unpin(self, thread: &mut JvmThread, frame: &mut Frame) {
        if self.depths.is_empty() {
            // Interpreter round i6 wave 119, lane L6 (page
            // `i116-L9-strict-monitorexit-throws-on-an-understated-record`):
            // a frame the compiled scope names no lock for may be a
            // single-pass body's with no monitor answer; its handler's
            // `monitorexit` must keep the strict arm's leniency.
            crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                frame,
                crate::runtime::interpreter::held_monitors::RecordProducer::HandlerSeed,
            );
            return;
        }
        // Interpreter round i6 wave 121, lane L6: pins truncated underneath
        // make `own_locks` answer nothing (its debug line says so) while the
        // frame holds every lock the scope named -- an understated record.
        let truncated = self.pins_truncated(thread);
        let method_monitor = self.own_method_monitor();
        for (j, (obj, depth)) in self.own_locks(thread).into_iter().enumerate() {
            if method_monitor == Some(j) {
                frame.monitor_on_exit = Some(obj);
                continue;
            }
            for _ in 0..depth {
                frame.held_monitors.push(obj);
            }
        }
        Self::note_seeded_record(
            frame,
            truncated,
            crate::runtime::interpreter::held_monitors::RecordProducer::HandlerSeed,
        );
        thread.native_pin_roots.truncate(self.pin_base);
    }

    /// Wave 121 (lane L6): were this stash's pins truncated underneath (so
    /// [`Self::own_locks`] answers nothing)?
    pub(in crate::runtime::interpreter::deopt_resume) fn pins_truncated(&self, thread: &JvmThread) -> bool {
        thread.native_pin_roots.len() < self.pin_base + self.depths.len()
    }

    /// Wave 121 (lane L6): name `producer` in a seeded record's mark --
    /// understated when the pins were truncated, as provenance otherwise.
    pub(in crate::runtime::interpreter::deopt_resume) fn note_seeded_record(
        frame: &mut Frame,
        truncated: bool,
        producer: crate::runtime::interpreter::held_monitors::RecordProducer,
    ) {
        if truncated {
            frame.held_monitors.mark_understated_by(producer);
        } else {
            crate::runtime::interpreter::held_monitors::note_trusted_record(frame, producer);
        }
    }

    /// The handed-over method monitor's index in [`Self::own_locks`]' list.
    pub(in crate::runtime::interpreter::deopt_resume) fn own_method_monitor(&self) -> Option<usize> {
        self.method_monitor
            .and_then(|i| i.checked_sub(self.own_from))
    }

    /// [`Self::seed_frame_and_unpin`] for a frame the sink has already pushed,
    /// at `thread.frames[frame_idx]` (the handler frame
    /// `route_jit_exception_through_method` pushes). The pins kept the objects
    /// current across whatever ran since the push.
    ///
    /// JIT round 17 wave 2, lane syncdoor2 (SD17-1): a handed-over METHOD
    /// monitor is NOT recorded: the sink made it the frame's
    /// `monitor_on_exit` before the push (`route_jit_exception_through_method`,
    /// [`Self::handed_method_monitor`]).
    pub(crate) fn seed_pushed_frame_and_unpin(self, thread: &mut JvmThread, frame_idx: usize) {
        if self.depths.is_empty() {
            // Wave 119 (lane L6): as `seed_frame_and_unpin`.
            if let Some(frame) = thread.frames.get_mut(frame_idx) {
                crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                    frame,
                    crate::runtime::interpreter::held_monitors::RecordProducer::PushedHandlerSeed,
                );
            }
            return;
        }
        let truncated = self.pins_truncated(thread);
        let method_monitor = self.own_method_monitor();
        let locks = self.own_locks(thread);
        if let Some(frame) = thread.frames.get_mut(frame_idx) {
            for (j, (obj, depth)) in locks.into_iter().enumerate() {
                if method_monitor == Some(j) {
                    continue;
                }
                for _ in 0..depth {
                    frame.held_monitors.push(obj);
                }
            }
            Self::note_seeded_record(
                frame,
                truncated,
                crate::runtime::interpreter::held_monitors::RecordProducer::PushedHandlerSeed,
            );
        }
        thread.native_pin_roots.truncate(self.pin_base);
    }

    /// The exception leaves the activation without its handler having run (a
    /// reason-9 sink that propagates: no handler covers the throw, a frame it
    /// could not map or materialise, a monitor or frame it could not build).
    /// The activation never resumes, so what its compiled code locked is
    /// released exactly as for a re-run ([`Self::release_for_a_rerun`]) —
    /// javac's catch-all would have released it and rethrown the same
    /// exception. Returns how many single releases succeeded.
    pub(crate) fn release_for_a_propagation(
        self,
        shared: &SharedVm,
        thread: &mut JvmThread,
    ) -> u32 {
        self.release_pinned(shared, thread, None)
    }

    /// [`Self::release_for_a_propagation`] for an OSR'd activation, whose
    /// frame is the LIVE interpreter frame at `frame_idx` and not a stash-only
    /// description: that frame's `held_monitors` record can still name a lock
    /// the interpreter took before the OSR entry, which the compiled frame
    /// describes too (`relock == false`). Each release retires one matching
    /// record entry, so the unwind that follows cannot release the same hold
    /// again (`held_monitors::replace_unwound_exception_if_locked`).
    pub(crate) fn release_for_a_propagation_of_live_frame(
        self,
        shared: &SharedVm,
        thread: &mut JvmThread,
        frame_idx: usize,
    ) -> u32 {
        self.release_pinned(shared, thread, Some(frame_idx))
    }

    /// JIT round 17 wave 3, lane syncdoor3
    /// (`r17-syncdoor2-dropped-exceptional-frames-keep-a-handed-method-monitor-FIXED-20261005.md`):
    /// does a sink that DROPS an exceptional frame (one it has just taken out
    /// of the stash and will not resume) release the holds the frame names?
    /// Exactly when the JIT may hand a self-locking body's method monitor over
    /// with such a frame: `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS=1` and
    /// `CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER` not `0` (`cratonvm_jit`
    /// `self_lock_exc_handover_enabled`). Off, every drop site keeps its old
    /// behaviour byte for byte (a dropped frame naming a compiled block lock
    /// leaks as before; `CRATONVM_DBG_DEOPT` names it at the next take).
    ///
    /// Read only on a drop path (an abandoned compiled attempt, a compiled
    /// catch with frames above its floor), never per call; not memoised, so a
    /// per-thread override reaches it.
    pub(crate) fn dropped_frame_holds_released() -> bool {
        // JIT round 18 wave 9 (lane syncdoor6): the JIT's own reader of the
        // handlers switch, so a default flip cannot leave the drop sites
        // unreleasing while the JIT hands holds over. JIT round 19 (lane
        // selflock19, SYNCDOOR618-4): and of the hand-over switch too, through
        // the one JIT function that reads both (the same two flags as before).
        cratonvm_jit::self_lock_exc_handover_switches_on()
    }

    /// JIT round 19 (lane selflock19, SDB17-2): count a drop-site event in
    /// this VM's self-lock census (`cratonvm_jit::self_lock_census`, printed
    /// on the `CRATONVM_DBG=jit-method-stats` exit dump). Reporting only.
    pub(in crate::runtime::interpreter::deopt_resume) fn note_drop_census(
        shared: &SharedVm,
        row: cratonvm_jit::self_lock_census::SelfLockRow,
        n: u64,
    ) {
        shared
            .jit
            .tiered_manager
            .verdicts()
            .self_lock_census()
            .note_n(row, n);
    }

    /// Does `rframe` name a hold its compiled code took (`relock == false`)
    /// in any scope? The frames the SDB17-2 leak row counts.
    pub(in crate::runtime::interpreter::deopt_resume) fn frame_names_compiled_hold(rframe: &cratonvm_jit::deopt::ReconstructedFrame) -> bool {
        std::iter::once(&rframe.monitors)
            .chain(rframe.caller_frames.iter().map(|scope| &scope.monitors))
            .flat_map(|monitors| monitors.iter())
            .any(|m| !m.relock)
    }

    /// Release what `rframe`, an exceptional frame a sink has TAKEN and is
    /// dropping, says its compiled code still holds: every taken lock of every
    /// scope, innermost first, as a propagation does
    /// ([`Self::release_for_a_propagation`]). The activation the frame
    /// describes has unwound and will never resume, and its stub kept those
    /// holds for the sink that takes the frame (a self-locking body's handed
    /// method monitor under the exceptional hand-over, or a block lock). A
    /// frame that names none costs nothing. Call right after the take, before
    /// anything that can allocate. `site` names the drop for
    /// `CRATONVM_DBG_DEOPT`. Returns how many single releases succeeded.
    ///
    /// The caller decides the gate ([`Self::dropped_frame_holds_released`]).
    /// JIT round 17 wave 3, lane syncdoor3.
    pub(crate) fn release_for_a_dropped_frame(
        shared: &SharedVm,
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
        site: &'static str,
    ) -> u32 {
        if rframe.monitors.is_empty()
            && rframe
                .caller_frames
                .iter()
                .all(|scope| scope.monitors.is_empty())
        {
            return 0;
        }
        let released =
            Self::pin_compiled_locks(thread, rframe).release_for_a_propagation(shared, thread);
        // JIT round 19 (lane selflock19, SDB17-2): the released rows.
        if released > 0 {
            use cratonvm_jit::self_lock_census::SelfLockRow;
            Self::note_drop_census(shared, SelfLockRow::DropReleasedFrames, 1);
            Self::note_drop_census(shared, SelfLockRow::DropReleasedHolds, u64::from(released));
        }
        if released > 0 && dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {site}: released {released} compiled lock hold(s) of a dropped \
                 exceptional frame {} bci={}",
                rframe.method_key, rframe.bci,
            );
        }
        released
    }

    /// JIT round 17 wave 4, lane stashdrop (site 1 of
    /// `r17-syncdoor3-exceptional-frames-discarded-beneath-a-take-keep-their-holds-FIXED-20261005.md`):
    /// `cratonvm_jit::deopt::take_exceptional_frame_with_point` for a VM taker
    /// that also releases what the frames the take DISCARDS from beneath its
    /// own still hold, right at that take (the one place `pop_stash_entry`'s
    /// "an unrelated later take" objection does not reach). Under
    /// [`Self::dropped_frame_holds_released`] only; off (or with at most one
    /// frame stashed, when nothing can be discarded), exactly the plain take.
    ///
    /// Why a discarded frame's holds are still held: every drop site releases
    /// under the same gate, so a frame still in the stash is one whose holds
    /// nobody has released, and its publisher has unwound. `keep` names a
    /// discarded frame that must NOT be released (the frames a live OSR
    /// activation's own body published, whose holds its live interpreter frame
    /// may still account for); it is dropped as before.
    #[allow(clippy::type_complexity)]
    pub(crate) fn take_exceptional_frame_releasing_discards(
        shared: &SharedVm,
        thread: &mut JvmThread,
        keep: &dyn Fn(&cratonvm_jit::deopt::ReconstructedFrame, usize) -> bool,
        site: &'static str,
    ) -> Option<(
        cratonvm_jit::deopt::ReconstructedFrame,
        Option<cratonvm_jit::deopt::DeoptCause>,
        usize,
    )> {
        if cratonvm_jit::deopt::exceptional_stash_depth() <= 1
            || !Self::dropped_frame_holds_released()
        {
            return cratonvm_jit::deopt::take_exceptional_frame_with_point();
        }
        let (frame, cause, point, discarded) =
            cratonvm_jit::deopt::take_exceptional_frame_with_point_and_discards()?;
        Self::release_discarded(shared, thread, discarded, keep, site);
        Some((frame, cause, point))
    }

    /// [`Self::take_exceptional_frame_releasing_discards`] for a taker that
    /// runs while COMPILED CALLERS are still on the native stack:
    /// `exception_dispatch.rs` `precise_handler_frame_for`, the claim of the
    /// call-site services (JIT round 17 wave 8, lane syncdoor5; STC17-3 of
    /// `jit-r17-stashcap-proposals-RETIRED-20261006.md`). `floor` is the exceptional-stash
    /// depth the innermost live JIT door recorded at its entry
    /// (`conservative_roots::exceptional_stash_floor`).
    ///
    /// The `keep` rule is the floor: a discarded frame stashed at index
    /// `floor` or above is released, one below it is dropped unreleased, as
    /// the plain take drops it. Why that is sound:
    ///
    /// * a compiled activation publishes an exceptional frame only on its way
    ///   OUT (the reason-9..=12 stubs end in its epilogue), so no activation
    ///   still on the native stack -- the compiled caller this service
    ///   returns to, or any compiled frame between it and the door -- can
    ///   have a frame in the stash;
    /// * every frame at or above the floor was published inside the door's
    ///   span, by an activation that has since unwound; nothing will ever
    ///   take it once this take discards it, so its holds are released here
    ///   or never (the argument of `cratonvm_jit::deopt::truncate_exceptional_stash_to`);
    /// * a frame below the floor was stashed before the door was entered: an
    ///   outer sink, or an OSR activation's live interpreter frame, may still
    ///   account for its holds, so it is not released.
    ///
    /// Off the exceptional hand-over's switches
    /// ([`Self::dropped_frame_holds_released`]), or when no discarded frame
    /// is at or above the floor, exactly the plain take.
    #[allow(clippy::type_complexity)]
    pub(crate) fn take_exceptional_frame_releasing_discards_above(
        shared: &SharedVm,
        thread: &mut JvmThread,
        floor: usize,
        site: &'static str,
    ) -> Option<(
        cratonvm_jit::deopt::ReconstructedFrame,
        Option<cratonvm_jit::deopt::DeoptCause>,
        usize,
    )> {
        // The discards sit at indices `0..depth - 1`; one at `floor` or above
        // exists only when `depth > floor + 1`.
        if cratonvm_jit::deopt::exceptional_stash_depth() <= floor.saturating_add(1)
            || !Self::dropped_frame_holds_released()
        {
            return cratonvm_jit::deopt::take_exceptional_frame_with_point();
        }
        let (frame, cause, point, discarded) =
            cratonvm_jit::deopt::take_exceptional_frame_with_point_and_discards()?;
        // Outermost first, as stashed: index `i` is the frame's stash index.
        // Released innermost first, as `release_discarded` does.
        for (index, (stale, _point)) in discarded.into_iter().enumerate().rev() {
            if index < floor {
                // JIT round 19 (lane selflock19, SDB17-2): the leak row.
                if Self::frame_names_compiled_hold(&stale) {
                    Self::note_drop_census(
                        shared,
                        cratonvm_jit::self_lock_census::SelfLockRow::DropUnreleasedFrames,
                        1,
                    );
                }
                if dbg_deopt_enabled() && !stale.monitors.is_empty() {
                    eprintln!(
                        "[cratonvm-deopt] {site}: dropped a stashed frame {} bci={} below the \
                         entry floor {floor} without a release",
                        stale.method_key, stale.bci,
                    );
                }
                continue;
            }
            let _ = Self::release_for_a_dropped_frame(shared, thread, &stale, site);
        }
        Some((frame, cause, point))
    }

    /// Release the frames a take discarded
    /// ([`Self::take_exceptional_frame_releasing_discards`]), innermost first,
    /// except the ones `keep` names. Nothing here can allocate, so a frame the
    /// caller took and has not pinned yet stays valid across it.
    pub(in crate::runtime::interpreter::deopt_resume) fn release_discarded(
        shared: &SharedVm,
        thread: &mut JvmThread,
        discarded: Vec<cratonvm_jit::deopt::DiscardedExceptionalFrame>,
        keep: &dyn Fn(&cratonvm_jit::deopt::ReconstructedFrame, usize) -> bool,
        site: &'static str,
    ) {
        for (frame, point) in discarded.into_iter().rev() {
            if keep(&frame, point) {
                Self::note_dropped_unreleased(shared, &frame, site);
                continue;
            }
            let _ = Self::release_for_a_dropped_frame(shared, thread, &frame, site);
        }
    }

    /// `CRATONVM_DBG_DEOPT`: a dropped frame `keep` refused to release.
    /// JIT round 19 (lane selflock19, SDB17-2): counted in the leak row when
    /// it names a compiled hold.
    pub(in crate::runtime::interpreter::deopt_resume) fn note_dropped_unreleased(
        shared: &SharedVm,
        frame: &cratonvm_jit::deopt::ReconstructedFrame,
        site: &str,
    ) {
        if Self::frame_names_compiled_hold(frame) {
            Self::note_drop_census(
                shared,
                cratonvm_jit::self_lock_census::SelfLockRow::DropUnreleasedFrames,
                1,
            );
        }
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {site}: dropped a stashed frame {} bci={} of the live \
                 activation's own body without a release",
                frame.method_key, frame.bci,
            );
        }
    }

    /// `cratonvm_jit::deopt::truncate_exceptional_stash_to(floor)` for a sink
    /// that releases what the truncated frames hold: under
    /// [`Self::dropped_frame_holds_released`] they are TAKEN one at a time from
    /// the top (`take_exceptional_frame_above_if`) and each one's holds are
    /// released ([`Self::release_for_a_dropped_frame`]) unless `keep` names it,
    /// as `helpers.rs` `drop_orphaned_exceptional_frames_at_compiled_catch`
    /// does since wave 3. Returns how many frames left the stash. Off, exactly
    /// the truncation; a stash at or below the floor reads no switch. JIT round
    /// 17 wave 4, lane stashdrop (sites 2 and 3 of the page above: the OSR
    /// sinks of `jit_bridge.rs`).
    pub(crate) fn truncate_exceptional_stash_releasing(
        shared: &SharedVm,
        thread: &mut JvmThread,
        floor: usize,
        keep: &dyn Fn(&cratonvm_jit::deopt::ReconstructedFrame, usize) -> bool,
        site: &'static str,
    ) -> usize {
        if cratonvm_jit::deopt::exceptional_stash_depth() <= floor {
            return 0;
        }
        let mut taken = 0usize;
        if Self::dropped_frame_holds_released() {
            while let Some((frame, _cause, point)) =
                cratonvm_jit::deopt::take_exceptional_frame_above_if(floor, |_, _| true)
            {
                taken += 1;
                if keep(&frame, point) {
                    Self::note_dropped_unreleased(shared, &frame, site);
                    continue;
                }
                // Right after the take, before anything that can allocate.
                let _ = Self::release_for_a_dropped_frame(shared, thread, &frame, site);
            }
        }
        taken + cratonvm_jit::deopt::truncate_exceptional_stash_to(floor)
    }

    /// `exception_dispatch::drop_own_exceptional_frame` (drop the top frame
    /// when it names this method, put a callee's back) for a sink that
    /// releases what the dropped frame names
    /// ([`Self::release_for_a_dropped_frame`]) and what the take discards
    /// from beneath it ([`Self::take_exceptional_frame_releasing_discards`]),
    /// except the frames `keep` names. Under
    /// [`Self::dropped_frame_holds_released`] only; off, exactly the old
    /// drop. JIT round 17 wave 4, lane stashdrop (site 4 of the page above:
    /// the sentinel arms of `run_jit_body_raw` and `try_osr`, and the
    /// first-call door's frame-push sink in `interpreter.rs`).
    pub(crate) fn drop_own_exceptional_frame_releasing(
        shared: &SharedVm,
        thread: &mut JvmThread,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        keep: &dyn Fn(&cratonvm_jit::deopt::ReconstructedFrame, usize) -> bool,
        site: &'static str,
    ) {
        // An empty stash: the old drop would take nothing either.
        if cratonvm_jit::deopt::exceptional_stash_depth() == 0 {
            return;
        }
        if !Self::dropped_frame_holds_released() {
            super::exception_dispatch::drop_own_exceptional_frame(
                class_name,
                method_name,
                descriptor,
            );
            return;
        }
        let Some((frame, cause, point, discarded)) =
            cratonvm_jit::deopt::take_exceptional_frame_with_point_and_discards()
        else {
            return;
        };
        Self::release_discarded(shared, thread, discarded, keep, site);
        if !deopt_frame_matches_method(&frame, class_name, method_name, descriptor) {
            // A callee's frame stays in flight, with its point, as before.
            cratonvm_jit::deopt::restash_exceptional_frame_with_point(frame, cause, point);
            return;
        }
        if keep(&frame, point) {
            Self::note_dropped_unreleased(shared, &frame, site);
            return;
        }
        let _ = Self::release_for_a_dropped_frame(shared, thread, &frame, site);
    }

    /// The frame is abandoned and its method re-runs from entry: release every
    /// pinned lock `lock_depth` times, innermost first, through the release a
    /// frame's `monitorexit` makes (`monitor_exit_and_retract_jmx`), then drop
    /// the pins. Returns how many single releases succeeded.
    ///
    /// A release the monitor table refuses (the thread does not own the
    /// object) stops that object's releases and is reported under
    /// `CRATONVM_DBG_DEOPT`; it can only mean the frame described a hold the
    /// thread no longer has, and releasing further could not be right.
    pub(crate) fn release_for_a_rerun(self, shared: &SharedVm, thread: &mut JvmThread) -> u32 {
        self.release_pinned(shared, thread, None)
    }

    /// The release both abandon paths share; `live_frame` names a live frame
    /// whose record entries each release retires (see
    /// [`Self::release_for_a_propagation_of_live_frame`]).
    pub(in crate::runtime::interpreter::deopt_resume) fn release_pinned(
        self,
        shared: &SharedVm,
        thread: &mut JvmThread,
        live_frame: Option<usize>,
    ) -> u32 {
        if self.depths.is_empty() {
            return 0;
        }
        let n = self.depths.len();
        let mut released = 0u32;
        // The pins must still be where they were put: every sink below this
        // one truncates to its own, higher watermark.
        if thread.native_pin_roots.len() >= self.pin_base + n {
            for i in (0..self.retake_from).rev() {
                let obj = thread.native_pin_roots[self.pin_base + i];
                for _ in 0..self.depths[i] {
                    if let Some(f) = live_frame.and_then(|idx| thread.frames.get_mut(idx)) {
                        let _ = f.held_monitors.remove_newest(obj);
                    }
                    match crate::vm::vm_exec::monitor_exit_and_retract_jmx(
                        shared,
                        obj,
                        thread.thread_id,
                    ) {
                        Ok(()) => released += 1,
                        Err(_) => {
                            if dbg_deopt_enabled() {
                                eprintln!(
                                    "[cratonvm-deopt] refused frame named a compiled lock \
                                     this thread does not hold; not released further"
                                );
                            }
                            break;
                        }
                    }
                }
            }
        } else if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] refused frame's lock pins were truncated underneath; \
                 {n} compiled lock(s) not released"
            );
        }
        thread.native_pin_roots.truncate(self.pin_base);
        if released > 0 && dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] released {released} compiled lock hold(s) of a refused frame \
                 before its re-run"
            );
        }
        released
    }

    /// An OSR'd activation's frame is abandoned but its LIVE interpreter frame
    /// at `frame_idx` carries on (the no-exception sentinel arm of `try_osr`,
    /// whose continuing arms resume the frame at its entry pc): release the
    /// holds the frame names BEYOND what that frame's `held_monitors` record
    /// already accounts for, innermost first, then drop the pins. Returns how
    /// many single releases succeeded. Interpreter round i1 wave 25, lane L2.
    ///
    /// The record is the interpreter's own account of the locks it took
    /// before the OSR entry (`try_osr_with_backoff` empties it only after a
    /// body that returned, threw out, or committed a transfer), and the entry
    /// contract lists each of them in the compiled frame too, `relock ==
    /// false`. Those stay with the frame, whose pc is still inside their
    /// blocks; the rest were taken by the compiled body after the entry and
    /// nothing else will release them. Each object's record count is spent on
    /// its outermost entries first, as the interpreter took those first.
    ///
    /// # The record may under-state (interpreter round i1 wave 28, lane L2)
    ///
    /// The record is allowed to UNDER-state (`held_monitors`' module doc): a
    /// committed single-pass OSR exit whose frame names no lock empties it
    /// while the frame, parked at the exit, may still be inside a block whose
    /// lock it holds. Trusted alone, the record then called that hold "taken
    /// by the body", and a later OSR body of the same activation that left
    /// through a reason-9 pad released the interpreter's own lock
    /// (`docs/feature-designs/real-jdk-mode.md`).
    /// So an object `entry` names — the thread owned it at the OSR entry,
    /// through one of the frame's reference locals ([`OsrEntryHolds`]) — is
    /// released at most as often as the thread's hold count of it GREW since
    /// that entry: the frame resumes at the entry pc, and the lock state it
    /// assumes there is the one the entry saw. That count comes from the
    /// monitor table, which cannot under-state. An object the snapshot does
    /// not name keeps the record rule, as before.
    pub(crate) fn release_beyond_live_record(
        self,
        shared: &SharedVm,
        thread: &mut JvmThread,
        frame_idx: usize,
        entry: &OsrEntryHolds,
    ) -> u32 {
        if self.depths.is_empty() {
            return 0;
        }
        let n = self.depths.len();
        let mut released = 0u32;
        // Holds of each snapshot object the body gained since the entry;
        // read before the first release below.
        let mut gained = entry.gains_since_entry(shared, thread, frame_idx);
        let mut kept_by_entry = 0u32;
        if thread.native_pin_roots.len() >= self.pin_base + n {
            // How many holds of each pinned entry the record accounts for.
            let mut kept: Vec<u32> = Vec::with_capacity(n);
            {
                let record: &[ObjectRef] = match thread.frames.get(frame_idx) {
                    Some(f) => f.held_monitors.as_slice(),
                    None => &[],
                };
                let mut spent: Vec<(ObjectRef, usize)> = Vec::new();
                for i in 0..self.retake_from {
                    let obj = thread.native_pin_roots[self.pin_base + i];
                    let in_record = record.iter().filter(|&&o| o == obj).count();
                    let already = spent.iter().find(|(o, _)| *o == obj).map_or(0, |(_, c)| *c);
                    let keep = in_record
                        .saturating_sub(already)
                        .min(usize::try_from(self.depths[i]).unwrap_or(usize::MAX));
                    match spent.iter_mut().find(|(o, _)| *o == obj) {
                        Some(entry) => entry.1 += keep,
                        None => spent.push((obj, keep)),
                    }
                    kept.push(u32::try_from(keep).unwrap_or(u32::MAX));
                }
            }
            for i in (0..self.retake_from).rev() {
                let obj = thread.native_pin_roots[self.pin_base + i];
                let mut left = self.depths[i].saturating_sub(kept[i]);
                while left > 0 {
                    // Wave 28: never below the entry's hold count.
                    if let Some(budget) = gained.iter_mut().find(|(o, _)| *o == obj) {
                        if budget.1 == 0 {
                            kept_by_entry += left;
                            break;
                        }
                        budget.1 -= 1;
                    }
                    left -= 1;
                    match crate::vm::vm_exec::monitor_exit_and_retract_jmx(
                        shared,
                        obj,
                        thread.thread_id,
                    ) {
                        Ok(()) => released += 1,
                        Err(_) => {
                            if dbg_deopt_enabled() {
                                eprintln!(
                                    "[cratonvm-deopt] an OSR'd body's abandoned frame named a \
                                     compiled lock this thread does not hold; not released further"
                                );
                            }
                            break;
                        }
                    }
                }
            }
        } else if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] an OSR'd body's lock pins were truncated underneath; \
                 {n} compiled lock(s) not released"
            );
        }
        thread.native_pin_roots.truncate(self.pin_base);
        if kept_by_entry > 0 && dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] kept {kept_by_entry} hold(s) an OSR'd body's abandoned frame \
                 named beyond the live record: the frame held them at the OSR entry"
            );
        }
        released
    }

    /// A frame resumed IN PLACE from an OSR'd body — the live interpreter
    /// frame at `frame_idx`, rewritten by an OSR-exit transfer — holds exactly
    /// the locks the exit frame names: REPLACE its `held_monitors` record with
    /// the trapping scope's pinned locks (`lock_depth` copies each, outermost
    /// first), then drop the pins. Nothing is entered or released.
    /// Interpreter round i1 wave 25, lane L2 (stage 2 of proposal
    /// `i23-L7-proposal-per-frame-locked-monitors`, the OSR half).
    ///
    /// Replaced, not added to: the record held the locks the interpreter took
    /// before the entry, and the compiled body may have released some of them
    /// since; the exit frame's description includes every one still held (the
    /// entry contract lists them `relock == false`). A frame that describes
    /// no taken lock (a single-pass body's where its wave-26 monitor analysis
    /// has no answer) leaves the record
    /// empty, which is what the transfers did before (`try_osr_with_backoff`
    /// emptied it): an under-stated record is the safe direction
    /// (`held_monitors`' module doc).
    pub(crate) fn replace_live_record_and_unpin(self, thread: &mut JvmThread, frame_idx: usize) {
        let truncated = !self.depths.is_empty() && self.pins_truncated(thread);
        let locks = if self.depths.is_empty() {
            Vec::new()
        } else {
            self.live_frame_locks(thread)
        };
        if let Some(frame) = thread.frames.get_mut(frame_idx) {
            if self.depths.is_empty() {
                // Interpreter round i6 wave 117, lane L6 (page
                // `i116-L9-strict-monitorexit-throws-on-an-understated-record`):
                // an exit frame that names no lock may be a single-pass body's
                // with no monitor answer, so the emptied record is marked
                // understated where the frame can hold one, and the strict
                // `monitorexit` arm stays lenient for this thread.
                crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                    frame,
                    crate::runtime::interpreter::held_monitors::RecordProducer::OsrExitTransfer,
                );
            } else {
                frame.held_monitors.clear_entries(); // t1 w5 lane K: keeps the exited bit
            }
            for (obj, depth) in locks {
                for _ in 0..depth {
                    frame.held_monitors.push(obj);
                }
            }
            // Wave 121 (lane L6): the producer in the mark; truncated pins
            // (`live_frame_locks` answered nothing) leave it understated.
            if !self.depths.is_empty() {
                Self::note_seeded_record(
                    frame,
                    truncated,
                    crate::runtime::interpreter::held_monitors::RecordProducer::OsrExitTransfer,
                );
            }
        }
        if !self.depths.is_empty() {
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }

    /// [`Self::replace_live_record_and_unpin`] for a transfer that RE-TOOK
    /// the exit frame's elided levels (`transfer_osr_guard_exit_into_live_frame`,
    /// wave 26): the record names the taken locks and then each re-taken
    /// level, `lock_depth` copies each. Interpreter round i1 wave 27, lane L2.
    ///
    /// Until wave 27 the re-taken levels were left out, which is not the
    /// safe direction it was taken for once a later sink counts the record:
    /// after `synchronized (o) { synchronized (o) { loop1 } loop2 }` left at
    /// `loop1`'s header (record `[o]`, table 2), the interpreter's inner
    /// `monitorexit` spent the record's one entry, so at `loop2` the frame
    /// held `o` once with an EMPTY record; a later OSR body entered there
    /// that left through a reason-9 pad with no exception pending
    /// ([`release_locks_of_own_pad_exits`] with the live frame, or the chain
    /// refusal of `try_osr`) released every hold beyond the record
    /// ([`Self::release_beyond_live_record`]) — the interpreter's own hold of
    /// `o` — and the frame's outer `monitorexit` then threw
    /// `IllegalMonitorStateException` with the lock already free.
    ///
    /// Round 13 wave 8 (lane sync5): the record is the OUTERMOST scope's
    /// ([`Self::live_frame_locks`]), and so are the re-taken levels
    /// ([`Self::pin_compiled_locks_and_elided_levels`]); identical for a
    /// single-scope frame. An inlined chain's inner scopes become pushed
    /// frames, which the chain transfer seeds with their own locks.
    pub(crate) fn replace_live_record_with_retaken_levels_and_unpin(
        self,
        thread: &mut JvmThread,
        frame_idx: usize,
    ) {
        let n = self.depths.len();
        let truncated = n != 0 && self.pins_truncated(thread);
        let mut locks = if n == 0 {
            Vec::new()
        } else {
            self.live_frame_locks(thread)
        };
        if n > self.retake_from && thread.native_pin_roots.len() >= self.pin_base + n {
            for i in self.retake_from..n {
                locks.push((thread.native_pin_roots[self.pin_base + i], self.depths[i]));
            }
        }
        if let Some(frame) = thread.frames.get_mut(frame_idx) {
            if n == 0 {
                // Wave 117 (lane L6): as `replace_live_record_and_unpin`.
                crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                    frame,
                    crate::runtime::interpreter::held_monitors::RecordProducer::PlanlessTransfer,
                );
            } else {
                frame.held_monitors.clear_entries(); // t1 w5 lane K: keeps the exited bit
            }
            for (obj, depth) in locks {
                for _ in 0..depth {
                    frame.held_monitors.push(obj);
                }
            }
            // Wave 121 (lane L6): as `replace_live_record_and_unpin`.
            if n != 0 {
                Self::note_seeded_record(
                    frame,
                    truncated,
                    crate::runtime::interpreter::held_monitors::RecordProducer::PlanlessTransfer,
                );
            }
        }
        if n != 0 {
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }
}

/// Release the compiled locks of the reason-9 frames the activation a door
/// just ran published on its way out, when no exception is pending: the
/// no-exception sentinel arm of `jit_bridge::run_jit_body_raw` (every
/// method-entry door) and of `try_osr`. Returns how many single releases
/// succeeded. Interpreter round i1 wave 25, lane L2
/// (`docs/feature-designs/real-jdk-mode.md`).
///
/// A protected call whose callee answered the DEOPT sentinel (a declined
/// callee-deopt service, a dispatch helper that raised only the deopt flag)
/// still reaches the caller's reason-9 pad, which publishes the caller's
/// frame and returns the sentinel without running a `monitorexit`. With no
/// exception pending, no handler runs and nothing resumes that frame, so it
/// is the only description of the locks the activation's compiled code took
/// (`relock == false`) and still holds; dropping it unreleased left the thread
/// owning them after the door re-ran the method from entry.
///
/// Claimed from the top of the stash while a frame sits above `floor` (the
/// depth the door recorded before entering the body), names this method and
/// was published by the body the door ran (`owns_point`); the first frame that
/// is not one stops the walk. Every claimed frame belongs to an activation
/// that left through its pad — the door's own, or a nested activation of the
/// same body whose caller passed the sentinel on — so each one's locks are
/// released, innermost first. With `live_frame` (the OSR door), the live
/// interpreter frame outlives the body: only the holds its record does not
/// already name are released ([`CompiledLocksOfAStash::release_beyond_live_record`]).
///
/// A frame below the floor is an earlier activation's, whose own sink decided
/// its locks' fate; it is never claimed here.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn release_locks_of_own_pad_exits(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owns_point: &dyn Fn(usize) -> bool,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    floor: usize,
    live_frame: Option<(usize, &OsrEntryHolds)>,
) -> u32 {
    let mut released = 0u32;
    while let Some((rframe, _cause, _point)) =
        cratonvm_jit::deopt::take_exceptional_frame_above_if(floor, |frame, point| {
            owns_point(point)
                && deopt_frame_matches_method(frame, class_name, method_name, descriptor)
        })
    {
        // Pinned right after the take, before anything that can allocate.
        let locks = CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
        released += match live_frame {
            Some((frame_idx, entry)) => {
                locks.release_beyond_live_record(shared, thread, frame_idx, entry)
            }
            None => locks.release_for_a_rerun(shared, thread),
        };
    }
    if released > 0 && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] released {released} compiled lock hold(s) of {class_name}.\
             {method_name}{descriptor}'s reason-9 frame(s) published for a deopt sentinel"
        );
    }
    released
}

/// The locks the LIVE interpreter frame's thread owned at an OSR entry,
/// through the frame's reference locals: `(local slot, the thread's hold
/// count of the object in it)`, one per distinct owned object. Taken by
/// `jit_bridge::try_osr` before the body runs, and read only by
/// [`CompiledLocksOfAStash::release_beyond_live_record`], which releases a
/// named object at most as often as the thread's hold count of it grew since.
/// Interpreter round i1 wave 28, lane L2
/// (`docs/feature-designs/real-jdk-mode.md`).
///
/// # Why the locals and the monitor table
///
/// The frame's `held_monitors` record may under-state (a committed
/// single-pass exit that names no lock empties it), and the artifact's entry
/// contract names its pre-entry locks by machine location, which the
/// interpreter cannot read before the body runs. javac keeps every block
/// lock in a local for the whole block (`aload o; dup; astore t;
/// monitorenter`), and the monitor table's count cannot under-state. The
/// count includes holds of the same object by CALLER frames; the release rule
/// compares it with the count at the abandon point, so those cancel out.
///
/// # GC
///
/// Slots, not addresses: the live frame's locals are roots every collector
/// remaps, and nothing writes them between the entry and the two readers (the
/// no-exception sentinel arm continues the frame without a transfer, and the
/// chain refusal comes after a transfer that refuses before its first write).
/// The OSR-entry park (`CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`, default on since
/// gen r5w5/oomjit9) nulls the frame's reference locals, but leaves every slot
/// this snapshot names in place ([`Self::names_slot`], gcf f1/f + f2/f). A
/// slot that no longer holds a reference when read back is skipped, and its
/// object falls back to the record rule.
///
/// # Cost
///
/// One pass over the frame's locals per OSR entry: a tag read per slot and a
/// mark-word read (`MonitorTable::holds`) per non-null reference; nothing is
/// allocated unless the thread owns one of them. An entry already builds the
/// seeded locals and tags from the same slots and walks every deopt point of
/// the artifact in `validate_osr_entry`.
#[derive(Debug, Default)]
pub(crate) struct OsrEntryHolds {
    pub(in crate::runtime::interpreter::deopt_resume) slots: Vec<(usize, u32)>,
}

impl OsrEntryHolds {
    /// Snapshot `thread.frames[frame_idx]` at an OSR entry (see the type).
    pub(crate) fn snapshot(shared: &SharedVm, thread: &JvmThread, frame_idx: usize) -> Self {
        let mut slots: Vec<(usize, u32)> = Vec::new();
        let Some(frame) = thread.frames.get(frame_idx) else {
            return Self { slots };
        };
        let monitors = &shared.threads.monitors;
        // gcf f1/f: only the slots the root scan treats as roots at this pc
        // (`Frame::scan_local_objects`' own per-bci liveness answer). A slot
        // the scan dropped as dead keeps its last address and its object tag,
        // and a moving young cycle may since have freed -- and decommitted --
        // the span it points into: `MonitorTable::holds` reading that mark
        // word was the SIGSEGV of
        // `gce-x-osr-entry-holds-snapshot-reads-a-dead-local-in-a-decommitted-young-span-20260929`,
        // and on a span that was reused it named whatever object lives there
        // now. A slot dead at an earlier collection, and not written since, is
        // dead at this pc too (every path from here continues a write-free
        // path from there), so the filter drops every such slot. A lock a
        // frame still owns is live: javac keeps the monitor temporary read by
        // the block's `monitorexit` and by its catch-any handler.
        let live = frame.live_locals_mask_here();
        for i in 0..frame.locals_len() {
            if !live.is_live(i) {
                continue;
            }
            // The kind-honouring tag first, as `park_osr_entry_reference_locals`
            // reads them: a category-2 value is not a reference.
            if frame.get_local_tag(i) != cratonvm_types::VTAG_OBJECT {
                continue;
            }
            let Value::Object(Some(obj)) = frame.get_local_unchecked(i) else {
                continue;
            };
            if !monitors.holds(obj, thread.thread_id) {
                continue;
            }
            let seen = slots
                .iter()
                .any(|&(j, _)| matches!(frame.get_local_unchecked(j), Value::Object(Some(o)) if o == obj));
            if !seen {
                slots.push((i, monitors.entry_count(obj)));
            }
        }
        Self { slots }
    }

    /// Does the snapshot read its object back through local `slot`? gcf f1/f:
    /// for `jit_bridge::park_osr_entry_reference_locals`, which must leave
    /// such a slot in place -- [`Self::gains_since_entry`] reads it after the
    /// body ran, and a parked (nulled) slot makes the snapshot name nothing,
    /// so the record rule alone decides again (the wave-28 defect). The
    /// object is a lock the frame owns, read by the block's `monitorexit`, so
    /// keeping it a root of the interpreter frame retains nothing the frame
    /// had dropped. (gcf f2/f: the caller's hunk is applied, so the
    /// `dead_code` allow is gone.)
    pub(crate) fn names_slot(&self, slot: usize) -> bool {
        self.slots.iter().any(|&(i, _)| i == slot)
    }

    /// Each snapshot object at its current address (read back through the
    /// live frame's slot), with how many holds of it the thread gained since
    /// the entry (0 when it holds it no more often, or no longer at all).
    pub(in crate::runtime::interpreter::deopt_resume) fn gains_since_entry(
        &self,
        shared: &SharedVm,
        thread: &JvmThread,
        frame_idx: usize,
    ) -> Vec<(ObjectRef, u32)> {
        if self.slots.is_empty() {
            return Vec::new();
        }
        let Some(frame) = thread.frames.get(frame_idx) else {
            return Vec::new();
        };
        let monitors = &shared.threads.monitors;
        let mut gained: Vec<(ObjectRef, u32)> = Vec::with_capacity(self.slots.len());
        for &(i, at_entry) in &self.slots {
            if i >= frame.locals_len() || frame.get_local_tag(i) != cratonvm_types::VTAG_OBJECT {
                continue;
            }
            let Value::Object(Some(obj)) = frame.get_local_unchecked(i) else {
                continue;
            };
            let now = if monitors.holds(obj, thread.thread_id) {
                monitors.entry_count(obj)
            } else {
                0
            };
            gained.push((obj, now.saturating_sub(at_entry)));
        }
        gained
    }
}

/// May a deopt sink resume this trapped body from its reconstructed frame
/// WITHOUT the backend's `can_deopt_resume` vouching for it?
///
/// # One predicate, asked at every sink
///
/// Four sinks consume a stashed IR deopt frame, and which one a trap reaches
/// depends only on how the callee was entered. Until 2026-09-07 they gave three
/// different answers to the same event:
///
/// * `try_resume_trapped_callee` (`jit/helpers.rs`) resumed it precisely;
/// * `execute`'s tier-up sink (`interpreter.rs`) raised a hard `InternalError`
///   — fixed 2026-09-07;
/// * `execute_jit_call` (`jit-callsite-a`), `execute_jit_call_decoded`
///   (`jit-callsite-b`) and [`resume_deopted_body_at_point`] re-ran the whole method from
///   entry, with no side-effect check — so a body that had already committed a
///   store committed it a second time, silently.
///
/// A deopt sentinel does NOT mean "nothing happened": the compiled body ran up
/// to `bci` and stopped. Re-entering at bci 0 re-executes everything before it.
/// [`resume_deopted_body_at_point`]'s own doc says exactly that, and names what it cost —
/// the hibernate-reactive `reactiveRemove`-fires-twice defect, one
/// `ArrayLoop.next()` dispatch and two deletes. The fix for THAT added the
/// resume call this predicate now makes reachable: it was gated on
/// `can_deopt_resume`, which an optimizing-tier artifact never has, so it could
/// not fire on the tier the defect actually needs.
///
/// # The three refusals
///
/// They are what the reconstructed frame genuinely cannot describe, and they
/// are the same three `try_resume_trapped_callee` makes or the emission side
/// names:
///
/// * an **`ACC_SYNCHRONIZED`** method — the method monitor is not in the frame;
/// * a body that **takes a monitor at all**. The optimizing tier's frame states
///   have recorded the monitor stack since 2026-09-12 (with a `relock` marker
///   for elided locks), but the SINGLE-PASS backend describes only the monitors
///   it scalar-replaced, and an elision that left no `MonitorInfo` would leave
///   no trace in the frame at all. Note the mood: that backend performs no such
///   elision today — `has_elided_monitor` is assigned `false` once and `true`
///   never, so the conjunct that would report one constant-folds (see the note
///   on `can_deopt_resume` in `jit/src/x64/driver.rs`, and
///   `docs/feature-designs/real-jdk-mode.md`).
///   The refusal is kept because it does not depend on that: this arm cannot
///   tell the two backends' artifacts apart, and eliding is a codegen decision,
///   not a bytecode rewrite, so the ops are still there to see — which is what
///   keeps it holding on the day an untraceable elision is added. An
///   optimizing-tier body with monitors resumes precisely through the
///   `can_deopt_resume` arm instead;
/// * a **resume bci past the method's code**.
///
/// # Additive by construction
///
/// Every caller ORs this beside its existing `can_deopt_resume` arm rather than
/// replacing it. A backend that SET that flag has already vouched no monitor
/// was elided, so applying these guards there too would refuse a single-pass
/// body with an ordinary `synchronized` block that resumes correctly today.
pub(crate) fn sink_precise_resume_allowed(
    code: &[u8],
    code_len: usize,
    is_synchronized: bool,
    bci: u32,
) -> bool {
    cratonvm_jit::deopt_sink_resume_enabled()
        && !is_synchronized
        && !cratonvm_jit::bytecode_holds_monitor(code, code_len)
        && (bci as usize) < code_len
}

/// [`sink_precise_resume_allowed`] for a sink holding a `CachedBytecodeMethod`
/// rather than a raw `Code` attribute.
///
/// `cached.code` is `padded_bytecode`, i.e. the real body followed by zero
/// bytes. That is safe for both readers: `0x00` is `nop`, so the monitor walk
/// runs off the end finding nothing, and the bci bound is the one
/// `try_resume_trapped_callee` already applies against the same padded length.
pub(crate) fn sink_precise_resume_allowed_for(
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    // The bound is on the bci in `cached`'s own code: an inlined chain's
    // `rframe.bci` is in a spliced callee's (wave 26, lane L2).
    sink_precise_resume_allowed(
        &cached.code,
        cached.code.len(),
        cached.is_synchronized,
        stash_identity_scope(rframe).bci,
    )
}

/// real-frame-deopt Step 4 — RESUME a real (Object-bearing) deopt at the trapping
/// bci instead of re-running the whole method from entry (the payoff that kills
/// the side-effect double-execution).
///
/// Builds the interpreter `Frame` from the reconstructed locals/stack (oops
/// GC-rooted across the pool refill — see [`build_deopt_frame_inner`]), then
/// PUSHES it onto `thread.frames` so the interpreter resumes there.
///
/// The GC-rooting HANDOFF is the load-bearing invariant: the temporary
/// `native_pin_roots` pins are held ACROSS `push_resumed_frame` and released
/// only AFTER the push — once
/// pushed, the frame's locals/stack are themselves GC roots (scanned by the
/// normal interpreter-frame root walk), so the reconstructed oops are rooted by
/// the pins, then by BOTH pins and frame, then by the frame alone, with no
/// unrooted window. Returns `Some(FramePushed)` on a clean resume, or `None`
/// (re-run) for an out-of-scope / unmappable frame — releasing any partial pins
/// in both cases.
///
/// Gated by `CRATONVM_DEOPT_REAL` at the sink; the pre-existing
/// `CRATONVM_IR_DEOPT_RESUME` int-only path (`resume_from_ir_deopt`) runs first
/// and is left intact.
///
/// `Err` (round 12 wave 5, lane replay3) is the one refusal with an answer
/// other than a re-run: the scalar-replaced objects could not be re-allocated
/// because the heap is exhausted ([`DeoptFrameBail::VirtualMaterialiseHeap`]).
/// The monitors the compiled code held are released and the returned
/// `OutOfMemoryError` is the trapped method's outcome, thrown at its caller, as
/// HotSpot pops the frame. Every caller propagates it; none re-runs.
///
/// `hints` are the trapping artifact's scope class ids for an inlined chain
/// ([`build_deopt_frame_chain_hinted`]); `&[]` resolves every scope by name.
/// `own_sources` are its inner scopes' own-source templates
/// ([`chain_inner_scope_own_sources`]); `&[]` for none.
pub(in crate::runtime::interpreter) fn resume_real_ir_deopt_or_throw(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    // No nested-stash refusal here, and deliberately. `rframe` is a re-execute
    // point (the only kind the ordinary stash admits), so if its bci holds an
    // invoke, that invoke has NOT run and re-executing it is the correct
    // resume. Anything the take discarded beneath it was a stale leftover, not
    // a callee — see `cratonvm_jit::deopt::deopt_stash_nesting_counts`.
    // N2 — an inlined caller chain is materialised and pushed, not refused.
    // `build_deopt_frame_chain` validates without pushing and without
    // pinning, and `push_inlined_chain` pushes each frame's oops directly into
    // the frame that roots them; only a chain naming scalar-replaced objects
    // pins, between their re-allocation and the push (its own watermark
    // below). A refusal still falls through to the whole-method re-run, so
    // the worst case is exactly the old behaviour.
    if !rframe.caller_frames.is_empty() {
        let trace = dbg_deopt_enabled();
        let Some(mut chain) =
            build_deopt_frame_chain_sourced(shared, cached, rframe, hints, own_sources)
        else {
            return Ok(None);
        };
        // Round 13 wave 5 (lane resume): a chain naming scalar-replaced
        // objects re-allocates them here, so a full heap gets the single
        // frame's answer (`OutOfMemoryError` at the caller). The chain holds
        // no monitor (`chain_virtuals_refusal`), so there is no lock to
        // release for the abandoned activation. Its pins end at the push.
        let pin_base = thread.native_pin_roots.len();
        if let Err(why) = materialise_chain_virtuals(shared, thread, &mut chain) {
            thread.native_pin_roots.truncate(pin_base);
            // Round 15 wave 4 (lane chain4): counted, as `push_inlined_chain`'s
            // own re-allocation counts it (this return was uncounted).
            note_inlined_chain_census(shared, ChainCensusEvent::Refused);
            if frame_build_heap_failure_is_answered(why) {
                return Err(rematerialisation_out_of_memory(shared, thread));
            }
            return Ok(None);
        }
        let pushed = push_inlined_chain(shared, thread, chain, trace);
        if thread.native_pin_roots.len() > pin_base {
            thread.native_pin_roots.truncate(pin_base);
        }
        return Ok(pushed);
    }
    let pin_base = thread.native_pin_roots.len();
    match build_deopt_frame_or_refusal(shared, thread, cached, rframe, false, true) {
        Ok(frame) => {
            // Push FIRST, pins STILL installed: during the push the oops are
            // rooted by the pins, and once pushed also by the frame. Only THEN
            // release the pins — the frame roots them from here on. No
            // MethodEntry: the method was entered in compiled code.
            push_resumed_frame(thread, frame);
            thread.native_pin_roots.truncate(pin_base);
            Ok(Some(CachedCallResult::FramePushed))
        }
        Err(why) => {
            // Out-of-scope / unmappable: release any partial pins, fall back to
            // the whole-method re-run. A full heap is answered instead.
            thread.native_pin_roots.truncate(pin_base);
            if frame_build_heap_failure_is_answered(why) {
                return Err(rematerialisation_out_of_memory(shared, thread));
            }
            Ok(None)        }    }}

/// deopt-osr Step 8 follow-up (P4) — TRUE OSR-exit: transfer the JIT-advanced loop
/// state from a reconstructed frame into the LIVE interpreter frame at `frame_idx`
/// (overwrite locals + operand stack in place, set pc), so the interpreter resumes
/// the loop body exactly where the OSR-compiled code bailed.
///
/// OSR is *same-frame* replacement — the OSR'd code ran within `frame_idx`'s
/// logical frame — so unlike `resume_real_ir_deopt` (which pushes a NEW frame for a
/// normal-call deopt) this MUTATES the existing frame, preserving its identity and
/// bookkeeping (`backward_count`, `osr_attempt_counts`, `monitor_on_exit`, `seq`,
/// the cold metadata) that a wholesale rebuild-and-swap would drop. That matters:
/// the safe-reject alternative discards the JIT's advanced loop state and lets the
/// interpreter re-run the iterations the OSR'd code already executed, which
/// double-executes any side effect it committed (the gap this closes).
///
/// Returns `Some(())` on a clean transfer (the caller then returns `None` from
/// `try_osr`, so the interpreter resumes THIS mutated frame), or `None` for an
/// out-of-scope / unmappable frame.
///
/// **A `None` here is NOT a safe reject.** By the time this runs the OSR'd body
/// has committed iterations, so "continue interpreting THIS frame from where it
/// was" re-executes every one of them. `artifact` + `plan` are the validated
/// entry (`CompiledMethod::validate_osr_entry`, spent by `osr_enter_planned`);
/// its `osr_exit_policy` walked every deopt point of the artifact at ADMISSION
/// and refused the entry outright (`osr-entry-unresumable-exit`) when any of
/// them could reconstruct a frame this transfer would reject. That is what makes
/// the refusal path unreachable after a committed body rather than merely rare —
/// discovering it here would be useless, because the only remaining options are
/// to replay the committed iterations or to lose them. See
/// `docs/jit/on-stack-replacement.md` §4.
///
/// `plan.resume_after_exit` — not `rframe.bci` — names the pc the live frame is
/// parked at; see the call site below for what it re-checks.
///
/// GC-safety. The reconstructed oops were read in-stub (`x64_deopt_entry`) as raw
/// heap words. The OSR-exit snapshot's provenance is Register / StackSlot /
/// StackSlotRef only — never `VirtualObject` — so there is NO materialization, and
/// an in-place overwrite reuses the frame's existing pooled buffers, so there is NO
/// pool refill. Hence NO Java-heap allocation runs between the in-stub capture and
/// the writes below: the addresses stay current and become rooted by the frame's
/// own GC-scanned slots the instant they are written. A `VirtualObject`-bearing
/// frame (unreachable from the OSR-exit emitter today) bails to reject rather than
/// allocate without the pin dance. The per-slot oop typing comes from the same
/// `local_oop_masks` dataflow that the already-validated deopt-EXIT resume relies
/// on; under `CRATONVM_DEOPT_VERIFY` the structural invariants are checked first.
pub(in crate::runtime::interpreter) fn transfer_osr_exit_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    plan: &cratonvm_jit::OsrEntryPlan,
) -> Option<()> {
    match transfer_osr_exit_into_live_frame_checked(
        shared, thread, frame_idx, rframe, artifact, plan,
    ) {
        Ok(()) => Some(()),
        Err(why) => {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit transfer reject ({why}) at bci={}",
                    rframe.bci
                );
            }
            None
        }
    }
}

/// The body of [`transfer_osr_exit_into_live_frame`], returning the refusal
/// *reason* instead of a bare `None`.
///
/// Split out so every refusal has a name a test can assert on. The bare
/// `Option` the caller sees discards the reason (and traces it under
/// `CRATONVM_DBG_DEOPT`), which is exactly how a `MaterializationRequired`
/// slot used to be reported as the generic "unmappable local" — see the guard
/// below.
///
/// **Fail-closed contract.** Every check that can refuse runs BEFORE the first
/// write to the live frame, so a refusal can never half-write it. That includes
/// the resume-bci decision: [`cratonvm_jit::OsrEntryPlan::resume_after_exit`]
/// is consulted before the locals/stack are overwritten, not after.
pub(in crate::runtime::interpreter) fn transfer_osr_exit_into_live_frame_checked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    plan: &cratonvm_jit::OsrEntryPlan,
) -> Result<(), String> {
    use cratonvm_jit::deopt::FrameValue;
    let trace = dbg_deopt_enabled();

    // Phase-A scope (mirror `build_deopt_frame_inner`): a single non-inlined frame,
    // no held monitors, no virtual (scalar-replaced) slots. The OSR-exit snapshot
    // never emits virtuals; if a future emitter does, reject until the elided-
    // monitor handling that gates `build_deopt_frame_inner` is wired here too.
    //
    // A caller chain is no longer refused here — it is transferred by the
    // multi-frame sibling below. The OUTERMOST scope is the OSR'd method, whose
    // interpreter frame is the live one this function writes in place; every
    // scope beneath it is an inlined callee that has to be PUSHED. Step 2 of the
    // five-step chain in
    // `docs/feature-designs/real-jdk-mode.md`.
    //
    // A refusal there still returns `Err`, so the caller's behaviour for any
    // chain the transfer cannot honour is exactly what it was.
    if !rframe.caller_frames.is_empty() {
        let transferred = transfer_osr_exit_chain_into_live_frame(
            shared,
            thread,
            frame_idx,
            rframe,
            artifact,
            ChainExitProvenance::RecordedBci,
            // A planned exit never re-allocates (see the function).
            &mut false,
        );
        // Round 15 wave 3 (lane chain3, CH2W3-1).
        note_osr_chain_transfer(shared, &transferred);
        return transferred.map(|_| ());
    }
    // The in-place transfer has no relock path; a lock the compiled code took
    // (`relock == false`) is still held and stays with the live frame.
    if rframe.monitors.iter().any(|m| m.relock) {
        return Err("held monitors that must be re-acquired".to_string());
    }
    if rframe.locals.iter().chain(rframe.stack.iter()).any(|v| {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        )
    }) {
        return Err("virtual-object slot".to_string());
    }
    // `MaterializationRequired` is NOT `Unsupported`, and the difference is the
    // whole reason `jit/src/deopt.rs` split the variant out: `Unsupported` means
    // "the coarse whole-method classifier cannot name this slot's kind" (tolerated
    // below — the live frame's current value is provably safe to leave in place),
    // whereas `MaterializationRequired` means an optimization DELETED a value that
    // *was* live and left no rebuild recipe. The live frame's stale pre-entry word
    // is then genuinely WRONG, not merely unread, and reconstructing it as a silent
    // null/zero is precisely what the variant exists to make impossible.
    //
    // Without this arm the refusal still happened — the variant falls through
    // `fv_to_value`'s catch-all `None` — but it was reported as "unmappable local",
    // naming the wrong cause, and only for LOCALS (a stack slot went to
    // "unmappable stack slot"). Naming it here also makes it a *scope* verdict,
    // decided before any mapping, alongside the other Phase-A refusals.
    //
    // Belt and braces: `deopt::frame_state_is_resumable` makes the same call on the
    // jit side, so `validate_osr_entry` refuses such an artifact at ENTRY
    // (`osr-entry-unresumable-exit`) and this transfer should never see one.
    if let Some(what) = rframe
        .locals
        .iter()
        .enumerate()
        .map(|(i, v)| ("local", i, v))
        .chain(
            rframe
                .stack
                .iter()
                .enumerate()
                .map(|(i, v)| ("stack", i, v)),
        )
        .find_map(|(region, i, v)| match v {
            FrameValue::MaterializationRequired(ev) => {
                Some(format!("materialization required ({region} {i}: {ev})"))
            }
            _ => None,
        })
    {
        return Err(what);
    }

    // CRATONVM_DEOPT_VERIFY: structural + oop-plausibility checks before mutating
    // the frame — mirrors `build_deopt_frame_inner`. Structural catches a shifted /
    // malformed snapshot (slot counts past the method maxima, bad virtual
    // descriptors); oop-plausibility (`verify_reconstructed_oops`) catches a
    // garbage address in an `Object` slot before it is written into the live frame
    // (which would otherwise hand the GC a dangling root). Default-off ⇒ skipped.
    if cratonvm_jit::deopt_verify_enabled() {
        let (max_locals, max_stack) = {
            let frame = &thread.frames[frame_idx];
            (frame.max_locals, frame.max_stack)
        };
        let verdict = verify_reconstructed_frame(rframe, max_locals, max_stack)
            .and_then(|()| verify_reconstructed_oops(rframe, shared));
        if let Err(why) = verdict {
            eprintln!(
                "[DEOPT-VERIFY] OSR-exit bci={}: reconstructed-frame invariant violated: {why} \
                 — forcing safe reject",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    // The reconstructed slots must fit the live frame's storage. (`set_local_unchecked`
    // / `push_unchecked` panic out-of-bounds, so this guard is load-bearing.)
    {
        let frame = &thread.frames[frame_idx];
        if rframe.locals.len() > frame.locals_len() || rframe.stack.len() > frame.max_stack as usize
        {
            return Err("slot overflow".to_string());
        }
    }

    // Map reconstructed FrameValues → interpreter Values (Int / Long / Float /
    // Double / Object / Undefined via `fv_to_value`; virtual / unresolved →
    // None ⇒ reject). Pure Rust; no Java allocation. Done BEFORE any frame
    // mutation so a reject can never half-write the frame.
    //
    // `ir_deopt_frame_values` is the 1:1 (NON-collapsing) mapper, which is exactly
    // what the in-place transfer needs: the locals snapshot is JVM-slot-indexed
    // (one entry per slot), and we write it slot-for-slot below. A cat-2
    // `long`/`double` therefore arrives as two entries — `Long`/`Double` at slot N
    // plus the reserved upper-half `Undefined` (→ `Int(0)`) at N+1 — which is the
    // correct two-slot JVM layout (`lload N` reads the full value from slot N; the
    // dead N+1 is never read). The deopt-EXIT path, which builds a fresh frame via
    // `Frame::new_pooled`, uses the COLLAPSING `ir_deopt_locals` instead because
    // `copy_args_to_locals` re-expands a compact list; here there is no
    // re-expansion, so collapsing would mis-align the direct slot writes.
    //
    // FIX (jit-osr-loop-duplicate-execution, silent data corruption): a LOCAL
    // slot's `FrameValue::Unsupported` must NOT reject the whole transfer the
    // way an unmappable STACK slot does, and it never refuses here: refusing
    // after the OSR'd body ran is the double execution described below.
    //
    // What makes leaving the live value in place sound is decided at COMPILE
    // time, where the deopt point is published
    // (`jit/src/x64/deopt_stubs.rs::build_frame_state_at`). A local that is not
    // live-in at the point's bci (handler-aware bytecode liveness) is published
    // `Undefined`, however the whole-method classifier typed it. A live local
    // of a slot reused as two kinds is described through its per-bci kind.
    // Only a live local that cannot be described is published `Unsupported`,
    // and one of those anywhere in the artifact makes `validate_osr_entry`
    // refuse the ENTRY (`osr_exit_policy`) before any iteration runs. An
    // `Unsupported` local that still reaches an admitted transfer comes from a
    // resolve-time metadata defect (`deopt::resolve_value`), not from a slot
    // the classifier could not type. See
    // `jit-resume-tolerates-unsupported-locals-without-liveness-FIXED-20260912.md`.
    // Previously this fell through to `bail("unmappable local")` on
    // every method with any such slot, which discarded the whole transfer —
    // even after the OSR'd loop had already run to completion with real,
    // committed side effects (e.g. `ArrayList.add`) — and let the interpreter
    // resume from the STALE pre-OSR pc/locals, silently re-executing (and
    // re-committing) every iteration since OSR entry. See
    // jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md.
    let mut locals: Vec<Option<Value>> = Vec::with_capacity(rframe.locals.len());
    for (i, v) in rframe.locals.iter().enumerate() {
        if matches!(v, cratonvm_jit::deopt::FrameValue::Unsupported) {
            locals.push(None);
        } else {
            match fv_to_value(v) {
                Some(val) => locals.push(Some(val)),
                // Reachable only for a variant `fv_to_value` cannot type. The
                // `MaterializationRequired` case — historically the confusing
                // occupant of this arm — is named by its own guard above, so this
                // label no longer stands in for it.
                None => return Err(format!("unmappable local ({i}: {v:?})")),
            }
        }
    }
    let stack_vals = match ir_deopt_frame_values(&rframe.stack) {
        Some(s) => s,
        None => return Err("unmappable stack slot".to_string()),
    };

    // The exact bci to park the live frame at. This is the ONLY sanctioned resume
    // point once compiled code has run: `resume_after_exit` re-checks that
    // `rframe.bci` is a deopt point THIS artifact recorded and that its
    // `ResumeSemantics` is `REEXECUTE` (i.e. the bytecode there has not taken
    // effect), and returns the reconstructed frame's own bci — never the OSR entry
    // bci. The bare `frame.pc = rframe.bci` it replaces trusted a bci that could be
    // a mis-routed stash, or a `RESUME`/`RETHROW` point that must not be re-executed
    // (the same double-execution defect this whole path exists to prevent, one
    // bytecode instead of one loop iteration).
    //
    // Decided BEFORE the writes below so a refusal leaves the frame untouched. A
    // refusal here means the OSR'd body committed work the interpreter cannot be
    // resumed after — the caller must NOT safe-reject; see the exit sink in
    // `try_osr`. `validate_osr_entry`'s `osr_exit_policy` walks every deopt point
    // of the artifact at admission and refuses the entry outright
    // (`osr-entry-unresumable-exit`) when one of them could land here, which is
    // what makes this branch unreachable rather than merely rare.
    let resume_bci = plan
        .resume_after_exit(artifact, rframe)
        .map_err(|b| format!("unresumable exit: {b}"))?;

    // Overwrite the live frame IN PLACE. No Java allocation here, so the
    // reconstructed oops remain valid and are rooted by the frame's slots the moment
    // they are written. The locals snapshot is JVM-slot-indexed (one entry per slot,
    // cat-2 as its two-slot pair), so slot `i` ← `locals[i]` is 1:1. A `None` entry
    // (an `Unsupported` source slot, see above) leaves that slot's existing live
    // value untouched instead of writing anything.
    let frame = &mut thread.frames[frame_idx];
    for (i, v) in locals.iter().enumerate() {
        if let Some(val) = v {
            frame.set_local_unchecked(i, *val);
        }
    }
    frame.stack.clear();
    for v in &stack_vals {
        frame.stack.push_unchecked(*v);
    }
    // The plan-validated resume point (see above), not the raw `rframe.bci`.
    frame.pc = resume_bci;

    // osr-02 frame comparator: record the RESUMED frame, after the write.
    //
    // After, not before, and not from `rframe`: what the brief asks about is
    // the state the interpreter resumes on, and that is not always what the
    // reconstruction proposed. An `Unsupported` source slot is deliberately
    // left at the live frame's current value (see the mapping loop above), so a
    // record built from `rframe` would describe a frame that never exists.
    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_exit(frame, resume_bci);
    }

    if trace {
        eprintln!(
            "[cratonvm-deopt] OSR-exit TRANSFER into live frame: resume bci={resume_bci} \
             ({} locals, {} stack, entry_pc={})",
            locals.len(),
            stack_vals.len(),
            plan.entry_pc,
        );
    }
    Ok(())
}

/// The RBC.6b lift's exception exit: transfer a **reason-9**
/// (`DeoptReason::PendingException`) frame published by an OSR'd body into the
/// live interpreter frame and park it at `handler_pc`, with `exc` on the
/// operand stack.
///
/// The sibling of [`transfer_osr_exit_into_live_frame_checked`], and it exists
/// for the same reason that one does: once an OSR'd body has committed loop
/// iterations, "resume interpretation where the frame was parked" is not a safe
/// fallback but a silent re-execution of everything since OSR entry (RBC.7).
/// Until 2026-08-17 that could not arise, because `compile_osr_artifact`
/// refused every method with an exception table outright; now that it admits
/// the ones whose protected-range throwing sites all publish a precise frame,
/// this is the path those exceptions take.
///
/// Three things differ from the resume transfer, and each is deliberate:
///
///  * **The resume point is a handler, not a re-execute bci.** A reason-9
///    point's [`cratonvm_jit::deopt::ResumeSemantics`] is `RETHROW`, so
///    `OsrEntryPlan::resume_after_exit` correctly refuses it — its bci names a
///    THROWING instruction, not somewhere the interpreter may be parked. The
///    caller has already run that bci through this frame's own exception table
///    (`find_exception_handler_any_pc`) and passes the handler it found.
///
///  * **The operand stack is not restored.** JVMS §2.10: entering a handler
///    empties the operand stack and pushes the throwable. Whatever the snapshot
///    recorded there is discarded by definition, so an unmappable STACK slot —
///    fatal to the resume transfer, which has to rebuild the stack exactly —
///    cannot make this transfer wrong. Locals are the whole payload.
///
///  * **The bci is checked against a RETHROW point.** The resume transfer
///    spends `resume_after_exit` on that check; here it is made through
///    `osr_exit::exceptional_reason_at_bci`, the rethrow half of that module's
///    two-way partition of the point list. A frame whose bci this artifact
///    recorded no rethrow point for is a mis-routed stash, and routing a
///    mis-routed stash into a handler would enter it with another site's
///    locals.
///
/// Fail-closed exactly as the sibling is: every check that can refuse runs
/// BEFORE the first write to the live frame, so a refusal can never half-write
/// it. `Err` means the caller must NOT resume this frame — see the exception
/// drain in `try_osr`, which propagates instead.
///
/// `heap_exhausted` (OSR round, 2026-10-06): a frame naming scalar-replaced
/// locals is admitted (`deopt::osr_exception_exit_virtuals_refusal`), and they
/// are re-allocated after every check has passed over null placeholders, as
/// the guard-exit sibling does. Set, with `Err`, when that found the heap
/// exhausted: the caller abandons the activation with `OutOfMemoryError`
/// instead of the throwable, as HotSpot pops the frames of a failed
/// reallocation (the handler must not run on locals that were never rebuilt).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn transfer_osr_exception_exit_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    handler_pc: usize,
    exc: ObjectRef,
    point_bci: u32,
    heap_exhausted: &mut bool,
) -> Result<(), String> {
    use cratonvm_jit::deopt::FrameValue;

    // ── Phase-A scope, same three as the sibling ──────────────────────────
    //
    // `rframe` is this artifact's OWN scope. For an exception thrown inside
    // spliced callees the caller passes the chain's outermost scope (the invoke
    // the exception left this method through); `point_bci` is then the bci of
    // the reason-9 point that was stashed, in whatever scope threw.
    if !rframe.caller_frames.is_empty() {
        return Err("inlined caller chain".to_string());
    }
    // As in the sibling: only a monitor the transfer would have to acquire.
    if rframe.monitors.iter().any(|m| m.relock) {
        return Err("held monitors that must be re-acquired".to_string());
    }
    // OSR round (2026-10-06): scalar-replaced locals are re-allocated below,
    // once every check has passed over null placeholders. Monitor-free only
    // (`deopt::osr_exception_exit_virtuals_refusal`): a failed re-allocation
    // abandons the activation, and no path here releases a lock for it.
    let virtual_local = |v: &FrameValue| {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        )
    };
    let rematerialise = rframe.locals.iter().any(virtual_local)
        && rframe.monitors.is_empty()
        && cratonvm_jit::deopt::osr_exception_exit_virtuals_enabled();
    let placeholders;
    let rframe_in = rframe;
    let rframe = if rematerialise {
        placeholders = with_virtual_placeholders(rframe_in);
        &placeholders
    } else {
        rframe_in
    };
    if rframe.locals.iter().any(virtual_local) {
        return Err("virtual-object local".to_string());
    }
    // `MaterializationRequired` means an optimization DELETED a value that WAS
    // live and left no rebuild recipe, so the live frame's stale pre-OSR word is
    // genuinely wrong rather than merely unread. Named here, as its own refusal,
    // for the reason the sibling names it: falling through to the generic
    // "unmappable local" reports the wrong cause. Only LOCALS are inspected —
    // the stack is discarded at handler entry (see the doc comment).
    if let Some(what) = rframe.locals.iter().enumerate().find_map(|(i, v)| match v {
        FrameValue::MaterializationRequired(ev) => {
            Some(format!("materialization required (local {i}: {ev})"))
        }
        _ => None,
    }) {
        return Err(what);
    }

    // ── The stash really is a reason-9 point of THIS artifact ─────────────
    //
    // The caller has already checked the frame's baked `method_key` names this
    // method. That is identity; this is provenance: an ordinary guard deopt
    // (reason 0-8) reaching here would mean the bci names a re-execute point
    // and the exception came from somewhere else entirely.
    //
    // Asked of `semantics.rethrow_exception`, not of `reason ==
    // PendingException` (round 10 wave 9, closing
    // `docs/feature-designs/deopt-osr.md` §5.2's consumer half). The flag
    // is the field that means "this bci is not a resume point", which is
    // exactly the property this check needs; `reason` is the charging key. The
    // two are equivalent on any installed artifact — `DeoptVerifier`'s
    // exception-state-agreement lane refuses a point where they disagree, on
    // both backends' install paths in release builds — so this is the same set
    // of points, asked for the right reason.
    //
    // Asked through `osr_exit::exceptional_reason_at_bci` rather than by an
    // inline `any(..)`, and that is not cosmetic. The two bci lookups in that
    // module PARTITION the point list — `deopt_reason_at_bci` skips the rethrow
    // points, this one skips everything else — and this sink is the half of the
    // partition that had no production caller, which left the split asserted
    // only by `jit`'s own unit tests. A lookup with no caller reads as a guard
    // that is in place and holding when it has never run; routing the sink
    // through it makes the two halves symmetric at the call sites as well as in
    // the module. See
    // `docs/feature-designs/real-jdk-mode.md`.
    //
    // `Ambiguous` is ACCEPTED here, unlike at the `deopt_reason_at_bci` sinks
    // that fall back to `UncommonTrap`. Those ask "which reason do I charge?",
    // a question two disagreeing points genuinely cannot answer. This one asks
    // "did this artifact record a rethrow point at this bci?", and two rethrow
    // points at one bci both answer yes. It is unreachable today —
    // `ResumeSemantics::for_reason` maps exactly one reason to `RETHROW`, pinned
    // by `osr_exit.rs::tests::the_exceptional_lookup_has_no_reachable_ambiguity`
    // — but the arm is written for what it would mean rather than left to be
    // decided under the pressure of a second `RETHROW` reason arriving.
    match cratonvm_jit::osr_exit::exceptional_reason_at_bci(&artifact.deopt_points, point_bci) {
        cratonvm_jit::osr_exit::ReasonAtBci::Unique(_)
        | cratonvm_jit::osr_exit::ReasonAtBci::Ambiguous => {}
        cratonvm_jit::osr_exit::ReasonAtBci::None => {
            return Err(format!(
                "exit bci {} is not a RETHROW (PendingException) point of this artifact",
                point_bci
            ));
        }
    }

    // ── Oop plausibility, before a garbage address becomes a GC root ──────
    //
    // Default-off (`CRATONVM_DEOPT_VERIFY`). The structural sibling check is
    // deliberately not run: it validates the operand stack against `max_stack`,
    // and this transfer does not write the operand stack.
    if cratonvm_jit::deopt_verify_enabled() {
        if let Err(why) = verify_reconstructed_oops(rframe, shared) {
            eprintln!(
                "[DEOPT-VERIFY] OSR exception-exit bci={}: {why} — refusing the transfer",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    // ── The reconstructed slots must fit the live frame ───────────────────
    if rframe.locals.len() > thread.frames[frame_idx].locals_len() {
        return Err("local slot overflow".to_string());
    }

    // ── Map, then write ───────────────────────────────────────────────────
    //
    // `Unsupported` in a LOCAL leaves the live frame's current value alone, for
    // exactly the argument the sibling makes: a slot that is dead at the
    // throwing bci is published `Undefined` at compile time, and a live slot
    // that cannot be described refuses the OSR entry at admission, so an
    // admitted artifact never carries one here.
    let map_locals = |f: &cratonvm_jit::deopt::ReconstructedFrame| {
        let mut locals: Vec<Option<Value>> = Vec::with_capacity(f.locals.len());
        for (i, v) in f.locals.iter().enumerate() {
            if matches!(v, FrameValue::Unsupported) {
                locals.push(None);
            } else {
                match fv_to_value(v) {
                    Some(val) => locals.push(Some(val)),
                    None => return Err(format!("unmappable local ({i}: {v:?})")),
                }
            }
        }
        Ok(locals)
    };
    let mut locals = map_locals(rframe)?;
    // Every check has passed: re-allocate (the throwable pinned across it, a
    // collection may run), and map the real references. The pins hold them
    // until the live frame roots them, below.
    let mut exc = exc;
    let mut pin_base: Option<usize> = None;
    if rematerialise {
        let exc_pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(exc);
        let materialised = match materialise_osr_exit_virtuals(shared, thread, rframe_in) {
            Ok((materialised, _)) => materialised,
            Err(why) => {
                thread.native_pin_roots.truncate(exc_pin);
                return Err(osr_exit_materialise_refusal(why, heap_exhausted));
            }
        };
        exc = thread.native_pin_roots[exc_pin];
        match map_locals(&materialised) {
            Ok(l) => locals = l,
            Err(why) => {
                // Unreachable: the placeholders mapped, and the materialiser
                // only turns virtual slots into `Object` words.
                thread.native_pin_roots.truncate(exc_pin);
                return Err(why);
            }
        }
        pin_base = Some(exc_pin);
    }

    let frame = &mut thread.frames[frame_idx];
    for (i, v) in locals.iter().enumerate() {
        if let Some(val) = v {
            frame.set_local_unchecked(i, *val);
        }
    }
    // JVMS §2.10 handler entry: empty the operand stack, push the throwable.
    frame.stack.clear();
    frame.stack.push_unchecked(Value::Object(Some(exc)));
    frame.pc = handler_pc;
    // The live frame roots the throwable and the re-allocated objects now.
    if let Some(base) = pin_base {
        thread.native_pin_roots.truncate(base);
    }

    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_exit(frame, handler_pc);
    }
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR exception-exit TRANSFER: throw bci={} -> handler pc={} \
             ({} locals, scalar-replaced objects re-allocated: {rematerialise})",
            rframe.bci,
            handler_pc,
            locals.len(),
        );
    }
    Ok(())
}

/// Tier proposal 27, phase 3 (round 11 wave 12, lane irexc): transfer a GUARD
/// exit of an optimizing OSR body into the live interpreter frame and park it
/// at the guard's bci, which re-executes the guarded bytecode. Returns that bci.
///
/// The third sibling of [`transfer_osr_exit_into_live_frame_checked`], for the
/// entry that has no `OsrEntryPlan`. The resume authority is
/// `osr_exit::guard_exit_resume_bci`: the stashed point, found by the
/// address `ir_deopt_entry` recorded, must be one of THIS artifact's guard
/// boxes at a bci the lowerer admitted (`ir_osr_guard_exit_bcis`), whose frame
/// it proved resumable in place at compile time. Everything else is the
/// sibling's in-place write — locals slot for slot, the operand stack rebuilt,
/// `Unsupported` locals left alone for the reason that function gives.
///
/// Why resuming here and re-entering later is sound is the lowerer's
/// classification, not this function's: see the phase-3 note in
/// `jit/src/osr_exit.rs`. Fail-closed like the siblings: every refusal happens
/// before the first write to the live frame.
///
/// `forced`: the caller's [`claim_forced_chain_trip`] of this trap, made
/// before anything here can refuse (round 15 wave 4, lane chain4; the caller
/// claims since lane chain5, because it also decides the exit's charge,
/// [`leave_forced_trip_uncharged`]). Its outcome is the chain transfer's.
pub(in crate::runtime::interpreter) fn transfer_osr_guard_exit_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    point_addr: usize,
    forced: bool,
    // Set when the refusal is a heap failure re-allocating the frame's
    // scalar-replaced objects (a collection ran): the caller raises
    // `OutOfMemoryError` out of the activation instead of continuing it.
    heap_exhausted: &mut bool,
) -> Result<usize, String> {
    use cratonvm_jit::deopt::FrameValue;

    // Provenance and the resume bci first: nothing below is worth checking of
    // a frame that is not this artifact's admitted guard exit.
    let resume_bci = cratonvm_jit::osr_exit::guard_exit_resume_bci(
        &artifact._deopt_point_boxes,
        artifact.ir_osr_guard_exit_bcis.as_deref(),
        point_addr,
        rframe.bci,
    )
    .map_err(|why| format!("unresumable guard exit: {why}"))?;

    // The admitted frame's shape, re-asserted on what was reconstructed.
    //
    // Round 13 wave 8 (lane chain3; `r13w6-chain2-osr-guard-exit-chain-lock-record-patch`
    // part (b)): an inlined chain -- a guard inside a spliced body -- is
    // transferred as the planned OSR exit transfers one: the OUTERMOST scope
    // written into this live frame, parked after its `invoke`, and the
    // spliced callees pushed above it. `guard_exit_resume_bci` above proved
    // the point is this artifact's and was admitted
    // (`osr_exit::chain_resumable_in_place`: no monitor and no scalar-replaced
    // object in any scope). The returned pc is the live frame's.
    if !rframe.caller_frames.is_empty() {
        let transferred = transfer_osr_guard_exit_chain_into_live_frame(
            shared,
            thread,
            frame_idx,
            rframe,
            artifact,
            heap_exhausted,
        );
        note_forced_chain_resumed(shared, forced, transferred.is_ok());
        return transferred;
    }
    // Interpreter round i1 wave 25, lane L2: a back-edge POLL exit may name
    // the locks the frame holds (`osr_exit::poll_exit_point_resumable`, which
    // `guard_exit_resume_bci` asked above; a guard exit's point still names
    // none). Each is `relock == false`, a hold the thread already has for
    // this activation, so it stays with the live frame (the door rewrites the
    // frame's record from them).
    //
    // Interpreter round i1 wave 26, lane L2: a poll exit may also name an
    // ELIDED level (`relock`: a nested-lock elision's re-take, or a lock
    // escape analysis removed; `osr_exit::OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED`),
    // which the interpreter frame will release and so must hold. It is
    // re-taken below, after every fallible step, as `build_deopt_frame_inner`
    // re-takes one for a method-entry exit; here each such object must
    // already be a resolved, non-null reference. (`guard_exit_resume_bci`
    // above admitted the point through `poll_exit_point_resumable`, which
    // names no elided level with that switch off, and a guard exit's none.)
    let mut retake: Vec<(ObjectRef, u32)> = Vec::new();
    for m in rframe.monitors.iter().filter(|m| m.relock) {
        match &m.object {
            FrameValue::Object(addr) if *addr != 0 => {
                // SAFETY: a non-null reference the stash resolved from the
                // exit frame's own word; nothing that can collect runs between
                // the take and the re-take below (the same window the locals
                // written below rely on).
                let obj = unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) };
                retake.push((obj, m.lock_depth));
            }
            _ => return Err("an elided lock level on an unresolved object".to_string()),
        }
    }
    // The for-each residual round (2026-10-06): a frame naming scalar-replaced
    // objects is admitted monitor-free (`osr_exit::guard_exit_fact`,
    // `deopt::osr_in_place_virtuals_refusal`), and they are re-allocated below,
    // after every check has passed over null placeholders. Asked again here.
    let rematerialise = reconstructed_chain_names_virtual(rframe)
        && cratonvm_jit::deopt::osr_guard_exit_virtuals_enabled()
        && rframe.monitors.is_empty();
    let placeholders;
    let checked = if rematerialise {
        placeholders = with_virtual_placeholders(rframe);
        &placeholders
    } else {
        rframe
    };
    if let Some(what) = checked
        .locals
        .iter()
        .enumerate()
        .map(|(i, v)| ("local", i, v))
        .chain(
            checked
                .stack
                .iter()
                .enumerate()
                .map(|(i, v)| ("stack", i, v)),
        )
        .find_map(|(region, i, v)| match v {
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_) => {
                Some(format!("virtual-object slot ({region} {i})"))
            }
            FrameValue::MaterializationRequired(ev) => {
                Some(format!("materialization required ({region} {i}: {ev})"))
            }
            _ => None,
        })
    {
        return Err(what);
    }

    if cratonvm_jit::deopt_verify_enabled() {
        let (max_locals, max_stack) = {
            let frame = &thread.frames[frame_idx];
            (frame.max_locals, frame.max_stack)
        };
        let verdict = verify_reconstructed_frame(checked, max_locals, max_stack)
            .and_then(|()| verify_reconstructed_oops(checked, shared));
        if let Err(why) = verdict {
            eprintln!(
                "[DEOPT-VERIFY] OSR guard-exit bci={}: {why} — refusing the transfer",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    {
        let frame = &thread.frames[frame_idx];
        if rframe.locals.len() > frame.locals_len() || rframe.stack.len() > frame.max_stack as usize
        {
            return Err("slot overflow".to_string());
        }
    }

    // Map, then write — the sibling's 1:1 (non-collapsing) mapping.
    let map_values = |f: &cratonvm_jit::deopt::ReconstructedFrame| {
        let mut locals: Vec<Option<Value>> = Vec::with_capacity(f.locals.len());
        for (i, v) in f.locals.iter().enumerate() {
            if matches!(v, FrameValue::Unsupported) {
                locals.push(None);
            } else {
                match fv_to_value(v) {
                    Some(val) => locals.push(Some(val)),
                    None => return Err(format!("unmappable local ({i}: {v:?})")),
                }
            }
        }
        match ir_deopt_frame_values(&f.stack) {
            Some(stack) => Ok((locals, stack)),
            None => Err("unmappable stack slot".to_string()),
        }
    };
    let (mut locals, mut stack_vals) = map_values(checked)?;
    // Every check has passed: re-allocate, and map the real references. The
    // pins hold them until the live frame roots them, below.
    let mut pin_base: Option<usize> = None;
    if rematerialise {
        let (materialised, base) = materialise_osr_exit_virtuals(shared, thread, rframe)
            .map_err(|why| osr_exit_materialise_refusal(why, heap_exhausted))?;
        match map_values(&materialised) {
            Ok((l, s)) => {
                locals = l;
                stack_vals = s;
            }
            Err(why) => {
                // Unreachable: the placeholders mapped, and the materialiser
                // only turns virtual slots into `Object` words.
                thread.native_pin_roots.truncate(base);
                return Err(why);
            }
        }
        pin_base = Some(base);
    }

    let frame = &mut thread.frames[frame_idx];
    for (i, v) in locals.iter().enumerate() {
        if let Some(val) = v {
            frame.set_local_unchecked(i, *val);
        }
    }
    frame.stack.clear();
    for v in &stack_vals {
        frame.stack.push_unchecked(*v);
    }
    frame.pc = resume_bci;

    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_exit(frame, resume_bci);
    }
    // The live frame roots the re-allocated objects now.
    if let Some(base) = pin_base {
        thread.native_pin_roots.truncate(base);
    }
    // The elided levels (wave 26), after every fallible step: `lock_depth`
    // re-entrant enters each, published for `getLockedMonitors()` as every
    // acquisition is. Uncontended by construction — the thread holds the
    // object already (a nested elision's re-take) or the object never escaped
    // it (an escape-analysis elision) — so nothing blocks or collects. The
    // door's record rewrite names these levels too since wave 27
    // (`CompiledLocksOfAStash::replace_live_record_with_retaken_levels_and_unpin`):
    // a record short of them let `release_beyond_live_record` release the
    // interpreter's own hold later (see that function).
    for &(obj, depth) in &retake {
        for _ in 0..depth {
            shared.threads.monitors.enter(obj, thread.thread_id);
        }
        if depth > 0 {
            shared
                .threads
                .thread_registry
                .complete_jmx_monitor_enter(thread.thread_id, obj);
        }
    }
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR guard-exit TRANSFER into live frame: resume bci={resume_bci} \
             ({} locals, {} stack, {} elided level(s) re-taken, scalar-replaced objects \
             re-allocated: {rematerialise})",
            locals.len(),
            stack_vals.len(),
            retake.len(),
        );
    }
    Ok(resume_bci)
}

/// The multi-frame half of the OSR-exit transfer.
///
/// **Step 2 of the inliner chain.** The single-frame transfer above writes the
/// live interpreter frame — the one OSR replaced — and returns. With an inlined
/// chain there is more than one frame to restore, and they are not alike:
///
/// * the **outermost** scope IS the OSR'd method, so its frame already exists
///   and is the live one. It is written IN PLACE, and parked at the SUCCESSOR of
///   its invoke, because that invoke is in progress (`RESUME` semantics) — the
///   same rule [`caller_resume_pc`] enforces for the deopt-exit sink.
/// * every scope **beneath** it is an inlined callee with no frame at all. Those
///   are pushed, outermost-first, so the innermost (trapping) one ends up on
///   top and the interpreter resumes in it.
///
/// The innermost scope is parked at its own bci: it is the trapping point, and
/// its bytecode has not taken effect.
///
/// **Fail-closed, and the ordering is the whole safety argument.** Everything
/// resolvable is resolved and checked BEFORE the live frame is touched, because
/// once it has been overwritten there is no way back to the caller's safe
/// reject — and unlike the single-frame case, a partial success here would leave
/// a live frame describing one method and a pushed frame describing another.
/// `materialise_inner_scopes` does all of the fallible work up front; the writes
/// below cannot fail.
///
/// Returns the pc the live frame is parked at (the outermost `invoke`'s
/// successor). The frames it pushes sit ABOVE `frame_idx`, so the caller's
/// interpreter loop must continue in the top frame, not in the live one
/// (`interpreter::try_osr_offer`, round 13 wave 8, lane chain3).
pub(in crate::runtime::interpreter::deopt_resume) fn transfer_osr_exit_chain_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe_in: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    provenance: ChainExitProvenance,
    // See [`transfer_osr_guard_exit_into_live_frame`]'s parameter.
    heap_exhausted: &mut bool,
) -> Result<usize, String> {
    use cratonvm_jit::deopt::FrameValue;

    // The for-each residual round (2026-10-06): the planless guard exit of a
    // chain naming scalar-replaced objects (admitted monitor-free by
    // `osr_exit::chain_resumable_in_place`) is checked over null placeholders,
    // then re-allocated just before the writes. The planned exit's admission
    // refuses every such chain, and so does this transfer for it.
    let rematerialise = provenance == ChainExitProvenance::PointAddress
        && cratonvm_jit::deopt::osr_guard_exit_virtuals_enabled()
        && reconstructed_chain_names_virtual(rframe_in)
        && rframe_in.monitors.is_empty()
        && rframe_in
            .caller_frames
            .iter()
            .all(|c| c.monitors.is_empty());
    let placeholders;
    let rframe = if rematerialise {
        placeholders = with_virtual_placeholders(rframe_in);
        &placeholders
    } else {
        rframe_in
    };

    let depth = rframe.caller_frames.len();
    let budget = inline_resume_budget();
    if depth > budget {
        return Err(format!(
            "inlined chain is {depth} deep, past the {budget}-frame resume budget"
        ));
    }
    let Some(outer_scope) = rframe.caller_frames.last() else {
        return Err("no caller frames".to_string());
    };

    // ── the outermost scope must be the live frame's own method ──────────
    //
    // An artifact only ever inlines INTO its own body, so a chain whose
    // outermost scope names some other method is a mis-routed stash rather than
    // a deep one — the same identity rule the single-frame path applies to
    // `rframe` itself.
    let (
        live_class,
        live_method,
        live_desc,
        live_class_id,
        live_code,
        live_max_locals,
        live_max_stack,
    ) = {
        let f = &thread.frames[frame_idx];
        (
            f.class_name().to_string(),
            f.method_name().to_string(),
            f.method_descriptor().to_string(),
            f.class_id,
            f.code.clone(),
            f.locals_len(),
            f.max_stack as usize,
        )
    };
    if !deopt_frame_matches_method(outer_scope, &live_class, &live_method, &live_desc) {
        return Err(format!(
            "outermost caller scope {} does not name the live frame {live_class}.{live_method}{live_desc}",
            outer_scope.method_key
        ));
    }

    // ── the outermost scope's own values, and where to park it ───────────
    //
    // `caller_frame_values` rather than the tolerant mapping the single-frame
    // transfer uses: this frame is being rewritten to describe a DIFFERENT point
    // in its own execution — the invoke it is parked in — so leaving an
    // `Unsupported` local at whatever the OSR'd body happened to leave there is
    // not the "provably dead or re-stored" case that rule rests on.
    let (mut outer_locals, mut outer_stack) = caller_frame_values(outer_scope)?;
    // `frame.code` carries 2 bytes of speculative-read padding.
    let live_code_len = live_code.len().saturating_sub(2);
    let outer_resume_pc = caller_resume_pc(&live_code, live_code_len, outer_scope.bci as usize)?;
    // `outer_locals` is COMPACT (one entry per value, a `long`/`double` once);
    // written into the live frame it spans its JVM slots
    // (`write_compact_locals_in_place`), so the fit is asked in slots.
    let outer_slots = compact_locals_slot_count(&outer_locals);
    if outer_slots > live_max_locals || outer_stack.len() > live_max_stack {
        return Err(format!(
            "outermost scope {} does not fit the live frame ({} local slots / {} stack against {}/{})",
            outer_scope.method_key,
            outer_slots,
            outer_stack.len(),
            live_max_locals,
            live_max_stack
        ));
    }

    // ── every scope beneath it, fully resolved before anything is written ──
    // By the artifact's own scope class ids (round 13 wave 6, lane chain2),
    // and never from a spliced callee's class redefined since the compile:
    // its frame would be rebuilt from the NEW bytecode at the old bci
    // (`r13w5-resume-chain-inner-scope-of-a-redefined-class`), which the
    // deopt-exit doors already refuse. Round 14 wave 4 (lane resume2, CH3W-3):
    // unless every such scope has the bytecode the compile spliced
    // ([`chain_inner_scope_own_sources`]), which it is rebuilt from and
    // restamped with the body's pool generation after the push, as the doors
    // do (R14DP-5). This refusal comes after committed iterations, so it is
    // not a safe reject: resuming the right bytecode is strictly better.
    let inner_own_sources =
        if chain_inner_scope_redefined_since_compile(shared, artifact, live_class_id, rframe) {
            match chain_own_source_sinks_enabled()
                .then(|| chain_inner_scope_own_sources(shared, artifact, live_class_id, rframe))
                .flatten()
            {
                Some(rows) => rows,
                None => {
                    return Err(
                        "a spliced callee's class was redefined since the compile".to_string()
                    );
                }
            }
        } else {
            Vec::new()
        };
    let mut inner = materialise_inner_scopes(
        shared,
        live_class_id,
        rframe,
        &artifact.splice_scope_class_ids,
        &inner_own_sources,
    )?;

    // The artifact must actually have recorded the trapping bci, exactly as
    // `resume_after_exit` requires for the flat case. Without this a mis-routed
    // stash whose scopes happen to type-check would be resumed. The planless
    // guard exit proved provenance by the point's ADDRESS instead
    // (`osr_exit::guard_exit_resume_bci`); an optimizing tier's chain points
    // live only in its `_deopt_point_boxes`, so this check would refuse every
    // one of them (round 13 wave 8, lane chain3).
    if provenance == ChainExitProvenance::RecordedBci
        && !artifact.deopt_points.iter().any(|p| p.bci == rframe.bci)
    {
        return Err(format!(
            "trapping bci {} is not a recorded deopt point of this artifact",
            rframe.bci
        ));
    }

    // `CRATONVM_DEOPT_VERIFY`, before a reconstructed oop becomes a GC root.
    if cratonvm_jit::deopt_verify_enabled() {
        // Every scope (round 13 wave 5, lane resume): the outermost is written
        // into the live frame and the others pushed, so each one's words
        // become roots.
        if let Err(why) = verify_chain_oops(rframe, shared) {
            eprintln!(
                "[DEOPT-VERIFY] OSR-exit chain at bci={}: {why} — refusing the transfer",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    // Virtual objects have no materializer on this path, in either half.
    if rframe
        .caller_frames
        .iter()
        .chain(std::iter::once(rframe))
        .any(|f| {
            f.locals.iter().chain(f.stack.iter()).any(|v| {
                matches!(
                    v,
                    FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
                )
            })
        })
    {
        return Err("virtual-object slot in an inlined chain".to_string());
    }

    // Every check has passed over the placeholders: re-allocate, and rebuild
    // every scope's values from the real references. Rust-side only between
    // here and the writes; the pins hold the objects until the live frame and
    // the pushed frames root them (released after the push, below).
    let mut pin_base: Option<usize> = None;
    if rematerialise {
        let (materialised, base) = materialise_osr_exit_virtuals(shared, thread, rframe_in)
            .map_err(|why| osr_exit_materialise_refusal(why, heap_exhausted))?;
        let rebuilt = (|| -> Result<(), String> {
            let outer = materialised
                .caller_frames
                .last()
                .ok_or_else(|| "no caller frames".to_string())?;
            let (l, s) = caller_frame_values(outer)?;
            if compact_locals_slot_count(&l) != compact_locals_slot_count(&outer_locals)
                || s.len() != outer_stack.len()
            {
                return Err("the re-allocated outermost scope changed shape".to_string());
            }
            outer_locals = l;
            outer_stack = s;
            // `inner` is outermost-inward, as `materialise_inner_scopes` built it.
            let scopes = materialised
                .caller_frames
                .iter()
                .rev()
                .skip(1)
                .chain(std::iter::once(&materialised));
            for (frame, scope) in inner.iter_mut().zip(scopes) {
                let (l, s) = caller_frame_values(scope)?;
                if l.len() != frame.locals.len() || s.len() != frame.stack.len() {
                    return Err("a re-allocated inner scope changed shape".to_string());
                }
                frame.locals = l;
                frame.stack = s;
            }
            Ok(())
        })();
        if let Err(why) = rebuilt {
            // Unreachable: the placeholders mapped, and the materialiser only
            // turns virtual slots into `Object` words.
            thread.native_pin_roots.truncate(base);
            return Err(why);
        }
        pin_base = Some(base);
    }

    // ── writes only, from here ───────────────────────────────────────────
    {
        let frame = &mut thread.frames[frame_idx];
        // By JVM slot, not by list index (interpreter round i1 wave 24, lane
        // L2): `outer_locals` is the compact list `caller_frame_values` makes
        // for `Frame::new_pooled`, which re-expands it. Written index for index,
        // a `long`/`double` local shifted every later local down one slot.
        // Cannot fail: the fit was checked in slots above.
        let _ = write_compact_locals_in_place(frame, &outer_locals);
        frame.stack.clear();
        for v in &outer_stack {
            frame.stack.push_unchecked(*v);
        }
        frame.pc = outer_resume_pc;
        // Round 13 wave 6 (lane chain2): parked in its invoke, which is where
        // the unwinder searches its handlers once the pushed callee throws.
        if chain_frames_report_their_bci() {
            frame.last_instr_pc = outer_scope.bci as usize;
        }
        if super::osr_frame_trace::enabled() {
            super::osr_frame_trace::record_exit(frame, outer_resume_pc);
        }
    }
    let trace = dbg_deopt_enabled();
    if trace {
        eprintln!(
            "[cratonvm-deopt] OSR-exit CHAIN transfer: {live_class}.{live_method} in place @{outer_resume_pc}, \
             then {} pushed frame(s), trapping bci={}",
            inner.len(),
            rframe.bci
        );
    }
    let first_pushed = thread.frames.len();
    let pushed = push_inlined_chain(shared, thread, DeoptFrameChain::ready(inner), trace)
        .map(|_| outer_resume_pc)
        .ok_or_else(|| {
            "pushing the inlined chain failed after the live frame was written".to_string()
        });
    // The live frame and the pushed frames root the re-allocated objects now.
    if let Some(base) = pin_base {
        thread.native_pin_roots.truncate(base);
    }
    let pushed = pushed?;
    // The pushed frames are the inner scopes only (the outermost is the live
    // frame), outermost-inward from `first_pushed`: restamp each one built
    // from an own-source template before the interpreter runs it.
    if !inner_own_sources.is_empty() {
        let stamp = artifact.compile_cp_stamp();
        let inner_scopes = rframe
            .caller_frames
            .iter()
            .rev()
            .skip(1)
            .chain(std::iter::once(rframe));
        for (k, scope) in inner_scopes.enumerate() {
            if inner_own_sources
                .iter()
                .any(|(key, _)| *key == scope.method_key)
            {
                super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
                    shared,
                    thread,
                    first_pushed + k,
                    stamp,
                );
            }
        }
    }
    Ok(pushed)
}

/// How [`transfer_osr_exit_chain_into_live_frame`] establishes that the chain
/// came out of the artifact it is given (round 13 wave 8, lane chain3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::runtime::interpreter::deopt_resume) enum ChainExitProvenance {
    /// The planned OSR exit: the trapping bci must be one of the artifact's
    /// recorded `deopt_points`.
    RecordedBci,
    /// The planless guard exit: `osr_exit::guard_exit_resume_bci` found the
    /// stashed point among the artifact's boxes by address, admitted.
    PointAddress,
}

/// [`transfer_osr_guard_exit_into_live_frame`] for an inlined chain (round 13
/// wave 8, lane chain3; `r13w6-chain2-osr-guard-exit-chain-lock-record-patch`
/// part (b)). Returns the live frame's new pc.
///
/// Admission (`osr_exit::chain_resumable_in_place`) refused any chain naming a
/// monitor, in any scope: this transfer re-takes no elided level, and a lock
/// here can only be the looping method's own, whose javac catch-all covers the
/// `invoke` the door requires uncovered (the OSR door's record rewrite reads
/// the outermost scope since lane sync5's part (c), so it would be right).
/// Asked again here, before anything is written, so a chain the admission
/// rule never saw cannot reach the rewrite either. Every other check -- identity
/// of the outermost scope, redefinition of a spliced callee's class, fit,
/// virtual objects, `CRATONVM_DEOPT_VERIFY` -- is the planned exit's, in
/// [`transfer_osr_exit_chain_into_live_frame`], which writes nothing before
/// all of them passed.
pub(in crate::runtime::interpreter::deopt_resume) fn transfer_osr_guard_exit_chain_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    heap_exhausted: &mut bool,
) -> Result<usize, String> {
    if !rframe.monitors.is_empty() || rframe.caller_frames.iter().any(|c| !c.monitors.is_empty()) {
        return Err("an inlined chain holding a monitor".to_string());
    }
    let transferred = transfer_osr_exit_chain_into_live_frame(
        shared,
        thread,
        frame_idx,
        rframe,
        artifact,
        ChainExitProvenance::PointAddress,
        heap_exhausted,
    );
    // Round 15 wave 3 (lane chain3, CH2W3-1).
    note_osr_chain_transfer(shared, &transferred);
    let resume_pc = transferred?;
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR guard-exit CHAIN transfer: live frame @{resume_pc}, {} frame(s) \
             pushed, trapping bci={}",
            rframe.caller_frames.len(),
            rframe.bci
        );
    }
    Ok(resume_pc)
}

/// How many JVM local slots a COMPACT local list (`ir_deopt_locals` /
/// `caller_frame_values`: one entry per value) spans: two per `long` /
/// `double`, one per anything else — the layout `copy_args_to_locals` builds.
pub(in crate::runtime::interpreter::deopt_resume) fn compact_locals_slot_count(locals: &[Value]) -> usize {
    locals
        .iter()
        .map(|v| if v.is_category2() { 2 } else { 1 })
        .sum()
}

/// Write a COMPACT local list into an existing frame's JVM slots, a cat-2
/// value at its slot with its upper half invalidated by the store
/// (`Frame::set_local_unchecked`) — `copy_args_to_locals`'s layout, for a frame
/// that is rewritten in place rather than built. Slots past the list keep
/// their values. `Err` (nothing written) when the list spans more slots than
/// the frame has. Interpreter round i1 wave 24, lane L2.
pub(in crate::runtime::interpreter::deopt_resume) fn write_compact_locals_in_place(frame: &mut Frame, locals: &[Value]) -> Result<(), String> {
    let slots = compact_locals_slot_count(locals);
    if slots > frame.locals_len() {
        return Err(format!(
            "{slots} local slots do not fit a frame of {}",
            frame.locals_len()
        ));
    }
    let mut slot = 0;
    for v in locals {
        frame.set_local_unchecked(slot, *v);
        slot += if v.is_category2() { 2 } else { 1 };
    }
    Ok(())
}

/// deopt-osr Step 9 — stamp a freshly compiled artifact with the method's
/// current live compilation epoch (`SharedVm::method_epochs`) so the
/// real-frame-deopt resume sink can distinguish a current compilation from one
/// superseded by a later invalidation. No-op (the field stays `0` and is never
/// read) unless `CRATONVM_DEOPT_REAL` is on, so production artifacts are
/// byte-identical. Uses the same `"<class>.<method>:<descriptor>"` key as
/// `DeoptimizationController::deoptimize`, which advances the epoch.
///
/// Step 9 follow-up (a): ALSO stamp the artifact's `DeoptEpochGuard` (baked into
/// its frame-deopt stubs) with the same creation epoch and a stable pointer to
/// the live-epoch cell. (`x64_deopt_entry` used it to short-circuit before
/// dereferencing the deopt box under a `CRATONVM_JIT_FREE_CODE=1` mode that
/// freed boxes under running frames; that mode is gone and the entry now
/// ignores the guard.) No-op when no guard was emitted (`deopt_epoch_guard`
/// null) — i.e. on every production artifact.
#[inline]
pub(in crate::runtime::interpreter) fn stamp_compilation_epoch(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    cm: &mut crate::jit::CompiledMethod,
) {
    if cratonvm_jit::deopt_real_enabled() {
        let key = format!("{class_name}.{method_name}:{descriptor}");
        // Take a stable pointer to the live-epoch cell first (creating it at
        // epoch 0 if absent), then read its value as the creation epoch, so the
        // field and the baked guard agree. A concurrent `bump` racing here only
        // makes the guard consider itself superseded → safe whole-method re-run.
        let cell = shared.live_epoch_cell_ptr(&key);
        // SAFETY: `cell` is a stable, retained `AtomicU64` from `method_epochs`.
        let epoch = unsafe { (*cell).load(std::sync::atomic::Ordering::Relaxed) };
        cm.compilation_epoch = epoch;
        cm.stamp_deopt_epoch_guard(epoch, cell);
    }
}

/// CRATONVM_DBG_DEOPT — record which `take_last_deopt()` sink consumed a
/// stashed frame, and what method that sink was running.
///
/// The stash is a single thread-local slot with four consumers. A frame taken
/// by the wrong one de-speculates an innocent method and cannot resume, so the
/// consuming site is the first thing any deopt investigation needs and was the
/// one thing not recorded.
pub(crate) fn dbg_deopt_sink(
    site: &str,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    running: &str,
) {
    if !dbg_deopt_enabled() {
        return;
    }
    eprintln!(
        "[cratonvm-deopt] sink={site} running={running} stash={} bci={}",
        rframe.method_key, rframe.bci,
    );
}

/// May the interpreter re-run this method from entry instead of resuming
/// precisely at `resume_bci`?
///
/// `true` means the abandoned compiled attempt cannot have committed anything
/// observable, so a re-run from bci 0 is the same execution — the locals are
/// rebuilt from the same arguments and nothing outside the frame was written.
///
/// # Why a PREFIX, and not the whole body
///
/// The rule used to be "the whole method commits no side effect", and that is
/// the right rule for a method the compiler ran end to end. It is the wrong
/// rule for an abandoned attempt: what a replay could DUPLICATE is only what
/// the attempt already committed, and the attempt stopped at `resume_bci`.
/// Every deopt point this VM emits carries `ResumeSemantics::REEXECUTE` (only
/// `PendingException` differs, and those frames go to a different stash), so
/// the bytecode AT `resume_bci` had not completed and the bytecodes after it
/// never ran. Only `code[..resume_bci]` can have committed anything.
///
/// That is true of straight-line code only. Inside a loop the earlier
/// iterations also ran the code after `resume_bci`, so since round 12 wave 3
/// the scanned range is the loop-closed extent
/// (`cratonvm_jit::ir::replay_committed_extent`,
/// `r12w2-irbuild-replay-prefix-ignores-loops`), and a point the compiler
/// marked `first_arrival` (an IR site trap, which fires the first time control
/// reaches it) scans only what is reachable before that first arrival. The rule
/// itself is `cratonvm_jit::deopt::replay_from_entry_commits_nothing_with_handlers`,
/// the one the compiler also asks before installing a body whose points would
/// need it; [`replay_from_entry_is_observably_equivalent_for_frame`] is the
/// form that can read the frame's `first_arrival`, and this one assumes the
/// point was conditional (never less strict).
///
/// This form follows no exception edge: it is for a caller that holds no body
/// (the `u32::MAX` whole-body question, or an impure `spliced_bodies_pure`,
/// where no edge can change the answer). A sink holding the body that ran asks
/// [`replay_from_entry_is_observably_equivalent_after`] or the stash form,
/// which add the edges that body's local handlers can take (round 13 wave 12,
/// lane replay6).
///
/// The narrower rule cost a real answer: netty's
/// `UnpooledHeapByteBuf._getUnsignedMedium` is `getfield array; invokestatic
/// getUnsignedMedium`, and once `CRATONVM_JIT_IR_INLINE` splices that callee
/// in, an out-of-bounds read deopts at the invoke. The whole body "commits a
/// side effect" — it contains a call — so the replay was refused and a
/// three-byte bounds check raised `InternalError` instead of
/// `IndexOutOfBoundsException`. Nothing before the invoke commits anything, and
/// the call itself never completed, so the replay was always safe. See
/// `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`.
///
/// # The second half
///
/// A spliced artifact's attempt also ran part of a RELOCATED callee body, which
/// the caller's bytecode does not describe. `spliced_bodies_pure` is the
/// artifact's own answer for that half (`CompiledMethod::
/// spliced_bodies_side_effect_free`), computed at compile time with this same
/// predicate over each spliced region. It is vacuously `true` for an artifact
/// that spliced nothing.
///
/// Conservative in both directions it can be: an out-of-range `resume_bci`
/// (including the `u32::MAX` identity-less re-run sentinel) falls back to
/// asking the question of the whole body, which is the historical rule.
pub(crate) fn replay_from_entry_is_observably_equivalent(
    code: &[u8],
    spliced_bodies_pure: bool,
    resume_bci: u32,
) -> bool {
    // The historical rule first (inside the shared predicate): a body that
    // commits nothing anywhere needs no reasoning about where the attempt
    // stopped, and answers `true` even for the `u32::MAX` sentinel.
    //
    // `code` is the frame's padded bytecode; the two trailing zero bytes are
    // `nop`s to every walk the predicate makes.
    replay_is_exact_after(code, None, spliced_bodies_pure, resume_bci, false)
}

/// [`replay_from_entry_is_observably_equivalent`] for an attempt of the
/// compiled body `ran` that stopped at `resume_bci` (a conditional point), for
/// a sink that holds that body but no stash: the KCFULL-13 re-run after an
/// escaped exception and the lambda direct arm's implicit exception, both in
/// `helpers.rs` (round 13 wave 12, lane replay6). The spliced-body half is
/// `ran`'s own (`false` without a body), and the extent follows the exception
/// edges `ran`'s local handlers can take
/// (`cratonvm_jit::deopt::local_handler_replay_edges`).
pub(crate) fn replay_from_entry_is_observably_equivalent_after(
    code: &[u8],
    ran: Option<&crate::jit::CompiledMethod>,
    resume_bci: u32,
) -> bool {
    let pure = ran.is_some_and(|r| r.spliced_bodies_side_effect_free);
    replay_is_exact_after(code, ran, pure, resume_bci, false)
}

/// The one VM call into the shared replay rule
/// (`cratonvm_jit::deopt::replay_from_entry_commits_nothing_with_handlers`),
/// with the exception edges of the body that ran (round 13 wave 12, lane
/// replay6, `r13w12-osrdoor-vm-sinks-replay-extent-ignores-handler-edges`).
///
/// A single-pass body with local handlers enters its own `catch` block without
/// leaving compiled code, so a handler placed at or before its throw is a way
/// back to the trap the branch-only extent does not see. The edges are the
/// body's own local-handler sites, not the method's whole table: those are
/// the only exception edges its frame can take, and the single-pass finalizer
/// judged the body with the whole table (a superset), so the sink is never
/// stricter than the check that installed the body. `ran` is `None`, or a body
/// without sites (every optimizing-tier body), exactly as before.
/// `CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES=0` ignores the edges here as at
/// compile time.
pub(in crate::runtime::interpreter::deopt_resume) fn replay_is_exact_after(
    code: &[u8],
    ran: Option<&crate::jit::CompiledMethod>,
    spliced_bodies_pure: bool,
    resume_bci: u32,
    first_arrival: bool,
) -> bool {
    let edges = ran.map_or_else(Vec::new, |r| {
        cratonvm_jit::deopt::local_handler_replay_edges(
            &r._jit_local_handler_sites,
            code,
            code.len(),
        )
    });
    cratonvm_jit::deopt::replay_from_entry_commits_nothing_with_handlers(
        code,
        code.len(),
        spliced_bodies_pure,
        resume_bci,
        first_arrival,
        &edges,
    )
}

/// [`replay_from_entry_is_observably_equivalent`] for the stashed frame
/// itself: the same rule, reading the frame's own `bci` and its
/// `ResumeSemantics::first_arrival` (round 12 wave 3, lane replay).
///
/// A sink holding the reconstructed frame asks this one, so an IR site trap
/// inside a loop (`for (..) { <indy>; counter++; }`, where the store is only
/// reachable through the trap) replays exactly instead of being refused by the
/// loop-closed extent, which must assume a conditional deopt.
///
/// `ran` is the body that fired the frame, whose local-handler edges the
/// extent follows (round 13 wave 12, lane replay6); `None` follows none.
pub(crate) fn replay_from_entry_is_observably_equivalent_for_frame(
    code: &[u8],
    spliced_bodies_pure: bool,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ran: Option<&crate::jit::CompiledMethod>,
) -> bool {
    replay_is_exact_after(
        code,
        ran,
        spliced_bodies_pure,
        rframe.bci,
        rframe.semantics.first_arrival,
    )
}

/// The replay question as a method-entry sink holding the stash has to ask
/// it: `code` is the bytecode of the method the sink is about to re-run,
/// `ran` the artifact that ran, `rframe` / `point_addr` the stash (round 12
/// wave 3, lane replay).
///
/// * A frame some OTHER body fired (`deopt_stash_is_from_artifact` false) says
///   nothing about where THIS attempt stopped: its bci is another method's.
///   Only the whole-body rule answers (the `u32::MAX` form). The lambda sink
///   used to pass that foreign bci as if it were the impl's own.
/// * A frame with no point address (a plain restash) keeps the pre-wave-3
///   question at its bci, without the `first_arrival` fact, which only a point
///   `ran` provably owns may assert.
/// * An inlined chain (`caller_frames` non-empty) names its INNERMOST scope's
///   bci, a pc of a spliced callee's bytecode, not of `code`: only the
///   whole-body rule answers for it too (interpreter round i1 wave 28, lane
///   L2). `helpers.rs`'s callee service already filtered chains out before
///   asking; the lambda sink and [`door_rerun_verdict`] did not.
/// * Otherwise [`replay_from_entry_is_observably_equivalent_for_frame`].
pub(crate) fn replay_from_entry_is_observably_equivalent_for_stash(
    code: &[u8],
    ran: &crate::jit::CompiledMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point_addr: usize,
) -> bool {
    let pure = ran.spliced_bodies_side_effect_free;
    if !deopt_stash_is_from_artifact(ran, point_addr) || !rframe.caller_frames.is_empty() {
        return replay_from_entry_is_observably_equivalent(code, pure, u32::MAX);
    }
    // Round 13 wave 12 (lane replay6): `ran` fired this frame, so the extent
    // follows the exception edges its local handlers can take.
    if point_addr == 0 {
        return replay_is_exact_after(code, Some(ran), pure, rframe.bci, false);
    }
    replay_from_entry_is_observably_equivalent_for_frame(code, pure, rframe, Some(ran))
}

/// The guard that fired for an ordinary stashed frame, when it can be named —
/// the one attribution the sinks holding the method's bytecode share (round 11
/// wave 2, r11-tier-deopt-stash-drops-the-trap-reason, VM half).
///
/// The stash carries a bci and no reason, so the reason is re-derived:
///
/// 1. The non-`RETHROW` points at the bci, with the loop-header `OsrExit` maps
///    skipped outside the OSR-exit test triggers (an `OsrExit` map is an exit
///    source only under those; a loop-header BCE guard trap used to be charged
///    as `OsrExit`, which neither evicts nor matches the guard's de-spec id).
///    Before round 11 wave 2 only the non-resume sinks applied this rule, so one
///    trap was charged under two reasons depending on which arm caught it.
/// 2. When those disagree and are exactly the optimizing tier's `NullCheck` +
///    `BoundsCheck` pair at an array access (`ir_lower`'s unelided
///    `xaload`/`xastore`), the frame itself says which fired: it is a
///    re-execute point, so the array reference is on its operand stack, and
///    the null check precedes the bounds check. A null reference is
///    `NullCheck`; any other is `BoundsCheck`. Before this, every such trap was
///    charged as `UncommonTrap` and four of them withdrew BOTH speculations
///    through the wildcard id.
/// 3. A bci whose only point is an `OsrExit` map answers that.
///
/// `None` means the guard cannot be named from here.
///
/// Interpreter round i1 wave 50, lane L1
/// (`interpreter-L1-proposal-one-cause-less-trap-reason-lookup-FIXED`): the rule lives in
/// ONE place, `cratonvm_jit::osr_exit::trap_reason_at_bci`, which the
/// frame-less sinks ask without a frame (`deopt_reason_at_bci`); this kept its
/// own copy of it until then.
pub(crate) fn trap_reason_at_frame(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<cratonvm_jit::deopt::DeoptReason> {
    match cratonvm_jit::osr_exit::trap_reason_at_bci(
        &compiled.deopt_points,
        rframe.bci,
        Some((code, &rframe.stack)),
    ) {
        cratonvm_jit::osr_exit::ReasonAtBci::Unique(r) => Some(r),
        _ => None,
    }
}

/// The guard that fired, named from the stash's own [`DeoptCause`] when it has
/// one, else re-derived by [`trap_reason_at_frame`] (round 11 wave 5,
/// r11w4-tier-vm-sinks-should-read-the-stashed-deopt-cause).
///
/// The cause is the reason and speculation id of the point the trampoline was
/// handed, so it is exact where the bci-based derivation answers `None` (two
/// guards at one non-array bci). It is believed only when THIS artifact has a
/// non-`RETHROW` point with that reason (and, for a non-zero id, that id) at
/// the frame's bci. A cause that describes some other artifact's point — a
/// foreign or leftover frame, an inlined scope's point — is ignored and the
/// old derivation runs, so a sink can never do worse than before.
///
/// The answered `speculation_id` is the point's own when it has one: that is
/// the id every compile-side consumer asks the de-spec registry about
/// (`p.speculation_id`), which the `(bci, reason)` derivation only equals by
/// convention. Otherwise it is [`cratonvm_jit::deopt::speculation_id`].
/// `CRATONVM_DEOPT_CAUSE_FROM_GUARD_BOXES` (nio21 wave 19b, lane L4) --
/// **default ON**, `=0` restores the earlier rule (only an `OsrExit` cause is
/// recognised in the boxes). Should [`trap_cause_at_frame`] believe a stash
/// cause that names a guard published in the artifact's `_deopt_point_boxes`
/// alone? An optimizing body's guards are published ONLY there; its
/// `deopt_points` table holds a plain `TransferToInterpreter` resume point at
/// the same bci, and the bci derivation then answered THAT -- an uncharged
/// reason, so a guard that fails on every call is never charged, evicted or
/// recompiled (`r21-treemap-comparator-rows-microseconds-per-op-FIXED-20261010.md`,
/// wave 19b: ~1 M traps per pass of `getEntryUsingComparator` / `put(K,V,Z)`).
/// The cause is still believed only when THIS artifact has a non-rethrow point
/// with that reason (and id) at the frame's bci. Read per trap, not per call.
pub(crate) fn cause_from_guard_boxes_enabled() -> bool {
    true
}

/// nio21 wave 20, lane L4: the reason of the deopt point `compiled` itself
/// published at `addr` (the address the trampoline stashed beside the frame,
/// `take_last_deopt_with_point`), searched in both homes the way
/// `CompiledMethod::owns_deopt_point` searches them. Addresses are only
/// compared, never dereferenced; `None` for 0 or an address this artifact does
/// not own.
pub(crate) fn nio21_fired_point_reason(
    compiled: &crate::jit::CompiledMethod,
    addr: usize,
) -> Option<cratonvm_jit::deopt::DeoptReason> {
    if addr == 0 {
        return None;
    }
    compiled
        ._deopt_point_boxes
        .iter()
        .map(|b| &**b)
        .chain(compiled.deopt_points.iter())
        .find(|p| *p as *const cratonvm_jit::deopt::DeoptimizationPoint as usize == addr)
        .map(|p| p.reason)
}

/// nio21 wave 20, lane L4 (under `CRATONVM_DEOPT_CAUSE_FROM_GUARD_BOXES`): the
/// cause-less twin of the guard-box rule. An attribution that came out as the
/// table's plain `TransferToInterpreter` resume point (uncharged) while the
/// point that actually FIRED, named by its address, is a guard of this artifact
/// with another reason, is charged under that guard's reason. A fired
/// `TransferToInterpreter` point (a genuine resume) keeps its attribution, and
/// so do the reasons that never escalate on their own terms (`OsrExit`, which
/// the interpreter-only-exit rule owns, and `PendingException`).
pub(crate) fn nio21_charge_reason_from_fired_point(
    attributed: cratonvm_jit::deopt::DeoptReason,
    fired: Option<cratonvm_jit::deopt::DeoptReason>,
) -> cratonvm_jit::deopt::DeoptReason {
    use cratonvm_jit::deopt::DeoptReason as R;
    match fired {
        Some(r)
            if attributed == R::TransferToInterpreter
                && !matches!(
                    r,
                    R::TransferToInterpreter | R::OsrExit | R::PendingException
                )
                && cause_from_guard_boxes_enabled() =>
        {
            r
        }
        _ => attributed,
    }
}

pub(crate) fn trap_cause_at_frame(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<cratonvm_jit::deopt::DeoptCause> {
    use cratonvm_jit::deopt::{speculation_id, DeoptCause, SPECULATION_ID_ANY};
    if rframe.bci == u32::MAX {
        return None;
    }
    if let Some(c) = cause {
        let names_it = |p: &cratonvm_jit::deopt::DeoptimizationPoint| {
            p.bci == rframe.bci
                && p.reason == c.reason
                && !p.semantics.rethrow_exception
                && (c.speculation_id == SPECULATION_ID_ANY || p.speculation_id == c.speculation_id)
        };
        // An optimizing body's back-edge poll exit (interpreter round i1
        // waves 15/18, `ir_lower::Lowerer::emit_poll_mode_exit`) is published
        // in its boxes only, never in `deopt_points`: recognised there, so the
        // sinks that charge by this attribution (the first-call door's
        // `despeculate_trapped_method`) see the `OsrExit` an agent's exit is
        // and decline it (`exit_left_for_the_interpreter`).
        //
        // nio21 wave 19b, lane L4 (`CRATONVM_DEOPT_CAUSE_FROM_GUARD_BOXES`):
        // so is every other GUARD of an optimizing body. Its point lives in
        // the boxes alone too; the table holds only the plain
        // `TransferToInterpreter` resume point at the same bci. Without this
        // the cause was ignored and the derivation answered that resume
        // point: a guard that fired on every call (`TreeMap.put(K,V,Z)` /
        // `getEntryUsingComparator` compiled under `CASE_INSENSITIVE_ORDER`
        // and then called with another comparator, ~1 M traps per pass) was
        // charged as `TransferToInterpreter` -- uncharged, never evicted,
        // never recompiled, forever.
        let recognised = compiled.deopt_points.iter().any(&names_it)
            || ((c.reason == cratonvm_jit::deopt::DeoptReason::OsrExit
                || cause_from_guard_boxes_enabled())
                && compiled._deopt_point_boxes.iter().any(|p| names_it(p)));
        if recognised {
            let id = if c.speculation_id == SPECULATION_ID_ANY {
                speculation_id(rframe.bci, c.reason)
            } else {
                c.speculation_id
            };
            return Some(DeoptCause {
                reason: c.reason,
                speculation_id: id,
            });
        }
    }
    trap_reason_at_frame(compiled, code, rframe).map(|reason| DeoptCause {
        reason,
        speculation_id: speculation_id(rframe.bci, reason),
    })
}

/// [`trap_cause_at_frame`]'s reason, with the `UncommonTrap` fallback
/// (`trap_reason_for_frame`'s rule) for a guard that cannot be named.
pub(crate) fn trap_reason_for_frame_with_cause(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> cratonvm_jit::deopt::DeoptReason {
    trap_cause_at_frame(compiled, code, rframe, cause)
        .map(|c| c.reason)
        .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap)
}

/// Did this trap come out of an artifact that is no longer the published body
/// of its method — i.e. the JIT cache currently holds a DIFFERENT body for it?
///
/// `DeoptimizationController::deoptimize` evicts by name, not by artifact. A
/// thread still running (or re-entering through a stale call-site cache) the
/// body a first trap already retired would otherwise evict the FRESH body that
/// trap's recompile published, and charge the ledger again for the same
/// speculation failure — N threads in the old body charge N times, and 20
/// inside one decay window reach `MakeNotCompilable`
/// (r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff).
///
/// An EMPTY cache slot answers `false`: the old body was evicted and nothing
/// replaced it yet, and charging then is the pre-existing behaviour (the eviction
/// is a no-op). Only the positive identity mismatch is treated as stale, so a
/// body the cache never held (a lookup under another class id, a hash
/// collision) keeps being charged exactly as before.
pub(crate) fn trapped_artifact_is_superseded(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    compiled: &crate::jit::CompiledMethod,
) -> bool {
    let live = shared.jit.jit_cache.read().get(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        cached.declaring_class_id,
    );
    match live {
        Some(live) => !std::ptr::eq(Arc::as_ptr(&live), compiled),
        None => false,
    }
}

/// jit-invokedynamic-groovy-regression fix — does the stashed reconstructed
/// frame belong to `cached`? The producer bakes the compiling method's
/// `"<class>.<method>:<descriptor>"` key into every snapshot
/// (`build_and_record_deopt_point`); a frame stashed by a NESTED compiled
/// callee (whose sentinel bubbled up through its compiled callers' epilogue
/// bails) carries THAT callee's key and must never be resumed as if it were
/// this method's. An empty key (legacy/test producer, or the superseded-guard
/// `bci == u32::MAX` sentinel frame) never matches.
pub(crate) fn deopt_frame_matches_method(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if rframe.method_key.is_empty() {
        return false;
    }
    let Some((rest, key_desc)) = rframe.method_key.rsplit_once(':') else {
        return false;
    };
    let Some((key_class, key_method)) = rest.rsplit_once('.') else {
        return false;
    };
    key_class == class_name && key_method == method_name && key_desc == descriptor}

/// The scope a stashed frame's IDENTITY is judged by at a sink that owns a
/// chain path: the OUTERMOST scope of an inlined chain — the method the
/// artifact belongs to, i.e. the one the door ran (`caller_frames` runs
/// innermost-first, so it is the `last()`) — and the frame itself otherwise.
/// Interpreter round i1 wave 26, lane L2
/// (`interpreter-L2-stash-identity-gates-name-the-innermost-scope-so-no-chain-resumes-FIXED-20260928`).
///
/// A chain's own `method_key` names the spliced callee that trapped, so a
/// gate that compares it with the sink's method read every chain as a nested
/// callee's stash, charged the innocent callee and re-ran the method from
/// entry: the chain paths behind those gates (`resume_real_ir_deopt`'s,
/// `transfer_osr_exit_into_live_frame`'s) were unreachable from a door. Only
/// a sink whose continuation materialises the chain may use this; one that
/// reads the frame's locals and bci as the sink method's own must keep
/// comparing the innermost name, which refuses a chain.
pub(crate) fn stash_identity_scope(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> &cratonvm_jit::deopt::ReconstructedFrame {
    rframe.caller_frames.last().unwrap_or(rframe)
}

/// Where a stashed frame's trap is charged against the sink's (outermost)
/// method: `(reason, bci)` in THAT method's bytecode space (interpreter round
/// i1 wave 26, lane L2). For a single-scope frame this is exactly the
/// attribution every sink shares — [`trap_reason_for_frame_with_cause`] at
/// `rframe.bci`. For an inlined chain the trap happened inside a spliced
/// callee, at a bci of the CALLEE's code: the charge goes to the call site in
/// the outer method (the outermost scope's bci), which is where the artifact
/// that speculated — and a recompile that stops speculating — lives, with the
/// stash's own reason (the bci-derived attribution would decode the callee's
/// bci in the outer method's bytecode) or `UncommonTrap`. HotSpot likewise
/// invalidates the outer nmethod; its per-bci trap history is kept in the
/// trapping scope's own profile, which this VM does not have per inline scope.
pub(crate) fn stash_charge_site(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> (cratonvm_jit::deopt::DeoptReason, u32) {
    match rframe.caller_frames.last() {
        Some(outer) => (
            cause.map_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap, |c| c.reason),
            outer.bci,
        ),
        None => (
            trap_reason_for_frame_with_cause(compiled, code, rframe, cause),
            rframe.bci,
        ),
    }
}

/// JIT round 16 (lane indy, IB-3): did `rframe` stop at one of `compiled`'s
/// own IR SITE TRAPS -- the unconditional `UncommonTrap` guard the IR builder
/// plants at a site it cannot lower (an `invokedynamic`), marked
/// `ResumeSemantics::first_arrival` by `ir_lower`?
///
/// # Why the sinks must ask this before charging
///
/// An optimizing body publishes its GUARD points in `_deopt_point_boxes`
/// only; `deopt_points` holds one plain `TransferToInterpreter` resume point
/// per safepoint snapshot. So both attributions the doors charge by
/// ([`trap_cause_at_frame`], [`stash_charge_site`]) found that plain point at
/// the trap's bci and charged `TransferToInterpreter`, whose policy is
/// `Reinterpret`: the body was never evicted, the site-trap memo
/// (`helpers::despeculate_trapped_method`) was never consulted, and the trap
/// fired on every call -- 157 971 deopts in 180 000 calls of
/// `R16IndyThrower.thrower` (page
/// `r16-indy-optimizing-tier-traps-at-every-live-invokedynamic-20261001`). A
/// `true` routes the stash through `despeculate_trapped_method`, which
/// applies the site-trap policy (IR ban for the method, recompile, per-bci
/// de-spec) exactly once per method and class version.
///
/// A site trap fires on EVERY arrival at its bci and nothing else stops
/// there (the guard is the first thing the bci emits), so a non-chain frame
/// at that bci of this artifact is the trap. A rethrow point is never one.
/// Kill switch `CRATONVM_JIT_SITE_TRAP_STASH_CHARGE=0` (default ON) restores
/// the old attribution; read only on a stash.
pub(crate) fn stash_is_own_ir_site_trap(
    compiled: &crate::jit::CompiledMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if rframe.bci == u32::MAX || !rframe.caller_frames.is_empty() {
        return false;
    }
    let is_site_trap = compiled._deopt_point_boxes.iter().any(|p| {
        p.bci == rframe.bci
            && p.reason == cratonvm_jit::deopt::DeoptReason::UncommonTrap
            && p.semantics.first_arrival
            && !p.semantics.rethrow_exception
    });
    is_site_trap
}

/// Round 14 wave 3 (lane resume; proposal CH3-4 of `jit-r13-chain3-proposals-RETIRED-20260929.md`):
/// charge an optimizing OSR body's CHAIN guard exit -- a guard inside a
/// spliced callee, left through the planless guard exit's in-place chain
/// transfer (admitted since round 13 wave 8) -- as the doors charge a chain
/// trap ([`stash_charge_site`] and the chain arm of
/// [`real_frame_deopt_resume_or_throw_and_despeculate`]): against the OSR'd
/// method at its OUTERMOST scope's bci (the call site the artifact spliced),
/// under the stash's own reason, and after `PER_BCI_DESPEC_LIMIT` such
/// charges at that site the wildcard per-bci de-spec there. Until now
/// `jit_bridge::charge_osr_guard_exit` declined every chain, so such an exit
/// was never recorded at all, however often it recurred.
///
/// Held to the two reasons the flat OSR charge answers (`BoundsCheck`,
/// `ReceiverTypeChanged`) and to a stash cause (a chain's bci is a callee's,
/// so nothing can be read from the OSR'd method's own points). Returns whether
/// the exit was charged. The caller must NOT treat a charged chain exit as a
/// committed OSR entry: the de-spec at the call site does not withdraw the
/// spliced guard itself, so the per-pc rejection budget stays its brake.
/// Kill switch `CRATONVM_JIT_OSR_CHAIN_GUARD_EXIT_CHARGE` (default ON; read
/// only on such an exit).
pub(crate) fn charge_osr_chain_guard_exit(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> bool {
    use cratonvm_jit::deopt::DeoptReason;
    let Some(outer) = rframe.caller_frames.last() else {
        return false;
    };
    let Some(reason) = cause.map(|c| c.reason).filter(|r| {
        matches!(
            r,
            DeoptReason::BoundsCheck
                | DeoptReason::ReceiverTypeChanged
                | DeoptReason::UncommonTrap
                | DeoptReason::UnstableIf
        )    }) else {
        return false;
    };
    if rframe.bci == u32::MAX
        || outer.bci == u32::MAX
    {
        return false;
    }
    // JIT round 19 wave 3 (lane splice3, SPLICE2-2): an expected throw's
    // trip is not charged (`splice_expected_throw_trip`).
    if splice_expected_throw_trip(rframe) {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] OSR chain guard exit at an expected throw: \
                 {class_name}.{method_name}{descriptor} call-site bci={} (innermost {} bci={}) \
                 -- not charged",
                outer.bci, rframe.method_key, rframe.bci
            );
        }
        return false;
    }
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR chain guard exit charged: {class_name}.{method_name}{descriptor} \
             reason={reason:?} call-site bci={} ({}-frame chain, innermost {} bci={})",
            outer.bci,
            rframe.caller_frames.len() + 1,
            rframe.method_key,
            rframe.bci
        );
    }
    let method_key = format!("{class_name}.{method_name}:{descriptor}");
    if matches!(reason, DeoptReason::UncommonTrap | DeoptReason::UnstableIf) {
        let spec_id = if !rframe.method_key.is_empty() {
            cratonvm_jit::deopt::chain_predicate_speculation_id(
                &rframe.method_key,
                rframe.bci,
                cratonvm_jit::deopt::PredicateKind::UnstableIf,
            )
        } else {
            cratonvm_jit::deopt::SPECULATION_ID_ANY
        };
        if !shared
            .jit
            .despec_registry
            .insert_if_absent(&method_key, outer.bci, spec_id)
        {
            return true;
        }
        let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
            shared,
            class_id,
            class_name,
            method_name,
            descriptor,
            reason,
            outer.bci,
        );
        return true;
    }
    if shared.jit.despec_registry.contains_speculation(
        &method_key,
        outer.bci,
        cratonvm_jit::deopt::SPECULATION_ID_ANY,
    ) {
        return true;
    }
    let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
        shared,
        class_id,
        class_name,
        method_name,
        descriptor,
        reason,
        outer.bci,
    );
    // The doors' limit and grain: counted at `(reason, call site)`, withdrawn
    // with the wildcard (the call site speculated whatever it spliced).
    {
        let _ = despec_chain_site_after_trap_limit(
            shared,
            &method_key,
            rframe,
            outer.bci,
            Some(reason),
            cratonvm_jit::deopt::SPECULATION_ID_ANY,
        );
    }
    true
}

/// JIT round 19 wave 3 (lane splice3, SPLICE2-2 = TF15-1): is `rframe` the
/// trip of a spliced guard the compile marked as an EXPECTED throw
/// (`ResumeSemantics::expected_throw`, `ir_lower::emit_deopt_unless`), under
/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW`? Such a trip is resumed like any
/// chain trap (the interpreter re-executes the callee's bytecode and throws)
/// but neither charged nor de-spec'd: the body and its splice stay. Only a
/// chain (a non-empty `caller_frames`) qualifies.
pub(crate) fn splice_expected_throw_trip(rframe: &cratonvm_jit::deopt::ReconstructedFrame) -> bool {
    rframe.semantics.expected_throw
        && !rframe.caller_frames.is_empty()
        && rframe.bci != u32::MAX
        && cratonvm_jit::ir::ir_splice_expected_throw_enabled()
}

/// JIT round 19 wave 4 (lane splice4, SPLICE3-3): [`splice_expected_throw_trip`]
/// with the trip rate limit of `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_RATE_LIMIT`.
/// Switch off: exactly [`splice_expected_throw_trip`]. On: a marked trip is
/// counted on `compiled` (the body that fired it), and stays uncharged until
/// `ir::expected_throw_trip_rate_exceeded` says the body throws there on too
/// many of its entries; past that it is charged as an unmarked chain trap
/// (whose next escalation, the named entry already present, records the
/// call-site wildcard: [`despec_chain_site_after_trap_limit`]).
pub(crate) fn expected_throw_trip_uncharged(
    compiled: &crate::jit::CompiledMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    use std::sync::atomic::Ordering;
    if !splice_expected_throw_trip(rframe) {
        return false;
    }
    if !cratonvm_jit::ir::ir_splice_expected_throw_rate_limit_enabled() {
        return true;
    }
    let trips = compiled
        .splice_expected_throw_trips
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
            Some(t.saturating_add(1))
        })
        .map_or(u32::MAX, |prev| prev.saturating_add(1));
    // JIT round 20 (lane throw,
    // `r19-expected-throw-rate-limit-has-no-denominator-in-optimizing-bodies-FIXED-20261012.md`):
    // only a body with an armed entry counter has a denominator. Optimizing
    // bodies (the only ones that carry a marked point) are built without one,
    // so `entry_count()` read `0` and the rule charged every trip past the
    // floor however rarely the site threw. No counter: cannot judge, keep the
    // mark (the switch is a no-op there until such bodies count entries).
    let entries = compiled._jit_entry_counter.as_ref().map(|c| c.get());
    let exceeded = entries
        .is_some_and(|entries| cratonvm_jit::ir::expected_throw_trip_rate_exceeded(trips, entries));
    if exceeded && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] expected-throw rate exceeded: {} bci={} ({trips} marked trips over {} \
             entries) -- charged",
            rframe.method_key,
            rframe.bci,
            compiled.entry_count()
        );
    }
    !exceeded
}

/// JIT round 19 wave 4 (lane splice4, SPLICE3-2): the stash sinks outside
/// the door sink ask this before charging a stashed frame -- the
/// trapped-callee helper sink (`helpers::try_resume_trapped_callee`) and the
/// first-call tier-up sink (`interpreter.rs`). `true` only under
/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS` (with the base switch)
/// for a marked trip [`expected_throw_trip_uncharged`] leaves uncharged;
/// `compiled` is the body that fired it (`None`: the rate limit, when on,
/// cannot count it, and the trip is charged).
pub(crate) fn stash_sink_expected_throw_uncharged(
    compiled: Option<&crate::jit::CompiledMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if !cratonvm_jit::ir::ir_splice_expected_throw_stash_sinks_enabled()
        || !splice_expected_throw_trip(rframe)
    {
        return false;
    }
    match compiled {
        Some(compiled) => expected_throw_trip_uncharged(compiled, rframe),
        None => !cratonvm_jit::ir::ir_splice_expected_throw_rate_limit_enabled(),
    }
}

/// [`despec_site_after_trap_limit`] for a CHAIN trap charged at its call site
/// `bci` in `method_key` (the outermost scope), additively under
/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW` (JIT round 19 wave 3, lane splice3,
/// SPLICE2-2 = TF15-1). Switch off, a single-scope frame, or a reason that is
/// not an implicit exception (`deopt::is_expected_throw_reason`): exactly
/// [`despec_site_after_trap_limit`].
///
/// On, once the site reaches the same limit at the same grain, the FIRST
/// escalation records the named innermost entry `(rframe.method_key,
/// rframe.bci, expected_throw_speculation_id(rframe.bci, reason))` INSTEAD of
/// `speculation` at the call site: the planner keeps the splice (no wildcard
/// withdraws it, `ir_splice_site_withdrawn`) and the recompile marks the
/// guard an expected throw. A later escalation at the site, with that entry
/// already present (the recompile did not mark the guard -- its opcode does
/// not throw for the reason, or no chain described it), records `speculation`
/// as before, so a site that keeps trapping still loses its splice, one
/// recompile later than today.
pub(crate) fn despec_chain_site_after_trap_limit(
    shared: &SharedVm,
    method_key: &str,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    bci: u32,
    counted: Option<cratonvm_jit::deopt::DeoptReason>,
    speculation: u32,
) -> Option<usize> {
    let named = counted.filter(|&reason| {
        cratonvm_jit::deopt::is_expected_throw_reason(reason)
            && !rframe.caller_frames.is_empty()
            && rframe.bci != u32::MAX
            && !rframe.method_key.is_empty()
            && bci != u32::MAX
            && cratonvm_jit::ir::ir_splice_expected_throw_enabled()
    });
    let Some(reason) = named else {
        return despec_site_after_trap_limit(shared, method_key, bci, counted, speculation);
    };
    let id = cratonvm_jit::deopt::expected_throw_speculation_id(rframe.bci, reason);
    let already = shared
        .jit
        .despec_registry
        .entries_for(&rframe.method_key)
        .contains(&(rframe.bci, id));
    if already {
        return despec_site_after_trap_limit(shared, method_key, bci, counted, speculation);
    }
    let charged = shared
        .jit
        .deopt_log
        .lock()
        .deopt_count_at_site(method_key, reason, bci);
    if charged < PER_BCI_DESPEC_LIMIT {
        return None;
    }
    shared
        .jit
        .despec_registry
        .insert(&rframe.method_key, rframe.bci, id);
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] expected-throw de-spec: {method_key} call-site bci={bci} \
             ({charged} {reason:?} chain traps >= {PER_BCI_DESPEC_LIMIT}) -> named \
             {} bci={} (splice kept, guard marked on the next compile)",
            rframe.method_key, rframe.bci
        );
    }
    Some(charged)
}

/// Charged traps at one `(method, bci)` -- at `(reason, bci)` for a named
/// speculation -- before that speculation is withdrawn from the method's next
/// compile. HotSpot's `PerBytecodeTrapLimit`.
pub(crate) const PER_BCI_DESPEC_LIMIT: usize = 4;

/// Round 14 wave 4 (lane resume2; proposal RS-3 of `jit-r14-resume-proposals-RETIRED-20260930.md`):
/// the per-bci de-spec rule, in one copy. Once `method_key` has been charged
/// [`PER_BCI_DESPEC_LIMIT`] times at `bci` -- counted at `(reason, bci)` when
/// `counted` names the reason, else every reason at the bci -- `speculation`
/// is recorded in this VM's de-spec registry at `bci`, so the next compile of
/// the method drops that speculative guard (or, for
/// `SPECULATION_ID_ANY`, everything it speculated there) and the method stays
/// compiled. Returns the count when it withdrew. Nothing for the
/// superseded-guard sentinel (`bci == u32::MAX`). Call it AFTER the charge
/// (the log's lock is taken here; the charge must have released it).
///
/// The callers keep the two choices that are deliberately theirs: which
/// reason is counted (a named speculation's own, a chain's stash reason at
/// the call site, or the whole bci for a wildcard) and which id is withdrawn.
/// The rule itself -- the limit, the grain, the sentinel, the insert -- was
/// written out three times (the doors, the call-site service's
/// `helpers::despeculate_trapped_method`, the OSR chain charge) and had
/// drifted once (round 11 wave 4: one copy summed every reason at the bci).
pub(crate) fn despec_site_after_trap_limit(
    shared: &SharedVm,
    method_key: &str,
    bci: u32,
    counted: Option<cratonvm_jit::deopt::DeoptReason>,
    speculation: u32,
) -> Option<usize> {
    if bci == u32::MAX {
        return None;
    }
    crate::runtime::frozen_pages::note_method_trapped(shared, method_key, bci);
    let charged = {
        let log = shared.jit.deopt_log.lock();
        match counted {
            Some(reason) => log.deopt_count_at_site(method_key, reason, bci),
            None => log.deopt_count_at_bci(method_key, bci),
        }
    };
    if charged < per_bci_despec_limit(counted, speculation) {
        return None;
    }
    shared
        .jit
        .despec_registry
        .insert(method_key, bci, speculation);
    // j27 wave 8 (lane W8ARG): every path's withdrawal, for the
    // `CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2` triage (a method that bails
    // at the per-method cutoff with no such line is the wildcard case).
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] per-bci de-spec withdrawn: {method_key} bci={bci} \
             speculation={speculation:#x} charged={charged}"
        );
    }
    Some(charged)
}

/// JIT r20 wave 11, lane pProf (W17L4-5 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the per-bci
/// limit [`despec_site_after_trap_limit`] applies. A never-taken prune's own
/// reason (`DeoptReason::UnstableIf`, stamped only under
/// `CRATONVM_JIT_PROF_UNSTABLE_IF_REASON`) is withdrawn after ONE charge --
/// HotSpot's `Reason_unstable_if` recompiles with the branch kept at once --
/// so the first real taken edge costs one deopt and one recompile instead of
/// [`PER_BCI_DESPEC_LIMIT`]. Every other reason, and the wildcard, keep the
/// limit.
///
/// The r23 L8 heap-buffer close: a deopting compactness guard's NAMED
/// speculation (`SpeculationFailed` under its own flat or chain id, never the
/// wildcard, which withdraws a whole splice) is withdrawn after one charge
/// too, under `CRATONVM_JIT_PROVEN_COMPACT_ONE_TRAP`
/// (`cratonvm_jit::deopt::proven_compact_one_trap_enabled`): a legacy object
/// never becomes compact, so the next three traps there could only repeat
/// the first.
pub(crate) fn per_bci_despec_limit(
    counted: Option<cratonvm_jit::deopt::DeoptReason>,
    speculation: u32,
) -> usize {
    match counted {
        Some(cratonvm_jit::deopt::DeoptReason::UnstableIf) => 1,
        Some(cratonvm_jit::deopt::DeoptReason::SpeculationFailed)
            if speculation != cratonvm_jit::deopt::SPECULATION_ID_ANY
                && cratonvm_jit::deopt::proven_compact_one_trap_enabled() =>
        {
            1
        }
        _ => PER_BCI_DESPEC_LIMIT,
    }
}

/// Round nio20 wave 4 (lane L3, W3L4-4 of `jit-r20-nio-proposals-w3-L4.md`):
/// the named speculation id a chain trap out of `compiled` is charged under at
/// its call site, when the trap is a NESTED trap guard
/// (`CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP`, interface and nest-exact
/// virtual rows): an `UncommonTrap` whose
/// innermost scope stopped at its own `invokevirtual` / `invokeinterface`,
/// read from the body the compile spliced (`CompiledMethod::splice_scope_sources`).
/// The id is [`cratonvm_jit::nested_trap_speculation_id`] of that scope's key
/// and bci, which the IR planner derives for the same nested row
/// (`ir_splice_strip_withdrawn_nested_traps`) and drops alone.
///
/// `None` (the wildcard, as before) for anything else: another reason, a
/// trap at a non-invoke bci (a bounds or null guard of the callee body, whose
/// withdrawal must take the whole splice), no retained body, or
/// `CRATONVM_JIT_IR_NESTED_TRAP_OWN_CHARGE=0`.
pub(in crate::runtime::interpreter::deopt_resume) fn nested_trap_charge_id(
    compiled: &crate::jit::CompiledMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<u32> {
    if rframe.caller_frames.is_empty()
        || rframe.bci == u32::MAX
        || cause?.reason != cratonvm_jit::deopt::DeoptReason::UncommonTrap
        || !cratonvm_jit::ir_nested_trap_own_charge_enabled()
    {
        return None;
    }
    let source = compiled
        .splice_scope_sources
        .iter()
        .find(|row| row.method_key == rframe.method_key)?;
    let op = *source.code.get(usize::try_from(rframe.bci).ok()?)?;
    if !matches!(op, 0xb6 | 0xb9) {
        return None;
    }
    Some(cratonvm_jit::nested_trap_speculation_id(
        &rframe.method_key,
        rframe.bci,
    ))
}

/// JIT round 23 wave 4 (lane L1): the named id a CHAIN trap is charged under
/// at its outermost call site when it is a loop-entry predicate of a loop
/// spliced from a callee -- `ir_lower` gave that guard's point
/// `cratonvm_jit::deopt::chain_predicate_speculation_id` of its innermost
/// scope (`rframe.method_key`, at its header `rframe.bci`), and the planter
/// asks for exactly that id at the call site
/// (`cratonvm_jit::deopt::chain_predicate_withdrawn`). `None` for a flat frame,
/// another reason, or any other id (the wildcard path then stands, as before).
pub(crate) fn chain_predicate_charge_id(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<u32> {
    let c = cause?;
    if rframe.caller_frames.is_empty()
        || rframe.bci == u32::MAX
        || c.reason != cratonvm_jit::deopt::DeoptReason::UncommonTrap
        || !cratonvm_jit::deopt::is_chain_predicate_speculation_id(
            &rframe.method_key,
            rframe.bci,
            c.speculation_id,
        )
    {
        return None;
    }
    Some(c.speculation_id)
}

/// JIT round 25 (item 4 residual 1, spliced withdrawal): the named id a CHAIN
/// trap is charged under at its outermost call site when it is the deopting
/// compactness guard of a spliced field site -- `ir_lower`'s
/// `emit_deopt_unless_as` gave that point
/// `cratonvm_jit::deopt::chain_compact_speculation_id` of its innermost scope
/// at its own bci, and the next compile asks for exactly that id at the call
/// site (`cratonvm_jit::deopt::chain_compact_withdrawn`), keeping the splice
/// and withdrawing that one guard. `None` for a flat frame, another reason or
/// any other id (the wildcard path then stands, as before).
pub(crate) fn chain_compact_charge_id(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<u32> {
    let c = cause?;
    if rframe.caller_frames.is_empty()
        || rframe.bci == u32::MAX
        || c.reason != cratonvm_jit::deopt::DeoptReason::SpeculationFailed
        || !cratonvm_jit::deopt::is_chain_compact_speculation_id(
            &rframe.method_key,
            rframe.bci,
            c.speculation_id,
        )
    {
        return None;
    }
    Some(c.speculation_id)
}

/// The named id a CHAIN trap of a guard that one spliced site owns is charged
/// under at its call site: a spliced loop's entry predicate
/// ([`chain_predicate_charge_id`]) or a spliced field site's compactness
/// guard ([`chain_compact_charge_id`]). `None`: the wildcard.
pub(crate) fn chain_named_charge_id(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<u32> {
    chain_predicate_charge_id(rframe, cause).or_else(|| chain_compact_charge_id(rframe, cause))
}

