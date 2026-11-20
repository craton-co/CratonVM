#[allow(unused_imports)]
use super::*;

/// The compile half of [`compile_osr_artifact`]: resolve the OSR body's
/// per-site tables, run the single-pass backend and publish the artifact.
///
/// Entered only after the door's gates passed and no reusable OSR artifact was
/// cached. `None` refuses the OSR compile; every refusal inside records its own
/// reason, exactly as when this was the door's inline closure. The inputs are
/// the door's own parameters plus what its gates derived, so the stage sees
/// nothing else.
#[allow(clippy::too_many_arguments, clippy::ptr_arg)]
pub(in crate::runtime::interpreter::jit_bridge) fn compile_osr_body(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &String,
    method_name: &String,
    method_descriptor: &String,
    // Read (or adopted) inside the compile's epoch witness (interpreter round
    // i1 wave 43, lane L2; `CompileInputs`).
    inputs: &CompileInputs<OsrBytecode>,
    entry_pc: usize,
    class_name_arc: &Arc<str>,
    method_name_arc: &Arc<str>,
    descriptor_arc: &Arc<str>,
    method_name_check: &str,
    osr_key: &crate::jit::tiered::MethodKey,
) -> Option<Arc<crate::jit::CompiledMethod>> {
    let code: &[u8] = &inputs.get().code;
    let max_locals: usize = inputs.get().max_locals;
    // ── The admission gate ────────────────────────────────────────
    //
    // The ONE door. This path reaches `x64::compile_with_param_slots`
    // directly rather than through `jit::try_compile`, and for a long
    // time that meant it applied whatever subset of `try_compile`'s
    // admission chain someone had noticed was missing:
    //
    //   * RBC.2 — the permanent bail-list, hand-copied here after the
    //     full compile pipeline re-ran on every OSR trigger of a
    //     permanently uncompilable hot method (35,923 wasted pipelines
    //     on `Nat.inc`'s dup_x2 bail in one crypto-prng suite run);
    //   * the bisect levers, hand-copied after every bisect step on the
    //     annotation-scan SIGSEGV read "no effect" while 11 methods
    //     kept compiling;
    //   * the code-cache cap, never copied at all — an OSR compile
    //     could commit code past a cap the ordinary door respected.
    //
    // `compile_gate::admit` asks all of them, in one place, for all
    // three doors, and the token it returns owns the compile-epoch
    // witness. That witness used to be opened ~1,000 lines below, after
    // every class load and constant-pool read this function performs:
    // a redefinition landing in that window produced a body stamped
    // with the CURRENT epoch, which the install barrier then accepted.
    // Holding the token from here is what closes it.
    let admission = match cratonvm_jit::compile_gate::admit(
        // THIS VM's bail list, the one every refusal below records into.
        jit_verdicts(shared),
        class_id,
        &class_name,
        &method_name,
        &method_descriptor,
        cratonvm_jit::compile_gate::CompileDoor::Osr,
        // M7 — handed over even though the OSR door never raises
        // `RuntimeDespeculated`. Which doors honour the verdict is the GATE's
        // decision and is argued in one place; a caller that withheld the
        // registry here would be making that decision again, silently, and the
        // two copies would drift the way every other hand-copied check in this
        // list did.
        Some(&shared.jit.runtime_despec),
    ) {
        Ok(a) => a,
        Err(reason) => {
            // NAME the refusal. `.ok()?` threw the `CompileRefusal`
            // away, and the four it can carry want opposite responses:
            // `PermanentlyBailListed` points at an EARLIER compile of
            // this method that the backend refused (and whose cause
            // `jit_bail_reason_for` still holds), `CodeCacheAtCapacity`
            // is transient, and the other two are configuration. An OSR
            // refusal is silent and permanent, so the one that fired is
            // the whole diagnosis.
            osr_stage("admission-refused");
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr-DENY (admission: {}) {}.{}{} — earlier bail: {}",
                    reason,
                    class_name,
                    method_name,
                    method_descriptor,
                    jit_verdicts(shared)
                        .bail_reason_for(class_id, &class_name, &method_name, &method_descriptor,)
                        .unwrap_or_else(|| "none recorded".to_string()),
                );
            }
            return None;
        }
    };
    osr_stage("past-admission");
    let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::Compilation);
    let osr_decision_started =
        cratonvm_jfr::jit_decision::jit_decision_enabled().then(std::time::Instant::now);
    // `H20-1`'s other half, landed with `H12-1` O1/O2 on
    // 2026-09-22: say which policy this door's direct-call
    // ladder obeys, so `undeclared_direct_bind_rows` can tell a
    // door that refused from a door that never asked. Before
    // this, `--real-jdk` and `--fast-boot` measured an IDENTICAL
    // per-door bind profile and nothing in the tree could say
    // why (`H12-1` §3b).
    //
    // Declaring is not deciding. The decision is
    // `helpers::admit_direct_native_entry`'s, at each bind site,
    // because that is the only place a refusal can fall through
    // to the `invoke_info` build instead of stranding the pc.
    admission.declare_direct_call_policy(
        cratonvm_jit::compile_gate::DirectCallPolicy::from_jdk_only(
            crate::vm::dispatch_policy(shared).is_jdk_only(),
        ),
    );
    // A previous compile for exactly this back-edge produced a body
    // whose `osr_dead_mask` refuses entry there. That verdict is a pure
    // function of a deterministic compile, so re-running the pipeline
    // can only reach it again — 256 times over ten H2 `nioMemLZF:`
    // operations before this memo existed. See `mark_osr_entry_rejected`.
    if jit_verdicts(shared).is_osr_entry_rejected(
        class_id,
        &class_name,
        &method_name,
        &method_descriptor,
        entry_pc,
    ) {
        return None;
    }
    let code_len = code.len().saturating_sub(2); // padded_bytecode adds 2
    let scan = match crate::jit::x64::jit_scan(&code, code_len, &method_descriptor) {
        Some(s) => s,
        None => {
            // RBC.4 — scan rejects are permanent (see jit::try_compile_inner).
            //
            // NAME the opcode. A scan reject here bail-lists the method
            // for EVERY door, so a method whose only route to compiled
            // code is OSR — one big method that is one big loop — runs
            // interpreted for the life of the process, and until this
            // line existed it did so with no output whatsoever.
            // `jit_scan` records the site it refused at; taking it is
            // the difference between "OSR failed" and a bytecode to go
            // look at.
            osr_stage("jit-scan-refused");
            let bail_site = if crate::runtime::env_cache::dbg_jitc()
                || cratonvm_jfr::jit_decision::jit_decision_enabled()
            {
                cratonvm_jit::take_jit_bail_site()
            } else {
                None
            };
            if crate::runtime::env_cache::dbg_jitc() {
                let (site, pc, op) = bail_site.unwrap_or(("<unrecorded>", 0, 0));
                eprintln!(
                    "[cratonvm-jitc] osr-DENY (jit_scan refused: {site} @pc={pc} op=0x{op:02x}) {}.{}{}",
                    class_name, method_name, method_descriptor,
                );
            }
            cratonvm_jit::emit_direct_door_jit_decision(
                &class_name,
                &method_name,
                &method_descriptor,
                cratonvm_jit::compile_gate::CompileDoor::Osr,
                cratonvm_jfr::jit_decision::CompileOutcome::Refused,
                "jit_scan_refused",
                bail_site,
                code_len,
                osr_decision_started,
            );
            jit_verdicts(shared).mark_bail_listed(
                class_id,
                &class_name,
                &method_name,
                &method_descriptor,
            );
            return None;
        }
    };
    osr_stage("past-jit-scan");
    // ── Would the optimizing tier have taken this method? ─────────
    //
    // INERT *in this function*, and deliberately so:
    // `compile_osr_body` reaches `x64::compile_with_param_slots` and has
    // no promotion to refuse. Read the scope carefully — it is the
    // FUNCTION that is single-pass only, not the OSR door. `try_osr`,
    // the caller two frames up, asks `compile_optimizing_artifact`
    // FIRST and enters an optimizing body through
    // `CompiledMethod::ir_osr_enter` when one carries a stub for this
    // bci and is `ir_osr_sentinel_free`; this function is what runs
    // when that declines. The BACKGROUND worker asks the optimizing route
    // itself before it calls this function (`background_osr_optimizing_body`,
    // round 11 wave 9, r11w8-tier-optimizing-osr-route-unreachable-behind-
    // single-pass); only under `CRATONVM_JIT_OSR_OPTIMIZING_BG=0` does a
    // refusal here still mean the optimizing route was never asked.
    // (This comment read "this door ... is
    // single-pass only" until 2026-09-16, which a reader takes as a
    // statement about OSR rather than about one stage of it.)
    //
    // It is a COUNTER, the same shape the String-intrinsic pin already
    // takes at this door and for the same reason -- a zero from a
    // one-door instrument is indistinguishable from "there was nothing
    // to ask about", and that is what made the reach of the optimizing
    // tier unfalsifiable.
    //
    // `ir_compatible_sized` is the FIRST of four gates, so this is an
    // UPPER BOUND on what an OSR route could deliver, which is exactly
    // what a go/no-go on building that route needs. The conjunct that
    // refuses is named on stderr by `ir_reject` under
    // `CRATONVM_DBG_IR_COMPILES`, so the reasons come free.
    //
    // Pure and lock-free: `scan` is already in hand and
    // `ir_compatible_sized` reads nothing else.
    let osr_ir_eligible = cratonvm_jit::ir::ir_compatible_sized(&scan, code_len);
    cratonvm_types::osr_refusal_census::note_ir_eligibility(osr_ir_eligible);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr ir-eligibility: {} for {}.{}{} -- INERT in this STAGE, which is the single-pass body; try_osr and the background worker ask the optimizing route before this runs",
            if osr_ir_eligible { "ACCEPTED" } else { "refused" },
            class_name, method_name, method_descriptor,
        );
    }
    // This method's own exception table. Read ONCE, here, because both
    // of the RBC gates below need it: RBC.6 (immediately below) decides
    // whether an `athrow` method's entries inside a protected range stay
    // enterable, and RBC.6b (further down) admits a non-empty table only
    // when every throwing site inside a protected range publishes a
    // precise exceptional frame.
    //
    // MON10-1 (lane mon11, wave 11; `CRATONVM_JIT_OSR_SYNC_BLOCK_ENTRY`,
    // opt-in): which rows are javac's `synchronized`-block monitor-release
    // catch-any, judged on the CLASSFILE's bytecode (the handler shape), read
    // in the same pass. Empty (every row `false`) with the switch off.
    let (osr_exception_table, osr_monitor_release_rows) =
        match shared.classes.class_manager.read().get_class(class_id) {
            Some(class) => class
                .methods
                .iter()
                .find(|m| {
                    &*m.name == method_name_check && &*m.descriptor == method_descriptor.as_str()
                })
                .and_then(|m| {
                    m.attributes.iter().find_map(|a| match a.as_decoded() {
                        Some(cratonvm_reader::attribute::Attribute::Code(ca)) => {
                            let release_rows: Vec<bool> =
                                if crate::runtime::env_cache::osr_sync_block_entry() {
                                    ca.exception_table
                                        .iter()
                                        .map(|row| is_javac_monitor_release_row(&ca.code, row))
                                        .collect()
                                } else {
                                    Vec::new()
                                };
                            Some((ca.exception_table.clone(), release_rows))
                        }
                        _ => None,
                    })
                })
                .unwrap_or_default(),
            None => (Vec::new(), Vec::new()),
        };
    // RBC.6 (2026-07-18; LIFTED 2026-08-17 for the no-handler case) —
    // this door used to refuse **any** method containing a bare
    // `athrow` (0xbf), whatever its exception table looked like, on the
    // grounds stated in its own comment: "the OSR bail path resumes
    // interpretation at the back-edge, so an athrow lowering that ran
    // side effects natively before throwing could see them re-applied".
    // That hazard is RBC.7's silent-corruption shape and it was real
    // when the comment was written — the athrow drain's only move was
    // to re-stash the throwable and resume the live interpreter frame
    // at the STALE pre-OSR back-edge pc, re-running every iteration the
    // OSR'd code had already committed.
    //
    // Its blast radius was not deliberate. OSR is the ONLY door out of
    // the interpreter for a method invoked once — which is what a
    // `@Test` body, a `main`, and any one-shot driver is — so a `throw`
    // anywhere in such a method, even on a path never taken, kept its
    // hot loop interpreted for the method's whole life. Witness:
    // `BOBYQAOptimizerTest`, whose `trsbox`/`bobyqb` (each called once
    // per test; translated-from-Fortran numerical code that `throw`s a
    // `MathIllegalStateException` on an internal assertion and catches
    // nothing) turned a sub-second `optimize()` call into an unbounded
    // hang. See the known-issue page cited from that suite's RESULTS.
    //
    // What makes the lift safe is not new machinery but a PRECONDITION
    // that is checkable right here: with an EMPTY exception table, an
    // `athrow` in this body cannot be caught by the OSR'd frame, so no
    // drain ever has to resume that frame. `route_osr_exception_out_of_
    // artifact` answers `Propagate` on its first line for exactly this
    // population, and the throwable goes to the dispatch loop's
    // unwinder as `OsrBackoffOutcome::ThrowJava`: the frame is torn
    // down, and there is no stale resume for already-committed
    // iterations to be re-run from. That path is not new either — it is
    // the one the callee-throw fix already routes an unwinding
    // exception through — and it re-checks the empty table rather than
    // assuming it.
    //
    // A NON-empty table (round 11 wave 8, NARROWED from a method-wide
    // refusal — `r11w7-orch-osr-refuses-every-synchronized-block-loop`).
    // It used to be refused outright, which refused every loop holding a
    // javac `synchronized (x) { .. }`: that shape ends its catch-any
    // handler in an `athrow` (`aload; monitorexit; aload; athrow`) that
    // lies OUTSIDE both of its ranges. The argument that refused it:
    //
    //   An `athrow` outside every range publishes no precise frame, so
    //   `route_osr_exception_out_of_artifact` answers `Propagate`, and
    //   `ThrowJava` then handed the throwable to `unwind_to_handler`
    //   keyed on `entry_pc` — the BACK-EDGE the body was entered at. With
    //   that back-edge inside a protected range the unwinder entered a
    //   handler that does not cover the throw, on stale pre-OSR locals.
    //
    // That unwinder half no longer exists: every `ThrowJava` arm in
    // `interpreter.rs` hands `OSR_FRAME_DECLINED_TO_CATCH` (a pc no row can
    // contain) as the unwind pc since 1c3096c87 (2026-08-17, the same day
    // this refusal was written), so the unwinder starts at the CALLER and
    // `entry_pc` never meets this table on the way out. The rest of the
    // path needs no condition on `entry_pc`:
    //
    //   * an `athrow` INSIDE a range is RBC.6b's business below: it is
    //     admitted only while `precise_alloc_athrow_enabled` makes its
    //     lowering publish a reason-9 frame at its own bci, and then the
    //     table is range-tested at the THROW bci;
    //   * an `athrow` OUTSIDE every range publishes nothing, and "no
    //     precise frame ⇒ the throw site is outside every range" is the
    //     deduction RBC.6b's admission makes sound — `Propagate` is the
    //     JVMS answer;
    //   * a compiled local handler that catches publishes nothing either
    //     (the reason-9 stub is on the lookup's MISS edge), so a later
    //     unprotected `athrow` — the javac handler's own — cannot pick up
    //     a stale frame from the throw it caught;
    //   * the optimizing route takes such a body only through its
    //     exception exits (`Op::Throw` pushes a call-exception patch, so it
    //     is never `ir_osr_sentinel_free`): the door admits it only when
    //     every exit bci lies outside every exception-table row
    //     (`osr_exception_exit_bcis_uncovered`), and withholds a header a
    //     handler reaches (`closed_by_a_handler`). It runs BEFORE this
    //     door and asks `CRATONVM_JIT_OSR_ATHROW` itself
    //     (`osr_optimizing_build_declined`, round 12 wave 1).
    //
    // So the one residual kept is belt and braces, not a known hazard:
    // an ENTRY at a pc some protected range covers is withdrawn from the
    // artifact (`withdraw_osr_entries_inside_protected_ranges`, after the
    // backend, below), so `try { for (..) { .. } } catch` with a `throw`
    // anywhere keeps its pre-wave-8 behaviour while every loop whose
    // header no range covers — the synchronized-block population — is
    // admitted. The verdict is per ENTRY PC by construction: it lives in
    // the artifact's `osr_pc_to_native`, which every door (the reuse
    // path, the background arm's `can_osr_enter`, `validate_osr_entry`,
    // `osr_enter`) already reads, and it is memoed per pc by the existing
    // `mark_osr_entry_rejected` / per-pc rejection schedule. A method-wide
    // `mark_osr_denied` here would also have switched off the method's
    // OTHER loops, and the optimizing route with them (`try_osr` asks
    // `is_osr_denied` first).
    //
    // `CRATONVM_JIT_OSR_ATHROW=0` restores the blanket refusal (empty
    // table or not), so one binary can A/B both lifts. That verdict is
    // method-wide and a function of the bytecode and a process-constant
    // flag, so it stays remembered method-wide until the install epoch
    // moves; not bail-listed, because method-entry compilation propagates
    // cleanly through the JIT-return exception drains either way.
    if scan.has_athrow && !crate::runtime::env_cache::osr_athrow_allowed() {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr-DENY (RBC.6 athrow, CRATONVM_JIT_OSR_ATHROW=0, handlers={}) {}.{}{}",
                osr_exception_table.len(),
                class_name,
                method_name,
                method_descriptor
            );
        }
        shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
        return None;
    }
    // Consumed after the backend: see `withdraw_osr_entries_inside_protected_ranges`.
    let rbc6_withdraw_protected_entries = scan.has_athrow && !osr_exception_table.is_empty();
    // RBC.7 (jit-osr-loop-duplicate-execution, silent data corruption,
    // 2026-07-20) — never OSR a method containing `invokedynamic`. The
    // 0xba codegen arm lowers every indy call site to an unconditional
    // `DeoptReason::UnreachedCode` trap (it never links/inlines the
    // bootstrap), so entering that site while OSR-compiled always
    // bails. For a NORMAL (method-entry) compile that bail resumes by
    // building a brand-new frame from scratch (no prior live frame to
    // reconcile), which is precise. For OSR the bail must instead
    // transfer the JIT-advanced state IN PLACE into the pre-existing
    // live interpreter frame (`transfer_osr_exit_into_live_frame`) —
    // and that transfer routinely fails: the operand-stack values
    // live at an indy call site (its recipe/constant args) are exactly
    // the kind of value the single-pass backend's per-site stack-slot
    // classifier cannot always precisely re-type (see `FrameValue::
    // Unsupported`'s doc in jit/src/deopt.rs), which correctly rejects
    // the transfer rather than fabricate one. The OSR trigger then
    // falls back to its "safe reject" default — resuming interpretation
    // at the STALE pre-OSR back-edge pc/locals — which is only actually
    // safe when the bail precedes any committed loop iteration. An indy
    // trap reached *after* a hot loop that already ran to completion
    // inside the OSR'd continuation (e.g. a `System.out.println("..." +
    // n + ...)` immediately following the loop, "..." string-concat
    // compiling to `invokedynamic`) violates that precondition: the
    // loop's real side effects (already committed once, correctly, by
    // the OSR'd code) get silently RE-EXECUTED by the interpreter from
    // the stale resume state — e.g. an `ArrayList` ending up with extra
    // duplicate elements with no exception anywhere. See
    // jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md
    // for the full repro and trace. Like `has_athrow` above,
    // method-entry compilation (unaffected by this OSR-only bail path)
    // remains available, so do NOT bail-list here.
    //
    // RELAXED 2026-07-30 (tomcat known-issue 30): the blanket refusal
    // moved below, to after `indy_info` is resolved. A site that lowers
    // to the StringConcatFactory bridge emits a real call, not a trap,
    // so there is no imprecise resume for an OSR frame to take. Only
    // methods with an UNBRIDGED indy are still refused.
    // RBC.6b (dohead-residuals, 2026-07-18; LIFTED 2026-08-17) — this
    // door used to refuse **any** method with a non-empty exception
    // table, on the grounds that `compile_with_param_slots` below is
    // never handed one, so an OSR artifact carries no handler ranges
    // and a callee exception unwinding into that frame escapes a
    // `catch` that textually guards the call. That hazard was real and
    // was observed (a servlet's `try { resp.resetBuffer(); } catch
    // (IllegalStateException)` silently ceasing to catch, once the
    // blanket `ldc`-string OSR denial that had masked it was lifted).
    //
    // The blast radius was not deliberate. OSR is the ONLY door out of
    // the interpreter for a method invoked once — which is what a
    // `@Test` body, a `main`, and any one-shot driver is — so "a hot
    // loop with a try/catch in it", ordinary Java, ran interpreted for
    // its whole life. Measured: netty's two
    // `HttpHeaderValidationUtilTest` exhaustive loops at 19 242 and
    // 309 423 ns/iteration against HotSpot's 8.2 and 9.4.
    //
    // What replaces it is the method-entry path's own contract, staged
    // here for the first time (see the three `set_*_request` calls
    // immediately before `compile_with_param_slots` below):
    //
    //   1. `BackendRequest::precise_exception_frames(true)` makes every
    //      invoke inside a protected range publish a **reason-9**
    //      (`DeoptReason::PendingException`) frame keyed on the
    //      THROWING bci, not on the stale back-edge pc the live
    //      interpreter frame is parked at;
    //   2. `BackendRequest::protected_ranges` suppresses the sibling
    //      tail-call inside a `try` (which would tear this frame down
    //      and `JMP`, unwinding past the handler);
    //   3. `BackendRequest::exception_ranges` makes the handler entry edges
    //      visible to `find_bypassable_loop_headers`.
    //
    // and the ADMISSION rule that makes the stale-resume fallback
    // unreachable rather than merely unlikely: every throwing site
    // inside a protected range must publish such a frame. That is
    // exactly `first_unsupported_precise_frame_site`, the predicate
    // RBC.6 already uses on the method-entry path — asked here rather
    // than copied, so the two doors cannot drift. A method with an
    // `ldc`, an array access, an `athrow`, a `new` or an
    // `invokedynamic` inside a `try` is still refused, and named.
    //
    // The refusal has to be a COMPILE-time one. Deciding it at the exit
    // instead would leave `transfer_osr_exit_into_live_frame`'s
    // fail-closed reject as the only backstop, and for an exception
    // exit that reject is not merely slow: it resumes interpretation at
    // the STALE pre-OSR back-edge pc, re-running every iteration the
    // OSR'd code already committed (RBC.7's silent-corruption shape).
    //
    // `CRATONVM_JIT_OSR_EXC_TABLE=0` restores the blanket refusal, so
    // one binary can A/B the lift.
    // (`osr_exception_table` is read once, above RBC.6, which gates on
    // the same table.)
    if !osr_exception_table.is_empty() {
        // Name the ONE site that blocks the method, the way the
        // method-entry path's `rbc6-handler-reads-unsafe-local` bail
        // does. A bare "OSR denied" here is what cost this defect a
        // six-arm shape bisect to find in the first place.
        // x86-64 only: `first_unsupported_precise_frame_site` is
        // `#[cfg(target_arch = "x86_64")]`, because the precise-frame
        // publication it screens for is a property of that backend's
        // lowering. On another architecture there is no OSR at all (the
        // aarch64 backend publishes no `osr_pc_to_native`), so there is
        // nothing to deny and nothing to name.
        #[cfg(target_arch = "x86_64")]
        if let Some((pc, op)) = cratonvm_jit::first_unsupported_precise_frame_site(
            &code,
            code_len,
            &osr_exception_table,
        ) {
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr-DENY (osr-exc-site-unpublished pc={} opcode={:#04x}) {}.{}{}",
                    pc, op, class_name, method_name, method_descriptor
                );
            }
            // An OSR-door verdict, so an OSR denial (round 11 wave 8): it was
            // a bail-listing, which `compile_gate::admit` honours at EVERY
            // door, so a method whose first activation's loop reached this
            // line also lost its method-entry compile — which asks its own,
            // narrower question (`rbc6-handler-reads-unsafe-local` refuses only
            // when a handler reads an unsafe local) and would have compiled.
            // Once RBC.6 stopped refusing `athrow` + table outright, every
            // javac `synchronized` block with an unpublished site in it
            // reached here. The denial is remembered and expires with the
            // install epoch, like every other `mark_osr_denied`.
            shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
            return None;
        }
    }
    // 2026-07-10 BC-crypto session: OSR of `GOST3412_2015Engine.
    // init_gf256_mul_table` (a nested primitive-array allocation loop)
    // was observed to "resume with corrupt stack state for the next
    // newarray length", and a blanket per-method OSR deny for any
    // `newarray`-containing method was added as a workaround.
    //
    // perf/throughput-20260710: the deny is now DEFAULT-OFF. It was a
    // huge hammer — any hot loop in any method that allocates a
    // primitive array anywhere ran interpreted forever (BenchSuite
    // sieve250k: 3.2s → 177s, ~55x; every BC math/EC kernel under
    // JIT-allow lost OSR) — and the corruption does not reproduce on
    // the current tree (GOST3412Test 10/10 at -Xmx256m across both
    // getfield modes; an exact-shape nested-allocation repro is
    // checksum-identical to HotSpot under heap pressure; EC AllTests
    // passes under full JIT-allow). See `osr_newarray_allowed` for the
    // full evidence trail; `CRATONVM_OSR_NEWARRAY=0` restores the deny
    // for bisection.
    if scan.has_newarray && !crate::runtime::env_cache::osr_newarray_allowed() {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr-DENY (has_newarray, CRATONVM_OSR_NEWARRAY=0) {}.{}{}",
                class_name, method_name, method_descriptor
            );
        }
        shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
        return None;
    }

    // Resolve multianewarray
    let mut mna_info = Vec::new();
    if !scan.multianewarray_ops.is_empty() {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        for &(pc, cp_idx, _ndims) in &scan.multianewarray_ops {
            // A malformed CP entry is still a whole-compile refusal.
            let _ = class.constant_pool.get_class_name(cp_idx)?;
            mna_info.push((
                pc,
                crate::jit::pack_multianewarray_site(class_id.as_u32(), cp_idx),
            ));
        }
    }

    // Resolve typecheck
    let mut typecheck_info: Vec<(usize, *const u8, usize)> = Vec::new();
    let mut owned_jit_strings2: Vec<Box<str>> = Vec::new();
    if !scan.typecheck_ops.is_empty() {
        let cm_lock = shared.classes.class_manager.read();
        let class = cm_lock.get_class(class_id)?;
        for &(pc, cp_idx) in &scan.typecheck_ops {
            let cn = class.constant_pool.get_class_name(cp_idx)?;
            // THE THIRD DOOR, and the last one still handing the
            // runtime helper a bare name. The other two — the ordinary
            // tiering door in `jit::try_compile_inner` and the eager
            // first-call door in `interpreter.rs` — both resolve the
            // site's `CONSTANT_Class` through THIS class's own defining
            // loader and intern the name under that `ClassId`, so that
            // `jit_checkcast` can compare ids instead of re-resolving a
            // name against a `(ClassLoaderId, name)`-keyed dictionary,
            // and so the JIT can compare them INLINE.
            //
            // This door did neither: it boxed a per-compilation copy of
            // the name, which recorded no id and, being a fresh address
            // every compile, could not even share the helper's
            // `(ptr, len)` memo with the other two doors' copies of the
            // same site.
            //
            // Measured: `CcProbe2`, a 20-million-iteration loop whose
            // body is one `(Node) o` cast, reported
            // `checkcast inline sites: single-pass=0 optimizing=0
            // refused-no-target-id=2` and 63,989,000 membership walks —
            // the inline compare could not fire ANYWHERE in it. A loop
            // is exactly what reaches this door, so "the hot case" and
            // "the door with no target id" were the same set. Third
            // time this shape has been recorded (the thin native binds,
            // the `String` call-site intrinsics): a bind at one compile
            // door is not a bind.
            // The interpreter drives the loader for a type-check target, so
            // the target passes the loader-faithful rule the method-entry
            // doors' `new` resolver (which also answers their type-check
            // sites) applies: a cold site naming a JDK-global class the
            // referencing loader may define itself is interned under its
            // holder instead of baking java.base's id (interpreter round i1
            // wave 38, lane L2; `L2W38JdkNameColdTypeCheck`).
            // The entry's record first (JIT round 16, lane classsite).
            let target_id =
                jit_known_class_constant_at(shared, &cm_lock, cn, class_id, Some(cp_idx))
                    .map(|id| id.as_u32());
            // Round 9 wave 8 (`arraylist8`): the same finality verdict the
            // method-entry door asks through
            // `CompileRequest::class_is_final_resolver`, from the lock already
            // held, so the inline `instanceof` final-class miss fires at this
            // door too (a bind at one door is not a bind).
            let target_is_final = target_id
                .is_some_and(|id| jit_class_is_final_for_typecheck_in(shared, &cm_lock, id));
            // Not known through this class's loader yet: intern the site under
            // its holder, so the helper resolves it through that loader at run
            // time instead of by bare name.
            let (ptr, len) = match target_id {
                Some(_) => cratonvm_jit::intern_typecheck_target_with_finality(
                    cn,
                    target_id,
                    target_is_final,
                ),
                None => cratonvm_jit::intern_typecheck_unresolved_in(cn, class_id.as_u32(), cp_idx),
            };
            typecheck_info.push((pc, ptr, len));
        }
    }

    // Interpreter round i4 wave 93, lane L2 (`CRATONVM_JIT_DEFERRED_FIELD_SITES`,
    // default OFF; `i92-L2-deferred-field-sites-only-at-the-method-entry-door`):
    // a field site this door cannot resolve (its owner not loadable without
    // the holder's loader yet: a cold branch inside the loop, or an owner the
    // loader-faithful check refuses) compiles as a deferred site --
    // resolved, linked, initialised and accessed at its first execution by
    // `DirectHelperTable::field_site_cp` -- as the method-entry door's
    // `build_single_pass_tables` does since wave 92, instead of refusing the
    // OSR body (which kept a once-called method's loop interpreted for good).
    // `(pc, (holder class id, cp index, descriptor tag))`; handed to the
    // backend as `BackendRequest::deferred_field_sites` below.
    let osr_defer_fields = jit_deferred_field_sites_enabled();
    let mut osr_deferred_field_sites: Vec<(usize, (u32, u16, u8))> = Vec::new();
    let mut osr_defer_or_refuse = |pc: usize, cp_idx: u16, kind: &str| -> bool {
        let tag = if osr_defer_fields {
            jit_deferred_field_tag(shared, class_id, cp_idx)
        } else {
            None
        };
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr field site unresolved at pc={pc} {kind} cp={cp_idx}: {} {}.{}{}",
                if tag.is_some() { "deferred" } else { "refused" },
                class_name,
                method_name,
                method_descriptor
            );
        }
        match tag {
            Some(tag) => {
                osr_deferred_field_sites.push((pc, (class_id.as_u32(), cp_idx, tag)));
                true
            }
            None => false,
        }
    };

    // Resolve static fields
    let mut static_field_info: Vec<(usize, u32, usize, u8, bool)> = Vec::new();
    if !scan.static_field_ops.is_empty() {
        for &(pc, cp_idx) in &scan.static_field_ops {
            // Round 15 wave 4 (lane fieldowner): the loader-faithful owner
            // check with the resolution (`jit_resolve_field_site`).
            let Ok(field) = jit_resolve_field_site(shared, class_id, cp_idx) else {
                if osr_defer_or_refuse(pc, cp_idx, "static") {
                    continue;
                }
                return None;
            };
            if !jit_field_kind_agrees(&field, true) {
                deny_osr_for_field_site(shared, osr_key, pc, "static-kind");
                return None;
            }
            if code.get(pc) == Some(&0xb3)
                && final_field_put_illegal(shared, class_id, method_name, &field)
            {
                deny_osr_for_field_site(shared, osr_key, pc, "final-put");
                return None;
            }
            let cm_lock = shared.classes.class_manager.read();
            let class = cm_lock.get_class(class_id)?;
            let nat_idx = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::FieldReference {
                    name_and_type_index,
                    ..
                }) => *name_and_type_index,
                _ => return None,
            };
            let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
            let type_tag = *descriptor.as_bytes().first()?;
            if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
                drop(cm_lock);
                deny_osr_for_field_site(shared, osr_key, pc, "static-tag");
                return None;
            }
            static_field_info.push((
                pc,
                field.declaring_class_id.as_u32(),
                field.field_index,
                type_tag,
                field.is_volatile,
            ));
        }
    }

    // Resolve instance fields for getfield/putfield. Without this, the
    // OSR-compiled method had no `field_info` and every getfield/putfield
    // fell back to the `(pc, 0, b'I')` default in `compile_op_getfield` /
    // `compile_op_putfield` — silently routing every instance-field write
    // through `jit_putfield_int` into slot 0 with the wrong type tag.
    // Manifested in `org/eclipse/jdt/internal/compiler/util/HashtableOfInt`
    // (Eclipse JDT BatchCompiler boot): the OSR-compiled `rehash()`
    // stored the new `int[]` into `keyTable`'s slot tagged as
    // `Value::Int(low32_of_ptr)`, and the next `put()` then read back the
    // bogus tag and crashed at `arraylength` with
    //   `expected object reference, got int(N)`.
    // Mirrors the field_info collection in the first-call JIT compile
    // path (this file, ~line 1915) and `resolve_inline_site` (~line 13367).
    let mut field_info: Vec<(usize, usize, u8)> = Vec::new();
    // Compact reference-field layout: per-pc packed offset + ref-ness for
    // inline compact getfield/putfield in the OSR-recompiled method.
    let mut compact_field_info: Vec<(usize, u32, bool)> = Vec::new();
    // Volatile `getfield`/`putfield` pcs: the backend fences every volatile
    // store (`BackendRequest::volatile_field_pcs`, staged below). This door
    // calls the backend directly, so it has to collect them itself, as the
    // method-entry doors' `CompileRequest::cp_field_volatile_resolver` does
    // (`volatile-instance-fields-are-plain-accesses-in-compiled-code`).
    let mut osr_volatile_field_pcs: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    let compact_fields = cratonvm_types::compact_ref_fields_enabled();
    if !scan.field_ops.is_empty() {
        for &(pc, cp_idx) in &scan.field_ops {
            let Ok(field) = jit_resolve_field_site(shared, class_id, cp_idx) else {
                // Wave 93, lane L2: see `osr_deferred_field_sites` above.
                if osr_defer_or_refuse(pc, cp_idx, "instance") {
                    continue;
                }
                return None;
            };
            if !jit_field_kind_agrees(&field, false) {
                deny_osr_for_field_site(shared, osr_key, pc, "instance-kind");
                return None;
            }
            if jit_load_of_referent(shared, code.get(pc).copied(), &field) {
                deny_osr_for_field_site(shared, osr_key, pc, "reference-referent-load");
                return None;
            }
            if code.get(pc) == Some(&0xb5)
                && final_field_put_illegal(shared, class_id, method_name, &field)
            {
                deny_osr_for_field_site(shared, osr_key, pc, "final-put");
                return None;
            }
            let cm_lock = shared.classes.class_manager.read();
            let class = cm_lock.get_class(class_id)?;
            let nat_idx = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::FieldReference {
                    name_and_type_index,
                    ..
                }) => *name_and_type_index,
                _ => return None,
            };
            let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
            let type_tag = *descriptor.as_bytes().first()?;
            if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
                drop(cm_lock);
                deny_osr_for_field_site(shared, osr_key, pc, "instance-tag");
                return None;
            }
            field_info.push((pc, field.field_index, type_tag));
            if field.is_volatile {
                osr_volatile_field_pcs.insert(pc);
            }
            if compact_fields {
                let slot = cratonvm_types::compact_field_disp(
                    field.declaring_class_id.as_u32(),
                    field.field_index,
                );
                if let Some((c_off, c_ref)) = slot {
                    compact_field_info.push((pc, c_off as u32, c_ref));
                } else if false {
                    // ENGAGEMENT CENSUS. A `None` here is not a missing
                    // optimisation, it is a *helper call on every access*:
                    // this pc takes the guarded uniform arm, which keys on
                    // GC_FLAG_COMPACT and routes every COMPACT receiver —
                    // the default layout — to `jit_getfield`, whose
                    // `is_object_address` walk the profile shows at ~31% of
                    // a field-dense run. Naming the declaring class and
                    // index is what separates "no layout registered for
                    // this class" from "index outside the layout it has".
                    let declaring = declaring_class_name_for_diag(shared, field.declaring_class_id);
                    eprintln!(
                        "[compact-inline] MISS osr pc={pc} declaring={declaring} class_id={} field_index={} -> guarded-uniform arm (helper on every compact receiver)",
                        field.declaring_class_id.as_u32(),
                        field.field_index,
                    );
                }
            }
        }
    }

    // This method's own receiver profile, for the guarded String
    // call-site intrinsics resolved below. The method-entry door gets
    // one as a parameter (`try_compile_inner`'s `profile`) and screens
    // its guarded intrinsics against it; this door never had one, so
    // an OSR artifact would emit a `String` receiver guard at a
    // `CharSequence` site the interpreter had already recorded
    // thousands of non-`String` receivers at. Fetched once for the
    // whole scan, not per site.
    let osr_receiver_profile = {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: class_id.as_u32(),
            method_name: method_name.as_str().into(),
            descriptor: method_descriptor.as_str().into(),
        };
        shared.jit.profile_store.get_profile(&profile_key)
    };
    // Same `"<class>.<method>:<descriptor>"` key the deopt log and
    // `method_epochs` use; see `DespecRegistry::contains`.
    let osr_despec_key = format!("{class_name}.{method_name}:{method_descriptor}");

    // Resolve invokes — collect info under lock, then compile callees after release
    let mut invoke_info: Vec<(usize, *const crate::jit::JitInvokeInfo)> = Vec::new();
    let mut owned_jit_invoke_infos2: Vec<Box<crate::jit::JitInvokeInfo>> = Vec::new();
    let mut direct_calls2: Vec<(usize, crate::jit::JitDirectCall)> = Vec::new();
    // t1 wave 3, lane X (`CRATONVM_JIT_OSR_SELF_CALL_SERVICE`, default ON): the
    // `JitInvokeInfo` describing THIS method as the callee of the body's raw
    // static self-calls (`BackendRequest::self_call_invoke_info`), staged at
    // the first one by the self-call arm below. `0`: none.
    let mut osr_self_call_invoke_info: usize = 0;
    // Round 15 wave 3 (lane vh3): the thin `VarHandle` binds below record
    // `cratonvm_jit::VARHANDLE_THIN_BIND_DEPENDENCY` here; it joins the
    // artifact's `inlined_methods`, so the VM's first exact mint withdraws it.
    let mut osr_vh_thin_bind_deps: Vec<(String, String, String)> = Vec::new();
    // Round 15 wave 6 (lane vh8; VH7-1): the judged `VarHandle` binds' site
    // records (`cratonvm_jit::VarHandleJudgedSite`); the artifact owns them.
    let mut osr_vh_judged_sites: Vec<Box<cratonvm_jit::VarHandleJudgedSite>> = Vec::new();
    // FIX (jit-osr-loop-direct-call-retired-code-segv): the entry
    // addresses of COMPILED callees baked into this body as raw
    // machine-code CALLs. `JitCache::prepare_for_publication` turns
    // these into `_direct_callee_roots` — strong `Arc`s that keep each
    // callee's `ExecutableBuffer` mapped for as long as this artifact
    // can run. The method-entry tier has always populated
    // `_direct_callee_entries` (jit/src/lib.rs); this OSR tier never
    // did, so its baked CALLs were rooted by NOTHING: the first
    // background tier-up `put` for such a callee dropped the artifact
    // this body calls, `ExecutableBuffer::drop` unmapped it, and the
    // next OSR entry jumped into freed memory (SIGSEGV with
    // `pc == addr` at the callee's page-aligned entry — ~1-2 % of runs
    // idle, ~20 % under load, on the `JitOsrLoopProgress` fixture).
    //
    // Only real compiled-artifact entries belong here: the intrinsic /
    // helper direct calls emitted above (`Math.sqrt`,
    // `Integer.valueOf`, `Integer.intValue`, the `HashMap` fast paths)
    // are Rust function addresses with no owning artifact, and
    // `resolve_jit_entry_owner` would fail on them — inflating the
    // unrooted-callee counter and, under
    // `CRATONVM_JIT_STRICT_CALLEE_ROOTS`, refusing publication.
    //
    // FIX (jdk-only-string-indexof-int-int-miscompiles-in-the-single-pass-tier):
    // `(entry, artifact id AT BAKE TIME)`, not a bare entry. Keeping the
    // callee mapped is only half of what publication has to check. The other
    // half is IDENTITY: `prepare_for_publication` compares this recorded id
    // against whoever owns the address when the body is finally published, and
    // refuses when they differ, because an executable buffer freed after emit
    // is handed straight back out to the next compile. With no id recorded,
    // `expected_of` answered `None` for every OSR-baked call and that check
    // degraded to the liveness-only test a recycled buffer passes: the body
    // published a raw CALL into a method it never resolved, entered with the
    // ABI of the one it did. `String.indexOf(II)I` calling
    // `StringLatin1.indexOf([BIII)I` context-free and landing in a
    // context-taking tenant is how it was caught.
    let mut osr_direct_callee_entries: Vec<(usize, u64)> = Vec::new();
    let mut mic_slots2: Vec<(usize, *const crate::jit::JitMICSlot)> = Vec::new();
    let mut owned_mic_slots2: Vec<Box<crate::jit::JitMICSlot>> = Vec::new();
    let mut pic_slots2: Vec<(usize, *const crate::jit::JitPICSlot)> = Vec::new();
    let mut owned_pic_slots2: Vec<Box<crate::jit::JitPICSlot>> = Vec::new();
    // Pending statically-bound callee compilations:
    // `(pc, class, method, desc, num_jit_args, invoke_kind)`.
    //
    // `param_count` is the JLS argument count, receiver EXCLUDED, for
    // both kinds. The two consumers below disagree about the receiver
    // and each must be fed its own convention:
    //
    //   * `JitDirectCall.num_params` wants it receiver-EXCLUDED — the
    //     codegen's instance arm computes `let n = callee_params + 1`
    //     itself ("the JLS argument count is `callee_params`, and total
    //     operands popped is `callee_params + 1`");
    //   * `JitInvokeInfo.num_jit_args` wants it receiver-INCLUDED.
    //
    // Passing the receiver-included count to BOTH made the emitter pop
    // three operands off a two-operand stack for `F.<init>(I)V`, so
    // `compile_with_param_slots` refused the method — and an OSR
    // refusal is silent: the loop just runs interpreted forever while
    // `OSR-recompile reason=no-cached-artifact` repeats. Measured 5x
    // SLOWER, with `disp_calls` at 0, which is what the intended fix
    // also looks like.
    //
    // Kind 1 was admitted 2026-08-13. It had never been: this door
    // eagerly compiled and direct-bound `invokestatic` callees only, so
    // in an OSR'd loop — which is what a hot loop always is — every
    // `new X(...)` paid a full `jit_invoke_dispatch` round trip for its
    // constructor, forever, while the identical body reached through
    // `invokestatic` or `invokevirtual` was bound. Measured on the same
    // loop, same body: 26.7 ns (static, bound), 29.1 ns (virtual, bound
    // via the MIC), **317.9 ns** (`new Holder(i)`, dispatched — the only
    // one of the three producing dispatch-trace entries at all).
    //
    // The other two compile doors already bind kind 1: the single-pass
    // ladder on `matches!(invoke_kind, 1 | 3)`, and the IR ladder since
    // its `!is_ctor` term was removed. This is the third.
    let mut pending_callee_compiles: Vec<(usize, String, String, String, usize, u8)> = Vec::new();
    // Round 16 wave 13 (lane mega): the dynamically dispatched sites
    // `(pc, class, method, desc, invoke_kind)` the guarded OSR-door pass may
    // splice (`cratonvm_jit::plan_osr_door_guarded_splices`). Filled only under
    // `CRATONVM_JIT_OSR_SPLICE_GUARDED=1` (and not under
    // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=0`, the PGO-02 opt-out).
    let osr_guarded_splices = cratonvm_jit::osr_door_guarded_splices_enabled()
        && crate::runtime::env_cache::jit_guarded_virtual_inline();
    let mut osr_guarded_candidates: Vec<(usize, String, String, String, u8)> = Vec::new();
    // Trivial-ctor elision (OSR tier) — same deferred mechanism as the
    // `execute` first-call path: record `invokespecial …<init>()V` sites
    // here, resolve their target via `load_class_concurrent` after the
    // lock drops, and emit elidable ones as `Object.<init>` so codegen
    // drops the per-object dispatch. See `execute` for the rationale.
    let ctor_direct_call_off = crate::runtime::env_cache::ctor_direct_call_disabled();
    let osr_ctor_bind_off = crate::runtime::env_cache::osr_ctor_bind_disabled();
    let mut pending_ctor_sites: Vec<(usize, String, usize)> = Vec::new();
    // THIRD COMPILE DOOR, 2026-08-13. `java/lang/String`'s call-site
    // intrinsics (`length`/`isEmpty`/`charAt`/`hashCode`/`equals`/
    // `compareTo`/`indexOf`) were bound only in `jit::try_compile`'s
    // ladder, and this door passed `string_layout: None` under the
    // comment "String intrinsics land in a later wave". The wave never
    // came, so in a hot loop — the one place they matter, and the one
    // place compiled HERE — every one of them was inert: measured on
    // this branch before the fix, `String.charAt(i)` in a 20M-iteration
    // loop cost **408 ns/call** (HotSpot: 0.6 ns), because the site ran
    // the real `charAt` → `isLatin1` → `StringLatin1.charAt` →
    // `String.checkIndex` → `Preconditions.checkIndex` chain instead of
    // the inline decode. `String.length()` likewise cost 28 ns.
    //
    // This is verbatim the lesson the `Thread.currentThread()` bind
    // above records ("binding it in all THREE compile doors is the
    // whole lesson of that document") — and, like it, no timing could
    // have found it: a resolver, a codegen ladder and two green unit
    // suites all agree the intrinsic exists. `CRATONVM_DBG_INTRINSIC=1`
    // (jit/src/lib.rs) is the lever that names which door produced a
    // body, so the next one of these is a one-run question.
    //
    // Resolved BEFORE the `class_manager` read lock below: this helper
    // takes that same lock, and a recursive read on a `parking_lot`
    // RwLock can deadlock against a queued writer.
    let osr_string_layout = super::dispatch_static::resolve_string_field_layout(shared);
    // -- The String-intrinsic pin, ASKED at this door -- D1, 2026-09-01
    //
    // Measured on the built branch, ONE binary, two probes:
    //
    //     CharAtCostCurve   JIT String-intrinsic pin: fired=2   (method entry)
    //     CharAtWarmShape   JIT String-intrinsic pin: fired=0   (OSR)
    //
    // `CharAtWarmShape`'s entire body is `charAt`. Its `fired=0` was
    // never a method that FAILED the pin's test -- it was a method the
    // pin was never shown. The pin was a term of `try_compile_inner`'s
    // eligibility conjunction, i.e. of `CompileDoor::MethodEntry` and
    // of nothing else, and this door reaches
    // `x64::compile_with_param_slots` directly. A zero from a one-door
    // counter is indistinguishable from "there was nothing to pin",
    // and that is what let the five hypotheses in
    // `string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901`
    // each be refuted without converging: every one of them varied the
    // METHOD, and the discriminator was the DOOR.
    //
    // # The answer is INERT at this door today -- stated, not implied
    //
    // The pin means "keep this method on the backend that HAS the
    // inline `charAt` decode", i.e. do not promote it to the optimizing
    // tier. This door has no promotion to refuse. `compile_osr_artifact`
    // reaches `x64::compile_with_param_slots` -- the single-pass backend
    // -- unconditionally; the only production call of
    // `ir_lower::lower_inner` in the tree is inside `try_compile_inner`,
    // and nothing under `vm/**` names `ir_lower` at all. So "do not tier
    // this up" is already true here BY TOPOLOGY, and there is no machine
    // code this ask can change today. The ~3x that LIFTING the pin
    // bought at the method-entry door is not available here, because
    // there is nothing here to refuse -- do not read this as closing a
    // live hole.
    //
    // What is not inert is the ASKING. `string_pin_asked(Osr)` and
    // `string_pin_declined(Osr)` now say how much of the population the
    // pin governs is compiled through this door, and
    // `string_pin_not_asked(Osr)` stops being this door's whole row --
    // the one number that would have named the defect above in a single
    // run. This is a COUNTER, deliberately, and not a fix.
    //
    // It stops being inert if either half of the topology moves: an OSR
    // route to the optimizing tier (then this answer must GATE it), or
    // an IR String-intrinsic emitter (which retires the pin instead).
    // The `if` below is the tripwire for the first, and is silent today.
    //
    // What this does NOT claim: a method OSR-compiled here can still be
    // promoted later through `try_jit_upgrade_with_gate`, which goes
    // through `try_compile_with_invokespecial_resolver` -- the
    // method-entry door, which DOES ask the pin. Nothing on that route
    // changed.
    //
    // COST: once per OSR compile, never per back-edge. The back-edge
    // counter reaches a cached artifact; `compile_osr_artifact` runs
    // once per (method, entry_pc) compile, and this sits on that path,
    // not on the loop. The resolver takes the `class_manager` read lock
    // per site -- the same shape as `c_invoke_resolver` in this file --
    // and `string_intrinsic_pin_verdict` calls it only for
    // `invokevirtual`/`invokeinterface` sites, stops at the first
    // String-family receiver, and asks nothing at all for a method with
    // no `0xb6`/`0xb9` site. No lock is held at this point:
    // `resolve_string_field_layout` above took and released its own,
    // and the invoke loop below takes its own AFTER this -- never
    // nested, which is the recursive-read deadlock the comment above
    // warns about.
    let osr_pin_invoke_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        let (mn, desc) = class.constant_pool.get_name_and_type(nat_idx)?;
        Some((target_class.to_string(), mn.to_string(), desc.to_string()))
    };
    // The SAME `osr_string_layout` the invoke loop below screens with
    // and that `compile_with_param_slots` is handed. Asking the pin
    // about a different layout than the one this compile uses would
    // make the census describe a compile that did not happen.
    let osr_string_pin_declines = admission.string_intrinsic_pin_declines(
        &scan.invoke_ops,
        Some(&osr_pin_invoke_resolver),
        osr_string_layout,
    );
    if osr_string_pin_declines && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr String-intrinsic pin declines the optimizing tier for \
                     {}.{}{} -- INERT at this door, which is single-pass only. If this door ever \
                     gains a route to the optimizing tier, THIS is the answer that must gate it.",
            class_name, method_name, method_descriptor,
        );
    }
    if !scan.invoke_ops.is_empty() {
        let cm_lock = shared.classes.class_manager.read();
        let class = cm_lock.get_class(class_id)?;
        for &(pc, cp_idx, opcode) in &scan.invoke_ops {
            let (ref_class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index),
                Some(ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index),
                // t1 lane O: refuse, never skip. A skipped `invokestatic`
                // site has neither a row nor a `JitInvokeInfo`, which the
                // single-pass emitter reads as a SELF-recursive call and
                // lowers to `CALL <this body>` (the SBR4 `binarySort` shape;
                // `docs/feature-designs/jit-ir-optimizations.md`).
                // t1 wave 4, lane X2: named under `CRATONVM_DBG_JITC` (the
                // refusal census greps it) and the loop denied, so a back
                // edge does not re-ask a compile that must refuse again.
                _ => {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] osr-refused non-method-invoke-cp-entry \
                             {class_name}.{method_name}{method_descriptor} pc={pc} cp={cp_idx}"
                        );
                    }
                    shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
                    return None;
                }
            };
            let target_class = class.constant_pool.get_class_name(ref_class_idx)?;
            let (mn, desc) = class.constant_pool.get_name_and_type(nat_idx)?;
            let param_count = crate::jit::count_param_slots(desc);
            let mut invoke_kind = match opcode {
                0xb6 => 0u8,
                0xb7 => 1,
                0xb9 => 2,
                0xb8 => 3,
                _ => continue,
            };
            // Interpreter round i5 wave 100, lane L2: a site the interpreter
            // answers with a linkage error on every execution
            // (`invoke_site_linkage_error`, which since wave 100 includes a
            // reference that resolves to nothing) keeps its loop interpreted,
            // as the method-entry door keeps its method (`CpResolvers::invoke`).
            if invoke_site_linkage_error(
                shared, &cm_lock, class_id, cp_idx, target_class, mn, desc, opcode,
            ) {
                let why = "invoke-site-linkage-error";
                dbg_special_site_refusal(
                    "osr",
                    class_name,
                    method_name,
                    method_descriptor,
                    (target_class, mn, desc),
                    why,
                );
                drop(cm_lock);
                jit_verdicts(shared).record_compile_refusal(
                    class_id,
                    class_name,
                    method_name,
                    method_descriptor,
                    why,
                );
                shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
                return None;
            }
            // JVMS 5.4.6 — an `invokevirtual` naming a PRIVATE method is
            // not a dispatch site. The OSR door reaches
            // `x64::compile_with_param_slots` directly, so like the
            // eager first-call door it has to make the reclassification
            // itself rather than inheriting `try_compile`'s. See
            // `invoke::invokevirtual_site_targets_private`.
            //
            // An `invokeinterface` (kind 2) whose resolved method is a PRIVATE
            // interface method is pinned the same way (JVMS §6.5 selection
            // step 1): it selects that method, never the receiver's public
            // namesake.
            if matches!(invoke_kind, 0 | 2)
                && super::invoke::invokevirtual_site_targets_private(
                    &cm_lock,
                    class_id,
                    target_class,
                    mn,
                    desc,
                )
            {
                // `--real-jdk`: a pinned private INTERFACE method has no
                // compiled receiver check (`CpResolvers::invoke`); the loop
                // is denied and the interpreter raises the ICCE (interpreter
                // round i1 wave 37, lane L2).
                // Interpreter round i6 wave 132, lane L6
                // (`CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN`, default OFF): not
                // when this site's receiver is the method's unstored `this`.
                if opcode == 0xb9
                    && shared.classes.selection_checks
                    && !private_iface_this_pin_proven(
                        shared,
                        &cm_lock,
                        class_id,
                        method_name,
                        method_descriptor,
                        None,
                        target_class,
                        cp_idx,
                        Some(pc),
                        desc,
                    )
                {
                    let why = "invokeinterface-private-receiver-unchecked";
                    dbg_special_site_refusal(
                        "osr",
                        class_name,
                        method_name,
                        method_descriptor,
                        (target_class, mn, desc),
                        why,
                    );
                    drop(cm_lock);
                    jit_verdicts(shared).record_compile_refusal(
                        class_id,
                        class_name,
                        method_name,
                        method_descriptor,
                        why,
                    );
                    shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
                    return None;
                }
                if invoke_kind == 0 {
                    cratonvm_jit::PRIVATE_INVOKEVIRTUAL_PINNED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                invoke_kind = 1;
            }
            let invoke_kind = invoke_kind;
            // An `invokespecial` this door would bind by its constant-pool
            // class (below: `pending_callee_compiles` / the dispatch info
            // both carry `target_class`, with no §6.5 owner substitution)
            // while the interpreter selects another body or raises an error
            // (`special_site_bind_refusal`; interpreter round i1 wave 37,
            // lane L2). The loop is denied, as an unbridged indy is below:
            // the answer is a function of the hierarchy, and the interpreter
            // runs the site right. Asked for `invokespecial` sites only, at
            // compile time.
            // Interpreter round i6 wave 119, lane L2
            // (`CRATONVM_JIT_INVOKE_SITE_CLASSIFIER`, default ON since the i6 wave-126 flip, `=0` off): with the
            // classifier, a selection start other than the constant-pool
            // class is BOUND (its name replaces `target_class` for the rest
            // of this site: the dispatch info and the callee compile), not
            // denied. The info's class-id hint stays the caller's: the owner
            // is the caller's ancestor, so its name resolves to it through
            // the caller's loader.
            let mut substituted_owner: Option<Arc<str>> = None;
            if opcode == 0xb7 && mn != "<init>" {
                let is_iface = matches!(
                    class.constant_pool.get(cp_idx),
                    Some(ConstantPoolEntry::InterfaceMethodReference { .. })
                );
                let refusal = if invoke_site_classifier_enabled() {
                    match classify_invoke_site(
                        shared, &cm_lock, class_id, target_class, is_iface, mn, desc,
                    ) {
                        SiteBinding::ConstantPoolClass => None,
                        // Interpreter round i7 wave 135, lane L2
                        // (`interpreter-i135-L2-osr-classifier-owner-is-bound-by-name-FIXED-20261021`):
                        // this door carries the owner by NAME only (the
                        // dispatch info's hint is the caller, and the callee
                        // compile resolves the name through the caller's
                        // loader), unlike the splice and method-entry doors,
                        // which carry its id. The "ancestor resolves by name"
                        // premise above fails for an owner a child-first
                        // loader shadows (the caller's loader defines its own
                        // copy of an ancestor's name), so the owner is bound
                        // only when the caller's loader names exactly it;
                        // otherwise the loop is denied, as with the
                        // classifier off.
                        SiteBinding::Owner(owner, owner_id)
                            if class_resolved_without_loading(shared, &cm_lock, class_id, &owner)
                                != Some(owner_id) =>
                        {
                            Some("invokespecial-selection-start")
                        }
                        SiteBinding::Owner(owner, _) => {
                            if crate::runtime::env_cache::dbg_jitc() {
                                eprintln!(
                                    "[cratonvm-jitc] invokespecial-selection SUBSTITUTED osr \
                                     {class_name}.{method_name}{method_descriptor}: \
                                     {target_class}.{mn}{desc} binds {owner}"
                                );
                            }
                            substituted_owner = Some(owner);
                            None
                        }
                        SiteBinding::Refused(why) => Some(why),
                    }
                } else {
                    special_site_bind_refusal(
                        shared,
                        &cm_lock,
                        class_id,
                        target_class,
                        is_iface,
                        mn,
                        desc,
                        true,
                    )
                };
                if let Some(why) = refusal {
                    dbg_special_site_refusal(
                        "osr",
                        class_name,
                        method_name,
                        method_descriptor,
                        (target_class, mn, desc),
                        why,
                    );
                    drop(cm_lock);
                    jit_verdicts(shared).record_compile_refusal(
                        class_id,
                        class_name,
                        method_name,
                        method_descriptor,
                        why,
                    );
                    shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
                    return None;
                }
            }
            let target_class: &str = substituted_owner.as_deref().unwrap_or(target_class);
            let is_recursive_call = target_class == class_name.as_str()
                && mn == method_name.as_str()
                && desc == method_descriptor.as_str();
            // The tail-shaped static self-call (`invokestatic_self_call_uses_tail_jump`)
            // and every other one take the same route here: the raw
            // self-recursive arm.
            //
            // BUG-1 companion (OSR/callee tier parity with
            // `jit::try_compile`'s routing): NON-tail static
            // self-recursive sites stay on the raw direct-CALL path —
            // the backend emits the cheap `self_call_stack_guard` call
            // (always wired via `build_helpers`) instead of the full
            // `jit_invoke_dispatch` round trip. `scan.needs_heap` is
            // already true (every invoke op sets it), so the guard's
            // vm_ptr frame slot exists.
            //
            // t1 wave 3, lane X: the arm registers the positive self-call
            // marker the emitter requires under
            // `CRATONVM_JIT_STATIC_SELF_CALL_MARKER`
            // (`cratonvm_jit::static_self_call_marker`), as the method-entry
            // door registers it.
            if invoke_kind == 3 && is_recursive_call {
                direct_calls2.push((
                    pc,
                    cratonvm_jit::static_self_call_marker(
                        param_count,
                        crate::jit::return_type(desc),
                    ),
                ));
                // t1 wave 3, lane X: the callee-deopt service's info for the
                // raw self-call, which this door never staged (the method-entry
                // door's `stage_self_call_invoke_info`; without it an inner
                // activation's throw skipped that activation's own handler,
                // `regression-suite/probes/R10SelfRecCatch.java`'s shape). On
                // its own channel, NOT in `invoke_info`: a row there would
                // route the site to the dispatch helper.
                if osr_self_call_invoke_info == 0 {
                    let mut staged: Vec<(usize, *const crate::jit::JitInvokeInfo)> = Vec::new();
                    push_osr_invoke_info(
                        &mut owned_jit_strings2,
                        &mut owned_jit_invoke_infos2,
                        &mut staged,
                        pc,
                        (
                            class_name.as_str().into(),
                            method_name.as_str().into(),
                            method_descriptor.as_str().into(),
                        ),
                        // Receiver-INCLUDED; a static callee has none.
                        param_count,
                        crate::jit::return_type(desc),
                        3,
                        class_id.as_u32(),
                    );
                    // Cast: the arena address, as `BackendRequest` carries it.
                    osr_self_call_invoke_info = staged.first().map_or(0, |&(_, p)| p as usize);
                }
                continue;
            }

            // `java/lang/String` / `java/lang/CharSequence` call-site
            // intrinsics — see `osr_string_layout` above for why this
            // arm exists and what its absence cost. The matcher and the
            // codegen must agree about the layout or the codegen would
            // fall through and `CALL` an intrinsic sentinel address, so
            // the SAME `osr_string_layout` value is handed to
            // `compile_with_param_slots` below (the `string_layout`
            // argument) — exactly the invariant
            // `try_compile_inner` documents for its own copy.
            //
            // `guard_class_id` comes from the resolver: 0 for a
            // `java/lang/String` site (final class, monomorphic), the
            // real String class id for a `java/lang/CharSequence` site
            // so the codegen guards the receiver and deopts for any
            // non-String `CharSequence`.
            if matches!(invoke_kind, 0 | 2) {
                if let Some((entry, num_params, ret, guard_class_id)) =
                    cratonvm_jit::try_resolve_string_intrinsic(
                        target_class,
                        mn,
                        desc,
                        osr_string_layout,
                    )
                    // The same screen the method-entry door applies: a
                    // `CharSequence` site's `String` class-id guard
                    // DEOPTS on a miss, so a bci the profile says is
                    // hardly ever a `String` must not get one. See
                    // `cratonvm_jit::receiver_profile_rejects_guard`.
                    .filter(|&(entry, _, _, guard_class_id)| {
                        if guard_class_id == 0 || !cratonvm_jit::receiver_despec_enabled() {
                            return true;
                        }
                        // A declining intrinsic's guard miss is a CALL,
                        // not a deopt, so this screen has nothing to
                        // protect against — see
                        // `string_intrinsic_declines_to_a_call`.
                        if cratonvm_jit::string_intrinsic_declines_to_a_call(entry) {
                            return true;
                        }
                        let supported = cratonvm_jit::receiver_profile_supports_guard(
                            osr_receiver_profile.as_ref(),
                            pc,
                            guard_class_id,
                        ) || cratonvm_jit::charseq_blind_guard_enabled();
                        let by_profile = !supported
                            || cratonvm_jit::receiver_profile_rejects_guard(
                                osr_receiver_profile.as_ref(),
                                pc,
                                guard_class_id,
                            );
                        // M4 — ask about THIS speculation, not about the
                        // bytecode index. The guard this decides on takes the
                        // `ReceiverTypeChanged` deopt edge
                        // (`x64/op_invoke.rs`), and a protected invoke also
                        // publishes a `PendingException` frame at the SAME bci.
                        // Under the old per-bci key, four traps on the
                        // exceptional frame withdrew this guard too — a
                        // speculation that had never failed, on evidence about
                        // a different one. A wildcard entry
                        // (`SPECULATION_ID_ANY`, recorded when the VM could not
                        // recover which point trapped) still matches, so the
                        // conservative case is unchanged.
                        let by_despec = shared.jit.despec_registry.contains_speculation(
                            &osr_despec_key,
                            pc as u32,
                            cratonvm_jit::deopt::speculation_id(
                                pc as u32,
                                cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged,
                            ),
                        );
                        if by_profile {
                            cratonvm_jit::metrics::note_receiver_despec(
                                cratonvm_jit::metrics::RECEIVER_DESPEC_PROFILE_DECLINED,
                            );
                        }
                        if by_despec {
                            cratonvm_jit::metrics::note_receiver_despec(
                                cratonvm_jit::metrics::RECEIVER_DESPEC_DECLINED,
                            );
                        }
                        !(by_profile || by_despec)
                    })
                {
                    // A declining intrinsic needs its own
                    // `JitInvokeInfo` at THIS door too: the emitted
                    // fast path declines into this exact dispatch, and
                    // a site the resolver registers as an intrinsic
                    // never reaches the generic `invoke_info.push`
                    // below. Fixing only the method-entry door left
                    // every once-invoked hot loop — which is every
                    // method this door exists for — failing to compile
                    // and running interpreted.
                    if cratonvm_jit::string_intrinsic_declines_to_a_call(entry) {
                        push_osr_invoke_info(
                            &mut owned_jit_strings2,
                            &mut owned_jit_invoke_infos2,
                            &mut invoke_info,
                            pc,
                            (
                                target_class.to_string().into_boxed_str(),
                                mn.to_string().into_boxed_str(),
                                desc.to_string().into_boxed_str(),
                            ),
                            // Receiver-INCLUDED, unlike
                            // `JitDirectCall.num_params`.
                            num_params + 1,
                            ret,
                            invoke_kind,
                            class_id.as_u32(),
                        );
                    }
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            guard_class_id,
                        },
                    ));
                    continue;
                }
            }

            // The layout-independent STATIC intrinsic families
            // (`Math`/`StrictMath`, `Integer`/`Long` bit ops, …). This
            // door previously recognised exactly one of them by hand —
            // `Math.sqrt`, immediately below — so an OSR body paid full
            // dispatch for `Math.abs`, `Math.min`/`max`,
            // `Integer.bitCount`, `Long.numberOfTrailingZeros` and the
            // rest, all of which lower to one or two instructions.
            //
            // Restricted to `invokestatic`: every member of those
            // families is static, so `guard_class_id: 0` (no receiver
            // guard) is exactly right, and the restriction also keeps
            // the CRC32/CRC32C members — the only ones in
            // `try_resolve_intrinsic` whose inline code is sound ONLY
            // behind a resolved receiver class-id guard, which this
            // door has no resolver for — off this path entirely.
            if invoke_kind == 3 {
                if let Some((entry, num_params, ret)) =
                    cratonvm_jit::try_resolve_intrinsic(target_class, mn, desc)
                {
                    if !cratonvm_jit::JitIntrinsic::from_entry(entry)
                        .is_some_and(|i| i.is_crc32_family())
                    {
                        // SBR4 osr: `System.arraycopy` needs its own
                        // `JitInvokeInfo` at THIS door, exactly as the
                        // method-entry scan registers one
                        // (`jit/src/lib.rs`, the `ArraycopyPrimitive` arm).
                        // The emitted fast path bails on every guard it
                        // cannot prove (a reference-element array, as in
                        // `TimSort.binarySort`, ALWAYS bails) into that
                        // dispatch. Without the info the bail is a
                        // `BoundsCheck` deopt trap, and once the per-bci
                        // de-spec registry withdraws the intrinsic after
                        // four traps, a recompile finds neither an
                        // intrinsic nor an info and the emitter's last arm
                        // lowers the site as the SELF-RECURSIVE call: the
                        // OSR body then called itself with the five
                        // arraycopy operands in the method's own parameter
                        // slots (`c` = the length `n`, so `rsi=5` where the
                        // `Comparator` belonged: wrong sorts, NPE on a null
                        // `c`, SIGSEGV at `[rsi+6]`). An intrinsic
                        // registered in one door is inert in the others.
                        if entry == cratonvm_jit::JitIntrinsic::ArraycopyPrimitive.as_entry() {
                            push_osr_invoke_info(
                                &mut owned_jit_strings2,
                                &mut owned_jit_invoke_infos2,
                                &mut invoke_info,
                                pc,
                                (
                                    target_class.to_string().into_boxed_str(),
                                    mn.to_string().into_boxed_str(),
                                    desc.to_string().into_boxed_str(),
                                ),
                                // A static call: no receiver, so the
                                // argument count is `num_params` (5).
                                num_params,
                                ret,
                                invoke_kind,
                                class_id.as_u32(),
                            );
                        }
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: false,
                                num_params,
                                return_type: ret,
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
            }

            // ===== INTRINSIC REGION BEGIN: FFM_SEGMENT =====
            // `MemorySegment.getAtIndex`/`setAtIndex` on the OSR /
            // direct-bind door.
            //
            // Registered HERE as well as in the single-pass scan
            // because an intrinsic registered in one door is inert in
            // the others — the lesson this file already records for the
            // `Thread.currentThread` and String binds. An engagement
            // counter is what caught it: `publishes=4000001
            // fast_hits=0` said the native was publishing verdicts on
            // every element and compiled code was never asking, because
            // the only door that ran was this one.
            //
            // `invoke_kind` 0/2 (virtual/interface), unlike the
            // `try_resolve_intrinsic` block above which is
            // `invokestatic`-only: these accessors are interface calls.
            // No receiver class-id guard is needed or wanted — the
            // helper asks the native for a verdict rather than
            // speculating on a receiver class, and DECLINES into this
            // site's ordinary dispatch for anything it does not
            // recognise.
            if matches!(invoke_kind, 0 | 2)
                && target_class == "java/lang/foreign/MemorySegment"
                && cratonvm_jit::ffm_element_accessor_name(mn)
                && cratonvm_jit::ffm_kind_for_descriptor(desc).is_some()
            {
                let entry = if cratonvm_jit::ffm_element_accessor_is_get(mn) {
                    cratonvm_jit::JitIntrinsic::FfmSegmentGetAtIndex.as_entry()
                } else {
                    cratonvm_jit::JitIntrinsic::FfmSegmentSetAtIndex.as_entry()
                };
                if false {
                    eprintln!(
                        "[ffm] REGISTERED(bridge) {target_class}.{mn}{desc} @pc={pc} kind={invoke_kind}"
                    );
                }
                // The site's own dispatch info. REQUIRED, not
                // incidental: the emitted fast path DECLINES into this
                // exact dispatch for any carrier it does not recognise,
                // and the emitter also reads the descriptor back off it
                // to recover the element kind. Without it the emitter
                // refuses the site (`info=false`) and nothing is gained.
                push_osr_invoke_info(
                    &mut owned_jit_strings2,
                    &mut owned_jit_invoke_infos2,
                    &mut invoke_info,
                    pc,
                    (
                        target_class.to_string().into_boxed_str(),
                        mn.to_string().into_boxed_str(),
                        desc.to_string().into_boxed_str(),
                    ),
                    // Receiver-INCLUDED, unlike `JitDirectCall.num_params`.
                    param_count + 1,
                    cratonvm_jit::return_type(desc),
                    invoke_kind,
                    class_id.as_u32(),
                );
                direct_calls2.push((
                    pc,
                    crate::jit::JitDirectCall {
                        entry,
                        needs_context: false,
                        num_params: param_count,
                        return_type: cratonvm_jit::return_type(desc),
                        guard_class_id: 0,
                    },
                ));
                continue;
            }
            // ===== INTRINSIC REGION END: FFM_SEGMENT =====

            // Math.sqrt intrinsic: inline as SQRTSD (no dispatch overhead)
            if invoke_kind == 3
                && target_class == "java/lang/Math"
                && mn == "sqrt"
                && desc == "(D)D"
            {
                direct_calls2.push((
                    pc,
                    crate::jit::JitDirectCall {
                        entry: crate::jit::MATH_SQRT_INTRINSIC,
                        needs_context: false,
                        num_params: 1,
                        return_type: b'D',
                        guard_class_id: 0,
                    },
                ));
                continue;
            }
            // `Thread.currentThread()` thin direct call — the JIT half
            // of the funnel bypass the interpreter already has
            // (`InterpIntrinsic::ThreadCurrentThread`). A statically
            // bound NATIVE callee, so exactly like `Integer.valueOf`
            // below the eager callee compile can never succeed, and
            // every call paid `jit_invoke_dispatch` →
            // `vm_exec::invoke_or_native`, which re-resolves the callee
            // BY NAME and only then enters the funnel. The JDK calls it
            // twice per uncontended `ReentrantLock` lock/unlock pair.
            // See `jit::helpers::jit_thread_current_thread_direct` and
            // native-call-funnel-per-call-floor-item2-20260805.md.
            //
            // Binding it in all THREE compile doors is the whole lesson
            // of that document. A version bound only in
            // `jit::try_compile`'s two ladders was completely inert:
            // `CRATONVM_INTRINSIC_STATS=1` reported 0 bypasses across a
            // loop that made 8,000,000 calls — and 0 invokestatic sites
            // even EXAMINED by either of those ladders — because a hot
            // loop is compiled HERE, by the OSR door, which reaches
            // `x64::compile_with_param_slots` directly and carries its
            // own copy of the ladder. No timing could have shown that:
            // a 0 % change and a fast path that was never installed
            // produce the same table.
            if invoke_kind == 3
                && target_class == "java/lang/Thread"
                && mn == "currentThread"
                && desc == "()Ljava/lang/Thread;"
            {
                // `H12-1` O1 (2026-09-22): the entry address comes from
                // `admit_direct_native_entry`, which asks the registry
                // what `vm_exec::invoke_or_native` would do with this
                // triple and hands one out only if the answer is "run
                // the native". A `None` FALLS THROUGH -- it must NOT
                // `continue`, or the pc loses its `invoke_info` and the
                // backend compiles the site as a call to the enclosing
                // method. See the H20-1 module block in `jit::helpers`
                // for the backend's own statement of that rule, and for
                // why the policy token rides on the address.
                //
                // That address is still the ITEM's, not the
                // `*_DIRECT_FN` cell's, which is what every arm at this
                // door used to say for itself: `build_helpers` registers
                // those cells only AFTER this construction block, so a
                // cell read here yields 0 on the first OSR compile in a
                // process and the bind is silently skipped. The reason
                // now lives once, on `DirectNativeShadow::entry_addr`.
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::ThreadCurrentThread,
                ) {
                    cratonvm_jit::THREAD_CURRENT_THREAD_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // nio21 wave 4, lane L4 (page
            // `r21-jdk-only-current-thread-prefix-is-never-published-FIXED-20261008.md`,
            // second gap): `Thread.currentCarrierThread()` at THIS door too.
            // The two other doors bind it since wave 3 (its registered native
            // IS `currentThread`'s; its own helper, so a decline dispatches
            // this triple); a hot loop compiled here kept the generic
            // dispatch. Same terms as the arm above, and the same family
            // switch `CRATONVM_JIT_CARRIER_THREAD_DIRECT`; counted with the
            // `currentThread` sites, as the other doors count it. A `None`
            // falls through (see the arm above).
            if invoke_kind == 3
                && target_class == "java/lang/Thread"
                && mn == "currentCarrierThread"
                && desc == "()Ljava/lang/Thread;"
                && cratonvm_jit::direct_helpers::carrier_thread_direct_enabled()
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::ThreadCurrentCarrierThread,
                ) {
                    cratonvm_jit::THREAD_CURRENT_THREAD_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `Preconditions.checkIndex` / `Reference.reachabilityFence`
            // thin direct calls — the THIRD door.
            //
            // Both are recognised in `jit::try_compile`'s single-pass
            // ladder and in its IR path, and for `checkIndex` that was
            // enough: it is reached through `Objects.checkIndex`, a JDK
            // method the method-entry door compiles, so the bind landed
            // inside the callee. `reachabilityFence` has no such
            // intermediary — a hot loop calls it directly — and a hot
            // loop's body is compiled HERE, by the OSR door, which runs
            // its own callee-binding loop rather than that ladder.
            // Wiring the other two doors and not this one bound
            // `Preconditions.checkIndex=2 Reference.reachabilityFence=0`
            // (`CRATONVM_DBG=jit-method-stats`) while the fence's cost
            // did not move — 361 ns before, 142 after, against
            // `Objects.checkIndex`'s 352 -> 15. Three doors, and the
            // counter is what said which one was missing.
            //
            // Through `admit_direct_native_entry` since `H12-1` O1, like
            // every other shadowing bind at this door.
            if invoke_kind == 3
                && cratonvm_jit::census_direct_helpers_enabled()
                && target_class == "jdk/internal/util/Preconditions"
                && mn == "checkIndex"
                && desc == "(IILjava/util/function/BiFunction;)I"
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::PreconditionsCheckIndex,
                ) {
                    cratonvm_jit::PRECONDITIONS_CHECK_INDEX_SITES
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 3,
                            return_type: b'I',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `ByteBuffer.put(int,byte)` / `get(int)` and
            // `MessageDigest.update(byte)` — AT THIS DOOR TOO, and this
            // is the door that mattered.
            //
            // All three were bound in `try_compile_inner`'s ladder
            // first, and the binds moved NOTHING: `MessageDigest.
            // update(byte)` went 216 -> 195 ns and `ByteBuf.writeByte`
            // 700 -> 607, against an expected ~16x. The reason is the
            // one this file already states two arms up for
            // `reachabilityFence`: a hot LOOP body is compiled HERE, by
            // the OSR door, which runs its own callee-binding loop
            // rather than that ladder — and `testHugeDecompress` is one
            // loop, 268 million iterations, calling all three directly.
            //
            // Through `admit_direct_native_entry` since `H12-1` O1, like
            // every other shadowing bind at this door.
            //
            // No `jdk_only` TERM here, and there never was one, because
            // the bind now carries the question instead of spelling it:
            // `admit_direct_native_entry` asks both the policy this door
            // declared and the one `shared` is actually executing under,
            // so a declaration that drifts cannot widen the gate. Belt
            // and braces with
            // the callee side, which is the half that used to carry it
            // alone — each helper asks
            // `jit_direct_helper_refused` on its own fast path, with its
            // own gate, and a refusal declines to the generic dispatcher,
            // which is policy-checked. Since 2026-09-22 that guard asks
            // whether the generic dispatcher would ACTUALLY refuse, rather
            // than refusing every strict-mode call and routing it to a tail
            // that ran the same native anyway — see
            // `helpers::jit_direct_helper_jdk_only_refusals`.
            //
            // GATED, like the other two doors, and this is the door
            // where the gate does the work — for the same reason the
            // paragraph above gives about the bind itself. The helper
            // serves DIRECT receivers; on a `HeapByteBuffer` it
            // declines, and a declined call is worse than no bind at
            // all, because an unbound site takes the inline cache
            // straight to the compiled `HeapByteBuffer.get` (a bounds
            // check and an array load). Measured on the two
            // monomorphic control probes, one binary, bind on/off:
            // heap 237 ns bound against 18-22 unbound, direct 52 ns
            // bound against 132-138 unbound. Neither blanket answer is
            // right, so the question is asked per site.
            //
            // `osr_receiver_profile` is the same profile this door
            // already screens its guarded `String` intrinsics against,
            // two hundred lines up — and the reason that fetch exists
            // is the reason this gate does: a door with no profile
            // emits a decision the interpreter's own observations
            // contradict.
            if invoke_kind == 0
                && cratonvm_jit::nio_byte_direct_helpers_enabled()
                && target_class == "java/nio/ByteBuffer"
                && ((mn == "put" && desc == "(IB)Ljava/nio/ByteBuffer;")
                    || (mn == "get" && desc == "(I)B"))
                && !cratonvm_jit::nio_byte_element_bind_refused(
                    &crate::jit::helpers::direct_helper_table(),
                    osr_receiver_profile.as_ref(),
                    pc,
                    mn == "put",
                    "osr",
                )
            {
                let is_put = mn == "put";
                // `for_write` is the choice between two registered triples, so
                // it picks the shadow as well as the helper -- the same split
                // `NIO_BYTEBUFFER_{PUT,GET}_BYTE_GATE` makes on the callee side.
                let shadow = if is_put {
                    crate::jit::helpers::DirectNativeShadow::ByteBufferPutByte
                } else {
                    crate::jit::helpers::DirectNativeShadow::ByteBufferGetByte
                };
                if let Some(entry) =
                    crate::jit::helpers::admit_direct_native_entry(shared, &admission, shadow)
                {
                    cratonvm_jit::NIO_BYTE_ELEMENT_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: if is_put { 2 } else { 1 },
                            return_type: if is_put { b'L' } else { b'I' },
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `Buffer.session()` — a constant null behind a ~160 ns
            // funnel, one crossing per multi-byte heap accessor. Bound
            // at this door for the reason the block above is: a hot
            // loop body is compiled HERE, and this is where a
            // `ByteBuffer.getInt` loop's `session()` site lives.
            //
            // No receiver screen at compile time, and none is
            // possible: `session()` is `final` on `java/nio/Buffer`, so
            // a site names it through whatever buffer class the caller
            // is, and whether the null-returning SHIM (rather than the
            // real `getfield segment` bytecode) owns that receiver is a
            // per-receiver runtime fact. The helper makes that check
            // itself, against the classes the shim has actually served,
            // and declines to the generic dispatcher otherwise.
            if invoke_kind == 0
                && cratonvm_jit::buffer_session_direct_enabled()
                && cratonvm_jit::is_buffer_session_site(mn, desc)
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::BufferSession,
                ) {
                    cratonvm_jit::BUFFER_SESSION_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            // A `MemorySessionImpl` reference: the
                            // return-value ladder must oop-mark it, and the
                            // decline edge can return a real one.
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // JIT round 17 wave 1, lane nio (NIO2-2): `Object.getClass()` at
            // this door too (a hot loop's body is compiled HERE), opt-in
            // `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT`. The helper screens the
            // receiver; see `jit_object_get_class_direct`.
            if invoke_kind == 0
                && cratonvm_jit::is_object_get_class_site(mn, desc)
                // JIT round 20 (lane qMisc, correctness): resolved to
                // `Object.getClass`, not a class's own private / static
                // `getClass()` (a private one is already pinned above).
                && super::invoke::invokevirtual_site_resolves_to_object(
                    &cm_lock,
                    class_id,
                    target_class,
                    mn,
                    desc,
                )
                && cratonvm_jit::object_get_class_direct_enabled()
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::ObjectGetClass,
                ) {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] object-getclass-direct bind (osr) {target_class}.{mn} @pc={pc}"
                        );
                    }
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            // A `Class` mirror: oop-marked by the return ladder.
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            if invoke_kind == 0 && cratonvm_jit::scoped_memory_direct_enabled() {
                if let Some(op) = cratonvm_jit::is_scoped_memory_op(target_class, mn, desc) {
                    // Sixteen hand-written `as *const () as usize` arms until
                    // 2026-09-22 (`H12-1` O1). The op-to-shadow map lives beside
                    // the enumeration and is round-tripped through
                    // `is_scoped_memory_op`, so the jit crate's recognition and
                    // the VM crate's policy identity cannot drift.
                    let shadow = crate::jit::helpers::DirectNativeShadow::for_scoped_memory_op(op);
                    if let Some(entry) =
                        crate::jit::helpers::admit_direct_native_entry(shared, &admission, shadow)
                    {
                        cratonvm_jit::SCOPED_MEMORY_SITES_OSR
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: op.num_params(),
                                return_type: op.return_type(),
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
            }
            // Plain `Unsafe.get/put{Byte,Short,Int,Long}(Object,long)` at
            // this door too. `jdk/internal/misc/Unsafe`'s natives have no
            // bytecode, so §1.4 and §7 step 3 have nothing to say about
            // these eight on the native-shadow policy question -- but
            // `capability::classify_native` maps the whole class to
            // `RawMemory`, and the eight now carry that on their own
            // `DirectHelperGate`s (`jit_direct_helper_refused` refuses them
            // unconditionally of `--real-jdk` / `--compatible`), so binding
            // through the same enumerated path as everything else below
            // costs nothing this door was not already paying and closes the
            // last raw address-takes `direct_bind_source_witness` could not
            // see. See
            // `docs/feature-designs/real-jdk-mode.md`.
            // JIT round 20 wave 3, lane atomics: the private `refersTo0`
            // site arrives as `invoke_kind == 1` (see the single-pass twin in
            // `cratonvm_jit`'s `build_single_pass_tables`).
            if invoke_kind == 0 || invoke_kind == 1 {
                if let Some(op) = cratonvm_jit::is_unsafe_accessor_op(target_class, mn, desc)
                    .filter(|op| op.direct_enabled() && (invoke_kind == 0 || op.is_refers_to()))
                {
                    let shadow =
                        crate::jit::helpers::DirectNativeShadow::for_unsafe_accessor_op(op);
                    if let Some(entry) =
                        crate::jit::helpers::admit_direct_native_entry(shared, &admission, shadow)
                    {
                        op.note_site(2);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: op.num_params(),
                                return_type: op.return_type(),
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
            }
            if invoke_kind == 0
                && cratonvm_jit::md_update_direct_helper_enabled()
                && target_class == "java/security/MessageDigest"
                && mn == "update"
                && desc == "(B)V"
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::MessageDigestUpdateByte,
                ) {
                    cratonvm_jit::MD_UPDATE_BYTE_SITES
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 1,
                            return_type: b'V',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // JIT round 17 wave 1, lane nio (NIO2-2): the fence as NO code in
            // this single-pass OSR body, opt-in
            // `CRATONVM_JIT_REACHABILITY_FENCE_ELIDE`. An intrinsic sentinel,
            // not a helper address: no registry row is consulted or bypassed,
            // because JDK 25's body is an empty `return`. See
            // `cratonvm_jit::JitIntrinsic::ReachabilityFence` for the liveness
            // argument (the feeding bytecode keeps the referent mapped).
            if invoke_kind == 3
                && cratonvm_jit::is_reachability_fence_site(target_class, mn, desc)
                && cratonvm_jit::reachability_fence_elide_enabled()
            {
                if crate::runtime::env_cache::dbg_jitc() {
                    eprintln!("[cratonvm-jitc] reachability-fence elided (osr) @pc={pc}");
                }
                direct_calls2.push((
                    pc,
                    crate::jit::JitDirectCall {
                        entry: cratonvm_jit::JitIntrinsic::ReachabilityFence.as_entry(),
                        needs_context: false,
                        num_params: 1,
                        return_type: b'V',
                        guard_class_id: 0,
                    },
                ));
                continue;
            }
            if invoke_kind == 3
                && cratonvm_jit::census_direct_helpers_enabled()
                && target_class == "java/lang/ref/Reference"
                && mn == "reachabilityFence"
                && desc == "(Ljava/lang/Object;)V"
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::ReachabilityFence,
                ) {
                    cratonvm_jit::REACHABILITY_FENCE_SITES
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 1,
                            return_type: b'V',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `Integer.valueOf(I)` thin direct call — statically bound
            // NATIVE callee, so the eager callee compile below can never
            // succeed and the generic dispatch round trip is pure fixed
            // overhead on the hottest autoboxing path. Parity with the
            // recognition in `jit::try_compile`; see
            // `jit::helpers::jit_integer_value_of_direct`. This OSR-tier
            // site matters most: a single-invocation harness method
            // (e.g. a benchmark main loop) runs its entire life inside
            // the OSR body.
            if invoke_kind == 3
                && target_class == "java/lang/Integer"
                && mn == "valueOf"
                && desc == "(I)Ljava/lang/Integer;"
            {
                // `H12-1` O1. `Integer.valueOf` is `intrinsic`, so §1.4's
                // reviewed exception admits it in both modes and this bind
                // does not move — which is the point of asking rather than
                // assuming: the answer is now a measured `Some`, not an
                // omission that happens to be right.
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::IntegerValueOf,
                ) {
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 1,
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `Long.valueOf(J)` — the twin of the `Integer.valueOf(I)`
            // recognition directly above, at this door for the same
            // stated reason: an OSR body is where a hot boxing loop
            // actually runs. `num_params: 1`, not 2 — the JIT counts one
            // operand slot per PARAMETER, not JVMS category-2 pairs.
            if invoke_kind == 3
                && cratonvm_jit::long_box_direct_helpers_enabled()
                && target_class == "java/lang/Long"
                && mn == "valueOf"
                && desc == "(J)Ljava/lang/Long;"
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::LongValueOf,
                ) {
                    cratonvm_jit::LONG_VALUE_OF_SITES
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 1,
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
            // `AtomicInteger` read-modify-write family, through the
            // SAME matcher `jit::try_compile_inner` uses so the two
            // doors cannot drift on which shapes are admitted.
            //
            // Registered here because this door reaches
            // `x64::compile_with_param_slots` directly. For this family
            // the OSR site is the load-bearing one, for the same reason
            // spelled out on the HashMap arm below: a counter loop
            // written inside ONE method never passes through
            // `jit::try_compile`, and that is exactly the shape
            // (`while (nextIndex.getAndIncrement() < MAX)`) this
            // intrinsic exists to speed up.
            //
            // The class id comes off `cm_lock`, the guard this loop
            // ALREADY holds, and not from a fresh `.read()`.
            //
            // This comment used to say "the class-manager guard is read
            // and dropped inside the `let` so no lock is held across the
            // matcher call". That was true and it was the wrong half of
            // the question: the matcher never held one, and the LOOP
            // did — `cm_lock` at the top of this `if
            // !scan.invoke_ops.is_empty()` block is live for every
            // iteration, because `class` is borrowed out of it. A second
            // `.read()` here is therefore recursive.
            //
            // `parking_lot`'s `RwLock` read is not reentrant. A writer
            // that arrives between the outer acquisition and the inner
            // one parks the inner read BEHIND itself (readers do not
            // barge past a queued writer), and the guard that writer is
            // waiting for is the outer one this thread is holding. A
            // background compile racing any class load is the whole
            // window, and the `cratonvm-jit-co` thread is where it was
            // caught: `vm-cli/tests/jit_compile_gate_doors.rs` failed
            // with `lock order violation: attempted to acquire
            // ClassManager (level 10) while holding ClassManager (level
            // 10)` — the L10 `OrderedPlRwLock` reporting the deadlock
            // one step before it could happen.
            //
            // Three regions in this loop had it (`AtomicInteger`,
            // `AtomicLong`, and the box/unbox pair); all three now read
            // through `cm_lock`, which is also strictly cheaper.
            //
            // The rule is not new here. `dispatch_virtual.rs`'s proxy
            // walk states it in full — "walk the chain using the
            // ALREADY-HELD `cm` guard rather than calling
            // `class_chain_reaches_proxy_instance` (which takes its own
            // read) -- a nested second read acquisition on the same
            // thread self-deadlocks under parking_lot's writer-preferring
            // fairness once any writer is queued" — and these three
            // arms were written without it. `vm-cli/tests/
            // jit_compile_gate_doors.rs::
            // the_osr_door_takes_no_recursive_class_manager_lock` is the
            // guard that now names the families rather than waiting for
            // one to turn up in an unrelated probe.
            //
            // Review #80: a site naming a SUBCLASS (`Counter extends
            // AtomicInteger`) matches only through the class that
            // declares the resolved method, and only for a method `final`
            // in the JDK. Its guard is the subclass's own id, so any
            // other receiver class falls back. Both answers come off
            // `cm_lock` too (`site_class_and_declaring_class_name`).
            if invoke_kind == 0
                && cratonvm_jit::atomic_intrinsic_site_may_match(
                    "java/util/concurrent/atomic/AtomicInteger",
                    &target_class,
                    &mn,
                    &desc,
                )
            {
                let atomic_site: Option<(u32, Option<String>)> = if target_class
                    == "java/util/concurrent/atomic/AtomicInteger"
                {
                    cm_lock
                        .find_bootstrap_class_by_name("java/util/concurrent/atomic/AtomicInteger")
                        .map(|id| (id.as_u32(), None))
                } else {
                    site_class_and_declaring_class_name(
                        shared,
                        &cm_lock,
                        class_id,
                        &target_class,
                        &mn,
                        &desc,
                    )
                    .map(|(cid, declaring_class)| (cid, Some(declaring_class)))
                };
                if let Some((entry, num_params, ret, guard_class_id)) =
                    atomic_site.and_then(|(cid, declaring_class)| {
                        cratonvm_jit::try_resolve_atomic_intrinsic_for_site(
                            &target_class,
                            declaring_class.as_deref(),
                            &mn,
                            &desc,
                            cid,
                        )
                    })
                {
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            guard_class_id,
                        },
                    ));
                    continue;
                }
            }
            // ===== INTRINSIC REGION END: ATOMIC_INT =====

            // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
            // The OSR door's copy of the 64-bit family. It has to be
            // here as well as in `jit::try_compile`: an intrinsic
            // registered in one compile door is INERT in the others,
            // which is the failure this file's own
            // `Thread.currentThread` and String binds each record once.
            //
            // It matters most exactly where OSR matters: a
            // single-invocation method whose whole life is one hot
            // loop. `HashedWheelTimer`'s worker is that shape, and its
            // `pendingTimeouts` is an `AtomicLong` incremented once per
            // scheduled timeout and decremented once per expiry.
            // Subclass sites match through the declaring class, on the
            // AtomicInteger arm's terms (which excludes `longValue()`).
            if invoke_kind == 0
                && cratonvm_jit::atomic_intrinsic_site_may_match(
                    "java/util/concurrent/atomic/AtomicLong",
                    &target_class,
                    &mn,
                    &desc,
                )
            {
                // Through `cm_lock` — see the AtomicInteger arm above for
                // why a fresh `.read()` here is a recursive acquisition.
                let atomic_long_site: Option<(u32, Option<String>)> =
                    if target_class == "java/util/concurrent/atomic/AtomicLong" {
                        cm_lock
                            .find_bootstrap_class_by_name("java/util/concurrent/atomic/AtomicLong")
                            .map(|id| (id.as_u32(), None))
                    } else {
                        site_class_and_declaring_class_name(
                            shared,
                            &cm_lock,
                            class_id,
                            &target_class,
                            &mn,
                            &desc,
                        )
                        .map(|(cid, declaring_class)| (cid, Some(declaring_class)))
                    };
                if let Some((entry, num_params, ret, guard_class_id)) =
                    atomic_long_site.and_then(|(cid, declaring_class)| {
                        cratonvm_jit::try_resolve_atomic_long_intrinsic_for_site(
                            &target_class,
                            declaring_class.as_deref(),
                            &mn,
                            &desc,
                            cid,
                        )
                    })
                {
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            guard_class_id,
                        },
                    ));
                    continue;
                }
            }
            // ===== INTRINSIC REGION END: ATOMIC_LONG =====

            // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
            // `Long.longValue()` / `Integer.intValue()` emitted INLINE.
            //
            // Deliberately placed BEFORE the two thin direct binds
            // below, which recognise the same two triples: whichever
            // arm runs first `continue`s, so this ordering is what
            // decides that the site gets an inline `MOV` rather than a
            // CALL. The binds stay as the fallback for a site whose
            // receiver class id does not resolve.
            //
            // THIS is the load-bearing door for the workload that
            // motivates the intrinsic, and the reason it is not enough
            // to add it to `try_compile` alone. An autoboxed counter
            // lives in a LOOP BODY — `probes/BlobStreamCostCpu.java`'s
            // `boxed Long counter` arm is `if (count > 0) { count--; }`
            // — and a loop body is what OSR compiles: that probe
            // reports `osr_entered=51` against `method-entry:
            // admitted=2`. A single-pass-only intrinsic would report
            // sites and move nothing, which is the exact failure the
            // `VarHandle` bind below records for itself.
            if invoke_kind == 0
                && (target_class == "java/lang/Long" || target_class == "java/lang/Integer")
            {
                // Through `cm_lock` — see the AtomicInteger arm above for
                // why a fresh `.read()` here is a recursive acquisition.
                let box_cid = cm_lock
                    .find_bootstrap_class_by_name(&target_class)
                    .map(|id| id.as_u32());
                if let Some((entry, num_params, ret, guard_class_id)) = box_cid.and_then(|cid| {
                    cratonvm_jit::try_resolve_box_unbox_intrinsic(&target_class, &mn, &desc, cid)
                }) {
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            guard_class_id,
                        },
                    ));
                    continue;
                }
            }
            // ===== INTRINSIC REGION END: BOX_UNBOX =====

            // `Integer.intValue()` thin direct call — `Integer` is
            // `final`, so a site declared against it is statically
            // monomorphic (guard-free); the helper handles the
            // null-receiver NPE itself.
            if invoke_kind == 0
                && target_class == "java/lang/Integer"
                && mn == "intValue"
                && desc == "()I"
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::IntegerIntValue,
                ) {
                    cratonvm_jit::note_integer_int_value_direct_site();
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            return_type: b'I',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `ThreadLocal.get()` thin direct call (SBR round 2, wave 8, lane
            // `tlocal`): not final, so the helper screens the receiver's class
            // itself and declines what it does not recognise to the generic
            // dispatch. `--real-jdk` only.
            if invoke_kind == 0
                && (target_class == "java/lang/ThreadLocal"
                    || target_class == "java/lang/InheritableThreadLocal")
                && mn == "get"
                && desc == "()Ljava/lang/Object;"
                && cratonvm_jit::thread_local_get_direct_enabled()
                && crate::vm::dispatch_policy(shared).is_jdk_only()
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::ThreadLocalGet,
                ) {
                    cratonvm_jit::note_thread_local_get_direct_site();
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            return_type: b'L',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `ThreadLocal.set(Object)`, the twin of the `get()` bind above.
            if invoke_kind == 0
                && (target_class == "java/lang/ThreadLocal"
                    || target_class == "java/lang/InheritableThreadLocal")
                && mn == "set"
                && desc == "(Ljava/lang/Object;)V"
                && cratonvm_jit::thread_local_get_direct_enabled()
                && crate::vm::dispatch_policy(shared).is_jdk_only()
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::ThreadLocalSet,
                ) {
                    cratonvm_jit::note_thread_local_get_direct_site();
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 1,
                            return_type: b'V',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            // `Long.longValue()` — the twin of `Integer.intValue()`
            // directly above. `java/lang/Long` is `final` on the same
            // terms, so a site declared against it is statically
            // monomorphic and needs no receiver guard; the helper
            // handles the null-receiver NPE and declines anything that
            // fails heap validation to the generic dispatcher.
            if invoke_kind == 0
                && cratonvm_jit::long_box_direct_helpers_enabled()
                && target_class == "java/lang/Long"
                && mn == "longValue"
                && desc == "()J"
            {
                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                    shared,
                    &admission,
                    crate::jit::helpers::DirectNativeShadow::LongLongValue,
                ) {
                    cratonvm_jit::LONG_LONG_VALUE_SITES
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // The BIND-vs-EXECUTION triple's half, which
                    // `vm/src/runtime/interp_census.rs` prints as `sites_bound=`
                    // under `CRATONVM_DBG=direct-binds`. Two counters for one bind
                    // is not redundancy — `LONG_LONG_VALUE_SITES` belongs to the
                    // BOXING pair `tiered.rs` prints beside `Long.valueOf`, and
                    // this one belongs to the bind/served/declined triple — and
                    // until 2026-09-22 only the first was fed from this door, so
                    // `sites_bound` read LOWER than the `tiered.rs` figure for the
                    // same quantity. Exactly as the `Integer.intValue` arm above
                    // calls `note_integer_int_value_direct_site()`.
                    // (`docs/feature-designs/jit-ir-optimizations.md`)
                    cratonvm_jit::note_long_long_value_direct_site();
                    direct_calls2.push((
                        pc,
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 0,
                            return_type: b'J',
                            guard_class_id: 0,
                        },
                    ));
                    continue;
                }
            }
            if invoke_kind == 0
                && target_class == "java/lang/invoke/VarHandle"
                && cratonvm_jit::direct_jit_callee_calls_enabled()
                && cratonvm_jit::varhandle_write_direct_helpers_enabled()
                && crate::jit::helpers::varhandle_method_entry_binds_admitted(shared)
                && (!crate::vm::dispatch_policy(shared).is_jdk_only()
                    || (cratonvm_native_builtins::lang_invoke::vh_osr_bind_asks_admission_enabled(
                    ) && crate::jit::helpers::direct_native_bind_admitted(
                        shared,
                        "java/lang/invoke/VarHandle",
                        &mn,
                        "([Ljava/lang/Object;)V",
                    )))
            {
                if let Some(slot) = cratonvm_jit::varhandle_byte_buffer_view_write_slot(&mn, &desc)
                {
                    let entry =
                        crate::jit::helpers::varhandle_byte_buffer_view_write_direct_fn(slot);
                    let record = cratonvm_jit::VarHandleJudgedSite::new(
                        shared as *const SharedVm as usize, // Cast: the helper's `SharedVm` word
                        entry,
                        &target_class,
                        &mn,
                        &desc,
                        param_count + 1,
                        b'V',
                        0,
                        class_id.as_u32(),
                        0,
                    );
                    let row = record.direct_call(3, b'V');
                    osr_vh_judged_sites.push(record);
                    direct_calls2.push((pc, row));
                    continue;
                }
            }
            // `VarHandle` read modes on an instance field with a
            // primitive return — parity with `jit::try_compile`'s
            // recognition (see `cratonvm_jit::VARHANDLE_READ_DIRECT_FNS`).
            //
            // THIS is the load-bearing door for the workload that
            // motivates the bind. netty checks `refCnt` on every
            // buffer accessor, so the reads happen inside the
            // byte-transfer LOOPS of `SnappyFrameDecoder` and
            // `ByteToMessageDecoder`, and a loop body is what OSR
            // compiles. A single-pass-only bind would report sites and
            // move nothing.
            //
            // Unlike its neighbours here this one asks the policy
            // question, because the answer is not the same for every
            // bind: the four read modes are registered
            // `NativeKind::Bridge`, which JDK-ONLY-WAVE2 §1.4 does not
            // permit a compile-time bake of. `dispatch_policy` is the
            // same source `jit::try_compile`'s `jdk_only` argument comes
            // from, so both doors ask the registry (round 15 wave 4).
            if invoke_kind == 0
                && cratonvm_jit::varhandle_read_direct_helpers_enabled()
                && target_class == "java/lang/invoke/VarHandle"
                // Round 15 wave 4 (lane vh5 patch, orchestrator): the MethodEntry
                // door's policy question (`direct_native_helper` ->
                // `direct_native_bind_admitted`) on the erased registered
                // descriptor, so both doors ask the registry under `--real-jdk`.
                && (!crate::vm::dispatch_policy(shared).is_jdk_only()
                    || (cratonvm_native_builtins::lang_invoke::vh_osr_bind_asks_admission_enabled()
                        && crate::jit::helpers::direct_native_bind_admitted(
                            shared,
                            "java/lang/invoke/VarHandle",
                            &mn,
                            "([Ljava/lang/Object;)Ljava/lang/Object;",
                        )))
                // Round 15 wave 2 (lane vh2): not once an exact handle exists,
                // unless the bind is JUDGED (round 15 wave 6, lane vh8).
                && crate::jit::helpers::varhandle_method_entry_binds_admitted(shared)
            {
                if let Some(slot) = cratonvm_jit::varhandle_byte_array_view_read_slot(&mn, &desc) {
                    let entry = crate::jit::helpers::varhandle_byte_array_view_read_direct_fn(slot);
                    let ret = cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[slot];
                    cratonvm_jit::VARHANDLE_READ_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let row = if cratonvm_jit::varhandle_judged_binds_enabled() {
                        let record = cratonvm_jit::VarHandleJudgedSite::new(
                            shared as *const SharedVm as usize, // Cast: the helper's `SharedVm` word
                            entry,
                            &target_class,
                            &mn,
                            &desc,
                            param_count + 1,
                            cratonvm_jit::return_type(&desc),
                            0,
                            class_id.as_u32(),
                            0,
                        );
                        let row = record.direct_call(2, ret);
                        osr_vh_judged_sites.push(record);
                        row
                    } else {
                        cratonvm_jit::add_varhandle_thin_bind_dependency(
                            &mut osr_vh_thin_bind_deps,
                        );
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 2,
                            return_type: ret,
                            guard_class_id: 0,
                        }
                    };
                    direct_calls2.push((pc, row));
                    continue;
                }
                // VH2-3 (round 15 wave 2 hand-back, lane vh2): not a slot
                // whose site needs a reference cast the helper cannot make.
                if let Some(slot) = cratonvm_jit::varhandle_read_helper_slot(&mn, &desc)
                    .filter(|&slot| crate::jit::helpers::varhandle_read_slot_bindable(slot))
                {
                    // The helper address is taken directly rather than
                    // read out of the jit-crate cell, for the
                    // registration-order reason spelled out on the
                    // `Integer.valueOf` recognition above: this path can
                    // run before `build_helpers` has published them.
                    let entry = crate::jit::helpers::varhandle_read_direct_fn(slot);
                    cratonvm_jit::VARHANDLE_READ_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Round 15 wave 6 (lane vh8; VH7-1): JUDGED when the switch
                    // is on (no thin-bind dependency: the first mint has
                    // nothing to withdraw), else the wave-3 unjudged row.
                    let row = if cratonvm_jit::varhandle_judged_binds_enabled() {
                        let record = cratonvm_jit::VarHandleJudgedSite::new(
                            shared as *const SharedVm as usize, // Cast: the helper's `SharedVm` word
                            entry,
                            &target_class,
                            &mn,
                            &desc,
                            param_count + 1,
                            cratonvm_jit::return_type(&desc),
                            0,
                            class_id.as_u32(),
                            0,
                        );
                        let row =
                            record.direct_call(1, cratonvm_jit::varhandle_read_slot_return(slot));
                        osr_vh_judged_sites.push(record);
                        row
                    } else {
                        cratonvm_jit::add_varhandle_thin_bind_dependency(
                            &mut osr_vh_thin_bind_deps,
                        );
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 1,
                            return_type: cratonvm_jit::varhandle_read_slot_return(slot),
                            guard_class_id: 0,
                        }
                    };
                    direct_calls2.push((pc, row));
                    continue;
                }
            }
            // `VarHandle` write modes on an instance field — parity
            // with `jit::try_compile`'s recognition (see
            // `cratonvm_jit::VARHANDLE_WRITE_DIRECT_FNS`).
            //
            // THIS door is the load-bearing one, for the same reason
            // the read bind above says it is, and the write bind
            // learned it the expensive way: bound in the single-pass
            // ladder ALONE, a native census of `HibfixVarHandleProbe`
            // still counted 698 000 `VarHandle.set` dispatches with the
            // bind on and 698 000 with it off — identical, because the
            // probe's stores are in a `main` loop and a loop body is
            // what OSR compiles. The site counter said "bound"; the
            // census said "moved nothing".
            //
            // Asks the policy question for the same reason: `set` is
            // registered `NativeKind::Bridge`, which JDK-ONLY-WAVE2
            // §1.4 does not permit a compile-time bake of.
            if invoke_kind == 0
                && cratonvm_jit::varhandle_write_direct_helpers_enabled()
                && target_class == "java/lang/invoke/VarHandle"
                // Round 15 wave 4 (lane vh5 patch, orchestrator): the MethodEntry
                // door's policy question (`direct_native_helper` ->
                // `direct_native_bind_admitted`) on the erased registered
                // descriptor, so both doors ask the registry under `--real-jdk`.
                && (!crate::vm::dispatch_policy(shared).is_jdk_only()
                    || (cratonvm_native_builtins::lang_invoke::vh_osr_bind_asks_admission_enabled()
                        && crate::jit::helpers::direct_native_bind_admitted(
                            shared,
                            "java/lang/invoke/VarHandle",
                            &mn,
                            "([Ljava/lang/Object;)V",
                        )))
                // Round 15 wave 2 (lane vh2): not once an exact handle exists,
                // unless the bind is JUDGED (round 15 wave 6, lane vh8).
                && crate::jit::helpers::varhandle_method_entry_binds_admitted(shared)
            {
                if let Some(slot) = cratonvm_jit::varhandle_write_helper_slot(&mn, &desc) {
                    // Address taken directly rather than out of the
                    // jit-crate cell — this path can run before
                    // `build_helpers` has published them.
                    let entry = crate::jit::helpers::varhandle_write_direct_fn(slot);
                    cratonvm_jit::VARHANDLE_WRITE_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Round 15 wave 6 (lane vh8; VH7-1): see the read arm.
                    let row = if cratonvm_jit::varhandle_judged_binds_enabled() {
                        let record = cratonvm_jit::VarHandleJudgedSite::new(
                            shared as *const SharedVm as usize, // Cast: the helper's `SharedVm` word
                            entry,
                            &target_class,
                            &mn,
                            &desc,
                            param_count + 1,
                            cratonvm_jit::return_type(&desc),
                            0,
                            class_id.as_u32(),
                            0,
                        );
                        let row = record.direct_call(2, b'V');
                        osr_vh_judged_sites.push(record);
                        row
                    } else {
                        cratonvm_jit::add_varhandle_thin_bind_dependency(
                            &mut osr_vh_thin_bind_deps,
                        );
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 2,
                            return_type: b'V',
                            guard_class_id: 0,
                        }
                    };
                    direct_calls2.push((pc, row));
                    continue;
                }
            }
            // `VarHandle.compareAndSet` on an instance field — parity
            // with `jit::try_compile`'s recognition (see
            // `cratonvm_jit::VARHANDLE_CAS_DIRECT_FNS`).
            //
            // THIS door is the load-bearing one, for exactly the reason
            // the write bind above records in full: a benchmark-style
            // `main` loop, and `CompletableFuture.tryPushStack` reached
            // from one, live their whole lives inside an OSR body and
            // never pass through `jit::try_compile`.
            if invoke_kind == 0
                && cratonvm_jit::varhandle_cas_direct_helpers_enabled()
                && target_class == "java/lang/invoke/VarHandle"
                // Round 15 wave 4 (lane vh5 patch, orchestrator): the MethodEntry
                // door's policy question (`direct_native_helper` ->
                // `direct_native_bind_admitted`) on the erased registered
                // descriptor, so both doors ask the registry under `--real-jdk`.
                && (!crate::vm::dispatch_policy(shared).is_jdk_only()
                    || (cratonvm_native_builtins::lang_invoke::vh_osr_bind_asks_admission_enabled()
                        && crate::jit::helpers::direct_native_bind_admitted(
                            shared,
                            "java/lang/invoke/VarHandle",
                            &mn,
                            "([Ljava/lang/Object;)Z",
                        )))
                // Round 15 wave 2 (lane vh2): not once an exact handle exists,
                // unless the bind is JUDGED (round 15 wave 6, lane vh8).
                && crate::jit::helpers::varhandle_method_entry_binds_admitted(shared)
            {
                if let Some(slot) = cratonvm_jit::varhandle_cas_helper_slot(&mn, &desc) {
                    // Address taken directly rather than out of the
                    // jit-crate cell — this path can run before
                    // `build_helpers` has published them.
                    let entry = crate::jit::helpers::varhandle_cas_direct_fn(slot);
                    cratonvm_jit::VARHANDLE_CAS_SITES_OSR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Round 15 wave 6 (lane vh8; VH7-1): see the read arm.
                    let row = if cratonvm_jit::varhandle_judged_binds_enabled() {
                        let record = cratonvm_jit::VarHandleJudgedSite::new(
                            shared as *const SharedVm as usize, // Cast: the helper's `SharedVm` word
                            entry,
                            &target_class,
                            &mn,
                            &desc,
                            param_count + 1,
                            cratonvm_jit::return_type(&desc),
                            0,
                            class_id.as_u32(),
                            0,
                        );
                        let row = record.direct_call(3, b'Z');
                        osr_vh_judged_sites.push(record);
                        row
                    } else {
                        cratonvm_jit::add_varhandle_thin_bind_dependency(
                            &mut osr_vh_thin_bind_deps,
                        );
                        crate::jit::JitDirectCall {
                            entry,
                            needs_context: true,
                            num_params: 3,
                            return_type: b'Z',
                            guard_class_id: 0,
                        }
                    };
                    direct_calls2.push((pc, row));
                    continue;
                }
            }
            // Exact-HashMap `put`/`get` thin direct calls — parity
            // with `jit::try_compile`'s recognition (guard-free: the
            // helper verifies the receiver's EXACT class and routes
            // subclasses / non-overlay cases to the full generic
            // dispatcher). As with `Integer.valueOf` above, this
            // OSR-tier site is the load-bearing one: a once-invoked
            // benchmark-style method runs its whole life inside the
            // OSR body and never passes through `jit::try_compile`.
            if matches!(invoke_kind, 0 | 1)
                && target_class == "java/lang/reflect/Constructor"
                && mn == "newInstance"
                && desc == "([Ljava/lang/Object;)Ljava/lang/Object;"
            {
                direct_calls2.push((
                    pc,
                    crate::jit::JitDirectCall {
                        entry: crate::jit::helpers::jit_ctor_new_instance_direct as *const () as usize,
                        needs_context: true,
                        num_params: 1,
                        return_type: b'L',
                        guard_class_id: 0,
                    },
                ));
                continue;
            }
            if invoke_kind == 0 && target_class == "java/util/HashMap" {
                let recognized = if mn == "put"
                    && desc == "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                {
                    Some((
                        crate::jit::helpers::DirectNativeShadow::HashMapPut,
                        2usize,
                        &cratonvm_jit::HASHMAP_PUT_DIRECT_SITES_OSR,
                    ))
                } else if mn == "get" && desc == "(Ljava/lang/Object;)Ljava/lang/Object;" {
                    Some((
                        crate::jit::helpers::DirectNativeShadow::HashMapGet,
                        1usize,
                        &cratonvm_jit::HASHMAP_GET_DIRECT_SITES_OSR,
                    ))
                } else {
                    None
                };
                if let Some((shadow, num_params, sites)) = recognized {
                    if let Some(entry) =
                        crate::jit::helpers::admit_direct_native_entry(shared, &admission, shadow)
                    {
                        // `H12-1` N2: these two binds had no counter at all, and
                        // that is why `H7-1` §6c's prediction about
                        // `collection_direct_helper_sites()` was unfalsifiable --
                        // the counters it named belong to the single-pass door and
                        // cannot see this one. One relaxed add per compile.
                        sites.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
            }

            // Trivial constructor: defer for after-lock elidability
            // resolution. Checked BEFORE the eager-compile scheduling
            // below, because a `<init>()V` that turns out to be
            // elidable must be elided rather than called — the
            // resolution needs `load_class_concurrent`, so it cannot
            // happen here under the lock.
            if invoke_kind == 1 && mn == "<init>" && desc == "()V" && !ctor_direct_call_off {
                pending_ctor_sites.push((pc, target_class.to_string(), param_count));
                continue;
            }

            // Schedule eager callee compilation (after lock release) for
            // every STATICALLY BOUND site: `invokestatic` (kind 3) and
            // `invokespecial` (kind 1). See `pending_callee_compiles`
            // for what admitting kind 1 was worth and why it is safe —
            // the bind applies the same four refusals either way.
            if matches!(invoke_kind, 1 | 3) && !is_recursive_call && !cratonvm_jit::offload_hook::is_reflect_ctor_dispatch(&target_class, &mn, &desc) {
                pending_callee_compiles.push((
                    pc,
                    target_class.to_string(),
                    mn.to_string(),
                    desc.to_string(),
                    param_count,
                    invoke_kind,
                ));
                continue;
            }

            // Round 16 wave 13 (lane mega): a candidate for a guarded splice.
            // The site keeps everything below (its `JitInvokeInfo` and its
            // inline caches): that dispatch is the guard chain's miss edge.
            if osr_guarded_splices && matches!(invoke_kind, 0 | 2) {
                osr_guarded_candidates.push((
                    pc,
                    target_class.to_string(),
                    mn.to_string(),
                    desc.to_string(),
                    invoke_kind,
                ));
            }
            let num_jit_args = if invoke_kind == 3 {
                param_count
            } else {
                param_count + 1
            };
            let return_type = crate::jit::return_type(desc);
            push_osr_invoke_info(
                &mut owned_jit_strings2,
                &mut owned_jit_invoke_infos2,
                &mut invoke_info,
                pc,
                (
                    target_class.to_string().into_boxed_str(),
                    mn.to_string().into_boxed_str(),
                    desc.to_string().into_boxed_str(),
                ),
                num_jit_args,
                return_type,
                invoke_kind,
                class_id.as_u32(),
            );
            allocate_dynamic_dispatch_slots(
                invoke_kind,
                pc,
                &mut mic_slots2,
                &mut owned_mic_slots2,
                &mut pic_slots2,
                &mut owned_pic_slots2,
            );
        }
    }

    // invokedynamic-uncommon-trap fix: resolve each `indy_ops` site's
    // target descriptor to the minimal stack-effect info the codegen
    // needs (arg slot count + return type tag) — see the `indy_info`
    // field doc on the x64 `Compiler` struct. A site that cannot be
    // resolved is simply omitted; the x64 codegen's 0xba arm then
    // bails the whole compile (`return false`) rather than guessing.
    //
    // Sites come from the per-VM intern table (`interned_jit_indy_bridge_site`),
    // so an attempt refused after this loop no longer leaks them; the loop
    // stays below the invoke loop only because nothing needs it earlier.
    let mut indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)> = Vec::new();
    if !scan.indy_ops.is_empty() {
        let cm_lock = shared.classes.class_manager.read();
        if let Some(class) = cm_lock.get_class(class_id) {
            for &(pc_indy, cp_idx) in &scan.indy_ops {
                if let Some(ConstantPoolEntry::InvokeDynamic {
                    name_and_type_index,
                    ..
                }) = class.constant_pool.get(cp_idx)
                {
                    if let Some((_, descriptor)) =
                        class.constant_pool.get_name_and_type(*name_and_type_index)
                    {
                        let arg_slots = crate::jit::count_param_slots(descriptor);
                        let ret_type = crate::jit::return_type(descriptor);
                        let arg_type_tags = crate::jit::indy_arg_type_tags(descriptor);
                        let bridge_site = interned_jit_indy_bridge_site(
                            shared,
                            &cm_lock,
                            class,
                            class_id,
                            cp_idx,
                            Some((method_name.as_str(), method_descriptor.as_str(), pc_indy)),
                        );
                        indy_info.push((pc_indy, arg_slots, ret_type, arg_type_tags, bridge_site));
                    }
                }
            }
        }
    }

    // RBC.7 (relaxed) — an OSR frame cannot safely take the generic
    // indy uncommon trap: the bail resumes the pre-OSR interpreter
    // frame at the stale back-edge, silently re-running a loop whose
    // side effects already committed (see
    // jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md).
    // Admit only BRIDGED sites, which emit a direct call and never
    // deopt at the indy bci. `indy_info` drops sites it cannot resolve,
    // so a length mismatch also means "not fully bridged" and is
    // refused here.
    //
    // The bridged set grew on 2026-08-23 from `StringConcatFactory`
    // alone to `LambdaMetafactory` as well, and this gate is the reason
    // that matters as much as the whole-method one: `osr-DENY
    // (unbridged invokedynamic)` is METHOD-WIDE — one lambda creation
    // anywhere in a method denied OSR to every loop in it.
    //
    // Denied (`mark_osr_denied`, until the install epoch moves) rather than
    // merely refused: whether a site is bridged is a function of the
    // constant pool and process-constant levers, so every retry — up to five
    // per activation, from the frame's backoff — reached the same answer after
    // paying every invoke site's resolution above.
    if indy_info.len() != scan.indy_ops.len()
        || indy_info
            .iter()
            .any(|(_, _, ret_type, _, bridge_site)| *bridge_site == 0 || *ret_type == b'V')
    {
        if crate::runtime::env_cache::dbg_jitc() && !scan.indy_ops.is_empty() {
            eprintln!(
                "[cratonvm-jitc] osr-DENY (unbridged invokedynamic) {}.{}{}",
                class_name, method_name, method_descriptor
            );
        }
        shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
        return None;
    }

    // Resolve the deferred trivial-ctor sites FIRST (cm_lock released),
    // because the answer decides which of two later paths each site
    // takes: an elidable `C.<init>()V` is emitted AS
    // `java/lang/Object.<init>` so the codegen elision drops it
    // entirely, while a NON-elidable one now joins
    // `pending_callee_compiles` for the same eager-compile + direct
    // bind every other statically-bound site gets. It used to fall
    // straight to dispatch — the `new FastThreadLocal<Boolean>()`
    // shape, and the reason that loop paid ~300 ns per iteration.
    //
    // Pcs whose `<init>()V` target `is_elidable_construction` PROVED empty. The
    // backend may elide only these; a no-arg constructor that is NOT proven empty
    // keeps both its allocation and its call, because eliding it would drop
    // whatever the body writes to global state (see
    // jit-elided-constructor-side-effects-FIXED-20260812.md).
    let mut elidable_init_pcs: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (pc, tclass, pcount) in pending_ctor_sites {
        let elidable = resolve_cp_class_for_owner(shared, class_id, &tclass)
            .map(|tid| {
                let cm2 = shared.classes.class_manager.read();
                is_elidable_construction(shared, &cm2, tid)
            })
            .unwrap_or(false);
        if elidable {
            elidable_init_pcs.insert(pc);
            // Emitted as `Object.<init>` so the codegen elision fires;
            // no call survives, so there is nothing to bind.
            push_osr_invoke_info(
                &mut owned_jit_strings2,
                &mut owned_jit_invoke_infos2,
                &mut invoke_info,
                pc,
                ("java/lang/Object".into(), "<init>".into(), "()V".into()),
                pcount + 1, // receiver + params
                b'V',
                1,
                class_id.as_u32(),
            );
        } else if !osr_ctor_bind_off {
            // NOT elidable, and the site joins `pending_callee_compiles`
            // for the same eager-compile + direct bind every other
            // statically-bound site in this door gets. This is what the
            // `new FastThreadLocal<Boolean>()` loop was paying a
            // per-allocation `jit_invoke_dispatch` for.
            //
            // HISTORY, because the obvious reading of it is wrong.
            // This reroute was tried on 2026-08-13 and REVERTED: it made
            // `compile_with_param_slots` refuse the enclosing method,
            // and an OSR refusal is not a fallback to a slower compile —
            // it marks the method OSR-denied for the process lifetime,
            // so the hot loop interpreted forever (`new A()` 311 ns ->
            // 1412 ns). The page filed that as "why the codegen refuses
            // that shape is unresolved".
            //
            // It was not the shape. A direct-bound site ALSO needs a
            // `JitInvokeInfo` — the codegen's direct-call arm reads it
            // to name the callee for the exceptional-return service —
            // and this door pushed none, which is exactly the refusal
            // the sibling non-`()V` `invokespecial` admission hit and
            // fixed a few hundred lines below ("an `invokespecial` bind
            // without it makes `compile_with_param_slots` refuse the
            // whole method"). That fix landed for kind-1 sites arriving
            // through the scan loop; `()V` ctor sites arrive through
            // HERE, bypassed it, and so still had none. Routing them
            // into the same list makes them take the same bind, and the
            // `JitInvokeInfo` comes with it.
            //
            // `CRATONVM_NO_OSR_CTOR_BIND=1` restores the dispatch.
            pending_callee_compiles.push((
                pc,
                tclass,
                "<init>".to_string(),
                "()V".to_string(),
                pcount,
                1u8,
            ));
        } else {
            // `CRATONVM_NO_OSR_CTOR_BIND=1`: keep the per-allocation
            // dispatch, the pre-2026-08-17 behaviour.
            push_osr_invoke_info(
                &mut owned_jit_strings2,
                &mut owned_jit_invoke_infos2,
                &mut invoke_info,
                pc,
                (tclass.into_boxed_str(), "<init>".into(), "()V".into()),
                pcount + 1, // receiver + params
                b'V',
                1,
                class_id.as_u32(),
            );
        }
    }

    // Eagerly compile statically-bound callees (class_manager lock released)
    //
    // Every baked direct-call target must stay mapped until this caller
    // is published, because publication is what roots them
    // (`JitCache::prepare_for_publication` -> `_direct_callee_roots`).
    // Dropping the callee `Arc` here would let a concurrent tier-up
    // `put` unmap a body whose address is already baked into the
    // machine code being emitted.
    // Round 9 wave 9 (`evict9`): retire cells planned by `retirable_osr_direct_call`;
    // the artifact owns them (moved into `cm._jit_cycle_cells` below).
    let mut osr_retire_cells: Vec<std::sync::Arc<cratonvm_jit::JitCycleEdgeCell>> = Vec::new();
    let mut baked_callee_pins: Vec<std::sync::Arc<cratonvm_jit::CompiledMethod>> = Vec::new();
    // Round 11 wave 14 (lane `spsync`): the `invokestatic` sites this loop
    // leaves on the dispatch helper, `(pc, param_count, ret, class, method,
    // desc)`. A `static synchronized` callee always lands there
    // (`try_jit_compile_callee` refuses it), and is planned as a caller-held
    // direct CALL once the helper tables exist (see
    // `osr_sync_direct_candidates` below).
    let mut osr_sync_direct_candidates: Vec<(usize, usize, u8, String, String, String)> =
        Vec::new();
    // Round 13 wave 12 (lane osrdoor, W5-1,
    // `r12w5-mega4-megamorphic-call-time-accounting` finding 1): the splices
    // this body makes, planned by the method-entry policy over the statically
    // bound sites above, BEFORE their eager compile. This door used to hand the
    // backend an empty `inline_sites`, so every static helper an OSR loop calls
    // was a full compiled call per iteration. A planned site keeps everything
    // the loop below gives it (a direct bind or the dispatch fallback, and its
    // `JitInvokeInfo`): that is the path the emitter falls back to when it
    // rolls a splice back. The soundness argument is on
    // `cratonvm_jit::plan_osr_door_splices`. No class-manager guard is held
    // here (the resolver and the bind take their own).
    // `CRATONVM_JIT_OSR_SPLICE=0` plans nothing.
    //
    // The pins keep every body a splice's own call bakes mapped until this
    // caller publishes, as `baked_callee_pins` does for the top-level binds.
    let osr_splice_pins: std::cell::RefCell<Vec<std::sync::Arc<cratonvm_jit::CompiledMethod>>> =
        std::cell::RefCell::new(Vec::new());
    let mut osr_splice_plan = if cratonvm_jit::osr_door_splices_enabled() {
        let splice_calls = true;
        let osr_splice_bind = |holder: ClassId,
                               callee_class: &str,
                               callee_method: &str,
                               callee_desc: &str|
         -> Option<(usize, bool)> {
            let (pin, entry, needs_ctx) =
                osr_splice_direct_bind(shared, holder, callee_class, callee_method, callee_desc)?;
            osr_splice_pins.borrow_mut().push(pin);
            Some((entry, needs_ctx))
        };
        let osr_splice_resolver = |callee_class: &str,
                                   callee_method: &str,
                                   callee_desc: &str|
         -> Option<cratonvm_jit::InlineSite> {
            resolve_inline_site(
                shared,
                class_id,
                callee_class,
                callee_method,
                callee_desc,
                if splice_calls {
                    Some(&osr_splice_bind as InlineDirectBind<'_>)
                } else {
                    None
                },
                // This door binds by the constant-pool class and substitutes
                // no owner.
                None,
            )
        };
        let candidates: Vec<cratonvm_jit::OsrSpliceCandidate<'_>> = pending_callee_compiles
            .iter()
            .map(
                |(pc, callee_class, callee_method, callee_desc, _, invoke_kind)| {
                    cratonvm_jit::OsrSpliceCandidate {
                        pc: *pc,
                        invoke_kind: *invoke_kind,
                        class_name: callee_class.as_str(),
                        method_name: callee_method.as_str(),
                        descriptor: callee_desc.as_str(),
                    }
                },
            )
            .collect();
        let mut plan = cratonvm_jit::plan_osr_door_splices(
            code,
            code_len,
            (
                class_name.as_str(),
                method_name.as_str(),
                method_descriptor.as_str(),
            ),
            &osr_exception_table,
            // `backend.precise_exception_frames` below, which clears a plan.
            !osr_exception_table.is_empty(),
            osr_receiver_profile.as_ref(),
            // Round 14 wave 3 (lane calls, OD-1): the loop this trigger fired
            // in is hot evidence even when no branch profile was armed.
            Some(entry_pc),
            &candidates,
            &osr_splice_resolver,
        );
        // Round 16 wave 13 (lane mega): PGO-02 guarded splices of the
        // in-loop virtual and interface sites, with the method-entry door's
        // two resolvers. A call-carrying body is refused, never downgraded
        // (an unbound spliced call is the blind dispatch, 3.5x, `facd6c8ff8`);
        // j27 wave 4 (lane OSRSUPER, `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND`,
        // opt-in): a body the plain resolver refuses is asked again with this
        // door's binder and kept only when every call left in it is a BOUND
        // `super.m()` (`cratonvm_jit::osr_guarded_super_bind`). Same budget,
        // same precise-frames interlock. Soundness on
        // `cratonvm_jit::plan_osr_door_guarded_splices` and that module.
        if !osr_guarded_candidates.is_empty() {
            let class_id_namer = |namer_cid: u32| -> Option<String> {
                let cm = shared.classes.class_manager.read();
                cm.get_class(cratonvm_types::ClassId::new(namer_cid))
                    .map(|c| c.name.to_string())
            };
            let receiver_inline_resolver = |rcv_cid: u32,
                                            cp_class: &str,
                                            name: &str,
                                            desc: &str| {
                resolve_receiver_inline_site(shared, class_id, rcv_cid, cp_class, name, desc, None)
            };
            let receiver_super_bound_resolver =
                |rcv_cid: u32,
                 cp_class: &str,
                 name: &str,
                 desc: &str|
                 -> Option<cratonvm_jit::InlineSite> {
                    resolve_receiver_inline_site(
                        shared,
                        class_id,
                        rcv_cid,
                        cp_class,
                        name,
                        desc,
                        Some(&osr_splice_bind as InlineDirectBind<'_>),
                    )
                };
            // `CRATONVM_JIT_OSR_SPLICE_CALLS=0` binds no call in any splice.
            let super_bound: Option<cratonvm_jit::osr_guarded_super_bind::ReceiverResolve<'_>> =
                if splice_calls {
                    Some(&receiver_super_bound_resolver
                        as cratonvm_jit::osr_guarded_super_bind::ReceiverResolve<'_>)
                } else {
                    None
                };
            let guarded: Vec<cratonvm_jit::OsrSpliceCandidate<'_>> = osr_guarded_candidates
                .iter()
                .map(
                    |(pc, callee_class, callee_method, callee_desc, invoke_kind)| {
                        cratonvm_jit::OsrSpliceCandidate {
                            pc: *pc,
                            invoke_kind: *invoke_kind,
                            class_name: callee_class.as_str(),
                            method_name: callee_method.as_str(),
                            descriptor: callee_desc.as_str(),
                        }
                    },
                )
                .collect();
            cratonvm_jit::plan_osr_door_guarded_splices_bound(
                &mut plan,
                code,
                code_len,
                (
                    class_name.as_str(),
                    method_name.as_str(),
                    method_descriptor.as_str(),
                ),
                &osr_exception_table,
                !osr_exception_table.is_empty(),
                osr_receiver_profile.as_ref(),
                Some(entry_pc),
                &guarded,
                &class_id_namer,
                &receiver_inline_resolver,
                super_bound,
            );
        }
        plan
    } else {
        cratonvm_jit::OsrSplicePlan::default()
    };
    cratonvm_jit::intern_osr_door_splice_targets(
        &mut osr_splice_plan,
        &mut owned_jit_strings2,
        &mut owned_jit_invoke_infos2,
        &mut osr_direct_callee_entries,
    );
    let cratonvm_jit::OsrSplicePlan {
        inline_sites: osr_inline_sites,
        inlined_methods: osr_inlined_methods,
        needs_heap: osr_splice_needs_heap,
        tally: osr_inline_tally,
        guard_variants: osr_guard_variants,
        ..
    } = osr_splice_plan;
    for (ipc, callee_class, callee_method, callee_desc, param_count, site_invoke_kind) in
        pending_callee_compiles
    {
        // `try_jit_compile_callee` resolves the callee BY NAME; a site whose
        // own loader sees another class under that name keeps the dispatch
        // helper. See `jit_callee_name_is_another_loaders_copy` for the H2
        // `TestUpgrade` miscompile binding the other copy here produced.
        let another_loaders_copy = crate::jit::helpers::jit_callee_name_is_another_loaders_copy(
            shared,
            class_id,
            &callee_class,
        );
        // t1 wave 2, lane jit2 (B5, `CRATONVM_JIT_OSR_LATE_STATIC_CELL`): set
        // below when the callee had no compiled body for a compile-ORDER
        // reason (`callee-not-yet-compiled`), the one a late-bind cell serves.
        let mut osr_late_cell_candidate = false;
        let compiled_callee = if another_loaders_copy {
            cratonvm_jit::note_direct_callee_bind_refusal(
                cratonvm_jit::DirectBindRefusal::AnotherLoadersCopy,
            );
            None
        } else {
            // Eager direct-call callee compile — optimized (C2) tier. Resolved
            // through this method's own loader, so a callee name two loaders
            // define compiles the copy this site calls instead of being
            // refused as ambiguous. In place, never handed to the worker
            // (wave 93, lane L7): this OSR body is built once.
            try_jit_compile_callee_from_site_in_place(
                shared,
                class_id.as_u32(),
                &callee_class,
                &callee_method,
                &callee_desc,
                osr_callee_compile_optimize(),
            )
            .inspect(|found| {
                note_osr_c1_callee_publish(
                    shared,
                    class_id,
                    &callee_class,
                    &callee_method,
                    &callee_desc,
                    found,
                );
            })
            .or_else(|| {
                // JIT round 19 wave 5 (lane sync5, SYNC5-3): a synchronized
                // callee with no self-locking body yet.
                osr_synchronized_callee_late_bind(
                    shared,
                    class_id,
                    class_name_arc,
                    method_name_arc,
                    descriptor_arc,
                    &callee_class,
                    &callee_method,
                    &callee_desc,
                )
            })
        };
        // `CRATONVM_DBG_OSR_BIND=1` names, per call site, whether this
        // OSR artifact bound a direct machine-code CALL or fell back to
        // the dispatch helper, and WHICH gate refused. Without it the
        // two outcomes are indistinguishable from outside, and they are
        // ~4.6x apart on a call-dense loop — see
        // `docs/feature-designs/real-jdk-mode.md`.
        let dbg_bind = false;
        if let Some((callee_pin, entry, needs_ctx)) = compiled_callee {
            // Read off the artifact this site would bind, not re-probed by name:
            // for a callee name two loaders define the name-keyed probe reads
            // another copy's row (or none) and would answer `false`.
            let callee_has_indy_trap = callee_pin.has_indy_trap;
            baked_callee_pins.push(callee_pin);
            // The callee class as this method's loader resolves it, asked only
            // when a recursion-cycle member has the callee's names, so another
            // loader's same-named cycle does not keep this site on dispatch
            // (interpreter round i1 wave 12). No class-manager guard is held
            // here.
            let refuse_dispatch = jit_verdicts(shared).direct_call_requires_dispatch_of(
                &callee_class,
                &callee_method,
                &callee_desc,
                || {
                    let cm = shared.classes.class_manager.read();
                    jit_known_class(shared, &cm, &callee_class, class_id)
                },
            );
            let refuse_handlers = osr_callee_bars_direct_call(
                shared,
                class_id,
                &callee_class,
                &callee_method,
                &callee_desc,
            );
            // jit-invokedynamic-groovy-regression fix: never bake a
            // direct machine-code CALL to an indy-trap-bearing
            // artifact — see the matching gate in `callee_compiler`.
            let refuse_indy = callee_has_indy_trap;
            if dbg_bind {
                eprintln!(
                    "[osr-bind] {}.{}{} @pc={} compiled=yes direct={} entry={:#x} requires_dispatch={} declares_handlers={} indy_trap={}",
                    callee_class,
                    callee_method,
                    callee_desc,
                    ipc,
                    !(refuse_dispatch || refuse_handlers.is_some() || refuse_indy),
                    entry,
                    refuse_dispatch,
                    refuse_handlers
                        .map(|r| cratonvm_jit::DIRECT_BIND_REFUSAL_NAMES[r as usize])
                        .unwrap_or("no"),
                    refuse_indy
                );
            }
            if !refuse_dispatch && refuse_handlers.is_none() && !refuse_indy {
                // The OSR door's engagement counter. See
                // `note_osr_direct_callee_bind_hit`: this ladder used to
                // bind and refuse without touching the census at all, so
                // `direct callee binds: 0 bound, 0 left` was printed by a
                // process whose hot loops were doing both.
                cratonvm_jit::note_osr_direct_callee_bind_hit();
                // Round 9 wave 9 (`evict9`): an `invokestatic` bind goes
                // through a retire cell under `CRATONVM_JIT_RETIRE_CELL=1`,
                // so a withdrawn callee is not re-entered by this (long-running)
                // OSR frame; otherwise this is the plain row, unchanged.
                // Every site of an OSR body, not only an in-loop one
                // (interpreter round i1 wave 21, lane L1: the code after the
                // loop runs in the same activation).
                // `num_params` is receiver-EXCLUDED, as before.
                direct_calls2.push((
                    ipc,
                    cratonvm_jit::retirable_osr_direct_call(
                        code,
                        code_len,
                        ipc,
                        site_invoke_kind,
                        entry,
                        needs_ctx,
                        param_count,
                        crate::jit::return_type(&callee_desc),
                        &callee_class,
                        &callee_method,
                        &callee_desc,
                        &mut osr_retire_cells,
                    ),
                ));
                // Keep this callee's body mapped for the lifetime of
                // the artifact we are about to emit — see the
                // declaration of `osr_direct_callee_entries`.
                // `baked_callee_pins` covers bind-to-publication (it is
                // declared above the bind loop and dropped after
                // `put_osr`), which is what keeps THIS tier's baked
                // addresses meaning the same method at publication as
                // they did at bind. The method-entry tiers had no such
                // pin until 2026-09-21 — their bind closure returned a
                // raw address and dropped its `Arc` on the way out — and
                // that is the whole of
                // `jdk-only-string-indexof-int-int-miscompiles-in-the-single-pass-tier`.
                // Do not narrow this to the emit window.
                osr_direct_callee_entries.push((entry, cratonvm_jit::jit_entry_artifact_id(entry)));
                // A direct-bound site ALSO needs its `JitInvokeInfo`.
                // The codegen's direct-call arm reads it to name the
                // callee for the exceptional-return service, and
                // `jit/src/lib.rs`'s single-pass ladder registers one
                // for exactly this reason ("582 sites in one run had no
                // service check; every one of them is a place an orphan
                // can be minted"). This door never did — tolerated
                // while only `invokestatic` was bound here, but an
                // `invokespecial` bind without it makes
                // `compile_with_param_slots` refuse the whole method,
                // so the OSR artifact never materialises and the loop
                // silently runs interpreted forever
                // (`OSR-recompile reason=no-cached-artifact`, repeating).
                push_osr_invoke_info(
                    &mut owned_jit_strings2,
                    &mut owned_jit_invoke_infos2,
                    &mut invoke_info,
                    ipc,
                    (
                        callee_class.clone().into_boxed_str(),
                        callee_method.clone().into_boxed_str(),
                        callee_desc.clone().into_boxed_str(),
                    ),
                    // Receiver-INCLUDED here, unlike `num_params` above.
                    if site_invoke_kind == 3 {
                        param_count
                    } else {
                        param_count + 1
                    },
                    crate::jit::return_type(&callee_desc),
                    site_invoke_kind,
                    class_id.as_u32(),
                );
                continue;
            }
            // Bound-then-refused: the callee compiled, and one of the
            // three gates above sent the site to the helper anyway.
            // Attributed rather than lumped in with "not compiled yet",
            // because the two want opposite fixes -- a compile-ORDER
            // miss is re-bindable, a policy refusal is not.
            let reason = if refuse_dispatch {
                cratonvm_jit::DirectBindRefusal::EagerChainCycle
            } else if let Some(reason) = refuse_handlers {
                reason
            } else {
                cratonvm_jit::DirectBindRefusal::IndyTrap
            };
            cratonvm_jit::note_osr_direct_callee_bind_miss(reason);
            // OSRENTRY6-2: this VM's per-door census.
            note_osr_door_bind_refusal(shared, reason);
        } else {
            cratonvm_jit::note_osr_direct_callee_bind_miss(
                cratonvm_jit::DirectBindRefusal::CalleeNotYetCompiled,
            );
            // OSRENTRY6-2 (JIT round 19 proposal wave 7, lane interp7): the
            // process table above keeps its lump; this VM's per-door census
            // names the reason the slow door's bare `None` stood for.
            let reason = if another_loaders_copy {
                cratonvm_jit::DirectBindRefusal::AnotherLoadersCopy
            } else {
                osr_unbound_callee_reason(
                    shared,
                    class_id,
                    &callee_class,
                    &callee_method,
                    &callee_desc,
                    site_invoke_kind,
                )
            };
            note_osr_door_bind_refusal(shared, reason);
            osr_late_cell_candidate = matches!(
                reason,
                cratonvm_jit::DirectBindRefusal::CalleeNotYetCompiled
            );
            if dbg_bind {
                eprintln!(
                    "[osr-bind] {}.{}{} @pc={} unbound: {}",
                    callee_class,
                    callee_method,
                    callee_desc,
                    ipc,
                    cratonvm_jit::DIRECT_BIND_REFUSAL_NAMES[reason as usize]
                );
            }
            // EAGER19-4 (`CRATONVM_JIT_LATE_BIND_OSR`, opt-in): a compile-ORDER
            // refusal waits for the callee's first compiled serve.
            note_osr_late_bind_waiter(
                shared,
                reason,
                &callee_class,
                &callee_method,
                &callee_desc,
                class_id,
                (class_name_arc, method_name_arc, descriptor_arc),
                entry_pc,
            );
        }

        // Compilation failed, or the callee participates in a recursive
        // compile cycle. Fall back to the guarded dispatch helper.
        if dbg_bind {
            eprintln!(
                "[osr-bind] {}.{}{} @pc={} -> DISPATCH HELPER",
                callee_class, callee_method, callee_desc, ipc
            );
        }
        if site_invoke_kind == 3 {
            osr_sync_direct_candidates.push((
                ipc,
                param_count,
                crate::jit::return_type(&callee_desc),
                callee_class.clone(),
                callee_method.clone(),
                callee_desc.clone(),
            ));
        }
        // JIT round 20, lane dispatch
        // (`r21-single-pass-bound-site-ic-covers-final-owners-only` item 2):
        // the method-entry planner's `sp_bound_site_ic_planned` rule, mirrored
        // for this door. An unbound statically bound instance site
        // (`invokevirtual` / `invokespecial`, kind 1, a constructor only under
        // `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR` (JIT r22), not spliced) whose owner class, as this method's loader resolves it, is
        // `final` gets the MIC/PIC pair; its miss helper's kind-1 route
        // (`jit_invoke_bound_site_mic`) makes the call the blind site made and
        // publishes the body that call entered. Final owner only: one receiver
        // class reaches the site, so the PIC never overflows into the
        // virtual-selection paths. Same switch (`CRATONVM_JIT_SP_BOUND_SITE_IC`).
        // JIT r22 wave 1, lane inline (`CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR`,
        // opt-in; churn-loop J2): an unbound constructor site too, through
        // the same bound PIC. Elided constructors never reach this loop (they
        // are pushed as `Object.<init>` above); `Object.<init>` itself is the
        // codegen's elision and is excluded here.
        let osr_bound_site_ic = site_invoke_kind == 1
            && (callee_method != "<init>"
                || (cratonvm_jit::sp_bound_site_ic_ctor_enabled()
                    && callee_class != "java/lang/Object"
                    && code.get(ipc) == Some(&0xb7)))
            && !osr_inline_sites.contains_key(&ipc)
            && code.get(ipc).is_some_and(|&op| matches!(op, 0xb6 | 0xb7))
            && cratonvm_jit::sp_bound_site_ic_enabled()
            // JIT round 20 wave 11, lane pLate (W18L4-2,
            // `CRATONVM_JIT_BOUND_PIC_ANY_OWNER`): any owner once the slot is
            // a BOUND PIC (marked below), which never overflows into the
            // virtual-selection paths.
            && (cratonvm_jit::bound_pic_any_owner_enabled() || {
                let cm = shared.classes.class_manager.read();
                jit_known_class(shared, &cm, &callee_class, class_id).is_some_and(|cid| {
                    jit_class_is_final_for_typecheck_in(shared, &cm, cid.as_u32())
                })
            });
        if osr_bound_site_ic && dbg_bind {
            eprintln!(
                "[osr-bind] {}.{}{} @pc={} -> bound-site inline cache ({})",
                callee_class,
                callee_method,
                callee_desc,
                ipc,
                if cratonvm_jit::bound_pic_any_owner_enabled() {
                    "bound pic, any owner"
                } else {
                    "final owner"
                }
            );
        }
        let return_type = crate::jit::return_type(&callee_desc);
        // t1 wave 2, lane jit2 (B5 of
        // `jit-proposals/jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`,
        // `CRATONVM_JIT_OSR_LATE_STATIC_CELL`, default ON since j27): an `invokestatic` whose
        // callee was merely not compiled yet gets the method-entry doors'
        // late-bind `JitCycleEdgeCell`. `JitCache::put` fills it once the
        // callee publishes, so this one long activation stops paying
        // `jit_invoke_dispatch` per call from then on. The `JitInvokeInfo`
        // pushed just below is the cell's dispatch fallback (empty or cleared
        // cell), exactly as for the plain dispatch row this replaces. Gates:
        // the method-entry door's (`late_static_cell_site` callers in
        // `cratonvm_jit`), plus the OSR door's own exception-table refusal and
        // a callee the site's class declares itself (the cell is keyed by that
        // class, so an inherited static would never fill it).
        if osr_late_cell_candidate
            && site_invoke_kind == 3
            && code.get(ipc) == Some(&0xb8)
            && !osr_inline_sites.contains_key(&ipc)
            && !direct_calls2.iter().any(|&(p, _)| p == ipc)
            && cratonvm_jit::jit_osr_late_static_cell_enabled()
            && !cratonvm_jit::offload_hook::keeps_dispatch_helper(
                &callee_class,
                &callee_method,
                &callee_desc,
            )
        {
            if let Some(target_cid) = osr_late_static_cell_target(
                shared,
                class_id,
                &callee_class,
                &callee_method,
                &callee_desc,
            ) {
                let cell = cratonvm_jit::JitCycleEdgeCell::new(
                    &callee_class,
                    &callee_method,
                    &callee_desc,
                    target_cid,
                    true,
                );
                direct_calls2.push((
                    ipc,
                    crate::jit::JitDirectCall {
                        entry: std::sync::Arc::as_ptr(&cell) as usize,
                        needs_context: true,
                        // Receiver-EXCLUDED; a static site has none.
                        num_params: param_count,
                        return_type,
                        guard_class_id: cratonvm_jit::JIT_CYCLE_EDGE_CELL_GUARD,
                    },
                ));
                // The artifact owns it (`cm._jit_cycle_cells` below);
                // `JitCache::put_osr` registers it by its target key.
                osr_retire_cells.push(cell);
                if dbg_bind || crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] osr-late-static-cell {class_name}.{method_name}{method_descriptor} @pc={ipc} -> {callee_class}.{callee_method}{callee_desc} class_id={}",
                        target_cid.as_u32()
                    );
                }
            }
        }
        push_osr_invoke_info(
            &mut owned_jit_strings2,
            &mut owned_jit_invoke_infos2,
            &mut invoke_info,
            ipc,
            (
                callee_class.into_boxed_str(),
                callee_method.into_boxed_str(),
                callee_desc.into_boxed_str(),
            ),
            // Receiver-INCLUDED, unlike `JitDirectCall.num_params`.
            if site_invoke_kind == 3 {
                param_count
            } else {
                param_count + 1
            },
            return_type,
            // The site's own kind. Hard-coding 3 here was correct while
            // only `invokestatic` reached this loop; with kind 1
            // admitted it would make the dispatch helper resolve a
            // constructor as a static call and drop the receiver.
            site_invoke_kind,
            class_id.as_u32(),
        );
        if osr_bound_site_ic {
            allocate_bound_site_dispatch_slots(
                ipc,
                &mut mic_slots2,
                &mut owned_mic_slots2,
                &mut pic_slots2,
                &mut owned_pic_slots2,
            );
            // W18L4-2: the slot just allocated is this site's; mark it bound
            // before any code can reach it.
            if cratonvm_jit::bound_pic_any_owner_enabled() {
                if let Some(pic) = owned_pic_slots2.last() {
                    pic.mark_bound_site();
                }
            }
        }
    }

    // Resolve ldc/ldc_w constants. String constants are wired the
    // same way as `jit::try_compile`'s cp_ldc_resolver (boxed text
    // retained via `owned_jit_strings2` → `cm._jit_strings`, codegen
    // materializes through `helpers.ldc_string`). Before this
    // (perf/halfgap-20260717), ANY method containing `ldc "str"`
    // silently failed its OSR-artifact compile and got permanently
    // OSR-denied — a once-invoked method with a hot loop after a
    // string constant (StringRegexOnly.run: `Pattern.compile("(\\d+)")`)
    // then interpreted its entire workload.
    let mut ldc_info2: Vec<(usize, i64)> = Vec::new();
    let mut ldc_string_info2: Vec<(usize, u32, u16)> = Vec::new();
    // Class-`ldc` sites, served at run time by `helpers.ldc_class_cp`.
    // Before this an OSR artifact refused any method containing one.
    let mut ldc_class_info2: Vec<(usize, u32, u16)> = Vec::new();
    // The `ldc`-family pcs whose constant is floating-point. Codegen
    // types these by their consuming opcode; the deopt operand-stack
    // snapshot has none to ask, and without the tag every numeric `ldc`
    // read as `Unsupported` and refused OSR entry for the whole
    // artifact — see `x64::Compiler::ldc_fp_pcs`.
    let mut ldc_fp_pcs2: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    if !scan.ldc_ops.is_empty() {
        let cm_lock = shared.classes.class_manager.read();
        let class = cm_lock.get_class(class_id)?;
        for &(pc, cp_idx) in &scan.ldc_ops {
            let val = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::Integer(v)) => *v as i64, // JVM spec: bounded float-to-long conversion
                Some(ConstantPoolEntry::Float(v)) => {
                    ldc_fp_pcs2.insert(pc);
                    v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                }
                Some(ConstantPoolEntry::StringReference { string_index })
                    if class.constant_pool.get_utf8_wide(*string_index).is_none() =>
                {
                    // The SITE, for the reason the `try_compile`
                    // resolver records one: the recorded resolution is
                    // keyed `(class, cp index)`. `get_utf8` is only the
                    // representability test.
                    match class.constant_pool.get_utf8(*string_index) {
                        Some(_) => {
                            ldc_string_info2.push((pc, class_id.as_u32(), cp_idx));
                            continue;
                        }
                        None => {
                            deny_osr_for_constant_kind(shared, osr_key, pc);
                            return None;
                        }
                    }
                }
                Some(ConstantPoolEntry::ClassReference { .. }) => {
                    ldc_class_info2.push((pc, class_id.as_u32(), cp_idx));
                    continue;
                }
                // wide-string/MethodHandle/MethodType/condy — bail out of OSR.
                _ => {
                    deny_osr_for_constant_kind(shared, osr_key, pc);
                    return None;
                }
            };
            ldc_info2.push((pc, val));
        }
    }

    // Resolve ldc2_w constants
    let mut ldc2w_info2: Vec<(usize, i64)> = Vec::new();
    if !scan.ldc2w_ops.is_empty() {
        let cm_lock = shared.classes.class_manager.read();
        let class = cm_lock.get_class(class_id)?;
        for &(pc, cp_idx) in &scan.ldc2w_ops {
            let val = match class.constant_pool.get(cp_idx)? {
                ConstantPoolEntry::Long(v) => v.get(),
                ConstantPoolEntry::Double(v) => {
                    ldc_fp_pcs2.insert(pc);
                    v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                }
                _ => return None,
            };
            ldc2w_info2.push((pc, val));
        }
    }

    // Resolve new/anewarray info for stackless path (mirrors first JIT site)
    let mut new_info2: Vec<(usize, u32, usize, bool, bool)> = Vec::new();
    let mut anewarray_info2: Vec<(usize, u32)> = Vec::new();
    // Cold-`new` fix — sites this path cannot resolve at compile time.
    // Previously they were baked as the nonsense sentinel entry
    // `(pc, class_id 0, 0 fields, true, true)`: a compiled `new` that
    // allocated against class id 0 rather than the class the bytecode
    // names. They now compile to the CP-indexed helper, which does the
    // real loader-faithful resolution + access check + `<clinit>` at the
    // actual program point, exactly like `Instruction::New`.
    let mut new_deferred2: Vec<(usize, u32, u16)> = Vec::new();
    let mut anewarray_deferred2: Vec<(usize, u32, u16)> = Vec::new();
    let is_real_class2 = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| !c.origin.is_compatibility_stub())
        .unwrap_or(false);
    if is_real_class2 && (!scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty()) {
        let new_class_names: Vec<(usize, u16, Option<String>)> = {
            let cm_lock = shared.classes.class_manager.read();
            if let Some(class) = cm_lock.get_class(class_id) {
                scan.new_ops
                    .iter()
                    .map(|&(pc_new, cp_idx)| {
                        (
                            pc_new,
                            cp_idx,
                            class
                                .constant_pool
                                .get_class_name(cp_idx)
                                .map(|s| s.to_string()),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };
        let arr_class_names: Vec<(usize, u16, Option<String>)> = {
            let cm_lock = shared.classes.class_manager.read();
            if let Some(class) = cm_lock.get_class(class_id) {
                scan.anewarray_ops
                    .iter()
                    .map(|&(pc_arr, cp_idx)| {
                        (
                            pc_arr,
                            cp_idx,
                            class
                                .constant_pool
                                .get_class_name(cp_idx)
                                .map(|s| s.to_string()),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };
        for (pc_new, cp_idx_new, name_opt) in new_class_names {
            if let Some(name) = name_opt {
                let load_result = resolve_cp_class_for_owner(shared, class_id, &name);
                if let Some(target_id) = load_result {
                    // JVMS 5.4.4 / 6.5 `new` access check -- mirrors
                    // the `new_info` site above (same rationale: an
                    // inaccessible `new` site must not be baked into
                    // an inlined JIT fast path; fall back to the
                    // sentinel entry so the interpreter's real check
                    // (`Instruction::New`) is what actually fires).
                    // The interpreter's `new` verdict itself (i10-L2): the raw
                    // `check_class_access` copy here lacked HotSpot's
                    // serialization-accessor exemption, so such a site was
                    // never baked. A missing class is still admitted.
                    let accessible = super::constants::class_constant_fill_admitted(
                        shared, class_id, &name, target_id,
                    );
                    // A class whose initialization is still pending gets the
                    // CP-indexed helper, which initializes; a resolved row is an
                    // allocation with no class-init check, or none at all once
                    // scalar-replaced (wave 4, L7; `jit_new_target_init_pending`).
                    // A never-linked class is linked here when that runs no
                    // Java (wave 8, L5).
                    let init_pending = jit_new_target_pending_after_link(shared, target_id);
                    if accessible && !init_pending {
                        // Round 15 wave 5 (lane osrtier): the real flags, as
                        // the method-entry door (`resolve_jit_new_site`) bakes
                        // them, so an OSR loop's `new` takes the inline TLAB
                        // bump (`emit_resolved_new_alloc`'s `can_inline`)
                        // instead of a `jit_new_object` call per iteration. The
                        // literal `(true, true)` below was measured slower in
                        // 2026-08 (118.0 vs 111.4 ns/op, `CtorShapeRateProbe`)
                        // while `emit_inline_tlab_new` still routed the bump
                        // through the helper; it bumps inline now.
                        // `CRATONVM_JIT_OSR_REAL_NEW_SITE_FLAGS=0` restores it.
                        let (num_fields, needs_post_alloc_init, has_finalizer) = {
                            let cm = shared.classes.class_manager.read();
                            if osr_real_new_site_flags_enabled() {
                                jit_new_site_flags(&cm, target_id)
                            } else {
                                (
                                    cm.get_class(target_id)
                                        .map(|c| c.num_total_fields)
                                        .unwrap_or(0),
                                    true,
                                    true,
                                )
                            }
                        };
                        new_info2.push((
                            pc_new,
                            target_id.as_u32(),
                            num_fields,
                            needs_post_alloc_init,
                            has_finalizer,
                        ));
                    } else {
                        // Inaccessible at compile time — defer, so the
                        // helper raises the JVMS 5.4.4 IllegalAccessError
                        // at the site instead of the old class-0 bake. Or its
                        // initialization is pending, and the helper runs it.
                        new_deferred2.push((pc_new, class_id.as_u32(), cp_idx_new));
                    }
                } else {
                    new_deferred2.push((pc_new, class_id.as_u32(), cp_idx_new));
                }
            }
        }
        for (pc_arr, cp_idx_arr, name_opt) in arr_class_names {
            if let Some(name) = name_opt {
                // An inaccessible component is deferred to the CP helper,
                // which raises the IllegalAccessError (i9-L2).
                if let Some(target_id) =
                    resolve_cp_class_for_owner(shared, class_id, &name).filter(|&id| {
                        super::constants::class_constant_fill_admitted(shared, class_id, &name, id)
                    })
                {
                    anewarray_info2.push((pc_arr, target_id.as_u32()));
                } else {
                    anewarray_deferred2.push((pc_arr, class_id.as_u32(), cp_idx_arr));
                }
            }
        }
    }

    // Prologue argument-slot count. `count_param_slots` returns only
    // the declared descriptor parameters; an instance method also
    // receives the implicit `this` as JVM local 0, so the prologue
    // must load `1 + declared` argument registers. Omitting `this`
    // makes the prologue zero-init local 0 — the OSR-compiled method
    // would then run with a null receiver.
    // Interpreter round i2 wave 62, lane L2: and whether it is known NOT to be
    // `ACC_SYNCHRONIZED` (`false` when the method is not found: unknown), for
    // `BackendRequest::method_known_unsynchronized` below.
    let (osr_method_is_static, osr_method_known_unsynchronized) = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|class| {
            class
                .methods
                .iter()
                .find(|m| {
                    &*m.name == method_name_check && &*m.descriptor == method_descriptor.as_str()
                })
                .map(|m| (m.is_static(), !m.is_synchronized()))
                .unwrap_or((false, false))
        })
        .unwrap_or((false, false));
    let param_slots = crate::jit::count_param_slots(&method_descriptor)
        + if osr_method_is_static { 0 } else { 1 };
    let (param_jvm_slots, param_slot_span) =
        crate::jit::compute_param_jvm_slots(&method_descriptor, osr_method_is_static);
    let helpers = crate::jit::helpers::build_helpers_for(shared);
    // HIB-CV-20 — seed the local-oop dataflow with this method's reference
    // PARAMETERS, exactly as the hot-path `jit::try_compile` does. The
    // legacy `x64::compile` wrapper hardcodes `param_oop_mask = 0`, so an
    // oop parameter (e.g. the `byte[]` of `Arrays.fill([BIIB)V`, JVM local
    // 0) that lives in a callee-saved register across an early safepoint
    // (its pre-loop `rangeCheck(III)V` call) was never marked an oop:
    // `emit_post_safepoint_reload` skipped it, so a moving young GC during
    // that call rewrote the canonical frame slot but left the register
    // stale → the OSR loop then wrote through the pre-GC address → heap
    // corruption / hang (Hibernate XSD-parse, MappingXsdSupport bootstrap).
    // Seed it via `compile_with_param_slots` (legacy `&[]`/`0` slot layout,
    // unchanged) so the reload covers oop params too. Gate on the precise-
    // maps flag so the gate-off path stays byte-identical (mask = 0).
    // Also seed under `deopt_real_enabled()` — see the matching
    // comment at the hot-path `jit::try_compile` seed site
    // (jit/src/lib.rs) for why an unseeded mask makes every deopt at
    // this compile silently double-execute side effects via the
    // whole-method re-run fallback.
    let param_oop_mask = if crate::jit::x64::precise_jit_maps_enabled()
        || crate::jit::x64::moving_young_enabled()
        || cratonvm_jit::deopt_real_enabled()
    {
        crate::jit::compute_param_oop_mask(&method_descriptor, osr_method_is_static)
    } else {
        0
    };
    // Pure-kernel GPR local homes, OSR tier (perf/halfgap-20260717):
    // request them like `jit::try_compile` does for method-entry
    // compiles. The backend engages ONLY when its own purity
    // conditions hold (no invokes/direct-calls/fields/allocs/
    // typechecks/spec-BCE — checked against the exact vectors passed
    // below), keeps reference locals frame-homed, and — unlike the
    // method-entry tier — publishes real OSR entries: the trampoline
    // seeds every local into its `osr_local_assignments` register,
    // which for a kernel body IS its home. Once-invoked kernels
    // (benchArithmetic, matmul) live entirely in this artifact and
    // previously ran memory-homed. Opt out:
    // `CRATONVM_JIT_KERNEL_REG_OSR=0`.
    let mut backend = crate::jit::x64::BackendRequest {
        c1_branch_counters: cratonvm_jit::profile::single_pass_branch_recording_enabled().then(|| {
            shared.jit.profile_store.branch_counters_borrowed(
                class_id.as_u32(),
                method_name_arc,
                descriptor_arc,
                code_len,
            )
        }),
        direct_helpers: crate::jit::helpers::direct_helper_table_for(shared),
        kernel_reg_homes_osr: true,
        // A `goto` back-edge poll may leave the loop through its header's
        // OSR-exit map when the poll's slow path says the interpreter is
        // required (interpreter round i1 wave 12, lane L4;
        // `jvmti_events::compiled_loop_must_leave`). Slow path only; this
        // door's transfer sink takes the exit uncharged.
        mode_exit_polls: true,
        // Unless the exit would show a dead but assigned local as `0` to a
        // debugger (interpreter round i1 wave 19, lane L1).
        debugger_observes_locals: crate::runtime::jvmti::debugger_observes_locals(shared),
        // t1 wave 2, lane M: no elided lock under a monitor observer.
        monitor_observer_at_compile: crate::runtime::jvmti::monitor_observer_may_exist(shared),
        // t1 wave 3, lane M: no scalar replacement under an object observer.
        scalar_observer_at_compile: crate::runtime::jvmti::object_observer_may_exist(shared),
        // Interpreter round i2 wave 62, lane L2: read by an OSR-tier compile
        // only where a post-call exit after a real call inside a splice is
        // admitted (`op_invoke.rs::sp_osr_post_call_splice_exits_enabled`);
        // the tier rule that also reads it admits every OSR-tier site anyway.
        method_known_unsynchronized: osr_method_known_unsynchronized,
        // t1 wave 3, lane X: the self-call arm above marks every raw static
        // self-call (`cratonvm_jit::static_self_call_marker`), so the emitter
        // refuses an `invokestatic` this door left with no registration at
        // all, and stages the callee-deopt service's info for them.
        static_self_calls_registered: cratonvm_jit::static_self_call_marker_enabled(),
        self_call_invoke_info: osr_self_call_invoke_info,
        ..Default::default()
    };
    // The descriptor's per-slot kinds for a primitive parameter the loop never
    // reads, read by the backend only under a debugger (interpreter round i1
    // wave 22, lane L1; `BackendRequest::param_slot_tags`).
    if backend.debugger_observes_locals {
        backend.param_slot_tags =
            crate::jit::param_slot_kind_tags(&method_descriptor, osr_method_is_static);
    }
    // The `jit_lambda_int_to_double` adapter fold, armed by this method's
    // constant pool as at the method-entry door (interpreter round i1 wave 10;
    // it was armed by the `Dist.quantile` / `Dist.cdf` method label). The
    // resolvers are asked only at adapter-shaped runs, and no class-manager
    // guard is held here (each takes its own read).
    {
        let osr_cp = CpResolvers {
            shared,
            class_id,
            substituted_owners: SubstitutedOwners::default(),
            method_name: method_name.as_str(),
            method_descriptor: method_descriptor.as_str(),
            code,
            ldc2w_trace: None,
            invoke_operands: Default::default(),
            first_calls_seen: Default::default(),
            deferred_fields_seen: Default::default(),
        };
        backend.lambda_int_to_double_proven = cratonvm_jit::lambda_int_to_double_runs_proven(
            code,
            code_len,
            &|cp_idx| osr_cp.invoke(cp_idx),
            &|cp_idx| osr_cp.class_name(cp_idx),
        );
    }
    // Round 11 wave 14 (lane `spsync`, page
    // `r11w13-spsync-osr-door-has-no-sync-direct-route`): the caller-held
    // synchronized direct CALL, planned exactly as `build_single_pass_tables`
    // plans it for the other single-pass doors (same gates, same VM answer,
    // same rows). Planned HERE because the gates read this compile's helper
    // tables, and because the `ldc_slots` loop below then gives the mirror
    // row its GC-maintained slot like any class `ldc`. No class-manager
    // guard is held (`sync_direct_target` takes its own). The site's
    // `JitInvokeInfo` is already registered by the dispatch fallback above;
    // the backend needs it for a pc it serves by dispatch (a protected pc, no
    // spill room), where the helper locks by name.
    if osr_door_sync_direct_enabled() && !osr_sync_direct_candidates.is_empty() {
        let osr_cp = CpResolvers {
            shared,
            class_id,
            substituted_owners: SubstitutedOwners::default(),
            method_name: method_name.as_str(),
            method_descriptor: method_descriptor.as_str(),
            code,
            ldc2w_trace: None,
            invoke_operands: Default::default(),
            first_calls_seen: Default::default(),
            deferred_fields_seen: Default::default(),
        };
        for (ipc, param_count, ret, callee_class, callee_method, callee_desc) in
            osr_sync_direct_candidates
        {
            // Round 11 wave 14 (orchestrator): never at a pc a handler of this
            // method protects. `R11TierSyncDoorCompiled` (`drive`'s protected
            // `bumpThrow` call) let the callee's exception escape its handler
            // under `CRATONVM_C2_ACCEPT=never` with the row planned there; the
            // site keeps the dispatch helper, exactly as before wave 14.
            if osr_exception_table
                .iter()
                .any(|e| ipc >= e.start_pc as usize && ipc < e.end_pc as usize)
            {
                continue;
            }
            if !cratonvm_jit::compile_request::single_pass_sync_direct_site_admitted(
                jit_verdicts(shared),
                &helpers,
                &backend.direct_helpers,
                &callee_class,
                &callee_method,
                &callee_desc,
                ret,
            ) {
                continue;
            }
            let Some(cp_idx) = scan
                .invoke_ops
                .iter()
                .find(|&&(p, _, op)| p == ipc && op == 0xb8)
                .map(|&(_, cp_idx, _)| cp_idx)
            else {
                continue;
            };
            let Some((t, pin)) = sync_direct_target(shared, class_id, &osr_cp, cp_idx) else {
                continue;
            };
            if t.entry == 0 || t.instance {
                continue;
            }
            // Mapped and identity-checked until publication, as every
            // baked callee of this door.
            baked_callee_pins.push(pin);
            osr_direct_callee_entries.push((t.entry, cratonvm_jit::jit_entry_artifact_id(t.entry)));
            // The monitor: the Methodref's own `CONSTANT_Class`, resolved
            // through this class's loader, at the INVOKE pc (only the `ldc`
            // arms read this table by pc).
            ldc_class_info2.push((ipc, class_id.as_u32(), t.class_cp_idx));
            direct_calls2.push((
                ipc,
                crate::jit::JitDirectCall {
                    entry: t.entry,
                    needs_context: t.needs_ctx,
                    num_params: param_count,
                    return_type: ret,
                    guard_class_id: cratonvm_jit::JIT_SYNC_DIRECT_GUARD,
                },
            ));
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] sp-sync-direct-call osr {callee_class}.{callee_method}{callee_desc} @pc={ipc} entry={:#x} mirror_slot={:#x}",
                    t.entry, t.mirror_slot
                );
            }
        }
    }
    // Round 13 wave 1 (lane `sync`, page
    // `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow`, option
    // 1): the INSTANCE twin, for the `invokevirtual` sites the loop above left
    // on their inline cache (a `JitInvokeInfo` and a MIC/PIC slot, no row). A
    // `synchronized` instance callee declared by the Methodref's own class C
    // (`sync_direct_target`, the same closed-body contract) is CALLed with this
    // body holding the RECEIVER's monitor (`x64/op_invoke.rs`,
    // `cratonvm_jit::sync_direct_instance_guard`): statically bound when no
    // receiver can select an override (`unoverridable`: C final, or the method
    // final or private), otherwise behind an exact-class guard on C, which CHA
    // must find to be C's whole instantiable subtree. The guard's miss edge
    // (null included) is the site's unchanged inline-cache dispatch, so the row
    // only adds a fast path. `CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE=0` is the
    // kill switch, `CRATONVM_JIT_SYNC_DIRECT=0` the static route's.
    if osr_door_sync_direct_enabled() && cratonvm_jit::sp_sync_direct_instance_enabled() {
        let osr_cp = CpResolvers {
            shared,
            class_id,
            substituted_owners: SubstitutedOwners::default(),
            method_name: method_name.as_str(),
            method_descriptor: method_descriptor.as_str(),
            code,
            ldc2w_trace: None,
            invoke_operands: Default::default(),
            first_calls_seen: Default::default(),
            deferred_fields_seen: Default::default(),
        };
        for &(ipc, cp_idx, op) in &scan.invoke_ops {
            if op != 0xb6
                || direct_calls2.iter().any(|&(p, _)| p == ipc)
                || !mic_slots2.iter().any(|&(p, _)| p == ipc)
                || !invoke_info.iter().any(|&(p, _)| p == ipc)
                || osr_exception_table
                    .iter()
                    .any(|e| ipc >= e.start_pc as usize && ipc < e.end_pc as usize)
            {
                continue;
            }
            let Some((callee_class, callee_method, callee_desc)) = osr_cp.invoke(cp_idx) else {
                continue;
            };
            if callee_method == "<init>" || callee_class == "java/lang/Object" {
                continue;
            }
            let ret = crate::jit::return_type(&callee_desc);
            if !cratonvm_jit::compile_request::single_pass_sync_direct_site_admitted(
                jit_verdicts(shared),
                &helpers,
                &backend.direct_helpers,
                &callee_class,
                &callee_method,
                &callee_desc,
                ret,
            ) {
                continue;
            }
            let Some((t, pin)) = sync_direct_target(shared, class_id, &osr_cp, cp_idx) else {
                continue;
            };
            if t.entry == 0 || !t.instance {
                continue;
            }
            let exact_class_id = if t.unoverridable {
                0
            } else {
                match resolve_unique_concrete_bind(
                    shared,
                    class_id,
                    &callee_class,
                    &callee_method,
                    &callee_desc,
                ) {
                    // The implementor IS the site's static type: the only class
                    // whose instances select C's own declaration.
                    Some(bind)
                        if bind.implementor_class_id != 0
                            && bind.implementor_class_id == bind.static_type_class_id =>
                    {
                        bind.implementor_class_id
                    }
                    _ => continue,
                }
            };
            let Some(guard) = cratonvm_jit::sync_direct_instance_guard(exact_class_id) else {
                continue;
            };
            // Mapped and identity-checked until publication, as every baked
            // callee of this door.
            baked_callee_pins.push(pin);
            osr_direct_callee_entries.push((t.entry, cratonvm_jit::jit_entry_artifact_id(t.entry)));
            direct_calls2.push((
                ipc,
                crate::jit::JitDirectCall {
                    entry: t.entry,
                    needs_context: t.needs_ctx,
                    // Receiver-EXCLUDED, as on every instance row.
                    num_params: crate::jit::count_param_slots(&callee_desc),
                    return_type: ret,
                    guard_class_id: guard,
                },
            ));
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] sp-sync-direct-call osr {callee_class}.{callee_method}{callee_desc} @pc={ipc} entry={:#x} receiver-monitor exact_class_id={exact_class_id}",
                    t.entry
                );
            }
        }
    }
    // GC-maintained `ldc` constant slots (ldc7 request H, wired round 9
    // wave 7b). No class-manager guard is held here (the ldc scan's ended
    // with its block), which `compiled_ldc_slot_for` requires. A `0` answer
    // (kill switch, redefinition, unresolved site) leaves the site on its
    // helper CALL.
    for &(_, holder, cp_idx) in ldc_string_info2.iter().chain(ldc_class_info2.iter()) {
        let slot_addr =
            crate::jit::helpers::compiled_ldc_slot_for(shared, ClassId::new(holder), cp_idx);
        if slot_addr != 0 {
            backend.ldc_slots.insert((holder, cp_idx), slot_addr);
        }
    }
    // ── The RBC.6b lift's three staged requests ──────────────────
    //
    // `jit::try_compile` has always staged these for a method-entry
    // compile; this door never did, which is the whole of why an OSR
    // artifact "NEVER carries handler ranges" and why RBC.6b refused
    // every method with an exception table. Staged here, at the same
    // point `try_compile` stages them — after every early return above,
    // so a refused attempt cannot leak a request into the next method
    // compiled on this worker thread, and all three travel on this
    // compile's `BackendRequest`.
    //
    // Set unconditionally, including the empty-table case, so the
    // request state this compile runs under is decided HERE rather than
    // inherited from whatever ran before it.
    //
    //   * precise frames — every invoke inside a protected range
    //     publishes a reason-9 (`PendingException`) deopt frame keyed
    //     on the THROWING bci. That bci is the whole point: the live
    //     interpreter frame is parked at the stale pre-OSR back-edge
    //     pc, so without it the handler `[start_pc, end_pc)` test runs
    //     against a pc that has nothing to do with where the throw
    //     happened. `admit_osr_exception_frame` consumes it.
    //
    //     The method-entry path asks for these only when a handler
    //     reads a non-parameter local (`local_handler_reads_unsafe_
    //     local`), because there it is a cost paid to keep a compile
    //     that would otherwise be refused. Here it is asked for EVERY
    //     method with a table: the alternative is not a coarser frame,
    //     it is no bci at all.
    //
    //   * protected ranges — suppresses the sibling tail-call for a
    //     call inside a `try`. A tail-call tears this frame down and
    //     `JMP`s into the callee, so a callee exception unwinds past a
    //     handler that was supposed to catch it — and for an OSR frame
    //     that frame is the live interpreter one.
    //
    //   * pending exception ranges — handler entry edges are invisible
    //     to the backend's bytecode branch decoding, so
    //     `find_bypassable_loop_headers` needs them or a handler
    //     entered from outside a loop lands in the body without running
    //     its pre-header.
    backend.precise_exception_frames = !osr_exception_table.is_empty();
    // The protected ranges, the handler edges and (interpreter round i1 wave
    // 27, lane L2) which entries are catch-alls, where the taken-monitor
    // analysis stops a pc's handler search: the one staging every
    // single-pass door shares since wave 28 (lane L2;
    // `BackendRequest::stage_exception_table`).
    backend.stage_exception_table(&osr_exception_table);
    // The StoreLoad fence after each volatile `putfield`; collected with
    // `field_info` above.
    backend.volatile_field_pcs = osr_volatile_field_pcs;
    // The deferred field sites collected with the field tables above
    // (interpreter round i4 wave 93, lane L2; empty unless
    // `CRATONVM_JIT_DEFERRED_FIELD_SITES` is on). The backend lifts them
    // through its loop rewriter and refuses them from every hoist loop, as
    // for a method-entry body.
    if !osr_deferred_field_sites.is_empty() && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr deferred-field-sites {}.{}{}: {} site(s) at pcs {:?}",
            class_name,
            method_name,
            method_descriptor,
            osr_deferred_field_sites.len(),
            osr_deferred_field_sites
                .iter()
                .map(|&(pc, _)| pc)
                .collect::<Vec<_>>(),
        );
    }
    // Interpreter round i5 wave 102, lane L2: whether to arm the OSR re-tier
    // once the body is published (`arm_osr_deferred_field_site_retier`).
    let osr_deferred_any = !osr_deferred_field_sites.is_empty();

    backend.deferred_field_sites = osr_deferred_field_sites;
    // The one-shot first-execution guards of the call sites no door has
    // resolved yet (interpreter round i1 wave 50, lane L1;
    // `cratonvm_jit::JitFirstCallCell`). This door binds its sites from the
    // constant pool without resolving them, so a site first run in the OSR
    // body was never recorded; the guard records it there.
    backend.first_call_cells = scan
        .invoke_ops
        .iter()
        .filter(|&&(_, _, op)| matches!(op, 0xb6..=0xb9))
        .filter_map(|&(pc, cp_idx, _)| {
            let cell = first_call_cell_for(shared, class_id, cp_idx);
            (cell != 0).then_some((pc, cell))
        })
        .collect();
    // ── Compiled local exception handlers, OSR tier ─────────────────
    //
    // The same table a fourth time, with catch TYPES, so this artifact
    // can run its own `catch` blocks instead of leaving compiled code
    // for every one of them.
    //
    // This door matters MORE than the method-entry one, not less: a
    // `@Test` body is invoked once, so OSR is its only route out of the
    // interpreter, and `HttpHeaderValidationUtilTest`'s two exhaustive
    // loops — the class this feature was written for — are exactly that
    // shape. Staging it only in `jit::try_compile` would have left the
    // feature structurally inert on the population it exists for.
    //
    // Staged only when every catch type resolves: a partial table would
    // make a site answer "propagate" where the real table has a match —
    // a slower answer arrived at by a lie.
    if cratonvm_jit::local_handlers_enabled() && !osr_exception_table.is_empty() {
        let cm_lock = shared.classes.class_manager.read();
        let table = cm_lock.get_class(class_id).and_then(|class| {
            let mut rows: Vec<(usize, usize, usize, &'static str)> =
                Vec::with_capacity(osr_exception_table.len());
            for e in osr_exception_table.iter() {
                let name: &'static str = if e.catch_type == 0 {
                    ""
                } else {
                    match class.constant_pool.get_class_name(e.catch_type) {
                        Some(n) => cratonvm_jit::intern_catch_type_name(n),
                        None => return None,
                    }
                };
                rows.push((
                    // Widening: classfile pcs are u16.
                    e.start_pc as usize,
                    e.end_pc as usize,
                    e.handler_pc as usize,
                    name,
                ));
            }
            Some(rows)
        });
        drop(cm_lock);
        if let Some(table) = table {
            backend.local_handler_table = table;
            backend.local_handler_class = class_id.as_u32();
        }
    }
    // This artifact's install epoch was stamped by the `compile_gate`
    // admission at the top of this closure — before the class loading
    // and constant-pool resolution above, not here. A witness opened at
    // this line covered only the backend call, so a redefinition that
    // landed while the resolvers ran produced a body the install
    // barrier could not tell from a current one.
    osr_stage("backend");
    // Round 13 wave 12 (lane osrdoor, W5-1): the backend is asked through a
    // closure so that a refusal of a body that splices something can be asked
    // again with no splices before it becomes the permanent bail below. A bail
    // here bail-lists the method for every door, and this door is the only
    // route out of the interpreter for a `main` or `@Test` loop, so a splice
    // must never be able to cost the loop its compiled body. The tables are
    // cloned per ask (small, once per OSR compile).
    let osr_splices_planned = !osr_inline_sites.is_empty();
    // addr-keyed: not an address -- bytecode pcs of the method being compiled.
    let osr_backend = |inline_sites: std::collections::HashMap<usize, cratonvm_jit::InlineSite>,
                       guard_variants: std::collections::HashMap<
        usize,
        Vec<(u32, cratonvm_jit::InlineSite)>,
    >| {
        crate::jit::x64::compile_with_param_slots(
            &admission,
            &code,
            code_len,
            param_slots,
            max_locals, // Widening: u16 to usize (OSR target's max_locals, frame-free)
            // A BRIDGED indy site calls a helper, and every helper call in
            // this backend loads the hidden `SharedVm` pointer out of the
            // frame slot `heap_local_offset` names — a slot that only
            // EXISTS when `needs_heap` is set. `scan.needs_heap` does not
            // know about it: an `invokedynamic` used to lower to a trap
            // that calls nothing. Same hazard, same one-line remedy, as the
            // spliced-call site records at `needs_heap = has_field_ops ||
            // …` below; an OSR body has usually asked for the slot for some
            // other reason, which is precisely why this was invisible for
            // as long as the concat bridge was OSR-only. A spliced body that
            // touches fields or calls needs it the same way (round 13 wave 12).
            scan.needs_heap
                || osr_splice_needs_heap
                || indy_info
                    .iter()
                    .any(|&(_, _, _, _, bridge_site)| bridge_site != 0),
            mna_info.clone(),
            field_info.clone(),
            typecheck_info.clone(),
            static_field_info.clone(),
            new_info2.clone(),
            new_deferred2.clone(),
            anewarray_info2.clone(),
            anewarray_deferred2.clone(),
            invoke_info.clone(),
            direct_calls2.clone(),
            mic_slots2.clone(),
            pic_slots2.clone(),
            ldc_info2.clone(),
            ldc_string_info2.clone(), // wired (perf/halfgap-20260717) — see the
            // resolve block above; bytes owned by owned_jit_strings2 →
            // cm._jit_strings, same retention as the invoke-info strs.
            ldc_class_info2.clone(),
            ldc2w_info2.clone(),
            ldc_fp_pcs2.clone(),
            std::collections::HashMap::new(), // branch_hints
            std::collections::HashMap::new(), // loop_unroll_hints
            &helpers,
            scan.non_escaping_new.clone(), // escape analysis results
            // Round 13 wave 12 (lane osrdoor, W5-1): the statically bound
            // splices planned above (empty under `CRATONVM_JIT_OSR_SPLICE=0`,
            // and on the retry below).
            inline_sites,
            // Round 16 wave 13 (lane mega): the guarded variants planned above
            // (empty unless `CRATONVM_JIT_OSR_SPLICE_GUARDED=1`, and on the
            // retry below).
            guard_variants,
            // string_layout — the SAME value the matcher above used, so a
            // registered String sentinel is never one this codegen cannot
            // emit. Was `None` ("String intrinsics land in a later wave"),
            // which made every String intrinsic inert in OSR bodies.
            osr_string_layout,
            &param_jvm_slots,
            param_slot_span,
            param_oop_mask,
            compact_field_info.clone(),
            // method_key — needed so OSR-artifact deopt snapshots carry
            // this method's identity (the resume sinks verify a stashed
            // frame's `method_key` before resuming it; an empty key would
            // force every OSR-frame deopt onto the imprecise safe-reject
            // path). Also enables the per-bci de-spec consult below.
            &format!("{class_name}.{method_name}:{method_descriptor}"),
            // This VM's de-spec registry — per VM, never another VM's.
            Some(&shared.jit.despec_registry),
            indy_info.clone(),
            Some(elidable_init_pcs.clone()),
            backend.clone(),
        )
    };
    let mut cm = osr_backend(osr_inline_sites, osr_guard_variants);
    // The splices are kept only when the body that carries them compiled.
    let osr_splices_kept = osr_splices_planned && cm.is_some();
    if cm.is_none() && osr_splices_planned {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr-splice-retry {class_name}.{method_name}{method_descriptor} \
                 entry_pc={entry_pc}: the backend refused the body with its splices; asking again \
                 without them",
            );
        }
        cm = osr_backend(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        );
    }
    let Some(mut cm) = cm else {
        // Record the same verdict as `jit::try_compile`: an unencodable
        // backend failure is permanent, while a measured buffer shortfall
        // keeps its bounded larger-buffer retry. Permanently disabling every
        // failed OSR compile here used to strand otherwise compilable hot
        // methods (including Netty's trie builder) in the interpreter.
        //
        // Record the REASON as well, which this door never did.
        // `jit::try_compile` records one, so `CRATONVM_DBG=jit-method-stats`
        // can say WHY a permanently uncompilable method is stuck; an OSR
        // bail reached the same table as `reason=unrecorded`. That is the
        // bail that matters most — this door compiles a `@Test` method's
        // hot loop, and a method denied here runs its whole life in the
        // interpreter with no other diagnostic. See
        // osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md,
        // which took a six-arm shape bisect to find for exactly this reason.
        jit_verdicts(shared).mark_bail_listed_with_site_and_metrics(
            class_id,
            &class_name,
            &method_name,
            &method_descriptor,
            i32::try_from(code_len).unwrap_or(i32::MAX),
            osr_decision_started
                .map(|t| u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX))
                .unwrap_or(0),
        );
        // r10 lane `obs`: the ONLY site that can raise this row. `OSR_EVENTS`
        // has carried `osr_compile_declined` — with a doc comment describing
        // precisely this arm, "the OSR compile produced no artifact at all" —
        // while nothing in the tree incremented it, so `osr_counts()` reported
        // zero declines forever and read as "this never happens". That is the
        // failure mode a mute instrument has: it does not look broken, it looks
        // like evidence.
        cratonvm_jit::metrics::record_osr_event("osr_compile_declined");
        return None;
    };
    cm.compiled_via_osr = true;
    cm.osr_compiled_entry_pc = Some(entry_pc);
    // Round 15 wave 3 (lane vh3): the thin `VarHandle` binds' dependency, in
    // both compiles above (the retry keeps `direct_calls2`).
    cm.inlined_methods.extend(osr_vh_thin_bind_deps);
    // Round 13 wave 12 (lane osrdoor, W5-1): what the body spliced, so a
    // redefinition or unload of a spliced class withdraws it (`put_osr`
    // records these and checks them against the invalidation log as `put`
    // does), and the decision totals for the compile report. Nothing when the
    // retry above compiled the body without its splices.
    if osr_splices_kept {
        cm.inlined_methods.extend(osr_inlined_methods);
        cm.inline_tally = osr_inline_tally;
    }
    // RBC.6's residual (see the gate): an `athrow` method with a table is not
    // entered at a pc a protected range covers. Withdrawn here, before
    // publication, so every door that later asks `can_osr_enter` /
    // `validate_osr_entry` of this artifact sees it; the compile itself is
    // entry-pc-independent and still serves every other loop header.
    if rbc6_withdraw_protected_entries {
        if let Some(table) = cm.osr_pc_to_native.as_mut() {
            // MON10-1: under `CRATONVM_JIT_OSR_SYNC_BLOCK_ENTRY` javac's
            // monitor-release catch-any rows withdraw nothing (empty flags,
            // i.e. every row withdrawing, with the switch off).
            let withdrawn = withdraw_osr_entries_inside_protected_ranges_except(
                table,
                &osr_exception_table,
                |i| {
                    backend.precise_exception_frames
                        || osr_monitor_release_rows.get(i).copied().unwrap_or(false)
                },
            );
            if withdrawn > 0 && crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr RBC.6 {class_name}.{method_name}{method_descriptor}: \
                     athrow + {} handler row(s); {withdrawn} OSR entry pc(s) inside a protected \
                     range withdrawn, entries outside every range kept",
                    osr_exception_table.len(),
                );
            }
        }
    }
    // Hand the baked callee entries to `put_osr` ->
    // `prepare_for_publication`, which upgrades each to a strong `Arc`
    // in `_direct_callee_roots`. Without this the OSR body's direct
    // CALLs are unrooted; see the declaration above.
    // By entry, and deduped by entry alone: the same address baked twice in
    // one compile is one pin and one identity. Two DIFFERENT ids for one
    // address would mean the buffer was recycled mid-compile, and keeping the
    // first makes publication refuse — which is the outcome we want.
    osr_direct_callee_entries.sort_unstable_by_key(|(entry, _)| *entry);
    osr_direct_callee_entries.dedup_by_key(|(entry, _)| *entry);
    cm._direct_callee_entries = osr_direct_callee_entries.iter().map(|(e, _)| *e).collect();
    cm._direct_callee_expected = osr_direct_callee_entries;
    cm._jit_strings = owned_jit_strings2;
    cm._jit_invoke_infos = owned_jit_invoke_infos2;
    // Round 15 wave 6 (lane vh8): the code bakes these records' addresses.
    cm._jit_vh_judged_sites.extend(osr_vh_judged_sites);
    cm._jit_mic_slots.extend(owned_mic_slots2);
    cm._jit_pic_slots.extend(owned_pic_slots2);
    cm._jit_cycle_cells.extend(osr_retire_cells);
    // `CRATONVM_DBG_JIT_CODE=<substring>` dumped single-pass and IR
    // bodies but never an OSR one, so the artifact that actually runs a
    // `@Test` method's hot loop was the one body no diff could see.
    // That is precisely the artifact the compile-order question turns
    // on — see
    // docs/feature-designs/real-jdk-mode.md,
    // where the callee's body was byte-comparable between the fast and
    // slow arms and the CALLER's was not observable at all. Same
    // format as the other two dumps, tagged `backend=osr`.
    if let Ok(want) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        let full = format!("{class_name}.{method_name}{method_descriptor}");
        if full.contains(&want) {
            let slice = cm._buffer_slice_for_debug();
            let mut hex = String::new();
            for b in slice {
                hex.push_str(&format!("{:02x}", b));
            }
            eprintln!(
                "[JIT_CODE] backend=osr {} entry={:p} entry_pc={} len={}
{}",
                full,
                cm.entry_ptr(),
                entry_pc,
                slice.len(),
                hex
            );
        }
    }
    stamp_compilation_epoch(
        shared,
        &class_name_arc,
        &method_name_arc,
        &descriptor_arc,
        &mut cm,
    );
    // `put_osr` returns nothing and silently DROPS the body when its
    // publication epoch is stale, a dependency was invalidated mid-compile, or
    // `prepare_for_publication` refuses it. The `get_osr` below then answers
    // with whatever OSR body was cached before — typically one compiled for a
    // different loop header, or the despec-stale body the door just declined
    // to reuse — and the door would enter that, or memo "cannot enter at this
    // pc" (`mark_osr_entry_rejected`) as a lasting verdict about a body that
    // was never this compile's. Only the body THIS call published counts;
    // identified by its code address, which the move into the cache's `Arc`
    // does not change and which no other live body can share. A refusal is
    // transient: `None`, no memo, the frame's backoff schedule retries.
    let published_entry = cm.entry_ptr() as usize;
    let mut jit_cache = shared.jit.jit_cache.write();
    // Round 15 wave 3 (VH3-1, `r15w3-vh4-thin-bound-publication-doors-patch`):
    // a thin-`VarHandle`-bound body built before the first exact mint is not
    // published; the read-back below reports the refusal as transient.
    if !crate::jit::helpers::varhandle_thin_bound_artifact_is_stale(shared, &cm) {
        jit_cache.put_osr(
            class_name_arc.clone(),
            method_name_arc.clone(),
            descriptor_arc.clone(),
            class_id,
            cm,
        );
    }
    let published = jit_cache
        .get_osr(&class_name_arc, &method_name_arc, &descriptor_arc, class_id)
        .filter(|c| c.entry_ptr() as usize == published_entry);
    // lock released at scope exit
    // JIT round 21 (lane tcshape, `CRATONVM_JIT_DEFERRED_FIELD_RECOMPILE`):
    // the published body waits on its deferred field sites and on the
    // not-yet-initialized classes it reads statics of through the helper.
    if let Some(body) = published.as_ref() {
        note_osr_init_waiters(
            shared,
            class_id,
            (class_name_arc, method_name_arc, descriptor_arc),
            entry_pc,
            &body.static_init_classes,
        );
    }
    if published.is_none() {
        // Counted with the backend declines: in both cases this compile left
        // no artifact behind. (`record_osr_event` ignores a name absent from
        // `OSR_EVENTS`, so a new row would need the jit crate.)
        cratonvm_jit::metrics::record_osr_event("osr_compile_declined");
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] OSR-publish-refused {}.{}{} entry_pc={} (stale epoch, \
                 invalidated dependency, or prepare_for_publication)",
                *class_name_arc, *method_name_arc, *descriptor_arc, entry_pc
            );
        }
        cratonvm_jit::emit_direct_door_jit_decision(
            &class_name,
            &method_name,
            &method_descriptor,
            cratonvm_jit::compile_gate::CompileDoor::Osr,
            cratonvm_jfr::jit_decision::CompileOutcome::Refused,
            "osr-publish-refused",
            None,
            code_len,
            osr_decision_started,
        );
    } else {
        cratonvm_jit::emit_direct_door_jit_decision(
            &class_name,
            &method_name,
            &method_descriptor,
            cratonvm_jit::compile_gate::CompileDoor::Osr,
            cratonvm_jfr::jit_decision::CompileOutcome::SinglePass,
            "compiled",
            None,
            code_len,
            osr_decision_started,
        );
    }
    // Interpreter round i5 wave 102, lane L2
    // (`CRATONVM_JIT_OSR_DEFERRED_FIELD_SITE_RETIER`, default OFF): a published
    // body that deferred a field site is re-tiered once that site links. The
    // cache lock is released first; the arm takes only the bridge's map.
    if osr_deferred_any {
        if let Some(body) = published.as_deref() {
            arm_osr_deferred_field_site_retier(shared, class_id, osr_key, body, entry_pc);
        }
    }
    published
}

