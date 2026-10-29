pub const DEOPT_REASON_CLASS_CHECK: i64 = 1;
pub const DEOPT_REASON_BOUNDS_CHECK: i64 = 2;
pub const DEOPT_REASON_DIV_BY_ZERO: i64 = 3;
pub const DEOPT_REASON_RECEIVER_TYPE_CHANGED: i64 = 4;
pub const DEOPT_REASON_CLASS_LOADING: i64 = 5;
pub const DEOPT_REASON_UNCOMMON_TRAP: i64 = 6;
pub const DEOPT_REASON_SPECULATION_FAILED: i64 = 7;
pub const DEOPT_REASON_UNREACHED_CODE: i64 = 8;

/// Deopt action codes returned from `jit_uncommon_trap`.
pub const DEOPT_ACTION_REINTERPRET: i64 = 0;
pub const DEOPT_ACTION_RECOMPILE: i64 = 1;
pub const DEOPT_ACTION_BLACKLIST: i64 = 2;

pub fn reason_code_to_deopt_reason(code: i64) -> cratonvm_jit::deopt::DeoptReason {
    match code {
        DEOPT_REASON_NULL_CHECK => cratonvm_jit::deopt::DeoptReason::NullCheck,
        DEOPT_REASON_CLASS_CHECK => cratonvm_jit::deopt::DeoptReason::ClassCheck,
        DEOPT_REASON_BOUNDS_CHECK => cratonvm_jit::deopt::DeoptReason::BoundsCheck,
        DEOPT_REASON_DIV_BY_ZERO => cratonvm_jit::deopt::DeoptReason::DivByZero,
        DEOPT_REASON_RECEIVER_TYPE_CHANGED => cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged,
        DEOPT_REASON_CLASS_LOADING => cratonvm_jit::deopt::DeoptReason::ClassLoading,
        DEOPT_REASON_UNCOMMON_TRAP => cratonvm_jit::deopt::DeoptReason::UncommonTrap,
        DEOPT_REASON_SPECULATION_FAILED => cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
        DEOPT_REASON_UNREACHED_CODE => cratonvm_jit::deopt::DeoptReason::UnreachedCode,
        _ => cratonvm_jit::deopt::DeoptReason::UncommonTrap,
    }
}

/// The method-stats `reason=` of a method retired by its deoptimization
/// budget (`DeoptAction::MakeNotCompilable`), named by the reason of the trap
/// that crossed it (JIT round 23 wave 5, lane L3). A static string because the
/// verdict registry keeps `&'static str` sites.
pub(crate) fn deopt_budget_refusal_label(
    reason: cratonvm_jit::deopt::DeoptReason,
) -> &'static str {
    use cratonvm_jit::deopt::DeoptReason as R;
    match reason {
        R::NullCheck => "deopt-budget-not-compilable/NullCheck",
        R::ClassCheck => "deopt-budget-not-compilable/ClassCheck",
        R::BoundsCheck => "deopt-budget-not-compilable/BoundsCheck",
        R::DivByZero => "deopt-budget-not-compilable/DivByZero",
        R::ReceiverTypeChanged => "deopt-budget-not-compilable/ReceiverTypeChanged",
        R::ClassLoading => "deopt-budget-not-compilable/ClassLoading",
        R::UninitializedAccess => "deopt-budget-not-compilable/UninitializedAccess",
        R::TransferToInterpreter => "deopt-budget-not-compilable/TransferToInterpreter",
        R::UncommonTrap => "deopt-budget-not-compilable/UncommonTrap",
        R::SpeculationFailed => "deopt-budget-not-compilable/SpeculationFailed",
        R::NotCompiled => "deopt-budget-not-compilable/NotCompiled",
        R::UnreachedCode => "deopt-budget-not-compilable/UnreachedCode",
        R::OsrExit => "deopt-budget-not-compilable/OsrExit",
        R::PendingException => "deopt-budget-not-compilable/PendingException",
        R::UnstableIf => "deopt-budget-not-compilable/UnstableIf",
    }
}

/// Orchestrates deoptimization: records the event, invalidates compiled code,
/// and queues recompilation based on the deopt log's recommended action.
pub struct DeoptimizationController;

impl DeoptimizationController {
    /// Execute a full deoptimization cycle for a method.
    ///
    /// 1. Record the deopt event in the deopt log
    /// 2. Invalidate the compiled method in the JIT cache
    /// 3. Notify the tiered compilation manager
    /// 4. If receiver type changed, invalidate via the invalidation manager
    /// 5. Return the recommended action
    ///
    /// Resolves the class BY NAME, which answers `ClassId(0)` when two loaders
    /// define it; a caller that knows the trapping frame's class identity
    /// should call [`Self::deoptimize_in`] instead
    /// (r11-tier-deopt-tiering-key-drops-the-class-id).
    pub fn deoptimize(
        vm: &SharedVm,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        reason: cratonvm_jit::deopt::DeoptReason,
        bci: u32,
    ) -> cratonvm_jit::deopt::DeoptAction {
        let class_id = vm
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(class_name)
            .unwrap_or(cratonvm_types::ClassId::new(0));
        Self::deoptimize_in(
            vm,
            class_id,
            class_name,
            method_name,
            descriptor,
            reason,
            bci,
        )
    }

    /// [`Self::deoptimize_in`] when the caller knows the declaring class id,
    /// [`Self::deoptimize`]'s by-name lookup when it passes id 0 (a caller
    /// that never resolved one; unit tests).
    pub(crate) fn deoptimize_or_by_name(
        vm: &SharedVm,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        reason: cratonvm_jit::deopt::DeoptReason,
        bci: u32,
    ) -> cratonvm_jit::deopt::DeoptAction {
        if class_id.as_u32() == 0 {
            Self::deoptimize(vm, class_name, method_name, descriptor, reason, bci)
        } else {
            Self::deoptimize_in(
                vm,
                class_id,
                class_name,
                method_name,
                descriptor,
                reason,
                bci,
            )
        }
    }

    /// [`Self::deoptimize`] for a caller that holds the trapping frame's
    /// declaring `class_id` (`cached.declaring_class_id` at the `jit_bridge`
    /// doors and `resume_deopted_body`, the OSR door's `class_id`).
    ///
    /// The id keys three things a by-name lookup gets wrong when two loaders
    /// define the class: the tiered manager's `MethodKey` (a class-id-0 key
    /// misses the real state, which then never drops to `Interpreter`, charges
    /// no trap and gets a second id-0 twin from the eager re-queue), the JIT
    /// cache eviction, and the runtime de-speculation mark.
    pub fn deoptimize_in(
        vm: &SharedVm,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        reason: cratonvm_jit::deopt::DeoptReason,
        bci: u32,
    ) -> cratonvm_jit::deopt::DeoptAction {
        // Interpreter round i1 wave 15, lane L3: an `OsrExit` taken because the
        // method must now run interpreted is an agent's request, not a failed
        // speculation. Nothing is recorded, evicted or escalated; see
        // `interpreter::exit_left_for_the_interpreter`, which every stash sink
        // also asks before its own per-bci de-speculation.
        if crate::runtime::interpreter::exit_left_for_the_interpreter(
            vm,
            reason,
            class_id,
            method_name,
            descriptor,
        ) {
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] {class_name}.{method_name}:{descriptor} bci={bci}: \
                     left for the interpreter-only mode, not charged"
                );
            }
            return cratonvm_jit::deopt::DeoptAction::Reinterpret;
        }
        // JIT round 21 lane indylate (`CRATONVM_JIT_INDY_LATE_LINK_TRAP_RECOMPILE`):
        // the single-pass tier's `invokedynamic` trap is charged
        // `UnreachedCode`, whose policy is `MakeNotCompilable` on the first
        // trap -- right for a bootstrap no bridge serves, wrong for an
        // instruction that was merely unlinked when the body was built and
        // that the linked bridge serves now: the method then ran interpreted
        // for good (`R21IndyConstLateBind` const-late-bind 5297 ns). Such a
        // trap is charged `SpeculationFailed` (evict, recompile; the count
        // escalation still bounds it). See `jit_bridge::indy_late_link_trap_reprieve`.
        let reason = if reason == cratonvm_jit::deopt::DeoptReason::UnreachedCode
            && crate::runtime::interpreter::jit_bridge::indy_late_link_trap_reprieve(
                vm,
                class_id,
                class_name,
                method_name,
                descriptor,
                bci,
            ) {
            cratonvm_jit::deopt::DeoptReason::SpeculationFailed
        } else {
            reason
        };
        // Build method key for deopt log
        let method_key = format!("{}.{}:{}", class_name, method_name, descriptor);

        // Create the deopt event
        let event = cratonvm_jit::deopt::DeoptEvent {
            reason,
            action: cratonvm_jit::deopt::DeoptAction::Reinterpret, // initial; may be overridden
            bci,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            // Derived, not `SPECULATION_ID_ANY`: this caller was handed both
            // `(bci, reason)`, which is the whole key. Same derivation function
            // as every producer and every consumer
            // (`cratonvm_jit::deopt::speculation_id`), so an id recorded here
            // names the same speculation the emitter gates ask about.
            //
            // Nothing decides on this field today — `DeoptEvent` is the
            // diagnostic ring and the trap ledger's input, and the per-bci
            // de-spec verdict is recorded separately by
            // `runtime/interpreter/deopt_resume.rs`. Filling it correctly is
            // what makes the ring readable next to the registry rather than a
            // column of zeroes.
            speculation_id: cratonvm_jit::deopt::speculation_id(bci, reason),
        };

        // Record in deopt log and get recommended action. The tiered manager's
        // key carries the class identity its per-method state is keyed by.
        let tiered_key = cratonvm_jit::tiered::MethodKey::with_class_id(
            class_id,
            class_name,
            method_name,
            descriptor,
        );
        // Before recording: the OSR re-entry memo's stamp must move no later
        // than the log does (`JitBridgeState::deopt_events`).
        vm.jit.bridge.note_deopt_event();
        let action = vm.record_deoptimization(&method_key, event, &tiered_key);
        // JIT round 19 wave 2, lane eager2 (EAGER19-1,
        // `CRATONVM_JIT_UNINIT_NEW_TRAP`): an uninitialized-`new` trap spends
        // its site and asks the recompile. A no-op unless such a site was
        // admitted in this VM.
        if reason == cratonvm_jit::deopt::DeoptReason::UncommonTrap {
            crate::runtime::interpreter::jit_bridge::uninit_new_trap_fired(
                vm,
                class_id,
                class_name,
                method_name,
                descriptor,
                bci,
            );
        }

        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] {} reason={:?} bci={} action={:?}",
                method_key, reason, bci, action
            );
        }

        // Invalidate the compiled method from the JIT cache. Skipped only
        // for a SOFT OsrExit (action == Reinterpret, i.e. still within the
        // "tolerate a few, they're rare" window in deopt.rs's OsrExit arm):
        // that case means a loop boundary inside an otherwise-good artifact
        // bailed to the interpreter for THIS call (real_frame_deopt_resume_
        // and_despeculate already reconstructs and resumes the frame
        // correctly without needing eviction) -- not that the compiled code
        // is wrong, so recompiling it would be wasted work. Once OsrExit
        // escalates past that window (action == MakeNotCompilable, meaning
        // the SAME bci keeps exiting -- a structural property of the loop,
        // not noise), evict normally like every other reason: keeping a
        // doomed compiled entry alive pays the reconstruct-and-resume tax on
        // literally every future call, which measured net SLOWER than plain
        // interpretation (347s/round vs a 63-72s/round fully-interpreted
        // baseline for the same benchmark -- docs/known-issues/tomcat-08-07/
        // silent-hang-no-signature-cluster.md, TestResponsePerformance).
        // The predicate is shared with the tiered manager, which must drop the
        // method's tier exactly when this evicts its body.
        let skip_eviction = !cratonvm_jit::tiered::deopt_evicts_method_body(reason, action);
        // JIT r25 wave 2 (`CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2`): whether the
        // body this trap evicts was an optimizing one, for the re-queue below.
        let mut evicted_was_optimizing = false;
        if !skip_eviction {
            // The same `class_id` the tiered key carries: resolved by name in
            // `deoptimize` (its ~20 `&str`-keyed callers, not loader-aware),
            // supplied by the frame in `deoptimize_in`. It used to be looked
            // up a second time here, under a second `class_manager.read()`.
            // See `JitKey::declaring_class_id`'s doc comment.
            let mut jit_cache = vm.jit.jit_cache.write();
            // Round 16 wave 15 (lane trapfix): the body this eviction
            // withdraws, read first, so its entry can be made not entrant
            // below (`make_trap_evicted_body_not_entrant`).
            let evicted = trap_eviction_not_entrant_enabled()
                .then(|| jit_cache.get(class_name, method_name, descriptor, class_id))
                .flatten();
            if cratonvm_jit::tiered::trap_recompile_keeps_c2_enabled() {
                // Read while the cache still holds the body, so this handle
                // is never its last owner.
                evicted_was_optimizing = match &evicted {
                    Some(body) => body.used_ir_backend,
                    None => jit_cache
                        .get(class_name, method_name, descriptor, class_id)
                        .is_some_and(|body| body.used_ir_backend),
                };
            }
            jit_cache.remove(class_name, method_name, descriptor, class_id);
            if let Some(evicted) = evicted {
                make_trap_evicted_body_not_entrant(vm, &method_key, evicted);
            }
        }

        // deopt-osr Step 9 — advance the method's live compilation epoch so that
        // (a) the next compilation is stamped fresh and (b) any frame still
        // executing this now-evicted artifact, when it reaches the
        // real-frame-deopt resume sink, sees `compilation_epoch < live` and
        // re-runs instead of resuming a superseded speculation. Gated on the
        // resume feature: the epoch is only ever *read* under `deopt_real_enabled()`,
        // so production VMs neither bump nor consult it (byte-identical).
        if cratonvm_jit::deopt_real_enabled() {
            vm.bump_compilation_epoch(&method_key);
        }

        // If the deopt log recommends giving up, add to the JIT skip set.
        // Also record it in the SEPARATE cratonvm_jit registry -- `jit_skip_set`
        // alone only gates the interpreter's own per-call hotness/upgrade path
        // (`vm/src/runtime/interpreter.rs`'s `execute()`); a JIT-compiled
        // caller dispatching to this method as a CALLEE goes through
        // `try_jit_compile_callee`/`_slow`, which never consults
        // `jit_skip_set` at all. Without this, a give-up decision reached via
        // the callee-dispatch path (exactly the shape of Tomcat's
        // `Response.toAbsolute()`, called from `TestResponsePerformance`'s
        // JIT-compiled `doHomebrew()` loop) never actually stuck: the method
        // kept getting recompiled every time its deopt count re-crossed the
        // threshold, an unbounded repeat of the same 15KB-recompile thrash
        // this fix exists to stop. See docs/known-issues/tomcat-08-07/
        // silent-hang-no-signature-cluster.md.
        //
        // N3: the registry is `compile_gate::mark_runtime_not_compilable` and
        // NOT `mark_jit_bail_listed`, which is what this line used to call.
        // `mark_jit_bail_listed`'s own contract is "the heavy `x64::compile`
        // path returned None, typically because of an unsupported backend
        // pattern that won't change on retry" -- a claim about the BACKEND,
        // consulted at every compile door including OSR. What we know here is
        // something else entirely: one call site in this method mis-speculated
        // at runtime. Filing the second under the first banned OSR of every
        // unrelated loop in the method, and one cold `assert x : "msg " + y;`
        // under `-ea` was enough to do it -- a non-bridged `invokedynamic`, an
        // unconditional reason-8 trap, and `recommend_action(UnreachedCode)` is
        // `MakeNotCompilable` on the FIRST occurrence. The new registry has the
        // same reach at the method-entry and callee-dispatch doors, so the
        // tomcat fix above is preserved exactly, and none at the OSR door.
        if action == cratonvm_jit::deopt::DeoptAction::MakeNotCompilable {
            let mut skip = vm.jit.jit_skip_set.write();
            skip.insert((class_name.into(), method_name.into(), descriptor.into()));
            // NOTES-cha.md C1: mark THIS VM's registry. Every READ of the
            // runtime de-speculation verdicts names a VM since M7 moved them
            // off a process global onto `JitRealm::runtime_despec`; this was
            // the one WRITE that did not, and the only reason
            // `compile_gate::LIVE_REGISTRIES` fan-out had a production caller.
            // We hold `vm: &SharedVm` here, so the VM is not in doubt.
            //
            // `mark_runtime_not_compilable` is deliberately NOT deleted: it is
            // `pub`, and the `AMBIGUOUS_RUNTIME_MARKS` counter it maintains is
            // the only evidence anyone can gather about the residual. It now
            // has no production caller, which is the point — the counter
            // staying at zero is what "closed" looks like.
            vm.jit
                .runtime_despec
                .mark(class_id, class_name, method_name, descriptor);
            // JIT round 23 wave 5 (lane L3,
            // `r23-w4-hot-throwing-method-ends-interpreted-ineligible`): name
            // the verdict. Without this the end-of-run method-stats table read
            // `ineligible-by-policy reason=unrecorded` for a method retired by
            // its deopt budget, which sent the reader looking for a compile
            // refusal that never happened. Diagnostic only.
            vm.jit.tiered_manager.verdicts().record_compile_refusal(
                class_id,
                class_name,
                method_name,
                descriptor,
                deopt_budget_refusal_label(reason),
            );
        }

        // deopt-osr Step 9 follow-up (b): eager recompile re-queue. On a
        // RecompileAndReinterpret action the artifact was just evicted
        // (make-not-entrant); historically the method then had to re-cross the
        // interpreter hotness threshold (up to JIT_RETRY_STRIDE calls) before the
        // optimized body was rebuilt. When a background compiler is active,
        // eagerly enqueue the recompile so it happens off-thread without that
        // delay (`on_deoptimization` above already reset `queued_for_compilation`
        // / `current_tier`, so this enqueue is accepted). Gated on the deopt-
        // resume feature so the production uncommon-trap path is byte-identical;
        // a no-op when no background worker drains the queue — the existing
        // hotness-retry path still recompiles, so behaviour never regresses.
        if cratonvm_jit::deopt_real_enabled()
            && action == cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret
            && vm.jit.tiered_manager.compiler_active()
        {
            // JIT r25 wave 2 (`CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2`, default ON since j27):
            // an evicted OPTIMIZING body is re-queued at C2 (the gated retry
            // door); C1 below only when that door refuses.
            let c2_requeued = evicted_was_optimizing
                && matches!(
                    vm.jit.tiered_manager.request_trap_recompile_c2(&tiered_key),
                    cratonvm_jit::tiered::C2RetryAnswer::Enqueued
                        | cratonvm_jit::tiered::C2RetryAnswer::AlreadyQueued
                );
            if c2_requeued && dbg_deopt_on() {
                eprintln!("[cratonvm-deopt] eager re-queue at C2 (trap evicted an optimizing body) {method_key}");
            }
            if !c2_requeued {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                vm.jit
                    .tiered_manager
                    .enqueue_compilation(cratonvm_jit::tiered::CompilationTask {
                        method_key: tiered_key.clone(),
                        target_tier: cratonvm_jit::tiered::CompilationTier::C1,
                        priority: cratonvm_jit::tiered::CompilationPriority::High,
                        enqueue_time_ms: now_ms,
                        osr_bci: None,
                    });
                if dbg_deopt_on() {
                    eprintln!(
                        "[cratonvm-deopt] eager re-queue (RecompileAndReinterpret) {}",
                        method_key
                    );
                }
            }
        }

        tracing::debug!(
            "deopt: {} reason={:?} bci={} action={:?}",
            method_key,
            reason,
            bci,
            action
        );

        // Emit JFR deoptimization event
        // Round-4: emit_deoptimization_event takes `&'static str` for reason
        // and action — both are bounded enums, so map to static literals
        // rather than `format!("{:?}", ...)`-allocating per deopt.
        //
        // Round 11: only while a recording is active. The emitter returns on
        // `!cratonvm_jfr::is_enabled()` itself, but only AFTER this block had
        // read the clock and taken the process-wide recorder mutex, per trap.
        // SBR5 lane jfr: the real JFR backend's jdk.Deoptimization, with
        // HotSpot's reason / action spellings.
        if cratonvm_native_builtins::jfr_vm_events::any_recording() {
            let reason_hs = match reason {
                cratonvm_jit::deopt::DeoptReason::NullCheck => "null_check",
                cratonvm_jit::deopt::DeoptReason::ClassCheck
                | cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged => "class_check",
                cratonvm_jit::deopt::DeoptReason::BoundsCheck => "range_check",
                cratonvm_jit::deopt::DeoptReason::DivByZero => "div0_check",
                cratonvm_jit::deopt::DeoptReason::ClassLoading
                | cratonvm_jit::deopt::DeoptReason::UninitializedAccess => "uninitialized",
                cratonvm_jit::deopt::DeoptReason::UnreachedCode => "unreached",
                _ => "unstable_if",
            };
            let action_hs = match action {
                cratonvm_jit::deopt::DeoptAction::Reinterpret
                | cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret => "reinterpret",
                _ => "make_not_entrant",
            };
            cratonvm_native_builtins::jfr_vm_more::deoptimization(
                vm.vm_identity,
                &method_key,
                reason_hs,
                action_hs,
                bci as i32,
            );
        }
        if cratonvm_jfr::is_enabled() {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let reason_static: &'static str = match reason {
                cratonvm_jit::deopt::DeoptReason::NullCheck => "NullCheck",
                cratonvm_jit::deopt::DeoptReason::ClassCheck => "ClassCheck",
                cratonvm_jit::deopt::DeoptReason::BoundsCheck => "BoundsCheck",
                cratonvm_jit::deopt::DeoptReason::DivByZero => "DivByZero",
                cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged => "ReceiverTypeChanged",
                cratonvm_jit::deopt::DeoptReason::ClassLoading => "ClassLoading",
                cratonvm_jit::deopt::DeoptReason::UninitializedAccess => "UninitializedAccess",
                cratonvm_jit::deopt::DeoptReason::TransferToInterpreter => "TransferToInterpreter",
                cratonvm_jit::deopt::DeoptReason::UncommonTrap => "UncommonTrap",
                cratonvm_jit::deopt::DeoptReason::SpeculationFailed => "SpeculationFailed",
                cratonvm_jit::deopt::DeoptReason::NotCompiled => "NotCompiled",
                cratonvm_jit::deopt::DeoptReason::UnreachedCode => "UnreachedCode",
                cratonvm_jit::deopt::DeoptReason::OsrExit => "OsrExit",
                cratonvm_jit::deopt::DeoptReason::PendingException => "PendingException",
                cratonvm_jit::deopt::DeoptReason::UnstableIf => "UnstableIf",
            };
            let action_static: &'static str = match action {
                cratonvm_jit::deopt::DeoptAction::Reinterpret => "Reinterpret",
                cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret => {
                    "RecompileAndReinterpret"
                }
                cratonvm_jit::deopt::DeoptAction::MakeNotEntrant => "MakeNotEntrant",
                cratonvm_jit::deopt::DeoptAction::MakeNotCompilable => "MakeNotCompilable",
            };
            let mut jfr = vm.debug.flight_recorder.lock();
            cratonvm_jfr::builtin::emit_deoptimization_event(
                &mut jfr,
                &method_key,
                0, // compile_id
                reason_static,
                action_static,
                bci as i32,
                // Round-5 MED-fix (2026-05-17): plumb the real JFR thread id
                // so JMC can attribute the deopt to the thread that triggered
                // it. `current_jfr_thread_id()` is TLS-cached, allocates once
                // per thread, and steady-state cost is a TLS read + branch.
                cratonvm_jfr::builtin::current_jfr_thread_id(),
                now_ns,
            );
        }

        action
    }
}

/// JIT runtime helper: called from compiled code when a speculative
/// optimization fails (uncommon trap).
///
/// Signature: extern "C" fn(vm_ptr: i64, reason: i64, bci: i64) -> i64
///
/// The reason parameter encodes a `DeoptReason` variant as an integer.
/// Returns a deopt action code:
///   0 = reinterpret (continue in interpreter)
///   1 = recompile (invalidate and recompile with updated profile)
///   2 = blacklist (never compile again)
///
/// After this returns, the JIT code should return control to the interpreter.
/// The calling convention is that the JIT method returns a sentinel value
/// (i64::MIN) to signal "deoptimized, resume in interpreter".
// SAFETY: Called from JIT-compiled code when a speculative optimization fails.
// vm_ptr must be 0 or a valid SharedVm pointer. reason encodes a DeoptReason variant.
// bci is the bytecode index of the failing instruction. Accesses the JIT thread pointer
// (via jit_thread_mut) and the deoptimization controller to invalidate compiled code.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_uncommon_trap(vm_ptr: i64, reason: i64, bci: i64) -> i64 {
    let f = || jit_uncommon_trap_body(vm_ptr, reason, bci);
    contain(
        "jit_uncommon_trap",
        OnPanic::DeoptFor { vm_ptr },
        DEOPT_ACTION_REINTERPRET,
        f,
    )
}

/// Body of [`jit_uncommon_trap`], run under its panic guard.
// SAFETY: Called under `contain` panic guard from JIT deopt stubs with valid ABI parameters.
unsafe fn jit_uncommon_trap_body(vm_ptr: i64, reason: i64, bci: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Out-of-band deopt signal: the x64 `emit_deopt_stubs` stub that calls this
    // helper loads `i64::MIN` into RAX and runs the method epilogue immediately
    // afterwards, so this trap ALWAYS precedes an `i64::MIN` method return. Flag
    // it as a genuine deopt so the interpreter doesn't mistake a method
    // legitimately returning `Long.MIN_VALUE` for one.
    set_jit_deopt_pending();
    // Before the `vm_ptr` test: a method with no heap local passes a null VM.
    note_frameless_trap_stub(reason, bci);
    if vm_ptr == 0 {
        return DEOPT_ACTION_REINTERPRET;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // The x64 stub packs the trapping artifact's compile id into the high half
    // of `reason` (0 when it had none to pass).
    let compile_id = ((reason as u64) >> 32) as u32; // Cast: the high half is the id
    let reason = reason & 0xFFFF_FFFF;
    let deopt_reason = reason_code_to_deopt_reason(reason);

    // The method that TRAPPED. `thread.frames.last()` is not it: a compiled
    // method pushes no interpreter frame, so that frame is its interpreted
    // CALLER, and the deopt count, the cache eviction and the bail-list verdict
    // all landed on the caller while the trapping artifact stayed installed and
    // trapped again on every call. Resolve the artifact by compile id; the old
    // guess remains only for a stub that had no id to pass.
    let trapping_artifact = (compile_id != 0)
        .then(|| cratonvm_jit::lookup_compile_id(compile_id))
        .flatten()
        // SAFETY: the id was published by the artifact whose code is
        // executing this trap, so the artifact is live for this call.
        .map(|cm_ptr| &*(cm_ptr as *const cratonvm_jit::CompiledMethod));
    let trapping = trapping_artifact.and_then(|cm| split_method_label(&cm.method_label));

    // A trap out of a body the cache no longer publishes -- a thread still
    // inside it, or re-entering it through a stale call-site cache, after an
    // earlier trap's recompile published a fresh one -- is not charged:
    // `deoptimize` evicts BY NAME, so it would remove the fresh body and charge
    // the ledger again for the one speculation failure, N threads N times
    // (r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff,
    // this sink's half; the two VM sinks ask the same question through
    // `deopt_resume::trapped_artifact_is_superseded`). The deopt itself still
    // happens: the pending flag is set above and the stub returns the sentinel.
    if let (Some(cm), Some((class_name, method_name, descriptor))) =
        (trapping_artifact, trapping.as_ref())
    {
        if trapping_artifact_is_superseded(vm, cm, class_name, method_name, descriptor) {
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] {class_name}.{method_name}:{descriptor} bci={bci} \
                     reason={deopt_reason:?}: trap from a superseded body, not charged"
                );
            }
            return DEOPT_ACTION_REINTERPRET;
        }
    }

    let named_by_artifact = trapping.is_some();
    // Otherwise determine the method from the JIT thread context. If we can't,
    // we still record the deopt but with a generic key.
    let (class_name, method_name, descriptor) = if let Some(identity) = trapping {
        identity
    } else {
        // The thread's current frame has the method info
        let default = (
            "unknown".to_string(),
            "unknown".to_string(),
            "()V".to_string(),
        );
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(frame) = thread.frames.last() {
                (
                    frame.class_name().to_string(),
                    frame.method_name().to_string(),
                    frame.method_descriptor().to_string(),
                )
            } else {
                default
            }
        } else {
            default
        }
    };

    // The trapping body's own class, when its label names that class (round 12
    // wave 1, lane rt). By name, two loaders' copies of one class resolve to
    // `ClassId(0)`, which keyed the charge, the eviction and the
    // de-speculation on no class: the trapping body stayed published and
    // trapped again on every call, the defect the compile-id lookup above was
    // written to end. A body published under another class's key (an
    // inherited method under its receiver's) keeps the by-name answer.
    let class_id = trapping_artifact
        .filter(|_| named_by_artifact)
        .map(|cm| cm.owner_class_id)
        .filter(|&id| id != cratonvm_types::jit_activation::NO_OWNER_CLASS)
        .map(ClassId::new)
        .filter(|&id| {
            vm.classes
                .class_manager
                .read()
                .get_class(id)
                .is_some_and(|class| class.name.as_ref() == class_name.as_str())
        })
        .unwrap_or_else(|| ClassId::new(0));
    let action = DeoptimizationController::deoptimize_or_by_name(
        vm,
        class_id,
        &class_name,
        &method_name,
        &descriptor,
        deopt_reason,
        bci as u32,
    );

    match action {
        cratonvm_jit::deopt::DeoptAction::Reinterpret => DEOPT_ACTION_REINTERPRET,
        cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret => DEOPT_ACTION_RECOMPILE,
        cratonvm_jit::deopt::DeoptAction::MakeNotEntrant => DEOPT_ACTION_RECOMPILE,
        cratonvm_jit::deopt::DeoptAction::MakeNotCompilable => DEOPT_ACTION_BLACKLIST,
    }
}

/// Name the frameless trap stub that just raised the deopt flag
/// ([`FramelessTrapSite::Stub`]; round 13 wave 8, lane replay4), so a re-run
/// from entry after its bare sentinel can be told apart from one after a
/// helper's (`deopt_resume::frameless_rerun_is_exact`). `reason` / `bci` are
/// [`jit_uncommon_trap`]'s arguments; its caller is only the frameless arm of
/// `emit_deopt_stubs`.
fn note_frameless_trap_stub(reason: i64, bci: i64) {
    // Cast: the high half of the stub's reason word is the compile id.
    let compile_id = ((reason as u64) >> 32) as u32;
    let site = match u32::try_from(bci) {
        Ok(bci) if compile_id != 0 => match cratonvm_jit::lookup_compile_id(compile_id) {
            Some(cm_ptr) => {
                // SAFETY: the id was published by the artifact whose stub is
                // executing this trap, so the artifact is live for this call
                // (the rule `jit_uncommon_trap_body` reads it under).
                let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                FramelessTrapSite::Stub {
                    compile_id,
                    bci,
                    exact: cm.frameless_traps_replay_exact,
                    // Round 13 wave 13 (lane jitfix): whose stamp `exact` is.
                    method: split_method_label(&cm.method_label)
                        .map_or(0, |(c, m, d)| frameless_trap_method_hash(&c, &m, &d)),
                }
            }
            None => FramelessTrapSite::Unknown,
        },
        _ => FramelessTrapSite::Unknown,
    };
    JIT_SIGNALS.with(|s| s.frameless_trap.set(site));
}

/// Does the JIT cache publish a DIFFERENT body than `trapping` for its method?
///
/// `deopt_resume::trapped_artifact_is_superseded`'s rule, for a sink that
/// holds the trapping artifact itself: keyed on the class id the artifact was
/// published under (`owner_class_id`, the cache's own key), compared by
/// pointer, and only a POSITIVE mismatch counts. An empty slot answers `false`
/// (the pre-existing charge), and so do an OSR body and an unpublished one
/// (`!was_published()`: the accepted optimizing OSR body carries its class's
/// `owner_class_id` since round 12 wave 7 without ever being published): the
/// cache's normal-entry slot never holds them, so a different body there says
/// nothing about whether they are retired.
fn trapping_artifact_is_superseded(
    vm: &SharedVm,
    trapping: &cratonvm_jit::CompiledMethod,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if trapping.compiled_via_osr
        || !trapping.was_published()
        || trapping.owner_class_id == cratonvm_types::jit_activation::NO_OWNER_CLASS
    {
        return false;
    }
    let live = vm.jit.jit_cache.read().get(
        class_name,
        method_name,
        descriptor,
        ClassId::new(trapping.owner_class_id),
    );
    match live {
        Some(live) => !std::ptr::eq(std::sync::Arc::as_ptr(&live), trapping),
        None => false,
    }
}

/// `(class, method, descriptor)` from a compiled artifact's `method_label`,
/// which is `<class>.<method>:<descriptor>` or `<class>.<method><descriptor>`.
fn split_method_label(label: &str) -> Option<(String, String, String)> {
    let open = label.find('(')?;
    let head = label[..open].trim_end_matches(':');
    let dot = head.rfind('.')?;
    Some((
        head[..dot].to_string(),
        head[dot + 1..].to_string(),
        label[open..].to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Helper table construction
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // N3(b) — the trapped callee's reason comes from its artifact
    // -----------------------------------------------------------------------

    fn deopt_point(
        bci: u32,
        native_offset: u32,
        reason: cratonvm_jit::deopt::DeoptReason,
    ) -> cratonvm_jit::deopt::DeoptimizationPoint {
        cratonvm_jit::deopt::DeoptimizationPoint {
            native_offset,
            bci,
            reason,
            action: cratonvm_jit::deopt::DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: cratonvm_jit::deopt::FrameState {
                method_key: "P.callee:(I)J".to_string(),
                bci,
                locals: vec![cratonvm_jit::deopt::FrameValue::Int(7)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: cratonvm_jit::deopt::ResumeSemantics::for_reason(reason),
        }
    }

    /// The defect: `try_resume_trapped_callee`'s SUCCESS path — the precise,
    /// no-side-effect-replayed resume this whole machinery exists to produce —
    /// charged a hard-coded `UnreachedCode`, which is the one reason
    /// `recommend_action` answers `MakeNotCompilable` on the FIRST occurrence.
    /// A `ReceiverTypeChanged` guard that a recompile would simply re-profile
    /// therefore retired the callee permanently.
    /// JIT round 19 wave 10, lane deopt10 (i62-L2): a chain trap's charge.
    /// The frame-less by-bci arm reads the OUTER method's points at the
    /// outermost scope's bci, where a receiver guard on the invoke answers for
    /// a callee's null check; the chain rule answers the stash's own cause.
    #[test]
    fn a_chain_trap_is_charged_its_own_cause_not_the_call_sites_guard() {
        use cratonvm_jit::deopt::{DeoptCause, DeoptReason, SPECULATION_ID_ANY};
        let outer = vec![deopt_point(12, 0x40, DeoptReason::ReceiverTypeChanged)];
        let cause = Some(DeoptCause {
            reason: DeoptReason::NullCheck,
            speculation_id: SPECULATION_ID_ANY,
        });
        // Off (today): the call site's guard answers for the callee's trap.
        assert_eq!(
            deopt_reason_from_artifact(Some(&outer), 12, cause),
            DeoptReason::ReceiverTypeChanged
        );
        // On: the stash's own reason, `UncommonTrap` without one.
        assert_eq!(chain_trap_charge_reason(cause), DeoptReason::NullCheck);
        assert_eq!(chain_trap_charge_reason(None), DeoptReason::UncommonTrap);
    }

    #[test]
    fn a_trapped_callee_is_charged_the_reason_its_artifact_recorded() {
        let points = vec![deopt_point(
            12,
            0x40,
            cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged,
        )];
        assert_eq!(
            deopt_reason_from_artifact(Some(&points), 12, None),
            cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged
        );
        assert_ne!(
            deopt_reason_from_artifact(Some(&points), 12, None),
            cratonvm_jit::deopt::DeoptReason::UnreachedCode,
            "`UnreachedCode` is reserved for genuinely unreachable code; \
             charging it here blacklists the callee on its first trap"
        );
    }

    /// A protected invoke carries a re-execute guard AND a rethrow frame at one
    /// bci, and the frame this sink holds came from `take_last_deopt`, so it
    /// cannot have come from the rethrow point. Recorded FIRST here, which is
    /// what a list-order pick would get wrong.
    #[test]
    fn a_protected_invoke_charges_the_guard_and_not_the_rethrow() {
        let points = vec![
            deopt_point(6, 0x10, cratonvm_jit::deopt::DeoptReason::PendingException),
            deopt_point(
                6,
                0x20,
                cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged,
            ),
        ];
        assert_eq!(
            deopt_reason_from_artifact(Some(&points), 6, None),
            cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged
        );
    }

    /// Three ways the lookup can come up empty, all of which answer
    /// `UncommonTrap`: it is CHARGED, so the trap rate stays visible and the
    /// method still backs off, and it escalates on the ordinary count-based
    /// schedule — so an attribution nobody could make can never blacklist a
    /// method by itself.
    #[test]
    fn an_unattributable_callee_trap_is_charged_as_an_uncommon_trap() {
        // Two GUARDS: since wave 49 (`OSR_EXIT_MAPS_BESIDE_GUARDS`) an
        // `OsrExit` map beside one guard no longer makes the bci ambiguous
        // (`an_exit_map_beside_a_guard_charges_the_guard_or_the_cause`).
        let ambiguous = vec![
            deopt_point(
                16,
                0x37e,
                cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged,
            ),
            deopt_point(16, 0x3c1, cratonvm_jit::deopt::DeoptReason::BoundsCheck),
        ];
        for (what, got) in [
            // The artifact was evicted between the trap and this call.
            ("no artifact", deopt_reason_from_artifact(None, 12, None)),
            // No point in the artifact carries this bci.
            (
                "no point at the bci",
                deopt_reason_from_artifact(Some(&ambiguous), 99, None),
            ),
            // Two guards at one bci: which fired is not recoverable from a bci.
            (
                "two disagreeing guards",
                deopt_reason_from_artifact(Some(&ambiguous), 16, None),
            ),
        ] {
            assert_eq!(
                got,
                cratonvm_jit::deopt::DeoptReason::UncommonTrap,
                "{what} must not escalate one-shot"
            );
        }
    }

    /// Interpreter round i1 wave 49, lane L2
    /// (`cratonvm_jit::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS`): an `OsrExit`
    /// map at a guard's bci (a loop header's exit beside its BCE guard, or a
    /// post-call exit beside the successor's snapshot). Without a cause the
    /// frame-less arm charges the guard (the map is a mode exit, not a
    /// speculation); with one it charges what the stub baked. Switch off:
    /// the wave-48 answers (`UncommonTrap`, the cause ignored).
    #[test]
    fn an_exit_map_beside_a_guard_charges_the_guard_or_the_cause() {
        use cratonvm_jit::deopt::{DeoptCause, DeoptReason, SPECULATION_ID_ANY};
        let points = vec![
            deopt_point(16, 0x37e, DeoptReason::OsrExit),
            deopt_point(16, 0x3c1, DeoptReason::BoundsCheck),
        ];
        let exit = Some(DeoptCause {
            reason: DeoptReason::OsrExit,
            speculation_id: SPECULATION_ID_ANY,
        });
        // A cause no point at the bci carries is not believed.
        let foreign = Some(DeoptCause {
            reason: DeoptReason::NullCheck,
            speculation_id: SPECULATION_ID_ANY,
        });
        let on = cratonvm_jit::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS;
        let guard_or_fallback = if on {
            DeoptReason::BoundsCheck
        } else {
            DeoptReason::UncommonTrap
        };
        assert_eq!(
            deopt_reason_from_artifact(Some(&points), 16, None),
            guard_or_fallback
        );
        assert_eq!(
            deopt_reason_from_artifact(Some(&points), 16, foreign),
            guard_or_fallback
        );
        assert_eq!(
            deopt_reason_from_artifact(Some(&points), 16, exit),
            if on {
                DeoptReason::OsrExit
            } else {
                DeoptReason::UncommonTrap
            }
        );
    }

    // -----------------------------------------------------------------------
    // N6 -- an exceptional frame is dropped only by the method that published it
    // -----------------------------------------------------------------------

    /// The identity rule the three `route_implicit_exc_through_callee` drops
    /// now go through.
    ///
    /// `clear_exceptional_frame()` took no identity at all, so its own stated
    /// rule -- "sinks that merely pass an exception along must NOT call this"
    /// -- could not be checked by the function it was written on. What it
    /// dropped on a nested trap was a DIFFERENT method's frame, one the
    /// abandoning caller never published.
    #[test]
    fn an_exceptional_frame_belongs_to_the_method_its_key_names() {
        // The ordinary case: same method, same descriptor.
        assert!(exceptional_frame_key_names("p/C.m:(I)J", "m", "(I)J"));
        // Virtual dispatch: the key names the RESOLVED declaring class, which
        // may be a supertype of the class the call site was written with. That
        // is why the class is not compared -- comparing it would decline every
        // frame published by an inherited method.
        assert!(exceptional_frame_key_names("p/Base.m:(I)J", "m", "(I)J"));
        // Covariant return: the call site's descriptor and the resolved
        // method's differ in the return type only.
        assert!(exceptional_frame_key_names(
            "p/C.get:()Lp/Sub;",
            "get",
            "()Lp/Base;"
        ));

        // A nested callee's frame: a different method, which this sink never
        // published and is not entitled to drop.
        assert!(!exceptional_frame_key_names("p/C.other:(I)J", "m", "(I)J"));
        // Same name, different parameters -- an overload is a different method.
        assert!(!exceptional_frame_key_names("p/C.m:(J)J", "m", "(I)J"));

        // Fail closed on anything that names no method at all. The empty key is
        // what the legacy/test wrappers bake, and it must never match.
        for key in ["", "no-colon", "nodot:(I)J", ":()V"] {
            assert!(
                !exceptional_frame_key_names(key, "m", "(I)J"),
                "{key:?} names no method and is nobody's to drop"
            );
        }
    }

    /// Round 11 wave 5 (lane rt): the parameter list is cut at the `)` that
    /// closes it. A class name may contain `)` (JVMS 4.2.2); the first-`)` cut
    /// compared `(LA)` with `(LA)` for the two different lists below and
    /// matched them, and `varhandle_cas_operand_kinds` read a truncated list.
    #[test]
    fn a_paren_in_a_class_name_does_not_cut_the_parameter_list() {
        assert!(!descriptors_match_modulo_return("(LA)B;)Z", "(LA)C;)Z"));
        assert!(descriptors_match_modulo_return("(LA)B;)Z", "(LA)B;)I"));
        assert!(!descriptors_match_modulo_return("(LA)B;)Z", "(LA)Z"));
        assert_eq!(
            varhandle_cas_operand_kinds("(LA)B;II)Z"),
            Some((b'I', b'I'))
        );
        assert_eq!(
            varhandle_cas_operand_kinds("(Ljava/lang/Object;JJ)Z"),
            Some((b'J', b'J'))
        );
        assert_eq!(varhandle_cas_operand_kinds("(LA)B;I)Z"), None);
        assert_eq!(varhandle_cas_operand_kinds("II)Z"), None);
    }

    /// JIT round 16 wave 7 (lane varhandle, MH16-6): the interpreter's CAS
    /// site kind is the common kind of `expected` and `new`, references
    /// collapsed; mixed kinds, an index coordinate and a static site decline.
    #[test]
    fn the_interpreter_cas_site_kind_needs_one_operand_kind() {
        assert_eq!(varhandle_cas_site_kind("(LH;II)Z"), Some(b'I'));
        assert_eq!(varhandle_cas_site_kind("(LH;JJ)Z"), Some(b'J'));
        assert_eq!(
            varhandle_cas_site_kind("(LH;Ljava/lang/Object;Ljava/lang/String;)Z"),
            Some(b'L')
        );
        assert_eq!(
            varhandle_cas_site_kind("(LH;[ILjava/lang/Object;)Z"),
            Some(b'L')
        );
        assert_eq!(varhandle_cas_site_kind("(LH;IJ)Z"), None);
        assert_eq!(varhandle_cas_site_kind("(LH;Ljava/lang/Integer;I)Z"), None);
        assert_eq!(varhandle_cas_site_kind("([IIII)Z"), None);
        assert_eq!(varhandle_cas_site_kind("(II)Z"), None);
    }

    // -----------------------------------------------------------------------
    // Every implicit JIT signal is an implicit exception at BOTH callee doors
    // -----------------------------------------------------------------------

    /// The regression this file's `ImplicitSignal` exists for.
    ///
    /// `route_implicit_exc_through_callee` used to decide "is there an implicit
    /// signal at all?" with a hand-written `aioobe.is_none() && !npe`, while
    /// `handle_compiled_callee_deopt_sentinel` decided the same question with a
    /// three-armed `match` that also knew about `arithmetic`. A `/ by zero` in a
    /// compiled callee reached through the dispatch helper therefore read as
    /// "nothing implicit pending", and the callee's own
    /// `catch (ArithmeticException)` was skipped.
    ///
    /// A pass/fail probe could only ever sample that — which door a call takes
    /// depends on compile ordering, so it showed up at ~8 % of runs. The
    /// arithmetic is not a race, so pin it here where it is deterministic.
    #[test]
    fn a_pending_arithmetic_signal_is_an_implicit_exception() {
        assert_eq!(
            implicit_signal_of(None, false, true),
            ImplicitSignal::Arithmetic,
            "a zero-divisor signal must classify as an implicit exception; \
             reading it as `None` is the ExcTableDirectCallOracle flake"
        );
        assert_ne!(
            implicit_signal_of(None, false, true),
            ImplicitSignal::None,
            "and in particular must not take the `no implicit signal` branch"
        );
    }

    /// Only the empty triple is `None`. Stated as its own test because it is
    /// the property a future fourth signal would break: adding a field to
    /// `JitSignals` and forgetting this function puts the new signal back in
    /// the `None` bucket, which is silently "propagate to the caller".
    #[test]
    fn the_no_signal_answer_is_reserved_for_the_empty_triple() {
        assert_eq!(implicit_signal_of(None, false, false), ImplicitSignal::None);
        for signal in [
            implicit_signal_of(Some((3, 2)), false, false),
            implicit_signal_of(None, true, false),
            implicit_signal_of(None, false, true),
        ] {
            assert_ne!(signal, ImplicitSignal::None);
        }
    }

    /// Precedence is AIOOBE, then NPE, then arithmetic — the order both doors
    /// already drained the flags in, kept so this refactor is behaviour-neutral
    /// for the two signals that were already handled. Only one throwable can be
    /// delivered, so an overlapping triple must pick exactly one.
    #[test]
    fn implicit_signal_precedence_is_aioobe_then_npe_then_arithmetic() {
        assert_eq!(
            implicit_signal_of(Some((7, 5)), true, true),
            ImplicitSignal::Aioobe {
                index: 7,
                length: 5
            }
        );
        assert_eq!(implicit_signal_of(None, true, true), ImplicitSignal::Npe);
        assert_eq!(
            implicit_signal_of(None, false, true),
            ImplicitSignal::Arithmetic
        );
    }

    /// The AIOOBE payload has to survive classification: the message the
    /// throwable carries is built from it, and HotSpot's text names both
    /// numbers.
    #[test]
    fn the_aioobe_payload_survives_classification() {
        assert_eq!(
            implicit_signal_of(Some((-1, 0)), false, false),
            ImplicitSignal::Aioobe {
                index: -1,
                length: 0
            }
        );
    }

    /// A drained signal that is restashed must classify back to itself.
    ///
    /// This is the fallback both doors take when the callee's own table does
    /// not cover the throw: the caller's drain builds the throwable FROM the
    /// flag, so a round trip that loses it is not a missed optimisation, it is
    /// an exception that never happens. Runs on this thread's real
    /// `JIT_SIGNALS`, and leaves it clear.
    #[test]
    fn a_restashed_implicit_signal_round_trips_through_the_thread_local() {
        for original in [
            ImplicitSignal::Aioobe {
                index: 9,
                length: 4,
            },
            ImplicitSignal::Npe,
            ImplicitSignal::Arithmetic,
        ] {
            restash_implicit_signal(original);
            let aioobe = take_jit_pending_aioobe();
            let npe = take_jit_pending_npe();
            let arithmetic = take_jit_pending_arithmetic();
            assert_eq!(implicit_signal_of(aioobe, npe, arithmetic), original);
        }
        restash_implicit_signal(ImplicitSignal::None);
        assert_eq!(
            implicit_signal_of(
                take_jit_pending_aioobe(),
                take_jit_pending_npe(),
                take_jit_pending_arithmetic()
            ),
            ImplicitSignal::None
        );
    }

    fn drained_signals(
        npe: bool,
        aioobe: bool,
        arithmetic: bool,
        deopt: bool,
    ) -> DrainedJitSignals {
        DrainedJitSignals {
            exception: None,
            athrow_bci: -1,
            athrow_owner: 0,
            aioobe: aioobe.then_some((5, 4)),
            arithmetic,
            npe,
            trap_frames: None,
            npe_action: 0,
            deopt,
            redispatch_escape: 0,
        }
    }

    /// Interpreter round i1 wave 3 (L7): an implicit exception out of a
    /// directly called lambda impl arrives as its flag PLUS the deopt flag its
    /// stub raises. It is an escaping exception, not a deopt — classifying it
    /// as one re-ran the impl from entry and committed its side effects twice
    /// (`L7JitDoorOutcomes`: `sideEffects` one over HotSpot).
    #[test]
    fn a_lambda_implicit_exception_is_not_a_deopt() {
        for (npe, aioobe, arithmetic) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let sig = drained_signals(npe, aioobe, arithmetic, true);
            assert!(
                lambda_direct_sentinel_is_not_a_deopt(&sig, false, false),
                "npe={npe} aioobe={aioobe} arithmetic={arithmetic} must propagate"
            );
            // An impl with its own handlers keeps the decline: it may catch.
            assert!(!lambda_direct_sentinel_is_not_a_deopt(&sig, false, true));
            // A stashed frame is a deopt whatever else is set.
            assert!(!lambda_direct_sentinel_is_not_a_deopt(&sig, true, false));
        }
        // Unchanged: a bare deopt flag is a deopt; no signal at all is a
        // genuine `Long.MIN_VALUE`.
        assert!(!lambda_direct_sentinel_is_not_a_deopt(
            &drained_signals(false, false, false, true),
            false,
            false
        ));
        assert!(lambda_direct_sentinel_is_not_a_deopt(
            &drained_signals(false, false, false, false),
            false,
            false
        ));
    }

    /// JIT round 23 wave 2, lane L3 (`CRATONVM_JIT_LAMBDA_IMPLICIT_WITHOUT_DEOPT_ROUTES`):
    /// `jit_getfield`'s null receiver raises the NPE flag WITHOUT the deopt
    /// flag. Out of an impl with handlers that must still be offered to the
    /// impl's table (`false`), not restashed past its `catch`; without
    /// handlers it propagates as before. `=0` keeps the old answer.
    #[test]
    fn a_lambda_implicit_signal_without_the_deopt_flag_routes_through_the_impl() {
        for (npe, aioobe, arithmetic) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let sig = drained_signals(npe, aioobe, arithmetic, false);
            assert!(!lambda_direct_sentinel_is_not_a_deopt_as(&sig, false, true, true));
            assert!(lambda_direct_sentinel_is_not_a_deopt_as(&sig, false, false, true));
            // The old classification, kept under `=0`.
            assert!(lambda_direct_sentinel_is_not_a_deopt_as(&sig, false, true, false));
            // With the deopt flag both arms agree.
            let with_deopt = drained_signals(npe, aioobe, arithmetic, true);
            for routes in [false, true] {
                assert!(!lambda_direct_sentinel_is_not_a_deopt_as(
                    &with_deopt,
                    false,
                    true,
                    routes
                ));
                assert!(lambda_direct_sentinel_is_not_a_deopt_as(
                    &with_deopt,
                    false,
                    false,
                    routes
                ));
            }
        }
        // No signal: unchanged by the switch.
        for routes in [false, true] {
            let bare = drained_signals(false, false, false, true);
            assert!(!lambda_direct_sentinel_is_not_a_deopt_as(&bare, false, true, routes));
            let none = drained_signals(false, false, false, false);
            assert!(lambda_direct_sentinel_is_not_a_deopt_as(&none, false, true, routes));
        }
    }

    // -----------------------------------------------------------------------
    // The `Matcher` leaf's index and its arg-count table must agree
    // -----------------------------------------------------------------------

    /// `matcher_site_index` and `matcher_native_arg_count` are two hand-written
    /// tables over the same eight triples, and each answers half of one
    /// decision: the index picks the memo cell that carries the census handle,
    /// the arg count decides whether `call_matcher_native_raw` will build a
    /// frame at all. A row present in one and absent from the other is silently
    /// wrong in a direction the type system cannot see — a pair with an index
    /// but no arg count memoizes an admission nothing ever redeems, and a pair
    /// with an arg count but no index dispatches **uncounted**, which is the
    /// exact §4 census gap the memo was added to close.
    ///
    /// So pin both directions, plus the index being a permutation of `0..8`
    /// (a duplicate would make two triples share one `NativeCallSite`, which is
    /// the misuse that type `debug_assert!`s about).
    #[test]
    fn matcher_leaf_index_and_arg_count_cover_the_same_triples() {
        let mut seen = [false; 8];
        for (name, descriptor) in MATCHER_SITE_TRIPLES {
            let info = JitInvokeInfo {
                class_name: MATCHER_CLASS,
                method_name: name,
                descriptor,
                num_jit_args: 0,
                return_type: 0,
                invoke_kind: 0,
                declaring_class_id: 0,
                owner_class_id: 0,
            };
            let index = matcher_site_index(&info)
                .unwrap_or_else(|| panic!("no site index for Matcher.{name}{descriptor}"));
            assert!(
                !seen[index],
                "two Matcher triples share site index {index}; they would share one \
                 NativeCallSite cell"
            );
            seen[index] = true;
            assert!(
                matcher_native_arg_count(&info).is_some(),
                "Matcher.{name}{descriptor} has a memo cell but no arg count: the \
                 admission is memoized and then always declined"
            );
        }
        assert!(seen.iter().all(|s| *s), "site indices are not 0..8");
    }

    /// The other direction: nothing `matcher_native_arg_count` admits may be
    /// missing from the index, or it dispatches without a census handle.
    /// Checked over the callback table's own domain rather than a second copy
    /// of the list, so adding a ninth triple to `native-builtins` without
    /// adding a cell here fails loudly.
    #[test]
    fn every_matcher_fastpath_triple_has_a_site_index() {
        for (name, descriptor) in MATCHER_SITE_TRIPLES {
            assert!(
                cratonvm_native_builtins::matcher_realjdk_native_callback(name, descriptor)
                    .is_some(),
                "Matcher.{name}{descriptor} has a site index but native-builtins no longer \
                 serves it — the memo cell is dead"
            );
        }
    }

    // -----------------------------------------------------------------------
    // A compiled `invokestatic` must not pick the other loader's copy
    // -----------------------------------------------------------------------

    /// Two loaders define one binary name; a compiled static call made from
    /// inside the SECOND loader's copy must dispatch on that loader's copy, not
    /// on whichever one the flat global name→id map happens to hold.
    ///
    /// # Why this is not a hypothetical
    ///
    /// `AotIntegrationTests.endToEndTestsForBeanOverrides` runs everything under
    /// `@CompileWithForkedClassLoader`, which defines its own copy of every
    /// `org.springframework.*` class it is asked for. A compiled static call out
    /// of the forked copy of `ResolvableType` landed in the APPLICATION copy, so
    /// the `ResolvableType` handed back belonged to the other class;
    /// `WildcardBounds.get` then ran
    /// `while (…) candidate = candidate.resolveType()` with a `NONE` from the
    /// wrong copy, `candidate == NONE` could never hold, and one core spun until
    /// the run was killed. `--nojit` passed the same method, because the
    /// interpreter resolves the owner through the caller's own loader.
    ///
    /// # Three assertions, because one would pass against a stub
    ///
    /// The positive alone is satisfied by "always return the caller-loader
    /// answer", which is the change that would have to be rejected for
    /// `invoke_or_native`'s special cases (native-override order, the
    /// `SyntheticStub` yield rule, the signature-polymorphic intercepts) to keep
    /// working on every ordinary call. So the two negatives — a name with ONE
    /// definition, and a caller whose loader agrees with the global map — are
    /// asserted alongside it. `None` there is what keeps the previous path.
    ///
    /// # The two-copy state
    ///
    /// Built the way `aastore_fails_open_across_a_split_loaders_two_copies_of_one_name`
    /// builds it: `ensure_synthetic_class` dedupes by name, so the second copy is
    /// fabricated under its own name and renamed in place, then registered under
    /// the user loader with `register_class_name` (the class manager's own
    /// documented test side-door for exactly this).
    #[test]
    fn a_compiled_invokestatic_resolves_its_owner_in_the_callers_own_loader() {
        use cratonvm_types::ClassLoaderId;

        const OWNER: &str = "cratonvm/test/SplitStaticOwner";
        const LONELY: &str = "cratonvm/test/LonelyStaticOwner";
        const FORK: ClassLoaderId = ClassLoaderId::UserDefined(4242);

        let shared =
            std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));

        let (app_copy, fork_copy, fork_caller, app_caller, lonely) = {
            let mut cm = shared.classes.class_manager.write();

            // The application loader's copy — what the global name→id map
            // answers with, and what the old by-name dispatch would pick.
            let app_copy = cm
                .try_ensure_synthetic_class(OWNER, 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");

            // The forked loader's copy: fabricated under its own name, renamed,
            // then given that loader's identity and name registration.
            let fork_copy = cm
                .try_ensure_synthetic_class("cratonvm/test/SplitStaticOwner$Fork", 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
            cm.class_store
                .get_mut(fork_copy)
                .expect("just fabricated")
                .name = cratonvm_types::intern_arc(OWNER);
            cm.class_store
                .get_mut(fork_copy)
                .expect("just fabricated")
                .loader_id = FORK;
            cm.register_class_name(FORK, OWNER, fork_copy);

            // The call SITE's class, once inside the fork and once outside it.
            let fork_caller = cm
                .try_ensure_synthetic_class("cratonvm/test/ForkCaller", 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
            cm.class_store
                .get_mut(fork_caller)
                .expect("just fabricated")
                .loader_id = FORK;
            cm.register_class_name(FORK, "cratonvm/test/ForkCaller", fork_caller);
            let app_caller = cm
                .try_ensure_synthetic_class("cratonvm/test/AppCaller", 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");

            // A name only ONE loader ever defined — the overwhelmingly common
            // case, and the one that must stay on the old path.
            let lonely = cm
                .try_ensure_synthetic_class(LONELY, 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");

            (app_copy, fork_copy, fork_caller, app_caller, lonely)
        };
        assert_ne!(app_copy, fork_copy, "the two copies must be distinct ids");

        let cm = shared.classes.class_manager.read();
        assert!(
            matches!(
                cm.classify_loaded_name(OWNER),
                cratonvm_classloading::NameResolution::Ambiguous { .. }
            ),
            "the fixture must actually produce two definitions of one name — \
             without that this test measures nothing",
        );

        // THE FIX. A call site inside the fork gets the fork's copy.
        assert_eq!(
            loader_faithful_static_owner(&cm, fork_caller, OWNER),
            Some(fork_copy),
            "a compiled invokestatic from a fork-loaded class must dispatch on \
             the fork's copy of the owner, not on the application copy the \
             global name map holds",
        );

        // Negative 1: the caller's loader agrees with the global map. Nothing to
        // correct, and `None` is what keeps `invoke_or_native`'s path.
        assert_eq!(
            loader_faithful_static_owner(&cm, app_caller, OWNER),
            None,
            "when the caller's loader resolves to the same class the global map \
             does, the override must decline so the ordinary dispatch path runs",
        );

        // Negative 2: one definition of the name. This is every call in a
        // single-loader process, and it must not pay for or take the new path.
        assert_eq!(
            loader_faithful_static_owner(&cm, fork_caller, LONELY),
            None,
            "an unambiguous name must decline before any lookup — otherwise \
             every static dispatch in every ordinary process changes route",
        );
        let _ = lonely;
    }

    /// The wrapper the dispatch helper actually calls must keep its two
    /// process-wide pre-gates, which the test above deliberately does not arm.
    ///
    /// A source witness rather than a behavioural test: arming
    /// `any_defining_loader_registered` was a process-global latch that would
    /// leak into every other test in this binary. Matched on text inside the
    /// function's own bounds, never on line numbers.
    #[test]
    fn jit_invokestatic_owner_override_is_gated_before_it_is_consulted() {
        let src = crate::jit::helpers::HELPERS_SRC;
        let start = src
            .find("fn jit_static_owner_override")
            .expect("jit_static_owner_override must exist");
        let body = &src[start..];
        let end = body[1..]
            .find("\nfn ")
            .expect("a top-level function must follow it")
            + 1;
        let body = &body[..end];
        assert!(
            body.contains("info.declaring_class_id == 0"),
            "a call site with no class id must decline — those are the synthetic \
             sites naming JDK classes with one definition",
        );
        assert!(
            body.contains("any_defining_loader_registered("),
            "the process-wide latch must be the first real gate: with no user \
             loader there can be no second copy, and this sits on every compiled \
             static call",
        );
        assert!(
            body.contains("loader_faithful_static_owner"),
            "the wrapper must delegate the decision to the function the test \
             above covers, or that coverage vouches for nothing",
        );
    }

    /// Every door that finds a statically-bound callee's compiled body BY NAME
    /// declines a site whose own loader sees another class under that name.
    /// Five doors, two files; the H2 `TestUpgrade` miscompile came through the
    /// OSR one while the other three were equally open. Source witness for the
    /// same reason as the test above: the latch the predicate reads is
    /// process-global.
    #[test]
    fn every_by_name_callee_door_declines_another_loaders_copy() {
        let helpers = crate::jit::helpers::HELPERS_SRC;
        let start = helpers
            .find("unsafe fn jit_invoke_dispatch_body")
            .expect("jit_invoke_dispatch_body must exist");
        let body = &helpers[start..];
        let body = &body[..body[1..].find("\nunsafe fn ").map_or(body.len(), |e| e + 1)];
        assert!(body.contains("jit_callee_name_is_another_loaders_copy("));
        assert_eq!(
            body.matches("&& by_name_is_the_callers_class").count(),
            2,
            "both the JitCache probe and the compile trigger must be gated",
        );
        let bridge = include_str!("../../runtime/interpreter/jit_bridge.rs");
        assert_eq!(
            bridge
                .matches("crate::jit::helpers::jit_callee_name_is_another_loaders_copy(")
                .count(),
            4,
            "the OSR pending-callee bind, the eager direct bind, the direct \
             bind of calls inside an OSR-door splice (`osr_splice_direct_bind`, \
             round 13 wave 12), and the in-place self-locking callee compile \
             (`compile_unpublished_self_locking_callee`, JIT round 19 wave 4)",
        );
    }

    // -----------------------------------------------------------------------
    // JIT round 21 wave 14, lane hidden-static: a hidden caller's reference to
    // itself is answered by its id, never by the name
    // (`docs/feature-designs/class-resolution-and-caching.md`).
    // -----------------------------------------------------------------------

    /// The body of the top-level function `name` in `src` (to the next
    /// top-level `fn` or `unsafe fn`).
    fn hidden_static_fn_body<'a>(src: &'a str, name: &str) -> &'a str {
        let start = src
            .find(name)
            .unwrap_or_else(|| panic!("{name} must exist"));
        let body = &src[start..];
        let rest = &body[1..];
        let end = [rest.find("\nfn "), rest.find("\nunsafe fn "), rest.find("\npub")]
            .into_iter()
            .flatten()
            .min()
            .map_or(body.len(), |e| e + 1);
        &body[..end]
    }

    #[test]
    fn hidden_self_owner_answers_the_caller_only_for_a_self_reference_under_the_switch() {
        let caller = ClassId::new(4242);
        assert_eq!(hidden_self_owner_decision(true, caller, || true), Some(caller));
        // Switch off (`CRATONVM_JIT_HIDDEN_SELF_BY_ID=0`): the by-name routes.
        assert_eq!(hidden_self_owner_decision(true, caller, || false), None);
        // Not a hidden self-reference: the switch is not even asked.
        assert_eq!(
            hidden_self_owner_decision(false, caller, || panic!("asked for a non-self site")),
            None
        );
    }

    #[test]
    fn hidden_self_owner_declines_a_site_with_no_class_context() {
        // A synthetic site (`declaring_class_id == 0`) is never a hidden
        // caller; answered before any lock or counter.
        let vm = SharedVm::new(crate::config::VmConfig::default());
        assert_eq!(
            jit_hidden_self_owner(&vm, ClassId::new(0), "R21Fz3HiddenAccess1Payload"),
            None
        );
    }

    /// JIT round 21 wave 22, lane hidden-resid (item 6 of
    /// `r21-review-w15-native-findings-FIXED-20261020.md`,
    /// `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER`): the lock-free exit is
    /// opt-in, and with nothing recorded for the caller it never rules a name
    /// out (the class-manager read decides). The table itself is pinned by
    /// `class_manager::tests::the_hidden_self_name_prefilter_rules_out_only_another_name`.
    #[test]
    fn the_hidden_self_prefilter_is_opt_in_and_never_guesses() {
        let vm = SharedVm::new(crate::config::VmConfig::default());
        let bits = &vm.classes.hidden_classes;
        let caller = ClassId::new(77);
        bits.set_self_name_prefilter(false);
        assert!(!hidden_self_prefilter_rules_out(bits, caller, "p/Other"), "off: no exit");
        bits.set_self_name_prefilter(true);
        assert!(
            !hidden_self_prefilter_rules_out(bits, caller, "p/Other"),
            "no recorded name for the caller: ask under the lock"
        );
        // The guard asks it after the bitmap and before the class-manager read.
        let src = crate::jit::helpers::HELPERS_SRC;
        let body = hidden_static_fn_body(src, "\npub(crate) fn jit_hidden_self_owner(");
        let bit = body.find("hidden_class_may_refer_to_itself(").expect("the bitmap");
        let pre = body.find(concat!("hidden_self_prefilter", "_rules_out(&vm")).expect("the prefilter");
        let read = body.find("class_manager.read()").expect("the locked rule");
        assert!(bit < pre && pre < read);
    }

    /// Source witness: the static owner override answers a hidden self-call
    /// BEFORE its process-wide loader latch, which is what kept it from ever
    /// running in a process with no user-defined loader (the probe's).
    #[test]
    fn the_static_owner_override_asks_the_hidden_self_owner_before_the_loader_latch() {
        let src = crate::jit::helpers::HELPERS_SRC;
        let body = hidden_static_fn_body(src, "\nfn jit_static_owner_override(");
        let hidden = body.find("jit_hidden_self_owner(").expect("asks the hidden self owner");
        let latch = body
            .find("any_defining_loader_registered(")
            .expect("keeps the loader latch");
        assert!(hidden < latch, "the hidden self-reference is answered before the latch");
    }

    /// Source witness: the by-name callee guard every by-name door asks
    /// answers a hidden self-reference before its loader latch, so neither
    /// the dispatch helper's `JitCache` probe and compile trigger nor the OSR
    /// and entry doors' direct binds take a body by the hidden class's
    /// class-file name.
    #[test]
    fn the_by_name_callee_guard_refuses_a_hidden_self_reference_before_the_loader_latch() {
        let src = crate::jit::helpers::HELPERS_SRC;
        let body =
            hidden_static_fn_body(src, "\npub(crate) fn jit_callee_name_is_another_loaders_copy(");
        let hidden = body.find("jit_hidden_self_owner(").expect("asks the hidden self owner");
        let latch = body
            .find("any_defining_loader_registered(")
            .expect("keeps the loader latch");
        assert!(hidden < latch);
    }

    /// Source witness: the register-overflow / exception bail answers a hidden
    /// self-call on the caller by id, for `invokestatic` and `invokespecial`.
    #[test]
    fn the_bail_to_the_interpreter_dispatches_a_hidden_self_call_on_the_caller() {
        let src = crate::jit::helpers::HELPERS_SRC;
        let body = hidden_static_fn_body(src, "\nunsafe fn bail_to_interpreter(");
        let asked = body
            .find("jit_hidden_self_owner(")
            .expect("the bail asks the hidden self owner");
        let special = body
            .find("jit_substituted_owner(vm, info).or(hidden_self)")
            .expect("the invokespecial arm takes it after a substituted owner");
        let static_arm = body
            .find("(info.invoke_kind, hidden_self)")
            .expect("the invokestatic arm takes it");
        let on_class = body[static_arm..]
            .find("crate::vm::invoke_static_shared_on_class(")
            .map(|at| static_arm + at)
            .expect("and dispatches on the caller");
        let by_name_static = body
            .find("crate::vm::invoke_static_or_native(")
            .expect("the by-name static entry stays for every other site");
        assert!(asked < special && special < static_arm);
        assert!(on_class < by_name_static);
    }

    /// The JIT native fast-path door must ask §7 the SAME question as the tail
    /// it falls through to.
    ///
    /// A source witness, and it has to be: the defect it pins is that a refusal
    /// here routes the call to `vm_exec::invoke_or_native`, which then runs the
    /// very native this door refused. Nothing observable changed — the same
    /// bytes came out — so no behavioural assertion could have caught it. What
    /// changed was 1 048 576 by-name registry lookups per 1 MiB of
    /// `MessageDigest.update(byte)`, and a `refusals.jit_fastpath_admissions`
    /// column that reported strict mode blocking something it had not blocked.
    ///
    /// The two terms are `invoke_or_native`'s: `has_real` is computed only for
    /// a `SyntheticStub` on `real_protected_stub_class`, and
    /// `bytecode_available` is `has_real || dial`. A future edit that hands the
    /// raw hierarchy probe straight to the resolver again re-opens it, so the
    /// probe must be reachable from this door ONLY through
    /// `jit_fast_native_bytecode_terms`.
    #[test]
    fn jit_fast_native_admission_asks_the_tails_question() {
        let src = crate::jit::helpers::HELPERS_SRC;
        let start = src
            .find("fn jdk_only_admit_jit_fast_native")
            .expect("jdk_only_admit_jit_fast_native must exist");
        let after = &src[start..];
        let end = after[1..]
            .find("\n/// Policy admission + census handle")
            .expect("`admit_jit_fast_native`'s doc comment must follow it")
            + 1;
        let door = &after[..end];
        assert!(
            !door.contains("jit_fast_native_has_bytecode("),
            "the raw hierarchy probe must not be handed to the resolver from \
             this door: `invoke_or_native` computes it only for a SyntheticStub \
             on an allow-listed class, so running it unconditionally here \
             refuses a Bridge that the tail then runs anyway",
        );
        assert!(
            door.contains("jit_fast_native_bytecode_terms("),
            "both of §7's inputs must come from the one predicate that mirrors \
             `invoke_or_native`, or the two doors can drift apart again",
        );
        assert!(
            door.contains("!has_real,"),
            "`compat_native_wins` must be the tail's `!has_real`, not a \
             hard-coded `true`",
        );

        // The mirrored predicate must actually mirror: both terms, the
        // allow-list gate on the first, and the dial on the second.
        let start = src
            .find("fn jit_fast_native_bytecode_terms")
            .expect("jit_fast_native_bytecode_terms must exist");
        let after = &src[start..];
        let end = after[1..]
            .find("\n/// ")
            .expect("a doc comment must follow it")
            + 1;
        let terms = &after[..end];
        assert!(
            terms.contains("NativeKind::SyntheticStub")
                && terms.contains("real_protected_stub_class("),
            "`has_real` is gated on a SyntheticStub whose class is allow-listed \
             — that gate IS the difference this function exists to preserve",
        );
        assert!(
            terms.contains("jdk_only_dial_yields_to_bytecode("),
            "the enforcement dial is the term that actually makes the tail \
             yield, so it must survive here or arming a class stops reaching \
             this door",
        );
        assert!(
            terms.contains("(has_real, has_real || dial)"),
            "the pair must be exactly `invoke_or_native`'s `has_real` and its \
             `has_real || dial`",
        );
    }

    // -----------------------------------------------------------------------
    // Leaf natives: the set, and the one thing that could make skipping
    // `invoke_or_native` wrong
    // -----------------------------------------------------------------------

    /// Populate a registry the way `vm_init` does for **real-JDK mode**, so the
    /// leaf set under test is the one a real run has rather than a hand-written
    /// list.
    ///
    /// `register_essential_natives_with_shims` and not `register_builtins`,
    /// deliberately: the latter is `#[cfg(feature = "synthetic-jdk")]`-gated in
    /// `native-builtins` and degrades to a no-op shim
    /// (`vm/src/native/builtins.rs`) without it, so a test built on it would
    /// pass vacuously — an empty registry has no non-conforming leaf — in
    /// exactly the configuration it was meant to cover. This entry point is
    /// ungated and is the one every `--java-home` run takes, which is also the
    /// mode the probes measure.
    fn registry_with_builtins() -> cratonvm_native_api::NativeMethodRegistry {
        let mut r = cratonvm_native_api::NativeMethodRegistry::new();
        cratonvm_native_builtins::register_essential_natives(&mut r);
        r
    }

    /// Round 13 wave 10 (lane callcost4): only the VM's own `MethodHandle`
    /// carrier class opens the `invoke` / `invokeExact` family to the site
    /// cache (`method_handle_carrier_site`), and the registry holds the erased
    /// natives `resolve_method_handle_carrier_native` resolves them to.
    #[test]
    fn only_the_method_handle_carrier_opens_the_invoke_family() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SITE_CACHE_MH_INVOKE") {
            return;
        }
        assert!(method_handle_carrier_site("java/lang/invoke/MethodHandle"));
        for other in [
            "java/lang/invoke/DirectMethodHandle",
            "java/lang/invoke/BoundMethodHandle$Species_L",
            "java/lang/foreign/DowncallHandle",
            "java/lang/invoke/VarHandle",
            "",
        ] {
            assert!(!method_handle_carrier_site(other), "{other}");
        }
        // The family stays special-cased for every other receiver.
        assert!(site_name_is_special_cased("invoke"));
        assert!(site_name_is_special_cased("invokeExact"));
        // `register_essential_natives` does not register these two: the VM's
        // boot (`vm_init.rs`) calls `register_t4_method_handle_invoke`
        // itself, after the essential set, and so does this registry.
        let mut registry = registry_with_builtins();
        cratonvm_native_builtins::lang_invoke::register_t4_method_handle_invoke(&mut registry);
        for name in ["invoke", "invokeExact"] {
            assert!(
                registry
                    .resolve_id(
                        "java/lang/invoke/MethodHandle",
                        name,
                        "([Ljava/lang/Object;)Ljava/lang/Object;"
                    )
                    .is_some(),
                "MethodHandle.{name} is registered under the erased descriptor"
            );
        }
    }

    /// The kill switch has to actually kill, and the default has to be ON.
    ///
    /// Both halves matter and they fail differently. A switch that silently
    /// does nothing is worse than no switch: the next investigation runs with
    /// `-native-site-cache`, sees the failure anyway, and CLEARS this path as a
    /// suspect when it never left the run. And an accidental default-OFF gives
    /// back the AQS pair's 2,687 → 1,229 ns with nothing saying so.
    ///
    /// Measured on `BatchDataMongoAutoConfigurationTests`, this path on:
    /// 8 of 8 runs failed before `383e7f5cf`, 0 of 14 after it. See
    /// [`native_site_cache_enabled`].
    #[test]
    fn native_site_cache_default_is_on_and_the_kill_switch_kills() {
        assert!(
            !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_NATIVE_SITE_CACHE"),
            "this test asserts the DEFAULT; unset CRATONVM_JIT_NO_NATIVE_SITE_CACHE to run it"
        );
        assert!(
            native_site_cache_enabled(),
            "the JIT native site cache is default-ON; if it has been turned off \
             by default, say why where the perf it gives back is documented"
        );
        // The token has to reach the reader's key. `CRATONVM_JIT=-native-site-cache`
        // sets `CRATONVM_JIT_NO_NATIVE_SITE_CACHE`, and nothing else does.
        let entry = cratonvm_types::flag_groups::INVENTORY
            .iter()
            .find(|e| e.token == "native-site-cache")
            .expect("`CRATONVM_JIT=-native-site-cache` must stay declared");
        assert_eq!(
            entry.off_key,
            Some("CRATONVM_JIT_NO_NATIVE_SITE_CACHE"),
            "the kill switch's off_key must be the key `native_site_cache_enabled` reads, \
             or `-native-site-cache` is a no-op that reads as a cleared suspect"
        );
    }

    /// The leaf fast path skips `vm_exec::invoke_or_native` entirely, and that
    /// function opens with a cascade of hand-written gates that can route a
    /// call somewhere OTHER than its own registry slot. If a triple is ever
    /// marked leaf whose method name that cascade claims, compiled code would
    /// silently start taking a different call than the interpreter — a
    /// miscompile with no symptom at the registration site.
    ///
    /// `site_name_is_special_cased` is the fill-time refusal that prevents
    /// it. This pins the two together: the actual boot-time leaf set, against
    /// the actual predicate. It still matters with the path default-OFF: the
    /// refusal is what a re-landing has to keep.
    #[test]
    fn leaf_native_sites_avoid_invoke_or_native_special_cases() {
        let registry = registry_with_builtins();
        let leaves = registry.leaf_registrations();

        // Floor, not just "none are bad": a refactor that silently stopped
        // applying `set_leaf` would leave this test green and vacuous while
        // every leaf quietly went back to paying the funnel.
        assert!(
            leaves.len() >= 20,
            "expected the boot leaf set to be populated, got {} entries: {:?}",
            leaves.len(),
            leaves
        );

        for (class, method, descriptor) in &leaves {
            assert!(
                !site_name_is_special_cased(method),
                "{class}.{method}{descriptor} is registered as a leaf, but \
                 `invoke_or_native` special-cases the method name `{method}` \
                 before it reaches the registry — the JIT leaf fast path would \
                 take a different call than the interpreter. Either drop the \
                 leaf claim or remove the special case."
            );
        }
    }

    /// The site cache serves EVERY registered native by default.
    ///
    /// This test was written asserting the opposite, on a measurement that was
    /// correct and a conclusion that was not: `CacheAutoConfigurationTests`
    /// went 53 failures to 0 when this mode was narrowed to `leaf`, so the
    /// widening looked like the defect. It was the amplifier —
    /// `dev`'s `383e7f5cf` (a recycled `JitInvokeInfo` address serving one call
    /// site's memo to another) was the defect, and with it in, `all` measures 0
    /// failures twice over. See [`SiteCacheMode`] for the full correction.
    ///
    /// It is kept, inverted, because the regression it now guards is the one
    /// that actually happened: a plausible measurement talking someone into
    /// gating a perf win around a defect that lives somewhere else.
    ///
    /// Asserted through the mode's own predicates rather than by re-deriving
    /// the rule, and in BOTH directions — a predicate answering the same for
    /// every mode would satisfy a one-sided check while making the lever inert.
    #[test]
    fn site_cache_admits_every_registered_native_by_default() {
        assert_eq!(
            site_cache_mode_from(None),
            SiteCacheMode::All,
            "the default is `All`: gating it would cost 836631dcc's win to work \
             around 383e7f5cf's recycled-key defect, which is fixed. See the \
             correction on `SiteCacheMode`."
        );
        assert!(
            SiteCacheMode::All.admits_non_leaf(),
            "the default must actually admit non-leaf natives, or the fast path \
             is inert and the default reads as a decision nobody made"
        );
        // Both directions. A predicate that answered the same for every mode
        // would satisfy a one-sided check while making the lever inert, which
        // is the failure this whole family keeps producing.
        assert!(SiteCacheMode::NoSuperWalk.admits_non_leaf());
        assert!(!SiteCacheMode::Leaf.admits_non_leaf());
        assert!(!SiteCacheMode::Off.admits_non_leaf());
    }

    /// Every spelling the lever accepts, and the fact that it rejects anything
    /// else loudly. A silently-ignored value would read as "this mode made no
    /// difference", which is exactly the verdict the lever exists to produce.
    #[test]
    fn site_cache_mode_parses_every_spelling() {
        assert_eq!(site_cache_mode_from(Some("")), SiteCacheMode::All);
        assert_eq!(site_cache_mode_from(Some("all")), SiteCacheMode::All);
        assert_eq!(site_cache_mode_from(Some(" all ")), SiteCacheMode::All);
        assert_eq!(site_cache_mode_from(Some("leaf")), SiteCacheMode::Leaf);
        assert_eq!(
            site_cache_mode_from(Some("no-super-walk")),
            SiteCacheMode::NoSuperWalk
        );
        assert_eq!(site_cache_mode_from(Some("off")), SiteCacheMode::Off);
    }

    #[test]
    #[should_panic(expected = "is not one of")]
    fn site_cache_mode_rejects_an_unknown_spelling() {
        let _ = site_cache_mode_from(Some("leaves"));
    }

    /// The leaf set is an audited list, so it is worth stating what is on it —
    /// and, more importantly, what is deliberately NOT. The CAS / fetch-add
    /// members of the atomics go through the monitor table's per-object CAS
    /// lock, which is a lock this thread can be made to wait on; contract
    /// item 2 excludes exactly that, so they must stay on the funnel.
    #[test]
    fn the_leaf_set_holds_the_accessors_and_not_the_locking_atomics() {
        let registry = registry_with_builtins();
        let is_leaf = |class: &str, method: &str, descriptor: &str| {
            registry
                .resolve_id(class, method, descriptor)
                .is_some_and(|id| registry.is_leaf_id(id))
        };

        let ai = "java/util/concurrent/atomic/AtomicInteger";
        assert!(is_leaf(ai, "get", "()I"), "AtomicInteger.get is a leaf");
        assert!(is_leaf(ai, "set", "(I)V"), "AtomicInteger.set is a leaf");
        assert!(is_leaf("java/lang/System", "nanoTime", "()J"));
        assert!(is_leaf("java/lang/Math", "abs", "(I)I"));

        assert!(
            !is_leaf(ai, "compareAndSet", "(II)Z"),
            "compareAndSet takes the CAS lock — it must not claim the leaf contract"
        );
        assert!(
            !is_leaf(ai, "getAndIncrement", "()I"),
            "getAndIncrement goes through atomic_fetch_add_int — not a leaf"
        );
        assert!(
            !is_leaf(ai, "toString", "()Ljava/lang/String;"),
            "toString allocates a String — not a leaf"
        );
        assert!(
            !is_leaf("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
            "currentThread's registered body allocates the mirror on first call; \
             the JIT serves it through LeafNativeKind::ThreadCurrentThread, which \
             falls through to the ordinary dispatcher instead of claiming leaf"
        );
    }

    // -----------------------------------------------------------------------
    // Per-VM keying of the JIT dispatch memos
    // (vm-jit-cache-keying.md)
    // -----------------------------------------------------------------------

    /// Serializes the tests that reset the process-global, VM-owned bitmap
    /// `class_init_memo` (`system_class_memo`, the other one, was deleted in
    /// i1 wave 5). Nothing else in the unit-test suite reaches it — only
    /// `jit_getstatic` does, and that needs compiled code — but two of these
    /// tests running concurrently would clear each other's claim.
    static MEMO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn memo_test_guard() -> std::sync::MutexGuard<'static, ()> {
        MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -----------------------------------------------------------------------
    // The pending JIT exception is thread-resident, not TLS-resident
    // (jit-signals-root-gap.md)
    // -----------------------------------------------------------------------

    fn scratch_thread(id: u64) -> JvmThread {
        use crate::threading::jvm_thread::ThreadId;
        JvmThread::new(ThreadId(id), "jit-signals-test")
    }

    /// Clear this test thread's TLS signal block so a test never inherits the
    /// scalar flags of whichever test ran before it on the same OS thread
    /// (libtest may reuse one under `--test-threads=1`).
    fn reset_tls_signals() {
        let mut scratch = scratch_thread(u64::MAX);
        let _ = take_all_jit_signals(&mut scratch);
    }

    /// A throwable stashed for thread A must be reachable **through A's
    /// `JvmThread`** — that is the whole point of the move, because a
    /// `thread_local!` is invisible to the collector's root scan and post-move
    /// fixup. Draining a different thread must not see it.
    #[test]
    fn a_stashed_jit_exception_is_reachable_through_the_thread_it_belongs_to() {
        reset_tls_signals();
        // SAFETY: an aligned, non-null address that is never dereferenced —
        // the signal plumbing only moves the `ObjectRef` around.
        let exc = unsafe { ObjectRef::from_raw(0x4000usize as *mut u8) };
        let mut a = scratch_thread(1);
        let mut b = scratch_thread(2);

        set_jit_pending_exception(&mut a, exc);

        assert_eq!(
            a.jit_pending_exception.map(|o| o.as_ptr() as usize),
            Some(0x4000),
            "the stash must land in the GC-rooted `JvmThread` slot; if this is \
             None the reference has gone back into a thread-local and is \
             unrooted again"
        );
        assert!(
            b.jit_pending_exception.is_none(),
            "another thread's slot must stay empty"
        );

        // The drain is per-thread and consuming.
        assert!(
            take_all_jit_signals(&mut b).exception.is_none(),
            "draining B must not steal A's pending exception"
        );
        assert_eq!(
            take_all_jit_signals(&mut a)
                .exception
                .map(|o| o.as_ptr() as usize),
            Some(0x4000)
        );
        assert!(
            a.jit_pending_exception.is_none(),
            "the drain must consume the slot"
        );
    }

    /// `take_jit_pending_exception` is the interpreter's other drain door; it
    /// must consume the same slot.
    #[test]
    fn take_jit_pending_exception_consumes_the_thread_slot() {
        reset_tls_signals();
        // SAFETY: never dereferenced.
        let exc = unsafe { ObjectRef::from_raw(0x4100usize as *mut u8) };
        let mut t = scratch_thread(3);

        stash_jit_pending_exception(&mut t, exc);
        assert_eq!(
            take_jit_pending_exception(&mut t).map(|o| o.as_ptr() as usize),
            Some(0x4100)
        );
        assert!(take_jit_pending_exception(&mut t).is_none());
    }

    /// The RBC.6 `athrow_bci` stays in TLS (it is a scalar the collector has no
    /// interest in) but must still travel with the thread-resident throwable
    /// through one drain, and reset afterwards.
    #[test]
    fn the_athrow_bci_still_pairs_with_the_thread_resident_exception() {
        reset_tls_signals();
        // SAFETY: never dereferenced.
        let exc = unsafe { ObjectRef::from_raw(0x4200usize as *mut u8) };
        let mut t = scratch_thread(4);

        set_jit_pending_exception_with_bci(&mut t, exc, 17);
        assert_eq!(peek_jit_athrow_bci(), 17);

        let drained = take_all_jit_signals(&mut t);
        assert_eq!(drained.exception.map(|o| o.as_ptr() as usize), Some(0x4200));
        assert_eq!(drained.athrow_bci, 17);
        assert_eq!(
            peek_jit_athrow_bci(),
            -1,
            "the drain must reset the bci so it cannot leak onto the next \
             unrelated exception"
        );

        // The general setter always clears the bci back to "unknown".
        set_jit_pending_exception(&mut t, exc);
        assert_eq!(peek_jit_athrow_bci(), -1);
        let _ = take_all_jit_signals(&mut t);
    }

    fn gce_e1f_info(class: &'static str, method: &'static str) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: class,
            method_name: method,
            descriptor: "(Ljava/nio/file/Path;)V",
            num_jit_args: 1,
            return_type: b'V',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    /// gce e1/f: a throwable that escaped a not-entrant re-dispatch is
    /// recorded for the callee it escaped, spent by the first site that asks,
    /// and never survives a new throwable or a full drain.
    #[test]
    fn gce_e1f_a_redispatch_escape_speaks_once_for_its_own_callee() {
        reset_tls_signals();
        // SAFETY: never dereferenced.
        let exc = unsafe { ObjectRef::from_raw(0x4400usize as *mut u8) };
        let mut t = scratch_thread(5);
        let m = gce_e1f_info("p/Sub", "m");
        // A site naming a subclass spells the method identically.
        let same = gce_e1f_info("p/Base", "m");
        let other = gce_e1f_info("p/Base", "n");

        // No throwable pending: a real `Long.MIN_VALUE` records nothing.
        assert_eq!(note_redispatch_escape(&t, &m, i64::MIN), i64::MIN);
        assert!(!take_redispatch_escape_for(&m));
        // A normal return records nothing.
        set_jit_pending_exception(&mut t, exc);
        assert_eq!(note_redispatch_escape(&t, &m, 7), 7);
        assert!(!take_redispatch_escape_for(&m));

        // The sentinel with a throwable: recorded (when the switch is on) and
        // spent by the first ask.
        let on = not_entrant_escape_enabled();
        note_redispatch_escape(&t, &m, i64::MIN);
        assert_eq!(take_redispatch_escape_for(&same), on);
        assert!(!take_redispatch_escape_for(&same), "spent");

        // Another callee's site drops it without using it.
        note_redispatch_escape(&t, &m, i64::MIN);
        assert!(!take_redispatch_escape_for(&other));
        assert!(!take_redispatch_escape_for(&m), "dropped by the other ask");

        // A new or re-stashed throwable clears it, and so does a full drain.
        note_redispatch_escape(&t, &m, i64::MIN);
        set_jit_pending_exception(&mut t, exc);
        assert!(!take_redispatch_escape_for(&m));
        note_redispatch_escape(&t, &m, i64::MIN);
        set_jit_pending_exception_with_bci(&mut t, exc, 3);
        assert!(!take_redispatch_escape_for(&m));
        note_redispatch_escape(&t, &m, i64::MIN);
        let _ = take_all_jit_signals(&mut t);
        assert!(!take_redispatch_escape_for(&m));
    }

    /// Interpreter round i5 wave 103, lane L2: an interpreter door drains the
    /// record WITH the throwable and asks it about the method it entered. A
    /// body retired between the door's entrant check and the call ran its
    /// not-entrant stub, whose re-dispatch let the throwable escape; the door
    /// must see "escaped", not "pc unknown" (the pc-unknown search matched
    /// `outsideTryStep`'s typed row by class alone: 19997 of 20000 under load).
    /// A stamped throwable, another method, or no throwable reads "not
    /// escaped".
    #[test]
    fn i103_l2_a_door_drain_sees_an_escape_of_the_method_it_entered() {
        reset_tls_signals();
        // SAFETY: never dereferenced.
        let exc = unsafe { ObjectRef::from_raw(0x4400usize as *mut u8) };
        let mut t = scratch_thread(5);
        let m = gce_e1f_info("p/Sub", "m");
        let on = not_entrant_escape_enabled();

        // The re-dispatch re-posts the throwable unstamped, then records.
        set_jit_pending_exception(&mut t, exc);
        note_redispatch_escape(&t, &m, i64::MIN);
        let sig = take_all_jit_signals(&mut t);
        assert_eq!(sig.escaped_a_redispatch_of("m", m.descriptor), on);
        assert!(!sig.escaped_a_redispatch_of("n", m.descriptor));
        assert!(!take_redispatch_escape_for(&m), "drained");

        // A compiled exit that stamped after the record names another throw.
        set_jit_pending_exception(&mut t, exc);
        note_redispatch_escape(&t, &m, i64::MIN);
        // SAFETY: a bare stamp setter; no pointer.
        unsafe { jit_set_throw_bci(20) };
        let sig = take_all_jit_signals(&mut t);
        assert!(!sig.escaped_a_redispatch_of("m", m.descriptor));

        // Nothing pending: nothing escaped.
        let sig = take_all_jit_signals(&mut t);
        assert!(!sig.escaped_a_redispatch_of("m", m.descriptor));
    }

    /// Interpreter round i5 wave 106, lane L2 (page `i101-L2-pc-unknown-...`):
    /// a dispatch-error re-post records that the throwable left the method it
    /// was posted for, so the call-site services and the doors propagate it
    /// without a pc-unknown search (`note_dispatch_error_escape`, called by
    /// `handle_jit_dispatch_error`). A compiled exit's stamp retires the
    /// record; nothing pending writes none; `=0` writes none.
    #[test]
    fn i106_l2_a_dispatch_error_repost_records_the_escape() {
        reset_tls_signals();
        // SAFETY: never dereferenced.
        let exc = unsafe { ObjectRef::from_raw(0x4800usize as *mut u8) };
        let mut t = scratch_thread(5);
        let m = gce_e1f_info("p/Sub", "rethrow");
        let on = not_entrant_escape_enabled();

        // The service side: the record names the site's callee.
        set_jit_pending_exception(&mut t, exc);
        note_dispatch_error_escape(true, m.method_name, m.descriptor);
        assert_eq!(take_redispatch_escape_for(&m), on);
        assert!(!take_redispatch_escape_for(&m), "taken");

        // The door side: an unstamped throwable with the record.
        set_jit_pending_exception(&mut t, exc);
        note_dispatch_error_escape(true, m.method_name, m.descriptor);
        let sig = take_all_jit_signals(&mut t);
        assert_eq!(sig.escaped_a_redispatch_of("rethrow", m.descriptor), on);

        // A compiled exit stamped after the re-post: the record is retired,
        // for the services as well as for the doors.
        set_jit_pending_exception(&mut t, exc);
        note_dispatch_error_escape(true, m.method_name, m.descriptor);
        // SAFETY: a bare stamp setter; no pointer.
        unsafe { jit_set_throw_bci(7) };
        assert!(
            !take_redispatch_escape_for(&m),
            "a stamp retires the record"
        );
        let _ = take_all_jit_signals(&mut t);

        // Nothing re-posted: nothing recorded.
        note_dispatch_error_escape(false, m.method_name, m.descriptor);
        assert!(!take_redispatch_escape_for(&m));

        // The kill switch.
        set_jit_pending_exception(&mut t, exc);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DISPATCH_ERROR_ESCAPE_RECORD", Some("0"))],
            || note_dispatch_error_escape(true, m.method_name, m.descriptor),
        );
        assert!(!take_redispatch_escape_for(&m), "=0 records nothing");
        let _ = take_all_jit_signals(&mut t);
    }

    /// `jit_pending_exception_is_set` resolves the thread through the raw
    /// `JIT_THREAD` pointer. With no JIT thread installed (this test thread
    /// never enters compiled code) it must answer "nothing pending" rather
    /// than dereference null.
    #[test]
    fn the_pending_exception_peek_is_null_safe_without_a_jit_thread() {
        // Establish the premise rather than assuming it: another test may have
        // run on this OS thread first.
        clear_jit_thread();
        assert!(current_jit_thread_ptr().is_null());
        assert!(!jit_pending_exception_is_set());
    }

    #[test]
    fn jit_site_key_separates_vm_identities() {
        let info_ptr = &INTEGER_VALUE_OF_INFO as *const JitInvokeInfo as usize;
        assert_eq!(jit_site_key(1, info_ptr), jit_site_key(1, info_ptr));
        assert_ne!(
            jit_site_key(1, info_ptr),
            jit_site_key(2, info_ptr),
            "the same JitInvokeInfo address in two VMs must not be one key"
        );
        assert_ne!(jit_site_key(1, info_ptr), jit_site_key(1, info_ptr + 8));
    }

    /// The concrete premise behind [`JitSiteKey`]: the direct helpers dispatch
    /// through the address of a `static JitInvokeInfo`, which is literally the
    /// same `usize` in every VM in the process. Before the fix that address
    /// WAS the whole cache key.
    #[test]
    fn static_invoke_infos_share_one_address_across_vms() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let a = SharedVm::new(VmConfig::default());
        let b = SharedVm::new(VmConfig::default());
        assert_ne!(a.vm_identity, b.vm_identity);

        for ptr in [
            &INTEGER_VALUE_OF_INFO as *const JitInvokeInfo as usize,
            &INTEGER_INT_VALUE_INFO as *const JitInvokeInfo as usize,
            &HASHMAP_PUT_DIRECT_INFO as *const JitInvokeInfo as usize,
            &HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as usize,
            &CONCURRENT_HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as usize,
            &STRING_LATIN1_LOWER_DIRECT_INFO as *const JitInvokeInfo as usize,
        ] {
            assert_ne!(
                jit_site_key(a.vm_identity, ptr),
                jit_site_key(b.vm_identity, ptr),
                "process-global JitInvokeInfo at 0x{ptr:x} must key differently per VM"
            );
        }
    }

    // -----------------------------------------------------------------------
    // §4 census — the thin direct-call helpers declare themselves uncounted
    // (docs/feature-designs/jdk-only-mode.md)
    // -----------------------------------------------------------------------

    fn census_probe_native(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(None)
    }

    /// Every helper the JIT can bind a call site to must be on the bypass list.
    ///
    /// The list is the only thing standing between a reader and a census row
    /// that claims an exact `invocations` count it cannot deliver, and it is a
    /// hand-written table over the same triples the `JitInvokeInfo` statics
    /// carry. A helper added with a new `*_DIRECT_INFO` and no list row fails
    /// silently and in the one direction `mark_invocations_incomplete`'s doc
    /// says this instrument must never err.
    ///
    /// Note what this test does NOT assert: that every list row has an info.
    /// `ConcurrentHashMap.get` deliberately has none — see
    /// [`DIRECT_CALL_HELPER_NATIVES`] for why both concurrent-map slots are
    /// listed while only the interface one appears in a `JitInvokeInfo`.
    #[test]
    fn every_thin_direct_call_helper_is_on_the_census_bypass_list() {
        for info in [
            &INTEGER_VALUE_OF_INFO,
            &INTEGER_INT_VALUE_INFO,
            &LONG_VALUE_OF_INFO,
            &LONG_LONG_VALUE_INFO,
            &HASHMAP_PUT_DIRECT_INFO,
            &HASHMAP_GET_DIRECT_INFO,
            &CONCURRENT_HASHMAP_GET_DIRECT_INFO,
            &STRING_LATIN1_LOWER_DIRECT_INFO,
            &THREAD_CURRENT_THREAD_INFO,
            &THREAD_LOCAL_GET_DIRECT_INFO,
            &THREAD_LOCAL_SET_DIRECT_INFO,
            // 2026-09-22: everything below was already served by a thin helper
            // and was missing from the list, so eight families of census rows
            // claimed an exact total a compiled site had bypassed.
            &MD_UPDATE_BYTE_INFO,
            &NIO_BYTEBUFFER_GET_BYTE_INFO,
            &NIO_BYTEBUFFER_PUT_BYTE_INFO,
            &BUFFER_SESSION_INFO,
            &PRECONDITIONS_CHECK_INDEX_INFO,
            &SCOPED_MEMORY_GET_INT_INFO,
            &SCOPED_MEMORY_GET_SHORT_INFO,
            &SCOPED_MEMORY_GET_LONG_INFO,
            &SCOPED_MEMORY_GET_CHAR_INFO,
            &SCOPED_MEMORY_PUT_INT_INFO,
            &SCOPED_MEMORY_PUT_SHORT_INFO,
            &SCOPED_MEMORY_PUT_LONG_INFO,
            &SCOPED_MEMORY_PUT_CHAR_INFO,
            &SCOPED_MEMORY_GET_INT_INTERNAL_INFO,
            &SCOPED_MEMORY_GET_SHORT_INTERNAL_INFO,
            &SCOPED_MEMORY_GET_LONG_INTERNAL_INFO,
            &SCOPED_MEMORY_GET_CHAR_INTERNAL_INFO,
            &SCOPED_MEMORY_PUT_INT_INTERNAL_INFO,
            &SCOPED_MEMORY_PUT_SHORT_INTERNAL_INFO,
            &SCOPED_MEMORY_PUT_LONG_INTERNAL_INFO,
            &SCOPED_MEMORY_PUT_CHAR_INTERNAL_INFO,
            &UNSAFE_GET_BYTE_INFO,
            &UNSAFE_GET_SHORT_INFO,
            &UNSAFE_GET_INT_INFO,
            &UNSAFE_GET_LONG_INFO,
            &UNSAFE_GET_REFERENCE_INFO,
            &UNSAFE_GET_REFERENCE_VOLATILE_INFO,
            &UNSAFE_PUT_BYTE_INFO,
            &UNSAFE_PUT_SHORT_INFO,
            &UNSAFE_PUT_INT_INFO,
            &UNSAFE_PUT_LONG_INFO,
            &UNSAFE_CAS_INT_INFO,
            &UNSAFE_CAS_LONG_INFO,
            &OBJECT_GET_CLASS_INFO,
            // nio21 wave 3, lane L4.
            &UNSAFE_GET_INT_VOLATILE_INFO,
            &UNSAFE_GET_LONG_VOLATILE_INFO,
            &UNSAFE_PUT_INT_VOLATILE_INFO,
            &UNSAFE_PUT_LONG_VOLATILE_INFO,
            &UNSAFE_PUT_REFERENCE_VOLATILE_INFO,
            &UNSAFE_CAS_REFERENCE_INFO,
            &UNSAFE_FULL_FENCE_INFO,
            &REFERENCE_REFERS_TO0_INFO,
            &PHANTOM_REFERENCE_REFERS_TO0_INFO,
            &THREAD_CURRENT_CARRIER_THREAD_INFO,
            &SYSTEM_ARRAYCOPY_INFO,
        ] {
            assert!(
                DIRECT_CALL_HELPER_NATIVES
                    .iter()
                    .any(|&(class, method, descriptor)| {
                        class == info.class_name
                            && method == info.method_name
                            && descriptor == info.descriptor
                    }),
                "{}.{}{} is served by a thin direct-call helper but is missing \
                 from DIRECT_CALL_HELPER_NATIVES, so its census row would report \
                 `invocations_complete: true` for a count the helper never \
                 increments",
                info.class_name,
                info.method_name,
                info.descriptor
            );
        }

        // Signature-polymorphic helpers own typed site descriptors, while the
        // registry owns the erased array descriptor. Check the actual bind
        // families so adding a mode cannot leave its native count falsely exact.
        for (modes, descriptor) in [
            (
                cratonvm_jit::VARHANDLE_READ_MODES.as_slice(),
                "([Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (
                cratonvm_jit::VARHANDLE_WRITE_MODES.as_slice(),
                "([Ljava/lang/Object;)V",
            ),
            (["compareAndSet"].as_slice(), "([Ljava/lang/Object;)Z"),
        ] {
            for method in modes {
                assert!(
                    DIRECT_CALL_HELPER_NATIVES.contains(&(
                        "java/lang/invoke/VarHandle",
                        *method,
                        descriptor
                    )),
                    "{method}{descriptor} has a direct helper but an exact-looking census"
                );
            }
        }

        // Duplicate-free. A duplicate is harmless at run time — the mark is
        // idempotent — but it would let the count assertions below pass while
        // one triple was covered twice and another not at all.
        let mut seen: Vec<(&str, &str, &str)> = Vec::new();
        for row in DIRECT_CALL_HELPER_NATIVES {
            assert!(
                !seen.contains(&row),
                "DIRECT_CALL_HELPER_NATIVES lists {row:?} twice"
            );
            seen.push(row);
        }
    }

    /// Marking must turn exactly the helper-served rows into floors, leave a
    /// control row exact, and leave the tally itself alone.
    ///
    /// The last part is the one worth pinning: the whole design rests on the
    /// count staying readable and staying a floor. A mark that zeroed or
    /// otherwise disturbed `invocations` would replace an under-report with no
    /// report, which is worse — `G33-1` §4's safe reading ("did this ever run,
    /// through a counted path") depends on the number surviving.
    #[test]
    fn marking_the_direct_call_helpers_turns_their_rows_into_floors() {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        registry.with_category(cratonvm_native_api::NativeKind::Bridge, |r| {
            for (class, method, descriptor) in DIRECT_CALL_HELPER_NATIVES {
                r.register(class, method, descriptor, census_probe_native);
            }
            // `G33-1` §2's control: a native with no direct helper, measured at
            // exactly 100,000 of 100,000 calls in every arm. It must not pick
            // up doubt from its neighbours.
            r.register(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
                census_probe_native,
            );
        });

        let control = registry
            .resolve_id(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
            )
            .expect("control registered");
        let hashmap_put = registry
            .resolve_id(
                "java/util/HashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            )
            .expect("registered");

        // Premise: nothing is marked until a bypassing path says so.
        assert_eq!(registry.slots_with_incomplete_invocations(), 0);
        // The counted dispatches this row DID see — the 2,000 of `G33-1` §2's
        // table, in miniature.
        registry.record_invocation(hashmap_put);
        registry.record_invocation(hashmap_put);

        let marked = mark_direct_call_helper_natives_incomplete_in(&registry);
        assert_eq!(
            marked,
            DIRECT_CALL_HELPER_NATIVES.len(),
            "every listed triple was registered above, so every one must resolve"
        );

        for (class, method, descriptor) in DIRECT_CALL_HELPER_NATIVES {
            let id = registry
                .resolve_id(class, method, descriptor)
                .expect("registered");
            assert_eq!(
                registry.invocations_complete(id),
                Some(false),
                "{class}.{method}{descriptor} must report its count as a floor"
            );
        }
        assert_eq!(
            registry.invocations_complete(control),
            Some(true),
            "the flag is per slot: a native with no direct helper stays exact"
        );
        assert_eq!(
            registry.slots_with_incomplete_invocations(),
            DIRECT_CALL_HELPER_NATIVES.len()
        );
        assert_eq!(
            registry.invocations_of_id(hashmap_put),
            Some(2),
            "the count must survive marking — it is the floor the reader falls \
             back on, not a value the flag replaces"
        );

        // Idempotent: `build_helpers_for` runs once per compile, and the latch
        // is an optimisation, not a correctness requirement.
        assert_eq!(
            mark_direct_call_helper_natives_incomplete_in(&registry),
            DIRECT_CALL_HELPER_NATIVES.len()
        );
        assert_eq!(
            registry.slots_with_incomplete_invocations(),
            DIRECT_CALL_HELPER_NATIVES.len()
        );

        // The census is the door a reader actually uses. Every helper-served
        // row must carry the doubt out through it.
        let census = registry.census();
        for entry in &census {
            let listed = DIRECT_CALL_HELPER_NATIVES
                .iter()
                .any(|&(class, method, descriptor)| {
                    entry.class == class && entry.name == method && entry.descriptor == descriptor
                });
            assert_eq!(
                entry.invocations_complete, !listed,
                "{}.{}{} census row: invocations_complete should be {}",
                entry.class, entry.name, entry.descriptor, !listed
            );
        }
    }

    /// A registry that never registered the collection natives must still be
    /// marked correctly — with nothing.
    ///
    /// `register_collections_natives` is a separate registrar from
    /// `register_essential_natives`, and `jit::helpers` is reached from VM-less
    /// unit tests as well as from a live VM. Resolving a triple that is not
    /// there must be a quiet no-op, not a panic on the compile path.
    #[test]
    fn marking_an_empty_registry_marks_nothing_and_does_not_panic() {
        let registry = cratonvm_native_api::NativeMethodRegistry::new();
        assert_eq!(mark_direct_call_helper_natives_incomplete_in(&registry), 0);
        assert_eq!(registry.slots_with_incomplete_invocations(), 0);
    }

    #[test]
    fn dispatch_cache_does_not_serve_another_vms_compiled_entry() {
        let info_ptr = &HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as usize;
        let vm_a = jit_site_key(11, info_ptr);
        let vm_b = jit_site_key(12, info_ptr);

        DISPATCH_CACHE.with(|dc| {
            dc.borrow_mut().insert(
                vm_a,
                DispatchCache {
                    entry: 0xdead_beef,
                    needs_context: false,
                    owner: None,
                    rerun_pins: false,
                },
            );
        });

        let from_a = DISPATCH_CACHE.with(|dc| dc.borrow().get(&vm_a).map(|c| c.entry));
        let from_b = DISPATCH_CACHE.with(|dc| dc.borrow().get(&vm_b).map(|c| c.entry));
        DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());

        assert_eq!(from_a, Some(0xdead_beef), "the owning VM must still hit");
        assert_eq!(
            from_b, None,
            "another VM must never be handed a raw entry pointer into VM A's code cache"
        );
    }

    #[test]
    fn virtual_dispatch_cache_does_not_serve_another_vms_compiled_entry() {
        let info_ptr = &CONCURRENT_HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as usize;
        // The SAME receiver class id in both VMs — the collision that makes
        // this cache miscompile-grade rather than merely stale.
        let cid = 4242u32;
        let key_a = (jit_site_key(21, info_ptr), cid);
        let key_b = (jit_site_key(22, info_ptr), cid);

        VIRTUAL_DISPATCH_CACHE.with(|dc| {
            dc.borrow_mut().insert(
                key_a,
                DispatchCache {
                    entry: 0x1234_5678,
                    needs_context: true,
                    owner: None,
                    rerun_pins: false,
                },
            );
        });
        let from_a = VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow().get(&key_a).map(|c| c.entry));
        let from_b = VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow().get(&key_b).map(|c| c.entry));
        VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());

        assert_eq!(from_a, Some(0x1234_5678));
        assert_eq!(
            from_b, None,
            "identical (site, class id) in a different VM must not hit"
        );
    }

    #[test]
    fn virtual_target_cache_does_not_serve_another_vms_class_name() {
        let info_ptr = &HASHMAP_PUT_DIRECT_INFO as *const JitInvokeInfo as usize;
        let cid = 77u32;
        let key_a = (jit_site_key(31, info_ptr), cid);
        let key_b = (jit_site_key(32, info_ptr), cid);

        VIRTUAL_TARGET_CACHE.with(|c| {
            c.borrow_mut().insert(
                key_a,
                CachedDispatchTarget {
                    class_name: std::rc::Rc::from("com/example/OnlyInVmA"),
                    cacheable_receiver: true,
                    globally_named: true,
                    selection_error: false,
                    selected_cid: 0,
                },
            );
        });
        let a_name =
            VIRTUAL_TARGET_CACHE.with(|c| c.borrow().get(&key_a).map(|t| t.class_name.to_string()));
        let b_hit = VIRTUAL_TARGET_CACHE.with(|c| c.borrow().get(&key_b).is_some());
        VIRTUAL_TARGET_CACHE.with(|c| c.borrow_mut().clear());

        assert_eq!(a_name.as_deref(), Some("com/example/OnlyInVmA"));
        assert!(
            !b_hit,
            "a callee class name resolved in VM A must not be reused in VM B"
        );
    }

    #[test]
    fn integer_native_dispatch_cache_is_vm_scoped() {
        // The value is irrelevant — `None` means "policy declined this
        // optimization at this site", which is itself a per-VM answer (the
        // execution policy and the native registry are both per-VM).
        let info_ptr = &INTEGER_VALUE_OF_INFO as *const JitInvokeInfo as usize;
        let key_a = jit_site_key(41, info_ptr);
        let key_b = jit_site_key(42, info_ptr);

        INTEGER_NATIVE_DISPATCH_CACHE.with(|c| c.borrow_mut().insert(key_a, None));
        let a_present = INTEGER_NATIVE_DISPATCH_CACHE.with(|c| c.borrow().contains_key(&key_a));
        let b_present = INTEGER_NATIVE_DISPATCH_CACHE.with(|c| c.borrow().contains_key(&key_b));
        INTEGER_NATIVE_DISPATCH_CACHE.with(|c| c.borrow_mut().clear());

        assert!(a_present);
        assert!(
            !b_present,
            "a per-VM native admission decision must not leak to another VM"
        );
    }

    #[test]
    fn dispatch_counter_is_vm_scoped() {
        let info_ptr = &HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as usize;
        let key_a = jit_site_key(51, info_ptr);
        let key_b = jit_site_key(52, info_ptr);
        DISPATCH_COUNTER.with(|dc| {
            let mut m = dc.borrow_mut();
            *m.entry(key_a).or_insert(0) += 5;
        });
        let (a, b) = DISPATCH_COUNTER.with(|dc| {
            (
                dc.borrow().get(&key_a).copied(),
                dc.borrow().get(&key_b).copied(),
            )
        });
        DISPATCH_COUNTER.with(|dc| dc.borrow_mut().clear());
        assert_eq!(a, Some(5));
        assert_eq!(b, None, "hotness counted in VM A must not tier up VM B");
    }

    /// A `JitInvokeInfo` box dies with its `CompiledMethod`, so its address can
    /// be re-issued to the next compile's info — at which point every memo
    /// keyed on `(vm_identity, info_ptr)` names a DIFFERENT call site while
    /// still holding the old site's answer. `NATIVE_SITE_CACHE` then calls the
    /// previous site's native (returning, say, a `boolean` 1 into a slot the
    /// caller treats as an `Annotation[]`) and `VIRTUAL_TARGET_CACHE` resolves
    /// the new site's method name against the old site's class
    /// (`NoSuchMethodError: java.lang.Object.annotationType()`).
    ///
    /// Publishing the new `CompiledMethod` bumps the JIT cache generation
    /// unconditionally, so the generation is the signal that closes it — but
    /// only if EVERY site-keyed memo is on the flush list. Before this fix four
    /// of the eight were on neither trigger's list.
    ///
    /// The flush list is no longer hand-maintained: `site_keyed_memos!`
    /// generates `clear_site_keyed_dispatch_memos` from the same declaration
    /// list, so a memo cannot be declared and left unflushed. What is still
    /// worth testing is that the TRIGGER fires — that a generation change
    /// actually reaches the flush — and that is what this asserts.
    ///
    /// Non-vacuous two ways. **Every** memo is populated and checked non-empty
    /// before the flush, so no assertion below can pass on a map that was
    /// already clear. And both sweeps run over `site_keyed_memo_census()` — the
    /// real list — rather than a copy of it, so a memo added later is covered
    /// the moment it is declared, and the pre-flush sweep names it if whoever
    /// added it forgot a population line.
    #[test]
    fn a_jit_generation_change_clears_every_site_keyed_memo() {
        let _g = memo_test_guard();
        populate_every_site_keyed_memo();

        // Make this thread's remembered generation differ from the live one in
        // its RETIRE half, which is the state a withdrawal or a freed body
        // elsewhere leaves it in. With `JIT_RETIRE_SHIFT == 0` that is any
        // change, as it was before the publish/retire split.
        let stale = cratonvm_jit::jit_cache_generation().wrapping_sub(1u64 << JIT_RETIRE_SHIFT);
        DISPATCH_CACHE_JIT_GENERATION.with(|seen| seen.set(stale));
        flush_raw_entry_dispatch_caches();

        for (name, len) in site_keyed_memo_census() {
            assert_eq!(
                len, 0,
                "{name} survived a JIT generation change — a recycled JitInvokeInfo \
                 address would inherit the previous call site's answer out of it"
            );
        }
    }

    /// A publication that retired nothing drops exactly the
    /// `publication_sensitive` group and keeps every `retire_only` memo
    /// (rt proposal W8-1). Driven through the generated clear rather than
    /// the generation word, so it tests the partition whatever
    /// [`JIT_RETIRE_SHIFT`] is in this build.
    #[test]
    fn a_publication_only_flush_keeps_exactly_the_retire_only_memos() {
        let _g = memo_test_guard();
        populate_every_site_keyed_memo();
        clear_publication_sensitive_site_memos();
        let sensitive = publication_sensitive_memo_names();
        for (name, len) in site_keyed_memo_census() {
            if sensitive.contains(&name) {
                assert_eq!(len, 0, "{name} is publication-sensitive and survived");
            } else {
                assert!(len > 0, "{name} is retire-only and was dropped");
            }
        }
        clear_site_keyed_dispatch_memos();
    }

    /// The partition itself, frozen. A memo moves into `retire_only` only
    /// with an argument that no first-time publication can falsify its
    /// entries (see `site_keyed_memos!`); this list is where a reviewer sees
    /// that move. Refusals, call counts and native resolutions stay here.
    #[test]
    fn the_publication_sensitive_memo_set_is_frozen() {
        assert_eq!(
            publication_sensitive_memo_names(),
            vec![
                "DISPATCH_COUNTER",
                "OBJECT_NATIVE_DISPATCH_CACHE",
                "INTEGER_NATIVE_DISPATCH_CACHE",
                "NATIVE_SITE_CACHE",
                // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-1): a native
                // resolution, keyed by the second receiver class.
                "NATIVE_SITE_CACHE_ALT",
                "VIRTUAL_DISPATCH_COUNTER",
                "MIC_COMPILE_DECLINED",
                "STATIC_BYTECODE_CALLEE_CACHE",
                "VIRTUAL_BYTECODE_CALLEE_CACHE",
                "SPECIAL_BYTECODE_CALLEE_CACHE",
                // Round 13 wave 6 (lane sync3): a refusal a self-locking
                // body's publication falsifies.
                "WRAPPED_STATIC_CALLEE",
            ]
        );
    }

    /// The flush decision, at both shifts, whatever this build uses.
    #[test]
    fn site_memo_flush_splits_publication_from_retire() {
        use SiteMemoFlush::*;
        // Shift 0: every change is a retire (the pre-split behaviour).
        assert_eq!(site_memo_flush(5, 5, 0), Nothing);
        assert_eq!(site_memo_flush(5, 6, 0), All);
        // Shift 32: the low half counts publications, the high half retires.
        let r = 1u64 << 32;
        assert_eq!(site_memo_flush(7 * r + 3, 7 * r + 3, 32), Nothing);
        assert_eq!(
            site_memo_flush(7 * r + 3, 7 * r + 9, 32),
            PublicationSensitive
        );
        assert_eq!(site_memo_flush(7 * r + 3, 8 * r + 3, 32), All);
        assert_eq!(site_memo_flush(7 * r + 3, 8 * r + 4, 32), All);
        // A carry out of the publish half reads as a retire: spurious, safe.
        assert_eq!(site_memo_flush(r - 1, r, 32), All);
    }

    /// Fill every site-keyed memo on this thread, and assert that each one is
    /// non-empty, so a flush test built on it cannot pass on an empty map.
    fn populate_every_site_keyed_memo() {
        let info_ptr = &INTEGER_VALUE_OF_INFO as *const JitInvokeInfo as usize;
        let key = jit_site_key(9001, info_ptr);
        let vkey = (key, 77u32);

        DISPATCH_CACHE.with(|c| {
            c.borrow_mut().insert(
                key,
                DispatchCache {
                    entry: 0xdead_beef,
                    needs_context: false,
                    owner: None,
                    rerun_pins: false,
                },
            );
        });
        DISPATCH_COUNTER.with(|c| {
            c.borrow_mut().insert(key, 3);
        });
        INTEGER_NATIVE_DISPATCH_CACHE.with(|c| {
            c.borrow_mut().insert(key, None);
        });
        NATIVE_SITE_CACHE.with(|c| {
            c.borrow_mut().insert(key, (0, None));
        });
        // The two-way native door's second way (JIT round 19 wave 3,
        // `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`), keyed by (site, receiver).
        NATIVE_SITE_CACHE_ALT.with(|c| {
            c.borrow_mut().insert((key, 77), (0, None));
        });
        VIRTUAL_DISPATCH_CACHE.with(|c| {
            c.borrow_mut().insert(
                vkey,
                DispatchCache {
                    entry: 0xfeed_face,
                    needs_context: true,
                    owner: None,
                    rerun_pins: false,
                },
            );
        });
        VIRTUAL_DISPATCH_COUNTER.with(|c| {
            c.borrow_mut().insert(vkey, 3);
        });
        VIRTUAL_TARGET_CACHE.with(|c| {
            c.borrow_mut().insert(
                vkey,
                CachedDispatchTarget {
                    class_name: std::rc::Rc::from("com/example/PreviousOwnerOfThisAddress"),
                    cacheable_receiver: true,
                    globally_named: true,
                    selection_error: false,
                    selected_cid: 0,
                },
            );
        });
        // A `NativeCallback` is a plain fn pointer, so this needs no registry —
        // the callback is never invoked here, only stored and then flushed.
        OBJECT_NATIVE_DISPATCH_CACHE.with(|c| {
            c.borrow_mut().insert(
                key,
                NativeDispatchCache {
                    receiver_class_id: 77,
                    callback: (|_ctx: &mut dyn cratonvm_native_api::NativeContext,
                                _args: &[Value]| {
                        unreachable!("this test stores the callback, it never calls it")
                    }) as cratonvm_native_api::NativeCallback,
                    kind: ObjectNativeKind::HashMap,
                    native_id: None,
                },
            );
        });

        // The declined-compile-probe memo. Its value is a call count, and the
        // key is `(site, receiver class)` like the two caches above.
        MIC_COMPILE_DECLINED.with(|c| {
            c.borrow_mut().insert((key, 77), 1);
        });

        // The interpreted-callee template memo. `None` is its cached REFUSAL,
        // which is a real entry — storing it is what stops an ineligible site
        // from re-resolving — so it has to be flushed like any other, and a
        // recycled `JitInvokeInfo` address inheriting a refusal would silently
        // deny the new site the fast path forever.
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().insert(key, None);
        });

        // Its virtual/interface twin, keyed on `(site, receiver class)`. Same
        // argument, plus one more: the key names a `ClassId`, and a class id is
        // reissued when a class is unloaded — so a stale entry here could serve
        // one class's method body as another's.
        VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().insert((key, 77), None);
        });

        // The `invokespecial` twin, site-keyed like the static one.
        SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().insert(key, None);
        });

        // The wrapped-entry refusal memo: a recycled address inheriting it
        // would keep a new site off its compiled callee.
        WRAPPED_STATIC_CALLEE.with(|c| {
            c.borrow_mut().insert(key, ());
        });
        // The quiesce verdict (round 12): a recycled address inheriting a
        // `true` would keep a new site off compiled code.
        QUIESCE_VERDICT.with(|c| {
            c.borrow_mut().insert(key, (0, true));
        });
        // The callee-handler template memo (interpreter round i1 wave 5, L7):
        // a recycled address inheriting it would resume ANOTHER method's
        // handler on this site's exception.
        CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| {
            c.borrow_mut().insert(
                (key, 77),
                (
                    std::sync::Arc::new(
                        crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                            cratonvm_jit_api::CachedMethodParts {
                                declaring_class_id: ClassId::new(77),
                                class_name: std::sync::Arc::from("com/example/Callee"),
                                method_name: std::sync::Arc::from("m"),
                                method_descriptor: std::sync::Arc::from("()V"),
                                source_file: None,
                                code: crate::runtime::frame::padded_bytecode(&[0xb1]),
                                exception_table: std::sync::Arc::from(Vec::new()),
                                max_stack: 0,
                                max_locals: 0,
                                num_params: 0,
                                is_synchronized: false,
                                is_static: true,
                            },
                        ),
                    ),
                    crate::classloading::resolution::RedefineGate::never_stale(),
                ),
            );
        });

        // Every memo non-empty first, or the post-flush sweep proves nothing.
        for (name, len) in site_keyed_memo_census() {
            assert!(
                len > 0,
                "{name} was not populated by this test. A site-keyed memo added to \
                 `site_keyed_memos!` needs a population line here too, otherwise the \
                 sweep below passes on a map that was empty to begin with."
            );
        }
    }

    /// The callee-handler template memo (interpreter round i1 wave 5, L7): a
    /// statically bound site's key ignores the receiver; a fresh entry is
    /// served; a redefined callee's entry is evicted, never served.
    ///
    /// The positive half retries: a class defined or code published by a
    /// concurrently running test moves an epoch the memo flushes on, which can
    /// legitimately clear the entry between the insert and the lookup.
    #[test]
    fn a_callee_handler_template_is_served_until_its_class_is_redefined() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let _g = memo_test_guard();
        let vm = SharedVm::new(VmConfig::default());
        let info = &INTEGER_VALUE_OF_INFO;
        assert_eq!(
            info.invoke_kind, 3,
            "the fixture is a statically bound site"
        );
        assert_eq!(
            callee_handler_template_key(&vm, info, ClassId::new(77)),
            callee_handler_template_key(&vm, info, ClassId::new(0)),
            "a static site's key must not depend on the receiver class"
        );
        let key = callee_handler_template_key(&vm, info, ClassId::new(0));
        let cached = std::sync::Arc::new(
            crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: ClassId::new(77),
                    class_name: std::sync::Arc::from("com/example/Callee"),
                    method_name: std::sync::Arc::from("valueOf"),
                    method_descriptor: std::sync::Arc::from("(I)Ljava/lang/Integer;"),
                    source_file: None,
                    code: crate::runtime::frame::padded_bytecode(&[0x01, 0xb0]),
                    exception_table: std::sync::Arc::from(Vec::new()),
                    max_stack: 1,
                    max_locals: 1,
                    num_params: 1,
                    is_synchronized: false,
                    is_static: true,
                },
            ),
        );
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let insert = || {
            CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| {
                c.borrow_mut().insert(
                    key,
                    (
                        std::sync::Arc::clone(&cached),
                        crate::classloading::resolution::RedefineGate::snapshot(
                            std::sync::Arc::clone(&counter),
                        ),
                    ),
                );
            });
        };
        let mut served = false;
        for _ in 0..100 {
            flush_raw_entry_dispatch_caches();
            flush_class_identity_dispatch_memos(&vm);
            insert();
            if let Some(hit) = callee_handler_template_hit(&vm, info, ClassId::new(0)) {
                assert!(std::sync::Arc::ptr_eq(&hit, &cached));
                served = true;
                break;
            }
        }
        assert!(served, "a fresh entry must be served");

        insert();
        counter.fetch_add(1, std::sync::atomic::Ordering::Release);
        assert!(
            callee_handler_template_hit(&vm, info, ClassId::new(0)).is_none(),
            "a redefined callee's template must not be served"
        );
        assert!(
            CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| !c.borrow().contains_key(&key)),
            "a stale entry is evicted"
        );
    }

    /// Each VM owns its class-init memo (interpreter round i1 wave 22, lane
    /// L5): one VM's initialized class never answers for another VM's class
    /// at the same id, and a VM created after another has its own fast path
    /// (it used to be refused the table the first VM claimed, for the life of
    /// the process).
    #[test]
    fn class_init_memo_is_per_vm_and_every_vm_has_one() {
        use crate::config::VmConfig;
        let class_id = 1_000_500u32;
        let a = SharedVm::new(VmConfig::default());
        assert!(!class_init_memo::is_initialized(&a, class_id));
        class_init_memo::mark_initialized(&a, class_id, class_init_memo::generation(&a));
        assert!(
            class_init_memo::is_initialized(&a, class_id),
            "the VM keeps its fast path"
        );
        let b = SharedVm::new(VmConfig::default());
        assert!(
            !class_init_memo::is_initialized(&b, class_id),
            "VM B must run the authoritative JVMS 5.5 init check for its own class {class_id}"
        );
        class_init_memo::mark_initialized(&b, class_id + 1, class_init_memo::generation(&b));
        assert!(
            class_init_memo::is_initialized(&b, class_id + 1),
            "a later VM has a table of its own"
        );
        assert!(
            !class_init_memo::is_initialized(&a, class_id + 1),
            "and cannot publish into another VM's"
        );
        assert!(
            !class_init_memo::is_initialized(&b, u32::MAX),
            "an id outside the table is not known initialized"
        );
    }

    #[test]
    fn native_dispatch_scratch_stays_inline_through_register_envelope() {
        let mut args = JitDecodedArgs::with_capacity(INLINE_JIT_NATIVE_ARGS);
        args.resize(INLINE_JIT_NATIVE_ARGS, Value::Int(0));
        assert!(!args.spilled(), "eight native arguments must stay inline");
        args.push(Value::Int(0));
        assert!(args.spilled(), "larger descriptors may use heap fallback");
    }

    #[test]
    fn virtual_dispatch_target_uses_cp_class_for_cid0_non_object_method() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let vm = SharedVm::new(VmConfig::default());
        let receiver = vm.mem.heap.alloc_object(ClassId::new(0), 0);
        let info = JitInvokeInfo {
            class_name: "java/io/InputStream",
            method_name: "read",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        };

        let target = unsafe { virtual_dispatch_target_for_receiver(&vm, receiver, &info) };
        assert_eq!(target.class_name.as_ref(), "java/io/InputStream");
        assert!(
            !target.cacheable_receiver,
            "ClassId(0) is the MIC/PIC empty-slot sentinel and must not be cached"
        );
    }

    #[test]
    fn virtual_dispatch_target_keeps_object_members_on_object_for_cid0() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let vm = SharedVm::new(VmConfig::default());
        let receiver = vm.mem.heap.alloc_object(ClassId::new(0), 0);
        let info = JitInvokeInfo {
            class_name: "java/lang/Object",
            method_name: "hashCode",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        };

        let target = unsafe { virtual_dispatch_target_for_receiver(&vm, receiver, &info) };
        assert_eq!(target.class_name.as_ref(), "java/lang/Object");
        assert!(!target.cacheable_receiver);
    }

    #[test]
    fn compiled_entry_reentrant_wrapper_preserves_no_ctx_abi() {
        // SAFETY: a plain constant-returning function. It is `unsafe extern
        // "C"` only so its type matches the compiled-entry ABI the helper
        // transmutes to; the body touches nothing.
        unsafe extern "C" fn ret_seven() -> i64 {
            7
        }

        // SAFETY: the test function has the no-context, zero-arg ABI selected below.
        let result = unsafe {
            try_call_compiled_entry_reentrant(ret_seven as *const () as usize, false, 0, &[])
        };
        assert_eq!(result, Some(7));
    }

    #[test]
    fn compiled_entry_reentrant_wrapper_preserves_ctx_abi() {
        // SAFETY: pure integer arithmetic over its two arguments. `unsafe
        // extern "C"` only to match the with-context compiled-entry ABI.
        unsafe extern "C" fn add_ctx_arg(ctx: i64, arg: i64) -> i64 {
            ctx + arg
        }

        // SAFETY: the test function has the with-context, one-arg ABI selected below.
        let result = unsafe {
            try_call_compiled_entry_reentrant(add_ctx_arg as *const () as usize, true, 5, &[7])
        };
        assert_eq!(result, Some(12));
    }

    #[test]
    fn compiled_entry_reentrant_wrapper_preserves_stack_arg_abi() {
        // SAFETY: pure integer arithmetic over its arguments. `unsafe extern
        // "C"` only to match the no-context five-argument entry ABI, which is
        // what puts the fifth argument on the stack under the Windows x64
        // convention — the property this test exists to pin.
        unsafe extern "C" fn sum_five(a: i64, b: i64, c: i64, d: i64, e: i64) -> i64 {
            a + b + c + d + e
        }
        // SAFETY: as above, for the with-context four-argument shape.
        unsafe extern "C" fn sum_ctx_four(ctx: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
            ctx + a + b + c + d
        }
        // SAFETY: both functions use the C ABI selected by the helper, including
        // the first stack-passed parameter on Windows.
        let no_ctx = unsafe {
            try_call_compiled_entry_reentrant(
                sum_five as *const () as usize,
                false,
                0,
                &[1, 2, 3, 4, 5],
            )
        };
        // SAFETY: same entry ABI contract — `sum_ctx_four` is the with-context,
        // four-argument shape selected by the `true` flag below.
        let with_ctx = unsafe {
            try_call_compiled_entry_reentrant(
                sum_ctx_four as *const () as usize,
                true,
                10,
                &[1, 2, 3, 4],
            )
        };
        assert_eq!(no_ctx, Some(15));
        assert_eq!(with_ctx, Some(20));
    }

    /// Round 12 wave 2 (lane `calls`): a helper call wider than the eight-word
    /// arity tables reaches the compiled entry, every word in its place, up to
    /// sixteen ABI words; past that it still answers `None` (the caller's
    /// interpreter fallback). It used to answer `None` from nine words on.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn r12w2_a_wide_compiled_entry_call_passes_every_word_in_order() {
        // SAFETY (all three): pure integer arithmetic over the arguments,
        // `unsafe extern "C"` only to match the compiled-entry ABI.
        #[allow(clippy::too_many_arguments)]
        unsafe extern "C" fn weigh_nine(
            a: i64,
            b: i64,
            c: i64,
            d: i64,
            e: i64,
            f: i64,
            g: i64,
            h: i64,
            i: i64,
        ) -> i64 {
            a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h + 9 * i
        }
        #[allow(clippy::too_many_arguments)]
        unsafe extern "C" fn weigh_ctx_eight(
            ctx: i64,
            a: i64,
            b: i64,
            c: i64,
            d: i64,
            e: i64,
            f: i64,
            g: i64,
            h: i64,
        ) -> i64 {
            1000 * ctx + a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h
        }
        #[allow(clippy::too_many_arguments)]
        unsafe extern "C" fn weigh_ctx_fifteen(
            ctx: i64,
            a: i64,
            b: i64,
            c: i64,
            d: i64,
            e: i64,
            f: i64,
            g: i64,
            h: i64,
            i: i64,
            j: i64,
            k: i64,
            l: i64,
            m: i64,
            n: i64,
            o: i64,
        ) -> i64 {
            1000 * ctx
                + a
                + 2 * b
                + 3 * c
                + 4 * d
                + 5 * e
                + 6 * f
                + 7 * g
                + 8 * h
                + 9 * i
                + 10 * j
                + 11 * k
                + 12 * l
                + 13 * m
                + 14 * n
                + 15 * o
        }
        let args: Vec<i64> = (1..=15).map(|v| v * 3).collect();
        let weighed = |n: usize| -> i64 {
            args[..n]
                .iter()
                .enumerate()
                .map(|(i, v)| (i as i64 + 1) * v)
                .sum()
        };
        // SAFETY: each function has the ABI and arity the flag and slice select.
        let nine = unsafe {
            try_call_compiled_entry_reentrant(
                weigh_nine as *const () as usize,
                false,
                0,
                &args[..9],
            )
        };
        assert_eq!(nine, Some(weighed(9)), "nine context-free words");
        // SAFETY: as above.
        let ctx_eight = unsafe {
            try_call_compiled_entry_reentrant(
                weigh_ctx_eight as *const () as usize,
                true,
                7,
                &args[..8],
            )
        };
        assert_eq!(ctx_eight, Some(7000 + weighed(8)), "the context plus eight");
        // SAFETY: as above.
        let ctx_fifteen = unsafe {
            try_call_compiled_entry_reentrant(
                weigh_ctx_fifteen as *const () as usize,
                true,
                5,
                &args[..15],
            )
        };
        assert_eq!(ctx_fifteen, Some(5000 + weighed(15)), "sixteen words");
        // Seventeen words: nothing is called.
        // SAFETY: the entry is never called for a slice this wide.
        let too_wide = unsafe {
            try_call_compiled_entry_reentrant(
                weigh_ctx_fifteen as *const () as usize,
                false,
                0,
                &[1; 17],
            )
        };
        assert_eq!(too_wide, None);
    }

    #[test]
    fn jit_checkcast_null_ptr_returns_zero() {
        // SAFETY: Passing all-zero/null arguments exercises the null-object fast path;
        // no heap pointers are dereferenced.
        let result = unsafe { jit_checkcast(0, 0, std::ptr::null(), 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_checkcast_non_heap_ptr_returns_zero() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let vm = SharedVm::new(VmConfig::default());
        let target = "java/lang/Object";
        // SAFETY: vm points to a live SharedVm and target is a valid UTF-8
        // string. The fake receiver is aligned and pointer-shaped, but outside
        // the VM heap; jit_checkcast must reject it before any header read.
        let result = unsafe {
            jit_checkcast(
                &vm as *const SharedVm as i64,
                0x1000,
                target.as_ptr(),
                target.len() as i64,
            )
        };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_null_ptr_returns_zero() {
        // SAFETY: Passing all-zero/null arguments exercises the null-object fast path;
        // no heap pointers are dereferenced.
        let result = unsafe { jit_instanceof(0, 0, std::ptr::null(), 0) };
        assert_eq!(result, 0);
    }

    /// NEW-1.2 regression: a typecheck site with a missing class-name pointer
    /// is a JIT-side bug (typecheck_info should always be populated by the
    /// scanner). The safe behavior is to *fail closed* — return 0 from
    /// checkcast, which the JIT-compiled code interprets as a failed cast and
    /// surfaces as a deterministic ClassCastException through the interpreter
    /// fallback. The old behavior silently let the cast through, masking the
    /// underlying scan/compile mismatch.
    #[test]
    fn jit_checkcast_negative_len_fails_closed() {
        // SAFETY: obj_ptr is 0 (null) and class_name_len is negative, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_checkcast(0, 42, "test".as_ptr(), -1) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_negative_len_returns_zero() {
        // SAFETY: obj_ptr is 0 (null) and class_name_len is negative, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_instanceof(0, 42, "test".as_ptr(), -1) };
        assert_eq!(result, 0);
    }

    /// NEW-1.2 regression: same fail-closed semantics for a null class-name
    /// pointer with a positive length.
    #[test]
    fn jit_checkcast_null_class_name_ptr_fails_closed() {
        // SAFETY: obj_ptr is 0 (null) and class_name_ptr is null, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_checkcast(0, 99, std::ptr::null(), 5) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_null_class_name_ptr_returns_zero() {
        // SAFETY: obj_ptr is 0 (null) and class_name_ptr is null, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_instanceof(0, 99, std::ptr::null(), 5) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_baload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §baload: NPE on null array. Like iaload/aaload, the helper returns
        // the i64::MIN deopt sentinel and sets the pending-NPE flag (the old
        // "return 0" silently fabricated a zero byte and masked real null derefs).
        let _ = take_jit_pending_npe(); // clear any prior state
        let result = unsafe { jit_baload(0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(
            take_jit_pending_npe(),
            "baload(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_iaload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §iaload: NPE on null array. Helper returns the i64::MIN deopt sentinel
        // and sets the pending-NPE flag for the interpreter to consume.
        let _ = take_jit_pending_npe(); // clear any prior state
        let result = unsafe { jit_iaload(0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(
            take_jit_pending_npe(),
            "iaload(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_aaload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without
        // dereferencing. JVMS §aaload: NPE on null array.
        //
        // `vm_ptr` is deliberately 0, and that is sound ONLY because of the
        // ordering: `plausible_heap_pointer(0)` is false, so the helper takes
        // the NPE arm and returns before `heap_from_vm(vm_ptr)` is reached.
        // State it rather than leaving a bare 0 -- if that ordering ever
        // changes, this argument becomes a null deref instead of a wrong
        // answer. Pass a real `SharedVm` (as `jit_aaload_oob_sets_pending_aioobe`
        // does) if this test ever grows past the guard arm.
        let _ = take_jit_pending_npe();
        let result = unsafe { jit_aaload(0, 0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(
            take_jit_pending_npe(),
            "aaload(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_arraylength_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §arraylength: NPE on null array. Previously returned -1, which
        // silently corrupted any downstream length-comparison or loop-bound use.
        let _ = take_jit_pending_npe();
        let result = unsafe { jit_arraylength(0) };
        assert_eq!(result, i64::MIN);
        assert!(
            take_jit_pending_npe(),
            "arraylength(null) must set pending NPE flag"
        );
    }

    /// Round-8 CRIT fix: store helpers (`jit_iastore` / `jit_bastore` /
    /// `jit_aastore`) on a null array previously called
    /// `std::process::abort()` with a comment claiming the helper was
    /// unreachable; in reality the helpers were registered in
    /// `JitRuntimeHelpers` and reachable. They now set the pending-NPE
    /// flag (drained on every JIT return by the interpreter) so the NPE
    /// surfaces at the right method.
    #[test]
    fn jit_iastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences.
        unsafe { jit_iastore(0, 0, 0) };
        assert!(
            take_jit_pending_npe(),
            "iastore(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_bastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        let _ = take_jit_deopt_pending();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences.
        unsafe { jit_bastore(0, 0, 0) };
        assert!(
            take_jit_pending_npe(),
            "bastore(null) must set pending NPE flag"
        );
    }

    // -----------------------------------------------------------------------
    // Out-of-band deopt signal (finding 1): i64::MIN sentinel disambiguation
    // -----------------------------------------------------------------------

    #[test]
    fn deopt_pending_set_take_cycle() {
        let _ = take_jit_deopt_pending(); // clear any prior state
        assert!(!take_jit_deopt_pending(), "flag must start clear");
        set_jit_deopt_pending();
        assert!(take_jit_deopt_pending(), "set then take must observe true");
        assert!(!take_jit_deopt_pending(), "take must clear the flag");
    }

    #[test]
    fn jit_dispatch_threw_peeks_all_signals_nondestructively() {
        // The J/D-call-site disambiguation peek: `1` ⇒ a genuine exception/deopt
        // is pending (caller bails), `0` ⇒ the `i64::MIN` in RAX is a legitimate
        // `Long.MIN_VALUE` return (caller keeps it). It must read EVERY signal a
        // dispatch could leave, and must NOT clear any (the interpreter drain
        // still needs them).
        let _ = take_jit_deopt_pending();
        let _ = take_jit_pending_npe();
        let _ = take_jit_pending_aioobe();
        let _ = take_jit_pending_arithmetic();

        // Nothing pending → legitimate value.
        assert_eq!(
            jit_dispatch_threw(),
            0,
            "no pending signal must report 0 (a legitimate Long.MIN_VALUE return)"
        );

        // Out-of-band deopt flag → genuine sentinel, peeked non-destructively.
        set_jit_deopt_pending();
        assert_eq!(
            jit_dispatch_threw(),
            1,
            "a pending deopt must report 1 (bail)"
        );
        assert_eq!(jit_dispatch_threw(), 1, "the peek must be non-clearing");
        assert!(
            take_jit_deopt_pending(),
            "the deopt flag must survive the peek for the interpreter drain"
        );
        assert_eq!(jit_dispatch_threw(), 0, "drained → back to 0");

        // Pending NPE (e.g. a null-receiver dispatch) → 1, non-clearing.
        set_jit_pending_npe();
        assert_eq!(jit_dispatch_threw(), 1, "a pending NPE must report 1");
        assert!(take_jit_pending_npe(), "NPE flag must survive the peek");
        let _ = take_jit_pending_npe_action();
        assert_eq!(jit_dispatch_threw(), 0);

        // Pending AIOOBE (a bounds-check failure) → 1, non-clearing.
        stash_jit_pending_aioobe(5, 3);
        assert_eq!(jit_dispatch_threw(), 1, "a pending AIOOBE must report 1");
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((5, 3)),
            "AIOOBE payload must survive the peek"
        );
        assert_eq!(jit_dispatch_threw(), 0, "fully drained → 0");

        // Pending `/ by zero` alone (restored flag-only by a door) → 1.
        stash_jit_pending_arithmetic();
        assert_eq!(
            jit_dispatch_threw(),
            1,
            "a pending ArithmeticException must report 1"
        );
        assert!(
            take_jit_pending_arithmetic(),
            "the arithmetic flag must survive the peek"
        );
        assert_eq!(jit_dispatch_threw(), 0, "fully drained → 0");
    }

    /// A frameless callee trap is the deopt flag ALONE: any implicit signal
    /// beside it means the stub threw, and the helpers route that instead of
    /// re-running the callee (interpreter round i1 wave 6, L7).
    #[test]
    fn a_frameless_callee_trap_is_the_deopt_flag_alone() {
        let _ = take_jit_deopt_pending();
        let _ = take_jit_pending_npe();
        let _ = take_jit_pending_npe_action();
        let _ = take_jit_pending_aioobe();
        let _ = take_jit_pending_arithmetic();
        let _ = cratonvm_jit::deopt::take_last_deopt();
        assert!(!frameless_callee_trap_pending(), "nothing pending");
        set_jit_deopt_pending();
        assert!(frameless_callee_trap_pending(), "the flag alone");
        set_jit_pending_npe();
        assert!(
            !frameless_callee_trap_pending(),
            "a thrown NPE is not a trap"
        );
        let _ = take_jit_pending_npe();
        let _ = take_jit_pending_npe_action();
        stash_jit_pending_arithmetic();
        assert!(!frameless_callee_trap_pending(), "nor a `/ by zero`");
        let _ = take_jit_pending_arithmetic();
        stash_jit_pending_aioobe(1, 0);
        assert!(!frameless_callee_trap_pending(), "nor an AIOOBE");
        let _ = take_jit_pending_aioobe();
        assert!(
            frameless_callee_trap_pending(),
            "the flag is untouched by the peek"
        );
        assert!(take_jit_deopt_pending());
        assert!(!frameless_callee_trap_pending());
    }

    /// Round 13 wave 8 (lane replay4): the deopt flag's producer is recorded
    /// with it. A helper raising the flag is `Helper`; a frameless stub with no
    /// compile id to pass (or one no artifact is bound to) is `Unknown`, the
    /// old rule; a dropped callee frame keeps its own verdict; and taking the
    /// flag leaves the record for the sink that reads it.
    #[test]
    fn the_deopt_flag_records_its_producer() {
        set_jit_deopt_pending();
        assert_eq!(last_frameless_trap_site(), FramelessTrapSite::Helper);
        note_frameless_trap_stub(2, 17);
        assert_eq!(
            last_frameless_trap_site(),
            FramelessTrapSite::Unknown,
            "no compile id in the reason word's high half"
        );
        // An id no allocator hands out resolves to no artifact.
        note_frameless_trap_stub(((0xFFFF_FF09u64) << 32 | 2) as i64, 17);
        assert_eq!(last_frameless_trap_site(), FramelessTrapSite::Unknown);
        set_jit_deopt_pending();
        note_dropped_callee_frame(true);
        assert!(take_jit_deopt_pending());
        assert_eq!(
            last_frameless_trap_site(),
            FramelessTrapSite::Dropped { exact: true },
            "taking the flag leaves the record"
        );
        set_jit_deopt_pending();
        assert_eq!(last_frameless_trap_site(), FramelessTrapSite::Helper);
        let _ = take_jit_deopt_pending();
    }

    /// `jit_invoke_virtual_mic` stands its compiled-entry arms down in
    /// interpreter-only mode as `jit_invoke_dispatch_body` does, and routes
    /// every Rust-armed sentinel through the frameless-trap service. A text
    /// pin: the helper cannot be driven without compiled code.
    #[test]
    fn the_mic_helper_gates_compiled_entries_and_services_frameless_traps() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = src
            .find("\nunsafe fn jit_invoke_virtual_mic_body(")
            .expect("the MIC body exists");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
        assert!(body.contains(
            "let compiled_entry_forbidden =\n        redefine_jit_quiesced || \
             crate::runtime::interpreter::jvmti_requires_interpreter(vm);"
        ));
        assert!(
            !body.contains("&& !redefine_jit_quiesced")
                && !body.contains("|| redefine_jit_quiesced\n"),
            "every compiled-entry gate asks compiled_entry_forbidden"
        );
        assert!(
            !body.contains("handle_compiled_callee_deopt_sentinel("),
            "the Rust arms route through handle_rust_armed_callee_sentinel"
        );
        let lambda = &src[src
            .find("\nunsafe fn try_lambda_site_direct_call(")
            .expect("the lambda arm exists")..];
        let lambda = &lambda[..lambda.find("\n\x7d\n").map_or(lambda.len(), |e| e + 3)];
        assert!(lambda.contains("jvmti_requires_interpreter(vm)"));
    }

    #[test]
    fn jit_throw_aioobe_sets_deopt_pending() {
        let _ = take_jit_deopt_pending();
        let _ = take_jit_pending_aioobe();
        clear_jit_athrow_bci();
        // SAFETY: only stores into thread-locals; no pointer dereference.
        let r = unsafe { jit_throw_aioobe(5, 3, 0, 17) };
        assert_eq!(r, i64::MIN);
        assert!(
            take_jit_deopt_pending(),
            "jit_throw_aioobe must set the out-of-band deopt signal so the \
             interpreter does not mistake a legitimate Long.MIN_VALUE return"
        );
        // also leaves the AIOOBE payload for the interpreter drain
        assert_eq!(take_jit_pending_aioobe(), Some((5, 3)));
        assert_eq!(
            peek_jit_athrow_bci(),
            17,
            "AIOOBE helper must replace any stale throw location with its trapping bci"
        );
        clear_jit_athrow_bci();
    }

    #[test]
    fn jit_bastore_null_sets_deopt_pending() {
        let _ = take_jit_deopt_pending();
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); null-guard early return, no deref.
        unsafe { jit_bastore(0, 0, 0) };
        assert!(
            take_jit_deopt_pending(),
            "bastore(null) (the null-check-store stub path) must set the deopt signal"
        );
        let _ = take_jit_pending_npe();
    }

    // r10 help2 / integrator: the two tests that stood here --
    // `jit_monitor_enter_no_thread_sets_deopt_pending` and its exit twin -- are
    // REMOVED, and deliberately not replaced.
    //
    // The fix they were written for is real and is still in place: both
    // `jit_monitor_enter_body` and `jit_monitor_exit_body` now call
    // `set_jit_deopt_pending()` before returning `i64::MIN` from their "no live
    // JIT thread" arm, because these helpers answer with a REFERENCE and an
    // unflagged sentinel becomes a wild pointer in an oop slot.
    //
    // The tests could not work. They called the body with `vm_ptr = 0` and a SAFETY
    // comment asserting the function "returns from the `jit_thread_mut` arm before
    // either raw pointer is ever dereferenced". It does not: `let vm = &*(vm_ptr as
    // *const SharedVm)` runs BEFORE that arm. The null dereference is undefined
    // behaviour, not a catchable panic, so it aborted the whole `cratonvm-vm` test
    // binary with STATUS_STACK_BUFFER_OVERRUN -- taking every other test in this
    // crate with it -- rather than failing one case.
    //
    // Reaching that arm honestly needs a real `SharedVm` with no `JIT_THREAD`
    // installed, which is close to a contradiction: a compiled frame always
    // installs one, which is why the arm is documented as unreachable in the first
    // place. It is belt-and-braces for a case that should not happen, and the
    // honest position is that it is not unit-testable at this level -- not that it
    // is covered.

    #[test]
    fn jit_aastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences either array_ptr or
        // vm_ptr / val (the null path returns before touching them).
        unsafe { jit_aastore(0, 0, 0, 0) };
        assert!(
            take_jit_pending_npe(),
            "aastore(null) must set pending NPE flag"
        );
    }

    // ----------------------------------------------------------------------
    // B1 fix (review `vm-runtime.md`): the array load/store helpers used to
    // silently swallow an out-of-bounds index (load returned a fabricated 0 /
    // null, store dropped the write) instead of raising AIOOBE, diverging from
    // JVMS and masking real bugs. They now mirror `jit_throw_aioobe`: set the
    // pending-AIOOBE payload + return the `i64::MIN` deopt sentinel (loads) /
    // set the flag and return (void stores). The fast in-bounds path is
    // unchanged — verified by the round-trip assertions below.
    // ----------------------------------------------------------------------

    /// Allocate a small real heap and a single array on it. Returns the owning
    /// `SharedVm` box (kept alive by the caller) and the raw array pointer the
    /// JIT helpers consume.
    fn alloc_test_array(et: ArrayElementType, len: usize) -> (Box<crate::vm::SharedVm>, i64) {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let mut config = VmConfig::default();
        config.max_heap_size = 4 * 1024 * 1024; // 4 MB — plenty for one tiny array
        config.initial_heap_size = 4 * 1024 * 1024;
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(config));
        let arr = vm_box.mem.heap.alloc_array(ClassId::new(0), et, len);
        let arr_ptr = arr.as_ptr() as i64;
        (vm_box, arr_ptr)
    }

    /// r9w12: the `Object[]` early-out hoisted into `aastore_store_is_refused`
    /// can only ever ADMIT a store the covariance walk would also admit.
    ///
    /// The hoist moved `reference_array_component_is_object` ahead of
    /// `aastore_element_assignable`, which opens with the same test behind a
    /// `kind_of` + a second `element_type_of` that this caller has already
    /// established. It short-circuits only on `true`, so the property that
    /// makes it behaviour-preserving is an IMPLICATION, not an equality:
    ///
    /// > `early == true`  =>  the walk would have permitted the store too.
    ///
    /// A `false` costs nothing — the walk runs exactly as it did before — which
    /// is why this fixture is still worth testing even though its component is
    /// NOT `java.lang.Object`: `alloc_test_array` builds its array with
    /// `ClassId::new(0)`, and whatever a bare `SharedVm` has registered under
    /// that id is what the memo resolves. Asserting `early` directly is what an
    /// earlier draft of this test got wrong.
    ///
    /// It also pins the direction end to end: this helper must answer "store
    /// it" here. A FALSE `ArrayStoreException` is a new failure, not a
    /// restored one.
    #[test]
    fn r9w12_object_array_early_out_only_admits_what_the_walk_admits() {
        let (vm, arr_ptr) = alloc_test_array(ArrayElementType::Reference, 2);
        // SAFETY: `arr_ptr` is the live reference array just allocated above.
        let arr = unsafe { ObjectRef::from_raw(arr_ptr as usize as *mut u8) };
        assert_eq!(
            vm.mem.heap.element_type_of(arr),
            ArrayElementType::Reference,
            "the early-out's precondition, supplied by the line above it"
        );
        let early = crate::runtime::interpreter::reference_array_component_is_object(&vm, arr);
        let walk = crate::runtime::interpreter::aastore_element_assignable(&vm, arr, arr);
        assert!(
            !early || walk,
            "the early-out admitted a store the covariance walk refuses"
        );
        // And the helper the hoist lives in still says "store it" — on this
        // shape it is the walk that says so, through the unchanged fall-through.
        let vm_ptr = &*vm as *const crate::vm::SharedVm as i64;
        // SAFETY: `vm_ptr` is the live `SharedVm` above and `arr_ptr` a live
        // reference array on its heap.
        assert_eq!(
            unsafe { jit_aastore_type_check(vm_ptr, arr_ptr, arr_ptr) },
            0
        );
    }

    /// The check-only helper allows every store it cannot adjudicate.
    ///
    /// `aastore_element_assignable` fails OPEN, and so must everything in
    /// front of it: the JVMS rule is additive correctness, and a FALSE
    /// `ArrayStoreException` on a stale or unknowable pointer would be a new
    /// failure rather than a restored one.
    #[test]
    fn jit_aastore_type_check_allows_null_and_implausible_operands() {
        // SAFETY: every argument below takes a guard arm that returns before
        // any pointer is dereferenced — that is exactly what is under test.
        unsafe {
            assert_eq!(jit_aastore_type_check(0, 0, 0), 0, "null value is legal");
            assert_eq!(
                jit_aastore_type_check(0, 0x1000, 0),
                0,
                "null value stays legal whatever the array is"
            );
            // Unaligned / non-heap pointers: unknowable type, so allow.
            assert_eq!(
                jit_aastore_type_check(0, 0x1001, 0x1003),
                0,
                "an implausible value pointer must not be dereferenced"
            );
            assert_eq!(
                jit_aastore_type_check(0, 0, 0x1003),
                0,
                "an implausible array pointer must not be dereferenced"
            );
        }

        // A real reference array on a real heap, with no class graph behind it.
        // Passing the array as its own element keeps `val` a live heap pointer
        // without a second allocation. This asserts the WHOLE helper runs to a
        // "proceed" answer on a degenerate-but-live input:
        // `aastore_element_assignable` fails open when it cannot resolve the
        // component (it degrades to `[Ljava/lang/Object;`, which admits every
        // reference), and even on the refusing branch there is no JIT thread in
        // a unit-test thread, so the documented no-thread degradation returns
        // `0` too. Either way the answer must be "store it" — this helper must
        // never manufacture an ArrayStoreException out of missing information.
        let (vm, arr_ptr) = alloc_test_array(ArrayElementType::Reference, 2);
        let vm_ptr = &*vm as *const crate::vm::SharedVm as i64;
        // SAFETY: `vm_ptr` points at the live `SharedVm` owned by `vm` for the
        // whole call, and `arr_ptr` is a live reference array on its heap.
        assert_eq!(
            unsafe { jit_aastore_type_check(vm_ptr, arr_ptr, arr_ptr) },
            0,
            "an unresolvable component type must fail open (never a false ASE)"
        );
    }

    /// The x64 emitter must actually CALL the check.
    ///
    /// This is the shape of the original defect, and it is worth a test that
    /// cannot be satisfied by a comment: `jit_aastore` grew the covariance
    /// check while the emitter had already stopped calling `jit_aastore` at
    /// all, so a JIT-compiled `aastore` stored anything into anything. The
    /// helper slot is declared `required: true` in `jit-api`, which makes a
    /// null slot a `validate()` failure — this pins the other half, that the
    /// slot is populated with the real function.
    #[test]
    fn aastore_type_check_helper_slot_is_populated() {
        let helpers = build_helpers();
        assert_eq!(
            helpers.aastore_type_check, jit_aastore_type_check as *const () as usize,
            "the aastore element-type check must be wired to its helper"
        );
        assert!(
            helpers.validate().is_ok(),
            "the helper table must validate with the appended required slot"
        );
    }

    #[test]
    fn jit_iaload_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe(); // clear any prior state
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Int, 4);
        // SAFETY: arr_ptr is a live int[4] on the test heap; index 4 is out of
        // bounds so the helper takes the bounds-check arm and never dereferences
        // an element. index -1 is likewise rejected before any element read.
        let hi = unsafe { jit_iaload(arr_ptr, 4) };
        assert_eq!(
            hi,
            i64::MIN,
            "iaload OOB-high must return the deopt sentinel"
        );
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((4, 4)),
            "iaload OOB-high must set pending AIOOBE (index, length)"
        );
        // SAFETY: `arr_ptr` is a valid test array header built above; `jit_iaload`
        // bounds-checks the index and signals AIOOBE rather than reading OOB.
        let lo = unsafe { jit_iaload(arr_ptr, -1) };
        assert_eq!(
            lo,
            i64::MIN,
            "iaload OOB-low must return the deopt sentinel"
        );
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((-1, 4)),
            "iaload OOB-low (negative index) must set pending AIOOBE"
        );
    }

    #[test]
    fn jit_iastore_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Int, 4);
        // SAFETY: arr_ptr is a live int[4]; index 7 is OOB so the store is not
        // performed and no element pointer is dereferenced.
        unsafe { jit_iastore(arr_ptr, 7, 0x1234) };
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((7, 4)),
            "iastore OOB must set pending AIOOBE and not write past the array"
        );
    }

    #[test]
    fn jit_baload_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Byte, 2);
        // SAFETY: arr_ptr is a live byte[2]; index 2 is OOB.
        let r = unsafe { jit_baload(arr_ptr, 2) };
        assert_eq!(r, i64::MIN, "baload OOB must return the deopt sentinel");
        assert_eq!(take_jit_pending_aioobe(), Some((2, 2)));
    }

    #[test]
    fn jit_bastore_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Byte, 2);
        // SAFETY: arr_ptr is a live byte[2]; index 5 is OOB so no write occurs.
        unsafe { jit_bastore(arr_ptr, 5, 0xFF) };
        assert_eq!(take_jit_pending_aioobe(), Some((5, 2)));
    }

    #[test]
    fn jit_aaload_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (vm, arr_ptr) = alloc_test_array(ArrayElementType::Reference, 3);
        let vm_ptr = &*vm as *const crate::vm::SharedVm as i64;
        // SAFETY: arr_ptr is a live Object[3]; index 3 is OOB, so the helper
        // returns on the bounds check BEFORE the element read and never reaches
        // `heap_from_vm`. Unlike the null-array test above, this one has a VM in
        // hand, so it passes a live `vm_ptr` and does not depend on that
        // ordering to be sound.
        let r = unsafe { jit_aaload(vm_ptr, arr_ptr, 3) };
        assert_eq!(r, i64::MIN, "aaload OOB must return the deopt sentinel");
        assert_eq!(take_jit_pending_aioobe(), Some((3, 3)));
    }

    #[test]
    fn jit_aastore_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (vm, arr_ptr) = alloc_test_array(ArrayElementType::Reference, 3);
        let vm_ptr = &*vm as *const crate::vm::SharedVm as i64;
        // SAFETY: arr_ptr is a live Object[3]; index 9 is OOB so the helper
        // returns BEFORE the SATB barrier / element write and never touches
        // vm_ptr or val. vm_ptr is a live SharedVm regardless.
        unsafe { jit_aastore(vm_ptr, arr_ptr, 9, 0) };
        assert_eq!(take_jit_pending_aioobe(), Some((9, 3)));
    }

    #[test]
    fn jit_int_array_in_bounds_roundtrip_unchanged() {
        // The fast in-bounds path must be untouched by the B1 fix: a value
        // stored at a valid index reads back identically, and no AIOOBE flag
        // is left pending.
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Int, 4);
        // SAFETY: arr_ptr is a live int[4]; indices 0..4 are all in bounds.
        unsafe {
            jit_iastore(arr_ptr, 0, 11);
            jit_iastore(arr_ptr, 3, -7);
        }
        assert!(
            take_jit_pending_aioobe().is_none(),
            "in-bounds stores must NOT set the AIOOBE flag"
        );
        // SAFETY: in-bounds loads.
        let a = unsafe { jit_iaload(arr_ptr, 0) };
        let b = unsafe { jit_iaload(arr_ptr, 3) };
        assert_eq!(a, 11, "in-bounds iaload(0) must read back the stored value");
        assert_eq!(b, -7, "in-bounds iaload(3) must read back the stored value");
        assert!(
            take_jit_pending_aioobe().is_none(),
            "in-bounds loads must NOT set the AIOOBE flag"
        );
    }

    #[test]
    fn jit_getfield_null_sets_pending_npe() {
        // B2 fix (review `vm-runtime.md`): JVMS §getfield throws NPE on a null
        // receiver. The helper used to return 0, silently fabricating a
        // zero/null field value (same masked-null-deref class as the array-load
        // helpers). It now sets the pending-NPE flag and returns the i64::MIN
        // deopt sentinel, mirroring `jit_arraylength`.
        let _ = take_jit_pending_npe(); // clear any prior state
                                        // SAFETY: obj_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_getfield(0, 0, 0) };
        assert_eq!(
            result,
            i64::MIN,
            "getfield(null) must return the deopt sentinel"
        );
        assert!(
            take_jit_pending_npe(),
            "getfield(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_getfield_oob_slot_does_not_read_past_object() {
        // B2 fix (review `vm-runtime.md`): a stale/miscompiled `field_index`
        // must NOT read past the object into the neighbouring heap object
        // (info leak / follow-on UAF). The helper now bounds-checks the slot
        // against the receiver's `num_slots` header field — exactly like the
        // symmetric `jit_putfield_slot_in_bounds` guard on the putfield
        // helpers — and returns 0 (mirroring the interpreter's out-of-range
        // `get_field` default) instead of dereferencing.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        // Object with exactly 2 reference fields (num_slots == 2).
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 2);
        let obj_ptr = obj.as_ptr() as i64;
        // SAFETY: obj_ptr is a live 2-field object; slot indices 2 and 5 are
        // out of range so the helper takes the bounds-check arm and never
        // dereferences past the object. A negative index is likewise rejected.
        let oob_hi = unsafe { jit_getfield(vm_ptr, obj_ptr, 2) };
        assert_eq!(
            oob_hi, 0,
            "getfield on an out-of-range slot must not read OOB"
        );
        // SAFETY: `obj_ptr` is a valid test object header built above; `jit_getfield`
        // bounds-checks the slot index and returns 0 rather than reading OOB.
        let oob_far = unsafe { jit_getfield(vm_ptr, obj_ptr, 5) };
        assert_eq!(oob_far, 0, "getfield far past num_slots must not read OOB");
        let oob_neg = unsafe { jit_getfield(vm_ptr, obj_ptr, -1) };
        assert_eq!(oob_neg, 0, "getfield on a negative slot must not read OOB");
        assert!(
            !take_jit_pending_npe(),
            "an in-range receiver with an OOB slot must not raise NPE"
        );
    }

    /// A reference load must never hand compiled code a primitive payload.
    ///
    /// This is the Tomcat/Derby SIGSEGV of 2026-08-23, reproduced at the helper
    /// instead of waited for. `org.apache.derby.iapi.types.SQLChar.rawData` is
    /// declared `[C` and was read back as `Int(1)`; the emitted code was
    ///
    /// ```text
    ///   call  jit_getfield          ; -> rax = 1
    ///   mov   r10, 8000000000000000h
    ///   cmp   rax, r10              ; the ONLY value it checks for
    ///   je    <deopt>
    ///   mov   eax,[rax+4]           ; arraylength -> SIGSEGV at addr 0x5
    /// ```
    ///
    /// so `i64::MIN` was the only rejected word and every other primitive
    /// payload became a pointer compiled code would follow. Three crashes
    /// carried byte-identical registers (`rax=1`, `addr=0x5`), which is what
    /// showed the miscompiled body was deterministic and only its REACHABILITY
    /// was racy — 3 crashes in 38 runs one hour, 0 in 129 the next. An
    /// end-to-end test could not tell a fix from luck; this can.
    ///
    /// Exercises the LEGACY 16-byte slot arm, which is the one that crashed:
    /// a `ClassId` with no registered compact layout takes it.
    #[test]
    fn a_reference_getfield_refuses_a_primitive_found_in_the_slot() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 2);
        let obj_ptr = obj.as_ptr() as i64;
        // Slot 1 punned: an int where a reference belongs. That is the G30-1
        // species this VM still produces; the point here is what the JIT helper
        // does when handed one, not how it got there.
        vm_box.mem.heap.set_field(obj, 1, Value::Int(1));

        let before = jit_getfield_primitive_in_ref_slot();

        // A caller that will NOT dereference still gets the payload — that arm
        // is what makes this helper usable for primitive fields at all, and
        // breaking it would be a far larger bug than the one being fixed.
        let as_primitive = unsafe { jit_getfield(vm_ptr, obj_ptr, 1) };
        assert_eq!(
            as_primitive, 1,
            "a primitive load still returns its payload"
        );

        // A caller that WILL dereference gets null.
        let ref_arg = cratonvm_jit_api::getfield_index_arg(1, true, false, 0) as i64;
        let as_reference = unsafe { jit_getfield(vm_ptr, obj_ptr, ref_arg) };
        assert_eq!(
            as_reference,
            0,
            "a reference load returned {as_primitive}, which compiled code \
             dereferences at {:#x}",
            as_primitive + 4
        );
        assert_eq!(
            jit_getfield_primitive_in_ref_slot() - before,
            1,
            "the containment counter must move, or the fix is invisible in a run"
        );

        // The flag must not disturb the case it exists to protect: a real
        // reference still comes back as its own pointer, and null as 0.
        let other = vm_box.mem.heap.alloc_object(ClassId::new(0), 1);
        vm_box
            .mem
            .heap
            .set_field(obj, 0, Value::Object(Some(other)));
        let arg0 = cratonvm_jit_api::getfield_index_arg(0, true, false, 0) as i64;
        assert_eq!(
            unsafe { jit_getfield(vm_ptr, obj_ptr, arg0) },
            other.as_ptr() as i64,
            "a genuine reference must pass through the new guard untouched"
        );
        vm_box.mem.heap.set_field(obj, 0, Value::Object(None));
        assert_eq!(
            unsafe { jit_getfield(vm_ptr, obj_ptr, arg0) },
            0,
            "null must still read back as 0, not as a refusal"
        );
        assert!(
            !take_jit_pending_npe(),
            "containing a punned slot is not an NPE — the receiver was fine"
        );
    }

    /// zt t25/hunt8: h3_P2's receiver, rebuilt. Its corrupt cell was
    /// `raw0=0x656d726f66736e61 raw1=0x72` = `"ansforme"` + `"r"` + padding:
    /// bytes 32..41 of the 41-byte name below, which is the 16-byte cell at
    /// slot 2 (`HEADER_SIZE + 2 * SLOT_SIZE` = 48 = data offset 32) of the
    /// `byte[]` holding it. The bound admits slot 2 because the array's LENGTH
    /// (41) sits in the shape word `num_slots` reads.
    ///
    /// With the screen ON a getfield raises the miscompile NPE (sentinel +
    /// pending NPE) instead of decoding the bytes, a putfield leaves every
    /// element as it was, and both are counted. With `=0` the old answers come
    /// back: the striding read decodes the corrupt cell to null (0, no NPE) --
    /// the kill arm is the positive control that this receiver really reaches
    /// the legacy arm.
    #[test]
    fn zt25hunt8_a_compiled_field_access_on_an_array_receiver_is_refused() {
        const NAME: &[u8] = b"java/lang/instrument/ClassFileTransformer";
        // Worked by hand: 41 bytes; data[32..40] = "ansforme", data[40] = 'r'.
        assert_eq!(NAME.len(), 41);
        assert_eq!(&NAME[32..41], b"ansformer");

        let _ = take_jit_pending_npe();
        let (vm_box, arr_ptr) = alloc_test_array(ArrayElementType::Byte, NAME.len());
        let vm_ptr = (&*vm_box as *const crate::vm::SharedVm) as i64;
        let data = (arr_ptr as *mut u8).wrapping_add(cratonvm_types::ARRAY_DATA_OFFSET);
        // SAFETY: a fresh byte[41]; its data area holds exactly 41 bytes.
        unsafe { std::ptr::copy_nonoverlapping(NAME.as_ptr(), data, NAME.len()) };
        // The cell the old read decoded is h3_P2's, byte for byte.
        // SAFETY: slot 2's cell is body offset 48..64, inside the 16 + 48-byte
        // (41 rounded up to 8) allocation.
        let (raw0, raw1) = unsafe {
            let p = (arr_ptr as *const u8).add(HEADER_SIZE + 2 * SLOT_SIZE);
            (
                std::ptr::read_unaligned(p as *const u64),
                std::ptr::read_unaligned(p.add(8) as *const u64),
            )
        };
        assert_eq!(raw0, 0x656d_726f_6673_6e61, "raw0 of the t22 report");
        assert_eq!(raw1 & 0xff, 0x72, "raw1's low byte of the t22 report");

        let snapshot = || {
            let mut out = [0u8; 41];
            // SAFETY: 41 readable bytes of the live array's data area.
            unsafe { std::ptr::copy_nonoverlapping(data as *const u8, out.as_mut_ptr(), 41) };
            out
        };

        // Screen ON (unset = default).
        let refusals = jit_field_array_receiver_refusals();
        let census = cratonvm_types::cell_census::array_receiver();
        let (as_int, as_ref) = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_FIELD_ARRAY_RECEIVER_SCREEN", None)],
            || {
                let as_int = unsafe { jit_getfield(vm_ptr, arr_ptr, 2) };
                let int_npe = take_jit_pending_npe();
                let ref_arg = cratonvm_jit_api::getfield_index_arg(2, true, false, 0) as i64;
                let as_ref = unsafe { jit_getfield(vm_ptr, arr_ptr, ref_arg) };
                let ref_npe = take_jit_pending_npe();
                assert!(int_npe && ref_npe, "an array receiver must raise the NPE");
                unsafe { jit_putfield_int(arr_ptr, 2, 0x4141_4141) };
                unsafe { jit_putfield_long(arr_ptr, 1, -1) };
                (as_int, as_ref)
            },
        );
        assert_eq!(as_int, i64::MIN, "getfield on an array: the deopt sentinel");
        assert_eq!(as_ref, i64::MIN, "a reference getfield too");
        assert_eq!(
            snapshot().as_slice(),
            NAME,
            "a refused putfield wrote nothing"
        );
        assert!(
            jit_field_array_receiver_refusals() - refusals >= 4,
            "two reads and two writes refused, and counted"
        );
        assert!(
            cratonvm_types::cell_census::array_receiver() - census >= 4,
            "the corrupt-cell census counts them beside the heap accessors"
        );

        // Kill arm: the old striding read decodes the corrupt cell to null.
        let killed = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_FIELD_ARRAY_RECEIVER_SCREEN", Some("0"))],
            || unsafe { jit_getfield(vm_ptr, arr_ptr, 2) },
        );
        assert_eq!(killed, 0, "=0: the t22 answer, a corrupt cell read as null");
        assert!(!take_jit_pending_npe(), "=0 raises nothing");
    }

    /// zt t25/hunt8: the screen does not touch a plain legacy instance -- the
    /// receivers it exists for are never instances, and a false refusal here
    /// would turn every legacy field read into an NPE.
    #[test]
    fn zt25hunt8_an_instance_receiver_passes_the_array_screen() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 3);
        let obj_ptr = obj.as_ptr() as i64;
        // The global refusal counter is not asserted: the array test beside
        // this one moves it in parallel. The round trip is the claim.
        unsafe { jit_putfield_int(obj_ptr, 2, 41) };
        assert_eq!(
            unsafe { jit_getfield(vm_ptr, obj_ptr, 2) },
            41,
            "an instance's legacy cell is written and read as before"
        );
        assert!(!take_jit_pending_npe(), "an instance is never refused");
    }

    /// zt t26/hunt9: an array-receiver refusal names the compiled method and
    /// bci that were handed the array (`compiled_site`), with the inline chain
    /// and the callers, and says where the bci came from. The t25 line carried
    /// only `jit_callee`, which is empty unless `CRATONVM_DBG_JIT_PUTFIELD` is
    /// set, so page item 3 could not be closed from it.
    #[test]
    fn zt26hunt9_an_array_receiver_refusal_names_the_compiled_site() {
        use crate::jit::conservative_roots::InlinedLevel;
        let level = |label: &str, bci: u32| InlinedLevel {
            label: std::sync::Arc::from(label),
            bci,
            class_id: 0,
            hidden: None,
        };
        // h3_P2's suspect: String.hash (field index 2) read by an inlined
        // String.hashCode inside HashMap.hash, called from HashMap.get.
        let chain = [level("java/lang/String.hashCode:()I", 1)];
        let frames = [
            RefusalFrame {
                label: "java/util/HashMap.hash:(Ljava/lang/Object;)I",
                bci: 9,
                exact: true,
                chain: &chain,
            },
            RefusalFrame {
                label: "java/util/HashMap.get:(Ljava/lang/Object;)Ljava/lang/Object;",
                bci: 2,
                exact: false,
                chain: &[],
            },
            RefusalFrame {
                label: "Main.main:([Ljava/lang/String;)V",
                bci: -1,
                exact: false,
                chain: &[],
            },
        ];
        assert_eq!(
            format_array_receiver_site(&frames),
            "method=java/util/HashMap.hash:(Ljava/lang/Object;)I bci=9 \
             bci_source=npe-trap-site inlined=[java/lang/String.hashCode:()I@1] \
             callers=[java/util/HashMap.get:(Ljava/lang/Object;)Ljava/lang/Object;@2 <- \
             Main.main:([Ljava/lang/String;)V@-1]"
        );
        // A putfield (no trap site): the bci is the frame's last safepoint.
        let deeper = [level("a/B.c:()V", 4), level("a/B.d:()V", 7)];
        let put = [RefusalFrame {
            label: "a/B.e:()V",
            bci: 12,
            exact: false,
            chain: &deeper,
        }];
        assert_eq!(
            format_array_receiver_site(&put),
            "method=a/B.e:()V bci=12 bci_source=safepoint-id \
             inlined=[a/B.c:()V@4 <- a/B.d:()V@7] callers=[]"
        );
        let unplaced = [RefusalFrame {
            label: "a/B.e:()V",
            bci: -1,
            exact: false,
            chain: &[],
        }];
        assert!(format_array_receiver_site(&unplaced).contains("bci=-1 bci_source=none"));
        // No compiled activation: a runtime caller (this test thread).
        assert!(format_array_receiver_site(&[]).starts_with("none (no compiled activation"));
        assert!(
            array_receiver_compiled_site(0).starts_with("none (no compiled activation"),
            "a unit-test thread has no compiled frame"
        );
    }

    /// zt t18/zb1 (zr1br A2a): with the ZGC read barrier ARMED, the legacy
    /// cell arm of `jit_getfield` reads through the heap's barriered accessor:
    /// in plain-store mode it answers exactly what the raw arm answers (a
    /// reference, null, a primitive, a refused punned slot), and a slot
    /// holding a COLORED word -- which the raw arm would hand compiled code
    /// verbatim -- comes back as its object.
    #[cfg(feature = "zgc")]
    #[test]
    fn zt18zb1_an_armed_legacy_getfield_reads_through_the_barrier() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        // zt t19/x15: the arming flips the process-wide gate.
        let _gate = crate::ZGC_READ_GATE_TEST_LOCK.lock();

        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let crate::memory::vm_heap::VmHeap::Zgc(z) = &vm_box.mem.heap else {
            return; // the default backend is not ZGC in this build
        };
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        let heap = &vm_box.mem.heap;
        let target = heap.alloc_object(ClassId::new(0), 0);
        // No registered layout for class 0: a legacy body of 16-byte cells.
        let obj = heap.alloc_object(ClassId::new(0), 4);
        heap.set_field(obj, 0, Value::Object(Some(target)));
        heap.set_field(obj, 1, Value::Int(7));
        heap.set_field(obj, 2, Value::Object(None));
        heap.set_field(obj, 3, Value::Int(1));
        let obj_ptr = obj.as_ptr() as i64;
        let reference = |i: u32| cratonvm_jit_api::getfield_index_arg(i, true, false, 0) as i64;
        let read_all = || -> Vec<i64> {
            // SAFETY: a live 4-slot legacy object; every index is in range.
            unsafe {
                vec![
                    jit_getfield(vm_ptr, obj_ptr, reference(0)),
                    jit_getfield(vm_ptr, obj_ptr, 1),
                    jit_getfield(vm_ptr, obj_ptr, reference(2)),
                    jit_getfield(vm_ptr, obj_ptr, reference(3)),
                ]
            }
        };
        let unarmed = read_all();
        assert!(z.arm_load_barrier_plain_stores(false));
        let armed = read_all();
        // A colored word in slot 0's payload, as a coloring store would write.
        let base = z.conservative_addr_span().expect("an arena").0 as u64;
        let payload = (obj.as_ptr() as usize
            + HEADER_SIZE
            + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *mut u64;
        let colored: u64 = cratonvm_gc::zgc::vaddr::color(
            target.as_ptr() as u64 - base,
            cratonvm_gc::zgc::vaddr::ZColor::Remapped,
        );
        // SAFETY: the 8-byte payload word of slot 0 of a live legacy object.
        unsafe { std::ptr::write_volatile(payload, colored) };
        // SAFETY: as `read_all`.
        let healed = unsafe { jit_getfield(vm_ptr, obj_ptr, reference(0)) };
        z.set_barrier_color(None);
        // SAFETY: as above; restore the plain word before the heap is dropped.
        unsafe { std::ptr::write_volatile(payload, target.as_ptr() as u64) };

        assert_eq!(unarmed[0], target.as_ptr() as i64);
        assert_eq!(unarmed[1], 7);
        assert_eq!(unarmed[2], 0);
        assert_eq!(unarmed[3], 0, "a punned reference slot is refused");
        assert_eq!(armed, unarmed, "plain-store arming changes no answer");
        assert_eq!(
            healed,
            target.as_ptr() as i64,
            "the armed legacy arm resolves a colored word to its object"
        );
        assert!(!take_jit_pending_npe());
    }

    /// zt t19/zb2 (zr1br A2b): on a ZGC heap armed with COLORED stores the
    /// JIT's reference store helpers store through the heap accessor --
    /// `jit_putfield_object` leaves a PLAIN word in the legacy cell (zt t20/zb3),
    /// `jit_aastore` of null leaves `Z_NULL` -- and the read helpers the armed
    /// compiled `aaload` / `getfield` arms call (`jit_aaload`, `jit_getfield`)
    /// decode a colored word to its object.
    #[cfg(feature = "zgc")]
    #[test]
    fn zt19zb2_colored_store_helpers_store_through_the_heap_and_reads_decode() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use cratonvm_gc::zgc::vaddr::is_colored_word;
        // zt t21/x17: the colored arming flips the process-wide gate (and
        // publishes the process-wide colored-slot decode base), as the zt
        // t18/zb1 arming test above does.
        let _gate = crate::ZGC_READ_GATE_TEST_LOCK.lock();

        let _ = take_jit_pending_npe();
        let _ = take_jit_pending_aioobe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let crate::memory::vm_heap::VmHeap::Zgc(z) = &vm_box.mem.heap else {
            return; // the default backend is not ZGC in this build
        };
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        let heap = &vm_box.mem.heap;
        let target = heap.alloc_object(ClassId::new(0), 0);
        // No registered layout for class 0: a legacy body of 16-byte cells.
        let obj = heap.alloc_object(ClassId::new(0), 1);
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
        let (obj_ptr, arr_ptr) = (obj.as_ptr() as i64, arr.as_ptr() as i64);
        let payload = (obj.as_ptr() as usize
            + HEADER_SIZE
            + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const u64;
        let elem0 = (arr.as_ptr() as usize + HEADER_SIZE) as *const u64;

        assert!(z.arm_load_barrier_colored_stores());
        let colored_mode = heap.zgc_load_barrier_colored_stores();
        heap.set_array_element(arr, 0, Value::Object(Some(target)))
            .expect("in bounds");
        // SAFETY: `obj` is a live one-slot legacy object, `target` a live
        // object, `arr` a live two-element reference array; every index is in
        // range and `vm_ptr` is the live VM that owns all three.
        let (field_word, elem_word, field_read, elem_read, null_read, cleared) = unsafe {
            jit_putfield_object(vm_ptr, obj_ptr, 0, target.as_ptr() as i64);
            let field_word = std::ptr::read_volatile(payload);
            let elem_word = std::ptr::read_volatile(elem0);
            let reference = cratonvm_jit_api::getfield_index_arg(0, true, false, 0) as i64;
            let field_read = jit_getfield(vm_ptr, obj_ptr, reference);
            let elem_read = jit_aaload(vm_ptr, arr_ptr, 0);
            let null_read = jit_aaload(vm_ptr, arr_ptr, 1);
            jit_aastore(vm_ptr, arr_ptr, 0, 0);
            let cleared = std::ptr::read_volatile(elem0);
            (
                field_word, elem_word, field_read, elem_read, null_read, cleared,
            )
        };
        z.set_barrier_color(None);
        // Plain words again before the heap is dropped.
        heap.set_field(obj, 0, Value::Object(None));

        assert!(colored_mode, "armed with colored stores");
        // zt t20/zb3: a LEGACY cell stays plain on a colored-store heap (its
        // readers move whole `Value`s); the array element is colored.
        assert!(
            !is_colored_word(field_word) && field_word == target.as_ptr() as u64,
            "putfield stored {field_word:#x}"
        );
        assert!(is_colored_word(elem_word), "the element is {elem_word:#x}");
        assert_eq!(field_read, target.as_ptr() as i64, "getfield decodes");
        assert_eq!(elem_read, target.as_ptr() as i64, "aaload decodes");
        assert_eq!(null_read, 0, "a null element reads 0");
        assert_eq!(
            cleared,
            cratonvm_gc::zgc::vaddr::Z_NULL,
            "a null store is Z_NULL"
        );
        assert!(!take_jit_pending_npe());
        assert_eq!(take_jit_pending_aioobe(), None);
    }

    /// The flag bits must not be mistaken for part of the slot index.
    ///
    /// `field_index` is masked with `GETFIELD_FLAG_BITS`, and the bounds check
    /// that stops an out-of-range slot reading into the neighbouring object
    /// runs on the masked value. A mask that missed the new bit would turn
    /// every reference load into an enormous index — caught by the bounds
    /// check as `0`, i.e. every reference field in the VM silently reading
    /// null. That is a quiet catastrophe rather than a loud one, so it is
    /// asserted directly.
    #[test]
    fn the_getfield_flag_bits_are_stripped_before_the_slot_index_is_used() {
        use cratonvm_jit_api::{
            getfield_index_arg, GETFIELD_EXPECT_REFERENCE, GETFIELD_FLAG_BITS,
            GETFIELD_IMPLICIT_NULL_SITE, GETFIELD_NO_KEEPALIVE, GETFIELD_NPE_SITE_MASK,
            GETFIELD_RECEIVER_PROVEN_OOP,
        };
        assert_eq!(
            GETFIELD_FLAG_BITS,
            GETFIELD_EXPECT_REFERENCE
                | GETFIELD_RECEIVER_PROVEN_OOP
                | GETFIELD_NPE_SITE_MASK
                | GETFIELD_IMPLICIT_NULL_SITE
                | GETFIELD_NO_KEEPALIVE,
            "every flag must be in the strip mask"
        );
        for (index, is_ref, proven) in [
            (0u32, false, false),
            (1, true, false),
            (7, true, true),
            (u16::MAX as u32, true, true),
        ] {
            // A non-zero trap key on every row: it is the widest passenger and
            // the newest, so a strip site that misses it turns the slot index
            // into an astronomical one.
            let arg = getfield_index_arg(index, is_ref, proven, 0x00ff_ffff);
            assert_eq!(
                arg & !GETFIELD_FLAG_BITS,
                index as u64,
                "the index must survive the flags"
            );
            assert_eq!(
                arg & GETFIELD_EXPECT_REFERENCE != 0,
                is_ref,
                "EXPECT_REFERENCE must ride on exactly the reference loads"
            );
            assert_eq!(
                arg & GETFIELD_RECEIVER_PROVEN_OOP != 0,
                is_ref && proven,
                "the receiver proof is only claimed on the reference path"
            );
        }
    }

    #[test]
    fn jit_getfield_rejects_pointer_shaped_non_heap_receiver() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        // Aligned, above the null guard, and below 47 bits: it passes the
        // context-free plausibility filter, but it is not a heap address in
        // this VM. This is the Tomcat rc=139 shape where generated code had
        // stale/truncated receiver bits before getfield.
        let mut bad_receiver = 0x1000_i64;
        while vm_box
            .mem
            .heap
            .is_object_address(bad_receiver as usize)
            .is_some()
        {
            bad_receiver += 0x1000;
        }
        assert!(cratonvm_types::plausible_heap_pointer(bad_receiver as u64));
        let result = unsafe { jit_getfield(vm_ptr, bad_receiver, 0) };
        assert_eq!(result, i64::MIN);
        assert!(take_jit_pending_npe());
    }

    #[test]
    fn jit_getfield_in_bounds_reads_stored_int() {
        // The fast in-bounds path must be untouched by the B2 fix: a value
        // written into a valid slot reads back identically.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 2);
        // Write via the interpreter path (the helper read must observe it).
        vm_box.mem.heap.set_field(obj, 1, Value::Int(0x5A5A));
        let obj_ptr = obj.as_ptr() as i64;
        // SAFETY: obj_ptr is a live 2-field object; slot 1 is in bounds.
        let v = unsafe { jit_getfield(vm_ptr, obj_ptr, 1) };
        assert_eq!(
            v, 0x5A5A,
            "in-bounds getfield must read back the stored value"
        );
        assert!(
            !take_jit_pending_npe(),
            "an in-bounds getfield must not raise NPE"
        );
    }

    // Regression test for the PLAIN-SLOT TEARING FIX (2026-07-06, see
    // elasticsearch-lucene-binary-docvalues-range-hangs.md
    // #3): `jit_getfield` used to read a 16-byte `Value` slot via a bare,
    // non-atomic `ptr::read`, asymmetric with `jit_putfield_*`'s already-
    // atomic `write_value_atomic` (commit 4e6b560f). Two threads hammering
    // the same field slot -- one via `jit_putfield_int` (mirroring a
    // JIT-compiled `putfield`), one via `jit_getfield` (mirroring a
    // JIT-compiled `getfield`) -- must never observe a torn value.
    #[test]
    fn jit_getfield_never_tears_against_concurrent_jit_putfield_int() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let vm_box: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = Arc::as_ptr(&vm_box) as i64;
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 1);
        let obj_ptr = obj.as_ptr() as i64;

        const A: i32 = 0x1111_1111;
        const B: i32 = 0x2222_2222_u32 as i32;
        // The writer's floor, unchanged: on a machine that interleaves at all,
        // this is exactly the old exercise.
        const ITERATIONS: usize = 2_000_000;
        // ...and its ceiling, reached only when the reader has STILL not seen
        // both values after the floor — i.e. the two threads never overlapped.
        // Bounding by observed progress rather than a fixed count is the point:
        // a fixed count stops at an arbitrary line regardless of whether the
        // test's own premise was ever established.
        const MAX_ITERATIONS: usize = 20_000_000;
        // How often the writer samples the progress flag. Kept coarse so the
        // store loop this test exists to stress stays tight.
        const PROGRESS_POLL_MASK: usize = 0xFFFF;
        // Establish A as the slot'''s initial value BEFORE spawning the reader:
        // a freshly-allocated slot is zero-initialized (decodes as Value::Int(0)),
        // and 0 is neither A nor B, so a reader started before the writer'''s
        // first store would see a legitimate-but-unaccounted-for transient
        // value -- a test-harness race, not a torn read.
        unsafe { jit_putfield_int(obj_ptr, 0, A as i64) };
        let stop = Arc::new(AtomicBool::new(false));
        // Raised by the reader once it has observed BOTH values — the moment
        // this test's premise (the two threads actually overlap) is satisfied.
        let interleaved = Arc::new(AtomicBool::new(false));

        let writer = {
            let stop = Arc::clone(&stop);
            let interleaved = Arc::clone(&interleaved);
            std::thread::spawn(move || {
                let mut i = 0usize;
                loop {
                    let v = if i % 2 == 0 { A } else { B };
                    // SAFETY: obj_ptr is a live, single-field object; slot 0
                    // is in bounds.
                    unsafe { jit_putfield_int(obj_ptr, 0, v as i64) };
                    i += 1;
                    if i >= ITERATIONS {
                        // Past the floor: keep writing only while the reader
                        // has yet to see both values, so a starved reader gets
                        // a real chance instead of the loop ending on a count.
                        if i >= MAX_ITERATIONS {
                            break;
                        }
                        if i & PROGRESS_POLL_MASK == 0 && interleaved.load(Ordering::Acquire) {
                            break;
                        }
                    }
                }
                stop.store(true, Ordering::Release);
                i
            })
        };

        let reader = {
            let stop = Arc::clone(&stop);
            let interleaved = Arc::clone(&interleaved);
            std::thread::spawn(move || {
                let mut seen_a = 0usize;
                let mut seen_b = 0usize;
                while !stop.load(Ordering::Acquire) {
                    // SAFETY: obj_ptr is a live, single-field object; slot 0
                    // is in bounds.
                    let v = unsafe { jit_getfield(vm_ptr, obj_ptr, 0) } as i32;
                    if v == A {
                        seen_a += 1;
                    } else if v == B {
                        seen_b += 1;
                    } else {
                        // THE assertion. One check per read, and the only
                        // condition here that indicates a real defect.
                        panic!(
                            "torn read: observed {v:#x}, neither of the two \
                             legitimate written values ({A:#x}, {B:#x})"
                        );
                    }
                    if seen_a > 0 && seen_b > 0 && !interleaved.load(Ordering::Relaxed) {
                        // Release the writer from its extended budget.
                        interleaved.store(true, Ordering::Release);
                    }
                }
                (seen_a, seen_b)
            })
        };

        let writes = writer.join().unwrap();
        let (seen_a, seen_b) = reader.join().unwrap();

        // Everything above this line has already run the tearing check on every
        // one of the reader's observations. What remains is a PREMISE check:
        // did the two threads actually overlap? On a loaded shared machine they
        // can fail to, and that is a coverage gap, not a tearing violation —
        // failing here reports a bug that did not happen and hands every
        // unrelated change on a busy host a red suite to triage.
        //
        // Measured 2026-08-06 at load ~25-100: seen_a=669958, seen_b=0, i.e.
        // 670 K reads that all passed the tearing check while the writer never
        // got a core alongside the reader. Same tree passed in isolation.
        let reads = seen_a + seen_b;
        if reads == 0 {
            // The reader never executed a single read — the writer finished and
            // set `stop` before the reader was scheduled at all. NOTHING was
            // checked, so say that rather than implying a clean result.
            eprintln!(
                "SKIP jit_getfield_never_tears_against_concurrent_jit_putfield_int: \
                 the reader never performed a single read (writes={writes}), so the \
                 tearing check did not run and this execution establishes NOTHING \
                 either way. A scheduling outcome on a contended host, not a defect."
            );
            return;
        }
        if seen_a == 0 || seen_b == 0 {
            // The reader ran and every read passed the tearing check, but the
            // slot never changed under it, so the A<->B transition — the window
            // a torn read could appear in — went unexercised. A coverage gap,
            // not a violation.
            eprintln!(
                "SKIP jit_getfield_never_tears_against_concurrent_jit_putfield_int: \
                 {reads} reads all passed the tearing check, but the writer and \
                 reader never overlapped, so the A<->B transition was never \
                 exercised (seen_a={seen_a} seen_b={seen_b} writes={writes} \
                 floor={ITERATIONS} cap={MAX_ITERATIONS}). A scheduling outcome \
                 on a contended host, not a defect."
            );
            return;
        }
    }

    #[test]
    fn jit_thread_cleared_returns_none() {
        clear_jit_thread();
        // SAFETY: The JIT thread pointer was just cleared above, so jit_thread_mut
        // returns None without dereferencing any pointer.
        let result = unsafe { jit_thread_mut() };
        assert!(result.is_none());
    }

    // Regression: when the JIT JIT passes a NaN-boxed CompactValue raw bit
    // pattern as the array length (e.g. `0xFFFC_0000_0000_000B` for int(11)),
    // the helper must extract the low-32 int payload and NOT treat the upper
    // tag bits as part of the length. Previously, `length as usize` cast the
    // tag bits into an enormous unsigned value, triggering
    // "array data size overflow in gen_heap alloc_array" / "young gen exhausted".
    //
    // Repros: `bench/fannkuch` (n=11 → 0xFFFC_..._000B) and
    // `bench/FullStackBench` phase 5 (`new boolean[100000]`).
    #[test]
    fn jit_newarray_strips_nanbox_tag_from_length() {
        // 0xFFFC_0000_0000_000B is the NaN-boxed CompactValue for int(11)
        // (NANBOX_BITS | SUB_INT << 47 | 11). After narrowing to i32, the
        // value should be 11 — non-negative, so the helper takes the
        // early `vm_ptr == 0` exit rather than the abort path.
        let nan_boxed_11: i64 = 0xFFFC_0000_0000_000B_u64 as i64;
        // SAFETY: vm_ptr=0 hits the explicit null check after length narrowing,
        // so no dereference occurs.
        let result = unsafe {
            jit_newarray(0, 10 /* T_INT */, nan_boxed_11)
        };
        assert_eq!(result, 0, "jit_newarray must not abort on NaN-boxed length");
    }

    #[test]
    fn jit_anewarray_strips_nanbox_tag_from_length() {
        let nan_boxed_11: i64 = 0xFFFC_0000_0000_000B_u64 as i64;
        // SAFETY: vm_ptr=0 hits the explicit null check after length narrowing.
        let result = unsafe { jit_anewarray_object(0, 0, nan_boxed_11) };
        assert_eq!(
            result, 0,
            "jit_anewarray_object must not abort on NaN-boxed length"
        );
    }

    #[test]
    fn jit_newarray_negative_length_returns_zero_not_abort() {
        // Sign-extended -5 (0xFFFF_FFFF_FFFF_FFFB). After narrowing to i32,
        // value is -5; the helper must return 0 (would-be NegativeArraySize)
        // instead of casting to a huge usize and aborting.
        let neg_5: i64 = -5;
        // SAFETY: negative-length path returns 0 before any dereference.
        let result = unsafe { jit_newarray(0, 10, neg_5) };
        assert_eq!(result, 0);
        // SAFETY: negative-length path returns 0 before any dereference.
        let result2 = unsafe { jit_anewarray_object(0, 0, neg_5) };
        assert_eq!(result2, 0);
    }

    // -----------------------------------------------------------------
    // Task #43 (HIGH soundness): SATB pre-barrier + real-STW newarray
    // -----------------------------------------------------------------
    //
    // The two regression tests below pin the acceptance criteria from
    // task #43 (deferred from #25/#26):
    //
    //   (1) `jit_putfield_object` records the OLD reference in the SATB
    //       queue *before* overwriting the slot. Without this, concurrent
    //       marking loses any still-live ref reachable only through the
    //       overwritten slot, turning the next mixed evacuation into a
    //       use-after-free.
    //
    //   (2) `jit_newarray` under a low-heap-pressure / try_alloc_young_probe
    //       failure correctly drives a GC through the orchestrated STW
    //       path (`maybe_gc_forced_pub`) and successfully completes the
    //       follow-up `alloc_array` call without crashing. Previously the
    //       helper called `heap.collect_garbage` directly, bypassing the
    //       `gc_barrier.request_stw()` handshake — a multi-threaded UAF.

    /// Task #43 acceptance #3 (JIT-compiled putfield ref-store with
    /// non-null old ref correctly enqueues `old` in the SATB log).
    ///
    /// Build a SharedVm, install + activate the SATB queue, allocate a
    /// container object plus two payload objects, write the first
    /// payload into slot 0, then drive `jit_putfield_object` to
    /// overwrite slot 0 with the second payload. The first payload's
    /// raw address must land in the SATB queue after we drain the
    /// per-thread buffer.
    #[test]
    fn jit_putfield_object_satb_pre_barrier_enqueues_old_ref() {
        // The overwritten value below is YOUNG: pin the old-only SATB filter
        // (default on since gcf 2026-10-01) off so the barrier alone decides.
        let _flags = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_GEN_SATB_OLD_ONLY",
                Some("0"),
            )]),
        );
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use cratonvm_gc::{ConcurrentGcState, SatbQueue};
        use std::sync::Arc;

        // Build the SharedVm via Box so we can take a `&mut` to enable
        // concurrent GC before sharing it. The JIT helper only requires
        // a raw `*const SharedVm` pointer, so no Arc is needed.
        //
        // The collector is PINNED, not defaulted. This test is about the
        // GENERATIONAL backend's `satb_barrier` (as its own comment below
        // says), and SATB does not exist on ZGC at all — that backend is a
        // stop-the-world non-concurrent mark-sweep with no marking phase to
        // keep a snapshot for. Naming the subject rather than inheriting
        // `VmConfig::default()` is what makes the test survive a
        // default-collector change; it did not survive the 2026-08-10 flip to
        // `Zgc`, which is how this line came to be written.
        let cfg = VmConfig {
            gc_algorithm: crate::config::GcAlgorithm::Generational,
            ..VmConfig::default()
        };
        let mut vm_box: Box<SharedVm> = Box::new(SharedVm::new(cfg));

        // Wire up the SATB queue + concurrent GC state on the heap. The
        // generational backend's `satb_barrier` is a hard no-op until
        // both the queue and the state are present AND the state reports
        // marking active (ConcurrentMark or Remark phase).
        let satb: Arc<SatbQueue> = Arc::new(SatbQueue::new());
        let state: Arc<ConcurrentGcState> = Arc::new(ConcurrentGcState::new());
        vm_box
            .mem
            .heap
            .enable_concurrent_gc(satb.clone(), state.clone());

        // Activate marking. Both `satb.activate()` (so `is_active()`
        // returns true) and `state.set_phase(ConcurrentMark)` (so
        // `is_marking_active()` returns true) are required by the
        // generational `satb_barrier` fast-path gate.
        satb.activate();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);

        // Allocate a container with one reference field plus two payload
        // objects to use as old/new references for the putfield store.
        let container = vm_box.mem.heap.alloc_object(ClassId::new(0), 1);
        let old_obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 0);
        let new_obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 0);

        // Pre-write the old reference into slot 0 (interpreter path —
        // bypasses the SATB barrier we are about to test).
        vm_box
            .mem
            .heap
            .set_field(container, 0, Value::Object(Some(old_obj)));

        // Pre-drain any baggage from this thread's local SATB buffer so
        // the test only observes references logged by the JIT helper.
        vm_box.mem.heap.flush_thread_satb();
        let _ = satb.drain();

        let vm_ptr = &*vm_box as *const SharedVm as i64;
        let container_ptr = container.as_ptr() as i64;
        let new_obj_ptr = new_obj.as_ptr() as i64;

        // SAFETY: `vm_ptr` points to a live `SharedVm` (the Box we own);
        // `container_ptr` and `new_obj_ptr` are live heap objects; slot
        // 0 is within the container's declared layout (num_fields=1).
        unsafe {
            jit_putfield_object(vm_ptr, container_ptr, 0, new_obj_ptr);
        }

        // Flush this thread's SATB buffer into the global queue so the
        // drain below sees it. The per-thread buffer auto-flushes at
        // 256 entries; with a single store we must drain explicitly.
        vm_box.mem.heap.flush_thread_satb();
        let drained = satb.drain();

        // The SATB pre-barrier must have logged the OLD reference's
        // raw address (NOT the new ref's address). Searching is robust
        // against unrelated heap activity inside `alloc_object` that
        // might happen to log; the precise acceptance check is
        // "old is present".
        let old_addr = old_obj.as_ptr() as usize;
        assert!(
            drained.contains(&old_addr),
            "jit_putfield_object must SATB-log the OLD ref before overwriting; \
             drained={:?} expected_to_contain={:#x}",
            drained,
            old_addr,
        );

        // The new value must be visible in the slot post-store (sanity
        // check that the helper actually performed the write).
        let post = vm_box.mem.heap.get_field(container, 0);
        match post {
            Value::Object(Some(obj)) => assert_eq!(
                obj.as_ptr() as usize,
                new_obj.as_ptr() as usize,
                "post-store slot must hold the new ref",
            ),
            other => panic!("expected Object(Some) post-store, got {:?}", other),
        }

        // Clean up: deactivate SATB so the box's Drop path doesn't
        // race a marker (none is running in this test, but tidy state
        // is a habit worth keeping).
        // Discards the result, and this test is not a safepoint: the form
        // that cannot claim a snapshot is the correct one.
        satb.deactivate_and_discard();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
    }

    /// Task #43 acceptance #3 (jit_newarray under low heap pressure
    /// correctly triggers GC and re-attempts allocation).
    ///
    /// Drive `try_alloc_young_probe` into the failure arm by requesting
    /// a length that exceeds the young-gen capacity, then verify the
    /// helper does not crash and ultimately returns a non-zero pointer
    /// (the post-GC `alloc_array` succeeds because the heap can grow
    /// or because the requested length still fits after collection).
    ///
    /// The critical bit being tested is that the GC path goes through
    /// the orchestrated STW handshake (`maybe_gc_forced_pub`) — the
    /// previous direct `heap.collect_garbage` call would deadlock or
    /// UAF when multiple threads were active. Running this test under
    /// `--test-threads=2` exercises that handshake.
    #[test]
    fn jit_newarray_under_pressure_drives_real_stw_gc() {
        use crate::config::VmConfig;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::SharedVm;

        // Shrink the heap to a single-digit-MB size so the burn loop
        // below realistically pushes the young gen near exhaustion and
        // forces `try_alloc_young_probe` into the failure arm. Without
        // this, the default 256 MB heap would let the helper hit the
        // probe-success fast path on every iteration, never exercising
        // the orchestrated-STW code path under test.
        let mut config = VmConfig::default();
        config.max_heap_size = 4 * 1024 * 1024; // 4 MB
        config.initial_heap_size = 4 * 1024 * 1024;
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(config));

        // The JIT helper requires a `jit_thread` set via `set_jit_thread`
        // so `jit_thread_mut()` returns Some(thread) — otherwise the
        // GC-trigger arm silently no-ops and the test would not
        // exercise the STW path.
        let mut thread = JvmThread::new(ThreadId(0), "jit_newarray_test");

        // Burn through most of the young gen so the next allocation
        // probe is overwhelmingly likely to fail and drive the
        // `maybe_gc_forced_pub` arm. We use unrooted allocations so
        // they're immediately dead and the post-GC retry succeeds.
        //
        // CRIT: use the *fallible* `try_alloc_array`, not the panicking
        // `alloc_array`. On a 1 MB young gen this loop intentionally runs
        // the from-space to exhaustion; `alloc_array` would hit
        // `alloc_young`'s hard `std::process::abort()` (surfacing on
        // Windows as STATUS_STACK_BUFFER_OVERRUN / 0xC0000409) the instant
        // the heap filled — killing the test process during *setup*, before
        // `jit_newarray` is ever reached. `try_alloc_array` instead returns
        // `None` once the gen is full, which we harmlessly drop: the gen is
        // now pressured exactly as the test requires.
        for _ in 0..256 {
            let _ = vm_box
                .mem
                .heap
                .try_alloc_array(ClassId::new(0), ArrayElementType::Int, 1024);
        }

        // Install the JIT thread pointer so the helper's `jit_thread_mut`
        // returns Some. SAFETY: the thread outlives the helper call;
        // `set_jit_thread` only stashes a `*mut JvmThread` in TLS.
        let prev = set_jit_thread(&mut thread);

        let vm_ptr = &*vm_box as *const SharedVm as i64;
        // T_INT (10) with a moderately large length — large enough that
        // the probe almost certainly fails on the shrunken heap,
        // exercising the GC arm; small enough that the actual
        // `alloc_array` after GC succeeds.
        let len: i64 = 1024;

        // SAFETY: vm_ptr is a live SharedVm; T_INT is a valid atype;
        // length is non-negative. The helper either takes the probe-
        // success fast path or the GC-then-alloc slow path; either way
        // returns a non-zero pointer on success.
        let result = unsafe { jit_newarray(vm_ptr, 10, len) };

        // Restore the prior JIT thread pointer (probably null, but
        // preserve correctness in case the test harness runs in a
        // re-entrant context).
        restore_jit_thread(prev);

        // The post-GC `alloc_array` always runs (no early return on
        // probe-failure), so a non-zero return proves the GC arm did
        // not crash and the heap recovered enough to satisfy the
        // request. A zero return would indicate either an OOM panic
        // turned into None or a regression in the helper's control
        // flow — both of which would surface here.
        assert!(
            result != 0,
            "jit_newarray must return a non-zero ObjectRef pointer after \
             GC-on-pressure (orchestrated STW path); got 0",
        );

        // The returned pointer must reference a live array on this heap
        // with the requested length, confirming the post-GC retry took
        // the regular `alloc_array` path (not some salvage / abort
        // shortcut).
        // SAFETY: `result` is the non-zero array pointer just returned by the
        // successful `jit_newarray` call above, so it is a live, aligned heap object.
        let arr = unsafe { ObjectRef::from_raw(result as usize as *mut u8) };
        assert_eq!(
            vm_box.mem.heap.array_length(arr),
            len as usize,
            "post-GC alloc_array must produce an int[] of the requested length",
        );
    }
}
