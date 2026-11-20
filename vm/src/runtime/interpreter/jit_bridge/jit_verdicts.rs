#[allow(unused_imports)]
use super::*;


/// This VM's compile verdicts: the bail list, refusal reasons, OSR entry
/// rejects and the recursion-cycle sets
/// (`cratonvm_jit::JitVerdictRegistry`, owned by the tiered manager).
///
/// Every VM door reads and records through this and hands it to the compile
/// (`CpResolvers::with_compile_request`, `compile_gate::admit`), never through
/// the jit crate's free functions, which are the no-VM registry: a bail
/// recorded in one registry and asked of another would re-run the pipeline on
/// every attempt (interpreter round i1 wave 11; they were one process-global
/// store, so one VM's verdict answered for another VM's method of the same
/// `ClassId` and names).
#[inline]
pub(crate) fn jit_verdicts(shared: &SharedVm) -> &cratonvm_jit::JitVerdictRegistry {
    shared.jit.tiered_manager.verdicts()
}

/// This VM's compile realm: [`jit_verdicts`] and its deferred-`new` retry memo
/// (`JitBridgeState::deferred_new_retries`), the two registries
/// `CompileRequest::new` requires (interpreter round i1 wave 18, lane L5).
#[inline]
pub(crate) fn jit_compile_realm(shared: &SharedVm) -> cratonvm_jit::CompileRealm<'_> {
    cratonvm_jit::CompileRealm {
        verdicts: jit_verdicts(shared),
        deferred_new_retries: &shared.jit.bridge.deferred_new_retries,
        // t1 wave 3 (lane Y): this VM's `--real-jdk-report` sinks.
        jdk_only_sinks: shared.jit.jit_cache.jdk_only_sinks(),
    }
}

/// An optimizing OSR body a late bind made the memo forget; see
/// [`JitBridgeState::late_bind_osr_superseded`].
pub(in crate::runtime::interpreter::jit_bridge) struct SupersededOsrBody {
    pub(in crate::runtime::interpreter::jit_bridge) class_id: u32,
    pub(in crate::runtime::interpreter::jit_bridge) method_name: String,
    pub(in crate::runtime::interpreter::jit_bridge) descriptor: String,
    /// t1 wave 9, lane JITB (`t1-L1OSR-superseded-osr-rows-hold-bodies-...`):
    /// held WEAKLY, as `JitCache::exit_poll_bodies` holds its bodies. The row
    /// exists only to arm a frame still running the body, and such a frame
    /// holds its own `Arc` (the OSR door's `compiled` local) for the whole
    /// activation; a row whose body no longer upgrades names a body no frame
    /// runs, and is dropped. A strong row (wave 7 lane NBA's `RetainedCode`)
    /// kept the body and every callee it pins alive until a covering rebuild
    /// or a class unload, which a refused rebuild or a never-covered header
    /// never brings. Upgraded bodies are wrapped in a `RetainedCode` before
    /// use, so a release that turns out to be the last one still goes
    /// through the retirement queue.
    pub(in crate::runtime::interpreter::jit_bridge) body: std::sync::Weak<cratonvm_jit::CompiledMethod>,
}

impl SupersededOsrBody {
    pub(in crate::runtime::interpreter::jit_bridge) fn new(
        class_id: ClassId,
        names: (&str, &str, &str),
        body: &Arc<cratonvm_jit::CompiledMethod>,
    ) -> Self {
        Self {
            class_id: class_id.as_u32(),
            method_name: names.1.to_string(),
            descriptor: names.2.to_string(),
            body: Arc::downgrade(body),
        }
    }

    /// Could a frame still be running this row's body?
    pub(in crate::runtime::interpreter::jit_bridge) fn live(&self) -> bool {
        self.body.strong_count() > 0
    }

    /// Is this row's body `body`? Pointer identity; a dead row's allocation
    /// stays reserved while its `Weak` exists, so it never aliases a new body.
    #[cfg(test)]
    pub(in crate::runtime::interpreter::jit_bridge) fn names_body(&self, body: &Arc<cratonvm_jit::CompiledMethod>) -> bool {
        std::ptr::eq(self.body.as_ptr(), Arc::as_ptr(body))
    }
}

/// Push `row` onto [`JitBridgeState::late_bind_osr_superseded`] unless it
/// already holds a row for the same body. Rows are examined only when their
/// own method rebuilds, so a full list first drops the rows whose body no
/// frame runs any more (t1 wave 9, lane JITB); still bounded by
/// [`LATE_BIND_CALLEE_CAP`]. `false` when the row was not pushed.
pub(in crate::runtime::interpreter::jit_bridge) fn push_superseded_osr_row(rows: &mut Vec<SupersededOsrBody>, row: SupersededOsrBody) -> bool {
    if !row.live()
        || rows
            .iter()
            .any(|known| std::sync::Weak::ptr_eq(&known.body, &row.body))
    {
        return false;
    }
    if rows.len() >= LATE_BIND_CALLEE_CAP {
        rows.retain(SupersededOsrBody::live);
    }
    if rows.len() >= LATE_BIND_CALLEE_CAP {
        return false;
    }
    rows.push(row);
    true
}

/// The JIT bridge's own memos, one set per VM (`SharedVm.jit.bridge`).
///
/// Every table here used to be a process-global static keyed by names or by a
/// `ClassId` — and a `ClassId` is unique only within one VM, so a second VM in
/// the same process read the first one's verdicts: its negative callee
/// verdicts withheld compiles (until a 1-in-4096 re-probe), its accepted
/// optimizing OSR artifacts — code built against another heap — could be
/// handed out for a colliding key, and its tick counters paced the other VM's
/// sweeps. `AGENTS.md`: no process globals for per-VM state.
#[derive(Default)]
pub struct JitBridgeState {
    /// [`try_jit_compile_callee`]'s negative cache; see
    /// [`CALLEE_NEG_CACHE_SLOTS`]. Allocated on the first by-name compile
    /// request, so a VM that never reaches that door pays nothing for it.
    pub(in crate::runtime::interpreter::jit_bridge) callee_neg_cache: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>,
    /// CONF21-1: methods whose compile the tiered manager admitted, String
    /// constants not yet resolved; see [`resolve_admitted_compile_strings`].
    pub(in crate::runtime::interpreter::jit_bridge) admitted_compile_strings: Arc<AdmittedCompileStrings>,
    /// Optimizing OSR artifacts the OSR door refused; see
    /// [`osr_optimizing_already_refused`].
    /// The value is this VM's `JitCache::redefine_epoch` when the refusal was
    /// recorded: a redefinition re-opens it.
    pub(in crate::runtime::interpreter::jit_bridge) osr_optimizing_refused: std::sync::Mutex<std::collections::HashMap<(u32, u64, usize), u32>>,
    /// Optimizing OSR artifacts the OSR door accepted, with the pins of the
    /// callees their code CALLs; see [`osr_optimizing_cached`].
    pub(in crate::runtime::interpreter::jit_bridge) osr_optimizing_accepted:
        std::sync::Mutex<std::collections::HashMap<(u32, u64, usize), AcceptedOsrOptimizing>>,
    /// The VM code-state epoch (`JitRealm::code_state_epoch`) at which
    /// [`remember_osr_optimizing_artifact`] last purged
    /// `osr_optimizing_accepted` of stale entries.
    pub(in crate::runtime::interpreter::jit_bridge) osr_optimizing_purged_code_state_epoch: std::sync::atomic::AtomicU64,
    /// Tick source for [`maybe_drain_jit_entry_counters`].
    pub(in crate::runtime::interpreter::jit_bridge) entry_counter_drain_ticks: std::sync::atomic::AtomicU64,
    /// This VM's class-store definition epoch (`StoreEpochs`)
    /// [`resweep_held_deferred_new_retries`] last ran at, plus one (0 = never
    /// ran).
    pub(in crate::runtime::interpreter::jit_bridge) last_deferred_new_sweep_epoch: std::sync::atomic::AtomicU64,
    /// This VM's class-store definition epoch (`StoreEpochs`) the
    /// profile-replay hook ([`seed_profile_replay_after_class_definition`])
    /// last swept this VM at, plus one; 0 = never, or "the last sweep could
    /// not take the class manager and must be retried". Was one process
    /// static stamped with the process-wide epoch, so any VM's definition
    /// re-walked every VM (interpreter round i1 wave 21, lane L5).
    pub(in crate::runtime::interpreter::jit_bridge) last_profile_replay_sweep_epoch: std::sync::atomic::AtomicU64,
    /// This VM's deferred-`new` retry memo, handed to its compiles through
    /// `CompileRequest::deferred_new_retries` and consumed by
    /// [`take_deferred_new_retry_if_resolvable`] (interpreter round i1 wave 10:
    /// it was the jit crate's one process-global map, so another VM's sweep
    /// could spend this VM's one-shot retry and ask this VM's class manager
    /// about the other VM's site ids).
    pub(crate) deferred_new_retries: cratonvm_jit::DeferredNewRetries,
    /// Interpreter round i5 wave 100, lane L2
    /// (`CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER`, default OFF): the
    /// single-pass bodies an optimizing task fell through to, keyed by the
    /// body's compile id, valued by the method to recompile once one of the
    /// body's deferred field sites has linked
    /// ([`arm_deferred_field_site_retier`], taken by
    /// `jit::helpers::deferred_field_site_retier_take`). Bounded by
    /// [`DEFERRED_FIELD_RETIER_CAP`]; touched only on cold paths.
    ///
    /// Interpreter round i5 wave 102, lane L2: the second half of the value is
    /// the entry bci of an armed OSR body
    /// ([`arm_osr_deferred_field_site_retier`],
    /// `CRATONVM_JIT_OSR_DEFERRED_FIELD_SITE_RETIER`), `None` for a
    /// method-entry body.
    pub(crate) deferred_field_retier: std::sync::Mutex<
        std::collections::HashMap<u32, (crate::jit::tiered::MethodKey, Option<u32>)>,
    >,
    /// A 256-bit summary of `deferred_field_retier`'s keys (bit `compile id &
    /// 255`), rewritten under its lock, so a linked deferred site of a body
    /// that was never armed pays loads only. All zero while the switch is off.
    pub(crate) deferred_field_retier_filter: [std::sync::atomic::AtomicU64; 4],
    /// Interned `invokedynamic` bridge sites; see
    /// [`interned_jit_indy_bridge_site`].
    pub(in crate::runtime::interpreter::jit_bridge) indy_bridge_sites: std::sync::Mutex<std::collections::HashMap<IndyBridgeSiteKey, usize>>,
    /// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): interned concat
    /// bridge sites of fused `StringBuilder` chains, keyed by their recipe;
    /// see [`interned_jit_sb_chain_concat_site`]. `0` = the recipe was
    /// refused.
    pub(in crate::runtime::interpreter::jit_bridge) sb_chain_concat_sites: std::sync::Mutex<std::collections::HashMap<SbChainSiteKey, usize>>,
    /// Deoptimizations recorded in this VM, mirrored lock-free for
    /// [`osr_reentry_stamp`], which read `deopt_log.lock().total_deopts()` —
    /// a VM-wide mutex — on every OSR entry, i.e. once per caught exception on
    /// a `try`/`catch` loop. Bumped by `DeoptimizationController::deoptimize`
    /// (the one production writer of the log) BEFORE it records, so a stamp
    /// read after a deopt started always differs from one read before it.
    pub(in crate::runtime::interpreter::jit_bridge) deopt_events: std::sync::atomic::AtomicU64,
    /// Round 15 wave 3 (lane chain3, CH2W3-1): this VM's inlined-chain resume
    /// census, written and printed by `deopt_resume` (`InlinedChainCensus`).
    pub(crate) inlined_chain_census: super::deopt_resume::InlinedChainCensus,
    /// JIT round 21 lane fuzzvm (FUZZTOOL21-1): this VM's forced back-edge
    /// poll arrivals under the stress switches, granted / refused by reason,
    /// written by the safepoint slow path and printed at exit
    /// (`crate::jit::helpers::report_poll_stress_census_at_exit`).
    pub(crate) poll_stress_census: crate::jit::helpers::PollStressCensus,
    /// The first `ClassId` slot the profile-replay hook
    /// ([`seed_profile_replay_after_class_definition`]) has not looked at yet.
    ///
    /// Why a watermark rather than resolving every waiting name at every
    /// definition (`profile_store::seed_pending_replay`): that is O(waiting
    /// class names) resolver calls per class definition, and a real replay
    /// names hundreds to thousands of classes against an application that
    /// defines thousands -- millions of lookups during start-up, which is
    /// exactly the phase replay exists to shorten. `ClassId`s are allocated
    /// monotonically and never reused (`ClassStore::next_id`), so the classes
    /// defined since the last visit are the slots from the watermark to
    /// `slot_count()`, and each defined class costs one by-name probe of the
    /// pending map over the life of the VM.
    ///
    /// Was a process-global `Mutex<Vec<(Weak<SharedVm>, u32)>>` searched by
    /// VM address on every visit (interpreter round i1 wave 5, L7:
    /// `i1-L7-process-global-jit-bridge-state-not-vm-scoped`).
    pub(in crate::runtime::interpreter::jit_bridge) profile_replay_watermark: std::sync::atomic::AtomicU32,
    /// Loop headers whose OSR offers were all refused in whole activations,
    /// keyed by [`osr_loop_key`]; see [`osr_loop_offer_allowed`].
    pub(in crate::runtime::interpreter::jit_bridge) osr_refused_loops:
        std::sync::Mutex<std::collections::HashMap<(u32, u64, usize), OsrRefusedLoop>>,
    /// `osr_refused_loops.len()`, written under its lock, so a VM that never
    /// refused a whole activation's offers pays one load per consult.
    pub(crate) osr_refused_loop_count: std::sync::atomic::AtomicUsize,
    /// The origin of this VM's drain clock ([`JitBridgeState::drain_clock_ms`]),
    /// set on first use.
    pub(in crate::runtime::interpreter::jit_bridge) drain_clock_origin: std::sync::OnceLock<std::time::Instant>,
    /// [`JitBridgeState::drain_clock_ms`] when [`drain_jit_entry_counters`] last
    /// ran, plus one (0 = never). Read by the OSR-entry drain trigger
    /// ([`maybe_drain_jit_entry_counters_at_osr_entry`]).
    pub(in crate::runtime::interpreter::jit_bridge) last_entry_counter_drain_ms: std::sync::atomic::AtomicU64,
    /// Tick source that strides the OSR-entry drain trigger's clock read.
    pub(in crate::runtime::interpreter::jit_bridge) osr_entry_drain_ticks: std::sync::atomic::AtomicU64,
    /// A 256-bit summary of `osr_refused_loops`' keys ([`osr_loop_filter_bit`]),
    /// rewritten under its lock whenever the map changes
    /// ([`publish_osr_refused_loops`]). A consult or an entry for a header
    /// whose bit is clear has no record and skips the lock; before interpreter
    /// round i1 wave 20 (lane L5) one refused loop anywhere in the VM sent
    /// every loop's first offer of every activation through it.
    pub(in crate::runtime::interpreter::jit_bridge) osr_refused_loop_filter: [std::sync::atomic::AtomicU64; 4],
    /// OSR bodies of this VM running right now that can leave through a
    /// back-edge poll — single-pass bodies entered with a plan and, since
    /// wave 15 (lane L2), optimizing bodies with a poll mode exit — counted by
    /// [`OsrBodyRunning`] around the entry in [`try_osr`]. Read by
    /// `request_compiled_loop_exits`, which takes a pause only when a body
    /// could be leaving through its back-edge poll (interpreter round i1
    /// wave 12, lane L4).
    pub(in crate::runtime::interpreter::jit_bridge) osr_bodies_running: std::sync::atomic::AtomicUsize,
    /// Methods the upgrade door ([`try_jit_upgrade_with_gate`]) refused
    /// because their bytecode calls a native-shadowed target
    /// (`jit_method_calls_native_shadowed`), keyed by `(class id, method
    /// name, descriptor)`, valued by this VM's `JitCache::redefine_epoch` at
    /// the scan: a redefinition replaces the bytecode the scan judged
    /// (interpreter round i1 wave 38, lane L2). The door used to record the
    /// verdict in the backend bail list, which every door honours, so it also
    /// closed both OSR routes -- which never ask this policy -- for a method
    /// whose loop they compile.
    pub(in crate::runtime::interpreter::jit_bridge) upgrade_native_shadow_callers:
        std::sync::Mutex<std::collections::HashMap<(u32, Arc<str>, Arc<str>), u32>>,
    /// The eager first-call door's constant-pool resolver misses per method,
    /// keyed by `(class id, method name, descriptor)`
    /// ([`JitBridgeState::eager_resolver_miss_may_retry`]; interpreter round
    /// i1 wave 39, lane L2, item 3 of
    /// `i38-L2-compile-door-review-items-left-open`). Touched only on a miss.
    pub(in crate::runtime::interpreter::jit_bridge) eager_resolver_misses:
        std::sync::Mutex<std::collections::HashMap<(u32, Arc<str>, Arc<str>), u8>>,
    /// The optimizing OSR route's declined compiles whose recorded refusal is
    /// a constant-pool resolver miss, per method (the method-wide key of
    /// [`osr_optimizing_refusal_key`]): [`JitBridgeState::osr_optimizing_resolver_miss_may_retry`],
    /// under `CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY` (interpreter
    /// round i5 wave 101, lane L2; the narrow slice of
    /// `i39-L2-proposal-a-compile-request-returns-why-it-produced-no-body`).
    /// Touched only on such a miss.
    pub(in crate::runtime::interpreter::jit_bridge) osr_optimizing_resolver_misses: std::sync::Mutex<std::collections::HashMap<(u32, u64), u8>>,
    /// The first-call cells of this VM's compiled call sites the compile door
    /// found unresolved, keyed by `(class id, constant-pool index, owner class
    /// id)` ([`first_call_cell_for`]; interpreter round i1 wave 50, lane L1).
    /// Boxed, never removed while the VM lives: compiled code bakes a cell's
    /// address (`cratonvm_jit::JitFirstCallCell`).
    pub(in crate::runtime::interpreter::jit_bridge) first_call_cells: std::sync::Mutex<
        std::collections::HashMap<(u32, u16, u32), Box<cratonvm_jit::JitFirstCallCell>>,
    >,
    /// `CRATONVM_JIT_FIRST_CALL_RESOLVE`, read once per VM
    /// ([`first_call_resolve_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) first_call_switch: std::sync::OnceLock<bool>,
    /// Interpreter round i4 wave 92, lane L7: link and initialization
    /// completions in this VM ([`note_new_site_settle_event`]), the events
    /// besides a definition and a resolution-epoch bump that can turn a held
    /// deferred-`new` site resolvable (`jit_new_target_init_walk`). Bumped
    /// under the class-manager write guard that publishes the state, so the
    /// guard's own release sweep sees it.
    pub(in crate::runtime::interpreter::jit_bridge) deferred_new_settle_events: std::sync::atomic::AtomicU64,
    /// The release stamp ([`deferred_new_release_stamp`]) the last
    /// write-guard release sweep of this VM ran at, plus one (0 = never).
    /// Only under `CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP`.
    pub(in crate::runtime::interpreter::jit_bridge) last_deferred_new_release_stamp: std::sync::atomic::AtomicU64,
    /// `CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP`, read once per VM
    /// ([`deferred_new_release_stamp_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) deferred_new_release_stamp_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_CALLEE_NEG_KEYED_STAMP`, read once per VM
    /// ([`callee_neg_keyed_stamp_enabled`]; interpreter round i4 wave 92,
    /// lane L7).
    pub(in crate::runtime::interpreter::jit_bridge) callee_neg_keyed_stamp_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER`, read once per VM
    /// ([`callee_door_defers_to_worker_enabled`]; interpreter round i4 wave
    /// 93, lane L7).
    pub(in crate::runtime::interpreter::jit_bridge) callee_door_defers_to_worker_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_CALLEE_DOOR_C1_FIRST`, read once per VM.
    pub(in crate::runtime::interpreter::jit_bridge) callee_door_c1_first_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_SPLICE_REFUSES_UNRESOLVED_SITE`, read once per VM
    /// ([`splice_refuses_unresolved_site_enabled`]; wave 51, lane L1).
    pub(in crate::runtime::interpreter::jit_bridge) splice_unresolved_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_DOOR_PEEKS_UNRESOLVED_SITES`, read once per VM
    /// ([`door_peeks_unresolved_sites_enabled`]; wave 51, lane L1).
    pub(in crate::runtime::interpreter::jit_bridge) door_peek_switch: std::sync::OnceLock<bool>,
    /// Round 15 wave 4 (lane sync3, SY2-2): the recompiles
    /// [`request_static_self_lock_recompile`] asked for, keyed by `(class id,
    /// method name, descriptor)`, valued by `(JitCache::redefine_epoch, how
    /// many, the entry of the wrapped body the last one was asked against)`:
    /// at most [`STATIC_SELF_LOCK_RECOMPILES_PER_EPOCH`] per method until a
    /// redefinition, one per wrapped body. Touched only on a door's cold path.
    /// Round 15 wave 5 (lane sync4): the entry is also that of a body the last
    /// question found could not self-lock with the slot present, so that body
    /// is not asked about again.
    pub(in crate::runtime::interpreter::jit_bridge) static_self_lock_recompiles:
        std::sync::Mutex<std::collections::HashMap<(u32, Arc<str>, Arc<str>), (u32, u8, usize)>>,
    /// JIT round 16 wave 14 (lane throw4, TP16-20): the methods whose compile
    /// left a statically bound call on the dispatch helper because the callee
    /// could not bind YET ([`late_bind_refusal_may_lift`]), keyed by the
    /// callee's `class.methoddescriptor` ([`late_bind_callee_key`]). Filled by
    /// the background door's direct-bind refusals ([`note_late_bind_waiter`]),
    /// drained by [`late_bind_site_served`]. Touched only on cold paths, and
    /// only under `CRATONVM_JIT_LATE_BIND_RECOMPILE`.
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_waiters: std::sync::Mutex<std::collections::HashMap<String, Vec<LateBindWaiter>>>,
    /// `late_bind_waiters.len()`, written under its lock, so a dispatch fill in
    /// a VM with no waiter pays one load.
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_waiter_count: std::sync::atomic::AtomicUsize,
    /// The methods [`late_bind_site_served`] asked a recompile of, by `(class
    /// id, method name, descriptor)`: their next publication makes the body it
    /// displaces not entrant ([`late_bind_retire_displaced`]).
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_pending: std::sync::Mutex<std::collections::HashSet<(u32, Arc<str>, Arc<str>)>>,
    /// `late_bind_pending.len()`, written under its lock.
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_pending_count: std::sync::atomic::AtomicUsize,
    /// JIT round 19 wave 2, lane eager2 (EAGER19-3,
    /// `CRATONVM_JIT_LATE_BIND_PENDING_STAMP`): the late-bind generation each
    /// pending mark was set at, by the same key as `late_bind_pending`
    /// ([`late_bind_take_pending_started`]). Written beside the set, under its
    /// own lock, only while the switch is on.
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_pending_stamps:
        std::sync::Mutex<std::collections::HashMap<(u32, Arc<str>, Arc<str>), u64>>,
    /// The late-bind generation: bumped by every pending mark (EAGER19-3). A
    /// compile reads it when it starts reading its bytecode, so a publication
    /// can tell whether its compile began before or after a request.
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_generation: std::sync::atomic::AtomicU64,
    /// EAGER19-3: methods a publication found with a late-bind mark set after
    /// its compile started ([`late_bind_note_older_publication`]); the
    /// background task of that method takes its row and owes the recompile
    /// through its outcome ([`take_late_bind_owed_retry`]). Bounded by
    /// [`LATE_BIND_CALLEE_CAP`]. JIT round 21 lane indyconst: also the one-shot
    /// retries answered `AlreadyQueued` ([`owe_one_shot_retry_if_already_queued`]).
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_owed: std::sync::Mutex<std::collections::HashSet<(u32, Arc<str>, Arc<str>)>>,
    /// JIT round 20, lane stash (EAGER2-1): how many times each method's
    /// late-bind mark was put back after a republish whose `put` kept another
    /// body ([`late_bind_restore_after_unpublished_put`]), keyed like
    /// `late_bind_pending`. Bounds the restores: a `put` refusal that repeats
    /// (R12-4's single-pass-over-optimizing refusal is reported as a publish,
    /// so it charges no retry budget) cannot re-ask forever. Bounded by
    /// [`LATE_BIND_CALLEE_CAP`] rows.
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_restores: std::sync::Mutex<std::collections::HashMap<(u32, Arc<str>, Arc<str>), u8>>,
    /// JIT round 23 wave 8 (lane W8-L2, CF-W8-2,
    /// `CRATONVM_JIT_LATE_BIND_COLD_CALLEE`): the optimizing bodies that left
    /// a statically bound call unbound because its callee was COLD
    /// (`eager-callee-chain-cold`), and the callers already recompiled once on
    /// that account ([`note_cold_callee_late_bind_waiter`],
    /// [`late_bind_cold_fire_admits`]). Bounded by [`LATE_BIND_CALLEE_CAP`].
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_cold: std::sync::Mutex<LateBindColdState>,
    // JIT round 20 wave 11, lane pLate (W16L1-3,
    // `CRATONVM_JIT_STALE_CALLER_RECOMPILE`): the entries of the caller
    // bodies [`stale_caller_recompile_on_stub_pass`] already asked one
    // recompile of, so each caller artifact is asked at most once. Bounded
    // by [`STALE_CALLER_RECOMPILE_CAP`]; touched only on a not-entrant
    // stub's slow path.
    // addr-keyed: not a heap address -- a JIT code entry of a caller artifact; a reused entry only skips one optional recompile request.
    pub(in crate::runtime::interpreter::jit_bridge) stale_caller_recompiles: std::sync::Mutex<std::collections::HashSet<usize>>,
    /// JIT round 19 proposal wave 7, lane interp7 (OSRENTRY6-2): the OSR
    /// door's own direct-bind refusal census, by [`cratonvm_jit::DirectBindRefusal`]
    /// discriminant, with the reason the door classified for a callee the
    /// slow door did not hand back ([`osr_unbound_callee_reason`]) instead of
    /// the process table's lump `CalleeNotYetCompiled`. Per VM; compile-time
    /// increments only. Read by [`osr_door_bind_refusal_reasons`].
    pub(in crate::runtime::interpreter::jit_bridge) osr_door_bind_refusals: [std::sync::atomic::AtomicU64; cratonvm_jit::DIRECT_BIND_REFUSAL_COUNT],
    /// JIT round 19 proposal wave 7, lane interp7 (EAGER19-4,
    /// `CRATONVM_JIT_LATE_BIND_OSR`): OSR bodies whose statically bound call
    /// stayed on the dispatch helper for a refusal that can lift
    /// ([`late_bind_refusal_may_lift`]), keyed like
    /// [`Self::late_bind_waiters`] by the callee's constant-pool names.
    /// Bounded by [`LATE_BIND_CALLEE_CAP`] / [`LATE_BIND_WAITERS_PER_CALLEE`].
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_osr_waiters:
        std::sync::Mutex<std::collections::HashMap<String, Vec<OsrLateBindWaiter>>>,
    /// Number of keys in [`Self::late_bind_osr_waiters`]: the one relaxed
    /// load a dispatch fill pays when no OSR body waits.
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_osr_waiter_count: std::sync::atomic::AtomicUsize,
    /// EAGER19-4: methods whose cached single-pass OSR body a late bind asked
    /// to replace. The next BACKGROUND OSR compile of the method takes its row
    /// and compiles instead of reusing the cached body
    /// ([`take_late_bind_osr_stale`]); the mutator's reuse door never reads it.
    /// Bounded by [`LATE_BIND_CALLEE_CAP`].
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_osr_stale: std::sync::Mutex<std::collections::HashSet<(u32, Arc<str>, Arc<str>)>>,
    /// Number of rows in [`Self::late_bind_osr_stale`].
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_osr_stale_count: std::sync::atomic::AtomicUsize,
    /// JIT round 19 proposal wave 8, lane osr8 (INTERP719-2,
    /// `CRATONVM_JIT_LATE_BIND_OSR_OPTIMIZING`): optimizing OSR memo keys
    /// (`osr_optimizing_refusal_key`) whose accepted body a late bind asked
    /// forgotten; `osr_optimizing_cached` takes the row at the entry's next
    /// lookup ([`late_bind_osr_optimizing_stale_taken`]). Bounded by
    /// [`LATE_BIND_CALLEE_CAP`].
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_osr_optimizing_stale: std::sync::Mutex<std::collections::HashSet<(u32, u64, usize)>>,
    /// Number of rows in [`Self::late_bind_osr_optimizing_stale`].
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_osr_optimizing_stale_count: std::sync::atomic::AtomicUsize,
    /// Optimizing OSR bodies the memo forgot for a late bind
    /// ([`osr_optimizing_cached`]), with their method's identity: a frame may
    /// still be running one, in a loop it will not leave by itself. Once the
    /// rebuilt body is enterable at every header
    /// ([`arm_superseded_optimizing_osr_exits`]) each is armed to leave at its
    /// next back-edge poll, and the row is dropped. Rows hold their body
    /// weakly (t1 wave 9, lane JITB): a row whose body no frame runs is
    /// dropped too. Bounded by [`LATE_BIND_CALLEE_CAP`]
    /// ([`push_superseded_osr_row`]).
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_osr_superseded: std::sync::Mutex<Vec<SupersededOsrBody>>,

    /// JIT round 21, lane tcshape (`CRATONVM_JIT_DEFERRED_FIELD_RECOMPILE`):
    /// the deferred field sites (`(holder class id, cp index)`) a published
    /// body is waiting on ([`note_deferred_field_waiters`],
    /// [`note_osr_init_waiters`]), `false` until the site's first linked
    /// execution served it ([`deferred_field_site_linked`]), `true` after: a
    /// served site is never waited on again, so a recompile that still has to
    /// defer it cannot ask another. Bounded by [`LATE_BIND_CALLEE_CAP`].
    pub(in crate::runtime::interpreter::jit_bridge) deferred_field_sites_waited: std::sync::Mutex<std::collections::HashMap<(u32, u16), bool>>,
    /// Unserved rows of [`Self::deferred_field_sites_waited`], written under
    /// its lock: the one load a deferred site's execution pays when no body
    /// waits on any.
    pub(in crate::runtime::interpreter::jit_bridge) deferred_field_waiting_count: std::sync::atomic::AtomicUsize,
    /// tcm21 (TCSHAPE21-1 census): executions of a deferred field site's helper
    /// (`jit_field_site_cp`) in this VM, counted only under
    /// `CRATONVM_DBG_JITC=1` ([`note_deferred_field_site_exec`]).
    pub(in crate::runtime::interpreter::jit_bridge) deferred_field_site_execs: std::sync::atomic::AtomicU64,
    /// JIT round 19 proposal wave 7, lane interp7 (OSRENTRY6-3,
    /// `CRATONVM_JIT_OSR_EARLY_REQUEST`): back edges each method's
    /// interpreted activations ran, summed at the frame pop, keyed by the
    /// invocation-counter key ([`note_osr_method_backedges`],
    /// [`take_osr_method_backedges`]). Written only under the switch.
    /// Bounded by [`OSR_METHOD_BACKEDGE_KEYS_MAX`].
    pub(in crate::runtime::interpreter::jit_bridge) osr_method_backedges:
        parking_lot::RwLock<std::collections::HashMap<u128, std::sync::atomic::AtomicU32>>,
    /// JIT round 19 proposal wave 11, lane mon11 (the short-loop page's
    /// `i + b`, `CRATONVM_JIT_TIERUP_I_PLUS_B`): each method's RAW summed back
    /// edges, never reset ([`note_tierup_method_backedges`]). A separate map
    /// from [`Self::osr_method_backedges`], which OSRENTRY6-3 resets per
    /// request. Written only under the switch. Bounded by
    /// [`OSR_METHOD_BACKEDGE_KEYS_MAX`].
    pub(in crate::runtime::interpreter::jit_bridge) tierup_method_backedges:
        parking_lot::RwLock<std::collections::HashMap<u128, std::sync::atomic::AtomicU32>>,
    /// JIT round 19 wave 2, lane eager2 (EAGER19-1,
    /// `CRATONVM_JIT_UNINIT_NEW_TRAP`): the `(class id, method, descriptor,
    /// bci)` of every uninitialized-`new` trap an optimizing compile was
    /// offered ([`uninit_new_trap_admitted`]), valued `true` once one fired
    /// there (the per-bci one-shot budget: a spent site compiles as before).
    /// Bounded by [`UNINIT_NEW_TRAP_SITES_MAX`]; touched only on cold paths.
    pub(in crate::runtime::interpreter::jit_bridge) uninit_new_trap_sites:
        std::sync::Mutex<std::collections::HashMap<(u32, Arc<str>, Arc<str>, u32), bool>>,
    /// JIT round 21 lane indylate (`CRATONVM_JIT_INDY_LATE_LINK_TRAP_RECOMPILE`):
    /// the `(class id, method, descriptor, bci)` of every `invokedynamic` trap
    /// reprieved while its instruction was NOT linked yet
    /// ([`indy_late_link_trap_reprieve`]): one such reprieve per instruction.
    /// Bounded by [`INDY_LATE_LINK_REPRIEVES_MAX`]; touched only on deopts.
    pub(in crate::runtime::interpreter::jit_bridge) indy_late_link_reprieves:
        std::sync::Mutex<std::collections::HashSet<(u32, Arc<str>, Arc<str>, u32)>>,
    /// indy21c: the class id of `java.lang.invoke.CallSite` on this VM, learnt
    /// on the first read of a call site's `target`
    /// (`invokedynamic::call_site_target_slot`), so the per-call read takes no
    /// class-manager lock. Per VM, never reissued (class ids are not reused).
    pub(crate) call_site_class: std::sync::OnceLock<ClassId>,
    /// JIT round 19 wave 3, lane eager3: the `CRATONVM_DBG_EAGER_CHAIN_CENSUS`
    /// counters ([`EagerChainCensus`]), written only with that switch on.
    pub(in crate::runtime::interpreter::jit_bridge) eager_chain_census: EagerChainCensus,
    /// `CRATONVM_JIT_LATE_BIND_RECOMPILE`, read once per VM
    /// ([`late_bind_recompile_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) late_bind_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_FIRST_CALL_RECOMPILE`, read once per VM
    /// ([`first_call_recompile_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) first_call_recompile_switch: std::sync::OnceLock<bool>,
    /// JIT round 21 wave 1 (lane indy): `CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST`,
    /// read once per VM ([`getstatic_own_clinit_fast_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) getstatic_own_clinit_switch: std::sync::OnceLock<bool>,
    /// JIT round 21 wave 1 (lane indy, INDY21-3): `CRATONVM_JIT_STATIC_INIT_RACE_SERVE`,
    /// read once per VM ([`static_init_race_serve_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) static_init_race_switch: std::sync::OnceLock<bool>,
    /// JIT round 22 wave 1 (lane runtime): `CRATONVM_JIT_STATIC_INIT_RACE_EXACT`,
    /// read once per VM ([`static_init_race_exact_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) static_init_race_exact_switch: std::sync::OnceLock<bool>,
    /// JIT round 21 wave 1 (lane indy, INDY21-3): the classes whose
    /// `<clinit>` completed most recently, numbered ([`StaticInitLog`]), so a
    /// compile door can tell which of its static holders finished initializing
    /// WHILE it compiled ([`note_static_init_waiters`]). Written once per
    /// successful class initialization, read once per method-entry compile.
    pub(in crate::runtime::interpreter::jit_bridge) static_init_log: std::sync::Mutex<StaticInitLog>,
    /// JIT round 21 wave 1 (lane indy, INDY21-2, `CRATONVM_JIT_IR_CLINIT_BARRIER`):
    /// the init-owner cells compiled class-init barriers bake, by class id
    /// ([`clinit_owner_cell_for`]). Each cell is leaked (compiled code holds
    /// its address); at most [`CLINIT_OWNER_CELL_CAP`].
    pub(in crate::runtime::interpreter::jit_bridge) clinit_owner_cells:
        std::sync::Mutex<std::collections::HashMap<u32, &'static std::sync::atomic::AtomicU64>>,
    /// `clinit_owner_cells.len()`, so a class-init claim or completion in a VM
    /// with no cell pays one load ([`clinit_owner_note`]).
    pub(in crate::runtime::interpreter::jit_bridge) clinit_owner_cell_count: std::sync::atomic::AtomicUsize,
    /// JIT round 16 wave 17 (lane mh7; MH16-8 at compile time,
    /// `CRATONVM_JIT_MH_CONSTANT_COMPILE`): the exact direct STATIC handles the
    /// dispatch helpers' constant bind admitted, keyed by `(handle address,
    /// call-site descriptor)`, valued by the collection epoch of the admission
    /// and the member ([`note_mh_constant_target`]). Read by the compile doors
    /// ([`CpResolvers::mh_constant`]), which compare the address a `static
    /// final` field holds now, in the same epoch: an equal address in an equal
    /// epoch is the same object. Bounded by [`MH_CONSTANT_TARGETS_MAX`];
    /// touched only on cold paths.
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_targets:
        std::sync::Mutex<std::collections::HashMap<(u64, Box<str>), MhConstantTarget>>,
    /// JIT r23 wave 4b, lane L2 (L2W4B-1, `CRATONVM_JIT_MH_CONSTANT_CONDY`):
    /// the member of each resolved `CONSTANT_Dynamic` whose result is a real
    /// `DirectMethodHandle`, keyed by `(holder class id, cp index)`, decoded
    /// once on the mutator that resolved the entry
    /// ([`note_mh_condy_member`]). A resolved condy never changes (JVMS
    /// §5.4.3.6), so a row goes stale only by a redefinition of the holder,
    /// which drops it ([`retire_mh_condy_members_of_class`]); an unload of
    /// the holder drops its rows too (j27 wave 7,
    /// [`retire_mh_condy_members_of_unloaded`]). Bounded by
    /// [`MH_CONDY_MEMBERS_MAX`]; touched only on cold paths.
    pub(in crate::runtime::interpreter::jit_bridge) const_folds: std::sync::Mutex<cratonvm_jit::ir::ConstFoldTables>,
    pub(in crate::runtime::interpreter::jit_bridge) mh_condy_members: std::sync::Mutex<
        std::collections::HashMap<(u32, u16), cratonvm_native_builtins::lang_invoke::MhDirectMember>,
    >,
    /// JIT r23 wave 6, lane L2 (`CRATONVM_JIT_WELL_KNOWN_BOX_CLASS_MEMO`):
    /// `java/lang/Integer`'s and `java/lang/Long`'s class ids once
    /// `jit_well_known_class_id` has answered them (`0` until then); see
    /// [`well_known_box_class_memo`].
    pub(in crate::runtime::interpreter::jit_bridge) well_known_box_class_ids: [std::sync::atomic::AtomicU32; 2],
    /// `CRATONVM_JIT_VH_THIN_RELEASE_PLAIN`, read once per VM
    /// ([`vh_thin_release_plain_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) vh_thin_release_plain_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_MH_CONSTANT_COMPILE`, read once per VM
    /// ([`mh_constant_compile_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_switch: std::sync::OnceLock<bool>,
    /// JIT round 17 wave 1 (lane mhconst; MH7-2,
    /// `CRATONVM_JIT_MH_CONSTANT_FIELD_ROWS`): the member a `static final`
    /// field's handle was admitted as, keyed by `(holder class id, static
    /// index, call-site descriptor)`. Filled only where the field was READ
    /// holding an admitted handle in the admission's own collection epoch
    /// ([`note_mh_constant_field_row`]); unlike
    /// [`Self::mh_constant_targets`] it survives a collection, because the
    /// key is the field and not the handle's address. Bounded by
    /// [`MH_CONSTANT_FIELD_ROWS_MAX`]; never evicted (class ids are never
    /// reused, so an unloaded holder's rows are dead weight, not wrong).
    ///
    /// JIT round 17 wave 3 (lane mh4;
    /// `r17-mh-constant-field-row-trusts-a-rewritten-field-across-a-collection-FIXED-20261005.md`):
    /// each row carries its WITNESS, a JNI weak global ref (0: none) of the
    /// handle the field was read holding, made on a mutator
    /// ([`note_mh_constant_field_row_with`], [`mh_constant_field_rows_witnessed`]).
    /// The collector remaps it, so a row of an older epoch is trusted only
    /// when its witness resolves to the address the field holds now
    /// ([`mh_constant_field_row_witness_holds`]).
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_field_rows: std::sync::Mutex<
        std::collections::HashMap<(u32, usize, Box<str>), (MhConstantTarget, u64)>,
    >,
    /// JIT round 17 wave 1 (lane mhconst; MH7-1,
    /// `CRATONVM_JIT_MH_CONSTANT_RECOMPILE`): the optimizing compiles that
    /// found no admitted handle for a `static final` field, keyed by `(holder
    /// class id, static index, call-site descriptor)`
    /// ([`note_mh_constant_waiter`]). The constant bind's next admission of
    /// the handle that field holds asks one recompile of each
    /// ([`mh_constant_waiters_served`]).
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_waiters:
        std::sync::Mutex<std::collections::HashMap<(u32, usize, Box<str>), Vec<LateBindWaiter>>>,
    /// `mh_constant_waiters.len()`, written under its lock: one relaxed load
    /// when nothing waits.
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_waiter_count: std::sync::atomic::AtomicUsize,
    /// JIT round 22 (lane memory, FFM22-1): the compiled-call rows of `static
    /// final` bound FFM downcall handles ([`ffm_const_note_fill`],
    /// [`ffm_const_note_compile`], [`ffm_const_row_addr`]).
    pub(in crate::runtime::interpreter::jit_bridge) ffm_const: FfmConstState,
    /// Recompiles [`mh_constant_waiters_served`] asked for, per method: at
    /// most [`MH_CONSTANT_RECOMPILES_PER_METHOD`] each, so a program that
    /// collects between every fill and every compile cannot recompile a
    /// method forever.
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_recompiled:
        std::sync::Mutex<std::collections::HashMap<(u32, Arc<str>, Arc<str>), u8>>,
    /// `CRATONVM_JIT_MH_CONSTANT_RECOMPILE`, read once per VM
    /// ([`mh_constant_recompile_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_recompile_switch: std::sync::OnceLock<bool>,
    /// `CRATONVM_JIT_MH_CONSTANT_FIELD_ROWS`, read once per VM
    /// ([`mh_constant_field_rows_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_field_rows_switch: std::sync::OnceLock<bool>,
    /// JIT round 17 wave 2 (lane mh2; MH7-3,
    /// `CRATONVM_JIT_MH_CONSTANT_IMMUTABLE_CHECK`): the words of every handle
    /// [`Self::mh_constant_targets`] admitted (`MH_COMPILED_BIND_GUARD_SLOTS`
    /// and `type`), by address, with the epoch they were read in
    /// ([`note_mh_constant_guard`]). A handle at the same address in the same
    /// epoch with other words was rewritten in place after its admission
    /// ([`mh_constant_guard_verify`], [`mh_constant_guard_sweep`]). Bounded
    /// by [`MH_CONSTANT_TARGETS_MAX`]; touched only on cold paths.
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_guards: std::sync::Mutex<std::collections::HashMap<u64, (u64, [u64; 7])>>,
    /// `CRATONVM_JIT_MH_CONSTANT_INTERP_FILL`, read once per VM
    /// ([`mh_constant_interp_fill_enabled`]; round 17 wave 2, MHCONST17-1).
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_interp_fill_switch: std::sync::OnceLock<bool>,
    /// JIT round 17 wave 2 (lane mh3;
    /// `r17-mh-constant-compiled-body-not-retired-on-handle-rewrite-FIXED-20261005.md`,
    /// `CRATONVM_JIT_MH_CONSTANT_RETIRE_ON_REWRITE`): the classes whose
    /// methods the compile door ([`CpResolvers::mh_constant`]) compiled a
    /// constant handle into as a plain call of its member, keyed by that
    /// member `(class id, name, descriptor)`, each with its class name
    /// ([`note_mh_constant_compiled`]). An admitted handle found rewritten in
    /// place ([`mh_constant_guard_violated`]) withdraws those classes' bodies
    /// ([`mh_constant_retire_compiled`]). Bounded by
    /// [`MH_CONSTANT_COMPILED_MEMBERS_MAX`] and
    /// [`MH_CONSTANT_COMPILED_CLASSES_PER_MEMBER`]; past either the door
    /// refuses the site (no dependency, no assumption).
    pub(in crate::runtime::interpreter::jit_bridge) mh_constant_compiled: std::sync::Mutex<
        std::collections::HashMap<(u32, Box<str>, Box<str>), Vec<(u32, Arc<str>)>>,
    >,
    /// JIT round 22 wave 1, lane runtime (INDY21-4,
    /// `CRATONVM_JIT_INDY_LEAF_DIRECT_CALL`), read once per VM
    /// ([`indy_leaf_direct_call_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) indy_leaf_direct_call_switch: std::sync::OnceLock<bool>,
    /// JIT round 22 wave 1, lane runtime (INDY21-5): the retarget recompiles
    /// of `MutableCallSite` / `VolatileCallSite` owner methods
    /// (`CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`) this VM asked for,
    /// `[will run, refused]` ([`note_indy_retarget_recompile`],
    /// [`indy_retarget_recompile_census`]). Diagnostic counters, read at exit.
    pub(in crate::runtime::interpreter::jit_bridge) indy_retarget_recompiles: [std::sync::atomic::AtomicU64; 2],
    /// JIT round 22 wave 11, lane runtime (reporting only): the sweep driver's
    /// windowed drains (`CRATONVM_JIT_CODE_CACHE_SWEEP_SELF_BLOCKED`),
    /// `[opened, left the cap cleared, skipped: chain unreadable]`
    /// ([`code_cache_sweep_driver_census`]).
    pub(in crate::runtime::interpreter::jit_bridge) sweep_self_drains: [std::sync::atomic::AtomicU64; 3],
}

/// `CRATONVM_JIT_INDY_LEAF_DIRECT_CALL` -- perf, **default on**, `=1` opts in
/// (JIT round 22 wave 1, lane runtime; INDY21-4 of
/// `docs/feature-designs/jit-ir-optimizations.md`): a linked
/// `invokedynamic` bridge call whose target is a bound direct leaf (the
/// linked-direct memo, `jit_indy_linked_direct_leaf`, or a bimorphic guard's
/// second arm, `jit_indy_guarded_leaf`) enters the leaf's cached compiled
/// entry through `jit::helpers::mh_bind_direct_call` -- the constant-bound
/// `MethodHandle` sites' lean door (`CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT`,
/// default ON since its A/B) -- instead of walking the whole
/// `jit_invoke_dispatch_body` prefix per call. Every call that door cannot
/// serve (no cached entry yet, a native, a quiesced site, a diagnostic run)
/// is handed to the same `jit_invoke_dispatch_body` as before. Read once per
/// VM.
pub(crate) fn indy_leaf_direct_call_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .indy_leaf_direct_call_switch
        .get_or_init(|| {
            true
        })
}

/// JIT round 22 wave 1, lane runtime (INDY21-5): count one retarget
/// recompile request of a `MutableCallSite` / `VolatileCallSite` owner
/// method, `will_run` as [`indy_mutable_retarget_recompile`] answered it.
/// Independent of `CRATONVM_DBG_JITC`: the exit census
/// ([`indy_retarget_recompile_census`]) shows whether any workload arms the
/// retarget recompile at all.
pub(crate) fn note_indy_retarget_recompile(shared: &SharedVm, will_run: bool) {
    note_indy_retarget_recompile_in(&shared.jit.bridge, will_run);
}

/// [`note_indy_retarget_recompile`] on one VM's bridge state.
pub(in crate::runtime::interpreter::jit_bridge) fn note_indy_retarget_recompile_in(state: &JitBridgeState, will_run: bool) {
    let i = usize::from(!will_run);
    state.indy_retarget_recompiles[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// JIT round 22 wave 1, lane runtime (INDY21-5): `(will_run, refused)`
/// retarget recompile requests this VM made
/// (`CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`). Both stay `0` with the
/// switch off; with it on, `0 / 0` means no `MutableCallSite` was retargeted
/// away from a planned target long enough to ask one. Printed by the
/// launcher's `CRATONVM_INTRINSIC_STATS=1` exit census.
pub fn indy_retarget_recompile_census(shared: &SharedVm) -> (u64, u64) {
    indy_retarget_recompile_census_in(&shared.jit.bridge)
}

/// [`indy_retarget_recompile_census`] of one VM's bridge state.
pub(in crate::runtime::interpreter::jit_bridge) fn indy_retarget_recompile_census_in(state: &JitBridgeState) -> (u64, u64) {
    let c = &state.indy_retarget_recompiles;
    (
        c[0].load(std::sync::atomic::Ordering::Relaxed),
        c[1].load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// One handle of [`JitBridgeState::mh_constant_targets`]: the member the
/// constant bind admitted it as (an exact direct static handle), and the
/// collection epoch it was admitted in (JIT round 16 wave 17, lane mh7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MhConstantTarget {
    pub(in crate::runtime::interpreter::jit_bridge) epoch: u64,
    pub(in crate::runtime::interpreter::jit_bridge) class: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) name: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) desc: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) class_id: u32,
    /// `3` for an exact direct static handle; `0` (JIT round 17 wave 3, lane
    /// mh4; MHCONST17-2, [`mh_constant_virtual_enabled`]) for an unbound
    /// virtual one, whose member is called with the site's first argument as
    /// its receiver.
    ///
    /// JIT round 21 wave 19 (lane mhbind2; MHBIND21-1,
    /// [`mh_constant_compile_bound_enabled`]): `4` for a REAL `bindTo` result
    /// of a virtual member, whose member is called on the handle's bound value
    /// (`argL1`); [`Self::bound`] says where that value is and what it is.
    pub(in crate::runtime::interpreter::jit_bridge) invoke_kind: u8,
    /// `invoke_kind 4` only: `(slot of the species' argL1 field, the bound
    /// value's exact class id)`, read by the helper's fill on the mutator. The
    /// field is `final` and the handle the same object (an equal address in an
    /// equal epoch), so both hold for the handle's life. `None` otherwise.
    pub(in crate::runtime::interpreter::jit_bridge) bound: Option<(u32, u32)>,
    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1,
    /// [`mh_constant_compile_filter_enabled`]): `invoke_kind 5` only, a REAL
    /// filter species' second call ([`MhConstantFilter`]); the row's own
    /// member is the MAIN call. `None` otherwise.
    pub(in crate::runtime::interpreter::jit_bridge) filter: Option<Box<MhConstantFilter>>,
    /// `invoke_kind 7` (`CRATONVM_JIT_MH_CONSTANT_COMPILE_CONVERTING`): a
    /// converting REAL `asType` handle's casts, unboxes, and return boxing.
    pub(in crate::runtime::interpreter::jit_bridge) convert: Option<Box<cratonvm_jit::compile_request::MhConstantConvertRow>>,
}

/// The PRE call of a filter species row (JIT round 21 wave 20, lane
/// mhfilter; MHBIND2-1): what [`CpResolvers::mh_constant_filter`] hands the
/// jit crate as a `MhConstantFilterRow`. Read by the helper's fill on the
/// mutator; every field it comes from is `final` and the handle the same
/// object (an equal address in an equal epoch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MhConstantFilter {
    /// `Some(p)`: `filterArguments` at argument `p` (PRE is the filter, the
    /// row's member the target); `None`: `filterReturnValue` (PRE is the
    /// target, the row's member the filter).
    pub(in crate::runtime::interpreter::jit_bridge) position: Option<u16>,
    /// The row's own (MAIN) member's kind: `3` static, `0` virtual.
    pub(in crate::runtime::interpreter::jit_bridge) main_kind: u8,
    pub(in crate::runtime::interpreter::jit_bridge) pre_class: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) pre_name: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) pre_desc: Box<str>,
    /// `3` static, `0` virtual.
    pub(in crate::runtime::interpreter::jit_bridge) pre_kind: u8,
    pub(in crate::runtime::interpreter::jit_bridge) pre_class_id: u32,
    /// The species' `argL0` slot, `0` when it did not resolve.
    pub(in crate::runtime::interpreter::jit_bridge) target_slot: u32,
}

impl MhConstantFilter {
    /// The row the helper's fill records (`vm/src/jit/helpers.rs`).
    pub(crate) fn new(
        position: Option<u16>,
        main_kind: u8,
        (class, name, desc): (&str, &str, &str),
        pre_kind: u8,
        pre_class_id: u32,
        target_slot: u32,
    ) -> Self {
        MhConstantFilter {
            position,
            main_kind,
            pre_class: Box::from(class),
            pre_name: Box::from(name),
            pre_desc: Box::from(desc),
            pre_kind,
            pre_class_id,
            target_slot,
        }
    }

    /// The jit crate's view of it.
    pub(in crate::runtime::interpreter::jit_bridge) fn row(&self) -> cratonvm_jit::compile_request::MhConstantFilterRow {
        cratonvm_jit::compile_request::MhConstantFilterRow {
            position: self.position,
            main_kind: self.main_kind,
            pre_class: self.pre_class.to_string(),
            pre_name: self.pre_name.to_string(),
            pre_desc: self.pre_desc.to_string(),
            pre_kind: self.pre_kind,
            pre_class_id: self.pre_class_id,
            target_slot: self.target_slot,
        }
    }
}

/// Rows of [`JitBridgeState::mh_constant_targets`]; past it a fill drops the
/// rows of older epochs, and records nothing if that frees no room.
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_TARGETS_MAX: usize = 256;

/// `CRATONVM_JIT_MH_CONSTANT_COMPILE` -- default ON since the w17a A/B
/// (exact-nested 511 -> 74 ns, exact-two 526 -> 302; `=0` turns it off) (JIT round 16
/// wave 17, lane mh7; MH16-8 at compile time): the optimizing tier compiles a
/// `MethodHandle.invokeExact` / `invoke` whose receiver is a `static final`
/// field holding an exact direct static handle as an `invokestatic` of the
/// handle's member ([`CpResolvers::mh_constant`], `ir_mh_constant_plan` in the
/// jit crate). `1`/`true`/`on`/`yes` turns it on. Read once per VM.
pub(crate) fn mh_constant_compile_enabled(shared: &SharedVm) -> bool {
    *shared.jit.bridge.mh_constant_switch.get_or_init(|| {
        true
    })
}

/// [`note_mh_constant_target`] for a member of `invoke_kind` (`3` static, `0`
/// an unbound virtual handle's; JIT round 17 wave 3, lane mh4). On the
/// mutator: the fills' own thread.
pub(crate) fn note_mh_constant_target_kind(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    (class, name, desc): (&str, &str, &str),
    class_id: u32,
    invoke_kind: u8,
) {
    let target = MhConstantTarget {
        epoch,
        class: Box::from(class),
        name: Box::from(name),
        desc: Box::from(desc),
        class_id,
        invoke_kind,
        bound: None,
        filter: None,
        convert: None,
    };
    note_mh_constant_target_row(shared, handle, epoch, site, target);
}

/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND` -- perf, default ON, `=0` off (JIT
/// round 21 wave 19, lane mhbind2; MHBIND21-1 of
/// `jit-r21-mhbind-proposals.md`, page
/// `jr21-mhbind-constant-bound-and-bindto-FIXED-20261029.md`): the dispatch helper's
/// fill records a REAL `bindTo` result of a virtual member it bound
/// (`CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND`) for the compile doors
/// ([`note_mh_constant_target_bound`]), and the door answers it as `invoke_kind
/// 4` ([`CpResolvers::mh_constant`], [`CpResolvers::mh_constant_bound`]): the
/// optimizing tier compiles `CB.invokeExact(a*)` on a `static final` such handle
/// as `invokevirtual member` whose receiver is the handle's `argL1` field
/// (`IrBuilder::mh_constant_opcode`), and splices the member for the bound
/// value's exact class behind the guarded splice's class test. Read per
/// question (cold fills and compile time only).
pub(crate) fn mh_constant_compile_bound_enabled() -> bool {
    true
}

/// [`note_mh_constant_target_kind`] for a REAL `bindTo` result of a virtual
/// member (`invoke_kind 4`; JIT round 21 wave 19, lane mhbind2, MHBIND21-1):
/// `receiver_slot` is the species' `argL1` slot, `receiver_class` the bound
/// value's exact class. On the mutator: the helper fill's own thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn note_mh_constant_target_bound(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    (class, name, desc): (&str, &str, &str),
    class_id: u32,
    receiver_slot: u32,
    receiver_class: u32,
) {
    let target = MhConstantTarget {
        epoch,
        class: Box::from(class),
        name: Box::from(name),
        desc: Box::from(desc),
        class_id,
        invoke_kind: 4,
        bound: Some((receiver_slot, receiver_class)),
        filter: None,
        convert: None,
    };
    note_mh_constant_target_row(shared, handle, epoch, site, target);
}

/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC` -- perf, default ON since
/// 2026-10-28 (j27 WARNING flip; =0 restores; j27 wave 6, lane T1LAM2; item 1 of
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the dispatch helper's fill records a REAL `bindTo` result of a STATIC
/// member bound on its leading reference parameter (the MH16-7 bind,
/// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND_STATIC`) for the compile doors
/// ([`note_mh_constant_target_bound_static`]), and the door answers it as
/// `invoke_kind 6` ([`CpResolvers::mh_constant`], [`CpResolvers::mh_constant_bound`]):
/// the optimizing tier compiles `BS.invokeExact(a*)` on a `static final` such
/// handle as `invokestatic member(BS.argL1, a*)` (`IrBuilder::mh_constant_bound_opcode`).
/// Off (`=0`), the call keeps the helper's bound route. Was opt-in until the
/// flip (MH16-7 itself is not host-verified yet). Read per question (cold
/// fills and compile time only).
pub(crate) fn mh_constant_compile_bound_static_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// [`note_mh_constant_target_bound`] for a REAL `bindTo` result of a STATIC
/// member (`invoke_kind 6`; j27 wave 6, lane T1LAM2): `value_slot` is the
/// species' `argL1` slot, whose value is the member's leading argument. The
/// row's class word is `0`: a static call has no receiver class to splice
/// for. On the mutator: the helper fill's own thread.
pub(crate) fn note_mh_constant_target_bound_static(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    (class, name, desc): (&str, &str, &str),
    class_id: u32,
    value_slot: u32,
) {
    let target = MhConstantTarget {
        epoch,
        class: Box::from(class),
        name: Box::from(name),
        desc: Box::from(desc),
        class_id,
        invoke_kind: 6,
        bound: Some((value_slot, 0)),
        filter: None,
        convert: None,
    };
    note_mh_constant_target_row(shared, handle, epoch, site, target);
}

/// May [`CpResolvers::mh_constant_bound`] hand a bound row of `invoke_kind` to
/// the jit crate (j27 wave 6, lane T1LAM2)? A virtual bound row (`4`) under
/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND`, a static one (`6`) under
/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`; no other kind has one.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_bound_kind_admitted(invoke_kind: u8, bound_on: bool, bound_static_on: bool) -> bool {
    match invoke_kind {
        4 => bound_on,
        6 => bound_static_on,
        _ => false,
    }
}

/// Is `site` the `invokeExact` type of a static `member` bound on its leading
/// reference parameter (j27 wave 6, lane T1LAM2)? The member's descriptor is
/// the site's with one leading `L..;` parameter in front -- the mirror of the
/// jit crate's `mh_constant_virtual_site_matches(site, member)`, whose site
/// carries the extra reference instead.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_bound_static_site_matches(member: &str, site: &str) -> bool {
    cratonvm_jit::mh_constant_virtual_site_matches(member, site)
}

/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER` -- perf, default ON, `=0` off
/// (JIT round 21 wave 20, lane mhfilter; MHBIND2-1 of
/// `jit-r21-mhbind2-proposals.md`, page
/// `jr21-mhbind-filtered-species-walks-the-link-chain-FIXED-20261027.md`): the
/// dispatch helper's fill records a REAL `filterArguments` /
/// `filterReturnValue` species over two direct handles that every bind
/// refused ([`note_mh_constant_target_filter`]), and the door answers it as
/// `invoke_kind 5` ([`CpResolvers::mh_constant`], [`CpResolvers::mh_constant_filter`]):
/// the optimizing tier compiles `F.invokeExact(a*)` on a `static final` such
/// handle as two plain calls (`IrBuilder::mh_constant_opcode`). Off, every
/// call walks the signature-polymorphic link chain (~6 helper hops). Read per
/// question (cold fills and compile time only).
pub(crate) fn mh_constant_compile_filter_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_CONVERTING` -- default ON (`=0` off):
/// compiles a `static final` converting REAL `asType` handle (`invoke_kind 7`)
/// with IR reference casts, wrapper unboxes, member call, and return boxing.
pub(crate) fn mh_constant_compile_converting_enabled() -> bool {
    true
}

/// [`note_mh_constant_target_kind`] for a REAL filter species (`invoke_kind
/// 5`; JIT round 21 wave 20, lane mhfilter, MHBIND2-1): the row's member is
/// the MAIN call, `filter` the PRE call. On the mutator: the helper fill's own
/// thread.
pub(crate) fn note_mh_constant_target_filter(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    (class, name, desc): (&str, &str, &str),
    class_id: u32,
    filter: MhConstantFilter,
) {
    let target = MhConstantTarget {
        epoch,
        class: Box::from(class),
        name: Box::from(name),
        desc: Box::from(desc),
        class_id,
        invoke_kind: 5,
        bound: None,
        filter: Some(Box::new(filter)),
        convert: None,
    };
    note_mh_constant_target_row(shared, handle, epoch, site, target);
}

/// [`note_mh_constant_target_kind`] for a converting REAL `asType` handle
/// (`invoke_kind 7`).
pub(crate) fn note_mh_constant_target_converting(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    (class, name, desc): (&str, &str, &str),
    class_id: u32,
    convert: cratonvm_jit::compile_request::MhConstantConvertRow,
) {
    let target = MhConstantTarget {
        epoch,
        class: Box::from(class),
        name: Box::from(name),
        desc: Box::from(desc),
        class_id,
        invoke_kind: 7,
        bound: None,
        filter: None,
        convert: Some(Box::new(convert)),
    };
    note_mh_constant_target_row(shared, handle, epoch, site, target);
}

/// The memo write, waiters and witnesses of one admitted handle (the body
/// [`note_mh_constant_target_kind`] had; shared with
/// [`note_mh_constant_target_bound`], JIT round 21 wave 19, lane mhbind2).
pub(in crate::runtime::interpreter::jit_bridge) fn note_mh_constant_target_row(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    target: MhConstantTarget,
) {
    {
        let mut rows = shared
            .jit
            .bridge
            .mh_constant_targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (handle, Box::from(site));
        let room = rows.contains_key(&key) || rows.len() < MH_CONSTANT_TARGETS_MAX || {
            rows.retain(|_, t| t.epoch == epoch);
            rows.len() < MH_CONSTANT_TARGETS_MAX
        };
        if room {
            rows.insert(key, target.clone());
        }
    }
    // JIT round 17 wave 1 (lane mhconst; MH7-1): the compiles that waited for
    // this handle. On the mutator, outside the memo's lock.
    mh_constant_waiters_served(shared, handle, epoch, site, &target);
    // JIT round 17 wave 3 (lane mh4): the field rows of this member that no
    // mutator has witnessed yet (written by a compile door, which may run off
    // the mutator), when their field holds this very handle now.
    mh_constant_field_rows_witnessed(shared, handle, epoch, site, &target);
}

/// Rows of [`JitBridgeState::mh_condy_members`]; past it nothing more is
/// recorded (the site keeps its dispatch helper).
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONDY_MEMBERS_MAX: usize = 4096;

/// `CRATONVM_JIT_MH_CONSTANT_CONDY` -- perf, **default OFF**, opt-in (JIT r23
/// wave 4b, lane L2; L2W4B-1 of `docs/feature-designs/jit-ir-optimizations.md`, page
/// `r23-w2-boxing-method-ref-call-costs-microseconds-FIXED-20261021.md`): the
/// optimizing tier compiles a `MethodHandle.invokeExact` / `invoke` whose
/// receiver is an `ldc` of a resolved `CONSTANT_Dynamic` holding a real
/// `DirectMethodHandle` as a plain call of the handle's member -- the
/// JDK-spun lambda forwarder that loads its implementation handle as class
/// data. The member is the handle's own (`MemberName.clazz` -> class id, its
/// name and descriptor found IN that class), not a by-name resolution from the
/// caller's loader, and the caller's access is not asked: the handle's lookup
/// already checked it. Needs `CRATONVM_JIT_LDC_CONDY_SLOT` (the slot the IR
/// receiver reads). Read per call (cold paths only).
pub(crate) fn mh_constant_condy_enabled() -> bool {
    false
}

/// Record the member of the condy `(holder, cp_idx)` just resolved to
/// `handle`, when it is a real `DirectMethodHandle` (JIT r23 wave 4b, lane L2;
/// [`mh_constant_condy_enabled`]). On the resolving mutator: the decode reads
/// the handle's final fields, so it must not run on the compile worker.
pub(crate) fn note_mh_condy_member(
    shared: &SharedVm,
    thread: &mut JvmThread,
    holder: ClassId,
    cp_idx: u16,
    handle: cratonvm_types::ObjectRef,
) {
    if !mh_constant_condy_enabled() || !mh_constant_compile_enabled(shared) {
        return;
    }
    // r23 w4b review: the row is keyed `(class, cp index)`, which a
    // redefinition re-points at another pool. Record only the entry's own
    // RECORDED result (a resolution whose record straddled a redefinition is
    // not recorded: `record_cp_constant_permanent_as_of`), and only while no
    // redefinition has begun since (`as_of`, re-checked under the rows lock,
    // which `retire_mh_condy_members_of_class` also takes after the bump).
    let as_of = crate::classloading::class_redefinition_count();
    let recorded = matches!(
        super::constants::probe_recorded_cp_constant(shared, holder, cp_idx),
        Some(cratonvm_types::Value::Object(Some(r))) if r.as_ptr() == handle.as_ptr()
    );
    if !recorded {
        return;
    }
    let member = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        cratonvm_native_builtins::lang_invoke::mh_real_direct_member(&mut ctx, handle)
    };
    let Some(member) = member else {
        return;
    };
    let mut rows = shared
        .jit
        .bridge
        .mh_condy_members
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if rows.len() < MH_CONDY_MEMBERS_MAX
        && crate::classloading::class_redefinition_count() == as_of
    {
        rows.entry((holder.as_u32(), cp_idx)).or_insert(member);
    }
}

/// r23 w4b review: drop `holder`'s [`JitBridgeState::mh_condy_members`] rows.
/// A redefinition replaced its constant pool, so `(holder, cp index)` may now
/// name another entry (or another condy whose handle is another member), and
/// an `or_insert` row would otherwise answer for it forever. Called by
/// `redefine_class_with` next to `retire_ldc_slots_of_class`, after the
/// redefinition count moved and BEFORE the code-cache flush.
pub(crate) fn retire_mh_condy_members_of_class(shared: &SharedVm, holder: ClassId) {
    let holder = holder.as_u32();
    shared
        .jit
        .bridge
        .mh_condy_members
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|&(h, _), _| h != holder);
}

/// j27 wave 7 (lane T1L2, L2W4B-1 review): drop the
/// [`JitBridgeState::mh_condy_members`] rows whose holder this unload removed.
/// The holder of a class-data condy is a hidden lambda proxy, which unloads
/// with its mirror; a `ClassId` is never reused, so its rows are dead, but
/// they still count against [`MH_CONDY_MEMBERS_MAX`], past which nothing more
/// is recorded. Called by `unload_dead_class_metadata` with the batch's ids.
pub(crate) fn retire_mh_condy_members_of_unloaded(
    shared: &SharedVm,
    unloaded: &rustc_hash::FxHashSet<u32>,
) {
    if unloaded.is_empty() {
        return;
    }
    let mut rows = shared
        .jit
        .bridge
        .mh_condy_members
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retain_condy_rows_of_live_holders(&mut rows, unloaded);
}

/// Remove every `(holder, cp index)` row whose holder is in `dead`; the count
/// removed. Pure.
pub(in crate::runtime::interpreter::jit_bridge) fn retain_condy_rows_of_live_holders<V>(
    rows: &mut std::collections::HashMap<(u32, u16), V>,
    dead: &rustc_hash::FxHashSet<u32>,
) -> usize {
    let before = rows.len();
    rows.retain(|&(h, _), _| !dead.contains(&h));
    before - rows.len()
}

/// The call [`CpResolvers::mh_constant`] answers for an `invokeExact` /
/// `invoke` site of descriptor `site` on the condy handle `m` (JIT r23 wave
/// 4b, lane L2): `(class, name, descriptor, invoke_kind, class id)`, or `None`
/// to keep the dispatch helper.
///
/// * The site must BE the handle's type (`invokeExact`'s rule; `invoke` at the
///   handle's own type is the same call).
/// * The member is looked up in its own class by id, and must be a method
///   that class declares, neither `native` nor `synchronized` (the rule the
///   field-held bind keeps), of the static-ness its kind says.
/// * `REF_invokeStatic` is an `invokestatic` row (`3`); `REF_invokeVirtual` an
///   `invokevirtual` row (`0`) of a class; `REF_invokeInterface` an
///   `invokeinterface` row (`2`) of an interface. `REF_invokeSpecial` is
///   admitted only where virtual selection cannot pick another method (a
///   `private` or `final` member, or a `final` class), as an `invokevirtual`
///   row (`0`); a true super call keeps the helper.
/// * `java/`, `jdk/`, `sun/` members keep the helper (caller-sensitive
///   methods see their caller), as the field-held bind does; so does a member
///   of a hidden class, whose name the call's by-name class context cannot
///   resolve.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_condy_answer(
    cm: &crate::classloading::ClassManager,
    m: &cratonvm_native_builtins::lang_invoke::MhDirectMember,
    site: &str,
) -> Option<(String, String, String, u8, u32)> {
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    if m.declared != site
        || ["java/", "jdk/", "sun/"]
            .iter()
            .any(|p| m.class.starts_with(p))
    {
        return None;
    }
    let class = cm.get_class(ClassId::new(m.class_id))?;
    if class.hidden || *class.name != *m.class {
        return None;
    }
    let method = class
        .methods
        .iter()
        .find(|x| *x.name == *m.name && *x.descriptor == *m.desc)?;
    let invoke_kind = mh_condy_invoke_kind(
        m.ref_kind,
        method.access_flags,
        class.is_interface(),
        class.access_flags.contains(ClassAccessFlags::FINAL),
        &m.desc,
        site,
    )?;
    Some((
        m.class.clone(),
        m.name.clone(),
        m.desc.clone(),
        invoke_kind,
        m.class_id,
    ))
}

/// The call row of [`mh_condy_answer`] for a member of handle kind
/// `ref_kind` with access `flags`, declared by an interface or not, in a
/// `final` class or not, of descriptor `desc`, at an `invokeExact` site
/// `site`: `3` an `invokestatic` row, `0` an `invokevirtual` row, `2` an
/// `invokeinterface` row; `None` keeps the dispatch helper. Pure.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_condy_invoke_kind(
    ref_kind: i32,
    flags: cratonvm_reader::class_access_flags::MethodAccessFlags,
    declared_by_interface: bool,
    class_is_final: bool,
    desc: &str,
    site: &str,
) -> Option<u8> {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    if flags.intersects(MethodAccessFlags::NATIVE | MethodAccessFlags::SYNCHRONIZED) {
        return None;
    }
    let is_static = flags.contains(MethodAccessFlags::STATIC);
    let is_abstract = flags.contains(MethodAccessFlags::ABSTRACT);
    let instance_shape = mh_constant_virtual_member_desc(site).as_deref() == Some(desc);
    match ref_kind {
        6 if is_static && !is_abstract && desc == site => Some(3),
        5 if !is_static && !is_abstract && !declared_by_interface && instance_shape => Some(0),
        9 if !is_static && declared_by_interface && instance_shape => Some(2),
        // `REF_invokeSpecial`: only where virtual selection cannot pick
        // another method.
        7 if !is_static
            && !is_abstract
            && !declared_by_interface
            && instance_shape
            && (flags.intersects(MethodAccessFlags::PRIVATE | MethodAccessFlags::FINAL)
                || class_is_final) =>
        {
            Some(0)
        }
        _ => None,
    }
}

/// Rows of [`JitBridgeState::mh_constant_field_rows`]; past it nothing more
/// is recorded (JIT round 17 wave 1, lane mhconst).
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_FIELD_ROWS_MAX: usize = 1024;

/// Fields [`JitBridgeState::mh_constant_waiters`] remembers, and waiting
/// methods per field; past either a miss is simply not remembered (round 16's
/// behaviour).
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_WAITER_FIELDS_MAX: usize = 1024;
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_WAITERS_PER_FIELD: usize = 64;

/// Recompiles [`mh_constant_waiters_served`] asks per method, and in all,
/// per VM.
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_RECOMPILES_PER_METHOD: u8 = 2;
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_RECOMPILES_MAX: usize = 4096;

/// `CRATONVM_JIT_MH_CONSTANT_RECOMPILE` -- default ON since the round-17
/// close, `=0` off; A/B in jit-optin-switch-triage-r17-CLOSED-20261006.md (JIT round 17
/// wave 1, lane mhconst; MH7-1 of `jit-r16-mh7-proposals-RETIRED-20261006.md`): an optimizing
/// compile whose constant-handle site found no admitted handle
/// ([`CpResolvers::mh_constant`]: `no admitted handle in this epoch`, the
/// shape of `R16MhFloor exact-static`, whose method compiles before any
/// compiled caller ran the site) is remembered ([`note_mh_constant_waiter`]);
/// when the constant bind then admits the handle that field holds
/// ([`note_mh_constant_target`] on the mutator, which reads the field in the
/// admission's epoch), the method is recompiled once
/// ([`mh_constant_waiters_served`]) and the body it displaces is made not
/// entrant (the late-bind recompile's publication rule). Read once per VM.
pub(crate) fn mh_constant_recompile_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .mh_constant_recompile_switch
        .get_or_init(|| {
            true
        })
}

/// `CRATONVM_JIT_MH_CONSTANT_FIELD_ROWS` -- default OFF, opt-in (JIT round 17
/// wave 1, lane mhconst; MH7-2 of `jit-r16-mh7-proposals-RETIRED-20261006.md`): a `static
/// final` field once READ holding an admitted handle, in that admission's
/// collection epoch, answers the compile doors by the FIELD from then on
/// ([`JitBridgeState::mh_constant_field_rows`]), so a collection between the
/// constant bind's fill and the compile no longer costs the plan. The trust is
/// the one the compiled call already takes: a `static final` field of an
/// initialized class is written by nothing but `Unsafe` and JNI (JVMS §6.5;
/// reflection refuses static finals), and a body compiled from it never
/// re-reads it either. Read once per VM.
pub(crate) fn mh_constant_field_rows_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .mh_constant_field_rows_switch
        .get_or_init(|| {
            false
        })
}

/// Record that the `static final` field `(holder, index)` holds a handle
/// admitted as `target` for call sites of descriptor `site` (MH7-2). Called
/// only where the field's value was read and compared to the admitted
/// handle's address in the admission's own collection epoch. With no witness
/// (the compile door's write, which may run off the mutator): see
/// [`note_mh_constant_field_row_with`].
pub(in crate::runtime::interpreter::jit_bridge) fn note_mh_constant_field_row(
    shared: &SharedVm,
    holder: u32,
    index: usize,
    site: &str,
    target: &MhConstantTarget,
) {
    note_mh_constant_field_row_with(shared, holder, index, site, target, None);
}



/// Release field-row witnesses (JNI weak global refs; `0` is none).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_field_row_witnesses_released(shared: &SharedVm, witnesses: &[u64]) {
    if witnesses.iter().all(|w| *w == 0) {
        return;
    }
    let mut refs = shared.natives.jni_global_refs.lock();
    for &w in witnesses.iter().filter(|w| **w != 0) {
        refs.remove(w as crate::native::jni::JObject);
    }
}

/// [`note_mh_constant_field_row`] with the handle the field was read holding
/// as the row's witness (JIT round 17 wave 3, lane mh4). `witness` must be
/// `Some` only ON A MUTATOR, read in `target`'s epoch with no safepoint since:
/// the weak reference is made from its address, and only a mutator is sure no
/// collection moved the object in between. A replaced row's witness is
/// released.
pub(in crate::runtime::interpreter::jit_bridge) fn note_mh_constant_field_row_with(
    shared: &SharedVm,
    holder: u32,
    index: usize,
    site: &str,
    target: &MhConstantTarget,
    witness: Option<ObjectRef>,
) {
    if !mh_constant_field_rows_enabled(shared) {
        return;
    }
    let weak = match witness {
        Some(obj)
            if shared.mem.address_epoch() == target.epoch =>
        {
            shared.natives.jni_global_refs.lock().add_weak(obj) as u64
        }
        _ => 0,
    };
    let released = {
        let mut rows = shared
            .jit
            .bridge
            .mh_constant_field_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (holder, index, Box::from(site));
        if rows.contains_key(&key) || rows.len() < MH_CONSTANT_FIELD_ROWS_MAX {
            rows.insert(key, (target.clone(), weak))
                .map_or(0, |(_, old)| old)
        } else {
            weak
        }
    };
    mh_constant_field_row_witnesses_released(shared, &[released]);
}

/// The member [`note_mh_constant_field_row`] recorded for the field `(holder,
/// index)` and call sites of descriptor `site` (MH7-2).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_field_row_of(
    shared: &SharedVm,
    holder: u32,
    index: usize,
    site: &str,
) -> Option<MhConstantTarget> {
    mh_constant_field_row_entry_of(shared, holder, index, site).map(|(t, _)| t)
}

/// [`mh_constant_field_row_of`] with the row's witness (`0`: none).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_field_row_entry_of(
    shared: &SharedVm,
    holder: u32,
    index: usize,
    site: &str,
) -> Option<(MhConstantTarget, u64)> {
    if !mh_constant_field_rows_enabled(shared) {
        return None;
    }
    let rows = shared
        .jit
        .bridge
        .mh_constant_field_rows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    rows.get(&(holder, index, Box::from(site))).cloned()
}

/// Drop the field row `(holder, index, site)` when it is still `target`, and
/// release its witness (JIT round 17 wave 3, lane mh4).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_field_row_dropped(
    shared: &SharedVm,
    holder: u32,
    index: usize,
    site: &str,
    target: &MhConstantTarget,
) {
    let released = {
        let mut rows = shared
            .jit
            .bridge
            .mh_constant_field_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (holder, index, Box::from(site));
        if rows.get(&key).is_some_and(|(t, _)| t == target) {
            rows.remove(&key).map_or(0, |(_, w)| w)
        } else {
            0
        }
    };
    mh_constant_field_row_witnesses_released(shared, &[released]);
}

/// Does the witness `weak` of a field row name, in collection epoch `epoch`,
/// the object at `handle` (the field's value, read in that same epoch)? `None`
/// with no witness, or when a collection ran during the question; `Some(false)`
/// when it names another address or was cleared: the field no longer holds the
/// admitted handle (it holds it strongly, so a cleared witness means a
/// rewrite). No heap object is dereferenced (a JNI weak slot holds the
/// collector's remapped address), so a background compile may ask. An equal
/// address in an equal epoch is the same object; an unequal answer inside a
/// concurrent cycle only costs the row (JIT round 17 wave 3, lane mh4).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_field_row_witness_holds(
    shared: &SharedVm,
    weak: u64,
    epoch: u64,
    handle: u64,
) -> Option<bool> {
    if weak == 0 {
        return None;
    }
    let realm = &shared.mem;
    if realm.address_epoch() != epoch {
        return None;
    }
    let now = shared
        .natives
        .jni_global_refs
        .lock()
        .resolve(weak as crate::native::jni::JObject);
    if realm.address_epoch() != epoch {
        return None;
    }
    Some(now.is_some_and(|o| o.as_ptr() as u64 == handle))
}

/// The same member, admitted the same way (the epoch aside).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_same_member(a: &MhConstantTarget, b: &MhConstantTarget) -> bool {
    (&a.class, &a.name, &a.desc, a.class_id, a.invoke_kind)
        == (&b.class, &b.name, &b.desc, b.class_id, b.invoke_kind)
        // JIT round 21 wave 19 (lane mhbind2): a bound row's receiver too.
        && a.bound == b.bound
        // JIT round 21 wave 20 (lane mhfilter): a filter row's PRE call too.
        && a.filter == b.filter
}

/// The constant bind just admitted the handle at `handle`, in collection epoch
/// `epoch`, for call sites of descriptor `site`, as `target` (ON THE MUTATOR:
/// the fills' own thread). Every field row of that site and member that has no
/// witness, or one of an older epoch, is read now: a field holding this very
/// handle gets the handle as its witness and the row this epoch, so the row
/// answers across later collections (JIT round 17 wave 3, lane mh4). Every
/// read is bracketed by the collection count. Cold: once per fill, and one
/// lock when no row is stale.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_field_rows_witnessed(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    target: &MhConstantTarget,
) {
    if !mh_constant_field_rows_enabled(shared) {
        return;
    }
    let stale: Vec<(u32, usize)> = {
        let rows = shared
            .jit
            .bridge
            .mh_constant_field_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.iter()
            .filter(|((_, _, s), (t, w))| {
                **s == *site && mh_constant_same_member(t, target) && (*w == 0 || t.epoch != epoch)
            })
            .map(|((holder, index, _), _)| (*holder, *index))
            .collect()
    };
    let realm = &shared.mem;
    for (holder, index) in stale {
        if realm.address_epoch() != epoch {
            return;
        }
        let value = crate::vm::get_static_shared(shared, ClassId::new(holder), index);
        if realm.address_epoch() != epoch {
            return;
        }
        let cratonvm_types::Value::Object(Some(h)) = value else {
            continue;
        };
        if h.as_ptr() as u64 != handle {
            continue;
        }
        let weak = shared.natives.jni_global_refs.lock().add_weak(h) as u64;
        let released = {
            let mut rows = shared
                .jit
                .bridge
                .mh_constant_field_rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match rows.get_mut(&(holder, index, Box::from(site))) {
                Some((t, w)) if mh_constant_same_member(t, target) => {
                    t.epoch = epoch;
                    std::mem::replace(w, weak)
                }
                _ => weak,
            }
        };
        mh_constant_field_row_witnesses_released(shared, &[released]);
    }
}

// ---------------------------------------------------------------------------
// JIT round 22 (lane memory, FFM22-1): the compiled call of a constant bound
// FFM downcall (`cratonvm_jit::ir_lower::FfmConstRow`)
// ---------------------------------------------------------------------------
//
// The optimizing tier makes an `invokeExact` of a `static final` bound
// downcall handle as the C call itself when the VM files a row for the field
// (`Lowerer::try_emit_ir_ffm_const_prefix`). The row is the handle's direct
// stub (`panama::JitDowncallDirect`), which only the bound row's fill computes
// (a mutator, after the handle's own first call classified it). So: every
// successful direct call files its stub by `(handle address, collection
// epoch)` ([`ffm_const_note_fill`]); the compile door that reads the field
// (`CpResolvers::mh_constant`, on the compiling thread) makes the field's row
// when the address it reads now is a filed one of this epoch (an equal
// address in an equal epoch is the same object) and otherwise leaves the
// method waiting; a fill that finds a waited-on field holding its handle makes
// the row and asks one recompile of each waiter (the MH7-1 late-bind
// recompile, under its own caps). The field is `static final` of an
// initialised class (the door's own checks), so its handle -- and the stub's
// target, kinds and criticality -- never change: a row is never retired.

/// The most stubs [`FfmConstState::by_handle`] keeps (cleared when full: the
/// next fill files again), and the most fields that may wait or have rows.
pub(in crate::runtime::interpreter::jit_bridge) const FFM_CONST_STUBS_MAX: usize = 256;
pub(in crate::runtime::interpreter::jit_bridge) const FFM_CONST_FIELDS_MAX: usize = 1024;

/// Per-VM state of the constant FFM compiled call.
#[derive(Default)]
pub(crate) struct FfmConstState {
    /// The direct stubs the bound row called, with the descriptor of the site
    /// it was bound for, by `(handle address, collection epoch)`.
    pub(in crate::runtime::interpreter::jit_bridge) by_handle: std::sync::Mutex<std::collections::HashMap<(u64, u64), FfmConstFiled>>,
    /// The rows, by `(holder class id, static index)`. Boxed: compiled code
    /// and compiles read them by address for the VM's life.
    pub(in crate::runtime::interpreter::jit_bridge) rows: std::sync::Mutex<std::collections::HashMap<(u32, usize), Box<FfmConstEntry>>>,
    /// Compiles that found no filed stub for the field's handle.
    pub(in crate::runtime::interpreter::jit_bridge) waiters: std::sync::Mutex<std::collections::HashMap<(u32, usize), Vec<LateBindWaiter>>>,
    /// `waiters.len()`, written under its lock.
    pub(in crate::runtime::interpreter::jit_bridge) waiter_count: std::sync::atomic::AtomicUsize,
}

/// A stub the bound row filed ([`FfmConstState::by_handle`]): the stub, the
/// descriptor of the site it was bound for, the site's native slot and return
/// descriptor character.
#[derive(Clone)]
pub(in crate::runtime::interpreter::jit_bridge) struct FfmConstFiled {
    pub(in crate::runtime::interpreter::jit_bridge) stub: cratonvm_native_builtins::panama::JitDowncallDirect,
    pub(in crate::runtime::interpreter::jit_bridge) site: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) native_id: Option<cratonvm_native_api::NativeMethodId>,
    pub(in crate::runtime::interpreter::jit_bridge) site_ret: u8,
}

/// A filed row (first, so the row's address is the entry's), the stub it was
/// made from (steps (b) and (c)'s helpers call it), the site's native slot
/// and return descriptor character (step (c)'s call reads them) and the
/// descriptor its `desc_ptr` names.
#[repr(C)]
pub(crate) struct FfmConstEntry {
    pub(crate) row: cratonvm_jit::ir_lower::FfmConstRow,
    pub(crate) stub: cratonvm_native_builtins::panama::JitDowncallDirect,
    pub(crate) native_id: Option<cratonvm_native_api::NativeMethodId>,
    pub(crate) site_ret: u8,
    pub(in crate::runtime::interpreter::jit_bridge) _desc: Box<str>,
}

/// Does any compiled-call switch want rows? (`CRATONVM_JIT_IR_FFM_CRITICAL_CALL`
/// for critical handles; `CRATONVM_JIT_IR_FFM_LAZY_CALL` for the others.)
pub(in crate::runtime::interpreter::jit_bridge) fn ffm_const_wanted(stub: &cratonvm_native_builtins::panama::JitDowncallDirect) -> bool {
    if stub.compiled_call_shape().is_none() {
        cratonvm_jit::ir_lower::ir_ffm_segment_call_enabled()
    } else if stub.critical() {
        cratonvm_jit::ir_lower::ir_ffm_critical_call_enabled()
    } else {
        cratonvm_jit::ir_lower::ir_ffm_lazy_call_enabled()
    }
}

/// Make the row of field `key` for `stub`, filed for sites of `site` (under
/// the caller's `rows` lock). `None` when the stub has no compiled shape.
pub(in crate::runtime::interpreter::jit_bridge) fn ffm_const_make_row(
    shared: &SharedVm,
    rows: &mut std::collections::HashMap<(u32, usize), Box<FfmConstEntry>>,
    key: (u32, usize),
    filed: &FfmConstFiled,
) -> Option<i64> {
    use cratonvm_jit::ir_lower as jl;
    if let Some(entry) = rows.get(&key) {
        return Some(std::ptr::addr_of!(entry.row) as i64);
    }
    if rows.len() >= FFM_CONST_FIELDS_MAX {
        return None;
    }
    let stub = &filed.stub;
    let guards = cratonvm_native_builtins::panama::compiled_call_access_guard(shared.vm_identity)?;
    let desc: Box<str> = Box::from(&*filed.site);
    let critical = if stub.critical() {
        jl::FFM_CONST_CRITICAL
    } else {
        0
    };
    // FFM22-3: the segment stub whose one segment argument a compiled caller
    // may check itself (its slot count is its arity: no lead slot).
    let seg_shape = stub
        .segment_call_shape()
        .filter(|shape| usize::from(shape.arity) == stub.slot_count());
    // Step (a) / (b): a call the compiled caller can make; step (c): one only
    // the bound row's own call makes, through its helper.
    let (arity, params, ret, flags, call_helper) = match stub.compiled_call_shape() {
        Some(shape) => (
            shape.arity,
            shape.params,
            shape.ret,
            critical,
            if stub.critical() {
                0
            } else {
                crate::jit::helpers::jit_ffm_const_lazy_call as usize as u64
            },
        ),
        None => {
            let ret = match filed.site_ret {
                b'V' => jl::FFM_CONST_VOID,
                b'J' => jl::FFM_CONST_W64,
                b'I' | b'S' | b'C' | b'B' | b'Z' => jl::FFM_CONST_S32,
                b'L' | b'[' => jl::FFM_CONST_REF,
                _ => return None,
            };
            // Cast: a slot count of at most the plain encoding's eight.
            let slots = u8::try_from(stub.slot_count()).ok()?;
            // FFM22-3: a stub with one segment parameter keeps its codes (the
            // segment helper widens by them); its return code is the shape's,
            // which reads a narrow primitive as the stub returns it.
            let (params, ret) = match seg_shape {
                Some(shape) => (shape.params, shape.ret),
                None => ([0; 6], ret),
            };
            (
                slots,
                params,
                ret,
                critical | jl::FFM_CONST_HELPER_ONLY,
                crate::jit::helpers::jit_ffm_const_direct_call as usize as u64,
            )
        }
    };
    // JIT round 22 (lane memory, FFM22-3): the codes are `panama`'s, copied.
    const _: () = assert!(cratonvm_native_builtins::panama::COMPILED_CALL_REF == jl::FFM_CONST_REF);
    let (seg_call_helper, seg_param) = match seg_shape {
        Some(shape) => (
            crate::jit::helpers::jit_ffm_const_seg_call as usize as u64,
            shape.segment,
        ),
        None => (0, jl::FFM_CONST_NO_SEGMENT),
    };
    let row = jl::FfmConstRow {
        fn_addr: stub.fn_addr() as u64,
        arity,
        params,
        ret,
        flags,
        guard_words: [guards[0].0, guards[1].0],
        guard_values: [guards[0].1, guards[1].1],
        call_helper,
        desc_ptr: desc.as_ptr() as u64,
        desc_len: desc.len() as u64,
        seg_call_helper,
        seg_param,
    };
    let entry = Box::new(FfmConstEntry {
        row,
        stub: *stub,
        native_id: filed.native_id,
        site_ret: filed.site_ret,
        _desc: desc,
    });
    let addr = std::ptr::addr_of!(entry.row) as i64;
    rows.insert(key, entry);
    Some(addr)
}

/// The bound row just made the direct call of `handle` (validated in epoch
/// `epoch`) through `stub`, for a site of descriptor `site` (FFM22-1). Files
/// the stub; a field some compile waited on that holds this handle now gets
/// its row, and each waiter one recompile. On the mutator. A fill already
/// filed, or one no switch wants, is one lock (or nothing).
pub(crate) fn ffm_const_note_fill(
    shared: &SharedVm,
    handle: ObjectRef,
    epoch: u64,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
    site: &str,
    native_id: Option<cratonvm_native_api::NativeMethodId>,
    site_ret: u8,
) {
    if !ffm_const_wanted(stub) || crate::runtime::interpreter::jvmti_requires_interpreter(shared) {
        return;
    }
    let filed = FfmConstFiled {
        stub: *stub,
        site: Box::from(site),
        native_id,
        site_ret,
    };
    let state = &shared.jit.bridge.ffm_const;
    let addr = handle.as_ptr() as u64;
    {
        let mut by_handle = state
            .by_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if by_handle.contains_key(&(addr, epoch)) {
            return;
        }
        if by_handle.len() >= FFM_CONST_STUBS_MAX {
            by_handle.clear();
        }
        by_handle.insert((addr, epoch), filed.clone());
    }
    // The compiled call makes no invocation tally: the slot's count is a floor.
    if let Some(id) = native_id {
        shared
            .natives
            .native_methods
            .mark_invocations_incomplete(id);
    }
    if state
        .waiter_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return;
    }
    let candidates: Vec<(u32, usize)> = state
        .waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .copied()
        .collect();
    let realm = &shared.mem;
    let mut holding: Vec<(u32, usize)> = Vec::new();
    for (holder, index) in candidates {
        if realm.address_epoch() != epoch {
            return;
        }
        let value = crate::vm::get_static_shared(shared, ClassId::new(holder), index);
        if realm.address_epoch() != epoch {
            return;
        }
        if matches!(value, cratonvm_types::Value::Object(Some(h)) if h.as_ptr() as u64 == addr) {
            holding.push((holder, index));
        }
    }
    if holding.is_empty() {
        return;
    }
    let mut waiting: Vec<LateBindWaiter> = Vec::new();
    {
        let mut rows = state
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut waiters = state
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for &key in &holding {
            if ffm_const_make_row(shared, &mut rows, key, &filed).is_none() {
                continue;
            }
            if let Some(list) = waiters.remove(&key) {
                for w in list {
                    if !waiting.contains(&w) {
                        waiting.push(w);
                    }
                }
            }
        }
        state
            .waiter_count
            .store(waiters.len(), std::sync::atomic::Ordering::Release);
    }
    if waiting.is_empty()
        || !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::c2_supersede()
    {
        return;
    }
    for waiter in waiting {
        if !mh_constant_recompile_admitted(shared, &waiter) {
            continue;
        }
        if cratonvm_native_builtins::panama::ffm_dbg_enabled() {
            eprintln!(
                "[ffm-const] recompile {}.{}{}: a waited-on field holds a filed stub",
                waiter.class_name, waiter.method_name, waiter.descriptor,
            );
        }
        late_bind_mark_and_request(shared, &waiter);
    }
}

/// The compile door read the `static final` field `(holder, index)` holding
/// the object at `addr` in epoch `epoch`, for an `invokeExact` of descriptor
/// `site` (FFM22-1): make its row when the bound row filed that object's stub
/// in this epoch for this descriptor, else leave `waiter()` waiting. On the
/// compiling thread; never reads the object.
pub(in crate::runtime::interpreter::jit_bridge) fn ffm_const_note_compile(
    shared: &SharedVm,
    holder: u32,
    index: usize,
    addr: u64,
    epoch: u64,
    site: &str,
    waiter: impl FnOnce() -> Option<LateBindWaiter>,
) {
    if !cratonvm_jit::ir_lower::ir_ffm_critical_call_enabled()
        && !cratonvm_jit::ir_lower::ir_ffm_lazy_call_enabled()
        && !cratonvm_jit::ir_lower::ir_ffm_segment_call_enabled()
    {
        return;
    }
    let state = &shared.jit.bridge.ffm_const;
    let key = (holder, index);
    let filed = state
        .by_handle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(addr, epoch))
        .cloned();
    let mut rows = state
        .rows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if rows.contains_key(&key) {
        return;
    }
    if let Some(filed) = filed {
        if *filed.site == *site && shared.mem.address_epoch() == epoch {
            ffm_const_make_row(shared, &mut rows, key, &filed);
        }
        return;
    }
    drop(rows);
    let Some(waiting) = waiter() else {
        return;
    };
    let mut waiters = state
        .waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if waiters.len() >= FFM_CONST_FIELDS_MAX && !waiters.contains_key(&key) {
        return;
    }
    let list = waiters.entry(key).or_default();
    if list.len() < MH_CONSTANT_WAITERS_PER_FIELD && !list.contains(&waiting) {
        list.push(waiting);
    }
    state
        .waiter_count
        .store(waiters.len(), std::sync::atomic::Ordering::Release);
}

/// The address of the row filed for the `static final` field `(holder,
/// index)` (`cratonvm_jit::ir_lower::WELL_KNOWN_FFM_CONST_ROW_TAG`), `0` when
/// there is none.
pub(crate) fn ffm_const_row_addr(shared: &SharedVm, holder: u32, index: u32) -> i64 {
    shared
        .jit
        .bridge
        .ffm_const
        .rows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(holder, index as usize))
        .map_or(0, |entry| std::ptr::addr_of!(entry.row) as i64)
}

/// Remember that the optimizing compile of `waiting` found no admitted handle
/// in the `static final` field `(holder, index)` for its call site of
/// descriptor `site` (MH7-1). Cold: one compile-time miss.
pub(in crate::runtime::interpreter::jit_bridge) fn note_mh_constant_waiter(
    shared: &SharedVm,
    holder: u32,
    index: usize,
    site: &str,
    waiting: LateBindWaiter,
) {
    if !mh_constant_recompile_enabled(shared) {
        return;
    }
    let bridge = &shared.jit.bridge;
    let mut waiters = bridge
        .mh_constant_waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (holder, index, Box::from(site));
    if waiters.len() >= MH_CONSTANT_WAITER_FIELDS_MAX && !waiters.contains_key(&key) {
        return;
    }
    let list = waiters.entry(key).or_default();
    if list.len() >= MH_CONSTANT_WAITERS_PER_FIELD || list.contains(&waiting) {
        return;
    }
    list.push(waiting);
    bridge
        .mh_constant_waiter_count
        .store(waiters.len(), std::sync::atomic::Ordering::Release);
}

/// The constant bind just admitted the handle at `handle`, in collection
/// epoch `epoch`, for call sites of descriptor `site`, as `target` (MH7-1).
/// Every waiting field of that descriptor is read now, on this mutator: one
/// that holds this very handle, both reads inside `epoch` (an equal address in
/// an equal epoch is the same object), gets its field row (MH7-2), and every
/// method that waited on it is asked one more optimizing compile, which now
/// finds the admission. The request is the late-bind recompile's
/// (`request_late_bind_recompile`), and so is the publication rule: the body
/// it displaces is made not entrant ([`late_bind_mark_pending`]).
///
/// One relaxed load when nothing waits.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_waiters_served(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
    target: &MhConstantTarget,
) {
    let bridge = &shared.jit.bridge;
    if bridge
        .mh_constant_waiter_count
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return;
    }
    let candidates: Vec<(u32, usize)> = {
        let waiters = bridge
            .mh_constant_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        waiters
            .keys()
            .filter(|(_, _, s)| **s == *site)
            .map(|(holder, index, _)| (*holder, *index))
            .collect()
    };
    let realm = &shared.mem;
    let mut holding: Vec<(u32, usize)> = Vec::new();
    // JIT round 17 wave 3 (lane mh4): the handle as the fields hold it, the
    // witness of each field row written below (this is a mutator).
    let mut held: Option<ObjectRef> = None;
    for (holder, index) in candidates {
        if realm.address_epoch() != epoch {
            return;
        }
        let value = crate::vm::get_static_shared(shared, ClassId::new(holder), index);
        if realm.address_epoch() != epoch {
            return;
        }
        if let cratonvm_types::Value::Object(Some(h)) = value {
            if h.as_ptr() as u64 == handle {
                holding.push((holder, index));
                held = Some(h);
            }
        }
    }
    if holding.is_empty() {
        return;
    }
    let mut waiting: Vec<LateBindWaiter> = Vec::new();
    {
        let mut waiters = bridge
            .mh_constant_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for &(holder, index) in &holding {
            if let Some(list) = waiters.remove(&(holder, index, Box::from(site))) {
                for w in list {
                    if !waiting.contains(&w) {
                        waiting.push(w);
                    }
                }
            }
        }
        bridge
            .mh_constant_waiter_count
            .store(waiters.len(), std::sync::atomic::Ordering::Release);
    }
    for &(holder, index) in &holding {
        note_mh_constant_field_row_with(shared, holder, index, site, target, held);
    }
    // With no background worker nothing would run the task, and with no C1 ->
    // C2 supersede the recompile could not replace the published body (the
    // late-bind recompile's own gate).
    if waiting.is_empty()
        || !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::c2_supersede()
    {
        return;
    }
    for waiter in waiting {
        if !mh_constant_recompile_admitted(shared, &waiter) {
            continue;
        }
        if crate::jit::helpers::mh_constant_bind_dbg() {
            eprintln!(
                "[mh-bind] recompile {}.{}{}: a waited-on field holds {}.{}{}",
                waiter.class_name,
                waiter.method_name,
                waiter.descriptor,
                target.class,
                target.name,
                target.desc,
            );
        }
        late_bind_mark_and_request(shared, &waiter);
    }
}

/// Count one [`mh_constant_waiters_served`] recompile of `waiter`: `false`
/// past [`MH_CONSTANT_RECOMPILES_PER_METHOD`] for it or
/// [`MH_CONSTANT_RECOMPILES_MAX`] methods in all.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_recompile_admitted(shared: &SharedVm, waiter: &LateBindWaiter) -> bool {
    let mut asked = shared
        .jit
        .bridge
        .mh_constant_recompiled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (
        waiter.class_id,
        Arc::clone(&waiter.method_name),
        Arc::clone(&waiter.descriptor),
    );
    if !asked.contains_key(&key) && asked.len() >= MH_CONSTANT_RECOMPILES_MAX {
        return false;
    }
    let count = asked.entry(key).or_insert(0);
    if *count >= MH_CONSTANT_RECOMPILES_PER_METHOD {
        return false;
    }
    *count += 1;
    true
}

/// `CRATONVM_JIT_MH_CONSTANT_LOADER_CHECK` -- correctness, default ON, `=0`
/// restores round 16's door (JIT round 17 wave 1, lane mhconst): see
/// [`mh_constant_descriptor_agrees`]. Read per question (compile time only).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_loader_check_enabled() -> bool {
    true
}

/// The class names of `desc`'s reference components (parameters and return),
/// array elements unwrapped; primitives are skipped.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_descriptor_class_names(desc: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = desc;
    while let Some(at) = rest.find('L') {
        let tail = &rest[at + 1..];
        let Some(end) = tail.find(';') else {
            break;
        };
        names.push(&tail[..end]);
        rest = &tail[end + 1..];
    }
    names
}

/// Does every class `desc` names resolve, from the calling class `caller` and
/// from the member's class `member`, to ONE class (JIT round 17 wave 1, lane
/// mhconst)? The constant bind admits a handle when its type's descriptor
/// STRING is the call site's (`lang_invoke::mh_compiled_bind_target`), and a
/// string is loader-blind: a handle of `(LBox;)I` minted against one loader's
/// `Box` and invoked from a class whose `Box` is another loader's is a
/// `WrongMethodTypeException` on HotSpot (the site's type is resolved by the
/// caller's loader, JVMS §5.4.3.6), never a call. Compiled as a plain call it
/// would hand the member an object of the wrong class. The JVMS §5.3.4
/// loader-constraint question, asked of both loaders without loading: an
/// unresolved name on either side is a refusal (the site keeps its door).
/// Exactly the descriptors whose components are all primitives, or whose
/// classes both sides already see as one, compile.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_descriptor_agrees(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    member: ClassId,
    desc: &str,
) -> bool {
    if caller == member || !mh_constant_loader_check_enabled() {
        return true;
    }
    mh_constant_descriptor_class_names(desc)
        .into_iter()
        .all(|name| {
            let from_caller = class_resolved_without_loading(shared, cm, caller, name);
            let from_member = class_resolved_without_loading(shared, cm, member, name);
            from_caller.is_some() && from_caller == from_member
        })
}

/// `CRATONVM_JIT_MH_CONSTANT_NONPUBLIC` -- default OFF, opt-in (JIT round 17
/// wave 2, lane mh3; MHCONST17-3 of `jit-r17-mhconst-proposals-RETIRED-20261006.md`, widened):
/// see [`mh_constant_nonpublic_member_callable`]. Read per question (compile
/// time only).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_nonpublic_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_MH_CONSTANT_VIRTUAL` -- default ON, `=0` off (built opt-in in
/// JIT round 17 wave 3, lane mh4; MHCONST17-2 of `jit-r17-mhconst-proposals-RETIRED-20261006.md`, MH317-1):
/// the constant bind's fills record an exact unbound VIRTUAL handle too
/// (`helpers.rs` `mh_constant_bind_fill`, [`mh_constant_interpreter_fill`]),
/// and the compile door ([`CpResolvers::mh_constant`]) answers it as
/// `invoke_kind 0`: the optimizing tier compiles the `invokeExact` on a
/// `static final` `findVirtual` handle as a guarded `invokevirtual` of its
/// member (`ir_mh_constant_plan`, `IrBuilder::mh_constant_opcode`), whose
/// `null` receiver re-executes the `invokeExact` in the interpreter. An
/// interface member is not admitted (the bind's admission refuses interface
/// classes). Read per question (cold fills and compile time only).
pub(crate) fn mh_constant_virtual_enabled() -> bool {
    true
}

/// The member descriptor an unbound virtual handle's `invokeExact` site of
/// descriptor `site` calls: the site less its leading receiver (one `L...;`).
/// `None` when the site does not start with a class reference (JIT round 17
/// wave 3, lane mh4).
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_virtual_member_desc(site: &str) -> Option<String> {
    let rest = site.strip_prefix("(L")?;
    let end = rest.find(';')?;
    (end > 0).then(|| format!("({}", &rest[end + 1..]))
}

/// `CRATONVM_JIT_MH_CONSTANT_INTERFACE` -- perf, **default ON** since JIT
/// round 20 (`=0` is the kill switch; 480 -> 50 ns on `R17Mh4Virtual
/// exact-interface`, 10x on every `R19MhifaceExact` row; JIT round 19 wave 1,
/// lane mhiface; MHRESID18-4, the compile-door half of
/// MH417-1, `r18-mh-interface-handle-invokeexact-costs-microseconds-FIXED-20261007.md`):
/// the constant bind's fills record an unbound `findVirtual` handle of an
/// INTERFACE method too (`invoke_kind 2`; the dispatch helpers' fill admits it
/// under `CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE`, the interpreter's under
/// this switch), and the compile door ([`CpResolvers::mh_constant`]) answers
/// it: the optimizing tier compiles the `invokeExact` on a `static final`
/// such handle as an `invokeinterface` of its member, behind an `instanceof`
/// guard of the interface on the receiver that deopts at the invoke
/// (`IrBuilder::mh_constant_opcode`), so a `null` or a receiver that does not
/// implement the interface meets the handle's own door. Read per question
/// (cold fills and compile time only).
pub(crate) fn mh_constant_interface_enabled() -> bool {
    true
}

/// The word the `[mh-bind]` lines print for an admitted member's call kind.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_kind_word(invoke_kind: u8) -> &'static str {
    match invoke_kind {
        3 => "static",
        2 => "interface",
        // JIT round 21 wave 19 (lane mhbind2; MHBIND21-1).
        4 => "bound",
        // j27 wave 6 (lane T1LAM2).
        6 => "bound-static",
        // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1).
        5 => "filter",
        _ => "virtual",
    }
}

/// May the fills record a member of `invoke_kind` (JIT round 17 wave 3, lane
/// mh4): a static one always, an unbound virtual one with
/// [`mh_constant_virtual_enabled`], an unbound interface one with
/// [`mh_constant_interface_enabled`] (JIT round 19 wave 1, lane mhiface).
pub(crate) fn mh_constant_records_kind(invoke_kind: u8) -> bool {
    invoke_kind == 3
        || (invoke_kind == 0 && mh_constant_virtual_enabled())
        || (invoke_kind == 2 && mh_constant_interface_enabled())
}

/// May the class `caller` call the NON-public static member of `member_class`
/// (access `flags`) as a plain `invokestatic` (JIT round 17 wave 2, lane mh3)?
/// The compile door admitted only a public member or one of the caller's own
/// class, so a `static final` handle of a package-private helper of another
/// class of the package, or of a private static of a nested class (`R16Mh7Const
/// exact-private`), stayed on the dispatch helper. JVMS §5.4.4 lets the
/// caller's own `invokestatic` reach both: a private member from a nestmate
/// (JEP 181, `access_control::are_nestmates`, which also requires one loader
/// and one run-time package), a package-private or protected one from the same
/// run-time package (`access_control::same_runtime_package`: one defining
/// loader, one package name). Cross-package protected access is not taken.
/// `false` with the switch off.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_nonpublic_member_callable(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    member_class: &crate::classloading::Class,
    flags: cratonvm_reader::class_access_flags::MethodAccessFlags,
) -> bool {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    if !mh_constant_nonpublic_enabled() {
        return false;
    }
    let Some(caller_class) = cm.get_class(caller) else {
        return false;
    };
    if flags.contains(MethodAccessFlags::PRIVATE) {
        crate::classloading::access_control::are_nestmates(
            caller_class,
            member_class,
            cm.class_store(),
        )
    } else {
        crate::classloading::access_control::same_runtime_package(caller_class, member_class)
    }
}

/// The member [`note_mh_constant_target`] recorded for the handle at `handle`
/// and a call site of descriptor `site`, admitted in collection epoch `epoch`.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_target_of(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    site: &str,
) -> Option<MhConstantTarget> {
    let rows = shared
        .jit
        .bridge
        .mh_constant_targets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    rows.get(&(handle, Box::from(site)))
        .filter(|t| t.epoch == epoch)
        .cloned()
}

/// May a call site of descriptor `site`, in the class with raw id `caller`,
/// call the member of class `member` that the constant bind admitted (JIT
/// round 17 wave 2, lane mh2; the dispatch helpers' half of
/// `r17-mh-constant-bind-admission-is-loader-blind-FIXED-20261005.md`)? The
/// admission compares descriptor STRINGS; this is the compile door's loader
/// question ([`mh_constant_descriptor_agrees`]) asked of the helper's site:
/// every class `site` names (receiver, parameters, return) must resolve,
/// without loading, to one class from the caller and from the member. A
/// primitive-only site never asks. `caller == 0` (a site with no class
/// context) is admitted only for `java/` names, which one loader defines.
/// Same switch as the compile door (`CRATONVM_JIT_MH_CONSTANT_LOADER_CHECK`).
/// Cold: once per fill.
pub(crate) fn mh_constant_bind_loaders_agree(
    shared: &SharedVm,
    caller: u32,
    member: u32,
    site: &str,
) -> bool {
    let names = mh_constant_descriptor_class_names(site);
    if names.is_empty() || !mh_constant_loader_check_enabled() {
        return true;
    }
    if caller == 0 {
        return names.iter().all(|n| n.starts_with("java/"));
    }
    let cm = shared.classes.class_manager.read();
    mh_constant_descriptor_agrees(
        shared,
        &cm,
        ClassId::new(caller),
        ClassId::new(member),
        site,
    )
}

/// `CRATONVM_JIT_MH_CONSTANT_IMMUTABLE_CHECK` -- check, default ON, `=0`
/// turns it off (JIT round 17 wave 2, lane mh2; MH7-3 of
/// `jit-r16-mh7-proposals-RETIRED-20261006.md`). The compile-time bind
/// (`CRATONVM_JIT_MH_CONSTANT_COMPILE`, default ON) never re-reads a handle
/// it compiled as its member, where the helper's per-call guard re-read
/// `MH_COMPILED_BIND_GUARD_SLOTS` on every call. By reading (round 17 wave 2,
/// see the r12w7-mh5 page) every in-place writer of those slots and of `type`
/// runs on a handle before it escapes, or on one too narrow to be admitted.
/// This checks it on the mutator: a cold fill that finds an admitted handle
/// (same address, same epoch) with other words than at its admission drops
/// the handle's memo rows and the field rows naming its member, so no later
/// compile trusts it ([`mh_constant_guard_verify`]). Read per fill (cold).
pub(crate) fn mh_constant_immutable_check_enabled() -> bool {
    true
}

/// `CRATONVM_DBG_JIT_MH_BIND=2` (MH7-3's debug level): every cold fill also
/// re-reads EVERY admitted handle of the current epoch
/// ([`mh_constant_guard_sweep`]) and prints each one rewritten in place.
pub(crate) fn mh_constant_guard_sweep_dbg() -> bool {
    matches!(
        None::<&str>,
        Some("2")
    )
}

/// Record the words of the handle at `handle` the constant bind just admitted
/// in collection epoch `epoch` (MH7-3). Cold: next to each
/// [`note_mh_constant_target`].
pub(crate) fn note_mh_constant_guard(shared: &SharedVm, handle: u64, epoch: u64, words: [u64; 7]) {
    let mut rows = shared
        .jit
        .bridge
        .mh_constant_guards
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let room = rows.contains_key(&handle) || rows.len() < MH_CONSTANT_TARGETS_MAX || {
        rows.retain(|_, (e, _)| *e == epoch);
        rows.len() < MH_CONSTANT_TARGETS_MAX
    };
    if room {
        rows.insert(handle, (epoch, words));
    }
}

/// The cold fill found the handle at `handle` in collection epoch `epoch`;
/// `now` reads its words (`None`: unreadable as a wide handle), asked only
/// when the handle was admitted in this epoch. `true` when its words changed
/// since its admission: an in-place write the compile-time bind does not see.
/// Its rows are then dropped ([`mh_constant_guard_violated`]). Cold: once per
/// fill, one lock.
pub(crate) fn mh_constant_guard_verify(
    shared: &SharedVm,
    handle: u64,
    epoch: u64,
    now: impl FnOnce() -> Option<[u64; 7]>,
) -> bool {
    let recorded = {
        let rows = shared
            .jit
            .bridge
            .mh_constant_guards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.get(&handle)
            .filter(|(e, _)| *e == epoch)
            .map(|(_, w)| *w)
    };
    match recorded {
        Some(words) if now() != Some(words) => {
            mh_constant_guard_violated(shared, handle, "fill");
            true
        }
        _ => false,
    }
}

/// `CRATONVM_DBG_JIT_MH_BIND=2`: re-read every admitted handle of epoch
/// `epoch` through `read` (the handle's words now, on this mutator; an equal
/// address in an equal epoch is the admitted object) and report each one
/// whose words changed. Every read is bracketed by the collection count.
pub(crate) fn mh_constant_guard_sweep(
    shared: &SharedVm,
    epoch: u64,
    read: &dyn Fn(u64) -> Option<[u64; 7]>,
) {
    let rows: Vec<(u64, [u64; 7])> = {
        let rows = shared
            .jit
            .bridge
            .mh_constant_guards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.iter()
            .filter(|(_, (e, _))| *e == epoch)
            .map(|(h, (_, w))| (*h, *w))
            .collect()
    };
    let realm = &shared.mem;
    for (handle, words) in rows {
        if realm.address_epoch() != epoch {
            return;
        }
        let now = read(handle);
        if realm.address_epoch() != epoch {
            return;
        }
        if now != Some(words) {
            mh_constant_guard_violated(shared, handle, "sweep");
        }
    }
}

/// An admitted handle was rewritten in place (MH7-3): forget it. Its address
/// memo rows (every site) and its guard row go, and so does every field row
/// naming one of its members, so neither door compiles it again.
///
/// JIT round 17 wave 2 (lane mh3): and the bodies already compiled from it
/// are withdrawn ([`mh_constant_retire_compiled`], under
/// `CRATONVM_JIT_MH_CONSTANT_RETIRE_ON_REWRITE`). When no memo row of the
/// handle is left to name its member (evicted by a full table), every field
/// row and every recorded compile is taken instead: the member is unknown, and
/// a rewrite is rare enough that over-withdrawing costs only recompiles.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_guard_violated(shared: &SharedVm, handle: u64, how: &str) {
    let bridge = &shared.jit.bridge;
    let purged: Vec<MhConstantTarget> = {
        let mut rows = bridge
            .mh_constant_targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        rows.retain(|(h, _), t| {
            if *h == handle {
                out.push(t.clone());
                false
            } else {
                true
            }
        });
        out
    };
    bridge
        .mh_constant_guards
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&handle);
    // Round 17 wave 2 (lane mh3): with no memo row left the member is
    // unknown, so every field row goes (the retire switch's `=0` keeps
    // mh2's behaviour, which dropped none then).
    let retire = mh_constant_retire_on_rewrite_enabled();
    if !purged.is_empty() || retire {
        // JIT round 17 wave 3 (lane mh4): each dropped row's witness is
        // released.
        let mut released: Vec<u64> = Vec::new();
        bridge
            .mh_constant_field_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, (t, w)| {
                let keep = !purged.is_empty()
                    && !purged.iter().any(|p| {
                        (&p.class, &p.name, &p.desc, p.class_id)
                            == (&t.class, &t.name, &t.desc, t.class_id)
                    });
                if !keep {
                    released.push(*w);
                }
                keep
            });
        mh_constant_field_row_witnesses_released(shared, &released);
    }
    if crate::jit::helpers::mh_constant_bind_dbg() {
        for t in &purged {
            eprintln!(
                "[mh-bind] IMMUTABILITY VIOLATED ({how}): handle {handle:#x} admitted as {}.{}{} was rewritten in place",
                t.class, t.name, t.desc
            );
        }
        if purged.is_empty() {
            eprintln!(
                "[mh-bind] IMMUTABILITY VIOLATED ({how}): handle {handle:#x} was rewritten in place"
            );
        }
    }
    if retire {
        let members: Vec<(u32, Box<str>, Box<str>)> = purged
            .iter()
            .map(|t| (t.class_id, t.name.clone(), t.desc.clone()))
            .collect();
        mh_constant_retire_compiled(shared, (!members.is_empty()).then_some(&members[..]));
    }
}

/// Members [`JitBridgeState::mh_constant_compiled`] remembers, and compiling
/// classes per member; past either the compile door refuses the site
/// (JIT round 17 wave 2, lane mh3).
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_COMPILED_MEMBERS_MAX: usize = 1024;
pub(in crate::runtime::interpreter::jit_bridge) const MH_CONSTANT_COMPILED_CLASSES_PER_MEMBER: usize = 64;

/// `CRATONVM_JIT_MH_CONSTANT_RETIRE_ON_REWRITE` -- correctness, default ON,
/// `=0` restores round 17 wave 2's mh2 behaviour (JIT round 17 wave 2, lane
/// mh3; `r17-mh-constant-compiled-body-not-retired-on-handle-rewrite-FIXED-20261005.md`).
/// A body the compile door built with a constant handle's member as a plain
/// `invokestatic` depends on the handle staying what it was admitted as, the
/// way a body that inlined a class depends on that class: the door records
/// the dependency ([`note_mh_constant_compiled`]) and refuses a site it cannot
/// record, and a handle found rewritten in place
/// ([`mh_constant_guard_violated`]) withdraws the dependent bodies
/// ([`mh_constant_retire_compiled`]). Read per question (compile time and the
/// violation only).
pub(crate) fn mh_constant_retire_on_rewrite_enabled() -> bool {
    true
}

/// Record that the compile door compiled a site of a method of the class
/// `(class_id, class_name)` as a plain call of `member` (`(member class id,
/// name, descriptor)`). `false` when the table is full for it: the door then
/// refuses the site, since nothing could retire the body. Cold: one admitted
/// site per compile.
pub(in crate::runtime::interpreter::jit_bridge) fn note_mh_constant_compiled(
    shared: &SharedVm,
    member: (u32, &str, &str),
    class_id: u32,
    class_name: &Arc<str>,
) -> bool {
    let mut rows = shared
        .jit
        .bridge
        .mh_constant_compiled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (member.0, Box::from(member.1), Box::from(member.2));
    if rows.len() >= MH_CONSTANT_COMPILED_MEMBERS_MAX && !rows.contains_key(&key) {
        return false;
    }
    let list = rows.entry(key).or_default();
    if list.iter().any(|(id, _)| *id == class_id) {
        return true;
    }
    if list.len() >= MH_CONSTANT_COMPILED_CLASSES_PER_MEMBER {
        return false;
    }
    list.push((class_id, Arc::clone(class_name)));
    true
}

/// Take the classes [`note_mh_constant_compiled`] recorded for `members`
/// (every recorded class for `None`), each once.
pub(in crate::runtime::interpreter::jit_bridge) fn take_mh_constant_compiled(
    shared: &SharedVm,
    members: Option<&[(u32, Box<str>, Box<str>)]>,
) -> Vec<(u32, Arc<str>)> {
    let mut rows = shared
        .jit
        .bridge
        .mh_constant_compiled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lists: Vec<Vec<(u32, Arc<str>)>> = match members {
        Some(members) => members.iter().filter_map(|m| rows.remove(m)).collect(),
        None => rows.drain().map(|(_, list)| list).collect(),
    };
    let mut classes: Vec<(u32, Arc<str>)> = Vec::new();
    for (id, name) in lists.into_iter().flatten() {
        if !classes.iter().any(|(c, _)| *c == id) {
            classes.push((id, name));
        }
    }
    classes
}

/// Withdraw every compiled body of the classes whose methods the compile door
/// compiled one of `members` into (all recorded ones for `None`) as a plain
/// call, because the handle that call stood for was rewritten in place (JIT
/// round 17 wave 2, lane mh3).
///
/// The withdrawal is a class redefinition's without the redefinition
/// (`JitRealm::withdraw_class_for_the_interpreter`, the breakpoint's scoped
/// withdrawal): the class's bodies, the bodies that inlined one of its methods
/// and their baked callers are evicted and made not entrant, its optimizing
/// OSR bodies marked withdrawn, and the exits of every such body forced, so a
/// frame already running a loop that calls the old member leaves for the
/// interpreter at its next exit-capable back edge; the invalidation log's
/// `RedefinedClass` record refuses the publication of a compile of the class
/// already running. The withdrawn methods go back to the interpreter tier and
/// compile again; the memo rows are gone by then, so the door keeps the
/// `invokeExact` (or re-admits the handle as it now is). Class-scoped where
/// one method would do: the event is a defensive one (no writer reaches an
/// admitted handle today), and the class is the unit the withdrawal knows.
/// HotSpot deoptimizes the dependents of a changed call site the same way.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_retire_compiled(shared: &SharedVm, members: Option<&[(u32, Box<str>, Box<str>)]>) {
    shared.jit.reflect_link_ids.invalidate_invoke_cache();
    let classes = take_mh_constant_compiled(shared, members);
    if classes.is_empty() {
        return;
    }
    // Armed exactly as the redefinition, the VarHandle mint and the late-bind
    // retire arm it; idempotent. Without it nothing is made not entrant.
    shared
        .jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: shared as *const SharedVm as usize,
        });
    for (class_id, class_name) in classes {
        let (evicted, patched, forced, scoped) = shared
            .jit
            .withdraw_class_for_the_interpreter(ClassId::new(class_id), &class_name);
        if crate::jit::helpers::mh_constant_bind_dbg() {
            eprintln!(
                "[mh-bind] retired {class_name}: compiled from a rewritten handle (evicted={evicted} not-entrant={patched} exits-forced={forced} scoped={scoped})"
            );
        }
    }
}

/// Stage 3b, force slice, of
/// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`
/// (interpreter round i6 wave 123, lane L2;
/// `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`, default off): make the compiled
/// activation of `cm` suspended in the call at `call_bci` leave for the
/// interpreter at the post-call exit right after that call, so a debugger
/// write to its first `locals` locals can be applied to the frame the exit
/// rebuilds (slice 4, apply).
///
/// The force is the breakpoint's scoped withdrawal of the body's class
/// (`JitRealm::withdraw_class_for_the_interpreter`): the class's bodies, the
/// bodies that inlined or copied one of its methods and their baked callers
/// are made not entrant, marked withdrawn and have their exits forced. The
/// post-call stub's verdict then sends a withdrawn body out with no agent gate
/// (`jvmti_events::withdrawn_body_may_leave`), and the withdrawal mark is
/// never cleared, so it is also the hold: nothing has to keep the method
/// "must run interpreted" while the write is pending. A later compile of the
/// method is a new body; the pending write keys on this activation's frame
/// base and body, so it never meets it.
///
/// Fail closed: the switch off, a door that is not open, a body with no
/// owner class or no parseable label, or any refusal of
/// `CompiledMethod::debugger_force_verdict` after the withdrawal answers
/// something other than `Leaves`, and the caller keeps `OPAQUE_FRAME`. The
/// door is asked BEFORE anything is withdrawn, so a refused write costs no
/// compiled code. Called on the thread parked for the debugger (the only one
/// on which the activation's frame base is valid), by `StackFrame.SetValues`
/// (`debug::inspect::set_compiled_home_values`, wave 123), which records the
/// write only on `Leaves`.
#[cfg(feature = "experimental-debug")]
pub(crate) fn force_compiled_home_exit(
    shared: &SharedVm,
    cm: &cratonvm_jit::CompiledMethod,
    call_bci: u32,
    locals: usize,
) -> cratonvm_jit::DebuggerForceVerdict {
    use cratonvm_jit::{DebuggerForceVerdict as V, DebuggerWriteDoor as D};
    if !true {
        return V::Door(D::NoExit);
    }
    let door = super::compiled_home_write_door(cm, call_bci, locals);
    if !matches!(door, D::Open { .. }) {
        return V::Door(door);
    }
    let class_name = cm
        .method_label
        .rsplit_once(':')
        .and_then(|(rest, _)| rest.rsplit_once('.'))
        .map(|(class_name, _)| class_name);
    let owner = cm.owner_class_id;
    let Some(class_name) =
        class_name.filter(|_| owner != cratonvm_types::jit_activation::NO_OWNER_CLASS)
    else {
        // No class to withdraw by: the verdict would never name the body.
        return cm.debugger_force_verdict(call_bci, locals, false, false);
    };
    // Armed exactly as the breakpoint's withdrawal arms it; idempotent.
    // Without it nothing is made not entrant or marked withdrawn.
    shared
        .jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: shared as *const SharedVm as usize,
        });
    let class_id = ClassId::new(owner);
    let (evicted, patched, forced, scoped) = shared
        .jit
        .withdraw_class_for_the_interpreter(class_id, class_name);
    let withdrawn = shared.jit.jit_cache.body_withdrawn_by_redefinition(cm);
    // `withdrawn_body_may_leave`'s obsolete clause, without its grant side
    // effects: a body compiled before its class's last redefinition stays.
    let obsolete = !cm.leaves_as_renumbered_obsolete()
        && (crate::runtime::redefine_state::class_was_redefined(shared, class_id)
            || crate::runtime::redefine_state::named_class_was_redefined(shared, class_name))
        && !shared
            .jit
            .jit_cache
            .compiled_since_redefinition_of(class_id, cm.install_epoch);
    let verdict = cm.debugger_force_verdict(call_bci, locals, withdrawn, obsolete);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] debugger home write force {} call-bci={call_bci}: {} \
             (scoped={scoped} evicted={evicted} not-entrant={patched} exits-forced={forced})",
            cm.method_label,
            verdict.name(),
        );
    }
    verdict
}

/// `CRATONVM_JIT_MH_CONSTANT_INTERP_FILL` -- default OFF, opt-in (JIT round
/// 17 wave 2, lane mh2; MHCONST17-1 of `jit-r17-mhconst-proposals-RETIRED-20261006.md`): the
/// interpreter's `MethodHandle.invokeExact` / `invoke` door
/// (`invoke.rs`, `try_stackless_invoke`'s erased arm) asks the constant bind's
/// admission too ([`mh_constant_interpreter_fill`]), so a method whose first
/// optimizing compile comes before any COMPILED caller ran its site
/// (`R16MhFloor exact-static`) finds the handle admitted at that compile, and
/// a waiter of `CRATONVM_JIT_MH_CONSTANT_RECOMPILE` is served with no compiled
/// call of the site. Read once per VM.
pub(crate) fn mh_constant_interp_fill_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .mh_constant_interp_fill_switch
        .get_or_init(|| {
            false
        })
}

/// `CRATONVM_MH_SITE_LINK_LOADS_TYPES` -- correctness, default ON, `=0`
/// restores round 17 wave 2's mh2 behaviour (JIT round 17 wave 2, lane mh3;
/// item 1 of `r17-mh-door-loader-check-residuals-FIXED-20261005.md`): see
/// [`mh_site_link_caller_types`]. Read only for a site naming a class outside
/// `java/`.
pub(in crate::runtime::interpreter::jit_bridge) fn mh_site_link_loads_types_enabled() -> bool {
    true
}

/// Link an interpreted `MethodHandle.invokeExact` / `invoke` call site of
/// descriptor `site` in the class `caller` the way HotSpot links it (JIT
/// round 17 wave 2, lane mh3): the site's `MethodType` is resolved through
/// the CALLER's loader (`SystemDictionary::find_method_handle_type`, JVMS
/// §5.4.3.6), which LOADS each class it names. A child-first loader thereby
/// defines its own copy of a name another loader's handle was minted against,
/// and `invokeExact`'s check then compares two classes
/// (`lang_invoke::mh_exact_loader_refusal`, which resolves without loading and
/// so used to see only the handle's class: `R17MhLoaders cross-loader-null`,
/// a fresh loader passing `null`).
///
/// Only names outside `java/` that the caller's loader does not see yet
/// ([`mh_site_link_missing_names`]; round 17 wave 3 asks the initiating
/// records too) are resolved, through `resolve_class_or_array_loader_aware`, the
/// interpreter's constant-pool class resolution; a JDK caller never. A
/// failure is ignored, as before (HotSpot would throw the
/// `NoClassDefFoundError` at the link; our resolution can fail where it would
/// not, and the door's own checks still run). The arguments are pinned across
/// the loads, which may run the loader's Java code and collect: `Some` is the
/// arguments' current values, to be used instead of `args`; `None` when
/// nothing was loaded. With the signature-polymorphic route cache on, this
/// runs on a site's first execution per receiver class only.
pub(crate) fn mh_site_link_caller_types(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    door: &str,
    site: &str,
    caller: ClassId,
    args: &[Value],
) -> Option<Vec<Value>> {
    let missing = mh_site_link_missing_names(shared, door, site, caller);
    if missing.is_empty() {
        return None;
    }
    let mut current = args.to_vec();
    let pins = InvokeArgsRootGuard::new(thread, &current);
    for name in &missing {
        let _ = mh_site_link_load(shared, thread, door, site, caller, name, "");
    }
    pins.refresh(&mut current);
    drop(pins);
    Some(current)
}

/// The names [`mh_site_link_caller_types`] must load for a site of
/// descriptor `site` in the class `caller`: those outside `java/` that the
/// caller's loader has neither defined nor initiated. Empty for a JDK caller,
/// a door other than `invokeExact` / `invoke`, or the switch off. No loading
/// (JIT round 17 wave 3, lane mh4: shared with the compiled site's link).
///
/// Round 17 wave 3 (lane mh4; item 2 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): asked through
/// `class_resolved_without_loading`, the compile door's reader, which also
/// sees the names a user loader DELEGATED (the initiating record a
/// `loadClass` drive leaves), where `find_class_by_name_for_class` walks only
/// the caller's own namespace and the built-in chain: a caller whose loader's
/// parent is another user loader no longer re-resolves the site's names at
/// every route-cache miss.
pub(crate) fn mh_site_link_missing_names(
    shared: &SharedVm,
    door: &str,
    site: &str,
    caller: ClassId,
) -> Vec<String> {
    if !matches!(door, "invokeExact" | "invoke") {
        return Vec::new();
    }
    let names = mh_constant_descriptor_class_names(site);
    if names.iter().all(|n| n.starts_with("java/")) || !mh_site_link_loads_types_enabled() {
        return Vec::new();
    }
    let cm = shared.classes.class_manager.read();
    let Some(caller_class) = cm.get_class(caller) else {
        return Vec::new();
    };
    if ["java/", "jdk/", "sun/"]
        .iter()
        .any(|p| caller_class.name.starts_with(p))
        || cm.get_loader_id(caller).is_none()
    {
        return Vec::new();
    }
    let mut missing: Vec<String> = Vec::new();
    for name in names {
        if !name.starts_with("java/")
            && class_resolved_without_loading(shared, &cm, caller, name).is_none()
            && !missing.iter().any(|m| m == name)
        {
            missing.push(name.to_string());
        }
    }
    missing
}

/// Resolve `name` through the loader of `caller` for the link of a site of
/// descriptor `site` (the interpreter's constant-pool class resolution). The
/// unchecked interpreted link ignores a failure (see
/// [`mh_site_link_caller_types`]); the compiled site's link propagates it
/// under `CRATONVM_MH_SITE_LINK_NCDFE` (JIT round 18 wave 11, lane mhresid:
/// `helpers.rs` `mh_compiled_site_link`). `how` tags the debug line (`""`
/// interpreted, `" (compiled)"` the dispatch helper's).
pub(crate) fn mh_site_link_load(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    door: &str,
    site: &str,
    caller: ClassId,
    name: &str,
    how: &str,
) -> Result<ClassId, crate::error::MethodCallFailed> {
    let loaded = super::resolve_class_or_array_loader_aware(shared, thread, caller, name);
    if crate::jit::helpers::mh_constant_bind_dbg() {
        eprintln!(
            "[mh-bind] site link{how} MethodHandle.{door}{site}: {name} through the caller's loader -> {}",
            match &loaded {
                Ok(id) => format!("class id {}", id.as_u32()),
                Err(_) => "not resolved".to_string(),
            }
        );
    }
    loaded
}



/// The interpreter's fill of the constant-bind memo (MHCONST17-1): an
/// interpreted `invokeExact` (or, with `CRATONVM_JIT_MH_CONSTANT_BIND_INVOKE`,
/// `invoke`) of call-site descriptor `site`, in class `caller`, on `handle`.
/// Records what the dispatch helpers' cold fill records
/// (`helpers.rs`, `mh_constant_bind_fill`) for an exact direct STATIC handle:
/// the admission (`lang_invoke::mh_compiled_bind_target`, pure reads), the
/// loader check ([`mh_constant_bind_loaders_agree`]), the handle's words
/// (MH7-3) and the memo row, which serves any waiter. Once per (handle, site,
/// epoch); with the signature-polymorphic route cache on, the interpreter
/// reaches this only on a site's first execution per receiver class.
pub(crate) fn mh_constant_interpreter_fill(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    handle: ObjectRef,
    door: &str,
    site: &str,
    caller: ClassId,
) {
    if !mh_constant_interp_fill_enabled(shared) || !mh_constant_compile_enabled(shared) {
        return;
    }
    let door_admitted = door == "invokeExact"
        || (door == "invoke"
            && cratonvm_native_builtins::lang_invoke::mh_constant_bind_invoke_enabled());
    if !door_admitted {
        return;
    }
    let realm = &shared.mem;
    let epoch = realm.address_epoch();
    let raw = handle.as_ptr() as u64;
    if mh_constant_target_of(shared, raw, epoch, site).is_some() {
        // JIT round 17 wave 2 (lane mh3): the immutability check (MH7-3) on
        // this fill too, so a handle compiled into every caller (whose sites
        // no helper fill reaches again) is still checked by the interpreted
        // ones. A rewrite drops its rows, so the admission below runs again.
        let rewritten = mh_constant_immutable_check_enabled()
            && mh_constant_guard_verify(shared, raw, epoch, || {
                crate::jit::helpers::mh_constant_handle_words(shared, handle)
            });
        if !rewritten {
            return;
        }
    }
    let target = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        // JIT round 19 wave 1 (lane mhiface; MHRESID18-4): an unbound
        // interface handle, asked only after the exact admission refused,
        // under `CRATONVM_JIT_MH_CONSTANT_INTERFACE` (the helper fill's own
        // admission, `helpers.rs` `mh_constant_bind_fill`).
        // JIT round 21 wave 19 (lane mhbind2;
        // `CRATONVM_JIT_MH_INTERP_FILL_SKIP_BOUND`): a bound answer is
        // discarded below, so the REAL bound admission is not asked.
        let exact = {
            cratonvm_native_builtins::lang_invoke::mh_compiled_bind_target_unbound(
                &mut ctx, handle, site,
            )
        };
        exact.or_else(|| {
            if mh_constant_interface_enabled()
                && cratonvm_native_builtins::lang_invoke::mh_constant_bind_interface_enabled()
            {
                cratonvm_native_builtins::lang_invoke::mh_compiled_bind_interface_target(
                    &mut ctx, handle, site,
                )
            } else {
                None
            }
        })
    };
    if target.is_none()
        && mh_constant_compile_converting_enabled()
        && cratonvm_native_builtins::lang_invoke::mh_constant_bind_real_converting_enabled()
    {
        let real_conv = {
            let mut ctx = crate::vm::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            cratonvm_native_builtins::lang_invoke::mh_real_converting_compiled_bind(
                &mut ctx, handle, site,
            )
        };
        if let Some((t, c)) = real_conv {
            if !t.bound
                && matches!(t.invoke_kind, 0 | 2 | 3)
                && realm.address_epoch() == epoch
                && mh_constant_bind_loaders_agree(shared, caller.as_u32(), t.class_id, site)
            {
                if let Some(rc) = crate::jit::helpers::mh_real_conv_of(shared, &c) {
                    if let Some(row) = crate::jit::helpers::mh_real_convert_compile_row(
                        shared,
                        t.invoke_kind,
                        site,
                        &rc,
                    ) {
                        if mh_constant_immutable_check_enabled() {
                            if let Some(words) =
                                crate::jit::helpers::mh_constant_handle_words(shared, handle)
                            {
                                note_mh_constant_guard(shared, raw, epoch, words);
                            }
                        }
                        if crate::jit::helpers::mh_constant_bind_dbg() {
                            eprintln!(
                                "[mh-bind] interpreter fill (converting) MethodHandle.{door}{site} -> {}{}{}",
                                t.class, t.name, t.desc
                            );
                        }
                        note_mh_constant_target_converting(
                            shared,
                            raw,
                            epoch,
                            site,
                            (&t.class, &t.name, &t.desc),
                            t.class_id,
                            row,
                        );
                        return;
                    }
                }
            }
        }
    }
    // JIT round 17 wave 3 (lane mh4): an unbound virtual handle too, under
    // `CRATONVM_JIT_MH_CONSTANT_VIRTUAL`.
    let Some(t) = target.filter(|t| !t.bound && mh_constant_records_kind(t.invoke_kind)) else {
        return;
    };
    if realm.address_epoch() != epoch
        || !mh_constant_bind_loaders_agree(shared, caller.as_u32(), t.class_id, site)
    {
        return;
    }
    if mh_constant_immutable_check_enabled() {
        if let Some(words) = crate::jit::helpers::mh_constant_handle_words(shared, handle) {
            note_mh_constant_guard(shared, raw, epoch, words);
        }
    }
    if crate::jit::helpers::mh_constant_bind_dbg() {
        eprintln!(
            "[mh-bind] interpreter fill MethodHandle.{door}{site} -> {}.{}{} {}",
            t.class,
            t.name,
            t.desc,
            mh_constant_kind_word(t.invoke_kind)
        );
    }
    note_mh_constant_target_kind(
        shared,
        raw,
        epoch,
        site,
        (&t.class, &t.name, &t.desc),
        t.class_id,
        t.invoke_kind,
    );
}

/// How many first-call compiles the eager door defers, per method, on a
/// constant-pool resolver miss before it seals the method as it always did:
/// the miss is the common case at a method's FIRST call (a `getstatic` of a
/// class nothing has loaded yet), which the interpreted call it falls back to
/// resolves, so the second attempt normally compiles.
pub(in crate::runtime::interpreter::jit_bridge) const EAGER_RESOLVER_MISS_RETRIES: u8 = 4;

/// `CRATONVM_JIT_FIRST_CALL_RESOLVE`: may a compile door leave a call site no
/// door has resolved to resolve at its first execution, through a one-shot
/// guard (`cratonvm_jit::JitFirstCallCell`), instead of recording its owner
/// when the method is compiled (interpreter round i1 wave 50, lane L1;
/// `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`, item 2)? Read
/// once per VM; on by default. Off (`=0`): every compile records its sites'
/// owners at compile time, the wave-49 behaviour.
pub(crate) fn first_call_resolve_enabled(shared: &SharedVm) -> bool {
    *shared.jit.bridge.first_call_switch.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP` (interpreter round i4 wave 92,
/// lane L7; default OFF until measured): does a class-manager write guard's
/// release re-sweep this VM's held deferred-`new` retries only when
/// something that can settle a `new` site moved since the last release sweep
/// ([`deferred_new_release_sweep_due`])? Read once per VM.
pub(crate) fn deferred_new_release_stamp_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .deferred_new_release_stamp_switch
        .get_or_init(|| {
            false
        })
}

/// `CRATONVM_JIT_CALLEE_NEG_KEYED_STAMP` (interpreter round i4 wave 92, lane
/// L7; default OFF until measured): is the by-name callee door's negative
/// cache stamped with the JIT cache's per-`(method, descriptor)` publication
/// stamp (`JitCache::callee_publish_stamp`) instead of the cache-wide
/// generation, which every publication and retirement anywhere moves? Read
/// once per VM.
pub(crate) fn callee_neg_keyed_stamp_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .callee_neg_keyed_stamp_switch
        .get_or_init(|| {
            false
        })
}

/// `CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER` (interpreter round i4 wave 93,
/// lane L7; **default ON** since 2026-10-25, `=0` off -- WARNING:
/// tomcat-defer-default-flip, see the commit): under the background compiler, the
/// by-name callee door (`try_jit_compile_callee*`, which the compiled-code
/// dispatch helpers call for an uncompiled callee) ENQUEUES the compile it
/// would have run on the mutator and answers `None`, so the call keeps its
/// dispatch helper and the callee runs interpreted until the worker
/// publishes. Every other `None` of the door is already that fallback, so the
/// answer is never less correct; only a compile on the mutator thread moves.
/// Read once per VM.
pub(crate) fn callee_door_defers_to_worker_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .callee_door_defers_to_worker_switch
        .get_or_init(|| {
            true
        })
}

/// `CRATONVM_JIT_CALLEE_DOOR_C1_FIRST` (**default ON** since 2026-11-03, `=0` off --
/// WARNING: callee-door-c1-first-default-flip, see the commit; Netty buffer compile-cost
/// proposal, Phase 2a): the callee door's deferred compile is a C1 task
/// even when the door would have optimized, so an uncompiled callee pays the
/// top-tier price only after the entry counters promote it. Read once per VM.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_door_c1_first_enabled(shared: &SharedVm) -> bool {
    *shared
        .jit
        .bridge
        .callee_door_c1_first_switch
        .get_or_init(|| {
            true
        })
}

/// The deferred task's `optimize`: the door's own, unless the C1-first switch is on.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_door_deferred_optimize(optimize: bool, c1_first: bool) -> bool {
    optimize && !c1_first
}

/// The by-name callee door's answer when no published body serves the caller
/// (every filter of the door -- re-entry-only, synchronized / self-locking,
/// hidden-self name, bail list -- applies in all three modes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalleeDoorMiss {
    /// Compile on this thread, never deferring: the OSR door's binds
    /// ([`try_jit_compile_callee_from_site_in_place`]).
    CompileInPlace,
    /// Defer to the worker when [`callee_door_defers_now`] says so, compile on
    /// this thread otherwise: every other compiled-code door.
    MayDefer,
    /// Answer `None`: neither defer nor compile. The virtual dispatch
    /// counter's crossing under `CRATONVM_JIT_VIRTUAL_DISPATCH_COUNTER_PROBE_ONLY`
    /// (j27 wave 3, lane DISPATCH2), whose method is counted and nominated by
    /// the tiered manager through the call's tail instead.
    ProbeOnly,
}

impl CalleeDoorMiss {
    /// `may_defer` of [`callee_door_defers_now`].
    pub(in crate::runtime::interpreter::jit_bridge) fn may_defer(self) -> bool {
        self == Self::MayDefer
    }

    /// Whether a miss may go on to the worker or to the slow compile path.
    pub(in crate::runtime::interpreter::jit_bridge) fn may_compile(self) -> bool {
        self != Self::ProbeOnly
    }
}

/// Should the callee door, about to compile, enqueue instead
/// ([`callee_door_defers_to_worker_enabled`])? Only for a caller that may be
/// answered later, on a mutator thread that is not inside any compile:
/// - `may_defer`: false for [`try_jit_compile_callee_from_site_in_place`],
///   the OSR door's binds ([`compile_osr_body`]), which run outside any
///   `try_compile` (the compile-stack depth reads 0) into a body built once
///   per loop, where a deferred callee would stay on the dispatch helper for
///   the body's life;
/// - the background compiler is the default (`env_cache::bg_compile`); with
///   `CRATONVM_BG_COMPILE=0` the mutator is the compiler and nothing drains a
///   queue;
/// - no compile is open on this thread (`jit_active_compile_depth() == 0`): a
///   nested callee compile is the eager chain of a compile already running,
///   whose budget and cycle rules decide it;
/// - this is not the background worker itself, whose compiles are the ones
///   the queue feeds.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_door_defers_now(shared: &SharedVm, may_defer: bool) -> bool {
    may_defer
        && callee_door_defers_to_worker_enabled(shared)
        && crate::runtime::env_cache::bg_compile()
        && cratonvm_jit::jit_active_compile_depth() == 0
        && std::thread::current().name() != Some("cratonvm-jit-compiler")
}

/// Hand the callee door's compile to the background worker
/// ([`callee_door_defers_now`]): a method-entry task under the key the door
/// probes and the worker publishes under (`class_id`, the receiver class's
/// identity, and `class_name`; `try_jit_compile_callee_slow` publishes under
/// that key), at the tier the door would have compiled. The worker's task
/// compiles through `try_jit_compile_wrapped_entry`, so a synchronized method
/// comes back wrapped and the door keeps refusing it, as it refuses one now.
/// The manager's admission refuses a method already queued, ineligible or out
/// of retries, so a re-probe enqueues nothing twice. Returns whether a task
/// was queued.
pub(in crate::runtime::interpreter::jit_bridge) fn defer_callee_compile_to_worker(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> bool {
    ensure_bg_compiler_started(shared);
    let enqueue_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64) // Cast: epoch milliseconds, u128 to u64
        .unwrap_or(0);
    let method_key =
        crate::jit::tiered::MethodKey::with_class_id(class_id, class_name, method_name, descriptor);
    // A method with no body yet goes to the C1 lane first, as HotSpot's tiers do
    // (0 -> 3 -> 4); its C1 body is upgraded to C2 the ordinary way. Queued
    // straight at C2 it waited behind the whole C2 lane, `queued` and so offered
    // nothing else, while every compiled caller ran it interpreted through the
    // by-name tail (the Spring MVC C2-lane stall, 2026-11-14).
    let optimize = optimize
        && shared.jit.tiered_manager.current_tier(&method_key)
            != crate::jit::tiered::CompilationTier::Interpreter;
    let task = crate::jit::tiered::CompilationTask {
        method_key,
        target_tier: if optimize {
            crate::jit::tiered::CompilationTier::C2
        } else {
            crate::jit::tiered::CompilationTier::C1
        },
        priority: crate::jit::tiered::CompilationPriority::Normal,
        enqueue_time_ms,
        osr_bci: None,
    };
    let queued = shared.jit.tiered_manager.enqueue_compilation(task);
    cratonvm_native_builtins::mh_finder_census::note_jit_callee_door(
        if queued { "deferred-queued" } else { "deferred-not-queued" },
        class_name,
        method_name,
        descriptor,
    );
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] callee-door DEFERRED {class_name}.{method_name}{descriptor} \
             optimize={optimize} queued={queued}"
        );
    }
    queued
}

/// The finder census's classification of a re-entered callee (interpreter
/// round i4 wave 93, lane L7; `interpreter::execute` asks it only when
/// `mh_finder_census::in_reentry_now`): is a method-entry body published under
/// the callee's own key, is the method bail-listed, and otherwise has it been
/// invoked the tier-up threshold times (a body retired and not compiled again
/// yet, or one still queued) or not (never compiled)? Census only: a JIT-cache
/// read lock, the verdict registry and a profile-store read.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn census_note_reentry_callee_state(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    let published = shared
        .jit
        .jit_cache
        .read()
        .get(class_name, method_name, descriptor, class_id)
        .is_some();
    let state = if published {
        "published"
    } else if jit_verdicts(shared).is_bail_listed(class_id, class_name, method_name, descriptor) {
        "bail-listed"
    } else {
        let calls = shared
            .jit
            .profile_store
            .invocation_count(cratonvm_jit_api::invoc_key_parts(
                class_id.as_u32(),
                method_name,
                descriptor,
            ));
        if calls >= shared.config.jit_invocation_threshold {
            "hot-unpublished"
        } else {
            "cold"
        }
    };
    cratonvm_native_builtins::mh_finder_census::note_reentry_callee_state(
        state,
        class_name,
        method_name,
        descriptor,
    );
}

/// May the callee door hand this compile to the worker
/// ([`defer_callee_compile_to_worker`])? Not when `try_jit_compile_callee_slow`
/// would refuse it at once for a reason that is not a verdict on the method:
/// a `static` method whose declaring class is not initialized yet (JVMS 5.5;
/// the door does not cache that `None`, and the worker's task would charge it
/// to the method's compile retries, which three such failures exhaust for the
/// life of the process), or no method / no `Code` to compile. Those keep the
/// slow path, which answers them without compiling. One class-manager read
/// and a hierarchy lookup.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_compile_may_be_deferred(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let store = cm.class_store();
    let Some((method, declaring_id)) =
        crate::classloading::find_method_recursive(class_id, method_name, descriptor, store)
    else {
        return false;
    };
    if method.code().is_none() {
        return false;
    }
    !method.is_static()
        || store
            .get(declaring_id)
            .is_some_and(crate::vm::is_class_initialized_fast)
}

/// Record a link or initialization completion in this VM: an event that can
/// turn a held deferred-`new` site resolvable (`jit_new_target_init_walk`
/// asks whether the target and its supertypes are linked and initialized).
/// Called WHILE the class-manager write guard that published the new state
/// is still held, so that guard's own release sweep sees the stamp move
/// (interpreter round i4 wave 92, lane L7). One relaxed-cost atomic add;
/// unconditional, so the stamp is right whichever way the switch is set.
#[inline]
pub(crate) fn note_new_site_settle_event(shared: &SharedVm) {
    shared
        .jit
        .bridge
        .deferred_new_settle_events
        .fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Everything a held deferred-`new` site's resolvability can move with, for
/// this VM, as one number: the class store's definition epoch (a target
/// defined), its resolution epoch (an initiating-loader answer recorded or
/// cached resolutions dropped), the link / initialization completions
/// ([`note_new_site_settle_event`]) and the retries ever armed (a retry held
/// after the last sweep). Each counter only grows, so the sum moves whenever
/// any of them does. Plus one, so 0 stays "never swept".
pub(in crate::runtime::interpreter::jit_bridge) fn deferred_new_release_stamp(shared: &SharedVm) -> u64 {
    use std::sync::atomic::Ordering;
    let epochs = cratonvm_classloading::store_epochs(shared.jit.class_layout_domain);
    epochs
        .class_definition_epoch()
        .wrapping_add(epochs.resolution_epoch())
        .wrapping_add(
            shared
                .jit
                .bridge
                .deferred_new_settle_events
                .load(Ordering::Acquire),
        )
        .wrapping_add(shared.jit.bridge.deferred_new_retries.arm_generation())
        .wrapping_add(1)
}

/// Should this write-guard release re-sweep `shared`'s held deferred-`new`
/// retries? Always, with `CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP` off (the
/// historical behaviour). With it on, only when nothing is held answers no
/// at once, and otherwise only when [`deferred_new_release_stamp`] moved
/// since the last release sweep of this VM.
///
/// Why this is enough (interpreter round i4 wave 92, lane L7): the stamp is
/// read BEFORE the sweep reads the held list and the class manager, and every
/// event it sums is published BEFORE its counter moves (a definition and a
/// link / init state under the write guard that bumps; an arm after its entry
/// is in the memo). So an event either lands before the read, and this sweep
/// sees it, or after, and the stamp differs for the next release. Two
/// releases racing on the swap: the loser skips only a stamp the winner is
/// sweeping at. What the stamp does not see is an input that moves without
/// a write guard and without a counter: a constant-pool class entry recorded
/// by the interpreter (`recorded_class_entry`) and the JDWP `ClassPrepare`
/// listener set (`class_prepare_observed`). A site waiting on one of those
/// is re-offered at the next stamp move instead of the next release; that
/// is a later tier-up, never a wrong answer -- the sweep only re-offers a
/// compile.
pub(in crate::runtime::interpreter::jit_bridge) fn deferred_new_release_sweep_due(shared: &SharedVm) -> bool {
    if !deferred_new_release_stamp_enabled(shared) {
        return true;
    }
    if shared.jit.bridge.deferred_new_retries.held_count() == 0 {
        return false;
    }
    let stamp = deferred_new_release_stamp(shared);
    shared
        .jit
        .bridge
        .last_deferred_new_release_stamp
        .swap(stamp, std::sync::atomic::Ordering::AcqRel)
        != stamp
}

/// `CRATONVM_JIT_SPLICE_REFUSES_UNRESOLVED_SITE`: does a splice planner keep
/// a callee out of line when one of its calls has an owner record that would
/// be observable and is not made yet (interpreter round i1 wave 51, lane L1;
/// `interpreter-L1-a-call-site-of-a-spliced-callee-is-never-recorded-as-resolved-FIXED-20261015`)?
/// A splice binds its calls by name and never resolves them, so such a call
/// first run inside it recorded nothing. Read once per VM; on by default
/// (`=0` splices such a callee, the wave-50 behaviour).
pub(crate) fn splice_refuses_unresolved_site_enabled(shared: &SharedVm) -> bool {
    *shared.jit.bridge.splice_unresolved_switch.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_DOOR_PEEKS_UNRESOLVED_SITES`: does a compile door's
/// access check of an invoke site the site cache does not hold PEEK at it
/// (`MemberResolver::method_ref_peek`: the same answer and refusals, but
/// neither the site cache filled nor the owner's class-entry row written)
/// rather than resolve it (interpreter round i1 wave 51, lane L1; stage 1 of
/// `i50-L1-proposal-compile-doors-resolve-nothing`)? The site then resolves
/// at its first execution, as in HotSpot: interpreted, by the interpreter's
/// own door; compiled, by its first-call guard when its record is observable
/// ([`first_call_cell_for`]), and otherwise with nothing to record (compiled
/// dispatch reads no site cache). Off (`=0`): the door resolves every
/// unguarded site when it compiles the method, the wave-50 behaviour. Read
/// once per VM; on by default.
pub(crate) fn door_peeks_unresolved_sites_enabled(shared: &SharedVm) -> bool {
    *shared.jit.bridge.door_peek_switch.get_or_init(|| {
        true
    })
}

/// The address of the first-call cell of the invoke at `cp_idx` of
/// `class_id`'s pool, armed, when the site is one whose owner the compile must
/// not record yet ([`super::invoke::unrecorded_method_ref_owner`]); `0`
/// otherwise (the switch off, a backend without the guard, or a site whose
/// record is unobservable or already made). The doors hand it to the backend
/// (`CompileRequest::cp_invoke_first_call`, `BackendRequest::first_call_cells`)
/// and peek at a flagged site instead of resolving it
/// ([`CpResolvers::invoke`]).
///
/// One cell per `(class, cp index, owner)` for the VM's life, re-armed when a
/// compile flags it again: the row it would write is absent, so any older
/// body's guard has not run (or its row went with an unload).
pub(crate) fn first_call_cell_for(shared: &SharedVm, class_id: ClassId, cp_idx: u16) -> usize {
    // Only the x86-64 backends emit the guard (`runtime_lowering`).
    if !cfg!(target_arch = "x86_64") || !first_call_resolve_enabled(shared) {
        return 0;
    }
    let Some(owner) = super::invoke::unrecorded_method_ref_owner(shared, class_id, cp_idx) else {
        return 0;
    };
    let mut cells = shared
        .jit
        .bridge
        .first_call_cells
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cell = cells
        .entry((class_id.as_u32(), cp_idx, owner.as_u32()))
        .or_insert_with(|| {
            Box::new(cratonvm_jit::JitFirstCallCell {
                armed: std::sync::atomic::AtomicU8::new(1),
                resolve: jit_first_call_resolve,
                ctx: shared as *const SharedVm as usize,
                referencing: class_id.as_u32(),
                owner: owner.as_u32(),
            })
        });
    cell.armed.store(1, std::sync::atomic::Ordering::Release);
    &**cell as *const cratonvm_jit::JitFirstCallCell as usize
}

/// The cold arm of a first-call guard (`runtime_lowering::emit_first_call_guard`):
/// the site's first execution in compiled code writes the owner's resolved
/// class-entry row, as the interpreter's resolution of the site would
/// (`resolve_method_metadata`'s `note_member_owner_resolved`), once. A LEAF:
/// it allocates nothing, runs no Java code and reaches no safepoint, which is
/// what lets compiled code call it with the site's operands still live.
///
/// # Safety
///
/// `cell` is a cell [`first_call_cell_for`] handed out; its `ctx` is the VM
/// that owns it, which outlives every body that bakes the cell.
pub(in crate::runtime::interpreter::jit_bridge) unsafe extern "C" fn jit_first_call_resolve(cell: *const cratonvm_jit::JitFirstCallCell) {
    // SAFETY: see the function's contract.
    let cell = unsafe { &*cell };
    if cell.armed.swap(0, std::sync::atomic::Ordering::AcqRel) == 0 {
        return;
    }
    // SAFETY: `ctx` is the owning `SharedVm` (see above).
    let shared = unsafe { &*(cell.ctx as *const SharedVm) };
    let referencing = ClassId::new(cell.referencing);
    let owner = ClassId::new(cell.owner);
    // A panic must not unwind into compiled code.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::runtime::resolve::initiating_records::note_member_owner_resolved(
            shared,
            referencing,
            owner,
        );
        // al16: the bodies compiled with this site left to its first
        // execution may now splice it (`note_first_call_waiters`).
        late_bind_key_served(
            shared,
            &first_call_waiter_key(cell as *const cratonvm_jit::JitFirstCallCell as usize),
            "its first call has run",
        );
    }));
    if false {
        eprintln!(
            "[first-call] resolved at first execution: class {} -> owner {}",
            cell.referencing, cell.owner
        );
    }
}

/// Is a single-pass OSR body of this VM running right now? See
/// `JitBridgeState::osr_bodies_running`. The caller fences (`SeqCst`) between
/// publishing the interpreter-only mode and this read.
#[inline]
pub(in crate::runtime::interpreter) fn osr_bodies_running(shared: &SharedVm) -> bool {
    shared
        .jit
        .bridge
        .osr_bodies_running
        .load(std::sync::atomic::Ordering::SeqCst)
        != 0
}

/// The compiled half of a multi-class redefinition call's install fence
/// (interpreter round i1 wave 41, lane L2;
/// `docs/feature-designs/class-resolution-and-caching.md`).
/// Called by `begin_redefinition_install` once the call's interpreter fence
/// is up (`obsolete_frames::raise_install_fence`) and the code cache's
/// publications are held (`JitCache::hold_publications`), BEFORE the call's
/// first class is installed.
///
/// Until now each class's compiled dependents were withdrawn at that class's
/// own install (`JitRealm::note_class_redefinition_of`, under the writer of
/// `redefine_class_with`). So between the first install and the last, a
/// compiled body that depends on a LATER class of the call only -- one that
/// spliced the old `Q.v` and calls `P.v` out of line -- stayed entrant and
/// kept running; its call into the installed `P` reached the interpreter,
/// which the fence holds until the last install, and returned `P`'s new
/// value into a body that then ran its old `Q` splice (a post-call exit is
/// the only thing that stops it, and not every call site has one). The
/// install fence turns that rare shape into a systematic one: every round's
/// held call spans the later installs.
///
/// Here every class of the call has its compiled dependents withdrawn
/// ahead of the first install -- made not entrant, the optimizing OSR memo's
/// marked, their exit polls forced -- and one loop-exit handshake
/// (`request_withdrawn_body_exits`) makes every running compiled body pass a
/// poll: a frame of a withdrawn body leaves for the interpreter there, where
/// the fence holds it at its next top. What remains compiled during the
/// window depends on no class of the call; its calls into them re-dispatch
/// through the interpreter (held) or a not-entrant stub (no body is published
/// while the hold is up). Each install still withdraws what depends on its
/// class, as before; withdrawing ahead only costs recompiles the install
/// would have caused anyway. A class the call then fails to install loses
/// its compiled code for nothing, which is only a recompile.
///
/// Positive control: `CRATONVM_DBG_JITC=1` prints one `[cratonvm-jitc]
/// install fence: withdrew ...` line per fenced call.
pub(crate) fn withdraw_compiled_dependents_ahead_of_install(
    shared: &SharedVm,
    owner: crate::ThreadId,
    classes: &[(ClassId, &[u8])],
) {
    if classes.len() < 2 {
        return;
    }
    // The caller has armed the cache's not-entrant hook
    // (`JitCache::arm_not_entrant`, as `redefine_class_with` does): an
    // unarmed cache patches nothing.
    let withdrawn_before = shared.jit.jit_cache.bodies_withdrawn_by_redefinition();
    let mut evicted = 0usize;
    {
        // `note_class_redefinition_of` is asked under the class-manager
        // writer (its scope decision reads the copied-bytecode marks a
        // compile sets under the read guard).
        let cm = shared.classes.class_manager_write();
        for &(class_id, _) in classes {
            let Some(class) = cm.get_class(class_id) else {
                continue;
            };
            let name = class.name.to_string();
            // The same question `redefine_class_with` asks: a class with a
            // registered native keeps the whole-cache flush.
            let has_registered_natives = class.methods.iter().any(|m| {
                shared
                    .natives
                    .native_methods
                    .find_with_kind(&name, &m.name, &m.descriptor)
                    .is_some()
            });
            evicted +=
                shared
                    .jit
                    .note_class_redefinition_of(class_id, &name, has_registered_natives);
        }
    }
    let withdrawn = shared
        .jit
        .jit_cache
        .bodies_withdrawn_by_redefinition()
        .wrapping_sub(withdrawn_before);
    // Only when something was withdrawn: no running body can be one then.
    let handshake = withdrawn != 0
        && crate::runtime::interpreter::request_withdrawn_body_exits(
            shared,
            owner,
            withdrawn_before,
        );
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] install fence: withdrew {withdrawn} bodies (evicted {evicted}) of {} \
             classes ahead of the first install; loop-exit handshake={handshake}",
            classes.len()
        );
    }
}

/// Counts one single-pass OSR body in `JitBridgeState::osr_bodies_running`
/// for as long as it runs, unwinds included.
pub(in crate::runtime::interpreter::jit_bridge) struct OsrBodyRunning<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> OsrBodyRunning<'a> {
    pub(in crate::runtime::interpreter::jit_bridge) fn enter(shared: &'a SharedVm) -> Self {
        let count = &shared.jit.bridge.osr_bodies_running;
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Pairs with the fence in `request_compiled_loop_exits`: either that
        // call sees this count, or the body's first slow-path verdict sees
        // the mode the caller published before it.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for OsrBodyRunning<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// `(class, redefine generation, cp index, singleton method key, singleton
/// pc)`; the last two are `lambda_singleton_site_for` of the instruction, or
/// `(0, usize::MAX)` when the door could not name it.
pub(in crate::runtime::interpreter::jit_bridge) type IndyBridgeSiteKey = (u32, u32, u16, u64, usize);

/// `(recipe units, constants, descriptor)` of a fused `StringBuilder` chain
/// (STRINTR21-4).
pub(in crate::runtime::interpreter::jit_bridge) type SbChainSiteKey = (Vec<u16>, Vec<Vec<u16>>, String, bool);

/// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4;
/// `CRATONVM_JIT_IR_SB_CHAIN_FUSE`): the concat bridge site of a fused
/// `new StringBuilder()...toString()` chain's recipe
/// (`CompileRequest::sb_concat_site_resolver`), interned per VM so a
/// recompile of the same chain reuses its process-lived site instead of
/// leaking another. `None` when the recipe is refused
/// ([`crate::runtime::invokedynamic::make_jit_string_concat_site_from_recipe`]).
pub(in crate::runtime::interpreter::jit_bridge) fn interned_jit_sb_chain_concat_site(
    shared: &SharedVm,
    recipe: &[u16],
    constants: &[Vec<u16>],
    descriptor: &str,
    deopt_on_overflow: bool,
) -> Option<usize> {
    let build = || {
        crate::runtime::invokedynamic::make_jit_string_concat_site_from_recipe(
            recipe,
            constants,
            descriptor,
            deopt_on_overflow,
        )
        .unwrap_or(0)
    };
    let site = match shared.jit.bridge.sb_chain_concat_sites.lock() {
        Ok(mut sites) => {
            let key: SbChainSiteKey = (
                recipe.to_vec(),
                constants.to_vec(),
                descriptor.to_string(),
                deopt_on_overflow,
            );
            *sites.entry(key).or_insert_with(build)
        }
        // A poisoned table still answers correctly, one leaked site at a time.
        Err(_) => build(),
    };
    (site != 0).then_some(site)
}

impl JitBridgeState {
    /// The negative-cache slots, allocated on first use.
    pub(in crate::runtime::interpreter::jit_bridge) fn callee_neg_slots(&self) -> &[std::sync::atomic::AtomicU64] {
        self.callee_neg_cache.get_or_init(|| {
            cratonvm_types::zeroed_table::zeroed_boxed_slice(CALLEE_NEG_CACHE_SLOTS, || {
                std::sync::atomic::AtomicU64::new(0)
            })
        })
    }

    /// Every optimizing OSR body the memo holds (interpreter round i1 wave 23,
    /// lane L6): a class redefinition marks the stale ones withdrawn
    /// (`JitRealm::withdrawn_osr_memo_candidates`), so a frame running one
    /// leaves at its next back-edge poll. They are never in the `JitCache`
    /// its not-entrant pass scans. A poisoned memo answers nothing.
    pub(crate) fn osr_optimizing_bodies(&self) -> Vec<Arc<cratonvm_jit::CompiledMethod>> {
        match self.osr_optimizing_accepted.lock() {
            Ok(accepted) => accepted
                .values()
                .map(|entry| Arc::clone(entry.artifact.arc()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// t1 wave 3, lane X: forget the optimizing OSR memo rows of the classes a
    /// class unload removed -- the accepted bodies (each holding its code and
    /// the pins of every callee it CALLs) and the refusals. Both maps are keyed
    /// by the method's `ClassId` (`osr_optimizing_key`), and class ids are not
    /// reused, so such a row is never read again; but `JitCache` never sees
    /// these bodies, so `invalidate_unloaded_classes` did not reach them, and
    /// the memo's own purge runs only when the VM's code-state epoch moves
    /// (`remember_osr_optimizing_artifact`), which an unload does not move.
    /// Every redeploy therefore kept the dead classes' loop bodies and their
    /// callees' code alive. No frame can run a body of an unloaded class, so
    /// dropping it here is the same release a purge makes (the retirement
    /// queue, outside the lock). Returns the accepted bodies forgotten.
    pub(crate) fn forget_osr_optimizing_of_unloaded(&self, unloaded: &dyn Fn(u32) -> bool) -> usize {
        let forgotten: Vec<AcceptedOsrOptimizing> = match self.osr_optimizing_accepted.lock() {
            Ok(mut accepted) => {
                let keys: Vec<(u32, u64, usize)> =
                    accepted.keys().filter(|k| unloaded(k.0)).copied().collect();
                keys.iter().filter_map(|k| accepted.remove(k)).collect()
            }
            Err(_) => Vec::new(),
        };
        if let Ok(mut refused) = self.osr_optimizing_refused.lock() {
            refused.retain(|k, _| !unloaded(k.0));
        }
        // The superseded bodies a late bind left armed-to-be (weak since t1
        // wave 9, capped by `LATE_BIND_CALLEE_CAP`): a dead class's row would
        // only occupy the cap.
        let superseded: Vec<SupersededOsrBody> = match self.late_bind_osr_superseded.lock() {
            Ok(mut rows) => {
                let (dead, live): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut *rows).into_iter().partition(|row| unloaded(row.class_id));
                *rows = live;
                dead
            }
            Err(_) => Vec::new(),
        };
        drop(superseded);
        let n = forgotten.len();
        // Cast: a count of map entries fits u64.
        note_osr_optimizing_memo_change(0, n as u64);
        drop(forgotten);
        n
    }

    /// The eager first-call door (`execute`, `CRATONVM_BG_COMPILE=0`) could not
    /// resolve a constant-pool operand of `class_id`'s `name` `descriptor`
    /// without loading: may it defer the compile to a later call instead of
    /// sealing the method? `true` for the first [`EAGER_RESOLVER_MISS_RETRIES`]
    /// misses of the method. Until wave 39 the first miss sealed it
    /// (`early-backend-bail` into the name-keyed `jit_skip_set`), which kept
    /// the method out of this door and `execute`'s cache consult for the life
    /// of the VM although the interpreted call resolves the operand
    /// (interpreter round i1 wave 39, lane L2). A poisoned memo seals.
    pub(in crate::runtime::interpreter) fn eager_resolver_miss_may_retry(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> bool {
        let Ok(mut misses) = self.eager_resolver_misses.lock() else {
            return false;
        };
        let count = misses
            .entry((class_id.as_u32(), Arc::from(name), Arc::from(descriptor)))
            .or_insert(0);
        *count = count.saturating_add(1);
        *count <= EAGER_RESOLVER_MISS_RETRIES
    }

    /// The optimizing OSR route declined a compile of the method whose
    /// method-wide refusal key is `key` on a constant-pool resolver miss: may
    /// it leave the refusal unmemoised, so a later offer (after the
    /// interpreted loop has loaded the class) compiles again? `true` for the
    /// first [`OSR_OPTIMIZING_RESOLVER_MISS_RETRIES`] such misses of the
    /// method; then the route memoises it as before. A poisoned memo answers
    /// `false` (memoise). Interpreter round i5 wave 101, lane L2.
    pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_resolver_miss_may_retry(&self, key: (u32, u64, usize)) -> bool {
        let Ok(mut misses) = self.osr_optimizing_resolver_misses.lock() else {
            return false;
        };
        let count = misses.entry((key.0, key.1)).or_insert(0);
        *count = count.saturating_add(1);
        *count <= OSR_OPTIMIZING_RESOLVER_MISS_RETRIES
    }

    /// Count one deoptimization about to be recorded (see `deopt_events`).
    #[inline]
    pub(crate) fn note_deopt_event(&self) {
        self.deopt_events
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Deoptimizations counted so far (see `deopt_events`).
    #[inline]
    pub(in crate::runtime::interpreter::jit_bridge) fn deopt_events(&self) -> u64 {
        self.deopt_events.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Milliseconds on this VM's drain clock (monotonic, from the first call).
    pub(in crate::runtime::interpreter::jit_bridge) fn drain_clock_ms(&self) -> u64 {
        let origin = self.drain_clock_origin.get_or_init(std::time::Instant::now);
        // Cast: u128 milliseconds of process lifetime fit a u64.
        origin.elapsed().as_millis() as u64
    }
}

/// The `invokedynamic` bridge site for `class`'s `cp_idx`, built once per VM
/// and reused by every compile attempt that asks — `0` when the bootstrap is
/// one the bridge cannot serve (the site keeps its uncommon trap).
///
/// `make_jit_indy_bridge_site_from_parts` returns a process-lived allocation:
/// generated code embeds the pointer and no artifact owns it. Called directly
/// by every compile door, it leaked one site per indy instruction per ATTEMPT,
/// including every attempt that published nothing — a refusal after the sites
/// were built, a backend bail, the earlier failed attempts of a method that
/// compiled later. Interned, the allocations are bounded by the indy
/// instructions ever loaded, and the five doors agree on ONE site per
/// instruction (`docs/feature-designs/deopt-osr.md`).
///
/// Keyed by the class's redefine generation as well: a redefinition keeps the
/// `ClassId` and replaces the constant pool, and a site describes the pool it
/// was built from. Class ids are never reused after an unload
/// (`classloading::class` tombstones the slot), and a synthetic stub upgraded
/// in place has no bytecode to have compiled an indy from. Entries are never
/// freed, for the reason the allocation is process-lived: retired code may
/// still hold the pointer.
///
/// The caller holds the class-manager read guard `cm` that `class` came from;
/// the site is built under this table's lock, which nothing else takes, so the
/// order is always class manager, then table.
///
/// `instruction` is `(method name, method descriptor, bci)` of the
/// `invokedynamic` being compiled, or `None` when the door cannot tell which
/// of several instructions sharing `cp_idx` it is. A zero-capture lambda site
/// keys its singleton on it, so the table is per INSTRUCTION, not per CP entry
/// (`interpreter-L6-lambda-singleton-identity-splits-between-interpreter-and-jit-FIXED`,
/// fixed wave 5); `None` refuses such a site (see
/// `make_jit_indy_bridge_site_from_parts`).
pub(in crate::runtime::interpreter) fn interned_jit_indy_bridge_site(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    class_id: ClassId,
    cp_idx: u16,
    instruction: Option<(&str, &str, usize)>,
) -> usize {
    let singleton_site = instruction.map(|(name, descriptor, bci)| {
        crate::runtime::invokedynamic::lambda_singleton_site_for(name, descriptor, bci)
    });
    let (site_method, site_pc) = singleton_site.unwrap_or((0, usize::MAX));
    let key = (
        class_id.as_u32(),
        cm.class_redefine_generation(class_id),
        cp_idx,
        site_method,
        site_pc,
    );
    // JIT round 16 wave 7 (lane indy; `CRATONVM_JIT_INDY_LINKED_CALL_SITE`,
    // default OFF): an instruction the bridge does not implement natively is
    // admitted once the interpreter has LINKED it
    // (`make_jit_indy_linked_site_from_parts`). That fact changes over time, so
    // with the switch on a memoised `0` is asked again -- only that half, the
    // only one whose answer can change -- and replaced by the site once the
    // row exists. With the switch off the table behaves exactly as before (a
    // `0` is final).
    let linked_retry = crate::runtime::invokedynamic::jit_indy_linked_call_site_enabled();
    let build_linked = || {
        if !linked_retry {
            return 0;
        }
        crate::runtime::invokedynamic::make_jit_indy_linked_site_from_parts(
            shared.vm_identity,
            &class.constant_pool,
            cp_idx,
            class_id,
            singleton_site,
        )
        .unwrap_or(0)
    };
    let build = || match crate::runtime::invokedynamic::make_jit_indy_bridge_site_from_parts(
        &class.constant_pool,
        &class.bootstrap_methods,
        cp_idx,
        class_id,
        singleton_site,
    ) {
        Some(site) => site,
        None => build_linked(),
    };
    let site = match shared.jit.bridge.indy_bridge_sites.lock() {
        Ok(mut sites) => match sites.get(&key).copied() {
            Some(0) if linked_retry => {
                let site = build_linked();
                if site != 0 {
                    sites.insert(key, site);
                }
                site
            }
            Some(site) => site,
            None => {
                let site = build();
                sites.insert(key, site);
                site
            }
        },
        // A poisoned table still answers correctly, one leaked site at a time.
        Err(_) => build(),
    };
    // JIT round 16 wave 10 (lane indyir, IN16-8): a generic site learns the
    // method it compiles (first writer wins; a no-op for every other kind), so
    // binding a constant target can ask for that method's optimizing
    // recompile (`JitIndyGenericSite::request_constant_recompile`).
    if let Some((name, descriptor, _)) = instruction {
        crate::runtime::invokedynamic::note_jit_indy_site_method(site, name, descriptor);
    }
    // nio21 wave 12 L1 (W11L3-1 of `jit-r21-nio-proposals-w11-L3.md`): the
    // method's OWN constant-lambda sites learn their singleton slot here, at
    // the door, as the splice resolver's sites already do, so the first
    // compiled execution of an instruction the interpreter already minted is a
    // probe hit instead of one bridge call. A no-op for any other site kind,
    // for `0`, for a site not yet minted, and under
    // `CRATONVM_JIT_INDY_CONSTANT_PRIME=0` (checked inside). Lock order: the
    // caller's class-manager read guard, then the resolution cache's read lock
    // and the singleton table's -- the order `prime_jit_indy_constant_word`
    // documents; the intern table's lock is already released.
    let _ = crate::runtime::invokedynamic::prime_jit_indy_constant_word(shared, site);
    site
}

/// An accepted optimizing OSR artifact and the owners of the callee entries
/// baked into it. The body is never published through `JitCache`, so nothing
/// else roots those callees; the pins live exactly as long as the memo entry.
///
/// Both held as `RetainedCode`: since the memo moved into per-VM state it is
/// dropped with its VM, and a last reference must be released through the
/// retirement queue, never by a bare unmap.
pub(in crate::runtime::interpreter::jit_bridge) struct AcceptedOsrOptimizing {
    pub(in crate::runtime::interpreter::jit_bridge) artifact: cratonvm_jit::RetainedCode,
    pub(in crate::runtime::interpreter::jit_bridge) _callee_pins: Arc<[cratonvm_jit::RetainedCode]>,
    /// The declaring class's redefinition generation, read BEFORE the compile.
    /// An entry is served only while it is still current (round 11 wave 2,
    /// r11-tier-osr-optimizing-route-bypasses-static-gates item 2): a
    /// redefinition in between makes the entry a miss instead of every entry
    /// in a redefined class being refused forever and rebuilt on every attempt.
    pub(in crate::runtime::interpreter::jit_bridge) redefine_generation: u32,
    /// This VM's code-state epoch (`JitRealm::code_state_epoch`), read BEFORE
    /// the compile; see [`osr_optimizing_artifact_is_stale`].
    pub(in crate::runtime::interpreter::jit_bridge) code_state_epoch: u64,
    /// This VM's `JitCache::redefine_epoch`, read BEFORE the compile: the
    /// stamp of a refusal recorded about this body after it ran
    /// ([`note_osr_optimizing_value_equivalent_exit`]).
    pub(in crate::runtime::interpreter::jit_bridge) jit_redefine_epoch: u32,
    /// The bytecode the entering frame ran when the body was built. Served
    /// only to a frame running the same bytecode, which also keeps an obsolete
    /// frame (old bytecode, same key) and a colliding key hash from being
    /// handed a body built for other code.
    pub(in crate::runtime::interpreter::jit_bridge) code: Arc<[u8]>,
    /// Tier proposal 27 (round 11 wave 10, lane irexc): the build admitted a
    /// body that is NOT sentinel-free because every exit it has is exceptional
    /// and resolvable ([`osr_optimizing_exception_exits_admissible`]). The
    /// verdict needs the method's exception table, which the per-entry lookup
    /// does not have, so it is taken at build time and kept here.
    pub(in crate::runtime::interpreter::jit_bridge) exits_admitted: bool,
    /// Value-equivalent guard exits this artifact has taken while served from
    /// this entry (round 11 wave 13, lane irexc); see
    /// [`note_osr_optimizing_value_equivalent_exit`].
    pub(in crate::runtime::interpreter::jit_bridge) value_equivalent_exits: u32,
    /// Round 16 (lane osrdoor): the current burst of caught-exception exits
    /// (`(count, burst start)`) this artifact has taken while served from this
    /// entry; see [`note_osr_optimizing_handler_exit`].
    pub(in crate::runtime::interpreter::jit_bridge) handler_exit_burst: (u32, Option<std::time::Instant>),
    /// Withdrawn by [`note_osr_optimizing_value_equivalent_exit`]: a miss for
    /// [`osr_optimizing_cached`]. Kept in the map rather than removed, so the
    /// callee pins outlive any frame still running the body.
    pub(in crate::runtime::interpreter::jit_bridge) withdrawn: bool,
    /// The process invalidation epoch this body's dependencies were last
    /// found current through (`cratonvm_jit::jit_invalidation_epoch`, read
    /// before that check); starts at the body's own compile-start stamp. See
    /// [`osr_optimizing_dependencies_current`].
    pub(in crate::runtime::interpreter::jit_bridge) dependencies_checked_through: std::sync::atomic::AtomicU64,
    /// al16 (2026-10-05): the first-call cells of the declaring class still
    /// armed when the body was remembered ([`first_call_cell_for`]), as
    /// addresses. A site the compile door found unresolved is built as a plain
    /// call with a first-execution guard -- never spliced -- so a loop the
    /// method had not run yet when its OSR body was built (the iteration loop
    /// after a fill loop) keeps those calls for the life of the entry. Once
    /// one of them has fired, [`osr_optimizing_cached`] forgets the entry and
    /// the door rebuilds it with the site resolved
    /// ([`osr_rebuild_resolved_first_calls_enabled`]).
    pub(in crate::runtime::interpreter::jit_bridge) first_call_cells_at_build: Box<[usize]>,
    /// JIT round 21 (lane typecheck, TYPECHECK21-8,
    /// `CRATONVM_JIT_OSR_REBUILD_STATIC_INIT`): the classes of the body's
    /// static accesses (`CompiledMethod::static_init_classes`) that were NOT
    /// initialized when it was remembered. Those sites are `jit_getstatic` /
    /// `jit_putstatic` helper calls (a direct access needs a completed
    /// `<clinit>`), and the method-entry door's recompile on initialization
    /// (`note_static_init_waiters`) does not run for this door; once one of
    /// them is initialized, [`osr_optimizing_cached`] forgets the entry and
    /// the door rebuilds it with direct accesses, as for a fired first-call
    /// cell above. Empty with the switch off.
    pub(in crate::runtime::interpreter::jit_bridge) static_init_pending_at_build: Box<[u32]>,
    /// JIT round 19 wave 2 (lane osr2, OSR19-1,
    /// `CRATONVM_JIT_OSR_POLY_DECLINE_PER_HEADER`): remembered under a header
    /// the body was NOT built for (the build's "every other header" loop), and
    /// the polymorphic-loop decline has not been asked for that header yet.
    /// [`osr_optimizing_cached`] asks it once, at the header's first lookup.
    pub(in crate::runtime::interpreter::jit_bridge) poly_decline_unasked: bool,
}

/// `CRATONVM_JIT_OSR_REBUILD_RESOLVED_FIRST_CALLS` (al16, 2026-10-05): does a
/// cached optimizing OSR body that was built while a call site of its class
/// was still unresolved get rebuilt once that site's first-call guard has
/// fired (`AcceptedOsrOptimizing::first_call_cells_at_build`)? HotSpot's
/// compilers leave such a site to an uncommon trap and recompile; without
/// this the body keeps the plain call forever. Read once per process; on by
/// default (`=0` keeps the body).
pub(in crate::runtime::interpreter::jit_bridge) fn osr_rebuild_resolved_first_calls_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The armed first-call cells of `class_id` ([`first_call_cell_for`]), as
/// addresses, for [`AcceptedOsrOptimizing::first_call_cells_at_build`].
pub(in crate::runtime::interpreter::jit_bridge) fn armed_first_call_cells_of(shared: &SharedVm, class_id: u32) -> Box<[usize]> {
    if !osr_rebuild_resolved_first_calls_enabled() {
        return Box::default();
    }
    let Ok(cells) = shared.jit.bridge.first_call_cells.lock() else {
        return Box::default();
    };
    cells
        .iter()
        .filter(|(&(class, _, _), cell)| {
            class == class_id && cell.armed.load(std::sync::atomic::Ordering::Acquire) != 0
        })
        .map(|(_, cell)| &**cell as *const cratonvm_jit::JitFirstCallCell as usize)
        .collect()
}

/// Has one of `cells` ([`armed_first_call_cells_of`]) fired since?
pub(in crate::runtime::interpreter::jit_bridge) fn some_first_call_cell_fired(cells: &[usize]) -> bool {
    cells.iter().any(|&cell| {
        // SAFETY: a first-call cell is boxed in `JitBridgeState::first_call_cells`
        // for the life of the VM that owns the memo entry holding its address.
        let cell = unsafe { &*(cell as *const cratonvm_jit::JitFirstCallCell) };
        cell.armed.load(std::sync::atomic::Ordering::Acquire) == 0
    })
}

/// `CRATONVM_JIT_OSR_REBUILD_STATIC_INIT` (JIT round 21, lane typecheck,
/// TYPECHECK21-8; perf, **opt-in** since the w1f A/B, `=1` arms it): does a cached
/// optimizing OSR body built while a class whose statics it reads or writes
/// was not initialized get rebuilt once that class's `<clinit>` completed
/// ([`AcceptedOsrOptimizing::static_init_pending_at_build`])? The OSR twin of
/// `CRATONVM_JIT_STATIC_INIT_RECOMPILE`. Read once per remembered body, never
/// on the lookup path (an empty list there is the switch off).
///
/// Opt-in after host w1f: default ON lost on every `R20PIRTableMod` row,
/// control rows included (iremonly 335 vs 226 ns). The first form recorded a
/// class as pending when `ClassInitBits::is_set` was false, but that bitmap
/// is a lazily filled MEMO (`set_as_of`, on the interpreter's
/// `ensure_class_initialized_shared` path), not the initialization state: a
/// long-initialized class whose bit had not been filled yet was "pending",
/// and the first later fill forgot a good body and forced an OSR rebuild,
/// once per such class. [`static_init_pending_of`] now asks the class's own
/// `init_state`; re-measure before flipping it back on.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_rebuild_static_init_enabled() -> bool {
    false
}

/// The static-access classes of `artifact` not initialized now, for
/// [`AcceptedOsrOptimizing::static_init_pending_at_build`]. A class whose
/// initialization completed between the compile and this read is missed (its
/// sites stay helper calls for the entry's life, as before).
pub(in crate::runtime::interpreter::jit_bridge) fn static_init_pending_of(
    shared: &SharedVm,
    artifact: &cratonvm_jit::CompiledMethod,
) -> Box<[u32]> {
    if artifact.static_init_classes.is_empty() || !osr_rebuild_static_init_enabled() {
        return Box::default();
    }
    // The authoritative state (`init_state`), not the lazily filled
    // `class_init_bits` memo: a class initialized long ago may have no bit
    // yet (see `osr_rebuild_static_init_enabled`). Off the hot path: once per
    // remembered body.
    let cm = shared.classes.class_manager.read();
    artifact
        .static_init_classes
        .iter()
        .copied()
        .filter(|&class_id| {
            class_id != 0
                && cm
                    .get_class(ClassId::new(class_id))
                    .is_some_and(|c| !crate::vm::is_class_initialized_fast(c))
        })
        .collect()
}

/// Has one class of a body's [`static_init_pending_of`] list finished
/// initializing? The trigger [`osr_optimizing_cached`] asks on the lookup
/// path, so it must ask the same authority the list was recorded from: the
/// class's own `init_state` (JIT round 21, lane smallfix). The
/// `class_init_bits` memo is filled lazily (the interpreter's ensure path), so
/// a class initialized from compiled code, a native or another thread's
/// helper may never get its bit and the body was never forgotten. A set bit is
/// exact (initialization is monotonic), so it stays a lock-free positive; for
/// the rest the class manager is read without blocking (`try_read_untracked`:
/// the memo's lock is held here, and a non-blocking read adds no order edge).
/// A busy class manager is a "not yet": the next lookup asks again. An empty
/// list (the switch off, or nothing pending) pays nothing. Also the
/// "initialized" test of the non-OSR twin, [`note_static_init_waiters`].
pub(in crate::runtime::interpreter::jit_bridge) fn static_init_pending_any_initialized(shared: &SharedVm, pending: &[u32]) -> bool {
    if pending.is_empty() {
        return false;
    }
    if pending
        .iter()
        .any(|&class_id| shared.classes.class_init_bits.is_set(class_id))
    {
        return true;
    }
    let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
        return false;
    };
    pending.iter().any(|&class_id| {
        class_id != 0
            && cm
                .get_class(ClassId::new(class_id))
                .is_some_and(|c| crate::vm::is_class_initialized_fast(c))
    })
}

/// Cold-path authoritative check of whether `class_id` is initialized: checks
/// the lock-free `class_init_bits` memo first, then blocks on
/// `class_manager.read()` (unlike [`static_init_pending_any_initialized`]'s
/// `try_read_untracked`, which is for hot OSR lookups). Used by cold
/// waiter-registration doors where no `class_manager` or `late_bind_waiters`
/// lock is held, so a concurrent `class_manager.write()` cannot cause a missed
/// initialization and register an unserved waiter (F4).
pub(in crate::runtime::interpreter::jit_bridge) fn static_init_class_is_initialized(shared: &SharedVm, class_id: u32) -> bool {
    if class_id == 0 {
        return false;
    }
    if shared.classes.class_init_bits.is_set(class_id) {
        return true;
    }
    let cm = shared.classes.class_manager.read();
    cm.get_class(ClassId::new(class_id))
        .is_some_and(|c| crate::vm::is_class_initialized_fast(c))
}

/// What an optimizing OSR build reads BEFORE its compile and keeps with the
/// accepted body ([`AcceptedOsrOptimizing`]), so a redefinition or flush
/// racing the compile leaves the entry stale rather than current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct OsrBuildStamps {
    /// The declaring class's redefinition generation
    /// ([`osr_redefine_generation`]).
    pub(in crate::runtime::interpreter::jit_bridge) redefine_generation: u32,
    /// This VM's code-state epoch (`JitRealm::code_state_epoch`); see
    /// [`osr_optimizing_artifact_is_stale`].
    pub(in crate::runtime::interpreter::jit_bridge) code_state_epoch: u64,
    /// This VM's `JitCache::redefine_epoch`: what a refusal this build
    /// records is stamped with ([`note_osr_optimizing_refusal`]).
    pub(in crate::runtime::interpreter::jit_bridge) jit_redefine_epoch: u32,
    /// The process redefinition clock (`class_redefinition_count`) as the
    /// build began: [`osr_build_spans_a_redefinition`]. Interpreter round i6
    /// wave 117, lane L3.
    pub(in crate::runtime::interpreter::jit_bridge) redefinition_count: u64,
}

impl OsrBuildStamps {
    /// Every stamp, as of now.
    pub(in crate::runtime::interpreter::jit_bridge) fn read(shared: &SharedVm, class_id: ClassId) -> Self {
        Self {
            code_state_epoch: shared.jit.code_state_epoch(),
            jit_redefine_epoch: shared.jit.jit_cache.redefine_epoch(),
            redefine_generation: osr_redefine_generation(shared, class_id),
            redefinition_count: cratonvm_classloading::class_redefinition_count(),
        }
    }
}

/// Did this VM redefine any class while an optimizing OSR build that began at
/// the redefinition clock `started_at` ran? Interpreter round i6 wave 117,
/// lane L3 (stage 3 of `i14-L3-proposal-class-scoped-redefinition-invalidation`):
/// with `CRATONVM_JIT_SCOPED_REDEFINITION_KEEPS_CODE_STATE` a scoped
/// redefinition no longer moves the code-state epoch that
/// [`osr_optimizing_artifact_is_stale`] reads, and such a body bypasses
/// `JitCache`'s per-class barrier; this refuses it the same way, from this
/// VM's redefinition log (lock-free), conservatively: any redefinition of this
/// VM in the span, or a log that cannot answer. One load while nothing moved.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_build_spans_a_redefinition(shared: &SharedVm, started_at: u64) -> bool {
    let now = cratonvm_classloading::class_redefinition_count();
    if now == started_at {
        return false;
    }
    let mut any = false;
    let answered = shared
        .classes
        .redefinition_log
        .for_each_between(started_at, now, |_| any = true);
    any || !answered
}

/// The declaring class's redefinition generation, without the class-manager
/// lock on a VM that has never redefined anything.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_redefine_generation(shared: &SharedVm, class_id: ClassId) -> u32 {
    if !crate::classloading::any_class_redefined() {
        return 0;
    }
    shared
        .classes
        .class_manager
        .read()
        .class_redefine_generation(class_id)
}

/// The same bytecode: the same buffer, or equal contents.
pub(in crate::runtime::interpreter::jit_bridge) fn same_bytecode(a: &Arc<[u8]>, b: &Arc<[u8]>) -> bool {
    Arc::ptr_eq(a, b) || a[..] == b[..]
}

/// Class name for the `CRATONVM_DBG_COMPACT_INLINE` engagement census, or a
/// `<class_id=N>` placeholder when the class store cannot name it.
///
/// Diagnostic-only, and deliberately `#[cold]`: the census fires once per
/// unresolved field site at COMPILE time, never on the execution path it is
/// reporting about.
#[inline(never)]
#[cold]
pub(in crate::runtime::interpreter::jit_bridge) fn declaring_class_name_for_diag(shared: &SharedVm, class_id: ClassId) -> String {
    let cm = shared.classes.class_manager.read();
    cm.get_class(class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<class_id={}>", class_id.as_u32()))
}

/// `CRATONVM_DBG_JITC` diagnostic: name the cache state that forced an OSR
/// recompile — `no-cached-artifact` (the one legitimate case),
/// `cached-not-via-osr`, or `cached-cannot-enter-at-pc`.
///
/// The last one is the interesting one: a PUBLISHED `compiled_via_osr` artifact
/// that cannot be entered at `entry_pc` will never become enterable
/// (`osr_pc_to_native[entry_pc]` is a pure function of the bytecode and the
/// entry pc — the codegen writes `-1` for a pc strictly inside a LICM-hoisted
/// loop body), so every recompile rebuilds it byte for byte. Measured on the
/// `CallRate.allocPutOld` probe: 200 full C2 pipelines per 200 000 iterations,
/// 199 of them this case, with the loop interpreted throughout.
///
/// `#[inline(never)]` + `#[cold]` keep the formatting temporaries of a
/// debug-only path out of `compile_osr_artifact`'s frame. That caller is ~1100
/// lines and runs on the mutator stack, and a frame reservation is
/// unconditional even for a branch that never executes without the env var —
/// so this is worth keeping out of line on principle, cheaply.
///
/// (Honesty note for the next reader: this was briefly *suspected* of causing
/// a `main-vm` stack overflow in `TestDefaultServlet`. It does not. That crash
/// reproduces on binaries with no diagnostic and no OSR change at all — it is
/// a pre-existing flaky, load-dependent overflow on `dev`, seen once in four
/// runs of an unmodified baseline binary on a heavily loaded host. Do not read
/// these attributes as fixing anything.)
#[inline(never)]
#[cold]
pub(in crate::runtime::interpreter) fn dbg_osr_recompile_reason(
    cached_osr: Option<&crate::jit::CompiledMethod>,
    despec_stale: bool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    entry_pc: usize,
) {
    if !false {
        return;
    }
    let why = match cached_osr {
        None => "no-cached-artifact",
        Some(c) if !c.compiled_via_osr => "cached-not-via-osr",
        Some(_) if despec_stale => "cached-keeps-a-despeculated-guard",
        // JIT round 23 wave 1, lane L1: armed for an OSR profit exit.
        Some(c) if c.osr_profit_exit_armed() => "cached-armed-for-a-profit-exit",
        Some(_) => "cached-cannot-enter-at-pc",
    };
    eprintln!(
        "[cratonvm-jitc] OSR-recompile reason={why} {class_name}.{method_name}{method_descriptor} entry_pc={entry_pc}"
    );
}

/// Why the OSR door must leave a statically bound callee on the checked
/// dispatch helper — `None` when it may bake a direct machine-code `CALL`.
///
/// This is the OSR door's copy of the refusal `callee_compiler` and
/// `direct_callee_lookup` each spell out, and until 2026-08-24 it disagreed
/// with both: it refused **every** callee declaring an exception table,
/// unconditionally, while the other two doors had asked
/// `direct_call_exc_table_publish_enabled` since that gate existed and stopped
/// refusing when it was flipped default-ON on 2026-08-21.
///
/// That disagreement is the whole of the OSR half of
/// `osr-door-refused-every-exception-table-callee-FIXED-20260824.md`. The gate's flip
/// moved the ordinary-frame rung of `probes/NativeFunnelFloorProbe.java` from
/// 107.30 to 10.57 ns/op and left the OSR rung at ~101 in both arms, and the
/// page read that — correctly, given what it could see — as "an OSR body never
/// consults the gate at all". It does not consult it because this predicate
/// never asked; a hot loop is always an OSR body, so the sites that stand to
/// gain most from a direct call were by construction the ones excluded.
///
/// Nothing else about the OSR door needed to change for this to be sound. The
/// gate's two stated preconditions are properties of the EMITTER, not of the
/// door: `emit_inline_callee_deopt_check` is emitted after the baked `CALL` by
/// the same `x64/bytecode_walk.rs` arms this door reaches through
/// `compile_with_param_slots`, and a site that cannot reserve the service
/// slots fails the compile (`direct-call-service-slots`) rather than emitting
/// an unserviced edge. The OSR ladder already registers a `JitInvokeInfo` for
/// every direct-bound site, which is what that check reads.
///
/// The two refusals that are NOT about the gate stay unconditional: a callee
/// this door cannot resolve, and a callee with no `Code` attribute at all —
/// there is no body to bake a `CALL` to. `direct_callee_lookup` maps the
/// no-`Code` case onto `CalleeExceptionTable` as well, and this mirrors it so
/// the census table means the same thing at all three doors.
pub(in crate::runtime::interpreter) fn osr_callee_bars_direct_call(
    shared: &SharedVm,
    caller_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<cratonvm_jit::DirectBindRefusal> {
    let cm = shared.classes.class_manager.read();
    let Some(callee_cid) = jit_known_class(shared, &cm, callee_class, caller_class_id) else {
        return Some(cratonvm_jit::DirectBindRefusal::CalleeClassNotFound);
    };
    let store = cm.class_store();
    let Some((method, _decl)) =
        crate::classloading::find_method_recursive(callee_cid, callee_method, callee_desc, store)
    else {
        return Some(cratonvm_jit::DirectBindRefusal::CalleeMethodNotFound);
    };
    match method.code() {
        None => Some(cratonvm_jit::DirectBindRefusal::CalleeExceptionTable),
        Some(code)
            if !code.exception_table.is_empty()
                && !cratonvm_jit::direct_call_exc_table_publish_enabled() =>
        {
            Some(cratonvm_jit::DirectBindRefusal::CalleeExceptionTable)
        }
        Some(_) => None,
    }
}

thread_local! {
    /// The last checkpoint [`compile_osr_artifact`] reached on this thread.
    ///
    /// That function has ~25 bare `return None`s and as many `?` operators, and
    /// an OSR refusal is SILENT: the loop just runs interpreted forever while
    /// `OSR-recompile reason=no-cached-artifact` repeats. The comment on
    /// `pending_callee_compiles` already records one defect that cost 5x and
    /// looked exactly like its own fix; this is the same failure mode with no
    /// diagnostic at all.
    ///
    /// Measured on netty's `FastLz.compress` — a 1617-byte method whose whole
    /// job is one loop, so OSR is its ONLY route to compiled code. It refuses,
    /// is marked OSR-denied for the process, and 256 MiB of compression runs in
    /// the interpreter at 138x HotSpot. The named denies (athrow, indy,
    /// exception table, newarray) all printed nothing, because none of them was
    /// the one that fired.
    pub(in crate::runtime::interpreter::jit_bridge) static OSR_STAGE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("entry") };
}

/// Record the region [`compile_osr_artifact`] has reached.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_stage(stage: &'static str) {
    OSR_STAGE.with(|c| c.set(stage));
}

/// The last region [`compile_osr_artifact`] reached on this thread, for the
/// refusal report. Read on the SAME thread that ran the compile — the
/// background worker — which is where the `if !published` arm runs.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_stage_get() -> &'static str {
    OSR_STAGE.with(std::cell::Cell::get)
}

// ---------------------------------------------------------------------------
// Which interpreter frames are, right now, being run by compiled code
// ---------------------------------------------------------------------------
//
// jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902, defect (3):
// a trace captured after `main` has OSR'd says `main:62` -- the back-edge it
// tiered up at -- where HotSpot says `main:66`, the call that was executing.
//
// ## The mechanism, read off this file rather than assumed
//
// `try_osr` enters through `osr_enter_planned` and the artifact runs the method
// to its RETURN: the value comes back through that function's `ret_type`
// conversion and `try_osr_with_backoff` turns it into
// `OsrBackoffOutcome::ReturnOuter`. Compiled code therefore does NOT stop at
// the loop exit -- everything after the loop, and every call the method makes
// from there on, executes inside the artifact while the interpreter `Frame` for
// that same activation sits untouched on `thread.frames` with `pc == entry_pc`.
// A capture taken from inside that window (a throw in a callee, or another
// thread's `Thread.getStackTrace()`) walks `thread.frames` and reports the loop
// header for a method that is executing far below it. That is the *during*
// sub-case, and it is the only one `probes/StackTraceAfterOsr.java` exercises.
//
// The *after* sub-case -- the interpreter continuing past the loop with a stale
// pc -- does not exist here, and that was checked rather than assumed. Every
// exit that leaves this frame alive already writes a pc: the OSR-exit transfer
// (`deopt_resume::transfer_osr_exit_into_live_frame`) assigns
// `frame.pc = resume_bci`, the RBC.6b handler entry assigns
// `frame.pc = handler_pc`, and the safe-reject path deliberately leaves
// `entry_pc` standing because by admission nothing was committed before it.
// The only other way out is the normal return, which pops the frame.
//
// ## Why the pc itself must not be moved
//
// Two independent reasons, either one sufficient:
//
//   * `Frame::live_locals_mask_here` and `Frame::scan_local_objects_inner`
//     compute the per-bci live-locals ROOT FILTER from
//     `[self.pc, self.last_instr_pc]`. Advancing `pc` to where compiled code
//     really is would make every slot that dies in between stop being a root --
//     on a frame whose locals are the pre-OSR copies that the conservative half
//     of the JIT root scan is leaning on.
//   * The safe-reject exit above is correct only BECAUSE `frame.pc` is still
//     `entry_pc`. Moving it would resume the interpreter at a bci this
//     activation never reached, which is the silent-corruption shape RBC.7 is
//     named for.
//
// So the refresh has to be a SIDE CHANNEL that the trace assembler reads and
// neither the root scan nor any resume path can see. `Frame` is not this file's
// to widen, so the channel is this registry.
//
// ## What this registry can and cannot answer
//
// It answers, authoritatively, WHICH interpreter frame is a live OSR
// continuation and OF WHICH artifact -- two facts only the entry site knows.
// `stackwalker::drop_osr_continuations` infers the first from
// `cm.can_osr_enter(frame.pc)`, which is a property of a pc and not of an
// activation: an interpreted frame genuinely parked on a back-edge while a
// RECURSIVE compiled activation of the same method is live satisfies it too,
// and that frame's compiled entry is then dropped from the trace.
//
// It cannot answer the current bci by itself, and no honest version of it can.
// The bci compiled code is at lives in that activation's own frame at
// `[rbp - cm.sp_id_slot_off]` -- the safepoint-id slot every GC-capable site
// stores its `OopMapEntry::bytecode_pc` into, which is the mapping deopt and
// the precise root scan already share. The OSR activation's RBP is reachable
// only from the saved-RBP chain walk in `vm/src/jit/conservative_roots.rs`:
// `top_rbp_mirror_read` names the INNERMOST compiled frame, and by capture time
// that is some callee's, while the chain entry's `exact_rbp` has been
// overwritten by every prologue that ran underneath it. Re-deriving the bci any
// other way would be a SECOND pc->bci mapping beside deopt's, which is how this
// class of defect gets made in the first place. The bci must therefore come
// from the compiled entry `conservative_roots::active_compiled_frames` already
// reports for this same activation, matched to the interpreter frame by the
// pair below. See `.agent-requests/A7-wiring.txt`.

/// Kill switch for the OSR-continuation registry.
///
/// Default ON. `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` stops the registry being
/// written and makes `live_osr_continuation_artifact` answer `None` everywhere,
/// so every consumer falls back to the pc-shaped heuristic it used before --
/// one binary, both answers.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_pc_refresh_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| !false)
}

