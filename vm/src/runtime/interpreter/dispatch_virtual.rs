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

use super::site_cache::{site_stats, IfaceSelectSiteCache};
use super::*;

/// Kill switch for the interface receiver-selection memo
/// (`CRATONVM_JIT_NO_IFACE_SELECT_MEMO=1`, or
/// `CRATONVM_JIT=-iface-select-memo`). Set, every `invokeinterface` cache hit
/// takes the `class_manager` read lock and walks the hierarchy again, exactly
/// as it did before the memo — which is what makes the two arms comparable
/// inside one binary. Gates the read AND the write, so a disabled run cannot
/// leave entries behind for an enabled one to redeem.
#[inline]
fn iface_select_memo_disabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_IFACE_SELECT_MEMO").is_some()
    })
}

/// The receiver class a receiver-guarded invoke-cache target was filled for,
/// and the parameter slot count (receiver excluded) that locates the receiver
/// on the operand stack. `None` for the unguarded kinds (`Bytecode`, `Native`,
/// `Jit`, a static `Intrinsic`).
#[inline]
fn receiver_guard_of(target: &CachedInvokeTarget) -> Option<(ClassId, usize)> {
    match target {
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id,
            cached,
            ..
        } => Some((*receiver_class_id, cached.num_params as usize)),
        CachedInvokeTarget::VirtualNative {
            receiver_class_id,
            num_params,
            ..
        } => Some((*receiver_class_id, *num_params as usize)),
        CachedInvokeTarget::Intrinsic {
            receiver_class_id: Some(receiver_class_id),
            num_params,
            ..
        } => Some((*receiver_class_id, *num_params as usize)),
        _ => None,
    }
}

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

/// The package part of an internal class name (`java/util` for
/// `java/util/List`), `""` for the default package. A string split, no lock:
/// the vtable fast path uses it to decide whether §5.4.6 selection can
/// disagree with the vtable at all before paying for the walk.
#[inline]
fn package_of_internal_name(name: &str) -> &str {
    name.rfind('/').map_or("", |i| &name[..i])
}

/// Upper bound on `JvmThread::native_shadow_cache`; the map is cleared
/// wholesale when it fills.
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

/// T10.9.A — VtableManager path for invokevirtual / invokeinterface.
///
/// Consulted on an inline-cache MISS: the interpreter's `0xb6`/`0xb9` arms
/// try the fast doors and `execute_invokevirtual_cached` first, and come here
/// only when those decline. It is not lock-free: steps 3-4 below take the
/// resolution-cache and vtable-manager read locks, and the native-shadow and
/// receiver-selection checks further down take `class_manager.read()`. The
/// vtable carries a fully-built `Arc<CachedBytecodeMethod>` populated at
/// class-link time.
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
    //
    // The count that miss saw, read before Step 2 reads the caller's pool:
    // the fills at the end store against it (`put_as_of`), because the
    // monitor enter and the frame push between here and there are not
    // assumed free of Java (interpreter round i1 wave 19, lane L4).
    let fill_as_of = thread.invoke_cache.redefinitions_seen();

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
    if is_spring_adapt_isin(&method_class_name, &method_name, &method_descriptor) {
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
    //
    // `Object.equals(Object)` used to be refused here and in both populators
    // (Brave's `WeakKey.equals` / `TraceContext.equals`, 2026-07-15), on the
    // grounds that a constant-pool index is not a call-site identity. Every
    // entry this path and `populate_virtual_invoke_cache` install is
    // receiver-guarded, so a shared Methodref cannot serve one receiver class
    // with another's target; the one unguarded kind (`Bytecode`) is admitted
    // for a virtual site only for a private target. See
    // `docs/internal/fixed-bugs/interpreter-L3-object-equals-sites-never-cached-FIXED-20260923.md`.

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
        // A null RECEIVER is not the null-name contract: it owes the JEP 358
        // `Cannot invoke "java.lang.ClassLoader.…"` NPE, which the receiver
        // match below defers to the slow path. Without this the native ran
        // with a null `this` and answered with its own message instead.
        && matches!(receiver_val, Value::Object(Some(_)))
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
            // Defer to the slow path (`execute_invoke_kind`), which raises
            // the JEP 358 NPE with its `because` clause. (The `Compatible`
            // null-receiver shims this used to defer to were deleted in
            // interpreter round i1 wave 10.)
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
    // dispatch. See bug-h2-suite-residual-fail-triage-FIXED.md.
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
    if shared.classes.is_lambda_proxy_class(receiver_class_id) {
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
    //
    // `receiver_vtable_shadows` is read under the same guard (no extra lock):
    // whether the receiver's vtable holds THIS call's signature in more than
    // one slot, which only the loader-aware §5.4.5 policy records (see Step
    // 4). Per signature since round 13 wave 5 (lane proxy3): the other calls
    // on such a receiver have one slot and need no validation.
    let receiver_vtable_shadows;
    {
        let cm = shared.classes.class_manager.read();
        receiver_vtable_shadows =
            cm.vtable_shadows_signature(receiver_class_id, &method_name, &method_descriptor);
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
                // Round 13 wave 5 (lane proxy3): since round 13 wave 4 the
                // slow path itself runs the generated `$ProxyN` body for a
                // call its class declares (`proxy_call_takes_ordinary_dispatch`)
                // and caches it keyed by the receiver class, which is exactly
                // right: every such proxy method has its own body, so no
                // instance of the class can reach an interface default through
                // the slot. This path may therefore serve that call too; only
                // the calls the slow path still intercepts (the synthetic shim,
                // undeclared methods, an `AnnotationProxy` handler under
                // `CRATONVM_PROXY_ANNOTATION_BODY=0`, the switch off) are ceded. Asked under the guard this block holds.
                // `CRATONVM_PROXY_VTABLE_FAST_PATH=0` cedes every proxy call.
                if is_proxy
                    && !(cratonvm_native_builtins::reflect_annotations::proxy_vtable_fast_path()
                        && proxy_receiver_runs_its_body_in(
                            shared,
                            &cm,
                            receiver_obj,
                            receiver_class_id,
                            &method_name,
                            &method_descriptor,
                        ))
                {
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
            // The registry generation is folded into the fingerprint: a
            // `false` verdict memoized before `alias_class` or a lazy
            // `register_*` pass appended a shadowing native would otherwise
            // stay `false` for the life of the thread, and this site would keep
            // running the bytecode the native now owns. Same keying rule as
            // `NativeCallSite`.
            let registry_generation = u64::from(shared.natives.native_methods.generation());
            let native_shadow_cache_key = Some(vtable_native_shadow_cache_key(
                receiver_class_id,
                hierarchy_fingerprint_in(&cm, receiver_class_id)
                    ^ registry_generation.wrapping_mul(0x9E37_79B9_7F4A_7C15),
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
                                )
                                // A `Bridge` above a retired row is not a
                                // shadow under `--jdk-only` (the populate's
                                // walk masks it too; round 14 wave 2, lane
                                // shadow), so the vtable's bytecode stands.
                                && !crate::runtime::interpreter::retired_row_masks_ancestor_bridge(
                                    shared,
                                    &cm,
                                    (receiver_class_id, None),
                                    parent_id,
                                    &method_name,
                                    &method_descriptor,
                                    shared
                                        .natives
                                        .native_methods
                                        .resolve_id(&parent.name, &method_name, &method_descriptor)
                                        .and_then(|id| shared.natives.native_methods.kind_of_id(id))
                                        .unwrap_or(cratonvm_native_api::NativeKind::Bridge),
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
        //
        // The same holds for an `invokevirtual` whose vtable pick lives in a
        // different package from the constant-pool class: the vtable build
        // overrides by (name, descriptor) alone, and a package-private
        // resolved method is not overridden from another package (JVMS
        // §5.4.5). For javac output that is the only shape in which the slot
        // and §5.4.6 selection can disagree (a package-private method is a
        // member only of classes in its own package, so the CP class naming
        // it is in that package), and `java/lang/Object` declares none. Only
        // a STRICT selection overrides the slot on this arm; a lenient one
        // (a compatibility shape) keeps today's vtable answer.
        //
        // Under the loader-aware policy (both modes since 2026-09-27, unless
        // `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`) a package-private method is
        // not overridden from another RUNTIME package either — same package
        // name, another defining loader — so a receiver's vtable can hold one
        // signature twice with both slots' classes in one package NAME, and
        // `lookup_slot` answers the lower slot whatever the call site
        // resolved (`invokevirtual Sub'.m` on a child loader's `Sub'` would
        // get the application `A.m` it does not override). Such receivers are
        // recorded at link time (`ClassManager::vtable_shadows_signature`)
        // and validated here the same way.
        let cross_package = !is_interface
            && method_class_name.as_ref() != "java/lang/Object"
            && (receiver_vtable_shadows
                || package_of_internal_name(&cached.class_name)
                    != package_of_internal_name(&method_class_name));
        if is_interface || cross_package {
            let cm = shared.classes.class_manager.read();
            let resolved = crate::runtime::resolve::selection::resolved_ref_from_caller(
                &cm,
                caller_class_id,
                &method_class_name,
                &method_name,
                &method_descriptor,
            );
            // JVMS §6.5 `invokeinterface`: a receiver whose class does not
            // implement the resolved interface is an
            // `IncompatibleClassChangeError`, which only the slow path raises.
            // Selection alone would happily find the receiver's same-named
            // public method, so ask first.
            if let Some(crate::runtime::resolve::selection::ResolvedRef::Interface(Some(iface))) =
                resolved
            {
                if crate::runtime::resolve::selection::receiver_does_not_implement(
                    &cm,
                    receiver_class_id,
                    iface,
                )
                .is_some()
                {
                    drop(cm);
                    return Ok(CachedCallResult::CacheMiss);
                }
            }
            let receiver_selected = if is_interface {
                crate::runtime::resolve::selection::select_or_lenient(
                    &cm.class_store,
                    receiver_class_id,
                    resolved,
                    &method_name,
                    &method_descriptor,
                )
                .map(|(_, declaring_id)| declaring_id)
            } else {
                use crate::runtime::resolve::selection::{select, Selection};
                match resolved.map(|r| {
                    select(
                        &cm.class_store,
                        receiver_class_id,
                        r,
                        &method_name,
                        &method_descriptor,
                    )
                }) {
                    Some(Selection::Selected(declaring_id)) => Some(declaring_id),
                    Some(Selection::AbstractMethod(_))
                    | Some(Selection::ConflictingDefaults(..))
                    | Some(Selection::NotPublic(_)) => None,
                    Some(Selection::Lenient) | None => Some(cached.declaring_class_id),
                }
            };
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
    if thread.frames.at_frame_limit(0, shared.config.max_stack_depth) {
        dump_stack_on_soe(thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }

    if crate::jit::profile::is_receiver_profiling_enabled()
        && !thread.frames[frame_idx].runs_obsolete_method()
    {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        record_receiver_memoized(
            shared,
            cid,
            mn,
            md,
            site_pc,
            receiver_class_id.as_u32(),
            thread.frames[frame_idx].code.len(),
        );
    }

    let total_args = num_params + 1;
    const MAX_INLINE_ARGS: usize = 16;
    // Decode args bit-exact via parameter descriptors (receiver slot = 'L').
    // The prior pop_unchecked()/to_value() dropped the high bits of a
    // category-2 long arg whose NaN-box bit pattern collides with a tagged
    // sub-tag (BC safegcd 0xFFFC_… accumulators). See
    // bc-ec-mod-mododdinverse-investigation.md.
    // ONE forward scan for the whole descriptor. This closure used to call
    // `nth_param_tag_byte` per argument, and that rescans from `(` each time,
    // so popping N args cost O(N^2) tokenising of a string fixed per call site.
    let param_tags = ParamTags::for_method(&entry_cached);
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
    install_cached_frame(
        shared,
        thread,
        Arc::clone(&entry_cached),
        args_slice,
        monitor_obj,
        None,
        false,
    );

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
    // The gate is snapshotted AFTER the vtable read above, so a redefinition
    // that completed in between would pair the OLD body with the NEW
    // generation — an entry that never goes stale, published to every thread
    // below. A redefinition installs its vtables, bumps the generation and
    // raises `any_class_redefined` all under the class-manager writer, and the
    // snapshot's reader lock orders after that, so the latch is the exact
    // condition under which the slot must be re-read. Only this call's frame
    // (already pushed, running the body the vtable held when the call began)
    // is kept; the stale pairing is simply not cached.
    if crate::classloading::any_class_redefined() {
        let still_current = {
            let guard = shared.classes.vtable_manager.read();
            guard
                .get_vtable(receiver_class_id.as_u32() as u64)
                .and_then(|v| {
                    v.lookup_slot(&method_name, &method_descriptor)
                        .and_then(|slot| v.get(slot))
                })
                .and_then(|e| e.resolved_method.as_ref())
                .is_some_and(|c| Arc::ptr_eq(c, &entry_cached))
        };
        if !still_current {
            return Ok(CachedCallResult::FramePushed);
        }
    }
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
        .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
    thread.invoke_cache.put_poly_as_of(
        caller_class_id,
        cp_index,
        false,
        site_pc as u32,
        receiver_class_id,
        fill_as_of,
        target.clone(),
    );
    thread.invoke_cache.put_as_of(
        caller_class_id,
        cp_index,
        false,
        site_pc as u32,
        fill_as_of,
        target,
    );

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
        // Interpreter round i1 wave 24 (lane L1): a JDWP breakpoint, step,
        // suspension or method event concerning the getter needs its frame
        // — answered without one, a breakpoint on `return x;` in a getter
        // never stopped and a step into it skipped it.
        || debugger_concerns_method(
            shared,
            cached.declaring_class_id,
            &cached.method_name,
            &cached.method_descriptor,
        )
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

#[inline]
pub(super) fn execute_invokevirtual_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    site_pc: usize,
    is_special: bool,
    is_interface: bool,
) -> Result<CachedCallResult, MethodCallFailed> {
    execute_invokevirtual_cached_probed(
        shared,
        thread,
        frame_idx,
        cp_index,
        site_pc,
        is_special,
        is_interface,
        DoorProbe::NotProbed,
    )
}

/// [`execute_invokevirtual_cached`], starting from what a fast door already
/// read from the inline cache for this site (see [`DoorProbe`]), so a door
/// decline costs one probe of the key, not two. Only for the key the door
/// probed: the same `is_special` and `site_pc`. The virtual door probes the
/// non-special key; the non-virtual door (`invoke_fast::execute_nonvirtual_fast_door`,
/// since interpreter round i1 wave 22) the special one, and hands over only
/// `Found` / `Miss` — the `Primary` placeholder-gate shape is virtual-only.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_invokevirtual_cached_probed(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    site_pc: usize,
    is_special: bool,
    is_interface: bool,
    door_probe: DoorProbe,
) -> Result<CachedCallResult, MethodCallFailed> {
    let caller_class_id = thread.frames[frame_idx].class_id;
    debug_assert!(
        !is_special || !matches!(door_probe, DoorProbe::Primary { .. }),
        "the non-virtual door hands over only `Found` / `Miss`"
    );

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
                let cached =
                    thread
                        .invoke_cache
                        .get(caller_class_id, cp_index, is_special, site_pc as u32);
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

    // Spring's loader-split `MergedAnnotation$Adapt.isIn` identity bridge must
    // stay in the slow dispatcher. That is enforced where entries are MADE —
    // `populate_virtual_invoke_cache` and the vtable path both refuse the
    // triple (`is_spring_adapt_isin`) — so no entry for it can exist here.
    //
    // It used to be enforced HERE, per call: once a process-global latch saw
    // any constant pool name the method, every cached invokevirtual/-interface/
    // -special in the process paid a full `resolve_method_ref` (resolution-
    // cache lock, hash probe, four `Arc` clone/drop pairs), and the virtual
    // fast door declined every call, just to recognise one Spring method. The
    // latch was also shared by every VM in the process, against AGENTS.md's
    // "no process globals for compatibility state".

    fn is_classloader_null_name_method(name: &str, descriptor: &str) -> bool {
        matches!(
            (name, descriptor),
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
    }

    // Set for a `DoorProbe::Primary` target, whose gate is the placeholder;
    // cleared if the receiver-first swap below replaces the target.
    let mut gate_deferred = false;
    let probed = match door_probe {
        DoorProbe::Found(t) => Some(t),
        DoorProbe::Primary {
            receiver_class_id,
            cached,
        } => {
            gate_deferred = true;
            Some(CachedInvokeTarget::VirtualBytecode {
                receiver_class_id,
                cached,
                gate: DEFERRED_DOOR_GATE.with(RedefineGate::clone),
            })
        }
        DoorProbe::Miss => None,
        DoorProbe::NotProbed => thread
            .invoke_cache
            .get(caller_class_id, cp_index, is_special, site_pc as u32)
            .cloned(),
    };
    let target = match probed {
        Some(t) => {
            dbg_invoke_stats_record(0);
            t
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
    // The redefinition count as of the lookup that produced `target` (this
    // function's own probe, or the door's just before it). The `Bytecode`
    // arm's upgrade to a `Jit` entry stores against it (`put_as_of`): the
    // constant-pool check below and the arm's interceptors can run Java.
    let entry_as_of = thread.invoke_cache.redefinitions_seen();

    // A previous non-null invocation may have cached the real-JDK bytecode
    // body of an inherited ClassLoader method.  That body does not reliably
    // enforce the public null-name contract for a synthetic embedded loader,
    // whereas the registered ClassLoader natives do.  Otherwise
    // `resources("...")` poisons the same CP entry and a later
    // `resources(null)` silently returns a Stream.
    //
    // Checked AFTER the cache probe, not before it: `peek_at(0)` is the LAST
    // ARGUMENT, so every `map.put(k, null)`, `list.add(null)` or
    // `append((String) null)` used to pay a full `resolve_method_ref` (a
    // resolution-cache lock, a probe and four `Arc` clone/drop pairs) just to
    // learn it was not one of these five methods. A bytecode-backed entry names
    // its method, and virtual selection matches the reference's name and
    // descriptor exactly, so it answers that question for free; an entry that
    // names nothing still resolves, exactly as before. A miss above returns
    // `CacheMiss` either way, which is the outcome this block produces.
    if !is_special
        && matches!(
            thread.frames[frame_idx].stack.peek_at(0),
            Value::Object(None)
        )
    {
        let may_be_classloader_contract = match &target {
            CachedInvokeTarget::Bytecode { cached, .. }
            | CachedInvokeTarget::VirtualBytecode { cached, .. }
            | CachedInvokeTarget::Jit { cached, .. } => {
                is_classloader_null_name_method(&cached.method_name, &cached.method_descriptor)
            }
            // A native entry carries no name, but its parameter count is the
            // reference's (receiver excluded), and all five methods take one.
            CachedInvokeTarget::Native { num_params, .. }
            | CachedInvokeTarget::VirtualNative { num_params, .. } => *num_params == 1,
            _ => true,
        };
        if may_be_classloader_contract {
            if let Ok((method_class_name, method_name, method_descriptor, _)) =
                resolve_method_ref(shared, caller_class_id, cp_index)
            {
                if method_class_name.as_ref() == "java/lang/ClassLoader"
                    && is_classloader_null_name_method(&method_name, &method_descriptor)
                {
                    thread
                        .invoke_cache
                        .evict(caller_class_id, cp_index, is_special);
                    return Ok(CachedCallResult::CacheMiss);
                }
            }
        }
    }
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
            // ...AND WHEN THE REDEFINED CLASS IS AN ANCESTOR OF THE RECEIVER.
            //
            // The exact-class test above is the whole check a `VirtualNative`
            // shadow ever got at HIT time, and it cannot see the shape Mockito
            // produces for an ABSTRACT class: the inline mock maker weaves
            // advice into the class itself (`java.io.InputStream`) and hands
            // back a generated SUBCLASS that overrides only the abstract
            // methods and the identity plumbing. The receiver is that subclass,
            // whose redefine generation is and stays 0, while the class whose
            // bytecode the native shadows is its superclass.
            //
            // `populate_virtual_invoke_cache`'s own comment already names this
            // ("`execute_invokevirtual_cached`'s eviction check only inspects
            // the RECEIVER class's redefine generation ... so it never catches
            // a shadow whose declaring class is an ancestor") and concludes
            // "the fix has to be here, where the entry is created". Guarding
            // creation is necessary and is not sufficient: an entry created or
            // PROMOTED (`insert_promoted_invoke` publishes to sibling threads)
            // before the agent retransformed the ancestor is never revisited,
            // because nothing at hit time asks the ancestor's generation.
            //
            // Measured 2026-08-30 on
            // `org.springframework.http.client.SimpleClientHttpResponseTests`.
            // A backtrace from inside the `transferTo` native named this door:
            // `execute_invokevirtual_cached` -> `invoke_cached_native_callback`
            // -> `safe_native_call_impl`. The buffer size settles which body
            // ran without a debugger -- CratonVM's native copies through
            // 16 MiB, the JDK body through 16384, and the mock saw 16777216 on
            // every call where HotSpot saw 16384.
            //
            // `hierarchy_was_redefined` is the question, and it already existed
            // in `redefine_state` with no callers. It is behind
            // `any_class_redefined()` twice over (here and in its own first
            // line), so a process with no agent pays one relaxed atomic load
            // and never walks a chain.
            //
            // The immunity allowlists are consulted exactly as the `Native` arm
            // below consults them, and for the same reason: without them,
            // mocking one `StringBuilder` (or one of the synthetic collections)
            // anywhere in the process would evict a native shadow that the real
            // JDK bytecode cannot replace, because CratonVM's instances do not
            // carry the layout that bytecode assumes.
            if crate::runtime::redefine_state::hierarchy_was_redefined(shared, cid) {
                let immune = resolve_method_ref(shared, caller_class_id, cp_index).is_ok_and(
                    |(mcn, mn, desc, _)| redefine_immune_forced_native(&mcn, &mn, &desc),
                );
                if !immune {
                    thread
                        .invoke_cache
                        .evict(caller_class_id, cp_index, is_special);
                    return Ok(CachedCallResult::CacheMiss);
                }
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
    if is_special && crate::jit::profile::is_receiver_profiling_enabled()
        && !thread.frames[frame_idx].runs_obsolete_method()
    {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared
            .jit
            .profile_store
            .record_call_site_borrowed(cid, mn, md, site_pc, thread.frames[frame_idx].code.len());
    }

    // Receiver first, target second. The primary slot holds whichever receiver
    // class called through this site last; when the receiver on the stack is a
    // different class, the per-site poly entries are consulted for EVERY
    // receiver-guarded kind here. The arms below used to do it only for
    // `VirtualBytecode`, and only when the poly entry was again
    // `VirtualBytecode`, so a site mixing a native or intrinsic target with a
    // bytecode one (`CharSequence.length()` over `String` and
    // `StringBuilder`) fell to the vtable / slow path on every alternation.
    // After a redefinition only a `VirtualBytecode` poly entry is swapped in:
    // the redefine checks above looked at the primary entry only, and they
    // concern native and intrinsic SHADOWS, which a bytecode entry is not (a
    // primary `VirtualBytecode` hit is served past them unchecked). This used
    // to skip the swap for every kind for the rest of the process after the
    // first redefinition anywhere (interpreter round i1 wave 18, lane L4).
    // Nothing is popped or recorded here; the arm the swapped target selects
    // does both, exactly as for a primary hit.
    let target = match receiver_guard_of(&target) {
        Some((guard, num_params))
            if invoke_fast::DOORS_SURVIVE_REDEFINITION
                || !crate::classloading::any_class_redefined() =>
        {
            match thread.frames[frame_idx].stack.peek_at(num_params) {
                Value::Object(Some(obj_ref)) => {
                    let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
                    let actual = shared.mem.heap.class_id_of(obj_ref);
                    if actual != guard
                        && shared.mem.heap.kind_of(obj_ref) != cratonvm_types::ObjectKind::Array
                    {
                        match thread.invoke_cache.get_poly(
                            caller_class_id,
                            cp_index,
                            is_special,
                            site_pc as u32,
                            actual,
                        ) {
                            Some(poly)
                                if receiver_guard_of(&poly).is_some_and(|(g, _)| g == actual)
                                    && (matches!(
                                        poly,
                                        CachedInvokeTarget::VirtualBytecode { .. }
                                    ) || !crate::classloading::any_class_redefined()) =>
                            {
                                gate_deferred = false;
                                poly
                            }
                            _ => target,
                        }
                    } else {
                        target
                    }
                }
                _ => target,
            }
        }
        _ => target,
    };

    // A `Jit` entry under a non-virtual key (an `invokespecial`, or a private
    // `invokevirtual`) is the `Bytecode` arm's own upgrade (below): it runs
    // through that arm with the entry's body in hand, so every check the arm
    // makes before entering compiled code — the null-receiver defer, the
    // loader-split owner re-check, the stack-depth check, the interceptors —
    // still runs, and a body that declines (`execute_jit_call_decoded` answers
    // `None`) falls back to the arm's interpreted push. A static target stays a
    // `Jit` entry: under this key it is an `invokestatic` fill of the same
    // `Methodref`, which the `Jit` arm refuses. See
    // `invoke_fast::NONVIRTUAL_ARM_UPGRADES_TO_JIT`.
    let (target, entry_body) = match target {
        CachedInvokeTarget::Jit {
            compiled,
            cached,
            gate,
            ..
        } if invoke_fast::NONVIRTUAL_ARM_UPGRADES_TO_JIT && !cached.is_static => (
            CachedInvokeTarget::Bytecode { cached, gate },
            Some(compiled),
        ),
        other => (other, None),
    };

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
                    if crate::jit::profile::is_receiver_profiling_enabled()
                        && !thread.frames[frame_idx].runs_obsolete_method()
                    {
                        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                        record_receiver_memoized(
                            shared,
                            cid,
                            mn,
                            md,
                            site_pc,
                            actual_class_id.as_u32(),
                            thread.frames[frame_idx].code.len(),
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
                            site_pc as u32,
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
                                gate_deferred = false;
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
                    //
                    // ── Why this is memoized (2026-09-02) ────────────────
                    //
                    // The answer is a property of `(actual_class_id, method
                    // name, descriptor)`, all three fixed for as long as the
                    // entry is valid — but it was being re-derived on EVERY
                    // `invokeinterface` cache hit, with a `class_manager` read
                    // lock and a full `find_method_recursive` hierarchy walk.
                    //
                    // `invokeinterface` and `invokevirtual` reach this same
                    // function and differ in exactly this block, so the cost is
                    // directly attributable: `probes/Dispatch.java` (`--nojit`,
                    // min-of-7, arms interleaved) put interface-over-virtual at
                    // **114 ns**, against **3.4 ns** on HotSpot's template
                    // interpreter.
                    //
                    // Two steps, cheapest first:
                    //
                    //  1. If the receiver's own class IS the cached method's
                    //     declaring class, receiver-rooted selection starts
                    //     there and finds it there. Nothing to check — one
                    //     integer compare replaces the whole block.
                    //  2. Otherwise consult the per-thread memo, which stores
                    //     the `(receiver, declaring)` pair a previous walk
                    //     verified. Both halves are compared, because one
                    //     interface site can see several receiver classes and
                    //     the answer belongs to the receiver, not the site; a
                    //     rotating site simply misses and re-walks, which is
                    //     exactly today's behaviour.
                    //
                    // Validity is `SiteCache`'s, and here that set is precise
                    // rather than merely sufficient: a class's superclass and
                    // superinterface chain is fixed at load time, so the walk's
                    // answer can only move under a JVMTI redefine (which moves
                    // the resolution epoch since interpreter round i1 wave 17;
                    // see `site_cache::SITE_CACHES_SURVIVE_REDEFINITION`)
                    // or an `upgrade_synthetic_class` /
                    // `recompute_subclass_layouts` rewrite under an unchanged
                    // `ClassId` (the resolution epoch, which the invalidate
                    // hook bumps and which nothing else on the invoke-cache
                    // path observes).
                    //
                    // The kill switch disables BOTH steps, not just the memo.
                    // A switch that left the short-circuit in place would not
                    // restore the pre-change path, so its "off" arm would not
                    // be a control — and this is not hypothetical: the first
                    // A/B of this change compared an arm the switch could not
                    // reach and separated nothing.
                    let iface_fast_off = iface_select_memo_disabled();
                    if is_interface
                        && (iface_fast_off || actual_class_id != cached.declaring_class_id)
                    {
                        let memo_hit = !iface_fast_off
                            && thread
                                .iface_select_sites
                                .get(caller_class_id, cp_index)
                                .is_some_and(|&(recv, decl)| {
                                    recv == actual_class_id && decl == cached.declaring_class_id
                                });
                        if memo_hit {
                            site_stats::bump(site_stats::IFACE_SELECT_HIT);
                        } else {
                            site_stats::bump(site_stats::IFACE_SELECT_MISS);
                            // Read BEFORE the walk; see `SiteCache::put`.
                            let epochs_at_entry = IfaceSelectSiteCache::epochs_for(shared);
                            let cm = shared.classes.class_manager.read();
                            // JVMS §5.4.6, the same rule every fill site
                            // uses (`runtime::resolve::selection`); an
                            // interface method is public, so the interface's
                            // identity does not enter the answer.
                            let receiver_selected =
                                crate::runtime::resolve::selection::select_or_lenient(
                                    &cm.class_store,
                                    actual_class_id,
                                    Some(
                                        crate::runtime::resolve::selection::ResolvedRef::Interface(
                                            None,
                                        ),
                                    ),
                                    &cached.method_name,
                                    &cached.method_descriptor,
                                )
                                .map(|(_, declaring_id)| declaring_id);
                            drop(cm);
                            if receiver_selected != Some(cached.declaring_class_id) {
                                thread
                                    .invoke_cache
                                    .evict(caller_class_id, cp_index, is_special);
                                return Ok(CachedCallResult::CacheMiss);
                            }
                            if !iface_fast_off {
                                site_stats::bump(site_stats::IFACE_SELECT_FILL);
                                thread.iface_select_sites.put(
                                    caller_class_id,
                                    cp_index,
                                    epochs_at_entry,
                                    (actual_class_id, cached.declaring_class_id),
                                );
                            }
                        }
                    } else if is_interface {
                        site_stats::bump(site_stats::IFACE_SELECT_TRIVIAL);
                    }

                    if thread.frames.at_frame_limit(0, shared.config.max_stack_depth) {
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
                    // bc-ec-mod-mododdinverse-investigation.md.
                    // ONE forward scan; the per-argument form rescanned from `(` each time.

                    let param_tags = ParamTags::for_method(&cached);

                    let arg_desc_byte = |i: usize| -> u8 {
                        param_tags.get_with_receiver(&cached.method_descriptor, i)
                    };
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
                    // warm — see core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md
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
                    // docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md.
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
                    // `CRATONVM_DBG_TIERUP_DECLINE=1` — name the FIRST
                    // condition below that refuses this site, per method. The
                    // chain gates the invocation COUNTER as well as the
                    // promotion, so a method refused here is a method
                    // `jit-method-stats` cannot see and `CRATONVM_JIT_THRESHOLD`
                    // cannot reach. Mirrors the `&&` order exactly.
                    if crate::runtime::interp_census::tierup_decline_enabled() {
                        let reason = if is_special {
                            "is_special"
                        } else if matches!(thread.kind, crate::threading::ThreadKind::Virtual) {
                            "virtual_thread"
                        } else if cached.is_synchronized {
                            "synchronized"
                        } else if crate::runtime::interpreter::invoke_fast::tier_up_skips_redefined_target(
                            entry_gate.generation,
                        ) {
                            "entry_gate_generation"
                        } else if crate::runtime::env_cache::disable_jit() {
                            "nojit"
                        } else if has_registered_native() {
                            "registered_native"
                        } else if !crate::runtime::env_cache::jit_virtual_tierup() {
                            "virtual_tierup_off"
                        } else {
                            // MIRRORS THE `&&` CHAIN, INCLUDING THE TWO 2026-09-02
                            // SWITCHES — which it did not until 2026-09-11, and
                            // that is why item 2 of
                            // `composition-native-callback-and-the-promotion-question-20260902.md`
                            // could not read its own arm. This chain still
                            // tested `receiver_is_java_util` and the exception
                            // table in the ORDER and the SENSE they had before
                            // nomination was separated from promotion, so with
                            // `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS=1
                            // CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1` it
                            // reported `receiver_is_java_util` — a DECLINE — for
                            // sites the chain had in fact admitted. An
                            // instrument that reports the pre-switch verdict
                            // when the switch is on cannot answer a question
                            // about the switch.
                            //
                            // It now distinguishes the four states that matter:
                            // barred outright, nominated-but-promotion-barred
                            // (which is what the switch pair BUYS), and
                            // admitted — and which HALF barred it, because the
                            // two want opposite next steps: the exception table
                            // is an unresumable-handler hazard, the prefix a
                            // stale-receiver-entry one (cb563d707).
                            // The bar is relaxable now: see
                            // `env_cache::jit_virtual_promote_handler_callee`.
                            let barred_handler =
                                !crate::runtime::env_cache::jit_virtual_promote_handler_callee()
                                    && !cached.exception_table.is_empty();
                            let barred_prefix =
                                !crate::runtime::env_cache::jit_virtual_promote_java_util()
                                    && receiver_is_java_util();
                            if crate::runtime::env_cache::jit_virtual_nominate_always() {
                                if barred_handler {
                                    "nominated_promotion_barred_exception_table"
                                } else if barred_prefix {
                                    "nominated_promotion_barred_java_util"
                                } else {
                                    "admitted"
                                }
                            } else if barred_handler {
                                "callee_exception_table"
                            } else if barred_prefix {
                                "receiver_is_java_util"
                            } else {
                                "admitted"
                            }
                        };
                        crate::runtime::interp_census::record_tierup_decline(
                            reason,
                            &cached.class_name,
                            &cached.method_name,
                            &cached.method_descriptor,
                        );
                    }
                    // Set by the last `&&` operand below, and read inside the
                    // block: which of the two PROMOTION hazards, if either,
                    // applies to this site. See
                    // `env_cache::jit_virtual_nominate_always`.
                    let mut promotion_barred = false;
                    if !is_special
                        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
                        && !cached.is_synchronized
                        // A target redefined before the fill tiers up like any
                        // other since wave 40 (lane L2): a constant `true`
                        // unless `invoke_fast::REDEFINED_TARGETS_TIER_UP`'s
                        // kill switch restores the old policy.
                        && !crate::runtime::interpreter::invoke_fast::tier_up_skips_redefined_target(
                            entry_gate.generation,
                        )
                        && !crate::runtime::env_cache::disable_jit()
                        // Ordered AFTER the cheap field reads and the JIT
                        // kill-switch on purpose: every condition in this chain
                        // is a pure predicate, so `&&` may order them freely,
                        // and this one costs a `NativeMethodRegistry` resolve.
                        // Under `--nojit` it is now never evaluated at all.
                        && !has_registered_native()
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
                        && crate::runtime::env_cache::jit_virtual_tierup()
                        // The two promotion hazards, evaluated ONCE, and the
                        // ONLY place either is evaluated.
                        //
                        // `receiver_is_java_util` used to be an operand of this
                        // chain in its own right (its comment, kept below on
                        // `promotion_barred`'s first line, is the
                        // generic-conversion regression it was added for). It
                        // is a PROMOTION hazard, so it belongs where the
                        // exception-table test now is; leaving it in the chain
                        // as well is what made the first cut of this change
                        // inert.
                        //
                        // Written as a block so the `&&` chain above still
                        // short-circuits past its class-manager `try_read`
                        // under `--nojit` and for a synchronized or
                        // native-shadowed callee, exactly as before.
                        //
                        // With `jit_virtual_nominate_always` (default-OFF; see
                        // its own doc, and the 0.994x that is why) the
                        // chain no longer STOPS here: it enters the block with
                        // `promotion_barred` set, which suppresses the
                        // `jit_cache` probe and the inline upgrade but lets the
                        // invocation counter and the tiered nomination run.
                        && {
                            // `receiver_is_java_util` is evaluated only when it
                            // can still change the answer, so the promotion arm
                            // does not pay its class-manager `try_read` either.
                            promotion_barred = (!crate::runtime::env_cache::
                                jit_virtual_promote_handler_callee()
                                && !cached.exception_table.is_empty())
                                || (!crate::runtime::env_cache::jit_virtual_promote_java_util()
                                    && receiver_is_java_util());
                            crate::runtime::env_cache::jit_virtual_nominate_always()
                                || !promotion_barred
                        }
                    {
                        // Fast path: already compiled (by this counter or OSR)?
                        //
                        // T2.2 -- epoch-guarded exactly like the invokestatic twin
                        // in `execute_invokestatic_cached`: skip the string-keyed
                        // `JitCache::get` while this entry's snapshot of
                        // `JitCache::generation()` is still current, because no
                        // publication or invalidation has happened since the probe
                        // that missed. Read the generation before probing so a
                        // racing publication can only cause a redundant re-probe,
                        // never a missed one.
                        let jit_generation = shared.jit.jit_cache.generation();
                        // The SECOND census. `tierup-decline` above stops at
                        // the `&&` chain; from here down is everything that
                        // can still refuse a site the chain ADMITTED, and
                        // until this existed nothing named any of it. See
                        // `interp_census::promote_refuse_enabled`.
                        let census = crate::runtime::interp_census::promote_refuse_enabled();
                        let mut refusal: &'static str = "entered_compiled";
                        let compiled_opt = if promotion_barred
                            || cached.jit_probe_is_current(jit_generation)
                        {
                            if census {
                                refusal = if promotion_barred {
                                    // Which HALF, because they want opposite
                                    // next steps: the exception table is an
                                    // unresumable-handler hazard, the prefix a
                                    // stale-receiver-entry one (cb563d707).
                                    if !cached.exception_table.is_empty() {
                                        "barred_callee_exception_table"
                                    } else {
                                        "barred_receiver_is_java_util"
                                    }
                                } else {
                                    // The T2.2 epoch guard: this entry already
                                    // probed the jit cache at this generation
                                    // and missed, so it does not probe again.
                                    // A site that is compiled LATER but whose
                                    // publication does not move
                                    // `jit_cache_generation` would sit here
                                    // forever, which is precisely the shape
                                    // item 2 was looking for.
                                    "jit_probe_epoch_guard"
                                };
                            }
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
                                if census {
                                    refusal = "jit_cache_miss";
                                }
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
                            let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                            let cnt = shared.jit.profile_store.increment_invocation(invoc_key);
                            let should_attempt = should_offer_tier_up(cnt, threshold);
                            // Refine the row. "the jit cache had no body" and
                            // "this method is not hot yet" are the same None
                            // and want opposite next steps, and a barred site
                            // keeps its own reason — the counter runs for it
                            // by design (that is what nomination-without-
                            // promotion IS), so reaching here does not mean
                            // the promotion was available.
                            if census && refusal == "jit_cache_miss" && cnt < threshold {
                                refusal = "counter_below_threshold";
                            }
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
                                    // Real invocation count — see the invokestatic
                                    // twin: stride-boundary `+= 1` counting deflated
                                    // the manager's hotness view 64x.
                                    super::jit_bridge::release_withdrawn_code_owners(thread);
                                    let _ = offer_invocation_to_tiered_manager(
                                        shared, &*cached, cnt as u64,
                                    );
                                } else if !promotion_barred {
                                    // A `DoorProbe::Primary` target carries a
                                    // placeholder gate; the upgrade binds the
                                    // compiled entry to the real one, so read
                                    // it from the entry (still this callee's,
                                    // or no upgrade this time).
                                    let gate = if gate_deferred {
                                        match thread.invoke_cache.get(
                                            caller_class_id,
                                            cp_index,
                                            is_special,
                                            site_pc as u32,
                                        ) {
                                            Some(CachedInvokeTarget::VirtualBytecode {
                                                cached: primary,
                                                gate,
                                                ..
                                            }) if Arc::ptr_eq(primary, &cached) => {
                                                Some(gate.clone())
                                            }
                                            _ => None,
                                        }
                                    } else {
                                        Some(entry_gate.clone())
                                    };
                                    let upgraded = match gate {
                                        Some(gate) => {
                                            try_jit_upgrade_with_gate(shared, &cached, gate)
                                        }
                                        None => None,
                                    };
                                    if let Some(CachedInvokeTarget::Jit { compiled, .. }) = upgraded
                                    {
                                        return Some(compiled);
                                    }
                                }
                            }
                            None
                        });
                        if census && compiled_opt.is_some() {
                            // The probe found a body, so whatever the earlier
                            // arm recorded is superseded; what happens next is
                            // `execute_jit_call_decoded`'s business and it
                            // records its own rows.
                            refusal = "entered_compiled";
                        }
                        if census {
                            crate::runtime::interp_census::record_promote_refuse(
                                refusal,
                                &cached.class_name,
                                &cached.method_name,
                                &cached.method_descriptor,
                            );
                        }
                        if let Some(compiled) = compiled_opt {
                            // Engagement, not a clock: which population this
                            // direct compiled call belongs to. See
                            // `site_stats::HANDLER_CALLEE_DIRECT`.
                            site_stats::bump(if cached.exception_table.is_empty() {
                                site_stats::PLAIN_CALLEE_DIRECT
                            } else {
                                site_stats::HANDLER_CALLEE_DIRECT
                            });
                            let ret = cached.return_tag();
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
                            // First use on a full heap collects and throws
                            // `OutOfMemoryError` (gen r5w1/oom5).
                            super::constants::static_sync_monitor_or_oom(
                                shared,
                                thread,
                                cached.declaring_class_id,
                                args_slice,
                            )?
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
                    install_cached_frame(
                        shared,
                        thread,
                        cached,
                        args_slice,
                        monitor_obj,
                        Some("vcached"),
                        false,
                    );
                    Ok(CachedCallResult::FramePushed)
                }
                Value::Object(None) => {
                    // Defer to the slow path, which raises the JEP 358 NPE.
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
            facts,
            sync,
            gate: _,
        } => {
            let num_params_usize = num_params as usize; // Widening: parameter count conversion
            let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params_usize);

            match receiver_val {
                Value::Object(Some(obj_ref)) => {
                    // Refresh via the same GC-forwarding barrier as invoke
                    // args (`refresh_stale_object_args`) — this receiver
                    // came from a bare `peek_at`, not a `pop`. See
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
                    if crate::jit::profile::is_receiver_profiling_enabled()
                        && !thread.frames[frame_idx].runs_obsolete_method()
                    {
                        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                        record_receiver_memoized(
                            shared,
                            cid,
                            mn,
                            md,
                            site_pc,
                            actual_class_id.as_u32(),
                            thread.frames[frame_idx].code.len(),
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
                    // and exception handling. An entry whose native answers a
                    // synchronized method (`sync`) skips this and the two
                    // shortcuts below: the funnel further down holds its
                    // monitor.
                    if sync.is_none()
                        && cratonvm_native_builtins::is_matcher_realjdk_native_callback(callback)
                    {
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
                    let cached_string_lower = sync.is_none()
                        && num_params_usize == 1
                        && cratonvm_native_builtins::lang_string::is_lower_case_native_callback(
                            callback,
                        );
                    let cached_map_get = sync.is_none()
                        && num_params_usize == 1
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

                    // THE LEAF QUESTION WAS NEVER ASKED HERE. Both `Native`
                    // arms have gone through `invoke_cached_native_callback_
                    // leaf_aware` since the leaf funnel landed; this one --
                    // the arm that serves every `invokevirtual` and
                    // `invokeinterface` on a registered native, which is the
                    // largest single population the fast doors decline --
                    // still paid the full funnel for a body that cannot block.
                    // The id is the one the cache already resolved, so the
                    // question is one bounds-checked index.
                    //
                    // And `facts` is the call site's descriptor, which is a
                    // constant of this inline-cache entry: re-resolving it per
                    // call cost a resolution-cache `RwLock` read, a hash probe,
                    // three `Arc<str>` clone/drop pairs and two scans of the
                    // string it returned, plus two heap `Vec`s.
                    if native_site_facts_usable(&facts, num_params_usize, true) {
                        site_stats::bump(site_stats::NATFACTS_VIRTUAL);
                        let mut buf = [Value::Uninitialized; MAX_CACHED_NATIVE_ARGS];
                        let n = pop_coerced_invoke_args_virtual_facts(
                            shared,
                            frame_idx,
                            thread,
                            &facts,
                            num_params_usize,
                            &mut buf,
                        )?;
                        invoke_cached_native_callback_leaf_aware(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            native_id,
                            &buf[..n],
                            RetTag::Known(facts.ret_tag),
                            sync,
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }
                    // The kill switch has to restore the OLD path whole, and
                    // the old path here was not leaf-aware -- so this arm asks
                    // the full funnel exactly as it always did. A control that
                    // keeps half the change is not a control; see this page's
                    // own note on the first `iface-select` A/B.
                    site_stats::bump(site_stats::NATFACTS_RESOLVE);
                    let (args, method_descriptor) = pop_coerced_invoke_args_virtual(
                        shared,
                        caller_class_id,
                        cp_index,
                        frame_idx,
                        thread,
                    )?;
                    match sync {
                        Some(sync) => invoke_cached_native_callback_synchronized(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            &args,
                            RetTag::Scan(&method_descriptor),
                            sync,
                        )?,
                        None => invoke_cached_native_callback(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            &args,
                            &method_descriptor,
                        )?,
                    }
                    Ok(CachedCallResult::Handled)
                }
                Value::Object(None) => {
                    // Defer to the slow path, which raises the JEP 358 NPE.
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
                        // See bug-h2-suite-residual-fail-triage-FIXED.md.
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
                        if crate::jit::profile::is_receiver_profiling_enabled()
                            && !thread.frames[frame_idx].runs_obsolete_method()
                        {
                            let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                            record_receiver_memoized(
                                shared,
                                cid,
                                mn,
                                md,
                                site_pc,
                                actual_class_id.as_u32(),
                                thread.frames[frame_idx].code.len(),
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
        CachedInvokeTarget::Bytecode {
            cached,
            gate: entry_gate,
        } => {
            // NULL-RECEIVER-CACHED-20260801: JVMS §6.5 — invokevirtual /
            // invokespecial / invokeinterface must raise NullPointerException
            // when `objectref` is null, BEFORE the callee frame exists. This
            // arm popped the receiver straight into `args_slice[0]` and pushed
            // the frame regardless, so a warmed call site silently RAN the
            // callee body with `this == null`; the sibling `VirtualBytecode`
            // arm has always deferred `Value::Object(None)` to the slow path
            // (which owns the canonical NPE), and this arm — which serves invokespecial,
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
            //
            // Anything but a non-null object is deferred, not only
            // `Object(None)`: an `Int(0)`-as-null slot peeks as `Int(0)` but
            // POPS as `Object(None)` under the `L` tag below, so it used to
            // pass this check, push the frame with a null `this` — or, for a
            // synchronized callee, return `CacheMiss` AFTER popping, which
            // made the general path pop the caller's operand stack twice.
            if !cached.is_static
                && !matches!(
                    thread.frames[frame_idx]
                        .stack
                        .peek_at(cached.num_params as usize),
                    Value::Object(Some(_))
                )
            {
                return Ok(CachedCallResult::CacheMiss);
            }
            // This arm has no receiver guard. For a virtual site (`!is_special`)
            // that is sound only because `populate_invoke_cache` admits nothing
            // but a PRIVATE instance target there (it refuses every other
            // instance method). A static entry under a virtual site's key can
            // only be an `invokestatic` fill of the same constant-pool entry —
            // never a target for this call, whose receiver it would not pop.
            if !is_special && cached.is_static {
                return Ok(CachedCallResult::CacheMiss);
            }
            // The same owner re-check as the `invokestatic` hit path, and the
            // one `populate_invoke_cache` refuses to fill against; memoized
            // per thread, so a caller whose loader cannot make it stale pays
            // no `class_manager` lock here.
            if is_special
                && super::dispatch_static::cached_static_owner_stale(
                    shared,
                    caller_class_id,
                    &cached,
                )
            {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            }
            if thread.frames.at_frame_limit(0, shared.config.max_stack_depth) {
                dump_stack_on_soe(thread);
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::StackOverflowError,
                )));
            }

            let total_args = cached.num_params as usize + 1; // Widening: parameter count conversion
            const MAX_INLINE_ARGS: usize = 16;
            // Decode args bit-exact via parameter descriptors (receiver = 'L');
            // pop_unchecked()/to_value() dropped the high bits of collision-
            // pattern long args. See bc-ec-mod-mododdinverse-investigation.md.
            // ONE forward scan; the per-argument form rescanned from `(` each time.

            let param_tags = ParamTags::for_method(&cached);

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
            // Same class-filter gate as the slow path above, and the same
            // `dbg_field_watch` guard in front of it: unarmed, the filter's
            // default substring scans ran on every cached bytecode call —
            // twice, because this block used to be repeated verbatim below.
            if let Value::Object(Some(o)) = &args_slice[0] {
                if crate::runtime::env_cache::dbg_field_watch()
                    && crate::runtime::env_cache::field_watch_class_matches(&cached.class_name)
                {
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

            // JIT tier-up, ported from `execute_invokestatic_cached`'s `Bytecode`
            // arm (T2.2/T2.5) -- until this, a `Bytecode`-cached invokespecial
            // site (every private method, every super call, every constructor
            // reached through a warm call site) never consulted `jit_cache` and
            // never counted an invocation, so it ran interpreted FOREVER no
            // matter how hot it was. `ArrayList$ArrayListSpliterator.tryAdvance`/
            // `getFence` -- both private, both invokespecial-only by construction
            // -- is the workload that found it: see
            // known-issues/spring/parallelexecutionspringextensiontests-throughput-gap-20260910.md.
            //
            // Unlike the `VirtualBytecode` arm above, this needs no
            // `receiver_is_java_util`/`promotion_barred` check at all:
            // invokespecial is statically bound to `cached`'s declaring class by
            // JVMS §6.5 -- there is no receiver-specific dispatch here for a
            // stale entry to publish against, so cb563d707's hazard (a virtual
            // route publishing a stale receiver-specific entry and spinning)
            // does not exist on this one.
            // Since wave 39 (lane L2) `false` but under the kill switch
            // (`invoke_fast::REDEFINED_TARGETS_TIER_UP`).
            let target_redefined =
                crate::runtime::interpreter::invoke_fast::tier_up_skips_redefined_target(
                    entry_gate.generation,
                );
            if !crate::runtime::env_cache::disable_jit()
                && !target_redefined
                && !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
            {
                // A body found here is also stored as this key's `Jit` entry,
                // as `execute_invokestatic_cached`'s `Bytecode` arm stores its
                // own: from the next call on, the special door declines on the
                // entry's kind in one match, and this arm enters the body
                // without a `JitCache::get` (interpreter round i1 wave 20,
                // lane L3). Not for a static target, which the `Jit` arm
                // refuses under this key.
                let upgrades = invoke_fast::NONVIRTUAL_ARM_UPGRADES_TO_JIT && !cached.is_static;
                let jit_generation = if entry_body.is_some() {
                    0
                } else {
                    shared.jit.jit_cache.generation()
                };
                let mut compiled_probe = if let Some(body) = entry_body {
                    Some(body)
                } else if cached.jit_probe_is_current(jit_generation) {
                    None
                } else {
                    // The VM's supersede epoch, read before the probe for the
                    // same reason as the generation (`JitRealm::supersede_gate`).
                    let supersede_epoch = shared.jit.supersede_gate();
                    let found = shared.jit.jit_cache.read().get(
                        &cached.class_name,
                        &cached.method_name,
                        &cached.method_descriptor,
                        cached.declaring_class_id,
                    );
                    match found {
                        None => {
                            cached.record_jit_probe_miss(jit_generation);
                            None
                        }
                        Some(found) => {
                            let compiled = cratonvm_jit::RetainedCode::new(found);
                            if upgrades {
                                let needs_heap = compiled.needs_heap();
                                thread.invoke_cache.put_as_of(
                                    caller_class_id,
                                    cp_index,
                                    is_special,
                                    site_pc as u32,
                                    entry_as_of,
                                    CachedInvokeTarget::Jit {
                                        compiled: compiled.clone(),
                                        num_params: cached.num_params,
                                        return_type: cached.return_tag(),
                                        needs_heap,
                                        cached: Arc::clone(&cached),
                                        gate: entry_gate.clone(),
                                        supersede_epoch,
                                    },
                                );
                            }
                            Some(compiled)
                        }
                    }
                };
                if compiled_probe.is_none() {
                    // Warmup counter mirroring execute_invokestatic_cached.
                    let invoc_key = cached.invoc_key();
                    let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                    let cnt = shared.jit.profile_store.increment_invocation(invoc_key);
                    let should_attempt = should_offer_tier_up(cnt, threshold);
                    if should_attempt {
                        if crate::runtime::env_cache::bg_compile() {
                            ensure_bg_compiler_started(shared);
                            let _ =
                                offer_invocation_to_tiered_manager(shared, &*cached, cnt as u64);
                        } else if let Some(jit_target) =
                            try_jit_upgrade_with_gate(shared, &cached, entry_gate.clone())
                        {
                            if let CachedInvokeTarget::Jit { compiled, .. } = &jit_target {
                                compiled_probe = Some(compiled.clone());
                            }
                            // The inline compile is not assumed free of Java
                            // (a verifier load): `put_as_of` drops the entry
                            // if a redefinition ran since the lookup.
                            if upgrades && compiled_probe.is_some() {
                                thread.invoke_cache.put_as_of(
                                    caller_class_id,
                                    cp_index,
                                    is_special,
                                    site_pc as u32,
                                    entry_as_of,
                                    jit_target,
                                );
                            }
                        }
                    }
                }
                if let Some(compiled) = compiled_probe {
                    let ret = cached.return_tag();
                    let heap = compiled.needs_heap();
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
                    // None -> deopt / too-many-args: fall through to the
                    // interpreted frame push (operand stack untouched).
                }
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
                    // First use on a full heap collects and throws
                    // `OutOfMemoryError` (gen r5w1/oom5).
                    super::constants::static_sync_monitor_or_oom(
                        shared,
                        thread,
                        cached.declaring_class_id,
                        args_slice,
                    )?
                } else {
                    // Receiver is args_slice[0] for instance calls. The
                    // arguments are already popped, so a `CacheMiss` here
                    // would let the general path pop the caller's stack a
                    // second time; the non-null check at the top of this arm
                    // makes this unreachable, and a null `this` is the NPE
                    // the monitor enter raises.
                    match args_slice.first() {
                        Some(Value::Object(Some(r))) => *r,
                        _ => {
                            return Err(RuntimeError::NullPointerException { message: None }.into())
                        }
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
            install_cached_frame(
                shared,
                thread,
                cached,
                args_slice,
                monitor_obj,
                Some("vcached2"),
                false,
            );
            Ok(CachedCallResult::FramePushed)
        }
        CachedInvokeTarget::Native {
            callback,
            native_id,
            native_kind,
            num_params,
            facts,
            is_static,
            sync,
            gate: _,
        } => {
            // A static native under this key can only be an `invokestatic`
            // fill of the same `Methodref` (JVMS §6.5 makes this call an
            // `IncompatibleClassChangeError`, which the slow path raises).
            // Declined before the receiver peek below, whose depth assumes an
            // instance target. Not evicted: the entry is right for its own
            // site, as in the `Bytecode` arm.
            if is_static {
                return Ok(CachedCallResult::CacheMiss);
            }
            // NULL-RECEIVER-CACHED-20260801: same guard as the `Bytecode` arm
            // above — this function only ever serves instance invokes
            // (invokestatic goes to `execute_invokestatic_cached`), so
            // `pop_coerced_invoke_args_virtual` always lays the receiver down
            // as `args[0]`. Handing a registered native a null `args[0]` is
            // how `Class.reflectionData()` came to return null instead of
            // throwing: its body answers a non-object receiver with
            // `Value::Object(None)`, and dozens of sibling natives do the
            // same. Defer to the slow path, which raises the NPE exactly as
            // the cold call did.
            // Anything but a non-null object defers, as in the `Bytecode` arm:
            // an `Int(0)`-as-null slot peeks as `Int(0)` and pops as null.
            if !matches!(
                thread.frames[frame_idx].stack.peek_at(num_params as usize),
                Value::Object(Some(_))
            ) {
                return Ok(CachedCallResult::CacheMiss);
            }
            let Some(callback) = revalidate_cached_native(shared, native_id, callback, native_kind)
            else {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            };
            // See the matching arm in `execute_invokestatic_cached`: the call
            // site's descriptor is a constant of this entry, so ask `facts`
            // rather than re-resolving the constant pool per call.
            let num_params = num_params as usize;
            if native_site_facts_usable(&facts, num_params, true) {
                site_stats::bump(site_stats::NATFACTS_VIRTUAL);
                let mut buf = [Value::Uninitialized; MAX_CACHED_NATIVE_ARGS];
                let n = pop_coerced_invoke_args_virtual_facts(
                    shared, frame_idx, thread, &facts, num_params, &mut buf,
                )?;
                invoke_cached_native_callback_leaf_aware(
                    shared,
                    thread,
                    frame_idx,
                    callback,
                    native_id,
                    &buf[..n],
                    RetTag::Known(facts.ret_tag),
                    sync,
                )?;
                return Ok(CachedCallResult::Handled);
            }
            site_stats::bump(site_stats::NATFACTS_RESOLVE);
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
                RetTag::Scan(&method_descriptor),
                sync,
            )?;
            Ok(CachedCallResult::Handled)
        }
        // A non-static `Jit` entry was turned into the `Bytecode` arm above;
        // what is left is an `invokestatic` fill of the same `Methodref`
        // (or every `Jit` entry under the kill switch), never this call's.
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
    // See `dispatch_static::populate_invoke_cache` for why this must be the
    // invoke's own offset.
    site_pc: usize,
) {
    // The count every fill below stores against, read before the resolution
    // below reads the caller's constant pool: a fill whose resolution ran
    // Java that looked the cache up after a redefinition is dropped
    // (`put_as_of`; interpreter round i1 wave 19, lane L4). This function
    // re-resolves from the pool as it stands now, so what the slow path that
    // called it ran before (the callee included) is not part of the window.
    let fill_as_of = thread.invoke_cache.redefinitions_seen();
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
        thread.invoke_cache.put_poly_as_of(
            caller_class_id,
            cp_index,
            false,
            site_pc as u32,
            receiver_class_id,
            fill_as_of,
            target.clone(),
        );
        thread.invoke_cache.put_as_of(
            caller_class_id,
            cp_index,
            false,
            site_pc as u32,
            fill_as_of,
            target,
        );
        return;
    }

    // Don't cache lambda proxy dispatch (they have special capture semantics)
    if shared.classes.is_lambda_proxy_class(receiver_class_id) {
        return;
    }

    // Resolve method reference from constant pool
    let (class_name, method_name, descriptor, num_params) =
        match resolve_method_ref(shared, caller_class_id, cp_index) {
            Ok(r) => r,
            Err(_) => return,
        };

    // Never cached: see `is_spring_adapt_isin`. The promoted-target fast path
    // above cannot hold this triple either, because only this function and the
    // vtable path (which refuses it too) promote virtual targets.
    if is_spring_adapt_isin(&class_name, &method_name, &descriptor) {
        return;
    }

    // `native_sync_enabled` (default on; `CRATONVM_NATIVE_SYNC=0` disarms it):
    // see the matching fact in `populate_invoke_cache`. A `VirtualNative`
    // entry for a synchronized method selected for this receiver carries the
    // monitor fact (`native_sync`) and its hit arm holds the receiver's
    // monitor around the call, as the slow path does; no `Intrinsic` entry is
    // built for such a method (it carries no monitor). A `VirtualBytecode`
    // entry takes the monitor on its own hit path. Asked after the promoted
    // hit above, which needs no answer; one bool load when off.
    let native_sync: Option<(bool, ClassId)> = if native_sync_enabled(shared) {
        let cm = shared.classes.class_manager.read();
        if super::dispatch_static::native_sync_site_may_cache_native(
            shared,
            &cm,
            receiver_class_id,
            &method_name,
            &descriptor,
        ) {
            super::invoke::resolved_method_sync_facts(
                shared,
                &cm,
                receiver_class_id,
                &method_name,
                &descriptor,
            )
            .and_then(|(is_synchronized, is_static, declaring_id)| {
                is_synchronized.then_some((is_static, declaring_id))
            })
        } else {
            None
        }
    } else {
        None
    };

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
            // An `Intrinsic` entry carries no monitor: a synchronized method
            // (`native_sync`) falls through to the entries below, which the
            // slow path's own route decides.
            if let Some(kind) = intrinsic_kind.filter(|_| native_sync.is_none()) {
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
                    .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
                thread.invoke_cache.put_poly_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    receiver_class_id,
                    fill_as_of,
                    target.clone(),
                );
                thread.invoke_cache.put_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    fill_as_of,
                    target,
                );
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
                // Round 13 wave 7 (lane shadow): the same question for the
                // enforcement dial, which `populate_invoke_cache` (the static
                // twin) and `revalidate_cached_native` already ask. Without
                // it an armed class's `Bridge` was published here, evicted by
                // the next hit, re-resolved to bytecode through step 1 and
                // published again: a slow-path call every time. `false` unless
                // `--jdk-only` with `CRATONVM_ENFORCE_NATIVE_SHADOW` covering
                // the class, so an unarmed run publishes what it did. The
                // bytecode probe takes `read_recursive()`; `cm` is held.
                .filter(|(_, _, kind)| {
                    !crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                        shared,
                        &lookup_name,
                        &method_name,
                        &descriptor,
                        *kind,
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
                facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
                sync: native_sync,
                gate,
            };
            // T10.4 — promote so sibling threads skip the class_manager walk.
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
            thread.invoke_cache.put_poly_as_of(
                caller_class_id,
                cp_index,
                false,
                site_pc as u32,
                receiver_class_id,
                fill_as_of,
                target.clone(),
            );
            thread.invoke_cache.put_as_of(
                caller_class_id,
                cp_index,
                false,
                site_pc as u32,
                fill_as_of,
                target,
            );
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
                        // The dial term `invoke_or_native`'s matching arm
                        // (`InvokeOrNativeParentShadow`) asks; see the
                        // direct lookup above. A yield ends the walk there too.
                        if let Some((callback, native_id, native_kind)) =
                            resolve_cached_native_registration(
                                shared,
                                &parent_name,
                                &method_name,
                                &descriptor,
                            )
                            .filter(|(_, _, kind)| {
                                !crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                                    shared,
                                    &parent_name,
                                    &method_name,
                                    &descriptor,
                                    *kind,
                                )
                                // A retired row between the receiver and
                                // `parent` masks its `Bridge` (round 14
                                // wave 2, lane shadow).
                                && !crate::runtime::interpreter::retired_row_masks_ancestor_bridge(
                                    shared,
                                    &cm,
                                    (receiver_class_id, None),
                                    parent_id,
                                    &method_name,
                                    &descriptor,
                                    *kind,
                                )
                            })
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
                                facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
                                sync: native_sync,
                                gate,
                            };
                            shared
                                .classes
                                .shared_resolution
                                .insert_promoted_invoke_as_of(
                                    promoted_key,
                                    target.clone(),
                                    fill_as_of,
                                );
                            thread.invoke_cache.put_poly_as_of(
                                caller_class_id,
                                cp_index,
                                false,
                                site_pc as u32,
                                receiver_class_id,
                                fill_as_of,
                                target.clone(),
                            );
                            thread.invoke_cache.put_as_of(
                                caller_class_id,
                                cp_index,
                                false,
                                site_pc as u32,
                                fill_as_of,
                                target,
                            );
                            return;
                        }
                        break;
                    }
                    // `InvokeOrNativeParent`'s dial term; a yield keeps walking,
                    // as that arm does.
                    if let Some((callback, native_id, native_kind)) =
                        resolve_cached_native_registration(
                            shared,
                            &parent_name,
                            &method_name,
                            &descriptor,
                        )
                        .filter(|(_, _, kind)| {
                            !crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                                shared,
                                &parent_name,
                                &method_name,
                                &descriptor,
                                *kind,
                            )
                            // Masked: the walk keeps going, as for an
                            // ancestor with no registration.
                            && !crate::runtime::interpreter::retired_row_masks_ancestor_bridge(
                                shared,
                                &cm,
                                (receiver_class_id, None),
                                parent_id,
                                &method_name,
                                &descriptor,
                                *kind,
                            )
                        })
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
                            facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
                            sync: native_sync,
                            gate,
                        };
                        shared
                            .classes
                            .shared_resolution
                            .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
                        thread.invoke_cache.put_poly_as_of(
                            caller_class_id,
                            cp_index,
                            false,
                            site_pc as u32,
                            receiver_class_id,
                            fill_as_of,
                            target.clone(),
                        );
                        thread.invoke_cache.put_as_of(
                            caller_class_id,
                            cp_index,
                            false,
                            site_pc as u32,
                            fill_as_of,
                            target,
                        );
                        return;
                    }
                }
                cid = parent_id;
            }
        }
    }

    // Look up method on the receiver's actual class (virtual dispatch
    // resolution). JVMS §5.4.6 selection — the same rule the slow path
    // (`try_stackless_invoke` step 4) dispatched with — so the entry cached
    // here runs the body the first call ran. A selection that ends in
    // `AbstractMethodError` / `IncompatibleClassChangeError` caches nothing:
    // the slow path raises it on every execution.
    let cm = shared.classes.class_manager.read();
    let resolved = crate::runtime::resolve::selection::resolved_ref_from_caller(
        &cm,
        caller_class_id,
        &class_name,
        &method_name,
        &descriptor,
    );
    let store = &cm.class_store;
    let Some((method, declaring_id)) = crate::runtime::resolve::selection::select_or_lenient(
        store,
        receiver_class_id,
        resolved,
        &method_name,
        &descriptor,
    ) else {
        return;
    };

    if method.is_native() {
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        if let Some((callback, native_id, native_kind)) =
            resolve_cached_native_registration(shared, declaring_name, &method_name, &descriptor)
        {
            // WP2.4-F1: gate bound to the declaring class.
            let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
            // The selected method's own flags, as the slow path's `ACC_NATIVE`
            // arm reads them (`resolved_native_sync`).
            let sync = super::dispatch_static::cached_native_sync(
                shared,
                method.is_synchronized(),
                method.is_static(),
                declaring_id,
            );
            drop(cm);
            let target = CachedInvokeTarget::VirtualNative {
                receiver_class_id,
                callback,
                native_id,
                native_kind,
                num_params: num_params as u16, // Widening: parameter count conversion
                facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
                sync,
                gate,
            };
            // T10.4 — promote so sibling threads skip this walk.
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
            thread.invoke_cache.put_poly_as_of(
                caller_class_id,
                cp_index,
                false,
                site_pc as u32,
                receiver_class_id,
                fill_as_of,
                target.clone(),
            );
            thread.invoke_cache.put_as_of(
                caller_class_id,
                cp_index,
                false,
                site_pc as u32,
                fill_as_of,
                target,
            );
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
        let dial_retires = crate::runtime::env_cache::enforce_shadow_scope().covers(declaring_name);
        let force = force_native_over_real_jdk_bytecode(declaring_name, &method_name, &descriptor)
            || (!dial_retires
                && matches!(
                    declaring_name,
                    "java/util/HashMap"
                        | "java/util/LinkedHashMap"
                        | "java/util/Hashtable"
                        | "java/util/concurrent/ConcurrentHashMap"
                )
                && matches!(
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
            if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
                shared,
                declaring_name,
                &method_name,
                &descriptor,
            ) {
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
                    facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
                    sync: native_sync,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
                thread.invoke_cache.put_poly_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    receiver_class_id,
                    fill_as_of,
                    target.clone(),
                );
                thread.invoke_cache.put_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    fill_as_of,
                    target,
                );
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
                    facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
                    sync: native_sync,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
                thread.invoke_cache.put_poly_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    receiver_class_id,
                    fill_as_of,
                    target.clone(),
                );
                thread.invoke_cache.put_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    fill_as_of,
                    target,
                );
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
                    facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
                    sync: native_sync,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
                thread.invoke_cache.put_poly_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    receiver_class_id,
                    fill_as_of,
                    target.clone(),
                );
                thread.invoke_cache.put_as_of(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    fill_as_of,
                    target,
                );
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
            facts: cratonvm_jit_api::DescriptorFacts::of(&descriptor),
            sync: native_sync,
            gate,
        };
        shared
            .classes
            .shared_resolution
            .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
        thread.invoke_cache.put_poly_as_of(
            caller_class_id,
            cp_index,
            false,
            site_pc as u32,
            receiver_class_id,
            fill_as_of,
            target.clone(),
        );
        thread.invoke_cache.put_as_of(
            caller_class_id,
            cp_index,
            false,
            site_pc as u32,
            fill_as_of,
            target,
        );
        return;
    }

    let Some(code_attr) = method.code() else {
        return;
    };

    let class = cm.get_class(declaring_id);
    let source_file = class.and_then(|c| c.source_file.as_deref()).map(Arc::from);
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");

    let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: declaring_id,
        class_name: Arc::from(declaring_class_name),
        method_name: Arc::clone(&method_name),
        method_descriptor: Arc::clone(&descriptor),
        source_file,
        // The per-method memo, not a fresh copy per call site (and per
        // thread): the quickened stream and the local-liveness table are
        // keyed on this `Arc`'s address, so every site of one method now
        // shares one of each instead of building its own.
        code: crate::runtime::frame::padded_bytecode_for_method(
            declaring_id,
            &method_name,
            &descriptor,
            &code_attr.code,
        ),
        exception_table: Arc::from(code_attr.exception_table.as_slice()),
        max_stack: code_attr.max_stack,
        max_locals: code_attr.max_locals,
        num_params: num_params as u16, // Widening: parameter count conversion
        is_synchronized: method.is_synchronized(),
        is_static: method.is_static(),
    })
    // The pool generation the code indexes, read under the guard the code was
    // copied under (interpreter round i1 wave 43, lane L2;
    // `CachedBytecodeMethod::pool_generation`).
    .with_pool_generation(cratonvm_classloading::class_redefinition_count());

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
        .insert_promoted_invoke_as_of(promoted_key, target.clone(), fill_as_of);
    thread.invoke_cache.put_poly_as_of(
        caller_class_id,
        cp_index,
        false,
        site_pc as u32,
        receiver_class_id,
        fill_as_of,
        target.clone(),
    );
    thread.invoke_cache.put_as_of(
        caller_class_id,
        cp_index,
        false,
        site_pc as u32,
        fill_as_of,
        target,
    );
}

/// The class whose `isIn` call sites are kept out of every invoke cache.
pub(super) const SPRING_MERGED_ANNOTATION_ADAPT: &str =
    "org/springframework/core/annotation/MergedAnnotation$Adapt";

/// Is this method reference Spring's loader-split `MergedAnnotation$Adapt.isIn`
/// bridge, which must always reach the slow dispatcher (`execute_invoke_kind`
/// reconciles the two loaders' enum copies there) while loader-aware
/// resolution is on?
///
/// Checked when a cache entry would be MADE — `populate_virtual_invoke_cache`
/// and the vtable path — so that no entry for it exists and no hit path needs
/// to ask. Replaces a process-global latch that made every cached invoke in
/// the process re-resolve its method reference once Spring was seen.
#[inline]
pub(super) fn is_spring_adapt_isin(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    crate::runtime::env_cache::loader_aware_resolution()
        && class_name == SPRING_MERGED_ANNOTATION_ADAPT
        && method_name == "isIn"
        && descriptor == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
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
/// `docs/internal/jdk-only/G42-1-the-intrinsic-cache-and-the-1999-20260817.md`.
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

// ── Monomorphic virtual fast door (2026-09-02) ───────────────────────────
//
// `execute_invokevirtual_cached` is the spec-complete cached dispatcher: it
// clones the cache entry (two `Arc` increments and two decrements per call),
// consults every debug trace, runs the interception chain on a `[Value; 16]`
// it fills by decoding every argument, takes a class-manager read lock to ask
// whether the receiver is a `java.util` class, and pays a sharded read lock
// plus a hash lookup for the invocation counter — on every warm hit.
//
// This door handles the one shape that is nearly every hit — a warm
// monomorphic site, a bytecode callee, nothing to intercept — with: a
// borrowed cache entry (one `Arc` increment for the callee), a header
// compare on the receiver, memoized answers to every per-callee question, a
// relaxed counter for tier-up, and a verbatim `CompactValue` transfer of the
// arguments into the callee's locals. Anything it cannot prove verbatim
// returns `None` with the operand stack untouched, and the general dispatcher
// runs exactly as before.
//
// What it declines, so the general path keeps owning it: a null or
// non-object receiver (the helpful NPE), a polymorphic receiver (the poly
// cache), lambda and annotation proxies, an interface site whose receiver
// selection is not yet memoized, a synchronized callee, a callee with a
// registered native or a non-zero intercept shape, a `java.util` receiver's
// tier-up, a callee with an exception table or more than eight parameters,
// any argument whose slot is not already in the exact representation the
// callee's locals want (unmarked category-2, `Int(0)`-as-null, ...), a full
// frame stack, a virtual thread, PGO profiling, and every invoke diagnostic.
//
// Kill switch: `CRATONVM_JIT_NO_INVOKE_FAST_DOOR=1`. Engagement:
// `CRATONVM_DBG=invokestats` counts these hits with the cache hits.

thread_local! {
    /// One-entry memo for [`record_receiver_memoized`]:
    /// `(vm_identity, class_id, method_name, descriptor, handle)` (the VM
    /// since round 13 wave 3, lane replay2).
    ///
    /// Thread-local rather than a `JvmThread` field only to keep the change
    /// small — the door always runs on the current thread, so the two are
    /// equivalent here.
    ///
    /// # Why the keys are owned `Arc`s and not raw addresses — N7
    ///
    /// They used to be the two `Arc<str>`s' ADDRESSES, on the argument that
    /// "the pointers are compared, never dereferenced, and the `Arc`s they name
    /// are kept alive by the live frame whose method they belong to". The
    /// second half is false of this entry: it is a `thread_local` replaced only
    /// on a MISS, so it outlives the frame that installed it by an unbounded
    /// amount — every subsequent call from a different caller leaves it in
    /// place until one of them misses.
    ///
    /// What actually kept those allocations alive was the loaded class. On a
    /// class unload and reload where the `ClassId` is recycled AND two fresh
    /// `Arc<str>` allocations land at the recycled addresses, all three integer
    /// compares pass and receivers are recorded into the wrong method's
    /// profile. Profile pollution, not memory unsafety — nothing is ever
    /// dereferenced through those addresses — but the wrong profile is what
    /// picks the guarded-inline receiver.
    ///
    /// Holding a clone makes the argument true instead of nearly true: the
    /// allocation cannot be recycled while the memo names it, so pointer
    /// equality means identity again. The cost is two refcount bumps per MISS
    /// — i.e. per change of calling method, not per call — and the hit path is
    /// the same three compares it always was.
    static DOOR_RECV_MEMO: std::cell::RefCell<
        Option<(
            usize,
            u32,
            std::sync::Arc<str>,
            std::sync::Arc<str>,
            cratonvm_jit::profile::ReceiverRecorder,
        )>,
    > = const { std::cell::RefCell::new(None) };
}

/// Record the door's receiver, paying the profile-store lookup once per CALLER
/// METHOD instead of once per call.
///
/// The door records against the caller's method, which cannot change while that
/// frame is live, so a single memo entry hits on essentially every call in a
/// hot loop. It is validated by POINTER equality on the two `Arc<str>` keys —
/// three integer compares — where the unmemoized path hashed both strings and
/// took two locks. Measured at +44% CPU on interpreted dispatch before this.
///
/// The keys are compared by POINTER, never dereferenced and never hashed, and
/// the memo holds an `Arc` clone of each so the allocation it names cannot be
/// freed and its address cannot be recycled — see [`DOOR_RECV_MEMO`] for the
/// class-unload case that made the older "the live frame keeps them alive"
/// argument false. A miss (different caller, or first call) does the full
/// lookup and replaces the entry, so the memo can never answer for the wrong
/// method.
///
/// `#[inline(never)]` (interpreter round i1 wave 23, lane L4): it has eight
/// callers, four of them inside the virtual fast door, which is itself
/// inlined into the dispatch loop's `0xb6` / `0xb9` arms. Inlined, each copy
/// brought a thread-local access, a `RefCell` borrow, a `parking_lot` lock
/// and unlock (with their slow-path calls) and the profile-store miss into
/// `execute_frame_from_index` — code that runs only when a JIT will read the
/// profile (see `vm_init.rs`, the `enable_receiver_profiling` call), but
/// whose size and register pressure every opcode of the dispatch function
/// shares. The call costs one `call`/`ret` against a mutex round trip and two
/// hash probes; the predicate that skips it stays inline in the callers.
#[inline(never)]
fn record_receiver_memoized(
    shared: &SharedVm,
    class_id: u32,
    method_name: &std::sync::Arc<str>,
    descriptor: &std::sync::Arc<str>,
    site_pc: usize,
    receiver_class_id: u32,
    // The caller frame's `code.len()`: a frame of another version of the
    // method (not yet moved by `obsolete_frames`) is not recorded into this
    // version's slot (round 13 wave 3, lane replay2;
    // `CRATONVM_JIT_PROFILE_FRAME_LEN_CHECK`).
    frame_code_len: usize,
) {
    if crate::runtime::env_cache::no_door_recv_memo() {
        // The unmemoized path this replaced: full lookup, every call.
        shared.jit.profile_store.record_receiver_borrowed(
            class_id,
            method_name,
            descriptor,
            site_pc,
            receiver_class_id,
            frame_code_len,
        );
        return;
    }
    DOOR_RECV_MEMO.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some((vm, c, n, d, rec)) = slot.as_ref() {
            // Four compares, the three of the address form and the VM:
            // `Arc::ptr_eq` is a pointer comparison, not a string comparison.
            // What the owned clones buy is that the pointers still MEAN
            // something -- of one class DEFINITION only while the store keeps
            // its slot: the names are interned, so a redefined class (same
            // id) or a class reloaded into a recycled id compares equal, and
            // a VM run later on this thread may reuse the ids. The VM is in
            // the key, and `record` answers `false` for a slot the store
            // reclaimed, which sends this lookup down the miss path (round 13
            // wave 3, lane replay2).
            if *vm == shared.vm_identity
                && *c == class_id
                && std::sync::Arc::ptr_eq(n, method_name)
                && std::sync::Arc::ptr_eq(d, descriptor)
                && rec.record(site_pc, receiver_class_id, frame_code_len)
            {
                return;
            }
        }
        let rec =
            shared
                .jit
                .profile_store
                .receiver_recorder_borrowed(class_id, method_name, descriptor);
        rec.record(site_pc, receiver_class_id, frame_code_len);
        *slot = Some((
            shared.vm_identity,
            class_id,
            std::sync::Arc::clone(method_name),
            std::sync::Arc::clone(descriptor),
            rec,
        ));
    });
}

/// The virtual fast door's receiver-profile record, taken where the door has
/// committed to serving the call (the general path records on entry to its own
/// `VirtualBytecode` arm, so a record before a door decline counted the call
/// twice). `CRATONVM_JIT_NO_DOOR_RECEIVER_RECORD` still turns it off.
///
/// Under `--nojit` the predicate is false (receiver profiling is armed only
/// for a consumer, interpreter round i1 wave 23) and the record is never
/// called; `#[inline(always)]` keeps that predicate a test in the door rather
/// than a call.
///
/// Takes the frame stack rather than the whole thread (wave 25) so the door
/// can record while it still BORROWS its entry from `thread.invoke_cache`.
#[inline(always)]
fn door_record_receiver(
    shared: &SharedVm,
    frames: &crate::runtime::frame::FrameStack,
    frame_idx: usize,
    site_pc: usize,
    receiver_class_id: ClassId,
) {
    if crate::jit::profile::is_receiver_profiling_enabled()
        && !crate::runtime::env_cache::no_door_receiver_record()
        && !frames[frame_idx].runs_obsolete_method()
    {
        let (cid, mn, md) = method_key_parts(&frames[frame_idx]);
        record_receiver_memoized(
            shared,
            cid,
            mn,
            md,
            site_pc,
            receiver_class_id.as_u32(),
            frames[frame_idx].code.len(),
        );
    }
}

/// What [`execute_invokevirtual_fast_door`] learnt from its own inline-cache
/// probe when it declined, for [`execute_invokevirtual_cached_probed`] to
/// start from instead of probing the same key a second time.
///
/// Set only where nothing has run since the probe that could have changed the
/// cache: a decline right at the probe (the entry's kind, or no entry), or a
/// LATE decline of an accepted `VirtualBytecode` entry before any inline
/// compile ran ([`late_decline_probe`]). The dispatch loop calls the general
/// dispatcher next, with no safepoint and no cache write in between. A decline
/// after an inline compile attempt leaves `NotProbed`.
pub(super) enum DoorProbe {
    /// The general dispatcher probes for itself (the door did not probe, or
    /// declined after an inline compile attempt).
    NotProbed,
    /// The site has no primary entry.
    Miss,
    /// The primary entry, cloned at the probe; the door does not serve its kind.
    /// Also a poly `VirtualBytecode` entry the door accepted and then declined
    /// (`get_poly` hands out an owned clone, gate included).
    Found(CachedInvokeTarget),
    /// The PRIMARY `VirtualBytecode` entry the door accepted and then declined,
    /// without its `RedefineGate`: the door reads only the gate's generation,
    /// so that its hit path takes no `Arc` bump on the declaring class's
    /// shared counter. Only for an entry whose generation is 0. The general
    /// dispatcher rebuilds the target with a placeholder gate and fetches the
    /// real one only for an inline upgrade, the one place the gate's handle
    /// (not its generation) is used.
    Primary {
        receiver_class_id: ClassId,
        cached: Arc<CachedBytecodeMethod>,
    },
}

/// What a late decline of an accepted `VirtualBytecode` entry hands the
/// general dispatcher, so the decline costs no second inline-cache probe: the
/// poly entry with its own gate when the door served one (`poly_gate`), the
/// primary entry without its gate when that gate's generation is 0, and
/// nothing after an inline compile attempt (which can safepoint).
#[inline]
fn late_decline_probe(
    receiver_class_id: ClassId,
    cached: Arc<CachedBytecodeMethod>,
    poly_gate: Option<RedefineGate>,
    gate_generation: u32,
    after_inline_attempt: bool,
) -> DoorProbe {
    if after_inline_attempt {
        return DoorProbe::NotProbed;
    }
    match poly_gate {
        Some(gate) => DoorProbe::Found(CachedInvokeTarget::VirtualBytecode {
            receiver_class_id,
            cached,
            gate,
        }),
        None if gate_generation == 0 => DoorProbe::Primary {
            receiver_class_id,
            cached,
        },
        None => DoorProbe::NotProbed,
    }
}

thread_local! {
    /// The placeholder gate a [`DoorProbe::Primary`] target carries in the
    /// general dispatcher: generation 0 (the only generation a door hands over)
    /// and a counter no redefinition ever advances. Per thread, so cloning it
    /// is an uncontended refcount bump. Never reaches a cache write: the one
    /// consumer of a gate's handle, the inline upgrade, fetches the real gate.
    static DEFERRED_DOOR_GATE: RedefineGate = RedefineGate::never_stale();
}

// ── The virtual door's out-of-line tails (interpreter round i1 wave 28) ─────
//
// Stage 1 of
// `docs/known-issues/interpreter/i27-L7-proposal-split-the-virtual-fast-door-into-a-hit-and-out-of-line-tails-20260928.md`:
// `execute_invokevirtual_fast_door` keeps its hit path (the probe, the
// checks, the frameless answers, the verbatim push) and calls these for
// everything else, so the door's own code, stack frame and saved registers
// are sized for the warm monomorphic interpreted call it serves on nearly
// every virtual call, not for its rarest paths. Each helper is the moved code,
// in the same order; behaviour is unchanged.

/// A decline right at the door's inline-cache probe, on the entry's kind
/// (`found` is `Some`) or on a miss (`None`): hand what was found to the
/// general dispatcher, which would otherwise hash the same key again, and
/// record the one census reason both shapes always shared.
#[cold]
#[inline(never)]
fn door_decline_at_probe(
    door_probe: &mut DoorProbe,
    found: Option<&CachedInvokeTarget>,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    *door_probe = match found {
        Some(other) => DoorProbe::Found(other.clone()),
        None => DoorProbe::Miss,
    };
    crate::runtime::interpreter::invoke_fast::note_virtual_decline(
        "cached target is not VirtualBytecode",
    );
    None
}

/// A late decline of an accepted `VirtualBytecode` entry the door still
/// BORROWS (from the inline cache, or the poly-served owned callee by
/// reference): hand it over ([`late_decline_probe`], before any inline compile
/// attempt) and record `why`. The `Arc` clone the hand-over needs is taken
/// here, off the door's path, as each site used to take it inline.
#[cold]
#[inline(never)]
fn door_decline_late(
    door_probe: &mut DoorProbe,
    receiver_class_id: ClassId,
    cached: &Arc<CachedBytecodeMethod>,
    poly_gate: Option<RedefineGate>,
    gate_generation: u32,
    why: &'static str,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    *door_probe = late_decline_probe(
        receiver_class_id,
        Arc::clone(cached),
        poly_gate,
        gate_generation,
        false,
    );
    crate::runtime::interpreter::invoke_fast::note_virtual_decline(why);
    None
}

/// [`door_decline_late`] for the sites after the door took its OWNED entry
/// (`callee.into_owned()`), which is moved into the hand-over;
/// `after_inline_attempt` as [`late_decline_probe`] takes it.
#[cold]
#[inline(never)]
fn door_decline_late_owned(
    door_probe: &mut DoorProbe,
    receiver_class_id: ClassId,
    cached: Arc<CachedBytecodeMethod>,
    poly_gate: Option<RedefineGate>,
    gate_generation: u32,
    after_inline_attempt: bool,
    why: &'static str,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    *door_probe = late_decline_probe(
        receiver_class_id,
        cached,
        poly_gate,
        gate_generation,
        after_inline_attempt,
    );
    crate::runtime::interpreter::invoke_fast::note_virtual_decline(why);
    None
}

/// What [`virtual_door_tier_up`] tells the door.
enum VirtualDoorTierUp {
    /// Serve the call interpreted: no compiled body and none produced inline
    /// (not due, barred, a registered native, or offered to the background).
    Interpret,
    /// Enter this compiled body ([`virtual_door_call_compiled`]).
    Compiled(cratonvm_jit::RetainedCode),
    /// Decline, handing the accepted entry over ([`door_decline_late_owned`],
    /// before any inline compile attempt).
    Decline(&'static str),
    /// Decline without a hand-over: the general dispatcher probes for itself.
    DeclineUnprobed(&'static str),
}

/// The virtual door's tier-up block, out of line (wave 28; it was ~170 lines
/// inline in the door). The door calls it only under the block's own
/// condition (the JIT on, `jit_virtual_tierup`, and -- only under
/// `invoke_fast::REDEFINED_TARGETS_TIER_UP`'s kill switch -- a gate
/// generation of 0), so a
/// `--nojit` call never enters it. `cached` is the door's OWNED entry;
/// `poly_gate` is taken only for an inline upgrade, after which no decline
/// hands the entry over; `inline_attempted` is set right before
/// `try_jit_upgrade_with_gate` runs, as the door's own flag was.
///
/// Tier-up: the same gate as the general path, with the two per-call
/// questions it used to answer with a registry probe and a class-manager
/// read lock replaced by the `NativeCallSite` memo and the `java/util/`
/// bitmap, and the counter kept on the callee.
///
/// THIS IS THE SECOND `java/util/` EXCLUSION, and until 2026-09-11 nothing
/// was looking at it.
///
/// `composition-native-callback-and-the-promotion-question-20260902.md`
/// item 2 separated nomination from promotion for the prefix and built
/// `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS` /
/// `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL` to price the two halves -- in
/// `execute_invokevirtual_cached`. It then ran both switches on and found
/// `CompletableFuture.getNow` STILL entered interpreted 40 000 times in
/// 40 000 chains, and could not say whether "the promotion does not engage,
/// or it engages and this site declines for a reason downstream of the
/// prefix".
///
/// It was neither. `getNow`'s site is served by THIS door -- the warm
/// monomorphic `VirtualBytecode` hit is nearly every hit -- and this door
/// carried its own copy of the prefix test, spelled as the
/// `java/util/` bitmap (`ClassRealm::java_util_classes`) rather than as
/// `starts_with("java/util/")`, which is why a grep over the tier-up paths
/// finds only the other one. The switch pair could not reach it, so the arm
/// that page ran was an arm on a path the workload does not take for that
/// method. The door also still gated the COUNTER on the prefix, so the
/// conflation that page's own change undid in one door survived intact in the
/// other: `getNow` was never counted, never nominated, never compiled -- and
/// no `tierup-decline` row named it, because it never reached the chain that
/// census instruments.
///
/// Both doors now read the same two switches, and the DEFAULT is
/// bit-for-bit what it was: with both off, `promotion_barred` is
/// `handler_bearing || java_util`, so the admission test reduces to the
/// `exception_table.is_empty() && !has_native && !java_util` it replaces.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn virtual_door_tier_up(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    receiver_class_id: ClassId,
    poly_gate: &mut Option<RedefineGate>,
    caller_class_id: ClassId,
    cp_index: u16,
    site_pc: usize,
    verbatim: bool,
    inline_attempted: &mut bool,
) -> VirtualDoorTierUp {
    let handler_bearing = !crate::runtime::env_cache::jit_virtual_promote_handler_callee()
        && !cached.exception_table.is_empty();
    let nominate_always = crate::runtime::env_cache::jit_virtual_nominate_always();
    // Under the defaults a handler-bearing callee is barred outright and
    // nothing below can change that, so neither the registry memo nor the
    // bitmap is consulted for one -- which keeps this door's `return None`
    // on an unanswerable bitmap exactly where it was.
    if handler_bearing && !nominate_always {
        return VirtualDoorTierUp::Interpret;
    }
    let has_native = cached
        .native_call_site()
        .resolve(
            &shared.natives.native_methods,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
        .is_some();
    let java_util = match shared.classes.java_util_classes.contains(receiver_class_id) {
        Some(b) => b,
        None => {
            return VirtualDoorTierUp::Decline(
                "the java.util bitmap has no answer for the receiver class",
            );
        }
    };
    // The two PROMOTION hazards, exactly as `execute_invokevirtual_cached`
    // names them. A handler-bearing callee entered by a direct compiled
    // call has no interpreter boundary at which its own handler can be
    // resumed; the prefix's own origin (cb563d707) is that this route
    // "can publish a stale receiver-specific entry and then spin".
    // Both are correctness hazards, which is why the switch that
    // relaxes the second ships default-OFF even though the 30 %
    // regression once cited for it does not reproduce.
    let promotion_barred = handler_bearing
        || (!crate::runtime::env_cache::jit_virtual_promote_java_util() && java_util);
    let census = crate::runtime::interp_census::promote_refuse_enabled();
    if census && (has_native || (promotion_barred && !nominate_always)) {
        crate::runtime::interp_census::record_promote_refuse(
            if has_native {
                "door_registered_native"
            } else if handler_bearing {
                "door_callee_exception_table"
            } else {
                "door_receiver_is_java_util"
            },
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        );
    }
    if has_native || !(nominate_always || !promotion_barred) {
        return VirtualDoorTierUp::Interpret;
    }
    let jit_generation = shared.jit.jit_cache.generation();
    let found = if promotion_barred || cached.jit_probe_is_current(jit_generation) {
        if census && promotion_barred {
            crate::runtime::interp_census::record_promote_refuse(
                if handler_bearing {
                    "door_nominated_promotion_barred_exception_table"
                } else {
                    "door_nominated_promotion_barred_java_util"
                },
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
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
        found.map(cratonvm_jit::RetainedCode::new)
    };
    match found {
        Some(c) => VirtualDoorTierUp::Compiled(c),
        // Decline an uncompiled callee whose arguments need coercion
        // BEFORE counting it: the general path counts the call
        // itself. Not for a synchronized callee, which the general
        // path's tier-up chain never counts, so the door's count
        // below is the only one it gets.
        None if !verbatim && !cached.is_synchronized => {
            VirtualDoorTierUp::Decline("an argument slot needs coercion")
        }
        None => {
            let threshold = crate::runtime::env_cache::jit_invocation_threshold();
            // The one door counter (saturating, credited to the
            // profile store in batches, and since wave 22 a plain
            // increment: `invoke_fast::DOOR_COUNT_IS_PLAIN`). This
            // door used to carry its own copy of it.
            let cnt = invoke_fast::count_door_invocation(shared, cached);
            let should_attempt = should_offer_tier_up(cnt, threshold);
            if census && !promotion_barred && !should_attempt {
                crate::runtime::interp_census::record_promote_refuse(
                    if cnt < threshold {
                        "door_counter_below_threshold"
                    } else {
                        "door_jit_cache_miss"
                    },
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                );
            }
            if !should_attempt {
                return VirtualDoorTierUp::Interpret;
            }
            if crate::runtime::env_cache::bg_compile() {
                ensure_bg_compiler_started(shared);
                super::jit_bridge::release_withdrawn_code_owners(thread);
                let _ = offer_invocation_to_tiered_manager(shared, &**cached, cnt as u64);
                return VirtualDoorTierUp::Interpret;
            }
            if promotion_barred {
                return VirtualDoorTierUp::Interpret;
            }
            // A poly-served callee brought its own gate;
            // the primary slot's belongs to another class.
            let gate = match poly_gate.take() {
                Some(gate) => gate,
                None => match thread.invoke_cache.get(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                ) {
                    Some(CachedInvokeTarget::VirtualBytecode { gate, .. }) => gate.clone(),
                    _ => {
                        return VirtualDoorTierUp::DeclineUnprobed(
                            "an inline tier-up attempt is due",
                        );
                    }
                },
            };
            *inline_attempted = true;
            match try_jit_upgrade_with_gate(shared, cached, gate) {
                Some(CachedInvokeTarget::Jit { compiled, .. }) => {
                    VirtualDoorTierUp::Compiled(compiled)
                }
                _ => VirtualDoorTierUp::Interpret,
            }
        }
    }
}

/// The virtual door's compiled call, out of line (wave 28): it declares the
/// 16-`Value` argument buffer, which used to sit in the door's own stack frame
/// on every call. Runs after the call was counted; it answers every outcome
/// (the compiled result, an error, or the interpreted re-run's frame push).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn virtual_door_call_compiled(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: Arc<CachedBytecodeMethod>,
    compiled: cratonvm_jit::RetainedCode,
    total_args: usize,
    site_pc: usize,
    actual_class_id: ClassId,
) -> Result<CachedCallResult, MethodCallFailed> {
    // Same engagement split as the general dispatcher's promotion arm.
    site_stats::bump(if cached.exception_table.is_empty() {
        site_stats::PLAIN_CALLEE_DIRECT
    } else {
        site_stats::HANDLER_CALLEE_DIRECT
    });
    // Compiled callee: the direct call wants `Value` arguments, so this
    // is the one shape that still decodes them.
    // `total_args` is at most `INLINE_PARAMS + 1` (the facts check in the
    // door declined anything longer), so the buffer always fits and this path,
    // which runs after the call was counted, has no size decline.
    const MAX_INLINE_ARGS: usize = 16;
    const _: () = assert!(cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS < MAX_INLINE_ARGS);
    door_record_receiver(shared, &thread.frames, frame_idx, site_pc, actual_class_id);
    dbg_invoke_stats_record(0);
    let param_tags = ParamTags::for_method(&cached);
    let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
    {
        let frame = &mut thread.frames[frame_idx];
        for i in (0..total_args).rev() {
            let tag = param_tags.get_with_receiver(&cached.method_descriptor, i);
            args_buf[i] = match frame.stack.pop_arg_for_descriptor_checked(tag) {
                Ok(v) => v,
                Err(e) => return Err(MethodCallFailed::from(e)),
            };
        }
    }
    let args_slice = &mut args_buf[..total_args];
    refresh_stale_object_args(shared, args_slice);
    let ret = cached.return_tag();
    let heap = compiled.needs_heap();
    match execute_jit_call_decoded(
        shared,
        thread,
        frame_idx,
        &compiled,
        total_args as u16, // Cast: small param count
        ret,
        heap,
        &cached,
        args_slice,
    ) {
        Ok(Some(ccr)) => return Ok(ccr),
        Ok(None) => {}
        Err(e) => return Err(e),
    }
    // DECLINED (ABI too narrow for the argument count, or a deopt with no
    // resumable frame): the callee is re-run interpreted from `args_slice`.
    // An `ACC_SYNCHRONIZED` callee must hold its monitor for that run,
    // exactly as the general path's frame push does. The compiled entry's
    // own `JitSynchronizedMonitorGuard` covered only the compiled attempt
    // and has already released it; this door admits synchronized callees
    // (see `invoke_fast::door_sync_enabled`), and the general path's
    // tier-up chain refuses them outright, so this fallback is the only
    // place the interpreted re-run can take the lock. Pushing with `None`
    // ran the body unlocked: `SyncManyArgs wide` (a synchronized 8-int
    // instance method, 4 threads x 2M calls) lost updates on w8b
    // (7_995_609 of 8_000_000); see
    // `docs/internal/jit-review-r9/NOTES-w9-review9b.md` F1.
    // The receiver is `args_slice[0]`, a validated non-null heap object
    // (refreshed above), and the args are already popped, so this cannot
    // decline any more: take the blocking acquire, which pins and remaps
    // `args_slice` across a contended wait.
    let sync_receiver = match args_slice.first() {
        Some(Value::Object(Some(obj))) if cached.is_synchronized => Some(*obj),
        _ => None,
    };
    let monitor_obj = sync_receiver.map(|obj| {
        crate::vm::monitor_enter_synchronized_method(shared, thread, obj, args_slice)
    });
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    install_cached_frame(shared, thread, cached, args_slice, monitor_obj, None, false);
    Ok(CachedCallResult::FramePushed)
}

/// See the module note above `execute_invokevirtual_fast_door`.
///
/// `#[inline(never)]` since interpreter round i1 wave 28 (lane L7; it was
/// `#[inline]`), stage 1 of
/// `i27-L7-proposal-split-the-virtual-fast-door-into-a-hit-and-out-of-line-tails-20260928.md`:
/// the dispatch loop's `invokevirtual` and `invokeinterface` arms are then the
/// same call in every build, instead of fat LTO inlining the door (twice) into
/// `execute_frame_from_index` or not depending on the size of unrelated code
/// -- the layout step the dispatch-loop page measured on the type-check rows.
/// If the host shows this costs the call rows more than the split saved,
/// revert this commit alone.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_invokevirtual_fast_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    is_interface: bool,
    fast_field: Option<&cratonvm_gc::zgc::ZgcRealHeap>,
    site_pc: usize,
    door_probe: &mut DoorProbe,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    // A redefinition retires the entries it can change inside the lookup below
    // (caller side) and through their gates (target side); see
    // `invoke_fast::DOORS_SURVIVE_REDEFINITION`.
    if crate::runtime::interpreter::invoke_fast::doors_latched_off_by_redefinition() {
        crate::runtime::interpreter::invoke_fast::note_virtual_decline("a class was redefined");
        return None;
    }
    // No `MergedAnnotation$Adapt.isIn` check here: that triple is never
    // cached (see `is_spring_adapt_isin`), so it can never reach this door.
    let caller_class_id = thread.frames[frame_idx].class_id;
    // The entry is BORROWED from the inline cache (interpreter round i1 wave
    // 25, lane L4; the wave-24 note in
    // `interpreter-L3-proposal-cheap-interpreted-call-RETIRED-20261003.md`): through every
    // decline and both frameless answers (the trivial getter, the empty body)
    // the door touches only fields of `thread` disjoint from `invoke_cache`
    // (`frames`, `iface_select_sites`, `fast_field_sites`), so a call answered
    // without a frame takes no `Arc` clone and drop -- two atomic
    // read-modify-writes on the callee's shared refcount. A decline clones for
    // the hand-over (`late_decline_probe`); the owned `Arc` is taken where the
    // door commits to the tier-up block and a push (`callee.into_owned()`).
    // A poly-served callee is owned already (`get_poly` hands out a clone).
    let (receiver_class_id, cached, gate_generation) =
        match thread
            .invoke_cache
            .get(caller_class_id, cp_index, false, site_pc as u32)
        {
            Some(CachedInvokeTarget::VirtualBytecode {
                receiver_class_id,
                cached,
                gate,
            }) => (*receiver_class_id, cached, gate.generation),
            // A non-virtual target (a private nestmate method): serve it here
            // through the non-virtual door's body instead of letting the
            // dispatch loop probe this same key again through that door. This
            // hand-off is the only place an `invokevirtual` reaches that body
            // (wave 6, lane L1: the dispatch loop's `0xb6` arm no longer runs
            // the non-virtual door after this one), so its decline is recorded
            // here. A target redefined before the fill is served
            // like any other: the general `Bytecode` arm runs it interpreted
            // too, and the door then neither probes for a compiled body nor
            // counts (it does both for any other target, as that arm does).
            // `invokevirtual` only: the `invokeinterface` arm runs no
            // non-virtual door, and its general path owns the §6.5 receiver
            // check a private interface method still needs.
            Some(CachedInvokeTarget::Bytecode { cached, gate })
                if !is_interface
                    && !crate::runtime::interpreter::invoke_fast::door_declines_redefined_target(
                        gate.generation,
                    )
                    && crate::runtime::interpreter::invoke_fast::nonvirtual_handoff_enabled() =>
            {
                let target_redefined =
                    crate::runtime::interpreter::invoke_fast::tier_up_skips_redefined_target(
                        gate.generation,
                    );
                // The entry stays BORROWED through the half of the door that
                // can answer an empty body without a frame (wave 24, see
                // `execute_nonvirtual_fast_door`); its `Arc` is cloned only
                // for a frame push.
                let mut slots = crate::runtime::interpreter::invoke_fast::empty_arg_slots();
                let prelude = crate::runtime::interpreter::invoke_fast::nonvirtual_door_prelude(
                    shared,
                    &mut thread.frames,
                    frame_idx,
                    caller_class_id,
                    false,
                    cached,
                    None,
                    &mut slots,
                )?;
                let crate::runtime::interpreter::invoke_fast::NonvirtualPrelude::Frame {
                    total_args,
                } = prelude
                else {
                    return Some(Ok(CachedCallResult::Handled));
                };
                let cached = Arc::clone(cached);
                return crate::runtime::interpreter::invoke_fast::nonvirtual_door_finish(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    &slots,
                    total_args,
                    None,
                    target_redefined,
                );
            }
            // Declined on the entry's kind, with nothing done since the probe:
            // hand what was found to the general dispatcher, which would
            // otherwise hash the same key again. Both arms keep the one census
            // reason they always shared.
            // Out of line and cold (`door_decline_at_probe`, wave 28).
            found => return door_decline_at_probe(door_probe, found),
        };
    // A synchronized callee is decided at the push, by `door_monitor_acquire`:
    // this door serves it whenever the monitor is free. See `door_sync_enabled`.
    // Every decline from here on hands the accepted entry to the general
    // dispatcher (`late_decline_probe`), so it costs no second probe.
    if cached.is_synchronized && !crate::runtime::interpreter::invoke_fast::door_sync_enabled() {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            None,
            gate_generation,
            "callee is SYNCHRONIZED",
        );
    }
    if cached.is_static {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            None,
            gate_generation,
            "callee is static",
        );
    }
    let num_params = cached.num_params as usize;
    let total_args = num_params + 1;
    let stack = &thread.frames[frame_idx].stack;
    if stack.len() < total_args {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            None,
            gate_generation,
            "operand stack shallower than the argument count",
        );
    }
    let Some(recv_ptr) = stack.peek_compact_at(num_params).as_object_ptr() else {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            None,
            gate_generation,
            "receiver slot is not an object pointer",
        );
    };
    if shared
        .mem
        .heap
        .is_object_address(recv_ptr as usize)
        .is_none()
    {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            None,
            gate_generation,
            "receiver is not a heap object address",
        );
    }
    // SAFETY: `recv_ptr` is a registered object start on this heap.
    let header = unsafe { &*(recv_ptr as *const cratonvm_gc::ObjectHeader) };
    if header.kind() == cratonvm_types::ObjectKind::Array {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            None,
            gate_generation,
            "receiver is an array",
        );
    }
    let actual_class_id = header.class_id;
    // The poly entry's gate, when the primary slot answered for another
    // receiver class; the inline tier-up below needs it instead of the
    // primary's.
    let mut poly_gate: Option<RedefineGate> = None;
    let mut receiver_class_id = receiver_class_id;
    let mut gate_generation = gate_generation;
    // The callee: the primary entry, still borrowed, or the poly entry, owned.
    // Chosen INSIDE each branch, so the primary borrow is dead on the path
    // that calls `get_poly` (a second `&mut` borrow of `invoke_cache`).
    let callee: std::borrow::Cow<'_, Arc<CachedBytecodeMethod>> = if actual_class_id
        != receiver_class_id
    {
        // A polymorphic site: serve this receiver from the per-site poly
        // entries, as `execute_invokevirtual_cached`'s receiver-first swap
        // does, when its entry is again a `VirtualBytecode`. Every poly entry
        // is filled by the same populators, together with the primary slot,
        // so it answers exactly what this door would have served had this
        // receiver called last. The callee may differ from the primary's (an
        // override), so the per-callee checks above are repeated for it; the
        // parameter count cannot differ (one descriptor per site).
        match thread.invoke_cache.get_poly(
            caller_class_id,
            cp_index,
            false,
            site_pc as u32,
            actual_class_id,
        ) {
            Some(CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: poly_receiver,
                cached: poly_cached,
                gate,
            }) if poly_receiver == actual_class_id
                && poly_cached.num_params as usize == num_params
                && !poly_cached.is_static
                && (!poly_cached.is_synchronized
                    || crate::runtime::interpreter::invoke_fast::door_sync_enabled()) =>
            {
                site_stats::bump(site_stats::DOOR_POLY_HIT);
                receiver_class_id = poly_receiver;
                gate_generation = gate.generation;
                poly_gate = Some(gate);
                std::borrow::Cow::Owned(poly_cached)
            }
            poly => {
                // A poly entry of another kind for this receiver (a native or
                // intrinsic override) is exactly what the general dispatcher's
                // receiver-first swap would select after probing the primary
                // slot and the poly entries again: hand it over. Nothing has
                // run since the probes; the receiver was checked non-null,
                // non-array, of class `actual_class_id` above.
                if let Some(poly) = poly.filter(|p| {
                    receiver_guard_of(p).is_some_and(|(guard, _)| guard == actual_class_id)
                }) {
                    *door_probe = DoorProbe::Found(poly);
                }
                crate::runtime::interpreter::invoke_fast::note_virtual_decline(
                    "receiver class differs from the cached one (site went polymorphic)",
                );
                return None;
            }
        }
    } else {
        std::borrow::Cow::Borrowed(cached)
    };
    let cached: &Arc<CachedBytecodeMethod> = &callee;
    if shared.classes.is_lambda_proxy_class(actual_class_id)
        || shared.classes.is_annotation_proxy_class(actual_class_id)
    {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            poly_gate,
            gate_generation,
            "receiver is a lambda or annotation proxy",
        );
    }
    // RECORD THE RECEIVER, exactly as `execute_invokevirtual_cached` does at
    // its own Step 5 — done by `door_record_receiver` at each point below
    // where the door COMMITS to the call (the trivial getter, the compiled
    // call, the verbatim push), not here. The general path records on entry to
    // its `VirtualBytecode` arm, so a record taken here and followed by one of
    // the door's later declines counted that call twice, biasing the profile
    // towards exactly the callees the door declines (wave 7).
    //
    // This door serves the WARM MONOMORPHIC hit -- which is nearly every hit.
    // Without this it recorded nothing, so `profile_store` saw only the calls
    // the door DECLINED: a sample biased by construction, and biased towards
    // the receivers the door is worst at. `classify_receiver_shape` and
    // `CallSiteEvidence` then read that sample, and the single-pass backend
    // pre-populates virtual-call MICs from it, so a site whose common receiver
    // never appears in the profile can be devirtualised on a rare one.
    //
    // Measured: `org.h2.test.store.TestRandomMapOps --Xmx 256m` returned the
    // wrong answer (`AssertionError: (1810, null)`, seed 0, op 1033, every run
    // in 12-23 s) with this door on. It needed BOTH ingredients -- the door,
    // and `CRATONVM_TIER_PGO_RECEIVERS` (default ON since 2026-09-02) -- and
    // switching either off made it clean, which is what named the interaction.
    //
    // The cost is the general path's cost: one `is_receiver_profiling_enabled`
    // load, and on the profiled arm a borrowed record. A door that skips the
    // bookkeeping its slow path owes is not a fast path, it is a different
    // answer.
    if is_interface && actual_class_id != cached.declaring_class_id {
        let memo_hit = !iface_select_memo_disabled()
            && thread
                .iface_select_sites
                .get(caller_class_id, cp_index)
                .is_some_and(|&(recv, decl)| {
                    recv == actual_class_id && decl == cached.declaring_class_id
                });
        if !memo_hit {
            return door_decline_late(
                door_probe,
                receiver_class_id,
                cached,
                poly_gate,
                gate_generation,
                "interface receiver-selection memo miss",
            );
        }
        site_stats::bump(site_stats::IFACE_SELECT_HIT);
    } else if is_interface {
        site_stats::bump(site_stats::IFACE_SELECT_TRIVIAL);
    }
    // Every question the interception chain asks is a constant of the callee.
    let shape = *cached.intercept_shape_cache.get_or_init(|| {
        intercept_shape_of(
            cached.class_name.as_ref(),
            cached.method_name.as_ref(),
            cached.method_descriptor.as_ref(),
        )
    });
    if shape != 0 {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            poly_gate,
            gate_generation,
            "callee is intercepted",
        );
    }
    // `force_native_cache` is filled by the general path; until it has
    // answered `false` once, or if it answered `true`, this is not our call.
    if cached.force_native_cache.get() != Some(&false) {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            poly_gate,
            gate_generation,
            "force-native cache has not answered false",
        );
    }
    if thread.frames.at_frame_limit(0, shared.config.max_stack_depth) {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            poly_gate,
            gate_generation,
            "frame stack is full",
        );
    }
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            poly_gate,
            gate_generation,
            "descriptor facts are disabled",
        );
    }
    let facts: cratonvm_jit_api::DescriptorFacts = *cached.descriptor_facts();
    if facts.param_tags_overflow
        || num_params > cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS
        || facts.param_tag_len as usize != num_params
    {
        return door_decline_late(
            door_probe,
            receiver_class_id,
            cached,
            poly_gate,
            gate_generation,
            "descriptor facts do not fit the inline parameter budget",
        );
    }
    // `CRATONVM_DBG_INVOKESTATS` counts a door hit at the three commit points
    // below (next to `door_record_receiver`), not here: a decline between here
    // and a commit is counted again by the general path's own probe.

    // Trivial instance getter (`aload_0; getfield; xreturn`): answer from the
    // quickened field site of the getter's own class without a frame, the
    // way `try_execute_cached_trivial_instance_getter` does without locks.
    //
    // NOT for a `synchronized` getter. Answering without a frame also answers
    // without the receiver's monitor, and a synchronized getter must block
    // while another thread holds it and then read what that thread wrote
    // (JLS 17.4.5). `try_execute_cached_trivial_instance_getter` refuses
    // `is_synchronized` for exactly this reason; this twin did not, and since
    // the door admits synchronized callees (`door_sync_enabled`) it answered
    // them lock-free: `SyncGetter` (thread A holds `o1`'s monitor, writes 1,
    // sleeps, writes 2; B warms `read(o2)` then calls `read(o1)`) printed
    // `seen=1` on w8b and `seen=2` with `CRATONVM_JIT_NO_INVOKE_FAST_DOOR=1`.
    // A synchronized getter falls through to `door_monitor_acquire` below.
    if num_params == 0 && !cached.is_synchronized {
        if let Some(zgc) = fast_field {
            let code = &cached.code;
            if code.len() == 7
                && code[0] == 0x2a
                && code[1] == 0xb4
                && crate::runtime::env_cache::trivial_getter_fast_path()
                && !crate::runtime::jvmti::any_method_entry_listener_active()
                && !crate::runtime::jvmti::any_method_exit_listener_active()
                // A JDWP request concerning the getter needs its frame (wave
                // 24, lane L1; see `try_execute_cached_trivial_instance_getter`).
                && !debugger_concerns_method(
                    shared,
                    cached.declaring_class_id,
                    &cached.method_name,
                    &cached.method_descriptor,
                )
            {
                let field_cp = u16::from_be_bytes([code[2], code[3]]);
                let ret_opcode = code[4];
                let declaring = cached.declaring_class_id;
                let frame = &mut thread.frames[frame_idx];
                if field_fast::getfield_fast_keyed(
                    shared,
                    zgc,
                    &mut thread.fast_field_sites,
                    &mut frame.stack,
                    declaring,
                    field_cp,
                    ret_opcode,
                ) {
                    door_record_receiver(shared, &thread.frames, frame_idx, site_pc, actual_class_id);
                    dbg_invoke_stats_record(0);
                    return Some(Ok(CachedCallResult::Handled));
                }
            }
        }
    }

    // Validate the verbatim argument transfer BEFORE the tier-up bookkeeping
    // below. It only reads the operand stack, and knowing the answer up front
    // lets a call whose slots need coercion decline before the door counts it:
    // the general path counts every call it serves, so a count taken here and
    // followed by that decline counted the call twice, and such a callee
    // reached its compile threshold at half the calls (wave 8). The slots are
    // read again before the push if an inline compile ran in between.
    //
    // Stage 2b (wave 38, lane L7): with argument overlap on, the slots are
    // validated where they are and never copied
    // (`invoke_fast::push_frame_in_place`).
    let in_place = invoke_fast::in_place_args(shared);
    let mut slots = invoke_fast::empty_arg_slots();
    let verbatim = if in_place {
        invoke_fast::validate_args_verbatim(&thread.frames[frame_idx].stack, &facts, total_args, true)
    } else {
        invoke_fast::read_args_verbatim(
            &thread.frames[frame_idx].stack,
            &facts,
            total_args,
            true,
            &mut slots,
        )
    };
    // An empty body (its first instruction is `return`: a no-op listener or
    // hook, an empty override) is answered without a frame, as the
    // non-virtual door answers `Object.<init>` since wave 20 (interpreter
    // round i1 wave 22, lane L4; `invoke_fast::VIRTUAL_DOOR_ELIDES_EMPTY_BODY`).
    // Every check above that could decline has run — receiver non-null,
    // non-array, of the cached (or poly-served) class, no proxy, the
    // interface selection memo, no interception or registered native, stack
    // depth — and the slots are verbatim, so `total_args` is exactly what
    // the call occupies. Like the non-virtual door, it is not counted for
    // tier-up (a compiled empty body could do nothing faster); the receiver
    // is still recorded, because the call is still made.
    if invoke_fast::VIRTUAL_DOOR_ELIDES_EMPTY_BODY
        && verbatim
        && invoke_fast::body_returns_at_entry(&cached)
    {
        let elide = invoke_fast::empty_body_elidable(shared, &cached);
        invoke_fast::note_empty_body_call(elide);
        if elide {
            door_record_receiver(shared, &thread.frames, frame_idx, site_pc, actual_class_id);
            dbg_invoke_stats_record(0);
            thread.frames[frame_idx].stack.discard_top(total_args);
            return Some(Ok(CachedCallResult::Handled));
        }
    }
    // Committed to the tier-up block and a push, both of which want the whole
    // thread and an owned entry: the one clone of a primary entry this door
    // still takes (moved into the frame by `push_frame_verbatim`). A
    // poly-served entry is owned already.
    let cached: Arc<CachedBytecodeMethod> = callee.into_owned();
    // Set when `try_jit_upgrade_with_gate` ran: an inline compile can reach a
    // safepoint, after which `slots` and `recv_ptr` may name moved objects.
    let mut inline_attempted = false;

    // Tier-up (`virtual_door_tier_up`, out of line since wave 28 with the
    // `java/util/` history it carries), under the block's own condition, so
    // `--nojit` tests three cached flags here and never makes the call. A
    // target redefined before the fill enters it too since wave 40 (lane L2;
    // a constant `true` unless `invoke_fast::REDEFINED_TARGETS_TIER_UP`'s kill
    // switch is off): its decline hands nothing over (`late_decline_probe`
    // gives `NotProbed` for a non-zero generation, so the general dispatcher
    // probes for itself), and its inline upgrade fetches the entry's real
    // gate from the cache.
    if !invoke_fast::tier_up_skips_redefined_target(gate_generation)
        && !crate::runtime::env_cache::disable_jit()
        && crate::runtime::env_cache::jit_virtual_tierup()
    {
        match virtual_door_tier_up(
            shared,
            thread,
            &cached,
            receiver_class_id,
            &mut poly_gate,
            caller_class_id,
            cp_index,
            site_pc,
            verbatim,
            &mut inline_attempted,
        ) {
            VirtualDoorTierUp::Interpret => {}
            // The compiled call answers every outcome, the interpreted
            // re-run's frame push included (`virtual_door_call_compiled`).
            VirtualDoorTierUp::Compiled(compiled) => {
                return Some(virtual_door_call_compiled(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    compiled,
                    total_args,
                    site_pc,
                    actual_class_id,
                ));
            }
            VirtualDoorTierUp::Decline(why) => {
                return door_decline_late_owned(
                    door_probe,
                    receiver_class_id,
                    cached,
                    poly_gate,
                    gate_generation,
                    false,
                    why,
                );
            }
            VirtualDoorTierUp::DeclineUnprobed(why) => {
                invoke_fast::note_virtual_decline(why);
                return None;
            }
        }
    }

    // Verbatim argument transfer, validated by the `read_args_verbatim` above
    // (the one copy of the slot check all three doors share; this door used to
    // carry a hand-written twin of it). A synchronized callee whose slots need
    // coercion reaches here uncounted-by-the-general-path and declines now.
    //
    // An inline compile (`try_jit_upgrade_with_gate`) can reach a safepoint,
    // and a moving collection there leaves `slots` and `recv_ptr` naming the
    // old addresses: read the slots again from the operand stack the collector
    // updated. Without an inline attempt nothing since the read can safepoint.
    if inline_attempted {
        let reread = if in_place {
            invoke_fast::validate_args_verbatim(
                &thread.frames[frame_idx].stack,
                &facts,
                total_args,
                true,
            )
        } else {
            invoke_fast::read_args_verbatim(
                &thread.frames[frame_idx].stack,
                &facts,
                total_args,
                true,
                &mut slots,
            )
        };
        if !reread {
            invoke_fast::note_virtual_decline("an argument slot needs coercion");
            return None;
        }
    } else if !verbatim {
        return door_decline_late_owned(
            door_probe,
            receiver_class_id,
            cached,
            poly_gate,
            gate_generation,
            false,
            "an argument slot needs coercion",
        );
    }
    // One push for all three doors (see `invoke_fast::push_frame_verbatim`),
    // which is also what lets a virtual call reuse the retired frame slot the
    // return left behind. This tail used to be a second copy of the same
    // sequence, and the copy is exactly why the slot-reuse change reached the
    // static doors first and left `virtual1` flat.
    // The receiver is `slots[0]`, read with the has-receiver flag from the slot
    // validated above as a live, non-array heap object of the cached class,
    // and nothing between that read and this push can safepoint. It is taken
    // from `slots`, not `recv_ptr`, which is stale after an inline compile
    // (stage 2b: from the validated receiver slot itself, current for the
    // same reason).
    let recv_slot = if in_place {
        invoke_fast::receiver_ptr_on_stack(&thread.frames[frame_idx].stack, total_args)
    } else {
        slots.receiver_ptr()
    };
    let Some(recv_ptr) = recv_slot else {
        invoke_fast::note_virtual_decline("receiver slot is not an object pointer");
        return None;
    };
    // SAFETY: `recv_ptr` is the receiver slot's current object reference.
    let recv = Some(unsafe { ObjectRef::from_raw(recv_ptr as *mut u8) });
    let monitor = match invoke_fast::door_monitor_acquire(shared, thread, &cached, recv) {
        Some(m) => m,
        None => {
            return door_decline_late_owned(
                door_probe,
                receiver_class_id,
                cached,
                poly_gate,
                gate_generation,
                inline_attempted,
                "synchronized callee is contended",
            );
        }
    };
    // Committed: no decline after this. The record touches only the Rust-side
    // profile store, so it cannot safepoint either.
    door_record_receiver(shared, &thread.frames, frame_idx, site_pc, actual_class_id);
    dbg_invoke_stats_record(0);
    if in_place {
        return Some(Ok(invoke_fast::push_frame_in_place(
            shared, thread, frame_idx, cached, total_args, true, monitor,
        )));
    }
    Some(Ok(invoke_fast::push_frame_verbatim(
        shared, thread, frame_idx, cached, &slots, total_args, monitor,
    )))
}

/// `receiver_guard_of` decides which cached targets the receiver-first poly
/// swap in `execute_invokevirtual_cached` may replace: every receiver-guarded
/// kind, with the operand-stack depth of its receiver, and nothing unguarded.
#[cfg(test)]
mod receiver_guard_tests {
    use super::*;

    fn cached(num_params: u16) -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(7),
                class_name: Arc::from("p/Target"),
                method_name: Arc::from("m"),
                method_descriptor: Arc::from("(II)V"),
                source_file: None,
                code: Arc::from(vec![0xB1u8].as_slice()),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 0,
                max_locals: 3,
                num_params,
                is_synchronized: false,
                is_static: false,
            },
        ))
    }

    #[test]
    fn receiver_guard_covers_guarded_kinds_only() {
        let virtual_bytecode = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(42),
            cached: cached(2),
            gate: RedefineGate::never_stale(),
        };
        assert_eq!(
            receiver_guard_of(&virtual_bytecode),
            Some((ClassId::new(42), 2))
        );
        // `Bytecode` serves private / special targets and carries no guard.
        let bytecode = CachedInvokeTarget::Bytecode {
            cached: cached(2),
            gate: RedefineGate::never_stale(),
        };
        assert_eq!(receiver_guard_of(&bytecode), None);
        let kind = cratonvm_native_api::InterpIntrinsic::ThreadOnSpinWait;
        let intrinsic = |receiver_class_id| CachedInvokeTarget::Intrinsic {
            kind,
            callback: cratonvm_native_builtins::intrinsics::callback_for(kind),
            num_params: 1,
            param_descs: Arc::from(Vec::<Arc<str>>::new()),
            return_type: b'V',
            receiver_class_id,
            gate: RedefineGate::never_stale(),
        };
        assert_eq!(
            receiver_guard_of(&intrinsic(Some(ClassId::new(9)))),
            Some((ClassId::new(9), 1))
        );
        // A static intrinsic has no receiver to compare.
        assert_eq!(receiver_guard_of(&intrinsic(None)), None);
    }

    /// What a late decline of the virtual door hands the general dispatcher
    /// (wave 8): a served poly entry with its own gate, the primary entry
    /// without its gate only at generation 0, and nothing once an inline
    /// compile attempt may have safepointed.
    #[test]
    fn a_late_door_decline_hands_over_the_accepted_entry() {
        let receiver = ClassId::new(42);
        assert!(matches!(
            late_decline_probe(receiver, cached(2), None, 0, false),
            DoorProbe::Primary { receiver_class_id, .. } if receiver_class_id == receiver
        ));
        assert!(matches!(
            late_decline_probe(receiver, cached(2), None, 1, false),
            DoorProbe::NotProbed
        ));
        assert!(matches!(
            late_decline_probe(
                receiver,
                cached(2),
                Some(RedefineGate::never_stale()),
                0,
                false
            ),
            DoorProbe::Found(CachedInvokeTarget::VirtualBytecode { receiver_class_id, .. })
                if receiver_class_id == receiver
        ));
        assert!(matches!(
            late_decline_probe(
                receiver,
                cached(2),
                Some(RedefineGate::never_stale()),
                0,
                true
            ),
            DoorProbe::NotProbed
        ));
        // The placeholder a `Primary` target carries is never stale and has
        // the only generation a door hands over.
        let placeholder = DEFERRED_DOOR_GATE.with(RedefineGate::clone);
        assert_eq!(placeholder.generation, 0);
        assert!(!placeholder.is_stale());
    }

    /// Why the cached `Bytecode` / `Native` arms of
    /// `execute_invokevirtual_cached` defer every receiver slot that does not
    /// PEEK as a non-null object: an `Int(0)`-as-null slot peeks as `Int(0)`,
    /// which the old `Object(None)` test let through, and then pops as a null
    /// reference under the `L` tag the arms pop the receiver with.
    #[test]
    fn an_int_zero_receiver_slot_peeks_as_int_but_pops_as_null() {
        let mut stack = crate::runtime::ValueStack::new(4);
        stack.push_compact(CompactValue::int(0));
        assert!(!matches!(stack.peek_at(0), Value::Object(None)));
        assert!(!matches!(stack.peek_at(0), Value::Object(Some(_))));
        assert!(matches!(
            stack.pop_arg_for_descriptor_checked(b'L'),
            Ok(Value::Object(None))
        ));
    }
}
