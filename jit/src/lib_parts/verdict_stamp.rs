#[allow(unused_imports)]
use super::super::*;

impl JitCache {
    pub fn new() -> Self {
        let shards = (0..JIT_CACHE_SHARDS)
            .map(|_| JitCacheShard::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            reflect_epoch: std::sync::OnceLock::new(),
            mutation: parking_lot::Mutex::new(()),
            // Never flushed: every artifact is at or above epoch 1.
            flush_barrier: std::sync::atomic::AtomicU64::new(0),
            publication_holds: std::sync::atomic::AtomicU32::new(0),
            inlined_class_names: parking_lot::Mutex::new(std::collections::HashMap::new()),
            dependency_buckets: DependencyBucketBook::default(),
            invalidations: parking_lot::Mutex::new(InvalidationLog::default()),
            generation: std::sync::atomic::AtomicU64::new(1),
            redefine_epoch: std::sync::atomic::AtomicU32::new(0),
            callee_publish_buckets: (0..CALLEE_PUBLISH_BUCKETS)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            callee_publish_flushes: std::sync::atomic::AtomicU64::new(0),
            sweep: parking_lot::Mutex::new(CodeCacheSweepState::default()),
            cycle_cells: parking_lot::Mutex::new(FxHashMap::default()),
            retire_cells: parking_lot::Mutex::new(FxHashMap::default()),
            cycle_forwards: parking_lot::Mutex::new(FxHashMap::default()),
            cycle_forwards_installed: std::sync::atomic::AtomicU64::new(0),
            cycle_forwards_released: std::sync::atomic::AtomicU64::new(0),
            ic_supersede_retarget: true,
            reentry_only_enabled: false,
            reentry_only_keys: parking_lot::RwLock::new(FxHashSet::default()),
            reentry_only_any: std::sync::atomic::AtomicBool::new(false),
            invalidation_retires_superseded: false,
            superseded_bodies: superseded_bodies::SupersededBodies::default(),
            copied_bytecode_classes: parking_lot::Mutex::new(FxHashSet::default()),
            redefinition_barriers: parking_lot::Mutex::new(FxHashMap::default()),
            withdrawal_sink: std::sync::OnceLock::new(),
            not_entrant_hook: parking_lot::Mutex::new(None),
            not_entrant_patched: std::sync::atomic::AtomicU64::new(0),
            not_entrant_refused: std::sync::atomic::AtomicU64::new(0),
            mega_dispatch: std::sync::OnceLock::new(),
            lambda_adapters: crate::lambda_adapter::LambdaAdapterMap::new(),
            jdk_only_sinks: crate::jdk_only_sink::JdkOnlyReportSinks::new(),
            not_entrant_refusals: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            withdrawn_by_redefinition: std::sync::atomic::AtomicU64::new(0),
            whole_cache_redefinition_barrier: std::sync::atomic::AtomicU64::new(0),
            unpatched_bodies: parking_lot::Mutex::new(Vec::new()),
            exit_poll_bodies: parking_lot::Mutex::new(Vec::new()),
            unforced_withdrawal_at: std::sync::atomic::AtomicU64::new(0),
            post_call_exit_verdicts: std::sync::atomic::AtomicU64::new(0),
            post_call_exits_taken: std::sync::atomic::AtomicU64::new(0),
            osr_profit_exit: crate::tiered::osr_profit_exit_enabled(),
            osr_profit_armed: std::sync::atomic::AtomicU64::new(0),
            osr_profit_verdicts: std::sync::atomic::AtomicU64::new(0),
            osr_profit_exits_taken: std::sync::atomic::AtomicU64::new(0),
            osr_profit_disarms: std::sync::atomic::AtomicU64::new(0),
            door_owned_args_unpinned: true,
            by_name_tail_unpins_args: false,
            refused_resume_restashes_after_collection: true,
            // WARNING gci-i48-flipB-20261017: default ON (user-approved flip).
            deposit_locals_keep_liveness: true,
            interp_door_args_weak: true,
            first_call_door_args_weak: true,
            forced_chain_publications: parking_lot::Mutex::new(Vec::new()),
            supersede_redirect: true,
            supersede_pending: parking_lot::Mutex::new(FxHashMap::default()),
            supersede_redirects: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            sp_nested_miss_evidence: parking_lot::Mutex::new(FxHashMap::default()),
        }
    }

    /// TAIL9-20-2 (JIT round 19 wave 11, lane tail11): file `compiled`'s
    /// guarded nested splice counters under their sites. Empty (and nothing
    /// locked) unless the body was compiled under
    /// `CRATONVM_JIT_SP_NESTED_MISS_SHARE_REFUSE`. Fed whether or not the
    /// publication then succeeds: the words of a refused body never move.
    pub(crate) fn note_sp_nested_miss_cells(&self, compiled: &CompiledMethod) {
        if compiled.sp_nested_miss_cells.is_empty() {
            return;
        }
        let mut map = self.sp_nested_miss_evidence.lock();
        for row in compiled.sp_nested_miss_cells.iter() {
            map.entry(row.key.clone()).or_default().push(row.counters);
        }
    }

    /// TAIL11-2 (JIT round 19 wave 12, lane tail12): the published method-entry
    /// bodies whose OWN guarded nested splice counters crossed the TAIL9-20-2
    /// bar ([`sp_nested_miss_share_refuses`]), each claimed once
    /// ([`CompiledMethod::sp_nested_withdraw_claimed`]), by key -- for the VM
    /// to withdraw and recompile. Empty (and only a walk of the shard maps)
    /// when no body carries counters.
    pub fn claim_sp_nested_stuck_bodies(&self) -> Vec<WithdrawnMethodKey> {
        use std::sync::atomic::Ordering;
        let mut out = Vec::new();
        for shard in self.shards.iter() {
            for (key, cm) in shard.methods.load().values() {
                if cm.sp_nested_miss_cells.is_empty()
                    || cm.retired.load(Ordering::SeqCst)
                    || cm.sp_nested_withdraw_claimed.load(Ordering::Relaxed)
                {
                    continue;
                }
                let stuck = cm.sp_nested_miss_cells.iter().any(|row| {
                    sp_nested_miss_share_refuses(
                        row.counters.hits.load(Ordering::Relaxed),
                        row.counters.misses.load(Ordering::Relaxed),
                    )
                });
                if stuck && !cm.sp_nested_withdraw_claimed.swap(true, Ordering::AcqRel) {
                    out.push((
                        Arc::clone(&key.class_name),
                        Arc::clone(&key.method_name),
                        Arc::clone(&key.descriptor),
                        key.declaring_class_id,
                    ));
                }
            }
        }
        out
    }

    /// TAIL9-20-2: `(hits, misses)` summed over every body this cache was
    /// handed that guarded-spliced the call at `callee_pc` of `enclosing`
    /// (`"class/Name.method:desc"`) on `guard_class_id`; `None` when none did.
    /// Read by the VM's single-pass nested resolver under
    /// `CRATONVM_JIT_SP_NESTED_MISS_SHARE_REFUSE`.
    pub fn sp_nested_miss_counts(
        &self,
        enclosing: &str,
        callee_pc: usize,
        guard_class_id: u32,
    ) -> Option<(u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        let map = self.sp_nested_miss_evidence.lock();
        let rows = map.get(&(Box::<str>::from(enclosing), callee_pc, guard_class_id))?;
        Some(rows.iter().fold((0u64, 0u64), |(h, m), c| {
            (
                h.saturating_add(c.hits.load(Relaxed)),
                m.saturating_add(c.misses.load(Relaxed)),
            )
        }))
    }

    /// gcg g6/jm: may this VM's Rust dispatch doors skip pinning a call's
    /// arguments across a callee body that owns them? Read once, at
    /// construction (the VM's `door_pins_call_args`).
    #[inline]
    pub fn door_owned_args_unpinned(&self) -> bool {
        self.door_owned_args_unpinned
    }

    /// gch h1/fr (2026-10-08, `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`):
    /// may this VM's by-name dispatch (`invoke_on_class_shared_inner`) release
    /// its argument pins before its bytecode tail enters
    /// `interpreter::execute`? `execute` pins its own copy at entry and drops it
    /// once the callee's frame holds the arguments, so the door's pins are
    /// redundant there and only keep a parameter the callee nulled reachable
    /// for the whole call (HotSpot: the callee's frame map drops it). Opt-in;
    /// read once, at construction.
    #[inline]
    pub fn by_name_tail_unpins_args(&self) -> bool {
        self.by_name_tail_unpins_args
    }

    /// gch h2/fj (2026-10-08,
    /// `gcd-d10f-proposal-restash-a-callee-frame-refused-after-a-collection-20260928`):
    /// does this VM's call-site service (`helpers::try_resume_trapped_callee`)
    /// pin a taken callee stash's object words across the frame rebuild, so a
    /// rebuild refused after a collection RESTASHES the frame at its current
    /// addresses instead of dropping it and forcing a re-run from entry?
    /// Opt-in; read once, at construction.
    #[inline]
    pub fn refused_resume_restashes_after_collection(&self) -> bool {
        self.refused_resume_restashes_after_collection
    }

    /// gch h2/fj (2026-10-08, `gch-h1jp-dispatch-doors-and-oome-ladder-review-findings-DONE-20261012`
    /// item 1): does this VM's in-native root deposit
    /// (`NativeContextImpl::deposit_root_snapshot_inner`) keep the bytecode
    /// liveness proof for frame locals under the conservative-locals rule (plus
    /// the tag-blind probe), as `collect_roots` and `update_root_snapshot` do,
    /// instead of rooting every local, dead or alive? Opt-in; read once, at
    /// construction.
    #[inline]
    pub fn deposit_locals_keep_liveness(&self) -> bool {
        self.deposit_locals_keep_liveness
    }

    /// gch h2/fj (2026-10-08, `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`,
    /// P3; `gch-h1fr-proposal-owned-body-argument-pins-remap-but-do-not-root`):
    /// do this VM's interpreter->compiled doors (`jit_bridge::execute_jit_call`,
    /// `execute_jit_call_decoded`) hold the reference arguments of a body that
    /// owns its arguments as WEAK (remapped, cleared when dead) handles across
    /// the compiled run instead of strong pins, so a parameter the callee
    /// nulled can be collected, as on HotSpot? Default ON since JIT r20 GC
    /// wave 9 (`=0` kills); read once, at construction.
    #[inline]
    pub fn interp_door_args_weak(&self) -> bool {
        self.interp_door_args_weak
    }

    /// gch h3/fd (2026-10-08, `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`,
    /// P2): does `interpreter::execute`'s first-call door hold the reference
    /// arguments of a body that owns its arguments as WEAK handles across the
    /// compiled run instead of its `InvokeArgsRootGuard` pins (the door the
    /// not-entrant re-dispatch reaches)? Default ON since JIT r20 GC wave 9
    /// (`=0` kills), separate from
    /// [`Self::interp_door_args_weak`] so each door's rows decide it alone;
    /// read once, at construction.
    #[inline]
    pub fn first_call_door_args_weak(&self) -> bool {
        self.first_call_door_args_weak
    }

    /// Round 16 lane chain (CH5W4-3): record `body`'s chain points for the
    /// VM's eager-chains census, at publication (called from
    /// [`Self::note_exit_poll_body`], which every publication funnel calls:
    /// [`Self::put`], [`Self::put_osr`] and the VM's optimizing OSR memo).
    /// Nothing for a body without forced blocks, i.e. for every body unless
    /// `CRATONVM_DEOPT_EAGER_CHAINS` is on; then one description per chain
    /// point, made while the body is certainly live.
    pub fn note_forced_chain_publication(&self, body: &CompiledMethod) {
        if body._forced_chain_deopt_points.is_empty() {
            return;
        }
        let kind = if !body.was_published() {
            "osr-optimizing"
        } else if body.compiled_via_osr {
            "osr"
        } else {
            "entry"
        };
        if let Some(points) = body.forced_chain_points_for_census(kind) {
            self.forced_chain_publications.lock().push(points);
        }
    }

    /// Round 16 lane chain (CH5W4-3): drain the rows
    /// [`Self::note_forced_chain_publication`] recorded since the last drain,
    /// oldest first (the VM census folds them into its coverage table).
    pub fn take_forced_chain_publications(&self) -> Vec<deopt::PublishedChainPoints> {
        std::mem::take(&mut *self.forced_chain_publications.lock())
    }

    /// This VM's shared megamorphic dispatch table, allocated on first use
    /// (round 12 wave 2, lane calls, proposal W2-1). For a publisher.
    pub fn mega_dispatch_table(&self) -> &MegaDispatchTable {
        self.mega_dispatch
            .get_or_init(|| Box::new(MegaDispatchTable::new()))
    }

    /// The table if anything was ever published into it. For a reader, which
    /// has nothing to find in a table nobody allocated.
    pub fn mega_dispatch_table_if_any(&self) -> Option<&MegaDispatchTable> {
        self.mega_dispatch.get().map(|table| &**table)
    }

    /// This VM's lambda-adapter thunk map: what the VM's lambda helper builds
    /// thunks into (`lambda_adapter::lambda_adapter_entry` and friends) and
    /// what this cache's invalidation and supersede sweeps search.
    pub fn lambda_adapters(&self) -> &crate::lambda_adapter::LambdaAdapterMap {
        &self.lambda_adapters
    }

    /// This VM's `--real-jdk-report` sinks: the compile-time thin-native
    /// refusals (via `CompileRealm::jdk_only_sinks`), the inline-cache
    /// refusals made inside its compiles, and the runtime helpers' fast-path
    /// refusals (recorded by the VM).
    pub fn jdk_only_sinks(&self) -> &crate::jdk_only_sink::JdkOnlyReportSinks {
        &self.jdk_only_sinks
    }

    /// Round 13 wave 4 (lane mega3, `r13w2-mega-dead-loader-selectors-patch`):
    /// forget the megamorphic selectors of loaders that unloaded (`dead`
    /// answers native loader ids, `ClassLoaderId::to_native_id`) and withdraw
    /// the ways keyed on them, so loader churn stops exhausting
    /// `MEGA_DISPATCH_SELECTOR_CAP`. Returns how many selectors were forgotten.
    /// Switch `CRATONVM_JIT_MEGA_FORGET_DEAD_LOADERS` (default on; `0` keeps
    /// them for the table's life, as before). Read once per unload pass.
    pub fn forget_dead_loader_selectors(&self, dead: &dyn Fn(u32) -> bool) -> usize {
        self.mega_dispatch_table_if_any()
            .map_or(0, |table| table.forget_loader_selectors(dead))
    }

    /// Record the classes `compiled` inlined from, so a later
    /// `invalidate_for_class` naming a class nobody inlined can answer without
    /// a scan. See [`Self::inlined_class_names`].
    pub(crate) fn note_inlined_classes(&self, compiled: &CompiledMethod) {
        if compiled.inlined_methods.is_empty() {
            return;
        }
        let mut names = self.inlined_class_names.lock();
        for (class_name, method, desc) in &compiled.inlined_methods {
            let members = names.entry(Arc::from(class_name.as_str())).or_default();
            // Avoid allocating the signature again for repeated publications.
            if !members.iter().any(|(m, d)| m.as_ref() == method && d.as_ref() == desc) {
                members.insert((Arc::from(method.as_str()), Arc::from(desc.as_str())));
            }
        }
    }

    /// Could any published body have inlined from `class_name`? A `false` here
    /// is a proof that an `inlined_methods`-only invalidation would find
    /// nothing.
    pub(crate) fn any_body_inlined_from(&self, class_name: &str) -> bool {
        self.inlined_class_names.lock().contains_key(class_name)
    }

    /// The same conservative publication summary, restricted to the members
    /// a subtype can change. Constructor-only dependencies (especially Object)
    /// need no cache scan. Publication records these before checking the log,
    /// so this early return has the same race ordering as the name-only one.
    pub(crate) fn any_body_depends_on_subtype(&self, class_name: &str, fixed: &[(Arc<str>, Arc<str>)]) -> bool {
        self.inlined_class_names.lock().get(class_name).is_some_and(|members| {
            members.iter().any(|(m, d)| subtype_sensitive_member(m, d, fixed))
        })
    }

    /// The perf counters `sun.ci.findWitnessIn` and `sun.ci.nmethodBuckets*`
    /// of this cache ([`DependencyBucketBook`]). Purges the book first, so
    /// one lock and a pass over the live records; the perf sampler's read.
    pub fn dependency_counter_values(&self) -> DependencyCounterValues {
        self.dependency_buckets.values()
    }

    /// Compatibility accessors for callers written against the historical
    /// outer `RwLock<JitCache>`. They deliberately do not acquire a lock.
    #[inline]
    pub fn read(&self) -> &Self {
        self
    }

    #[inline]
    pub fn write(&self) -> &Self {
        self
    }

    #[inline]
    pub(crate) fn shard_index(hash: u64) -> usize {
        (hash as usize) & (JIT_CACHE_SHARDS - 1)
    }

    /// Look up a compiled method by (class, method, descriptor).
    ///
    /// Hot path: this is called on every invoke of a JIT-compiled
    /// method via the call-site cache. To avoid three `Arc<str>`
    /// clones per probe (each = an atomic refcount increment), the
    /// lookup is keyed by a precomputed u64 hash and the full string
    /// key is verified after the hash hit (collision check).
    ///
    /// On a hash collision (extremely rare; the caller will fall
    /// through to the slow path and re-JIT), this returns `None`.
    /// That is a correctness-safe but slightly suboptimal outcome —
    /// see [`compute_jit_key_hash`] for the rationale.
    ///
    /// Callers holding an `&Arc<str>` can pass it directly: deref
    /// coercion turns it into `&str`.
    pub fn get(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> Option<Arc<CompiledMethod>> {
        self.get_for_reentry(class_name, method_name, descriptor, declaring_class_id)
            // A re-entry-only body is never a binding target (see
            // [`CompiledMethod::reentry_only`]); only [`Self::get_for_reentry`]
            // answers it.
            .filter(|method| !method.reentry_only)
    }

    /// [`Self::get`], also answering a RE-ENTRY-ONLY body
    /// ([`CompiledMethod::reentry_only`]). For the callers that run the
    /// method's OWN bytecode because they were asked to (`interpreter::execute`
    /// for the method it executes, and a compile door's read-back of the body
    /// it just published), never for a door that binds a caller to the body.
    /// Interpreter round i6 wave 117, lane L2.
    pub fn get_for_reentry(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> Option<Arc<CompiledMethod>> {
        let h = compute_jit_key_hash(class_name, method_name, descriptor, declaring_class_id);
        let methods = self.shards[Self::shard_index(h)].methods.load();
        let (key, method) = methods.get(&h)?;
        if &*key.class_name == class_name
            && &*key.method_name == method_name
            && &*key.descriptor == descriptor
            && key.declaring_class_id == declaring_class_id
        {
            Some(method.arc().clone())
        } else {
            None
        }
    }

    /// Mark `class.method descriptor` (every class id) as publishing
    /// RE-ENTRY-ONLY bodies ([`CompiledMethod::reentry_only`]): the method is
    /// shadowed by a registered `Intrinsic`, and only the natives'
    /// bytecode-only re-entries (`interpreter::execute`) may run a compiled
    /// body of it. Per cache (per VM); a no-op answering `false` unless
    /// `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`. Interpreter round i6 wave 117,
    /// lane L2 (page
    /// `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`).
    pub fn mark_reentry_only(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        if !self.reentry_only_enabled {
            return false;
        }
        self.mark_reentry_only_row(class_name, method_name, descriptor);
        true
    }

    /// [`Self::mark_reentry_only`] for one row whose OWN switch the caller
    /// has already asked, so the cache-wide
    /// `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION` does not gate it (JIT round 23
    /// wave 8h, lane W8-L1: `BuiltinClassLoader.loadClassOrNull` under
    /// `CRATONVM_JIT_LOADER_REENTRY_BODIES`). Every reader of the mark asks
    /// key membership only, so the row gets exactly the re-entry-only
    /// publication the switch gives every marked row.
    pub fn mark_reentry_only_row(&self, class_name: &str, method_name: &str, descriptor: &str) {
        let h = compute_jit_key_hash(
            class_name,
            method_name,
            descriptor,
            cratonvm_types::ClassId::new(0),
        );
        self.reentry_only_keys.write().insert(h);
        self.reentry_only_any
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Is `class.method descriptor` marked by [`Self::mark_reentry_only`]?
    /// One load while nothing is marked (always, with the switch off).
    pub fn is_reentry_only_key(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        if !self
            .reentry_only_any
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }
        let h = compute_jit_key_hash(
            class_name,
            method_name,
            descriptor,
            cratonvm_types::ClassId::new(0),
        );
        self.reentry_only_keys.read().contains(&h)
    }

    /// DIAG: every `declaring_class_id` under which this (class, method,
    /// descriptor) triple is published, ignoring the id entirely.
    ///
    /// [`get`](Self::get) requires an EXACT `declaring_class_id` match, so a
    /// caller that probes with the wrong id (notably
    /// `get_loaded_class_id(..).unwrap_or(0)`) misses a body that is sitting
    /// right there. This says so instead of leaving it to inference.
    pub fn debug_ids_for(&self, class_name: &str, method_name: &str, descriptor: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for shard in self.shards.iter() {
            let methods = shard.methods.load();
            for (key, _) in methods.values() {
                if &*key.class_name == class_name
                    && &*key.method_name == method_name
                    && &*key.descriptor == descriptor
                {
                    out.push(key.declaring_class_id.as_u32());
                }
            }
        }
        out
    }

    /// Look up the independently published OSR body for a method.
    pub fn get_osr(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> Option<Arc<CompiledMethod>> {
        let h = compute_jit_key_hash(class_name, method_name, descriptor, declaring_class_id);
        let methods = self.shards[Self::shard_index(h)].osr_methods.load();
        let (key, method) = methods.get(&h)?;
        if &*key.class_name == class_name
            && &*key.method_name == method_name
            && &*key.descriptor == descriptor
            && key.declaring_class_id == declaring_class_id
        {
            Some(method.arc().clone())
        } else {
            None
        }
    }

    /// Returns `false` when this artifact was compiled against inputs a later
    /// flush of THIS cache retired, in which case it must not be published.
    ///
    /// Callers must hold `mutation` — that is what makes the answer stable
    /// against a concurrent [`Self::clear_all`].
    /// Hold every publication into this cache until the matching
    /// [`Self::release_publications`]: the VM's multi-class redefinition call
    /// (`obsolete_frames::raise_install_fence`) takes it before its first
    /// class is installed and releases it after its last (interpreter round
    /// i1 wave 40, lane L2 follow-up;
    /// `L3W38RedefineTwoClassesSamePool.java`).
    ///
    /// JVMTI installs a call atomically. The VM installs it class by class,
    /// under a fence that holds every other thread's INTERPRETER loops; a
    /// compiled body is not held. A body compiled between two installs from
    /// a class already installed (the new `P.v`), published and entered
    /// before the next install, returned the new version while a still-valid
    /// compiled body of the next class (the old `Q.v`) returned the old one.
    /// With publication held, nothing compiled inside the window is ever
    /// entered: the bodies that were published before it and depend on an
    /// installed class were withdrawn by that install, and what is compiled
    /// inside it is refused here (a later call recompiles it). A compile that
    /// begins inside the window and publishes after it is judged by the
    /// invalidation log as before: a dependency on a class installed after
    /// it began withdraws it.
    ///
    /// Taken under `mutation`, the lock both publication paths hold, so a
    /// publication either lands entirely before the hold (and so before the
    /// first install, which withdraws it if it depends on the call's classes)
    /// or sees it.
    pub fn hold_publications(&self) {
        let _mutation = self.mutation.lock();
        self.publication_holds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Release one [`Self::hold_publications`].
    pub fn release_publications(&self) {
        let _mutation = self.mutation.lock();
        let _ = self.publication_holds.try_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |holds| holds.checked_sub(1),
        );
    }

    /// Is a multi-class redefinition call of this cache's VM installing
    /// ([`Self::hold_publications`])? One relaxed-enough load; the OSR door
    /// asks it before entering a body it built or found outside this cache.
    #[inline]
    pub fn publications_held(&self) -> bool {
        self.publication_holds
            .load(std::sync::atomic::Ordering::SeqCst)
            != 0
    }

    /// [`Self::put`] / [`Self::put_osr`]'s refusal while publications are held,
    /// with its positive control (`CRATONVM_DBG_JITC=1`).
    pub(crate) fn publication_refused_by_hold(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        if !self.publications_held() {
            return false;
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] publication held (a redefinition call is installing): {class_name}.{method_name}{descriptor}"
            );
        }
        true
    }

    pub(crate) fn publication_epoch_is_current(&self, compiled: &CompiledMethod) -> bool {
        let barrier = self
            .flush_barrier
            .load(std::sync::atomic::Ordering::Acquire);
        if compiled.install_epoch >= barrier {
            return true;
        }
        STALE_INSTALL_EPOCH_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dbg_jit_pin_enabled() {
            eprintln!(
                "[jit-stale-install] refusing a body compiled at epoch {} into a cache \
flushed at epoch {barrier}",
                compiled.install_epoch,
            );
        }
        !strict_install_epoch_enabled()
    }

    /// Did a compilation stamped `install_epoch` ([`CompiledMethod::install_epoch`])
    /// start after this cache's last [`Self::clear_all`]? The publication
    /// test of [`Self::put`], without the census, asked of a body that may
    /// already be running. Since interpreter round i1 wave 20 a redefinition
    /// may withdraw only its dependents and flush nothing
    /// ([`Self::invalidate_for_redefinition`]), so "compiled from bytecode no
    /// redefinition has replaced since" is [`Self::compiled_since_redefinition_of`];
    /// this answers the flush half alone (interpreter round i1 wave 18, lane
    /// L3).
    #[inline]
    pub fn compiled_since_last_flush(&self, install_epoch: u64) -> bool {
        install_epoch
            >= self
                .flush_barrier
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// [`Self::compiled_since_last_flush`] for a body of `class_id`, also
    /// after a scoped redefinition ([`Self::invalidate_for_redefinition`]),
    /// which arms no flush barrier: `true` when the body's compilation began
    /// after this cache's last flush AND after the last scoped redefinition
    /// of `class_id`, so it runs the class's current bytecode. A scoped
    /// redefinition of another class does not make it `false` (interpreter
    /// round i1 wave 20, lane L1).
    pub fn compiled_since_redefinition_of(
        &self,
        class_id: cratonvm_types::ClassId,
        install_epoch: u64,
    ) -> bool {
        self.compiled_since_last_flush(install_epoch)
            && self
                .redefinition_barriers
                .lock()
                .get(&class_id)
                .map_or(true, |&barrier| install_epoch >= barrier)
    }

    /// Did a scoped redefinition ([`Self::invalidate_for_redefinition`]) of
    /// the class of a body `compiled` baked a direct call to happen after
    /// `compiled`'s compilation began? The scan retires the class's PUBLISHED
    /// bodies, which `prepare_for_publication` refuses as callees; a body a
    /// tier-up had superseded is in no map, so nothing retired it, and a
    /// compile that baked it while it was current would publish a call into
    /// the old bytecode (the full flush's barrier refused every such
    /// compile). On `true` the pins `prepare_for_publication` just took are
    /// released, as its own refusals release them. Callers hold `mutation`.
    pub(crate) fn baked_callee_redefined_since(&self, compiled: &mut CompiledMethod) -> bool {
        let stale = {
            let barriers = self.redefinition_barriers.lock();
            !barriers.is_empty()
                && compiled._direct_callee_roots.iter().any(|root| {
                    barriers
                        .get(&cratonvm_types::ClassId::new(root.owner_class_id))
                        .is_some_and(|&barrier| compiled.install_epoch < barrier)
                })
        };
        if stale {
            STALE_DEPENDENCY_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // `RetainedCode` handles: dropping them releases through the queue.
            drop(std::mem::take(&mut compiled._direct_callee_roots));
        }
        stale
    }

    /// Record that a compile for this cache copied method bytecode of
    /// `class_id` into another method's code (a splice, or an elided empty
    /// constructor). Call BEFORE reading that bytecode, under the VM's
    /// class-manager read guard. See `copied_bytecode_classes`.
    ///
    /// Interpreter round i1 wave 25, lane L6 (stage 1 of
    /// `i20-L1-proposal-per-body-copied-bytecode-dependencies`): the copy is
    /// also recorded on the compilation open on this thread, so every body it
    /// builds names the class ([`CompiledMethod::copied_classes`]) and a
    /// scoped redefinition of the class withdraws exactly those bodies. With
    /// [`SCOPE_REDEFINITIONS_BY_COPIED_CLASSES`] such a recorded copy no
    /// longer marks the class for the whole-cache path; a copy made with no
    /// compilation open on this thread always does.
    pub fn note_bytecode_copied(&self, class_id: cratonvm_types::ClassId) {
        if note_copy_in_open_compile(class_id.as_u32()) && SCOPE_REDEFINITIONS_BY_COPIED_CLASSES {
            return;
        }
        let mut copied = self.copied_bytecode_classes.lock();
        if !copied.contains(&class_id) {
            copied.insert(class_id);
        }
    }

    /// Has any compile for this cache copied method bytecode of `class_id`?
    /// See [`Self::note_bytecode_copied`].
    pub fn bytecode_was_copied(&self, class_id: cratonvm_types::ClassId) -> bool {
        self.copied_bytecode_classes.lock().contains(&class_id)
    }

    /// This cache's generation (one acquire load).
    ///
    /// The interpreter's "no compiled body" memos
    /// (`CachedBytecodeMethod::jit_probe_generation`, lambda sites) compare
    /// against it. They used to read the process-wide
    /// [`jit_cache_generation`], so a compilation in one VM flushed every VM's
    /// memos. That counter remains for the thread-local dispatch memos that
    /// outlive any one VM.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A stamp that moves when a body named `method_name descriptor` (in ANY
    /// class) is published in this cache, when the cache is flushed
    /// ([`Self::clear_all`]) and when a class is redefined
    /// ([`Self::bump_redefine_epoch`]) -- and, through bucket sharing, on
    /// some unrelated publications ([`CALLEE_PUBLISH_BUCKETS`]). Unlike
    /// [`Self::generation`] it does NOT move on a retirement or on a
    /// publication of another name. Interpreter round i4 wave 92, lane L7
    /// (`CRATONVM_JIT_CALLEE_NEG_KEYED_STAMP`): the VM's by-name callee door
    /// stamps its "no compiled entry" verdicts with it, so a define burst
    /// that retires and republishes other methods' bodies no longer
    /// re-probes every uncompiled callee. Keyed by name and descriptor
    /// without the class on purpose: an inherited synchronized method's
    /// self-locking body is published under its DECLARING class, and the
    /// door's answer for a subclass receiver changes with it. Three acquire
    /// loads.
    #[inline]
    pub fn callee_publish_stamp(&self, method_name: &str, descriptor: &str) -> u64 {
        use std::sync::atomic::Ordering;
        self.callee_publish_buckets[callee_publish_bucket(method_name, descriptor)]
            .load(Ordering::Acquire)
            .wrapping_add(self.callee_publish_flushes.load(Ordering::Acquire))
            .wrapping_add(u64::from(self.redefine_epoch()))
    }

    /// Advance one publication bucket (after the body is in its map).
    #[inline]
    pub(crate) fn bump_callee_publish_bucket(&self, bucket: usize) {
        self.callee_publish_buckets[bucket & (CALLEE_PUBLISH_BUCKETS - 1)]
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Advance this cache's generation and the process-wide one. `retire`:
    /// this mutation withdrew a body (or `put_osr` replaced one), so it moves
    /// the retire half of the process-wide word (see
    /// [`JIT_CACHE_RETIRE_SHIFT`]). A `put` replacement passes `false`: see the
    /// note at its bump and [`CompiledMethod::superseded`].
    #[inline]
    pub fn set_reflect_epoch(&self, epoch: std::sync::Arc<std::sync::atomic::AtomicU64>) {
        let _ = self.reflect_epoch.set(epoch);
    }

    pub(crate) fn bump_generation(&self, retire: bool) {
        use std::sync::atomic::Ordering;
        let step = if retire { JIT_CACHE_RETIRE_UNIT } else { 1 };
        JIT_CACHE_GENERATION.fetch_add(step, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
        if retire {
            if let Some(ep) = self.reflect_epoch.get() {
                ep.fetch_add(1, Ordering::Release);
            }
        }
    }

    /// Class redefinitions this VM has performed (one acquire load).
    ///
    /// Inline-cache slots stamp the value they were last validated against
    /// ([`JitMICSlot::redefine_epoch`]), so a redefinition costs each slot one
    /// flush rather than a flush on every dispatch for the rest of the process.
    /// Per cache, because the slots live in this cache's bodies: a process-wide
    /// epoch made one VM's redefinition flush every VM's inline caches and
    /// gate-pass memos.
    #[inline]
    pub fn redefine_epoch(&self) -> u32 {
        self.redefine_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record that a class was redefined in this VM. Invalidates each of this
    /// VM's inline caches exactly once, lazily, as it is next dispatched
    /// through.
    ///
    /// Also advances [`JIT_INSTALL_EPOCH`], so a compilation that is already in
    /// flight when this runs carries a strictly older stamp than any cache flush
    /// that follows. Arming is [`Self::clear_all`]'s job — see
    /// [`Self::flush_barrier`].
    pub fn bump_redefine_epoch(&self) {
        self.redefine_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        bump_jit_install_epoch();
        // Round 12 wave 2 (lane calls, W2-1): the shared megamorphic table has
        // no per-slot epoch stamp, so it is emptied here, AFTER the bump: a
        // publisher that read the old epoch either published before this
        // clear or re-reads the epoch after publishing and withdraws.
        if let Some(table) = self.mega_dispatch_table_if_any() {
            table.clear_entries();
        }
    }

    /// Log a dependency invalidation of this cache BEFORE it scans.
    ///
    /// A publication then either reads the record in
    /// [`Self::dependencies_are_current`], or published before the log was
    /// written and is found by the scan that follows. The epoch is advanced
    /// under the log lock so log order is epoch order.
    pub(crate) fn log_invalidation(&self, record: InvalidationRecord) {
        let mut log = self.invalidations.lock();
        let epoch = JIT_INVALIDATION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
        if log.records.len() >= INVALIDATION_LOG_CAPACITY {
            if let Some((dropped, _)) = log.records.pop_front() {
                log.dropped_through = dropped;
            }
        }
        log.records.push_back((epoch, record));
    }

    /// Returns `false` when an invalidation of THIS cache logged after
    /// `compiled`'s compilation began withdraws what it depends on, in which
    /// case it must not be published.
    ///
    /// `put` used to check only the install epoch against the flush barrier.
    /// A background compile that inlined or devirtualised `Base.m()` could
    /// therefore publish after `invalidate_for_class_change("Base")` had scanned
    /// a cache that did not contain it yet, and nothing would ever withdraw it.
    ///
    /// Callers hold `mutation` and have already recorded the body's inlined
    /// classes in `inlined_class_names`: an invalidation's early-out then either
    /// sees the name (and scans after this publication), or logged its record
    /// before this reads the log.
    pub(crate) fn dependencies_are_current(&self, key: &JitKey, compiled: &CompiledMethod) -> bool {
        self.dependencies_current_since(key, compiled, compiled.invalidation_epoch)
    }

    /// [`Self::dependencies_are_current`] for a body that is never published
    /// through this cache: the VM's optimizing OSR bodies, which its OSR door
    /// memoises itself (`vm/src/runtime/interpreter/jit_bridge.rs`,
    /// `osr_optimizing_cached`). `false` when an invalidation this cache logged
    /// after epoch `since` withdraws `compiled` as the method
    /// `class_name.method_name descriptor` of `declaring_class_id` -- a class it
    /// inlined from or devirtualised against gained a subclass or was
    /// redefined, or a class it depends on was unloaded. `since` is the
    /// body's own `invalidation_epoch`, or a later epoch the caller already
    /// checked it through (read from [`jit_invalidation_epoch`] BEFORE that
    /// check), so a re-check scans only the newer records. A refusal counts in
    /// [`stale_dependency_refusals`], like a refused publication.
    pub fn unpublished_body_dependencies_are_current(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
        compiled: &CompiledMethod,
        since: u64,
    ) -> bool {
        {
            // Nothing logged after `since`: no key to build.
            let log = self.invalidations.lock();
            match log.records.back() {
                None => return true,
                Some(&(newest, _)) if since >= newest => return true,
                Some(_) => {}
            }
        }
        let key = JitKey {
            class_name: Arc::from(class_name),
            method_name: Arc::from(method_name),
            descriptor: Arc::from(descriptor),
            declaring_class_id,
        };
        self.dependencies_current_since(&key, compiled, since)
    }

    /// [`Self::unpublished_body_dependencies_are_current`] for an unpublished
    /// body that is already INSTALLED -- an optimizing OSR body the VM's memo
    /// accepted and serves (`osr_optimizing_cached` / `_peek` re-checks), not
    /// the first check of a fresh build, which is its publication. t1 wave 7
    /// lane NBA (page `t1-V6-osr-memo-recheck-refuses-installed-bodies-after-
    /// observer-gain-20261026.md`): judged by [`InvalidationRecord::withdraws_installed_in`],
    /// so an object-observer gain spares it, as HotSpot's installed nmethods
    /// are spared (`ciEnv::jvmti_state_changed` fails only compiles in
    /// flight); every other record withdraws it as before.
    pub fn installed_body_dependencies_are_current(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
        compiled: &CompiledMethod,
        since: u64,
    ) -> bool {
        {
            let log = self.invalidations.lock();
            match log.records.back() {
                None => return true,
                Some(&(newest, _)) if since >= newest => return true,
                Some(_) => {}
            }
        }
        let key = JitKey {
            class_name: Arc::from(class_name),
            method_name: Arc::from(method_name),
            descriptor: Arc::from(descriptor),
            declaring_class_id,
        };
        self.dependencies_current_since_by(&key, compiled, since, true)
    }

    /// [`Self::dependencies_are_current`] against the records logged after
    /// `stamp` rather than after the body's own compile-start stamp.
    pub(crate) fn dependencies_current_since(
        &self,
        key: &JitKey,
        compiled: &CompiledMethod,
        stamp: u64,
    ) -> bool {
        self.dependencies_current_since_by(key, compiled, stamp, false)
    }

    /// [`Self::dependencies_current_since`] with the record rule chosen:
    /// `installed` judges by `withdraws_installed_in` (a body already
    /// installed), otherwise by the publication rule `withdraws_in`.
    pub(crate) fn dependencies_current_since_by(
        &self,
        key: &JitKey,
        compiled: &CompiledMethod,
        stamp: u64,
        installed: bool,
    ) -> bool {
        let log = self.invalidations.lock();
        let Some(&(newest, _)) = log.records.back() else {
            return true;
        };
        if stamp >= newest {
            return true;
        }
        let withdrawn = stamp < log.dropped_through
            || log
                .records
                .iter()
                .rev()
                .take_while(|(epoch, _)| *epoch > stamp)
                .any(|(_, record)| {
                    if installed {
                        record.withdraws_installed_in(self, key, compiled)
                    } else {
                        record.withdraws_in(self, key, compiled)
                    }
                });
        if !withdrawn {
            return true;
        }
        STALE_DEPENDENCY_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dbg_jit_pin_enabled() {
            eprintln!(
                "[jit-stale-dependency] refusing {}.{}{} compiled at invalidation epoch \
{stamp}: a later invalidation withdrew something it depends on",
                key.class_name, key.method_name, key.descriptor,
            );
        }
        false
    }

    /// Returns `false` when a baked direct-call target could not be pinned, in
    /// which case this body must NOT be published — its emitted code contains a
    /// `call` to an address nothing keeps alive.
    pub(crate) fn prepare_for_publication(
        compiled: &mut CompiledMethod,
        superseded_callable: &dyn Fn(&CompiledMethod) -> bool,
    ) -> bool {
        // DIAG (`CRATONVM_DBG_JIT_BAKED_AUDIT=1`): find `mov rax, imm64; call rax`
        // sequences whose imm64 is the entry of a live body this artifact does not
        // list in `_direct_callee_entries` -- a baked call nothing roots.
        if false {
            let code = compiled._buffer.as_slice();
            let listed: std::collections::HashSet<usize> =
                compiled._direct_callee_entries.iter().copied().collect();
            let owners = jit_entry_owners().lock();
            let mut i = 0usize;
            while i + 12 <= code.len() {
                if code[i] == 0x48
                    && code[i + 1] == 0xB8
                    && code[i + 10] == 0xFF
                    && code[i + 11] == 0xD0
                {
                    // The imm64, little-endian, folded from its eight bytes:
                    // no fallible slice-to-array conversion (the panic-free
                    // ratchet, `jit/tests/panic_free_compile_ratchet.rs`).
                    let imm = code[i + 2..i + 10]
                        .iter()
                        .rev()
                        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
                        as usize;
                    if owners.contains_key(&imm) && !listed.contains(&imm) {
                        let callee = owners
                            .get(&imm)
                            .and_then(std::sync::Weak::upgrade)
                            .and_then(|c| c.method_key_for_report());
                        eprintln!(
                            "[jit-baked-audit] UNROOTED baked call at +{i:#x} of {:?} (osr={}, entry={:#x}) -> {imm:#x} {:?}",
                            compiled.method_key_for_report(),
                            compiled.compiled_via_osr,
                            compiled.entry as usize,
                            callee
                        );
                    }
                    i += 12;
                } else {
                    i += 1;
                }
            }
        }
        let wanted = compiled._direct_callee_entries.len();
        // IDENTITY, not liveness. `resolve_jit_entry_owner` answers "what owns
        // this ADDRESS now"; `_direct_callee_expected` says which artifact owned
        // it when the address was baked into this code. A recycled buffer makes
        // those two different bodies and the pin then SUCCEEDS on the wrong one,
        // so the old liveness-only test let the caller publish a `CALL` into a
        // method it never resolved. Compared by `artifact_id` and not by
        // pointer: the freed `Arc` allocation is recycled too, so an address
        // comparison is an ABA and reads as a match.
        let expected: Vec<(usize, u64)> = compiled._direct_callee_expected.clone();
        let expected_of = |entry: usize| -> Option<u64> {
            expected
                .iter()
                .find(|(e, _)| *e == entry)
                .map(|(_, id)| *id)
        };
        // A callee an invalidation has already retired is alive only because
        // something still holds it — typically the retirement queue. Baking a
        // call to it would publish a body that runs code the invalidation
        // withdrew. Refused whatever `CRATONVM_JIT_STRICT_CALLEE_ROOTS` says:
        // that switch is about a callee that could not be pinned, and this is
        // a correctness refusal.
        let mut retired_callee = false;
        // ONE acquisition of the owner map for the whole body, not one per
        // baked call (this runs under `mutation`, so every acquisition here is
        // also time every other publication into this cache waits). The
        // upgraded `Arc`s are only MOVED into `resolved` while the lock is held;
        // none is dropped there. That matters: a refused pin can be the last
        // owner of its body, and dropping it runs `CompiledMethod::drop`, which
        // takes this same, non-reentrant lock.
        let resolved: Vec<(usize, Option<Arc<CompiledMethod>>)> = {
            let owners = jit_entry_owners().lock();
            compiled
                ._direct_callee_entries
                .iter()
                .map(|&entry| (entry, owners.get(&entry).and_then(std::sync::Weak::upgrade)))
                .collect()
        };
        let mut roots: Vec<Arc<CompiledMethod>> = Vec::with_capacity(resolved.len());
        // Pins taken and then refused. Released through the retirement queue
        // rather than dropped: see `release_unpublished_pins`.
        let mut refused: Vec<Arc<CompiledMethod>> = Vec::new();
        let dbg_baked = x64::dbg_jitc_enabled();
        for (entry, owner) in resolved {
            let Some(owner) = owner else {
                // JIT r25 wave 2: name the refusal `put` reports only as
                // `baked-callee-retired` (an unpinnable callee, under
                // `CRATONVM_JIT_STRICT_CALLEE_ROOTS`).
                if dbg_baked {
                    eprintln!(
                        "[cratonvm-jitc] baked-callee UNROOTED entry={entry:#x}: its body was freed before this publication"
                    );
                }
                continue;
            };
            // JIT r25 (`CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE`, default ON since R25-FLIP):
            // a callee the supersede redirect retired (`superseded`, its
            // entry already the not-entrant stub that forwards to the new
            // body, and no logged invalidation withdraws it) is still
            // callable, exactly as it is for every published caller that
            // baked it; `superseded_callable` answers that, and the body
            // publishes rooting it. Any other retired callee is refused.
            if owner.retired.load(std::sync::atomic::Ordering::SeqCst)
                && !superseded_callable(owner.as_ref())
            {
                if dbg_baked {
                    eprintln!(
                        "[cratonvm-jitc] baked-callee RETIRED {} entry={entry:#x}: superseded={} not_entrant={} redefined={}",
                        owner.method_label,
                        owner.superseded.load(std::sync::atomic::Ordering::Acquire),
                        owner.is_not_entrant(),
                        owner.is_withdrawn_by_redefinition(),
                    );
                }
                retired_callee = true;
                refused.push(owner);
                continue;
            }
            match expected_of(entry) {
                // Baked with no recorded identity (a synthetic range): keep
                // the historical liveness-only answer rather than refuse a
                // body that was fine before.
                None | Some(0) => roots.push(owner),
                Some(want) if owner.artifact_id == want => roots.push(owner),
                Some(_) => {
                    REBOUND_DIRECT_CALLEES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if dbg_jit_pin_enabled() {
                        eprintln!(
                            "[jit-rebound-callee] entry {entry:#x} now belongs to a \
different artifact than the one baked into this body: its buffer was recycled before \
publication"
                        );
                    }
                    refused.push(owner);
                }
            }
        }
        compiled._direct_callee_roots = roots.into_iter().map(RetainedCode::new).collect();
        release_unpublished_pins(refused);
        if retired_callee {
            RETIRED_DIRECT_CALLEES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if dbg_jit_pin_enabled() {
                eprintln!(
                    "[jit-retired-callee] a baked direct-call target was retired by an \
invalidation before this body's publication"
                );
            }
            // Not published, so the pins this body did take are released now,
            // through the queue, instead of by the artifact's plain drop.
            // `RetainedCode` handles: dropping them releases through the queue.
            drop(std::mem::take(&mut compiled._direct_callee_roots));
            return false;
        }
        let got = compiled._direct_callee_roots.len();
        if got == wanted {
            return true;
        }
        // A baked callee entry no longer resolves to a live owner. This is the
        // use-after-free window: the callee was replaced (tier-up) or otherwise
        // retired between the compile driver baking its address and this
        // publication, so its `jit_entry_owners` Weak no longer upgrades.
        UNROOTED_DIRECT_CALLEES.fetch_add(wanted - got, std::sync::atomic::Ordering::Relaxed);
        if dbg_jit_pin_enabled() {
            eprintln!(
                "[jit-unrooted-callee] {}/{} baked direct-call targets could not be pinned",
                wanted - got,
                wanted,
            );
        }
        if strict_callee_roots_enabled() {
            // Refused: release the pins that did resolve, as above.
            // `RetainedCode` handles: dropping them releases through the queue.
            drop(std::mem::take(&mut compiled._direct_callee_roots));
            return false;
        }
        true
    }

    /// JIT r25 (`CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE`, default ON): may an
    /// unpublished body that baked a direct call into `callee`, which is now
    /// `retired`, still publish (rooting it)?
    ///
    /// **Why this exists.** `CompiledMethod::superseded` documents that a body
    /// merely displaced by a newer publication of its own key is NOT retired
    /// and that `prepare_for_publication` must ignore it. Since nio21 wave 16
    /// the supersede redirect ([`JitCache::redirect_superseded_locked`]) makes
    /// the displaced body not entrant AND stores `retired` on it, so every
    /// body still compiling when one of its baked callees tiers up or
    /// republishes is dropped at `put` as `baked-callee-retired`. A large body
    /// (`Pattern.compile()V`, 110 KB, about 1 s to compile, with callees such
    /// as `expr`, `curly`, `Start.<init>` republishing meanwhile) loses that
    /// race on nearly every attempt, exhausts `tier_fail_count` and keeps its
    /// single-pass body for the life of the process (the r23-w5 page's waves
    /// 9b and 9c; round 24's `compile` row: 8.0k instructions per op in nine
    /// runs of ten, 3.1k in the run whose C2 body published).
    ///
    /// **Why publishing is sound.** Every published caller that baked
    /// `callee` keeps calling its entry, which the redirect patched to the
    /// not-entrant stub that forwards to the current body (and, on its slow
    /// path, rebinds the caller's site in place: r23 wave 10). An unpublished
    /// caller that roots `callee` (`_direct_callee_roots`, so the entry stays
    /// mapped) is in exactly that position. What the refusal still has to
    /// keep out is an INVALIDATED callee -- a body whose assumptions broke,
    /// whose baked callers are withdrawn with it. So the answer is yes only
    /// when all of these hold:
    ///
    /// * the switch is on;
    /// * `callee.superseded`: a later publication of its own key displaced it
    ///   (an invalidation removes a body from the map; it never marks it
    ///   superseded);
    /// * `callee.is_not_entrant()`: its entry is the forwarding stub, so the
    ///   baked call cannot run withdrawn code (a refused redirect leaves the
    ///   body entrant AND unretired, and never reaches here);
    /// * it was not withdrawn by a redefinition
    ///   (`is_withdrawn_by_redefinition`);
    /// * the callee's key is known (`published_key`), and no invalidation this
    ///   cache logged after the callee's OWN compile-start stamp withdraws it
    ///   (the [`Self::dependencies_current_since`] scan, without its refusal
    ///   counter); a stamp older than the log's retained window answers no;
    /// * the callee was compiled after this cache's last flush and after the
    ///   last scoped redefinition of its class
    ///   ([`Self::compiled_since_redefinition_of`] on its `install_epoch`).
    ///   JIT r25 wave 6 (review lane W6-R): [`Self::clear_all`] empties the
    ///   invalidation log WITHOUT raising `dropped_through` (its own argument
    ///   only covers the PUBLISHING body, which its barrier refuses), and it
    ///   clears the redefinition barriers too. A superseded callee is in no
    ///   map, so the flush never marks it, and with the log empty the scan
    ///   below answered `true` for a callee compiled before the flush. That is
    ///   exactly the "older than the retained window" case, so it answers no.
    ///
    /// `caller` names the body asking, for the `CRATONVM_DBG_JITC` line only.
    pub(crate) fn superseded_callee_still_callable(&self, callee: &CompiledMethod, caller: &JitKey) -> bool {
        use std::sync::atomic::Ordering;
        if !publish_over_superseded_callee_enabled() {
            return false;
        }
        if !callee.superseded.load(Ordering::Acquire)
            || !callee.is_not_entrant()
            || callee.is_withdrawn_by_redefinition()
        {
            return false;
        }
        let Some((class_name, method_name, descriptor, class_id)) = callee.published_key.get()
        else {
            return false;
        };
        let callee_key = JitKey {
            class_name: Arc::clone(class_name),
            method_name: Arc::clone(method_name),
            descriptor: Arc::clone(descriptor),
            declaring_class_id: cratonvm_types::ClassId::new(*class_id),
        };
        let stamp = callee.invalidation_epoch;
        // JIT r25 wave 6 (W6-R): a callee from before the last flush or the
        // last scoped redefinition of its class is outside what the log can
        // still answer for (see the list above). Taken before, never under,
        // the log lock.
        let current_generation = self.compiled_since_redefinition_of(
            cratonvm_types::ClassId::new(*class_id),
            callee.install_epoch,
        );
        let callable = current_generation && {
            let log = self.invalidations.lock();
            match log.records.back() {
                None => true,
                Some(&(newest, _)) if stamp >= newest => true,
                Some(_) => {
                    !(stamp < log.dropped_through
                        || log
                            .records
                            .iter()
                            .rev()
                            .take_while(|(epoch, _)| *epoch > stamp)
                            .any(|(_, record)| {
                                record.withdraws_installed_in(self, &callee_key, callee)
                            }))
                }
            }
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] publish-over-superseded-callee {}: {}.{}{} keeps its baked call to the superseded {}.{}{}",
                if callable {
                    "ADMITTED"
                } else if !current_generation {
                    "REFUSED (the callee predates the last flush or redefinition of its class)"
                } else {
                    "REFUSED (an invalidation withdrew the callee)"
                },
                caller.class_name,
                caller.method_name,
                caller.descriptor,
                callee_key.class_name,
                callee_key.method_name,
                callee_key.descriptor,
            );
        }
        callable
    }

    pub fn put(
        &self,
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        declaring_class_id: cratonvm_types::ClassId,
        mut compiled: CompiledMethod,
    ) {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::CodeInstall);
        // TAIL9-20-2 (opt-in; empty otherwise): before the mutation lock.
        self.note_sp_nested_miss_cells(&compiled);
        let _mutation = self.mutation.lock();
        // Wave 40 (lane L2 follow-up): nothing is published while a
        // multi-class redefinition call installs. See `hold_publications`.
        if self.publication_refused_by_hold(&class_name, &method_name, &descriptor) {
            jit_cache_put_dropped(&class_name, &method_name, &descriptor, &compiled, "redefinition-hold");

            dbg_jit_put_dropped(
                &class_name,
                &method_name,
                &descriptor,
                &compiled,
                "redefinition-hold",
            );
            return;
        }
        // Interpreter round i6 wave 117, lane L2
        // (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`): a marked method's body
        // publishes RE-ENTRY-ONLY, whichever door compiled it, and never
        // displaces a live binding body (the supersede below would redirect
        // that body's baked callers to it).
        if self.is_reentry_only_key(&class_name, &method_name, &descriptor) {
            compiled.reentry_only = true;
        }
        if compiled.reentry_only {
            let h =
                compute_jit_key_hash(&class_name, &method_name, &descriptor, declaring_class_id);
            let live_binding_body = self.shards[Self::shard_index(h)]
                .methods
                .load()
                .get(&h)
                .is_some_and(|(key, live)| {
                    *key.class_name == *class_name
                        && *key.method_name == *method_name
                        && *key.descriptor == *descriptor
                        && key.declaring_class_id == declaring_class_id
                        && !live.arc().reentry_only
                        && !live
                            .arc()
                            .retired
                            .load(std::sync::atomic::Ordering::Acquire)
                });
            if live_binding_body {
                // t1 w8 ACCEPT: the seventh drop, silent until now.
                jit_cache_put_dropped(&class_name, &method_name, &descriptor, &compiled, "reentry-only-keeps-live-binding-body");
                return;
            }
        }
        if !self.publication_epoch_is_current(&compiled) {
            // Compiled against bytecode a redefinition (or a synthetic-stub
            // layout upgrade) has since replaced. Dropping it leaves the method
            // interpreted; it recompiles on a later invocation, against the
            // bytecode that is actually installed. See
            // `CompiledMethod::install_epoch`.
            jit_cache_put_dropped(&class_name, &method_name, &descriptor, &compiled, "install-epoch-moved");

            dbg_jit_put_dropped(
                &class_name,
                &method_name,
                &descriptor,
                &compiled,
                "stale-install-epoch",
            );
            return;
        }
        // Round 13 wave 4 (lane sync2): a self-locking body supplies its own
        // monitor, so it is NOT a wrapped entry, whatever the door stamped from
        // the method's `ACC_SYNCHRONIZED` bit just before this call. Here, the
        // one publication funnel, so no door can hand it out wrapped-flagged
        // (which would only cost speed) -- and none can hand a WRAPPED body out
        // unflagged, since only the self-locking backend sets the field.
        if compiled.self_locks_monitor {
            compiled.requires_wrapped_entry = false;
        }
        // Tier proposal R12-4: under `mutation`, so no publication can land
        // between this check and the store below. A single-pass body asked
        // not to replace a live optimizing one is dropped; the caller's
        // read-back then answers the optimizing body.
        if compiled.keep_live_optimizing_body && !compiled.used_ir_backend {
            let h =
                compute_jit_key_hash(&class_name, &method_name, &descriptor, declaring_class_id);
            let keeps = self.shards[Self::shard_index(h)]
                .methods
                .load()
                .get(&h)
                .is_some_and(|(key, live)| {
                    *key.class_name == *class_name
                        && *key.method_name == *method_name
                        && *key.descriptor == *descriptor
                        && key.declaring_class_id == declaring_class_id
                        && live.arc().used_ir_backend
                        && !live
                            .arc()
                            .retired
                            .load(std::sync::atomic::Ordering::Acquire)
                });
            if keeps {
                jit_cache_put_dropped(&class_name, &method_name, &descriptor, &compiled, "keeps-live-optimizing-body");

                dbg_jit_put_dropped(
                    &class_name,
                    &method_name,
                    &descriptor,
                    &compiled,
                    "live-optimizing-body-kept",
                );
                return;
            }
        }
        let h = compute_jit_key_hash(&class_name, &method_name, &descriptor, declaring_class_id);
        // Interpreter round i4 wave 92, lane L7: this publication's
        // `(method, descriptor)` bucket, moved beside the generation below.
        let publish_bucket = callee_publish_bucket(&method_name, &descriptor);
        let key = JitKey {
            class_name,
            method_name,
            descriptor,
            declaring_class_id,
        };
        // Record what this body inlined BEFORE checking the invalidation log
        // (and before it is shared). A class definition racing this publication
        // then either finds the name and scans after this publication, or
        // logged its invalidation before the check below reads the log. Both
        // run under `mutation` on this side.
        self.note_inlined_classes(&compiled);
        if !self.dependencies_are_current(&key, &compiled) {
            // An invalidation of this cache that began after this compilation
            // withdrew something the body depends on. Dropping it leaves the
            // method interpreted; it recompiles against the current hierarchy.
            // See `CompiledMethod::invalidation_epoch`.
            jit_cache_put_dropped(&key.class_name, &key.method_name, &key.descriptor, &compiled, "dependency-invalidated-since-compile");

            dbg_jit_put_dropped(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                &compiled,
                "dependency-invalidated",
            );
            return;
        }
        let superseded_callable = |callee: &CompiledMethod| -> bool {
            self.superseded_callee_still_callable(callee, &key)
        };
        if !Self::prepare_for_publication(&mut compiled, &superseded_callable) {
            // Unsafe to publish — see `prepare_for_publication`. Dropping the
            // artifact leaves the method interpreted; it is recompiled on a
            // later invocation, by which time the callee has a live body.
            jit_cache_put_dropped(&key.class_name, &key.method_name, &key.descriptor, &compiled, "baked-callee-retired (prepare_for_publication)");
            dbg_jit_put_dropped(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                &compiled,
                "baked-callee-unpinnable",
            );
            return;
        }
        if self.baked_callee_redefined_since(&mut compiled) {
            jit_cache_put_dropped(&key.class_name, &key.method_name, &key.descriptor, &compiled, "baked-callee-redefined");
            dbg_jit_put_dropped(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                &compiled,
                "baked-callee-redefined",
            );
            return;
        }
        // CRATONVM_DBG_JIT_COMPILED: one line per successfully published
        // compilation. The cheapest way to answer "is this method actually
        // running compiled?" -- an A/B whose only visible difference is
        // throughput can otherwise cost a whole session to attribute (the
        // 2026-07-26 H2 TestFreeSpace residual: java/util/BitSet silently
        // stopped being compiled once org/h2/ became JIT-eligible).
        // JIT round 21 (lane accept): printed after the LAST drop check. It
        // used to precede `prepare_for_publication` and the redefinition
        // check, so a body either of them then dropped still printed `put`
        // (the str17 `[jit-retired-callee]` drop read as a publication); every
        // drop now prints its own `dropped (<reason>)` line instead
        // ([`dbg_jit_put_dropped`]).
        if false {
            eprintln!(
                "CRATONVM_DBG_JIT_COMPILED: put {}.{}{}",
                key.class_name, key.method_name, key.descriptor
            );
        }
        // Stamp the declaring class before the artifact is shared: it is what
        // `JitEntryGuard` reads to keep this class's defining loader alive
        // while one of these frames is on a stack.
        compiled.owner_class_id = declaring_class_id.as_u32();
        // And the key, which names the method a not-entrant stub re-dispatches
        // (`CompiledMethod::make_not_entrant`), superseded or not.
        compiled.stamp_published_key(
            &key.class_name,
            &key.method_name,
            &key.descriptor,
            declaring_class_id.as_u32(),
        );
        process_jit_verdicts().clear_bail_site(
            declaring_class_id,
            &key.class_name,
            &key.method_name,
            &key.descriptor,
        );
        let counted = compiled._buffer.mark_published();
        // Immediately after the install accounting `mark_published` just did, so
        // the high-water mark sees this body. See
        // `note_jit_published_live_bytes_peak` for why the sample belongs at the
        // caller and why the subtraction inside it saturates.
        note_jit_published_live_bytes_peak();
        // The per-method half of the same accounting, gated on `mark_published`
        // having actually counted this body so the census and `installed_bodies`
        // describe the same population. See `note_jit_published_method_version`.
        if counted {
            note_jit_published_method_version(h);
        }
        let arc = Arc::new(compiled);
        // Interpreter round i1 wave 26, lane L6: reachable by a redefinition's
        // exit-poll pass after it is superseded or evicted, while a frame may
        // still run it.
        self.note_exit_poll_body(&arc);
        self.dependency_buckets.note_published(&arc);
        // Bind the identity the prologue already encoded. Until this runs the
        // id resolves to `None`, so a frame entered during compilation — there
        // cannot be one, but the ordering is what makes that safe rather than
        // lucky — degrades to the old decode instead of naming a half-built
        // artifact.
        arc.note_bound_arc();
        bind_compile_id(arc.compile_id, Arc::as_ptr(&arc) as usize);
        // Round 11 wave 19 (lane lhm): this body's inline caches record what
        // they publish on the callee, for the supersede walk below.
        if self.ic_supersede_retarget {
            bind_inline_cache_holders(&arc);
        }
        // Stage 5 — register this method's code range for the GC RBP-chain
        // walker. Enabled when the precise gate is on (the registry is consulted
        // by `remap_active_jit_frames`) OR when the BUG-03 cross-thread STW JIT
        // root scan is on (which classifies a forcibly-stopped peer's `Rip` as
        // in-JIT-or-not by looking it up in this registry — an empty registry
        // would make every peer look "not in JIT" and the scan a no-op). The
        // default path keeps zero bookkeeping overhead.
        if crate::x64::precise_jit_maps_enabled() || xt_jit_root_scan_enabled() {
            register_jit_code_range_owned(arc.entry_ptr() as usize, arc.code_len(), &arc);
        }
        // DBG (spring-bug-11): record entry→name so a crash report can name the
        // faulting JIT method. Gated; no overhead unless CRATONVM_DBG_JIT_NAMES.
        if jit_names_enabled() {
            register_jit_method_name(
                arc.entry_ptr() as usize,
                arc.code_len(),
                format!("{}.{}{}", key.class_name, key.method_name, key.descriptor),
            );
        }
        // JVMTI `CompiledMethodLoad` (interpreter round i2 wave 63, lane L1).
        crate::code_events::publish_method(
            arc.entry_ptr() as usize,
            arc.code_len(),
            &key.class_name,
            &key.method_name,
            &key.descriptor,
        );
        // Profiler/debugger symbols (perf map, jitdump, GDB); no-op unless enabled.
        crate::code_events::publish(arc.entry_ptr() as usize, arc.code_len(), || {
            let tier = if arc.used_ir_backend {
                crate::code_events::CodeTier::C2
            } else {
                crate::code_events::CodeTier::C1
            };
            crate::code_events::method_name(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                tier,
            )
        });
        jit_entry_owners()
            .lock()
            .insert(arc.entry_ptr() as usize, Arc::downgrade(&arc));
        // Round 9 wave 7 (`mutrec7`): point every cycle-edge cell waiting for
        // this method at the new body, and register (and, when its target is
        // already published, fill) this body's own. See `JitCycleEdgeCell`.
        // A re-entry-only body is no cycle edge's target: those cells are
        // compiled callers' binds (`JitCycleEdgeCell`).
        if !arc.reentry_only {
            self.publish_cycle_cells(h, &key, &arc);
        }
        // Round 9 wave 9 (`evict9`): fill this body's retire cells from the
        // callee roots `prepare_for_publication` just pinned.
        self.publish_retire_cells(&arc);
        // nio21 wave 16 (lane L1): the new body, for the supersede redirect
        // below. A `RetainedCode`, so even a drop that raced a withdrawal
        // releases through the retirement queue.
        let new_body = RetainedCode::new(Arc::clone(&arc));
        let shard = &self.shards[Self::shard_index(h)];
        let mut next = (**shard.methods.load()).clone();
        let superseded = next.insert(h, (key, RetainedCode::new(arc)));
        shard.methods.store(Arc::new(next));
        // The artifact this publication replaces may still be EXECUTING: a
        // mutator inside its body, or inside a callee it roots via
        // `_direct_callee_roots`. Dropping it here runs
        // `ExecutableBuffer::drop`, which unmaps the code under that thread —
        // observed as a SIGSEGV with `pc == addr` MID-body (offset 0x553 of a
        // 1732-byte `maybeThrow`) immediately after this site's `[jit-unmap]`
        // line for the same page. Hand it to the same quiescence queue the
        // inline caches use: `defer_jit_owner` drops immediately when no JIT
        // execution is in flight (the common case, so no retention change) and
        // otherwise holds the `Arc` until `ACTIVE_JIT_EXECUTIONS` reaches zero.
        // `SUPERSEDED`: this publication displaced an earlier body of the
        // same key, which is the definition of the code.
        //
        // Marked BEFORE the generation bump, with release ordering: a memo
        // hit that has seen this publication's bump also sees the flag. A hit
        // that races ahead of it runs the replaced body once more, which is
        // still correct code, kept mapped by the memo's own `RetainedCode`.
        if let Some((_, cm)) = &superseded {
            cm.arc()
                .superseded
                .store(true, std::sync::atomic::Ordering::Release);
            self.superseded_bodies.note(cm.arc());
            // Round 11 wave 19 (lane lhm,
            // `r11w18-hashmap-inline-caches-keep-a-superseded-callee`): the
            // inline caches that publish the displaced body stop publishing
            // it, AFTER the flag, so a racing publication either lands in the
            // list this walk takes or sees the flag and withdraws itself.
            // Without this a site kept the first body it cached for the life
            // of its own body (a C1 callee stayed C1 at every caller that
            // cached it before its C2 body landed).
            if self.ic_supersede_retarget {
                cm.arc().retarget_superseded_inline_caches();
            }
            // Round 12 wave 2 (lane calls, W2-1): the shared megamorphic table
            // stops naming the displaced body, after the flag (a racing
            // publication re-reads it: `install` refuses a retired owner, and
            // the helper's hit path refuses a superseded one).
            if let Some(table) = self.mega_dispatch_table_if_any() {
                table.retire_entry(cm.arc().entry_ptr() as usize);
            }
            // JIT r25 wave 10 (W10-5, `CRATONVM_JIT_LAMBDA_ADAPTER_SUPERSEDE`):
            // and every lambda-adapter thunk that tail-jumps into the
            // displaced body, with the caches that hold it (after the flag,
            // as above).
            self.retarget_superseded_lambda_adapters(cm.arc());
            // Round 12 wave 2 (lane calls, proposal C12-4): the census that
            // decides C12-1 (forward a superseded body's entry to its
            // successor). Diagnostic only, one walk per supersede.
            if crate::x64::dbg_jitc_enabled() {
                self.note_supersede_baked_callers(cm.arc());
            }
            // str16: a cycle forward to the displaced body lets go of it, so
            // the cycle it closed cannot keep it mapped.
            // Cast: an address, the registry's key.
            self.release_cycle_forwards(Some(&[cm.arc().entry_ptr() as usize][..]));
        }
        // nio21 wave 16 (lane L1): a caller that BAKED the displaced body's
        // entry (a direct bind, a splice's call, an OSR body) reaches the new
        // body through the displaced one's not-entrant stub -- including a
        // single-pass body displaced by another single-pass body, whose
        // callers otherwise kept it (and its entry counter, which the drain
        // never reads) for life. After the flag and the inline-cache walk,
        // still under `mutation`.
        self.redirect_superseded_locked(
            h,
            superseded.as_ref().map(|(_, cm)| cm.arc()),
            new_body.arc(),
        );
        drop(new_body);
        let had_superseded = superseded.is_some();
        retire_withdrawn_body(
            superseded.map(|(_, cm)| cm.arc().clone()),
            retire_reason::SUPERSEDED,
        );
        if had_superseded {
            if let Some(ep) = self.reflect_epoch.get() {
                ep.fetch_add(1, std::sync::atomic::Ordering::Release);
            }
        }
        // T2.2 — bump on EVERY publication, not just replacements. A first-time
        // insertion is exactly the event the interpreter's negative
        // "no compiled body for this method" memo
        // (`CachedBytecodeMethod::jit_probe_generation`) must observe. See
        // `jit_cache_generation`.
        //
        // A replacement moves only the PUBLISH half (round 11 wave 11). The
        // retire half exists for two things, and neither needs it here: a
        // freed `JitInvokeInfo` address (freed only when a body DROPS, and
        // `Drop for CompiledMethod` bumps the retire half itself), and a
        // positive memo naming the replaced body (the two memos that hold a
        // body check `superseded` at every hit and drop such an entry). Moving
        // it on every tier-up emptied every thread's positive dispatch memos
        // once per replaced method. `put_osr` and the invalidation and
        // withdrawal paths still move it.
        self.bump_generation(false);
        self.bump_callee_publish_bucket(publish_bucket);
    }

    /// Proposal C12-4 (round 12 wave 2, lane `calls`): under
    /// `CRATONVM_DBG_JITC`, one line per supersede naming how many published
    /// bodies (method-entry and OSR) still BAKE a direct call into the body it
    /// just displaced (`_direct_callee_roots`). Those callers keep running the
    /// old body until they recompile themselves, which is the population
    /// proposal C12-1 (forward the old entry to the new body) would serve; the
    /// sum over a run says whether C12-1 is worth its risk. An upper bound: a
    /// caller that recompiles later stops calling the old body. Called with
    /// `mutation` held, after the map swap, so the displaced body is in no map.
    pub(crate) fn note_supersede_baked_callers(&self, old: &CompiledMethod) {
        let mut callers = 0usize;
        let mut osr_callers = 0usize;
        for shard in self.shards.iter() {
            for (osr, map) in [
                (false, shard.methods.load()),
                (true, shard.osr_methods.load()),
            ] {
                for (_hash, (_key, cm)) in map.iter() {
                    if cm
                        ._direct_callee_roots
                        .iter()
                        .any(|root| root.artifact_id == old.artifact_id)
                    {
                        if osr {
                            osr_callers += 1;
                        } else {
                            callers += 1;
                        }
                    }
                }
            }
        }
        eprintln!(
            "[cratonvm-jitc] supersede-baked-callers {} old_tier={} callers={callers} \
             osr_callers={osr_callers}",
            old.method_label,
            if old.used_ir_backend { "C2" } else { "C1" },
        );
    }

    /// `(installed, released)` not-entrant forwards across a reference cycle
    /// in this cache so far ([`Self::forward_not_entrant`]; diagnostic).
    pub fn cycle_forward_census(&self) -> (u64, u64) {
        (
            self.cycle_forwards_installed
                .load(std::sync::atomic::Ordering::Relaxed),
            self.cycle_forwards_released
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Point the not-entrant stub of `record` at `target`, the body now
    /// published under the record's key (`jit_not_entrant_entry`'s forward;
    /// strings round, `str16`). A forward that closes a reference cycle
    /// ([`NotEntrantRecord::forward_to_releasable`]) is registered here, by
    /// the target's entry, and released when the target is withdrawn
    /// ([`Self::release_cycle_forwards`]) -- the discipline the
    /// [`JitCycleEdgeCell`]s keep for the same cycles. Under `mutation`, so a
    /// withdrawal cannot slip between the install and the registration: the
    /// target is checked still published inside it. Returns whether the stub
    /// now forwards.
    pub fn forward_not_entrant(
        &self,
        record: &NotEntrantRecord,
        target: &Arc<CompiledMethod>,
    ) -> bool {
        if !record.may_forward_to(target) {
            return false;
        }
        let _mutation = self.mutation.lock();
        // Withdrawn since the caller read it: no forward this time.
        if target.retired.load(std::sync::atomic::Ordering::SeqCst)
            || target.superseded.load(std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        match record.forward_to_releasable(target) {
            ForwardOutcome::Refused => false,
            ForwardOutcome::Installed => true,
            ForwardOutcome::InstalledCycle => {
                self.cycle_forwards_installed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (entry, artifact_id) = record.patched_body();
                let mut registry = self.cycle_forwards.lock();
                let bucket = registry.entry(target.entry_ptr() as usize).or_default();
                if !bucket.contains(&(entry, artifact_id)) {
                    bucket.push((entry, artifact_id));
                }
                true
            }
        }
    }

    /// Release the cycle forwards ([`Self::forward_not_entrant`]) whose
    /// target is one of `targets` (every one, for `None`). Called with
    /// `mutation` held, after the withdrawn bodies were marked retired or
    /// superseded, beside [`Self::invalidate_cycle_cells`]. A patched body
    /// that is gone already took its record (and the forward) with it.
    pub(crate) fn release_cycle_forwards(&self, targets: Option<&[usize]>) {
        let mut registry = self.cycle_forwards.lock();
        if registry.is_empty() {
            return;
        }
        let doomed: Vec<(usize, Vec<(usize, u64)>)> = match targets {
            Some(targets) => targets
                .iter()
                .filter_map(|t| registry.remove(t).map(|v| (*t, v)))
                .collect(),
            None => registry.drain().collect(),
        };
        drop(registry);
        if doomed.is_empty() {
            return;
        }
        let mut released: Vec<RetainedCode> = Vec::new();
        for (target_entry, patched) in doomed {
            for (entry, artifact_id) in patched {
                let body = jit_entry_owners()
                    .lock()
                    .get(&entry)
                    .and_then(std::sync::Weak::upgrade);
                let Some(body) = body.filter(|b| b.artifact_id == artifact_id) else {
                    continue;
                };
                if let Some(record) = body.not_entrant.get() {
                    released.extend(record.release_forwards_to(target_entry));
                }
                // Released through `RetainedCode`: the patched body may be its
                // own last owner by now.
                drop(RetainedCode::new(body));
            }
        }
        self.cycle_forwards_released
            .fetch_add(released.len() as u64, std::sync::atomic::Ordering::Relaxed);
        // Each owner leaves through the retirement queue (`RetainedCode`'s
        // drop): a thread may still be between the stub's word load and its
        // flag tests.
        drop(released);
    }

    /// [`Self::put`]'s [`JitCycleEdgeCell`] half (round 9 wave 7, `mutrec7`).
    /// Called with `mutation` held, after `body` is fully published (owner,
    /// code range and identity registered) and before it enters the map.
    ///
    /// 1. Every live cell waiting for `key` is pointed at `body` (the newest
    ///    suitable body wins; an unsuitable one leaves the cell as it was).
    /// 2. `body`'s own cells are registered, and each one whose target is
    ///    already published is filled now. In the eager-compile order that
    ///    creates them the target is the caller's compile-stack ANCESTOR,
    ///    which publishes later and fills them through step 1.
    pub(crate) fn publish_cycle_cells(&self, h: u64, key: &JitKey, body: &Arc<CompiledMethod>) {
        let own = &body._jit_cycle_cells;
        let mut registry = self.cycle_cells.lock();
        if own.is_empty() && registry.is_empty() {
            return;
        }
        let mut upgraded: Vec<Arc<JitCycleEdgeCell>> = Vec::new();
        let mut now_empty = false;
        if let Some(waiting) = registry.get_mut(&h) {
            waiting.retain(|w| w.strong_count() > 0);
            for w in waiting.iter() {
                if let Some(cell) = w.upgrade() {
                    if cell.targets(key) {
                        cell.install(body);
                    }
                    upgraded.push(cell);
                }
            }
            now_empty = waiting.is_empty();
        }
        if now_empty {
            registry.remove(&h);
        }
        for cell in own {
            // A retire cell is registered by `publish_retire_cells`, by its
            // pinned entry; it waits for no key.
            if cell.pinned_entry != 0 {
                continue;
            }
            registry
                .entry(cell.target_hash())
                .or_default()
                .push(Arc::downgrade(cell));
            if let Some(target) = self.get(
                &cell.class_name,
                &cell.method_name,
                &cell.descriptor,
                cell.declaring_class_id,
            ) {
                cell.install(&target);
            }
        }
        drop(registry);
        release_cycle_cells(upgraded);
    }

    /// t1 wave 2, lane jit2 (B5, [`jit_osr_late_static_cell_enabled`]):
    /// [`Self::publish_osr_body_late_cells`] for a body this cache never
    /// publishes (the VM's optimizing OSR bodies, which its own memo keeps),
    /// under `mutation` as a publication would hold it, so a concurrent
    /// [`Self::invalidate_cycle_cells`] cannot be overtaken by the fill.
    /// Called by the VM only under the switch.
    pub fn register_unpublished_body_late_cells(&self, body: &Arc<CompiledMethod>) {
        if body._jit_cycle_cells.iter().all(|c| c.pinned_entry != 0) {
            return;
        }
        let _mutation = self.mutation.lock();
        self.publish_osr_body_late_cells(body);
    }

    /// t1 wave 2, lane jit2 (B5, [`jit_osr_late_static_cell_enabled`]): register
    /// an OSR body's OWN key-waiting cells (`pinned_entry == 0`: the late-bind
    /// cells the single-pass OSR door plants, and the late / cycle-edge cells
    /// the IR table builder plants) and fill each whose target is already
    /// published -- the second half of [`Self::publish_cycle_cells`]. The first
    /// half (fill the cells waiting for THIS key) does not apply: an OSR body is
    /// never in the method-entry map, so it is no cell's target. Called with
    /// `mutation` held: by [`Self::put_osr`] after `prepare_for_publication`
    /// accepted the body, or through [`Self::register_unpublished_body_late_cells`];
    /// that is the ordering [`Self::invalidate_cycle_cells`] relies on. A body
    /// with no such cell takes no lock.
    pub(crate) fn publish_osr_body_late_cells(&self, body: &Arc<CompiledMethod>) {
        if body._jit_cycle_cells.iter().all(|c| c.pinned_entry != 0) {
            return;
        }
        let mut registry = self.cycle_cells.lock();
        for cell in &body._jit_cycle_cells {
            if cell.pinned_entry != 0 {
                continue;
            }
            let bucket = registry.entry(cell.target_hash()).or_default();
            bucket.retain(|w| w.strong_count() > 0);
            bucket.push(Arc::downgrade(cell));
            if let Some(target) = self.get(
                &cell.class_name,
                &cell.method_name,
                &cell.descriptor,
                cell.declaring_class_id,
            ) {
                cell.install(&target);
            }
        }
    }

    /// Clear every registered [`JitCycleEdgeCell`] whose current target is in
    /// `targets` (or every cell, for `None`). Called with `mutation` held,
    /// after the doomed bodies are marked retired, so a concurrent fill cannot
    /// land behind it: fills run under `mutation` too, and
    /// [`JitCycleEdgeCell::admits`] refuses a retired body.
    ///
    /// Walks the registry rather than the cached bodies: the closing caller's
    /// artifact may already be superseded (and so out of the maps) while an
    /// older caller still bakes a direct call into it and runs it.
    pub(crate) fn invalidate_cycle_cells(&self, targets: Option<&std::collections::HashSet<usize>>) {
        let mut registry = self.cycle_cells.lock();
        if registry.is_empty() {
            return;
        }
        let mut upgraded: Vec<Arc<JitCycleEdgeCell>> = Vec::new();
        registry.retain(|_, waiting| {
            waiting.retain(|w| w.strong_count() > 0);
            !waiting.is_empty()
        });
        for waiting in registry.values() {
            for w in waiting {
                if let Some(cell) = w.upgrade() {
                    match targets {
                        Some(targets) => cell.invalidate_target(targets),
                        None => cell.clear(),
                    }
                    upgraded.push(cell);
                }
            }
        }
        drop(registry);
        release_cycle_cells(upgraded);
    }

    /// Register `body`'s RETIRE cells and fill each from `body`'s own pinned
    /// callee roots (round 9 wave 9, `evict9`; see [`retirable_direct_call`]).
    /// Called by [`Self::put`] and [`Self::put_osr`] with `mutation` held, after
    /// `prepare_for_publication` accepted the body.
    ///
    /// The cell is filled with the root whose entry is its pinned one --
    /// exactly the body the caller baked, kept alive by the caller itself.
    /// A body `prepare_for_publication` could not root (or rooted as another
    /// artifact) leaves the cell at `0`: the site dispatches, which is always
    /// correct. An invalidation that already retired the callee refused the
    /// whole publication there, and `admits` refuses a retired body anyway.
    pub(crate) fn publish_retire_cells(&self, body: &CompiledMethod) {
        if body._jit_cycle_cells.iter().all(|c| c.pinned_entry == 0) {
            return;
        }
        let mut registry = self.retire_cells.lock();
        for cell in &body._jit_cycle_cells {
            let pinned = cell.pinned_entry;
            if pinned == 0 {
                continue;
            }
            let bucket = registry.entry(pinned).or_default();
            bucket.retain(|w| w.strong_count() > 0);
            bucket.push(Arc::downgrade(cell));
            if let Some(root) = body
                ._direct_callee_roots
                .iter()
                .find(|r| r.entry_ptr() as usize == pinned)
            {
                cell.install(root.arc());
            }
        }
    }

    /// Register a rebound RETIRE cell under `new_entry` so a later
    /// invalidation of `new_entry` clears it (`JitCache::rebind_stale_call_site`).
    pub(crate) fn note_rebound_retire_cell(
        &self,
        new_entry: usize,
        cell: &Arc<JitCycleEdgeCell>,
    ) {
        let mut registry = self.retire_cells.lock();
        let bucket = registry.entry(new_entry).or_default();
        bucket.retain(|w| w.strong_count() > 0);
        bucket.push(Arc::downgrade(cell));
    }

    /// Clear every RETIRE cell pinned to one of `targets` (or every one, for
    /// `None`). Called with `mutation` held, after the doomed bodies are marked
    /// retired -- beside [`Self::invalidate_cycle_cells`], for the same reason.
    pub(crate) fn invalidate_retire_cells(&self, targets: Option<&std::collections::HashSet<usize>>) {
        let mut registry = self.retire_cells.lock();
        if registry.is_empty() {
            return;
        }
        let doomed: Vec<std::sync::Weak<JitCycleEdgeCell>> = match targets {
            Some(targets) => targets
                .iter()
                .filter_map(|entry| registry.remove(entry))
                .flatten()
                .collect(),
            None => registry.drain().flat_map(|(_, cells)| cells).collect(),
        };
        drop(registry);
        let upgraded: Vec<Arc<JitCycleEdgeCell>> =
            doomed.iter().filter_map(std::sync::Weak::upgrade).collect();
        for cell in &upgraded {
            cell.clear();
        }
        release_cycle_cells(upgraded);
    }

    /// Clear every RETIRE cell whose callee is `(class, method, descriptor)`,
    /// whatever body it is pinned to (round 9 wave 9, `evict9`).
    ///
    /// [`Self::remove`] withdraws only the method's CURRENT body; a caller
    /// that baked an older one -- superseded by a tier-up, so never retired --
    /// would keep running it after the method was evicted for misbehaving
    /// (the C2 supersede leaves the C1 body callable). Any loader's same-named
    /// method matches: a spurious clear only sends a site to dispatch.
    pub(crate) fn invalidate_retire_cells_named(&self, class_name: &str, method_name: &str, descriptor: &str) {
        let _mutation = self.mutation.lock();
        let mut registry = self.retire_cells.lock();
        if registry.is_empty() {
            return;
        }
        let mut upgraded: Vec<Arc<JitCycleEdgeCell>> = Vec::new();
        registry.retain(|_, cells| {
            cells.retain(|w| match w.upgrade() {
                Some(cell) => {
                    let named = cell.retires_with(class_name, method_name, descriptor);
                    upgraded.push(cell);
                    !named
                }
                None => false,
            });
            !cells.is_empty()
        });
        drop(registry);
        for cell in &upgraded {
            if cell.retires_with(class_name, method_name, descriptor) {
                cell.clear();
            }
        }
        release_cycle_cells(upgraded);
    }

    /// Publish an OSR body without superseding the method-entry body.
    pub fn put_osr(
        &self,
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        declaring_class_id: cratonvm_types::ClassId,
        mut compiled: CompiledMethod,
    ) {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::CodeInstall);
        // TAIL9-20-2 (opt-in; empty otherwise), as `put`.
        self.note_sp_nested_miss_cells(&compiled);
        let _mutation = self.mutation.lock();
        debug_assert!(compiled.compiled_via_osr);
        // As `put` (wave 40, lane L2 follow-up; `hold_publications`).
        if self.publication_refused_by_hold(&class_name, &method_name, &descriptor) {
            jit_cache_put_dropped(&class_name, &method_name, &descriptor, &compiled, "osr: redefinition-hold");

            dbg_jit_put_dropped(
                &class_name,
                &method_name,
                &descriptor,
                &compiled,
                "osr; redefinition-hold",
            );
            return;
        }
        // Same gate as `put` — an OSR body compiled against retired bytecode is
        // if anything worse, since it is entered into a loop already running.
        if !self.publication_epoch_is_current(&compiled) {
            jit_cache_put_dropped(&class_name, &method_name, &descriptor, &compiled, "osr: install-epoch-moved");

            dbg_jit_put_dropped(
                &class_name,
                &method_name,
                &descriptor,
                &compiled,
                "osr; stale-install-epoch",
            );
            return;
        }
        let h = compute_jit_key_hash(&class_name, &method_name, &descriptor, declaring_class_id);
        // Interpreter round i4 wave 92, lane L7: this publication's
        // `(method, descriptor)` bucket, moved beside the generation below.
        let publish_bucket = callee_publish_bucket(&method_name, &descriptor);
        let key = JitKey {
            class_name,
            method_name,
            descriptor,
            declaring_class_id,
        };
        // Record what this body inlined BEFORE checking the invalidation log
        // (and before it is shared). A class definition racing this publication
        // then either finds the name and scans after this publication, or
        // logged its invalidation before the check below reads the log. Both
        // run under `mutation` on this side.
        self.note_inlined_classes(&compiled);
        if !self.dependencies_are_current(&key, &compiled) {
            // An invalidation of this cache that began after this compilation
            // withdrew something the body depends on. Dropping it leaves the
            // method interpreted; it recompiles against the current hierarchy.
            // See `CompiledMethod::invalidation_epoch`.
            jit_cache_put_dropped(&key.class_name, &key.method_name, &key.descriptor, &compiled, "osr: dependency-invalidated-since-compile");

            dbg_jit_put_dropped(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                &compiled,
                "osr; dependency-invalidated",
            );
            return;
        }
        let superseded_callable = |callee: &CompiledMethod| -> bool {
            self.superseded_callee_still_callable(callee, &key)
        };
        if !Self::prepare_for_publication(&mut compiled, &superseded_callable) {
            // Unsafe to publish — see `prepare_for_publication`. Dropping the
            // artifact leaves the method interpreted; it is recompiled on a
            // later invocation, by which time the callee has a live body.
            jit_cache_put_dropped(&key.class_name, &key.method_name, &key.descriptor, &compiled, "osr: baked-callee-retired (prepare_for_publication)");
            dbg_jit_put_dropped(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                &compiled,
                "osr; baked-callee-unpinnable",
            );
            return;
        }
        if self.baked_callee_redefined_since(&mut compiled) {
            jit_cache_put_dropped(&key.class_name, &key.method_name, &key.descriptor, &compiled, "osr: baked-callee-redefined");
            dbg_jit_put_dropped(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                &compiled,
                "osr; baked-callee-redefined",
            );
            return;
        }
        // CRATONVM_DBG_JIT_COMPILED: one line per successfully published
        // compilation. The cheapest way to answer "is this method actually
        // running compiled?" -- an A/B whose only visible difference is
        // throughput can otherwise cost a whole session to attribute (the
        // 2026-07-26 H2 TestFreeSpace residual: java/util/BitSet silently
        // stopped being compiled once org/h2/ became JIT-eligible).
        // JIT round 21 (lane accept): after the last drop check, as in `put`.
        if false {
            eprintln!(
                "CRATONVM_DBG_JIT_COMPILED: osr {}.{}{}",
                key.class_name, key.method_name, key.descriptor
            );
        }
        // See the matching notes in `put`.
        compiled.owner_class_id = declaring_class_id.as_u32();
        compiled.stamp_published_key(
            &key.class_name,
            &key.method_name,
            &key.descriptor,
            declaring_class_id.as_u32(),
        );
        process_jit_verdicts().clear_bail_site(
            declaring_class_id,
            &key.class_name,
            &key.method_name,
            &key.descriptor,
        );
        let counted = compiled._buffer.mark_published();
        // Immediately after the install accounting `mark_published` just did, so
        // the high-water mark sees this body. See
        // `note_jit_published_live_bytes_peak` for why the sample belongs at the
        // caller and why the subtraction inside it saturates.
        note_jit_published_live_bytes_peak();
        // An OSR body is a version of the SAME method as its method-entry twin,
        // under the same key hash and in the same census table. `installs`
        // counts both, and `versions_per_method` is the ratio of that figure to
        // the census, so splitting them here would make the ratio mean nothing.
        // See `CompilationCensusState`.
        if counted {
            note_jit_published_method_version(h);
        }
        let arc = Arc::new(compiled);
        // Interpreter round i1 wave 26, lane L6: as in `put`.
        self.note_exit_poll_body(&arc);
        self.dependency_buckets.note_published(&arc);
        // Bind the identity the prologue already encoded. Until this runs the
        // id resolves to `None`, so a frame entered during compilation — there
        // cannot be one, but the ordering is what makes that safe rather than
        // lucky — degrades to the old decode instead of naming a half-built
        // artifact.
        arc.note_bound_arc();
        bind_compile_id(arc.compile_id, Arc::as_ptr(&arc) as usize);
        // Round 11 wave 19 (lane lhm): as in `put`. An OSR body is the
        // long-running caller whose inline caches most need to follow a
        // callee's tier-up.
        if self.ic_supersede_retarget {
            bind_inline_cache_holders(&arc);
        }
        if crate::x64::precise_jit_maps_enabled() || xt_jit_root_scan_enabled() {
            register_jit_code_range_owned(arc.entry_ptr() as usize, arc.code_len(), &arc);
        }
        if jit_names_enabled() {
            register_jit_method_name(
                arc.entry_ptr() as usize,
                arc.code_len(),
                format!(
                    "{}.{}{} [osr]",
                    key.class_name, key.method_name, key.descriptor
                ),
            );
        }
        // JVMTI `CompiledMethodLoad` (interpreter round i2 wave 63, lane L1).
        crate::code_events::publish_method(
            arc.entry_ptr() as usize,
            arc.code_len(),
            &key.class_name,
            &key.method_name,
            &key.descriptor,
        );
        // Profiler/debugger symbols (perf map, jitdump, GDB); no-op unless enabled.
        crate::code_events::publish(arc.entry_ptr() as usize, arc.code_len(), || {
            let tier = crate::code_events::CodeTier::Osr(arc.osr_compiled_entry_pc);
            crate::code_events::method_name(
                &key.class_name,
                &key.method_name,
                &key.descriptor,
                tier,
            )
        });
        jit_entry_owners()
            .lock()
            .insert(arc.entry_ptr() as usize, Arc::downgrade(&arc));
        // Round 9 wave 9 (`evict9`): an OSR body is THE long-running caller a
        // retire cell exists for (the OSR door plans them through
        // `retirable_direct_call`). Filled here as in `put`.
        self.publish_retire_cells(&arc);
        // t1 jit2 (B5, `CRATONVM_JIT_OSR_LATE_STATIC_CELL`): its late-bind
        // cells wait for their callees' publications.
        self.publish_osr_body_late_cells(&arc);
        let shard = &self.shards[Self::shard_index(h)];
        let mut next = (**shard.osr_methods.load()).clone();
        let superseded = next.insert(h, (key, RetainedCode::new(arc)));
        shard.osr_methods.store(Arc::new(next));
        // Round 15 wave 5 (lane vh7; G1 of
        // `r15w4-vh6-review-findings-CLOSED-20260930.md`): marked as `put` marks a
        // displaced method-entry body, before the generation bump below. An
        // OSR body is never an inline-cache or mega-table target, so the flag
        // is all `put`'s supersede block owes it.
        if let Some((_, cm)) = &superseded {
            {
                cm.arc()
                    .superseded
                    .store(true, std::sync::atomic::Ordering::Release);
                self.superseded_bodies.note(cm.arc());
            }
        }
        // Same reasoning as `put` — an OSR body is, if anything, more likely to
        // be mid-execution when it is replaced.
        // `SUPERSEDED`: this publication displaced an earlier body of the
        // same key, which is the definition of the code.
        let replaced = superseded.is_some();
        retire_withdrawn_body(
            superseded.map(|(_, cm)| cm.arc().clone()),
            retire_reason::SUPERSEDED,
        );
        // T2.2 — unconditional, for the same reason as `put` above.
        self.bump_generation(replaced);
        self.bump_callee_publish_bucket(publish_bucket);
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.methods.load().len() + shard.osr_methods.load().len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every published method-entry body whose compiled prologue count MOVED
    /// since the previous call, as `(class, method, descriptor, class_id,
    /// count)`. Each call consumes the "moved" state
    /// ([`CompiledMethod::entry_count_if_moved`]), so this has one production
    /// reader, the VM's drain.
    ///
    /// This is the READ half of the compiled-entry invocation counter; the write
    /// half is two instructions in `x64::frames::emit_prologue`. The VM feeds
    /// each row to `TieredCompilationManager::on_observed_invocations` as its
    /// `observed_count`, which is the parameter that exists precisely for an
    /// externally-observed count — see [`crate::entry_counter`] for why the
    /// manager's own count is dead once a C1 body is published.
    ///
    /// ## Why this is cheap enough to call on a timer
    ///
    /// Empty and allocation-free the instant the counter is disarmed
    /// (`CRATONVM_JIT_ENTRY_COUNTER=0`; it is default-on): the flag test comes
    /// first. When armed, the walk takes NO
    /// lock — each shard's method map is an `arc_swap` snapshot, the same
    /// structure `get` reads on the dispatch fast path — and clones only
    /// `Arc<str>` handles, never string data. Rows that have not moved are
    /// skipped, so a cache full of cold bodies produces an empty vector.
    ///
    /// ## Why it returns owned rows instead of taking a callback
    ///
    /// The caller's next move is to take the tiering manager's global `methods`
    /// mutex, once per row. Doing that from inside this walk would hold an
    /// `arc_swap` guard across a lock that publication also wants, which is how
    /// a lock-order inversion gets written by accident. Materialising the rows
    /// first makes the two phases structurally incapable of overlapping, and the
    /// vector is bounded by the number of published bodies that have actually
    /// been entered.
    ///
    /// OSR artifacts are deliberately excluded. An OSR body is entered past the
    /// method-entry prologue by construction (`osr_entry.rs`), so its counter
    /// can only ever read zero, and `request_osr` is the door that serves loop
    /// entry anyway.
    ///
    /// The sixth field (HANDLER8-2, JIT round 19 wave 9, lane handler9) is the
    /// row's RAW back-edge word, read beside the weighted count, which the VM
    /// hands to `on_observed_invocations_with_back_edges` for the tier census
    /// (`drained-be=`); no decision reads it.
    #[allow(clippy::type_complexity)]
    pub fn snapshot_entry_counts(
        &self,
    ) -> Vec<(
        Arc<str>,
        Arc<str>,
        Arc<str>,
        cratonvm_types::ClassId,
        u64,
        u64,
    )> {
        if !crate::entry_counter::entry_counter_enabled() {
            return Vec::new();
        }
        let mut rows = Vec::new();
        for shard in self.shards.iter() {
            for (key, cm) in shard.methods.load().values() {
                // Only rows whose count moved since the previous drain: an
                // unmoved row cost the manager's `methods` mutex and two key
                // clones for a count it already has.
                let Some(count) = cm.entry_count_if_moved() else {
                    continue;
                };
                rows.push((
                    Arc::clone(&key.class_name),
                    Arc::clone(&key.method_name),
                    Arc::clone(&key.descriptor),
                    key.declaring_class_id,
                    count,
                    cm.entry_back_edges(),
                ));
            }
        }
        rows
    }

    /// Retire the bodies of this cache that have not been entered since the
    /// previous sweep — RT-8's cold-body sweeper.
    ///
    /// # Why this exists
    ///
    /// Without it, code-cache exhaustion is a permanent cliff. A long-running
    /// application that loads and unloads generations of classes fills the cache
    /// with live-but-cold bodies; the first method to go hot afterwards is
    /// refused by [`jit_code_cache_at_capacity`] and stays interpreted for the
    /// life of the process, while code that last ran an hour ago stays mapped.
    /// `jit/tests/code_cache_cap.rs` already pins the property this depends on:
    /// the refusal is not sticky, so one byte of headroom admits again.
    ///
    /// # The policy, stated honestly
    ///
    /// This is **least-recently-entered, approximated by "not entered at all
    /// since the last sweep"**. It is not LRU, and LRU is not implementable
    /// from this tree — see `platform.rs`'s REVIEW-NOTE 8: no `MethodState`, no
    /// `CompiledMethod` and no arena block records a last-use time, and
    /// `last_compile_time_ms` is a duration. What this policy adds over
    /// "least-invoked" (the other implementable one, and a much worse one, in
    /// which a method that ran a million times an hour ago outranks one running
    /// now forever) is the DIFFERENCE between two readings, which is a recency
    /// signal built out of a monotonic counter and no clock at all.
    ///
    /// # Three couplings that make it safe, and one limit it does not fix
    ///
    ///  * **It needs `CRATONVM_JIT_ENTRY_COUNTER`.** That counter is this
    ///    tree's only per-artifact usage signal (default-on, with a kill
    ///    switch), so with it off every count reads zero, every body looks equally cold and the
    ///    second sweep would flush the entire cache. Refused explicitly rather
    ///    than left to produce that, because "swept everything" and "swept the
    ///    cold ones" are indistinguishable in the counters afterwards.
    ///  * **OSR artifacts are never candidates.** An OSR body is entered past
    ///    the method-entry prologue by construction (`osr_entry.rs`), so its
    ///    counter can only ever read zero — exactly the reason
    ///    [`Self::snapshot_entry_counts`] excludes them. Only the `methods`
    ///    maps are walked; an OSR body leaves through the reverse closure
    ///    below, if at all.
    ///  * **Retirement goes through [`Self::invalidate_matching`]**, which
    ///    hands every withdrawn body to `retire_withdrawn_body` ->
    ///    [`defer_jit_owner`] and therefore to the per-thread quiescence proof.
    ///    Dropping the `Arc`s directly would unmap a body a parked thread can
    ///    still return into; the whole argument in
    ///    [`ThreadQuiescenceEvidence::permits`] is what makes a release safe,
    ///    and a sweeper is precisely the caller most tempted to bypass it.
    ///
    /// The limit it does not fix: a body pinned by some caller's
    /// `_direct_callee_roots` is withdrawn from the cache but its mapping is
    /// not released, so bytes freed are not proportional to
    /// [`JitCodeCacheSweepReport::swept`]. Read `jit_code_reclamation_stats`
    /// for what was actually returned.
    ///
    /// # No invalidation record is logged, on purpose
    ///
    /// Every other caller of `invalidate_matching` calls `log_invalidation`
    /// first, so a compile already in flight against the withdrawn dependency
    /// is refused at publication. A sweep is not a dependency change: the body
    /// it withdraws was correct, and a compile that is mid-flight for the same
    /// method should publish normally — the method is evidently hot again.
    /// Logging here would refuse exactly the recompile the sweep wants.
    ///
    /// # `progress`
    ///
    /// A monotonic clock of work the process did, which the caller supplies
    /// because this crate has none: the VM passes its interpreter tier-up
    /// tick. "No body was entered since the last sweep" is coldness only if
    /// something ELSE ran meanwhile; with this clock unchanged too, the sweep
    /// is refused as [`JitCodeCacheSweepRefusal::NothingRanSinceTheLastSweep`].
    pub fn sweep_cold_bodies(&self, progress: u64) -> JitCodeCacheSweepReport {
        use std::sync::atomic::Ordering;
        let mut report = JitCodeCacheSweepReport::default();
        if !code_cache_sweep_enabled() {
            report.refused = Some(JitCodeCacheSweepRefusal::Disabled);
            return report;
        }
        if !crate::entry_counter::entry_counter_enabled() {
            report.refused = Some(JitCodeCacheSweepRefusal::NoUsageSignal);
            return report;
        }

        // This cache's own lock, taken by nothing else, and released before
        // `invalidate_matching` takes `mutation`. The two are never held
        // together; a sweeper that nested them would invert against `put`,
        // which holds `mutation` while publishing.
        let mut state = self.sweep.lock();

        let mut live: Vec<(u64, u64, usize)> = Vec::new();
        let mut total_entries: u64 = 0;
        for shard in self.shards.iter() {
            for (_key, cm) in shard.methods.load().values() {
                // Already withdrawn by an invalidation and waiting on the
                // retirement queue: not this sweep's to retire, and counting
                // it would make `observed` disagree with what was examined.
                if cm.retired.load(Ordering::SeqCst) {
                    continue;
                }
                let count = cm.entry_count();
                total_entries = total_entries.saturating_add(count);
                live.push((cm.artifact_id, count, cm.entry_ptr() as usize));
            }
        }
        report.observed = live.len();

        let (doomed, refusal) = state.plan(&live, total_entries, progress);
        drop(state);

        report.candidates = doomed.len();
        report.refused = refusal;
        if doomed.is_empty() {
            // Either a refusal, or — with `refused` still `None` — the sweeper
            // looked and everything in the cache had moved. Those two are
            // deliberately distinguishable in the report.
            return report;
        }

        // The identities come back FROM the withdrawal, not from a pre-scan of
        // the candidates. A pre-scan can only see what the coldness predicate
        // chose; the withdrawal also takes the transitive reverse closure over
        // baked direct calls and lambda adapters, and those bodies need the same
        // demotion — see [`JitCodeCacheSweepReport::withdrawn`] for why a
        // withdrawal without it is worse than not sweeping at all.
        //
        // CODECACHE21-6 (tag cc21): matched by ARTIFACT, not by entry address.
        // `plan` keys its readings on `artifact_id` precisely because a freed
        // buffer's address is recycled; planning on an artifact and then
        // withdrawing "whatever is at that address" brought the ABA back for
        // the window between the plan and the withdrawal (a doomed body
        // withdrawn by another door, drained, and its address re-issued to a
        // fresh, hot body, which the sweep then withdrew).
        let doomed_ids = doomed_artifact_ids(&live, &doomed);
        let (swept, withdrawn, withdrawn_bytes) = self
            .invalidate_matching_collecting(retire_reason::CACHE_PRESSURE, |_key, cm| {
                doomed_ids.contains(&cm.artifact_id)
            });
        report.swept = swept;
        report.withdrawn = withdrawn;
        report.withdrawn_bytes = withdrawn_bytes;
        // Reporting only (wave 11): taken after the withdrawal, never nested
        // with `mutation`.
        self.sweep.lock().swept_total += swept as u64; // Widening: usize -> u64
        report
    }

    /// JIT round 22 wave 11, lane runtime (reporting only; the
    /// `code_cache_sweep_phases` failure message): this cache's sweep census --
    /// sweeps, refusals because nothing ran, sweeps that found nothing cold,
    /// cold candidates and bodies withdrawn over all sweeps.
    pub fn code_cache_sweep_census(&self) -> String {
        let s = self.sweep.lock();
        format!(
            "sweeps={} refused_nothing_ran={} found_nothing_cold={} candidates={} swept={}",
            s.sweeps, s.refused_nothing_ran, s.found_nothing_cold, s.candidates_total, s.swept_total
        )
    }

    /// Sweeps this cache has run, census included. Diagnostic; also what a
    /// test uses to tell "refused" apart from "never called".
    pub fn code_cache_sweeps(&self) -> u64 {
        self.sweep.lock().sweeps
    }

    /// Remove a compiled method from the cache (for invalidation).
    ///
    /// Verifies the full string key matches before removing, so a
    /// (rare) hash collision can't cause an unrelated cached entry to
    /// be evicted.
    pub fn remove(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) {
        self.log_invalidation(InvalidationRecord::Key {
            class_name: Arc::from(class_name),
            method_name: Arc::from(method_name),
            descriptor: Arc::from(descriptor),
            declaring_class_id,
        });
        // `DEOPTIMIZED`, and the justification is the CALLER's, not this
        // function's: `remove`'s own doc says only "for invalidation", but its
        // single production caller in the workspace is the deopt eviction in
        // `vm/src/jit/helpers.rs`, behind
        // `tiered::deopt_evicts_method_body(reason, action)`. See
        // `retire_reason::DEOPTIMIZED` for the standing hazard that creates and
        // for the remedy if a second caller ever appears.
        let h = compute_jit_key_hash(class_name, method_name, descriptor, declaring_class_id);
        let shard = &self.shards[Self::shard_index(h)];
        let matches_key = |key: &JitKey| {
            &*key.class_name == class_name
                && &*key.method_name == method_name
                && &*key.descriptor == descriptor
                && key.declaring_class_id == declaring_class_id
        };
        {
            let mutation = self.mutation.lock();
            let mut remove_entries = std::collections::HashSet::new();
            if let Some((key, cm)) = shard.methods.load().get(&h) {
                if matches_key(key) {
                    remove_entries.insert(cm.entry_ptr() as usize);
                }
            }
            if let Some((key, cm)) = shard.osr_methods.load().get(&h) {
                if matches_key(key) {
                    remove_entries.insert(cm.entry_ptr() as usize);
                }
            }
            if !remove_entries.is_empty() {
                let _ = self.withdraw_entries_locked(
                    mutation,
                    retire_reason::DEOPTIMIZED,
                    remove_entries,
                    Vec::new(),
                );
            }
        }
        // Round 9 wave 9 (`evict9`): an evicted method must not stay reachable
        // through a caller that baked one of its OLDER bodies either. After
        // `mutation` has been released.
        self.invalidate_retire_cells_named(class_name, method_name, descriptor);
    }

    /// T5.4.4 — Class hierarchy change invalidation.
    ///
    /// When a new class is loaded that extends/implements an existing
    /// class, any JIT-compiled method whose `inlined_methods` list
    /// references the changed class must be evicted because the
    /// CHA-based devirtualization assumption (only one implementor of
    /// the virtual method) is no longer valid.
    ///
    /// Returns the number of evicted entries.
    pub fn invalidate_for_class_change(&self, changed_class: &str) -> usize {
        // Log first, even when nothing published names the class: a compile
        // that inlined from it may still be running, and the log is the only
        // thing that will stop it publishing. See `dependencies_are_current`.
        self.log_invalidation(InvalidationRecord::InlinedClass(Arc::from(changed_class)));
        // See `inlined_class_names`: nobody inlined it, so nothing can match,
        // and the scan below would walk every shard to say 0.
        if !self.any_body_inlined_from(changed_class) {
            return 0;
        }
        let names_class = |cm: &CompiledMethod| -> bool {
            self.dependency_buckets.note_checked(cm, changed_class);
            cm.inlined_methods
                .iter()
                .any(|(cls, _, _)| cls == changed_class)
        };
        // Round 15 wave 5 (lane vh7; VH6-1, opt-in): and the superseded
        // bodies (`invalidation_retires_superseded`).
        if self.invalidation_retires_superseded {
            return self
                .invalidate_matching_collecting_with(
                    retire_reason::INVALIDATED,
                    |_, cm| names_class(cm),
                    Some(&names_class as &dyn Fn(&CompiledMethod) -> bool),
                )
                .0;
        }
        self.invalidate_matching(retire_reason::INVALIDATED, |_, cm| names_class(cm))
    }

    /// [`Self::invalidate_for_class_change`] for the one event the VM's class
    /// define listener reports for every class in a new class's supertype
    /// closure: `supertype` gained a subtype (interpreter round i4 wave 88,
    /// lane L7, `CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED`, default ON, `=0`
    /// restores the name-only match).
    ///
    /// A new subtype changes what a VIRTUAL member of `supertype` binds to,
    /// and nothing else: its constructors, initialiser, static, private and
    /// final methods (`fixed`, read by the caller from the class store, plus
    /// `<init>` / `<clinit>` by name) keep their one body. So only a body
    /// whose `inlined_methods` names `supertype` with some OTHER member is
    /// withdrawn ([`names_subtype_sensitive_member`]), and the log record a
    /// running compile is checked against at publication carries the same
    /// rule. The name-only match withdrew every body that inlined a
    /// constructor chain on every class define (all of them reach
    /// `java/lang/Object.<init>`): `L4W88FindConstructorSplitBench` showed
    /// 1284 installs over 713 methods, `Utf8EntryImpl.<init>` compiled 16x.
    ///
    /// Returns the number of evicted entries.
    pub fn invalidate_for_new_subtype(
        &self,
        supertype: &str,
        fixed: Arc<[(Arc<str>, Arc<str>)]>,
    ) -> usize {
        // Logged first, as `invalidate_for_class_change` logs: a compile
        // still running when the define lands is judged at publication.
        self.log_invalidation(InvalidationRecord::NewSubtypeOf {
            class_name: Arc::from(supertype),
            fixed: Arc::clone(&fixed),
        });
        if !self.any_body_depends_on_subtype(supertype, &fixed) {
            return 0;
        }
        let sensitive = |cm: &CompiledMethod| -> bool {
            self.dependency_buckets.note_checked(cm, supertype);
            names_subtype_sensitive_member(cm, supertype, &fixed)
        };
        if self.invalidation_retires_superseded {
            return self
                .invalidate_matching_collecting_with(
                    retire_reason::INVALIDATED,
                    |_, cm| sensitive(cm),
                    Some(&sensitive as &dyn Fn(&CompiledMethod) -> bool),
                )
                .0;
        }
        self.invalidate_matching(retire_reason::INVALIDATED, |_, cm| sensitive(cm))
    }

    /// Batched [`Self::invalidate_for_new_subtype`] for every supertype in a
    /// newly defined class's closure: logs each supertype, checks
    /// `inlined_class_names` under a single lock, and runs at most one
    /// `JitCache` scan/withdrawal pass.
    pub fn invalidate_for_new_subtypes(
        &self,
        supertypes: &[(Arc<str>, Arc<[(Arc<str>, Arc<str>)]>)],
        introduced: Arc<[(Arc<str>, Arc<str>)]>,
    ) -> usize {
        if supertypes.is_empty() || introduced.is_empty() {
            return 0;
        }
        for (sup, fixed) in supertypes {
            self.log_invalidation(InvalidationRecord::NewSubtypeIntroducing {
                class_name: Arc::clone(sup),
                fixed: Arc::clone(fixed),
                introduced: Arc::clone(&introduced),
            });
        }
        let matching: Vec<&(Arc<str>, Arc<[(Arc<str>, Arc<str>)]>)> = {
            let names = self.inlined_class_names.lock();
            supertypes
                .iter()
                .filter(|(sup, fixed)| {
                    names.get(sup.as_ref()).is_some_and(|members| {
                        members.iter().any(|(m, d)| {
                            subtype_sensitive_member(m, d, fixed)
                                && introduced
                                    .iter()
                                    .any(|(im, id)| im.as_ref() == m.as_ref() && id.as_ref() == d.as_ref())
                        })
                    })
                })
                .collect()
        };
        if matching.is_empty() {
            return 0;
        }
        let sensitive = |cm: &CompiledMethod| -> bool {
            matching.iter().any(|(sup, fixed)| {
                self.dependency_buckets.note_checked(cm, sup);
                names_subtype_sensitive_introduced_member(cm, sup, fixed, &introduced)
            })
        };
        if self.invalidation_retires_superseded {
            return self
                .invalidate_matching_collecting_with(
                    retire_reason::INVALIDATED,
                    |_, cm| sensitive(cm),
                    Some(&sensitive as &dyn Fn(&CompiledMethod) -> bool),
                )
                .0;
        }
        self.invalidate_matching(retire_reason::INVALIDATED, |_, cm| sensitive(cm))
    }

    /// t1 wave 5, lane M (`t1-V3-in-flight-compile-keeps-scalar-replacement-
    /// after-live-can-tag-objects`; `CRATONVM_JIT_OBSERVER_GAIN_REFUSES_IN_FLIGHT`,
    /// correctness, default ON, `=0` kill): the VM just gained an object
    /// observer (a live-phase `can_tag_objects`; the caller has already
    /// published the gate the compile doors read, so every compile that begins
    /// after this record reads it). Logs
    /// [`InvalidationRecord::ObjectObserverGained`], which refuses at
    /// publication (`put`, `put_osr`, and the optimizing OSR memo through
    /// [`Self::unpublished_body_dependencies_are_current`]) every body whose
    /// compilation began earlier: HotSpot's `ciEnv::jvmti_state_changed`.
    /// The refusal is a failed compile, so the method retries. Installed bodies are
    /// not touched, as in HotSpot. Returns whether the record was logged.
    pub fn refuse_in_flight_for_object_observer(&self) -> bool {
        self.log_invalidation(InvalidationRecord::ObjectObserverGained);
        true
    }

    /// The VM's class define listener reports that `class_name` (`class_id`)
    /// gained a FINALIZABLE subtype while it has a constructor that can throw
    /// before its own `super(..)` / `this(..)` call (JIT round 21 wave 13, lane
    /// finalize, `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED`; page
    /// `docs/feature-designs/jit-ir-optimizations.md`).
    ///
    /// The VM withdraws the early finalizer registration of an object whose
    /// constructor chain provably threw before `Object.<init>` returned, and
    /// the proof needs that throw in an interpreted frame. From now on such a
    /// constructor of this class is refused by every compile door, so the
    /// bodies built before the define go too: its own constructor bodies and
    /// every body that spliced one ([`withdraws_for_finalizable_subtype`]).
    /// Not on any hot path: once per define of a finalizable class, for each
    /// ancestor up to the first finalizable one. No subtype instance exists yet
    /// when the define lands, and a compile still running is judged at
    /// publication by the logged record.
    ///
    /// Returns the number of evicted entries.
    pub fn invalidate_for_finalizable_subtype(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> usize {
        self.log_invalidation(InvalidationRecord::FinalizableSubtypeOf {
            class_id,
            class_name: Arc::from(class_name),
        });
        // Flip review item 7: asked once, before the walk; a copy noted later
        // is judged at its publication by the logged record (`withdraws_in`).
        let copy_unrecorded = finalizer_copy_unrecorded_on_body(self, class_id);
        self.invalidate_matching(retire_reason::INVALIDATED, |key, cm| {
            withdraws_for_finalizable_subtype(key, cm, class_id, class_name, copy_unrecorded)
        })
    }

    /// `CRATONVM_FROZEN_PAGES`: withdraw the bodies of `methods`, whose last
    /// build missed a fold that is available now, so their next tier-up makes
    /// it. Returns the number of entries evicted.
    pub fn invalidate_for_frozen_facts(&self, methods: &[frozen_facts::MethodName]) -> usize {
        self.invalidate_matching(retire_reason::SUPERSEDED, |key, _| {
            methods
                .iter()
                .any(|m| {
                    key.class_name == m.0
                        // `frozen_facts::want`'s "every method of the class".
                        && ((&*m.1 == "*" && &*m.2 == "*") || (key.method_name == m.1 && key.descriptor == m.2))
                })
        })
    }

    /// Invalidate all compiled methods that inlined code from `class_name`.
    /// Returns the number of methods evicted.
    pub fn invalidate_for_class(&self, class_name: &str) -> usize {
        // Log first — see `invalidate_for_class_change`.
        self.log_invalidation(InvalidationRecord::InlinedClass(Arc::from(class_name)));
        if !self.any_body_inlined_from(class_name) {
            return 0;
        }
        // Round 15 wave 4 (lane vh6): the first-exact-mint sweep also retires
        // the SUPERSEDED bodies that bound a thin `VarHandle` helper, or bake a
        // raw call into one it withdraws (`varhandle_retire_superseded_enabled`).
        // Round 15 wave 5 (lane vh7; VH6-1, opt-in): every other class too
        // under `invalidation_retires_superseded`.
        let is_thin_or_cha = class_name == VARHANDLE_THIN_BIND_DEPENDENCY
            || class_name.starts_with(crate::frozen_facts::CHA_FIELD_DEP_PREFIX)
            || class_name.starts_with(crate::frozen_facts::CHA_LEAF_DEP_PREFIX);
        if (is_thin_or_cha && varhandle_retire_superseded_enabled())
            || (!is_thin_or_cha && self.invalidation_retires_superseded)
        {
            let names_class = |compiled: &CompiledMethod| -> bool {
                compiled
                    .inlined_methods
                    .iter()
                    .any(|(cn, _, _)| cn == class_name)
            };
            return self
                .invalidate_matching_collecting_with(
                    retire_reason::INVALIDATED,
                    |_, compiled| names_class(compiled),
                    Some(&names_class as &dyn Fn(&CompiledMethod) -> bool),
                )
                .0;
        }
        self.invalidate_matching(retire_reason::INVALIDATED, |_, compiled| {
            compiled
                .inlined_methods
                .iter()
                .any(|(cn, _, _)| cn == class_name)
        })
    }

    /// The VM's first-exact-`VarHandle`-mint sweep: [`Self::invalidate_for_class`]
    /// of [`VARHANDLE_THIN_BIND_DEPENDENCY`], and then (round 15 wave 5, lane
    /// vh7; `r15w3-vh3-running-thin-bound-activation-keeps-binds-FIXED-20260930.md`)
    /// every body that sweep withdrew is marked withdrawn
    /// ([`Self::mark_withdrawn_by_redefinition`]) and has its exit polls and
    /// post-call exits forced ([`Self::force_withdrawn_exit_polls`]), exactly
    /// as a class redefinition treats the bodies it withdraws.
    ///
    /// Withdrawal stops new entries only. A frame already running a thin-bound
    /// body -- an OSR loop that bound `VarHandle.set`, or a loop that bakes a
    /// raw call into a thin-bound callee -- kept calling the helper, which
    /// cannot judge an exact handle, for the rest of the loop
    /// (`R15Vh3LateMintRetire` `midLoop`: `accepted=10000`, HotSpot
    /// `wmte=10000`). Marked and forced, such a frame asks the safepoint slow
    /// path at its next back edge, or as soon as the call it is inside returns
    /// (the minting thread's own loop is inside the mint's call), and the
    /// verdict (`jvmti_events::withdrawn_body_may_leave`) sends it to the
    /// interpreter, whose next access is judged; its OSR door and memo refuse
    /// the marked body from then on. HotSpot deoptimizes dependent frames the
    /// same way.
    ///
    /// The candidates are the live bodies registered for forcing
    /// ([`Self::note_exit_poll_body`]) that are neither retired nor marked
    /// BEFORE the sweep; the ones retired AFTER it are the sweep's (a body
    /// another invalidation retires in between is withdrawn as well, and
    /// leaving is correct for it too). A body with nothing to force is not
    /// registered: its frames finish on it, as before. Returns `(withdrawn,
    /// bodies forced)`. `CRATONVM_JIT_VH_MINT_FORCES_EXITS=0`
    /// ([`varhandle_mint_forces_exits_enabled`]) withdraws only.
    ///
    /// And (VH6-3's entry half, [`varhandle_mint_not_entrant_enabled`]) every
    /// method-entry body that bound a helper itself is made not entrant once
    /// withdrawn, when the VM armed the hook: a raw call into it that no
    /// closure withdrew and no forced exit reaches (an optimizing body's
    /// running loop, whose polls cannot be forced) re-dispatches instead.
    pub fn invalidate_varhandle_thin_binds(&self) -> (usize, usize) {
        self.invalidate_forcing_exits(VARHANDLE_THIN_BIND_DEPENDENCY)
    }

    /// [`Self::invalidate_varhandle_thin_binds`] for `dependency`: logs the
    /// invalidation record (refusing in-flight compiles), evicts published and
    /// superseded bodies, patches entry prologues not-entrant, and forces
    /// loop-back-edge exits on active frames.
    pub fn invalidate_forcing_exits(&self, dependency: &str) -> (usize, usize) {
        use std::sync::atomic::Ordering::Acquire;
        let candidates: Vec<Arc<CompiledMethod>> = if varhandle_mint_forces_exits_enabled() {
            // Upgraded under the list's lock and none dropped there, as
            // `live_exit_poll_bodies` does.
            let list = self.exit_poll_bodies.lock();
            list.iter().filter_map(std::sync::Weak::upgrade).collect()
        } else {
            Vec::new()
        };
        let (candidates, earlier): (Vec<Arc<CompiledMethod>>, Vec<Arc<CompiledMethod>>) =
            candidates
                .into_iter()
                .partition(|cm| !cm.retired.load(Acquire) && !cm.is_withdrawn_by_redefinition());
        release_unpublished_pins(earlier);
        // VH6-3's entry half (`varhandle_mint_not_entrant_enabled`): the
        // published method-entry bodies that bound a thin helper themselves,
        // listed before the sweep empties the maps. Only when the VM armed
        // the hook (`make_not_entrant` patches nothing without). Not the
        // superseded ones: their registry is process-wide, and a patch bakes
        // THIS VM's hook, which must never answer another VM's raw call.
        let thin_bound: Vec<Arc<CompiledMethod>> =
            if varhandle_mint_not_entrant_enabled() && self.not_entrant_hook.lock().is_some() {
                let mut bodies: Vec<Arc<CompiledMethod>> = Vec::new();
                for shard in self.shards.iter() {
                    for (_hash, (_key, cm)) in shard.methods.load().iter() {
                        let cm = cm.arc();
                        if !cm.compiled_via_osr
                            && cm.inlined_methods.iter().any(|(cn, _, _)| cn == dependency)
                        {
                            bodies.push(Arc::clone(cm));
                        }
                    }
                }
                bodies
            } else {
                Vec::new()
            };
        let withdrawn = self.invalidate_for_class(dependency);
        // Every listed body is withdrawn now (the sweep's predicate names each
        // one); `make_not_entrant` skips any a racing publication re-published.
        let patched = self.make_not_entrant(&thin_bound);
        release_unpublished_pins(thin_bound);
        let (doomed, kept): (Vec<Arc<CompiledMethod>>, Vec<Arc<CompiledMethod>>) = candidates
            .into_iter()
            .partition(|cm| cm.retired.load(Acquire));
        release_unpublished_pins(kept);
        let (marked, forced) = if doomed.is_empty() {
            (0, 0)
        } else {
            let marked = self.mark_withdrawn_by_redefinition(&doomed);
            // No class was redefined: a class id no published body carries
            // and a name no label starts with, so nothing is spared as an
            // obsolete activation (`JitRealm::withdraw_every_body_for_the_interpreter`
            // forces with the same pair).
            let forced = self.force_withdrawn_exit_polls(
                &doomed,
                cratonvm_types::ClassId::new(u32::MAX),
                "<first exact VarHandle mint>",
            );
            (marked, forced)
        };
        // `CRATONVM_DBG_JIT_PIN=1`: what the sweep did to running code.
        if (!doomed.is_empty() || patched != 0) && dbg_jit_pin_enabled() {
            eprintln!(
                "[jit-vh-mint-exits] withdrew {withdrawn}; not entrant {patched}; {} forceable \
                 bod{} marked {marked}, forced {forced}: {}",
                doomed.len(),
                if doomed.len() == 1 { "y" } else { "ies" },
                doomed
                    .iter()
                    .take(16)
                    .map(|cm| cm.method_label.as_str())
                    .collect::<Vec<&str>>()
                    .join(", ")
            );
        }
        release_unpublished_pins(doomed);
        (withdrawn, forced)
    }

    // Retire every body owned by an unloaded class and every caller that
    // inlined one of its methods. Retired executable allocations are reclaimed
    // by the epoch/quiescence path once no active frame can still execute them.
    // gc-common w36: production unloads go through the batch form
    // (`invalidate_unloaded_classes`, one sweep per unload), so the single
    // form is kept for the equivalence and inventory tests only.
    #[cfg(test)]
    pub(crate) fn invalidate_unloaded_class(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> usize {
        self.log_invalidation(InvalidationRecord::UnloadedClass {
            class_id,
            class_name: Arc::from(class_name),
        });
        let copied_arm = true;
        // JIT round 22 wave 2, lane review: as the batch form.
        let lost_copy_arm = copied_arm
            && class_id.as_u32() != 0
            && self.bytecode_was_copied(class_id);
        self.invalidate_matching(retire_reason::CLASS_UNLOADED, |key, compiled| {
            key.declaring_class_id == class_id
                || compiled
                    .inlined_methods
                    .iter()
                    .any(|(cn, _, _)| cn == class_name)
                // Round 13 wave 12 (lane guardsplice): as the batch form.
                || (copied_arm
                    && class_id.as_u32() != 0
                    && compiled.copied_bytecode_of(class_id))
                || (lost_copy_arm && compiled.copy_record_lost)
        })
    }

    /// Batch form of [`Self::invalidate_unloaded_class`] for a whole unloaded
    /// set (gc-common w36-c): one `InvalidationRecord::UnloadedClass` per
    /// class, as the loop logged, then ONE `invalidate_matching` -- one walk
    /// of every shard and one reverse closure, instead of one of each per
    /// class. Returns the number of bodies withdrawn, which equals the loop's
    /// sum: the loop's per-class withdrawals are disjoint (a withdrawn body is
    /// no longer in the cache for the next class), and the closure of a union
    /// is the union of the closures.
    pub fn invalidate_unloaded_classes(&self, classes: &UnloadedClassSet<'_>) -> usize {
        if classes.is_empty() {
            return 0;
        }
        for (class_id, class_name) in classes.iter() {
            self.log_invalidation(InvalidationRecord::UnloadedClass {
                class_id,
                class_name: Arc::from(class_name),
            });
        }
        let names = classes.names();
        let copied_arm = true;
        // JIT round 22 wave 2, lane review
        // (`CRATONVM_JIT_UNLOAD_WITHDRAWS_LOST_COPY`): when some unloaded class
        // was ever copied on this cache, a body whose compile lost a copy
        // record is withdrawn too (its `copied_classes` cannot vouch for it).
        // Decided once per unload; the per-body cost is one flag test.
        let lost_copy_arm = copied_arm
            && classes
                .iter()
                .any(|(id, _)| id.as_u32() != 0 && self.bytecode_was_copied(id));
        // The key-free half of the predicate, shared with the superseded
        // bodies' (round 15 wave 5, lane vh7; VH6-1, opt-in).
        let depends = |compiled: &CompiledMethod| -> bool {
            compiled
                .inlined_methods
                .iter()
                .any(|(cn, _, _)| names.contains(cn.as_str()))
                // Round 13 wave 12 (lane guardsplice): a body that spliced an
                // unloaded class's bytecode (`copied_classes`), which the IR
                // tier records instead of `inlined_methods`
                // (`CRATONVM_JIT_UNLOAD_WITHDRAWS_COPIED`).
                || (copied_arm
                    && compiled
                        .copied_classes
                        .iter()
                        .any(|&c| classes.contains_nonzero_id(cratonvm_types::ClassId::new(c))))
                || (lost_copy_arm && compiled.copy_record_lost)
        };
        // gci i12/ul F3: the keyed form of `depends`. An inlinee name counts
        // only if the body's own declaring class cannot be shown to resolve
        // it to a live namesake (`UnloadedClassSet::inlinee_is_member`; by
        // name alone unless the VM armed `with_live_namesakes`).
        let depends_keyed = |declaring: cratonvm_types::ClassId, compiled: &CompiledMethod| {
            compiled
                .inlined_methods
                .iter()
                .any(|(cn, _, _)| classes.inlinee_is_member(declaring, cn.as_str()))
                || (copied_arm
                    && compiled
                        .copied_classes
                        .iter()
                        .any(|&c| classes.contains_nonzero_id(cratonvm_types::ClassId::new(c))))
                || (lost_copy_arm && compiled.copy_record_lost)
        };
        // A superseded body is judged by its dependencies only: the key half
        // (a body OF an unloaded class) needs the body's key, which a
        // superseded body no longer has in any map (the list is per cache since
        // j27 lane GLOBALS); and a body of an unloaded class has no live caller
        // outside its own dead loader.
        let withdrawn = self
            .invalidate_matching_collecting_with(
                retire_reason::CLASS_UNLOADED,
                |key, compiled| {
                    classes.contains_id(key.declaring_class_id)
                        || depends_keyed(key.declaring_class_id, compiled)
                },
                self.invalidation_retires_superseded
                    .then_some(&depends as &dyn Fn(&CompiledMethod) -> bool),
            )
            .0;
        // Round 12 wave 4 (lane mega3, applying
        // `r12w4-hunter2-mega-table-keeps-unloaded-receiver-ways-patch`): the
        // pass above retires the shared megamorphic table's ways by TARGET,
        // which leaves a way keyed on an unloaded RECEIVER whose target a
        // surviving class declares (an inherited or default method). It can
        // never hit again (class ids are never reused), and a full set is never
        // evicted, so loader churn would fill the table with such keys.
        if let Some(table) = self.mega_dispatch_table_if_any() {
            table.retire_receiver_classes(&|id: u32| {
                classes.contains_nonzero_id(cratonvm_types::ClassId::new(id))
            });
        }
        // Round 13 wave 8 (lane mega7,
        // `r13w8-mega7-site-caches-keep-unloaded-receivers-FIXED-20260928.md`): and
        // every surviving body's own MICs and PIC ways keyed on an unloaded
        // RECEIVER, which the pass above leaves when their target survives. A
        // snapshot walk, as the redefinition pass's: a body published after
        // it can hold no receiver of a class that is already gone.
        if inline_cache_pic::ic_forget_dead_receivers_enabled() {
            let dead = |id: u32| classes.contains_nonzero_id(cratonvm_types::ClassId::new(id));
            for shard in self.shards.iter() {
                for map in [shard.methods.load(), shard.osr_methods.load()] {
                    for (_hash, (_key, cm)) in map.iter() {
                        inline_cache_pic::forget_dead_receivers_in(cm, &dead);
                    }
                }
            }
        }
        withdrawn
    }

    /// Arm this cache to report every withdrawal to `sink`, the owning VM's
    /// tiered manager (tier proposal W3-1, round 12 wave 4; see
    /// [`JitBodyWithdrawalSink`]). The first sink wins; answers whether this
    /// call armed it. `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0` (default on) leaves
    /// the cache unarmed, which restores the pre-wave-4 behaviour: only the
    /// callers that demote by hand (the sweeper, the redefinition paths) do.
    pub fn install_withdrawal_sink(
        &self,
        sink: std::sync::Weak<dyn JitBodyWithdrawalSink>,
    ) -> bool {
        self.withdrawal_sink.set(sink).is_ok()
    }

    /// Whether this cache reports its withdrawals itself
    /// ([`Self::install_withdrawal_sink`]), so a caller that used to demote by
    /// hand must not do it a second time.
    pub fn reports_withdrawals(&self) -> bool {
        self.withdrawal_sink.get().is_some()
    }

    /// HotSpot's `PerMethodRecompilationCutoff` input (JIT round 21 wave 4,
    /// lane codecache; rev-x64 item 8 in wave 5): hand the sink the keys whose
    /// OPTIMIZING body this withdrawal took, method-entry ones and -- under
    /// `CRATONVM_JIT_OSR_WITHDRAWAL_CHARGED` (CODECACHE21-4, default ON, `=0`
    /// leaves them out) -- OSR ones, deduplicated per key, for
    /// [`JitBodyWithdrawalSink::optimizing_bodies_invalidated`].
    ///
    /// Only a withdrawal that HotSpot would count as a decompilation charges:
    /// [`retire_reason::INVALIDATED`] (a dependency: a class gained a subtype,
    /// an inlined class was redefined or unloaded) and
    /// [`retire_reason::DEOPTIMIZED`] (a trap evicted it). A cache-pressure
    /// eviction, a class-unload sweep of the class's OWN bodies, a flush or
    /// shutdown is not a recompilation the code caused, and an unload/reload
    /// cycle must not walk a method down to C1. Must be called with
    /// `mutation` released, as [`Self::report_withdrawn`].
    pub(crate) fn report_charged_withdrawals(
        &self,
        reason: usize,
        entry_optimizing: &[WithdrawnMethodKey],
        osr_optimizing: &[WithdrawnMethodKey],
    ) {
        if reason != retire_reason::INVALIDATED && reason != retire_reason::DEOPTIMIZED {
            return;
        }
        let osr_charged = !osr_optimizing.is_empty();
        if entry_optimizing.is_empty() && !osr_charged {
            return;
        }
        let Some(sink) = self.withdrawal_sink.get().and_then(|weak| weak.upgrade()) else {
            return;
        };
        let mut seen: FxHashSet<WithdrawnMethodKey> = FxHashSet::default();
        let osr: &[WithdrawnMethodKey] = if osr_charged { osr_optimizing } else { &[] };
        let keys: Vec<WithdrawnMethodKey> = entry_optimizing
            .iter()
            .chain(osr.iter())
            .filter(|k| seen.insert((*k).clone()))
            .cloned()
            .collect();
        if !keys.is_empty() {
            sink.optimizing_bodies_invalidated(&keys);
        }
    }

    /// Hand the sink `entry_withdrawn`, the keys whose METHOD-ENTRY body this
    /// withdrawal took. `current_tier` describes method-entry bodies only (an
    /// OSR publish never raises it), so a key that lost only its OSR body is
    /// not passed in: its tier still names a live body, and lowering it would
    /// cost a recompile of a method that has one.
    ///
    /// A key that has an OPTIMIZING method-entry body published again by the
    /// time this runs is left out as well: a compile that published between
    /// the withdrawal and this call has already set its tier, and that tier
    /// is backed. A single-pass body published in that window is not trusted
    /// the same way: the tier may still read C2 from the withdrawn body, and
    /// the interpreter tier is the one claim that cannot strand the method.
    ///
    /// Must be called with `mutation` released: the sink takes the tiered
    /// manager's lock.
    pub(crate) fn report_withdrawn(&self, entry_withdrawn: &[WithdrawnMethodKey]) {
        if entry_withdrawn.is_empty() {
            return;
        }
        let Some(sink) = self.withdrawal_sink.get().and_then(|weak| weak.upgrade()) else {
            return;
        };
        let stranded: Vec<WithdrawnMethodKey> = entry_withdrawn
            .iter()
            .filter(|key| {
                // `RetainedCode`: a body withdrawn again between this read and
                // its drop is released through the retirement queue.
                self.get(&key.0, &key.1, &key.2, key.3)
                    .map(RetainedCode::new)
                    .is_none_or(|live| !live.used_ir_backend)
            })
            .cloned()
            .collect();
        if !stranded.is_empty() {
            sink.bodies_withdrawn(&stranded);
        }
    }

    /// Withdraw every cached body matching `predicate`, and the transitive
    /// reverse closure over baked direct calls and lambda adapters.
    ///
    /// Answers the count only. The demotion a withdrawal owes (a body
    /// withdrawn without a matching `note_body_withdrawn` is stuck at its old
    /// tier forever) is not the caller's: [`JitCache::invalidate_matching_collecting`]
    /// reports every withdrawal to the VM's sink ([`JitBodyWithdrawalSink`],
    /// round 12 wave 4). Use that form when the caller needs the identities
    /// for something else.
    ///
    /// `reason` is a [`retire_reason`] code, chosen by the NAMED entry point
    /// that called this, never by this function. That is what lets the report
    /// tell class unloading from assumption invalidation even though both
    /// arrive through this one predicate scan -- see [`retire_reason`].
    pub(crate) fn invalidate_matching(
        &self,
        reason: usize,
        predicate: impl Fn(&JitKey, &CompiledMethod) -> bool,
    ) -> usize {
        self.invalidate_matching_collecting(reason, predicate).0
    }

    /// [`JitCache::invalidate_matching`], also handing back the identity of
    /// every entry it withdrew -- **including the closure members**, which is
    /// the whole reason this form exists.
    ///
    /// Before this, the closure was computed here and thrown away: the sweeper
    /// pre-scanned the shard maps for its own COLD CANDIDATES and reported
    /// those, so a caller that baked a direct call into a cold callee, or a
    /// lambda adapter reaching one, lost its body and was never demoted.
    /// `current_tier` only advances, so such a method could never be offered
    /// for compilation again -- the stuck-at-C1 residual RT-8 records.
    ///
    /// Keys come from BOTH maps. The old pre-scan looked only at
    /// `shard.methods`, so an OSR body withdrawn on its own was invisible to
    /// it as well; an OSR entry's key names the same method, and demoting it is
    /// the same claim (its compiled body is gone). Duplicates are removed
    /// rather than left for the caller, since the caller's action is
    /// idempotent but its accounting is not.
    ///
    /// The empty-match fast path allocates nothing: `Vec::new` does not, and
    /// that path is the overwhelmingly common one -- this runs on every class
    /// define.
    pub(crate) fn invalidate_matching_collecting(
        &self,
        reason: usize,
        predicate: impl Fn(&JitKey, &CompiledMethod) -> bool,
    ) -> (usize, Vec<WithdrawnMethodKey>, u64) {
        self.invalidate_matching_collecting_with(reason, predicate, None)
    }

    /// [`JitCache::invalidate_matching_collecting`], and, when
    /// `superseded_predicate` is given, the SUPERSEDED bodies too (round 15
    /// wave 4, lane vh6; `r15w4-vh5-via-caller-keeps-thin-bind-after-mint-FIXED-20260930.md`).
    ///
    /// A body replaced by a tier-up is in no map, so the scan above cannot see
    /// it, yet it stays executable through every raw caller that baked it
    /// ("the C2 supersede leaves the C1 body callable",
    /// [`JitCache::invalidate_retire_cells_named`]). With the predicate, every
    /// live superseded body of THIS cache (`superseded_bodies`, not yet
    /// retired) that matches it, or that bakes a raw call into anything this
    /// invalidation withdraws, joins the doomed set: it is marked `retired`, and
    /// the closure, the inline-cache, mega-table, cycle-cell and retire-cell
    /// passes treat its entry as they treat a withdrawn body's, so a PUBLISHED
    /// raw caller of it is withdrawn and a slot holding it is cleared. It is
    /// not re-queued: its supersede already handed the cache's reference to the
    /// retirement queue. Not counted in the returned total (no map lost it).
    ///
    /// Per cache since j27 wave 4 (lane GLOBALS, R15B-23): the process-wide
    /// walk let another VM's sweep match a body by class name or per-VM class
    /// id and mark it `retired` without withdrawing that VM's raw callers,
    /// which its own sweep then skipped (`superseded_bodies` module header).
    pub(crate) fn invalidate_matching_collecting_with(
        &self,
        reason: usize,
        predicate: impl Fn(&JitKey, &CompiledMethod) -> bool,
        superseded_predicate: Option<&dyn Fn(&CompiledMethod) -> bool>,
    ) -> (usize, Vec<WithdrawnMethodKey>, u64) {
        let _mutation = self.mutation.lock();
        let mut remove_entries = std::collections::HashSet::new();
        for shard in self.shards.iter() {
            for (_hash, (key, cm)) in shard.methods.load().iter() {
                if predicate(key, cm) {
                    remove_entries.insert(cm.entry_ptr() as usize);
                }
            }
            for (_hash, (key, cm)) in shard.osr_methods.load().iter() {
                if predicate(key, cm) {
                    remove_entries.insert(cm.entry_ptr() as usize);
                }
            }
        }
        // Round 15 wave 4 (lane vh6): the superseded candidates, only when
        // asked for (empty otherwise, which allocates nothing).
        let superseded: Vec<Arc<CompiledMethod>> = match superseded_predicate {
            Some(matches) => {
                let live = self.superseded_bodies.live();
                for cm in &live {
                    if matches(&**cm) {
                        remove_entries.insert(cm.entry_ptr() as usize);
                    }
                }
                live
            }
            None => Vec::new(),
        };

        // Nothing matched -- the overwhelmingly common outcome, because this
        // runs on EVERY class define and almost no define invalidates a CHA
        // assumption. Bail before the three whole-cache passes below.
        //
        // Each of them is a no-op in this state, but not a cheap one: the
        // closure loop below still makes a full pass over every shard, the
        // retarget pass walks every compiled method's inline-cache slots with
        // an empty set, and the publication loop CLONES all 128 shard maps
        // only to find `next.len() == old_len` and drop them again. Together
        // they were ~31% of total CPU on
        // `beans.factory.aot.BeanRegistrationsAotContributionTests`, which
        // defines classes continuously (AOT codegen + in-process javac):
        // `invalidate_cached_targets` 12.8%, `HashMap::clone` 8.0%,
        // `drop_in_place<JitKey>` 5.6%, `invalidate_for_class_change` 3.4%.
        //
        // Correct by construction: the closure loop only ever adds a method
        // whose callee is ALREADY in `remove_entries`, so an empty set stays
        // empty; `invalidate_cached_targets` of an empty set changes nothing;
        // and `retain` with an empty set removes nothing, so no snapshot is
        // published and the generation counter must not move.
        if remove_entries.is_empty() {
            release_unpublished_pins(superseded);
            return (0, Vec::new(), 0);
        }
        self.withdraw_entries_locked(_mutation, reason, remove_entries, superseded)
    }

    fn withdraw_entries_locked(
        &self,
        _mutation: parking_lot::MutexGuard<'_, ()>,
        reason: usize,
        mut remove_entries: std::collections::HashSet<usize>,
        superseded: Vec<Arc<CompiledMethod>>,
    ) -> (usize, Vec<WithdrawnMethodKey>, u64) {
        // A raw direct caller of an invalidated body is invalid too. Compute
        // the transitive reverse closure before publishing any new snapshot.
        // Also track which shards hold any doomed entry and mark them retired
        // during the final (converged) pass so we avoid two extra 64-shard scans.
        let mut method_shard_mask: u64 = 0;
        let mut osr_shard_mask: u64 = 0;
        loop {
            let before = remove_entries.len();
            for (shard_idx, shard) in self.shards.iter().enumerate() {
                let bit = 1u64 << shard_idx;
                for (is_osr, map) in [(false, shard.methods.load()), (true, shard.osr_methods.load())] {
                    for (_hash, (_key, cm)) in map.iter() {
                        let self_entry = cm.entry_ptr() as usize;
                        if remove_entries.contains(&self_entry) {
                            cm.retired.store(true, std::sync::atomic::Ordering::SeqCst);
                            if is_osr {
                                osr_shard_mask |= bit;
                            } else {
                                method_shard_mask |= bit;
                            }
                            continue;
                        }
                        // Lane rev16: and a caller whose rebound call site names it.
                        if cm
                            ._direct_callee_entries
                            .iter()
                            .any(|entry| remove_entries.contains(entry))
                            || cm.rebound_call_reaches(|entry| remove_entries.contains(&entry))
                        {
                            remove_entries.insert(self_entry);
                            cm.retired.store(true, std::sync::atomic::Ordering::SeqCst);
                            if is_osr {
                                osr_shard_mask |= bit;
                            } else {
                                method_shard_mask |= bit;
                            }
                        }
                    }
                }
            }
            // Round 15 wave 4 (lane vh6): a superseded raw caller too.
            for cm in &superseded {
                // Lane rev16: and a caller whose rebound call site names it.
                if cm
                    ._direct_callee_entries
                    .iter()
                    .any(|entry| remove_entries.contains(entry))
                    || cm.rebound_call_reaches(|entry| remove_entries.contains(&entry))
                {
                    remove_entries.insert(cm.entry_ptr() as usize);
                }
            }
            // A lambda-adapter thunk bakes a direct jump to a lambda impl
            // exactly as a compiled caller bakes a direct call, but it is not a
            // cached METHOD and so appears in none of the maps above. Without
            // this it would survive the eviction of the very body it jumps
            // into, and the inline-cache slot holding it would keep dispatching
            // to code the cache has withdrawn. See `lambda_adapter`.
            for entry in
                crate::lambda_adapter::adapters_reaching(&self.lambda_adapters, &remove_entries)
            {
                remove_entries.insert(entry);
            }
            if remove_entries.len() == before {
                break;
            }
        }
        crate::lambda_adapter::forget_adapters(&self.lambda_adapters, &remove_entries);
        // Round 15 wave 4 (lane vh6): and every doomed superseded body.
        let mut superseded_retired: Vec<&str> = Vec::new();
        for cm in &superseded {
            if remove_entries.contains(&(cm.entry_ptr() as usize)) {
                cm.retired.store(true, std::sync::atomic::Ordering::SeqCst);
                superseded_retired.push(cm.method_label.as_str());
            }
        }

        // Second lambda-adapter sweep, AFTER the mark. A thunk built for a
        // doomed impl between the first sweep (in the closure loop above) and
        // the mark passed both of `lambda_adapter_entry`'s retired checks and
        // is in no reach set yet. See `withdraw_late_lambda_adapters`.
        withdraw_late_lambda_adapters(&self.lambda_adapters, &mut remove_entries);

        // Retarget dynamic inline caches before withdrawing ownership from the
        // cache. Readers that already hold an old caller snapshot either miss
        // after this release publication or keep the callee alive through the
        // slot's strong owner until that snapshot is dropped.
        let single_target = (remove_entries.len() == 1)
            .then(|| remove_entries.iter().next().copied())
            .flatten();
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    if cm._jit_mic_slots.is_empty() && cm._jit_pic_slots.is_empty() {
                        continue;
                    }
                    if let Some(single) = single_target {
                        cm.invalidate_cached_target_single(single);
                    } else {
                        cm.invalidate_cached_targets(&remove_entries);
                    }
                }
            }
        }
        // Round 15 wave 4 (lane vh6): a superseded body is still run by its raw
        // callers, so its own slots must not keep a withdrawn target either.
        for cm in &superseded {
            cm.invalidate_cached_targets(&remove_entries);
        }
        // Round 12 wave 2 (lane calls, W2-1): the shared megamorphic table's
        // ways, after the same retired mark.
        if let Some(table) = self.mega_dispatch_table_if_any() {
            table.invalidate_targets(&remove_entries);
        }
        // Round 9 wave 7 (`mutrec7`): the cycle-edge cells too -- after the
        // retired mark above, like the inline caches. See `JitCycleEdgeCell`.
        self.invalidate_cycle_cells(Some(&remove_entries));
        // Round 9 wave 9 (`evict9`): and the retire cells pinned to a doomed
        // body, so a still-running caller dispatches instead of re-entering it.
        self.invalidate_retire_cells(Some(&remove_entries));
        // str16: and the cycle forwards to a doomed body, after the same mark.
        {
            let doomed: Vec<usize> = remove_entries.iter().copied().collect();
            self.release_cycle_forwards(Some(&doomed));
        }

        // Copy-on-write only the shards that actually lose an entry. The
        // clone is the expensive half of a publication, and an invalidation
        // typically touches one or two of the 64 shards.
        let doomed = |map: &JitCacheMap| {
            map.values()
                .any(|(_, cm)| remove_entries.contains(&(cm.entry_ptr() as usize)))
        };
        let mut removed = 0;
        let mut withdrawn: Vec<WithdrawnMethodKey> = Vec::new();
        // The method-entry half of `withdrawn`, for the sink (W3-1): one entry
        // per key, since a map holds one body per key.
        let mut entry_withdrawn: Vec<WithdrawnMethodKey> = Vec::new();
        // CODECACHE21-4 (JIT round 21 wave 4, lane codecache): the keys whose
        // OPTIMIZING OSR body this withdrawal took, charged to the method's
        // recompile cutoff by the sink.
        let mut osr_optimizing_withdrawn: Vec<WithdrawnMethodKey> = Vec::new();
        // And the method-entry keys whose body was OPTIMIZING (rev-x64 item 8):
        // charged by the body's backend, not by the tier the sink reads.
        let mut entry_optimizing_withdrawn: Vec<WithdrawnMethodKey> = Vec::new();
        let mut withdrawn_bytes: u64 = 0;
        // t1 wave 5, lane X2 (`CRATONVM_JIT_INVALIDATE_NOT_ENTRANT`, opt-in):
        // the method-entry bodies a dependency invalidation withdrew, made not
        // entrant once `mutation` is released (the pass takes it itself).
        let not_entrant_after = reason == retire_reason::INVALIDATED
            && jit_invalidate_not_entrant_enabled()
            && self.not_entrant_hook.lock().is_some();
        let mut not_entrant_due: Vec<Arc<CompiledMethod>> = Vec::new();
        for (shard_idx, shard) in self.shards.iter().enumerate() {
            let bit = 1u64 << shard_idx;
            if (method_shard_mask & bit) != 0 {
            let current = shard.methods.load();
            if doomed(&current) {
                // Retire through the quiescence queue — see `put`. An
                // invalidation is exactly the case where a body is likely to be
                // running (a redefine racing live code).
                let evicted: Vec<Arc<CompiledMethod>> = current
                    .values()
                    .filter(|(_, cm)| remove_entries.contains(&(cm.entry_ptr() as usize)))
                    .map(|(key, cm)| {
                        let withdrawn_key = (
                            Arc::clone(&key.class_name),
                            Arc::clone(&key.method_name),
                            Arc::clone(&key.descriptor),
                            key.declaring_class_id,
                        );
                        if cm.arc().used_ir_backend {
                            entry_optimizing_withdrawn.push(withdrawn_key.clone());
                        }
                        entry_withdrawn.push(withdrawn_key.clone());
                        withdrawn.push(withdrawn_key);
                        cm.arc().clone()
                    })
                    .collect();
                let mut next = (**current).clone();
                let old_len = next.len();
                next.retain(|_, (_, cm)| !remove_entries.contains(&(cm.entry_ptr() as usize)));
                removed += old_len - next.len();
                shard.methods.store(Arc::new(next));
                // Release the pre-invalidation snapshot BEFORE retiring, or the
                // retirement cannot be the release.
                //
                // `current` is the old map and still holds a strong reference
                // to every body being withdrawn. With it alive,
                // `defer_jit_owner`'s quiescent fast path takes
                // `AuthorisedReclaim::enter()` and drops an owner whose
                // refcount is still 2, so no mapping is returned there. It is
                // returned later, when this guard dies, with no authorisation
                // in scope at all — a published body's executable memory
                // reaching the OS by a path the retirement queue never saw.
                // That is what `published_code_free_audit().1` counts and what
                // `docs/jit/code-cache-lifecycle.md` calls an invariant that
                // must stay zero; `jit/tests/code_free_audit.rs` read 32 of 32
                // before this line existed.
                drop(current);
                withdrawn_bytes += evicted
                    .iter()
                    .map(|cm| cm._buffer.capacity() as u64)
                    .sum::<u64>();
                // The caller's reason, carried through unchanged -- including
                // for the transitive-closure members, which were withdrawn
                // BY that same cause. See `retire_reason`. One queue trip for
                // the whole map: see `enqueue_retired_batch`.
                if not_entrant_after {
                    not_entrant_due.extend(evicted.iter().cloned());
                }
                retire_withdrawn_bodies(evicted, reason);
            }

            }
            if (osr_shard_mask & bit) != 0 {
            let current = shard.osr_methods.load();
            if doomed(&current) {
                let evicted: Vec<Arc<CompiledMethod>> = current
                    .values()
                    .filter(|(_, cm)| remove_entries.contains(&(cm.entry_ptr() as usize)))
                    .map(|(key, cm)| {
                        let withdrawn_key = (
                            Arc::clone(&key.class_name),
                            Arc::clone(&key.method_name),
                            Arc::clone(&key.descriptor),
                            key.declaring_class_id,
                        );
                        if cm.arc().used_ir_backend {
                            osr_optimizing_withdrawn.push(withdrawn_key.clone());
                        }
                        withdrawn.push(withdrawn_key);
                        cm.arc().clone()
                    })
                    .collect();
                let mut next = (**current).clone();
                let old_len = next.len();
                next.retain(|_, (_, cm)| !remove_entries.contains(&(cm.entry_ptr() as usize)));
                removed += old_len - next.len();
                shard.osr_methods.store(Arc::new(next));
                // Same as the `methods` map above: the old snapshot has to go
                // before the retirement, or the retirement is not the release.
                drop(current);
                withdrawn_bytes += evicted
                    .iter()
                    .map(|cm| cm._buffer.capacity() as u64)
                    .sum::<u64>();
                // The caller's reason, carried through unchanged -- including
                // for the transitive-closure members, which were withdrawn
                // BY that same cause. See `retire_reason`.
                retire_withdrawn_bodies(evicted, reason);
            }
            }
        }
        // Round 15 wave 4 (lane vh6): a retired superseded body moves the
        // retire half too, so no memo keeps handing it out.
        if removed != 0 || !superseded_retired.is_empty() {
            self.bump_generation(true);
        }
        // `CRATONVM_DBG_JIT_PIN=1`: which superseded bodies this sweep caught
        // (the decisive line for the `viaCaller` page).
        if !superseded_retired.is_empty() && dbg_jit_pin_enabled() {
            eprintln!(
                "[jit-retired-superseded] {} superseded bod{} retired with {removed} withdrawn: {}",
                superseded_retired.len(),
                if superseded_retired.len() == 1 {
                    "y"
                } else {
                    "ies"
                },
                superseded_retired
                    .iter()
                    .take(16)
                    .copied()
                    .collect::<Vec<&str>>()
                    .join(", ")
            );
        }
        drop(superseded_retired);
        // j27 wave 5 (lane X2LCAP): and the doomed superseded method-entry
        // bodies, which a baked caller still enters (`doomed_superseded_entry_bodies`).
        if not_entrant_after {
            not_entrant_due.extend(doomed_superseded_entry_bodies(&superseded, &remove_entries));
        }
        // Through the retirement queue, never a plain drop: a candidate can be
        // the last reference to its body.
        release_unpublished_pins(superseded);
        // One method can hold both a method-entry and an OSR body; the caller's
        // demotion is idempotent but its accounting is not, so dedupe here.
        //
        // Through a set rather than a sort: `ClassId` is `Hash + Eq` and not
        // `Ord`, and the clone is three refcount bumps -- `Arc<str>` hashes and
        // compares by value, so no string is copied.
        let mut seen: FxHashSet<WithdrawnMethodKey> = FxHashSet::default();
        withdrawn.retain(|k| seen.insert(k.clone()));
        // W3-1: every door but `clear_all` withdraws through here, so this is
        // where their methods leave their tier. After `mutation`, so the
        // tiered manager's lock, which the sink takes, is never nested
        // inside this cache's.
        drop(_mutation);
        // t1 wave 5, lane X2: after `mutation` (the pass takes it), each body
        // no longer published; released through the retirement queue.
        if !not_entrant_due.is_empty() {
            let patched = self.make_not_entrant_after_eviction(&not_entrant_due);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] invalidate-not-entrant: {patched} of {} withdrawn bodies patched",
                    not_entrant_due.len()
                );
            }
            release_unpublished_pins(not_entrant_due);
        }
        self.report_withdrawn(&entry_withdrawn);
        self.report_charged_withdrawals(
            reason,
            &entry_optimizing_withdrawn,
            &osr_optimizing_withdrawn,
        );
        (removed, withdrawn, withdrawn_bytes)
    }

    /// Invalidate every compiled method in the cache.
    ///
    /// JVMTI redefine can invalidate caller-side direct calls and inline caches,
    /// not just methods declared by the redefined class. A full flush is rare
    /// but conservative; ownership pins any body still referenced by an
    /// already-loaded reader while new snapshots become empty atomically.
    ///
    /// Also **arms the install barrier**: emptying the maps retires every body
    /// that is already here, but says nothing about the bodies still being
    /// compiled from the same retired inputs. Raising `flush_barrier` under
    /// `mutation` — the lock the two publication paths also hold — makes those
    /// unpublishable, which is the half of the redefinition protocol the
    /// compilation broker cannot supply (it stamps no epoch on a queued or
    /// in-flight request, so a completion cannot notice its bytecode is gone).
    pub fn clear_all(&self) -> usize {
        self.clear_all_collecting().0
    }

    /// [`Self::clear_all`], also handing back the key of every body it
    /// withdrew (method-entry and OSR, deduplicated), so the owning VM can
    /// demote their tiered state (`TieredCompilationManager::note_body_withdrawn`):
    /// `current_tier` only advances, so a hot method at C2 whose body a flush
    /// withdrew was otherwise never offered for compilation again
    /// (interpreter round i1 wave 20, lane L1).
    pub fn clear_all_collecting(&self) -> (usize, Vec<WithdrawnMethodKey>) {
        // Declared first, so its one queue trip follows the lock's release.
        let _retire_batch = RetireBatch::open();
        let _mutation = self.mutation.lock();
        // Advance first, then arm, so the barrier is strictly greater than the
        // stamp of every compilation that had already started.
        let barrier = bump_jit_install_epoch();
        self.flush_barrier
            .fetch_max(barrier, std::sync::atomic::Ordering::AcqRel);
        // The maps this set summarises are about to be emptied.
        self.inlined_class_names.lock().clear();
        // TAIL9-20-2: a flush can follow a redefinition, after which a pc of
        // the enclosing callee may name another call.
        self.sp_nested_miss_evidence.lock().clear();
        // A compilation that began before this flush is refused by the barrier
        // above, so no later publication can need a record older than it.
        self.invalidations.lock().records.clear();
        // The barrier above is above every per-class redefinition barrier.
        self.redefinition_barriers.lock().clear();
        // Every body is retired before ANY inline cache is cleared — a full
        // pass first, because the per-shard loop below clears one shard's slots
        // before it has reached the bodies of the next. See
        // `invalidate_matching`.
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    cm.retired.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        let mut count = 0;
        let mut withdrawn: Vec<WithdrawnMethodKey> = Vec::new();
        // Round 9 wave 7 (`mutrec7`): every cycle-edge cell goes back to
        // dispatch, after the retired mark above. See `JitCycleEdgeCell`.
        self.invalidate_cycle_cells(None);
        self.invalidate_retire_cells(None);
        self.release_cycle_forwards(None);
        // Round 12 wave 2 (lane calls, W2-1): the shared megamorphic table,
        // after the retired mark, like the inline caches below.
        if let Some(table) = self.mega_dispatch_table_if_any() {
            table.clear_entries();
        }
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (key, cm)) in map.iter() {
                    cm.clear_cached_targets();
                    withdrawn.push((
                        Arc::clone(&key.class_name),
                        Arc::clone(&key.method_name),
                        Arc::clone(&key.descriptor),
                        key.declaring_class_id,
                    ));
                }
            }
            count += shard.methods.load().len() + shard.osr_methods.load().len();
            // Retire through the quiescence queue — see `put`. `clear_all` runs
            // on JVMTI redefine, which does not stop the world here, so every
            // body it drops may be live.
            let evicted: Vec<Arc<CompiledMethod>> =
                [shard.methods.load(), shard.osr_methods.load()]
                    .iter()
                    .flat_map(|map| {
                        map.values()
                            .map(|(_, cm)| cm.arc().clone())
                            .collect::<Vec<_>>()
                    })
                    .collect();
            shard.methods.store(Arc::new(FxHashMap::default()));
            shard.osr_methods.store(Arc::new(FxHashMap::default()));
            // `INVALIDATED`, not `SHUTDOWN`, and this is where the
            // proposal's mapping was wrong. `clear_all`'s two production
            // callers are `vm/src/vm/vm_exec.rs`'s `redefineClass` handler
            // and `vm/src/vm/vm_init.rs`'s `jit_invalidate_adapter`
            // (a synthetic-stub layout change); both are redefinition-class
            // invalidations of a RUNNING VM. Nothing calls this at process
            // exit. See `retire_reason::SHUTDOWN` for why that code is kept
            // with no site anyway.
            //
            // One queue trip per shard rather than per body: a full flush is
            // the largest invalidation there is, and per-body retirement ran
            // a whole-queue drain per body (`enqueue_retired_batch`).
            retire_withdrawn_bodies(evicted, retire_reason::INVALIDATED);
        }
        // Unconditionally, empty or not (interpreter round i1 wave 39, lane
        // L2; item 4 of `i38-L6-compiled-dispatch-leftovers`): the retire unit
        // is what makes every dispatch helper drop its `site_keyed_memos!`
        // memos, and a flush for a synthetic stub promoted in place
        // (`jit_invalidate_adapter`) must reach the memos of a compiled
        // caller that is still RUNNING although its body was already
        // withdrawn -- the one case in which the cache is empty here. A flush
        // is rare (a redefinition, a layout upgrade), and it held a body in
        // every other case, so this adds no bump anywhere else.
        self.bump_generation(true);
        // Every bucket's stamp moves (`callee_publish_stamp`): a flush follows
        // a redefinition or a layout upgrade, which can lift a refusal.
        self.callee_publish_flushes
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        // A method can hold both a method-entry and an OSR body.
        let mut seen: FxHashSet<WithdrawnMethodKey> = FxHashSet::default();
        withdrawn.retain(|k| seen.insert(k.clone()));
        // W3-1, as in `invalidate_matching_collecting`, but every key: after a
        // flush no body of any kind is left, so a tier above the interpreter
        // is stale for an OSR-only key too.
        drop(_mutation);
        self.report_withdrawn(&withdrawn);
        (count, withdrawn)
    }

    /// Census, read-only (stage 1 of
    /// `i14-L3-proposal-class-scoped-redefinition-invalidation`): how many
    /// published bodies (method-entry and OSR) a class-scoped eviction for a
    /// redefinition of `(class_id, class_name)` would have to withdraw — the
    /// bodies the class declares, the bodies whose `inlined_methods` name it
    /// (an inlined callee, or a receiver class a guarded speculation relied
    /// on), and the transitive reverse closure over baked direct calls, the
    /// closure [`Self::invalidate_matching_collecting`] computes. The
    /// redefinition path asks it just before [`Self::clear_all`], which
    /// withdraws every body; the difference is what a scoped eviction would
    /// keep. Evicts nothing; one scan of the cache per closure round.
    pub fn redefinition_dependents(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> usize {
        let mut dependents = std::collections::HashSet::new();
        let callers = self.redefinition_call_dependents(class_id, class_name);
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (key, cm)) in map.iter() {
                    if depends_on_redefined_class(key, cm, class_id, class_name)
                        || callers.contains(&cm.artifact_id)
                    {
                        dependents.insert(cm.entry_ptr() as usize);
                    }
                }
            }
        }
        if dependents.is_empty() {
            return 0;
        }
        loop {
            let before = dependents.len();
            for shard in self.shards.iter() {
                for map in [shard.methods.load(), shard.osr_methods.load()] {
                    for (_hash, (_key, cm)) in map.iter() {
                        // Lane rev16: and a caller whose rebound call site names it.
                        if cm
                            ._direct_callee_entries
                            .iter()
                            .any(|entry| dependents.contains(entry))
                            || cm.rebound_call_reaches(|entry| dependents.contains(&entry))
                        {
                            dependents.insert(cm.entry_ptr() as usize);
                        }
                    }
                }
            }
            if dependents.len() == before {
                break;
            }
        }
        dependents.len()
    }

    /// Stage 2b of `i14-L3-proposal-class-scoped-redefinition-invalidation`
    /// (interpreter round i1 wave 20, lane L1): withdraw exactly the bodies a
    /// redefinition of `(class_id, class_name)` can make stale, instead of
    /// [`Self::clear_all`]. Returns the number withdrawn and their keys (for
    /// the tiered manager's demotion).
    ///
    /// Withdrawn: the bodies the class declares (method-entry and OSR), the
    /// bodies whose `inlined_methods` name it (a callee the single-pass plan
    /// spliced, a receiver a guarded speculation or a class-hierarchy bind
    /// relied on), and [`Self::invalidate_matching_collecting`]'s transitive
    /// closure over baked direct calls, lambda adapters, cycle and retire
    /// cells. The census [`Self::redefinition_dependents`] counts the same
    /// set. The caller still owes [`Self::bump_redefine_epoch`], which each
    /// inline-cache slot checks on its next HELPER visit only: a filled slot
    /// is called by generated code without one, so (wave 21) every slot whose
    /// retained body is stale, published or superseded, is cleared here
    /// ([`Self::invalidate_inline_caches_reaching_redefined`]), and every
    /// retire and cycle cell holding one
    /// ([`Self::invalidate_cells_reaching_redefined`]).
    ///
    /// ONLY for a class no compile copied bytecode of
    /// ([`Self::bytecode_was_copied`]): a splice the IR tier made, a nested
    /// single-pass splice and an elided empty constructor record no
    /// dependency a scan could find, so the VM takes the full flush for such
    /// a class (`JitRealm::note_class_redefinition_of`).
    ///
    /// In flight: the record logged BEFORE the scan refuses a later
    /// publication of a compile that began earlier and declares or inlined
    /// the class ([`Self::dependencies_are_current`]); a compile that baked a
    /// direct call to a body withdrawn here is refused by
    /// `prepare_for_publication`'s retired-callee check. No flush barrier is
    /// armed, so unrelated compiles still publish; the per-class barrier
    /// answers [`Self::compiled_since_redefinition_of`].
    pub fn invalidate_for_redefinition(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> (usize, Vec<WithdrawnMethodKey>) {
        // One queue trip for the class's dependents and their inline caches.
        let _retire_batch = RetireBatch::open();
        let barrier = bump_jit_install_epoch();
        {
            let mut barriers = self.redefinition_barriers.lock();
            let slot = barriers.entry(class_id).or_insert(0);
            *slot = (*slot).max(barrier);
        }
        self.withdraw_dependents_of_class(class_id, class_name)
    }

    /// [`Self::invalidate_for_redefinition`] without the per-class
    /// redefinition barrier: withdraw the bodies a change to `(class_id,
    /// class_name)` would make stale by their own record, their baked callers'
    /// closure, and the inline-cache slots and cells holding one, and log the
    /// record that refuses a compile of the class already in flight. For a
    /// class that is NOT redefined (interpreter round i1 wave 38, lane L1: a
    /// debugger breakpoint gained in one of its methods,
    /// [`Self::withdraw_class_for_the_interpreter`]): the barrier is what
    /// [`Self::compiled_since_redefinition_of`] reads as "compiled before the
    /// class's last redefinition", and raising it for a class an agent had
    /// redefined earlier would turn a current body's granted exits into
    /// refused resumes.
    pub(crate) fn withdraw_dependents_of_class(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> (usize, Vec<WithdrawnMethodKey>) {
        self.log_invalidation(InvalidationRecord::RedefinedClass {
            class_id,
            class_name: Arc::from(class_name),
        });
        // Built under `mutation`, before the scan, by the inline-cache pass
        // (interpreter round i1 wave 21, lane L1), which must see every body
        // while it is still published: one this redefinition withdraws may be
        // running, and keeps calling through its own inline caches. A body
        // published between the two passes that bakes a body of the class is
        // refused by the logged record or the retired-callee check.
        let callers = self.invalidate_inline_caches_reaching_redefined(class_id, class_name);
        let (removed, withdrawn, _bytes) =
            self.invalidate_matching_collecting(retire_reason::INVALIDATED, |key, cm| {
                depends_on_redefined_class(key, cm, class_id, class_name)
                    || callers.contains(&cm.artifact_id)
            });
        self.invalidate_cells_reaching_redefined(class_id, class_name, &callers);
        (removed, withdrawn)
    }

    /// Send every published body's inline-cache slot whose retained target a
    /// redefinition of `(class_id, class_name)` makes stale back to its
    /// helper, and answer [`Self::redefinition_call_dependents`] (interpreter
    /// round i1 wave 21, lane L1).
    ///
    /// The scoped eviction's scan clears the slots whose target it withdraws,
    /// i.e. a PUBLISHED body. A slot keeps the body it was filled with after a
    /// tier-up superseded it (a MIC filled while the C1 body was current
    /// holds it for as long as its receiver matches), and generated code
    /// calls a filled slot without reaching the helper, where the per-cache
    /// redefine epoch ([`Self::bump_redefine_epoch`]) is checked. So after
    /// the redefinition every call through such a slot ran the class's OLD
    /// bytecode -- for any caller, not only one already running. The full
    /// flush cleared every slot ([`Self::clear_all`]); this clears exactly the
    /// slots whose target is stale ([`redefinition_makes_stale`]), found by the
    /// owner each slot retains.
    pub(crate) fn invalidate_inline_caches_reaching_redefined(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> FxHashSet<u64> {
        let _mutation = self.mutation.lock();
        let reached = self.redefinition_call_dependents(class_id, class_name);
        let stale = |body: &Arc<CompiledMethod>| {
            redefinition_makes_stale(body, class_id, class_name, &reached)
        };
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    cm.invalidate_cached_owners(&stale);
                }
            }
        }
        // Round 12 wave 2 (lane calls, W2-1): and the shared megamorphic table.
        if let Some(table) = self.mega_dispatch_table_if_any() {
            table.invalidate_owned_targets(&stale);
        }
        reached
    }

    /// Clear every retire and cycle-edge cell that holds a body a
    /// redefinition of `(class_id, class_name)` makes stale, whether or not
    /// that body is still published (interpreter round i1 wave 21, lane L1).
    ///
    /// [`Self::invalidate_matching_collecting`] clears the cells holding a
    /// body it withdrew, i.e. a PUBLISHED one. A cell can hold another: a
    /// retire cell keeps the body its caller baked after a tier-up superseded
    /// it (it never takes a newer one), and a cycle cell keeps an older body
    /// when the newer one is not admitted. Such a body of the redefined class
    /// -- or one that inlined it, or calls into it (`reached`,
    /// [`Self::redefinition_call_dependents`]; [`redefinition_makes_stale`])
    /// -- runs the old bytecode, and a caller still running (an OSR'd loop)
    /// kept entering it through the cell. A full flush clears every cell
    /// ([`Self::clear_all`]); this is its scoped twin. A cleared retire cell
    /// leaves the registry (nothing refills it); a cycle cell stays and is
    /// refilled by its key's next publication.
    pub(crate) fn invalidate_cells_reaching_redefined(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        reached: &FxHashSet<u64>,
    ) {
        let _mutation = self.mutation.lock();
        let stale = |cell: &Arc<JitCycleEdgeCell>| -> bool {
            let owner = cell.owner.lock();
            let holds_stale_body = owner
                .as_ref()
                .is_some_and(|body| redefinition_makes_stale(body, class_id, class_name, reached));
            holds_stale_body
        };
        // Every upgraded cell is released after the registry locks are
        // dropped, through `release_cycle_cells`: a cell whose owning
        // artifact went away meanwhile must not drop a published body outside
        // the retirement queue.
        let mut upgraded: Vec<(Arc<JitCycleEdgeCell>, bool)> = Vec::new();
        {
            let mut registry = self.retire_cells.lock();
            registry.retain(|_, cells| {
                cells.retain(|w| match w.upgrade() {
                    Some(cell) => {
                        let doomed = stale(&cell);
                        upgraded.push((cell, doomed));
                        !doomed
                    }
                    None => false,
                });
                !cells.is_empty()
            });
        }
        {
            let registry = self.cycle_cells.lock();
            for waiting in registry.values() {
                for w in waiting {
                    if let Some(cell) = w.upgrade() {
                        let doomed = stale(&cell);
                        upgraded.push((cell, doomed));
                    }
                }
            }
        }
        let mut cells = Vec::with_capacity(upgraded.len());
        for (cell, doomed) in upgraded {
            if doomed {
                cell.clear();
            }
            cells.push(cell);
        }
        release_cycle_cells(cells);
    }

    /// The `artifact_id`s of the bodies a redefinition of `(class_id,
    /// class_name)` reaches through baked direct calls: every body reachable
    /// from this cache's published bodies along `_direct_callee_roots`, at any
    /// depth, that declares the class, inlined from it, or calls (at any
    /// depth) a body that does.
    ///
    /// Walks the ROOTS, not the published entries, and that is the point: a
    /// tier-up replaces a body in the cache without retiring it (it is still
    /// correct code), and a caller that baked the old body keeps calling it.
    /// [`Self::invalidate_matching_collecting`]'s closure matches callers by
    /// the entries of PUBLISHED bodies only, so it cannot see a caller of a
    /// superseded body of the redefined class, which runs the old bytecode. A
    /// full flush withdrew such a caller with everything else; a scoped
    /// eviction has to find it (interpreter round i1 wave 20, lane L1).
    pub(crate) fn redefinition_call_dependents(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> FxHashSet<u64> {
        // Full snapshots: every reference below borrows from them.
        let maps: Vec<Arc<JitCacheMap>> = self
            .shards
            .iter()
            .flat_map(|shard| [shard.methods.load_full(), shard.osr_methods.load_full()])
            .collect();
        let mut callees_of: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        let mut reached: FxHashSet<u64> = FxHashSet::default();
        let mut rebound_targets: Vec<u64> = Vec::new();
        let mut stack: Vec<&CompiledMethod> = maps
            .iter()
            .flat_map(|map| map.values().map(|(_, cm)| &**cm))
            .collect();
        while let Some(body) = stack.pop() {
            if callees_of.contains_key(&body.artifact_id) {
                continue;
            }
            if body.owner_class_id == class_id.as_u32()
                || body
                    .inlined_methods
                    .iter()
                    .any(|(cn, _, _)| cn == class_name)
                || body.copied_bytecode_of(class_id)
            {
                reached.insert(body.artifact_id);
            }
            let mut callees = Vec::with_capacity(body._direct_callee_roots.len());
            for root in &body._direct_callee_roots {
                let callee: &CompiledMethod = root;
                callees.push(callee.artifact_id);
                stack.push(callee);
            }
            // Lane rev16: a rebound call site's body is a callee too
            // (`CompiledMethod::rebound_call_reaches`); not a root, so it is
            // walked only when published (below).
            let rebound = body.rebound_callee_artifacts();
            if !rebound.is_empty() {
                rebound_targets.extend(rebound.iter().copied());
                callees.extend(rebound);
            }
            callees_of.insert(body.artifact_id, callees);
        }
        // A rebound target this walk never reached (no longer published, so
        // its own reach is unknown here) counts as reached: its callers are
        // withdrawn, the direction that stays correct.
        for id in rebound_targets {
            if !callees_of.contains_key(&id) {
                reached.insert(id);
            }
        }
        if reached.is_empty() {
            return reached;
        }
        loop {
            let before = reached.len();
            for (body, callees) in &callees_of {
                if !reached.contains(body) && callees.iter().any(|c| reached.contains(c)) {
                    reached.insert(*body);
                }
            }
            if reached.len() == before {
                break;
            }
        }
        reached
    }

    /// Send every published body's machine-code inline caches back to their
    /// helpers — each MIC's compiled-entry word and every PIC way — and retire
    /// nothing. Returns the number of bodies visited.
    ///
    /// The `clear_cached_targets` pass of [`Self::clear_all`] alone: no body is
    /// retired, no barrier is armed and the generation does not move, so every
    /// artifact stays published and callable, and a slot refills through its
    /// helper on its next miss. For the VM's JVMTI interpreter-only transition
    /// (`vm/src/runtime/jvmti.rs`, `publish_union_listener_flags`): a compiled
    /// frame that is already running keeps calling through its inline caches
    /// without ever reaching a helper, and the helpers are where the VM stands
    /// down to the interpreter. Cycle-edge and RETIRE cells and baked direct
    /// CALLs are deliberately left alone — a cleared cell is refilled only by a
    /// later publication (or never, for a RETIRE cell), which would turn a
    /// transient agent request into a lasting slowdown.
    pub fn clear_inline_caches(&self) -> usize {
        let _mutation = self.mutation.lock();
        let mut bodies = 0;
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    cm.clear_cached_targets();
                    bodies += 1;
                }
            }
        }
        // Round 12 wave 2 (lane calls, W2-1): and the shared megamorphic table.
        if let Some(table) = self.mega_dispatch_table_if_any() {
            table.clear_entries();
        }
        bodies
    }
}

impl Default for JitCache {
    fn default() -> Self {
        Self::new()
    }
}


impl std::fmt::Debug for JitCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "JitCache({} methods, {} osr methods)",
            self.shards
                .iter()
                .map(|s| s.methods.load().len())
                .sum::<usize>(),
            self.shards
                .iter()
                .map(|s| s.osr_methods.load().len())
                .sum::<usize>()
        )
    }
}

// ---------------------------------------------------------------------------
// Compilation entry point
// ---------------------------------------------------------------------------

// round-7 fix (bug 1, CRIT, permanent recompile waste):
//
// `try_compile` returns `None` for two distinct reasons:
//   1. Transient failure (resolver returned None, executable-buffer alloc
//      failed, profile not yet present, etc.) — these are worth re-trying
//      later once the missing state is populated.
//   2. Permanent bail — the x64 backend hit an unsupported pattern
//      (e.g. >ARG_REGS direct-call args at x64.rs:10380/10422/10583/10665)
//      and returned `false` from `compile_bytecode`.  These will *always*
//      fail until the JIT grows the missing feature, but the interpreter
//      keeps polling `try_compile` every 2000 invocations forever — each
//      attempt burns ~50µs walking the same scan/IR pipeline only to bail
//      at the same site.
//
// We can't trivially distinguish (1) from (2) without plumbing a richer
// return type through the entire compilation pipeline.  As a pragmatic
// approximation, we treat the *most expensive* failure mode — the one
// where x64::compile() runs the full scan/IR/optimization pipeline and
// then returns None because of a backend bail — as permanent.  All the
// "transient" None paths (resolver-fail, allocator-fail) short-circuit
// *before* x64::compile and therefore don't pollute the bail-list.
//
// Implementation: ONE verdict store per VM (`JitVerdictRegistry`, owned by the
// VM's tiered manager; process-wide until interpreter round i1 wave 11) for
// this module's three negative memos -- the bail list, the refusal reason
// recorded beside it, and the per-(method, pc) OSR entry rejects further down.
//
// They used to be three separate process-wide sets keyed by a bare hash and
// never pruned. A verdict recorded against bytecode that a JVMTI redefinition
// had since replaced, or against a class that had since been unloaded (and its
// name reused), refused the new code for the life of the process. Now:
//
//   * every entry stores the full (class, method, descriptor) names and a hit
//     verifies them, so a hash collision cannot refuse an innocent method;
//   * every verdict is stamped with the registry's redefine epoch at record
//     time and stops counting once a redefinition in the same VM moves it;
//   * a verdict that depends on compile-time state rather than on the bytecode
//     alone (`osr_refusal_depends_on_compile_state`) is also stamped with the
//     registry's install epoch, which that VM's code-cache flush moves too;
//   * `forget_jit_verdicts_for_class` drops a class's entries outright; the
//     tiered manager calls it on class unload and on redefinition.
//
// Every entry is keyed by the declaring class's `ClassId` as well as its names,
// and a hit verifies both. Two same-named classes in different loaders (two
// webapps, a devtools restart loader, Groovy or JSR-223 scripts) therefore keep
// separate verdicts: one copy being bail-listed no longer keeps the other
// interpreted, and unloading or redefining one copy leaves the other's verdicts
// alone. `ClassId(0)` is the "no resolved identity" sentinel; a caller that
// passes it shares its entries with every other id-less caller of the same
// names, which is the old behaviour.

pub(crate) static JIT_BAIL_SHORTCIRCUITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// When a compile verdict was recorded, for deciding whether it still counts.
///
/// Both epochs are the OWNING registry's ([`JitVerdictRegistry::note_redefinition`],
/// [`JitVerdictRegistry::note_code_cache_flush`]), not the process-wide
/// redefinition count (removed in interpreter round i1 wave 23) /
/// [`jit_install_epoch`]: another VM's redefinition or
/// code-cache flush says nothing about this VM's bytecode or compile state
/// (interpreter round i1 wave 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerdictStamp {
    /// The registry's redefine epoch when the verdict was recorded.
    pub(crate) redefine_epoch: u32,
    /// The registry's install epoch when the verdict was recorded, for a
    /// verdict that depends on compile-time state; `None` for one about the
    /// bytecode alone.
    pub(crate) install_epoch: Option<u64>,
}

impl VerdictStamp {
    pub(crate) fn is_current_at(self, redefine: u32, install: u64) -> bool {
        self.redefine_epoch == redefine && self.install_epoch.map_or(true, |epoch| epoch == install)
    }
}

/// Every compile verdict recorded about one method of one loaded class.
pub(crate) struct MethodVerdicts {
    /// The declaring class's identity; `ClassId(0)` when the recorder had none.
    pub(crate) class_id: cratonvm_types::ClassId,
    pub(crate) class_name: Box<str>,
    pub(crate) method_name: Box<str>,
    pub(crate) descriptor: Box<str>,
    /// Bail-listed by [`JitVerdictRegistry::mark_bail_listed`].
    pub(crate) bail_listed: Option<VerdictStamp>,
    /// The last refusal site a compile of the method recorded.
    pub(crate) bail_site: Option<(VerdictStamp, (&'static str, u32, u32))>,
    /// OSR entry pcs whose compiled body refused entry.
    pub(crate) osr_rejects: Vec<(usize, VerdictStamp)>,
}

impl MethodVerdicts {
    pub(crate) fn new(
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Self {
        Self {
            class_id,
            class_name: Box::from(class_name),
            method_name: Box::from(method_name),
            descriptor: Box::from(descriptor),
            bail_listed: None,
            bail_site: None,
            osr_rejects: Vec::new(),
        }
    }

    /// Whether this entry is about exactly this method of exactly this class.
    pub(crate) fn is_for(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        self.class_id == class_id
            && &*self.class_name == class_name
            && &*self.method_name == method_name
            && &*self.descriptor == descriptor
    }

    /// Whether this entry is about a method of the class `(class_id,
    /// class_name)`, by the rule `tiered::MethodKey::belongs_to` uses: identity
    /// decides when both sides carry one, the name decides otherwise.
    pub(crate) fn belongs_to_class(&self, class_id: cratonvm_types::ClassId, class_name: &str) -> bool {
        if self.class_id.as_u32() != 0 && class_id.as_u32() != 0 {
            self.class_id == class_id
        } else {
            &*self.class_name == class_name
        }
    }
}

/// The classes one class-unload transaction removed, as `(ClassId, name)`
/// pairs, indexed for the batch invalidations that sweep a JIT table ONCE for
/// the whole set (gc-common w36-c). The per-class loop in
/// `vm/src/memory/gc.rs::unload_dead_class_metadata` swept every table once
/// per class:
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// [`UnloadedClassSet::owns`] is the id-then-name rule of
/// `tiered::MethodKey::belongs_to`, asked of every member at once: a key with
/// a non-zero id is owned by a member with the same id, or by an id-less
/// member of the same name; an id-less key is owned by any member of the same
/// name.
pub struct UnloadedClassSet<'a> {
    pub(crate) classes: Vec<(cratonvm_types::ClassId, &'a str)>,
    /// Every member's id, `ClassId(0)` included (the JIT cache matches its
    /// `declaring_class_id` by plain equality, as its per-class form does).
    pub(crate) ids: rustc_hash::FxHashSet<cratonvm_types::ClassId>,
    /// Every member's name.
    pub(crate) names: rustc_hash::FxHashSet<&'a str>,
    /// The names of the members that carry no id.
    pub(crate) idless_names: rustc_hash::FxHashSet<&'a str>,
    /// gci i12/ul F3: `(declaring class of a body, inlined class name)` ->
    /// "that name, as the body's own loader sees it, is a LIVE class, not a
    /// member" ([`Self::with_live_namesakes`]). `None` = match inlinees by
    /// name alone, as before.
    pub(crate) live_namesake: Option<&'a dyn Fn(cratonvm_types::ClassId, &str) -> bool>,
}

impl<'a> UnloadedClassSet<'a> {
    /// gci i12/ul F3 (`CRATONVM_JIT_UNLOAD_KEEPS_LIVE_NAMESAKES`, read by the
    /// VM): `inlined_methods` records an inlinee by NAME only, so a redeploy's
    /// unload of the OLD loader's `p/X` withdrew every compiled body of the
    /// NEW loader that inlined its own, live, `p/X`. With this predicate a
    /// keyed body survives an inlinee name when the predicate says the body's
    /// declaring class resolves that name, in its own namespace, to a live
    /// class. Superseded bodies (no key) are still judged by name.
    pub fn with_live_namesakes(
        mut self,
        live_namesake: &'a dyn Fn(cratonvm_types::ClassId, &str) -> bool,
    ) -> Self {
        self.live_namesake = Some(live_namesake);
        self
    }

    /// gci i12/ul F3: does a body declared by `declaring` that inlined
    /// `inlined_name` depend on a member? By name, unless the namesake
    /// predicate proves the name resolves to a live class for that body.
    pub fn inlinee_is_member(
        &self,
        declaring: cratonvm_types::ClassId,
        inlined_name: &str,
    ) -> bool {
        self.names.contains(inlined_name)
            && !self
                .live_namesake
                .is_some_and(|live| live(declaring, inlined_name))
    }

    /// Index `classes`. Duplicates are harmless.
    pub fn new(classes: impl IntoIterator<Item = (cratonvm_types::ClassId, &'a str)>) -> Self {
        let classes: Vec<(cratonvm_types::ClassId, &'a str)> = classes.into_iter().collect();
        let mut ids = rustc_hash::FxHashSet::default();
        let mut names = rustc_hash::FxHashSet::default();
        let mut idless_names = rustc_hash::FxHashSet::default();
        for &(id, name) in &classes {
            ids.insert(id);
            names.insert(name);
            if id.as_u32() == 0 {
                idless_names.insert(name);
            }
        }
        Self {
            classes,
            ids,
            names,
            idless_names,
            live_namesake: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.classes.len()
    }

    /// The members, in the order given.
    pub fn iter(&self) -> impl Iterator<Item = (cratonvm_types::ClassId, &'a str)> + '_ {
        self.classes.iter().copied()
    }

    /// Every member's name.
    pub fn names(&self) -> &rustc_hash::FxHashSet<&'a str> {
        &self.names
    }

    /// Whether a member has exactly this id (`ClassId(0)` included).
    pub fn contains_id(&self, class_id: cratonvm_types::ClassId) -> bool {
        self.ids.contains(&class_id)
    }

    /// Whether a member has exactly this NON-ZERO id.
    pub fn contains_nonzero_id(&self, class_id: cratonvm_types::ClassId) -> bool {
        class_id.as_u32() != 0 && self.ids.contains(&class_id)
    }

    /// Whether a key of the class `(class_id, class_name)` belongs to some
    /// member, by `tiered::MethodKey::belongs_to`'s rule (see the type doc).
    pub fn owns(&self, class_id: cratonvm_types::ClassId, class_name: &str) -> bool {
        if class_id.as_u32() != 0 {
            self.ids.contains(&class_id) || self.idless_names.contains(class_name)
        } else {
            self.names.contains(class_name)
        }
    }
}


/// One VM's compile verdicts: the bail list, the refusal reasons recorded
/// beside it, the per-(method, pc) OSR entry rejects, and the two
/// compile-time recursion-cycle sets
/// ([`JitVerdictRegistry::direct_call_requires_dispatch`]).
///
/// Interpreter round i1 wave 11: these were three process-global statics
/// (`JIT_VERDICTS`, `JIT_RECURSIVE_CYCLE_METHODS`,
/// `JIT_STICKY_DISPATCH_METHODS`). The verdicts are keyed by `ClassId`, which
/// is unique only within one VM (every `ClassStore` allocates from 0), so one
/// VM's bail-listing answered for another VM's method of the same id and
/// names; the cycle sets were keyed by names alone. Each VM now owns one, in
/// its tiered manager ([`tiered::TieredCompilationManager::verdicts`]), which
/// also forgets a class's verdicts on unload and redefinition. A compile
/// reaches it through [`CompileRequest::verdicts`]; the VM's doors that record
/// or ask outside a request name it directly.
///
/// A reader and a writer must use the SAME registry: a method bail-listed in
/// one and asked of another is re-compiled on every attempt, which is the
/// waste the list exists to stop (RBC.4: 35,923 re-run pipelines on
/// `Nat.inc`). The free functions over [`process_jit_verdicts`], the
/// registry of compiles with no VM in scope (`is_jit_bail_listed`,
/// `mark_jit_bail_listed`, ...), are therefore `#[cfg(test)]` since
/// interpreter round i1 wave 12: a VM path cannot reach the wrong registry
/// by picking the free spelling of a method. Its diagnostic counts are on the
/// `CRATONVM_DBG=jit-method-stats` line (`tiered::dump_method_stats_to_stderr`).
///
/// No method holds two of its locks at once, and none calls out of the
/// registry while holding one (a cycle-set ask resolves the caller's class
/// identity after its read guard is dropped).
///
/// A verdict stops counting when THIS registry's epochs move
/// ([`JitVerdictRegistry::note_redefinition`],
/// [`JitVerdictRegistry::note_code_cache_flush`]), which the owning VM
/// advances beside its own `JitCache::bump_redefine_epoch` /
/// `JitCache::clear_all`. Until interpreter round i1 wave 13 the stamps read
/// the process-wide counters, so one VM's redefinition re-opened every
/// bail-listed method and OSR reject of every other VM in the process.
/// A VM's registry reads its flush epoch from the VM's code-state epoch
/// ([`JitVerdictRegistry::with_flush_epoch`], [`FlushEpoch`]) and, since
/// interpreter round i1 wave 17, its redefine epoch from the VM's redefine
/// epoch ([`JitVerdictRegistry::with_vm_epochs`], [`RedefineEpoch`]).
#[derive(Default)]
pub struct JitVerdictRegistry {
    pub(crate) verdicts: parking_lot::RwLock<rustc_hash::FxHashMap<u64, MethodVerdicts>>,
    /// This registry's identity, for the compile witness.
    pub(crate) id: VerdictRegistryId,
    /// Redefinitions of this VM, as far as its verdicts are concerned.
    pub(crate) redefine_epoch: RedefineEpoch,
    /// Code-cache flushes (and redefinitions) of this VM, for the verdicts
    /// that depend on compile-time state.
    pub(crate) install_epoch: FlushEpoch,
    /// The redefinition census (reporting only).
    pub(crate) redefinition_census: RedefinitionCensus,
    /// Every method seen on a compile-time recursion cycle, forever.
    pub(crate) recursive_cycle_methods: parking_lot::RwLock<CycleSet>,
    /// The subset of `recursive_cycle_methods` whose calls stay on the
    /// dispatch route; see [`JitVerdictRegistry::direct_call_requires_dispatch`].
    pub(crate) sticky_dispatch_methods: parking_lot::RwLock<CycleSet>,
    /// The optimizing tier's planted-trap memo and site-trap decisions
    /// (`ir.rs`, [`JitVerdictRegistry::claim_site_trap_decision`]); process
    /// statics keyed by name until interpreter round i1 wave 23.
    pub(crate) site_traps: ir::SiteTrapMemo,
    /// The IR acceptance gate's refusal memo (`ir_evidence.rs`,
    /// [`JitVerdictRegistry::ir_refusal_key`]); a process static until
    /// interpreter round i1 wave 23.
    pub(crate) ir_refusals: ir_evidence::IrRefusalMemo,
    /// JIT round 19 (lane selflock19, SYNCDOOR618-2 / SDB17-2): the
    /// self-lock admission and monitor-drop census (reporting only).
    pub(crate) self_lock_census: self_lock_census::SelfLockCensus,
    /// JIT round 23 wave 4 (lane L1; `CRATONVM_JIT_IR_SELFREC_FRAME_EVIDENCE`):
    /// the frame size of the last single-pass body published for a method,
    /// keyed by its IR refusal key (method hash, declaring class, redefine
    /// epoch), for the optimizing acceptance gate's frame evidence
    /// ([`ir_selfrec_frame_evidence`]).
    pub(crate) sp_frame_sizes: parking_lot::Mutex<rustc_hash::FxHashMap<u64, i32>>,
}

impl JitVerdictRegistry {
    /// JIT round 23 wave 4 (lane L1): record the frame size of the single-pass
    /// body this compile produced for the method keyed `key`.
    pub(crate) fn note_sp_frame_size(&self, key: u64, frame_size: i32) {
        if frame_size > 0 {
            self.sp_frame_sizes.lock().insert(key, frame_size);
        }
    }

    /// The frame size [`Self::note_sp_frame_size`] recorded for `key`.
    pub(crate) fn sp_frame_size(&self, key: u64) -> Option<i32> {
        self.sp_frame_sizes.lock().get(&key).copied()
    }
}

/// `CRATONVM_JIT_IR_SELFREC_FRAME_EVIDENCE` (JIT round 23 wave 4, lane L1;
/// perf, **default ON**, `=0` kill;
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// the optimizing acceptance gate takes a materially smaller frame as evidence
/// for a body with a DIRECT self-call ([`ir_selfrec_frame_evidence`]).
pub(crate) fn ir_selfrec_frame_evidence_enabled() -> bool {
    true
}

/// Is `ir_frame` (the optimizing body's `osr_frame_size`) evidence the
/// acceptance gate keeps it for: the body makes a direct self-call
/// (`has_direct_self_call`: an `invoke_kind == 4` row), and its frame is at
/// least one eighth smaller than the single-pass body's (`sp_frame`, the one
/// this VM published for the same method and bytecode). Compiled recursion
/// depth is the stack budget over the frame, so for such a body the frame IS
/// a transform the baseline lacks: `H4b.rec(I)I` measured 0x1E0 (480 B) on
/// the optimizing tier against 0x250 (592 B) single-pass, 8 665 levels against
/// 6 634 under the same 4 MiB budget, past the interpreter's 8 190. Scoped to
/// a direct self-call: no other body's verdict changes. `false` with no
/// recorded single-pass frame (no predecessor: nothing to be smaller than).
pub(crate) fn ir_selfrec_frame_evidence(
    has_direct_self_call: bool,
    ir_frame: i32,
    sp_frame: Option<i32>,
) -> bool {
    let Some(sp) = sp_frame else {
        return false;
    };
    has_direct_self_call
        && ir_frame > 0
        && sp > 0
        && i64::from(ir_frame) * 8 <= i64::from(sp) * 7
        && ir_selfrec_frame_evidence_enabled()
}



/// The registry of compiles with no VM in scope (backend tests, the positional
/// `try_compile`), and of the free verdict functions. See
/// [`JitVerdictRegistry`].
pub fn process_jit_verdicts() -> &'static JitVerdictRegistry {
    static PROCESS: OnceLock<JitVerdictRegistry> = OnceLock::new();
    PROCESS.get_or_init(JitVerdictRegistry::default)
}

/// A verdict registry's flush epoch (the "install" half of its stamps): a
/// counter of its own, or its VM's code-state epoch (`JitRealm::code_state_epoch`
/// in the `vm` crate, which that VM advances on every redefinition and
/// code-cache flush) shared with it.
///
/// Shared, one counter answers for the VM's tiered manager, its compile
/// verdicts ([`JitVerdictRegistry`]) and its runtime de-speculation verdicts
/// (`compile_gate::RuntimeDespecRegistry`); until interpreter round i1 wave 15
/// each registry kept its own copy, advanced beside the VM's in the same two
/// places. A shared counter is advanced only by its owner: the registries'
/// `note_*` calls then move only what they own (the redefine epoch, unless
/// that is shared too: [`RedefineEpoch`], interpreter round i1 wave 17).
pub enum FlushEpoch {
    /// The registry's own counter (no VM: backend tests, the process registry).
    Own(std::sync::atomic::AtomicU64),
    /// The owning VM's code-state epoch.
    Shared(std::sync::Arc<std::sync::atomic::AtomicU64>),
}

impl Default for FlushEpoch {
    fn default() -> Self {
        Self::Own(std::sync::atomic::AtomicU64::new(0))
    }
}

