// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `invokestatic` dispatch and its call-site cache.
//!
//! The simplest of the three dispatch tiers — the target is known from the
//! constant pool and does not depend on a receiver — which is exactly why
//! the cache here is the most aggressive: `execute_invokestatic_cached`
//! skips resolution entirely once `populate_invoke_cache` has recorded a
//! target, and `cached_static_owner_stale` is the one check that keeps a
//! class redefinition from letting it run stale bytecode.
//!
//! The intrinsic counters and `pop_coerced_invoke_args_intrinsic` live here
//! because a static call is where an intrinsic substitution is decided:
//! the arguments are popped and coerced once, then either handed to the
//! Rust intrinsic or pushed into a real frame.

use super::*;

pub(super) fn execute_invokestatic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    let (method_class_name, method_name, method_descriptor, num_params) =
        resolve_method_ref(shared, current_class_id, cp_index)?;

    // PGO-01: call-site evidence — same placement rationale as
    // execute_invoke_kind's is_special arm (record once, early, right after
    // resolution succeeds; this is the slow path, only reached on a cache
    // miss from execute_invokestatic_cached, so the instruction is
    // definitely executing by this point).
    if crate::jit::profile::is_receiver_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared
            .jit
            .profile_store
            .record_call_site_borrowed(cid, mn, md, pc);
    }

    // Skip class init if this is a registered native method (avoids initialization hangs).
    // Walk the superclass chain because the constant pool may reference a subclass
    // while the native is registered on the declaring superclass.
    // Skip hierarchy walk for <init> — constructors are NOT inherited.
    // SyntheticStub registrations on real-protected classes must not suppress
    // loading the real owner. Otherwise the first call materializes a stub and
    // seeds a native invoke-cache entry before real bytecode can take over.
    // ONE registry probe, not two (ARCH-2026-08-04, native-dispatch pass).
    //
    // This was `find(..).is_some()` immediately followed by `kind_of(..)` on
    // the *same* triple. Both funnel into `slot_for_exact`, and each pass
    // hashes the class name, probes the `classes_with_natives` prefilter,
    // hashes name + descriptor, indexes, and then verifies all three strings —
    // so the pair did that work twice, back to back, on the `invokestatic`
    // resolution path. `find_with_kind` returns both facts from one pass.
    //
    // Behaviour is identical *for this comparison*, which is the only thing
    // that made the merge safe and is worth writing down. The two forms differ
    // on the cold descriptor-quirk path: `kind_of` is `slot_for_exact`-only, so
    // a quirk match gives `None`, while `find_with_kind` deliberately reports
    // `Bridge` there (its own comment explains why it does not report the true
    // kind — that would change which natives the real-JDK `SyntheticStub` drop
    // applies to). Neither `None` nor `Bridge` equals `SyntheticStub`, so
    // `direct_synthetic_stub_may_yield` is `false` either way. `find` and
    // `find_with_kind` also share the same `slot_for_exact`-then-quirks
    // structure, so `.is_some()` agrees on every input.
    let direct = shared.natives.native_methods.find_with_kind(
        &method_class_name,
        &method_name,
        &method_descriptor,
    );
    let direct_native_registered = direct.is_some();
    let direct_synthetic_stub_may_yield = direct
        .is_some_and(|(_, kind)| kind == cratonvm_native_api::NativeKind::SyntheticStub)
        && real_protected_stub_class(&method_class_name);
    let direct_native = direct_native_registered && !direct_synthetic_stub_may_yield;
    let is_native = direct_native
        || (method_name.as_ref() != "<init>" && {
            let cm = shared.classes.class_manager.read();
            let mut found = false;
            if let Some(mut cid) = cm.get_loaded_class_id(&method_class_name) {
                while let Some(pid) = cm.get_class(cid).and_then(|c| c.superclass) {
                    if let Some(p) = cm.get_class(pid) {
                        if shared
                            .natives
                            .native_methods
                            .find(&p.name, &method_name, &method_descriptor)
                            .is_some()
                        {
                            found = true;
                            break;
                        }
                    }
                    cid = pid;
                }
            }
            found
        });
    if crate::runtime::env_cache::modstatic_dbg()
        && (method_name.as_ref() == "initBootModuleLoader"
            || method_class_name.as_ref() == "org/jboss/modules/Module")
    {
        eprintln!(
            "MODSTATIC: invokestatic {}.{} {} is_native={} direct_native={}",
            method_class_name, method_name, method_descriptor, is_native, direct_native
        );
    }
    // Self-call identity fix (2026-07-06 — see hib-proxyclassreuse-loader-
    // blind-class-resolution.md Residual B): when the invokestatic's
    // constant-pool owner class NAME textually equals the CURRENTLY
    // EXECUTING class's own name, that class is by definition already
    // loaded/linked/initialized — it is literally running this bytecode
    // right now. Record its `ClassId` directly (`current_class_id`) instead
    // of letting the dispatch below re-resolve the name through the global,
    // loader-blind `get_loaded_class_id` (which answers "whichever loader's
    // copy of this name exists" and is silently wrong whenever 2+ DIFFERENT
    // loaders each define their own class under the same simple name — the
    // common case for Groovy, which names closure literals positionally
    // per-script: `<Script>$_run_closure1`, `$_run_closure2`, ... — so two
    // unrelated scripts compiled by two different `GroovyClassLoader`s
    // routinely produce two DISTINCT classes sharing the identical name).
    //
    // Concretely: a Groovy closure's `doCall` calling its OWN private
    // synthetic class-literal-cache accessor (`$get$$class$Foo()`, a
    // compiler-generated `invokestatic` self-call) re-resolved the owner by
    // name and landed on a DIFFERENT script's same-named closure class
    // instead of the class that is actually executing — `NoSuchMethodError`
    // on the synthetic accessor, since the wrong script's copy never
    // declared that particular helper. Spring `GroovyBeanDefinitionReaderTests`
    // (two sequential test methods, each a fresh `GroovyShell`/
    // `GroovyClassLoader`, each producing a `beans$_run_closure1`)
    // reproduces this deterministically (confirmed with a minimal
    // `KRunMethod` 2-test repro before this fix; not reproducible with a
    // simple 2-`GroovyShell` top-level-closure repro, which already worked
    // — the bug needs a SELF-call inside the closure body, not merely
    // distinct closure identity).
    //
    // The dispatch identity selected below feeds BOTH the class-init step and
    // the `try_stackless_invoke` dispatch override, so the method
    // lookup itself (not just class initialization) uses the correct,
    // already-known identity for a self-call.
    let self_class_id = shared
        .classes
        .class_manager
        .read()
        .get_class(current_class_id)
        .filter(|c| c.name.as_ref() == method_class_name.as_ref())
        .map(|_| current_class_id);

    // Sibling static owners in a user-defined loader need the same identity
    // preservation as self-calls; resolving by flat name can pick the app copy.
    //
    // bt18-inline-tlab-regression-20260724 (part 2): both probes below are
    // gated on the process-wide "any defining loader registered at all"
    // atomic. Without the gate, every dispatched static call paid the
    // defining-loader registry mutex twice — and, worse, the
    // cache-suppression rule below misread a plain SELF-RECURSIVE static
    // call (static_dispatch_class_id == Some(self)) as loader-specific,
    // suppressing invoke-cache promotion for every recursive static in
    // every application and sending each call through the slow dispatcher
    // (bt18 4.8x). Only a genuinely loader-specific owner selection —
    // tracked via `loader_specific_dispatch` — may suppress promotion.
    let any_user_loader = cratonvm_native_builtins::classloader::any_defining_loader_registered();
    let isolated_url_definition =
        any_user_loader && is_isolated_url_loader_definition(shared, thread, current_class_id);
    // A defining-loader registration is authoritative even when the
    // class-manager's loader-id metadata is unavailable for a real-JDK
    // subclass. Static symbolic references still use the caller's defining
    // loader under JVMS 5.4.3; restricting that to the historical fork/Groovy
    // gates leaves ordinary ModifiedClassPathClassLoader bytecode bound to the
    // flat application copy.
    let has_user_defining_loader = any_user_loader
        && cratonvm_native_builtins::classloader::defining_loader_for(
            shared.vm_identity,
            current_class_id.as_u32(),
        )
        .is_some();
    let mut loader_specific_dispatch = false;
    let static_dispatch_class_id = self_class_id.or_else(|| {
        // Keep static method owners in the same initiating-loader namespace
        // as every other symbolic reference. This includes the narrow
        // CompileWithForkedClassLoader carve-out, not only the global opt-in:
        // Spring's forked BootstrapUtils calls MergedAnnotations.search(), and
        // mixing a forked SearchStrategy singleton with an application Search
        // instance makes the latter's identity check fail spuriously.
        if should_use_loader_initiated_resolution(shared, current_class_id)
            || isolated_url_definition
            || has_user_defining_loader
        {
            // Preserve the initiating loader even when the global classpath
            // already has a same-named class. This is required for nested
            // implementation jars whose owner is only visible to the caller
            // loader.
            let known = if isolated_url_definition || has_user_defining_loader {
                // An initiating-cache hit may be an application definition
                // recorded before this isolated URL loader defined its own
                // copy. Static calls into that stale class share its caches
                // with the child and poison Class-identity keyed metadata.
                lookup_loader_defined_exact(shared, current_class_id, &method_class_name)
            } else {
                lookup_loader_initiated(shared, current_class_id, &method_class_name)
            };
            let selected = known.or_else(|| {
                drive_defining_loader_load(shared, thread, current_class_id, &method_class_name)
            });
            // Set on SELECTION, not on attempt.
            //
            // This flag's only job is to suppress invoke-cache promotion, and
            // the comment above it already states the rule: "Only a genuinely
            // loader-specific owner selection ... may suppress promotion."
            // Setting it before the lookup broke that rule the moment
            // `loader_aware_resolution()` became default-on (2026-07-04):
            // `should_use_loader_initiated_resolution` then returns `true`
            // unconditionally, so EVERY invokestatic whose constant-pool owner
            // is not the caller class entered this arm, set the flag, resolved
            // `None`, dispatched through the ordinary flat path anyway — and
            // was never promoted. The invokestatic inline cache was, in
            // effect, globally off.
            //
            // The cost is not subtle. An uncached invokestatic re-runs the
            // whole slow path on EVERY call: constant-pool resolve, three
            // string-keyed native-registry probes, a superclass walk under the
            // class-manager `RwLock`. `Thread.onSpinWait()` — an empty JDK
            // method — measured **807 ns per call** against 166 ns for a
            // byte-identical empty static in a user class (which IS cached,
            // via `self_class_id`) and 44 ns on HotSpot. Because
            // `AbstractQueuedSynchronizer.acquire` spins up to 255
            // `onSpinWait` rounds before parking, that lands directly on every
            // lock and condition handoff in the VM: `Condition.signal ->
            // await` 118 us vs HotSpot's 7.4 us, `ThreadPoolExecutor.execute
            // -> task entered` 183 us vs 7.9 us.
            //
            // Promoting a site that selected nothing is sound:
            // `populate_invoke_cache` performs its OWN loader-aware owner
            // resolution (`loader_owner_override`) before building the entry,
            // which is exactly why `invokevirtual`/`invokespecial` sites have
            // always been promoted through it. Suppression stays in place for
            // the case it was written for — a loader-specific owner actually
            // chosen here — because that owner is a `ClassId` the cache key
            // cannot yet carry.
            if selected.is_some() {
                loader_specific_dispatch = true;
            }
            selected
        } else {
            None
        }
    });

    // Cached: this sat as a raw per-call getenv inside the static
    // dispatcher (visible in the bt18 regression profile's getenv storm).
    fn invokestatic_loader_trace() -> bool {
        static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *G.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_INVOKESTATIC_LOADER_TRACE").is_some()
        })
    }
    if invokestatic_loader_trace()
        && (method_class_name.contains("SpringFactoriesLoader")
            || method_class_name.contains("EnvironmentPostProcessorsFactory")
            || method_class_name.contains("ManagementPortType")
            || (method_class_name.as_ref() == "org/springframework/util/ClassUtils"
                && method_name.as_ref() == "forName")
            || (method_class_name.as_ref() == "java/lang/Class"
                && method_name.as_ref() == "forName"))
    {
        let cur_loader = shared
            .classes
            .class_manager
            .read()
            .get_loader_id(current_class_id);
        let cur_name = shared
            .classes
            .class_manager
            .read()
            .get_class(current_class_id)
            .map(|c| c.name.to_string());
        let resolved_loader = static_dispatch_class_id
            .and_then(|id| shared.classes.class_manager.read().get_loader_id(id));
        let global_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(&method_class_name);
        let global_loader =
            global_id.and_then(|id| shared.classes.class_manager.read().get_loader_id(id));
        eprintln!(
            "[INVOKESTATIC-LOADER-TRACE] method_class={} method={} current_class_id={:?} current_class_name={:?} current_loader={:?} is_native={} self_class_id={:?} static_dispatch_class_id={:?} static_dispatch_loader={:?} global_lookup_id={:?} global_lookup_loader={:?}",
            method_class_name, method_name, current_class_id, cur_name, cur_loader, is_native, self_class_id, static_dispatch_class_id, resolved_loader, global_id, global_loader
        );
    }
    if !is_native {
        let target_class_id = if let Some(id) = static_dispatch_class_id {
            id
        } else {
            // Load and initialize the target class.
            // Always go through load_class_concurrent so synthetic stubs
            // get upgraded to real classes when the .class file is available.
            match shared.load_class_concurrent(&method_class_name) {
                Ok(id) => id,
                Err(e) => {
                    // Loader-aware rescue: the `invokestatic` owner may live ONLY
                    // behind the referencing class's defining loader (e.g. a webapp
                    // class calling a static method on a sibling in its own
                    // `/WEB-INF/lib` jar — JSTL `JstlBaseTLV` →
                    // `XmlUtil.newXMLReader`). Drive that loader before failing.
                    // Strictly additive — only on a global miss.
                    match drive_defining_loader_load(
                        shared,
                        thread,
                        current_class_id,
                        &method_class_name,
                    ) {
                        Some(id) => id,
                        None => {
                            return Err(convert_class_not_found(
                                shared,
                                thread,
                                &method_class_name,
                                e.into(),
                            ))
                        }
                    }
                }
            }
        };
        ensure_class_initialized_shared(shared, thread, target_class_id)?;
    }

    // PERF (2026-07-21): see `pop_coerced_invoke_args_virtual` — same
    // non-allocating `nth_param_tag_byte` swap for `split_method_descriptor`.
    // This was the exact call site pinned by cdb stack sampling as the
    // dominant hot spot behind the ~197x Xerces SAX-parse slowdown (every
    // invokestatic re-parsed + heap-allocated a Vec<String> from scratch);
    // the primary fix is wiring `execute_invokestatic_cached` into the main
    // dispatch loop (see its call site's history) so this cold/miss path is
    // only reached once per call site instead of on every call. Fixed here
    // too since it's the same wasteful pattern and still runs on every
    // cache miss (JVMTI redefine, synthetic-stub upgrade, first call).
    //
    // BC SM2 fix (2026-05-28): pop slots as raw CompactValue and decode
    // with the parameter descriptor so a Long whose bit pattern collides
    // with the NaN-tagged SUB_OBJECT space is not silently coerced to 0L
    // by `to_value() -> Value::Object`. The descriptor-aware decode
    // path (`decode_by_descriptor(b'J')`) reinterprets the raw bits.
    let mut tmp_cv: Vec<(CompactValue, u8)> = Vec::with_capacity(num_params);
    for _ in 0..num_params {
        tmp_cv.push(thread.frames[frame_idx].stack.pop_with_kind()?);
    }
    tmp_cv.reverse();
    let mut args = Vec::with_capacity(num_params);
    // ONE forward scan, hoisted out of the per-argument loop below.
    let param_tags = ParamTags::of(&method_descriptor);
    for (i, (cv, kind)) in tmp_cv.into_iter().enumerate() {
        let pd_byte = param_tags.get(&method_descriptor, i);
        let v = decode_arg_kind_aware(cv, kind, pd_byte);
        args.push(coerce_invoke_arg_for_descriptor(pd_byte, v));
    }

    if let Some(res) = intercept_force_registered_native(
        shared,
        thread,
        frame_idx,
        &method_class_name,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        &args,
    ) {
        return res;
    }

    // GPU offload hook (Part E). Behind `gpu-offload`: with the
    // feature off, the entire block is removed by the preprocessor
    // and `execute_invokestatic` falls through to the existing CPU
    // path unchanged. On Hit the OffloadCache marshals, launches, and
    // writes back via `crate::runtime::offload::try_dispatch`.
    //
    // Invoke-cache interaction (found 2026-07-11): the interpreter
    // consults `thread.invoke_cache` BEFORE this slow path, so a site
    // promoted into the cache dispatches straight to the CPU body and
    // never re-enters this hook. An offloaded (or gated-but-eligible)
    // site must therefore never be promoted, or the SECOND call at
    // the site silently stops offloading — exactly the shape of a
    // warm benchmark loop. The slow-path re-entry cost is noise next
    // to any kernel that clears `--gpu-min-work`.
    #[cfg_attr(not(feature = "gpu-offload"), allow(unused_mut))]
    // The per-thread/static promoted invoke caches do not encode the
    // loader-specific owner selected above. Promoting this call site would
    // re-resolve its symbolic owner through the flat store and send the next
    // call into an application-loader copy. Keep loader-specific static calls
    // on the already-correct slow dispatcher until the cache key can carry
    // the resolved ClassId.
    //
    // bt18 part 2: suppression keys on `loader_specific_dispatch`, NOT on
    // `static_dispatch_class_id.is_some()` — the latter is Some for every
    // plain self-recursive static call (self_class_id), which suppressed
    // caching for all recursive statics in loader-free applications.
    let mut suppress_invoke_cache = loader_specific_dispatch || has_user_defining_loader;
    #[cfg(feature = "gpu-offload")]
    {
        // The guard is timed: it runs on EVERY call at a hooked site, ahead
        // of `try_dispatch`, and `get_or_create` is not obviously free.
        // Charged to `gpu_refusal_census` as `hook_guard` so the bench's
        // per-call overhead can be attributed instead of assumed -- which is
        // how it was established that the hook is 4% of it and the lost
        // invoke cache is the other 96%.
        let hook_timed = cratonvm_types::gpu_refusal_census::enabled();
        let hook_entered = std::time::Instant::now();
        let offload_cache = shared
            .offload_registry
            .get_or_create(shared.config.gpu_device_ordinal, &shared.config);
        let hook_open = shared.config.gpu_offload_enabled && offload_cache.has_device();
        // The call site, keyed as the pc-keyed invoke cache keys it. See the
        // `FallThroughKeepHooked` arm below.
        let offload_site = ((current_class_id.as_u32() as u64) << 32) | pc as u64;
        if hook_timed {
            cratonvm_types::gpu_refusal_census::add(4, hook_entered.elapsed().as_nanos() as u64);
        }
        if hook_open {
            match crate::runtime::offload::try_dispatch(
                shared,
                thread,
                frame_idx,
                &method_class_name,
                &method_name,
                &method_descriptor,
                &args,
            )? {
                crate::runtime::offload::DispatchOutcome::Handled => {
                    // This site just offloaded, so it is not a
                    // small-arrays-forever case; forget any refusal streak.
                    offload_cache.clear_site_below_min_work(offload_site);
                    // Deliberately NOT populating the invoke cache —
                    // see the block comment above.
                    return Ok(CachedCallResult::Handled);
                }
                crate::runtime::offload::DispatchOutcome::HandledWithValue(value) => {
                    // Part E — reduction kernel completed on the device;
                    // push its scalar return value onto the operand
                    // stack EXACTLY the way the fallback invokestatic
                    // path a few dozen lines below does it (tag-exact
                    // 2-slot push for `Value::Long`, then the
                    // native-pending-return clear so a stale pinned
                    // ObjectRef can't outlive this call site across GC —
                    // moot for Int/Long today, but this is the one push
                    // sequence and it must stay identical for either
                    // caller).
                    offload_cache.clear_site_below_min_work(offload_site);
                    let ret = crate::jit::return_type(&method_descriptor);
                    let value = coerce_value_for_return(value, ret);
                    push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                    // Deliberately NOT populating the invoke cache —
                    // same reasoning as the plain `Handled` arm above.
                    return Ok(CachedCallResult::Handled);
                }
                crate::runtime::offload::DispatchOutcome::FallThroughKeepHooked => {
                    // Eligible kernel, but a per-call gate (e.g.
                    // --gpu-min-work) declined this particular call.
                    // Run the CPU path but keep the site un-promoted
                    // so a future call can still offload.
                    //
                    // Until it has declined too many times in a row. Denying
                    // the site the invoke cache costs 10.3 us per call
                    // against 0.35 us cached, while the hook doing the
                    // denying costs 0.48 -- so this line, not the hook, is
                    // 96% of the measured overhead. After
                    // `CRATONVM_GPU_MIN_WORK_GIVEUP` consecutive refusals the
                    // site is promoted and the hook is never consulted there
                    // again. Requires `CRATONVM_INVOKE_CACHE_PC_KEY`, without
                    // which a "site" is a method reference and promoting one
                    // caller deoptimises its siblings.
                    if !offload_cache.note_site_below_min_work(offload_site) {
                        suppress_invoke_cache = true;
                    }
                }
                crate::runtime::offload::DispatchOutcome::FallThrough => {
                    // Method is ineligible / blacklisted / launch
                    // deopted. Run the CPU path below; the operand
                    // stack and locals are untouched.
                }
            }
        }
    }

    // Try stackless frame push for bytecode methods
    // For invokestatic, walk the native hierarchy to find inherited natives.
    match try_stackless_invoke(
        shared,
        thread,
        frame_idx,
        &method_class_name,
        &method_name,
        &method_descriptor,
        &args,
        true,
        false,
        static_dispatch_class_id,
    )? {
        CachedCallResult::FramePushed => {
            if !suppress_invoke_cache {
                populate_invoke_cache(thread, shared, current_class_id, cp_index, false, pc);
            }
            return Ok(CachedCallResult::FramePushed);
        }
        CachedCallResult::Handled => {
            if !suppress_invoke_cache {
                populate_invoke_cache(thread, shared, current_class_id, cp_index, false, pc);
            }
            return Ok(CachedCallResult::Handled);
        }
        CachedCallResult::CacheMiss => {}
    }

    // Fallback: recursive dispatch. `invoke_or_native` re-resolves the
    // owner by NAME (no ClassId-override parameter exists on it), so a
    // self-call whose class-literal method wasn't found by the stackless
    // path above, or a loader-local sibling static owner, still needs the
    // identity fix: dispatch straight through `invoke_on_class_shared` instead
    // of falling into the loader-blind name lookup `invoke_or_native` performs
    // internally.
    let result = if let Some(cid) = static_dispatch_class_id {
        crate::vm::invoke_on_class_shared(
            shared,
            thread,
            cid,
            &method_name,
            &method_descriptor,
            &args,
        )?
    } else {
        invoke_or_native(
            shared,
            thread,
            &method_class_name,
            &method_name,
            &method_descriptor,
            &args,
        )?
    };
    if let Some(value) = result {
        let ret = crate::jit::return_type(&method_descriptor);
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            // T18.K4 — tag-exact push for J/D fallback invokestatic return values.
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            // Hypothesis (b): symmetric clear-after-push for the slow invokestatic
            // path. `invoke_or_native` may recursively run `safe_native_call` which
            // pins the object return in `thread.native_pending_return`; without
            // this clear, the field outlives the call site and `update_root_snapshot`
            // re-roots a stale (already-popped) ObjectRef across GC.
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }

    // Populate invoke cache for future fast-path hits
    if !suppress_invoke_cache {
        populate_invoke_cache(thread, shared, current_class_id, cp_index, false, pc);
    }

    Ok(CachedCallResult::Handled)
}

// ───────────────────────── Interpreter intrinsic table ─────────────────────
//
// See `gaps/feature_roadmap_interpreter_intrinsic_table.md`. An intrinsic is a
// hot JDK method (`String.length`, `Object.getClass`, `System.arraycopy`, …)
// resolved ONCE at inline-cache fill time into a `CachedInvokeTarget::Intrinsic`
// entry. The steady-state hit pops args and calls the stored callback with no
// `RwLock`, no descriptor parse, and no native-registry `HashMap` probe.

/// Process-wide count of intrinsic fast-path dispatches. Incremented on every
/// `CachedInvokeTarget::Intrinsic` hit (static and virtual). Exposed for the
/// differential-test harness and profiling/acceptance counters.
pub(super) static INTRINSIC_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Number of interpreter intrinsic fast-path dispatches since process start.
pub fn intrinsic_hit_count() -> u64 {
    INTRINSIC_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Declare that filling an inline cache with `CachedInvokeTarget::Intrinsic`
/// has taken this native's registry slot **off the counted path**, so
/// `--dump-native-registry`'s `invocations` column reports a floor for it and
/// says so.
///
/// # Why this is needed at all
///
/// An `Intrinsic` entry holds a raw `fn` pointer resolved once, at cache-fill
/// time. Every later dispatch through that entry runs the callback directly and
/// **never consults the registry again**, so
/// [`record_invocation`](cratonvm_native_api::NativeMethodRegistry::record_invocation)
/// is never reached. MEASURED (`G33-1` §2, causal, one variable): 100,000
/// `Math.abs` calls report `invocations: 1`; the identical run with
/// `CRATONVM_DISABLE_INTRINSICS=1` reports **100,000**.
///
/// This bypass is **arm-independent** — it is just as blind under `--nojit` as
/// with the JIT on — which makes it invisible to the paired-arm cross-check
/// (`G37-1` §5) that is the first thing a reader reaches for. It is the largest
/// of the four known families.
///
/// # Bind time, never per call
///
/// A hot-path counter measured **+9.2 ns/call** against a 1.25 ns baseline
/// (`G33-1` §5) — plausibly the entire margin the intrinsic cache exists to
/// buy, since the whole point of an `Intrinsic` entry is to skip the `RwLock`,
/// the descriptor parse and the registry probe. So this is called **once per
/// cache fill**, on the cold populate path, and never from the steady-state
/// dispatch arm. It is `#[cold]` and it is one relaxed store.
///
/// # What the bit claims
///
/// That a bypassing path is **wired** for the slot, not that a bypassing
/// dispatch has happened. That is the statically true statement, and it is the
/// one a report-time reader can act on: it converts `invocations: N` from a
/// claimed total into an admitted floor.
///
/// Sticky, by the registry's design — see
/// [`mark_invocations_incomplete`](cratonvm_native_api::NativeMethodRegistry::mark_invocations_incomplete).
#[cold]
pub(super) fn mark_intrinsic_cache_bypass(
    registry: &cratonvm_native_api::NativeMethodRegistry,
    id: cratonvm_native_api::NativeMethodId,
) {
    registry.mark_invocations_incomplete(id);
}

/// [`mark_intrinsic_cache_bypass`] for the two install sites that hold the
/// triple but **no** `NativeMethodId`: `dispatch_virtual.rs`'s
/// `populate_invoke_cache` intrinsic block, which resolves its intrinsic
/// through the class store rather than through the registry, and the
/// `Thread.onSpinWait` install in this file, which deliberately fires without
/// requiring a registered native at all.
///
/// Returns `true` if a registry row existed and was marked.
///
/// **A `false` is not a failure.** Several intrinsics have no registry row in
/// this VM — `String.charAt` and `String.length` are registered nowhere in the
/// `--jdk-only` binary (`G33-1` §2, checked directly in the dump, *absent* and
/// not zero-valued) — and a triple with no row has no `invocations` cell that
/// could mislead anyone. Marking is only meaningful where a row exists to be
/// read.
///
/// One `resolve_id` — a class prefilter hit plus one hash — paid once per cache
/// fill on the cold populate path, in a block that has just taken the class
/// manager's read lock and walked the superclass chain. It is not measurable
/// against that.
#[cold]
pub(super) fn mark_intrinsic_cache_bypass_by_triple(
    registry: &cratonvm_native_api::NativeMethodRegistry,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match registry.resolve_id(class_name, method_name, descriptor) {
        Some(id) => {
            mark_intrinsic_cache_bypass(registry, id);
            true
        }
        None => false,
    }
}

/// Dispatch a resolved intrinsic: count the hit, run the callback through the
/// same `safe_native_call` machinery the `Native`/`VirtualNative` arms use,
/// and push any return value onto the caller's operand stack.
///
/// `args` follows the native-registry convention — `[receiver, param0, …]`
/// for a virtual call, `[param0, …]` for a static call — so an intrinsic
/// handler is byte-for-byte interchangeable with the native it shadows.
#[inline]
/// Phase 3 — dispatch a resolved interpreter intrinsic. The return-type byte
/// is pre-computed (stored in the IC entry), so unlike
/// `invoke_cached_native_callback` this path does NOT scan a descriptor
/// string at steady state. `safe_native_call` already records the native
/// ring enter/exit, so — unlike `invoke_cached_native_callback` — this path
/// does not redundantly record it a second time.
pub(super) fn invoke_cached_intrinsic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    return_type: u8,
) -> Result<(), MethodCallFailed> {
    INTRINSIC_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
    if let Some(value) = result.filter(|_| return_type != b'V') {
        let value = coerce_value_for_return(value, return_type);
        push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
        crate::vm::native_return_pushed_to_stack(shared, thread);
    }
    Ok(())
}

/// Largest `num_params + receiver` across the whole intrinsic table — the
/// widest is `System.arraycopy` (5 static params). An 8-slot stack buffer
/// covers every intrinsic with headroom, so the steady-state arg pop needs
/// no heap allocation.
pub(super) const MAX_INTRINSIC_ARGS: usize = 8;

/// Map a record's javac-generated `hashCode`/`equals` body to its interpreter
/// intrinsic, or `None` for anything else.
///
/// These two cannot live in `intrinsics::lookup`'s static
/// `(class, name, descriptor)` table: they apply to every record class in the
/// program, and only when the body really is the generated
/// `invokedynamic java.lang.runtime.ObjectMethods.bootstrap` shape — a record
/// that hand-writes `hashCode` must keep its own code.
/// [`Class::generated_record_object_methods`] performs (and memoises) that
/// body-shape check.
///
/// `toString` is deliberately left on the ordinary `invokedynamic` path: it is
/// not hot, and its formatting needs the component descriptors that the
/// call-site data carries.
pub(super) fn record_object_intrinsic(
    declaring: &crate::classloading::Class,
    method_name: &str,
    descriptor: &str,
) -> Option<cratonvm_native_api::InterpIntrinsic> {
    use crate::classloading::{RECORD_OBJ_EQUALS, RECORD_OBJ_HASH_CODE};
    let generated = declaring.generated_record_object_methods();
    match (method_name, descriptor) {
        ("hashCode", "()I") if generated & RECORD_OBJ_HASH_CODE != 0 => {
            Some(cratonvm_native_api::InterpIntrinsic::RecordHashCode)
        }
        ("equals", "(Ljava/lang/Object;)Z") if generated & RECORD_OBJ_EQUALS != 0 => {
            Some(cratonvm_native_api::InterpIntrinsic::RecordEquals)
        }
        _ => None,
    }
}

/// Phase 3 — pop intrinsic call arguments into a caller-provided stack
/// buffer using the parameter descriptors cached in the IC entry (split once
/// at fill time). The steady-state cost is a stack pop per arg plus
/// `coerce_invoke_arg_for_descriptor`: no `resolve_method_ref` (so no
/// resolution-cache `RwLock`, no `HashMap` probe), no `split_method_descriptor`
/// parse, and no heap allocation. `with_receiver` is true for virtual
/// intrinsics, where the receiver occupies args[0].
pub(super) fn pop_coerced_invoke_args_intrinsic<'b>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    num_params: usize,
    param_descs: &[Arc<str>],
    with_receiver: bool,
    buf: &'b mut [Value; MAX_INTRINSIC_ARGS],
) -> Result<&'b [Value], MethodCallFailed> {
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    let total = num_params + with_receiver as usize;
    debug_assert!(total <= MAX_INTRINSIC_ARGS);
    // BC SM2 fix (2026-05-28): pop raw CompactValue slots and decode each
    // with the corresponding parameter descriptor so a Long bit pattern
    // colliding with SUB_OBJECT survives intact (instead of being
    // converted to `Value::Object(None)` by `to_value()` and then
    // coerced to 0L).
    let mut cv_buf: [(CompactValue, u8); MAX_INTRINSIC_ARGS] =
        [(CompactValue::uninitialized(), 0u8); MAX_INTRINSIC_ARGS];
    for i in (0..total).rev() {
        cv_buf[i] = thread.frames[frame_idx].stack.pop_with_kind()?;
    }
    let base = if with_receiver {
        buf[0] = coerce_invoke_arg_for_descriptor(b'L', cv_buf[0].0.decode_by_descriptor(b'L'));
        1
    } else {
        0
    };
    for i in 0..num_params {
        let pd = param_descs
            .get(i)
            .map(|s| &**s)
            .unwrap_or("Ljava/lang/Object;");
        let pd_byte = pd.as_bytes().first().copied().unwrap_or(b'L');
        let (cv, kind) = cv_buf[base + i];
        buf[base + i] =
            coerce_invoke_arg_for_descriptor(pd_byte, decode_arg_kind_aware(cv, kind, pd_byte));
    }
    refresh_stale_object_args(shared, &mut buf[..total]);
    Ok(&buf[..total])
}

/// Populate the invoke cache for a given (caller_class, cp_index) pair.
/// Called after the first successful invokestatic to cache everything needed
/// for subsequent calls to bypass the entire invoke chain.
pub(super) fn populate_invoke_cache(
    thread: &mut JvmThread,
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_index: u16,
    is_special: bool,
    // The bytecode offset of the invoke being cached. Part of the cache key
    // under `CRATONVM_INVOKE_CACHE_PC_KEY`; ignored otherwise. It must be the
    // offset of the INVOKE ITSELF, not the pc the interpreter has already
    // advanced past it -- a `put` and a `get` that disagree key one site two
    // ways and it misses forever. See `resolution::pc_key_enabled`.
    site_pc: usize,
) {
    // Check if already cached
    if let Some(existing) = thread
        .invoke_cache.get(caller_class_id, cp_index, is_special, site_pc as u32)
    {
        if crate::runtime::env_cache::dbg_loader_trace() {
            let dbg_relevant = matches!(
                resolve_method_ref(shared, caller_class_id, cp_index),
                Ok((cn, ..)) if cn.contains("RootReference")
            );
            if dbg_relevant {
                let desc = match existing {
                    CachedInvokeTarget::Bytecode { cached, .. } => format!(
                        "Bytecode declaring={:?} class={} {}{}",
                        cached.declaring_class_id,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor
                    ),
                    CachedInvokeTarget::VirtualBytecode {
                        receiver_class_id,
                        cached,
                        ..
                    } => format!(
                        "VirtualBytecode rc={:?} declaring={:?} class={} {}{}",
                        receiver_class_id,
                        cached.declaring_class_id,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor
                    ),
                    CachedInvokeTarget::Native { .. } => "Native".to_string(),
                    CachedInvokeTarget::VirtualNative { .. } => "VirtualNative".to_string(),
                    CachedInvokeTarget::Intrinsic { .. } => "Intrinsic".to_string(),
                    _ => "Other".to_string(),
                };
                eprintln!(
                    "[PIC-ALREADY-CACHED] caller_class_id={:?} cp_index={} is_special={} existing={}",
                    caller_class_id, cp_index, is_special, desc
                );
            }
        }
        return;
    }

    // Resolve the method reference from the constant pool. We need the symbolic
    // owner before consulting the shared promoted cache because loader-aware
    // invokestatic/invokespecial sites may have a per-loader owner that differs
    // from the flat global owner.
    let resolved = match resolve_method_metadata(shared, caller_class_id, cp_index) {
        Ok(resolved) => resolved,
        Err(_) => return,
    };
    let symbolic_class_name = Arc::clone(&resolved.class_name);
    let mut class_name = Arc::clone(&resolved.class_name);
    let method_name = Arc::clone(&resolved.method_name);
    let descriptor = Arc::clone(&resolved.method_descriptor);
    let num_params = resolved.num_params as usize;
    if crate::runtime::env_cache::dbg_loader_trace() && class_name.contains("RootReference") {
        eprintln!(
            "[PIC-ENTRY] caller_class_id={:?} cp_index={} is_special={} class_name(cp)={} method={}{}",
            caller_class_id, cp_index, is_special, class_name, method_name, descriptor
        );
    }
    // JVMS §6.5 super-call redirect (see `invokespecial_owner_class_name`) —
    // this is the PRIMARY invokespecial resolution path (the stackless
    // `thread.invoke_cache`/`shared_resolution` builder consulted by every
    // `execute_invokevirtual_cached(is_special=true)` probe); `execute_invoke_kind`
    // only serves the rare exotic fallback. Must run before EVERY downstream
    // use of `class_name` below (native lookup, loader-owner override,
    // `target_class_id` resolution, the promoted-cache key), so a
    // super-call bridge's cache entry is never built from the wrong class.
    class_name = if is_special {
        invokespecial_owner_class_name(shared, caller_class_id, cp_index, &class_name, &method_name)
    } else {
        class_name
    };

    // `Thread.onSpinWait()` — install the intrinsic entry WITHOUT requiring a
    // registered native, unlike the ordinary intrinsic probe further down.
    //
    // In real-JDK mode there IS no `Thread.onSpinWait` native (the schema-2
    // census lists none), so that probe — which only runs for a method that
    // has one — never sees this site. Answering it inline is
    // behaviour-identical to running it: the JDK body is empty
    // (`@IntrinsicCandidate public static void onSpinWait() {}`) and HotSpot
    // lowers it to a single PAUSE. See the matching arm in
    // `execute_invokestatic_cached`, which skips `safe_native_call` entirely.
    //
    // `java/lang/Thread` is a bootstrap class, so the loader-aware owner
    // resolution this branch skips has nothing to choose between.
    if !is_special
        && !crate::runtime::env_cache::intrinsics_disabled()
        && matches!(
            cratonvm_native_builtins::intrinsics::lookup(&class_name, &method_name, &descriptor),
            Some(cratonvm_native_api::InterpIntrinsic::ThreadOnSpinWait)
        )
    {
        let cm = shared.classes.class_manager.read();
        let gate = match cm.get_loaded_class_id(&class_name) {
            Some(cid) => RedefineGate::snapshot(cm.class_redefine_generation_handle(cid)),
            None => RedefineGate::never_stale(),
        };
        drop(cm);
        let kind = cratonvm_native_api::InterpIntrinsic::ThreadOnSpinWait;
        let param_descs: Arc<[Arc<str>]> = Arc::from(Vec::new());
        // Census: the blindest of the three intrinsic installs — the matching
        // arm in `execute_invokestatic_cached` answers this call site WITHOUT
        // running the callback at all, so a registered `Thread.onSpinWait`
        // native is bypassed completely. There is no row to mark in real-JDK
        // mode (the census lists none, which is this branch's whole premise);
        // there IS one when the builtins registrar has run, and that is the
        // configuration in which the count would otherwise lie.
        mark_intrinsic_cache_bypass_by_triple(
            &shared.natives.native_methods,
            &class_name,
            &method_name,
            &descriptor,
        );
        thread.invoke_cache.put(
            caller_class_id,
            cp_index,
            is_special,
            site_pc as u32,
            CachedInvokeTarget::Intrinsic {
                kind,
                callback: cratonvm_native_builtins::intrinsics::callback_for(kind),
                num_params: 0,
                param_descs,
                return_type: b'V',
                receiver_class_id: None,
                gate,
            },
        );
        return;
    }

    // A ConstantPool Methodref is not a call-site identity: the same
    // `Object.equals(Object)` entry can be used by several bytecode offsets
    // in one method with unrelated receiver shapes.  Keep this highly
    // polymorphic JDK operation out of the CP-indexed monomorphic cache until
    // the cache key carries a bytecode offset as well.  Caching it can reuse
    // Brave's `WeakKey.equals` target for a later `TraceContext.equals` call.
    if !is_special
        && class_name.as_ref() == "java/lang/Object"
        && method_name.as_ref() == "equals"
        && descriptor.as_ref() == "(Ljava/lang/Object;)Z"
    {
        return;
    }

    // The forked Spring test loader has the same identity requirement as the
    // global loader-aware mode. Its private classes may share binary names with
    // application classes, so never build a cache entry from the flat owner.
    let loader_owner_override = if should_use_loader_initiated_resolution(shared, caller_class_id) {
        lookup_loader_initiated(shared, caller_class_id, &class_name)
    } else {
        None
    };

    // T10.4 fast path: reuse a sibling thread's fully-built target unless this
    // static/special site has a loader-local owner. Reusing a flat-global owner
    // here would bypass loader-faithful constructor/super/static dispatch.
    let promoted_key: crate::runtime::lockfree_resolve::PromotedInvokeKey =
        (caller_class_id, cp_index, is_special, None);
    if loader_owner_override.is_none() {
        if let Some(target) = shared
            .classes
            .shared_resolution
            .get_promoted_invoke(&promoted_key)
        {
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, is_special, site_pc as u32, target);
            return;
        }
    }

    // Check if it's a native method.  WP2.4-F1: the staleness gate binds
    // to the *referenced* class — if that class is later redefined to a
    // bytecode body, the cached Native entry must invalidate.  Look up
    // the class_id here rather than synthesizing a never-stale gate so
    // even native-resolved entries participate in JEP 109 invalidation.
    //
    // SyntheticStub yield: a stub-tagged native on a real-protected class
    // whose real bytecode is loaded must NOT be cached (and especially not
    // promoted to the cross-thread cache) — the stub body exists only for
    // stub-phase bootstraps. Without this, a call site whose first
    // resolution goes through this population path permanently pins the
    // stub even though the slow-path dispatch sites correctly yield
    // (observed 2026-07-13 with the since-removed OutputStreamWriter stub
    // surface: `HttpServlet$NoBodyPrintWriter.resetBuffer`'s
    // `new OutputStreamWriter` kept minting encoders with a null `se`
    // while the sibling constructor call site ran the real ctor).
    // Fall through to the bytecode resolution below instead.
    let cached_exact_native = (loader_owner_override.is_none()
        && class_name == symbolic_class_name)
        .then(|| resolved.native_target.zip(resolved.native_kind))
        .flatten();
    let native_for_cache = cached_exact_native
        .or_else(|| {
            shared
                .natives
                .native_methods
                .find_with_kind(&class_name, &method_name, &descriptor)
        })
        .and_then(|(callback, kind)| {
            // The stub-yield predicate IS this site's compatibility verdict —
            // the old `.filter` — so it is handed to §7 as `compat_native_wins`
            // verbatim and `Compatible` mode caches exactly what it cached
            // before.
            let compat_native_wins = !synthetic_stub_kind_should_yield_to_real_bytecode(
                shared,
                &class_name,
                &method_name,
                &descriptor,
                Some(kind),
            );
            match crate::vm::resolve_native_dispatch_wave1(
                crate::vm::DispatchDoor::CachePopulate,
                crate::vm::dispatch_policy(shared),
                &class_name,
                &method_name,
                &descriptor,
                Some((callback, kind)),
                compat_native_wins,
                // The bytecode method is resolved BELOW, only once this native
                // route has declined. §7 step 3's input is therefore unknown
                // here, and `false` keeps `JdkOnly` from changing what gets
                // cached on a fact it has not established.
                //
                // The dial is the exception: it establishes the fact itself,
                // with the same hierarchy walk step 1 uses. Declining to cache
                // an armed class's bridge is what keeps this call site from
                // pinning it -- the resolution below then caches the bytecode
                // target, so the armed answer is the same on the first
                // execution and the millionth. See
                // `jdk_only_dial_yields_to_bytecode`.
                crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                    shared,
                    &class_name,
                    &method_name,
                    &descriptor,
                    kind,
                ),
            ) {
                // Deliberately NOT an error path: populating a call-site cache
                // is not a dispatch. Under `JdkOnly` a refused stub simply does
                // not get cached, and the refusal is raised — once, with a real
                // call site behind it — when something actually tries to run
                // it. Reporting it from here would fire on mere resolution and
                // for call sites that never execute.
                Some(crate::vm::DispatchDecision::Reject(_)) => None,
                Some(decision) => decision.native_callback(),
                None => None,
            }
        });
    // No §4 census increment anywhere in this function: caching a native is not
    // invoking one, and counting it here would inflate every counter by one per
    // call site regardless of whether the site ever ran.
    if let Some(callback) = native_for_cache {
        let Some((callback, native_id, native_kind)) =
            resolve_cached_native_registration(shared, &class_name, &method_name, &descriptor)
        else {
            return;
        };
        let cm = shared.classes.class_manager.read();
        let gate = match cm.get_loaded_class_id(&class_name) {
            Some(cid) => RedefineGate::snapshot(cm.class_redefine_generation_handle(cid)),
            None => RedefineGate::never_stale(),
        };
        drop(cm);
        // Interpreter intrinsic probe (invokestatic only). The cp class IS
        // the declaring class for a static call, so keying the table on
        // `class_name` is correct and needs no receiver guard
        // (`receiver_class_id: None`). We gate on `!is_special` and on
        // `intrinsics::is_static(kind)` so an `invokespecial` site (e.g.
        // `super.hashCode()`) — which targets an instance method and pops
        // a receiver — is never cached here; that case is left to the slow
        // path. `intrinsics_disabled()` is the differential-test off-switch.
        if !is_special && !crate::runtime::env_cache::intrinsics_disabled() {
            if let Some(kind) =
                cratonvm_native_builtins::intrinsics::lookup(&class_name, &method_name, &descriptor)
                    .filter(|k| cratonvm_native_builtins::intrinsics::is_static(*k))
            {
                // Phase 3 — split the descriptor ONCE here so the
                // steady-state dispatch path never re-resolves or re-parses.
                let (pd_vec, _) = split_method_descriptor(&descriptor);
                let param_descs: Arc<[Arc<str>]> =
                    pd_vec.iter().map(|s| Arc::from(s.as_str())).collect();
                let return_type = crate::jit::return_type(&descriptor);
                // Census: this site is about to be bound to a raw `fn` pointer
                // and will never consult the registry again. `native_id` is the
                // row that would have carried the count, resolved a few lines
                // above — declare the slot a floor before the entry exists.
                mark_intrinsic_cache_bypass(&shared.natives.native_methods, native_id);
                let target = CachedInvokeTarget::Intrinsic {
                    kind,
                    callback: cratonvm_native_builtins::intrinsics::callback_for(kind),
                    // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                    num_params: num_params as u16,
                    param_descs,
                    return_type,
                    receiver_class_id: None,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, is_special, site_pc as u32, target);
                return;
            }
        }
        let target = CachedInvokeTarget::Native {
            callback,
            native_id,
            native_kind,
            num_params: num_params as u16, // Widening: parameter count conversion
            gate,
        };
        shared
            .classes
            .shared_resolution
            .insert_promoted_invoke(promoted_key, target.clone());
        thread
            .invoke_cache
            .put(caller_class_id, cp_index, is_special, site_pc as u32, target);
        return;
    }

    // Find the bytecode method. For static/special refs from a loader-private class,
    // the symbolic owner must stay in that loader's namespace; resolving through
    // the flat global store would cache the app-loader method body.
    let target_class_id = match loader_owner_override.or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(&class_name)
    }) {
        Some(id) => id,
        None => return,
    };

    if crate::runtime::env_cache::dbg_loader_trace() && class_name.contains("RootReference") {
        let cm = shared.classes.class_manager.read();
        let caller_loader = cm.get_loader_id(caller_class_id);
        let target_loader = cm.get_loader_id(target_class_id);
        drop(cm);
        eprintln!(
            "[LOADER-TRACE] populate_invoke_cache is_special={} caller_class_id={:?} caller_loader={:?} method={}.{}{} class_name(cp)={} loader_owner_override={:?} target_class_id={:?} target_loader={:?}",
            is_special, caller_class_id, caller_loader, class_name, method_name, descriptor, class_name, loader_owner_override, target_class_id, target_loader
        );
    }

    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(target_class_id) else {
        return;
    };

    // Walk superclass chain to find the declaring class
    let store = &cm.class_store;
    let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
        target_class_id,
        &method_name,
        &descriptor,
        store,
    ) else {
        return;
    };

    // Native-shadow check keyed on the *declaring* class, honored regardless
    // of whether the resolved method is `native` or has a (shadowed) bytecode
    // body. The early CP-class lookup above keys on the symbolic-ref class —
    // which, for an inherited method invoked through a super-class symbolic ref
    // (e.g. Groovy's `GroovyClassLoader.loadClass(String,Z,Z,Z)` calling
    // `super.loadClass(String,Z)`, whose CP ref names `URLClassLoader`/
    // `SecureClassLoader`, not `java.lang.ClassLoader`) — misses the native
    // registered on the declaring class `ClassLoader` (`cl_real_load_class`).
    // Without this, the first call serves the native (slow path) but every
    // cached call runs the real `ClassLoader`/`BuiltinClassLoader` delegation
    // bytecode the VM can't satisfy → spurious `ClassNotFoundException`
    // (e.g. `groovy.grape.GrabAnnotationTransformation` during Groovy's global
    // AST-transform scan, breaking every Groovy compile). Re-applies the
    // 92b7bd80 fix (the original `if method.is_native()`-only gate below was
    // restored by the `cd396a04` "Merge branch 'main' into dev" merge).
    {
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        // Inline SyntheticStub yield (the full helper re-acquires the
        // class-manager read lock, which is already held here): a stub-tagged
        // native on a real-protected declaring class whose resolved method is
        // real bytecode yields — do not cache the stub. `method`/`declaring_id`
        // are the already-resolved real method/class from
        // `find_method_recursive` above.
        let stub_yields =
            shared
                .natives
                .native_methods
                .kind_of(declaring_name, &method_name, &descriptor)
                == Some(cratonvm_native_api::NativeKind::SyntheticStub)
                && real_protected_stub_class(declaring_name)
                && store
                    .get(declaring_id)
                    .is_some_and(|c| !c.origin.is_compatibility_stub())
                && !method.is_native()
                && method.code().is_some();
        if !stub_yields {
            if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
                shared,
                declaring_name,
                &method_name,
                &descriptor,
            ) {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
                drop(cm);
                let target = CachedInvokeTarget::Native {
                    callback,
                    native_id,
                    native_kind,
                    // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                    num_params: num_params as u16,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, is_special, site_pc as u32, target);
                return;
            }
        }
    }

    if method.is_native() {
        // Already handled above, but the method might be native in a superclass
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        if let Some((callback, native_id, native_kind)) =
            resolve_cached_native_registration(shared, declaring_name, &method_name, &descriptor)
        {
            // WP2.4-F1: gate bound to the *declaring* class — that's the
            // class whose method body could be replaced via redefine.
            let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
            drop(cm);
            let target = CachedInvokeTarget::Native {
                callback,
                native_id,
                native_kind,
                num_params: num_params as u16, // Widening: parameter count conversion
                gate,
            };
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke(promoted_key, target.clone());
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, is_special, site_pc as u32, target);
        }
        return;
    }

    let Some(code_attr) = method.code() else {
        return;
    };

    let source_file = class.source_file.as_deref().map(Arc::from);
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");

    let cached = CachedBytecodeMethod {
        declaring_class_id: declaring_id,
        class_name: Arc::from(declaring_class_name),
        method_name: Arc::clone(&method_name),
        method_descriptor: Arc::clone(&descriptor),
        source_file,
        code: crate::runtime::frame::padded_bytecode(&code_attr.code),
        exception_table: Arc::from(code_attr.exception_table.as_slice()),
        max_stack: code_attr.max_stack,
        max_locals: code_attr.max_locals,
        num_params: num_params as u16, // Widening: parameter count conversion
        is_synchronized: method.is_synchronized(),
        is_static: method.is_static(),
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };

    // WP2.4-F1: snapshot the redefine generation BEFORE dropping the
    // class_manager read-lock.  Any subsequent `redefine_class` will
    // bump the same `Arc<AtomicU32>` (because `class_redefine_generation_handle`
    // is now `&self` and shares state via the inner `RwLock<HashMap>`),
    // so the next cache hit will observe the bump and auto-evict.
    let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
    drop(cm);
    let target = CachedInvokeTarget::Bytecode {
        cached: std::sync::Arc::new(cached),
        gate,
    };
    // T10.4 — promote so sibling threads skip the class_manager walk.
    shared
        .classes
        .shared_resolution
        .insert_promoted_invoke(promoted_key, target.clone());
    thread
        .invoke_cache
        .put(caller_class_id, cp_index, is_special, site_pc as u32, target);
}

#[inline]
pub(super) fn cached_static_owner_stale(
    shared: &SharedVm,
    caller_class_id: ClassId,
    cached: &CachedBytecodeMethod,
) -> bool {
    if !should_use_loader_initiated_resolution(shared, caller_class_id) {
        return false;
    }
    lookup_loader_initiated(shared, caller_class_id, cached.class_name.as_ref())
        .is_some_and(|owner_cid| owner_cid != cached.declaring_class_id)
}

/// Fast invokestatic using the invoke cache (stackless dispatch).
/// Returns FramePushed for bytecode (caller updates frame_idx),
/// Handled for native, CacheMiss for fall-through to slow path.
pub(super) fn execute_invokestatic_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    let caller_class_id = thread.frames[frame_idx].class_id;
    let continuation_interpreted = matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    // A --nojit execution can neither enter a cached artifact nor request a
    // new one.  Keep the call-cache dispatch entirely interpreter-only rather
    // than paying its JIT generation probes and hotness atomics on every call.
    let jit_enabled = !crate::runtime::env_cache::disable_jit();

    // Thread-local invoke cache — no locking needed. invokestatic uses
    // is_special=false since static calls never collide cp_index with
    // invokespecial in the same class (different CP entries semantically).
    let ph_t0 = crate::runtime::interpreter::invoke_phases::now();
    let target = match thread.invoke_cache.get(caller_class_id, cp_index, false, pc as u32) {
        Some(t) => t.clone(),
        None => return Ok(CachedCallResult::CacheMiss),
    };
    let ph_t1 = crate::runtime::interpreter::invoke_phases::now();
    crate::runtime::interpreter::invoke_phases::charge(
        crate::runtime::interpreter::invoke_phases::P_IC_LOOKUP,
        ph_t0,
        ph_t1,
    );
    // JVMTI redefine guard (static): never serve a cached native/intrinsic
    // SHADOW for a static method whose declaring class an agent has redefined
    // (e.g. Mockito `mockStatic(X)` woves X's static methods) — evict and
    // re-resolve so the woven bytecode + advice run. Fast-pathed on
    // `any_class_redefined`.
    let any_class_redefined = crate::classloading::any_class_redefined();
    if any_class_redefined
        && matches!(
            &target,
            CachedInvokeTarget::Native { .. } | CachedInvokeTarget::Intrinsic { .. }
        )
    {
        if let Ok((mcn, _, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
            if native_shadow_suppressed_by_redefine(shared, &mcn) {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            }
        }
    }
    if continuation_interpreted && matches!(&target, CachedInvokeTarget::Jit { .. }) {
        thread.invoke_cache.evict(caller_class_id, cp_index, false);
        return Ok(CachedCallResult::CacheMiss);
    }
    if crate::runtime::env_cache::modstatic_dbg() {
        if let Ok((mcn, mn, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
            if mn.as_ref() == "initBootModuleLoader" {
                eprintln!("MODSTATIC: invokestatic_cached HIT {}.{}", mcn, mn);
            }
        }
    }

    // PGO-01: call-site evidence. Placed here, after every early
    // `return Ok(CacheMiss)` above (JVMTI redefine eviction, stale-owner
    // eviction, continuation-interpreted eviction) and right before the
    // dispatch match below — every remaining path through this match
    // actually dispatches (Handled/FramePushed), so this is "the call site
    // fired," not "we merely consulted the cache."
    if crate::jit::profile::is_receiver_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared
            .jit
            .profile_store
            .record_call_site_borrowed(cid, mn, md, pc);
    }

    match target {
        CachedInvokeTarget::Native {
            callback,
            native_id,
            native_kind,
            num_params,
            gate: _,
        } => {
            let Some(callback) = revalidate_cached_native(shared, native_id, callback, native_kind)
            else {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            };
            let (args, method_descriptor) = pop_coerced_invoke_args_static(
                shared,
                caller_class_id,
                cp_index,
                frame_idx,
                thread,
            )?;
            invoke_cached_native_callback_leaf_aware(
                shared,
                thread,
                frame_idx,
                callback,
                native_id,
                &args,
                &method_descriptor,
            )?;
            Ok(CachedCallResult::Handled)
        }
        // Interpreter intrinsic — invokestatic has no receiver guard
        // (`receiver_class_id` is `None` for static-resolved entries).
        // Phase 3: args are popped against the IC-cached `param_descs` and
        // the result coerced with the cached `return_type` — the
        // steady-state path touches no shared lock, no HashMap, and parses
        // no descriptor string.
        CachedInvokeTarget::Intrinsic {
            callback,
            num_params,
            param_descs,
            return_type,
            kind,
            receiver_class_id: _,
            gate: _,
        } => {
            // `Thread.onSpinWait()` is answered WITHOUT a call.
            //
            // Its JDK body is empty (`@IntrinsicCandidate public static void
            // onSpinWait() {}`) and HotSpot lowers it to one PAUSE — 44 ns
            // measured. Routing it through the ordinary intrinsic path costs
            // `safe_native_call` (arg pinning, panic catch, JNI exception
            // drain), and `AbstractQueuedSynchronizer.acquire` spins up to 255
            // rounds before it parks, so that overhead lands on every lock and
            // condition handoff in the VM.
            //
            // The "255 spin rounds" rationale is the ORIGINAL one and it did
            // not survive: measuring the *uncontended* path — which never
            // spins — showed the same cost, so the spin was never the story
            // (`uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md`).
            // The change is kept because the narrower reason is still true:
            // this call site's whole body is empty, so any dispatch cost at
            // all is pure waste.
            //
            // There is nothing to pop (no params, no receiver) and nothing to
            // push (`()V`), so the entire call is the hint below plus the pc
            // advance that returning `Handled` performs. Counted like any
            // other intrinsic dispatch so `CRATONVM_INTRINSIC_STATS=1` can
            // PROVE the fast path fires — the first cut of this change was
            // inert (the entry was never installed) and the timings alone
            // could not tell that apart from "installed but no faster".
            if kind == cratonvm_native_api::InterpIntrinsic::ThreadOnSpinWait {
                INTRINSIC_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::hint::spin_loop();
                return Ok(CachedCallResult::Handled);
            }
            // `Thread.currentThread()` is answered from the thread's own
            // mirror field, without entering `safe_native_call`.
            //
            // Measured (`probes/NativeShapeProbe.java`): a no-argument static
            // native costs ~330-410 ns here against 8.4 ns for an ordinary
            // Java call, and ~98% of that is the funnel — arg pinning, the
            // GC-forwarding barrier, two thread-state transitions, the native
            // ring buffer, `catch_unwind` and the GC-pressure probes. None of
            // it is needed to hand back a field that is already a GC root:
            // there is no argument to pin, nothing here allocates or collects,
            // and it cannot throw.
            //
            // The JDK leans on this constantly — `probes/LockNativeCensusProbe`
            // censuses **two** calls per uncontended `ReentrantLock`
            // lock/unlock pair, alongside two `setExclusiveOwnerThread` and one
            // `Unsafe.compareAndSetInt`.
            //
            // `java_thread_obj` is `None` only before this thread's mirror has
            // been built; that first call falls through to the ordinary path so
            // `current_thread_object`'s allocating slow path still runs. The
            // mirror is a per-thread GC root that the collector remaps, and
            // pushing it onto the operand stack roots it again, with no
            // allocation in between.
            if kind == cratonvm_native_api::InterpIntrinsic::ThreadCurrentThread {
                if let Some(obj) = thread.java_thread_obj {
                    INTRINSIC_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    push_invoke_return_value(
                        &mut thread.frames[frame_idx].stack,
                        Value::Object(Some(obj)),
                    )?;
                    return Ok(CachedCallResult::Handled);
                }
            }
            let mut arg_buf = [Value::Uninitialized; MAX_INTRINSIC_ARGS];
            let args = pop_coerced_invoke_args_intrinsic(
                shared,
                thread,
                frame_idx,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                num_params as usize,
                &param_descs,
                false,
                &mut arg_buf,
            )?;
            invoke_cached_intrinsic(shared, thread, frame_idx, callback, args, return_type)?;
            Ok(CachedCallResult::Handled)
        }
        CachedInvokeTarget::Jit {
            compiled,
            num_params,
            return_type,
            needs_heap,
            cached,
            gate: _,
            supersede_epoch: _,
        } => {
            if cached_static_owner_stale(shared, caller_class_id, &cached) {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            }
            execute_jit_call(
                shared,
                thread,
                frame_idx,
                &compiled,
                num_params,
                return_type,
                needs_heap,
                &cached,
            )
        }
        // Bound BY VALUE, not `ref`: the entry `Arc` cloned out of the invoke
        // cache above is owned by `target`, which dies at the end of this arm,
        // so the frame can take it by MOVE. Bound by reference it had to be
        // cloned again — two atomic refcount bumps per call (and two matching
        // decrements on pop) where one is the minimum, since the cache keeps
        // its own and the frame needs its own.
        CachedInvokeTarget::Bytecode {
            cached,
            gate: entry_gate,
        } => {
            if cached_static_owner_stale(shared, caller_class_id, &cached) {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            }

            // Fast path: check if the method was already JIT-compiled (e.g. by OSR)
            // before going through the invocation counter.
            //
            // T2.2 -- `JitCache::get` hashes class name + method name +
            // descriptor and then re-compares all three with full string
            // equality, and this probe used to run on EVERY interpreted call of
            // a not-yet-compiled method purely to discover whether a background
            // compile had published a body. That is exactly the re-resolution
            // this per-call-site inline cache exists to avoid.
            //
            // It is now epoch-guarded: `jit_cache_generation()` advances on every
            // JIT-cache publication AND every invalidation (see
            // `jit::JitCache::put`/`put_osr`/`invalidate_matching`/`clear_all` --
            // the only mutators), so while this entry's snapshot still equals the
            // live generation the cache content is unchanged since we last looked
            // and missed, and looking again cannot find anything. Steady state is
            // two integer loads plus a compare: no hashing, no string compares.
            let target_redefined = entry_gate.generation > 0;
            if jit_enabled && !target_redefined && !continuation_interpreted {
                // Read the generation BEFORE probing. A publication racing in
                // after the probe leaves us memoizing an older generation, which
                // compares unequal next time and re-probes -- safe. Memoizing a
                // generation NEWER than the probe would be the unsound direction,
                // and this ordering makes it impossible.
                let jit_generation = cratonvm_jit::jit_cache_generation();
                let compiled_probe = if cached.jit_probe_is_current(jit_generation) {
                    None
                } else {
                    let found = shared.jit.jit_cache.read().get(
                        &cached.class_name,
                        &cached.method_name,
                        &cached.method_descriptor,
                        cached.declaring_class_id,
                    );
                    if found.is_none() {
                        cached.record_jit_probe_miss(jit_generation);
                    }
                    found
                };
                if let Some(compiled) = compiled_probe {
                    let ret = cached.return_tag();
                    let heap = compiled.needs_heap();
                    // WP2.4-F1: inherit the gate from the bytecode entry —
                    // both the JIT path and the bytecode path bind to the
                    // same declaring class, so a future redefine bumps the
                    // same counter and invalidates the upgraded JIT entry.
                    let jit_target = CachedInvokeTarget::Jit {
                        compiled: compiled.clone().into(),
                        num_params: cached.num_params,
                        return_type: ret,
                        needs_heap: heap,
                        cached: cached.clone(),
                        gate: entry_gate.clone(),
                        supersede_epoch: crate::classloading::jit_supersede_epoch(),
                    };
                    thread
                        .invoke_cache
                        .put(caller_class_id, cp_index, false, pc as u32, jit_target.clone());
                    if let CachedInvokeTarget::Jit {
                        compiled,
                        num_params,
                        return_type,
                        needs_heap,
                        cached,
                        gate: _,
                        supersede_epoch: _,
                    } = jit_target
                    {
                        return execute_jit_call(
                            shared,
                            thread,
                            frame_idx,
                            &compiled,
                            num_params,
                            return_type,
                            needs_heap,
                            &cached,
                        );
                    }
                }
            }

            // Gate JIT compilation behind an invocation counter (warmup threshold).
            // Pack class_id and a hash of method name+descriptor into a u64 key
            // for cheap per-method counting without allocating strings.
            //
            // T2.5 -- the 31-multiplier byte loops over method name AND full
            // descriptor used to run inline here on every interpreted call.
            // `cached` is `Arc`-shared per call site, so the key is memoized in
            // it (same precedent as `force_native_cache` /
            // `native_callback_cache`) and computed once per call site instead of
            // once per call. The hash is bit-identical, so existing warmup counts
            // keep the same keys.
            let invoc_key = cached.invoc_key();
            // BUG-2: hot-method promotion for short-but-very-hot methods.
            //
            // The old gate `invoc_count >= T && invoc_count % T == 0` fired
            // ONLY at exact multiples of the threshold. Two failure modes hit
            // the all-interpreted call trees in BC's PQC RegressionTest
            // (`Permute.permute`, `ChaChaEngine.chachaCore`,
            // `HashFunctions.hash_n_n`, `Salsa20Engine.processBytes`):
            //   (a) a *single* transient upgrade-gate failure at count T pushed
            //       the next attempt out another whole T calls (2000 → 4000),
            //       multiplying interpreter time; and
            //   (b) these methods never OSR-compile because their per-call
            //       back-edge count is tiny (OSR's `backward_count` resets every
            //       invocation), so the invocation counter is their ONLY path
            //       to the JIT — and the exact-multiple gate made it fragile.
            //
            // Fix: (1) lower the warmup threshold so astronomically-hot short
            // methods promote sooner, and (2) once past the threshold, RETRY on
            // a short stride instead of only at the next full multiple — so a
            // transient compile failure recovers within `JIT_RETRY_STRIDE`
            // calls, not another full threshold. The persistent
            // `increment_invocation` count means the bar is crossed even from
            // the interpreted-call-tree case, and a successful upgrade rewrites
            // the invoke cache to `Jit`, so this counting block stops being
            // reached and there is no ongoing re-spam. Normal warmup is
            // preserved: cold methods still wait for `JIT_INVOCATION_THRESHOLD`
            // calls before any compile attempt.
            // Warmup threshold (default 500), overridable via CRATONVM_JIT_THRESHOLD.
            // Raising it keeps short-lived / call-heavy code interpreted (HotSpot's
            // interpreter-first behaviour) instead of paying CratonVM's
            // currently-slower JIT'd dispatch for code that never amortizes the
            // switch; genuinely-hot compute loops still cross it and compile.
            let jit_invocation_threshold = crate::runtime::env_cache::jit_invocation_threshold();
            /// Re-attempt stride once a method is past the warmup threshold but
            /// not yet successfully compiled. Small so a transient upgrade-gate
            /// failure is retried within a few hundred calls rather than after
            /// another full warmup threshold.
            const JIT_RETRY_STRIDE: u32 = 64;
            let invoc_count = if jit_enabled {
                shared.jit.profile_store.increment_invocation(invoc_key)
            } else {
                0
            };
            // Fire on the first crossing of the threshold, then re-attempt every
            // `JIT_RETRY_STRIDE` calls until the upgrade succeeds (after which
            // the invoke cache routes through JIT and this block is bypassed).
            let past_threshold = jit_enabled && invoc_count >= jit_invocation_threshold;
            let should_attempt = past_threshold
                && (invoc_count == jit_invocation_threshold
                    || (invoc_count - jit_invocation_threshold) % JIT_RETRY_STRIDE == 0);
            if should_attempt && !target_redefined && !continuation_interpreted {
                // Consult tiered compilation manager for recommended tier
                let tiered_key = crate::jit::tiered::MethodKey::new(
                    cached.class_name.as_ref(),
                    cached.method_name.as_ref(),
                    cached.method_descriptor.as_ref(),
                );
                // wire-tiered-manager: OFF-THREAD codegen for the invocation
                // tier-up trigger. **DEFAULT-ON as of Step 7** ("retire the
                // single fixed-threshold inline path"); `CRATONVM_BG_COMPILE=0`
                // is the opt-out safety net.
                //
                //  * On (default) — start the background compile thread once
                //    (idempotent), then ENQUEUE-ONLY: the tiered manager's
                //    `on_method_invocation` pushes a CompilationTask at the
                //    recommended tier and the worker compiles it off the mutator
                //    (publishing into `shared.jit.jit_cache`). The mutator does NOT
                //    compile inline; it keeps interpreting until the worker
                //    publishes, at which point the `jit_cache` fast-path at the top
                //    of the `Bytecode` arm flips this call site to `Jit`. (The
                //    eager *first-call* single-pass compile in `fn execute` is a
                //    separate quick first tier and still runs; this governs the
                //    invocation-counted re-tiering + OSR.)
                //  * Off (`=0`) — never start the worker; keep the historical
                //    inline `try_jit_upgrade_with_gate` path EXACTLY as before
                //    (the safety net while the off-thread pipeline soaks on the
                //    gauntlet).
                let bg_compile_on = crate::runtime::env_cache::bg_compile();
                if bg_compile_on {
                    // Start the off-thread compile worker once (idempotent). See
                    // `ensure_bg_compiler_started` for the closure / GC rationale.
                    ensure_bg_compiler_started(shared);
                }
                // Pass the REAL per-method invocation count — this hook runs
                // only at stride boundaries, and the manager's historical
                // `+= 1` counting deflated its hotness view 64x (first C1
                // recommendation at ~threshold + 64×c1_threshold real calls).
                let recommended_tier = shared
                    .jit
                    .tiered_manager
                    .on_method_invocation_observed(&tiered_key, invoc_count as u64);
                if let Some(tier) = recommended_tier {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] tiered-enqueue {}.{}{} tier={:?} invoc_count={} bg={}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            tier,
                            invoc_count,
                            bg_compile_on,
                        );
                    }
                }
                // When background compilation is ON the mutator does NOT compile
                // inline — the worker owns codegen. Skip straight to interpreted
                // execution; a later call picks up the published JIT entry via the
                // `jit_cache` fast-path above.
                if !bg_compile_on {
                    // WP2.4-F1: pass the bytecode entry's gate to inherit
                    // the staleness binding — the JIT'd body executes the same
                    // declaring class, so a future `redefine_class` must
                    // invalidate this JIT entry too.
                    let upgrade_result =
                        try_jit_upgrade_with_gate(shared, &cached, entry_gate.clone());
                    if upgrade_result.is_none() && crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] upgrade-FAIL {}.{}{} invoc_count={}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            invoc_count
                        );
                    }
                    if let Some(jit_target) = upgrade_result {
                        // Upgrade cache entry to Jit for future calls
                        thread.invoke_cache.put(
                            caller_class_id,
                            cp_index,
                            false,
                            pc as u32,
                            jit_target.clone(),
                        );
                        // Execute via JIT right now
                        if let CachedInvokeTarget::Jit {
                            compiled,
                            num_params,
                            return_type,
                            needs_heap,
                            cached,
                            gate: _,
                            supersede_epoch: _,
                        } = jit_target
                        {
                            return execute_jit_call(
                                shared,
                                thread,
                                frame_idx,
                                &compiled,
                                num_params,
                                return_type,
                                needs_heap,
                                &cached,
                            );
                        }
                    }
                } // end !bg_compile_on inline-upgrade path
            } // end invocation threshold check

            // Fallback: interpreted execution
            // Check stack overflow before pushing frame
            if thread.frames.len() >= shared.config.max_stack_depth {
                dump_stack_on_soe(thread);
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::StackOverflowError,
                )));
            }

            // Pop args into stack-allocated buffer (avoids Vec allocation).
            // Decode each slot with its parameter descriptor (bit-exact): the
            // prior `pop_unchecked()` → `to_value()` decoded a category-2 long
            // arg whose NaN-box bit pattern collides with a tagged sub-tag
            // (e.g. `0xFFFC_…`, a BC safegcd accumulator) as `Value::Int`,
            // dropping the high bits before it reached the callee's locals.
            // The non-cached `execute_invokestatic` path already decodes this
            // way. See gaps/bc-ec-mod-mododdinverse-investigation.md.
            // 8, not 16. `args_buf` is `[Value; MAX_INLINE_ARGS]` and `Value`
            // is 16 bytes, so at 16 this initialised 256 BYTES on every call
            // regardless of how many arguments the callee actually takes. The
            // phase instrument charged 60.9 cyc/call to argument handling on a
            // workload of nothing but ZERO-argument calls, which is what that
            // initialisation costs. Eight covers essentially every method and
            // wider ones still spill to `args_vec` exactly as before.
            const MAX_INLINE_ARGS: usize = 8;
            let num_params = cached.num_params as usize; // Widening: parameter count conversion
            let ph_t2 = crate::runtime::interpreter::invoke_phases::now();
            crate::runtime::interpreter::invoke_phases::charge(
                crate::runtime::interpreter::invoke_phases::P_GUARDS,
                ph_t1,
                ph_t2,
            );
            // A zero-argument call builds no buffer and scans no descriptor at
            // all. `invokestatic` of a no-arg method is a very common shape and
            // it was paying for both. `args_buf` is deliberately declared
            // WITHOUT an initialiser so the 128 bytes are written only on the
            // path that uses them.
            let mut args_buf: [Value; MAX_INLINE_ARGS];
            let mut args_vec: Vec<Value> = Vec::new();
            let args_slice: &mut [Value] = if num_params == 0 {
                &mut []
            } else if num_params <= MAX_INLINE_ARGS {
                // ONE forward scan; the per-argument form rescanned from `(` each time.
                let param_tags = ParamTags::for_method(&cached);
                args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
                for i in (0..num_params).rev() {
                    args_buf[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(
                            param_tags.get(&cached.method_descriptor, i),
                        )?;
                }
                &mut args_buf[..num_params]
            } else {
                let param_tags = ParamTags::for_method(&cached);
                args_vec.resize(num_params, Value::Uninitialized);
                for i in (0..num_params).rev() {
                    args_vec[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(
                            param_tags.get(&cached.method_descriptor, i),
                        )?;
                }
                &mut args_vec
            };
            refresh_stale_object_args(shared, args_slice);

            if let Some(res) = intercept_force_registered_native_cached(
                shared, thread, frame_idx, &cached, args_slice,
            ) {
                return res;
            }

            // Acquire monitor for synchronized methods
            let monitor_obj: Option<ObjectRef> = if cached.is_synchronized {
                let obj = if cached.is_static {
                    // JVMS §2.11.10: static-synchronized monitor is the Class mirror
                    // (same object as ldc class / synchronized(X.class) / X.class.wait).
                    get_or_create_class_mirror(shared, cached.declaring_class_id)
                } else {
                    match args_slice.first() {
                        Some(Value::Object(Some(obj_ref))) => *obj_ref,
                        _ => return Ok(CachedCallResult::CacheMiss),
                    }
                };
                Some(crate::vm::monitor_enter_synchronized_method(
                    shared, thread, obj, args_slice,
                ))
            } else {
                None
            };

            // Push frame — caller handles execution via stackless loop
            // T10.7 — refill per-thread pool from the shared VM-wide VecPool
            // if it's empty so we reuse the promoted allocation.
            thread.refill_pools_from_shared(
                &shared.mem.operand_stack_pool,
                &shared.mem.tag_pool,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                cached.max_locals as usize,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                (cached.max_stack as usize).max(16) + 8,
            );
            let ph_t3 = crate::runtime::interpreter::invoke_phases::now();
            crate::runtime::interpreter::invoke_phases::charge(
                crate::runtime::interpreter::invoke_phases::P_ARGS,
                ph_t2,
                ph_t3,
            );
            let mut frame = Frame::new_pooled_cached(
                cached,
                args_slice,
                &mut thread.locals_pool,
                &mut thread.stacks_pool,
            );
            frame.monitor_on_exit = monitor_obj;
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[FRAME_PUSH/stackless_cached] depth={} {}.{}{}",
                    thread.frames.len(),
                    frame.class_name(),
                    frame.method_name(),
                    frame.method_descriptor()
                );
            }
            let ph_t4 = crate::runtime::interpreter::invoke_phases::now();
            crate::runtime::interpreter::invoke_phases::charge(
                crate::runtime::interpreter::invoke_phases::P_FRAME_BUILD,
                ph_t3,
                ph_t4,
            );
            push_frame_and_fire_entry(shared.vm_identity, thread, frame);
            let ph_t5 = crate::runtime::interpreter::invoke_phases::now();
            crate::runtime::interpreter::invoke_phases::charge(
                crate::runtime::interpreter::invoke_phases::P_PUSH,
                ph_t4,
                ph_t5,
            );
            crate::runtime::interpreter::invoke_phases::count_call();
            Ok(CachedCallResult::FramePushed)
        }
        _ => Ok(CachedCallResult::CacheMiss),
    }
}

/// Resolve `java/lang/String`'s instance-field layout (slot indices of
/// `value`, `coder`, `hash`) for the JIT String call-site intrinsics
/// (`length`/`charAt`/`isEmpty`/`hashCode`/`equals`/`compareTo`/`indexOf`).
///
/// String extends Object, which has no instance fields, so `find_own_field`'s
/// indices match the slot indices used by `jit_getfield`. `coder` is optional —
/// the legacy synthetic `char[]`-backed String has none, in which case
/// `StringFieldLayout::new` records `has_coder = false` and the `coder`-dependent
/// intrinsics bail to normal native dispatch. Returns `None` (all String
/// intrinsics bail to dispatch) when String is not yet loaded or lacks the
/// mandatory `value` / `hash` fields. Cheap enough to call once per compilation.
pub(super) fn resolve_string_field_layout(
    shared: &SharedVm,
) -> Option<cratonvm_jit::StringFieldLayout> {
    let cm = shared.classes.class_manager.read();
    let string_id = cm.find_bootstrap_class_by_name("java/lang/String")?;
    let class = cm.get_class(string_id)?;
    let (value_idx, _) = class.find_own_field("value")?;
    let (hash_idx, _) = class.find_own_field("hash")?;
    let coder_idx = class.find_own_field("coder").map(|(idx, _)| idx);
    // `string_id` doubles as the ObjectHeader class id used to guard
    // `java/lang/CharSequence` accessor call sites (the receiver must be a
    // real String for the inline String-layout decode to be sound).
    Some(cratonvm_jit::StringFieldLayout::new(
        value_idx,
        coder_idx,
        hash_idx,
        string_id.as_u32(),
    ))
}

/// Backward branch count threshold before triggering OSR compilation.
pub(super) const OSR_THRESHOLD: u32 = 1_000;

/// Try On-Stack Replacement: compile the current method and enter JIT mid-execution.
///
/// Returns `Some(Option<Value>)` if OSR succeeds (the method completed via JIT),
/// or `None` if OSR is not possible (method not JIT-compatible, compilation failed, etc.).
/// wire-tiered-manager Step 5 (precise background OSR): compile (or reuse) an
/// OSR-enterable artifact for `(class, method, descriptor)` and publish it into
/// `shared.jit.jit_cache`. Extracted verbatim from `try_osr`'s former inline compile
/// closure so it can run **off the mutator** on the background compile worker
/// (which holds no live frame): every input is method metadata, not runtime frame
/// state. The mutator's `try_osr` then does the live-frame entry on the returned
/// artifact. `entry_pc` only selects which back-edge the *reuse* probe checks
/// (`can_osr_enter`); the compile itself is entry-pc-independent (the artifact
/// supports OSR entry at every loop header it emits).
///
/// GC-STW-safety on the worker: same discipline as `try_jit_compile_callee_slow`
/// (see its doc) — every `class_manager` lock is read out and dropped before any
/// blocking / allocating call (`load_class_concurrent`, eager callee compile), and
/// `PENDING_COMPACT_FIELD_INFO` is thread-local so the worker stages its own.
#[allow(clippy::too_many_arguments)]
/// Allocate the dynamic-dispatch cache pair consumed by both x64 compilation
/// entry points owned by the interpreter.
///
/// Method-entry compilation in `cratonvm-jit` has always supplied these slots,
/// but the interpreter's early-compile and OSR paths passed empty vectors. That
/// backend skew forced every virtual/interface operation in a hot OSR loop
/// through `jit_invoke_dispatch`, even though x64 already had MIC codegen.
pub(super) fn allocate_dynamic_dispatch_slots(
    invoke_kind: u8,
    pc: usize,
    mic_slots: &mut Vec<(usize, *const crate::jit::JitMICSlot)>,
    owned_mic_slots: &mut Vec<Box<crate::jit::JitMICSlot>>,
    pic_slots: &mut Vec<(usize, *const crate::jit::JitPICSlot)>,
    owned_pic_slots: &mut Vec<Box<crate::jit::JitPICSlot>>,
) {
    if !matches!(invoke_kind, 0 | 2) {
        return;
    }

    let mic = Box::new(crate::jit::JitMICSlot::new());
    let pic = Box::new(crate::jit::JitPICSlot::new());
    let mic_ptr: *const crate::jit::JitMICSlot = &*mic;
    let pic_ptr: *const crate::jit::JitPICSlot = &*pic;
    owned_mic_slots.push(mic);
    owned_pic_slots.push(pic);
    mic_slots.push((pc, mic_ptr));
    pic_slots.push((pc, pic_ptr));
}

#[cfg(test)]
mod dynamic_dispatch_slot_tests {
    // `super::` here is the enclosing `invoke` module, which is where this
    // helper now lives.
    use super::allocate_dynamic_dispatch_slots;

    #[test]
    fn caches_virtual_and_interface_sites_but_not_static_or_special_sites() {
        for (kind, expected) in [(0, 1), (1, 0), (2, 1), (3, 0)] {
            let mut mic = Vec::new();
            let mut owned_mic = Vec::new();
            let mut pic = Vec::new();
            let mut owned_pic = Vec::new();
            allocate_dynamic_dispatch_slots(
                kind,
                27,
                &mut mic,
                &mut owned_mic,
                &mut pic,
                &mut owned_pic,
            );
            assert_eq!(mic.len(), expected);
            assert_eq!(owned_mic.len(), expected);
            assert_eq!(pic.len(), expected);
            assert_eq!(owned_pic.len(), expected);
            if expected == 1 {
                assert_eq!(mic[0].0, 27);
                assert_eq!(pic[0].0, 27);
            }
        }
    }
}

/// The census half of the intrinsic inline cache: `mark_intrinsic_cache_bypass`
/// and `mark_intrinsic_cache_bypass_by_triple`, which are what stop
/// `--dump-native-registry`'s `invocations` column from claiming a total it
/// cannot have.
///
/// Recorded in
/// `docs/known-issues/jdk-only/G42-1-the-intrinsic-cache-and-the-1999-20260817.md`.
#[cfg(test)]
mod intrinsic_census_tests {
    use super::{
        mark_intrinsic_cache_bypass, mark_intrinsic_cache_bypass_by_triple, OSR_THRESHOLD,
    };
    use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodId, NativeMethodRegistry};
    use cratonvm_types::error::MethodCallResult;
    use cratonvm_types::Value;

    fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }

    fn dummy_native_2(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(42)))
    }

    /// The core contract: marking turns a claimed total into an admitted floor
    /// WITHOUT disturbing the number, is per slot, and is idempotent.
    ///
    /// Idempotence is not decoration. `populate_invoke_cache` runs once per
    /// (caller class, cp index) call site and there are many call sites per
    /// triple, plus the promoted-resolution cache re-publishes entries across
    /// threads — so this mark fires repeatedly for the same slot by design.
    #[test]
    fn marking_an_intrinsic_install_turns_its_row_into_a_floor() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Intrinsic, |r| {
            r.register("java/lang/Math", "abs", "(I)I", dummy_native);
            r.register(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
                dummy_native_2,
            );
        });
        let abs = registry
            .resolve_id("java/lang/Math", "abs", "(I)I")
            .expect("registered");
        // The control that stayed exact in every configuration G33-1 measured.
        let control = registry
            .resolve_id(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
            )
            .expect("registered");

        // Before any cache fill both rows claim to be totals.
        assert_eq!(registry.invocations_complete(abs), Some(true));
        assert_eq!(registry.slots_with_incomplete_invocations(), 0);

        // The first call at a site goes down the counted path; the cache fill
        // then binds every later call to a raw `fn` pointer.
        registry.record_invocation(abs);
        mark_intrinsic_cache_bypass(&registry, abs);

        assert_eq!(
            registry.invocations_of_id(abs),
            Some(1),
            "marking must not disturb the tally — the floor is the number a \
             reader falls back on, and this is the measured `Math.abs` = 1"
        );
        assert_eq!(registry.invocations_complete(abs), Some(false));
        assert_eq!(
            registry.invocations_complete(control),
            Some(true),
            "the bit is per slot: an intrinsic install must not cast doubt on \
             a native that is still dispatched through the counted path"
        );
        assert_eq!(registry.slots_with_incomplete_invocations(), 1);

        // Many call sites, one slot.
        mark_intrinsic_cache_bypass(&registry, abs);
        mark_intrinsic_cache_bypass(&registry, abs);
        assert_eq!(registry.slots_with_incomplete_invocations(), 1);
        assert_eq!(registry.invocations_of_id(abs), Some(1));

        // And the doubt has to survive the trip into the census, which is the
        // only place a reader ever sees it.
        let census = registry.census();
        let abs_row = census
            .iter()
            .find(|r| r.class == "java/lang/Math" && r.owns_slot)
            .expect("slot owner");
        assert!(!abs_row.invocations_complete);
        let control_row = census
            .iter()
            .find(|r| r.class == "java/lang/System" && r.owns_slot)
            .expect("slot owner");
        assert!(control_row.invocations_complete);
    }

    /// `dispatch_virtual.rs` reaches its intrinsic through the CLASS STORE and
    /// marks the **declaring** class's row. Pin that choice: `Object.hashCode`
    /// and a subclass's own `hashCode` are two different registry slots, and
    /// marking the wrong one would leave the row that actually loses the calls
    /// still claiming to be a total — a silent failure in the one direction
    /// this instrument must never err in.
    #[test]
    fn the_by_triple_helper_marks_only_the_named_classes_row() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("java/lang/Object", "hashCode", "()I", dummy_native);
            r.register("p/Sub", "hashCode", "()I", dummy_native_2);
        });
        let declaring = registry
            .resolve_id("java/lang/Object", "hashCode", "()I")
            .expect("registered");
        let other = registry
            .resolve_id("p/Sub", "hashCode", "()I")
            .expect("registered");

        assert!(
            mark_intrinsic_cache_bypass_by_triple(&registry, "java/lang/Object", "hashCode", "()I"),
            "a triple with a registry row reports that it marked one"
        );
        assert_eq!(registry.invocations_complete(declaring), Some(false));
        assert_eq!(
            registry.invocations_complete(other),
            Some(true),
            "same method name and descriptor, different class, different slot"
        );
        assert_eq!(registry.slots_with_incomplete_invocations(), 1);
    }

    /// A `false` from the by-triple helper is the NORMAL case for several live
    /// intrinsics and is not a failure.
    ///
    /// `String.charAt` and `String.length` are registered nowhere in the
    /// `--jdk-only` binary — G33-1 §2 checked the dump directly and found them
    /// *absent*, not zero-valued — and `Thread.onSpinWait` has no row in
    /// real-JDK mode either, which is the premise of the branch that installs
    /// it. A triple with no row has no `invocations` cell that could mislead
    /// anyone, so there is nothing to mark and nothing to report.
    ///
    /// It must also not panic: this runs on the inline-cache fill path for
    /// every call in the VM.
    #[test]
    fn a_triple_with_no_registry_row_is_a_silent_no_op() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("java/lang/Object", "hashCode", "()I", dummy_native);
        });

        assert!(!mark_intrinsic_cache_bypass_by_triple(
            &registry,
            "java/lang/String",
            "charAt",
            "(I)C"
        ));
        assert!(!mark_intrinsic_cache_bypass_by_triple(
            &registry,
            "java/lang/Thread",
            "onSpinWait",
            "()V"
        ));
        assert_eq!(
            registry.slots_with_incomplete_invocations(),
            0,
            "an absent triple must not manufacture doubt on some other slot"
        );

        // The empty-registry case: `register_collections_natives` and
        // `register_essential_natives` are separate registrars, so a cache fill
        // can run against a registry that has not been populated yet.
        let empty = NativeMethodRegistry::new();
        assert!(!mark_intrinsic_cache_bypass_by_triple(
            &empty,
            "java/lang/Math",
            "abs",
            "(I)I"
        ));
        assert_eq!(empty.slots_with_incomplete_invocations(), 0);

        // And the foreign/stale-handle contract, inherited from
        // `record_invocation`: ignored, never a panic.
        mark_intrinsic_cache_bypass(&registry, NativeMethodId::from_u32(9_999));
        assert_eq!(registry.slots_with_incomplete_invocations(), 0);
    }

    /// Pins the constant the measured **1,999** is derived from, so that a
    /// future retune of the loop threshold makes the record's headline number
    /// visibly stale instead of quietly wrong.
    ///
    /// MEASURED on `9ae371468` (G42-1 §2): a `Method.invoke` loop in `main`
    /// reports `invocations: 1,999` in the JIT arm and 99,999 under `--nojit`.
    /// The arithmetic is
    /// `OSR_THRESHOLD × 2 (the first backoff retry) × 1 call per iteration − 1
    /// (the reflection deficit at the first call)`. It is a **transition
    /// count** — how many calls were made before the compiled artifact was
    /// installed — not a cap: with four invoke sites in one loop body the same
    /// run reports **7,999**, four times as many, at the same back-edge count.
    #[test]
    fn the_measured_1999_is_two_osr_thresholds_of_interpreted_calls() {
        assert_eq!(OSR_THRESHOLD, 1_000);
        let first_retry_backedges = OSR_THRESHOLD * 2;
        assert_eq!(first_retry_backedges - 1, 1_999);
        // Four call sites in one loop body: same back-edges, four times the
        // calls, and the measurement scaled exactly.
        assert_eq!(first_retry_backedges * 4 - 1, 7_999);
    }
}
