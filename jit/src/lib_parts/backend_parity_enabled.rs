#[allow(unused_imports)]
use super::super::*;

/// `CRATONVM_JIT_BACKEND_PARITY=1` — when the optimizing tier produces a body,
/// compile the SAME method with the single-pass backend as well, compare what
/// the two bodies contain, and throw the single-pass one away.
///
/// # default on, and it has to stay that way in production
///
/// With the flag on, **every IR-tier compile pays for a full single-pass
/// compile it discards**: every constant-pool resolution, every inline plan,
/// every callee compile the single-pass inliner asks `callee_compiler` for, the
/// whole x64 selection and encoding pass, and one executable buffer allocated
/// and immediately freed. That is roughly a doubling of compile CPU on exactly
/// the methods the compiler spends the most on. It is fine for a corpus run and
/// unacceptable in production. This is a diagnostic instrument, not a policy.
///
/// # What it buys
///
/// `x64/single_pass_only.rs` is a hand-maintained list of six lowerings the
/// single-pass backend has and the IR tier does not, and its own header states
/// the limit: it "cannot catch one nobody has written down — a future
/// single-pass specialisation added without a variant below is invisible to it,
/// which is exactly the failure `cov-02` demonstrated one level up". `cov-02`
/// cost `CratonBench.sieve([ZI)I` 2,462 ms -> 15,823 ms with an unchanged
/// checksum and no failing test. This flag is the machine that would have
/// caught it: [`backend_parity`] decodes both bodies and reports a capability
/// the baseline body has and the optimizing body does not.
///
/// # Deliberately NOT cached in a `OnceLock`
///
/// Read once per optimizing-tier compile, which is nowhere near hot enough to
/// need a cache — and caching is precisely what makes
/// `x64::single_pass_only::c1_vector_veto_enabled` impossible to flip per
/// compile, which is obstacle (2) in `backend_parity`'s own REVIEW-NOTE. Not
/// repeating that here means a test can arrange this flag on one thread through
/// `cratonvm_types::flags::with_thread_overrides` instead of needing a whole
/// process.
pub(crate) fn backend_parity_enabled() -> bool {
    true
}

/// Compile `req`'s method a second time with the single-pass backend and hand
/// both bodies to [`backend_parity::compare_bodies`]. Diagnostic only.
///
/// Called from [`try_compile_inner`] on the path where [`ir_tier`] SUCCEEDED,
/// and only when [`backend_parity_enabled`]. The artifact built here is never
/// installed and never published; it is measured and dropped. **The flag cannot
/// change which body runs** — `try_compile_inner` returns the optimizing
/// artifact it already had, untouched, whatever happens in here.
///
/// # What it can compare, and what it cannot
///
/// On the label, because `single_pass_only.rs` is right that "a parity check
/// that only *looks* general is worse than one whose limits are on the label":
///
///   * **Only methods the optimizing tier TAKES are compared.** This is the
///     path after `ir_tier` returned `Some`. A method the IR tier declines
///     never reaches here and is not compared — and it does not need to be,
///     since its installed body IS the single-pass one.
///   * **The six shapes most worth comparing are, by default, not comparable
///     at all.** `single_pass_only_lowering_for` declines the optimizing tier
///     for exactly those shapes, so on them `ir_tier` returns `None` and
///     control never gets here. **A corpus run that wants them therefore has to
///     set `CRATONVM_JIT_C1_VECTOR_VETO=0` for the whole process** — whole
///     process because `c1_vector_veto_enabled` caches its answer in a
///     `OnceLock` and cannot be flipped per compile.
///   * **A body the decoder cannot walk yields no finding in either
///     direction.** [`backend_parity::profile_body`] fails closed and counts
///     itself blind; three of the six `SinglePassOnly` variants have no byte
///     signature at all. See that module's header.
///
/// # Containment
///
/// The shadow compile runs inside [`tiered::contain_compile_panic`], the same
/// containment the background compile worker uses, so a panic anywhere in the
/// second compile is caught, counted as blind and reported — never allowed to
/// unwind through a compile that has already succeeded. Its thread-local
/// depth counter also tells the VM's crash handler not to write an
/// `hs_err_pid` report for a panic that is about to be swallowed.
///
/// Three further containments, none of which `catch_unwind` provides:
///
///   * `backend_attempted` is a LOCAL here, never the caller's. That flag is
///     what routes a method onto the permanent bail-list, and a diagnostic
///     compile must not bail-list a method whose real compile succeeded.
///   * The recorder is [`metrics::CompileRecorder::disabled`], not the live
///     one. A shadow `enter_single_pass` on the real recorder would report the
///     optimizing tier as having fallen through when it did not.
///   * Three thread-local compile diagnostics are snapshotted and restored:
///     the fall-through signal `last_compile_fell_through_to_single_pass`
///     reads, the pipeline stage `take_jit_pipeline_stage` reads, and the bail
///     site `take_jit_bail_site` reads. All three are written unconditionally
///     by the single-pass tier, and leaving the shadow's values behind would
///     make the *real* compile's post-mortem lie.
///
/// # Two costs it does NOT contain, stated rather than hidden
///
///   * The caller's admission token is reused rather than a second one being
///     opened. That is right for the epoch witness — this is the same compile
///     of the same method — but it does mean the shadow's executable buffer is
///     allocated without a fresh code-cache-cap check. It is freed again before
///     this function returns, so the cap is not durably exceeded; a corpus run
///     should still expect a transiently higher code-cache high-water mark.
///   * Everything `build_single_pass_tables` is not pure about happens twice.
///     Its own doc enumerates that, and the callee compiles the single-pass
///     inliner asks for are the expensive item.
#[allow(clippy::too_many_arguments)]
pub(crate) fn backend_parity_shadow_compile(
    req: &CompileRequest<'_>,
    self_call_identity_stable: bool,
    admission: &compile_gate::CompileAdmission,
    local_handlers_disarmed: bool,
    code: &[u8],
    code_len: usize,
    scan: x64::JitScanResult,
    prologue_param_slots: usize,
    precise_exception_frames: bool,
    resolved_string_layout: Option<StringFieldLayout>,
    optimizing: &CompiledMethod,
) {
    let cached = req.cached;
    let method = backend_parity::MethodId {
        class_name: &*cached.class_name,
        method_name: &*cached.method_name,
        descriptor: &*cached.method_descriptor,
    };

    // Copied, not borrowed, and copied BEFORE the shadow compile: the analyser
    // is handed two `&[u8]`, and holding a borrow of the real artifact across a
    // `catch_unwind` boundary is a complication with no upside here. It is one
    // memcpy of a few hundred bytes on a path that is about to run a whole
    // second compile.
    let optimizing_bytes = optimizing.code_bytes().to_vec();

    // See the "Containment" section above: these three are written
    // unconditionally by `single_pass_tier` and belong to the REAL compile.
    let fell_through_before = IR_FELL_THROUGH_TO_SINGLE_PASS.with(|c| c.get());
    let stage_before = JIT_PIPELINE_STAGE.with(|c| c.get());
    let bail_site_before = JIT_BAIL_SITE.with(|c| c.get());

    let shadow = tiered::contain_compile_panic(|| {
        let mut shadow_attempted = false;
        single_pass_tier(
            req,
            &mut shadow_attempted,
            self_call_identity_stable,
            admission,
            local_handlers_disarmed,
            &metrics::CompileRecorder::disabled(),
            code,
            code_len,
            scan,
            prologue_param_slots,
            precise_exception_frames,
            resolved_string_layout,
        )
    });

    IR_FELL_THROUGH_TO_SINGLE_PASS.with(|c| c.set(fell_through_before));
    JIT_PIPELINE_STAGE.with(|c| c.set(stage_before));
    JIT_BAIL_SITE.with(|c| c.set(bail_site_before));

    let shadow = match shadow {
        Ok(Some(cm)) => cm,
        // NOT a finding, and not silence either. The single-pass tier declined
        // — a constant-pool resolver that could not answer, a backend bail — so
        // there is no baseline body and nothing to compare. Counted as blind
        // because `backend_parity`'s whole argument is that "no finding" and
        // "not looked at" must not be the same number.
        Ok(None) => {
            backend_parity::note_comparison_blind(
                method,
                "the shadow single-pass compile declined",
            );
            return;
        }
        Err(payload) => {
            backend_parity::note_comparison_blind(
                method,
                &format!(
                    "the shadow single-pass compile PANICKED ({}); the installed \
                     optimizing body is untouched",
                    tiered::panic_payload_message(&*payload)
                ),
            );
            return;
        }
    };

    backend_parity::compare_bodies(method, shadow.code_bytes(), &optimizing_bytes);

    // Explicit, because it is the whole contract of this function: the second
    // artifact existed only to be measured. Dropping it frees its executable
    // buffer and the arenas `single_pass_tier` attached to it; nothing ever
    // held a pointer to either, because it was never published.
    drop(shadow);
}

/// Everything [`x64::compile_with_param_slots`] CONSUMES, resolved once.
///
/// Split out of [`single_pass_tier`] so that resolving the tables and running
/// the backend are separable — which is what makes a second backend run on the
/// same method possible at all. See [`build_single_pass_tables`].
///
/// # The `Clone` is real, and it has one hazard. Read this before using it.
///
/// Four of these tables hold RAW POINTERS into arenas that the sibling
/// [`SinglePassParts`] owns:
///
/// | table | arena |
/// |---|---|
/// | `typecheck_info` | `owned_strings` |
/// | `invoke_info` | `owned_invoke_infos` (and `owned_strings`) |
/// | `mic_slots` | `owned_mic_slots` |
/// | `pic_slots` | `owned_pic_slots` |
///
/// A clone copies the pointers. It does not copy the pointees. So a clone is
/// valid only while the `SinglePassParts` it was built beside is alive, and —
/// the part that matters — **at most one artifact built from a given
/// `SinglePassParts` may ever be INSTALLED**, because installation is what
/// moves those arenas onto the `CompiledMethod` (`_jit_strings`,
/// `_jit_invoke_infos`, `_jit_mic_slots`, `_jit_pic_slots`) and so what makes
/// the pointees outlive the machine code that bakes their addresses. Two
/// installed bodies sharing one arena is the `BUG-24` use-after-free written up
/// in `single_pass_tier`'s tail, one level up.
///
/// Nothing on the production path clones: `single_pass_tier` moves the tables
/// straight into [`SinglePassTables::run_backend`], and
/// `backend_parity_shadow_compile` builds a SECOND, independent plan rather
/// than cloning this one. The only caller that clones is the unit test
/// `single_pass_table_build_is_callable_twice`, which compares the two bodies'
/// bytes and drops both without installing either.
#[allow(clippy::type_complexity)]
#[derive(Clone)]
pub(crate) struct SinglePassTables {
    pub(crate) param_slots: usize,
    pub(crate) mna_info: Vec<(usize, i64)>,
    pub(crate) field_info: Vec<(usize, usize, u8)>,
    pub(crate) typecheck_info: Vec<(usize, *const u8, usize)>,
    pub(crate) static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    pub(crate) new_info: Vec<(usize, u32, usize, bool, bool)>,
    pub(crate) new_deferred_info: Vec<(usize, u32, u16)>,
    pub(crate) anewarray_info: Vec<(usize, u32)>,
    pub(crate) anewarray_deferred_info: Vec<(usize, u32, u16)>,
    pub(crate) invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    pub(crate) direct_calls: Vec<(usize, JitDirectCall)>,
    pub(crate) mic_slots: Vec<(usize, *const JitMICSlot)>,
    pub(crate) pic_slots: Vec<(usize, *const JitPICSlot)>,
    pub(crate) ldc_info: Vec<(usize, i64)>,
    pub(crate) ldc_string_info: Vec<(usize, u32, u16)>,
    pub(crate) ldc_class_info: Vec<(usize, u32, u16)>,
    pub(crate) ldc2w_info: Vec<(usize, i64)>,
    pub(crate) ldc_fp_pcs: FxHashSet<usize>,
    pub(crate) branch_hints: std::collections::HashMap<usize, bool>,
    pub(crate) loop_unroll_hints: std::collections::HashMap<usize, usize>,
    pub(crate) inline_sites: HashMap<usize, InlineSite>,
    pub(crate) inline_guard_variants: HashMap<usize, Vec<(u32, InlineSite)>>,
    pub(crate) string_layout: Option<StringFieldLayout>,
    pub(crate) param_oop_mask: u64,
    pub(crate) compact_field_info: Vec<(usize, u32, bool)>,
    pub(crate) indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)>,
    pub(crate) sb_chain_fusions: std::collections::HashMap<usize, sb_chain_fuse::SbChainFusion>,
    pub(crate) elidable_init_pcs: Option<std::collections::HashSet<usize>>,
    pub(crate) backend: x64::BackendRequest,
    /// JIT round 19 wave 5 (lane sync5, SYNCDOOR4-2): the dispatch infos of
    /// the synchronized self-call sites planned as direct self-calls; see
    /// [`SinglePassTables::unbind_sync_self_calls`]. Never read by the backend.
    pub(crate) sync_self_call_fallback: Vec<(usize, *const JitInvokeInfo)>,
}

/// The other half of one resolved single-pass plan: what the backend only
/// BORROWS, plus what the finished artifact has to be decorated with.
///
/// Not merged into [`SinglePassTables`] for two reasons, both load-bearing.
/// The arenas must survive a backend run that consumes the tables, so they
/// cannot live in the consumed half. And they must not be `Clone`: cloning an
/// arena would produce a second set of `Box`es that no pointer in any table
/// refers to, which is a silent way to build a table pointing at freed memory.
pub(crate) struct SinglePassParts {
    pub(crate) needs_heap: bool,
    pub(crate) param_jvm_slots: Vec<usize>,
    pub(crate) param_slot_span: usize,
    pub(crate) despec_method_key: String,
    pub(crate) owned_strings: Vec<Box<str>>,
    pub(crate) owned_invoke_infos: Vec<Box<JitInvokeInfo>>,
    pub(crate) owned_mic_slots: Vec<Box<JitMICSlot>>,
    pub(crate) owned_pic_slots: Vec<Box<JitPICSlot>>,
    /// The [`JitCycleEdgeCell`]s whose addresses `direct_calls` rows marked
    /// [`JIT_CYCLE_EDGE_CELL_GUARD`] carry. Same ownership rule as the slot
    /// arenas above.
    pub(crate) owned_cycle_cells: Vec<Arc<JitCycleEdgeCell>>,
    /// The [`VarHandleJudgedSite`]s whose addresses `direct_calls` rows marked
    /// [`JIT_VARHANDLE_JUDGED_GUARD`] carry (round 15 wave 6, lane vh8). Same
    /// ownership rule as the arenas above.
    pub(crate) owned_vh_judged_sites: Vec<Box<VarHandleJudgedSite>>,
    pub(crate) direct_callee_entries: Vec<(usize, u64)>,
    pub(crate) inlined_methods: Vec<(String, String, String)>,
    pub(crate) inline_tally: InlineDecisionTally,
}

impl SinglePassTables {
    /// JIT round 19 wave 5 (lane sync5, SYNCDOOR4-2): make this plan one whose
    /// synchronized self-call sites DISPATCH. A direct self-call CALLs this
    /// compile's own entry, which takes the receiver's monitor only in a
    /// SELF-LOCKING body; a run that is not one (the wrapped retry, or a plan
    /// whose self-lock was withdrawn before the backend ran) must not CALL its
    /// unlocking entry for another activation, which may name another
    /// receiver. Each marker row the planner staged a fallback for is dropped
    /// and the site's dispatch info registered instead, exactly the row the
    /// planner would have produced without the switch. The self-call info is
    /// cleared with them: a synchronized instance method has no other raw
    /// self-call site. No-op for every plan without such a site.
    pub(crate) fn unbind_sync_self_calls(&mut self) {
        if self.sync_self_call_fallback.is_empty() {
            return;
        }
        let pcs: FxHashSet<usize> = self
            .sync_self_call_fallback
            .iter()
            .map(|&(pc, _)| pc)
            .collect();
        self.direct_calls
            .retain(|(pc, dc)| !(dc.guard_class_id == JIT_SELF_CALL_GUARD && pcs.contains(pc)));
        self.invoke_info.append(&mut self.sync_self_call_fallback);
        if !self
            .direct_calls
            .iter()
            .any(|(_, dc)| dc.guard_class_id == JIT_SELF_CALL_GUARD)
        {
            self.backend.self_call_invoke_info = 0;
        }
    }

    /// Hand this plan to the single-pass backend.
    ///
    /// The argument order below is the one that was written inline in
    /// `single_pass_tier` before the split, field for field, because
    /// [`x64::compile_with_param_slots`] takes twenty-odd positional
    /// same-typed `Vec`s and a transposition would compile and miscompile.
    ///
    /// `self` is consumed: the production path must not pay a clone of every
    /// table on every single-pass compile. A caller that wants a second run
    /// clones first, having read the `Clone` note on this struct.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_backend(
        self,
        admission: &compile_gate::CompileAdmission,
        code: &[u8],
        code_len: usize,
        max_locals: usize,
        helpers: &JitRuntimeHelpers,
        despec: Option<&std::sync::Arc<crate::deopt::DespecRegistry>>,
        parts: &SinglePassParts,
    ) -> Option<CompiledMethod> {
        let SinglePassTables {
            param_slots,
            mna_info,
            field_info,
            typecheck_info,
            static_field_info,
            new_info,
            new_deferred_info,
            anewarray_info,
            anewarray_deferred_info,
            invoke_info,
            direct_calls,
            mic_slots,
            pic_slots,
            ldc_info,
            ldc_string_info,
            ldc_class_info,
            ldc2w_info,
            ldc_fp_pcs,
            branch_hints,
            loop_unroll_hints,
            inline_sites,
            inline_guard_variants,
            string_layout,
            param_oop_mask,
            compact_field_info,
            indy_info,
            sb_chain_fusions,
            elidable_init_pcs,
            backend,
            // Swapped in by `unbind_sync_self_calls` before a run that does
            // not self-lock; a self-locking run keeps the markers.
            sync_self_call_fallback: _,
        } = self;
        let mut backend = backend;
        backend.sb_chain_fusions = sb_chain_fusions;
        x64::compile_with_param_slots(
            admission,
            code,
            code_len,
            param_slots,
            max_locals,
            parts.needs_heap,
            mna_info,
            field_info,
            typecheck_info,
            static_field_info,
            new_info,
            new_deferred_info,
            anewarray_info,
            anewarray_deferred_info,
            invoke_info,
            direct_calls,
            mic_slots,
            pic_slots,
            ldc_info,
            ldc_string_info,
            ldc_class_info,
            ldc2w_info,
            ldc_fp_pcs,
            branch_hints,
            loop_unroll_hints,
            helpers,
            std::collections::HashSet::new(), // non_escaping_new — escape analysis done inside x64 too
            inline_sites,
            inline_guard_variants,
            string_layout,
            &parts.param_jvm_slots,
            parts.param_slot_span,
            param_oop_mask,
            compact_field_info,
            &parts.despec_method_key,
            despec,
            indy_info,
            elidable_init_pcs,
            backend,
        )
    }
}

/// `CRATONVM_JIT_SELF_LOCKING_SYNC` (round 13 wave 4, lane sync2; default ON,
/// `0` restores the wrapped entry for every synchronized method): compile an
/// admitted `ACC_SYNCHRONIZED` instance method as a SELF-LOCKING single-pass
/// body ([`self_lock_admits_method`]). Read per compile.
pub fn self_locking_sync_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS` (JIT round 17 wave 2, lane
/// syncdoor; proposal SY16-4 of `jit-r16-sync-proposals-RETIRED-20261006.md`; **default ON**
/// since the round-18 close, `0` refuses every table as before): admit a
/// synchronized method WITH an exception table to the single-pass
/// self-locking body ([`self_lock_admits_method`]).
///
/// Such a method had only a wrapped entry, so every compiled caller's call of
/// it paid the helper door (`try_jit_special_bytecode_callee` /
/// `try_jit_virtual_bytecode_callee` -> `install_and_run_cached_frame_monitored`
/// -> `execute_wrapped_body_door_locked`, ~400-700 ns a call on
/// `R16SyncDoorCalls` `tc` / `th` / `tv`). With the switch:
///
/// * the compile is asked for precise exception frames
///   ([`self_lock_handlers_need_precise_frames`]), which is what arms the
///   compiled local handlers, so a throw this method catches is caught IN the
///   frame that holds the monitor (no release, no re-acquire);
/// * the finished body is published only when every exceptional exit at a
///   protected bci goes through such a handler first
///   (`x64/deopt_stubs.rs` `Compiler::self_lock_handler_exits_admitted`); a
///   miss there releases in the stub's epilogue (its frame names no method
///   monitor), which is right for an exception no handler of this method
///   takes;
/// * the optimizing self-lock ([`ir_self_lock_candidate`]) keeps refusing
///   every exception table.
///
/// What it does not close: a handler the in-frame lookup DECLINES (a JVMTI
/// exception-event client armed, a catch type only the method's own user
/// loader can resolve) is run by the interpreter-side sink after the release,
/// which re-acquires: hold counts and `Thread.holdsLock` stay exact, but
/// another thread can take the monitor between the two. Page
/// `r17-syncdoor-declined-handler-in-self-locking-body-reacquires-FIXED-20261005.md`.
/// Read per compile.
///
/// Round 17 wave 2, lane syncdoor2 (SD17-1): closed under
/// [`self_lock_exc_handover_enabled`] (default ON whenever this switch is):
/// the precise exceptional exits keep the hold and hand it to the sink.
///
/// JIT round 18 wave 9 (lane syncdoor6): `pub` so the VM's readers of the same
/// switch (`exception_dispatch.rs` `synchronized_callee_handlers_served`,
/// `deopt_resume.rs` `CompiledLocksOfAStash::dropped_frame_holds_released`)
/// ask this one function: a default flip must move them all at once, and a
/// JIT that hands holds over while a VM drop site does not release them leaks
/// the monitor. `jit_bridge.rs` `self_locking_body_bind_candidate` asks it too.
/// Default ON since the round-18 close (A/B in `r18-ROUND-SUMMARY-RECORD-20261006.md`).
pub fn self_locking_sync_handlers_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER` (JIT round 17 wave 2, lane
/// syncdoor2; SD17-1 of `jit-r17-syncdoor-proposals-RETIRED-20261006.md`; **default ON**, `0`
/// restores the releasing stubs; effective only under
/// [`self_locking_sync_handlers_enabled`]): may a self-locking body WITH an
/// exception table hand its method monitor over at its precise exceptional
/// exits (stub reasons 9..=12, `DeoptReason::PendingException`) instead of
/// releasing it in the stub's epilogue?
///
/// Those exits are reached after the in-frame handler lookup answered "not
/// here", which is a definitive miss OR a decline (a JVMTI exception-event
/// client, a catch type only a user loader resolves, one that will not load).
/// On a decline a handler of this method does take the exception, and the
/// interpreter-side sink used to re-acquire the monitor the stub had just
/// released: another thread could enter it in between
/// (`r17-syncdoor-declined-handler-in-self-locking-body-reacquires-FIXED-20261005.md`).
/// With the hand-over the point names the hold (`x64/deopt_stubs.rs`
/// `self_lock_hands_over_at`), the stub keeps it, and the VM sinks adopt it
/// as the handler frame's `monitor_on_exit` (`CompiledLocksOfAStash` naming
/// the method monitor) or release it once on a miss. HotSpot never opens the
/// window.
///
/// Off while `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME` is set: that
/// switch makes `run_jit_callee_handler` leave the frame unclaimed, so no
/// sink would ever release a handed hold. Read per compile.
pub(crate) fn self_lock_exc_handover_enabled() -> bool {
    self_lock_exc_handover_switches_on()
        && None::<std::ffi::OsString>
            .is_none()
}

/// JIT round 19 (lane selflock19, SYNCDOOR618-4 "one reader"): the two
/// switches of the exceptional hand-over, read in ONE place: the handlers
/// switch ([`self_locking_sync_handlers_enabled`]) and
/// `CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER`. [`self_lock_exc_handover_enabled`]
/// adds the `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME` opt-out on top;
/// the VM's drop gate (`deopt_resume.rs`
/// `CompiledLocksOfAStash::dropped_frame_holds_released`) asks this one, as
/// it asked the same two flags itself before (no behaviour change), so a
/// flip or a renamed flag moves the JIT and every drop site together.
pub fn self_lock_exc_handover_switches_on() -> bool {
    self_locking_sync_handlers_enabled()
        && true
}

/// `CRATONVM_JIT_SELF_LOCKING_SYNC_BLOCKS` (JIT round 18 wave 9, lane
/// syncdoor6; SD17-3 of `jit-r17-syncdoor-proposals-RETIRED-20261006.md`; **default ON**
/// since the round-18 close, `0` refuses monitor bytecode as before): admit a synchronized INSTANCE method whose body holds
/// structured `synchronized` blocks (`monitorenter` / `monitorexit`) to the
/// self-locking body ([`self_lock_admits_method`]), instead of the wrapped
/// entry behind the ~700 ns helper door (`R16SyncDoorCalls bl`,
/// `r17-syncdoor-nested-monitor-synchronized-methods-keep-the-door-FIXED-20261006.md`).
///
/// Effective only with [`self_locking_sync_handlers_enabled`] and
/// [`self_lock_exc_handover_enabled`] (javac gives every block a catch-any
/// row, so such a method always has a table). The finished body is published
/// only when the per-pc monitor analysis describes every block exactly
/// (`x64/deopt_stubs.rs` `TakenMonitorShape::Described`); then every exit
/// that hands the method monitor over names it FIRST, ahead of the open
/// blocks, and marks its point `ResumeSemantics::method_monitor_first`, which
/// is how the VM tells it from a block on `this`
/// (`deopt::handed_method_monitor_index`). Read per compile.
/// `CRATONVM_JIT_SELF_LOCK_BUFFER_RETRY` (JIT round 19 wave 5, lane sync5,
/// SYNC5-1 of `docs/feature-designs/jit-ir-optimizations.md`; **default ON** since the round-19 proposal round, `0` disables):
/// when a SELF-LOCKING single-pass run bails only because it overflowed its
/// code-buffer estimate (`CODE_BUFFER_TOO_SMALL_SITE`, retry budget left), run
/// the self-locking plan once more at the measured size before falling back
/// to the wrapped body, so a measurement is never memoized as a self-lock
/// refusal. Read once per self-locking compile.
pub(crate) fn self_lock_buffer_retry_enabled() -> bool {
    true
}

pub(crate) fn self_locking_sync_blocks_enabled() -> bool {
    self_lock_exc_handover_enabled()
}

/// JIT round 17 wave 2 (lane syncdoor, SY16-4): must this compile be asked for
/// precise exception frames because it is a single-pass SELF-LOCKING body of
/// a method with an exception table ([`self_locking_sync_handlers_enabled`])?
///
/// RBC.6 asks for them only when a handler reads a non-parameter local; a
/// handler that reads only `this` and the parameters (`catch (E e) { n++;
/// return -1; }`) gets neither precise frames nor compiled local handlers,
/// and its catch would then run in the interpreter after the self-lock's exit
/// released the monitor. Precise frames arm the local handlers
/// (`x64/driver.rs` `local_handlers_armed`), so the catch stays in the frame.
/// `false` when some protected site cannot publish a precise frame
/// ([`precise_handler_frame_blocking_site`]): that compile keeps no local
/// handlers, so its body check refuses the self-lock and the wrapped retry
/// takes over, as for any refused body.
/// JIT round 17 wave 2 (lane syncdoor): does a protected range of `table`
/// cover a `*return` (`0xac..=0xb1`)? A self-locking body releases its
/// monitor AT the return, through the walk's own `monitorexit`, whose failure
/// edge is filed like any fallible site's: inside a protected range it would
/// be offered to this method's own handlers, so an
/// `IllegalMonitorStateException` from the METHOD monitor (JVMS 2.11.10
/// raises it in the invoker) could be caught by the method itself, and its
/// miss edge is not the tagged one that skips the second release. javac never
/// puts a return inside a `try` range; refused for any other producer. `true`
/// for code the strict decoder cannot walk (fail closed).
pub(crate) fn self_lock_table_covers_a_return(
    code: &[u8],
    code_len: usize,
    table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> bool {
    let Some(insns) = bytecode_analysis::decode_method(code, code_len) else {
        return true;
    };
    insns.iter().any(|insn| {
        (0xac..=0xb1).contains(&insn.op)
            && table
                .iter()
                .any(|e| insn.pc >= e.start_pc as usize && insn.pc < e.end_pc as usize)
    })
}

#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(crate) fn self_lock_handlers_need_precise_frames(
    req: &CompileRequest<'_>,
    cached: &CachedBytecodeMethod,
    code: &[u8],
    code_len: usize,
    door: compile_gate::CompileDoor,
) -> bool {
    !cached.exception_table.is_empty()
        && self_locking_sync_handlers_enabled()
        && single_pass_self_lock_preferred(
            req.verdicts(),
            cached,
            code,
            code_len,
            door,
            req.self_lock_mirror_slot,
        )
        && precise_handler_frame_blocking_site(code, code_len, &cached.exception_table).is_none()
}

/// Round 13 wave 4 (lane sync2, proposal S13-1 in `jit-r13-sync-proposals-RETIRED-20260929.md`):
/// may this method-entry compile make `cached` a SELF-LOCKING body -- the
/// prologue enters the receiver's monitor and every exit releases it -- so
/// that a compiled caller's inline cache, the dispatch cache and every other
/// raw door CALL it directly, instead of the ~400 ns helper door that supplies
/// a wrapped entry's monitor (`r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow-CLOSED-20261006.md`)?
///
/// Method-level half; the backend checks the finished body
/// (`x64/driver.rs` `self_lock_exits_are_closed`) and the door falls back to
/// the wrapped entry when that refuses. Admitted: an instance method (the
/// monitor is `this`, local 0; a `static synchronized` one keeps the
/// caller-held route), not a constructor, with
///
/// * no exception table: every exception that reaches an exit leaves the
///   method, which is exactly when JVMS 2.11.10 releases the monitor. JIT
///   round 17 wave 2 (lane syncdoor): a table too under
///   [`self_locking_sync_handlers_enabled`], whose body check keeps that
///   sentence true at every exit it admits;
/// * no `monitorenter`/`monitorexit` (structured locking cannot be violated,
///   so the exits' release never meets another hold on top) and no
///   `jsr`/`ret`;
/// * no store to local 0 of any kind, so its frame home is the receiver at
///   every pc (the backend keeps it out of registers);
/// * no `invokedynamic` (its trap re-runs the method): it would fail the body
///   check anyway, and refusing it here keeps
///   [`single_pass_self_lock_preferred`] from sending such a method to a body
///   that then cannot self-lock. Round 15 wave 5 (lane sync4, page
///   `r13w4-sync2-self-locking-bodies-with-deopt-exits-FIXED-20260930.md`, the SY2-4
///   population): admitted under [`self_locking_sync_indy_enabled`]. A
///   BRIDGED site -- the `StringConcatFactory`, `LambdaMetafactory`,
///   `SwitchBootstraps` and `ObjectMethods` bootstraps, i.e. every javac
///   string `+` and lambda -- lowers to a helper call whose failure edge is the
///   shared sentinel exit, which releases, so it is no deopt exit at all
///   (`x64/op_invoke.rs` `indy_site_is_bridged`). Only a site the bridge
///   cannot serve lowers to the reason-8 trap, and the body check refuses
///   that one (`x64/driver.rs` `self_lock_exits_are_closed`); the wrapped
///   retry and the refusal memo then take over, as for any refused body;
/// * no loop, unless [`self_locking_sync_loops_enabled`] (round 13 wave 6,
///   lane sync3): the back-edge polls of a self-locking body no longer leave
///   for the interpreter (`x64/safepoint.rs` `holds_self_lock_no_frame_names`),
///   so a loop no longer fails the body check. A loop whose body indexes an
///   array stays refused: its speculative bounds-check guards are deopt exits,
///   which do fail it, and an admitted method is kept off the optimizing tier.
///   Round 13 wave 10 (lane sync6, proposal CH4-2): unless those guards hand
///   the monitor over instead (`CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER`), see
///   [`self_locking_sync_array_loops_enabled`]; a static method too since
///   round 13 wave 13 (proposal S7-2), under
///   `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER`.
///
/// Round 13 wave 10 (lane sync6,
/// `r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md`): a
/// `static synchronized` method too, under
/// [`self_locking_sync_static_enabled`], when the VM handed the compile its
/// class's mirror slot (`mirror_slot`, `CompileRequest::self_lock_mirror_slot`:
/// the monitor is the class mirror, which moves, so the body reads it from
/// that GC-maintained word). The local-0 rule does not apply to it (its
/// monitor is not a local), and `<clinit>` is never admitted.
pub(crate) fn self_lock_admits_method(
    cached: &CachedBytecodeMethod,
    code: &[u8],
    code_len: usize,
    mirror_slot: usize,
) -> bool {
    self_lock_admission_refusal(cached, code, code_len, mirror_slot).is_none()
}

/// [`self_lock_admits_method`] with its reason: `None` admits, `Some(row)`
/// names the first rule that refused, as a [`self_lock_census::SelfLockRow`]
/// (JIT round 19, lane selflock19, SYNCDOOR618-2: the refusal census by
/// cause). The rules and their conjunction are exactly the ones the boolean
/// asked before; only the reporting is new. A method that is not
/// `ACC_SYNCHRONIZED` answers `RefusedSwitchOff`, which the census never
/// counts (its callers count only synchronized methods).
pub(crate) fn self_lock_admission_refusal(
    cached: &CachedBytecodeMethod,
    code: &[u8],
    code_len: usize,
    mirror_slot: usize,
) -> Option<self_lock_census::SelfLockRow> {
    use self_lock_census::SelfLockRow as R;
    if !cached.is_synchronized {
        return Some(R::RefusedSwitchOff);
    }
    if cached.is_static {
        if mirror_slot == 0 {
            return Some(R::RefusedStaticNoSlot);
        }
        if !self_locking_sync_static_enabled() {
            return Some(R::RefusedStaticSwitchOff);
        }
    }
    if &*cached.method_name == "<init>" || &*cached.method_name == "<clinit>" {
        return Some(R::RefusedInit);
    }
    // JIT round 17 wave 2 (lane syncdoor, SY16-4): a table too, under
    // `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS` (default ON since the round-18
    // close); the body check then demands an in-frame handler in front of
    // every protected exit (`self_lock_handler_exits_admitted`).
    if !cached.exception_table.is_empty() {
        if !self_locking_sync_handlers_enabled() {
            return Some(R::RefusedTableHandlersOff);
        }
        if self_lock_table_covers_a_return(code, code_len, &cached.exception_table) {
            return Some(R::RefusedTableCoversReturn);
        }
    }
    if !self_locking_sync_enabled() {
        return Some(R::RefusedSwitchOff);
    }
    self_lock_bytecode_refusal(
        code,
        code_len,
        SelfLockBytecodeRules {
            loops: true,
            // A loop that indexes an array is admitted only where its
            // speculative bounds guards (reason 2) can hand the monitor
            // over (`x64/deopt_stubs.rs` `self_lock_hands_over_at`); else
            // the body check would refuse it, at the cost of a compile.
            // Round 13 wave 13 (lane syncres, proposal S7-2): a static body
            // hands over too since wave 11, under its own switch
            // (`CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER`), so it is
            // asked here with the same predicate the stub asks.
            array_loops: (!cached.is_static
                || crate::deopt::self_lock_static_deopt_handover_enabled())
                && crate::deopt::self_lock_deopt_handover_enabled()
                && self_locking_sync_array_loops_enabled(),
            local0_is_monitor: !cached.is_static,
            indy: true,
            // JIT round 18 wave 9 (lane syncdoor6, SD17-3): structured
            // blocks in an instance method with a table, under
            // `CRATONVM_JIT_SELF_LOCKING_SYNC_BLOCKS`; the body check
            // (`self_lock_handler_exits_admitted`) decides the rest.
            // JIT round 19 (lane selflock19, SYNCDOOR618-1): a `static
            // synchronized` method too, under its own opt-in sub-switch
            // ([`self_locking_sync_static_blocks_enabled`]).
            blocks: (!cached.is_static || self_locking_sync_static_blocks_enabled())
                && !cached.exception_table.is_empty()
                && self_locking_sync_blocks_enabled(),
        },
    )
}

/// `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC_BLOCKS` (JIT round 19, lane
/// selflock19; proposal SYNCDOOR618-1 of `jit-r18-syncdoor6-proposals.md`;
/// **default OFF**, `1` enables): admit a `static synchronized` method whose
/// body holds structured `synchronized` blocks to the self-locking body, as
/// [`self_locking_sync_blocks_enabled`] admits the instance shape.
///
/// What makes the static shape the instance one, checked for this switch
/// (`jit-r19-selflock19-proposals.md`, SYNCDOOR618-1):
///
/// * the hold a static body hands over is its SCRATCH mirror word
///   (`x64/driver.rs` `self_lock_scratch_off`, past every local, hoist and
///   scalar slot), and a block monitor is named from its LOCAL's home
///   (`x64/deopt_stubs.rs` `taken_monitor_infos`), so a block can never be
///   the hold's own frame word (`point_hands_self_lock_over`'s
///   `!is_hold(b)` rule), even for `synchronized (Self.class)`, whose block
///   names the same OBJECT from a different word;
/// * the reason-9 shared hand-over tail and the per-site 10..=12 tails
///   refresh the scratch word from the mirror slot after the register spill
///   and before `x64_deopt_entry` reads the frame
///   (`emit_self_lock_refresh_mirror_word`); it writes that word only, so a
///   block monitor named beside it (a local home, or a register read back
///   from the spill region) is untouched;
/// * the VM half is shape-blind: `deopt::handed_method_monitor_index`
///   answers `Some(0)` for a marked `SinkMethodMonitor::Static` frame with
///   any number of blocks after it.
///
/// Effective only with [`self_locking_sync_blocks_enabled`] (and so the
/// handlers switch and the exceptional hand-over) and the static self-lock
/// switches. Read per compile. Default ON since JIT round 19 (kill word `0`:
/// R19Selflock19StaticBlocks sb 380 -> 158 ns).
pub(crate) fn self_locking_sync_static_blocks_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SELF_LOCK_HANDLERS_MODE_EXITS` (JIT round 19 wave 3, lane
/// syncdoor3, proposal SYNCDOOR3-1 of `jit-r19-syncdoor3-proposals.md`;
/// **default OFF**, `1` enables): may the body check of a SELF-LOCKING body
/// WITH an exception table (`x64/deopt_stubs.rs`
/// `Compiler::self_lock_handler_exits_admitted`) admit its MODE exits -- the
/// post-call, back-edge and helper `OsrExit` stubs (reason 7) and the
/// post-call exits that bake their own point
/// (`op_invoke::POST_CALL_OWN_POINT_STUB_TAG`) -- when their point hands the
/// method monitor over (`point_hands_self_lock_over`), exactly as the
/// table-free check (`Compiler::self_lock_deopt_exits_hand_over`) has
/// admitted them since interpreter round i2 wave 56?
///
/// Why it exists: wave 56 turned `CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER`
/// ON, after which every call in a self-locking body is followed by a
/// reason-7 mode exit that names the hold. The table-free check learned the
/// new reason; the handlers check (`self_lock_handler_stub_admitted`) did
/// not, so EVERY synchronized method with an exception table that makes a
/// call -- the whole population round 17/18's `_HANDLERS` / `_BLOCKS` flips
/// were measured on -- has been refused its self-locking body since, and its
/// compiled callers pay the ~700 ns helper door again (R17SyncDoor `tc`/`th`/
/// `tv`/`te`/`sc`/`rc`, R16SyncDoorCalls `door *`, R18Syncdoor6Blocks).
/// `CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER=0` admits them too, by planting
/// no mode exit; this switch keeps the mode exits and admits them.
///
/// Sound for the same reason the table-free admission is: a mode exit is
/// taken only on a VM verdict (an interpreter-only request, a redefinition),
/// never by an exception, so the in-frame handler rule has nothing to say
/// about it; its point names the hold (first, marked, ahead of any open
/// block), the stub leaves it held, and the VM resumes the frame owning it as
/// `monitor_on_exit` (`deopt::handed_method_monitor_index`, which reads no
/// reason). A point that does not name the hold is still refused. Read per
/// compile. Default ON since JIT round 19 (proposal round, 2026-10-08; kill
/// word `0`): a regression fix -- R17SyncDoor `sc` 620 -> 22 ns, `tv` 707 ->
/// 26, R18Syncdoor6Blocks `bt` 614 -> 45; batteries and a loaded 4x6 loop clean.
pub(crate) fn self_lock_handlers_mode_exits_enabled() -> bool {
    true
}



/// Does `code` hold an `invokedynamic`? `true` for code the strict decoder
/// cannot walk (fail closed: [`ir_self_lock_candidate`] then declines, and
/// [`self_lock_admits_method`] refuses such code anyway).
pub(crate) fn self_lock_code_has_invokedynamic(code: &[u8], code_len: usize) -> bool {
    bytecode_analysis::decode_method(code, code_len)
        .is_none_or(|insns| insns.iter().any(|insn| insn.op == 0xba))
}



/// Does any resolved `invokedynamic` row (`SinglePassTables::indy_info`:
/// `(pc, arg slots, return tag, arg tags, bridge site)`) lower to the reason-8
/// uncommon trap rather than to the bridge call? The negation of the three
/// terms of `x64/op_invoke.rs` `indy_site_is_bridged` (a resolved bootstrap,
/// the bridge helper wired, a non-`void` return), on which the 0xba arm picks
/// the bridge call; the two must not drift. `x64/driver.rs` computes
/// `CompiledMethod::has_indy_trap` with it too (round 15 wave 6 removed that
/// file's inline copy).
pub(crate) fn indy_rows_have_trap(
    indy_info: &[(usize, usize, u8, Vec<u8>, usize)],
    bridge_entry: usize,
) -> bool {
    indy_info
        .iter()
        .any(|&(_pc, _arg_slots, ret_type, _, bridge_site)| {
            // j27 wave 7 lane T1DISP20 (SA6-3): the 0xba arm's own predicate,
            // not a second spelling of its three terms.
            !crate::x64::indy_site_is_bridged(bridge_site, bridge_entry, ret_type)
        })
}


/// `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC` (round 13 wave 10, lane sync6;
/// **default ON** since round 14 wave 2, `0` refuses): compile an admitted `static
/// synchronized` method as a SELF-LOCKING body that locks its class mirror
/// ([`self_lock_admits_method`]). Inert unless the VM also hands the compile
/// the class's mirror slot (`CompileRequest::self_lock_mirror_slot`). Read per
/// compile.
pub(crate) fn self_locking_sync_static_enabled() -> bool {
    // Default ON since JIT round 14 wave 2 (with the hand-over); `=0` refuses.
    true
}

/// `CRATONVM_JIT_SELF_LOCKING_SYNC_ARRAY_LOOPS` (round 13 wave 10, lane
/// sync6, proposal CH4-2 of `jit-r13-chain4-proposals-RETIRED-20260929.md`; default ON, `0`
/// refuses them as before): admit a synchronized method whose loop indexes an
/// array to the self-locking body (a `static` one only under
/// `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER`, round 13 wave 13 proposal
/// S7-2), but only while
/// `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER` is on, under which the loop's
/// speculative bounds-check guards (reason 2) hand the method monitor to the
/// resumed frame instead of failing the body check. A performance trade, not
/// a soundness one: such a method is then kept off the optimizing tier
/// ([`single_pass_self_lock_preferred`]) unless its body is refused anyway
/// (the refusal memo then gives the tier back). Read per compile.
pub(crate) fn self_locking_sync_array_loops_enabled() -> bool {
    true
}

/// What [`self_lock_bytecode_admitted`] may admit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SelfLockBytecodeRules {
    /// A loop at all ([`self_locking_sync_loops_enabled`]).
    pub(crate) loops: bool,
    /// Also a loop whose range indexes an array
    /// ([`self_locking_sync_array_loops_enabled`]).
    pub(crate) array_loops: bool,
    /// The monitor is local 0 (an instance method): no store to it.
    pub(crate) local0_is_monitor: bool,
    /// An `invokedynamic` ([`self_locking_sync_indy_enabled`]).
    pub(crate) indy: bool,
    /// `monitorenter` / `monitorexit` ([`self_locking_sync_blocks_enabled`],
    /// JIT round 18 wave 9, lane syncdoor6): balanced, matching counts; the
    /// backend's monitor analysis proves the structure.
    pub(crate) blocks: bool,
}



/// The bytecode half of [`self_lock_admits_method`]. `false` for anything the
/// strict decoder cannot walk. `rules.loops`: whether a loop may be admitted
/// at all ([`self_locking_sync_loops_enabled`]); even then one whose textual
/// range (`back_edges`' `target..=source`) holds an array element load or
/// store is refused unless `rules.array_loops` (see
/// [`self_lock_admits_method`]). `rules.local0_is_monitor`: refuse every store
/// to local 0 (an instance method, whose monitor it holds).
pub(crate) fn self_lock_bytecode_admitted(code: &[u8], code_len: usize, rules: SelfLockBytecodeRules) -> bool {
    self_lock_bytecode_refusal(code, code_len, rules).is_none()
}

/// [`self_lock_bytecode_admitted`] with the first rule that refused (JIT
/// round 19, lane selflock19, SYNCDOOR618-2); `None` admits. The rules are
/// unchanged.
pub(crate) fn self_lock_bytecode_refusal(
    code: &[u8],
    code_len: usize,
    rules: SelfLockBytecodeRules,
) -> Option<self_lock_census::SelfLockRow> {
    use self_lock_census::SelfLockRow as R;
    let SelfLockBytecodeRules {
        loops,
        array_loops,
        local0_is_monitor,
        indy,
        blocks,
    } = rules;
    let Some(insns) = bytecode_analysis::decode_method(code, code_len) else {
        return Some(R::RefusedUndecodable);
    };
    let local_of = |pc: usize, wide: bool| -> Option<usize> {
        if wide {
            let hi = *code.get(pc + 2)?;
            let lo = *code.get(pc + 3)?;
            Some(usize::from(u16::from_be_bytes([hi, lo])))
        } else {
            code.get(pc + 1).map(|&b| usize::from(b))
        }
    };
    // JIT round 18 wave 9 (lane syncdoor6, SD17-3): monitor bytecode only
    // under `rules.blocks`, and only with at least one `monitorexit` per
    // `monitorenter` (javac's normal exit plus its handler's): the backend's
    // per-pc analysis proves the structure (`TakenMonitorShape::Described`).
    let enters = insns.iter().filter(|insn| insn.op == 0xc2).count();
    let exits = insns.iter().filter(|insn| insn.op == 0xc3).count();
    let monitors_refused = (enters | exits) != 0 && (!blocks || enters == 0 || exits < enters);
    if monitors_refused {
        return Some(R::RefusedMonitorBytecode);
    }
    let refused = insns.iter().find_map(|insn| match insn.op {
        // jsr / ret / jsr_w (monitor bytecode: above).
        0xa8 | 0xa9 | 0xc9 => Some(R::RefusedJsrRet),
        // invokedynamic: only under `rules.indy` (round 15 wave 5).
        0xba if !indy => Some(R::RefusedIndy),
        // istore_0 / lstore_0 / fstore_0 / dstore_0 / astore_0.
        0x3b | 0x3f | 0x43 | 0x47 | 0x4b if local0_is_monitor => Some(R::RefusedStoreLocal0),
        // istore .. astore and iinc with an explicit index (wide or not).
        // An unreadable index refuses too.
        0x36..=0x3a | 0x84 => match local_of(insn.pc, insn.wide) {
            None => Some(R::RefusedUndecodable),
            Some(0) if local0_is_monitor => Some(R::RefusedStoreLocal0),
            Some(_) => None,
        },
        _ => None,
    });
    if refused.is_some() {
        return refused;
    }
    let back_edges = bytecode_analysis::back_edges(code, code_len);
    if back_edges.is_empty() {
        return None;
    }
    if !loops {
        return Some(R::RefusedLoop);
    }
    // iaload..saload, iastore..sastore.
    let array_in_loop = insns.iter().any(|insn| {
        matches!(insn.op, 0x2e..=0x35 | 0x4f..=0x56)
            && back_edges
                .iter()
                .any(|&(target, source)| (target..=source).contains(&insn.pc))
    });
    (array_in_loop && !array_loops).then_some(R::RefusedArrayLoop)
}


/// Round 13 wave 4 (lane sync2): keep an admitted synchronized instance method
/// on the single-pass tier, whose body locks itself, instead of an optimizing
/// body, which is a wrapped entry that only the ~400 ns helper door can call
/// (the orchestrator's data on `SyncM`: once `step()` had an IR body with
/// deopt points, every caller went back to that door). Same switch.
///
/// Round 13 wave 6 (lane sync3): never for the OSR door. Its artifact runs
/// inside the interpreter frame that already holds the monitor and is never
/// self-locking (`x64/driver.rs` refuses the self-lock for that door), so
/// keeping it off the optimizing tier bought nothing. Unreachable while the
/// admission refused every loop; with `CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS`
/// an admitted method can have an OSR-able loop.
///
/// Round 13 wave 6 (lane sync4,
/// `r13w6-sync3-refused-self-lock-loses-the-optimizing-tier-FIXED-20260928.md`):
/// never for a method whose self-locking body the backend already refused in
/// this VM ([`self_lock_refusal_memoized`]). Such a method only ever got a
/// WRAPPED single-pass body here, entered through the same door an optimizing
/// body is, so holding it off the optimizing tier bought nothing and cost it
/// the IR body it had in round 12 (and a doubled single-pass compile on every
/// optimizing request).
///
/// Round 13 wave 10 (lane sync6): `mirror_slot` is the request's
/// `CompileRequest::self_lock_mirror_slot` (a `static synchronized` method is
/// admitted only with one). A static method compiled before any mutator minted
/// its class's slot is therefore NOT held here: that compile gets an ordinary
/// (wrapped) body, and a self-locking one only comes from a later compile that
/// carries the slot -- the usual order being a first-tier body entered through
/// the interpreter door, whose monitor acquire mints the slot, then the
/// upgrade compile.
///
/// Round 15 wave 4 (lane sync3, SY2-2): "a later compile" was not guaranteed.
/// The door a COMPILED caller's call takes (`jit_bridge`
/// `execute_wrapped_body_door_locked`) never minted, and an upgrade compile
/// that ran before the mint left the method wrapped at its last tier. That
/// door mints now, and a door that finds the published body wrapped once the
/// slot exists asks for one more compile
/// ([`static_self_lock_recompile_would_self_lock`] decides whether it could
/// self-lock; `CRATONVM_JIT_STATIC_SELF_LOCK_RECOMPILE`).
///
/// Round 15 wave 1 (lane sync, SS8-2): not for an instance method the
/// optimizing tier may build as a SELF-LOCKING body of its own
/// ([`ir_self_lock_candidate`]); a static one stays here (round 15 wave 3,
/// lane sync2, S15-3: unless the candidate admits it too).
pub(crate) fn single_pass_self_lock_preferred(
    verdicts: &JitVerdictRegistry,
    cached: &CachedBytecodeMethod,
    code: &[u8],
    code_len: usize,
    door: compile_gate::CompileDoor,
    mirror_slot: usize,
) -> bool {
    !matches!(door, compile_gate::CompileDoor::Osr)
        && self_lock_admits_method(cached, code, code_len, mirror_slot)
        && !self_lock_refusal_memoized(verdicts, cached)
        && !ir_self_lock_candidate(verdicts, cached, code, code_len, door, mirror_slot)
}

/// `CRATONVM_JIT_IR_SELF_LOCKING_SYNC` (round 15 wave 1, lane sync, proposal
/// SS8-2 of `jit-r14-sync4-proposals-RETIRED-20260930.md`; **default OFF**, `1` enables): let
/// the OPTIMIZING tier compile an admitted `synchronized` instance method as a
/// SELF-LOCKING body -- the builder's `MonitorEnter(this)` on the entry edge
/// and a `MonitorExit` before every return (`ir::MethodSelfLock`), a release
/// on every exceptional exit and a hand-over on every deopt exit
/// (`ir_lower::Lowerer::self_lock_refusal` refuses any other exit) --
/// published with `CompiledMethod::self_locks_monitor`, so raw callers CALL it
/// without the ~400 ns door. Off, such a method keeps the single-pass
/// self-locking body ([`single_pass_self_lock_preferred`]), byte for byte.
/// Read per compile.
pub(crate) fn ir_self_locking_sync_enabled() -> bool {
    false
}

/// Round 16 wave 16 (lane chains2): the `CRATONVM_DBG_JITC` line an
/// `ACC_SYNCHRONIZED` IR compile prints where the planner reads
/// `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS` (`deopt::sync_method_chains_enabled`).
/// Every reader of that switch is behind `cached.is_synchronized` (the
/// planner's three conditions) or a non-`None` `SinkMethodMonitor` (the
/// replay check), so a run that prints none of these lines compiles the same
/// bodies in both arms. `chains` is the planner's verdict
/// (`ir_chain_fences`): chain fences planned for this method.
pub(crate) fn sync_method_chains_dbg_line(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    switch_on: bool,
    self_lock: bool,
    chains: bool,
) -> String {
    format!(
        "[cratonvm-jitc] ir-sync-method-chains {class_name}.{method_name}{descriptor}: switch={} self-lock={} chains={}",
        if switch_on { "on" } else { "off" },
        self_lock,
        chains,
    )
}

/// Round 15 wave 1 (lane sync, SS8-2): should this compile's optimizing
/// attempt build `cached` as a self-locking body ([`ir_self_locking_sync_enabled`])?
/// An INSTANCE method the single-pass self-lock admits
/// ([`self_lock_admits_method`]: no exception table, no monitor bytecode, no
/// store to local 0, no `invokedynamic`), on a door other than OSR (an OSR
/// body runs inside the interpreter frame that already holds the monitor),
/// whose optimizing self-locking body this VM has not already refused
/// ([`note_ir_self_lock_refused`]).
///
/// Round 15 wave 3 (lane sync2, proposal S15-3 of `jit-r15-sync-proposals-RETIRED-20261003.md`):
/// a `static synchronized` method too, under
/// [`ir_self_locking_sync_static_enabled`], when the request carries its
/// class's mirror slot (`mirror_slot`, `CompileRequest::self_lock_mirror_slot`:
/// the GC-maintained word the single-pass static body reads its monitor
/// from). The builder then locks that word's mirror
/// (`ir::IrBuilder::set_method_self_lock_mirror`).
pub(crate) fn ir_self_lock_candidate(
    verdicts: &JitVerdictRegistry,
    cached: &CachedBytecodeMethod,
    code: &[u8],
    code_len: usize,
    door: compile_gate::CompileDoor,
    mirror_slot: usize,
) -> bool {
    ir_self_locking_sync_enabled()
        && (!cached.is_static || (mirror_slot != 0 && ir_self_locking_sync_static_enabled()))
        && !matches!(door, compile_gate::CompileDoor::Osr)
        && self_lock_admits_method(
            cached,
            code,
            code_len,
            if cached.is_static { mirror_slot } else { 0 },
        )
        // Round 15 wave 5 (lane sync4): the admission above may now take an
        // `invokedynamic` (`CRATONVM_JIT_SELF_LOCKING_SYNC_INDY`), which the
        // single-pass tier bridges; this tier has no bridge and plants a trap
        // at every indy, so such a method stays a single-pass candidate.
        && !self_lock_code_has_invokedynamic(code, code_len)
        // JIT round 17 wave 2 (lane syncdoor): the admission above may now
        // take an exception table (`CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS`),
        // which only the single-pass body check knows how to close (its
        // compiled local handlers); this tier keeps refusing every table.
        && cached.exception_table.is_empty()
        && !ir_self_lock_refusal_memoized(verdicts, cached)
}

/// Round 15 wave 4 (lane sync3, SY2-2 of `jit-r15w3-sync2-proposals-RETIRED-20261003.md`; page
/// `r15w4-sync3-static-sync-body-compiled-before-its-mirror-slot-FIXED-20260930.md`):
/// would a recompile of the `static synchronized` method `cached`, handed its
/// class's mirror slot `mirror_slot`, be offered a SELF-LOCKING body -- the
/// single-pass one ([`single_pass_self_lock_preferred`]), or the optimizing
/// one ([`ir_self_lock_candidate`]) with the single-pass one as its
/// fall-through? The VM asks it before it requests a recompile of a method
/// whose published body is WRAPPED because it was compiled before any mutator
/// minted the slot (the background worker only finds a minted slot), so a
/// recompile that could only produce another wrapped body is never asked for:
/// `false` with no slot, for an instance method, for a method
/// [`self_lock_admits_method`] refuses (an exception table, monitor bytecode,
/// `invokedynamic` with `CRATONVM_JIT_SELF_LOCKING_SYNC_INDY=0`, a switch
/// off) and for one whose self-locking body this
/// VM's backend already refused ([`self_lock_refusal_memoized`]).
pub fn static_self_lock_recompile_would_self_lock(
    verdicts: &JitVerdictRegistry,
    cached: &CachedBytecodeMethod,
    mirror_slot: usize,
) -> bool {
    let code = &cached.code;
    let code_len = code.len().saturating_sub(2);
    cached.is_static
        && mirror_slot != 0
        && code_len != 0
        && self_lock_admits_method(cached, code, code_len, mirror_slot)
        && !self_lock_refusal_memoized(verdicts, cached)
}

/// `CRATONVM_JIT_IR_SELF_LOCKING_SYNC_STATIC` (round 15 wave 3, lane sync2,
/// proposal S15-3; **default ON**, `0` keeps every `static synchronized`
/// method on the single-pass self-locking body): the `static synchronized`
/// arm of [`ir_self_lock_candidate`]. It rides
/// [`ir_self_locking_sync_enabled`] (default OFF), so it changes nothing
/// unless that switch is on. Read per compile.
pub(crate) fn ir_self_locking_sync_static_enabled() -> bool {
    true
}


/// The pseudo entry pc of the optimizing self-lock's refusal memo, beside
/// [`SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC`] in the same reject list and for the
/// same reasons (no real OSR entry pc reaches either).
pub(crate) const IR_SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC: usize = usize::MAX - 1;

/// Has this VM's optimizing tier already failed to publish `cached` as a
/// self-locking body ([`note_ir_self_lock_refused`])? `false` with
/// `CRATONVM_JIT_SELF_LOCK_REFUSAL_MEMO=0`, which retries every compile.
pub(crate) fn ir_self_lock_refusal_memoized(
    verdicts: &JitVerdictRegistry,
    cached: &CachedBytecodeMethod,
) -> bool {
    verdicts.is_osr_entry_rejected(
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            IR_SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC,
        )
}

/// Round 15 wave 1 (lane sync, SS8-2): the optimizing attempt of an
/// [`ir_self_lock_candidate`] published nothing (a builder refusal, the
/// closed-exits check, any other decline), and the method fell to the
/// single-pass tier. Later compiles keep it there -- the single-pass
/// self-locking body, today's default -- instead of paying the optimizing
/// build again, until the class is redefined or the code cache is flushed
/// (the stamp of [`note_self_lock_refused`]).
pub(crate) fn note_ir_self_lock_refused(verdicts: &JitVerdictRegistry, cached: &CachedBytecodeMethod) {
    verdicts.record_osr_entry_reject(
        cached.declaring_class_id,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        IR_SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC,
        true,
    );
}



/// The pseudo entry pc under which the self-lock refusal memo is kept in the
/// VM's verdict registry: its OSR-entry reject list
/// ([`JitVerdictRegistry::is_osr_entry_rejected`]) is exactly the shape needed
/// (per method of a loaded class, stamped, forgotten when the class is
/// redefined or unloaded, not recorded by a compile that went stale) and no
/// real OSR entry pc can reach it (a method has at most 65535 bytecode
/// bytes). The one visible cost: the `osr-entry-rejects=` count of the
/// `jit-method-stats` line includes these entries.
pub(crate) const SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC: usize = usize::MAX;

/// Has this VM's backend already refused `cached`'s self-locking body
/// ([`note_self_lock_refused`]) for the bytecode loaded now, and since its
/// last code-cache flush? `false` with the memo switched off.
pub(crate) fn self_lock_refusal_memoized(
    verdicts: &JitVerdictRegistry,
    cached: &CachedBytecodeMethod,
) -> bool {
    verdicts.is_osr_entry_rejected(
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC,
        )
}

/// Record that the backend refused `cached`'s self-locking body and the
/// wrapped retry compiled it ([`single_pass_tier`]). Stamped with the
/// install epoch too: the refusal is a property of the finished body (a
/// guarded inline, a speculative bounds guard), which rests on profiles and
/// loaded classes as well as on the bytecode, so a code-cache flush offers
/// the self-locking body again.
pub(crate) fn note_self_lock_refused(verdicts: &JitVerdictRegistry, cached: &CachedBytecodeMethod) {
    verdicts.record_osr_entry_reject(
        cached.declaring_class_id,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC,
        true,
    );
}


/// The single-pass tier of [`try_compile_inner`]: resolve the per-site tables
/// the baseline backend needs, run it, and publish the artifact.
///
/// Reached when the optimizing tier was not admitted or declined. The inputs
/// are what the admission and scan stages produced; `scan` is consumed here.
///
/// # Why this is three items and not one function
///
/// This was ~2,900 lines that resolved twenty-odd per-site tables into LOCALS
/// and then handed every one of them by value to
/// [`x64::compile_with_param_slots`]. That shape is the reason
/// [`backend_parity`] shipped with an analyser and no driver: nothing between
/// `try_compile_inner`'s two tier calls could reach those locals, so a method
/// the optimizing tier had already taken could not be given a second,
/// single-pass body to compare it against. `x64/single_pass_only.rs`'s header
/// states the same obstacle from the other side — the mechanism that would
/// close its gap needs "the two backends driven independently over the same
/// method, which the compile path does not currently allow (the single-pass
/// call site sits ~2,000 lines below the IR one, behind local state built in
/// between)".
///
/// So the resolution moved into [`build_single_pass_tables`], which returns
/// [`SinglePassTables`] (what the backend consumes) and [`SinglePassParts`]
/// (what the artifact is decorated with once the backend returns), and this
/// function is now build, run, publish.
///
/// **The split is meant to be invisible.** The moved code is the code that was
/// here: same order, same resolver calls, same `jitc_bail!` sites, the same
/// argument order into the backend. Nothing on the default path clones and
/// nothing on the default path runs twice — the tables move straight into
/// [`SinglePassTables::run_backend`] — so a default compile emits the bytes it
/// emitted before the split. `single_pass_table_build_is_callable_twice` pins
/// the other half of that claim: the tables are the whole input to the backend.
#[allow(clippy::too_many_arguments)]
pub(crate) fn single_pass_tier(
    req: &CompileRequest<'_>,
    backend_attempted: &mut bool,
    self_call_identity_stable: bool,
    admission: &compile_gate::CompileAdmission,
    local_handlers_disarmed: bool,
    metrics: &metrics::CompileRecorder,
    code: &[u8],
    code_len: usize,
    scan: x64::JitScanResult,
    prologue_param_slots: usize,
    precise_exception_frames: bool,
    resolved_string_layout: Option<StringFieldLayout>,
) -> Option<CompiledMethod> {
    let cached = req.cached;
    let (mut tables, parts) = build_single_pass_tables(
        req,
        backend_attempted,
        self_call_identity_stable,
        local_handlers_disarmed,
        metrics,
        code,
        code_len,
        scan,
        prologue_param_slots,
        precise_exception_frames,
        resolved_string_layout,
    )?;
    // Round 15 wave 6 (lane sweepA, SY4-2 of `jit-r15w5-sync4-proposals-RETIRED-20261003.md`):
    // an `invokedynamic` the bridge cannot serve lowers to the reason-8 trap,
    // which the self-locking body check always refuses
    // (`self_lock_deopt_exits_hand_over` answers `false` for reason 8), so the
    // self-locking run below would be thrown away. Decide it from the resolved
    // rows instead and compile the wrapped body once; the refusal memo is
    // stamped exactly as the backend's refusal would stamp it.
    let self_lock_indy_refused = tables.backend.self_lock_receiver
        && indy_rows_have_trap(&tables.indy_info, tables.backend.direct_helpers.indy_bridge);
    if self_lock_indy_refused {
        tables.backend.self_lock_receiver = false;
        tables.backend.self_lock_mirror_slot = 0;
    }
    // JIT round 19 (lane selflock19, SYNCDOOR618-2): the per-VM self-lock
    // census, by cause, for every method-entry compile of a synchronized
    // method (`self_lock_census.rs`). Reporting only: nothing below reads it.
    let sl_census = (cached.is_synchronized
        && !matches!(admission.door(), compile_gate::CompileDoor::Osr))
    .then(|| req.verdicts().self_lock_census());
    if let Some(census) = sl_census {
        use self_lock_census::SelfLockRow as R;
        census.note(R::EntryCompiles);
        if tables.backend.self_lock_receiver || self_lock_indy_refused {
            census.note(R::Admitted);
        } else {
            census.note(
                self_lock_admission_refusal(cached, code, code_len, req.self_lock_mirror_slot)
                    .unwrap_or(R::RefusedMemo),
            );
        }
        if self_lock_indy_refused {
            census.note(R::BodyRefusedIndyTrap);
        }
    }

    note_jit_pipeline_stage(JIT_STAGE_SINGLE_PASS);
    // The backend: selection, encoding and buffer install in one call, exactly
    // as `lower_inner` is for the optimizing tier. The phase guard also covers
    // the `?` — a backend bail is a compilation that spent this time.
    let metrics_single_pass = metrics.phase(metrics::Phase::SinglePass);
    let mut compiled = if tables.backend.self_lock_receiver {
        // Round 13 wave 4 (lane sync2): a self-locking body the backend
        // refuses (an exit that is not a propagating throw, a tail call, an
        // unwired helper) is compiled again as the wrapped entry. The clone
        // copies the tables' arena POINTERS (see the `Clone` note on
        // `SinglePassTables`), which is sound here because at most one of the
        // two runs produces an artifact: the first either returns the body
        // that is installed or returns nothing, and only then does the clone
        // run.
        let mut wrapped = tables.clone();
        wrapped.backend.self_lock_receiver = false;
        wrapped.backend.self_lock_mirror_slot = 0;
        // SYNCDOOR4-2: the wrapped entry takes no monitor, so it must not be
        // CALLed for a synchronized self-call (lane sync5).
        wrapped.unbind_sync_self_calls();
        // JIT round 19 wave 5 (lane sync5, SYNC5-1): a second self-locking
        // run for a body that only overflowed its code-buffer estimate
        // (`self_lock_buffer_retry_enabled`). Cloned only under the switch.
        let mut buffer_retry = self_lock_buffer_retry_enabled().then(|| tables.clone());
        let first = tables.run_backend(
            admission,
            code,
            code_len,
            cached.max_locals as usize,
            req.helpers,
            req.despec,
            &parts,
        );
        // SYNC5-1: the self-locking body's estimate is the bytecode's, and a
        // body with nested blocks (one local-handler site per protected exit,
        // per level) outgrows it -- `nestSame` 16 116 bytes against the
        // wrapped body's 11 375 (`r19_sync5_tests`). The backend has just
        // recorded the size it wanted (`code_buffer_hint`), which is exactly
        // what the next attempt would use; without this, the wrapped retry
        // below fits, `note_self_lock_refused` memoizes a refusal that was a
        // MEASUREMENT, and the method keeps the door for the VM's life (the
        // `nest` rows, ~480 ns in every arm). Run the self-locking plan once
        // more at the measured size; any other bail, or a second shortfall,
        // falls through to the wrapped retry exactly as before.
        let first = match (first, buffer_retry.take()) {
            (None, Some(again))
                if JIT_BAIL_SITE.with(|c| c.get()).map(|(site, _, _)| site)
                    == Some(CODE_BUFFER_TOO_SMALL_SITE)
                    && !code_buffer_retries_exhausted(&format!(
                        "{}.{}:{}",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    )) =>
            {
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] self-lock-buffer-retry {}.{}{}: the self-locking body overflowed its estimate; again at the measured size",
                        cached.class_name, cached.method_name, cached.method_descriptor
                    );
                }
                again.run_backend(
                    admission,
                    code,
                    code_len,
                    cached.max_locals as usize,
                    req.helpers,
                    req.despec,
                    &parts,
                )
            }
            (first, _) => first,
        };
        match first {
            Some(body) => {
                if let Some(census) = sl_census {
                    census.note(if body.self_locks_monitor {
                        self_lock_census::SelfLockRow::BodyPublished
                    } else {
                        self_lock_census::SelfLockRow::BodyRefusedOther
                    });
                    // JIT round 22 wave 8, lane sync: link 1 of the hand-over
                    // chain -- this body's precise exceptional exits hand the
                    // method monitor over (`x64/deopt_stubs.rs`
                    // `self_lock_exc_hands_over`: a table and the switch).
                    if body.self_locks_monitor
                        && !cached.exception_table.is_empty()
                        && self_lock_exc_handover_enabled()
                    {
                        census.note(self_lock_census::SelfLockRow::ExcHandoverBodies);
                    }
                }
                body
            }
            None => {
                // JIT round 19 (lane selflock19, SYNCDOOR618-2): the body
                // check's cause is the bail site `x64/driver.rs` recorded
                // (read, not taken: the compile's own report still takes it).
                if let Some(census) = sl_census {
                    let site = JIT_BAIL_SITE.with(|c| c.get()).map(|(site, _, _)| site);
                    census.note(self_lock_census::SelfLockCensus::body_refusal_row(site));
                }
                let body = wrapped.run_backend(
                    admission,
                    code,
                    code_len,
                    cached.max_locals as usize,
                    req.helpers,
                    req.despec,
                    &parts,
                )?;
                // Round 13 wave 6 (lane sync4): only a refusal the wrapped
                // body survived is the self-lock's; a body that fails both
                // ways is the ordinary bail path's business.
                note_self_lock_refused(req.verdicts(), cached);
                body
            }
        }
    } else {
        // SYNCDOOR4-2 (lane sync5): not a self-locking run (the indy
        // refusal above, or a plan that never was one), so no synchronized
        // self-call may CALL this body's own entry.
        tables.unbind_sync_self_calls();
        let body = tables.run_backend(
            admission,
            code,
            code_len,
            cached.max_locals as usize,
            req.helpers,
            req.despec,
            &parts,
        )?;
        // SY4-2: the wrapped body survived, so the self-lock's refusal is
        // recorded (as the arm above records it after its wrapped retry).
        if self_lock_indy_refused {
            note_self_lock_refused(req.verdicts(), cached);
        }
        body
    };
    drop(metrics_single_pass);

    // Everything below decorates the artifact with what the resolution phase
    // built and the backend only borrowed. Moving these out of `parts` is what
    // transfers OWNERSHIP of the arenas whose addresses the emitted code has
    // baked into it, and it is why at most one artifact per `SinglePassParts`
    // may be installed — see the `Clone` note on `SinglePassTables`.
    let SinglePassParts {
        needs_heap,
        param_jvm_slots,
        param_slot_span,
        // Borrowed by the backend call above as a `&str`; dead from here.
        despec_method_key: _,
        owned_strings,
        owned_invoke_infos,
        owned_mic_slots,
        owned_pic_slots,
        owned_cycle_cells,
        owned_vh_judged_sites,
        mut direct_callee_entries,
        inlined_methods,
        inline_tally,
    } = parts;

    compiled._jit_strings = owned_strings;
    compiled._jit_invoke_infos = owned_invoke_infos;
    // Round 15 wave 6 (lane vh8): extended, like the slot arenas below, so a
    // record a backend path attached itself is never dropped under its code.
    compiled._jit_vh_judged_sites.extend(owned_vh_judged_sites);
    // BUG-24: `compile(...)` already moved the loop-unroll *cloned* MIC/PIC
    // slots into `compiled._jit_{mic,pic}_slots` (see the `extend` in
    // `x64.rs::compile`, whose contract states the caller attaches its owned
    // slots ADDITIVELY). Assigning here would DROP those cloned boxes while the
    // unrolled machine code still holds baked pointers into them → use-after-
    // free: the freed `Box<JitPICSlot>` memory gets reused by the allocator for
    // Java objects, so the inline PIC cascade later reads class-id-pair garbage
    // out of `entry_words[i]` and `CALL`s it (the Mockito-under-JIT
    // `EXCEPTION_ACCESS_VIOLATION at 0x0000033E0000033B`, a packed-class-id
    // value). Extend, don't overwrite, so both the cloned and owned slots stay
    // alive for the lifetime of the compiled code.
    compiled._jit_mic_slots.extend(owned_mic_slots);
    compiled._jit_pic_slots.extend(owned_pic_slots);
    // Round 9 wave 7 (`mutrec7`): the code bakes these cells' addresses, so
    // the artifact owns them (and `JitCache::put` registers them).
    compiled._jit_cycle_cells.extend(owned_cycle_cells);
    direct_callee_entries.sort_unstable();
    direct_callee_entries.dedup();
    if let Ok(want) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        let key = format!(
            "{}.{}{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
        if key.contains(&want) {
            eprintln!(
                "[jit-kept-callees] method={key} entries={:x?}",
                direct_callee_entries
            );
        }
    }
    compiled._direct_callee_entries = direct_callee_entries.iter().map(|(e, _)| *e).collect();
    compiled._direct_callee_expected = direct_callee_entries;
    compiled.inlined_methods = inlined_methods;
    // C2-review P1 — publish the inlining decision totals on the artifact so
    // `metrics::CompileRecorder::installed` can harvest them alongside
    // `code_bytes` / `deopt_points` / `frame_bytes`. Empty unless something was
    // actually considered, so it costs one moved `Vec` header on every other
    // compilation.
    compiled.inline_tally = inline_tally;

    if let Ok(want) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        let full = format!(
            "{}.{}{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
        if full.contains(&want) {
            let slice = compiled._buffer_slice_for_debug();
            let mut hex = String::new();
            for b in slice {
                hex.push_str(&format!("{:02x}", b));
            }
            eprintln!(
                "[JIT_CODE] {} entry={:p} len={} param_jvm_slots={:?} span={} needs_heap={}\n{}",
                full,
                compiled.entry,
                slice.len(),
                param_jvm_slots,
                param_slot_span,
                needs_heap,
                hex
            );
        }
    }

    // Single-pass success: same harvest as the optimizing path. `installed`
    // takes the artifact's own `used_ir_backend` as the authority on which
    // backend produced it, so the recorded path cannot drift from the truth.
    metrics.installed(&compiled);
    Some(compiled)
}

/// Resolve every per-site table one single-pass compile needs.
///
/// Lifted verbatim out of [`single_pass_tier`], which is its only production
/// caller. It answers `None` for the same reasons the inline code returned
/// `None` before the split: a constant-pool resolver that cannot answer
/// (`jitc_bail!` — transient, the method is retried) or a property of the class
/// file that retrying cannot change (`jitc_permanent_bail!`, which sets
/// `backend_attempted` and so routes the method onto the permanent bail-list).
///
/// # This is not a pure function, and a second caller has to know which parts
///
/// Most of it is resolution — read the constant pool, fill a `Vec` — and that
/// part is repeatable. Three things it does are side effects on state outside
/// it, and a second call in the same process does them again:
///
///   * `metrics.enter_single_pass()` and `note_single_pass_entered()`, the
///     per-compile diagnostics. The second matters most: it sets the thread's
///     "the optimizing pipeline ran and fell through to single-pass" signal
///     that [`last_compile_fell_through_to_single_pass`] reports. A caller that
///     runs this AFTER an optimizing body was already produced must pass a
///     recorder that records nothing and restore that signal itself — which is
///     exactly what [`backend_parity_shadow_compile`] does, and why.
///   * `intern_typecheck_target` / `intern_catch_type_name`: process-wide name
///     interning behind a `Mutex`-ed set. Both are keyed by the name and return
///     the existing `&'static str` on a second call, so they are idempotent and
///     leak nothing extra — which is what makes running this twice safe rather
///     than merely wasteful.
///   * the callee compiles the single-pass inliner asks `callee_compiler` for.
///     That closure belongs to the VM, not to this crate, so how much a second
///     ask costs is not this function's to promise. Assume it is not free; it
///     is the main reason `CRATONVM_JIT_BACKEND_PARITY` is a corpus-run
///     instrument rather than something to leave switched on.
///
/// Everything else the returned pair owns — the boxed MIC/PIC slots, the
/// interned invoke infos, the inline plans — is freshly allocated per call and
/// belongs to that call's [`SinglePassParts`].
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
/// `(invoke pc, cell)` of every `invokevirtual` / `invokespecial` /
/// `invokestatic` / `invokeinterface` in `invoke_ops` that the VM flags as
/// unresolved at compile time ([`CompileRequest::cp_invoke_first_call`];
/// interpreter round i1 wave 50, lane L1). Empty without a resolver.
pub(crate) fn first_call_cells_of(
    req: &CompileRequest<'_>,
    invoke_ops: &[(usize, u16, u8)],
) -> Vec<(usize, usize)> {
    let Some(flag) = req.cp_invoke_first_call else {
        return Vec::new();
    };
    let cells: Vec<(usize, usize)> = invoke_ops
        .iter()
        .filter(|&&(_, _, opcode)| matches!(opcode, 0xb6..=0xb9))
        .filter_map(|&(pc, cp_idx, _)| {
            let cell = flag(cp_idx);
            (cell != 0).then_some((pc, cell))
        })
        .collect();
    if !cells.is_empty() && false {
        eprintln!(
            "[first-call] {}.{}{}: {} unresolved site(s) at pcs {:?}",
            req.cached.class_name,
            req.cached.method_name,
            req.cached.method_descriptor,
            cells.len(),
            cells.iter().map(|&(pc, _)| pc).collect::<Vec<_>>(),
        );
    }
    cells
}

/// `CRATONVM_JIT_SP_INTRINSIC_TRAP_DESPEC` -- JIT round 23 wave 5 (lane L3,
/// `r23-w4-hot-throwing-method-ends-interpreted-ineligible-20261021.md`).
/// Default ON; `0` restores the old resolver, which registered an unguarded
/// single-pass call-site intrinsic whatever the de-spec registry said.
///
/// The single-pass `String` access intrinsics (`length`, `isEmpty`, `charAt`,
/// `hashCode`) and the inline `Integer.intValue` / `Long.longValue` unbox have
/// no exception path of their own: a null receiver or an out-of-range
/// `charAt` index branches to the reason-6 (`ReceiverTypeChanged`) trap, and
/// the interpreter re-executes the invoke and throws. The trapped-callee sink
/// charges every such trap and, after `PER_BCI_DESPEC_LIMIT` at one bci,
/// records the site in the de-spec registry -- but nothing on these
/// intrinsics' resolver arms read that record (the receiver-guard filter
/// admits `guard_class_id == 0` before asking it), so each recompile
/// re-emitted the same trap. A method that throws there on most calls burned
/// through `max_deopts_per_method` (twice it, with the de-spec spare) and was
/// retired by `MakeNotCompilable`: interpreted for the rest of the run, where
/// HotSpot's C1/C2 keep a compiled body that throws through the call.
pub fn sp_intrinsic_trap_despec_enabled() -> bool {
    true
}

/// Whether the single-pass resolver must leave the call-site intrinsic at
/// `pc` unregistered because the compiling VM's de-spec registry already
/// withdrew that site's reason-6 trap (or the whole bci, by the wildcard).
/// `false` with the switch off or no registry. See
/// [`sp_intrinsic_trap_despec_enabled`].
pub(crate) fn sp_intrinsic_trap_site_withdrawn(
    despec: Option<&std::sync::Arc<crate::deopt::DespecRegistry>>,
    cached: &CachedBytecodeMethod,
    pc: usize,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some(registry) = despec else {
        return false;
    };
    if !sp_intrinsic_trap_despec_enabled() {
        return false;
    }
    let withdrawn = sp_intrinsic_trap_withdrawn_in(
        registry,
        &format!(
            "{}.{}:{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        ),
        pc as u32,
    );
    if withdrawn && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] sp-intrinsic WITHDRAWN {class_name}.{method_name}{descriptor} @pc={pc} \
             in {}.{}{} (de-spec'd trap site; CRATONVM_JIT_SP_INTRINSIC_TRAP_DESPEC)",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
    withdrawn
}

/// The registry half of [`sp_intrinsic_trap_site_withdrawn`]: the reason-6
/// speculation the trapped-callee sink records for these intrinsics
/// (`speculation_id(bci, ReceiverTypeChanged)`), or the wildcard it records
/// when the reason was ambiguous.
pub(crate) fn sp_intrinsic_trap_withdrawn_in(
    registry: &crate::deopt::DespecRegistry,
    method_key: &str,
    bci: u32,
) -> bool {
    registry.contains_speculation(
        method_key,
        bci,
        crate::deopt::speculation_id(bci, crate::deopt::DeoptReason::ReceiverTypeChanged),
    )
}

