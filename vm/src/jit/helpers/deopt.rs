#[allow(unused_imports)]
use super::*;
// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

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
pub fn deopt_budget_refusal_label(
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
    pub fn deoptimize_or_by_name(
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
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::Deoptimization);
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
        // baseline for the same benchmark -- docs/feature-designs/gpu-offload.md, TestResponsePerformance).
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
        // this fix exists to stop. See docs/feature-designs/gpu-offload.md.
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
        // A `Reinterpret` that EVICTED the body is the same recompile-with-a-
        // fresh-profile path (`tiered::deopt_evicts_method_body`) and is
        // re-queued too: left to the hotness retry, a callee reached only from
        // compiled code (`Pattern$BmpCharPropertyGreedy.match` on the Spring MVC
        // row) was never offered again and ran interpreted for the rest of the
        // process (the Spring MVC C2-lane stall, 2026-11-14).
        let evicting_reinterpret =
            action == cratonvm_jit::deopt::DeoptAction::Reinterpret && !skip_eviction;
        if cratonvm_jit::deopt_real_enabled()
            && (action == cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret
                || evicting_reinterpret)
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
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
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
            cratonvm_native_builtins_jfr::jfr_vm_more::deoptimization(
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
pub unsafe fn jit_uncommon_trap_body(vm_ptr: i64, reason: i64, bci: i64) -> i64 {
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
pub fn note_frameless_trap_stub(reason: i64, bci: i64) {
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
pub fn trapping_artifact_is_superseded(
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
pub fn split_method_label(label: &str) -> Option<(String, String, String)> {
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

