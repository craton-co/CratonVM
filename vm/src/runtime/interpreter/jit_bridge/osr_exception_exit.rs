#[allow(unused_imports)]
use super::*;

/// Where an exception raised inside an OSR'd body must go.
///
/// See [`route_osr_exception_out_of_artifact`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::runtime::interpreter) enum OsrExceptionExit {
    /// The live interpreter frame now sits at one of this method's handlers,
    /// with the throwable on its operand stack. Keep interpreting it.
    EnteredHandler,
    /// The exception escapes this method. Hand THIS throwable to `throw_out`:
    /// the handler search may have replaced the one passed in (a catch type
    /// that fails to resolve, JVMS §5.4.3) or moved it.
    Propagate(ObjectRef),
}

/// Route an exception raised inside an OSR'd body out of the artifact —
/// through this method's own exception table when it can catch, and out of the
/// frame when it cannot.
///
/// **This is the consumer half of the RBC.6b lift.** Before 2026-08-17 the OSR
/// door refused every method with an exception table, so every exception
/// reaching an OSR bail was by construction one this frame could not catch, and
/// the four drains in `try_osr` could simply propagate. Now that such methods
/// compile, the frame CAN catch — and the one thing it must not do is what the
/// drains used to do for a `NullPointerException`: search the table at
/// `entry_pc`. `entry_pc` is the back-edge the OSR'd body was ENTERED at, which
/// has nothing to do with where the throw happened, and the live frame's locals
/// are the stale pre-OSR ones the compiled code never advanced. Entering a
/// handler on those is a silent wrong answer.
///
/// The precise answer is the reason-9 (`DeoptReason::PendingException`) frame
/// the compiled body publishes at every throwing site inside a protected range:
/// it carries the THROWING bci and the live locals. `compile_osr_artifact`
/// admits a method with an exception table only when every such site publishes
/// one (`first_unsupported_precise_frame_site`), which is what makes the two
/// deductions below sound:
///
///  * a frame IS stashed ⇒ its bci is the exact throw site; range-test the
///    table against it;
///  * NO frame is stashed ⇒ the throw site lies outside every protected range
///    of this method, i.e. this method saying it cannot catch. Propagate. This
///    is the same reasoning `route_jit_signal_exception` encodes as
///    `JitThrowPc::OutsideAllRanges` on the method-entry path.
///
/// A stashed frame naming a *callee* is re-stashed untouched: its owner's own
/// drain routes it, and dropping it there made a compiled callee's handler read
/// its non-parameter locals as null once already (see
/// `drop_own_exceptional_frame`).
///
/// ... unless `entry_stash_floor` is `Some` (gen r5w1/oom5, opt-in
/// `CRATONVM_JIT_OSR_DROP_ORPHANS`; item 2 of
/// `docs/feature-designs/jit-ir-optimizations.md`).
/// It is the exceptional-stash depth `try_osr` recorded just before it entered
/// the artifact. When this runs, the OSR body has RETURNED: every compiled
/// frame that ran inside the activation has unwound, and a compiled frame
/// publishes only on its way out, so a frame stashed at or above the floor was
/// published by an owner that is gone -- nothing can consume it correctly
/// again, and every object it names stays a GC root until the next take on
/// this thread. A frame an outer sink had stashed before the entry (the case
/// the re-stash exists for: a sink that publishes its own frame, then runs
/// Java that reaches this OSR'd loop) is below the floor and survives. A take
/// inside the activation can only lower what sits above the floor, so the rule
/// under-drops, never over-drops -- the argument of
/// `cratonvm_jit::deopt::truncate_exceptional_stash_to`, which this uses.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn route_osr_exception_out_of_artifact(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    exc: ObjectRef,
    entry_stash_floor: Option<usize>,
) -> OsrExceptionExit {
    use super::deopt_resume::CompiledLocksOfAStash as StashLocks;
    let trace = dbg_deopt_enabled();
    // JIT round 17 wave 4, lane stashdrop: a frame this body published is
    // never released by the drops below (the live frame's own record may
    // still account for its holds); every other frame they drop belongs to an
    // activation that has unwound, and is released under the exceptional
    // hand-over's switches (`CompiledLocksOfAStash::dropped_frame_holds_released`).
    let keep_own = |_: &cratonvm_jit::deopt::ReconstructedFrame, point: usize| {
        super::deopt_resume::deopt_stash_is_from_artifact(compiled, point)
    };
    // Matched by body as well as by name (interpreter round i1 wave 11): a
    // same-named frame whose point is not `compiled`'s was published by
    // another body and is put back, point and all, like any foreign frame.
    //
    // An exception thrown inside bodies the optimizing tier SPLICED into this
    // artifact is published as an inlined chain: the frame names the innermost
    // callee, and its `caller_frames` end with this method's own scope at the
    // invoke the exception left through. Judged by the innermost name it read as
    // a foreign callee's frame, was put back, and the exception escaped a
    // `try`/`catch (Throwable)` that covers the call (`CatchEscape`: a hot
    // `for (;;) { exec.execute(task); }` loop, OSR-compiled, whose callee chain
    // throws). The method's own handler search needs that outermost scope: its
    // invoke bci is the throw site as far as this method is concerned, and its
    // locals are the ones the handler reads.
    // The take's discards are released (lane stashdrop, site 1).
    let taken = StashLocks::take_exceptional_frame_releasing_discards(
        shared,
        thread,
        &keep_own,
        "OSR exceptional sink",
    );
    let precise = match taken {
        Some((rframe, _, point_addr))
            if super::deopt_resume::deopt_frame_matches_method(
                super::deopt_resume::stash_identity_scope(&rframe),
                class_name,
                method_name,
                method_descriptor,
            ) && super::deopt_resume::deopt_stash_is_from_artifact(compiled, point_addr) =>
        {
            let point_bci = rframe.bci;
            Some(match rframe.caller_frames.last() {
                Some(outer) => (outer.clone(), point_bci),
                None => (rframe, point_bci),
            })
        }
        Some((foreign, cause, point_addr)) => {
            if trace {
                eprintln!(
                    "[cratonvm-deopt] OSR exception route in {class_name}.{method_name}: stashed frame {} bci={} chain={} point_from_artifact={} is foreign; putting it back",
                    foreign.method_key,
                    foreign.bci,
                    foreign.caller_frames.len(),
                    super::deopt_resume::deopt_stash_is_from_artifact(compiled, point_addr)
                );
            }
            cratonvm_jit::deopt::restash_exceptional_frame_with_point(foreign, cause, point_addr);
            None
        }
        None => None,
    };
    if let Some(floor) = entry_stash_floor {
        // Lane stashdrop (site 2): the orphans' holds are released.
        let dropped = StashLocks::truncate_exceptional_stash_releasing(
            shared,
            thread,
            floor,
            &keep_own,
            "OSR exceptional sink",
        );
        if dropped > 0 && (trace || crate::jit::helpers::oldmark_census_on()) {
            eprintln!(
                "[oldmark-root-census] OSR sink {class_name}.{method_name}{method_descriptor}: \
                 dropped {dropped} orphaned exceptional frame(s) above the entry floor {floor}"
            );
        }
    }
    // Interpreter round i1 wave 24, lane L2: the locks this activation holds
    // as its own frame names them (`relock == false`: the ones the compiled
    // code took, and the ones the interpreter took before the OSR entry, which
    // the entry contract lists the same way), pinned until the route decides.
    // Entering the handler keeps them with the live frame, whose catch-all
    // releases them. Every `Propagate` below unwinds this frame past its
    // handler (only hand-written bytecode, or a refusal admission makes
    // unreachable, gets there holding a lock: javac's catch-all covers every
    // throwing site inside its block), so they are released there, as the
    // catch-all would have done; each release also retires a matching entry
    // of the live frame's `held_monitors` record (a lock the interpreter took
    // before the entry), so the unwind cannot release it a second time.
    let compiled_locks = std::cell::Cell::new(precise.as_ref().map(|(rframe, _)| {
        super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks(thread, rframe)
    }));
    let propagate = |thread: &mut JvmThread, exc: ObjectRef| -> OsrExceptionExit {
        if let Some(locks) = compiled_locks.take() {
            locks.release_for_a_propagation_of_live_frame(shared, thread, frame_idx);
        }
        OsrExceptionExit::Propagate(exc)
    };

    if thread.frames[frame_idx].exception_table().is_empty() {
        // The pre-lift population, and still the overwhelming majority: no
        // handler can exist here, so there is nothing to route.
        return propagate(thread, exc);
    }

    let Some((mut rframe, point_bci)) = precise else {
        if trace {
            eprintln!(
                "[cratonvm-deopt] OSR exception with no precise frame in {class_name}.\
                 {method_name}{method_descriptor} — throw site is outside every protected \
                 range; propagating (artifact rethrow points={}, stash depth={})",
                compiled
                    .deopt_points
                    .iter()
                    .filter(|p| p.semantics.rethrow_exception)
                    .count(),
                cratonvm_jit::deopt::exceptional_stash_depth()
            );
        }
        return OsrExceptionExit::Propagate(exc);
    };
    let throw_pc = rframe.bci as usize;
    // Pinned across the search, which may replace the throwable with a catch
    // type's resolution error and may collect (a catch type loaded through a
    // user loader, the error materialized). `rframe`'s raw object addresses
    // are pinned with it and re-read afterwards — the pin/re-read the deopt
    // materializer uses — so the search runs with `may_collect: true`. (Until
    // wave 4 it could not re-read them and searched with `false`, reading an
    // unresolvable catch type as "no match".)
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let frame_base = crate::runtime::deopt_materialize::pin_frame_object_refs(thread, &rframe);
    let handler =
        find_exception_handler_any_pc(shared, thread, frame_idx, throw_pc, pin_base, true);
    crate::runtime::deopt_materialize::reread_frame_object_refs(thread, &mut rframe, frame_base);
    let exc_ref = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    let Some(handler_pc) = handler else {
        if trace {
            eprintln!(
                "[cratonvm-deopt] OSR exception at bci={throw_pc} in {class_name}.\
                 {method_name}{method_descriptor} — no handler covers it; propagating"
            );
        }
        // Reported here, at the precise throw bci, rather than by the unwinder
        // at the frame's stale pc (`OSR_FRAME_DECLINED_TO_CATCH`), which then
        // finds it already reported (wave 21, L2). Pins across its callbacks.
        let exc_ref =
            super::deliver_exception_event_if_armed(shared, thread, frame_idx, exc_ref, throw_pc);
        return propagate(thread, exc_ref);
    };

    let mut heap_exhausted = false;
    match super::deopt_resume::transfer_osr_exception_exit_into_live_frame(
        shared,
        thread,
        frame_idx,
        &rframe,
        compiled,
        handler_pc,
        exc_ref,
        point_bci,
        &mut heap_exhausted,
    ) {
        Ok(()) => {
            // The engagement counter for the whole lift. `osr_entered` says an
            // artifact was entered; only this says an exception raised inside
            // one was routed through the method's own handler — the thing
            // RBC.6b refused to allow at all. Read it beside `osr_entered` and
            // `osr_exited`: a run with `osr_entered` climbing and this at zero
            // is a loop whose `catch` never fires, not a working lift.
            cratonvm_jit::metrics::record_osr_event("osr_exception_handler_entered");
            // Round 16 (lane osrdoor): an optimizing OSR body leaves on every
            // exception its loop catches (its graph has no handler); a burst of
            // such exits hands the loop to the single-pass OSR body, whose
            // local handler stubs catch in compiled code.
            note_osr_optimizing_handler_exit(shared, compiled);
            // `Exception` at the precise throw bci (wave 21, L2; the live
            // frame now roots the throwable), then `ExceptionCatch`, then the
            // caught exception is no longer "reported" (wave 11, L1).
            super::report_exception_caught_by_compiled_door(
                shared, thread, frame_idx, throw_pc, handler_pc,
            );
            // gen r4w5/oomjit5: the handler frame now holds the throwable, and
            // no interpreted throw path (`settle_thrown_exception`) will run
            // for this exception — so drop a leftover native return here, as
            // that path would have.
            let _ = crate::jit::helpers::drain_native_return_at_compiled_catch(
                thread,
                "OSR exception handler",
            );
            // The live frame, parked at its handler, keeps the locks, and its
            // `held_monitors` record now names exactly them (interpreter round
            // i1 wave 25, lane L2): the reason-9 frame describes every lock
            // the activation holds at the throw, the pre-entry ones included,
            // so javac's catch-all releases each through the record. A
            // single-pass body's frame describes them too since wave 26 where
            // its monitor analysis answers; one that describes none leaves the
            // record empty, as `try_osr_with_backoff` used to for every
            // handler entry.
            match compiled_locks.take() {
                Some(locks) => locks.replace_live_record_and_unpin(thread, frame_idx),
                // Wave 117 (lane L6): no description at all -- marked
                // understated where the frame can hold a block monitor
                // (`held_monitors::clear_untrusted_record`).
                None => crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                    &mut thread.frames[frame_idx],
                    crate::runtime::interpreter::held_monitors::RecordProducer::OsrExceptionNoStash,
                ),
            }
            OsrExceptionExit::EnteredHandler
        }
        // Unreachable by admission: `validate_osr_entry`'s `osr_exit_policy`
        // walks EVERY deopt point of the artifact — reason-9 ones included —
        // and refuses the entry outright when one reconstructs an unresumable
        // frame, so an artifact that was entered cannot publish one here.
        // Propagating rather than entering the handler on locals we could not
        // map is the fail-closed direction: a visibly uncaught exception beats
        // a handler silently running on stale values.
        Err(why) => {
            if trace {
                eprintln!(
                    "[cratonvm-deopt] OSR exception-exit transfer refused ({why}) at bci={} \
                     in {class_name}.{method_name}{method_descriptor} — propagating",
                    rframe.bci
                );
            }
            // OSR round (2026-10-06): re-allocating the frame's scalar-replaced
            // locals found the heap exhausted. The activation is abandoned with
            // `OutOfMemoryError`, as HotSpot pops the frames of a failed
            // reallocation; the original throwable is dropped with it.
            if heap_exhausted {
                if let MethodCallFailed::ExceptionThrown(oom) =
                    super::deopt_resume::rematerialisation_out_of_memory(shared, thread)
                {
                    return propagate(thread, oom);
                }
            }
            propagate(thread, exc_ref)
        }
    }
}

/// Let the OSR door reach the OPTIMIZING tier -- **default ON** since
/// 2026-09-05; `CRATONVM_JIT_OSR_OPTIMIZING=0` is the kill switch.
///
/// This door has always reached `x64::compile_with_param_slots` directly, so a
/// method whose only route to compiled code is a back edge -- one big method
/// that is one big loop -- has never had an optimizing body, whatever the tier
/// settings said. `CRATONVM_DBG=ir-linear-scan` measured what that costs: on
/// CratonBench, `arithmetic`, `hashmap`, `sieve` and `matrix` get no optimizing
/// body at all, while six of seven methods compiled here would pass the tier's
/// structural gate.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_tier_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        // 2026-09-05: DEFAULT ON. `=0` is the kill switch — which is why
        // this reads the VALUE now rather than only asking whether the
        // variable is present.
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// `(class, method+descriptor, entry pc)` triples whose optimizing OSR
/// artifact this door has already built and REFUSED.
///
/// # Why a negative cache is the whole point
///
/// The door's admission is `ir_osr_entry_addr(entry_pc).is_some() &&
/// ir_osr_sentinel_free`, and `ir_osr_sentinel_free` means the body emits no
/// deopt stub AND no call-exception stub — so **any method containing a call
/// fails it**. That is most methods. Without a memo the door pays a FULL
/// optimizing compile on every OSR attempt for every one of them, throws the
/// artifact away, and falls through to the single-pass path it was always
/// going to take.
///
/// Measured on `LambdaJitTierUp.warmChecksum` (`entries=[] sentinel_free=false`,
/// refused every time): the repeated compiles delay the single-pass OSR entry
/// the frame actually gets, and while the loop is still interpreted its in-loop
/// SAM call site has no compiled caller to hang an inline-cache thunk on — so
/// every dispatch is answered by Rust. That is the intermittent
/// `test_inline_cache_takes_over_the_sam_call_site` failure, at ~3% on a
/// contended host and 0 in 60 runs with the door off.
///
/// Keyed by name rather than by `CachedBytecodeMethod` identity because the
/// frame's handle is resolved afresh on some paths, and a memo that missed
/// would be no memo at all. Lives in [`JitBridgeState::osr_optimizing_refused`].
///
/// The key both optimizing-OSR memos use.
///
/// `vm` is the owning VM's `vm_identity`, hashed in with the name. Both memos
/// were process-wide statics while a `ClassId` is only unique WITHIN a VM, so
/// without it a second VM in the same process with the same class id, method
/// and pc was handed the first VM's accepted `CompiledMethod` — code built
/// against another heap, other class ids and other `ldc` slots — or refused an
/// optimizing body on the strength of another VM's verdict. Both memos now
/// live in the VM's own [`JitBridgeState`]; the identity stays in the key as a
/// second, independent guard.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_refusal_key(
    vm: usize,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    entry_pc: usize,
) -> (u32, u64, usize) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    vm.hash(&mut h);
    method_name.hash(&mut h);
    descriptor.hash(&mut h);
    (class_id.as_u32(), h.finish(), entry_pc)
}

/// `key` with its pc replaced by one no bytecode has (`usize::MAX`): the
/// refusal-memo key for a verdict that does not depend on the entry pc (round
/// 11 wave 9). The optimizing OSR build is pc-blind — the single-pass
/// preference, the static gates, a declined compile and a body that is not
/// sentinel-free are all properties of the METHOD — and only "no entry stub at
/// this pc" is a per-pc verdict. Only ever used as a refusal key: nothing is
/// cached under it.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_method_wide_key(key: (u32, u64, usize)) -> (u32, u64, usize) {
    (key.0, key.1, usize::MAX)
}

/// `CRATONVM_DBG_OSR` — trace OSR entries, refusals and bails. Read once: the
/// entry trace was an uncached `runtime_flag_on` on every OSR entry.
pub(in crate::runtime::interpreter::jit_bridge) fn dbg_osr_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)
}

/// Whole activations whose every OSR offer at one loop header may be refused
/// before later activations stop offering there (until the install epoch
/// moves; see [`osr_loop_offer_allowed`]).
pub(crate) const OSR_REFUSED_ACTIVATIONS_BUDGET: u32 = 8;

/// Once a loop header's budget is spent, one activation in this many offers
/// again, so a refusal that was transient after all costs a bounded delay
/// rather than the loop's OSR for the rest of the epoch.
pub(crate) const OSR_SPENT_LOOP_REPROBE_INTERVAL: u32 = 256;

/// One loop header's cross-activation OSR record (interpreter round i1 wave 8,
/// L7; `interpreter-L7-osr-refusals-reoffered-per-activation-FIXED`, fix 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct OsrRefusedLoop {
    /// This VM's code-state epoch (`JitRealm::code_state_epoch`) the counts
    /// below belong to; a record from an older epoch is discarded when next
    /// touched. The process install epoch until interpreter round i1 wave 14,
    /// so another VM's flush reset this VM's budget.
    pub(in crate::runtime::interpreter::jit_bridge) epoch: u64,
    /// Activations whose offers at this pc were all refused.
    pub(in crate::runtime::interpreter::jit_bridge) refused_activations: u32,
    /// Activations turned away since the budget was spent (or the last
    /// re-probe).
    pub(in crate::runtime::interpreter::jit_bridge) skipped: u32,
}

/// The key of one loop header of the method `frame` runs: `(class id, the
/// method's name-and-descriptor fingerprint, pc)`.
///
/// The fingerprint is the low half of the method's invocation-counter key
/// (`cratonvm_jit_api::invoc_key_parts`), which a cached frame's entry
/// memoises (`CachedBytecodeMethod::invoc_key`), so a cached frame hashes no
/// string here; an owned frame computes the same value. Until interpreter
/// round i1 wave 20 (lane L5) this was `osr_optimizing_refusal_key` -- a
/// SipHash of the VM identity, name and descriptor per consult. The table is
/// the VM's own (`JitBridgeState`), so the identity is not needed, and a
/// fingerprint collision could only share a performance budget (the record
/// never decides what runs, only whether OSR is offered).
pub(in crate::runtime::interpreter::jit_bridge) fn osr_loop_key(frame: &Frame, entry_pc: usize) -> (u32, u64, usize) {
    let method_key = match frame.cached_method() {
        Some(cached) => cached.invoc_key(),
        None => cratonvm_jit_api::invoc_key_parts(
            frame.class_id.as_u32(),
            frame.method_name(),
            frame.method_descriptor(),
        ),
    };
    // Truncation: the low 64 bits are the fingerprint; the high half is the
    // class id, which the key carries itself.
    (frame.class_id.as_u32(), method_key as u64, entry_pc)
}

/// The word and bit of `JitBridgeState::osr_refused_loop_filter` a loop key
/// maps to: the top byte of a multiplicative mix of all three parts.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_loop_filter_bit(key: (u32, u64, usize)) -> (usize, u64) {
    // Widening: `usize` is at most 64 bits on every supported target.
    let mixed = (key.1 ^ u64::from(key.0).rotate_left(32) ^ key.2 as u64)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let bit = mixed >> 56;
    // Truncation: `bit >> 6` is at most 3.
    ((bit >> 6) as usize, 1u64 << (bit & 63))
}

/// May `osr_refused_loops` hold a record for `key`? `false` is exact (no
/// record, as of the last publish); `true` may be a filter collision.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_loop_may_have_record(bridge: &JitBridgeState, key: (u32, u64, usize)) -> bool {
    let (word, bit) = osr_loop_filter_bit(key);
    (bridge.osr_refused_loop_filter[word].load(std::sync::atomic::Ordering::Acquire) & bit) != 0
}

/// Publish `loops` -- the map behind `bridge.osr_refused_loops`, whose lock
/// the caller holds -- to the lock-free readers: the filter first, then the
/// count, both from the map as it now is. Called after every change.
pub(in crate::runtime::interpreter::jit_bridge) fn publish_osr_refused_loops(
    bridge: &JitBridgeState,
    loops: &std::collections::HashMap<(u32, u64, usize), OsrRefusedLoop>,
) {
    let mut words = [0u64; 4];
    for key in loops.keys() {
        let (word, bit) = osr_loop_filter_bit(*key);
        words[word] |= bit;
    }
    for (slot, word) in bridge.osr_refused_loop_filter.iter().zip(words) {
        slot.store(word, std::sync::atomic::Ordering::Release);
    }
    bridge
        .osr_refused_loop_count
        .store(loops.len(), std::sync::atomic::Ordering::Release);
}

/// May this activation offer OSR at `entry_pc`? Asked once per activation and
/// loop header, at the first offer the frame's backoff schedule makes there.
///
/// The frame's own budget (`Frame::osr_attempt_counts`, five offers) is reset
/// by every activation, so a loop whose offers are always refused was
/// re-offered five times per call for the life of the process -- in the
/// inline configuration (`CRATONVM_BG_COMPILE=0`) five compile pipelines per
/// call. This is the method-wide record HotSpot keeps as "not OSR-compilable":
/// after [`OSR_REFUSED_ACTIVATIONS_BUDGET`] fully refused activations the loop
/// is not offered again at this install epoch, except in one activation of
/// every [`OSR_SPENT_LOOP_REPROBE_INTERVAL`], which starts one refusal short of
/// the budget -- so a re-probe that is refused again re-spends it, and one that
/// is not leaves the loop offered normally.
///
/// Per VM and per code-state epoch (`JitRealm::code_state_epoch`), like
/// `mark_osr_denied`: a redefinition or a code-cache flush of THIS VM forgets
/// every record. `false` only when a record exists.
///
/// Cost when no record can exist for this header: one load (no loop in the
/// VM has a record), or the memoised method key and one more load (the
/// header's filter bit is clear, `osr_loop_may_have_record`) -- no string
/// hash and no lock. Only a header whose bit is set takes the lock.
pub(crate) fn osr_loop_offer_allowed(shared: &SharedVm, frame: &Frame, entry_pc: usize) -> bool {
    let bridge = &shared.jit.bridge;
    if bridge
        .osr_refused_loop_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return true;
    }
    let key = osr_loop_key(frame, entry_pc);
    if !osr_loop_may_have_record(bridge, key) {
        return true;
    }
    osr_loop_offer_allowed_at(bridge, key, shared.jit.code_state_epoch())
}

/// [`osr_loop_offer_allowed`] for a given key and epoch.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_loop_offer_allowed_at(bridge: &JitBridgeState, key: (u32, u64, usize), epoch: u64) -> bool {
    let Ok(mut loops) = bridge.osr_refused_loops.lock() else {
        return true;
    };
    let Some(record) = loops.get_mut(&key) else {
        return true;
    };
    if record.epoch != epoch {
        loops.remove(&key);
        publish_osr_refused_loops(bridge, &loops);
        return true;
    }
    if record.refused_activations < OSR_REFUSED_ACTIVATIONS_BUDGET {
        return true;
    }
    record.skipped = record.skipped.saturating_add(1);
    if record.skipped >= OSR_SPENT_LOOP_REPROBE_INTERVAL {
        record.skipped = 0;
        record.refused_activations = OSR_REFUSED_ACTIVATIONS_BUDGET - 1;
        return true;
    }
    false
}

/// Record that this activation's offers at `entry_pc` were all refused (its
/// frame budget reached `OSR_MAX_ATTEMPTS` through refusals). See
/// [`osr_loop_offer_allowed`]. `CRATONVM_DBG_OSR` names the loop at every
/// power-of-two count, and when its budget is spent: the census of how many
/// offers this budget removes.
pub(crate) fn note_osr_refused_activation(shared: &SharedVm, frame: &Frame, entry_pc: usize) {
    let key = osr_loop_key(frame, entry_pc);
    let refused =
        note_osr_refused_activation_at(&shared.jit.bridge, key, shared.jit.code_state_epoch());
    if dbg_osr_enabled() && (refused.is_power_of_two() || refused == OSR_REFUSED_ACTIVATIONS_BUDGET)
    {
        dbg_osr_loop_budget(frame, entry_pc, refused);
    }
}

/// [`note_osr_refused_activation`] for a given key and epoch; returns the
/// loop's count of refused activations at that epoch.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_refused_activation_at(
    bridge: &JitBridgeState,
    key: (u32, u64, usize),
    epoch: u64,
) -> u32 {
    let Ok(mut loops) = bridge.osr_refused_loops.lock() else {
        return 0;
    };
    // Records of an older epoch are dead (the next touch would discard each),
    // but a loop never offered again kept its record for the life of the VM,
    // and a non-zero `osr_refused_loop_count` sends every first offer of every
    // loop through this lock. Purged here: this runs once per fully refused
    // activation, over a map of refused loop headers (interpreter round i1
    // wave 14).
    loops.retain(|_, r| r.epoch >= epoch);
    let record = loops.entry(key).or_default();
    if record.epoch != epoch {
        *record = OsrRefusedLoop {
            epoch,
            ..OsrRefusedLoop::default()
        };
    }
    record.refused_activations = record.refused_activations.saturating_add(1);
    let refused = record.refused_activations;
    publish_osr_refused_loops(bridge, &loops);
    refused
}

/// An OSR body was ENTERED at `entry_pc` (it returned, threw, or committed a
/// transfer into the frame): forget the loop header's refused-activation
/// record, so only a loop whose offers are ALWAYS refused -- the population
/// [`osr_loop_offer_allowed`] is for -- ever spends its budget.
///
/// Round 11 wave 15 (lane tier). Without this, refusals were never forgiven
/// within an install epoch: eight activations with a TRANSIENT refusal (a
/// callee still compiling inline, a deferred `new`, a background task still
/// queued when the frame's five offers ran out) spread over thousands of
/// activations that entered fine retired the loop to one activation in
/// [`OSR_SPENT_LOOP_REPROBE_INTERVAL`] for the rest of the epoch, and a
/// successful re-probe left it one refusal from that again. One load when no
/// loop in the VM has a record.
pub(crate) fn note_osr_loop_entered(shared: &SharedVm, frame: &Frame, entry_pc: usize) {
    let bridge = &shared.jit.bridge;
    if bridge
        .osr_refused_loop_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return;
    }
    let key = osr_loop_key(frame, entry_pc);
    if !osr_loop_may_have_record(bridge, key) {
        return;
    }
    forget_osr_refused_loop_at(bridge, key);
}

/// [`note_osr_loop_entered`] for a given key.
pub(in crate::runtime::interpreter::jit_bridge) fn forget_osr_refused_loop_at(bridge: &JitBridgeState, key: (u32, u64, usize)) {
    let Ok(mut loops) = bridge.osr_refused_loops.lock() else {
        return;
    };
    if loops.remove(&key).is_some() {
        publish_osr_refused_loops(bridge, &loops);
    }
}

/// `CRATONVM_DBG_OSR` line for the per-loop budget. Out of line and cold.
#[inline(never)]
#[cold]
pub(in crate::runtime::interpreter::jit_bridge) fn dbg_osr_loop_budget(frame: &Frame, entry_pc: usize, refused: u32) {
    let spent = if refused == OSR_REFUSED_ACTIVATIONS_BUDGET {
        format!(
            " -- budget spent: not offered again at this install epoch except one \
             activation in {OSR_SPENT_LOOP_REPROBE_INTERVAL}"
        )
    } else {
        String::new()
    };
    eprintln!(
        "[cratonvm-osr] all-offers-refused {}.{}{} entry_pc={entry_pc} \
         refused_activations={refused}{spent}",
        frame.class_name(),
        frame.method_name(),
        frame.method_descriptor()
    );
}

/// `CRATONVM_JIT_OSR_OPTIMIZING_MEMO=0` — the refusal memo's kill switch, so
/// the memo can be A/B'd inside ONE binary. Read once: it used to be re-read
/// (uncached) on every OSR entry that reached the memo.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_memo_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// Has this door already built and refused an optimizing artifact here?
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_already_refused(shared: &SharedVm, key: (u32, u64, usize)) -> bool {
    // Kill switch, so the memo can be A/B'd inside ONE binary. Comparing an
    // intermittent event across two builds is not a comparison.
    if osr_optimizing_memo_disabled() {
        return false;
    }
    // A refusal recorded before this VM's latest redefinition judged other
    // bytecode (the key names the method, not its code): not a refusal of
    // this one. It was kept for the life of the VM until interpreter round i1
    // wave 14, so a loop whose old body made a call never took the optimizing
    // route after a redefinition removed it.
    let now = shared.jit.jit_cache.redefine_epoch();
    shared
        .jit
        .bridge
        .osr_optimizing_refused
        .lock()
        .map(|s| s.get(&key) == Some(&now))
        .unwrap_or(false)
}

/// May the optimizing tier splice a callee containing `ldc` / `ldc_w` /
/// `ldc2_w`? **Default ON**; `CRATONVM_JIT_IR_SPLICE_LDC=0` restores the
/// blanket refusal, so one binary can be A/B'd against its own pre-change
/// behaviour. See the `0x12 | 0x13 | 0x14` arm of the splice scanner.
pub(in crate::runtime::interpreter::jit_bridge) fn ir_splice_ldc_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// Optimizing OSR artifacts this door has already built and ACCEPTED, keyed
/// exactly like the refusal memo beside it.
///
/// # Why this exists
///
/// Only refusals were remembered. A success was wrapped in a fresh `Arc`,
/// entered, and dropped when the OSR'd frame left — so the next back edge
/// rebuilt the same artifact from bytecode, and the one after that, for the
/// life of the process. The single-pass door has never worked this way: it
/// publishes its artifact and every later entry reports `OSR-reuse`.
///
/// Measured before this, on a counted-loop probe entered 20 000 times
/// (`CRATONVM_JIT_METRICS=1`): **502 compiles of one method**, 63.3 ms of
/// compile wall on a 262 ms run — 24 % of the process, spent rebuilding a body
/// that was byte-identical every time. Switching the door off with
/// `CRATONVM_JIT_OSR_OPTIMIZING=0` recovered all of it, which is what
/// identified the cache rather than the codegen as the cost.
///
/// # Lifetime
///
/// The map holds an `Arc`, so a cached artifact's `ExecutableBuffer` stays
/// mapped for the life of the process. That is *safer* than the previous
/// behaviour, not less safe: an artifact used to be unmapped as soon as the
/// last OSR frame in it returned, which is precisely the retired-code hazard
/// `ExecutableBuffer::drop` and `defer_jit_owner` exist to police. Retained
/// executable code is this VM's standing policy.
///
/// # Redefinition
///
/// Unlike the refusal memo — where a stale entry costs only a missed
/// optimization — a stale entry HERE would run pre-redefinition code. So each
/// entry carries the class's redefinition generation (read before its compile)
/// and the bytecode it was built for, and is served only while both still match
/// (round 11 wave 2; it used to refuse every redefined class outright, which
/// made the door rebuild the body on every attempt in such a class).
/// `any_class_redefined` makes the generation one relaxed load on every run
/// that never redefines anything.
///
/// # Callee pins
///
/// The body is never published through `JitCache`, so
/// `prepare_for_publication` never roots the callees whose entries it CALLs.
/// Each entry therefore carries the pins `compile_optimizing_artifact` took at
/// bind time ([`AcceptedOsrOptimizing`]) for as long as it is cached.
///
/// Lives in [`JitBridgeState::osr_optimizing_accepted`] (per VM). It has
/// `JitCache`'s dependency rule (see [`osr_optimizing_dependencies_current`]),
/// and since wave 10 an entry older than the VM's code-state epoch is purged at
/// the next insert ([`remember_osr_optimizing_artifact`]); like `JitCache`'s own
/// OSR bodies it is never a cold-sweep candidate — see
/// `interpreter-L7-osr-optimizing-door-bypasses-osr-admission-FIXED-20260925.md`, item 4.
///
/// The artifact this door built for `key`, when it may still be entered.
///
/// Re-checks the two properties the build site checked before accepting, rather
/// than trusting that they were checked once: an entry stub for THIS pc, and a
/// body that cannot return the deopt sentinel. They are properties of the
/// artifact and cannot change while it is cached, so this is a cheap assertion
/// of the contract at the point of use, not a second policy.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_cached(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    key: (u32, u64, usize),
    entry_pc: usize,
    code: &Arc<[u8]>,
) -> Option<Arc<cratonvm_jit::CompiledMethod>> {
    use std::sync::atomic::Ordering;
    if osr_optimizing_cache_disabled() {
        return None;
    }
    // Round 11 wave 2: an entry built before the class's latest redefinition,
    // or for other bytecode than this frame runs, is a miss. This replaces a
    // `class_was_redefined` refusal, which is sticky: once a class had been
    // redefined, every OSR attempt in it rebuilt the optimizing body and threw
    // it away (`remember_osr_optimizing_artifact` still inserted it).
    let generation = osr_redefine_generation(shared, class_id);
    // INTERP719-2 (JIT round 19 wave 8, lane osr8,
    // `CRATONVM_JIT_LATE_BIND_OSR_OPTIMIZING`): a callee this body left on the
    // dispatch helper for a "not yet" refusal is served compiled now
    // (`late_bind_osr_key_served`); forget the body, as al16's first-call rule
    // below does, so the door rebuilds it with the call bound. Taken before
    // the memo's lock (the mark's own lock is never held with it). One
    // relaxed load while nothing is marked.
    if late_bind_osr_optimizing_stale_taken(shared, key) {
        let late = shared
            .jit
            .bridge
            .osr_optimizing_accepted
            .lock()
            .ok()
            .and_then(|mut accepted| accepted.remove(&key));
        note_osr_optimizing_memo_change(0, u64::from(late.is_some()));
        if let Some(entry) = &late {
            if late_bind_osr_optimizing_enabled() {
                // A weak row (t1 wave 9, lane JITB): `late` is the memo's
                // last handle unless a frame runs the body.
                if let Ok(mut superseded) = shared.jit.bridge.late_bind_osr_superseded.lock() {
                    push_superseded_osr_row(
                        &mut superseded,
                        SupersededOsrBody::new(class_id, names, entry.artifact.arc()),
                    );
                }
            }
        }
        if crate::runtime::env_cache::dbg_jitc() {
            if let Some(entry) = &late {
                let body = entry.artifact.arc();
                eprintln!(
                    "[cratonvm-jitc] osr optimizing memo FORGETS a body a late bind can now bind: {} id={}",
                    body.method_label, body.compile_id
                );
            }
        }
        drop(late);
        return None;
    }
    let mut accepted = shared.jit.bridge.osr_optimizing_accepted.lock().ok()?;
    let is_stale = accepted.get(&key).is_some_and(|entry| {
        !entry.withdrawn
            && entry.redefine_generation == generation
            && same_bytecode(&entry.code, code)
            && cached_osr_body_is_despec_stale(
                shared,
                entry.artifact.arc(),
                &Arc::from(names.0),
                &Arc::from(names.1),
                &Arc::from(names.2),
            )
    });
    if is_stale {
        if let Some(stale_entry) = accepted.remove(&key) {
            drop(accepted);
            if let Ok(mut superseded) = shared.jit.bridge.late_bind_osr_superseded.lock() {
                push_superseded_osr_row(
                    &mut superseded,
                    SupersededOsrBody::new(class_id, names, stale_entry.artifact.arc()),
                );
            }
        }
        return None;
    }
    let entry = accepted.get_mut(&key).filter(|entry| {
        !entry.withdrawn
            && entry.redefine_generation == generation
            && same_bytecode(&entry.code, code)
    })?;
    // Equal bytes but another allocation -- a body the background worker built
    // from `fetch_osr_compile_inputs`' own copy -- would pay a byte-for-byte
    // compare of the whole method on every later entry. Keep the entering
    // frame's copy instead, so the next hit is `Arc::ptr_eq` (wave 10).
    if !Arc::ptr_eq(&entry.code, code) {
        entry.code = Arc::clone(code);
    }
    let cached = Arc::clone(entry.artifact.arc());
    let exits_admitted = entry.exits_admitted;
    // al16: a call site the build left to its first execution has run since;
    // forget the body so the door rebuilds it with the site resolved (and
    // spliceable). See `AcceptedOsrOptimizing::first_call_cells_at_build`.
    if some_first_call_cell_fired(&entry.first_call_cells_at_build) {
        let resolved = accepted.remove(&key);
        drop(accepted);
        note_osr_optimizing_memo_change(0, u64::from(resolved.is_some()));
        // t1 wave 8, lane L1OSR (L1W1-4,
        // `CRATONVM_JIT_OSR_PROFIT_EXIT_SUPERSEDED_OPTIMIZING`, default ON since j27): a frame
        // still in this body keeps the unresolved site's dispatch call for its
        // activation, the late-bind forget's shape; queued like it, armed by
        // the rebuild once it is no longer remembered under any header.
        if let Some(entry) = &resolved {
            if osr_profit_exit_superseded_optimizing_enabled() {
                queue_superseded_optimizing_osr_bodies(
                    shared,
                    class_id,
                    names,
                    vec![entry.artifact.clone()],
                );
            }
        }
        drop(resolved);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing memo FORGETS a body built before a first call: {} id={}",
                cached.method_label, cached.compile_id
            );
        }
        return None;
    }
    // JIT round 21 (lane typecheck, TYPECHECK21-8): a class this body reached
    // through the static helpers has finished initializing since; forget the
    // body so the door rebuilds it with direct accesses. Asked through
    // `init_state`, the same authority `static_init_pending_of` recorded
    // the list from (JIT round 21, lane smallfix).
    if static_init_pending_any_initialized(shared, &entry.static_init_pending_at_build) {
        let rebuilt = accepted.remove(&key);
        drop(accepted);
        note_osr_optimizing_memo_change(0, u64::from(rebuilt.is_some()));
        // t1 wave 9, lane JITB: a frame still in this body keeps the static
        // helper path for its activation, the al16 forget's shape above;
        // queued like it under the same opt-in switch.
        if let Some(entry) = &rebuilt {
            if osr_profit_exit_superseded_optimizing_enabled() {
                queue_superseded_optimizing_osr_bodies(
                    shared,
                    class_id,
                    names,
                    vec![entry.artifact.clone()],
                );
            }
        }
        drop(rebuilt);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing memo FORGETS a body built before a class initialized: {} id={}",
                cached.method_label, cached.compile_id
            );
        }
        return None;
    }
    // Interpreter round i1 wave 25 follow-up (lane L6b): a body a class
    // redefinition withdrew is never served again, whatever its stamps say
    // (the OSR door refuses to start one anyway, `try_osr`; this makes the
    // memo agree and forget it, so the door rebuilds against the current
    // bytecode instead of refusing the same entry at every back edge).
    if shared.jit.jit_cache.body_withdrawn_by_redefinition(&cached) {
        let withdrawn = accepted.remove(&key);
        drop(accepted);
        note_osr_optimizing_memo_change(0, u64::from(withdrawn.is_some()));
        drop(withdrawn);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing memo FORGETS a withdrawn body: {} id={}",
                cached.method_label, cached.compile_id
            );
        }
        return None;
    }
    // Compiled before the last redefinition, code-cache flush or layout
    // upgrade of this VM (its code-state epoch moves on each): what it
    // inlined or bound may be gone, and `JitCache::put`'s flush barrier —
    // which would refuse it — never sees this body. Miss, so the door
    // rebuilds and `remember_osr_optimizing_artifact` replaces it. The stale
    // body is forgotten here, not at that rebuild: a rebuild the door refuses
    // or skips remembers nothing, and the stale code and its callee pins
    // stayed held for the life of the VM.
    if osr_optimizing_artifact_is_stale(shared, entry.code_state_epoch) {
        let stale = accepted.remove(&key);
        drop(accepted);
        note_osr_optimizing_memo_change(0, u64::from(stale.is_some()));
        drop(stale);
        return None;
    }
    // A class it inlined from or devirtualised against gained a subclass (or
    // was unloaded) since it was last checked: `JitCache` withdraws a
    // published body for exactly that, and this one is never published. One
    // atomic load while no invalidation was logged anywhere since the last
    // check; the log scan runs once per epoch move, over the newer records
    // only. A withdrawn body is forgotten (dropped after the guard), so the
    // door rebuilds against the current hierarchy.
    let checked_through = entry.dependencies_checked_through.load(Ordering::Acquire);
    let now = cratonvm_jit::jit_invalidation_epoch();
    if checked_through < now {
        let current = osr_optimizing_installed_dependencies_current(
            shared,
            class_id,
            names,
            &cached,
            checked_through,
        );
        if !current {
            let withdrawn = accepted.remove(&key);
            drop(accepted);
            note_osr_optimizing_memo_change(0, u64::from(withdrawn.is_some()));
            drop(withdrawn);
            return None;
        }
        entry
            .dependencies_checked_through
            .store(now, Ordering::Release);
    }
    // JIT round 19 wave 2 (lane osr2, OSR19-1): taken, so it is asked once.
    let reask_poly_decline = std::mem::take(&mut entry.poly_decline_unasked);
    drop(accepted);
    if reask_poly_decline
        && cratonvm_jit::osr_poly_decline_per_header_enabled()
        && osr_optimizing_header_declined_on_reuse(shared, class_id, names, key, entry_pc, code)
    {
        return None;
    }
    // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
    // Tier proposal 27 (round 11 wave 10): a body the build admitted for its
    // exceptional exits is enterable too; `ir_osr_sentinel_free` alone keeps
    // its meaning, "no exit at all".
    if cached.ir_osr_entry_addr(entry_pc as u32).is_some()
        && (cached.ir_osr_sentinel_free || exits_admitted)
    {
        Some(cached)
    } else {
        None
    }
}

/// JIT round 19 wave 2 (lane osr2, OSR19-1,
/// `CRATONVM_JIT_OSR_POLY_DECLINE_PER_HEADER`): a body built at another
/// header is served here for the first time. Ask the polymorphic-loop decline
/// for THIS header, with the profile as it is now (what a first trigger here
/// would have been asked); on a decline, forget the body under this key and
/// memoise the refusal for this header only, so the single-pass OSR body that
/// splices the site runs this loop. `true` = declined (the caller misses).
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_header_declined_on_reuse(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    key: (u32, u64, usize),
    entry_pc: usize,
    code: &Arc<[u8]>,
) -> bool {
    let judged_at = shared.jit.jit_cache.redefine_epoch();
    let Some(pc) =
        osr_door_prefers_spliced_single_pass_early(shared, class_id, names, code, entry_pc)
    else {
        return false;
    };
    note_osr_optimizing_refusal(shared, key, judged_at);
    let forgotten = match shared.jit.bridge.osr_optimizing_accepted.lock() {
        Ok(mut m) => m.remove(&key),
        Err(_) => None,
    };
    note_osr_optimizing_memo_change(0, u64::from(forgotten.is_some()));
    drop(forgotten);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr optimizing {}.{} pc={entry_pc}: reused body declined at this header -- the single-pass OSR body splices the polymorphic site at pc={pc} (CRATONVM_JIT_OSR_POLY_DECLINE_PER_HEADER)",
            names.0, names.1
        );
    }
    true
}

/// `CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS=1` — tier proposal 27, phases 1-2
/// (round 11 wave 10, lane irexc): let the optimizing OSR door enter a body
/// whose only exits are exceptional. **Default ON** since round 11 wave 11
/// (`=0` is the kill switch). The same variable makes
/// `ir_lower` emit OSR entry stubs into such a body, so one switch turns the
/// whole route on. Declared by
/// `r11w10-irexc-types-ir-precise-handler-frames-flag-patch-FIXED-20260924.md`.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_exception_exits_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Tier proposal 27, phases 1-2 (round 11 wave 10, lane irexc): may an
/// optimizing OSR body that is NOT sentinel-free be entered, because every
/// exit it can take is exceptional and the door resolves each one exactly?
///
/// `cm.ir_osr_exception_exit_bcis` is `Some` only when the body emits no guard
/// deopt (a guard exit would need a planless `resume_after_exit` for an IR
/// frame state — phase 3) and no publishing-nothing exit leaves a snapshot
/// that holds a monitor. What is left is two kinds of exit, and the door
/// resolves both without replaying an iteration:
///
/// 1. a reason-9 rethrow pad, inside a protected range: its precise frame is
///    taken by `route_osr_exception_out_of_artifact`, which enters the
///    method's own handler in the live frame
///    (`transfer_osr_exception_exit_into_live_frame`, which finds the point in
///    `deopt_points`). The lowerer refused every frame that transfer would
///    decline (`ir_lower::rethrow_frame_refusal`);
/// 2. a call-exception exit that publishes nothing, at one of `bcis`. The
///    router finds no frame and PROPAGATES from the caller
///    (`OSR_FRAME_DECLINED_TO_CATCH`). That is exact only when no row of this
///    method's exception table covers the throw bci (JVMS §2.10) — checked
///    here, because only the door has the table — and when the frame holds no
///    monitor there, which the lowerer's fact already guarantees.
///
/// NOT closed, and shared with the single-pass door: a callee whose sentinel
/// service declines leaves a foreign stash (`try_osr`'s identity-mismatch
/// arm). See r11w9-tier-optimizing-osr-exception-exits-door-contract-patch.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_exception_exits_admissible(
    cm: &cratonvm_jit::CompiledMethod,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> bool {
    if !osr_optimizing_exception_exits_enabled() {
        return false;
    }
    osr_exception_exit_bcis_uncovered(cm.ir_osr_exception_exit_bcis.as_deref(), exception_table)
}

/// The table half of [`osr_optimizing_exception_exits_admissible`], without
/// the switch: `Some(bcis)` with no bci inside any `[start_pc, end_pc)` row.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_exception_exit_bcis_uncovered(
    bcis: Option<&[u32]>,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> bool {
    let Some(bcis) = bcis else {
        return false;
    };
    bcis.iter().all(|&bci| {
        exception_table
            .iter()
            .all(|row| !(u32::from(row.start_pc) <= bci && bci < u32::from(row.end_pc)))
    })
}

/// The single-pass OSR door's caller-held synchronized direct CALL (round 11
/// wave 14, lane spsync). Withdrawn at the round-11 close: with it, a callee
/// that catches its own `ArithmeticException` (`R11TierSyncDoorCompiled`'s
/// `bumpCatch`, called from `drive`'s single-pass OSR body under
/// `CRATONVM_C2_ACCEPT=never`) let the exception escape to `main`.
///
/// RE-ENABLED in round 11 wave 15 (lane tier). The cause was not this door:
/// `sync_direct_target` admitted a callee that declares an exception table,
/// and no caller-held CALL services the callee's own handler (see the
/// refusal there, which now covers every door). `CRATONVM_JIT_SYNC_DIRECT=0`
/// (read by `single_pass_sync_direct_site_admitted`) still turns the route
/// off here as everywhere. See
/// `r11w13-spsync-osr-door-has-no-sync-direct-route-FIXED-20260925.md`.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_door_sync_direct_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_OSR_OPTIMIZING_GUARD_EXITS=1` — tier proposal 27, phase 3
/// (round 11 wave 12, lane irexc): let the optimizing OSR door enter a body
/// whose guard exits it can resume without a plan. DEFAULT ON since round 11
/// wave 14 (`=0` is the kill switch). The same variable
/// makes `ir_lower` emit OSR entry stubs into such a body. Declared by
/// `r11w12-irexc-types-osr-optimizing-guard-exits-flag-patch-FIXED-20260924.md`.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_guard_exits_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_OSR_GUARD_EXITS_OPAQUE_HANDLER_ENTRIES` (OSR round,
/// 2026-10-06; **default ON**, `=0` is the kill switch): let
/// [`osr_optimizing_guard_exits_admissible`] admit a body whose protected
/// guard exits' handlers reach an entry when every such entry is OPAQUE
/// (`CompiledMethod::ir_osr_opaque_entry_bcis`).
///
/// The handler-reach rule keeps a caught throw's continuation from
/// re-entering the body at a header whose facts assumed a handler-free path.
/// An opaque entry assumes nothing about the state it enters with: every live
/// local arrives through a φ with an `Op::OsrLocal` input and memory through
/// `Op::OsrMemory` (`IrBuilder::build_opaque_osr_merge`). The door's own
/// per-entry verdict (`closed_by_a_handler` in
/// `build_osr_optimizing_artifact`) has exempted such an entry since round 11
/// wave 17; this test did not, so every OSR-door body (opaque at every header
/// since 44df838b5f) holding a guard exit inside a `try` whose handler falls
/// back into the loop was refused method-wide and ran the single-pass body
/// (`FerOsrTry`, `docs/feature-designs/deopt-osr.md`).
/// Read without a `static` (`process_global_statics_ratchet`); asked once per
/// door verdict on such a body.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_handler_entries_opaque_enabled() -> bool {
    true
}

/// May an optimizing OSR body with guard exits be entered?
///
/// The lowerer's guard fact certifies that every exit can reconstruct its
/// frame and transfer its monitors. Strict entries additionally require exact
/// or value-equivalent guards; speculative guards require every emitted entry
/// to import arbitrary locals and memory through an opaque merge.
///
/// Unprotected exits are admitted directly. Protected exits require known
/// handler reach that cannot reach any emitted entry, so a caught throw's
/// continuation cannot reuse assumptions from the exited activation. This
/// proof applies to unlocked frames too. Held monitors still require the
/// separate opt-in producer rule and the runtime monitor transfer checks.
///
/// The planless arm of `try_osr` resumes the guard at its own bytecode and
/// rechecks the same frame contract before transferring any state.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_guard_exits_admissible(
    cm: &cratonvm_jit::CompiledMethod,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    code: &[u8],
) -> bool {
    if !osr_optimizing_guard_exits_enabled() {
        return false;
    }
    let bcis = cm.ir_osr_guard_exit_bcis.as_deref();
    if osr_exception_exit_bcis_uncovered(bcis, exception_table) {
        return true;
    }
    // A protected guard may also belong to an ordinary, unlocked frame.
    // The artifact's guard fact already certifies resumability and monitors.
    // A caught throw is safe here when no handler continuation can reach any
    // emitted entry; do not require the unrelated held-lock extension.
    // OSR round 2026-10-06: an OPAQUE entry is exempt, as the door's own
    // `closed_by_a_handler` exempts it (`osr_handler_entries_opaque_enabled`).
    let entry_bcis = osr_entries_a_caught_throw_may_reenter(
        cm.ir_osr_entries.iter().map(|&(bci, _, _)| bci),
        &cm.ir_osr_opaque_entry_bcis,
        osr_handler_entries_opaque_enabled(),
    );
    let admitted = osr_guard_exits_caught_off_every_entry(
        bcis,
        osr_handler_reachable_pcs(code, exception_table).as_deref(),
        &entry_bcis,
    );
    if admitted && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr optimizing: guard exits {:?} admitted inside a protected range (no entry {:?} on a handler-reached pc)",
            bcis.unwrap_or(&[]),
            entry_bcis,
        );
    }
    admitted
}

/// The entries of a body a caught throw's continuation must not reach: every
/// entry bci, less the OPAQUE ones (`opaque`, sorted:
/// `CompiledMethod::ir_osr_opaque_entry_bcis`) when `opaque_exempt`
/// ([`osr_handler_entries_opaque_enabled`]). An opaque entry assumes nothing
/// about the state it enters with (OSR round, 2026-10-06).
pub(in crate::runtime::interpreter::jit_bridge) fn osr_entries_a_caught_throw_may_reenter(
    entry_bcis: impl Iterator<Item = u32>,
    opaque: &[u32],
    opaque_exempt: bool,
) -> Vec<u32> {
    entry_bcis
        .filter(|bci| !(opaque_exempt && opaque.binary_search(bci).is_ok()))
        .collect()
}

/// The table half of the round-16-wave-13 widening of
/// [`osr_optimizing_guard_exits_admissible`], without the switches: the
/// lowerer listed the guard exits (`bcis` is `Some`), the handlers' reach is
/// known (`handler_reach`, [`osr_handler_reachable_pcs`], sorted; `None` =
/// unanalysable, refused), and no entry stub of the body (`entry_bcis`) is on
/// a pc a handler reaches. `build_osr_optimizing_artifact` already closes such
/// an entry for every exits-admitted body (`closed_by_a_handler`); here it is
/// the admission condition, so a caught throw's continuation -- javac's
/// release-and-rethrow, or any handler path -- can never re-enter the body at
/// a header whose facts it did not establish. Every local a handler reads is
/// in the guard's frame: the snapshot prune keeps what the exception edges
/// read (`ea_ir_bridge::body_local_liveness`), and a frame with an
/// undescribable live slot was refused by the lowerer's fact.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_guard_exits_caught_off_every_entry(
    bcis: Option<&[u32]>,
    handler_reach: Option<&[u32]>,
    entry_bcis: &[u32],
) -> bool {
    let (Some(_), Some(reach)) = (bcis, handler_reach) else {
        return false;
    };
    entry_bcis
        .iter()
        .all(|bci| reach.binary_search(bci).is_err())
}

/// `try_osr`'s sentinel arm for an entry taken WITHOUT a plan, i.e. through an
/// optimizing body's own stub (tier proposal 27, phase 3). `true` when the
/// guard exit was transferred into the live frame, which is then parked at
/// the guard's bci; `false` leaves the caller's refusal path exactly as it is
/// for any exit the transfer cannot honour. `point_addr` is the address
/// `take_last_deopt_with_point` returned with `rframe`.
///
/// Refuses with the switch off: then no body was admitted for its guard exits
/// (`osr_optimizing_guard_exits_admissible`, same switch), and a guard exit
/// reaching here came from a body this arm never vouched for.
///
/// `uncharged_forced` (round 15 wave 4, lane chain5): set when the exit was a
/// forced chain exit of `CRATONVM_DEOPT_EAGER_CHAINS` the caller must leave
/// uncharged (`deopt_resume::leave_forced_trip_uncharged`); it is then not
/// counted toward the memo's value-equivalent withdrawal either.
///
/// `throw_out` (the for-each residual round, 2026-10-06): set, with `false`
/// returned, when re-allocating the exit frame's scalar-replaced objects found
/// the heap exhausted. The body ran, so continuing the live frame would replay
/// its iterations: the activation is abandoned with `OutOfMemoryError`, as
/// HotSpot pops the frames of a failed reallocation, and the caller must
/// propagate it. Such a frame holds no monitor (`osr_exit::guard_exit_fact`).
///
/// `counted_out` (2026-10-26): set when the transferred exit was counted toward
/// the memo's value-equivalent withdrawal
/// ([`note_osr_optimizing_value_equivalent_exit`]). That bound is the exit's
/// brake, so the caller takes the entry as committed rather than charging it
/// against the frame's per-header budget as well: charged too, the fourth exit
/// withdrew the body exactly as it spent the activation's fifth offer, and a
/// never-returning loop whose speculation stopped holding (a pruned branch now
/// taken) stayed interpreted for the rest of its life, never offered the
/// single-pass body the withdrawal hands it to (`TcuLateOsr`, 766 ns per
/// iteration against 10).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_transfer_guard_exit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    compiled: &cratonvm_jit::CompiledMethod,
    point_addr: usize,
    uncharged_forced: &mut bool,
    throw_out: &mut Option<ObjectRef>,
    counted_out: &mut bool,
) -> bool {
    if !osr_optimizing_guard_exits_enabled() {
        return false;
    }
    // Round 15 wave 4 (lane chain4/chain5): a forced chain exit of this body,
    // claimed before the transfer can refuse (census); its charge is decided
    // once it was transferred (a refused one is charged by the refusal arm).
    let forced = super::deopt_resume::claim_forced_chain_trip(shared, compiled, point_addr);
    let mut heap_exhausted = false;
    match super::deopt_resume::transfer_osr_guard_exit_into_live_frame(
        shared,
        thread,
        frame_idx,
        rframe,
        compiled,
        point_addr,
        forced,
        &mut heap_exhausted,
    ) {
        Ok(_) => {
            *uncharged_forced = super::deopt_resume::leave_forced_trip_uncharged(shared, forced);
            if !*uncharged_forced {
                *counted_out =
                    note_osr_optimizing_value_equivalent_exit(shared, compiled, point_addr);
            }
            true
        }
        Err(why) => {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] OSR optimizing guard-exit transfer reject ({why}) at bci={}",
                    rframe.bci
                );
            }
            if heap_exhausted {
                if let MethodCallFailed::ExceptionThrown(exc) =
                    super::deopt_resume::rematerialisation_out_of_memory(shared, thread)
                {
                    *throw_out = Some(exc);
                }
            }
            false
        }
    }
}

/// Value-equivalent guard exits an accepted optimizing OSR body may take before
/// its memo entries are withdrawn. The same four `PER_BCI_DESPEC_LIMIT` gives
/// a charged speculation.
pub(in crate::runtime::interpreter::jit_bridge) const OSR_OPTIMIZING_VALUE_EQUIVALENT_EXIT_LIMIT: u32 = 4;

/// Tier proposal 27 phase 3, item 2 (round 11 wave 13, lane irexc): bound the
/// churn of a VALUE-EQUIVALENT guard that fails on every entry.
///
/// An exact guard's exit ends the frame (the re-executed bytecode throws), but
/// a value-equivalent one — the `Integer` / `Long` unbox class and layout
/// guards, the only `ReceiverTypeChanged` points an admitted body can carry
/// (`osr_exit::guard_exit_fact` refuses every other) — resumes a frame that
/// keeps looping. The interpreter finishes the iteration, re-enters at the next
/// back edge, and exits again: correct, and one OSR entry plus one transfer per
/// iteration. `charge_osr_guard_exit` de-specs the site after four, but nothing
/// withdrew THIS memo (the single-pass cache has
/// `cached_osr_body_is_despec_stale`), and a rebuild would not help: the IR
/// unbox lowering does not consult the de-spec registry, so it would emit the
/// same guards.
///
/// So after [`OSR_OPTIMIZING_VALUE_EQUIVALENT_EXIT_LIMIT`] such exits every memo
/// entry serving `compiled` is withdrawn and its key refused: the loop falls
/// back to the single-pass OSR body, which honours the de-spec. Entries are
/// marked, not removed, so their callee pins stay alive for any frame still
/// running the body. Reached only after a successful planless transfer, i.e.
/// only under `CRATONVM_JIT_OSR_OPTIMIZING_GUARD_EXITS=1`.
///
/// `true` when the exit was one this bound counts (value-equivalent or, for an
/// opaque-entry body, speculative); the door then leaves it uncharged against
/// the frame (`osr_optimizing_transfer_guard_exit`'s `counted_out`).
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_value_equivalent_exit(
    shared: &SharedVm,
    compiled: &cratonvm_jit::CompiledMethod,
    point_addr: usize,
) -> bool {
    // Round 11 wave 14 (lane osrmerge): under `CRATONVM_JIT_OSR_OPAQUE_ENTRY`
    // an admitted body may also carry SPECULATIVE `Op::Guard` exits
    // (`UncommonTrap`, `osr_exit::guard_exit_fact_opaque`), and one whose
    // speculation stopped holding fails on every re-entry exactly like a
    // value-equivalent guard: bounded the same way. Wave 17 (lane tier): asked
    // of the artifact (it was built with opaque arms), not of the process
    // switch, like the door's handler-reach refusal.
    let opaque_entries = !compiled.ir_osr_opaque_entry_bcis.is_empty();
    let value_equivalent = compiled._deopt_point_boxes.iter().any(|p| {
        let point: &cratonvm_jit::deopt::DeoptimizationPoint = p;
        let is_chain = point.frame_state.caller.is_some();
        point as *const cratonvm_jit::deopt::DeoptimizationPoint as usize == point_addr
            && (point.reason == cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged
                || (opaque_entries
                    && !is_chain
                    && point.reason == cratonvm_jit::deopt::DeoptReason::UncommonTrap))
    });
    if !value_equivalent {
        return false;
    }
    // Each withdrawn key with its body's build stamp: the refusal is about
    // the bytecode that body was built from.
    let mut refused: Vec<((u32, u64, usize), u32)> = Vec::new();
    if let Ok(mut memo) = shared.jit.bridge.osr_optimizing_accepted.lock() {
        for (key, entry) in memo.iter_mut() {
            if !std::ptr::eq(Arc::as_ptr(entry.artifact.arc()), compiled) {
                continue;
            }
            entry.value_equivalent_exits = entry.value_equivalent_exits.saturating_add(1);
            if !entry.withdrawn
                && entry.value_equivalent_exits >= OSR_OPTIMIZING_VALUE_EQUIVALENT_EXIT_LIMIT
            {
                entry.withdrawn = true;
                refused.push((*key, entry.jit_redefine_epoch));
            }
        }
    }
    for (key, judged_at) in refused {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing pc={}: withdrawn -- a value-equivalent guard failed {OSR_OPTIMIZING_VALUE_EQUIVALENT_EXIT_LIMIT} times",
                key.2
            );
        }
        note_osr_optimizing_refusal(shared, key, judged_at);
    }
    true
}



/// Caught-exception exits of one optimizing OSR body, within
/// [`OSR_OPTIMIZING_HANDLER_EXIT_SPAN`], that withdraw it.
pub(in crate::runtime::interpreter::jit_bridge) const OSR_OPTIMIZING_HANDLER_EXIT_BURST: u32 = 64;

/// The window [`OSR_OPTIMIZING_HANDLER_EXIT_BURST`] exits must fall in. 64 in
/// 250 ms is >= 256 exits a second; at the ~10 us one costs (measured on
/// `R16OsrdoorCatch catch-every`, Windows) that is >= 0.25% of the thread
/// spent leaving and re-entering the body, and a loop that throws less often
/// never reaches it.
pub(in crate::runtime::interpreter::jit_bridge) const OSR_OPTIMIZING_HANDLER_EXIT_SPAN: std::time::Duration = std::time::Duration::from_millis(250);

/// Count one exit into `burst` (`(count, start)`) at `now`; `true` when it
/// completes a burst of `limit` exits no longer than `span`. A burst that
/// outlived `span` starts over at this exit.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_handler_exit_burst_completes(
    burst: &mut (u32, Option<std::time::Instant>),
    now: std::time::Instant,
    limit: u32,
    span: std::time::Duration,
) -> bool {
    match burst.1 {
        Some(start) if now.saturating_duration_since(start) <= span => {
            burst.0 = burst.0.saturating_add(1);
        }
        _ => *burst = (1, Some(now)),
    }
    burst.0 >= limit
}

/// Round 16, lane osrdoor: an exception a callee throws and THIS loop catches
/// leaves an optimizing OSR body every time.
///
/// The optimizing graph has no handler in it: a call inside a protected range
/// gets a reason-9 rethrow pad, [`route_osr_exception_out_of_artifact`]
/// transfers the frame to the method's handler in the interpreter, the
/// interpreter runs the `catch` to the next back edge, and `try_osr` enters
/// the body again (`R16ThrowpathLoop callercatch`: 27 028 re-entries in ~6 s).
/// That round trip costs ~10 us per caught exception (`R16OsrdoorCatch`
/// `catch-every` 10 411 ns/op), where the single-pass OSR body catches in
/// compiled code through its local handler stubs (1 322 ns/op, the same run;
/// `finally` 19 710 vs 1 746, `catch-1in16` 594 vs 115).
///
/// So after a burst of such exits ([`OSR_OPTIMIZING_HANDLER_EXIT_BURST`]
/// within [`OSR_OPTIMIZING_HANDLER_EXIT_SPAN`]) every memo entry serving
/// `compiled` is withdrawn and its key refused, exactly as
/// [`note_osr_optimizing_value_equivalent_exit`] withdraws a body whose guard
/// fails on every entry: the loop's next back edge takes the single-pass OSR
/// route. Nothing about what runs changes but the tier: the handler, its
/// `finally` copies and the stack trace (captured in the callee, before any
/// of this) are the method's own, run by the interpreter today and by the
/// single-pass local handler after the fallback. Entries are marked, not
/// removed, so their callee pins outlive a frame still running the body. A
/// body not served from the memo (a single-pass one) matches no entry.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_handler_exit(shared: &SharedVm, compiled: &cratonvm_jit::CompiledMethod) {
    let now = std::time::Instant::now();
    let mut refused: Vec<((u32, u64, usize), u32)> = Vec::new();
    if let Ok(mut memo) = shared.jit.bridge.osr_optimizing_accepted.lock() {
        for (key, entry) in memo.iter_mut() {
            if entry.withdrawn || !std::ptr::eq(Arc::as_ptr(entry.artifact.arc()), compiled) {
                continue;
            }
            if osr_handler_exit_burst_completes(
                &mut entry.handler_exit_burst,
                now,
                OSR_OPTIMIZING_HANDLER_EXIT_BURST,
                OSR_OPTIMIZING_HANDLER_EXIT_SPAN,
            ) {
                entry.withdrawn = true;
                refused.push((*key, entry.jit_redefine_epoch));
            }
        }
    }
    for (key, judged_at) in refused {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing pc={}: withdrawn -- {OSR_OPTIMIZING_HANDLER_EXIT_BURST} caught exceptions left the body within {OSR_OPTIMIZING_HANDLER_EXIT_SPAN:?}; the single-pass OSR body catches locally",
                key.2
            );
        }
        note_osr_optimizing_refusal(shared, key, judged_at);
    }
}

/// Round 11 wave 11 (lane irexc): the pcs one of the method's exception
/// handlers can lead to (`deopt::pcs_reachable_from_handlers` over every
/// row's `handler_pc`); `None` when the code cannot be analysed, which the
/// caller reads as "every pc" (`deopt::pc_reachable_from_handlers`). An
/// optimizing OSR body admitted for its exceptional exits is never entered
/// at such a pc — see `build_osr_optimizing_artifact`.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_handler_reachable_pcs(
    code: &[u8],
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> Option<Vec<u32>> {
    let handlers: Vec<u32> = exception_table
        .iter()
        .map(|row| u32::from(row.handler_pc))
        .collect();
    cratonvm_jit::deopt::pcs_reachable_from_handlers(code, &handlers)
}

/// May the OSR body `body` have folded code of a class that was redefined
/// after the frame stamped `frame_stamp` began (interpreter round i1 wave 25
/// follow-up, lane L6c)? True when a class whose bytecode the body copied --
/// `CompiledMethod::copied_classes`, or named by its `inlined_methods` -- has
/// a redefinition that completed after the stamp
/// (`RedefinitionHistory::latest_redefinition`). Asked by [`try_osr`] before
/// it enters a body; one class-manager read, taken only once some class was
/// redefined and only for a body that copied something.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_body_may_fold_code_the_frame_predates(
    shared: &SharedVm,
    frame_stamp: u64,
    body: &cratonvm_jit::CompiledMethod,
) -> bool {
    if body.copied_classes().is_empty() && body.inlined_methods.is_empty() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    let by_id = body.copied_classes().iter().map(|&id| ClassId::new(id));
    let by_name = body
        .inlined_methods
        .iter()
        .filter_map(|(class_name, _, _)| cm.get_loaded_class_id(class_name));
    class_redefined_after(frame_stamp, by_id.chain(by_name), |class_id| {
        cm.redefinition_history(class_id)
            .map(|history| history.latest_redefinition())
    })
}

/// Does the OSR body `body` run another generation of the frame's class's
/// bytecode and constant pool than `frame` does (interpreter round i1 wave 39,
/// lane L2;
/// `docs/feature-designs/deopt-osr.md`)?
/// Asked by [`try_osr`] on the body it is about to enter, whichever route
/// produced it (the published single-pass body, the re-entry memo, the
/// optimizing memo, a build on the mutator), so no route can pair a frame with
/// a body of another generation.
///
/// Every earlier guard compares BYTES (`AcceptedOsrOptimizing::code`,
/// `same_bytecode`, `runs_current_body`), and a redefinition that changes only
/// a pool constant leaves the bytes equal while an `ldc` of the constant, which
/// every compiled door bakes as an immediate, reads another value. The stamps
/// say which pool each side reads: the frame's `Frame::redefine_stamp` (the
/// pool generation its code indexes; a frame moved onto the current pool
/// carries the class's last redefinition) and the body's
/// `CompiledMethod::compile_cp_stamp` (the count when its compilation began;
/// a body without one counts as the oldest). They name one generation exactly
/// when no redefinition of the class completed between them, i.e. both are at
/// or after its last one. A frame running an obsolete method never enters.
///
/// One relaxed load while no class was ever redefined, and nothing more when
/// the two stamps are equal. Otherwise the redefinition ring answers when it
/// names no redefinition of this class between the stamps; only when it does
/// (or cannot answer) is the class-manager read lock taken for the class's
/// history. HotSpot's old methods lose their compiled code; so does a frame
/// refused here, which keeps interpreting.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_body_generation_differs_from_frame(
    shared: &SharedVm,
    frame: &crate::runtime::frame::Frame,
    body: &cratonvm_jit::CompiledMethod,
) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    if frame.runs_obsolete_method() {
        return true;
    }
    let frame_stamp = frame.redefine_stamp();
    let body_stamp = body.compile_cp_stamp().unwrap_or(0);
    let (older, newer) = (frame_stamp.min(body_stamp), frame_stamp.max(body_stamp));
    if older == newer {
        return false;
    }
    let class_id = frame.class_id;
    let mut names_the_class = false;
    // nio21 wave 5 (L1): this VM's redefinition log, not the process ring.
    let answered = shared
        .classes
        .redefinition_log
        .for_each_between(older, newer, |redefined| {
            names_the_class |= redefined == class_id;
        });
    if answered && !names_the_class {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    stamps_straddle_a_redefinition(
        older,
        cm.redefinition_history(class_id)
            .map(|history| history.latest_redefinition()),
    )
}

/// The decision of [`osr_body_generation_differs_from_frame`] apart from its
/// lookups: `older` is the lesser of the two stamps and `latest` the
/// `class_redefinition_count()` the class's last redefinition completed at
/// (`None` for a class never redefined). The two sides share a generation
/// only when the older one is at or after that redefinition; a pair that
/// both predate it may or may not share one, and is refused (such a body is
/// withdrawn by the redefinition anyway).
pub(in crate::runtime::interpreter::jit_bridge) fn stamps_straddle_a_redefinition(older: u64, latest: Option<u64>) -> bool {
    latest.is_some_and(|latest| older < latest)
}

/// Was any class of `classes` last redefined after `stamp`
/// (`latest_of(class)`: the `class_redefinition_count()` its last
/// redefinition completed at, `None` for a class never redefined)? The
/// decision of [`osr_body_may_fold_code_the_frame_predates`], apart from its
/// lookups.
pub(in crate::runtime::interpreter::jit_bridge) fn class_redefined_after(
    stamp: u64,
    classes: impl IntoIterator<Item = ClassId>,
    latest_of: impl Fn(ClassId) -> Option<u64>,
) -> bool {
    classes
        .into_iter()
        .any(|class_id| latest_of(class_id).is_some_and(|latest| stamp < latest))
}

/// Did an event that makes compiled code obsolete — a class redefinition, a
/// `JitCache::clear_all`, a synthetic layout upgrade, in THIS VM — happen
/// after an optimizing OSR body's compilation began? `stamp` is the VM's
/// code-state epoch read before that compile
/// (`AcceptedOsrOptimizing::code_state_epoch`).
///
/// `JitRealm::code_state_epoch` moves only on those events, so `<` is exactly
/// "one happened since". These bodies bypass `JitCache` (and so its flush
/// barrier and eviction), which is the redefinition half of
/// `i1-L7-osr-optimizing-door-bypasses-osr-admission` item 4. One atomic load.
/// Until interpreter round i1 wave 14 this compared the artifact's process
/// install stamp with the process epoch, so another VM's redefinition or
/// flush made this VM's bodies stale.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_artifact_is_stale(shared: &SharedVm, stamp: u64) -> bool {
    stamp < shared.jit.code_state_epoch()
}

/// Is this optimizing OSR body still current against every dependency
/// invalidation this VM's `JitCache` logged after invalidation epoch `since`?
///
/// The dependency half of `i1-L7-osr-optimizing-door-bypasses-osr-admission`
/// item 4. The body's `inlined_methods` names each callee it inlined and each
/// receiver class a guarded or CHA-bound call relied on; the class-define
/// listener (`vm_init.rs`, `invalidate_for_class_change` over the new class
/// and its supertypes) logs a record per name, and `JitCache` withdraws every
/// PUBLISHED body a record names. These bodies bypass `JitCache`, so the same
/// log is asked here, through the rule `JitCache::put` applies to a
/// publication (`unpublished_body_dependencies_are_current`). `names` is
/// `(class, method, descriptor)` of the method the body was compiled for.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_dependencies_current(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    artifact: &cratonvm_jit::CompiledMethod,
    since: u64,
) -> bool {
    shared
        .jit
        .jit_cache
        .unpublished_body_dependencies_are_current(
            names.0, names.1, names.2, class_id, artifact, since,
        )
}

/// [`osr_optimizing_dependencies_current`] for a body the memo already
/// ACCEPTED and serves (the re-checks in [`osr_optimizing_cached`] and
/// [`osr_optimizing_cached_peek`]): judged as an installed body
/// (`JitCache::installed_body_dependencies_are_current`), so a live-phase
/// object-observer gain (`can_tag_objects`) no longer drops and rebuilds every
/// running optimizing OSR body -- HotSpot fails only compiles in flight at the
/// gain. The build site's first check stays on the publication rule. t1 wave 7
/// lane NBA, page
/// `t1-V6-osr-memo-recheck-refuses-installed-bodies-after-observer-gain-FIXED-20261027.md`.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_installed_dependencies_current(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    artifact: &cratonvm_jit::CompiledMethod,
    since: u64,
) -> bool {
    shared
        .jit
        .jit_cache
        .installed_body_dependencies_are_current(
            names.0, names.1, names.2, class_id, artifact, since,
        )
}



/// The install point of a freshly built optimizing OSR body, called once the
/// build remembered it under every header it serves and before anything
/// enters it.
///
/// The build's dependency check runs before the memo inserts, and every memo
/// re-check judges by the INSTALLED rule (`withdraws_installed_in`, which
/// spares an object-observer gain) from the body's compile-start stamp. So a
/// live-phase `can_tag_objects` landing between that check and the insert was
/// spared, and the building thread entered the body anyway -- a compile in
/// flight at the gain, which HotSpot refuses at install
/// (`ciEnv::jvmti_state_changed`, checked under its lock).
///
/// This re-runs the PUBLICATION rule over every record logged since the
/// compile began. `false`: every memo entry holding `artifact` is forgotten
/// and the caller must not enter it (the next entry rebuilds). `true`: the
/// entries' `dependencies_checked_through` is raised to the epoch read before
/// this check, so their re-checks judge only the records logged after it --
/// installed by then -- by the installed rule. An epoch read here covers every
/// record with that epoch or less: `log_invalidation` advances the epoch and
/// appends the record under the log lock the check takes.
///
/// Residual: another thread can be served the entry from the memo between the
/// insert and this check (microseconds); the entry is forgotten here, but such
/// a thread is already inside.
pub(in crate::runtime::interpreter::jit_bridge) fn settle_osr_optimizing_install(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    artifact: &Arc<cratonvm_jit::CompiledMethod>,
) -> bool {
    let through = cratonvm_jit::jit_invalidation_epoch();
    let current = osr_optimizing_dependencies_current(
        shared,
        class_id,
        names,
        artifact,
        artifact.invalidation_epoch,
    );
    let method = osr_optimizing_refusal_key(shared.vm_identity, class_id, names.1, names.2, 0);
    // Forgotten entries are dropped after the guard, outside the lock.
    let accepted = shared.jit.bridge.osr_optimizing_accepted.lock();
    let forgotten: Vec<AcceptedOsrOptimizing> = match accepted {
        Ok(mut memo) => {
            let mine: Vec<(u32, u64, usize)> = memo
                .iter()
                .filter(|(key, entry)| {
                    key.0 == method.0
                        && key.1 == method.1
                        && Arc::ptr_eq(entry.artifact.arc(), artifact)
                })
                .map(|(key, _)| *key)
                .collect();
            if current {
                for key in &mine {
                    if let Some(entry) = memo.get(key) {
                        entry
                            .dependencies_checked_through
                            .fetch_max(through, std::sync::atomic::Ordering::AcqRel);
                    }
                }
                Vec::new()
            } else {
                mine.iter().filter_map(|key| memo.remove(key)).collect()
            }
        }
        Err(_) => Vec::new(),
    };
    // Cast: a count of map entries fits u64.
    note_osr_optimizing_memo_change(0, forgotten.len() as u64);
    drop(forgotten);
    current
}

/// Keep an accepted optimizing OSR artifact, with its callee pins, for the next
/// entry at this pc.
pub(in crate::runtime::interpreter::jit_bridge) fn remember_osr_optimizing_artifact(
    shared: &SharedVm,
    key: (u32, u64, usize),
    artifact: &Arc<cratonvm_jit::CompiledMethod>,
    callee_pins: &Arc<[cratonvm_jit::RetainedCode]>,
    stamps: OsrBuildStamps,
    code: &Arc<[u8]>,
    exits_admitted: bool,
) {
    if osr_optimizing_cache_disabled() {
        return;
    }
    let entry = AcceptedOsrOptimizing {
        artifact: cratonvm_jit::RetainedCode::new(Arc::clone(artifact)),
        _callee_pins: Arc::clone(callee_pins),
        redefine_generation: stamps.redefine_generation,
        code_state_epoch: stamps.code_state_epoch,
        jit_redefine_epoch: stamps.jit_redefine_epoch,
        code: Arc::clone(code),
        exits_admitted,
        value_equivalent_exits: 0,
        handler_exit_burst: (0, None),
        withdrawn: false,
        dependencies_checked_through: std::sync::atomic::AtomicU64::new(
            artifact.invalidation_epoch,
        ),
        first_call_cells_at_build: armed_first_call_cells_of(shared, key.0),
        static_init_pending_at_build: static_init_pending_of(shared, artifact),
        poly_decline_unasked: false,
    };
    // Interpreter round i1 wave 10 (item 4 of
    // `osr-optimizing-door-bypasses-osr-admission`, narrowed): an entry older
    // than the last code-state epoch move is never served again
    // (`osr_optimizing_cached` misses on it), but was forgotten only when its
    // OWN key was looked up — so after a flush or a redefinition every body of
    // a loop that never ran again kept its code and callee pins for the life of
    // the VM. Purged here, once per epoch move (one atomic swap otherwise); an
    // insert follows a compile, which dwarfs the scan.
    let now = shared.jit.code_state_epoch();
    let purge = shared
        .jit
        .bridge
        .osr_optimizing_purged_code_state_epoch
        .swap(now, std::sync::atomic::Ordering::AcqRel)
        != now;
    // A replaced or purged entry is dropped after the guard, outside the lock;
    // `RetainedCode` releases through the retirement queue.
    let (replaced, purged) = match shared.jit.bridge.osr_optimizing_accepted.lock() {
        Ok(mut m) => {
            let purged: Vec<AcceptedOsrOptimizing> = if purge {
                let stale: Vec<(u32, u64, usize)> = m
                    .iter()
                    .filter(|(_, e)| e.code_state_epoch < now)
                    .map(|(k, _)| *k)
                    .collect();
                stale.iter().filter_map(|k| m.remove(k)).collect()
            } else {
                Vec::new()
            };
            (m.insert(key, entry), purged)
        }
        Err(_) => (None, Vec::new()),
    };
    // Cast: a count of map entries fits u64.
    note_osr_optimizing_memo_change(1, u64::from(replaced.is_some()) + purged.len() as u64);
    // t1 wave 2, lane jit2 (L1W1-4 census of `docs/feature-designs/jit-ir-optimizations.md`): a
    // DIFFERENT artifact replacing this key's entry (an acceptance re-offer, a
    // de-spec rebuild, a late-bind rebuild) is the event a profit exit for a
    // superseded optimizing OSR body would serve -- but only while a frame
    // still runs the old artifact. `old_strong` counts the old artifact's other
    // holders after this entry lets go: each live continuation running it
    // holds one (`osr_optimizing_cached` hands the door a clone for the whole
    // activation), and so does each other header key it is remembered under,
    // so `old_strong` above that header count is a running frame. Diagnostic
    // only (`CRATONVM_DBG_JITC`); nothing is armed here.
    if let Some(old) = replaced.as_ref() {
        if !Arc::ptr_eq(old.artifact.arc(), artifact) && crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr-optimizing memo REPLACED pc={} old_id={} new_id={} old_strong={} old_headers={}",
                key.2,
                old.artifact.arc().compile_id,
                artifact.compile_id,
                Arc::strong_count(old.artifact.arc()).saturating_sub(1),
                old.artifact.arc().ir_osr_entries.len()
            );
        }
    }
    drop(replaced);
    drop(purged);
}

/// `(remembered, forgotten)` accepted optimizing OSR memo entries since start,
/// over every VM. Diagnostic only (the `CRATONVM_DBG_JITC` exit census): their
/// difference is the population item 4 of
/// `osr-optimizing-door-bypasses-osr-admission` would bound, measured before
/// any cap is chosen.
pub(in crate::runtime::interpreter::jit_bridge) static OSR_OPTIMIZING_MEMO_REMEMBERED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::jit_bridge) static OSR_OPTIMIZING_MEMO_FORGOTTEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_memo_change(remembered: u64, forgotten: u64) {
    if remembered != 0 {
        OSR_OPTIMIZING_MEMO_REMEMBERED.fetch_add(remembered, std::sync::atomic::Ordering::Relaxed);
    }
    if forgotten != 0 {
        OSR_OPTIMIZING_MEMO_FORGOTTEN.fetch_add(forgotten, std::sync::atomic::Ordering::Relaxed);
    }
}

/// See [`OSR_OPTIMIZING_MEMO_REMEMBERED`].
pub fn osr_optimizing_memo_census() -> (u64, u64) {
    (
        OSR_OPTIMIZING_MEMO_REMEMBERED.load(std::sync::atomic::Ordering::Relaxed),
        OSR_OPTIMIZING_MEMO_FORGOTTEN.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `CRATONVM_JIT_OSR_OPTIMIZING_CACHE=0` restores the pre-cache behaviour —
/// rebuild the artifact on every entry — so the two arms can be measured from
/// ONE binary. Comparing an intermittent event across two builds is not a
/// comparison; this is the same argument `CRATONVM_JIT_OSR_OPTIMIZING_MEMO`
/// makes for the refusal memo, and the same spelling.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_cache_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// Record a refusal so the next attempt at this pc skips straight to
/// single-pass.
///
/// `judged_at` is this VM's `JitCache::redefine_epoch` read BEFORE the
/// judgement began — before the build ([`OsrBuildStamps::jit_redefine_epoch`])
/// or the policy scan ([`osr_optimizing_build_declined`]). A refusal counts
/// only while that epoch is current ([`osr_optimizing_already_refused`]), so a
/// verdict about bytecode a redefinition replaced mid-judgement is born stale
/// instead of refusing the new bytecode until the next redefinition, which
/// stamping at record time did (interpreter round i1 wave 15, lane L4).
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_refusal(shared: &SharedVm, key: (u32, u64, usize), judged_at: u32) {
    if let Ok(mut s) = shared.jit.bridge.osr_optimizing_refused.lock() {
        // A late verdict about older bytecode never replaces a current one.
        let stamp = s.entry(key).or_insert(judged_at);
        *stamp = (*stamp).max(judged_at);
    }
}

/// The optimizing OSR route's policy BEFORE it builds a body for `key`:
/// `true` means "do not build here", and the single-pass body answers.
///
/// Asked by both builders — `try_osr`'s compile arm and the background
/// worker's OSR task ([`background_osr_optimizing_body`], round 11 wave 9) — so
/// the two cannot disagree about which methods the route may take. Every
/// verdict reached here is kept in the refusal memo, so this runs once per
/// `(method, pc)`. `door` labels the trace lines (`""` for `try_osr`, whose
/// lines are unchanged).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_build_declined(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    key: (u32, u64, usize),
    entry_pc: usize,
    code: &[u8],
    door: &str,
) -> bool {
    // The stamp of any refusal recorded below, read before the judgement
    // (see `note_osr_optimizing_refusal`).
    let judged_at = shared.jit.jit_cache.redefine_epoch();
    if osr_optimizing_already_refused(shared, key)
        || osr_optimizing_already_refused(shared, osr_optimizing_method_wide_key(key))
    {
        // Already built and refused, or already judged not worth building
        // (the arms below) — at this pc, or for the whole method.
        return true;
    }
    if osr_optimizing_runtime_despeculated(
        shared,
        class_id,
        class_name,
        method_name,
        method_descriptor,
    ) {
        // The route's compile is admitted at the method-entry door, which
        // refuses a method this VM de-speculated at run time
        // (`compile_gate::admit`'s `RuntimeDespeculated`). That verdict
        // expires with a code-cache flush or a redefinition, so it is asked
        // here each time and NOT memoised: the memo used to keep the refusal
        // until the next redefinition (interpreter round i1 wave 38, lane L2;
        // item 2 of the wave-37 compile-door review). One read-locked empty
        // check in the common case.
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: not attempted -- runtime de-speculated (transient, not memoised)"
            );
        }
        return true;
    }
    if osr_prefer_single_pass_enabled()
        && osr_single_pass_preferred(code, osr_optimizing_exception_exits_enabled())
    {
        // Admission prefers the single-pass body where the optimizing one is
        // measured slower (see `osr_optimizing_not_known_better`), and only
        // where that measurement applies (`osr_single_pass_preferred`). Kept in the
        // refusal memo, so the scan runs once per method and the next entry
        // skips straight to the single-pass artifact -- no optimizing compile
        // is paid for a body that would not be entered. A whole-method scan,
        // so a whole-method verdict.
        note_osr_optimizing_refusal(shared, osr_optimizing_method_wide_key(key), judged_at);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: not attempted -- single-pass preferred (ldiv/lrem/lcmp; CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 opts out)"
            );
        }
        return true;
    }
    if let Some(gate) =
        osr_static_gate_refusal(shared, class_id, class_name, method_name, method_descriptor)
    {
        // Round 11 wave 2: the single-pass door's static gates, asked of this
        // route too before it compiles (`compile_optimizing_artifact` carries
        // no admission policy). Asked on the compile arm only, so a cached
        // accepted body pays nothing per entry; a body is cached only after
        // passing them. Memoised as a refusal of the whole method — the gates
        // are method properties — so this runs once per method; the
        // single-pass door then refuses the same way.
        note_osr_optimizing_refusal(shared, osr_optimizing_method_wide_key(key), judged_at);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused by the OSR gate `{}`",
                gate.census
            );
        }
        return true;
    }
    if !crate::runtime::env_cache::osr_athrow_allowed() && bytecode_has_athrow(code) {
        // Round 12 wave 1 (lane osr): `CRATONVM_JIT_OSR_ATHROW=0` is documented
        // as restoring the blanket RBC.6 refusal of every `athrow` method, but
        // only the single-pass door (`compile_osr_body`) asked it. This route
        // runs FIRST (`try_osr`, and the worker's OSR task), and since the
        // exception exits went default-on it admits such a method on its own,
        // so the kill switch did not stop OSR for it. A lever, fixed for the
        // process, of the bytecode alone: a whole-method verdict.
        note_osr_optimizing_refusal(shared, osr_optimizing_method_wide_key(key), judged_at);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused -- RBC.6 athrow, CRATONVM_JIT_OSR_ATHROW=0"
            );
        }
        return true;
    }
    // The single-pass door's other two OSR bisect levers, which this route
    // did not ask either, so it OSR-compiled the very methods an arm with the
    // lever off was meant to keep interpreted (interpreter round i1 wave 38,
    // lane L2; item 8 of the wave-37 compile-door review). Levers fixed for
    // the process, of the method alone: whole-method verdicts. Asked only with
    // a lever off.
    if !crate::runtime::env_cache::osr_newarray_allowed() && bytecode_has_opcode(code, 0xbc) {
        note_osr_optimizing_refusal(shared, osr_optimizing_method_wide_key(key), judged_at);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused -- newarray, CRATONVM_OSR_NEWARRAY=0"
            );
        }
        return true;
    }
    if false
        && method_has_exception_table(shared, class_id, method_name, method_descriptor)
    {
        note_osr_optimizing_refusal(shared, osr_optimizing_method_wide_key(key), judged_at);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused -- exception table, CRATONVM_JIT_OSR_EXC_TABLE=0"
            );
        }
        return true;
    }
    // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-4 / OSRFOLLOW18-4): the
    // polymorphic-loop decline of `CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS`,
    // asked before the compile (no single-pass method body is compiled and
    // dropped) and answered by the single-pass OSR door's own planner, so it
    // declines only where that door really splices the site polymorphically.
    // The method-wide verdict the in-compile decline it replaces ended in
    // (`build_osr_optimizing_artifact`'s "judged no optimizing body" arm). See
    // `cratonvm_jit::osr_door_single_pass_polymorphic_splice_pc`.
    if let Some(pc) = osr_door_prefers_spliced_single_pass_early(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        code,
        entry_pc,
    ) {
        // JIT round 19 wave 2 (lane osr2, OSR19-1): under
        // `CRATONVM_JIT_OSR_POLY_DECLINE_PER_HEADER` the answer is about the
        // entry's loop (the jit function filters the site to it), so it is
        // memoised for this header only and another loop asks again.
        let memo_key = if cratonvm_jit::osr_poly_decline_per_header_enabled() {
            key
        } else {
            osr_optimizing_method_wide_key(key)
        };
        note_osr_optimizing_refusal(shared, memo_key, judged_at);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: not attempted -- the single-pass OSR body splices the polymorphic site at pc={pc} (CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS=0 opts out)"
            );
        }
        return true;
    }
    // JIT round 19 proposal wave 3 (lane megaosr, MEGAOSR19-1,
    // `CRATONVM_JIT_OSR_DECLINE_BLIND_MEGA_SPLICE`, default ON since the round-19 proposal round): the entry's loop
    // calls a statically bound body whose own receiver profile calls one of its
    // virtual / interface sites megamorphic. Spliced into this body that site
    // would start blind (an unseeded inline cache), so the single-pass OSR body
    // runs the loop, as under `CRATONVM_JIT_OSR_OPTIMIZING=0`. About this
    // header's loop only, so memoised under the entry's own key.
    if let Some((call_pc, callee_pc)) = osr_door_blind_megamorphic_splice_early(
        shared,
        class_id,
        (method_name, method_descriptor),
        code,
        entry_pc,
    ) {
        note_osr_optimizing_refusal(shared, key, judged_at);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: not attempted -- the loop's call at pc={call_pc} splices a body whose own profile calls its site at pc={callee_pc} megamorphic; spliced, that site would be blind (CRATONVM_JIT_OSR_DECLINE_BLIND_MEGA_SPLICE)"
            );
        }
        return true;
    }
    false
}

/// MEGAOSR19-1 (JIT round 19 proposal wave 3, lane megaosr): the VM half of
/// `cratonvm_jit::osr_door_blind_megamorphic_splice_site`, with this door's
/// inputs: the method's own `invokestatic` / `invokespecial` sites named
/// through the constant pool (not a `<init>()V`), the optimizing tier's splice
/// resolver ([`resolve_ir_inline_site`], without a direct binder, so asking
/// compiles nothing), and the profile store's profile of each resolved body.
/// `None` without the switch (checked first, so the default arm pays one flag
/// read per build verdict), with IR inlining off (nothing would be spliced), or
/// when no such site exists.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_door_blind_megamorphic_splice_early(
    shared: &SharedVm,
    class_id: ClassId,
    method: (&str, &str),
    code: &[u8],
    entry_pc: usize,
) -> Option<(usize, usize)> {
    if !cratonvm_jit::osr_decline_blind_mega_splice_enabled() || !cratonvm_jit::ir_inline_enabled()
    {
        return None;
    }
    let ops = cratonvm_jit::osr_door_own_invoke_ops(code);
    if !ops.iter().any(|&(_, _, op)| matches!(op, 0xb7 | 0xb8)) {
        return None;
    }
    let cp = CpResolvers {
        shared,
        class_id,
        substituted_owners: SubstitutedOwners::default(),
        method_name: method.0,
        method_descriptor: method.1,
        code,
        ldc2w_trace: None,
        invoke_operands: Default::default(),
        first_calls_seen: Default::default(),
        deferred_fields_seen: Default::default(),
    };
    let named: Vec<(usize, String, String, String, u8)> = ops
        .into_iter()
        .filter(|&(_, _, op)| matches!(op, 0xb7 | 0xb8))
        .filter_map(|(pc, cp_idx, opcode)| {
            let (cn, mn, desc) = cp.invoke(cp_idx)?;
            if mn == "<init>"
                && desc == "()V"
                && osr_door_estimate_skips_noarg_ctor(shared, class_id, &cn)
            {
                return None;
            }
            Some((pc, cn, mn, desc, if opcode == 0xb8 { 3u8 } else { 1 }))
        })
        .collect();
    let statics: Vec<cratonvm_jit::OsrSpliceCandidate<'_>> = named
        .iter()
        .map(|row| cratonvm_jit::OsrSpliceCandidate {
            pc: row.0,
            invoke_kind: row.4,
            class_name: row.1.as_str(),
            method_name: row.2.as_str(),
            descriptor: row.3.as_str(),
        })
        .collect();
    let static_resolver = |callee_class: &str,
                           callee_method: &str,
                           callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_ir_inline_site(
            shared,
            class_id,
            callee_class,
            callee_method,
            callee_desc,
            None,
        )
    };
    let callee_profile = |callee_class_id: u32,
                          callee_method: &str,
                          callee_desc: &str|
     -> Option<cratonvm_jit::profile::MethodProfile> {
        let get = |id: u32| {
            shared
                .jit
                .profile_store
                .get_profile(&crate::jit::profile::MethodKey {
                    class_id: id,
                    method_name: callee_method.into(),
                    descriptor: callee_desc.into(),
                })
        };
        let found = get(callee_class_id);
        if found.is_some() || !cratonvm_jit::osr_decline_blind_mega_declaring_key_enabled() {
            return found;
        }
        // JIT round 19 proposal wave 5 (lane mega5): `callee_class_id` is
        // `InlineSite::class_id`, the CONSTANT-POOL class of an unguarded
        // resolution, while the interpreter records under the declaring
        // class: an inherited static (`Sub.m()` declared in `Super`) has
        // its profile under `Super`. The class-manager guard is dropped
        // before the profile store is asked.
        let declaring = {
            let cm = shared.classes.class_manager.read();
            crate::classloading::find_method_recursive(
                ClassId::new(callee_class_id),
                callee_method,
                callee_desc,
                cm.class_store(),
            )
            .map(|(_, declaring_id)| declaring_id.as_u32())
        };
        declaring
            .filter(|&id| id != 0 && id != callee_class_id)
            .and_then(get)
    };
    cratonvm_jit::osr_door_blind_megamorphic_splice_site(
        code,
        // The padded bytecode carries two trailing zero bytes, as
        // `osr_door_prefers_spliced_single_pass_early` assumes.
        code.len().saturating_sub(2),
        entry_pc,
        &statics,
        &static_resolver,
        &callee_profile,
    )
}

/// STR719-1 (JIT round 19 proposal wave 8, lane str8;
/// `CRATONVM_JIT_OSR_DOOR_NOARG_CTOR_CANDIDATES`, default on): do the two OSR door
/// estimators ([`osr_door_blind_megamorphic_splice_early`],
/// [`osr_door_prefers_spliced_single_pass_early`]) leave this `invokespecial
/// C.<init>()V` site out of their static candidates?
///
/// Both left out EVERY no-argument constructor, on the premise that the
/// single-pass OSR door "elides or calls" one. That premise is stale: since the
/// OSR ctor bind (`CRATONVM_NO_OSR_CTOR_BIND`), the door's deferred ctor pass
/// sends a constructor `is_elidable_construction` does NOT prove empty to
/// `pending_callee_compiles`, so it is a splice candidate of
/// `plan_osr_door_splices` exactly like `C.<init>(I)V`. With the estimators
/// blind to it, `new StringBuilder()` and `new StringBuilder(16)` -- the same
/// JDK body, `super(16)` vs `super(capacity)` -- could be judged differently
/// by the door choice (`R19Str8Rows sb-new-*`). Under the switch only a
/// constructor the door's own rule proves elidable is left out (the door
/// emits it as `Object.<init>` and splices nothing); an unresolvable class is
/// not elidable there either. Off: every `<init>()V` is left out, unchanged.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_door_estimate_skips_noarg_ctor(shared: &SharedVm, class_id: ClassId, target: &str) -> bool {
    resolve_cp_class_for_owner(shared, class_id, target)
        .map(|tid| {
            let cm = shared.classes.class_manager.read();
            is_elidable_construction(shared, &cm, tid)
        })
        .unwrap_or(false)
}

/// OSRFOLLOW18-4 (JIT round 18 wave 11, lane osrfollow): the single-pass OSR
/// door's splice planning (`compile_osr_body`: `plan_osr_door_splices`, then
/// `plan_osr_door_guarded_splices`) asked for its polymorphic answer alone,
/// with that door's inputs: the profile store's profile, the method's
/// exception table (non-empty means precise frames, which plans nothing), the
/// entry pc (hot pricing), the class namer, `resolve_receiver_inline_site` and
/// `resolve_inline_site` without a direct binder (the door's static binder
/// compiles callees; without it a call-carrying static body is refused, so
/// the static pass can only spend LESS budget here), and the candidates read
/// from the bytecode through the constant pool: `invokestatic` /
/// `invokespecial` (not a `<init>()V`, which that door elides or calls) as
/// static candidates, `invokevirtual` / `invokeinterface` as guarded ones when
/// that door plans guarded splices at all
/// (`osr_door_guarded_splices_enabled` and `jit_guarded_virtual_inline`).
///
/// Residual differences from `compile_osr_body`'s own lists, each making a
/// site a candidate here that the door handles another way: a site that door
/// binds to an intrinsic or a thin helper first (String, `HashMap`,
/// `VarHandle`, boxing), and an `invokevirtual` it pins to kind 1 (a private
/// or final target). A polymorphic receiver profile at such a site is rare;
/// the cheaper superset (`osr_door_lost_polymorphic_splice_pc`) runs first
/// inside the jit function and skips everything for a method without one.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_door_prefers_spliced_single_pass_early(
    shared: &SharedVm,
    class_id: ClassId,
    enclosing: (&str, &str, &str),
    code: &[u8],
    entry_pc: usize,
) -> Option<usize> {
    let (class_name, method_name, method_descriptor) = enclosing;
    let profile = {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: class_id.as_u32(),
            method_name: method_name.into(),
            descriptor: method_descriptor.into(),
        };
        shared.jit.profile_store.get_profile(&profile_key)
    };
    if profile.is_none() {
        return None;
    }
    let exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry> = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(class_id)
            .and_then(|class| class.find_method(method_name, method_descriptor))
            .and_then(|method| method.code())
            .map(|code| code.exception_table.clone())
            .unwrap_or_default()
    };
    let cp = CpResolvers {
        shared,
        class_id,
        substituted_owners: SubstitutedOwners::default(),
        method_name,
        method_descriptor,
        code,
        ldc2w_trace: None,
        invoke_operands: Default::default(),
        first_calls_seen: Default::default(),
        deferred_fields_seen: Default::default(),
    };
    // `(pc, class, method, descriptor, invoke kind)` of every own invoke the
    // constant pool names.
    let named: Vec<(usize, String, String, String, u8)> =
        cratonvm_jit::osr_door_own_invoke_ops(code)
            .into_iter()
            .filter_map(|(pc, cp_idx, opcode)| {
                let (cn, mn, desc) = cp.invoke(cp_idx)?;
                let kind = match opcode {
                    0xb6 => 0u8,
                    0xb7 => 1,
                    0xb8 => 3,
                    _ => 2,
                };
                Some((pc, cn, mn, desc, kind))
            })
            .collect();
    // A fn item, not a closure: its output borrows its input.
    fn as_candidate(
        row: &(usize, String, String, String, u8),
    ) -> cratonvm_jit::OsrSpliceCandidate<'_> {
        cratonvm_jit::OsrSpliceCandidate {
            pc: row.0,
            invoke_kind: row.4,
            class_name: row.1.as_str(),
            method_name: row.2.as_str(),
            descriptor: row.3.as_str(),
        }
    }
    let statics: Vec<cratonvm_jit::OsrSpliceCandidate<'_>> = named
        .iter()
        .filter(|row| {
            matches!(row.4, 1 | 3)
                && !(row.2 == "<init>"
                    && row.3 == "()V"
                    && osr_door_estimate_skips_noarg_ctor(shared, class_id, &row.1))
        })
        .map(as_candidate)
        .collect();
    let guarded: Vec<cratonvm_jit::OsrSpliceCandidate<'_>> =
        if cratonvm_jit::osr_door_guarded_splices_enabled()
            && crate::runtime::env_cache::jit_guarded_virtual_inline()
        {
            named
                .iter()
                .filter(|row| matches!(row.4, 0 | 2))
                .map(as_candidate)
                .collect()
        } else {
            Vec::new()
        };
    if guarded.is_empty() {
        return None;
    }
    let class_id_namer = |namer_cid: u32| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cratonvm_types::ClassId::new(namer_cid))
            .map(|c| c.name.to_string())
    };
    let receiver_resolver = |rcv_cid: u32,
                             cp_class: &str,
                             name: &str,
                             desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_receiver_inline_site(shared, class_id, rcv_cid, cp_class, name, desc, None)
    };
    let static_resolver = |callee_class: &str,
                           callee_method: &str,
                           callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_inline_site(
            shared,
            class_id,
            callee_class,
            callee_method,
            callee_desc,
            None,
            None,
        )
    };
    cratonvm_jit::osr_door_single_pass_polymorphic_splice_pc(
        code,
        // The padded bytecode carries two trailing zero bytes, as
        // `compile_osr_body`'s `code_len` assumes.
        code.len().saturating_sub(2),
        (class_name, method_name, method_descriptor),
        &exception_table,
        profile.as_ref(),
        Some(entry_pc),
        &statics,
        &guarded,
        &static_resolver,
        &class_id_namer,
        &receiver_resolver,
    )
}

/// Does `class_id`'s method `method_name method_descriptor` have a non-empty
/// exception table? Takes the class-manager read lock; `false` for a method
/// that is not found.
pub(in crate::runtime::interpreter::jit_bridge) fn method_has_exception_table(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    let cm = shared.classes.class_manager.read();
    cm.get_class(class_id)
        .and_then(|class| class.find_method(method_name, method_descriptor))
        .and_then(|method| method.code())
        .is_some_and(|code| !code.exception_table.is_empty())
}

/// Has this VM de-speculated `class_id.method` at run time
/// (`JitRealm::runtime_despec`), so that the method-entry admission the
/// optimizing OSR route's compile goes through refuses it? Transient: the
/// verdict expires with a code-cache flush or a redefinition.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_runtime_despeculated(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    shared.jit.runtime_despec.is_not_compilable(
        class_id,
        class_name,
        method_name,
        method_descriptor,
    )
}

/// Was an optimizing OSR build that came back empty refused for a reason that
/// expires (interpreter round i1 wave 38, lane L2; item 2 of the wave-37
/// compile-door review)? The code-cache cap (`CodeCacheAtCapacity`, cleared by
/// reclamation) and a run-time de-speculation (cleared by a flush or a
/// redefinition). Such a refusal is not memoised: the method-wide memo is
/// judged by the redefine epoch alone, so it kept the method on its
/// single-pass OSR body until the next redefinition, where the single-pass
/// door and the method-entry door retry.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_refusal_is_transient(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    cratonvm_jit::jit_code_cache_under_pressure()
        || osr_optimizing_runtime_despeculated(
            shared,
            class_id,
            class_name,
            method_name,
            method_descriptor,
        )
}

/// How many declined optimizing OSR compiles of one method, each on a
/// constant-pool resolver miss, the route leaves unmemoised under
/// `CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY` before it memoises the
/// refusal as before (the eager door's bound, `EAGER_RESOLVER_MISS_RETRIES`,
/// for the same reason: the interpreted loop resolves the operand, so the
/// next attempt normally compiles).
pub(in crate::runtime::interpreter::jit_bridge) const OSR_OPTIMIZING_RESOLVER_MISS_RETRIES: u8 = 4;

/// The refusal sites at which the single-pass pipeline found a constant-pool
/// operand its resolver could not name WITHOUT loading (the resolver answered
/// `None`; `jit/src/lib.rs`'s `jitc_bail!` sites): "waiting on a class" in the
/// terms of `i39-L2-proposal-a-compile-request-returns-why-it-produced-no-body`.
/// A missing resolver (`cp_*_resolver`) is a property of the door, an
/// `*_unwired` site of the helper table, and neither is listed.
pub(in crate::runtime::interpreter::jit_bridge) const RESOLVER_MISS_BAIL_SITES: &[&str] = &[
    "field_resolve",
    "static_field_resolve",
    "invoke_resolve",
    "new_resolve",
    "anewarray_resolve",
    "typecheck_class",
    "multianewarray_class",
    "indy_descriptor_resolve",
    // Interpreter round i6 wave 115, lane L2 (page `i107-L2`): the aarch64
    // block's transient bail on a field site whose owner is not loaded (or,
    // for a static, not initialized) yet
    // (`CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`); never recorded elsewhere.
    "arm64_cold_field_site",
];

/// Is `reason` (`JitVerdictRegistry::bail_reason_for`: a site, optionally
/// followed by `(pc=..,op=..)`) one of [`RESOLVER_MISS_BAIL_SITES`]?
pub(in crate::runtime::interpreter::jit_bridge) fn bail_reason_is_resolver_miss(reason: &str) -> bool {
    let site = reason.split('(').next().unwrap_or(reason);
    RESOLVER_MISS_BAIL_SITES.contains(&site)
}


/// Did the optimizing OSR route's compile that just declined (on this
/// thread) stop at a constant-pool resolver miss? Read from the bail reason
/// `try_compile_request` records for every declined compile of the method
/// (`JitVerdictRegistry::record_bail_reason`). A stale reason from an older
/// compile can misclassify one decline; the retry bound keeps that bounded
/// (interpreter round i5 wave 101, lane L2).
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_refusal_is_resolver_miss(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    jit_verdicts(shared)
        .bail_reason_for(class_id, class_name, method_name, method_descriptor)
        .is_some_and(|reason| bail_reason_is_resolver_miss(&reason))
}

/// `CRATONVM_JIT_BG_RESOLVER_MISS_WAITS_ON_CLASS` (default OFF; token
/// `jit-bg-resolver-miss-waits-on-class`; interpreter round i6 wave 116, lane
/// L2, the background door's slice of
/// `i39-L2-proposal-a-compile-request-returns-why-it-produced-no-body`): a
/// background compile that declined at a constant-pool resolver miss is parked
/// by the tiered manager until a class is defined
/// (`tiered::note_compile_waiting_on_class`), instead of being charged to the
/// method's `MAX_TIER_FAIL_RETRIES` budget, three of which settled the method
/// interpreted for good although the interpreted calls load the class.
/// Performance only. Read per unpublished background task (cold). Also
/// installs the manager's class-epoch source (`vm_init.rs`).
pub(crate) fn bg_resolver_miss_waits_on_class_enabled() -> bool {
    false
}

/// Did the background compile of `key` that just declined on this thread stop
/// at a constant-pool resolver miss? Read from the bail reason
/// `try_compile_request` records for every declined compile, as the
/// optimizing OSR route reads it ([`osr_optimizing_refusal_is_resolver_miss`]);
/// a stale reason can misclassify one decline, which the manager's park bound
/// (`tiered::MAX_WAITING_ON_CLASS_PARKS`) keeps bounded.
pub(in crate::runtime::interpreter::jit_bridge) fn background_refusal_is_resolver_miss(
    shared: &SharedVm,
    key: &crate::jit::tiered::MethodKey,
) -> bool {
    osr_optimizing_refusal_is_resolver_miss(
        shared,
        key.class_id,
        &key.class_name,
        &key.method_name,
        &key.descriptor,
    )
}

/// Does `code` contain an `athrow` (0xbf) instruction? Walked by instruction,
/// so an operand byte of 0xbf is not one.
pub(in crate::runtime::interpreter::jit_bridge) fn bytecode_has_athrow(code: &[u8]) -> bool {
    bytecode_has_opcode(code, 0xbf)
}

/// Does `code` contain an instruction whose opcode is `opcode`? Walked by
/// instruction, so an operand byte of that value is not one.
pub(in crate::runtime::interpreter::jit_bridge) fn bytecode_has_opcode(code: &[u8], opcode: u8) -> bool {
    let mut pc = 0usize;
    while let Some(&op) = code.get(pc) {
        if op == opcode {
            return true;
        }
        // `max(1)`: a walk that must terminate does not lean on the decoder
        // stepping an undecodable byte.
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    false
}

/// Build the optimizing OSR body for `cached`, judge it for an entry at
/// `entry_pc`, and remember the verdict: the accepted artifact with its callee
/// pins, or `None` after recording a refusal (or, for a bytecode mismatch,
/// recording nothing — that is a property of the asker, not of the method).
///
/// The one builder behind both doors: `try_osr`'s compile arm and the
/// background worker's OSR task ([`background_osr_optimizing_body`], round 11
/// wave 9). The caller has already asked [`osr_optimizing_build_declined`].
/// `code` is the bytecode the entering frame runs (the worker: the class's
/// current bytecode, which is what a frame reaching the mutator's lookup must
/// run too — `osr_optimizing_cached` compares it).
#[allow(clippy::too_many_arguments)]
/// Round 13 wave 8 (lane codecache): is every direct-call target baked into
/// `cm` owned by one of `pins` -- the very artifact the bake recorded, when it
/// recorded one (`_direct_callee_expected`; `0` means none was recorded)?
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_baked_callees_pinned(
    cm: &cratonvm_jit::CompiledMethod,
    pins: &[cratonvm_jit::RetainedCode],
) -> bool {
    cm._direct_callee_entries.iter().all(|&entry| {
        let want = cm
            ._direct_callee_expected
            .iter()
            .find(|(e, _)| *e == entry)
            .map_or(0, |&(_, id)| id);
        pins.iter().any(|p| {
            // Cast: JIT entry point to address, compared with a baked entry.
            p.entry_ptr() as usize == entry && (want == 0 || p.artifact_id == want)
        })
    })
}

/// `CRATONVM_JIT_OSR_UNENTERABLE_PROMOTE`, **default OFF** (round 15 wave 5,
/// lane osrtier, OT-1 of `jit-r15w5-osrtier-proposals-RETIRED-20261003.md`; `=1` opts in): see
/// [`promote_unenterable_osr_method`]. Off until a gcpbench A/B shows the
/// promoted body serving the method's later calls. Not memoised: asked once
/// per refused optimizing OSR build.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_unenterable_promote_enabled() -> bool {
    false
}

/// OT-1: the optimizing OSR body of `cached` was ACCEPTED by the evidence gate
/// but the door cannot enter it (no entry stub at the header, or none of its
/// exits admitted), so every later activation of the method re-enters the
/// single-pass OSR body. Ask for a METHOD-ENTRY optimizing compile instead,
/// whose body those activations then run from their first bytecode; the same
/// graph was just judged worth publishing. Nothing when the switch is off, the
/// gate did not accept the body, or an optimizing method-entry body is already
/// published. Bounded by the door's refusal memo (one build per header per
/// redefinition epoch) and by the manager's C2-retry gates.
pub(in crate::runtime::interpreter::jit_bridge) fn promote_unenterable_osr_method(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    accepted: bool,
    door: &str,
) {
    if !accepted || !osr_unenterable_promote_enabled() {
        return;
    }
    let optimizing_entry_published = shared
        .jit
        .jit_cache
        .get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        )
        .is_some_and(|body| body.used_ir_backend);
    if optimizing_entry_published {
        return;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr optimizing{door} {}.{}{}: accepted but unenterable -- requesting a method-entry optimizing compile",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
    shared.jit.tiered_manager.request_osr_unenterable_promotion(
        &crate::jit::tiered::MethodKey::with_class_id(
            cached.declaring_class_id,
            &*cached.class_name,
            &*cached.method_name,
            &*cached.method_descriptor,
        ),
    );
}

/// nio21 wave 18, lane L4: the `CRATONVM_DBG_JIT_COMPILED` line of the
/// optimizing OSR door ([`build_osr_optimizing_artifact`]), whose bodies are
/// never published through `JitCache` and so never reached `JitCache::put` /
/// `put_osr`'s line. Diagnostics only; the format is
/// [`osr_optimizing_jit_compiled_line`]'s.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_report_jit_compiled(
    tag: &str,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    entry_pc: usize,
    what: &str,
) {
    if false {
        eprintln!(
            "{}",
            osr_optimizing_jit_compiled_line(
                tag,
                class_name,
                method_name,
                method_descriptor,
                entry_pc,
                what
            )
        );
    }
}

/// The text of [`osr_optimizing_report_jit_compiled`]'s line. Its tag is never
/// `put` or `osr`, so a count of `CRATONVM_DBG_JIT_COMPILED: osr ` lines (the
/// published single-pass OSR bodies) is unchanged.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_jit_compiled_line(
    tag: &str,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    entry_pc: usize,
    what: &str,
) -> String {
    format!(
        "CRATONVM_DBG_JIT_COMPILED: {tag} {class_name}.{method_name}{method_descriptor} pc={entry_pc} ({what})"
    )
}

pub(in crate::runtime::interpreter::jit_bridge) fn build_osr_optimizing_artifact(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    cached: &Arc<CachedBytecodeMethod>,
    code: &Arc<[u8]>,
    entry_pc: usize,
    door: &str,
) -> Option<(
    Arc<cratonvm_jit::CompiledMethod>,
    Arc<[cratonvm_jit::RetainedCode]>,
)> {
    let key = osr_optimizing_refusal_key(
        shared.vm_identity,
        class_id,
        method_name,
        method_descriptor,
        entry_pc,
    );
    // The body is entered by a frame running `code`, so it must be built from
    // that bytecode. A frame with no handle of its own is resolved by name,
    // and after a redefinition that resolves the NEW body while an obsolete
    // frame still runs the old one.
    if !same_bytecode(&cached.code, code) {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: resolved body is not the frame's bytecode"
            );
        }
        return None;
    }
    // Interpreter round i1 wave 43, lane L2 (`CompileInputs`): the body is
    // adopted inside the compile's epoch witness, compared with the class's
    // current method after the witness opened (the frame's bytecode equals it,
    // above). A resolved body a redefinition replaced is not compiled, and
    // like the mismatch above that records nothing.
    let Some(inputs) = CompileInputs::adopt(shared, Arc::clone(cached)) else {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: resolved body is not the class's current body"
            );
        }
        return None;
    };
    // Read BEFORE the compile, so a redefinition or flush racing it leaves the
    // cached entry stale rather than current.
    let stamps = OsrBuildStamps::read(shared, class_id);
    let inputs = inputs.with_osr_entry_pc(entry_pc);
    // `true`: this body's unconditional back-edge polls may leave for the
    // interpreter (interpreter round i1 wave 15, lane L2).
    let (cm, callee_pins, own_verdict) = match compile_optimizing_artifact(shared, &inputs, true) {
        Some(PinnedArtifact {
            compiled,
            callee_pins,
            verdict,
        }) => (Some(compiled), callee_pins, verdict),
        None => (None, Vec::new(), None),
    };
    // WHOSE body the three fields below describe.
    //
    // Round 9 wave 12. `compile_optimizing_artifact` returns the SINGLE-PASS
    // body when `ir_evidence::accept` refuses the optimizing one, and a
    // single-pass body carries no OSR entries and `ir_osr_sentinel_free ==
    // false`. So the line read `entries=[] sentinel_free=false` for two
    // opposite events -- "the optimizing body has a stub the door cannot
    // enter" and "the optimizing body was thrown away by the acceptance gate"
    // -- and nothing distinguished them.
    //
    // That cost a wave a wrong conclusion: wave 11 of
    // `perf-long-counted-loops-lose-the-optimizing-osr-body` read a
    // partial-unroll A/B as "the unrolled optimizing body beats single-pass"
    // when the gate had refused the unrolled body (its only evidence bit is
    // `Unrolled`, which `is_worth_publishing` deliberately excludes) and BOTH
    // arms were running the single-pass body.
    //
    // The verdict is a thread-local take-and-clear published by the compile
    // that just ran on THIS thread, so this reads its own compile's answer.
    // Taken unconditionally rather than inside the `dbg_jitc()` guard, so the
    // slot is left in the same state whether or not the diagnostic is on —
    // which on a compile WORKER also keeps this verdict from being read by
    // the next method-entry task's supersede check.
    //
    // Round 15 wave 2 (lane calls2, CR15-4): "published by the compile that
    // just ran" did not hold. The in-place callee compiles nested in this
    // compile publish too, and one run from the single-pass fall-through's
    // planning (after this compile's gate) replaced its verdict; a compile that
    // never reached the gate read whatever an earlier compile on this mutator
    // left. `compile_optimizing_artifact` now hands its own verdict back as a
    // value and leaves the slot empty, so the take below answers only with
    // `CRATONVM_JIT_OPTIMIZING_DOOR_OWN_VERDICT=0`.
    let accept = match own_verdict.or_else(cratonvm_jit::ir_evidence::take_last_verdict) {
        Some(true) => "accepted",
        Some(false) => "REFUSED-by-acceptance-gate",
        None => "not-judged",
    };
    if crate::runtime::env_cache::dbg_jitc() {
        match &cm {
            None => eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: compile declined"
            ),
            // Round 15 wave 5 (lane osrtier): and the lowerer's two exit facts
            // plus every guard exit whose reason is not an exact JVMS check.
            // `entries=[]` on an ACCEPTED body has two causes the line could
            // not tell apart: no fact admitted the body's exits, so the
            // lowerer emitted no stub at all (`exc_exits=None
            // guard_exits=None`; `non_exact` names the guards that withdrew
            // the guard fact), or a fact held and every header refused its
            // stub (`CRATONVM_DBG_IR_LINEAR_SCAN=1` names that refusal).
            Some(cm) => eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: stub={} entries={:?} sentinel_free={} accept={accept} exc_exits={:?} guard_exits={:?} non_exact={:?}",
                cm.ir_osr_entry_addr(entry_pc as u32).is_some(),
                cm.ir_osr_entries.iter().map(|(b, _, _)| *b).collect::<Vec<_>>(),
                cm.ir_osr_sentinel_free,
                cm.ir_osr_exception_exit_bcis.as_deref(),
                cm.ir_osr_guard_exit_bcis.as_deref(),
                cm._deopt_point_boxes
                    .iter()
                    .filter(|p| {
                        !p.semantics.rethrow_exception
                            && !matches!(
                                p.reason,
                                cratonvm_jit::deopt::DeoptReason::NullCheck
                                    | cratonvm_jit::deopt::DeoptReason::BoundsCheck
                                    | cratonvm_jit::deopt::DeoptReason::DivByZero
                            )
                    })
                    .map(|p| format!("{}@{:?}", p.bci, p.reason))
                    .collect::<Vec<_>>(),
            ),
        }
    }
    let Some(cm) = cm else {
        // A refusal that expires is not a verdict (wave 38, lane L2): the
        // next offer after the cap clears or the de-speculation expires asks
        // again. See `osr_optimizing_refusal_is_transient`.
        if osr_optimizing_refusal_is_transient(
            shared,
            class_id,
            class_name,
            method_name,
            method_descriptor,
        ) {
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: compile declined for a transient reason (not memoised)"
                );
            }
            return None;
        }
        // Interpreter round i5 wave 101, lane L2
        // (`CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY`, default ON since j27; the
        // narrow slice of `i39-L2-proposal-a-compile-request-returns-why-it-
        // produced-no-body`): a decline at a constant-pool resolver miss (a
        // class the loop has not loaded yet) is "waiting on a class", not a
        // property of the bytecode. Left unmemoised a bounded number of times,
        // so the loop's later activations, which have loaded the class, get
        // the optimizing body instead of the single-pass one for the rest of
        // the epoch.
        if osr_optimizing_refusal_is_resolver_miss(
                shared,
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            && shared
                .jit
                .bridge
                .osr_optimizing_resolver_miss_may_retry(osr_optimizing_method_wide_key(key))
        {
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: compile declined at a resolver miss (not memoised; retried later)"
                );
            }
            return None;
        }
        // The compile itself declined. Same conclusion as a refused artifact
        // for this door's purposes, and the same waste if it is repeated. The
        // compile never saw `entry_pc`, so the verdict is the whole method's.
        note_osr_optimizing_refusal(
            shared,
            osr_optimizing_method_wide_key(key),
            stamps.jit_redefine_epoch,
        );
        return None;
    };
    // Round 13 wave 8 (lane codecache): every direct-call target baked into
    // this body must be held by a pin of the artifact the bake recorded. This
    // body is never published through `JitCache`, so the check
    // `prepare_for_publication` makes for every published body never runs
    // for it. Not a refusal of the method: the next entry rebuilds.
    if !osr_optimizing_baked_callees_pinned(&cm, &callee_pins)
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused -- a baked direct-call target has no live pin"
            );
        }
        return None;
    }
    // Compiled across a redefinition or flush: neither entered nor remembered,
    // and not a refusal (the next entry rebuilds from the current bytecode).
    // See `osr_optimizing_artifact_is_stale`.
    if osr_optimizing_artifact_is_stale(shared, stamps.code_state_epoch) {
        return None;
    }
    // Interpreter round i6 wave 117, lane L3: the same refusal for a
    // redefinition that did not move the code-state epoch (a scoped one under
    // `CRATONVM_JIT_SCOPED_REDEFINITION_KEEPS_CODE_STATE`).
    if osr_build_spans_a_redefinition(shared, stamps.redefinition_count) {
        return None;
    }
    // Likewise for a class-hierarchy change, logged while it compiled, that
    // withdraws what it inlined or devirtualised
    // (`osr_optimizing_dependencies_current`).
    if !osr_optimizing_dependencies_current(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        &cm,
        cm.invalidation_epoch,
    ) {
        return None;
    }
    // Tier proposal 27 (round 11 wave 10, lane irexc): a body that is not
    // sentinel-free is admitted when every exit it has is exceptional and the
    // door resolves each one exactly — see
    // `osr_optimizing_exception_exits_admissible`. `false` with the switch off,
    // so the door is then exactly what it was.
    //
    // Phase 3 (round 11 wave 12, lane irexc): or when its guard exits are ones
    // `try_osr` resumes without a plan and nothing their interpreter
    // continuation does falsifies the body — see
    // `osr_optimizing_guard_exits_admissible`. `false` with that switch off.
    let exits_admitted = !cm.ir_osr_sentinel_free
        && (osr_optimizing_exception_exits_admissible(&cm, &cached.exception_table)
            || osr_optimizing_guard_exits_admissible(&cm, &cached.exception_table, code));
    if !cm.ir_osr_sentinel_free && !exits_admitted {
        // Round 15 wave 5 (lane osrtier): said, not silent. The line above
        // read `accept=accepted` for this body, and nothing said the door then
        // threw it away (`r15w5-osrtier-accepted-optimizing-osr-body-never-entered`).
        if crate::runtime::env_cache::dbg_jitc() {
            // Round 15 wave 6 (lane irosr): `not-judged` means the acceptance
            // gate never saw an optimizing body (the IR route declined and the
            // single-pass body came back, `R15OsrtierAllocLoop.wide`'s
            // `IrBuilder::build refused`); "the door admits none of its exits"
            // sent a reader after the exits of a body that was never built.
            let why = if accept == "not-judged" {
                "the acceptance gate judged no optimizing body (the `[ir]` lines above name why the IR route declined)"
            } else {
                "the body has exits and the door admits none of them"
            };
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused (method-wide) -- {why}"
            );
        }
        // nio21 wave 18, lane L4: said under the `jit-compiled` recipe too
        // (`osr_optimizing_report_jit_compiled`), which is the one the
        // perf-gap triage runs.
        osr_optimizing_report_jit_compiled(
            "ir-osr-refused",
            class_name,
            method_name,
            method_descriptor,
            entry_pc,
            if accept == "not-judged" {
                "no optimizing body was judged"
            } else {
                "the body has exits the door admits none of"
            },
        );
        promote_unenterable_osr_method(shared, cached, accept == "accepted", door);
        // A property of the whole body, so of every loop header in it: the
        // method's other headers skip the rebuild too (round 11 wave 9). Before
        // this, each header paid its own identical compile — on the MUTATOR,
        // inside `try_osr`, whenever a single-pass body was enterable there.
        note_osr_optimizing_refusal(
            shared,
            osr_optimizing_method_wide_key(key),
            stamps.jit_redefine_epoch,
        );
        return None;
    }
    // Round 11 wave 11 (lane irexc): the door's half of
    // `r11w10-orch-osr-exc-exits-loses-handler-updated-locals`. A body admitted
    // for its exceptional exits hands the interpreter exceptions its own
    // handlers catch; a header such a handler reaches is then re-entered with
    // state no path of the graph produced, and the graph's header facts (a φ
    // collapsed to a constant, a range, a type) are wrong about it. The lowerer
    // already withholds those entries (`osr_entry_closed_by_a_handler`); this
    // is the defence in depth, and it is the only check for a body from a
    // lowering that did not know the handlers.
    let handler_reach: Option<Vec<u32>> = if exits_admitted {
        osr_handler_reachable_pcs(code, &cached.exception_table)
    } else {
        Some(Vec::new())
    };
    // Round 11 wave 14 (lane osrmerge): NOT at an OPAQUE entry. There the
    // builder put an opaque merge in front of the header, so no fact at the
    // entry assumes a graph path, and the handler's state is as good as any
    // other.
    //
    // Round 11 wave 17 (lane tier, blocker 4 of
    // `r11w16-irback-osr-opaque-entry-scope-and-door-bit-patch`): keyed on the
    // ARTIFACT's per-entry bit (`ir_osr_opaque_entry_bcis`), not on the
    // process switch. Before, under `CRATONVM_JIT_OSR_OPAQUE_ENTRY` the door
    // relaxed for every header and trusted the lowerer to have withheld each
    // NON-opaque handler-reached entry (`osr_entry_closed_by_a_handler`); the
    // door is the defence in depth and now checks the stub it would enter.
    // A copy of the slice, because `cm` moves into the `Arc` below while the
    // fan-out still asks this.
    let opaque_entry_bcis: Box<[u32]> = cm.ir_osr_opaque_entry_bcis.clone();
    let closed_by_a_handler = |pc: usize| {
        let opaque_here =
            u32::try_from(pc).is_ok_and(|b| opaque_entry_bcis.binary_search(&b).is_ok());
        exits_admitted
            && !opaque_here
            && cratonvm_jit::deopt::pc_reachable_from_handlers(handler_reach.as_deref(), pc)
    };
    // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
    let enterable_here =
        cm.ir_osr_entry_addr(entry_pc as u32).is_some() && !closed_by_a_handler(entry_pc);
    if !enterable_here {
        // No stub for THIS header, or one a handler can reach: a per-pc
        // verdict. The body is still kept under the OTHER headers it can be
        // entered at (the fan-out below, which applies the same two tests per
        // header): the build is pc-blind, and a header whose single-pass body
        // is published but unenterable never asks this route again (round 11
        // wave 11, r11w11-tier-osr-optimizing-no-stub-body-kept-for-other-headers-patch).
        if crate::runtime::env_cache::dbg_jitc() && closed_by_a_handler(entry_pc) {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused -- an exception handler of the method reaches this header"
            );
        } else if crate::runtime::env_cache::dbg_jitc() {
            // Round 15 wave 5 (lane osrtier): the other per-pc refusal, said.
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused -- the lowerer emitted no entry stub at this header (CRATONVM_DBG_IR_LINEAR_SCAN=1 names why)"
            );
        }
        promote_unenterable_osr_method(shared, cached, accept == "accepted", door);
        note_osr_optimizing_refusal(shared, key, stamps.jit_redefine_epoch);
    }
    // Keep it. Every OSR entry at this pc used to rebuild this artifact from
    // bytecode and drop it again when the frame left — see
    // `remember_osr_optimizing_artifact` for the measurement that motivates
    // the cache. Its callee pins are kept with it: this body is never
    // published through `JitCache`, so nothing else roots what it CALLs.
    //
    // Owned by its class, as `JitCache::put_osr` stamps a published OSR body:
    // unstamped (`NO_OWNER_CLASS`), a walk that lays this frame out unmerged
    // printed it `(Unknown Source)`, handed the caller-class walk an id naming
    // no class, and `jit_activation::enter` kept no class active under it
    // (exc3 W5-4; patch by round 12 wave 6 lane jni, applied in wave 7 lane
    // replay4). `helpers::trapping_artifact_is_superseded` excludes it by
    // `was_published()`, not by the missing owner.
    let mut cm = cm;
    cm.owner_class_id = cached.declaring_class_id.as_u32();
    // The artifact owns its callees: a memo hit hands a thread the artifact
    // without the memo entry's pins, and a purge of that entry would free a
    // callee under a frame still running this body.
    cm.adopt_callee_pins(callee_pins.clone());
    let artifact = Arc::new(cm);
    // Make its frames walkable BEFORE anything can enter it. This body is
    // never published through `JitCache`, so neither `put`'s compile-id bind
    // nor its code-range registration ran for it — and since round 11 wave 12
    // (`osr_single_pass_preferred`) it is chosen for loops whose methods CALL,
    // so it direct-CALLs compiled callees. A return address into an
    // unregistered range reads as "the interpreter" to every walker: the
    // callee's frame was reported (and GC-scanned, and remapped) under THIS
    // body's name and layout, and this body's own frame was skipped. See
    // `cratonvm_jit::register_unpublished_body` and
    // `r11-orch-osr-optimizing-body-hides-its-direct-callee-frame-FIXED-20260925`.
    cratonvm_jit::register_unpublished_body(&artifact);
    // Profiler symbols (perf map, jitdump, GDB), as `JitCache::put_osr` gives a
    // published OSR body; no-op unless a sink is on. Without it a profile of a
    // loop this body runs (`R16ArraylistSplit iter-foreach 5`) scattered its
    // samples over unnamed `[JIT]` addresses (OSR round, 2026-10-06).
    // JVMTI `CompiledMethodLoad` (interpreter round i2 wave 63, lane L1).
    cratonvm_jit::code_events::publish_method(
        artifact.entry_ptr() as usize,
        artifact.code_len(),
        class_name,
        method_name,
        method_descriptor,
    );
    cratonvm_jit::code_events::publish(artifact.entry_ptr() as usize, artifact.code_len(), || {
        cratonvm_jit::code_events::method_name(
            class_name,
            method_name,
            method_descriptor,
            cratonvm_jit::code_events::CodeTier::Osr(Some(entry_pc)),
        )
    });
    // Interpreter round i1 wave 26, lane L6: a class redefinition forces the
    // exit polls of this body even after the memo below forgot it while a
    // frame still runs it (`JitCache::live_unpublished_exit_poll_bodies`).
    shared.jit.jit_cache.note_exit_poll_body(&artifact);
    // t1 wave 2, lane jit2 (B5, `CRATONVM_JIT_OSR_LATE_STATIC_CELL`, default ON since j27):
    // the IR table builder plants late-bind / cycle-edge cells in this body as
    // in any optimizing body, but only `JitCache::put` registers a body's cells
    // and this one is never published, so they stayed `0` (dispatch) for its
    // whole life. Registered here, before anything can enter the body.
    // t1 wave 3, lane X (`CRATONVM_JIT_IR_OSR_CELL_REGISTER`, default ON; the
    // opt-in switch above still implies it): on the default path too. A cell
    // that is registered is cleared by every invalidation of its target
    // (`JitCache::invalidate_cycle_cells` walks the registry), so it never
    // keeps a withdrawn body; one that is not stays `0` and dispatches.
    if cratonvm_jit::jit_ir_osr_cell_registration_enabled() {
        shared
            .jit
            .jit_cache
            .register_unpublished_body_late_cells(&artifact);
    }
    let callee_pins: Arc<[cratonvm_jit::RetainedCode]> = callee_pins.into();
    // nio21 wave 18, lane L4 (`r21-row-driver-loop-is-never-compiled-CLOSED-20261012.md`):
    // this body is never published through `JitCache`, so the
    // `CRATONVM_DBG_JIT_COMPILED` recipe (`CRATONVM_DBG=jit-compiled`) printed
    // nothing for it and a loop running its optimizing OSR body read as "never
    // compiled". One line per cached body, under its own tag.
    osr_optimizing_report_jit_compiled(
        "ir-osr",
        class_name,
        method_name,
        method_descriptor,
        entry_pc,
        if enterable_here {
            "cached"
        } else {
            "kept for other headers"
        },
    );
    // t1 wave 8, lane L1OSR (L1W1-4 of `docs/feature-designs/jit-ir-optimizations.md`,
    // `CRATONVM_JIT_OSR_PROFIT_EXIT_SUPERSEDED_OPTIMIZING`, default ON since j27): the older
    // artifacts the memo serves for this method before this body replaces
    // them (an acceptance re-offer, a de-spec rebuild); see
    // [`queue_superseded_optimizing_osr_bodies`].
    let superseded_before = if osr_profit_exit_superseded_optimizing_enabled() {
        osr_optimizing_remembered_other_artifacts(
            shared,
            class_id,
            (class_name, method_name, method_descriptor),
            Some(&artifact),
        )
    } else {
        Vec::new()
    };
    if enterable_here {
        remember_osr_optimizing_artifact(
            shared,
            key,
            &artifact,
            &callee_pins,
            stamps,
            code,
            exits_admitted,
        );
        note_osr_optimizing_static_init_waiters(
            shared,
            class_id,
            (class_name, method_name, method_descriptor),
            entry_pc,
            &artifact,
        );
        note_osr_optimizing_first_call_waiters(
            shared,
            class_id,
            (class_name, method_name, method_descriptor),
            entry_pc,
        );
    }
    // And under every OTHER loop header it carries a stub for (round 11 wave
    // 9). The build is pc-blind — `compile_optimizing_artifact` takes no entry
    // pc, and neither does any verdict of `osr_optimizing_build_declined` — so
    // a later entry at another header would rebuild this body byte for byte.
    // Sentinel-freedom is a property of the whole body, and
    // `osr_optimizing_cached` re-checks the stub for the pc it serves.
    for &(bci, _, _) in artifact.ir_osr_entries.iter() {
        // Widening: a bci is u32.
        let pc = bci as usize;
        // A bci past the method's own code is a spliced callee's relocated
        // one: no frame of this method stands there, so the door never asks
        // for it (round 12 wave 1, lane osr).
        if pc != entry_pc && pc < code.len() {
            let pc_key = osr_optimizing_refusal_key(
                shared.vm_identity,
                class_id,
                method_name,
                method_descriptor,
                pc,
            );
            // Round 11 wave 11: the per-pc refusal above, for this header.
            if closed_by_a_handler(pc) {
                note_osr_optimizing_refusal(shared, pc_key, stamps.jit_redefine_epoch);
                continue;
            }
            // JIT round 19 wave 2 (lane osr2, OSR19-1): a header already
            // declined for its own polymorphic loop keeps its single-pass
            // body; any other is remembered with the decline still to ask
            // (`AcceptedOsrOptimizing::poly_decline_unasked`).
            let per_header = cratonvm_jit::osr_poly_decline_per_header_enabled();
            if per_header && osr_optimizing_already_refused(shared, pc_key) {
                continue;
            }
            remember_osr_optimizing_artifact(
                shared,
                pc_key,
                &artifact,
                &callee_pins,
                stamps,
                code,
                exits_admitted,
            );
            if per_header {
                if let Ok(mut m) = shared.jit.bridge.osr_optimizing_accepted.lock() {
                    if let Some(entry) = m.get_mut(&pc_key) {
                        entry.poly_decline_unasked = true;
                    }
                }
            }
        }
    }
    // t1 wave 9, lane JITB (`CRATONVM_JIT_OSR_INSTALL_RECHECK`, correctness,
    // default ON): the install-time re-check. A record logged between the
    // dependency check above and the memo inserts is judged as in flight
    // (the publication rule), and the memo entries then start their
    // installed-rule re-checks after it; see
    // [`settle_osr_optimizing_install`].
    if !settle_osr_optimizing_install(
            shared,
            class_id,
            (class_name, method_name, method_descriptor),
            &artifact,
        )
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing{door} {class_name}.{method_name} pc={entry_pc}: refused at install -- an invalidation logged while it was remembered withdraws it"
            );
        }
        // The older artifacts this body displaced from the memo are no longer
        // remembered either: queued as usual (weak rows), so a frame still
        // running one is armed by the next covering rebuild.
        queue_superseded_optimizing_osr_bodies(
            shared,
            class_id,
            (class_name, method_name, method_descriptor),
            superseded_before,
        );
        return None;
    }
    queue_superseded_optimizing_osr_bodies(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        superseded_before,
    );
    // JIT round 23 wave 1, lane L1 (THROWS22-1, `CRATONVM_JIT_OSR_PROFIT_EXIT`):
    // a frame already running the method's single-pass OSR body moves to this
    // one at its next back edge, once every header can enter it.
    arm_single_pass_osr_profit_exit(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        code,
        door,
    );
    arm_superseded_optimizing_osr_exits(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        code,
    );
    enterable_here.then_some((artifact, callee_pins))
}

/// Tier proposal 26, the mutator's half (round 11 wave 9): is an accepted
/// optimizing OSR body cached for `entry_pc`, so that `try_osr` enters it
/// WITHOUT compiling anything?
///
/// `try_osr_with_backoff`'s background arm asks this beside "is a single-pass
/// OSR body published and enterable here". Before wave 9 it asked only the
/// latter, so a loop whose single-pass OSR compile was refused — or published
/// without an entry at this pc — never reached `try_osr`, where the optimizing
/// route lives. The background worker now builds that body
/// ([`background_osr_optimizing_body`]); this is how the mutator finds it.
///
/// Answers `false` for a method whose OSR is denied, so the arm's
/// `osr_method_denied` accounting is unchanged, and under either kill switch
/// (`CRATONVM_JIT_OSR_OPTIMIZING_BG=0`, `CRATONVM_JIT_OSR_OPTIMIZING=0`), which
/// restores the pre-wave-9 routing exactly. `try_osr` then asks the same
/// cache first, so a `true` here is entered without a compile on this thread.
pub(in crate::runtime::interpreter) fn osr_optimizing_body_ready(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    code: &Arc<[u8]>,
    entry_pc: usize,
) -> bool {
    if !osr_optimizing_tier_enabled()
    {
        return false;
    }
    let key = osr_optimizing_refusal_key(
        shared.vm_identity,
        class_id,
        method_name,
        method_descriptor,
        entry_pc,
    );
    if osr_optimizing_cached(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        key,
        entry_pc,
        code,
    )
    .is_none()
    {
        return false;
    }
    let method_key = crate::jit::tiered::MethodKey::with_class_id(
        class_id,
        class_name,
        method_name,
        method_descriptor,
    );
    !shared.jit.tiered_manager.is_osr_denied(&method_key)
}

/// Tier proposal 26, the worker's half (round 11 wave 9,
/// `r11w8-tier-optimizing-osr-route-unreachable-behind-single-pass`): build the
/// OPTIMIZING OSR body for a background OSR task, BEFORE the single-pass one —
/// the order `try_osr` has always used.
///
/// `true` when an accepted optimizing body with an entry stub for `osr_bci` is
/// cached (already, or by this call), so the task need not build a
/// single-pass body for this pc: the mutator's background arm finds it through
/// [`osr_optimizing_body_ready`] and `try_osr` enters it from the cache.
/// `false` leaves the task to the single-pass compile exactly as before, and a
/// refusal is memoed so `try_osr` does not rebuild the body on the mutator
/// once the single-pass one is published.
///
/// Compiling on the worker is established practice for OSR: the single-pass
/// OSR body this task builds already compiles its direct callees eagerly on
/// this thread (`compile_osr_body` → `try_jit_compile_callee`), which is the
/// same one-level nesting `compile_optimizing_artifact`'s callee compiler does.
/// The optimizing body's own compile used to run on the MUTATOR, inside
/// `try_osr`, on the first entry after the single-pass body published; that
/// stall moves here too.
pub(in crate::runtime::interpreter::jit_bridge) fn background_osr_optimizing_body(
    shared: &SharedVm,
    key: &crate::jit::tiered::MethodKey,
    osr_bci: usize,
) -> bool {
    // x86-64 only, like the entry that consumes it (`ir_osr_enter`).
    if cfg!(not(target_arch = "x86_64")) {
        return false;
    }
    if !osr_optimizing_tier_enabled()
    {
        return false;
    }
    let Some((class_id, code, _max_locals)) = fetch_osr_compile_inputs(
        shared,
        key.class_id,
        &key.class_name,
        &key.method_name,
        &key.descriptor,
    ) else {
        return false;
    };
    let class_name: &str = &key.class_name;
    let method_name: &str = &key.method_name;
    let method_descriptor: &str = &key.descriptor;
    let opt_key = osr_optimizing_refusal_key(
        shared.vm_identity,
        class_id,
        method_name,
        method_descriptor,
        osr_bci,
    );
    if let Some(body) = osr_optimizing_cached(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        opt_key,
        osr_bci,
        &code,
    ) {
        // zt pl4 item 1: the body this task's OSR compile stands for.
        note_published_body_sizes(code.len(), &body);
        return true;
    }
    if osr_optimizing_build_declined(
        shared,
        class_id,
        class_name,
        method_name,
        method_descriptor,
        opt_key,
        osr_bci,
        &code,
        " (bg)",
    ) {
        return false;
    }
    let lookup = format!("{class_name}.{method_name}:{method_descriptor}");
    let cached = match super::deopt_resume::resolve_inlined_callee(shared, class_id, &lookup) {
        Ok(c) => c,
        Err(why) => {
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr optimizing (bg) {class_name}.{method_name} pc={osr_bci}: no cached method ({why})"
                );
            }
            return false;
        }
    };
    match build_osr_optimizing_artifact(
        shared,
        class_id,
        class_name,
        method_name,
        method_descriptor,
        &cached,
        &code,
        osr_bci,
        " (bg)",
    ) {
        Some((body, _)) => {
            // zt pl4 item 1: the optimizing OSR body is kept in its memo, not
            // the cache: its sizes are reported here.
            note_published_body_sizes(code.len(), &body);
            true
        }
        None => false,
    }
}

/// `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS`, **default ON** again since round 9
/// wave 6 (`=0` opts out): let the OSR door keep the single-pass body for a
/// method whose optimizing OSR body is not known to be faster (see
/// [`osr_optimizing_not_known_better`]).
///
/// # Why it went back on (round 9 wave 6, lane irlower6)
///
/// Wave 5 made the SINGLE-PASS OSR body register-resident and unrolled
/// (`ArithOsr.mulOnly`: 13 register-only instructions per iteration, two
/// iterations per trip) and the optimizing body did not move, so wave 3's
/// comparison below reversed. Measured on the wave-5 binary
/// (`cratonvm-jitr9-w5b.exe`), `=0` (optimizing) vs `=1` (single-pass), 1e9
/// iterations, two runs each, interleaved:
///
/// | `ArithOsr` loop | optimizing | single-pass |
/// |---|---:|---:|
/// | `mul` (`lcmp`) | 1 709 / 1 720 ms | 790 / 762 ms |
/// | `div` | 1 241 / 1 166 ms | 868 / 844 ms |
/// | `rem` | 1 428 / 1 440 ms | 1 160 / 1 128 ms |
/// | `full` | 1 908 / 1 784 ms | 1 526 / 1 560 ms |
/// | `int` (`idiv`/`irem`, no `lcmp`) | 1 942 / 1 982 ms | 2 032 / 2 038 ms |
///
/// and `CratonBench arithmetic` 4 420 / 4 029 / 3 970 ms optimizing against
/// 3 400 / 3 056 / 3 197 single-pass; `matrix` and `sieve` (no gated opcode)
/// were a wash (2 651/2 833, 2 660/2 638, 2 708/2 712; 7 148/7 258,
/// 7 458/7 445, 7 471/7 416). The `int` row is why the predicate no longer
/// names `idiv` / `irem`: there the optimizing body is the (slightly) faster
/// one.
///
/// # Re-measured round 9 wave 12 (2026-09-20), and the default stands
///
/// The wave-5 table above is kept for the history; it is four binaries old and
/// its absolute numbers are no longer the ones this flag is chosen on. On a
/// `dev` build (`00cc38bea`), 1e9 iterations, interleaved ABC-CBA, three
/// rounds, minimum of six runs per cell (the host carried load 4-7):
///
/// | `ArithOsr` loop | `=1`, single-pass (default) | `=0`, optimizing | HotSpot 21 |
/// |---|---:|---:|---:|
/// | `mul` (`lcmp`) | **393 ms** | 418 ms | 350 ms |
/// | `div` | **511 ms** | 904 ms | 450 ms |
/// | `rem` | **816 ms** | 920 ms | 772 ms |
/// | `full` | **1 066 ms** | 1 237 ms | 964 ms |
/// | `int` (no `lcmp`) | 1 384 ms | **1 401 ms** (a wash) | 1 095 ms |
///
/// and `ArithProbe` (`CratonBench.benchArithmetic`'s loop, 2e9) 3 007 / 3 304 /
/// 3 393 ms on the default against HotSpot's 2 394 / 2 518 / 2 518 and
/// 4 019 / 3 758 / 3 755 with `=0`. The ordering the wave-6 table established
/// holds on every row, so the default stays ON. See
/// `docs/feature-designs/deopt-osr.md`
/// for what the remaining difference is (the single-pass OSR tier's 2x unroll
/// and its in-place carried-value update, not instruction selection: a loop as
/// simple as `s += i * 3` measures 289 ms single-pass against 451 optimizing).
///
/// `mul` was also the one shape whose optimizing body never fused its `lcmp`
/// (the compare's clobber test refused it; fixed this wave in
/// `ir_lower::prepare_fusion_tables`). Re-measure `mul` with `=0` after that
/// build; the other rows already had the fused compare and are unaffected.
///
/// # History
///
/// It landed default-ON in round 9 wave 3 as the stop-gap for the table below,
/// and went default-OFF at that wave's integration once the optimizing tier
/// strength-reduced constant division and fused `lcmp` (`ir_lower.rs`,
/// `emit_div_by_const` / the fused-lcmp branch). Re-measured on the wave-3
/// binary, single-pass (flag on) vs optimizing (flag off), 5e8 iterations:
/// `ArithOsr div` 496 vs 449 ms, `rem` 758 vs 593, `int` 1 094 vs 780, `full`
/// 903 vs 738, `mul` 648 vs 679 (noise); `CratonBench arithmetic` 3 341 vs
/// 2 997 ms; `matrix` three pairs 2 411/2 470, 2 606/2 415, 2 501/2 425 (noise).
/// The optimizing body is now the faster one on every shape the gate names.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_prefer_single_pass_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Whether `code` contains a shape whose optimizing OSR body is measured
/// SLOWER than the single-pass one, so the door should not trade the
/// single-pass body for it: a long division / remainder (`ldiv`, `lrem`) or a
/// long compare (`lcmp`) -- i.e. every `long`-counted loop.
///
/// Round 9 wave 6 dropped `idiv` / `irem`: on the wave-5 binary an `int`
/// induction with both measured 1 942 / 1 982 ms optimizing against
/// 2 032 / 2 038 ms single-pass (see [`osr_prefer_single_pass_enabled`]), so
/// the optimizing body is known to be at least as good there.
///
/// # Confirmed round 9 wave 12 (2026-09-20)
///
/// The predicate's SHAPE, not just its default, was re-measured on a `dev`
/// build: the four `long` rows (`ldiv`/`lrem`/`lcmp`) have the single-pass
/// body winning by 6 % (`mul`), 77 % (`div`), 13 % (`rem`) and 16 % (`full`),
/// and the `int` row -- which this predicate deliberately does not name -- is a
/// wash with the optimizing body marginally ahead. The table is in
/// [`osr_prefer_single_pass_enabled`]. So the membership below is exactly
/// right and nothing about it changed; what changed is that it now rests on a
/// current binary instead of a two-year-old one.
///
/// # The measurement (round 9 wave 3, wave-2 binary, 1e9 iterations)
///
/// `diag/src/ArithOsr.java`, `CRATONVM_JIT_OSR_OPTIMIZING=1` vs `=0` (the
/// optimizing body admitted -- `stub=true entries=[5] sentinel_free=true` --
/// vs the single-pass body), two or three runs each:
///
/// | loop | optimizing | single-pass |
/// |---|---:|---:|
/// | `s += i*3 + (i>>1)` (`lcmp`) | 1 309 / 1 323 ms | 1 026 / 1 025 ms |
/// | `s += i*3 - i/2` | 3 601 / 3 377 ms | 1 607 / 1 357 ms |
/// | `s += i*3 + i%7` | 3 740 / 3 512 ms | 2 102 / 2 204 ms |
/// | `/` and `%` | 6 679 / 6 391 ms | 2 280 / 2 512 ms |
/// | same, `int` induction | 3 289 / 3 116 ms | 2 261 / 2 377 ms |
///
/// and `CratonBench arithmetic`: 8 650 ms vs 3 411 ms. Once wave 2 made a
/// constant divisor guard-free, these bodies became sentinel-free and the door
/// admitted them -- a 2.5x regression on the benchmark the admission was meant
/// to help. The causes are in the optimizing tier (`IDIV` for a constant
/// divisor where single-pass multiplies by a magic number; `lcmp` materialised
/// to {-1,0,1} and spilled instead of fused into the branch --
/// `docs/feature-designs/deopt-osr.md`),
/// so this is a policy for "not known to be better", not a correctness gate:
/// flip it off (`CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0`) and re-measure once
/// the optimizing tier strength-reduces constant division and fuses `lcmp`.
///
/// Loops without these shapes keep the optimizing body: `CratonBench matrix`
/// measured 2 099 ms optimizing vs 2 344 ms single-pass, `sieve` 4 352 vs
/// 4 295 (a wash).
///
/// The whole method is scanned, not only the loop, and the scan runs once per
/// method -- its verdict is kept in the refusal memo. Every row above was
/// measured on a CALL-FREE body: until tier proposal 27 a call made the body
/// non-sentinel-free and the door refused it anyway. With exceptional exits
/// admitted that no longer holds; [`osr_single_pass_preferred`] is the
/// predicate the door asks, and it keeps this one to the population it was
/// measured on.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_not_known_better(code: &[u8]) -> bool {
    const LDIV: u8 = 0x6d;
    const LREM: u8 = 0x71;
    const LCMP: u8 = 0x94;
    let mut pc = 0usize;
    while pc < code.len() {
        if matches!(code[pc], LDIV | LREM | LCMP) {
            return true;
        }
        // `max(1)`: the decoder steps an undecodable byte by one, but a walk
        // that must terminate does not lean on that.
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    false
}

/// The single-pass preference as the door asks it (round 11 wave 11, lane
/// tier, r11w8-tier-optimizing-osr-route-unreachable-behind-single-pass):
/// [`osr_optimizing_not_known_better`], except for a method with a call
/// inside a loop once the door admits bodies with exceptional exits.
///
/// The preference's tables are all call-free loops, the only kind the door
/// could enter before tier proposal 27. A loop with a call is a population
/// they never measured, and the one measurement there is has the optimizing
/// body far ahead: `R11X64genSyncCost` `sync-method-1t` 1 115 ms on the
/// single-pass OSR body against 91 ms on the optimizing one (whose direct
/// synchronized call binding the single-pass body lacks). There the door's
/// own order (optimizing first) answers, not a preference from other loops.
/// A call outside every loop (a `println` after it) does not count: the loop
/// the body runs is still the measured shape.
///
/// With `exception_exits_admitted == false` the door refuses any body with a
/// call, so this is exactly [`osr_optimizing_not_known_better`]. Exits have
/// been admitted by default since round 11 wave 11, so the exemption below is
/// the default path.
///
/// # Which call lifts the preference (round 11 wave 12, lane tier)
///
/// Only a call in EVERY loop that carries a gated opcode. Wave 11 lifted the
/// preference for a call in ANY loop, and that includes a loop that merely
/// encloses the measured one. The standard timing harness is exactly that
/// shape: `for (rep ..) { t0 = nanoTime(); for (long i ..) s += i / 2;
/// println(..); }`. The outer loop's range holds `nanoTime` and `println`, so
/// the inner call-free `long` loop, which is the one the OSR body spends its
/// time in, lost the single-pass body that the tables measured as faster for
/// it. Now a call-free gated loop keeps the preference whatever encloses it,
/// and a gated loop that calls on every iteration (`R11TierOsrLongCallLoop`)
/// still goes to the optimizing body. A method whose gated opcode is outside
/// every loop (a `long` compare after an `int` call loop) is exempted as in
/// wave 11, because no loop of the measured shape exists.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_single_pass_preferred(code: &[u8], exception_exits_admitted: bool) -> bool {
    osr_optimizing_not_known_better(code)
        && !(exception_exits_admitted && BytecodeLoopScan::of(code).every_gated_loop_calls())
}

/// The loop ranges, calls and gated opcodes of a method's bytecode, for
/// [`osr_single_pass_preferred`].
///
/// A loop is a range `[header, back edge]` per backward branch. That is a
/// range test, not a CFG. A call that javac placed after the back edge but
/// that only the loop reaches is missed, and then the preference stands, as
/// before. A call inside the range on a path the loop never takes is counted,
/// and then the optimizing build is attempted and the door keeps or refuses it
/// on its merits.
///
/// Calls are `invokevirtual` / `invokespecial` / `invokestatic` /
/// `invokeinterface`. `invokedynamic` is left out: the IR tier makes it an
/// uncommon trap, whose guard exit the door never admits, so counting it would
/// only buy a compile that is refused. The gated opcodes are
/// [`osr_optimizing_not_known_better`]'s: `ldiv`, `lrem`, `lcmp`.
pub(in crate::runtime::interpreter::jit_bridge) struct BytecodeLoopScan {
    pub(in crate::runtime::interpreter::jit_bridge) loops: Vec<(usize, usize)>,
    pub(in crate::runtime::interpreter::jit_bridge) calls: Vec<usize>,
    pub(in crate::runtime::interpreter::jit_bridge) gated: Vec<usize>,
}

impl BytecodeLoopScan {
    pub(in crate::runtime::interpreter::jit_bridge) fn of(code: &[u8]) -> Self {
        let mut scan = Self {
            loops: Vec::new(),
            calls: Vec::new(),
            gated: Vec::new(),
        };
        let mut pc = 0usize;
        while let Some(&op) = code.get(pc) {
            // Cast: a bytecode offset fits i64.
            let at = pc as i64;
            let offset = match op {
                0xb6..=0xb9 => {
                    scan.calls.push(pc);
                    None
                }
                // ldiv, lrem, lcmp.
                0x6d | 0x71 | 0x94 => {
                    scan.gated.push(pc);
                    None
                }
                // if<cond>, if_icmp<cond>, if_acmp<cond>, goto, ifnull, ifnonnull.
                0x99..=0xa7 | 0xc6 | 0xc7 => match code.get(pc + 1..pc + 3) {
                    Some(&[hi, lo]) => Some(i64::from(i16::from_be_bytes([hi, lo]))),
                    _ => None,
                },
                // goto_w.
                0xc8 => match code.get(pc + 1..pc + 5) {
                    Some(&[a, b, c, d]) => Some(i64::from(i32::from_be_bytes([a, b, c, d]))),
                    _ => None,
                },
                _ => None,
            };
            // A back edge lands at or before its own pc.
            let back_edge_target = offset
                .map(|off| at + off)
                .filter(|target| (0..=at).contains(target));
            if let Some(target) = back_edge_target {
                // Cast: 0 <= target <= pc, so it fits usize.
                scan.loops.push((target as usize, pc));
            }
            pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
        }
        scan
    }

    pub(in crate::runtime::interpreter::jit_bridge) fn range_holds(range: (usize, usize), pcs: &[usize]) -> bool {
        pcs.iter().any(|pc| (range.0..=range.1).contains(pc))
    }

    /// Does some loop's range hold a call?
    pub(in crate::runtime::interpreter::jit_bridge) fn a_loop_calls(&self) -> bool {
        self.loops
            .iter()
            .any(|&range| Self::range_holds(range, &self.calls))
    }

    /// Does some loop hold a call, and does every loop that holds a gated
    /// opcode hold a call too? Vacuously the second when no loop holds a gated
    /// opcode.
    pub(in crate::runtime::interpreter::jit_bridge) fn every_gated_loop_calls(&self) -> bool {
        self.a_loop_calls()
            && self
                .loops
                .iter()
                .filter(|&&range| Self::range_holds(range, &self.gated))
                .all(|&range| Self::range_holds(range, &self.calls))
    }
}

/// The answer for a `try_osr` arm reached AFTER the OSR'd body ran but which
/// cannot hand its state to the live frame (round 11 wave 2,
/// r11-tier-osr-post-entry-refusals-replay-loop-iterations).
///
/// `try_osr`'s `None` means "rejected, nothing ran": the caller resumes the loop
/// at `entry_pc` with the PRE-entry locals. Returned after the body committed
/// iterations, that replays every one of them. So such an arm now:
///
/// * denies OSR for the method (`mark_osr_denied`, expires with the install
///   epoch), so the body is not entered again and the arm cannot repeat;
/// * replays silently only when the bytecode commits no side effect anywhere
///   (`bytecode_commits_side_effect`), where a re-run from `entry_pc` computes
///   the same thing — the rule `replay_from_entry_is_observably_equivalent`
///   applies to the method-entry sinks;
/// * otherwise raises `java.lang.InternalError` through `throw_out`, which is
///   the method-entry sinks' "refusing side-effecting replay" answer: loud,
///   instead of a duplicated side effect nobody can see.
///
/// The caller must `return` this function's result directly.
///
/// `replay_forbidden` (gen r5w1/oom5): the entry parked the frame's reference
/// locals (`CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`), so the frame no longer holds
/// the pre-entry state a replay would resume from, and even a side-effect-free
/// body is refused loudly.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_refuse_replay(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    code: &[u8],
    entry_pc: usize,
    what: &str,
    throw_out: &mut Option<ObjectRef>,
    replay_forbidden: bool,
) -> Option<Option<Value>> {
    shared
        .jit
        .tiered_manager
        .mark_osr_denied(crate::jit::tiered::MethodKey::with_class_id(
            class_id,
            class_name,
            method_name,
            method_descriptor,
        ));
    if !replay_forbidden && !cratonvm_jit::bytecode_commits_side_effect(code, code.len()) {
        return None;
    }
    let message = format!(
        "OSR body of {class_name}.{method_name}{method_descriptor} entered at bci {entry_pc} \
         left compiled code without a transferable state ({what}); refusing side-effecting \
         replay of its loop iterations"
    );
    if let MethodCallFailed::ExceptionThrown(exc) = crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::InternalError { message },
    ) {
        *throw_out = Some(exc);
    }
    None
}

/// `try_osr`'s AIOOBE drain builds its throwable with
/// `create_implicit_exception_object`, which, unlike `throw_runtime_error`,
/// reports heap exhaustion as `Err` instead of throwing the preallocated
/// `OutOfMemoryError`. That drain runs AFTER the OSR body ran, so its `Err` arm
/// ("not entered") replayed every committed iteration whenever the heap was
/// full (round 11 wave 15, lane tier;
/// r11-tier-osr-post-entry-refusals-replay-loop-iterations). This maps exactly
/// that failure to the singleton, the answer `throw_runtime_error` gives, so it
/// takes the drain's exception route; any other failure is returned unchanged.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_drain_oom_singleton(
    shared: &SharedVm,
    failed: MethodCallFailed,
) -> Result<ObjectRef, MethodCallFailed> {
    // gcf f2/o: the preallocated error OF THE MESSAGE, as
    // `throw_runtime_error` answers (`preallocated_oome_for`): under
    // `CRATONVM_GC_PREALLOCATED_OOME_KINDS` a `Requested array size exceeds VM
    // limit` / `Metaspace` refusal used to surface here as the `Java heap
    // space` singleton. Without the switch `preallocated_oome_for` IS the
    // singleton, as before.
    let preallocated = match &failed {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message,
        })) => crate::runtime::exceptions::preallocated_oome_for(
            shared,
            cratonvm_types::error::hotspot_oome_detail(message),
        ),
        _ => None,
    };
    match preallocated {
        Some(oom) => Ok(oom),
        None => Err(failed),
    }
}

/// `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS` — default **on** since gen r5w5/oomjit9
/// (2026-09-27); `=0` is the kill switch: an OSR entry takes the reference
/// locals out of the interpreter frame it leaves behind. Read once.
///
/// gen r5w1/oom5, item 3 of
/// `docs/feature-designs/real-jdk-mode.md`
/// (`NativeGrowthReclaimProbe`). `try_osr` enters the OSR body with a COPY of
/// the frame's locals (`jit_locals`) and leaves `thread.frames[frame_idx]` on
/// the stack, unchanged, for the whole activation. The root scan still reads
/// that frame (`scan_local_objects` by liveness at the entry bci, and the
/// liveness-blind `conservative_locals` pass under any JIT-active collection),
/// so whatever the frame held at entry stayed reachable until the OSR'd method
/// returned -- for an OSR'd `main`, the rest of the program; the census named
/// a dropped 35 MB `ArrayList` at every major.
///
/// With the flag the entry replaces each non-null reference local with null
/// just before the body runs ([`park_osr_entry_reference_locals`]). The
/// values are NOT put back: every way out of the activation that keeps the
/// frame writes the locals it needs (the OSR-exit transfers and the exception
/// handler transfer write every local the artifact publishes, dead ones as
/// `Undefined`, and admission refuses an artifact with an unresumable -- e.g.
/// `Unsupported` -- slot), a return pops the frame, and a propagating
/// exception unwinds it (`OSR_FRAME_DECLINED_TO_CATCH`). The only readers of
/// the PRE-entry locals are the replay arms, which resume at `entry_pc` as if
/// nothing ran; with parked locals they cannot, so each refuses through
/// [`osr_refuse_replay`] (`replay_forbidden`) with its `InternalError`. Those
/// arms are documented as expected-zero and counted (`CRATONVM_DBG_JITC=1`).
///
/// It was opt-in for two reasons, both still true: a JVMTI `GetLocal*` or a
/// debugger reads null from an OSR'd frame during the activation (instead of
/// the pre-entry value, which was stale once the body had run anyway), and
/// the replay arms become loud. gen r5w5/oomjit9 made it the default as a
/// retention fix: with the JIT on, `NativeGrowthReclaimProbe` (`-Xmx128m`)
/// died of `OutOfMemoryError` on the frame's dropped `ArrayList`, which
/// `--nojit` and HotSpot reclaim. The replay arms are documented as
/// expected-zero and counted (`CRATONVM_DBG_JITC=1`); a non-zero count there
/// under this default is the thing to report, with `=0` as the bisect.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_park_entry_locals_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The reference locals [`park_osr_entry_reference_locals`] took out of an
/// OSR'd frame, kept only for the one arm where the body did NOT run.
pub(in crate::runtime::interpreter::jit_bridge) struct OsrParkedEntryLocals {
    pub(in crate::runtime::interpreter::jit_bridge) slots: Vec<(usize, crate::types::CompactValue)>,
    /// `collection_count()` when they were parked: the raw words are
    /// addresses only while no collection has run since.
    pub(in crate::runtime::interpreter::jit_bridge) epoch: u64,
}

impl OsrParkedEntryLocals {
    /// The machine-level entry declined (`osr_enter` / `ir_osr_enter` answered
    /// `None`: nothing ran, and the interpreter resumes this frame at
    /// `entry_pc`). Put the parked references back while they are still
    /// addresses; `false` when a collection ran in between and they cannot
    /// be (the caller refuses the resume rather than run on nulls).
    pub(in crate::runtime::interpreter::jit_bridge) fn restore_after_declined_entry(
        self,
        shared: &SharedVm,
        frame: &mut crate::runtime::frame::Frame,
    ) -> bool {
        if shared.mem.heap.collection_count() != self.epoch {
            return false;
        }
        for (i, cv) in self.slots {
            if i < frame.locals_len() {
                frame.set_local_compact_unchecked(i, cv);
            }
        }
        true
    }
}



/// Replace every non-null reference local of `frame` with null; `None` when
/// there was none. See [`osr_park_entry_locals_enabled`]. Nothing allocates
/// here, and the setter bumps the frame's `exec_epoch`, so a cached root
/// snapshot of this frame is not reused with the old references.
pub(in crate::runtime::interpreter::jit_bridge) fn park_osr_entry_reference_locals(
    shared: &SharedVm,
    frame: &mut crate::runtime::frame::Frame,
    // gcf f1/f: slots the OSR entry's lock snapshot reads back
    // (`OsrEntryHolds::names_slot`) stay in place: each holds a lock the
    // frame owns and its block's `monitorexit` reads, so it is live anyway.
    keep: impl Fn(usize) -> bool,
) -> Option<OsrParkedEntryLocals> {
    let mut slots = Vec::new();
    for i in 0..frame.locals_len() {
        if keep(i) {
            continue;
        }
        // The kind-honouring tag first: a category-2 value whose bits collide
        // with the reference encoding is not a reference (`get_local_raw`).
        if frame.get_local_tag(i) != cratonvm_types::VTAG_OBJECT {
            continue;
        }
        if let Value::Object(Some(_)) = frame.get_local_unchecked(i) {
            slots.push((i, frame.get_local_compact_unchecked(i)));
        }
    }
    if slots.is_empty() {
        return None;
    }
    let epoch = shared.mem.heap.collection_count();
    for &(i, _) in &slots {
        frame.set_local_compact_unchecked(i, crate::types::CompactValue::null());
    }
    Some(OsrParkedEntryLocals { slots, epoch })
}

/// The OSR-exit sink's identity gate: is the stashed frame `ran`'s own exit
/// of the OSR'd method `class_name.method_name descriptor`? The point must be
/// one of `ran`'s (`deopt_stash_is_from_artifact`), and the frame must name
/// the method — by its OUTERMOST scope for an inlined chain
/// (`deopt_resume::stash_identity_scope`; interpreter round i1 wave 26, lane
/// L2), whose own key names the spliced callee that trapped. Before that, a
/// chain exit read as a foreign stash: the innocent callee was charged, the
/// frame's locks released and the committed iterations replayed, while the
/// chain transfer behind the gate (`transfer_osr_exit_chain_into_live_frame`,
/// which checks the outermost scope against the live frame again) was
/// reachable from its unit tests only.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_exit_stash_is_this_bodys(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ran: &crate::jit::CompiledMethod,
    point_addr: usize,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    deopt_frame_matches_artifact(
        super::deopt_resume::stash_identity_scope(rframe),
        ran,
        point_addr,
        class_name,
        method_name,
        descriptor,
    )
}

