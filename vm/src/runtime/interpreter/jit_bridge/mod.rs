// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Every place the interpreter asks for, enters, or leaves compiled code.
//!
//! This module exists to make that list *enumerable*. It was not, and the cost
//! is on the record: the OSR path compiles through `compile_osr_artifact`,
//! which calls the x64 backend directly instead of going through
//! `try_jit_compile_callee`, so everything the ordinary compile door does at
//! entry is silently skipped on that one. Nobody chose that; it was invisible
//! because the two doors were 6,000 lines apart in a 24,000-line file.
//!
//! What lives here:
//!
//! * **Requesting code.** `try_jit_compile_callee` and its slow path, the
//!   background compile task, the negative cache, and `try_jit_upgrade_with_gate`
//!   — the admission gate that decides a method is worth compiling.
//! * **On-stack replacement.** `try_osr` and `compile_osr_artifact`, the second
//!   compile door.
//! * **Entering and leaving.** `execute_jit_call` and its decoded form, the
//!   monitor guard that keeps a `synchronized` compiled frame balanced when it
//!   unwinds, and `jit_panic_to_exception`.
//! * **Deciding what a compile may assume.** The native-shadow and
//!   forced-metadata probes, `resolve_inline_site`, and the elidable-construction
//!   analysis.
//!
//! Adding a new way to reach compiled code means adding it here. That is the
//! point of the file.

use super::*;
mod private_iface_this;
// j27 wave 4, lane SPLICECAT: concat bridge sites as splice indy rows.
mod splice_concat_rows;
// j27 lane ONESHOT: a pending OSR re-entry memo entry re-asks its owed verdict.
mod osr_reentry_pending_reask;
// j27 wave 5, lane PROXY: the exact class set of an interface profile site.
mod profile_class_set;
#[cfg(test)]
mod r15w1_calls_supersede_keep_tests;

/// Round 15 wave 4 (lane sync3, SY2-2): the supersede keep rule for a
/// self-locking fall-through over a wrapped baseline.


/// Round 15 wave 2 (lane calls2): CR15-1 (the in-place callee compile's
/// lookup-only binder), CR15-4 (the optimizing door's verdict as a value) and
/// the keep rule's sum order.
#[cfg(test)]
mod r15w2_calls2_tests;

/// Run `f` while `cm_ptr` is published as the OSR continuation of the frame at
/// `frame_index`, exactly as the OSR door publishes it around a body it runs
/// (interpreter round i1 wave 24, lane L6: the safepoint slow path names an
/// unpublished optimizing OSR body through this record, `helpers.rs`'s
/// `running_osr_body_named`).
#[cfg(test)]
pub(crate) fn with_osr_continuation_for_test<R>(
    frame_index: usize,
    cm_ptr: usize,
    f: impl FnOnce() -> R,
) -> R {
    let _published = OsrContinuationGuard::publish(frame_index + 1, cm_ptr);
    f()
}

#[cfg(test)]
mod r16_callbind_spliced_final_call_tests;

#[cfg(test)]
mod i12_l2_spliced_call_holder_tests;
#[cfg(test)]
mod nio21w7_byte_array_field_row_tests;
#[cfg(test)]
mod inline_path_entry_tests;
/// The resolver's feature gates, read once per VM instead of once per
/// resolution. Each is a flag-table probe (`runtime_flag_default_on` hashes the
/// name into `VmFlags::declared`); the resolver read about seventy of them per
/// body. The names are the gate functions' own, so a field and its reader
/// cannot drift. Flags are fixed for the process, except under a test override
/// ([`inline_gates`]).
macro_rules! inline_gates {
    ($($name:ident = $path:path,)*) => {
        #[derive(Clone)]
        pub(crate) struct InlineGates {
            $(pub(crate) $name: bool,)*
        }

        impl InlineGates {
            fn read() -> Self {
                Self { $($name: $path(),)* }
            }
        }
    };
}

#[cfg(test)]
mod cf_w8_cold_callee_late_bind_tests;
#[cfg(test)]
mod nio20w6_l3_ldc_mint_at_offer_tests;
/// Interpreter round i7 wave 136, lane L2: stage 0 of
/// `docs/feature-designs/real-jdk-mode.md`
/// without a sampler. `CRATONVM_DBG_DOOR_CYCLES=1` (diagnostic, read once per
/// process) times every OUTERMOST [`run_jit_body_raw`] with the cycle counter
/// (`mic_prof::now`): the whole call, and the compiled call inside it (the
/// `try_call` / `try_call_with_context` alone). `door - body` is the
/// bookkeeping the proposal would fold (the deopt / stamp retire, the
/// exceptional-stash floor, `set_jit_thread` / `restore_jit_thread`, the
/// poll-word mirror, `JitEntryGuard`, the signal drain, the sentinel
/// classification). One line at exit (`[door-cycles]`, from
/// `interp_census::report_at_exit`), with the counter's ticks per wall
/// nanosecond measured over the run so the orchestrator can turn cycles into
/// ns. Each timed call pays four counter reads (about 20-40 cycles), counted
/// inside `door`; the line's `self_cost` row is that cost measured on this
/// machine, to subtract. Off: one cached-flag load per door call.
pub(crate) mod door_cycles {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static CALLS: AtomicU64 = AtomicU64::new(0);
    static DOOR: AtomicU64 = AtomicU64::new(0);
    // `[0]`: the compiled body. `[1..=5]`: t1 wave 5 (lane S) split
    // segments under `CRATONVM_DBG_DOOR_CYCLES=split` -- pre-entry, guard
    // entry, guard exit, `restore_jit_thread`, signal drain and classification.
    // One array so the per-VM statics ratchet's count does not move.
    static BODY: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
    static FIRST_TICK: AtomicU64 = AtomicU64::new(0);
    static FIRST_NS: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

    thread_local! {
        static DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    /// `CRATONVM_DBG_DOOR_CYCLES`, read once per process (a diagnostic):
    /// 0 off, 1 on, 2 on with the split segments (`=split`).
    #[inline]
    fn mode() -> u8 {
        static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            if false
            {
                2
            } else {
                u8::from(false)
            }
        })
    }

    /// Is the census on (either form)?
    #[inline]
    pub(crate) fn enabled() -> bool {
        mode() != 0
    }

    /// One timed door call; `None` when off or nested in another timed door.
    pub(crate) struct DoorSpan {
        t0: u64,
        body: Cell<u64>,
        /// `=split`: the tick the current segment started at; 0 when unsplit.
        last: Cell<u64>,
    }

    impl DoorSpan {
        #[inline]
        pub(crate) fn enter() -> Option<Self> {
            if !enabled() {
                return None;
            }
            let outermost = DEPTH.with(|d| {
                let was = d.get();
                d.set(was + 1);
                was == 0
            });
            if !outermost {
                DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
                return None;
            }
            FIRST_NS.get_or_init(std::time::Instant::now);
            let t0 = crate::jit::helpers::mic_prof::now();
            let _ = FIRST_TICK.compare_exchange(0, t0, Relaxed, Relaxed);
            Some(Self {
                t0,
                body: Cell::new(0),
                last: Cell::new(if mode() == 2 { t0 } else { 0 }),
            })
        }

        /// `=split`: close segment `seg` (1-based index into `BODY`) here.
        #[inline]
        pub(crate) fn mark(span: Option<&Self>, seg: usize) {
            let Some(span) = span else {
                return;
            };
            let last = span.last.get();
            if last == 0 {
                return;
            }
            let now = crate::jit::helpers::mic_prof::now();
            BODY[seg].fetch_add(now.wrapping_sub(last), Relaxed);
            span.last.set(now);
        }

        /// The compiled call's own ticks, read around it.
        #[inline]
        pub(crate) fn time_body<R>(span: Option<&Self>, call: impl FnOnce() -> R) -> R {
            let Some(span) = span else {
                return call();
            };
            let t0 = crate::jit::helpers::mic_prof::now();
            let r = call();
            let t1 = crate::jit::helpers::mic_prof::now();
            span.body.set(t1.wrapping_sub(t0));
            let last = span.last.get();
            if last != 0 {
                BODY[2].fetch_add(t0.wrapping_sub(last), Relaxed);
                span.last.set(t1);
            }
            r
        }
    }

    impl Drop for DoorSpan {
        fn drop(&mut self) {
            let t1 = crate::jit::helpers::mic_prof::now();
            CALLS.fetch_add(1, Relaxed);
            DOOR.fetch_add(t1.wrapping_sub(self.t0), Relaxed);
            BODY[0].fetch_add(self.body.get(), Relaxed);
            let last = self.last.get();
            if last != 0 {
                BODY[5].fetch_add(t1.wrapping_sub(last), Relaxed);
            }
            DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }

    /// The `[door-cycles]` exit line; nothing when off or never timed.
    pub(crate) fn report() {
        if !enabled() {
            return;
        }
        let calls = CALLS.load(Relaxed);
        if calls == 0 {
            eprintln!("[door-cycles] calls=0");
            return;
        }
        let door = DOOR.load(Relaxed);
        let body = BODY[0].load(Relaxed);
        // Both clocks read now, from the first timed call: the counter's rate.
        let ticks = crate::jit::helpers::mic_prof::now().wrapping_sub(FIRST_TICK.load(Relaxed));
        let ns = FIRST_NS.get().map_or(0, |t| t.elapsed().as_nanos() as u64);
        // The cost of the four counter reads one timed call adds, on this core.
        let probe = {
            let t0 = crate::jit::helpers::mic_prof::now();
            for _ in 0..1000 {
                std::hint::black_box(crate::jit::helpers::mic_prof::now());
            }
            crate::jit::helpers::mic_prof::now().wrapping_sub(t0) / 1000
        };
        let pct = |part: u64| part as f64 * 100.0 / door.max(1) as f64;
        eprintln!(
            "[door-cycles] calls={calls} door_per_call={:.1} body_per_call={:.1} \
             bookkeeping_per_call={:.1} bookkeeping_share={:.1}% self_cost_per_call={} \
             ticks_per_ns={:.3}",
            door as f64 / calls as f64,
            body as f64 / calls as f64,
            door.saturating_sub(body) as f64 / calls as f64,
            pct(door.saturating_sub(body)),
            probe * 4,
            ticks as f64 / ns.max(1) as f64,
        );
        if mode() == 2 {
            let per = |i: usize| BODY[i].load(Relaxed) as f64 / calls as f64;
            eprintln!(
                "[door-cycles] split pre={:.1} guard_enter={:.1} guard_exit={:.1} \
                 restore_thread={:.1} drain={:.1} (ticks per call; each segment carries \
                 about one counter read)",
                per(1),
                per(2),
                per(3),
                per(4),
                per(5),
            );
        }
    }
}

/// Round 16 wave 17 (lane trapfix): a body whose entry was made not entrant is
/// declined by the doors; a live one is entered.
#[cfg(all(test, target_arch = "x86_64"))]
mod r16w17_not_entrant_door_tests;
#[cfg(test)]
mod elidable_ctor_policy_tests;

#[cfg(test)]
mod supersede_classification_tests;

/// JIT r23 wave 11, lane L2 (`CRATONVM_JIT_SUPERSEDE_EPOCH_ONLY_ON_REPLACE`;
/// page `r23-w11-first-publish-evicts-every-jit-invoke-cache-entry-FIXED-20261023.md`).
#[cfg(test)]
mod r23_w11_supersede_epoch_only_on_replace_tests;
#[cfg(test)]
mod decoded_arg_pin_tests;

#[cfg(test)]
mod r9w3_branch_profile_arm_tests;

#[cfg(test)]
mod r9w3_profile_replay_hook_tests;

#[cfg(test)]
mod r9w3_osr_single_pass_preference_tests;

/// Round 12 wave 1 (lane osr): the optimizing route's half of the
/// `CRATONVM_JIT_OSR_ATHROW=0` kill switch asks this.
#[cfg(test)]
mod r12w1_osr_athrow_scan_tests;

#[cfg(test)]
mod r9w10_call10_osr_despec_stale_tests;

#[cfg(test)]
mod r11w6_sync_door_tests;

#[cfg(test)]
mod r11w7_sync_door_count_tests;

#[cfg(test)]
mod r9w11_osr11_tests;

#[cfg(test)]
mod i2_l7_door_core_tests;

#[cfg(test)]
mod i3_l7_indy_intern_tests;

/// Interpreter round i1 wave 4, lane L7: the compiled-`new` class-init rule,
/// the OSR exit bookkeeping, and the deopt-flag hygiene of the legacy door.
#[cfg(test)]
mod i1w4_l7_tests;

/// Interpreter round i1 wave 5, lane L7: the loader-aware callee door, the
/// lock-free OSR re-entry stamp, the per-VM profile-replay watermark and the
/// debugger stand-down.
#[cfg(test)]
mod i1w5_l7_tests;

#[cfg(test)]
mod r11w8_rbc6_narrowing_tests;

/// MON10-1 (JIT round 19 proposal wave 11, lane mon11,
/// `CRATONVM_JIT_OSR_SYNC_BLOCK_ENTRY`): a loop INSIDE a javac `synchronized`
/// block keeps its header entry; any other covering row still withdraws it.
#[cfg(test)]
mod mon11_sync_block_entry_tests;

/// Round 11 wave 9, tier proposal 26
/// (r11w8-tier-optimizing-osr-route-unreachable-behind-single-pass): the
/// mutator's background arm counts a cached optimizing OSR body as enterable,
/// and only one `try_osr` would actually enter from the cache.
#[cfg(test)]
mod r11w9_osr_optimizing_background_tests;

/// Round 11 wave 10, lane tier.
#[cfg(test)]
mod r11w10_tier_tests;

/// Round 11 wave 11, lane tier: the single-pass OSR route must not pre-empt an
/// enterable optimizing body
/// (r11w8-tier-optimizing-osr-route-unreachable-behind-single-pass).
#[cfg(test)]
mod r11w11_tier_tests;

/// Interpreter round i1 wave 6, lane L7: policy declines reported as declines,
/// the OSR early gates' denials, and the deferred-`new` memo's lock order.
/// Interpreter round i1 wave 9, lane L6: the native-shadow rule that replaced
/// the upgrade door's `Dist` class-name exemption
/// (`docs/feature-designs/real-jdk-mode.md`).
#[cfg(test)]
mod i1w9_l6_native_shadow_rule_tests;

#[cfg(test)]
mod i1w6_l7_tests;

/// Interpreter round i1 wave 7, lane L7: callee compile doors that name the
/// class they compile, the first-call door without a static-owner check, and
/// the optimizing OSR memo's staleness test.
#[cfg(test)]
mod i1w7_l7_tests;

/// Interpreter round i1 wave 8, lane L7: the method-wide OSR offer budget.
#[cfg(test)]
mod i1w8_l7_tests;

/// Interpreter round i1 wave 9, lane L7: background OSR give-ups, the
/// loop-work credit, OSR field-site denials and the optimizing OSR memo's
/// dependency rule.
#[cfg(test)]
mod i1w9_l7_tests;

/// Round 11 wave 13 (lane irexc): the value-equivalent churn bound.
#[cfg(test)]
mod r11w13_irexc_value_equivalent_exit_tests;

/// Round 16 (lane osrdoor): a burst of caught-exception exits hands the loop to
/// the single-pass OSR body.
#[cfg(test)]
mod r16_osrdoor_handler_exit_tests;

/// JIT round 16 wave 13 (lane monitor3): the door's widening for guard exits
/// inside javac's `synchronized` catch-all (`CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK`).
#[cfg(test)]
mod r16w13_monitor3_osr_guard_exits_in_a_sync_block_tests;

/// Tier proposal 27 (round 11 wave 10, lane irexc): the door's table test.
#[cfg(test)]
mod r11w10_irexc_osr_exception_exit_tests;

/// Round 11 wave 15, lane tier.
#[cfg(test)]
mod r11w15_tier_tests;

/// t1 wave 8, lane L1OSR (L1W1-4, `CRATONVM_JIT_OSR_PROFIT_EXIT_SUPERSEDED_OPTIMIZING`):
/// an optimizing OSR artifact is queued for the profit exit only once a newer
/// body replaced it under EVERY header the memo remembered it at.
#[cfg(test)]
mod t1_w8_l1osr_superseded_optimizing_tests;

/// t1 wave 9, lane JITB (`CRATONVM_JIT_OSR_INSTALL_RECHECK`; residual of
/// `t1-V6-osr-memo-recheck-refuses-installed-bodies-after-observer-gain-FIXED-20261027.md`):
/// an object-observer gain logged after the build's dependency check but
/// before the install is judged in flight; one logged after the install is
/// spared by the memo's installed-rule re-check.
#[cfg(test)]
mod t1_w9_jitb_install_recheck_tests;

/// Interpreter round i1 wave 10, lane L5: items 3 and 4 of
/// `osr-optimizing-door-bypasses-osr-admission`.
#[cfg(test)]
mod i10_l5_tests;

#[cfg(test)]
mod i1w11_l4_charge_tests;

/// Round 11 wave 18 (lane `tier`): callee promotion P1-P3
/// (`r11-orch-callee-compiled-bodies-are-never-promoted`).
#[cfg(test)]
mod r11w18_tier_tests;

/// Round 11 wave 19, lane tier: the two counted post-entry OSR arms answer
/// loudly (tier proposal W17-5,
/// r11-tier-osr-post-entry-refusals-replay-loop-iterations).
#[cfg(test)]
mod r11w19_tier_tests;

/// Interpreter round i1 wave 37, lane L2: `CpResolvers::invoke` asks the
/// JVMS §6.5 selection question only for an `invokespecial` operand, found by
/// walking instructions (an operand byte equal to `0xb7` is not one).
#[cfg(test)]
mod i37_l2_invokespecial_operand_tests;

/// Round 15 wave 4 (lane fieldowner): a compiled field site's owner must be
/// the one the interpreter resolves (`jit_field_owner_refusal`), and CH3-1's
/// four field asks share one answer per site (`JitFieldSiteMemo`).
#[cfg(test)]
mod r15w4_fieldowner_tests;

/// Interpreter round i1 wave 12, lane L4: the OSR door's half of leaving a
/// compiled loop when the interpreter is required.
#[cfg(test)]
mod i12_l4_osr_loop_exit_tests;

#[cfg(test)]
mod i13_l2_substituted_owner_tests;

/// Interpreter round i1 wave 14, lane L3: the cross-activation OSR budget and
/// the optimizing OSR memo expire on THIS VM's code-state epoch
/// (`JitRealm::code_state_epoch`), never on another VM's redefinition or
/// flush (`interpreter-L3-per-vm-state-still-keyed-by-the-process-install-epoch-FIXED`).
#[cfg(test)]
mod i1w14_l3_tests;

/// Interpreter round i1 wave 15, lane L2
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`): the optimizing OSR
/// door's side of a back-edge poll mode exit. Only that door compiles one;
/// its planless transfer takes it without an admitted bci while a guard at an
/// unadmitted bci is still refused; and such a body is counted for the
/// loop-exit pause.
#[cfg(test)]
mod i15_l2_ir_poll_exit_tests;

/// Interpreter round i1 wave 16, lane L5: the per-frame OSR poll point
/// (`Frame::osr_poll_at`, `interpreter::osr_next_poll`) that spaces out the
/// out-of-line OSR calls of a frame whose loop header is not due.
#[cfg(test)]
mod i1w16_l5_tests;



/// Interpreter round i1 wave 20, lane L5: the OSR loop budget is keyed by the
/// method's memoised fingerprint and consulted through a lock-free filter
/// (`i1-L7-proposal-per-method-backedge-counters`, "Progress (wave 20)").
#[cfg(test)]
mod i1w20_l5_tests;

/// Interpreter round i1 wave 21, lane L5: the profile-replay sweep latch is
/// per VM and stamped with the VM's own class-store epoch
/// (`interpreter-L2-class-epoch-readers-outside-the-site-caches-stay-process-wide-FIXED-20260925.md`).
#[cfg(test)]
mod w21_l5_profile_replay_latch_tests;

/// gen r5w1/oom5: the OSR entry's parking of the interpreter frame's
/// reference locals (`CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`).
#[cfg(test)]
mod r5w1_oom5_osr_park_tests;

/// Interpreter round i1 wave 23, lane L2: a stashed frame whose compiled code
/// took a lock (`MonitorInfo::relock == false`) and that the sink REFUSES must
/// not leave the lock held for the re-run from entry, and one it RESUMES must
/// keep it for the resumed frame. Page
/// `docs/feature-designs/real-jdk-mode.md`.
#[cfg(test)]
mod i23_l2_refused_frame_lock_tests;

/// Interpreter round i1 wave 25 follow-up, lane L6c: the OSR door's
/// "frame predates a redefinition of a class the body copied" decision
/// (`try_osr`, `osr_body_may_fold_code_the_frame_predates`).
#[cfg(test)]
mod i25_l6c_osr_frame_predates_redefinition_tests;

#[cfg(test)]
mod w26_l2_chain_identity_tests;

/// Round 12 wave 7 (lane replay4): which door re-runs are refused
/// (`r12w3-replay-jit-bridge-doors-rerun-without-the-replay-rule`).
#[cfg(test)]
mod r12w7_replay4_door_refusal_tests;

/// Interpreter round i1 wave 39, lane L2: the OSR entry's generation rule
/// (`osr_body_generation_differs_from_frame`,
/// `i38-L2-an-obsolete-activation-enters-an-osr-body-of-the-new-bytecode`).
#[cfg(test)]
mod i39_l2_osr_generation_tests;



/// Interpreter round i1 wave 43, lane L2
/// (`interpreter-L2-proposal-compile-inputs-are-read-inside-the-compile-witness-FIXED-20261007`):
/// every compile door builds its request from a `CompileInputs`, whose
/// bytecode was read (or adopted) inside the compile's epoch witness. Text
/// pins for the doors, which need a loaded class, a backend and a racing
/// redefinition to exercise.
#[cfg(test)]
mod i43_l2_compile_inputs_tests;

/// JIT round 16 (lane throwpath, TP-1): an inherited synchronized method's
/// self-locking body is found under its DECLARING class's key.
#[cfg(test)]
mod r16_throwpath_declaring_body_tests;

/// JIT round 16 wave 13 (lane mega, MG16-3): a package-private override is a
/// guarded splice target exactly when JVMS §5.4.6 selection agrees, and only
/// under `CRATONVM_JIT_GUARDED_SPLICE_PACKAGE_PRIVATE=1`.


/// JIT round 16 wave 14 (lane throw4, TP16-20): the late-bind recompile's
/// bookkeeping -- which refusals are remembered, that a served callee wakes
/// its waiters once, and that the switch off remembers nothing.


/// JIT round 16 wave 17 (lane mh7; MH16-8 at compile time): the per-VM memo of
/// constant handles the dispatch helpers admitted, which the compile doors read
/// (`CpResolvers::mh_constant`).
#[cfg(test)]
mod r16w17_mh_constant_memo_tests;

/// JIT round 17 wave 1 (lane mhconst): the recompile waiters (MH7-1), the
/// field-keyed rows (MH7-2) and the descriptor loader check of the constant
/// handle compile door (`CpResolvers::mh_constant`).


/// JIT round 17 wave 2 (lane mh2): the immutability check of admitted handles
/// (MH7-3), the interpreter fill's switch (MHCONST17-1) and the helper path's
/// loader question.


/// JIT round 17 wave 2 (lane mh3): the dependency of a body compiled from a
/// constant handle on that handle
/// (`r17-mh-constant-compiled-body-not-retired-on-handle-rewrite-FIXED-20261005.md`)
/// and the non-public member switch (MHCONST17-3).




/// Record that the dispatch helpers' constant bind admitted the handle at
/// address `handle`, in collection epoch `epoch`, for a call site of
/// descriptor `site`, as an exact direct STATIC handle of `(class, name,
/// desc)` (`class_id` the member's class). Called by the cold fill
/// (`vm/src/jit/helpers.rs`, `mh_constant_bind_fill`) on the mutator; read by
/// [`CpResolvers::mh_constant`] (JIT round 16 wave 17, lane mh7).
#[allow(clippy::too_many_arguments)]
// JIT round 17 wave 3 (lane mh4): the fills call
// [`note_mh_constant_target_kind`] now; the static form stays for the tests.
// Kept below the production source: the source-text tests cut at the first
// `#[cfg(test)]`.
#[cfg(test)]
pub(crate) fn note_mh_constant_target(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    class: &str,
    name: &str,
    desc: &str,
    class_id: u32,
) {
    note_mh_constant_target_kind(
        shared,
        handle,
        epoch,
        site,
        (class, name, desc),
        class_id,
        3,
    );
}

/// JIT round 18 wave 9 (lane eagercold): the optimizing door's eager
/// static-callee compile asks the callee's invocation counter first
/// (`CRATONVM_JIT_EAGER_STATIC_CALLEE_NEEDS_RUN`), and a constructor splice
/// refused only for an uninitialized class can wait for the late-bind
/// recompile (`CRATONVM_JIT_LATE_BIND_UNINIT_CTOR`). Page
/// `r18-eager-static-callee-chain-compiles-uninitialized-constructors-blind-FIXED-20261006.md`.


#[cfg(test)]
mod al16_osr_rebuild_first_call_tests;



#[cfg(test)]
mod nio21w6_never_run_static_exit_tests;



/// JIT round 19 wave 2, lane eager2: EAGER19-1/2/3/5/6 bookkeeping, each
/// behind its own opt-in switch.


/// JIT round 19 wave 3, lane eager3: the class-initialized guard's admission
/// (EAGER2-3), the uninitialized-`new` site lookup the deopt service asks
/// (EAGER2-4 and the EAGER19-1 site-trap fix), and the eager-chain census.


/// JIT round 19 wave 4, lane syncdoor4 (SYNCDOOR4-1,
/// `CRATONVM_JIT_SELF_LOCK_EAGER_BIND`): source witnesses. Reaching the doors
/// needs a live VM, a compiled caller and a synchronized callee; the
/// end-to-end engagement is the orchestrator's `CRATONVM_DBG_JITC` run
/// (`self-lock-eager-bind COMPILED`).


#[cfg(test)]
mod nio21w18_osr_optimizing_jit_compiled_tests;

/// gci i14/od (oomjit10 item 1, the door promise on the other doors). Kept
/// after every door: the text pins below find each door by its first
/// occurrence.
#[cfg(test)]
mod gci_i14od_door_promise_tests;

/// JIT round 20 wave 19, lane yStash (STASH20-3).
#[cfg(test)]
mod r20w19_ystash_c1_duplicate_tests;

#[cfg(test)]
mod i92_deferred_new_release_stamp_tests;

#[cfg(test)]
mod i93_callee_door_defers_to_worker_tests;

#[cfg(test)]
mod i101_l2_resolver_miss_tests;

/// Interpreter round i6 wave 115, lane L6 (page
/// `i114-L6-debug-build-asserts-on-every-admitted-virtual-thread-compiled-entry`):
/// the one check both interpreter->compiled entries pass lets a JDK
/// continuation's thread through in every build (the doors admit it on
/// purpose since wave 92), marks it once, and still refuses a bound
/// (VM-scheduled) virtual thread in a debug build.
#[cfg(test)]
mod i115_l6_jdk_continuation_compiled_entry_tests;

#[cfg(test)]
mod i115_l2_retier_cause_tests;
/// JIT round 21 wave 1 (lane indy, INDY21-3, `CRATONVM_JIT_STATIC_INIT_RACE_SERVE`):
/// the numbered static-init log a compile door asks "which holders finished
/// initializing while I compiled?".
#[cfg(test)]
mod r21_indy_static_init_log_tests;





/// JIT round 21 wave 16 (lane perf-cf-lean): [`entry_null_guard_field`]'s
/// abstract execution. At the end of the file: the source-pin tests above cut
/// the production text at the first `#[cfg(test)]`.
#[cfg(test)]
mod r21w16_entry_null_guard_tests;

/// JIT round 22 wave 1, lane runtime: INDY21-4 (`CRATONVM_JIT_INDY_LEAF_DIRECT_CALL`)
/// and INDY21-5 (the retarget recompile census). At the end of the file, as the
/// module above says.
#[cfg(test)]
mod r22_runtime_indy_tests;



/// JIT round 22 wave 2, lane runtime (flip review R1 / R3): both reflective
/// links ask the hidden-frames latch per call, before any target work, and the
/// stand-in synthesis is opt-in.
#[cfg(test)]
mod r22w2_runtime_reflect_link_hidden_latch_tests;

/// JIT round 22 wave 8, lane runtime (page
/// `r22-w7-code-cache-sweep-phases-bistable-FIXED-20261020.md`): the sweep driver
/// drains the retirement queue inside its own blocked window, so a mutator
/// inside an OSR loop vouches for itself through its stack instead of pinning
/// every withdrawn body until the loop returns, and re-reports the pressure.
/// The end-to-end pin is `vm/tests/code_cache_sweep_phases.rs`, unchanged.
#[cfg(test)]
mod r22w8_runtime_sweep_self_blocked_drain_tests;

/// JIT round 23 wave 1, lane L1 (THROWS22-1, `CRATONVM_JIT_OSR_PROFIT_EXIT`):
/// the OSR door's four halves of the profit exit. Text pins: each half needs a
/// live frame, a published single-pass OSR body with forced polls and an
/// accepted optimizing body (the jit crate's
/// `r23_l1_an_armed_single_pass_osr_body_has_its_polls_forced_once` drives the
/// patch itself; the probe `C:\craton\n23p\w1L1\` drives the whole route).
#[cfg(test)]
mod r23_w1_l1_osr_profit_exit_tests;



#[cfg(test)]
mod frozen_lambda_capture_slot_tests;

#[cfg(test)]
mod r23_w3_osr_predicate_exit_charge_tests;

/// t1 wave 2, lane jit2 (B5 of `jit-proposals/jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`,
/// `CRATONVM_JIT_OSR_LATE_STATIC_CELL`): text pins on the single-pass OSR door's
/// late-bind cell. The cell's run-time behaviour is pinned in `cratonvm_jit`
/// (`t1_jit2_osr_late_cell_tests`); these pin the door's half.
#[cfg(test)]
mod t1_jit2_osr_late_cell_door_tests;

/// t1 wave 3, lane X: a class unload forgets the optimizing OSR memo rows of
/// its classes (`JitBridgeState::forget_osr_optimizing_of_unloaded`), which
/// `JitCache::invalidate_unloaded_classes` never reaches.
#[cfg(test)]
mod t1x_osr_memo_unload_tests;

#[cfg(test)]
mod jr21_smallfix_osr_static_init_tests;

#[cfg(test)]
mod tc21b_osr_static_init_trigger_tests;

#[cfg(test)]
mod jr21_indylate_late_link_trap_tests;

/// JIT round 21 (lane tl2, `CRATONVM_JIT_INLINE_SCREEN_RETIRED_MASK`): the
/// selected-method and receiver-chain screens consult the dispatch walks'
/// retired-row mask for the declaring class's registration, and only there.
#[cfg(test)]
mod jr21_tl2_retired_mask_screen_tests;

/// JIT round 21 lane indylate2
/// (`jr21-indylate2-indy-target-refused-for-a-child-loader-caller-20261014.md`):
/// an `invokedynamic` target whose class name the caller's loader does not
/// answer is admitted by the id the handle names, under
/// `CRATONVM_JIT_INDY_ADMIT_BY_ID`, and asked in its own class context.
#[cfg(test)]
mod jr21_indylate2_indy_admit_by_id_tests;

/// JIT round 21, lane tcshape (`CRATONVM_JIT_DEFERRED_FIELD_RECOMPILE`): the
/// deferred-field wait table's one-shot rule.
#[cfg(test)]
mod r21_tcshape_deferred_field_tests;

/// 2026-10-26 (`jr21-typecheck-lane-perf-rows` retirement, `TcuLateOsr`): an
/// optimizing OSR body's exit that the memo's value-equivalent bound counts is
/// taken as committed by the door, not charged against the frame's per-header
/// budget too. Charged twice, the fourth exit withdrew the body as it spent the
/// activation's fifth offer, and a never-returning loop whose pruned branch
/// started to be taken stayed interpreted for life.
#[cfg(test)]
mod osr_value_equivalent_exit_commit_tests;

#[cfg(test)]
mod supersede_fall_through_reading_tests;

/// JIT round j27 (lane MHINDY): the line a filter species site's two pcs are
/// compared by ([`mh_filter_line_at`]).
#[cfg(test)]
mod j27_mh_filter_line_tests;

/// JIT round j27 (lane JNINAT, REFLECT2-21-3): the constant-pool screen's
/// `native-shadow` refusal names the kind and the call-site intrinsic, and
/// keeps the reason token `jit-native-shadow-rank.py` matches.
#[cfg(test)]
mod j27_native_shadow_refusal_text_tests;


#[cfg(test)]
mod j27_callee_door_probe_only_tests;

#[cfg(test)]
mod j27_spliceffm_refusal_source_tests;

/// j27 wave 5 (lane INVSP, page
/// `j27-super-special-splice-walks-past-an-abstract-redeclaration-FIXED-20261029.md`,
/// "Still open"): every `invokespecial` inside a spliced body, in both tiers,
/// is bound through [`classify_invoke_site`] when the body is scanned (the
/// `special_sites` loop of `resolve_inline_site_from`), before it becomes a
/// kind-1 `invoke_targets` row the nested splice resolves. These pin the
/// JVMS 6.5 answers that scan gives for the shapes the page names: an abstract
/// re-declaration refuses the enclosing body, and a constant-pool class above
/// the direct superclass binds the selected owner, so the nested resolution
/// walks from it.
#[cfg(test)]
mod j27_invsp_nested_special_selection_tests;



/// j27 wave 7 (lane T1L2, L2W4B-1 review): an unload drops the condy-handle
/// member rows of its holders, and only theirs.
#[cfg(test)]
mod j27_t1l2_condy_member_unload_tests;

/// j27 wave 6 (lane T1LAM2; item 1 of
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`): the compile doors answer a
/// bound STATIC handle (`invoke_kind 6`) only under its own switch (default ON
/// since 2026-10-28, j27 WARNING flip; =0 restores), at
/// the site type the bound handle has.
#[cfg(test)]
mod j27_t1lam2_mh_bound_static_door_tests;

/// INDY21 closure (2026-11-02, `docs/feature-designs/native-dispatch-architecture.md`):
/// the own-`<clinit>` fast path of the `getstatic` helper
/// (`CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST`).
#[cfg(test)]
mod indy21_closure_tests;





mod jit_verdicts;
#[allow(unused_imports)]
pub use jit_verdicts::*;
#[allow(unused_imports)]
use jit_verdicts::*;
mod osr_continuation;
#[allow(unused_imports)]
pub use osr_continuation::*;
#[allow(unused_imports)]
use osr_continuation::*;
mod compile_osr_body;
#[allow(unused_imports)]
pub use compile_osr_body::*;
#[allow(unused_imports)]
use compile_osr_body::*;
mod osr_exception_exit;
#[allow(unused_imports)]
pub use osr_exception_exit::*;
#[allow(unused_imports)]
use osr_exception_exit::*;
mod try_osr;
#[allow(unused_imports)]
pub use try_osr::*;
#[allow(unused_imports)]
use try_osr::*;
mod native_shadow_arms;
#[allow(unused_imports)]
pub use native_shadow_arms::*;
#[allow(unused_imports)]
use native_shadow_arms::*;
mod compile_optimizing_artifact;
#[allow(unused_imports)]
pub use compile_optimizing_artifact::*;
#[allow(unused_imports)]
use compile_optimizing_artifact::*;
mod never_run_static_exit_pcs;
#[allow(unused_imports)]
pub use never_run_static_exit_pcs::*;
#[allow(unused_imports)]
use never_run_static_exit_pcs::*;
mod try_jit_compile_callee_slow;
#[allow(unused_imports)]
pub use try_jit_compile_callee_slow::*;
#[allow(unused_imports)]
use try_jit_compile_callee_slow::*;
mod byte_array_field_declarer_admitted;
#[allow(unused_imports)]
pub use byte_array_field_declarer_admitted::*;
#[allow(unused_imports)]
use byte_array_field_declarer_admitted::*;
mod folded_handle_member_row;
#[allow(unused_imports)]
pub use folded_handle_member_row::*;
#[allow(unused_imports)]
use folded_handle_member_row::*;
mod callee_never_taken_rows;
#[allow(unused_imports)]
pub use callee_never_taken_rows::*;
#[allow(unused_imports)]
use callee_never_taken_rows::*;
mod mint_ldc_slots_at_offer;
#[allow(unused_imports)]
pub use mint_ldc_slots_at_offer::*;
#[allow(unused_imports)]
use mint_ldc_slots_at_offer::*;
