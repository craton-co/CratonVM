#[allow(unused_imports)]
use super::*;

/// The full (slow) compile pipeline behind [`try_jit_compile_callee`].
/// Sets `*cache_negative = false` when a `None` return is for a reason that
/// may change soon (currently: receiver class not loaded yet), so the caller
/// does not negative-cache it.
///
/// ## GC-STW-safety / VM-lock discipline (wire-tiered-manager increment 3)
///
/// This function runs both on the mutator (inline JIT-dispatch helpers) AND,
/// when `CRATONVM_BG_COMPILE` is on, on the GC-neutral `cratonvm-jit-compiler`
/// worker via [`background_compile_task`]. The worker is an unregistered
/// `std::thread::Builder` daemon (the G1-MarkComplete precedent): the STW
/// barrier never waits for it, so concurrent relocation is harmless to it
/// PROVIDED it holds no VM lock across a blocking op. The fatal failure mode is
/// indirect: a mutator wanting `class_manager.write()` (class definition) that
/// blocks behind a read lock the worker is holding across a wait can no longer
/// reach its safepoint, so a STW initiated by a third thread (whose `expected`
/// count includes that blocked mutator) never completes.
///
/// Lock order and bounded scopes (each VM lock acquire -> read out what is
/// needed -> DROP -> proceed; never two held simultaneously, never one held
/// across a blocking call / nested VM-lock acquisition / managed allocation):
///   1. `class_manager.read()` (`cm`) — bytecode/method metadata extraction
///      only; explicitly `drop(cm)` BEFORE `jit::try_compile`. The constant-pool
///      resolver closures handed to `try_compile` re-acquire `class_manager`
///      read locks TRANSIENTLY, each scoped to a single CP lookup and dropped at
///      closure return. The one resolver that can BLOCK or take
///      `class_manager.write()` — `resolve_field_ref` -> `load_class_concurrent`
///      (per-class-name condvar wait, write-lock class load with <clinit>/GC) —
///      is invoked by `field_resolver`/`static_field_resolver` BEFORE those
///      closures take their own `cm` read, so no VM read lock is alive across
///      that blocking/allocating call.
///   2. `flight_recorder.lock()` — held only for the single
///      `emit_compilation_event_arc` call; description + timestamp built first.
///   3. `jit_cache.write()` — held only for the publishing `put`; the key Arc
///      is built first. This is the cross-thread publish: a mutator's
///      `jit_cache` fast-path flips the call site to `Jit` on its next call.
/// No two of {class_manager, flight_recorder, jit_cache} are ever held at once.
/// GC itself takes none of these during STW (it scans deposited root snapshots),
/// so the worker's transient holds only matter via the mutator-stall path above.
#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn try_jit_compile_callee_slow(
    shared: &SharedVm,
    // The receiver class's identity when the caller knows it (the background
    // task's `MethodKey::class_id`). Honoured only while it still names a
    // loaded class called `class_name`; otherwise, and for `None`, the class
    // is resolved by name, which answers nothing when two loaders define it.
    class_id_hint: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    // wire-tiered-manager Step 3: `false` compiles this method with the fast
    // single-pass C1 backend (no IR pipeline); `true` uses the optimizing C2
    // backend. Threaded into `jit::try_compile`'s trailing flag.
    optimize: bool,
    cache_negative: &mut bool,
    // Set to `true` when a `None` is a standing POLICY verdict about the loaded
    // method — a registered native runs instead of its bytecode, the FJP lever,
    // no `Code` attribute — rather than a codegen attempt that failed. Nothing
    // can lift such a verdict short of a redefinition (which clears the tiered
    // manager's `ineligible` too), so the background door reports it as a
    // decline instead of spending one of the method's compile retries
    // (`i1-L7-callee-compile-doors-are-loader-blind`, fix 3). Never cleared
    // here; the caller initialises it.
    policy_declined: &mut bool,
    allow_synchronized_wrapped_entry: bool,
    // JIT round 21 lane reflect4 (REFLECT4-21-1, `CRATONVM_JIT_BYTECODE_ONLY_KEY`):
    // compile the body of a reviewed `Intrinsic` for its own-body fallback
    // only (`interpreter::bytecode_only_body`). The method's OWN registered
    // native does not refuse it, and the body is published under the
    // door-invisible key `bytecode_only_body::bytecode_only_key_descriptor`
    // names, which no ordinary caller looks up. Its call sites keep every gate.
    bytecode_only: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    // Interpreter round i4 wave 91, lane L4: a compile run on a mutator inside
    // a finder census scope (`CRATONVM_DBG=mh-finder-census`; one flag read
    // otherwise, and a background worker has no scope).
    let _census_compile = cratonvm_native_builtins::mh_finder_census::work_clock(
        cratonvm_native_builtins::mh_finder_census::Work::JitCompile,
    );
    // Wave 93, lane L7: name the method compiled on the mutator in the scope
    // (`jit-callee-door.<scope> compile-on-mutator ...`); a no-op without the
    // census or outside a scope, so the worker's compiles are not named.
    cratonvm_native_builtins::mh_finder_census::note_jit_callee_door(
        "compile-on-mutator",
        class_name,
        method_name,
        descriptor,
    );
    // `eager-callee-chain`: a top-level compile starts with a fresh transitive
    // callee-compile budget; a nested one inherits the outer compile's.
    eager_callee_chain_enter_top_level();
    // JIT round 21 wave 1 (lane indy, INDY21-3): before the compile reads any
    // class's initialization state (`note_static_init_waiters`).
    let static_init_seq0 = static_init_log_seq(shared);
    // Round 15 wave 1 (lane calls): the acceptance verdict this function reads
    // below is its OWN compile's, not an enclosing compile's pending one. See
    // `NestedVerdictHandOff`.
    let _outer_verdict = NestedVerdictHandOff::take_for_this_compile();
    // The hint, when it still names a class called `class_name`. A stale or
    // zero id (a key built before its class was known) falls back to the name.
    let hinted_class_id = |cm: &crate::classloading::ClassManager| -> Option<ClassId> {
        class_id_hint.filter(|&id| {
            id.as_u32() != 0 && cm.get_class(id).is_some_and(|c| &*c.name == class_name)
        })
    };
    // Compile verdicts are kept per loaded class. The two refusals below fire
    // before this function resolves the receiver class, so they look its
    // identity up only when they fire, with no class-manager guard held. It
    // is the same resolution `callee_class_id` gets further down.
    let receiver_class_id_for_verdicts = || {
        let cm = shared.classes.class_manager.read();
        hinted_class_id(&cm)
            .or_else(|| cm.find_unique_class_by_name(class_name))
            .unwrap_or(ClassId::new(0))
    };
    // RFJP.1 (RETIRED, lever-only) — refuse a method whose declaring class
    // transitively extends `java/util/concurrent/ForkJoinTask`. Inert unless
    // `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`. This is the exit that reported
    // `reason=vm-fjp-subclass-blocklisted` for both `CompletableFuture$
    // UniCompose.tryFire` and `$UniRelay.tryFire`.
    if is_fjp_subclass_blocklisted(shared, class_name, None) {
        jit_verdicts(shared).record_compile_refusal(
            receiver_class_id_for_verdicts(),
            class_name,
            method_name,
            descriptor,
            "vm-fjp-subclass-blocklisted",
        );
        *policy_declined = true;
        return None;
    }
    // FJP fix (CORRECTED): refuse to compile a method only when the method that
    // would ACTUALLY RUN for this receiver is natively shadowed — compiling its
    // bytecode would bypass the native (e.g. `ForkJoinTask.fork()`'s Unsafe-CAS
    // body). Two such cases:
    //   (1) the receiver's OWN class has a native for (method, desc) — checked
    //       here, before resolution;
    //   (2) the method is INHERITED from a class that has the native (e.g. a
    //       `ForkJoinTask` subclass that does not override `fork()`) — checked at
    //       the resolved DECLARING class just after `find_method_recursive` below.
    //
    // The previous implementation walked the ENTIRE ancestor chain and refused
    // whenever ANY ancestor had a native of the same signature. That wrongly
    // refused every bytecode OVERRIDE of a method that is native on `Object`
    // (`hashCode`/`equals`/`toString`/`clone`): a real override shadows the
    // ancestor native, so `find_method_recursive` stops at the override and it
    // IS compilable. The bug made hashCode/equals-heavy code run interpreted —
    // e.g. ANTLR's `ParserATNSimulator` ATN simulation (PredictionContext/
    // ATNConfig/DFAState `hashCode` are ~58% of the Groovy-parse profile) never
    // compiled, ~100x slower than HotSpot (Spring Boot buildSrc
    // `SpringRepositoriesExtensionTests` hang).
    // Interpreter round i6 wave 117, lane L2
    // (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`): a shadowed method marked
    // re-entry-only compiles; its body publishes re-entry-only (`JitCache::put`),
    // the read-back below answers it, and the binding funnel
    // (`try_jit_compile_callee_hinted`) withholds it from every caller.
    if !bytecode_only
        && registered_native_will_run(shared, class_name, method_name, descriptor)
        && !shared
            .jit
            .jit_cache
            .is_reentry_only_key(class_name, method_name, descriptor)
    {
        jit_verdicts(shared).record_compile_refusal(
            receiver_class_id_for_verdicts(),
            class_name,
            method_name,
            descriptor,
            "vm-callee-is-registered-native",
        );
        *policy_declined = true;
        return None;
    }
    // Look up the method bytecode
    let cm = shared.classes.class_manager.read();
    let callee_class_id =
        match hinted_class_id(&cm).or_else(|| cm.find_unique_class_by_name(class_name)) {
            Some(id) => id,
            None => {
                // Two different "no answer"s, which need opposite handling
                // (`i1-L7-callee-compile-doors-are-loader-blind`, fix 2). A name
                // several loaders define resolves to nothing without a hint for
                // as long as they all stay loaded, so re-running this whole
                // pipeline on every dispatch miss buys nothing: that refusal is
                // named for what it is and left cacheable (the negative cache's
                // periodic re-probe still notices an unload). A class that is
                // simply not loaded yet may be on the next attempt, so that
                // `None` must not be cached.
                //
                // Neither is a `policy_declined`: an unload can end the
                // ambiguity and a load ends the absence, and the tiered
                // manager's `ineligible` would outlive both.
                let ambiguous = matches!(
                    cm.classify_loaded_name(class_name),
                    cratonvm_classloading::NameResolution::Ambiguous { .. }
                );
                if !ambiguous {
                    *cache_negative = false;
                }
                // No loaded class to key the reason by: `ClassId(0)`.
                jit_verdicts(shared).record_compile_refusal(
                    ClassId::new(0),
                    class_name,
                    method_name,
                    descriptor,
                    if ambiguous {
                        "vm-callee-class-name-ambiguous"
                    } else {
                        "vm-declaring-class-not-loaded"
                    },
                );
                return None;
            }
        };
    let store = cm.class_store();
    let (method, declaring_id) = crate::classloading::find_method_recursive(
        callee_class_id,
        method_name,
        descriptor,
        store,
    )?;
    // Interpreter round i1 wave 39 (lane L1, cross-lane): no body of a method
    // a debugger's breakpoint sits in, so no direct bind finds one
    // (`interpreter::breakpoint_bars_compiling`). Not cached and not a policy
    // verdict: the breakpoint goes.
    if super::breakpoint_bars_compiling(shared, declaring_id, method_name, descriptor) {
        *cache_negative = false;
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] compile REFUSED {class_name}.{method_name}{descriptor}: \
                 debugger-breakpoint"
            );
        }
        return None;
    }
    // JIT round 21 wave 11 (lane finalize): no body of a finalizable class's
    // constructor that can throw before its `super(..)`
    // (`exception_dispatch::finalizer_bars_compiling`). Negative-cached since
    // wave 13 (review rev-jit w13 item 3): the switches are read once per VM
    // and a refusal never turns back into an allowed answer (it flips only
    // from allowed to refused, at the define of a finalizable subclass, which
    // has no stale negative entry to clear).
    if super::finalizer_bars_compiling(shared, declaring_id, method_name, descriptor) {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] compile REFUSED {class_name}.{method_name}{descriptor}: \
                 finalizable-ctor-may-throw-before-init"
            );
        }
        return None;
    }
    // An inherited method's body is published under the RECEIVER's key below,
    // where a scoped redefinition of the declaring class, which matches bodies
    // by their key's class, cannot find it: mark the declaring class as copied
    // (it then takes the full flush), under the guard its bytecode is read
    // under (interpreter round i1 wave 20, lane L1;
    // `JitRealm::scoped_redefinition_admitted`).
    if declaring_id != callee_class_id {
        shared.jit.jit_cache.note_bytecode_copied(declaring_id);
    }
    // Interpreter round i1 wave 42, lane L2
    // (`interpreter-L2-serving-a-redefined-target-compiled-tears-a-multi-class-redefinition-FIXED-20261006`):
    // open the compile's epoch witness HERE, under the class-manager read
    // guard the bytecode below is copied under, not at `compile_gate::admit`
    // inside `try_compile_request`. The bytecode is copied into `cached` and
    // the guard dropped before `admit` runs, and a redefinition waiting for
    // the writer takes it exactly there: its fence (the install barrier, the
    // `RedefinedClass` record) and its eviction ran before `admit` stamped
    // the body, so a body compiled from the OLD bytecode carried an epoch
    // newer than both and `JitCache::put` published it AFTER the
    // redefinition, under the method's key -- stale code served until the
    // class's next redefinition. Stamped under the guard, the body predates
    // any redefinition that did not install before this read, and `put`
    // refuses it. `admit`'s own witness nests inside this one and keeps its
    // epochs (an older stamp can only refuse more). Opened after the
    // `note_bytecode_copied` mark above, which must keep marking the class
    // for the whole-cache path (with a compilation open it would only be
    // recorded on this body). Since wave 43 the witness is the
    // `BytecodeReadWitness` the copy below is paired with (`CompileInputs`).
    let bytecode_read_witness = BytecodeReadWitness::open();
    // JIT round 19 wave 2, lane eager2 (EAGER19-3): the late-bind generation
    // this compile started at ([`late_bind_take_pending_started`]).
    let late_bind_started = late_bind_compile_started(shared);
    // Direct dispatcher compilation also bypasses interpreter frame creation.
    if method.is_synchronized() && !allow_synchronized_wrapped_entry {
        jit_verdicts(shared).record_compile_refusal(
            callee_class_id,
            class_name,
            method_name,
            descriptor,
            "vm-synchronized-no-wrapped-entry",
        );
        return None;
    }

    let Some(code_attr) = method.code() else {
        // No body to compile (a `native` or `abstract` method): permanent for
        // the loaded bytecode.
        *policy_declined = true;
        return None;
    };
    // `cm` is intentionally passed through: do not recursively read-lock the
    // class manager while this guard and its borrowed method are alive.
    //
    // The class id here MUST be `declaring_id`, not `callee_class_id`. The scan
    // resolves the constant-pool indices embedded in `code_attr.code`, and
    // those indices only mean anything in the constant pool of the class that
    // DECLARES the method. `callee_class_id` is the RECEIVER's class, which for
    // any inherited method is a different class with a completely unrelated
    // pool: the same index there is a Utf8, a Fieldref, or out of range, so the
    // scan's `_ => return true` arm fired and the method was permanently
    // `mark_jit_bail_listed`ed for a constant it never referenced.
    //
    // The cost of that landed on exactly the code least able to absorb it —
    // framework hierarchies whose hot methods are inherited accessors. On
    // `InPredicateTest`'s 100k-element criteria IN-list, every hot SQM
    // accessor (`SqmTextValuedSimplePath.getReferencedPathSource`,
    // `BasicSqmPathSource.getExpressible`, ...) was bail-listed this way, so
    // `jit_invoke_virtual_mic` reported `hit_entry=0 / pub_probe_none=2228315`
    // — the inline cache NEVER held an entry, and 2.2M dispatches from compiled
    // code each took the entryless helper path instead of a direct call. That
    // made JIT-on ~2x SLOWER than `--nojit` on that test, which is what
    // docs/known-issues/hibernate/hib-inpredicate-*.md has been tracking.
    // `CRATONVM_COMPATIBLE_JDK_REIFIER` (interpreter round i2 wave 57, lane
    // L3): the generic-metadata natives are not registered, the JDK's bodies
    // run in both tiers, and there is nothing for the scan to protect.
    let scan_refuses = !shared.natives.native_methods.compatible_jdk_reifier()
        && jit_method_calls_forced_class_generic_metadata(
            &cm,
            declaring_id,
            &code_attr.code,
            code_attr.code.len(),
        );
    // DIAG (`CRATONVM_DBG=callee-probe`): name the set of methods the
    // `callee_class_id` -> `declaring_id` fix above newly ADMITS, i.e. every
    // method the buggy argument would have bail-listed and this one compiles.
    //
    // That set is the search space for a miscompile the fix exposes: the fix
    // itself is a trigger, not a cause, so the question "which method does the
    // backend get wrong" has to be answered over the methods whose status it
    // actually changed -- not over the whole program. Diffing the tally across
    // two builds cannot answer it (the dump is truncated to 30 rows, and the
    // two runs load different code), but evaluating BOTH arguments in the one
    // build that exhibits the failure names the delta exactly.
    if callee_probe_dbg() && !scan_refuses && declaring_id != callee_class_id {
        let old_arg_refuses = jit_method_calls_forced_class_generic_metadata(
            &cm,
            callee_class_id,
            &code_attr.code,
            code_attr.code.len(),
        );
        if old_arg_refuses {
            note_newly_admitted_callee(class_name, method_name, descriptor);
        }
    }
    if scan_refuses {
        jit_verdicts(shared).mark_bail_listed(callee_class_id, class_name, method_name, descriptor);
        jit_verdicts(shared).record_compile_refusal(
            callee_class_id,
            class_name,
            method_name,
            descriptor,
            "vm-bytecode-scan-refused",
        );
        return None;
    }
    // jit-invokestatic-clinit-gap fix (2026-07-17): third occurrence of the
    // same gap as the `callee_compiler` (compile-time direct_calls) and
    // `resolve_inline_site` (inlining) closures above -- this is the
    // RUNTIME-side counterpart, invoked from `jit_invoke_dispatch`
    // (`vm/src/jit/helpers.rs`) via `try_compile_callee` the first time a
    // generic-dispatch invokestatic call site actually executes. On success
    // this function's `(entry_ptr, needs_context)` gets cached into
    // `jit_cache` and, per `jit_invoke_dispatch`'s own comment two call
    // sites down, invoked via `invoke_or_native`/a direct call -- NOT
    // through `invoke_shared`'s `ensure_class_initialized_shared` call.
    // Same JVMS SS5.5 requirement, same fix: refuse to hand back a compiled
    // entry for a `static` method whose declaring class isn't initialized
    // yet, forcing this call (this one time) through the always-safe
    // `invoke_or_native` fallback that jit_invoke_dispatch uses when this
    // function returns `None`. Monotonic init state makes a compile-time
    // (well, first-dispatch-time) check here sound for the cached entry's
    // entire remaining lifetime, exactly like the other two sites.
    if method.is_static() {
        let declaring_class_initialized = store
            .get(declaring_id)
            .map(crate::vm::is_class_initialized_fast)
            .unwrap_or(false);
        if !declaring_class_initialized {
            // Not yet initialized: don't cache a negative result either --
            // the class may initialize very soon (e.g. the very next
            // dispatch through the safe fallback), at which point this
            // function should succeed and start caching the fast entry.
            *cache_negative = false;
            jit_verdicts(shared).record_compile_refusal(
                callee_class_id,
                class_name,
                method_name,
                descriptor,
                "vm-declaring-class-not-initialized",
            );
            return None;
        }
    }
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name)?;
    // FJP fix (CORRECTED) case (2): the resolved method is INHERITED from a
    // class that has a Rust native override (e.g. `ForkJoinTask.fork()` reached
    // through a subclass that does not override it). Compiling its bytecode
    // would bypass the native, so refuse. Checking the DECLARING class (the
    // resolution point) — rather than every ancestor — is precisely what lets
    // bytecode OVERRIDES on the receiver/intermediate classes still compile.
    // (When `declaring_class_name == class_name` the own-class check above
    // already returned None, so this only fires for genuinely inherited natives.)
    if declaring_class_name != class_name
        && inherited_native_runs(
            shared,
            declaring_id,
            declaring_class_name,
            method_name,
            descriptor,
        )
        // `--real-jdk`: a Bridge above a retired row is masked at every
        // dispatch walk, so the bytecode is what runs (round 14 wave 2, lane
        // shadow; `CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES`).
        && !crate::runtime::interpreter::retired_row_masks_ancestor_bridge(
            shared,
            &cm,
            (callee_class_id, None),
            declaring_id,
            method_name,
            descriptor,
            shared
                .natives
                .native_methods
                .kind_of(declaring_class_name, method_name, descriptor)
                .unwrap_or(cratonvm_native_api::NativeKind::Bridge),
        )
        // JIT round 21 (lane refs2, `CRATONVM_JIT_GATES_YIELDED_BRIDGE`): a
        // `Bridge` over the declaring class's own `Code` loses at every
        // `--real-jdk` dispatch door (`Reference.refersTo` for a
        // `SoftReference` receiver), so compiling it bypasses nothing.
        && !crate::runtime::interpreter::bridge_yields_to_loaded_bytecode_for_gates(
            shared,
            Some(&*cm),
            declaring_class_name,
            method_name,
            descriptor,
            shared
                .natives
                .native_methods
                .kind_of(declaring_class_name, method_name, descriptor)
                .unwrap_or(cratonvm_native_api::NativeKind::Bridge),
        )
    {
        jit_verdicts(shared).record_compile_refusal(
            callee_class_id,
            class_name,
            method_name,
            descriptor,
            "vm-native-override-present",
        );
        *policy_declined = true;
        return None;
    }
    let source_file = store
        .get(declaring_id)
        .and_then(|c| c.source_file.as_deref())
        .map(Arc::from);
    let num_params = count_method_params(descriptor);
    let padded_code = crate::runtime::frame::padded_bytecode(&code_attr.code);

    let inputs = bytecode_read_witness.inputs(Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring_id,
            class_name: Arc::from(declaring_class_name),
            method_name: Arc::from(method_name),
            method_descriptor: Arc::from(descriptor),
            source_file,
            code: padded_code,
            exception_table: Arc::from(code_attr.exception_table.as_slice()),
            max_stack: code_attr.max_stack,
            max_locals: code_attr.max_locals,
            num_params: num_params as u16, // Widening: parameter count conversion
            is_synchronized: method.is_synchronized(),
            is_static: method.is_static(),
        },
    )));
    drop(cm);
    let cached: &Arc<CachedBytecodeMethod> = inputs.cached();

    // S-HIB.1 — apply the static skip list to callee compilations triggered
    // from JIT helper callbacks (jit_invoke_virtual_mic et al.).  Without this
    // check any method — including complex `<init>`/`<clinit>` bodies with
    // putfield/invokedynamic that the first-call and OSR paths correctly ban —
    // could be compiled via this path, leading to JIT codegen bugs like the
    // `JoinedList.<init>` checkcast-on-primitive crash.
    {
        // The static JIT ban list was deleted 2026-07-31 (see
        // docs/feature-designs/native-dispatch-architecture.md).
        // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
        // remaining force-interpret lever, applied in `jit::try_compile`.
        // GPU-offload JIT admission gate — see offload_jit_gate.
        #[cfg(feature = "gpu-offload")]
        if crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
            shared,
            cached.declaring_class_id,
            method_name,
            &cached.method_descriptor,
        ) {
            jit_verdicts(shared).record_compile_refusal(
                callee_class_id,
                class_name,
                method_name,
                descriptor,
                "vm-callee-compile-gate-refused",
            );
            return None;
        }
    }

    // Build resolvers for the callee's constant pool
    let cid = declaring_id;
    // The constant-pool resolvers (see `CpResolvers`).
    let cp = CpResolvers {
        shared,
        class_id: cid,
        substituted_owners: SubstitutedOwners::default(),
        method_name: &cached.method_name,
        method_descriptor: &cached.method_descriptor,
        code: &cached.code,
        ldc2w_trace: Some("full"),
        invoke_operands: Default::default(),
        first_calls_seen: Default::default(),
        deferred_fields_seen: Default::default(),
    };
    // The `cp_*` resolvers themselves come from `cp.with_compile_request` at
    // the request below (interpreter round i1 wave 5, L7).
    // PGO-02: receiver class-id -> class-name resolver for a guarded
    // speculative virtual/interface inline plan's SpeculatedReceiver
    // invalidation dependency (plan_inline's fail-closed rule — see
    // docs/feature-designs/profile-guided-inlining.md). `None` (id not
    // loaded, or unloaded between profiling and compiling) refuses
    // that one speculation rather than recording an unmatchable
    // name-less dependency.
    let class_id_namer = |namer_cid: u32| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cratonvm_types::ClassId::new(namer_cid))
            .map(|c| c.name.to_string())
    };
    // PGO-02 R0: see the sibling resolver in `try_jit_compile` — the body a
    // receiver of exactly this class id dispatches to, which is the only body a
    // guard admitting that class may splice.
    let receiver_inline_resolver = |rcv_cid: u32, cp_class: &str, name: &str, desc: &str| {
        resolve_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            rcv_cid,
            cp_class,
            name,
            desc,
            // No direct-bind resolver on the guarded-virtual path: the
            // closure that owns it is declared further down this function, and
            // a spliced body reached through a receiver guard is planned before
            // it exists. The consequence is a REFUSAL, never a downgrade — a
            // call-carrying body with nothing to bind to is not admitted at all
            // (see the admission rule in `resolve_inline_site_from`).
            None,
        )
    };
    // CHA (NOTES-deopt2.md M5) — see the sibling resolver in `try_jit_compile`.
    let unique_concrete_resolver = |cp_class: &str, name: &str, desc: &str| {
        resolve_unique_concrete_bind(shared, cached.declaring_class_id, cp_class, name, desc)
    };
    // Elidable-`<init>` resolver for `new` scalar replacement, default-ON
    // (opt-out: CRATONVM_JIT_SCALAR_NEW=0).
    let scalar_new_on = true;

    let pgo_profile = {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: cached.declaring_class_id.as_u32(),
            method_name: cached.method_name.clone(),
            descriptor: cached.method_descriptor.clone(),
        };
        shared.jit.profile_store.get_profile(&profile_key)
    };
    let helpers = crate::jit::helpers::build_helpers_for(shared);

    // Resolve java/lang/String's field layout for the JIT String call-site
    // intrinsics (see `resolve_string_field_layout`).
    let string_layout_resolver = || resolve_string_field_layout(shared);

    // Direct JIT-to-JIT calls, LOOKUP-ONLY (tomcat doc 04, 2026-07-27).
    //
    // This argument used to be `None` ("no recursive callee compilation"),
    // which is what the tiered BACKGROUND worker compiles every hot method
    // with. `None` does not merely decline to compile a callee eagerly — it
    // means `jit/src/lib.rs` plans NO `direct_calls` at all, so every
    // `invokestatic`/`invokespecial` in a background-compiled body falls
    // through to the generic `jit_invoke_dispatch` helper round trip, on
    // every call, forever. Measured on `apps/tomcat-suite-runner/probes/
    // CallCostProbe.java` (marginal cost of one extra invoke inside a
    // compiled loop, same binary, same run):
    //
    //     invokestatic, background-compiled body  ~196 ns
    //     invokestatic, mutator-compiled body     ~5-9 ns   (CRATONVM_BG_COMPILE=0)
    //     invokestatic, HotSpot                   ~0.5 ns
    //
    // i.e. ~30x on the single most common call form in ordinary Java, on the
    // path that compiles almost everything. Deploy-heavy Tomcat classes are
    // exactly this shape (reflection, class loading, Digester), which is the
    // residual "interpreter throughput x method count" wall doc 04 describes.
    //
    // The stated reason for `None` was recursion: `try_jit_upgrade_with_gate`'s
    // `callee_compiler` will COMPILE an unseen callee inline, and doing that
    // from the compile worker would nest compiles. This resolver keeps that
    // property by never compiling anything — it answers only from
    // `jit_cache`, so a callee that is already compiled (the overwhelmingly
    // common case once a workload is warm; the tiered manager compiles leaves
    // before their callers because leaves reach the invocation threshold
    // first) gets a raw `CALL`, and anything else stays on dispatch exactly
    // as before.
    //
    // Every refusal gate of the mutator-side `callee_compiler` is mirrored
    // here, in the same order; each `None` is correctness-safe because it
    // just leaves the site on the checked dispatch helper:
    //   * FJP subclass blocklist (RFJP.1)
    //   * Rust native shadow (S111r15)
    //   * BUG-H: callee declaring its own exception table — a raw CALL would
    //     let an implicit AIOOBE/NPE escape the callee's own `catch`
    //   * `synchronized` callee (no monitor pairing across a raw CALL)
    //   * JVMS 5.5: a `static` callee whose declaring class is not yet
    //     initialized (the direct CALL bypasses every init check)
    //   * an artifact carrying an unconditional invokedynamic trap
    // FIX (jdk-only-string-indexof-int-int-miscompiles-in-the-single-pass-tier):
    // every callee address this compile binds stays pinned until the compile
    // publishes. See the wrapper below for why the bind, and not publication,
    // is where the keep-alive has to start.
    let bind_pins: std::cell::RefCell<Vec<std::sync::Arc<cratonvm_jit::CompiledMethod>>> =
        std::cell::RefCell::new(Vec::new());
    // `holder` is the class whose constant pool names `callee_class` (see
    // `InlineDirectBind`, i11-L0): this method's class, or a spliced body's.
    let direct_callee_lookup_bound = |holder: ClassId,
                                      callee_class: &str,
                                      callee_method: &str,
                                      callee_desc: &str|
     -> Option<(usize, bool)> {
        // Each arm names its refusal TWICE on purpose: the string is for the
        // per-site `dbg_jitc` trace, the `DirectBindRefusal` variant is for the
        // process-wide tally the `intrinsic-stats` census prints. A bare
        // `bind_misses` total cannot distinguish a compile-ORDER accident
        // (re-bindable) from a standing policy refusal (not), which is the
        // choice the two-causes page leaves open.
        macro_rules! dc_no {
                ($why:expr, $reason:expr) => {{
                    let reason: cratonvm_jit::DirectBindRefusal = $reason;
                    cratonvm_jit::note_direct_callee_bind_refusal(reason);
                    // JIT round 19 wave 3, lane eager3: the eager chain's own
                    // rules, for `CRATONVM_DBG_EAGER_CHAIN_CENSUS`.
                    eager_chain_census_note_direct_refusal(shared, reason);
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] bg-direct-call DECLINED {callee_class}.{callee_method}{callee_desc}: {}",
                            $why
                        );
                    }
                    // JIT round 16 wave 14 (lane throw4, TP16-20): a refusal
                    // that only says "not yet" remembers this method, so the
                    // site's first compiled-callee dispatch can ask for a
                    // recompile that binds it (`late_bind_site_served`).
                    note_late_bind_waiter(shared, reason, callee_class, callee_method, callee_desc, cached);
                    return None;
                }};
            }
        if let Some(refusal) =
            direct_bind_name_refusal(shared, holder, callee_class, callee_method, callee_desc)
        {
            dc_no!(direct_bind_refusal_name(refusal), refusal);
        }
        // The site's class id, and the class that DECLARES the resolved method
        // (by id and name). The compile stack names a method by its declaring
        // class (`JitCompileMethodKey::from_cached`), and so does every body
        // published by the method's own tiering, so the cycle guard below
        // asks by it and the cache probe tries it first. By the site's name
        // alone, `Sub.f()` resolving to an inherited `A.f` missed a published
        // `A.f`, did not see `A.f` open on the compile stack, and compiled
        // `A.f` again, nested, up to `MAX_EAGER_CALLEE_CHAIN_DEPTH` times per
        // top-level compile (interpreter round i1 wave 13, L3).
        let (
            callee_class_id,
            declaring_class_id,
            declaring_class_arc,
            self_locking_only,
            callee_is_static,
        ) = {
            let cm = shared.classes.class_manager.read();
            // A substituted owner by its recorded id (i12-L2).
            let Some(bind_class) = direct_bind_callee_class(
                shared,
                &cm,
                &cp.substituted_owners,
                holder,
                callee_class,
                callee_method,
                callee_desc,
            ) else {
                dc_no!(
                    "callee-class-not-found",
                    cratonvm_jit::DirectBindRefusal::CalleeClassNotFound
                );
            };
            let callee_cid = bind_class.id;
            let store = cm.class_store();
            let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
                callee_cid,
                callee_method,
                callee_desc,
                store,
            ) else {
                dc_no!(
                    "callee-method-not-found",
                    cratonvm_jit::DirectBindRefusal::CalleeMethodNotFound
                );
            };
            // A member of a class only a substitution names, whose access
            // depends on that class (`DirectBindClass::admits`): the caller
            // could not name the class itself, so this is the refusal it had.
            if !bind_class.admits(method) {
                dc_no!(
                    "substituted-owner-member-access",
                    cratonvm_jit::DirectBindRefusal::CalleeClassNotFound
                );
            }
            // A callee with no `Code` attribute at all is always refused —
            // there is no body to bake a CALL to. A callee that merely DECLARES
            // a table is refused only while `direct_call_exc_table_publish_enabled`
            // is off; see that function for why the stated reason has expired
            // and what it is interlocked against.
            let declaring_class_initialized = store
                .get(declaring_id)
                .map(crate::vm::is_class_initialized_fast)
                .unwrap_or(false);
            // Round 13 wave 6 (lane sync3): a synchronized instance callee
            // may still bind its published SELF-LOCKING body, and nothing
            // else (`self_locking_body_bind_candidate`; round 13 wave 10, lane
            // sync6: a static one too, once its class is initialized).
            let self_locking_only =
                match direct_bind_method_refusal(method, declaring_class_initialized) {
                    Some(cratonvm_jit::DirectBindRefusal::Synchronized)
                        if self_locking_body_bind_candidate(
                            method,
                            declaring_class_initialized,
                        ) =>
                    {
                        true
                    }
                    Some(refusal) => dc_no!(direct_bind_refusal_name(refusal), refusal),
                    None => false,
                };
            // The declaring class is in the store (`find_method_recursive`
            // just walked to it); the site's own class is the fallback.
            let (declaring_cid, declaring_name) = match store.get(declaring_id) {
                Some(declaring) => (declaring_id, declaring.name.clone()),
                None => (callee_cid, Arc::from(callee_class)),
            };
            (
                callee_cid,
                declaring_cid,
                declaring_name,
                self_locking_only,
                method.is_static(),
            )
        };
        // A body of the resolved method may be published under either key:
        // its declaring class (its own tiering, the optimizing callee door) or
        // the site's class (`try_jit_compile_callee_slow` keys by the name it
        // was asked for, its recompile-storm fix). Probe both, declaring
        // first, under one read lock that is RELEASED before the transitive
        // compile below, which takes the same cache for writing.
        let probe_published = || {
            let jit_cache = shared.jit.jit_cache.read();
            let by_declaring = jit_cache.get(
                &declaring_class_arc,
                callee_method,
                callee_desc,
                declaring_class_id,
            );
            if by_declaring.is_some() || declaring_class_id == callee_class_id {
                return by_declaring;
            }
            jit_cache.get(callee_class, callee_method, callee_desc, callee_class_id)
        };
        let mut cached_body = probe_published();
        // JIT round 19 wave 4, lane syncdoor4 (SYNCDOOR4-1): a synchronized
        // instance callee with NO body yet compiles its self-locking body in
        // place under `CRATONVM_JIT_SELF_LOCK_EAGER_BIND`; a self-locking
        // answer binds below like any published one.
        if self_locking_only && cached_body.is_none() && self_lock_eager_bind_enabled() {
            cached_body = compile_unpublished_self_locking_callee(
                shared,
                "bg",
                holder,
                callee_class,
                declaring_class_id,
                &declaring_class_arc,
                callee_method,
                callee_desc,
                callee_is_static,
            );
        }
        // Round 13 wave 6 (lane sync3): a synchronized callee binds only a
        // published self-locking body; it is never compiled from here
        // (except under SYNCDOOR4-1, just above).
        if self_locking_only && !cached_body.as_deref().is_some_and(is_self_locking_body) {
            // JIT round 19, lane eager19 (TP16-21): the callee is a self-locking
            // candidate whose body is not published YET; remember this method
            // so that publication asks the late-bind recompile
            // ([`late_bind_self_lock_published`]).
            note_self_lock_late_bind_waiter(
                shared,
                &declaring_class_arc,
                callee_method,
                callee_desc,
                cached,
            );
            dc_no!(
                direct_bind_refusal_name(cratonvm_jit::DirectBindRefusal::Synchronized),
                cratonvm_jit::DirectBindRefusal::Synchronized
            );
        }
        // TRANSITIVE EAGER CALLEE COMPILE (`eager-callee-chain`).
        //
        // This resolver used to stop here with `callee-not-yet-compiled`, which
        // made eager callee binding exactly ONE level deep: the mutator door's
        // `callee_compiler` compiled a direct callee, but that callee was itself
        // compiled through this function, whose resolver bound ITS statically bound
        // sites to the generic `jit_invoke_dispatch` helper whenever they were not
        // compiled yet — and a compiled body never re-binds. So the whole chain's
        // speed depended on the ORDER the tiered manager happened to reach the
        // methods in, permanently.
        //
        // Measured on `probes/org/junit/jupiter/api/CompileOrderProbe.java` (JUnit's
        // `assertEquals` chain under a hot loop, real-JDK mode, G1): 478 ns/iter
        // compiling top-down against 73 bottom-up, with `CRATONVM_DBG_MIC_PROF=1`
        // reporting `disp_calls` 2 003 538 against 3 926 — one helper round trip per
        // iteration, ~150 ns of it, at ONE site (`CRATONVM_DBG_MIC_TRACE=1` named it:
        // `AssertEquals.assertEquals(Object,Object,String)`, reached from the
        // two-argument overload that had been compiled first).
        //
        // Compiling the callee here makes the bind order-independent: whichever
        // method the tiered manager reaches first, its statically bound callees are
        // compiled before its body is emitted, so the site binds directly.
        //
        // Bounded three ways, because this recursion is unbounded in principle:
        //  * DEPTH — `jit_active_compile_depth`; past the bound the site simply
        //    keeps dispatch, which is always correct;
        //  * CYCLES — `jit_active_compile_contains` declines a callee already open
        //    on this thread's compile stack (the jit crate's own
        //    `note_recursive_compile_cycle` answers the same question for its
        //    `callee_compiler`);
        //  * FAN-OUT — a per-top-level-compile budget, so a method with many call
        //    sites cannot turn one compile into a whole-program one.
        //
        // Every other refusal is unchanged: this arm is reached only when the callee
        // passed all of the gates above and the ONLY thing missing was a compiled
        // body.
        let compiled = match cached_body {
            Some(compiled) => compiled,
            None => {
                if !crate::runtime::env_cache::jit_eager_callee_chain() {
                    dc_no!(
                        "callee-not-yet-compiled",
                        cratonvm_jit::DirectBindRefusal::CalleeNotYetCompiled
                    );
                }
                if cratonvm_jit::jit_active_compile_depth() > MAX_EAGER_CALLEE_CHAIN_DEPTH {
                    dc_no!(
                        "eager-callee-chain-depth",
                        cratonvm_jit::DirectBindRefusal::EagerChainDepth
                    );
                }
                // By the declaring class: the compile stack is keyed by the
                // method being compiled, never by a caller's name for it.
                if cratonvm_jit::jit_active_compile_contains(
                    &declaring_class_arc,
                    callee_method,
                    callee_desc,
                ) {
                    dc_no!(
                        "eager-callee-chain-cycle",
                        cratonvm_jit::DirectBindRefusal::EagerChainCycle
                    );
                }
                // A callee nobody has called much is not worth a compile on its
                // caller's behalf: the site keeps the dispatch helper, and a
                // caller that stays hot is rebound by its own re-tier to C2,
                // which asks this again with the callee's count by then. Not a
                // late-bind waiter (`late_bind_refusal_may_lift`): every cold
                // callee of every compiled method would be one: thousands of
                // recompiles per process. One loaded Spring run with that row
                // saw a wrong `DoubleToDecimal.toDecimal`, which was never
                // reproduced again (retired page
                // `late-bind-recompile-miscompiles-todecimal-under-load-RETIRED-20261007.md`).
                // Asked before the budget is spent, so a cold callee does not
                // use up a hot one's share.
                if !eager_callee_chain_is_warm(
                    shared,
                    declaring_class_id,
                    callee_method,
                    callee_desc,
                ) {
                    // CF-W8-2 (`CRATONVM_JIT_LATE_BIND_COLD_CALLEE`): an
                    // optimizing body waits on the cold callee, once.
                    note_cold_callee_late_bind_waiter(
                        shared,
                        callee_class,
                        callee_method,
                        callee_desc,
                        cached,
                        optimize,
                    );
                    dc_no!(
                        "eager-callee-chain-cold",
                        cratonvm_jit::DirectBindRefusal::EagerChainCold
                    );
                }
                if !eager_callee_chain_try_spend() {
                    dc_no!(
                        "eager-callee-chain-budget",
                        cratonvm_jit::DirectBindRefusal::EagerChainBudget
                    );
                }
                // The cache probe above was loader-faithful; the compile below
                // is by name. See `jit_callee_name_is_another_loaders_copy`.
                if crate::jit::helpers::jit_callee_name_is_another_loaders_copy(
                    shared,
                    holder,
                    callee_class,
                ) {
                    dc_no!(
                        "another-loaders-copy",
                        cratonvm_jit::DirectBindRefusal::AnotherLoadersCopy
                    );
                }
                // `optimize` is this compile's own backend selection, so a C1 body's
                // callees are compiled at C1 and a C2 body's at C2 — the callee never
                // arrives at a tier its caller was not compiled against.
                //
                // Compiled as `callee_class_id` — the loader-faithful answer the
                // probe above used — so the body compiled is the one this site
                // binds and is published under a key that probe reads. By name,
                // a callee class name two loaders define was refused as ambiguous
                // even when this site's loader agrees with the flat map.
                // JIT round 19, lane eager19 (EAGERCOLD18-2): a C2 caller's
                // thin-profile callee is compiled at C1 under the opt-in
                // (`eager_callee_compile_optimize`); otherwise the caller's tier.
                let callee_optimize = eager_callee_compile_optimize(
                    shared,
                    declaring_class_id,
                    callee_method,
                    callee_desc,
                    optimize,
                );
                // JIT round 19 wave 2, lane eager2 (EAGER19-2): an inherited
                // callee compiled and reported under its DECLARING class's key
                // ([`eager_callee_compile_key`]); the probe above reads that
                // key first.
                let (compile_class_id, compile_class) = eager_callee_compile_key(
                    callee_class_id,
                    callee_class,
                    declaring_class_id,
                    &declaring_class_arc,
                );
                match try_jit_compile_callee_for_class(
                    shared,
                    compile_class_id,
                    compile_class,
                    callee_method,
                    callee_desc,
                    callee_optimize,
                ) {
                    Some((body, _entry, _needs_ctx)) => {
                        // Round 11 wave 18, P1: `body` is what is published
                        // under this key (read back after the `put`, or the
                        // cache hit), and no tiered task produced it.
                        note_callee_publish_to_tiered_manager(
                            shared,
                            compile_class_id,
                            compile_class,
                            callee_method,
                            callee_desc,
                            &body,
                        );
                        eager_chain_census_note_compiled(
                            shared,
                            EAGER_DOOR_ORDINARY,
                            declaring_class_id,
                            callee_method,
                            callee_desc,
                            Some(&body),
                        );
                        body
                    }
                    None => {
                        eager_chain_census_note_compiled(
                            shared,
                            EAGER_DOOR_ORDINARY,
                            declaring_class_id,
                            callee_method,
                            callee_desc,
                            None,
                        );
                        // A refused supersede answers `None` BECAUSE a baseline
                        // body was published while it ran (`refused_supersede`
                        // in `try_jit_compile_callee_slow`). Bind to that body
                        // rather than leave this site on the blind dispatch
                        // helper for the life of the caller's artifact
                        // (`docs/feature-designs/deopt-osr.md`).
                        let raced = probe_published();
                        match raced {
                            Some(body) => body,
                            None => dc_no!(
                                "eager-callee-chain-compile-declined",
                                cratonvm_jit::DirectBindRefusal::EagerChainCompileDeclined
                            ),
                        }
                    }
                }
            }
        };
        if compiled.has_indy_trap {
            dc_no!("indy-trap", cratonvm_jit::DirectBindRefusal::IndyTrap);
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] bg-direct-call BOUND {callee_class}.{callee_method}{callee_desc}"
            );
        }
        // Cast: object/code pointer to integer address
        Some((compiled.entry_ptr() as usize, compiled.needs_context()))
    };
    // The pinning wrapper. `direct_callee_lookup_bound` decides WHETHER to bind and to what;
    // this holds the answer alive. A raw entry address is only as meaningful
    // as the artifact that owns it, and between this bind and
    // `prepare_for_publication` there is a whole compilation during which that
    // artifact can be superseded, retired, freed and its buffer re-handed to
    // an unrelated method. `pin_jit_entry` upgrades the owner's `Weak` here,
    // while it is still the callee, so the address cannot change meaning
    // underneath the code being emitted against it.
    let direct_callee_lookup_in = |holder: ClassId,
                                   callee_class: &str,
                                   callee_method: &str,
                                   callee_desc: &str|
     -> Option<(usize, bool)> {
        let bound = direct_callee_lookup_bound(holder, callee_class, callee_method, callee_desc);
        if let Some((entry, _)) = bound {
            let Some(owner) = cratonvm_jit::pin_jit_entry(entry) else {
                // Do not return a raw callee address if its ownership raced
                // retirement between lookup and publication.
                return None;
            };
            bind_pins.borrow_mut().push(owner);
        }
        bound
    };
    // What the jit crate asks for this method's OWN call sites, whose names
    // come from this class's constant pool.
    let direct_callee_lookup =
        |callee_class: &str, callee_method: &str, callee_desc: &str| -> Option<(usize, bool)> {
            direct_callee_lookup_in(cid, callee_class, callee_method, callee_desc)
        };
    // Round 11 wave 13 (lane `spsync`, page
    // `r11w12-calls-single-pass-sync-direct-patch`): the caller-held
    // synchronized direct CALL's lookup, as the optimizing door asks it
    // (`sync_direct_target`). Until now only that door passed one, so a method
    // this door compiled single-pass kept every `static synchronized` callee on
    // `jit_invoke_dispatch`. Pinned with the other bound callees: the entry must
    // stay mapped until this compile publishes.
    let sync_direct_lookup =
        |cp_idx: u16| -> Option<cratonvm_jit::compile_request::SyncDirectTarget> {
            // Round 14 wave 3 (lane sync, SS-2): see the optimizing door's twin.
            let Some((target, pin)) = sync_direct_target(shared, cid, &cp, cp_idx) else {
                return sync_direct_monitor_answer(shared, cid, &cp, cp_idx);
            };
            bind_pins.borrow_mut().push(pin);
            Some(target)
        };

    let compile_start = std::time::Instant::now();
    let self_call_identity = self_call_identity_stable(shared, cached.declaring_class_id);
    // JDK-ONLY-WAVE2 §4. Answers "would the generic tail admit this
    // triple's registered native?" — which since 2026-09-22 is a wider
    // question than "is it a reviewed `NativeKind::Intrinsic`?", because a
    // refused bind never stopped the call: the site keeps its `invoke_info`
    // and `vm_exec::invoke_or_native` ran the same `Bridge` anyway. Still
    // fail-closed for a `SyntheticStub`, for anything unregistered and for
    // an armed `CRATONVM_ENFORCE_NATIVE_SHADOW`. The one predicate is
    // `helpers::direct_native_bind_admitted`, shared with the callee-side
    // gate; this closure used to be a third hand-written copy of a question
    // that had already been hoisted there.
    // Cheap: only the strict arm of `direct_native_helper` calls it, and only
    // for a triple whose helper cell is already non-zero.
    let direct_bind_admission = |class: &str, method: &str, descriptor: &str| -> bool {
        crate::jit::helpers::direct_native_bind_admitted(shared, class, method, descriptor)
    };

    // Build inline resolver for method inlining (Session 31).
    //
    // Declared HERE rather than beside the other resolvers above because it
    // borrows `direct_callee_lookup`: the calls inside a body about to be
    // spliced get the same lookup-only direct binding this method's own call
    // sites get, and a spliced call that falls to the blind dispatch helper
    // instead is a measured 3.5x loss (see `jit_inline_call_dispatch`).
    let inline_resolver = |callee_class: &str,
                           callee_method: &str,
                           callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            // The spliced body's class is the holder (`InlineDirectBind`).
            Some(&direct_callee_lookup_in),
            // A substituted owner by its recorded id (i13-L2).
            Some(&cp.substituted_owners),
        )
    };

    // The optimizing tier's own inline resolver. Same three-name question, a
    // different admission set — see `resolve_ir_inline_site`.
    //
    // This passed NO direct-bind resolver until 2026-09-09, on the stated
    // grounds that "`IrBuilder` has no direct-call lowering inside a relocated
    // body to bake an entry into". That was true and is no longer: the
    // `ir_direct_calls` map is keyed by COMBINED-BUFFER pc, `ir_lower`'s
    // `Op::Call` arm looks a spliced pc up in it like any other, and
    // `append_ir_inline_site` now produces the rows. Without the binder here
    // those rows are all empty, so every statically-bound call a splice left
    // behind kept resolving its callee BY NAME on every execution — the
    // measured 3.5x loss the single-pass sibling's comment above describes,
    // paid on the tier that is supposed to be the fast one.
    //
    // The same lookup-only closure the single-pass sibling uses, and for the
    // same reason: it binds an ALREADY-compiled callee and never compiles one,
    // so planning cannot recurse into compilation here.
    let ir_inline_resolver = |callee_class: &str,
                              callee_method: &str,
                              callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_ir_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&direct_callee_lookup_in as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };
    // Round 12 wave 5 (lane iropt4, proposal W4-2): the exact-receiver twin,
    // with the same lookup-only direct-bind resolver.
    let ir_exact_receiver_resolver = |callee_class: &str,
                                      callee_method: &str,
                                      callee_desc: &str|
     -> Option<(cratonvm_jit::InlineSite, u32)> {
        resolve_ir_exact_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&direct_callee_lookup_in as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };
    // Round 13 wave 12 (lane guardsplice, W7-1): the profile-selected twin,
    // with the same lookup-only direct-bind resolver.
    let ir_receiver_inline_resolver = |receiver_class_id: u32,
                                       callee_class: &str,
                                       callee_method: &str,
                                       callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_ir_profile_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            receiver_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&direct_callee_lookup_in as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };

    // Round 9 wave 7 (`mutrec7`): see `cycle_edge_target_admitted`.
    let class_is_final_resolver = |cid: u32| jit_class_is_final_for_typecheck(shared, cid);
    // JIT r20 wave 11 (lane pIR): see `jit_class_is_subclass_for_catch`.
    let class_is_subclass_resolver =
        |thrown: u32, catch_id: u32| jit_class_is_subclass_for_catch(shared, thrown, catch_id);
    let cycle_edge_admission =
        |caller_class_id: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            cycle_edge_target_admitted(
                shared,
                ClassId::new(caller_class_id),
                callee_class,
                callee_method,
                callee_desc,
            )
        };
    // Every `cp_*` resolver (and `cached` / `helpers`) comes from `cp_request`
    // (`CpResolvers::with_compile_request`); the fields below are this door's own.
    // JIT round 19 wave 2, lane eager2 (EAGER19-1): this method-entry door's
    // uninitialized-`new` trap admission ([`uninit_new_trap_admitted`]).
    let uninit_new_trap =
        |cp_idx: u16, pc: usize| uninit_new_trap_admitted(shared, cached, cp_idx, pc);
    // JIT round 19 wave 3, lane eager3 (EAGER2-3): the class-initialized guard.
    let uninit_new_guard =
        |cp_idx: u16, pc: usize| uninit_new_guard_admitted(shared, cached, cp_idx, pc);
    // JIT round 21 lane indylate2 (`CRATONVM_JIT_INDY_ADMIT_BY_ID`): the
    // direct binder and the optimizing inliner asked in the class context of
    // an indy-planned site (its target class), whose name this class's loader
    // may not answer (`CompileRequest::indy_context_callee_compiler`).
    let indy_context_callee =
        |context: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            direct_callee_lookup_in(
                ClassId::new(context),
                callee_class,
                callee_method,
                callee_desc,
            )
        };
    let indy_context_ir_inline =
        |context: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            resolve_ir_inline_site(
                shared,
                ClassId::new(context),
                callee_class,
                callee_method,
                callee_desc,
                if cratonvm_jit::ir_splice_direct_call_enabled() {
                    Some(&direct_callee_lookup_in as InlineDirectBind<'_>)
                } else {
                    None
                },
            )
        };
    let indy_by_id = jit_indy_admit_by_id_enabled();
    // Interpreter round i4 wave 95, lane L2 (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`):
    // the unresolved-field trap at this method-entry door too. The background
    // tiered worker's C2 task compiles here (`try_jit_compile_wrapped_entry`),
    // not at the upgrade door, so without it the switch admitted nothing under
    // the background compiler. Optimizing compiles only: a single-pass compile
    // never builds IR, and asking would record sites nothing plants.
    let field_site_trap =
        |cp_idx: u16, pc: usize| field_site_trap_admitted(shared, cached, cp_idx, pc, "callee");
    // JIT round 21 lane indylate2 (`CRATONVM_JIT_INDY_ADMIT_BY_ID`, re-landed
    // t1 wave 8 lane RL-REFL): the direct binder and the optimizing inliner
    // asked in the class context of an indy-planned site (its target class),
    // whose name this class's loader may not answer
    // (`CompileRequest::indy_context_callee_compiler`).
    let indy_context_callee =
        |context: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            direct_callee_lookup_in(
                ClassId::new(context),
                callee_class,
                callee_method,
                callee_desc,
            )
        };
    let indy_context_ir_inline =
        |context: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            resolve_ir_inline_site(
                shared,
                ClassId::new(context),
                callee_class,
                callee_method,
                callee_desc,
                if cratonvm_jit::ir_splice_direct_call_enabled() {
                    Some(&direct_callee_lookup_in as InlineDirectBind<'_>)
                } else {
                    None
                },
            )
        };
    let indy_by_id = jit_indy_admit_by_id_enabled();
    let mut compiled = cp.with_compile_request(&inputs, &helpers, |cp_request| {
        crate::jit::try_compile_request(&crate::jit::CompileRequest {
            // Lookup-only: binds an ALREADY-compiled callee to a raw CALL, never
            // compiles one (see `direct_callee_lookup` above).
            callee_compiler: Some(&direct_callee_lookup),
            cp_uninit_new_trap_resolver: Some(&uninit_new_trap),
            cp_field_trap_resolver: if optimize {
                Some(&field_site_trap)
            } else {
                None
            },
            cp_uninit_new_guard_resolver: Some(&uninit_new_guard),
            profile: pgo_profile.as_ref(),
            inline_resolver: Some(&inline_resolver),
            string_layout_resolver: Some(&string_layout_resolver),
            cp_elidable_init_resolver: if scalar_new_on {
                cp_request.cp_elidable_init_resolver
            } else {
                None
            },
            // wire-tiered-manager Step 3: `optimize` selects the backend per call.
            // Inline JIT-dispatch callers pass `true` (optimized C2); the background
            // tiered worker passes the C1/C2 value derived from the task's tier.
            optimize: optimize,
            // Gap B: int-only invokestatic → Op::Call. Now default-ON (inc 23);
            // `CRATONVM_JIT_IR_CALL=0` is the opt-out (single-pass dispatch).
            ir_emit_calls: crate::runtime::env_cache::jit_ir_call(),
            // inc 24/29: invokespecial → Op::Call. Now default-ON; `CRATONVM_JIT_IR_CALL_SPECIAL=0` opts out.
            ir_emit_special_calls: true,
            // inc 25/29: long methods → IR path. Now default-ON; `CRATONVM_JIT_IR_LONG=0` opts out.
            ir_emit_long: crate::runtime::env_cache::jit_ir_long(),
            // inc 26: invokevirtual/invokeinterface → Op::Call (dynamic dispatch via
            // the helper), gated default-OFF (its own soak). `=1` opts in.
            ir_emit_virtual_calls: crate::runtime::env_cache::jit_ir_call_virtual(),
            // inc 30 + Slices A/B/C: double/float XMM value tier. Now default-ON
            // (opcode-complete + validated == HotSpot). `CRATONVM_JIT_IR_FP=0` opts out.
            ir_emit_fp: true,
            class_id_name_resolver: if crate::runtime::env_cache::jit_guarded_virtual_inline() {
                Some(&class_id_namer)
            } else {
                None
            },
            receiver_inline_resolver: if crate::runtime::env_cache::jit_guarded_virtual_inline() {
                Some(&receiver_inline_resolver)
            } else {
                None
            },
            // CHA has its own lever (`CRATONVM_JIT_CHA=0`) AND rides the guard it
            // feeds. The conjunction is the point: `jit_cha` makes "is
            // class-hierarchy analysis the cause" a one-variable bisect that leaves
            // the receiver-profile devirtualization in place, while
            // `jit_guarded_virtual_inline` stays load-bearing because CHA's verdict
            // is lowered as a receiver class-id compare — with no namer the plan
            // would be refused for want of an invalidation dependency anyway. See
            // `env_cache::jit_cha`.
            unique_concrete_resolver: if crate::runtime::env_cache::jit_cha()
                && crate::runtime::env_cache::jit_guarded_virtual_inline()
            {
                Some(&unique_concrete_resolver)
            } else {
                None
            },
            // IR-tier inlining. Behind its own gate; `None` splices nothing.
            ir_inline_resolver: if cratonvm_jit::ir_inline_enabled() {
                Some(&ir_inline_resolver)
            } else {
                None
            },
            // JIT round 21 lane indylate2: see `indy_context_callee`.
            indy_context_callee_compiler: if indy_by_id {
                Some(&indy_context_callee)
            } else {
                None
            },
            indy_context_ir_inline_resolver: if cratonvm_jit::ir_inline_enabled() && indy_by_id {
                Some(&indy_context_ir_inline)
            } else {
                None
            },
            // Round 12 wave 5 (lane iropt4): exact-receiver splices, as at the
            // main optimizing door.
            ir_exact_receiver_resolver: if cratonvm_jit::ir_inline_enabled()
                && cratonvm_jit::ir::ir_exact_receiver_splice_enabled()
            {
                Some(&ir_exact_receiver_resolver)
            } else {
                None
            },
            // Round 13 wave 12 (lane guardsplice, W7-1): profile-selected
            // guarded splices, as at the main optimizing door.
            ir_receiver_inline_resolver: if cratonvm_jit::ir_inline_enabled()
                && cratonvm_jit::ir::ir_profile_guarded_splice_enabled()
            {
                Some(&ir_receiver_inline_resolver)
            } else {
                None
            },
            // Per-VM Real-JDK policy (JDK-ONLY-WAVE2 §2). Was a process-global
            // latch the JIT read for itself, so a `Compatible` VM sharing a
            // process with a `JdkOnly` one lost the thin direct-call helpers.
            jdk_only: crate::vm::dispatch_policy(shared).is_jdk_only(),
            // JDK-ONLY-WAVE2 §4: the registry's own `NativeKind`, in place of
            // the JIT's seven hard-coded triples, as the §1.4 verdict on
            // whether a thin direct-call helper may shadow real bytecode.
            direct_bind_admission: Some(&direct_bind_admission),
            // This VM's per-bci de-spec registry.
            despec: Some(&shared.jit.despec_registry),
            // M7 — this VM's runtime de-speculation verdicts.
            runtime_despec: Some(&shared.jit.runtime_despec),
            self_call_identity_stable: self_call_identity,
            direct_helpers: &crate::jit::helpers::direct_helper_table_for(shared),
            cycle_edge_admission: Some(&cycle_edge_admission),
            // Round 9 wave 8 (`arraylist8`): final-class verdicts for the inline
            // `instanceof` miss. See `jit_class_is_final_for_typecheck`.
            class_is_final_resolver: Some(&class_is_final_resolver),
            class_is_subclass_resolver: Some(&class_is_subclass_resolver),
            // Round 11 wave 13 (lane `spsync`): caller-held synchronized
            // direct CALLs, for this door's single-pass tables too. See
            // `sync_direct_lookup` above.
            sync_direct_lookup: Some(&sync_direct_lookup),
            ..cp_request
        })
    })?;
    note_first_call_waiters(shared, &cp, cached);
    // JIT round 20 wave 17 (lane pStr3): see `note_static_init_waiters`.
    note_static_init_waiters(shared, &compiled, cached, static_init_seq0);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] full-compile {}.{}{} entry={:p} len={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            compiled.entry_ptr(),
            compiled.code_bytes().len()
        );
    }
    crate::jit::disasm::maybe_dump(
        if compiled.used_ir_backend {
            "full/ir"
        } else {
            "full/sp"
        },
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        compiled.entry_ptr(),
        compiled.code_bytes(),
    );
    // The eager first-call door's half of the deferred-`new` retry. This door
    // reaches the backend directly and produces no `CompileOutcome`, so the
    // worker-loop route in `compile_one_task` never sees a method compiled
    // here -- which is every method the interpreter hands over on its own,
    // `RJitGc.make` among them. The one-shot memo is consumed here exactly as
    // it is there, so the two doors together still produce at most one extra
    // compile per method.
    if crate::runtime::env_cache::c2_supersede()
        && take_deferred_new_retry_if_resolvable(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    {
        let answer = shared
            .jit
            .tiered_manager
            // With the declaring class's identity, as the re-sweep below and
            // every invocation hook build it: `MethodKey` hashes `class_id`, so
            // a `MethodKey::new` (class id 0) key matched none of them and took
            // a second in-flight slot beside the real one.
            .request_deferred_new_retry(&crate::jit::tiered::MethodKey::with_class_id(
                cached.declaring_class_id,
                &*cached.class_name,
                &*cached.method_name,
                &*cached.method_descriptor,
            ));
        // JIT round 21 lane indyconst (`CRATONVM_JIT_ONE_SHOT_RETRY_OWED`):
        // the memo is spent; a background task holding the slot may have
        // built before the class loaded.
        owe_one_shot_retry_if_already_queued(
            shared,
            answer,
            cached.declaring_class_id.as_u32(),
            &cached.method_name,
            &cached.method_descriptor,
            "deferred-new",
        );
    }
    // …and the other half: every method whose retry is HELD because its class
    // was not loaded. Nothing brings such a method back on its own — it already
    // has a body, so it is never compiled again, and the door above is only ever
    // walked by the method being compiled right now.
    resweep_held_deferred_new_retries(shared, false);
    let compile_duration_ns = compile_start.elapsed().as_nanos() as u64; // Cast: duration to u64 nanoseconds
                                                                         // SBR5 lane jfr: the real JFR backend's jdk.Compilation (+ the counters
                                                                         // behind jdk.CompilerStatistics). Nothing is locked here.
    cratonvm_native_builtins_jfr::jfr_vm_more::compilation(
        shared.vm_identity,
        class_name,
        &cached.method_name,
        &cached.method_descriptor,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            - compile_duration_ns,
        compile_duration_ns,
        i64::try_from(compiled.code_bytes().len()).unwrap_or(i64::MAX),
        false,
        true,
        4,
    );

    // Record JFR compilation event.
    //
    // GC-STW-safety / lock-scope discipline (wire-tiered-manager increment 3):
    // when this runs on the GC-neutral `cratonvm-jit-compiler` worker, the
    // `shared.debug.flight_recorder.lock()` must be held for the MINIMAL scope and
    // NEVER across a blocking op or a nested VM-lock acquisition — a mutator
    // wanting the recorder must not stall behind the worker (which would keep
    // that mutator off its safepoint and stall a third-thread STW). The
    // timestamp and the `Arc<str>` description (a Rust-heap alloc, not a managed
    // GC allocation) are built BEFORE the lock so the guard's live region is
    // exactly the `emit_compilation_event_arc` call and nothing else. No other
    // VM lock (`class_manager` / `jit_cache`) is held here — `cm` was dropped at
    // the `drop(cm)` above, and `jit_cache.write()` is taken AFTER this block.
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64; // Cast: duration to u64 nanoseconds
                            // Round-9 HIGH-5: build the Arc<str> once and hand ownership to the
                            // `_arc` variant instead of letting `emit_compilation_event` reallocate
                            // a fresh Arc from `&str` internally. Built before the lock so the alloc
                            // is outside the `flight_recorder` critical section.
    let method_desc: Arc<str> = Arc::from(format!(
        "{}::{}{}",
        cached.class_name, cached.method_name, cached.method_descriptor
    ));
    {
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_compilation_event_arc(
            &mut jfr,
            method_desc,
            1,     // compile_id
            4,     // compile_level (C2 equivalent)
            true,  // succeeded
            false, // is_osr
            0,     // code_size
            0,     // inlined_bytes
            now_ns.saturating_sub(compile_duration_ns),
            compile_duration_ns,
        );
    }

    // Store in JIT cache.
    //
    // Recompile-storm fix: key by the RECEIVER `class_name` (the value the
    // lookup in `try_jit_compile_callee` uses), NOT `cached.class_name` (the
    // DECLARING class). For an INHERITED method the two differ — e.g.
    // `SingletonPredictionContext.hashCode` resolves to the final
    // `PredictionContext.hashCode`, whose declaring class is
    // `PredictionContext`. Storing under the declaring class while the lookup
    // probes the receiver class made the cache miss on every polymorphic call,
    // so the dispatch helper recompiled the same method endlessly (42k+
    // recompiles of `PredictionContext.hashCode` observed during a Groovy
    // parse). Keying by the receiver makes the next call on the same receiver
    // class hit; distinct subclasses recompile at most once each. The compiled
    // code is identical regardless of receiver (it is the resolved method's
    // body), so dispatching it for any receiver of that class is correct.
    //
    // GC-STW-safety / lock-scope discipline (wire-tiered-manager increment 3):
    // this is the "publish" step — on the GC-neutral worker it is the moment a
    // freshly compiled body becomes visible to mutators (their `jit_cache`
    // fast-path flips the call site to `Jit` on the next invocation, the
    // cross-thread analogue of flipping the invoke cache). The
    // `shared.jit.jit_cache.write()` is held for the MINIMAL scope: the receiver
    // key `Arc` is built BEFORE the lock, the codegen + every resolver read of
    // `class_manager` already completed above (no VM lock is live here), and the
    // guard covers exactly the `put`. Holding nothing else means a mutator
    // taking `jit_cache.read()` (or `class_manager.write()` to define a class)
    // never blocks behind the worker, so it always reaches its safepoint and a
    // concurrent STW completes promptly.
    let receiver_key: std::sync::Arc<str> = std::sync::Arc::from(class_name);
    let method_name_key = cached.method_name.clone();
    let method_desc_key = cached.method_descriptor.clone();
    // JIT round 21 lane reflect4 (REFLECT4-21-1): the key this body is
    // published and read back under. A bytecode-only body takes the
    // door-invisible descriptor; every other use of the method's identity
    // (the epoch stamp, the waiters, the verdicts) keeps the real one.
    let publish_desc_key: std::sync::Arc<str> = if bytecode_only {
        super::bytecode_only_body::bytecode_only_key_descriptor(&method_desc_key)
    } else {
        method_desc_key.clone()
    };
    // A supersede the acceptance gate refused publishes NOTHING.
    //
    // The gate discards the optimizing body and `try_compile_inner` falls
    // through to the single-pass backend, so what reaches this point is an
    // equivalent of the body already in the cache. Publishing it replaces a C1
    // body with an equal one AND bumps the VM's supersede epoch (process-wide
    // then), which stales every cached invoke target in every thread -- 1,558 evictions on
    // one H2 run. That is the entire cost the gate exists to avoid, paid in
    // full by a gate that refused.
    //
    // Measured before this: with the gate refusing 571 bodies,
    // `CRATONVM_C2_SUPERSEDE=0` was still ~5% faster in mean CPU on H2 against
    // a control pair agreeing to 0.04%.
    //
    // Both halves of the condition matter. A REFUSED verdict alone is not
    // enough: at the eager first-call door there is no predecessor, and
    // skipping the publish there would leave the method interpreted. `None`
    // means the method never reached the optimizing pipeline, which is an
    // absence of opinion rather than a refusal.
    //
    // Round 15 wave 1 (lane calls): "an equivalent of the body already in the
    // cache" is the premise, and it fails when the published baseline resolved
    // FEWER of its calls than this fall-through does -- see
    // `fall_through_resolves_more_calls`. Such a baseline is not abandoned to.
    //
    // JIT r25 (W13-2 residual, `CRATONVM_JIT_C2_FALL_THROUGH_KEEPS_BASELINE`,
    // default ON): not only an acceptance refusal. Any optimizing compile
    // whose IR attempt fell through (a build bail, a row-less field site, an
    // admission pin the pipeline reached) produces the same equivalent of the
    // baseline. Republished, it displaced a byte-identical body (`c2-supersede
    // published ... outcome=unchanged`, ~11 per `regex-groups` run), made it
    // not entrant behind a stub its baked callers then hopped through, and
    // bumped the VM-wide supersede epoch. The baseline must still be entrant.
    let (refused_verdict, fell_through_over_baseline) = supersede_fall_through_reading(
        compiled.used_ir_backend,
        optimize,
        cratonvm_jit::ir_evidence::take_last_verdict() == Some(false),
        cratonvm_jit::last_compile_fell_through_to_single_pass(),
        true,
    );
    let refused_supersede = (refused_verdict || fell_through_over_baseline)
        && {
            let jit_cache = shared.jit.jit_cache.read();
            jit_cache
            .get(
                &receiver_key,
                &method_name_key,
                &publish_desc_key,
                callee_class_id,
            )
            .filter(|published| !fell_through_over_baseline || !published.is_not_entrant())
            .is_some_and(|published| {
                let richer = fall_through_resolves_more_calls(&compiled, &published);
                if richer && crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] supersede KEPT {}.{}{} -- the refused optimizing body's single-pass fall-through binds or splices more calls than the published baseline (inlined {} vs {}, direct {} vs {})",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        compiled.inline_tally.inlined_sites,
                        published.inline_tally.inlined_sites,
                        compiled._direct_callee_entries.len(),
                        published._direct_callee_entries.len(),
                    );
                }
                // Round 15 wave 4 (lane sync3, SY2-2): nor to a WRAPPED
                // baseline, when the fall-through locks its own monitor.
                let sheds_door = fall_through_sheds_the_door(&compiled, &published);
                if sheds_door && crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] supersede KEPT {}.{}{} -- the refused optimizing body's single-pass fall-through locks its own monitor and the published baseline is a wrapped entry",
                        cached.class_name, cached.method_name, cached.method_descriptor,
                    );
                }
                !richer && !sheds_door
            })
        };
    if refused_supersede {
        cratonvm_jit::ir_evidence::note_supersede_abandoned();
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] supersede ABANDONED {}.{}{} -- {} and a baseline body is already published",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
                if refused_verdict {
                    "the optimizing body carried no evidence"
                } else {
                    "the optimizing attempt fell through to the single-pass backend"
                },
            );
        }
        return None;
    }

    stamp_compilation_epoch(
        shared,
        &receiver_key,
        &method_name_key,
        &method_desc_key,
        &mut compiled,
    );
    // Round 13 wave 3 (lane replay2; R13-1): the compile's own template
    // (the DECLARING method, also when the body is keyed by a receiver).
    stamp_compiled_source_of(&compiled, Arc::clone(cached));
    // Wave 42 (lane L2): the positive control of `bytecode_read_witness`
    // above. A redefinition of the class that landed between this compile's
    // bytecode read and here makes `put` refuse the body; name it. Only
    // under `CRATONVM_DBG_JITC`, and only in a VM that redefined something.
    if crate::runtime::env_cache::dbg_jitc()
        && crate::classloading::any_class_redefined()
        && !shared
            .jit
            .jit_cache
            .compiled_since_redefinition_of(callee_class_id, compiled.install_epoch)
    {
        eprintln!(
            "[cratonvm-jitc] compile of {}.{}{} read its bytecode before a redefinition of its class: \
             publication refused (bytecode-read witness)",
            cached.class_name, cached.method_name, cached.method_descriptor,
        );
    }
    // JIT round 16 wave 14 (lane throw4, TP16-20): a recompile
    // `late_bind_site_served` asked for retires the body it displaces.
    // JIT round 19 wave 2, lane eager2 (EAGER19-3): only a compile started
    // after the request consumes its mark (`u64::MAX` with the switch off).
    let late_bind_take = if bytecode_only {
        LateBindPendingTake::None
    } else {
        late_bind_take_pending_started(shared, cached, late_bind_started)
    };
    let late_bind_republish = late_bind_take == LateBindPendingTake::Taken;
    // An inherited method is published under the receiver's key; its frames
    // still report the declaring class (`CompiledMethod::holder_class_id`).
    if declaring_id != callee_class_id {
        compiled.holder_class_id = declaring_id.as_u32();
    }
    // Stamp the wrapped-entry requirement before the body is shared (kept next
    // to the `put`: `every_jit_cache_publication_stamps_the_wrapped_entry_requirement`
    // looks 900 bytes back from it).
    // See `CompiledMethod::requires_wrapped_entry`: publication is the
    // last point that still knows this is an `ACC_SYNCHRONIZED` method,
    // and every unwrapped consumer downstream holds only a raw entry
    // pointer.
    compiled.requires_wrapped_entry = cached.is_synchronized;
    // Tier proposal R12-4: a single-pass body never replaces a live
    // optimizing one (`JitCache::put` decides under its lock). The read-back
    // below then returns the optimizing body, which is what the caller runs.
    compiled.keep_live_optimizing_body =
        !optimize && crate::jit::tiered::keep_live_optimizing_body_enabled();
    // What `put` may refuse without a word (see `note_refused_optimizing_publication`).
    let offered_optimizing = optimize && compiled.used_ir_backend;
    let offered_artifact = compiled.artifact_id;
    // JIT round 19, lane eager19 (TP16-23): read the body this publish may
    // displace for the forward too ([`supersede_forward_every_door_enabled`]).
    let forward_every_door =
        !late_bind_republish && !bytecode_only && supersede_forward_every_door_enabled();
    // JIT round 20, lane stash (`r21-supersede-redirect-residuals`, item 1):
    // the supersede redirect needs the cache armed; arm it before the `put`.
    arm_not_entrant_at_publish(shared);
    let (published, late_bind_displaced, forward_displaced) = {
        let jit_cache = shared.jit.jit_cache.write();
        let before = (late_bind_republish || forward_every_door)
            .then(|| {
                jit_cache.get(
                    &receiver_key,
                    &method_name_key,
                    &publish_desc_key,
                    callee_class_id,
                )
            })
            .flatten();
        let (displaced, forward_displaced) = if late_bind_republish {
            (before, None)
        } else {
            (None, before)
        };
        // VH3-1: see `compile_osr_body`.
        if !crate::jit::helpers::varhandle_thin_bound_artifact_is_stale(shared, &compiled) {
            jit_cache.put(
                receiver_key.clone(),
                method_name_key.clone(),
                publish_desc_key.clone(),
                callee_class_id,
                compiled,
            );
        } else if crate::runtime::env_cache::dbg_jitc() {
            // JIT round 21 wave 3 (lane splice): the drop names its reason,
            // as `JitCache::put`'s own drops do (`jit-cache-put DROPPED`).
            eprintln!(
                "[cratonvm-jitc] jit-cache-put DROPPED {receiver_key}.{method_name_key}{method_desc_key} ({}): \
                 vh-thin-bind-stale (CRATONVM_JIT_VH_THIN_BIND_PUBLISH_REFUSAL)",
                if compiled.used_ir_backend { "ir" } else { "sp" },
            );
        }
        // Read back a STRONG reference to what is now published under this key
        // rather than returning the address we held before the `put`. If a
        // concurrent publish won the race, this is its artifact — the live one
        // — instead of one whose buffer is already being unmapped.
        // Wave 117 (L2): including a re-entry-only body, so the worker reports
        // the publication; the binding funnel filters it.
        let published = jit_cache.get_for_reentry(
            &receiver_key,
            &method_name_key,
            &publish_desc_key,
            callee_class_id,
        );
        (published, displaced, forward_displaced)
    };
    let Some(published) = published else {
        // Nothing published under the key (a withdrawal raced the read-back):
        // a displaced body read for the forward is released the authorised
        // way, never by a bare drop (it may be the last owner). So is the
        // late-bind one, which the `?` this replaces dropped bare (a
        // correctness fix, unconditional).
        if let Some(displaced) = forward_displaced.or(late_bind_displaced) {
            drop(cratonvm_jit::RetainedCode::new(displaced));
        }
        // JIT round 20, lane stash (EAGER2-1): the mark this republish took
        // is not spent on a body nobody published.
        if late_bind_republish {
            late_bind_restore_after_unpublished_put(shared, cached);
        }
        return None;
    };
    // JIT round 20, lane stash (EAGER2-1): the `put` kept another body (a
    // refusal, a raced publish, the VarHandle-stale skip); the mark this
    // republish took above is put back and the recompile asked again.
    if late_bind_republish && published.artifact_id != offered_artifact {
        late_bind_restore_after_unpublished_put(shared, cached);
    }
    if offered_optimizing
        && !bytecode_only
        && published.artifact_id != offered_artifact
        && !published.used_ir_backend
    {
        note_refused_optimizing_publication(&receiver_key, &method_name_key, &method_desc_key);
    }
    if late_bind_take == LateBindPendingTake::Older {
        late_bind_note_older_publication(shared, cached);
    }
    if let Some(displaced) = late_bind_displaced {
        late_bind_retire_displaced(shared, &published, displaced, cached);
    } else if let Some(displaced) = forward_displaced {
        // JIT round 19, lane eager19 (TP16-23): every other supersede here.
        supersede_forward_displaced(shared, displaced, &published, &cached.method_name);
    }
    // JIT round 19, lane eager19 (TP16-21): a published self-locking body
    // wakes the methods that refused to bind it before it existed. Both keys:
    // the declaring class (how the refusal names it) and the receiver key this
    // door published under.
    // Not for a bytecode-only body: those refusals bind the ordinary key.
    if !bytecode_only {
        late_bind_self_lock_published(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            &published,
        );
    }
    if !bytecode_only && *receiver_key != *cached.class_name {
        late_bind_self_lock_published(
            shared,
            &receiver_key,
            &cached.method_name,
            &cached.method_descriptor,
            &published,
        );
    }
    // A late static cell calls a newly published callee directly, so it never
    // fills `jit_invoke_dispatch`'s cache and cannot wake that site's waiters.
    // Publication is safe to serve here: the bounded cold-callee waiter was
    // registered only after the caller observed this target as never-run or
    // below its warm threshold, and this body now exists for direct binding.
    if !bytecode_only
        && shared
            .jit
            .bridge
            .late_bind_waiter_count
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    {
        let receiver_waiter_key =
            late_bind_callee_key(&receiver_key, &method_name_key, &publish_desc_key);
        late_bind_key_served(
            shared,
            &receiver_waiter_key,
            "published its direct-call body",
        );
        if *receiver_key != *cached.class_name {
            let declaring_waiter_key = late_bind_callee_key(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
            late_bind_key_served(
                shared,
                &declaring_waiter_key,
                "published its direct-call body",
            );
        }
    }
    let entry = published.entry_ptr() as usize; // Cast: JIT entry point to address
    let needs_ctx = published.needs_context();
    Some((published, entry, needs_ctx))
}

/// Round 15 wave 1 (lane calls,
/// `r14w7-mega-static-leaf-call-costs-an-interpreter-call-under-c2-never`): does
/// a refused supersede's single-pass fall-through (`fresh`) splice or direct-bind
/// MORE of its calls than the single-pass baseline already published under its
/// key (`published`)?
///
/// The abandon rule in [`try_jit_compile_callee_slow`] assumes the fall-through
/// recompiles the published body. It does not when that body came from a door
/// that compiles with fewer resolvers: the optimizing door's eager callee
/// compile (`compile_optimizing_artifact`, "no inlining in early-compile path")
/// passed no inline resolver and no callee compiler (until round 15 wave 2,
/// CR15-1, and still with `CRATONVM_JIT_INPLACE_CALLEE_LOOKUP_BINDER=0`), so
/// every static call in such a body is a `jit_invoke_dispatch` round trip. A
/// body bound when fewer callees were published is poorer the same way.
/// Measured on
/// `R12Mega4OneSite` under `CRATONVM_C2_ACCEPT=never`: `entStatic24`'s stripped
/// body (4000 bytes, the `leaf` call on the helper, ~160 ns per call) stayed
/// published for the whole run while every tier-up built, and abandoned, a body
/// splicing `leaf`. The default `evidence` policy strands such a body whenever
/// the caller's optimizing body carries no evidence.
///
/// "More" is the SUM of the two counts (round 15 wave 2, lane calls2). Wave 1
/// kept a fall-through that beat the baseline on EITHER count, and that is not
/// an order: a site spliced in one compile and direct-bound in the next trades
/// one count for the other, so `(1 spliced, 0 bound)` and `(0, 1)` each
/// "resolved more" than the other and two such compiles could replace each
/// other on every refused supersede (each bumping the supersede epoch). The
/// sum strictly grows with every body kept, so the keeps are bounded: no
/// churn. A published OPTIMIZING body is never displaced (its tallies are not
/// comparable, and a single-pass body must not replace a live optimizing one).
/// `CRATONVM_JIT_SUPERSEDE_KEEP_RICHER_FALLTHROUGH=0` restores the unconditional
/// abandon.
pub(in crate::runtime::interpreter::jit_bridge) fn fall_through_resolves_more_calls(
    fresh: &cratonvm_jit::CompiledMethod,
    published: &cratonvm_jit::CompiledMethod,
) -> bool {
    let resolved = |body: &cratonvm_jit::CompiledMethod| -> usize {
        // Widening: u32 splice count to usize
        (body.inline_tally.inlined_sites as usize).saturating_add(body._direct_callee_entries.len())
    };
    !published.used_ir_backend && !fresh.used_ir_backend && resolved(fresh) > resolved(published)
}

/// The supersede abandon's reading of a finished optimizing compile:
/// `(refused_verdict, fell_through_over_baseline)`. Both premise a
/// single-pass fall-through, "an equivalent of the body already in the
/// cache", so a body the OPTIMIZING backend produced answers `(false, false)`
/// whatever the thread-local signals say. They are per thread, not per
/// compile: a nested callee compile on this thread (the eager chain of the
/// optimizing compile) resets and re-sets them, and its fall-through used to
/// abandon the outer compile's accepted body (`supersede ABANDONED` over a
/// `full-compile` IR body; `ConfSbSteady` under `--compatible` kept the
/// single-pass `kernel` for good).
pub(in crate::runtime::interpreter::jit_bridge) fn supersede_fall_through_reading(
    used_ir_backend: bool,
    optimize: bool,
    refused_verdict: bool,
    fell_through: bool,
    keeps_baseline: bool,
) -> (bool, bool) {
    if used_ir_backend {
        return (false, false);
    }
    (
        refused_verdict,
        optimize && !refused_verdict && fell_through && keeps_baseline,
    )
}

/// Round 15 wave 4 (lane sync3, SY2-2): is the refused optimizing body's
/// single-pass fall-through `fresh` a SELF-LOCKING body
/// (`CompiledMethod::self_locks_monitor`) and the `published` baseline a
/// WRAPPED one? Then the premise of the abandon ("an equivalent of the body
/// already in the cache") fails as it does for a richer fall-through: every
/// compiled caller of the baseline pays the ~400 ns door, and of `fresh` a
/// CALL. The shape [`request_static_self_lock_recompile`] asks for: under
/// `CRATONVM_JIT_IR_SELF_LOCKING_SYNC` its recompile tries the optimizing
/// self-lock first, and an acceptance-gate decline would otherwise abandon the
/// self-locking fall-through and keep the wrapped body for good. Bounded: a
/// self-locking body is never wrapped, so a kept one is never displaced by
/// this rule again. Under [`static_self_lock_recompile_enabled`].
pub(in crate::runtime::interpreter::jit_bridge) fn fall_through_sheds_the_door(
    fresh: &cratonvm_jit::CompiledMethod,
    published: &cratonvm_jit::CompiledMethod,
) -> bool {
    fresh.self_locks_monitor
        && published.requires_wrapped_entry
        && !published.self_locks_monitor
        && static_self_lock_recompile_enabled()
}

/// Round 15 wave 1 (lane calls): the acceptance gate's verdict is ONE
/// thread-local slot (`ir_evidence::publish_verdict` / `take_last_verdict`),
/// and [`try_jit_compile_callee_slow`] reads it as its own compile's answer.
///
/// Two ways the slot held someone else's answer:
///  * NESTED: the outer method's gate publishes, then its single-pass
///    fall-through runs, and that fall-through's direct binder
///    (`direct_callee_lookup_bound`, the eager callee chain) compiles a callee
///    through this same function, which took the OUTER verdict as its own and
///    left the slot empty. The outer method then read `None` ("never reached the
///    optimizing pipeline") and republished its equivalent fall-through, paying
///    the supersede the abandon rule exists to avoid. The pending verdict is now
///    handed back when the nested compile returns.
///  * STALE: a door that publishes and never takes
///    (`compile_optimizing_artifact`'s in-place callee compile) left a verdict
///    the next top-level compile on the thread read as its own. Discarded at
///    entry now. Since round 15 wave 2 (lane calls2, CR15-4) that door and
///    `compile_optimizing_artifact` itself use this hand-off too and take their
///    own verdict, so neither leaves one behind
///    (`CRATONVM_JIT_OPTIMIZING_DOOR_OWN_VERDICT`).
///
/// `CRATONVM_JIT_NESTED_COMPILE_KEEPS_OUTER_VERDICT=0` restores the old
/// behaviour (no take at entry, nothing handed back).
pub(in crate::runtime::interpreter::jit_bridge) struct NestedVerdictHandOff(
    pub(in crate::runtime::interpreter::jit_bridge) Option<bool>,
);

impl NestedVerdictHandOff {
    pub(in crate::runtime::interpreter::jit_bridge) fn take_for_this_compile() -> Self {
        let pending = cratonvm_jit::ir_evidence::take_last_verdict();
        // `0` outside any compile: a pending verdict there is stale.
        Self(pending.filter(|_| cratonvm_jit::jit_active_compile_depth() > 0))
    }
}

impl Drop for NestedVerdictHandOff {
    fn drop(&mut self) {
        if let Some(verdict) = self.0.take() {
            cratonvm_jit::ir_evidence::publish_verdict(verdict);
        }
    }
}

/// wire-tiered-manager increment 2 — the REAL off-thread compile callback.
///
/// Invoked on the background compile thread (see
/// `jit::tiered::start_background_compiler`) for each `CompilationTask` the
/// tiered manager drained off the mutator. Gated by `CRATONVM_BG_COMPILE`
/// (the closure is only installed when the flag is on).
///
/// It upgrades the captured `Weak<SharedVm>` (the worker holds no `Arc` of its
/// own, so it cannot keep the VM alive past teardown) and runs the same codegen
/// entry point the inline mutator path uses: [`try_jit_compile_callee`] does the
/// by-name `(class, method, descriptor)` lookup, builds the constant-pool
/// resolvers, calls `jit::try_compile`, and PUBLISHES the result into
/// `shared.jit.jit_cache`. Publishing into the shared cache is the cross-thread
/// "flip the invoke cache" mechanism: the per-thread `invoke_cache` is
/// thread-local and cannot be mutated from here, but the `Bytecode` arm's
/// `jit_cache` fast-path (interpreter.rs ~14366) upgrades the call site to
/// `Jit` on the next mutator invocation once the entry is present.
///
/// Step 3 (C1/C2 routing) — NOW REAL: the target tier selects the backend via
/// [`crate::jit::tiered::tier_uses_optimized_backend`], and that boolean is
/// threaded through `try_jit_compile_callee` into `jit::try_compile`'s trailing
/// `optimize` flag. `C2`/`FullProfile` → the optimizing IR pipeline; `C1`/
/// `C1WithProfiling` → the fast single-pass `x64::compile` backend (no IR
/// lowering / escape analysis / scheduling). This replaces the former advisory-
/// only hint (which compiled both tiers identically).
///
/// Boundary (follow-ups): the JIT-cache probe in `try_jit_compile_callee`
/// returns any already-published body regardless of tier, so a method is
/// compiled at whatever tier reaches it first — there is no C1→C2 supersede /
/// re-compile yet (that needs safe code-cache replacement; cf. the bug-24 baked-
/// pointer UAF risk). The single-pass backend still runs its own internal escape
/// analysis; disabling x64-internal passes for an even-leaner C1 is a separate
/// step. Both are out of scope for this routing increment and gated default-off
/// behind `CRATONVM_BG_COMPILE` regardless.
///
/// Returns `(wall_clock_compile_time_ms, published)` for the tiered stats.
/// A compile miss / bail (native shadow, skip-listed, backend bail, or a dropped
/// VM) reports `published = false` — the queued flag is still cleared by the
/// worker's `complete_task`, but (unlike an earlier version of this code)
/// `current_tier` is NOT advanced on a failed attempt, so a later mutator
/// invocation genuinely re-attempts (bounded by
/// `jit::tiered::MAX_TIER_FAIL_RETRIES` — see `complete_task`) instead of the
/// method being silently marked "compiled" and stuck interpreting forever.
///
/// ## GC-neutral daemon (wire-tiered-manager increment 3)
///
/// This closure body is the entire VM-side surface of the
/// `cratonvm-jit-compiler` worker, which `jit::tiered::start_background_compiler`
/// spawns as an UNREGISTERED `std::thread::Builder` daemon — exactly the
/// G1-MarkComplete / JDWP class of VM-internal thread. It is deliberately NOT a
/// mutator: it is never `register_with_daemon`'d, never polls a safepoint, never
/// calls `arrive_and_wait`, and holds NO managed `ObjectRef` across any GC point.
/// Therefore the STW barrier's `expected` count (driven by
/// `thread_registry.alive_count()`) never includes it, and concurrent
/// relocation during a STW is harmless to it. Registering it instead would
/// WRONGLY add its native Rust stack to the GC root set and make STW wait on a
/// thread that has no safepoint — neither is wanted.
///
/// It captures a `Weak<SharedVm>` (never an `Arc`, so it cannot keep the VM
/// alive past teardown), upgrades it per task, and NO-OPS when the upgrade fails
/// (VM dropped) — the same `self_arc.upgrade()` pattern JDWP uses. All VM-lock
/// scopes it touches are bounded inside `try_jit_compile_callee[_slow]` (see that
/// function's lock-order contract): nothing is held across the worker's queue
/// wait (its own `CompilerCore::wake` condvar, no VM lock), across class loading,
/// or across the JFR / jit_cache publish.
/// Phase 5 fast boot (WP3): `true` in a boot image's capture run, where
/// compile admission is closed: no compiled code is installed and no compiler
/// thread starts before the snapshot point, so the image holds no code cache.
/// Every door that checks it refuses the way `CRATONVM_DISABLE_JIT=1` does
/// (a plain `None` before any verdict is recorded), so the refusal leaves no
/// state behind for the restored run. One atomic load; `false` in every
/// normal run.
#[inline]
pub(crate) fn compile_admission_closed_for_capture() -> bool {
    cratonvm_types::image_layout::capturing()
}

/// wire-tiered-manager Step 5: start the background compile worker once
/// (idempotent), wiring the real off-thread compile closure. Captures a
/// `Weak<SharedVm>` (the worker owns no Arc of the VM) and, per drained task,
/// runs [`background_compile_task`] off the mutator. Called from BOTH the
/// invocation tier-up trigger AND the back-edge OSR path, so an all-hot-loop
/// program (a `main()` loop that never crosses the invocation threshold) still
/// starts the worker.
pub(in crate::runtime::interpreter) fn ensure_bg_compiler_started(shared: &SharedVm) {
    // Phase 5 WP3: no compiler thread in a boot image's capture run. Tasks the
    // manager queued meanwhile wait for the first call after the restore,
    // which starts the workers (`compiler_active` is still false).
    if compile_admission_closed_for_capture() {
        return;
    }
    // One atomic load once this VM's workers are running. The `Weak` below
    // takes the `self_arc` read lock, and every tier-up stride used to pay it.
    if shared.jit.tiered_manager.compiler_active() {
        return;
    }
    let weak_vm: std::sync::Weak<SharedVm> =
        shared.self_arc.read().as_ref().cloned().unwrap_or_default();
    // The compiled-entry counter drain's third clock, installed before the
    // workers start so the first idle wait is already timed. See
    // `TieredCompilationManager::set_idle_tick` and `drain_jit_entry_counters`'s
    // "The clock, honestly": the other two clocks stop in a fully compiled
    // steady state. The drain is a no-op with `CRATONVM_JIT_ENTRY_COUNTER=0`.
    let idle_vm = weak_vm.clone();
    shared.jit.tiered_manager.set_idle_tick(Box::new(move || {
        if let Some(vm) = idle_vm.upgrade() {
            drain_jit_entry_counters(&vm);
        }
    }));
    crate::jit::tiered::ensure_background_compiler(&shared.jit.tiered_manager, || {
        Box::new(
            move |task: &crate::jit::tiered::CompilationTask| -> crate::jit::tiered::CompileOutcome {
                let outcome = background_compile_task(&weak_vm, task);
                if outcome.published {
                    if let Some(shared) = weak_vm.upgrade() {
                        note_background_published_sizes(&shared, task);
                    }
                }
                outcome
            },
        )
    });
}

/// Release every withdrawn compiled body this thread's dispatch caches still
/// own: the invoke cache's `Jit` entries, the raw-entry dispatch memos and the
/// lambda direct-call sites.
///
/// # Why a sweep freed nothing without this (RT-8 soak, 2026-09-18)
///
/// Each of those caches OWNS its artifact (`RetainedCode`) and lets go of it
/// only when the call site that filled it runs again. The code-cache sweeper
/// withdraws exactly the bodies whose callers have stopped running. So every
/// withdrawn body stayed mapped behind a dead call site's cache entry: on a
/// phased workload at a 1 MiB cap a sweep withdrew 1,040,800 bytes, the
/// retirement queue released none of them, and the cap never cleared.
///
/// # Cost and schedule
///
/// One relaxed load and a compare when nothing was withdrawn since this
/// thread last looked, which is the steady state. A scan only after a
/// withdrawal -- a supersede, an invalidation or a sweep -- and it removes
/// only entries whose body is `retired`, so a live body keeps its cached
/// dispatch. Called by the owning thread (these caches are thread-owned), from
/// the interpreter's tier-up strides -- the same clock that drives the sweeper
/// -- and from `safepoint_check`, which reaches every thread at a GC pause.
///
/// Dropping an entry here is safe even with a frame of that body on this
/// thread's stack: `RetainedCode` releases through the retirement queue, which
/// frees nothing a thread can still return into.
#[inline]
pub(crate) fn release_withdrawn_code_owners(thread: &mut JvmThread) {
    let withdrawn = cratonvm_jit::jit_withdrawn_bodies();
    if withdrawn == thread.jit_withdrawals_seen {
        return;
    }
    release_withdrawn_code_owners_slow(thread, withdrawn);
}

#[inline(never)]
fn release_withdrawn_code_owners_slow(thread: &mut JvmThread, withdrawn: u64) {
    thread.jit_withdrawals_seen = withdrawn;
    thread
        .invoke_cache
        .retain_jit(|code| !code.retired.load(std::sync::atomic::Ordering::Acquire));
    crate::jit::helpers::flush_raw_entry_dispatch_caches();
    super::lambda::release_withdrawn_lambda_code();
}

/// Interpreted invocations between two tier-up offers of a method that is
/// past the warmup threshold but not yet compiled. Small, so a transient
/// upgrade-gate failure is retried within a few hundred calls rather than
/// after another full warmup threshold.
///
/// One definition for every dispatch door; each used to declare its own copy.
pub(crate) const JIT_RETRY_STRIDE: u32 = 64;

/// Should a dispatch site whose method has now been counted `cnt` times offer
/// it for compilation? On the first crossing of `threshold`, then once every
/// [`JIT_RETRY_STRIDE`] calls until a compiled body routes the site elsewhere.
#[inline]
pub(crate) fn should_offer_tier_up(cnt: u32, threshold: u32) -> bool {
    cnt >= threshold && (cnt - threshold) % JIT_RETRY_STRIDE == 0
}

/// Offer one tier-up stride of `cached`'s method to the tiered manager, and
/// return the tier it queued a compile at, if any.
///
/// The door every interpreter tier-up hook goes through. It checks the call
/// site's `tiering_settled` stamp first: a method the manager has already said
/// it can do nothing more for -- declined by policy, out of compile retries,
/// already at C2 -- used to take the manager's global `methods` mutex and
/// allocate three `String`s for its key at every stride, for the life of the
/// process. Now it costs one relaxed load and one atomic compare until
/// something (a deopt, an unload, a redefinition, a policy change) moves the
/// manager's generation. The key it builds shares the cached method's
/// `Arc<str>`s and carries the declaring class's identity, so same-named
/// classes in different loaders keep separate tiering state.
///
/// Starting the worker stays the caller's job, because the `CRATONVM_BG_COMPILE=0`
/// paths consult the manager without one.
pub(in crate::runtime::interpreter) fn offer_invocation_to_tiered_manager(
    shared: &SharedVm,
    // The offering mutator, when the caller holds it mutably: the walk below
    // then mints method-handle rows itself instead of waiting for a recompile.
    mut thread: Option<&mut JvmThread>,
    cached: &cratonvm_jit_api::CachedBytecodeMethod,
    invocation_count: u64,
) -> Option<crate::jit::tiered::CompilationTier> {
    use std::sync::atomic::Ordering::Relaxed;
    let manager = &shared.jit.tiered_manager;
    // Piggyback the compiled-entry counter drain on this door, BEFORE the
    // settled short-circuit. This door is the interpreter's tier-up stride, so
    // it ticks whenever anything in the process is still being interpreted —
    // which is the clock the drain needs and cannot get from the compiled path
    // itself. Placed ahead of the `tiering_settled` return because that stamp is
    // about the method this call names, and the drain is about every OTHER
    // method; a settled caller is still a perfectly good clock. Strided and
    // flag-gated, so a default build pays one memoised boolean here.
    maybe_drain_jit_entry_counters(shared);
    // `CRATONVM_FROZEN_PAGES`: and the frozen facts, which only a mutator
    // may read off the heap; ahead of `tiering_settled` so a withdrawal here
    // un-settles this method on the same stride. One load with the switch off.
    crate::runtime::frozen_pages::refresh_facts_if_due(shared);
    if manager.tiering_settled(cached.tiering_settled.load(Relaxed)) {
        return None;
    }
    // Interpreter round i1 wave 37 (lane L1, cross-lane): nothing is offered
    // while a debugger or agent request holds this VM's compiled code
    // withdrawn (`jvmti_events::note_every_method_needs_the_interpreter`): no
    // door would enter the body, and the next such request would withdraw it
    // again. Nothing is recorded, so the method is offered at its next stride
    // once the request ends. One relaxed load, at a stride, not per call.
    // Wave 39 (lane L1): and for a linger after the last request went, so a
    // debugger's next step finds nothing compiled (`tier_up_held`).
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.tier_up_held() {
        return None;
    }
    // Wave 39 (lane L1): nor a method a breakpoint sits in, whose body a
    // compiled caller could bind past the breakpoint
    // (`interpreter::breakpoint_bars_compiling`; one load without one).
    if super::breakpoint_bars_compiling(
        shared,
        cached.declaring_class_id,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        return None;
    }
    // JIT round 21 wave 11 (lane finalize): nor a finalizable class's
    // constructor that can throw before its `super(..)` (one string compare
    // for any other method).
    if super::finalizer_bars_compiling(
        shared,
        cached.declaring_class_id,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        return None;
    }
    let key = crate::jit::tiered::MethodKey::with_class_id(
        cached.declaring_class_id,
        Arc::clone(&cached.class_name),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
    );
    // nio21 wave 2, lane L2 (proposal L2-5): mint the `ldc` slots BEFORE the
    // manager is asked, because `on_method_invocation_settling` enqueues the
    // background compile itself and an idle worker could otherwise compile the
    // method before the mint below ran, baking slot `0` for good. The OSR door
    // already mints first (`interpreter::try_osr_offer`). Every stride of an
    // unsettled method pays one operand walk (idempotent: a slot that exists
    // is found, not re-made). `CRATONVM_JIT_LDC_SLOT_MINT_BEFORE_ENQUEUE=0`
    // keeps the wave-6 placement after the verdict.
    let mint_before_enqueue = true;
    if mint_before_enqueue {
        let _ = mint_ldc_slots_at_offer(shared, cached.declaring_class_id, &cached.code);
        mint_const_ref_folds_at_offer(shared, thread.as_deref_mut(), cached.declaring_class_id, &cached.code);
        let _ = crate::jit::helpers::class_mirror_slot_for(shared, cached.declaring_class_id);
    }
    // The mutator's first accesses may have resolved a VarHandle slot after
    // clinit's one-shot feed. File those pure plans before the worker can run.
    crate::jit::helpers::refresh_static_varhandles_before_compile(
        shared,
        cached.declaring_class_id,
    );
    let verdict = manager.on_method_invocation_settling(&key, invocation_count);
    resolve_admitted_compile_strings(shared);
    cached
        .tiering_settled
        .store(verdict.settled_generation, Relaxed);
    // The per-method branch-profile arm: a compile was just queued for a method
    // that is, by construction of this door, still interpreted. Recording its
    // branches from here until its first body publishes is the stretch the
    // optimizing compile's scheduler reads (`branch_counts`), and it is the
    // stretch the global C2 window never covers -- that window opens at the
    // C1 -> C2 supersede, after the method left the interpreter. See
    // `docs/feature-designs/jit-osr-entry-metadata.md`.
    //
    // Under `CRATONVM_INTERP_BRANCH_ARM_GATE` (interpreter round i2 wave 62,
    // lane L7) only with a JIT: without one nothing compiles the recorded
    // stretch, and the dispatch loop then skips its re-read of the arm at
    // every back edge on the promise that no arm happens
    // (`interpreter::execute_frame_from_index`'s `branch_arm_possible`).
    if verdict.recommended.is_some()
        && crate::jit::profile::c2_branch_window_enabled()
        && (shared.config.jit_enabled || !shared.config.interp_branch_arm_gate)
    {
        let _ = arm_method_branch_profile(&shared.jit.profile_store, cached);
    }
    // Round 15 wave 5 (lane sync4, SY3-1): see the callee.
    if verdict.recommended.is_some() && cached.is_static && cached.is_synchronized {
        mint_static_self_lock_slot_at_offer(shared, cached);
    }
    // nio20 wave 6, lane L3: the queued compile's `ldc <Class>` / `ldc
    // <String>` slots, minted on this mutator so the worker finds them.
    // nio21 wave 2 (L2-5): only when the mint did not already run above.
    if verdict.recommended.is_some() && !mint_before_enqueue {
        let _ = mint_ldc_slots_at_offer(shared, cached.declaring_class_id, &cached.code);
        mint_const_ref_folds_at_offer(shared, thread, cached.declaring_class_id, &cached.code);
    }
    verdict.recommended
}

/// Arm `cached`'s method for per-method branch recording: its lock-free
/// counters are created and armed in `store`, and the entry's
/// `branch_profile_armed` flag -- what the interpreter's conditional-branch
/// arms test -- is set. Returns whether this call did the arming (`false`:
/// the entry was already armed, and nothing was touched).
///
/// The code length passed is [`cached_method_code_len`], the same length
/// `interpreter::record_branch_for_frame` sizes the counters with, so the arm
/// and the recorder agree on the counters' shape. A different length is read
/// as another version of the method: `flat_branches_for` hands it detached
/// counters, so the arm would not reach the recorder's.
///
/// The flag is per ENTRY, and a method can have several
/// `CachedBytecodeMethod`s (one per call site that resolved it). Only the entry
/// the nomination came through is armed; the other entries of the same method
/// keep recording nothing unless the process-wide gate is on. That loses
/// observations, never correctness, and the nominating entry is the one whose
/// calls made the method hot.
pub(crate) fn arm_method_branch_profile(
    store: &crate::jit::profile::ProfileStore,
    cached: &cratonvm_jit_api::CachedBytecodeMethod,
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    if cached.branch_profile_armed.load(Relaxed) {
        return false;
    }
    let _ = store.arm_branch_counters_borrowed(
        cached.declaring_class_id.as_u32(),
        &cached.method_name,
        &cached.method_descriptor,
        cached_method_code_len(cached),
    );
    // Stored after the counters exist, so an interpreter thread that sees the
    // flag finds armed counters (it would create them itself otherwise; the
    // order only saves that thread the slow path).
    !cached.branch_profile_armed.swap(true, Relaxed)
}

/// The method's real bytecode length: `cached.code` carries the two
/// speculative-read padding bytes `frame::padded_bytecode` appends (the idiom
/// `callee_cached.code.len().saturating_sub(2)` elsewhere in this file).
///
/// The length every branch-counter door passes to the profile store. The
/// store stamps it on the method's slot, a profile save writes it, and a
/// replay compares it with the class file's `code_length` (round 11 wave 14):
/// the padded length stamped every fast-path record 2 bytes long, so a
/// replay dropped each one as stale, and a seeded method's own recorder read
/// as another version of it.
pub(crate) fn cached_method_code_len(cached: &cratonvm_jit_api::CachedBytecodeMethod) -> usize {
    cached.code.len().saturating_sub(2)
}

/// Calls of [`maybe_drain_jit_entry_counters`] between actual drains.
///
/// The drain walks every published body once, so it must not run per
/// invocation; equally it must not run so rarely that a method sits above the
/// C2 bar for seconds. 256 tier-up strides is the compromise, and the stride is
/// itself already a throttle: `offer_invocation_to_tiered_manager` is reached
/// once per `JIT_RETRY_STRIDE` (64) interpreted invocations past warmup, so this
/// is one cache walk per ~16 000 interpreted invocations of not-yet-settled
/// methods. Nothing depends on the exact number — it trades drain latency
/// against walk cost and both are far from any cliff.
pub(in crate::runtime::interpreter::jit_bridge) const ENTRY_COUNTER_DRAIN_STRIDE: u64 = 4096;

/// Drain the compiled-entry invocation counters, at most once per
/// [`ENTRY_COUNTER_DRAIN_STRIDE`] calls.
///
/// Costs one relaxed `fetch_add` and a compare when the counter is armed,
/// which is the default since round 9 wave 3 (`CRATONVM_JIT_ENTRY_COUNTER`
/// unset = ON, priced in `NOTES-w3-tiering3.md`); one memoised boolean when it
/// is disarmed with `CRATONVM_JIT_ENTRY_COUNTER=0` / `CRATONVM_JIT=-entry-counter`.
pub(in crate::runtime::interpreter) fn maybe_drain_jit_entry_counters(shared: &SharedVm) {
    if !crate::jit::entry_counter::entry_counter_enabled() {
        return;
    }
    // Per VM (`JitBridgeState::entry_counter_drain_ticks`), relaxed: an exact
    // modulus is not wanted here, only a bounded average rate, and two threads
    // racing to the same tick simply means one extra drain or one skipped one.
    // A process-wide tick let one VM's strides pace another VM's drains.
    let tick = shared
        .jit
        .bridge
        .entry_counter_drain_ticks
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if tick % ENTRY_COUNTER_DRAIN_STRIDE != 0 {
        return;
    }
    let bridge = &shared.jit.bridge;
    let now = bridge.drain_clock_ms().saturating_add(1);
    let last = bridge.last_entry_counter_drain_ms.load(std::sync::atomic::Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 25 {
        maybe_sweep_cold_jit_bodies(shared, tick);
        return;
    }
    drain_jit_entry_counters(shared);
    maybe_sweep_cold_jit_bodies(shared, tick);
}

/// TAIL11-2 (JIT round 19 wave 12, lane tail12): withdraw, once each, the
/// published bodies whose own single-pass guarded nested splice crossed the
/// TAIL9-20-2 miss-share bar (`JitCache::claim_sp_nested_stuck_bodies`), by
/// the ordinary deoptimization (`SpeculationFailed`: evict, demote, recompile
/// on the counted schedule). The recompile's nested resolver then refuses the
/// guarded row (`CRATONVM_JIT_SP_NESTED_MISS_SHARE_REFUSE`), so the outer call
/// reaches the enclosing callee's compiled body. `CRATONVM_DBG_JITC` names each.
pub(in crate::runtime::interpreter::jit_bridge) fn withdraw_stuck_nested_splice_bodies(
    shared: &SharedVm,
) {
    let stuck = shared.jit.jit_cache.read().claim_sp_nested_stuck_bodies();
    for (class_name, method_name, descriptor, class_id) in stuck {
        let action = crate::jit::helpers::DeoptimizationController::deoptimize_or_by_name(
            shared,
            class_id,
            &class_name,
            &method_name,
            &descriptor,
            cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
            u32::MAX,
        );
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] sp-nested-stuck-withdraw {class_name}.{method_name}{descriptor}: \
                 a guarded nested splice crossed the miss-share bar -> action={action:?}"
            );
        }
    }
}

/// JIT round 22 wave 11, lane runtime (reporting only; the
/// `code_cache_sweep_phases` failure message): this VM's windowed sweep drains,
/// `(opened, left the cap cleared, skipped because the entry chain was
/// unreadable)`.
pub fn code_cache_sweep_driver_census(shared: &SharedVm) -> (u64, u64, u64) {
    let c = &shared.jit.bridge.sweep_self_drains;
    let r = |i: usize| c[i].load(std::sync::atomic::Ordering::Relaxed);
    (r(0), r(1), r(2))
}

/// Retire this VM's cold compiled bodies when the process-wide code-cache cap
/// has been reached — RT-8.
///
/// Driven from here, and not from `jit_code_cache_at_capacity`, because the
/// decision is per-VM and that function is not: eviction needs a `JitCache`,
/// the bodies in it and this VM's own entry counters, none of which a
/// process-wide free function in the jit crate can reach. There is no registry
/// of live `JitCache`s and `AGENTS.md` forbids adding a process global to carry
/// per-VM state, so the driver sits on the side that already holds a
/// `SharedVm`.
///
/// This is also the one place in the VM that already runs on the right
/// schedule. It is downstream of the same `ENTRY_COUNTER_DRAIN_STRIDE` throttle
/// and of the same `entry_counter_enabled()` gate that
/// `JitCache::sweep_cold_bodies` itself requires — a sweeper without that
/// counter has no per-artifact usage signal and would find every body equally
/// cold. The counts it acts on are the ones `drain_jit_entry_counters` has just
/// carried back to the tiered manager, so a method the manager is about to
/// promote is one whose count moved, and is therefore not a candidate.
///
/// Costs one relaxed load of `COMMITTED_JIT_CODE_BYTES` per drain while the
/// sweeper is disarmed, which is the default.
///
/// `progress` is the drain's own tick, handed to the sweeper as the clock that
/// tells "no compiled body was entered" apart from "nothing ran" -- see
/// `JitCache::sweep_cold_bodies`.
///
/// Every call also reports the cap's pressure to the tiered manager, before
/// the early return, because the return is the moment that matters: pressure
/// having just CLEARED is what releases the methods the manager deferred at
/// the cap. See `TieredCompilationManager::note_code_cache_pressure`.
pub(in crate::runtime::interpreter::jit_bridge) fn maybe_sweep_cold_jit_bodies(
    shared: &SharedVm,
    progress: u64,
) {
    let at_cap = cratonvm_jit::jit_code_cache_under_pressure();
    shared.jit.tiered_manager.note_code_cache_pressure(at_cap);
    if !at_cap {
        return;
    }
    let report = shared.jit.jit_cache.read().sweep_cold_bodies(progress);

    // The half a sweeper is not allowed to skip. `current_tier` only ever
    // advances, and it advances at PUBLICATION; nothing lowers it when a body
    // is removed. A method swept while the manager still believes it is at C1
    // falls back to the interpreter and can never be offered again — its only
    // remaining door is `should_compile`'s C1 -> C2 arm, which asks ~20 000
    // invocations of `MethodState::invocation_count`, a counter with no writer
    // once a C1 body has been published. Without this loop the sweeper would
    // permanently deoptimise every method it touched, and nothing would count
    // it. See `TieredCompilationManager::note_body_withdrawn`.
    //
    // Taken outside the cache walk, per row, because it takes the manager's
    // `methods` mutex — the same two-phase shape and the same reason as
    // `drain_jit_entry_counters` above.
    //
    // Round 12 wave 4: an armed cache (`JitCache::install_withdrawal_sink`)
    // has already demoted every method that lost its method-entry body, and
    // only those; this loop would demote each a second time and would lower
    // an OSR-only withdrawal under a live method-entry body. It stays for an
    // unarmed cache (`CRATONVM_JIT_WITHDRAWAL_DEMOTES=0`).
    if !shared.jit.jit_cache.read().reports_withdrawals() {
        for (class_name, method_name, descriptor, class_id) in &report.withdrawn {
            shared.jit.tiered_manager.note_body_withdrawn(
                &crate::jit::tiered::MethodKey::with_class_id(
                    *class_id,
                    Arc::clone(class_name),
                    Arc::clone(method_name),
                    Arc::clone(descriptor),
                ),
            );
        }
    }

    // JIT round 22 wave 8, lane runtime (page
    // `r22-w7-code-cache-sweep-phases-bistable-FIXED-20261020.md`;
    // `CRATONVM_JIT_CODE_CACHE_SWEEP_SELF_BLOCKED`, default ON, `=0` the old
    // schedule): release what the sweeps withdrew FROM HERE, with this thread
    // vouching for itself through its own stack.
    //
    // The bistability: a withdrawn body goes back to the OS only when every
    // thread in compiled code has returned to JIT depth 0 since its withdrawal,
    // or is in a blocked window whose stack does not hold it
    // (`ThreadQuiescenceEvidence::permits`). This driver runs on a mutator at an
    // interpreter stride, and a phased program's driver loop is typically an
    // OSR body: the mutator is "running inside compiled code" for the whole
    // phase, so no drain it or anyone else made could release a byte until the
    // phase returned, and the phase's own methods, deferred at the cap, never
    // found room. Whether that loop had an OSR body yet (a background compile
    // racing the cap) decided the outcome: pass or fail with no configuration
    // change. Here the thread runs no compiled code, so it opens the same
    // blocked window a blocking native opens (its stack band up to the
    // outermost compiled entry is scanned, and only bodies that band can return
    // into are kept), drains, closes it, and reports the pressure again so the
    // methods deferred at the cap re-ask at once.
    if report.refused != Some(cratonvm_jit::JitCodeCacheSweepRefusal::Disabled)
        && cratonvm_jit::jit_retirement_queue_len() != 0
        && {
            let readable = crate::jit::conservative_roots::current_thread_jit_chain_is_readable();
            if !readable {
                shared.jit.bridge.sweep_self_drains[2]
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            readable
        }
    // The band's top is the outermost `JitEntryGuard`'s `entry_sp`, read
    // through `try_borrow`; an unreadable chain would yield an EMPTY band
    // for a thread still inside compiled code, which `permits` reads as
    // "returns into nothing". Never open the window on that.
    {
        // What the window means, to whom (wave-8 safety review): it only sets
        // this thread's JIT code-retirement record (`blocked_nesting`, the lazy
        // stack-band summary) in `cratonvm_jit`, which only
        // `drain_deferred_jit_owners` / `classify_thread_quiescence` and this
        // thread's own `jit_execution_enter` / exit read. No GC, safepoint or
        // handshake state is touched (`note_blocking_transition_enter` is a
        // stack-pointer read plus `jit_thread_blocked_enter`), so no collector
        // treats this thread as stopped. The band `[this SP, outermost
        // entry_sp)` holds every compiled frame of this thread, the OSR body
        // and any compiled -> interpreter -> compiled nesting included, since
        // each is entered below its own guard's `entry_sp`. No lock is held
        // here: the sweep's `sweep` / `mutation` locks and the `jit_cache`
        // read guards above are statement temporaries, released already.
        crate::jit::conservative_roots::note_blocking_transition_enter();
        let _ = cratonvm_jit::reclaim_retired_jit_code();
        crate::jit::conservative_roots::note_blocking_transition_leave();
        let still_at_cap = cratonvm_jit::jit_code_cache_under_pressure();
        shared
            .jit
            .tiered_manager
            .note_code_cache_pressure(still_at_cap);
        let census = &shared.jit.bridge.sweep_self_drains;
        census[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !still_at_cap {
            census[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    if report.swept != 0 && cratonvm_jit::metrics::print_compilation_enabled() {
        eprintln!(
            "[cratonvm-jit] code-cache sweep: withdrew {} of {} published bodies, \
             {} bytes ({} cold candidates, {} demoted to the interpreter). The bytes \
             are an upper bound: they return to the OS only once the retirement \
             queue proves no thread can re-enter them and no surviving caller \
             still pins them.",
            report.swept,
            report.observed,
            report.withdrawn_bytes,
            report.candidates,
            report.withdrawn.len()
        );
    }
}

/// Carry every compiled body's own entry count back to the tiered manager.
///
/// This is the second half of the compiled-entry invocation counter; the first
/// is two instructions in `x64::frames::emit_prologue`. See
/// `cratonvm_jit::entry_counter` for why it is needed at all — in one line,
/// `MethodState::invocation_count` has no writer once a C1 body is published,
/// so `should_compile`'s C1→C2 hotness arm is fed a count frozen at roughly the
/// C1 threshold and can never reach the 20 000 it asks for.
///
/// `observed_count` is the parameter this feeds, and it is the parameter that
/// was added for exactly this: it fast-forwards the manager's count to an
/// externally observed one and "degrades to the historical `+= 1` behaviour"
/// when the value is stale or smaller. That degradation is what makes this safe
/// to call unconditionally — a method whose compiled count is below what the
/// manager already knows is left exactly as it was.
///
/// ## What acts on the result
///
/// Nothing here, deliberately. `on_observed_invocations` reaches
/// `should_compile_inner`, which ENQUEUES the recommended task itself (through
/// `CompilerCore::admit`) before returning the tier; the returned tier is
/// informational for the interpreter's own logging and worker-start path. So a
/// method this drain pushes over the C2 bar is queued by the same code, at the
/// same priority, through the same idempotence gate, as one the interpreter
/// pushed over it — which is the property that keeps this from being a second
/// tiering policy.
///
/// ## A known imprecision, in the safe direction
///
/// (There used to be two. The other, a `+= 1` charged per drain through
/// `on_method_invocation_settling`, went with r9w2-vm2's switch to
/// `on_observed_invocations`. Since round 11 wave 2 a body is reported only
/// when its counter moved since the previous drain,
/// `CompiledMethod::entry_count_if_moved`.)
///
///  * The compiled counter starts at zero when the body is published, so the
///    method's interpreted history is not in it. The manager still holds that
///    history in its own (frozen) count and takes the maximum, so the union is
///    never worse than either half alone.
///
/// ## The clock, honestly
///
/// It runs off three clocks: the interpreter's tier-up stride, every
/// background compile, and (round 11 wave 4) an idle C1-lane compile worker
/// every 200 ms (`TieredCompilationManager::set_idle_tick`, installed by
/// `ensure_bg_compiler_started`). The first two stop in a state where NOTHING
/// is interpreted and NOTHING compiles, and that state is reachable: a
/// benchmark whose `main` loop runs in an OSR body and calls only compiled
/// methods reaches it once the last compile lands, and a method that crossed
/// the C2 bar after that was never promoted. The third clock is what covers
/// it, at 200 ms granularity; since round 11 wave 18 an OSR entry is a fourth,
/// gapped at 50 ms ([`maybe_drain_jit_entry_counters_at_osr_entry`]), and the
/// rows carry back edges as well as entries (`JitEntryCounter::BACK_EDGE_OFFSET`).
/// The prologue-side alternative (an inline threshold compare and a
/// one-shot helper call) is recorded in
/// `docs/feature-designs/real-jdk-mode.md`; since j27 wave 1 (lane TIER) it
/// exists opt-in as a fifth clock, `CRATONVM_JIT_ENTRY_TRIP`: a published C1
/// body armed at the C2 bar (`arm_published_entry_trip`) wakes an idle C1
/// worker's tick from its prologue when it counts the bar's entry.
pub(in crate::runtime::interpreter) fn drain_jit_entry_counters(shared: &SharedVm) {
    if !crate::jit::entry_counter::entry_counter_enabled() {
        return;
    }
    // Every clock stamps the drain, so the OSR-entry trigger
    // (`maybe_drain_jit_entry_counters_at_osr_entry`) only fires when all three
    // of the others have been quiet. `+ 1` keeps 0 meaning "never".
    shared.jit.bridge.last_entry_counter_drain_ms.store(
        shared.jit.bridge.drain_clock_ms().saturating_add(1),
        std::sync::atomic::Ordering::Relaxed,
    );
    // JIT round 19 wave 12, lane tail12 (TAIL11-2, opt-in
    // `CRATONVM_JIT_SP_NESTED_STUCK_WITHDRAW`): the drain's clocks are the
    // ones that keep running in a compiled steady state, which is where a
    // drifted guarded nested splice sits.
    if cratonvm_jit::sp_nested_stuck_withdraw_enabled() {
        withdraw_stuck_nested_splice_bodies(shared);
    }
    // Lock-free `arc_swap` walk; empty unless some compiled prologue has
    // actually run. See `JitCache::snapshot_entry_counts` for why the rows are
    // materialised before the manager's mutex is taken rather than during.
    let rows = shared.jit.jit_cache.read().snapshot_entry_counts();
    if rows.is_empty() {
        return;
    }
    // A recommendation is worth nothing without a worker to serve it, and this
    // door can be the first one reached on a `CRATONVM_BG_COMPILE` run whose
    // interpreted methods all settled early. One atomic load once the workers
    // are up.
    ensure_bg_compiler_started(shared);
    let manager = &shared.jit.tiered_manager;
    for (class_name, method_name, descriptor, class_id, count, back_edges) in rows {
        let key = crate::jit::tiered::MethodKey::with_class_id(
            class_id,
            class_name,
            method_name,
            descriptor,
        );
        // The verdict is dropped on purpose — see the doc comment: the enqueue
        // has already happened inside the manager by the time this returns, and
        // the settled stamp belongs to a `CachedBytecodeMethod` this walk does
        // not have and must not guess at.
        //
        // r9w2-vm2: `on_observed_invocations`, not `on_method_invocation_settling`
        // — a drain REPORTS invocations the compiled prologue already counted;
        // it is not itself an invocation, so it must not charge the `+1`.
        //
        // HANDLER8-2 (JIT round 19 wave 9): the row's raw back-edge word rides
        // along for the `CRATONVM_DBG_TIER_ENQUEUE` line's `drained-be=` field;
        // no decision reads it.
        let _ = manager.on_observed_invocations_with_back_edges(&key, count, back_edges);
    }
    // CONF21-1: a C2 supersede this drain queued (a no-op on the worker).
    resolve_admitted_compile_strings(shared);
}

/// OSR entries between two looks at the drain clock
/// ([`maybe_drain_jit_entry_counters_at_osr_entry`]). A `try`/`catch` loop
/// takes one OSR entry per caught exception (`osr_entered=2280923` on
/// `OsrExcRateProbe`), so even the clock read is strided. Small, because the
/// entries this clock exists for are rare (one per call of a method whose loop
/// OSRs), and an entry that follows a caught exception has already paid far
/// more than one `Instant::now`.
pub(in crate::runtime::interpreter::jit_bridge) const OSR_ENTRY_DRAIN_STRIDE: u64 = 4;

/// The OSR-entry trigger fires only when no drain ran for this long.
pub(in crate::runtime::interpreter::jit_bridge) const OSR_ENTRY_DRAIN_MIN_GAP_MS: u64 = 50;

/// The drain's fourth clock: an OSR entry (round 11 wave 18, P3 of
/// `r11-orch-callee-compiled-bodies-are-never-promoted`).
///
/// The other three stop in the state that page measured: nothing interpreted
/// (no tier-up stride), nothing compiling (no background compile), and an idle
/// C1 worker ticks only every 200 ms, so a short run's callee-compiled bodies
/// were never drained at all. An OSR entry is the one event such a run still
/// produces on the mutator, and it is where a hot loop is entered, so it is
/// the natural moment to carry the back-edge counts
/// (`JitEntryCounter::BACK_EDGE_OFFSET`) home.
///
/// Bounded twice: one relaxed `fetch_add` per OSR entry, a clock read every
/// [`OSR_ENTRY_DRAIN_STRIDE`] entries, and a drain only when the last one (by
/// any clock) is at least [`OSR_ENTRY_DRAIN_MIN_GAP_MS`] old. The kill switch
/// (`CRATONVM_JIT_CALLEE_PROMOTION=0`) is read only after both gates passed,
/// because it is not memoised.
pub(in crate::runtime::interpreter::jit_bridge) fn maybe_drain_jit_entry_counters_at_osr_entry(
    shared: &SharedVm,
) {
    use std::sync::atomic::Ordering::Relaxed;
    if !crate::jit::entry_counter::entry_counter_enabled() {
        return;
    }
    let bridge = &shared.jit.bridge;
    if bridge.osr_entry_drain_ticks.fetch_add(1, Relaxed) % OSR_ENTRY_DRAIN_STRIDE != 0 {
        return;
    }
    let now = bridge.drain_clock_ms().saturating_add(1);
    let last = bridge.last_entry_counter_drain_ms.load(Relaxed);
    if last != 0 && now.saturating_sub(last) < OSR_ENTRY_DRAIN_MIN_GAP_MS {
        return;
    }
    // Claim this drain, so two threads entering OSR bodies together do not
    // both walk the cache. The drain re-stamps with its own reading.
    if bridge
        .last_entry_counter_drain_ms
        .compare_exchange(last, now, Relaxed, Relaxed)
        .is_err()
    {
        return;
    }
    if !crate::jit::entry_counter::callee_promotion_enabled() {
        return;
    }
    drain_jit_entry_counters(shared);
}

/// Tell the tiered manager that a door it did not schedule just published a
/// body for `(class_id, class_name, method_name, descriptor)` (round 11 wave
/// 18, P1 of `r11-orch-callee-compiled-bodies-are-never-promoted`): the
/// optimizing doors' callee compiler, the eager callee chain and the
/// caller-held synchronized callee compile. `body` must be the artifact that
/// is published under that key, not merely one that was compiled; its backend
/// decides the tier (`TieredCompilationManager::note_published_outside_manager`).
/// Called with no VM lock held (it takes the manager's `methods` mutex).
pub(in crate::runtime::interpreter::jit_bridge) fn note_callee_publish_to_tiered_manager(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    body: &cratonvm_jit::CompiledMethod,
) {
    // j27 TIER (`CRATONVM_JIT_ENTRY_TRIP`): a callee-door C1 body reaches C2
    // through the same drain, so it gets its trip armed at the outside-manager
    // C2 drain bar (`OUTSIDE_C1_DRAIN_C2_BAR`).
    arm_outside_published_entry_trip(shared, body);
    let tier = if body.used_ir_backend {
        crate::jit::tiered::CompilationTier::C2
    } else {
        crate::jit::tiered::CompilationTier::C1
    };
    // HANDLER8-1 (JIT round 19 wave 9, lane handler9): with the census
    // (`CRATONVM_DBG_OUTSIDE_MANAGER_C2_CENSUS`) or the opt-in supersede
    // (`CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE`) armed, ask of a C1 publish the
    // question the manager path asks of its own (`c2_upgrade_candidate` in the
    // worker's compile closure): would the optimizing tier take this method?
    // Both off: `false`, and the manager call is exactly the old one.
    let upgrade = crate::jit::tiered::outside_manager_c2_upgrade_enabled();
    let c2_candidate = !body.used_ir_backend
        && (upgrade || crate::jit::tiered::outside_manager_c2_census_enabled())
        && outside_publish_would_take_c2(shared, class_id, class_name, method_name, descriptor);
    // Wave-9 follow-up: the supersede compiles only from a manager-deep profile.
    let profile_warm = c2_candidate
        && upgrade
        && outside_publish_profile_is_warm(shared, class_id, method_name, descriptor);
    // HANDLER11-6 (JIT round 19 wave 12, lane handler12; opt-in): and only
    // when the C1 body predicts that the C2 publish can forward its baked
    // callers (`outside_supersede_forward_predicted`).
    let forward_ok = !profile_warm
        || !outside_supersede_needs_forward_enabled()
        || outside_supersede_forward_predicted(body);
    if profile_warm && forward_ok {
        // The supersede is a background request: make sure a worker serves it
        // (this door can run before any tiered task started the workers).
        ensure_bg_compiler_started(shared);
    }
    shared
        .jit
        .tiered_manager
        .note_published_outside_manager_gated(
            &crate::jit::tiered::MethodKey::with_class_id(
                class_id,
                class_name,
                method_name,
                descriptor,
            ),
            tier,
            c2_candidate,
            profile_warm,
            forward_ok,
        );
}

/// `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_NEEDS_FORWARD` (HANDLER11-6, JIT
/// round 19 wave 12, lane handler12; **default OFF**, `1` enables; effective
/// only with `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE`): see
/// [`outside_supersede_forward_predicted`]. Read per warm candidate (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn outside_supersede_needs_forward_enabled() -> bool
{
    false
}

/// HANDLER11-6: does the outside-published C1 `body` predict that a C2 body
/// published over it could be forwarded to from its not-entrant stub (hm5,
/// `supersede_retire_superseded`), so that the callers which baked a direct
/// call to `body` -- the eager or sync-direct caller that made the method a
/// candidate -- would reach the C2 body at all? The terms of
/// `cratonvm_jit::supersede_forward_admissible` that the C1 body already
/// decides: a stamped argument count, no wrapped entry (a synchronized
/// method's C2 is wrapped too), no `invokedynamic` trap (likewise inherited by
/// a C2 of the same bytecode), not bound by a caller-held site (a C2 nobody
/// bound is refused by HANDLER11-5), and not already not entrant. HANDLER9-2's
/// census counts what this predicts (`c1-kept-entrant=` against
/// `c1-forwarded=`).
pub(in crate::runtime::interpreter::jit_bridge) fn outside_supersede_forward_predicted(
    body: &cratonvm_jit::CompiledMethod,
) -> bool {
    body.entry_java_args != u16::MAX
        && !body.requires_wrapped_entry
        && !body.has_indy_trap
        && !body.is_bound_by_a_caller_held_site()
        && !body.is_not_entrant()
}

/// Wave-9 follow-up of HANDLER8-1: may the opt-in supersede compile C2 from
/// this method's interpreter profile? Yes when the profile store counted at
/// least `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_MIN_PROFILE` invocations
/// (default `CRATONVM_JIT_THRESHOLD`, the depth a manager-scheduled C1 publish
/// has behind it, which is the only C1 publish `request_c2_upgrade` served
/// before this switch). An eager-chain callee can be published at 31 calls and
/// a sync-direct one at its first; the loaded `R16LocaleWide` stress crashed
/// 1/40 with the ungated supersede (a C2 callee's trap that no call-site
/// service claimed, and a corrupt reference handed back from compiled code),
/// 0/40 without it. Read only when the supersede is armed and the method is a
/// candidate (no memo, no static).
pub(in crate::runtime::interpreter::jit_bridge) fn outside_publish_profile_is_warm(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let floor = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(shared.config.jit_invocation_threshold);
    floor == 0
        || shared
            .jit
            .profile_store
            .invocation_count(cratonvm_jit_api::invoc_key_parts(
                class_id.as_u32(),
                method_name,
                descriptor,
            ))
            >= floor
}

/// HANDLER8-1: the manager path's C1 -> C2 supersede predicate
/// (`c2_supersede()` and `c2_upgrade_would_engage` over the method's bytecode,
/// the spelling of the worker's `c2_upgrade_candidate`), for a C1 body a door
/// outside the manager published. Takes the class manager's read lock briefly
/// (`fetch_osr_compile_inputs`); the caller holds no VM lock.
pub(in crate::runtime::interpreter::jit_bridge) fn outside_publish_would_take_c2(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    crate::runtime::env_cache::c2_supersede()
        && fetch_osr_compile_inputs(shared, class_id, class_name, method_name, descriptor)
            .map(|(_, padded, _)| {
                let code_len = padded.len().saturating_sub(2);
                cratonvm_jit::c2_upgrade_would_engage(
                    &padded,
                    code_len,
                    descriptor,
                    crate::runtime::env_cache::jit_ir_long(),
                    true,
                    crate::runtime::env_cache::jit_ir_call_virtual(),
                )
            })
            .unwrap_or(false)
}

/// What a C1→C2 supersede publish actually did to the cached body.
///
/// The distinction exists because only one of the three can invalidate an
/// invoke-cache entry, and the supersede epoch is a VM-wide counter that
/// every `Jit` entry in every thread of the VM is measured against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::runtime::interpreter) enum SupersedeOutcome {
    /// No body was in the cache under this key before the publish, so nothing
    /// was superseded. Reached by `promote_scalar_selfrec_to_ir`, which sends
    /// the narrow scalar self-recursion shape straight to the optimizing tier
    /// without a C1 body ever existing.
    FirstPublish,
    /// A body was replaced by one with identical code bytes — what a C2 task
    /// that fell back to the single-pass backend produces.
    Unchanged,
    /// A body was replaced by different code.
    Changed,
}

impl SupersedeOutcome {
    pub(in crate::runtime::interpreter) fn as_str(self) -> &'static str {
        match self {
            SupersedeOutcome::FirstPublish => "first-publish",
            SupersedeOutcome::Unchanged => "unchanged",
            SupersedeOutcome::Changed => "changed",
        }
    }
}

/// Classify a supersede from the artifact that was in the cache before the
/// publish and the one in it afterwards.
///
/// A missing *replacement* is reported as `Changed`, not as one of the two
/// cheap outcomes: the lookup failing is not evidence that nothing changed, and
/// this decides whether to skip an invalidation, so the unknown case must fail
/// towards the old unconditional behaviour.
pub(in crate::runtime::interpreter) fn classify_supersede(
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> SupersedeOutcome {
    let Some(before) = before else {
        return SupersedeOutcome::FirstPublish;
    };
    let Some(after) = after else {
        return SupersedeOutcome::Changed;
    };
    // Pointer equality first: `put` may have refused the publish (install-epoch
    // guard, code-cache cap), leaving the cache holding the very artifact that
    // was there before. That is an unchanged body by definition and skips the
    // byte compare entirely.
    if std::ptr::eq(before.as_ptr(), after.as_ptr()) && before.len() == after.len() {
        return SupersedeOutcome::Unchanged;
    }
    if before == after {
        SupersedeOutcome::Unchanged
    } else {
        SupersedeOutcome::Changed
    }
}

/// Engagement census for the three supersede outcomes.
///
/// A switch that suppresses work needs a count of what it suppressed, or a
/// "no regression" reading cannot be told apart from "never fired".
pub(in crate::runtime::interpreter::jit_bridge) static SUPERSEDE_FIRST_PUBLISH:
    std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::jit_bridge) static SUPERSEDE_UNCHANGED:
    std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::jit_bridge) static SUPERSEDE_CHANGED:
    std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Wall time spent in C2 tasks, split by whether the optimizing pipeline
/// produced the body or threw its work away and let single-pass do it.
///
/// This is the number that decides whether the fall-through is worth
/// preventing. The epoch bump it also pays was already measured at ~9
/// invoke-cache evictions per run, i.e. nothing; the COMPILE is the part that
/// could plausibly cost something, so it is timed rather than assumed.
pub(in crate::runtime::interpreter::jit_bridge) static C2_FELL_THROUGH_COUNT:
    std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::jit_bridge) static C2_FELL_THROUGH_US:
    std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::jit_bridge) static C2_LOWERED_COUNT:
    std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::jit_bridge) static C2_LOWERED_US: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(in crate::runtime::interpreter) fn note_c2_compile(fell_through: bool, micros: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let (n, t) = if fell_through {
        (&C2_FELL_THROUGH_COUNT, &C2_FELL_THROUGH_US)
    } else {
        (&C2_LOWERED_COUNT, &C2_LOWERED_US)
    };
    n.fetch_add(1, Relaxed);
    t.fetch_add(micros, Relaxed);
}

/// Published fell-through optimizing tasks whose build bailed on a row-less
/// field site of its own (`cratonvm_jit::IrFallThroughCause::RowlessFieldSite`);
/// the rest of `C2_FELL_THROUGH_COUNT` is unclassified. Interpreter round i6
/// wave 115, lane L2 (`i100-L2` item 3), printed by the `[c2-supersede]`
/// census under `CRATONVM_DBG_JITC`.
pub(in crate::runtime::interpreter::jit_bridge) static C2_FELL_THROUGH_ROWLESS_FIELD_COUNT:
    std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count a published optimizing task's fall-through cause (`None`: it did not
/// fall through). Next to [`note_c2_compile`], on the same publications.
pub(in crate::runtime::interpreter) fn note_c2_fall_through_cause(
    cause: Option<cratonvm_jit::IrFallThroughCause>,
) {
    if let Some(cratonvm_jit::IrFallThroughCause::RowlessFieldSite { .. }) = cause {
        C2_FELL_THROUGH_ROWLESS_FIELD_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How many of [`c2_compile_census`]'s fell-through compiles bailed on a
/// row-less field site of their own method.
pub fn c2_fall_through_rowless_field_count() -> u64 {
    C2_FELL_THROUGH_ROWLESS_FIELD_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// `(fell_through_count, fell_through_us, lowered_count, lowered_us)`.
pub fn c2_compile_census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        C2_FELL_THROUGH_COUNT.load(Relaxed),
        C2_FELL_THROUGH_US.load(Relaxed),
        C2_LOWERED_COUNT.load(Relaxed),
        C2_LOWERED_US.load(Relaxed),
    )
}

pub(in crate::runtime::interpreter) fn note_supersede_outcome(outcome: SupersedeOutcome) {
    let counter = match outcome {
        SupersedeOutcome::FirstPublish => &SUPERSEDE_FIRST_PUBLISH,
        SupersedeOutcome::Unchanged => &SUPERSEDE_UNCHANGED,
        SupersedeOutcome::Changed => &SUPERSEDE_CHANGED,
    };
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(first_publish, unchanged, changed)` supersede counts for this process.
pub fn supersede_census() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        SUPERSEDE_FIRST_PUBLISH.load(Relaxed),
        SUPERSEDE_UNCHANGED.load(Relaxed),
        SUPERSEDE_CHANGED.load(Relaxed),
    )
}

/// Does an optimizing publish have to advance the VM's supersede epoch?
///
/// The epoch exists so a `Jit` invoke-cache entry holding a REPLACED body
/// re-resolves; moving it evicts every `Jit` entry of every thread of the VM,
/// and the next call at each of those sites takes the slow path, which runs
/// its callee interpreted once (`try_stackless_invoke` never enters compiled
/// code). A first publish (`FirstPublish`: no body under the key when the
/// compile started) replaced nothing, so no entry can hold a body it made
/// stale, unless another body of the key was published in the compile's
/// window and this publish displaced it. That window is closed with the
/// name's publication stamp (`JitCache::callee_publish_stamp`), read before
/// the predecessor lookup and again after the publish: exactly one step is
/// this publish alone; anything else (another publication in the bucket, a
/// flush, a redefinition, or a publish that put twice) bumps, as does a
/// missing stamp. `Unchanged` and `Changed` always bump. A withdrawn
/// predecessor (deoptimized, then recompiled to a first publish here) needs
/// no epoch: its entries are pruned by `release_withdrawn_code_owners`.
pub(in crate::runtime::interpreter) fn supersede_epoch_bump_needed(
    outcome: SupersedeOutcome,
    stamp_before: Option<u64>,
    stamp_after: Option<u64>,
    only_on_replace: bool,
) -> bool {
    if !only_on_replace || outcome != SupersedeOutcome::FirstPublish {
        return true;
    }
    match (stamp_before, stamp_after) {
        (Some(before), Some(after)) => after.wrapping_sub(before) != 1,
        _ => true,
    }
}

/// Advance `shared`'s supersede epoch when [`supersede_epoch_bump_needed`]
/// says so, and report whether it did.
pub(in crate::runtime::interpreter) fn apply_supersede_epoch(
    shared: &SharedVm,
    outcome: SupersedeOutcome,
    stamp_before: Option<u64>,
    stamp_after: Option<u64>,
    only_on_replace: bool,
) -> bool {
    let bump = supersede_epoch_bump_needed(outcome, stamp_before, stamp_after, only_on_replace);
    if bump {
        // This VM's epoch: another VM's entries cannot hold this body.
        shared.jit.bump_supersede_epoch();
    }
    bump
}

/// wire-tiered-manager Step 5: resolve the inputs the off-thread OSR compile
/// needs for `(class, method, descriptor)` — `(class_id, padded bytecode,
/// max_locals)` — from already-loaded class metadata. The method is currently
/// executing in the interpreter (that is what tripped the back-edge), so its
/// class is loaded; we never load it here. Returns `None` if the
/// class/method/Code attribute is absent. The padded bytecode matches
/// `Frame::code`'s layout (`padded_bytecode`, +2 zero tail) so the compiled
/// artifact's PC mapping lines up with the interpreter frame at OSR entry.
pub(in crate::runtime::interpreter) fn fetch_osr_compile_inputs(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(ClassId, std::sync::Arc<[u8]>, u16)> {
    let cm = shared.classes.class_manager.read();
    // The task's own class identity first: resolving by name alone returns
    // `None` when two loaders define the name, and would otherwise compile
    // whichever copy the name lookup picked. The name is the fallback for a
    // key built without an id.
    let class_id = if class_id.as_u32() != 0
        && cm
            .get_class(class_id)
            .is_some_and(|class| class.name.as_ref() == class_name)
    {
        class_id
    } else {
        cm.get_loaded_class_id(class_name)?
    };
    let class = cm.get_class(class_id)?;
    let method = class
        .methods
        .iter()
        .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)?;
    let code_attr = method
        .attributes
        .iter()
        .find_map(|a| match a.as_decoded() {
            Some(cratonvm_reader::attribute::Attribute::Code(ca)) => Some(ca),
            _ => None,
        })?;
    // `padded_bytecode` only copies bytes (Rust-heap alloc, no VM lock / no
    // blocking / no managed allocation), so building it under the `cm` read
    // lock is GC-STW-safe; the lock drops at function return.
    let padded = crate::runtime::frame::padded_bytecode(&code_attr.code);
    let max_locals = code_attr.max_locals;
    Some((class_id, padded, max_locals))
}

/// Admit the narrow scalar self-recursion shape directly to the optimizing IR
/// backend on its first background compilation. Compiling it as C1 first is
/// counterproductive: once a recursive C1 frame is entered, its direct calls
/// remain in that slower body for the entire subtree even if C2 is published
/// concurrently. Every call target is resolved here and must be this exact
/// static `(I)I` method; all remaining structural checks live in the JIT crate.
pub(in crate::runtime::interpreter) fn promote_scalar_selfrec_to_ir(
    shared: &SharedVm,
    key: &crate::jit::tiered::MethodKey,
) -> bool {
    let Some((class_id, padded, _)) = fetch_osr_compile_inputs(
        shared,
        key.class_id,
        &key.class_name,
        &key.method_name,
        &key.descriptor,
    ) else {
        return false;
    };
    let code_len = padded.len().saturating_sub(2);
    if !cratonvm_jit::scalar_selfrec_ir_would_engage(&padded, code_len, &key.descriptor) {
        return false;
    }
    let Some(scan) = crate::jit::x64::jit_scan(&padded, code_len, &key.descriptor) else {
        return false;
    };
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    let is_static = class
        .methods
        .iter()
        .find(|m| &*m.name == &*key.method_name && &*m.descriptor == &*key.descriptor)
        .map(|m| m.is_static())
        .unwrap_or(false);
    if !is_static {
        return false;
    }
    scan.invoke_ops.iter().all(|(_, cp_idx, opcode)| {
        if *opcode != 0xb8 {
            return false;
        }
        let Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
            ..
        }) = class.constant_pool.get(*cp_idx)
        else {
            return false;
        };
        let Some(target_class) = class.constant_pool.get_class_name(*class_index) else {
            return false;
        };
        let Some((target_name, target_desc)) =
            class.constant_pool.get_name_and_type(*name_and_type_index)
        else {
            return false;
        };
        target_class == &*key.class_name
            && target_name == &*key.method_name
            && target_desc == &*key.descriptor
    })
}

/// STASH20-3 (`r21-duplicate-single-pass-compiles-of-a-c1-method-CLOSED-20261012.md`,
/// items 1-2): is this single-pass METHOD-ENTRY background task a duplicate of
/// the body already live under its key, so the worker may answer it with that
/// body instead of compiling and republishing?
///
/// The duplicates: a C1 request the manager queued while the method was at
/// `Interpreter` in its books is served after an eager-chain or callee door
/// published the method outside the manager
/// (`note_published_outside_manager` raises the tier but withdraws no queued
/// request), and the background door never probes the cache
/// (`try_jit_compile_wrapped_entry` compiles unconditionally), so it compiled
/// and published a second single-pass body (`HashMap.newNode` 11 times,
/// `TreeMap.put` 3 times on one `tmapcmp` run), displacing the first: one more
/// compile, one more code-cache body, and since wave 16 a not-entrant redirect
/// whose stub every caller of the first body then paid.
///
/// The page's objection to skipping is kept: a body compiled before its own
/// callees were published left their statically bound calls on the dispatch
/// helper, and a later compile binds them (`fall_through_resolves_more_calls`'
/// premise). So the task is a duplicate only when the live body is a
/// single-pass, method-entry body, neither withdrawn nor not entrant nor
/// wrapped, with NO statically bound call site left on the dispatch helper (no
/// `invokestatic` / `invokespecial` invoke record, `invoke_kind` 3 or 1): every
/// such site it has is a baked direct call or a splice already, and a
/// recompile of the same bytecode by the same backend binds nothing more.
/// Virtual and interface sites are served by the inline caches either way.
///
/// Answered as published: the manager records C1 (`finish` reads the live
/// body) and the caller's tail still asks the C1 -> C2 upgrade, exactly as
/// after a publish. `CRATONVM_DBG_JITC`: `bg-compile C1 DEDUPED` (skipped) or
/// `bg-compile C1 duplicate KEPT` (a live body with unbound static sites).
pub(in crate::runtime::interpreter::jit_bridge) fn c1_task_duplicates_live_body(
    shared: &SharedVm,
    task: &crate::jit::tiered::CompilationTask,
) -> bool {
    if task.osr_bci.is_some() {
        return false;
    }
    // The class id is resolved before the cache is read-locked, never under
    // it (the `superseded_body` lock order in `background_compile_task`).
    let Some((class_id, _, _)) = fetch_osr_compile_inputs(
        shared,
        task.method_key.class_id,
        &task.method_key.class_name,
        &task.method_key.method_name,
        &task.method_key.descriptor,
    ) else {
        return false;
    };
    let live = {
        let jit_cache = shared.jit.jit_cache.read();
        jit_cache
            .get(
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
                class_id,
            )
            // A later publish may withdraw it while it is held here.
            .map(cratonvm_jit::RetainedCode::new)
    };
    let Some(live) = live else {
        return false;
    };
    let Some(unbound_static) = c1_duplicate_unbound_statics(&live) else {
        return false;
    };
    if crate::runtime::env_cache::dbg_jitc() {
        if unbound_static == 0 {
            eprintln!(
                "[cratonvm-jitc] bg-compile C1 DEDUPED {}.{}{}: a single-pass body with no \
                 statically bound call on the dispatch helper is live (entry={:#x})",
                task.method_key.class_name,
                task.method_key.method_name,
                task.method_key.descriptor,
                // Cast: an address, for the trace line.
                live.entry_ptr() as usize,
            );
        } else {
            eprintln!(
                "[cratonvm-jitc] bg-compile C1 duplicate KEPT {}.{}{}: the live single-pass body \
                 has {unbound_static} statically bound call(s) on the dispatch helper",
                task.method_key.class_name, task.method_key.method_name, task.method_key.descriptor,
            );
        }
    }
    unbound_static == 0
}

/// STASH20-3: for a live body a single-pass task could duplicate, how many of
/// its statically bound call sites (`invoke_kind` 1 or 3) still go through the
/// dispatch helper; `None` when the body is not one a single-pass recompile
/// would merely repeat (an optimizing, OSR or wrapped body, or one withdrawn,
/// superseded or not entrant). [`c1_task_duplicates_live_body`] skips the task
/// only on `Some(0)`.
pub(in crate::runtime::interpreter::jit_bridge) fn c1_duplicate_unbound_statics(
    live: &cratonvm_jit::CompiledMethod,
) -> Option<usize> {
    use std::sync::atomic::Ordering;
    if live.used_ir_backend
        || live.compiled_via_osr
        || live.requires_wrapped_entry
        || live.retired.load(Ordering::Acquire)
        || live.superseded.load(Ordering::Acquire)
        || live.is_not_entrant()
    {
        return None;
    }
    Some(
        live._jit_invoke_infos
            .iter()
            .filter(|info| matches!(info.invoke_kind, 1 | 3))
            .count(),
    )
}

/// zt pl4 item 1 (`sun.ci.standardBytes` / `osrBytes` / `lastSize` /
/// `nmethodSize` / `nmethodCodeSize`): report to the tier manager the sizes
/// of the body `task` just published -- the method's bytecode bytes plus the
/// callees its body inlined (`InlineDecisionTally::inlined_bytecodes`,
/// HotSpot's `code_size() + num_inlined_bytecodes()`), and the body's
/// code-cache and instruction bytes -- read back from the cache. A body the
/// cache no longer holds (withdrawn already, or an optimizing OSR body kept
/// in its memo) reports nothing; its compile still counts.
pub(in crate::runtime::interpreter::jit_bridge) fn note_background_published_sizes(
    shared: &SharedVm,
    task: &crate::jit::tiered::CompilationTask,
) {
    let key = &task.method_key;
    let Some((class_id, bytecode, _)) = fetch_osr_compile_inputs(
        shared,
        key.class_id,
        &key.class_name,
        &key.method_name,
        &key.descriptor,
    ) else {
        return;
    };
    let body = {
        let cache = shared.jit.jit_cache.read();
        if task.osr_bci.is_some() {
            cache.get_osr(&key.class_name, &key.method_name, &key.descriptor, class_id)
        } else {
            cache.get_for_reentry(&key.class_name, &key.method_name, &key.descriptor, class_id)
        }
    };
    let Some(body) = body else {
        return;
    };
    if task.osr_bci.is_none() {
        arm_published_entry_trip(shared, &body);
    }
    note_published_body_sizes(bytecode.len(), &body);
}

/// j27 wave 1, lane TIER (`CRATONVM_JIT_ENTRY_TRIP`, opt-in; page
/// `jr21-conf-compiled-bodies-reach-c2-only-at-a-drain-clock-FIXED-20261029.md`):
/// arm a just-published single-pass METHOD-ENTRY body's entry trip at the C2
/// hotness bar, so the prologue that counts the bar's entry wakes the drain
/// (`drain_jit_entry_counters`, the idle tick) at once. The drain then queues
/// the C2 request through the same `on_observed_invocations` door every other
/// clock uses, so this adds a moment, not a policy.
///
/// The compiled counter starts at zero at the publish and the manager takes
/// the MAXIMUM of its own count and the drained one, so the drained count
/// crosses the bar exactly when the compiled entries reach it: the bar itself
/// is the trip point. Optimizing bodies carry no counter (or only the
/// sweeper's) and have nowhere to be promoted, so they are not armed; nor is
/// anything while tiering is disabled (`u64::MAX`). Bodies published by the
/// other doors (callee compiles, synchronous tier-up) stay on the drain
/// clocks.
pub(in crate::runtime::interpreter::jit_bridge) fn arm_published_entry_trip(
    shared: &SharedVm,
    body: &cratonvm_jit::CompiledMethod,
) {
    let bar = shared.jit.tiered_manager.c2_drain_trip_bar();
    arm_published_entry_trip_at(shared, body, bar);
}

/// Arm an outside-manager C1 body's entry trip at
/// `OUTSIDE_C1_DRAIN_C2_BAR.min(manager.c2_hotness_bar())` so hot dispatchers
/// published via the callee/eager doors trigger drain -> C2 promotion within
/// the outside-C1 drain bar.
pub(in crate::runtime::interpreter::jit_bridge) fn arm_outside_published_entry_trip(
    shared: &SharedVm,
    body: &cratonvm_jit::CompiledMethod,
) {
    let manager_bar = shared.jit.tiered_manager.c2_hotness_bar();
    let bar = if manager_bar != u64::MAX && crate::jit::tiered::outside_c1_drain_c2_enabled() {
        crate::jit::tiered::OUTSIDE_C1_DRAIN_C2_BAR.min(manager_bar)
    } else {
        manager_bar
    };
    arm_published_entry_trip_at(shared, body, bar);
}

fn arm_published_entry_trip_at(shared: &SharedVm, body: &cratonvm_jit::CompiledMethod, bar: u64) {
    if body.used_ir_backend || !crate::jit::entry_counter::entry_trip_enabled() || bar == u64::MAX {
        return;
    }
    let Some(counter) = body._jit_entry_counter.as_deref() else {
        return;
    };
    counter.arm_trip(bar, &shared.jit.tiered_manager.entry_trip_sink());
}

/// zt pl4 item 1: [`note_background_published_sizes`]'s report for `body`, a
/// method of `bytecode_len` bytecode bytes; a report already made by this
/// task (the optimizing OSR route reports the body it kept in its memo) is
/// not replaced.
pub(in crate::runtime::interpreter::jit_bridge) fn note_published_body_sizes(
    bytecode_len: usize,
    body: &cratonvm_jit::CompiledMethod,
) {
    // Widening: usize / u32 -> u64.
    crate::jit::tiered::note_published_sizes_once(crate::jit::tiered::PublishedSizes {
        bytecode_bytes: bytecode_len as u64 + u64::from(body.inline_tally.inlined_bytecodes),
        total_size: body.code_capacity() as u64,
        insts_size: body.code_len() as u64,
    });
}

pub(in crate::runtime::interpreter) fn background_compile_task(
    weak_vm: &std::sync::Weak<SharedVm>,
    task: &crate::jit::tiered::CompilationTask,
) -> crate::jit::tiered::CompileOutcome {
    use crate::jit::tiered::CompileOutcome;
    // A compile that RAN and failed — spends one of the method's retries.
    let fail = CompileOutcome::failed;
    // The method was refused on grounds that cannot change while this process
    // lives (the skip list and the OSR-denial set are pure functions of the
    // loaded method), so the tier manager records the verdict once instead of
    // burning the retry budget re-asking. Keeping these apart is what makes
    // `tier_fail_count` mean "codegen is broken" rather than "banned by
    // policy" — see `MethodState::ineligible`.
    let declined = CompileOutcome::declined;
    let shared = match weak_vm.upgrade() {
        Some(s) => s,
        // VM dropped (teardown) — nothing to compile, and nothing to learn
        // about this method. Charge it to neither counter.
        None => return fail(0),
    };
    // The compiled-entry counter's second clock. The interpreter's tier-up
    // stride is the first, and it stops ticking for a method once that method
    // settles — but every background compile is also evidence that the process
    // is still warming up, and it arrives on a worker thread that holds no VM
    // lock and is about to spend milliseconds in codegen. A full drain here is
    // free by comparison, and it is the tick that keeps working when the last
    // interpreted method has settled but compiles are still landing.
    //
    // Unstrided on purpose: compiles are rare events by construction (each one
    // costs milliseconds), so there is nothing here to throttle. Returns before
    // any lock unless `CRATONVM_JIT_ENTRY_COUNTER` is armed.
    {
        let now = shared.jit.bridge.drain_clock_ms().saturating_add(1);
        let last = shared
            .jit
            .bridge
            .last_entry_counter_drain_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        if last == 0 || now.saturating_sub(last) >= 25 {
            drain_jit_entry_counters(&shared);
        }
    }
    // A redefined class is compiled here at METHOD ENTRY like any other
    // (interpreter round i1 wave 38, lane L2; item 5 of the wave-37
    // compile-door review). This task used to decline one for good
    // (`class_id_or_name_was_redefined`, which stays true once a class has
    // been redefined, and `declined` marks the method `ineligible`), while the
    // tiered manager's `on_class_redefined` reset exactly those verdicts so
    // the method could re-tier. A method-entry body is compiled from the
    // class's CURRENT bytecode (`try_jit_compile_wrapped_entry`) and entered
    // only by a new call, which runs that bytecode; a redefinition racing the
    // compile makes it unpublishable (the compile-epoch witness).
    //
    // An OSR task of a redefined class is compiled too (interpreter round i1
    // wave 40, lane L2). Waves 38-39 declined it: the wave-38 host run of the
    // first `L2W38RedefinedClassCompiles` (an `int` iteration count asked
    // `n > 0`) printed `old constant=false` once with such bodies published.
    // What makes admitting them safe is at the entry, not here: `try_osr` is
    // the only function that enters an OSR body, it is reached only behind
    // the offer's `obsolete_frames::frame_runs_replaced_code`, and it asks
    // `osr_body_generation_differs_from_frame` on the body every route
    // produced, which refuses a frame running an obsolete method and any
    // frame/body pair whose constant-pool stamps straddle a redefinition of
    // the class. So an activation enters only a body that reads the pool
    // generation its own interpreted code reads (a frame the conversion
    // restamps as current either runs the current body or an EMCP one whose
    // translated bytes -- constant-pool indices included -- are the current
    // body's). The long-count probe matched HotSpot 30/30 on the host with
    // these bodies published, on wave 39 and on `dev` 038a7f265 (before the
    // entry guard), and `L2W40ObsoleteOsrAcrossConstantToggles` keeps four
    // generations of one loop apart. The record:
    // `docs/feature-designs/deopt-osr.md`.
    //
    // Lever, read per task and only for a redefined class (a task is
    // milliseconds of codegen): `CRATONVM_JIT_BG_DECLINE_REDEFINED=1`
    // declines every task, as before wave 38. (The wave-38 opt-in that
    // admitted only the OSR tasks is retired with the decline it lifted.)
    let redefined = crate::runtime::redefine_state::class_id_or_name_was_redefined(
        &shared,
        task.method_key.class_id.as_u32(),
        &task.method_key.class_name,
    );
    if redefined && false {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] bg-compile declined {}.{}{}{}: redefined class",
                task.method_key.class_name,
                task.method_key.method_name,
                task.method_key.descriptor,
                task.osr_bci
                    .map(|b| format!(" osr_bci={b}"))
                    .unwrap_or_default(),
            );
        }
        return declined(0);
    }
    // Keep asynchronous tiering aligned with the foreground admission paths.
    // Without this gate, methods rejected by the conservative skip list are
    // repeatedly queued by the tier manager. The final compiler gate then
    // declines each task, but the hot interpreter path keeps paying for the
    // failed background attempts. Hibernate's package-level fail-closed
    // policy made that retry loop large enough to turn ordinary suite classes
    // into timeout candidates.
    // The static JIT ban list was deleted 2026-07-31 (see
    // docs/feature-designs/native-dispatch-architecture.md).
    // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
    // remaining force-interpret lever, applied in `jit::try_compile`.
    if task.osr_bci.is_some() && shared.jit.tiered_manager.is_osr_denied(&task.method_key) {
        return declined(0);
    }
    let optimized = crate::jit::tiered::tier_uses_optimized_backend(task.target_tier)
        || (task.osr_bci.is_none() && promote_scalar_selfrec_to_ir(&shared, &task.method_key));
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] bg-compile {}.{}{} tier={:?} optimized={}{}{}",
            task.method_key.class_name,
            task.method_key.method_name,
            task.method_key.descriptor,
            task.target_tier,
            optimized,
            task.osr_bci
                .map(|b| format!(" osr_bci={b}"))
                .unwrap_or_default(),
            // Wave 38 (lane L2): the positive control of the redefined-class
            // admission above.
            if redefined { " redefined-class" } else { "" },
        );
    }
    // wire-tiered-manager Step 5 (precise background OSR): an OSR-motivated task
    // compiles an OSR-enterable artifact OFF the mutator (the worker has no live
    // frame). `compile_osr_artifact` resolves all metadata from the (loaded)
    // class and publishes a `compiled_via_osr` body into `jit_cache`; the
    // mutator's back-edge path then ENTERS that published artifact (the existing
    // `osr_reused` reuse path) with no inline compile stall. `osr_bci` is the
    // back-edge the mutator will enter at — the artifact supports entry at every
    // loop header it emits, so the compile itself is entry-pc-independent.
    if let Some(osr_bci) = task.osr_bci {
        let start = std::time::Instant::now();
        // Tier proposal 26 (round 11 wave 9): the OPTIMIZING OSR body first,
        // the order `try_osr` has always tried the two routes in. Before this,
        // the route was asked only by `try_osr`, which the mutator reaches
        // under background compilation only once THIS task has published an
        // enterable single-pass body — so a refused single-pass compile, or a
        // body with no entry at `osr_bci`, meant the optimizing route was never
        // asked (r11w8-tier-optimizing-osr-route-unreachable-behind-single-pass).
        // An accepted body is kept in the per-VM accepted-artifact memo, which
        // the mutator's background arm now reads (`osr_optimizing_body_ready`),
        // so the single-pass compile is skipped for this pc exactly as
        // `try_osr` skips it. A refusal falls through to the single-pass
        // compile unchanged, and is memoed, so `try_osr` does not rebuild the
        // body on the mutator once the single-pass artifact is published.
        // `CRATONVM_JIT_OSR_OPTIMIZING_BG=0` restores the old routing.
        let mut cached = optimized
            && background_osr_optimizing_body(&shared, &task.method_key, osr_bci as usize);
        // A late-bind wake that arrived while the body was being built found
        // this task holding the OSR slot (`OsrRequest::Pending`) and left only
        // its forget mark. The door takes a mark at its next lookup, and a
        // frame that never leaves the old body never causes one, so ask again
        // here: the rebuild sees the site resolved, and its remembering arms
        // the old body's exit. Each mark is one fired waiter; capped anyway.
        for _ in 0..2 {
            if !cached
                || !late_bind_osr_optimizing_stale_pending(
                    &shared,
                    osr_optimizing_refusal_key(
                        shared.vm_identity,
                        task.method_key.class_id,
                        &task.method_key.method_name,
                        &task.method_key.descriptor,
                        osr_bci as usize,
                    ),
                )
            {
                break;
            }
            cached = background_osr_optimizing_body(&shared, &task.method_key, osr_bci as usize);
        }
        if cached {
            cover_headers_for_superseded_osr_exits(&shared, &task.method_key, osr_bci as usize);
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] OSR-compile (bg) {}.{}{} osr_bci={}: optimizing body cached, single-pass compile skipped",
                    task.method_key.class_name,
                    task.method_key.method_name,
                    task.method_key.descriptor,
                    osr_bci,
                );
            }
            return CompileOutcome {
                // Widening: smaller integer -> 64-bit (zero/sign-extended).
                compile_time_ms: start.elapsed().as_millis() as u64,
                published: true,
                // As the single-pass arm below: OSR artifacts serve loop entry
                // and never seed a C2 upgrade.
                c2_upgrade_candidate: false,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            };
        }
        // The class the compile's verdicts are recorded under: the key's id,
        // or the one `fetch_osr_compile_inputs` resolves when the key has none.
        let mut verdict_class_id = task.method_key.class_id;
        // Wave 42 (lane L2): the compile's epoch witness opens before the
        // bytecode is fetched, as `try_jit_compile_callee_slow`'s does (see
        // `bytecode_read_witness` there): `fetch_osr_compile_inputs` copies
        // it under a guard it drops before `compile_osr_body` reaches
        // `compile_gate::admit`, so a redefinition landing in between gave a
        // body of the old bytecode the new epochs and constant-pool stamp.
        // `admit`'s witness nests in this one. Since wave 43 the witness and
        // the fetched bytecode are one `CompileInputs`, which
        // `compile_osr_body` requires.
        let published = if let Some(inputs) = CompileInputs::read(|| {
            fetch_osr_compile_inputs(
                &shared,
                task.method_key.class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            )
            .map(|(class_id, code, max_locals)| OsrBytecode {
                class_id,
                code,
                max_locals: usize::from(max_locals),
            })
        }) {
            let class_id = inputs.get().class_id;
            verdict_class_id = class_id;
            compile_osr_artifact(
                &shared,
                class_id,
                task.method_key.class_name.to_string(),
                task.method_key.method_name.to_string(),
                task.method_key.descriptor.to_string(),
                OsrCodeSource::Read(&inputs),
                osr_bci as usize,
            )
            .is_some()
        } else {
            false
        };
        if !published {
            // Which failures deny OSR, and which are retried.
            //
            // This used to deny OSR for the method on ANY unpublished result,
            // for the rest of the process. Most ways a background OSR compile
            // comes back empty are transient: the class manager could not hand
            // over the inputs this time (`fetch_osr_compile_inputs` returned
            // `None`), the code cache was full, a redefinition landed
            // mid-compile, a callee had not loaded yet. Denying on those turned
            // one unlucky compile into a method whose loops stayed interpreted
            // forever (a once-invoked harness main with the hot loop inline ran
            // ~8x slow with zero other diagnostics, perf/halfgap-20260717).
            //
            // So a failure is a failed compile -- it spends one of the method's
            // `MAX_TIER_FAIL_RETRIES` and the next hot back-edge asks again,
            // which also bounds the re-enqueue loop the old denial existed to
            // stop -- unless the compiler recorded a verdict the loaded bytecode
            // cannot change by bail-listing the method. Only then is OSR denied,
            // and that denial still expires when the install epoch moves.
            // ...or unless the door itself denied OSR for the method on a
            // standing verdict (`compile_osr_artifact`'s lever, `synchronized`
            // and registered-native gates, the unsupported-constant and RBC.6
            // refusals in `compile_osr_body`): that is a policy decline, not a
            // failed compile, and must not spend a retry.
            let permanent = jit_verdicts(&shared).is_bail_listed(
                verdict_class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            ) || shared.jit.tiered_manager.is_osr_denied(
                &crate::jit::tiered::MethodKey::with_class_id(
                    verdict_class_id,
                    Arc::clone(&task.method_key.class_name),
                    Arc::clone(&task.method_key.method_name),
                    Arc::clone(&task.method_key.descriptor),
                ),
            );
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] OSR-compile FAILED {}.{}{} osr_bci={} stage={} — {}",
                    task.method_key.class_name,
                    task.method_key.method_name,
                    task.method_key.descriptor,
                    osr_bci,
                    osr_stage_get(),
                    if permanent {
                        "bail-listed or OSR-denied: OSR denied until the install epoch moves"
                    } else {
                        "transient: counted as a failed compile and retried"
                    },
                );
                eprintln!(
                    "[cratonvm-jitc]   …and the bail this method last recorded: {}",
                    jit_verdicts(&shared)
                        .bail_reason_for(
                            verdict_class_id,
                            &task.method_key.class_name,
                            &task.method_key.method_name,
                            &task.method_key.descriptor,
                        )
                        .unwrap_or_else(|| "none recorded".to_string()),
                );
                // Since round 11 wave 9 this task asks the optimizing route
                // itself, first (`background_osr_optimizing_body`), and its
                // verdict is the `osr optimizing (bg)` line above this one.
                // Under `CRATONVM_JIT_OSR_OPTIMIZING_BG=0` it does not: the
                // route then lives only in `try_osr`, which the mutator calls
                // once THIS single-pass artifact is published and enterable,
                // so a single-pass refusal here is the whole answer
                // (r11w7-orch-osr-refuses-every-synchronized-block-loop).
                {
                    eprintln!(
                        "[cratonvm-jitc]   …the optimizing OSR route was asked first by this task \
                         and produced no enterable body (see its `osr optimizing (bg)` line)"
                    );
                }
            }
            if permanent {
                shared
                    .jit
                    .tiered_manager
                    .mark_osr_denied(task.method_key.clone());
                return declined(start.elapsed().as_millis() as u64);
            }
        }
        return CompileOutcome {
            // Widening: smaller integer -> 64-bit (zero/sign-extended).
            compile_time_ms: start.elapsed().as_millis() as u64,
            published,
            // OSR artifacts serve loop entry; the invocation path re-tiers
            // separately, so an OSR task never seeds a C2 upgrade.
            c2_upgrade_candidate: false,
            deferred_new_retry: false,
            // A codegen attempt actually ran here; a non-publish is a real
            // failure, not a policy verdict.
            declined_permanently: false,
            optimizing_publication_refused: false,
        };
    }
    let start = std::time::Instant::now();
    // The body about to be REPLACED, captured before the publish overwrites it.
    //
    // Nothing compares a C2 body against the C1 body it supersedes before
    // keeping it, and the open policy question that follows from that
    // (`perf-01-sieve-ir-body-slower-than-c1`) is deliberately not answered
    // here: the metrics a static comparison could use are all proxies, and the
    // two that look obvious both misjudge the good cases — a bigger body is
    // usually inlining or unrolling, and MORE call sites can be a callee's
    // calls after its frame was inlined away. What is missing is not a rule
    // but DATA, so this records the replacement instead of guessing at it.
    //
    // The capture is the whole artifact, not its length, and it is NOT gated on
    // the diagnostic flag any more: the epoch bump below is now conditional on
    // what this finds, so a debug-only capture would make the diagnostic change
    // the behaviour it reports. `JitCache::get` returns an `Arc` clone, so
    // holding it across the publish costs a refcount and keeps the superseded
    // artifact alive against `defer_jit_owner`'s drop.
    //
    // Held as `RetainedCode`, NOT a bare `Arc`. That keep-alive is exactly what
    // made this local the body's LAST owner: the publish below withdraws the
    // C1 body into the retirement queue, the queue's drop is then not the last
    // one, and this local used to unmap a published body at the end of the
    // function with no quiescence proof — ~27 times per Spring test process,
    // and once under a thread still executing it
    // (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`).
    // `RetainedCode`'s drop routes a last reference through the queue.
    //
    // JIT r23 wave 11, lane L2 (`CRATONVM_JIT_SUPERSEDE_EPOCH_ONLY_ON_REPLACE`):
    // the name's publication stamp, read BEFORE the predecessor lookup, so a
    // body of this key published between that lookup and this task's own
    // publish moves it (`supersede_epoch_bump_needed`).
    let publish_stamp_before = optimized.then(|| {
        shared
            .jit
            .jit_cache
            .read()
            .callee_publish_stamp(&task.method_key.method_name, &task.method_key.descriptor)
    });
    let superseded_body: Option<cratonvm_jit::RetainedCode> = optimized
        .then(|| {
            let (class_id, _, _) = fetch_osr_compile_inputs(
                &shared,
                task.method_key.class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            )?;
            let jit_cache = shared.jit.jit_cache.read();
            jit_cache.get(
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
                class_id,
            )
        })
        .flatten()
        .map(cratonvm_jit::RetainedCode::new);
    // Real codegen + publish into the shared JIT cache. `try_jit_compile_callee`
    // is the by-name entry point shared with the JIT dispatch helpers; it stores
    // the compiled body under `(class, method, descriptor)` so the mutator's
    // `jit_cache` fast-path flips the call site to `Jit` on its next call.
    // `is_some()` reports whether it actually published one — a `None` (skip-
    // listed, resolver miss, code-cache cap, concurrent redefine, ...) must
    // NOT be reported as success, or the tiered manager marks this tier
    // "done" despite nothing having been compiled (see
    // `jit::tiered::CompilerCore::complete_task`).
    let mut policy_declined = false;
    take_refused_optimizing_publication(None);
    cratonvm_jit::ir_evidence::take_supersede_abandoned();
    // JIT round 20 wave 19, lane yStash (STASH20-3,
    // `CRATONVM_JIT_C1_DUPLICATE_SKIP`): a single-pass task whose method
    // already has a single-pass body that a recompile could not improve is
    // answered by that body (reported published, so the manager records C1
    // and the tail below still asks the C2 upgrade).
    let deduped = !optimized && c1_task_duplicates_live_body(&shared, task);
    let published = deduped
        || try_jit_compile_wrapped_entry(
            &shared,
            // Loader-aware: without the task's own class identity, every method of
            // a class name two loaders define was refused here (and counted as a
            // failed compile) because the name alone resolves to no unique class.
            task.method_key.class_id,
            &task.method_key.class_name,
            &task.method_key.method_name,
            &task.method_key.descriptor,
            // wire-tiered-manager Step 3: C1 (no-opt single-pass) vs C2 (optimizing
            // pipeline), selected by the task's target tier. This is the real
            // backend routing that replaces the former advisory-only hint.
            optimized,
            &mut policy_declined,
        )
        .is_some();
    // A C2 body `put` refused while the C1 body stayed live is not a publish:
    // reported as one, it recorded C2 and the method was never offered C2 again.
    // The worker re-asks C2 for it (`optimizing_publication_refused`), charged
    // to the tier-failure budget like any unpublished compile.
    let optimizing_publication_refused = optimized
        && take_refused_optimizing_publication(Some((
            &task.method_key.class_name,
            &task.method_key.method_name,
            &task.method_key.descriptor,
        )));
    // A supersede the acceptance gate refused publishes nothing on purpose
    // (`supersede ABANDONED`): the baseline body stays and is the method's
    // answer, as `c1_task_duplicates_live_body`'s is for a C1 task. Reported
    // as unpublished it spent one of the method's three `tier_fail_count`
    // retries, and the third left the method `compile-failed` for good: when
    // a callee's deopt later withdrew that baseline with its raw callers, the
    // method could not be compiled again and ran interpreted for the life of
    // the process (`CharBuffer.position` in a loop, 100 ns -> 2.5-5 us per call).
    let abandoned_supersede = optimized && cratonvm_jit::ir_evidence::take_supersede_abandoned();
    let published = (published || abandoned_supersede) && !optimizing_publication_refused;
    // Read on the SAME thread, immediately after the compile: did this C2 task
    // run the whole optimizing pipeline and then produce a single-pass body?
    let fell_through = cratonvm_jit::last_compile_fell_through_to_single_pass();
    // Interpreter round i6 wave 115, lane L2 (`i100-L2`): and why, as far as
    // the IR builder classifies it. Same thread, same moment as the bool.
    let fall_through_cause = cratonvm_jit::last_compile_fall_through_cause();
    let compile_us = start.elapsed().as_micros() as u64;
    if published && optimized {
        note_c2_compile(fell_through, compile_us);
        note_c2_fall_through_cause(fall_through_cause);
    }
    // JIT r22 wave 2, lane inline (INLINE22-1,
    // `CRATONVM_JIT_C2_ACCEPTANCE_REOFFER`, opt-in): a C2 task that published
    // its single-pass fall-through because the acceptance gate refused the
    // optimizing body leaves the method at C2, settled; arm one re-offer the
    // drain fires once the live count has grown (`arm_acceptance_reoffer`
    // reads the switch and bounds the arms).
    if published
        && optimized
        && fell_through
        && matches!(
            fall_through_cause,
            Some(cratonvm_jit::IrFallThroughCause::AcceptanceRefused)
        )
    {
        let _ = shared
            .jit
            .tiered_manager
            .arm_acceptance_reoffer(&task.method_key);
    }
    // Interpreter round i5 wave 100, lane L2
    // (`CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER`, default ON since the w100
    // triage): a C2 task that published a single-pass body arms one
    // recompile, asked when a deferred field site of that body links. Wave
    // 115 (`CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER_ROWLESS_ONLY`, default
    // OFF): only when the optimizing build bailed on a row-less field site
    // of its own, the one bail a linked site can take back.
    if published
        && optimized
        && fell_through
        && deferred_field_site_retier_cause_admits(fall_through_cause)
    {
        arm_deferred_field_site_retier(&shared, &task.method_key);
    }
    // C1→C2 supersede, publish side: a freshly-published C2 body REPLACED the
    // C1 entry in `jit_cache` (JitCache::put overwrites by key). Bump this
    // VM's supersede epoch so per-thread invoke-cache `Jit` entries (which
    // snapshot the epoch at IC-fill time) report stale on their next hit,
    // self-evict, and re-resolve to the C2 body. Without this, call sites that
    // already flipped to the C1 artifact would run it forever.
    //
    // That bump is VM-WIDE: it invalidates every `Jit` invoke-cache entry of
    // this VM, for every call site, in every thread — `CachedInvokeTarget::is_stale`
    // compares the VM's one counter (`JitRealm::supersede_epoch`; a process
    // static until interpreter round i1 wave 19), and `InvokeCache::get`
    // self-evicts on it. So it must be paid only when there is something to invalidate. The
    // first reading off the `c1=`/`c2=` diagnostic said it usually is not:
    // in one CratonBench run, 7 of 9 supersedes republished a body of exactly
    // the same size, and a 8th (`fib`) had no predecessor at all.
    //
    // Two of the three outcomes below cannot invalidate anything:
    //
    //  * `FirstPublish` — no prior body, so no `Jit` entry can be holding a
    //    replaced one. `fib` reaches C2 without a C1 body at all, because
    //    `promote_scalar_selfrec_to_ir` sends the narrow scalar self-recursion
    //    shape straight to the optimizing pipeline. The interpreter's negative
    //    "no compiled body" memo is NOT this counter's job: `JitCache::put`
    //    bumps `jit_cache_generation` on every publication precisely so a
    //    first insertion is observed there.
    //  * `Unchanged` — the published body is byte-identical to the one it
    //    replaced, which is what a C2 task that fell back to the single-pass
    //    backend produces (the IR admission gate declines, single-pass
    //    recompiles the same bytecode deterministically). An IC entry still
    //    holding the old artifact executes identical machine code, and it owns
    //    an `Arc` to it, so the artifact stays alive.
    //
    // Identical code bytes also imply an identical ABI — `needs_heap` and
    // `needs_context` are visible in the prologue — so an entry kept on the old
    // artifact cannot be called the wrong way.
    //
    // Every outcome still bumps. The bump was measured, not assumed, to be
    // nearly free — 9 invoke-cache evictions over a whole CratonBench run and 0
    // over the regex workload — so skipping those two saves nothing; the
    // opt-in that did (`CRATONVM_JIT_SUPERSEDE_EPOCH_SKIP_USELESS`) was deleted
    // in JIT round 14 wave 2. The outcome is still classified for the census.
    //
    // JIT r23 wave 11 (lane L2): the cost is not the eviction count but what
    // each eviction does next. An evicted site's next call takes the slow
    // path, which runs the callee INTERPRETED once, so a `FirstPublish` moved
    // every compiled call site of the VM back through the interpreter for one
    // call (`jit_lambda_door_deopt_resumes`: `warm`'s first publish sent
    // `trip`'s call of the compiled `step` to the interpreter's lambda door).
    // `FirstPublish` no longer bumps unless another body of the key landed in
    // the compile's window (`supersede_epoch_bump_needed`,
    // `CRATONVM_JIT_SUPERSEDE_EPOCH_ONLY_ON_REPLACE`, default ON). `Unchanged`
    // still bumps: it replaced a live body.
    if published && optimized {
        // The class id is resolved BEFORE `jit_cache` is read-locked, never
        // under it: `fetch_osr_compile_inputs` takes `class_manager.read()`,
        // and the class-invalidation hook (`jit_invalidate_adapter`) takes
        // `jit_cache.write()` while `ClassManager` holds its own write lock
        // (redefinition, synthetic upgrade, layout recompute). Holding the
        // cache read across the class-manager read was the opposite order —
        // an AB/BA deadlock between this worker and that mutator, with a
        // stop-the-world GC then waiting on the stuck mutator.
        // `superseded_body` above already resolves in this order.
        let replacement = {
            fetch_osr_compile_inputs(
                &shared,
                task.method_key.class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            )
            .and_then(|(class_id, _, _)| {
                let jit_cache = shared.jit.jit_cache.read();
                jit_cache.get(
                    &task.method_key.class_name,
                    &task.method_key.method_name,
                    &task.method_key.descriptor,
                    class_id,
                )
            })
            // Same reason as `superseded_body`: a later publish can withdraw
            // this body before the block ends, leaving this the last owner.
            .map(cratonvm_jit::RetainedCode::new)
        };
        let outcome = classify_supersede(
            superseded_body.as_ref().map(|cm| cm.code_bytes()),
            replacement.as_ref().map(|cm| cm.code_bytes()),
        );
        note_supersede_outcome(outcome);
        // hm5: and a baked direct call to the superseded body, at any depth of
        // a compiled caller chain, is forwarded to the new one.
        if outcome == SupersedeOutcome::Changed {
            if let (Some(old), Some(new)) = (superseded_body.as_ref(), replacement.as_ref()) {
                let forwarded =
                    supersede_retire_superseded(&shared, old, new, &task.method_key.method_name);
                // HANDLER9-2 (JIT round 19 wave 11, lane handler11): the
                // outside-manager census's split of C2 publications over a
                // candidate's C1 body by whether its baked callers now reach
                // the C2 body (a no-op unless the census or the supersede is
                // armed).
                shared
                    .jit
                    .tiered_manager
                    .note_c2_supersede_of_outside_candidate(&task.method_key, forwarded);
            }
        }
        // JIT r23 wave 11, lane L2 (`CRATONVM_JIT_SUPERSEDE_EPOCH_ONLY_ON_REPLACE`,
        // default ON): a first publish that displaced nothing leaves the
        // epoch alone. The stamp is read after `replacement`, so any
        // publication of this name since `publish_stamp_before` counts.
        let publish_stamp_after = publish_stamp_before.map(|_| {
            shared
                .jit
                .jit_cache
                .read()
                .callee_publish_stamp(&task.method_key.method_name, &task.method_key.descriptor)
        });
        let bumped = apply_supersede_epoch(
            &shared,
            outcome,
            publish_stamp_before,
            publish_stamp_after,
            true,
        );
        if crate::runtime::env_cache::dbg_jitc() {
            // `c1=` is the body this one replaced, `c2=` the one that replaced
            // it. Both, always: the question this line exists for is whether
            // the optimizing tier is producing a BETTER body, and a size on
            // its own answers nothing without the size it displaced.
            //
            // `outcome=` is what separates the three cases a bare `c1=?` used
            // to conflate — no predecessor, an identical republish, and a real
            // replacement — and `epoch_bumped=` says whether this publish
            // actually paid the VM-wide invalidation.
            eprintln!(
                "[cratonvm-jitc] c2-supersede published {}.{}{} (epoch={}) c1={} c2={} outcome={} epoch_bumped={}",
                task.method_key.class_name,
                task.method_key.method_name,
                task.method_key.descriptor,
                shared.jit.supersede_epoch(),
                superseded_body
                    .as_ref()
                    .map(|cm| cm.code_bytes().len().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                replacement
                    .as_ref()
                    .map(|cm| cm.code_bytes().len().to_string())
                    .unwrap_or_else(|| "?".to_string()),
                outcome.as_str(),
                bumped,
            );
            if fell_through {
                eprintln!(
                    "[cratonvm-jitc]   …the optimizing pipeline ran and then handed this method to the single-pass backend ({compile_us} us total)",
                );
            }
            // When a replacement is the same LENGTH but not the same bytes,
            // say how far apart it actually is. A handful of scattered bytes
            // is a relocation (an embedded absolute address that moved),
            // which is a body that could still be treated as unchanged; a
            // large fraction is genuinely different code and cannot.
            if let (Some(b), Some(a)) = (superseded_body.as_ref(), replacement.as_ref()) {
                let (bb, ab) = (b.code_bytes(), a.code_bytes());
                if bb.len() == ab.len() && bb != ab {
                    let differing = bb.iter().zip(ab).filter(|(x, y)| x != y).count();
                    let first = bb.iter().zip(ab).position(|(x, y)| x != y).unwrap_or(0);
                    eprintln!(
                        "[cratonvm-jitc]   …same length, {} of {} bytes differ ({:.3}%), first at +0x{:x}",
                        differing,
                        bb.len(),
                        100.0 * differing as f64 / bb.len() as f64,
                        first,
                    );
                }
            }
        }
    }
    // C1→C2 supersede, trigger side: report whether this method would take
    // the optimizing IR pipeline (and is expected to benefit) so the worker
    // loop enqueues a Low-priority C2 recompile after it records this C1
    // publish. Evaluated only on a successful non-optimized publish — the
    // scan + predicate are cheap and run once per method.
    // A method whose IR build bailed on a `new` whose class was not loaded YET
    // is owed ONE more optimizing attempt. Note the absence of `!optimized`:
    // that bail happens INSIDE a C2 task, which then falls through to the
    // single-pass backend — so the C1->C2 promotion below, which is only for a
    // C1 publish, is not the door this can use. `take_deferred_new_retry`
    // consumes the memo only when the deferred `new` sites RESOLVE now: a class
    // still unloaded holds the retry rather than burning it on an attempt that
    // would bail identically.
    let deferred_new_retry = published
        && crate::runtime::env_cache::c2_supersede()
        && take_deferred_new_retry_if_resolvable(
            &shared,
            &task.method_key.class_name,
            &task.method_key.method_name,
            &task.method_key.descriptor,
        );
    // JIT round 19 wave 2, lane eager2 (EAGER19-3): this task's publication
    // found its late-bind mark newer than its compile, and the recompile it
    // asked was refused while this task held the slot: the worker asks the
    // same C2 retry after `finish` (`late_bind_note_older_publication`).
    // Taken unconditionally so a stale row never outlives its task.
    let late_bind_owed = take_late_bind_owed_retry(
        &shared,
        task.method_key.class_id.as_u32(),
        &task.method_key.method_name,
        &task.method_key.descriptor,
    );
    let deferred_new_retry = deferred_new_retry
        || (published && late_bind_owed && crate::runtime::env_cache::c2_supersede());
    let c2_upgrade_candidate = published
        && !optimized
        && crate::runtime::env_cache::c2_supersede()
        && fetch_osr_compile_inputs(
            &shared,
            task.method_key.class_id,
            &task.method_key.class_name,
            &task.method_key.method_name,
            &task.method_key.descriptor,
        )
        .map(|(_, padded, _)| {
            let code_len = padded.len().saturating_sub(2);
            cratonvm_jit::c2_upgrade_would_engage(
                &padded,
                code_len,
                &task.method_key.descriptor,
                crate::runtime::env_cache::jit_ir_long(),
                true,
                crate::runtime::env_cache::jit_ir_call_virtual(),
            )
        })
        .unwrap_or(false);
    // A `None` above is not one thing. The comment on `published` already lists
    // the causes — "skip-listed, resolver miss, code-cache cap, concurrent
    // redefine" — and two of them are PERMANENT POLICY, not a codegen attempt
    // that failed. Reporting every `None` as a codegen failure made
    // `complete_task` spend `tier_fail_count` on methods no compile was ever
    // run for: three futile background tasks each, then the method is retired
    // and reported by `jit-method-stats` as `compile-failed reason=unrecorded`
    // — unrecorded precisely because nothing ran to record a bail site.
    //
    // That is how a Spring Boot context startup reported
    // `hot_but_stuck_in_interpreter=79 (ineligible-by-policy=0,
    // compile-failures=69)` while 2499 methods sat in the skip-seal census:
    // every one of those 69 was a policy verdict wearing a codegen failure's
    // label, which sent the reader looking for a compiler bug that is not there.
    //
    // `MethodState::ineligible` is the field that exists for exactly this, and
    // `complete_task` already honours it — it just was never told.
    //
    // `policy_declined` is the slow path's own report for the refusals that
    // record a verdict without bail-listing (a registered native runs instead,
    // the FJP lever, an inherited native override, no `Code`); before it, each
    // of those spent the method's three retries as "failed compiles"
    // (interpreter round i1 wave 6, L7).
    let declined_permanently = !published
        && (policy_declined
            || shared
                .jit
                .jit_skip_set
                .read()
                // The skip-set and `MethodKey` both hold `Arc<str>`, so the
                // probe key is three reference-count bumps.
                .contains(&(
                    task.method_key.class_name.clone(),
                    task.method_key.method_name.clone(),
                    task.method_key.descriptor.clone(),
                ))
            || jit_verdicts(&shared).is_bail_listed(
                task.method_key.class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            ));
    // Interpreter round i6 wave 116, lane L2
    // (`CRATONVM_JIT_BG_RESOLVER_MISS_WAITS_ON_CLASS`, default OFF; pages
    // `i39-L2-proposal-a-compile-request-returns-why-it-produced-no-body`,
    // `i107-L2`): an unpublished compile that stopped at a constant-pool
    // resolver miss (a class not loaded yet; `RESOLVER_MISS_BAIL_SITES`) is
    // "waiting on a class", not a codegen failure: the manager parks the
    // method until a class is defined instead of spending one of its three
    // retries (`tiered::note_compile_waiting_on_class`). Cold: asked only for
    // an unpublished, undeclined task.
    if !published
        && !declined_permanently
        && !optimizing_publication_refused
        && bg_resolver_miss_waits_on_class_enabled()
        && background_refusal_is_resolver_miss(&shared, &task.method_key)
    {
        cratonvm_jit::tiered::note_compile_waiting_on_class();
    }
    CompileOutcome {
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        compile_time_ms: start.elapsed().as_millis() as u64,
        published,
        c2_upgrade_candidate,
        deferred_new_retry,
        declined_permanently,
        optimizing_publication_refused,
    }
}

/// Does the panic message `msg` NAME `exception` as its head — exactly
/// `exception`, or `exception: detail`, the shape the historical JIT helpers
/// panicked with? A message that merely mentions the word (a Rust assertion
/// such as `"unexpected NullPointerException state"`) is a VM defect, and
/// turning it into a catchable Java exception would hide it.
pub(in crate::runtime::interpreter::jit_bridge) fn panic_names_java_exception(
    msg: &str,
    exception: &str,
) -> bool {
    msg.strip_prefix(exception)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
}

/// Convert a JIT panic payload into a `MethodCallFailed`.
///
/// A payload whose message is headed by a Java exception name
/// ([`panic_names_java_exception`], e.g. `"ArrayIndexOutOfBoundsException:
/// index 7 out of bounds for length 5"`) becomes that exception. Everything
/// else becomes `InternalError` so it doesn't crash the VM — and stays visible
/// as the defect it is: this used to match by SUBSTRING, so any Rust panic that
/// mentioned `NullPointerException`, `ClassCastException`, `StackOverflow` or
/// `ArithmeticException` anywhere became a Java exception an application
/// `catch` could swallow.
pub fn jit_panic_to_exception(
    shared: &SharedVm,
    thread: &mut JvmThread,
    payload: Box<dyn std::any::Any + Send>,
) -> MethodCallFailed {
    let msg = if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        *s
    } else {
        "unknown JIT panic"
    };

    // Parse ArrayIndexOutOfBoundsException
    if panic_names_java_exception(msg, "ArrayIndexOutOfBoundsException") {
        let error = RuntimeError::aioobe_index_only(parse_aioobe_index(msg));
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse NullPointerException
    if panic_names_java_exception(msg, "NullPointerException") {
        let error = RuntimeError::NullPointerException {
            message: Some(msg.to_string()),
        };
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse ClassCastException
    if panic_names_java_exception(msg, "ClassCastException") {
        let error = RuntimeError::ClassCastException {
            message: msg.to_string(),
        };
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse StackOverflowError
    if panic_names_java_exception(msg, "StackOverflowError") {
        let error = RuntimeError::StackOverflowError;
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse ArithmeticException (e.g. division by zero)
    if panic_names_java_exception(msg, "ArithmeticException") {
        let error = RuntimeError::ArithmeticException {
            message: msg.to_string(),
        };
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Unknown panic — wrap as InternalError (non-catchable)
    MethodCallFailed::InternalError(VmError::Internal {
        message: format!("JIT panic: {msg}"),
    })
}

/// Resolve an inline site for a callee method.
///
/// Returns `Some(InlineSite)` if the callee is eligible for inlining:
/// - Bytecode length <= MAX_INLINE_BYTECODE_SIZE (35)
/// - No exception handlers, not synchronized
/// - No unsupported bytecodes (new, checkcast, instanceof, invoke*, etc.)
///
/// Resolution starts at the CONSTANT-POOL class, which is the right answer for
/// `invokestatic`/`invokespecial` and the wrong one for a guarded virtual or
/// interface site — see [`resolve_receiver_inline_site`].
/// The plan-time direct-bind resolver an inline site consults for the calls
/// inside the body it is about to splice.
///
/// Exactly the closure shape the top-level `direct_calls` planning already uses
/// — `callee_compiler` on the mutator door, `direct_callee_lookup` on the
/// background one — so a spliced call inherits every one of their refusal gates
/// unchanged: FJP blocklist, native shadow, callee exception table,
/// `synchronized`, JVMS §5.5 static-init, indy trap, and the eager-callee-chain
/// depth / cycle / fan-out bounds. Answers `(compiled entry, needs context)`.
///
/// The first argument is the class whose constant pool NAMES the callee — the
/// spliced body's declaring class, not the compile's root (interpreter round
/// i1 wave 12, L2). The binder resolves the name through that class's loader
/// and asks JVMS §5.4.4 access with it as the accessor: a body spliced from
/// `java.util.Arrays` names `jdk.internal.util.ArraysSupport`, which the root
/// class may be unable to access (or, under another loader, may name a
/// different class altogether).
pub(in crate::runtime::interpreter) type InlineDirectBind<'a> =
    &'a dyn Fn(ClassId, &str, &str, &str) -> Option<(usize, bool)>;

/// Direct-bind the statically bound calls (`invoke_kind` 1 or 3) a spliced
/// body leaves behind, skipping a pc that is itself NESTED (a spliced body
/// beats a call). Each target is asked for on behalf of its own
/// `declaring_class_id`, the class whose constant pool named it — never the
/// root compile's class (i11-L0).
pub(in crate::runtime::interpreter::jit_bridge) fn bind_spliced_static_calls(
    invoke_targets: &mut [(usize, cratonvm_jit::InlineInvokeTarget)],
    nested_sites: &[cratonvm_jit::NestedInlineSite],
    bind: InlineDirectBind<'_>,
) {
    for (ipc, target) in invoke_targets.iter_mut() {
        if nested_sites.iter().any(|n| n.callee_pc == *ipc) {
            continue;
        }
        if target.invoke_kind != 1 && target.invoke_kind != 3 {
            continue;
        }
        target.direct_entry = bind(
            ClassId::new(target.declaring_class_id),
            &target.class_name,
            &target.method_name,
            &target.descriptor,
        );
    }
}

/// `CRATONVM_JIT_IR_SPLICE_FINAL_DIRECT` (JIT round 16, lane callbind),
/// **default OFF** (not measured): [`bind_spliced_final_calls`]. Read per
/// resolution, so it adds no process global. Inert unless the optimizing tier
/// lowers a spliced bound row as a raw `CALL` at all
/// (`ir_splice_direct_call_enabled`, `ir_direct_calls_enabled`): without that
/// lowering a kind-1 row is a by-name dispatch, worse than the inline cache
/// the virtual row keeps.
pub(in crate::runtime::interpreter::jit_bridge) fn ir_splice_final_direct_enabled() -> bool {
    false
        && cratonvm_jit::ir_splice_direct_call_enabled()
        && cratonvm_jit::ir_direct_calls_enabled()
}

/// Direct-bind the unoverridable `invokevirtual`s of an IR-mode spliced body
/// (`pins`: callee pc, declaring class, its id; collected under the class
/// manager by `invokevirtual_site_final_owner`), JIT round 16, lane callbind.
///
/// A pinned row becomes what the top-level door makes of the same call: kind 1
/// (statically bound) at the declaring class, with that class's id as the
/// substituted owner (`owner_class_id`, as a private pin carries it) and the
/// compiled entry. The emitter then lowers a raw `CALL` with the receiver null
/// check (`emit_direct_cross_call`) instead of an inline-cached virtual call.
/// The row changes ONLY when the binder answers an entry: a statically bound
/// row with no entry is what `ir-splice-unbindable-surviving-call` refuses, so
/// a refused bind leaves the virtual row exactly as it was.
///
/// Like [`bind_spliced_static_calls`], a NESTED pc is skipped (wave b; wave a
/// bound it, so that a nested body the planner prunes back to its call would
/// fall back to a direct `CALL`). Bound, such a row is dead whenever the body
/// IS spliced, yet `intern_inline_invoke_targets` still files its entry in
/// `_direct_callee_entries`: the caller keeps the callee artifact alive and is
/// evicted with it (`supersede-baked-callers`) each time the callee tiers up,
/// for a call it never makes. `HashMap.get` -> `getNode` is spliced in the
/// round-16 default arm, so that was all cost and no gain there; the pruned
/// case (the `multi-return` stage) keeps its inline cache.
pub(in crate::runtime::interpreter::jit_bridge) fn bind_spliced_final_calls(
    invoke_targets: &mut [(usize, cratonvm_jit::InlineInvokeTarget)],
    pins: &[(usize, String, u32)],
    nested_sites: &[cratonvm_jit::NestedInlineSite],
    bind: InlineDirectBind<'_>,
) {
    for (pin_pc, owner, owner_id) in pins {
        if nested_sites.iter().any(|n| n.callee_pc == *pin_pc) {
            continue;
        }
        let Some((_, target)) = invoke_targets
            .iter_mut()
            .find(|(pc, t)| pc == pin_pc && t.invoke_kind == 0)
        else {
            continue;
        };
        let Some(entry) = bind(
            ClassId::new(target.declaring_class_id),
            owner,
            &target.method_name,
            &target.descriptor,
        ) else {
            continue;
        };
        target.invoke_kind = 1;
        target.class_name = owner.clone();
        target.owner_class_id = *owner_id;
        target.direct_entry = Some(entry);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] splice-final-direct {owner}.{}{} at callee_pc={pin_pc} -> BOUND",
                target.method_name, target.descriptor,
            );
        }
    }
}

/// Resolve a body for the **single-pass** tier to splice at a statically
/// bound site of `requesting_class_id`.
///
/// `substituted` is the compile's [`SubstitutedOwners`]: the single-pass
/// ladder asks with the owner `cp_invokespecial_owner_resolver` substituted
/// for the constant-pool class (the declaring class of a private or `final`
/// target, a super call's selection start), and such a name is resolved by
/// the id the substitution recorded, under the direct binder's access rule
/// ([`direct_bind_callee_class`]), not looked up as a class constant of the
/// caller (i13-L2).
/// nio21 wave 7, lane L4 (W5L4-3 of `jit-r21-nio-proposals-w5-L4.md`): does a
/// `getfield` whose constant pool names `(name, descriptor)` read one of the
/// fields the optimizing tier may treat as "a `byte[]` or null"? The pool row
/// alone is not the proof (name-only resolution may find another field of
/// that name); [`byte_array_field_declarer_admitted`] confirms the resolved
/// field's declaring class. `ByteBuffer.hb` is the one row: the heap
/// buffer's backing array, the base of every heap `ByteBuffer` scoped access
/// (`R17NioRows heapbb`).
pub(in crate::runtime::interpreter::jit_bridge) fn byte_array_field_row(
    name: &str,
    descriptor: &str,
) -> bool {
    name == "hb" && descriptor == "[B"
}
