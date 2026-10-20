// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cohesive pieces of [`super::execute`], moved out of its ~4 000-line body.
//!
//! Stage 1 of `docs/internal/fixed-bugs/interpreter-L3-proposal-split-execute-RETIRED-20261003.md`:
//! PURE MOVES. Each function here is a block of `execute` transplanted
//! verbatim — same statements, same order, same comments — with only what a
//! move forces changed: the block's free variables became parameters, its
//! `return`s return from the function (the caller returns what it returns),
//! and a path that was relative to `interpreter.rs` is spelled absolutely.
//! Review a change here as a move first.
//!
//! One deliberate change since the move (interpreter round i1 wave 6): the
//! no-`Code` arm takes the resolved method's `ACC_NATIVE` / `ACC_STATIC`
//! flags ([`NoCodeFlags`]), skips the receiver rescues for a static method,
//! and raises `UnsatisfiedLinkError` for an unbound native.

use super::*;

/// `execute`'s entry diagnostics, run once the arguments are pinned and before
/// the method lookup: the `CRATONVM_DBG_LETSGO` dispatch ring, the
/// `intValue` trace, and the `CRATONVM_IAE_TRACE` constructor trace. Each is
/// one cached flag test (or a name compare first) when unarmed; `#[inline(always)]`
/// keeps those tests where they were.
///
/// `args` is `execute`'s caller-owned slice, as the moved block read it.
#[inline(always)]
pub(super) fn execute_entry_trace_hooks(
    shared: &SharedVm,
    thread: &JvmThread,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) {
    // letsgo postmortem instrumentation: record every bytecode-method
    // entry into the global dispatch ring. Gated by `CRATONVM_DBG_LETSGO=1`
    // (cheap atomic-bool check on the disabled path).
    if crate::dispatch_trace::is_enabled() {
        let class_name_owned = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("cid#{class_id:?}"));
        crate::dispatch_trace::record_bytecode(
            // Widening: small integer index -> usize (non-negative, fits in pointer width)
            thread.thread_id.0 as usize,
            &class_name_owned,
            method_name,
            method_descriptor,
        );
    }
    if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
        eprintln!(
            "[interpreter::execute] class_id={:?} method={} desc={} args.len={}",
            class_id,
            method_name,
            method_descriptor,
            args.len()
        );
    }
    // CRATONVM_IAE_TRACE: log args when executing AnnotationScopeMetadataResolver.<init>
    //
    // Perf: the eprintln! only ever fires for `<init>` methods, so gate the
    // env-flag check + `class_manager.read()` RwLock acquire + `to_string()`
    // allocation behind the cheap `method_name == "<init>"` predicate first.
    // For every non-`<init>` call (the overwhelming majority) this path now
    // does zero work even when CRATONVM_IAE_TRACE is set. Behaviour is
    // identical — `method_name == "<init>"` was already a required conjunct.
    if method_name == "<init>" && crate::runtime::env_cache::iae_trace_os() {
        let class_name_for_trace = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if class_name_for_trace.contains("AnnotationScopeMetadataResolver") {
            eprintln!(
                "[execute] {}.{}{} args={:?}",
                class_name_for_trace, method_name, method_descriptor, args
            );
        }
    }
}

/// The resolved method's own flags, which the no-`Code` arm needs and cannot
/// re-derive without a second method lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NoCodeFlags {
    /// `ACC_NATIVE`: an unbound native, not an abstract declaration. When
    /// every rescue declines, JVMS §5.6 / HotSpot raise
    /// `UnsatisfiedLinkError: 'void p.Foo.bar()'`, not `AbstractMethodError`.
    pub(super) is_native: bool,
    /// `ACC_STATIC`: `args[0]` is the first PARAMETER, not a receiver, so the
    /// receiver-based rescues must not treat it as one.
    pub(super) is_static: bool,
    /// `ACC_SYNCHRONIZED` (only an `ACC_NATIVE` method can carry it here): the
    /// resolved-class native runs under the method's monitor while
    /// `invoke::native_sync_enabled` holds, as every other native door does.
    /// `execute` takes the monitor only for a method with `Code`, after this
    /// arm has returned.
    pub(super) is_synchronized: bool,
}

/// The error the no-`Code` arm raises when every rescue declined:
/// `(exception class, message)`.
fn no_code_error(
    flags: NoCodeFlags,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> (&'static str, String) {
    if flags.is_native {
        (
            "java/lang/UnsatisfiedLinkError",
            format!(
                "'{}'",
                crate::runtime::resolve::selection::external_method_name(
                    class_name,
                    method_name,
                    method_descriptor,
                )
            ),
        )
    } else {
        (
            "java/lang/AbstractMethodError",
            format!("method {class_name}.{method_name}{method_descriptor} has no Code attribute"),
        )
    }
}

/// `execute`'s no-`Code` arm: the resolved method has no `Code` attribute
/// (abstract, interface, or an unbound native). Tries, in order, a native
/// registered on the resolved class, the `forEachOrdered` rewrite, the
/// lambda-proxy and annotation-proxy receivers, a native on the receiver's
/// hierarchy, "Path A" (the receiver's concrete override) and "Path B" (the
/// canonical collection stand-in), and otherwise raises
/// `AbstractMethodError` — or, for an `ACC_NATIVE` method nothing implements,
/// the `UnsatisfiedLinkError` HotSpot raises. It always answers the call.
///
/// `args` is `execute`'s caller-owned slice; `args_buf` / `args_root_guard`
/// are its pinned copy, read back through `live_args!()` after any possible
/// safepoint, exactly as in `execute`.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_no_code_rescue(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
    class_name_owned: String,
    flags: NoCodeFlags,
    args_buf: &mut smallvec::SmallVec<[Value; 8]>,
    args_root_guard: &super::field_access::InvokeArgsRootGuard,
) -> MethodCallResult {
    macro_rules! live_args {
        () => {{
            args_root_guard.refresh(&mut args_buf[..]);
            &args_buf[..]
        }};
    }

    // A native registered directly on the resolved class IS the
    // intended implementation of an otherwise-abstract method — e.g.
    // the synthetic `java/nio/channels/FileChannel`, whose
    // `read`/`write`/`size`/`position`/... natives back a real fd.
    // The receiver-walk rescue below only dispatches when the
    // target class differs from `class_id` (to avoid re-resolving
    // back through the same abstract declaration), so it misses the
    // case where the receiver's runtime class IS that abstract
    // class. A native is a concrete Rust fn — there is no
    // re-resolution loop — so dispatch straight to it.
    // Stream.forEachOrdered(Consumer) gap: its only native registration
    // lives in `register_phase56_stream_extras`, reachable solely from
    // `register_synthetic_overrides` (synthetic-jdk feature, compiled out
    // of the real-JDK CLI). So an `invokeinterface Stream.forEachOrdered`
    // resolves to the abstract interface declaration (no Code) and the
    // interface->concrete retarget that already makes `forEach` work does
    // not fire for `forEachOrdered`, surfacing as
    //   AbstractMethodError: Stream.forEachOrdered(...)V has no Code attribute
    // (24+ WildFly `ejb.security` tests, plus any real-JDK code using it).
    // For our sequential streams `forEachOrdered` is semantically identical
    // to `forEach`; re-dispatch as `forEach`, whose receiver-walk rescue
    // (Path A below) resolves the concrete override on the receiver.
    if method_name == "forEachOrdered" && method_descriptor == "(Ljava/util/function/Consumer;)V" {
        return execute(shared, thread, class_id, "forEach", method_descriptor, args);
    }
    // JVMTI redefine guard: when this class has been redefined in
    // place by an agent, its woven bytecode is authoritative — skip the
    // per-class native shadow so the interpreter runs the (instrumented)
    // body and the advice fires. Fast-pathed on `any_class_redefined`.
    //
    // JDK-ONLY-WAVE2: the `redefine_immune_*` predicates are
    // hard-coded class/method-name exception lists (defined in
    // `native_override.rs`, not owned here). They encode "this native
    // keeps winning even over instrumented bytecode", which is a §1.4
    // shadow decision taken outside `resolve_dispatch`. What should
    // replace them: `NativeKind` — exactly `Intrinsic` should be
    // redefine-immune, and everything else should yield to redefined
    // bytecode, with no name list at all. NOT deleted this wave; the
    // lists gate real Mockito/ByteBuddy behaviour.
    //
    // This site used to open-code `reflection && string_builder &&
    // path`, which is the aggregate MINUS five arms: `jfr`,
    // `bc_crypto_math`, `stamped_lock`, the `FileHandler` ctor/publish
    // group, and `synthetic_collection`. The last one is the one that
    // bites: a redefined `java/util/HashMap` reaching here lost its
    // registered native and ran real JDK bytecode against a CratonVM
    // synthetic object, which `redefine_immune_synthetic_collection_native`
    // says can never work.
    //
    // That is the SAME defect `layout_immunity_is_not_open_coded`
    // exists to prevent, and it was invisible to it: that gate
    // `include_str!`s `native_override.rs` and polices only its own
    // file, while this hand-rolled chain lives here. The gate now scans
    // its siblings too — a fifth staleness mode, after the three its
    // own comment lists and the CRLF one below them.
    let class_redefined = crate::classloading::any_class_redefined()
        && shared
            .classes
            .class_manager
            .read()
            .class_redefine_generation(class_id)
            > 0
        && !redefine_immune_forced_native(&class_name_owned, method_name, method_descriptor);
    if method_name != "<init>" && method_name != "<clinit>" && !class_redefined {
        // JDK-only §7, resolved-class native. `bytecode_available =
        // false`: this whole arm only runs when the resolved method has
        // NO `Code` attribute, so the registered native is the only
        // implementation the method has (§7 step 3b) — there is no
        // bytecode for it to shadow, and §1.4 is not in play.
        match resolve_native_for_dispatch(
            shared,
            &class_name_owned,
            method_name,
            method_descriptor,
            false,
        ) {
            Ok(Some(cb)) => {
                // JVMS §2.11.10: a `synchronized native` runs holding its
                // monitor (the receiver, or this class's mirror). `class_id`
                // declares the method (`execute` found it on that class).
                let sync = (flags.is_synchronized && super::invoke::native_sync_enabled(shared))
                    .then_some((flags.is_static, class_id));
                return super::invoke::safe_native_call_synchronized(
                    shared, thread, cb, args, sync,
                );
            }
            Ok(None) => {}
            Err(violation) => {
                return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
            }
        }
    }
    // S111r10 — interface-dispatch receiver-walk fallback. The
    // canonical Spring Boot fat-jar tripwire is
    // `HashSet.iterator()` line 183 = `map.keySet().iterator()`:
    // the inner `iterator()` is `invokeinterface Set.iterator`, but
    // dispatch resolves to the abstract `Set.iterator` declaration
    // (no Code) instead of the receiver's concrete override.
    // Before throwing AbstractMethodError, walk the receiver's
    // runtime-class chain for a same-name+descriptor method that
    // does have Code (or a registered native), and dispatch
    // through there. This generalises the S111r7/r8 collection-view
    // rescue to any interface-method call where the cp class
    // resolved to an abstract declaration but the receiver carries
    // a concrete override on its real runtime class.
    //
    // Guards:
    //  * Only attempts the rescue for non-`<init>` instance methods
    //    (`<init>` and `<clinit>` aren't virtually dispatched).
    //  * Only fires when the receiver's runtime class differs from
    //    `class_id` AND is a non-interface concrete class — keeps
    //    the rescue from looping back through the same abstract
    //    declaration.
    //  * Bytecode dispatch is delegated through
    //    `invoke_on_class_shared_no_retarget` on the receiver's
    //    class so `find_method_recursive` walks superclasses
    //    starting from the receiver, NOT from the interface
    //    declaration we just came from.
    //  * Never for a static method (an unbound static native): its
    //    `args[0]` is the first parameter, and dispatching on that
    //    object's class ran a same-named method of an unrelated class
    //    with the parameter as `this`.
    if method_name != "<init>" && method_name != "<clinit>" && !flags.is_static {
        if let Some(Value::Object(Some(recv_obj))) = args.first().copied() {
            let recv_cid = shared.mem.heap.class_id_of(recv_obj);
            let recv_kind = shared.mem.heap.kind_of(recv_obj);
            // Round 7 — receiver-is-lambda-proxy rescue. When an
            // invokeinterface lands on an interface declaration with no
            // Code (e.g. `CacheOverride.close()V`) but the receiver is
            // a lambda proxy implementing that interface (e.g.
            // `SoftReferenceConfigurationPropertyCache#NOOP`,
            // declared as `CacheOverride o = () -> {}`), route through
            // try_lambda_dispatch so the proxy's SAM impl_handle runs
            // instead of throwing AbstractMethodError.
            let recv_is_lambda = shared.classes.lambda_proxies.read().contains_key(&recv_cid);
            if recv_is_lambda {
                let rest = if args.is_empty() { &[][..] } else { &args[1..] };
                if let Some(inner) = try_lambda_dispatch(
                    shared,
                    thread,
                    recv_obj,
                    recv_cid,
                    method_name,
                    method_descriptor,
                    rest,
                )? {
                    return Ok(inner);
                }
                if let Some(inner) = try_lambda_default_method_dispatch(
                    shared,
                    thread,
                    recv_cid,
                    method_name,
                    method_descriptor,
                    live_args!(),
                )? {
                    return Ok(inner);
                }
            }
            // A declined lambda rescue may still have reached a
            // safepoint: read the (possibly moved) arguments and
            // receiver back from their pins for every use below.
            let args = live_args!();
            let recv_obj = match args.first() {
                Some(Value::Object(Some(obj))) => *obj,
                _ => recv_obj,
            };
            // Receiver-is-annotation-proxy rescue. An annotation
            // member call (e.g. JUnit5's `ExtendWith.value()`)
            // resolves to the abstract interface declaration (no
            // Code), but the receiver is one of our synthetic
            // `java/lang/annotation/AnnotationProxy` objects whose
            // members live in its name/value element arrays — there
            // is no bytecode body to find anywhere. Route through
            // the annotation-proxy element dispatch (same handler
            // the direct invoke path at `execute_invoke` uses)
            // instead of throwing AbstractMethodError.
            if recv_kind == cratonvm_types::ObjectKind::Object
                && shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(recv_cid)
                    .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
                    .unwrap_or(false)
            {
                let rest = if args.is_empty() { &[][..] } else { &args[1..] };
                let result = crate::vm::annotation_proxy_invoke_shared(
                    shared,
                    thread,
                    recv_obj,
                    method_name,
                    rest,
                )?;
                // Unbox primitive-returning members. The proxy stores
                // its element values as boxed wrappers, but an
                // `invokeinterface <Ann>.member()` whose declared return
                // is a primitive needs the UNBOXED value — otherwise the
                // wrapper reference is reinterpreted as the primitive
                // (e.g. ByteBuddy reads `@Advice.OnMethodEnter.skipOnIndex()`
                // and sees the Integer object pointer — a positive int —
                // instead of -1, so `RelocationHandler.ForType.of` throws
                // "void is not an array type but an index for a
                // relocation is defined" and Hibernate's BytecodeProvider
                // service-load fails). Mirror the direct AnnotationProxy
                // dispatch path in `execute_invoke`.
                if let Some(value) = result {
                    let ret_char = method_descriptor
                        .rsplit(')')
                        .next()
                        .unwrap_or("L")
                        .chars()
                        .next()
                        .unwrap_or('L');
                    let unboxed = match ret_char {
                        'I' | 'Z' | 'B' | 'C' | 'S' | 'J' | 'F' | 'D' => {
                            if let Value::Object(Some(obj)) = value {
                                shared.mem.heap.get_field(obj, 0)
                            } else {
                                value
                            }
                        }
                        _ => value,
                    };
                    return Ok(Some(unboxed));
                }
                return Ok(None);
            }
            // Receiver-own-class native rescue (general). When the
            // resolved method has no Code, but a native is registered
            // on the receiver's OWN runtime class (or a superclass) for
            // the same name+descriptor, dispatch to it — provided the
            // receiver has no real bytecode override (those are handled
            // by Path A below). This covers synthetic objects stamped
            // with an interface/abstract runtime class that carry their
            // method natives on that exact name, regardless of whether
            // the class store flags it `interface` — e.g. the
            // scheduled-executor shim's `ScheduledFuture` object, whose
            // `cancel(Z)Z` native is what JUnit's `@Timeout` finally
            // block needs (the cp dispatch resolved to the abstract
            // `Future.cancel`, so the resolved-class native check above
            // missed it). Without this, every `@Timeout` test throws
            // `AbstractMethodError: Future.cancel(Z)Z has no Code`.
            if recv_cid != ClassId::new(0) {
                let (recv_native_cb, has_bytecode, jdk_only_violation) = {
                    let cm2 = shared.classes.class_manager.read();
                    let bytecode = crate::classloading::find_method_recursive(
                        recv_cid,
                        method_name,
                        method_descriptor,
                        &cm2.class_store,
                    )
                    .map(|(m, _)| m.code().is_some())
                    .unwrap_or(false);
                    let mut cb = None;
                    let mut violation = None;
                    // JDK-only §7, receiver-hierarchy native rescue.
                    //
                    // The walk used to run unconditionally and its result
                    // was then discarded whenever `has_bytecode` held.
                    // Hoisting that test keeps the §4 census honest — a
                    // resolution that can never dispatch must not be
                    // counted as an invocation — and is otherwise
                    // behaviour-identical: `recv_native_cb` has no other
                    // reader, so the walk was pure work in that case.
                    if !bytecode {
                        let mut walk = Some(recv_cid);
                        while let Some(cid) = walk {
                            if let Some(cls) = cm2.class_store.get(cid) {
                                match resolve_native_for_dispatch(
                                    shared,
                                    &cls.name,
                                    method_name,
                                    method_descriptor,
                                    // §7 step 3b: nothing in the
                                    // receiver's chain has `Code` for
                                    // this signature (that is what
                                    // `!bytecode` means), so a
                                    // registered native is the only
                                    // implementation and shadows
                                    // nothing.
                                    false,
                                ) {
                                    Ok(Some(found)) => {
                                        cb = Some(found);
                                        break;
                                    }
                                    // Under `JdkOnly` a `SyntheticStub`
                                    // here is a refusal, NOT a reason to
                                    // keep walking: continuing to the
                                    // superclass would be precisely the
                                    // silent fallback §1.3 forbids.
                                    Err(v) => {
                                        violation = Some(v);
                                        break;
                                    }
                                    Ok(None) => {}
                                }
                                walk = cls.superclass;
                            } else {
                                break;
                            }
                        }
                    }
                    (cb, bytecode, violation)
                };
                if let Some(violation) = jdk_only_violation {
                    return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                }
                if !has_bytecode {
                    if let Some(cb) = recv_native_cb {
                        let r = crate::vm::safe_native_call(shared, thread, cb, args)?;
                        return Ok(r);
                    }
                }
            }
            // Path A — receiver carries a real (non-zero) class_id.
            //   Walk its runtime-class chain for a same-signature
            //   override that has Code (or a registered native) and
            //   dispatch through there. This catches the canonical
            //   `HashSet.iterator()` → `map.keySet().iterator()`
            //   chain when `keySet()` returned a concrete subclass
            //   (e.g. HashMap$KeySet) but the cp dispatch resolved
            //   to the abstract Set.iterator declaration.
            if recv_cid != ClassId::new(0) {
                let (recv_concrete, has_better, better_decl) = {
                    let cm2 = shared.classes.class_manager.read();
                    let concrete = cm2
                        .get_class(recv_cid)
                        .map(|c| !c.is_interface())
                        .unwrap_or(false);
                    let (better, decl) = if concrete {
                        match crate::classloading::find_method_recursive(
                            recv_cid,
                            method_name,
                            method_descriptor,
                            &cm2.class_store,
                        ) {
                            Some((m, d)) => (m.code().is_some(), Some(d)),
                            None => (false, None),
                        }
                    } else {
                        (false, None)
                    };
                    (concrete, better, decl)
                };
                // JDK-only §7: this is a *routing* probe, not a dispatch.
                // It only decides whether Path A retargets to
                // `invoke_on_class_shared_no_retarget`, which is itself
                // routed through `resolve_dispatch` — so the policy
                // decision is taken there, with the resolved `&Class` /
                // `&Method` in hand, and no invocation is counted here
                // (nothing is invoked here).
                //
                // The kind is deliberately NOT filtered: refusing to
                // retarget on a `SyntheticStub` under `JdkOnly` would
                // convert what should be a structured
                // `SyntheticNativeInvocation` refusal into a silent
                // `AbstractMethodError` from the fall-through below.
                // `find_with_kind` is used anyway so the probe is on a
                // kind-aware API (identical cost — same `slot_for_exact`
                // fast path, same descriptor-quirk fallback).
                let recv_native = if recv_concrete {
                    let cm2 = shared.classes.class_manager.read();
                    let mut walk = Some(recv_cid);
                    let mut found = false;
                    while let Some(cid) = walk {
                        if let Some(cls) = cm2.class_store.get(cid) {
                            if shared
                                .natives
                                .native_methods
                                .find_with_kind(&cls.name, method_name, method_descriptor)
                                .is_some()
                            {
                                found = true;
                                break;
                            }
                            walk = cls.superclass;
                        } else {
                            break;
                        }
                    }
                    found
                } else {
                    false
                };
                let target_cid = if has_better {
                    better_decl.unwrap_or(recv_cid)
                } else {
                    recv_cid
                };
                if recv_concrete && (has_better || recv_native) && target_cid != class_id {
                    return crate::vm::invoke_on_class_shared_no_retarget(
                        shared,
                        thread,
                        target_cid,
                        method_name,
                        method_descriptor,
                        args,
                    );
                }
            }
            // Path B — receiver is a synthetic alloc with cid=0
            //   (no class_id ever stamped onto its header) and the
            //   cp class is a well-known collection interface. The
            //   receiver shape matches no concrete class in our
            //   class store, but the registered native for the
            //   canonical concrete subclass (e.g. HashSet for Set,
            //   HashMap$KeyItr for Iterator) implements the
            //   external contract correctly. Look up that native
            //   and dispatch through it. This generalises the
            //   S111r7/r8 collection-view rescue to the case where
            //   the cp dispatch class is the *interface* itself
            //   (Set/Iterator/Collection/Map/List).
            let recv_is_iface = {
                let cm2 = shared.classes.class_manager.read();
                cm2.get_class(recv_cid)
                    .map(|c| c.is_interface())
                    .unwrap_or(false)
            };
            if recv_cid == ClassId::new(0)
                || recv_kind == cratonvm_types::ObjectKind::Array
                || recv_is_iface
            {
                // Map well-known interfaces -> canonical concrete
                // class whose natives we register.
                //
                // JDK-ONLY-WAVE2: hard-coded class-name exception list.
                // This substitutes a *different class's* native for an
                // unresolvable interface call — a compatibility
                // substitution in the §1 sense, and one that is silent:
                // the receiver is not an instance of `canonical`. What
                // should replace it: a real interface-method resolution
                // (JVMS §5.4.3.4 `selectMethod` over the receiver's
                // runtime class), with the `ClassOrigin` of the receiver
                // deciding whether a shim receiver is even legal. Under
                // `JdkOnly` real class bytes make every one of these
                // interfaces resolvable, so the map should become
                // unreachable rather than conditional. NOT deleted this
                // wave — removing shim mappings has regressed real-JDK
                // boot before.
                let canonical: &'static str = canonical_concrete_for_interface(&class_name_owned);
                // G13-1, 2026-08-17. An ARRAY receiver cannot be an
                // instance of ANY interface this map covers: JLS 4.10.3
                // gives an array type exactly two superinterfaces,
                // `java.lang.Cloneable` and `java.io.Serializable`, and
                // neither is in the list. So for an array the
                // substitution is not "a shim that is probably right" —
                // it is provably wrong, and it is the shape that hides
                // the wrongness best, because every one of the canonical
                // natives reads its receiver through an `elementData`/
                // bucket layout an array does not have and reports
                // **empty** rather than refusing.
                //
                // MEASURED, 2026-08-17, on the `d87dff06a`+2 binary:
                // `LinkedHashMap.values()` mints a real
                // `LinkedHashMap$LinkedValues` carrier, whose
                // `elementData` slot (absolute 1, from the real
                // `java/util/ArrayList` layout) collides with the ONE
                // member of that carrier family that declares two
                // fields — `LinkedValues` has `reversed` at 0 and
                // `this$0` at 1. The view's source-map read then hands
                // native-collections the `Object[]` element buffer as
                // though it were the backing `Map`, and it arrives here
                // as `ctx.invoke("java/util/Map", "isEmpty", "()Z",
                // [Object[11]])`. Proven by reflection on both VMs
                // (`--add-opens java.base/java.util=ALL-UNNAMED`):
                // HotSpot reports `this$0 -> java.util.LinkedHashMap`,
                // CratonVM `this$0 -> [Ljava.lang.Object;[len=11]`.
                // Under `Compatible` that substitution answered
                // `isEmpty() == true` for a three-entry map and the
                // whole `values()` view silently came back EMPTY; under
                // `--jdk-only` the §8 refusal below turned it into an
                // `AbstractMethodError` naming `java/util/Map` — which
                // reads as an interface-door defect and is not one.
                //
                // Refusing here is what makes the two modes agree and
                // what puts the receiver's real shape in the message.
                // The blast radius is MEASURED, not argued: across all
                // 105 corpus main classes, in `--jdk-only` and in
                // `Compatible`, `[CANONICAL_CENSUS]` reports this map
                // firing exactly ONCE — `java/util/Map -> java/util/
                // HashMap isEmpty 1`, in `RJdkMapViews`, the vector this
                // record is about. No currently-green vector reaches it.
                //
                // The root cause is the slot collision, and it lives in
                // `native-collections/src/lib.rs` (nominated in
                // `G13-1-…-20260817.md`). This site cannot fix it; it
                // can stop laundering it into a wrong answer.
                if !canonical.is_empty() && recv_kind == cratonvm_types::ObjectKind::Array {
                    let msg = format!(
                        "array receiver does not implement the requested interface \
                         {class_name_owned} (dispatching \
                         {class_name_owned}.{method_name}{method_descriptor})"
                    );
                    match crate::runtime::exceptions::create_exception_object(
                        shared,
                        thread,
                        "java/lang/IncompatibleClassChangeError",
                        Some(&msg),
                    ) {
                        Ok(exc) => {
                            return Err(MethodCallFailed::ExceptionThrown(exc));
                        }
                        // Same fallback the AbstractMethodError path
                        // below takes: never lose the diagnostic to a
                        // heap exhaustion during exception construction.
                        Err(_) => {
                            return Err(MethodCallFailed::InternalError(VmError::Internal {
                                message: msg,
                            }));
                        }
                    }
                }
                // JDK-ONLY-WAVE2 §8, 2026-08-06. The record says: "Under
                // `JdkOnly` real class bytes make every one of these
                // interfaces resolvable, so the map should become
                // unreachable rather than conditional." That is now
                // enforced instead of hoped for.
                //
                // Substituting a DIFFERENT class's native for an
                // unresolvable interface call is a compatibility
                // substitution in the §1 sense and a silent one — the
                // receiver is not an instance of `canonical`, so
                // `HashMap$KeyItr`'s native runs against something that
                // is not one. Strict mode may not do that quietly.
                //
                // Measured before changing anything: across all 53
                // regression-corpus classes the map fires **zero**
                // times, in `--real-jdk` and `--jdk-only` alike
                // (`CRATONVM_DBG_CHECK_OVERRIDE=1`, `[CANONICAL_CENSUS]
                // rows=0` in both). So this is a guard against a
                // regression, not a live path being taken away — which
                // is also why it is a refusal and not a rewrite: the
                // record warns that "removing shim mappings has
                // regressed real-JDK boot before", and `Compatible`
                // keeps the mapping untouched.
                if !canonical.is_empty() && crate::vm::dispatch_policy(shared).is_jdk_only() {
                    crate::vm::record_canonical_substitution(
                        &class_name_owned,
                        canonical,
                        method_name,
                    );
                    crate::vm::record_interface_substitution_refusal(
                        &class_name_owned,
                        canonical,
                        method_name,
                        method_descriptor,
                    );
                    // Fall through to the ordinary no-native handling
                    // below, which raises the resolution error the JVMS
                    // calls for. Deliberately NOT a silent `Ok(None)`.
                } else if !canonical.is_empty() {
                    crate::vm::record_canonical_substitution(
                        &class_name_owned,
                        canonical,
                        method_name,
                    );
                    // JDK-only §7. `bytecode_available = false`
                    // throughout: we are in the no-`Code` arm and the
                    // receiver matched no concrete class, so a
                    // registered native is the only implementation
                    // (§7 step 3b).
                    match resolve_native_for_dispatch(
                        shared,
                        canonical,
                        method_name,
                        method_descriptor,
                        false,
                    ) {
                        Ok(Some(cb)) => {
                            let r = crate::vm::safe_native_call(shared, thread, cb, args)?;
                            return Ok(r);
                        }
                        Ok(None) => {}
                        // §1.3: a refused stub must NOT fall through to
                        // the interface-name probe below. Falling
                        // through would be a silent second attempt at
                        // exactly the substitution strict mode refused.
                        Err(violation) => {
                            return Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                                violation,
                            )));
                        }
                    }
                    // Also try the cp class itself — natives may be
                    // registered directly on the interface name.
                    match resolve_native_for_dispatch(
                        shared,
                        &class_name_owned,
                        method_name,
                        method_descriptor,
                        false,
                    ) {
                        Ok(Some(cb)) => {
                            let r = crate::vm::safe_native_call(shared, thread, cb, args)?;
                            return Ok(r);
                        }
                        Ok(None) => {}
                        Err(violation) => {
                            return Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                                violation,
                            )));
                        }
                    }
                }
                // No native implements this interface method on the
                // unrecognised receiver. The cp class resolves to an
                // abstract method declaration, so fall through to the
                // AbstractMethodError path below. Fabricating a benign
                // result here is forbidden: it would make a non-empty
                // collection silently appear empty.
            }
        }
    }
    // Build an AbstractMethodError (UnsatisfiedLinkError for an unbound
    // native) so Java try/catch can see it.
    let (error_class, msg) =
        no_code_error(flags, &class_name_owned, method_name, method_descriptor);
    if crate::runtime::env_cache::nocode_dbg() {
        let recv_info = match live_args!().first().copied() {
            Some(Value::Object(Some(r))) => {
                let rc = shared.mem.heap.class_id_of(r);
                let rn = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(rc)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("<cid {rc}>"));
                // G13-1: the KIND matters as much as the class here,
                // and it was the missing half. `class_id_of` reports
                // `ClassId(0)` for an array as well as for an
                // unstamped synthetic object, and `get_class(0)`
                // resolves to `java/lang/Object` — so the two shapes
                // printed identically ("recv_cid=0
                // recv_class=java/lang/Object") and a lane reading
                // this line could not tell an `Object[]` receiver
                // from a class-less allocation. That is exactly the
                // distinction that separates "the interface door is
                // missing a row" from "a native handed us the wrong
                // object", and this record's whole first hypothesis
                // was the wrong one of those two.
                //
                // `recv_is_declaring` is the second discriminator, and
                // it separates the two mechanisms G13-1 measured behind
                // one message. `true` means the receiver's runtime
                // class IS the abstract/interface class the call
                // resolved to — i.e. a native minted an instance of an
                // abstract type and the method invoked on it has no
                // native either (`HttpRequest.version()`,
                // `PathMatcher.matches()`). `false` with
                // `recv_kind=Array`, or with a class unrelated to the
                // message, means something handed dispatch an object
                // that is not an instance of the resolved type at all
                // (`Map.isEmpty()` on an `Object[]`). The first needs a
                // registration; the second needs the caller fixed. They
                // are not the same bug and they print the same
                // sentence.
                let rk = shared.mem.heap.kind_of(r);
                let recv_is_declaring = rc == class_id;
                format!(
                    "recv_cid={rc} recv_class={rn} recv_kind={rk:?} \
                     recv_is_declaring={recv_is_declaring}"
                )
            }
            other => format!("recv={other:?}"),
        };
        eprintln!("[DBG_NOCODE] {msg} | {recv_info}");
    }
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        error_class,
        Some(&msg),
    ) {
        Ok(exc) => {
            return Err(MethodCallFailed::ExceptionThrown(exc));
        }
        Err(_) => {
            // Heap-exhausted or class-load failure during exception
            // construction — fall back to the legacy InternalError so
            // we never lose the diagnostic entirely.
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: msg,
            }));
        }
    }
}

#[cfg(test)]
mod no_code_error_tests {
    use super::{no_code_error, NoCodeFlags};

    /// An unbound native is HotSpot's `UnsatisfiedLinkError` with the method
    /// spelled `'ret holder.name(params)'`; an abstract declaration keeps the
    /// `AbstractMethodError` text it always had.
    #[test]
    fn an_unbound_native_is_an_unsatisfied_link_error() {
        let native = NoCodeFlags {
            is_native: true,
            is_static: true,
            is_synchronized: false,
        };
        assert_eq!(
            no_code_error(native, "p/Foo", "bar", "(ILjava/lang/String;)V"),
            (
                "java/lang/UnsatisfiedLinkError",
                "'void p.Foo.bar(int, java.lang.String)'".to_string()
            )
        );
        let abstract_method = NoCodeFlags {
            is_native: false,
            is_static: false,
            is_synchronized: false,
        };
        assert_eq!(
            no_code_error(abstract_method, "p/Foo", "bar", "()I"),
            (
                "java/lang/AbstractMethodError",
                "method p/Foo.bar()I has no Code attribute".to_string()
            )
        );
    }
}
