#[allow(unused_imports)]
use super::super::*;

impl FlushEpoch {
    /// The current value (one acquire load).
    pub fn load(&self) -> u64 {
        match self {
            Self::Own(counter) => counter.load(std::sync::atomic::Ordering::Acquire),
            Self::Shared(counter) => counter.load(std::sync::atomic::Ordering::Acquire),
        }
    }

    /// A flush (or a redefinition) of the owning VM: advance an own counter;
    /// a shared one was, or will be, advanced by its owner.
    pub fn note_flush(&self) {
        if let Self::Own(counter) = self {
            counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

/// A verdict registry's redefine epoch (the "redefine" half of its stamps):
/// a counter of its own, or its VM's redefine epoch (`JitRealm::redefine_epoch`
/// in the `vm` crate, which that VM advances once per class redefinition)
/// shared with it.
///
/// Interpreter round i1 wave 17: the redefine half of [`FlushEpoch`]'s fold.
/// Until then each of a VM's two registries kept a copy advanced beside the
/// VM's `JitCache::bump_redefine_epoch` by `JitRealm::note_class_redefinition`;
/// shared, one counter answers for both and the realm advances it. The
/// `JitCache`'s own counter stays inline in the cache: its readers are the
/// inline-cache helpers, where one load beats a load through an `Arc`.
pub enum RedefineEpoch {
    /// The registry's own counter (no VM: backend tests, the process registry).
    Own(std::sync::atomic::AtomicU32),
    /// The owning VM's redefine epoch.
    Shared(std::sync::Arc<std::sync::atomic::AtomicU32>),
}

impl Default for RedefineEpoch {
    fn default() -> Self {
        Self::Own(std::sync::atomic::AtomicU32::new(0))
    }
}

impl RedefineEpoch {
    /// The current value (one acquire load).
    pub fn load(&self) -> u32 {
        match self {
            Self::Own(counter) => counter.load(std::sync::atomic::Ordering::Acquire),
            Self::Shared(counter) => counter.load(std::sync::atomic::Ordering::Acquire),
        }
    }

    /// A redefinition of the owning VM: advance an own counter; a shared one
    /// was, or will be, advanced by its owner.
    pub fn note_redefinition(&self) {
        if let Self::Own(counter) = self {
            counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

/// A [`JitVerdictRegistry`]'s identity, for the compile witness
/// ([`JitVerdictRegistry::open_verdict_epoch_witness`]). Drawn from a process
/// counter, never reused, so a witness can never name a dropped registry's
/// successor at the same address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerdictRegistryId(pub(crate) u64);

impl Default for VerdictRegistryId {
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

thread_local! {
    /// The registry the admitted compilation open on this thread records its
    /// verdicts into, with that registry's `(redefine, flush)` epochs as of
    /// the admission: `(registry id, redefine, flush)`, or `None` when no
    /// admitted compilation is open. Outermost wins, like
    /// [`COMPILE_INSTALL_EPOCH`]. See
    /// [`JitVerdictRegistry::recording_compile_is_stale`].
    pub(crate) static COMPILE_VERDICT_EPOCHS: std::cell::Cell<Option<(u64, u32, u64)>> =
        const { std::cell::Cell::new(None) };
}

/// RAII scope of [`JitVerdictRegistry::open_verdict_epoch_witness`], held by a
/// `compile_gate::CompileAdmission` for the whole compilation. Restores the
/// outer value on drop.
#[must_use = "the witness must stay alive for the whole compilation"]
pub struct VerdictEpochWitness {
    pub(crate) prev: Option<(u64, u32, u64)>,
}

impl Drop for VerdictEpochWitness {
    fn drop(&mut self) {
        COMPILE_VERDICT_EPOCHS.with(|c| c.set(self.prev));
    }
}

/// How many class redefinitions a VM performed and what they cost its code
/// cache: the census of `i14-L3-proposal-class-scoped-redefinition-invalidation`
/// stage 1, printed at the end of the `JIT method stats:` line
/// (`tiered::verdict_count_fields`).
#[derive(Default)]
pub(crate) struct RedefinitionCensus {
    /// Redefinitions of the owning VM (`JitRealm::note_class_redefinition`).
    pub(crate) redefinitions: std::sync::atomic::AtomicU64,
    /// Bodies (method-entry and OSR) the full flush of each redefinition
    /// evicted, summed.
    pub(crate) evicted_bodies: std::sync::atomic::AtomicU64,
    /// Of those, the bodies a class-scoped eviction would also have had to
    /// evict (`JitCache::redefinition_dependents`), summed. Counted only when
    /// the stats line that prints it is armed (`CRATONVM_DBG=jit-method-stats`,
    /// read once at start-up), since it costs a scan of the cache.
    pub(crate) dependent_bodies: std::sync::atomic::AtomicU64,
}

pub(crate) fn verdict_key(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> u64 {
    compute_jit_key_hash(class_name, method_name, descriptor, class_id)
}

impl JitVerdictRegistry {
    /// A registry whose flush epoch is its VM's code-state epoch `epoch`
    /// (see [`FlushEpoch`]); the tiered manager builds its VM's registry
    /// with the same counter it stamps with.
    pub fn with_flush_epoch(epoch: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            install_epoch: FlushEpoch::Shared(epoch),
            ..Self::default()
        }
    }

    /// A registry whose flush epoch is its VM's code-state epoch `flush` and
    /// whose redefine epoch is its VM's redefine epoch `redefine` (see
    /// [`FlushEpoch`], [`RedefineEpoch`]): the VM advances both, and
    /// [`Self::note_redefinition`] / [`Self::note_code_cache_flush`] move
    /// nothing here. What the tiered manager builds a VM's registry with
    /// (interpreter round i1 wave 17).
    pub fn with_vm_epochs(
        flush: std::sync::Arc<std::sync::atomic::AtomicU64>,
        redefine: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) -> Self {
        Self {
            install_epoch: FlushEpoch::Shared(flush),
            redefine_epoch: RedefineEpoch::Shared(redefine),
            ..Self::default()
        }
    }

    /// Record that the owning VM redefined a class: every verdict recorded
    /// before this stops counting. Moves the registry's own epochs only: an
    /// own redefine and flush epoch advance here; shared ones are the VM's
    /// redefine and code-state epochs, which `JitRealm::note_class_redefinition`
    /// in the `vm` crate advances itself.
    ///
    /// Whole-registry on purpose, as the process epoch was: a verdict about one
    /// method may rest on another class's bytecode (an inlined callee), and
    /// [`JitVerdictRegistry::forget_for_class`] reaches only the redefined
    /// class's own entries.
    pub fn note_redefinition(&self) {
        self.redefine_epoch.note_redefinition();
        self.note_code_cache_flush();
    }

    /// Census (`i14-L3-proposal-class-scoped-redefinition-invalidation` stage
    /// 1): the owning VM redefined a class, and the full flush that went with
    /// it evicted `evicted_bodies` bodies, `dependent_bodies` of which a
    /// class-scoped eviction would also have evicted (`None`: not counted).
    /// Reporting only; moves no epoch.
    pub fn note_redefinition_census(&self, evicted_bodies: usize, dependent_bodies: Option<usize>) {
        use std::sync::atomic::Ordering::Relaxed;
        let census = &self.redefinition_census;
        census.redefinitions.fetch_add(1, Relaxed);
        census
            .evicted_bodies
            .fetch_add(evicted_bodies as u64, Relaxed);
        if let Some(dependent) = dependent_bodies {
            census.dependent_bodies.fetch_add(dependent as u64, Relaxed);
        }
    }

    /// `(redefinitions, evicted bodies, dependent bodies)` recorded by
    /// [`Self::note_redefinition_census`].
    pub fn redefinition_census(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let census = &self.redefinition_census;
        (
            census.redefinitions.load(Relaxed),
            census.evicted_bodies.load(Relaxed),
            census.dependent_bodies.load(Relaxed),
        )
    }

    /// JIT round 19 (lane selflock19): this VM's self-lock census
    /// ([`self_lock_census::SelfLockCensus`]). Reporting only.
    pub fn self_lock_census(&self) -> &self_lock_census::SelfLockCensus {
        &self.self_lock_census
    }

    /// Record that the owning VM flushed its code cache
    /// (`JitCache::clear_all`): the verdicts that depend on compile-time state
    /// (`osr_refusal_depends_on_compile_state`) stop counting. A no-op on a
    /// shared flush epoch, which its owner advances ([`FlushEpoch::note_flush`]).
    pub fn note_code_cache_flush(&self) {
        self.install_epoch.note_flush();
    }

    /// This registry's `(redefine, install)` epochs.
    pub(crate) fn epochs(&self) -> (u32, u64) {
        (self.redefine_epoch.load(), self.install_epoch.load())
    }

    /// A stamp for a verdict recorded now.
    pub(crate) fn stamp_now(&self, depends_on_compile_state: bool) -> VerdictStamp {
        let (redefine, install) = self.epochs();
        VerdictStamp {
            redefine_epoch: redefine,
            install_epoch: depends_on_compile_state.then_some(install),
        }
    }

    /// Whether a verdict stamped `stamp` still counts.
    pub(crate) fn stamp_is_current(&self, stamp: VerdictStamp) -> bool {
        let (redefine, install) = self.epochs();
        stamp.is_current_at(redefine, install)
    }

    /// Open this registry's compile witness: record, for the compilation about
    /// to run on this thread, which registry it records into and this
    /// registry's epochs now. `compile_gate::admit` opens one beside the
    /// [`CompileEpochWitness`] and the admission holds it. Outermost wins: a
    /// nested admission keeps the outer compile's witness.
    pub fn open_verdict_epoch_witness(&self) -> VerdictEpochWitness {
        COMPILE_VERDICT_EPOCHS.with(|c| {
            let prev = c.get();
            if prev.is_none() {
                let (redefine, flush) = self.epochs();
                c.set(Some((self.id.0, redefine, flush)));
            }
            VerdictEpochWitness { prev }
        })
    }

    /// Whether the compilation open on this thread began before the last
    /// redefinition or code-cache flush of THIS registry's VM.
    ///
    /// A verdict is stamped when it is RECORDED, so a compile of the old
    /// bytecode that bailed after a redefinition landed would bail-list the
    /// new bytecode until the next one (`forget_for_class` had already run).
    /// Such a verdict is not recorded: that costs one more compile attempt,
    /// never a wrong answer.
    ///
    /// The witness is this registry's own epochs when the open compilation was
    /// admitted against this registry ([`Self::open_verdict_epoch_witness`]):
    /// they are exactly the epochs the stamps read, so this VM's flush still
    /// suppresses a verdict about pre-flush compile state and another VM's
    /// does not. Until interpreter round i1 wave 17 this compared the process
    /// witness ([`CompileEpochWitness`], moved by every VM's redefinitions and
    /// flushes), so a VM that flushed often kept every other VM's permanently
    /// failing compiles off the bail list. A compile admitted against another
    /// registry (or none: a backend opened with `for_backend_test`) falls back
    /// to the process witness; no witness at all (a door that records outside
    /// a compilation) records as before.
    pub(crate) fn recording_compile_is_stale(&self) -> bool {
        if let Some((id, redefine, flush)) = COMPILE_VERDICT_EPOCHS.with(|c| c.get()) {
            if id == self.id.0 {
                return (redefine, flush) != self.epochs();
            }
        }
        COMPILE_INSTALL_EPOCH
            .with(|c| c.get())
            .is_some_and(|(install, _)| install != jit_install_epoch())
    }

    /// Read a method's verdicts, if any are recorded under its exact class
    /// identity and names.
    pub(crate) fn read_verdicts<R>(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        read: impl FnOnce(&MethodVerdicts) -> R,
    ) -> Option<R> {
        let verdicts = self.verdicts.read();
        let entry = verdicts.get(&verdict_key(class_id, class_name, method_name, descriptor))?;
        entry
            .is_for(class_id, class_name, method_name, descriptor)
            .then(|| read(entry))
    }

    /// Update a method's verdicts, creating its entry on first use.
    pub(crate) fn write_verdicts(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        write: impl FnOnce(&mut MethodVerdicts),
    ) {
        let mut verdicts = self.verdicts.write();
        let entry = verdicts
            .entry(verdict_key(class_id, class_name, method_name, descriptor))
            .or_insert_with(|| MethodVerdicts::new(class_id, class_name, method_name, descriptor));
        if !entry.is_for(class_id, class_name, method_name, descriptor) {
            // A genuine 64-bit collision between two different methods: the
            // newer one takes the slot. Losing the older verdicts costs that
            // method one more compile attempt, which is the safe direction.
            *entry = MethodVerdicts::new(class_id, class_name, method_name, descriptor);
        }
        write(entry);
    }

    /// Forget every compile verdict recorded about methods of the loaded class
    /// `(class_id, class_name)` -- the bail list, refusal reasons and OSR entry
    /// rejects. Called by the tiered manager when the class is unloaded or
    /// redefined. Returns the number of methods whose verdicts were dropped.
    ///
    /// Identity decides when both the entry and the argument carry a non-zero
    /// id, so another loader's same-named class keeps its verdicts. An id-less
    /// side falls back to the name, which errs towards forgetting too much -- a
    /// forgotten verdict costs one more compile attempt, never a wrong answer.
    ///
    /// The recursion-cycle sets are not pruned here: a member only makes a
    /// call MORE conservative (a dispatched call), and forgetting one is the
    /// unsafe direction. An unload prunes them by exact identity
    /// ([`JitVerdictRegistry::forget_cycle_members_of_unloaded_class`]).
    pub fn forget_for_class(&self, class_id: cratonvm_types::ClassId, class_name: &str) -> usize {
        let mut verdicts = self.verdicts.write();
        let before = verdicts.len();
        verdicts.retain(|_, entry| !entry.belongs_to_class(class_id, class_name));
        before - verdicts.len()
    }

    /// Batch form of [`Self::forget_for_class`] for a whole unloaded set: ONE
    /// `retain`, by the same id-then-name rule (gc-common w36-c). Equivalent
    /// to calling `forget_for_class` for each member; returns the total.
    pub fn forget_for_classes(&self, classes: &UnloadedClassSet<'_>) -> usize {
        if classes.is_empty() {
            return 0;
        }
        let mut verdicts = self.verdicts.write();
        let before = verdicts.len();
        verdicts.retain(|_, entry| !classes.owns(entry.class_id, &entry.class_name));
        before - verdicts.len()
    }

    /// Whether the given method of the loaded class `class_id` has been added
    /// to the bail-list by a prior permanent-bail compilation attempt of the
    /// bytecode loaded now. Asked by `compile_gate::admit` at every door to
    /// short-circuit re-attempts.
    pub fn is_bail_listed(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        self.read_verdicts(class_id, class_name, method_name, descriptor, |entry| {
            entry
                .bail_listed
                .is_some_and(|stamp| self.stamp_is_current(stamp))
        })
        .unwrap_or(false)
    }

    /// Mark the method of the loaded class `class_id` permanently bail-listed.
    /// Called when the heavy `x64::compile` path returns None (typically
    /// because of an unsupported backend pattern that won't change on retry).
    pub fn mark_bail_listed(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) {
        if self.recording_compile_is_stale() {
            return;
        }
        let stamp = self.stamp_now(false);
        self.write_verdicts(class_id, class_name, method_name, descriptor, |entry| {
            entry.bail_listed = Some(stamp);
        });
    }

    /// Bail-list the method AND record the refusal site the compile that just
    /// ran left behind, so `CRATONVM_DBG=jit-method-stats` can name it.
    ///
    /// [`JitVerdictRegistry::mark_bail_listed`] alone leaves the reason
    /// unrecorded. That is fine for a caller that has already recorded one,
    /// and wrong for the OSR door in `vm/src/runtime/interpreter/jit_bridge.rs`,
    /// which reaches the backend directly: its bails arrived in the report as
    /// `reason=unrecorded`, which is exactly the shape that sends a reader
    /// looking for a compiler bug somewhere else. An OSR bail is also the one
    /// that matters most — that door compiles a `@Test` method's hot loop, and
    /// a method denied there runs its whole life interpreted with no other
    /// diagnostic. Found the hard way on `HttpHeaderValidationUtilTest`'s two
    /// exhaustive loops (osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md).
    ///
    /// Consumes the thread-local site, like `try_compile`'s own recorder.
    /// A measured code-buffer shortfall is recoverable until its bounded
    /// resize budget is exhausted. Keep that verdict aligned with method-entry
    /// compilation: an OSR attempt must not permanently retire a method whose
    /// next attempt will allocate the recorded larger buffer.
    pub fn mark_bail_listed_with_site(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) {
        self.mark_bail_listed_with_site_and_metrics(
            class_id,
            class_name,
            method_name,
            descriptor,
            0,
            0,
        );
    }

    /// Like [`Self::mark_bail_listed_with_site`], carrying the OSR attempt's
    /// real bytecode length and elapsed duration for `cratonvm.JitCompileDecision`.
    pub fn mark_bail_listed_with_site_and_metrics(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        bytecode_size: i32,
        duration_ns: u64,
    ) {
        let site = take_jit_bail_site().unwrap_or((take_jit_pipeline_stage(), 0, 0));
        let recoverable_capacity = site.0 == CODE_BUFFER_TOO_SMALL_SITE
            && !code_buffer_retries_exhausted(&format!("{class_name}.{method_name}:{descriptor}"));
        if !recoverable_capacity {
            self.mark_bail_listed(class_id, class_name, method_name, descriptor);
        }
        self.record_bail_reason(class_id, class_name, method_name, descriptor, site);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] OSR-bail site={} pc={} opcode={:#04x} {class_name}.{method_name}{descriptor}",
                site.0, site.1, site.2,
            );
        }
        // JFR `cratonvm.JitCompileDecision`, OSR door.
        //
        // This door reaches `x64::compile_with_param_slots` directly and never
        // passes through `try_compile_with_invokespecial_resolver`, so the
        // single event that funnel emits cannot see it at all — and by the
        // argument in this method's doc comment, an OSR bail is the one that
        // matters most. `Refused` is unambiguous here:
        // this function is only ever called on the arm where the OSR backend
        // returned `None`.
        //
        // Gate first, as everywhere: `record_jit_compile_decision` re-checks
        // it, but only a caller-side check can refund what the caller already
        // built.
        if cratonvm_jfr::jit_decision::jit_decision_enabled() {
            use cratonvm_jfr::jit_decision as jd;
            // `(0, 0)` is `note_jit_bail_site`'s "not one bytecode's fault"
            // encoding. 0 is a legal bci AND a legal opcode (`nop`), so it has
            // to become the sentinel rather than be passed through as a
            // coordinate a reader would believe.
            let (bail_bci, bail_opcode) = if site.1 == 0 && site.2 == 0 {
                (jd::NO_BAIL_SITE, jd::NO_BAIL_SITE)
            } else {
                (
                    i32::try_from(site.1).unwrap_or(jd::NO_BAIL_SITE),
                    i32::try_from(site.2).unwrap_or(jd::NO_BAIL_SITE),
                )
            };
            jd::record_jit_compile_decision(&jd::JitCompileDecision {
                class_name,
                method_name,
                method_descriptor: descriptor,
                door: jd::CompileDoor::Osr,
                outcome: jd::CompileOutcome::Refused,
                // Every `note_jit_bail_site*` name is a literal, so the reason
                // on this path allocates nothing.
                reason: jd::DecisionText::Static(site.0),
                bail_bci,
                bail_opcode,
                bytecode_size,
                start_time_ns: jit_decision_now_nanos().saturating_sub(duration_ns),
                duration_ns,
            });
        }
    }

    /// Record why a compile was refused BEFORE the jit crate was ever entered.
    ///
    /// The jit crate's own refusals already reach the stats table: every exit
    /// in `try_compile_with_invokespecial_resolver` funnels through
    /// `take_jit_bail_site().unwrap_or((take_jit_pipeline_stage(), 0, 0))`, so
    /// even a siteless bail names the stage. The VM-side pipeline has no such
    /// funnel — `try_jit_compile_callee_slow` has NINE `return None` exits,
    /// none of which recorded anything, and a method refused at one of them
    /// was retried three times by the background worker and then retired as
    /// `compile-failed reason=unrecorded`.
    ///
    /// That label is worse than no label: `compile-failed` says the compiler
    /// was asked and refused, which sends the reader into the backend, when
    /// the truth may be that the backend was never reached. Measured on
    /// `HibfixComposeProbe2`: `CompletableFuture$UniCompose.tryFire` (97 716
    /// invocations) and `UniRelay.tryFire` (58 612) both retired this way,
    /// and they ARE `CompletableFuture` composition.
    ///
    /// `class_id` is the loaded class the refusal is about, or `ClassId(0)`
    /// when the refusal is that no such class could be found.
    pub fn record_compile_refusal(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        why: &'static str,
    ) {
        self.record_bail_reason(class_id, class_name, method_name, descriptor, (why, 0, 0));
    }

    /// Diagnostic: number of methods currently bail-listed in this registry.
    pub fn bail_list_size(&self) -> usize {
        self.verdicts
            .read()
            .values()
            .filter(|entry| {
                entry
                    .bail_listed
                    .is_some_and(|stamp| self.stamp_is_current(stamp))
            })
            .count()
    }

    /// Record the refusal site of a compile that just bailed, beside the
    /// method's other verdicts.
    ///
    /// The thread-local [`JIT_BAIL_SITE`] answers "why did the compile that
    /// just ran bail?", which only helps somebody already watching
    /// `CRATONVM_DBG_JITC` at the moment it happened. The place a
    /// permanently-uncompilable hot method is actually *noticed* is the
    /// end-of-run `CRATONVM_DBG=jit-method-stats` table, which runs long after
    /// every compile worker has moved on — so the reason has to outlive the
    /// compile. It expires with the bytecode it describes, like every other
    /// verdict.
    pub(crate) fn record_bail_reason(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        site: (&'static str, u32, u32),
    ) {
        if self.recording_compile_is_stale() {
            return;
        }
        let stamp = self.stamp_now(false);
        self.write_verdicts(class_id, class_name, method_name, descriptor, |entry| {
            entry.bail_site = Some((stamp, site));
        });
    }

    /// Clear any refusal site recorded for this method of the loaded class
    /// `class_id`, because a subsequent compilation succeeded.
    pub fn clear_bail_site(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) {
        self.write_verdicts(class_id, class_name, method_name, descriptor, |entry| {
            entry.bail_site = None;
        });
    }

    /// The refusal site last recorded for this method of the loaded class
    /// `class_id`, rendered for a report line, or `None` if no compile of the
    /// currently loaded bytecode bailed.
    pub fn bail_reason_for(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<String> {
        let site = self
            .read_verdicts(class_id, class_name, method_name, descriptor, |entry| {
                entry
                    .bail_site
                    .filter(|&(stamp, _)| self.stamp_is_current(stamp))
                    .map(|(_, site)| site)
            })
            .flatten()?;
        Some(format_jit_bail_site(Some(site)))
    }

    /// Whether a previous OSR compile of the currently loaded bytecode produced
    /// a body that refused to OSR-enter at `entry_pc`. Checked before re-running
    /// the OSR pipeline; see the "OSR entry-point reject memo" notes further
    /// down this file.
    pub fn is_osr_entry_rejected(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        entry_pc: usize,
    ) -> bool {
        self.read_verdicts(class_id, class_name, method_name, descriptor, |entry| {
            entry
                .osr_rejects
                .iter()
                .any(|&(pc, stamp)| pc == entry_pc && self.stamp_is_current(stamp))
        })
        .unwrap_or(false)
    }

    /// Record that compiling this method for `entry_pc` yields a body that
    /// cannot enter there, so the pipeline is not re-run for that PC until the
    /// bytecode changes. For a refusal the compiler explained with a
    /// [`bailout::Bailout`], use
    /// [`JitVerdictRegistry::mark_osr_entry_rejected_by`], which also lets the
    /// memo expire when the refusal depended on compile-time state.
    pub fn mark_osr_entry_rejected(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        entry_pc: usize,
    ) {
        self.record_osr_entry_reject(
            class_id,
            class_name,
            method_name,
            descriptor,
            entry_pc,
            false,
        );
    }

    /// [`JitVerdictRegistry::mark_osr_entry_rejected`] for a refusal the
    /// compiler explained. A refusal in [`OSR_COMPILE_STATE_REFUSAL_TAGS`] is
    /// stamped with the JIT install epoch and stops counting when that moves.
    pub fn mark_osr_entry_rejected_by(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        entry_pc: usize,
        refusal: &bailout::Bailout,
    ) {
        self.record_osr_entry_reject(
            class_id,
            class_name,
            method_name,
            descriptor,
            entry_pc,
            osr_refusal_depends_on_compile_state(refusal),
        );
    }

    pub(crate) fn record_osr_entry_reject(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        entry_pc: usize,
        depends_on_compile_state: bool,
    ) {
        if self.recording_compile_is_stale() {
            return;
        }
        let stamp = self.stamp_now(depends_on_compile_state);
        self.write_verdicts(
            class_id,
            class_name,
            method_name,
            descriptor,
            |entry| match entry.osr_rejects.iter_mut().find(|(pc, _)| *pc == entry_pc) {
                Some(slot) => slot.1 = stamp,
                None => entry.osr_rejects.push((entry_pc, stamp)),
            },
        );
    }

    /// Diagnostic: number of (method, entry_pc) pairs whose OSR reject still
    /// counts in this registry.
    pub fn osr_entry_reject_count(&self) -> usize {
        self.verdicts
            .read()
            .values()
            .map(|entry| {
                entry
                    .osr_rejects
                    .iter()
                    .filter(|&&(_, stamp)| self.stamp_is_current(stamp))
                    .count()
            })
            .sum()
    }
}

/// The key the IR-tier site-trap registry is hashed under.
///
/// `ClassId::new(0)` on purpose: exposed so the runtime can look a method up
/// in [`JitVerdictRegistry::method_has_site_trap`] by the names a trapped frame
/// carries, without the raw hash function becoming public. `try_compile_inner`'s
/// `ir_method_hash` uses the same sentinel, and two callers agreeing on a hash
/// is exactly the kind of thing that silently stops agreeing. No memo is keyed
/// by this alone: each registry method mixes in the declaring class id (and
/// the decision and refusal keys the VM's redefine epoch); see
/// `ir_refusal_memo_key`.
pub fn ir_method_memo_hash(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    compute_jit_key_hash(
        class_name,
        method_name,
        descriptor,
        cratonvm_types::ClassId::new(0),
    )
}

/// Byte length of the bytecode instruction at `pc`, from the JIT's one
/// decoder (`bytecode_analysis::step`): `wide` forms, both switch paddings,
/// the 5-byte invokes and wide branches. An undecodable instruction steps one
/// byte, so a walk always terminates. For crates outside the JIT that walk
/// bytecode, so they need no length table of their own.
pub fn bytecode_insn_len(code: &[u8], pc: usize) -> usize {
    bytecode_analysis::step(code, pc)
}

/// The IR refusal memo's key: the method's IR memo hash re-keyed by the
/// declaring class's identity and the redefine epoch.
///
/// That memo (`ir_evidence::IrRefusalMemo`, one per VM in its
/// [`JitVerdictRegistry`]) records "a previous compile carried no evidence the
/// optimizing tier helps" and skips the IR attempt ever after. The verdict was
/// about the bytecode compiled at the time, and after a redefinition it went on
/// skipping the new body's IR attempt. Mixing in the epoch (the VM's own, read
/// by [`JitVerdictRegistry::ir_refusal_key`]) grants every method one fresh
/// attempt per redefinition, a retry bounded by the number of redefinitions.
/// Mixing in the class id keeps a same-named class in another loader, whose
/// bytecode may differ, from inheriting the verdict. The site-trap decision
/// ([`JitVerdictRegistry::claim_site_trap_decision`]) uses the same mixing;
/// the planted-trap memo omits the epoch on purpose: the runtime looks it up
/// for frames of code that is already installed.
pub(crate) fn ir_refusal_memo_key(
    ir_method_hash: u64,
    class_id: cratonvm_types::ClassId,
    redefine_epoch: u32,
) -> u64 {
    let mut h = FxHasher::default();
    h.write_u64(ir_method_hash);
    h.write_u32(class_id.as_u32());
    h.write_u32(redefine_epoch);
    h.finish()
}

/// [`JitVerdictRegistry::clear_bail_site`] on [`process_jit_verdicts`].
pub fn jit_clear_bail_site(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    process_jit_verdicts().clear_bail_site(class_id, class_name, method_name, descriptor);
}

thread_local! {
    /// Why the compile currently running on this thread refused the method.
    ///
    /// `(site, bytecode_pc, opcode)`; the pc/opcode pair is `(0, 0)` for the
    /// sites that are not a single bytecode's fault.
    ///
    /// # Why this exists
    ///
    /// `try_compile` reported a failed compile as
    /// `compile-bail <method> backend_attempted=<bool>` and nothing else.
    /// `backend_attempted` reads like "the backend ran and gave up", but it is
    /// really a *permanence* flag: three constant-pool resolver misses set it
    /// too, precisely so the method lands on the bail-list instead of being
    /// retried forever. So the one bit that looked like a classification
    /// conflated "codegen has a hole" with "this CP entry is a shape the
    /// compiler never accepts", and a permanently-interpreted hot JDK method
    /// was indistinguishable from a transient resolver miss — which is exactly
    /// how `java/text/DateFormatSymbols.getProviderInstance` (a plain
    /// `ldc <Class>`) sat unexplained. See
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    ///
    /// One-shot per compile: `try_compile` clears it on entry so a bail can
    /// only ever report its own cause, never a previous method's.
    pub(crate) static JIT_BAIL_SITE: std::cell::Cell<Option<(&'static str, u32, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Record why this compile is about to bail. Cheap enough (one `Cell` store)
/// to run unconditionally, so the reason is available whether or not
/// `CRATONVM_DBG_JITC` was set when the compile started.
pub fn note_jit_bail_site(site: &'static str) {
    JIT_BAIL_SITE.with(|c| c.set(Some((site, 0, 0))));
}

/// [`note_jit_bail_site`] for a refusal attributable to one bytecode — the
/// single-pass backend's per-opcode `return false` arms.
pub fn note_jit_bail_site_at(site: &'static str, pc: usize, opcode: u8) {
    JIT_BAIL_SITE.with(|c| c.set(Some((site, pc as u32, u32::from(opcode)))));
}

/// Take (and clear) the recorded bail site.
pub fn take_jit_bail_site() -> Option<(&'static str, u32, u32)> {
    JIT_BAIL_SITE.with(|c| c.take())
}

/// Compiled call sites reclassified from `invokevirtual` to a direct,
/// non-dispatching bind because the constant pool resolved them to a **private**
/// method (JVMS 5.4.6; javac has emitted `invokevirtual` for such calls since
/// Java 11 / JEP 181).
///
/// This is the ENGAGEMENT counter for that reclassification. Without it, "the
/// probe passes now" cannot be told apart from "no site on this workload was
/// one" — and the shape is common enough that a zero on a large workload would
/// itself be the finding.
pub static PRIVATE_INVOKEVIRTUAL_PINNED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Compiled call sites reclassified from `invokevirtual` to a direct,
/// non-dispatching bind because the resolved method can never be overridden —
/// it is `final`, or its class is.
///
/// Separate from [`PRIVATE_INVOKEVIRTUAL_PINNED`] on purpose. The two rules
/// reach the same conclusion by different arguments and cover different code,
/// so one counter for both could not answer the only question worth asking of
/// either: did THIS rule engage on THIS workload. A shared tally that moves is
/// indistinguishable from a rule that never fired.
pub static FINAL_INVOKEVIRTUAL_PINNED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Call sites the `final`-devirtualisation door REFUSED because a registered
/// native shadows the body it would have bound.
///
/// `final` promises no subclass declares another BODY; it promises nothing
/// about CratonVM's native registry, which shadows a JDK method by registering
/// on a class name that is usually a SUBCLASS of the one declaring the body.
/// See `invoke::final_devirt_native_shadow` for the two channel defects that
/// escaped through the gap and the measurements behind them.
///
/// Read it beside [`FINAL_INVOKEVIRTUAL_PINNED`]: together they say how much of
/// the door's candidate set the screen takes. A zero here on a workload that
/// moves the sibling counter means the screen is inert for that workload, not
/// that it is unnecessary.
pub static FINAL_DEVIRT_NATIVE_SHADOW_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`FINAL_DEVIRT_NATIVE_SHADOW_REFUSED`].
#[must_use]
pub fn final_devirt_native_shadow_refused_count() -> u64 {
    FINAL_DEVIRT_NATIVE_SHADOW_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Call sites where the static-bind rewrite YIELDED to a call-site intrinsic.
///
/// `java/lang/String` is `final`, so `invokevirtual_site_final_owner` answers
/// for every `String.charAt`/`length`/`isEmpty`/`hashCode` site in the tree and
/// the rewrite below turns `invoke_kind` 0 into 1. That is a correct statement
/// about dispatch and it was catastrophic here: the instance call-site
/// intrinsic gate a few hundred lines down is `invoke_kind == 0 ||
/// invoke_kind == 2`, and the inline/direct-bind ladder that kind-1 sites take
/// FIRST ends in a `continue`. So the site was bound to a real call to
/// `String.charAt` and never offered the inline decode — not declined, not
/// counted, not printed by any of the three `string-intrinsic` diagnostics,
/// because it left the loop before reaching them.
///
/// Measured on `probes/CharAtDoorProbe.java`, one class, one run, five
/// byte-identical bodies: the arm called straight from `main` read **349.64
/// ns/char** and the four reached through a functional interface (which the
/// OSR door compiles, and which never runs this rewrite) read 3.2-4.3.
/// `CRATONVM_JIT_FINAL_DEVIRT=0` on the SAME binary moved the first arm to
/// **6.85** — 51x from one flag, and the flag is not the fix, it is the proof.
///
/// A non-zero reading here is the count of sites this rule handed back.
pub static DEVIRT_YIELDED_TO_INTRINSIC: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`DEVIRT_YIELDED_TO_INTRINSIC`].
pub fn devirt_yielded_to_intrinsic_count() -> u64 {
    DEVIRT_YIELDED_TO_INTRINSIC.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD=1` — restore the pre-2026-09-02
/// ordering, in which a `final`-class devirtualisation took a call site away
/// from an intrinsic that would have inlined it.
///
/// Default OFF (the yield is ON). The B arm of an in-binary A/B: with this set,
/// `probes/CharAtDoorProbe.java`'s `direct-from-main` row returns to ~350
/// ns/char while every other arm is unchanged, which is the whole finding in
/// one line.
///
/// NOT `OnceLock`-cached, matching `string_intrinsic_pin_enabled`: read at
/// compile time only, never on a runtime hot path.
pub(crate) fn devirt_intrinsic_yield_enabled() -> bool {
    !false
}

/// Would an inline call-site intrinsic take this `invokevirtual` site?
///
/// The two matchers between them are exactly the set the instance-intrinsic
/// gate accepts, asked the same way it asks: `try_resolve_intrinsic` is the
/// layout-independent one and `try_resolve_string_intrinsic` the layout-aware
/// `java/lang/String` / `java/lang/CharSequence` one. Answering `true` keeps the
/// site at `invoke_kind == 0` so that gate can still see it.
///
/// **The blast radius is narrower than it looks.** The caller only consults
/// this where `cp_invokespecial_owner_resolver` would otherwise have answered
/// — a PRIVATE target, or a `final` one. No intrinsic matches a private method,
/// so the JVMS 5.4.6 correctness rule is untouched and what is left is exactly
/// "a final class with an instance intrinsic". `java/lang/String` is the
/// measured member of that set and the whole of the 100x; any other is the same
/// defect by construction and is not measured here.
///
/// A site that yields and is then declined at registration (the CRC32
/// guard-class-id rule, the CharSequence receiver-profile filter) falls to
/// ordinary MIC/PIC dispatch rather than a static bind. On a final class that
/// cache is monomorphic, so the cost is a cache probe, not a dispatch walk.
pub(crate) fn site_yields_to_call_site_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
    string_layout: Option<StringFieldLayout>,
) -> bool {
    if !devirt_intrinsic_yield_enabled() {
        return false;
    }
    try_resolve_intrinsic(class, name, descriptor).is_some()
        || try_resolve_string_intrinsic(class, name, descriptor, string_layout).is_some()
        || site_yields_to_thin_instance_helper(class, name, descriptor)
}

/// [`site_yields_to_call_site_intrinsic`] for the ATOMIC_INT / ATOMIC_LONG
/// ladders (review #80).
///
/// Those two cannot be asked by name alone: registering one resolves the
/// site's guard class id and, for a subclass site, the class that declares
/// the method. So this asks [`atomic_intrinsic_for_invoke_site`] exactly as
/// `try_compile_inner` registers the site. Without it the `final`-method
/// rewrite could bind `counter.incrementAndGet()` statically, and the site
/// would leave through the direct-bind arm before the intrinsic saw it.
pub(crate) fn site_yields_to_atomic_intrinsic(
    cp_idx: u16,
    class: &str,
    name: &str,
    descriptor: &str,
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    cp_invoke_declaring_class_resolver: Option<&dyn Fn(u16) -> Option<String>>,
) -> bool {
    if !devirt_intrinsic_yield_enabled() {
        return false;
    }
    [
        "java/util/concurrent/atomic/AtomicInteger",
        "java/util/concurrent/atomic/AtomicLong",
    ]
    .into_iter()
    .any(|jdk| {
        atomic_intrinsic_for_invoke_site(
            jdk,
            cp_idx,
            class,
            name,
            descriptor,
            cp_invoke_class_id_resolver,
            cp_invoke_declaring_class_resolver,
        )
        .is_some()
    })
}

/// The other two members of the set, and the reason the first version of
/// this yield did not cover them.
///
/// The two matchers above are the *intrinsic* matchers. The kind-0 invoke
/// ladder also carries a row of THIN DIRECT-HELPER binds — `Integer.intValue`,
/// `Long.longValue`, the three `VarHandle` families, `ByteBuffer.get`/`put`,
/// `MessageDigest.update`, `ConcurrentMap.get`, `HashMap.get` — which neither
/// matcher knows about, and each of those arms ends in a `continue` exactly
/// like an intrinsic's. A site the `final`-class rewrite claims never reaches
/// them either.
///
/// # The enumeration
///
/// Devirtualisation only answers for a `final` method or a method of a
/// `final` class, so the population is the ladder's rows whose declared class
/// is final. Walked, row by row:
///
/// | class | final? | reached by the rewrite |
/// |---|---|---|
/// | `java/lang/String` | **yes** | yes — the 100x, closed by the matcher above |
/// | `java/lang/Integer` | **yes** | **yes** |
/// | `java/lang/Long` | **yes** | **yes** |
/// | `java/lang/invoke/VarHandle` | no | no |
/// | `java/nio/ByteBuffer` | no (abstract) | no |
/// | `java/security/MessageDigest` | no | no |
/// | `java/util/HashMap` | no | no |
/// | `java/util/concurrent/ConcurrentMap` | interface (kind 2) | no |
/// | `java/lang/CharSequence`, `java/lang/foreign/MemorySegment` | interfaces | no |
/// | `java/util/zip/CRC32`, `CRC32C`, `AtomicInteger`, `AtomicLong` | no | no |
///
/// So the set is `{String, Integer, Long}` and not `{String}`.
///
/// # What the other two were costing
///
/// Measured 2026-09-02, `probes/FinalDevirtInstanceIntrinsics.java`, one
/// binary, x86-64 Linux, three interleaved rounds (ns per unboxing):
///
/// | site | HotSpot | rewrite claims it | rewrite yields |
/// |---|---:|---:|---:|
/// | `Integer.intValue()` | 0.46 | **76.10** | **1.24** |
/// | `Long.longValue()` | 0.63 | **423.55** | **1.07** |
///
/// 61x and **396x**. Both alternatives are a CALL — this is not `String`'s
/// inline-versus-call shape — but they are not the same call: the bind the
/// rewrite installs is a Java frame with a shadow-stack push and an
/// invocation-counter increment, and the helper the ladder installs is a
/// leaf Rust call that reads the box's field and returns.
///
/// # The one case this over-approximates
///
/// Asked WITHOUT the `jdk_only` policy check the arms themselves apply, so a
/// build where `direct_native_helper` declines the bind (a `NativeKind`
/// refusal under `--real-jdk`) yields a site that then takes ordinary
/// MIC/PIC dispatch rather than a static bind. On a `final` class that cache
/// is monomorphic by construction, so the cost is a cache probe — the same
/// trade the CharSequence receiver-profile refusal above already makes, and
/// the alternative is threading two more resolver arguments through a
/// predicate that four call sites share.
pub(crate) fn site_yields_to_thin_instance_helper(class: &str, name: &str, descriptor: &str) -> bool {
    // Every arm below is itself gated on this, so without it there is no bind
    // to yield to and the rewrite is the better answer.
    if !direct_jit_callee_calls_enabled() {
        return false;
    }
    match (class, name, descriptor) {
        ("java/lang/Integer", "intValue", "()I") => true,
        ("java/lang/Long", "longValue", "()J") => long_box_direct_helpers_enabled(),
        _ => false,
    }
}

/// `checkcast` sites that got the inline class-id compare, and the two reasons
/// the rest did not.
///
/// Per CAUSE, not a single "not inlined" total. The two refusals want opposite
/// fixes — an unresolved target id means the site's `CONSTANT_Class` was not
/// loaded when this body was compiled (transient, and a re-compile fixes it),
/// while an untrusted operand means the backend's `stack_oop_marks` could not
/// prove the operand is an oop (structural, and the same open question the
/// compact `getfield` arm's own diagnostic exists to answer). A combined count
/// that moves cannot say which one a workload is hitting.
///
/// These are COMPILE-time counts. The runtime engagement counter already
/// exists and needs nothing new: `CRATONVM_DBG=jit-method-stats` prints
/// `membership walks by JIT site: checkcast=…`, and every walk the fast path
/// avoids is one this number does not report.
pub static CHECKCAST_INLINE_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// [`CHECKCAST_INLINE_SITES`] for the OPTIMIZING tier's own `checkcast`
/// lowering (`ir_lower`'s `Op::CheckCast`).
///
/// Counted apart from the single-pass door because they are two independent
/// emitters and a hot method only ever runs the second one. Folding them into
/// one number is what would have hidden this fix's first version, which
/// patched only the single-pass arm and moved a four-million-cast probe by
/// 80 walks out of 3,203,316.
pub static CHECKCAST_INLINE_SITES_IR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// [`CHECKCAST_INLINE_SITES`] for the one-dimensional PRIMITIVE ARRAY variant,
/// which proves its answer from the header's kind/element tags instead of a
/// class id (see `cratonvm_types::primitive_array_kind_tags_byte`).
///
/// Counted apart because it serves a disjoint population and was added for a
/// workload the class-id compare provably could not help: netty's
/// `PooledByteBuf<T>` casts its erased `memory` field to `byte[]` on every
/// byte, and a primitive array's header class id is 0, so the first version of
/// this fix moved `checkcast=11,994,524` walks by 7,690 out of 12 million.
pub static CHECKCAST_INLINE_SITES_PRIM_ARRAY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Sites refused because the target class was not resolvable at compile time.
pub static CHECKCAST_INLINE_NO_TARGET_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Sites refused because the operand was not a provably-trusted oop.
pub static CHECKCAST_INLINE_UNTRUSTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The `instanceof` arm's site census — the twin of the four `CHECKCAST_*`
// counters above.
//
// WHY THESE FIVE ARE FUNCTION CALLS AND THE CHECKCAST FOUR ARE BARE STATICS.
//
// Not a style difference: `jit/tests/process_global_statics_ratchet.rs` freezes
// the number of `static` declaration lines under `jit/src` and asserts EQUALITY
// in both directions, so five new `pub static AtomicU64`s are a red build, and
// the correction would have to be made in that test file — a file no one lane
// owns. The counters are therefore FIELDS on `RECLAMATION`'s
// `TypecheckSiteCensus`, which is private, so the emitter reaches them through
// these five `pub(crate) fn`s instead of naming a static. That is the remedy
// `docs/feature-designs/jit-ir-optimizations.md`
// itself names, against the alternative of bumping the ratchet.
//
// `pub(crate)`, not `pub`, and that also matters: `scripts/check-orphan-
// instruments.sh`'s C1 check flags any `pub fn note_*` with an atomic body and
// no caller in another file. These are called only from `jit/src/x64/op_object.rs`
// — another file, so they would pass — but they have no business outside this
// crate, and the narrower visibility keeps them out of the census entirely.
//
// WHY THE SPLIT IS BY CAUSE. The same argument `CHECKCAST_INLINE_SITES`' doc
// makes, unchanged: the two refusals want opposite fixes (an unresolved target
// id is transient and a recompile fixes it; an untrusted operand is structural),
// so a single "not inlined" total that moves identifies neither. The refusal
// buckets are together the answer to "how many sites fell through to
// `jit_instanceof` with no inline arm at all", which is the denominator
// `r10-readers-instanceof-arm-has-no-site-census-FIXED-20260922.md` asks for by the
// name `INSTANCEOF_HELPER_SITES`: without it, a zero on the inline buckets
// cannot be told from an `instanceof`-free workload.
//
// BOTH DOORS ARE NOW COVERED. These five count the SINGLE-PASS door; the four
// `note_instanceof_ir_*` notifiers below count the optimizing tier's own fast
// path (`ir_lower.rs`'s `Op::InstanceOf`, round 9 wave 9), which is to this arm
// what `CHECKCAST_INLINE_SITES_IR` is to `CHECKCAST_INLINE_SITES` — two
// independent emitters where a hot method only ever runs the second, so they are
// counted apart and never summed. The IR half landed 2026-09-22 and closed
// `docs/feature-designs/jit-ir-optimizations.md`;
// read [`instanceof_ir_inline_sites`] for why it has four buckets and this one
// has five.

/// The `instanceof` arm emitted the 1-D primitive-array KIND_TAGS compare.
pub(crate) fn note_instanceof_inline_prim_array_site() {
    RECLAMATION
        .typecheck_sites
        .instanceof_prim_array
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The `instanceof` arm emitted the inline class-id compare.
pub(crate) fn note_instanceof_inline_class_id_site() {
    RECLAMATION
        .typecheck_sites
        .instanceof_class_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The `instanceof` arm extended a class-id site with the `final`-target
/// definite-miss inline `0`. A SUBSET of
/// [`note_instanceof_inline_class_id_site`]'s bucket.
pub(crate) fn note_instanceof_inline_final_miss_site() {
    RECLAMATION
        .typecheck_sites
        .instanceof_final_miss
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The `instanceof` arm refused: no resolvable target class id.
pub(crate) fn note_instanceof_refused_no_target_id() {
    RECLAMATION
        .typecheck_sites
        .instanceof_no_target_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The `instanceof` arm refused: the operand was not a provably-trusted oop.
pub(crate) fn note_instanceof_refused_untrusted() {
    RECLAMATION
        .typecheck_sites
        .instanceof_untrusted
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The OPTIMIZING tier's `instanceof` arm emitted the 1-D primitive-array
/// KIND_TAGS compare.
pub(crate) fn note_instanceof_ir_inline_prim_array_site() {
    RECLAMATION
        .typecheck_sites
        .instanceof_ir_prim_array
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The OPTIMIZING tier's `instanceof` arm emitted the inline class-id compare.
pub(crate) fn note_instanceof_ir_inline_class_id_site() {
    RECLAMATION
        .typecheck_sites
        .instanceof_ir_class_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The OPTIMIZING tier's `instanceof` arm extended a class-id site with the
/// `final`-target definite-miss inline `0`. A SUBSET of
/// [`note_instanceof_ir_inline_class_id_site`]'s bucket.
pub(crate) fn note_instanceof_ir_inline_final_miss_site() {
    RECLAMATION
        .typecheck_sites
        .instanceof_ir_final_miss
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The OPTIMIZING tier's `instanceof` arm refused: no usable target class id
/// and no primitive-array tag, so the site took the bare `jit_instanceof` call.
/// See [`TypecheckSiteCensus::instanceof_ir_no_target_id`] for which sub-cause
/// is reachable on this door.
pub(crate) fn note_instanceof_ir_refused_no_target_id() {
    RECLAMATION
        .typecheck_sites
        .instanceof_ir_no_target_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Snapshot of the OPTIMIZING tier's `instanceof` site census:
/// `(prim_array, class_id, final_miss, refused_no_target_id)`.
///
/// The sibling of [`instanceof_inline_sites`], kept SEPARATE from it rather
/// than widening that tuple, for two reasons that are the same reason:
///
/// * the bucket sets differ. This tier has no `refused-untrusted-operand`
///   analogue and must not pretend to: that bucket is fed by the single-pass
///   backend's `operand_is_trusted_oop`, a test over its `stack_oop_marks`
///   abstract-stack marks, and this tier's operand is an SSA value whose
///   reference-ness is a property of the IR. A four-tuple beside a five-tuple
///   says that in its shape; a nine-tuple with one element that can never move
///   says nothing;
/// * the two tiers are the pair `CHECKCAST_INLINE_SITES` and
///   `CHECKCAST_INLINE_SITES_IR` already are — independent emitters where a hot
///   method only ever runs the second — so the figures are compared, never
///   added.
///
/// **All four read zero when `CRATONVM_JIT_CHECKCAST_INLINE=0`**, exactly as
/// [`instanceof_inline_sites`]'s five do: `fast_enabled` in the `Op::InstanceOf`
/// arm is `name_ptr != 0 && checkcast_inline_enabled()`, and the census sits
/// under the same gate. An all-zero reading therefore has two causes (the flag
/// is off, or nothing reached this tier with an `instanceof` in it) and the
/// census cannot separate them; the flag's state can, and the line that prints
/// these says so.
///
/// Counted at EMISSION rather than in the predicate (review #80), as the
/// single-pass five and the `checkcast` four are: a code-buffer retry that
/// re-emits a method counts its sites again, so these are upper bounds on
/// distinct source sites, to be compared against zero and against each other and
/// never divided.
#[must_use]
pub fn instanceof_ir_inline_sites() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &RECLAMATION.typecheck_sites;
    (
        c.instanceof_ir_prim_array.load(Relaxed),
        c.instanceof_ir_class_id.load(Relaxed),
        c.instanceof_ir_final_miss.load(Relaxed),
        c.instanceof_ir_no_target_id.load(Relaxed),
    )
}

/// Snapshot of the single-pass `instanceof` site census:
/// `(prim_array, class_id, final_miss, refused_no_target_id,
/// refused_untrusted)`.
///
/// The twin of [`checkcast_inline_sites`], in the same tuple ORDER as the
/// `checkcast` arm's `if`/`else if` chain, so the two `[cratonvm]` lines that
/// print them can be read side by side. `final_miss` is a SUBSET of `class_id`
/// and is the only element with no `checkcast` counterpart; see
/// `TypecheckSiteCensus::instanceof_final_miss`.
///
/// **All five read zero when `CRATONVM_JIT_CHECKCAST_INLINE=0`**, because the
/// whole arm — census included — sits under `checkcast_inline_enabled()`, which
/// is exactly what the `checkcast` counters do. That means an all-zero reading
/// has two causes (the flag is off, or this workload compiled no `instanceof`)
/// and the census cannot separate them; the flag's state can, and the line that
/// prints these says so.
///
/// **This is the SINGLE-PASS door only.** The optimizing tier's own arm is
/// [`instanceof_ir_inline_sites`], and it is the door a hot method reaches — so
/// a zero here on a workload whose hot methods all tiered up is the expected
/// reading, not a finding. The two are printed on one line and compared; they
/// are never summed.
#[must_use]
pub fn instanceof_inline_sites() -> (u64, u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &RECLAMATION.typecheck_sites;
    (
        c.instanceof_prim_array.load(Relaxed),
        c.instanceof_class_id.load(Relaxed),
        c.instanceof_final_miss.load(Relaxed),
        c.instanceof_no_target_id.load(Relaxed),
        c.instanceof_untrusted.load(Relaxed),
    )
}

/// Snapshot of the [`CHECKCAST_INLINE_SITES`] counters: single-pass sites,
/// optimizing-tier sites, and the two refusal causes.
pub fn checkcast_inline_sites() -> (u64, u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        CHECKCAST_INLINE_SITES.load(Relaxed),
        CHECKCAST_INLINE_SITES_IR.load(Relaxed),
        CHECKCAST_INLINE_SITES_PRIM_ARRAY.load(Relaxed),
        CHECKCAST_INLINE_NO_TARGET_ID.load(Relaxed),
        CHECKCAST_INLINE_UNTRUSTED.load(Relaxed),
    )
}

/// The interned name of a typecheck site, for the emitters' descriptor tests.
///
/// # Safety
/// `ptr`/`len` must be a pair produced by [`intern_typecheck_target`], which
/// leaks its names for the life of the process.
pub unsafe fn typecheck_site_name<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() || len == 0 {
        return None;
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).ok()
}

/// Snapshot of [`FINAL_INVOKEVIRTUAL_PINNED`].
pub fn final_invokevirtual_pinned() -> u64 {
    FINAL_INVOKEVIRTUAL_PINNED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Snapshot of [`PRIVATE_INVOKEVIRTUAL_PINNED`].
pub fn private_invokevirtual_pinned() -> u64 {
    PRIVATE_INVOKEVIRTUAL_PINNED.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_DBG_JIT_SLOT_OVERLAP=1` — report a spill reservation that hands
/// out a frame slot an OPEN inline scope still owns. See
/// `Compiler::dbg_note_spill_overlap`.
pub(crate) fn dbg_jit_slot_overlap() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// Parsed `CRATONVM_JIT_DENY` filter (see the `try_compile` call site).
/// `None` = disabled.
///
/// This is the SINGLE force-interpret lever. It absorbed
/// `CRATONVM_JIT_BISECT_SKIP` (deleted 2026-07-31 along with
/// `vm/src/jit/skip_list.rs`), which did the same `Class.method` match on a
/// smaller feature set: entries here are substrings of `Class.method`, so an
/// exact `org/h2/mvstore/MVStore.commit` pins one method and a bare
/// `org/keycloak/` pins a whole package.
///
/// EMPTY ENTRIES ARE DROPPED, and that is load-bearing rather than tidiness.
/// A substring match against `""` is true for every method, so one stray
/// separator — `CRATONVM_JIT_DENY=org/h2/,` — used to turn a one-package deny
/// into `--nojit` for the whole run. That is the exact failure
/// [`jit_force_interpret`]'s doc comment warns about (a lever that reads as a
/// result and is really a no-op) arriving from the other side: every bisect
/// step would say "the crash goes away", because nothing was compiled at all.
/// [`jit_bisect_only_filter`] has always filtered its own empty entries; the
/// two levers now agree.
pub(crate) fn jit_deny_filter() -> Option<&'static Vec<String>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<Vec<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let v = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()?;
            let entries: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if entries.is_empty() {
                return None;
            }
            Some(entries)
        })
        .as_ref()
}

/// Parsed `CRATONVM_JIT_BISECT_ONLY` allowlist — the INVERSE of
/// `CRATONVM_JIT_DENY`: when set, a class whose name starts with none of the
/// listed prefixes is force-interpreted. Bisects "which package holds the
/// miscompile" from the other direction, which `CRATONVM_JIT_DENY` cannot
/// express. Moved here from `vm/src/jit/skip_list.rs` when that file was
/// deleted (2026-07-31); deliberately NOT folded into the deny lever, because
/// allow-list and deny-list are different questions. `None` = disabled.
pub(crate) fn jit_bisect_only_filter() -> Option<&'static Vec<String>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<Vec<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let v = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()?;
            let prefixes: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if prefixes.is_empty() {
                return None;
            }
            Some(prefixes)
        })
        .as_ref()
}

/// Do the bisect levers force `class_name`.`method_name` to stay interpreted?
///
/// **Call this at EVERY codegen entry point, not just [`try_compile`].** Both
/// levers used to be checked only inside `try_compile`, and two paths reach the
/// backend without going through it — `execute`'s eager first-call single-pass
/// compile, and `compile_osr_artifact`, which says so in its own comment
/// ("This path calls the backend directly instead of going through
/// `try_compile`"). An entire class of compiled body was therefore invisible to
/// both levers.
///
/// That is worse than a missing feature, because it makes the levers *lie*.
/// Isolating a miscompile with them is a proof by elimination, and "deny this
/// package and the crash goes away" only means something if the deny actually
/// stopped a compile. Measured 2026-08-04 against the annotation-scan SIGSEGV
/// (the retired `annotation-scan-arrayread-sigsegv` write-up):
/// `CRATONVM_JIT=bisect-only=zzzNoSuchPrefix` — a prefix matching nothing, so
/// nothing should compile at all — still left **11 methods compiled** and the
/// crash still reproduced. Every bisect step read "no effect", which reads as
/// an exoneration and was really a silent no-op.
///
/// The levers are otherwise unchanged: `CRATONVM_JIT_DENY` is a
/// comma-separated list of substrings matched against `Class.method`, and
/// `CRATONVM_JIT_BISECT_ONLY` is the inverse allowlist of class-name prefixes.
/// Both are no-ops unless set, and the `format!` is reached only when the deny
/// lever is actually configured.
pub fn jit_force_interpret(class_name: &str, method_name: &str) -> bool {
    force_interpret_matches(
        jit_deny_filter().map(|v| v.as_slice()),
        jit_bisect_only_filter().map(|v| v.as_slice()),
        class_name,
        method_name,
    )
}

/// The matching rule behind [`jit_force_interpret`], with the two parsed
/// filters passed in rather than read from the process-wide `OnceLock`s — so
/// it is unit-testable without racing every other test in this binary for the
/// one-shot env read.
pub(crate) fn force_interpret_matches(
    deny: Option<&[String]>,
    bisect_only: Option<&[String]>,
    class_name: &str,
    method_name: &str,
) -> bool {
    if let Some(filter) = deny {
        let sig = format!("{class_name}.{method_name}");
        if filter.iter().any(|f| sig.contains(f.as_str())) {
            return true;
        }
    }
    bisect_only.is_some_and(|prefixes| !prefixes.iter().any(|p| class_name.starts_with(p.as_str())))
}


/// Diagnostic: number of compile requests short-circuited because the method
/// was already bail-listed. Each short-circuit saves the ~50µs we'd otherwise
/// have spent re-running scan/IR/lowering only to re-hit the same backend bail.
///
/// Counts **every** door since the admission gate landed, not just
/// `try_compile`: the OSR door's hand-copied bail-list check never incremented
/// this, so the figure understated the saving by exactly the traffic that
/// motivated adding the check there (35 923 pipelines on `Nat.inc`).
pub fn jit_bail_shortcircuits() -> u64 {
    JIT_BAIL_SHORTCIRCUITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record one bail-list short-circuit. Called from [`compile_gate::admit`].
pub(crate) fn note_jit_bail_shortcircuit() {
    JIT_BAIL_SHORTCIRCUITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Why each compiled frame in a stack trace got, or did not get, a line.
///
/// Lives in this crate rather than beside its only writer
/// (`vm::jit::conservative_roots::activation_bci`) so that
/// [`tiered::dump_method_stats_to_stderr`] can print it: the `vm` crate depends
/// on this one, not the other way round.
///
/// **This is a refusal census, not a hit rate**, and the distinction is the
/// whole point. The audit branch's `bci_lookup_census` was dropped in the
/// 2026-09-01 merge along with the two-source lookup it described, and the
/// lesson recorded in its place was that *a silent fallback producing a
/// plausible answer is indistinguishable, from the outside, from the precise
/// path working*. A bare `answered/total` pair has the same defect one level
/// up: it cannot say whether the frames with no line are aarch64 artifacts, an
/// optimizing tier with no translation table, frames stopped between
/// safepoints, or a kill switch someone left set in an environment. Each of
/// those wants a different fix and three of them are invisible in a trace,
/// which prints `(Unknown Source)` for all of them.
///
/// Slots, in the order [`compiled_frame_line_counts`] returns them:
///
/// | # | name | meaning |
/// |---|---|---|
/// | 0 | `single-pass` | answered from the safepoint-id slot directly |
/// | 1 | `ir` | answered through `CompiledMethod::safepoint_bci_table` |
/// | 2 | `npe-trap` | answered from an inline null check's trap site |
/// | 3 | `no-sp-id` | no usable safepoint id: no slot reserved (`sp_id_slot_off == 0`, which is also every aarch64 artifact), an out-of-band RBP, or the prologue's unset sentinel |
/// | 4 | `id-unrecorded` | the id named no safepoint of this artifact's own |
/// | 5 | `ir-untranslated` | an optimizing-tier id with no `(id, bci)` row |
/// | 6 | `out-of-range` | the recovered value is not a spec-legal bci |
/// | 7 | `switched-off` | `CRATONVM_JIT_NO_COMPILED_FRAME_LINES` or `CRATONVM_JIT_NO_IR_FRAME_LINES` |
///
/// | 8 | `innermost-unrecorded` | SUB-column of 4: the frame was read through the innermost-frame mirror's RBP (no return address of its own) -- a dead innermost frame shows here (OSRFRAME18-1) |
///
/// Slots 3-7 are the populations the page
/// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902` had to
/// reason about with no instrument at all. Slots 0-7 partition the verdicts;
/// slot 8 is NOT a ninth verdict but counts a subset of slot 4 (JIT round 19
/// proposal wave 7, lane gc7, OSRFRAME18-1), so a total of frames is the sum
/// of slots 0-7 only.
pub(crate) static COMPILED_FRAME_LINE_COUNTS: [std::sync::atomic::AtomicU64; FRAME_LINE_SLOTS] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Index into [`COMPILED_FRAME_LINE_COUNTS`]: answered from the safepoint-id
/// slot, single-pass backend.
pub const FRAME_LINE_ANSWERED_SINGLE_PASS: usize = 0;
/// Answered through the optimizing tier's `(id, bci)` table.
pub const FRAME_LINE_ANSWERED_IR: usize = 1;
/// Answered from an inline null check's recorded trap site.
pub const FRAME_LINE_ANSWERED_NPE_TRAP: usize = 2;
/// No usable safepoint id in the frame.
pub const FRAME_LINE_REFUSED_NO_SP_ID: usize = 3;
/// The id named no safepoint this artifact recorded.
pub const FRAME_LINE_REFUSED_ID_UNRECORDED: usize = 4;
/// An optimizing-tier id with no translation.
pub const FRAME_LINE_REFUSED_IR_UNTRANSLATED: usize = 5;
/// The recovered value is not a spec-legal bci.
pub const FRAME_LINE_REFUSED_OUT_OF_RANGE: usize = 6;
/// A kill switch is set.
pub const FRAME_LINE_REFUSED_SWITCHED_OFF: usize = 7;
/// JIT round 19 proposal wave 7, lane gc7 (OSRFRAME18-1): of the
/// [`FRAME_LINE_REFUSED_ID_UNRECORDED`] refusals, those of a frame read
/// through the innermost-frame mirror's RBP. A sub-column, counted beside
/// slot 4, never instead of it.
pub const FRAME_LINE_INNERMOST_UNRECORDED: usize = 8;
/// The census's column count (slots 0-7 partition the verdicts, slot 8 is
/// a sub-column of slot 4).
pub const FRAME_LINE_SLOTS: usize = 9;

/// Human names, parallel to the slot indices, so the dump and any future
/// consumer cannot disagree about which column is which.
pub const FRAME_LINE_SLOT_NAMES: [&str; FRAME_LINE_SLOTS] = [
    "single-pass",
    "ir",
    "npe-trap",
    "no-sp-id",
    "id-unrecorded",
    "ir-untranslated",
    "out-of-range",
    "switched-off",
    "innermost-unrecorded",
];

/// Record one verdict. One bump, taken by every arm of `activation_bci`, so a
/// new refusal cannot be added without choosing a column for it.
#[inline]
pub fn note_compiled_frame_line(slot: usize) {
    if let Some(c) = COMPILED_FRAME_LINE_COUNTS.get(slot) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Record a whole walk's verdicts at once: `counts[slot]` more in each column
/// (JIT round 16 wave 15, lane throwfill). The same census as calling
/// [`note_compiled_frame_line`] once per verdict, with one update of the
/// shared counter per NON-ZERO column instead of one per compiled frame: a
/// walk of a deep compiled stack bumped the same process-wide cache line once
/// per activation, and every throwing thread bumps the same line.
pub fn note_compiled_frame_lines(counts: &[u64; FRAME_LINE_SLOTS]) {
    for (slot, &n) in counts.iter().enumerate() {
        if n != 0 {
            if let Some(c) = COMPILED_FRAME_LINE_COUNTS.get(slot) {
                c.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Read the census. See [`COMPILED_FRAME_LINE_COUNTS`] for the columns.
pub fn compiled_frame_line_counts() -> [u64; FRAME_LINE_SLOTS] {
    let mut out = [0u64; FRAME_LINE_SLOTS];
    for (i, slot) in COMPILED_FRAME_LINE_COUNTS.iter().enumerate() {
        out[i] = slot.load(std::sync::atomic::Ordering::Relaxed);
    }
    out
}

/// Interpreter round i5 wave 102, lane L2 (page
/// `i101-L2-compiled-frame-without-a-safepoint-id-has-no-line`):
/// `CRATONVM_JIT_RETURN_SITE_LINE_FALLBACK` (default ON, `=0` off). The
/// optimizing tier records a `(return address, bci)` row for every call it
/// keys (`CompiledMethod::return_site_bci_table`) in EVERY body, not only a
/// `frame_holds_no_references` one, and a stack walk places a frame below the
/// innermost one by its return address when its safepoint-id slot cannot say
/// (any of the four refusals after `no-sp-id`). The row is the call's own bci,
/// so the answer is exact, never a guess; a frame whose return address has no
/// row is still refused. Read per compile and per refused frame.
pub fn return_site_line_fallback_enabled() -> bool {
    true
}

/// What the single-pass backend's operand-spill cursor did, and why a compile
/// was refused when it ran out of room.
///
/// `spill-range-exhausted` is a REFUSAL, and a refused compile is invisible:
/// the method keeps running interpreted and nothing says so. The one thing the
/// tree could say about it was the last bail site of the last compile
/// (`take_jit_bail_site`), a single thread-local slot with no count — so
/// "does this happen at all, and on what?" had no answer, and the fix that was
/// tried for it on 2026-09-02 was measured by a GC-stress test rather than by
/// the cursor it moved.
///
/// | # | name | meaning |
/// |---|---|---|
/// | 0 | `flush-calls` | `flush_scratch_registers` invocations |
/// | 1 | `flush-reserved` | words the flush took from the cursor, growing the region |
/// | 2 | `flush-canonical` | words the flush wrote to the position's own `base + i*8` home instead of taking a new one |
/// | 3 | `exhausted` | compiles refused `spill-range-exhausted` |
/// | 4 | `past-limit` | compiles refused `spill-cursor-past-limit` |
/// | 5 | `peak-words` | high-water mark of live spill words in any one compile (a MAX, not a sum) |
/// | 6 | `res-push` | the ordinary operand push |
/// | 7 | `res-invalidate` | `invalidate_callee_saved` re-homing register-aliased entries |
/// | 8 | `res-total` | every word reserved. NOT a counter: it is DERIVED at read time as the sum of the seven reason columns, so the partition is structural. Counting it separately and asserting the sum could not work — the columns are process-global atomics and seven loads plus an eighth are never a consistent snapshot while other threads compile |
/// | 9 | `min-headroom` | the FEWEST words left between a reservation's end and `spill_limit_offset`, over every compile (a MIN; `u64::MAX` means nothing reserved) |
/// | 10 | `inline-reserve-sum` | largest per-compile inline reserve as `spill_size` computes it today: a SUM over every site (a MAX over compiles) |
/// | 11 | `inline-reserve-path` | what the same compile would need if the reserve were a MAX over top-level sites and over each site's deepest nested PATH (a MAX over compiles) |
/// | 13 | `res-inline-locals` | an inlined callee's local frame |
/// | 14 | `res-inline-merge` | an inlined body's branch-merge area |
/// | 15 | `res-call-service` | the direct-call argument-service copy |
/// | 17 | `range-probe-declined` | a `spill_range_fits` PROBE answered no. Separate from `exhausted`, which counts only ranges that were actually being taken — the two used to be the same number, because the probe and the reservation shared one function |
/// | 16 | `res-helper-args` | a helper's argument buffer or out-parameter (intrinsic dispatch, FFM, the monitor receiver) |
/// | 12 | `inline-reserve-spent` | what `spill_size` ACTUALLY added (a MAX over compiles). The engagement counter: it equals column 10 with the switch off and column 11 with it on, and inferring which without measuring it is how an inert change ships |
///
/// Column 2 is retired and reads zero. It was the engagement counter for a
/// canonical-home flush — store to `base + i*8`, reclaim a dead word below the
/// cursor — and it read ZERO in every arm at every budget, which is what
/// withdrew that change. The column is kept so the number has somewhere to go
/// if anyone tries the idea again, and so this record does not have to be
/// re-derived; see `Compiler::flush_home`.
///
/// Column 9 is the one that answers "how close did anything actually get?".
/// `peak-words` alone cannot: the limit is `max_stack` words and varies per
/// method, so 19 words is nearly the whole budget for one method and a rounding
/// error for another. A refusal count of zero plus a large minimum headroom is
/// a much stronger statement than the refusal count on its own.
pub(crate) static SPILL_CURSOR_COUNTS: [std::sync::atomic::AtomicU64; 18] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(u64::MAX),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// `flush_scratch_registers` invocations.
pub const SPILL_FLUSH_CALLS: usize = 0;
/// Words the flush took from the spill cursor.
pub const SPILL_FLUSH_RESERVED: usize = 1;
/// Words the flush wrote to the position's own canonical home.
pub const SPILL_FLUSH_CANONICAL: usize = 2;
/// Compiles refused because the spill range ran out.
pub const SPILL_REFUSED_EXHAUSTED: usize = 3;
/// Compiles refused because the cursor was already past the limit.
pub const SPILL_REFUSED_PAST_LIMIT: usize = 4;
/// High-water mark of live spill words in any one compile.
pub const SPILL_PEAK_WORDS: usize = 5;
/// Words reserved by the ordinary operand push.
pub const SPILL_RES_PUSH: usize = 6;
/// Words reserved by `invalidate_callee_saved`.
pub const SPILL_RES_INVALIDATE: usize = 7;
/// Every word reserved, by any caller. Derived, never stored — see the table.
pub const SPILL_RES_TOTAL: usize = 8;

/// The seven columns that partition [`SPILL_RES_TOTAL`], in `SpillReason` order.
pub const SPILL_RES_REASON_COLUMNS: [usize; 7] = [
    SPILL_RES_PUSH,
    SPILL_FLUSH_RESERVED,
    SPILL_RES_INVALIDATE,
    SPILL_RES_INLINE_LOCALS,
    SPILL_RES_INLINE_MERGE,
    SPILL_RES_CALL_SERVICE,
    SPILL_RES_HELPER_ARGS,
];
/// Fewest words ever left between a reservation and the spill limit.
pub const SPILL_MIN_HEADROOM: usize = 9;
/// Largest per-compile inline reserve, summed over sites as today.
pub const SPILL_INLINE_RESERVE_SUM: usize = 10;
/// The same compile's requirement if the reserve were a max over sites/paths.
pub const SPILL_INLINE_RESERVE_PATH: usize = 11;
/// What `spill_size` actually added for inlining.
pub const SPILL_INLINE_RESERVE_SPENT: usize = 12;
/// An inlined callee's local frame.
pub const SPILL_RES_INLINE_LOCALS: usize = 13;
/// An inlined body's branch-merge area.
pub const SPILL_RES_INLINE_MERGE: usize = 14;
/// The direct-call argument-service copy.
pub const SPILL_RES_CALL_SERVICE: usize = 15;
/// A helper's argument buffer or out-parameter.
pub const SPILL_RES_HELPER_ARGS: usize = 16;
/// A `spill_range_fits` probe answered no. Not a reservation, not in the
/// partition — a question, counted so making it answerable did not lose it.
pub const SPILL_RANGE_PROBE_DECLINED: usize = 17;
/// Alias: the flush's own reservation column, named for `SpillReason::Flush`.
pub const SPILL_RES_FLUSH: usize = SPILL_FLUSH_RESERVED;

/// Human names, parallel to the slot indices.
pub const SPILL_CURSOR_SLOT_NAMES: [&str; 18] = [
    "flush-calls",
    "flush-reserved",
    "flush-canonical",
    "exhausted",
    "past-limit",
    "peak-words",
    "res-push",
    "res-invalidate",
    "res-total",
    "min-headroom",
    "inline-reserve-sum",
    "inline-reserve-path",
    "inline-reserve-spent",
    "res-inline-locals",
    "res-inline-merge",
    "res-call-service",
    "res-helper-args",
    "range-probe-declined",
];

/// Add `n` to one column. `peak-words` must not go through here — it is a
/// maximum, and summing maxima produces a number that describes no run.
#[inline]
pub fn note_spill_cursor(slot: usize, n: u64) {
    debug_assert!(
        slot != SPILL_PEAK_WORDS && slot != SPILL_MIN_HEADROOM,
        "peak-words is a max and min-headroom a min; neither is a sum"
    );
    if let Some(c) = SPILL_CURSOR_COUNTS.get(slot) {
        c.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Record one compile's inline reserve, as computed today and as a
/// max-over-paths alternative would compute it. Both are maxima over compiles:
/// the question is how big the worst frame gets, not how many frames there are.
#[inline]
pub fn note_inline_reserve(sum_words: u64, path_words: u64, spent_words: u64) {
    SPILL_CURSOR_COUNTS[SPILL_INLINE_RESERVE_SUM]
        .fetch_max(sum_words, std::sync::atomic::Ordering::Relaxed);
    SPILL_CURSOR_COUNTS[SPILL_INLINE_RESERVE_PATH]
        .fetch_max(path_words, std::sync::atomic::Ordering::Relaxed);
    SPILL_CURSOR_COUNTS[SPILL_INLINE_RESERVE_SPENT]
        .fetch_max(spent_words, std::sync::atomic::Ordering::Relaxed);
}

/// Raise the peak-words high-water mark to `words` if it is higher, and lower
/// the headroom low-water mark to `headroom` if it is smaller. One call, so a
/// reservation cannot record one and forget the other.
#[inline]
pub fn note_spill_peak(words: u64, headroom: u64) {
    SPILL_CURSOR_COUNTS[SPILL_PEAK_WORDS].fetch_max(words, std::sync::atomic::Ordering::Relaxed);
    SPILL_CURSOR_COUNTS[SPILL_MIN_HEADROOM]
        .fetch_min(headroom, std::sync::atomic::Ordering::Relaxed);
}

/// Read the census. See [`SPILL_CURSOR_COUNTS`] for the columns.
pub fn spill_cursor_counts() -> [u64; 18] {
    let mut out = [0u64; 18];
    for (i, slot) in SPILL_CURSOR_COUNTS.iter().enumerate() {
        out[i] = slot.load(std::sync::atomic::Ordering::Relaxed);
    }
    // `res-total` is derived, not counted. Every reservation bumps exactly one
    // reason column, so the sum IS the total by construction and no reservation
    // can reach the cursor without landing in it.
    out[SPILL_RES_TOTAL] = SPILL_RES_REASON_COLUMNS.iter().map(|&c| out[c]).sum();
    out
}

/// How often each of `stackwalker::drop_osr_continuations`' two rules removed a
/// compiled entry that was the SAME ACTIVATION as an interpreter frame.
///
/// | # | name | rule |
/// |---|---|---|
/// | 0 | `osr-authoritative` | rule 1, decided by the live-OSR-continuation registry |
/// | 1 | `osr-heuristic` | rule 1, decided by `can_osr_enter(frame.pc)` because the registry had nothing to say |
/// | 2 | `call-opcode` | rule 2, an ordinary compiled activation whose interpreter frame is not suspended at an `invoke*` |
/// | 3 | `osr-heuristic-spared-caller` | NOT a drop: rule 1's pc heuristic matched a frame suspended at an `invoke*`, and the entry was kept (wave 127, lane L3) |
///
/// Rule 2 is why this exists. Its revert shape is asserted by no test, and
/// deliberately so: the only place it was ever "observed" was behind
/// `CRATONVM_JIT_NO_INLINE=1`, a variable that does not exist and never did, so
/// that arm ran the default configuration and isolated nothing. The shipped fix
/// therefore rests on an opcode PROOF rather than on a measurement, and a check
/// whose expected output nobody has measured is a false red waiting to happen.
///
/// A counter is what an unmeasurable claim can honestly have instead: it cannot
/// say the rule is right, but it can say whether it ever FIRES, which is the
/// question "does this code do anything at all" that no green test answers. A
/// permanent zero across real workloads is itself a finding.
pub(crate) static STACK_WALK_DEDUPE_COUNTS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Rule 1, decided by the live-OSR-continuation registry.
pub const DEDUPE_OSR_AUTHORITATIVE: usize = 0;
/// Rule 1, decided by the `can_osr_enter` pc heuristic.
pub const DEDUPE_OSR_HEURISTIC: usize = 1;
/// Rule 2, the ordinary compiled activation.
pub const DEDUPE_CALL_OPCODE: usize = 2;
/// NOT a dedupe: rule 1's pc heuristic matched, but the interpreter frame is
/// suspended at an `invoke*`, so the compiled entry is its recursive callee and
/// was kept (interpreter round i6 wave 127, lane L3,
/// `CRATONVM_JIT_OSR_DEDUPE_SPARES_INVOKE_CALLER`). Before that switch each of
/// these was an `osr-heuristic` drop of a real frame.
pub const DEDUPE_OSR_HEURISTIC_SPARED_CALLER: usize = 3;

/// Names parallel to the slot indices. See [`STACK_WALK_DEDUPE_COUNTS`].
pub const DEDUPE_SLOT_NAMES: [&str; 4] =
    ["osr-authoritative", "osr-heuristic", "call-opcode", "osr-heuristic-spared-caller"];

/// Record one dedupe. See [`STACK_WALK_DEDUPE_COUNTS`].
#[inline]
pub fn note_stack_walk_dedupe(slot: usize) {
    if let Some(c) = STACK_WALK_DEDUPE_COUNTS.get(slot) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Read the dedupe census. See [`STACK_WALK_DEDUPE_COUNTS`].
pub fn stack_walk_dedupe_counts() -> [u64; 4] {
    let mut out = [0u64; 4];
    for (i, slot) in STACK_WALK_DEDUPE_COUNTS.iter().enumerate() {
        out[i] = slot.load(std::sync::atomic::Ordering::Relaxed);
    }
    out
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct JitCompileMethodKey {
    /// The declaring class's identity; `ClassId(0)` when the compile had none.
    pub(crate) class_id: cratonvm_types::ClassId,
    pub(crate) class_name: String,
    pub(crate) method_name: String,
    pub(crate) descriptor: String,
}

impl JitCompileMethodKey {
    /// An id-less key, which stands for every same-named class.
    #[cfg(test)]
    pub(crate) fn new(class_name: &str, method_name: &str, descriptor: &str) -> Self {
        Self::of_class(
            cratonvm_types::ClassId::new(0),
            class_name,
            method_name,
            descriptor,
        )
    }

    pub(crate) fn of_class(
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Self {
        Self {
            class_id,
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
        }
    }

    pub(crate) fn from_cached(cached: &CachedBytecodeMethod) -> Self {
        Self::of_class(
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    }

    /// Whether the names are these. Identity is [`Self::may_be_class`]'s.
    pub(crate) fn matches(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        self.class_name == class_name
            && self.method_name == method_name
            && self.descriptor == descriptor
    }

    /// Whether this same-named key may be about the class `class_id`: identity
    /// decides when both sides carry one, and the names (already matched)
    /// decide otherwise -- the `MethodVerdicts::belongs_to_class` rule.
    pub(crate) fn may_be_class(&self, class_id: Option<cratonvm_types::ClassId>) -> bool {
        match class_id {
            Some(id) if id.as_u32() != 0 && self.class_id.as_u32() != 0 => id == self.class_id,
            _ => true,
        }
    }
}

/// One recursion-cycle set of a [`JitVerdictRegistry`], its members bucketed
/// by the hash of their names so that a lookup by names allocates nothing.
///
/// Interpreter round i1 wave 12: a member carries the identity of its
/// declaring class (the compile stack's `CachedBytecodeMethod::
/// declaring_class_id`), so two loaders' same-named classes no longer share a
/// cycle verdict. An asker that cannot name the class it calls, and a member
/// recorded without an identity, fall back to the names, which is the
/// conservative (dispatched) answer.
#[derive(Default)]
pub(crate) struct CycleSet {
    pub(crate) by_names: rustc_hash::FxHashMap<u64, Vec<JitCompileMethodKey>>,
    /// The `(method, descriptor)` hashes of every member, whatever its class:
    /// the allocation-free pre-filter [`CycleSet::may_name_method`] answers
    /// before a site's declaring class is resolved (interpreter round i1
    /// wave 13).
    pub(crate) method_names: rustc_hash::FxHashSet<u64>,
}

/// What a [`CycleSet`] holds under one method's names.
pub(crate) enum CycleMembership {
    /// No member has these names.
    Absent,
    /// A member recorded without a class identity, which stands for every
    /// same-named class.
    AnyClass,
    /// Members declared by exactly these classes.
    Classes(Vec<cratonvm_types::ClassId>),
}

impl CycleSet {
    pub(crate) fn names_hash(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
        compute_jit_key_hash(
            class_name,
            method_name,
            descriptor,
            cratonvm_types::ClassId::new(0),
        )
    }

    /// The hash of a method's name and descriptor, ignoring its class.
    pub(crate) fn method_hash(method_name: &str, descriptor: &str) -> u64 {
        let mut h = FxHasher::default();
        h.write(method_name.as_bytes());
        h.write_u8(0xff);
        h.write(descriptor.as_bytes());
        h.finish()
    }

    /// Whether a member of ANY class may have this name and descriptor. A
    /// hash collision answers `true`, which only costs the caller one
    /// declaring-class resolution.
    pub(crate) fn may_name_method(&self, method_name: &str, descriptor: &str) -> bool {
        !self.method_names.is_empty()
            && self
                .method_names
                .contains(&Self::method_hash(method_name, descriptor))
    }

    pub(crate) fn insert(&mut self, key: JitCompileMethodKey) {
        self.method_names
            .insert(Self::method_hash(&key.method_name, &key.descriptor));
        let bucket = self
            .by_names
            .entry(Self::names_hash(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
            ))
            .or_default();
        if !bucket.contains(&key) {
            bucket.push(key);
        }
    }

    pub(crate) fn membership(&self, class_name: &str, method_name: &str, descriptor: &str) -> CycleMembership {
        // The overwhelmingly common set is EMPTY (no compile-time cycle has
        // been seen), and every bind ladder asks: answer that before hashing.
        if self.by_names.is_empty() {
            return CycleMembership::Absent;
        }
        let Some(bucket) =
            self.by_names
                .get(&Self::names_hash(class_name, method_name, descriptor))
        else {
            return CycleMembership::Absent;
        };
        let mut classes = Vec::new();
        for key in bucket
            .iter()
            .filter(|key| key.matches(class_name, method_name, descriptor))
        {
            if key.class_id.as_u32() == 0 {
                return CycleMembership::AnyClass;
            }
            classes.push(key.class_id);
        }
        if classes.is_empty() {
            CycleMembership::Absent
        } else {
            CycleMembership::Classes(classes)
        }
    }

    /// Number of members (a method of one class identity each).
    pub(crate) fn len(&self) -> usize {
        self.by_names.values().map(Vec::len).sum()
    }

    /// Drop the members recorded with EXACTLY this class identity; returns
    /// how many. An id-less member (or `ClassId(0)`) is kept: it stands for
    /// every same-named class, and forgetting a cycle member is the
    /// less-conservative direction. `method_names` is left as it is (a stale
    /// entry only costs a pre-filter one resolution).
    pub(crate) fn forget_class_id(&mut self, class_id: cratonvm_types::ClassId) -> usize {
        if class_id.as_u32() == 0 {
            return 0;
        }
        let mut removed = 0;
        self.by_names.retain(|_, bucket| {
            let before = bucket.len();
            bucket.retain(|key| key.class_id != class_id);
            removed += before - bucket.len();
            !bucket.is_empty()
        });
        removed
    }

    /// [`Self::forget_class_id`] for every member of `classes`, in one pass.
    pub(crate) fn forget_class_ids(&mut self, classes: &UnloadedClassSet<'_>) -> usize {
        let mut removed = 0;
        self.by_names.retain(|_, bucket| {
            let before = bucket.len();
            bucket.retain(|key| !classes.contains_nonzero_id(key.class_id));
            removed += before - bucket.len();
            !bucket.is_empty()
        });
        removed
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.by_names.clear();
        self.method_names.clear();
    }
}

impl CycleMembership {
    /// Whether a call into the class `class_id` answers (resolved only when a
    /// member has the names) hits a member. A class the asker cannot name hits
    /// every same-named member.
    pub(crate) fn covers(self, class_id: impl FnOnce() -> Option<cratonvm_types::ClassId>) -> bool {
        match self {
            CycleMembership::Absent => false,
            CycleMembership::AnyClass => true,
            CycleMembership::Classes(classes) => match class_id().filter(|id| id.as_u32() != 0) {
                Some(id) => classes.contains(&id),
                None => true,
            },
        }
    }
}

thread_local! {
    pub(crate) static JIT_COMPILE_STACK: std::cell::RefCell<JitCompileStack> =
        const { std::cell::RefCell::new(JitCompileStack::new()) };
}

/// This thread's nested-compile stack ([`JitCompileStackGuard`]) and the
/// eager-chain fan-out budget of the top-level compile at its bottom.
///
/// JIT round 19, lane eager19 (EAGERCOLD18-5 of `docs/feature-designs/jit-ir-optimizations.md`):
/// the budget lives beside the stack, not in a thread-local of its own (no new
/// process static), so it is reset by construction where a top-level compile
/// starts -- [`JitCompileStackGuard::enter`] at depth 0 -- on every door that
/// compiles, and a VM reads it through [`jit_compile_chain_budget_try_spend`].
/// Dereferences to the key vector, which is all every other reader uses.
pub(crate) struct JitCompileStack {
    pub(crate) keys: Vec<JitCompileMethodKey>,
    /// Transitive (eager-chain) callee compiles charged against the current
    /// top-level compile ([`jit_compile_chain_budget_try_spend`]).
    pub(crate) chain_budget_spent: u32,
}

impl JitCompileStack {
    pub(crate) const fn new() -> Self {
        Self {
            keys: Vec::new(),
            chain_budget_spent: 0,
        }
    }
}

impl std::ops::Deref for JitCompileStack {
    type Target = Vec<JitCompileMethodKey>;
    fn deref(&self) -> &Self::Target {
        &self.keys
    }
}

impl std::ops::DerefMut for JitCompileStack {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.keys
    }
}

/// Charge one transitive callee compile against the eager-chain budget of
/// this thread's current top-level compile; `false` when `max` are already
/// spent. The budget restarts whenever a compile enters at depth 0
/// ([`JitCompileStackGuard::enter`]), so every top-level compile -- whatever
/// door started it -- gets exactly one. A charge made outside any compile
/// (depth 0: a door planning its rows before it compiles, such as the
/// single-pass OSR door's synchronized-callee lookup) is a top-level request
/// of its own and starts a fresh budget, as the VM's own counter did for it
/// (JIT round 19, lane eager19, EAGERCOLD18-5; the VM asks this only under
/// `CRATONVM_JIT_COMPILE_STACK_CHAIN_BUDGET`).
pub fn jit_compile_chain_budget_try_spend(max: u32) -> bool {
    JIT_COMPILE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        if stack.is_empty() {
            stack.chain_budget_spent = 0;
        }
        if stack.chain_budget_spent >= max {
            false
        } else {
            stack.chain_budget_spent += 1;
            true
        }
    })
}

/// The eager-chain budget spent under this thread's current top-level compile
/// ([`jit_compile_chain_budget_try_spend`]); diagnostic and tests.
pub fn jit_compile_chain_budget_spent() -> u32 {
    JIT_COMPILE_STACK.with(|stack| stack.borrow().chain_budget_spent)
}

/// The recursion-cycle half of [`JitVerdictRegistry`].
///
/// `recursive_cycle_methods` holds every compile-time recursion-cycle
/// participant, forever; `sticky_dispatch_methods` is the subset that stays on
/// the dispatch route even under [`jit_cycle_direct_bind_enabled`] (round 9
/// wave 3,
/// `perf-mutual-recursion-forces-every-call-through-invoke-dispatch-20260918.md`).
/// Both were process-global name-keyed sets until interpreter round i1 wave
/// 11; they are the COMPILING VM's now, so one VM's cycle no longer keeps
/// another VM's calls on dispatch. Since wave 12 a member also carries its
/// declaring class's identity (`CycleSet`), so a cycle through one loader's
/// `com/x/Parser.expr` no longer keeps another loader's same-named method on
/// dispatch; an asker that cannot name the class it calls still matches by
/// names, which only ever makes a call more conservative (a dispatched call).
///
/// # Why most cycle participants may bind directly
///
/// A direct call bakes an entry that EXISTS when the caller is compiled, and a
/// compiled body never re-binds. So every baked edge points at an artifact
/// strictly older than its caller (a self call is the one exception, and it has
/// its own inline stack-floor guard). The directed graph of baked edges between
/// artifacts is therefore acyclic: unbounded recursion through a
/// compile-time cycle must cross a non-baked edge -- the cycle-CLOSING edge,
/// whose target was still on the compile stack, unpublished, when it was
/// compiled -- at least once every few frames, and that edge keeps
/// `jit_invoke_dispatch` and its depth guard
/// ([`JitVerdictRegistry::note_recursive_compile_cycle`] answers `true` for it,
/// which every ladder honours before it asks the callee compiler). Later recompiles of the closing caller bind the then-published
/// (older) artifact and add one frame per generation, not a cycle.
///
/// Marking EVERY participant, as the old rule did, made every call into any of
/// them dispatch forever, from any caller: `a -> b -> a` paid the helper round
/// trip on every call (936 ms vs 119 ms self-recursive on `MutualRec`).
///
/// # Who is still sticky
///
/// * Participants of a cycle closed by an INLINE-CACHE edge (`invokevirtual` /
///   `invokeinterface` that was not pinned statically bound). A MIC/PIC slot is
///   filled at run time with the NEWEST entry, so it is not a baked edge and the
///   acyclicity argument above does not cover it: a direct edge into such a
///   cycle could close a loop with no dispatch on it at all.
/// * A caller [`JitVerdictRegistry::mark_current_compile_method_recursive_cycle`]
///   marks because its callee still demanded dispatch after compiling -- the
///   previous rule's transitive propagation, kept for the edges that rule still
///   applies to.
impl JitVerdictRegistry {
    pub(crate) fn mark_recursive_cycle_method(&self, key: JitCompileMethodKey) {
        // Sticky as well: this is the "the callee still demanded dispatch
        // after it compiled" propagation, which the cycle-direct-bind rule
        // keeps.
        self.sticky_dispatch_methods.write().insert(key.clone());
        self.recursive_cycle_methods.write().insert(key);
    }

    /// Mark the method this thread is compiling right now (the top of the
    /// nested-compile stack) as a sticky cycle participant.
    pub(crate) fn mark_current_compile_method_recursive_cycle(&self) {
        if let Some(key) = JIT_COMPILE_STACK.with(|stack| stack.borrow().last().cloned()) {
            self.mark_recursive_cycle_method(key);
        }
    }

    /// Returns true when `target` closes a compile-time cycle to an outer
    /// method. When that happens, every method on the cycle path is marked so
    /// callers that compiled the callee recursively can still avoid baking a
    /// raw direct call.
    ///
    /// `closing_edge_is_inline_cache` is true when the closing call site is an
    /// `invokevirtual` / `invokeinterface` the compile will serve through a
    /// MIC/PIC (not pinned statically bound): such a cycle's participants also
    /// become sticky, because a runtime-filled inline cache is not a baked
    /// edge. A statically bound closing edge only records the cycle.
    ///
    /// `class_id` answers which loaded class the call site names, and is asked
    /// only when an outer compile has the target's names: an outer method of
    /// another same-named class is not the target (`JitCompileMethodKey::
    /// may_be_class`). `None` matches by names.
    pub(crate) fn note_recursive_compile_cycle(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        closing_edge_is_inline_cache: bool,
        class_id: impl FnOnce() -> Option<cratonvm_types::ClassId>,
    ) -> bool {
        let names_on_stack = JIT_COMPILE_STACK.with(|stack| {
            let stack = stack.borrow();
            let current_idx = stack.len().saturating_sub(1);
            stack[..current_idx]
                .iter()
                .any(|k| k.matches(class_name, method_name, descriptor))
        });
        if !names_on_stack {
            return false;
        }
        // Resolved with the stack released: the answer may come from a VM
        // resolver, which must not find this thread's stack borrowed.
        let class_id = class_id();
        let cycle_path = JIT_COMPILE_STACK.with(|stack| {
            let stack = stack.borrow();
            let current_idx = stack.len().checked_sub(1)?;
            let target_idx = stack[..current_idx].iter().position(|k| {
                k.matches(class_name, method_name, descriptor) && k.may_be_class(class_id)
            })?;
            Some(stack[target_idx..].to_vec())
        });

        if let Some(path) = cycle_path {
            if closing_edge_is_inline_cache {
                let mut sticky = self.sticky_dispatch_methods.write();
                for key in &path {
                    sticky.insert(key.clone());
                }
            }
            let mut recursive = self.recursive_cycle_methods.write();
            for key in path {
                recursive.insert(key);
            }
            true
        } else {
            false
        }
    }

    /// Whether a statically bound call into `(class, method, descriptor)` must
    /// stay on the `jit_invoke_dispatch` route instead of binding the callee's
    /// compiled entry directly.
    ///
    /// Direct JIT-to-JIT calls bypass the dispatch depth guard. Before round 9
    /// wave 3 every compile-time recursion-cycle participant answered `true`
    /// here forever. Now (default, `CRATONVM_JIT_CYCLE_DIRECT_BIND`) only the
    /// sticky members do: a cycle closed by statically bound edges keeps its
    /// CLOSING edge on dispatch (the ladders ask
    /// [`JitVerdictRegistry::note_recursive_compile_cycle`] for that
    /// separately), and that is enough, because baked edges only ever point at
    /// older artifacts.
    ///
    /// No method is kept on dispatch by its NAME any more: the
    /// `TOMCAT-SILENT-HANG.5` rule for BCEL `ClassParser.readInterfaces` was
    /// removed in interpreter round i1 wave 11; see
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    ///
    /// Matches by names alone, so every same-named class's member counts; an
    /// asker that knows which loaded class it calls uses
    /// [`JitVerdictRegistry::direct_call_requires_dispatch_of`].
    pub fn direct_call_requires_dispatch(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        self.direct_call_requires_dispatch_of(class_name, method_name, descriptor, || None)
    }

    /// [`JitVerdictRegistry::direct_call_requires_dispatch`] for a call into
    /// the loaded class `class_id` answers. It is asked only when a member has
    /// the names, after the set's lock is released, and a member of another
    /// same-named class (another loader's copy) does not count. `None`, or
    /// `ClassId(0)`, matches by names.
    pub fn direct_call_requires_dispatch_of(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        class_id: impl FnOnce() -> Option<cratonvm_types::ClassId>,
    ) -> bool {
        let set = if jit_cycle_direct_bind_enabled() {
            &self.sticky_dispatch_methods
        } else {
            &self.recursive_cycle_methods
        };
        let membership = set.read().membership(class_name, method_name, descriptor);
        membership.covers(class_id)
    }

    /// Whether a compile open on this thread, or a recorded cycle member, has
    /// this method name and descriptor under ANY class: the cheap pre-filter
    /// before a site's declaring class is resolved for the cycle checks
    /// ([`cycle_site_declaring_class`]). The recursion set holds every sticky
    /// member too. Takes one lock at a time and holds it only for the lookup.
    pub(crate) fn cycle_members_may_name_method(&self, method_name: &str, descriptor: &str) -> bool {
        let open = JIT_COMPILE_STACK.with(|stack| {
            stack
                .borrow()
                .iter()
                .any(|k| k.method_name == method_name && k.descriptor == descriptor)
        });
        open || self
            .recursive_cycle_methods
            .read()
            .may_name_method(method_name, descriptor)
    }

    /// Whether `(class, method, descriptor)` of any same-named class has been
    /// seen on a compile-time recursion cycle (the whole-participant record,
    /// whatever [`jit_cycle_direct_bind_enabled`] says). The bind decision is
    /// [`JitVerdictRegistry::direct_call_requires_dispatch`].
    #[cfg(test)]
    pub fn is_recursive_cycle_participant(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        !matches!(
            self.recursive_cycle_methods
                .read()
                .membership(class_name, method_name, descriptor),
            CycleMembership::Absent
        )
    }

    /// Forget the recursion-cycle members of a class that was UNLOADED, by
    /// its exact identity; returns how many entries were dropped from the two
    /// sets. Called by the tiered manager's unload path only
    /// (`TieredCompilationManager::invalidate_class`).
    ///
    /// The sets were never pruned, so a class-churning VM (scripts, redeploys)
    /// grew them without bound. (This doc used to add that a class loaded
    /// under a recycled `ClassId` inherited a sticky member; within one VM a
    /// `ClassId` is never reused -- `ClassStore::add` appends and `remove`
    /// leaves a tombstone -- so the leak was the whole defect. gc-common w36-c.)
    /// An unloaded class's methods can no longer be compiled or bound, so
    /// forgetting them loses no protection. Not called on redefinition: the
    /// redefined class is still loaded and a direct edge into it could still
    /// close a loop an inline cache is on. Id-less members are kept
    /// ([`CycleSet::forget_class_id`]).
    pub fn forget_cycle_members_of_unloaded_class(
        &self,
        class_id: cratonvm_types::ClassId,
    ) -> usize {
        let sticky = self
            .sticky_dispatch_methods
            .write()
            .forget_class_id(class_id);
        let members = self
            .recursive_cycle_methods
            .write()
            .forget_class_id(class_id);
        sticky + members
    }

    /// Batch form of [`Self::forget_cycle_members_of_unloaded_class`] for a
    /// whole unloaded set: one pass over each of the two sets, by exact
    /// non-zero identity (gc-common w36-c). Returns the total dropped.
    pub fn forget_cycle_members_of_unloaded_classes(
        &self,
        classes: &UnloadedClassSet<'_>,
    ) -> usize {
        if classes.is_empty() {
            return 0;
        }
        let sticky = self
            .sticky_dispatch_methods
            .write()
            .forget_class_ids(classes);
        let members = self
            .recursive_cycle_methods
            .write()
            .forget_class_ids(classes);
        sticky + members
    }

    /// Diagnostic: `(cycle members, sticky members)` recorded in this
    /// registry. A non-zero sticky count is what keeps statically bound calls
    /// into those methods on `jit_invoke_dispatch` for the life of the VM.
    pub(crate) fn recursion_cycle_member_counts(&self) -> (usize, usize) {
        let members = self.recursive_cycle_methods.read().len();
        let sticky = self.sticky_dispatch_methods.read().len();
        (members, sticky)
    }
}

/// `CRATONVM_JIT_CYCLE_DIRECT_BIND` (default ON; `0` restores the pre-wave-3
/// rule): whether a statically bound call into a compile-time recursion-cycle
/// participant may bind the callee's published entry directly. See
/// [`JitVerdictRegistry`]'s cycle-set notes for why that is stack-safe. Consulted at
/// compile time only (a few times per call site), so it is read through the
/// thread-overridable flag layer rather than cached.
pub fn jit_cycle_direct_bind_enabled() -> bool {
    true
}

pub(crate) struct JitCompileStackGuard {
    pub(crate) key: JitCompileMethodKey,
}

impl JitCompileStackGuard {
    pub(crate) fn enter(cached: &CachedBytecodeMethod) -> Self {
        let key = JitCompileMethodKey::from_cached(cached);
        JIT_COMPILE_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            // EAGERCOLD18-5: a top-level compile starts a fresh budget.
            if stack.is_empty() {
                stack.chain_budget_spent = 0;
            }
            stack.push(key.clone());
        });
        Self { key }
    }
}

impl Drop for JitCompileStackGuard {
    fn drop(&mut self) {
        JIT_COMPILE_STACK.with(|stack| {
            let popped = stack.borrow_mut().pop();
            debug_assert_eq!(popped.as_ref(), Some(&self.key));
        });
    }
}

/// Depth of the nested-compile stack on this thread.
///
/// `0` outside any compile; `1` inside a top-level `try_compile`; deeper while a
/// `callee_compiler` compiles a callee inside its caller's compilation.
///
/// # Why a VM-side caller needs this
///
/// Binding a statically bound call site to a raw `CALL` requires the callee to be
/// COMPILED ALREADY, and `vm/.../jit_bridge.rs`'s callee resolver
/// (`direct_callee_lookup`) may compile one transitively to get there. Without a
/// depth bound, a deep call chain compiled bottom-up on one thread would nest one
/// compile per level; with one, the chain stops binding directly past the bound
/// and falls back to the dispatch helper, which is always correct.
pub fn jit_active_compile_depth() -> usize {
    JIT_COMPILE_STACK.with(|stack| stack.borrow().len())
}

/// Whether `(class_name, method_name, descriptor)` is already being compiled
/// somewhere on this thread's nested-compile stack.
///
/// A VM-side transitive callee compile must ask this before recursing:
/// re-entering a compile that is already open would recurse until the depth bound
/// (or the native stack) ran out. Same question
/// [`JitVerdictRegistry::note_recursive_compile_cycle`] answers for this crate's
/// own `callee_compiler`, minus the cycle bookkeeping —
/// the VM caller only needs to decline.
pub fn jit_active_compile_contains(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    JIT_COMPILE_STACK.with(|stack| {
        stack
            .borrow()
            .iter()
            .any(|k| k.matches(class_name, method_name, descriptor))
    })
}

/// The method this thread is compiling right now (the top of its
/// nested-compile stack), as `(declaring class id, class name, method name,
/// descriptor)`; `None` outside any [`try_compile`]. The class id is
/// `ClassId(0)` for a compile that had none.
///
/// JIT round 18 wave 9 (lane eagercold): the VM's inline resolver names the
/// compiling method by this when a constructor splice is refused only because
/// the class is not initialized yet, so the late-bind recompile can be asked
/// for it once the class is (`CRATONVM_JIT_LATE_BIND_UNINIT_CTOR`).
pub fn jit_active_compile_top() -> Option<(cratonvm_types::ClassId, String, String, String)> {
    JIT_COMPILE_STACK.with(|stack| {
        stack.borrow().last().map(|k| {
            (
                k.class_id,
                k.class_name.clone(),
                k.method_name.clone(),
                k.descriptor.clone(),
            )
        })
    })
}

/// Enables raw, machine-code JIT-to-JIT direct calls to a compiled callee:
/// eager callee-compile-and-direct-CALL in this function's caller
/// (`try_compile_inner`) and the inline virtual MIC fast path (`x64.rs`).
/// (`vm/src/jit/helpers.rs`'s OWN separate dispatch-helper direct-entry cache
/// — `jit_invoke_dispatch`/`jit_invoke_virtual_mic`'s cached `entry_ptr` path
/// — is a different, still-default-OFF gate,
/// `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY`: it has its own separate,
/// still-open target-resolution bug on top of the RBP-mirror race below, so
/// it does not share this flag's default.) This includes calls this JIT's
/// own self-recursion detection does not resolve to the dedicated
/// `self_call_patches` fast path (e.g. `BenchSuite.make`/`check`'s tiered
/// background-compile route) — the flag being off routed those through the
/// full `jit_invoke_dispatch` helper on every call, a ~15x regression on
/// bintrees16 (44s vs. 2.9s) that generalizes to any call-heavy recursive
/// workload, not just the ES repro this flag was introduced for.
///
/// A raw JIT-to-JIT CALL updates the per-thread active-RBP mirror to the
/// callee, while the interpreter-owned root-chain entry still describes the
/// caller until the callee's own prologue runs — a GC landing in that window
/// can select the caller's oop map for the callee's frame and lose live
/// roots. This exact mechanism produced the IVFKnn stress-test
/// stale-precise-root-mirror corruption (see
/// ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b-FIXED.md).
/// Closed by two companion fixes that ship alongside this flag:
/// [`Compiler::emit_post_call_rbp_republish`] (re-publishes the caller's RBP
/// after every raw JIT-to-JIT return) and `JitEntryGuard::enter_with_compiled`
/// always retaining precise frame metadata (`vm/src/jit/conservative_roots.rs`)
/// so `scan_compiled_frame_bands` can bound each stacked raw-call frame
/// exactly instead of one imprecise whole-band conservative sweep. Re-enabled
/// default-ON (re-verified against `testSlicesSparseWithFilter` /
/// `testRandomWithFilter` / the full `cratonvm-gc`/`cratonvm-jit`/
/// `cratonvm-vm` --lib suites — see the ES-HANG doc above for results) to
/// close the general-throughput regression; opt out with
/// `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` if a new corruption is ever suspected
/// here again (matches this codebase's established off-switch convention, see
/// [`x64::guarded_inline_getfield_enabled`]).
///
/// NOT OnceLock-cached (mirrors [`x64::guarded_inline_getfield_enabled`]):
/// this is checked at JIT-compile time, never on the runtime hot path within
/// this crate, so re-reading the env var has no measurable cost — and caching
/// would make the flag racy against whichever test/thread compiles first.
/// IR direct-call lowering — may the optimizing IR pipeline bind a resolved,
/// statically-bound `invokestatic` / non-`<init>` `invokespecial` straight to its
/// callee's compiled entry (a raw `CALL`), the way the single-pass backend
/// already does?
///
/// Subordinate to [`direct_jit_callee_calls_enabled`]: the IR path emits exactly
/// the same raw JIT-to-JIT edge the single-pass backend emits, so it must respect
/// the same master switch. `CRATONVM_JIT_IR_DIRECT_CALL=0` is an additional
/// IR-only opt-out for bisecting a regression to this lowering specifically
/// (which restores the historical `jit_invoke_dispatch` route for every IR call
/// site) without also disabling single-pass direct calls.
/// `CRATONVM_JIT_FORCE_C2=1` — request the optimizing tier for every compile,
/// overriding the caller's `optimize` argument. Default off. See the call site
/// in `try_compile_inner` for why it exists.
///
/// NOT `OnceLock`-cached, matching [`direct_jit_callee_calls_enabled`]: read at
/// compile time only, never on a runtime hot path, and caching would make it
/// racy against whichever test thread compiles first.
pub fn force_c2_enabled() -> bool {
    false
}

/// `CRATONVM_DBG_JIT_SINGLE_PASS_ONLY` (zs s7/vl; a measurement switch, unset
/// = off): does the filter keep `class_name.method_name` out of the optimizing
/// tier?
///
/// The value is a comma-separated list of substrings of `Class.method`, with
/// the internal class name -- the matching rule of `CRATONVM_JIT_DENY`
/// ([`jit_force_interpret`]), e.g. `Zs5Bt2Stores.burstPut` or
/// `com/acme/Kernel.`. A matching method:
///
/// * is refused by `try_compile_inner`'s optimizing admission, so every door
///   that asks for the optimizing tier through it -- the method-entry compile,
///   the C1 -> C2 supersede, the optimizing OSR body
///   (`compile_optimizing_artifact`), the scalar self-recursion promotion --
///   falls through to the single-pass backend, as for any other admission
///   refusal;
/// * is not spliced into an optimizing-tier caller at a call site whose
///   constant-pool callee it names (the IR inline planner), so the caller
///   CALLS the method's own body instead of running its bytecode under the
///   optimizing tier's emitters.
///
/// Why it exists: `CRATONVM_TIER_C2_THRESHOLD=2000000000` does NOT keep a hot
/// method in the single-pass tier. It gates only the invocation-count doors;
/// the OSR door and the IR splice into a hot caller still run its bytecode
/// through the optimizing tier. zs s5a's host rows measured exactly that:
/// `Zs5Bt2Stores.burstPut`'s disassembly carried the optimizing tier's fused
/// store gate under the threshold, so the single-pass store gate's A/B arm
/// was vacuous. This switch names the method instead of a hotness bar.
///
/// What it does NOT cover: a body spliced NESTED inside another spliced body
/// (the VM resolves those), and a guarded virtual site whose constant-pool
/// class differs from the receiver's (name the constant-pool class, or the
/// whole caller). Read per compile, never on a run-time path; the `format!`
/// runs only when the variable is set.
pub fn dbg_jit_single_pass_only(class_name: &str, method_name: &str) -> bool {
    false
}

/// The matching rule behind [`dbg_jit_single_pass_only`], with the filter
/// passed in so it is testable without the process environment.
pub(crate) fn single_pass_only_filter_matches(filter: &str, class_name: &str, method_name: &str) -> bool {
    let mut entries = filter.split(',').map(str::trim).filter(|s| !s.is_empty());
    let Some(first) = entries.next() else {
        return false;
    };
    let sig = format!("{class_name}.{method_name}");
    sig.contains(first) || entries.any(|f| sig.contains(f))
}


/// PERF-01: which single-pass-only lowering, if any, applies to this method?
///
/// A thin adapter — it exists to convert this crate's exception table into the
/// `(start, end, handler)` triples `find_bypassable_loop_headers` wants, the
/// same conversion `BackendRequest::exception_ranges` does at backend entry. The
/// decision, and the enumeration behind it, live in `x64::single_pass_only`.
///
/// Called once per compile request, on the compile path only, and rejected on
/// a raw-byte scan before it does any analysis.
/// Whether a method carrying a `java/lang/String` access-intrinsic call site is
/// pinned to the single-pass backend (see the gate in `try_compile_inner`).
///
/// Default ON. `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` is the B arm of an
/// in-binary A/B and the kill switch if a method is ever found where the
/// optimizing body wins despite losing the intrinsic.
///
/// # The premise this whole mechanism rested on, and what is left of it
///
/// The pin was only worth anything because the optimizing tier had NO String
/// access intrinsic — not a worse one, none. That was carried as a belief for
/// months before it was written down as a grep, and the grep has since gone
/// off. It no longer holds unconditionally:
///
/// ```text
/// $ grep -c 'fn try_string_access_intrinsic' jit/src/ir.rs
/// 1
/// $ grep -c 'StringCharAt' jit/src/ir_lower.rs
/// 0
/// $ grep -rln StringCharAt jit/src/
/// jit/src/lib.rs
/// jit/src/ir.rs
/// jit/src/x64/bytecode_walk.rs
/// ```
///
/// Re-read 2026-09-01, the same day the original reading was recorded. The
/// first reading was `jit/src/ir.rs:0` and a two-file list; `ir.rs` has since
/// grown `IrBuilder::try_string_access_intrinsic`, which expands
/// `String.length()`, `String.isEmpty()` and `String.charAt(int)` into
/// ordinary IR nodes.
///
/// RE-KEYED, because the old tripwire could no longer fire. It watched
/// `ir_lower.rs`, and the emitter deliberately landed in `ir.rs` instead: a
/// new `Op::StringCharAt` would have to be added to `ir_verify`'s exhaustive
/// `expected_arity` match, so the expansion emits existing nodes and
/// `ir_lower.rs` stays at 0 forever. A tripwire keyed to a file the feature
/// cannot touch is not a tripwire. It is keyed to the expander's own name
/// now, which is what actually moves.
///
/// What the pin still protects, and how it retires: the expander handles
/// three ACCESSORS on an UNGUARDED (`java/lang/String`) receiver, outside a
/// splice, and only when a `StringFieldLayout` was published. Everything else
/// the single-pass region emits — `hashCode`, `equals`, `compareTo`, both
/// `indexOf` forms, and every `java/lang/CharSequence` site — is still lost
/// by admitting the method, and still gets the full `charAt -> isLatin1 ->
/// StringLatin1.charAt -> String.checkIndex -> Preconditions.checkIndex`
/// chain whose tail is a registered NATIVE. The day
/// `try_string_access_intrinsic` covers those too, the pin,
/// `ir_over_intrinsic_enabled`, [`ir_string_access_expander_handles`] and the
/// fail-closed rule in [`string_intrinsic_pin_declines`] retire together.
///
/// Note that the pin is NOT the only door in front of the expander, and was
/// not the one that made it unreachable: see
/// [`ir_string_access_expander_handles`] for the invoke-planning gate that
/// bailed the whole method before a graph existed, which is why switching the
/// pin off alone measured nothing.
///
/// # The pin is asked at ONE of the three doors, and this population takes
/// # another — MEASURED 2026-09-01
///
/// `probes/CharAtWarmShape.java`, one binary, three arms:
///
/// ```text
/// arm A  default (pin on)                                314-336 ns/char
/// arm B  CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1           66-108 ns/char  ~3x faster
/// arm C  pin off + CRATONVM_JIT_IR_STRING_INTRINSICS=0       421 ns/char  (control)
/// ```
///
/// and the ENGAGEMENT census on arm A, the same workload:
///
/// ```text
/// [cratonvm] JIT String-intrinsic pin: fired=0 blind-no-layout=0
///            blind-no-resolver=0 fail-closed=0
///
/// [cratonvm-jitc] bg-compile  CharAtWarmShape.scanBig(...)I tier=C2 optimized=true osr_bci=12
/// [cratonvm-jitc] OSR-compile CharAtWarmShape.scanBig(...)I entry_pc=12
/// ```
///
/// **All four of the pin's counters read zero on the very workload the pin
/// exists to govern**, in a run where switching the pin off moved that workload
/// 3x. That is the falsifying evidence for the belief that this pin governs
/// this population. It does not: the pin is a term of `try_compile_inner`'s
/// eligibility conjunction, `try_compile_inner` is
/// `compile_gate::CompileDoor::MethodEntry`, and the third line above says this
/// method is compiled through the OSR door — which `jit/src/compile_gate.rs`'s
/// own header records reaching `x64::compile_with_param_slots` DIRECTLY,
/// never through `try_compile_inner`. A counter installed at one door reports
/// zero for traffic through another, and a zero reads as *correctly inert*.
///
/// Arm C is the control that keeps the DECISION intact: lifting the pin
/// *without* the IR String emitter is worse than the default (421 against 336),
/// which independently reproduces the 504-vs-135 ns/call reading recorded at
/// the conjunction in `try_compile_inner`. The pin is right; its placement was
/// not. So the asking moved to the one object all three doors already hold —
/// `compile_gate::CompileAdmission::string_intrinsic_pin_declines`, with a
/// per-door `asked` / `pinned` / `NOT asked` breakdown
/// ([`string_intrinsic_pin_door_census_line`]). Nothing this function returns
/// changed, and nothing [`string_intrinsic_pin_declines`] decides changed.
///
/// The lesson is worth stating flatly, because the audit that preceded this
/// spent five hypotheses on it: every one of the five was about which *method*
/// takes the fast shape — OSR versus method entry as a property of the method,
/// callee warm order, caller kind across six shapes, first-compile context,
/// scale — and each was refuted by its own measurement without ever
/// converging. The discriminator was not a property of the method. It was
/// which DOOR compiled it, and no instrument in the tree reported the pin per
/// door.
///
/// NOT `OnceLock`-cached, matching `ir_direct_calls_enabled` and
/// `x64::guarded_inline_getfield_enabled`: this is read at compile time only,
/// never on a runtime hot path, and caching would make the flag racy against
/// whichever thread compiles first.
pub(crate) fn string_intrinsic_pin_enabled() -> bool {
    !false
}

/// A `java/lang/String` field layout that exists ONLY so
/// [`try_resolve_string_intrinsic`] can be asked the half of its policy that
/// is about the callee's identity rather than about the heap.
///
/// That function's first act after the class-name test is
/// `let layout = string_layout?;`, so handing it `None` makes it answer `None`
/// for EVERY name and descriptor -- such a probe cannot tell `charAt` from
/// `substring`, and any predicate built on it is vacuously false. Only two
/// fields below are read by the name/descriptor ladder: `has_coder` (a legacy
/// `char[]` String bails to native dispatch) and `string_class_id` (a
/// `java/lang/CharSequence` site with no resolved String class id is refused
/// for a reason that has nothing to do with the callee's name). Every offset
/// is zero and this value MUST NOT reach codegen -- it describes no real
/// object. It is consumed by [`ir_string_access_expander_handles`] and
/// nothing else.
pub(crate) const STRING_INTRINSIC_NAME_PROBE: StringFieldLayout = StringFieldLayout {
    value_field_index: 0,
    value_compact_offset: 0,
    value_compact_is_narrow: false,
    value_legacy_offset: 0,
    hash_field_index: 0,
    hash_compact_offset: 0,
    hash_legacy_offset: 0,
    has_coder: true,
    coder_field_index: 0,
    coder_compact_offset: 0,
    coder_legacy_offset: 0,
    coder_compact_is_byte: false,
    // Non-zero purely so a CharSequence site is not refused for want of an id;
    // the caller then rejects every guarded site anyway. Never an object's
    // real class id, and never compared against one.
    string_class_id: 1,
    builder: None,
    array_list: None,
    hash_is_zero_compact_offset: None,
    locale_language: None,
};

/// [`STRING_INTRINSIC_NAME_PROBE`] with a probe `StringBuilder` half, so the
/// resolver's STRINGBUILDER_ACCESS rows answer a NAME question (nio20 wave 4,
/// lane L5: [`ir_string_search_prefix_kind`]'s builder kinds). The class id is
/// non-zero only so the rows are reachable; never compared against an object.
pub(crate) const STRING_BUILDER_INTRINSIC_NAME_PROBE: StringFieldLayout = StringFieldLayout {
    builder: Some(StringBuilderFieldLayout {
        count_field_index: 0,
        count_compact_offset: 0,
        count_legacy_offset: 0,
        value_field_index: 0,
        value_compact_offset: 0,
        value_legacy_offset: 0,
        value_compact_is_narrow: false,
        coder_field_index: 0,
        coder_compact_offset: 0,
        coder_legacy_offset: 0,
        coder_compact_is_byte: false,
        class_id: 1,
        maybe_latin1_compact_offset: None,
        maybe_latin1_false_is_exact: false,
    }),
    ..STRING_INTRINSIC_NAME_PROBE
};

/// Does the OPTIMIZING tier's own String-access expander handle this exact
/// `(class, name, descriptor)`?
///
/// # The third door
///
/// `ir.rs`'s `IrBuilder::try_string_access_intrinsic` expands
/// `String.length()`, `String.isEmpty()` and `String.charAt(int)` into
/// ordinary IR nodes. It is offered the site from `IrBuilder::build`'s `0xb6`
/// arm -- but only AFTER `self.invoke_info.get(&pc)`, which returns
/// `bail_invoke` on a miss. And `invoke_info` is left unset for the WHOLE
/// method whenever the invoke-planning loop in `try_compile_inner` marks any
/// one site non-emittable. That loop's `is_intrinsic_site` carried a bare
/// unconditional `cn == "java/lang/String"`, so a method containing any
/// `java/lang/String` call at all bailed to the single-pass backend before a
/// graph was ever built, and the expander could not be entered by ANY input.
///
/// The consequence was not a slow path, it was an unmeasurable one:
/// `ir.rs`'s own section header documents a one-flag settling arm
/// (`CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1`) that could never have reached
/// the emitter it is meant to price. Reaching it additionally needed
/// `CRATONVM_JIT_IR_OVER_INTRINSIC=1`, which switches this gate off for every
/// intrinsic family at once -- FFM element accessors, `AtomicInteger`,
/// `VarHandle`, the `ByteBuffer` byte accessors -- and so measures a
/// different program.
///
/// This is the three-doors failure `jit/src/compile_gate.rs` exists for. The
/// pin ([`string_intrinsic_pin_enabled`]), the eligibility conjunction, and
/// this invoke-planning gate are three independent refusals of the same
/// method, and opening two of them is indistinguishable from opening none.
///
/// # Why the carve-out is narrow, and stays narrow
///
/// The policy is [`try_resolve_string_intrinsic`]'s, CALLED and not re-listed
/// -- the same discipline `ir.rs` keeps, and the reason a third copy of the
/// name/descriptor table does not exist here. On top of it this asks the two
/// questions the expander asks and the resolver does not:
///
///   * the resolved entry must be one of the three ACCESSORS. The resolver
///     also admits `hashCode`, `equals`, `compareTo` and both `indexOf`
///     forms; the expander answers `SI_NOT_ACCESSOR` for all of them (each is
///     a loop or a lazy-cache protocol, needing control flow the expansion
///     cannot open mid-bytecode), so a method containing one must keep
///     bailing to the single-pass backend that CAN emit it. Nothing about
///     their cost changes here. (nio20 wave 3, lane L2: the optimizing tier
///     now answers those five with an inline call prefix instead, and
///     [`string_site_bails_ir_planning`] / the pin carve them out through
///     their own predicate, [`ir_string_search_prefix_handles`]; this one
///     stays the accessor expander's.)
///   * the site must be UNGUARDED (`guard_class_id == 0`, i.e. declared on
///     `java/lang/String`, which is final). The expander refuses a
///     `java/lang/CharSequence` receiver (`SI_GUARDED_REFUSED`) because no IR
///     node reads an `ObjectHeader` class id, so admitting one would trade an
///     emitted guarded decode for a plain dispatch.
///
/// Everything else declared on `java/lang/String` -- `substring`, `concat`,
/// `split`, every method the single-pass backend has no intrinsic for at all
/// -- still answers `false` here and still bails, byte-for-byte as before.
/// Whether the blanket was over-broad for those is a separate question with
/// its own measurement; this predicate is scoped to the three sites the
/// expander can actually emit, because those are the three where the gate is
/// demonstrably refusing a method the other tier is ready to serve.
///
/// Row 1 of `is_intrinsic_site`, `try_resolve_intrinsic`, cannot defeat this:
/// its String section registers nothing for `java/lang/String` on purpose
/// (the family needs a `StringFieldLayout` and that ladder is
/// layout-independent), so the carve-out is not re-closed one line above it.
///
/// # The switch
///
/// `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT=1` restores the blanket class-name
/// refusal exactly, so the fix and the pre-existing behaviour are A/B-able
/// inside ONE binary -- which is the only way to attribute a `charAt` reading
/// to this change rather than to a rebuild. Note that with the default
/// (`string_intrinsic_pin_enabled`) pin ON, a method with a String accessor
/// site never reaches this loop anyway, so the switch changes nothing until
/// the pin is also off; that is deliberate, and it is why this carve-out does
/// not need a default on of its own.
///
/// NOT `OnceLock`-cached, matching [`string_intrinsic_pin_enabled`] beside
/// it: read at compile time only, never on a runtime hot path, and caching
/// would make the flag racy against whichever thread compiles first.
/// `CRATONVM_JIT_IR_STRING_VALUE_OF_CALL`, **default on** (default on) -- JIT
/// round 19, lane irops19, IN16-1 of `jit-r16-indy-proposals-RETIRED-20261006.md`.
///
/// The optimizing planner's `is_intrinsic_site` carries a blanket
/// `cn == "java/lang/String"` row: every `java/lang/String` invoke except the
/// three accessors [`ir_string_access_expander_handles`] carves out refuses
/// the WHOLE method ("loop-shaped or memory call-site intrinsic; the IR tier
/// has no node for it"). The row exists for the single-pass String
/// intrinsics (`equals`/`hashCode`/`indexOf`/...), which this tier cannot
/// emit. `String.valueOf` is none of them -- `try_resolve_string_intrinsic`
/// names no `valueOf`, so the single-pass body calls it too -- and javac 25
/// emits `String.valueOf(o)` before every string concatenation with an object
/// operand, so the blanket refusal costs every such method the optimizing
/// tier for nothing. Under this switch the `valueOf` overloads leave the
/// row and build as the ordinary `invokestatic` `Op::Call` every other
/// static call is (same dispatch, same `JitInvokeInfo`, same exception edge).
///
/// The overloads admitted are exactly the ones `java/lang/String` declares
/// `static ... valueOf(...)Ljava/lang/String;` in JDK 25, the eight one-arg
/// forms and `valueOf([CII)`; `copyValueOf` stays refused (unmeasured). Read
/// per compile; no `static` (`process_global_statics_ratchet`).
pub(crate) fn ir_string_value_of_call_admits(class: &str, name: &str, descriptor: &str) -> bool {
    class == "java/lang/String"
        && name == "valueOf"
        && matches!(
            descriptor,
            "(Ljava/lang/Object;)Ljava/lang/String;"
                | "([C)Ljava/lang/String;"
                | "([CII)Ljava/lang/String;"
                | "(Z)Ljava/lang/String;"
                | "(C)Ljava/lang/String;"
                | "(I)Ljava/lang/String;"
                | "(J)Ljava/lang/String;"
                | "(F)Ljava/lang/String;"
                | "(D)Ljava/lang/String;"
        )
        && true
}

/// `CRATONVM_JIT_IR_STRING_CODER_ACCESSOR_ADMIT` (JIT round 21, lane strings,
/// page `jr21-latin1-case-rows-FIXED-20261015.md`, row `R20PNatLatin1`
/// latin1-compareToCI; default ON, `=0` off): the three package-private
/// `java/lang/String` accessors `coder()`, `isLatin1()` and `value()` (a field
/// read and a `COMPACT_STRINGS` test; no tier intrinsifies them) leave the
/// planning gate's blanket refusal. Before, `String$CaseInsensitiveComparator
/// .compare(String,String)` (`s1.coder()`), `String.toLowerCase(Locale)`
/// (`isLatin1()`) and every other `java.lang` body reading a coder this way
/// were single-pass bodies, so the `StringLatin1.compareToCI` call prefix --
/// whose only call site is that comparator -- never ran. The sites build as
/// ordinary calls the splice planner inlines. Compile time only, not cached.
pub(crate) fn ir_string_coder_accessor_call_admits(class: &str, name: &str, descriptor: &str) -> bool {
    class == "java/lang/String"
        && matches!(
            (name, descriptor),
            ("coder", "()B") | ("isLatin1", "()Z") | ("value", "()[B")
        )
}

/// `CRATONVM_JIT_IR_STRING_CODER_CTOR_ADMIT` (JIT round 21, lane strings, page
/// `jr21-latin1-case-rows-FIXED-20261015.md`, row `R20PNatLatin1` latin1-toLowerCase;
/// default ON, `=0` off): the package-private `String.<init>(byte[], byte)` at
/// an `invokespecial` of a method declared on `java/lang/StringLatin1` or
/// `java/lang/StringUTF16` leaves the planning gate's blanket `java/lang/String`
/// refusal. `try_resolve_string_intrinsic` names no constructor, so the
/// single-pass body calls it too; the refusal cost the whole method -- every
/// `StringLatin1.toLowerCase` / `toUpperCase` / `replace` / `trim`-style body
/// ending in `new String(result, LATIN1)`, with its per-byte
/// `CharacterDataLatin1` calls -- the optimizing tier for nothing. The site
/// builds as the ordinary constructor `Op::Call`. `=0` restores the refusal.
/// Compile time only, not cached.
pub(crate) fn ir_string_coder_ctor_call_admits(
    caller_class: &str,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    class == "java/lang/String"
        && name == "<init>"
        && descriptor == "([BB)V"
        && matches!(
            caller_class,
            "java/lang/StringLatin1" | "java/lang/StringUTF16"
        )
}



pub(crate) fn ir_string_access_expander_handles(class: &str, name: &str, descriptor: &str) -> bool {
    if false {
        return false;
    }
    match try_resolve_string_intrinsic(class, name, descriptor, Some(STRING_INTRINSIC_NAME_PROBE)) {
        Some((entry, _num_params, _ret, guard_class_id)) => {
            // Compared against the sentinels rather than `from_entry`, which
            // only classifies the CRC32 family -- the same identification
            // `ir.rs` and the single-pass region both make.
            guard_class_id == 0
                && (entry == JitIntrinsic::StringLength.as_entry()
                    || entry == JitIntrinsic::StringIsEmpty.as_entry()
                    || entry == JitIntrinsic::StringCharAt.as_entry())
        }
        None => false,
    }
}

/// nio20 wave 1, lane L3 (NIO19-8): `CRATONVM_JIT_IR_STRING_CALL_ADMIT`
/// (default ON since r23 wave 6, `=0` off) narrows the invoke-planning gate's blanket
/// `java/lang/String` refusal to the members the single-pass backend would
/// actually intrinsify. NOT `OnceLock`-cached, matching
/// [`ir_string_access_expander_handles`]: compile time only.
///
/// nio20 wave 3 (lane L2): still default OFF, deliberately. The five members
/// whose inline body admission would have lost are now answered by the
/// optimizing tier's search prefix and carved out on BOTH arms
/// ([`ir_string_search_prefix_handles`]), so what this switch still moves is
/// only the population with no String intrinsic in either tier (`getChars`,
/// `substring`, constructors, `String`'s own static helpers, ...). Its one
/// measurement (`heapcb`, 2491 -> 3024 ns with it ON) cannot have been a lost
/// String intrinsic -- the intrinsic members still bailed on that arm -- so
/// the regression is the optimizing tier's body for that population, and
/// flipping is a measurement, not a correctness, question:
/// `r20-string-call-admit-heapcb-regression-20261007`.
///
/// JIT round 23 wave 6 (lane L1): **default ON**, `=0` kill (any other value,
/// `callers` included, keeps its meaning). The measurement the history asked
/// for, on `n23-w5`, P-core pinned:
/// - `RxLoop split-comma` 2080 / 2256 -> 1672 / 1322 ns/op;
/// - `R23RefArraycopy split-comma-10` 1550 / 1667 -> 1085 / 1088;
/// - `RxLoop split-ws` 10977 / 10984 -> 10357 / 9328;
/// - `heapcb` unchanged (the getChars prefix moved that population off; with
///   `CRATONVM_JIT_IR_STRING_GETCHARS_PREFIX=0` the old regression is still
///   there, 50 -> 70 ns per put).
///
/// The `callers` arm measured no better than unset (2371 / 2261). The
/// default arm's real cost was `String.split(CIZ)`: its cold
/// `String.valueOf(char)` site at pc 31 refused the whole method, so the
/// JDK's split fast path never left the single-pass tier.
pub(crate) fn ir_string_call_admit_enabled() -> bool {
    true
}

/// Does a `cn.mn desc` invoke make `try_compile_inner`'s invoke-planning
/// loop bail the whole method to the single-pass backend for being a
/// `java/lang/String` site? (nio20 wave 1, lane L3, NIO19-8.)
///
/// Under `CRATONVM_JIT_IR_STRING_CALL_ADMIT=0`: every `java/lang/String`
/// member except the three accessors the optimizing tier expands
/// ([`ir_string_access_expander_handles`]) -- the blanket this gate applied
/// before r23 wave 6, byte for byte.
///
/// Under [`ir_string_call_admit_enabled`] (the default): only a member
/// [`try_resolve_string_intrinsic`] names (`hashCode`, `equals`,
/// `compareTo`, both `indexOf` forms -- the ones whose single-pass inline body
/// a promotion would lose). A member with no intrinsic in EITHER tier
/// (`getChars`, `substring`, `valueOf`, a constructor, ...) is an ordinary
/// call in both, so refusing the optimizing tier for it buys nothing; it is
/// the third door in front of `HeapCharBuffer.put(String,int,int)`
/// (`src.getChars`), whose only intrinsic site, `src.length()`, the
/// optimizing tier expands. Asked through [`STRING_INTRINSIC_NAME_PROBE`], so
/// a site is refused whenever ANY layout with a `coder` would intrinsify it --
/// a superset of the real compile's answer, which is the safe direction here.
/// `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT=1` restores the blanket on every
/// arm.
pub(crate) fn string_site_bails_ir_planning(class: &str, name: &str, descriptor: &str) -> bool {
    if class != "java/lang/String" || ir_string_access_expander_handles(class, name, descriptor) {
        return false;
    }
    if false {
        return true;
    }
    // nio20 wave 3, lane L2: the five search members the optimizing tier now
    // answers inline as a guarded call prefix (`ir_lower`'s
    // `try_emit_ir_string_search_prefix`), so admitting them no longer trades
    // an inline body for a plain call. On BOTH arms of
    // `CRATONVM_JIT_IR_STRING_CALL_ADMIT`: that switch governs the members
    // with no intrinsic in either tier, a different population.
    if ir_string_search_prefix_handles(class, name, descriptor) {
        return false;
    }
    // JIT r21 wave 1, lane strings (STRINGS21-1,
    // `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT`): the package-private
    // `String(byte[], byte)` constructor every LATIN1 / UTF16 result of the
    // JDK's String code is built with -- no single-pass intrinsic, so an
    // ordinary statically bound call loses nothing. (`coder()` / `isLatin1()`
    // are admitted above, as prefix kinds.)
    if name == "<init>" && descriptor == "([BB)V" && ir_string_internal_admit_enabled() {
        return false;
    }
    if !ir_string_call_admit_enabled() {
        return true;
    }
    try_resolve_string_intrinsic(class, name, descriptor, Some(STRING_INTRINSIC_NAME_PROBE))
        .is_some()
}

/// nio20 wave 4, lane L5 (`r20-string-call-admit-heapcb-regression-20261007`):
/// [`string_site_bails_ir_planning`] asked for a site of a method declared on
/// `caller_class`. `CRATONVM_JIT_IR_STRING_CALL_ADMIT=callers` is the narrowed
/// third arm of that A/B: the admit applies to every caller EXCEPT the String
/// implementation classes themselves (`java/lang/String`, `StringLatin1`,
/// `StringUTF16`), which keep the default refusal -- so `HeapCharBuffer.put`
/// moves to the optimizing tier while `String.getChars` (the other method the
/// `heapcb` row moved, and the one whose body is a call into the `inflate`
/// native) stays where it was. Any other truthy value is the full admit.

pub(crate) fn string_implementation_class(class: &str) -> bool {
    matches!(
        class,
        "java/lang/String" | "java/lang/StringLatin1" | "java/lang/StringUTF16"
    )
}

/// Static String calls with no emitted intrinsic must not suppress a caller's
/// optimizing body merely because its cold error path formats a message.
/// Keep the existing String implementation boundary used by the callers arm.
pub(crate) fn ir_ordinary_static_string_call_admits(
    caller: &str,
    class: &str,
    name: &str,
    desc: &str,
) -> bool {
    class == "java/lang/String"
        && !string_implementation_class(caller)
        && try_resolve_string_intrinsic(class, name, desc, Some(STRING_INTRINSIC_NAME_PROBE))
            .is_none()
}

pub(crate) fn string_site_bails_ir_planning_in(
    caller_class: &str,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    let callers_only = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("callers"));
    if callers_only
        && string_implementation_class(caller_class)
        && class == "java/lang/String"
        && !ir_string_access_expander_handles(class, name, descriptor)
        && !ir_string_search_prefix_handles(class, name, descriptor)
    {
        return true;
    }
    string_site_bails_ir_planning(class, name, descriptor)
}

/// nio20 wave 3, lane L2 (the residual of NIO19-8): which of the five
/// `java/lang/String` members the single-pass backend intrinsifies
/// (`StringHashCode`, `StringEquals`, `StringCompareTo`, `StringIndexOfChar`,
/// `StringIndexOfStr`) a site is, for the optimizing tier's inline search
/// prefix (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`).
///
/// The identification is [`try_resolve_string_intrinsic`]'s, CALLED and not
/// re-listed -- the discipline [`ir_string_access_expander_handles`] keeps --
/// so the prefix can never claim a triple the single-pass region would not
/// have intrinsified. `None` under `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX=0`.
///
/// Also (proposal NIO20-L3-4, the same lane) the three GUARDED accessors --
/// `length()`, `isEmpty()`, `charAt(int)` declared on
/// `java/lang/CharSequence` -- which the IR expander refuses
/// (`SI_GUARDED_REFUSED`: no IR node reads a header class id). The prefix
/// makes that compare in machine code and DECLINES to the interface call on a
/// miss, so a non-String receiver costs one compare, never a deopt.
/// `CRATONVM_JIT_IR_CHARSEQ_PREFIX=0` withdraws these three alone.
pub fn ir_string_search_prefix_kind(
    class: &str,
    name: &str,
    descriptor: &str,
) -> Option<IrStringSearchKind> {
    // nio20 wave 4, lane L5 (proposal W3-L2-4): three `java/lang/StringBuilder`
    // members -- `length()`, `charAt(int)`, `append(char)` -- answered by the
    // same prefix over the builder layout, declining to the call on any miss
    // (a full LATIN1 payload, a UTF16 builder for `append`, an index out of
    // range). Identified through the resolver's STRINGBUILDER_ACCESS rows
    // (asked with a probe builder layout), never re-listed.
    // `CRATONVM_JIT_IR_SB_PREFIX=0` withdraws these three alone.
    if class == "java/lang/AbstractStringBuilder" && name == "setLength" && descriptor == "(I)V" {
        return (ir_string_search_prefix_enabled() && ir_sb_prefix_enabled())
            .then_some(IrStringSearchKind::SbSetLength);
    }
    if class == "java/lang/StringBuilder" {
        // JIT r20 wave 19, lane zCp (`R20PIRCodePoint` cp-append):
        // `appendCodePoint(int)` -- not a STRINGBUILDER_ACCESS row (so no
        // pin follows it), and outside the opt-in `CRATONVM_JIT_IR_SB_PREFIX`;
        // its own switch, `CRATONVM_JIT_IR_SB_APPEND_CODE_POINT_PREFIX`.
        // JIT r23 wave 2, lane L5 (R23W1L5-3,
        // `CRATONVM_JIT_IR_SB_CODE_POINT_AT_PREFIX`): `codePointAt(int)`,
        // named outside the opt-in `CRATONVM_JIT_IR_SB_PREFIX` like
        // `appendCodePoint`, so the single-pass row of the same site does not
        // pin its method.
        if name == "codePointAt" && descriptor == "(I)I" {
            return (ir_string_search_prefix_enabled() && ir_sb_code_point_at_prefix_enabled())
                .then_some(IrStringSearchKind::SbCodePointAt);
        }
        if name == "appendCodePoint" && descriptor == "(I)Ljava/lang/StringBuilder;" {
            return (ir_string_search_prefix_enabled() && ir_sb_append_code_point_prefix_enabled())
                .then_some(IrStringSearchKind::SbAppendCodePoint);
        }
        if name == "setLength" && descriptor == "(I)V" {
            return (ir_string_search_prefix_enabled() && ir_sb_prefix_enabled())
                .then_some(IrStringSearchKind::SbSetLength);
        }
        // JIT r21 wave 3, lane strings (STRINGS21-3,
        // `CRATONVM_JIT_IR_SB_LENGTH_PREFIX`): `length()` alone -- a `count`
        // read -- outside the opt-in `CRATONVM_JIT_IR_SB_PREFIX`, so a
        // builder loop whose only STRINGBUILDER_ACCESS site is `length()`
        // (`R21StringCodePoint.append`: `setLength`, `appendCodePoint`,
        // `length`) is released from the single-pass pin and reaches the
        // `appendCodePoint` / `codePointAt` prefixes. Same resolver row as
        // below, never re-listed.
        if name == "length"
            && descriptor == "()I"
            && ir_string_search_prefix_enabled()
            && ir_sb_length_prefix_enabled()
        {
            let (entry, _num_params, _ret, guard_class_id) = try_resolve_string_intrinsic(
                class,
                name,
                descriptor,
                Some(STRING_BUILDER_INTRINSIC_NAME_PROBE),
            )?;
            return (guard_class_id != 0 && entry == JitIntrinsic::StringBuilderLength.as_entry())
                .then_some(IrStringSearchKind::SbLength);
        }
        if !ir_string_search_prefix_enabled() || !ir_sb_prefix_enabled() {
            return None;
        }
        let (entry, _num_params, _ret, guard_class_id) = try_resolve_string_intrinsic(
            class,
            name,
            descriptor,
            Some(STRING_BUILDER_INTRINSIC_NAME_PROBE),
        )?;
        if guard_class_id == 0 {
            return None;
        }
        return if entry == JitIntrinsic::StringBuilderLength.as_entry() {
            Some(IrStringSearchKind::SbLength)
        } else if entry == JitIntrinsic::StringBuilderCharAt.as_entry() {
            Some(IrStringSearchKind::SbCharAt)
        } else if entry == JitIntrinsic::StringBuilderAppendChar.as_entry() {
            Some(IrStringSearchKind::SbAppendChar)
        } else if entry == JitIntrinsic::StringBuilderAppendString.as_entry()
            && ir_sb_append_string_prefix_enabled()
        {
            // nio20 wave 5, lane L5 (W4-L5-2, first member).
            Some(IrStringSearchKind::SbAppendString)
        } else if entry == JitIntrinsic::StringBuilderAppendInt.as_entry()
            && ir_sb_append_int_prefix_enabled()
        {
            // JIT r20 wave 18, lane xSb (STR20-2): the digit writer.
            Some(IrStringSearchKind::SbAppendInt)
        } else if entry == JitIntrinsic::StringBuilderToString.as_entry()
            && ir_sb_to_string_prefix_enabled()
        {
            // JIT r20 wave 18, lane xSb (STR20-2): the allocating LATIN1
            // copy. The resolver names it only under
            // `CRATONVM_JIT_SB_TOSTRING_INLINE` (default ON), and only for a
            // layout with a `coder`.
            Some(IrStringSearchKind::SbToString)
        } else {
            None
        };
    }
    // nio20 wave 6, lane L2: `StringLatin1.inflate([BI[CII)V`, the static the
    // LATIN1 `getChars` ends in (a registered native; no tier intrinsifies
    // it). Same switch as `getChars`; its own kind, because it is a static
    // site with no receiver and no String layout to read.
    if class == "java/lang/StringLatin1" {
        // JIT r21 wave 3, lane strings: the two LATIN1 workers the methods
        // `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT` moves to this tier call
        // (`CaseInsensitiveComparator.compare`, `String.toLowerCase(Locale)`),
        // answered inline so the admitted body does not lose to the
        // single-pass one (wave-1 A/B: cmpci-order 740 -> 1257, lower-latin1
        // 1373 -> 2338 with the bodies alone).
        if ir_string_search_prefix_enabled() && ir_string_internal_admit_enabled() {
            match (name, descriptor) {
                ("compareToCI", "([B[B)I") => return Some(IrStringSearchKind::Latin1CompareToCI),
                ("toLowerCase", "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;") => {
                    return Some(IrStringSearchKind::Latin1ToLowerCase)
                }
                _ => {}
            }
        }

        // JIT round 21, lane strings (ZCP-4, `R20PNatLatin1`
        // latin1-compareToCI): the static `compareToCI([B[B)I` the
        // case-insensitive comparator and `compareToIgnoreCase` end in for two
        // LATIN1 strings. `CRATONVM_JIT_IR_LATIN1_COMPARE_TO_CI_PREFIX=0`
        // withdraws it.
        if name == "compareToCI" && descriptor == "([B[B)I" {
            return (ir_string_search_prefix_enabled() && ir_latin1_compare_to_ci_prefix_enabled())
                .then_some(IrStringSearchKind::Latin1CompareToCI);
        }
        // JIT round 21, lane strings2 (STRINGS21-1, `R20PNatLatin1`
        // latin1-toLowerCase, `R21StringsLatin1Case` lower/upper-mixed): the
        // statics `String.toLowerCase(Locale)` / `toUpperCase(Locale)` end in
        // for a LATIN1 receiver. `CRATONVM_JIT_IR_LATIN1_CASE_MAP_PREFIX=0`
        // withdraws both.
        if matches!(name, "toLowerCase" | "toUpperCase")
            && descriptor == "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;"
        {
            let kind = if name == "toLowerCase" {
                IrStringSearchKind::Latin1ToLowerCase
            } else {
                IrStringSearchKind::Latin1ToUpperCase
            };
            return (ir_string_search_prefix_enabled() && ir_latin1_case_map_prefix_enabled())
                .then_some(kind);
        }
        // JIT round 21, lane str21b (STRINGS2-21-4): the static
        // `regionMatchesCI([BI[BII)Z` `String.regionMatches(true, ..)` ends in
        // for two LATIN1 strings. Opt-in
        // (`CRATONVM_JIT_IR_LATIN1_REGION_MATCHES_CI_PREFIX=1`).
        if name == "regionMatchesCI" && descriptor == "([BI[BII)Z" {
            return (ir_string_search_prefix_enabled()
                && ir_latin1_region_matches_ci_prefix_enabled())
            .then_some(IrStringSearchKind::Latin1RegionMatchesCI);
        }
        if name == "replace" && descriptor == "([BCC)Ljava/lang/String;" {
            return ir_string_search_prefix_enabled()
                .then_some(IrStringSearchKind::Latin1ReplaceChar);
        }
        return (name == "inflate"
            && descriptor == "([BI[CII)V"
            && ir_string_search_prefix_enabled()
            && ir_string_getchars_prefix_enabled())
        .then_some(IrStringSearchKind::Inflate);
    }
    // JIT r20 wave 19, lane zCp (`R20PIRCodePoint` cp-tochars): the static
    // `Character.toChars(int)` -- a fresh `char[1]` / `char[2]` bumped inline
    // for a valid code point, the call (which throws) otherwise. No String
    // layout read; the allocation plan is the `char[]` copies'.
    // `CRATONVM_JIT_IR_CHARACTER_TO_CHARS_PREFIX=0` withdraws it.
    // JIT r23 wave 2b, lane L5 (INTRINSICS22-5, inline-prefix half;
    // `CRATONVM_JIT_IR_ARRAYS_EQUALS_PREFIX`): the static integral
    // `Arrays.equals` overloads, answered inline (identity, null, length, the
    // 16-byte compare loop) instead of a call or a splice of the JDK body
    // (`ArraysSupport.mismatch` -> `vectorizedMismatch`). Only where the
    // planning gate admits the site (`ir_arrays_equals_call_admits`).
    if class == "java/util/Arrays" {
        // Round rx27 wave 3, lane splitfast
        // (`CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX`, opt-in): the static
        // `copyOfRange(byte[], int, int)`, a fresh `byte[]` bumped inline.
        if name == "copyOfRange" && descriptor == "([BII)[B" {
            return ir_arrays_copy_of_range_prefix_enabled()
                .then_some(IrStringSearchKind::ArraysCopyOfRangeBytes);
        }
        return (ir_string_search_prefix_enabled()
            && ir_arrays_equals_prefix_enabled()
            && ir_arrays_equals_call_admits(class, name, descriptor))
        .then_some(IrStringSearchKind::ArraysEquals);
    }
    if class == "java/lang/Character" {
        return (name == "toChars"
            && descriptor == "(I)[C"
            && ir_string_search_prefix_enabled()
            && ir_character_to_chars_prefix_enabled())
        .then_some(IrStringSearchKind::CharacterToChars);
    }
    // JIT r20 wave 18, lane xUtf8 (STR20-6): the two `StringCoding`
    // `@IntrinsicCandidate` leaves the UTF-8 / ASCII / Latin-1 coders' ASCII
    // runs end in (no tier intrinsified them; HotSpot has stubs for both).
    // Static sites, no String layout read. `CRATONVM_JIT_IR_STRING_CODING_PREFIX=0`
    // withdraws both.
    if class == "java/lang/StringCoding" {
        if !ir_string_search_prefix_enabled() || !ir_string_coding_prefix_enabled() {
            return None;
        }
        return match (name, descriptor) {
            ("countPositives", "([BII)I") => Some(IrStringSearchKind::CountPositives),
            ("implEncodeAsciiArray", "([CI[BII)I") => Some(IrStringSearchKind::EncodeAsciiArray),
            _ => None,
        };
    }
    let is_charseq = class == "java/lang/CharSequence";
    if (class != "java/lang/String" && !is_charseq) || !ir_string_search_prefix_enabled() {
        return None;
    }
    if is_charseq
        && false
    {
        return None;
    }
    // Round rx27, lane splitfast: `substring(int, int)` and the two
    // `subSequence(int, int)` forms (JDK bytecode: `checkBoundsBeginEnd`, then
    // `StringLatin1.newString` -> `Arrays.copyOfRange` + `new String`), named
    // here like `indexOf(II)`, each form behind its own opt-in switch; unset,
    // the site falls through to the resolver below, which names none of them.
    if name == "substring" || name == "subSequence" {
        let kind = match (is_charseq, name, descriptor) {
            (false, "substring", "(II)Ljava/lang/String;")
            | (false, "subSequence", "(II)Ljava/lang/CharSequence;")
                if ir_string_substring_prefix_enabled() =>
            {
                Some(IrStringSearchKind::Substring)
            }
            (true, "subSequence", "(II)Ljava/lang/CharSequence;")
                if ir_seq_subsequence_prefix_enabled() =>
            {
                Some(IrStringSearchKind::SeqSubSequence)
            }
            _ => None,
        };
        if kind.is_some() {
            return kind;
        }
    }
    // nio20 wave 6, lane L2 (`heapcb`): `String.getChars(int,int,char[],int)`,
    // which NO tier intrinsifies -- so it is named here, not through the
    // resolver. The optimizing tier answers a LATIN1 receiver inline (the
    // bounds checks, then the LATIN1 -> UTF-16 inflate the JDK body reaches
    // through `StringLatin1.inflate`, a native crossing), declining to the
    // call on anything else. Naming it lifts the invoke-planning gate for the
    // site (`string_site_bails_ir_planning`), which is what moves
    // `HeapCharBuffer.put(String,int,int)` to the optimizing tier, and keeps
    // it a call in the splice planner (`ir_string_prefix_site_stays_a_call`).
    // `CRATONVM_JIT_IR_STRING_GETCHARS_PREFIX=0` withdraws all three.
    if !is_charseq
        && name == "getChars"
        && descriptor == "(II[CI)V"
        && ir_string_getchars_prefix_enabled()
    {
        return Some(IrStringSearchKind::GetChars);
    }
    // JIT r21 wave 1, lane strings (CP21-1, `R20PIRCodePoint` cp-iterate):
    // `String.codePointAt(int)`, which no tier intrinsifies (its registered
    // native is a funnel crossing per call), so it is named here, not through
    // the resolver. Naming it lifts the invoke-planning gate for the site
    // (`string_site_bails_ir_planning`: a code-point loop no longer stays on
    // the single-pass backend for it) and keeps it a call in the splice
    // planner. Opt-in: `CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX=1`.
    if !is_charseq
        && name == "codePointAt"
        && descriptor == "(I)I"
        && ir_string_code_point_at_prefix_enabled()
    {
        return Some(IrStringSearchKind::CodePointAt);
    }
    // JIT r23 wave 1, lane L5 (STRINGS21-4): `codePointBefore(int)` (JDK
    // bytecode, no tier intrinsified it) and `codePointCount(int, int)` (a
    // registered `Intrinsic`: a funnel crossing per call), named here like
    // `codePointAt`, so a reverse code-point walk or a length-in-code-points
    // check no longer bails its method at the planning gate. Each behind its
    // own switch.
    if !is_charseq && descriptor == "(I)I" && name == "codePointBefore" {
        return ir_string_code_point_before_prefix_enabled()
            .then_some(IrStringSearchKind::CodePointBefore);
    }
    if !is_charseq && descriptor == "(II)I" && name == "codePointCount" {
        return ir_string_code_point_count_prefix_enabled()
            .then_some(IrStringSearchKind::CodePointCount);
    }
    // JIT j27 wave 1, lane STRRX: `indexOf(int, int)` (JDK bytecode, no tier
    // intrinsifies it), named like `codePointCount`; unset, the site falls
    // through to the resolver below, which names no `(II)I` member.
    if !is_charseq
        && name == "indexOf"
        && descriptor == "(II)I"
        && ir_string_indexof_from_prefix_enabled()
    {
        return Some(IrStringSearchKind::IndexOfCharFrom);
    }
    // JIT r21 wave 1, lane strings: STRINGS21-1 (`coder()` / `isLatin1()`,
    // the String-internal accessors whose sites kept the JDK's own String
    // code on the single-pass backend), STRINGS21-2 (`compareToIgnoreCase`)
    // and ZCP-3's non-allocating half (`toLowerCase` of an already-lower
    // LATIN1 string). Name-based, each behind its own opt-in switch.
    if !is_charseq {
        match (name, descriptor) {
            ("coder", "()B") if ir_string_internal_admit_enabled() => {
                return Some(IrStringSearchKind::StrCoder)
            }
            ("isLatin1", "()Z") if ir_string_internal_admit_enabled() => {
                return Some(IrStringSearchKind::StrIsLatin1)
            }
            ("compareToIgnoreCase", "(Ljava/lang/String;)I")
                if ir_string_compare_to_ci_prefix_enabled() =>
            {
                return Some(IrStringSearchKind::CompareToIgnoreCase)
            }
            ("toLowerCase", "(Ljava/util/Locale;)Ljava/lang/String;")
                if ir_string_lower_unchanged_prefix_enabled() =>
            {
                return Some(IrStringSearchKind::LowerLocaleUnchanged)
            }
            ("toLowerCase", "()Ljava/lang/String;") if ir_string_lower_unchanged_prefix_enabled() => {
                return Some(IrStringSearchKind::LowerUnchanged)
            }
            ("replace", "(CC)Ljava/lang/String;") => {
                return Some(IrStringSearchKind::ReplaceChar)
            }
            (
                "replace",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
            ) => return Some(IrStringSearchKind::ReplaceCharSeq),
            ("concat", "(Ljava/lang/String;)Ljava/lang/String;") => {
                return Some(IrStringSearchKind::StringConcat)
            }
            _ => {}
        }
    }

    // JIT r20 wave 12, lane qStr (book `jit-r20-nio-proposals-w6-L2.md`,
    // W6-L2-2): the three `char[]` copy members no tier intrinsified --
    // `toCharArray()`, the static `valueOf(char[])` / `copyValueOf(char[])`,
    // and `new String(char[])` at its `invokespecial` -- answered by an inline
    // TLAB allocation plus the inflate / compress copy, declining to the call
    // on anything else. Name-based, like `getChars`; naming them lifts the
    // planning gate and keeps them calls in the splice planner.
    // `CRATONVM_JIT_IR_STRING_CHARS_PREFIX=0` withdraws all three.
    if !is_charseq && ir_string_chars_prefix_enabled() {
        match (name, descriptor) {
            ("toCharArray", "()[C") => return Some(IrStringSearchKind::ToCharArray),
            ("valueOf" | "copyValueOf", "([C)Ljava/lang/String;") => {
                return Some(IrStringSearchKind::CharsValueOf)
            }
            ("<init>", "([C)V") => return Some(IrStringSearchKind::CharsInit),
            _ => {}
        }
    }
    let (entry, _num_params, _ret, guard_class_id) =
        try_resolve_string_intrinsic(class, name, descriptor, Some(STRING_INTRINSIC_NAME_PROBE))?;
    if is_charseq {
        // The probe's non-zero class id makes the resolver answer a guarded
        // site; the real guard id is the compile's layout's, read by the
        // prefix.
        if guard_class_id == 0 {
            return None;
        }
        return if entry == JitIntrinsic::StringLength.as_entry() {
            Some(IrStringSearchKind::SeqLength)
        } else if entry == JitIntrinsic::StringIsEmpty.as_entry() {
            Some(IrStringSearchKind::SeqIsEmpty)
        } else if entry == JitIntrinsic::StringCharAt.as_entry() {
            Some(IrStringSearchKind::SeqCharAt)
        } else {
            None
        };
    }
    if guard_class_id != 0 {
        return None;
    }
    if entry == JitIntrinsic::StringHashCode.as_entry() {
        Some(IrStringSearchKind::HashCode)
    } else if entry == JitIntrinsic::StringEquals.as_entry() {
        Some(IrStringSearchKind::Equals)
    } else if entry == JitIntrinsic::StringCompareTo.as_entry() {
        Some(IrStringSearchKind::CompareTo)
    } else if entry == JitIntrinsic::StringIndexOfChar.as_entry() {
        Some(IrStringSearchKind::IndexOfChar)
    } else if entry == JitIntrinsic::StringIndexOfStr.as_entry() {
        Some(IrStringSearchKind::IndexOfStr)
    } else if !ir_string_accessor_call_prefix_enabled() {
        None
    } else if entry == JitIntrinsic::StringLength.as_entry() {
        // JIT round 21, lane strintr2 (STRINTR21-1): the three accessors, for
        // an `Op::Call` the accessor expander left behind only.
        Some(IrStringSearchKind::StrLength)
    } else if entry == JitIntrinsic::StringIsEmpty.as_entry() {
        Some(IrStringSearchKind::StrIsEmpty)
    } else if entry == JitIntrinsic::StringCharAt.as_entry() {
        Some(IrStringSearchKind::StrCharAt)
    } else {
        None
    }
}

/// The String / CharSequence members [`ir_string_search_prefix_kind`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrStringSearchKind {
    /// `hashCode()I`: the cached non-zero `hash`, else the call.
    HashCode,
    /// `equals(Object)Z`: equal coders compare bytes; unequal coders answer false.
    Equals,
    /// `compareTo(String)I`: two LATIN1 strings, else the call.
    CompareTo,
    /// `indexOf(I)I`: a LATIN1 receiver (any needle), else the call.
    IndexOfChar,
    /// `indexOf(String)I`: all four coder pairs, else malformed data calls.
    IndexOfStr,
    /// `CharSequence.length()I` on a receiver whose header says String.
    SeqLength,
    /// `CharSequence.isEmpty()Z` on a receiver whose header says String.
    SeqIsEmpty,
    /// `CharSequence.charAt(I)C` on a receiver whose header says String and
    /// an in-range index.
    SeqCharAt,
    /// `StringBuilder.length()I`: `count` (nio20 wave 4, L5).
    SbLength,
    /// `StringBuilder.charAt(I)C`: an index below `count`, either coder.
    SbCharAt,
    /// `StringBuilder.append(C)`: a LATIN1 builder with room and a LATIN1
    /// char; returns the receiver.
    SbAppendChar,
    /// `StringBuilder.append(String)`: a non-null LATIN1 String into a LATIN1
    /// builder with room for all of it; returns the receiver (nio20 wave 5,
    /// L5).
    SbAppendString,
    /// `StringBuilder.append(I)` (JIT r20 wave 18, lane xSb, STR20-2): a
    /// LATIN1 builder whose array already holds `count + stringSize(i)`
    /// bytes gets the decimal digits written in place (`DecimalDigits
    /// .uncheckedGetCharsLatin1`'s text, `Integer.MIN_VALUE` and negatives
    /// included) and `count` advanced; returns the receiver. A UTF16 builder,
    /// a growth, a missing payload decline to the call.
    SbAppendInt,
    /// `StringBuilder.toString()` (JIT r20 wave 18, lane xSb, STR20-2): a
    /// LATIN1 builder with `0 < count <= value.length` becomes a FRESH LATIN1
    /// String (one TLAB bump for the `byte[count]` and the String, as the
    /// `char[]` copy prefixes allocate). A UTF16 or empty builder, compact
    /// strings off, a full TLAB or a collector pre-check miss decline to the
    /// call.
    SbToString,
    /// `String.getChars(II[CI)V` (nio20 wave 6, L2): a LATIN1 receiver with
    /// every bound in range is inflated into the `char[]` inline; anything
    /// else (null, UTF16, a bound the JDK body throws on) is the call. Void.
    /// Emitted by `ir_lower::Lowerer::try_emit_ir_string_getchars_prefix`.
    GetChars,
    /// `StringLatin1.inflate([BI[CII)V` (nio20 wave 6, L2), a STATIC site: a
    /// non-null `byte[]` / `char[]` pair with every bound in range is copied
    /// inline; anything else (including the JDK loop's no-op and partial-copy
    /// cases) is the call. Void. `ir_lower::Lowerer::try_emit_ir_string_inflate_prefix`.
    Inflate,
    /// `String.toCharArray()[C` (JIT r20 wave 12, lane qStr, W6-L2-2): a fresh
    /// `char[length()]` bumped inline and filled from either coder (LATIN1
    /// inflated, UTF16 copied); a null / non-String receiver, a null or
    /// non-`byte[]` value, an odd UTF16 length, a full TLAB or a collector
    /// pre-check miss is the call. `ir_lower::Lowerer::try_emit_ir_string_chars_prefix`.
    ToCharArray,
    /// The STATIC `String.valueOf([C)` / `copyValueOf([C)` (same lane): a
    /// non-null, non-empty `char[]` becomes a fresh String in one bump (the
    /// `byte[]` and the String, as the inline concat allocates), LATIN1 when
    /// `COMPACT_STRINGS` and every char fits, else UTF16 -- the coder
    /// `StringUTF16.compress` + `coderFromArrayLen` choose. `num_params` 0:
    /// the array stands where a receiver would.
    CharsValueOf,
    /// `String.<init>([C)V` at its `invokespecial` (same lane): the receiver,
    /// a COMPACT String whose `value` is still null, gets a fresh `byte[]`
    /// (one bump) and its `coder`; the same coder rule. Void.
    CharsInit,
    /// The STATIC `StringCoding.countPositives([BII)I` (JIT r20 wave 18, lane
    /// xUtf8, STR20-6): a non-null `byte[]` with `off` / `len` in range is
    /// scanned inline (16 bytes per SSE2 step) for the first negative byte;
    /// anything else (including the JDK loop's null / no-op and throwing
    /// cases) is the call. `ir_lower::Lowerer::try_emit_ir_string_coding_prefix`.
    CountPositives,
    /// The STATIC `StringCoding.implEncodeAsciiArray([CI[BII)I` (same lane):
    /// a non-null `char[]` / `byte[]` pair with every bound in range is copied
    /// inline up to the first char `>= 0x80` (8 chars per SSE2 step), storing
    /// nothing at or past it; anything else is the call.
    EncodeAsciiArray,
    /// `StringBuilder.appendCodePoint(I)` (JIT r20 wave 19, lane zCp): a
    /// LATIN1 builder with room and a code point in `0..=0xFF` gets one byte;
    /// a UTF16 builder with room gets one char (a BMP code point) or the
    /// surrogate pair (a supplementary one); `count` advanced, the receiver
    /// returned. A growth, an inflation, an invalid code point (the call
    /// throws) and a missing payload decline to the call before any store.
    SbAppendCodePoint,
    /// The STATIC integral `Arrays.equals([X[X)Z` (JIT r23 wave 2b, lane L5,
    /// INTRINSICS22-5): `a == b` true; a null side false; unequal lengths
    /// false; else the byte compare of `length << shift` bytes. A non-array of
    /// the declared element type or an oversized byte count is the call.
    /// `num_params` 1: the first array stands where a receiver would.
    ArraysEquals,
    /// `StringBuilder.codePointAt(I)I` (JIT r23 wave 2, lane L5, R23W1L5-3):
    /// `0 <= index < count` (and inside the payload) answers LATIN1
    /// `value[index] & 0xff`, or UTF16 `StringUTF16.codePointAtSB(value,
    /// index, count)` -- the unit, or the pair when a high surrogate is
    /// followed inside `count` by a low one. Anything else is the call.
    SbCodePointAt,
    /// `StringBuilder.setLength(I)V` / `AbstractStringBuilder.setLength(I)V`:
    /// `0 <= newLength <= count` sets `count = newLength` inline. Anything
    /// else (negative, growth, zero-fill) declines to the call.
    SbSetLength,
    /// The STATIC `Character.toChars(I)[C` (same lane): a valid code point
    /// gets a fresh `char[1]` (BMP) or `char[2]` (its surrogate pair) bumped
    /// inline; an invalid one, a full TLAB or a collector pre-check miss is
    /// the call (which raises the `IllegalArgumentException`). `num_params`
    /// 0: the code point stands where a receiver would.
    CharacterToChars,
    /// `String.codePointAt(I)I` (JIT r21 wave 1, lane strings, CP21-1): a
    /// LATIN1 receiver with `0 <= index < value.length` answers the byte; a
    /// UTF16 one with `0 <= index < value.length >> 1` the code unit, or the
    /// supplementary code point when it is a high surrogate followed (inside
    /// the string) by a low one -- `StringUTF16.codePointAt`'s answer, an
    /// unpaired surrogate included. A null receiver and an index out of range
    /// decline to the call (which throws the JDK's
    /// `StringIndexOutOfBoundsException`).
    CodePointAt,
    /// `String.codePointBefore(I)I` (JIT r23 wave 1, lane L5, STRINGS21-4):
    /// `i = index - 1` inside `[0, length)` answers LATIN1 `value[i] & 0xff`,
    /// or UTF16 `StringUTF16.codePointBefore(value, index)` -- the unit at
    /// `i`, or the supplementary code point when it is a low surrogate whose
    /// predecessor (inside the string) is a high one. A null receiver and an
    /// index outside `[1, length]` decline to the call (its SIOOBE names
    /// `index - 1`).
    CodePointBefore,
    /// `String.codePointCount(II)I` (same lane and item): `0 <= begin <= end
    /// <= length` answers LATIN1 `end - begin`, or UTF16 `end - begin` less
    /// one per high/low pair inside the range (`StringUTF16.codePointCount`).
    /// A null receiver and any other range decline to the call (its plain
    /// `IndexOutOfBoundsException: Range [b, e) out of bounds for length n`).
    CodePointCount,
    /// The package-private `String.coder()B` (JIT r21 wave 1, lane strings,
    /// STRINGS21-1): `COMPACT_STRINGS ? coder : UTF16`, read from this VM's
    /// compact-strings flag byte and the receiver's `coder` field. A null
    /// receiver, or no flag address at compile time, is the call.
    StrCoder,
    /// The private `String.isLatin1()Z` (same item): `COMPACT_STRINGS &&
    /// coder == LATIN1`, the same two reads.
    StrIsLatin1,
    /// `String.compareToIgnoreCase(String)I` (same lane, STRINGS21-2 / ZCP-4):
    /// two LATIN1 operands answered by the JDK's `StringLatin1.compareToCI`
    /// loop, whose two-step fold over Latin-1 is ONE map, `fold(b) =
    /// Character.toLowerCase(CharacterDataLatin1.toUpperCase(b))` (the
    /// identity except `A-Z` and `0xC0..=0xDE` without `0xD7`, `+0x20`, and
    /// `0xB5` -> `0x3BC`): the first `k` with `fold(a[k]) != fold(b[k])`
    /// answers their difference, else `len1 - len2`. A null argument, a
    /// UTF16 or mixed pair is the call.
    CompareToIgnoreCase,
    /// `String.toLowerCase(Locale)` (same lane, ZCP-3): a LATIN1 receiver
    /// with a non-null locale and no `CharacterDataLatin1.isUpperCase` byte
    /// (`A-Z`, `0xC0..=0xD6`, `0xD8..=0xDE`) answers the receiver itself, as
    /// `StringLatin1.toLowerCase`'s `return str` does before it reads the
    /// locale's language; a changed one answers the fresh mapped String
    /// (JIT r21 wave 3) as [`Self::Latin1ToLowerCase`] does. A UTF16
    /// receiver or a null locale is the call.
    LowerLocaleUnchanged,
    /// `String.toLowerCase()` (same): `toLowerCase(Locale.getDefault())`,
    /// whose locale is never null -- the same scan, no locale operand.
    LowerUnchanged,
    /// The STATIC `StringLatin1.compareToCI([B[B)I` (JIT r21 wave 3, lane
    /// strings, under `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT`): the worker the
    /// JDK's `CaseInsensitiveComparator.compare` calls for two LATIN1
    /// operands once that method reaches the optimizing tier -- the same
    /// fold loop as [`Self::CompareToIgnoreCase`] over two non-null `byte[]`s
    /// (a null or non-`byte[]` operand is the call). `num_params` 1: the first
    /// array stands where a receiver would.
    Latin1CompareToCI,
    /// The STATIC `StringLatin1.toLowerCase(String, byte[], Locale)` (same
    /// wave and switch): the worker `String.toLowerCase(Locale)` calls for a
    /// LATIN1 receiver. An unchanged `byte[]` answers `str`; a changed one
    /// with no `tr` / `az` / `lt`-sensitive byte (`I`, `J`, `0xCC`, `0xCD`:
    /// the only Latin-1 rows of `ConditionalSpecialCasing`'s lower-case
    /// table) answers a FRESH LATIN1 String of the mapped bytes, the JDK's
    /// non-locale arm; a null locale, a sensitive byte, a full TLAB or a
    /// collector pre-check miss is the call. `num_params` 2 (`str` stands
    /// where a receiver would). Under `CRATONVM_JIT_IR_LATIN1_CASE_MAP_PREFIX`
    /// (JIT round 21, lane strings2; re-landed t1 wave 8) it is lowered by the
    /// case-map prefix instead, whose language screen answers `I` / `0xCC` /
    /// `0xCD` for every language but `tr` / `az` / `lt`.
    Latin1ToLowerCase,
    /// The STATIC `StringLatin1.toUpperCase(String, byte[], Locale)` (JIT
    /// round 21, lane strings2, STRINGS21-1, served by
    /// `ir_lower::Lowerer::try_emit_ir_latin1_case_map_prefix`, which also
    /// serves [`Self::Latin1ToLowerCase`] and, with
    /// `try_emit_ir_latin1_compare_to_ci_prefix`, [`Self::Latin1CompareToCI`]
    /// outside `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT`): the twin of [`Self::Latin1ToLowerCase`], except that `0xB5`,
    /// `0xDF` and `0xFF` (upper-cased out of Latin-1) always decline and the
    /// locale-sensitive byte is `i` (`tr` / `az`).
    Latin1ToUpperCase,
    /// The STATIC `StringLatin1.regionMatchesCI([BI[BII)Z` (JIT round 21, lane
    /// str21b, STRINGS2-21-4; opt-in `CRATONVM_JIT_IR_LATIN1_REGION_MATCHES_CI_PREFIX`):
    /// the worker `String.regionMatches(true, ..)` -- the tail of every
    /// `equalsIgnoreCase` -- calls for two LATIN1 strings. Two non-null
    /// `byte[]`s and an in-range non-negative region answer inline (equal
    /// bytes, or `CharacterDataLatin1.equalsIgnoreCase`'s letter test on the
    /// others); anything else is the call. `num_params` 4 (`value` stands where
    /// a receiver would: `toffset`, `other`, `ooffset`, `len`); the result is
    /// a `boolean`.
    Latin1RegionMatchesCI,
    /// `String.length()I` at a site the accessor expander left a call (JIT
    /// round 21, lane strintr2, STRINTR21-1;
    /// `CRATONVM_JIT_IR_STRING_ACCESSOR_CALL_PREFIX`): [`Self::SeqLength`]'s
    /// arm without the kind-bit test. Never reported by
    /// [`ir_string_search_prefix_handled_kind`].
    StrLength,
    /// `String.isEmpty()Z`, the same way ([`Self::SeqIsEmpty`]'s arm).
    StrIsEmpty,
    /// `String.charAt(I)C`, the same way ([`Self::SeqCharAt`]'s arm, never
    /// the trapping form): an index outside `[0, length)` declines to the
    /// call, which raises the exact `StringIndexOutOfBoundsException`.
    StrCharAt,
    /// `String.indexOf(II)I` (JIT j27 wave 1, lane STRRX;
    /// `CRATONVM_JIT_IR_STRING_INDEXOF_FROM_PREFIX`, default ON): JDK 25's
    /// `fromIndex = Math.max(fromIndex, 0)` then `StringLatin1.indexOf(value,
    /// ch, min(fromIndex, len), len)` / `StringUTF16.indexOf(value, ch,
    /// min(fromIndex, len), len)` answered inline -- the scan
    /// [`Self::IndexOfChar`] runs, from the clamped start; a supplementary
    /// needle scans for its surrogate pair at `i < len - 1`, a negative or
    /// invalid one answers -1. No input throws in the JDK body except a null
    /// receiver, which declines to the call (its NPE). The `String.split(CIZ)`
    /// fast path's per-field `indexOf(ch, off)`. `num_params` 2.
    IndexOfCharFrom,
    /// `String.substring(II)` and `String.subSequence(II)` (round rx27, lane
    /// splitfast; `CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX`, default ON): a LATIN1
    /// receiver with `0 <= begin <= end <= length` answers itself for the
    /// whole range, else a FRESH LATIN1 String of `value[begin..end]` (one TLAB
    /// bump for the `byte[]` and the String). A UTF16 receiver, a range the JDK
    /// body throws for, the empty range (the `""` literal), a full TLAB or a
    /// collector pre-check miss is the call. The `String.split(CIZ)` fast
    /// path's per-field `substring(off, next)`. `num_params` 2.
    Substring,
    /// `CharSequence.subSequence(II)` (same lane;
    /// `CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX`, default ON): [`Self::Substring`]'s
    /// answer on a receiver whose header says String (any other receiver is
    /// the interface call). `Pattern.split` / `Matcher.group`'s
    /// `input.subSequence(..).toString()`. `num_params` 2.
    SeqSubSequence,
    /// The STATIC `Arrays.copyOfRange([BII)[B` (round rx27 wave 3, lane
    /// splitfast; `CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX`, opt-in): a
    /// non-null `byte[]` with `0 <= from <= to <= length` gets a FRESH
    /// `byte[to - from]` of `original[from..to]` bumped inline; every throwing
    /// input, the zero-padded `to > length`, a full TLAB or a collector
    /// pre-check miss is the call. `num_params` 2: `original` stands where a
    /// receiver would.
    ArraysCopyOfRangeBytes,
    /// `String.replace(CC)Ljava/lang/String;`: LATIN1 SSE2 scan + single-bump
    /// allocation + branchless 16-byte replace (`emit_ir_latin1_replace_char`).
    ReplaceChar,
    /// The STATIC `StringLatin1.replace([BCC)Ljava/lang/String;`, answering
    /// `null` when `oldChar > 0xFF` or absent.
    Latin1ReplaceChar,
    /// `String.replace(CharSequence, CharSequence)` for plain 1-byte LATIN1
    /// String target and replacement, declining multi-char / `""` / UTF16 /
    /// non-String / null to the compiled JDK body.
    ReplaceCharSeq,
    /// `String.concat(String)` for two LATIN1 Strings in one TLAB bump.
    StringConcat,
}

impl IrStringSearchKind {
    /// JIT round 21, lane strintr2 (STRINTR21-1): the three `java/lang/String`
    /// accessor kinds that only front an `Op::Call` the accessor expander left
    /// behind. The pin, the planning gate and the splice planners never see
    /// them ([`ir_string_search_prefix_handled_kind`] filters them), so they
    /// move no tier, splice or pin decision.
    pub(crate) fn is_string_accessor_call(self) -> bool {
        matches!(
            self,
            IrStringSearchKind::StrLength
                | IrStringSearchKind::StrIsEmpty
                | IrStringSearchKind::StrCharAt
        )
    }

    /// Java arguments after the receiver. For the static [`Self::Inflate`],
    /// the arguments after the FIRST one (`src` stands where a receiver
    /// would), so `3 + num_params()` is the `Op::Call`'s input count for
    /// every kind.
    pub fn num_params(self) -> usize {
        match self {
            IrStringSearchKind::HashCode
            | IrStringSearchKind::SeqLength
            | IrStringSearchKind::SeqIsEmpty
            | IrStringSearchKind::StrLength
            | IrStringSearchKind::StrIsEmpty
            | IrStringSearchKind::SbLength
            | IrStringSearchKind::SbToString
            | IrStringSearchKind::ToCharArray
            | IrStringSearchKind::CharsValueOf
            | IrStringSearchKind::CharacterToChars
            | IrStringSearchKind::StrCoder
            | IrStringSearchKind::StrIsLatin1
            | IrStringSearchKind::LowerUnchanged => 0,
            // srcBegin, srcEnd, dst, dstBegin / srcOff, dst, dstOff, len.
            IrStringSearchKind::GetChars | IrStringSearchKind::Inflate => 4,
            // off, len (after `ba`) / sp, da, dp, len (after `sa`).
            IrStringSearchKind::CountPositives => 2,
            // beginIndex, endIndex (JIT r23 wave 1, lane L5).
            IrStringSearchKind::CodePointCount => 2,
            // ch, fromIndex (JIT j27 wave 1, lane STRRX).
            IrStringSearchKind::IndexOfCharFrom => 2,
            // beginIndex, endIndex (round rx27, lane splitfast).
            IrStringSearchKind::Substring | IrStringSearchKind::SeqSubSequence => 2,
            // from, to (after `original`).
            IrStringSearchKind::ArraysCopyOfRangeBytes => 2,
            IrStringSearchKind::ReplaceChar
            | IrStringSearchKind::Latin1ReplaceChar
            | IrStringSearchKind::ReplaceCharSeq => 2,
            // value, locale (after `str`).
            IrStringSearchKind::Latin1ToLowerCase | IrStringSearchKind::Latin1ToUpperCase => 2,
            // toffset, other, ooffset, len (after `value`).
            IrStringSearchKind::Latin1RegionMatchesCI => 4,
            IrStringSearchKind::EncodeAsciiArray => 4,
            _ => 1,
        }
    }

    /// A `java/lang/StringBuilder` member (read through the layout's
    /// `builder` half rather than the String one).
    pub fn is_builder(self) -> bool {
        matches!(
            self,
            IrStringSearchKind::SbLength
                | IrStringSearchKind::SbCharAt
                | IrStringSearchKind::SbAppendChar
                | IrStringSearchKind::SbAppendString
                | IrStringSearchKind::SbAppendInt
                | IrStringSearchKind::SbToString
                | IrStringSearchKind::SbAppendCodePoint
                | IrStringSearchKind::SbCodePointAt
                | IrStringSearchKind::SbSetLength
        )
    }

    /// The `JitInvokeInfo::invoke_kind`s (and `invoke*` opcodes) this site can
    /// arrive as: virtual (0) or devirtualised special (1) for the
    /// `java/lang/String` and `java/lang/StringBuilder` members (both classes
    /// are final), interface (2) for the `CharSequence` ones, static (3) for
    /// [`Self::Inflate`].
    pub fn admits_invoke_kind(self, invoke_kind: u8) -> bool {
        match self {
            IrStringSearchKind::SeqLength
            | IrStringSearchKind::SeqIsEmpty
            | IrStringSearchKind::SeqCharAt
            | IrStringSearchKind::SeqSubSequence => invoke_kind == 2,
            IrStringSearchKind::Inflate
            | IrStringSearchKind::CharsValueOf
            | IrStringSearchKind::CountPositives
            | IrStringSearchKind::EncodeAsciiArray
            | IrStringSearchKind::CharacterToChars
            | IrStringSearchKind::Latin1CompareToCI
            | IrStringSearchKind::Latin1ToLowerCase
            | IrStringSearchKind::Latin1ToUpperCase
            | IrStringSearchKind::Latin1RegionMatchesCI
            | IrStringSearchKind::ArraysEquals
            | IrStringSearchKind::ArraysCopyOfRangeBytes
            | IrStringSearchKind::Latin1ReplaceChar => invoke_kind == 3,
            // A constructor arrives only as an `invokespecial`.
            IrStringSearchKind::CharsInit => invoke_kind == 1,
            _ => matches!(invoke_kind, 0 | 1),
        }
    }

    /// JIT r20 wave 12, lane qStr (W6-L2-2): the three kinds that ALLOCATE
    /// (`ir_lower::Lowerer::try_emit_ir_string_chars_prefix`).
    pub(crate) fn is_chars_copy(self) -> bool {
        matches!(
            self,
            IrStringSearchKind::ToCharArray
                | IrStringSearchKind::CharsValueOf
                | IrStringSearchKind::CharsInit
        )
    }

    /// JIT r21 wave 1/3, lane strings: the Latin-1 case kinds with their own
    /// emitter (`ir_lower::Lowerer::try_emit_ir_string_latin1_case_prefix`):
    /// the two `toLowerCase` user sites and the `StringLatin1.toLowerCase` worker.
    pub(crate) fn is_latin1_case(self) -> bool {
        matches!(
            self,
            IrStringSearchKind::LowerLocaleUnchanged
                | IrStringSearchKind::LowerUnchanged
                | IrStringSearchKind::Latin1ToLowerCase
        )
    }

    /// JIT r20 wave 18, lane xUtf8 (STR20-6): the two `java/lang/StringCoding`
    /// kinds (`ir_lower::Lowerer::try_emit_ir_string_coding_prefix`).
    pub(crate) fn is_string_coding(self) -> bool {
        matches!(
            self,
            IrStringSearchKind::CountPositives | IrStringSearchKind::EncodeAsciiArray
        )
    }

    /// The opcode form of [`Self::admits_invoke_kind`] for the pin's scan.
    pub(crate) fn admits_opcode(self, opcode: u8) -> bool {
        match self {
            IrStringSearchKind::SeqLength
            | IrStringSearchKind::SeqIsEmpty
            | IrStringSearchKind::SeqCharAt
            | IrStringSearchKind::SeqSubSequence => opcode == 0xb9,
            IrStringSearchKind::Inflate
            | IrStringSearchKind::CharsValueOf
            | IrStringSearchKind::CountPositives
            | IrStringSearchKind::EncodeAsciiArray
            | IrStringSearchKind::CharacterToChars
            | IrStringSearchKind::Latin1CompareToCI
            | IrStringSearchKind::Latin1ToLowerCase
            | IrStringSearchKind::Latin1ToUpperCase
            | IrStringSearchKind::Latin1RegionMatchesCI
            | IrStringSearchKind::ArraysEquals
            | IrStringSearchKind::ArraysCopyOfRangeBytes
            | IrStringSearchKind::Latin1ReplaceChar => opcode == 0xb8,
            IrStringSearchKind::CharsInit => opcode == 0xb7,
            _ => opcode == 0xb6,
        }
    }
}

/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX` (default ON, `=0` off): the
/// optimizing tier's inline String search prefix, AND the planning-gate / pin
/// carve-outs that let a method with such a site reach it -- one switch, so
/// the arms cannot disagree (`=0` restores both refusals exactly). NOT
/// `OnceLock`-cached: compile time only, as its neighbours.
pub(crate) fn ir_string_search_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_PREFIX` (nio20 wave 4, lane L5; default ON after the
/// j27 host A/B, `=0` off): the `StringBuilder` length / charAt / append(char)
/// prefix and the pin / splice carve-outs for those sites. The measured
/// `append(String)` member is separately opt-in; see
/// [`ir_sb_append_string_prefix_enabled`]. One switch keeps the prefix and its
/// admission rules aligned. Compile time only, not cached.
pub fn ir_sb_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_APPEND_STRING_PREFIX` (nio20 wave 5, lane L5, proposal
/// W4-L5-2's first member; default OFF after the j27 host A/B, `=1` on; inside
/// `CRATONVM_JIT_IR_SB_PREFIX`): `StringBuilder.append(String)` as
/// [`IrStringSearchKind::SbAppendString`] -- a non-null LATIN1 String copied
/// into a LATIN1 builder that already has room, declining to the call
/// otherwise (null, either side UTF16, a growth) -- and with it the pin /
/// splice carve-outs for that site. Unset or `=0` keeps an `append(String)` site
/// pinned to the single-pass backend; `=1` opts into the IR prefix.
/// Compile time only, not cached.
pub(crate) fn ir_sb_append_string_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_APPEND_INT_PREFIX` (JIT r20 wave 18, lane xSb, book
/// `jit-r20-strings-proposals.md` STR20-2; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_SB_PREFIX`): `StringBuilder.append(int)` as
/// [`IrStringSearchKind::SbAppendInt`] -- the decimal digits written in place
/// into a LATIN1 builder that already has room, declining to the call
/// otherwise -- and with it the pin / splice carve-outs for that site. `=0`
/// restores the earlier behaviour exactly (a method with an `append(int)` site
/// pinned to the single-pass backend). Compile time only, not cached.
pub(crate) fn ir_sb_append_int_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_TOSTRING_PREFIX` (JIT r20 wave 18, lane xSb, STR20-2;
/// default ON, `=0` off; inside `CRATONVM_JIT_IR_SB_PREFIX`, and named only
/// while `CRATONVM_JIT_SB_TOSTRING_INLINE` registers the site):
/// `StringBuilder.toString()` as [`IrStringSearchKind::SbToString`] -- a
/// fresh LATIN1 String allocated inline from a LATIN1 builder, declining to
/// the call otherwise -- and with it the pin / splice carve-outs for that
/// site. `=0` restores the earlier behaviour exactly. Compile time only.
pub(crate) fn ir_sb_to_string_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_CHARAT_STAYS_CALL` (JIT r20 wave 18, lane xSb, STR20-1;
/// default ON, `=0` off; only meaningful under `CRATONVM_JIT_IR_SB_PREFIX`):
/// a `StringBuilder.charAt(int)` site stays the CALL its IR prefix
/// ([`IrStringSearchKind::SbCharAt`]) fronts, instead of splicing
/// `AbstractStringBuilder.charAt` (`String.checkIndex` ->
/// `Preconditions.checkIndex(.., SIOOBE_FORMATTER)`, the coder test,
/// `StringUTF16.charAt`), which the single-pass body never pays: it answers
/// the site with the inline STRINGBUILDER_ACCESS load. The static reading of
/// why `Nio20StringIr sbappend` (one `charAt` per op) loses 20 ns/op under
/// the prefix while `sbappendstr` (no `charAt`) loses about 3. `=0` restores
/// the splice. Compile time only.
pub(crate) fn ir_sb_charat_stays_call_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_SSE2` (nio20 wave 4, lane L5; default ON, `=0`
/// off): the String prefix's 16-byte SSE2 steps (`equals` / `compareTo`'s
/// compare loop, `indexOf(int)`'s scan). SSE2 is baseline x86-64, and the
/// lowerer's FP scratch pair XMM0/XMM1 is free at a call prefix. Compile
/// time only, not cached.
pub(crate) fn ir_string_sse2_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_STRING_EQUALS_CODER_FALSE` (JIT round 22 wave 4, lane
/// intrinsics; perf, **opt-in**): the single-pass `String.equals` intrinsic
/// answers `false` for unequal coders (JDK 25's body) instead of deopting on
/// every such call. Compile time only.
pub(crate) fn sp_string_equals_coder_false_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_SB_CHARAT_CAPACITY_CHECK` (JIT round 22 wave 4, lane
/// intrinsics; correctness, **default ON**, `=0` restores the index-below-
/// `count` test alone): the single-pass `StringBuilder.charAt` intrinsic
/// (`x64/objects.rs` `emit_sb_char_at_body`) also declines an index at or past
/// the payload's capacity (`value.length`, halved for UTF16), as the JDK body's
/// array access and the optimizing tier's `SbCharAt` prefix do. Compile time
/// only.
pub(crate) fn sb_char_at_capacity_check_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_FP_NAN_OPERAND_ORDER` (JIT round 22 wave 6b, lane loops,
/// `docs/feature-designs/jit-ir-optimizations.md`; **opt-in**,
/// flipped OFF in wave 7): `=1` makes both tiers (and the IR vector pre-loop)
/// emit `fadd` / `dadd` / `fmul` / `dmul` with value2 in the SSE destination,
/// so an add or multiply of two NaNs answers value2's NaN -- HotSpot 25's
/// INTERPRETER rule. HotSpot C2 answers value1's NaN (value1 in the
/// destination, this switch off), and HotSpot is not tier-consistent here, so
/// compiled code follows C2 by default. The sum and product are the same bits
/// either way for every non-NaN pair. Compile time only (one read per FP binop
/// site).
pub(crate) fn fp_nan_operand_order_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_SB_APPEND_STRING_SSE2` (JIT round 22 wave 2, lane
/// intrinsics, NIO20W5-L5-3 of `jit-r20-nio-proposals-w5-L5.md`; perf,
/// **default on**): the optimizing tier's `StringBuilder.append(String)` prefix
/// (`ir_lower`, `K::SbAppendString`) copies 16 bytes per step before its byte
/// loop. Same bytes. Compile time only.
pub(crate) fn ir_sb_append_string_sse2_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_ARRAYS_EQUALS_SSE2` (JIT round 22 wave 1, lane intrinsics,
/// INTRINSICS21-5; perf, **default on**): the single-pass `Arrays.equals`
/// intrinsic (`JitIntrinsic::ArraysEquals1/2/4/8`) compares the two payloads
/// 16 bytes per step (`MOVDQU` + `PCMPEQB` + `PMOVMSKB`, then one 8-byte step
/// and a byte tail) instead of `REP CMPSB`. The same raw byte compare, so the
/// same answer. Compile time only (read once per intrinsic site).
pub(crate) fn arrays_equals_sse2_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SB_APPEND_INT_PAIRS` (JIT round 22 wave 1, lane intrinsics,
/// INTRINSICS21-3; perf, **default on**): the single-pass
/// `StringBuilder.append(int)` intrinsic renders two digits per step while
/// the value is at least 100 (one reciprocal multiply by 100 on the carried
/// chain per pair) before its one-digit loop. Same bytes. Compile time only.
pub(crate) fn sb_append_int_pairs_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_STRING_INDEXOF_CHAR` (JIT round 22 wave 2, lane
/// intrinsics, INTRINSICS21-2; perf, **default ON (flipped; `=0` kills)**): the single-pass
/// `String.indexOf(int)` intrinsic is registered for a VARIABLE needle too
/// (`string_index_of_char_site_admitted`; a needle outside `0..=0xFFFF`
/// takes the arm's run-time screen to an inline answer of the JDK body --
/// -1, or the surrogate-pair search -- since wave 3, never a deopt), and its scan starts with a
/// 16-byte SSE2 stage (`x64` `emit_sp_index_of_char_sse_stage`). Compile time
/// only.
pub(crate) fn sp_string_index_of_char_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_SP_MATH_ROUND` (JIT round 22 wave 2, lane intrinsics,
/// INTRINSICS22-4; perf, **default ON (flipped; `=0` kills)**): the single-pass backend answers
/// `Math.round` / `StrictMath.round` (both widths) inline
/// (`JitIntrinsic::MathRoundDouble` / `MathRoundFloat`) instead of calling the
/// registered native. Registered only where the optimizing tier claims the
/// row too, i.e. together with `CRATONVM_JIT_IR_MATH_ROUND_SCALAR=1`
/// ([`sp_scalar_row`]). Compile time only.
pub(crate) fn sp_math_round_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_SP_CODE_POINT_SCALAR` (JIT round 22 wave 2, lane intrinsics,
/// L3W6-4 of `jit-r20-nio-proposals-w6-L3.md`; perf, **opt-in**): the
/// single-pass backend answers `Character.isHighSurrogate` / `isLowSurrogate`
/// / `charCount` / `isBmpCodePoint` / `isValidCodePoint` inline instead of a
/// registered-native call, each only where the optimizing tier's own row
/// (`CRATONVM_JIT_IR_CHAR_SURROGATE_SCALAR` / `CRATONVM_JIT_IR_CODE_POINT_SCALAR`,
/// both default ON) claims it ([`sp_scalar_row`]). Compile time only.
pub(crate) fn sp_code_point_scalar_enabled() -> bool {
    false
}

/// The single-pass scalar rows of the SCALAR_ROWS_SP region: `(entry,
/// num_params, return type)` for a triple whose switch is on AND which
/// `ir::try_ir_scalar_intrinsic` claims (so the optimizing planner never
/// refuses a caller for it), else `None`.
pub(crate) fn sp_scalar_row(class: &str, name: &str, descriptor: &str) -> Option<(usize, usize, u8)> {
    let (intrinsic, num_params, ret) = match (class, name, descriptor) {
        ("java/lang/Math" | "java/lang/StrictMath", "round", "(D)J") if sp_math_round_enabled() => {
            (JitIntrinsic::MathRoundDouble, 1, b'J')
        }
        ("java/lang/Math" | "java/lang/StrictMath", "round", "(F)I") if sp_math_round_enabled() => {
            (JitIntrinsic::MathRoundFloat, 1, b'I')
        }
        ("java/lang/Character", "isHighSurrogate", "(C)Z") if sp_code_point_scalar_enabled() => {
            (JitIntrinsic::CharIsHighSurrogate, 1, b'Z')
        }
        ("java/lang/Character", "isLowSurrogate", "(C)Z") if sp_code_point_scalar_enabled() => {
            (JitIntrinsic::CharIsLowSurrogate, 1, b'Z')
        }
        ("java/lang/Character", "charCount", "(I)I") if sp_code_point_scalar_enabled() => {
            (JitIntrinsic::CharCharCount, 1, b'I')
        }
        ("java/lang/Character", "isBmpCodePoint", "(I)Z") if sp_code_point_scalar_enabled() => {
            (JitIntrinsic::CharIsBmpCodePoint, 1, b'Z')
        }
        ("java/lang/Character", "isValidCodePoint", "(I)Z") if sp_code_point_scalar_enabled() => {
            (JitIntrinsic::CharIsValidCodePoint, 1, b'Z')
        }
        ("java/lang/Character", "isISOControl", "(I)Z" | "(C)Z")
            if sp_code_point_scalar_enabled() =>
        {
            (JitIntrinsic::CharIsIsoControl, 1, b'Z')
        }
        _ => {
            if let Some(m) = crate::ir::MathCallOp::match_triple(class, name, descriptor) {
                use crate::ir::MathCallOp as M;
                let intr = match m {
                    M::MathSin => JitIntrinsic::MathSin,
                    M::MathCos => JitIntrinsic::MathCos,
                    M::MathTan => JitIntrinsic::MathTan,
                    M::MathExp => JitIntrinsic::MathExp,
                    M::MathLog => JitIntrinsic::MathLog,
                    M::MathLog10 => JitIntrinsic::MathLog10,
                    M::MathCbrt => JitIntrinsic::MathCbrt,
                    M::MathPow => JitIntrinsic::MathPow,
                    M::StrictSin => JitIntrinsic::StrictMathSin,
                    M::StrictCos => JitIntrinsic::StrictMathCos,
                    M::StrictTan => JitIntrinsic::StrictMathTan,
                    M::StrictExp => JitIntrinsic::StrictMathExp,
                    M::StrictLog => JitIntrinsic::StrictMathLog,
                    M::StrictLog10 => JitIntrinsic::StrictMathLog10,
                    M::StrictCbrt => JitIntrinsic::StrictMathCbrt,
                    M::StrictPow => JitIntrinsic::StrictMathPow,
                    M::Asin => JitIntrinsic::MathAsin,
                    M::Acos => JitIntrinsic::MathAcos,
                    M::Atan => JitIntrinsic::MathAtan,
                    M::Atan2 => JitIntrinsic::MathAtan2,
                    M::Sinh => JitIntrinsic::MathSinh,
                    M::Cosh => JitIntrinsic::MathCosh,
                    M::Tanh => JitIntrinsic::MathTanh,
                    M::Expm1 => JitIntrinsic::MathExpm1,
                    M::Log1p => JitIntrinsic::MathLog1p,
                    M::Hypot => JitIntrinsic::MathHypot,
                    M::IeeeRemainder => JitIntrinsic::MathIeeeRemainder,
                };
                return Some((intr.as_entry(), m.arity(), b'D'));
            }
            return None;
        }
    };
    ir::try_ir_scalar_intrinsic(class, name, descriptor)?;
    Some((intrinsic.as_entry(), num_params, ret))
}

/// `CRATONVM_JIT_STRING_HASH_IS_ZERO` (JIT round 19 proposal wave 6, lane
/// str6, STRGAP19-6; default ON since the round-19 close as a HotSpot-parity fix -- the
/// `hashIsZero` field is reflection-visible; `=0` turns it off): both tiers' inline
/// `String.hashCode()` honour and set `hashIsZero` exactly as JDK 25's body
/// (`if (h == 0 && !hashIsZero) { ...; if (h == 0) hashIsZero = true; else
/// hash = h; }`) on a COMPACT receiver whose layout records the field
/// ([`StringFieldLayout::hash_is_zero_compact_offset`]). Off: the single-pass
/// body recomputes a zero hash on every call and never writes the field, the
/// optimizing tier declines a zero hash to the call. Compile time only.
pub(crate) fn string_hash_is_zero_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_HASH_COMPUTE` (nio20 wave 4, lane L5, proposal
/// W3-L2-2; default ON, `=0` off): the prefix computes an UNCACHED
/// `String.hashCode()` of a COMPACT receiver inline and stores a non-zero
/// result into `hash`, as the JDK body does (and as the single-pass body
/// does under `CRATONVM_JIT_STRING_HASH_CACHE_WRITE`). A zero result, and a
/// legacy receiver, still decline to the call, which sets `hashIsZero` /
/// writes the legacy cell. Compile time only, not cached.
pub(crate) fn ir_string_hash_compute_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_GETCHARS_PREFIX` (nio20 wave 6, lane L2, the
/// `heapcb` row; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `String.getChars(II[CI)V` as
/// [`IrStringSearchKind::GetChars`] -- the optimizing tier's inline LATIN1 ->
/// UTF-16 copy (`ir_lower::Lowerer::try_emit_ir_string_getchars_prefix`), and
/// with it the planning-gate carve-out, the splice carve-out and the
/// `ScalarIntrinsic` acceptance evidence for that site. `=0` restores the
/// wave-5 behaviour exactly (a method calling `getChars` bails the planning
/// gate unless `CRATONVM_JIT_IR_STRING_CALL_ADMIT` admits it). Compile time
/// only, not cached.
pub(crate) fn ir_string_getchars_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_CHARS_PREFIX` (JIT r20 wave 12, lane qStr; book
/// `jit-r20-nio-proposals-w6-L2.md`, W6-L2-2; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `String.toCharArray()`,
/// `String.valueOf(char[])` / `copyValueOf(char[])` and `new String(char[])`
/// as the [`IrStringSearchKind::ToCharArray`] / `CharsValueOf` / `CharsInit`
/// prefix kinds -- an inline TLAB allocation plus the LATIN1 inflate or the
/// `PACKUSWB` compress where the call crossed into the native (`--compatible`)
/// or ran the JDK's per-char `StringUTF16.compress` / `StringLatin1.toChars`
/// chain (`--real-jdk`) -- and with them the planning-gate and splice
/// carve-outs for those sites. `=0` restores today's behaviour exactly (each
/// such site bails the planning gate unless `CRATONVM_JIT_IR_STRING_CALL_ADMIT`
/// admits it). Compile time only, not cached.
pub(crate) fn ir_string_chars_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_CODING_PREFIX` (JIT r20 wave 18, lane xUtf8; book
/// `jit-r20-strings-proposals.md`, STR20-6; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `StringCoding.countPositives` and
/// `StringCoding.implEncodeAsciiArray` as the [`IrStringSearchKind::CountPositives`]
/// / `EncodeAsciiArray` prefix kinds -- the inline 16-byte / 8-char SSE2 scan
/// and copy HotSpot's stubs make (`String.decodeASCII`, `hasNegatives`, the
/// `new String(bytes, cs)` / `getBytes` ASCII fast paths,
/// `UTF_8$Encoder.encodeArrayLoop`'s `JLA.uncheckedEncodeASCII`) in place of
/// the JDK's byte-per-iteration loops -- and with them the splice carve-out
/// that keeps those sites CALLs. `=0` restores today's behaviour exactly (the
/// bodies splice or are called as before). Compile time only, not cached.
pub(crate) fn ir_string_coding_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_APPEND_CODE_POINT_PREFIX` (JIT r20 wave 19, lane zCp;
/// page `jr20-pstr3-code-point-latin1-static-init-mcs-flip-rows-CLOSED-20261012.md`,
/// row `R20PIRCodePoint` cp-append; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`, NOT inside the opt-in
/// `CRATONVM_JIT_IR_SB_PREFIX`): `StringBuilder.appendCodePoint(int)` as
/// [`IrStringSearchKind::SbAppendCodePoint`] -- one byte into a LATIN1
/// builder, one char or the surrogate pair into a UTF16 one, in place,
/// declining to the call on a growth / inflation / invalid code point -- and
/// with it the splice carve-out that keeps the site a CALL. Not a
/// STRINGBUILDER_ACCESS row, so no method is pinned for containing one. `=0`
/// restores today's behaviour exactly. Compile time only, not cached.
pub(crate) fn ir_sb_append_code_point_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_CODE_POINT_AT_PREFIX` (JIT r23 wave 2, lane L5,
/// R23W1L5-3; perf, default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`, NOT inside the opt-in
/// `CRATONVM_JIT_IR_SB_PREFIX`): `StringBuilder.codePointAt(int)` as
/// [`IrStringSearchKind::SbCodePointAt`], which also releases the pin the
/// single-pass row of the site (`CRATONVM_JIT_SP_SB_CODE_POINT_AT`) would set.
/// Compile time only, not cached.
pub(crate) fn ir_sb_code_point_at_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ARRAYS_EQUALS_PREFIX` (JIT r23 wave 2b, lane L5,
/// INTRINSICS22-5's inline-prefix half; perf, default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX` and the
/// `CRATONVM_JIT_IR_ARRAYS_EQUALS_CALL` admission): the integral
/// `Arrays.equals` overloads as [`IrStringSearchKind::ArraysEquals`] -- an
/// inline compare in place of the call / the spliced JDK body -- and with it
/// the splice carve-out that keeps the site a call. Compile time only.
pub(crate) fn ir_arrays_equals_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SB_APPEND_CODE_POINT_COUNT_GUARD` (JIT r23 wave 1, lane
/// L5; correctness, default ON, `=0` restores the old arm): the
/// `SbAppendCodePoint` prefix's supplementary arm also checks `count <
/// capacity` (unsigned) before its room test `count + 2 <= capacity`, as its
/// BMP arm and the single-pass row (`x64/string_code_point.rs`) do. A
/// negative `count` -- only reflection or `Unsafe` can store one -- otherwise
/// passed the room test (`-2 + 2 == 0`) and the pair was stored outside the
/// array. Compile time only, not cached.
pub(crate) fn ir_sb_append_code_point_count_guard_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_CHARACTER_TO_CHARS_PREFIX` (JIT r20 wave 19, lane zCp;
/// same page, row `R20PIRCodePoint` cp-tochars; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `Character.toChars(int)` as
/// [`IrStringSearchKind::CharacterToChars`] -- a fresh `char[1]` / `char[2]`
/// bumped and filled inline (`x64::concat_emit::emit_ir_character_to_chars`)
/// where the call ran the JDK body (its `char[]` allocation, and an
/// invalid-code-point arm whose `String.format` call keeps the body out of
/// every splice) -- and with it the splice carve-out. `=0` restores today's
/// behaviour exactly. Compile time only, not cached.
pub(crate) fn ir_character_to_chars_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX` (JIT r21 wave 1, lane
/// strings, proposal CP21-1 in `jit-r20-wave20-residual-proposals.md`; row
/// `R20PIRCodePoint` cp-iterate, 9680 ns vs HotSpot 50; **default ON (flipped; `=0` kills)**, a
/// perf change; inside `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`):
/// `String.codePointAt(int)` as [`IrStringSearchKind::CodePointAt`]. Two
/// effects, one switch: a method with such a site is no longer refused the
/// optimizing tier by the invoke-planning gate (every unlisted
/// `java/lang/String` member bails it to the single-pass backend, where the
/// call is a native-funnel crossing to the registered `Intrinsic`), and the
/// site answers inline (`ir_lower`'s `try_emit_ir_string_search_prefix`).
/// It also keeps the site a call in the splice planner, so it supersedes the
/// opt-in `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_CODE_POINT` splice while
/// both are set. Unset restores today's behaviour exactly. Compile time only,
/// not cached.
pub(crate) fn ir_string_code_point_at_prefix_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_STRING_CODE_POINT_BEFORE_PREFIX` (JIT r23 wave 1, lane L5,
/// STRINGS21-4 in `jit-r21-strings-proposals.md`; perf, default ON, `=0` off;
/// inside `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `String.codePointBefore(int)`
/// as [`IrStringSearchKind::CodePointBefore`] -- the planning gate stops
/// bailing a method for the site, the site answers inline, and the splice
/// planner keeps it a call. `=0` restores today's refusal exactly. Compile
/// time only, not cached.
pub(crate) fn ir_string_code_point_before_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_CODE_POINT_COUNT_PREFIX` (JIT r23 wave 1, lane L5,
/// STRINGS21-4; perf, default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `String.codePointCount(int, int)`
/// as [`IrStringSearchKind::CodePointCount`] (the registered `Intrinsic`'s
/// funnel crossing replaced by a bounded inline loop), with the same three
/// effects. Compile time only, not cached.
pub(crate) fn ir_string_code_point_count_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT` (JIT r21 wave 1, lane strings,
/// STRINGS21-1 in `jit-r21-strings-proposals.md`; **default ON (flipped; `=0` kills)**, a perf
/// change; the two prefix kinds also sit inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`). The invoke-planning gate
/// (`string_site_bails_ir_planning`) stops refusing the optimizing tier to a
/// method for three String-internal members the JDK's own String code calls
/// everywhere: `coder()B` and `isLatin1()Z` become the
/// [`IrStringSearchKind::StrCoder`] / [`IrStringSearchKind::StrIsLatin1`]
/// prefix kinds (inline field reads; the call otherwise, which the splice
/// planner may also splice -- the JDK bodies are a static and a field read),
/// and the package-private `String.<init>([BB)V` (`this.value = value;
/// this.coder = coder`) is admitted as the ordinary statically bound call the
/// optimizing tier already makes for every other constructor. None of the
/// three is a single-pass intrinsic, so no inline body is lost. Unset
/// restores today's refusals exactly. Compile time only, not cached.
///
/// JIT r21 wave 3: under this switch the two `StringLatin1` workers those
/// admitted bodies call are prefix kinds too
/// ([`IrStringSearchKind::Latin1CompareToCI`],
/// [`IrStringSearchKind::Latin1ToLowerCase`]), so the admitted body is never
/// the JDK loop the wave-1 A/B measured slower than the single-pass body.
pub(crate) fn ir_string_internal_admit_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_STRING_ACCESSOR_CALL_PREFIX` (JIT round 21, lane strintr2,
/// STRINTR21-1 of `jit-r21-strintr-proposals.md`, page
/// `jr21-strintr-string-pin-residual-20261015.md`, probe `R21StrintrRows`
/// charat-oob; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): a `java/lang/String`
/// `length()` / `isEmpty()` / `charAt(int)` site that reaches the lowering as
/// an `Op::Call` (the accessor expander refused it: inside a splice, a
/// de-specialised site, the expander switched off) takes the
/// [`IrStringSearchKind::StrLength`] / `StrIsEmpty` / `StrCharAt` prefix --
/// the `CharSequence` arms minus the kind-bit test -- and declines to the
/// unchanged call on a null receiver, a null `value` or an index out of range
/// (the call raises the exact exception). These kinds are NOT reported by
/// [`ir_string_search_prefix_handled_kind`], so the pin, the planning gate
/// and both splice planners decide exactly as with `=0`. Compile time only.
pub(crate) fn ir_string_accessor_call_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW` (JIT round 21, lane strintr2,
/// STRINTR21-1, same page and probe row; default ON, `=0` off): the String
/// accessor expander (`ir::IrBuilder::try_string_access_intrinsic`) deopts
/// at the invoke on a null receiver, a null `value` and an index out of
/// range (`DeoptReason::UncommonTrap`), and every such deopt evicts the
/// artifact. It never asked the de-spec registry, so a `charAt` whose callers
/// catch the `StringIndexOutOfBoundsException` re-expanded on every recompile
/// and paid deopt + eviction + recompile per throw. On, a site whose bci the
/// VM has de-specialised (`PER_BCI_DESPEC_LIMIT` charged traps) is built as
/// the plain `Op::Call` instead, which the lowering fronts with the
/// [`ir_string_accessor_call_prefix_enabled`] prefix. Compile time only.
pub fn ir_string_access_trap_withdraw_enabled() -> bool {
    true
}

/// JIT round 21, lane strintr2 (STRINTR21-1): the compiling method's own
/// `invokevirtual` pcs whose String-accessor guard the VM has withdrawn --
/// `(UncommonTrap, pc)` (or the wildcard) in this VM's de-spec registry under
/// `method_key` (`"class.name:descriptor"`, the key the trap sinks charge).
/// Every `invokevirtual` pc is asked; the builder consults the set only at a
/// String accessor site. Empty under
/// `CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW=0` or with no registry.
pub(crate) fn string_access_withdrawn_pcs(
    despec: Option<&deopt::DespecRegistry>,
    method_key: &str,
    invoke_ops: &[(usize, u16, u8)],
) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    let Some(registry) = despec else {
        return out;
    };
    if !ir_string_access_trap_withdraw_enabled() {
        return out;
    }
    for &(pc, _, opcode) in invoke_ops {
        if !matches!(opcode, 0xb6 | 0xb9) {
            continue;
        }
        let Ok(bci) = u32::try_from(pc) else {
            continue;
        };
        let id = deopt::speculation_id(bci, deopt::DeoptReason::UncommonTrap);
        if registry.contains_speculation(method_key, bci, id) {
            out.insert(pc);
        }
    }
    out
}

/// `CRATONVM_JIT_IR_SB_LENGTH_PREFIX` (JIT r21 wave 3, lane strings,
/// STRINGS21-3 in `jit-r21-strings-proposals.md`; **opt-in** `=1`; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `StringBuilder.length()` as
/// [`IrStringSearchKind::SbLength`] WITHOUT the rest of the opt-in
/// `CRATONVM_JIT_IR_SB_PREFIX` -- one `count` read, the single-pass body's
/// own answer -- so a builder loop whose only STRINGBUILDER_ACCESS site is
/// `length()` is released from the single-pass pin. Compile time only.
pub(crate) fn ir_sb_length_prefix_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_STRING_COMPARE_TO_CI_PREFIX` (JIT r21 wave 1, lane
/// strings, STRINGS21-2 / ZCP-4; **default ON (flipped; `=0` kills)**; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `String.compareToIgnoreCase` as
/// [`IrStringSearchKind::CompareToIgnoreCase`] (two LATIN1 operands inline),
/// which also lifts the planning gate for its caller. Independent of
/// [`ir_string_internal_admit_enabled`] (a user site needs no internal
/// member); `CASE_INSENSITIVE_ORDER.compare` (an interface site) is moved only
/// by that switch. Compile time only, not cached.
pub(crate) fn ir_string_compare_to_ci_prefix_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_STRING_LOWER_UNCHANGED_PREFIX` (JIT r21 wave 1, lane
/// strings, ZCP-3's non-allocating half; **default ON (flipped; `=0` kills)**; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `String.toLowerCase(Locale)` /
/// `toLowerCase()` as [`IrStringSearchKind::LowerLocaleUnchanged`] /
/// [`IrStringSearchKind::LowerUnchanged`] -- an already-lower LATIN1 receiver
/// answers itself; since JIT r21 wave 3 a changed one answers a fresh LATIN1
/// String of the mapped bytes unless it holds a `tr` / `az` / `lt`-sensitive
/// byte (`I`, `J`, `0xCC`, `0xCD`: the language is not read, so those are the
/// call) or the compile has no allocation plan. Also lifts the planning gate
/// for the caller. Compile time only, not cached.
pub(crate) fn ir_string_lower_unchanged_prefix_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED` (JIT round 21, lane strintr,
/// STR16-P3, page `jr21-strintr-string-pin-residual-20261015.md`, probe
/// `R21StrintrRows` cmp-mixed; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): the optimizing tier's
/// `String.compareTo` prefix ([`IrStringSearchKind::CompareTo`]) answers a
/// MIXED coder pair inline too -- `StringLatin1.compareToUTF16` /
/// `StringUTF16.compareToLatin1`'s answer, the first differing decoded char's
/// difference, else the length difference -- where it declined to the call
/// (the single-pass region decodes every coder pair inline). `=0` restores the
/// decline exactly. Compile time only, not cached.
pub(crate) fn ir_string_compare_to_mixed_enabled() -> bool {
    true
}

