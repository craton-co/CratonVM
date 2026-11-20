#[allow(unused_imports)]
use super::*;

/// WP2.9 вЂ” Invoke a method with invokespecial semantics (no virtual dispatch).
///
/// Backs `MethodHandles.Lookup.findSpecial` and the JLS `super.m()` pattern.
/// Resolves `class_name`, walks to the declaring class for the requested
/// method, and dispatches *exactly* on that class вЂ” bypassing the
/// iface/abstract retarget that `invoke_on_class_shared` normally applies.
///
/// Native registry overrides take priority, matching `invoke_or_native`.
pub fn invoke_special_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_special_shared_impl(
        shared,
        thread,
        None,
        class_name,
        method_name,
        descriptor,
        args,
        false,
    )
}

/// [`invoke_special_shared`] with the owning class ALREADY resolved.
///
/// The name-resolving form calls `load_class_concurrent(class_name)`, which
/// consults the global binary-name map and therefore cannot tell two loaders'
/// definitions of the same name apart. A caller that knows the resolution
/// (because it resolved through the referencing class's own loader) passes it
/// here instead. `class_name` is still required — the native-override probe
/// and the diagnostics are name-keyed.
///
/// See `JitInvokeInfo::declaring_class_id` for the H2 `TestUpgrade` failure
/// that motivated this split.
pub fn invoke_special_shared_on_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_special_shared_impl(
        shared,
        thread,
        Some(class_id),
        class_name,
        method_name,
        descriptor,
        args,
        false,
    )
}

/// `invokestatic` on an owner class that is ALREADY resolved, for the one case
/// where the owner's binary NAME is not enough to name it.
///
/// A static call has no receiver, so there is no virtual retarget to suppress
/// and nothing about the dispatch depends on an instance — which is exactly why
/// [`invoke_special_shared_impl`] is the right body: its "walk from the resolved
/// class to the class that declares this method, then dispatch on that class and
/// only that class" is also JVMS §5.4.3.3's static-method lookup, and its
/// native-override probe is name-keyed and unaffected. What it must NOT do is
/// re-resolve `class_name` through the global map, which is the whole point of
/// the caller passing `class_id`.
///
/// The one difference from `invokespecial` is initialization: a static call
/// initializes the class that DECLARES the method (JVMS §5.5,
/// [`static_owner_init_pending`]), never the owner `class_id` of an inherited
/// static (wave 14).
///
/// Callers: `jit_invoke_dispatch`'s `invoke_kind == 3` arm, and only when the
/// loader-faithful owner DIFFERS from the global by-name answer — see there.
pub fn invoke_static_shared_on_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_special_shared_impl(
        shared,
        thread,
        Some(class_id),
        class_name,
        method_name,
        descriptor,
        args,
        true,
    )
}

/// `static_call`: initialize the declaring class of the method instead of
/// the resolved class (see [`invoke_static_shared_on_class`]).
#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn invoke_special_shared_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pre_resolved: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    static_call: bool,
) -> MethodCallResult {
    dbg_dispatch_tally("invoke_special_shared", class_name, method_name, descriptor);
    // A resolved static or special call follows the same bytecode-first
    // policy as invoke_or_native. The loader-faithful owner, when supplied,
    // also owns this decision: only a reviewed Intrinsic can replace Code.
    // Otherwise a compiled Arena.ofConfined site could create a modelled
    // session while its interpreted twin created a real ConfinedSession.
    //
    // `resolve_id` + `callback_of` is `find` (same prefilter, exact slot and
    // quirk fallback) that also names the slot, whose synchronized-method fact
    // `by_name_native_call` needs for the monitor.
    let registry = &shared.natives.native_methods;
    if let Some((callback, native_id)) = registry
        .resolve_id(class_name, method_name, descriptor)
        .and_then(|id| registry.callback_of(id).map(|callback| (callback, id)))
    {
        // JIT round 15 wave 6 (lane sweepB; WK4-2): an agent-woven owner's
        // body beats the native registered on it for every caller of this
        // door (`findSpecial`, `ctx.invoke_special`, the two-loader static
        // tail), as the compiled `invokespecial` tail decides; the bytecode
        // tail below asks the same predicate in its `override_cb`.
        // `CRATONVM_ANCESTOR_REDEFINE_RULE=0` restores the native.
        let woven_owner_body_wins = crate::jit::helpers::special_shared_owner_body_wins(
            shared,
            pre_resolved,
            class_name,
            method_name,
            descriptor,
            match args.first() {
                Some(Value::Object(Some(receiver))) if !static_call => Some(*receiver),
                _ => None,
            },
        );
        if !woven_owner_body_wins
            && !registered_native_yields_to_bytecode_on_owner(
                shared,
                pre_resolved,
                class_name,
                method_name,
                descriptor,
                callback,
                registry
                    .kind_of_id(native_id)
                    .unwrap_or(cratonvm_native_api::NativeKind::Bridge),
            )
            && !crate::runtime::interpreter::synthetic_stub_should_yield_to_real_bytecode(
                shared,
                class_name,
                method_name,
                descriptor,
            )
        {
            return by_name_native_call(
                shared,
                thread,
                callback,
                Some(native_id),
                args,
                (class_name, method_name, descriptor),
            )
            .map(|v| coerce_native_return(v, descriptor));
        }
    }

    // GC-safety: pin object args across class load + <clinit> and re-read the
    // forwarded addresses before dispatch -- same moving-GC stale-args window
    // as `invoke_shared` above.
    let pin_base = thread.native_pin_roots.len();
    let has_obj_args = push_object_arg_pins(thread, args);

    // Resolve and initialize the class. A caller that already resolved it
    // through the referencing class's loader passes the answer in; only the
    // name-keyed entry point falls back to the global map.
    let class_id = match pre_resolved {
        Some(cid) => cid,
        None => match shared.load_class_concurrent(class_name) {
            Ok(cid) => cid,
            Err(e) => {
                thread.native_pin_roots.truncate(pin_base);
                return Err(e.into());
            }
        },
    };
    // A static call initializes the declaring class only (`None` once it
    // is initialized); invokespecial initializes the resolved class.
    let init_class = if static_call {
        static_owner_init_pending(shared, class_id, class_name, method_name, descriptor)
    } else {
        Some(class_id)
    };
    if let Some(init_class) = init_class {
        if let Err(e) = super::ensure_class_initialized_shared(shared, thread, init_class) {
            thread.native_pin_roots.truncate(pin_base);
            return Err(e);
        }
    }

    // For default-method super-call: walk the hierarchy to the declaring
    // class so we land on the interface that owns the bytecode. This
    // ensures we don't re-resolve via the receiver's overriding method.
    let target_class_id = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        match crate::classloading::find_method_recursive(class_id, method_name, descriptor, store) {
            Some((_, declaring_id)) => declaring_id,
            None => class_id,
        }
    };

    // A refreshed copy only when the load or the initializer moved an
    // argument; the pins go before the dispatch, whose own pinned copy roots
    // the references (nothing in between collects). Wave 15: this copied on
    // every call with an object argument.
    let fresh = if has_obj_args {
        args_refreshed_if_moved(thread, pin_base, args)
    } else {
        None
    };
    thread.native_pin_roots.truncate(pin_base);
    invoke_on_class_shared_no_retarget(
        shared,
        thread,
        target_class_id,
        method_name,
        descriptor,
        fresh.as_deref().unwrap_or(args),
    )
}

/// Standalone variant of invokespecial semantics for a native that IS ITSELF
/// the native registered for `(class_name, method_name, descriptor)` and
/// needs to run THAT class's own real bytecode body directly (true
/// invokespecial / `super.m()` semantics: resolved statically on
/// `class_name`'s own hierarchy, never re-dispatched to a receiver's
/// overriding subclass).
///
/// Deliberately independent of [`invoke_special_shared`] -- NOT a thin
/// wrapper around it or a shared tail with it. Two things a native calling
/// itself must avoid, both of which [`invoke_special_shared`]'s own
/// machinery would reintroduce:
///
/// 1. Its native-check-first step would re-find and re-invoke the very
///    native calling in (unbounded Rust-stack recursion).
/// 2. Its bytecode fallback (`invoke_on_class_shared_no_retarget` ->
///    `invoke_on_class_shared_inner`) does its OWN unconditional
///    native-registry re-check for any non-interface declaring class
///    (`override_cb`) -- for a target class that itself has a registered
///    native, that ALSO re-finds and re-invokes the native forever, this
///    time as pure Rust-call recursion with no growing Java stack (a raw
///    native stack overflow / process abort, not even a clean
///    `StackOverflowError` -- confirmed via a standalone
///    `ScheduledThreadPoolExecutor.shutdown()` repro during this fix's own
///    verification).
///
/// This is exactly the trap `NativeContext::invoke_virtual_bytecode_only`'s
/// own doc comment already documents (it calls `interpreter::execute`
/// directly for the same reason) -- the bug this function exists to fix was
/// a native registered on `ThreadPoolExecutor.shutdown()`, reached via
/// `ScheduledThreadPoolExecutor.shutdown()`'s `super.shutdown()`, whose
/// `invoke_virtual_bytecode_only(this, "shutdown", ...)` call resolved the
/// declaring class from the STPE receiver's DYNAMIC class -- re-finding
/// STPE's own overriding shutdown() and looping forever. See
/// threadpoolexecutor-shutdown-super-call-self-recursion-FIXED.md.
pub fn invoke_special_bytecode_only_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_special_bytecode_only_shared_impl(
        shared,
        thread,
        None,
        None,
        class_name,
        method_name,
        descriptor,
        args,
    )
}

pub fn invoke_special_bytecode_only_shared_on_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_special_bytecode_only_shared_impl(
        shared,
        thread,
        Some(class_id),
        None,
        class_name,
        method_name,
        descriptor,
        args,
    )
}

/// [`invoke_special_bytecode_only_shared_on_class`] with a declared-method
/// index hint, for a native that re-enters its own JDK body on every miss
/// (interpreter round i3 wave 76, lane L4; `CRATONVM_NATIVE_OWN_BODY_HINT`,
/// default off, `VmConfig::native_own_body_hint`).
///
/// `*method_hint` names the method's index among `class_id`'s OWN declared
/// methods. When the switch is on and the index still names a non-abstract
/// `(method_name, descriptor)` there, the by-name hierarchy walk is skipped
/// and `interpreter::execute` is handed the index too (it re-verifies it and
/// skips its own by-name scan). Otherwise the call resolves by name as
/// [`invoke_special_bytecode_only_shared_on_class`] does. On return
/// `*method_hint` holds the index to pass next time (`None` when the switch is
/// off or the method is inherited). A stale hint (a redefinition reordered the
/// methods) is refused by the check and re-learnt; a hint that still matches
/// names the same method by construction (a class declares one method per
/// name and descriptor).
#[allow(clippy::too_many_arguments)]
pub fn invoke_special_bytecode_only_shared_hinted(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_hint: &mut Option<u32>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    if !shared.config.native_own_body_hint {
        *method_hint = None;
        return invoke_special_bytecode_only_shared_on_class(
            shared,
            thread,
            class_id,
            class_name,
            method_name,
            descriptor,
            args,
        );
    }
    invoke_special_bytecode_only_shared_impl(
        shared,
        thread,
        Some(class_id),
        Some(method_hint),
        class_name,
        method_name,
        descriptor,
        args,
    )
}

/// The index of `(method_name, descriptor)` among `class`'s own declared
/// methods, when it is declared there and is not abstract (the method
/// `find_method_recursive` answers first from that class).
pub(in crate::vm::vm_exec) fn own_concrete_method_index(
    class: &crate::classloading::Class,
    method_name: &str,
    descriptor: &str,
) -> Option<u32> {
    let pos = class
        .methods
        .iter()
        .position(|m| &*m.name == method_name && &*m.descriptor == descriptor)?;
    if class.methods[pos].is_abstract() {
        return None;
    }
    u32::try_from(pos).ok()
}

#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn invoke_special_bytecode_only_shared_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pre_resolved: Option<ClassId>,
    method_hint: Option<&mut Option<u32>>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // GC-safety: pin object args across class load + <clinit> and re-read the
    // forwarded addresses before dispatch -- same moving-GC stale-args window
    // as `invoke_shared`/`invoke_special_shared`.
    let pin_base = thread.native_pin_roots.len();
    let mut has_obj_args = false;
    for a in args {
        if let Value::Object(Some(o)) = a {
            thread.native_pin_roots.push(*o);
            has_obj_args = true;
        }
    }

    // Resolve and initialize the class. If pre_resolved was given, use it
    // directly. Otherwise, check whether the receiver argument (args[0]) has
    // class_name in its own loaded class hierarchy -- preserving the defining
    // ClassLoader of isolated loaders (e.g. Spring Boot's ModifiedClassPathClassLoader)
    // rather than collapsing to the global Application-loader copy.
    let class_id = if let Some(cid) = pre_resolved {
        cid
    } else if let Some(Value::Object(Some(receiver))) = args.first() {
        let receiver_cid = shared.mem.heap.class_id_of(*receiver);
        let found_cid = {
            let cm = shared.classes.class_manager.read();
            let mut curr = Some(receiver_cid);
            let mut match_id = None;
            while let Some(cid) = curr {
                if let Some(c) = cm.get_class(cid) {
                    if &*c.name == class_name {
                        match_id = Some(cid);
                        break;
                    }
                    curr = c.superclass;
                } else {
                    break;
                }
            }
            match_id
        };
        if let Some(cid) = found_cid {
            cid
        } else {
            match shared.load_class_concurrent(class_name) {
                Ok(cid) => cid,
                Err(e) => {
                    thread.native_pin_roots.truncate(pin_base);
                    return Err(e.into());
                }
            }
        }
    } else {
        match shared.load_class_concurrent(class_name) {
            Ok(cid) => cid,
            Err(e) => {
                thread.native_pin_roots.truncate(pin_base);
                return Err(e.into());
            }
        }
    };
    if let Err(e) = super::ensure_class_initialized_shared(shared, thread, class_id) {
        thread.native_pin_roots.truncate(pin_base);
        return Err(e);
    }

    // Walk the hierarchy FROM `class_name`'s own class_id (never the
    // receiver's dynamic class) to the declaring class -- true static
    // binding, matching `invoke_special_shared`.
    //
    // `CRATONVM_NATIVE_OWN_BODY_HINT` (wave 76, L4): a hint that still names
    // a concrete `(method_name, descriptor)` declared by `class_id` itself IS
    // the walk's answer (the walk returns the first concrete declaration from
    // `class_id` up), so the walk is skipped; a miss walks and re-learns.
    let hint_in: Option<u32> = method_hint.as_deref().copied().flatten();
    let (target_class_id, exec_hint) = {
        let cm = shared.classes.class_manager.read();
        let verified = hint_in.filter(|&i| {
            cm.get_class(class_id)
                .and_then(|c| c.methods.get(i as usize))
                .is_some_and(|m| {
                    &*m.name == method_name && &*m.descriptor == descriptor && !m.is_abstract()
                })
        });
        match verified {
            Some(i) => (class_id, Some(i)),
            None => {
                let store = &cm.class_store;
                match crate::classloading::find_method_recursive(
                    class_id,
                    method_name,
                    descriptor,
                    store,
                ) {
                    Some((_, declaring_id)) => {
                        let learnt = if method_hint.is_some() && declaring_id == class_id {
                            cm.get_class(class_id)
                                .and_then(|c| own_concrete_method_index(c, method_name, descriptor))
                        } else {
                            None
                        };
                        (declaring_id, learnt)
                    }
                    None => (class_id, None),
                }
            }
        }
    };
    if let Some(slot) = method_hint {
        *slot = exec_hint;
    }
    if let Some(i) = exec_hint {
        crate::runtime::interpreter::set_execute_method_hint(target_class_id, i);
    }

    // NOT invoke_on_class_shared_no_retarget -- see this function's own doc
    // comment (point 2) for why. Call `interpreter::execute` directly, the
    // same "just run this bytecode, no native check" primitive
    // `NativeContext::invoke_virtual_bytecode_only` uses.
    //
    // A refreshed copy only when the load or the initializer moved an
    // argument (interpreter round i3 wave 76, lane L4; the wave-15 rule of
    // `invoke_special_shared` above): this copied into a fresh `Vec` on every
    // call with an object argument, i.e. on every own-body re-entry of a
    // memo `Intrinsic`. The values handed to `execute` are the same either
    // way (an unmoved pin equals its argument), and the pins still live
    // across the call, as before.
    let fresh = if has_obj_args {
        args_refreshed_if_moved(thread, pin_base, args)
    } else {
        None
    };
    // JIT round 21 lane reflect4 (REFLECT4-21-1): or the door-invisible
    // compiled body of a reviewed `Intrinsic`'s own JDK body.
    let result = crate::runtime::interpreter::bytecode_only_body::execute_bytecode_only(
        shared,
        thread,
        target_class_id,
        method_name,
        descriptor,
        fresh.as_deref().unwrap_or(args),
    );
    thread.native_pin_roots.truncate(pin_base);
    result
}

// ---------------------------------------------------------------------------
// Dynamic Proxy dispatch (java.lang.reflect.Proxy)
// ---------------------------------------------------------------------------

/// WP2.5 вЂ” set a field on a synthetic `Method` object by JDK field name.
///
/// The proxy dispatch path used to write the synthesized `Method` object
/// using hard-coded slot indices (0=class, 1=name, 2=returnType, ...).
/// That layout was wrong: the real JDK `java.lang.reflect.Method` class
/// inherits a long chain of fields from `AccessibleObject` and
/// `Executable` first, so `name` is *not* at slot 1. The miswrite
/// surfaced as `m.getName()` returning null inside an InvocationHandler,
/// which silently broke any handler that branched on the method name вЂ”
/// the typical pattern for multi-interface proxies (FAIL-5) and
/// proxies on interfaces with default methods (FAIL-6).
///
/// This helper centralizes the by-name resolve so both
/// `proxy_invoke_handler` (NativeContextImpl path) and
/// `proxy_invoke_handler_shared` (shared interpreter path) use it.
pub(in crate::vm::vm_exec) fn proxy_method_set_field_by_name(
    shared: &SharedVm,
    method_obj: ObjectRef,
    field_name: &str,
    value: Value,
) {
    let class_id = shared.mem.heap.class_id_of(method_obj);
    let cm = shared.classes.class_manager.read();
    if let Some(idx) = resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store) {
        drop(cm);
        shared.mem.heap.set_field(method_obj, idx, value);
    }
}

/// The `OutOfMemoryError` a proxy, annotation-proxy or boxing site reports
/// when its collection-free allocation is refused.
///
/// gc-common w5-c (`handoff-w4c-infallible-callers-residue.md` §2): these
/// sites allocated through the INFALLIBLE `VmHeap::alloc_object` /
/// `alloc_array`, which on exhaustion aborts the process on Generational and
/// ZGC and spends the emergency reserve G1 keeps for the callers that truly
/// cannot report failure (`common-f-g1-fatal-abort-from-infallible-alloc-object`).
/// Each of them has a `MethodCallResult` in scope, so they now allocate with
/// `try_alloc_*_full` and report this instead. No collection: every one of
/// them holds bare `ObjectRef`s in Rust locals across the allocation. The
/// parenthesised site never reaches Java (`RuntimeError::as_java_throwable`).
pub(in crate::vm::vm_exec) fn proxy_heap_exhausted(site: &'static str) -> MethodCallFailed {
    RuntimeError::OutOfMemoryError {
        message: format!("Java heap space (proxy {site})"),
    }
    .into()
}

/// S111r11 — write the CratonVM extra metadata slots on a synthetic
/// proxy `Method` object so that
/// `native-builtins/.../lang_class.rs::native_method_invoke` finds the
/// descriptor + parameter-count when it later reflects through.
///
/// `native_method_invoke` reads the descriptor from the extra slot
/// (`base + METHOD_EXTRA_OFFSET_DESC`), NOT from the JDK `signature`
/// field — leaving these blank produces a `descriptor=""` virtual
/// dispatch that misses the real bridge method on the receiver class
/// (e.g. `ParameterizedTypeImpl.getRawType` covariant bridge) and
/// surfaces as a confusing `NoSuchMethodError`.
///
/// Constants mirror those in
/// `native-builtins/src/lang_class.rs::METHOD_EXTRA_*`. We don't import
/// them across the crate boundary because `vm` doesn't depend on
/// `native-builtins` (the dependency goes the other way).
pub(in crate::vm::vm_exec) fn proxy_method_write_extra_slots(
    shared: &SharedVm,
    method_obj: ObjectRef,
    descriptor: &str,
    param_count: usize,
) {
    const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;
    const METHOD_EXTRA_OFFSET_DESC: usize = 0;
    const METHOD_EXTRA_OFFSET_PARAM_COUNT: usize = 1;

    let class_id = shared.mem.heap.class_id_of(method_obj);
    let total_fields = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.first_field_index + c.fields.iter().filter(|f| !f.is_static()).count())
        .unwrap_or(0);
    let base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, total_fields);

    // Defensive: only write if the heap object was allocated wide enough.
    // `proxy_invoke_handler` and `proxy_invoke_handler_shared` both call
    // `alloc_object(method_class_id, total_fields.max(8))` — that gives
    // us at least the JDK width, but no extra slots when running against
    // a synthetic-stub Method class. In that case skip the writes; the
    // legacy hard-coded slots above already covered the synthetic path.
    let num_obj_fields = shared.mem.heap.num_fields(method_obj);
    let extra_desc_slot = base + METHOD_EXTRA_OFFSET_DESC;
    let extra_pc_slot = base + METHOD_EXTRA_OFFSET_PARAM_COUNT;
    if num_obj_fields <= extra_pc_slot {
        return;
    }
    let desc_str = super::create_java_string(shared, descriptor);
    shared
        .mem
        .heap
        .set_field(method_obj, extra_desc_slot, Value::Object(Some(desc_str)));
    shared
        .mem
        .heap
        .set_field(method_obj, extra_pc_slot, Value::Int(param_count as i32));
}

/// When a method is called on a `Proxy$Instance` object, this function
/// intercepts it and forwards to the `InvocationHandler.invoke()`.
///
/// Layout of Proxy$Instance (1-field):
///   - field 0: InvocationHandler reference
///
/// The InvocationHandler interface requires:
///   Object invoke(Object proxy, Method method, Object[] args)
///
/// We build a minimal `java.lang.reflect.Method` object (7 fields, same layout
/// as builtins.rs `create_method_object`) and a boxed Object[] of args.
pub(in crate::vm) fn proxy_invoke_handler(
    ctx: &mut NativeContextImpl<'_>,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Get the InvocationHandler from proxy field 0
    let handler_ref = match ctx.shared.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(h)) => h,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };

    // A real JDK annotation is a generated `$ProxyN` whose handler is our
    // synthetic AnnotationProxy. Its primitive members must leave this native
    // virtual-dispatch boundary as raw JVM Values, not boxed wrappers.
    if class_name_is(
        ctx.shared,
        handler_ref,
        "java/lang/annotation/AnnotationProxy",
    ) {
        return proxy_unbox_primitive_return(
            ctx.shared,
            descriptor,
            proxy_annotation_handler_invoke(ctx.shared, ctx.thread, handler_ref, method_name, args),
        );
    }

    // WP2.5 вЂ” build the Method object using **field-name-based** writes
    // so the JDK-real layout (which has many inherited fields from
    // AccessibleObject and Executable before `name`/`returnType`/...)
    // is honored. Hard-coded slot indices like the original 0..7 break
    // `Method.getName()` because the real `name` field is not at slot 1
    // вЂ” it's at whatever index the JDK class file declares it. Without
    // this, the InvocationHandler's `m.getName()` returns null and any
    // handler that branches on the method name (the common case for
    // multi-interface and default-method tests) silently returns null.
    // `--real-jdk`: resolve the signature's classes through the declaring
    // interface's loader BEFORE allocating anything this function keeps (the
    // loader runs Java). See `proxy_signature_class_ids`.
    let (param_descs, ret_desc) = proxy_split_descriptor(descriptor);
    let (sig_ids, proxy, args_refreshed) = match proxy_signature_class_ids_pinned(
        ctx.shared,
        ctx.thread,
        proxy,
        method_name,
        descriptor,
        args,
    ) {
        Some((ids, proxy, fresh)) => (ids, proxy, Some(fresh)),
        None => (Vec::new(), proxy, None),
    };
    let args: &[Value] = args_refreshed.as_deref().unwrap_or(args);
    let handler_ref = match ctx.shared.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(h)) => h,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    let sig_at = |i: usize| sig_ids.get(i).copied().flatten();
    let method_class_id = ctx
        .shared
        .classes
        .class_manager_write_settling()
        .load_class("java/lang/reflect/Method")
        .unwrap_or(ClassId::new(0));
    let total_fields = ctx
        .shared
        .classes
        .class_manager
        .read()
        .get_class(method_class_id)
        .map(|c| c.first_field_index + c.fields.iter().filter(|f| !f.is_static()).count())
        .unwrap_or(8);
    // S111r11: allocate METHOD_EXTRA_SLOTS more than the JDK layout so
    // `proxy_method_write_extra_slots` can write descriptor + param count
    // for `native_method_invoke` to read back.
    const METHOD_EXTRA_SLOTS: usize = 3;
    const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;
    let alloc_base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, total_fields);
    let method_obj = ctx
        .shared
        .mem
        .heap
        .try_alloc_object_full(method_class_id, alloc_base + METHOD_EXTRA_SLOTS)
        .ok_or_else(|| proxy_heap_exhausted("Method"))?;
    // gch h2/fj: every mirror of this Method on the unwinding arm
    // (`proxy_mirror_or_oome`): a full heap answers `OutOfMemoryError`.
    let zero_mirror =
        proxy_mirror_or_oome(|| super::get_or_create_class_mirror(ctx.shared, ClassId::new(0)))?;
    // Resolve the actual declaring-interface mirror for the synthesized
    // Method's `clazz` field — see `proxy_resolve_declaring_class_mirror`
    // for why `ClassId(0)` (== `Object` in production) is unsafe here.
    let declaring_mirror = proxy_mirror_or_oome(|| {
        proxy_resolve_declaring_class_mirror(ctx.shared, proxy, method_name, descriptor)
    })?;
    let name_str = super::try_create_java_string(ctx.shared, method_name)
        .ok_or_else(|| proxy_heap_exhausted("Method.name"))?;
    // Parse descriptor into per-parameter and return type descriptors so we
    // can populate the synthetic Method's `returnType` and `parameterTypes`
    // with semantically correct Class mirrors (e.g. real `Type.class`).
    // Without this, Spring's `TypeProxyInvocationHandler.invoke` falls
    // through to its default branch (`method.invoke(provider.getType())`)
    // and our native_method_invoke virtual-dispatches the proxy method
    // name on the underlying Type — calling e.g. `getGenericInterfaces`
    // on a `ParameterizedTypeImpl` (NoSuchMethodError).
    let return_type_mirror = proxy_signature_mirror(ctx.shared, sig_at(0), &ret_desc)?;
    let param_count = param_descs.len();
    // `Class[]`, not `Object[]`: `Method.getParameterTypes()` is real
    // bytecode that clones this field and `checkcast`s it to `[Ljava/lang/Class;`.
    let param_arr = ctx
        .shared
        .mem
        .heap
        .try_alloc_array_full(
            proxy_class_array_component(ctx.shared),
            crate::memory::heap::ArrayElementType::Reference,
            param_count,
        )
        .ok_or_else(|| proxy_heap_exhausted("Method.parameterTypes"))?;
    for (i, pdesc) in param_descs.iter().enumerate() {
        let pmirror = proxy_signature_mirror(ctx.shared, sig_at(i + 1), pdesc)?;
        ctx.shared
            .mem
            .heap
            .set_array_element(param_arr, i, Value::Object(Some(pmirror)))
            .ok();
    }
    let desc_str = super::try_create_java_string(ctx.shared, descriptor)
        .ok_or_else(|| proxy_heap_exhausted("Method.descriptor"))?;
    proxy_method_set_field_by_name(
        ctx.shared,
        method_obj,
        "clazz",
        Value::Object(Some(declaring_mirror)),
    );
    proxy_method_set_field_by_name(
        ctx.shared,
        method_obj,
        "name",
        Value::Object(Some(name_str)),
    );
    proxy_method_set_field_by_name(
        ctx.shared,
        method_obj,
        "returnType",
        Value::Object(Some(return_type_mirror)),
    );
    proxy_method_set_field_by_name(
        ctx.shared,
        method_obj,
        "parameterTypes",
        Value::Object(Some(param_arr)),
    );
    // `Method.getExceptionTypes()` is specified to return a non-null Class[]
    // and proxy InvocationHandlers receive this synthesized Method directly.
    // Keep the real declared throws clause instead of inheriting alloc_object's
    // null default (Spring's ReflectionUtils iterates this array unconditionally).
    let exception_arr =
        proxy_method_exception_types(ctx.shared, declaring_mirror, method_name, descriptor)
            .ok_or_else(|| proxy_heap_exhausted("Method.exceptionTypes"))?;
    proxy_method_set_field_by_name(
        ctx.shared,
        method_obj,
        "exceptionTypes",
        Value::Object(Some(exception_arr)),
    );
    let method_modifiers =
        proxy_resolve_declaring_method_modifiers(ctx.shared, proxy, method_name, descriptor);
    proxy_method_set_field_by_name(
        ctx.shared,
        method_obj,
        "modifiers",
        Value::Int(method_modifiers),
    );
    proxy_method_set_field_by_name(
        ctx.shared,
        method_obj,
        "signature",
        Value::Object(Some(desc_str)),
    );
    proxy_method_set_field_by_name(ctx.shared, method_obj, "slot", Value::Int(0));
    // S111r11: also populate the CratonVM extra-slot descriptor +
    // parameter-count cache so `native_method_invoke` (which reads via
    // `read_method_descriptor`, NOT `signature`) sees a non-empty
    // descriptor when the proxy fallback path reflectively re-invokes
    // the method on the underlying Type — fixes
    // `NoSuchMethodError: ParameterizedTypeImpl.getRawType` (descriptor
    // was empty, so virtual dispatch couldn't match the bridge method).
    proxy_method_write_extra_slots(ctx.shared, method_obj, descriptor, param_count);
    // Belt-and-suspenders: also write the legacy hard-coded slots so any
    // surviving raw-index reader (notably the lambda dispatch path that
    // pulls `Method.getName` via `get_field_by_name` already lands on
    // the right slot, but older diagnostic readers may still poke 1..7).
    if total_fields >= 8 {
        // Skip вЂ” class layout already has the JDK fields populated above.
    } else {
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 0, Value::Object(Some(declaring_mirror)));
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 1, Value::Object(Some(name_str)));
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 2, Value::Object(Some(return_type_mirror)));
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 3, Value::Object(Some(param_arr)));
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 4, Value::Int(method_modifiers));
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 5, Value::Object(Some(desc_str)));
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 6, Value::Int(param_count as i32));
        ctx.shared
            .mem
            .heap
            .set_field(method_obj, 9, Value::Object(Some(exception_arr)));
        // Silence "zero_mirror unused" — kept above to preserve the
        // original allocation flow.
        let _ = zero_mirror;
    }

    // Build Object[] of args вЂ” box primitives so InvocationHandler receives Object[].
    //
    // Per `java.lang.reflect.InvocationHandler.invoke` contract: when the
    // intercepted interface method takes no arguments, the `args` parameter
    // MUST be `null` (not an empty array). Spring's
    // `SerializableTypeWrapper$TypeProxyInvocationHandler.invoke` relies on
    // this — its `Type[].class` return-type branch is gated on `args == null`
    // (`aload_3 / ifnonnull -> default`), and the default branch reflectively
    // calls `method.invoke(provider.getType(), args)`. For 0-arg methods like
    // `getGenericInterfaces()`, passing an empty array (instead of null) makes
    // the branch fall through to the default, which then virtually dispatches
    // `getGenericInterfaces` on `provider.getType()` — a `ParameterizedTypeImpl`
    // that has no such method, surfacing as `NoSuchMethodError`.
    let args_value = if args.is_empty() {
        Value::Object(None)
    } else {
        let args_arr = ctx
            .shared
            .mem
            .heap
            .try_alloc_array_full(
                ClassId::new(0),
                crate::memory::heap::ArrayElementType::Reference,
                args.len(),
            )
            .ok_or_else(|| proxy_heap_exhausted("argument array"))?;
        for (i, arg) in args.iter().enumerate() {
            // Box per the formal parameter descriptor (Z/C/B/S/I distinct
            // wrappers; all share Value::Int) — see proxy_box_value_for_desc.
            let boxed = match param_descs.get(i) {
                Some(pdesc) => proxy_box_value_for_desc(ctx.shared, *arg, pdesc),
                None => proxy_box_value(ctx.shared, *arg),
            }
            .ok_or_else(|| proxy_heap_exhausted("argument box"))?;
            ctx.shared
                .mem
                .heap
                .set_array_element(args_arr, i, boxed)
                .ok();
        }
        Value::Object(Some(args_arr))
    };

    // Call InvocationHandler.invoke(Object proxy, Method method, Object[] args)
    // Resolve the handler's actual class for dispatch (may be anonymous).
    let handler_class_id = ctx.shared.mem.heap.class_id_of(handler_ref);

    // WP2.5: lambda InvocationHandler вЂ” see `proxy_invoke_handler_shared`
    // for the rationale. The lambda's class_id is synthetic and not in
    // the class store; dispatching by name would land on the abstract
    // interface method (no Code attribute).
    let handler_is_lambda = ctx
        .shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&handler_class_id);
    if handler_is_lambda {
        let call_args = [
            Value::Object(Some(proxy)),
            Value::Object(Some(method_obj)),
            args_value,
        ];
        // See `proxy_invoke_handler_shared`: route a lambda-handler throw through
        // the UndeclaredThrowableException parity wrap instead of `?`.
        //
        // gen r4w3/rooting: pin `proxy` across the handler body (a GC point)
        // — the wrap below reads its fields afterwards.
        let proxy_pin = ctx.thread.native_pin_roots.len();
        ctx.thread.native_pin_roots.push(proxy);
        let dispatch_res = crate::runtime::interpreter::try_lambda_dispatch(
            ctx.shared,
            ctx.thread,
            handler_ref,
            handler_class_id,
            "invoke",
            "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
            &call_args,
        );
        let proxy = ctx.thread.native_pin_roots[proxy_pin];
        ctx.thread.native_pin_roots.truncate(proxy_pin);
        let dispatch = match dispatch_res {
            Ok(d) => d,
            Err(failure) => {
                return proxy_wrap_undeclared_if_needed(
                    ctx.shared,
                    ctx.thread,
                    proxy,
                    method_name,
                    descriptor,
                    Err(failure),
                );
            }
        };
        if let Some(result) = dispatch {
            // See the identical fix + rationale on the sibling
            // object-handler path below: a primitive-returning proxy
            // method whose target is ITSELF a dynamic proxy re-boxes an
            // already-boxed wrapper (`Method.invoke`'s generic `Object`
            // return contract) into a fresh wrapper's raw-value slot,
            // corrupting the value (observed: `Bean.getAge()` through two
            // nested JDK proxies returned an unrelated int instead of 5).
            // G24-1: this is a USER handler's value, so it takes the strict
            // contract (null -> NPE, wrong type -> CCE), not the annotation
            // arm's lenient unbox.
            return proxy_coerce_handler_return(ctx.shared, descriptor, Ok(result));
        }
        return Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::AbstractMethodError {
                class_name: format!("<lambda#{}>", handler_class_id),
                method_name: "invoke".to_string(),
            },
        )));
    }

    let handler_class_name = ctx
        .shared
        .classes
        .class_manager
        .read()
        .get_class(handler_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| "java/lang/reflect/InvocationHandler".to_string());

    let invoke_args = [
        Value::Object(Some(handler_ref)),
        Value::Object(Some(proxy)),
        Value::Object(Some(method_obj)),
        args_value,
    ];
    // gen r4w3/rooting: pin `proxy` across the handler body (a GC point) —
    // `proxy_wrap_undeclared_if_needed` reads its fields afterwards.
    let proxy_pin = ctx.thread.native_pin_roots.len();
    ctx.thread.native_pin_roots.push(proxy);
    let result = ctx.invoke_or_native(
        &handler_class_name,
        "invoke",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        &invoke_args,
    );
    let proxy = ctx.thread.native_pin_roots[proxy_pin];
    ctx.thread.native_pin_roots.truncate(proxy_pin);
    // `InvocationHandler.invoke` returns generic `Object`, so a primitive-
    // returning proxy method (e.g. `getAge()I`) whose result came back as a
    // boxed wrapper needs unboxing here -- exactly like the AnnotationProxy
    // branch above already does via this same helper. Without it, a proxy
    // wrapping ANOTHER dynamic proxy (the handler's own body reflectively
    // re-invokes through `Method.invoke` on the nested proxy, which boxes
    // ITS primitive result) receives an already-boxed wrapper here and
    // would re-box the wrapper's OBJECT REFERENCE into a fresh wrapper's
    // raw `int` slot -- corrupting the value into an unrelated number
    // (confirmed: double-nested `Proxy.newProxyInstance` around a plain
    // pass-through `InvocationHandler`, `int getAge()` returned garbage
    // instead of the real value; a single proxy layer was unaffected).
    //
    // G24-1 promoted this from the lenient unbox to the strict contract: the
    // value here is whatever the user's `InvocationHandler` chose, so `null`
    // for a primitive return is an NPE and a mismatched wrapper is a CCE. The
    // nested-proxy case above still works, because a nested dispatch hands back
    // an already-raw value and every arm of the strict helper passes those
    // through untouched.
    proxy_coerce_handler_return(
        ctx.shared,
        descriptor,
        proxy_wrap_undeclared_if_needed(
            ctx.shared,
            ctx.thread,
            proxy,
            method_name,
            descriptor,
            result,
        ),
    )
}

/// Dispatch a method call on an annotation proxy object.
///
/// WP2.7: spec-compliant per `java.lang.annotation.Annotation` JavaDoc.
///
/// Layout of AnnotationProxy (4-field):
///   field 0 = String (type descriptor, e.g. "Ljava/lang/Override;")
///   field 1 = Class mirror (annotation type вЂ” for `annotationType()`)
///   field 2 = String[] (element names)
///   field 3 = Object[] (element values, parallel to names)
///
/// Methods implemented:
///   * `annotationType()` вЂ” returns the cached Class mirror (field 1).
///   * `toString()` вЂ” `@TypeName(name1=val1, name2=val2)` with members in
///     declaration order (matches the order WP1.7 captures, which mirrors
///     the order the source compiler wrote into the class file).
///   * `hashCode()` вЂ” sum over members of `(127 * nameHash) ^ valueHash`,
///     per `Annotation.hashCode()` Javadoc.
///   * `equals(Object)` вЂ” `true` iff the other reference is also an
///     `AnnotationProxy` of the same annotation type AND every element
///     value compares `equals` (by `valueEquals` semantics вЂ” array values
///     use `Arrays.equals`, scalar values use `Object.equals`).
///
/// Element accessor methods (e.g. `value()`, `count()`, `nested()`) walk
/// the parallel arrays and return the stored boxed value.
pub(in crate::vm::vm_exec) fn annotation_proxy_invoke(
    ctx: &mut NativeContextImpl<'_>,
    proxy: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    // Route through the SHARED entry point, not `annotation_proxy_dispatch_impl`
    // directly: only the shared one carries the `asMap` handler and the
    // cross-type `equals` delegation (an `AnnotationProxy` compared against a
    // FOREIGN annotation proxy of the same type -- e.g. Spring's
    // `MergedAnnotation.synthesize()` JDK proxy -- must delegate to that
    // proxy's own `equals`, or equality is asymmetric).
    //
    // This is the path a native takes via `NativeContext::invoke_virtual`,
    // which is how AssertJ's natively-shimmed
    // `StandardComparisonStrategy.areEqual` performs its final
    // `actual.equals(other)`. Bypassing the delegation made
    // `assertThat(realAnnotation).isEqualTo(synthesizedAnnotation)` fail while
    // the identical call written in Java passed
    // (`core.annotation.MergedAnnotationsTests.equalsForSynthesizedAnnotations`).
    annotation_proxy_invoke_shared(ctx.shared, ctx.thread, proxy, method_name, args)
}

/// Shared-interpreter version of `proxy_invoke_handler` вЂ” callable from the
/// iterative interpreter without a `NativeContextImpl`.
/// Whether `obj`'s runtime class has the given internal name.
pub(in crate::vm::vm_exec) fn class_name_is(shared: &SharedVm, obj: ObjectRef, name: &str) -> bool {
    shared
        .classes
        .class_manager
        .read()
        .get_class(shared.mem.heap.class_id_of(obj))
        .map(|c| &*c.name == name)
        .unwrap_or(false)
}

/// Return the result of an annotation handler as the proxied method's JVM
/// return value. Annotation element values are stored as Java wrappers, while
/// the generated `$ProxyN` method has a primitive JVM return descriptor. Keep
/// the boundary here (rather than in the individual callers) so native and
/// interpreter proxy dispatch cannot accidentally hand an `Integer` reference
/// to code expecting an `int`.
pub(crate) fn proxy_unbox_primitive_return(
    shared: &SharedVm,
    descriptor: &str,
    result: MethodCallResult,
) -> MethodCallResult {
    let Some(value) = result? else {
        return Ok(None);
    };
    let ret = descriptor
        .rsplit(')')
        .next()
        .unwrap_or("L")
        .chars()
        .next()
        .unwrap_or('L');
    match ret {
        'I' | 'Z' | 'B' | 'C' | 'S' | 'J' | 'F' | 'D' => {
            if let Value::Object(Some(wrapper)) = value {
                Ok(Some(shared.mem.heap.get_field(wrapper, 0)))
            } else {
                Ok(Some(value))
            }
        }
        _ => Ok(Some(value)),
    }
}

// ---------------------------------------------------------------------------
// G24-1 — the InvocationHandler return contract on the LIVE proxy path
// ---------------------------------------------------------------------------
//
// HotSpot's generated `$ProxyN` body does not merely *read* the handler's
// `Object` result — it emits a `checkcast` to the declared return type and, for
// a primitive return, a `checkcast` to the wrapper followed by the wrapper's
// `xxxValue()` call. Both bytecodes can fail, and the two failures are the
// whole of `RJdkProxy` checks 32 and 33:
//
//   * `null` for a primitive return -> `NullPointerException`, whose text is
//     the helpful-NPE for the `xxxValue()` call site and so names a DIFFERENT
//     wrapper per return type (G24-1 §2);
//   * any object that is not exactly the declared type -> `ClassCastException`.
//     There is **no widening**: an `Integer` returned for a `long`-declared
//     method is a CCE on HotSpot, not a conversion (G24-1 §3, MEASURED — the
//     row a careful reading of the JDK gets wrong).
//
// `proxy_unbox_primitive_return` above stays as it was, deliberately: it is the
// ANNOTATION arm's coercion, where the values are the VM's own recorded member
// data rather than something a user handler chose, and refusing one of those
// would turn a CratonVM bookkeeping gap into a thrown exception on a path
// `RJdkStrict` (359 checks) drives hard. The strict contract below is applied
// one level in — at the three points inside `proxy_invoke_handler{,_shared}`
// where the USER's `InvocationHandler` result comes back — so the annotation
// arm, which returns earlier, never reaches it.

/// The wrapper class and the unboxing accessor HotSpot's generated `$ProxyN`
/// body calls for each primitive return descriptor.
///
/// The accessor name is not decoration: it is quoted verbatim in the NPE
/// message, and it is the half that varies per type. TRANSCRIBED from the
/// oracle (`PRet` probe, HotSpot 25.0.3+9-LTS), not composed from a template.
pub(in crate::vm::vm_exec) fn proxy_primitive_return_wrapper(ret: char) -> Option<(&'static str, &'static str)> {
    Some(match ret {
        'Z' => ("java/lang/Boolean", "booleanValue"),
        'B' => ("java/lang/Byte", "byteValue"),
        'C' => ("java/lang/Character", "charValue"),
        'S' => ("java/lang/Short", "shortValue"),
        'I' => ("java/lang/Integer", "intValue"),
        'J' => ("java/lang/Long", "longValue"),
        'F' => ("java/lang/Float", "floatValue"),
        'D' => ("java/lang/Double", "doubleValue"),
        _ => return None,
    })
}

/// The return descriptor of `descriptor` — everything after the last `)`.
///
/// Spelled out rather than reusing the `rsplit(')')` idiom above because that
/// one answers the WHOLE string for a descriptor with no `)` at all, which
/// would make `proxy_coerce_handler_return` read a malformed descriptor's first
/// character as a return kind.
pub(in crate::vm::vm_exec) fn proxy_return_descriptor(descriptor: &str) -> &str {
    match descriptor.rfind(')') {
        Some(idx) => &descriptor[idx + 1..],
        None => "",
    }
}

/// HotSpot's `Klass::external_name()` for a cast-message operand: dotted, with
/// arrays left in descriptor form. The `(… are in module …)` parenthetical is
/// appended later by `runtime::exceptions::throw_runtime_error`, the single
/// funnel every VM-raised `RuntimeError` passes through — so the message built
/// here is deliberately the bare two-operand form that funnel recognises.
pub(in crate::vm::vm_exec) fn proxy_cast_display_name(internal: &str) -> String {
    crate::runtime::exceptions::hotspot_external_name(&internal)
}

/// HotSpot's helpful-NPE for the `xxxValue()` call the generated `$ProxyN` body
/// makes on a `null` handler result.
///
/// A separate function only so the text can be asserted without a VM: it is
/// TRANSCRIBED, and it is the half of this change that no amount of reading the
/// JDK would produce. The `because` clause is invariant — the receiver is
/// always the handler's return value — while the `Cannot invoke` half names the
/// wrapper and accessor for the declared return type.
pub(in crate::vm::vm_exec) fn proxy_null_return_npe_message(wrapper_internal: &str, accessor: &str) -> String {
    format!(
        "Cannot invoke \"{}.{accessor}()\" because the return value of \
         \"java.lang.reflect.InvocationHandler.invoke(Object, \
         java.lang.reflect.Method, Object[])\" is null",
        proxy_cast_display_name(wrapper_internal)
    )
}

/// The bare two-operand cast refusal `runtime::exceptions::throw_runtime_error`
/// recognises and decorates with the module/loader parenthetical.
pub(in crate::vm::vm_exec) fn proxy_cast_refusal_message(from_internal: &str, to_internal: &str) -> String {
    format!(
        "class {} cannot be cast to class {}",
        proxy_cast_display_name(from_internal),
        proxy_cast_display_name(to_internal)
    )
}

/// Can this VM state, with confidence, what `obj`'s type is *not*?
///
/// `true` means "no opinion" and the caller must fail open. The strict contract
/// can only ever REFUSE, so every uncertainty has to answer `true` here: a
/// wrong refusal turns a working proxy into a thrown `ClassCastException`,
/// which is strictly worse than the missing refusal it replaces.
///
/// Four populations are opaque:
///
/// * **Arrays.** `class_id_of` on an array header carries the COMPONENT class
///   (see the `Thread.clone` H2-CID0 note in `interpreter/invoke.rs`), so a
///   `String[]` would answer `is_assignable_to_name("java/lang/String")` —
///   true, for the wrong reason. Two measured rows (`String[]` -> `int[]`,
///   `int[]` -> `Object[]`) are left diverging by this and nothing asserts
///   them.
/// * **Lambda proxies.** Their `ClassId` is synthetic and absent from the class
///   store, so their SAM interface is on no `interfaces` list to walk. Costs
///   the measured `lambda` -> `String` row.
/// * **Anything on a proxy superclass chain** (`Proxy$Instance` or the real
///   `java/lang/reflect/Proxy`). A fabricated `$ProxyN` records
///   `GeneratedProxy { interfaces: [] }` — "no record", not "implements
///   nothing" — the same distinction `recorded_proxy_interface_set` in
///   `interpreter/typecheck.rs` exists to make. A user class that merely
///   extends `java.lang.reflect.Proxy` is NOT opaque: its declared hierarchy
///   is its whole type (round 13 wave 7, lane predicates;
///   `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0` treats it as opaque again).
/// * **A class the class manager cannot produce at all.**
pub(in crate::vm::vm_exec) fn proxy_return_type_is_opaque(shared: &SharedVm, obj: ObjectRef) -> bool {
    if shared.mem.heap.kind_of(obj) == ObjectKind::Array {
        return true;
    }
    let class_id = shared.mem.heap.class_id_of(obj);
    if shared.classes.lambda_proxies.read().contains_key(&class_id) {
        return true;
    }
    if crate::runtime::interpreter::class_is_generated_proxy_or_shim(shared, class_id) {
        return true;
    }
    shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_none()
}

/// The runtime class name to quote in a cast refusal, or `None` when
/// [`proxy_return_type_is_opaque`] says we must not refuse at all.
pub(in crate::vm::vm_exec) fn proxy_refusable_class_name(shared: &SharedVm, obj: ObjectRef) -> Option<String> {
    if proxy_return_type_is_opaque(shared, obj) {
        return None;
    }
    let class_id = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.name.to_string())
}

/// `Some(runtime_class_name)` when `obj` must be refused for a method whose
/// declared reference return descriptor is `ret_desc`; `None` to accept.
///
/// Assignability is answered by `Class::is_assignable_to_name`, which walks the
/// superclass chain AND the interface DAG comparing NAMES — so it needs no
/// `ClassId` for the declared type and therefore triggers no class loading on
/// a dispatch path. That matters: this runs once per proxy call with a
/// reference return, and a `load_class` here would put classloading behind
/// every such dispatch.
///
/// Two gates before a refusal, both measured-neutral, both there to keep a
/// refusal from being invented out of a lookup failure:
///
/// * `Ljava/lang/Object;` — by far the most common proxy return descriptor —
///   short-circuits before any lock is taken;
/// * the declared type must itself be a class this VM has a unique definition
///   for. If it is not loaded, "not assignable" is a statement about the class
///   store rather than about the value.
pub(in crate::vm::vm_exec) fn proxy_reference_return_refusal(
    shared: &SharedVm,
    obj: ObjectRef,
    ret_desc: &str,
) -> Option<String> {
    if ret_desc == "Ljava/lang/Object;" {
        return None;
    }
    // Array return types fail open together with array values: see
    // `proxy_return_type_is_opaque`.
    let target = ret_desc.strip_prefix('L')?.strip_suffix(';')?;
    if proxy_return_type_is_opaque(shared, obj) {
        return None;
    }
    let class_id = shared.mem.heap.class_id_of(obj);
    let name = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        if class.is_assignable_to_name(target, &cm.class_store) {
            return None;
        }
        // The declared type must itself be a class this VM has a unique
        // definition for — otherwise "not assignable" is a statement about the
        // class store rather than about the value.
        cm.find_unique_class_by_name(target)?;
        class.name.to_string()
    };
    // The two admit-only heuristics `aastore_element_assignable` reaches for at
    // the same point, after its own by-name walk has already declined. Both can
    // only ADMIT, and both cover populations whose real supertypes are not on
    // any `interfaces` list: a class this VM fabricated, and an annotation
    // proxy whose annotation interface lives on the heap object rather than in
    // its class entry. Borrowed rather than reimplemented so this predicate
    // cannot drift away from the one `aastore` and `checkcast` already use.
    if crate::runtime::interpreter::synthetic_implements_public(shared, class_id, target)
        || crate::runtime::interpreter::annotation_proxy_satisfies_target(shared, obj, target)
        // Same display-class admission `op_instanceof`/`op_checkcast` and the
        // JIT's `jit_typecheck_resolve` consult (H18-1, H15-2 §5.6): `obj`'s
        // header carries the raw `cratonvm/internal/Unmodifiable*` stamp here,
        // not the resolved display class `Object.getClass()` would report, so
        // a dynamic-proxy method declared to return e.g. `AbstractMap` and
        // handed back `Map.of(...)` was refused even though HotSpot accepts
        // it. Thread-free twin: this runs on a proxy dispatch path with no
        // `&mut JvmThread` in scope, so it declines rather than loads when the
        // display class is not already resolved.
        || crate::runtime::interpreter::display_class_satisfies_name_public(
            shared, obj, class_id, target,
        )
    {
        return None;
    }
    Some(name)
}

/// Apply the `InvocationHandler` return contract to the value a USER handler
/// produced, exactly as HotSpot's generated `$ProxyN` body would.
///
/// A raw (already-unboxed) `Value` passes through untouched on every arm. That
/// is not laxity — it is the re-entrancy valve. A proxy whose handler
/// reflectively re-invokes through a NESTED proxy gets that inner dispatch's
/// result back already coerced here, and `Method.invoke` re-boxes it per the
/// inner method's own descriptor, so the outer coercion sees a wrapper again.
/// A raw value at this boundary can therefore only be the VM's own, and
/// refusing it would refuse a value no Java code ever chose.
pub(crate) fn proxy_coerce_handler_return(
    shared: &SharedVm,
    descriptor: &str,
    result: MethodCallResult,
) -> MethodCallResult {
    let Some(value) = result? else {
        return Ok(None);
    };
    let ret_desc = proxy_return_descriptor(descriptor);
    let ret = ret_desc.chars().next().unwrap_or('L');

    if let Some((wrapper, accessor)) = proxy_primitive_return_wrapper(ret) {
        return match value {
            // The `because` clause is fixed text — the generated body's
            // receiver is always the handler's return value; the `Cannot
            // invoke` half names the wrapper for THIS return type.
            Value::Object(None) => Err(RuntimeError::NullPointerException {
                message: Some(proxy_null_return_npe_message(wrapper, accessor)),
            }
            .into()),
            Value::Object(Some(obj)) => {
                if class_name_is(shared, obj, wrapper) {
                    return Ok(Some(shared.mem.heap.get_field(obj, 0)));
                }
                match proxy_refusable_class_name(shared, obj) {
                    // The wrapper checkcast. NOT a widening site: an `Integer`
                    // for a `J` return refuses here rather than converting.
                    Some(actual) => Err(RuntimeError::ClassCastException {
                        message: proxy_cast_refusal_message(&actual, wrapper),
                    }
                    .into()),
                    // Opaque: keep the pre-G24 read of slot 0 verbatim.
                    None => Ok(Some(shared.mem.heap.get_field(obj, 0))),
                }
            }
            other => Ok(Some(other)),
        };
    }

    if ret == 'V' {
        return Ok(Some(value));
    }
    if let Value::Object(Some(obj)) = value {
        if let Some(actual) = proxy_reference_return_refusal(shared, obj, ret_desc) {
            let target = ret_desc
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                .unwrap_or(ret_desc);
            return Err(RuntimeError::ClassCastException {
                message: proxy_cast_refusal_message(&actual, target),
            }
            .into());
        }
    }
    Ok(Some(value))
}

/// Dispatch a real JDK `$ProxyN` annotation method through its synthetic
/// AnnotationProxy handler. `equals` needs the other real proxy unwrapped so
/// annotation equality compares its members rather than proxy identity.
pub(in crate::vm::vm_exec) fn proxy_annotation_handler_invoke(
    shared: &SharedVm,
    thread: &mut JvmThread,
    handler_ref: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    if method_name == "equals" {
        if let Some(Value::Object(Some(other))) = args.first().copied() {
            if let Value::Object(Some(other_handler)) = shared.mem.heap.get_field(other, 0) {
                if class_name_is(
                    shared,
                    other_handler,
                    "java/lang/annotation/AnnotationProxy",
                ) {
                    let routed = [Value::Object(Some(other_handler))];
                    if cratonvm_native_builtins::lang_class::annotation_jdk_semantics() {
                        return annotation_proxy_invoke_shared(
                            shared,
                            thread,
                            handler_ref,
                            method_name,
                            &routed,
                        );
                    }
                    return annotation_proxy_dispatch_impl(
                        shared,
                        handler_ref,
                        method_name,
                        &routed,
                    );
                }
            }
        }
    }
    // Fall through to the SHARED entry point, not `annotation_proxy_dispatch_impl`:
    // only the shared one delegates `equals` to a FOREIGN annotation proxy of
    // the same type (e.g. Spring's `MergedAnnotation.synthesize()` proxy, whose
    // handler is Spring's own, not an `AnnotationProxy`). Without it
    // `realAnnotation.equals(synthesized)` answered false while
    // `synthesized.equals(realAnnotation)` answered true -- asymmetric equality,
    // surfacing as `core.annotation.MergedAnnotationsTests
    // .equalsForSynthesizedAnnotations`.
    annotation_proxy_invoke_shared(shared, thread, handler_ref, method_name, args)
}

pub(crate) fn proxy_invoke_handler_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Get the InvocationHandler from proxy field 0
    let handler_ref = match shared.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(h)) => h,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    // The handler runs in a nested call and its result is coerced to the
    // proxy method's return type after it returns (`proxy_coerce_handler_return`,
    // the caller's unbox). A virtual thread whose handler parks must not
    // unmount here: the remount would hand the caller the handler's `Object`
    // where an `int` (or nothing) is due. Pinned for the call, as HotSpot pins
    // a continuation with a native frame on it (interpreter round i1 wave 26,
    // lane L4; `native_callee_memo::pin_continuation`).
    let _continuation_pin = matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        .then(crate::runtime::native_callee_memo::pin_continuation);

    // Real-annotation-proxy path (CRATONVM_REAL_ANNOTATIONS): an annotation
    // instance is a real `$ProxyN` proxy whose InvocationHandler is the
    // synthetic `AnnotationProxy` carrying the member data. Route its method
    // calls straight to the existing annotation dispatch so every annotation
    // semantic (value()/annotationType()/equals/hashCode/toString) is reused
    // unchanged. getClass() is intercepted earlier (the proxy hook returns the
    // real `$ProxyN` mirror), so it never reaches here.
    if class_name_is(shared, handler_ref, "java/lang/annotation/AnnotationProxy") {
        return proxy_unbox_primitive_return(
            shared,
            descriptor,
            proxy_annotation_handler_invoke(shared, thread, handler_ref, method_name, args),
        );
    }

    // Parse descriptor into per-parameter and return type descriptors —
    // needed either way below (arg-boxing uses `param_descs` regardless of
    // whether the Method object itself is served from cache).
    let (param_descs, ret_desc) = proxy_split_descriptor(descriptor);

    // Proxy-dispatch Method cache (see `proxy_method_cache`'s doc comment in
    // `class_realm.rs`): real JDK dynamic-proxy classes build this Method
    // object ONCE per interface method in their static initializer, not per
    // call. Before this cache, CratonVM rebuilt a fresh Method + two arrays +
    // two strings on every single reflective dispatch — on
    // allocation-heavy, reflection-driven workloads (ByteBuddy's
    // `JavaDispatcher.INVOKER` in particular) that generated enough
    // short-lived garbage to fragment the non-compacting old-gen arena and
    // OOM even with most of the heap nominally free.
    let proxy_class_id = shared.mem.heap.class_id_of(proxy);
    let cache_key = (
        proxy_class_id,
        method_name.to_string(),
        descriptor.to_string(),
    );
    let cached_method_obj = shared
        .classes
        .proxy_method_cache
        .read()
        .get(&cache_key)
        .copied();
    // `--real-jdk`, cache miss only: resolve the signature's classes through the
    // declaring interface's loader (it runs Java, so the values kept across it
    // are pinned and re-read). See `proxy_signature_class_ids`.
    let resolved = if cached_method_obj.is_none() {
        proxy_signature_class_ids_pinned(shared, thread, proxy, method_name, descriptor, args)
    } else {
        None
    };
    let (sig_ids, proxy, args_refreshed) = match resolved {
        Some((ids, proxy, fresh)) => (ids, proxy, Some(fresh)),
        None => (Vec::new(), proxy, None),
    };
    let args: &[Value] = args_refreshed.as_deref().unwrap_or(args);
    let handler_ref = match shared.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(h)) => h,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    let sig_at = |i: usize| sig_ids.get(i).copied().flatten();
    let method_obj = match cached_method_obj {
        Some(obj) => obj,
        None => {
            // WP2.5 вЂ” build the Method object using **field-name-based** writes.
            // See `proxy_method_set_field_by_name` for the rationale; mirrors the
            // fix applied to `proxy_invoke_handler` above.
            let method_class_id = shared
                .classes
                .class_manager_write_settling()
                .load_class("java/lang/reflect/Method")
                .unwrap_or(ClassId::new(0));
            let total_fields = shared
                .classes
                .class_manager
                .read()
                .get_class(method_class_id)
                .map(|c| c.first_field_index + c.fields.iter().filter(|f| !f.is_static()).count())
                .unwrap_or(8);
            // S111r11 — see `proxy_invoke_handler` above.
            const METHOD_EXTRA_SLOTS: usize = 3;
            const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;
            let alloc_base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, total_fields);
            let method_obj = shared
                .mem
                .heap
                .try_alloc_object_full(method_class_id, alloc_base + METHOD_EXTRA_SLOTS)
                .ok_or_else(|| proxy_heap_exhausted("Method"))?;
            // gch h2/fj: the mirrors on the unwinding arm, as in
            // `proxy_invoke_handler`.
            let zero_mirror = proxy_mirror_or_oome(|| {
                super::get_or_create_class_mirror(shared, ClassId::new(0))
            })?;
            // Resolve the actual declaring-interface mirror for the synthesized
            // Method's `clazz` field — see `proxy_resolve_declaring_class_mirror`
            // for why `ClassId(0)` (== `Object` in production) is unsafe here.
            let declaring_mirror = proxy_mirror_or_oome(|| {
                proxy_resolve_declaring_class_mirror(shared, proxy, method_name, descriptor)
            })?;
            let name_str = super::try_create_java_string(shared, method_name)
                .ok_or_else(|| proxy_heap_exhausted("Method.name"))?;
            let return_type_mirror = proxy_signature_mirror(shared, sig_at(0), &ret_desc)?;
            let param_count = param_descs.len();
            // `Class[]` -- see `proxy_invoke_handler`.
            let param_arr = shared
                .mem
                .heap
                .try_alloc_array_full(
                    proxy_class_array_component(shared),
                    crate::memory::heap::ArrayElementType::Reference,
                    param_count,
                )
                .ok_or_else(|| proxy_heap_exhausted("Method.parameterTypes"))?;
            for (i, pdesc) in param_descs.iter().enumerate() {
                let pmirror = proxy_signature_mirror(shared, sig_at(i + 1), pdesc)?;
                shared
                    .mem
                    .heap
                    .set_array_element(param_arr, i, Value::Object(Some(pmirror)))
                    .ok();
            }
            let desc_str = super::try_create_java_string(shared, descriptor)
                .ok_or_else(|| proxy_heap_exhausted("Method.descriptor"))?;
            proxy_method_set_field_by_name(
                shared,
                method_obj,
                "clazz",
                Value::Object(Some(declaring_mirror)),
            );
            proxy_method_set_field_by_name(
                shared,
                method_obj,
                "name",
                Value::Object(Some(name_str)),
            );
            proxy_method_set_field_by_name(
                shared,
                method_obj,
                "returnType",
                Value::Object(Some(return_type_mirror)),
            );
            proxy_method_set_field_by_name(
                shared,
                method_obj,
                "parameterTypes",
                Value::Object(Some(param_arr)),
            );
            // Mirror the NativeContext dispatch path: InvocationHandler.invoke() must
            // observe a non-null, accurately populated Method.exceptionTypes array.
            let exception_arr =
                proxy_method_exception_types(shared, declaring_mirror, method_name, descriptor)
                    .ok_or_else(|| proxy_heap_exhausted("Method.exceptionTypes"))?;
            proxy_method_set_field_by_name(
                shared,
                method_obj,
                "exceptionTypes",
                Value::Object(Some(exception_arr)),
            );
            let method_modifiers =
                proxy_resolve_declaring_method_modifiers(shared, proxy, method_name, descriptor);
            proxy_method_set_field_by_name(
                shared,
                method_obj,
                "modifiers",
                Value::Int(method_modifiers),
            );
            proxy_method_set_field_by_name(
                shared,
                method_obj,
                "signature",
                Value::Object(Some(desc_str)),
            );
            proxy_method_set_field_by_name(shared, method_obj, "slot", Value::Int(0));
            // S111r11 — see `proxy_invoke_handler` above for rationale.
            proxy_method_write_extra_slots(shared, method_obj, descriptor, param_count);
            if total_fields < 8 {
                // Synthetic-mode fallback (no JDK Method class loaded): keep the
                // old hard-coded layout so callers reading raw slots still find
                // the values.
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 0, Value::Object(Some(declaring_mirror)));
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 1, Value::Object(Some(name_str)));
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 2, Value::Object(Some(return_type_mirror)));
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 3, Value::Object(Some(param_arr)));
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 4, Value::Int(method_modifiers));
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 5, Value::Object(Some(desc_str)));
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 6, Value::Int(param_count as i32));
                shared
                    .mem
                    .heap
                    .set_field(method_obj, 9, Value::Object(Some(exception_arr)));
            }
            // Silence "zero_mirror unused" — kept above to preserve the original
            // allocation flow.
            let _ = zero_mirror;
            shared
                .classes
                .proxy_method_cache
                .write()
                .insert(cache_key, method_obj);
            method_obj
        }
    };

    // Build Object[] of args вЂ” box primitives. Per
    // `java.lang.reflect.InvocationHandler.invoke` contract, when the
    // intercepted method takes no arguments, `args` MUST be `null` (not an
    // empty Object[]). See `proxy_invoke_handler` above for the SportMe /
    // Spring `SerializableTypeWrapper` case that depends on this.
    let args_value = if args.is_empty() {
        Value::Object(None)
    } else {
        let args_arr = shared
            .mem
            .heap
            .try_alloc_array_full(
                ClassId::new(0),
                crate::memory::heap::ArrayElementType::Reference,
                args.len(),
            )
            .ok_or_else(|| proxy_heap_exhausted("argument array"))?;
        for (i, arg) in args.iter().enumerate() {
            // Box per the formal parameter descriptor so a boolean/char/byte/
            // short arg (all `Value::Int`) becomes Boolean/Character/Byte/Short
            // — not Integer — matching the proxied method's signature.
            let boxed = match param_descs.get(i) {
                Some(pdesc) => proxy_box_value_for_desc(shared, *arg, pdesc),
                None => proxy_box_value(shared, *arg),
            }
            .ok_or_else(|| proxy_heap_exhausted("argument box"))?;
            shared.mem.heap.set_array_element(args_arr, i, boxed).ok();
        }
        Value::Object(Some(args_arr))
    };

    // Call InvocationHandler.invoke(Object proxy, Method method, Object[] args)
    // Resolve the handler's actual class for dispatch (it may be an anonymous class
    // implementing InvocationHandler, so we can't use the interface name directly).
    let handler_class_id = shared.mem.heap.class_id_of(handler_ref);

    // WP2.5: if the handler is a synthetic lambda proxy (e.g. the user
    // passed `(proxy, m, a) -> ...` directly to `Proxy.newProxyInstance`),
    // its class_id will be a high synthetic id that isn't in the class
    // store. Dispatching by class name in that case would fall back to
    // `java/lang/reflect/InvocationHandler.invoke` (the abstract
    // interface method, which has no Code attribute) and crash with
    // "no Code attribute". Route through `try_lambda_dispatch` which
    // resolves the lambda's `impl_handle` and runs the SAM body.
    let handler_is_lambda = shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&handler_class_id);
    if handler_is_lambda {
        let call_args = [
            Value::Object(Some(proxy)),
            Value::Object(Some(method_obj)),
            args_value,
        ];
        // Note: a thrown exception here must route through the
        // UndeclaredThrowableException parity wrap (lambda handlers — e.g. a
        // `(proxy,m,a) -> ...` passed straight to `newProxyInstance` — are the
        // common case), so capture the Err instead of `?`-propagating it.
        //
        // gen r4w3/rooting: the handler body is a GC point and
        // `proxy_wrap_undeclared_if_needed` reads `proxy`'s fields afterwards;
        // pin it across the dispatch and hand the wrap the current address.
        let proxy_pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(proxy);
        let dispatch_res = crate::runtime::interpreter::try_lambda_dispatch(
            shared,
            thread,
            handler_ref,
            handler_class_id,
            "invoke",
            "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
            &call_args,
        );
        let proxy = thread.native_pin_roots[proxy_pin];
        thread.native_pin_roots.truncate(proxy_pin);
        let dispatch = match dispatch_res {
            Ok(d) => d,
            Err(failure) => {
                return proxy_wrap_undeclared_if_needed(
                    shared,
                    thread,
                    proxy,
                    method_name,
                    descriptor,
                    Err(failure),
                );
            }
        };
        if let Some(result) = dispatch {
            // G24-1: a `(proxy, m, a) -> …` handler passed straight to
            // `newProxyInstance` is the common shape — it is what `RJdkProxy`
            // uses — and its result is a user value, so it takes the same
            // strict contract as the object-handler tail below. This arm
            // returned verbatim before, leaving the coercion entirely to the
            // caller in `interpreter/invoke.rs`, whose `else { value }` arm is
            // what pushed a handler's `null` as `0`.
            return proxy_coerce_handler_return(shared, descriptor, Ok(result));
        }
        // Fell through unexpectedly вЂ” surface as a clearer error than
        // "no Code attribute".
        return Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::AbstractMethodError {
                class_name: format!("<lambda#{}>", handler_class_id),
                method_name: "invoke".to_string(),
            },
        )));
    }

    let handler_class_name = shared
        .classes
        .class_manager
        .read()
        .get_class(handler_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| "java/lang/reflect/InvocationHandler".to_string());

    let invoke_args = [
        Value::Object(Some(handler_ref)),
        Value::Object(Some(proxy)),
        Value::Object(Some(method_obj)),
        args_value,
    ];
    // gen r4w3/rooting: pin `proxy` across the handler body (a GC point) —
    // `proxy_wrap_undeclared_if_needed` reads its fields afterwards.
    let proxy_pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(proxy);
    let result = invoke_or_native(
        shared,
        thread,
        &handler_class_name,
        "invoke",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        &invoke_args,
    );
    let proxy = thread.native_pin_roots[proxy_pin];
    thread.native_pin_roots.truncate(proxy_pin);
    // G24-1: the object-handler tail. Everything reaching here came out of a
    // real `InvocationHandler.invoke` body, so the return contract applies —
    // `null` for a primitive return is an NPE and a value that is not the
    // declared type is a CCE, with no widening. The AnnotationProxy arm
    // returned long before this point, which is what keeps annotation member
    // data (and `RJdkStrict`) out of the blast radius; the exception wrap
    // stays innermost so an undeclared checked throw is still rewrapped before
    // the coercion sees `Err` and passes it straight through.
    proxy_coerce_handler_return(
        shared,
        descriptor,
        proxy_wrap_undeclared_if_needed(shared, thread, proxy, method_name, descriptor, result),
    )
}

/// proxy-real-classfile increment 6 — `UndeclaredThrowableException` parity for
/// the live proxy dispatch hooks (`proxy_invoke_handler{,_shared}`).
///
/// HotSpot's generated `$ProxyN` method body wraps any exception thrown by
/// `InvocationHandler.invoke` that is neither a `RuntimeException` / `Error` nor
/// a checked type **declared** by the proxied method in
/// `java.lang.reflect.UndeclaredThrowableException` (the `java.lang.reflect.Proxy`
/// contract / JLS §15.12.4.4). CratonVM's live dispatch hooks returned the
/// handler result verbatim, so an *undeclared checked* exception surfaced
/// unwrapped — a HotSpot divergence. (It was only ever masked because the
/// dead-code generated-body path that already wraps,
/// `native_builtins::native_proxy_dispatch_invoke`, is superseded by these
/// interpreter hooks.) This brings the live path to parity.
///
/// Pass-through for `Ok`, for a non-`ExceptionThrown` `Err`, for
/// `RuntimeException` / `Error`, and for any declared checked exception — only an
/// undeclared checked throw is rewrapped. Best-effort: if the
/// `UndeclaredThrowableException` class can't be resolved / constructed the
/// original exception propagates verbatim (losing the wrap beats losing the
/// exception).
pub(in crate::vm::vm_exec) fn proxy_wrap_undeclared_if_needed(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
    result: MethodCallResult,
) -> MethodCallResult {
    let thrown = match result {
        Err(MethodCallFailed::ExceptionThrown(t)) => t,
        other => return other,
    };
    let thrown_cid = shared.mem.heap.class_id_of(thrown);

    // RuntimeException / Error (or subclasses) propagate verbatim.
    for base in ["java/lang/RuntimeException", "java/lang/Error"] {
        if let Ok(bcid) = shared.load_class_concurrent(base) {
            if thrown_cid == bcid
                || shared
                    .classes
                    .class_manager
                    .read()
                    .is_subclass_of(thrown_cid, bcid)
            {
                return Err(MethodCallFailed::ExceptionThrown(thrown));
            }
        }
    }

    // A checked exception declared by the proxied method propagates verbatim.
    // `proxy_resolve_declaring_class_mirror` lands on the exact declaring
    // interface, so its `Exceptions` attribute is authoritative for (name,desc).
    // gch h2/fj: on the unwinding arm; a full heap keeps the original
    // exception, this function's best-effort rule.
    let Ok(decl_mirror) = proxy_mirror_or_oome(|| {
        proxy_resolve_declaring_class_mirror(shared, proxy, method_name, descriptor)
    }) else {
        return Err(MethodCallFailed::ExceptionThrown(thrown));
    };
    let decl_cid = shared
        .classes
        .class_mirrors_reverse
        .read()
        .get(&decl_mirror)
        .copied();
    if let Some(decl_cid) = decl_cid {
        for ex_name in proxy_method_declared_exceptions(shared, decl_cid, method_name, descriptor) {
            if let Ok(ex_cid) = shared.load_class_concurrent(&ex_name) {
                if thrown_cid == ex_cid
                    || shared
                        .classes
                        .class_manager
                        .read()
                        .is_subclass_of(thrown_cid, ex_cid)
                {
                    return Err(MethodCallFailed::ExceptionThrown(thrown));
                }
            }
        }
    }

    // Undeclared checked exception → wrap in UndeclaredThrowableException.
    let ute_name = "java/lang/reflect/UndeclaredThrowableException";
    let ute_cid = match shared.load_class_concurrent(ute_name) {
        Ok(c) => c,
        Err(_) => return Err(MethodCallFailed::ExceptionThrown(thrown)),
    };
    // gen r4w3/rooting: <clinit> and the `<init>` upcall below are GC points;
    // keep `thrown` (and then `ute`) pinned and re-read them after each.
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(thrown);
    let init_failed = super::ensure_class_initialized_shared(shared, thread, ute_cid).is_err();
    let thrown = thread.native_pin_roots[pin_base];
    if init_failed {
        thread.native_pin_roots.truncate(pin_base);
        return Err(MethodCallFailed::ExceptionThrown(thrown));
    }
    let n_fields = shared
        .classes
        .class_manager
        .read()
        .get_class(ute_cid)
        .map(|c| c.num_total_fields)
        .unwrap_or(8)
        .max(4);
    // Fallible (gc-common w5-c): a heap too full for the wrapper propagates the
    // ORIGINAL throwable, as the class-load and constructor failures do. It was
    // the infallible `alloc_object`, which aborts on exhaustion; see
    // `proxy_heap_exhausted`.
    let Some(ute) = shared.mem.heap.try_alloc_object_full(ute_cid, n_fields) else {
        thread.native_pin_roots.truncate(pin_base);
        return Err(MethodCallFailed::ExceptionThrown(thrown));
    };
    thread.native_pin_roots.push(ute);
    let ctor_args = [Value::Object(Some(ute)), Value::Object(Some(thrown))];
    // Run the real `<init>(Throwable)` so `getUndeclaredThrowable()` /
    // `getCause()` observe the wrapped exception. On ctor failure fall back to
    // verbatim propagation.
    let ctor_failed = invoke_or_native(
        shared,
        thread,
        ute_name,
        "<init>",
        "(Ljava/lang/Throwable;)V",
        &ctor_args,
    )
    .is_err();
    let thrown = thread.native_pin_roots[pin_base];
    let ute = thread.native_pin_roots[pin_base + 1];
    thread.native_pin_roots.truncate(pin_base);
    if ctor_failed {
        return Err(MethodCallFailed::ExceptionThrown(thrown));
    }
    Err(MethodCallFailed::ExceptionThrown(ute))
}

/// Declared `throws` types (internal names) of `(method_name, descriptor)` on
/// `class_id`, read from the method's `Exceptions` attribute. The proxy
/// declaring-class resolver already lands on the exact interface that declares
/// the method, so no hierarchy walk is needed here.
pub(in crate::vm::vm_exec) fn proxy_method_declared_exceptions(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Vec<String> {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return Vec::new();
    };
    for m in &class.methods {
        if &*m.name == method_name && &*m.descriptor == descriptor {
            for attr in &m.attributes {
                if let Some(cratonvm_reader::attribute::Attribute::Exceptions {
                    exception_indices,
                }) = attr.as_decoded()
                {
                    return exception_indices
                        .iter()
                        .filter_map(|idx| {
                            class
                                .constant_pool
                                .get_class_name(*idx)
                                .map(|s| s.to_string())
                        })
                        .collect();
                }
            }
            return Vec::new();
        }
    }
    Vec::new()
}

/// Build the `Class[]` stored in the synthetic `Method.exceptionTypes` field
/// supplied to an `InvocationHandler`. Unlike the backing object's default
/// null, this is always an array, including when the proxied method has no
/// `Exceptions` attribute. The declaring interface is already resolved by the
/// proxy dispatch path, so its classfile attribute is authoritative.
///
/// `None` when the heap cannot hold the array (gc-common w5-c; the callers
/// report `OutOfMemoryError`, see `proxy_heap_exhausted`).
/// The component `ClassId` of the `Class[]` arrays a synthesized proxy
/// `Method` carries (`parameterTypes`, `exceptionTypes`). `Object` only if
/// `java/lang/Class` cannot be loaded, which does not happen after boot.
pub(crate) fn proxy_class_array_component(shared: &SharedVm) -> ClassId {
    shared
        .classes
        .class_manager_write_settling()
        .load_class("java/lang/Class")
        .unwrap_or(ClassId::new(0))
}

pub(in crate::vm::vm_exec) fn proxy_method_exception_types(
    shared: &SharedVm,
    declaring_mirror: ObjectRef,
    method_name: &str,
    descriptor: &str,
) -> Option<ObjectRef> {
    let class_component = proxy_class_array_component(shared);
    let declaring_class = shared
        .classes
        .class_mirrors_reverse
        .read()
        .get(&declaring_mirror)
        .copied();
    let declared = declaring_class
        .map(|class_id| proxy_method_declared_exceptions(shared, class_id, method_name, descriptor))
        .unwrap_or_default();
    let exception_arr = shared.mem.heap.try_alloc_array_full(
        class_component,
        crate::memory::heap::ArrayElementType::Reference,
        declared.len(),
    )?;
    for (index, exception_name) in declared.iter().enumerate() {
        let exception_mirror =
            proxy_descriptor_to_class_mirror(shared, &format!("L{exception_name};"));
        let _ = shared.mem.heap.set_array_element(
            exception_arr,
            index,
            Value::Object(Some(exception_mirror)),
        );
    }
    Some(exception_arr)
}

/// Lock-free hint for "is this the AnnotationProxy class id?" in `shared`:
/// the VM's cached id of the synthetic `java/lang/annotation/AnnotationProxy`
/// class (`ClassRealm::annotation_proxy_cid`), or `u32::MAX` (never a real
/// class id) before it is warmed. Read on the hot JIT MIC path
/// (`jit_invoke_virtual_mic`) for an identity check. Relaxed ordering: a stale
/// read at worst misses the fast path once and falls through to the (correct)
/// slow path, which re-warms it.
///
/// Per VM since interpreter round i1 wave 23, lane L5: this was a PROCESS
/// static (`ANNOTATION_PROXY_CID`) beside the realm's own copy, and a
/// `ClassId` names a different class in every VM (per-vm-state.md, Fact 1),
/// so once VM A had warmed it, VM B's compiled virtual calls on a receiver of
/// whatever class VM B numbered the same were routed into the annotation-proxy
/// dispatch.
#[inline]
pub(crate) fn annotation_proxy_cid_hint(shared: &SharedVm) -> u32 {
    shared
        .classes
        .annotation_proxy_cid
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Warm `shared`'s [`annotation_proxy_cid_hint`] once the AnnotationProxy
/// class id is known.
#[inline]
pub(crate) fn note_annotation_proxy_cid(shared: &SharedVm, cid: u32) {
    shared
        .classes
        .annotation_proxy_cid
        .store(cid, std::sync::atomic::Ordering::Relaxed);
}

/// Shared-interpreter version of `annotation_proxy_invoke`.

pub(crate) fn annotation_proxy_invoke_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    // S111r19 — Spring's MergedAnnotation.adaptValueForMapOptions iterates
    // a `[LMergedAnnotation;` array and calls `aa[i].asMap(factory, adapts)`.
    // When `aa[i]` is one of our `AnnotationProxy` instances (because the
    // upstream `adaptForAttribute` retained the original proxy array
    // instead of building a fresh `[LMergedAnnotation;`), the dispatch
    // routes here. Without an `asMap` handler, the element-accessor walk
    // returns null and Spring stores null in the resulting
    // `AnnotationAttributes[]`, surfacing as the `TypeFilterUtils.java:77`
    // / `ComponentScanAnnotationParser.java:137` NPE on the next iteration
    // (`@Filter` attribute is null).
    if method_name == "asMap" {
        return annotation_proxy_as_map(shared, thread, proxy, args);
    }
    // Round 13 wave 6 (lane proxy4): `AnnotationInvocationHandler` semantics,
    // one implementation (`native_builtins::lang_class`) for both doors;
    // `CRATONVM_ANNOTATION_JDK_SEMANTICS=0` keeps the twin below.
    let jdk_semantics = cratonvm_native_builtins::lang_class::annotation_jdk_semantics();
    if jdk_semantics && method_name == "invoke" && args.len() == 3 {
        if let Some((real_name, inner)) = annotation_handler_invoke_target(shared, args) {
            return annotation_proxy_invoke_shared(shared, thread, proxy, &real_name, &inner);
        }
    }
    if jdk_semantics && method_name == "toString" && args.is_empty() {
        let text = {
            let mut ctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            cratonvm_native_builtins::lang_class::annotation_carrier_to_string(&mut ctx, proxy)?
        };
        let result = super::try_create_java_string_uninterned(shared, &text)
            .ok_or_else(|| proxy_heap_exhausted("annotation toString"))?;
        return Ok(Some(Value::Object(Some(result))));
    }
    if jdk_semantics && matches!(method_name, "hashCode" | "equals") {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        if let Some(result) = cratonvm_native_builtins::lang_class::annotation_carrier_object_method(
            &mut ctx,
            proxy,
            method_name,
            args,
        ) {
            return result;
        }
    }
    // Annotation equality is symmetric, but our native `AnnotationProxy` equals
    // can only introspect OUR own proxies. When the argument is a *foreign*
    // annotation of the same type — e.g. a Spring `MergedAnnotation.synthesize()`
    // JDK proxy — the pure-Rust path rejects it (wrong class) and returns false,
    // breaking `a.equals(b) == b.equals(a)`. Delegate to the foreign side's
    // `equals` (which compares member-by-member via our accessors, exactly as
    // JDK's `AnnotationInvocationHandler.equals` does). The result is a boolean,
    // so there is no GC-stale-reference hazard across the nested invocation.
    if method_name == "equals" {
        if let Some(other_val @ Value::Object(Some(other))) = args.first().copied() {
            if other != proxy
                && shared.mem.heap.kind_of(other) == crate::memory::heap::ObjectKind::Object
                && !class_name_is(shared, other, "java/lang/annotation/AnnotationProxy")
            {
                let other_cid = shared.mem.heap.class_id_of(other);
                let proxy_desc = annotation_proxy_type_descriptor(shared, proxy);
                let ann_cid = proxy_desc
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                    .and_then(|n| shared.classes.class_manager.read().get_loaded_class_id(n));
                let same_annotation_type = ann_cid.is_some_and(|ac| {
                    shared
                        .classes
                        .class_manager
                        .read()
                        .is_subclass_of(other_cid, ac)
                });
                if same_annotation_type {
                    let other_cname = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(other_cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    return invoke_or_native(
                        shared,
                        thread,
                        &other_cname,
                        "equals",
                        "(Ljava/lang/Object;)Z",
                        &[other_val, Value::Object(Some(proxy))],
                    );
                }
                // A foreign object that is not an instance of our annotation
                // type is never equal to this annotation.
                return Ok(Some(Value::Int(0)));
            }
        }
    }
    let result = annotation_proxy_dispatch_impl(shared, proxy, method_name, args)?;
    // Round 13 wave 8 (lane proxy5): a member with neither a value nor a
    // default throws `IncompleteAnnotationException` (it read `null`). Asked
    // only on a `null` read; `CRATONVM_ANNOTATION_MEMBER_CONFORMANCE=0` keeps it.
    if args.is_empty() && matches!(result, Some(Value::Object(None))) {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        if let Some(thrown) = cratonvm_native_builtins::lang_class::annotation_incomplete_member(
            &mut ctx,
            proxy,
            method_name,
        ) {
            return Err(thrown);
        }
    }
    if jdk_semantics {
        // A member's non-empty array is a fresh clone per access, as
        // `AnnotationInvocationHandler.invoke` returns it.
        if let Some(value @ Value::Object(Some(obj))) = result {
            if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
                let mut ctx = NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                return cratonvm_native_builtins::lang_class::annotation_member_value_copy(
                    &mut ctx, value,
                );
            }
        }
    }
    Ok(result)
}

/// Implement `MergedAnnotation.asMap(Function, Adapt[])` for AnnotationProxy
/// receivers. Builds the destination map by calling `factory.apply(proxy)`
/// then populates with element name→value pairs. Nested annotation arrays
/// are converted to `AnnotationAttributes[]` arrays containing
/// recursively-asMap'd children.
pub(in crate::vm::vm_exec) fn annotation_proxy_as_map(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    // gen r4w3/rooting: `factory.apply`, the recursive asMap of nested
    // values, and every `put` run Java (moving-GC points) while `proxy`,
    // the `args` objects, `dest_map` and the name/value arrays are Rust
    // locals. Pin `proxy` and the object args here (the body pins the rest)
    // and release everything on every exit.
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(proxy);
    let arg_pins: Vec<Option<usize>> = args
        .iter()
        .map(|a| match a {
            Value::Object(Some(o)) => {
                let idx = thread.native_pin_roots.len();
                thread.native_pin_roots.push(*o);
                Some(idx)
            }
            _ => None,
        })
        .collect();
    let result = annotation_proxy_as_map_rooted(shared, thread, pin_base, args, &arg_pins);
    thread.native_pin_roots.truncate(pin_base);
    result
}

/// gen r4w3/rooting: re-read a pinned argument slice (see
/// `annotation_proxy_as_map`) into an owned Vec of current addresses.
pub(in crate::vm::vm_exec) fn current_pinned_args(roots: &[ObjectRef], orig: &[Value], pins: &[Option<usize>]) -> Vec<Value> {
    orig.iter()
        .zip(pins.iter())
        .map(|(v, p)| match p {
            Some(idx) => Value::Object(Some(roots[*idx])),
            None => *v,
        })
        .collect()
}

/// Body of [`annotation_proxy_as_map`]. `proxy` lives at
/// `thread.native_pin_roots[pin_base]` and each object entry of `orig_args`
/// at its `arg_pins` slot; the caller truncates to `pin_base` afterwards, so
/// early returns here may leave extra pins behind. Every returned object is
/// re-read from its pin.
pub(in crate::vm::vm_exec) fn annotation_proxy_as_map_rooted(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pin_base: usize,
    orig_args: &[Value],
    arg_pins: &[Option<usize>],
) -> MethodCallResult {
    use crate::memory::heap::ObjectKind;

    let proxy = thread.native_pin_roots[pin_base];
    let args = current_pinned_args(&thread.native_pin_roots, orig_args, arg_pins);
    let factory = match args.first() {
        Some(Value::Object(Some(o))) => {
            if shared.mem.heap.kind_of(*o) == ObjectKind::Object {
                Some(*o)
            } else {
                None
            }
        }
        _ => None,
    };

    let dest_map = if let Some(f) = factory {
        let f_cid = shared.mem.heap.class_id_of(f);
        let is_lambda = shared.classes.lambda_proxies.read().contains_key(&f_cid);
        let res = if is_lambda {
            let dispatch = crate::runtime::interpreter::try_lambda_dispatch(
                shared,
                thread,
                f,
                f_cid,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(proxy))],
            )?;
            dispatch.unwrap_or(None)
        } else {
            invoke_on_class_shared(
                shared,
                thread,
                f_cid,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(f)), Value::Object(Some(proxy))],
            )?
        };
        match res {
            Some(Value::Object(Some(m))) => m,
            _ => {
                let cid = shared
                    .natives
                    .native_methods
                    .first_app_workaround_class(
                        cratonvm_native_api::AppWorkaroundKind::AnnotationMapFallbackClass,
                    )
                    .and_then(|cls| shared.load_class_concurrent(cls).ok())
                    .or_else(|| shared.load_class_concurrent("java/util/LinkedHashMap").ok())
                    .unwrap_or_else(|| cratonvm_types::ClassId::new(0));
                shared
                    .mem
                    .heap
                    .try_alloc_object_full(cid, 4)
                    .ok_or_else(|| proxy_heap_exhausted("annotation attribute map"))?
            }
        }
    } else {
        let cid = shared
            .load_class_concurrent("java/util/LinkedHashMap")
            .unwrap_or_else(|_| cratonvm_types::ClassId::new(0));
        shared
            .mem
            .heap
            .try_alloc_object_full(cid, 4)
            .ok_or_else(|| proxy_heap_exhausted("annotation attribute map"))?
    };

    // S111r32 — detect Adapt.CLASS_TO_STRING in the varargs Adapt[] arg
    // (`args[1]`) so we can convert `Class` / `Class[]` element values to
    // their FQN String / String[] representation. Spring's
    // `ConfigurationWarningsApplicationContextInitializer$ComponentScanPackageCheck`
    // calls `AnnotationMetadata.getAnnotationAttributes(name, true)` (the
    // `classValuesAsString=true` overload), which routes through
    // `AnnotatedElementUtils.getMergedAnnotationAttributes` → `asMap(...,
    // CLASS_TO_STRING)`, then immediately calls
    // `attrs.getStringArray("basePackageClasses")`. If we leave the raw
    // `Class[]` in the map, `AnnotationAttributes.assertAttributeType`
    // throws `IllegalArgumentException: Attribute 'basePackageClasses' is
    // of type Class[], but String[] was expected`.
    // gen r4w3/rooting: `factory.apply` above was a GC point — pin
    // `dest_map` and re-read `proxy` / `args` before using them.
    let dest_pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(dest_map);
    let proxy = thread.native_pin_roots[pin_base];
    let args = current_pinned_args(&thread.native_pin_roots, orig_args, arg_pins);
    let class_to_string = adapt_array_contains(shared, args.get(1).copied(), "CLASS_TO_STRING");

    let names_arr = match shared.mem.heap.get_field(proxy, 2) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(Some(dest_map)))),
    };
    let values_arr = match shared.mem.heap.get_field(proxy, 3) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(Some(dest_map)))),
    };
    // gen r4w3/rooting: the name/value arrays are read on every iteration,
    // after the previous iteration's recursion and `put` (GC points).
    let arrays_pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(names_arr);
    thread.native_pin_roots.push(values_arr);
    let n = shared.mem.heap.array_length(names_arr);
    for i in 0..n {
        let names_arr = thread.native_pin_roots[arrays_pin];
        let values_arr = thread.native_pin_roots[arrays_pin + 1];
        let name_val = match shared.mem.heap.get_array_element(names_arr, i) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let elem_val = match shared.mem.heap.get_array_element(values_arr, i) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // gen r4w3/rooting: `name_val` is held across the recursive adapt.
        let iter_pin = thread.native_pin_roots.len();
        if let Value::Object(Some(o)) = name_val {
            thread.native_pin_roots.push(o);
        }
        let args = current_pinned_args(&thread.native_pin_roots, orig_args, arg_pins);
        let adapted = adapt_annotation_value_for_map(shared, thread, elem_val, &args)?;
        let name_val = match name_val {
            Value::Object(Some(_)) => Value::Object(Some(thread.native_pin_roots[iter_pin])),
            other => other,
        };
        thread.native_pin_roots.truncate(iter_pin);
        // Apply CLASS_TO_STRING: replace Class / Class[] with String / String[].
        let adapted = if class_to_string {
            try_convert_class_values_to_strings(shared, adapted)
                .ok_or_else(|| proxy_heap_exhausted("CLASS_TO_STRING"))?
        } else {
            adapted
        };
        let dest_map = thread.native_pin_roots[dest_pin];
        let dest_cid = shared.mem.heap.class_id_of(dest_map);
        let _ = invoke_on_class_shared(
            shared,
            thread,
            dest_cid,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(dest_map)), name_val, adapted],
        )?;
    }
    Ok(Some(Value::Object(Some(thread.native_pin_roots[dest_pin]))))
}

/// Test whether the given (possibly-null) Adapt[] varargs array contains
/// an enum constant whose `name` slot equals `target_name`.
pub(in crate::vm::vm_exec) fn adapt_array_contains(shared: &SharedVm, arr_val: Option<Value>, target_name: &str) -> bool {
    use crate::memory::heap::ObjectKind;
    let arr_obj = match arr_val {
        Some(Value::Object(Some(o))) => o,
        _ => return false,
    };
    if shared.mem.heap.kind_of(arr_obj) != ObjectKind::Array {
        return false;
    }
    let n = shared.mem.heap.array_length(arr_obj);
    for i in 0..n {
        let elem = match shared.mem.heap.get_array_element(arr_obj, i) {
            Ok(Value::Object(Some(o))) => o,
            _ => continue,
        };
        // Resolve Enum.name through the actual hierarchy. In real-JDK mode
        // subclasses may have fields before their inherited Enum fields, so
        // assuming slot 0 causes CLASS_TO_STRING to be silently skipped.
        let name_index = {
            let cm = shared.classes.class_manager.read();
            resolve_field_index_in_hierarchy(
                shared.mem.heap.class_id_of(elem),
                "name",
                &cm.class_store,
            )
        };
        if let Some(name_index) = name_index {
            if let Value::Object(Some(name_obj)) = shared.mem.heap.get_field(elem, name_index) {
                if let Some(s) = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_obj) {
                    if s == target_name {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// If `val` is a `Class` mirror, return a String holding its FQN (dotted).
/// If `val` is a `Class[]`, return a fresh `String[]` of FQNs. Otherwise
/// return `val` unchanged. Implements the `Adapt.CLASS_TO_STRING` semantics
/// for `MergedAnnotation.asMap` so that downstream
/// `AnnotationAttributes.getStringArray("basePackageClasses")` succeeds.
///
/// `None` when the heap cannot hold the String / String[] it builds (the
/// asMap caller reports `OutOfMemoryError`, gc-common w5-c). Every
/// allocation is collection-free
/// (`try_alloc_array_full`, `try_create_java_string`), so the source array
/// stays valid across the loop.
pub(in crate::vm::vm_exec) fn try_convert_class_values_to_strings(shared: &SharedVm, val: Value) -> Option<Value> {
    use crate::memory::heap::ObjectKind;
    let obj = match val {
        Value::Object(Some(o)) => o,
        _ => return Some(val),
    };
    let kind = shared.mem.heap.kind_of(obj);
    if kind == ObjectKind::Object {
        // Detect Class mirror: class_id resolves to java/lang/Class, or the
        // object has a non-null `name` slot we can convert. The simplest
        // detection: the class name of `obj` is "java/lang/Class".
        let cid = shared.mem.heap.class_id_of(obj);
        let class_name = shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if class_name == "java/lang/Class" {
            let fqn = class_mirror_fqn(shared, obj);
            return Some(Value::Object(Some(super::try_create_java_string(
                shared, &fqn,
            )?)));
        }
        return Some(val);
    }
    if kind == ObjectKind::Array {
        // Reference array whose component class is `java/lang/Class`.
        let comp_cid = shared.mem.heap.class_id_of(obj);
        let comp_name = shared
            .classes
            .class_manager
            .read()
            .get_class(comp_cid)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if comp_name == "java/lang/Class" {
            let n = shared.mem.heap.array_length(obj);
            let str_cid = shared
                .load_class_concurrent("java/lang/String")
                .unwrap_or_else(|_| cratonvm_types::ClassId::new(0));
            let new_arr = shared.mem.heap.try_alloc_array_full(
                str_cid,
                cratonvm_types::ArrayElementType::Reference,
                n,
            )?;
            for i in 0..n {
                let elem = shared
                    .mem
                    .heap
                    .get_array_element(obj, i)
                    .unwrap_or(Value::Object(None));
                let s_val = match elem {
                    Value::Object(Some(m)) => {
                        let fqn = class_mirror_fqn(shared, m);
                        Value::Object(Some(super::try_create_java_string(shared, &fqn)?))
                    }
                    _ => Value::Object(None),
                };
                shared.mem.heap.set_array_element(new_arr, i, s_val).ok();
            }
            return Some(Value::Object(Some(new_arr)));
        }
        return Some(val);
    }
    Some(val)
}

/// Read the FQN (dotted) name out of a Class mirror. Tries the
/// reverse-lookup map first (`class_id_from_mirror`); falls back to slot 1
/// (the JDK 25 Class layout's `name` field) for primitive / array mirrors
/// allocated via `primitive_class_mirror`.
pub(in crate::vm::vm_exec) fn class_mirror_fqn(shared: &SharedVm, mirror: ObjectRef) -> String {
    let mirror_cid = super::class_id_from_mirror(shared, mirror);
    if let Some(cid) = mirror_cid {
        if let Some(name) = shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .map(|c| c.name.to_string())
        {
            return name.replace('/', ".");
        }
    }
    if let Value::Object(Some(name_obj)) = shared.mem.heap.get_field(mirror, 1) {
        if let Some(s) = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_obj) {
            return s.replace('/', ".");
        }
    }
    String::new()
}

/// Recursively adapt an annotation element value: nested annotation
/// proxies become their asMap result; arrays of annotation proxies become
/// an `AnnotationAttributes[]`-typed array of recursively-asMap'd children.
pub(in crate::vm::vm_exec) fn adapt_annotation_value_for_map(
    shared: &SharedVm,
    thread: &mut JvmThread,
    val: Value,
    asmap_args: &[Value],
) -> Result<Value, MethodCallFailed> {
    use crate::memory::heap::ObjectKind;
    let obj = match val {
        Value::Object(Some(o)) => o,
        _ => return Ok(val),
    };
    let cid = shared.mem.heap.class_id_of(obj);
    let kind = shared.mem.heap.kind_of(obj);
    let class_name = shared
        .classes
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    if kind == ObjectKind::Object && class_name == "java/lang/annotation/AnnotationProxy" {
        return annotation_proxy_as_map(shared, thread, obj, asmap_args)
            .map(|res| res.unwrap_or(Value::Object(None)));
    }
    if kind == ObjectKind::Array {
        let len = shared.mem.heap.array_length(obj);
        let any_proxy = (0..len).any(|i| {
            matches!(
                shared.mem.heap.get_array_element(obj, i),
                Ok(Value::Object(Some(e)))
                    if shared.mem.heap.kind_of(e) == ObjectKind::Object
                        && shared
                            .classes.class_manager
                            .read()
                            .get_class(shared.mem.heap.class_id_of(e))
                            .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
                            .unwrap_or(false)
            )
        });
        if any_proxy {
            let aa_cid = shared
                .natives
                .native_methods
                .first_app_workaround_class(
                    cratonvm_native_api::AppWorkaroundKind::AnnotationMapFallbackClass,
                )
                .and_then(|cls| shared.load_class_concurrent(cls).ok())
                .unwrap_or_else(|| cratonvm_types::ClassId::new(0));
            let new_arr = shared
                .mem
                .heap
                .try_alloc_array_full(aa_cid, cratonvm_types::ArrayElementType::Reference, len)
                .ok_or_else(|| proxy_heap_exhausted("AnnotationAttributes[]"))?;
            // gen r4w3/rooting: each recursive asMap is a GC point; pin the
            // source array, the result array and the asMap args, and re-read
            // them every iteration.
            let pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(obj);
            thread.native_pin_roots.push(new_arr);
            let asmap_pins: Vec<Option<usize>> = asmap_args
                .iter()
                .map(|a| match a {
                    Value::Object(Some(o)) => {
                        let idx = thread.native_pin_roots.len();
                        thread.native_pin_roots.push(*o);
                        Some(idx)
                    }
                    _ => None,
                })
                .collect();
            for i in 0..len {
                let obj = thread.native_pin_roots[pin];
                let elem = shared
                    .mem
                    .heap
                    .get_array_element(obj, i)
                    .unwrap_or(Value::Object(None));
                let cur_args =
                    current_pinned_args(&thread.native_pin_roots, asmap_args, &asmap_pins);
                let adapted = match adapt_annotation_value_for_map(shared, thread, elem, &cur_args)
                {
                    Ok(v) => v,
                    Err(e) => {
                        thread.native_pin_roots.truncate(pin);
                        return Err(e);
                    }
                };
                let new_arr = thread.native_pin_roots[pin + 1];
                shared.mem.heap.set_array_element(new_arr, i, adapted).ok();
            }
            let new_arr = thread.native_pin_roots[pin + 1];
            thread.native_pin_roots.truncate(pin);
            return Ok(Value::Object(Some(new_arr)));
        }
    }
    Ok(val)
}

// ---------------------------------------------------------------------------
// WP2.7: shared annotation-proxy dispatch (spec-compliant)
// ---------------------------------------------------------------------------

/// Read element-name + element-value parallel arrays from an annotation proxy.
/// Returns `(name, value)` pairs in the order they were stored at proxy build
/// time.
///
/// **That order is the class file's `element_value_pairs` order, and this is
/// load-bearing** — `annotation_proxy_to_string` prints it verbatim rather
/// than sorting, because HotSpot does. Established twice, 2026-08-12:
///
/// * by reading the whole pipeline, which is `Vec`-to-`Vec` with no map or set
///   anywhere — `decode_annotation_depth` (`reader/src/attribute.rs`) pushes
///   the pairs in file order, `convert_annotation` (this file) re-pushes them
///   in that order, `create_annotation_proxy_with_type`
///   (`native-builtins/src/lang_class.rs`) clones them into `all_elements`,
///   appends any `AnnotationDefault` members AFTER them, and writes both
///   parallel arrays by `enumerate()` index. That function holds the only
///   write to `ANN_PROXY_ELEM_NAMES` in the tree;
/// * by running it. `CRATONVM_IAE_TRACE2=1` makes that builder emit one
///   `ANN-ELEM` line per element as it stores them. A use site written
///   `mike, yankee, alpha, bravo, zulu` against an interface declaring
///   `zulu, alpha, mike, bravo, yankee` traced in USE-SITE order, which is
///   what HotSpot prints for it.
///
/// Do not introduce a `HashMap` anywhere on that path.
pub(in crate::vm::vm_exec) fn annotation_proxy_elements(shared: &SharedVm, proxy: ObjectRef) -> Vec<(String, Value)> {
    let names_arr = match shared.mem.heap.get_field(proxy, 2) {
        Value::Object(Some(a)) => a,
        _ => return Vec::new(),
    };
    let values_arr = match shared.mem.heap.get_field(proxy, 3) {
        Value::Object(Some(a)) => a,
        _ => return Vec::new(),
    };
    let n = shared.mem.heap.array_length(names_arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let name = match shared.mem.heap.get_array_element(names_arr, i) {
            Ok(Value::Object(Some(name_ref))) => {
                crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref).unwrap_or_default()
            }
            _ => continue,
        };
        let val = shared
            .mem
            .heap
            .get_array_element(values_arr, i)
            .unwrap_or(Value::Object(None));
        out.push((name, val));
    }
    out
}

/// Read the annotation's type descriptor (field 0) вЂ” e.g. "Ljava/lang/Override;".
pub(in crate::vm::vm_exec) fn annotation_proxy_type_descriptor(shared: &SharedVm, proxy: ObjectRef) -> String {
    if let Value::Object(Some(s)) = shared.mem.heap.get_field(proxy, 0) {
        crate::vm::vm_object::read_java_string(&shared.mem.heap, s).unwrap_or_default()
    } else {
        String::new()
    }
}

/// Convert an annotation type descriptor to an internal class name.
/// E.g. `"Ljava/lang/Override;"` в†’ `"java/lang/Override"`. Empty input
/// (or non-descriptor strings) returns `"<unknown>"` to give a non-empty
/// fallback in `toString` output.
pub(in crate::vm::vm_exec) fn descriptor_to_class_name(desc: &str) -> String {
    if let Some(stripped) = desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        stripped.to_string()
    } else if desc.is_empty() {
        "<unknown>".to_string()
    } else {
        desc.to_string()
    }
}

/// Convert an internal slash-separated class name to the dotted form used
/// in `Annotation.toString()` output, e.g. `"java/lang/Override"` в†’
/// `"java.lang.Override"`.
pub(in crate::vm::vm_exec) fn internal_to_dotted(name: &str) -> String {
    name.replace('/', ".")
}

/// The annotation type's CANONICAL name (JLS 6.7) — what HotSpot's
/// `AnnotationInvocationHandler.toString()` prints. It formats
/// `annotationType().getCanonicalName()`, not `getName()`, so a member
/// annotation type renders with `.` where the binary name has `$`. Measured
/// on JDK 25.0.3.9: HotSpot prints `@AnnToString.Multi(...)` for the type
/// whose binary name is `AnnToString$Multi`.
///
/// Resolved from the annotation type's own `InnerClasses` attribute
/// (JVMS §4.7.6), **not** by rewriting every `$` in the binary name: `$` is a
/// legal Java identifier character, so a top-level `@interface A$B` has
/// canonical name `A$B`, and a member type may legally be named `Inner$Class`
/// — the same distinction `native_class_get_canonical_name` /
/// `own_inner_class_entry` draw in `native-builtins/src/lang_class.rs`, and
/// the one its `class_get_canonical_name_preserves_literal_dollar_in_member_name`
/// test pins. javac records the whole nesting chain in the nested type's own
/// attribute, so one class lookup resolves an arbitrarily deep `A$B$C`.
///
/// Total for an annotation type, with no `null` case to render: JLS 9.6 admits
/// only top-level and member annotation types — never local, never anonymous
/// — so the "has no canonical name" branch is unreachable here. It is still
/// written, and falls back to the dotted binary name rather than inventing a
/// `null` this caller has no rendering for. Every other miss (no mirror, no
/// class id, class not loaded) takes the same fallback, which is exactly the
/// string this function used to return unconditionally.
pub(in crate::vm::vm_exec) fn annotation_type_canonical_name(shared: &SharedVm, proxy: ObjectRef, internal: &str) -> String {
    let dotted_binary = internal_to_dotted(internal);
    // Slot 1 is `ANN_PROXY_TYPE_MIRROR` (see the `annotationType` arm of
    // `annotation_proxy_dispatch_impl`, which returns this same field). The
    // builder can never leave it null — `create_annotation_proxy_with_type`
    // falls back to the admitted `ClassId` precisely so that callers may
    // dereference it without a null check — so a miss means a proxy minted by
    // some other route, and the binary name is the honest answer for it.
    let mirror = match shared.mem.heap.get_field(proxy, 1) {
        Value::Object(Some(m)) => m,
        _ => return dotted_binary,
    };
    let class_id = match super::class_id_from_mirror(shared, mirror) {
        Some(id) => id,
        None => return dotted_binary,
    };
    let guard = shared.classes.class_manager.read();
    let class = match guard.get_class(class_id) {
        Some(c) => c,
        None => return dotted_binary,
    };
    let mut segments: Vec<&str> = Vec::new();
    let mut name: &str = internal;
    // Bounded walk: a real nesting chain is short, and the bound stops a
    // malformed `InnerClasses` table whose entries cycle from spinning here.
    for _ in 0..64 {
        let entry = match class.inner_classes.iter().find(|e| e.inner_class == name) {
            Some(e) => e,
            // No entry under this name: a top-level type. Its dotted binary
            // name IS its canonical name, `$` characters included.
            None => {
                let mut out = internal_to_dotted(name);
                for seg in segments.iter().rev() {
                    out.push('.');
                    out.push_str(seg);
                }
                return out;
            }
        };
        // The JVMS §4.7.6 index-0 cases arrive as empty strings: an empty
        // `inner_name` is an ANONYMOUS class, an empty `outer_class` a LOCAL
        // one. Neither has a canonical name, and neither can be an annotation
        // type.
        if entry.inner_name.is_empty() || entry.outer_class.is_empty() {
            return dotted_binary;
        }
        segments.push(&entry.inner_name);
        name = &entry.outer_class;
    }
    dotted_binary
}

/// Format an annotation member value:
///
/// * `String` -> `"text"` (Java-string-literal-escaped quoted form)
/// * `Class` -> `TypeName.class`
/// * Annotation proxy -> recursive `@TypeName(...)`
/// * Reference array -> `[a, b, c]`
/// * Primitive array -> element-list joined by `, ` inside `[ ... ]`
/// * boxed Integer/Long/etc. (from element-value pairs) -> underlying numeric
/// * Enum -> constant name (annotation enum element renders without type qualifier)
///
/// **This is NOT yet HotSpot's rendering, and this comment used to say it
/// was.** `sun/reflect/annotation/AnnotationInvocationHandler.memberValueToString`
/// was diffed against it on 2026-08-12 (Microsoft OpenJDK 25.0.3.9, same class
/// file, same session). Six members render differently, `want` being HotSpot:
///
/// ```text
///   boolean        got off=0                  want off=false
///   char           got ch=113                 want ch='q'
///   byte           got b=3                    want b=(byte)0x03
///   any array      got [a, b]                 want {a, b}
///   String         got the non-ASCII chars    want them as \\uXXXX escapes
///   nested @Ann    got $Proxy0                want @Outer.Inner(n=5)
/// ```
///
/// The JDK's rules, for whoever closes these: `toSourceString(byte)` is
/// `String.format("(byte)0x%02x", b)`; `toSourceString(char)` quotes with
/// `\b \f \n \r \t \' \\` and `\\u%04x` for anything outside printable ASCII
/// (`' '..'~'`), and the String form is the same escape set with `"` escaped
/// and `'` not; arrays are `Collectors.joining(", ", "{", "}")`; a `Class`
/// member is `getCanonicalName() + ".class"`, so a `Class[]` member should
/// print `java.lang.String[].class`, not `[Ljava.lang.String;.class`.
///
/// The nested-annotation row is the severe one and is a different shape from
/// the rest: with real annotation proxies enabled (the default) a nested
/// member's value is a generated `$ProxyN`, not an `AnnotationProxy`, so the
/// carrier-class test below misses it and the whole nested annotation is lost.
/// Closing it needs the proxy's invocation handler, which this function has no
/// route to today.
pub(in crate::vm::vm_exec) fn format_annotation_value(shared: &SharedVm, val: Value) -> String {
    match val {
        Value::Object(None) => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Long(l) => l.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Object(Some(obj)) => {
            // Decide based on object class name + heap kind.
            let kind = shared.mem.heap.kind_of(obj);
            if kind == crate::memory::heap::ObjectKind::Array {
                return format_annotation_array(shared, obj);
            }
            let cid = shared.mem.heap.class_id_of(obj);
            let cname = shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // Nested annotation proxy: recurse via toString helper
            if cname == "java/lang/annotation/AnnotationProxy" {
                return annotation_proxy_to_string(shared, obj);
            }
            // String вЂ” render as Java-string-literal "text"
            if cname == "java/lang/String" {
                if let Some(s) = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj) {
                    return format!("\"{}\"", java_string_escape(&s));
                }
            }
            // Class mirror вЂ” render as `TypeName.class`
            if cname == "java/lang/Class" {
                if let Some(cls_name) = class_mirror_name(shared, obj) {
                    return format!("{}.class", internal_to_dotted(&cls_name));
                }
                return "<unknown>.class".to_string();
            }
            // Boxed wrapper (1-field synthetic): unbox and recurse
            if let Some(prim_name) = wrapper_class_to_primitive(&cname) {
                let inner = shared.mem.heap.get_field(obj, 0);
                let mut s = format_annotation_value(shared, inner);
                if prim_name == "long" {
                    s.push('L');
                } else if prim_name == "float" {
                    s.push('f');
                }
                return s;
            }
            // Enum constant вЂ” return its `name` field. Standard layout has
            // field 0 = String name (set by `Enum.<init>`).
            if let Value::Object(Some(name_ref)) = shared.mem.heap.get_field(obj, 0) {
                if let Some(name) = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref) {
                    if !name.is_empty() {
                        return name;
                    }
                }
            }
            cname.replace('/', ".")
        }
        Value::Uninitialized => "null".to_string(),
        _ => "null".to_string(),
    }
}

/// Render an array element (reference or primitive) the way
/// `Arrays.toString` does for annotation values: `[a, b, c]`.
pub(in crate::vm::vm_exec) fn format_annotation_array(shared: &SharedVm, arr: ObjectRef) -> String {
    let n = shared.mem.heap.array_length(arr);
    let mut s = String::from("[");
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        match shared.mem.heap.get_array_element(arr, i) {
            Ok(v) => s.push_str(&format_annotation_value(shared, v)),
            Err(_) => s.push_str("null"),
        }
    }
    s.push(']');
    s
}

/// Recursive `Annotation.toString()` helper.
///
/// Members are emitted in **class-file `element_value_pairs` order** — the
/// order they were parsed in, which is the order `annotation_proxy_elements`
/// hands back. They are NOT sorted.
///
/// This used to sort them alphabetically, and this comment used to claim the
/// sort matched HotSpot. It does not. Measured 2026-08-12 against Microsoft
/// OpenJDK 25.0.3.9, same class file, same session:
///
/// ```text
/// HotSpot   @AnnToString.Multi(zeta="Z", mid="M", alpha=9)
/// was       @AnnToString$Multi(alpha=9, mid="M", zeta="Z")
/// ```
///
/// HotSpot's `AnnotationInvocationHandler` iterates `memberValues`, the
/// `LinkedHashMap` `AnnotationParser.parseAnnotation2` fills by `put`-ing each
/// `element_value_pair` in file order (`sun/reflect/annotation/
/// AnnotationParser.java:268-287`), so declaration order in the class file is
/// what it prints. That was confirmed by scrambling the use site relative to
/// the interface's declaration order and watching the rendering follow the use
/// site, not the interface and not the alphabet.
///
/// **Defaulted members: this twin is the `CRATONVM_ANNOTATION_JDK_SEMANTICS=0`
/// arm only.** For a member left at its `AnnotationDefault`, HotSpot seeds
/// that same map from `AnnotationType.memberDefaults()`, a plain
/// `java.util.HashMap` (`AnnotationType.java:111`), so defaulted members print
/// FIRST, in that map's bucket order, then the members with no default in
/// class-file order. Measured: five all-defaulted members declared
/// `zulu, alpha, mike, bravo, yankee` print `bravo, yankee, mike, zulu,
/// alpha`. In the default configuration `annotation_proxy_invoke_shared`
/// answers `toString` before reaching this function, through
/// `cratonvm_native_builtins::lang_class::annotation_carrier_to_string`, and
/// that path reproduces the `memberDefaults` order
/// (`jdk_annotation_members_in_map_order`, round 13 wave 6 and round 16 wave
/// 13). This function (and `format_annotation_value`'s nested-annotation arm,
/// which only it reaches) still appends the defaults after the explicit
/// members; it is the old answer the switch restores, not a gap in the
/// default path. Round 16 wave 14, lane compat2 (proposal AO-3).
pub(crate) fn annotation_proxy_to_string(shared: &SharedVm, proxy: ObjectRef) -> String {
    let desc = annotation_proxy_type_descriptor(shared, proxy);
    let class_name = descriptor_to_class_name(&desc);
    let dotted = annotation_type_canonical_name(shared, proxy, &class_name);
    // NOT sorted: class-file `element_value_pairs` order is what HotSpot prints.
    let elems = annotation_proxy_elements(shared, proxy);
    let members: Vec<(String, String)> = elems
        .into_iter()
        .map(|(name, val)| (name, format_annotation_value(shared, val)))
        .collect();
    // Assembly — separators plus the JDK's `value=` elision for a sole `value`
    // member — is SHARED with this function's twin,
    // `ctx_annotation_proxy_to_string` in `native-builtins/src/lang_class.rs`,
    // which serves the SAME method on the SAME objects through
    // `native_proxy_dispatch_invoke`. The two had drifted three ways at once
    // (2026-08-12) and one process printed two different strings for one
    // annotation object on consecutive calls. Which copy answers a given call
    // is not under the caller's control and is NOT a tier question — measured
    // with `-Xint`, the split persists; it varies by call shape.
    //
    // `vm` depends on `native-builtins`, never the reverse, so the half that
    // needs no VM handle lives there. Do not re-inline it. The three
    // VM-touching steps (member extraction, type-name canonicalisation, value
    // rendering) stay twinned because the two callers hold different handles
    // (`&SharedVm` vs `&mut dyn NativeContext`) in different crates.
    cratonvm_native_builtins::lang_class::render_annotation_to_string(&dotted, &members)
}

/// Java-string-literal escape: backslash, quotes, and control chars.
pub(in crate::vm::vm_exec) fn java_string_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Read the internal class name from a `java/lang/Class` mirror. Layout:
///   field 0 = class id (Int)
///   field 1 = name (String вЂ” internal slash form)
pub(in crate::vm::vm_exec) fn class_mirror_name(shared: &SharedVm, mirror: ObjectRef) -> Option<String> {
    if let Value::Object(Some(name_ref)) = shared.mem.heap.get_field(mirror, 1) {
        return crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref);
    }
    None
}

/// Map a wrapper class internal name to its primitive name.
pub(in crate::vm::vm_exec) fn wrapper_class_to_primitive(class_name: &str) -> Option<&'static str> {
    Some(match class_name {
        "java/lang/Integer" => "int",
        "java/lang/Long" => "long",
        "java/lang/Short" => "short",
        "java/lang/Byte" => "byte",
        "java/lang/Character" => "char",
        "java/lang/Boolean" => "boolean",
        "java/lang/Float" => "float",
        "java/lang/Double" => "double",
        _ => return None,
    })
}

/// Compute the per-member hashCode contribution: `(127 * nameHash) ^ valueHash`.
pub(in crate::vm::vm_exec) fn annotation_member_hash(shared: &SharedVm, name: &str, val: Value) -> i32 {
    let name_hash = java_string_hash(name);
    let value_hash = annotation_value_hash(shared, val);
    127i32.wrapping_mul(name_hash) ^ value_hash
}

/// Java-spec `String.hashCode()` вЂ” `s[0]*31^(n-1) + ... + s[n-1]`.
///
/// Public for WP2.7 conformance tests that pin the hash recipe against
/// the JDK reference output.
pub fn java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for ch in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(ch as i32);
    }
    h
}

/// Hash an annotation member value per `Annotation.hashCode()` rules.
pub(in crate::vm::vm_exec) fn annotation_value_hash(shared: &SharedVm, val: Value) -> i32 {
    match val {
        Value::Int(i) => i,
        Value::Long(l) => (l ^ (l >> 32)) as i32, // Long.hashCode
        Value::Float(f) => f.to_bits() as i32,
        Value::Double(d) => {
            let bits = d.to_bits() as i64;
            (bits ^ (bits >> 32)) as i32
        }
        Value::Object(None) | Value::Uninitialized => 0,
        Value::Object(Some(obj)) => {
            let kind = shared.mem.heap.kind_of(obj);
            if kind == crate::memory::heap::ObjectKind::Array {
                return annotation_array_hash(shared, obj);
            }
            let cid = shared.mem.heap.class_id_of(obj);
            let cname = shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // Boxed wrapper вЂ” hash the boxed primitive
            if wrapper_class_to_primitive(&cname).is_some() {
                let inner = shared.mem.heap.get_field(obj, 0);
                return annotation_value_hash(shared, inner);
            }
            // String вЂ” Java's String.hashCode contract
            if cname == "java/lang/String" {
                if let Some(s) = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj) {
                    return java_string_hash(&s);
                }
            }
            // Nested annotation вЂ” recurse
            if cname == "java/lang/annotation/AnnotationProxy" {
                return annotation_proxy_hash_code(shared, obj);
            }
            // Class mirror, Enum constant, and any other reference-typed member
            // hash via `value.hashCode()`. `Class` and `Enum` do NOT override
            // `Object.hashCode()`, so per the `Annotation.hashCode()` spec their
            // member hash is the IDENTITY hash code вЂ” NOT a name-derived hash.
            // (The native annotation proxy previously hashed the class/enum name,
            // which disagreed with Spring's synthesized proxy that calls the real
            // `value.hashCode()` в†’ identity, so `equals` could hold while the
            // hash codes differed, breaking `MergedAnnotation` synthesis.) Both
            // sides reference the same singleton mirror / enum constant within a
            // run, so the identity hashes agree.
            //
            // gen r4w3/hunter (2026-09-23): through the monitor-aware composition
            // `System.identityHashCode` uses (`NativeContextImpl::identity_hash_code`),
            // not the heap accessor alone. The heap answers 0 for an object whose
            // mark word is THIN_LOCKED or INFLATED and was never hashed — and a
            // `Class` mirror is locked by every `static synchronized` method of its
            // class — so this member hash was 0 while the mirror was locked and the
            // real identity hash afterwards: one annotation, two `hashCode()`s, and a
            // `HashSet<Annotation>` that could not find its own entry.
            let heap = &shared.mem.heap;
            shared
                .threads
                .monitors
                .java_identity_hash(obj, heap.identity_hash_code(obj), || {
                    heap.next_identity_hash()
                })
        }
        _ => 0,
    }
}

/// Per `Arrays.hashCode` for the relevant element type.
pub(in crate::vm::vm_exec) fn annotation_array_hash(shared: &SharedVm, arr: ObjectRef) -> i32 {
    let n = shared.mem.heap.array_length(arr);
    let mut h: i32 = 1;
    for i in 0..n {
        let elem_hash = match shared.mem.heap.get_array_element(arr, i) {
            Ok(v) => annotation_value_hash(shared, v),
            Err(_) => 0,
        };
        h = h.wrapping_mul(31).wrapping_add(elem_hash);
    }
    h
}

/// Compute spec-compliant `Annotation.hashCode()`.
pub(crate) fn annotation_proxy_hash_code(shared: &SharedVm, proxy: ObjectRef) -> i32 {
    let elems = annotation_proxy_elements(shared, proxy);
    let mut h: i32 = 0;
    for (name, val) in elems {
        h = h.wrapping_add(annotation_member_hash(shared, &name, val));
    }
    h
}

/// Spec-compliant `Annotation.equals(Object)` вЂ” returns true iff the other
/// reference is also an `AnnotationProxy` with the same annotation type AND
/// every element value matches.
pub(crate) fn annotation_proxy_equals(shared: &SharedVm, a: ObjectRef, b: Value) -> bool {
    let other = match b {
        Value::Object(Some(o)) => o,
        _ => return false,
    };
    if a == other {
        return true;
    }
    // The other side must also be an annotation proxy of the same type.
    let other_kind = shared.mem.heap.kind_of(other);
    if other_kind != crate::memory::heap::ObjectKind::Object {
        return false;
    }
    let other_cid = shared.mem.heap.class_id_of(other);
    let other_cname = shared
        .classes
        .class_manager
        .read()
        .get_class(other_cid)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    if other_cname != "java/lang/annotation/AnnotationProxy" {
        return false;
    }
    let a_desc = annotation_proxy_type_descriptor(shared, a);
    let b_desc = annotation_proxy_type_descriptor(shared, other);
    if a_desc != b_desc {
        return false;
    }
    // Compare element-by-element. Look up by name on the b-side to be tolerant
    // of insertion-order drift between two construction paths for the same
    // annotation type (e.g. one with all explicit elements, one falling back
    // to AnnotationDefault).
    let a_elems = annotation_proxy_elements(shared, a);
    let b_elems = annotation_proxy_elements(shared, other);
    if a_elems.len() != b_elems.len() {
        return false;
    }
    for (name, av) in &a_elems {
        let bv = match b_elems.iter().find(|(n, _)| n == name) {
            Some((_, v)) => *v,
            None => return false,
        };
        if !annotation_values_equal(shared, *av, bv) {
            return false;
        }
    }
    true
}

/// Compare two annotation member values вЂ” `Arrays.equals` semantics on
/// arrays, recursive on nested annotations, identity-aware on others.
pub(in crate::vm::vm_exec) fn annotation_values_equal(shared: &SharedVm, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(None), _) | (_, Value::Object(None)) => false,
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x == y {
                return true;
            }
            let xk = shared.mem.heap.kind_of(x);
            let yk = shared.mem.heap.kind_of(y);
            if xk == crate::memory::heap::ObjectKind::Array
                || yk == crate::memory::heap::ObjectKind::Array
            {
                if xk != yk {
                    return false;
                }
                let nx = shared.mem.heap.array_length(x);
                let ny = shared.mem.heap.array_length(y);
                if nx != ny {
                    return false;
                }
                for i in 0..nx {
                    let av = shared
                        .mem
                        .heap
                        .get_array_element(x, i)
                        .unwrap_or(Value::Object(None));
                    let bv = shared
                        .mem
                        .heap
                        .get_array_element(y, i)
                        .unwrap_or(Value::Object(None));
                    if !annotation_values_equal(shared, av, bv) {
                        return false;
                    }
                }
                return true;
            }
            let xcid = shared.mem.heap.class_id_of(x);
            let ycid = shared.mem.heap.class_id_of(y);
            let xname = shared
                .classes
                .class_manager
                .read()
                .get_class(xcid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            let yname = shared
                .classes
                .class_manager
                .read()
                .get_class(ycid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // Nested annotation: compare structurally.
            if xname == "java/lang/annotation/AnnotationProxy"
                && yname == "java/lang/annotation/AnnotationProxy"
            {
                return annotation_proxy_equals(shared, x, Value::Object(Some(y)));
            }
            // String compare
            if xname == "java/lang/String" && yname == "java/lang/String" {
                let sx = crate::vm::vm_object::read_java_string(&shared.mem.heap, x).unwrap_or_default();
                let sy = crate::vm::vm_object::read_java_string(&shared.mem.heap, y).unwrap_or_default();
                return sx == sy;
            }
            // Class mirror compare by class id
            if xname == "java/lang/Class" && yname == "java/lang/Class" {
                let xcid_field = shared.mem.heap.get_field(x, 0);
                let ycid_field = shared.mem.heap.get_field(y, 0);
                return matches!(
                    (xcid_field, ycid_field),
                    (Value::Int(a), Value::Int(b)) if a == b
                );
            }
            // Boxed wrapper вЂ” unbox & recurse
            if wrapper_class_to_primitive(&xname).is_some()
                && wrapper_class_to_primitive(&yname).is_some()
            {
                let xv = shared.mem.heap.get_field(x, 0);
                let yv = shared.mem.heap.get_field(y, 0);
                return annotation_values_equal(shared, xv, yv);
            }
            // Enum / generic вЂ” compare name field if present.
            if let (Value::Object(Some(xn)), Value::Object(Some(yn))) = (
                shared.mem.heap.get_field(x, 0),
                shared.mem.heap.get_field(y, 0),
            ) {
                let sx = crate::vm::vm_object::read_java_string(&shared.mem.heap, xn).unwrap_or_default();
                let sy = crate::vm::vm_object::read_java_string(&shared.mem.heap, yn).unwrap_or_default();
                if !sx.is_empty() && !sy.is_empty() {
                    return sx == sy && xname == yname;
                }
            }
            x == y
        }
        _ => false,
    }
}

/// `CRATONVM_ANN_PROXY_DISPATCH_TRACE`, latched.
///
/// Read from the annotation-proxy element walk, which is hot enough that a
/// per-call environment lookup showed up as 21% of the dispatch. The flag
/// surface is immutable once any flag has been read, so one answer per process
/// is the whole truth.
pub(in crate::vm::vm_exec) fn ann_proxy_dispatch_trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>.is_some()
    })
}

/// `InvocationHandler.invoke(proxy, Method, Object[])` on an annotation
/// carrier: the annotation method's name and the unpacked arguments, when
/// `args[1]` is a real `Method` (a genuine 0-arg element named `invoke()`
/// never gets here).
pub(in crate::vm::vm_exec) fn annotation_handler_invoke_target(
    shared: &SharedVm,
    args: &[Value],
) -> Option<(String, Vec<Value>)> {
    let Some(Value::Object(Some(method_obj))) = args.get(1).copied() else {
        return None;
    };
    if !class_name_is(shared, method_obj, "java/lang/reflect/Method") {
        return None;
    }
    let name_val = {
        let method_cid = shared.mem.heap.class_id_of(method_obj);
        let cm = shared.classes.class_manager.read();
        resolve_field_index_in_hierarchy(method_cid, "name", &cm.class_store)
            .map(|idx| shared.mem.heap.get_field(method_obj, idx))
    };
    let Some(Value::Object(Some(name_ref))) = name_val else {
        return None;
    };
    let real_name = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref)?;
    // Unpack the InvocationHandler args array (null for a 0-arg annotation
    // method like `value()`).
    let inner: Vec<Value> = match args.get(2).copied() {
        Some(Value::Object(Some(arr))) => {
            let len = shared.mem.heap.array_length(arr);
            (0..len)
                .filter_map(|i| shared.mem.heap.get_array_element(arr, i).ok())
                .collect()
        }
        _ => Vec::new(),
    };
    Some((real_name, inner))
}

/// Spec-compliant dispatcher for any method invoked on an annotation proxy.
pub(crate) fn annotation_proxy_dispatch_impl(
    shared: &SharedVm,
    proxy: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    // `CRATONVM_DBG=ann-proxy-prof` — see [`ann_proxy_prof`]. Reading one
    // annotation attribute (`@Marker.value()`) measures ~10.5 us here against
    // ~10 ns on HotSpot, and against ~1.5 us for `annotationType()`, which takes
    // the same entry path but returns from the top of this match. That ~9 us
    // gap is what these timers split.
    let prof = ann_proxy_prof::on();
    let prof_entry = prof.then(std::time::Instant::now);
    struct AnnProfGuard(Option<std::time::Instant>);
    impl Drop for AnnProfGuard {
        fn drop(&mut self) {
            let Some(t0) = self.0 else { return };
            ann_proxy_prof::add(&ann_proxy_prof::TOTAL_NS, t0.elapsed().as_nanos() as u64);
            let n = ann_proxy_prof::CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n % ann_proxy_prof::REPORT_EVERY == 0 {
                ann_proxy_prof::report();
            }
        }
    }
    let _ann_prof_guard = AnnProfGuard(prof_entry);

    match method_name {
        // InvocationHandler.invoke(Object proxy, Method method, Object[] args):
        // Under CRATONVM_REAL_ANNOTATIONS an annotation instance is a real
        // `$ProxyN` whose InvocationHandler is this synthetic AnnotationProxy.
        // Spring's `AnnotationUtils.invokeAnnotationMethod` does NOT call the
        // generated proxy body — it reads the handler off the proxy directly
        // (`Proxy.getInvocationHandler(ann).invoke(ann, method, null)`), which
        // dispatches `invoke` straight onto the AnnotationProxy and lands here
        // with `method_name == "invoke"`. The AnnotationProxy has no "invoke"
        // element, so the element walk below returns null; Spring's
        // `MergedAnnotation` then sees every attribute as absent and
        // `getString(...)`/`getStringArray(...)` throw NoSuchElementException —
        // silently dropping e.g. `@Scope("prototype")` (S03 proto.* regression).
        // Route to the annotation method named by the passed `Method`, mirroring
        // the `Proxy$Dispatch.invokeProxy` fix in `native_proxy_dispatch_invoke`
        // (48128e1a). Guarded by arity + a real `Method` arg so a genuine
        // annotation element literally named `invoke()` (0-arg) still resolves
        // via the element walk.
        "invoke" if args.len() == 3 => {
            if let Some((real_name, inner)) = annotation_handler_invoke_target(shared, args) {
                return annotation_proxy_dispatch_impl(shared, proxy, &real_name, &inner);
            }
        }
        // annotationType() returns the cached Class mirror.
        "annotationType" => {
            return Ok(Some(shared.mem.heap.get_field(proxy, 1)));
        }
        // toString() вЂ” spec-compliant @Type(name=value, ...)
        "toString" => {
            let s = annotation_proxy_to_string(shared, proxy);
            // gc-common w7-b: a fresh string, as HotSpot's
            // `AnnotationInvocationHandler.toStringImpl` returns; interning
            // leaked one pool entry per distinct rendering, and the pooled
            // entry was infallible.
            let result = super::try_create_java_string_uninterned(shared, &s)
                .ok_or_else(|| proxy_heap_exhausted("annotation toString"))?;
            return Ok(Some(Value::Object(Some(result))));
        }
        // hashCode() вЂ” sum of (127 * nameHash) ^ valueHash
        "hashCode" => {
            return Ok(Some(Value::Int(annotation_proxy_hash_code(shared, proxy))));
        }
        // equals(Object) вЂ” annotation-equality contract
        "equals" => {
            let other = args.first().copied().unwrap_or(Value::Object(None));
            let eq = annotation_proxy_equals(shared, proxy, other);
            return Ok(Some(Value::Int(if eq { 1 } else { 0 })));
        }
        // S111r18 — `getClass()` is a final native on Object, but our
        // interception layer at `execute_invoke` routes EVERY method call
        // on an `AnnotationProxy` here (because the receiver's class_id
        // is the synthetic AnnotationProxy class, not Object). Without
        // this branch the call falls through to the element-accessor walk
        // below, finds no element named "getClass", and returns null —
        // breaking Spring's `MergedAnnotation.adaptForAttribute` which
        // calls `value.getClass().isArray()` on the raw attribute value
        // and NPEs. Return the proxy's class mirror (the annotation
        // type's mirror, stored in field 1 by `create_annotation_proxy`)
        // so callers see something sensible. We deliberately return the
        // annotation type Class — not the AnnotationProxy synthetic Class
        // — to match real-JDK behaviour where `q.getClass()` reports the
        // Proxy class and `q.annotationType()` reports the annotation
        // interface, but Spring only needs *some* non-null Class with
        // `isArray()==false` and `isAnnotation()==true`. The annotation-type
        // answer now applies only to the bare-carrier representation.
        "getClass" => {
            // Round 13 wave 9 (lane proxy6): with real annotations the
            // carrier is a `$ProxyN`'s handler, and its class is its own --
            // the answer compiled code reads from the header. The annotation
            // type stays for the bare-carrier representation.
            if cratonvm_native_builtins::lang_class::annotation_carrier_get_class_is_own() {
                let cid = shared.mem.heap.class_id_of(proxy);
                // gch h2/fj: on the unwinding arm (`proxy_mirror_or_oome`).
                let mirror =
                    proxy_mirror_or_oome(|| super::get_or_create_class_mirror(shared, cid))?;
                return Ok(Some(Value::Object(Some(mirror))));
            }
            return Ok(Some(shared.mem.heap.get_field(proxy, 1)));
        }
        // S111r20 — `getType()` is a `MergedAnnotation` interface method.
        // Spring's `TypeMappedAnnotation.adaptValueForMapOptions` treats
        // AnnotationProxy arrays as `MergedAnnotation[]` and calls
        // `asMap(factory, adaptations)` on each element. The factory lambda
        // is `mergedAnnotation -> new AnnotationAttributes(mergedAnnotation.getType(), ...)`.
        // Without this branch, `getType()` falls through, finds no element
        // named "getType", returns null, and AnnotationAttributes throws
        // `IllegalArgumentException: 'annotationType' must not be null`.
        //
        // Round 13 wave 8 (lane proxy5): only for an annotation interface that
        // declares no `getType()` member of its own -- one that does reads it
        // like any member (`CRATONVM_ANNOTATION_MEMBER_CONFORMANCE`).
        "getType"
            if !(cratonvm_native_builtins::lang_class::annotation_member_conformance()
                && annotation_type_declares_member(shared, proxy, "getType")) =>
        {
            return Ok(Some(shared.mem.heap.get_field(proxy, 1)));
        }
        _ => {}
    }

    // Everything above returned from the match; reaching here means this is an
    // ELEMENT ACCESSOR, the shape that measures ~9 us more than the fast arms.
    let walk_start = prof.then(std::time::Instant::now);
    struct AnnWalkGuard(Option<std::time::Instant>);
    impl Drop for AnnWalkGuard {
        fn drop(&mut self) {
            let Some(t0) = self.0 else { return };
            ann_proxy_prof::add(&ann_proxy_prof::WALK_NS, t0.elapsed().as_nanos() as u64);
            ann_proxy_prof::CALLS_WALK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let _ann_walk_guard = AnnWalkGuard(walk_start);

    // Element accessor: walk the parallel arrays for a matching name.
    let names_arr = match shared.mem.heap.get_field(proxy, 2) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let values_arr = match shared.mem.heap.get_field(proxy, 3) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // This was `runtime_var_os(...).is_some() && method_name == "value"`, and
    // `&&` evaluates left to right — so the env lookup ran on EVERY element
    // access, not only on the traced one. `ann-proxy-prof` priced it at ~155 ns
    // of a ~750 ns dispatch, 21% of the walk, to answer a question whose answer
    // cannot change after startup. Latched once per process instead; the
    // `method_name` test now runs first as well, so an unarmed run pays a
    // string compare against a `bool` load.
    let flagread_start = prof.then(std::time::Instant::now);
    let trace_on = ann_proxy_dispatch_trace_on();
    if let Some(t) = flagread_start {
        ann_proxy_prof::add(&ann_proxy_prof::FLAGREAD_NS, t.elapsed().as_nanos() as u64);
    }
    if method_name == "value" && trace_on {
        let type_desc = match shared.mem.heap.get_field(proxy, 0) {
            Value::Object(Some(s)) => {
                crate::vm::vm_object::read_java_string(&shared.mem.heap, s).unwrap_or_default()
            }
            _ => String::new(),
        };
        if type_desc.contains("Import") {
            eprintln!(
                "[ANN-PROXY-DISPATCH] proxy={:p} method={} type_desc={}",
                proxy.as_ptr(),
                method_name,
                type_desc
            );
        }
    }
    let n = shared.mem.heap.array_length(names_arr);
    for i in 0..n {
        if let Ok(Value::Object(Some(name_ref))) = shared.mem.heap.get_array_element(names_arr, i) {
            // `read_java_string` materialises a whole Rust `String` (bulk array
            // copy + UTF-8/UTF-16 decode + allocation) for a name we only ever
            // compare. Timed separately because that is the obvious suspect.
            let namecmp_start = prof.then(std::time::Instant::now);
            let decoded = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref);
            if let Some(t) = namecmp_start {
                ann_proxy_prof::add(&ann_proxy_prof::NAMECMP_NS, t.elapsed().as_nanos() as u64);
            }
            if let Some(name) = decoded {
                if name == method_name {
                    if let Ok(val) = shared.mem.heap.get_array_element(values_arr, i) {
                        // Deferred `TypeNotPresentException`: a Class-valued member
                        // that could not be resolved through the declaring class's
                        // loader (classloader-isolation / filtering) was stored as a
                        // `TypeNotPresentException` sentinel at proxy-creation time
                        // (mirrors HotSpot's `TypeNotPresentExceptionProxy`). Throw it
                        // now — on member ACCESS — so `getAnnotations()` did not throw
                        // but `annotation.value()` does.
                        if let Value::Object(Some(obj)) = val {
                            if class_name_is(shared, obj, "java/lang/TypeNotPresentException") {
                                return Err(MethodCallFailed::ExceptionThrown(obj));
                            }
                        }
                        return Ok(Some(val));
                    }
                }
            }
        }
    }
    // Element not found in the proxy's parallel arrays. A proxy materialised
    // before its annotation interface was loadable misses the backfilled
    // defaults (`create_annotation_proxy` only backfills when the annotation
    // ClassId resolves). For a PRIMITIVE-returning member that then surfaces as
    // a coerced-null garbage int/long: e.g. ByteBuddy reads
    // `@Advice.OnMethodEnter.skipOnIndex()` (declared `int ... default -1`) and
    // a garbage `>= 0` makes `RelocationHandler.ForType.of` throw "void is not
    // an array type but an index for a relocation is defined", failing
    // Hibernate's BytecodeProvider service-load. Mirror the real JDK's
    // AnnotationInvocationHandler: fall back to the interface's declared default.
    // Primitive defaults are returned unboxed — the caller in `interpreter.rs`
    // unboxes by the method's primitive return descriptor — while object-typed
    // defaults keep the previous null behaviour (those are normally backfilled).
    if let Some(elem) = annotation_member_declared_default(shared, proxy, method_name) {
        use crate::native::registry::AnnotationElementValue;
        match elem {
            AnnotationElementValue::Int(v) => return Ok(Some(Value::Int(v))),
            AnnotationElementValue::Long(v) => return Ok(Some(Value::Long(v))),
            AnnotationElementValue::Float(v) => return Ok(Some(Value::Float(v))),
            AnnotationElementValue::Double(v) => return Ok(Some(Value::Double(v))),
            _ => {}
        }
    }
    // Element not found — return null/default. This is the last stop for every
    // "annotation member reads back as null" report (Byte Buddy's
    // `AnnotationDescription$ForLoadedAnnotation.getValue` NPEs on it, Spring's
    // `AnnotationsScanner` treats the annotation as absent), so name the
    // annotation and the member when the dispatch trace is on.
    if trace_on {
        let type_desc = match shared.mem.heap.get_field(proxy, 0) {
            Value::Object(Some(s)) => {
                crate::vm::vm_object::read_java_string(&shared.mem.heap, s).unwrap_or_default()
            }
            _ => String::new(),
        };
        eprintln!("[ANN-PROXY-MISS] type={type_desc} member={method_name} elements={n} -> null");
    }
    Ok(Some(Value::Object(None)))
}

/// Whether the annotation interface behind the carrier `proxy` (its type
/// mirror, slot 1) declares a member `name()`.
pub(in crate::vm::vm_exec) fn annotation_type_declares_member(shared: &SharedVm, proxy: ObjectRef, name: &str) -> bool {
    let Value::Object(Some(mirror)) = shared.mem.heap.get_field(proxy, 1) else {
        return false;
    };
    let Some(type_cid) = super::class_id_from_mirror(shared, mirror) else {
        return false;
    };
    let cm = shared.classes.class_manager.read();
    cm.get_class(type_cid).is_some_and(|class| {
        class
            .methods
            .iter()
            .any(|m| &*m.name == name && m.descriptor.starts_with("()"))
    })
}

/// Look up the declared default value of annotation member `method_name` for
/// the annotation interface a proxy represents. Used as the absent-member
/// fallback in [`annotation_proxy_dispatch_impl`] — mirrors the real-JDK
/// `AnnotationInvocationHandler` default behaviour. Returns `None` if the proxy
/// has no resolvable type descriptor, the interface isn't loaded, the member is
/// absent, or it has no `AnnotationDefault` attribute.
pub(in crate::vm::vm_exec) fn annotation_member_declared_default(
    shared: &SharedVm,
    proxy: ObjectRef,
    method_name: &str,
) -> Option<crate::native::registry::AnnotationElementValue> {
    // Proxy field 0 holds the annotation type descriptor, e.g. `Lpkg/Ann;`.
    let desc = match shared.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(s)) => crate::vm::vm_object::read_java_string(&shared.mem.heap, s)?,
        _ => return None,
    };
    let class_name = desc
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(&desc)
        .to_string();
    let cid = shared
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name(&class_name)?;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(cid)?;
    for m in &class.methods {
        // Annotation members are no-arg abstract methods.
        if &*m.name == method_name && m.descriptor.starts_with("()") {
            for attr in &m.attributes {
                if let Some(cratonvm_reader::attribute::Attribute::AnnotationDefault(ev)) =
                    attr.as_decoded()
                {
                    return convert_element_value(ev, &class.constant_pool);
                }
            }
            return None;
        }
    }
    None
}

/// Count the number of parameters in a JVM descriptor like `(ILjava/lang/String;)V`.
pub(in crate::vm) fn proxy_count_params(descriptor: &str) -> usize {
    let inner = match descriptor.find('(') {
        Some(start) => match descriptor.find(')') {
            Some(end) if end > start => &descriptor[start + 1..end],
            _ => return 0,
        },
        None => return 0,
    };
    let mut count = 0;
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' => count += 1,
            'L' => {
                count += 1;
                // skip until ';'
                for c in chars.by_ref() {
                    if c == ';' {
                        break;
                    }
                }
            }
            '[' => {
                // array prefix вЂ” don't count the '[' itself, the element type follows
            }
            _ => {
                tracing::warn!(
                    "Unrecognized type character '{}' in method descriptor: {}",
                    ch,
                    descriptor
                );
            }
        }
    }
    count
}

/// Split a method descriptor into individual parameter type descriptors and
/// the return type descriptor. Each returned descriptor includes any leading
/// `[` array prefix and (for `L`-types) the trailing `;`.
///
/// `()V` -> (vec![], "V")
/// `(Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;`
///   -> (vec!["Ljava/lang/reflect/Method;", "[Ljava/lang/Object;"], "Ljava/lang/Object;")
pub(in crate::vm) fn proxy_split_descriptor(descriptor: &str) -> (Vec<String>, String) {
    let start = match descriptor.find('(') {
        Some(s) => s,
        None => return (Vec::new(), String::new()),
    };
    let end = match descriptor.find(')') {
        Some(e) if e > start => e,
        _ => return (Vec::new(), String::new()),
    };
    let inner = &descriptor[start + 1..end];
    let ret = descriptor[end + 1..].to_string();
    let mut params = Vec::new();
    let mut buf = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        buf.push(ch);
        match ch {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' | 'V' => {
                params.push(std::mem::take(&mut buf));
            }
            'L' => {
                for c in chars.by_ref() {
                    buf.push(c);
                    if c == ';' {
                        break;
                    }
                }
                params.push(std::mem::take(&mut buf));
            }
            '[' => {
                // array prefix — keep accumulating until we get a base type
            }
            _ => {
                buf.clear();
            }
        }
    }
    (params, ret)
}

/// Resolve a single JVMS field-type descriptor (e.g. `Ljava/lang/Object;`,
/// `[Ljava/lang/reflect/Type;`, `I`, `V`) to a Class mirror. Used by
/// `proxy_invoke_handler` so the synthetic `Method` object's `returnType`
/// and `parameterTypes` carry semantically correct mirrors — Spring's
/// `SerializableTypeWrapper$TypeProxyInvocationHandler.invoke` branches on
/// `method.getReturnType() == Type.class` / `Type[].class`, which only works
/// when those mirrors point at the real `java.lang.reflect.Type` Class.
/// The classes named in a proxy method's signature, resolved the way
/// the declaring interface's own bytecode resolves them -- through ITS defining
/// loader (JVMS 5.4.3.1). `proxy_descriptor_to_class_mirror` resolves by name, so
/// a `Method` handed to an `InvocationHandler` for an interface a forked loader
/// defined reported the APPLICATION's copy of its return and parameter types:
/// Spring's `TypeMappedAnnotation.adapt` then refused the fork's own enum constant
/// as "not an instance of" the application's enum.
///
/// One entry per descriptor: `Some(class id)` when the loader answered, `None`
/// to resolve by name as before (primitives, and every descriptor when
/// [`proxy_types_through_loader`] says no). This runs loader Java code, so it
/// must be called BEFORE the caller allocates anything it will keep: it returns
/// ids, not object references, for that reason.
pub(in crate::vm::vm_exec) fn proxy_signature_class_ids(
    shared: &SharedVm,
    thread: &mut JvmThread,
    declaring_mirror: ObjectRef,
    descs: &[&str],
) -> Vec<Option<ClassId>> {
    let none = || vec![None; descs.len()];
    if !proxy_types_through_loader(shared) {
        return none();
    }
    let Some(declaring) = super::class_id_from_mirror(shared, declaring_mirror) else {
        return none();
    };
    let mut ctx = NativeContextImpl { shared, thread };
    descs
        .iter()
        .map(|desc| {
            if !(desc.starts_with('L') || desc.starts_with('[')) {
                return None;
            }
            let mirror =
                cratonvm_native_builtins::lang_class::descriptor_to_class_mirror_via_loader(
                    &mut ctx, desc, declaring,
                );
            super::class_id_from_mirror(shared, mirror)
        })
        .collect()
}

/// [`proxy_signature_class_ids`] for a proxy dispatch, with the values the caller
/// keeps across the loader's Java code pinned and handed back refreshed: the
/// proxy itself and the argument array's object elements. `None` when
/// [`proxy_types_through_loader`] says no, which leaves every caller on its
/// by-name path.
pub(in crate::vm::vm_exec) fn proxy_signature_class_ids_pinned(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<(Vec<Option<ClassId>>, ObjectRef, Vec<Value>)> {
    if !proxy_types_through_loader(shared) {
        return None;
    }
    let (param_descs, ret_desc) = proxy_split_descriptor(descriptor);
    let mut all: Vec<&str> = Vec::with_capacity(param_descs.len() + 1);
    all.push(ret_desc.as_str());
    all.extend(param_descs.iter().map(String::as_str));
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(proxy);
    for a in args {
        if let Value::Object(Some(o)) = a {
            thread.native_pin_roots.push(*o);
        }
    }
    // gch h2/fj: on the unwinding arm (`proxy_mirror_or_oome`); a full heap
    // leaves the caller on its by-name path, whose own mints then answer it.
    let Ok(declaring) = proxy_mirror_or_oome(|| {
        proxy_resolve_declaring_class_mirror(shared, proxy, method_name, descriptor)
    }) else {
        thread.native_pin_roots.truncate(pin_base);
        return None;
    };
    let ids = proxy_signature_class_ids(shared, thread, declaring, &all);
    let fresh_proxy = thread.native_pin_roots[pin_base];
    let mut fresh_args = args.to_vec();
    let mut k = pin_base + 1;
    for v in fresh_args.iter_mut() {
        if let Value::Object(Some(_)) = v {
            *v = Value::Object(Some(thread.native_pin_roots[k]));
            k += 1;
        }
    }
    thread.native_pin_roots.truncate(pin_base);
    Some((ids, fresh_proxy, fresh_args))
}

/// Does a proxy dispatch resolve its `Method`'s signature types through the
/// declaring interface's loader ([`proxy_signature_class_ids`])? Always under
/// `--real-jdk`; under `--compatible` too since round 12 wave 8 (owner decision
/// 2026-09-27: a wrong answer is fixed in both modes), with the kill switch
/// `CRATONVM_PROXY_TYPES_THROUGH_LOADER=0` restoring the by-name types there.
/// The switch is read per call, not cached (no new `static`:
/// `vm/tests/per_vm_state_statics_ratchet.rs`), and never under `--real-jdk`.
pub(in crate::vm::vm_exec) fn proxy_types_through_loader(shared: &SharedVm) -> bool {
    compat2_mode_gate(shared.config.is_jdk_only(), || {
        true
    })
}

/// The thrown form of a §5.4.6 selection error: `AbstractMethodError` or
/// `IncompatibleClassChangeError`.
pub(in crate::vm::vm_exec) fn selection_error_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    error: crate::runtime::resolve::selection::SelectionError,
) -> MethodCallFailed {
    match error {
        crate::runtime::resolve::selection::SelectionError::AbstractMethod(message) => {
            match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/AbstractMethodError",
                Some(&message),
            ) {
                Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                Err(e) => e,
            }
        }
        crate::runtime::resolve::selection::SelectionError::ConflictingDefaults(message) => {
            crate::runtime::exceptions::throw_linkage_error(
                shared,
                thread,
                LinkageError::IncompatibleClassChangeError { message },
            )
        }
        crate::runtime::resolve::selection::SelectionError::NotPublic(message) => {
            crate::runtime::exceptions::throw_linkage_error(
                shared,
                thread,
                LinkageError::IllegalAccessError { message },
            )
        }
    }
}

/// `NativeContext::virtual_selection_failure` (round 12 wave 8 orchestrator,
/// `r12w8-orch-probe-findings-on-w8` item 1): `Method.invoke`'s virtual door
/// dispatches by the receiver's class, so `invoke_on_class_shared_inner`'s
/// strict block (`class_id != named_class_id`) never ran for it and a
/// reflective call ran whatever body the lenient walk found. Judged against
/// the resolved method's owner here instead. Same gate as that block
/// ([`native_invoke_strict_selection`]), asked only once an error is found;
/// a receiver of the owner's own class is never judged.
pub(crate) fn reflective_selection_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    receiver_class: ClassId,
    owner: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<MethodCallFailed> {
    if receiver_class == owner {
        return None;
    }
    flagged_selection_failure(
        shared,
        thread,
        receiver_class,
        owner,
        method_name,
        descriptor,
    )
}

/// [`reflective_selection_failure`] without its same-class fast accept, for a
/// compiled site whose (site, receiver class) target is already flagged as a
/// selection error (`jit/helpers.rs` `jit_selection_failure`). A receiver of
/// exactly the site's class still selects past an abstract re-declaration
/// the lenient walk skips: `final C extends abstract B { abstract m } extends
/// A { m }` called through `invokevirtual C.m` ran `A.m` once its caller was
/// compiled (interpreter round i1 wave 35, `L4W35FinalDevirtAbstract`).
pub(crate) fn flagged_selection_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    receiver_class: ClassId,
    owner: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<MethodCallFailed> {
    let error = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::resolve::selection::receiver_selection_error(
            &cm.class_store,
            receiver_class,
            owner,
            method_name,
            descriptor,
        )
    }?;
    if !native_invoke_strict_selection(shared) {
        return None;
    }
    Some(selection_error_failure(shared, thread, error))
}

/// For `invoke_virtual_declared` (a `findVirtual` handle's leaf, and the
/// natives that name the declaring class): the class whose method §5.4.6
/// selects for `receiver` against `method_name`/`descriptor` resolved in
/// `declared_class`, when that is NOT what the receiver-class walk would run.
/// `None` when the two agree -- the common case, answered without a
/// selection walk (`walk_selection_moves_to`'s fast accepts) -- when the
/// declared class is not on the receiver's superclass chain (an interface, a
/// lambda proxy, an array), and for every answer selection cannot speak for
/// (`Lenient`, or an error the walk's door keeps).
///
/// The declared class arrives by NAME; the class of that name on the
/// receiver's own chain is the one the handle's receiver check admitted, so a
/// loader that re-defines the name elsewhere cannot redirect it.
///
/// Kill switch `CRATONVM_REFLECTIVE_DISPATCH_SELECTS` (default on, both
/// modes; round 13 wave 1, lane mhffm), read only once selection disagrees
/// with the walk: `0` runs the walk's answer as before.
pub(in crate::vm::vm_exec) fn declared_dispatch_selection(
    shared: &SharedVm,
    declared_class: &str,
    declared_id: Option<ClassId>,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
) -> Option<ClassId> {
    if method_name.starts_with('<') {
        return None;
    }
    let heap = &shared.mem.heap;
    let receiver = heap.load_and_forward(receiver);
    if heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let receiver_class = heap.class_id_of(receiver);
    // Interpreter round i6 wave 118, lane L4 (`CRATONVM_MH_VIRTUAL_BY_OWNER_ID`):
    // with the handle's owner id the declared class is found on the
    // receiver's chain by id (one compare per step), not by name.
    let is_declared = |id: ClassId, name: &str| match declared_id {
        Some(owner) => id == owner,
        None => name == declared_class,
    };
    let selected = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let receiver_entry = store.get(receiver_class)?;
        if is_declared(receiver_class, &receiver_entry.name) {
            return None;
        }
        let mut owner = None;
        let mut cursor = receiver_entry.superclass;
        let mut steps = 0usize;
        while let Some(id) = cursor {
            steps += 1;
            if steps > 512 {
                return None;
            }
            let class = store.get(id)?;
            if is_declared(id, &class.name) {
                owner = Some(id);
                break;
            }
            cursor = class.superclass;
        }
        crate::runtime::resolve::selection::receiver_dispatch_moves_to(
            store,
            receiver_class,
            owner?,
            method_name,
            descriptor,
        )?
    };
    true
        .then_some(selected)
}



/// Round 12 wave 8 (lane compat2): may a native / reflective up-call through
/// [`invoke_on_class_shared_inner`] raise the §5.4.6 selection error
/// `strict_selection_error` found? Always under `--real-jdk`; under
/// `--compatible` unless `CRATONVM_NATIVE_INVOKE_STRICT_SELECTION=0`. Asked
/// only once an error was found, so the switch is never read on a call that
/// selects normally.
pub(in crate::vm::vm_exec) fn native_invoke_strict_selection(shared: &SharedVm) -> bool {
    compat2_mode_gate(dispatch_policy(shared).is_jdk_only(), || {
        true
    })
}

/// The shape of both gates above: a strict-mode correctness fix that also
/// applies under `--compatible` unless its kill switch is off. With the switch
/// off, `--compatible` answers exactly as before the fix.
pub(in crate::vm::vm_exec) fn compat2_mode_gate(jdk_only: bool, compatible_switch_on: impl FnOnce() -> bool) -> bool {
    jdk_only || compatible_switch_on()
}


/// The mirror for one signature descriptor: the loader-resolved class when
/// [`proxy_signature_class_ids`] found one, otherwise by name. Minted on the
/// unwinding arm ([`proxy_mirror_or_oome`], gch h2/fj).
pub(in crate::vm::vm_exec) fn proxy_signature_mirror(
    shared: &SharedVm,
    resolved: Option<ClassId>,
    desc: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    proxy_mirror_or_oome(|| match resolved {
        Some(cid) => super::get_or_create_class_mirror(shared, cid),
        None => proxy_descriptor_to_class_mirror(shared, desc),
    })
}

/// gch h2/fj (`gch-h1jp-dispatch-doors-and-oome-ladder-review-findings-DONE-20261012`
/// item 4, `gcg-g4oo-oome-ladder-review-findings` item 4): run a proxy
/// dispatch's class-mirror minting (`get_or_create_class_mirror`, directly or
/// through [`proxy_resolve_declaring_class_mirror`] /
/// [`proxy_descriptor_to_class_mirror`]) on the native allocators' unwinding
/// arm (`native_oom::catch_alloc_oom`), and answer a refusal with the
/// catchable `OutOfMemoryError` HotSpot throws. These doors are reached from
/// interpreter and JIT dispatch, outside any native scope, where the mirror's
/// refusal was the infallible arm: an abort on Generational and ZGC
/// (mirror-at-init does not cover a proxied method's signature types, which
/// need not be initialised). The unwind stops here, so it crosses no compiled
/// frame; no mint holds a class-manager lock when it unwinds; G1 keeps its
/// reserve rule (`native_mirror_refusal`). Like every other allocation of
/// these doors, nothing collects first (they hold unpinned references).
pub(in crate::vm::vm_exec) fn proxy_mirror_or_oome(mint: impl FnOnce() -> ObjectRef) -> Result<ObjectRef, MethodCallFailed> {
    crate::runtime::native_oom::catch_alloc_oom(mint).map_err(|oom| oom.into_runtime_error().into())
}

/// gci i22/oo: what `NativeContextImpl::try_get_class_mirror` answers on
/// Generational and G1 after the fallible mirror attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::vm::vm_exec) enum NativeTryMirror {
    /// The mirror (cached, or minted by the fallible arm).
    Mirror(ObjectRef),
    /// `None` to the caller, which collects and asks again.
    Decline,
    /// The infallible door, as before gci i22/oo (the switch is off).
    Infallible,
}

/// The pure rule of [`NativeTryMirror`]: a mirror passes through without the
/// switch being read; a refusal declines while
/// `CRATONVM_GC_NATIVE_MIRROR_RECLAIM` holds, else takes the infallible door.
pub(in crate::vm::vm_exec) fn native_try_mirror_answer(
    fallible: Option<ObjectRef>,
    reclaim_on: impl FnOnce() -> bool,
) -> NativeTryMirror {
    match fallible {
        Some(mirror) => NativeTryMirror::Mirror(mirror),
        None if reclaim_on() => NativeTryMirror::Decline,
        None => NativeTryMirror::Infallible,
    }
}

/// gci i22/oo: `CRATONVM_GC_NATIVE_MIRROR_RECLAIM` (default ON,
/// `runtime_flag_default_on`; `=0` restores the infallible
/// `try_get_class_mirror` on Generational and G1). Read only on a refused
/// mirror, never on the cached path.
pub(in crate::vm::vm_exec) fn native_mirror_reclaim_enabled() -> bool {
    true
}


pub(in crate::vm) fn proxy_descriptor_to_class_mirror(shared: &SharedVm, desc: &str) -> ObjectRef {
    if desc.is_empty() {
        return super::get_or_create_class_mirror(shared, ClassId::new(0));
    }
    // Primitive types -> primitive mirror.
    let prim_name = match desc {
        "V" => Some("void"),
        "Z" => Some("boolean"),
        "B" => Some("byte"),
        "C" => Some("char"),
        "S" => Some("short"),
        "I" => Some("int"),
        "J" => Some("long"),
        "F" => Some("float"),
        "D" => Some("double"),
        _ => None,
    };
    if let Some(p) = prim_name {
        return super::get_or_create_primitive_mirror(shared, p);
    }
    // Reference / array: load by JVMS internal name.
    //   `Ljava/lang/Object;` -> "java/lang/Object"
    //   `[Ljava/lang/reflect/Type;` -> "[Ljava/lang/reflect/Type;"  (array name)
    //   `[I` -> "[I"
    let load_name: String = if desc.starts_with('[') {
        desc.to_string()
    } else if desc.starts_with('L') && desc.ends_with(';') {
        desc[1..desc.len() - 1].to_string()
    } else {
        return super::get_or_create_class_mirror(shared, ClassId::new(0));
    };
    let cid = shared
        .classes
        .class_manager_write_settling()
        .load_class(&load_name)
        .unwrap_or(ClassId::new(0));
    super::get_or_create_class_mirror(shared, cid)
}

/// Resolve the Class mirror that should be stored in the synthesized
/// `Method.clazz` field for a proxy dispatch — i.e. the interface that
/// actually declares the method being invoked. Walks the proxy's
/// `interfaces` array (field 1) and, for each interface, scans its
/// `methods` list (including inherited super-interfaces, transitively)
/// for a matching `(name, descriptor)`.
///
/// Falls back to the first interface mirror if no match is found, or to
/// the `Object` mirror if the proxy carries no interfaces at all — the
/// latter is preferable to `ClassId(0)` since `ClassId(0)` happens to be
/// `java/lang/Object` in production builds (first class loaded), which
/// would make `method.getDeclaringClass() == Object.class` evaluate
/// `true` for **every** proxied method. ByteBuddy's
/// `JavaDispatcher$ProxiedInvocationHandler.invoke` gates its real
/// dispatch on `declaringClass != Object`, so the `ClassId(0)` sentinel
/// caused every non-`equals`/`hashCode`/`toString` proxied call to
/// surface as `IllegalStateException: Unexpected object method`.
pub(in crate::vm) fn proxy_resolve_declaring_class_mirror(
    shared: &SharedVm,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
) -> ObjectRef {
    // `Object`'s own instance methods are never declared on any implemented
    // interface, by construction (an interface implicitly inherits them, it
    // never redeclares them) — and per the JLS/`Proxy` contract, `equals`/
    // `hashCode`/`toString` are the ONLY `Object` methods a proxy's
    // `InvocationHandler` is ever asked to intercept (`getClass`/`notify`/
    // `wait`/`finalize` never route through `invoke()`). Short-circuit here
    // so the interface walk below — and specifically its "no declaring
    // interface found" fallback just past it, which deliberately guesses the
    // proxy's FIRST implemented interface for a genuinely-unresolvable
    // lookup — can never misattribute these three well-known `Object`
    // methods to an interface that merely happens to be listed first.
    //
    // This was the root cause of a 100%-reproducible bug where `Method
    // .getDeclaringClass()` on the `Method` passed to `invoke()` for
    // `toString()`/`equals()`/`hashCode()` incorrectly reported the proxy's
    // first interface instead of `java.lang.Object`: the walk below
    // correctly fails to find them declared on any interface (they aren't),
    // then fell through to the "prefer the first interface mirror... so the
    // Method still answers with a real interface" fallback — which is wrong
    // for these three specifically, since real `Object` IS the right answer.
    // (`JndiObjectFactoryBeanTests.lookupWithExposeAccessContext` —
    // `JndiContextExposingInterceptor.isEligible(Method)` checks `Object
    // .class != method.getDeclaringClass()`, which incorrectly evaluated
    // true for `toString()`, causing an extra unwanted JNDI context
    // open/close Mockito caught as an unexpected extra invocation.)
    if (method_name == "equals" && descriptor == "(Ljava/lang/Object;)Z")
        || (method_name == "hashCode" && descriptor == "()I")
        || (method_name == "toString" && descriptor == "()Ljava/lang/String;")
    {
        return super::get_or_create_class_mirror(shared, ClassId::new(0));
    }

    // Source the proxy's implemented interfaces. The synthetic 3-slot layout
    // stores the `Class[]` at slot 1; the real-super layout (proxy-real-classfile
    // migration: extends `java.lang.reflect.Proxy` — single field `h` at slot 0)
    // has no such slot, so read the interfaces from the proxy class's own
    // declared interfaces instead. Decide by the receiver's field count so we
    // never read slot 1 out of bounds on the 1-field real-super proxy (which
    // would trip the gen_heap OOB guard and mis-resolve `getDeclaringClass()` to
    // `Object`).
    let proxy_cid = shared.mem.heap.class_id_of(proxy);
    let has_iface_slot = shared
        .classes
        .class_manager
        .read()
        .get_class(proxy_cid)
        .map(|c| c.num_total_fields >= 2)
        .unwrap_or(false);

    // Collect the interface ClassIds to search + the first interface's mirror
    // (the fallback declaring class when no interface declares the method).
    let mut iface_cids: Vec<ClassId> = Vec::new();
    let mut first_iface_mirror: Option<ObjectRef> = None;
    if has_iface_slot {
        if let Value::Object(Some(arr)) = shared.mem.heap.get_field(proxy, 1) {
            let n = shared.mem.heap.array_length(arr);
            for i in 0..n {
                if let Ok(Value::Object(Some(m))) = shared.mem.heap.get_array_element(arr, i) {
                    if first_iface_mirror.is_none() {
                        first_iface_mirror = Some(m);
                    }
                    if let Some(cid) = shared.classes.class_mirrors_reverse.read().get(&m).copied()
                    {
                        iface_cids.push(cid);
                    }
                }
            }
        }
    } else {
        // Real-super layout: the proxy class's declared interfaces ARE the proxy
        // interface set (the generated `$ProxyN` declares them directly).
        let cids = shared
            .classes
            .class_manager
            .read()
            .get_class(proxy_cid)
            .map(|c| c.interfaces.clone())
            .unwrap_or_default();
        for cid in cids {
            if first_iface_mirror.is_none() {
                first_iface_mirror = Some(super::get_or_create_class_mirror(shared, cid));
            }
            iface_cids.push(cid);
        }
    }

    for iface_cid in iface_cids {
        // Walk this interface + all super-interfaces (transitively)
        // looking for a declared method matching (name, descriptor).
        let mut visited: rustc_hash::FxHashSet<ClassId> = rustc_hash::FxHashSet::default();
        let mut stack: Vec<ClassId> = vec![iface_cid];
        while let Some(cid) = stack.pop() {
            if !visited.insert(cid) {
                continue;
            }
            let supers: Vec<ClassId> = {
                let cm = shared.classes.class_manager.read();
                let Some(class) = cm.get_class(cid) else {
                    continue;
                };
                let found = class
                    .methods
                    .iter()
                    .any(|m| &*m.name == method_name && &*m.descriptor == descriptor);
                if found {
                    drop(cm);
                    return super::get_or_create_class_mirror(shared, cid);
                }
                class.interfaces.clone()
            };
            for s in supers {
                stack.push(s);
            }
        }
    }
    // No declaring interface found — prefer the first interface mirror
    // over `ClassId(0)` so the resulting Method object still answers
    // `getDeclaringClass()` with a real interface (not `Object`).
    if let Some(m) = first_iface_mirror {
        return m;
    }
    super::get_or_create_class_mirror(shared, ClassId::new(0))
}

