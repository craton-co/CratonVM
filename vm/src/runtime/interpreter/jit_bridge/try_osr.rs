#[allow(unused_imports)]
use super::*;

pub(in crate::runtime::interpreter) fn try_osr(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_id: ClassId,
    entry_pc: usize,
    // Out-channel: set to the in-flight throwable when the OSR'd body exited
    // exceptionally and this frame cannot catch it. Written on exactly the
    // paths that used to re-stash + safe-reject; see `OsrBackoffOutcome::
    // ThrowJava` for why the safe reject was wrong there.
    throw_out: &mut Option<ObjectRef>,
    // Out-channel: set when the OSR'd body RAN and this frame was advanced by
    // it, even though the return is `None`. Two paths: the RBC.6b lift's
    // handler entry, where the frame is left parked at a `catch` block with the
    // throwable on its stack, and (wave 4) an OSR-exit TRANSFER whose exit was
    // charged as a failed speculation.
    //
    // `None` from this function otherwise means "the OSR attempt was rejected,
    // nothing ran", and the caller charges it against the per-pc rejection
    // budget (`charge_osr_rejection`, retired for the activation after
    // `OSR_MAX_ATTEMPTS = 5`, and for the install epoch after
    // `OSR_REFUSED_ACTIVATIONS_BUDGET` such activations).
    // Charging a caught exception against that budget would turn OSR off after
    // the fifth `catch` — on a loop with netty's measured 7.7% throw rate, some
    // sixty-five compiled iterations out of four billion.
    committed_out: &mut bool,
) -> Option<Option<Value>> {
    // Not gated on `class_was_redefined`. That predicate is true forever once
    // a class has been redefined, so it barred OSR from a redefined class for
    // the life of the process rather than for the duration of the change.
    // `redefine_class` clears the entire JIT cache, and OSR compiles from the
    // frame's current bytecode, so there is nothing stale left to protect
    // against here. See the note in `interpreter.rs`'s compile gate.
    let frame = &thread.frames[frame_idx];
    // The `OsrEntryState::pc` invariant checked at the entry below (the entry
    // bci must be the frame's current pc), asked here first as well: it is
    // free, and it used to be reached only after a whole OSR compile.
    if entry_pc != frame.pc {
        return None;
    }
    // Neither entry seeds an operand stack: `osr_trampoline` and the optimizing
    // tier's stubs seed locals only, and the `OsrEntryState` below is built with
    // an empty stack. A taken back edge has consumed its operands, so this never
    // refuses today; it is what makes the "live operands are REFUSED" contract
    // true instead of resting on the emitter rule that
    // `CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY=1` switches off (round 11 wave 2).
    if !frame.stack.is_empty() {
        return None;
    }
    // The entry-counter drain's OSR clock (round 11 wave 18, P3): a run whose
    // hot code is all compiled still enters loops here. Strided and gapped;
    // see the function.
    maybe_drain_jit_entry_counters_at_osr_entry(shared);
    // Six heap allocations — three `to_string()` and three `Arc::from(&str)` —
    // used to run here, on EVERY back-edge that reaches this function, which is
    // once per OSR entry and not once per compile. The frame already holds all
    // three as `Arc<str>`, so `*_arc()` is a refcount bump; the `&str` views the
    // rest of the function wants come straight off those.
    //
    // It matters because entries are not rare: since the RBC.6b lift a caught
    // exception is an OSR exit plus a re-entry, so a `try`/`catch` loop takes
    // one entry per throw — measured `osr_entered=2280923` on
    // `probes/OsrExcRateProbe.java`, i.e. 13.7 million allocations that bought
    // nothing. `perf record` on that arm put 8.6% of the run in
    // `mi_malloc`/`mi_free`.
    //
    // `frame.code` was already an `Arc<[u8]>` clone and stays one.
    let class_name_arc: Arc<str> = frame.class_name_arc();
    let method_name_arc: Arc<str> = frame.method_name_arc();
    let descriptor_arc: Arc<str> = frame.method_descriptor_arc();
    let class_name: &str = &class_name_arc;
    let method_name: &str = &method_name_arc;
    let method_descriptor: &str = &descriptor_arc;
    let code = frame.code.clone();
    let max_locals = frame.max_locals as usize;
    // wire-tiered-manager Step 5: the OSR compile (or cache reuse) now lives in
    // `compile_osr_artifact`, which the background worker can also call off-thread.
    // The live-frame entry/transfer below stays on the mutator.
    // ── The optimizing tier, reached through the shared assembly ──────
    //
    // `compile_optimizing_artifact` is the input assembly WITHOUT the
    // method-entry door's admission policy — the split exists because asking
    // that door instead marks the method bail-listed on refusal, and
    // `compile_gate::admit` honours the bail-list here too, so the question
    // could switch off the single-pass OSR that works today.
    //
    // Used only when what comes back carries an entry stub for THIS bci. The
    // lowerer refuses a bci whose live-in set the interpreter cannot supply
    // (`emit_osr_entry_stubs`), so a stub's presence already means seeding here
    // is sound. And only when the body CANNOT RETURN THE SENTINEL: an entered body
    // that exits through one hands back a reconstructed frame, and resuming it
    // in place is what `OsrEntryPlan::resume_after_exit` proves — a proof an
    // optimizing entry does not have, leaving replay-or-lose at the exit. No
    // deopt points means nothing returns the sentinel and that fork is
    // unreachable.
    //
    // Anything else falls through to the single-pass path below, unchanged.
    let osr_opt_key = osr_optimizing_refusal_key(
        shared.vm_identity,
        class_id,
        &method_name,
        &method_descriptor,
        entry_pc,
    );
    // Round 9 wave 11 (`osr11`): a re-entry at the pc this thread last entered
    // -- the next back edge after a caught exception's handler -- takes the
    // artifact straight from the memo while nothing the door's answer depends
    // on has moved. See `OsrReentryMemo` for why a hit is exact. The stamp is
    // read BEFORE either lookup, so a change racing them can only cause a miss.
    // Cast: the bytecode's address is its identity.
    let code_identity = Arc::as_ptr(&code) as *const u8 as usize;
    let reentry_stamp = osr_reentry_stamp(shared);
    let memo_hit = reentry_stamp.and_then(|stamp| {
        osr_reentry_memo_get(
            shared,
            stamp,
            code_identity,
            class_id,
            &class_name_arc,
            &method_name_arc,
            &descriptor_arc,
            entry_pc,
        )
    });
    // Round 11 wave 11: a hit put while the worker still owed its optimizing
    // verdict here yields once that body is cached, so the single-pass body
    // does not pre-empt it at every re-entry. See `OsrReentryMemo`.
    // JIT round 23 wave 1, lane L1 (`CRATONVM_JIT_OSR_PROFIT_EXIT`): nor is a
    // single-pass body armed for a profit exit ever re-entered from the memo:
    // its frame just left it for the optimizing body (asked below), and its
    // forced polls would send the frame straight back out.
    let memo_hit = memo_hit.filter(|hit| {
        !hit.osr_profit_exit_armed()
            && !osr_reentry_memo_defers_to_optimizing(
                shared,
                class_id,
                (class_name, method_name, method_descriptor),
                osr_opt_key,
                entry_pc,
                &code,
            )
    });
    // The artifact this door built the LAST time it was asked for this pc.
    //
    // Reusing it is the whole point: without this the door is a compiler, not a
    // cache, and the single-pass door beside it — which publishes its artifact
    // and reports `OSR-reuse` on every later entry — was the only one of the
    // two that behaved like a tier.
    //
    // An OSR denial (`mark_osr_denied`: the `CRATONVM_OSR_NEWARRAY=0` lever, a
    // background worker's verdict, a body that panicked) stops this path too.
    // It used to be asked only by `compile_osr_artifact` and the re-entry memo,
    // so a denied method could still be entered through a body served from
    // the accepted-artifact memo. Asked last, so it costs nothing when the
    // optimizing door is off or the memo hit.
    //
    // `_fresh_callee_pins` keeps the callees a freshly built optimizing body
    // CALLs alive until this door returns, for the case where the memo is off
    // (`CRATONVM_JIT_OSR_OPTIMIZING_CACHE=0`) and nothing else holds them.
    //
    // The whole-method levers (`CRATONVM_DISABLE_JIT`, `CRATONVM_JIT_DENY`,
    // `CRATONVM_JIT_BISECT_ONLY`: `compiled_execution_forbidden`) are asked
    // here too. They were asked only by `compile_osr_artifact`, which this arm
    // runs BEFORE, so a force-interpreted method still got an optimizing OSR
    // body built and entered on its first hot back edge -- a bisect step that
    // cannot stop a compile reads as an exoneration
    // (`i1-L7-osr-optimizing-door-bypasses-osr-admission`, first stage of the
    // shared admission). The single-pass door then refuses and denies it.
    let mut _fresh_callee_pins: Option<Arc<[cratonvm_jit::RetainedCode]>> = None;
    // Set by tier proposal 29's arm below: the optimizing verdict for this pc
    // is the worker's, still owed, so a re-entry memo entry put by this call
    // is marked `optimizing_pending` (round 11 wave 11).
    let mut optimizing_verdict_pending = false;
    let ir_osr: Option<Arc<cratonvm_jit::CompiledMethod>> = if memo_hit.is_some()
        || !osr_optimizing_tier_enabled()
        || cratonvm_jit::compile_gate::compiled_execution_forbidden(class_name, method_name)
        // The GPU-offload gate, likewise asked only by `compile_osr_artifact`
        // before: fall through to it, which refuses and denies the method.
        || osr_gpu_offload_blocks(shared, class_id, method_name, method_descriptor)
        || shared
            .jit
            .tiered_manager
            .is_osr_denied(&crate::jit::tiered::MethodKey::with_class_id(
                class_id,
                Arc::clone(&class_name_arc),
                Arc::clone(&method_name_arc),
                Arc::clone(&descriptor_arc),
            )) {
        None
    } else if let Some(cached) = osr_optimizing_cached(
        shared,
        class_id,
        (class_name, method_name, method_descriptor),
        osr_opt_key,
        entry_pc,
        &code,
    ) {
        cratonvm_jit::metrics::record_osr_event("osr_optimizing_artifact_reused");
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing REUSE {class_name}.{method_name} pc={entry_pc}"
            );
        }
        Some(cached)
    } else if osr_optimizing_build_declined(
        shared,
        class_id,
        class_name,
        method_name,
        method_descriptor,
        osr_opt_key,
        entry_pc,
        &code,
        "",
    ) {
        // The memo, the single-pass preference and the static gates, shared
        // with the background worker's builder (round 11 wave 9): the
        // single-pass body, with no compile.
        None
    } else if crate::runtime::env_cache::bg_compile()
        && !osr_optimizing_cache_disabled()
    {
        // Tier proposal 29 (round 11 wave 10): under background compilation
        // the optimizing body is never built on the MUTATOR. The caller
        // reaches this door under `CRATONVM_BG_COMPILE` only when a body is
        // already enterable here, and since wave 9 the worker's OSR task
        // judges the optimizing route before any single-pass body publishes.
        // What still arrived at this arm was, in the default configuration,
        // a loop header the worker's pc-blind build had no stub for (so the
        // rebuild here found none either), or a header no task was queued
        // for. Either way the answer now comes from the worker: queue an OSR
        // task for this pc (idempotent; refused while the method's OSR slot is
        // held) and enter the single-pass body. The task's optimizing verdict
        // is memoed, and an accepted body is served from the cache at the next
        // entry. `CRATONVM_JIT_OSR_OPTIMIZING_BG=0` restores the mutator build,
        // and so does `CRATONVM_JIT_OSR_OPTIMIZING_CACHE=0`: with no cache the
        // worker's body could never be served, and that arm exists to measure
        // a rebuild on every entry.
        ensure_bg_compiler_started(shared);
        let queued = shared
            .jit
            .tiered_manager
            .request_osr(
                &crate::jit::tiered::MethodKey::with_class_id(
                    class_id,
                    Arc::clone(&class_name_arc),
                    Arc::clone(&method_name_arc),
                    Arc::clone(&descriptor_arc),
                ),
                // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
                entry_pc as u32,
            )
            .is_some();
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing {class_name}.{method_name} pc={entry_pc}: \
                 not built on the mutator; OSR task {}",
                if queued {
                    "queued for the worker"
                } else {
                    "not queued (slot held or OSR refused)"
                }
            );
        }
        optimizing_verdict_pending = true;
        None
    } else {
        // The frame's own handle when it has one, and a resolution when it does
        // not. `main` is entered by the launcher rather than through the invoke
        // cache, so its frame carries no `CachedBytecodeMethod` — and a method
        // whose only route to compiled code is a back edge is very often
        // exactly that shape, so taking the frame's handle alone made this
        // inert on the population it was built for.
        let cached_for_compile = thread.frames[frame_idx]
            .cached_method()
            .cloned()
            .or_else(|| {
                let key = format!("{class_name}.{method_name}:{method_descriptor}");
                super::deopt_resume::resolve_inlined_callee(shared, class_id, &key).ok()
            });
        cached_for_compile
            .or_else(|| {
                if crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] osr optimizing {class_name}.{method_name}: no cached method"
                    );
                }
                None
            })
            .and_then(|c| {
                // Build, judge and remember — one builder, shared with the
                // background worker's OSR task (round 11 wave 9).
                //
                // Under background compilation this arm is reached only when
                // `CRATONVM_JIT_OSR_OPTIMIZING_BG=0` or the cache is off (tier
                // proposal 29 routes every other build to the worker), and then
                // the build runs on the MUTATOR. Counted and timed
                // (`i1-L7-osr-optimizing-door-bypasses-osr-admission`, item 2),
                // one clock read per build.
                let mutator_build_started =
                    crate::runtime::env_cache::bg_compile().then(std::time::Instant::now);
                let built = build_osr_optimizing_artifact(
                    shared,
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                    &c,
                    &code,
                    entry_pc,
                    "",
                );
                if let Some(started) = mutator_build_started {
                    note_osr_optimizing_mutator_build(started.elapsed());
                }
                let (artifact, callee_pins) = built?;
                _fresh_callee_pins = Some(callee_pins);
                Some(artifact)
            })
    };
    // Slot kinds (interpreter round i1 wave 10, item 3 of
    // `osr-optimizing-door-bypasses-osr-admission`): the optimizing stub seeds
    // each local as the IR type its body reads, and nothing else compares that
    // with the interpreter's tags (`validate_osr_entry` is the single-pass
    // tier's check). A disagreement does not enter the optimizing body; the
    // offer falls to the single-pass door below. State-dependent, so nothing
    // is memoised. One pass over the seeded locals' tags per optimizing entry.
    let mut optimizing_kinds_refused = false;
    let ir_osr = ir_osr.filter(|c| {
        let frame = &thread.frames[frame_idx];
        // Cast: a bci fits u32.
        match c.ir_osr_entry_kind_mismatch(entry_pc as u32, |i| {
            (i < frame.locals_len()).then(|| frame.get_local_tag(i))
        }) {
            None => true,
            Some(local) => {
                dbg_osr_optimizing_kind_refusal(class_name, method_name, entry_pc, local);
                optimizing_kinds_refused = true;
                false
            }
        }
    });
    // Under background compilation the single-pass door below may only REUSE a
    // published body here; with none, the offer is declined (nothing ran)
    // rather than compiled on the mutator.
    if optimizing_kinds_refused
        && memo_hit.is_none()
        && crate::runtime::env_cache::bg_compile()
        && !shared
            .jit
            .jit_cache
            .read()
            .get_osr(&class_name_arc, &method_name_arc, &descriptor_arc, class_id)
            .is_some_and(|c| c.compiled_via_osr && c.can_osr_enter(entry_pc))
    {
        return None;
    }
    let compiled = match ir_osr {
        Some(c) => {
            cratonvm_jit::metrics::record_osr_event("osr_entered_optimizing");
            // Dump the body this door is about to ENTER, under a label that
            // distinguishes it from the single-pass one.
            //
            // Without this the optimizing artifact is invisible to
            // `CRATONVM_DBG_JIT_DISASM`: the only `osr` dump comes from inside
            // `compile_osr_artifact`, which this arm SKIPS — and the background
            // tier worker calls that function anyway, so a dump appears, is
            // labelled `osr`, and is the single-pass body. Reading it while the
            // door was on showed code that did not change when the residency
            // flags changed, which is exactly the wrong conclusion and cost
            // several rounds to catch. The counter said the door had engaged
            // and the disassembly said it had not; the disassembly was of
            // another artifact.
            crate::jit::disasm::maybe_dump(
                if c.used_ir_backend {
                    "osr-optimizing/ir"
                } else {
                    "osr-optimizing/sp"
                },
                &class_name_arc,
                &method_name_arc,
                &descriptor_arc,
                c.entry_ptr(),
                c.code_bytes(),
            );
            if reentry_stamp.is_some() {
                osr_reentry_memo_clear();
            }
            c
        }
        // Round 15 wave 1 (lane calls): a single-pass entry at this pc that
        // `validate_osr_entry` refused for good (memoed by the refusal arm
        // below, which also empties the re-entry memo) is not offered again:
        // the reuse path would hand back the same artifact and the same
        // refusal on every back-edge offer (`OSR-reuse(memo)` / `OSR-refuse`
        // alternating on `R12Mega4OneSite.osrStatic24`). A recompile would
        // stop at the same memo in `compile_osr_body`. The optimizing route
        // above is not affected. `CRATONVM_JIT_OSR_REJECTED_ENTRY_NOT_REOFFERED=0`
        // restores the re-offer.
        None if memo_hit.is_none()
            && jit_verdicts(shared).is_osr_entry_rejected(
                class_id,
                &class_name,
                &method_name,
                &method_descriptor,
                entry_pc,
            ) =>
        {
            return None;
        }
        None => match memo_hit {
            Some(c) => {
                if crate::runtime::env_cache::dbg_jitc() {
                    dbg_osr_reentry_memo_hit(
                        &c,
                        class_name,
                        method_name,
                        method_descriptor,
                        entry_pc,
                    );
                }
                c
            }
            None => {
                let c = compile_osr_artifact(
                    shared,
                    class_id,
                    class_name.to_string(),
                    method_name.to_string(),
                    method_descriptor.to_string(),
                    OsrCodeSource::Frame {
                        code: &code,
                        max_locals,
                    },
                    entry_pc,
                );
                match (c, reentry_stamp) {
                    (Some(c), Some(stamp)) => {
                        osr_reentry_memo_put(
                            shared,
                            stamp,
                            code_identity,
                            class_id,
                            &method_name_arc,
                            &descriptor_arc,
                            entry_pc,
                            &c,
                        );
                        if optimizing_verdict_pending {
                            osr_reentry_memo_set_optimizing_pending(true);
                        }
                        c
                    }
                    (Some(c), None) => c,
                    (None, _) => return None,
                }
            }
        },
    };

    // Convert interpreter locals to i64 for JIT frame (raw u64 → i64 reinterpret),
    // reading each slot's VTAG byte from the SAME snapshot as its word.
    // `Frame::get_local_tag` exists precisely "for JIT/OSR interop"
    // (`vm/src/runtime/frame.rs`) and was never actually passed to the JIT: until
    // the validated entry landed, `osr_enter` took raw words with no types
    // attached, so nothing compared the interpreter's idea of a slot against the
    // compiled entry's. A `double` seeded into a GPR home, or a `long` seeded
    // where the compiled body reads a reference, is a silent miscompile.
    //
    // Both halves must come from one uninterrupted read of the live frame, BEFORE
    // `set_jit_thread`, so no intervening safepoint can retype a slot between its
    // word and its tag. That is an invariant of the validated entry, not an
    // accident of this call site — see `docs/jit/on-stack-replacement.md` §6.
    let frame = &thread.frames[frame_idx];
    let num_locals = frame.locals_len();
    let mut jit_locals = Vec::with_capacity(num_locals);
    let mut jit_local_tags = Vec::with_capacity(num_locals);
    for i in 0..num_locals {
        jit_locals.push(frame.get_local_raw(i) as i64); // Cast: JIT ABI -- i64 register convention
        jit_local_tags.push(frame.get_local_tag(i));
    }
    // The other invariant of §6: `OsrEntryState::pc` is the frame's CURRENT pc.
    // Every back-edge site captures `entry_pc = frame.pc` after the branch was
    // taken (`interpreter.rs`, 14 sites) and nothing between there and here moves
    // it, so the entry bci and the "nothing ran" fallback bci are the same value —
    // which is what makes a refusal cost no replay. Check it instead of trusting
    // it: a future trigger passing some other pc must be refused, not silently
    // entered at a bci the interpreter is not standing on.
    if entry_pc != frame.pc {
        if dbg_osr_enabled() {
            eprintln!(
                "[cratonvm-osr] REFUSE {}.{}{} entry_pc={} != frame.pc={} \
                 (OsrEntryState::pc must be the frame's current pc)",
                &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc, frame.pc
            );
        }
        return None;
    }
    if dbg_osr_enabled() {
        eprintln!(
            "[cratonvm-osr] enter {}.{}{} entry_pc={} num_locals={} locals={:?} tags={:?}",
            &*class_name_arc,
            &*method_name_arc,
            &*descriptor_arc,
            entry_pc,
            num_locals,
            jit_locals,
            jit_local_tags
        );
    }

    // The interpreter's offer. The operand stack at a taken back-edge is empty by
    // construction (the branch already consumed its operands) and a non-empty one
    // was refused at the top of this function, so the empty slices below are the
    // frame's real stack, not a placeholder — `osr_trampoline` seeds locals only
    // and has no stack-seeding path.
    let osr_state = cratonvm_jit::OsrEntryState {
        pc: entry_pc,
        locals: &jit_locals,
        local_tags: &jit_local_tags,
        stack: &[],
        stack_tags: &[],
    };

    // ADMISSION. `validate_osr_entry` type-checks every offered slot against the
    // compiled entry's contract and — the part that matters for correctness —
    // walks every deopt point the artifact can exit through, refusing the entry
    // outright (`osr-entry-unresumable-exit`) when one of them reconstructs a
    // frame the in-place transfer could not resume. Discovering that AFTER
    // entering is useless: by then the body has committed iterations, and the only
    // remaining options are to replay them (the recorded
    // `jit-osr-bail-reruns-loop-iterations` defect) or to lose them.
    //
    // A refusal is free of side effects — the check reads metadata and these two
    // slices, allocates one `Vec`, and never enters compiled code — so falling
    // back to `entry_pc`, where the interpreter already is, replays nothing.
    // An optimizing-tier stub is admitted by its OWN construction, not by
    // `validate_osr_entry`: that check proves the offered slots against the
    // single-pass entry contract — `osr_num_locals`, the per-method local
    // assignments, the deopt points `osr_trampoline` would seed through — and
    // an SSA body has none of those. What stands in its place is the refusal
    // the lowerer already made: a bci gets a stub only when every value live on
    // entry is one the snapshot names, and `ir_osr_enter` refuses a locals
    // slice shorter than the stub reads.
    // Cast: a bci fits u32.
    if thread.jdk_continuation.jdk() {
        let cached = thread.frames[frame_idx].cached_method()?.clone();
        if !vthread_body_admitted(shared, thread, &compiled, &cached, true) {
            return None;
        }
    }
    let ir_entry = compiled.ir_osr_entry_addr(entry_pc as u32);
    let plan = match if ir_entry.is_some() {
        // Nothing to validate, and nothing validated: skip straight past.
        Ok(None)
    } else {
        compiled.validate_osr_entry(&osr_state).map(Some)
    } {
        Ok(plan) => plan,
        Err(b) => {
            // Only an ARTIFACT-level verdict may be memoed: it is a pure function
            // of a deterministic compile, so it reproduces for every future
            // back-edge over this pc and re-running the pipeline can only reach it
            // again. A state-dependent refusal (a slot's type, the local count,
            // live operands) must NOT be memoed — the next trip over the back-edge
            // carries different locals and may well be admissible.
            cratonvm_jit::metrics::record_osr_event("osr_refused_entry");
            let permanent = cratonvm_jit::osr_refusal_is_permanent(&b);
            if permanent {
                // `_by`: a refusal that depends on compile-time state expires
                // when that state is flushed, instead of standing for good.
                jit_verdicts(shared).mark_osr_entry_rejected_by(
                    class_id,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    entry_pc,
                    &b,
                );
                // Round 15 wave 1 (lane calls): and not served again from
                // the re-entry memo, which never asks the reject memo (the
                // single-pass arm above does, on a memo miss).
                osr_reentry_memo_clear();
            }
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] OSR-refuse {}.{}{} entry_pc={entry_pc} {b}{}",
                    &*class_name_arc,
                    &*method_name_arc,
                    &*descriptor_arc,
                    if permanent { " (memoed)" } else { "" }
                );
            }
            // Nothing ran: keep interpreting THIS frame at `entry_pc`.
            return None;
        }
    };

    // A single-pass body can leave its loop from a back-edge poll when an agent
    // needs the interpreter (`jvmti_events::request_compiled_loop_exits`,
    // interpreter round i1 wave 12, lane L4). Counted in first, then the mode
    // asked again: an agent that published it after the OSR gate in
    // `try_osr_with_backoff` asked, and before this count, either sees the
    // count and pauses, or is seen here and the entry is declined. Nothing
    // has run. Since wave 15 (lane L2) an optimizing body entered through its
    // own stub (no plan) can too, when its lowering gave an unconditional
    // back-edge poll a mode exit (`osr_exit::has_poll_mode_exits`).
    let leaves_at_its_polls =
        plan.is_some() || cratonvm_jit::osr_exit::has_poll_mode_exits(&compiled._deopt_point_boxes);
    let osr_body_running = leaves_at_its_polls.then(|| OsrBodyRunning::enter(shared));
    if osr_body_running.is_some() {
        let frame = &thread.frames[frame_idx];
        if super::jvmti_requires_interpreter_for_method(
            shared,
            frame.class_id,
            frame.method_name(),
            frame.method_descriptor(),
        ) {
            return None;
        }
    }
    // Interpreter round i1 wave 24, lane L6: never start a body a class
    // redefinition withdrew. The lookups above miss one (the memo's code-state
    // epoch, the cache's eviction), but this door may have passed them just
    // before the redefinition. Asked after the running count: either the
    // redefinition's loop-exit decision (`request_withdrawn_body_exits`, which
    // skips its pause when no body runs) sees this body counted, or this read
    // sees the withdrawal; both sides fence. Nothing has run.
    // Interpreter round i1 wave 40, lane L2 follow-up: never start an OSR body
    // while a multi-class redefinition call of this VM installs
    // (`JitCache::hold_publications`). The optimizing memo and a mutator
    // build hand out bodies that were never published, so the cache's
    // refusal alone does not cover them: one built between two installs could
    // splice the new body of an installed class beside the old body of one
    // not yet installed. One load; nothing has run, and the loop keeps
    // interpreting (held at its top by the call's fence anyway).
    if shared.jit.jit_cache.publications_held() {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] OSR entry REFUSED (a redefinition call is installing): {} id={} pc={entry_pc}",
                compiled.method_label, compiled.compile_id
            );
        }
        return None;
    }
    if shared
        .jit
        .jit_cache
        .body_withdrawn_by_redefinition(&compiled)
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] OSR entry REFUSED (withdrawn by a redefinition): {} id={} pc={entry_pc}",
                compiled.method_label, compiled.compile_id
            );
        }
        return None;
    }
    // Wave 25 follow-up (lane L6c): a body that copied the bytecode of a
    // class redefined after this frame began may have folded values the frame
    // computed with the class's OLD code. The optimizing tier compiles the
    // method from its entry and enters through stubs that seed only the
    // header's live homes: a loop-invariant local whose value the graph
    // proved from the entry path -- `int before = Callee.value()` spliced to
    // a constant -- has no home, so the frame's own value is ignored and the
    // NEW constant stands in for it (the host run of
    // `L6/RedefineSpliceAcrossACallProbe`: the loop re-entered a body
    // compiled after the retransform and compared the new value against the
    // new value). Such a frame stays interpreted; HotSpot's OSR compile takes
    // every local from the frame, so its body never assumes one.
    if crate::classloading::any_class_redefined()
        && osr_body_may_fold_code_the_frame_predates(
            shared,
            thread.frames[frame_idx].redefine_stamp(),
            &compiled,
        )
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] OSR entry REFUSED (the frame predates a redefinition of a class \
                 the body copied): {} id={} pc={entry_pc} frame_stamp={}",
                compiled.method_label,
                compiled.compile_id,
                thread.frames[frame_idx].redefine_stamp(),
            );
        }
        return None;
    }
    // Interpreter round i1 wave 39, lane L2: the body must read the pool
    // generation the frame's code indexes, whatever the bytes say (a
    // redefinition that changed only a constant leaves them equal). Asked on
    // the body every route above produced; nothing has run.
    if osr_body_generation_differs_from_frame(shared, &thread.frames[frame_idx], &compiled) {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] OSR entry REFUSED (the body's constant-pool generation is not \
                 the frame's): {} id={} pc={entry_pc} frame_stamp={} obsolete={} body_stamp={:?}",
                compiled.method_label,
                compiled.compile_id,
                thread.frames[frame_idx].redefine_stamp(),
                thread.frames[frame_idx].runs_obsolete_method(),
                compiled.compile_cp_stamp(),
            );
        }
        return None;
    }
    // Wave 25 follow-up (lane L6b): once any redefinition withdrew a body,
    // name every body this door starts, with its age, so a run that re-enters
    // pre-redefinition code shows which door handed it out. Wave 39 (lane L2):
    // with both constant-pool stamps, so a run shows which generation each
    // side read. Wave 40 (lane L2): and whether the frame was moved across a
    // redefinition (`predates=true`: an EMCP activation of an older version,
    // admitted because its translated body is the current one).
    if crate::runtime::env_cache::dbg_jitc()
        && shared.jit.jit_cache.bodies_withdrawn_by_redefinition() != 0
    {
        eprintln!(
            "[cratonvm-jitc] OSR entry after a redefinition: {} id={} pc={entry_pc} ir={} \
             install_epoch={} exit_sites={} frame_stamp={} body_stamp={:?} predates={}",
            compiled.method_label,
            compiled.compile_id,
            compiled.used_ir_backend,
            compiled.install_epoch,
            compiled.exit_poll_site_count(),
            thread.frames[frame_idx].redefine_stamp(),
            compiled.compile_cp_stamp(),
            thread.frames[frame_idx].predates_its_class_redefinition(),
        );
    }

    // osr-02 frame comparator: record the ENTRY frame — the state compiled code
    // is about to start from. Here, because the entry has validated and nothing
    // has run yet.
    //
    // Without this record the comparator cannot see a replay at all: compiled
    // iterations produce no back-edge arrivals, so "entered at frame 5, ran to
    // 12, resumed at 5" and "entered at 5 and advanced nothing" are the same
    // sequence of arrival indices, both strictly increasing. A hand-written
    // fixture modelling the historical defect passed without it. Paired with
    // the next exit record this makes the advance a MEASURED quantity —
    // `index(X) - index(E)` over the un-compiled run's own trajectory, not
    // anything the JIT claims.
    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_entry(&thread.frames[frame_idx], entry_pc);
    }

    // A deopt flag pending HERE is stale by construction: it describes an
    // `i64::MIN` some earlier compiled body returned, and this body has not run.
    // Left set, the body's first `J`/`D` call site that receives a genuine
    // `Long.MIN_VALUE` reads it through `jit_dispatch_threw` as a sentinel and
    // bails the whole OSR activation, and the "no frame" arm below then replays
    // every iteration the body committed (interpreter round i1 wave 4, L7). The
    // interpreted loop that led here can leak one through any door that drains
    // selectively. One thread-local store per OSR entry.
    let _ = crate::jit::helpers::take_jit_deopt_pending();
    // Interpreter round i5 wave 104, lane L2: so is a throw-site stamp; this
    // body's exits write their own.
    crate::jit::helpers::clear_jit_athrow_bci();
    // The exceptional stash's depth before the body runs, for the sentinel
    // arm's claim of this body's own reason-9 frames (interpreter round i1
    // wave 25, lane L2; `run_jit_body_raw` records the same floor).
    let exc_floor = cratonvm_jit::deopt::exceptional_stash_depth();
    // Interpreter round i1 wave 28, lane L2: the locks this frame's thread
    // owns through its reference locals, with their hold counts, before the
    // body runs and before the locals are parked below. The two arms that
    // release what an abandoned frame of this body took while the live frame
    // carries on (`release_beyond_live_record`: the sentinel arm and the
    // chain refusal) never release below these counts, so a record that
    // under-states (a committed single-pass exit that named no lock emptied
    // it) cannot cost the frame its own lock. See `OsrEntryHolds`.
    let osr_entry_holds = super::deopt_resume::OsrEntryHolds::snapshot(shared, thread, frame_idx);

    // gen r5w1/oom5 (`CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`, default on since
    // gen r5w5/oomjit9): the body
    // owns the locals now (it was seeded from `jit_locals`); left in the
    // frame, they are roots for the whole activation. Admitted, nothing has
    // run, and nothing below allocates before the body starts. x86-64 only,
    // the one target that enters an OSR body at all (see the `cfg` arms
    // below), so the non-x86 "nothing ran" `None` never sees parked locals.
    let parked_locals = if cfg!(target_arch = "x86_64") && osr_park_entry_locals_enabled() {
        park_osr_entry_reference_locals(shared, &mut thread.frames[frame_idx], |i| {
            osr_entry_holds.names_slot(i)
        })
    } else {
        None
    };
    let parked_entry_locals = parked_locals.is_some();
    // gen r5w1/oom5 (`CRATONVM_JIT_OSR_DROP_ORPHANS`, default on): the stash depth
    // at entry. Every frame above it when the body has returned was published
    // by a compiled frame of this activation, which is gone.
    let entry_stash_floor =
        true.then(cratonvm_jit::deopt::exceptional_stash_depth);
    // HANDLER11-1: the ordinary stash's push mark at entry; the sentinel arm
    // claims only a frame pushed after it (`take_door_stash_since_entry`).
    let entry_stash_mark = cratonvm_jit::deopt::deopt_stash_push_mark();

    // gci i11/vt item 10: an OSR entry is machine code too.
    assert_no_compiled_continuation(thread);
    // Set JIT thread for invoke dispatch callbacks (save/restore for re-entrancy)
    let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);
    // Interpreter round i6 wave 119, lane L2 (`CRATONVM_JIT_POLL_THREAD_WORD`,
    // default off): compiled polls of THIS VM may read this OS thread's loop
    // poll word, registered with this VM's barrier first; restored (to the
    // enclosing door's value, or null) when this door returns. Off: one
    // cached-flag load.
    let _poll_word_mirror = crate::jit::helpers::PollWordMirrorScope::enter(shared);
    // Capture this `*mut JvmThread` so the OSR trampoline can cache it and the
    // OSR-entered frame's safepoints push/reload precisely. `set_jit_thread`
    // just allocated this thread's shadow stack; the value is a raw address
    // (Copy `i64`, holds no borrow), so the closure below captures it by value
    // and `thread` stays free for later use.
    // Cast: reinterpret pointer/address to typed pointer
    let thread_ptr = thread as *mut JvmThread as i64;
    // NEW-1.5 + T1.1.a: record native stack pointer for GC root scan.
    // Uses the precise-oop-map path when the compiled method has
    // populated maps; falls back to conservative otherwise.
    let _qd0 = cratonvm_gc::gc_quiescence::depth();
    let vm_ptr = shared as *const _ as i64; // Cast: JIT ABI -- pointer to i64 register
    let result_i64 = {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::JitExecution);
        let _jit_root_guard = crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
            &*compiled,
            Some(thread.frames.len()),
        );
        // Publish this activation for exactly the window it is on the stack --
        // see the "Which interpreter frames are, right now, being run by
        // compiled code" block near the top of this file for why a trace needs
        // it and why `frame.pc` itself must not be moved instead. Deliberately
        // in the same scope as `_jit_root_guard` and taking the same
        // `thread.frames.len()`, so the two records agree about the depth by
        // construction rather than by convention. Declared second, so it is
        // withdrawn FIRST: there is never an instant where the registry claims
        // an activation whose chain entry has already gone.
        //
        // Cost: one push and one truncate per OSR ENTRY. Nothing is added to
        // the back-edge poll (`should_try_osr` returns long before this
        // function is reached) and nothing at all to the compiled loop; an
        // entry already pays a cache lookup, two `Vec`s of locals and tags, and
        // `validate_osr_entry`'s walk over every deopt point, each of which
        // dwarfs this.
        let _osr_continuation =
            OsrContinuationGuard::publish(thread.frames.len(), Arc::as_ptr(&compiled) as usize);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: compiled is a finalized JIT CompiledMethod whose entry point
            // was validated; `plan` is the proof, obtained from
            // `validate_osr_entry` on THIS artifact with THIS state just above, so
            // every seeded slot's JVM type has been checked against the compiled
            // entry's contract and `osr_state.locals` matches `osr_num_locals`.
            //
            // x86-64 only: `osr_enter_planned` is `#[cfg(target_arch =
            // "x86_64")]`. Reaching here off x86-64 would mean an artifact
            // published OSR entry points, and no other backend does, so
            // `should_try_osr` refuses long before here.
            #[cfg(target_arch = "x86_64")]
            {
                match &plan {
                    // The single-pass trampoline, with its proof.
                    // SAFETY: `plan` is `validate_osr_entry`'s result for THIS
                    // artifact at THIS bci, so every seeded slot's JVM type has
                    // been checked against the compiled entry's contract — the
                    // argument spelled out above this `cfg` block.
                    Some(plan) => unsafe {
                        compiled.osr_enter_planned(vm_ptr, &osr_state, plan, thread_ptr)
                    },
                    // The optimizing tier's own stub. It builds this tier's
                    // frame, seeds the locals the snapshot at this bci names,
                    // and jumps into the body — two arguments where the
                    // trampoline takes twenty layout fields, because the
                    // lowerer knows the layout and the trampoline never could.
                    // SAFETY: a DIFFERENT contract from the arm above, and the
                    // reason each arm states its own: this entry takes no plan,
                    // so what must hold is that `entry_pc` is an OSR entry the
                    // artifact published and `jit_locals` matches the snapshot
                    // that bci names.
                    None => unsafe { compiled.ir_osr_enter(entry_pc as u32, vm_ptr, &jit_locals) },
                }
            }
            // `None`, NOT `unreachable!()`. The reasoning above is sound and
            // the panic was still the wrong answer twice over. `None` is this
            // call's existing word for "no entry was taken" -- the caller
            // matches `Ok(None) => return None` and the interpreter carries on
            // in the frame it is already in -- so an argument that turns out to
            // be wrong on some future backend degrades to running the loop
            // interpreted instead of aborting the VM. And the panic-free gate
            // over this module is a TEXT scanner: it does not evaluate `cfg`,
            // so a site compiled out of every x86-64 build still counted
            // against a budget of zero and turned the gate red for everyone.
            #[cfg(not(target_arch = "x86_64"))]
            {
                let _ = (&osr_state, &plan, thread_ptr, vm_ptr, &jit_locals);
                None
            }
        }));
        // DBG: detect a quiescence LEAK across the OSR call (a nested JIT entry
        // that did not pop). Before this site's own guard drops, depth should be
        // back to _qd0 + 1. Anything higher leaked.
        {
            let now = cratonvm_gc::gc_quiescence::depth();
            if now > _qd0 + 1
                && false
            {
                eprintln!(
                    "[quiesce-leak] OSR site leaked: depth before={} after={} (expected {})",
                    _qd0,
                    now,
                    _qd0 + 1
                );
            }
        }
        result
    };

    crate::jit::helpers::restore_jit_thread(saved_jit_thread);

    // An entry was taken. Counted here rather than before the call so a panic
    // inside compiled code is not reported as a successful entry, and counted
    // unconditionally because the whole point of `osr_entered` is to be the
    // denominator `osr_exited` is read against.
    cratonvm_jit::metrics::record_osr_event("osr_entered");

    // FIX (OSR uncommon-trap fallthrough, HHH-15895 `InPredicateTest`):
    // `jit_uncommon_trap` (used by, among others, the invokedynamic 0xba arm's
    // unconditional deopt stub — see its `DeoptReason::UnreachedCode` doc
    // comment) signals a deopt via `set_jit_deopt_pending()` alone; unlike the
    // guard-based `x64_deopt_entry` / `ir_deopt_entry` trampolines, it does NOT
    // stash a frame in `cratonvm_jit::deopt::LAST_DEOPT`. Drain the flag now,
    // before the exception-specific drains below, so the `result_i64 ==
    // i64::MIN` check further down can distinguish "a real uncommon-trap
    // deopt with no reconstructed frame" from "a genuine `Long.MIN_VALUE`
    // return". Without this, the former fell through to the return-value
    // conversion, which for a reference-typed method reinterprets the raw
    // `i64::MIN` sentinel bits as a heap pointer — observed as `values` (a
    // live, non-empty `List` local read back null) in Hibernate's
    // `InPredicateTest`, whose `getNames()`-style hot loop contains a live
    // (non-dead-assert) invokedynamic string concatenation.
    let deopt_signaled = crate::jit::helpers::take_jit_deopt_pending();
    // Interpreter round i5 wave 104, lane L2 (page
    // `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
    // this drain takes the OSR body's throwable and signals one by one and
    // never reads the throw-site stamp (`route_osr_exception_out_of_artifact`
    // decides from the precise frame). It used to leave the stamp in place,
    // so after the interpreter caught the exception in this frame the next
    // door drain, on some unrelated body's ordinary return, read a stale stamp
    // of this method: the w104 `foreign throw stamp refused` lines labelled
    // `ArrayList.<init>`, `NullPointerException.<init>`,
    // `Reference.refersToImpl`. Retire it with the signals it belongs to.
    crate::jit::helpers::clear_jit_athrow_bci();

    // Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): the previous OSR
    // exit drain *consumed* the pending-exception, pending-NPE, and
    // pending-AIOOBE flags with `let _ =` / `if let Some(_exc)` on the
    // (false) assumption that falling back to the interpreter "re-executes
    // from the interpreter PC, which will issue the same null-deref / oob
    // access and surface the exception". That assumption is wrong: OSR
    // hands control back at the back-edge PC, NOT at the JIT helper's PC,
    // so the failing access is never re-executed and the exception was
    // silently lost (visible only as a wrong-result or downstream NPE at
    // an unrelated site).
    //
    // The fix: take the flags so we can inspect them, but if any were set,
    // re-stash them onto the same TLS slot via the new
    // `stash_jit_pending_*` helpers. The interpreter dispatch loop drains
    // `take_jit_pending_exception` and the NPE / AIOOBE flags at the next
    // JIT helper return (see lines ~2249 / ~2267 and the post-JIT path at
    // ~12696). Re-stashing preserves the exception across the
    // OSR→interpreter handoff without expanding the OSR signature
    // (`Option<Option<Value>>`, no error channel).
    if let Some(exc) = crate::jit::helpers::take_jit_pending_exception(thread) {
        if dbg_osr_enabled() {
            let cid = shared.mem.heap.class_id_of(exc);
            let cname = shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            eprintln!(
                "[cratonvm-osr] BAIL pending_exception {}.{}{} entry_pc={} exc_class={}",
                &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc, cname
            );
        }
        // FIX (jit-osr-bail-on-callee-exception-reruns-loop-iterations,
        // silent wrong answers on default settings): re-stashing and
        // continuing is "OSR rejected, keep interpreting THIS frame from where
        // it was". That is correct only when the bail precedes any committed
        // loop iteration — true for the unconditional-at-header OSR-exit
        // trigger it was written for, false here: the exception surfaces at an
        // arbitrary invoke, possibly thousands of iterations into the loop, and
        // the interpreter frame's induction variable and accumulators were
        // never advanced by the OSR'd code. Every iteration between OSR entry
        // and the throw was therefore executed a SECOND time (measured: 20 000
        // requested, 20 008 executed on the doc's repro; 12 346 requested,
        // 42 730 executed when the exception escapes the OSR'd method
        // entirely).
        //
        // Since the RBC.6b lift (2026-08-17) this frame may also CATCH, which
        // it never could before — `route_osr_exception_out_of_artifact` is
        // where that is decided, from the precise reason-9 throw bci rather
        // than from the stale back-edge pc. It also owns the exceptional-frame
        // stash (taking a frame that names this method, re-stashing one that
        // names a callee), which is what `drop_own_exceptional_frame` used to
        // do here.
        match route_osr_exception_out_of_artifact(
            shared,
            thread,
            frame_idx,
            &compiled,
            &class_name,
            &method_name,
            &method_descriptor,
            exc,
            entry_stash_floor,
        ) {
            OsrExceptionExit::EnteredHandler => {
                *committed_out = true;
                return None;
            }
            OsrExceptionExit::Propagate(exc) => {
                *throw_out = Some(exc);
                return None;
            }
        }
    }
    if crate::jit::helpers::take_jit_pending_npe() {
        // Taken BEFORE the construction below re-captures a stack the compiled
        // frames have already left — see `attach_snapshotted_trap_frames`.
        let npe_snapshot = crate::jit::helpers::take_jit_pending_trap_frames();
        // The JEP 358 action code the null-check stub recorded. Consuming it
        // here is what `take_jit_pending_npe_action`'s own contract says the
        // drain does ("the interpreter drain calls this right after
        // `take_jit_pending_npe` to build the action-only message") and no
        // drain did until 2026-09-06 — `jit_npe_message_gated` had no
        // production caller at all.
        //
        // It was LATENT when it was wired, and measured to be: on 2026-09-06
        // the only setter of an action code was `jit_npe_with_action`, which
        // pairs it with `set_jit_deopt_pending`, so a recorded action always
        // bailed to the interpreter and every NPE that reached this drain
        // carried `NONE`. It was wired anyway because the alternative is a
        // silent drop — which is exactly how this machinery came to be fully
        // built, unit-tested and connected to nothing.
        //
        // It stopped being latent on 2026-09-11. The helpers that detect a null
        // RECEIVER — `jit_getfield`, `jit_invoke_dispatch`,
        // `jit_invoke_virtual_mic` and the direct-bound intrinsics — record a
        // trap-site key or an `INVOKE_RECEIVER` code and DO reach here, because
        // they do not deopt; that is the defect
        // `the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md`
        // is about. The code is now read by `jit_npe_message` as the
        // corroboration for a bci, and as the fallback message when it declines.
        let npe_action = crate::jit::helpers::take_jit_pending_npe_action();
        // Round-9/10 HIGH fix: route the NPE through the OSR'd method's own
        // exception table rather than losing it. The OSR target IS the method
        // whose code raised the NPE, so this frame's table is the one to
        // search.
        //
        // It used to be searched at `entry_pc` — "our best-known throw site —
        // the back-edge OSR entry, which dominates the failing helper call".
        // That was inert while RBC.6b refused every method with a table, and
        // is not a defensible throw site now that they compile: the back-edge
        // pc has nothing to do with where the null was dereferenced, and the
        // live frame's locals are the stale pre-OSR ones. A protected
        // `putfield` on a null receiver publishes a precise frame at the
        // trapping bci (`emit_precise_null_check_field_store`, which records a
        // `PendingException` deopt point and raises the NPE from the stub), so
        // the same router the pending-exception drain uses has the real answer.
        //
        // r9w9 exc9: through the `OmitStackTraceInFastThrow` door (opt-in; off,
        // this is exactly the `throw_runtime_error` call it wraps).
        match crate::runtime::exceptions::throw_implicit_runtime_error(
            shared,
            thread,
            crate::runtime::exceptions::FastThrowKind::NullPointer,
            crate::runtime::exceptions::fast_throw_site_of(npe_snapshot.as_deref()).as_ref(),
            || RuntimeError::NullPointerException {
                // See `super::jit_npe_message` — the trapping method's own
                // bytecode first, the action-only string as the fallback.
                message: super::jit_npe_message::jit_npe_message(
                    shared,
                    npe_snapshot.as_deref(),
                    npe_action,
                ),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                attach_osr_drain_trap_frames(
                    shared,
                    thread,
                    exc,
                    npe_snapshot,
                    (frame_idx + 1, Arc::as_ptr(&compiled) as usize),
                );
                match route_osr_exception_out_of_artifact(
                    shared,
                    thread,
                    frame_idx,
                    &compiled,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    exc,
                    entry_stash_floor,
                ) {
                    OsrExceptionExit::EnteredHandler => {
                        *committed_out = true;
                        return None;
                    }
                    OsrExceptionExit::Propagate(exc) => {
                        *throw_out = Some(exc);
                        return None;
                    }
                }
            }
            _ => {
                // Couldn't construct a Java NPE object (e.g. rt.jar not
                // loaded) — re-stash the raw flag as before so the next
                // JIT drain still surfaces it. The frame snapshot is NOT
                // re-stashed: it was taken for this raise, and by the time a
                // later drain surfaced the flag it would describe frames that
                // are long gone. A short trace beats a confidently wrong one.
                //
                // `set_jit_pending_npe_flag_only` is what makes that sentence
                // true of the CODE: `stash_jit_pending_npe` takes a fresh
                // snapshot of its own, so the frames were not dropped here at
                // all — they were silently replaced by a shallower set.
                //
                // gen r5w2/oomjit6 (item 3 of
                // `docs/feature-designs/generative-gc-architecture.md`):
                // this arm runs AFTER the body did, and `None` resumes the
                // frame at `entry_pc` on its pre-entry locals -- a replay of
                // every committed iteration, and with the entry locals parked
                // a resume on nulls. Answer it like every other post-entry
                // arm; the raw flag is re-stashed only when the replay was
                // judged safe (nothing thrown in its place).
                let refused = osr_refuse_replay(
                    shared,
                    thread,
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                    &code,
                    entry_pc,
                    "the body's NullPointerException could not be constructed",
                    throw_out,
                    parked_entry_locals,
                );
                if throw_out.is_none() {
                    crate::jit::helpers::set_jit_pending_npe_flag_only();
                }
                // `osr_refuse_replay` answers `None` on both of its arms, so
                // the `return None` after this `match` is its answer.
                debug_assert!(refused.is_none());
            }
        }
        return None;
    }
    if let Some((index, length)) = crate::jit::helpers::take_jit_pending_aioobe() {
        // Round-11 fix: mirror the NPE block above for AIOOBE on the OSR bail
        // path — routed through the same `route_osr_exception_out_of_artifact`,
        // for the same reason that block no longer searches at `entry_pc`.
        //
        // A protected array access is admitted by
        // `first_unsupported_precise_frame_site` (round 9) because it
        // publishes a precise exceptional frame (reason 10/11); the router
        // consumes that frame and routes to the covering handler. Without one
        // the site is outside every protected range, and the router's "no
        // precise frame => propagate" deduction is the answer.
        //
        // The compiled frames `jit_throw_aioobe` snapshotted go onto the
        // throwable, as the NPE block above does (round 13 wave 10, lane
        // trace2): the body has unwound, so the construction's own walk names
        // none of them. See `take_osr_drain_trap_frames`.
        let trap_snapshot = take_osr_drain_trap_frames();
        let created = crate::runtime::exceptions::create_implicit_exception_object(
            shared,
            thread,
            crate::runtime::exceptions::FastThrowKind::ArrayIndexOutOfBounds,
            crate::runtime::exceptions::fast_throw_site_of(trap_snapshot.as_deref()).as_ref(),
            || cratonvm_types::error::out_of_bounds_message::check_index(index, length),
        );
        if let Ok(exc) = &created {
            attach_osr_drain_trap_frames(
                shared,
                thread,
                *exc,
                trap_snapshot,
                (frame_idx + 1, Arc::as_ptr(&compiled) as usize),
            );
        }
        match created.or_else(|e| osr_drain_oom_singleton(shared, e)) {
            Ok(exc_obj) => {
                match route_osr_exception_out_of_artifact(
                    shared,
                    thread,
                    frame_idx,
                    &compiled,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    exc_obj,
                    entry_stash_floor,
                ) {
                    OsrExceptionExit::EnteredHandler => {
                        *committed_out = true;
                        return None;
                    }
                    OsrExceptionExit::Propagate(exc) => {
                        *throw_out = Some(exc);
                        return None;
                    }
                }
            }
            Err(_) => {
                // Couldn't construct the Java object (e.g. rt.jar not loaded) —
                // re-stash the raw flag as before so the next JIT drain still
                // surfaces it. Heap exhaustion no longer lands here (round 11
                // wave 15, lane tier): `create_implicit_exception_object` has
                // no OOM fallback of its own, unlike `throw_runtime_error`, so
                // a full heap made this a post-entry "not entered" that
                // replayed every committed iteration. It now throws the
                // singleton `OutOfMemoryError` at the site, as the NPE and
                // `ArithmeticException` drains already did.
                //
                // gen r5w2/oomjit6: a post-entry arm, answered like the NPE
                // drain's above (see there).
                let refused = osr_refuse_replay(
                    shared,
                    thread,
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                    &code,
                    entry_pc,
                    "the body's ArrayIndexOutOfBoundsException could not be constructed",
                    throw_out,
                    parked_entry_locals,
                );
                if throw_out.is_none() {
                    crate::jit::helpers::stash_jit_pending_aioobe(index, length);
                }
                // Always `None`; the `return None` below is its answer.
                debug_assert!(refused.is_none());
            }
        }
        return None;
    }
    // Divide-by-zero direct-throw drain on the OSR bail path (sibling of the
    // NPE/AIOOBE OSR blocks above). Routed through the same
    // `route_osr_exception_out_of_artifact` as the other three, and for the
    // same reason the NPE block above no longer searches at `entry_pc`.
    //
    // A protected `idiv`/`irem`/`ldiv`/`lrem` is admitted by
    // `first_unsupported_precise_frame_site` since round 11 wave 8: its zero
    // guard publishes a precise exceptional frame (deopt-stub reason 12), which
    // the router consumes. A drain here without one comes from an unprotected
    // site, so the router's "no precise frame => propagate" deduction holds.
    if crate::jit::helpers::take_jit_pending_arithmetic() {
        // Round 13 wave 10 (lane trace2,
        // `r13w10-orch-intermittent-empty-stack-trace-at-startup-under-g1`):
        // the compiled frames `jit_throw_arithmetic` snapshotted go onto the
        // throwable, as the NPE block above does. Without them a `/ by zero`
        // that reaches this drain as a bare flag -- raised in a compiled
        // callee of the OSR'd loop that no dispatch helper built a throwable
        // for on the way out (a directly bound callee: whether it is depends
        // on which of the two compiles finished first) -- came out with none
        // of the compiled frames between the trap and this loop, on every
        // throw from then on (`R11RtFibTraceLines`: `recFrames 0`).
        let trap_snapshot = take_osr_drain_trap_frames();
        match crate::runtime::exceptions::throw_implicit_runtime_error(
            shared,
            thread,
            crate::runtime::exceptions::FastThrowKind::Arithmetic,
            crate::runtime::exceptions::fast_throw_site_of(trap_snapshot.as_deref()).as_ref(),
            || RuntimeError::ArithmeticException {
                message: "/ by zero".to_string(),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                attach_osr_drain_trap_frames(
                    shared,
                    thread,
                    exc,
                    trap_snapshot,
                    (frame_idx + 1, Arc::as_ptr(&compiled) as usize),
                );
                match route_osr_exception_out_of_artifact(
                    shared,
                    thread,
                    frame_idx,
                    &compiled,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    exc,
                    entry_stash_floor,
                ) {
                    OsrExceptionExit::EnteredHandler => {
                        *committed_out = true;
                        return None;
                    }
                    OsrExceptionExit::Propagate(exc) => {
                        *throw_out = Some(exc);
                        return None;
                    }
                }
            }
            _ => {
                // Couldn't construct the Java object — re-stash the raw flag,
                // unless the post-entry refusal (gen r5w2/oomjit6; see the NPE
                // drain above) threw in its place.
                let refused = osr_refuse_replay(
                    shared,
                    thread,
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                    &code,
                    entry_pc,
                    "the body's ArithmeticException could not be constructed",
                    throw_out,
                    parked_entry_locals,
                );
                if throw_out.is_none() {
                    crate::jit::helpers::stash_jit_pending_arithmetic();
                }
                // Always `None`; the `return None` below is its answer.
                debug_assert!(refused.is_none());
            }
        }
        return None;
    }
    let result_i64 = match result_i64 {
        Ok(Some(v)) => v,
        Ok(None) => {
            // The machine-level entry declined: nothing ran, and the frame
            // resumes at `entry_pc`. Parked locals go back first (gen
            // r5w1/oom5); if a collection ran in between they cannot, and
            // resuming on nulls would be a wrong answer, so refuse loudly.
            if let Some(parked) = parked_locals {
                if !parked.restore_after_declined_entry(shared, &mut thread.frames[frame_idx]) {
                    return osr_refuse_replay(
                        shared,
                        thread,
                        class_id,
                        class_name,
                        method_name,
                        method_descriptor,
                        &code,
                        entry_pc,
                        "the entry declined after a collection took its parked locals",
                        throw_out,
                        true,
                    );
                }
            }
            return None;
        }
        // A panic that unwound out of the OSR'd body. It used to be dropped
        // here without a word, which is "rejected, nothing ran": the frame
        // resumed at `entry_pc` with its pre-entry locals and re-executed every
        // iteration the body had committed before panicking, the same
        // silent-replay shape the exception drains above were fixed for — and
        // the cached body was re-entered on the next hot back edge. Every other
        // compiled entry converts the payload (`jit_panic_to_exception`); do the
        // same, and deny OSR for this method so the body is not entered again.
        Err(payload) => {
            shared.jit.tiered_manager.mark_osr_denied(
                crate::jit::tiered::MethodKey::with_class_id(
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                ),
            );
            return match jit_panic_to_exception(shared, thread, payload) {
                // A Java-mappable panic propagates from THIS frame, like any
                // other exception the body could not route to a handler.
                MethodCallFailed::ExceptionThrown(exc) => {
                    *throw_out = Some(exc);
                    None
                }
                // A VM defect, reached AFTER the body ran: resuming at
                // `entry_pc` replays whatever it committed. So it takes the
                // other post-entry arms' answer (round 11 wave 4): a silent
                // replay only for a body with no side effect, otherwise a
                // loud `InternalError` from this frame.
                other => {
                    tracing::error!(
                        "OSR body {}.{}{} panicked at entry_pc={entry_pc}: {other:?}; \
                         OSR denied for this method",
                        class_name,
                        method_name,
                        method_descriptor,
                    );
                    osr_refuse_replay(
                        shared,
                        thread,
                        class_id,
                        class_name,
                        method_name,
                        method_descriptor,
                        &code,
                        entry_pc,
                        "the OSR body panicked",
                        throw_out,
                        parked_entry_locals,
                    )
                }
            };
        }
    };

    // deopt-osr Step 8: OSR-exit. A frame-deopt taken inside the OSR'd code (the
    // deopt-osr loop-boundary trigger, or any future guard) stashes a reconstructed
    // frame in LAST_DEOPT and returns the i64::MIN sentinel. OSR is *same-frame*
    // replacement, so unlike the `execute_jit_call` sink we must NOT push a new
    // frame or treat the sentinel as the return value (i64::MIN as i32 == 0 → the
    // corrupt result this guards against). Two handlings, gated:
    //
    //   * TRUE OSR-exit (P4, `CRATONVM_OSR_EXIT_TRANSFER`): transfer the
    //     JIT-advanced loop state into THIS live frame and resume the loop body
    //     there (`return None` ⇒ the interpreter continues the mutated frame). This
    //     is required for correctness once the OSR'd code commits per-iteration side
    //     effects — re-running them in the interpreter (the reject below) would
    //     double-execute them.
    //   * Safe reject (gate off / out-of-scope / unmappable): clear the stash and
    //     `return None` so the interpreter continues executing THIS frame from where
    //     it was — correct only when the bail precedes any committed loop iteration
    //     (the unconditional-at-header trigger). The validated Step-8 default.
    if result_i64 == i64::MIN {
        // The entered frame is leaving compiled code without a value. This is
        // the event the doc leads with: an OSR bail that resumes at the wrong
        // interpreter state re-runs loop iterations, which is a wrong-answer
        // bug that no termination test can see. Counting it does not fix that
        // — it makes "entered and immediately left, every time" visible in a
        // default run, which is the shape of the livelock.
        cratonvm_jit::metrics::record_osr_event("osr_exited");
        // No exception is pending here (every pending-exception arm above
        // returned), so a reason-9 frame of THIS method is stale — the same
        // drop as `run_jit_body_raw`'s sentinel arm.
        //
        // Interpreter round i1 wave 25, lane L2: and, as there, one this body
        // published above `exc_floor` is the last description of the locks its
        // compiled code still holds. The live frame outlives the body here, so
        // only the holds its `held_monitors` record does not already name are
        // released — the ones the body took after the entry; a lock the
        // interpreter took before it (the frame keeps its pc inside that block
        // on every arm below that continues it) stays with the frame. Every
        // arm below that such a body can reach continues this frame without a
        // transfer: a transfer needs this body's own `LAST_DEOPT` frame, which
        // a body that left through a reason-9 pad cannot also have stashed,
        // and a pad exit is never a genuine `Long.MIN_VALUE` return.
        let owns =
            |point: usize| super::deopt_resume::deopt_stash_is_from_artifact(&compiled, point);
        super::deopt_resume::release_locks_of_own_pad_exits(
            shared,
            thread,
            &owns,
            &class_name_arc,
            &method_name_arc,
            &descriptor_arc,
            exc_floor,
            Some((frame_idx, &osr_entry_holds)),
        );
        // JIT round 17 wave 4, lane stashdrop (sites 3 and 4 of
        // `r17-syncdoor3-exceptional-frames-discarded-beneath-a-take-keep-their-holds-FIXED-20261005.md`):
        // under the exceptional hand-over's switches the frames dropped here
        // (and the ones the take discards) have their holds released, except
        // a frame THIS body published, whose holds the live frame's record may
        // still account for (the claim above took every such frame it could
        // release correctly). Off, the old unreleasing drops.
        let keep_own = |_: &cratonvm_jit::deopt::ReconstructedFrame, point: usize| owns(point);
        super::deopt_resume::CompiledLocksOfAStash::drop_own_exceptional_frame_releasing(
            shared,
            thread,
            &class_name_arc,
            &method_name_arc,
            &descriptor_arc,
            &keep_own,
            "OSR sentinel arm",
        );
        // gen r5w1/oom5 (`CRATONVM_JIT_OSR_DROP_ORPHANS`, default on): the callee
        // frames `drop_own_exceptional_frame` re-stashed were published inside
        // this activation, whose compiled frames have all unwound; see
        // `route_osr_exception_out_of_artifact`.
        if let Some(floor) = entry_stash_floor {
            use super::deopt_resume::CompiledLocksOfAStash as StashLocks;
            let _ = StashLocks::truncate_exceptional_stash_releasing(
                shared,
                thread,
                floor,
                &keep_own,
                "OSR sentinel arm",
            );
        }
        if let Some((rframe, cause, point_addr)) = take_door_stash_since_entry(
            entry_stash_mark,
            JitBodyName {
                class_name: &class_name_arc,
                method_name: &method_name_arc,
                descriptor: &descriptor_arc,
            },
        ) {
            dbg_deopt_sink("osr-exit", &rframe, "");
            // Interpreter round i1 wave 23, lane L2: the locks the stashed
            // frame's compiled code took. A transfer hands them to the live
            // frame; both refusals below abandon the frame, so they release
            // them (`CompiledLocksOfAStash`). Wave 27 (lane L2): with the
            // elided levels, which the planless transfer re-takes and the
            // record must then name.
            let compiled_locks =
                super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks_and_elided_levels(
                    thread, &rframe,
                );
            // The lane's step 4, and the only place it can be answered: the
            // artifact records a set of loop-boundary exit-map bcis
            // (`osr_exit_points`) and until now nothing compared it with where
            // exits are actually taken. Counted before the identity gate,
            // because the classification is about THIS artifact's own view of
            // the bci the frame names; whether the frame is ours to transfer is
            // the separate question the gate below answers.
            //
            // Ungated, for the same reason `osr_entered` / `osr_exited` are: an
            // exit at a bci this artifact records nothing for otherwise leaves
            // no trace at all.
            // JIT round 19 wave 11 (lane tail11, DEOPT10-3,
            // `CRATONVM_OSR_EXIT_SITE_FRAME_KEYED`, opt-in): keyed by the
            // frame's kind too; off, exactly `classify_osr_exit_site`.
            let exit_site = compiled
                .classify_osr_exit_site_of_frame(rframe.bci, !rframe.caller_frames.is_empty());
            cratonvm_jit::metrics::record_osr_event(exit_site.metric());
            if matches!(exit_site, cratonvm_jit::osr_exit::OsrExitSite::Unrecorded)
                && dbg_deopt_enabled()
            {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit at bci={} which {}.{}{} records neither an \
                     exit map nor a deopt point for ({} exit points, {} deopt points)",
                    rframe.bci,
                    &*class_name_arc,
                    &*method_name_arc,
                    &*descriptor_arc,
                    compiled.osr_exit_points.len(),
                    compiled.deopt_points.len(),
                );
            }
            // Identity gate (jit-invokedynamic-groovy-regression): the stash
            // could belong to a NESTED compiled callee of the OSR'd code whose
            // sentinel bubbled up here; transferring THAT frame into this live
            // frame would resume this method's bytecode at the callee's bci
            // with the callee's locals/stack. Verify the frame's baked
            // `method_key` names THIS method before transferring; on a
            // mismatch, despeculate the frame's real owner and safe-reject.
            // Wave 10 (L4): and the point must be one of THIS body's, so a
            // same-named frame of another body (another loader's copy, a
            // nested activation) is not transferred either.
            // An inlined chain is named by its OUTERMOST scope (wave 26, lane
            // L2; `osr_exit_stash_is_this_bodys`).
            let identity_ok = osr_exit_stash_is_this_bodys(
                &rframe,
                &compiled,
                point_addr,
                &class_name,
                &method_name,
                &method_descriptor,
            );
            if !identity_ok {
                despeculate_stashed_frame_method_with_cause(shared, &rframe, cause, point_addr);
                if dbg_deopt_enabled() {
                    eprintln!(
                        "[cratonvm-deopt] OSR-exit stash identity mismatch (frame={} bci={}) \
                         for {}.{}{} — safe reject",
                        rframe.method_key,
                        rframe.bci,
                        &*class_name_arc,
                        &*method_name_arc,
                        &*descriptor_arc
                    );
                }
                // Returning `None` here replays whatever the body committed
                // before the callee's sentinel reached here. Round 11 wave 2
                // kept it that way on purpose: a callee trap is an ordinary
                // event, so raising here would have turned a routine
                // speculation failure into a crash wherever the call site did
                // not service its callee's sentinel. Since round 11 wave 12 the
                // call site's service re-runs a callee whose trap it declines
                // (`helpers::rerun_declined_callee_from_entry`), so this arm is
                // expected to be unreachable; the count below is the evidence.
                // See r11-tier-osr-post-entry-refusals-replay-loop-iterations.
                note_osr_unresumable_exit(
                    shared,
                    &OSR_EXIT_FOREIGN_STASH_REPLAYS,
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                );
                // The callee's frame is dropped and the callee runs again
                // from entry: what its compiled code locked is released.
                compiled_locks.release_for_a_rerun(shared, thread);
                // Round 11 wave 19 (tier proposal W17-5): the census read zero,
                // so a producer that reaches this arm now is a new defect, and
                // it answers like every other post-entry arm with no state to
                // transfer instead of replaying the committed iterations.
                // Round 12 wave 2 (lane osrfix): the message names the stash.
                // The defect is always in the helper that called the frame's
                // method and did not service its sentinel, and the frame's key
                // is the only lead to it once the caller chain is gone.
                if osr_loud_unresumable_exits() {
                    return osr_refuse_replay(
                        shared,
                        thread,
                        class_id,
                        class_name,
                        method_name,
                        method_descriptor,
                        &code,
                        entry_pc,
                        &format!(
                            "a callee's stashed frame reached the OSR sink: {} bci {}",
                            rframe.method_key, rframe.bci
                        ),
                        throw_out,
                        parked_entry_locals,
                    );
                }
                // gen r5w1/oom5: a replay needs the pre-entry locals, which a
                // parked entry no longer has.
                if parked_entry_locals {
                    return osr_refuse_replay(
                        shared,
                        thread,
                        class_id,
                        class_name,
                        method_name,
                        method_descriptor,
                        &code,
                        entry_pc,
                        "a callee's stashed frame reached the OSR sink (entry locals parked)",
                        throw_out,
                        true,
                    );
                }
                return None;
            }
            // FIX (silent data corruption, HHH-15895 InPredicateTest / AccumRepro3
            // residual): `compiled.can_osr_exit` is already a precise gate — it's
            // false unless this compile genuinely recorded an OSR-exit snapshot
            // (either the experimental `deopt_real_enabled()`-gated loop-header
            // guards, or the now-unconditional invokedynamic uncommon-trap
            // snapshot — see the fix notes in `jit/src/x64.rs`). The separate
            // `osr_exit_transfer_enabled()` opt-in gate was redundant on top of
            // that and, left in place, would silently keep the invokedynamic
            // trap on the corruption-prone "safe reject" path in default builds
            // (`CRATONVM_OSR_EXIT_TRANSFER` unset). Dropped in favor of
            // `can_osr_exit` alone.
            // `plan` is the validated entry (see the admission block above). The
            // transfer spends it on `OsrEntryPlan::resume_after_exit`, which names
            // the EXACT bci the OSR'd body stopped at — a recorded deopt point of
            // this artifact whose `ResumeSemantics` is `REEXECUTE` — instead of
            // trusting `rframe.bci` verbatim. It is the only sanctioned resume
            // point once compiled code has run.
            // `plan` is `None` for an optimizing-tier entry. Such an entry has
            // guard exits only when tier proposal 27 phase 3 admitted it
            // (`osr_optimizing_guard_exits_admissible`), and those are resumed
            // by the door's planless transfer, whose authority is the guard's
            // own point (`osr_exit::guard_exit_resume_bci`). Anything it does
            // not honour falls to the refusal below, as before.
            // Round 15 wave 4 (lane chain5): a forced chain exit of
            // `CRATONVM_DEOPT_EAGER_CHAINS` left uncharged, set by the planless
            // transfer (`deopt_resume::leave_forced_trip_uncharged`).
            let mut uncharged_forced = false;
            // An exit the optimizing memo's value-equivalent bound counted.
            let mut counted_value_equivalent = false;
            let transferred = match plan.as_ref() {
                Some(plan) => {
                    compiled.can_osr_exit
                        && transfer_osr_exit_into_live_frame(
                            shared, thread, frame_idx, &rframe, &compiled, plan,
                        )
                        .is_some()
                }
                None => osr_optimizing_transfer_guard_exit(
                    shared,
                    thread,
                    frame_idx,
                    &rframe,
                    &compiled,
                    point_addr,
                    &mut uncharged_forced,
                    throw_out,
                    &mut counted_value_equivalent,
                ),
            };
            // The for-each residual round: re-allocating the exit frame's
            // scalar-replaced objects found the heap exhausted, after the body
            // ran. The activation is abandoned with the error (see the
            // function); it holds no lock its compiled code took.
            if !transferred && throw_out.is_some() {
                compiled_locks.unpin_keeping_the_locks(thread);
                return None;
            }
            if transferred {
                // Interpreter round i1 wave 25, lane L2: the live frame, parked
                // at the exit, holds exactly what the exit frame names. An
                // optimizing body describes every lock it holds (`relock ==
                // false`, the pre-entry ones included), so its frame's record
                // is REPLACED with them (for a planless guard exit, which is
                // admitted only monitor-free, that is an empty record). A
                // single-pass body describes none, so its frame keeps what the
                // transfers always left: the record emptied below for a
                // committed exit, kept otherwise.
                //
                // Wave 26 (lane L2): a single-pass exit frame that names a lock
                // its code took describes the whole monitor stack at its bci
                // (`x64/deopt_stubs.rs` `taken_monitor_slots_per_pc` answers a
                // pc only when it can name every open lock), so it is as
                // authoritative as the optimizing tier's and REPLACES the
                // record too. One that names none may still be one the
                // analysis could not describe, and keeps the old rule. A chain
                // does not either: its trapping scope's locks belong to the
                // frames the chain transfer pushed, not to the live one.
                let single_pass_names_its_locks = plan.is_some()
                    && rframe.caller_frames.is_empty()
                    && rframe.monitors.iter().any(|m| !m.relock);
                // Wave 27 (lane L2): the planless transfer re-took the exit
                // frame's elided levels (`transfer_osr_guard_exit_into_live_frame`),
                // so the record names them too; a record short of them let a
                // later sink's `release_beyond_live_record` release the
                // interpreter's own hold. A single-pass frame has none.
                if plan.is_none() {
                    compiled_locks
                        .replace_live_record_with_retaken_levels_and_unpin(thread, frame_idx);
                } else if single_pass_names_its_locks {
                    compiled_locks.replace_live_record_and_unpin(thread, frame_idx);
                } else {
                    compiled_locks.unpin_keeping_the_locks(thread);
                }
                if dbg_deopt_enabled() {
                    eprintln!(
                        "[cratonvm-deopt] OSR-exit TRANSFER {}.{}{} entry_pc={} resume_bci={}",
                        &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc, rframe.bci
                    );
                }
                // Round 9 wave 11 (`osr11`): a failed speculation is charged
                // here too, so a guard that fails on every entry is withdrawn
                // instead of being re-entered forever. See the function.
                //
                // The body RAN and this frame now stands at the exit bci, so a
                // charged exit is a COMMITTED entry, not a rejection (interpreter
                // round i1 wave 4, L7): charging it against the per-pc budget
                // too shadowed a long-running loop for the rest of its
                // activation after five occasional guard failures. What bounds
                // a guard that fails on EVERY entry is the charge itself (the
                // per-bci de-spec after four, then `cached_osr_body_is_despec_
                // stale` recompiles without the guard). An exit the charge
                // declines (an `OsrExit` map, an ambiguous reason, the test
                // triggers) has no such bound, so it keeps the budget.
                //
                // Why not `true` for every transfer (round 11's first answer,
                // re-audited at the wave-7 merge): the budget is the ONLY
                // brake on an uncharged exit. A committed entry resets neither
                // `backward_count` nor the attempt count, so `should_try_osr`
                // is due again at the very next back edge; an exit that
                // recurs on every entry (a `NullCheck` / `ClassCheck` guard, an
                // `OsrExit` map) would then pay a whole OSR entry and transfer
                // per back edge for the rest of the activation. `charged`
                // keeps round 11's intent for the exits a de-spec does bound.
                //
                // Round 15 wave 4 (lane chain5): a forced chain exit is a
                // passing guard: not charged, and committed (below) rather
                // than a rejection, so the soak keeps entering the body; the
                // forced block's own schedule (one arrival in 64) bounds it.
                // JIT round 19 wave 4 (lane splice4; the orchestrator's
                // measurement of SPLICE2-2: `R19Splice3Probe` throw-never
                // 3 -> 322 ns, throw-eighth 349 -> 643 ns under
                // `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW=1`): a marked
                // expected-throw exit of an inlined chain is a COMMITTED entry
                // (the body ran, the frame stands at the callee's bci and the
                // interpreter throws there), not a rejection. Before, it was
                // neither charged (`charge_osr_chain_guard_exit` declines a
                // marked trip) nor committed, so every expected throw spent
                // the frame's OSR budget: five throws retired the loop for the
                // activation, the refused activations then retired it for the
                // install epoch (`note_osr_refused_activation`), and a loop
                // that no longer threw at all ran interpreted. Under the
                // wave-3 switch only (the mark exists only there).
                let expected_throw_exit = !uncharged_forced
                    && !rframe.caller_frames.is_empty()
                    && super::deopt_resume::splice_expected_throw_trip(&rframe);
                if expected_throw_exit && dbg_deopt_enabled() {
                    eprintln!(
                        "[cratonvm-deopt] OSR-exit at an expected throw committed: {}.{}{} \
                         entry_pc={} (innermost {} bci={})",
                        &*class_name_arc,
                        &*method_name_arc,
                        &*descriptor_arc,
                        entry_pc,
                        rframe.method_key,
                        rframe.bci
                    );
                }
                let charged = !uncharged_forced
                    && !expected_throw_exit
                    && charge_osr_guard_exit(
                        shared,
                        class_id,
                        class_name,
                        method_name,
                        method_descriptor,
                        &compiled,
                        &rframe,
                        cause,
                    );
                // An `OsrExit` map the back-edge poll left through because the
                // interpreter is now required (wave 12, L4) recurs only when
                // that mode is switched on again, and the OSR gate declines
                // every offer while it holds: committed, not a rejection.
                // Interpreter round i1 wave 23, lane L6: so is the exit of a
                // body a class redefinition withdrew
                // (`jvmti_events::withdrawn_body_may_leave`); its door's memo
                // or the cache serves a fresh body at the next offer.
                // JIT round 23 wave 1, lane L1 (`CRATONVM_JIT_OSR_PROFIT_EXIT`):
                // so is the exit of a single-pass body armed for a profit exit
                // (`osr_profit_exit_left`): the frame now stands at the poll's
                // header (or its back-edge branch), and its next back edge
                // enters the optimizing body this door prefers.
                let jvmti_left = !charged
                    && osr_exit_left_for_the_interpreter(shared, thread, frame_idx, cause);
                let profit_exit =
                    !charged && !jvmti_left && osr_profit_exit_left(shared, &compiled, cause);
                let left_for_interpreter = !charged
                    && (jvmti_left
                        || profit_exit
                        || (compiled.is_withdrawn_by_redefinition()
                            && matches!(
                                cause,
                                Some(c) if c.reason == cratonvm_jit::deopt::DeoptReason::OsrExit
                            )));
                // An exit the memo's value-equivalent bound counted is braked
                // there (`osr_optimizing_transfer_guard_exit`'s `counted_out`).
                *committed_out = charged
                    || uncharged_forced
                    || left_for_interpreter
                    || expected_throw_exit
                    || counted_value_equivalent;
                // The single-pass half of the record rule above: what
                // `try_osr_with_backoff` did for every committed entry until
                // wave 25, which now leaves the record to the committing path.
                if plan.is_some() && *committed_out && !single_pass_names_its_locks {
                    // Wave 117 (lane L6, page
                    // `i116-L9-strict-monitorexit-throws-on-an-understated-record`):
                    // emptied because the body names no lock, so marked
                    // understated where the frame can hold one.
                    crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                        &mut thread.frames[frame_idx],
                        crate::runtime::interpreter::held_monitors::RecordProducer::SinglePassOsrExit,
                    );
                }
                // Interpreter round i1 wave 45, lane L2: an OSR body of its own
                // class's old bytecode, which a renumbering redefinition sent
                // out (`CompiledMethod::leaves_as_renumbered_obsolete`), left
                // into the obsolete activation's own frame. Move that frame
                // onto its translated body now, before its next bytecode reads
                // a constant: an exit through a forced post-call or
                // constant-pool site passed no `safepoint_check`, and a
                // redefinition that raised no fence armed no loop's poll word.
                // A busy class-manager lock defers it to the loop's next top
                // (the deferral bumps this thread's poll word). One load and a
                // compare when the frame was moved already.
                if compiled.leaves_as_renumbered_obsolete() {
                    crate::runtime::interpreter::obsolete_frames::convert_obsolete_frames_if_redefined(
                        shared, thread,
                    );
                }
                return None;
            }
            // Safe reject: the method is not OSR-exit-capable (`can_osr_exit`
            // false ⇒ this artifact recorded no exit map, so no compiled
            // iteration was committed through one), or the reconstructed frame
            // was out of scope / unmappable.
            //
            // The second case is the one that used to be a correctness bug:
            // "continue interpreting THIS frame from where it was" is correct
            // only when the bail precedes any committed iteration, and a
            // mid-loop transfer failure does not. It is now closed at ADMISSION
            // rather than here — `validate_osr_entry` refuses
            // (`osr-entry-unresumable-exit`, permanent, memoed) any artifact
            // whose deopt points include one that reconstructs an unresumable
            // frame, which is exactly the set this branch could receive:
            // `MaterializationRequired` / `Unsupported` slots
            // (`deopt::frame_state_is_resumable`), held monitors, an inlined
            // caller scope, or non-`REEXECUTE` semantics. An admitted entry is
            // therefore `OsrExitPolicy::ExactTransfer`, under which every exit
            // this body can take transfers cleanly; and `can_osr_exit` implies
            // a non-empty `deopt_points`, so the `PropagateOnly` artifacts never
            // reach the transfer at all.
            //
            // Reaching here after a committed body would mean that invariant
            // broke. Do not add a resume path for it — the fix belongs at
            // admission, where nothing has run yet.
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit bail rejected (continue interpreting) {}.{}{} entry_pc={}",
                    &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc
                );
            }
            // FIX (2026-07-07, jit-invokedynamic-groovy-regression, FOURTH-pass
            // root cause): same missing-despeculation gap as the other two
            // fixed call sites (see the fix note at the first-call tier-up
            // site in `execute()`). A rejected bail here previously just
            // continued interpreting THIS frame with no blacklist, so an
            // `UnreachedCode` (reason 8) trap reached through the OSR-exit
            // path also kept re-triggering on every subsequent call. Recover
            // the reason from the matching deopt point (falling back to
            // `UnreachedCode`) and drive the same de-speculation call the
            // other two fixed sites do.
            // The attribution every other sink shares (round 11 wave 2), at
            // the site in this method's own bytecode (wave 26: an inlined
            // chain's call site, `stash_charge_site`).
            let attribution =
                super::deopt_resume::stash_trap_attribution(&compiled, &code, &rframe, cause);
            let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
                shared,
                class_id,
                &class_name_arc,
                &method_name_arc,
                &descriptor_arc,
                attribution.charge_reason(),
                attribution.charge_bci,
            );
            // Round nio21 wave 7 (lane L2): and the per-bci de-spec every
            // own-stash sink records after its charge
            // (`deopt_resume::despec_own_stash_site_after_charge`; kill switch
            // `CRATONVM_JIT_RERUN_SINK_PER_BCI_DESPEC=0`).
            if super::deopt_resume::rerun_sink_per_bci_despec_enabled() {
                let method_key = format!("{class_name}.{method_name}:{method_descriptor}");
                let _ = super::deopt_resume::despec_own_stash_site_after_charge(
                    shared,
                    &method_key,
                    &rframe,
                    &attribution,
                );
            }
            // NOT released here, unlike the foreign arm above: this is the OSR'd
            // activation's own frame, and a `relock == false` monitor of an
            // OSR-door body may be one the INTERPRETER took before the entry
            // (`OsrEntryPlan`'s entry contract lists it that way). The live
            // frame keeps its pc inside the block and will run the matching
            // `monitorexit`, so releasing here would unbalance it. (Admission
            // makes this arm unreachable for a body that committed anything.)
            //
            // An inlined chain (admitted by the gate since wave 26, lane L2)
            // also names its spliced scopes' locks, which the body took after
            // the entry and no live record names: those are released, and the
            // pre-entry ones the record names stay, as at the sentinel arm.
            if rframe.caller_frames.is_empty() {
                compiled_locks.unpin_keeping_the_locks(thread);
            } else {
                let _ = compiled_locks.release_beyond_live_record(
                    shared,
                    thread,
                    frame_idx,
                    &osr_entry_holds,
                );
            }
            // Round 11 wave 2: the invariant above is argued, not checked, and
            // this arm is reached AFTER the body ran; if it ever fires, say so
            // rather than replay. See `osr_refuse_replay`.
            return osr_refuse_replay(
                shared,
                thread,
                class_id,
                class_name,
                method_name,
                method_descriptor,
                &code,
                entry_pc,
                "the OSR-exit frame could not be transferred",
                throw_out,
                parked_entry_locals,
            );
        }
        // No stashed deopt frame. If the uncommon-trap path signaled a deopt
        // (`jit_uncommon_trap`'s `set_jit_deopt_pending`, e.g. `UnreachedCode`
        // for a live invokedynamic — see the fix note above), this is NOT a
        // genuine method result: safe-reject exactly like the stashed-frame
        // case above, so the interpreter resumes THIS frame from where it
        // was instead of reinterpreting the `i64::MIN` sentinel as a value.
        if deopt_signaled {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit bail rejected (uncommon trap, no frame) {}.{}{} entry_pc={}",
                    &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc
                );
            }
            // Round 11 wave 2 did NOT route this through `osr_refuse_replay`
            // (wave 19 does, below, under a kill switch): a frameless trap is
            // also what a guard that fails on the FIRST
            // iteration, before anything committed, looks like, and for that
            // the replay is exactly right. Without a frame the two cannot be
            // told apart here. See
            // r11-tier-osr-post-entry-refusals-replay-loop-iterations-FIXED-20260926.md.
            //
            // Since round 11 wave 6 the artifact's OWN frameless stubs cannot
            // arrive here: `validate_osr_entry` refuses an artifact with
            // `has_frameless_trap_stub` before it is entered, and an IR-path
            // artifact always stashes a frame. What is left is a sentinel with
            // the deopt flag raised by something the body CALLED, the
            // identity-mismatch population above without a stash. Since round
            // 11 wave 13 both Java callee doors re-run such a frameless callee
            // trap at the call site (`BARE_CALLEE_TRAPS_SERVICED`: the dispatch
            // helper, and `handle_compiled_callee_deopt_sentinel` for the MIC /
            // direct sites), so this arm is expected to be unreachable from a
            // Java call; `CRATONVM_DBG_JITC=1` prints the count at exit.
            note_osr_unresumable_exit(
                shared,
                &OSR_EXIT_FRAMELESS_REPLAYS,
                class_id,
                class_name,
                method_name,
                method_descriptor,
            );
            // Round 11 wave 19 (tier proposal W17-5): loud, like the
            // identity-mismatch arm above. The benign first-iteration shape the
            // wave-2 note kept this arm silent for is refused at admission
            // since wave 6, so what reaches here replays committed work.
            if osr_loud_unresumable_exits() {
                return osr_refuse_replay(
                    shared,
                    thread,
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                    &code,
                    entry_pc,
                    "a deopt sentinel with no frame reached the OSR sink",
                    throw_out,
                    parked_entry_locals,
                );
            }
            // gen r5w1/oom5: see the identity-mismatch arm above.
            if parked_entry_locals {
                return osr_refuse_replay(
                    shared,
                    thread,
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                    &code,
                    entry_pc,
                    "a deopt sentinel with no frame reached the OSR sink (entry locals parked)",
                    throw_out,
                    true,
                );
            }
            return None;
        }
        // Otherwise this is a genuine `Long.MIN_VALUE` method result — fall
        // through to the normal return-value conversion below.
    }

    // Convert i64 result back to Value based on return type
    let ret_type = crate::jit::return_type(&method_descriptor);
    match ret_type {
        b'V' => Some(None),
        b'I' | b'B' | b'C' | b'S' | b'Z' => {
            Some(Some(Value::Int(narrow_int_return(ret_type, result_i64))))
        }
        b'J' => Some(Some(Value::Long(result_i64))),
        b'F' => Some(Some(Value::Float(f32::from_bits(result_i64 as u32)))), // Cast: JIT ABI -- i64 register convention
        b'D' => Some(Some(Value::Double(f64::from_bits(result_i64 as u64)))), // Cast: JIT ABI -- i64 register convention
        b'L' | b'[' => {
            if result_i64 == 0 {
                Some(Some(Value::Object(None)))
            } else {
                // SAFETY: result_i64 is a non-zero JIT/OSR return value encoding a heap pointer to a valid object header.
                Some(Some(Value::Object(Some(unsafe {
                    ObjectRef::from_raw(result_i64 as usize as *mut u8) // Cast: JIT ABI — i64 register convention
                }))))
            }
        }
        _ => Some(None),
    }
}

/// The compiled-frame snapshot a JIT direct-throw helper took for the
/// `/ by zero` or AIOOBE signal `try_osr`'s drain is about to build a
/// throwable for (round 13 wave 10, lane trace2). The NPE drain always took
/// its snapshot; these two left theirs in the signal record, so the throwable
/// was built from a walk the trapping frames had already left.
///
/// `CRATONVM_JIT_OSR_DRAIN_TRAP_FRAMES` -- default ON; `0` leaves the snapshot
/// where the helper put it, the previous behaviour. Not memoised: read once
/// per implicit exception that leaves an OSR body as a bare flag, next to a
/// throwable construction and a stack walk, and a memo would be one more
/// process static.
pub(in crate::runtime::interpreter::jit_bridge) fn take_osr_drain_trap_frames() -> Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>
{
    crate::jit::helpers::take_jit_pending_trap_frames()
}

/// Splice `snapshot` onto `exc`'s stored trace, as
/// `exceptions::attach_snapshotted_trap_frames` does -- except onto the
/// preallocated `OutOfMemoryError` a full heap hands back in the throwable's
/// place (`throw_runtime_error`'s fallback). That object is shared by every
/// later out-of-memory throw, so trap frames spliced into it would name this
/// trap in all of them. A stackless fast-throw object has no stored trace and
/// is left alone by the splice itself.
///
/// `osr_body` is the `(interp_depth, cm_ptr)` pair the OSR entry published
/// for the body that just returned (`OsrContinuationGuard`). Round 15 wave 6
/// (lane trace6, `r15w5-throw1-compatible-osr-getfield-npe-reports-the-back-edge-line`):
/// that guard is withdrawn when the body returns, so the splice's dedupe
/// (`stackwalker::dedupe_compiled_snapshot`) found the registry silent, took
/// the pc-heuristic arm, dropped the snapshot's OSR entry WITHOUT its bci
/// override, and the trace kept the frame's parked back-edge line (`:249`
/// where HotSpot says `:252`). The pairing is still exactly right here --
/// this drain runs for that body, on that frame, which has not moved -- so it
/// is republished for the splice alone (not the construction, which runs
/// Java). `CRATONVM_JIT_OSR_DRAIN_PAIRING=0` restores the heuristic arm.
pub(in crate::runtime::interpreter::jit_bridge) fn attach_osr_drain_trap_frames(
    shared: &SharedVm,
    thread: &JvmThread,
    exc: ObjectRef,
    snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    osr_body: (usize, usize),
) {
    if snapshot.is_none() || *shared.mem.singleton_oom.read() == Some(exc) {
        return;
    }
    let _pairing = true
        .then(|| OsrContinuationGuard::publish(osr_body.0, osr_body.1));
    crate::runtime::exceptions::attach_snapshotted_trap_frames(
        shared,
        &thread.frames,
        exc,
        snapshot,
    );
}

/// Re-offer every HELD deferred-`new` retry whose class has since loaded.
///
/// Holding a retry rather than burning it on an attempt that would bail keeps
/// the method's one chance alive, but a kept chance nobody offers again is the
/// same outcome as a spent one. This is what offers it.
///
/// Cost when nothing has loaded is one acquire load: `class_definition_epoch`
/// is bumped by every class definition, so an unchanged epoch means no `new`
/// site anywhere can have become resolvable since the last sweep. That is the
/// whole rate limit — deliberately not a time or count budget, because those
/// silence a trigger whose rate depends on the workload rather than on whether
/// there is anything to do.
pub(in crate::runtime::interpreter) fn resweep_held_deferred_new_retries(shared: &SharedVm, on_class_definition: bool) {
    if !crate::runtime::env_cache::c2_supersede() {
        return;
    }
    // One relaxed load, and almost always zero. This VM's own memo.
    if shared.jit.bridge.deferred_new_retries.held_count() == 0 {
        return;
    }
    // The epoch gate is for the COMPILE door, which fires constantly and where
    // an unchanged epoch means no `new` site can have become resolvable since
    // the last look. The class-definition caller IS the event, so it never
    // needs the gate — and must not take it, or two callers racing on the swap
    // would let one of them skip the definition that mattered.
    if !on_class_definition {
        // Stored plus one, so the per-VM default of 0 reads as "never swept".
        // This VM's class store's epoch (`StoreEpochs`, interpreter round i1
        // wave 21): only a definition in this VM can make one of its held
        // sites resolvable, so another VM's class loading no longer re-opens
        // the sweep here.
        let stamp = cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
            .class_definition_epoch()
            .wrapping_add(1);
        if shared
            .jit
            .bridge
            .last_deferred_new_sweep_epoch
            .swap(stamp, std::sync::atomic::Ordering::AcqRel)
            == stamp
        {
            return;
        }
    }
    let held = shared.jit.bridge.deferred_new_retries.held_methods();
    if held.is_empty() {
        return;
    }
    // Round 11 wave 17 (lane tier, `r11-orch-callee-compiled-bodies-are-never-promoted`
    // item 2): drained once, before the first retry this sweep grants. The
    // manager's `request_deferred_new_retry` refuses a method "below the
    // c2-upgrade hotness gate", and for a body entered only from compiled code
    // (a callee compiled by the optimizing door's callee compiler, reached by
    // a direct CALL from an OSR body) the manager's count is whatever the last
    // drain reported, which can be nothing at all when nothing is interpreted
    // and nothing compiles. Only on a grant, which happens at most once per
    // method: a sweep runs on every class definition while a memo is held, and
    // a drain walks the whole cache.
    let mut drained = false;
    for (recorded_class_id, class_name, method_name, descriptor) in held {
        let granted =
            take_deferred_new_retry_if_resolvable(shared, &class_name, &method_name, &descriptor);
        if granted {
            if !drained {
                drained = true;
                drain_jit_entry_counters(shared);
            }
            DEFERRED_NEW_REOFFERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] deferred-new RE-OFFERED {class_name}.{method_name}{descriptor} — its class has loaded"
                );
            }
            // The declaring class the recorder compiled (wave 10), so another
            // loader's same-named class is never the one queued. A key without
            // the class's identity would not match the state the invocation
            // hooks built, and the retry would take a second in-flight slot
            // beside it. A memo recorded without one (`ClassId(0)`) resolves
            // the class the way this file's other by-name doors do.
            let class_id = if recorded_class_id.as_u32() != 0 {
                recorded_class_id
            } else {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id(&class_name)
                    .unwrap_or(ClassId::new(0))
            };
            let answer = shared.jit.tiered_manager.request_deferred_new_retry(
                &crate::jit::tiered::MethodKey::with_class_id(
                    class_id,
                    &*class_name,
                    &*method_name,
                    &*descriptor,
                ),
            );
            // JIT round 21 lane indyconst (`CRATONVM_JIT_ONE_SHOT_RETRY_OWED`):
            // the memo is spent; a task already holding the slot may have
            // built before the class loaded.
            owe_one_shot_retry_if_already_queued(
                shared,
                answer,
                class_id.as_u32(),
                &method_name,
                &descriptor,
                "deferred-new",
            );
        }
    }
}

/// Retries handed back out by the sweep. A sweep that re-offers nothing is a
/// sweep that is not running, or one running where no class ever loads after.
pub(in crate::runtime::interpreter::jit_bridge) static DEFERRED_NEW_REOFFERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many held deferred-`new` retries the sweep has re-offered.
pub fn deferred_new_reoffered() -> u64 {
    DEFERRED_NEW_REOFFERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Round 9 wave 8 (`arraylist8`; `NOTES-w5-typecheck5.md` cross-lane request
/// 2): is class `cid` a `final`, non-interface, non-array class that nothing can
/// subclass in this VM, so that a type check against it may answer a definite
/// MISS for any receiver of another class?
///
/// That needs two things, and both are asked:
///
/// * the class's own `ACC_FINAL && !ACC_INTERFACE` (an array class's
///   synthetic flags are not a statement about subclassing, and the inline
///   `instanceof` arm never takes an array target anyway);
/// * that the "cannot extend a final class" rule is actually ENFORCED:
///   `verify_final_class_constraint` runs in link-time Pass 2, which
///   `-Xverify:none` / `skip_verification` switches off, and a subclass of a
///   final class could then load. Under it the answer is `false`.
///
/// A class whose loader is later unloaded takes its `ClassId` with it, and the
/// sites interned under that id with it; a redefinition cannot change class
/// modifiers. The helper's non-hierarchy admissions (lambda proxies and the
/// autobox wrapper at ids `>= 0x8000_0000`, the by-name loader-duplication
/// fallback behind the duplicate-class-name latch) are screened by the emitted
/// code itself, exactly as for the final boot classes.
pub(in crate::runtime::interpreter) fn jit_class_is_final_for_typecheck_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    cid: u32,
) -> bool {
    if shared.config.skip_verification {
        return false;
    }
    // JIT round 21 (lane typecheck; correctness, unconditional): nor a class an
    // `Unmodifiable*` stamp can DISPLAY as (`typecheck::unmod_stamp_display_name`:
    // `ImmutableCollections$ListN` / `$SetN` / `$MapN` are final). The
    // interpreter and the helpers admit such a stamp by its instance's display
    // class (`display_class_satisfies_target`, H18-1), so the inline final-miss
    // arm of either tier answered `0` where they answer `1`. Every display name
    // lives under these two prefixes.
    cm.get_class(ClassId::new(cid)).is_some_and(|c| {
        c.is_final()
            && !c.is_interface()
            && !c.name.starts_with('[')
            && !c.name.starts_with("java/util/ImmutableCollections$")
            && !c.name.starts_with("java/util/Collections$")
    })
}

/// [`jit_class_is_final_for_typecheck_in`] under its own class-manager read
/// lock: the `CompileRequest::class_is_final_resolver` every compile door
/// passes.
pub(in crate::runtime::interpreter) fn jit_class_is_final_for_typecheck(shared: &SharedVm, cid: u32) -> bool {
    let cm = shared.classes.class_manager.read();
    jit_class_is_final_for_typecheck_in(shared, &cm, cid)
}

/// Whether `cid` is currently a non-final, non-interface leaf class with zero
/// loaded subclasses in this VM. Registers `c.name` in `cha_leaf_watched` so
/// loading any subclass invalidates compiled code depending on `cha_leaf/<name>`.
pub(in crate::runtime::interpreter) fn jit_class_is_cha_leaf_for_typecheck(
    shared: &SharedVm,
    cid: u32,
) -> bool {
    if shared.config.skip_verification {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    let target_id = ClassId::new(cid);
    let Some(c) = cm.get_class(target_id) else {
        return false;
    };
    if c.is_final()
        || c.is_interface()
        || c.name.starts_with('[')
        || c.name.starts_with("java/util/ImmutableCollections$")
        || c.name.starts_with("java/util/Collections$")
    {
        return false;
    }
    let store = cm.class_store();
    let end_id = store.next_id().as_u32();
    for raw in 0..end_id {
        if let Some(other) = store.get(ClassId::new(raw)) {
            if other.superclass == Some(target_id) {
                return false;
            }
        }
    }
    let mut watched = shared.classes.frozen.cha_leaf_watched.lock();
    watched.insert(Arc::clone(&c.name));
    shared
        .classes
        .frozen
        .cha_leaf_watched_nonempty
        .store(true, std::sync::atomic::Ordering::Release);
    true
}

/// JIT round 20 wave 11 (lane pIR; books `jit-r21-nio-proposals-w1-L2.md`
/// L2-3, `-w2-L2.md` L2W2-4): the `CompileRequest::class_is_subclass_resolver`
/// every method-entry compile door passes. Would the handler search
/// (`exception_dispatch::find_exception_handler_impl`) match an exception of
/// exactly class `thrown` against a typed row whose catch type resolved to
/// class `catch_id`? It answers with the search's own predicate on a LOADED catch
/// class -- `is_subclass_of` by id, then (only while the
/// `NamespacedClassTypes` alignment row is off) the loader-blind
/// `is_subclass_of_by_name` on the catch class's name -- so a compiled edge
/// and the interpreter can never disagree. Both classes loaded: their
/// superclass chains and interface sets are fixed (redefinition cannot change
/// them), so the answer cannot change for the life of the compiled body.
/// Either class unknown: `None` (undecided; the compiled throw leaves as
/// before).
pub(in crate::runtime::interpreter) fn jit_class_is_subclass_for_catch(
    shared: &SharedVm,
    thrown: u32,
    catch_id: u32,
) -> Option<bool> {
    // JIT r20 wave 11 review (lane rev11): the loader-blind by-name fallback
    // is the handler search's only while the `NamespacedClassTypes` alignment
    // row is OFF (`exception_dispatch::catch_row_verdict`'s `name_rule`); it
    // is on in both modes by default, and then a resolved catch row matches
    // by class alone (HotSpot's answer). Asking by name unconditionally let a
    // compiled edge catch another loader's same-named exception class the
    // interpreter and HotSpot let propagate. Correctness: unconditional.
    let name_rule = !shared
        .classes
        .aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes);
    let cm = shared.classes.class_manager.read();
    cm.get_class(ClassId::new(thrown))?;
    let catch_class = cm.get_class(ClassId::new(catch_id))?;
    let catch_name: &str = &catch_class.name;
    Some(
        cm.is_subclass_of(ClassId::new(thrown), ClassId::new(catch_id))
            || (name_rule && cm.is_subclass_of_by_name(ClassId::new(thrown), catch_name)),
    )
}

/// CRIT-2 — shared body for the JIT `cp_new_resolver` closures: resolve a
/// `new`/`anewarray` CP index in `holder_cid`'s constant pool to a
/// [`cratonvm_jit::JitNewSite`] — `Resolved { class_id, num_fields,
/// needs_post_alloc_init, has_finalizer }` when the target class is already loaded,
/// `Deferred` when it is not (see the COLD-`new` GAP note below).
///
/// The two flags feed the inline-TLAB `new` fast path, which skips the
/// `jit_post_tlab_init` helper call when BOTH are false — i.e. no
/// long/float/double instance field anywhere in the hierarchy (the JIT now
/// explicitly clears the body, so int/byte/char/short/boolean are already the
/// correct all-zero `Value::Int(0)`) and no finalizer to register.
/// `has_nonzero_tag_primitive_init` mirrors the hierarchy walk in
/// `crate::jit::helpers::jit_init_primitive_fields`; `has_finalizer` mirrors
/// the single-class read in `jit_post_tlab_init`. Unresolvable metadata
/// reports `(true, true)` so the helper call stays in place.
///
/// COLD-`new` GAP (2026-07-31): `find_class_by_name_for_class` only sees
/// ALREADY-LOADED classes — it deliberately does not run a user
/// `ClassLoader.loadClass` from inside the compile path. A miss therefore used
/// to return `None`, which bailed the WHOLE compile and, after
/// `MAX_TIER_FAIL_RETRIES`, left the method interpreted forever. That silently
/// disqualified every hot method whose only un-taken branch does
/// `throw new SomeException(...)`: nothing had loaded the exception class yet
/// (json-smart's `JSONParserBase.readMain` — 293,940 interpreted invocations of
/// the workload's hottest method). A miss is now reported as
/// [`cratonvm_jit::JitNewSite::Deferred`] and the site compiles to the
/// CP-indexed helper, which resolves at run time. `None` stays reserved for a
/// site no runtime resolution can rescue: the holder class is gone, or the CP
/// entry at `cp_idx` is not a class reference at all.
pub(in crate::runtime::interpreter) fn resolve_jit_new_site(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder_cid: ClassId,
    cp_idx: u16,
) -> Option<cratonvm_jit::JitNewSite> {
    use cratonvm_jit::JitNewSite;
    let class = cm.get_class(holder_cid)?;
    let class_name = class.constant_pool.get_class_name(cp_idx)?;
    // The entry's recorded resolution answers before the memo, as at run
    // time (`helpers::jit_resolve_cp_class`) and in the interpreter's `new`:
    // the memo follows a loader whose answer changed since. Still refused
    // (deferred) where `holder` may not access it, as `jit_known_class`
    // refuses. Round 15 wave 5, lane classsite2
    // (`r15w5-classsite-class-entry-shared-by-every-instruction-patch-FIXED-20260930.md`,
    // part C).
    let known = match super::constants::recorded_class_entry(shared, holder_cid, cp_idx) {
        // Not bound while its package step is refused (`jit_known_class_at`;
        // interpreter round i4 wave 96, lane L5).
        Some(id) if crate::vm::class_path_package_refused_at_compile(shared, cm, id) => None,
        Some(id) => super::constants::class_constant_access_denial_in(
            shared,
            cm,
            holder_cid,
            class_name,
            Some(id),
        )
        .is_none()
        .then_some(id),
        None => jit_known_class_constant_at(shared, cm, class_name, holder_cid, Some(cp_idx)),
    };
    let Some(target_id) = known else {
        // A `Deferred` site gets no `new_info` row, and the IR builder's 0xbb
        // arm then bails the WHOLE method to single-pass — which also costs it
        // escape analysis, so every allocation in it survives. Name the class
        // that could not be resolved: "the method bailed" is not actionable,
        // "Short2 was not found from VolumeShort2's loader" is.
        if false {
            // Separate the two ways this lookup fails: the holder has no loader
            // id at all, versus the class simply not being visible from that
            // loader. `find_class_by_name` is the context-free probe, so a hit
            // there with a miss above means the LOADER SCOPE is the problem and
            // not the class being unloaded.
            let loader = cm.get_loader_id(holder_cid);
            #[allow(deprecated)]
            let anywhere = cm.find_class_by_name(class_name).is_some();
            eprintln!(
                "[ir] new-site DEFERRED: {class_name} holder={} loader={loader:?} \
                 loaded_anywhere={anywhere} cp_idx={cp_idx}",
                cm.get_class(holder_cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("{holder_cid:?}")),
            );
        }
        return Some(JitNewSite::Deferred {
            holder_class_id: holder_cid.as_u32(),
            cp_idx,
        });
    };
    let Some(target) = cm.get_class(target_id) else {
        return Some(JitNewSite::Resolved {
            class_id: target_id.as_u32(),
            num_fields: 0,
            needs_post_alloc_init: true,
            has_finalizer: true,
        });
    };
    let (num_fields, needs_post_alloc_init, has_finalizer) =
        jit_new_site_flags(cm, target_id);
    Some(JitNewSite::Resolved {
        class_id: target_id.as_u32(),
        num_fields,
        needs_post_alloc_init,
        has_finalizer,
    })
}

/// Would a `new` of `target_id` executed NOW still owe JVMS §5.5
/// initialization anything observable: a `<clinit>` to run somewhere in the
/// chain `new` initializes, or an erroneous class to answer with
/// `NoClassDefFoundError`?
///
/// Interpreter round i1 wave 4, L7
/// (`interpreter-L7-compiled-new-of-an-uninitialized-class-skips-clinit-FIXED`). A `Resolved`
/// `new` row compiles to code with NO class-init check: the single-pass inline
/// TLAB bump, the optimizing tier's `Op::New` bump, and scalar replacement,
/// which removes the allocation and every helper with it. Only the `jit_new_object`
/// slow path initializes, and it runs only when the TLAB is exhausted. So a
/// method compiled while its `new` target was LOADED but not initialized (after
/// an `anewarray` of it, an `ldc` of its `Class`, `Class.forName(n, false, l)`,
/// a verifier load) allocated instances whose `<clinit>` never ran —
/// `L7CompiledNewInitializesClass.java`. Such a site is
/// answered `Deferred` instead (see [`CpResolvers::new_site`]): the CP-indexed
/// helper (`jit_new_object_cp`) resolves, initializes and allocates, the way
/// the interpreter's `new` does.
///
/// A class that has no `<clinit>`, and whose superclasses and superinterfaces
/// are initialized or have none either, is NOT pending: initializing it runs no
/// code and cannot fail, so skipping it is unobservable. That keeps the common
/// cold `throw new SomeException(...)` (loaded by the verifier, never
/// instantiated, no static initializer) on its resolved row, and the method on
/// the optimizing tier. Superinterfaces are walked whether or not they declare
/// default methods (JVMS only initializes those), which can only answer
/// "pending" too often. Initialization is monotonic, so a "not pending" answer
/// never goes stale.
///
/// Initialization also LINKS each class first (`vm_util::initialize_class_shared`,
/// Step 1), and a link error is observable too: a class never linked whose link
/// is not trivial (`vm::class_linked_or_link_trivial`) is pending as well
/// (wave 8, L5, `interpreter-L5-compiled-new-skips-linking-of-a-never-linked-class-FIXED`).
/// The compile doors that hold no lock ask
/// [`jit_new_target_pending_after_link`], which links such a class first when
/// that runs no Java, so it keeps its resolved row.
pub(in crate::runtime::interpreter) fn jit_new_target_init_pending(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    target_id: ClassId,
) -> bool {
    !matches!(
        jit_new_target_init_walk(shared, cm, target_id),
        NewTargetInit::Settled
    )
}

/// What a `new` of a class still owes before it may compile to a resolved row
/// ([`jit_new_target_init_pending`]).
pub(in crate::runtime::interpreter::jit_bridge) enum NewTargetInit {
    /// Nothing observable: the row may be resolved.
    Settled,
    /// A `<clinit>` to run, an erroneous class, a known link failure, or a
    /// class `new` may not instantiate (`InstantiationError`).
    Pending,
    /// Only these never-linked classes of the chain stand in the way.
    NeedsLink(Vec<ClassId>),
}

pub(in crate::runtime::interpreter::jit_bridge) fn jit_new_target_init_walk(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    target_id: ClassId,
) -> NewTargetInit {
    // A class hierarchy is shallow; a walk this long is a malformed or cyclic
    // graph, and "pending" is the safe answer for it.
    const MAX_WALK: usize = 256;
    // JVMS 6.5: a `new` of an interface or abstract class throws
    // `InstantiationError`, which only the CP-indexed helper raises; a resolved
    // row would allocate one (wave 16, `new_instantiation_refusal`).
    if cm
        .get_class(target_id)
        .and_then(super::gc_and_alloc::new_instantiation_refusal)
        .is_some()
    {
        return NewTargetInit::Pending;
    }
    let mut todo: Vec<ClassId> = vec![target_id];
    let mut unlinked: Vec<ClassId> = Vec::new();
    let mut walked = 0usize;
    while let Some(id) = todo.pop() {
        walked += 1;
        if walked > MAX_WALK {
            return NewTargetInit::Pending;
        }
        let Some(class) = cm.get_class(id) else {
            return NewTargetInit::Pending;
        };
        // An initialized class's superclass and superinterfaces were settled
        // before it (§5.5 step 7), so nothing above it needs walking. An
        // initialized INTERFACE settled none of its superinterfaces (step 7 is
        // for classes only; `vm_util::superinterfaces_to_initialize`), so
        // those are still walked.
        if crate::vm::is_class_initialized_fast(class) {
            if class.is_interface() {
                todo.extend(class.interfaces.iter().copied());
            }
            continue;
        }
        // A class whose linking already failed owes the linkage error again
        // (`vm_util::LINK_FAILED_STATE`), like an erroneous one.
        if class.state == crate::classloading::ClassState::InitializationError
            || crate::vm::class_link_failed(class)
            || class.methods.iter().any(|m| &*m.name == "<clinit>")
        {
            return NewTargetInit::Pending;
        }
        let linked_or_trivial = crate::vm::class_linked_or_link_trivial(shared, cm, class);
        if !linked_or_trivial {
            unlinked.push(id);
        }
        // Interpreter round i5 wave 106, lane L1 (i102 residue): a never-linked
        // class a resolved row may allocate unlinked is recorded for the JDWP
        // listing; a linked one costs a state compare.
        #[cfg(feature = "experimental-debug")]
        {
            if linked_or_trivial {
                crate::debug::note_class_used_unlinked(shared, class);
            }
        }
        todo.extend(class.superclass);
        todo.extend(class.interfaces.iter().copied());
    }
    if unlinked.is_empty() {
        NewTargetInit::Settled
    } else {
        NewTargetInit::NeedsLink(unlinked)
    }
}

/// [`jit_new_target_init_pending`], after linking the never-linked classes
/// that are all that stands in the way, when that runs no Java
/// (`vm::link_class_without_java`). Takes the class-manager lock itself, so
/// the caller must hold none.
///
/// A link that cannot be done here (a check fails, a catch type is not loaded
/// yet, another thread holds the class) leaves the site pending: the
/// CP-indexed helper links on a Java thread at run time and raises the error
/// there, as the interpreter's `new` does.
pub(in crate::runtime::interpreter) fn jit_new_target_pending_after_link(shared: &SharedVm, target_id: ClassId) -> bool {
    let walk = {
        let cm = shared.classes.class_manager.read();
        jit_new_target_init_walk(shared, &cm, target_id)
    };
    match walk {
        NewTargetInit::Settled => false,
        NewTargetInit::Pending => true,
        NewTargetInit::NeedsLink(unlinked) => {
            if !unlinked
                .into_iter()
                .all(|id| crate::vm::link_class_without_java(shared, id))
            {
                return true;
            }
            let cm = shared.classes.class_manager.read();
            jit_new_target_init_pending(shared, &cm, target_id)
        }
    }
}

/// Does `code` contain a `new` (`0xbb`) whose constant-pool operand is
/// `cp_idx`? The `new` half of [`CpResolvers::new_site`]'s question: the same
/// `CONSTANT_Class` index also serves `anewarray` / `checkcast` / `instanceof`,
/// which do not initialize, and whose rows should not be given up for a class
/// only a `new` has to initialize.
pub(in crate::runtime::interpreter::jit_bridge) fn code_has_new_of(code: &[u8], cp_idx: u16) -> bool {
    const NEW: u8 = 0xbb;
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == NEW
            && pc + 2 < code.len()
            && u16::from_be_bytes([code[pc + 1], code[pc + 2]]) == cp_idx
        {
            return true;
        }
        // `max(1)`: the decoder steps an undecodable byte by one, but a walk
        // that must terminate does not lean on that.
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    false
}

/// `CRATONVM_JIT_UNINIT_NEW_TRAP` (JIT round 19 wave 2, lane eager2, EAGER19-1
/// = EAGERCOLD18-3 of `jit-r19-eager19-proposals.md`; **default on**):
/// HotSpot's uncommon trap at a `new` whose class is not initialized yet.
///
/// Today such a `new` is answered `Deferred` ([`CpResolvers::new_site`]), so
/// the optimizing tier bails the WHOLE method to single-pass (the deferred-`new`
/// memo re-offers one C2 attempt once the sites resolve) and the single-pass
/// body allocates through `jit_new_object_cp` and dispatches the `<init>`
/// blind. With the switch the method-entry doors admit each such site
/// ([`uninit_new_trap_admitted`]) and the IR builder plants an uncommon trap
/// there (`TrapCause::UninitNew`, `DeoptReason::UncommonTrap` /
/// `DeoptAction::Reinterpret` through `Op::Guard`), keeping the optimizing
/// body for the rest of the method. The first execution traps: the body is
/// evicted (a charged trap), the interpreter runs the `new` (initializing the
/// class), and the trap's service ([`uninit_new_trap_fired`]) spends the
/// site's one-shot budget and asks the late-bind C2 recompile, which compiles
/// the site as the resolved, initialized `new` and splices its constructor.
///
/// Bounds: one trap per `(method, bci)` per VM (a spent site compiles as
/// today: the bail), so a class whose initialization fails (erroneous: the
/// interpreter throws `NoClassDefFoundError` each time) or stays in progress
/// cannot churn; erroneous classes are never admitted; the builder plants
/// only where the trap is resumable (`plant_uncommon_trap`'s replay rule) and
/// never inside a splice. Not the OSR door (EAGER19-4). Read per admitted
/// site and per uncommon trap (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn uninit_new_trap_enabled() -> bool {
    true
}

/// The most `(method, bci)` sites [`JitBridgeState::uninit_new_trap_sites`]
/// remembers; past it no new site is admitted (today's bail).
pub(in crate::runtime::interpreter::jit_bridge) const UNINIT_NEW_TRAP_SITES_MAX: usize = 4096;

/// Does any class `new` of `target_id` would initialize sit in an erroneous
/// state (a failed `<clinit>`, a failed link), or may `new` not instantiate it
/// at all? Such a site keeps today's path: the interpreter answers it with an
/// error every time, so a trap there could only repeat.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_new_target_erroneous(cm: &crate::classloading::ClassManager, target_id: ClassId) -> bool {
    const MAX_WALK: usize = 256;
    if cm
        .get_class(target_id)
        .and_then(super::gc_and_alloc::new_instantiation_refusal)
        .is_some()
    {
        return true;
    }
    let mut todo: Vec<ClassId> = vec![target_id];
    let mut walked = 0usize;
    while let Some(id) = todo.pop() {
        walked += 1;
        if walked > MAX_WALK {
            return true;
        }
        let Some(class) = cm.get_class(id) else {
            return true;
        };
        if class.state == crate::classloading::ClassState::InitializationError
            || crate::vm::class_link_failed(class)
        {
            return true;
        }
        if crate::vm::is_class_initialized_fast(class) && !class.is_interface() {
            continue;
        }
        todo.extend(class.superclass);
        todo.extend(class.interfaces.iter().copied());
    }
    false
}

/// [`uninit_new_trap_enabled`]: may the optimizing compile of `cached` plant
/// an uncommon trap at the `new` at `(cp_idx, pc)`, which the door's new-site
/// resolver answered `Deferred`? `true` only for a class that is LOADED, owes
/// its initialization and is not erroneous, at a site whose one-shot budget is
/// unspent; the site is then recorded for [`uninit_new_trap_fired`]. Called
/// by the jit crate with no class-manager guard held.
pub(in crate::runtime::interpreter::jit_bridge) fn uninit_new_trap_admitted(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    cp_idx: u16,
    pc: usize,
) -> bool {
    if !uninit_new_trap_enabled() {
        return false;
    }
    let Ok(bci) = u32::try_from(pc) else {
        return false;
    };
    if cached.code.get(pc) != Some(&0xbb) {
        return false;
    }
    let key = (
        cached.declaring_class_id.as_u32(),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
        bci,
    );
    if shared
        .jit
        .bridge
        .uninit_new_trap_sites
        .lock()
        .map_or(true, |sites| sites.get(&key).copied().unwrap_or(false))
    {
        // A trap fired here already: compile the site as today.
        return false;
    }
    let candidate = {
        let cm = shared.classes.class_manager.read();
        match resolve_jit_new_site(shared, &cm, cached.declaring_class_id, cp_idx) {
            Some(cratonvm_jit::JitNewSite::Resolved { class_id, .. }) => {
                let target = ClassId::new(class_id);
                jit_new_target_init_pending(shared, &cm, target)
                    && !jit_new_target_erroneous(&cm, target)
                    && cm
                        .get_class(target)
                        .is_some_and(|c| !crate::vm::is_class_initialized_fast(c))
            }
            _ => false,
        }
    };
    if !candidate {
        return false;
    }
    let Ok(mut sites) = shared.jit.bridge.uninit_new_trap_sites.lock() else {
        return false;
    };
    if sites.len() >= UNINIT_NEW_TRAP_SITES_MAX && !sites.contains_key(&key) {
        return false;
    }
    let spent = *sites.entry(key).or_insert(false);
    drop(sites);
    if !spent {
        eager_chain_census_note_uninit_admitted(shared, false);
    }
    if !spent && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] uninit-new TRAP admitted {}.{}{} bci={bci} cp={cp_idx}",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
    !spent
}

/// `CRATONVM_JIT_IR_FIELD_SITE_TRAP` (interpreter round i4 wave 94, lane L2;
/// **default ON** since the wave-97 triage, `=0` kills; the text said "default
/// OFF" until tcm21 read the reader and `types/src/flag_groups.rs`;
/// performance): HotSpot C2's uncommon trap at a field site
/// whose holder is not loaded (`Deoptimization::Reason_unloaded`), instead of
/// bailing the whole method to the single-pass tier.
///
/// The optimizing tier has no field row for a `getstatic` / `putstatic` /
/// `getfield` / `putfield` whose owner the compile doors' peek does not
/// resolve -- a user loader's owner it has not initiated, and, under
/// `CRATONVM_JIT_DEFERRED_FIELD_SITES`, any owner not loaded (the peek loads
/// none: `field_access::field_peek_never_loads_enabled`) -- and such a method
/// lost the optimizing tier for good (the single-pass body defers or refuses
/// the site). With the switch the method-entry optimizing door admits each
/// such site ([`field_site_trap_admitted`]) and the IR builder plants an
/// uncommon trap there (`TrapCause::UnresolvedField`), keeping the optimizing
/// body for the rest of the method. The site is recorded in the
/// uninitialized-`new` trap's one-shot map, so its first firing is served by
/// the same machinery ([`uninit_new_trap_fired`], [`charge_uninit_new_trap`]):
/// the body is evicted without the site-trap IR ban, the interpreter runs the
/// site (loading and initialising the owner, or throwing), and the late-bind
/// recompile sees the owner loaded. A spent site is never admitted again.
/// Not the OSR door (as EAGER19-1). Read per row-less field site of an
/// optimizing method-entry compile (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn ir_field_site_trap_enabled() -> bool {
    true
}

/// [`ir_field_site_trap_enabled`]: may the optimizing compile of `cached`
/// plant an uncommon trap at the field site at `(cp_idx, pc)`? `Some(the
/// Fieldref's descriptor tag)` only when its owner is NOT known to the
/// holder's loader without loading (`constants::class_resolved_without_loading`
/// answers `None`; nothing is loaded or resolved here) -- a site that has never
/// run, since running it loads the owner -- and the site's one-shot budget is
/// unspent; the site is then recorded for [`uninit_new_trap_fired`]. Called by
/// the jit crate with no class-manager guard held.
///
/// `door` names the compile door for the `CRATONVM_DBG_JITC` census line
/// (`upgrade`: [`compile_optimizing_artifact`]; `callee`: the by-name door
/// [`try_jit_compile_callee_slow`], which is where the background C2 task
/// compiles). Interpreter round i4 wave 95, lane L2: wave 94 wired only the
/// upgrade door, and that door answers a method that already has a
/// single-pass body WITH that body (`try_jit_upgrade_with_gate_uncontained`'s
/// cache check) instead of compiling it, while the background tiered worker's
/// C2 task compiles through the callee door, which never asked this. So under
/// the background compiler the trap was never admitted and the method kept
/// its single-pass body (`L2W94ClassPathColdFieldBench` cp-cold 18 ns with and
/// without the switch, the single-pass tier's speed, not the deferred helper:
/// the timed calls never reach the cold branch).
pub(in crate::runtime::interpreter::jit_bridge) fn field_site_trap_admitted(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    cp_idx: u16,
    pc: usize,
    door: &str,
) -> Option<u8> {
    if !ir_field_site_trap_enabled() {
        return None;
    }
    let bci = u32::try_from(pc).ok()?;
    if !matches!(cached.code.get(pc), Some(0xb2..=0xb5)) {
        return None;
    }
    let holder = cached.declaring_class_id;
    if !field_site_owner_unloaded(shared, holder, cp_idx) {
        return None;
    }
    let tag = jit_deferred_field_tag(shared, holder, cp_idx)?;
    let key = (
        holder.as_u32(),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
        bci,
    );
    let Ok(mut sites) = shared.jit.bridge.uninit_new_trap_sites.lock() else {
        return None;
    };
    if sites.len() >= UNINIT_NEW_TRAP_SITES_MAX && !sites.contains_key(&key) {
        return None;
    }
    let spent = *sites.entry(key).or_insert(false);
    drop(sites);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] unresolved-field TRAP {} {}.{}{} bci={bci} cp={cp_idx} door={door}",
            if spent { "spent, not admitted" } else { "admitted" },
            cached.class_name,
            cached.method_name,
            cached.method_descriptor
        );
    }
    (!spent).then_some(tag)
}

/// Is the owner of the Fieldref at `cp_idx` of `holder` NOT known to the
/// holder's loader without loading? Then the site has never run (running it
/// loads the owner). Nothing is loaded or resolved here. Takes the
/// class-manager read lock: call with no guard held.
pub(in crate::runtime::interpreter::jit_bridge) fn field_site_owner_unloaded(shared: &SharedVm, holder: ClassId, cp_idx: u16) -> bool {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(holder) else {
        return false;
    };
    let Some(ConstantPoolEntry::FieldReference { class_index, .. }) =
        class.constant_pool.get(cp_idx)
    else {
        return false;
    };
    let Some(owner) = class.constant_pool.get_class_name(*class_index) else {
        return false;
    };
    super::constants::class_resolved_without_loading(shared, &cm, holder, &owner.to_string())
        .is_none()
}

/// The deoptimization service saw an uncommon trap at `bci` of
/// `class_id.method_name descriptor` (`helpers::deoptimize_in`): when it is a
/// site [`uninit_new_trap_admitted`] recorded and has not fired before, spend
/// its budget and ask the late-bind C2 recompile, which compiles the `new` as
/// resolved now that the interpreter's `new` initialized the class. Cold: one
/// lock when the map is non-empty, per trap.
pub(crate) fn uninit_new_trap_fired(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    bci: u32,
) {
    let first = {
        let Ok(mut sites) = shared.jit.bridge.uninit_new_trap_sites.lock() else {
            return;
        };
        if sites.is_empty() {
            return;
        }
        let key = (
            class_id.as_u32(),
            Arc::<str>::from(method_name),
            Arc::<str>::from(descriptor),
            bci,
        );
        match sites.get_mut(&key) {
            Some(spent) if !*spent => {
                *spent = true;
                true
            }
            _ => false,
        }
    };
    if !first {
        return;
    }
    if eager_chain_census_enabled() {
        shared
            .jit
            .bridge
            .eager_chain_census
            .uninit_fired_first
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] uninit-new TRAP taken {class_name}.{method_name}{descriptor} \
             bci={bci}: site spent, late-bind recompile requested"
        );
    }
    if !crate::runtime::env_cache::bg_compile() {
        // The mutator re-tiers the evicted method at its threshold.
        return;
    }
    shared.jit.tiered_manager.request_late_bind_recompile(
        &crate::jit::tiered::MethodKey::with_class_id(
            class_id,
            class_name,
            method_name,
            descriptor,
        ),
    );
}

/// `CRATONVM_JIT_UNINIT_NEW_GUARD` (JIT round 19 wave 3, lane eager3, EAGER2-3
/// of `jit-r19-eager2-proposals.md`; **default on**): a REAL class-initialized
/// guard at an uninitialized `new`, instead of EAGER19-1's unconditional trap.
///
/// The method-entry doors ask [`uninit_new_guard_admitted`] for each `new` the
/// new-site resolver answered `Deferred` (BEFORE the EAGER19-1 trap
/// admission): a LOADED, not initialized, not erroneous class at an unspent
/// site is answered with its resolved row and its own init word
/// (`Class::init_state`). The optimizing tier then compiles the site as
/// `Guard(ClassInitTest(word)); New` (`IrBuilder::plant_class_init_guard`): a
/// body compiled before the class initialized allocates without a trap once
/// any thread initializes it. Only when the guard fails (the class is still not
/// initialized when the body first reaches the `new`) does the body deopt, at
/// the `new`, and that deopt is served exactly as the EAGER19-1 trap (the site
/// is recorded in the same map: one-shot budget, late-bind recompile). A
/// refused plant (no frame state, a replay-unsafe prefix, a splice) falls back
/// to the EAGER19-1 trap when that switch admits the site, else to today's
/// bail. Independent of `CRATONVM_JIT_UNINIT_NEW_TRAP`. Read per deferred `new`
/// site of an optimizing method-entry compile (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn uninit_new_guard_enabled() -> bool {
    true
}

/// [`uninit_new_guard_enabled`]: the guard row for the `new` at `(cp_idx, pc)`
/// of `cached`, or `None`. Admits exactly what [`uninit_new_trap_admitted`]
/// admits (a LOADED class owing its initialization, not erroneous, at an
/// unspent site), and records the site in the same map, so the guard's failing
/// edge is recognised by [`uninit_new_trap_fired`]. Called by the jit crate
/// with no class-manager guard held.
pub(in crate::runtime::interpreter::jit_bridge) fn uninit_new_guard_admitted(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    cp_idx: u16,
    pc: usize,
) -> Option<cratonvm_jit::UninitNewGuardSite> {
    if !uninit_new_guard_enabled() {
        return None;
    }
    let bci = u32::try_from(pc).ok()?;
    if cached.code.get(pc) != Some(&0xbb) {
        return None;
    }
    let key = (
        cached.declaring_class_id.as_u32(),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
        bci,
    );
    if shared
        .jit
        .bridge
        .uninit_new_trap_sites
        .lock()
        .map_or(true, |sites| sites.get(&key).copied().unwrap_or(false))
    {
        // Spent (a trap or a failed guard fired here already): as today.
        return None;
    }
    let site = {
        let cm = shared.classes.class_manager.read();
        match resolve_jit_new_site(shared, &cm, cached.declaring_class_id, cp_idx) {
            Some(cratonvm_jit::JitNewSite::Resolved {
                class_id,
                num_fields,
                needs_post_alloc_init,
                has_finalizer,
            }) => {
                let target = ClassId::new(class_id);
                let class = cm.get_class(target)?;
                (jit_new_target_init_pending(shared, &cm, target)
                    && !jit_new_target_erroneous(&cm, target)
                    && !crate::vm::is_class_initialized_fast(class))
                .then(|| cratonvm_jit::UninitNewGuardSite {
                    class_id,
                    num_fields,
                    needs_post_alloc_init,
                    has_finalizer,
                    init_state: Arc::clone(&class.init_state),
                    initialized: cratonvm_classloading::CLASS_INIT_INITIALIZED,
                })
            }
            _ => None,
        }
    }?;
    {
        let Ok(mut sites) = shared.jit.bridge.uninit_new_trap_sites.lock() else {
            return None;
        };
        if sites.len() >= UNINIT_NEW_TRAP_SITES_MAX && !sites.contains_key(&key) {
            return None;
        }
        if *sites.entry(key).or_insert(false) {
            return None;
        }
    }
    eager_chain_census_note_uninit_admitted(shared, true);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] uninit-new GUARD admitted {}.{}{} bci={bci} cp={cp_idx}",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
    Some(site)
}

/// Is `bci` of `class_id.method_name descriptor` a site the uninitialized-`new`
/// trap or guard was admitted at ([`uninit_new_trap_admitted`],
/// [`uninit_new_guard_admitted`]), spent or not? Cold: one lock when the map
/// is non-empty, per trap (`helpers::despeculate_trapped_method`).
pub(crate) fn uninit_new_trap_site_known(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    bci: u32,
) -> bool {
    let Ok(sites) = shared.jit.bridge.uninit_new_trap_sites.lock() else {
        return false;
    };
    if sites.is_empty() {
        return false;
    }
    sites.contains_key(&(
        class_id.as_u32(),
        Arc::<str>::from(method_name),
        Arc::<str>::from(descriptor),
        bci,
    ))
}

/// The most instructions [`JitBridgeState::indy_late_link_reprieves`] records.
pub(in crate::runtime::interpreter::jit_bridge) const INDY_LATE_LINK_REPRIEVES_MAX: usize = 4096;



/// [`indy_late_link_trap_reprieve`]'s decision from the instruction's state:
/// `linked` is `None` for an instruction no linked bridge ever serves (not an
/// `InvokeDynamic` entry, a `void` site), else whether it has its generic
/// linkage. A linked one is reprieved only when the bridge serves it NOW
/// (`bridgeable_now`: the trapping body was built before the link); an
/// unlinked one once per instruction (`first_unlinked_trap`: the interpreter
/// links it as it resumes there, and a site whose bootstrap never publishes a
/// linkage must not be reprieved for ever). The closures run only when asked.
pub(in crate::runtime::interpreter::jit_bridge) fn indy_late_link_reprieve_verdict(
    linked: Option<bool>,
    bridgeable_now: impl FnOnce() -> bool,
    first_unlinked_trap: impl FnOnce() -> bool,
) -> bool {
    match linked {
        None => false,
        Some(false) => first_unlinked_trap(),
        Some(true) => bridgeable_now(),
    }
}

/// JIT round 21 lane indylate (`CRATONVM_JIT_INDY_LATE_LINK_TRAP_RECOMPILE`):
/// is the trap at `bci` of `class_id.method_name descriptor` an
/// `invokedynamic` trap planted only because the instruction was not LINKED
/// when the body was built -- one the linked bridge
/// (`invokedynamic::make_jit_indy_linked_site_from_parts`) serves once the
/// interpreter has linked it?
///
/// Both tiers lower such an instruction to an unconditional trap (its interned
/// bridge site is `0` until a row exists). The charge then retired the method
/// on its first trap: the single-pass tier's `UnreachedCode` is
/// `MakeNotCompilable`, the optimizing tier's site-trap policy bans the IR
/// tier stickily. Both were written for a bootstrap no bridge serves, where
/// the trap fires on every call; for a late bind (a `ConstantCallSite` /
/// `MutableCallSite` bootstrap whose instruction first runs after the
/// method's compiles -- `R21IndyConstLateBind` const-late-bind) the next
/// compile bridges the site, and the optimizing tier may then plan the bound
/// target as a plain call. `true` asks the caller to charge a recompile
/// (`SpeculationFailed`) instead; its count escalation still bounds a site
/// that keeps trapping.
///
/// Takes the class-manager read lock and, for a linked instruction, the
/// bridge-site intern table (the order [`interned_jit_indy_bridge_site`]
/// documents), so the recompile finds the site already built. Cold: called
/// per trap charged as `UnreachedCode` or at a planted IR site trap.
pub(crate) fn indy_late_link_trap_reprieve(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    bci: u32,
) -> bool {
    if class_id.as_u32() == 0
        || bci == u32::MAX
        || !crate::runtime::invokedynamic::jit_indy_linked_call_site_enabled()
        || !crate::runtime::env_cache::jit_indy_bridge()
    {
        return false;
    }
    let at = bci as usize;
    let reprieved = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return false;
        };
        let Some(cp_idx) = class
            .find_method(method_name, descriptor)
            .and_then(|m| m.code())
            .and_then(|code| indy_operand_at(code.code.as_bytes(), at))
        else {
            return false;
        };
        let linked = crate::runtime::invokedynamic::jit_indy_instruction_linked(
            shared.vm_identity,
            &class.constant_pool,
            class_id,
            cp_idx,
            (method_name, descriptor, at),
        );
        indy_late_link_reprieve_verdict(
            linked,
            || {
                interned_jit_indy_bridge_site(
                    shared,
                    &cm,
                    class,
                    class_id,
                    cp_idx,
                    Some((method_name, descriptor, at)),
                ) != 0
            },
            || {
                let Ok(mut seen) = shared.jit.bridge.indy_late_link_reprieves.lock() else {
                    return false;
                };
                if seen.len() >= INDY_LATE_LINK_REPRIEVES_MAX {
                    return false;
                }
                seen.insert((
                    class_id.as_u32(),
                    Arc::<str>::from(method_name),
                    Arc::<str>::from(descriptor),
                    bci,
                ))
            },
        )
    };
    if reprieved && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[indy-late] {class_name}.{method_name}{descriptor} bci={bci}: invokedynamic trap of \
             an instruction the linked bridge serves once linked, charged as a recompile"
        );
    }
    reprieved
}

/// The deopt service's charge for an uninitialized-`new` trap or failed guard
/// (EAGER19-1 / EAGER2-3), named either by the point's own cause (EAGER2-4,
/// `CRATONVM_JIT_TRAP_CAUSE_ON_POINT`) or by its admitted `(method, bci)`
/// ([`uninit_new_trap_site_known`]).
///
/// Charged as the `UncommonTrap` it is (the body is evicted, and
/// `deoptimize_in` spends the site and asks the late-bind recompile through
/// [`uninit_new_trap_fired`]) -- and NOT through the IR site-trap policy
/// (`helpers::despeculate_trapped_method`), which bans the optimizing tier for
/// the method (stickily, under the default `CRATONVM_C2_ACCEPT` evidence
/// policy) and charges `SpeculationFailed`, which the hook does not serve: the
/// recompile EAGER19-1 exists to buy would then never be asked, and the method
/// would stay on the single-pass tier for good, where before the trap the
/// deferred-`new` memo re-offered it one C2 attempt. Correctness of the
/// EAGER19-1 path, so unconditional (it only engages at an admitted site).
pub(crate) fn charge_uninit_new_trap(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    bci: u32,
    by_point_cause: bool,
) {
    eager_chain_census_note_uninit_taken(shared, by_point_cause);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] uninit-new TRAP charged {class_name}.{method_name}{descriptor} \
             bci={bci} (named by {}): UncommonTrap, no site-trap IR ban",
            if by_point_cause {
                "the point's cause"
            } else {
                "the admitted site"
            }
        );
    }
    crate::jit::helpers::DeoptimizationController::deoptimize_or_by_name(
        shared,
        class_id,
        class_name,
        method_name,
        descriptor,
        cratonvm_jit::deopt::DeoptReason::UncommonTrap,
        bci,
    );
}

/// `CRATONVM_DBG_EAGER_CHAIN_CENSUS` (JIT round 19 wave 3, lane eager3;
/// diagnostic, **default OFF**): the startup / compile-cost census of the eager
/// callee chain, printed once at exit
/// ([`report_eager_chain_census_at_exit`]): eager callee compiles by door and
/// by tier, refusals by rule, how many of the eagerly compiled callees ever
/// ran, and the uninitialized-`new` trap / guard rows. The number that decides
/// whether any of the opt-in eager-chain switches is worth flipping on a real
/// application boot. Per VM ([`JitBridgeState::eager_chain_census`]). Read per
/// eager-chain event (cold: compile time) and once at exit.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_chain_census_enabled() -> bool {
    false
}

/// The eager-chain doors [`EagerChainCensus`] counts by: the ordinary chain
/// (`direct_callee_lookup_bound` in [`try_jit_compile_callee_slow`]), the
/// optimizing door's nested static callee
/// ([`compile_unpublished_static_callee_bind`]) and, since JIT round 19
/// proposal wave 6 (lane osrentry6, EAGER3-19-2), the synchronized direct
/// door's compile ([`sync_direct_compile_unpublished_callee`]: its four bounds
/// as refusals, its compile as a compile; diagnostic only, under the census's
/// own switch).
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_DOOR_ORDINARY: usize = 0;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_DOOR_OPTIMIZING_STATIC: usize = 1;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_DOOR_SYNC_DIRECT: usize = 2;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_DOORS: usize = 3;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_DOOR_NAMES: [&str; EAGER_DOORS] = ["ordinary", "optimizing-static", "sync-direct"];

/// The rules an eager-chain compile is refused by, as [`EagerChainCensus`]
/// counts them.
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_CHAIN_OFF: usize = 0;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_DEPTH: usize = 1;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_CYCLE: usize = 2;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_COLD: usize = 3;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_NEVER_RUN: usize = 4;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_BUDGET: usize = 5;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_ANOTHER_LOADER: usize = 6;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_BREAKPOINT: usize = 7;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULES: usize = 8;
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RULE_NAMES: [&str; EAGER_RULES] = [
    "chain-off",
    "depth",
    "cycle",
    "cold",
    "never-run",
    "budget",
    "another-loaders-copy",
    "breakpoint",
];

/// The most eagerly compiled bodies [`EagerChainCensus::bodies`] remembers for
/// the "ever ran" row; past it the row counts them as `unrecorded`.
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_CHAIN_CENSUS_BODIES_MAX: usize = 65_536;

/// One eagerly compiled callee, for the "ever ran" row: the door, the
/// profile-store key of the DECLARING method and its count at the compile, and
/// the body, weakly (the census must not keep a superseded body mapped).
pub(in crate::runtime::interpreter::jit_bridge) struct EagerCompiledBody {
    pub(in crate::runtime::interpreter::jit_bridge) invoc_key: u128,
    pub(in crate::runtime::interpreter::jit_bridge) count_at_compile: u64,
    pub(in crate::runtime::interpreter::jit_bridge) body: std::sync::Weak<cratonvm_jit::CompiledMethod>,
}

/// Per-VM counters behind `CRATONVM_DBG_EAGER_CHAIN_CENSUS`
/// ([`eager_chain_census_enabled`]). Written only with the switch on.
#[derive(Default)]
pub(crate) struct EagerChainCensus {
    /// `[door][tier]` eager compiles that produced a body (tier `0` = single
    /// pass, `1` = optimizing).
    pub(in crate::runtime::interpreter::jit_bridge) compiled: [[std::sync::atomic::AtomicU64; 2]; EAGER_DOORS],
    /// Eager compiles that produced no body, by door.
    pub(in crate::runtime::interpreter::jit_bridge) declined: [std::sync::atomic::AtomicU64; EAGER_DOORS],
    /// `[door][rule]` refusals before any compile.
    pub(in crate::runtime::interpreter::jit_bridge) refused: [[std::sync::atomic::AtomicU64; EAGER_RULES]; EAGER_DOORS],
    /// The bodies, for the "ever ran" row.
    pub(in crate::runtime::interpreter::jit_bridge) bodies: std::sync::Mutex<Vec<EagerCompiledBody>>,
    /// Bodies past [`EAGER_CHAIN_CENSUS_BODIES_MAX`].
    pub(in crate::runtime::interpreter::jit_bridge) bodies_unrecorded: std::sync::atomic::AtomicU64,
    /// Uninitialized-`new` sites admitted: `[trap, guard]`.
    pub(in crate::runtime::interpreter::jit_bridge) uninit_admitted: [std::sync::atomic::AtomicU64; 2],
    /// Uninitialized-`new` traps / failed guards charged: `[by admitted site,
    /// by the point's cause]`.
    pub(in crate::runtime::interpreter::jit_bridge) uninit_taken: [std::sync::atomic::AtomicU64; 2],
    /// Admitted sites whose trap / failed guard FIRST fired
    /// ([`uninit_new_trap_fired`]: site spent, recompile asked).
    pub(in crate::runtime::interpreter::jit_bridge) uninit_fired_first: std::sync::atomic::AtomicU64,
    /// Deopt points that fired with a recorded cause
    /// (`CompiledMethod::trap_cause_of_point`), by `TrapCause::code`.
    /// Five since interpreter round i4 wave 94 (`TrapCause::UnresolvedField`).
    pub(in crate::runtime::interpreter::jit_bridge) taken_by_point_cause: [std::sync::atomic::AtomicU64; 5],
}

/// Count one eager-chain refusal of `door` by `rule`.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_chain_census_note_refused(shared: &SharedVm, door: usize, rule: usize) {
    if !eager_chain_census_enabled() {
        return;
    }
    if let Some(cell) = shared
        .jit
        .bridge
        .eager_chain_census
        .refused
        .get(door)
        .and_then(|row| row.get(rule))
    {
        cell.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The ordinary door's refusal (`dc_no!` in [`try_jit_compile_callee_slow`]),
/// counted when it is one of the eager chain's own rules.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_chain_census_note_direct_refusal(
    shared: &SharedVm,
    reason: cratonvm_jit::DirectBindRefusal,
) {
    use cratonvm_jit::DirectBindRefusal as R;
    let rule = match reason {
        R::CalleeNotYetCompiled => EAGER_RULE_CHAIN_OFF,
        R::EagerChainDepth => EAGER_RULE_DEPTH,
        R::EagerChainCycle => EAGER_RULE_CYCLE,
        R::EagerChainCold => EAGER_RULE_COLD,
        R::EagerChainBudget => EAGER_RULE_BUDGET,
        R::AnotherLoadersCopy => EAGER_RULE_ANOTHER_LOADER,
        // `EagerChainCompileDeclined` is counted at the compile itself
        // (`eager_chain_census_note_compiled`), raced or not.
        _ => return,
    };
    eager_chain_census_note_refused(shared, EAGER_DOOR_ORDINARY, rule);
}

/// Count one eager compile of `door` and remember its body for the "ever
/// ran" row. `declaring_id.method descriptor` names the method the profile
/// store counts (the DECLARING class's).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_chain_census_note_compiled(
    shared: &SharedVm,
    door: usize,
    declaring_id: ClassId,
    method: &str,
    descriptor: &str,
    body: Option<&std::sync::Arc<cratonvm_jit::CompiledMethod>>,
) {
    if !eager_chain_census_enabled() || door >= EAGER_DOORS {
        return;
    }
    let census = &shared.jit.bridge.eager_chain_census;
    let Some(body) = body else {
        census.declined[door].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    let tier = usize::from(body.used_ir_backend);
    census.compiled[door][tier].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let invoc_key = cratonvm_jit_api::invoc_key_parts(declaring_id.as_u32(), method, descriptor);
    let count_at_compile = shared.jit.profile_store.invocation_count(invoc_key);
    let Ok(mut bodies) = census.bodies.lock() else {
        return;
    };
    if bodies.len() >= EAGER_CHAIN_CENSUS_BODIES_MAX {
        census
            .bodies_unrecorded
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    bodies.push(EagerCompiledBody {
        invoc_key,
        count_at_compile: u64::from(count_at_compile),
        body: Arc::downgrade(body),
    });
}

/// An uninitialized-`new` site admitted (`guard` = EAGER2-3, else EAGER19-1).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_chain_census_note_uninit_admitted(shared: &SharedVm, guard: bool) {
    if eager_chain_census_enabled() {
        shared.jit.bridge.eager_chain_census.uninit_admitted[usize::from(guard)]
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// An uninitialized-`new` trap or failed guard charged
/// ([`charge_uninit_new_trap`]).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_chain_census_note_uninit_taken(shared: &SharedVm, by_point_cause: bool) {
    if eager_chain_census_enabled() {
        shared.jit.bridge.eager_chain_census.uninit_taken[usize::from(by_point_cause)]
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// EAGER2-4: a deopt point that fired carried the recorded `cause`
/// (`CompiledMethod::trap_cause_of_point`).
pub(crate) fn eager_chain_census_note_point_cause(
    shared: &SharedVm,
    cause: cratonvm_jit::ir::TrapCause,
) {
    if eager_chain_census_enabled() {
        if let Some(cell) = shared
            .jit
            .bridge
            .eager_chain_census
            .taken_by_point_cause
            .get(usize::from(cause.code()))
        {
            cell.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The "ever ran" split of the remembered eager bodies: `(ran, never_ran,
/// unknown)`. Ran: the body's entry counter moved, or the profile store's
/// count of the method grew after the compile (it ran interpreted, e.g. after
/// an eviction). Never ran: neither, and the body is alive with an entry
/// counter (a single-pass body; `CRATONVM_JIT_ENTRY_COUNTER` is on by
/// default). Unknown: neither, and nothing could have counted a compiled call
/// (an optimizing body carries no entry counter, or the body was dropped).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_chain_census_ran_split(shared: &SharedVm) -> (u64, u64, u64) {
    let Ok(bodies) = shared.jit.bridge.eager_chain_census.bodies.lock() else {
        return (0, 0, 0);
    };
    let (mut ran, mut never, mut unknown) = (0u64, 0u64, 0u64);
    for b in bodies.iter() {
        let now = u64::from(shared.jit.profile_store.invocation_count(b.invoc_key));
        // j27 wave 7 (lane T1LAM3): released through the retirement queue
        // (the `Weak` upgrade may be the last owner).
        let body = b.body.upgrade().map(cratonvm_jit::RetainedCode::new);
        let entered = body.as_ref().is_some_and(|m| m.entry_count() > 0);
        if entered || now > b.count_at_compile {
            ran += 1;
        } else if body
            .as_ref()
            .is_some_and(|m| m._jit_entry_counter.is_some())
        {
            never += 1;
        } else {
            unknown += 1;
        }
    }
    (ran, never, unknown)
}

/// Print the `CRATONVM_DBG_EAGER_CHAIN_CENSUS` lines
/// ([`eager_chain_census_enabled`]) for this VM. Called from both shutdown
/// arms (through `deopt_resume::report_inlined_chain_census_at_exit`).
pub(crate) fn report_eager_chain_census_at_exit(shared: &SharedVm) {
    use std::sync::atomic::Ordering::Relaxed;
    if !eager_chain_census_enabled() {
        return;
    }
    let c = &shared.jit.bridge.eager_chain_census;
    let compiles = (0..EAGER_DOORS)
        .map(|d| {
            format!(
                "{} c1={} c2={} declined={}",
                EAGER_DOOR_NAMES[d],
                c.compiled[d][0].load(Relaxed),
                c.compiled[d][1].load(Relaxed),
                c.declined[d].load(Relaxed),
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    eprintln!("[eager-chain-census] compiles: {compiles}");
    let refused = (0..EAGER_DOORS)
        .map(|d| {
            let rules = (0..EAGER_RULES)
                .map(|r| format!("{}={}", EAGER_RULE_NAMES[r], c.refused[d][r].load(Relaxed)))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{} {rules}", EAGER_DOOR_NAMES[d])
        })
        .collect::<Vec<_>>()
        .join(" | ");
    eprintln!("[eager-chain-census] refused: {refused}");
    let (ran, never, unknown) = eager_chain_census_ran_split(shared);
    eprintln!(
        "[eager-chain-census] ran: compiled={} ran={ran} never-ran={never} unknown={unknown} \
         unrecorded={} (ran = entry counter moved or the profile count grew after the \
         compile; unknown = no entry counter (optimizing body) or body dropped)",
        ran + never + unknown,
        c.bodies_unrecorded.load(Relaxed),
    );
    let causes = c
        .taken_by_point_cause
        .iter()
        .enumerate()
        .map(|(i, n)| {
            // Cast: a census index below 5 is a `TrapCause` code.
            let name = cratonvm_jit::ir::TrapCause::from_code(i as u8)
                .map_or("?", cratonvm_jit::ir::TrapCause::as_str);
            format!("{name}={}", n.load(Relaxed))
        })
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "[eager-chain-census] uninit-new: admitted trap={} guard={} | fired-first={} | \
         site-trap-policy bypassed by-site={} by-point-cause={} | points fired with a cause: \
         {causes}",
        c.uninit_admitted[0].load(Relaxed),
        c.uninit_admitted[1].load(Relaxed),
        c.uninit_fired_first.load(Relaxed),
        c.uninit_taken[0].load(Relaxed),
        c.uninit_taken[1].load(Relaxed),
    );
}

/// The constant-pool operand of the `invokedynamic` at `bci`, or `None` when
/// `bci` holds no `invokedynamic` (JIT round 16 wave 10, lane indyir).
pub(in crate::runtime::interpreter::jit_bridge) fn indy_operand_at(code: &[u8], bci: usize) -> Option<u16> {
    const INVOKEDYNAMIC: u8 = 0xba;
    if code.get(bci) != Some(&INVOKEDYNAMIC) {
        return None;
    }
    Some(u16::from_be_bytes([
        *code.get(bci + 1)?,
        *code.get(bci + 2)?,
    ]))
}

/// `CRATONVM_DBG_IR_INDY_SPLICE_CENSUS` -- JIT round 19 wave 2 (lane splice2,
/// the census SPLICE19-2 of `jit-r19-splice19-proposals.md` asks for before
/// any nested indy plan is built). Diagnostic only, default OFF; read once
/// per refused IR-mode body (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn ir_indy_splice_census_enabled() -> bool {
    false
}

/// SPLICE19-2 census: one `[ir] indy-splice-census` line for an IR-mode body
/// the inline resolver refuses for its `invokedynamic`, saying how many indy
/// instructions it has and how many of them a nested indy plan could compile
/// as a plain call today: `interned` = the bridge has a site for the
/// instruction (built by a compile of the callee itself), `constant_direct` =
/// that site answers `jit_indy_constant_direct_target` (a bound constant
/// target; needs `CRATONVM_JIT_INDY_CONSTANT_INVOKE`). A body is a SPLICE19-2
/// candidate when `constant_direct == indy`. Lookup only: no site is built,
/// no bootstrap runs. `requester` names the class whose compile asked (the
/// enclosing body's declaring class for `depth > 0`). Called with the class
/// manager read guard the resolver holds; takes the bridge table's lock after
/// it, the order [`interned_jit_indy_bridge_site`] uses.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn ir_indy_splice_census_line(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    requesting_class_id: ClassId,
    declaring_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    nest_depth: usize,
    code: &[u8],
) {
    const INVOKEDYNAMIC: u8 = 0xba;
    let generation = cm.class_redefine_generation(declaring_id);
    let mut indy = 0usize;
    let mut interned = 0usize;
    let mut constant_direct = 0usize;
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == INVOKEDYNAMIC {
            indy += 1;
            if let Some(cp_idx) = indy_operand_at(code, pc) {
                let (method_key, site_pc) =
                    crate::runtime::invokedynamic::lambda_singleton_site_for(
                        callee_method,
                        callee_desc,
                        pc,
                    );
                let class = declaring_id.as_u32();
                let keys = [
                    (class, generation, cp_idx, method_key, site_pc),
                    (class, generation, cp_idx, 0u64, usize::MAX),
                ];
                let sites = shared.jit.bridge.indy_bridge_sites.lock().ok();
                let site = sites.and_then(|sites| {
                    keys.iter()
                        .find_map(|k| sites.get(k).copied().filter(|&s| s != 0))
                });
                if let Some(site) = site {
                    interned += 1;
                    if crate::runtime::invokedynamic::jit_indy_constant_direct_target(shared, site)
                        .is_some()
                    {
                        constant_direct += 1;
                    }
                }
            }
        }
        let len = inline_instr_length(code, pc);
        if len == 0 {
            break;
        }
        pc += len;
    }
    let requester = cm
        .class_store()
        .get(requesting_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("#{}", requesting_class_id.as_u32()));
    eprintln!(
        "[ir] indy-splice-census requester={requester} callee={callee_class}.{callee_method}{callee_desc} \
         depth={nest_depth} indy={indy} interned={interned} constant_direct={constant_direct} \
         constant_invoke={}",
        u8::from(crate::runtime::invokedynamic::jit_indy_constant_invoke_enabled()),
    );
}

/// The bci of the ONE `invokedynamic` (`0xba`) in `code` whose constant-pool
/// operand is `cp_idx`, or `None` when there is none or more than one. How
/// [`CpResolvers::indy`], whose caller passes only the CP index, names the
/// instruction a lambda bridge site keys its singleton on.
pub(in crate::runtime::interpreter::jit_bridge) fn sole_indy_bci_of(code: &[u8], cp_idx: u16) -> Option<usize> {
    const INVOKEDYNAMIC: u8 = 0xba;
    let mut found = None;
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == INVOKEDYNAMIC
            && pc + 2 < code.len()
            && u16::from_be_bytes([code[pc + 1], code[pc + 2]]) == cp_idx
        {
            if found.is_some() {
                return None;
            }
            found = Some(pc);
        }
        // Same walk, and the same `max(1)`, as `code_has_new_of` above.
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    found
}

/// A `new` site's retry predicate for the held deferred-`new` retries: the
/// site now compiles to a resolved row, i.e. its class is loaded AND
/// [`jit_new_target_init_pending`] is false. The retry machinery only records
/// `new` sites, so the init half applies to every site it asks about.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_new_site_resolves_now(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: u32,
    cp_idx: u16,
) -> bool {
    match resolve_jit_new_site(shared, cm, ClassId::new(holder), cp_idx) {
        Some(cratonvm_jit::JitNewSite::Resolved { class_id, .. }) => {
            !jit_new_target_init_pending(shared, cm, ClassId::new(class_id))
        }
        _ => false,
    }
}

/// [`cratonvm_jit::DeferredNewRetries::take_retry`] for `class.method descriptor`,
/// its site predicate answered by [`jit_new_site_resolves_now`] — the one
/// spelling of that call for its three callers (the re-sweep, the first-call
/// door, the background task).
///
/// The class-manager read guard is taken ONCE, BEFORE the call. The memo holds
/// its write lock while it asks the predicate, and each of the three copies
/// this replaces took `class_manager.read()` inside the predicate, per site:
/// memo, then class manager. A class definition queued for
/// `class_manager.write()` then parked that thread while it held the memo, and
/// every other memo user (every compile's `note_deferred_new_bail`, every
/// sweep) behind it (`i1-L7-process-global-jit-bridge-state-not-vm-scoped`,
/// the lock-order half). With the guard taken first, the memo lock is only
/// ever held around work that cannot block, and the order is the class
/// manager, then the memo.
///
/// One relaxed load first: with nothing armed the memo has nothing to grant,
/// and the common case pays neither lock.
pub(in crate::runtime::interpreter::jit_bridge) fn take_deferred_new_retry_if_resolvable(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // This VM's memo (interpreter round i1 wave 10): its `(holder, cp_idx)`
    // sites are this VM's class ids, asked of this VM's class manager.
    let memo = &shared.jit.bridge.deferred_new_retries;
    if memo.held_count() == 0 {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    memo.take_retry(class_name, method_name, descriptor, &|holder, cp_idx| {
        jit_new_site_resolves_now(shared, &cm, holder, cp_idx)
    })
}

/// `(num_fields, needs_post_alloc_init, has_finalizer)` for an already-resolved `new`
/// target — the three values every compile door has to put in its `new_info`
/// row, computed once here so the doors cannot disagree about them.
///
/// # Why this is not just an extract-method
///
/// The two flags decide whether the codegen may take the pure inline-TLAB
/// path: `bytecode_walk`'s `skip_helper = !needs_post_alloc_init && !has_finalizer`,
/// and only that arm emits an allocation with **no call in it**. Everything
/// else routes through `jit_post_tlab_init`, or — when `can_inline` is false
/// outright — through the full `jit_new_object` helper.
///
/// [`resolve_jit_new_site`] has computed the real flags since it was written.
/// The other two doors did not call it: the interpreter's first-call compile
/// path and `compile_osr_artifact` both pushed a literal
///
/// ```text
/// new_info.push((pc_new, target_id.as_u32(), num_fields, true, true));
/// ```
///
/// under a comment promising "a follow-up should extract the real flags from
/// class metadata to enable the skip path". So on those two doors — which
/// includes **every OSR-compiled hot loop** — no `new` site could ever be
/// inline-allocated, whatever the class actually looked like.
///
/// That is what made `CRATONVM_JIT_ENABLE_INLINE_NEW=1` look like a dead
/// lever. The flag forces `can_inline`, but it does not touch `skip_helper`,
/// so it swapped a `jit_new_object` call for an inline bump plus a
/// `jit_post_tlab_init` call and measured flat (5 243 769/s vs 5 277 311/s
/// in 2026-08-12's table, 108.7 vs 108.6 ns/op when re-taken 2026-08-17).
/// The `fastthreadlocal-2e9-iteration-throughput-wall` page read that flat
/// result as "the gating flags are not what this loop is paying for" and
/// filed the in-tree TODO as measured-and-refuted. The A/B was sound; what it
/// could not show is that the arm never reached the path being tested.
///
/// Those two doors still pushed the literal. Their opt-in to this function
/// (`CRATONVM_JIT_REAL_NEW_SITE_FLAGS`) measured slower (118.0 vs 111.4
/// ns/op, `CtorShapeRateProbe`: `emit_inline_tlab_new` then wrote only the
/// header inline, so `skip_helper` saved a `jit_post_tlab_init` call and
/// nothing else) and was deleted in JIT round 14 wave 2. The bump is inline
/// now (header first, cursor committed last), and since round 15 wave 5 (lane
/// osrtier) the OSR door calls this again
/// (`CRATONVM_JIT_OSR_REAL_NEW_SITE_FLAGS`); the interpreter's first-call
/// door still pushes the literal
/// (`r15w5-osrtier-first-call-door-new-site-flags-patch-FIXED-20260930.md`).
///
/// Conservative in exactly the two places the old code was: an unresolvable
/// class or an unknown superclass reports `(.., true, true)`, which keeps the
/// helper call.
pub(in crate::runtime::interpreter) fn jit_new_site_flags(
    cm: &crate::classloading::ClassManager,
    target_id: ClassId,
) -> (usize, bool, bool) {
    let Some(target) = cm.get_class(target_id) else {
        return (0, true, true);
    };
    let num_fields = target.num_total_fields;
    // JVMS 6.5: `new` initialises its class, and only `jit_new_object` runs
    // `<clinit>` -- the pure inline-TLAB arm (`skip_helper`) and
    // `jit_post_tlab_init` do not. A class not yet initialised at compile time
    // therefore reports `(true, true)`, which keeps the single-pass emitter on
    // the `jit_new_object` call for this site. Initialisation is monotone, so a
    // `true` read here is permanent; a `false` costs only a cold site (a `new`
    // that has run has initialised its class). See
    // r11-x64loop-scalar-replaced-new-skips-clinit-FIXED-20260924.md.
    //
    // LOAD-BEARING for the optimizing tier too (round 11 wave 3). The IR
    // lowerer takes "this class is in `tlab_announce_classes`" as proof that
    // the class is initialised (`ir_lower.rs`,
    // `Lowerer::new_class_proven_initialized`), and `lib.rs` admits a class to
    // that set only when both flags are `false`. So a class that is not
    // initialised must never report `(.., false, false)` here, or the IR
    // `Op::New` gets its inline bump back and runs no `<clinit>`
    // (r11w2-tier-ir-inline-tlab-new-runs-no-clinit-FIXED-20260924.md).
    if !crate::vm::is_class_initialized_fast(target) {
        return (num_fields, true, true);
    }
    // gcg g4/rs: under `CRATONVM_REF_DISCOVER_AT_ALLOCATION` a `java.lang.ref`
    // reference class keeps the helper call (`jit_post_alloc_init` registers
    // it), exactly as a finalizer does: the pure inline bump calls nothing.
    // SBR3 `ref`: unconditionally (the per-VM switch is not in hand here, and a
    // `java.lang.ref` class carries reference fields, so it keeps the helper
    // path on most layouts anyway; the cost is one superclass walk per compile).
    let has_finalizer = target.has_finalizer
        || crate::memory::ref_discovery::reference_kind_in(&cm.class_store, target_id).is_some();
    // Mirror the allocation recipe: long/float/double defaults need typed
    // stores in legacy layouts, as do all reference defaults (Int(0) is not
    // Object(None) there). Int-family defaults are already correct in a zeroed
    // body. The compact-zero flag below can clear the gate for a fully compact
    // class because every JVM default is then represented by zero bytes.
    let mut needs_post_alloc_init = false;
    let mut cid = Some(target_id);
    while let Some(current) = cid {
        let Some(c) = cm.get_class(current) else {
            // Unknown superclass — conservatively keep the helper call.
            needs_post_alloc_init = true;
            break;
        };
        if c.fields.iter().any(|f| {
            !f.is_static()
                && !matches!(
                    f.descriptor.as_bytes().first(),
                    Some(b'I' | b'B' | b'C' | b'S' | b'Z')
                )
        }) {
            // A zeroed legacy Value cell decodes as Int(0), not Object(None).
            // Make the JIT allocation door run post-init for reference-only
            // classes too; the cached helper writes their null defaults.
            needs_post_alloc_init = true;
            break;
        }
        cid = c.superclass;
    }
    // gcf f2/p: a class that allocates COMPACT stores long/float/double
    // tagless, so their typed zero is zero bytes: no prim-init is owed
    // (`gcf-f2p-compiled-new-of-a-compact-class-with-long-fields-calls-the-allocation-helper`).
    // Only for an initialised class without a finalizer, so the `(true, true)`
    // row stays the uninitialised one.
    if needs_post_alloc_init
        && !has_finalizer
        && cratonvm_types::compact_tlab_total_size(target_id.as_u32(), num_fields).is_some()
    {
        needs_post_alloc_init = false;
    }
    (num_fields, needs_post_alloc_init, has_finalizer)
}

/// Would dispatching `class_name.<init>()V` reach a native, rather than the
/// bytecode constructor?
///
/// Split out of [`is_elidable_construction`] so it can be tested directly: the
/// elision decision is a *compile-time prediction* of what a later dispatch will
/// do, and a prediction that drifts from the dispatch is exactly the class of
/// bug the caller's json-smart comment describes.
///
/// # Why this is `resolve_native_dispatch_wave1` and not `find(..).is_some()`
///
/// It used to be the latter, which is the wrong question under strict policy:
/// `JdkOnly` sends a non-`Intrinsic` bridge standing in front of concrete
/// bytecode to the bytecode (§7 step 3), so the native the old check refused
/// over never runs, and the refusal was pure pessimism — the JIT declined to
/// elide a constructor that provably does nothing.
///
/// The two inputs a name triple cannot supply:
///
/// * `compat_native_wins: true` — this site's pre-existing verdict, and a
///   faithful one: today a registered `<init>()V` native wins over the bytecode
///   constructor, which is the whole reason the caller checks at all.
/// * `bytecode_available: true` — established by the caller, not assumed: it
///   only asks after `init.code()` returned `Some`.
///
/// # Why the prediction cannot drift
///
/// The dispatch side of this decision — `admit_forced_native`, reached from
/// `intercept_force_registered_native{,_cached}` — calls the SAME resolver with
/// the SAME two constants (`compat_native_wins: true`, `bytecode_available:
/// true`) for the same triple. One function, one pair of inputs, so compile-time
/// and run-time cannot answer differently. That is the property to preserve if
/// either side is ever changed.
///
/// A `Some(_)` of any shape means "not the trivial bytecode body": `NativeBridge`
/// and `Intrinsic` both run other code, and a strict `Reject` throws, which
/// eliding would silently turn into success. `None` means the bytecode is what
/// executes.
///
/// `Compatible` is bit-for-bit the old behaviour: with `compat_native_wins ==
/// true` the resolver answers `Some` for every registration and `None` for none,
/// which is `find(..).is_some()` spelled through the policy.
pub(in crate::runtime::interpreter) fn elidable_ctor_native_would_run(shared: &SharedVm, class_name: &str) -> bool {
    let registered = shared
        .natives
        .native_methods
        .find_with_kind(class_name, "<init>", "()V");
    crate::vm::resolve_native_dispatch_wave1_in(
        &shared.natives.native_shadow_sink,
        crate::vm::DispatchDoor::ElidableCtor,
        crate::vm::dispatch_policy(shared),
        class_name,
        "<init>",
        "()V",
        registered,
        true,
        true,
    )
    .is_some()
}

/// Whether constructing `class_id` via its no-arg constructor is *elidable* for
/// JIT escape-analysis scalar replacement — i.e. `new C(); dup; invokespecial
/// C.<init>()V` may be replaced by a zero-initialised scalar object with no call.
///
/// SOUND only for the empty default constructor of a direct `java/lang/Object`
/// subclass: `C.<init>()V`'s body is exactly `aload_0; invokespecial
/// java/lang/Object.<init>()V; return` (bytes `2a b7 hi lo b1`). That guarantees
/// the constructor (a) writes NO field (the object stays zero-initialised, so the
/// scalar slots' zero defaults are correct), (b) does NOT escape its receiver,
/// and (c) has NO other side effect (the only call is the empty `Object.<init>`).
///
/// This is deliberately narrower than `classify_init_complexity`'s `Trivial`,
/// which admits arbitrary calls (e.g. `register(this)`) that escape the receiver
/// — unsound to elide. (A future refinement may recurse the super chain to admit
/// non-`Object` supers whose `<init>` is itself elidable.)
pub(in crate::runtime::interpreter) fn is_elidable_construction(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> bool {
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    // Eliding the `<init>` is what lets escape analysis scalar-replace the
    // `new` (both tiers key on this answer), and a scalar-replaced `new` makes
    // no call: nothing would run `<clinit>` (JVMS 6.5) or register the object
    // with the finalizer queue. So refuse a class that is not initialised yet
    // (monotone, so the answer is permanent once true) or that has a finalizer
    // (HotSpot also treats finalizable allocations as escaping). The `new` then
    // keeps its allocation and its constructor call.
    if !crate::vm::is_class_initialized_fast(class) || class.has_finalizer {
        return false;
    }
    let Some(init) = class.find_method("<init>", "()V") else {
        return false;
    };
    // Not `synchronized` (t1 wave 3, lane K; the interpreter door's twin,
    // `invoke_fast::body_is_trivial_ctor_shape`): a boot class's `<init>` is
    // not format-checked here or on HotSpot, whose call then takes the monitor.
    if init.is_synchronized() {
        return false;
    }
    let Some(code) = init.code() else {
        return false;
    };
    // A REGISTERED NATIVE SHADOWS THE BYTECODE CONSTRUCTOR. `invokespecial`
    // prefers a registered native over bytecode, so a trivial-looking
    // `<init>()V` body says nothing about what actually runs — and eliding the
    // call skips the native's side effects entirely.
    //
    // `java/util/HashMap.<init>()V` is exactly this shape: an empty bytecode
    // constructor plus `native_map_init`, which allocates the 16-bucket table
    // and initialises size/threshold. With the call elided, a JIT-compiled
    // `new HashMap<>()` left `table` null; the first `put` then materialised
    // the table through `map_resize`, which DOUBLED the assumed default to 32
    // buckets. Every JIT-created HashMap therefore iterated its keys in a
    // different order than an interpreter-created one holding the same keys —
    // found as a json-smart parse/serialize/re-parse round-trip mismatch at the
    // exact iteration `JSONParserBase.readObject` tiered up
    // (jsonsmart-parser-jit-retired-20260727.md). The companion
    // `map_resize` fix makes the fallback capacity correct; this one keeps the
    // native constructor running in the first place.
    //
    // The §3 item-4 residual of the retired wave-2 markers record.
    // The question is NOT "is a native registered" but "would dispatching this
    // `<init>` reach one" — see [`elidable_ctor_native_would_run`], which is
    // where that distinction and its `bytecode_available` premise are argued.
    // The check sits BELOW the body lookup because that premise is `init.code()`
    // having returned `Some`.
    if elidable_ctor_native_would_run(shared, &class.name) {
        return false;
    }
    let bc = &code.code;
    // aload_0 (0x2a); invokespecial (0xb7) hi lo; return (0xb1) — exactly 5 bytes.
    if bc.len() != 5 || bc[0] != 0x2a || bc[1] != 0xb7 || bc[4] != 0xb1 {
        return false;
    }
    // Cast: numeric/representation conversion
    let mref_idx = ((bc[2] as u16) << 8) | bc[3] as u16;
    let cp = &class.constant_pool;
    let nat_idx = match cp.get(mref_idx) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }) => {
            if cp.get_class_name(*class_index) != Some("java/lang/Object") {
                return false;
            }
            *name_and_type_index
        }
        _ => return false,
    };
    let elidable = matches!(cp.get_name_and_type(nat_idx), Some(("<init>", "()V")));
    if elidable {
        // A caller that elides this constructor depends on its bytecode and
        // records it nowhere; marked under the caller's class-manager guard,
        // which a redefinition's writer excludes (interpreter round i1 wave
        // 20, lane L1; `JitRealm::scoped_redefinition_admitted`).
        shared.jit.jit_cache.note_bytecode_copied(class_id);
    }
    elidable
}

/// Return the constant-pool index for a dynamically dispatched invoke.
#[inline]
pub(in crate::runtime::interpreter) fn jit_native_shadow_dynamic_invoke_index(instruction: &Instruction) -> Option<u16> {
    match instruction {
        // A native-shadowed direct call uses the generic JIT dispatch fallback
        // when no bytecode target is available, preserving its native behavior.
        // Retain the guard for virtual/interface calls: an override can be
        // selected at runtime, which is the ByteBuddy/Object.equals safety case.
        Instruction::Invokevirtual(index) => Some(*index),
        Instruction::Invokeinterface { index, .. } => Some(*index),
        _ => None,
    }
}

/// A final wrapper's primitive accessor is non-overridable and the JIT
/// dispatcher preserves the registered native implementation.  It is therefore
/// safe to compile callers containing these unboxing calls; unlike a general
/// virtual native call, there is no runtime override to bypass.
pub(in crate::runtime::interpreter) fn jit_native_shadow_is_final_wrapper_unbox(
    target_class: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if !matches!(
        target_class,
        "java/lang/Boolean"
            | "java/lang/Byte"
            | "java/lang/Character"
            | "java/lang/Short"
            | "java/lang/Integer"
            | "java/lang/Long"
            | "java/lang/Float"
            | "java/lang/Double"
    ) {
        return false;
    }
    matches!(
        (method_name, descriptor),
        ("booleanValue", "()Z")
            | ("byteValue", "()B")
            | ("charValue", "()C")
            | ("shortValue", "()S")
            | ("intValue", "()I")
            | ("longValue", "()J")
            | ("floatValue", "()F")
            | ("doubleValue", "()D")
    )
}

// Round 15 wave 7 (lane drainB): the `Double` raw-bits exemption that stood
// here is gone. `jit_method_calls_native_shadowed` offers this gate only
// `invokevirtual` / `invokeinterface` sites
// (`jit_native_shadow_dynamic_invoke_index`), and `doubleToRawLongBits` /
// `longBitsToDouble` are `static`, so it could never fire: an `invokestatic`
// of them is not scanned at all, which is what lets the FP_BITS intrinsic
// resolve.

/// Is the native registered for this exact triple a reviewed `Intrinsic`?
pub(in crate::runtime::interpreter::jit_bridge) fn native_is_intrinsic(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor)
        == Some(cratonvm_native_api::NativeKind::Intrinsic)
}

/// The interface-blind fallback of [`jit_invoke_targets_native_shadow`] for an
/// `invokeinterface` whose resolved target is an abstract declaration: could a
/// native registered for `(method_name, descriptor)` on some OTHER class run in
/// place of bytecode for a receiver of this call?
///
/// The fallback exists for a receiver that implements the interface but
/// inherits the method BODY from an unrelated, natively registered class
/// (`GroovyClassValueJava7 extends java.lang.ClassValue`). So an owner counts
/// unless it cannot be that:
///
/// * the call's own target / declaring class — judged by the `direct` and
///   `inherited` arms, which discount a registration on the abstract
///   declaration itself;
/// * an owner whose registration never runs (`registered_native_will_run`);
/// * an owner that is loaded (as the caller sees the name) and resolves the
///   method to no `Code` and no `ACC_NATIVE`: the native is its only
///   implementation, there is no body to shadow, and a compiled call reaches
///   it through the run-time helpers;
/// * an owner that is not loaded, is in a `java/` package (which only the
///   boot and platform loaders may define) and is absent from the runtime
///   image: it can only ever be VM-fabricated, i.e. body-less
///   (`java/util/function/Function$Identity`, the object behind the
///   `Function.identity()` stub, is the case that matters).
///
/// A reviewed `Intrinsic` always counts, and so does an unloaded owner the
/// image cannot rule out. No owner at all (the digest probe answered for a
/// descriptor the compatibility rewrite could match) counts too.
pub(in crate::runtime::interpreter::jit_bridge) fn abstract_declaration_owner_may_shadow(
    shared: &SharedVm,
    caller_class_id: ClassId,
    target_class: &str,
    declaring_class: Option<&str>,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let owners = shared
        .natives
        .native_methods
        .owner_classes_for_method(method_name, descriptor);
    if owners.is_empty() {
        return true;
    }
    owners.iter().any(|owner| {
        let owner: &str = owner;
        if owner == target_class || declaring_class == Some(owner) {
            return false;
        }
        // Takes the class-manager lock itself: asked with none held.
        if !registered_native_will_run(shared, owner, method_name, descriptor) {
            return false;
        }
        if native_is_intrinsic(shared, owner, method_name, descriptor) {
            return true;
        }
        let cm = shared.classes.class_manager.read();
        let Some(owner_id) = class_resolved_without_loading(shared, &cm, caller_class_id, owner)
        else {
            return !(owner.starts_with("java/")
                && cm.runtime_image_defines_class(owner) == Some(false));
        };
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        let Ok(found) =
            resolver.declared_method(&cm, resolver.scope(owner_id), method_name, descriptor)
        else {
            // Nothing declares it from the owner: no body to shadow.
            return false;
        };
        let Ok((declaring_id, index)) = resolver.adopt(found) else {
            return true;
        };
        cm.class_store()
            .get(declaring_id)
            .and_then(|declaring| declaring.methods.get(index as usize))
            .map_or(true, |method| method.is_native() || method.code().is_some())
    })
}

/// # A negative result, kept so it is not re-derived
///
/// `AbstractOwnableSynchronizer.setExclusiveOwnerThread` looks like it belongs
/// beside the final-wrapper exemption above, and the argument for it is sound as far as
/// it goes. The seal is not a tier-up delay — `jit_method_calls_native_shadowed`
/// memoizes its verdict through `mark_jit_bail_listed`, so a method containing
/// such a call is JIT-denied for the lifetime of the process — and three
/// methods on the JDK's uncontended `ReentrantLock` path contain one:
/// `ReentrantLock$NonfairSync.initialTryLock`, `.tryAcquire` and
/// `ReentrantLock$Sync.tryRelease`. That is every `LinkedBlockingQueue.add`
/// and `.take`. The seal's own reason ("a compiled direct call bypasses the
/// interpreter's native-vs-bytecode decision") does not apply to that triple:
/// the callee is never compiled (`registered_native_will_run` refuses it),
/// never spliced (`resolve_inline_site_from` refuses any site whose selected
/// method's declaring class carries a registered native, unconditionally), and
/// cannot be direct-bound (the call is `invokevirtual`, and
/// `pending_callee_compiles` admits kinds 1 and 3 only).
///
/// It was implemented on 2026-08-27, and it is not here because it **bought
/// nothing measurable**. The exemption engaged — `aqs-owner-exempt=4`, and the
/// skip-seal census went `calls-native-shadowed-method=36` to `32`, so four
/// methods really did stop being sealed — and then:
///
/// | | seal lifted | seal kept |
/// |---|---:|---:|
/// | `AqsAttributionProbe` ReentrantLock lock+unlock | 2349 ns | 2336 ns |
/// | `HwtScaleProbe` n=100 000 drain | 767 / 718 ms | 745 / 647 ms |
///
/// One binary, the switch the only variable, interleaved. Unsealing a method
/// is not the same as compiling it, and nothing here says those four ever
/// reached a tier — that is the question anyone reviving this should answer
/// FIRST, with a compile counter, rather than by re-writing the exemption.
/// A relaxation that widens what the JIT will compile is not free of risk, and
/// this one has no measurement to pay for it.
pub(in crate::runtime::interpreter) fn jit_invoke_targets_native_shadow(
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_idx: u16,
) -> bool {
    let (
        target_class,
        method_name,
        descriptor,
        declaring_class,
        is_interface_ref,
        target_is_abstract_declaration,
    ) = {
        let cm = shared.classes.class_manager.read();
        let Some(caller) = cm.get_class(caller_class_id) else {
            return true;
        };
        let (class_index, nat_index, is_interface_ref) = match caller.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index, false),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index, true),
            _ => return true,
        };
        let Some(target_class) = caller
            .constant_pool
            .get_class_name(class_index)
            .map(str::to_string)
        else {
            return true;
        };
        let Some((method_name, descriptor)) = caller.constant_pool.get_name_and_type(nat_index)
        else {
            return true;
        };
        let method_name = method_name.to_string();
        let descriptor = descriptor.to_string();
        // `target_is_abstract_declaration`: the resolved method has no `Code`
        // and is not `ACC_NATIVE` — a native registered on it runs only through
        // `resolve_dispatch`'s no-`Code` branch, i.e. for a receiver with no
        // implementation of its own. Census-only (see the `direct` split below).
        let (declaring_class, target_is_abstract_declaration) = if let Some(target_id) =
            jit_known_class_at(
                shared,
                &cm,
                &target_class,
                caller_class_id,
                Some(class_index),
            ) {
            let store = cm.class_store();
            match crate::classloading::find_method_recursive(
                target_id,
                &method_name,
                &descriptor,
                store,
            ) {
                Some((method, declaring_id)) => (
                    store
                        .get(declaring_id)
                        .map(|declaring| declaring.name.to_string()),
                    !method.is_native() && method.code().is_none(),
                ),
                None => (None, false),
            }
        } else {
            (None, false)
        };
        (
            target_class,
            method_name,
            descriptor,
            declaring_class,
            is_interface_ref,
            target_is_abstract_declaration,
        )
    };

    if jit_native_shadow_is_final_wrapper_unbox(&target_class, &method_name, &descriptor) {
        return false;
    }
    // A compiled direct call bypasses the interpreter's native-vs-bytecode
    // decision. Treat forced real-JDK overrides exactly like registered
    // native shadows, so a caller of Class's Signature bridge cannot enter
    // the incompatible JDK bytecode body.
    // Both arms ask `registered_native_will_run` rather than
    // `find(..).is_some()`: a native the arbitration always yields is not a
    // shadow, and sealing the caller for it costs tier-up while protecting
    // nothing. Every other triple answers exactly as before.
    // A forced triple seals only while a registration exists that the forced
    // dispatch could reach -- the rule every other consumer of the gate
    // applies (`dispatch_virtual.rs`: `resolve_cached_native_registration`).
    // Under `--real-jdk` the retired HashMap/LinkedHashMap families have none,
    // and the interpreter runs the same bytecode a compiled call would.
    // `--compatible` keeps the name-only answer, byte-for-byte.
    // `CRATONVM_COMPATIBLE_JDK_REIFIER` (wave 57, lane L3): `--compatible`'s
    // generic-metadata rows have no registration then, and seal nothing.
    let forced = native_is_intrinsic(shared, &target_class, &method_name, &descriptor)
        || declaring_class.as_deref().is_some_and(|declaring| {
            native_is_intrinsic(shared, declaring, &method_name, &descriptor)
        });
    let direct =
        forced || registered_native_will_run(shared, &target_class, &method_name, &descriptor);
    let inherited = declaring_class.as_ref().is_some_and(|declaring_class| {
        registered_native_will_run(shared, declaring_class, &method_name, &descriptor)
    });
    // Interpreter round i1 wave 9 (the rule that replaced the upgrade door's
    // `org/elasticsearch/tdigest/Dist` name exemption): a native registered on
    // the resolved ABSTRACT declaration (`Function.apply`, `Supplier.get`, ...,
    // registered for the VM's own body-less objects) shadows no bytecode. A
    // receiver with an implementation runs that implementation
    // (`resolve_dispatch` step 3, real class bytes are authoritative); one
    // without has nothing a compiled caller could bind, so every compiled form
    // of the call reaches the native through the run-time helpers
    // (`jit_invoke_virtual_mic` / `jit_invoke_dispatch` -> `invoke_or_native`),
    // and CHA and PGO inlining guard on the implementor class (audited in
    // wave 8). Not discounted: a forced override, and a reviewed `Intrinsic`,
    // which may win even where bytecode exists. The rule itself is
    // `native_shadow_verdict`.
    let target_native_is_intrinsic = direct
        && target_is_abstract_declaration
        && native_is_intrinsic(shared, &target_class, &method_name, &descriptor);
    let declaring_native_is_intrinsic = inherited
        && target_is_abstract_declaration
        && declaring_class.as_deref().is_some_and(|declaring_class| {
            native_is_intrinsic(shared, declaring_class, &method_name, &descriptor)
        });
    // Interface-dispatch blind spot: for `invokeinterface`, `declaring_class`
    // above is resolved by walking UP FROM THE INTERFACE (`find_method_recursive`
    // starting at the CP-referenced interface's own class_id) — it can only ever
    // land on that same interface (its own abstract/default declaration) or a
    // super-INTERFACE. It can never see a concrete implementor's SUPERCLASS
    // chain, because `Class::interfaces` points only upwards; the reverse edge
    // exists in `ClassStore`'s direct-implementor index (added for
    // class-hierarchy analysis), but this walk does not consult it and it
    // would answer "which classes could implement this", not "which class the
    // receiver here actually is". So
    // for a receiver that implements this interface but inherits the actual
    // method body from an unrelated ancestor CLASS — exactly
    // `org.codehaus.groovy.reflection.v7.GroovyClassValueJava7 implements
    // GroovyClassValue, extends java.lang.ClassValue` inheriting `get()` from
    // `ClassValue`, which IS natively registered — `direct`/`inherited` above
    // both come back false even though the call is genuinely native-shadowed
    // at every concrete receiver. Fall back to a cheap, class-blind "does ANY
    // registered native have this exact (name, descriptor)" probe — the same
    // idiom `might_have_method_descriptor` already serves as a pre-filter
    // elsewhere (e.g. `execute_invokevirtual_vtable_fast`) — and treat a hit
    // as a possible shadow. This can only ever ADD conservatism (a same-named,
    // same-descriptor native for a genuinely unrelated interface is rare and
    // merely costs a missed tier-up opportunity for that one caller, never a
    // correctness bug).
    // `CRATONVM_JIT=-native-shadow-interface-blind` suppresses this arm, so its
    // cost can be A/B'd on a real workload before anyone decides whether to make
    // it precise. Default-on: it is a CORRECTNESS guard (a compiled direct call
    // bypasses the interpreter's native-vs-bytecode choice), and the shape it
    // covers is real — `GroovyClassValueJava7 implements GroovyClassValue,
    // extends java.lang.ClassValue` inheriting a natively-registered `get()`.
    // The lever exists to measure the arm, not to be shipped off.
    //
    // Wave 9: the probe is class-blind only where the target has a body. For
    // an abstract-declaration target it asks which classes carry the
    // registrations (`abstract_declaration_owner_may_shadow`) and discounts the
    // call's own target / declaring class (judged above) and every owner that
    // has no bytecode for the method to shadow. The old class-blind answer is
    // kept (`interface_blind_class_blind`) for the census below.
    let interface_blind_class_blind = is_interface_ref
        && shared
            .natives
            .native_methods
            .might_have_method_descriptor(&method_name, &descriptor);
    let verdict = native_shadow_verdict(
        NativeShadowArms {
            forced,
            direct,
            inherited,
            target_is_abstract_declaration,
            target_native_is_intrinsic,
            declaring_native_is_intrinsic,
            interface_blind_class_blind,
        },
        || {
            abstract_declaration_owner_may_shadow(
                shared,
                caller_class_id,
                &target_class,
                declaring_class.as_deref(),
                &method_name,
                &descriptor,
            )
        },
    );
    // Which arm fired, counted. The three have very different standing:
    // `direct`/`inherited` are precise facts about THIS call, while
    // `interface_blind` is a class-blind "does ANY registered native have this
    // (name, descriptor)" probe whose own comment concedes it "can only ever ADD
    // conservatism". On a Spring Boot context startup this whole predicate seals
    // 1,279 methods out of the JIT — more than the 1,155 that reach C2 — and
    // until now nothing said which arm was responsible for them.
    //
    // MEASURED 2026-08-12 on netty `AdaptiveByteBufAllocatorTest` (dev
    // `6d1bfd531`), which is the shape this predicate should hurt most: 826 M
    // calls, and its hot allocator methods call `ArrayList.add`, `Math.min` and
    // `AtomicIntegerArray.get`, all shadowed. Arm split
    // `direct=474 interface-blind=97 inherited=60` — the class-blind arm is 15%
    // of the population, not the bulk.
    //
    // And the seal is NOT a throughput lever here. Interleaved on one box:
    // default 594 s / 1117 sealed, `-native-shadow-interface-blind` 493 s /
    // 1056 sealed, `-native-shadow-caller-seal` (the whole seal off) **591 s**
    // / 675 sealed. Compiling 626 more methods moved the wall clock 0.5%. So
    // making this arm precise is a correctness/coverage argument, not a
    // performance one — the cost on call-dense code is the per-entry transfer
    // machinery, not the population this seals. See
    // `docs/feature-designs/native-dispatch-architecture.md`.
    // Waves 7-8 counted the abstract-declaration population apart
    // (`direct-abstract-declaration`, `inherited-abstract-declaration`) to size
    // it before admitting it. Wave 9 admits it; what the pre-wave-9 rule would
    // have sealed and this one does not is counted as
    // `abstract-declaration-admitted`, so the change stays measurable.
    if let Some(cause) = verdict.cause() {
        cratonvm_jit::note_jit_native_shadow_cause(cause);
    }
    if verdict.sealed_before_wave9 && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] native-shadow target={}.{}{} direct={} inherited={} interface_blind={} abstract_declaration={} shadow={}",
            target_class,
            method_name,
            descriptor,
            direct,
            inherited,
            verdict.interface_blind,
            target_is_abstract_declaration,
            verdict.shadow()
        );
    }
    verdict.shadow()
}

