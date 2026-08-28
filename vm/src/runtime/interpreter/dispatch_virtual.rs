// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `invokevirtual` / `invokeinterface` dispatch and the inline-cache consult.
//!
//! Three tiers, fastest first, and the ordering between them is the whole
//! design:
//!
//! * `try_execute_cached_trivial_instance_getter` — a monomorphic getter
//!   answered without building a frame at all.
//! * `execute_invokevirtual_vtable_fast` — a vtable index resolved once and
//!   reused while the receiver's class is unchanged.
//! * `execute_invokevirtual_cached` — the general cached path, and the one
//!   that has to re-check everything the faster two assumed.
//!
//! Every tier has to answer the same question before it may run: does a
//! registered native shadow this virtual method for this receiver's class?
//! That is what `vtable_native_shadow_cache_key` and
//! `native_override_for_cached_reflect_invoke` are for, and it is why a
//! cache here is keyed on more than the call site — the policy that decides
//! the answer lives in `interpreter/native_override.rs` and can differ per
//! subclass.

use super::*;

// ---------------------------------------------------------------------------
// invokestatic
// ---------------------------------------------------------------------------
//
// Static dispatch, its call-site cache, the staleness check that survives a
// redefinition, and the intrinsic substitution point:
// `interpreter/dispatch_static.rs`.



// ---------------------------------------------------------------------------
// The JIT bridge
// ---------------------------------------------------------------------------
//
// Compile requests, OSR, entering and leaving compiled code, and the probes
// that decide what a compile may assume: `interpreter/jit_bridge.rs`.


/// T10.9.A — VtableManager fast-path for invokevirtual / invokeinterface.
///
/// Consulted as **fast path 0** ahead of the per-thread `invoke_cache`
/// and the shared-resolution promoted-cache. A hit here bypasses
/// `class_manager.read()` entirely: the vtable carries a fully-built
/// `Arc<CachedBytecodeMethod>` populated at class-link time.
///
/// Ordering of reads:
///   1. `thread.invoke_cache` — cheapest, purely thread-local. If hit,
///      the VtableManager read is skipped.
///   2. The receiver object itself (peek; may be null → NPE).
///   3. `shared.classes.resolution_cache.read()` — may yield the
///      `(method_name, descriptor, num_params)` triple without
///      touching `class_manager`.
///   4. `shared.classes.vtable_manager.read()` → `resolve_virtual_slot`.
///   5. Frame push using the entry's `resolved_method` Arc.
///
/// Returns:
///   - `Ok(FramePushed)` on bytecode dispatch (interpreter must advance
///     `frame_idx`).
///   - `Ok(Handled)` on native dispatch (result already pushed).
///   - `Ok(CacheMiss)` when the vtable slot is empty/unresolved, the
///     receiver class has no installed vtable, the resolution-cache
///     doesn't yet have the method-ref, or the entry is marked
///     `is_native` (native path not handled here — invoke_cache will
///     fill that in on a later call).
///   - `Err(_)` propagates any NPE/StackOverflowError.
///
/// After a successful dispatch this function populates the thread-local
/// `invoke_cache` so subsequent invocations from the same caller class
/// take the faster monomorphic inline-cache path.
///
/// Bounds: `class_id` out-of-range and `slot` out-of-range both return
/// `CacheMiss` via `VtableManager::resolve_virtual_slot`'s own bounds
/// checks (see `vtable.rs` tests).
pub(super) const NATIVE_SHADOW_CACHE_MAX_ENTRIES: usize = 8192;

#[inline]
pub(super) fn vtable_native_shadow_cache_key(
    receiver_class_id: ClassId,
    redefine_fingerprint: u64,
    method_name: &str,
    method_descriptor: &str,
) -> (u32, u64, u64, u64) {
    (
        receiver_class_id.as_u32(),
        redefine_fingerprint,
        crate::runtime::fx_collections::fx_hash_str(method_name),
        crate::runtime::fx_collections::fx_hash_str(method_descriptor),
    )
}

#[inline]
pub(super) fn remember_vtable_native_shadow(
    thread: &mut JvmThread,
    key: Option<(u32, u64, u64, u64)>,
    verdict: bool,
) {
    if let Some(key) = key {
        if thread.native_shadow_cache.len() >= NATIVE_SHADOW_CACHE_MAX_ENTRIES {
            thread.native_shadow_cache.clear();
        }
        thread.native_shadow_cache.insert(key, verdict);
    }
}

/// Would a native registered for this triple ACTUALLY run, or is it one the
/// arbitration always yields to the real JDK body?
///
/// # Why the fast paths cannot just ask `find(..).is_some()`
///
/// [`execute_invokevirtual_vtable_fast`] bails to the slow, fully name-keyed
/// dispatch route the moment ANY native is registered for the triple, and
/// remembers that verdict in `thread.native_shadow_cache` — so the bail is
/// permanent for that call site. But a `SyntheticStub` on a
/// [`real_protected_stub_class`] whose real bytecode is loaded LOSES the
/// arbitration at every dispatch site
/// ([`synthetic_stub_yields_with_cm`]). The call site therefore paid for the
/// slow route and got exactly the bytecode a cached entry would have given it.
///
/// `populate_virtual_invoke_cache` already asks this question before publishing
/// a `VirtualNative` target — it "publishes nothing" and falls through to the
/// bytecode target. That fix could never take effect, because the fast path
/// above it returned `CacheMiss` before population was ever reached.
///
/// MEASURED 2026-08-22 on `perf/webclient-reactive-20260821`, 200k-iteration
/// loops, HotSpot 25 control in brackets:
///
/// | call | CratonVM | HotSpot |
/// |---|---:|---:|
/// | `Instant.getNano()` (native, always yielded) | 1627 ns | 2.3 ns |
/// | `Instant.compareTo()` (same class, no native) | 39 ns | 2.9 ns |
/// | `ReentrantLock.lock()`+`unlock()` | 1760 ns | 11.9 ns |
/// | `AtomicBoolean.compareAndSet()` | 4325 ns | 5.5 ns |
/// | `LinkedBlockingDeque.peek()` | 1820 ns | 11.3 ns |
/// | `StringJoiner.length()` | 2924 ns | 5.2 ns |
/// | `Duration.getSeconds()` (control, no native) | 24 ns | 2.3 ns |
///
/// The 40-70x spread between two methods of the SAME class is the lost inline
/// cache entry and nothing else. `java.time.Instant.now()` alone runs 1_240_308
/// times in one `WebClientIntegrationTests` run.
///
/// Returning `true` reproduces the previous behaviour exactly for every triple
/// that is not a yielded stub, so nothing outside the twelve-class allow-list
/// changes.
fn registered_native_actually_shadows(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // Same ordering as `synthetic_stub_kind_should_yield_to_real_bytecode`:
    // both cheap terms short-circuit before anything touches the class store.
    // Spelled out here rather than delegating to the shared
    // `registered_native_will_run`, because this site runs UNDER the
    // class-manager read guard the caller already holds and that sibling takes
    // its own `read()` - a nested read is the writer-starvation trap
    // `synthetic_stub_yields_with_cm` was split out to avoid.
    let kind = shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor);
    if kind != Some(cratonvm_native_api::NativeKind::SyntheticStub)
        || !real_protected_stub_class(class_name)
    {
        return true;
    }
    !synthetic_stub_yields_with_cm(cm, class_name, method_name, descriptor)
}

#[inline]
pub(super) fn execute_invokevirtual_vtable_fast(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    site_pc: usize,
    is_interface: bool,
) -> Result<CachedCallResult, MethodCallFailed> {
    dbg_invoke_stats_record(2);
    let caller_class_id = thread.frames[frame_idx].class_id;

    // Step 1 — already covered by the caller (execute_invokevirtual_cached
    // runs first on every call site). This function is only invoked on
    // its miss path, so the thread-local cache is cold for this key.

    // Step 2 — resolve the method reference from the caller's constant
    // pool. We want the `(name, descriptor, num_params)` triple without
    // acquiring `class_manager.read()`; that's only possible via the
    // already-populated `resolution_cache`. On a cold cache we return
    // `CacheMiss` and let the slow path populate it.
    let (method_class_name, method_name, method_descriptor, num_params_slots) = {
        let rc = shared.classes.resolution_cache.read();
        match rc.get_method(caller_class_id, cp_index) {
            Some(rm) => (
                Arc::clone(&rm.class_name),
                Arc::clone(&rm.method_name),
                Arc::clone(&rm.method_descriptor),
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                rm.num_params as usize,
            ),
            None => return Ok(CachedCallResult::CacheMiss),
        }
    };

    // Force this call site through the slow dispatcher (see the matching
    // comment in `execute_invoke_kind`) rather than a fast-path vtable/cache
    // hit — empirically required to keep
    // WebFluxManagementChildContextConfigurationIntegrationTests from
    // hanging even with the registered native override in place.
    if crate::runtime::env_cache::loader_aware_resolution()
        && method_class_name.as_ref()
            == "org/springframework/core/annotation/MergedAnnotation$Adapt"
        && method_name.as_ref() == "isIn"
        && method_descriptor.as_ref()
            == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    // An array-typed call site resolves to `java.lang.Object`'s method table
    // statically (JVMS §4.4.1 — an array class declares no methods), so there
    // is nothing for a receiver-class vtable to answer. The array-kind guard
    // further down only covers a receiver whose header still SAYS array; a
    // reclaimed-and-re-served block does not, and this path would then resolve
    // the site against the occupant's vtable — the `"[J".clone()` ->
    // `java.lang.Thread.clone` route of
    // `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame`.
    // Cede to the slow dispatcher, which decides from the call site.
    if method_class_name.starts_with('[') {
        return Ok(CachedCallResult::CacheMiss);
    }

    // Step 3 — peek the receiver. The receiver sits `num_params_slots`
    // down the operand stack from the top.
    // `cp_index` alone is not a call-site identity. Do not install a
    // monomorphic target for a shared `Object.equals` Methodref; the same
    // entry can be used at receiver-polymorphic bytecode offsets.
    if method_class_name.as_ref() == "java/lang/Object"
        && method_name.as_ref() == "equals"
        && method_descriptor.as_ref() == "(Ljava/lang/Object;)Z"
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    let num_params = num_params_slots;
    let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params);
    // A cold invokevirtual site in a lambda reaches this vtable fast path
    // before the ordinary invoke interceptor.  ClassLoader's resource native
    // owns the null-name contract, so invoke it directly for just this case;
    // normal resource lookup remains eligible for the vtable cache.
    if method_class_name.as_ref() == "java/lang/ClassLoader"
        && matches!(
            (method_name.as_ref(), method_descriptor.as_ref()),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
                | ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                | ("resources", "(Ljava/lang/String;)Ljava/util/stream/Stream;")
        )
        && matches!(
            thread.frames[frame_idx].stack.peek_at(0),
            Value::Object(None)
        )
    {
        let (args, _) =
            pop_coerced_invoke_args_virtual(shared, caller_class_id, cp_index, frame_idx, thread)?;
        let callback = match method_name.as_ref() {
            "getResource" => cratonvm_native_builtins::classloader::cl_get_resource_essential,
            "getResources" => cratonvm_native_builtins::classloader::cl_get_resources_essential,
            "getResourceAsStream" => {
                cratonvm_native_builtins::classloader::cl_get_resource_as_stream_essential
            }
            // The callback is reached only with a null name, so it always
            // throws before producing a URL-typed result. Reuse its canonical
            // ClassLoader NPE construction for `resources(String)`.
            "resources" => cratonvm_native_builtins::classloader::cl_get_resource_essential,
            "loadClass" => cratonvm_native_builtins::classloader::cl_load_class_essential,
            _ => return Ok(CachedCallResult::CacheMiss),
        };
        let value = crate::vm::safe_native_call(shared, thread, callback, &args)?;
        if let Some(value) = value {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, crate::jit::return_type(&method_descriptor)),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        return Ok(CachedCallResult::Handled);
    }
    if crate::runtime::env_cache::dbg_jetty2() && &*method_name == "getClasspath" {
        eprintln!(
            "[jetty2-vtfast] {}{} receiver={:?}",
            &*method_name, &*method_descriptor, receiver_val
        );
    }
    let receiver_obj = match receiver_val {
        Value::Object(Some(obj_ref)) => obj_ref,
        Value::Object(None) => {
            // Round 63 — Spring's GenericConversionService$Converters
            // .getClassHierarchy threads nulls through
            // `Class.componentType/getSuperclass/getInterfaces/arrayType`.
            // See the matching block in execute_invoke_cached for the
            // detailed rationale. Mirror that null-tolerant shape here so
            // the inline-cache fast path doesn't NPE first.
            //
            // We need the constant-pool class of the call site, which we
            // can read out of the resolution cache populated in step 2.
            // We don't have it locally yet, so fall back to the slow path
            // by emitting a CacheMiss — `execute_invoke_cached` will run
            // its own block with the full method-ref triple in hand.
            return Ok(CachedCallResult::CacheMiss);
        }
        _ => return Ok(CachedCallResult::CacheMiss),
    };
    // Refresh via the same GC-forwarding barrier applied to invoke args
    // (see `refresh_stale_object_args`). This value came from a bare
    // `peek_at` (not a `pop`), so while it's technically still visible to
    // root-scanning on the operand stack, downstream consumers here
    // (`class_id_of`/`kind_of` used to pick the dispatch target) are
    // exactly the class-resolution step implicated in the TestUpgrade
    // RootReference residual — refresh defensively before trusting it for
    // dispatch. See fixed-suite-bugs/h2-suite-bugs/
    // bug-h2-suite-residual-fail-triage-FIXED.md.
    let receiver_obj = shared.mem.heap.load_and_forward(receiver_obj);

    // Arrays go through java/lang/Object — don't dispatch via the
    // receiver's array-component vtable. Let the slow path handle it.
    if shared.mem.heap.kind_of(receiver_obj) == cratonvm_types::ObjectKind::Array {
        return Ok(CachedCallResult::CacheMiss);
    }
    let receiver_class_id = shared.mem.heap.class_id_of(receiver_obj);
    // All-zero header = stale pointer from zeroed GC memory — fall back
    // to the slow path which has detailed recovery logic.
    if receiver_class_id == ClassId::new(0) {
        return Ok(CachedCallResult::CacheMiss);
    }

    // Synthetic lambda proxies implement their SAM through
    // `try_lambda_dispatch`, not a vtable body.
    if shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&receiver_class_id)
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    if resolved_private_invokevirtual_target(
        shared,
        caller_class_id,
        &method_class_name,
        &method_name,
        &method_descriptor,
    )
    .is_some()
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    if crate::runtime::env_cache::dbg_vdisp()
        && (method_name.as_ref() == "hashCode"
            || method_name.as_ref() == "equals"
            || method_name.as_ref() == "run")
    {
        let cm = shared.classes.class_manager.read();
        let caller = cm
            .get_class(caller_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let rcv = cm
            .get_class(receiver_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let found = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &method_descriptor,
            &cm.class_store,
        );
        let declaring = found
            .and_then(|(_m, did)| cm.class_store.get(did).map(|c| c.name.to_string()))
            .unwrap_or_default();
        let is_abstract = found.map(|(m, _)| m.is_abstract()).unwrap_or(false);
        let has_code = found.map(|(m, _)| m.code().is_some()).unwrap_or(false);
        eprintln!("[vdisp] VTFAST caller={caller} method={method_name}{method_descriptor} receiver_class={rcv} declaring={declaring} is_abstract={is_abstract} has_code={has_code}");
    }

    // Interpreter intrinsic shadowing guard.
    //
    // This vtable fast path runs BEFORE `execute_invokevirtual_cached` (it is
    // dispatched first at the 0xb6/0xb9 opcode sites) — so if it dispatched a
    // method body here, the intrinsic inline cache in
    // `execute_invokevirtual_cached` would never be consulted for that site.
    //
    // In practice every intrinsic-eligible virtual method (`String.length`,
    // `Object.getClass`/`hashCode`, `StringBuilder.append`/`toString`/`length`)
    // already has a Rust native registered, and the native-override block
    // below returns `CacheMiss` for those — so the call falls through to the
    // cached path that owns the intrinsic IC. This explicit check makes that
    // guarantee independent of native-registration coverage: when the
    // resolved declaring class + (name, descriptor) is in the intrinsic
    // table we always emit `CacheMiss`, ceding dispatch to
    // `execute_invokevirtual_cached`. Keyed on the *resolved* declaring class
    // so an overriding subclass (whose body is NOT in the table) is
    // unaffected and dispatches here normally. Suppressed by
    // `intrinsics_disabled()` (the differential-test off-switch) so the
    // off-run behaves byte-for-byte like the pre-intrinsic VM.
    if !crate::runtime::env_cache::intrinsics_disabled()
        && cratonvm_native_builtins::intrinsics::might_have_method_descriptor(
            &method_name,
            &method_descriptor,
        )
    {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let is_intrinsic = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &method_descriptor,
            store,
        )
        .and_then(|(_m, declaring_id)| {
            let declaring_class = store.get(declaring_id)?;
            // A class redefined in place by a JVMTI agent (e.g. a Mockito
            // inline mock) has authoritative woven bytecode — do not shadow
            // it with the Rust intrinsic, or the advice never runs.
            if crate::classloading::any_class_redefined()
                && cm.class_redefine_generation(declaring_id) > 0
                && !redefine_immune_layout_native(
                    &declaring_class.name,
                    &method_name,
                    &method_descriptor,
                )
            {
                return Some(false);
            }
            Some(
                cratonvm_native_builtins::intrinsics::lookup(
                    &declaring_class.name,
                    &method_name,
                    &method_descriptor,
                )
                .is_some()
                    // Records (JEP 395): a generated `hashCode`/`equals` is an
                    // intrinsic too, but it is not in the static table — see
                    // `record_object_intrinsic`. Without this arm the vtable
                    // fast path runs the `invokedynamic` body and the
                    // intrinsic IC is never reached.
                    || record_object_intrinsic(declaring_class, &method_name, &method_descriptor)
                        .is_some(),
            )
        })
        .unwrap_or(false);
        drop(cm);
        if is_intrinsic {
            return Ok(CachedCallResult::CacheMiss);
        }
    }

    // WP0.1 — Native override priority. A Rust native registered for
    // (receiver_class, method_name, descriptor) MUST take priority over
    // bytecode from the class file. This matches the dispatch order in
    // `populate_virtual_invoke_cache` (line ~10444) and `try_stackless_invoke`
    // (line ~7818): native override first, class hierarchy second.
    //
    // Without this check the vtable fast path can dispatch to real-JDK
    // bytecode for classes that are shadowed by a synthetic stub — e.g.
    // `java/io/PrintStream`, where the `System.out` object is allocated
    // with only the synthetic field layout. The real bytecode then
    // accesses fields that don't exist, producing the canonical
    // "Cannot invoke write on null" NPE on the second `println` call
    // once the vtable has been populated for the reused class_id.
    //
    // The check peeks at the receiver's class name (no allocation) and
    // consults the native registry. A hit dispatches via the cached
    // fast-path at the call site below, by falling through to
    // `execute_invokevirtual_cached` which already handles
    // `CachedInvokeTarget::VirtualNative` correctly — ensuring the
    // invoke_cache entry populated by prior slow-path calls is honored.
    {
        let cm = shared.classes.class_manager.read();
        let rcv_name_owned = cm.get_class(receiver_class_id).map(|c| c.name.clone());
        if let Some(rcv_name) = rcv_name_owned.as_ref() {
            // JVMTI redefine guard: a class redefined in place by an agent has
            // authoritative woven bytecode, so the per-class native shadows
            // below must be suppressed (the mock's advice has to run). Cheap
            // fast-path on `any_class_redefined`.
            // Reflection-metadata natives stay authoritative even when the
            // receiver class was redefined (see `redefine_immune_reflection_native`):
            // a Mockito inline mock of `java.lang.reflect.Method` must not
            // disable `Method.getDeclaredAnnotations()` for every method object.
            let receiver_redefined = crate::classloading::any_class_redefined()
                && cm.class_redefine_generation(receiver_class_id) > 0
                && !redefine_immune_reflection_native(rcv_name, &method_name)
                && !redefine_immune_layout_native(rcv_name, &method_name, &method_descriptor);
            // WP2.7 — annotation proxies have no real bytecode for
            // equals/hashCode/toString. Force fall-through to the slow path
            // so `execute_invoke`'s annotation_proxy interception layer
            // serves the spec-compliant `Annotation` contract.
            if &**rcv_name == "java/lang/annotation/AnnotationProxy" {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            // Dynamic-proxy default-method dispatch guard. A JDK dynamic
            // proxy must route EVERY interface method call — including
            // concrete default methods like `AgeHolder.age()` — through its
            // `InvocationHandler.invoke()` (see `is_proxy_dispatch` in
            // `execute_invoke_kind`, which does this correctly). This vtable
            // fast path installs a direct `VirtualBytecode` target keyed by
            // `receiver_class_id` alone; since every proxy instance sharing
            // an interface set shares one generated `$ProxyN` class id, the
            // FIRST call that resolves a default method here poisons the
            // cache for that call site, so a LATER call on a *different*
            // proxy instance of the same generated class dispatches straight
            // to the default method's bytecode and skips the handler
            // entirely (observed: `AspectJAutoProxyCreatorTests.twoAdviceAspectPrototype`/
            // `twoAdviceAspectSingleton` advice silently not firing on the
            // second proxy instance).
            // Force the slow path for any proxy receiver so `is_proxy_dispatch`
            // is consulted on every call; cost is zero on non-proxy dispatch.
            //
            // NOTE: walk the chain using the ALREADY-HELD `cm` guard rather
            // than calling `class_chain_reaches_proxy_instance` (which takes
            // its own `shared.classes.class_manager.read()`) — a nested second read
            // acquisition on the same thread self-deadlocks under
            // parking_lot's writer-preferring fairness once any writer is
            // queued, since the outer guard here is never dropped before the
            // inner one blocks.
            {
                const MAX_DEPTH: usize = 32;
                let mut current = Some(receiver_class_id);
                let mut is_proxy = false;
                for _ in 0..MAX_DEPTH {
                    let cid = match current {
                        Some(c) => c,
                        None => break,
                    };
                    let class = match cm.get_class(cid) {
                        Some(c) => c,
                        None => break,
                    };
                    // `class_name_is_proxy_super`, NOT a local
                    // `Proxy$Instance` literal. The super of a generated
                    // `$ProxyN` is `java/lang/reflect/Proxy` by default
                    // (`CRATONVM_REAL_PROXY_SUPER` is truthy-default-true),
                    // so a one-name test recognises no shipped proxy and
                    // this guard never fired between 2026-07-02 and
                    // 2026-08-13 — see, under docs/known-issues/jdk-only/,
                    // F32-1-the-proxy-route-and-the-drifted-twin-20260813.md.
                    // The shared predicate is name-only and takes NO lock,
                    // which is what makes it callable under the `cm` guard
                    // this block holds (a nested second read self-deadlocks
                    // under parking_lot's writer-preferring fairness — the
                    // original reason this walk was inlined at all).
                    if class_name_is_proxy_super(&class.name) {
                        is_proxy = true;
                        break;
                    }
                    if &*class.name == "java/lang/Object" {
                        break;
                    }
                    current = class.superclass;
                }
                if is_proxy {
                    drop(cm);
                    return Ok(CachedCallResult::CacheMiss);
                }
            }
            // `URLClassLoader.findClass` invoked on a SUBCLASS receiver (the
            // native is registered on `URLClassLoader`, not the subclass, so the
            // `find(rcv_name, …)` probe below misses and the parent walk would
            // dispatch URLClassLoader's bytecode — which reads the shimmed `ucp`
            // and throws CNF). Cede to the slow path, where
            // `intercept_urlclassloader_subclass_find_class` forces the
            // `ucl_find_class` native. Jasper's `JasperLoader.loadClass` →
            // `findClass` (runtime-compiled `org.apache.jsp.*_jsp`) is the
            // canonical case. Only when the receiver inherits (does not override)
            // `findClass` — checked via the resolved declaring class.
            if matches!(
                (&*method_name, &*method_descriptor),
                ("findClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                    | ("addURL", "(Ljava/net/URL;)V")
            ) && &**rcv_name != "java/net/URLClassLoader"
                && crate::classloading::find_method_recursive(
                    receiver_class_id,
                    &method_name,
                    &method_descriptor,
                    &cm.class_store,
                )
                .and_then(|(_m, did)| {
                    cm.class_store
                        .get(did)
                        .map(|c| &*c.name == "java/net/URLClassLoader")
                })
                .unwrap_or(false)
            {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            if surefire_lazy_launcher_discover_native(
                shared,
                &method_name,
                &method_descriptor,
                receiver_obj,
            )
            .is_some()
            {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            let native_shadow_cache_key = Some(vtable_native_shadow_cache_key(
                receiver_class_id,
                hierarchy_fingerprint_in(&cm, receiver_class_id),
                &method_name,
                &method_descriptor,
            ));
            let cached_native_shadow = native_shadow_cache_key
                .and_then(|key| thread.native_shadow_cache.get(&key).copied());
            if cached_native_shadow == Some(true) {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            if cached_native_shadow != Some(false)
                && shared
                    .natives
                    .native_methods
                    .might_have_method_descriptor(&method_name, &method_descriptor)
            {
                let direct_native_shadow = !receiver_redefined
                    && shared
                        .natives
                        .native_methods
                        .find(rcv_name, &method_name, &method_descriptor)
                        .is_some()
                    // A stub-tagged native on an allow-listed class never runs
                    // while the real body is loaded, so it is not a shadow and
                    // bailing this call site to the slow path forever buys
                    // nothing — see `registered_native_actually_shadows`.
                    && registered_native_actually_shadows(
                        shared,
                        &cm,
                        rcv_name,
                        &method_name,
                        &method_descriptor,
                    );
                if direct_native_shadow {
                    remember_vtable_native_shadow(thread, native_shadow_cache_key, true);
                    if &**rcv_name == "java/lang/invoke/ConstantCallSite"
                        && crate::runtime::env_cache::dbg_ccsprobe()
                    {
                        eprintln!(
                            "[ccs-probe] vtable_fast: native found for {} {}{} — emitting CacheMiss",
                            rcv_name, method_name, method_descriptor,
                        );
                    }
                    drop(cm);
                    return Ok(CachedCallResult::CacheMiss);
                }
                if &**rcv_name == "java/lang/invoke/ConstantCallSite"
                    && crate::runtime::env_cache::dbg_ccsprobe()
                {
                    eprintln!(
                        "[ccs-probe] vtable_fast: native NOT found for {} {}{}",
                        rcv_name, method_name, method_descriptor,
                    );
                }
                // Receiver's-own-class override guard. The parent-walk below
                // (FJP fix, next comment) starts at `receiver_class_id`'s
                // SUPERCLASS — it never inspects whether `receiver_class_id`
                // itself directly declares this method. That is correct for
                // an INHERITED method (the walk's intended case: no override
                // on the receiver, so scanning ancestors for the nearest
                // bytecode-or-native is exactly right), but wrong for an
                // OVERRIDDEN one: a Mockito/ByteBuddy-generated mock subclass
                // of e.g. `java.net.HttpURLConnection` declares its OWN
                // bytecode body for every mockable method, and that override
                // is the most-derived target — it must win over a native
                // registered on an ancestor (`HttpURLConnection` itself),
                // exactly like real JVM virtual dispatch. Without this guard
                // the walk finds `HttpURLConnection`'s native at the very
                // first step and treats it as authoritative, so EVERY call on
                // the mock — including Mockito's own stubbing/verification
                // calls — silently bypasses the mock's advice and runs the
                // real native instead (observed as `MockitoException: Could
                // not modify all classes` / stray `IllegalArgumentException:
                // HttpURLConnection: URL not set` from `given(mock.foo())`).
                let receiver_has_own_bytecode = cm
                    .get_class(receiver_class_id)
                    .map(|c| c.find_method(&method_name, &method_descriptor).is_some())
                    .unwrap_or(false);
                if !receiver_has_own_bytecode {
                    // FJP fix: walk the parent chain to find natives registered on
                    // a superclass (e.g. `RecursiveTask.fork()` defined on
                    // `ForkJoinTask` but registered as a Rust native at
                    // `RecursiveTask`). Without this walk, the vtable would
                    // dispatch the inherited JDK bytecode for `fork()`, which uses
                    // Unsafe CAS and bypasses our native side-table.
                    let mut cid = receiver_class_id;
                    while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass) {
                        if let Some(parent) = cm.get_class(parent_id) {
                            // S107 collection-toString fix: if this parent has its
                            // own bytecode for the method, the bytecode override wins
                            // over any deeper native ancestor (e.g. Object.toString).
                            // Stop walking so the vtable bytecode path runs.
                            //
                            // Round 19 (peaceful-sammet) — IMPORTANT exception: if
                            // the parent has BOTH bytecode AND a Rust native, the
                            // native wins. See `populate_virtual_invoke_cache` for
                            // the LinkedHashMap-overlay rationale.
                            let has_bytecode = parent
                                .find_method(&method_name, &method_descriptor)
                                .is_some();
                            // Suppress an inherited native shadow when the declaring
                            // parent has been redefined by an agent (woven bytecode wins).
                            let parent_redefined = crate::classloading::any_class_redefined()
                                && cm.class_redefine_generation(parent_id) > 0
                                && !redefine_immune_forced_native(
                                    &parent.name,
                                    &method_name,
                                    &method_descriptor,
                                );
                            let has_native = !parent_redefined
                                && shared
                                    .natives
                                    .native_methods
                                    .find(&parent.name, &method_name, &method_descriptor)
                                    .is_some()
                                // Same yielded-stub exclusion as the
                                // receiver's-own-class probe above: an
                                // inherited stub that loses the arbitration is
                                // not a shadow either.
                                && registered_native_actually_shadows(
                                    shared,
                                    &cm,
                                    &parent.name,
                                    &method_name,
                                    &method_descriptor,
                                );
                            if has_native {
                                remember_vtable_native_shadow(
                                    thread,
                                    native_shadow_cache_key,
                                    true,
                                );
                                drop(cm);
                                return Ok(CachedCallResult::CacheMiss);
                            }
                            if has_bytecode {
                                break;
                            }
                        }
                        cid = parent_id;
                    }
                }
            }
            if cached_native_shadow != Some(false) {
                remember_vtable_native_shadow(thread, native_shadow_cache_key, false);
            }
        }
        drop(cm);
    }

    // Step 4 — VtableManager read. Look up the slot by
    // (method_name, descriptor) on the receiver's class, then fetch the
    // entry. A miss here (no installed vtable, no matching slot, slot
    // invalidated by CHA) falls through to invoke_cache / slow path.
    let (entry_cached, entry_is_native) = {
        let guard = shared.classes.vtable_manager.read();
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        let vtable = match guard.get_vtable(receiver_class_id.as_u32() as u64) {
            Some(v) => v,
            None => return Ok(CachedCallResult::CacheMiss),
        };
        let slot = match vtable.lookup_slot(&method_name, &method_descriptor) {
            Some(s) => s,
            None => return Ok(CachedCallResult::CacheMiss),
        };
        let entry = match vtable.get(slot) {
            Some(e) if e.resolved => e,
            _ => return Ok(CachedCallResult::CacheMiss),
        };
        if entry.is_native {
            return Ok(CachedCallResult::CacheMiss);
        }
        let cached = match &entry.resolved_method {
            Some(c) => Arc::clone(c),
            None => return Ok(CachedCallResult::CacheMiss),
        };
        let is_native = entry.is_native;
        // The class loader installs a vtable while it holds the class-manager
        // writer. Do not acquire the class-manager reader below while this
        // vtable read guard is live: concurrent class loading and virtual
        // dispatch would otherwise form an AB-BA deadlock.
        drop(guard);

        // A vtable slot stores one erased (name, descriptor) target, but an
        // invokeinterface call can require a more-specific interface default
        // than the slot inherited from its CP owner. Validate that the slot
        // agrees with receiver-rooted JVMS selection before dispatching it.
        // In particular, a covariant bridge has the parent return descriptor,
        // so starting resolution at the CP interface picks its own default and
        // skips the subinterface bridge.
        if is_interface {
            let cm = shared.classes.class_manager.read();
            let receiver_selected = crate::classloading::find_method_recursive(
                receiver_class_id,
                &method_name,
                &method_descriptor,
                &cm.class_store,
            )
            .map(|(_, declaring_id)| declaring_id);
            drop(cm);
            if receiver_selected != Some(cached.declaring_class_id) {
                return Ok(CachedCallResult::CacheMiss);
            }
        }

        // `VtableManager::install_vtable` refreshes inherited entries when a
        // declaring class is reinstalled by JVMTI redefine. The snapshot here
        // is therefore generation-current even for an already-linked
        // subclass; dispatch can stay on the cached path instead of
        // permanently re-resolving every call after the first redefine.
        // `vtable_install_adapter` (called
        // from `ClassManager::define_class_with_options` while defining a
        // class) takes the locks in the OPPOSITE order - class_manager
        // (held for the whole definition) then vtable_manager (to install
        // the new vtable). Holding both here in vtable-then-class order
        // was a real AB-BA deadlock under concurrent class loading +
        // virtual dispatch (confirmed live via gdb: a class-loading
        // thread blocked acquiring vtable_manager for write while holding
        // class_manager for write, and a dispatching thread blocked here
        // acquiring class_manager for read while holding vtable_manager
        // for read). `resolve_virtual_slot`'s own doc/test
        // (`vm.rs` vtable fast-path test) already establishes the
        // invariant that the vtable must be queryable WITHOUT holding
        // class_manager - this restores it.
        // `CachedBytecodeMethod` retains the resolved declaring method, so
        // memoize this pure, 55-branch decision on that shared entry instead
        // of reopening `class_manager` and re-evaluating it for every vtable
        // hit.
        let force_native = *cached.force_native_cache.get_or_init(|| {
            force_native_over_real_jdk_bytecode(
                cached.class_name.as_ref(),
                cached.method_name.as_ref(),
                cached.method_descriptor.as_ref(),
            )
        });
        // Site A2 of `native-dispatch-memoization.md` §3 Step 2.
        //
        // T2.5 -- the registry probe is memoized too, via the SAME per-entry
        // `NativeCallSite` the force-native interception path
        // (`intercept_force_registered_native_cached`, site A1) and the
        // instance tier-up gate (site A3) use. All three ask for exactly this
        // entry's triple, which is what the one-cell-one-triple invariant
        // requires; `NativeMethodRegistry::find` would re-hash all three
        // strings on every vtable hit.
        //
        // The probe used to be a `OnceLock` memo justified by "every `register`
        // call runs in `vm_init.rs` against the `&mut` registry BEFORE
        // `SharedVm` is constructed". That is not quite true -- `alias_class`
        // and the lazy `register_*` passes append slots after the first
        // bytecode executes -- and a `None` memoized before them never healed.
        // Keying on `NativeMethodRegistry::generation()` makes the memo correct
        // at every point in the VM's lifetime, not just after boot.
        // `entry.resolved_method` is a long-lived `Arc` held by the vtable slot,
        // so the memo really does persist across vtable hits.
        let force_native_registered = force_native
            && cached
                .native_call_site()
                .resolve(
                    &shared.natives.native_methods,
                    cached.class_name.as_ref(),
                    cached.method_name.as_ref(),
                    cached.method_descriptor.as_ref(),
                )
                .is_some();
        if force_native_registered {
            return Ok(CachedCallResult::CacheMiss);
        }
        (cached, is_native)
    };
    let _ = entry_is_native; // silence unused

    // Step 5 — dispatch. Pop args, push a new frame, and populate
    // invoke_cache for subsequent sibling-class misses.
    if thread.frames.len() >= shared.config.max_stack_depth {
        dump_stack_on_soe(thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }

    if crate::jit::profile::is_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared.jit.profile_store.record_receiver_borrowed(
            cid,
            mn,
            md,
            site_pc,
            receiver_class_id.as_u32(),
        );
    }

    let total_args = num_params + 1;
    const MAX_INLINE_ARGS: usize = 16;
    // Decode args bit-exact via parameter descriptors (receiver slot = 'L').
    // The prior pop_unchecked()/to_value() dropped the high bits of a
    // category-2 long arg whose NaN-box bit pattern collides with a tagged
    // sub-tag (BC safegcd 0xFFFC_… accumulators). See
    // gaps/bc-ec-mod-mododdinverse-investigation.md.
    // ONE forward scan for the whole descriptor. This closure used to call
    // `nth_param_tag_byte` per argument, and that rescans from `(` each time,
    // so popping N args cost O(N^2) tokenising of a string fixed per call site.
    let param_tags = ParamTags::of(&entry_cached.method_descriptor);
    let arg_desc_byte =
        |i: usize| -> u8 { param_tags.get_with_receiver(&entry_cached.method_descriptor, i) };
    let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
    let mut args_vec: Vec<Value> = Vec::new();
    let args_slice: &mut [Value] = if total_args <= MAX_INLINE_ARGS {
        for i in (0..total_args).rev() {
            args_buf[i] = thread.frames[frame_idx]
                .stack
                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
        }
        &mut args_buf[..total_args]
    } else {
        args_vec.resize(total_args, Value::Uninitialized);
        for i in (0..total_args).rev() {
            args_vec[i] = thread.frames[frame_idx]
                .stack
                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
        }
        &mut args_vec
    };
    refresh_stale_object_args(shared, args_slice);
    if crate::runtime::env_cache::dbg_loader_trace() && &*method_name == "compareAndSetRoot" {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[CASROOT-TRACE/vtfast] caller_class_id={:?} cp_index={} receiver_map(args[0])={} expected(args[1])={} updated(args[2])={}",
            caller_class_id,
            cp_index,
            describe(&args_slice[0]),
            args_slice.get(1).map(describe).unwrap_or_default(),
            args_slice.get(2).map(describe).unwrap_or_default(),
        );
    }

    if let Some(res) = intercept_classloader_set_default_assertion_status(
        shared,
        thread,
        entry_cached.method_name.as_ref(),
        entry_cached.method_descriptor.as_ref(),
        args_slice,
    ) {
        return res;
    }

    if let Some(res) = intercept_force_registered_native(
        shared,
        thread,
        frame_idx,
        entry_cached.class_name.as_ref(),
        entry_cached.method_name.as_ref(),
        entry_cached.method_descriptor.as_ref(),
        args_slice,
    ) {
        return res;
    }

    let monitor_obj: Option<ObjectRef> = if entry_cached.is_synchronized {
        // Non-static virtual — receiver owns the monitor.
        match args_slice.first() {
            Some(Value::Object(Some(r))) => {
                let obj = *r;
                Some(crate::vm::monitor_enter_synchronized_method(
                    shared, thread, obj, args_slice,
                ))
            }
            _ => None,
        }
    } else {
        None
    };

    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        entry_cached.max_locals as usize,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        (entry_cached.max_stack as usize).max(16) + 8,
    );
    let mut frame = Frame::new_pooled_cached(
        Arc::clone(&entry_cached),
        args_slice,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    frame.monitor_on_exit = monitor_obj;
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);

    // Populate invoke_cache so subsequent sibling-class misses from this
    // caller class take the cheaper thread-local path next time.
    // WP2.4-F1: bind the gate to the declaring class — that's the class
    // whose method body lives in `entry_cached`. A redefine of that
    // class will bump the same Arc<AtomicU32> and the next cache hit
    // here will auto-evict.
    let gate = RedefineGate::snapshot(
        shared
            .classes
            .class_manager
            .read()
            .class_redefine_generation_handle(entry_cached.declaring_class_id),
    );
    let target = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id,
        cached: Arc::clone(&entry_cached),
        gate,
    };
    // T10.4 — also promote so sibling threads dispatching the same
    // call-site skip the class_manager walk.
    let promoted_key: crate::runtime::lockfree_resolve::PromotedInvokeKey =
        (caller_class_id, cp_index, false, Some(receiver_class_id));
    shared
        .classes
        .shared_resolution
        .insert_promoted_invoke(promoted_key, target.clone());
    thread.invoke_cache.put_poly(
        caller_class_id,
        cp_index,
        false,
        receiver_class_id,
        target.clone(),
    );
    thread
        .invoke_cache
        .put(caller_class_id, cp_index, false, target);

    Ok(CachedCallResult::FramePushed)
}

/// WP2.2 — `Method.invoke` / `Constructor.newInstance` are bytecode in the JDK
/// classfiles but have Rust overrides for correct primitive boxing. The
/// monomorphic invoke cache can still hold [`CachedInvokeTarget::VirtualBytecode`]
/// if populate missed the native; the fast path must not execute JDK bodies
/// (they bypass `try_stackless_invoke` — e.g. Surefire `LazyLauncher` NPE).
#[inline]
pub(super) fn native_override_for_cached_reflect_invoke(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<cratonvm_native_api::NativeCallback> {
    // Every arm is a fully-constant triple, so each gets its OWN memo cell
    // (native-dispatch-memoization §3 Step 1, B5/B6/B7).
    //
    // ONE STATIC, ONE TRIPLE — a `NativeCallSite` is keyed on the registry
    // generation alone and does not re-verify the triple on a warm hit, so a
    // cell reached with two different triples can hand the second one the
    // first one's memoized negative and silently report "no native" for a
    // registered one. Each arm below is a distinct triple and therefore a
    // distinct cell; the third does NOT reuse
    // `surefire_lazy_launcher_discover_native`'s cell even though the triple
    // is identical, so no cell is reachable from more than one call.
    static NCS_METHOD_INVOKE: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    static NCS_CONSTRUCTOR_NEW_INSTANCE: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    static NCS_REFLECT_LAZY_LAUNCHER_DISCOVER: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    match (class_name, method_name, descriptor) {
        (
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        ) => NCS_METHOD_INVOKE.callback(
            &shared.natives.native_methods,
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "java/lang/reflect/Constructor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ) => NCS_CONSTRUCTOR_NEW_INSTANCE.callback(
            &shared.natives.native_methods,
            "java/lang/reflect/Constructor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "org/apache/maven/surefire/junitplatform/LazyLauncher",
            "discover",
            "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;",
        ) => NCS_REFLECT_LAZY_LAUNCHER_DISCOVER.callback(
            &shared.natives.native_methods,
            "org/apache/maven/surefire/junitplatform/LazyLauncher",
            "discover",
            "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;",
        ),
        _ => None,
    }
}

/// Fast invokevirtual/invokeinterface/invokespecial using monomorphic inline cache
/// (stackless dispatch). Returns FramePushed for bytecode cache hits,
/// Handled for native, CacheMiss for fall-through.
/// Execute the canonical instance-field accessor body without allocating an
/// interpreter frame.
///
/// A substantial fraction of real workloads are made of generated getters
/// (`aload_0; getfield; <x>return`).  They are semantically simple, but even
/// with an already-warm virtual-call cache the normal interpreter path still
/// builds a frame, executes three bytecodes, and tears the frame down.  That
/// dominates `--nojit` graph-planning workloads, where the accessors are hot
/// enough that compiling them would normally hide the cost.
///
/// This deliberately accepts only the exact five-byte verifier-safe shape,
/// uses an *already resolved* field entry (a cold symbolic reference falls
/// through to ordinary bytecode), and stays out of every JVMTI/redefinition
/// mode that needs to observe the callee frame or field access.  It therefore
/// has the same field value and exception behaviour as the bytecode body while
/// retaining the normal path for all observable instrumentation cases.
pub(super) fn try_execute_cached_trivial_instance_getter(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: &CachedBytecodeMethod,
    args: &[Value],
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    if !crate::runtime::env_cache::trivial_getter_fast_path()
        || cached.is_static
        || cached.is_synchronized
        || cached.num_params != 0
        || args.len() != 1
        || crate::runtime::jvmti::any_method_entry_listener_active()
        || crate::runtime::jvmti::any_method_exit_listener_active()
        || crate::runtime::jvmti::any_field_watchpoint_active()
    {
        return Ok(None);
    }

    // Cached bytecode carries two speculative-read padding bytes.
    let code_len = cached.code.len().saturating_sub(2);
    let code = &cached.code[..code_len];
    if code.len() != 5 || code[0] != 0x2a || code[1] != 0xb4 {
        return Ok(None);
    }
    let return_opcode = code[4];
    if !matches!(return_opcode, 0xac | 0xad | 0xae | 0xaf | 0xb0) {
        return Ok(None);
    }
    let field_cp_index = u16::from_be_bytes([code[2], code[3]]);

    // Do not resolve here: resolving a first-use symbolic reference may load
    // classes and collect, while the decoded argument is intentionally a
    // short-lived Rust local.  The ordinary first invocation resolves it and
    // all later invocations can use this pure cache hit.
    let field = match shared
        .classes
        .resolution_cache
        .read()
        .get_field(cached.declaring_class_id, field_cp_index)
    {
        Some(field) if !field.is_static && !field.is_volatile => field.clone(),
        _ => return Ok(None),
    };

    // Validate that the field descriptor is precisely this method's return
    // descriptor, not just the broad return opcode category.
    let descriptor_matches = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(cached.declaring_class_id) else {
            return Ok(None);
        };
        let Some(ConstantPoolEntry::FieldReference {
            name_and_type_index,
            ..
        }) = class.constant_pool.get(field_cp_index)
        else {
            return Ok(None);
        };
        let Some((_, field_descriptor)) =
            class.constant_pool.get_name_and_type(*name_and_type_index)
        else {
            return Ok(None);
        };
        cached
            .method_descriptor
            .strip_prefix("()")
            .is_some_and(|return_descriptor| return_descriptor == field_descriptor)
    };
    if !descriptor_matches {
        return Ok(None);
    }

    let return_matches_field = matches!(
        (field.desc_byte, return_opcode),
        (b'J', 0xad)
            | (b'F', 0xae)
            | (b'D', 0xaf)
            | (b'L' | b'[', 0xb0)
            | (b'Z' | b'B' | b'C' | b'S' | b'I', 0xac)
    );
    if !return_matches_field {
        return Ok(None);
    }

    // CRATONVM_TRIVIAL_GETTER_VERIFY — resolve the same field reference the way
    // the `getfield` opcode would and report any divergence.
    //
    // This exists because the fast path reads the `resolution_cache` raw, while
    // the opcode goes through `resolve_field_ref_loader_aware`, which documents
    // that a cache entry "may have been populated by a loader-blind helper" and
    // refuses to trust one for a user-defined-loader caller. That made the fast
    // path the prime suspect for the 2026-07-30 Hibernate HQL mis-parse. It was
    // not: a full `ASTParserLoadingTest` run under this verifier reported ZERO
    // divergences while still mis-parsing, and the same run passed 106/106 with
    // `CRATONVM_NO_MOVING_YOUNG=1` — the defect was missing native roots in the
    // ANTLR intrinsics. Keep the verifier so that conclusion stays one env var
    // away from being re-checked instead of re-argued.
    if crate::runtime::env_cache::trivial_getter_verify() {
        if let Ok(authoritative) = resolve_field_ref_loader_aware(
            shared,
            thread,
            cached.declaring_class_id,
            field_cp_index,
        ) {
            if authoritative.field_index != field.field_index
                || authoritative.desc_byte != field.desc_byte
                || authoritative.is_reference != field.is_reference
                || authoritative.is_volatile != field.is_volatile
                || authoritative.declaring_class_id != field.declaring_class_id
            {
                eprintln!(
                    "[TRIVIAL-GETTER-DIVERGENCE] {}.{}{} cp#{field_cp_index}: \
                     cached(idx={} desc={} ref={} vol={} owner={:?}) != \
                     loader_aware(idx={} desc={} ref={} vol={} owner={:?})",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    field.field_index,
                    field.desc_byte as char,
                    field.is_reference,
                    field.is_volatile,
                    field.declaring_class_id,
                    authoritative.field_index,
                    authoritative.desc_byte as char,
                    authoritative.is_reference,
                    authoritative.is_volatile,
                    authoritative.declaring_class_id,
                );
            }
        }
    }

    let Value::Object(Some(receiver)) = args[0] else {
        // The normal invokevirtual null check remains responsible for the
        // precisely constructed NPE and its stack trace.
        return Ok(None);
    };
    let receiver = shared.mem.heap.load_and_forward(receiver);
    let mut value = shared.mem.heap.get_field(receiver, field.field_index);
    match field.desc_byte {
        b'J' => {
            let bits = match value {
                Value::Long(x) => x,
                Value::Double(x) => x.to_bits() as i64,
                Value::Int(x) => x as i64,
                Value::Object(None) | Value::Uninitialized => 0,
                Value::Object(Some(raw)) => raw.as_ptr() as usize as i64,
                Value::Float(x) => x.to_bits() as i64,
                Value::ReturnAddress(pc) => pc as i64,
            };
            thread.frames[frame_idx]
                .stack
                .push_compact_long_checked(CompactValue::long(bits))?;
        }
        b'D' => {
            let number = match value {
                Value::Double(x) => x,
                Value::Long(x) => f64::from_bits(x as u64),
                Value::Int(x) => x as f64,
                Value::Object(None) | Value::Uninitialized => 0.0,
                Value::Object(Some(raw)) => f64::from_bits(raw.as_ptr() as usize as u64),
                Value::Float(x) => x as f64,
                Value::ReturnAddress(pc) => pc as f64,
            };
            thread.frames[frame_idx]
                .stack
                .push_compact_double_checked(CompactValue::double_raw(number))?;
        }
        _ => {
            if field.is_reference {
                if matches!(value, Value::Int(0) | Value::Long(0)) {
                    value = Value::Object(None);
                }
            } else {
                value = match value {
                    Value::Object(None) => Value::Int(0),
                    Value::Object(Some(raw)) => Value::Int(raw.as_ptr() as usize as i32),
                    other => other,
                };
                value = narrow_int_to_field_type(value, field.desc_byte);
            }
            if let Value::Object(Some(object)) = value {
                value = Value::Object(Some(shared.mem.heap.load_and_forward(object)));
            }
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
        }
    }
    Ok(Some(CachedCallResult::Handled))
}

pub(super) fn execute_invokevirtual_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    site_pc: usize,
    is_special: bool,
    is_interface: bool,
) -> Result<CachedCallResult, MethodCallFailed> {
    let caller_class_id = thread.frames[frame_idx].class_id;

    if crate::runtime::env_cache::dbg_h2trace() {
        if let Ok((owner, method, descriptor, _)) =
            resolve_method_ref(shared, caller_class_id, cp_index)
        {
            if method.as_ref() == "prepareJoinBatch" {
                let cm = shared.classes.class_manager.read();
                let caller_name = cm
                    .get_class(caller_class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                let caller_loader = cm.get_loader_id(caller_class_id);
                let receiver = thread.frames[frame_idx].stack.peek_at(0);
                let recv_info = if let Value::Object(Some(r)) = receiver {
                    let rcid = shared.mem.heap.class_id_of(r);
                    let rname = cm
                        .get_class(rcid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    let rloader = cm.get_loader_id(rcid);
                    format!("class_id={rcid:?} class={rname} loader={rloader:?}")
                } else {
                    format!("{receiver:?}")
                };
                let cached = thread
                    .invoke_cache
                    .get(caller_class_id, cp_index, is_special);
                let cached_info = cached.as_ref().map(|t| format!("{t:?}"));
                drop(cm);
                eprintln!(
                    "[h2trace-pjb] site owner={owner} method={method}{descriptor} caller={caller_name} caller_loader={caller_loader:?} cp_index={cp_index} receiver=[{recv_info}] cached={cached_info:?}",
                );
            }
        }
    }

    if crate::runtime::env_cache::dbg_loader_trace()
        && is_special
        && thread.frames[frame_idx].class_name().contains("MVMap")
    {
        let resolved = resolve_method_ref(shared, caller_class_id, cp_index);
        eprintln!(
            "[EIVC-ENTRY-ALL] caller_class_id={caller_class_id:?} cp_index={cp_index} caller_method={}.{}{} resolved={:?}",
            thread.frames[frame_idx].class_name(),
            thread.frames[frame_idx].method_name(),
            thread.frames[frame_idx].method_descriptor(),
            resolved.as_ref().map(|(cn, mn, md, np)| format!("{cn}.{mn}{md} np={np}")),
        );
    }

    // Keep Spring's loader-split Adapt identity bridge in the slow dispatcher.
    // The cache can predate the receiver loader's enum copy and otherwise
    // bypasses that narrowly scoped reconciliation entirely.
    //
    // PERF (H2 TestFileSystem.testConcurrent, 2026-07-26): `loader_aware_
    // resolution()` is default-ON, so this ran a full `resolve_method_ref`
    // — resolution-cache `RwLock` read, hash probe, four `Arc` clone/drop
    // pairs — on EVERY invokevirtual/-interface/-special in the VM, purely to
    // compare the owner against one hard-coded Spring class name. `adapt_isin_
    // seen()` is a relaxed atomic load that stays `false` until
    // `resolve_method_metadata` has actually resolved a CP entry naming that
    // method, which is a strict prerequisite for this site to matter: the
    // check's only effect is to return `CacheMiss`, and an unresolved call
    // site has no inline-cache entry to bypass in the first place, so it
    // returns `CacheMiss` from the lookup below anyway.
    if crate::runtime::env_cache::loader_aware_resolution() && adapt_isin_seen() {
        if let Ok((owner, method, descriptor, _)) =
            resolve_method_ref(shared, caller_class_id, cp_index)
        {
            if owner.as_ref() == "org/springframework/core/annotation/MergedAnnotation$Adapt"
                && method.as_ref() == "isIn"
                && descriptor.as_ref()
                    == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
            {
                return Ok(CachedCallResult::CacheMiss);
            }
        }
    }

    // A previous non-null invocation may have cached the real-JDK bytecode
    // body of an inherited ClassLoader method.  That body does not reliably
    // enforce the public null-name contract for a synthetic embedded loader,
    // whereas the registered ClassLoader natives do.  Do this before reading
    // the inline cache: otherwise `resources("...")` poisons the same CP
    // entry and a later `resources(null)` silently returns a Stream.
    if !is_special
        && matches!(
            thread.frames[frame_idx].stack.peek_at(0),
            Value::Object(None)
        )
    {
        if let Ok((method_class_name, method_name, method_descriptor, _)) =
            resolve_method_ref(shared, caller_class_id, cp_index)
        {
            if method_class_name.as_ref() == "java/lang/ClassLoader"
                && matches!(
                    (method_name.as_ref(), method_descriptor.as_ref()),
                    ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                        | ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                        | (
                            "getResources",
                            "(Ljava/lang/String;)Ljava/util/Enumeration;"
                        )
                        | (
                            "getResourceAsStream",
                            "(Ljava/lang/String;)Ljava/io/InputStream;"
                        )
                        | ("resources", "(Ljava/lang/String;)Ljava/util/stream/Stream;")
                )
            {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            }
        }
    }

    let target = match thread
        .invoke_cache
        .get(caller_class_id, cp_index, is_special)
    {
        Some(t) => {
            dbg_invoke_stats_record(0);
            t.clone()
        }
        None => {
            dbg_invoke_stats_record(1);
            if crate::runtime::env_cache::dbg_gse() {
                if let Ok((_, mn, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
                    if mn.as_ref() == "getSyntaxError" {
                        let cm = shared.classes.class_manager.read();
                        let caller_loader = cm.get_loader_id(caller_class_id);
                        eprintln!(
                            "[GSE] CACHE-MISS caller_class_id={caller_class_id:?} caller_loader={caller_loader:?} cp_index={cp_index} is_special={is_special}"
                        );
                    }
                }
            }
            return Ok(CachedCallResult::CacheMiss);
        }
    };
    if crate::runtime::env_cache::dbg_gse() {
        if let Ok((_, mn, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
            if mn.as_ref() == "getSyntaxError" {
                let cm = shared.classes.class_manager.read();
                let caller_loader = cm.get_loader_id(caller_class_id);
                let target_desc = match &target {
                    CachedInvokeTarget::Bytecode { cached, .. } => {
                        let dcid = cached.declaring_class_id;
                        let dloader = cm.get_loader_id(dcid);
                        format!("Bytecode declaring_class_id={dcid:?} declaring_loader={dloader:?} declaring_name={}", cached.class_name)
                    }
                    CachedInvokeTarget::Native { .. } => "Native".to_string(),
                    CachedInvokeTarget::VirtualBytecode {
                        cached,
                        receiver_class_id,
                        ..
                    } => {
                        format!("VirtualBytecode receiver_class_id={receiver_class_id:?} declaring_name={}", cached.class_name)
                    }
                    CachedInvokeTarget::VirtualNative {
                        receiver_class_id, ..
                    } => {
                        format!("VirtualNative receiver_class_id={receiver_class_id:?}")
                    }
                    CachedInvokeTarget::Jit { cached, .. } => {
                        format!("Jit declaring_name={}", cached.class_name)
                    }
                    CachedInvokeTarget::Intrinsic { .. } => "Intrinsic".to_string(),
                };
                eprintln!(
                    "[GSE] CACHE-HIT caller_class_id={caller_class_id:?} caller_loader={caller_loader:?} cp_index={cp_index} is_special={is_special} target={target_desc}"
                );
            }
        }
    }
    // JVMTI redefine guard: never serve a cached native/intrinsic SHADOW for a
    // class an agent has redefined in place — evict the entry and re-resolve
    // through the slow path, whose gates dispatch the woven bytecode so the
    // instrumentation advice runs. Without this, the FIRST call resolves to the
    // woven body (slow path) but the inline cache it populates can still hold a
    // VirtualNative/Intrinsic shadow that later calls hit directly, silently
    // bypassing the advice (e.g. mockStatic(X)+mock(X): the stub registers but
    // subsequent `m.method()` calls return the native default). Cheap
    // fast-path on the global `any_class_redefined` flag.
    if crate::classloading::any_class_redefined() {
        let shadow_cid = match &target {
            CachedInvokeTarget::VirtualNative {
                receiver_class_id, ..
            } => Some(*receiver_class_id),
            CachedInvokeTarget::Intrinsic {
                receiver_class_id, ..
            } => *receiver_class_id,
            _ => None,
        };
        if let Some(cid) = shadow_cid {
            if shared
                .classes
                .class_manager
                .read()
                .class_redefine_generation(cid)
                > 0
            {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            }
        }
        // `Native` (the invokespecial/invokestatic direct-callback target --
        // see "Static cache entries: invokespecial uses Bytecode/Native"
        // below, since this function also serves invokespecial) carries no
        // `receiver_class_id` field, so the shadow_cid check above can never
        // catch it. `execute_invokestatic_cached` already resolves the CP
        // method-ref's owning class name and consults
        // `native_shadow_suppressed_by_redefine` to evict exactly this kind
        // of stale shadow for invokestatic; this function never did the same
        // for invokespecial. Concretely: Mockito's inline mock maker weaves
        // `AbstractStringBuilder.substring(int)`'s OWN body while
        // `StringBuilder.substring(int)`'s compiler-generated bridge still
        // forwards to it via `invokespecial`; `native_sb_substring` stays
        // registered on `AbstractStringBuilder` forever, so once this
        // invokespecial call site cached `Native` (warmed right after call
        // #1's correct, uncached dispatch), it was never evicted --
        // call #2 onward silently ran the native (real, empty-buffer)
        // implementation instead of Mockito's woven advice.
        //
        // PERF (H2 TestFileSystem.testConcurrent, 2026-07-26): every one of the
        // four conditions below is `false` unless `native_shadow_suppressed_by_
        // redefine` is `true`, and that helper's own first line is
        // `if !any_class_redefined() { return false }` — a relaxed atomic load.
        // Hoisting it above the `resolve_method_ref` skips a resolution-cache
        // `RwLock` read, a hash probe and four `Arc` clone/drop pairs on every
        // cached NATIVE invoke in a process where nothing has ever been
        // redefined, which is every run without an inline mock maker.
        if matches!(&target, CachedInvokeTarget::Native { .. })
            && crate::classloading::any_class_redefined()
        {
            if let Ok((mcn, mn, desc, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
                // Mirror `receiver_redefined` (populate_virtual_invoke_cache /
                // execute_invokevirtual_vtable_fast): a plain
                // `native_shadow_suppressed_by_redefine` call has no idea
                // about the redefine-immune allowlists, so without these it
                // would evict e.g. `setCharAt`'s native shadow for a
                // genuinely real, non-mock `StringBuilder` the instant ANY
                // StringBuilder anywhere in the process got Mockito-mocked
                // (real bytecode reads the incompatible compact byte[]/coder
                // layout -- see `is_string_builder_layout_native_override` --
                // and crashes with ArrayIndexOutOfBoundsException).
                if native_shadow_suppressed_by_redefine(shared, &mcn)
                    && !redefine_immune_reflection_native(&mcn, &mn)
                    && !redefine_immune_layout_native(&mcn, &mn, &desc)
                {
                    thread
                        .invoke_cache
                        .evict(caller_class_id, cp_index, is_special);
                    return Ok(CachedCallResult::CacheMiss);
                }
            }
        }
    }
    // [PB-DIAG] one-shot dump for InfoCmp.getInfoCmp at pc 38
    if crate::runtime::env_cache::dbg_pbstart() {
        let cn = thread.frames[frame_idx].class_name();
        let mn = thread.frames[frame_idx].method_name();
        if cn.ends_with("/InfoCmp") && mn == "getInfoCmp" && site_pc == 38 {
            let tname = match &target {
                CachedInvokeTarget::VirtualBytecode {
                    receiver_class_id,
                    cached,
                    ..
                } => format!(
                    "VirtualBytecode rc={} class={} {}{}",
                    receiver_class_id.as_u32(),
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor
                ),
                CachedInvokeTarget::VirtualNative {
                    receiver_class_id, ..
                } => format!("VirtualNative rc={}", receiver_class_id.as_u32()),
                CachedInvokeTarget::Native { .. } => "Native".to_string(),
                CachedInvokeTarget::Intrinsic { .. } => "Intrinsic".to_string(),
                CachedInvokeTarget::Bytecode { cached, .. } => format!(
                    "Bytecode class={} {}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                ),
                _ => format!("{:?}", target),
            };
            eprintln!(
                "[PB-DIAG-INVOKE] caller={}.{} pc={} cp_idx={} is_special={} target={}",
                cn, mn, site_pc, cp_index, is_special, tname
            );
        }
    }

    // PGO-01: call-site evidence for invokespecial's fast cached path (this
    // function also serves invokevirtual/invokeinterface, which are NOT
    // recorded here — they already have receiver-type coverage below, and
    // this function's own is_special branches are how invokespecial reaches
    // this cache at all, per "Static cache entries: invokespecial uses
    // Bytecode/Native" elsewhere in this function). Same placement rationale
    // as execute_invokestatic_cached: after every early CacheMiss eviction
    // above, right before the dispatch match.
    if is_special && crate::jit::profile::is_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared.jit.profile_store.record_call_site_borrowed(cid, mn, md, site_pc);
    }

    match target {
        CachedInvokeTarget::VirtualBytecode {
            mut receiver_class_id,
            mut cached,
            gate: mut entry_gate,
        } => {
            let num_params = cached.num_params as usize; // Widening: parameter count conversion
            let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params);

            match receiver_val {
                Value::Object(Some(obj_ref)) => {
                    // Refresh via the same GC-forwarding barrier as invoke
                    // args (`refresh_stale_object_args`) — this receiver
                    // came from a bare `peek_at`, not a `pop`. See
                    // fixed-suite-bugs/h2-suite-bugs/
                    // bug-h2-suite-residual-fail-triage-FIXED.md.
                    let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
                    // JVMS §4.4.1: an array type inherits its method table
                    // from `java.lang.Object`, but an array's header stores
                    // its COMPONENT class id (`ClassId(0)` for primitive
                    // arrays). Comparing that raw id against the cached
                    // receiver class therefore lets a `Foo[]` receiver hit an
                    // entry installed for a plain `Foo` and run `Foo`'s body
                    // with the array as `this` — `new Foo[3].toString()`
                    // returned `Foo`'s override reading array element 0 as
                    // field 0. Cede to the slow path, which routes array
                    // receivers through `Object` (same guard as
                    // `execute_invokevirtual_vtable_fast` and the JIT
                    // MIC/PIC's `receiver_is_plain_object`).
                    if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    let actual_class_id = shared.mem.heap.class_id_of(obj_ref);
                    if crate::jit::profile::is_profiling_enabled() {
                        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                        shared.jit.profile_store.record_receiver_borrowed(
                            cid,
                            mn,
                            md,
                            site_pc,
                            actual_class_id.as_u32(),
                        );
                    }
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && (cached.class_name.contains("RootReference")
                            || cached.class_name.contains("MVMap"))
                    {
                        eprintln!(
                            "[LOADER-TRACE] execute_invokevirtual_cached HIT-CHECK caller_class_id={:?} cp_index={} is_special={} method={}.{}{} cached.declaring={} cached_receiver_class_id={:?} actual_class_id={:?} match={}",
                            caller_class_id, cp_index, is_special,
                            cached.class_name, cached.method_name, cached.method_descriptor,
                            cached.class_name, receiver_class_id, actual_class_id, actual_class_id == receiver_class_id
                        );
                    }
                    if actual_class_id != receiver_class_id {
                        // PERF (h2-bnf-perf 2026-07-23): the primary monomorphic
                        // cache is stale for THIS receiver (it holds whichever
                        // class called through this site most recently), not
                        // simply empty -- so a megamorphic call site alternating
                        // between a handful of concrete classes (e.g. H2's
                        // `org.h2.bnf.Rule` family) hits this exact mismatch on
                        // nearly every call. Before giving up, check the small
                        // polymorphic overflow cache for an entry recorded for
                        // THIS specific receiver class on an earlier call (see
                        // `InvokeCache::get_poly`/`put_poly`) -- if found, swap
                        // in its (already staleness-checked, by construction
                        // keyed to `actual_class_id`) target and fall through
                        // to the rest of this arm's normal dispatch below,
                        // instead of paying for the full vtable-fast/slow-path
                        // resolution again for a receiver class this call site
                        // has already seen.
                        let poly_result = thread.invoke_cache.get_poly(
                            caller_class_id,
                            cp_index,
                            is_special,
                            actual_class_id,
                        );
                        match poly_result {
                            Some(CachedInvokeTarget::VirtualBytecode {
                                receiver_class_id: poly_rc,
                                cached: poly_cached,
                                gate: poly_gate,
                            }) => {
                                receiver_class_id = poly_rc;
                                cached = poly_cached;
                                entry_gate = poly_gate;
                            }
                            _ => return Ok(CachedCallResult::CacheMiss),
                        }
                    }
                    // Lambda proxy classes have no bytecode implementation of
                    // their functional-interface method. They must reach the
                    // slow path, which dispatches their SAM method handle.
                    if !is_special && shared.classes.is_lambda_proxy_class(actual_class_id) {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    // WP2.7 — AnnotationProxy methods (incl. Object.equals/hashCode/
                    // toString from Object) must dispatch through the spec-compliant
                    // interception in `execute_invoke`, not Object's bytecode.
                    if !is_special && shared.classes.is_annotation_proxy_class(actual_class_id) {
                        return Ok(CachedCallResult::CacheMiss);
                    }

                    // A cache entry created through a vtable slot is valid for
                    // an interface site only when it is the same target that
                    // receiver-rooted maximally-specific default resolution
                    // selects. This prevents a parent-interface default from
                    // remaining cached after it masked a covariant bridge on a
                    // receiver subinterface.
                    if is_interface {
                        let cm = shared.classes.class_manager.read();
                        let receiver_selected = crate::classloading::find_method_recursive(
                            actual_class_id,
                            &cached.method_name,
                            &cached.method_descriptor,
                            &cm.class_store,
                        )
                        .map(|(_, declaring_id)| declaring_id);
                        drop(cm);
                        if receiver_selected != Some(cached.declaring_class_id) {
                            thread
                                .invoke_cache
                                .evict(caller_class_id, cp_index, is_special);
                            return Ok(CachedCallResult::CacheMiss);
                        }
                    }

                    if thread.frames.len() >= shared.config.max_stack_depth {
                        dump_stack_on_soe(thread);
                        return Err(MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::StackOverflowError,
                        )));
                    }

                    let total_args = num_params + 1;
                    const MAX_INLINE_ARGS: usize = 16;
                    // Decode args bit-exact via parameter descriptors (receiver
                    // = 'L'); pop_unchecked()/to_value() dropped the high bits
                    // of collision-pattern long args. See
                    // gaps/bc-ec-mod-mododdinverse-investigation.md.
                    // ONE forward scan; the per-argument form rescanned from `(` each time.

                    let param_tags = ParamTags::of(&cached.method_descriptor);

                    let arg_desc_byte =

                        |i: usize| -> u8 { param_tags.get_with_receiver(&cached.method_descriptor, i) };
                    let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
                    let mut args_vec: Vec<Value> = Vec::new();
                    let args_slice: &mut [Value] = if total_args <= MAX_INLINE_ARGS {
                        for i in (0..total_args).rev() {
                            args_buf[i] = thread.frames[frame_idx]
                                .stack
                                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                        }
                        &mut args_buf[..total_args]
                    } else {
                        args_vec.resize(total_args, Value::Uninitialized);
                        for i in (0..total_args).rev() {
                            args_vec[i] = thread.frames[frame_idx]
                                .stack
                                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                        }
                        &mut args_vec
                    };
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && cached.class_name.contains("RootReference")
                        && cached.method_name.as_ref() == "tryUpdate"
                    {
                        let describe = |v: &Value| -> String {
                            match v {
                                Value::Object(Some(obj)) => {
                                    let cid = shared.mem.heap.class_id_of(*obj);
                                    let cn = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .get_class(cid)
                                        .map(|c| c.name.to_string())
                                        .unwrap_or_default();
                                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                                }
                                Value::Object(None) => "null".to_string(),
                                other => format!("{:?}", other),
                            }
                        };
                        eprintln!(
                            "[TRYUPDATE-TRACE/vbc] caller_class_id={:?} cp_index={} receiver_class_id={:?} pre-refresh receiver(args[0])={} updated(args[1])={}",
                            caller_class_id,
                            cp_index,
                            receiver_class_id,
                            describe(&args_slice[0]),
                            args_slice.get(1).map(describe).unwrap_or_default(),
                        );
                    }
                    refresh_stale_object_args(shared, args_slice);
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && cached.class_name.contains("RootReference")
                        && cached.method_name.as_ref() == "tryUpdate"
                    {
                        let describe = |v: &Value| -> String {
                            match v {
                                Value::Object(Some(obj)) => {
                                    let cid = shared.mem.heap.class_id_of(*obj);
                                    let cn = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .get_class(cid)
                                        .map(|c| c.name.to_string())
                                        .unwrap_or_default();
                                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                                }
                                Value::Object(None) => "null".to_string(),
                                other => format!("{:?}", other),
                            }
                        };
                        eprintln!(
                            "[TRYUPDATE-TRACE/vbc] caller_class_id={:?} cp_index={} receiver_class_id={:?} post-refresh receiver(args[0])={} updated(args[1])={}",
                            caller_class_id,
                            cp_index,
                            receiver_class_id,
                            describe(&args_slice[0]),
                            args_slice.get(1).map(describe).unwrap_or_default(),
                        );
                    }
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && cached.method_name.as_ref() == "compareAndSetRoot"
                    {
                        let describe = |v: &Value| -> String {
                            match v {
                                Value::Object(Some(obj)) => {
                                    let cid = shared.mem.heap.class_id_of(*obj);
                                    let cn = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .get_class(cid)
                                        .map(|c| c.name.to_string())
                                        .unwrap_or_default();
                                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                                }
                                Value::Object(None) => "null".to_string(),
                                other => format!("{:?}", other),
                            }
                        };
                        eprintln!(
                            "[CASROOT-TRACE/vbc] caller_class_id={:?} cp_index={} receiver_class_id={:?} receiver_map(args[0])={} expected(args[1])={} updated(args[2])={}",
                            caller_class_id,
                            cp_index,
                            receiver_class_id,
                            describe(&args_slice[0]),
                            args_slice.get(1).map(describe).unwrap_or_default(),
                            args_slice.get(2).map(describe).unwrap_or_default(),
                        );
                    }

                    if let Some(res) = intercept_classloader_set_default_assertion_status(
                        shared,
                        thread,
                        cached.method_name.as_ref(),
                        cached.method_descriptor.as_ref(),
                        args_slice,
                    ) {
                        return res;
                    }

                    if let Some(callback) = surefire_lazy_launcher_discover_native(
                        shared,
                        cached.method_name.as_ref(),
                        cached.method_descriptor.as_ref(),
                        obj_ref,
                    ) {
                        invoke_cached_native_callback(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            args_slice,
                            cached.method_descriptor.as_ref(),
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }

                    if let Some(callback) = native_override_for_cached_reflect_invoke(
                        shared,
                        cached.class_name.as_ref(),
                        cached.method_name.as_ref(),
                        cached.method_descriptor.as_ref(),
                    ) {
                        invoke_cached_native_callback(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            args_slice,
                            cached.method_descriptor.as_ref(),
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }

                    if let Some(res) = intercept_force_registered_native_cached(
                        shared, thread, frame_idx, &cached, args_slice,
                    ) {
                        return res;
                    }

                    if let Some(result) = try_execute_cached_trivial_instance_getter(
                        shared, thread, frame_idx, &cached, args_slice,
                    )? {
                        return Ok(result);
                    }

                    // (bug-03 layer B, default-ON; off-switch CRATONVM_JIT_VIRTUAL_TIERUP=0)
                    // Instance-method invocation tier-up. Today only static
                    // methods have an invocation counter, so short-loop instance
                    // hot methods (e.g. java.util.regex Pattern$*.match) never
                    // JIT-compile. Placed AFTER every interception above (so a
                    // natively-overridden method is never run as JIT'd bytecode)
                    // and BEFORE the method-level monitor below (the JIT body does
                    // not acquire a `synchronized`-method monitor, so those are
                    // excluded). Monomorphic: the receiver class id was already
                    // checked `== receiver_class_id`, so `cached` is the exact
                    // target for this receiver. `args_slice` is already decoded;
                    // on deopt/too-many-args we fall through to the interpreted
                    // frame push below (the operand stack is untouched).
                    //
                    // `!has_registered_native`: the comment above claims natives
                    // are already excluded by this point, but that's only true
                    // for FORCED natives (`intercept_force_registered_native_cached`
                    // just above only fires when `force_native_over_real_jdk_bytecode`
                    // says so). An ordinary, non-forced registered native — the
                    // common case, which the interpreter's per-call dispatch
                    // prefers over real bytecode by default — is invisible to
                    // that check, so tier-up would compile the method's REAL
                    // BYTECODE and, once compiled, EVERY future call through this
                    // receiver class permanently bypasses the native (compiled
                    // code doesn't re-run the interpreter's native-vs-bytecode
                    // decision). Found 2026-07-20 via `java.lang.ClassValue#get`
                    // (see `classvalue_cache.rs`/`is_classvalue_native_override`):
                    // Apache Groovy's `ClassInfo` registry calls it thousands of
                    // times per app boot — comfortably past the tier-up threshold
                    // — and the real bytecode it silently switched to depends on
                    // `Class.classValueMap`/`Unsafe` CAS machinery CratonVM
                    // doesn't faithfully reproduce, reintroducing the exact
                    // always-null symptom the native override exists to fix, but
                    // ONLY under JIT (this tier-up is JIT-only) and ONLY once
                    // warm — see fixed-suite-bugs/springboot/
                    // core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md
                    // Cluster C "Residual 5". Site A3 of
                    // `native-dispatch-memoization.md` §3 Step 2: reusing this
                    // entry's `NativeCallSite` (already warm from the
                    // force-native path above, and asking for the same triple)
                    // keeps this two integer loads, not a per-call triple hash.
                    // Unlike the `OnceLock` it replaces, a native registered by
                    // a lazy `register_*` pass after this site first ran is now
                    // seen -- which matters here, because a missed native means
                    // tier-up compiles the REAL BYTECODE and permanently
                    // bypasses the override.
                    // BOTH of these are CLOSURES now, and that is the point.
                    //
                    // They used to be `let` bindings evaluated HERE, above the
                    // `if` that consumes them -- so every cached invoke in the
                    // VM paid a `NativeMethodRegistry` resolve and a
                    // class-manager `try_read` + `get_class` +
                    // `starts_with("java/util/")` to decide an OPTIONAL JIT
                    // tier-up, including on invokes where the `&&` chain below
                    // could never reach them. `--nojit` is the extreme case:
                    // `!disable_jit()` is false, so the chain short-circuits
                    // several conditions EARLIER, and the work was done anyway.
                    // `perf --call-graph=fp` on
                    // `probes/InvokeAttributionProbe.java` under `--nojit` put
                    // `OrderedPlRwLock<ClassManager>::try_read` at 1.67% and
                    // `::read` at 1.38% of the interpreted-invoke arm, both
                    // attributed straight to this function. See
                    // known-issues/perf/interpreted-invoke-cost-350ns-20260825.md.
                    //
                    // Called in place in the `&&` chain they inherit its
                    // short-circuit, which is what the ordering of that chain
                    // was already written to express: cheap field reads first,
                    // then these. Both are pure -- `resolve` fills an
                    // idempotent memo, `try_read` is a read -- so deferring
                    // them changes nothing except how often they run.
                    let has_registered_native = || {
                        cached
                            .native_call_site()
                            .resolve(
                                &shared.natives.native_methods,
                                &cached.class_name,
                                &cached.method_name,
                                &cached.method_descriptor,
                            )
                            .is_some()
                    };
                    // `cached.class_name` is the call site's symbolic owner;
                    // for an interface call it need not be the concrete
                    // receiver that this monomorphic cache just validated.
                    // Consult the receiver ClassId for the java.util virtual
                    // tier-up exclusion so subtypes reached through List/Map
                    // or Iterator are covered as well.
                    let receiver_is_java_util = || {
                        // This dispatch can run while the current thread still
                        // owns the class-manager write lock during bootstrap.
                        // A blocking read here self-deadlocks. If the table is
                        // busy, conservatively suppress this optional tier-up.
                        shared
                            .classes
                            .class_manager
                            .try_read()
                            .map(|cm| {
                                cm.get_class(receiver_class_id)
                                    .is_some_and(|class| class.name.starts_with("java/util/"))
                            })
                            .unwrap_or(true)
                    };
                    if !is_special
                        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
                        && !cached.is_synchronized
                        && entry_gate.generation == 0
                        && !crate::runtime::env_cache::disable_jit()
                        // Ordered AFTER the cheap field reads and the JIT
                        // kill-switch on purpose: every condition in this chain
                        // is a pure predicate, so `&&` may order them freely,
                        // and this one costs a `NativeMethodRegistry` resolve.
                        // Under `--nojit` it is now never evaluated at all.
                        && !has_registered_native()
                        // The generic-conversion regression reaches a hot
                        // java.util graph while Spring creates annotation and
                        // conversion metadata. Its instance-method tier-ups
                        // are independently JIT-safe at direct/static sites,
                        // but this cached virtual route can publish a stale
                        // receiver-specific entry and then spin. Keep only
                        // this virtual promotion out of java.util; static
                        // compilation and ordinary direct dispatch remain on.
                        && !receiver_is_java_util()
                        // A handler-bearing callee must never be entered by a
                        // DIRECT compiled call. `execute_jit_call_decoded`
                        // below has no interpreter boundary at which the
                        // callee's own exception table can be resumed, so an
                        // implicit NPE/AIOOBE raised inside it bails out
                        // through THIS caller's epilogue, past the only point
                        // able to route it — leaving a pending exceptional
                        // frame the caller cannot resume. On an embedded server
                        // that surfaces as a request that never completes
                        // (`MultipartAutoConfigurationTests`).
                        //
                        // `try_jit_upgrade_with_gate` already refuses these,
                        // and so do the MIC/PIC and OSR direct-call sites
                        // (`mic_callee_has_exception_table`,
                        // `osr_callee_bars_direct_call`). This route was the
                        // gap: under `bg_compile` — the DEFAULT — the worker
                        // publishes and the `jit_cache` probe below promotes
                        // the site without consulting the gate, which is the
                        // only reason `jit_virtual_tierup` had to be turned off
                        // wholesale. `exception_table` is carried on the
                        // callee's own cache entry, so this costs one field
                        // read, not a class-manager lookup.
                        && cached.exception_table.is_empty()
                        && crate::runtime::env_cache::jit_virtual_tierup()
                    {
                        // Fast path: already compiled (by this counter or OSR)?
                        //
                        // T2.2 -- epoch-guarded exactly like the invokestatic twin
                        // in `execute_invokestatic_cached`: skip the string-keyed
                        // `JitCache::get` while this entry's snapshot of
                        // `jit_cache_generation()` is still current, because no
                        // publication or invalidation has happened since the probe
                        // that missed. Read the generation before probing so a
                        // racing publication can only cause a redundant re-probe,
                        // never a missed one.
                        let jit_generation = cratonvm_jit::jit_cache_generation();
                        let compiled_opt = if cached.jit_probe_is_current(jit_generation) {
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
                            // See the interpreter's twin: released on the
                            // mutator, and regularly the last owner.
                            found.map(cratonvm_jit::RetainedCode::new)
                        }
                        .or_else(|| {
                            // Warmup counter mirroring execute_invokestatic_cached.
                            // T2.5 -- memoized per call site; see
                            // `CachedBytecodeMethod::invoc_key`.
                            let invoc_key = cached.invoc_key();
                            const JIT_RETRY_STRIDE: u32 = 64;
                            let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                            let cnt = shared.jit.profile_store.increment_invocation(invoc_key);
                            let should_attempt = cnt >= threshold
                                && (cnt == threshold || (cnt - threshold) % JIT_RETRY_STRIDE == 0);
                            if should_attempt {
                                // wire-tiered-manager Step 7: off-thread tier-up is now
                                // the default. Under `bg_compile` (default-ON) we ENQUEUE
                                // and keep interpreting — the `jit_cache.get` fast-path
                                // above flips this site to `Jit` once the worker
                                // publishes — instead of compiling inline on the mutator.
                                // Mirrors `execute_invokestatic_cached`. Opt-out
                                // (`CRATONVM_BG_COMPILE=0`) restores the inline path.
                                if crate::runtime::env_cache::bg_compile() {
                                    ensure_bg_compiler_started(shared);
                                    let tiered_key = crate::jit::tiered::MethodKey::new(
                                        cached.class_name.as_ref(),
                                        cached.method_name.as_ref(),
                                        cached.method_descriptor.as_ref(),
                                    );
                                    // Real invocation count — see the invokestatic
                                    // twin: stride-boundary `+= 1` counting deflated
                                    // the manager's hotness view 64x.
                                    let _ = shared
                                        .jit
                                        .tiered_manager
                                        .on_method_invocation_observed(&tiered_key, cnt as u64);
                                } else if let Some(CachedInvokeTarget::Jit { compiled, .. }) =
                                    try_jit_upgrade_with_gate(shared, &cached, entry_gate.clone())
                                {
                                    return Some(compiled);
                                }
                            }
                            None
                        });
                        if let Some(compiled) = compiled_opt {
                            let ret = crate::jit::return_type(&cached.method_descriptor);
                            let heap = compiled.needs_heap();
                            // total_args = receiver + declared params; the decoded
                            // dispatcher treats arg 0 as the receiver.
                            if let Some(ccr) = execute_jit_call_decoded(
                                shared,
                                thread,
                                frame_idx,
                                &compiled,
                                total_args as u16, // Cast: small param count
                                ret,
                                heap,
                                &cached,
                                args_slice,
                            )? {
                                return Ok(ccr);
                            }
                            // None → deopt / too-many-args: fall through to the
                            // interpreted frame push (operand stack untouched).
                        }
                    }

                    // Acquire monitor for synchronized methods
                    let monitor_obj: Option<ObjectRef> = if cached.is_synchronized {
                        let obj = if cached.is_static {
                            // JVMS §2.11.10: static-synchronized monitor is the Class
                            // mirror (same object as ldc class / synchronized(X.class)).
                            get_or_create_class_mirror(shared, cached.declaring_class_id)
                        } else {
                            // Receiver is args_slice[0] for virtual calls
                            match args_slice.first() {
                                Some(Value::Object(Some(r))) => *r,
                                _ => return Ok(CachedCallResult::CacheMiss),
                            }
                        };
                        Some(crate::vm::monitor_enter_synchronized_method(
                            shared, thread, obj, args_slice,
                        ))
                    } else {
                        None
                    };

                    // T10.7 — top off the thread-local pool from the shared
                    // VM-wide VecPool when empty so sibling-thread releases
                    // bubble back into the hot path.
                    thread.refill_pools_from_shared(
                        &shared.mem.operand_stack_pool,
                        &shared.mem.tag_pool,
                        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                        cached.max_locals as usize,
                        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                        (cached.max_stack as usize).max(16) + 8,
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
                            "[FRAME_PUSH/vcached] depth={} {}.{}{}",
                            thread.frames.len(),
                            frame.class_name(),
                            frame.method_name(),
                            frame.method_descriptor()
                        );
                    }
                    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
                    Ok(CachedCallResult::FramePushed)
                }
                Value::Object(None) => {
                    // Round 63 — defer to slow path which has the
                    // null-tolerant shim for Spring's
                    // `GenericConversionService$Converters.getClassHierarchy`.
                    Ok(CachedCallResult::CacheMiss)
                }
                _ => Ok(CachedCallResult::CacheMiss),
            }
        }
        CachedInvokeTarget::VirtualNative {
            receiver_class_id,
            callback,
            native_id,
            native_kind,
            num_params,
            gate: _,
        } => {
            let num_params_usize = num_params as usize; // Widening: parameter count conversion
            let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params_usize);

            match receiver_val {
                Value::Object(Some(obj_ref)) => {
                    // Refresh via the same GC-forwarding barrier as invoke
                    // args (`refresh_stale_object_args`) — this receiver
                    // came from a bare `peek_at`, not a `pop`. See
                    // fixed-suite-bugs/h2-suite-bugs/
                    // bug-h2-suite-residual-fail-triage-FIXED.md.
                    let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
                    // JVMS §4.4.1: an array type inherits its method table
                    // from `java.lang.Object`, but an array's header stores
                    // its COMPONENT class id (`ClassId(0)` for primitive
                    // arrays). Comparing that raw id against the cached
                    // receiver class therefore lets a `Foo[]` receiver hit an
                    // entry installed for a plain `Foo` and run `Foo`'s body
                    // with the array as `this` — `new Foo[3].toString()`
                    // returned `Foo`'s override reading array element 0 as
                    // field 0. Cede to the slow path, which routes array
                    // receivers through `Object` (same guard as
                    // `execute_invokevirtual_vtable_fast` and the JIT
                    // MIC/PIC's `receiver_is_plain_object`).
                    if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    let actual_class_id = shared.mem.heap.class_id_of(obj_ref);
                    if crate::jit::profile::is_profiling_enabled() {
                        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                        shared.jit.profile_store.record_receiver_borrowed(
                            cid,
                            mn,
                            md,
                            site_pc,
                            actual_class_id.as_u32(),
                        );
                    }
                    if actual_class_id != receiver_class_id {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    let Some(callback) =
                        revalidate_cached_native(shared, native_id, callback, native_kind)
                    else {
                        thread
                            .invoke_cache
                            .evict(caller_class_id, cp_index, is_special);
                        return Ok(CachedCallResult::CacheMiss);
                    };
                    // The callback identity proves this cache entry is one of
                    // the eight real-layout Matcher leaves. The receiver was
                    // just checked against the cache's monomorphic class guard,
                    // so it cannot be a lambda/annotation proxy and its sole
                    // object argument is already heap-validated. Skip those two
                    // generic proxy locks and the duplicate heap-membership
                    // search while retaining ordinary native pinning, return,
                    // and exception handling.
                    if cratonvm_native_builtins::is_matcher_realjdk_native_callback(callback) {
                        let (args, method_descriptor) = pop_coerced_invoke_args_virtual(
                            shared,
                            caller_class_id,
                            cp_index,
                            frame_idx,
                            thread,
                        )?;
                        invoke_cached_native_callback_prevalidated(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            &args,
                            &method_descriptor,
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }
                    // The two hottest cache lookups in Tomcat are
                    // `String.toLowerCase(Locale)` and `Map.get(Object)`.
                    // Their virtual-native cache entries already prove both
                    // receiver class and callback identity, but the generic
                    // arm below still re-resolved the CP descriptor and
                    // allocated a `Vec` for their two reference arguments on
                    // every iteration. Pop the verifier-known reference pair
                    // directly and retain the ordinary prevalidated native
                    // call for pinning, GC remapping, exceptions and return
                    // coercion.
                    let cached_string_lower = num_params_usize == 1
                        && cratonvm_native_builtins::lang_string::is_lower_case_native_callback(
                            callback,
                        );
                    let cached_map_get = num_params_usize == 1
                        && !cached_string_lower
                        && cratonvm_native_collections::is_hot_map_get_native_callback(callback);
                    if cached_string_lower || cached_map_get {
                        let argument = thread.frames[frame_idx]
                            .stack
                            .pop_with_kind()?
                            .0
                            .decode_by_descriptor(b'L');
                        let receiver = thread.frames[frame_idx]
                            .stack
                            .pop_with_kind()?
                            .0
                            .decode_by_descriptor(b'L');
                        let mut args = [receiver, argument];
                        refresh_stale_object_args(shared, &mut args);
                        let descriptor = if cached_string_lower {
                            "(Ljava/util/Locale;)Ljava/lang/String;"
                        } else {
                            "(Ljava/lang/Object;)Ljava/lang/Object;"
                        };
                        invoke_cached_native_callback_prevalidated(
                            shared, thread, frame_idx, callback, &args, descriptor,
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }
                    // Lambda proxies require the slow `try_lambda_dispatch`
                    // route instead of a cached interface target.
                    if !is_special && shared.classes.is_lambda_proxy_class(actual_class_id) {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    // WP2.7 — same escape hatch as in the bytecode branch:
                    // AnnotationProxy method dispatch must always go through
                    // `execute_invoke`'s spec-compliant interception layer.
                    if !is_special && shared.classes.is_annotation_proxy_class(actual_class_id) {
                        return Ok(CachedCallResult::CacheMiss);
                    }

                    let (args, method_descriptor) = pop_coerced_invoke_args_virtual(
                        shared,
                        caller_class_id,
                        cp_index,
                        frame_idx,
                        thread,
                    )?;
                    invoke_cached_native_callback(
                        shared,
                        thread,
                        frame_idx,
                        callback,
                        &args,
                        &method_descriptor,
                    )?;
                    Ok(CachedCallResult::Handled)
                }
                Value::Object(None) => {
                    // Round 63 — defer to slow path which has the
                    // null-tolerant shim for Spring's
                    // `GenericConversionService$Converters.getClassHierarchy`.
                    Ok(CachedCallResult::CacheMiss)
                }
                _ => Ok(CachedCallResult::CacheMiss),
            }
        }
        // Interpreter intrinsic reached via invokevirtual/invokeinterface/
        // invokespecial. Modelled on the `VirtualNative` arm: the
        // `receiver_class_id` guard is the virtual-dispatch soundness check
        // (roadmap §3.4) — an overriding subclass produces a different
        // class id and falls to `CacheMiss`, re-resolving down the slow
        // path. A `None` `receiver_class_id` means a static intrinsic
        // (only reachable here via invokespecial); it carries no guard.
        CachedInvokeTarget::Intrinsic {
            kind: _,
            callback,
            num_params,
            param_descs,
            return_type,
            receiver_class_id,
            gate: _,
        } => {
            if let Some(guard_class_id) = receiver_class_id {
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                let num_params_usize = num_params as usize;
                let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params_usize);
                match receiver_val {
                    Value::Object(Some(obj_ref)) => {
                        // Refresh via the same GC-forwarding barrier as
                        // invoke args (`refresh_stale_object_args`) — this
                        // receiver came from a bare `peek_at`, not a `pop`.
                        // See fixed-suite-bugs/h2-suite-bugs/
                        // bug-h2-suite-residual-fail-triage-FIXED.md.
                        let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
                        // JVMS §4.4.1: an array type inherits its method table
                        // from `java.lang.Object`, but an array's header stores
                        // its COMPONENT class id (`ClassId(0)` for primitive
                        // arrays). Comparing that raw id against the cached
                        // receiver class therefore lets a `Foo[]` receiver hit an
                        // entry installed for a plain `Foo` and run `Foo`'s body
                        // with the array as `this` — `new Foo[3].toString()`
                        // returned `Foo`'s override reading array element 0 as
                        // field 0. Cede to the slow path, which routes array
                        // receivers through `Object` (same guard as
                        // `execute_invokevirtual_vtable_fast` and the JIT
                        // MIC/PIC's `receiver_is_plain_object`).
                        if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                            return Ok(CachedCallResult::CacheMiss);
                        }
                        let actual_class_id = shared.mem.heap.class_id_of(obj_ref);
                        if crate::jit::profile::is_profiling_enabled() {
                            let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                            shared.jit.profile_store.record_receiver_borrowed(
                                cid,
                                mn,
                                md,
                                site_pc,
                                actual_class_id.as_u32(),
                            );
                        }
                        // Virtual-dispatch soundness guard: a receiver whose
                        // actual class differs from the resolved one may
                        // override the method — miss the intrinsic.
                        if actual_class_id != guard_class_id {
                            return Ok(CachedCallResult::CacheMiss);
                        }
                        // Phase 3: pop against the IC-cached `param_descs`
                        // (receiver included) into a stack buffer — no
                        // resolve, no descriptor parse, no heap alloc on the
                        // steady-state path.
                        let mut arg_buf = [Value::Uninitialized; MAX_INTRINSIC_ARGS];
                        let args = pop_coerced_invoke_args_intrinsic(
                            shared,
                            thread,
                            frame_idx,
                            num_params_usize,
                            &param_descs,
                            true,
                            &mut arg_buf,
                        )?;
                        invoke_cached_intrinsic(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            args,
                            return_type,
                        )?;
                        Ok(CachedCallResult::Handled)
                    }
                    Value::Object(None) => Ok(CachedCallResult::CacheMiss),
                    _ => Ok(CachedCallResult::CacheMiss),
                }
            } else {
                // `receiver_class_id: None` is only ever produced by the
                // invokestatic populator, which is consulted via
                // `execute_invokestatic_cached` — never this function. So
                // this arm is unreachable in practice; degrade safely to
                // the slow path rather than guess the arg-pop convention.
                let _ = callback;
                Ok(CachedCallResult::CacheMiss)
            }
        }
        // Static cache entries: invokespecial uses Bytecode/Native
        CachedInvokeTarget::Bytecode { cached, gate: _ } => {
            // NULL-RECEIVER-CACHED-20260801: JVMS §6.5 — invokevirtual /
            // invokespecial / invokeinterface must raise NullPointerException
            // when `objectref` is null, BEFORE the callee frame exists. This
            // arm popped the receiver straight into `args_slice[0]` and pushed
            // the frame regardless, so a warmed call site silently RAN the
            // callee body with `this == null`; the sibling `VirtualBytecode`
            // arm has always deferred `Value::Object(None)` to the slow path
            // (which owns both the canonical NPE and the deliberate
            // null-tolerant shims), and this arm — which serves invokespecial,
            // i.e. every private/super call — simply never got the same guard.
            //
            // Measured on `probes/NullReceiverInvokeProbe.java`: the FIRST
            // `Impl.callPrivateOn(null)` throws NPE correctly (slow path),
            // and after 50k warming calls the SAME site returns `3` — the
            // private method's body, executed with a null `this`.
            //
            // That divergence is what turned a null element in
            // `getPermittedSubclasses0()` into
            // `NullPointerException: Cannot read field "interfaces" because
            // "rd" is null` at `Class.java:1217` instead of a plain NPE at
            // `Class.isDirectSubType`: `c.getInterfaces(false)` is an
            // invokespecial, the callee frame was pushed with `this == null`,
            // and `Class.reflectionData()`'s registered native answers a null
            // receiver with a null RETURN. Deferring to the slow path here
            // makes the warmed and cold answers identical, whichever the slow
            // path decides.
            if !cached.is_static
                && matches!(
                    thread.frames[frame_idx]
                        .stack
                        .peek_at(cached.num_params as usize),
                    Value::Object(None)
                )
            {
                return Ok(CachedCallResult::CacheMiss);
            }
            if is_special && crate::runtime::env_cache::loader_aware_resolution() {
                if let Some(owner_cid) =
                    lookup_loader_initiated(shared, caller_class_id, cached.class_name.as_ref())
                {
                    if owner_cid != cached.declaring_class_id {
                        thread
                            .invoke_cache
                            .evict(caller_class_id, cp_index, is_special);
                        return Ok(CachedCallResult::CacheMiss);
                    }
                }
            }
            if thread.frames.len() >= shared.config.max_stack_depth {
                dump_stack_on_soe(thread);
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::StackOverflowError,
                )));
            }

            let total_args = cached.num_params as usize + 1; // Widening: parameter count conversion
            const MAX_INLINE_ARGS: usize = 16;
            // Decode args bit-exact via parameter descriptors (receiver = 'L');
            // pop_unchecked()/to_value() dropped the high bits of collision-
            // pattern long args. See gaps/bc-ec-mod-mododdinverse-investigation.md.
            // ONE forward scan; the per-argument form rescanned from `(` each time.

            let param_tags = ParamTags::of(&cached.method_descriptor);

            let arg_desc_byte =

                |i: usize| -> u8 { param_tags.get_with_receiver(&cached.method_descriptor, i) };
            let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
            let mut args_vec: Vec<Value> = Vec::new();
            let args_slice: &mut [Value] = if total_args <= MAX_INLINE_ARGS {
                for i in (0..total_args).rev() {
                    args_buf[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                }
                &mut args_buf[..total_args]
            } else {
                args_vec.resize(total_args, Value::Uninitialized);
                for i in (0..total_args).rev() {
                    args_vec[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                }
                &mut args_vec
            };
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("RootReference")
                && cached.method_name.as_ref() == "tryUpdate"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[TRYUPDATE-TRACE] caller_class_id={:?} cp_index={} pre-refresh receiver(args[0])={} updated(args[1])={}",
                    caller_class_id,
                    cp_index,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                );
            }
            refresh_stale_object_args(shared, args_slice);
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("RootReference")
                && cached.method_name.as_ref() == "tryUpdate"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[TRYUPDATE-TRACE] caller_class_id={:?} cp_index={} post-refresh receiver(args[0])={} updated(args[1])={}",
                    caller_class_id,
                    cp_index,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                );
            }
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("Page")
                && cached.method_name.as_ref() == "<init>"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[PAGEINIT-TRACE/bc] caller_class_id={:?} cp_index={} ctor_desc={} new_page(args[0])={} map_arg(args[1])={}",
                    caller_class_id,
                    cp_index,
                    cached.method_descriptor,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                );
            }
            // Same class-filter gate as the slow path above.
            if let Value::Object(Some(o)) = &args_slice[0] {
                if crate::runtime::env_cache::field_watch_class_matches(&cached.class_name) {
                    cratonvm_types::field_watch::watch(*o);
                }
            }
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("RootReference")
                && cached.method_name.as_ref() == "<init>"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[ROOTREFINIT-TRACE/bc] caller_class_id={:?} cp_index={} ctor_desc={} this(args[0])={} a1={} a2={} a3={}",
                    caller_class_id,
                    cp_index,
                    cached.method_descriptor,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                    args_slice.get(2).map(describe).unwrap_or_default(),
                    args_slice.get(3).map(describe).unwrap_or_default(),
                );
            }
            // Same class-filter gate as the slow path above.
            if let Value::Object(Some(o)) = &args_slice[0] {
                if crate::runtime::env_cache::field_watch_class_matches(&cached.class_name) {
                    cratonvm_types::field_watch::watch(*o);
                }
            }

            if let Some(res) = intercept_classloader_set_default_assertion_status(
                shared,
                thread,
                cached.method_name.as_ref(),
                cached.method_descriptor.as_ref(),
                args_slice,
            ) {
                return res;
            }

            if let Some(res) = intercept_force_registered_native_cached(
                shared, thread, frame_idx, &cached, args_slice,
            ) {
                return res;
            }

            // Acquire monitor for synchronized methods. This statically-resolved
            // `Bytecode` arm serves `invokespecial` (super-calls) and any
            // invoke whose inline cache resolved to a non-virtual bytecode
            // target. Unlike the `VirtualBytecode` and `invokestatic`
            // `Bytecode` arms — which both do this — the original code here
            // pushed the frame WITHOUT entering the method's monitor and
            // without recording `monitor_on_exit`. A `synchronized` method
            // reached on this cached path therefore ran without holding its
            // lock: silently wrong for mutual exclusion, and an outright
            // IllegalMonitorStateException the moment the body calls
            // `wait`/`notify`/`notifyAll` on `this`. The first call (slow path
            // `try_stackless_invoke`) acquires the monitor correctly, so the
            // bug only surfaces on the 2nd+ call once this arm's cache entry is
            // live. Observed as Apache Derby's embedded boot failing with
            // `XBM01.D` — `BasePage.releaseExclusive` is a `synchronized`
            // method invoked via `super` (invokespecial) that calls
            // `notifyAll()` on `this` to release a page latch. Mirror the
            // `VirtualBytecode` arm exactly.
            let monitor_obj: Option<ObjectRef> = if cached.is_synchronized {
                let obj = if cached.is_static {
                    // JVMS §2.11.10: static-synchronized monitor is the Class
                    // mirror (same object as ldc class / synchronized(X.class)).
                    get_or_create_class_mirror(shared, cached.declaring_class_id)
                } else {
                    // Receiver is args_slice[0] for instance calls.
                    match args_slice.first() {
                        Some(Value::Object(Some(r))) => *r,
                        _ => return Ok(CachedCallResult::CacheMiss),
                    }
                };
                Some(crate::vm::monitor_enter_synchronized_method(
                    shared, thread, obj, args_slice,
                ))
            } else {
                None
            };

            // T10.7 — refill per-thread pool from the shared VecPool if empty.
            thread.refill_pools_from_shared(
                &shared.mem.operand_stack_pool,
                &shared.mem.tag_pool,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                cached.max_locals as usize,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                (cached.max_stack as usize).max(16) + 8,
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
                    "[FRAME_PUSH/vcached2] depth={} {}.{}{}",
                    thread.frames.len(),
                    frame.class_name(),
                    frame.method_name(),
                    frame.method_descriptor()
                );
            }
            push_frame_and_fire_entry(shared.vm_identity, thread, frame);
            Ok(CachedCallResult::FramePushed)
        }
        CachedInvokeTarget::Native {
            callback,
            native_id,
            native_kind,
            num_params,
            gate: _,
        } => {
            // NULL-RECEIVER-CACHED-20260801: same guard as the `Bytecode` arm
            // above — this function only ever serves instance invokes
            // (invokestatic goes to `execute_invokestatic_cached`), so
            // `pop_coerced_invoke_args_virtual` always lays the receiver down
            // as `args[0]`. Handing a registered native a null `args[0]` is
            // how `Class.reflectionData()` came to return null instead of
            // throwing: its body answers a non-object receiver with
            // `Value::Object(None)`, and dozens of sibling natives do the
            // same. Defer to the slow path, which raises the NPE (or applies
            // the deliberate null-tolerant shim) exactly as the cold call did.
            if matches!(
                thread.frames[frame_idx].stack.peek_at(num_params as usize),
                Value::Object(None)
            ) {
                return Ok(CachedCallResult::CacheMiss);
            }
            let Some(callback) =
                revalidate_cached_native(shared, native_id, callback, native_kind)
            else {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            };
            let (args, method_descriptor) = pop_coerced_invoke_args_virtual(
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
        // JIT entries don't apply to virtual dispatch
        CachedInvokeTarget::Jit { .. } => Ok(CachedCallResult::CacheMiss),
    }
}

/// Populate the virtual invoke cache for a given call site after a successful virtual dispatch.
/// The cache is monomorphic: it stores the receiver class of the most recent call.
pub(super) fn populate_virtual_invoke_cache(
    thread: &mut JvmThread,
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_index: u16,
    receiver_class_id: ClassId,
    receiver_value: &Value,
) {
    // T10.4 fast path — the VM-wide `SharedResolutionState` may already
    // hold a fully-built `CachedInvokeTarget` that a sibling thread promoted
    // after running the slow resolution walk.  A hit here only takes the
    // lock-free read-guard on `promoted_invokes` and completely bypasses
    // the class_manager + resolution_cache write locks below.
    let promoted_key: crate::runtime::lockfree_resolve::PromotedInvokeKey =
        (caller_class_id, cp_index, false, Some(receiver_class_id));
    if let Some(target) = shared
        .classes
        .shared_resolution
        .get_promoted_invoke(&promoted_key)
    {
        thread.invoke_cache.put_poly(
            caller_class_id,
            cp_index,
            false,
            receiver_class_id,
            target.clone(),
        );
        thread
            .invoke_cache
            .put(caller_class_id, cp_index, false, target);
        return;
    }

    // Don't cache lambda proxy dispatch (they have special capture semantics)
    if shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&receiver_class_id)
    {
        return;
    }

    // Resolve method reference from constant pool
    let (class_name, method_name, descriptor, num_params) =
        match resolve_method_ref(shared, caller_class_id, cp_index) {
            Ok(r) => r,
            Err(_) => return,
        };

    // See the matching guard in `populate_invoke_cache`: a shared
    // `Object.equals(Object)` Methodref is not safely cacheable by constant
    // pool index alone because one method can invoke it at several distinct
    // receiver-polymorphic bytecode offsets.
    if class_name.as_ref() == "java/lang/Object"
        && method_name.as_ref() == "equals"
        && descriptor.as_ref() == "(Ljava/lang/Object;)Z"
    {
        return;
    }

    if crate::runtime::env_cache::dbg_vdisp()
        && (method_name.as_ref() == "hashCode"
            || method_name.as_ref() == "equals"
            || method_name.as_ref() == "run")
    {
        let cm = shared.classes.class_manager.read();
        let caller = cm
            .get_class(caller_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let rcv = cm
            .get_class(receiver_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let cp_static = cm
            .get_class(caller_class_id)
            .and_then(|_| resolve_method_ref(shared, caller_class_id, cp_index).ok())
            .map(|(cn, _, _, _)| cn.to_string())
            .unwrap_or_default();
        let found = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &descriptor,
            &cm.class_store,
        );
        let declaring = found
            .and_then(|(_m, did)| cm.class_store.get(did).map(|c| c.name.to_string()))
            .unwrap_or_default();
        let is_abstract = found.map(|(m, _)| m.is_abstract()).unwrap_or(false);
        let has_code = found.map(|(m, _)| m.code().is_some()).unwrap_or(false);
        eprintln!("[vdisp] POPULATE caller={caller} cp_static={cp_static} method={method_name}{descriptor} receiver_class={rcv} declaring={declaring} is_abstract={is_abstract} has_code={has_code}");
    }

    // Interpreter intrinsic probe (invokevirtual/invokeinterface).
    //
    // CRITICAL — virtual-dispatch soundness (roadmap §3.4): the table is
    // keyed on the *resolved declaring class* — the class that actually
    // provides the method body for THIS receiver — NOT the static cp
    // class. We walk `find_method_recursive` from the receiver's actual
    // class: an overriding subclass resolves to its own class name, which
    // is not in the intrinsic table, so it correctly MISSES the intrinsic
    // and falls through to ordinary dispatch. A non-overriding subclass
    // resolves to `java/lang/Object` (etc.) and may legitimately hit.
    //
    // The entry stores `receiver_class_id: Some(receiver_class_id)`; the
    // execute path additionally guards `actual == receiver` at dispatch
    // time, so even a megamorphic call site stays sound.
    //
    // `intrinsics_disabled()` suppresses population entirely — the
    // differential-test off-switch.
    if !crate::runtime::env_cache::intrinsics_disabled()
        && cratonvm_native_builtins::intrinsics::might_have_method_descriptor(
            &method_name,
            &descriptor,
        )
    {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        if let Some((_method, declaring_id)) = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &descriptor,
            store,
        ) {
            let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
            // WP0.1 soundness — a Rust native registered for the method on the
            // RECEIVER's class (or any ancestor strictly below the resolved
            // declaring class) outranks the intrinsic, exactly as it outranks
            // bytecode (see the native-override block below and the matching
            // guard in `execute_invokevirtual_vtable_fast`). Without this walk,
            // a synthetic class with no bytecode (e.g.
            // `cratonvm/internal/UnmodifiableList`, whose `hashCode` native
            // implements the List contract) resolves to `java/lang/Object` and
            // caches the identity-hash intrinsic — the FIRST call (slow path)
            // honours the native, every later call through the poisoned IC
            // returns the identity hash. Canonical victim: JUnit 6
            // `Namespace.hashCode()` (a `List.of` parts list) became unstable,
            // so `NamespacedHierarchicalStore` lookups missed and every
            // @ParameterizedTest died in `getDeclarationContext` (NPE).
            let native_override_below_declaring = if shared
                .natives
                .native_methods
                .might_have_method_descriptor(&method_name, &descriptor)
            {
                let mut cid = receiver_class_id;
                let mut hit = false;
                loop {
                    if cid == declaring_id {
                        break;
                    }
                    let Some(class) = store.get(cid) else { break };
                    if shared
                        .natives
                        .native_methods
                        .find(&class.name, &method_name, &descriptor)
                        .is_some()
                    {
                        hit = true;
                        break;
                    }
                    match class.superclass {
                        Some(parent) => cid = parent,
                        None => break,
                    }
                }
                hit
            } else {
                false
            };
            // JVMTI redefine guard (2026-07-23 mockitobean length() fix):
            // this intrinsic-population block had NO redefine awareness at
            // all, unlike its sibling gate in `execute_invokevirtual_vtable_fast`
            // (which already checks exactly this) and unlike the plain-Native
            // populate-side check further down in THIS function (fixed for
            // bug 3 of the mockitobean session). `StringBuilder.length()`'s
            // compiler-generated public bridge (`AbstractStringBuilder` is
            // package-private) is declared directly on `StringBuilder` --
            // `find_method_recursive` resolves `declaring_id` to
            // `StringBuilder` itself, which IS in the intrinsic table
            // (`StringBuilderLength`) -- so a Mockito inline mock's woven
            // advice on that exact bridge was being permanently shadowed by
            // this early, unconditional `Intrinsic` cache population, never
            // even reaching the (already redefine-aware) native-override
            // check below. Without this guard `verify(mock).length()`
            // silently no-ops instead of throwing "wanted but not invoked".
            let declaring_redefined_not_immune = crate::classloading::any_class_redefined()
                && cm.class_redefine_generation(declaring_id) > 0
                && !redefine_immune_layout_native(declaring_name, &method_name, &descriptor);
            let intrinsic_kind = if declaring_redefined_not_immune {
                None
            } else {
                (!native_override_below_declaring)
                    .then(|| {
                        cratonvm_native_builtins::intrinsics::lookup(
                            declaring_name,
                            &method_name,
                            &descriptor,
                        )
                        .or_else(|| {
                            // Records (JEP 395): `hashCode`/`equals` are not
                            // keyed on a fixed class, so they cannot come from
                            // the static table — see `record_object_intrinsic`.
                            store.get(declaring_id).and_then(|declaring| {
                                record_object_intrinsic(declaring, &method_name, &descriptor)
                            })
                        })
                    })
                    .flatten()
            };
            if let Some(kind) = intrinsic_kind {
                // Gate bound to the receiver class — a redefine of the
                // receiver swaps the dispatched body, mirroring the
                // `VirtualNative` gate binding below.
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
                // Census: unlike the `invokestatic` twin in `dispatch_static.rs`,
                // this block reaches an intrinsic through the CLASS STORE — it
                // never resolved a `NativeMethodId`, so one has to be looked up
                // to be declared. The row to mark is the DECLARING class's: the
                // walk above has already established that no native is
                // registered strictly below `declaring_id`
                // (`native_override_below_declaring`), so the declaring triple
                // is the only registry row this cache fill can shadow.
                // Resolved before `cm` is dropped, because `declaring_name`
                // borrows the class store.
                let bypassed_native_id = shared.natives.native_methods.resolve_id(
                    declaring_name,
                    &method_name,
                    &descriptor,
                );
                drop(cm);
                // Phase 3 — split the descriptor ONCE here so the
                // steady-state dispatch path never re-resolves or re-parses.
                let (pd_vec, _) = split_method_descriptor(&descriptor);
                let param_descs: Arc<[Arc<str>]> =
                    pd_vec.iter().map(|s| Arc::from(s.as_str())).collect();
                let return_type = crate::jit::return_type(&descriptor);
                if let Some(id) = bypassed_native_id {
                    mark_intrinsic_cache_bypass(&shared.natives.native_methods, id);
                }
                let target = CachedInvokeTarget::Intrinsic {
                    kind,
                    callback: cratonvm_native_builtins::intrinsics::callback_for(kind),
                    // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                    num_params: num_params as u16,
                    param_descs,
                    return_type,
                    receiver_class_id: Some(receiver_class_id),
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
        drop(cm);
    }

    // Check native overrides FIRST — a Rust native registered for a method
    // takes priority over bytecode from the class file.  This matches the
    // dispatch order in `try_stackless_invoke` (step 1: native override,
    // step 4: class hierarchy).  Without this check, a JDK method that is
    // NOT ACC_NATIVE but HAS a Rust override (e.g. Method.getName) would
    // be cached as VirtualBytecode, causing the bytecode to execute with
    // the wrong field layout.
    {
        // Determine the class name for native lookup.  Arrays dispatch
        // through java/lang/Object; for normal classes use the receiver's
        // name directly.
        let cm = shared.classes.class_manager.read();
        let rcv_name = cm
            .get_class(receiver_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let lookup_name = if rcv_name.starts_with('[') {
            "java/lang/Object".to_string()
        } else {
            rcv_name
        };
        let native_signature_may_exist = shared
            .natives
            .native_methods
            .might_have_method_descriptor(&method_name, &descriptor);
        // (`ThreadPoolExecutor.execute(Runnable)` receiver-shape exemption
        // deleted 2026-08-06 — see `force_native_over_real_jdk_bytecode` in
        // `native_override.rs`. `resolve_cached_native_registration` consults
        // the `SyntheticStub` yield rule, which is receiver-independent, so a
        // cache entry populated here is right for every receiver of this
        // class_id and there is nothing left to exempt.)
        // JVMTI redefine guard: this direct-native-override lookup has no
        // awareness of JVMTI `redefineClasses` -- unlike the SLOW,
        // uncached dispatch path (`intercept_force_registered_native` /
        // `should_force_registered_native_over_bytecode`), which correctly
        // cedes to a redefined class's woven bytecode. A Mockito inline
        // mock of e.g. `StringBuilder` redefines BOTH `StringBuilder` and
        // `AbstractStringBuilder` in place (advice woven into
        // `substring(int)`'s own body) while `native_sb_substring` stays
        // registered on both class names forever. Call #1 at a fresh call
        // site takes the slow path (correct: runs the woven advice), but
        // this populate step -- run to warm the cache for next time --
        // still matched the always-present native registration and cached
        // it as `VirtualNative`, with nothing to notice the receiver had
        // already been redefined. Every call #2+ then hit that cached
        // native directly, silently skipping Mockito's advice and running
        // the real (empty-buffer) implementation instead of the stubbed
        // answer. Mirrors the `receiver_redefined` guard in
        // `execute_invokevirtual_vtable_fast` -- `is_string_builder_layout_native_override`'s
        // methods (append/charAt/delete/getChars/insert/length/toString/
        // <init>) stay forced-native even after redefine because their real
        // JDK bodies assume a compact byte[]/coder layout CratonVM's
        // synthetic StringBuilder doesn't have; `substring` is deliberately
        // NOT in that list; it (RE-)validated the SAME cache before this fix.
        let receiver_redefined = crate::classloading::any_class_redefined()
            && cm.class_redefine_generation(receiver_class_id) > 0
            && !redefine_immune_reflection_native(&lookup_name, &method_name)
            && !redefine_immune_layout_native(&lookup_name, &method_name, &descriptor);
        let direct_native = if native_signature_may_exist && !receiver_redefined {
            resolve_cached_native_registration(shared, &lookup_name, &method_name, &descriptor)
                // A `SyntheticStub` on an allow-listed class yields to real
                // bytecode, and `revalidate_cached_native` enforces that on
                // every cache HIT — so publishing a `VirtualNative` target here
                // would produce an entry that is evicted and re-resolved on
                // every single call. Ask the same question once, at population
                // time, and publish nothing.
                //
                // This generalises the `ThreadPoolExecutor.execute(Runnable)`
                // receiver-shape exemption that used to sit here (deleted
                // 2026-08-06): that one named a single triple and asked about
                // the RECEIVER's `workers` field; this asks the arbitration
                // that already governs every other allow-listed class.
                //
                // `_with_cm` because the `cm` read guard above is still held.
                .filter(|(_, _, kind)| {
                    *kind != cratonvm_native_api::NativeKind::SyntheticStub
                        || !real_protected_stub_class(&lookup_name)
                        || !synthetic_stub_yields_with_cm(
                            &cm,
                            &lookup_name,
                            &method_name,
                            &descriptor,
                        )
                })
        } else {
            None
        };
        if let Some((callback, native_id, native_kind)) = direct_native {
            // WP2.4-F1: gate bound to the receiver class (where dispatch
            // landed). A redefine of the receiver swaps the method body.
            let gate =
                RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
            drop(cm);
            let target = CachedInvokeTarget::VirtualNative {
                receiver_class_id,
                callback,
                native_id,
                native_kind,
                // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                num_params: num_params as u16,
                gate,
            };
            // T10.4 — promote so sibling threads skip the class_manager walk.
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke(promoted_key, target.clone());
            thread.invoke_cache.put_poly(
                caller_class_id,
                cp_index,
                false,
                receiver_class_id,
                target.clone(),
            );
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, false, target);
            return;
        }
        // FJP fix: walk the parent chain looking for natives registered on
        // an ancestor class. Mirrors the slow path in `vm_exec::invoke_or_native`
        // so that calls like `SumTask.fork()` (where the native is registered
        // on `RecursiveTask`/`ForkJoinTask`) reach the Rust override and not
        // the inherited JDK bytecode.
        //
        // Round 63 (peaceful-sammet) — receiver-bytecode short-circuit:
        // if the *receiver* class declares its own bytecode for this
        // method, the subclass override must win over any ancestor's
        // native registration. Without this guard, Kafka's
        // `Group$GroupType.toString()` (a subclass override that reads
        // the subclass `name` field — "classic"/lowercase) was being
        // shadowed by the native `Enum.toString()` registered on
        // `java/lang/Enum`, which reads `Enum.name` ("CLASSIC"/upper).
        // First call dispatched via the slow path (correct override);
        // this cache populator then poisoned the inline cache with the
        // parent native, breaking subsequent calls and causing
        // `GroupCoordinatorConfig.<clinit>` to default the rebalance
        // protocols list to `["CONSUMER", "CLASSIC", "STREAMS"]`
        // instead of the lowercase forms the validator accepts.
        let receiver_has_own_bytecode = cm
            .get_class(receiver_class_id)
            .map(|c| c.find_method(&method_name, &descriptor).is_some())
            .unwrap_or(false);
        if receiver_has_own_bytecode {
            // Skip the ancestor-native promotion entirely — fall through
            // to the bytecode dispatch path below.
        } else if native_signature_may_exist {
            let mut cid = receiver_class_id;
            while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass) {
                if let Some(parent) = cm.get_class(parent_id) {
                    // S107 collection-toString fix: if this parent has its own
                    // bytecode for the method (e.g. AbstractCollection.toString),
                    // the bytecode override wins over any deeper native ancestor
                    // (e.g. Object.toString). Stop walking so the bytecode dispatch
                    // path runs (via find_method_recursive below).
                    //
                    // Round 19 (peaceful-sammet) — IMPORTANT exception: if the
                    // parent has BOTH its own bytecode AND a Rust native registered
                    // for this method, the native wins. Our LinkedHashMap natives
                    // store state in an external overlay (not real fields), so
                    // executing JDK bytecode for inherited callers like
                    // `AnnotationAttributes` (which extends `LinkedHashMap`) walks
                    // empty real fields and returns empty `entrySet()` /
                    // `keySet()`, breaking Spring's
                    // `MetadataReader.getAnnotationAttributes("...Import", true)`
                    // for `@EnableAutoConfiguration` and surfacing as
                    // `MissingWebServerFactoryBean` on Spring Boot startup.
                    let parent_name = parent.name.to_string();
                    // JVMTI redefine guard: Mockito's inline mock maker mocks a
                    // CONCRETE class (e.g. `java.net.HttpURLConnection`) by
                    // redefining that class directly (weaving advice into its
                    // methods) and instantiating a trivial marker SUBCLASS that
                    // does NOT itself override every mockable method — so the
                    // RECEIVER here is that marker subclass (never redefined),
                    // while an ANCESTOR up this walk (`parent`) is the one
                    // that was redefined. Without this guard, BOTH branches
                    // below (the Round-19 dual-registration exception and the
                    // pure-inherited-native case) cache a `VirtualNative`
                    // target pointing at our native, permanently shadowing the
                    // now-woven bytecode for every FUTURE call at this call
                    // site — even though the first (uncached) call correctly
                    // ran the woven bytecode via the slow path and the mock's
                    // advice fired. `execute_invokevirtual_cached`'s eviction
                    // check only inspects the RECEIVER class's redefine
                    // generation (stored in the cached target), so it never
                    // catches a shadow whose declaring class is an ancestor —
                    // the fix has to be here, where the entry is created.
                    let parent_redefined = crate::classloading::any_class_redefined()
                        && cm.class_redefine_generation(parent_id) > 0
                        && !redefine_immune_reflection_native(&parent_name, &method_name)
                        && !redefine_immune_layout_native(&parent_name, &method_name, &descriptor);
                    if parent_redefined {
                        break;
                    }
                    if parent.find_method(&method_name, &descriptor).is_some() {
                        if let Some((callback, native_id, native_kind)) =
                            resolve_cached_native_registration(
                            shared,
                            &parent_name,
                            &method_name,
                            &descriptor,
                        ) {
                            let gate = RedefineGate::snapshot(
                                cm.class_redefine_generation_handle(receiver_class_id),
                            );
                            drop(cm);
                            let target = CachedInvokeTarget::VirtualNative {
                                receiver_class_id,
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
                            thread.invoke_cache.put_poly(
                                caller_class_id,
                                cp_index,
                                false,
                                receiver_class_id,
                                target.clone(),
                            );
                            thread
                                .invoke_cache
                                .put(caller_class_id, cp_index, false, target);
                            return;
                        }
                        break;
                    }
                    if let Some((callback, native_id, native_kind)) =
                        resolve_cached_native_registration(
                            shared,
                            &parent_name,
                            &method_name,
                            &descriptor,
                        )
                    {
                        let gate = RedefineGate::snapshot(
                            cm.class_redefine_generation_handle(receiver_class_id),
                        );
                        drop(cm);
                        let target = CachedInvokeTarget::VirtualNative {
                            receiver_class_id,
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
                        thread.invoke_cache.put_poly(
                            caller_class_id,
                            cp_index,
                            false,
                            receiver_class_id,
                            target.clone(),
                        );
                        thread
                            .invoke_cache
                            .put(caller_class_id, cp_index, false, target);
                        return;
                    }
                }
                cid = parent_id;
            }
        }
    }

    // Look up method on the receiver's actual class (virtual dispatch resolution)
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
        receiver_class_id,
        &method_name,
        &descriptor,
        store,
    ) else {
        return;
    };

    if method.is_native() {
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
            shared,
            declaring_name,
            &method_name,
            &descriptor,
        )
        {
            // WP2.4-F1: gate bound to the declaring class.
            let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
            drop(cm);
            let target = CachedInvokeTarget::VirtualNative {
                receiver_class_id,
                callback,
                native_id,
                native_kind,
                num_params: num_params as u16, // Widening: parameter count conversion
                gate,
            };
            // T10.4 — promote so sibling threads skip this walk.
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke(promoted_key, target.clone());
            thread.invoke_cache.put_poly(
                caller_class_id,
                cp_index,
                false,
                receiver_class_id,
                target.clone(),
            );
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, false, target);
        }
        return;
    }

    // S111r13: For real-JDK Map functional methods (computeIfAbsent / compute
    // / merge / putIfAbsent / forEach / replaceAll / getOrDefault / replace /
    // putMapEntries — internal helper invoked from putAll / Map.copyOf), the
    // bytecode reads `getfield table` followed by `arraylength`.  Our
    // synthetic HashMap layout stores the `Int(capacity)` in slot 2 instead
    // of an array, surfacing as
    //   `expected object reference, got int(N)`.
    // Mirror the force-native override list at vm_exec.rs:invoke_on_class_shared_inner.
    // This branch fires when find_method_recursive resolved to non-native
    // bytecode declared on HashMap (or a sibling), but a Rust native exists
    // for the (declaring_class, method, descriptor) triple — caching the
    // bytecode would re-introduce the layout mismatch on every dispatch
    // through this call-site.
    {
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        // (`ThreadPoolExecutor.execute(Runnable)` receiver-shape exemption
        // deleted 2026-08-06. It existed because
        // `force_native_over_real_jdk_bytecode` carried a receiver-blind arm
        // for that triple which this site consulted directly, bypassing the
        // receiver checks at the other call sites and poisoning this inline
        // cache with `VirtualNative` for a genuinely real ThreadPoolExecutor.
        // That arm is gone, so this site no longer forces anything for it.)
        // W7-96 §7 NOMINATION 1. The shared function above now returns `false`
        // for a class the retirement dial covers, but this mirror ORs in a
        // cluster of its own -- so the dial has to be consulted here too or
        // arming it still measures the gate. `Off::covers()` is `false`, so an
        // unarmed run is unchanged.
        let dial_retires =
            crate::runtime::env_cache::enforce_shadow_scope().covers(declaring_name);
        let force = force_native_over_real_jdk_bytecode(declaring_name, &method_name, &descriptor)
            || (!dial_retires
                && matches!(
                declaring_name,
                "java/util/HashMap"
                    | "java/util/LinkedHashMap"
                    | "java/util/Hashtable"
                    | "java/util/concurrent/ConcurrentHashMap"
            ) && matches!(
                    &*method_name,
                    "computeIfAbsent" | "compute" | "computeIfPresent"
            | "merge" | "putIfAbsent" | "replace"
            | "forEach" | "replaceAll" | "getOrDefault"
            | "putMapEntries" | "putAll"
            | "keySet" | "values" | "entrySet"
            // S111r14: see vm_exec.rs — logback LoggerContext.<init>
            // hits HashMap.put → putVal → arraylength on Int(16).
            | "put" | "get" | "remove"
            | "containsKey" | "containsValue"
            | "size" | "isEmpty" | "clear"
            | "<init>"
            // `Hashtable.keys()` / `elements()` — legacy pre-1.2 Enumeration
            // accessors. Real-JDK body walks `this.table[]`; our overrides
            // store data in a side-store, so the JDK bytecode sees an empty
            // table. See companion entry in `force_native_over_real_jdk_bytecode`.
            | "keys" | "elements"
            ));
        if force {
            if let Some((callback, native_id, native_kind)) =
                resolve_cached_native_registration(
                    shared,
                    declaring_name,
                    &method_name,
                    &descriptor,
                )
            {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
                drop(cm);
                let target = CachedInvokeTarget::VirtualNative {
                    receiver_class_id,
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
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
    }

    // WP2.2 / Surefire — never promote `VirtualBytecode` for `Method.invoke` or
    // `Constructor.newInstance`; the monomorphic fast path skips
    // `try_stackless_invoke` and would execute JDK bytecode instead of the
    // Rust overrides registered in `register_essential_natives`.
    let declaring_for_jython = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
    if declaring_for_jython == "org/python/core/PyObject"
        && method_name.as_ref() == "__findattr__"
        && descriptor.as_ref() == "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        let receiver_name = store.get(receiver_class_id).map(|c| &*c.name).unwrap_or("");
        if receiver_name == "org/python/core/PyModule" {
            if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
                shared,
                "org/python/core/PyModule",
                &method_name,
                &descriptor,
            ) {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
                drop(cm);
                let target = CachedInvokeTarget::VirtualNative {
                    receiver_class_id,
                    callback,
                    native_id,
                    native_kind,
                    num_params: num_params as u16,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
    }

    let declaring_for_pyjavatype = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
    if declaring_for_pyjavatype == "org/python/core/PyType"
        && method_name.as_ref() == "__findattr_ex__"
        && descriptor.as_ref() == "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        let receiver_name = store.get(receiver_class_id).map(|c| &*c.name).unwrap_or("");
        if receiver_name == "org/python/core/PyJavaType" {
            if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
                shared,
                "org/python/core/PyJavaType",
                &method_name,
                &descriptor,
            ) {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
                drop(cm);
                let target = CachedInvokeTarget::VirtualNative {
                    receiver_class_id,
                    callback,
                    native_id,
                    native_kind,
                    num_params: num_params as u16,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
    }

    let declaring_for_reflect = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
    if native_override_for_cached_reflect_invoke(
        shared,
        declaring_for_reflect,
        method_name.as_ref(),
        descriptor.as_ref(),
    )
    .is_some()
    {
        let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
            shared,
            declaring_for_reflect,
            method_name.as_ref(),
            descriptor.as_ref(),
        ) else {
            return;
        };
        let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
        drop(cm);
        let target = CachedInvokeTarget::VirtualNative {
            receiver_class_id,
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
        thread.invoke_cache.put_poly(
            caller_class_id,
            cp_index,
            false,
            receiver_class_id,
            target.clone(),
        );
        thread
            .invoke_cache
            .put(caller_class_id, cp_index, false, target);
        return;
    }

    let Some(code_attr) = method.code() else {
        return;
    };

    let class = cm.get_class(declaring_id);
    let source_file = class.and_then(|c| c.source_file.as_deref()).map(Arc::from);
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
            intercept_shape_cache: std::sync::OnceLock::new(),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };

    // WP2.4-F1: snapshot before dropping the class_manager read-lock so
    // we get the same Arc<AtomicU32> that `redefine_class` will later
    // bump.  Bind to the declaring class — that's the class whose
    // method body lives in `cached`.
    let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
    drop(cm);
    let target = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id,
        cached: std::sync::Arc::new(cached),
        gate,
    };
    // T10.4 — promote so sibling threads dispatching the same call-site
    // skip the class_manager read-lock and find_method_recursive walk.
    shared
        .classes
        .shared_resolution
        .insert_promoted_invoke(promoted_key, target.clone());
    thread.invoke_cache.put_poly(
        caller_class_id,
        cp_index,
        false,
        receiver_class_id,
        target.clone(),
    );
    thread
        .invoke_cache
        .put(caller_class_id, cp_index, false, target);
}

/// Spring's `MergedAnnotation$Adapt.isIn` loader-split bridge (see the guard
/// at the top of `execute_invokevirtual_cached`) applies only to call sites
/// whose constant pool actually names that method. This latches the first
/// time [`resolve_method_metadata`] resolves such an entry, so the guard costs
/// one relaxed atomic load per invoke instead of a full method-ref resolution
/// in every process that has never loaded Spring.
pub(super) static ADAPT_ISIN_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The class whose `isIn` call sites [`ADAPT_ISIN_SEEN`] arms for.
pub(super) const SPRING_MERGED_ANNOTATION_ADAPT: &str =
    "org/springframework/core/annotation/MergedAnnotation$Adapt";

#[inline]
pub(super) fn adapt_isin_seen() -> bool {
    ADAPT_ISIN_SEEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// The census half of this file's intrinsic inline-cache fill.
///
/// The helpers themselves live in `dispatch_static.rs` (with the rest of the
/// interpreter intrinsic table) and are unit-tested there. What is specific to
/// THIS file, and what these tests pin, is **which registry row it marks**:
/// unlike the `invokestatic` twin, this block reaches its intrinsic through the
/// class store and never resolved a `NativeMethodId`, so one has to be looked
/// up — and looking up the wrong one is a silent failure.
///
/// Recorded in
/// `docs/known-issues/jdk-only/G42-1-the-intrinsic-cache-and-the-1999-20260817.md`.
#[cfg(test)]
mod intrinsic_census_virtual_tests {
    use super::mark_intrinsic_cache_bypass;
    use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
    use cratonvm_types::error::MethodCallResult;
    use cratonvm_types::Value;

    fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }

    /// Reproduces the exact two-step this file performs — `resolve_id` on the
    /// **declaring** class while the class-manager read guard is still alive,
    /// then `mark_intrinsic_cache_bypass` once the guard is dropped — and pins
    /// that it lands on the declaring row.
    ///
    /// The receiver's own class is the tempting wrong answer, and it is wrong
    /// twice over: the superclass walk above the call site has already
    /// established that nothing is registered strictly below the declaring
    /// class (`native_override_below_declaring`, which would have *vetoed* the
    /// intrinsic if it had found one), so a receiver-keyed lookup would resolve
    /// nothing and mark nothing — leaving the row that actually loses the calls
    /// still claiming to be a total.
    ///
    /// `java/lang/Object.hashCode()I` is the concrete case: G33-1 §2 measured
    /// it reporting **1** for 100,000 interpreted calls.
    #[test]
    fn the_declaring_classes_row_is_the_one_that_loses_the_calls() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("java/lang/Object", "hashCode", "()I", dummy_native);
        });
        // The receiver class in this scenario registers nothing — that is
        // precisely why the intrinsic was allowed to win.
        assert!(
            registry
                .resolve_id("p/Receiver", "hashCode", "()I")
                .is_none(),
            "the walk would have vetoed the intrinsic if the receiver had a native"
        );

        let declaring_name = "java/lang/Object";
        let bypassed_native_id = registry.resolve_id(declaring_name, "hashCode", "()I");
        let id = bypassed_native_id.expect("the declaring class carries the row");

        registry.record_invocation(id);
        mark_intrinsic_cache_bypass(&registry, id);

        assert_eq!(
            registry.invocations_of_id(id),
            Some(1),
            "the floor survives — this is the measured `Object.hashCode` = 1"
        );
        assert_eq!(registry.invocations_complete(id), Some(false));
        assert_eq!(registry.slots_with_incomplete_invocations(), 1);
    }

    /// A record's `hashCode`/`equals` intrinsic (`record_object_intrinsic`) is
    /// not keyed on a fixed class and has no registry row at all, so the
    /// resolve yields `None` and the fill proceeds unmarked. That is correct:
    /// a triple with no row has no `invocations` cell to mislead anyone.
    ///
    /// It must also not panic — this is the inline-cache fill path for every
    /// virtual call in the VM.
    #[test]
    fn an_intrinsic_with_no_registry_row_leaves_the_census_untouched() {
        let registry = NativeMethodRegistry::new();
        assert!(registry
            .resolve_id("p/SomeRecord", "hashCode", "()I")
            .is_none());
        assert_eq!(registry.slots_with_incomplete_invocations(), 0);
    }
}
